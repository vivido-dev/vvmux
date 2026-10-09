use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
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

use crate::agent::{
    AgentRuntime, AgentSnapshot, AgentStatus, DetectorHandle, ProbeTarget, ProcessUpdate,
};
use crate::config::{Config, OpenMode};
use crate::ipc::{
    Action, AttachmentTarget, AutomationCompletion, AutomationError, AutomationMethod,
    AutomationRequest, AutomationResponse, Axis, ClientMessage, Direction, FloatingEditCommand,
    FloatingEditKind, MediaTrackIdentity, MediaTrackWaitCondition, MouseEvent, MouseKind,
    PluginEventEnvelope, ServerMessage, SharedWriter, TabSelector, TextSource,
};
use crate::layout::{
    EdgeMask, FloatOrigin, FloatingLayer, PaneId, PaneLayer, PaneProjection, Rect, TiledNode,
    directional_focus,
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
use crate::session_state::{
    FloatExtras, HistoryColor, HistoryRow, HistoryRun, HistoryStyle, PaneAgentExtras, PaneExtras,
    PaneHistory, SessionHistory, SessionSnapshot, SnapshotExtras, TabExtras, TabHistory,
};
use crate::tab_view::{SidebarLine, SidebarPane, SidebarTab, SidebarTarget, TabView};
use vivid_sdk::presenter::{
    BridgeClipRect, BridgeNode, BridgeOverlayWindow, BridgePlayRequest, BridgeSource,
    BridgeSourceDescriptor, BridgeSourceKey, BridgeSourceKind, BridgeSurface, BridgeSurfaceKey,
    DisplayMetrics,
};

mod agents;
mod automation;
mod clients;
mod commands;
mod input;
mod overlays;
mod pane_ops;
mod panes;
mod persistence;
mod render;
mod waiters;

pub(crate) use input::copy_action_bytes;

/// Capacity of the session actor's event channel. Producers block when it is full, so this bounds
/// memory without dropping events; larger only adds latency under a burst.
const EVENT_QUEUE: usize = 1024;
/// Maximum adjacent output bytes one actor turn parses for a pane.
///
/// PTYs may return a large write as many tiny reads. Combining only consecutive records for the
/// same pane avoids repeating whole-grid and waiter bookkeeping while keeping a hard fairness
/// boundary and preserving every intervening actor event.
const PTY_OUTPUT_BATCH_BYTES: usize = 64 * 1024;
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
/// Largest copy-mode selection or OSC 52 store kept, in bytes; matches the terminal's OSC 52 decode
/// ceiling so the two cannot disagree.
const COPY_BUFFER_LIMIT: usize = 1024 * 1024;
/// Longest gap between clicks that still counts as a double or triple click; the common desktop
/// default.
const MOUSE_MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Most rectangles one media node may be split into when floating panes occlude it; a node needing
/// more is hidden rather than drawn with an unbounded clip.
const MAX_NODE_FRAGMENTS: usize = 8;
/// Most media nodes projected to the outer presenter at once, bounding the scene the outer terminal
/// must composite.
const MAX_PROJECTED_NODES: usize = 256;
/// Shortest interval between input-status notices shown to the user, so a key held down cannot
/// flood the status line.
const INPUT_STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Most automation requests one client may have in flight; further requests are refused so one
/// client cannot exhaust the actor's waiters.
const MAX_AUTOMATION_REQUESTS_PER_CLIENT: usize = 64;
/// Most automation requests waiting on a condition across the session; each holds a reply slot
/// until it resolves or times out.
const MAX_AUTOMATION_WAITERS: usize = 256;
/// Most background jobs whose results the actor is waiting for; work beyond this is refused rather
/// than queued without bound.
const MAX_PENDING_ACTOR_WORK: usize = 256;
/// Hard ceiling on live panes per session, enforced by every automation pane creator
/// (`split`, `run` with placement, `new-tab`). Layout files keep their own lower
/// [`crate::layout_file::MAX_LAYOUT_PANES`] fork bound.
const MAX_SESSION_PANES: usize = 256;
/// How long shape changes coalesce before a snapshot is written.
///
/// Long enough that dragging a split or opening several panes is one write, short enough that a
/// crash loses a few seconds of shape rather than a session.
const SNAPSHOT_DEBOUNCE: Duration = Duration::from_secs(5);
/// Scrollback lines one pane contributes to a persisted history.
const HISTORY_MAX_ROWS: usize = 2000;
/// Bytes one pane contributes, whatever its line count.
const HISTORY_MAX_PANE_BYTES: usize = 256 * 1024;
/// Text bytes a whole session's history may capture, whatever its pane count.
///
/// Below `session_state::MAX_HISTORY_BYTES`, which bounds the serialized file: this counts text,
/// and the file additionally carries JSON structure, so the two must not be equal or a maximal
/// capture would be refused by its own writer.
const HISTORY_MAX_SESSION_BYTES: usize = 4 * 1024 * 1024;

/// A restored pane's agent and the conversation it will reopen.
///
/// Deliberately not the command line. The argv is resolved when the resume fires, not when it is
/// armed: the agent catalog is compiled from the plugin registry and arrives by event *after* the
/// actor is constructed, so at restore time there is nothing to resolve against. Resolving late also
/// means a provider disabled between restore and attach simply does not resume, rather than running
/// a command built from a registry that no longer applies.
#[derive(Debug, Clone)]
struct AgentResumePlan {
    kind: crate::agent::AgentId,
    /// The alias this agent carried, reapplied once it is actually running again.
    alias: Option<crate::agent::AgentAlias>,
    /// The integration that reported the session, checked against the provider at fire time.
    source: String,
    session: crate::agent::AgentSessionRef,
    /// Identifies one conversation, so two panes cannot reopen the same one.
    dedupe_key: String,
}

/// What one capture walk produces: the state a layout file cannot describe, and the slot-to-pane
/// mapping that only the walk itself knows.
#[derive(Default)]
struct SnapshotCapture {
    extras: SnapshotExtras,
    /// Per captured tab, the pane holding each slot. Never persisted — pane IDs do not survive a
    /// restart, which is exactly why the persisted form is keyed by slot.
    slots: Vec<Vec<PaneId>>,
}
/// How long to wait before retrying a capture that found a write already in flight.
const SNAPSHOT_WRITE_RETRY: Duration = Duration::from_millis(250);
/// Delayed writes outstanding across the session.
///
/// One prompt schedules one Enter, so this is far above any real load; it exists so a caller
/// looping on prompts cannot grow the heap without bound.
const MAX_DELAYED_INPUTS: usize = 256;
/// Most projection snapshots queued for a client; older ones are superseded, because only the
/// newest projection matters.
const MAX_PENDING_MEDIA_PROJECTIONS: usize = 64;
/// Largest single read from a pane's PTY, forwarded to the actor as one output event.
///
/// 64 KiB drains a burst such as `cat` of a large file in few events without making one event
/// expensive to parse; PTY drivers rarely return more than a few KiB per read anyway.
const PTY_READ_CHUNK: usize = 64 * 1024;
/// Longest the actor sleeps when no timer is due, so housekeeping still runs on an idle session.
const IDLE_WAKE_INTERVAL: Duration = Duration::from_secs(1);
/// How recently a microphone packet must have arrived for the status line to show it as active.
const MICROPHONE_ACTIVE_WINDOW: Duration = Duration::from_millis(200);
/// Frames allowed outstanding before rendering pauses for the client to catch up.
///
/// The acknowledgement arrives after the client writes the frame to its terminal, so this bounds
/// how far the server may run ahead of what the user can actually see. Media snapshots and media
/// records are never gated by it: a slow terminal must not stall the projected scene.
const MAX_UNACKNOWLEDGED_FRAMES: u64 = 8;
/// Clients attached to one session at once. Each is composed and diffed separately every frame,
/// so the bound is what keeps a render turn's cost fixed.
const MAX_ATTACHED_CLIENTS: usize = 16;
/// Kitty graphics bytes buffered across the session for direct-attach forwarding; a packet that
/// would exceed it is dropped, along with the rest of its transfer, instead of growing memory.
const KITTY_GRAPHICS_SESSION_BYTES: usize = 64 * 1024 * 1024;
/// Capacity of the channel to the automation response writer; small because each entry can be large
/// and the writer drains it promptly.
const AUTOMATION_RESPONSE_QUEUE: usize = 8;
/// Screen-change records kept per pane for `wait-screen-change`; older records are forgotten, which
/// only shortens how far back a waiter can look.
const SCREEN_CHANGE_HISTORY: usize = 1024;
/// Exited panes remembered so `wait-exit` can still report them after the pane is gone.
const EXIT_TOMBSTONES: usize = 128;
/// Plugin events kept for replay to late subscribers; with [`PLUGIN_EVENT_JOURNAL_BYTES`] it bounds
/// the journal, and overflow becomes an explicit sequence gap.
const PLUGIN_EVENT_JOURNAL: usize = 1024;
/// Serialized bytes the plugin event journal may hold; see [`PLUGIN_EVENT_JOURNAL`].
const PLUGIN_EVENT_JOURNAL_BYTES: usize = 2 * 1024 * 1024;
/// Most concurrent plugin event subscriptions per session.
const PLUGIN_EVENT_SUBSCRIPTIONS: usize = 16;
/// Capacity of each subscription's event channel, which also caps how many journal entries one
/// replay sends.
const PLUGIN_EVENT_STREAM_QUEUE: usize = 64;
/// How long an automation input waits for its PTY write to complete before reporting failure; a
/// pane that stops reading its input must not hold the request forever.
const PTY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest automation reply, in bytes; requests whose result would exceed it are refused up front.
/// Matches the IPC automation response limit.
const AUTOMATION_REPLY_LIMIT: usize = 16 * 1024 * 1024;
/// A `run` command is one shell command line, not a script; this only has to be generous.
const MAX_RUN_COMMAND_BYTES: usize = 64 * 1024;
/// Longest tab name, in bytes, so the tab bar stays bounded however a name is set.
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
        source: TextSource,
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
    /// A session snapshot write finished on its worker.
    SnapshotWritten {
        result: Result<(), String>,
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
    PluginRegistrationsApplied {
        generation: u64,
        keybindings: Vec<crate::ipc::PluginKeybinding>,
        link_handlers: Vec<crate::plugin::LinkRegistration>,
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
    /// A one-shot foreground probe finished for a parked automation request.
    ///
    /// Distinct from `AgentProcesses`, which reports the detector's periodic view: this answers a
    /// specific request that must not act on a cached identity.
    // Constructed once `agent-start` and `agent-prompt` park requests on a probe.
    AgentProbeComplete {
        reply: AutomationReplyTarget,
        pane_id: PaneId,
        probe: AgentProbe,
        job: ForegroundProbe,
    },
}

/// Bytes queued to reach a pane's PTY at `due`.
///
/// Full-screen agents read a bracketed paste and its submitting Enter as one event when they
/// arrive together, swallowing the Enter into the pasted text. Separating them in time is what
/// makes the prompt actually submit, so the delay is a feature of the input, not a retry.
struct DelayedInput {
    due: Instant,
    /// Breaks ties so equal deadlines drain in enqueue order; `Instant` has no such guarantee and
    /// a heap is free to reorder equal keys.
    sequence: u64,
    pane_id: PaneId,
    bytes: Vec<u8>,
}

impl PartialEq for DelayedInput {
    fn eq(&self, other: &Self) -> bool {
        (self.due, self.sequence) == (other.due, other.sequence)
    }
}

impl Eq for DelayedInput {}

impl Ord for DelayedInput {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.due, self.sequence).cmp(&(other.due, other.sequence))
    }
}

impl PartialOrd for DelayedInput {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// What a parked request asked the foreground probe to decide.
#[derive(Debug, Clone)]
pub enum AgentProbe {
    /// Whether the pane is an available shell, for a caller about to launch an agent in it.
    ///
    /// Carries the launch it gates, because the answer is only useful together with the command
    /// the shell name decides how to quote.
    ShellAvailable {
        agent: crate::agent::AgentId,
        argv: Vec<String>,
        timeout_ms: u64,
    },
    /// Whether `agent` is still the pane's foreground process, for a caller about to prompt it.
    HostsAgent {
        agent: crate::agent::AgentId,
        prompt: Option<PendingAgentPrompt>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct PendingAgentPrompt {
    text: String,
    wait: bool,
    until: Vec<crate::agent::AgentStatus>,
    timeout_ms: u64,
    baseline_seq: u64,
    baseline_status: Option<AgentStatus>,
}

/// One foreground probe's result: the agent the job resolves to, plus the raw job behind it.
#[derive(Debug, Clone)]
pub struct ForegroundProbe {
    pub identity: Option<crate::agent::AgentIdentity>,
    pub processes: Vec<crate::agent_drive::ForegroundProcess>,
}

#[derive(Clone)]
pub struct AutomationReplyTarget {
    client_id: u64,
    request_id: u64,
    writer: SharedWriter,
    cancel: crate::platform::ConnectionCancel,
    /// The key this request claimed, so its successful reply is what a retry replays.
    ///
    /// Carried on the reply target rather than looked up again because a reply can be produced
    /// long after dispatch — a wait resolves from a timer, a media trace from an event — and by
    /// then the request is gone.
    idempotency_key: Option<String>,
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
    filter: crate::ipc::EventFilter,
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
    pub terminated: Arc<AtomicBool>,
}

struct ActorTermination(Arc<AtomicBool>);

impl Drop for ActorTermination {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
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

/// One attached terminal client.
///
/// Everything that describes what a particular terminal shows or has acknowledged lives here, so
/// a slow or departing client never stalls or resets another. What the clients share — the view,
/// the layout, the panes — stays on the actor.
struct AttachedClient {
    id: u64,
    writer: SharedWriter,
    /// This client's own terminal. The layout follows `general.window_size` across all clients;
    /// a client whose terminal differs sees it clipped or padded, with its tab list at its edge.
    display: DisplayMetrics,
    vivid: bool,
    media_enabled: bool,
    shared_visuals: SharedVisualState,
    kitty_graphics: bool,
    read_only: bool,
    /// Whether this client's host terminal holds focus. Assumed true at attach, because a client
    /// attaches into a focused window.
    focused: bool,
    /// Session-wide ordinal of this client's last attach, resize, or input. `latest` sizing
    /// follows the highest.
    activity: u64,
    /// Frames are numbered per client, so flow control counts only this client's backlog.
    frame_id: u64,
    acknowledged_frame: u64,
    rendered_session_sequence: u64,
    frame_sequences: VecDeque<(u64, u64)>,
    /// The screen this client last received, which its next frame is diffed against.
    last_screen: Option<ScreenBuffer>,
    force_full: bool,
    /// A change this client has not been sent yet. Held while its frame backlog is full, so a
    /// slow client catches up once without delaying anyone else.
    render_pending: bool,
    /// Click targets from this client's last frame. Its tab list sits at its own terminal's edge,
    /// so the targets differ between clients whose sizes differ.
    status_tab_targets: Vec<(std::ops::Range<usize>, u64)>,
    sidebar_targets: Vec<(u16, SidebarLine)>,
    /// Last focused-pane keyboard and mouse coordinate modes sent to this host terminal.
    reported_input_mode: Option<(u8, bool)>,
    ipc: Arc<crate::metrics::IpcCounters>,
    #[cfg(windows)]
    outer_bracketed_paste: Option<bool>,
    /// Which Vivido window hosts this client, as the client reported it.
    ///
    /// Replaced wholesale on every attach, because a reattach can be a different window.
    outer: Option<crate::ipc::OuterIdentity>,
}

impl AttachedClient {
    /// Whether this client could present media at all: a Vivid bridge or a Kitty graphics host.
    fn media_capable(&self) -> bool {
        self.vivid || self.kitty_graphics
    }

    fn render_blocked(&self) -> bool {
        self.frame_id.saturating_sub(self.acknowledged_frame) >= MAX_UNACKNOWLEDGED_FRAMES
    }
}

/// Retained subscriptions never own producer credit or playback state. One body per source may
/// be outstanding; subsequent revisions are read from the latest retained canvas after its ack.
#[derive(Default)]
struct SharedVisualState {
    bridge_instance: Option<u64>,
    revision: u64,
    pending_revision: Option<u64>,
    pending_sources: HashMap<BridgeSourceKey, u64>,
    applied_sources: HashMap<BridgeSourceKey, u64>,
    outer_revision: u64,
    attachment_generations: HashMap<BridgeSourceKey, u64>,
    last_projection: Option<MediaProjectionKey>,
    sent: HashMap<BridgeSourceKey, (u64, u64)>,
    inflight: HashSet<BridgeSourceKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachmentView {
    Session,
    Pane(PaneId),
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
    /// The agent snapshot this pane has already accounted for.
    ///
    /// Agent state is mutated from several unrelated causes, so the live snapshot alone cannot
    /// say whether a transition is new. `sync_agent_status` compares against this and stores the
    /// result, making a transition observable exactly once whatever produced it.
    agent_published: Option<AgentSnapshot>,
    /// Count of published agent lifecycle transitions on this pane.
    ///
    /// A caller that submits work and then asks "did anything happen?" needs a number that moves
    /// only on lifecycle change. `screen_sequence` cannot answer: a spinner or a clock redraws
    /// constantly without the agent's state meaning anything different.
    agent_change_seq: u64,
    /// A resume this pane will run once a client attaches, from a restored snapshot.
    pending_resume: Option<AgentResumePlan>,
    /// A name waiting for the agent it belongs to to come back.
    ///
    /// Held here rather than set on the agent runtime directly, because the resume has only just
    /// been typed: the agent's process arrives a moment later, and `observe_process` clears the
    /// runtime's alias when a new foreground group appears — correctly, since it cannot tell a
    /// resumed agent from a different program the user started. The name is applied once the agent
    /// is actually detected.
    pending_alias: Option<crate::agent::AgentAlias>,
    /// A bounded rolling window of this pane's raw output.
    transcript: PaneTranscript,
    /// The durable name a user gave this pane, unique within the session.
    ///
    /// Survives a server restart, unlike the pane's ID: see [`crate::layout::PaneName`].
    name: Option<crate::layout::PaneName>,
    copy: Option<CopyState>,
    mouse_selection: Option<MouseSelection>,
    vivid_metrics: Option<(u16, u16, u16, u16)>,
    /// Temporary cell-metric multiplier while a scaled capture is in flight.
    ///
    /// A producer sizes its raster to the pane viewport in pixels, and the only lever the closed
    /// `terminal-surface-v1` descriptor offers for that is the cell size. Raising it asks the
    /// producer to re-render the same grid at a higher density, which is what makes small text
    /// legible in a capture; it is cleared again as soon as the capture finishes or fails.
    capture_scale: Option<u32>,
    /// Whether the pane leaves its background to the outer terminal rather than painting one.
    ///
    /// A transparent pane's default-background cells stay SGR 49, so a translucent host window
    /// shows the desktop through them. An opaque pane substitutes `theme.pane_background` during
    /// composition and reads as a solid panel against transparent neighbours.
    transparent: bool,
    /// Whether the pane stays open after its process exits, keeping the output readable.
    hold_on_exit: bool,
    /// Set once a held pane's process has exited, so the corpse is reported only once.
    exit_status: Option<PtyExitStatus>,
    /// Whether this pane was last told it holds the host terminal's focus. A pane that has not
    /// enabled focus reporting still tracks it, so enabling the mode later reports no stale event.
    focus_reported: bool,
    /// Whether this pane's client keystrokes are inside a host bracketed paste, whose contents
    /// `key_input_bytes` must pass through untranslated even when a paste spans input messages.
    key_paste: bool,
    last_input_warning: Option<Instant>,
    screen_sequence: u64,
    last_screen_change: Instant,
    screen_changes: VecDeque<ScreenChange>,
    role: PaneRole,
    /// How this pane's process was started, with its working directory resolved, so a respawn
    /// runs the same command in the same place.
    spawn: PaneSpawn,
}

#[derive(Debug, Clone)]
struct ScreenChange {
    sequence: u64,
    rows: Option<Vec<usize>>,
    /// When this change landed, so stability can be measured over a subset of the screen rather
    /// than only against the pane's single most recent change.
    at: Instant,
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

/// How long the layers must stop changing before a scaled capture is taken.
const CAPTURE_SETTLE_QUIET: Duration = Duration::from_millis(250);
/// How long a scaled capture waits for quiet before taking whatever is there.
const CAPTURE_SETTLE_LIMIT: Duration = Duration::from_secs(3);

/// What the settle state says to do next, kept pure so the timing rule can be tested directly.
#[derive(Debug, PartialEq, Eq)]
enum CaptureSettleStep {
    /// Nothing to capture yet, carrying forward new settle state when it changed.
    Wait(Option<CaptureSettling>),
    /// Frames have gone quiet, or waited long enough; take the capture.
    Capture,
}

/// Decide whether a scaled capture may be taken yet.
///
/// The rule exists because a producer answers a cell-metric change by replacing its raster track,
/// and that replacement track's first frame is the blank buffer it was created with. Resolving on
/// the first observed change therefore captures an empty page — which is exactly what it did
/// before this was a settle. Waiting for the frames to stop instead catches the render that
/// follows, and the give-up bound keeps a pane that never stops animating from waiting forever.
fn capture_settle_step(
    baseline: &[CaptureIdentity],
    current: Vec<CaptureIdentity>,
    settling: Option<&CaptureSettling>,
    now: Instant,
) -> CaptureSettleStep {
    if current == baseline {
        // The resize has not reached the producer yet.
        return CaptureSettleStep::Wait(None);
    }
    let Some(settling) = settling else {
        return CaptureSettleStep::Wait(Some(CaptureSettling {
            seen: current,
            quiet_since: now,
            give_up_at: now + CAPTURE_SETTLE_LIMIT,
        }));
    };
    if settling.seen != current {
        return CaptureSettleStep::Wait(Some(CaptureSettling {
            seen: current,
            quiet_since: now,
            give_up_at: settling.give_up_at,
        }));
    }
    if now >= settling.quiet_since + CAPTURE_SETTLE_QUIET || now >= settling.give_up_at {
        return CaptureSettleStep::Capture;
    }
    CaptureSettleStep::Wait(None)
}

/// What one poll of a scaled capture decided.
enum ScaledCapturePoll {
    /// Keep waiting, optionally with new settle state to carry forward.
    Pending(Option<CaptureSettling>),
    Done(Result<serde_json::Value, AutomationError>),
}

/// One layer's identity, enough to notice a producer re-rendering: the node, the raster size it
/// rendered at, and which frame it is.
type CaptureIdentity = (u64, u32, u32, Option<u64>);

/// Waiting out the frames that follow a capture resize.
///
/// A producer answers a cell-metric change by replacing its raster track, and the first frame on
/// that new track is the blank buffer it was created with — the rendered content arrives after it.
/// Capturing on the first change therefore captures nothing, so this waits for the frames to stop
/// arriving instead.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureSettling {
    seen: Vec<CaptureIdentity>,
    quiet_since: Instant,
    /// Capture regardless past this point, so a pane that never stops animating still answers.
    give_up_at: Instant,
}

struct PendingAgentRead {
    reply: AutomationReplyTarget,
    read: crate::alt_read::PendingAltRead,
    lines: usize,
    json: bool,
}

enum AutomationWaitKind {
    /// A scaled capture, waiting for the producer to re-render at the raised cell metrics.
    ///
    /// `baseline` is what the pane's layers looked like before the resize, so the wait resolves on
    /// the producer having actually produced something new rather than on a timer. The pane's
    /// `capture_scale` must be cleared on every exit from this wait, the timeout included, or the
    /// pane is left resized.
    CaptureMedia {
        path: String,
        baseline: Vec<CaptureIdentity>,
        /// Set once the resize has landed, to wait out the frames that follow it.
        settling: Option<CaptureSettling>,
    },
    Text {
        pattern: AutomationTextPattern,
        after_screen: Option<u64>,
    },
    /// Match against a pane's raw output stream rather than its screen.
    ///
    /// The difference that matters: a screen wait can miss text entirely, because the pane may
    /// overwrite it before any snapshot runs. Output is matched as it arrives.
    Output {
        pattern: AutomationTextPattern,
        after_offset: Option<u64>,
    },
    /// A command running in a pane's own shell, waiting for its OSC 133 completion marker.
    ShellCommand {
        /// The command counter before the command was submitted; completion must be past it.
        after_command_id: u64,
        started_screen: u64,
    },
    /// Settle, then read: the wait half of the `capture` composite.
    ///
    /// One waiter rather than a chain of them because the read has to happen at the moment the
    /// condition holds. A caller that waited and then read separately would be reading a screen
    /// that had moved on again.
    Capture {
        after_screen: Option<u64>,
        quiet: Option<Duration>,
        rendered_after_session: Option<u64>,
        grid: bool,
    },
    ScreenChange {
        after_screen: u64,
    },
    ScreenStable {
        quiet: Duration,
        /// Rows at the bottom of the pane that do not count as activity.
        ignore_bottom: u16,
        after_screen: Option<u64>,
    },
    Rendered {
        after_session: u64,
    },
    Exit,
    AgentState {
        until: Vec<AgentStatus>,
        /// The status when the wait was registered, so a caller can see what it moved from.
        /// `None` when the pane had no agent yet.
        initial: Option<AgentStatus>,
    },
    /// A launch typed into a shell, waiting for that pane to be running the agent it named.
    AgentLaunch {
        agent: crate::agent::AgentId,
        argv: Vec<String>,
        /// When absence stops meaning "still starting" and starts meaning "did not start".
        ready_after: Instant,
    },
    AgentPrompt {
        phase: AgentPromptPhase,
        until: Vec<AgentStatus>,
        baseline_status: Option<AgentStatus>,
    },
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

enum AgentPromptPhase {
    Stall {
        baseline_seq: u64,
        stall_deadline: Instant,
    },
    Settle,
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

/// Queue keystrokes a client typed, encoded for the pane's cursor-key mode.
///
/// Host terminals encode keys for their own modes, and vvmux does not mirror a pane's DECCKM to
/// them, so arrows arrive in the normal `CSI` form whatever the pane asked for. A program that
/// enabled application cursor keys may recognise only the `SS3` form — Python's REPL reads
/// terminfo's `kcuu1`, `ESC O A`, after sending `smkx` — so the pane receives what a terminal in
/// its mode would have sent.
fn queue_key_input(pane: &mut Pane, bytes: &[u8]) -> Option<InputFailure> {
    let bytes = key_input_bytes(bytes, pane.terminal.modes(), &mut pane.key_paste);
    queue_pane_input(pane, &bytes)
}

/// Rewrite unmodified normal-mode cursor keys to their application-mode form when `modes` asks
/// for it, leaving bracketed-paste contents and every other sequence byte-for-byte intact.
fn key_input_bytes<'a>(
    bytes: &'a [u8],
    modes: TerminalModes,
    in_paste: &mut bool,
) -> Cow<'a, [u8]> {
    // Under Kitty's report-all-keys flag every key is already an unambiguous `CSI` sequence.
    let translate = modes.application_cursor && modes.keyboard_flags & 8 == 0;
    let mut output: Option<Vec<u8>> = None;
    let mut index = 0;
    while index < bytes.len() {
        let rest = &bytes[index..];
        if rest.starts_with(b"\x1b[200~") {
            *in_paste = true;
        } else if rest.starts_with(b"\x1b[201~") {
            *in_paste = false;
        } else if translate
            && !*in_paste
            && let [
                0x1b,
                b'[',
                final_byte @ (b'A' | b'B' | b'C' | b'D' | b'H' | b'F'),
                ..,
            ] = rest
        {
            let output = output.get_or_insert_with(|| bytes[..index].to_vec());
            output.extend_from_slice(&[0x1b, b'O', *final_byte]);
            index += 3;
            continue;
        }
        if let Some(output) = &mut output {
            output.push(bytes[index]);
        }
        index += 1;
    }
    output.map_or(Cow::Borrowed(bytes), Cow::Owned)
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

/// Bytes to be shown as text later.
///
/// The last resort for observing output that was never on screen long enough to snapshot: a
/// progress line overwritten by carriage returns, or output that scrolled past between two polls.
/// `get-text` reads the grid, which by then holds whatever replaced it.
///
/// In memory and bounded, and deliberately not the opt-in on-disk `[session] pane_history`: this
/// is a small rolling window a caller can wait on, not a recording of everything a pane ever
/// printed. Nothing here is written anywhere.
#[derive(Debug, Default)]
struct PaneTranscript {
    bytes: VecDeque<u8>,
    /// Total bytes ever fed to this pane, which is the offset just past the end of `bytes`.
    ///
    /// Monotonic and never reset, so an offset a caller read stays comparable even after the
    /// window it pointed into has been overwritten — which is how a gap is detectable at all.
    offset: u64,
}

impl PaneTranscript {
    fn push(&mut self, chunk: &[u8]) {
        self.offset = self.offset.saturating_add(chunk.len() as u64);
        // A chunk larger than the whole window keeps only its tail; the older bytes were going to
        // be dropped by the loop below regardless, and copying them first is wasted work.
        let tail = chunk.len().min(MAX_TRANSCRIPT_BYTES);
        self.bytes.extend(&chunk[chunk.len() - tail..]);
        while self.bytes.len() > MAX_TRANSCRIPT_BYTES {
            let excess = self.bytes.len() - MAX_TRANSCRIPT_BYTES;
            self.bytes.drain(..excess);
        }
    }

    /// The offset of the oldest byte still retained.
    fn start(&self) -> u64 {
        self.offset.saturating_sub(self.bytes.len() as u64)
    }

    /// Bytes from `after` onward, and whether anything between `after` and them was dropped.
    fn since(&self, after: Option<u64>) -> (Vec<u8>, bool) {
        let start = self.start();
        let after = after.unwrap_or(start);
        let gap = after < start;
        let from = after.max(start).min(self.offset);
        let skip = (from - start) as usize;
        (self.bytes.iter().skip(skip).copied().collect(), gap)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseSelectionMode {
    Character,
    Word,
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

struct CompiledLinkRegistration {
    regex: regex::Regex,
    action: String,
}

struct PluginLinkPress {
    pane: PaneId,
    content: Rect,
    cell: (isize, usize),
    uri: String,
    action: String,
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

/// The right-click pane menu, anchored where it was opened and bound to one pane.
///
/// Only the target and the pointer state are stored. The entries are rebuilt from live state
/// whenever they are drawn or used, so a label such as Zoom/Unzoom never goes stale and a pane
/// that closes underneath the menu closes the menu too.
#[derive(Debug, Clone, Copy)]
struct PaneMenu {
    tab_id: u64,
    pane_id: PaneId,
    /// The cell the menu was opened from. The button release that ends the opening click lands
    /// here and must not choose an item; a release anywhere else is a press-drag-release choice.
    origin: (u16, u16),
    selected: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneMenuCommand {
    Split(Axis),
    Swap(Direction),
    Kill,
    Respawn,
    ToggleZoom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneMenuEntry {
    Separator,
    Item {
        label: &'static str,
        key: u8,
        command: PaneMenuCommand,
        enabled: bool,
    },
}

impl PaneMenuEntry {
    fn enabled_command(self) -> Option<PaneMenuCommand> {
        match self {
            Self::Item {
                command,
                enabled: true,
                ..
            } => Some(command),
            _ => None,
        }
    }
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
    /// The reported display name, falling back to the provider's agent label.
    label: String,
    /// The reported name for this status, falling back to the built-in one.
    status_label: String,
    /// Block reason and metadata tokens, already joined for display.
    detail: String,
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
                .and_then(|pane| queue_key_input(pane, bytes))
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
        origin: Option<FloatOrigin>,
    },
    Resize {
        tab_id: u64,
        pane: PaneId,
        edges: EdgeMask,
        start: (u16, u16),
        original: Rect,
        origin: Option<FloatOrigin>,
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
    /// The client whose prefix parser is in edit mode; `None` when automation entered it, in
    /// which case every client is told.
    client: Option<u64>,
    pane: PaneId,
    kind: FloatingEditKind,
    original: Rect,
    origin: Option<FloatOrigin>,
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
    /// Decoder reset serials carried by this exact submitted snapshot. Source keys survive a seek,
    /// so activating by key alone can release the new generation on an older apply acknowledgement.
    decoder_reset_serials: HashMap<BridgeSourceKey, u64>,
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
    config_path: Option<PathBuf>,
    sender: mpsc::SyncSender<ActorEvent>,
    /// Where this session's persisted state lives, when it persists any.
    snapshot_paths: Option<crate::runtime::SnapshotPaths>,
    /// Set when the live shape has diverged from the last snapshot written.
    ///
    /// A payload-free dirty bit rather than a queue of changes, for the reason the config watcher
    /// uses one: a hundred splits in a second are one snapshot, and the capture reads current state
    /// anyway.
    snapshot_dirty: bool,
    /// When the debounce expires and a capture is due. `None` when nothing is pending.
    snapshot_due: Option<Instant>,
    /// A write is on a worker thread; the actor holds off starting another.
    snapshot_writing: bool,
    /// Whether this session's shape came from a snapshot, for `msg snapshot` to report.
    restored_from_snapshot: bool,
    agent_detector: DetectorHandle,
    panes: BTreeMap<PaneId, Pane>,
    tabs: Vec<Tab>,
    active_tab: usize,
    /// Every attached client, keyed by connection ID and bounded by `MAX_ATTACHED_CLIENTS`.
    clients: BTreeMap<u64, AttachedClient>,
    /// What every attached client shows: the session UI, or one pane over the whole terminal.
    /// Shared, as a tmux session's current window is, and meaningful only while a client is
    /// attached; an attach asking for a different view is refused rather than splitting geometry.
    view: AttachmentView,
    /// The client receiving exclusive tracks and host services. Images and rasters are shared.
    ///
    /// Never reassigned implicitly. When the presenter detaches, media stays unpresented until a
    /// client claims the role, so a remote text viewer never starts pulling video because
    /// somebody else left.
    presenter: Option<u64>,
    /// The client whose message is being handled. Input routing, clipboard replies, and UI
    /// ownership consult it; it is `None` for automation, PTY output, and timers.
    current_client: Option<u64>,
    /// The client that opened the transient UI — menus, prompts, float editing, pointer drags.
    ///
    /// That UI is drawn only on this client's terminal and consumes only its input. Another
    /// client's keys go straight to the focused pane meanwhile, so one user's open menu never
    /// freezes the other's shell. `None` with UI open means automation opened it for everyone.
    ui_owner: Option<u64>,
    client_activity: u64,
    /// The display panes are laid out for, derived from the attached clients by
    /// `general.window_size`. Kept across a detach so a relayout while nobody is attached does not
    /// publish geometry no producer can honor.
    last_display: DisplayMetrics,
    next_pane_id: PaneId,
    next_tab_id: u64,
    copy_buffer: Vec<u8>,
    search_pattern: Option<(String, SearchPattern)>,
    /// Click targets from the status row being composed, keyed by stable tab identity. Moved to
    /// the client the frame is for once composition finishes.
    status_tab_targets: Vec<(std::ops::Range<usize>, u64)>,
    /// Where the tab list is drawn. Session-wide, so every client sees the same view.
    tab_view: TabView,
    /// Sidebar expansion the user chose per tab ID. A tab without an entry is expanded exactly
    /// while it is the active tab.
    sidebar_expanded: HashMap<u64, bool>,
    /// The first sidebar line drawn when the tree is taller than the host.
    sidebar_scroll: usize,
    /// Click targets from the sidebar being composed, keyed by host row. Moved to the client the
    /// frame is for, like `status_tab_targets`.
    sidebar_targets: Vec<(u16, SidebarLine)>,
    /// Every client needs a full repaint; folded into each client's own flag at the next render.
    force_full: bool,
    /// Some client has a change it has not been sent.
    pending_render: bool,
    /// Validated Kitty transfers live only for the current presenter's attachment.
    kitty_transfers: KittyTransferBuffer,
    layout_revision: u64,
    last_media_projection: Option<MediaProjectionKey>,
    shared_visual_sources: HashSet<BridgeSourceKey>,
    media_projection_revision: u64,
    /// Source sets submitted to the foreground bridge but not yet acknowledged as physically
    /// applied. Timed producer workers remain parked until the matching revision is consumed.
    pending_media_projections: BTreeMap<u64, PendingMediaProjection>,
    outer_virtual_revision: u64,
    bridge_instance_id: Option<u64>,
    microphone_recipient: Option<(BridgeSourceKey, u64, PaneId, Instant)>,
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
    pane_menu: Option<PaneMenu>,
    tab_rename: Option<TabRename>,
    close_pane_confirmation: Option<ClosePaneConfirmation>,
    save_layout_prompt: Option<SaveLayoutPrompt>,
    status_notice: Option<StatusNotice>,
    agent_catalog: Arc<crate::agent::AgentCatalog>,
    agent_catalog_generation: u64,
    plugin_registration_generation: u64,
    plugin_keybindings: Vec<crate::ipc::PluginKeybinding>,
    plugin_link_handlers: Vec<CompiledLinkRegistration>,
    plugin_link_press: Option<PluginLinkPress>,
    /// Per-pane notification floor. Bounded by the pane set, and pruned with it.
    last_notified: HashMap<PaneId, Instant>,
    next_float_mode: u64,
    session_sequence: u64,
    /// Direct count of general-queue actor wakeups for fairness/compatibility diagnostics.
    actor_wakeups: u64,
    response_sender: mpsc::SyncSender<AutomationResponseJob>,
    automation_inflight: HashMap<u64, HashSet<u64>>,
    /// Replies already produced for an idempotency key, so a retry returns the first answer.
    /// Advisory pane leases, so several agents can share one session without fighting.
    leases: crate::lease::Leases,
    /// A recording in progress, started explicitly and never by default.
    recorder: Option<crate::record::Recorder>,
    idempotency_keys: HashMap<String, serde_json::Value>,
    /// Insertion order, so the bounded map evicts the oldest key rather than an arbitrary one.
    idempotency_order: VecDeque<String>,
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
    alt_reads: HashMap<PaneId, PendingAgentRead>,
    /// Input scheduled to reach a pane later, earliest first.
    delayed_inputs: BinaryHeap<Reverse<DelayedInput>>,
    next_delayed_input: u64,
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
    /// Counters for the presenter's VVMX connection, retained across a detach for the same
    /// reason. Replaced when another client takes the media role.
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
    /// Session state a layout plan cannot describe, when this session is being restored.
    ///
    /// Separate from `layout` because the two come from different places: a plan may equally have
    /// come from `--layout` or `startup.toml`, and only a snapshot carries extras.
    pub restore: Option<SnapshotExtras>,
    /// What was on each restored pane's screen, when that is persisted and enabled.
    pub history: Option<SessionHistory>,
    /// Where to persist this session's shape, or `None` to persist nothing.
    pub snapshot_paths: Option<crate::runtime::SnapshotPaths>,
}

pub fn start(options: SessionOptions) -> io::Result<ActorHandle> {
    let SessionOptions {
        name,
        config,
        config_path,
        vivid_endpoint,
        layout,
        restore,
        history,
        snapshot_paths,
    } = options;
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE);
    let (media_sender, media_receiver) = mpsc::sync_channel(MEDIA_EVENT_QUEUE);
    let shutdown = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    let vivid =
        VirtualVivid::start_with_events(vivid_endpoint, config.media.clone(), Some(media_sender))?;
    let media_projection_pending = Arc::new(AtomicBool::new(false));
    {
        // Never block ingest on the actor's general queue. The atomic dirty bit makes a lost
        // coalescible wake harmless even for immutable images, which advance retained projection
        // state without placing a payload on the dedicated media queue.
        let wakeup = sender.clone();
        let pending = Arc::clone(&media_projection_pending);
        vivid.set_media_wakeup(Arc::new(move || {
            request_media_service(&wakeup, &pending);
        }));
    };
    let config_reload_pending = Arc::new(AtomicBool::new(false));
    if let Some(path) = config_path.clone() {
        // Watch the resolved path even when nothing is there yet: creating the file later is an
        // ordinary way to start configuring a running session.
        crate::config_watch::spawn(
            path,
            sender.clone(),
            Arc::clone(&shutdown),
            Arc::clone(&config_reload_pending),
        )?;
    }
    let plugin_reload_pending = Arc::new(AtomicBool::new(false));
    let (response_sender, response_receiver) =
        mpsc::sync_channel::<AutomationResponseJob>(AUTOMATION_RESPONSE_QUEUE);
    let response_receiver = Arc::new(std::sync::Mutex::new(response_receiver));
    for index in 0..2 {
        let receiver = Arc::clone(&response_receiver);
        std::thread::Builder::new()
            .name(format!("vvmux-automation-response-{index}"))
            .spawn(move || {
                loop {
                    let job = {
                        receiver
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    let tab_view = config.general.tab_view;
    let last_display = normalized_display(
        DisplayMetrics {
            columns: 80,
            rows: 24,
            cell_width: 0,
            cell_height: 0,
        },
        tab_view,
    );
    let detector_sender = sender.clone();
    let agent_detector = crate::agent::start_detector(move |updates| {
        let _ = detector_sender.send(ActorEvent::AgentProcesses(updates));
    })?;
    let session_instance = crate::plugin::random_id()?;
    let (plugin_supervisor, plugin_watch_shutdown, agent_catalog_generation, agent_catalog) =
        if config.plugins.enabled {
            let (supervisor, agent_catalog_generation, agent_catalog) =
                crate::plugin_supervisor::PluginSupervisor::start(
                    name.clone(),
                    session_instance.clone(),
                    sender.clone(),
                )?;
            let watcher_shutdown = Arc::new(AtomicBool::new(false));
            if let Err(error) = crate::config_watch::spawn_plugin_registry(
                crate::plugin::registry_path()?,
                sender.clone(),
                Arc::clone(&watcher_shutdown),
                Arc::clone(&plugin_reload_pending),
            ) {
                supervisor.shutdown();
                return Err(error);
            }
            // The scan behind `agent_catalog` already ran before this session signaled ready, so
            // the detector sees it from its first tick — a command that reports an agent must not
            // race the async `AgentCatalogApplied` event to learn the kind it names is enabled.
            agent_detector.replace_catalog(Arc::clone(&agent_catalog));
            (
                Some(supervisor),
                Some(watcher_shutdown),
                agent_catalog_generation,
                agent_catalog,
            )
        } else {
            (
                None,
                None,
                0,
                Arc::new(crate::agent::AgentCatalog::default()),
            )
        };
    let mut actor = SessionActor {
        name,
        session_instance,
        config,
        config_path,
        sender: sender.clone(),
        snapshot_paths,
        snapshot_dirty: false,
        snapshot_due: None,
        snapshot_writing: false,
        restored_from_snapshot: restore.is_some(),
        agent_detector,
        panes: BTreeMap::new(),
        tabs: Vec::new(),
        active_tab: 0,
        clients: BTreeMap::new(),
        view: AttachmentView::Session,
        presenter: None,
        current_client: None,
        ui_owner: None,
        client_activity: 0,
        last_display,
        next_pane_id: 1,
        next_tab_id: 1,
        copy_buffer: Vec::new(),
        search_pattern: None,
        status_tab_targets: Vec::new(),
        tab_view,
        sidebar_expanded: HashMap::new(),
        sidebar_scroll: 0,
        sidebar_targets: Vec::new(),
        force_full: true,
        pending_render: false,
        kitty_transfers: KittyTransferBuffer::default(),
        layout_revision: 0,
        last_media_projection: None,
        shared_visual_sources: HashSet::new(),
        media_projection_revision: 0,
        pending_media_projections: BTreeMap::new(),
        outer_virtual_revision: 0,
        bridge_instance_id: None,
        microphone_recipient: None,
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
        pane_menu: None,
        tab_rename: None,
        close_pane_confirmation: None,
        save_layout_prompt: None,
        status_notice: None,
        agent_catalog,
        agent_catalog_generation,
        plugin_registration_generation: 0,
        plugin_keybindings: Vec::new(),
        plugin_link_handlers: Vec::new(),
        plugin_link_press: None,
        last_notified: HashMap::new(),
        next_float_mode: 0,
        session_sequence: 1,
        actor_wakeups: 0,
        response_sender,
        automation_inflight: HashMap::new(),
        leases: crate::lease::Leases::default(),
        recorder: None,
        idempotency_keys: HashMap::new(),
        idempotency_order: VecDeque::new(),
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
        alt_reads: HashMap::new(),
        delayed_inputs: BinaryHeap::new(),
        next_delayed_input: 0,
        exit_tombstones: VecDeque::new(),
        shutdown: Arc::clone(&shutdown),
        vivid,
        media_projection_pending,
        config_reload_pending,
        plugin_reload_pending,
        plugin_watch_shutdown,
        bridge_metrics: crate::metrics::BridgeMetrics::default(),
        client_ipc: None,
    };
    let restored_session = restore.is_some();
    match layout {
        Some(layout) => actor.apply_layout_plan(layout, restore.as_ref(), history.as_ref())?,
        None => actor.new_tab()?,
    }
    // Published whether or not plugins are enabled. `msg subscribe` is an automation surface that
    // reads the same journal and explicitly does not require the plugin system, and this is the
    // event a restart-aware agent replays from sequence 0 to learn it is looking at a restored
    // session rather than a fresh one.
    actor.publish_plugin_event(
        "session.started",
        serde_json::json!({"restored": restored_session}),
        None,
        None,
    );
    let actor_terminated = Arc::clone(&terminated);
    std::thread::Builder::new()
        .name("vvmux-session".into())
        .spawn(move || {
            let _termination = ActorTermination(actor_terminated);
            actor.run(receiver, media_receiver);
        })?;
    Ok(ActorHandle {
        sender,
        shutdown,
        terminated,
    })
}

impl SessionActor {
    fn run(
        &mut self,
        receiver: mpsc::Receiver<ActorEvent>,
        media_receiver: mpsc::Receiver<vivid_sdk::presenter::MediaEvent>,
    ) {
        let mut render_at = Instant::now();
        let mut deferred_event = None;
        loop {
            // Re-read every iteration: a config reload must be able to retune the render cadence
            // without restarting the session.
            let interval = Duration::from_millis(self.config.general.render_interval_ms);
            let mut timeout = if self.pending_render {
                render_at.saturating_duration_since(Instant::now())
            } else {
                IDLE_WAKE_INTERVAL
            };
            timeout = timeout.min(self.next_automation_deadline());
            timeout = timeout.min(self.next_agent_evaluation_delay());
            timeout = timeout.min(self.next_notice_deadline());
            timeout = timeout.min(self.next_sync_flush_delay());
            timeout = timeout.min(self.next_delayed_input_deadline());
            timeout = timeout.min(self.next_alt_read_deadline());
            timeout = timeout.min(self.next_snapshot_deadline());
            // Give ready media low-latency service, but force a general-queue turn after a bounded
            // batch. A bounded channel is not a bounded drain when its producer can refill it.
            if self.drain_media(&media_receiver) {
                timeout = Duration::ZERO;
            }
            let received = deferred_event
                .take()
                .map_or_else(|| receiver.recv_timeout(timeout), Ok);
            match received {
                Ok(event) => {
                    let (event, deferred, consumed) =
                        coalesce_ready_pty_output(event, &receiver, PTY_OUTPUT_BATCH_BYTES);
                    deferred_event = deferred;
                    self.actor_wakeups = self.actor_wakeups.saturating_add(consumed as u64);
                    if self.handle_event(event).is_err() {
                        self.force_full = true;
                    }
                    self.flush_expired_sync_updates();
                    self.drain_media(&media_receiver);
                    self.sync_pending_media_projection();
                    self.expire_status_notice();
                    self.flush_delayed_inputs();
                    self.flush_snapshot();
                    if self.pending_render && render_at <= Instant::now() {
                        self.render();
                        render_at = Instant::now() + interval;
                    } else if self.pending_render && render_at < Instant::now() + interval {
                        // Keep the already scheduled coalescing boundary.
                    } else if self.pending_render {
                        render_at = Instant::now() + interval;
                    }
                    // Classification runs before the waiter sweep so a wait decides against this
                    // tick's screen. The ordering matters at exactly one moment: the wake that
                    // ends detection's startup grace, where the stored status is still the graced
                    // `idle` and the grace no longer guards it. `evaluate_agent_states` re-checks
                    // waiters itself for anything that changes state, so this only adds the sweep
                    // for the cases where classification changed nothing.
                    self.evaluate_agent_states();
                    self.poll_alt_reads();
                    self.check_automation_waiters();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.flush_expired_sync_updates();
                    self.drain_media(&media_receiver);
                    self.sync_pending_media_projection();
                    self.expire_status_notice();
                    self.flush_delayed_inputs();
                    self.flush_snapshot();
                    if self.pending_render {
                        self.render();
                        render_at = Instant::now() + interval;
                    }
                    self.sync_media(false);
                    // Classification runs before the waiter sweep so a wait decides against this
                    // tick's screen. The ordering matters at exactly one moment: the wake that
                    // ends detection's startup grace, where the stored status is still the graced
                    // `idle` and the grace no longer guards it. `evaluate_agent_states` re-checks
                    // waiters itself for anything that changes state, so this only adds the sweep
                    // for the cases where classification changed nothing.
                    self.evaluate_agent_states();
                    self.poll_alt_reads();
                    self.check_automation_waiters();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.tabs.is_empty() || self.shutdown.load(Ordering::Acquire) {
                break;
            }
        }
        // Written inline rather than through the worker: there is no loop left to deliver the
        // completion to, and the debounce must not be why a clean shutdown loses the last few
        // seconds of shape. A session that ended because its last tab closed is the one case where
        // there is deliberately nothing to save, and the capture reports that itself.
        self.write_snapshot_now();
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
    fn drain_media(
        &mut self,
        media_receiver: &mpsc::Receiver<vivid_sdk::presenter::MediaEvent>,
    ) -> bool {
        drain_ready_batch(media_receiver, MEDIA_EVENTS_PER_TURN, |event| {
            self.forward_media(event);
        })
    }

    fn sync_pending_media_projection(&mut self) {
        if self.media_projection_pending.swap(false, Ordering::AcqRel) {
            self.sync_media(false);
            for request in self.vivid.take_overlay_host_requests() {
                let id = request.id;
                let sent = self
                    .presenter_client()
                    .filter(|client| client.vivid)
                    .is_some_and(|client| {
                        crate::ipc::send(
                            &client.writer,
                            &ServerMessage::OverlayHostRequest(request),
                        )
                        .is_ok()
                    });
                if !sent {
                    self.vivid.complete_overlay_host_request(
                        id,
                        Err("no outer overlay host is attached".into()),
                    );
                }
            }
        }
    }

    fn forward_media(&mut self, event: vivid_sdk::presenter::MediaEvent) {
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
        if self
            .shared_visual_sources
            .contains(&bridge_key(event.source))
        {
            self.vivid.release_bridge_delivery(event.delivery_id);
            return;
        }
        let sent = self
            .presenter_client()
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
        if !sent
            && matches!(
                event.record_type,
                vivid_protocol::messages::RASTER_FRAME | vivid_protocol::messages::IMAGE_DATA
            )
        {
            // The retained canvas owns these bytes now. Viewer completions report presentation
            // separately; a slow subscriber must not hold the producer's ingress allowance.
            self.vivid.release_bridge_delivery(event.delivery_id);
        } else if !sent {
            self.record_delivery_result(event.delivery_id, false);
            self.vivid
                .complete_bridge_delivery(event.delivery_id, false);
        }
    }

    fn handle_event(&mut self, event: ActorEvent) -> io::Result<()> {
        // A failed client message returns early; never let its sender linger as the current one.
        self.current_client = None;
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
                self.alt_reads
                    .retain(|_, pending| pending.reply.client_id != id);
                self.plugin_event_subscriptions
                    .retain(|_, subscription| subscription.client_id != id);
                self.detach_client(id, None);
            }
            ActorEvent::PtyOutput(pane_id, bytes) => {
                // Recorded before the terminal consumes it: the point of the transcript is the
                // bytes the grid is about to overwrite.
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.transcript.push(&bytes);
                }
                if self.recorder.is_some() {
                    use base64::Engine as _;
                    let base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    self.record(crate::record::RecordedEvent::Output { pane_id, base64 });
                }
                self.drive_pane_terminal(pane_id, |terminal| terminal.feed(&bytes));
                self.check_automation_waiters();
            }
            ActorEvent::PtyExit(pane_id, status) => {
                self.record(crate::record::RecordedEvent::PaneExited {
                    pane_id,
                    code: status.and_then(|status| status.code),
                    signal: status.and_then(|status| status.signal),
                });
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
                        pane.agent.observe_process(None, None, Instant::now());
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
            ActorEvent::SnapshotWritten { result } => {
                self.snapshot_writing = false;
                if let Err(error) = result {
                    // Re-arm rather than give up: the next change will try again, and a session
                    // that cannot persist is still a session that works.
                    log::warn!(
                        event = "session.snapshot.write.failure",
                        error:% = error;
                        "could not write the session snapshot"
                    );
                    self.snapshot_dirty = true;
                    self.snapshot_due = Some(Instant::now() + SNAPSHOT_DEBOUNCE);
                }
            }
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
                    self.status(&format!("plugin registry reload failed: {}", error.message));
                }
            },
            ActorEvent::AgentCatalogApplied {
                generation,
                catalog,
            } => {
                if self.config.plugins.enabled && generation >= self.agent_catalog_generation {
                    self.agent_catalog_generation = generation;
                    self.agent_catalog = Arc::clone(&catalog);
                    self.agent_detector.replace_catalog(catalog);
                    for pane in self.panes.values_mut() {
                        if pane.agent.reconcile_catalog(&self.agent_catalog) {
                            pane.terminal.clear_agent_osc();
                        }
                    }
                    self.evaluate_agent_states();
                }
            }
            ActorEvent::PluginRegistrationsApplied {
                generation,
                keybindings,
                link_handlers,
            } => {
                if self.plugin_supervisor.is_some()
                    && generation >= self.plugin_registration_generation
                {
                    self.plugin_registration_generation = generation;
                    self.plugin_keybindings = keybindings;
                    self.plugin_link_handlers = link_handlers
                        .into_iter()
                        .filter_map(|handler| {
                            regex::Regex::new(&handler.pattern).ok().map(|regex| {
                                CompiledLinkRegistration {
                                    regex,
                                    action: handler.action,
                                }
                            })
                        })
                        .collect();
                    self.send_plugin_keymap();
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
                        && pane.agent.observe_process(
                            update.process_group,
                            update.identity,
                            Instant::now(),
                        )
                    {
                        pane.terminal.clear_agent_osc();
                    }
                }
                self.evaluate_agent_states();
            }
            ActorEvent::AgentProbeComplete {
                reply,
                pane_id,
                probe,
                job,
            } => {
                self.complete_pending_actor_work(&reply);
                self.resume_agent_probe(reply, pane_id, probe, job);
            }
        }
        // Attachment, pane focus, tab switching, pane teardown, and a program enabling the mode
        // all move focus, so reconcile once here rather than at each of those call sites.
        self.sync_pane_focus();
        self.sync_client_input_mode();
        self.current_client = None;
        if !self.owned_ui_active() {
            self.ui_owner = None;
        }
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
                        TerminalEvent::GridScroll { .. } | TerminalEvent::Clear { .. }
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
                        alternate,
                    } => {
                        // The authenticated marker is consumed here. Media ownership is
                        // connected by the virtual-presenter module, never forwarded into
                        // the outer terminal byte stream. The position was captured when
                        // the marker was consumed: the live cursor has already moved on
                        // when ConPTY batches repositioning output behind the marker.
                        self.vivid
                            .observe_marker(pane_id, &marker, row as i32, column, alternate);
                    }
                    TerminalEvent::KittyGraphics(command) => kitty_commands.push(command),
                    TerminalEvent::GridScroll {
                        lines, alternate, ..
                    } => {
                        self.vivid.scroll_anchors(pane_id, lines, alternate);
                    }
                    TerminalEvent::Clear { alternate } => {
                        self.vivid.clear_anchors(pane_id, alternate);
                    }
                    TerminalEvent::ScreenSwap { alternate } => {
                        self.vivid.set_alternate_screen(pane_id, alternate);
                    }
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
                    at: pane.last_screen_change,
                });
                while pane.screen_changes.len() > SCREEN_CHANGE_HISTORY {
                    pane.screen_changes.pop_front();
                }
                changed_screen_sequence = Some(pane.screen_sequence);
                self.session_sequence = self.session_sequence.wrapping_add(1);
            }
            if let Some(title) = title {
                self.send_to_clients(&ServerMessage::Title(format!("{title} — vvmux")));
            }
            if bell {
                self.send_to_clients(&ServerMessage::Bell);
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
    /// vvte buffers everything between BSU and ESU but never enforces the deadline it arms, so a
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
            self.drive_pane_terminal(pane_id, vvmux_terminal::Terminal::flush_synchronized_update);
        }
    }

    fn handle_kitty_graphics(&mut self, pane_id: PaneId, command: KittyGraphicsCommand) {
        // Graphics are media: only the presenter's terminal is offered them.
        let capable = self
            .presenter_client()
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

    fn status(&self, message: &str) {
        self.send_to_clients(&ServerMessage::Status(message.into()));
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

/// Where a pane's child process is now, as opposed to where it started.
///
/// Read from the process rather than tracked, because a shell's `cd` is invisible to vvmux: no
/// escape sequence is required to change directory, so the only authority is the kernel. Absent
/// where that is not readable, which a caller must treat as "unknown" rather than "unchanged".
#[cfg(target_os = "linux")]
fn pane_cwd(child_pid: u32) -> Option<String> {
    (child_pid != 0)
        .then(|| std::fs::read_link(format!("/proc/{child_pid}/cwd")).ok())
        .flatten()
        .map(|path| path.display().to_string())
}

#[cfg(not(target_os = "linux"))]
fn pane_cwd(_child_pid: u32) -> Option<String> {
    None
}

/// A pane-local mouse position, resolved into the session's own coordinate space.
///
/// `pixels` is present only when the caller gave pixels, because that is the one form the SGR
/// pixel encoder can pass through exactly; a cell has no sub-cell position to preserve.
struct ResolvedMousePoint {
    x: u16,
    y: u16,
    pixels: Option<(u16, u16)>,
}

/// One mouse request's parameters, gathered so the handlers take one argument instead of nine.
struct MouseRequest {
    action: crate::ipc::MouseAction,
    position: Option<crate::ipc::MousePosition>,
    button: crate::ipc::MouseButton,
    route: crate::ipc::MouseRoute,
    shift: bool,
    alt: bool,
    ctrl: bool,
    scroll: i16,
    points: Vec<crate::ipc::MousePosition>,
}

/// Points one `mouse path` gesture may carry.
///
/// Matches Vivido's bound, so a caller that learned the gesture there does not find a different
/// ceiling here. Two is the minimum a press-and-release needs.
const MAX_MOUSE_PATH_POINTS: usize = 1_000;

/// Wheel notches one `mouse scroll` may send.
const MAX_MOUSE_SCROLL_NOTCHES: u16 = 1_000;

/// Raw output one pane retains for `transcript` and `wait output`.
///
/// In memory, per pane, and never written to disk. Large enough to hold a burst a caller might
/// have missed between two polls, small enough that a session full of chatty panes cannot grow
/// without bound.
const MAX_TRANSCRIPT_BYTES: usize = 256 * 1024;

/// Idempotency keys one session remembers, and how large one may be.
///
/// Bounded because the map is filled by callers: a chatty or hostile one must not be able to grow
/// it without limit. Oldest-first eviction means a key stays claimed for a long run of subsequent
/// requests, which is the window a retry actually happens in.
const MAX_IDEMPOTENCY_KEYS: usize = 256;
/// Longest automation idempotency key, in bytes.
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// Steps one `resolve_pane` route may take.
///
/// A route crosses one split per step, so a session cannot need more than its pane count; this is
/// well past that and keeps a hostile caller from asking for an unbounded walk.
const MAX_RESOLVE_PANE_STEPS: usize = 32;

/// When this pane last changed anywhere above its bottom `ignore_bottom` rows.
///
/// A TUI that paints a live clock, spinner, or token counter into its status bar never lets the
/// whole screen go quiet, so plain stability never fires while such a program runs — which is
/// exactly when a caller is waiting for it to finish. Discounting the rows the caller names makes
/// the wait answer the question being asked instead of timing out on a ticking second counter.
///
/// Falls back to the oldest retained change when the entire history is status-bar noise: the pane
/// demonstrably has not changed meaningfully in at least that long, which is the conservative
/// answer rather than claiming it never has.
fn last_meaningful_change(
    changes: &VecDeque<ScreenChange>,
    last_change: Instant,
    rows: usize,
    ignore_bottom: u16,
) -> Instant {
    if ignore_bottom == 0 {
        return last_change;
    }
    let oldest = || changes.front().map_or(last_change, |change| change.at);
    let cutoff = rows.saturating_sub(usize::from(ignore_bottom));
    if cutoff == 0 {
        // Every row was discounted. Nothing can be evidence of activity, so the pane counts as
        // quiet since its oldest retained change.
        return oldest();
    }
    changes
        .iter()
        .rev()
        .find(|change| {
            change
                .rows
                .as_ref()
                // A whole-screen repaint is always meaningful: nothing says it was confined to
                // the discounted band.
                .is_none_or(|rows| rows.iter().any(|row| *row < cutoff))
        })
        .map_or_else(oldest, |change| change.at)
}

/// Cell size assumed for Vivid metrics before any client has reported one.
const PLACEHOLDER_CELL_SIZE: (u16, u16) = (10, 20);

/// The cell size a pane advertises to its producer. A session started detached has never seen a
/// display, and the presenter publishes no target for a zero cell, so a producer started there
/// would be refused until someone attached. It is given the placeholder instead, and the first
/// attach replaces it through the ordinary metrics path.
fn vivid_cell_size(cell_width: u16, cell_height: u16) -> (u16, u16) {
    if cell_width == 0 || cell_height == 0 {
        PLACEHOLDER_CELL_SIZE
    } else {
        (cell_width, cell_height)
    }
}

/// A pane's advertised cell size, raised while a scaled capture is in flight.
///
/// Saturating rather than checked because the caller has already validated the scale against a
/// pixel budget; this only has to refuse to wrap.
fn scaled_cells(cell_width: u16, cell_height: u16, scale: Option<u32>) -> (u16, u16) {
    let Some(scale) = scale.filter(|scale| *scale > 1) else {
        return (cell_width, cell_height);
    };
    let apply = |cell: u16| {
        u32::from(cell)
            .saturating_mul(scale)
            .try_into()
            .unwrap_or(u16::MAX)
    };
    (apply(cell_width), apply(cell_height))
}

/// Whether `method` targets one pane, which dispatch resolves before running it.
///
/// Exhaustive on purpose: a new automation method does not compile until it is classified here,
/// and dispatch relies on this answer through [`required_pane`].
fn method_needs_pane(method: &AutomationMethod) -> bool {
    use crate::ipc::LeaseOperation;
    match method {
        AutomationMethod::Capabilities
        | AutomationMethod::ListPanes
        | AutomationMethod::SessionInspect
        | AutomationMethod::ListClients
        | AutomationMethod::DetachClient { .. }
        | AutomationMethod::ListTabs
        | AutomationMethod::SelectTab { .. }
        | AutomationMethod::Diagnose { .. }
        | AutomationMethod::WaitRendered { .. }
        | AutomationMethod::ReloadConfig
        | AutomationMethod::GetConfig
        | AutomationMethod::Layout
        | AutomationMethod::ResolvePane { .. }
        | AutomationMethod::NewTab { .. }
        | AutomationMethod::RenameTab { .. }
        | AutomationMethod::ResetTabTitle { .. }
        | AutomationMethod::CloseTab { .. }
        | AutomationMethod::SaveLayout { .. }
        | AutomationMethod::Record(_)
        | AutomationMethod::Lease(
            LeaseOperation::Renew { .. } | LeaseOperation::Release { .. } | LeaseOperation::List,
        )
        | AutomationMethod::SessionSnapshot
        | AutomationMethod::Plugin(_)
        // Session-wide. Its filter may name a pane, but that narrows the stream rather than
        // targeting the request.
        | AutomationMethod::Subscribe { .. } => false,
        AutomationMethod::Lease(LeaseOperation::Acquire { .. })
        | AutomationMethod::Action(_)
        | AutomationMethod::ActivatePane
        | AutomationMethod::AgentExplain
        | AutomationMethod::AgentPrompt { .. }
        | AutomationMethod::AgentRead { .. }
        | AutomationMethod::AgentRename { .. }
        | AutomationMethod::AgentSendKeys { .. }
        | AutomationMethod::AgentStart { .. }
        | AutomationMethod::Capture { .. }
        | AutomationMethod::CaptureMedia { .. }
        | AutomationMethod::ClearAgentReport { .. }
        | AutomationMethod::ClosePane
        | AutomationMethod::Focus
        | AutomationMethod::FocusWait { .. }
        | AutomationMethod::GetGrid { .. }
        | AutomationMethod::GetText { .. }
        | AutomationMethod::Inspect
        | AutomationMethod::InspectMedia
        | AutomationMethod::Key { .. }
        | AutomationMethod::Mouse { .. }
        | AutomationMethod::MovePane { .. }
        | AutomationMethod::PaneRename { .. }
        | AutomationMethod::Paste { .. }
        | AutomationMethod::ReportAgent { .. }
        | AutomationMethod::ReportAgentSession { .. }
        | AutomationMethod::ReportMetadata { .. }
        | AutomationMethod::ResizePane { .. }
        | AutomationMethod::Run { .. }
        | AutomationMethod::Search { .. }
        | AutomationMethod::SetFlag { .. }
        | AutomationMethod::SetSyncInput { .. }
        | AutomationMethod::ShellCommand { .. }
        | AutomationMethod::Signal { .. }
        | AutomationMethod::Split { .. }
        | AutomationMethod::SubmitLine { .. }
        | AutomationMethod::TraceMedia { .. }
        | AutomationMethod::Transcript { .. }
        | AutomationMethod::Typing { .. }
        | AutomationMethod::WaitAgentState { .. }
        | AutomationMethod::WaitExit { .. }
        | AutomationMethod::WaitMedia { .. }
        | AutomationMethod::WaitMediaTrack { .. }
        | AutomationMethod::WaitOutput { .. }
        | AutomationMethod::WaitScreenChange { .. }
        | AutomationMethod::WaitScreenStable { .. }
        | AutomationMethod::WaitText { .. } => true,
    }
}

/// The pane that dispatch resolved for a method [`method_needs_pane`] says targets one.
///
/// # Panics
///
/// Panics if the dispatch arm and [`method_needs_pane`] disagree about the method, which is a
/// programming error in this module.
#[track_caller]
fn required_pane(pane_id: Option<PaneId>) -> PaneId {
    pane_id.expect("dispatch reached a pane method that method_needs_pane() did not resolve")
}

fn event_sequence(envelope: &PluginEventEnvelope) -> Option<u64> {
    match envelope {
        PluginEventEnvelope::Event { sequence, .. } => Some(*sequence),
        PluginEventEnvelope::Gap { .. } => None,
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn validate_automation_method(method: &AutomationMethod) -> Result<(), AutomationError> {
    let input = match method {
        AutomationMethod::Typing { text, .. }
        | AutomationMethod::Paste { text, .. }
        | AutomationMethod::SubmitLine { text, .. } => Some(text.len()),
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
        | AutomationMethod::ReportAgentSession { source, .. }
        | AutomationMethod::ClearAgentReport { source, .. }
            if source.is_empty() || source.len() > crate::agent::MAX_REPORT_SOURCE_BYTES =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "agent report source must contain 1..=128 bytes",
            ))
        }
        AutomationMethod::ReportAgent {
            message: Some(message),
            ..
        } if message.is_empty() || message.len() > crate::agent::MAX_REPORT_MESSAGE_BYTES => {
            Err(AutomationError::new(
                "invalid_params",
                "agent report message must contain 1..=256 bytes",
            ))
        }
        AutomationMethod::ReportAgent {
            session_id,
            session_path,
            ..
        }
        | AutomationMethod::ReportAgentSession {
            session_id,
            session_path,
            ..
        } if [session_id, session_path]
            .into_iter()
            .flatten()
            .any(|value| {
                value.is_empty() || value.len() > crate::agent::MAX_AGENT_SESSION_BYTES
            }) =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "agent session identity must contain 1..=256 bytes",
            ))
        }
        AutomationMethod::ReportAgentSession {
            session_id: None,
            session_path: None,
            ..
        } => Err(AutomationError::new(
            "invalid_params",
            "agent session identity is required",
        )),
        // Validated here as well as in the CLI parser, so a direct VVMX caller gets the same
        // answer rather than a silently different behavior.
        AutomationMethod::AgentStart { args, .. }
            if args.len() > crate::agent_drive::MAX_AGENT_START_ARGS =>
        {
            Err(AutomationError::new(
                "invalid_params",
                format!(
                    "agent start takes at most {} arguments",
                    crate::agent_drive::MAX_AGENT_START_ARGS
                ),
            ))
        }
        AutomationMethod::AgentStart { args, .. }
            if args.iter().any(|argument| {
                argument.len() > crate::agent_drive::MAX_AGENT_START_ARG_BYTES
                    || argument.chars().any(char::is_control)
            }) =>
        {
            Err(AutomationError::new(
                "invalid_agent_argument",
                format!(
                    "each agent argument must contain at most {} printable bytes",
                    crate::agent_drive::MAX_AGENT_START_ARG_BYTES
                ),
            ))
        }
        AutomationMethod::AgentStart { timeout_ms, .. }
            if !(millis(crate::agent_drive::AGENT_START_MIN_TIMEOUT)
                ..=millis(crate::agent_drive::AGENT_START_MAX_TIMEOUT))
                .contains(timeout_ms) =>
        {
            Err(AutomationError::new(
                "invalid_agent_timeout",
                "agent start timeout must be from 3s through 300s",
            ))
        }
        AutomationMethod::AgentPrompt {
            text,
            wait,
            until,
            timeout_ms,
        } => {
            if text.is_empty() {
                return Err(AutomationError::new(
                    "empty_agent_prompt",
                    "agent prompt text must be non-empty",
                ));
            }
            if !(millis(crate::agent_drive::AGENT_START_MIN_TIMEOUT)
                ..=millis(crate::agent_drive::AGENT_START_MAX_TIMEOUT))
                .contains(timeout_ms)
            {
                return Err(AutomationError::new(
                    "invalid_agent_timeout",
                    "agent prompt timeout must be from 3s through 300s",
                ));
            }
            if until.len() > 4 {
                Err(AutomationError::new(
                    "invalid_params",
                    "until must name from 1 through 4 agent statuses",
                ))
            } else if !wait && !until.is_empty() {
                Err(AutomationError::new(
                    "invalid_params",
                    "agent-prompt --until requires --wait",
                ))
            } else if !wait && *timeout_ms != 30_000 {
                Err(AutomationError::new(
                    "invalid_params",
                    "agent-prompt --timeout requires --wait",
                ))
            } else {
                Ok(())
            }
        }
        AutomationMethod::AgentSendKeys { keys } if keys.is_empty() => Err(AutomationError::new(
            "invalid_key",
            "agent-send-keys requires at least one key",
        )),
        AutomationMethod::AgentSendKeys { keys } if keys.len() > 32 => Err(AutomationError::new(
            "invalid_key",
            "agent-send-keys takes at most 32 keys",
        )),
        AutomationMethod::AgentRead { lines, .. }
            if !(1..=crate::alt_read::MAX_READ_LINES as u16).contains(lines) =>
        {
            Err(AutomationError::new(
                "invalid_params",
                "agent-read lines must be from 1 through 1000",
            ))
        }
        AutomationMethod::WaitAgentState { until, .. } if until.is_empty() || until.len() > 4 => {
            Err(AutomationError::new(
                "invalid_params",
                "until must name from 1 through 4 agent statuses",
            ))
        }
        AutomationMethod::SubmitLine { text, .. } if text.contains(['\n', '\r']) => {
            Err(AutomationError::new(
                "invalid_params",
                "submit takes one line; use paste for multi-line input",
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
        AutomationMethod::GetText {
            rows: Some(rows), ..
        } if !(1..=1000).contains(rows) => Err(AutomationError::new(
            "invalid_params",
            "rows must be from 1 through 1000",
        )),
        AutomationMethod::GetText {
            rows: Some(_),
            source: source @ (TextSource::Visible | TextSource::Detection),
        } => Err(AutomationError::new(
            "invalid_params",
            format!(
                "rows does not apply to the {} source",
                match source {
                    TextSource::Detection => "detection",
                    _ => "visible",
                }
            ),
        )),
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
                | AutomationMethod::FocusWait { timeout_ms, .. }
                | AutomationMethod::SelectTab {
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
        // Both projections of one table. `methods` stays the flat list older callers read;
        // `method_capabilities` says what each one does, which is what a plan preflight and a
        // scoped remote token need in order to skip or refuse a mutation.
        "methods": crate::ipc::METHOD_CAPABILITIES
            .iter()
            .map(|capability| capability.name)
            .collect::<Vec<_>>(),
        "method_capabilities": crate::ipc::METHOD_CAPABILITIES,
        // Every code an automation reply can carry, so a caller can branch on the set it knows and
        // treat the rest as unrecognized rather than guessing from message text.
        "error_codes": AUTOMATION_ERROR_CODES,
        // The names `subscribe --name` and a plugin manifest hook may both use.
        "event_kinds": vvmux_plugin_api::EVENT_KINDS,
        "limits": automation_limits(),
        "completion_waits": {
            "outer": "foreground_bridge_projection_acknowledgement",
            "rendered": "attached_client_terminal_frame_acknowledgement",
        },
        "plugins": plugin,
    })
}

/// Every `code` an [`AutomationError`] reply can carry.
///
/// Advertised so a caller can tell "a failure I know how to handle" from "a failure this release
/// added", which message text cannot express. Sorted, and covered by a test that scans the crate
/// for constructed codes so a new one cannot go unadvertised.
pub(crate) const AUTOMATION_ERROR_CODES: &[&str] = &[
    "action_not_found",
    "agent_alias_not_found",
    "agent_alias_taken",
    "agent_kind_mismatch",
    "agent_launch_pending",
    "agent_not_detected",
    "agent_not_idle",
    "agent_not_launchable",
    "agent_not_ready",
    "agent_not_running",
    "agent_pane_busy",
    "agent_prompt_failed",
    "agent_prompt_stalled",
    "agent_start_failed",
    "agent_start_input_failed",
    "alt_read_in_progress",
    "busy",
    "cancelled",
    "capability_denied",
    "capture_failed",
    "capture_in_progress",
    "capture_scale_rejected",
    "capture_would_disturb_client",
    "capture_write_failed",
    "cell_metrics_unknown",
    "client_not_found",
    "dependency_failed",
    "duplicate_request_id",
    "empty_agent_prompt",
    "event_gap",
    "invalid_agent_argument",
    "invalid_agent_kind",
    "invalid_agent_report",
    "invalid_agent_timeout",
    "invalid_argument",
    "invalid_config",
    "invalid_key",
    "invalid_params",
    "invalid_state",
    "job_not_found",
    "lease_denied",
    "lease_not_found",
    "limit_exceeded",
    "missing_attachment",
    "mouse_reporting_disabled",
    "no_focused_pane",
    "not_alternate_screen",
    "output_invalid",
    "pane_name_taken",
    "pane_not_found",
    "pane_required",
    "plugin_disabled",
    "plugin_not_found",
    "protocol_error",
    "pty_closed",
    "pty_spawn_failed",
    "regex_invalid",
    "runtime_crashed",
    "runtime_unavailable",
    "save_failed",
    "schema_invalid",
    "scope_denied",
    "sequence_gap",
    "serialization_failed",
    "spawn_failed",
    "tab_not_found",
    "timeout",
    "track_not_found",
    "unsupported",
];

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
    let mut limits = automation_base_limits();
    // Merged rather than declared inline: `serde_json::json!` recurses once per key, and the base
    // object already sits at the macro's expansion limit.
    if let Some(object) = limits.as_object_mut() {
        for (name, value) in [
            ("transcript_bytes_per_pane", MAX_TRANSCRIPT_BYTES),
            ("mouse_path_points", MAX_MOUSE_PATH_POINTS),
            (
                "mouse_scroll_notches",
                usize::from(MAX_MOUSE_SCROLL_NOTCHES),
            ),
            ("resolve_pane_steps", MAX_RESOLVE_PANE_STEPS),
            ("pane_name_bytes", crate::layout::MAX_PANE_NAME_BYTES),
            ("tab_name_bytes", MAX_TAB_NAME_BYTES),
        ] {
            object.insert(name.into(), value.into());
        }
    }
    limits
}

fn automation_base_limits() -> serde_json::Value {
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
        "agent_report_sources": crate::agent::MAX_REPORT_SOURCES,
        "agent_report_message_bytes": crate::agent::MAX_REPORT_MESSAGE_BYTES,
        "agent_session_bytes": crate::agent::MAX_AGENT_SESSION_BYTES,
        "agent_alias_bytes": crate::agent::MAX_AGENT_ALIAS_BYTES,
        // Persistence bounds. A caller cannot raise these, but it can tell from them why a long
        // scrollback came back shorter than it went in, which is the difference between a bound and
        // a mystery.
        "session_snapshot_bytes": crate::session_state::MAX_SNAPSHOT_BYTES,
        "session_history_bytes": crate::session_state::MAX_HISTORY_BYTES,
        "pane_history_rows": HISTORY_MAX_ROWS,
        "pane_history_bytes": HISTORY_MAX_PANE_BYTES,
        "session_history_capture_bytes": HISTORY_MAX_SESSION_BYTES,
        "agent_resume_args": vvmux_plugin_api::MAX_AGENT_RESUME_ARGS,
        "agent_metadata_tokens": crate::agent::MAX_METADATA_TOKENS,
        "agent_metadata_key_bytes": crate::agent::MAX_METADATA_KEY_BYTES,
        "agent_metadata_value_bytes": crate::agent::MAX_METADATA_VALUE_BYTES,
        "agent_metadata_state_labels": crate::agent::MAX_METADATA_STATE_LABELS,
        "agent_metadata_ttl_ms": { "minimum": 1, "maximum": crate::agent::MAX_METADATA_TTL_MS },
        "media_trace_events": crate::media_trace::MAX_MEDIA_TRACE_EVENTS,
        "media_trace_bytes": crate::media_trace::MAX_MEDIA_TRACE_BYTES,
        "media_trace_query_events": crate::media_trace::MAX_MEDIA_TRACE_QUERY_EVENTS,
        "agent_start_args": crate::agent_drive::MAX_AGENT_START_ARGS,
        "agent_start_arg_bytes": crate::agent_drive::MAX_AGENT_START_ARG_BYTES,
        "agent_start_timeout_ms": {
            "minimum": millis(crate::agent_drive::AGENT_START_MIN_TIMEOUT),
            "maximum": millis(crate::agent_drive::AGENT_START_MAX_TIMEOUT),
        },
        "agent_prompt_timeout_ms": {
            "minimum": millis(crate::agent_drive::AGENT_START_MIN_TIMEOUT),
            "maximum": millis(crate::agent_drive::AGENT_START_MAX_TIMEOUT),
        },
        "agent_prompt_until": 4,
        "agent_send_keys": 32,
        "agent_read_lines": { "minimum": 1, "maximum": crate::alt_read::MAX_READ_LINES },
        "agent_read_concurrency": crate::alt_read::MAX_ALT_SCREEN_READS,
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
        "message": agent.message,
        "session_present": agent.session_present,
    })
}

/// Which notification, if any, a status warrants under the configured policy.
fn notification_kind(
    status: AgentStatus,
    settings: &crate::config::Notifications,
) -> Option<crate::ipc::NotifyKind> {
    if !settings.enabled {
        return None;
    }
    match status {
        AgentStatus::Blocked if settings.on.contains(&crate::config::NotifyOn::Blocked) => {
            Some(crate::ipc::NotifyKind::AgentBlocked)
        }
        AgentStatus::Done if settings.on.contains(&crate::config::NotifyOn::Done) => {
            Some(crate::ipc::NotifyKind::AgentDone)
        }
        // `working` and `idle` are never notifications: they are the states an agent passes
        // through while you are not being interrupted.
        _ => None,
    }
}

/// Whether a pane's notification floor has elapsed.
fn notification_allowed(last: Option<Instant>, now: Instant, floor: Duration) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= floor)
}

/// Clamp notification text on a char boundary.
///
/// A block reason is bounded at 256 bytes on its own, but a title composed with an agent label
/// still needs a ceiling before it reaches an escape sequence.
fn bounded_notification_text(text: &str) -> String {
    /// Longest notification text, in bytes, after the agent label is added; keeps the escape
    /// sequence well inside terminal limits.
    const MAX: usize = 160;
    if text.len() <= MAX {
        return text.to_owned();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn agent_metadata_json(metadata: &crate::agent::AgentMetadata) -> serde_json::Value {
    let tokens = metadata
        .tokens()
        .map(|(key, value)| (key.to_owned(), serde_json::Value::String(value.to_owned())))
        .collect::<serde_json::Map<_, _>>();
    let state_labels = [
        AgentStatus::Idle,
        AgentStatus::Working,
        AgentStatus::Blocked,
        AgentStatus::Done,
    ]
    .into_iter()
    .filter_map(|status| {
        metadata.state_label(status).map(|label| {
            (
                status.label().to_owned(),
                serde_json::Value::String(label.to_owned()),
            )
        })
    })
    .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "tokens": tokens,
        "display_agent": metadata.display_agent(),
        "title": metadata.title(),
        "state_labels": state_labels,
    })
}

/// How much of a pane's reported agent identity a serialization may disclose.
///
/// A native session reference names a resumable conversation on the user's agent account, so it
/// is returned only where a caller asked about exactly one pane. Bulk listings, plugin-visible
/// session inspection, and diagnostics — including the debug bundle built from them — see
/// presence alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentDisclosure {
    Presence,
    Full,
}

fn history_color(color: TerminalColor) -> Option<HistoryColor> {
    match color {
        TerminalColor::Default => None,
        TerminalColor::Indexed(index) => Some(HistoryColor::Indexed(index)),
        TerminalColor::Rgb(red, green, blue) => Some(HistoryColor::Rgb(red, green, blue)),
    }
}

fn restored_color(color: Option<HistoryColor>) -> TerminalColor {
    match color {
        None => TerminalColor::Default,
        Some(HistoryColor::Indexed(index)) => TerminalColor::Indexed(index),
        Some(HistoryColor::Rgb(red, green, blue)) => TerminalColor::Rgb(red, green, blue),
    }
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
    source: vivid_sdk::presenter::SourceKey,
    live_delivery_source: Option<vivid_sdk::presenter::SourceKey>,
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
    std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from(r"C:\"), PathBuf::from)
}

fn saved_percent(extent: u16, available: u16) -> u16 {
    if available == 0 {
        return 60;
    }
    ((u32::from(extent) * 100) / u32::from(available)).clamp(10, 100) as u16
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

fn bridge_key(key: vivid_sdk::presenter::SourceKey) -> BridgeSourceKey {
    key
}

/// A pointer position in the pane-local logical pixels an overlay window lays out against.
///
/// The outer terminal reports pixels when it can, which is what an overlay needs: a cell-resolution
/// fallback lands in the middle of the cell, so a click still reaches whatever occupies it.
fn overlay_pointer_position(
    mouse: MouseEvent,
    pixels: Option<(u16, u16)>,
    content: Rect,
    display: DisplayMetrics,
) -> (f64, f64) {
    let cell_width = u32::from(display.cell_width.max(1));
    let cell_height = u32::from(display.cell_height.max(1));
    match pixels {
        Some((x, y)) => (
            f64::from(u32::from(x).saturating_sub(u32::from(content.x).saturating_mul(cell_width))),
            f64::from(
                u32::from(y).saturating_sub(u32::from(content.y).saturating_mul(cell_height)),
            ),
        ),
        None => (
            f64::from(
                u32::from(mouse.x.saturating_sub(content.x))
                    .saturating_mul(cell_width)
                    .saturating_add(cell_width / 2),
            ),
            f64::from(
                u32::from(mouse.y.saturating_sub(content.y))
                    .saturating_mul(cell_height)
                    .saturating_add(cell_height / 2),
            ),
        ),
    }
}

/// Place a pane-local overlay window in the outer terminal's pixel space.
///
/// A pane is its own coordinate space: its producer laid the window out against the pane's pixel
/// rectangle, while the outer presenter places windows against the whole terminal. Both use the
/// same cell size, so this is a translation by the pane's origin — a zoomed pane is already a
/// larger projected rectangle rather than a scale factor.
///
/// `SET_OVERLAY_WINDOW` carries no clip, so a window that would overhang its pane is pulled back
/// inside it, and one too large to fit at all is withheld: painting over a neighbouring pane is a
/// worse failure than not painting.
fn project_overlay_window(
    window: vivid_sdk::presenter::SnapshotOverlayWindow,
    content: Rect,
    display: DisplayMetrics,
) -> BridgeOverlayWindow {
    let cell_width = i64::from(display.cell_width.max(1));
    let cell_height = i64::from(display.cell_height.max(1));
    let slack_x = i64::from(content.width)
        .saturating_mul(cell_width)
        .saturating_sub(window.width);
    let slack_y = i64::from(content.height)
        .saturating_mul(cell_height)
        .saturating_sub(window.height);
    BridgeOverlayWindow {
        parent: window.parent,
        min_width: window.min_width,
        min_height: window.min_height,
        offset_x: i64::from(content.x)
            .saturating_mul(cell_width)
            .saturating_add(window.x.clamp(0, slack_x.max(0)))
            .saturating_sub(window.x),
        offset_y: i64::from(content.y)
            .saturating_mul(cell_height)
            .saturating_add(window.y.clamp(0, slack_y.max(0)))
            .saturating_sub(window.y),
        generation: window.generation,
        revision: window.revision,
        x: i64::from(content.x)
            .saturating_mul(cell_width)
            .saturating_add(window.x.clamp(0, slack_x.max(0))),
        y: i64::from(content.y)
            .saturating_mul(cell_height)
            .saturating_add(window.y.clamp(0, slack_y.max(0))),
        width: window.width,
        height: window.height,
        mode: window.mode,
        visible: window.visible && slack_x >= 0 && slack_y >= 0,
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
    node: &vivid_sdk::presenter::SceneNode,
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

fn retained_raster_body(raster: &vivid_sdk::presenter::RetainedRaster) -> io::Result<Vec<u8>> {
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

/// Merge immediately ready output records only while they remain adjacent in actor-queue order.
///
/// The first different event is returned to the run loop rather than re-enqueued, so client input,
/// detach, PTY exit, and other panes retain their exact position relative to the output bytes.
fn coalesce_ready_pty_output(
    event: ActorEvent,
    receiver: &mpsc::Receiver<ActorEvent>,
    maximum_bytes: usize,
) -> (ActorEvent, Option<ActorEvent>, usize) {
    let ActorEvent::PtyOutput(pane_id, mut bytes) = event else {
        return (event, None, 1);
    };
    let mut consumed = 1;
    let mut deferred = None;
    while bytes.len() < maximum_bytes {
        let Ok(next) = receiver.try_recv() else {
            break;
        };
        match next {
            ActorEvent::PtyOutput(next_pane, next_bytes)
                if next_pane == pane_id
                    && next_bytes.len() <= maximum_bytes.saturating_sub(bytes.len()) =>
            {
                bytes.extend_from_slice(&next_bytes);
                consumed += 1;
            }
            other => {
                deferred = Some(other);
                break;
            }
        }
    }
    (ActorEvent::PtyOutput(pane_id, bytes), deferred, consumed)
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

fn normalized_display(display: DisplayMetrics, view: TabView) -> DisplayMetrics {
    DisplayMetrics {
        // A sidebar takes at most a third of the columns, which still leaves the minimum 6-wide
        // float representable.
        columns: display.columns.clamp(10, 1000),
        // A tab bar row is outside the pane area, so retain an extra host row to leave the
        // minimum 6x4 float (4x2 content plus frame) representable.
        rows: display.rows.clamp(4 + view.bar_rows(), 500),
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
                ..
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
            TerminalEvent::Clear { .. } => return None,
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
        && !mouse.shift
        && (copy_mode || !modes.mouse_clicks)
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

/// Find a word edge without crossing a hard line break or leaving the retained pane grid.
fn mouse_selection_word_edge(
    terminal: &Terminal,
    mut point: (isize, usize),
    backwards: bool,
) -> (isize, usize) {
    // Match Vivido's default terminal word delimiters, keeping paths and dotted names whole.
    let is_delimiter = |cell: &Cell| {
        !cell.wide_continuation
            && !cell.leading_wide_spacer
            && (cell.ch.is_whitespace() || ",│`|:\"'()[]{}<>".contains(cell.ch))
    };
    let Some(cell) = terminal
        .viewport_line(point.0)
        .and_then(|line| line.get(point.1))
    else {
        return point;
    };
    if is_delimiter(cell) {
        return point;
    }
    loop {
        let next = if backwards {
            if let Some(column) = point.1.checked_sub(1) {
                (point.0, column)
            } else {
                let Some(row) = point.0.checked_sub(1) else {
                    break;
                };
                if terminal.line_wrapped(row) != Some(true) {
                    break;
                }
                let Some(column) = terminal
                    .viewport_line(row)
                    .and_then(|line| line.len().checked_sub(1))
                else {
                    break;
                };
                (row, column)
            }
        } else if terminal
            .viewport_line(point.0)
            .is_some_and(|line| point.1 + 1 < line.len())
        {
            (point.0, point.1 + 1)
        } else {
            if terminal.line_wrapped(point.0) != Some(true) {
                break;
            }
            let Some(row) = point.0.checked_add(1) else {
                break;
            };
            (row, 0)
        };
        let Some(cell) = terminal
            .viewport_line(next.0)
            .and_then(|line| line.get(next.1))
        else {
            break;
        };
        if is_delimiter(cell) {
            break;
        }
        point = next;
    }
    point
}

fn mouse_selection_bounds(
    terminal: &Terminal,
    selection: MouseSelection,
) -> ((isize, usize), (isize, usize)) {
    if selection.mode == MouseSelectionMode::Word {
        return (
            mouse_selection_word_edge(terminal, selection.start, true),
            mouse_selection_word_edge(terminal, selection.start, false),
        );
    }
    let (start, end) = if selection.start <= selection.end {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    };
    (start, end)
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
    let (start, end) = mouse_selection_bounds(terminal, selection);
    let offset = isize::try_from(display_offset).unwrap_or(isize::MAX);
    let mut runs = Vec::new();
    for line in start.0..=end.0 {
        let row = line.saturating_add(offset);
        if row < 0 || row >= viewport_height as isize {
            continue;
        }
        let (first, mut last) = match selection.mode {
            MouseSelectionMode::Line => (0, viewport_width - 1),
            MouseSelectionMode::Character | MouseSelectionMode::Word => (
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
        if selection.mode != MouseSelectionMode::Line
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

#[cfg(test)]
fn tab_status_text(tabs: &[Tab], active: usize, columns: u16) -> String {
    tab_status_layout(tabs, active, columns).0
}

fn tab_status_layout(
    tabs: &[Tab],
    active: usize,
    columns: u16,
) -> (String, Vec<(std::ops::Range<usize>, u64)>) {
    let width = usize::from(columns);
    if width == 0 || tabs.is_empty() {
        return (String::new(), Vec::new());
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
        return (all, tab_status_targets(tabs, &segments, 0, segments.len()));
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
        (visible, tab_status_targets(tabs, &segments, start, end))
    } else {
        let text =
            render_narrow_active_tab(&segments[active], start > 0, end < segments.len(), width);
        let prefix = if start > 0 { 3 } else { 1 };
        let suffix = if end < segments.len() { 2 } else { 1 };
        let end = text.chars().count().saturating_sub(suffix);
        let targets = if prefix < end {
            vec![(prefix..end, tabs[active].id)]
        } else {
            Vec::new()
        };
        (text, targets)
    }
}

// Status text is drawn one character per cell by ScreenBuffer::draw_text.
fn tab_status_targets(
    tabs: &[Tab],
    segments: &[String],
    start: usize,
    end: usize,
) -> Vec<(std::ops::Range<usize>, u64)> {
    let mut column = if start > 0 { 3 } else { 1 };
    (start..end)
        .map(|index| {
            let next = column + segments[index].chars().count();
            let target = (column..next, tabs[index].id);
            column = next + 1;
            target
        })
        .collect()
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

fn pane_menu_title(pane_id: PaneId) -> String {
    format!(" Pane {pane_id} ")
}

/// Where the pane menu sits: its top-left corner at the click, as tmux places a mouse menu,
/// pulled back inside `area` when it would run off the right or bottom edge.
fn pane_menu_rect(
    area: Rect,
    origin: (u16, u16),
    entries: &[PaneMenuEntry],
    title: &str,
) -> Option<Rect> {
    let label_width = entries
        .iter()
        .map(|entry| match entry {
            PaneMenuEntry::Item { label, .. } => label.chars().count(),
            PaneMenuEntry::Separator => 0,
        })
        .max()?;
    // Border, a space, the label, two spaces, the key, a space, border.
    let width = (label_width + 7).max(title.chars().count() + 4);
    let width = u16::try_from(width).ok()?;
    let height = u16::try_from(entries.len().checked_add(2)?).ok()?;
    if width > area.width || height > area.height {
        return None;
    }
    let right = area.x + area.width - width;
    let bottom = area.y + area.height - height;
    Some(Rect {
        x: origin.0.clamp(area.x, right),
        y: origin.1.clamp(area.y, bottom),
        width,
        height,
    })
}

/// The entry row under a cell of the menu, excluding its frame.
fn pane_menu_hit(rect: Rect, x: u16, y: u16) -> Option<usize> {
    (x > rect.x && x + 1 < rect.x + rect.width && y > rect.y && y + 1 < rect.y + rect.height)
        .then(|| usize::from(y - rect.y - 1))
}

fn first_enabled(entries: &[PaneMenuEntry], order: Vec<usize>) -> Option<usize> {
    order.into_iter().find(|index| {
        entries
            .get(*index)
            .is_some_and(|entry| entry.enabled_command().is_some())
    })
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
        if let Some(length) = non_press_key_report(&bytes[offset..]) {
            offset += length;
        } else {
            kept.push(bytes[offset]);
            offset += 1;
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

/// This pane's agent-mesh position, `f<tab>p<pane>`, when both IDs can be address indices.
///
/// `f` is vvmux's tab — the addressing scheme calls it a frame so it cannot be confused with a
/// Vivido or Vivida tab — and `p` is the pane. An address index is a one-based `u32`, so a session
/// that somehow ran past `u32::MAX` tabs or panes publishes no position rather than one that cannot
/// parse: an unparsable address fails `vvagent bind` outright, while none of it costs only the
/// position.
fn mesh_address(tab_id: u64, pane_id: PaneId) -> Option<String> {
    let addressable = |id: u64| (1..=u64::from(u32::MAX)).contains(&id);
    (addressable(tab_id) && addressable(pane_id)).then(|| format!("f{tab_id}p{pane_id}"))
}

fn extract_selection(terminal: &Terminal, start: (isize, usize), end: (isize, usize)) -> Vec<u8> {
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let mut output = String::new();
    for line_index in start.0..=end.0 {
        let Some(line_len) = terminal
            .viewport_line(line_index)
            .map(<[vvmux_terminal::Cell]>::len)
        else {
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
        MouseSelectionMode::Word => {
            let (start, end) = mouse_selection_bounds(terminal, selection);
            extract_selection(terminal, start, end)
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
                let Some(line_len) = terminal
                    .viewport_line(line)
                    .map(<[vvmux_terminal::Cell]>::len)
                else {
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
mod tests;

#[cfg(test)]
mod pane_menu_tests {
    use super::*;

    fn item(label: &'static str, enabled: bool) -> PaneMenuEntry {
        PaneMenuEntry::Item {
            label,
            key: b'x',
            command: PaneMenuCommand::Kill,
            enabled,
        }
    }

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        }
    }

    #[test]
    fn the_menu_opens_at_the_click_and_is_pulled_inside_the_area() {
        let entries = [
            item("Horizontal Split", true),
            PaneMenuEntry::Separator,
            item("Kill", true),
        ];
        let title = pane_menu_title(7);
        let rect = pane_menu_rect(area(), (10, 5), &entries, &title).unwrap();
        assert_eq!((rect.x, rect.y), (10, 5));
        // Two border columns, a space, the widest label, two spaces, the key, a space.
        assert_eq!(rect.width, 16 + 7);
        assert_eq!(rect.height, 5);
        let corner = pane_menu_rect(area(), (79, 23), &entries, &title).unwrap();
        assert_eq!(
            (corner.x + corner.width, corner.y + corner.height),
            (80, 24)
        );
        let tiny = Rect {
            width: 10,
            ..area()
        };
        assert_eq!(pane_menu_rect(tiny, (0, 0), &entries, &title), None);
        assert_eq!(pane_menu_rect(area(), (0, 0), &[], &title), None);
    }

    #[test]
    fn only_entry_rows_inside_the_frame_are_hit() {
        let rect = Rect {
            x: 10,
            y: 5,
            width: 12,
            height: 5,
        };
        assert_eq!(pane_menu_hit(rect, 11, 6), Some(0));
        assert_eq!(pane_menu_hit(rect, 20, 8), Some(2));
        // The opening click lands on the top-left border, which is not an entry.
        assert_eq!(pane_menu_hit(rect, 10, 5), None);
        assert_eq!(pane_menu_hit(rect, 21, 6), None);
        assert_eq!(pane_menu_hit(rect, 11, 9), None);
    }

    #[test]
    fn selection_skips_separators_and_disabled_items() {
        let entries = [
            item("a", false),
            PaneMenuEntry::Separator,
            item("b", true),
            item("c", false),
            item("d", true),
        ];
        assert_eq!(first_enabled(&entries, (0..5).collect()), Some(2));
        assert_eq!(first_enabled(&entries, (0..5).rev().collect()), Some(4));
        assert_eq!(first_enabled(&entries, vec![0, 1, 3]), None);
        assert_eq!(entries[3].enabled_command(), None);
        assert_eq!(entries[4].enabled_command(), Some(PaneMenuCommand::Kill));
    }
}

#[cfg(test)]
mod capability_advertisement_tests {
    use super::{AUTOMATION_ERROR_CODES, automation_capabilities, disabled_plugin_capabilities};

    /// Every `code` this crate actually constructs, read from its own source.
    ///
    /// A source scan rather than a curated fixture: the point is to catch a code added in some
    /// distant handler, and only the source knows about that one.
    fn constructed_error_codes() -> std::collections::BTreeSet<String> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_files(&root, &mut files);
        assert!(!files.is_empty(), "no source files were scanned");
        let mut codes = std::collections::BTreeSet::new();
        for file in files {
            let source = std::fs::read_to_string(&file).expect("crate source is readable");
            // Whitespace between the call and its first argument spans lines in rustfmt output,
            // so match on the flattened text rather than line by line.
            let flattened = source.split_whitespace().collect::<Vec<_>>().join(" ");
            for (marker, offset) in [
                ("AutomationError::new(", 21),
                ("AutomationError { code:", 23),
            ] {
                let mut rest = flattened.as_str();
                while let Some(index) = rest.find(marker) {
                    rest = &rest[index + offset..];
                    let trimmed = rest.trim_start();
                    if let Some(quoted) = trimmed.strip_prefix('"')
                        && let Some(end) = quoted.find('"')
                        && !quoted[..end].is_empty()
                        && quoted[..end]
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                    {
                        codes.insert(quoted[..end].to_owned());
                    }
                }
            }
        }
        codes
    }

    fn collect_rust_files(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("source directory is readable") {
            let path = entry.expect("directory entry is readable").path();
            if path.is_dir() {
                collect_rust_files(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    #[test]
    fn every_constructed_error_code_is_advertised() {
        let missing = constructed_error_codes()
            .into_iter()
            .filter(|code| !AUTOMATION_ERROR_CODES.contains(&code.as_str()))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "these error codes are returned but not advertised: {missing:?}"
        );
    }

    #[test]
    fn advertised_error_codes_are_sorted_and_unique() {
        let mut sorted = AUTOMATION_ERROR_CODES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), AUTOMATION_ERROR_CODES);
    }

    /// The handshake a caller reads before deciding what it may run.
    #[test]
    fn capabilities_describe_methods_events_and_errors() {
        let value = automation_capabilities(disabled_plugin_capabilities("test-instance"));
        assert_eq!(value["protocol"], "VVMX");
        assert_eq!(value["protocol_version"], crate::ipc::VERSION);

        let methods = value["methods"].as_array().expect("methods is a list");
        let capabilities = value["method_capabilities"]
            .as_array()
            .expect("method_capabilities is a list");
        assert_eq!(methods.len(), capabilities.len());
        assert_eq!(methods.len(), crate::ipc::METHOD_CAPABILITIES.len());

        // The two entries the hand-written list got wrong.
        assert!(methods.iter().any(|name| name == "session_snapshot"));
        assert!(methods.iter().any(|name| name == "plugin"));
        assert!(!methods.iter().any(|name| name == "snapshot"));

        // An observation is advertised as safe to run; an input is not.
        let class_of = |wanted: &str| {
            capabilities
                .iter()
                .find(|entry| entry["name"] == wanted)
                .map(|entry| (entry["class"].clone(), entry["mutating"].clone()))
        };
        assert_eq!(
            class_of("get_text"),
            Some((serde_json::json!("observe"), serde_json::json!(false)))
        );
        assert_eq!(
            class_of("typing"),
            Some((serde_json::json!("input"), serde_json::json!(true)))
        );
        assert_eq!(
            class_of("get_config"),
            Some((serde_json::json!("observe"), serde_json::json!(false)))
        );

        assert_eq!(
            value["error_codes"],
            serde_json::json!(AUTOMATION_ERROR_CODES)
        );
        assert_eq!(
            value["event_kinds"],
            serde_json::json!(vvmux_plugin_api::EVENT_KINDS)
        );
        // Advertising an event nobody subscribes to would be as misleading as omitting one.
        assert!(
            vvmux_plugin_api::EVENT_KINDS.contains(&"session.started"),
            "the restart-detection event must stay advertised"
        );
    }
}
