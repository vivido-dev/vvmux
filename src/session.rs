use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use base64::Engine;
use vvmux_terminal::TerminalHyperlink;
use vvmux_terminal::pty::{PtyControl, PtyExitStatus, PtyInput, PtyProcess};
use vvmux_terminal::{
    Cell, KittyGraphicsCommand, Terminal, TerminalColor, TerminalEvent, TerminalModes,
    UnderlineStyle,
};

use crate::agent::{AgentRuntime, AgentSnapshot, DetectorHandle, ProbeTarget, ProcessUpdate};
use crate::config::{Config, OpenMode};
use crate::ipc::{
    Action, AutomationCompletion, AutomationError, AutomationMethod, AutomationRequest,
    AutomationResponse, Axis, BridgeClipRect, BridgeNode, BridgePlayRequest, BridgeSource,
    BridgeSourceDescriptor, BridgeSourceKey, BridgeSourceKind, BridgeSurface, BridgeSurfaceKey,
    ClientMessage, Direction, DisplayMetrics, FloatingEditCommand, FloatingEditKind,
    MediaTrackIdentity, MediaTrackWaitCondition, MouseEvent, MouseKind, PluginEventEnvelope,
    ServerMessage, SharedWriter,
};
use crate::layout::{
    EdgeMask, FloatingLayer, PaneId, PaneLayer, PaneProjection, Rect, TiledNode, directional_focus,
};
use crate::layout_file::{
    LayoutFile, LayoutFloat, LayoutNode, LayoutPlan, LayoutTab, MAX_LAYOUT_PANES, MAX_LAYOUT_TABS,
};
use crate::media::VirtualVivid;
use crate::media_trace::{MediaKeyframeStage, MediaTraceFilter, MediaTraceJournal, MediaTraceKind};
use crate::platform::VirtualPresenterEndpoint;
use crate::region::{FixedRect, from_cells, intersect, subtract_all};
use crate::screen::{LinkStyle, ScreenBuffer, ansi_diff};
use crate::search::{
    PromptAction, SearchDirection, SearchMatch, SearchPattern, apply_prompt_key, find_all,
    find_next, find_on_line, row_text_with_columns,
};

const EVENT_QUEUE: usize = 1024;
/// Slots on the dedicated media-event receiver.
///
/// Total queued media bytes are separately bounded by `media.ipc_queue_bytes`, so this only needs
/// enough slots that small records — audio access units especially — cannot exhaust it while that
/// byte budget is far from spent.
const MEDIA_EVENT_QUEUE: usize = 256;
/// Maximum media deliveries forwarded before the actor gives its general queue another turn.
///
/// The media receiver is bounded, but a live producer can refill it while it is being drained.
/// Without a per-turn limit, "drain to exhaustion" can therefore starve detach, input, credits,
/// and projection updates indefinitely.
const MEDIA_EVENTS_PER_TURN: usize = 32;
const COPY_BUFFER_LIMIT: usize = 1024 * 1024;
const MOUSE_MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MAX_NODE_FRAGMENTS: usize = 8;
const MAX_PROJECTED_NODES: usize = 256;
const INPUT_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const MAX_AUTOMATION_REQUESTS_PER_CLIENT: usize = 64;
const MAX_AUTOMATION_WAITERS: usize = 256;
const MAX_PENDING_ACTOR_WORK: usize = 256;
const MAX_PENDING_MEDIA_PROJECTIONS: usize = 64;
/// Frames allowed outstanding before rendering pauses for the client to catch up.
///
/// The acknowledgement arrives after the client writes the frame to its terminal, so this bounds
/// how far the server may run ahead of what the user can actually see. Media snapshots and media
/// records are never gated by it: a slow terminal must not stall the projected scene.
const MAX_UNACKNOWLEDGED_FRAMES: u64 = 8;
const KITTY_GRAPHICS_SESSION_BYTES: usize = 64 * 1024 * 1024;
const AUTOMATION_RESPONSE_QUEUE: usize = 8;
const SCREEN_CHANGE_HISTORY: usize = 1024;
const EXIT_TOMBSTONES: usize = 128;
const PLUGIN_EVENT_JOURNAL: usize = 1024;
const PLUGIN_EVENT_JOURNAL_BYTES: usize = 2 * 1024 * 1024;
const PLUGIN_EVENT_SUBSCRIPTIONS: usize = 16;
const PLUGIN_EVENT_STREAM_QUEUE: usize = 64;
const PTY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const AUTOMATION_REPLY_LIMIT: usize = 16 * 1024 * 1024;
/// A `run` command is one shell command line, not a script; this only has to be generous.
const MAX_RUN_COMMAND_BYTES: usize = 64 * 1024;
const MAX_TAB_NAME_BYTES: usize = 128;
/// A save target is a file name or a path, never a document; this only has to be generous.
const MAX_LAYOUT_NAME_BYTES: usize = 512;
/// How long a save result stays in the status row before the tab list returns.
const STATUS_NOTICE_DURATION: Duration = Duration::from_secs(4);
#[cfg(windows)]
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
#[cfg(windows)]
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";

#[derive(Debug, Clone, PartialEq, Eq)]
enum CallerOrigin {
    Automation {
        client_id: u64,
    },
    Plugin {
        plugin_id: String,
        plugin_instance: String,
    },
}

#[derive(Debug, Clone)]
struct CallerContext {
    origin: CallerOrigin,
    session_instance: String,
    focused_fallback: bool,
    capabilities: BTreeSet<vvmux_plugin_api::Permission>,
}

enum SessionCommand {
    InspectSession,
    ReadPaneText {
        pane_id: Option<PaneId>,
        rows: Option<usize>,
        max_bytes: usize,
    },
    WritePaneInput {
        pane_id: Option<PaneId>,
        bytes: Vec<u8>,
    },
    OpenPluginPane {
        launch: Box<PluginPaneLaunch>,
    },
    ClosePane {
        pane_id: PaneId,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct PluginPaneLaunch {
    pub(crate) scope: crate::plugin_supervisor::RuntimeScope,
    pub(crate) package_digest: String,
    pub(crate) package_root: PathBuf,
    pub(crate) vivi_helper: Option<PathBuf>,
    pub(crate) pane: vvmux_plugin_api::Pane,
}

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
        pane_id: PaneId,
        byte_count: usize,
        report: bool,
    },
    PluginComplete {
        reply: AutomationReplyTarget,
        result: Result<serde_json::Value, AutomationError>,
    },
    PluginNotice {
        reference: String,
        result: Result<(), String>,
    },
    PluginHostCall {
        scope: crate::plugin_supervisor::RuntimeScope,
        cause: Option<crate::plugin_supervisor::PluginCause>,
        call: vvmux_plugin_api::HostCall,
        reply: mpsc::SyncSender<Result<serde_json::Value, AutomationError>>,
    },
    PluginPaneOpen {
        launch: PluginPaneLaunch,
        reply: AutomationReplyTarget,
    },
    PluginPanesClose {
        plugin_id: String,
        package_digest: String,
    },
    PluginReloaded {
        result: Result<serde_json::Value, AutomationError>,
    },
    AgentCatalogApplied {
        generation: u64,
        catalog: Arc<crate::agent::AgentCatalog>,
    },
    PluginLifecycle {
        name: String,
        payload: serde_json::Value,
        context: Option<vvmux_plugin_api::InvocationContext>,
    },
    /// A media event is waiting on the dedicated media receiver.
    ///
    /// Carries no payload: it exists only to wake the actor promptly. Losing one to a full queue
    /// is harmless because the actor drains media at the top of every iteration anyway.
    MediaReady,
    /// The config file settled on new contents, or a reload was asked for directly.
    ///
    /// Carries no payload: the actor re-reads the file itself, so the watcher, SIGUSR1, and
    /// `msg reload-config` all converge on one parse-validate-apply path.
    ConfigChanged,
    /// The global plugin registry settled on a new atomic generation.
    PluginsChanged,
    /// Foreground process identity changes discovered by the bounded agent worker.
    AgentProcesses(Vec<ProcessUpdate>),
}

#[derive(Clone)]
pub struct AutomationReplyTarget {
    client_id: u64,
    request_id: u64,
    writer: SharedWriter,
    cancel: crate::platform::ConnectionCancel,
}

impl AutomationReplyTarget {
    pub(crate) fn client_id(&self) -> u64 {
        self.client_id
    }
}

struct AutomationResponseJob {
    writer: SharedWriter,
    response: AutomationResponse,
}

struct PluginEventSubscription {
    client_id: u64,
    sender: mpsc::SyncSender<PluginStreamMessage>,
    cancel: crate::platform::ConnectionCancel,
}

enum PluginStreamMessage {
    Response(AutomationResponse),
    Event(PluginEventEnvelope),
}

#[derive(Default)]
struct PluginEventJournal {
    entries: VecDeque<(PluginEventEnvelope, usize)>,
    bytes: usize,
}

impl PluginEventJournal {
    fn push(&mut self, envelope: PluginEventEnvelope) {
        let size =
            serde_json::to_vec(&envelope).map_or(PLUGIN_EVENT_JOURNAL_BYTES + 1, |body| body.len());
        if size > PLUGIN_EVENT_JOURNAL_BYTES {
            self.entries.clear();
            self.bytes = 0;
            return;
        }
        self.entries.push_back((envelope, size));
        self.bytes = self.bytes.saturating_add(size);
        while self.entries.len() > PLUGIN_EVENT_JOURNAL || self.bytes > PLUGIN_EVENT_JOURNAL_BYTES {
            if let Some((_, removed)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed);
            }
        }
    }

    fn replay(&self, after: u64, latest: u64, capacity: usize) -> Vec<PluginEventEnvelope> {
        if capacity == 0 || after >= latest {
            return Vec::new();
        }
        if capacity == 1 {
            return vec![PluginEventEnvelope::Gap {
                from_sequence: after.saturating_add(1),
                to_sequence: latest,
            }];
        }
        let eligible = self
            .entries
            .iter()
            .filter(|(envelope, _)| {
                event_sequence(envelope).is_some_and(|sequence| sequence > after)
            })
            .map(|(envelope, _)| envelope)
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            return vec![PluginEventEnvelope::Gap {
                from_sequence: after.saturating_add(1),
                to_sequence: latest,
            }];
        }
        let first_available = event_sequence(eligible[0]).unwrap();
        let retention_gap = after.saturating_add(1) < first_available;
        let event_capacity = if retention_gap || eligible.len() > capacity {
            capacity.saturating_sub(1)
        } else {
            capacity
        };
        let start = eligible.len().saturating_sub(event_capacity);
        let first_sent = eligible
            .get(start)
            .and_then(|envelope| event_sequence(envelope));
        let mut replay = Vec::with_capacity(capacity);
        if let Some(first_sent) = first_sent
            && after.saturating_add(1) < first_sent
        {
            replay.push(PluginEventEnvelope::Gap {
                from_sequence: after.saturating_add(1),
                to_sequence: first_sent.saturating_sub(1),
            });
        }
        replay.extend(eligible.into_iter().skip(start).cloned());
        replay
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

type PendingPluginStateEvent = (
    serde_json::Value,
    Option<PaneId>,
    Option<crate::plugin_supervisor::PluginCause>,
);

#[derive(Clone)]
pub struct ActorHandle {
    pub sender: mpsc::SyncSender<ActorEvent>,
    pub shutdown: Arc<AtomicBool>,
}

/// What a reload actually did, so the caller learns which sections could not take effect now
/// rather than assuming the whole file applied.
struct ReloadReport {
    path: String,
    /// Sections whose live behavior changed before the report was returned.
    applied: Vec<String>,
    /// Sections that cannot change in a live session and were carried forward.
    ignored: Vec<String>,
    /// Sections that were adopted but only affect future panes, clients, or processes.
    deferred: Vec<String>,
    /// Sections that retained their previous live value because activation failed.
    failed: BTreeMap<String, String>,
}

impl ReloadReport {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "reloaded": true,
            "path": self.path,
            "applied": self.applied,
            "ignored": self.ignored,
            "deferred": self.deferred,
            "failed": self.failed,
        })
    }
}

struct AttachedClient {
    id: u64,
    writer: SharedWriter,
    display: DisplayMetrics,
    acknowledged_frame: u64,
    vivid: bool,
    kitty_graphics: bool,
    rendered_session_sequence: u64,
    frame_sequences: VecDeque<(u64, u64)>,
}

#[derive(Default)]
struct KittyTransferBuffer {
    transfers: HashMap<PaneId, Vec<u8>>,
    pending: VecDeque<Vec<u8>>,
    bytes: usize,
}

impl KittyTransferBuffer {
    fn clear(&mut self) {
        self.transfers.clear();
        self.pending.clear();
        self.bytes = 0;
    }

    fn push(&mut self, pane_id: PaneId, packet: Vec<u8>, starts: bool, more: bool) -> bool {
        self.push_bounded(pane_id, packet, starts, more, KITTY_GRAPHICS_SESSION_BYTES)
    }

    fn push_bounded(
        &mut self,
        pane_id: PaneId,
        packet: Vec<u8>,
        starts: bool,
        more: bool,
        maximum_bytes: usize,
    ) -> bool {
        if starts {
            if let Some(old) = self.transfers.remove(&pane_id) {
                self.bytes = self.bytes.saturating_sub(old.len());
            }
            if !self.reserve(packet.len(), maximum_bytes) {
                return false;
            }
            if more {
                self.transfers.insert(pane_id, packet);
            } else {
                self.pending.push_back(packet);
            }
            return true;
        }

        let Some(mut transfer) = self.transfers.remove(&pane_id) else {
            return false;
        };
        if !self.reserve(packet.len(), maximum_bytes) {
            self.bytes = self.bytes.saturating_sub(transfer.len());
            return false;
        }
        transfer.extend_from_slice(&packet);
        if more {
            self.transfers.insert(pane_id, transfer);
        } else {
            self.pending.push_back(transfer);
        }
        true
    }

    fn reserve(&mut self, bytes: usize, maximum_bytes: usize) -> bool {
        let Some(total) = self.bytes.checked_add(bytes) else {
            return false;
        };
        if total > maximum_bytes {
            return false;
        }
        self.bytes = total;
        true
    }

    fn drain_pending(&mut self) -> Vec<u8> {
        let capacity = self.pending.iter().map(Vec::len).sum();
        let mut prefix = Vec::with_capacity(capacity);
        while let Some(packet) = self.pending.pop_front() {
            self.bytes = self.bytes.saturating_sub(packet.len());
            prefix.extend_from_slice(&packet);
        }
        prefix
    }
}

fn kitty_query_response(capable: bool, image_id: u32) -> Vec<u8> {
    if capable {
        format!("\x1b_Gi={image_id};OK\x1b\\").into_bytes()
    } else {
        format!("\x1b_Gi={image_id};ENOTSUP\x1b\\").into_bytes()
    }
}

/// How one pane's process should be started.
///
/// A shell pane is `PaneSpawn::default()`. A command pane carries the shell command string, an
/// optional working directory, and whether the pane outlives the command so its output stays
/// readable.
#[derive(Debug, Clone)]
pub struct PaneSpawn {
    /// A shell command run with `-c`, not an argument vector: pipes and redirection are the
    /// caller's to write and the shell's to parse.
    pub command: Option<OsString>,
    /// Exact program and arguments. This path never invokes a shell.
    pub argv: Option<Vec<OsString>>,
    pub cwd: Option<PathBuf>,
    /// Whether the pane starts transparent, or `None` to take `[panes].transparent` from config.
    /// A saved layout carries the pane's own state here; an ordinary split does not.
    pub transparent: Option<bool>,
    pub hold_on_exit: bool,
    /// Extra environment applied before the fixed pane identity, so it can never shadow it.
    pub extra_env: Vec<(String, String)>,
    /// Core panes participate in all ordinary pane behavior. Plugin identity is attached here,
    /// before spawn, and is never reconstructed from the child argv.
    pub(crate) role: PaneRole,
    /// Whether to mint the pane-scoped Vivid capability and expose its authenticated endpoint.
    pub(crate) vivid_capability: bool,
}

impl Default for PaneSpawn {
    fn default() -> Self {
        Self {
            command: None,
            argv: None,
            cwd: None,
            transparent: None,
            hold_on_exit: false,
            extra_env: Vec::new(),
            role: PaneRole::Core,
            vivid_capability: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum PaneRole {
    #[default]
    Core,
    Plugin(PluginPaneIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginPaneIdentity {
    session_instance: String,
    plugin_id: String,
    plugin_instance: String,
    package_digest: String,
    entrypoint_id: String,
    title: String,
    accept_sync_input: bool,
}

struct Pane {
    id: PaneId,
    terminal: Terminal,
    input: PtyInput,
    control: PtyControl,
    child_pid: u32,
    /// The directory this pane's process was started in. A saved layout reopens the pane here;
    /// it deliberately does not follow the shell's later `cd`.
    spawn_cwd: PathBuf,
    agent: AgentRuntime,
    copy: Option<CopyState>,
    mouse_selection: Option<MouseSelection>,
    vivid_metrics: Option<(u16, u16, u16, u16)>,
    /// Whether the pane leaves its background to the outer terminal rather than painting one.
    ///
    /// A transparent pane's default-background cells stay SGR 49, so a translucent host window
    /// shows the desktop through them. An opaque pane substitutes `theme.pane_background` during
    /// composition and reads as a solid panel against transparent neighbours.
    transparent: bool,
    /// Whether the pane stays open after its process exits, keeping the output readable.
    #[allow(dead_code)]
    hold_on_exit: bool,
    /// Set once a held pane's process has exited, so the corpse is reported only once.
    #[allow(dead_code)]
    exit_status: Option<PtyExitStatus>,
    /// Whether this pane was last told it holds the host terminal's focus. A pane that has not
    /// enabled focus reporting still tracks it, so enabling the mode later reports no stale event.
    focus_reported: bool,
    last_input_warning: Option<Instant>,
    screen_sequence: u64,
    last_screen_change: Instant,
    screen_changes: VecDeque<ScreenChange>,
    role: PaneRole,
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
    MediaTrace {
        after_sequence: Option<u64>,
        limit: u16,
        filter: MediaTraceFilter,
    },
    Completion {
        level: AutomationCompletion,
        after_outer: u64,
        after_session: u64,
        result: serde_json::Value,
    },
    MediaTrack {
        identity: MediaTrackIdentity,
        condition: MediaTrackWaitCondition,
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

/// OSC 52 selection names vvmux maps onto its single copy buffer.
fn is_supported_clipboard_selection(selection: u8) -> bool {
    matches!(selection, b'c' | b'p' | b's')
}

fn clipboard_store_allowed(
    policy: crate::config::Osc52,
    focused: bool,
    attached: bool,
    selection: u8,
) -> bool {
    policy.allows_store() && focused && attached && is_supported_clipboard_selection(selection)
}

fn osc52_reply(selection: u8, bytes: &[u8], terminator: &str) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("\x1b]52;{};{encoded}{terminator}", selection as char).into_bytes()
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
    search: Option<CopySearch>,
    matches: Vec<SearchMatch>,
    current: Option<SearchMatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseSelectionMode {
    Character,
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseSelection {
    start: (isize, usize),
    end: (isize, usize),
    mode: MouseSelectionMode,
}

/// The OSC 8 link currently under the pointer.
///
/// Keyed by pane as well as link so hover stays owner-scoped: styling and clearing must never
/// reach a pane the pointer is not in, even if another pane happens to show the same URI.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HoveredLink {
    pane: PaneId,
    link: TerminalHyperlink,
}

#[derive(Debug, Clone, Copy)]
struct MouseSelectionDrag {
    pane: PaneId,
    content: Rect,
    display_offset: usize,
    start: (isize, usize),
    mode: MouseSelectionMode,
    moved: bool,
}

#[derive(Debug, Clone, Copy)]
struct MouseClickTracker {
    pane: PaneId,
    cell: (isize, usize),
    count: u8,
    last: Instant,
}

impl MouseClickTracker {
    fn next(previous: Option<Self>, pane: PaneId, cell: (isize, usize), now: Instant) -> Self {
        let count = previous
            .filter(|previous| {
                previous.pane == pane
                    && previous.cell == cell
                    && now.saturating_duration_since(previous.last) <= MOUSE_MULTI_CLICK_INTERVAL
            })
            .map_or(1, |previous| {
                if previous.count >= 3 {
                    1
                } else {
                    previous.count + 1
                }
            });
        Self {
            pane,
            cell,
            count,
            last: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopySearch {
    prompt: Option<String>,
    direction: SearchDirection,
    query: String,
}

struct Tab {
    id: u64,
    name: Option<String>,
    /// `None` when the last tiled pane closed and only floats remain.
    tree: Option<TiledNode>,
    floating: FloatingLayer,
    focused: PaneId,
    last_focused_tiled: Option<PaneId>,
    zoomed: Option<PaneId>,
    sync_input: bool,
}

#[derive(Debug, Clone, Copy)]
struct AgentNavigator {
    selected: Option<PaneId>,
    selected_index: usize,
    scroll: usize,
}

#[derive(Debug, Clone, Copy)]
struct TabNavigator {
    selected: Option<u64>,
    selected_index: usize,
    scroll: usize,
}

#[derive(Debug, Clone)]
struct TabRename {
    tab_id: u64,
    value: String,
    pending_utf8: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEditInput {
    Editing,
    Commit,
    Cancel,
}

#[derive(Debug, Clone, Copy)]
struct ClosePaneConfirmation {
    tab_id: u64,
    pane_id: PaneId,
}

/// The status-row save-layout prompt: first the target name, then an overwrite question when the
/// resolved file already exists.
#[derive(Debug, Clone)]
struct SaveLayoutPrompt {
    stage: SaveLayoutStage,
    pending_utf8: Vec<u8>,
}

#[derive(Debug, Clone)]
enum SaveLayoutStage {
    Editing { value: String },
    Confirm { path: PathBuf },
}

/// A short-lived status-row message, used to report what a save wrote or why it failed.
#[derive(Debug, Clone)]
struct StatusNotice {
    message: String,
    expires: Instant,
}

#[derive(Debug, Clone)]
struct TabNavigatorRow {
    tab_id: u64,
    display_index: usize,
    name: Option<String>,
    pane_count: usize,
    active: bool,
}

#[derive(Debug, Clone)]
struct AgentNavigatorRow {
    pane_id: PaneId,
    tab_index: usize,
    tab_label: String,
    title: String,
    agent: AgentSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentNavigatorKey {
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Activate,
    Close,
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

fn sync_targets(tab: &Tab, excluded: &dyn Fn(PaneId) -> bool) -> Vec<PaneId> {
    let mut targets = tab.tree.as_ref().map_or_else(Vec::new, TiledNode::pane_ids);
    targets.extend(tab.floating.pane_ids());
    targets.retain(|pane_id| !excluded(*pane_id));
    targets
}

fn pane_role_accepts_sync(role: &PaneRole) -> bool {
    match role {
        PaneRole::Core => true,
        PaneRole::Plugin(owner) => owner.accept_sync_input,
    }
}

fn caller_owns_plugin_pane(caller: &CallerContext, role: &PaneRole) -> bool {
    match (&caller.origin, role) {
        (
            CallerOrigin::Plugin {
                plugin_id,
                plugin_instance,
            },
            PaneRole::Plugin(owner),
        ) => {
            owner.session_instance == caller.session_instance
                && &owner.plugin_id == plugin_id
                && &owner.plugin_instance == plugin_instance
        }
        _ => false,
    }
}

fn plugin_pane_matches_generation(
    role: &PaneRole,
    session_instance: &str,
    plugin_id: &str,
    package_digest: &str,
) -> bool {
    matches!(
        role,
        PaneRole::Plugin(owner)
            if owner.session_instance == session_instance
                && owner.plugin_id == plugin_id
                && owner.package_digest == package_digest
    )
}

fn queue_input_targets(
    panes: &mut BTreeMap<PaneId, Pane>,
    targets: &[PaneId],
    bytes: &[u8],
) -> Vec<(PaneId, InputFailure)> {
    targets
        .iter()
        .filter_map(|pane_id| {
            panes
                .get_mut(pane_id)
                .and_then(|pane| queue_pane_input(pane, bytes))
                .map(|failure| (*pane_id, failure))
        })
        .collect()
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

struct PendingMediaProjection {
    sources: HashSet<BridgeSourceKey>,
    /// Previously presented retained sources for which a fresh outer track would be blank.
    retained_replay_candidates: HashSet<BridgeSourceKey>,
    retained_replays: HashSet<BridgeSourceKey>,
    gateway_revision: u64,
}

fn should_sync_media(
    force: bool,
    last: Option<MediaProjectionKey>,
    current: MediaProjectionKey,
) -> bool {
    force || last != Some(current)
}

fn bridge_apply_is_current(
    current_instance: Option<u64>,
    current_virtual_revision: u64,
    current_bridge_revision: u64,
    incoming_instance: u64,
    incoming_virtual_revision: u64,
    incoming_bridge_revision: u64,
) -> bool {
    incoming_virtual_revision >= current_virtual_revision
        && (current_instance != Some(incoming_instance)
            || incoming_bridge_revision >= current_bridge_revision)
}

fn next_outer_compatibility_revision(current: u64, incoming_bridge_revision: u64) -> u64 {
    current.saturating_add(1).max(incoming_bridge_revision)
}

struct SessionActor {
    name: String,
    /// Stable for this exact daemon lifetime, independent of whether plugins are enabled.
    session_instance: String,
    config: Config,
    /// The config file backing `config`, re-read on reload. `None` when none could be resolved.
    // Read by the config reload path.
    #[allow(dead_code)]
    config_path: Option<PathBuf>,
    sender: mpsc::SyncSender<ActorEvent>,
    agent_detector: DetectorHandle,
    panes: BTreeMap<PaneId, Pane>,
    tabs: Vec<Tab>,
    active_tab: usize,
    attached: Option<AttachedClient>,
    /// Whether the attached client's host terminal currently holds focus. Assumed true until the
    /// client reports otherwise, because a client attaches into a focused window.
    client_focused: bool,
    /// Last focused-pane keyboard and mouse coordinate modes sent to the attached host terminal.
    reported_input_mode: Option<(u8, bool)>,
    last_display: DisplayMetrics,
    next_pane_id: PaneId,
    next_tab_id: u64,
    copy_buffer: Vec<u8>,
    search_pattern: Option<(String, SearchPattern)>,
    frame_id: u64,
    last_screen: Option<ScreenBuffer>,
    #[cfg(windows)]
    outer_bracketed_paste: Option<bool>,
    force_full: bool,
    pending_render: bool,
    /// Validated Kitty transfers live only for the current physical attachment.
    kitty_transfers: KittyTransferBuffer,
    layout_revision: u64,
    last_media_projection: Option<MediaProjectionKey>,
    media_projection_revision: u64,
    /// Source sets submitted to the foreground bridge but not yet acknowledged as physically
    /// applied. Timed producer workers remain parked until the matching revision is consumed.
    pending_media_projections: BTreeMap<u64, PendingMediaProjection>,
    outer_virtual_revision: u64,
    bridge_instance_id: Option<u64>,
    bridge_local_revision: u64,
    outer_projection_revision: u64,
    outer_apply_sequence: u64,
    outer_attachment_generations: HashMap<BridgeSourceKey, u64>,
    /// Recreated outer image/raster tracks whose retained body must cross VVMX once.
    retained_replay_requests: HashSet<BridgeSourceKey>,
    /// Forced retained replays sent but not yet confirmed by the outer presenter.
    retained_replay_inflight: HashSet<BridgeSourceKey>,
    traced_projected_sources: HashSet<BridgeSourceKey>,
    traced_recovery_deliveries: HashMap<u64, (BridgeSourceKey, Option<u64>, u32, i64)>,
    media_trace: MediaTraceJournal,
    fragment_assignments: HashMap<(u64, u64), FragmentMap>,
    last_projection_warning: Option<MediaProjectionKey>,
    pointer_drag: Option<PointerDrag>,
    mouse_selection_drag: Option<MouseSelectionDrag>,
    mouse_click_tracker: Option<MouseClickTracker>,
    hovered_link: Option<HoveredLink>,
    /// When a link was last handed to the host opener, so a double click opens one window.
    last_link_open: Option<Instant>,
    float_modal: Option<FloatModal>,
    agent_navigator: Option<AgentNavigator>,
    tab_navigator: Option<TabNavigator>,
    tab_rename: Option<TabRename>,
    close_pane_confirmation: Option<ClosePaneConfirmation>,
    save_layout_prompt: Option<SaveLayoutPrompt>,
    status_notice: Option<StatusNotice>,
    agent_catalog: Arc<crate::agent::AgentCatalog>,
    agent_catalog_generation: u64,
    next_float_mode: u64,
    session_sequence: u64,
    /// Direct count of general-queue actor wakeups for fairness/compatibility diagnostics.
    actor_wakeups: u64,
    response_sender: mpsc::SyncSender<AutomationResponseJob>,
    automation_inflight: HashMap<u64, HashSet<u64>>,
    pending_actor_work: HashSet<(u64, u64)>,
    plugin_supervisor: Option<crate::plugin_supervisor::PluginSupervisor>,
    plugin_event_sequence: u64,
    plugin_event_journal: PluginEventJournal,
    pending_plugin_state_events: BTreeMap<(String, String), PendingPluginStateEvent>,
    plugin_event_subscriptions: HashMap<String, PluginEventSubscription>,
    next_plugin_subscription: u64,
    active_plugin_cause: Option<crate::plugin_supervisor::PluginCause>,
    pending_pane_plugin_causes: HashMap<PaneId, crate::plugin_supervisor::PluginCause>,
    last_plugin_focus: Option<(bool, Option<u64>, Option<PaneId>)>,
    last_plugin_media_revision: u64,
    automation_waiters: Vec<AutomationWaiter>,
    exit_tombstones: VecDeque<ExitTombstone>,
    shutdown: Arc<AtomicBool>,
    vivid: VirtualVivid,
    media_projection_pending: Arc<AtomicBool>,
    /// Coalesces config-change wakes from the watcher thread, as `media_projection_pending` does
    /// for media: one queued wake the actor has not yet observed makes another redundant.
    config_reload_pending: Arc<AtomicBool>,
    /// Coalesces global plugin-registry watcher wakes until the actor submits one reload.
    plugin_reload_pending: Arc<AtomicBool>,
    /// Stops only the plugin registry watcher when the live global kill switch is disabled.
    plugin_watch_shutdown: Option<Arc<AtomicBool>>,
    /// Latest foreground-bridge counter report. Diagnostic only; retained across a detach so
    /// `inspect-media` still describes the last live bridge.
    bridge_metrics: crate::metrics::BridgeMetrics,
    /// Counters for the attached client's VVMX connection, retained across a detach for the same
    /// reason. Replaced when a new client attaches.
    client_ipc: Option<Arc<crate::metrics::IpcCounters>>,
}

/// Everything the session server hands to a new actor.
///
/// A struct rather than positional arguments because the later phases add a startup layout and a
/// watched config path here; growing a struct keeps `start`'s signature stable.
pub struct SessionOptions {
    pub name: String,
    pub config: Config,
    /// The config file the running config came from, watched for live reload. `None` when no
    /// config file could be resolved at all.
    pub config_path: Option<PathBuf>,
    pub vivid_endpoint: VirtualPresenterEndpoint,
    pub layout: Option<LayoutPlan>,
}

pub fn start(options: SessionOptions) -> io::Result<ActorHandle> {
    let SessionOptions {
        name,
        config,
        config_path,
        vivid_endpoint,
        layout,
    } = options;
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE);
    let (media_sender, media_receiver) = mpsc::sync_channel(MEDIA_EVENT_QUEUE);
    let shutdown = Arc::new(AtomicBool::new(false));
    let vivid =
        VirtualVivid::start_with_events(vivid_endpoint, config.media.clone(), Some(media_sender))?;
    let media_projection_pending = Arc::new(AtomicBool::new(false));
    {
        // Never block ingest on the actor's general queue. The atomic dirty bit makes a lost
        // coalescible wake harmless even for immutable images, which advance retained projection
        // state without placing a payload on the dedicated media queue.
        let wakeup = sender.clone();
        let pending = media_projection_pending.clone();
        vivid.set_media_wakeup(Arc::new(move || {
            request_media_service(&wakeup, &pending);
        }));
    }
    let config_reload_pending = Arc::new(AtomicBool::new(false));
    if let Some(path) = config_path.clone() {
        // Watch the resolved path even when nothing is there yet: creating the file later is an
        // ordinary way to start configuring a running session.
        crate::config_watch::spawn(
            path,
            sender.clone(),
            shutdown.clone(),
            config_reload_pending.clone(),
        )?;
    }
    let plugin_reload_pending = Arc::new(AtomicBool::new(false));
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
    let detector_sender = sender.clone();
    let agent_detector = crate::agent::start_detector(move |updates| {
        let _ = detector_sender.send(ActorEvent::AgentProcesses(updates));
    })?;
    let session_instance = crate::plugin::random_id()?;
    let (plugin_supervisor, plugin_watch_shutdown) = if config.plugins.enabled {
        let supervisor = crate::plugin_supervisor::PluginSupervisor::start(
            name.clone(),
            session_instance.clone(),
            sender.clone(),
        )?;
        let watcher_shutdown = Arc::new(AtomicBool::new(false));
        if let Err(error) = crate::config_watch::spawn_plugin_registry(
            crate::plugin::registry_path()?,
            sender.clone(),
            watcher_shutdown.clone(),
            plugin_reload_pending.clone(),
        ) {
            supervisor.shutdown();
            return Err(error);
        }
        (Some(supervisor), Some(watcher_shutdown))
    } else {
        (None, None)
    };
    let mut actor = SessionActor {
        name,
        session_instance,
        config,
        config_path,
        sender: sender.clone(),
        agent_detector,
        panes: BTreeMap::new(),
        tabs: Vec::new(),
        active_tab: 0,
        attached: None,
        client_focused: true,
        reported_input_mode: None,
        last_display,
        next_pane_id: 1,
        next_tab_id: 1,
        copy_buffer: Vec::new(),
        search_pattern: None,
        frame_id: 0,
        last_screen: None,
        #[cfg(windows)]
        outer_bracketed_paste: None,
        force_full: true,
        pending_render: false,
        kitty_transfers: KittyTransferBuffer::default(),
        layout_revision: 0,
        last_media_projection: None,
        media_projection_revision: 0,
        pending_media_projections: BTreeMap::new(),
        outer_virtual_revision: 0,
        bridge_instance_id: None,
        bridge_local_revision: 0,
        outer_projection_revision: 0,
        outer_apply_sequence: 0,
        outer_attachment_generations: HashMap::new(),
        retained_replay_requests: HashSet::new(),
        retained_replay_inflight: HashSet::new(),
        traced_projected_sources: HashSet::new(),
        traced_recovery_deliveries: HashMap::new(),
        media_trace: MediaTraceJournal::default(),
        fragment_assignments: HashMap::new(),
        last_projection_warning: None,
        pointer_drag: None,
        mouse_selection_drag: None,
        hovered_link: None,
        last_link_open: None,
        mouse_click_tracker: None,
        float_modal: None,
        agent_navigator: None,
        tab_navigator: None,
        tab_rename: None,
        close_pane_confirmation: None,
        save_layout_prompt: None,
        status_notice: None,
        agent_catalog: Arc::new(crate::agent::AgentCatalog::default()),
        agent_catalog_generation: 0,
        next_float_mode: 0,
        session_sequence: 1,
        actor_wakeups: 0,
        response_sender,
        automation_inflight: HashMap::new(),
        pending_actor_work: HashSet::new(),
        plugin_supervisor,
        plugin_event_sequence: 0,
        plugin_event_journal: PluginEventJournal::default(),
        pending_plugin_state_events: BTreeMap::new(),
        plugin_event_subscriptions: HashMap::new(),
        next_plugin_subscription: 1,
        active_plugin_cause: None,
        pending_pane_plugin_causes: HashMap::new(),
        last_plugin_focus: None,
        last_plugin_media_revision: 0,
        automation_waiters: Vec::new(),
        exit_tombstones: VecDeque::new(),
        shutdown: shutdown.clone(),
        vivid,
        media_projection_pending,
        config_reload_pending,
        plugin_reload_pending,
        plugin_watch_shutdown,
        bridge_metrics: crate::metrics::BridgeMetrics::default(),
        client_ipc: None,
    };
    match layout {
        Some(layout) => actor.apply_layout_plan(layout)?,
        None => actor.new_tab()?,
    }
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
        let mut render_at = Instant::now();
        loop {
            // Re-read every iteration: a config reload must be able to retune the render cadence
            // without restarting the session.
            let interval = Duration::from_millis(self.config.general.render_interval_ms);
            let mut timeout = if self.pending_render {
                render_at.saturating_duration_since(Instant::now())
            } else {
                Duration::from_secs(1)
            };
            timeout = timeout.min(self.next_automation_deadline());
            timeout = timeout.min(self.next_agent_evaluation_delay());
            timeout = timeout.min(self.next_notice_deadline());
            timeout = timeout.min(self.next_sync_flush_delay());
            // Give ready media low-latency service, but force a general-queue turn after a bounded
            // batch. A bounded channel is not a bounded drain when its producer can refill it.
            if self.drain_media(&media_receiver) {
                timeout = Duration::ZERO;
            }
            match receiver.recv_timeout(timeout) {
                Ok(event) => {
                    self.actor_wakeups = self.actor_wakeups.saturating_add(1);
                    if self.handle_event(event).is_err() {
                        self.force_full = true;
                    }
                    self.flush_expired_sync_updates();
                    self.drain_media(&media_receiver);
                    self.sync_pending_media_projection();
                    self.expire_status_notice();
                    if self.pending_render && render_at <= Instant::now() {
                        self.render();
                        render_at = Instant::now() + interval;
                    } else if self.pending_render && render_at < Instant::now() + interval {
                        // Keep the already scheduled coalescing boundary.
                    } else if self.pending_render {
                        render_at = Instant::now() + interval;
                    }
                    self.check_automation_waiters();
                    self.evaluate_agent_states();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.flush_expired_sync_updates();
                    self.drain_media(&media_receiver);
                    self.sync_pending_media_projection();
                    self.expire_status_notice();
                    if self.pending_render {
                        self.render();
                        render_at = Instant::now() + interval;
                    }
                    self.sync_media(false);
                    self.check_automation_waiters();
                    self.evaluate_agent_states();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.tabs.is_empty() || self.shutdown.load(Ordering::Acquire) {
                break;
            }
        }
        self.terminate_children();
        if let Some(stop) = self.plugin_watch_shutdown.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(supervisor) = self.plugin_supervisor.take() {
            supervisor.shutdown();
        }
        self.shutdown.store(true, Ordering::Release);
    }

    /// Forward one bounded batch of currently queued media events.
    ///
    /// Returns true when the batch limit was reached and more media may remain. The caller then
    /// polls the general actor queue without waiting, preserving detach, input, and credit
    /// liveness under a continuously refilled video queue.
    fn drain_media(&mut self, media_receiver: &mpsc::Receiver<crate::media::MediaEvent>) -> bool {
        drain_ready_batch(media_receiver, MEDIA_EVENTS_PER_TURN, |event| {
            self.forward_media(event);
        })
    }

    fn sync_pending_media_projection(&mut self) {
        if self.media_projection_pending.swap(false, Ordering::AcqRel) {
            self.sync_media(false);
        }
    }

    fn forward_media(&mut self, event: crate::media::MediaEvent) {
        if !self
            .vivid
            .bridge_delivery_is_pending(event.delivery_id, event.source)
        {
            // Its source crossed a hidden/detached falling edge after admission. The gateway has
            // already returned its allowance; forwarding this stale event after re-apply would
            // splice the old epoch into the replacement decoder.
            return;
        }
        if let Some((epoch, pts_us)) = event.recovered_keyframe {
            self.traced_recovery_deliveries.insert(
                event.delivery_id,
                (
                    bridge_key(event.source),
                    self.bridge_instance_id,
                    epoch,
                    pts_us,
                ),
            );
            self.record_media_trace(
                Some(bridge_key(event.source)),
                self.bridge_instance_id,
                None,
                MediaTraceKind::KeyframeRecovered { epoch, pts_us },
            );
        }
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
            self.record_delivery_result(event.delivery_id, false);
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
                if let Some(supervisor) = &self.plugin_supervisor {
                    supervisor.cancel_client(id);
                }
                self.automation_inflight.remove(&id);
                self.pending_actor_work
                    .retain(|(client_id, _)| *client_id != id);
                self.automation_waiters
                    .retain(|waiter| waiter.reply.client_id != id);
                self.plugin_event_subscriptions
                    .retain(|_, subscription| subscription.client_id != id);
                if self.attached.as_ref().is_some_and(|client| client.id == id) {
                    self.record_media_trace(
                        None,
                        self.bridge_instance_id,
                        None,
                        MediaTraceKind::BridgeClientDetached,
                    );
                    self.cancel_pointer_drag(true);
                    self.invalidate_mouse_selection_state();
                    self.attached = None;
                    self.clear_kitty_graphics();
                    self.reported_input_mode = None;
                    self.bridge_instance_id = None;
                    self.bridge_local_revision = 0;
                    self.pending_media_projections.clear();
                    self.retained_replay_requests.clear();
                    self.retained_replay_inflight.clear();
                    self.traced_recovery_deliveries.clear();
                    self.record_projection_sources(&HashSet::new(), self.vivid.revision());
                    self.last_screen = None;
                    #[cfg(windows)]
                    {
                        self.outer_bracketed_paste = None;
                    }
                    self.force_full = true;
                    self.end_float_mode(true);
                    self.clear_transient_ui();
                    self.vivid.deactivate_bridge();
                }
            }
            ActorEvent::PtyOutput(pane_id, bytes) => {
                self.drive_pane_terminal(pane_id, |terminal| terminal.feed(&bytes));
            }
            ActorEvent::PtyExit(pane_id, status) => {
                self.publish_plugin_event(
                    "pane.exited",
                    serde_json::json!({
                        "pane_id": pane_id,
                        "status": status.map(|status| status.code),
                    }),
                    Some(pane_id),
                    None,
                );
                // Waiters and the tombstone are recorded whichever way the pane goes: `wait exit`
                // must resolve for a held pane exactly as it does for one that closes.
                self.complete_exit_waiters(pane_id, status);
                self.exit_tombstones
                    .push_back(ExitTombstone { pane_id, status });
                while self.exit_tombstones.len() > EXIT_TOMBSTONES {
                    self.exit_tombstones.pop_front();
                }
                let held = self
                    .panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.hold_on_exit && pane.exit_status.is_none());
                if held {
                    let plugin = self.panes.get(&pane_id).and_then(|pane| match &pane.role {
                        PaneRole::Plugin(owner) => Some(owner.clone()),
                        PaneRole::Core => None,
                    });
                    if plugin.is_some() {
                        // The held terminal is only a diagnostic surface after exit. Its process,
                        // media authority, and exact runtime identity are already dead.
                        self.vivid.revoke_pane(pane_id);
                    }
                    let note = plugin.map_or_else(
                        || format!("\r\n[{}]\r\n", describe_exit(status)),
                        |owner| {
                            format!(
                                "\r\n[plugin {}/{} {}]\r\n",
                                owner.plugin_id,
                                owner.entrypoint_id,
                                describe_exit(status)
                            )
                        },
                    );
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.exit_status = status;
                        pane.agent.observe_process(None, None);
                        pane.terminal.clear_agent_osc();
                        pane.terminal.feed(note.as_bytes());
                    }
                    self.mark_pane_screen_change(pane_id, None);
                    self.schedule_render();
                } else {
                    self.close_pane(pane_id);
                }
            }
            ActorEvent::AutomationInputComplete {
                reply,
                result,
                pane_id,
                byte_count,
                report,
            } => match result {
                Ok(()) => {
                    self.complete_pending_actor_work(&reply);
                    let result = if report {
                        serde_json::json!({
                            "pane_id": pane_id,
                            "encoded_byte_count": byte_count,
                            "input_sequence": reply.request_id,
                            "pty_write_completed": true,
                            "application_consumption_observed": false,
                        })
                    } else {
                        serde_json::Value::Null
                    };
                    self.reply_automation(reply, result);
                }
                Err(message) => {
                    self.complete_pending_actor_work(&reply);
                    self.reply_automation_error(reply, AutomationError::new("pty_closed", message));
                }
            },
            ActorEvent::PluginComplete { reply, result } => {
                self.complete_pending_actor_work(&reply);
                match result {
                    Ok(value) => self.reply_automation(reply, value),
                    Err(error) => self.reply_automation_error(reply, error),
                }
            }
            ActorEvent::PluginNotice { reference, result } => {
                if let Err(error) = result {
                    self.status(&format!("plugin action {reference} failed: {error}"));
                }
            }
            ActorEvent::PluginHostCall {
                scope,
                cause,
                call,
                reply,
            } => {
                let previous_cause = std::mem::replace(&mut self.active_plugin_cause, cause);
                let result = self.handle_plugin_host_call(&scope, &call.method, call.params);
                self.active_plugin_cause = previous_cause;
                let _ = reply.try_send(result);
            }
            ActorEvent::PluginPaneOpen { launch, reply } => {
                let caller = CallerContext {
                    origin: CallerOrigin::Plugin {
                        plugin_id: launch.scope.plugin_id.clone(),
                        plugin_instance: launch.scope.plugin_instance.clone(),
                    },
                    session_instance: launch.scope.session_instance.clone(),
                    focused_fallback: false,
                    capabilities: launch.scope.permissions.iter().copied().collect(),
                };
                let result = self.execute_session_command(
                    &caller,
                    SessionCommand::OpenPluginPane {
                        launch: Box::new(launch),
                    },
                );
                self.complete_pending_actor_work(&reply);
                match result {
                    Ok(value) => self.reply_automation(reply, value),
                    Err(error) => self.reply_automation_error(reply, error),
                }
            }
            ActorEvent::PluginPanesClose {
                plugin_id,
                package_digest,
            } => self.close_plugin_panes(&plugin_id, &package_digest),
            ActorEvent::PluginReloaded { result } => match result {
                Ok(report)
                    if report["failed"]
                        .as_object()
                        .is_some_and(|failed| !failed.is_empty()) =>
                {
                    self.status(
                        "plugin registry reload kept invalid entries on their prior generation",
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    self.status(&format!("plugin registry reload failed: {}", error.message))
                }
            },
            ActorEvent::AgentCatalogApplied {
                generation,
                catalog,
            } => {
                if self.config.plugins.enabled && generation >= self.agent_catalog_generation {
                    self.agent_catalog_generation = generation;
                    self.agent_catalog = catalog.clone();
                    self.agent_detector.replace_catalog(catalog);
                    for pane in self.panes.values_mut() {
                        if pane.agent.reconcile_catalog(&self.agent_catalog) {
                            pane.terminal.clear_agent_osc();
                        }
                    }
                    self.evaluate_agent_states();
                }
            }
            ActorEvent::PluginLifecycle {
                name,
                payload,
                context,
            } => {
                self.publish_plugin_event(&name, payload, None, context);
            }
            // The payload or retained-projection dirty bit is drained by the run loop.
            ActorEvent::MediaReady => {}
            ActorEvent::ConfigChanged => {
                // Clear the coalescing bit before reading, so an edit landing during the reload
                // queues a fresh wake instead of being folded into the one being handled.
                self.config_reload_pending.store(false, Ordering::Release);
                if let Err(error) = self.reload_config() {
                    self.status(&format!("config reload failed: {error}"));
                }
            }
            ActorEvent::PluginsChanged => {
                self.plugin_reload_pending.store(false, Ordering::Release);
                if let Some(supervisor) = &self.plugin_supervisor
                    && let Err(error) = supervisor.reload_notice()
                {
                    self.status(&format!("plugin registry reload failed: {}", error.message));
                }
            }
            ActorEvent::AgentProcesses(updates) => {
                for update in updates {
                    if let Some(pane) = self.panes.get_mut(&update.pane_id)
                        && pane
                            .agent
                            .observe_process(update.process_group, update.identity)
                    {
                        pane.terminal.clear_agent_osc();
                    }
                }
                self.evaluate_agent_states();
            }
        }
        // Attachment, pane focus, tab switching, pane teardown, and a program enabling the mode
        // all move focus, so reconcile once here rather than at each of those call sites.
        self.sync_pane_focus();
        self.sync_client_input_mode();
        Ok(())
    }

    fn clear_kitty_graphics(&mut self) {
        self.kitty_transfers.clear();
    }

    /// Run terminal events produced for one pane through the full observation path: PTY replies,
    /// title and bell, media anchors, mouse-selection adjustment, the semantic-change sequence
    /// automation and plugins observe, and render scheduling.
    ///
    /// Live PTY output and a flushed synchronized update both come through here, so a flush is
    /// observed exactly like ordinary output rather than mutating the grid behind everyone's back.
    fn drive_pane_terminal<F>(&mut self, pane_id: PaneId, produce: F)
    where
        F: FnOnce(&mut Terminal) -> Vec<TerminalEvent>,
    {
        let focused = self.active_tab().is_some_and(|tab| tab.focused == pane_id);
        let mut title = None;
        let mut bell = false;
        let mut input_warning = false;
        let mut input_closed = false;
        let mut changed_screen_sequence = None;
        let mut kitty_commands = Vec::new();
        let mut clipboard_store = None;
        let mut clipboard_load = None;
        // Selection-relevant output, folded into the pane's mouse selection once the pane borrow
        // below has ended.
        let mut selection_events = Vec::new();
        let mut screen_switched = false;
        let mut history_len = 0;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let old_cells = pane.terminal.cells().to_vec();
            let old_cursor = pane.terminal.cursor();
            let old_modes = pane.terminal.modes();
            let old_screen = pane.terminal.alternate_screen();
            let events = produce(&mut pane.terminal);
            selection_events = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        TerminalEvent::GridScroll { .. } | TerminalEvent::Clear
                    )
                })
                .cloned()
                .collect();
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
                    TerminalEvent::KittyGraphics(command) => kitty_commands.push(command),
                    TerminalEvent::GridScroll { lines, .. } => {
                        self.vivid.scroll_anchors(pane_id, lines);
                    }
                    TerminalEvent::Clear => self.vivid.clear_anchors(pane_id),
                    // Deferred: honoring these needs the session's policy and focus state, which
                    // cannot be read while a pane is mutably borrowed.
                    TerminalEvent::ClipboardStore { selection, text } => {
                        clipboard_store = Some((selection, text));
                    }
                    TerminalEvent::ClipboardLoad {
                        selection,
                        terminator,
                    } => {
                        clipboard_load = Some((selection, terminator));
                    }
                    _ => {}
                }
            }
            screen_switched = old_screen != pane.terminal.alternate_screen();
            history_len = pane.terminal.history_len();
            let semantic_changed = old_cells != pane.terminal.cells()
                || old_cursor != pane.terminal.cursor()
                || old_modes != pane.terminal.modes()
                || screen_switched;
            if semantic_changed {
                let rows = changed_rows(&old_cells, pane.terminal.cells());
                let rows = (!screen_switched).then_some(rows);
                pane.screen_sequence = pane.screen_sequence.wrapping_add(1);
                pane.last_screen_change = Instant::now();
                pane.screen_changes.push_back(ScreenChange {
                    sequence: pane.screen_sequence,
                    rows,
                });
                while pane.screen_changes.len() > SCREEN_CHANGE_HISTORY {
                    pane.screen_changes.pop_front();
                }
                changed_screen_sequence = Some(pane.screen_sequence);
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
        self.adjust_mouse_selection_after_pane_output(
            pane_id,
            &selection_events,
            screen_switched,
            history_len,
        );
        for command in kitty_commands {
            self.handle_kitty_graphics(pane_id, command);
        }
        if let Some((selection, text)) = clipboard_store {
            self.handle_clipboard_store(focused, selection, text);
        }
        if let Some((selection, terminator)) = clipboard_load {
            self.handle_clipboard_load(pane_id, focused, selection, &terminator);
        }
        if let Some(screen_sequence) = changed_screen_sequence {
            let pending_cause = self.pending_pane_plugin_causes.remove(&pane_id);
            let previous_cause = std::mem::replace(&mut self.active_plugin_cause, pending_cause);
            self.queue_plugin_state_event(
                "pane.screen_changed",
                pane_id.to_string(),
                serde_json::json!({
                    "pane_id": pane_id,
                    "screen_sequence": screen_sequence,
                }),
                Some(pane_id),
            );
            self.active_plugin_cause = previous_cause;
        }
        if input_warning {
            self.status(&format!("pane {pane_id} input queue is unavailable"));
        }
        if input_closed {
            self.close_pane(pane_id);
        }
    }

    /// How long until the earliest pane's buffered synchronized update must be applied.
    fn next_sync_flush_delay(&self) -> Duration {
        let now = Instant::now();
        self.panes
            .values()
            .filter_map(|pane| pane.terminal.sync_flush_deadline())
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::MAX)
    }

    /// Apply synchronized updates whose deadline has passed.
    ///
    /// vte buffers everything between BSU and ESU but never enforces the deadline it arms, so a
    /// pane that opens DECSET 2026 and then stalls would look frozen until it produced two more
    /// megabytes of output.
    fn flush_expired_sync_updates(&mut self) {
        let now = Instant::now();
        let expired: Vec<PaneId> = self
            .panes
            .iter()
            .filter(|(_, pane)| {
                pane.terminal
                    .sync_flush_deadline()
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|(pane_id, _)| *pane_id)
            .collect();
        for pane_id in expired {
            self.drive_pane_terminal(pane_id, |terminal| terminal.flush_synchronized_update());
        }
    }

    fn handle_kitty_graphics(&mut self, pane_id: PaneId, command: KittyGraphicsCommand) {
        let capable = self
            .attached
            .as_ref()
            .is_some_and(|client| client.kitty_graphics);
        match command {
            KittyGraphicsCommand::Query { image_id } => {
                let response = kitty_query_response(capable, image_id);
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    let _ = queue_pane_input(pane, &response);
                }
            }
            KittyGraphicsCommand::Packet {
                bytes,
                starts_transfer,
                more,
            } => {
                if !capable {
                    return;
                }
                self.kitty_transfers
                    .push(pane_id, bytes, starts_transfer, more);
            }
        }
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
                kitty_graphics,
            } => {
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
                self.cancel_pointer_drag(true);
                self.invalidate_mouse_selection_state();
                self.end_float_mode(true);
                self.clear_transient_ui();
                // Even a clean client replacement owns a different physical presenter and fresh
                // decoder/audio devices. Park timed ingress until that client applies its first
                // authoritative projection.
                self.vivid.deactivate_bridge();
                self.pending_media_projections.clear();
                self.clear_kitty_graphics();
                let display = normalized_display(display, self.config.general.status_visible);
                self.last_display = display;
                self.client_ipc = Some(
                    writer
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .counters(),
                );
                // A client attaches into a focused window, and the previous client's last
                // reported blur says nothing about this one's.
                self.client_focused = true;
                self.bridge_metrics = crate::metrics::BridgeMetrics::default();
                self.bridge_instance_id = None;
                self.bridge_local_revision = 0;
                self.outer_attachment_generations.clear();
                self.retained_replay_requests.clear();
                self.retained_replay_inflight.clear();
                self.traced_recovery_deliveries.clear();
                self.attached = Some(AttachedClient {
                    id,
                    writer: writer.clone(),
                    display,
                    // This client never received the session's historical frames. Start its
                    // flow-control window at the current frame so the forced full repaint below
                    // becomes its first outstanding frame instead of being suppressed as stale.
                    acknowledged_frame: self.frame_id,
                    vivid,
                    kitty_graphics,
                    rendered_session_sequence: 0,
                    frame_sequences: VecDeque::new(),
                });
                self.reported_input_mode = None;
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
                self.record_media_trace(
                    None,
                    None,
                    None,
                    MediaTraceKind::BridgeClientAttached { vivid },
                );
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
                    self.mouse(mouse, false);
                }
            }
            ClientMessage::PixelMouse(mouse) => {
                if self.client_is(id) {
                    self.mouse(mouse, true);
                }
            }
            ClientMessage::Focus(focused) => {
                if self.client_is(id) {
                    self.client_focused = focused;
                    if !focused {
                        // There is no pointer-leave report, so blur is the only signal that the
                        // pointer is gone. Without this a link stays marked as hovered while the
                        // user works in another window.
                        self.set_hovered_link(None);
                    }
                }
            }
            ClientMessage::Resize(display) => {
                if self.client_is(id) {
                    let display = normalized_display(display, self.config.general.status_visible);
                    // A client may re-send its display without changing it: browser presenters
                    // report every dimension probe, not only real resizes. Relaying a phantom
                    // resize would bump `layout_revision`, so `should_sync_media` would rebuild
                    // the outer Vivid session on each one and destroy media that is still being
                    // projected. Only a display that actually changed is a resize.
                    let changed = is_display_change(
                        self.attached.as_ref().map(|client| client.display),
                        display,
                    );
                    if changed {
                        self.cancel_pointer_drag(true);
                        self.invalidate_mouse_selection_state();
                        self.end_float_mode(true);
                        self.clear_transient_ui();
                        if let Some(client) = &mut self.attached {
                            client.display = display;
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
                        self.record_media_trace(
                            Some(request.source),
                            self.bridge_instance_id,
                            None,
                            MediaTraceKind::KeyframeRequest {
                                stage: MediaKeyframeStage::ProducerQueued,
                                minimum_epoch: request.minimum_epoch,
                                reason: request.reason,
                            },
                        );
                        let outcome = self.vivid.request_keyframe(
                            request.source,
                            request.minimum_epoch,
                            request.reason,
                        );
                        self.record_media_trace(
                            Some(request.source),
                            self.bridge_instance_id,
                            None,
                            MediaTraceKind::KeyframeRequest {
                                stage: match outcome {
                                    crate::media::KeyframeRequestOutcome::Forwarded => {
                                        MediaKeyframeStage::ProducerWritten
                                    }
                                    crate::media::KeyframeRequestOutcome::Damped => {
                                        MediaKeyframeStage::ProducerDamped
                                    }
                                    crate::media::KeyframeRequestOutcome::Ignored => {
                                        MediaKeyframeStage::ProducerIgnored
                                    }
                                },
                                minimum_epoch: request.minimum_epoch,
                                reason: request.reason,
                            },
                        );
                    }
                }
            }
            ClientMessage::BridgeNeedFullFrames(sources) => {
                if self.client_is(id) {
                    self.vivid.request_full_frames(&sources, 1);
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
                    self.record_delivery_result(delivery_id, delivered);
                    let resync = self.vivid.complete_bridge_delivery(delivery_id, delivered);
                    if resync {
                        self.last_media_projection = None;
                        self.sync_media(true);
                    }
                }
            }
            ClientMessage::BridgeMediaReleased { delivery_id } => {
                if self.client_is(id) {
                    self.traced_recovery_deliveries.remove(&delivery_id);
                    self.vivid.release_bridge_delivery(delivery_id);
                }
            }
            ClientMessage::BridgeRetainedHydrated { source } => {
                if self.client_is(id) {
                    self.retained_replay_requests.remove(&source);
                    self.retained_replay_inflight.remove(&source);
                    self.vivid.complete_retained_hydration(source);
                }
            }
            ClientMessage::BridgeSnapshotRetry {
                reset_outer_session,
            } => {
                if self.client_is(id) {
                    self.record_media_trace(
                        None,
                        self.bridge_instance_id,
                        None,
                        MediaTraceKind::SnapshotRetry,
                    );
                    if reset_outer_session {
                        // Fragment and attachment identities are scoped to the outer session.
                        // Source-scoped recovery reuses that session and must preserve unrelated
                        // mappings; only a confirmed replacement invalidates all of them.
                        self.fragment_assignments.clear();
                        self.outer_attachment_generations.clear();
                        self.retained_replay_requests.clear();
                        self.retained_replay_inflight.clear();
                        self.last_media_projection = None;
                    } else {
                        self.retained_replay_requests
                            .extend(self.retained_replay_inflight.iter().copied());
                        self.last_media_projection = None;
                    }
                    self.sync_media(true);
                }
            }
            ClientMessage::BridgeApplied {
                bridge_instance_id,
                virtual_revision,
                outer_revision,
                outer_attachment_generations,
                recreated_retained_sources,
            } => {
                let instance_changed = self.bridge_instance_id != Some(bridge_instance_id);
                if self.client_is(id)
                    && bridge_apply_is_current(
                        self.bridge_instance_id,
                        self.outer_virtual_revision,
                        self.bridge_local_revision,
                        bridge_instance_id,
                        virtual_revision,
                        outer_revision,
                    )
                {
                    if instance_changed {
                        self.bridge_local_revision = 0;
                        self.outer_attachment_generations.clear();
                        self.retained_replay_requests.clear();
                        self.retained_replay_inflight.clear();
                    }
                    self.bridge_instance_id = Some(bridge_instance_id);
                    self.outer_virtual_revision = virtual_revision;
                    self.bridge_local_revision = outer_revision;
                    self.outer_apply_sequence = self.outer_apply_sequence.saturating_add(1);
                    self.outer_projection_revision = next_outer_compatibility_revision(
                        self.outer_projection_revision,
                        outer_revision,
                    );
                    let attachment_count =
                        u16::try_from(outer_attachment_generations.len()).unwrap_or(u16::MAX);
                    self.outer_attachment_generations =
                        outer_attachment_generations.into_iter().collect();
                    let resident_sources = self
                        .outer_attachment_generations
                        .keys()
                        .copied()
                        .collect::<HashSet<_>>();
                    self.retained_replay_requests
                        .retain(|source| resident_sources.contains(source));
                    self.retained_replay_inflight
                        .retain(|source| resident_sources.contains(source));
                    self.record_media_trace(
                        None,
                        Some(bridge_instance_id),
                        None,
                        MediaTraceKind::ProjectionApplied {
                            virtual_revision,
                            bridge_local_revision: outer_revision,
                            attachment_count,
                        },
                    );
                    let mut retry_retained = false;
                    if let Some(applied) = self.pending_media_projections.remove(&virtual_revision)
                    {
                        self.pending_media_projections
                            .retain(|revision, _| *revision > virtual_revision);
                        let requests = retained_replays_after_apply(
                            &recreated_retained_sources,
                            &applied.retained_replay_candidates,
                            &applied.retained_replays,
                            &self.retained_replay_inflight,
                        );
                        retry_retained = !requests.is_empty();
                        self.retained_replay_requests.extend(requests);
                        self.record_projection_sources(&applied.sources, applied.gateway_revision);
                        self.vivid.activate_bridge_projection(&applied.sources);
                    }
                    self.check_automation_waiters();
                    if retry_retained {
                        // The just-applied outer projection recreated a retained track after the
                        // snapshot was prepared against stale residency. Publish the same
                        // projection once more and force only those missing bodies across VVMX.
                        self.last_media_projection = None;
                        self.sync_media(true);
                    }
                }
            }
            ClientMessage::BridgeTrace {
                bridge_instance_id,
                event,
            } => {
                if self.client_is(id) {
                    if matches!(event.kind, MediaTraceKind::BridgeClientAttached { .. })
                        || self.bridge_instance_id.is_none()
                    {
                        if self.bridge_instance_id != Some(bridge_instance_id) {
                            self.bridge_local_revision = 0;
                            self.outer_attachment_generations.clear();
                            self.retained_replay_requests.clear();
                            self.retained_replay_inflight.clear();
                        }
                        self.bridge_instance_id = Some(bridge_instance_id);
                    }
                    if self.bridge_instance_id == Some(bridge_instance_id) {
                        self.record_media_trace(
                            event.source,
                            Some(bridge_instance_id),
                            Some(event.origin_monotonic_us),
                            event.kind,
                        );
                    }
                }
            }
            ClientMessage::BridgePlaybackState {
                source,
                state,
                eos_state,
            } => {
                if self.client_is(id) {
                    self.record_media_trace(
                        Some(source),
                        self.bridge_instance_id,
                        None,
                        MediaTraceKind::PlaybackState { state, eos_state },
                    );
                    self.vivid.apply_outer_playback(source, state, eos_state);
                }
            }
            ClientMessage::BridgeMetrics(metrics) => {
                if self.client_is(id) {
                    self.bridge_metrics = metrics;
                }
            }
            ClientMessage::Detach => {
                if self.client_is(id) {
                    self.record_media_trace(
                        None,
                        self.bridge_instance_id,
                        None,
                        MediaTraceKind::BridgeClientDetached,
                    );
                    self.cancel_pointer_drag(true);
                    self.invalidate_mouse_selection_state();
                    crate::ipc::send(
                        &writer,
                        &ServerMessage::Detached {
                            reason: "detached".into(),
                        },
                    )?;
                    self.attached = None;
                    self.clear_kitty_graphics();
                    self.reported_input_mode = None;
                    self.bridge_instance_id = None;
                    self.bridge_local_revision = 0;
                    self.pending_media_projections.clear();
                    self.retained_replay_requests.clear();
                    self.retained_replay_inflight.clear();
                    self.traced_recovery_deliveries.clear();
                    self.record_projection_sources(&HashSet::new(), self.vivid.revision());
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
        let caller = CallerContext {
            origin: CallerOrigin::Automation { client_id },
            session_instance: self.session_instance.clone(),
            focused_fallback: request.allow_focused,
            capabilities: plugin_enforceable_permissions().into_iter().collect(),
        };

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
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation(
                        target,
                        automation_capabilities(disabled_plugin_capabilities(
                            &self.session_instance,
                        )),
                    );
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.capabilities(target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::ReloadConfig => match self.reload_config() {
                Ok(report) => self.reply_automation(target, report.to_json()),
                Err(error) => {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_config", error),
                    );
                }
            },
            AutomationMethod::ListPanes => {
                match self.execute_session_command(&caller, SessionCommand::InspectSession) {
                    Ok(value) => self.reply_automation(target, value),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::SessionInspect => {
                self.reply_automation(target, self.automation_session_inspect());
            }
            AutomationMethod::ListTabs => {
                self.reply_automation(target, self.automation_tabs());
            }
            AutomationMethod::SelectTab {
                tab_id,
                wait,
                timeout_ms,
            } => {
                let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "tab_not_found",
                            format!("tab {tab_id} does not exist"),
                        ),
                    );
                    return;
                };
                let before_outer = self.outer_projection_revision;
                self.active_tab = index;
                self.force_full = true;
                self.relayout();
                let result = serde_json::json!({
                    "tab_id": tab_id,
                    "focused_pane_id": self.tabs[index].focused,
                    "session_sequence": self.session_sequence,
                    "layout_sequence": self.layout_revision,
                });
                self.finish_selection(target, wait, timeout_ms, before_outer, result);
            }
            AutomationMethod::Diagnose {
                pane_id: requested_pane,
                all_panes,
                trace_limit,
            } => match self.automation_diagnose(requested_pane, all_panes, trace_limit) {
                Ok(result) => self.reply_automation(target, result),
                Err(error) => self.reply_automation_error(target, error),
            },
            AutomationMethod::ReportAgent {
                agent,
                state,
                source,
                sequence,
            } => {
                let pane_id = pane_id.unwrap();
                let visible = self.pane_is_visibly_present(pane_id);
                let result = self
                    .agent_catalog
                    .identity(&agent)
                    .ok_or("agent definition is not enabled")
                    .and_then(|identity| {
                        self.panes
                            .get_mut(&pane_id)
                            .ok_or("pane no longer exists")
                            .and_then(|pane| {
                                pane.agent
                                    .report(identity, state, source, sequence, visible)
                            })
                    });
                match result {
                    Ok(()) => {
                        self.session_sequence = self.session_sequence.wrapping_add(1);
                        self.schedule_render();
                        let agent = self.panes[&pane_id].agent.snapshot().map(agent_json);
                        self.reply_automation(
                            target,
                            serde_json::json!({"pane_id": pane_id, "agent": agent}),
                        );
                    }
                    Err(message) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_agent_report", message),
                    ),
                }
            }
            AutomationMethod::ClearAgentReport { source, sequence } => {
                let pane_id = pane_id.unwrap();
                let result = self
                    .panes
                    .get_mut(&pane_id)
                    .ok_or("pane no longer exists")
                    .and_then(|pane| pane.agent.clear_report(&source, sequence));
                match result {
                    Ok(()) => {
                        self.evaluate_agent_states();
                        self.session_sequence = self.session_sequence.wrapping_add(1);
                        self.schedule_render();
                        let agent = self.panes[&pane_id].agent.snapshot().map(agent_json);
                        self.reply_automation(
                            target,
                            serde_json::json!({"pane_id": pane_id, "agent": agent}),
                        );
                    }
                    Err(message) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_agent_report", message),
                    ),
                }
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
                    self.outer_media_projection(),
                    self.relay_metrics(),
                );
                self.reply_automation(target, serde_json::to_value(status).unwrap());
            }
            AutomationMethod::TraceMedia {
                after_sequence,
                limit,
                timeout_ms,
                filter,
            } => {
                let pane_id = pane_id.unwrap();
                let result = self
                    .media_trace
                    .query(after_sequence, limit, Some(pane_id), filter);
                if timeout_ms == 0 || result.gap.is_some() || !result.events.is_empty() {
                    self.reply_automation(target, serde_json::to_value(result).unwrap());
                } else {
                    self.add_automation_waiter(AutomationWaiter {
                        reply: target,
                        pane_id: Some(pane_id),
                        deadline: deadline(timeout_ms),
                        kind: AutomationWaitKind::MediaTrace {
                            after_sequence,
                            limit,
                            filter,
                        },
                    });
                }
            }
            AutomationMethod::Split { axis } => {
                let pane_id = pane_id.unwrap();
                match self.automation_split(pane_id, axis) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::SaveLayout { path } => match self.automation_save_layout(path) {
                Ok(result) => self.reply_automation(target, result),
                Err(error) => self.reply_automation_error(target, error),
            },
            AutomationMethod::Run {
                command,
                placement,
                cwd,
                hold,
                focus,
            } => {
                let pane_id = pane_id.unwrap();
                match self.automation_run(pane_id, command, placement, cwd, hold, focus) {
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
            AutomationMethod::FocusWait { wait, timeout_ms } => {
                let pane_id = pane_id.unwrap();
                let before_outer = self.outer_projection_revision;
                match self.automation_focus(pane_id) {
                    Ok(()) => {
                        let result = serde_json::json!({
                            "pane_id": pane_id,
                            "tab_id": self.tabs[self.active_tab].id,
                            "session_sequence": self.session_sequence,
                            "layout_sequence": self.layout_revision,
                        });
                        self.finish_selection(target, Some(wait), timeout_ms, before_outer, result);
                    }
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::ClosePane => {
                let pane_id = pane_id.unwrap();
                self.close_pane(pane_id);
                self.reply_automation(target, serde_json::Value::Null);
            }
            AutomationMethod::Typing { text, report } => {
                self.automation_input(target, pane_id.unwrap(), text.into_bytes(), report);
            }
            AutomationMethod::Key {
                key,
                modifiers,
                repeat,
                report,
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
                        self.automation_input(
                            target,
                            pane_id,
                            encoded.repeat(usize::from(repeat)),
                            report,
                        );
                    }
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Paste { text, report } => {
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
                self.automation_input(target, pane_id, bytes, report);
            }
            AutomationMethod::GetText { rows } => {
                match self.execute_session_command(
                    &caller,
                    SessionCommand::ReadPaneText {
                        pane_id,
                        rows: rows.map(usize::from),
                        max_bytes: AUTOMATION_REPLY_LIMIT,
                    },
                ) {
                    Ok(value) => self.reply_automation(target, value),
                    Err(error) => self.reply_automation_error(target, error),
                }
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
            AutomationMethod::Search {
                pattern,
                regex,
                direction,
                start_line,
                start_column,
                limit,
            } => {
                let pane = &self.panes[&pane_id.unwrap()];
                match crate::search::compile(&pattern, regex, true) {
                    Ok(compiled) => {
                        let (matches, truncated) = automation_search(
                            &pane.terminal,
                            &compiled,
                            direction,
                            start_line,
                            start_column,
                            usize::from(limit),
                        );
                        self.reply_automation(
                            target,
                            serde_json::json!({
                                "matches": matches,
                                "truncated": truncated,
                            }),
                        );
                    }
                    Err(error) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_params", error),
                    ),
                }
            }
            AutomationMethod::SetSyncInput { enabled } => {
                let pane_id = pane_id.unwrap();
                let Some(tab_index) = self.tabs.iter().position(|tab| tab.contains(pane_id)) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                self.tabs[tab_index].sync_input = enabled;
                self.session_sequence = self.session_sequence.wrapping_add(1);
                if tab_index == self.active_tab {
                    self.force_full = true;
                    self.schedule_render();
                }
                self.reply_automation(
                    target,
                    serde_json::json!({
                        "tab_id": self.tabs[tab_index].id,
                        "sync_input": enabled,
                    }),
                );
            }
            AutomationMethod::Action(action) => {
                let pane_id = pane_id.unwrap();
                match self.automation_action(pane_id, action) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::Invoke {
                reference,
                input,
                detach,
            }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if detach {
                    match supervisor.invoke_detached(reference, input) {
                        Ok(job_id) => self.reply_automation(
                            target,
                            serde_json::json!({"job_id": job_id, "status": "queued"}),
                        ),
                        Err(error) => self.reply_automation_error(target, error),
                    }
                    return;
                }
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.invoke_automation(reference, input, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::JobStatus { job_id }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.job_status(job_id, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::JobCancel { job_id }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.job_cancel(job_id, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::JobLogs { job_id }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.job_logs(job_id, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::PaneOpen { reference }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.open_pane(reference, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::EventSubscribe {
                after_sequence,
            }) => {
                self.subscribe_plugin_events(target, after_sequence);
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::EventUnsubscribe {
                subscription_id,
            }) => {
                let removed = self
                    .plugin_event_subscriptions
                    .get(&subscription_id)
                    .is_some_and(|subscription| subscription.client_id == client_id)
                    && self
                        .plugin_event_subscriptions
                        .remove(&subscription_id)
                        .is_some();
                if removed {
                    self.reply_automation(
                        target,
                        serde_json::json!({"subscription_id": subscription_id, "subscribed": false}),
                    );
                } else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "scope_denied",
                            "event subscription is not owned by this client",
                        ),
                    );
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::Reload) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation(
                        target,
                        serde_json::json!({
                            "disabled": true,
                            "generation": null,
                            "applied": [],
                            "deferred": [],
                            "failed": {},
                        }),
                    );
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.reload_automation(target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
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
            AutomationMethod::WaitMediaTrack {
                identity,
                condition,
                timeout_ms,
            } => {
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::MediaTrack {
                        identity,
                        condition,
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

    fn subscribe_plugin_events(
        &mut self,
        target: AutomationReplyTarget,
        after_sequence: Option<u64>,
    ) {
        if self.plugin_supervisor.is_none() {
            self.reply_automation_error(target, plugin_disabled_error());
            return;
        }
        if self.plugin_event_subscriptions.len() >= PLUGIN_EVENT_SUBSCRIPTIONS {
            self.reply_automation_error(
                target,
                AutomationError::new("busy", "plugin event subscription limit reached"),
            );
            return;
        }
        let subscription_id = format!(
            "{}/events-{:016x}",
            self.session_instance, self.next_plugin_subscription
        );
        self.next_plugin_subscription = self.next_plugin_subscription.wrapping_add(1).max(1);
        let (sender, receiver) = mpsc::sync_channel(PLUGIN_EVENT_STREAM_QUEUE);
        let writer = target.writer.clone();
        let cancel = target.cancel.clone();
        let stream_subscription_id = subscription_id.clone();
        if std::thread::Builder::new()
            .name("vvmux-plugin-event-stream".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    let message = match message {
                        PluginStreamMessage::Response(response) => {
                            ServerMessage::Automation(response)
                        }
                        PluginStreamMessage::Event(envelope) => ServerMessage::PluginEvent {
                            subscription_id: stream_subscription_id.clone(),
                            envelope: Box::new(envelope),
                        },
                    };
                    if crate::ipc::send(&writer, &message).is_err() {
                        cancel.cancel();
                        break;
                    }
                }
            })
            .is_err()
        {
            self.reply_automation_error(
                target,
                AutomationError::new("runtime_unavailable", "could not start event stream"),
            );
            return;
        }
        self.finish_automation_request(target.client_id, target.request_id);
        let response = AutomationResponse::success(
            target.request_id,
            serde_json::json!({
                "subscription_id": subscription_id,
                "after_sequence": after_sequence,
                "latest_sequence": self.plugin_event_sequence,
            }),
        );
        if sender
            .try_send(PluginStreamMessage::Response(response))
            .is_err()
        {
            target.cancel.cancel();
            return;
        }
        if let Some(after) = after_sequence {
            for envelope in self.plugin_event_journal.replay(
                after,
                self.plugin_event_sequence,
                PLUGIN_EVENT_STREAM_QUEUE.saturating_sub(1),
            ) {
                if sender
                    .try_send(PluginStreamMessage::Event(envelope))
                    .is_err()
                {
                    target.cancel.cancel();
                    return;
                }
            }
        }
        self.plugin_event_subscriptions.insert(
            subscription_id,
            PluginEventSubscription {
                client_id: target.client_id,
                sender,
                cancel: target.cancel,
            },
        );
    }

    fn queue_plugin_state_event(
        &mut self,
        name: &str,
        key: String,
        payload: serde_json::Value,
        pane_id: Option<PaneId>,
    ) {
        self.pending_plugin_state_events.insert(
            (name.to_owned(), key),
            (payload, pane_id, self.active_plugin_cause.clone()),
        );
    }

    fn flush_plugin_state_events(&mut self) {
        let events = std::mem::take(&mut self.pending_plugin_state_events);
        for ((name, _), (payload, pane_id, cause)) in events {
            let previous_cause = std::mem::replace(&mut self.active_plugin_cause, cause);
            self.publish_plugin_event(&name, payload, pane_id, None);
            self.active_plugin_cause = previous_cause;
        }
    }

    fn publish_plugin_event(
        &mut self,
        name: &str,
        payload: serde_json::Value,
        pane_id: Option<PaneId>,
        context: Option<vvmux_plugin_api::InvocationContext>,
    ) {
        self.plugin_event_sequence = self.plugin_event_sequence.saturating_add(1);
        let sequence = self.plugin_event_sequence;
        let cause = self.active_plugin_cause.clone();
        let context = context.unwrap_or_else(|| vvmux_plugin_api::InvocationContext {
            correlation_id: cause.as_ref().map_or_else(
                || format!("{}-event-{sequence:016x}", self.session_instance),
                |cause| cause.correlation_id.clone(),
            ),
            causation_id: cause.as_ref().map_or_else(
                || format!("{}-event-{sequence:016x}", self.session_instance),
                |cause| cause.causation_id.clone(),
            ),
            causation_depth: cause.as_ref().map_or(0, |cause| cause.causation_depth),
            source: cause.map_or_else(|| "session".into(), |cause| cause.source),
            session_instance: self.session_instance.clone(),
            pane_id,
            tab_id: pane_id.and_then(|pane_id| {
                self.tabs
                    .iter()
                    .find(|tab| tab.contains(pane_id))
                    .map(|tab| tab.id)
            }),
            deadline_unix_ms: 0,
        });
        let envelope = PluginEventEnvelope::Event {
            sequence,
            name: name.to_owned(),
            payload,
            context,
        };
        self.plugin_event_journal.push(envelope.clone());
        self.plugin_event_subscriptions.retain(|_, subscription| {
            let sent = subscription
                .sender
                .try_send(PluginStreamMessage::Event(envelope.clone()))
                .is_ok();
            if !sent {
                subscription.cancel.cancel();
            }
            sent
        });
        if let Some(supervisor) = &self.plugin_supervisor {
            supervisor.publish_event(envelope);
        }
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
        self.spawn_pane(new_pane_id, tab_id, &PaneSpawn::default())
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

    /// Write the current layout to a startup layout file.
    ///
    /// Unlike the interactive prompt this never asks before replacing an existing file: an
    /// automation caller named the path itself.
    fn automation_save_layout(
        &mut self,
        path: Option<String>,
    ) -> Result<serde_json::Value, AutomationError> {
        let path = crate::layout_file::resolve_save_path(
            path.as_deref().unwrap_or(crate::layout_file::STARTUP_FILE),
        )
        .map_err(|error| AutomationError::new("invalid_argument", error.to_string()))?;
        let (tabs, panes) = self
            .save_layout(&path)
            .map_err(|error| AutomationError::new("save_failed", error.to_string()))?;
        Ok(serde_json::json!({
            "path": path.display().to_string(),
            "tabs": tabs,
            "panes": panes,
            "session_sequence": self.session_sequence,
        }))
    }

    /// Open a pane running one command.
    ///
    /// Ordering mirrors `automation_split`: the tiled tree is cloned and validated *before* any
    /// process is created, so a placement that cannot fit fails without leaving an orphan shell,
    /// and the tree is committed only once the spawn has succeeded.
    fn automation_run(
        &mut self,
        anchor: PaneId,
        command: String,
        placement: crate::ipc::RunPlacement,
        cwd: Option<String>,
        hold: bool,
        focus: bool,
    ) -> Result<serde_json::Value, AutomationError> {
        let spec = PaneSpawn {
            command: Some(OsString::from(command)),
            argv: None,
            cwd: cwd.map(PathBuf::from),
            transparent: None,
            hold_on_exit: hold,
            extra_env: Vec::new(),
            role: PaneRole::Core,
            vivid_capability: true,
        };
        self.place_pane(anchor, spec, placement, focus, None)
    }

    /// Validate placement before process creation, then commit one actor-owned pane mutation.
    fn place_pane(
        &mut self,
        anchor: PaneId,
        spec: PaneSpawn,
        placement: crate::ipc::RunPlacement,
        focus: bool,
        tab_name: Option<String>,
    ) -> Result<serde_json::Value, AutomationError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(anchor))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        let new_pane_id = self.next_pane_id;

        let tab_id = match placement {
            crate::ipc::RunPlacement::Split { axis } => {
                let tab_id = self.tabs[tab_index].id;
                let mut candidate = self.tabs[tab_index].tree.clone().ok_or_else(|| {
                    AutomationError::new("unsupported", "tab has no tiled layout")
                })?;
                if !candidate.contains(anchor) {
                    return Err(AutomationError::new(
                        "invalid_state",
                        "cannot split a floating pane",
                    ));
                }
                candidate
                    .split(anchor, new_pane_id, axis, self.content_area())
                    .map_err(|_| {
                        AutomationError::new("invalid_state", "pane is too small to split")
                    })?;
                self.spawn_pane(new_pane_id, tab_id, &spec)
                    .map_err(|error| AutomationError::new("spawn_failed", error.to_string()))?;
                self.tabs[tab_index].tree = Some(candidate);
                tab_id
            }
            crate::ipc::RunPlacement::Float => {
                let tab_id = self.tabs[tab_index].id;
                self.spawn_pane(new_pane_id, tab_id, &spec)
                    .map_err(|error| AutomationError::new("spawn_failed", error.to_string()))?;
                let area = self.content_area();
                let width_percent = self.config.floating.default_width_percent;
                let height_percent = self.config.floating.default_height_percent;
                self.tabs[tab_index].floating.insert(
                    new_pane_id,
                    area,
                    width_percent,
                    height_percent,
                );
                tab_id
            }
            crate::ipc::RunPlacement::Tab => {
                let tab_id = self.next_tab_id;
                self.spawn_pane(new_pane_id, tab_id, &spec)
                    .map_err(|error| AutomationError::new("spawn_failed", error.to_string()))?;
                self.tabs.push(Tab {
                    id: tab_id,
                    name: tab_name,
                    tree: Some(TiledNode::leaf(new_pane_id)),
                    floating: FloatingLayer::default(),
                    focused: new_pane_id,
                    last_focused_tiled: Some(new_pane_id),
                    zoomed: None,
                    sync_input: false,
                });
                self.next_tab_id += 1;
                tab_id
            }
        };

        self.next_pane_id = self.next_pane_id.wrapping_add(1);
        if focus {
            // A new tab is already focused on its own pane; only move the active tab when the
            // caller asked for focus.
            if let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) {
                self.tabs[index].set_focus(new_pane_id);
                self.active_tab = index;
            }
        }
        self.force_full = true;
        self.relayout();
        Ok(serde_json::json!({
            "pane_id": new_pane_id,
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

    fn finish_selection(
        &mut self,
        target: AutomationReplyTarget,
        wait: Option<AutomationCompletion>,
        timeout_ms: u64,
        before_outer: u64,
        result: serde_json::Value,
    ) {
        let Some(level) = wait else {
            self.reply_automation(target, result);
            return;
        };
        let supported = match level {
            AutomationCompletion::Outer => {
                self.attached.as_ref().is_some_and(|client| client.vivid)
            }
            AutomationCompletion::Rendered => self.attached.is_some(),
        };
        if !supported {
            let message = match level {
                AutomationCompletion::Outer => {
                    "no attached Vivid-capable foreground client can acknowledge media projection"
                }
                AutomationCompletion::Rendered => {
                    "no attached client can acknowledge the terminal frame"
                }
            };
            self.reply_automation_error(
                target,
                AutomationError::new("missing_attachment", message),
            );
            return;
        }
        self.add_automation_waiter(AutomationWaiter {
            reply: target,
            pane_id: None,
            deadline: deadline(timeout_ms),
            kind: AutomationWaitKind::Completion {
                level,
                after_outer: before_outer,
                after_session: self.session_sequence,
                result,
            },
        });
    }

    fn automation_tabs(&self) -> serde_json::Value {
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let mut pane_ids = tab.tree.as_ref().map_or_else(Vec::new, TiledNode::pane_ids);
                pane_ids.extend(tab.floating.pane_ids());
                pane_ids.sort_unstable();
                pane_ids.dedup();
                serde_json::json!({
                    "tab_id": tab.id,
                    "display_index": index,
                    "name": tab.name,
                    "active": index == self.active_tab,
                    "focused_pane_id": tab.focused,
                    "pane_ids": pane_ids,
                    "sync_input": tab.sync_input,
                    "zoomed_pane_id": tab.zoomed,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "session": self.name,
            "session_instance": self.session_instance,
            "active_tab_id": self.active_tab().map(|tab| tab.id),
            "tabs": tabs,
        })
    }

    fn automation_session_inspect(&self) -> serde_json::Value {
        let queue = self.relay_metrics();
        serde_json::json!({
            "schema_version": 1,
            "session": self.name,
            "session_instance": self.session_instance,
            "attachment": self.attached.as_ref().map(|client| serde_json::json!({
                "client_id": client.id,
                "vivid_capable": client.vivid,
                "acknowledged_frame": client.acknowledged_frame,
                "rendered_session_sequence": client.rendered_session_sequence,
                "pending_frame_acknowledgements": client.frame_sequences.len(),
            })),
            "active_tab_id": self.active_tab().map(|tab| tab.id),
            "active_pane_id": self.active_tab().map(|tab| tab.focused),
            "session_sequence": self.session_sequence,
            "layout_revision": self.layout_revision,
            "virtual_projection_revision": self.vivid.revision(),
            "submitted_projection_revision": self.media_projection_revision,
            "outer_projection_revision": self.outer_projection_revision,
            "outer_apply_sequence": self.outer_apply_sequence,
            "bridge_instance_id": self.bridge_instance_id,
            "bridge_local_revision": self.bridge_local_revision,
            "pending": {
                "actor_work": self.pending_actor_work.len(),
                "automation_waiters": self.automation_waiters.len(),
                "media_projections": self.pending_media_projections.len(),
                "render_scheduled": self.pending_render,
            },
            "queue_health": queue,
            "tabs": self.automation_tabs()["tabs"],
        })
    }

    fn automation_diagnose(
        &self,
        requested_pane: Option<PaneId>,
        all_panes: bool,
        trace_limit: u16,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane_ids = if all_panes {
            self.panes.keys().copied().collect::<Vec<_>>()
        } else {
            vec![
                requested_pane
                    .or_else(|| self.active_tab().map(|tab| tab.focused))
                    .ok_or_else(|| {
                        AutomationError::new("no_focused_pane", "session has no pane")
                    })?,
            ]
        };
        let mut panes = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let pane = self.pane_description(pane_id).ok_or_else(|| {
                AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
            })?;
            let media = self.vivid.pane_status(
                pane_id,
                self.outer_media_projection(),
                self.relay_metrics(),
            );
            let trace = self.media_trace.query(
                None,
                trace_limit,
                Some(pane_id),
                MediaTraceFilter::default(),
            );
            panes.push(serde_json::json!({"pane": pane, "media": media, "trace": trace}));
        }
        Ok(serde_json::json!({
            "schema_version": 1,
            "capture": {"atomic_actor_turn": true, "asynchronous_metric_age_ms": 0},
            "session": self.automation_session_inspect(),
            "panes": panes,
        }))
    }

    fn automation_action(
        &mut self,
        pane_id: PaneId,
        action: Action,
    ) -> Result<serde_json::Value, AutomationError> {
        if matches!(action, Action::CopyInput(_)) {
            return Err(AutomationError::new(
                "unsupported",
                "copy-mode input is not an automation action; use `vvmux msg key`",
            ));
        }
        self.automation_focus(pane_id)?;
        self.action(action);
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "session_sequence": self.session_sequence,
        }))
    }

    fn automation_input(
        &mut self,
        target: AutomationReplyTarget,
        pane_id: PaneId,
        bytes: Vec<u8>,
        report: bool,
    ) {
        if self.invalidate_mouse_selection_for_pane(pane_id) {
            self.schedule_render();
        }
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
        let byte_count = bytes.len();
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
                    pane_id,
                    byte_count,
                    report,
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
        let plugin = match &pane.role {
            PaneRole::Core => None,
            PaneRole::Plugin(owner) => Some(serde_json::json!({
                "plugin_id": owner.plugin_id,
                "plugin_instance": owner.plugin_instance,
                "package_digest": owner.package_digest,
                "entrypoint_id": owner.entrypoint_id,
                "accept_sync_input": owner.accept_sync_input,
            })),
        };
        let title = pane.terminal.title().or(match &pane.role {
            PaneRole::Plugin(owner) => Some(owner.title.as_str()),
            PaneRole::Core => None,
        });
        Some(serde_json::json!({
            "pane_id": pane_id,
            "tab_id": tab.id,
            "tab_name": tab.name,
            "active_tab": tab_index == self.active_tab,
            "focused": tab.focused == pane_id,
            "visible": visible,
            "layer": layer,
            "zoomed": tab.zoomed == Some(pane_id),
            "sync_input": tab.sync_input,
            "transparent": pane.transparent,
            "title": title,
            "plugin": plugin,
            "agent": pane.agent.snapshot().map(agent_json),
            "geometry": rect_json(outer),
            "content_geometry": rect_json(outer.content()),
            "columns": pane.terminal.cols(),
            "rows": pane.terminal.rows(),
            "history_size": pane.terminal.history_len(),
            "display_offset": pane.copy.as_ref().map_or(0, |copy| copy.offset),
            "copy_mode": pane.copy.is_some(),
            "copy": pane.copy.as_ref().map(|copy| serde_json::json!({
                "offset": copy.offset,
                "row": copy.row,
                "column": copy.column,
                "search_query": copy.search.as_ref().map(|search| search.query.as_str()),
            })),
            "cursor": { "row": cursor.0, "column": cursor.1, "visible": pane.terminal.modes().cursor_visible },
            "modes": terminal_mode_names(pane.terminal.modes()),
            "screen": if pane.terminal.alternate_screen() { "alternate" } else { "primary" },
            "process_state": if pane.exit_status.is_some() { "exited" } else { "running" },
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
                if let AutomationWaitKind::MediaTrace {
                    after_sequence,
                    limit,
                    filter,
                } = waiter.kind
                {
                    let result =
                        self.media_trace
                            .query(after_sequence, limit, waiter.pane_id, filter);
                    self.reply_automation(waiter.reply, serde_json::to_value(result).unwrap());
                } else {
                    self.reply_automation_error(
                        waiter.reply,
                        AutomationError::new("timeout", "automation wait timed out"),
                    );
                }
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
                    self.outer_media_projection(),
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
            AutomationWaitKind::MediaTrace {
                after_sequence,
                limit,
                filter,
            } => {
                let result =
                    self.media_trace
                        .query(*after_sequence, *limit, waiter.pane_id, *filter);
                (result.gap.is_some() || !result.events.is_empty()).then(|| {
                    serde_json::to_value(result).map_err(|error| {
                        AutomationError::new("serialization_failed", error.to_string())
                    })
                })
            }
            AutomationWaitKind::Completion {
                level,
                after_outer,
                after_session,
                result,
            } => {
                let ready = match level {
                    AutomationCompletion::Outer => {
                        if self.attached.as_ref().is_none_or(|client| !client.vivid) {
                            return Some(Err(AutomationError::new(
                                "missing_attachment",
                                "foreground Vivid bridge detached while waiting",
                            )));
                        }
                        self.outer_projection_revision > *after_outer
                    }
                    AutomationCompletion::Rendered => {
                        let Some(client) = self.attached.as_ref() else {
                            return Some(Err(AutomationError::new(
                                "missing_attachment",
                                "terminal client detached while waiting for render",
                            )));
                        };
                        client.rendered_session_sequence >= *after_session
                    }
                };
                ready.then(|| {
                    let mut result = result.clone();
                    result["completion"] = serde_json::json!({
                        "level": match level {
                            AutomationCompletion::Outer => "outer",
                            AutomationCompletion::Rendered => "rendered",
                        },
                        "outer_projection_revision": self.outer_projection_revision,
                        "rendered_session_sequence": self.rendered_session_sequence(),
                    });
                    Ok(result)
                })
            }
            AutomationWaitKind::MediaTrack {
                identity,
                condition,
            } => {
                let pane_id = waiter.pane_id?;
                let status = self.vivid.pane_status(
                    pane_id,
                    self.outer_media_projection(),
                    self.relay_metrics(),
                );
                let track = status.tracks.iter().find(|track| {
                    track.producer_id == identity.producer_id
                        && track.context_id == identity.context_id
                        && track.surface_id == identity.surface_id
                        && track.track_id == identity.track_id
                });
                let Some(track) = track else {
                    return Some(Err(AutomationError::new(
                        "track_not_found",
                        "media track does not exist in the requested pane",
                    )));
                };
                let clock_started = track.milestones & (1 << 6) != 0;
                let eos = track.milestones & (1 << 7) != 0;
                let random_access = track.milestones & (1 << 3) != 0;
                let matched = match condition {
                    MediaTrackWaitCondition::Visible => track.visible,
                    MediaTrackWaitCondition::Hidden => !track.visible,
                    MediaTrackWaitCondition::OuterAttached => {
                        track.outer_mapping_fresh && track.outer_channel_generation.is_some()
                    }
                    MediaTrackWaitCondition::KeyframeNeeded => track.keyframe_needed,
                    MediaTrackWaitCondition::KeyframeRecovered => {
                        !track.keyframe_needed && random_access
                    }
                    MediaTrackWaitCondition::Playing => track.lifecycle == "playing",
                    MediaTrackWaitCondition::Paused => track.lifecycle == "live" && clock_started,
                    MediaTrackWaitCondition::Eos => eos || track.lifecycle == "ended",
                    MediaTrackWaitCondition::Lost => track.lifecycle == "lost",
                    MediaTrackWaitCondition::QueueDrained => {
                        track.queued_packets == 0 && track.queued_bytes == 0
                    }
                };
                matched.then(|| {
                    serde_json::to_value(track).map_err(|error| {
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
        let screen_sequence = pane.screen_sequence;
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.queue_plugin_state_event(
            "pane.screen_changed",
            pane_id.to_string(),
            serde_json::json!({
                "pane_id": pane_id,
                "screen_sequence": screen_sequence,
            }),
            Some(pane_id),
        );
    }

    fn refresh_agent_detector_targets(&self) {
        self.agent_detector.replace_targets(
            self.panes
                .values()
                .filter(|pane| pane.exit_status.is_none())
                .map(|pane| ProbeTarget {
                    pane_id: pane.id,
                    child_pid: pane.child_pid,
                    control: pane.control.clone(),
                })
                .collect(),
        );
    }

    fn evaluate_agent_states(&mut self) {
        let visible = if self.attached.is_some() && self.client_focused {
            let area = self.content_area();
            self.active_tab()
                .map(|tab| {
                    visible_projections(tab, area)
                        .into_iter()
                        .map(|projection| projection.pane_id)
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default()
        } else {
            HashSet::new()
        };
        let now = Instant::now();
        let mut changed = false;
        for pane in self.panes.values_mut() {
            let before = pane.agent.snapshot();
            pane.agent.evaluate_terminal(
                &self.agent_catalog,
                &pane.terminal,
                visible.contains(&pane.id),
                now,
            );
            changed |= before != pane.agent.snapshot();
        }
        if changed {
            self.session_sequence = self.session_sequence.wrapping_add(1);
            if self.agent_navigator.is_some() {
                self.schedule_render();
            }
        }
    }

    fn next_agent_evaluation_delay(&self) -> Duration {
        let now = Instant::now();
        self.panes
            .values()
            .filter_map(|pane| pane.agent.next_evaluation_delay(now))
            .min()
            .unwrap_or(Duration::from_secs(1))
    }

    fn input(&mut self, bytes: Vec<u8>) {
        if self.agent_navigator.is_some() {
            self.agent_navigator_input(&key_presses(&bytes));
            return;
        }
        if self.tab_navigator.is_some() {
            self.tab_navigator_input(&key_presses(&bytes));
            return;
        }
        if self.tab_rename.is_some() {
            self.tab_rename_input(&key_presses(&bytes));
            return;
        }
        if self.close_pane_confirmation.is_some() {
            self.close_pane_confirmation_input(&key_presses(&bytes));
            return;
        }
        if self.save_layout_prompt.is_some() {
            self.save_layout_prompt_input(&key_presses(&bytes));
            return;
        }
        if self.invalidate_mouse_selection_state() {
            self.schedule_render();
        }
        let Some(pane_id) = self.active_tab().map(|tab| tab.focused) else {
            return;
        };
        if self
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.copy.is_some())
        {
            self.copy_input(pane_id, &key_presses(&bytes));
        } else if self.active_tab().is_some_and(|tab| tab.sync_input) {
            self.broadcast_input(&bytes);
        } else if let Some(pane) = self.panes.get_mut(&pane_id) {
            let failure = queue_pane_input(pane, &bytes);
            self.report_input_failure(pane_id, failure);
        }
    }

    fn broadcast_input(&mut self, bytes: &[u8]) {
        let targets = self.active_tab().map_or_else(Vec::new, |tab| {
            sync_targets(tab, &|pane_id| {
                self.panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.copy.is_some() || !pane_role_accepts_sync(&pane.role))
            })
        });
        let failures = queue_input_targets(&mut self.panes, &targets, bytes);
        for (pane_id, failure) in failures {
            self.report_input_failure(pane_id, Some(failure));
        }
    }

    fn clear_retained_mouse_selections(&mut self) -> bool {
        let mut changed = false;
        for pane in self.panes.values_mut() {
            changed |= pane.mouse_selection.take().is_some();
        }
        changed
    }

    fn invalidate_mouse_selection_state(&mut self) -> bool {
        self.mouse_selection_drag = None;
        self.mouse_click_tracker = None;
        // Everything the pointer was resolved against is now stale — the client detached, the
        // display resized, or the pane scrolled under a stationary pointer. A wheel scroll is the
        // case that matters most: no motion event follows it, so a hover kept here would stay
        // painted on whichever link happened to scroll into that cell.
        //
        // Deliberately not tied to pane output, which no longer invalidates selections wholesale:
        // clearing on every PTY chunk would make a link in any actively-printing pane unhoverable
        // (and erased selections during any continuous redraw — see
        // `adjust_mouse_selection_after_pane_output` for the replacement).
        self.hovered_link = None;
        self.clear_retained_mouse_selections()
    }

    /// Fold one batch of pane output into the pane's mouse-selection state.
    ///
    /// The selection used to be invalidated on every PTY output chunk, which erased it during any
    /// continuous redraw. It now survives redraws and rotates with content that scrolls into
    /// scrollback; the same transform keeps a drag anchor and a multi-click cell on their text, so
    /// a selection in progress in a busy pane finishes on what was selected. Rendering is left to
    /// the caller — pane output already schedules one.
    fn adjust_mouse_selection_after_pane_output(
        &mut self,
        pane_id: PaneId,
        events: &[TerminalEvent],
        screen_switched: bool,
        history_len: usize,
    ) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let adjusted = pane_mouse_selection_after_output(
                pane.mouse_selection,
                events,
                screen_switched,
                history_len,
            );
            pane.mouse_selection = adjusted;
        }
        if let Some(drag) = self
            .mouse_selection_drag
            .take()
            .filter(|drag| drag.pane == pane_id)
        {
            let anchor = MouseSelection {
                start: drag.start,
                end: drag.start,
                mode: drag.mode,
            };
            match pane_mouse_selection_after_output(
                Some(anchor),
                events,
                screen_switched,
                history_len,
            ) {
                Some(anchor) => {
                    self.mouse_selection_drag = Some(MouseSelectionDrag {
                        start: anchor.start,
                        ..drag
                    })
                }
                None => self.mouse_click_tracker = None,
            }
        }
        if let Some(click) = self
            .mouse_click_tracker
            .take()
            .filter(|click| click.pane == pane_id)
        {
            let cell = MouseSelection {
                start: click.cell,
                end: click.cell,
                mode: MouseSelectionMode::Character,
            };
            if let Some(cell) =
                pane_mouse_selection_after_output(Some(cell), events, screen_switched, history_len)
            {
                self.mouse_click_tracker = Some(MouseClickTracker {
                    cell: cell.start,
                    ..click
                });
            }
        }
    }

    fn invalidate_mouse_selection_for_pane(&mut self, pane_id: PaneId) -> bool {
        if self
            .mouse_selection_drag
            .is_some_and(|drag| drag.pane == pane_id)
        {
            self.mouse_selection_drag = None;
        }
        if self
            .mouse_click_tracker
            .is_some_and(|click| click.pane == pane_id)
        {
            self.mouse_click_tracker = None;
        }
        self.panes
            .get_mut(&pane_id)
            .and_then(|pane| pane.mouse_selection.take())
            .is_some()
    }

    fn begin_mouse_selection(&mut self, pane_id: PaneId, content: Rect, mouse: MouseEvent) {
        if !self.panes.contains_key(&pane_id) {
            return;
        }
        // Motion during a drag is consumed before it reaches hover tracking, so a hover left
        // standing here would stay painted for the whole drag. Activation does not depend on it:
        // `finish_mouse_selection` re-reads the link from the grid cell the press landed on.
        self.set_hovered_link(None);
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        let display_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let Some(cell) = mouse_selection_cell(content, mouse.x, mouse.y, display_offset) else {
            return;
        };
        let cell = normalize_mouse_selection_cell(&pane.terminal, cell);
        let click =
            MouseClickTracker::next(self.mouse_click_tracker, pane_id, cell, Instant::now());
        self.mouse_click_tracker = Some(click);
        let mode = if click.count == 3 {
            MouseSelectionMode::Line
        } else {
            MouseSelectionMode::Character
        };
        self.mouse_selection_drag = Some(MouseSelectionDrag {
            pane: pane_id,
            content,
            display_offset,
            start: cell,
            mode,
            moved: false,
        });
        if mode == MouseSelectionMode::Line {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.mouse_selection = Some(MouseSelection {
                    start: cell,
                    end: cell,
                    mode,
                });
            }
            self.schedule_render();
        }
    }

    fn update_mouse_selection(&mut self, mouse: MouseEvent) {
        let Some(mut drag) = self.mouse_selection_drag.take() else {
            return;
        };
        let Some(pane) = self.panes.get(&drag.pane) else {
            return;
        };
        let Some(end) = mouse_selection_cell(drag.content, mouse.x, mouse.y, drag.display_offset)
        else {
            return;
        };
        let end = normalize_mouse_selection_cell(&pane.terminal, end);
        drag.moved = true;
        if let Some(pane) = self.panes.get_mut(&drag.pane) {
            pane.mouse_selection = Some(MouseSelection {
                start: drag.start,
                end,
                mode: drag.mode,
            });
        }
        self.mouse_selection_drag = Some(drag);
        self.schedule_render();
    }

    fn finish_mouse_selection(&mut self, mouse: MouseEvent) {
        let Some(drag) = self.mouse_selection_drag.take() else {
            return;
        };
        let Some(pane) = self.panes.get(&drag.pane) else {
            return;
        };
        let Some(end) = mouse_selection_cell(drag.content, mouse.x, mouse.y, drag.display_offset)
        else {
            return;
        };
        let end = normalize_mouse_selection_cell(&pane.terminal, end);
        let selected = drag.mode == MouseSelectionMode::Line || drag.moved || end != drag.start;
        if !selected {
            if let Some(pane) = self.panes.get_mut(&drag.pane) {
                pane.mouse_selection = None;
            }
            // A press and release on one cell with no motion between them is a click, not a
            // selection — the same test Vivido uses to decide a drag should not launch a link.
            if self.config.hyperlinks.enabled
                && self.config.hyperlinks.open == OpenMode::Local
                && let Some(link) = self.link_at_cell(drag.pane, drag.start)
            {
                self.open_link_locally(&link.uri);
            }
            self.schedule_render();
            return;
        }
        let selection = MouseSelection {
            start: drag.start,
            end,
            mode: drag.mode,
        };
        let bytes = extract_mouse_selection(&pane.terminal, selection);
        if let Some(pane) = self.panes.get_mut(&drag.pane) {
            pane.mouse_selection = Some(selection);
        }
        self.set_copy_buffer(bytes);
        self.schedule_render();
    }

    /// Adopt `bytes` as the copy buffer and mirror it to the attached client's clipboard.
    fn set_copy_buffer(&mut self, bytes: Vec<u8>) {
        self.copy_buffer = bytes;
        self.copy_buffer.truncate(COPY_BUFFER_LIMIT);
        let clipboard = String::from_utf8_lossy(&self.copy_buffer).into_owned();
        if let Some(client) = &self.attached {
            let _ = crate::ipc::send(&client.writer, &ServerMessage::Clipboard(clipboard));
        }
    }

    /// Honor an OSC 52 store from a pane.
    ///
    /// Restricted to the focused pane of an attached session. The copy buffer belongs to the user,
    /// so a background pane silently overwriting it — or a detached session accepting a write
    /// nobody can see — is not something the user asked for. Between a focused pane and a mouse
    /// selection the later write wins, and an in-progress selection is left untouched.
    fn handle_clipboard_store(&mut self, focused: bool, selection: u8, text: Vec<u8>) {
        if !clipboard_store_allowed(
            self.config.clipboard.osc52,
            focused,
            self.attached.is_some(),
            selection,
        ) {
            return;
        }
        self.set_copy_buffer(text);
    }

    /// Answer an OSC 52 query on the requesting pane's own PTY.
    fn handle_clipboard_load(
        &mut self,
        pane_id: PaneId,
        focused: bool,
        selection: u8,
        terminator: &str,
    ) {
        if !self.config.clipboard.osc52.allows_load()
            || !focused
            || self.attached.is_none()
            || !is_supported_clipboard_selection(selection)
        {
            return;
        }
        let reply = osc52_reply(selection, &self.copy_buffer, terminator);
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let _ = queue_pane_input(pane, &reply);
        }
    }

    fn mouse(&mut self, mut mouse: MouseEvent, pixel_coordinates: bool) {
        let display = self.layout_display();
        let pixels = pixel_coordinates.then_some((mouse.x, mouse.y));
        if pixel_coordinates {
            mouse = pixel_mouse_to_cells(mouse, display);
        }
        if self.agent_navigator.is_some() {
            self.agent_navigator_mouse(mouse);
            return;
        }
        if self.tab_navigator.is_some() {
            self.tab_navigator_mouse(mouse);
            return;
        }
        if self.tab_rename.is_some() || self.close_pane_confirmation.is_some() {
            return;
        }
        if self.mouse_selection_drag.is_some() {
            match mouse.kind {
                MouseKind::Move if mouse.button == 0 => {
                    self.update_mouse_selection(mouse);
                    return;
                }
                MouseKind::Release if mouse.button == 0 => {
                    self.finish_mouse_selection(mouse);
                    return;
                }
                _ => {
                    self.mouse_selection_drag = None;
                    self.mouse_click_tracker = None;
                }
            }
        }
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
            self.forward_application_mouse(mouse, pixels, display);
            return;
        }
        if mouse.kind == MouseKind::Wheel && self.invalidate_mouse_selection_state() {
            self.schedule_render();
        }
        let cleared_selection =
            mouse.kind == MouseKind::Press && self.clear_retained_mouse_selections();
        if cleared_selection {
            self.schedule_render();
        }
        if mouse.kind == MouseKind::Press && mouse.button != 0 {
            self.mouse_click_tracker = None;
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
            if mouse.kind == MouseKind::Press && mouse.button == 0 {
                self.mouse_click_tracker = None;
            }
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
                self.mouse_click_tracker = None;
                return;
            }
        } else if mouse.kind == MouseKind::Press
            && mouse.button == 0
            && !mouse.shift
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
            self.mouse_click_tracker = None;
            return;
        }

        let content = rect.content();
        if mouse.x < content.x
            || mouse.x >= content.x + content.width
            || mouse.y < content.y
            || mouse.y >= content.y + content.height
        {
            self.schedule_render();
            if mouse.kind == MouseKind::Press && mouse.button == 0 {
                self.mouse_click_tracker = None;
            }
            return;
        }
        let selection_gesture = self.panes.get(&pane_id).is_some_and(|pane| {
            starts_mouse_selection(mouse, pane.copy.is_some(), pane.terminal.modes())
        });
        if selection_gesture {
            self.begin_mouse_selection(pane_id, content, mouse);
            return;
        }
        if mouse.kind == MouseKind::Press && mouse.button == 0 {
            self.mouse_click_tracker = None;
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
                let (x, y) = application_mouse_coordinates(
                    mouse,
                    pixels,
                    content,
                    display,
                    modes.sgr_pixels,
                );
                translated = Some(format!("\x1b[<{button};{x};{y}{terminator}"));
            } else if mouse.kind == MouseKind::Wheel {
                copy_view_render = true;
                let previous_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
                let copy = pane.copy.get_or_insert(CopyState {
                    offset: 0,
                    row: 0,
                    column: 0,
                    selection_start: None,
                    search: None,
                    matches: Vec::new(),
                    current: None,
                });
                if mouse.button == 0 {
                    copy.offset = (copy.offset + 3).min(pane.terminal.history_len());
                } else {
                    copy.offset = copy.offset.saturating_sub(3);
                    if copy.offset == 0 {
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

    /// The OSC 8 link on a pane's grid cell, if any.
    fn link_at_cell(&self, pane_id: PaneId, cell: (isize, usize)) -> Option<TerminalHyperlink> {
        let pane = self.panes.get(&pane_id)?;
        let line = pane.terminal.viewport_line(cell.0)?;
        line.get(cell.1)?.hyperlink.clone()
    }

    /// The OSC 8 link at a pane-content coordinate, if any.
    fn link_at(&self, pane_id: PaneId, content: Rect, x: u16, y: u16) -> Option<TerminalHyperlink> {
        let pane = self.panes.get(&pane_id)?;
        let display_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let cell = mouse_selection_cell(content, x, y, display_offset)?;
        self.link_at_cell(pane_id, cell)
    }

    /// Open a clicked link on the host vvmux itself runs on.
    ///
    /// Only reached in `open = "local"`. The default delegates instead, because vvmux is often the
    /// remote end of an ssh session where the browser would come up on the wrong machine.
    fn open_link_locally(&mut self, uri: &str) {
        // A double click is two press/release pairs on one cell, so without a cooldown it would
        // launch the handler twice.
        let now = Instant::now();
        if self
            .last_link_open
            .is_some_and(|last| now.duration_since(last) < LINK_OPEN_COOLDOWN)
        {
            return;
        }
        // The URI comes from whatever wrote to the pane, so it is untrusted input. Passing it as a
        // single argv element keeps it out of any shell, and only a vetted scheme is handed over at
        // all — an OSC 8 link is not constrained by the URL regex that guards text matches, so
        // `file:` and friends would otherwise be one click from launching a local handler.
        if !is_openable_uri(uri) {
            self.notice(format!("refused to open unsupported link: {uri}"));
            return;
        }
        self.last_link_open = Some(now);
        match crate::platform::open_external(uri) {
            Ok(()) => self.notice(format!("opening {uri}")),
            Err(error) => self.notice(format!("could not open link: {error}")),
        }
    }

    /// Record the link under the pointer, redrawing when it changes.
    fn set_hovered_link(&mut self, hovered: Option<HoveredLink>) {
        if self.hovered_link == hovered {
            return;
        }
        self.hovered_link = hovered;
        // No `force_full`: the hover mark is applied to cells during composition, so the ordinary
        // cell diff already repaints exactly the run that changed. Forcing a full repaint here
        // would rewrite the whole screen on every pointer motion.
        self.schedule_render();
    }

    /// Drop hover state belonging to `pane_id`, leaving any other pane's hover alone.
    fn clear_pane_hover(&mut self, pane_id: PaneId) {
        if self
            .hovered_link
            .as_ref()
            .is_some_and(|hovered| hovered.pane == pane_id)
        {
            self.set_hovered_link(None);
        }
    }

    /// Forward motion/release reports without changing pane focus. These used to return before
    /// application mouse handling, so even a pane in DEC 1003 mode could never hover, drag, or
    /// release a button.
    fn forward_application_mouse(
        &mut self,
        mouse: MouseEvent,
        pixels: Option<(u16, u16)>,
        display: DisplayMetrics,
    ) {
        let area = self.content_area();
        let Some(projection) = self.active_tab().and_then(|tab| {
            visible_projections(tab, area)
                .into_iter()
                .rev()
                .find(|projection| projection.outer.contains(mouse.x, mouse.y))
        }) else {
            self.set_hovered_link(None);
            return;
        };
        let content = projection.outer.content();
        if !content.contains(mouse.x, mouse.y) {
            self.set_hovered_link(None);
            return;
        }
        let pane_id = projection.pane_id;
        let Some(modes) = self.panes.get(&pane_id).map(|pane| pane.terminal.modes()) else {
            self.set_hovered_link(None);
            return;
        };
        let application_mouse = !mouse.shift
            && match mouse.kind {
                MouseKind::Move => modes.mouse_motion,
                MouseKind::Release => modes.mouse_clicks,
                _ => false,
            };
        // Hover only where vvmux is the one reading the mouse. A pane running a full-screen
        // application that asked for motion reports owns those events, exactly as vvmux owns them
        // from the outer terminal; tracking hover anyway would mark links the pane cannot activate.
        if mouse.kind == MouseKind::Move {
            let hovered = (!application_mouse && self.config.hyperlinks.enabled)
                .then(|| self.link_at(pane_id, content, mouse.x, mouse.y))
                .flatten()
                .map(|link| HoveredLink {
                    pane: pane_id,
                    link,
                });
            self.set_hovered_link(hovered);
        }
        if !application_mouse {
            return;
        }
        let mut button = u16::from(mouse.button);
        if mouse.kind == MouseKind::Move {
            button |= 32;
        }
        let terminator = if mouse.kind == MouseKind::Release {
            'm'
        } else {
            'M'
        };
        let (x, y) =
            application_mouse_coordinates(mouse, pixels, content, display, modes.sgr_pixels);
        self.send_pane_input(
            pane_id,
            format!("\x1b[<{button};{x};{y}{terminator}").as_bytes(),
        );
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
        if self.invalidate_mouse_selection_state() {
            self.schedule_render();
        }
        // Any prefix action during a float-edit mode invalidates it (focus, tab, zoom, and
        // layout changes are all cancellation triggers); restore the entry rectangle first.
        self.end_float_mode(true);
        match action {
            Action::ToggleAgentNavigator => {
                self.toggle_agent_navigator();
                return;
            }
            Action::ToggleTabNavigator => {
                self.toggle_tab_navigator();
                return;
            }
            Action::BeginRenameTab => {
                self.begin_tab_rename();
                return;
            }
            Action::BeginClosePaneConfirmation => {
                self.begin_close_pane_confirmation();
                return;
            }
            Action::BeginSaveLayout => {
                self.begin_save_layout();
                return;
            }
            Action::ResolveClosePaneConfirmation(confirmed) => {
                self.resolve_close_pane_confirmation(confirmed);
                return;
            }
            _ => {}
        }
        if self.transient_ui_active() {
            return;
        }
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
            Action::ToggleSyncInput => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.sync_input = !tab.sync_input;
                    self.session_sequence = self.session_sequence.wrapping_add(1);
                    self.force_full = true;
                    self.schedule_render();
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
                        search: None,
                        matches: Vec::new(),
                        current: None,
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
            Action::TogglePaneTransparency => self.toggle_transparency(),
            Action::EnterFloatingMoveMode => self.enter_float_mode(FloatingEditKind::Move),
            Action::EnterFloatingResizeMode => self.enter_float_mode(FloatingEditKind::Resize),
            Action::Plugin(reference) => self.start_plugin_action(reference),
            _ => {}
        }
    }

    fn start_plugin_action(&mut self, reference: String) {
        let Some(invocation) = reference.strip_prefix("plugin:").map(ToOwned::to_owned) else {
            self.status("invalid plugin action reference");
            return;
        };
        if !valid_invocation_reference(&invocation) {
            self.status("invalid plugin action reference");
            return;
        }
        let Some(supervisor) = self.plugin_supervisor.clone() else {
            self.status("plugin action rejected: plugins are disabled in this session");
            return;
        };
        if let Err(error) = supervisor.invoke_notice(invocation, serde_json::json!({}), reference) {
            self.status(&format!("plugin action rejected: {}", error.message));
        }
    }

    fn execute_session_command(
        &mut self,
        caller: &CallerContext,
        command: SessionCommand,
    ) -> Result<serde_json::Value, AutomationError> {
        authorize_session_scope(caller, &self.session_instance)?;
        match command {
            SessionCommand::InspectSession => {
                authorize_session_capability(caller, vvmux_plugin_api::Permission::SessionRead)?;
                let panes = self
                    .panes
                    .keys()
                    .copied()
                    .filter_map(|pane| self.pane_description(pane))
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({
                    "session": self.name,
                    "session_instance": self.session_instance,
                    "session_sequence": self.session_sequence,
                    "actor_wakeups": self.actor_wakeups,
                    "layout_sequence": self.layout_revision,
                    "rendered_session_sequence": self.rendered_session_sequence(),
                    "panes": panes,
                }))
            }
            SessionCommand::ReadPaneText {
                pane_id,
                rows,
                max_bytes,
            } => {
                authorize_session_capability(caller, vvmux_plugin_api::Permission::PaneRead)?;
                let pane_id = self.resolve_session_command_pane(caller, pane_id)?;
                let pane = self.panes.get(&pane_id).ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
                })?;
                let text = rows.map_or_else(
                    || {
                        pane.terminal
                            .visible_text(pane.copy.as_ref().map_or(0, |copy| copy.offset))
                    },
                    |rows| pane.terminal.latest_text(rows),
                );
                if text.len() > max_bytes {
                    return Err(AutomationError::new(
                        "limit_exceeded",
                        "pane text exceeds the bounded command result",
                    ));
                }
                Ok(serde_json::Value::String(text))
            }
            SessionCommand::WritePaneInput { pane_id, bytes } => {
                authorize_session_capability(caller, vvmux_plugin_api::Permission::PaneInput)?;
                let pane_id = self.resolve_session_command_pane(caller, pane_id)?;
                if bytes.len() > 1024 * 1024 {
                    return Err(AutomationError::new(
                        "limit_exceeded",
                        "PTY input exceeds 1 MiB",
                    ));
                }
                let pane = self.panes.get_mut(&pane_id).ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
                })?;
                pane.input.send(&bytes).map_err(|error| {
                    AutomationError::new("runtime_unavailable", error.to_string())
                })?;
                if let Some(cause) = self.active_plugin_cause.clone() {
                    self.pending_pane_plugin_causes.insert(pane_id, cause);
                }
                Ok(serde_json::Value::Null)
            }
            SessionCommand::OpenPluginPane { launch } => {
                if !self.config.plugins.enabled || self.plugin_supervisor.is_none() {
                    return Err(plugin_disabled_error());
                }
                authorize_session_capability(caller, vvmux_plugin_api::Permission::PaneCreate)?;
                let (plugin_id, plugin_instance) = match &caller.origin {
                    CallerOrigin::Plugin {
                        plugin_id,
                        plugin_instance,
                    } => (plugin_id, plugin_instance),
                    CallerOrigin::Automation { .. } => {
                        return Err(AutomationError::new(
                            "scope_denied",
                            "plugin pane launch requires a broker-owned plugin identity",
                        ));
                    }
                };
                if plugin_id != &launch.scope.plugin_id
                    || plugin_instance != &launch.scope.plugin_instance
                    || caller.session_instance != launch.scope.session_instance
                {
                    return Err(AutomationError::new(
                        "scope_denied",
                        "plugin pane launch identity does not match its caller",
                    ));
                }
                let anchor = self.active_tab().map(|tab| tab.focused).ok_or_else(|| {
                    AutomationError::new("pane_not_found", "session has no pane to anchor launch")
                })?;
                let identity = PluginPaneIdentity {
                    session_instance: caller.session_instance.clone(),
                    plugin_id: plugin_id.clone(),
                    plugin_instance: plugin_instance.clone(),
                    package_digest: launch.package_digest.clone(),
                    entrypoint_id: launch.pane.id.clone(),
                    title: launch.pane.title.clone(),
                    accept_sync_input: launch.pane.accept_sync_input,
                };
                let placement = match launch.pane.placement {
                    vvmux_plugin_api::Placement::Split => crate::ipc::RunPlacement::Split {
                        axis: crate::ipc::Axis::Vertical,
                    },
                    vvmux_plugin_api::Placement::Float => crate::ipc::RunPlacement::Float,
                    vvmux_plugin_api::Placement::Tab => crate::ipc::RunPlacement::Tab,
                };
                let vivid_capability = caller
                    .capabilities
                    .contains(&vvmux_plugin_api::Permission::MediaProduce);
                let mut extra_env = vec![
                    ("VVMUX_PLUGIN_ID".into(), plugin_id.clone()),
                    ("VVMUX_PLUGIN_INSTANCE".into(), plugin_instance.clone()),
                    ("VVMUX_PLUGIN_PANE".into(), launch.pane.id.clone()),
                ];
                if vivid_capability && let Some(helper) = launch.vivi_helper.as_ref() {
                    extra_env.extend([
                        (
                            "VVMUX_VIVI_BIN".into(),
                            helper.to_string_lossy().into_owned(),
                        ),
                        ("VVMUX_VIVI_PROTOCOL_VERSION".into(), "1.5".into()),
                    ]);
                }
                let spec = PaneSpawn {
                    command: None,
                    argv: Some(launch.pane.command.iter().map(OsString::from).collect()),
                    cwd: Some(launch.package_root),
                    transparent: None,
                    hold_on_exit: launch.pane.hold_on_exit,
                    extra_env,
                    role: PaneRole::Plugin(identity),
                    vivid_capability,
                };
                let mut result = self.place_pane(
                    anchor,
                    spec,
                    placement,
                    true,
                    (launch.pane.placement == vvmux_plugin_api::Placement::Tab)
                        .then(|| launch.pane.title.clone()),
                )?;
                if let Some(object) = result.as_object_mut() {
                    object.insert(
                        "plugin_id".into(),
                        serde_json::Value::String(plugin_id.clone()),
                    );
                    object.insert(
                        "plugin_instance".into(),
                        serde_json::Value::String(plugin_instance.clone()),
                    );
                    object.insert(
                        "entrypoint_id".into(),
                        serde_json::Value::String(launch.pane.id),
                    );
                    object.insert("vivid".into(), serde_json::Value::Bool(vivid_capability));
                }
                Ok(result)
            }
            SessionCommand::ClosePane { pane_id } => {
                let pane = self.panes.get(&pane_id).ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
                })?;
                let owns = caller_owns_plugin_pane(caller, &pane.role);
                if owns {
                    if !caller
                        .capabilities
                        .contains(&vvmux_plugin_api::Permission::PaneManageAny)
                    {
                        authorize_session_capability(
                            caller,
                            vvmux_plugin_api::Permission::PaneManageOwn,
                        )?;
                    }
                } else {
                    authorize_session_capability(
                        caller,
                        vvmux_plugin_api::Permission::PaneManageAny,
                    )?;
                }
                self.close_pane(pane_id);
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn resolve_session_command_pane(
        &self,
        caller: &CallerContext,
        pane_id: Option<PaneId>,
    ) -> Result<PaneId, AutomationError> {
        pane_id
            .or_else(|| {
                caller
                    .focused_fallback
                    .then(|| self.active_tab().map(|tab| tab.focused))
                    .flatten()
            })
            .ok_or_else(|| {
                AutomationError::new(
                    "pane_required",
                    "command requires an explicit pane or focused fallback",
                )
            })
    }

    fn handle_plugin_host_call(
        &mut self,
        scope: &crate::plugin_supervisor::RuntimeScope,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AutomationError> {
        if !self.config.plugins.enabled || self.plugin_supervisor.is_none() {
            return Err(plugin_disabled_error());
        }
        let caller = CallerContext {
            origin: CallerOrigin::Plugin {
                plugin_id: scope.plugin_id.clone(),
                plugin_instance: scope.plugin_instance.clone(),
            },
            session_instance: scope.session_instance.clone(),
            focused_fallback: false,
            capabilities: scope.permissions.iter().copied().collect(),
        };
        if method != "pane.close" {
            let required = plugin_host_permission(method).ok_or_else(|| {
                AutomationError::new(
                    "action_not_found",
                    format!("unknown plugin host call `{method}`"),
                )
            })?;
            authorize_session_capability(&caller, required)?;
        }
        match method {
            "session.inspect" => {
                require_plugin_params(&params, &[])?;
                self.execute_session_command(&caller, SessionCommand::InspectSession)
            }
            "pane.get_text" => {
                require_plugin_params(&params, &["pane_id", "rows"])?;
                let pane_id = plugin_u64_param(&params, "pane_id")?;
                let rows = plugin_optional_u64_param(&params, "rows")?
                    .map(|rows| usize::try_from(rows).unwrap_or(usize::MAX));
                if rows.is_some_and(|rows| !(1..=1000).contains(&rows)) {
                    return Err(AutomationError::new(
                        "invalid_params",
                        "rows must be from 1 through 1000",
                    ));
                }
                self.execute_session_command(
                    &caller,
                    SessionCommand::ReadPaneText {
                        pane_id: Some(pane_id),
                        rows,
                        max_bytes: vvmux_plugin_api::MAX_FRAME_BYTES / 2,
                    },
                )
            }
            "pane.input" => {
                require_plugin_params(&params, &["pane_id", "text"])?;
                let pane_id = plugin_u64_param(&params, "pane_id")?;
                let text = params
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AutomationError::new("invalid_params", "text must be a string")
                    })?;
                self.execute_session_command(
                    &caller,
                    SessionCommand::WritePaneInput {
                        pane_id: Some(pane_id),
                        bytes: text.as_bytes().to_vec(),
                    },
                )
            }
            "pane.close" => {
                require_plugin_params(&params, &["pane_id"])?;
                let pane_id = plugin_u64_param(&params, "pane_id")?;
                self.execute_session_command(&caller, SessionCommand::ClosePane { pane_id })
            }
            _ => Err(AutomationError::new(
                "action_not_found",
                format!("unknown plugin host call `{method}`"),
            )),
        }
    }

    fn transient_ui_active(&self) -> bool {
        self.agent_navigator.is_some()
            || self.tab_navigator.is_some()
            || self.tab_rename.is_some()
            || self.close_pane_confirmation.is_some()
            || self.save_layout_prompt.is_some()
    }

    fn clear_transient_ui(&mut self) -> bool {
        let active = self.transient_ui_active();
        self.agent_navigator = None;
        self.tab_navigator = None;
        self.tab_rename = None;
        self.close_pane_confirmation = None;
        self.save_layout_prompt = None;
        active
    }

    fn agent_navigator_rows(&self) -> Vec<AgentNavigatorRow> {
        let mut rows = Vec::new();
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            let tab_label = tab
                .name
                .as_ref()
                .map(|name| format!("tab {} {name}", tab_index + 1))
                .unwrap_or_else(|| format!("tab {}", tab_index + 1));
            let mut pane_ids = tab.tree.as_ref().map_or_else(Vec::new, TiledNode::pane_ids);
            pane_ids.extend(tab.floating.pane_ids());
            pane_ids.sort_unstable();
            pane_ids.dedup();
            for pane_id in pane_ids {
                let Some(pane) = self.panes.get(&pane_id) else {
                    continue;
                };
                let Some(agent) = pane.agent.snapshot() else {
                    continue;
                };
                rows.push(AgentNavigatorRow {
                    pane_id,
                    tab_index,
                    tab_label: tab_label.clone(),
                    title: pane
                        .terminal
                        .title()
                        .map_or_else(|| format!("pane {pane_id}"), ToOwned::to_owned),
                    agent,
                });
            }
        }
        rows.sort_by_key(|row| (row.agent.status.urgency(), row.tab_index, row.pane_id));
        rows
    }

    fn toggle_agent_navigator(&mut self) {
        if self.agent_navigator.take().is_some() {
            self.schedule_render();
            return;
        }
        self.clear_transient_ui();
        let rows = self.agent_navigator_rows();
        let focused = self.active_tab().map(|tab| tab.focused);
        let selected = focused
            .filter(|pane| rows.iter().any(|row| row.pane_id == *pane))
            .or_else(|| rows.first().map(|row| row.pane_id));
        let selected_index = selected
            .and_then(|pane| rows.iter().position(|row| row.pane_id == pane))
            .unwrap_or(0);
        self.agent_navigator = Some(AgentNavigator {
            selected,
            selected_index,
            scroll: 0,
        });
        self.force_full = true;
        self.schedule_render();
    }

    fn agent_navigator_input(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() && self.agent_navigator.is_some() {
            let (consumed, key) = decode_agent_navigator_key(&bytes[offset..]);
            offset += consumed;
            match key {
                Some(AgentNavigatorKey::Up) => self.move_agent_navigator(-1),
                Some(AgentNavigatorKey::Down) => self.move_agent_navigator(1),
                Some(AgentNavigatorKey::Home) => self.move_agent_navigator_to(false),
                Some(AgentNavigatorKey::End) => self.move_agent_navigator_to(true),
                Some(AgentNavigatorKey::PageUp) => self.page_agent_navigator(false),
                Some(AgentNavigatorKey::PageDown) => self.page_agent_navigator(true),
                Some(AgentNavigatorKey::Activate) => self.activate_agent_navigator(),
                Some(AgentNavigatorKey::Close) => {
                    self.agent_navigator = None;
                    self.force_full = true;
                    self.schedule_render();
                }
                None => {}
            }
        }
    }

    fn move_agent_navigator(&mut self, delta: isize) {
        let rows = self.agent_navigator_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .agent_navigator
            .and_then(|navigator| navigator.selected)
            .and_then(|pane| rows.iter().position(|row| row.pane_id == pane))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(rows.len().saturating_sub(1));
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = Some(rows[next].pane_id);
            navigator.selected_index = next;
        }
        self.reveal_agent_navigator_selection(rows.len(), next);
    }

    fn move_agent_navigator_to(&mut self, end: bool) {
        let rows = self.agent_navigator_rows();
        let Some(index) = (!rows.is_empty()).then(|| if end { rows.len() - 1 } else { 0 }) else {
            return;
        };
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = Some(rows[index].pane_id);
            navigator.selected_index = index;
        }
        self.reveal_agent_navigator_selection(rows.len(), index);
    }

    fn page_agent_navigator(&mut self, down: bool) {
        let page = agent_navigator_rect(self.content_area(), self.agent_navigator_rows().len())
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        self.move_agent_navigator(if down {
            page as isize
        } else {
            -(page as isize)
        });
    }

    fn reveal_agent_navigator_selection(&mut self, row_count: usize, index: usize) {
        let page = agent_navigator_rect(self.content_area(), row_count)
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        if let Some(navigator) = &mut self.agent_navigator {
            if index < navigator.scroll {
                navigator.scroll = index;
            } else if index >= navigator.scroll.saturating_add(page) {
                navigator.scroll = index + 1 - page;
            }
            navigator.scroll = navigator.scroll.min(row_count.saturating_sub(page));
        }
        self.schedule_render();
    }

    fn activate_agent_navigator(&mut self) {
        let selected = self
            .agent_navigator
            .and_then(|navigator| navigator.selected);
        self.agent_navigator = None;
        let Some(pane_id) = selected else {
            self.force_full = true;
            self.schedule_render();
            return;
        };
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.agent.mark_seen();
        }
        if let Err(error) = self.automation_focus(pane_id) {
            self.status(&error.message);
        }
        self.force_full = true;
        self.schedule_render();
    }

    fn agent_navigator_mouse(&mut self, mouse: MouseEvent) {
        let rows = self.agent_navigator_rows();
        let Some(rect) = agent_navigator_rect(self.content_area(), rows.len()) else {
            self.agent_navigator = None;
            return;
        };
        if mouse.kind == MouseKind::Wheel {
            self.move_agent_navigator(if mouse.button == 0 { -1 } else { 1 });
            return;
        }
        if mouse.kind != MouseKind::Press || mouse.button != 0 {
            return;
        }
        if !rect.contains(mouse.x, mouse.y) {
            self.agent_navigator = None;
            self.force_full = true;
            self.schedule_render();
            return;
        }
        if mouse.y <= rect.y || mouse.y + 1 >= rect.y + rect.height {
            return;
        }
        let scroll = self.agent_navigator.map_or(0, |navigator| navigator.scroll);
        let index = scroll + usize::from(mouse.y - rect.y - 1);
        let Some(row) = rows.get(index) else { return };
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = Some(row.pane_id);
            navigator.selected_index = index;
        }
        self.activate_agent_navigator();
    }

    fn draw_agent_navigator(
        &mut self,
        screen: &mut ScreenBuffer,
        theme: crate::theme::ResolvedTheme,
    ) {
        let rows = self.agent_navigator_rows();
        let Some(rect) = agent_navigator_rect(self.content_area(), rows.len()) else {
            self.agent_navigator = None;
            return;
        };
        let page = usize::from(rect.height.saturating_sub(2)).max(1);
        let selected_index = self.agent_navigator.and_then(|navigator| {
            navigator
                .selected
                .and_then(|pane| rows.iter().position(|row| row.pane_id == pane))
                .or_else(|| {
                    (!rows.is_empty()).then_some(navigator.selected_index.min(rows.len() - 1))
                })
        });
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = selected_index.map(|index| rows[index].pane_id);
            navigator.selected_index = selected_index.unwrap_or(0);
            navigator.scroll = navigator.scroll.min(rows.len().saturating_sub(page));
            if let Some(index) = selected_index {
                if index < navigator.scroll {
                    navigator.scroll = index;
                } else if index >= navigator.scroll + page {
                    navigator.scroll = index + 1 - page;
                }
            }
        }
        screen.draw_frame(rect, " Agents ", theme.frame(true));
        let style = theme.status();
        let inner_width = usize::from(rect.width.saturating_sub(2));
        let blank = " ".repeat(inner_width);
        for offset in 0..page {
            let y = rect.y + 1 + offset as u16;
            screen.draw_text(rect.x + 1, y, &blank, style);
        }
        if rows.is_empty() {
            screen.draw_text(rect.x + 2, rect.y + 1, "No detected AI agent panes", style);
        } else {
            let scroll = self.agent_navigator.map_or(0, |navigator| navigator.scroll);
            for (offset, row) in rows.iter().skip(scroll).take(page).enumerate() {
                let text = format!(
                    "[{:<7}] {:<8} {} · pane {} · {}",
                    row.agent.status.label().to_ascii_uppercase(),
                    row.agent.label,
                    single_line(&row.tab_label),
                    row.pane_id,
                    single_line(&row.title),
                );
                let y = rect.y + 1 + offset as u16;
                screen.draw_text(rect.x + 1, y, &text, style);
                if self
                    .agent_navigator
                    .and_then(|navigator| navigator.selected)
                    == Some(row.pane_id)
                {
                    screen.invert(rect.x + 1, y, rect.width.saturating_sub(2));
                }
            }
        }
        screen.cursor = None;
    }

    fn tab_navigator_rows(&self) -> Vec<TabNavigatorRow> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(display_index, tab)| {
                let tiled = tab.tree.as_ref().map_or(0, |tree| tree.pane_ids().len());
                TabNavigatorRow {
                    tab_id: tab.id,
                    display_index,
                    name: tab.name.clone(),
                    pane_count: tiled + tab.floating.pane_ids().len(),
                    active: display_index == self.active_tab,
                }
            })
            .collect()
    }

    fn toggle_tab_navigator(&mut self) {
        if self.tab_navigator.take().is_some() {
            self.force_full = true;
            self.schedule_render();
            return;
        }
        self.clear_transient_ui();
        let selected = self.active_tab().map(|tab| tab.id);
        self.tab_navigator = Some(TabNavigator {
            selected,
            selected_index: self.active_tab,
            scroll: 0,
        });
        self.force_full = true;
        self.schedule_render();
    }

    fn tab_navigator_input(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() && self.tab_navigator.is_some() {
            let (consumed, key) = decode_agent_navigator_key(&bytes[offset..]);
            offset += consumed;
            match key {
                Some(AgentNavigatorKey::Up) => self.move_tab_navigator(-1),
                Some(AgentNavigatorKey::Down) => self.move_tab_navigator(1),
                Some(AgentNavigatorKey::Home) => self.move_tab_navigator_to(false),
                Some(AgentNavigatorKey::End) => self.move_tab_navigator_to(true),
                Some(AgentNavigatorKey::PageUp) => self.page_tab_navigator(false),
                Some(AgentNavigatorKey::PageDown) => self.page_tab_navigator(true),
                Some(AgentNavigatorKey::Activate) => self.activate_tab_navigator(),
                Some(AgentNavigatorKey::Close) => {
                    self.tab_navigator = None;
                    self.force_full = true;
                    self.schedule_render();
                }
                None => {}
            }
        }
    }

    fn move_tab_navigator(&mut self, delta: isize) {
        let rows = self.tab_navigator_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .tab_navigator
            .and_then(|navigator| navigator.selected)
            .and_then(|tab_id| rows.iter().position(|row| row.tab_id == tab_id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(rows.len().saturating_sub(1));
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = Some(rows[next].tab_id);
            navigator.selected_index = next;
        }
        self.reveal_tab_navigator_selection(rows.len(), next);
    }

    fn move_tab_navigator_to(&mut self, end: bool) {
        let rows = self.tab_navigator_rows();
        let Some(index) = (!rows.is_empty()).then(|| if end { rows.len() - 1 } else { 0 }) else {
            return;
        };
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = Some(rows[index].tab_id);
            navigator.selected_index = index;
        }
        self.reveal_tab_navigator_selection(rows.len(), index);
    }

    fn page_tab_navigator(&mut self, down: bool) {
        let page = tab_navigator_rect(self.content_area(), self.tabs.len())
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        self.move_tab_navigator(if down {
            page as isize
        } else {
            -(page as isize)
        });
    }

    fn reveal_tab_navigator_selection(&mut self, row_count: usize, index: usize) {
        let page = tab_navigator_rect(self.content_area(), row_count)
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        if let Some(navigator) = &mut self.tab_navigator {
            if index < navigator.scroll {
                navigator.scroll = index;
            } else if index >= navigator.scroll.saturating_add(page) {
                navigator.scroll = index + 1 - page;
            }
            navigator.scroll = navigator.scroll.min(row_count.saturating_sub(page));
        }
        self.schedule_render();
    }

    fn activate_tab_navigator(&mut self) {
        let selected = self.tab_navigator.and_then(|navigator| navigator.selected);
        self.tab_navigator = None;
        let Some(index) =
            selected.and_then(|tab_id| self.tabs.iter().position(|tab| tab.id == tab_id))
        else {
            self.force_full = true;
            self.schedule_render();
            return;
        };
        if index == self.active_tab {
            self.force_full = true;
            self.schedule_render();
        } else {
            self.active_tab = index;
            self.force_full = true;
            self.relayout();
        }
    }

    fn tab_navigator_mouse(&mut self, mouse: MouseEvent) {
        let rows = self.tab_navigator_rows();
        let Some(rect) = tab_navigator_rect(self.content_area(), rows.len()) else {
            self.tab_navigator = None;
            return;
        };
        if mouse.kind == MouseKind::Wheel {
            self.move_tab_navigator(if mouse.button == 0 { -1 } else { 1 });
            return;
        }
        if mouse.kind != MouseKind::Press || mouse.button != 0 {
            return;
        }
        if !rect.contains(mouse.x, mouse.y) {
            self.tab_navigator = None;
            self.force_full = true;
            self.schedule_render();
            return;
        }
        if mouse.y <= rect.y || mouse.y + 1 >= rect.y + rect.height {
            return;
        }
        let scroll = self.tab_navigator.map_or(0, |navigator| navigator.scroll);
        let index = scroll + usize::from(mouse.y - rect.y - 1);
        let Some(row) = rows.get(index) else { return };
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = Some(row.tab_id);
            navigator.selected_index = index;
        }
        self.activate_tab_navigator();
    }

    fn draw_tab_navigator(
        &mut self,
        screen: &mut ScreenBuffer,
        theme: crate::theme::ResolvedTheme,
    ) {
        let rows = self.tab_navigator_rows();
        let Some(rect) = tab_navigator_rect(self.content_area(), rows.len()) else {
            self.tab_navigator = None;
            return;
        };
        let page = usize::from(rect.height.saturating_sub(2)).max(1);
        let selected_index = self.tab_navigator.and_then(|navigator| {
            navigator
                .selected
                .and_then(|tab_id| rows.iter().position(|row| row.tab_id == tab_id))
                .or_else(|| {
                    (!rows.is_empty()).then_some(navigator.selected_index.min(rows.len() - 1))
                })
        });
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = selected_index.map(|index| rows[index].tab_id);
            navigator.selected_index = selected_index.unwrap_or(0);
            navigator.scroll = navigator.scroll.min(rows.len().saturating_sub(page));
            if let Some(index) = selected_index {
                if index < navigator.scroll {
                    navigator.scroll = index;
                } else if index >= navigator.scroll + page {
                    navigator.scroll = index + 1 - page;
                }
            }
        }
        screen.draw_frame(rect, " Tabs ", theme.frame(true));
        let style = theme.status();
        let inner_width = usize::from(rect.width.saturating_sub(2));
        let blank = " ".repeat(inner_width);
        for offset in 0..page {
            screen.draw_text(rect.x + 1, rect.y + 1 + offset as u16, &blank, style);
        }
        let scroll = self.tab_navigator.map_or(0, |navigator| navigator.scroll);
        for (offset, row) in rows.iter().skip(scroll).take(page).enumerate() {
            let marker = if row.active { '*' } else { ' ' };
            let name = row
                .name
                .as_deref()
                .map(single_line)
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "(unnamed)".to_owned());
            let text = format!(
                "{marker} {:>2}: {name} · panes:{}",
                row.display_index + 1,
                row.pane_count
            );
            let y = rect.y + 1 + offset as u16;
            screen.draw_text(rect.x + 1, y, &text, style);
            if self.tab_navigator.and_then(|navigator| navigator.selected) == Some(row.tab_id) {
                screen.invert(rect.x + 1, y, rect.width.saturating_sub(2));
            }
        }
        screen.cursor = None;
    }

    fn begin_tab_rename(&mut self) {
        let Some((tab_id, name)) = self
            .active_tab()
            .map(|tab| (tab.id, tab.name.clone().unwrap_or_default()))
        else {
            return;
        };
        self.clear_transient_ui();
        self.tab_rename = Some(TabRename {
            tab_id,
            value: truncate_utf8(single_line(&name), MAX_TAB_NAME_BYTES),
            pending_utf8: Vec::new(),
        });
        self.force_full = true;
        self.schedule_render();
    }

    fn tab_rename_input(&mut self, bytes: &[u8]) {
        let Some(mut rename) = self.tab_rename.take() else {
            return;
        };
        let action = apply_tab_rename_input(&mut rename, bytes);
        match action {
            LineEditInput::Editing => {
                if self.tabs.iter().any(|tab| tab.id == rename.tab_id) {
                    self.tab_rename = Some(rename);
                }
                self.schedule_render();
            }
            LineEditInput::Cancel => {
                self.force_full = true;
                self.schedule_render();
            }
            LineEditInput::Commit => {
                let name = rename.value.trim();
                let next = (!name.is_empty()).then(|| name.to_owned());
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == rename.tab_id)
                    && tab.name != next
                {
                    tab.name = next;
                    self.session_sequence = self.session_sequence.wrapping_add(1);
                }
                self.force_full = true;
                self.schedule_render();
            }
        }
    }

    fn begin_close_pane_confirmation(&mut self) {
        let Some((tab_id, pane_id)) = self.active_tab().map(|tab| (tab.id, tab.focused)) else {
            return;
        };
        self.clear_transient_ui();
        self.close_pane_confirmation = Some(ClosePaneConfirmation { tab_id, pane_id });
        self.force_full = true;
        self.schedule_render();
    }

    fn resolve_close_pane_confirmation(&mut self, confirmed: bool) {
        let Some(confirmation) = self.close_pane_confirmation.take() else {
            return;
        };
        if confirmed
            && self
                .tabs
                .iter()
                .any(|tab| tab.id == confirmation.tab_id && tab.contains(confirmation.pane_id))
        {
            self.close_pane(confirmation.pane_id);
        } else {
            self.force_full = true;
            self.schedule_render();
        }
    }

    fn begin_save_layout(&mut self) {
        self.clear_transient_ui();
        self.save_layout_prompt = Some(SaveLayoutPrompt {
            stage: SaveLayoutStage::Editing {
                value: crate::layout_file::STARTUP_FILE.to_owned(),
            },
            pending_utf8: Vec::new(),
        });
        self.force_full = true;
        self.schedule_render();
    }

    fn save_layout_prompt_input(&mut self, bytes: &[u8]) {
        let Some(mut prompt) = self.save_layout_prompt.take() else {
            return;
        };
        match &mut prompt.stage {
            SaveLayoutStage::Editing { value } => {
                match apply_line_edit(
                    value,
                    &mut prompt.pending_utf8,
                    MAX_LAYOUT_NAME_BYTES,
                    bytes,
                ) {
                    LineEditInput::Editing => {
                        self.save_layout_prompt = Some(prompt);
                        self.schedule_render();
                    }
                    LineEditInput::Cancel => {
                        self.force_full = true;
                        self.schedule_render();
                    }
                    LineEditInput::Commit => {
                        let target = crate::layout_file::resolve_save_path(value);
                        self.force_full = true;
                        match target {
                            // Replacing a layout the user already has is the one destructive part
                            // of this flow, so it asks first.
                            Ok(path) if path.exists() => {
                                self.save_layout_prompt = Some(SaveLayoutPrompt {
                                    stage: SaveLayoutStage::Confirm { path },
                                    pending_utf8: Vec::new(),
                                });
                                self.schedule_render();
                            }
                            Ok(path) => self.commit_save_layout(&path),
                            Err(error) => self.notice(format!("save failed: {error}")),
                        }
                    }
                }
            }
            SaveLayoutStage::Confirm { path } => {
                let path = path.clone();
                for byte in bytes {
                    match byte {
                        b'y' | b'Y' => {
                            self.force_full = true;
                            self.commit_save_layout(&path);
                            return;
                        }
                        b'n' | b'N' | 0x1b => {
                            self.force_full = true;
                            self.notice("save canceled");
                            return;
                        }
                        _ => {}
                    }
                }
                self.save_layout_prompt = Some(prompt);
            }
        }
    }

    fn commit_save_layout(&mut self, path: &Path) {
        self.save_layout_prompt = None;
        match self.save_layout(path) {
            Ok((tabs, panes)) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let tab_word = if tabs == 1 { "tab" } else { "tabs" };
                let pane_word = if panes == 1 { "pane" } else { "panes" };
                self.notice(format!(
                    "saved {tabs} {tab_word}, {panes} {pane_word} to {name}"
                ));
            }
            Err(error) => self.notice(format!("save failed: {error}")),
        }
    }

    /// Show a short-lived status-row message. Nothing else about the session changes.
    fn notice(&mut self, message: impl Into<String>) {
        self.status_notice = Some(StatusNotice {
            message: single_line(&message.into()),
            expires: Instant::now() + STATUS_NOTICE_DURATION,
        });
        self.force_full = true;
        self.schedule_render();
    }

    fn active_status_notice(&self) -> Option<&str> {
        self.status_notice
            .as_ref()
            .filter(|notice| notice.expires > Instant::now())
            .map(|notice| notice.message.as_str())
    }

    /// Drop an expired notice and repaint once, so the status row returns to the tab list without
    /// waiting for unrelated activity.
    fn expire_status_notice(&mut self) {
        if self
            .status_notice
            .as_ref()
            .is_some_and(|notice| notice.expires <= Instant::now())
        {
            self.status_notice = None;
            self.force_full = true;
            self.schedule_render();
        }
    }

    fn next_notice_deadline(&self) -> Duration {
        self.status_notice
            .as_ref()
            .map(|notice| notice.expires.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::MAX)
    }

    fn close_pane_confirmation_input(&mut self, bytes: &[u8]) {
        for byte in bytes {
            match byte {
                b'y' | b'Y' => {
                    self.resolve_close_pane_confirmation(true);
                    return;
                }
                b'n' | b'N' | 0x1b => {
                    self.resolve_close_pane_confirmation(false);
                    return;
                }
                _ => {}
            }
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
        if self
            .spawn_pane(pane_id, tab_id, &PaneSpawn::default())
            .is_err()
        {
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
        if self
            .spawn_pane(pane_id, tab_id, &PaneSpawn::default())
            .is_err()
        {
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

    /// Flip whether the focused pane paints its own background.
    ///
    /// No full repaint is forced: the substitution changes the composited cells themselves, so the
    /// ordinary per-cell diff already carries it. The status message is worth the line because the
    /// effect is invisible unless the outer terminal is running translucent — without it, a user
    /// whose window is opaque cannot tell the action fired at all.
    fn toggle_transparency(&mut self) {
        let Some(pane_id) = self.active_tab().map(|tab| tab.focused) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        pane.transparent = !pane.transparent;
        let transparent = pane.transparent;
        self.schedule_render();
        self.status(if transparent {
            "pane background: transparent"
        } else {
            "pane background: opaque"
        });
    }

    fn new_tab(&mut self) -> io::Result<()> {
        let pane_id = self.next_pane_id;
        let tab_id = self.next_tab_id;
        self.spawn_pane(pane_id, tab_id, &PaneSpawn::default())?;
        self.next_pane_id += 1;
        self.tabs.push(Tab {
            id: tab_id,
            name: None,
            tree: Some(TiledNode::leaf(pane_id)),
            floating: FloatingLayer::default(),
            focused: pane_id,
            last_focused_tiled: Some(pane_id),
            zoomed: None,
            sync_input: false,
        });
        self.next_tab_id += 1;
        self.schedule_render();
        Ok(())
    }

    fn apply_layout_plan(&mut self, plan: LayoutPlan) -> io::Result<()> {
        let area = self.content_area();
        for planned in plan.tabs {
            let tab_id = self.next_tab_id;
            self.next_tab_id = self.next_tab_id.wrapping_add(1);

            // Allocate every slot before spawning so the planned tree is independent of spawn
            // order. Failed slots consume their scoped IDs and are then closed out of the tree.
            let slot_ids = (0..planned.spawns.len())
                .map(|_| {
                    let pane_id = self.next_pane_id;
                    self.next_pane_id = self.next_pane_id.wrapping_add(1);
                    pane_id
                })
                .collect::<Vec<_>>();
            let mut failed = HashSet::new();
            for (slot, spec) in planned.spawns.iter().enumerate() {
                if self.spawn_pane(slot_ids[slot], tab_id, spec).is_err() {
                    failed.insert(slot_ids[slot]);
                }
            }

            let mut tree = planned.tiled.as_ref().map(|node| node.to_tiled(&slot_ids));
            for pane_id in slot_ids.iter().filter(|pane_id| failed.contains(pane_id)) {
                tree = tree.and_then(|tree| tree.close(*pane_id));
            }

            let mut floating = FloatingLayer::default();
            for (index, planned_float) in planned.floating.iter().enumerate() {
                let pane_id = slot_ids[planned_float.slot];
                if failed.contains(&pane_id) {
                    continue;
                }
                floating.insert(
                    pane_id,
                    area,
                    planned_float.width_percent,
                    planned_float.height_percent,
                );
                floating.set_pinned(pane_id, planned_float.pinned);
                if index != 0
                    && let Some(rect) = floating.get(pane_id).map(|float| float.rect)
                {
                    floating.set_rect(
                        pane_id,
                        Rect {
                            x: rect.x.saturating_add((index as u16).saturating_mul(2)),
                            y: rect.y.saturating_add(index as u16),
                            ..rect
                        },
                        area,
                    );
                }
            }

            if tree.is_none() && floating.is_empty() {
                continue;
            }
            let first_tiled = tree
                .as_ref()
                .and_then(|tree| tree.pane_ids().into_iter().next());
            let requested_focus = planned
                .focus_slot
                .map(|slot| slot_ids[slot])
                .filter(|pane_id| !failed.contains(pane_id));
            let last_focused_tiled = requested_focus
                .filter(|pane_id| tree.as_ref().is_some_and(|tree| tree.contains(*pane_id)))
                .or(first_tiled);
            let focused = requested_focus
                .or_else(|| floating.focus_candidate())
                .or(last_focused_tiled)
                .expect("a surviving layout tab has a focusable pane");
            self.tabs.push(Tab {
                id: tab_id,
                name: planned.name,
                tree,
                floating,
                focused,
                last_focused_tiled,
                zoomed: None,
                sync_input: false,
            });
        }
        if self.tabs.is_empty() {
            return self.new_tab();
        }
        self.active_tab = 0;
        self.schedule_render();
        Ok(())
    }

    /// Describe the live session in the startup-layout schema.
    ///
    /// Only core shell panes are captured: plugin panes are host-owned and must never be revived
    /// as shells, and zoom and synchronized input are projection/tab state rather than layout.
    /// Weights are rescaled into the parser's accepted range while preserving their ratio.
    fn capture_layout(&self) -> io::Result<LayoutFile> {
        let area = self.content_area();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut tabs = Vec::new();
        let mut total_panes = 0_usize;
        for tab in &self.tabs {
            let mut labels: Vec<(PaneId, String)> = Vec::new();
            let tiled = tab
                .tree
                .as_ref()
                .and_then(|tree| self.capture_node(tree, home.as_deref(), &mut labels));
            let mut floating = Vec::new();
            for float in tab.floating.panes() {
                let Some(pane) = self.core_pane(float.pane_id) else {
                    continue;
                };
                let label = format!("p{}", labels.len() + 1);
                labels.push((float.pane_id, label.clone()));
                floating.push(LayoutFloat::new(
                    label,
                    saved_cwd(&pane.spawn_cwd, home.as_deref()),
                    saved_percent(float.rect.width, area.width),
                    saved_percent(float.rect.height, area.height),
                    float.pinned,
                    !pane.transparent,
                ));
            }
            if tiled.is_none() && floating.is_empty() {
                continue;
            }
            let focus = labels
                .iter()
                .find(|(pane_id, _)| *pane_id == tab.focused)
                .map(|(_, label)| label.clone());
            total_panes = total_panes.saturating_add(labels.len());
            if total_panes > MAX_LAYOUT_PANES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a saved layout holds at most {MAX_LAYOUT_PANES} panes"),
                ));
            }
            tabs.push(LayoutTab::new(tab.name.clone(), focus, tiled, floating));
            if tabs.len() > MAX_LAYOUT_TABS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a saved layout holds at most {MAX_LAYOUT_TABS} tabs"),
                ));
            }
        }
        if tabs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "this session has no shell panes to save",
            ));
        }
        Ok(LayoutFile::from_tabs(tabs))
    }

    /// Capture one tiled subtree, collapsing away branches whose panes are not saveable so the
    /// surviving siblings keep their own shape.
    fn capture_node(
        &self,
        node: &TiledNode,
        home: Option<&Path>,
        labels: &mut Vec<(PaneId, String)>,
    ) -> Option<LayoutNode> {
        match node {
            TiledNode::Leaf(pane_id) => {
                let pane = self.core_pane(*pane_id)?;
                let label = format!("p{}", labels.len() + 1);
                labels.push((*pane_id, label.clone()));
                Some(LayoutNode::leaf(
                    label,
                    saved_cwd(&pane.spawn_cwd, home),
                    !pane.transparent,
                ))
            }
            TiledNode::Split {
                axis,
                first,
                second,
                first_weight,
                second_weight,
            } => {
                let captured_first = self.capture_node(first, home, labels);
                let captured_second = self.capture_node(second, home, labels);
                match (captured_first, captured_second) {
                    (Some(first), Some(second)) => Some(LayoutNode::split(
                        *axis,
                        saved_sizes(*first_weight, *second_weight),
                        vec![first, second],
                    )),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    fn core_pane(&self, pane_id: PaneId) -> Option<&Pane> {
        self.panes
            .get(&pane_id)
            .filter(|pane| matches!(pane.role, PaneRole::Core))
    }

    /// Capture the session and replace `path` atomically, reporting the saved tab and pane counts.
    ///
    /// The rendered file is small and bounded by the layout caps, and the writer stays on the
    /// actor for the same reason config reload does: one parse-render-write path, no shared state.
    fn save_layout(&self, path: &Path) -> io::Result<(usize, usize)> {
        let layout = self.capture_layout()?;
        let rendered = layout.render()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        fs::write(&temporary, rendered.as_bytes())?;
        if let Err(error) = crate::runtime::atomic_replace(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(layout.counts())
    }

    fn spawn_pane(&mut self, pane_id: PaneId, tab_id: u64, spec: &PaneSpawn) -> io::Result<()> {
        let shell = self
            .config
            .general
            .shell
            .as_ref()
            .map(|path| OsString::from(path.as_os_str()))
            .or_else(default_shell)
            .unwrap_or_else(fallback_shell);
        #[cfg(windows)]
        let shell = crate::platform::resolve_windows_executable(&shell).unwrap_or(shell);
        let cwd = spec
            .cwd
            .clone()
            .or_else(|| self.config.general.default_cwd.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(fallback_cwd);
        let term = if terminfo_installed() {
            "vvmux"
        } else {
            "xterm-256color"
        };
        let vvmux_bin = std::env::current_exe()?.to_string_lossy().into_owned();
        // The caller's extras go first so the fixed pane identity below always wins: nothing a
        // layout file or `run` supplies may shadow VIVID_ROOT_SECRET or VVMUX_PANE_ID.
        let mut environment: Vec<(String, String)> = spec.extra_env.clone();
        environment.extend([
            ("TERM".into(), term.into()),
            ("TERM_PROGRAM".into(), "vvmux".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("VVMUX_SESSION".into(), self.name.clone()),
            ("VVMUX_TAB_ID".into(), tab_id.to_string()),
            ("VVMUX_PANE_ID".into(), pane_id.to_string()),
            ("VVMUX_BIN".into(), vvmux_bin),
        ]);
        let vivid_capability = if spec.vivid_capability {
            environment.extend([
                ("VIVID_ENDPOINT_CONTROL".into(), self.vivid.endpoint()),
                (
                    "VIVID_ROOT_SECRET".into(),
                    self.vivid.issue_pane_capability(pane_id)?,
                ),
            ]);
            #[cfg(windows)]
            environment.push(("VIVID_ANCHOR_TRANSPORT".into(), "conpty".into()));
            true
        } else {
            false
        };
        // Every failure past `issue_pane_capability` must revoke it: the capability is already
        // minted, and leaving it live would let a dead pane's secret authenticate.
        let spawned = match spec.argv.as_deref() {
            Some([program, arguments @ ..]) => {
                PtyProcess::spawn_argv(program, arguments, &cwd, 80, 22, &environment)
            }
            Some([]) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pane argv must contain a program",
            )),
            None => PtyProcess::spawn(&shell, spec.command.as_deref(), &cwd, 80, 22, &environment),
        };
        let parts = match spawned {
            Ok(parts) => parts,
            Err(error) => {
                if vivid_capability {
                    self.vivid.revoke_pane(pane_id);
                }
                return Err(error);
            }
        };
        let child_pid = parts.child_pid;
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
                child_pid,
                spawn_cwd: cwd,
                agent: AgentRuntime::new(),
                copy: None,
                mouse_selection: None,
                vivid_metrics: None,
                transparent: spec.transparent.unwrap_or(self.config.panes.transparent),
                hold_on_exit: spec.hold_on_exit,
                exit_status: None,
                focus_reported: false,
                last_input_warning: None,
                screen_sequence: 1,
                last_screen_change: Instant::now(),
                screen_changes: VecDeque::new(),
                role: spec.role.clone(),
            },
        );
        self.refresh_agent_detector_targets();
        self.publish_plugin_event(
            "pane.opened",
            serde_json::json!({"pane_id": pane_id, "tab_id": tab_id}),
            Some(pane_id),
            None,
        );
        Ok(())
    }

    fn close_pane(&mut self, pane_id: PaneId) {
        self.invalidate_mouse_selection_for_pane(pane_id);
        self.clear_pane_hover(pane_id);
        if let Some(drag) = &self.pointer_drag {
            self.cancel_pointer_drag(drag.pane() != Some(pane_id));
        }
        if let Some(modal) = self.float_modal {
            // A closing edited pane discards the mode; any other close still invalidates it
            // but restores the entry rectangle.
            self.end_float_mode(modal.pane != pane_id);
        }
        let tab_id = self
            .tabs
            .iter()
            .find(|tab| tab.contains(pane_id))
            .map(|tab| tab.id);
        if let Some(pane) = self.panes.remove(&pane_id) {
            pane.control.terminate();
        }
        self.refresh_agent_detector_targets();
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
        self.tab_rename = self
            .tab_rename
            .take()
            .filter(|rename| self.tabs.iter().any(|tab| tab.id == rename.tab_id));
        self.close_pane_confirmation = self.close_pane_confirmation.take().filter(|confirmation| {
            self.tabs
                .iter()
                .any(|tab| tab.id == confirmation.tab_id && tab.contains(confirmation.pane_id))
        });
        self.force_full = true;
        self.relayout();
        self.publish_plugin_event(
            "pane.closed",
            serde_json::json!({"pane_id": pane_id, "tab_id": tab_id}),
            Some(pane_id),
            None,
        );
    }

    fn close_plugin_panes(&mut self, plugin_id: &str, package_digest: &str) {
        let panes = self
            .panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                plugin_pane_matches_generation(
                    &pane.role,
                    &self.session_instance,
                    plugin_id,
                    package_digest,
                )
                .then_some(*pane_id)
            })
            .collect::<Vec<_>>();
        for pane_id in panes {
            self.close_pane(pane_id);
        }
    }

    fn close_all_plugin_panes(&mut self) {
        let panes = self
            .panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                matches!(pane.role, PaneRole::Plugin(_)).then_some(*pane_id)
            })
            .collect::<Vec<_>>();
        for pane_id in panes {
            self.close_pane(pane_id);
        }
    }

    /// Start the session-scoped plugin machinery after a live false-to-true config transition.
    fn enable_plugin_runtime(&mut self) -> Result<(), String> {
        if self.plugin_supervisor.is_some() {
            return Ok(());
        }
        let supervisor = crate::plugin_supervisor::PluginSupervisor::start(
            self.name.clone(),
            self.session_instance.clone(),
            self.sender.clone(),
        )
        .map_err(|error| format!("could not start plugin supervisor: {error}"))?;
        let watcher_shutdown = Arc::new(AtomicBool::new(false));
        if let Err(error) = crate::plugin::registry_path().and_then(|path| {
            crate::config_watch::spawn_plugin_registry(
                path,
                self.sender.clone(),
                watcher_shutdown.clone(),
                self.plugin_reload_pending.clone(),
            )
        }) {
            watcher_shutdown.store(true, Ordering::Release);
            supervisor.shutdown();
            return Err(format!("could not start plugin registry watcher: {error}"));
        }
        self.plugin_supervisor = Some(supervisor);
        self.plugin_watch_shutdown = Some(watcher_shutdown);
        Ok(())
    }

    fn disable_plugin_runtime(&mut self) {
        if let Some(stop) = self.plugin_watch_shutdown.take() {
            stop.store(true, Ordering::Release);
        }
        self.plugin_reload_pending.store(false, Ordering::Release);
        self.close_all_plugin_panes();
        if let Some(supervisor) = self.plugin_supervisor.take() {
            supervisor.shutdown();
        }
        self.agent_catalog = Arc::new(crate::agent::AgentCatalog::default());
        self.agent_detector
            .replace_catalog(self.agent_catalog.clone());
        for pane in self.panes.values_mut() {
            if pane.agent.reconcile_catalog(&self.agent_catalog) {
                pane.terminal.clear_agent_osc();
            }
        }
        for (_, subscription) in self.plugin_event_subscriptions.drain() {
            subscription.cancel.cancel();
        }
    }

    /// Re-read the config file and adopt what can be adopted without disturbing live state.
    ///
    /// Three settings resist reloading, and each is handled rather than ignored:
    ///
    /// - `[media]` was moved into the running `VirtualVivid` at startup. Swapping it would strand
    ///   live retained media and in-flight tracks, so the running values are carried forward.
    /// - `general.prefix` and `[keys.prefix]` are interpreted by the *client's* prefix parser, and
    ///   `[server]` only by `vvmux serve`. Both are stored, but neither reaches this process's
    ///   behavior until the peer restarts or reattaches.
    /// - `shell`, `default_cwd`, and `scrollback_lines` are read when a pane spawns, so they apply
    ///   to the next pane rather than existing ones. `default_layout` is a next-session setting.
    ///
    /// A parse or validation failure leaves the running config completely untouched: a config
    /// saved mid-edit must never degrade a live session.
    fn reload_config(&mut self) -> Result<ReloadReport, String> {
        let path = self
            .config_path
            .clone()
            .or_else(crate::config::default_path)
            .ok_or_else(|| "no config file could be resolved for this session".to_owned())?;
        let source = std::fs::read_to_string(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                format!("{} does not exist", path.display())
            } else {
                format!("could not read {}: {error}", path.display())
            }
        })?;
        let mut next = crate::config::Config::parse(&source, &path).map_err(|error| {
            // `parse` already names the file and the offending key.
            error.to_string()
        })?;

        let mut report = ReloadReport {
            path: path.display().to_string(),
            applied: Vec::new(),
            ignored: Vec::new(),
            deferred: Vec::new(),
            failed: BTreeMap::new(),
        };

        // MediaConfig comes from vivid_gateway and does not derive PartialEq; comparing the
        // serialized form avoids depending on that.
        let media_changed =
            serde_json::to_value(&next.media).ok() != serde_json::to_value(&self.config.media).ok();
        if media_changed {
            next.media = self.config.media.clone();
            report.ignored.push("media".to_owned());
        }
        if next.general.prefix != self.config.general.prefix {
            report.deferred.push("general.prefix".to_owned());
        }
        if next.keys.prefix != self.config.keys.prefix {
            report.deferred.push("keys.prefix".to_owned());
        }
        if next.general.shell != self.config.general.shell
            || next.general.default_cwd != self.config.general.default_cwd
            || next.general.scrollback_lines != self.config.general.scrollback_lines
        {
            report.deferred.push("general.pane_defaults".to_owned());
        }
        if next.general.default_layout != self.config.general.default_layout {
            report.deferred.push("general.default_layout".to_owned());
        }
        // Only the seed for a newly spawned pane. Panes already open keep whatever they were last
        // toggled to, which a reload has no business overriding.
        if next.panes.transparent != self.config.panes.transparent {
            report.deferred.push("panes.transparent".to_owned());
        }
        let server_changed = serde_json::to_value(&next.server).ok()
            != serde_json::to_value(&self.config.server).ok();
        if server_changed {
            next.server = self.config.server.clone();
            report.ignored.push("server".to_owned());
        }

        if next.plugins.enabled != self.config.plugins.enabled {
            if next.plugins.enabled {
                if let Err(error) = self.enable_plugin_runtime() {
                    next.plugins.enabled = false;
                    report.failed.insert("plugins.enabled".into(), error);
                } else {
                    report.applied.push("plugins.enabled".into());
                }
            } else {
                self.disable_plugin_runtime();
                report.applied.push("plugins.enabled".into());
            }
        }

        let status_changed = next.general.status_visible != self.config.general.status_visible;
        self.config = next;
        self.config_path = Some(path);

        if status_changed {
            // The status row is outside the pane area, so the usable height just changed. The
            // stored displays were normalized against the old setting and must be re-normalized
            // before anything derives geometry from them.
            let status_visible = self.config.general.status_visible;
            self.last_display = normalized_display(self.last_display, status_visible);
            if let Some(client) = &mut self.attached {
                client.display = normalized_display(client.display, status_visible);
            }
            self.force_full = true;
            // One relayout for the whole change: it resizes every PTY and schedules the render.
            self.relayout();
        } else {
            // Colors can change without any geometry moving, and a cell whose only difference is
            // its color still diffs correctly; force_full is simply the cheapest way to be sure.
            self.force_full = true;
            self.schedule_render();
        }
        self.queue_plugin_state_event(
            "config.changed",
            "config".into(),
            serde_json::json!({"path": report.path}),
            None,
        );
        Ok(report)
    }

    fn relayout(&mut self) {
        self.invalidate_mouse_selection_state();
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.resize_all();
        self.queue_plugin_state_event(
            "layout.changed",
            "layout".into(),
            serde_json::json!({"layout_revision": self.layout_revision}),
            None,
        );
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
        let display = self.layout_display();
        let mut resize_failures = Vec::new();
        let mut resized_panes = 0_u64;
        for tab in &self.tabs {
            // Hidden ordinary floats keep consuming PTY output but are not resized while
            // hidden; a re-shown float is resized here on the next relayout if its content
            // dimensions changed.
            for projection in visible_projections(tab, area) {
                if let Some(pane) = self.panes.get_mut(&projection.pane_id) {
                    let content = projection.content;
                    // A pane squeezed to nothing still has a live program behind it, and neither
                    // a terminal grid nor a PTY has a zero dimension. Such a pane keeps a single
                    // cell so the window can shrink past its frame and grow back with the pane
                    // and its program intact.
                    let columns = content.width.max(1);
                    let rows = content.height.max(1);
                    let metrics = (
                        content.width,
                        content.height,
                        display.cell_width,
                        display.cell_height,
                    );
                    let dimensions_changed = pane.terminal.rows() != rows as usize
                        || pane.terminal.cols() != columns as usize;
                    let metrics_changed = pane.vivid_metrics != Some(metrics);
                    if dimensions_changed {
                        pane.terminal.resize(rows as usize, columns as usize);
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
                    }
                    if dimensions_changed || metrics_changed {
                        let pixel_width = u32::from(columns)
                            .checked_mul(u32::from(display.cell_width))
                            .and_then(|value| u16::try_from(value).ok())
                            .unwrap_or(0);
                        let pixel_height = u32::from(rows)
                            .checked_mul(u32::from(display.cell_height))
                            .and_then(|value| u16::try_from(value).ok())
                            .unwrap_or(0);
                        if pane
                            .control
                            .resize_with_pixels(columns, rows, pixel_width, pixel_height)
                            .is_err()
                        {
                            resize_failures.push(projection.pane_id);
                        }
                    }
                    if metrics_changed {
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
            // A resize the PTY refused leaves the pane at its previous size; it is not evidence
            // that the program behind it died. Only its exit closes a pane, so a window the user
            // can drag back open never costs them a shell.
            self.status(&format!("pane {pane} PTY resize failed"));
        }
    }

    fn render(&mut self) {
        self.flush_plugin_state_events();
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
        let theme = self.config.resolved_theme();
        let display = client.display;
        let writer = client.writer.clone();
        let kitty_graphics = client.kitty_graphics;
        #[cfg(windows)]
        let focused_bracketed_paste = self
            .active_tab()
            .and_then(|tab| self.panes.get(&tab.focused))
            .is_some_and(|pane| pane.terminal.modes().bracketed_paste);
        let mut screen = ScreenBuffer::new(display.columns, display.rows);
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
                let title = pane
                    .terminal
                    .title()
                    .map_or_else(|| format!("pane {}", pane.id), ToOwned::to_owned);
                let copy_suffix = pane.copy.as_ref().map(|_| " [copy]").unwrap_or("");
                let search_suffix = pane
                    .copy
                    .as_ref()
                    .and_then(|copy| copy.search.as_ref())
                    .filter(|search| !search.query.is_empty())
                    .map_or_else(String::new, |search| format!(" [search: {}]", search.query));
                let sync_suffix = if tab.sync_input { " [sync]" } else { "" };
                let pin_suffix = if projection.layer == PaneLayer::Pinned {
                    " [pin]"
                } else {
                    ""
                };
                // A held pane outlives its process; say so, or it looks like a live shell that
                // has stopped responding.
                let exit_suffix = pane.exit_status.map(|_| " [exited]").unwrap_or("");
                screen.draw_frame(
                    projection.outer,
                    &format!(
                        " {title}{copy_suffix}{search_suffix}{sync_suffix}{pin_suffix}{exit_suffix} "
                    ),
                    theme.frame(active),
                );
                let content = projection.content;
                let offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
                screen.draw_terminal(content, &pane.terminal, offset);
                if !pane.transparent {
                    // Filling here rather than in a pass over the finished buffer is what keeps
                    // the projection order authoritative: a pane drawn later still overwrites an
                    // opaque pane beneath it. The frame is included so the border does not stay
                    // see-through around solid content.
                    screen.fill_default_background(projection.outer, theme.pane_background);
                }
                if let Some(copy) = &pane.copy {
                    for found in &copy.matches {
                        let row = found.line + copy.offset as isize;
                        if row < 0 || row >= usize::from(content.height) as isize {
                            continue;
                        }
                        let start = found.start_column.min(usize::from(content.width));
                        let width = found
                            .end_column
                            .saturating_sub(found.start_column)
                            .min(usize::from(content.width).saturating_sub(start));
                        let style = if copy.current == Some(*found) {
                            theme.search_current()
                        } else {
                            theme.search_match()
                        };
                        screen.restyle(
                            content.x.saturating_add(start as u16),
                            content.y.saturating_add(row as u16),
                            width as u16,
                            style,
                        );
                    }
                }
                if let Some(selection) = pane.mouse_selection {
                    for (row, column, width) in mouse_selection_runs(
                        &pane.terminal,
                        selection,
                        offset,
                        usize::from(content.width),
                        usize::from(content.height),
                    ) {
                        screen.invert(
                            content.x.saturating_add(column as u16),
                            content.y.saturating_add(row as u16),
                            width as u16,
                        );
                    }
                }
                if self.config.hyperlinks.enabled {
                    // Only the pane under the pointer gets a hovered link, so an identically
                    // targeted link in another pane stays at its resting mark.
                    let hovered = self
                        .hovered_link
                        .as_ref()
                        .filter(|hovered| hovered.pane == projection.pane_id)
                        .map(|hovered| &hovered.link);
                    let resting = self
                        .config
                        .hyperlinks
                        .persistent_style
                        .then(|| LinkStyle::resting(theme.hyperlink));
                    screen.style_links(
                        content,
                        hovered,
                        resting,
                        LinkStyle::hovered(theme.hyperlink),
                    );
                }
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
        if self.agent_navigator.is_some() {
            self.draw_agent_navigator(&mut screen, theme);
        } else if self.tab_navigator.is_some() {
            self.draw_tab_navigator(&mut screen, theme);
        }
        if self.config.general.status_visible && screen.rows > 0 {
            let rename_prompt = self.tab_rename.as_ref().and_then(|rename| {
                self.tabs
                    .iter()
                    .position(|tab| tab.id == rename.tab_id)
                    .map(|index| format!("rename tab {}: {}", index + 1, rename.value))
            });
            let close_prompt = self
                .close_pane_confirmation
                .map(|confirmation| format!("kill pane {}? (y/n)", confirmation.pane_id));
            let save_prompt = self
                .save_layout_prompt
                .as_ref()
                .map(|prompt| match &prompt.stage {
                    SaveLayoutStage::Editing { value } => format!("save layout: {value}"),
                    SaveLayoutStage::Confirm { path } => {
                        format!("overwrite {}? (y/n)", path.display())
                    }
                });
            let search_prompt = self.active_tab().and_then(|tab| {
                self.panes
                    .get(&tab.focused)
                    .and_then(|pane| pane.copy.as_ref())
                    .and_then(|copy| copy.search.as_ref())
                    .and_then(|search| {
                        search.prompt.as_ref().map(|prompt| {
                            let leader = match search.direction {
                                SearchDirection::Forward => '/',
                                SearchDirection::Backward => '?',
                            };
                            format!("{leader}{prompt}")
                        })
                    })
            });
            let prompt_active = rename_prompt.is_some()
                || close_prompt.is_some()
                || save_prompt.is_some()
                || search_prompt.is_some();
            let status = rename_prompt
                .or(close_prompt)
                .or(save_prompt)
                .or(search_prompt)
                .or_else(|| self.active_status_notice().map(ToOwned::to_owned))
                // Below notices on purpose: a notice reports something the user cannot recover
                // once it scrolls past, while the hover preview returns the moment they point at
                // the link again.
                .or_else(|| {
                    self.hovered_link
                        .as_ref()
                        .map(|hovered| hyperlink_status_text(&hovered.link.uri, screen.columns))
                })
                .unwrap_or_else(|| tab_status_text(&self.tabs, self.active_tab, screen.columns));
            let style = theme.status();
            let row = screen.rows - 1;
            if theme.status_fill {
                // Paint the whole row first: a status background that stops where the text does
                // reads as a rendering bug rather than a bar.
                screen.fill_row(row, style);
            }
            screen.draw_text(0, row, &status, style);
            if self.agent_navigator.is_none() && self.tab_navigator.is_none() && prompt_active {
                screen.cursor = Some((
                    status.chars().count().min(usize::from(screen.columns - 1)) as u16,
                    row,
                ));
            }
        }
        if !kitty_graphics {
            screen.suppress_kitty_placeholders();
        }
        let mut kitty_prefix = if kitty_graphics {
            self.kitty_transfers.drain_pending()
        } else {
            Vec::new()
        };
        let frame_full = self.force_full || !kitty_prefix.is_empty();
        self.frame_id = self.frame_id.wrapping_add(1);
        // Mutated only by the Windows bracketed-paste prepend below.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut bytes = ansi_diff(self.last_screen.as_ref(), &screen, frame_full);
        if !kitty_prefix.is_empty() {
            kitty_prefix.extend_from_slice(&bytes);
            bytes = kitty_prefix;
        }
        #[cfg(windows)]
        let bracketed_paste_transition =
            bracketed_paste_transition(self.outer_bracketed_paste, focused_bracketed_paste);
        #[cfg(windows)]
        if let Some(transition) = bracketed_paste_transition {
            prepend_bracketed_paste_transition(&mut bytes, transition);
        }
        // Put the media projection on the ordered client stream before the terminal frame that
        // exposes the new tab. The client can then reconcile the retained scene concurrently
        // with terminal painting instead of always showing pane text first and the image later.
        self.sync_media(false);
        let sent = crate::ipc::send_render_record(
            &writer,
            self.frame_id,
            self.session_sequence,
            frame_full,
            &bytes,
        )
        .is_ok();
        if !sent {
            // Dropping the client here is the only signal it gets, so say why. Previously a
            // failed frame silently detached the session while the client kept running against a
            // frozen outer scene, which is indistinguishable from a hang.
            let _ = crate::ipc::send(
                &writer,
                &ServerMessage::Detached {
                    reason: "frame delivery failed".into(),
                },
            );
            self.attached = None;
            self.clear_kitty_graphics();
            self.reported_input_mode = None;
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
        let media_revision = self.vivid.revision();
        if media_revision != self.last_plugin_media_revision {
            self.last_plugin_media_revision = media_revision;
            self.queue_plugin_state_event(
                "media.changed",
                "media".into(),
                serde_json::json!({"media_revision": media_revision}),
                None,
            );
            self.schedule_render();
        }
        let Some(client) = &self.attached else {
            self.pending_media_projections.clear();
            self.retained_replay_requests.clear();
            self.retained_replay_inflight.clear();
            self.record_projection_sources(&HashSet::new(), self.vivid.revision());
            self.traced_recovery_deliveries.clear();
            self.vivid.deactivate_bridge();
            return;
        };
        if !client.vivid {
            self.pending_media_projections.clear();
            self.retained_replay_requests.clear();
            self.retained_replay_inflight.clear();
            self.record_projection_sources(&HashSet::new(), self.vivid.revision());
            self.traced_recovery_deliveries.clear();
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
        // Preparing a snapshot parks falling edges immediately but does not wake rising edges.
        // The matching BridgeApplied acknowledgement publishes those sources below.
        let mut snapshot = self
            .vivid
            .prepare_projection_snapshot_with_viewports(&panes, &viewport_offsets);
        let projection_key = MediaProjectionKey {
            virtual_revision: snapshot.revision,
            layout_revision: self.layout_revision,
        };
        let live_nodes = snapshot.live_nodes.iter().copied().collect::<HashSet<_>>();
        self.fragment_assignments
            .retain(|logical, _| live_nodes.contains(logical));
        let surfaces = snapshot
            .surfaces
            .iter()
            .map(|surface| BridgeSurface {
                key: BridgeSurfaceKey {
                    producer: surface.producer,
                    context: surface.context,
                    surface: surface.surface,
                },
                logical_width: surface.logical_width,
                logical_height: surface.logical_height,
                capture_policy: surface.capture_policy,
                descriptor: BridgeSourceDescriptor {
                    role: surface.semantic_descriptor.role,
                    title: surface.semantic_descriptor.title.clone(),
                    content_revision: surface.semantic_descriptor.content_revision,
                    semantic_availability: surface.semantic_descriptor.semantic_availability,
                    locator: surface.semantic_descriptor.locator.clone(),
                },
            })
            .collect::<Vec<_>>();
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
                live: source.live,
                active: source.active,
                audio_gain: source.audio_gain.map(|gain| gain.raw()),
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
        let projected_source_keys = sources
            .iter()
            .map(|source| source.key)
            .collect::<HashSet<_>>();
        let retained_replay_candidates = snapshot
            .sources
            .iter()
            .filter(|source| {
                source.first_visible_presented
                    && (source.retained.is_some() || source.retained_raster.is_some())
            })
            .map(|source| bridge_key(source.key))
            .collect::<HashSet<_>>();
        let surface_count = u16::try_from(surfaces.len()).unwrap_or(u16::MAX);
        let track_count = u16::try_from(sources.len()).unwrap_or(u16::MAX);
        let node_count = u16::try_from(nodes.len()).unwrap_or(u16::MAX);
        let projection_revision = self.media_projection_revision.wrapping_add(1);
        if crate::ipc::send(
            &writer,
            &ServerMessage::MediaSnapshot {
                revision: projection_revision,
                surfaces,
                tracks: sources,
                nodes,
                videos_needing_keyframes,
            },
        )
        .is_err()
        {
            return;
        }
        let still_applied = self
            .traced_projected_sources
            .intersection(&projected_source_keys)
            .copied()
            .collect::<HashSet<_>>();
        self.record_projection_sources(&still_applied, projection_key.virtual_revision);
        self.pending_media_projections.insert(
            projection_revision,
            PendingMediaProjection {
                sources: projected_source_keys,
                retained_replay_candidates,
                retained_replays: HashSet::new(),
                gateway_revision: projection_key.virtual_revision,
            },
        );
        while self.pending_media_projections.len() > MAX_PENDING_MEDIA_PROJECTIONS {
            self.pending_media_projections.pop_first();
        }
        self.record_media_trace(
            None,
            self.bridge_instance_id,
            None,
            MediaTraceKind::ProjectionSubmitted {
                virtual_revision: projection_key.virtual_revision,
                surface_count,
                track_count,
                node_count,
            },
        );
        for source in snapshot.sources {
            let source_key = bridge_key(source.key);
            let forced_replay = self.retained_replay_requests.contains(&source_key);
            if !should_replay_retained(
                source.key,
                live_delivery_source,
                source.first_visible_presented,
                self.outer_attachment_generations.contains_key(&source_key),
                forced_replay,
            ) {
                // The MediaEvent that triggered this projection sync follows immediately. Do not
                // also send the same retained raster body as delivery 0: the outer source would
                // observe the same frame ID twice and reject the live update. Likewise, an
                // already-presented retained body needs no IPC replay while its outer attachment
                // remains resident.
                continue;
            }
            let sent = match source.descriptor {
                crate::media::SourceDescriptor::Raster(_) => {
                    let Some(raster) = source.retained_raster else {
                        continue;
                    };
                    let Ok(body) = retained_raster_body(&raster) else {
                        continue;
                    };
                    send_media_body(
                        &writer,
                        0,
                        source_key,
                        vivid_protocol::messages::RASTER_FRAME,
                        &body,
                    )
                }
                crate::media::SourceDescriptor::Image(_) => source.retained.is_some_and(|body| {
                    send_media_body(
                        &writer,
                        0,
                        source_key,
                        vivid_protocol::messages::IMAGE_DATA,
                        &body,
                    )
                }),
                _ => continue,
            };
            if !sent {
                return;
            }
            {
                if let Some(pending) = self.pending_media_projections.get_mut(&projection_revision)
                {
                    pending.retained_replays.insert(source_key);
                }
                if forced_replay {
                    self.retained_replay_requests.remove(&source_key);
                    self.retained_replay_inflight.insert(source_key);
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

    fn outer_media_projection(&self) -> crate::media::OuterMediaProjection<'_> {
        crate::media::OuterMediaProjection {
            compatibility_revision: self.outer_projection_revision,
            apply_sequence: self.outer_apply_sequence,
            bridge_instance_id: self.bridge_instance_id,
            bridge_local_revision: self.bridge_local_revision,
            attachment_generations: &self.outer_attachment_generations,
        }
    }

    fn record_media_trace(
        &mut self,
        source: Option<BridgeSourceKey>,
        bridge_instance_id: Option<u64>,
        origin_monotonic_us: Option<u64>,
        kind: MediaTraceKind,
    ) {
        let pane = source.and_then(|source| self.vivid.pane_for_source(source));
        self.media_trace.push(
            &self.session_instance,
            pane,
            source,
            bridge_instance_id,
            origin_monotonic_us,
            kind,
        );
    }

    fn record_delivery_result(&mut self, delivery_id: u64, delivered: bool) {
        if let Some((source, bridge_instance_id, epoch, pts_us)) =
            self.traced_recovery_deliveries.remove(&delivery_id)
        {
            self.record_media_trace(
                Some(source),
                bridge_instance_id,
                None,
                MediaTraceKind::KeyframeDelivery {
                    delivery_id,
                    delivered,
                    epoch,
                    pts_us,
                },
            );
        } else if !delivered {
            self.record_media_trace(
                None,
                self.bridge_instance_id,
                None,
                MediaTraceKind::DeliveryFailed { delivery_id },
            );
        }
    }

    fn record_projection_sources(
        &mut self,
        current: &HashSet<BridgeSourceKey>,
        virtual_revision: u64,
    ) {
        let hidden = self
            .traced_projected_sources
            .difference(current)
            .copied()
            .collect::<Vec<_>>();
        let visible = current
            .difference(&self.traced_projected_sources)
            .copied()
            .collect::<Vec<_>>();
        for source in hidden {
            self.record_media_trace(
                Some(source),
                self.bridge_instance_id,
                None,
                MediaTraceKind::TrackVisibility {
                    visible: false,
                    virtual_revision,
                },
            );
        }
        for source in visible {
            self.record_media_trace(
                Some(source),
                self.bridge_instance_id,
                None,
                MediaTraceKind::TrackVisibility {
                    visible: true,
                    virtual_revision,
                },
            );
        }
        self.traced_projected_sources = current.clone();
    }

    /// Host metrics that back layout and the pane geometry published to producers.
    ///
    /// A detached session keeps the last attached host's metrics. `DisplayMetrics::default()` has a
    /// zero cell size, and a relayout while detached (a pane exiting, an automation split) would
    /// otherwise publish a zero-viewport `DISPLAY_CHANGED` — geometry no producer can honor — and
    /// then publish real metrics again on reattach, resizing every live source twice for a host
    /// that never changed.
    fn layout_display(&self) -> DisplayMetrics {
        self.attached
            .as_ref()
            .map_or(self.last_display, |client| client.display)
    }

    fn content_area(&self) -> Rect {
        let display = self.layout_display();
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

    fn pane_is_visibly_present(&self, pane_id: PaneId) -> bool {
        if self.attached.is_none() || !self.client_focused {
            return false;
        }
        let area = self.content_area();
        self.active_tab().is_some_and(|tab| {
            visible_projections(tab, area)
                .iter()
                .any(|projection| projection.pane_id == pane_id)
        })
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

    /// Tell each pane whose program enabled focus reporting whether it now holds focus.
    ///
    /// The client asks its own terminal for focus reports so the session can answer this, which
    /// means the reports arrive whether or not any pane wants them. Relaying them as ordinary
    /// input put `ESC[I`/`ESC[O` in front of every focused pane's program: a shell prompt showed
    /// it as a stray newline, and a program that never asked for the mode echoed `^[[O`. A pane
    /// hears about focus only when it asked to and only when its own state changed, so an
    /// unrelated pane's program sees nothing at all.
    fn sync_pane_focus(&mut self) {
        let focused = self
            .attached
            .is_some()
            .then(|| self.active_tab().map(|tab| tab.focused))
            .flatten()
            .filter(|_| self.client_focused);
        let mut failures = Vec::new();
        for (pane_id, pane) in &mut self.panes {
            let holds_focus = focused == Some(*pane_id);
            if pane.focus_reported == holds_focus {
                continue;
            }
            pane.focus_reported = holds_focus;
            if !pane.terminal.modes().focus_reporting {
                continue;
            }
            let report: &[u8] = if holds_focus { b"\x1b[I" } else { b"\x1b[O" };
            if let Some(failure) = queue_pane_input(pane, report) {
                failures.push((*pane_id, failure));
            }
        }
        for (pane_id, failure) in failures {
            self.report_input_failure(pane_id, Some(failure));
        }
        let focus_state = (
            self.client_focused && self.attached.is_some(),
            focused.and_then(|pane_id| {
                self.tabs
                    .iter()
                    .find(|tab| tab.contains(pane_id))
                    .map(|tab| tab.id)
            }),
            focused,
        );
        if self.last_plugin_focus != Some(focus_state) {
            self.last_plugin_focus = Some(focus_state);
            self.queue_plugin_state_event(
                "focus.changed",
                "focus".into(),
                serde_json::json!({
                    "client_focused": focus_state.0,
                    "tab_id": focus_state.1,
                    "pane_id": focus_state.2,
                }),
                focus_state.2,
            );
            self.schedule_render();
        }
    }

    /// Mirror the focused pane's Kitty keyboard and SGR-Pixels modes into the attached host
    /// terminal. Without this hop, a nested application can request enhanced key events and pixel
    /// coordinates while the physical presenter continues sending legacy keys and cell positions.
    fn sync_client_input_mode(&mut self) {
        let (keyboard_flags, sgr_pixels) = self
            .active_tab()
            .and_then(|tab| self.panes.get(&tab.focused))
            .map_or((0, false), |pane| {
                let modes = pane.terminal.modes();
                (modes.keyboard_flags, modes.sgr_pixels)
            });
        let input_mode = (keyboard_flags, sgr_pixels);
        if self.attached.is_none() || self.reported_input_mode == Some(input_mode) {
            return;
        }
        self.reported_input_mode = Some(input_mode);
        if let Some(client) = &self.attached {
            let _ = crate::ipc::send(
                &client.writer,
                &ServerMessage::InputMode {
                    keyboard_flags,
                    sgr_pixels,
                },
            );
        }
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
        let prompt_active = self.panes.get(&pane_id).is_some_and(|pane| {
            pane.copy
                .as_ref()
                .and_then(|copy| copy.search.as_ref())
                .is_some_and(|search| search.prompt.is_some())
        });
        if !prompt_active && bytes.len() > 1 && matches!(bytes.first(), Some(b'/') | Some(b'?')) {
            self.copy_input(pane_id, &bytes[..1]);
            self.copy_input(pane_id, &bytes[1..]);
            return;
        }
        let remapped = (!prompt_active)
            .then(|| copy_chord_name(bytes))
            .flatten()
            .and_then(|chord| self.config.keys.copy.get(chord))
            .and_then(|action| copy_action_bytes(action));
        let bytes = remapped.as_deref().unwrap_or(bytes);
        let Some(previous) = self.panes.get(&pane_id).and_then(|pane| pane.copy.clone()) else {
            return;
        };

        if prompt_active {
            let (action, direction) = {
                let pane = self.panes.get_mut(&pane_id).unwrap();
                let copy = pane.copy.as_mut().unwrap();
                let search = copy.search.as_mut().unwrap();
                let action = apply_prompt_key(search.prompt.as_mut().unwrap(), bytes);
                (action, search.direction)
            };
            match action {
                PromptAction::Editing => {}
                PromptAction::Cancel => {
                    if let Some(search) = self
                        .panes
                        .get_mut(&pane_id)
                        .unwrap()
                        .copy
                        .as_mut()
                        .and_then(|copy| copy.search.as_mut())
                    {
                        search.prompt = None;
                    }
                }
                PromptAction::Submit(query) => match crate::search::compile(&query, true, true) {
                    Ok(pattern) => {
                        self.search_pattern = Some((query.clone(), pattern));
                        let from = {
                            let copy = self.panes[&pane_id].copy.as_ref().unwrap();
                            (copy.row as isize - copy.offset as isize, copy.column)
                        };
                        let found = {
                            let pane = &self.panes[&pane_id];
                            find_next(
                                &pane.terminal,
                                &self.search_pattern.as_ref().unwrap().1,
                                from,
                                direction,
                                true,
                            )
                        };
                        let pane = &mut self.panes.get_mut(&pane_id).unwrap();
                        let copy = pane.copy.as_mut().unwrap();
                        let search = copy.search.as_mut().unwrap();
                        search.prompt = None;
                        search.query = query;
                        if let Some(found) = found {
                            copy_jump_to(pane, &self.search_pattern.as_ref().unwrap().1, found);
                        } else {
                            copy.current = None;
                            copy.matches.clear();
                            self.status("search pattern not found");
                        }
                    }
                    Err(error) => {
                        if let Some(search) = self
                            .panes
                            .get_mut(&pane_id)
                            .unwrap()
                            .copy
                            .as_mut()
                            .and_then(|copy| copy.search.as_mut())
                        {
                            search.prompt = None;
                        }
                        self.status(&format!("invalid search pattern: {error}"));
                    }
                },
            }
            self.finish_copy_input(pane_id, previous);
            return;
        }

        let mut search_not_found = false;
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
            b"/" | b"?" => {
                copy.search = Some(CopySearch {
                    prompt: Some(String::new()),
                    direction: if bytes == b"/" {
                        SearchDirection::Forward
                    } else {
                        SearchDirection::Backward
                    },
                    query: copy
                        .search
                        .as_ref()
                        .map_or_else(String::new, |search| search.query.clone()),
                });
            }
            b"n" | b"N" => {
                let Some(search) = copy.search.as_ref() else {
                    self.status("no search query");
                    self.finish_copy_input(pane_id, previous);
                    return;
                };
                if search.query.is_empty() {
                    self.status("no search query");
                    self.finish_copy_input(pane_id, previous);
                    return;
                }
                let direction = if bytes == b"n" {
                    search.direction
                } else {
                    search.direction.opposite()
                };
                let query = search.query.clone();
                let current = copy.current;
                let from = current.map_or(
                    (copy.row as isize - copy.offset as isize, copy.column),
                    |found| match direction {
                        SearchDirection::Forward => (found.line, found.end_column),
                        SearchDirection::Backward => {
                            (found.line, found.start_column.saturating_sub(1))
                        }
                    },
                );
                let needs_compile = self
                    .search_pattern
                    .as_ref()
                    .is_none_or(|(compiled, _)| compiled != &query);
                if needs_compile {
                    match crate::search::compile(&query, true, true) {
                        Ok(pattern) => self.search_pattern = Some((query, pattern)),
                        Err(error) => {
                            self.status(&format!("invalid search pattern: {error}"));
                            self.finish_copy_input(pane_id, previous);
                            return;
                        }
                    }
                }
                let found = find_next(
                    &pane.terminal,
                    &self.search_pattern.as_ref().unwrap().1,
                    from,
                    direction,
                    true,
                );
                if let Some(found) = found {
                    copy_jump_to(pane, &self.search_pattern.as_ref().unwrap().1, found);
                } else {
                    search_not_found = true;
                }
            }
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
        if let Some((query, pattern)) = &self.search_pattern
            && pane
                .copy
                .as_ref()
                .and_then(|copy| copy.search.as_ref())
                .is_some_and(|search| search.query == *query)
        {
            refresh_copy_matches(pane, pattern);
        }
        let _ = pane;
        if search_not_found {
            self.status("search pattern not found");
        }
        self.finish_copy_input(pane_id, previous);
    }

    fn finish_copy_input(&mut self, pane_id: PaneId, previous: CopyState) {
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        let previous = Some(previous);
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
        let Some(tab) = self.active_tab() else {
            return;
        };
        let focused = tab.focused;
        let targets = if tab.sync_input {
            sync_targets(tab, &|pane_id| {
                self.panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.copy.is_some() || !pane_role_accepts_sync(&pane.role))
            })
        } else {
            vec![focused]
        };
        let sanitized = sanitize_bracketed_paste(&self.copy_buffer);
        let mut failures = Vec::new();
        for pane_id in targets {
            let bytes = self.paste_payload_for(pane_id, &sanitized);
            if let Some(failure) = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| queue_pane_input(pane, &bytes))
            {
                failures.push((pane_id, failure));
            }
        }
        for (pane_id, failure) in failures {
            self.report_input_failure(pane_id, Some(failure));
        }
    }

    fn paste_payload_for(&self, pane_id: PaneId, sanitized: &[u8]) -> Vec<u8> {
        if self
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.terminal.modes().bracketed_paste)
        {
            let mut bytes = Vec::with_capacity(sanitized.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(sanitized);
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            self.copy_buffer.clone()
        }
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
            | AutomationMethod::SessionInspect
            | AutomationMethod::ListTabs
            | AutomationMethod::SelectTab { .. }
            | AutomationMethod::Diagnose { .. }
            | AutomationMethod::WaitRendered { .. }
            | AutomationMethod::ReloadConfig
            | AutomationMethod::SaveLayout { .. }
            | AutomationMethod::Plugin(_)
    )
}

fn event_sequence(envelope: &PluginEventEnvelope) -> Option<u64> {
    match envelope {
        PluginEventEnvelope::Event { sequence, .. } => Some(*sequence),
        PluginEventEnvelope::Gap { .. } => None,
    }
}

fn validate_automation_method(method: &AutomationMethod) -> Result<(), AutomationError> {
    let input = match method {
        AutomationMethod::Typing { text, .. } | AutomationMethod::Paste { text, .. } => {
            Some(text.len())
        }
        _ => None,
    };
    if input.is_some_and(|length| length > 1024 * 1024) {
        return Err(AutomationError::new(
            "limit_exceeded",
            "input exceeds 1 MiB",
        ));
    }
    match method {
        AutomationMethod::ReportAgent { source, .. }
        | AutomationMethod::ClearAgentReport { source, .. }
            if source.is_empty() || source.len() > crate::agent::MAX_REPORT_SOURCE_BYTES =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "agent report source must contain 1..=128 bytes",
            ))
        }
        AutomationMethod::Run { command, .. } if command.trim().is_empty() => Err(
            AutomationError::new("invalid_params", "run requires a non-empty command"),
        ),
        AutomationMethod::Action(Action::CopyInput(_)) => Err(AutomationError::new(
            "unsupported",
            "copy-mode input is not exposed through generic automation actions",
        )),
        AutomationMethod::Action(Action::Plugin(reference))
            if !valid_plugin_reference(reference) =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "plugin action must be plugin:<plugin-id>/<action-id>",
            ))
        }
        AutomationMethod::Plugin(crate::ipc::PluginMethod::Invoke {
            reference, input, ..
        }) if !valid_invocation_reference(reference)
            || serde_json::to_vec(input).map_or(true, |body| body.len() > 1024 * 1024) =>
        {
            Err(AutomationError::new(
                "limit_exceeded",
                "plugin reference or input exceeds its limit",
            ))
        }
        AutomationMethod::Plugin(crate::ipc::PluginMethod::PaneOpen { reference })
            if !valid_invocation_reference(reference) =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "plugin pane reference must be ID/PANE",
            ))
        }
        AutomationMethod::Plugin(
            crate::ipc::PluginMethod::JobStatus { job_id }
            | crate::ipc::PluginMethod::JobCancel { job_id }
            | crate::ipc::PluginMethod::JobLogs { job_id },
        ) if !crate::plugin::valid_job_id(job_id) => Err(AutomationError::new(
            "invalid_params",
            "plugin job ID is invalid",
        )),
        AutomationMethod::Plugin(crate::ipc::PluginMethod::EventUnsubscribe {
            subscription_id,
        }) if subscription_id.is_empty() || subscription_id.len() > 256 => Err(
            AutomationError::new("invalid_params", "plugin event subscription ID is invalid"),
        ),
        AutomationMethod::Run { command, .. } if command.len() > MAX_RUN_COMMAND_BYTES => Err(
            AutomationError::new("limit_exceeded", "command exceeds 64 KiB"),
        ),
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
        AutomationMethod::Search { pattern, limit, .. }
            if pattern.len() > crate::search::MAX_PATTERN_BYTES =>
        {
            Err(AutomationError::new(
                "limit_exceeded",
                "search pattern exceeds 8 KiB",
            ))
        }
        AutomationMethod::Search { limit, .. } if !(1..=1000).contains(limit) => Err(
            AutomationError::new("invalid_params", "search limit must be from 1 through 1000"),
        ),
        AutomationMethod::Search {
            start_line: None,
            start_column: Some(_),
            ..
        } => Err(AutomationError::new(
            "invalid_params",
            "start_column requires start_line",
        )),
        AutomationMethod::WaitText { text, regex, .. } if *regex && text.len() > 8 * 1024 => Err(
            AutomationError::new("limit_exceeded", "regular expression exceeds 8 KiB"),
        ),
        AutomationMethod::TraceMedia { limit, .. }
            if !(1..=crate::media_trace::MAX_MEDIA_TRACE_QUERY_EVENTS).contains(limit) =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "media trace limit must be from 1 through 512",
            ))
        }
        AutomationMethod::Diagnose { trace_limit, .. } if !(1..=512).contains(trace_limit) => {
            Err(AutomationError::new(
                "invalid_params",
                "diagnostic trace limit must be from 1 through 512",
            ))
        }
        AutomationMethod::TraceMedia { filter, .. }
            if (filter.context_id.is_some()
                || filter.surface_id.is_some()
                || filter.track_id.is_some())
                && filter.producer_id.is_none() =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "media trace identity filters require producer-id",
            ))
        }
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
                | AutomationMethod::WaitMedia { timeout_ms, .. }
                | AutomationMethod::WaitMediaTrack { timeout_ms, .. }
                | AutomationMethod::FocusWait { timeout_ms, .. } => Some(*timeout_ms),
                AutomationMethod::SelectTab {
                    wait: Some(_),
                    timeout_ms,
                    ..
                } => Some(*timeout_ms),
                AutomationMethod::TraceMedia { timeout_ms, .. } if *timeout_ms != 0 => {
                    Some(*timeout_ms)
                }
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

fn valid_invocation_reference(reference: &str) -> bool {
    let Some((plugin, action)) = reference.split_once('/') else {
        return false;
    };
    reference.len() <= 193
        && plugin.contains('.')
        && !plugin.is_empty()
        && plugin
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && !action.is_empty()
        && action
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn automation_capabilities(plugin: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "protocol": "VVMX",
        "protocol_version": crate::ipc::VERSION,
        "methods": [
            "capabilities", "list_panes", "session_inspect", "list_tabs", "select_tab", "diagnose",
            "inspect", "inspect_media", "split", "focus", "focus_wait", "close_pane",
            "typing", "key", "paste", "get_text", "get_grid", "search", "set_sync_input", "wait_text",
            "wait_screen_change", "wait_screen_stable", "wait_rendered", "wait_exit", "wait_media",
            "wait_media_track",
            "trace_media", "reload_config", "run", "action", "report_agent", "clear_agent_report",
            "save_layout"
        ],
        "limits": automation_limits(),
        "completion_waits": {
            "outer": "foreground_bridge_projection_acknowledgement",
            "rendered": "attached_client_terminal_frame_acknowledgement",
        },
        "plugins": plugin,
    })
}

fn disabled_plugin_capabilities(session_instance: &str) -> serde_json::Value {
    serde_json::json!({
        "enabled": false,
        "protocol_version": vvmux_plugin_api::PROTOCOL_VERSION,
        "session_instance": session_instance,
        "applied_generation": null,
        "methods": ["catalog", "invoke", "job_status", "job_cancel", "job_logs", "pane_open", "event_subscribe", "event_unsubscribe", "reload"],
        "native_trust": "full_user_authority",
        "component_sandbox": true,
        "enforceable_capabilities": plugin_enforceable_capabilities(),
        "actions": [],
        "failed": {},
    })
}

pub(crate) fn plugin_automation_error(error: io::Error) -> AutomationError {
    let message = error.to_string();
    let code = [
        "plugin_not_found",
        "plugin_disabled",
        "action_not_found",
        "schema_invalid",
        "capability_denied",
        "scope_denied",
        "runtime_unavailable",
        "runtime_crashed",
        "busy",
        "timeout",
        "cancelled",
        "event_gap",
        "dependency_failed",
        "output_invalid",
        "protocol_error",
        "job_not_found",
    ]
    .into_iter()
    .find(|code| message.starts_with(code))
    .unwrap_or("runtime_unavailable");
    AutomationError::new(code, message)
}

fn require_plugin_params(
    params: &serde_json::Value,
    allowed: &[&str],
) -> Result<(), AutomationError> {
    let object = params.as_object().ok_or_else(|| {
        AutomationError::new("invalid_params", "host-call params must be an object")
    })?;
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(AutomationError::new(
            "invalid_params",
            format!("unknown host-call parameter `{unknown}`"),
        ));
    }
    Ok(())
}

fn plugin_host_permission(method: &str) -> Option<vvmux_plugin_api::Permission> {
    use vvmux_plugin_api::Permission;

    match method {
        "session.inspect" => Some(Permission::SessionRead),
        "pane.get_text" => Some(Permission::PaneRead),
        "pane.input" => Some(Permission::PaneInput),
        _ => None,
    }
}

fn authorize_session_capability(
    caller: &CallerContext,
    required: vvmux_plugin_api::Permission,
) -> Result<(), AutomationError> {
    if !caller.capabilities.contains(&required) {
        let identity = match &caller.origin {
            CallerOrigin::Automation { client_id } => format!("automation client {client_id}"),
            CallerOrigin::Plugin {
                plugin_id,
                plugin_instance,
            } => format!("plugin {plugin_id} instance {plugin_instance}"),
        };
        let capability = serde_json::to_value(required)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".into());
        return Err(AutomationError::new(
            "capability_denied",
            format!("{identity} lacks `{capability}` capability"),
        ));
    }
    Ok(())
}

fn authorize_session_scope(
    caller: &CallerContext,
    session_instance: &str,
) -> Result<(), AutomationError> {
    if caller.session_instance == session_instance {
        Ok(())
    } else {
        Err(AutomationError::new(
            "scope_denied",
            "caller belongs to a different session instance",
        ))
    }
}

pub(crate) fn plugin_enforceable_permissions() -> [vvmux_plugin_api::Permission; 9] {
    use vvmux_plugin_api::Permission;
    [
        Permission::SessionRead,
        Permission::PaneRead,
        Permission::PaneInput,
        Permission::PaneCreate,
        Permission::PaneManageOwn,
        Permission::PaneManageAny,
        Permission::EventsSubscribe,
        Permission::PluginInvoke,
        Permission::MediaProduce,
    ]
}

pub(crate) fn plugin_enforceable_capabilities() -> Vec<String> {
    plugin_enforceable_permissions()
        .into_iter()
        .filter_map(|permission| {
            serde_json::to_value(permission)
                .ok()?
                .as_str()
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn plugin_disabled_error() -> AutomationError {
    AutomationError::new("plugin_disabled", "plugins are disabled in this session")
}

fn plugin_u64_param(params: &serde_json::Value, name: &str) -> Result<u64, AutomationError> {
    params
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            AutomationError::new(
                "invalid_params",
                format!("{name} must be an unsigned integer"),
            )
        })
}

fn plugin_optional_u64_param(
    params: &serde_json::Value,
    name: &str,
) -> Result<Option<u64>, AutomationError> {
    match params.get(name) {
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            AutomationError::new(
                "invalid_params",
                format!("{name} must be an unsigned integer"),
            )
        }),
        None => Ok(None),
    }
}

fn valid_plugin_reference(reference: &str) -> bool {
    let Some(value) = reference.strip_prefix("plugin:") else {
        return false;
    };
    let Some((plugin, action)) = value.split_once('/') else {
        return false;
    };
    !plugin.is_empty()
        && plugin.len() <= 128
        && plugin.contains('.')
        && plugin
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && !action.is_empty()
        && action.len() <= 64
        && action
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn automation_limits() -> serde_json::Value {
    serde_json::json!({
        "request_bytes": 1024 * 1024,
        "reply_bytes": 16 * 1024 * 1024,
        "rows": 1000,
        "key_repeats": 1000,
        "regex_bytes": 8 * 1024,
        "search_results": 1000,
        "search_scan_lines": crate::search::MAX_SEARCH_SCAN_LINES,
        "command_bytes": MAX_RUN_COMMAND_BYTES,
        "agent_report_source_bytes": crate::agent::MAX_REPORT_SOURCE_BYTES,
        "media_trace_events": crate::media_trace::MAX_MEDIA_TRACE_EVENTS,
        "media_trace_bytes": crate::media_trace::MAX_MEDIA_TRACE_BYTES,
        "media_trace_query_events": crate::media_trace::MAX_MEDIA_TRACE_QUERY_EVENTS,
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

fn agent_json(agent: AgentSnapshot) -> serde_json::Value {
    serde_json::json!({
        "kind": agent.kind,
        "label": agent.label,
        "provider": agent.provider,
        "state": agent.state,
        "status": agent.status,
        "source": agent.source,
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
    if modes.sgr_pixels {
        names.push("sgr_pixels");
    }
    if modes.keyboard_flags != 0 {
        names.push("kitty_keyboard");
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

/// A short human description of how a process ended, written into a held pane.
fn describe_exit(status: Option<PtyExitStatus>) -> String {
    match status {
        Some(status) => match (status.code, status.signal) {
            (_, Some(signal)) => format!("killed by signal {signal}"),
            (Some(code), None) => format!("exited {code}"),
            (None, None) => "exited".to_owned(),
        },
        None => "exited with an unknown status".to_owned(),
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

/// Whether one source's retained body has to be replayed with this projection.
///
/// A retained body that has already been presented and whose outer attachment is still resident
/// is where it needs to be, whatever kind it is. Replaying it anyway costs a full body per
/// projection change - megabytes per relayout for a page raster - which an outer resize produces
/// dozens of times a second; the client's bounded media queue overflows, and the records it drops
/// to stay bounded include the live ones a nested producer is waiting on.
fn should_replay_retained(
    source: crate::media::SourceKey,
    live_delivery_source: Option<crate::media::SourceKey>,
    presented: bool,
    outer_attachment_resident: bool,
    forced_replay: bool,
) -> bool {
    Some(source) != live_delivery_source
        && (forced_replay || !presented || !outer_attachment_resident)
}

/// Select recreated retained tracks whose bodies were not part of the applied projection.
///
/// The client owns outer identities, so its recreation report is authoritative. Intersecting it
/// with previously presented retained sources from the exact acknowledged projection prevents an
/// initial live frame or a stale/malformed report from replaying an unrelated owner's source. A
/// body already submitted with that projection, or already awaiting outer confirmation from an
/// earlier forced replay, must not be duplicated.
fn retained_replays_after_apply(
    recreated_retained_sources: &[BridgeSourceKey],
    replay_candidates: &HashSet<BridgeSourceKey>,
    submitted_retained_replays: &HashSet<BridgeSourceKey>,
    forced_replays_inflight: &HashSet<BridgeSourceKey>,
) -> HashSet<BridgeSourceKey> {
    recreated_retained_sources
        .iter()
        .copied()
        .filter(|source| {
            replay_candidates.contains(source)
                && !submitted_retained_replays.contains(source)
                && !forced_replays_inflight.contains(source)
        })
        .collect()
}

#[cfg(unix)]
fn default_shell() -> Option<OsString> {
    std::env::var_os("SHELL")
}

#[cfg(windows)]
fn default_shell() -> Option<OsString> {
    default_windows_shell(
        std::env::var_os("SHELL"),
        std::env::var_os("COMSPEC"),
        crate::platform::resolve_windows_executable,
    )
}

#[cfg(windows)]
fn default_windows_shell(
    shell: Option<OsString>,
    comspec: Option<OsString>,
    mut resolve: impl FnMut(&std::ffi::OsStr) -> Option<OsString>,
) -> Option<OsString> {
    shell.and_then(|shell| resolve(&shell)).or(comspec)
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

/// A pane's directory as a saved layout should spell it: `~/`-relative when it is under `$HOME`,
/// so the file stays portable, and omitted when the path is not valid UTF-8.
fn saved_cwd(cwd: &Path, home: Option<&Path>) -> Option<String> {
    if let Some(home) = home
        && let Ok(rest) = cwd.strip_prefix(home)
        && !rest.as_os_str().is_empty()
    {
        return rest.to_str().map(|rest| format!("~/{rest}"));
    }
    cwd.to_str().map(str::to_owned)
}

/// Rescale a live split's weights into the 1..=1000 the layout parser accepts, preserving ratio.
fn saved_sizes(first: u32, second: u32) -> Vec<u32> {
    let total = u64::from(first) + u64::from(second);
    if total == 0 {
        return vec![1, 1];
    }
    let scaled = ((u64::from(first) * 1000) / total).clamp(1, 999) as u32;
    vec![scaled, 1000 - scaled]
}

/// A float's size as a percentage of the content area, inside the parser's accepted range.
fn saved_percent(extent: u16, available: u16) -> u16 {
    if available == 0 {
        return 60;
    }
    ((u32::from(extent) * 100) / u32::from(available)).clamp(10, 100) as u16
}

fn bridge_play_request(request: crate::media::PlayRequest) -> BridgePlayRequest {
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
        b"/" => Some("/"),
        b"?" => Some("?"),
        b"n" => Some("n"),
        b"N" => Some("N"),
        b"\x1b" => Some("Escape"),
        _ => None,
    }
}

pub(crate) fn copy_action_bytes(action: &str) -> Option<Vec<u8>> {
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
            "search-forward" => b"/".as_slice(),
            "search-backward" => b"?".as_slice(),
            "search-next" => b"n".as_slice(),
            "search-previous" => b"N".as_slice(),
            _ => return None,
        }
        .to_vec(),
    )
}

fn refresh_copy_matches(pane: &mut Pane, pattern: &SearchPattern) {
    let Some(copy) = pane.copy.as_mut() else {
        return;
    };
    copy.matches.clear();
    for row in 0..pane.terminal.rows() {
        let line = row as isize - copy.offset as isize;
        copy.matches
            .extend(find_on_line(&pane.terminal, pattern, line));
    }
}

fn copy_jump_to(pane: &mut Pane, pattern: &SearchPattern, found: SearchMatch) {
    let rows = pane.terminal.rows();
    let centered = rows as isize / 2 - found.line;
    let offset = centered.clamp(0, pane.terminal.history_len() as isize) as usize;
    let copy = pane.copy.as_mut().unwrap();
    copy.offset = offset;
    copy.row = (found.line + offset as isize).clamp(0, rows.saturating_sub(1) as isize) as usize;
    copy.column = found
        .start_column
        .min(pane.terminal.cols().saturating_sub(1));
    copy.current = Some(found);
    refresh_copy_matches(pane, pattern);
}

fn automation_search(
    terminal: &Terminal,
    pattern: &SearchPattern,
    direction: SearchDirection,
    start_line: Option<isize>,
    start_column: Option<usize>,
    limit: usize,
) -> (Vec<serde_json::Value>, bool) {
    if direction == SearchDirection::Forward && start_line.is_none() {
        let (found, truncated) = find_all(terminal, pattern, limit);
        return (search_values(terminal, &found), truncated);
    }
    let first = -(terminal.history_len() as isize);
    let last = terminal.rows() as isize - 1;
    let start = start_line
        .unwrap_or(match direction {
            SearchDirection::Forward => first,
            SearchDirection::Backward => last,
        })
        .clamp(first, last);
    let start_column = start_column.unwrap_or(match direction {
        SearchDirection::Forward => 0,
        SearchDirection::Backward => terminal.cols(),
    });
    let mut found = Vec::new();

    let lines: Box<dyn Iterator<Item = isize>> = match direction {
        SearchDirection::Forward => Box::new(start..=last),
        SearchDirection::Backward => Box::new((first..=start).rev()),
    };
    for (scanned, line) in lines.enumerate() {
        if scanned == crate::search::MAX_SEARCH_SCAN_LINES {
            return (search_values(terminal, &found), true);
        }
        let mut line_matches = find_on_line(terminal, pattern, line);
        if direction == SearchDirection::Backward {
            line_matches.reverse();
        }
        for candidate in line_matches {
            if line == start
                && match direction {
                    SearchDirection::Forward => candidate.start_column < start_column,
                    SearchDirection::Backward => candidate.start_column > start_column,
                }
            {
                continue;
            }
            if found.len() == limit {
                return (search_values(terminal, &found), true);
            }
            found.push(candidate);
        }
    }
    (search_values(terminal, &found), false)
}

fn search_values(terminal: &Terminal, matches: &[SearchMatch]) -> Vec<serde_json::Value> {
    matches
        .iter()
        .map(|found| {
            let text = row_text_with_columns(terminal, found.line)
                .map(|(text, columns)| {
                    text.chars()
                        .zip(columns)
                        .filter_map(|(ch, column)| {
                            (column >= found.start_column && column < found.end_column)
                                .then_some(ch)
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            serde_json::json!({
                "line": found.line,
                "start_column": found.start_column,
                "end_column": found.end_column,
                "text": text,
            })
        })
        .collect()
}

fn bridge_key(key: crate::media::SourceKey) -> BridgeSourceKey {
    key
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
            compression_mode: u64::from(config.zstd_enabled),
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
            width: config.coded_width,
            height: config.coded_height,
            profile: config.profile,
            level: config.level,
            bitrate: u64::from(config.maximum_access_unit_bytes)
                .saturating_mul(8)
                .saturating_mul(240),
            color_primaries: config.color_primaries,
            transfer: config.transfer,
            matrix: config.matrix,
            range: config.signal_range,
            sar_num: u32::try_from(config.aspect_numerator).unwrap_or(u32::MAX),
            sar_den: u32::try_from(config.aspect_denominator).unwrap_or(u32::MAX),
            max_access_unit_bytes: config.maximum_access_unit_bytes,
            codec_string: config.codec_string.clone(),
            decoder_config: config.decoder_configuration.clone(),
        },
        crate::media::SourceDescriptor::Audio(config) => BridgeSourceKind::Audio {
            linked_video: config.linked_video_source_id.map(|source| BridgeSourceKey {
                producer: key.producer,
                context: key.context,
                surface: key.surface,
                track: source,
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
        surface: BridgeSurfaceKey {
            producer: node.config.node.track.producer,
            context: node.config.node.track.context,
            surface: node.config.node.track.surface,
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

fn retained_raster_body(raster: &crate::media::RetainedRaster) -> io::Result<Vec<u8>> {
    vivid_protocol::media::raster_frame_body(
        raster.epoch,
        raster.frame_id,
        raster.width,
        raster.height,
        &raster.pixels,
    )
}

/// Mark media/projection work pending and enqueue at most one coalesced actor wake.
///
/// A wake that loses a race with a full general queue is still safe: the dirty bit remains set,
/// and the actor checks it after every event and idle timeout.
fn request_media_service(wakeup: &mpsc::SyncSender<ActorEvent>, pending: &AtomicBool) {
    if !pending.swap(true, Ordering::AcqRel) {
        let _ = wakeup.try_send(ActorEvent::MediaReady);
    }
}

/// Consume no more than `limit` ready items, returning whether the limit was reached.
fn drain_ready_batch<T>(
    receiver: &mpsc::Receiver<T>,
    limit: usize,
    mut consume: impl FnMut(T),
) -> bool {
    for _ in 0..limit {
        let Ok(item) = receiver.try_recv() else {
            return false;
        };
        consume(item);
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

fn pixel_mouse_to_cells(mut mouse: MouseEvent, display: DisplayMetrics) -> MouseEvent {
    let cell_width = display.cell_width.max(1);
    let cell_height = display.cell_height.max(1);
    mouse.x = mouse
        .x
        .checked_div(cell_width)
        .unwrap_or(0)
        .min(display.columns.saturating_sub(1));
    mouse.y = mouse
        .y
        .checked_div(cell_height)
        .unwrap_or(0)
        .min(display.rows.saturating_sub(1));
    mouse
}

/// Coordinates for a pane's SGR mouse report. Cell input remains cell-based unless the pane asks
/// for DEC 1016, in which case the cell center is the best available fallback. Native input keeps
/// the original physical pixel so nested raster applications retain precise pointer placement.
fn application_mouse_coordinates(
    mouse: MouseEvent,
    pixels: Option<(u16, u16)>,
    content: Rect,
    display: DisplayMetrics,
    sgr_pixels: bool,
) -> (u32, u32) {
    if !sgr_pixels {
        return (
            u32::from(mouse.x.saturating_sub(content.x)) + 1,
            u32::from(mouse.y.saturating_sub(content.y)) + 1,
        );
    }

    let cell_width = u32::from(display.cell_width.max(1));
    let cell_height = u32::from(display.cell_height.max(1));
    let origin_x = u32::from(content.x).saturating_mul(cell_width);
    let origin_y = u32::from(content.y).saturating_mul(cell_height);
    match pixels {
        Some((x, y)) => (
            u32::from(x).saturating_sub(origin_x) + 1,
            u32::from(y).saturating_sub(origin_y) + 1,
        ),
        None => (
            u32::from(mouse.x.saturating_sub(content.x))
                .saturating_mul(cell_width)
                .saturating_add(cell_width / 2)
                + 1,
            u32::from(mouse.y.saturating_sub(content.y))
                .saturating_mul(cell_height)
                .saturating_add(cell_height / 2)
                + 1,
        ),
    }
}

/// Whether a reported display is a real resize rather than a repeat of the current one.
///
/// Browser presenters report every dimension re-measurement, not only genuine resizes, so an
/// unchanged display arrives many times a second. Acting on one bumps `layout_revision`, which
/// makes `should_sync_media` rebuild the outer Vivid session and tears down media that is still
/// being projected, so an unchanged display must not be treated as a resize.
fn is_display_change(current: Option<DisplayMetrics>, next: DisplayMetrics) -> bool {
    current.is_none_or(|display| display != next)
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

/// Shift one viewport-anchored selection cell by a full-screen scroll that entered scrollback.
fn shift_mouse_selection_cell(cell: (isize, usize), lines: i32) -> (isize, usize) {
    (cell.0 - lines as isize, cell.1)
}

/// Whether a selection's row span intersects the half-open row range `[top, bottom)`.
fn mouse_selection_intersects_rows(selection: MouseSelection, top: usize, bottom: usize) -> bool {
    let first = selection.start.0.min(selection.end.0);
    let last = selection.start.0.max(selection.end.0);
    last >= isize::try_from(top).unwrap_or(isize::MIN)
        && first < isize::try_from(bottom).unwrap_or(isize::MIN)
}

/// Decide what happens to a pane's mouse selection across one batch of pane output.
///
/// Mouse selections are viewport-relative: coordinates only stay on the same text when the whole
/// primary screen scrolls into scrollback (`pushed_to_history`), in which case both endpoints
/// shift up by the scroll count — vivido rotates its selections the same way. A scroll inside a
/// sub-region or on the alternate screen moves rows by an amount that depends on each row's
/// position in the region, so a selection intersecting it is dropped rather than rotated onto
/// different text; vivido clamps such selections instead, which needs grid-absolute anchors.
/// Screen clears, alternate-screen switches, and scrolling fully past the retained scrollback
/// also drop the selection.
fn pane_mouse_selection_after_output(
    selection: Option<MouseSelection>,
    events: &[TerminalEvent],
    screen_switched: bool,
    history_len: usize,
) -> Option<MouseSelection> {
    let mut selection = selection?;
    for event in events {
        match *event {
            TerminalEvent::GridScroll {
                lines,
                top,
                bottom,
                pushed_to_history,
            } => {
                if pushed_to_history {
                    selection.start = shift_mouse_selection_cell(selection.start, lines);
                    selection.end = shift_mouse_selection_cell(selection.end, lines);
                    let oldest_retained = -isize::try_from(history_len).unwrap_or(isize::MIN);
                    if selection.start.0.max(selection.end.0) < oldest_retained {
                        // Every selected line has scrolled past the retained scrollback.
                        return None;
                    }
                } else if mouse_selection_intersects_rows(selection, top, bottom) {
                    return None;
                }
            }
            TerminalEvent::Clear => return None,
            _ => {}
        }
    }
    if screen_switched {
        return None;
    }
    Some(selection)
}

fn mouse_selection_cell(
    content: Rect,
    x: u16,
    y: u16,
    display_offset: usize,
) -> Option<(isize, usize)> {
    if content.width == 0 || content.height == 0 {
        return None;
    }
    let right = content.x.saturating_add(content.width - 1);
    let bottom = content.y.saturating_add(content.height - 1);
    let column = usize::from(x.clamp(content.x, right) - content.x);
    let row = isize::try_from(y.clamp(content.y, bottom) - content.y).ok()?;
    let offset = isize::try_from(display_offset).unwrap_or(isize::MAX);
    Some((row.saturating_sub(offset), column))
}

fn starts_mouse_selection(mouse: MouseEvent, copy_mode: bool, modes: TerminalModes) -> bool {
    mouse.kind == MouseKind::Press
        && mouse.button == 0
        && (mouse.shift || copy_mode || !modes.mouse_clicks)
}

fn normalize_mouse_selection_cell(terminal: &Terminal, cell: (isize, usize)) -> (isize, usize) {
    let (line, mut column) = cell;
    if terminal
        .viewport_line(line)
        .and_then(|cells| cells.get(column))
        .is_some_and(|cell| cell.wide_continuation)
    {
        column = column.saturating_sub(1);
    }
    (line, column)
}

fn mouse_selection_runs(
    terminal: &Terminal,
    selection: MouseSelection,
    display_offset: usize,
    viewport_width: usize,
    viewport_height: usize,
) -> Vec<(usize, usize, usize)> {
    if viewport_width == 0 || viewport_height == 0 {
        return Vec::new();
    }
    let (start, end) = if selection.start <= selection.end {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    };
    let offset = isize::try_from(display_offset).unwrap_or(isize::MAX);
    let mut runs = Vec::new();
    for line in start.0..=end.0 {
        let row = line.saturating_add(offset);
        if row < 0 || row >= viewport_height as isize {
            continue;
        }
        let (first, mut last) = match selection.mode {
            MouseSelectionMode::Line => (0, viewport_width - 1),
            MouseSelectionMode::Character => (
                if line == start.0 { start.1 } else { 0 },
                if line == end.0 {
                    end.1
                } else {
                    viewport_width - 1
                },
            ),
        };
        if first >= viewport_width {
            continue;
        }
        last = last.min(viewport_width - 1);
        if last < first {
            continue;
        }
        if selection.mode == MouseSelectionMode::Character
            && last + 1 < viewport_width
            && terminal
                .viewport_line(line)
                .and_then(|cells| cells.get(last + 1))
                .is_some_and(|cell| cell.wide_continuation)
        {
            last += 1;
        }
        runs.push((row as usize, first, last - first + 1));
    }
    runs
}

fn tab_status_text(tabs: &[Tab], active: usize, columns: u16) -> String {
    let width = usize::from(columns);
    if width == 0 || tabs.is_empty() {
        return String::new();
    }
    let active = active.min(tabs.len() - 1);
    let segments = tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let number = index + 1;
            let label = tab
                .name
                .as_deref()
                .map(single_line)
                .filter(|name| !name.trim().is_empty())
                .map_or_else(|| number.to_string(), |name| format!("{number}:{name}"));
            if index == active {
                format!("[{label}]")
            } else {
                label
            }
        })
        .collect::<Vec<_>>();
    let all = render_tab_status_window(&segments, 0, segments.len());
    if all.chars().count() <= width {
        return all;
    }

    let mut start = active;
    let mut end = active + 1;
    loop {
        let mut changed = false;
        if start > 0 {
            let candidate = render_tab_status_window(&segments, start - 1, end);
            if candidate.chars().count() <= width {
                start -= 1;
                changed = true;
            }
        }
        if end < segments.len() {
            let candidate = render_tab_status_window(&segments, start, end + 1);
            if candidate.chars().count() <= width {
                end += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let visible = render_tab_status_window(&segments, start, end);
    if visible.chars().count() <= width {
        visible
    } else {
        render_narrow_active_tab(&segments[active], start > 0, end < segments.len(), width)
    }
}

fn render_tab_status_window(segments: &[String], start: usize, end: usize) -> String {
    let mut visible = Vec::with_capacity(end.saturating_sub(start) + 2);
    if start > 0 {
        visible.push("<".to_owned());
    }
    visible.extend_from_slice(&segments[start..end]);
    if end < segments.len() {
        visible.push(">".to_owned());
    }
    format!(" {} ", visible.join(" "))
}

fn clip_chars(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn render_narrow_active_tab(segment: &str, left: bool, right: bool, width: usize) -> String {
    let prefix = if left { " < " } else { " " };
    let suffix = if right { " >" } else { " " };
    let fixed = prefix.chars().count() + suffix.chars().count();
    if fixed >= width {
        return clip_chars(&format!("{prefix}{suffix}"), width);
    }
    let available = width - fixed;
    let clipped = if segment.starts_with('[') && segment.ends_with(']') && available >= 2 {
        let inner = &segment[1..segment.len() - 1];
        format!("[{}]", clip_chars(inner, available - 2))
    } else {
        clip_chars(segment, available)
    };
    format!("{prefix}{clipped}{suffix}")
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn apply_tab_rename_input(rename: &mut TabRename, bytes: &[u8]) -> LineEditInput {
    apply_line_edit(
        &mut rename.value,
        &mut rename.pending_utf8,
        MAX_TAB_NAME_BYTES,
        bytes,
    )
}

/// The shared status-row line editor: Enter commits, Escape cancels, Backspace deletes, and
/// printable input accumulates until `max_bytes`. Multi-byte input may arrive split across reads,
/// so an incomplete sequence stays pending rather than being interpreted.
fn apply_line_edit(
    value: &mut String,
    pending_utf8: &mut Vec<u8>,
    max_bytes: usize,
    bytes: &[u8],
) -> LineEditInput {
    pending_utf8.extend_from_slice(bytes);
    let mut offset = 0;
    while offset < pending_utf8.len() {
        let byte = pending_utf8[offset];
        if byte.is_ascii() {
            offset += 1;
            match byte {
                0x1b => {
                    pending_utf8.clear();
                    return LineEditInput::Cancel;
                }
                b'\r' | b'\n' => {
                    pending_utf8.clear();
                    return LineEditInput::Commit;
                }
                0x08 | 0x7f => {
                    value.pop();
                }
                printable if !printable.is_ascii_control() && value.len() < max_bytes => {
                    value.push(char::from(printable));
                }
                _ => {}
            }
            continue;
        }

        let width = match byte {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => {
                offset += 1;
                continue;
            }
        };
        if pending_utf8.len() - offset < width {
            break;
        }
        let candidate = &pending_utf8[offset..offset + width];
        if let Ok(text) = std::str::from_utf8(candidate) {
            if let Some(character) = text.chars().next()
                && !character.is_control()
                && value.len().saturating_add(width) <= max_bytes
            {
                value.push(character);
            }
            offset += width;
        } else {
            // Discard only the invalid lead byte so a following ASCII Enter/Escape is still
            // interpreted as prompt control rather than swallowed as a fake continuation.
            offset += 1;
        }
    }
    pending_utf8.drain(..offset);
    LineEditInput::Editing
}

fn tab_navigator_rect(area: Rect, row_count: usize) -> Option<Rect> {
    agent_navigator_rect(area, row_count)
}

fn agent_navigator_rect(area: Rect, row_count: usize) -> Option<Rect> {
    if area.width < 20 || area.height < 3 {
        return None;
    }
    let width = area.width.clamp(20, 100);
    let desired_height = u16::try_from(row_count.saturating_add(2)).unwrap_or(u16::MAX);
    let height = desired_height.clamp(3, area.height.min(18));
    Some(Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    })
}

/// Longest key report `key_presses` will look ahead for a final byte.
const MAX_KEY_REPORT_BYTES: usize = 64;

/// Drop key release and repeat reports from input bound for one of vvmux's own prompts.
///
/// A pane can ask the host terminal to report key events and not only presses, and vvmux mirrors
/// that request so the pane receives the stream it asked for. Its prompts read a much smaller key
/// language in which a leading ESC cancels, and every one of those reports begins `ESC [`: without
/// this filter `prefix w` closed its own popup as soon as the key came back up, and every
/// selection key closed it again. Pane input is never filtered.
fn key_presses(bytes: &[u8]) -> Cow<'_, [u8]> {
    if !bytes.contains(&0x1b) {
        return Cow::Borrowed(bytes);
    }
    let mut kept = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        match non_press_key_report(&bytes[offset..]) {
            Some(length) => offset += length,
            None => {
                kept.push(bytes[offset]);
                offset += 1;
            }
        }
    }
    if kept.len() == bytes.len() {
        Cow::Borrowed(bytes)
    } else {
        Cow::Owned(kept)
    }
}

/// The length of a leading key release or repeat report, if the input begins with one.
///
/// Both carry the event type as a sub-parameter of the modifier parameter — `ESC[119;1:3u` for a
/// released `w`, `ESC[1;1:2B` for a repeating Down — which is what separates them from the press
/// reports and legacy sequences a prompt understands. An incomplete report is left alone: the
/// client's parser only forwards whole sequences, so a truncated one is not a key report.
fn non_press_key_report(bytes: &[u8]) -> Option<usize> {
    let parameters = bytes.strip_prefix(b"\x1b[")?;
    let end = parameters
        .iter()
        .take(MAX_KEY_REPORT_BYTES)
        .position(|byte| (0x40..=0x7e).contains(byte))?;
    let event = parameters[..end]
        .split(|&byte| byte == b';')
        .nth(1)?
        .split(|&byte| byte == b':')
        .nth(1)?;
    matches!(event, b"2" | b"3").then_some(b"\x1b[".len() + end + 1)
}

fn decode_agent_navigator_key(input: &[u8]) -> (usize, Option<AgentNavigatorKey>) {
    const SEQUENCES: &[(&[u8], AgentNavigatorKey)] = &[
        (b"\x1b[A", AgentNavigatorKey::Up),
        (b"\x1bOA", AgentNavigatorKey::Up),
        (b"\x1b[B", AgentNavigatorKey::Down),
        (b"\x1bOB", AgentNavigatorKey::Down),
        (b"\x1b[H", AgentNavigatorKey::Home),
        (b"\x1bOH", AgentNavigatorKey::Home),
        (b"\x1b[1~", AgentNavigatorKey::Home),
        (b"\x1b[7~", AgentNavigatorKey::Home),
        (b"\x1b[F", AgentNavigatorKey::End),
        (b"\x1bOF", AgentNavigatorKey::End),
        (b"\x1b[4~", AgentNavigatorKey::End),
        (b"\x1b[8~", AgentNavigatorKey::End),
        (b"\x1b[5~", AgentNavigatorKey::PageUp),
        (b"\x1b[6~", AgentNavigatorKey::PageDown),
    ];
    for (sequence, key) in SEQUENCES {
        if input.starts_with(sequence) {
            return (sequence.len(), Some(*key));
        }
    }
    let key = match input.first().copied() {
        Some(b'k') => Some(AgentNavigatorKey::Up),
        Some(b'j') => Some(AgentNavigatorKey::Down),
        Some(b'\r' | b'\n') => Some(AgentNavigatorKey::Activate),
        Some(b'q' | 0x1b) => Some(AgentNavigatorKey::Close),
        Some(_) => None,
        None => return (0, None),
    };
    (1, key)
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Schemes vvmux will hand to the system opener.
///
/// Deliberately short. An OSC 8 URI is written by whatever program has the pane — including remote
/// output a user merely `cat`ed — and unlike a regex-matched text hint nothing has already vetted
/// its shape. Schemes such as `file:` map to local handlers that can launch applications, so the
/// list is an allow-list rather than a deny-list and grows only on request.
const OPENABLE_SCHEMES: [&str; 5] = ["http://", "https://", "mailto:", "irc://", "ircs://"];

/// Minimum gap between two local link activations.
const LINK_OPEN_COOLDOWN: Duration = Duration::from_millis(500);

/// Whether a link is safe to hand to the system opener.
fn is_openable_uri(uri: &str) -> bool {
    // Scheme comparison is ASCII case-insensitive per RFC 3986, and control characters must never
    // reach an argv element.
    if uri.chars().any(char::is_control) {
        return false;
    }
    let lowered = uri.to_ascii_lowercase();
    OPENABLE_SCHEMES
        .iter()
        .any(|scheme| lowered.starts_with(scheme) && lowered.len() > scheme.len())
}

/// The status-row text for a hovered link, trimmed to the row width.
///
/// The head of a URI is the part worth keeping — scheme and host say where a click would go — so an
/// over-long target loses its tail rather than its beginning. Control characters are stripped
/// because the URI is attacker-controlled: it arrives from whatever wrote to the pane, and the
/// status row is composited into the same buffer as everything else.
fn hyperlink_status_text(uri: &str, columns: u16) -> String {
    const ELLIPSIS: char = '…';
    let width = usize::from(columns);
    let text = single_line(uri);
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text;
    }
    let mut truncated: String = text.chars().take(width.saturating_sub(1)).collect();
    truncated.push(ELLIPSIS);
    truncated
}

fn extract_selection_row(
    terminal: &Terminal,
    line_index: isize,
    first: usize,
    last: usize,
) -> Option<String> {
    let line = terminal.viewport_line(line_index)?;
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
    Some(row)
}

fn extract_selection(terminal: &Terminal, start: (isize, usize), end: (isize, usize)) -> Vec<u8> {
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let mut output = String::new();
    for line_index in start.0..=end.0 {
        let Some(line_len) = terminal.viewport_line(line_index).map(|line| line.len()) else {
            continue;
        };
        let first = if line_index == start.0 { start.1 } else { 0 };
        let last = if line_index == end.0 {
            end.1 + 1
        } else {
            line_len
        };
        let row = extract_selection_row(terminal, line_index, first, last).unwrap_or_default();
        output.push_str(&row);
        if line_index != end.0 && !terminal.line_wrapped(line_index).unwrap_or(false) {
            output.push('\n');
        }
    }
    output.into_bytes()
}

fn extract_mouse_selection(terminal: &Terminal, selection: MouseSelection) -> Vec<u8> {
    match selection.mode {
        MouseSelectionMode::Character => {
            extract_selection(terminal, selection.start, selection.end)
        }
        MouseSelectionMode::Line => {
            let (start, end) = if selection.start.0 <= selection.end.0 {
                (selection.start.0, selection.end.0)
            } else {
                (selection.end.0, selection.start.0)
            };
            let mut output = String::new();
            let mut wrote_row = false;
            for line in start..=end {
                let Some(line_len) = terminal.viewport_line(line).map(|line| line.len()) else {
                    continue;
                };
                if wrote_row {
                    output.push('\n');
                }
                output.push_str(
                    &extract_selection_row(terminal, line, 0, line_len).unwrap_or_default(),
                );
                wrote_row = true;
            }
            output.into_bytes()
        }
    }
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

    #[test]
    fn kitty_transfers_are_pane_isolated_bounded_and_drained_in_order() {
        let mut transfers = KittyTransferBuffer::default();
        assert!(transfers.push_bounded(1, b"a1".to_vec(), true, true, 12));
        assert!(transfers.push_bounded(2, b"b1".to_vec(), true, true, 12));
        assert!(transfers.push_bounded(1, b"a2".to_vec(), false, false, 12));
        assert!(transfers.push_bounded(2, b"b2".to_vec(), false, false, 12));
        assert_eq!(transfers.drain_pending(), b"a1a2b1b2");
        assert_eq!(transfers.bytes, 0);

        assert!(transfers.push_bounded(1, b"123456".to_vec(), true, true, 8));
        assert!(!transfers.push_bounded(1, b"789".to_vec(), false, false, 8));
        assert_eq!(transfers.bytes, 0);
        assert!(transfers.pending.is_empty());
        assert!(transfers.transfers.is_empty());
    }

    #[test]
    fn clearing_kitty_transfers_drops_attachment_pixels() {
        let mut transfers = KittyTransferBuffer::default();
        assert!(transfers.push(1, b"upload".to_vec(), true, false));
        transfers.clear();
        assert_eq!(transfers.bytes, 0);
        assert!(transfers.pending.is_empty());
        assert!(transfers.transfers.is_empty());
    }

    #[test]
    fn kitty_support_query_reports_attachment_capability() {
        assert_eq!(kitty_query_response(true, 31), b"\x1b_Gi=31;OK\x1b\\");
        assert_eq!(kitty_query_response(false, 31), b"\x1b_Gi=31;ENOTSUP\x1b\\");
    }

    #[test]
    fn only_vetted_schemes_reach_the_host_opener() {
        assert!(is_openable_uri("https://example.test/a?b=1&c=2"));
        assert!(is_openable_uri("http://example.test/"));
        assert!(is_openable_uri("mailto:someone@example.test"));

        // An OSC 8 URI is written by whatever holds the pane, so these are the shapes an attacker
        // controls outright. None may reach a system handler.
        assert!(!is_openable_uri("file:///etc/passwd"));
        assert!(!is_openable_uri("javascript:alert(1)"));
        assert!(!is_openable_uri("smb://host/share"));
        assert!(!is_openable_uri("vscode://file/etc/passwd"));
        // A scheme with nothing after it gives the handler no target to reason about.
        assert!(!is_openable_uri("https://"));
        // Control characters must never reach an argv element.
        assert!(!is_openable_uri("https://example.test/\r\nX"));
        assert!(!is_openable_uri("https://example.test/\u{1b}]0;pwned\u{7}"));
        // Scheme matching is case-insensitive per RFC 3986.
        assert!(is_openable_uri("HTTPS://example.test/"));
        assert!(!is_openable_uri("FILE:///etc/passwd"));
    }

    #[test]
    fn the_link_preview_keeps_the_head_of_an_over_long_uri() {
        let uri = "https://example.test/a/very/long/path/that/will/not/fit";
        let preview = hyperlink_status_text(uri, 20);
        assert_eq!(preview.chars().count(), 20);
        assert!(preview.starts_with("https://example.tes"));
        assert!(preview.ends_with('…'));

        // A URI that fits is shown whole.
        assert_eq!(
            hyperlink_status_text("https://a.test/", 40),
            "https://a.test/"
        );
        // Control characters are neutralized before the preview is composited.
        assert_eq!(
            hyperlink_status_text("https://a.test/\u{1b}x", 40),
            "https://a.test/ x"
        );
        assert_eq!(hyperlink_status_text("https://a.test/", 0), "");
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefers_a_resolvable_inherited_shell_over_comspec() {
        let selected = default_windows_shell(
            Some(OsString::from("pwsh.exe")),
            Some(OsString::from(r"C:\Windows\System32\cmd.exe")),
            |shell| {
                (shell == std::ffi::OsStr::new("pwsh.exe"))
                    .then(|| OsString::from(r"C:\Program Files\PowerShell\7\pwsh.exe"))
            },
        );
        assert_eq!(
            selected,
            Some(OsString::from(r"C:\Program Files\PowerShell\7\pwsh.exe"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_ignores_an_inherited_shell_that_is_not_a_native_executable() {
        let comspec = OsString::from(r"C:\Windows\System32\cmd.exe");
        let selected = default_windows_shell(
            Some(OsString::from("/bin/bash")),
            Some(comspec.clone()),
            |_| None,
        );
        assert_eq!(selected, Some(comspec));
    }

    #[test]
    fn plugin_host_calls_have_explicit_capabilities_and_strict_params() {
        use vvmux_plugin_api::Permission;

        assert_eq!(
            plugin_host_permission("session.inspect"),
            Some(Permission::SessionRead)
        );
        assert_eq!(
            plugin_host_permission("pane.get_text"),
            Some(Permission::PaneRead)
        );
        assert_eq!(
            plugin_host_permission("pane.input"),
            Some(Permission::PaneInput)
        );
        assert_eq!(plugin_host_permission("pane.delete_anything"), None);
        let caller = CallerContext {
            origin: CallerOrigin::Plugin {
                plugin_id: "dev.example".into(),
                plugin_instance: "instance-a".into(),
            },
            session_instance: "session-a".into(),
            focused_fallback: false,
            capabilities: [Permission::PaneRead].into_iter().collect(),
        };
        assert!(authorize_session_scope(&caller, "session-a").is_ok());
        assert!(authorize_session_capability(&caller, Permission::PaneRead).is_ok());
        assert_eq!(
            authorize_session_capability(&caller, Permission::PaneInput)
                .unwrap_err()
                .code,
            "capability_denied"
        );
        assert_eq!(
            authorize_session_scope(&caller, "session-b")
                .unwrap_err()
                .code,
            "scope_denied"
        );
        assert_eq!(
            plugin_enforceable_capabilities(),
            [
                "session.read",
                "pane.read",
                "pane.input",
                "pane.create",
                "pane.manage_own",
                "pane.manage_any",
                "events.subscribe",
                "plugin.invoke",
                "media.produce",
            ]
        );
        assert!(require_plugin_params(&serde_json::json!({"pane_id": 1}), &["pane_id"]).is_ok());
        assert!(
            require_plugin_params(
                &serde_json::json!({"pane_id": 1, "unexpected": true}),
                &["pane_id"]
            )
            .is_err()
        );
    }

    #[test]
    fn plugin_pane_identity_scopes_sync_management_and_generation_cleanup() {
        use vvmux_plugin_api::Permission;

        let owner = PluginPaneIdentity {
            session_instance: "session-a".into(),
            plugin_id: "dev.example".into(),
            plugin_instance: "instance-a".into(),
            package_digest: "digest-a".into(),
            entrypoint_id: "dashboard".into(),
            title: "Dashboard".into(),
            accept_sync_input: false,
        };
        let role = PaneRole::Plugin(owner.clone());
        let caller = CallerContext {
            origin: CallerOrigin::Plugin {
                plugin_id: owner.plugin_id.clone(),
                plugin_instance: owner.plugin_instance.clone(),
            },
            session_instance: owner.session_instance.clone(),
            focused_fallback: false,
            capabilities: [Permission::PaneManageOwn].into_iter().collect(),
        };
        assert!(!pane_role_accepts_sync(&role));
        let mut sync_owner = owner.clone();
        sync_owner.accept_sync_input = true;
        assert!(pane_role_accepts_sync(&PaneRole::Plugin(sync_owner)));
        assert!(caller_owns_plugin_pane(&caller, &role));
        assert!(plugin_pane_matches_generation(
            &role,
            "session-a",
            "dev.example",
            "digest-a"
        ));

        // Two owners may reuse the same numeric pane ID in separate session instances. Cleanup
        // and management decisions use the complete identity, never that local number.
        let reused_numeric_pane_id = 7_u64;
        let other_role = PaneRole::Plugin(PluginPaneIdentity {
            session_instance: "session-b".into(),
            plugin_instance: "instance-b".into(),
            ..owner.clone()
        });
        assert_eq!(reused_numeric_pane_id, 7);
        assert!(!caller_owns_plugin_pane(&caller, &other_role));
        assert!(!plugin_pane_matches_generation(
            &other_role,
            "session-a",
            "dev.example",
            "digest-a"
        ));

        let restarted = CallerContext {
            origin: CallerOrigin::Plugin {
                plugin_id: owner.plugin_id,
                plugin_instance: "instance-restarted".into(),
            },
            ..caller
        };
        assert!(!caller_owns_plugin_pane(&restarted, &role));
    }

    #[test]
    fn agent_navigator_geometry_is_centered_and_bounded() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        assert_eq!(
            agent_navigator_rect(area, 100),
            Some(Rect {
                x: 10,
                y: 11,
                width: 100,
                height: 18,
            })
        );
        assert!(
            agent_navigator_rect(
                Rect {
                    width: 19,
                    height: 10,
                    ..Rect::default()
                },
                1
            )
            .is_none()
        );
    }

    #[test]
    fn agent_navigator_keys_decode_coalesced_input_without_pty_residue() {
        let mut input = b"\x1b[A\x1b[6~j\r".as_slice();
        let mut keys = Vec::new();
        while !input.is_empty() {
            let (consumed, key) = decode_agent_navigator_key(input);
            assert!(consumed > 0);
            input = &input[consumed..];
            keys.extend(key);
        }
        assert_eq!(
            keys,
            [
                AgentNavigatorKey::Up,
                AgentNavigatorKey::PageDown,
                AgentNavigatorKey::Down,
                AgentNavigatorKey::Activate,
            ]
        );
        assert_eq!(
            decode_agent_navigator_key(b"\x1b[7~"),
            (4, Some(AgentNavigatorKey::Home))
        );
        assert_eq!(
            decode_agent_navigator_key(b"\x1b[8~"),
            (4, Some(AgentNavigatorKey::End))
        );
    }

    #[test]
    fn prompt_input_keeps_presses_and_drops_release_and_repeat_reports() {
        // A pane running under Kitty flags 3 makes the host report key events, so the navigator
        // sees the release of the very key that opened it and the release of every key used to
        // move the selection. Each begins with ESC, which the prompt language reads as a cancel.
        assert_eq!(key_presses(b"\x1b[119;1:3u").as_ref(), b"");
        assert_eq!(key_presses(b"\x1b[1;1:2B").as_ref(), b"");
        assert_eq!(
            key_presses(b"j\x1b[106;1:3uk\x1b[107;1:3u\r").as_ref(),
            b"jk\r"
        );

        // Presses, legacy sequences, and a real Escape are the prompt's own language.
        assert_eq!(key_presses(b"\x1b[B").as_ref(), b"\x1b[B");
        assert_eq!(key_presses(b"\x1b[119u").as_ref(), b"\x1b[119u");
        assert_eq!(key_presses(b"\x1b").as_ref(), b"\x1b");
        assert!(matches!(key_presses(b"jk"), Cow::Borrowed(_)));

        let (consumed, key) = decode_agent_navigator_key(&key_presses(b"\x1b[119;1:3u"));
        assert_eq!((consumed, key), (0, None));
        assert_eq!(
            decode_agent_navigator_key(b"\x1b[119;1:3u"),
            (1, Some(AgentNavigatorKey::Close)),
            "the unfiltered report is what closed the popup"
        );
    }

    #[test]
    fn media_wakeups_coalesce_until_the_actor_clears_pending_work() {
        let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE);
        let pending = AtomicBool::new(false);

        for _ in 0..(EVENT_QUEUE * 2) {
            request_media_service(&sender, &pending);
        }

        assert!(matches!(receiver.try_recv(), Ok(ActorEvent::MediaReady)));
        assert!(
            matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "one video frame must not add another redundant actor wake while media work is pending"
        );

        pending.store(false, Ordering::Release);
        request_media_service(&sender, &pending);
        assert!(matches!(receiver.try_recv(), Ok(ActorEvent::MediaReady)));
    }

    #[test]
    fn saturated_media_is_batched_so_actor_control_gets_a_turn() {
        let (media_sender, media_receiver) = mpsc::sync_channel(MEDIA_EVENT_QUEUE);
        for item in 0..(MEDIA_EVENTS_PER_TURN * 2) {
            media_sender.try_send(item).unwrap();
        }
        let (control_sender, control_receiver) = mpsc::sync_channel(1);
        control_sender.try_send("detach").unwrap();

        let mut forwarded = Vec::new();
        assert!(drain_ready_batch(
            &media_receiver,
            MEDIA_EVENTS_PER_TURN,
            |item| forwarded.push(item)
        ));

        assert_eq!(forwarded.len(), MEDIA_EVENTS_PER_TURN);
        assert_eq!(
            control_receiver.try_recv().unwrap(),
            "detach",
            "a saturated or continuously refilled media receiver must yield to actor control"
        );
        assert!(
            media_receiver.try_recv().is_ok(),
            "the test must leave media queued rather than accidentally exercising exhaustion"
        );
    }

    #[test]
    fn plugin_event_replay_is_capacity_bounded_and_reports_direct_eviction_gap() {
        let mut journal = PluginEventJournal::default();
        for sequence in 1..=1_100 {
            journal.push(test_plugin_event(sequence, 8));
        }

        let replay = journal.replay(0, 1_100, 63);
        assert_eq!(replay.len(), 63);
        assert_eq!(
            replay.first(),
            Some(&PluginEventEnvelope::Gap {
                from_sequence: 1,
                to_sequence: 1_038,
            })
        );
        assert_eq!(event_sequence(replay.last().unwrap()), Some(1_100));
    }

    #[test]
    fn plugin_event_journal_stays_within_entry_and_byte_limits_under_firehose() {
        let mut journal = PluginEventJournal::default();
        for sequence in 1..=4_096 {
            journal.push(test_plugin_event(sequence, 8 * 1024));
        }

        assert!(journal.len() <= PLUGIN_EVENT_JOURNAL);
        assert!(journal.bytes <= PLUGIN_EVENT_JOURNAL_BYTES);
        let replay = journal.replay(0, 4_096, 63);
        assert!(replay.len() <= 63);
        assert!(matches!(
            replay.first(),
            Some(PluginEventEnvelope::Gap { .. })
        ));
        assert_eq!(event_sequence(replay.last().unwrap()), Some(4_096));
    }

    fn test_plugin_event(sequence: u64, payload_bytes: usize) -> PluginEventEnvelope {
        PluginEventEnvelope::Event {
            sequence,
            name: "pane.screen_changed".into(),
            payload: serde_json::json!({"padding": "x".repeat(payload_bytes)}),
            context: vvmux_plugin_api::InvocationContext {
                correlation_id: format!("correlation-{sequence}"),
                causation_id: format!("cause-{sequence}"),
                causation_depth: 0,
                source: "session".into(),
                session_instance: "session-a".into(),
                pane_id: Some(1),
                tab_id: Some(1),
                deadline_unix_ms: 0,
            },
        }
    }

    #[test]
    fn osc52_selections_map_onto_the_single_copy_buffer() {
        for selection in [b'c', b'p', b's'] {
            assert!(is_supported_clipboard_selection(selection));
        }
        for selection in [b'q', b'0', b'?'] {
            assert!(!is_supported_clipboard_selection(selection));
        }
    }

    #[test]
    fn osc52_store_requires_policy_focus_attachment_and_a_known_selection() {
        use crate::config::Osc52;

        let cases = [
            (Osc52::OnlyCopy, true, true, b'c', true),
            (Osc52::CopyPaste, true, true, b'p', true),
            (Osc52::CopyPaste, true, true, b's', true),
            (Osc52::OnlyCopy, false, true, b'c', false),
            (Osc52::OnlyCopy, true, false, b'c', false),
            (Osc52::Disabled, true, true, b'c', false),
            (Osc52::OnlyPaste, true, true, b'c', false),
            (Osc52::OnlyCopy, true, true, b'q', false),
        ];
        for (policy, focused, attached, selection, expected) in cases {
            assert_eq!(
                clipboard_store_allowed(policy, focused, attached, selection),
                expected,
                "policy={policy:?} focused={focused} attached={attached} selection={selection:?}"
            );
        }
    }

    #[test]
    fn osc52_load_reply_uses_request_selection_terminator_and_copy_buffer() {
        assert_eq!(
            osc52_reply(b'c', "héllo".as_bytes(), "\x1b\\"),
            b"\x1b]52;c;aMOpbGxv\x1b\\"
        );
        assert_eq!(
            osc52_reply(b'p', b"hello", "\x07"),
            b"\x1b]52;p;aGVsbG8=\x07"
        );
    }

    #[test]
    fn bracketed_paste_cannot_inject_terminator() {
        assert_eq!(sanitize_bracketed_paste(b"a\x1b[201~b"), b"a\x1b[201;~b");
    }

    #[test]
    fn pane_selection_clicks_are_counted_only_at_one_cell_within_the_interval() {
        let start = Instant::now();
        let first = MouseClickTracker::next(None, 7, (2, 3), start);
        let second =
            MouseClickTracker::next(Some(first), 7, (2, 3), start + Duration::from_millis(100));
        let third =
            MouseClickTracker::next(Some(second), 7, (2, 3), start + Duration::from_millis(200));
        assert_eq!((first.count, second.count, third.count), (1, 2, 3));
        assert_eq!(
            MouseClickTracker::next(Some(third), 7, (2, 3), start + Duration::from_millis(300),)
                .count,
            1,
            "a fourth click begins a new sequence"
        );
        assert_eq!(
            MouseClickTracker::next(Some(second), 7, (2, 4), start + Duration::from_millis(200),)
                .count,
            1,
            "moving to another cell resets the sequence"
        );
        assert_eq!(
            MouseClickTracker::next(Some(second), 7, (2, 3), start + Duration::from_millis(700),)
                .count,
            1,
            "an expired sequence resets"
        );
    }

    #[test]
    fn pane_selection_clamps_pointer_motion_to_the_captured_content_rectangle() {
        let content = Rect {
            x: 1,
            y: 2,
            width: 4,
            height: 3,
        };
        assert_eq!(mouse_selection_cell(content, 1, 2, 0), Some((0, 0)));
        assert_eq!(
            mouse_selection_cell(content, 40, 20, 0),
            Some((2, 3)),
            "motion over a right-hand pane stays at the origin pane's bottom-right cell"
        );
        assert_eq!(
            mouse_selection_cell(content, 0, 0, 2),
            Some((-2, 0)),
            "copy-view coordinates retain their history offset"
        );
    }

    #[test]
    fn child_mouse_keeps_normal_input_but_shift_and_copy_mode_select() {
        let press = MouseEvent {
            button: 0,
            x: 3,
            y: 4,
            kind: MouseKind::Press,
            shift: false,
        };
        let mut modes = TerminalModes::default();
        assert!(starts_mouse_selection(press, false, modes));
        modes.mouse_clicks = true;
        assert!(!starts_mouse_selection(press, false, modes));
        assert!(starts_mouse_selection(
            MouseEvent {
                shift: true,
                ..press
            },
            false,
            modes
        ));
        assert!(starts_mouse_selection(press, true, modes));
    }

    #[test]
    fn pane_selection_runs_are_bounded_and_reverse_direction_is_equivalent() {
        let terminal = Terminal::new(3, 4, 0);
        let forward = MouseSelection {
            start: (0, 2),
            end: (2, 1),
            mode: MouseSelectionMode::Character,
        };
        let backward = MouseSelection {
            start: forward.end,
            end: forward.start,
            ..forward
        };
        let expected = vec![(0, 2, 2), (1, 0, 4), (2, 0, 2)];
        assert_eq!(mouse_selection_runs(&terminal, forward, 0, 4, 3), expected);
        assert_eq!(mouse_selection_runs(&terminal, backward, 0, 4, 3), expected);

        let line = MouseSelection {
            start: (0, 3),
            end: (1, 1),
            mode: MouseSelectionMode::Line,
        };
        assert_eq!(
            mouse_selection_runs(&terminal, line, 0, 4, 3),
            [(0, 0, 4), (1, 0, 4)]
        );
    }

    #[test]
    fn pane_selection_keeps_wide_glyphs_whole() {
        let mut terminal = Terminal::new(1, 4, 0);
        terminal.feed("界x".as_bytes());
        assert_eq!(
            normalize_mouse_selection_cell(&terminal, (0, 1)),
            (0, 0),
            "clicking the continuation addresses the leading cell"
        );
        let selection = MouseSelection {
            start: (0, 0),
            end: (0, 0),
            mode: MouseSelectionMode::Character,
        };
        assert_eq!(
            mouse_selection_runs(&terminal, selection, 0, 4, 1),
            [(0, 0, 2)]
        );
        assert_eq!(
            extract_mouse_selection(&terminal, selection),
            "界".as_bytes()
        );
    }

    #[test]
    fn pane_selection_preserves_tabs_and_combining_text_and_trims_padding() {
        let mut terminal = Terminal::new(1, 12, 0);
        terminal.feed("e\u{301}\tb  ".as_bytes());
        let selection = MouseSelection {
            start: (0, 0),
            end: (0, 11),
            mode: MouseSelectionMode::Line,
        };
        assert_eq!(
            extract_mouse_selection(&terminal, selection),
            "e\u{301}\tb".as_bytes()
        );
    }

    #[test]
    fn mouse_selection_survives_output_that_does_not_scroll() {
        let mut terminal = Terminal::new(4, 20, 10);
        terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let selection = MouseSelection {
            start: (1, 0),
            end: (1, 2),
            mode: MouseSelectionMode::Character,
        };
        // A plain redraw that rewrites cells without scrolling.
        let events = terminal.feed(b"\x1b[2;1HTWO");
        assert_eq!(
            pane_mouse_selection_after_output(
                Some(selection),
                &events,
                false,
                terminal.history_len()
            ),
            Some(selection)
        );
    }

    #[test]
    fn mouse_selection_rotates_with_scrollback_scroll() {
        let mut terminal = Terminal::new(4, 20, 10);
        terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let selection = MouseSelection {
            start: (1, 0),
            end: (2, 3),
            mode: MouseSelectionMode::Character,
        };
        let text = extract_mouse_selection(&terminal, selection);

        // One full-screen line scrolls into history; both endpoints shift up with their text.
        let events = terminal.feed(b"\r\nx");
        let rotated = pane_mouse_selection_after_output(
            Some(selection),
            &events,
            false,
            terminal.history_len(),
        )
        .unwrap();
        assert_eq!(
            rotated,
            MouseSelection {
                start: (0, 0),
                end: (1, 3),
                mode: MouseSelectionMode::Character,
            }
        );
        assert_eq!(extract_mouse_selection(&terminal, rotated), text);
    }

    #[test]
    fn mouse_selection_partially_scrolled_into_history_is_kept() {
        let mut terminal = Terminal::new(4, 20, 10);
        terminal.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let selection = MouseSelection {
            start: (0, 0),
            end: (1, 3),
            mode: MouseSelectionMode::Character,
        };

        // After the scroll, row -1 lives in history and row 0 on screen; both resolve.
        let events = terminal.feed(b"\r\nx");
        assert_eq!(
            pane_mouse_selection_after_output(
                Some(selection),
                &events,
                false,
                terminal.history_len()
            ),
            Some(MouseSelection {
                start: (-1, 0),
                end: (0, 3),
                mode: MouseSelectionMode::Character,
            })
        );
    }

    #[test]
    fn mouse_selection_drops_when_scrolled_past_retained_history() {
        let mut terminal = Terminal::new(2, 20, 2);
        terminal.feed(b"one\r\ntwo");
        let selection = MouseSelection {
            start: (0, 0),
            end: (0, 2),
            mode: MouseSelectionMode::Character,
        };

        let mut events = Vec::new();
        for _ in 0..6 {
            events.extend(terminal.feed(b"\r\nx"));
        }
        // The selected line was evicted from the two-line scrollback long ago.
        assert_eq!(
            pane_mouse_selection_after_output(
                Some(selection),
                &events,
                false,
                terminal.history_len()
            ),
            None
        );
    }

    #[test]
    fn mouse_selection_drops_only_when_region_scroll_intersects_it() {
        // Scroll region rows 2..4 (0-based 1..3) on a 4-row terminal.
        let mut terminal = Terminal::new(4, 20, 10);
        terminal.feed(b"\x1b[2;4r\x1b[4;1H");
        let events = terminal.feed(b"\r\n");

        let inside = MouseSelection {
            start: (2, 0),
            end: (2, 3),
            mode: MouseSelectionMode::Character,
        };
        assert_eq!(
            pane_mouse_selection_after_output(Some(inside), &events, false, terminal.history_len()),
            None,
            "a selection inside the scrolled region now points at different text"
        );

        let above = MouseSelection {
            start: (0, 0),
            end: (0, 3),
            mode: MouseSelectionMode::Character,
        };
        assert_eq!(
            pane_mouse_selection_after_output(Some(above), &events, false, terminal.history_len()),
            Some(above),
            "rows outside the scrolled region keep their coordinates"
        );
    }

    #[test]
    fn mouse_selection_drops_on_screen_clear_and_alt_screen_switch() {
        let selection = MouseSelection {
            start: (0, 0),
            end: (1, 3),
            mode: MouseSelectionMode::Character,
        };

        assert_eq!(
            pane_mouse_selection_after_output(Some(selection), &[TerminalEvent::Clear], false, 10),
            None
        );

        let mut terminal = Terminal::new(4, 20, 10);
        terminal.feed(b"one\r\ntwo");
        let events = terminal.feed(b"\x1b[?1049h");
        assert_eq!(
            pane_mouse_selection_after_output(
                Some(selection),
                &events,
                true,
                terminal.history_len()
            ),
            None
        );
    }

    #[test]
    fn triple_click_copies_visible_rows_instead_of_joining_soft_wraps() {
        let mut terminal = Terminal::new(3, 4, 0);
        terminal.feed(b"abcdefgh");
        assert!(terminal.line_wrapped(0).unwrap());

        let first_row = MouseSelection {
            start: (0, 2),
            end: (0, 2),
            mode: MouseSelectionMode::Line,
        };
        assert_eq!(extract_mouse_selection(&terminal, first_row), b"abcd");

        let two_rows = MouseSelection {
            end: (1, 0),
            ..first_row
        };
        assert_eq!(extract_mouse_selection(&terminal, two_rows), b"abcd\nefgh");

        let character = MouseSelection {
            start: (0, 0),
            end: (1, 3),
            mode: MouseSelectionMode::Character,
        };
        assert_eq!(
            extract_mouse_selection(&terminal, character),
            b"abcdefgh",
            "ordinary selection preserves the existing soft-wrap copy semantics"
        );
    }

    #[test]
    fn a_new_bridge_instance_accepts_a_lower_local_revision_without_regressing_wait_sequence() {
        assert!(bridge_apply_is_current(Some(11), 40, 73, 12, 40, 1));
        assert!(
            !bridge_apply_is_current(Some(11), 40, 73, 11, 40, 72),
            "the current bridge must still reject its own stale local acknowledgement"
        );
        assert_eq!(next_outer_compatibility_revision(73, 1), 74);
        assert_eq!(next_outer_compatibility_revision(4, 9), 9);
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

    #[test]
    fn pixel_mouse_is_hit_tested_in_cells_but_forwarded_in_local_pixels() {
        let display = DisplayMetrics {
            columns: 80,
            rows: 24,
            cell_width: 10,
            cell_height: 20,
        };
        let pixel_mouse = MouseEvent {
            button: 0,
            x: 155,
            y: 130,
            kind: MouseKind::Press,
            shift: false,
        };
        let cell_mouse = pixel_mouse_to_cells(pixel_mouse, display);
        assert_eq!((cell_mouse.x, cell_mouse.y), (15, 6));

        let content = Rect {
            x: 4,
            y: 2,
            width: 30,
            height: 10,
        };
        assert_eq!(
            application_mouse_coordinates(
                cell_mouse,
                Some((pixel_mouse.x, pixel_mouse.y)),
                content,
                display,
                true,
            ),
            (116, 91),
            "pane-local SGR-Pixels coordinates stay one-based and preserve sub-cell position"
        );
        assert_eq!(
            application_mouse_coordinates(cell_mouse, None, content, display, false),
            (12, 5),
            "cell-coordinate clients keep the existing SGR cell report"
        );
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
            name: None,
            tree: Some(tree),
            floating,
            focused: 1,
            last_focused_tiled: Some(1),
            zoomed: None,
            sync_input: false,
        }
    }

    fn status_tab(id: u64, name: Option<&str>) -> Tab {
        Tab {
            id,
            name: name.map(ToOwned::to_owned),
            tree: Some(TiledNode::leaf(id)),
            floating: FloatingLayer::default(),
            focused: id,
            last_focused_tiled: Some(id),
            zoomed: None,
            sync_input: false,
        }
    }

    #[test]
    fn status_lists_only_numbered_tabs_and_marks_the_active_one() {
        let tabs = [
            status_tab(41, Some("dev")),
            status_tab(99, None),
            status_tab(7, Some("logs\nprod")),
        ];
        let status = tab_status_text(&tabs, 1, 80);
        assert_eq!(status, " 1:dev [2] 3:logs prod ");
        assert!(!status.contains("id:"));
        assert!(!status.contains("rev:"));
        assert!(!status.contains("vvmux:"));
    }

    #[test]
    fn narrow_tab_status_keeps_the_active_segment_visible() {
        let tabs = [
            status_tab(1, Some("one")),
            status_tab(2, Some("two")),
            status_tab(3, Some("three")),
            status_tab(4, Some("four")),
        ];
        let status = tab_status_text(&tabs, 2, 14);
        assert!(status.contains("[3:three]"), "{status:?}");
        assert!(
            status.contains('<'),
            "left overflow must be visible: {status:?}"
        );
        assert!(
            status.contains('>'),
            "right overflow must be visible: {status:?}"
        );
        assert!(status.chars().count() <= 14);

        let long = [
            status_tab(1, Some("one")),
            status_tab(2, Some("a-name-that-is-much-too-long")),
            status_tab(3, Some("three")),
        ];
        let status = tab_status_text(&long, 1, 14);
        assert!(status.contains('<'), "{status:?}");
        assert!(status.contains('>'), "{status:?}");
        assert!(status.contains('[') && status.contains(']'), "{status:?}");
        assert!(status.chars().count() <= 14);
    }

    #[test]
    fn tab_rename_input_is_bounded_fragment_safe_and_editable() {
        let mut rename = TabRename {
            tab_id: 1,
            value: "dev".into(),
            pending_utf8: Vec::new(),
        };
        assert_eq!(
            apply_tab_rename_input(&mut rename, &[0xc3]),
            LineEditInput::Editing
        );
        assert_eq!(
            apply_tab_rename_input(&mut rename, &[0xa9, 0x7f, b'X']),
            LineEditInput::Editing
        );
        assert_eq!(rename.value, "devX");
        assert_eq!(
            apply_tab_rename_input(&mut rename, b"\r"),
            LineEditInput::Commit
        );

        rename.value = "x".repeat(MAX_TAB_NAME_BYTES);
        assert_eq!(
            apply_tab_rename_input(&mut rename, b"ignored"),
            LineEditInput::Editing
        );
        assert_eq!(rename.value.len(), MAX_TAB_NAME_BYTES);
        assert_eq!(
            apply_tab_rename_input(&mut rename, b"\x1b"),
            LineEditInput::Cancel
        );

        rename.pending_utf8.clear();
        assert_eq!(
            apply_tab_rename_input(&mut rename, &[0xc3, b'\r']),
            LineEditInput::Commit,
            "an invalid UTF-8 lead byte must not swallow Enter"
        );
    }

    #[test]
    fn sync_targets_include_hidden_live_panes_and_exclude_copy_mode() {
        let mut tab = tab_with_floats();
        tab.floating.ordinary_visible = false;
        tab.zoomed = Some(1);
        assert_eq!(
            sync_targets(&tab, &|pane| matches!(pane, 2 | 10)),
            [1, 11],
            "visibility and zoom do not remove live targets, but copy mode does"
        );

        let empty = Tab {
            id: 9,
            name: None,
            tree: None,
            floating: FloatingLayer::default(),
            focused: 99,
            last_focused_tiled: None,
            zoomed: None,
            sync_input: true,
        };
        assert!(sync_targets(&empty, &|_| false).is_empty());
    }

    #[test]
    fn a_removed_sync_target_is_a_no_op() {
        let mut panes = BTreeMap::new();
        assert!(queue_input_targets(&mut panes, &[7], b"ignored").is_empty());
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
    fn repeated_identical_displays_are_not_resizes() {
        let display = DisplayMetrics {
            columns: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        };

        // The first display from a newly attached client is always a change.
        assert!(is_display_change(None, display));

        // A browser re-measurement that reports the same geometry must not relayout: doing so
        // bumps layout_revision and rebuilds the outer Vivid session, which destroys an image
        // that is still being projected.
        assert!(!is_display_change(Some(display), display));

        for changed in [
            DisplayMetrics {
                columns: 81,
                ..display
            },
            DisplayMetrics {
                rows: 25,
                ..display
            },
            DisplayMetrics {
                cell_width: 9,
                ..display
            },
            DisplayMetrics {
                cell_height: 17,
                ..display
            },
        ] {
            assert!(
                is_display_change(Some(display), changed),
                "a genuine geometry change must still resize: {changed:?}"
            );
        }
    }

    #[test]
    fn projection_sync_does_not_duplicate_the_triggering_live_raster() {
        let raster = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
        };
        assert!(!should_replay_retained(
            raster,
            Some(raster),
            false,
            false,
            false
        ));
        assert!(should_replay_retained(raster, None, false, true, false));
        assert!(should_replay_retained(
            raster,
            Some(BridgeSourceKey { track: 8, ..raster }),
            true,
            false,
            false
        ));
        assert!(
            !should_replay_retained(raster, None, true, true, false),
            "an already-presented retained body must not cross IPC again while its outer source \
             remains resident"
        );
        assert!(
            should_replay_retained(raster, None, true, true, true),
            "a recreated outer raster has no pixels even when an older attachment was presented"
        );
    }

    #[test]
    fn recreated_retained_replay_is_projection_and_owner_scoped() {
        let raster = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
        };
        let other_owner = BridgeSourceKey {
            producer: 4,
            ..raster
        };
        let candidates = HashSet::from([raster, other_owner]);

        assert_eq!(
            retained_replays_after_apply(&[raster], &candidates, &HashSet::new(), &HashSet::new()),
            HashSet::from([raster])
        );
        assert!(
            retained_replays_after_apply(
                &[raster],
                &HashSet::new(),
                &HashSet::new(),
                &HashSet::new()
            )
            .is_empty(),
            "initial outer creation must wait for its following live raster instead of replaying it"
        );
        assert!(
            retained_replays_after_apply(
                &[raster],
                &candidates,
                &HashSet::from([raster]),
                &HashSet::new()
            )
            .is_empty()
        );
        assert!(
            retained_replays_after_apply(
                &[raster],
                &candidates,
                &HashSet::new(),
                &HashSet::from([raster])
            )
            .is_empty()
        );
        assert_eq!(
            retained_replays_after_apply(
                &[raster],
                &HashSet::from([other_owner]),
                &HashSet::new(),
                &HashSet::new()
            ),
            HashSet::new(),
            "a same-numbered source from another producer cannot authorize replay"
        );
    }

    #[test]
    fn composed_retained_raster_becomes_a_self_contained_replay_body() {
        let pixels = Arc::<[u8]>::from([0x10, 0x20, 0x30, 0xff, 0x40, 0x50, 0x60, 0xff]);
        let retained = crate::media::RetainedRaster {
            epoch: 3,
            frame_id: 19,
            width: 2,
            height: 1,
            pixels: pixels.clone(),
        };

        let body = retained_raster_body(&retained).unwrap();
        let frame = vivid_protocol::media::parse_full_raster_frame(&body).unwrap();
        assert_eq!(frame.epoch, 3);
        assert_eq!(frame.frame_id, 19);
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 1);
        assert_eq!(
            vivid_protocol::media::decode_raster_pixels(frame).unwrap(),
            &*pixels
        );
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
            config: crate::media::SceneNodeConfig {
                node: crate::media::NodeConfig {
                    node_id: 9,
                    track: BridgeSourceKey {
                        producer: 3,
                        context: 1,
                        surface: 4,
                        track: 5,
                    },
                    x: 0,
                    y: 0,
                    width,
                    height,
                    z_index: 2,
                    visible: true,
                    anchor_id: None,
                },
                clip: None,
            },
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
            validate_automation_method(&AutomationMethod::Action(Action::CopyInput(vec![b'q'])))
                .is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::Action(Action::Plugin(
                "plugin:dev.example/run".into()
            )))
            .is_ok()
        );
        assert!(
            validate_automation_method(&AutomationMethod::Action(Action::Plugin(
                "dev.example/run".into()
            )))
            .is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::Key {
                key: "x".into(),
                modifiers: Vec::new(),
                repeat: 0,
                report: false,
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
            validate_automation_method(&AutomationMethod::Search {
                pattern: "x".repeat(crate::search::MAX_PATTERN_BYTES + 1),
                regex: false,
                direction: SearchDirection::Forward,
                start_line: None,
                start_column: None,
                limit: 1,
            })
            .is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::Search {
                pattern: "x".into(),
                regex: false,
                direction: SearchDirection::Forward,
                start_line: None,
                start_column: Some(1),
                limit: 1001,
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
        assert!(
            validate_automation_method(&AutomationMethod::TraceMedia {
                after_sequence: None,
                limit: 0,
                timeout_ms: 0,
                filter: MediaTraceFilter::default(),
            })
            .is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::TraceMedia {
                after_sequence: None,
                limit: 32,
                timeout_ms: 0,
                filter: MediaTraceFilter {
                    context_id: Some(4),
                    ..MediaTraceFilter::default()
                },
            })
            .is_err()
        );
        assert!(
            validate_automation_method(&AutomationMethod::TraceMedia {
                after_sequence: None,
                limit: 32,
                timeout_ms: 0,
                filter: MediaTraceFilter::default(),
            })
            .is_ok()
        );
    }
}
