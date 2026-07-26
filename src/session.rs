use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use vvmux_terminal::TerminalHyperlink;
use vvmux_terminal::pty::{PtyControl, PtyExitStatus, PtyInput, PtyProcess};
use vvmux_terminal::{Cell, Terminal, TerminalColor, TerminalEvent, TerminalModes, UnderlineStyle};

use crate::config::Config;
use crate::ipc::{
    Action, AutomationError, AutomationMethod, AutomationRequest, AutomationResponse, Axis,
    BridgeClipRect, BridgeNode, BridgePlayRequest, BridgeSource, BridgeSourceDescriptor,
    BridgeSourceKey, BridgeSourceKind, ClientMessage, Direction, DisplayMetrics,
    FloatingEditCommand, FloatingEditKind, MouseEvent, MouseKind, ServerMessage, SharedWriter,
};
use crate::layout::{
    EdgeMask, FloatingLayer, PaneId, PaneLayer, PaneProjection, Rect, TiledNode, directional_focus,
};
use crate::media::VirtualVivid;
use crate::platform::VirtualPresenterEndpoint;
use crate::region::{FixedRect, from_cells, intersect, subtract_all};
use crate::screen::{ScreenBuffer, ansi_diff};

const EVENT_QUEUE: usize = 1024;
/// Slots on the dedicated media-event receiver.
///
/// Total queued media bytes are separately bounded by `media.ipc_queue_bytes`, so this only needs
/// enough slots that small records — audio access units especially — cannot exhaust it while that
/// byte budget is far from spent.
const MEDIA_EVENT_QUEUE: usize = 256;
const COPY_BUFFER_LIMIT: usize = 1024 * 1024;
const MAX_NODE_FRAGMENTS: usize = 8;
const MAX_PROJECTED_NODES: usize = 256;
const INPUT_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const MAX_AUTOMATION_REQUESTS_PER_CLIENT: usize = 64;
const MAX_AUTOMATION_WAITERS: usize = 256;
const MAX_PENDING_ACTOR_WORK: usize = 256;
/// Frames allowed outstanding before rendering pauses for the client to catch up.
///
/// The acknowledgement arrives after the client writes the frame to its terminal, so this bounds
/// how far the server may run ahead of what the user can actually see. Media snapshots and media
/// records are never gated by it: a slow terminal must not stall the projected scene.
const MAX_UNACKNOWLEDGED_FRAMES: u64 = 8;
const AUTOMATION_RESPONSE_QUEUE: usize = 8;
const SCREEN_CHANGE_HISTORY: usize = 1024;
const EXIT_TOMBSTONES: usize = 128;
const PTY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const AUTOMATION_REPLY_LIMIT: usize = 16 * 1024 * 1024;
#[cfg(windows)]
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
#[cfg(windows)]
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";

pub enum ActorEvent {
    Client {
        id: u64,
        writer: SharedWriter,
        cancel: crate::platform::ConnectionCancel,
        message: ClientMessage,
    },
    Disconnected(u64),
    PtyOutput(PaneId, Vec<u8>),
    PtyExit(PaneId, Option<PtyExitStatus>),
    AutomationInputComplete {
        reply: AutomationReplyTarget,
        result: Result<(), String>,
    },
    /// A media event is waiting on the dedicated media receiver.
    ///
    /// Carries no payload: it exists only to wake the actor promptly. Losing one to a full queue
    /// is harmless because the actor drains media at the top of every iteration anyway.
    MediaReady,
}

#[derive(Clone)]
pub struct AutomationReplyTarget {
    client_id: u64,
    request_id: u64,
    writer: SharedWriter,
    cancel: crate::platform::ConnectionCancel,
}

struct AutomationResponseJob {
    writer: SharedWriter,
    response: AutomationResponse,
}

#[derive(Clone)]
pub struct ActorHandle {
    pub sender: mpsc::SyncSender<ActorEvent>,
    pub shutdown: Arc<AtomicBool>,
}

struct AttachedClient {
    id: u64,
    writer: SharedWriter,
    display: DisplayMetrics,
    acknowledged_frame: u64,
    vivid: bool,
    rendered_session_sequence: u64,
    frame_sequences: VecDeque<(u64, u64)>,
}

struct Pane {
    id: PaneId,
    terminal: Terminal,
    input: PtyInput,
    control: PtyControl,
    copy: Option<CopyState>,
    vivid_metrics: Option<(u16, u16, u16, u16)>,
    last_input_warning: Option<Instant>,
    screen_sequence: u64,
    last_screen_change: Instant,
    screen_changes: VecDeque<ScreenChange>,
}

#[derive(Debug, Clone)]
struct ScreenChange {
    sequence: u64,
    rows: Option<Vec<usize>>,
}

#[derive(Debug, Clone, Copy)]
struct ExitTombstone {
    pane_id: PaneId,
    status: Option<PtyExitStatus>,
}

struct AutomationWaiter {
    reply: AutomationReplyTarget,
    pane_id: Option<PaneId>,
    deadline: Instant,
    kind: AutomationWaitKind,
}

enum AutomationWaitKind {
    Text {
        pattern: AutomationTextPattern,
        after_screen: Option<u64>,
    },
    ScreenChange {
        after_screen: u64,
    },
    ScreenStable {
        quiet: Duration,
        after_screen: Option<u64>,
    },
    Rendered {
        after_session: u64,
    },
    Exit,
    Media {
        after_virtual_revision: Option<u64>,
        after_outer_revision: Option<u64>,
    },
}

enum AutomationTextPattern {
    Literal(String),
    Regex(regex::Regex),
}

#[derive(Clone, Copy)]
struct InputFailure {
    warn: bool,
    close: bool,
}

fn queue_pane_input(pane: &mut Pane, bytes: &[u8]) -> Option<InputFailure> {
    let error = pane.input.send(bytes).err()?;
    let now = Instant::now();
    let warn = pane
        .last_input_warning
        .is_none_or(|previous| now.duration_since(previous) >= INPUT_STATUS_INTERVAL);
    if warn {
        pane.last_input_warning = Some(now);
    }
    Some(InputFailure {
        warn,
        close: error.kind() == io::ErrorKind::BrokenPipe,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyState {
    offset: usize,
    row: usize,
    column: usize,
    selection_start: Option<(isize, usize)>,
}

struct Tab {
    id: u64,
    /// `None` when the last tiled pane closed and only floats remain.
    tree: Option<TiledNode>,
    floating: FloatingLayer,
    focused: PaneId,
    last_focused_tiled: Option<PaneId>,
    zoomed: Option<PaneId>,
}

impl Tab {
    fn contains(&self, pane: PaneId) -> bool {
        self.tree.as_ref().is_some_and(|tree| tree.contains(pane)) || self.floating.contains(pane)
    }

    fn is_empty(&self) -> bool {
        self.tree.is_none() && self.floating.is_empty()
    }

    /// The focus fallback when the current pane becomes unavailable: topmost visible pinned
    /// float, topmost visible ordinary float, last focused tiled pane, first tiled leaf.
    fn fallback_focus(&self) -> Option<PaneId> {
        self.floating
            .focus_candidate()
            .or_else(|| {
                self.last_focused_tiled
                    .filter(|pane| self.tree.as_ref().is_some_and(|tree| tree.contains(*pane)))
            })
            .or_else(|| {
                self.tree
                    .as_ref()
                    .and_then(|tree| tree.pane_ids().into_iter().next())
            })
    }

    fn set_focus(&mut self, pane: PaneId) {
        self.focused = pane;
        if self.tree.as_ref().is_some_and(|tree| tree.contains(pane)) {
            self.last_focused_tiled = Some(pane);
        } else {
            self.floating.raise(pane);
        }
    }
}

/// The ordered bottom-to-top paint list for one tab: tiled leaves, visible ordinary floats,
/// then pinned floats; a zoomed tab projects exactly its zoomed pane over the whole area, so
/// every other pane - pinned floats included - is hidden while zoomed.
fn visible_projections(tab: &Tab, area: Rect) -> Vec<PaneProjection> {
    if let Some(zoomed) = tab.zoomed {
        let layer = match tab.floating.get(zoomed) {
            Some(float) if float.pinned => PaneLayer::Pinned,
            Some(_) => PaneLayer::Floating,
            None => PaneLayer::Tiled,
        };
        return vec![PaneProjection {
            pane_id: zoomed,
            outer: area,
            content: area.content(),
            layer,
            focused: tab.focused == zoomed,
        }];
    }
    let mut projections = Vec::new();
    if let Some(tree) = &tab.tree {
        for (pane_id, outer) in tree.geometry(area) {
            projections.push(PaneProjection {
                pane_id,
                outer,
                content: outer.content(),
                layer: PaneLayer::Tiled,
                focused: tab.focused == pane_id,
            });
        }
    }
    for float in tab.floating.visible() {
        projections.push(PaneProjection {
            pane_id: float.pane_id,
            outer: float.rect,
            content: float.rect.content(),
            layer: if float.pinned {
                PaneLayer::Pinned
            } else {
                PaneLayer::Floating
            },
            focused: tab.focused == float.pane_id,
        });
    }
    projections
}

fn projection_pane_priority(tab: &Tab, projections: &[PaneProjection]) -> Vec<PaneId> {
    let mut panes = Vec::with_capacity(projections.len());
    let mut seen = HashSet::new();
    let mut push = |pane| {
        if seen.insert(pane) {
            panes.push(pane);
        }
    };
    if projections
        .iter()
        .any(|projection| projection.pane_id == tab.focused)
    {
        push(tab.focused);
    }
    for projection in projections
        .iter()
        .rev()
        .filter(|projection| projection.layer == PaneLayer::Pinned)
    {
        push(projection.pane_id);
    }
    for projection in projections
        .iter()
        .rev()
        .filter(|projection| projection.layer == PaneLayer::Floating)
    {
        push(projection.pane_id);
    }
    if let Some(pane) = tab.last_focused_tiled
        && projections
            .iter()
            .any(|projection| projection.pane_id == pane)
    {
        push(pane);
    }
    let mut tiled = projections
        .iter()
        .filter(|projection| projection.layer == PaneLayer::Tiled)
        .map(|projection| projection.pane_id)
        .collect::<Vec<_>>();
    tiled.sort_unstable();
    for pane in tiled {
        push(pane);
    }
    panes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatPointerTarget {
    Move,
    Resize(EdgeMask),
}

/// The top frame is a title/move bar except for configurable corner regions. Side and bottom
/// frames resize their corresponding edges; bottom corners select two edges.
fn float_pointer_target(
    rect: Rect,
    x: u16,
    y: u16,
    border_drag_margin: u16,
) -> Option<FloatPointerTarget> {
    if !rect.contains(x, y) || rect.width < 2 || rect.height < 2 {
        return None;
    }
    let right = rect.x + rect.width - 1;
    let bottom = rect.y + rect.height - 1;
    let on_left = x == rect.x;
    let on_right = x == right;
    let on_top = y == rect.y;
    let on_bottom = y == bottom;
    if on_top {
        let margin = border_drag_margin.max(1).min(rect.width / 2);
        if x < rect.x + margin {
            return Some(FloatPointerTarget::Resize(EdgeMask {
                left: true,
                top: true,
                ..EdgeMask::default()
            }));
        }
        if x >= rect.x + rect.width - margin {
            return Some(FloatPointerTarget::Resize(EdgeMask {
                right: true,
                top: true,
                ..EdgeMask::default()
            }));
        }
        return Some(FloatPointerTarget::Move);
    }
    if on_left || on_right || on_bottom {
        return Some(FloatPointerTarget::Resize(EdgeMask {
            left: on_left,
            right: on_right,
            bottom: on_bottom,
            ..EdgeMask::default()
        }));
    }
    None
}

#[derive(Debug, Clone)]
enum PointerDrag {
    TiledBoundary {
        tab_id: u64,
        axis: Axis,
        boundary: u16,
        last: u16,
        original: TiledNode,
    },
    Move {
        tab_id: u64,
        pane: PaneId,
        start: (u16, u16),
        original: Rect,
    },
    Resize {
        tab_id: u64,
        pane: PaneId,
        edges: EdgeMask,
        start: (u16, u16),
        original: Rect,
    },
}

impl PointerDrag {
    fn pane(&self) -> Option<PaneId> {
        match self {
            Self::TiledBoundary { .. } => None,
            Self::Move { pane, .. } | Self::Resize { pane, .. } => Some(*pane),
        }
    }
}

/// Keyboard float-edit mode, authoritative in the actor: the client parses edit keys only
/// after this mode is announced, and stale mode IDs are ignored.
#[derive(Debug, Clone, Copy)]
struct FloatModal {
    mode_id: u64,
    pane: PaneId,
    kind: FloatingEditKind,
    original: Rect,
}

#[derive(Debug, Default)]
struct FragmentMap {
    rectangles: HashMap<FixedRect, u8>,
}

impl FragmentMap {
    /// Preserve IDs for unchanged rectangles, recycle IDs from disappeared rectangles, then
    /// assign new rectangles the lowest available IDs in deterministic geometry order.
    fn assign(&mut self, fragments: &[FixedRect]) -> Option<Vec<(u8, FixedRect)>> {
        let unique = fragments.iter().copied().collect::<HashSet<_>>();
        if unique.len() != fragments.len() {
            return None;
        }
        let mut used = HashSet::new();
        let mut next = HashMap::new();
        let mut assigned = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            let id = if let Some(id) = self.rectangles.get(fragment).copied() {
                if !used.insert(id) {
                    return None;
                }
                id
            } else {
                let id = (0..=u8::MAX).find(|candidate| !used.contains(candidate))?;
                used.insert(id);
                id
            };
            next.insert(*fragment, id);
            assigned.push((id, *fragment));
        }
        self.rectangles = next;
        Some(assigned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MediaProjectionKey {
    virtual_revision: u64,
    layout_revision: u64,
}

fn should_sync_media(
    force: bool,
    last: Option<MediaProjectionKey>,
    current: MediaProjectionKey,
) -> bool {
    force || last != Some(current)
}

struct SessionActor {
    name: String,
    config: Config,
    sender: mpsc::SyncSender<ActorEvent>,
    panes: BTreeMap<PaneId, Pane>,
    tabs: Vec<Tab>,
    active_tab: usize,
    attached: Option<AttachedClient>,
    last_display: DisplayMetrics,
    next_pane_id: PaneId,
    next_tab_id: u64,
    copy_buffer: Vec<u8>,
    frame_id: u64,
    last_screen: Option<ScreenBuffer>,
    #[cfg(windows)]
    outer_bracketed_paste: Option<bool>,
    force_full: bool,
    pending_render: bool,
    layout_revision: u64,
    last_media_projection: Option<MediaProjectionKey>,
    media_projection_revision: u64,
    outer_virtual_revision: u64,
    outer_projection_revision: u64,
    outer_attachment_generations: HashMap<BridgeSourceKey, u64>,
    fragment_assignments: HashMap<(u64, u64), FragmentMap>,
    last_projection_warning: Option<MediaProjectionKey>,
    pointer_drag: Option<PointerDrag>,
    float_modal: Option<FloatModal>,
    next_float_mode: u64,
    session_sequence: u64,
    response_sender: mpsc::SyncSender<AutomationResponseJob>,
    automation_inflight: HashMap<u64, HashSet<u64>>,
    pending_actor_work: HashSet<(u64, u64)>,
    automation_waiters: Vec<AutomationWaiter>,
    exit_tombstones: VecDeque<ExitTombstone>,
    shutdown: Arc<AtomicBool>,
    vivid: VirtualVivid,
    /// Latest foreground-bridge counter report. Diagnostic only; retained across a detach so
    /// `inspect-media` still describes the last live bridge.
    bridge_metrics: crate::metrics::BridgeMetrics,
    /// Counters for the attached client's VVMX connection, retained across a detach for the same
    /// reason. Replaced when a new client attaches.
    client_ipc: Option<Arc<crate::metrics::IpcCounters>>,
}

pub fn start(
    name: String,
    config: Config,
    vivid_endpoint: VirtualPresenterEndpoint,
) -> io::Result<ActorHandle> {
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE);
    let (media_sender, media_receiver) = mpsc::sync_channel(MEDIA_EVENT_QUEUE);
    let shutdown = Arc::new(AtomicBool::new(false));
    let vivid =
        VirtualVivid::start_with_events(vivid_endpoint, config.media.clone(), Some(media_sender))?;
    {
        // Losing a wakeup to a full queue is harmless: the actor drains media around every event
        // and on every idle tick regardless, so `try_send` here must never block ingest.
        let wakeup = sender.clone();
        vivid.set_media_wakeup(Arc::new(move || {
            let _ = wakeup.try_send(ActorEvent::MediaReady);
        }));
    }
    let (response_sender, response_receiver) =
        mpsc::sync_channel::<AutomationResponseJob>(AUTOMATION_RESPONSE_QUEUE);
    let response_receiver = Arc::new(std::sync::Mutex::new(response_receiver));
    for index in 0..2 {
        let receiver = response_receiver.clone();
        std::thread::Builder::new()
            .name(format!("vvmux-automation-response-{index}"))
            .spawn(move || {
                loop {
                    let job = {
                        receiver
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .recv()
                    };
                    let Ok(job) = job else { break };
                    let _ = crate::ipc::send_automation(&job.writer, job.response);
                }
            })?;
    }
    // Media no longer travels through the actor's general event queue. That queue is shared with
    // `PtyOutput`, so a pane producing a lot of terminal output could fill it, block the forwarder,
    // back up the media channel, and make ingest drop frames — starving media in one pane because
    // a different pane was busy. Media now has its own receiver that the actor drains first, and
    // only a coalescible wakeup crosses the shared queue.
    let last_display = normalized_display(
        DisplayMetrics {
            columns: 80,
            rows: 24,
            cell_width: 0,
            cell_height: 0,
        },
        config.general.status_visible,
    );
    let mut actor = SessionActor {
        name,
        config,
        sender: sender.clone(),
        panes: BTreeMap::new(),
        tabs: Vec::new(),
        active_tab: 0,
        attached: None,
        last_display,
        next_pane_id: 1,
        next_tab_id: 1,
        copy_buffer: Vec::new(),
        frame_id: 0,
        last_screen: None,
        #[cfg(windows)]
        outer_bracketed_paste: None,
        force_full: true,
        pending_render: false,
        layout_revision: 0,
        last_media_projection: None,
        media_projection_revision: 0,
        outer_virtual_revision: 0,
        outer_projection_revision: 0,
        outer_attachment_generations: HashMap::new(),
        fragment_assignments: HashMap::new(),
        last_projection_warning: None,
        pointer_drag: None,
        float_modal: None,
        next_float_mode: 0,
        session_sequence: 1,
        response_sender,
        automation_inflight: HashMap::new(),
        pending_actor_work: HashSet::new(),
        automation_waiters: Vec::new(),
        exit_tombstones: VecDeque::new(),
        shutdown: shutdown.clone(),
        vivid,
        bridge_metrics: crate::metrics::BridgeMetrics::default(),
        client_ipc: None,
    };
    actor.new_tab()?;
    std::thread::Builder::new()
        .name("vvmux-session".into())
        .spawn(move || actor.run(receiver, media_receiver))?;
    Ok(ActorHandle { sender, shutdown })
}

impl SessionActor {
    fn run(
        &mut self,
        receiver: mpsc::Receiver<ActorEvent>,
        media_receiver: mpsc::Receiver<crate::media::MediaEvent>,
    ) {
        let interval = Duration::from_millis(self.config.general.render_interval_ms);
        let mut render_at = Instant::now();
        loop {
            let mut timeout = if self.pending_render {
                render_at.saturating_duration_since(Instant::now())
            } else {
                Duration::from_secs(1)
            };
            timeout = timeout.min(self.next_automation_deadline());
            // Media first, and to exhaustion: a projected source's frames must not queue behind an
            // unrelated pane's terminal output. This is cheap when idle because the media receiver
            // is empty and `try_recv` does not block.
            self.drain_media(&media_receiver);
            match receiver.recv_timeout(timeout) {
                Ok(event) => {
                    if self.handle_event(event).is_err() {
                        self.force_full = true;
                    }
                    self.drain_media(&media_receiver);
                    if self.pending_render && render_at <= Instant::now() {
                        self.render();
                        render_at = Instant::now() + interval;
                    } else if self.pending_render && render_at < Instant::now() + interval {
                        // Keep the already scheduled coalescing boundary.
                    } else if self.pending_render {
                        render_at = Instant::now() + interval;
                    }
                    self.check_automation_waiters();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.drain_media(&media_receiver);
                    if self.pending_render {
                        self.render();
                        render_at = Instant::now() + interval;
                    }
                    self.sync_media(false);
                    self.check_automation_waiters();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.tabs.is_empty() || self.shutdown.load(Ordering::Acquire) {
                break;
            }
        }
        self.terminate_children();
        self.shutdown.store(true, Ordering::Release);
    }

    /// Forward every media event currently queued.
    ///
    /// Bounded by the media channel's own capacity, so this cannot become an unbounded stall on
    /// the actor even when a producer is saturating its source.
    fn drain_media(&mut self, media_receiver: &mpsc::Receiver<crate::media::MediaEvent>) {
        while let Ok(event) = media_receiver.try_recv() {
            self.forward_media(event);
        }
    }

    fn forward_media(&mut self, event: crate::media::MediaEvent) {
        // PLAY/PAUSE/EOS arrive on the producer's control connection while media arrives on
        // independent source connections. Publish any resulting authoritative projection revision
        // before forwarding the next media record. Otherwise a busy stream can starve the actor's
        // idle sync, fill outer pre-roll with later audio, and leave video waiting for a PLAY
        // snapshot that is queued behind that media.
        self.sync_media_before_delivery(event.source);
        let sent = self
            .attached
            .as_ref()
            .filter(|client| client.vivid)
            .is_some_and(|client| {
                send_media_body(
                    &client.writer,
                    event.delivery_id,
                    bridge_key(event.source),
                    event.record_type,
                    &event.body,
                )
            });
        if !sent {
            self.vivid
                .complete_bridge_delivery(event.delivery_id, false);
        }
    }

    fn handle_event(&mut self, event: ActorEvent) -> io::Result<()> {
        match event {
            ActorEvent::Client {
                id,
                writer,
                cancel,
                message,
            } => {
                self.handle_client(id, writer, cancel, message)?;
            }
            ActorEvent::Disconnected(id) => {
                self.automation_inflight.remove(&id);
                self.pending_actor_work
                    .retain(|(client_id, _)| *client_id != id);
                self.automation_waiters
                    .retain(|waiter| waiter.reply.client_id != id);
                if self.attached.as_ref().is_some_and(|client| client.id == id) {
                    self.cancel_pointer_drag(true);
                    self.attached = None;
                    self.last_screen = None;
                    #[cfg(windows)]
                    {
                        self.outer_bracketed_paste = None;
                    }
                    self.force_full = true;
                    self.end_float_mode(true);
                }
            }
            ActorEvent::PtyOutput(pane_id, bytes) => {
                let focused = self.active_tab().is_some_and(|tab| tab.focused == pane_id);
                let mut title = None;
                let mut bell = false;
                let mut input_warning = false;
                let mut input_closed = false;
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    let old_cells = pane.terminal.cells().to_vec();
                    let old_cursor = pane.terminal.cursor();
                    let old_modes = pane.terminal.modes();
                    let old_screen = pane.terminal.alternate_screen();
                    let events = pane.terminal.feed(&bytes);
                    for event in events {
                        match event {
                            TerminalEvent::PtyWrite(bytes) => {
                                if let Some(failure) = queue_pane_input(pane, &bytes) {
                                    input_warning |= failure.warn;
                                    input_closed |= failure.close;
                                }
                            }
                            TerminalEvent::Title(next_title) if focused => {
                                title = next_title;
                            }
                            TerminalEvent::Bell => {
                                bell = true;
                            }
                            TerminalEvent::VividMarker {
                                marker,
                                row,
                                column,
                            } => {
                                // The authenticated marker is consumed here. Media ownership is
                                // connected by the virtual-presenter module, never forwarded into
                                // the outer terminal byte stream. The position was captured when
                                // the marker was consumed: the live cursor has already moved on
                                // when ConPTY batches repositioning output behind the marker.
                                self.vivid
                                    .observe_marker(pane_id, &marker, row as i32, column);
                            }
                            TerminalEvent::GridScroll(lines) => {
                                self.vivid.scroll_anchors(pane_id, lines);
                            }
                            TerminalEvent::Clear => self.vivid.clear_anchors(pane_id),
                            _ => {}
                        }
                    }
                    let semantic_changed = old_cells != pane.terminal.cells()
                        || old_cursor != pane.terminal.cursor()
                        || old_modes != pane.terminal.modes()
                        || old_screen != pane.terminal.alternate_screen();
                    if semantic_changed {
                        let rows = changed_rows(&old_cells, pane.terminal.cells());
                        let rows = (old_screen == pane.terminal.alternate_screen()).then_some(rows);
                        pane.screen_sequence = pane.screen_sequence.wrapping_add(1);
                        pane.last_screen_change = Instant::now();
                        pane.screen_changes.push_back(ScreenChange {
                            sequence: pane.screen_sequence,
                            rows,
                        });
                        while pane.screen_changes.len() > SCREEN_CHANGE_HISTORY {
                            pane.screen_changes.pop_front();
                        }
                        self.session_sequence = self.session_sequence.wrapping_add(1);
                    }
                    if let Some(client) = &self.attached {
                        if let Some(title) = title {
                            let _ = crate::ipc::send(
                                &client.writer,
                                &ServerMessage::Title(format!("{title} — vvmux")),
                            );
                        }
                        if bell {
                            let _ = crate::ipc::send(&client.writer, &ServerMessage::Bell);
                        }
                    }
                    self.schedule_render();
                }
                if input_warning {
                    self.status(&format!("pane {pane_id} input queue is unavailable"));
                }
                if input_closed {
                    self.close_pane(pane_id);
                }
            }
            ActorEvent::PtyExit(pane_id, status) => {
                self.complete_exit_waiters(pane_id, status);
                self.exit_tombstones
                    .push_back(ExitTombstone { pane_id, status });
                while self.exit_tombstones.len() > EXIT_TOMBSTONES {
                    self.exit_tombstones.pop_front();
                }
                self.close_pane(pane_id);
            }
            ActorEvent::AutomationInputComplete { reply, result } => match result {
                Ok(()) => {
                    self.complete_pending_actor_work(&reply);
                    self.reply_automation(reply, serde_json::Value::Null);
                }
                Err(message) => {
                    self.complete_pending_actor_work(&reply);
                    self.reply_automation_error(reply, AutomationError::new("pty_closed", message));
                }
            },
            // The payload arrives on the dedicated media receiver, which the run loop drains
            // around every event; this wakeup only ensures it happens promptly.
            ActorEvent::MediaReady => {}
        }
        Ok(())
    }

    fn handle_client(
        &mut self,
        id: u64,
        writer: SharedWriter,
        cancel: crate::platform::ConnectionCancel,
        message: ClientMessage,
    ) -> io::Result<()> {
        match message {
            ClientMessage::Attach {
                replace,
                display,
                vivid,
            } => {
                self.cancel_pointer_drag(true);
                self.end_float_mode(true);
                if let Some(old) = &self.attached {
                    if !replace {
                        return crate::ipc::send(
                            &writer,
                            &ServerMessage::Error("session already has an attached client".into()),
                        );
                    }
                    let _ = crate::ipc::send(
                        &old.writer,
                        &ServerMessage::Detached {
                            reason: "replaced by another client".into(),
                        },
                    );
                }
                let display = normalized_display(display, self.config.general.status_visible);
                self.last_display = display;
                self.client_ipc = Some(
                    writer
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .counters(),
                );
                self.bridge_metrics = crate::metrics::BridgeMetrics::default();
                self.attached = Some(AttachedClient {
                    id,
                    writer: writer.clone(),
                    display,
                    acknowledged_frame: 0,
                    vivid,
                    rendered_session_sequence: 0,
                    frame_sequences: VecDeque::new(),
                });
                self.fragment_assignments.clear();
                self.last_projection_warning = None;
                // Detached tabs retain their rectangles. Clamp them against the attaching host
                // before publishing the first text or media projection.
                let area = self.content_area();
                for tab in &mut self.tabs {
                    tab.floating.clamp_all(area);
                }
                crate::ipc::send(
                    &writer,
                    &ServerMessage::Attached {
                        session: self.name.clone(),
                        text_only: !vivid,
                    },
                )?;
                self.last_screen = None;
                #[cfg(windows)]
                {
                    self.outer_bracketed_paste = None;
                }
                self.force_full = true;
                self.resize_all();
                self.sync_media(true);
                self.schedule_render();
            }
            ClientMessage::Input(bytes) => {
                if self.client_is(id) {
                    self.input(bytes);
                }
            }
            ClientMessage::Mouse(mouse) => {
                if self.client_is(id) {
                    self.mouse(mouse);
                }
            }
            ClientMessage::Resize(display) => {
                if self.client_is(id) {
                    self.cancel_pointer_drag(true);
                    self.end_float_mode(true);
                    if let Some(client) = &mut self.attached {
                        client.display =
                            normalized_display(display, self.config.general.status_visible);
                        self.last_display = client.display;
                    }
                    // Deterministic host-resize clamp: size before position, per float.
                    let area = self.content_area();
                    for tab in &mut self.tabs {
                        tab.floating.clamp_all(area);
                    }
                    self.force_full = true;
                    self.relayout();
                }
            }
            ClientMessage::Action(action) => {
                if self.client_is(id) {
                    self.action(action);
                }
            }
            ClientMessage::RenderAck(frame_id) => {
                if let Some(client) = &mut self.attached
                    && client.id == id
                {
                    if frame_id < client.acknowledged_frame || frame_id > self.frame_id {
                        self.force_full = true;
                    } else {
                        client.acknowledged_frame = frame_id;
                        while let Some(&(sent_frame, sequence)) = client.frame_sequences.front() {
                            if sent_frame > frame_id {
                                break;
                            }
                            client.rendered_session_sequence =
                                client.rendered_session_sequence.max(sequence);
                            client.frame_sequences.pop_front();
                        }
                    }
                }
            }
            ClientMessage::RenderResync => {
                if self.client_is(id) {
                    // Treat the discarded backlog as acknowledged: those frames will never be
                    // displayed, and leaving them outstanding would stall the render gate.
                    if let Some(client) = &mut self.attached {
                        client.acknowledged_frame = self.frame_id;
                        client.frame_sequences.clear();
                    }
                    self.force_full = true;
                    self.last_screen = None;
                    self.schedule_render();
                }
            }
            ClientMessage::BridgeNeedKeyframes(requests) => {
                if self.client_is(id) {
                    for request in requests {
                        self.vivid.request_keyframe(
                            (request.source.producer, request.source.source),
                            request.minimum_epoch,
                            request.reason,
                        );
                    }
                }
            }
            ClientMessage::BridgeNeedFullFrames(sources) => {
                if self.client_is(id) {
                    self.vivid.request_full_frames(
                        &sources
                            .into_iter()
                            .map(|source| (source.producer, source.source))
                            .collect::<Vec<_>>(),
                        vivid_protocol::messages::NEED_FULL_FRAME_BASE_UNAVAILABLE,
                    );
                }
            }
            ClientMessage::BridgeCapabilitiesChanged { reason_mask } => {
                if self.client_is(id) {
                    let _ = self.vivid.notify_capabilities_changed(reason_mask);
                }
            }
            ClientMessage::BridgeMediaAck {
                delivery_id,
                delivered,
            } => {
                if self.client_is(id) {
                    let resync = self.vivid.complete_bridge_delivery(delivery_id, delivered);
                    if resync {
                        self.last_media_projection = None;
                        self.sync_media(true);
                    }
                }
            }
            ClientMessage::BridgeSnapshotRetry => {
                if self.client_is(id) {
                    // The worker requests this only when it will rebuild a replacement outer
                    // session. Fragment identities are scoped to that outer session.
                    self.fragment_assignments.clear();
                    self.last_media_projection = None;
                    self.sync_media(true);
                }
            }
            ClientMessage::BridgeApplied {
                virtual_revision,
                outer_revision,
                outer_attachment_generations,
            } => {
                if self.client_is(id)
                    && virtual_revision >= self.outer_virtual_revision
                    && outer_revision >= self.outer_projection_revision
                {
                    self.outer_virtual_revision = virtual_revision;
                    self.outer_projection_revision = outer_revision;
                    self.outer_attachment_generations =
                        outer_attachment_generations.into_iter().collect();
                    self.check_automation_waiters();
                }
            }
            ClientMessage::BridgePlaybackState {
                source,
                state,
                eos_state,
            } => {
                if self.client_is(id) {
                    self.vivid.apply_outer_playback(
                        (source.producer, source.source),
                        state,
                        eos_state,
                    );
                }
            }
            ClientMessage::BridgeMetrics(metrics) => {
                if self.client_is(id) {
                    self.bridge_metrics = metrics;
                }
            }
            ClientMessage::Detach => {
                if self.client_is(id) {
                    self.cancel_pointer_drag(true);
                    crate::ipc::send(
                        &writer,
                        &ServerMessage::Detached {
                            reason: "detached".into(),
                        },
                    )?;
                    self.attached = None;
                    self.last_screen = None;
                    #[cfg(windows)]
                    {
                        self.outer_bracketed_paste = None;
                    }
                    self.end_float_mode(true);
                    self.vivid.deactivate_bridge();
                }
            }
            ClientMessage::Kill => {
                self.shutdown.store(true, Ordering::Release);
                if let Some(client) = &self.attached {
                    let _ = crate::ipc::send(
                        &client.writer,
                        &ServerMessage::Detached {
                            reason: "session killed".into(),
                        },
                    );
                }
            }
            ClientMessage::FloatingEdit { mode_id, command } => {
                if self.client_is(id) {
                    self.float_edit(mode_id, command);
                }
            }
            ClientMessage::Ping => {
                crate::ipc::send(&writer, &ServerMessage::Pong)?;
            }
            ClientMessage::Automation(request) => {
                self.handle_automation(id, writer, cancel, request);
            }
        }
        Ok(())
    }

    fn handle_automation(
        &mut self,
        client_id: u64,
        writer: SharedWriter,
        cancel: crate::platform::ConnectionCancel,
        request: AutomationRequest,
    ) {
        let target = AutomationReplyTarget {
            client_id,
            request_id: request.id,
            writer,
            cancel,
        };
        let requests = self.automation_inflight.entry(client_id).or_default();
        if requests.contains(&request.id) {
            self.send_automation_response(
                &target,
                AutomationResponse::error(
                    target.request_id,
                    "duplicate_request_id",
                    "request ID is already in flight",
                ),
            );
            return;
        }
        if requests.len() >= MAX_AUTOMATION_REQUESTS_PER_CLIENT {
            self.reply_automation_error(
                target,
                AutomationError::new("limit_exceeded", "too many in-flight requests"),
            );
            return;
        }
        requests.insert(request.id);
        if let Err(error) = validate_automation_method(&request.method) {
            self.reply_automation_error(target, error);
            return;
        }

        let pane_id = if method_needs_pane(&request.method) {
            let resolved = if matches!(&request.method, AutomationMethod::WaitExit { .. })
                && request.pane_id.is_some_and(|pane| {
                    self.exit_tombstones.iter().any(|exit| exit.pane_id == pane)
                }) {
                Ok(request.pane_id.unwrap())
            } else {
                self.resolve_automation_pane(request.pane_id, request.allow_focused)
            };
            match resolved {
                Ok(pane) => Some(pane),
                Err(error) => {
                    self.reply_automation_error(target, error);
                    return;
                }
            }
        } else {
            None
        };

        match request.method {
            AutomationMethod::Capabilities => {
                self.reply_automation(target, automation_capabilities());
            }
            AutomationMethod::ListPanes => {
                let panes = self
                    .panes
                    .keys()
                    .copied()
                    .filter_map(|pane| self.pane_description(pane))
                    .collect::<Vec<_>>();
                self.reply_automation(
                    target,
                    serde_json::json!({
                        "session": self.name,
                        "session_sequence": self.session_sequence,
                        "rendered_session_sequence": self.rendered_session_sequence(),
                        "panes": panes,
                    }),
                );
            }
            AutomationMethod::Inspect => {
                let pane_id = pane_id.unwrap();
                let Some(pane) = self.pane_description(pane_id) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                self.reply_automation(
                    target,
                    serde_json::json!({
                        "session": self.name,
                        "session_sequence": self.session_sequence,
                        "layout_sequence": self.layout_revision,
                        "rendered_session_sequence": self.rendered_session_sequence(),
                        "pane": pane,
                        "limits": automation_limits(),
                    }),
                );
            }
            AutomationMethod::InspectMedia => {
                let pane_id = pane_id.unwrap();
                let status = self.vivid.pane_status(
                    pane_id,
                    self.outer_projection_revision,
                    &self.outer_attachment_generations,
                    self.relay_metrics(),
                );
                self.reply_automation(target, serde_json::to_value(status).unwrap());
            }
            AutomationMethod::Split { axis } => {
                let pane_id = pane_id.unwrap();
                match self.automation_split(pane_id, axis) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Focus => {
                let pane_id = pane_id.unwrap();
                match self.automation_focus(pane_id) {
                    Ok(()) => self.reply_automation(target, serde_json::Value::Null),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::ClosePane => {
                let pane_id = pane_id.unwrap();
                self.close_pane(pane_id);
                self.reply_automation(target, serde_json::Value::Null);
            }
            AutomationMethod::Typing { text } => {
                self.automation_input(target, pane_id.unwrap(), text.into_bytes());
            }
            AutomationMethod::Key {
                key,
                modifiers,
                repeat,
            } => {
                let pane_id = pane_id.unwrap();
                let Some(pane) = self.panes.get(&pane_id) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                match encode_automation_key(&key, &modifiers, pane.terminal.modes()) {
                    Ok(encoded) => {
                        let total = encoded.len().saturating_mul(usize::from(repeat));
                        if total > 1024 * 1024 {
                            self.reply_automation_error(
                                target,
                                AutomationError::new(
                                    "limit_exceeded",
                                    "encoded key input exceeds 1 MiB",
                                ),
                            );
                            return;
                        }
                        self.automation_input(target, pane_id, encoded.repeat(usize::from(repeat)));
                    }
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Paste { text } => {
                let pane_id = pane_id.unwrap();
                let bracketed = self
                    .panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.terminal.modes().bracketed_paste);
                let mut bytes = sanitize_bracketed_paste(text.as_bytes());
                if bracketed {
                    bytes.splice(0..0, b"\x1b[200~".iter().copied());
                    bytes.extend_from_slice(b"\x1b[201~");
                }
                self.automation_input(target, pane_id, bytes);
            }
            AutomationMethod::GetText { rows } => {
                let pane = &self.panes[&pane_id.unwrap()];
                let text = match rows {
                    Some(rows) => pane.terminal.latest_text(usize::from(rows)),
                    None => pane
                        .terminal
                        .visible_text(pane.copy.as_ref().map_or(0, |copy| copy.offset)),
                };
                if text.len() > AUTOMATION_REPLY_LIMIT {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "limit_exceeded",
                            "pane text exceeds the 16 MiB reply limit",
                        ),
                    );
                    return;
                }
                self.reply_automation(target, serde_json::Value::String(text));
            }
            AutomationMethod::GetGrid {
                start_line,
                row_count,
                since_screen,
            } => {
                let pane_id = pane_id.unwrap();
                match self.grid_snapshot(pane_id, start_line, row_count, since_screen) {
                    Ok(snapshot) => self.reply_automation(target, snapshot),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::WaitText {
                text,
                regex,
                after_screen,
                timeout_ms,
            } => {
                let current = self.panes[&pane_id.unwrap()].screen_sequence;
                if after_screen.is_some_and(|sequence| sequence > current) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "sequence_gap",
                            format!(
                                "screen sequence is {current}, before requested {after_screen:?}"
                            ),
                        ),
                    );
                    return;
                }
                let pattern = if regex {
                    if text.len() > 8 * 1024 {
                        self.reply_automation_error(
                            target,
                            AutomationError::new(
                                "limit_exceeded",
                                "regular expression exceeds 8 KiB",
                            ),
                        );
                        return;
                    }
                    match regex::Regex::new(&text) {
                        Ok(regex) => AutomationTextPattern::Regex(regex),
                        Err(error) => {
                            self.reply_automation_error(
                                target,
                                AutomationError::new("regex_invalid", error.to_string()),
                            );
                            return;
                        }
                    }
                } else {
                    AutomationTextPattern::Literal(text)
                };
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Text {
                        pattern,
                        after_screen,
                    },
                });
            }
            AutomationMethod::WaitScreenChange {
                after_screen,
                timeout_ms,
            } => {
                let pane_id = pane_id.unwrap();
                if after_screen
                    .is_some_and(|sequence| sequence > self.panes[&pane_id].screen_sequence)
                {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "sequence_gap",
                            "after-screen is newer than the pane screen sequence",
                        ),
                    );
                    return;
                }
                let after_screen = after_screen.unwrap_or(self.panes[&pane_id].screen_sequence);
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id: Some(pane_id),
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::ScreenChange { after_screen },
                });
            }
            AutomationMethod::WaitScreenStable {
                quiet_ms,
                after_screen,
                timeout_ms,
            } => {
                let current = self.panes[&pane_id.unwrap()].screen_sequence;
                if after_screen.is_some_and(|sequence| sequence > current) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "sequence_gap",
                            "after-screen is newer than the pane screen sequence",
                        ),
                    );
                    return;
                }
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::ScreenStable {
                        quiet: Duration::from_millis(quiet_ms),
                        after_screen,
                    },
                })
            }
            AutomationMethod::WaitRendered {
                after_session,
                timeout_ms,
            } => {
                if self.attached.is_none() {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("unsupported", "session has no attached client"),
                    );
                } else {
                    self.add_automation_waiter(AutomationWaiter {
                        reply: target,
                        pane_id: None,
                        deadline: deadline(timeout_ms),
                        kind: AutomationWaitKind::Rendered { after_session },
                    });
                }
            }
            AutomationMethod::WaitExit { timeout_ms } => {
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Exit,
                })
            }
            AutomationMethod::WaitMedia {
                after_virtual_revision,
                after_outer_revision,
                timeout_ms,
            } => {
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Media {
                        after_virtual_revision,
                        after_outer_revision,
                    },
                });
            }
        }
    }

    fn client_is(&self, id: u64) -> bool {
        self.attached.as_ref().is_some_and(|client| client.id == id)
    }

    fn resolve_automation_pane(
        &self,
        requested: Option<PaneId>,
        allow_focused: bool,
    ) -> Result<PaneId, AutomationError> {
        if let Some(pane) = requested {
            return self
                .panes
                .contains_key(&pane)
                .then_some(pane)
                .ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane} does not exist"))
                });
        }
        if allow_focused {
            return self
                .active_tab()
                .map(|tab| tab.focused)
                .filter(|pane| self.panes.contains_key(pane))
                .ok_or_else(|| AutomationError::new("no_focused_pane", "no focused vvmux pane"));
        }
        Err(AutomationError::new(
            "invalid_params",
            "this command requires a pane ID",
        ))
    }

    fn reply_automation(&mut self, target: AutomationReplyTarget, result: serde_json::Value) {
        self.finish_automation_request(target.client_id, target.request_id);
        self.send_automation_response(
            &target,
            AutomationResponse::success(target.request_id, result),
        );
    }

    fn reply_automation_error(&mut self, target: AutomationReplyTarget, error: AutomationError) {
        self.finish_automation_request(target.client_id, target.request_id);
        self.send_automation_response(
            &target,
            AutomationResponse {
                id: target.request_id,
                ok: false,
                result: None,
                error: Some(error),
            },
        );
    }

    fn send_automation_response(
        &self,
        target: &AutomationReplyTarget,
        response: AutomationResponse,
    ) {
        let job = AutomationResponseJob {
            writer: target.writer.clone(),
            response,
        };
        if self.response_sender.try_send(job).is_err() {
            target.cancel.cancel();
        }
    }

    fn finish_automation_request(&mut self, client_id: u64, request_id: u64) {
        let mut empty = false;
        if let Some(requests) = self.automation_inflight.get_mut(&client_id) {
            requests.remove(&request_id);
            empty = requests.is_empty();
        }
        if empty {
            self.automation_inflight.remove(&client_id);
        }
    }

    fn automation_split(
        &mut self,
        pane_id: PaneId,
        axis: Axis,
    ) -> Result<serde_json::Value, AutomationError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        let tab_id = self.tabs[tab_index].id;
        let mut candidate = self.tabs[tab_index]
            .tree
            .clone()
            .ok_or_else(|| AutomationError::new("unsupported", "tab has no tiled layout"))?;
        if !candidate.contains(pane_id) {
            return Err(AutomationError::new(
                "unsupported",
                "floating panes cannot be split",
            ));
        }
        let new_pane_id = self.next_pane_id;
        candidate
            .split(pane_id, new_pane_id, axis, self.content_area())
            .map_err(|_| AutomationError::new("invalid_state", "pane is too small to split"))?;
        self.spawn_pane(new_pane_id, tab_id)
            .map_err(|error| AutomationError::new("pty_spawn_failed", error.to_string()))?;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);
        self.tabs[tab_index].tree = Some(candidate);
        self.tabs[tab_index].set_focus(new_pane_id);
        self.force_full = true;
        self.relayout();
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "new_pane_id": new_pane_id,
            "tab_id": tab_id,
            "session_sequence": self.session_sequence,
        }))
    }

    fn automation_focus(&mut self, pane_id: PaneId) -> Result<(), AutomationError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        let tab = &mut self.tabs[tab_index];
        if tab
            .floating
            .get(pane_id)
            .is_some_and(|floating| !floating.pinned)
        {
            tab.floating.ordinary_visible = true;
        }
        if tab.zoomed.is_some_and(|zoomed| zoomed != pane_id) {
            tab.zoomed = None;
        }
        tab.set_focus(pane_id);
        self.active_tab = tab_index;
        self.force_full = true;
        self.relayout();
        Ok(())
    }

    fn automation_input(&mut self, target: AutomationReplyTarget, pane_id: PaneId, bytes: Vec<u8>) {
        if bytes.len() > 1024 * 1024 {
            self.reply_automation_error(
                target,
                AutomationError::new("limit_exceeded", "PTY input exceeds 1 MiB"),
            );
            return;
        }
        if !self.register_pending_actor_work(&target) {
            self.reply_automation_error(
                target,
                AutomationError::new("limit_exceeded", "session pending-work quota is exhausted"),
            );
            return;
        }
        let receiver = match self
            .panes
            .get(&pane_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pane no longer exists"))
            .and_then(|pane| pane.input.send_with_completion(&bytes))
        {
            Ok(receiver) => receiver,
            Err(error) => {
                self.complete_pending_actor_work(&target);
                self.reply_automation_error(
                    target,
                    AutomationError::new("pty_closed", error.to_string()),
                );
                return;
            }
        };
        let sender = self.sender.clone();
        let completion_target = target.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("vvmux-automation-input-{pane_id}"))
            .spawn(move || {
                let result = match receiver.recv_timeout(PTY_WRITE_TIMEOUT) {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        Err("PTY write did not complete within five seconds".into())
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        Err("PTY writer closed before acknowledging input".into())
                    }
                };
                let _ = sender.send(ActorEvent::AutomationInputComplete {
                    reply: completion_target,
                    result,
                });
            });
        if let Err(error) = spawn {
            self.complete_pending_actor_work(&target);
            self.reply_automation_error(
                target,
                AutomationError::new("unsupported", error.to_string()),
            );
        }
    }

    /// Admit work that may block outside the single-writer session actor.
    ///
    /// The actor performs ordered validation and admission, records a bounded completion key, and
    /// yields. A worker owns the blocking wait and can only return through a typed `ActorEvent`;
    /// mutation and reply emission resume on the actor after this key is released.
    fn register_pending_actor_work(&mut self, target: &AutomationReplyTarget) -> bool {
        if self.pending_actor_work.len() >= MAX_PENDING_ACTOR_WORK {
            return false;
        }
        self.pending_actor_work
            .insert((target.client_id, target.request_id))
    }

    fn complete_pending_actor_work(&mut self, target: &AutomationReplyTarget) {
        self.pending_actor_work
            .remove(&(target.client_id, target.request_id));
    }

    fn pane_description(&self, pane_id: PaneId) -> Option<serde_json::Value> {
        let pane = self.panes.get(&pane_id)?;
        let tab_index = self.tabs.iter().position(|tab| tab.contains(pane_id))?;
        let tab = &self.tabs[tab_index];
        let area = self.content_area();
        let tiled_geometry = tab.tree.as_ref().and_then(|tree| {
            tree.geometry(area)
                .into_iter()
                .find(|(pane, _)| *pane == pane_id)
        });
        let floating = tab.floating.get(pane_id);
        let (layer, outer) = if let Some(floating) = floating {
            (
                if floating.pinned {
                    "pinned"
                } else {
                    "floating"
                },
                floating.rect,
            )
        } else {
            (
                "tiled",
                tiled_geometry.map_or(Rect::default(), |(_, rect)| rect),
            )
        };
        let visible = tab_index == self.active_tab
            && visible_projections(tab, area)
                .iter()
                .any(|projection| projection.pane_id == pane_id);
        let cursor = pane.terminal.cursor();
        Some(serde_json::json!({
            "pane_id": pane_id,
            "tab_id": tab.id,
            "active_tab": tab_index == self.active_tab,
            "focused": tab.focused == pane_id,
            "visible": visible,
            "layer": layer,
            "zoomed": tab.zoomed == Some(pane_id),
            "title": pane.terminal.title(),
            "geometry": rect_json(outer),
            "content_geometry": rect_json(outer.content()),
            "columns": pane.terminal.cols(),
            "rows": pane.terminal.rows(),
            "history_size": pane.terminal.history_len(),
            "display_offset": pane.copy.as_ref().map_or(0, |copy| copy.offset),
            "copy_mode": pane.copy.is_some(),
            "cursor": { "row": cursor.0, "column": cursor.1, "visible": pane.terminal.modes().cursor_visible },
            "modes": terminal_mode_names(pane.terminal.modes()),
            "screen": if pane.terminal.alternate_screen() { "alternate" } else { "primary" },
            "process_state": "running",
            "screen_sequence": pane.screen_sequence,
            "session_sequence": self.session_sequence,
        }))
    }

    fn grid_snapshot(
        &self,
        pane_id: PaneId,
        start_line: Option<isize>,
        row_count: Option<u16>,
        since_screen: Option<u64>,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        let display_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let mut full = true;
        let mut gap = None;
        let mut viewport_rows = None;
        if let Some(since) = since_screen {
            if start_line.is_some() || row_count.is_some() {
                return Err(AutomationError::new(
                    "invalid_params",
                    "--since-screen conflicts with explicit line ranges",
                ));
            }
            if since > pane.screen_sequence {
                return Err(AutomationError::new(
                    "sequence_gap",
                    "requested screen sequence is newer than the pane",
                ));
            }
            if since == pane.screen_sequence {
                full = false;
                viewport_rows = Some(Vec::new());
            } else if display_offset > 0 {
                gap = Some(serde_json::json!({
                    "requested_sequence": since,
                    "oldest_sequence": pane.screen_changes.front().map(|change| change.sequence),
                    "current_sequence": pane.screen_sequence,
                    "reason": "copy_view",
                }));
            } else {
                let oldest = pane
                    .screen_changes
                    .front()
                    .map_or(pane.screen_sequence, |change| change.sequence);
                if since.saturating_add(1) < oldest {
                    gap = Some(serde_json::json!({
                        "requested_sequence": since,
                        "oldest_sequence": oldest,
                        "current_sequence": pane.screen_sequence,
                        "reason": "history_evicted",
                    }));
                } else {
                    let mut changed = std::collections::BTreeSet::new();
                    let mut invalidated = false;
                    for change in pane
                        .screen_changes
                        .iter()
                        .filter(|change| change.sequence > since)
                    {
                        match &change.rows {
                            Some(rows) => changed.extend(rows.iter().copied()),
                            None => invalidated = true,
                        }
                    }
                    if !invalidated {
                        full = false;
                        viewport_rows = Some(changed.into_iter().collect());
                    }
                }
            }
        }

        let (range_start, count) = if let (Some(start), Some(count)) = (start_line, row_count) {
            (start, usize::from(count))
        } else {
            (-(display_offset as isize), pane.terminal.rows())
        };
        let available_start = -(pane.terminal.history_len() as isize);
        let available_end = pane.terminal.rows() as isize;
        if range_start < available_start
            || range_start > available_end
            || range_start.saturating_add(count as isize) > available_end
        {
            return Err(AutomationError::new(
                "invalid_params",
                format!("grid range must be within {available_start}..{available_end}"),
            ));
        }
        let selected_rows = viewport_rows.unwrap_or_else(|| (0..count).collect());
        let mut estimated_bytes = 4096_usize;
        for row_index in selected_rows.iter().copied().filter(|row| *row < count) {
            let line = range_start + row_index as isize;
            let Some(cells) = pane.terminal.viewport_line(line) else {
                continue;
            };
            estimated_bytes = estimated_bytes
                .checked_add(pane.terminal.cols().saturating_mul(160))
                .ok_or_else(|| {
                    AutomationError::new("limit_exceeded", "grid reply size overflows")
                })?;
            for cell in cells.iter().take(pane.terminal.cols()) {
                estimated_bytes = estimated_bytes
                    .checked_add(cell.combining.len())
                    .and_then(|size| {
                        size.checked_add(cell.hyperlink.as_ref().map_or(0, |link| {
                            link.uri.len() + link.id.as_ref().map_or(0, String::len)
                        }))
                    })
                    .ok_or_else(|| {
                        AutomationError::new("limit_exceeded", "grid reply size overflows")
                    })?;
            }
            if estimated_bytes > AUTOMATION_REPLY_LIMIT {
                return Err(AutomationError::new(
                    "limit_exceeded",
                    "estimated grid reply exceeds 16 MiB; request fewer rows",
                ));
            }
        }
        let mut styles = Vec::<serde_json::Value>::new();
        let mut style_ids = HashMap::<StyleKey, usize>::new();
        let mut rows = Vec::new();
        let mut returned_lines = Vec::new();
        for row_index in selected_rows {
            if row_index >= count {
                continue;
            }
            let line = range_start + row_index as isize;
            let Some(source_cells) = pane.terminal.viewport_line(line) else {
                continue;
            };
            let mut cells = source_cells.to_vec();
            cells.resize(pane.terminal.cols(), Cell::default());
            cells.truncate(pane.terminal.cols());
            let serialized = cells
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let key = StyleKey::from(cell);
                    let style_id = *style_ids.entry(key.clone()).or_insert_with(|| {
                        let id = styles.len();
                        styles.push(style_json(&key));
                        id
                    });
                    let width = if cell.wide_continuation || cell.leading_wide_spacer {
                        0
                    } else if cells
                        .get(column + 1)
                        .is_some_and(|next| next.wide_continuation)
                    {
                        2
                    } else {
                        1
                    };
                    let text = if cell.wide_continuation || cell.leading_wide_spacer {
                        String::new()
                    } else if cell.tab_width.is_some() {
                        "\t".into()
                    } else {
                        let mut text = cell.ch.to_string();
                        text.push_str(&cell.combining);
                        text
                    };
                    serde_json::json!({
                        "text": text,
                        "width": width,
                        "kind": if cell.wide_continuation { "continuation" } else if cell.leading_wide_spacer { "leading_wide_spacer" } else if cell.tab_width.is_some() { "tab" } else { "character" },
                        "tab_width": cell.tab_width,
                        "style": style_id,
                    })
                })
                .collect::<Vec<_>>();
            returned_lines.push(line);
            rows.push(serde_json::json!({
                "grid_line": line,
                "viewport_row": ((line + display_offset as isize) >= 0
                    && (line + display_offset as isize) < pane.terminal.rows() as isize)
                    .then_some(line + display_offset as isize),
                "wrapped": pane.terminal.line_wrapped(line).unwrap_or(false),
                "cells": serialized,
            }));
        }
        let cursor = pane.terminal.cursor();
        let selection = pane.copy.as_ref().map(|copy| serde_json::json!({
            "cursor": { "row": copy.row, "column": copy.column },
            "start": copy.selection_start.map(|(line, column)| serde_json::json!({ "line": line, "column": column })),
        }));
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "screen_sequence": pane.screen_sequence,
            "session_sequence": self.session_sequence,
            "full": full,
            "gap": gap,
            "grid": { "columns": pane.terminal.cols(), "rows": pane.terminal.rows() },
            "returned_lines": {
                "start": returned_lines.first(),
                "end": returned_lines.last(),
            },
            "history_size": pane.terminal.history_len(),
            "display_offset": display_offset,
            "screen": if pane.terminal.alternate_screen() { "alternate" } else { "primary" },
            "terminal_modes": terminal_mode_names(pane.terminal.modes()),
            "cursor": { "line": cursor.0, "column": cursor.1, "visible": pane.terminal.modes().cursor_visible },
            "selection": selection,
            "styles": styles,
            "rows": rows,
        }))
    }

    fn add_automation_waiter(&mut self, waiter: AutomationWaiter) {
        if self.automation_waiters.len() >= MAX_AUTOMATION_WAITERS {
            self.reply_automation_error(
                waiter.reply,
                AutomationError::new("limit_exceeded", "too many automation waiters"),
            );
            return;
        }
        self.automation_waiters.push(waiter);
        self.check_automation_waiters();
    }

    fn next_automation_deadline(&self) -> Duration {
        let now = Instant::now();
        self.automation_waiters
            .iter()
            .map(|waiter| {
                let stable_ready = match (&waiter.kind, waiter.pane_id) {
                    (AutomationWaitKind::ScreenStable { quiet, .. }, Some(pane)) => self
                        .panes
                        .get(&pane)
                        .map(|pane| pane.last_screen_change + *quiet),
                    _ => None,
                };
                stable_ready
                    .map_or(waiter.deadline, |ready| ready.min(waiter.deadline))
                    .saturating_duration_since(now)
            })
            .min()
            .unwrap_or(Duration::from_secs(1))
    }

    fn check_automation_waiters(&mut self) {
        let now = Instant::now();
        let waiters = std::mem::take(&mut self.automation_waiters);
        for waiter in waiters {
            if waiter.deadline <= now {
                self.reply_automation_error(
                    waiter.reply,
                    AutomationError::new("timeout", "automation wait timed out"),
                );
                continue;
            }
            match self.automation_waiter_result(&waiter, now) {
                Some(Ok(result)) => self.reply_automation(waiter.reply, result),
                Some(Err(error)) => self.reply_automation_error(waiter.reply, error),
                None => self.automation_waiters.push(waiter),
            }
        }
    }

    fn automation_waiter_result(
        &self,
        waiter: &AutomationWaiter,
        now: Instant,
    ) -> Option<Result<serde_json::Value, AutomationError>> {
        match &waiter.kind {
            AutomationWaitKind::Media {
                after_virtual_revision,
                after_outer_revision,
            } => {
                let pane_id = waiter.pane_id?;
                let status = self.vivid.pane_status(
                    pane_id,
                    self.outer_projection_revision,
                    &self.outer_attachment_generations,
                    self.relay_metrics(),
                );
                let virtual_ready = after_virtual_revision
                    .is_none_or(|revision| status.virtual_projection_revision > revision);
                let outer_ready = after_outer_revision
                    .is_none_or(|revision| status.outer_projection_revision > revision);
                (virtual_ready && outer_ready).then(|| {
                    serde_json::to_value(status).map_err(|error| {
                        AutomationError::new("serialization_failed", error.to_string())
                    })
                })
            }
            AutomationWaitKind::Rendered { after_session } => {
                let Some(client) = self.attached.as_ref() else {
                    return Some(Err(AutomationError::new(
                        "unsupported",
                        "attached client disconnected while waiting for render",
                    )));
                };
                (client.rendered_session_sequence >= *after_session).then(|| {
                    Ok(serde_json::json!({
                        "session_sequence": self.session_sequence,
                        "rendered_session_sequence": client.rendered_session_sequence,
                    }))
                })
            }
            AutomationWaitKind::Exit => {
                let pane_id = waiter.pane_id?;
                self.exit_tombstones
                    .iter()
                    .rev()
                    .find(|exit| exit.pane_id == pane_id)
                    .map(|exit| Ok(exit_result(pane_id, exit.status)))
                    .or_else(|| {
                        (!self.panes.contains_key(&pane_id)).then(|| {
                            Err(AutomationError::new(
                                "pane_not_found",
                                "pane does not exist and has no retained exit status",
                            ))
                        })
                    })
            }
            kind => {
                let pane_id = waiter.pane_id?;
                let Some(pane) = self.panes.get(&pane_id) else {
                    return Some(Err(AutomationError::new(
                        "pane_not_found",
                        "pane closed while waiting",
                    )));
                };
                match kind {
                    AutomationWaitKind::Text {
                        pattern,
                        after_screen,
                    } => {
                        if after_screen.is_some_and(|sequence| pane.screen_sequence <= sequence) {
                            return None;
                        }
                        let text = pane
                            .terminal
                            .visible_text(pane.copy.as_ref().map_or(0, |copy| copy.offset));
                        let matched = match pattern {
                            AutomationTextPattern::Literal(pattern) => text.contains(pattern),
                            AutomationTextPattern::Regex(pattern) => pattern.is_match(&text),
                        };
                        matched.then(|| {
                            Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "screen_sequence": pane.screen_sequence,
                            }))
                        })
                    }
                    AutomationWaitKind::ScreenChange { after_screen } => {
                        (pane.screen_sequence > *after_screen).then(|| {
                            Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "screen_sequence": pane.screen_sequence,
                            }))
                        })
                    }
                    AutomationWaitKind::ScreenStable {
                        quiet,
                        after_screen,
                    } => {
                        let newer =
                            after_screen.is_none_or(|sequence| pane.screen_sequence > sequence);
                        (newer && now.saturating_duration_since(pane.last_screen_change) >= *quiet)
                            .then(|| {
                                Ok(serde_json::json!({
                                    "pane_id": pane_id,
                                    "screen_sequence": pane.screen_sequence,
                                    "quiet_ms": quiet.as_millis(),
                                }))
                            })
                    }
                    _ => None,
                }
            }
        }
    }

    fn complete_exit_waiters(&mut self, pane_id: PaneId, status: Option<PtyExitStatus>) {
        let waiters = std::mem::take(&mut self.automation_waiters);
        for waiter in waiters {
            if waiter.pane_id == Some(pane_id) && matches!(waiter.kind, AutomationWaitKind::Exit) {
                self.reply_automation(waiter.reply, exit_result(pane_id, status));
            } else {
                self.automation_waiters.push(waiter);
            }
        }
    }

    fn rendered_session_sequence(&self) -> u64 {
        self.attached
            .as_ref()
            .map_or(0, |client| client.rendered_session_sequence)
    }

    fn mark_pane_screen_change(&mut self, pane_id: PaneId, rows: Option<Vec<usize>>) {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        pane.screen_sequence = pane.screen_sequence.wrapping_add(1);
        pane.last_screen_change = Instant::now();
        pane.screen_changes.push_back(ScreenChange {
            sequence: pane.screen_sequence,
            rows,
        });
        while pane.screen_changes.len() > SCREEN_CHANGE_HISTORY {
            pane.screen_changes.pop_front();
        }
        self.session_sequence = self.session_sequence.wrapping_add(1);
    }

    fn input(&mut self, bytes: Vec<u8>) {
        let Some(pane_id) = self.active_tab().map(|tab| tab.focused) else {
            return;
        };
        if self
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.copy.is_some())
        {
            self.copy_input(pane_id, &bytes);
        } else if let Some(pane) = self.panes.get_mut(&pane_id) {
            let failure = queue_pane_input(pane, &bytes);
            self.report_input_failure(pane_id, failure);
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if self.pointer_drag.is_some() {
            match mouse.kind {
                MouseKind::Release if !mouse.shift && mouse.button == 0 => {
                    // A valid left-button release commits the live rectangle/tree.
                    self.pointer_drag = None;
                    return;
                }
                MouseKind::Move if !mouse.shift && mouse.button == 0 => {
                    self.update_pointer_drag(mouse);
                    return;
                }
                // A new press, Shift-modified event, wheel, wrong button, or other malformed
                // sequence cancels and restores the press-time state before normal handling.
                _ => self.cancel_pointer_drag(true),
            }
        }
        if matches!(mouse.kind, MouseKind::Move | MouseKind::Release) {
            return;
        }
        let area = self.content_area();
        let Some((tab_id, focused, original_tree, projection)) =
            self.active_tab().and_then(|tab| {
                // Top-down hit testing over the same ordered projections that composition paints,
                // so clicking always addresses the visually topmost pane.
                let hit = visible_projections(tab, area)
                    .into_iter()
                    .rev()
                    .find(|projection| projection.outer.contains(mouse.x, mouse.y))?;
                Some((tab.id, tab.focused, tab.tree.clone(), hit))
            })
        else {
            return;
        };
        let (pane_id, rect) = (projection.pane_id, projection.outer);
        let focus_changed = focused != pane_id;
        let raised = projection.layer != PaneLayer::Tiled && focus_changed;
        if focus_changed {
            self.end_float_mode(true);
        }
        if let Some(tab) = self.active_tab_mut() {
            tab.set_focus(pane_id);
        }
        if focus_changed {
            if raised {
                self.force_full = true;
            }
            self.projection_changed();
        }
        let on_vertical = mouse.x == rect.x || mouse.x + 1 == rect.x + rect.width;
        let on_horizontal = mouse.y == rect.y || mouse.y + 1 == rect.y + rect.height;
        if projection.layer != PaneLayer::Tiled {
            if mouse.kind == MouseKind::Press
                && mouse.button == 0
                && !mouse.shift
                && let Some(target) = float_pointer_target(
                    rect,
                    mouse.x,
                    mouse.y,
                    self.config.floating.border_drag_margin,
                )
            {
                self.pointer_drag = Some(match target {
                    FloatPointerTarget::Move => PointerDrag::Move {
                        tab_id,
                        pane: pane_id,
                        start: (mouse.x, mouse.y),
                        original: rect,
                    },
                    FloatPointerTarget::Resize(edges) => PointerDrag::Resize {
                        tab_id,
                        pane: pane_id,
                        edges,
                        start: (mouse.x, mouse.y),
                        original: rect,
                    },
                });
                self.schedule_render();
                return;
            }
        } else if mouse.kind == MouseKind::Press
            && mouse.button == 0
            && (on_vertical || on_horizontal)
        {
            let axis = if on_vertical {
                Axis::Vertical
            } else {
                Axis::Horizontal
            };
            let boundary = if axis == Axis::Vertical {
                mouse.x
            } else {
                mouse.y
            };
            let Some(original) = original_tree else {
                return;
            };
            self.pointer_drag = Some(PointerDrag::TiledBoundary {
                tab_id,
                axis,
                boundary,
                last: boundary,
                original,
            });
            self.schedule_render();
            return;
        }

        let content = rect.content();
        if mouse.x < content.x
            || mouse.x >= content.x + content.width
            || mouse.y < content.y
            || mouse.y >= content.y + content.height
        {
            self.schedule_render();
            return;
        }
        let mut translated = None;
        let mut copy_view_render = false;
        let mut media_view_changed = false;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let modes = pane.terminal.modes();
            let application_mouse = !mouse.shift
                && (modes.mouse_clicks || (mouse.kind == MouseKind::Move && modes.mouse_motion));
            if application_mouse {
                let mut button = u16::from(mouse.button);
                if mouse.kind == MouseKind::Wheel {
                    button |= 64;
                }
                if mouse.kind == MouseKind::Move {
                    button |= 32;
                }
                let terminator = if mouse.kind == MouseKind::Release {
                    'm'
                } else {
                    'M'
                };
                translated = Some(format!(
                    "\x1b[<{button};{};{}{terminator}",
                    mouse.x - content.x + 1,
                    mouse.y - content.y + 1
                ));
            } else if mouse.kind == MouseKind::Wheel || mouse.shift {
                copy_view_render = true;
                let previous_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
                let copy = pane.copy.get_or_insert(CopyState {
                    offset: 0,
                    row: 0,
                    column: 0,
                    selection_start: None,
                });
                if mouse.button == 0 {
                    copy.offset = (copy.offset + 3).min(pane.terminal.history_len());
                } else {
                    copy.offset = copy.offset.saturating_sub(3);
                    if copy.offset == 0 && !mouse.shift {
                        pane.copy = None;
                    }
                }
                media_view_changed =
                    previous_offset != pane.copy.as_ref().map_or(0, |copy| copy.offset);
            }
        }
        if media_view_changed {
            self.projection_changed();
        } else if copy_view_render {
            self.schedule_render();
        }
        if let Some(translated) = translated {
            self.send_pane_input(pane_id, translated.as_bytes());
        }
    }

    fn update_pointer_drag(&mut self, mouse: MouseEvent) {
        let Some(mut drag) = self.pointer_drag.take() else {
            return;
        };
        let active_tab_id = self.active_tab().map(|tab| tab.id);
        let drag_tab_id = match &drag {
            PointerDrag::TiledBoundary { tab_id, .. }
            | PointerDrag::Move { tab_id, .. }
            | PointerDrag::Resize { tab_id, .. } => *tab_id,
        };
        if active_tab_id != Some(drag_tab_id) {
            self.pointer_drag = Some(drag);
            self.cancel_pointer_drag(true);
            return;
        }

        let area = self.content_area();
        let (changed, floating) = match &mut drag {
            PointerDrag::TiledBoundary {
                axis,
                boundary,
                last,
                ..
            } => {
                let current = match axis {
                    Axis::Vertical => mouse.x,
                    Axis::Horizontal => mouse.y,
                };
                let mut changed = false;
                while *last < current {
                    if !self.resize_boundary(*axis, *boundary, true) {
                        break;
                    }
                    *last += 1;
                    *boundary += 1;
                    changed = true;
                }
                while *last > current {
                    if !self.resize_boundary(*axis, *boundary, false) {
                        break;
                    }
                    *last -= 1;
                    *boundary = boundary.saturating_sub(1);
                    changed = true;
                }
                (changed, false)
            }
            PointerDrag::Move {
                pane,
                start,
                original,
                ..
            } => {
                let dx = i32::from(mouse.x) - i32::from(start.0);
                let dy = i32::from(mouse.y) - i32::from(start.1);
                let changed = self
                    .active_tab_mut()
                    .is_some_and(|tab| tab.floating.move_from(*pane, *original, dx, dy, area));
                (changed, true)
            }
            PointerDrag::Resize {
                pane,
                edges,
                start,
                original,
                ..
            } => {
                let dx = i32::from(mouse.x) - i32::from(start.0);
                let dy = i32::from(mouse.y) - i32::from(start.1);
                let changed = self.active_tab_mut().is_some_and(|tab| {
                    tab.floating
                        .resize_from(*pane, *original, *edges, dx, dy, area)
                });
                (changed, true)
            }
        };
        self.pointer_drag = Some(drag);
        if changed {
            if floating {
                self.force_full = true;
            }
            self.relayout();
        }
    }

    fn cancel_pointer_drag(&mut self, restore: bool) {
        let Some(drag) = self.pointer_drag.take() else {
            return;
        };
        if !restore {
            return;
        }
        let area = self.content_area();
        let changed = match drag {
            PointerDrag::TiledBoundary {
                tab_id, original, ..
            } => self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == tab_id)
                .is_some_and(|tab| {
                    if tab.tree.as_ref() == Some(&original) {
                        false
                    } else {
                        tab.tree = Some(original);
                        true
                    }
                }),
            PointerDrag::Move {
                tab_id,
                pane,
                original,
                ..
            }
            | PointerDrag::Resize {
                tab_id,
                pane,
                original,
                ..
            } => self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == tab_id)
                .is_some_and(|tab| tab.floating.set_rect(pane, original, area)),
        };
        if changed {
            self.force_full = true;
            self.relayout();
        }
    }

    fn resize_boundary(&mut self, axis: Axis, boundary: u16, positive: bool) -> bool {
        let area = self.content_area();
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        let Some(tree) = tab.tree.as_mut() else {
            return false;
        };
        let geometry = tree.geometry(area);
        let candidate = geometry
            .iter()
            .find_map(|(pane, rect)| match (axis, positive) {
                (Axis::Vertical, true) if rect.x + rect.width == boundary => {
                    Some((*pane, Direction::Right))
                }
                (Axis::Vertical, false) if rect.x == boundary => Some((*pane, Direction::Left)),
                (Axis::Horizontal, true) if rect.y + rect.height == boundary => {
                    Some((*pane, Direction::Down))
                }
                (Axis::Horizontal, false) if rect.y == boundary => Some((*pane, Direction::Up)),
                _ => None,
            });
        candidate.is_some_and(|(pane, direction)| tree.resize(pane, direction, area))
    }

    fn action(&mut self, action: Action) {
        self.cancel_pointer_drag(true);
        // Any prefix action during a float-edit mode invalidates it (focus, tab, zoom, and
        // layout changes are all cancellation triggers); restore the entry rectangle first.
        self.end_float_mode(true);
        match action {
            Action::Split(axis) => self.split(axis),
            Action::Focus(direction) => self.focus(direction),
            Action::Resize(direction) => self.resize(direction),
            Action::NewTab if self.new_tab().is_ok() => {
                self.active_tab = self.tabs.len() - 1;
                self.relayout();
            }
            Action::NextTab if !self.tabs.is_empty() => {
                self.active_tab = (self.active_tab + 1) % self.tabs.len();
                self.force_full = true;
                self.relayout();
            }
            Action::PreviousTab if !self.tabs.is_empty() => {
                self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
                self.force_full = true;
                self.relayout();
            }
            Action::SelectTab(index) if index < self.tabs.len() => {
                self.active_tab = index;
                self.force_full = true;
                self.relayout();
            }
            Action::ClosePane => {
                if let Some(pane) = self.active_tab().map(|tab| tab.focused) {
                    self.close_pane(pane);
                }
            }
            Action::ToggleZoom => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.zoomed = if tab.zoomed.is_some() {
                        None
                    } else {
                        Some(tab.focused)
                    };
                    self.force_full = true;
                    self.relayout();
                }
            }
            Action::EnterCopyMode => {
                let rows = self.content_area().height.saturating_sub(2) as usize;
                if let Some(pane_id) = self.active_tab().map(|tab| tab.focused)
                    && let Some(pane) = self.panes.get_mut(&pane_id)
                {
                    pane.copy = Some(CopyState {
                        offset: 0,
                        row: rows.saturating_sub(1),
                        column: 0,
                        selection_start: None,
                    });
                    self.mark_pane_screen_change(pane_id, None);
                    self.schedule_render();
                }
            }
            Action::CopyInput(bytes) => {
                if let Some(pane) = self.active_tab().map(|tab| tab.focused) {
                    self.copy_input(pane, &bytes);
                }
            }
            Action::Paste => self.paste(),
            Action::NewFloatingPane => self.new_float(),
            Action::ToggleFloatingPanes => self.toggle_floats(),
            Action::TogglePanePinned => self.toggle_pin(),
            Action::EnterFloatingMoveMode => self.enter_float_mode(FloatingEditKind::Move),
            Action::EnterFloatingResizeMode => self.enter_float_mode(FloatingEditKind::Resize),
            _ => {}
        }
    }

    fn enter_float_mode(&mut self, kind: FloatingEditKind) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.zoomed.is_some() {
            self.status("unzoom before editing a floating pane");
            return;
        }
        let Some(float) = tab.floating.get(tab.focused) else {
            self.status("only floating panes can be moved or resized");
            return;
        };
        let (pane, original) = (float.pane_id, float.rect);
        let Some(next_mode) = self.next_float_mode.checked_add(1) else {
            self.status("floating edit mode ID space exhausted");
            return;
        };
        self.next_float_mode = next_mode;
        let mode_id = self.next_float_mode;
        self.float_modal = Some(FloatModal {
            mode_id,
            pane,
            kind,
            original,
        });
        if let Some(client) = &self.attached {
            let _ = crate::ipc::send(
                &client.writer,
                &ServerMessage::FloatingEditMode {
                    mode_id,
                    pane: Some(pane),
                    kind: Some(kind),
                },
            );
        }
    }

    /// End the active float-edit mode. `restore` puts the captured entry rectangle back
    /// (re-clamped against the current area); commit and destroyed-pane paths pass `false`.
    fn end_float_mode(&mut self, restore: bool) {
        let Some(modal) = self.float_modal.take() else {
            return;
        };
        if restore {
            let area = self.content_area();
            let changed = self
                .tabs
                .iter_mut()
                .find(|tab| tab.floating.contains(modal.pane))
                .is_some_and(|tab| tab.floating.set_rect(modal.pane, modal.original, area));
            if changed {
                self.force_full = true;
                self.relayout();
            }
        }
        if let Some(client) = &self.attached {
            let _ = crate::ipc::send(
                &client.writer,
                &ServerMessage::FloatingEditMode {
                    mode_id: modal.mode_id,
                    pane: None,
                    kind: None,
                },
            );
        }
    }

    fn float_edit(&mut self, mode_id: u64, command: FloatingEditCommand) {
        let Some(modal) = &self.float_modal else {
            return;
        };
        if modal.mode_id != mode_id {
            // A command from an already-ended mode raced its cancellation; ignore it.
            return;
        }
        let (pane, kind) = (modal.pane, modal.kind);
        match command {
            FloatingEditCommand::Commit => self.end_float_mode(false),
            FloatingEditCommand::Cancel => self.end_float_mode(true),
            FloatingEditCommand::Step { direction, cells } => {
                if !matches!(cells, 1 | 5) {
                    self.end_float_mode(true);
                    return;
                }
                let step = i32::from(cells);
                let (dx, dy) = match direction {
                    Direction::Left => (-step, 0),
                    Direction::Right => (step, 0),
                    Direction::Up => (0, -step),
                    Direction::Down => (0, step),
                };
                let area = self.content_area();
                let changed = self.active_tab_mut().is_some_and(|tab| match kind {
                    FloatingEditKind::Move => tab.floating.move_by(pane, dx, dy, area),
                    // Keyboard resize anchors the top-left corner: Left/Up shrink the
                    // bottom-right edges, Right/Down grow them.
                    FloatingEditKind::Resize => tab.floating.resize_by(
                        pane,
                        EdgeMask {
                            right: true,
                            bottom: true,
                            ..EdgeMask::default()
                        },
                        dx,
                        dy,
                        area,
                    ),
                });
                if changed {
                    self.force_full = true;
                    self.relayout();
                }
            }
        }
    }

    fn split(&mut self, axis: Axis) {
        let Some((focused, tree, tab_id)) = self
            .active_tab()
            .map(|tab| (tab.focused, tab.tree.clone(), tab.id))
        else {
            return;
        };
        let Some(tree) = tree else {
            self.status("no tiled pane to split");
            return;
        };
        if self
            .active_tab()
            .is_some_and(|tab| tab.floating.contains(focused))
        {
            self.status("cannot split a floating pane");
            return;
        }
        let pane_id = self.next_pane_id;
        let mut candidate = tree;
        if candidate
            .split(focused, pane_id, axis, self.content_area())
            .is_err()
        {
            self.status("pane is too small to split");
            return;
        }
        if self.spawn_pane(pane_id, tab_id).is_err() {
            self.status("could not spawn shell");
            return;
        }
        self.next_pane_id += 1;
        if let Some(tab) = self.active_tab_mut() {
            tab.tree = Some(candidate);
            tab.set_focus(pane_id);
        }
        self.relayout();
    }

    fn focus(&mut self, direction: Direction) {
        let area = self.content_area();
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if tab.zoomed.is_some() {
            return;
        }
        let projections = visible_projections(tab, area);
        if let Some(next) = directional_focus(&projections, tab.focused, direction) {
            tab.set_focus(next);
            self.force_full = true;
            self.projection_changed();
        }
    }

    fn resize(&mut self, direction: Direction) {
        let area = self.content_area();
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.zoomed.is_some() {
            return;
        }
        if tab.floating.contains(tab.focused) {
            self.status("floating panes resize with the resize mode or the mouse");
            return;
        }
        let changed = self.active_tab_mut().is_some_and(|tab| {
            let focused = tab.focused;
            tab.tree
                .as_mut()
                .is_some_and(|tree| tree.resize(focused, direction, area))
        });
        if changed {
            self.relayout();
        }
    }

    fn new_float(&mut self) {
        if self.active_tab().is_some_and(|tab| tab.zoomed.is_some()) {
            self.status("unzoom before creating a floating pane");
            return;
        }
        let pane_id = self.next_pane_id;
        let tab_id = self.active_tab().map_or(self.next_tab_id, |tab| tab.id);
        if self.spawn_pane(pane_id, tab_id).is_err() {
            self.status("could not spawn shell");
            return;
        }
        self.next_pane_id += 1;
        let area = self.content_area();
        let width_percent = self.config.floating.default_width_percent;
        let height_percent = self.config.floating.default_height_percent;
        if let Some(tab) = self.active_tab_mut() {
            tab.floating
                .insert(pane_id, area, width_percent, height_percent);
            tab.set_focus(pane_id);
        }
        self.force_full = true;
        self.relayout();
    }

    fn toggle_floats(&mut self) {
        if self.active_tab().is_some_and(|tab| tab.zoomed.is_some()) {
            self.status("unzoom before changing floating pane visibility");
            return;
        }
        if self.active_tab().is_none_or(|tab| tab.floating.is_empty()) {
            self.status("no floating panes in this tab");
            return;
        }
        if let Some(tab) = self.active_tab_mut() {
            tab.floating.ordinary_visible = !tab.floating.ordinary_visible;
            let focused_hidden = !tab.floating.ordinary_visible
                && tab
                    .floating
                    .get(tab.focused)
                    .is_some_and(|float| !float.pinned);
            if focused_hidden && let Some(next) = tab.fallback_focus() {
                tab.set_focus(next);
            }
        }
        self.force_full = true;
        self.relayout();
    }

    fn toggle_pin(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.zoomed.is_some() {
            self.status("unzoom before pinning a pane");
            return;
        }
        let Some(pinned) = tab.floating.get(tab.focused).map(|float| float.pinned) else {
            self.status("only floating panes can be pinned");
            return;
        };
        if let Some(tab) = self.active_tab_mut() {
            let focused = tab.focused;
            tab.floating.set_pinned(focused, !pinned);
        }
        self.force_full = true;
        self.projection_changed();
    }

    fn new_tab(&mut self) -> io::Result<()> {
        let pane_id = self.next_pane_id;
        let tab_id = self.next_tab_id;
        self.spawn_pane(pane_id, tab_id)?;
        self.next_pane_id += 1;
        self.tabs.push(Tab {
            id: tab_id,
            tree: Some(TiledNode::leaf(pane_id)),
            floating: FloatingLayer::default(),
            focused: pane_id,
            last_focused_tiled: Some(pane_id),
            zoomed: None,
        });
        self.next_tab_id += 1;
        self.schedule_render();
        Ok(())
    }

    fn spawn_pane(&mut self, pane_id: PaneId, tab_id: u64) -> io::Result<()> {
        let shell = self
            .config
            .general
            .shell
            .as_ref()
            .map(|path| OsString::from(path.as_os_str()))
            .or_else(default_shell)
            .unwrap_or_else(fallback_shell);
        let cwd = self
            .config
            .general
            .default_cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(fallback_cwd);
        let term = if terminfo_installed() {
            "vvmux"
        } else {
            "xterm-256color"
        };
        let environment = vec![
            ("TERM".into(), term.into()),
            ("TERM_PROGRAM".into(), "vvmux".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("VVMUX_SESSION".into(), self.name.clone()),
            ("VVMUX_TAB_ID".into(), tab_id.to_string()),
            ("VVMUX_PANE_ID".into(), pane_id.to_string()),
            ("VIVID_ENDPOINT".into(), self.vivid.endpoint()),
            (
                "VIVID_TOKEN".into(),
                self.vivid.issue_pane_capability(pane_id)?,
            ),
        ];
        #[cfg(windows)]
        let environment = {
            let mut environment = environment;
            environment.push(("VIVID_ANCHOR_TRANSPORT".into(), "conpty".into()));
            environment
        };
        let parts = match PtyProcess::spawn(&shell, &cwd, 80, 22, &environment) {
            Ok(parts) => parts,
            Err(error) => {
                self.vivid.revoke_pane(pane_id);
                return Err(error);
            }
        };
        let reader_sender = self.sender.clone();
        let mut reader = parts.reader;
        std::thread::Builder::new()
            .name(format!("vvmux-pty-{pane_id}"))
            .spawn(move || {
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if reader_sender
                                .send(ActorEvent::PtyOutput(pane_id, buffer[..read].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })?;
        let exit_sender = self.sender.clone();
        let waiter = parts.waiter;
        std::thread::Builder::new()
            .name(format!("vvmux-wait-{pane_id}"))
            .spawn(move || {
                let status = waiter.wait().ok();
                let _ = exit_sender.send(ActorEvent::PtyExit(pane_id, status));
            })?;
        self.panes.insert(
            pane_id,
            Pane {
                id: pane_id,
                terminal: Terminal::new(22, 80, self.config.general.scrollback_lines),
                input: parts.input,
                control: parts.control,
                copy: None,
                vivid_metrics: None,
                last_input_warning: None,
                screen_sequence: 1,
                last_screen_change: Instant::now(),
                screen_changes: VecDeque::new(),
            },
        );
        Ok(())
    }

    fn close_pane(&mut self, pane_id: PaneId) {
        if let Some(drag) = &self.pointer_drag {
            self.cancel_pointer_drag(drag.pane() != Some(pane_id));
        }
        if let Some(modal) = self.float_modal {
            // A closing edited pane discards the mode; any other close still invalidates it
            // but restores the entry rectangle.
            self.end_float_mode(modal.pane != pane_id);
        }
        if let Some(pane) = self.panes.remove(&pane_id) {
            pane.control.terminate();
        }
        self.vivid.revoke_pane(pane_id);
        let Some(tab_index) = self.tabs.iter().position(|tab| tab.contains(pane_id)) else {
            return;
        };
        let tab = &mut self.tabs[tab_index];
        if !tab.floating.remove(pane_id)
            && let Some(tree) = tab.tree.take()
        {
            tab.tree = tree.close(pane_id);
        }
        tab.zoomed = tab.zoomed.filter(|pane| *pane != pane_id);
        tab.last_focused_tiled = tab.last_focused_tiled.filter(|pane| *pane != pane_id);
        if tab.is_empty() {
            // A tab lives while either class has panes; it closes with its last pane.
            self.tabs.remove(tab_index);
        } else if tab.focused == pane_id
            && let Some(next) = tab.fallback_focus()
        {
            tab.set_focus(next);
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        self.force_full = true;
        self.relayout();
    }

    fn relayout(&mut self) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.resize_all();
        self.schedule_render();
    }

    /// Visibility, z-order, pin, and focus changes must refresh the media projection even when
    /// no rectangle changed: occlusion and quota priority depend on them. No PTY resizing is
    /// needed on this path.
    fn projection_changed(&mut self) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.schedule_render();
    }

    fn resize_all(&mut self) {
        let area = self.content_area();
        let display = self
            .attached
            .as_ref()
            .map(|client| client.display)
            .unwrap_or_default();
        let mut resize_failures = Vec::new();
        let mut resized_panes = 0_u64;
        for tab in &self.tabs {
            // Hidden ordinary floats keep consuming PTY output but are not resized while
            // hidden; a re-shown float is resized here on the next relayout if its content
            // dimensions changed.
            for projection in visible_projections(tab, area) {
                if let Some(pane) = self.panes.get_mut(&projection.pane_id) {
                    let content = projection.content;
                    if pane.terminal.rows() != content.height as usize
                        || pane.terminal.cols() != content.width as usize
                    {
                        pane.terminal
                            .resize(content.height as usize, content.width as usize);
                        pane.screen_sequence = pane.screen_sequence.wrapping_add(1);
                        pane.last_screen_change = Instant::now();
                        pane.screen_changes.push_back(ScreenChange {
                            sequence: pane.screen_sequence,
                            rows: None,
                        });
                        while pane.screen_changes.len() > SCREEN_CHANGE_HISTORY {
                            pane.screen_changes.pop_front();
                        }
                        resized_panes = resized_panes.wrapping_add(1);
                        if pane.control.resize(content.width, content.height).is_err() {
                            resize_failures.push(projection.pane_id);
                        }
                    }
                    let metrics = (
                        content.width,
                        content.height,
                        display.cell_width,
                        display.cell_height,
                    );
                    if pane.vivid_metrics != Some(metrics) {
                        self.vivid.update_metrics(
                            projection.pane_id,
                            content.width,
                            content.height,
                            (display.cell_width, display.cell_height),
                        );
                        pane.vivid_metrics = Some(metrics);
                    }
                }
            }
        }
        self.session_sequence = self.session_sequence.wrapping_add(resized_panes);
        resize_failures.sort_unstable();
        resize_failures.dedup();
        for pane in resize_failures {
            self.status(&format!("pane {pane} PTY resize failed"));
            self.close_pane(pane);
        }
    }

    fn render(&mut self) {
        // Frames the client has queued but not yet displayed. Producing more of them cannot make
        // the terminal any more current, and the extra bytes compete with media on the same
        // connection, so hold off and keep the render pending.
        if let Some(client) = &self.attached
            && self.frame_id.saturating_sub(client.acknowledged_frame) >= MAX_UNACKNOWLEDGED_FRAMES
        {
            self.pending_render = true;
            return;
        }
        self.pending_render = false;
        let Some(client) = &self.attached else {
            return;
        };
        #[cfg(windows)]
        let focused_bracketed_paste = self
            .active_tab()
            .and_then(|tab| self.panes.get(&tab.focused))
            .is_some_and(|pane| pane.terminal.modes().bracketed_paste);
        let mut screen = ScreenBuffer::new(client.display.columns, client.display.rows);
        let area = self.content_area();
        if let Some(tab) = self.active_tab() {
            // Composition follows the ordered projection list; later projections overwrite
            // earlier frames and cells, and the status row is drawn last.
            let projections = visible_projections(tab, area);
            let mut cursor: Option<((u16, u16), usize)> = None;
            for (index, projection) in projections.iter().enumerate() {
                let Some(pane) = self.panes.get(&projection.pane_id) else {
                    continue;
                };
                let active = projection.focused;
                let color = if active {
                    self.config.appearance.active_frame
                } else {
                    self.config.appearance.inactive_frame
                };
                let title = pane
                    .terminal
                    .title()
                    .map_or_else(|| format!("pane {}", pane.id), ToOwned::to_owned);
                let copy_suffix = pane.copy.as_ref().map(|_| " [copy]").unwrap_or("");
                let pin_suffix = if projection.layer == PaneLayer::Pinned {
                    " [pin]"
                } else {
                    ""
                };
                screen.draw_frame(
                    projection.outer,
                    &format!(" {title}{copy_suffix}{pin_suffix} "),
                    color,
                );
                let content = projection.content;
                let offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
                screen.draw_terminal(content, &pane.terminal, offset);
                if active {
                    if let Some(copy) = &pane.copy {
                        cursor = Some((
                            (
                                content.x
                                    + copy.column.min(content.width.saturating_sub(1) as usize)
                                        as u16,
                                content.y
                                    + copy.row.min(content.height.saturating_sub(1) as usize)
                                        as u16,
                            ),
                            index,
                        ));
                    } else if pane.terminal.modes().cursor_visible {
                        let (row, column) = pane.terminal.cursor();
                        cursor = Some((
                            (
                                content.x
                                    + column.min(content.width.saturating_sub(1) as usize) as u16,
                                content.y
                                    + row.min(content.height.saturating_sub(1) as usize) as u16,
                            ),
                            index,
                        ));
                    }
                }
            }
            // The cursor comes only from the focused projection and hides when a later
            // projection covers its cell.
            screen.cursor = cursor.and_then(|((x, y), index)| {
                projections[index + 1..]
                    .iter()
                    .all(|later| !later.outer.contains(x, y))
                    .then_some((x, y))
            });
        }
        if self.config.general.status_visible && screen.rows > 0 {
            let tab_number = self.active_tab + 1;
            let tab_id = self.active_tab().map_or(0, |tab| tab.id);
            let status = format!(
                " vvmux:{}  tab {}/{} (id:{})  panes:{}  rev:{} ",
                self.name,
                tab_number,
                self.tabs.len(),
                tab_id,
                self.panes.len(),
                self.layout_revision
            );
            screen.draw_text(
                0,
                screen.rows - 1,
                &status,
                self.config.appearance.status_foreground,
                self.config.appearance.status_background,
            );
        }
        self.frame_id = self.frame_id.wrapping_add(1);
        // Mutated only by the Windows bracketed-paste prepend below.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut bytes = ansi_diff(self.last_screen.as_ref(), &screen, self.force_full);
        #[cfg(windows)]
        let bracketed_paste_transition =
            bracketed_paste_transition(self.outer_bracketed_paste, focused_bracketed_paste);
        #[cfg(windows)]
        if let Some(transition) = bracketed_paste_transition {
            prepend_bracketed_paste_transition(&mut bytes, transition);
        }
        let sent = crate::ipc::send_render_record(
            &client.writer,
            self.frame_id,
            self.session_sequence,
            self.force_full,
            &bytes,
        )
        .is_ok();
        if !sent {
            // Dropping the client here is the only signal it gets, so say why. Previously a
            // failed frame silently detached the session while the client kept running against a
            // frozen outer scene, which is indistinguishable from a hang.
            let _ = crate::ipc::send(
                &client.writer,
                &ServerMessage::Detached {
                    reason: "frame delivery failed".into(),
                },
            );
            self.attached = None;
            self.last_screen = None;
            #[cfg(windows)]
            {
                self.outer_bracketed_paste = None;
            }
        } else {
            self.last_screen = Some(screen);
            if let Some(client) = &mut self.attached {
                client
                    .frame_sequences
                    .push_back((self.frame_id, self.session_sequence));
                while client.frame_sequences.len() > 1024 {
                    client.frame_sequences.pop_front();
                }
            }
            #[cfg(windows)]
            if bracketed_paste_transition.is_some() {
                self.outer_bracketed_paste = Some(focused_bracketed_paste);
            }
        }
        self.force_full = false;
        self.sync_media(false);
    }

    fn sync_media(&mut self, force: bool) {
        self.sync_media_inner(force, None);
    }

    fn sync_media_before_delivery(&mut self, source: crate::media::SourceKey) {
        self.sync_media_inner(false, Some(source));
    }

    fn sync_media_inner(
        &mut self,
        force: bool,
        live_delivery_source: Option<crate::media::SourceKey>,
    ) {
        let Some(client) = &self.attached else {
            self.vivid.deactivate_bridge();
            return;
        };
        if !client.vivid {
            self.vivid.deactivate_bridge();
            return;
        }
        let writer = client.writer.clone();
        let Some((projections, pane_priority)) = self.active_tab().map(|tab| {
            let projections = visible_projections(tab, self.content_area());
            let priority = projection_pane_priority(tab, &projections);
            (projections, priority)
        }) else {
            return;
        };
        let area = self.content_area();
        let panes = projections
            .iter()
            .map(|projection| projection.pane_id)
            .collect::<HashSet<_>>();
        let viewport_offsets = panes
            .iter()
            .filter_map(|pane_id| {
                let offset = self
                    .panes
                    .get(pane_id)
                    .and_then(|pane| pane.copy.as_ref())
                    .map_or(0, |copy| copy.offset);
                (offset != 0).then_some((*pane_id, offset))
            })
            .collect::<HashMap<_, _>>();
        let projection_key = MediaProjectionKey {
            virtual_revision: self.vivid.revision(),
            layout_revision: self.layout_revision,
        };
        if !should_sync_media(force, self.last_media_projection, projection_key) {
            return;
        }
        // projection_snapshot marks active video sources as requiring a fresh keyframe. Only call
        // it after deciding that the client will actually rebuild its outer Vivid session.
        let mut snapshot = self
            .vivid
            .projection_snapshot_with_viewports(&panes, &viewport_offsets);
        let projection_key = MediaProjectionKey {
            virtual_revision: snapshot.revision,
            layout_revision: self.layout_revision,
        };
        let live_nodes = snapshot.live_nodes.iter().copied().collect::<HashSet<_>>();
        self.fragment_assignments
            .retain(|logical, _| live_nodes.contains(logical));
        let sources = snapshot
            .sources
            .iter()
            .map(|source| BridgeSource {
                key: bridge_key(source.key),
                kind: bridge_source_kind(
                    source.key,
                    &source.descriptor,
                    source.raster_delta_operation_limit,
                ),
                capture_policy: source.capture_policy,
                descriptor: source.semantic_descriptor.as_ref().map(|descriptor| {
                    BridgeSourceDescriptor {
                        role: descriptor.role,
                        title: descriptor.title.clone(),
                        content_revision: descriptor.content_revision,
                        semantic_availability: descriptor.semantic_availability,
                        locator: descriptor.locator.clone(),
                    }
                }),
                playing: source.playing,
                play_request: bridge_play_request(source.play_request),
                eos_epoch: source.eos_epoch,
                causation_id: source.causation_id,
            })
            .collect::<Vec<_>>();
        let pane_rank = pane_priority
            .iter()
            .enumerate()
            .map(|(rank, pane)| (*pane, rank))
            .collect::<HashMap<_, _>>();
        snapshot.nodes.sort_by(|left, right| {
            pane_rank
                .get(&left.pane)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&pane_rank.get(&right.pane).copied().unwrap_or(usize::MAX))
                .then_with(|| right.config.node.z_index.cmp(&left.config.node.z_index))
                .then_with(|| left.producer.cmp(&right.producer))
                .then_with(|| left.config.node.node_id.cmp(&right.config.node.node_id))
        });
        let mut nodes = Vec::new();
        let mut fragment_omissions = 0_usize;
        let mut arithmetic_omissions = 0_usize;
        let mut quota_omissions = 0_usize;
        for (logical_index, logical) in snapshot.nodes.iter().enumerate() {
            let Some(projection_index) = projections
                .iter()
                .position(|projection| projection.pane_id == logical.pane)
            else {
                continue;
            };
            let projection = projections[projection_index];
            let occluders = projections[projection_index + 1..]
                .iter()
                .filter_map(|higher| from_cells(higher.outer))
                .collect::<Vec<_>>();
            let logical_key = (logical.producer, logical.config.node.node_id);
            let projected =
                match project_logical_node(logical, projection.content, area, &occluders) {
                    Ok(projected) => projected,
                    Err(ProjectionIssue::FragmentLimit) => {
                        fragment_omissions += 1;
                        let _ = self
                            .fragment_assignments
                            .entry(logical_key)
                            .or_default()
                            .assign(&[]);
                        continue;
                    }
                    Err(ProjectionIssue::Arithmetic) => {
                        arithmetic_omissions += 1;
                        let _ = self
                            .fragment_assignments
                            .entry(logical_key)
                            .or_default()
                            .assign(&[]);
                        continue;
                    }
                };
            let fragment_rects = projected
                .fragments
                .iter()
                .map(|fragment| fragment.clip)
                .collect::<Vec<_>>();
            let Some(assignments) = self
                .fragment_assignments
                .entry(logical_key)
                .or_default()
                .assign(&fragment_rects)
            else {
                arithmetic_omissions += 1;
                continue;
            };
            if nodes.len().saturating_add(assignments.len()) > MAX_PROJECTED_NODES {
                quota_omissions = snapshot.nodes.len() - logical_index;
                break;
            }
            for ((fragment_id, clip), mut bridge) in assignments.into_iter().zip(
                projected
                    .fragments
                    .into_iter()
                    .map(|fragment| fragment.node),
            ) {
                bridge.fragment = fragment_id;
                bridge.clip = BridgeClipRect {
                    x: clip.x,
                    y: clip.y,
                    width: clip.width,
                    height: clip.height,
                };
                nodes.push(bridge);
            }
        }
        if (fragment_omissions != 0 || arithmetic_omissions != 0 || quota_omissions != 0)
            && self.last_projection_warning != Some(projection_key)
        {
            self.status(&format!(
                "media projection omitted nodes (fragment-limit:{fragment_omissions}, arithmetic:{arithmetic_omissions}, quota:{quota_omissions})"
            ));
            self.last_projection_warning = Some(projection_key);
        }
        let videos_needing_keyframes = snapshot
            .videos_needing_keyframes
            .iter()
            .copied()
            .map(bridge_key)
            .collect();
        let projection_revision = self.media_projection_revision.wrapping_add(1);
        if crate::ipc::send(
            &writer,
            &ServerMessage::MediaSnapshot {
                revision: projection_revision,
                sources,
                nodes,
                videos_needing_keyframes,
            },
        )
        .is_err()
        {
            return;
        }
        for source in snapshot.sources {
            if !should_replay_retained(source.key, live_delivery_source) {
                // The MediaEvent that triggered this projection sync follows immediately. Do not
                // also send the same retained raster body as delivery 0: the outer source would
                // observe the same frame ID twice and reject the live update.
                continue;
            }
            if let Some(body) = source.retained {
                let record_type = match source.descriptor {
                    crate::media::SourceDescriptor::Raster(_) => {
                        vivid_protocol::messages::RASTER_FRAME
                    }
                    crate::media::SourceDescriptor::Image(_) => {
                        vivid_protocol::messages::IMAGE_DATA
                    }
                    _ => continue,
                };
                if !send_media_body(&writer, 0, bridge_key(source.key), record_type, &body) {
                    return;
                }
            }
        }
        self.last_media_projection = Some(projection_key);
        self.media_projection_revision = projection_revision;
    }

    fn schedule_render(&mut self) {
        self.pending_render = true;
    }

    /// Session-scoped relay counters for the media diagnostic surfaces.
    ///
    /// `delivery` is filled in by the virtual presenter, which owns those counters.
    fn relay_metrics(&self) -> crate::metrics::RelayMetrics {
        crate::metrics::RelayMetrics {
            ipc: self
                .client_ipc
                .as_ref()
                .map(|counters| counters.snapshot())
                .unwrap_or_default(),
            delivery: crate::metrics::DeliveryMetrics::default(),
            bridge: self.bridge_metrics,
        }
    }

    fn content_area(&self) -> Rect {
        let display = self
            .attached
            .as_ref()
            .map_or(self.last_display, |client| client.display);
        Rect {
            x: 0,
            y: 0,
            width: display.columns.max(1),
            height: display
                .rows
                .saturating_sub(u16::from(self.config.general.status_visible))
                .max(1),
        }
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    fn status(&self, message: &str) {
        if let Some(client) = &self.attached {
            let _ = crate::ipc::send(&client.writer, &ServerMessage::Status(message.into()));
        }
    }

    fn send_pane_input(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let failure = self
            .panes
            .get_mut(&pane_id)
            .and_then(|pane| queue_pane_input(pane, bytes));
        self.report_input_failure(pane_id, failure);
    }

    fn report_input_failure(&mut self, pane_id: PaneId, failure: Option<InputFailure>) {
        let Some(failure) = failure else {
            return;
        };
        if failure.warn {
            self.status(&format!("pane {pane_id} input queue is unavailable"));
        }
        if failure.close {
            self.close_pane(pane_id);
        }
    }

    fn copy_input(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let remapped = copy_chord_name(bytes)
            .and_then(|chord| self.config.keys.copy.get(chord))
            .and_then(|action| copy_action_bytes(action));
        let bytes = remapped.as_deref().unwrap_or(bytes);
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let previous = pane.copy.clone();
        let Some(copy) = &mut pane.copy else {
            return;
        };
        let rows = pane.terminal.rows();
        let columns = pane.terminal.cols();
        match bytes {
            b"q" | b"\x1b" => pane.copy = None,
            b"\x1b[A" => {
                if copy.row == 0 {
                    copy.offset = (copy.offset + 1).min(pane.terminal.history_len());
                } else {
                    copy.row -= 1;
                }
            }
            b"\x1b[B" => {
                if copy.row + 1 >= rows && copy.offset > 0 {
                    copy.offset -= 1;
                } else {
                    copy.row = (copy.row + 1).min(rows.saturating_sub(1));
                }
            }
            b"\x1b[C" => copy.column = (copy.column + 1).min(columns.saturating_sub(1)),
            b"\x1b[D" => copy.column = copy.column.saturating_sub(1),
            b"\x1b[5~" => {
                copy.offset = (copy.offset + rows).min(pane.terminal.history_len());
            }
            b"\x1b[6~" => copy.offset = copy.offset.saturating_sub(rows),
            b" " => {
                copy.selection_start =
                    Some((copy.row as isize - copy.offset as isize, copy.column));
            }
            b"\r" | b"\n" => {
                let end = (copy.row as isize - copy.offset as isize, copy.column);
                let start = copy.selection_start.unwrap_or((-(copy.offset as isize), 0));
                self.copy_buffer = extract_selection(&pane.terminal, start, end);
                self.copy_buffer.truncate(COPY_BUFFER_LIMIT);
                let clipboard = String::from_utf8_lossy(&self.copy_buffer).into_owned();
                pane.copy = None;
                if let Some(client) = &self.attached {
                    let _ = crate::ipc::send(&client.writer, &ServerMessage::Clipboard(clipboard));
                }
            }
            _ => {}
        }
        let changed = previous != pane.copy;
        let media_view_changed = previous.as_ref().map_or(0, |copy| copy.offset)
            != pane.copy.as_ref().map_or(0, |copy| copy.offset);
        if changed {
            self.mark_pane_screen_change(pane_id, None);
        }
        if media_view_changed {
            self.projection_changed();
        } else {
            self.schedule_render();
        }
    }

    fn paste(&mut self) {
        let Some(pane_id) = self.active_tab().map(|tab| tab.focused) else {
            return;
        };
        let bracketed = self
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.terminal.modes().bracketed_paste);
        let bytes = if bracketed {
            let sanitized = sanitize_bracketed_paste(&self.copy_buffer);
            let mut bytes = Vec::with_capacity(sanitized.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(&sanitized);
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            self.copy_buffer.clone()
        };
        self.send_pane_input(pane_id, &bytes);
    }

    fn terminate_children(&mut self) {
        let controls = std::mem::take(&mut self.panes)
            .into_values()
            .map(|pane| pane.control)
            .collect::<Vec<_>>();
        let workers = controls
            .into_iter()
            .filter_map(|control| {
                std::thread::Builder::new()
                    .name("vvmux-pane-shutdown".into())
                    .spawn(move || control.terminate_blocking())
                    .ok()
            })
            .collect::<Vec<_>>();
        for worker in workers {
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StyleKey {
    foreground: TerminalColor,
    background: TerminalColor,
    underline_color: Option<TerminalColor>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: UnderlineStyle,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikeout: bool,
    hyperlink: Option<TerminalHyperlink>,
}

impl From<&Cell> for StyleKey {
    fn from(cell: &Cell) -> Self {
        Self {
            foreground: cell.foreground,
            background: cell.background,
            underline_color: cell.underline_color,
            bold: cell.bold,
            dim: cell.dim,
            italic: cell.italic,
            underline: cell.underline_style,
            blink: cell.blink,
            inverse: cell.inverse,
            hidden: cell.hidden,
            strikeout: cell.strikeout,
            hyperlink: cell.hyperlink.clone(),
        }
    }
}

fn method_needs_pane(method: &AutomationMethod) -> bool {
    !matches!(
        method,
        AutomationMethod::Capabilities
            | AutomationMethod::ListPanes
            | AutomationMethod::WaitRendered { .. }
    )
}

fn validate_automation_method(method: &AutomationMethod) -> Result<(), AutomationError> {
    let input = match method {
        AutomationMethod::Typing { text } | AutomationMethod::Paste { text } => Some(text.len()),
        _ => None,
    };
    if input.is_some_and(|length| length > 1024 * 1024) {
        return Err(AutomationError::new(
            "limit_exceeded",
            "input exceeds 1 MiB",
        ));
    }
    match method {
        AutomationMethod::Key { repeat, .. } if !(1..=1000).contains(repeat) => Err(
            AutomationError::new("invalid_params", "key repeat must be from 1 through 1000"),
        ),
        AutomationMethod::GetText { rows: Some(rows) } if !(1..=1000).contains(rows) => Err(
            AutomationError::new("invalid_params", "rows must be from 1 through 1000"),
        ),
        AutomationMethod::GetGrid {
            start_line,
            row_count,
            since_screen,
        } => {
            if start_line.is_some() != row_count.is_some() {
                return Err(AutomationError::new(
                    "invalid_params",
                    "start_line and row_count must be supplied together",
                ));
            }
            if since_screen.is_some() && start_line.is_some() {
                return Err(AutomationError::new(
                    "invalid_params",
                    "since_screen conflicts with an explicit row range",
                ));
            }
            if row_count.is_some_and(|rows| !(1..=1000).contains(&rows)) {
                return Err(AutomationError::new(
                    "invalid_params",
                    "row_count must be from 1 through 1000",
                ));
            }
            Ok(())
        }
        AutomationMethod::WaitText { text, regex, .. } if *regex && text.len() > 8 * 1024 => Err(
            AutomationError::new("limit_exceeded", "regular expression exceeds 8 KiB"),
        ),
        AutomationMethod::WaitScreenStable { quiet_ms, .. }
            if !(1..=24 * 60 * 60 * 1000).contains(quiet_ms) =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "quiet duration must be from 1ms through 24h",
            ))
        }
        method => {
            let timeout = match method {
                AutomationMethod::WaitText { timeout_ms, .. }
                | AutomationMethod::WaitScreenChange { timeout_ms, .. }
                | AutomationMethod::WaitScreenStable { timeout_ms, .. }
                | AutomationMethod::WaitRendered { timeout_ms, .. }
                | AutomationMethod::WaitExit { timeout_ms }
                | AutomationMethod::WaitMedia { timeout_ms, .. } => Some(*timeout_ms),
                _ => None,
            };
            if timeout.is_some_and(|timeout| !(1..=24 * 60 * 60 * 1000).contains(&timeout)) {
                Err(AutomationError::new(
                    "invalid_params",
                    "timeout must be from 1ms through 24h",
                ))
            } else {
                Ok(())
            }
        }
    }
}

fn automation_capabilities() -> serde_json::Value {
    serde_json::json!({
        "protocol": "VVMX",
        "protocol_version": crate::ipc::VERSION,
        "methods": [
            "capabilities", "list_panes", "inspect", "inspect_media", "split", "focus", "close_pane",
            "typing", "key", "paste", "get_text", "get_grid", "wait_text",
            "wait_screen_change", "wait_screen_stable", "wait_rendered", "wait_exit", "wait_media"
        ],
        "limits": automation_limits(),
        "render_acknowledgment": "attached_client_write",
    })
}

fn automation_limits() -> serde_json::Value {
    serde_json::json!({
        "request_bytes": 1024 * 1024,
        "reply_bytes": 16 * 1024 * 1024,
        "rows": 1000,
        "key_repeats": 1000,
        "regex_bytes": 8 * 1024,
        "timeout_ms": { "minimum": 1, "maximum": 24 * 60 * 60 * 1000_u64 },
        "pty_write_timeout_ms": 5000,
    })
}

fn deadline(timeout_ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(timeout_ms.clamp(1, 24 * 60 * 60 * 1000))
}

fn rect_json(rect: Rect) -> serde_json::Value {
    serde_json::json!({
        "x": rect.x,
        "y": rect.y,
        "width": rect.width,
        "height": rect.height,
    })
}

fn terminal_mode_names(modes: TerminalModes) -> Vec<&'static str> {
    let mut names = Vec::new();
    if modes.application_cursor {
        names.push("application_cursor");
    }
    if modes.application_keypad {
        names.push("application_keypad");
    }
    if modes.bracketed_paste {
        names.push("bracketed_paste");
    }
    if modes.mouse_clicks {
        names.push("mouse_clicks");
    }
    if modes.mouse_motion {
        names.push("mouse_motion");
    }
    if modes.sgr_mouse {
        names.push("sgr_mouse");
    }
    if modes.focus_reporting {
        names.push("focus_reporting");
    }
    if modes.cursor_visible {
        names.push("cursor_visible");
    }
    names
}

fn style_json(style: &StyleKey) -> serde_json::Value {
    let mut attributes = Vec::new();
    if style.bold {
        attributes.push("bold");
    }
    if style.dim {
        attributes.push("dim");
    }
    if style.italic {
        attributes.push("italic");
    }
    match style.underline {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => attributes.push("underline"),
        UnderlineStyle::Double => attributes.push("double_underline"),
        UnderlineStyle::Curl => attributes.push("undercurl"),
        UnderlineStyle::Dotted => attributes.push("dotted_underline"),
        UnderlineStyle::Dashed => attributes.push("dashed_underline"),
    }
    if style.blink {
        attributes.push("blink");
    }
    if style.inverse {
        attributes.push("inverse");
    }
    if style.hidden {
        attributes.push("hidden");
    }
    if style.strikeout {
        attributes.push("strikeout");
    }
    serde_json::json!({
        "foreground": color_json(style.foreground),
        "background": color_json(style.background),
        "underline_color": style.underline_color.map(color_json),
        "attributes": attributes,
        "hyperlink": style.hyperlink.as_ref().map(|link| serde_json::json!({
            "id": link.id,
            "uri": link.uri,
        })),
    })
}

fn color_json(color: TerminalColor) -> serde_json::Value {
    match color {
        TerminalColor::Default => serde_json::json!({ "kind": "default" }),
        TerminalColor::Indexed(index) => {
            serde_json::json!({ "kind": "indexed", "index": index })
        }
        TerminalColor::Rgb(red, green, blue) => serde_json::json!({
            "kind": "rgb",
            "red": red,
            "green": green,
            "blue": blue,
        }),
    }
}

fn exit_result(pane_id: PaneId, status: Option<PtyExitStatus>) -> serde_json::Value {
    serde_json::json!({
        "pane_id": pane_id,
        "code": status.and_then(|status| status.code),
        "signal": status.and_then(|status| status.signal),
        "success": status.is_some_and(|status| status.success),
        "status_available": status.is_some(),
    })
}

fn changed_rows(previous: &[Vec<Cell>], current: &[Vec<Cell>]) -> Vec<usize> {
    let length = previous.len().max(current.len());
    (0..length)
        .filter(|row| previous.get(*row) != current.get(*row))
        .collect()
}

fn encode_automation_key(
    key: &str,
    modifiers: &[String],
    modes: TerminalModes,
) -> Result<Vec<u8>, AutomationError> {
    let mut bits = 0_u8;
    for modifier in modifiers {
        match modifier.to_ascii_lowercase().as_str() {
            "shift" => bits |= 1,
            "alt" | "option" => bits |= 2,
            "ctrl" | "control" => bits |= 4,
            "super" | "command" | "cmd" => bits |= 8,
            _ => {
                return Err(AutomationError::new(
                    "invalid_params",
                    format!("unknown modifier {modifier:?}"),
                ));
            }
        }
    }
    let modifier_parameter = bits + 1;
    let mut characters = key.chars();
    if let (Some(mut character), None) = (characters.next(), characters.next()) {
        if bits & 4 != 0 {
            character = character.to_ascii_lowercase();
            let control = match character {
                '@' | ' ' => Some(0),
                'a'..='z' => Some(character as u8 - b'a' + 1),
                '[' => Some(27),
                '\\' => Some(28),
                ']' => Some(29),
                '^' => Some(30),
                '_' | '?' => Some(31),
                _ => None,
            };
            if let Some(control) = control {
                let mut bytes = Vec::with_capacity(2);
                if bits & 2 != 0 {
                    bytes.push(0x1b);
                }
                bytes.push(control);
                return Ok(bytes);
            }
        }
        if bits & 8 != 0 {
            return Ok(format!("\x1b[{};{modifier_parameter}u", u32::from(character)).into_bytes());
        }
        let mut bytes = Vec::new();
        if bits & 2 != 0 {
            bytes.push(0x1b);
        }
        let mut encoded = [0; 4];
        bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        return Ok(bytes);
    }

    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    if let Some(byte) = match normalized.as_str() {
        "enter" | "return" => Some(b'\r'),
        "escape" | "esc" => Some(0x1b),
        "tab" => Some(b'\t'),
        "backspace" => Some(0x7f),
        _ => None,
    } {
        if normalized == "tab" && bits & 1 != 0 {
            let mut bytes = if bits & 2 != 0 {
                vec![0x1b]
            } else {
                Vec::new()
            };
            bytes.extend_from_slice(b"\x1b[Z");
            return Ok(bytes);
        }
        let mut bytes = if bits & 2 != 0 {
            vec![0x1b]
        } else {
            Vec::new()
        };
        bytes.push(byte);
        return Ok(bytes);
    }
    if let Some(final_byte) = match normalized.as_str() {
        "arrowup" | "up" => Some('A'),
        "arrowdown" | "down" => Some('B'),
        "arrowright" | "right" => Some('C'),
        "arrowleft" | "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    } {
        return if bits == 0 {
            Ok(format!(
                "{}{}",
                if modes.application_cursor {
                    "\x1bO"
                } else {
                    "\x1b["
                },
                final_byte
            )
            .into_bytes())
        } else {
            Ok(format!("\x1b[1;{modifier_parameter}{final_byte}").into_bytes())
        };
    }
    if let Some(code) = match normalized.as_str() {
        "insert" => Some(2),
        "delete" | "del" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        _ => None,
    } {
        return Ok(if bits == 0 {
            format!("\x1b[{code}~")
        } else {
            format!("\x1b[{code};{modifier_parameter}~")
        }
        .into_bytes());
    }
    if let Some(number) = normalized
        .strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .filter(|number| (1..=35).contains(number))
    {
        if number <= 4 {
            let final_byte = char::from(b'P' + number - 1);
            return Ok(if bits == 0 {
                format!("\x1bO{final_byte}")
            } else {
                format!("\x1b[1;{modifier_parameter}{final_byte}")
            }
            .into_bytes());
        }
        let codes = [
            15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
        ];
        let code = if number <= 20 {
            codes[usize::from(number - 5)]
        } else {
            42 + u32::from(number - 21)
        };
        return Ok(if bits == 0 {
            format!("\x1b[{code}~")
        } else {
            format!("\x1b[{code};{modifier_parameter}~")
        }
        .into_bytes());
    }
    let keypad = match normalized.as_str() {
        "keypad0" => Some((b'0', 'p')),
        "keypad1" => Some((b'1', 'q')),
        "keypad2" => Some((b'2', 'r')),
        "keypad3" => Some((b'3', 's')),
        "keypad4" => Some((b'4', 't')),
        "keypad5" => Some((b'5', 'u')),
        "keypad6" => Some((b'6', 'v')),
        "keypad7" => Some((b'7', 'w')),
        "keypad8" => Some((b'8', 'x')),
        "keypad9" => Some((b'9', 'y')),
        "keypaddecimal" => Some((b'.', 'n')),
        "keypaddivide" => Some((b'/', 'o')),
        "keypadmultiply" => Some((b'*', 'j')),
        "keypadsubtract" => Some((b'-', 'm')),
        "keypadadd" => Some((b'+', 'k')),
        "keypadenter" => Some((b'\r', 'M')),
        "keypadequal" => Some((b'=', 'X')),
        _ => None,
    };
    if let Some((literal, application)) = keypad {
        if modes.application_keypad {
            return Ok(if bits == 0 {
                format!("\x1bO{application}")
            } else {
                format!("\x1b[1;{modifier_parameter}{application}")
            }
            .into_bytes());
        }
        let mut bytes = if bits & 2 != 0 {
            vec![0x1b]
        } else {
            Vec::new()
        };
        bytes.push(literal);
        return Ok(bytes);
    }
    Err(AutomationError::new(
        "invalid_params",
        format!("unknown key {key:?}"),
    ))
}

fn should_replay_retained(
    source: crate::media::SourceKey,
    live_delivery_source: Option<crate::media::SourceKey>,
) -> bool {
    Some(source) != live_delivery_source
}

#[cfg(unix)]
fn default_shell() -> Option<OsString> {
    std::env::var_os("SHELL")
}

#[cfg(windows)]
fn default_shell() -> Option<OsString> {
    std::env::var_os("COMSPEC")
}

#[cfg(unix)]
fn fallback_shell() -> OsString {
    OsString::from("/bin/sh")
}

#[cfg(windows)]
fn fallback_shell() -> OsString {
    crate::platform::windows_fallback_shell()
}

#[cfg(unix)]
fn fallback_cwd() -> PathBuf {
    PathBuf::from("/")
}

#[cfg(windows)]
fn fallback_cwd() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\"))
}

fn bridge_play_request(request: vivid_protocol::messages::PlayRequest) -> BridgePlayRequest {
    BridgePlayRequest {
        start_pts_us: request.start_pts_us,
        minimum_buffer_us: request.minimum_buffer_us,
        maximum_latency_us: request.maximum_latency_us,
        rate_32_32: request.rate_32_32,
        late_policy: request.late_policy,
        loop_count: request.loop_count,
        start_policy: request.start_policy,
    }
}

fn copy_chord_name(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        b"\x1b[A" => Some("Up"),
        b"\x1b[B" => Some("Down"),
        b"\x1b[C" => Some("Right"),
        b"\x1b[D" => Some("Left"),
        b"\x1b[5~" => Some("PageUp"),
        b"\x1b[6~" => Some("PageDown"),
        b" " => Some("Space"),
        b"\r" | b"\n" => Some("Enter"),
        b"q" => Some("q"),
        b"\x1b" => Some("Escape"),
        _ => None,
    }
}

fn copy_action_bytes(action: &str) -> Option<Vec<u8>> {
    Some(
        match action {
            "up" => b"\x1b[A".as_slice(),
            "down" => b"\x1b[B".as_slice(),
            "left" => b"\x1b[D".as_slice(),
            "right" => b"\x1b[C".as_slice(),
            "page-up" => b"\x1b[5~".as_slice(),
            "page-down" => b"\x1b[6~".as_slice(),
            "start-selection" => b" ".as_slice(),
            "copy" => b"\r".as_slice(),
            "cancel" => b"q".as_slice(),
            _ => return None,
        }
        .to_vec(),
    )
}

fn bridge_key(key: crate::media::SourceKey) -> BridgeSourceKey {
    BridgeSourceKey {
        producer: key.0,
        source: key.1,
    }
}

fn bridge_source_kind(
    key: crate::media::SourceKey,
    descriptor: &crate::media::SourceDescriptor,
    raster_delta_operation_limit: Option<u32>,
) -> BridgeSourceKind {
    match descriptor {
        crate::media::SourceDescriptor::Raster(config) => BridgeSourceKind::Raster {
            width: config.width,
            height: config.height,
            alpha_mode: config.alpha_mode,
            compression_mode: config.compression_mode,
            delta_operation_limit: raster_delta_operation_limit,
        },
        crate::media::SourceDescriptor::Image(config) => BridgeSourceKind::Image {
            encoding: config.encoding,
            width: config.width,
            height: config.height,
            encoded_length: config.encoded_length,
            sha256: config.sha256,
        },
        crate::media::SourceDescriptor::Video(config) => BridgeSourceKind::Video {
            codec: config.codec.clone(),
            packetization: config.packetization.clone(),
            extradata: config.extradata.clone(),
            width: config.width,
            height: config.height,
            profile: config.profile,
            level: config.level,
            bitrate: config.bitrate,
            color_primaries: config.color_primaries,
            transfer: config.transfer,
            matrix: config.matrix,
            range: config.range,
            sar_num: config.sar_num,
            sar_den: config.sar_den,
            max_access_unit_bytes: config.max_access_unit_bytes,
            codec_string: config.codec_string.clone(),
            decoder_config: config.decoder_config.clone(),
        },
        crate::media::SourceDescriptor::Audio(config) => BridgeSourceKind::Audio {
            linked_video: config.linked_video_source_id.map(|source| BridgeSourceKey {
                producer: key.0,
                source,
            }),
            codec: config.codec.clone(),
            packetization: config.packetization.clone(),
            extradata: config.extradata.clone(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            channel_mask: config.channel_mask,
            bitrate: config.bitrate,
            max_access_unit_bytes: config.max_access_unit_bytes,
            codec_string: config.codec_string.clone(),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionIssue {
    Arithmetic,
    FragmentLimit,
}

#[derive(Debug)]
struct ProjectedFragment {
    clip: FixedRect,
    node: BridgeNode,
}

#[derive(Debug)]
struct ProjectedLogicalNode {
    fragments: Vec<ProjectedFragment>,
}

/// Translate one logical node into grid coordinates, clip it to its own bounds, downstream
/// clip, pane content, and tab area, then subtract every higher pane's opaque outer rectangle.
fn project_logical_node(
    node: &crate::media::SceneNode,
    pane: Rect,
    area: Rect,
    occluders: &[FixedRect],
) -> Result<ProjectedLogicalNode, ProjectionIssue> {
    if !node.config.node.visible {
        return Ok(ProjectedLogicalNode {
            fragments: Vec::new(),
        });
    }
    let offset_x = i64::from(pane.x)
        .checked_mul(crate::region::FIXED_ONE)
        .ok_or(ProjectionIssue::Arithmetic)?;
    let offset_y = i64::from(pane.y)
        .checked_mul(crate::region::FIXED_ONE)
        .ok_or(ProjectionIssue::Arithmetic)?;
    let x = node
        .config
        .node
        .x
        .checked_add(offset_x)
        .ok_or(ProjectionIssue::Arithmetic)?;
    let y = node
        .config
        .node
        .y
        .checked_add(offset_y)
        .ok_or(ProjectionIssue::Arithmetic)?;
    let node_bounds = FixedRect::new(x, y, node.config.node.width, node.config.node.height)
        .ok_or(ProjectionIssue::Arithmetic)?;
    let Some(mut clip) = intersect(
        node_bounds,
        from_cells(pane).ok_or(ProjectionIssue::Arithmetic)?,
    ) else {
        return Ok(ProjectedLogicalNode {
            fragments: Vec::new(),
        });
    };
    let Some(next) = intersect(clip, from_cells(area).ok_or(ProjectionIssue::Arithmetic)?) else {
        return Ok(ProjectedLogicalNode {
            fragments: Vec::new(),
        });
    };
    clip = next;
    if let Some(downstream) = node.config.clip {
        let downstream = FixedRect::new(
            downstream
                .x
                .checked_add(offset_x)
                .ok_or(ProjectionIssue::Arithmetic)?,
            downstream
                .y
                .checked_add(offset_y)
                .ok_or(ProjectionIssue::Arithmetic)?,
            downstream.width,
            downstream.height,
        )
        .ok_or(ProjectionIssue::Arithmetic)?;
        let Some(next) = intersect(clip, downstream) else {
            return Ok(ProjectedLogicalNode {
                fragments: Vec::new(),
            });
        };
        clip = next;
    }
    let fragments =
        subtract_all(clip, occluders, MAX_NODE_FRAGMENTS).ok_or(ProjectionIssue::FragmentLimit)?;
    let base = BridgeNode {
        producer: node.producer,
        node: node.config.node.node_id,
        fragment: 0,
        source: BridgeSourceKey {
            producer: node.producer,
            source: node.config.node.source_id,
        },
        x,
        y,
        width: node.config.node.width,
        height: node.config.node.height,
        z_index: node.config.node.z_index,
        visible: true,
        clip: BridgeClipRect {
            x: clip.x,
            y: clip.y,
            width: clip.width,
            height: clip.height,
        },
    };
    Ok(ProjectedLogicalNode {
        fragments: fragments
            .into_iter()
            .map(|clip| ProjectedFragment {
                clip,
                node: base.clone(),
            })
            .collect(),
    })
}

fn send_media_body(
    writer: &SharedWriter,
    delivery_id: u64,
    source: BridgeSourceKey,
    record_type: u16,
    body: &[u8],
) -> bool {
    crate::ipc::send_media_record(writer, delivery_id, source, record_type, body).is_ok()
}

fn normalized_display(display: DisplayMetrics, status_visible: bool) -> DisplayMetrics {
    DisplayMetrics {
        columns: display.columns.clamp(10, 1000),
        // A visible status row is outside the pane area, so retain five host rows to leave the
        // minimum 6x4 float (4x2 content plus frame) representable.
        rows: display.rows.clamp(if status_visible { 5 } else { 4 }, 500),
        ..display
    }
}

fn terminfo_installed() -> bool {
    let candidates = [
        std::env::var_os("TERMINFO").map(PathBuf::from),
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".terminfo")),
        Some(PathBuf::from("/usr/share/terminfo")),
        Some(PathBuf::from("/usr/local/share/terminfo")),
    ];
    candidates
        .into_iter()
        .flatten()
        .any(|root| root.join("v/vvmux").exists())
}

fn extract_selection(terminal: &Terminal, start: (isize, usize), end: (isize, usize)) -> Vec<u8> {
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let mut output = String::new();
    for line_index in start.0..=end.0 {
        let Some(line) = terminal.viewport_line(line_index) else {
            continue;
        };
        let first = if line_index == start.0 { start.1 } else { 0 };
        let last = if line_index == end.0 {
            end.1 + 1
        } else {
            line.len()
        };
        let mut row = String::new();
        let mut column = first.min(line.len());
        let last = last.min(line.len());
        while column < last {
            let cell = &line[column];
            if let Some(width) = cell.tab_width {
                row.push('\t');
                column = column.saturating_add(usize::from(width).max(1));
                continue;
            }
            if !cell.wide_continuation && !cell.leading_wide_spacer {
                row.push(cell.ch);
                row.push_str(&cell.combining);
            }
            column += 1;
        }
        while row.ends_with(' ') {
            row.pop();
        }
        output.push_str(&row);
        if line_index != end.0 && !terminal.line_wrapped(line_index).unwrap_or(false) {
            output.push('\n');
        }
    }
    output.into_bytes()
}

fn sanitize_bracketed_paste(bytes: &[u8]) -> Vec<u8> {
    const END: &[u8] = b"\x1b[201~";
    let mut output = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while let Some(position) = bytes[cursor..]
        .windows(END.len())
        .position(|window| window == END)
    {
        let absolute = cursor + position;
        output.extend_from_slice(&bytes[cursor..absolute]);
        output.extend_from_slice(b"\x1b[201;~");
        cursor = absolute + END.len();
    }
    output.extend_from_slice(&bytes[cursor..]);
    output
}

#[cfg(windows)]
fn bracketed_paste_transition(previous: Option<bool>, enabled: bool) -> Option<&'static [u8]> {
    if previous == Some(enabled) {
        None
    } else if enabled {
        Some(ENABLE_BRACKETED_PASTE)
    } else {
        Some(DISABLE_BRACKETED_PASTE)
    }
}

#[cfg(windows)]
fn prepend_bracketed_paste_transition(bytes: &mut Vec<u8>, transition: &[u8]) {
    let mut output = Vec::with_capacity(transition.len() + bytes.len());
    output.extend_from_slice(transition);
    output.append(bytes);
    *bytes = output;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[cfg(unix)]
    use crate::ipc::{ChannelKind, establish};
    #[cfg(unix)]
    use vivid_protocol::media::{self, AudioPacket, VideoPacket};
    #[cfg(unix)]
    use vivid_protocol::messages;
    #[cfg(unix)]
    use vivid_protocol::wire::{Connection, ConnectionKind, Endpoint};

    #[cfg(unix)]
    fn test_transport(stream: UnixStream) -> crate::platform::Transport {
        let reader = stream.try_clone().unwrap();
        let timeout_stream = stream.try_clone().unwrap();
        crate::platform::Transport::new(
            Box::new(reader),
            Box::new(stream),
            crate::platform::ConnectionCancel::inert(),
            Arc::new(move |duration| timeout_stream.set_read_timeout(duration)),
        )
    }

    #[test]
    fn bracketed_paste_cannot_inject_terminator() {
        assert_eq!(sanitize_bracketed_paste(b"a\x1b[201~b"), b"a\x1b[201;~b");
    }

    #[cfg(windows)]
    #[test]
    fn outer_bracketed_paste_transitions_are_authoritative_and_deduplicated() {
        assert_eq!(
            bracketed_paste_transition(None, false),
            Some(DISABLE_BRACKETED_PASTE)
        );
        assert_eq!(bracketed_paste_transition(Some(false), false), None);
        assert_eq!(
            bracketed_paste_transition(Some(false), true),
            Some(ENABLE_BRACKETED_PASTE)
        );
        assert_eq!(bracketed_paste_transition(Some(true), true), None);
        assert_eq!(
            bracketed_paste_transition(Some(true), false),
            Some(DISABLE_BRACKETED_PASTE)
        );

        let mut enabled = b"render".to_vec();
        prepend_bracketed_paste_transition(&mut enabled, ENABLE_BRACKETED_PASTE);
        assert_eq!(enabled, b"\x1b[?2004hrender");

        let mut disabled = Vec::new();
        prepend_bracketed_paste_transition(&mut disabled, DISABLE_BRACKETED_PASTE);
        assert_eq!(disabled, b"\x1b[?2004l");
    }

    #[test]
    fn display_is_bounded() {
        let with_status = normalized_display(DisplayMetrics::default(), true);
        assert_eq!((with_status.columns, with_status.rows), (10, 5));
        let without_status = normalized_display(DisplayMetrics::default(), false);
        assert_eq!((without_status.columns, without_status.rows), (10, 4));
    }

    fn tab_with_floats() -> Tab {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        };
        let mut tree = TiledNode::leaf(1);
        tree.split(1, 2, crate::ipc::Axis::Vertical, area).unwrap();
        let mut floating = FloatingLayer::default();
        floating.insert(10, area, 60, 60);
        floating.insert(11, area, 40, 40);
        floating.set_pinned(11, true);
        Tab {
            id: 1,
            tree: Some(tree),
            floating,
            focused: 1,
            last_focused_tiled: Some(1),
            zoomed: None,
        }
    }

    #[test]
    fn projections_order_tiled_then_ordinary_then_pinned_and_zoom_hides_floats() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        };
        let mut tab = tab_with_floats();
        let layers = visible_projections(&tab, area)
            .iter()
            .map(|projection| (projection.pane_id, projection.layer))
            .collect::<Vec<_>>();
        assert_eq!(
            layers,
            [
                (1, PaneLayer::Tiled),
                (2, PaneLayer::Tiled),
                (10, PaneLayer::Floating),
                (11, PaneLayer::Pinned),
            ]
        );

        tab.floating.ordinary_visible = false;
        let visible = visible_projections(&tab, area)
            .iter()
            .map(|projection| projection.pane_id)
            .collect::<Vec<_>>();
        assert_eq!(
            visible,
            [1, 2, 11],
            "hidden ordinary floats leave projection"
        );

        tab.zoomed = Some(1);
        let zoomed = visible_projections(&tab, area);
        assert_eq!(
            zoomed.len(),
            1,
            "zoom hides every other pane, pinned included"
        );
        assert_eq!(zoomed[0].pane_id, 1);
        assert_eq!(zoomed[0].outer, area);

        tab.tree = None;
        tab.zoomed = None;
        tab.floating.ordinary_visible = true;
        let floating_only = visible_projections(&tab, area)
            .iter()
            .map(|projection| projection.pane_id)
            .collect::<Vec<_>>();
        assert_eq!(floating_only, [10, 11], "floating-only tabs project");
    }

    #[test]
    fn media_pane_priority_is_a_strict_focus_and_layer_prefix() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        };
        let mut tab = tab_with_floats();
        let projections = visible_projections(&tab, area);
        assert_eq!(projection_pane_priority(&tab, &projections), [1, 11, 10, 2]);

        tab.set_focus(10);
        let projections = visible_projections(&tab, area);
        assert_eq!(projection_pane_priority(&tab, &projections), [10, 11, 1, 2]);
    }

    #[test]
    fn float_pointer_hit_testing_distinguishes_move_edges_and_corners() {
        let rect = Rect {
            x: 10,
            y: 5,
            width: 20,
            height: 10,
        };
        assert_eq!(
            float_pointer_target(rect, 15, 5, 2),
            Some(FloatPointerTarget::Move)
        );
        assert_eq!(
            float_pointer_target(rect, 10, 5, 2),
            Some(FloatPointerTarget::Resize(EdgeMask {
                left: true,
                top: true,
                ..EdgeMask::default()
            }))
        );
        assert_eq!(
            float_pointer_target(rect, 29, 14, 2),
            Some(FloatPointerTarget::Resize(EdgeMask {
                right: true,
                bottom: true,
                ..EdgeMask::default()
            }))
        );
        assert_eq!(float_pointer_target(rect, 15, 6, 2), None);
    }

    #[test]
    fn fallback_focus_prefers_pinned_then_ordinary_then_tiled() {
        let mut tab = tab_with_floats();
        assert_eq!(tab.fallback_focus(), Some(11), "topmost pinned float first");
        tab.floating.remove(11);
        assert_eq!(
            tab.fallback_focus(),
            Some(10),
            "then topmost visible ordinary float"
        );
        tab.floating.ordinary_visible = false;
        assert_eq!(
            tab.fallback_focus(),
            Some(1),
            "then the last focused tiled pane"
        );
        tab.last_focused_tiled = None;
        assert_eq!(tab.fallback_focus(), Some(1), "then the first tiled leaf");
        tab.tree = None;
        tab.floating.ordinary_visible = true;
        assert_eq!(tab.fallback_focus(), Some(10));
        tab.floating.remove(10);
        assert_eq!(tab.fallback_focus(), None);
        assert!(
            tab.is_empty(),
            "a tab with no tree and no floats is removable"
        );
    }

    #[test]
    fn set_focus_tracks_tiled_history_and_raises_floats() {
        let mut tab = tab_with_floats();
        tab.set_focus(2);
        assert_eq!(tab.last_focused_tiled, Some(2));
        tab.floating.insert(
            12,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 23,
            },
            40,
            40,
        );
        tab.set_focus(10);
        assert_eq!(
            tab.last_focused_tiled,
            Some(2),
            "focusing a float keeps the tiled fallback"
        );
        assert_eq!(
            tab.floating.pane_ids(),
            [12, 10, 11],
            "explicitly focusing a float raises it within its class"
        );
    }

    #[test]
    fn media_projection_is_invalidated_by_active_layout_changes() {
        let first_tab = MediaProjectionKey {
            virtual_revision: 9,
            layout_revision: 3,
        };
        assert!(should_sync_media(false, None, first_tab));
        assert!(!should_sync_media(false, Some(first_tab), first_tab));

        let second_tab = MediaProjectionKey {
            virtual_revision: 9,
            layout_revision: 4,
        };
        assert!(should_sync_media(false, Some(first_tab), second_tab));
        assert!(should_sync_media(true, Some(second_tab), second_tab));
    }

    #[test]
    fn projection_sync_does_not_duplicate_the_triggering_live_raster() {
        let raster = (3, 7);
        assert!(!should_replay_retained(raster, Some(raster)));
        assert!(should_replay_retained(raster, None));
        assert!(should_replay_retained(raster, Some((3, 8))));
    }

    #[test]
    fn fragment_ids_are_stable_and_recycle_only_disappeared_rectangles() {
        let left = FixedRect::new(0, 0, 10, 10).unwrap();
        let right = FixedRect::new(20, 0, 10, 10).unwrap();
        let bottom = FixedRect::new(0, 20, 10, 10).unwrap();
        let mut map = FragmentMap::default();
        assert_eq!(map.assign(&[left, right]).unwrap(), [(0, left), (1, right)]);
        assert_eq!(
            map.assign(&[right, left]).unwrap(),
            [(1, right), (0, left)],
            "snapshot ordering cannot renumber unchanged rectangles"
        );
        assert_eq!(
            map.assign(&[right, bottom]).unwrap(),
            [(1, right), (0, bottom)],
            "the disappeared left rectangle returns ID zero to the pool"
        );
        // Inactive-tab hiding does not call assign at all, so the next identical geometry keeps
        // both assignments.
        assert_eq!(
            map.assign(&[right, bottom]).unwrap(),
            [(1, right), (0, bottom)]
        );
        assert_eq!(
            FragmentMap::default().assign(&[right]).unwrap(),
            [(0, right)],
            "destroy/recreate starts with a fresh logical-node map"
        );
    }

    #[test]
    fn maximum_fragment_scene_stays_bounded_across_drag_storms() {
        let mut maps = (0..256)
            .map(|logical| (logical, FragmentMap::default()))
            .collect::<HashMap<_, _>>();
        for step in 0..1000_i64 {
            for (logical, map) in &mut maps {
                let fragments = (0..MAX_NODE_FRAGMENTS)
                    .map(|fragment| {
                        FixedRect::new(step + fragment as i64 * 3, *logical as i64 * 2, 2, 1)
                            .unwrap()
                    })
                    .collect::<Vec<_>>();
                let assignments = map.assign(&fragments).unwrap();
                assert_eq!(assignments.len(), MAX_NODE_FRAGMENTS);
                assert!(
                    assignments
                        .iter()
                        .all(|(id, _)| usize::from(*id) < MAX_NODE_FRAGMENTS)
                );
            }
        }
        assert_eq!(maps.len(), 256);
        assert_eq!(
            maps.values().map(|map| map.rectangles.len()).sum::<usize>(),
            256 * MAX_NODE_FRAGMENTS,
            "recycled drag geometry cannot grow fragment maps monotonically"
        );
        assert_eq!(
            MAX_PROJECTED_NODES / MAX_NODE_FRAGMENTS,
            32,
            "the strict global prefix admits exactly 32 eight-fragment nodes"
        );
    }

    fn projected_test_node(width: i64, height: i64) -> crate::media::SceneNode {
        crate::media::SceneNode {
            producer: 3,
            pane: 7,
            config: vivid_protocol::messages::ParsedSceneNode {
                node: vivid_protocol::messages::ParsedNodeConfig {
                    node_id: 9,
                    source_id: 4,
                    context_id: 1,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    text_layer: 1,
                    z_index: 2,
                    visible: true,
                    anchor_id: None,
                },
                clip: None,
            },
            retained_anchor: None,
        }
    }

    #[test]
    fn logical_node_projection_clips_and_subtracts_higher_outer_rectangles() {
        let pane = Rect {
            x: 1,
            y: 2,
            width: 10,
            height: 8,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 12,
        };
        let occluder = from_cells(Rect {
            x: 4,
            y: 3,
            width: 3,
            height: 4,
        })
        .unwrap();
        let node = projected_test_node(20_i64 << 32, 20_i64 << 32);
        let projected = project_logical_node(&node, pane, area, &[occluder]).unwrap();
        assert!(!projected.fragments.is_empty());
        for fragment in &projected.fragments {
            assert!(intersect(fragment.clip, occluder).is_none());
            assert_eq!(fragment.node.x, i64::from(pane.x) << 32);
            assert_eq!(fragment.node.y, i64::from(pane.y) << 32);
        }
        let pane_area = (i128::from(pane.width) * i128::from(pane.height)) << 64;
        let overlap = intersect(from_cells(pane).unwrap(), occluder).unwrap();
        let visible_area = projected
            .fragments
            .iter()
            .map(|fragment| i128::from(fragment.clip.width) * i128::from(fragment.clip.height))
            .sum::<i128>();
        assert_eq!(
            visible_area,
            pane_area - i128::from(overlap.width) * i128::from(overlap.height)
        );

        let covered =
            project_logical_node(&node, pane, area, &[from_cells(pane).unwrap()]).unwrap();
        assert!(covered.fragments.is_empty());
    }

    #[test]
    fn logical_node_fragment_limit_is_atomic() {
        let pane = Rect {
            x: 0,
            y: 0,
            width: 24,
            height: 5,
        };
        let node = projected_test_node(24_i64 << 32, 5_i64 << 32);
        let occluders = (1..18)
            .step_by(2)
            .map(|x| {
                from_cells(Rect {
                    x,
                    y: 1,
                    width: 1,
                    height: 3,
                })
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            project_logical_node(&node, pane, pane, &occluders).unwrap_err(),
            ProjectionIssue::FragmentLimit
        );
    }

    #[test]
    #[cfg(unix)]
    fn play_snapshot_precedes_the_next_linked_media_record() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("session-vivid.sock");
        let (media_sender, media_receiver) = mpsc::sync_channel(8);
        let config = Config::default();
        let vivid = match VirtualVivid::start_with_events(
            socket.clone(),
            config.media.clone(),
            Some(media_sender),
        ) {
            Ok(vivid) => vivid,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping session media-order socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = vivid.issue_pane_capability(7).unwrap();
        vivid.update_metrics(7, 80, 22, (10, 20));

        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        server_stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let client_establish = std::thread::spawn(move || {
            establish(test_transport(client_stream), ChannelKind::Control)
        });
        let (mut client_reader, _server_writer) =
            establish(test_transport(server_stream), ChannelKind::Control).unwrap();
        let (_unused_reader, client_writer) = client_establish.join().unwrap().unwrap();
        let (actor_sender, _actor_receiver) = mpsc::sync_channel(8);
        let (response_sender, _response_receiver) = mpsc::sync_channel(8);
        let mut actor = SessionActor {
            name: "media-order".into(),
            config,
            sender: actor_sender,
            panes: BTreeMap::new(),
            tabs: vec![Tab {
                id: 1,
                tree: Some(TiledNode::leaf(7)),
                floating: FloatingLayer::default(),
                focused: 7,
                last_focused_tiled: Some(7),
                zoomed: None,
            }],
            active_tab: 0,
            next_pane_id: 8,
            next_tab_id: 2,
            copy_buffer: Vec::new(),
            frame_id: 0,
            last_screen: None,
            #[cfg(windows)]
            outer_bracketed_paste: None,
            force_full: false,
            pending_render: false,
            layout_revision: 1,
            last_media_projection: None,
            media_projection_revision: 0,
            outer_virtual_revision: 0,
            outer_projection_revision: 0,
            outer_attachment_generations: HashMap::new(),
            fragment_assignments: HashMap::new(),
            last_projection_warning: None,
            pointer_drag: None,
            float_modal: None,
            next_float_mode: 0,
            session_sequence: 1,
            response_sender,
            automation_inflight: HashMap::new(),
            pending_actor_work: HashSet::new(),
            automation_waiters: Vec::new(),
            exit_tombstones: VecDeque::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            vivid,
            bridge_metrics: crate::metrics::BridgeMetrics::default(),
            client_ipc: None,
            attached: Some(AttachedClient {
                id: 1,
                writer: client_writer,
                display: DisplayMetrics {
                    columns: 80,
                    rows: 24,
                    cell_width: 10,
                    cell_height: 20,
                },
                acknowledged_frame: 0,
                vivid: true,
                rendered_session_sequence: 0,
                frame_sequences: VecDeque::new(),
            }),
            last_display: DisplayMetrics {
                columns: 80,
                rows: 24,
                cell_width: 10,
                cell_height: 20,
            },
        };

        let endpoint = Endpoint::Unix(socket);
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))
            .unwrap();
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::WELCOME
        );
        let video = messages::VideoSourceConfig {
            codec_string: None,
            decoder_config: None,
            source_id: 9,
            codec: "h264",
            packetization: "h264-annexb-au-v1",
            extradata: &[],
            width: 16,
            height: 16,
            profile: 0,
            level: 0,
            bitrate: 0,
            color_primaries: 1,
            transfer: 1,
            matrix: 1,
            range: 1,
            sar_num: 1,
            sar_den: 1,
            max_access_unit_bytes: 1024,
        };
        control
            .write_record(
                messages::CREATE_VIDEO,
                0,
                9,
                &messages::create_video(2, &video),
            )
            .unwrap();
        let video_ready =
            messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();
        let audio = messages::AudioSourceConfig {
            codec_string: None,
            source_id: 10,
            linked_video_source_id: Some(9),
            codec: "pcm_s16le",
            packetization: "pcm-packet-v1",
            extradata: &[],
            sample_rate: 48_000,
            channels: 2,
            channel_mask: 3,
            bitrate: 0,
            max_access_unit_bytes: 1024,
        };
        control
            .write_record(
                messages::CREATE_AUDIO,
                0,
                10,
                &messages::create_audio(3, &audio),
            )
            .unwrap();
        let audio_ready =
            messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();

        actor.sync_media(true);
        assert!(matches!(
            client_reader.recv_server().unwrap(),
            ServerMessage::MediaSnapshot { .. }
        ));

        let mut audio_media = Connection::open(&endpoint, ConnectionKind::Audio).unwrap();
        audio_media
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                10,
                &messages::attach_channel(&audio_ready.media_ticket),
            )
            .unwrap();
        let audio_packet = media::audio_packet_body(AudioPacket {
            epoch: 1,
            packet_id: 1,
            pts_us: 0,
            dts_us: 0,
            duration_us: 20_000,
            trim_start_samples: 0,
            trim_end_samples: 0,
            data: &[0, 0, 0, 0],
        })
        .unwrap();
        audio_media
            .write_record(messages::AUDIO_PACKET, 0, 10, &audio_packet)
            .unwrap();
        actor.forward_media(media_receiver.recv_timeout(Duration::from_secs(1)).unwrap());
        let delivery_id = match client_reader.recv_server().unwrap() {
            ServerMessage::MediaRecord {
                delivery_id,
                record_type: messages::AUDIO_PACKET,
                ..
            } => delivery_id,
            other => panic!("expected pre-roll audio record, got {other:?}"),
        };
        actor.vivid.complete_bridge_delivery(delivery_id, true);

        control
            .write_record(messages::PLAY, 0, 9, &messages::play(4, 9, 100_000))
            .unwrap();
        let returned_audio_credit = control.read_record().unwrap();
        assert_eq!(returned_audio_credit.record_type, messages::CREDIT);
        assert_eq!(returned_audio_credit.object_id, 10);
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        let mut video_media = Connection::open(&endpoint, ConnectionKind::Video).unwrap();
        video_media
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                9,
                &messages::attach_channel(&video_ready.media_ticket),
            )
            .unwrap();
        let video_packet = media::video_packet_body(VideoPacket {
            epoch: 1,
            packet_id: 1,
            pts_us: 0,
            dts_us: 0,
            duration_us: 33_000,
            key: true,
            data: &[0, 0, 0, 1, 0x65, 0x88],
        })
        .unwrap();
        video_media
            .write_record(messages::VIDEO_PACKET, 0, 9, &video_packet)
            .unwrap();
        actor.forward_media(media_receiver.recv_timeout(Duration::from_secs(1)).unwrap());

        match client_reader.recv_server().unwrap() {
            ServerMessage::MediaSnapshot { sources, .. } => {
                assert!(
                    sources
                        .iter()
                        .any(|source| source.key.source == 9 && source.playing)
                );
            }
            other => panic!("PLAY snapshot must precede post-PLAY media, got {other:?}"),
        }
        let video_delivery_id = match client_reader.recv_server().unwrap() {
            ServerMessage::MediaRecord {
                delivery_id,
                record_type: messages::VIDEO_PACKET,
                ..
            } => delivery_id,
            other => panic!("expected post-PLAY video record, got {other:?}"),
        };
        actor
            .vivid
            .complete_bridge_delivery(video_delivery_id, true);

        control
            .write_record(messages::EOS, 0, 9, &messages::eos(5, 9, 1))
            .unwrap();
        let returned_video_credit = control.read_record().unwrap();
        assert_eq!(returned_video_credit.record_type, messages::CREDIT);
        assert_eq!(returned_video_credit.object_id, 9);
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        actor.sync_media(false);
        match client_reader.recv_server().unwrap() {
            ServerMessage::MediaSnapshot { sources, .. } => {
                assert!(
                    sources
                        .iter()
                        .any(|source| source.key.source == 9 && source.playing),
                    "EOS must leave buffered outer playback running"
                );
            }
            other => panic!("expected post-EOS projection snapshot, got {other:?}"),
        }
    }

    #[test]
    fn automation_keys_honor_cursor_keypad_modifiers_and_reject_unknown_values() {
        let mut modes = TerminalModes::default();
        assert_eq!(
            encode_automation_key("ArrowUp", &[], modes).unwrap(),
            b"\x1b[A"
        );
        modes.application_cursor = true;
        assert_eq!(
            encode_automation_key("ArrowUp", &[], modes).unwrap(),
            b"\x1bOA"
        );
        modes.application_keypad = true;
        assert_eq!(
            encode_automation_key("Keypad7", &[], modes).unwrap(),
            b"\x1bOw"
        );
        assert_eq!(
            encode_automation_key("c", &["Ctrl".into()], modes).unwrap(),
            b"\x03"
        );
        assert_eq!(
            encode_automation_key("Tab", &["Shift".into()], modes).unwrap(),
            b"\x1b[Z"
        );
        assert!(encode_automation_key("NoSuchKey", &[], modes).is_err());
        assert!(encode_automation_key("x", &["Hyper".into()], modes).is_err());
    }

    #[test]
    fn automation_raw_request_limits_are_enforced() {
        assert!(
            validate_automation_method(&AutomationMethod::Key {
                key: "x".into(),
                modifiers: Vec::new(),
                repeat: 0,
            })
            .is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::GetText { rows: Some(1001) }).is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::GetGrid {
                start_line: Some(0),
                row_count: None,
                since_screen: None,
            })
            .is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::WaitText {
                text: "x".into(),
                regex: false,
                after_screen: None,
                timeout_ms: 0,
            })
            .is_err()
        );
    }
}
