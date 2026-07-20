use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use vvmux_terminal::pty::{PtyControl, PtyInput, PtyProcess};
use vvmux_terminal::{Terminal, TerminalEvent};

use crate::config::Config;
use crate::ipc::{
    Action, Axis, BridgeClipRect, BridgeNode, BridgePlayRequest, BridgeSource, BridgeSourceKey,
    BridgeSourceKind, ClientMessage, Direction, DisplayMetrics, FloatingEditCommand,
    FloatingEditKind, MouseEvent, MouseKind, ServerMessage, SharedWriter,
};
use crate::layout::{
    EdgeMask, FloatingLayer, PaneId, PaneLayer, PaneProjection, Rect, TiledNode, directional_focus,
};
use crate::media::VirtualVivid;
use crate::platform::VirtualPresenterEndpoint;
use crate::region::{FixedRect, from_cells, intersect, subtract_all};
use crate::screen::{ScreenBuffer, ansi_diff};

const EVENT_QUEUE: usize = 1024;
const COPY_BUFFER_LIMIT: usize = 1024 * 1024;
const MAX_NODE_FRAGMENTS: usize = 8;
const MAX_PROJECTED_NODES: usize = 256;
const INPUT_STATUS_INTERVAL: Duration = Duration::from_secs(1);

pub enum ActorEvent {
    Client {
        id: u64,
        writer: SharedWriter,
        message: ClientMessage,
    },
    Disconnected(u64),
    PtyOutput(PaneId, Vec<u8>),
    PtyExit(PaneId),
    Media(crate::media::MediaEvent),
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
}

struct Pane {
    id: PaneId,
    terminal: Terminal,
    input: PtyInput,
    control: PtyControl,
    copy: Option<CopyState>,
    vivid_metrics: Option<(u16, u16, u16, u16)>,
    last_input_warning: Option<Instant>,
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

#[derive(Debug, Clone)]
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
    next_pane_id: PaneId,
    next_tab_id: u64,
    copy_buffer: Vec<u8>,
    frame_id: u64,
    last_screen: Option<ScreenBuffer>,
    force_full: bool,
    pending_render: bool,
    layout_revision: u64,
    last_media_projection: Option<MediaProjectionKey>,
    media_projection_revision: u64,
    fragment_assignments: HashMap<(u64, u64), FragmentMap>,
    last_projection_warning: Option<MediaProjectionKey>,
    pointer_drag: Option<PointerDrag>,
    float_modal: Option<FloatModal>,
    next_float_mode: u64,
    shutdown: Arc<AtomicBool>,
    vivid: VirtualVivid,
}

pub fn start(
    name: String,
    config: Config,
    vivid_endpoint: VirtualPresenterEndpoint,
) -> io::Result<ActorHandle> {
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE);
    let (media_sender, media_receiver) = mpsc::sync_channel(64);
    let shutdown = Arc::new(AtomicBool::new(false));
    let vivid =
        VirtualVivid::start_with_events(vivid_endpoint, config.media.clone(), Some(media_sender))?;
    let media_events = sender.clone();
    std::thread::Builder::new()
        .name("vvmux-media-events".into())
        .spawn(move || {
            while let Ok(event) = media_receiver.recv() {
                if media_events.send(ActorEvent::Media(event)).is_err() {
                    break;
                }
            }
        })?;
    let mut actor = SessionActor {
        name,
        config,
        sender: sender.clone(),
        panes: BTreeMap::new(),
        tabs: Vec::new(),
        active_tab: 0,
        attached: None,
        next_pane_id: 1,
        next_tab_id: 1,
        copy_buffer: Vec::new(),
        frame_id: 0,
        last_screen: None,
        force_full: true,
        pending_render: false,
        layout_revision: 0,
        last_media_projection: None,
        media_projection_revision: 0,
        fragment_assignments: HashMap::new(),
        last_projection_warning: None,
        pointer_drag: None,
        float_modal: None,
        next_float_mode: 0,
        shutdown: shutdown.clone(),
        vivid,
    };
    actor.new_tab()?;
    std::thread::Builder::new()
        .name("vvmux-session".into())
        .spawn(move || actor.run(receiver))?;
    Ok(ActorHandle { sender, shutdown })
}

impl SessionActor {
    fn run(&mut self, receiver: mpsc::Receiver<ActorEvent>) {
        let interval = Duration::from_millis(self.config.general.render_interval_ms);
        let mut render_at = Instant::now();
        loop {
            let timeout = if self.pending_render {
                render_at.saturating_duration_since(Instant::now())
            } else {
                Duration::from_secs(1)
            };
            match receiver.recv_timeout(timeout) {
                Ok(event) => {
                    if self.handle_event(event).is_err() {
                        self.force_full = true;
                    }
                    if self.pending_render && render_at <= Instant::now() {
                        self.render();
                        render_at = Instant::now() + interval;
                    } else if self.pending_render && render_at < Instant::now() + interval {
                        // Keep the already scheduled coalescing boundary.
                    } else if self.pending_render {
                        render_at = Instant::now() + interval;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.pending_render {
                        self.render();
                        render_at = Instant::now() + interval;
                    }
                    self.sync_media(false);
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

    fn handle_event(&mut self, event: ActorEvent) -> io::Result<()> {
        match event {
            ActorEvent::Client {
                id,
                writer,
                message,
            } => {
                self.handle_client(id, writer, message)?;
            }
            ActorEvent::Disconnected(id) => {
                if self.attached.as_ref().is_some_and(|client| client.id == id) {
                    self.cancel_pointer_drag(true);
                    self.attached = None;
                    self.last_screen = None;
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
            ActorEvent::PtyExit(pane_id) => {
                self.close_pane(pane_id);
            }
            ActorEvent::Media(event) => {
                // PLAY/PAUSE/EOS arrive on the producer's control connection while media arrives
                // on independent source connections. Publish any resulting authoritative
                // projection revision before forwarding the next media record. Otherwise a busy
                // stream can starve the actor's idle sync, fill outer pre-roll with later audio,
                // and leave video waiting for a PLAY snapshot that is queued behind that media.
                self.sync_media(false);
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
        }
        Ok(())
    }

    fn handle_client(
        &mut self,
        id: u64,
        writer: SharedWriter,
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
                self.attached = Some(AttachedClient {
                    id,
                    writer: writer.clone(),
                    display: normalized_display(display, self.config.general.status_visible),
                    acknowledged_frame: 0,
                    vivid,
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
                    }
                }
            }
            ClientMessage::BridgeNeedKeyframes(sources) => {
                if self.client_is(id) {
                    self.vivid.request_keyframes(
                        &sources
                            .into_iter()
                            .map(|source| (source.producer, source.source))
                            .collect::<Vec<_>>(),
                    );
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
        }
        Ok(())
    }

    fn client_is(&self, id: u64) -> bool {
        self.attached.as_ref().is_some_and(|client| client.id == id)
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
                self.schedule_render();
            }
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
        let Some((focused, tree)) = self.active_tab().map(|tab| (tab.focused, tab.tree.clone()))
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
        if self.spawn_pane(pane_id).is_err() {
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
        if self.spawn_pane(pane_id).is_err() {
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
        self.spawn_pane(pane_id)?;
        self.next_pane_id += 1;
        self.tabs.push(Tab {
            id: self.next_tab_id,
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

    fn spawn_pane(&mut self, pane_id: PaneId) -> io::Result<()> {
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
            ("VVMUX_TAB_ID".into(), self.next_tab_id.to_string()),
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
                let _ = waiter.wait();
                let _ = exit_sender.send(ActorEvent::PtyExit(pane_id));
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
        self.resize_all();
        self.schedule_render();
    }

    /// Visibility, z-order, pin, and focus changes must refresh the media projection even when
    /// no rectangle changed: occlusion and quota priority depend on them. No PTY resizing is
    /// needed on this path.
    fn projection_changed(&mut self) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
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
        resize_failures.sort_unstable();
        resize_failures.dedup();
        for pane in resize_failures {
            self.status(&format!("pane {pane} PTY resize failed"));
            self.close_pane(pane);
        }
    }

    fn render(&mut self) {
        self.pending_render = false;
        let Some(client) = &self.attached else {
            return;
        };
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
        let bytes = ansi_diff(self.last_screen.as_ref(), &screen, self.force_full);
        let chunks = bytes.chunks(256 * 1024).collect::<Vec<_>>();
        let mut sent = true;
        for (index, chunk) in chunks.iter().enumerate() {
            let message = ServerMessage::Render {
                frame_id: self.frame_id,
                full: self.force_full && index == 0,
                last: index + 1 == chunks.len(),
                bytes: chunk.to_vec(),
            };
            if crate::ipc::send(&client.writer, &message).is_err() {
                sent = false;
                break;
            }
        }
        if !sent {
            self.attached = None;
            self.last_screen = None;
        } else {
            self.last_screen = Some(screen);
        }
        self.force_full = false;
        self.sync_media(false);
    }

    fn sync_media(&mut self, force: bool) {
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
            .collect();
        let projection_key = MediaProjectionKey {
            virtual_revision: self.vivid.revision(),
            layout_revision: self.layout_revision,
        };
        if !should_sync_media(force, self.last_media_projection, projection_key) {
            return;
        }
        // projection_snapshot marks active video sources as requiring a fresh keyframe. Only call
        // it after deciding that the client will actually rebuild its outer Vivid session.
        let mut snapshot = self.vivid.projection_snapshot(&panes);
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
                kind: bridge_source_kind(source.key, &source.descriptor),
                playing: source.playing,
                play_request: bridge_play_request(source.play_request),
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

    fn content_area(&self) -> Rect {
        let display = self.attached.as_ref().map_or(
            DisplayMetrics {
                columns: 80,
                rows: 24,
                cell_width: 0,
                cell_height: 0,
            },
            |client| client.display,
        );
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
        self.schedule_render();
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
) -> BridgeSourceKind {
    match descriptor {
        crate::media::SourceDescriptor::Raster(config) => BridgeSourceKind::Raster {
            width: config.width,
            height: config.height,
            alpha_mode: config.alpha_mode,
            compression_mode: config.compression_mode,
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
    const CHUNK: usize = 128 * 1024;
    for (index, bytes) in body.chunks(CHUNK).enumerate() {
        let offset = index * CHUNK;
        if crate::ipc::send(
            writer,
            &ServerMessage::MediaRecord {
                delivery_id,
                source,
                record_type,
                offset: offset as u32,
                total: body.len() as u32,
                last: offset + bytes.len() == body.len(),
                bytes: bytes.to_vec(),
            },
        )
        .is_err()
        {
            return false;
        }
    }
    true
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
        for cell in &line[first.min(line.len())..last.min(line.len())] {
            if !cell.wide_continuation {
                output.push(cell.ch);
                output.push_str(&cell.combining);
            }
        }
        while output.ends_with(' ') {
            output.pop();
        }
        if line_index != end.0 {
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
            force_full: false,
            pending_render: false,
            layout_revision: 1,
            last_media_projection: None,
            media_projection_revision: 0,
            fragment_assignments: HashMap::new(),
            last_projection_warning: None,
            pointer_drag: None,
            float_modal: None,
            next_float_mode: 0,
            shutdown: Arc::new(AtomicBool::new(false)),
            vivid,
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
            }),
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
            client_reader.recv::<ServerMessage>().unwrap(),
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
        actor
            .handle_event(ActorEvent::Media(
                media_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            ))
            .unwrap();
        let delivery_id = match client_reader.recv::<ServerMessage>().unwrap() {
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
        actor
            .handle_event(ActorEvent::Media(
                media_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            ))
            .unwrap();

        match client_reader.recv::<ServerMessage>().unwrap() {
            ServerMessage::MediaSnapshot { sources, .. } => {
                assert!(
                    sources
                        .iter()
                        .any(|source| source.key.source == 9 && source.playing)
                );
            }
            other => panic!("PLAY snapshot must precede post-PLAY media, got {other:?}"),
        }
        let video_delivery_id = match client_reader.recv::<ServerMessage>().unwrap() {
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
        match client_reader.recv::<ServerMessage>().unwrap() {
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
}
