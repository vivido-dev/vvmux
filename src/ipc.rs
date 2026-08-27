use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[allow(unused_imports)]
pub use vivid_gateway::{
    BridgeClipRect, BridgeKeyframeRequest, BridgeNode, BridgePlayRequest, BridgeSource,
    BridgeSourceDescriptor, BridgeSourceKey, BridgeSourceKind, BridgeSurface, BridgeSurfaceKey,
    DisplayMetrics, PaneMediaNodeStatus, PaneMediaStatus, PaneMediaSurfaceDescriptor,
    PaneMediaSurfaceStatus, PaneMediaTrackStatus,
};

use crate::metrics::{BlockTimer, IpcCounters};
use crate::platform::{ConnectionCancel, Transport};

pub const MAGIC: &[u8; 4] = b"VVMX";
/// Version 20 is the first number past the last published release (18): the development-only bumps
/// 19 through 29 never shipped, so their features are folded into 20 rather than renumbered. It
/// carries the per-pane transparency action; the agent-runtime drive surface of launching an agent
/// into a shell pane, prompting it, and reading its alternate-screen transcript; agent-name
/// targeting and the session-state surface that snapshots and restores a session's shape across
/// daemon restarts; the typed capability handshake and `get_config`; the topology surface of
/// `layout`/`resolve_pane`, durable pane names, and name-addressable tabs; input, process, and
/// observation parity (mouse, signals, exact pane resize, a bounded output transcript with
/// byte-offset waits, idempotent setters, pane movement); request atomicity, where a request may
/// carry the sequences it expects the session to be at and an idempotency key; and the outer
/// identity an attached client publishes, with multi-agent leases and session recording.
/// Version 18 reports recreated retained outer sources so their bodies can be rehydrated.
/// Version 17 added actor-owned tab-navigation, rename, and close-confirmation actions.
/// Version 16 was the hard cutover for deterministic automation waits and correlated diagnostics.
/// Agent IDs remain strings on this wire, so replacing the closed Rust enum with validated plugin
/// IDs does not change its encoding.
///
/// A mixed pair is rejected by [`VERSION_MISMATCH`] rather than negotiated down: the two encodings
/// differ in client-message framing, so accepting an older peer would misdecode bridge state.
/// A wire change does not raise this constant: the maintainer bumps it manually, so leave it alone
/// and keep the mixed-version rejection intact.
pub const VERSION: u16 = 20;
/// Raised when a peer's preface carries a different [`VERSION`].
///
/// A session server outlives the binary that spawned it, so rebuilding across a version bump
/// leaves the old daemon owning the socket. Callers that know which server they reached append its
/// identity to this text; see `server::describe_peer_version`.
pub const VERSION_MISMATCH: &str =
    "unsupported VVMX protocol version; restart the vvmux client and session server";
pub const CONTROL_MAX_BODY: u32 = 1024 * 1024;
pub const BULK_MAX_BODY: u32 = 64 * 1024 * 1024;
const STRUCTURED_RECORD: u16 = 1;
/// Media body chunk: fixed binary header, then raw payload bytes.
const MEDIA_RECORD: u16 = 2;
/// Terminal frame chunk: fixed binary header, then raw terminal bytes.
const RENDER_RECORD: u16 = 3;

/// Byte payloads bypass JSON because `serde_json` has no byte representation: a `Vec<u8>` becomes a
/// decimal number per byte, which measured at 3.57x for media and 3.29x for terminal frames, paid
/// twice — once formatting on the session actor, once parsing on the client's reader thread. Both
/// are single-threaded, so that cost is latency on the two hops least able to absorb it.
const MEDIA_RECORD_HEADER: usize = 56;
const RENDER_RECORD_HEADER: usize = 24;
const MEDIA_FLAG_LAST: u16 = 0x0001;
const RENDER_FLAG_FULL: u16 = 0x0001;
const RENDER_FLAG_LAST: u16 = 0x0002;
/// Preferred chunk sizes. The negotiated ceiling still bounds them; these keep a single record
/// from monopolizing the writer or the peer's reader for too long.
const MEDIA_CHUNK: usize = 128 * 1024;
const RENDER_CHUNK: usize = 256 * 1024;

/// Header fields of one `MEDIA_RECORD` chunk.
#[derive(Debug, Clone, Copy)]
struct MediaChunk {
    delivery_id: u64,
    source: BridgeSourceKey,
    record_type: u16,
    offset: u32,
    total: u32,
    last: bool,
}
const AUTOMATION_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const AUTOMATION_CHUNK_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct AutomationRequest {
    pub id: u64,
    pub pane_id: Option<u64>,
    /// Target the pane whose agent carries this alias, when no `pane_id` is given.
    ///
    /// A second field rather than a target enum replacing `pane_id`: every existing caller and every
    /// server-side path keeps its exact meaning, and the two are reconciled in the one place that
    /// already resolves a request's pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<crate::agent::AgentAlias>,
    /// Target the pane carrying this name, when no `pane_id` is given.
    ///
    /// A third targeting field for the same reason `agent` is a second one, and outranked by both
    /// of them: a caller that named a pane *and* an agent gets the agent, because an agent moves
    /// and a pane does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_name: Option<crate::layout::PaneName>,
    pub allow_focused: bool,
    /// A lease this request acts under.
    ///
    /// Only needed when somebody holds an exclusive lease on the target: an unleased pane is
    /// always open, which is what keeps leases advisory rather than a mode a session enters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// State this request assumes, rejected rather than applied when it no longer holds.
    ///
    /// The race every `inspect`-then-act pair has: the screen a caller reasoned about can change
    /// between reading it and acting on it, and typing into a dialog that already closed is worse
    /// than being told the state moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<ExpectedState>,
    /// Deduplicate a retried request, so a destructive action is applied at most once.
    ///
    /// A caller that retries after a lost reply cannot otherwise tell "the request never arrived"
    /// from "the reply did not come back", and pressing Enter twice is not the same as pressing it
    /// once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub method: AutomationMethod,
}

/// Who is presenting this session, published by the foreground client.
///
/// **Deliberately credential-free.** The outer Vivid endpoint and root secret stay in the client
/// and never reach the hidden server, which is a standing invariant — a daemon that held them
/// could present media on behalf of a window it does not own. What crosses is only the identity a
/// pane agent needs in order to address the right Vivido window: a window ID it can pass to
/// `vivido msg --window-id`, and the metrics that make a pane's pixel rectangle meaningful.
///
/// This exists because the alternative was worse. A pane used to inherit `VIVIDO_SOCKET` and
/// `VIVIDO_WINDOW_ID` from whatever window started the daemon; the daemon outlives that window, so
/// after a detach and a reattach they addressed the wrong one, and over `vvssh` they addressed a
/// machine the pane could not reach. Those are now scrubbed, and this is what replaces them: the
/// live answer, from whoever is attached now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OuterIdentity {
    /// The presenting Vivido window's ID, as `vivido msg --window-id` takes it.
    pub vivido_window_id: Option<u64>,
    /// The presenting Vivido instance's session name, for `vivido msg --target`.
    pub vivido_session: Option<String>,
    /// Whether the client holds an outer Vivid endpoint, without saying what it is.
    pub has_outer_endpoint: bool,
    /// True when the client reached this session over `vvssh`.
    ///
    /// The Vivido automation socket is not forwarded and never will be — it is an owner-only local
    /// socket on another machine. A pane agent that sees this must not attempt `vivido msg` at
    /// all, rather than trying and getting a confusing failure.
    pub remote: bool,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// Session state a request requires before it will run.
///
/// Every field is optional and checked only when present; an empty expectation matches anything.
/// The sequences are the ones `inspect` and `layout` already report, so a caller pins exactly what
/// it read.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
pub struct ExpectedState {
    /// The target pane's screen sequence.
    pub screen_sequence: Option<u64>,
    pub session_sequence: Option<u64>,
    pub layout_sequence: Option<u64>,
}

impl ExpectedState {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// What a mouse request does.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum MouseAction {
    Move,
    Click,
    DoubleClick,
    Down,
    Up,
    Drag,
    Scroll,
    /// One bounded press, move, release gesture over a list of points.
    Path,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// The SGR button code, which is also the wire encoding an application receives.
    pub fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
        }
    }
}

/// Who handles a mouse event.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum MouseRoute {
    /// Encode through the pane's terminal modes and write to its PTY. Works on a hidden pane.
    Application,
    /// Give it to vvmux, which is what handles selection, float drag, and pane focus.
    ///
    /// Named `mux` rather than Vivido's `ui`: vvmux has no GPU chrome, and what this route reaches
    /// is the multiplexer's own handling of a pointer.
    Mux,
}

/// Where in a pane a mouse action happens.
///
/// Exactly one form, always pane-local. Cells are the primary one because vvmux is a text
/// multiplexer and a pane's rectangle is measured in them; pixels exist for applications under SGR
/// pixel mouse mode, which want the exact position inside a cell.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MousePosition {
    Cell {
        column: u16,
        row: u16,
    },
    /// Physical pixels inside the pane's content area.
    Pixel {
        x: u32,
        y: u32,
    },
    /// A fraction of the pane's content area, in per-mille: 0 is the left/top edge, 1000 the
    /// right/bottom.
    ///
    /// Integers rather than a float, so the wire type stays comparable and a position round-trips
    /// through JSON unchanged. The CLI still takes `0.5` and converts.
    Relative {
        x: u16,
        y: u16,
    },
}

/// What a `record` request does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum RecordOperation {
    Start {
        /// Where the recording is written when it stops.
        path: String,
    },
    Stop,
    Status,
}

/// What a `lease` request does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum LeaseOperation {
    Acquire {
        scope: crate::lease::LeaseScope,
        ttl_ms: u64,
        /// A name shown to whoever is refused, so "who has this pane" has an answer.
        holder: Option<String>,
    },
    Renew {
        lease_id: String,
        ttl_ms: u64,
    },
    Release {
        lease_id: String,
    },
    List,
}

/// Which layer a pane should live on.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum PaneLayerRequest {
    Tiled,
    Floating,
}

/// A pane or tab flag an automation caller can set outright.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum PaneFlag {
    /// Project this pane over its whole tab. Tab-scoped: at most one pane is zoomed.
    Zoom,
    /// Keep this floating pane visible while the ordinary float block is hidden.
    Pinned,
    /// Paint this pane's own background instead of letting the outer terminal through.
    Transparent,
    /// Put this pane in copy mode, optionally at a scrollback offset.
    CopyMode,
    /// Show the tab's ordinary (unpinned) floating panes.
    FloatsVisible,
    /// Send input typed at any pane in this tab to all of them.
    SyncInput,
}

/// A signal a caller may deliver to a pane's foreground process group.
///
/// A closed list rather than a raw number: these are the ones a terminal automation caller has a
/// reason to send, they mean the same thing on every Unix, and a typo cannot become a different
/// signal.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "UPPERCASE")]
#[clap(rename_all = "UPPER")]
pub enum SignalName {
    Int,
    Term,
    Hup,
    Quit,
    Tstp,
    Cont,
    Winch,
    Kill,
    Stop,
}

impl SignalName {
    #[cfg(unix)]
    pub fn number(self) -> i32 {
        match self {
            Self::Int => libc::SIGINT,
            Self::Term => libc::SIGTERM,
            Self::Hup => libc::SIGHUP,
            Self::Quit => libc::SIGQUIT,
            Self::Tstp => libc::SIGTSTP,
            Self::Cont => libc::SIGCONT,
            Self::Winch => libc::SIGWINCH,
            Self::Kill => libc::SIGKILL,
            Self::Stop => libc::SIGSTOP,
        }
    }

    #[cfg(not(unix))]
    pub fn number(self) -> i32 {
        0
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Int => "INT",
            Self::Term => "TERM",
            Self::Hup => "HUP",
            Self::Quit => "QUIT",
            Self::Tstp => "TSTP",
            Self::Cont => "CONT",
            Self::Winch => "WINCH",
            Self::Kill => "KILL",
            Self::Stop => "STOP",
        }
    }
}

/// Which tab a request means.
///
/// Never a display index: a tab's position changes when another tab is closed, so an index that
/// was correct when it was read may name a different tab by the time it is used.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TabSelector {
    Id(u64),
    Name(String),
    /// The tab currently selected in the session.
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AutomationMethod {
    Capabilities,
    ListPanes,
    SessionInspect,
    ListTabs,
    SelectTab {
        tab: TabSelector,
        wait: Option<AutomationCompletion>,
        timeout_ms: u64,
    },
    Diagnose {
        pane_id: Option<u64>,
        all_panes: bool,
        trace_limit: u16,
    },
    ReportAgent {
        agent: crate::agent::AgentId,
        state: crate::agent::AgentState,
        source: String,
        sequence: u64,
        /// Why the agent is blocked, shown beside it in the agent navigator.
        message: Option<String>,
        /// Native session identity, retained for a later resume and never logged or bundled.
        session_id: Option<String>,
        session_path: Option<String>,
    },
    /// Attach native session identity without claiming lifecycle-state authority.
    ReportAgentSession {
        agent: crate::agent::AgentId,
        source: String,
        sequence: u64,
        session_id: Option<String>,
        session_path: Option<String>,
    },
    ClearAgentReport {
        source: String,
        sequence: u64,
    },
    /// Attach display-only metadata to one pane, without claiming lifecycle authority.
    ///
    /// Every field distinguishes "absent, leave alone" from "present but empty, clear".
    ReportMetadata {
        source: String,
        sequence: u64,
        tokens: Vec<(String, Option<String>)>,
        ttl_ms: Option<u64>,
        display_agent: Option<Option<String>>,
        state_labels: Vec<(crate::agent::AgentStatus, Option<String>)>,
        title: Option<Option<String>>,
    },
    /// Replay agent classification for one pane and report which rule decided its state.
    AgentExplain,
    /// Report where this session's state is persisted, and whether it was restored from one.
    SessionSnapshot,
    /// Name the agent in one pane, or clear its name when `alias` is absent.
    AgentRename {
        alias: Option<crate::agent::AgentAlias>,
    },
    /// Launch a recognized agent in a pane that is sitting at a shell prompt.
    AgentStart {
        agent: crate::agent::AgentId,
        args: Vec<String>,
        timeout_ms: u64,
    },
    /// Write text to a detected agent and optionally wait for its status transition.
    AgentPrompt {
        text: String,
        wait: bool,
        until: Vec<crate::agent::AgentStatus>,
        timeout_ms: u64,
    },
    /// Send one or more keys to one agent-aware pane.
    AgentSendKeys {
        keys: Vec<String>,
    },
    /// Read an idle agent's alternate-screen transcript, restoring its viewport afterward.
    AgentRead {
        lines: u16,
        json: bool,
    },
    /// Describe every tab and pane: the split tree, cell rectangles, and directional neighbors.
    ///
    /// The discovery call. `list_panes` and `list_tabs` are flat, so neither can answer "which pane
    /// is to the left of this one" or "where does this pane sit in the tree".
    Layout,
    /// Walk a directional route through the split tree without moving focus.
    ///
    /// Distinct from `Action::Focus`, which answers the same geometric question but commits to the
    /// answer. A caller translating "the pane below" into a target needs to look without touching.
    ResolvePane {
        /// Which tab the route runs in. Defaults to the caller's own tab, else the active one.
        tab: Option<TabSelector>,
        /// One navigation step per entry, applied in order. Empty resolves the start pane itself.
        ///
        /// The route starts at the request's `pane_id` when it names a pane in the selected tab,
        /// and at that tab's focused pane otherwise. A request that carries a `pane_name` instead
        /// resolves that name and takes no steps.
        path: Vec<Direction>,
    },
    /// Name one pane, or clear its name when `name` is absent.
    PaneRename {
        name: Option<crate::layout::PaneName>,
    },
    /// Make one pane visible without moving focus or stealing the attachment.
    ///
    /// Selects the owning tab, and lifts a zoom that is hiding the target. Visibility is what
    /// media projection keys off, so revealing a pane is a real operation and not a synonym for
    /// focusing it.
    ActivatePane,
    /// Open a tab, optionally named, and report the IDs it was given.
    NewTab {
        name: Option<String>,
    },
    /// Give a tab a name, replacing any name it had.
    RenameTab {
        tab: TabSelector,
        name: String,
    },
    /// Drop a tab's name so it falls back to its process-derived title.
    ResetTabTitle {
        tab: TabSelector,
    },
    /// Close a tab and every pane in it.
    CloseTab {
        tab: TabSelector,
    },
    /// Send one mouse action to a pane.
    ///
    /// Coordinates are pane-local, so a caller does not have to know where the pane sits in the
    /// session. `application` encodes through the pane's own live terminal modes and reaches it
    /// whether or not it is visible; `mux` drives vvmux's own handling — copy-mode selection,
    /// float drag and resize, focus — through the same path a real mouse takes.
    Mouse {
        action: MouseAction,
        position: Option<MousePosition>,
        button: MouseButton,
        route: MouseRoute,
        shift: bool,
        alt: bool,
        ctrl: bool,
        /// Wheel notches for `scroll`: negative scrolls up, positive down.
        scroll: i16,
        /// Points for `path`, as a bounded press/move/release gesture.
        points: Vec<MousePosition>,
        /// Pace a `path` over this long, so an application sees motion rather than a teleport.
        duration_ms: Option<u64>,
        /// Resolve only once the attached client has acknowledged a newer render.
        wait_rendered: bool,
        timeout_ms: u64,
    },
    /// Start or stop a bounded session recording.
    ///
    /// Always explicit, never a default: a recording holds pane output, which is whatever scrolled
    /// past — the same data `[session] pane_history` gates behind an opt-in, for the same reason.
    Record(RecordOperation),
    /// Take, renew, release, or list advisory leases on panes.
    Lease(LeaseOperation),
    /// Run one command in a pane's existing interactive shell and wait for its real exit status.
    ///
    /// Needs the shell to emit OSC 133 command markers. Without them the boundary between a
    /// command and its output can only be guessed from prompt text, which breaks on every unusual
    /// prompt — so a pane whose shell reports nothing is refused rather than guessed at.
    ///
    /// The pane's own shell, not a new one: aliases, functions, virtual environments, exported
    /// variables and the current directory are all state a fresh process would not have.
    ShellCommand {
        command: String,
        timeout_ms: u64,
    },
    /// Reveal a pane, optionally wait for it to settle, and read it — in one request.
    ///
    /// The composite an observation actually needs: a hidden pane has to be activated before its
    /// state means anything, and a pane still painting has to settle before a read is worth having.
    /// Three round trips a caller would otherwise have to sequence itself, with the same race
    /// between them that `expect` exists to close.
    /// Compose a pane's retained media into one PNG, with no presenter involved.
    ///
    /// Not a screenshot: terminal text is the presenter's to render, and vvmux has no font
    /// rasterizer. This carries the producer's own surfaces, which for a document reader or a
    /// browser is the whole visible content, at the producer's native resolution rather than
    /// scaled into a cell rectangle. It reads state the gateway already holds, so it works while
    /// the session is detached and while the pane is hidden.
    CaptureMedia {
        /// Where to write the PNG. Pixels are never returned inline.
        path: String,
        /// Ask the producer to re-render at this many device pixels per cell pixel.
        ///
        /// The producer sizes its raster to the pane viewport, so at scale 1 a capture inherits
        /// whatever density the last attached client implied — on a small pane that is far too
        /// coarse to read CJK text. Raising this asks for a genuine re-render rather than an
        /// upscale, and reverts as soon as the capture finishes.
        scale: u32,
        /// Allow a scaled capture while a client is attached, which visibly resizes the pane.
        force: bool,
        timeout_ms: u64,
    },
    Capture {
        /// Skip activation and read the pane where it is.
        no_activate: bool,
        after_screen: Option<u64>,
        /// Wait for the screen to stay unchanged this long before reading.
        stable_ms: Option<u64>,
        /// Also wait for the attached client to acknowledge a render.
        rendered: bool,
        /// Include the structured grid as well as the text.
        grid: bool,
        timeout_ms: u64,
    },
    /// Read a pane's bounded rolling window of raw output.
    ///
    /// The answer to "what did it print" when the grid has already overwritten it. Reports a gap
    /// rather than silently returning a shorter answer when the requested offset has scrolled out
    /// of the retained window.
    Transcript {
        after_offset: Option<u64>,
        /// Return the exact bytes, base64-encoded, instead of lossy text.
        base64: bool,
        max_bytes: Option<u32>,
    },
    /// Wait for a pattern in a pane's output stream rather than on its screen.
    ///
    /// A screen wait can miss text that was overwritten before any snapshot ran; this cannot.
    WaitOutput {
        pattern: String,
        regex: bool,
        after_offset: Option<u64>,
        timeout_ms: u64,
    },
    /// Give one pane an exact size, rather than nudging it one step.
    ///
    /// A tiled pane is sized by reweighting the split that decides its span; a floating pane's
    /// rectangle is set directly. Minimum pane sizes still apply, and the committed geometry is
    /// reported rather than assumed.
    ResizePane {
        columns: Option<u16>,
        rows: Option<u16>,
    },
    /// Move a pane within the session: to another tab, to a neighbour's place, or between layers.
    MovePane {
        to_tab: Option<TabSelector>,
        /// Swap with the pane one step in this direction, keeping both in place otherwise.
        swap: Option<Direction>,
        to_layer: Option<PaneLayerRequest>,
    },
    /// Set a pane or tab flag outright, rather than flipping it.
    ///
    /// A toggle cannot be retried: replaying one puts the flag back. Every interactive toggle keeps
    /// its keybinding and gains a setter here, so an automation caller states the state it wants.
    SetFlag {
        flag: PaneFlag,
        enabled: bool,
        /// Scrollback offset for `copy_mode`, ignored by every other flag.
        offset: Option<usize>,
    },
    /// Deliver one signal to a pane's foreground process group.
    ///
    /// Not the same as typing `Ctrl+C`: a signal reaches the job that owns the terminal even when
    /// it is not reading input, and does not depend on the pane's line discipline.
    Signal {
        signal: SignalName,
    },
    Inspect,
    InspectMedia,
    TraceMedia {
        after_sequence: Option<u64>,
        limit: u16,
        timeout_ms: u64,
        filter: crate::media_trace::MediaTraceFilter,
    },
    Split {
        axis: Axis,
    },
    /// Write the session's current tab and pane layout to a startup layout file.
    SaveLayout {
        path: Option<String>,
    },
    Focus,
    FocusWait {
        wait: AutomationCompletion,
        timeout_ms: u64,
    },
    ClosePane,
    Typing {
        text: String,
        report: bool,
    },
    Key {
        key: String,
        modifiers: Vec<String>,
        repeat: u16,
        report: bool,
    },
    Paste {
        text: String,
        report: bool,
    },
    /// Submit one line to a pane: the text and its Enter in a single PTY write.
    SubmitLine {
        text: String,
        report: bool,
    },
    GetText {
        rows: Option<u16>,
        source: TextSource,
    },
    GetGrid {
        start_line: Option<isize>,
        row_count: Option<u16>,
        since_screen: Option<u64>,
    },
    Search {
        pattern: String,
        regex: bool,
        direction: crate::search::SearchDirection,
        start_line: Option<isize>,
        start_column: Option<usize>,
        limit: u16,
    },
    SetSyncInput {
        enabled: bool,
    },
    /// Apply one ordinary user action to an explicitly resolved pane context.
    Action(Action),
    Plugin(PluginMethod),
    WaitText {
        text: String,
        regex: bool,
        after_screen: Option<u64>,
        timeout_ms: u64,
    },
    WaitScreenChange {
        after_screen: Option<u64>,
        timeout_ms: u64,
    },
    WaitScreenStable {
        quiet_ms: u64,
        after_screen: Option<u64>,
        timeout_ms: u64,
    },
    WaitRendered {
        after_session: u64,
        timeout_ms: u64,
    },
    /// Re-read the session's config file now, instead of waiting for the watcher to notice.
    ReloadConfig,
    /// Return the effective configuration this session is running with.
    ///
    /// The values in force, not the file: a session started before an edit, or one whose reload
    /// deferred a key, differs from what `vvmux.toml` currently says, and that difference is
    /// exactly what a caller is asking about.
    GetConfig,
    /// Open a pane running one shell command.
    Run {
        /// Handed to the shell with `-c`, so pipes and redirection work. Not an argument vector.
        command: String,
        placement: RunPlacement,
        cwd: Option<String>,
        /// Keep the pane open after the command exits, so its output stays readable.
        hold: bool,
        focus: bool,
    },
    WaitExit {
        timeout_ms: u64,
    },
    /// Stream session lifecycle events. Unlike the plugin event subscription this does not
    /// require plugins to be enabled: the events describe the session, not the plugin system.
    Subscribe {
        after_sequence: Option<u64>,
        filter: EventFilter,
    },
    /// Wait until a pane's agent reaches one of the given lifecycle states.
    WaitAgentState {
        until: Vec<crate::agent::AgentStatus>,
        timeout_ms: u64,
    },
    WaitMedia {
        after_virtual_revision: Option<u64>,
        after_outer_revision: Option<u64>,
        timeout_ms: u64,
    },
    WaitMediaTrack {
        identity: MediaTrackIdentity,
        condition: MediaTrackWaitCondition,
        timeout_ms: u64,
    },
}

/// What a method does, so a caller can tell an observation from a mutation before running one.
///
/// A class is coarse on purpose. It answers "may this run during a read-only pass, and what kind of
/// authority does it need", which is what a plan preflight and a scoped remote token each have to
/// decide without understanding every method.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MethodClass {
    /// Reads state or blocks until state arrives. Never writes.
    Observe,
    /// Writes to a pane's PTY.
    Input,
    /// Creates or destroys a pane.
    Pane,
    /// Rearranges, selects, or persists what already exists.
    Layout,
    /// Changes the session's configuration.
    Config,
    /// Acts on a pane's child processes rather than on its terminal.
    Process,
    /// Claims, names, or drives an agent in a pane.
    Agent,
    /// Enters the plugin host, whose own operations carry their own permissions.
    Plugin,
    /// Changes what the session persists about itself.
    Lifecycle,
}

impl MethodClass {
    /// Whether a method of this class can change session state.
    ///
    /// Derived rather than stored so the two can never disagree: everything that is not an
    /// observation mutates something.
    pub const fn mutating(self) -> bool {
        !matches!(self, Self::Observe)
    }
}

/// One advertised method: its wire name and what it does.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct MethodCapability {
    pub name: &'static str,
    pub class: MethodClass,
    /// Always `class.mutating()`. Carried explicitly so a caller filtering the list does not have
    /// to reproduce the class table to answer the only question most of them ask.
    pub mutating: bool,
}

const fn capability(name: &'static str, class: MethodClass) -> MethodCapability {
    MethodCapability {
        name,
        class,
        mutating: class.mutating(),
    }
}

/// Every automation method this release serves, in the order `capabilities` advertises them.
///
/// The single source of the advertised surface. It is checked against the generated schema in
/// tests, so an [`AutomationMethod`] variant added without an entry here fails the build rather
/// than silently becoming an unadvertised method — which is how `session_snapshot` came to be
/// advertised under a name that was never on the wire.
pub const METHOD_CAPABILITIES: &[MethodCapability] = {
    use MethodClass::{Agent, Config, Input, Layout, Lifecycle, Observe, Pane, Plugin, Process};
    &[
        capability("capabilities", Observe),
        capability("get_config", Observe),
        capability("layout", Observe),
        capability("list_panes", Observe),
        capability("list_tabs", Observe),
        capability("resolve_pane", Observe),
        capability("session_inspect", Observe),
        capability("session_snapshot", Observe),
        capability("inspect", Observe),
        capability("inspect_media", Observe),
        capability("capture_media", Observe),
        capability("diagnose", Observe),
        capability("trace_media", Observe),
        capability("get_text", Observe),
        capability("get_grid", Observe),
        capability("search", Observe),
        capability("agent_explain", Observe),
        capability("subscribe", Observe),
        capability("transcript", Observe),
        capability("wait_output", Observe),
        capability("wait_text", Observe),
        capability("wait_screen_change", Observe),
        capability("wait_screen_stable", Observe),
        capability("wait_rendered", Observe),
        capability("wait_exit", Observe),
        capability("wait_agent_state", Observe),
        capability("wait_media", Observe),
        capability("wait_media_track", Observe),
        capability("typing", Input),
        capability("key", Input),
        capability("paste", Input),
        capability("submit_line", Input),
        capability("agent_send_keys", Input),
        capability("mouse", Input),
        capability("shell_command", Input),
        // Layout rather than observe even for `list`: acquiring one excludes other callers, and
        // one class per method is what keeps a scoped token's check a single lookup.
        capability("lease", Layout),
        // Lifecycle rather than observe: starting one writes a file of everything the session
        // prints, which a read-only pass must not do on a caller's behalf.
        capability("record", Lifecycle),
        capability("split", Pane),
        capability("run", Pane),
        capability("close_pane", Pane),
        capability("activate_pane", Layout),
        // Activation is a layout change, so a read-only pass must skip this even though what it
        // returns is an observation. `--no-activate` exists for exactly that case.
        capability("capture", Layout),
        capability("close_tab", Layout),
        capability("new_tab", Layout),
        capability("pane_rename", Layout),
        capability("rename_tab", Layout),
        capability("reset_tab_title", Layout),
        capability("move_pane", Layout),
        capability("resize_pane", Layout),
        capability("select_tab", Layout),
        capability("set_flag", Layout),
        capability("focus", Layout),
        capability("focus_wait", Layout),
        capability("set_sync_input", Layout),
        capability("save_layout", Layout),
        capability("action", Layout),
        capability("signal", Process),
        capability("reload_config", Config),
        capability("report_agent", Agent),
        capability("report_agent_session", Agent),
        capability("clear_agent_report", Agent),
        capability("report_metadata", Agent),
        capability("agent_rename", Agent),
        capability("agent_start", Agent),
        capability("agent_prompt", Agent),
        // Observation in intent, but it scrolls the agent's viewport to reach the scrollback and
        // scrolls it back afterward. A read-only pass must skip it.
        capability("agent_read", Agent),
        capability("plugin", Plugin),
    ]
};

impl AutomationMethod {
    /// This method's wire tag, matching the `snake_case` name serde encodes.
    ///
    /// Exhaustive on purpose: a new variant does not compile until it is named here, and the tests
    /// then require that name to appear in [`METHOD_CAPABILITIES`].
    pub fn name(&self) -> &'static str {
        match self {
            Self::Capabilities => "capabilities",
            Self::ListPanes => "list_panes",
            Self::SessionInspect => "session_inspect",
            Self::ListTabs => "list_tabs",
            Self::SelectTab { .. } => "select_tab",
            Self::Diagnose { .. } => "diagnose",
            Self::ReportAgent { .. } => "report_agent",
            Self::ReportAgentSession { .. } => "report_agent_session",
            Self::ClearAgentReport { .. } => "clear_agent_report",
            Self::ReportMetadata { .. } => "report_metadata",
            Self::AgentExplain => "agent_explain",
            Self::SessionSnapshot => "session_snapshot",
            Self::AgentRename { .. } => "agent_rename",
            Self::AgentStart { .. } => "agent_start",
            Self::AgentPrompt { .. } => "agent_prompt",
            Self::AgentSendKeys { .. } => "agent_send_keys",
            Self::AgentRead { .. } => "agent_read",
            Self::Layout => "layout",
            Self::ResolvePane { .. } => "resolve_pane",
            Self::PaneRename { .. } => "pane_rename",
            Self::ActivatePane => "activate_pane",
            Self::NewTab { .. } => "new_tab",
            Self::RenameTab { .. } => "rename_tab",
            Self::ResetTabTitle { .. } => "reset_tab_title",
            Self::CloseTab { .. } => "close_tab",
            Self::Record(_) => "record",
            Self::Lease(_) => "lease",
            Self::ShellCommand { .. } => "shell_command",
            Self::Capture { .. } => "capture",
            Self::CaptureMedia { .. } => "capture_media",
            Self::Mouse { .. } => "mouse",
            Self::Transcript { .. } => "transcript",
            Self::WaitOutput { .. } => "wait_output",
            Self::ResizePane { .. } => "resize_pane",
            Self::MovePane { .. } => "move_pane",
            Self::SetFlag { .. } => "set_flag",
            Self::Signal { .. } => "signal",
            Self::Inspect => "inspect",
            Self::InspectMedia => "inspect_media",
            Self::TraceMedia { .. } => "trace_media",
            Self::Split { .. } => "split",
            Self::SaveLayout { .. } => "save_layout",
            Self::Focus => "focus",
            Self::FocusWait { .. } => "focus_wait",
            Self::ClosePane => "close_pane",
            Self::Typing { .. } => "typing",
            Self::Key { .. } => "key",
            Self::Paste { .. } => "paste",
            Self::SubmitLine { .. } => "submit_line",
            Self::GetText { .. } => "get_text",
            Self::GetGrid { .. } => "get_grid",
            Self::Search { .. } => "search",
            Self::SetSyncInput { .. } => "set_sync_input",
            Self::Action(_) => "action",
            Self::Plugin(_) => "plugin",
            Self::WaitText { .. } => "wait_text",
            Self::WaitScreenChange { .. } => "wait_screen_change",
            Self::WaitScreenStable { .. } => "wait_screen_stable",
            Self::WaitRendered { .. } => "wait_rendered",
            Self::ReloadConfig => "reload_config",
            Self::GetConfig => "get_config",
            Self::Run { .. } => "run",
            Self::WaitExit { .. } => "wait_exit",
            Self::Subscribe { .. } => "subscribe",
            Self::WaitAgentState { .. } => "wait_agent_state",
            Self::WaitMedia { .. } => "wait_media",
            Self::WaitMediaTrack { .. } => "wait_media_track",
        }
    }
}

/// Why a notification fired.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotifyKind {
    AgentBlocked,
    AgentDone,
}

/// Narrows an event stream. An empty filter accepts everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct EventFilter {
    pub names: Vec<String>,
    pub pane_id: Option<u64>,
}

impl EventFilter {
    pub const MAX_NAMES: usize = 16;
    pub const MAX_NAME_BYTES: usize = 64;

    pub fn accepts(&self, envelope: &PluginEventEnvelope) -> bool {
        match envelope {
            // A gap always passes. It reports events this subscriber will never see, and hiding
            // that because the lost events might not have matched would be a lie about coverage.
            PluginEventEnvelope::Gap { .. } => true,
            PluginEventEnvelope::Event { name, context, .. } => {
                (self.names.is_empty() || self.names.iter().any(|allowed| allowed == name))
                    && self
                        .pane_id
                        .is_none_or(|pane_id| context.pane_id == Some(pane_id))
            }
        }
    }
}

/// Which terminal text a pane read returns.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum TextSource {
    /// The current viewport, honoring copy-mode scroll. Soft wraps are joined.
    Visible,
    /// The last N rows, one line per physical terminal row.
    Recent,
    /// The last N rows with soft wraps joined, so output reads as the lines a command wrote.
    RecentUnwrapped,
    /// The exact bottom-buffer snapshot and OSC fields agent classification runs against.
    Detection,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutomationCompletion {
    Outer,
    Rendered,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct MediaTrackIdentity {
    pub producer_id: u64,
    pub context_id: u64,
    pub surface_id: u64,
    pub track_id: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaTrackWaitCondition {
    Visible,
    Hidden,
    OuterAttached,
    KeyframeNeeded,
    KeyframeRecovered,
    Playing,
    Paused,
    Eos,
    Lost,
    QueueDrained,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum PluginMethod {
    Invoke {
        reference: String,
        input: Value,
        detach: bool,
    },
    JobStatus {
        job_id: String,
    },
    JobCancel {
        job_id: String,
    },
    JobLogs {
        job_id: String,
    },
    PaneOpen {
        reference: String,
    },
    EventSubscribe {
        after_sequence: Option<u64>,
    },
    EventUnsubscribe {
        subscription_id: String,
    },
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginEventEnvelope {
    Event {
        sequence: u64,
        name: String,
        payload: Value,
        context: vvmux_plugin_api::InvocationContext,
    },
    Gap {
        from_sequence: u64,
        to_sequence: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct AutomationError {
    pub code: String,
    pub message: String,
}

impl AutomationError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct AutomationResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AutomationError>,
}

impl AutomationResponse {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(AutomationError::new(code, message)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChannelKind {
    Control = 1,
    Bulk = 2,
}

impl ChannelKind {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Control),
            2 => Ok(Self::Bulk),
            _ => Err(invalid("unknown VVMX channel kind")),
        }
    }

    fn maximum(self) -> u32 {
        match self {
            Self::Control => CONTROL_MAX_BODY,
            Self::Bulk => BULK_MAX_BODY,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub enum Axis {
    Vertical,
    Horizontal,
}

/// Where a `run` pane goes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub enum RunPlacement {
    /// Split the target pane, as `msg split` would.
    Split { axis: Axis },
    /// A floating pane over the target's tab.
    Float,
    /// A new tab of its own.
    Tab,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub enum Action {
    Split(Axis),
    Focus(Direction),
    Resize(Direction),
    NewTab,
    NextTab,
    PreviousTab,
    SelectTab(usize),
    ToggleTabNavigator,
    BeginRenameTab,
    BeginClosePaneConfirmation,
    ResolveClosePaneConfirmation(bool),
    ClosePane,
    ToggleZoom,
    ToggleSyncInput,
    EnterCopyMode,
    CopyInput(Vec<u8>),
    Paste,
    NewFloatingPane,
    ToggleFloatingPanes,
    TogglePanePinned,
    /// Flip whether this pane paints its own background or leaves it to the outer terminal.
    TogglePaneTransparency,
    EnterFloatingMoveMode,
    EnterFloatingResizeMode,
    /// Invoke a configured plugin action. The host resolves and validates this reference.
    Plugin(String),
    ToggleAgentNavigator,
    /// Open the status-row prompt that writes the current layout to a startup layout file.
    BeginSaveLayout,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FloatingEditKind {
    Move,
    Resize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FloatingEditCommand {
    /// One keyboard edit step; `cells` is validated to 1 (plain arrow) or 5 (Shift-arrow).
    Step {
        direction: Direction,
        cells: u8,
    },
    Commit,
    Cancel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Move,
    Wheel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: u8,
    pub x: u16,
    pub y: u16,
    pub kind: MouseKind,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMessage {
    Attach {
        replace: bool,
        target: AttachmentTarget,
        display: DisplayMetrics,
        vivid: bool,
        /// The directly hosting terminal speaks the Kitty graphics protocol.
        kitty_graphics: bool,
        /// Which Vivido window is presenting this session, and nothing that could reach it.
        outer: Option<OuterIdentity>,
    },
    Input(Vec<u8>),
    Mouse(MouseEvent),
    /// The client's host terminal gained or lost focus.
    ///
    /// This is terminal state rather than typed input: the session decides which pane, if any,
    /// asked to be told about it.
    Focus(bool),
    Resize(DisplayMetrics),
    Action(Action),
    RenderAck(u64),
    /// The client discarded queued frames and its screen no longer matches the server's model.
    ///
    /// Frame diffs are incremental, so the only safe recovery is a full redraw.
    RenderResync,
    BridgeNeedKeyframes(Vec<BridgeKeyframeRequest>),
    /// Raster sources whose outgoing delta chain broke on the outer hop.
    ///
    /// The bridge cannot synthesize a full frame — it never retains one — so recovery goes to the
    /// inner producer, which is the same path an inner base-frame loss already uses.
    BridgeNeedFullFrames(Vec<BridgeSourceKey>),
    BridgeCapabilitiesChanged {
        reason_mask: u64,
    },
    BridgeMediaAck {
        delivery_id: u64,
        delivered: bool,
    },
    /// A queued media delivery was superseded by a newer outer attachment generation.
    BridgeMediaReleased {
        delivery_id: u64,
    },
    BridgeSnapshotRetry {
        /// The bridge control session is uncertain and its hop-local identities will be replaced.
        ///
        /// Source-scoped recovery and display-generation churn retry on the existing session and
        /// must preserve unrelated attachment and fragment mappings.
        reset_outer_session: bool,
    },
    /// A retained body for one source reached the outer presenter.
    BridgeRetainedHydrated {
        source: BridgeSourceKey,
    },
    BridgeApplied {
        bridge_instance_id: u64,
        virtual_revision: u64,
        outer_revision: u64,
        outer_attachment_generations: Vec<(BridgeSourceKey, u64)>,
        /// Image/raster sources whose outer tracks were created by this projection.
        ///
        /// Fresh outer tracks have no pixels even when the same virtual source was presented
        /// before. The session uses this source-scoped report to replay retained bodies that it
        /// skipped because an older projection still reported the source as resident.
        recreated_retained_sources: Vec<BridgeSourceKey>,
    },
    BridgeTrace {
        bridge_instance_id: u64,
        event: crate::media_trace::BridgeMediaTraceEvent,
    },
    BridgePlaybackState {
        source: BridgeSourceKey,
        state: u64,
        eos_state: u64,
    },
    /// Foreground-bridge counters, reported periodically for `inspect-media`.
    ///
    /// Diagnostic only: the actor stores the latest report and never lets it influence
    /// projection, delivery, or flow control.
    BridgeMetrics(crate::metrics::BridgeMetrics),
    /// Keyboard float-edit input, valid only while the actor-confirmed mode `mode_id` is
    /// current; the actor ignores stale IDs.
    FloatingEdit {
        mode_id: u64,
        command: FloatingEditCommand,
    },
    Detach,
    Kill,
    Ping,
    Automation(AutomationRequest),
    /// SGR-Pixels mouse input. Coordinates are zero-based physical pixels in the attached
    /// terminal, unlike [`Self::Mouse`], whose coordinates are zero-based cells.
    PixelMouse(MouseEvent),
}

/// Which session projection a foreground controller owns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachmentTarget {
    /// The ordinary tab/pane multiplexer UI.
    Session,
    /// One pane rendered over the complete host terminal.
    Pane { pane_id: u64 },
    /// Resolve one live agent alias atomically when attachment is admitted.
    Agent { alias: crate::agent::AgentAlias },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMessage {
    Attached {
        session: String,
        text_only: bool,
    },
    Render {
        frame_id: u64,
        session_sequence: u64,
        full: bool,
        last: bool,
        bytes: Vec<u8>,
    },
    Title(String),
    Bell,
    /// Ask the foreground client to raise a desktop notification.
    ///
    /// The server decides whether to notify and what to say; the client alone knows the outer
    /// terminal and decides how to render it. Nothing about the outer terminal travels back.
    Notify {
        kind: NotifyKind,
        title: String,
        body: Option<String>,
    },
    Clipboard(String),
    Status(String),
    /// Effective plugin prefix bindings for this registry generation. The client owns prefix
    /// parsing and applies these only where neither user configuration nor a core chord wins.
    PluginKeymap {
        generation: u64,
        bindings: Vec<PluginKeybinding>,
    },
    MediaSnapshot {
        revision: u64,
        surfaces: Vec<BridgeSurface>,
        tracks: Vec<BridgeSource>,
        nodes: Vec<BridgeNode>,
        videos_needing_keyframes: Vec<BridgeSourceKey>,
    },
    MediaRecord {
        delivery_id: u64,
        source: BridgeSourceKey,
        record_type: u16,
        offset: u32,
        total: u32,
        last: bool,
        bytes: Vec<u8>,
    },
    Detached {
        reason: String,
    },
    /// Authoritative float-edit mode state: `pane`/`kind` are set while a mode is active and
    /// `None` after it ends. The client parses arrows/Enter/Escape only while a mode with this
    /// `mode_id` is active.
    FloatingEditMode {
        mode_id: u64,
        pane: Option<u64>,
        kind: Option<FloatingEditKind>,
    },
    Error(String),
    Pong,
    Automation(AutomationResponse),
    AutomationChunk {
        request_id: u64,
        index: u32,
        last: bool,
        base64: String,
    },
    PluginEvent {
        subscription_id: String,
        envelope: Box<PluginEventEnvelope>,
    },
    /// Kitty keyboard flags requested by the focused pane. The native client applies these to
    /// its host terminal so key encodings survive a nested terminal boundary.
    InputMode {
        keyboard_flags: u8,
        sgr_pixels: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginKeybinding {
    pub chord: u8,
    /// Canonical `PLUGIN/ACTION` reference.
    pub action: String,
}

pub struct RecordReader {
    stream: Box<dyn Read + Send>,
    expected_sequence: u64,
    maximum_body: u32,
    cancel: ConnectionCancel,
    counters: Arc<IpcCounters>,
}

pub struct RecordWriter {
    stream: Box<dyn Write + Send>,
    next_sequence: u64,
    maximum_body: u32,
    counters: Arc<IpcCounters>,
}

pub type SharedWriter = Arc<Mutex<RecordWriter>>;

pub fn establish(
    mut stream: Transport,
    channel: ChannelKind,
) -> io::Result<(RecordReader, SharedWriter)> {
    let preface = encode_preface(channel, channel.maximum());
    stream.writer.write_all(&preface)?;
    let mut peer = [0_u8; 12];
    stream.reader.read_exact(&mut peer)?;
    let (peer_channel, peer_maximum) = decode_preface(&peer)?;
    if peer_channel != channel {
        return Err(invalid("VVMX channel mismatch"));
    }
    let maximum = channel.maximum().min(peer_maximum);
    let cancel = stream.cancel();
    // One counter set per connection, shared by its reader and writer so a caller holding either
    // half can report the whole hop.
    let counters = IpcCounters::shared();
    Ok((
        RecordReader {
            stream: stream.reader,
            expected_sequence: 0,
            maximum_body: maximum,
            cancel,
            counters: counters.clone(),
        },
        Arc::new(Mutex::new(RecordWriter {
            stream: stream.writer,
            next_sequence: 0,
            maximum_body: maximum,
            counters,
        })),
    ))
}

impl RecordReader {
    pub fn cancel_handle(&self) -> ConnectionCancel {
        self.cancel.clone()
    }

    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        let (record_type, flags, body) = self.read_raw()?;
        if record_type != STRUCTURED_RECORD || flags != 0 {
            return Err(invalid("unexpected VVMX control record"));
        }
        decode_structured(&body)
    }

    /// Receive one server message, reassembling the binary record forms.
    ///
    /// Callers keep matching on [`ServerMessage`]; only the encoding differs, so the binary path
    /// is invisible above this method.
    pub fn recv_server(&mut self) -> io::Result<ServerMessage> {
        let (record_type, flags, body) = self.read_raw()?;
        if flags != 0 {
            return Err(invalid("unexpected VVMX control record"));
        }
        match record_type {
            STRUCTURED_RECORD => decode_structured(&body),
            MEDIA_RECORD => decode_media_record(body),
            RENDER_RECORD => decode_render_record(body),
            _ => Err(invalid("unexpected VVMX control record")),
        }
    }

    pub fn read_raw(&mut self) -> io::Result<(u16, u16, Vec<u8>)> {
        let mut header = [0_u8; 16];
        self.stream.read_exact(&mut header)?;
        let sequence = u64::from_be_bytes(header[0..8].try_into().unwrap());
        let record_type = u16::from_be_bytes(header[8..10].try_into().unwrap());
        let flags = u16::from_be_bytes(header[10..12].try_into().unwrap());
        let length = u32::from_be_bytes(header[12..16].try_into().unwrap());
        if sequence != self.expected_sequence {
            return Err(invalid("VVMX record sequence gap"));
        }
        if flags & !0x0001 != 0 {
            return Err(invalid("VVMX record uses reserved flags"));
        }
        if length > self.maximum_body {
            return Err(invalid("VVMX record body exceeds negotiated limit"));
        }
        let mut body = vec![0; length as usize];
        self.stream.read_exact(&mut body)?;
        self.expected_sequence = self.expected_sequence.wrapping_add(1);
        self.counters
            .record_read(header.len().saturating_add(body.len()));
        Ok((record_type, flags, body))
    }
}

impl RecordWriter {
    pub fn send<T: Serialize>(&mut self, message: &T) -> io::Result<()> {
        let body = serde_json::to_vec(message).map_err(io::Error::other)?;
        self.write_raw(STRUCTURED_RECORD, 0, &body)
    }

    pub fn write_raw(&mut self, record_type: u16, flags: u16, body: &[u8]) -> io::Result<()> {
        self.write_raw_parts(record_type, flags, &[body])
    }

    /// Write one record whose body is the concatenation of `parts`.
    ///
    /// The binary forms are a fixed header followed by a payload the caller already owns.
    /// Concatenating them first would allocate and copy the payload once per record, which is most
    /// of what this encoding exists to avoid.
    pub fn write_raw_parts(
        &mut self,
        record_type: u16,
        flags: u16,
        parts: &[&[u8]],
    ) -> io::Result<()> {
        if flags & !0x0001 != 0 {
            return Err(invalid("VVMX record uses reserved flags"));
        }
        let length = parts
            .iter()
            .try_fold(0_usize, |total, part| total.checked_add(part.len()))
            .ok_or_else(|| invalid("VVMX record body length overflows"))?;
        if length > self.maximum_body as usize {
            return Err(invalid("VVMX record body exceeds negotiated limit"));
        }
        let mut header = [0_u8; 16];
        header[0..8].copy_from_slice(&self.next_sequence.to_be_bytes());
        header[8..10].copy_from_slice(&record_type.to_be_bytes());
        header[10..12].copy_from_slice(&flags.to_be_bytes());
        header[12..16].copy_from_slice(&(length as u32).to_be_bytes());
        // The peer's socket buffer is the only backpressure this writer has, so the time spent
        // here is the time the caller's thread was unavailable for anything else.
        let blocked = BlockTimer::start();
        self.stream.write_all(&header)?;
        for part in parts {
            self.stream.write_all(part)?;
        }
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.counters
            .record_write(header.len().saturating_add(length), blocked.elapsed());
        Ok(())
    }

    /// Largest byte payload that fits in one record of the given binary form.
    fn payload_capacity(&self, header: usize) -> usize {
        (self.maximum_body as usize).saturating_sub(header)
    }

    fn write_media_record(&mut self, chunk: MediaChunk, bytes: &[u8]) -> io::Result<()> {
        let mut header = [0_u8; MEDIA_RECORD_HEADER];
        header[0..8].copy_from_slice(&chunk.delivery_id.to_be_bytes());
        header[8..16].copy_from_slice(&chunk.source.producer.to_be_bytes());
        header[16..24].copy_from_slice(&chunk.source.context.to_be_bytes());
        header[24..32].copy_from_slice(&chunk.source.surface.to_be_bytes());
        header[32..40].copy_from_slice(&chunk.source.track.to_be_bytes());
        header[40..42].copy_from_slice(&chunk.record_type.to_be_bytes());
        header[42..44].copy_from_slice(&if chunk.last { MEDIA_FLAG_LAST } else { 0 }.to_be_bytes());
        header[44..48].copy_from_slice(&chunk.offset.to_be_bytes());
        header[48..52].copy_from_slice(&chunk.total.to_be_bytes());
        self.write_raw_parts(MEDIA_RECORD, 0, &[&header, bytes])
    }

    fn write_render_record(
        &mut self,
        frame_id: u64,
        session_sequence: u64,
        full: bool,
        last: bool,
        bytes: &[u8],
    ) -> io::Result<()> {
        let mut flags = 0;
        if full {
            flags |= RENDER_FLAG_FULL;
        }
        if last {
            flags |= RENDER_FLAG_LAST;
        }
        let mut header = [0_u8; RENDER_RECORD_HEADER];
        header[0..8].copy_from_slice(&frame_id.to_be_bytes());
        header[8..16].copy_from_slice(&session_sequence.to_be_bytes());
        header[16..18].copy_from_slice(&flags.to_be_bytes());
        self.write_raw_parts(RENDER_RECORD, 0, &[&header, bytes])
    }

    pub fn counters(&self) -> Arc<IpcCounters> {
        self.counters.clone()
    }
}

/// A writer over an arbitrary sink, for tests that need a [`SharedWriter`] without a peer.
#[cfg(test)]
pub(crate) fn test_shared_writer(stream: Box<dyn Write + Send>) -> SharedWriter {
    Arc::new(Mutex::new(RecordWriter {
        stream,
        next_sequence: 0,
        maximum_body: CONTROL_MAX_BODY,
        counters: IpcCounters::shared(),
    }))
}

pub fn send(writer: &SharedWriter, message: &ServerMessage) -> io::Result<()> {
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(message)
}

/// Send one media body as `MEDIA_RECORD` chunks.
///
/// Chunk size is derived from the negotiated ceiling rather than fixed, so a chunk can never be
/// rejected for exceeding it. Under the previous JSON encoding the fixed size was a guess against
/// an expansion that varies with the payload's byte values.
pub fn send_media_record(
    writer: &SharedWriter,
    delivery_id: u64,
    source: BridgeSourceKey,
    record_type: u16,
    body: &[u8],
) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writer.counters().record_media_payload(body.len());
    let total = u32::try_from(body.len())
        .map_err(|_| invalid("VVMX media body exceeds the addressable chunk range"))?;
    let chunk = writer
        .payload_capacity(MEDIA_RECORD_HEADER)
        .clamp(1, MEDIA_CHUNK);
    let mut header = MediaChunk {
        delivery_id,
        source,
        record_type,
        offset: 0,
        total,
        last: true,
    };
    if body.is_empty() {
        return writer.write_media_record(header, &[]);
    }
    for (index, bytes) in body.chunks(chunk).enumerate() {
        header.offset = (index * chunk) as u32;
        header.last = header.offset as usize + bytes.len() == body.len();
        writer.write_media_record(header, bytes)?;
    }
    Ok(())
}

/// Send one terminal frame as `RENDER_RECORD` chunks; `full` marks only the first.
pub fn send_render_record(
    writer: &SharedWriter,
    frame_id: u64,
    session_sequence: u64,
    full: bool,
    body: &[u8],
) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writer.counters().record_render_payload(body.len());
    let chunk = writer
        .payload_capacity(RENDER_RECORD_HEADER)
        .clamp(1, RENDER_CHUNK);
    if body.is_empty() {
        return writer.write_render_record(frame_id, session_sequence, full, true, &[]);
    }
    let chunks = body.len().div_ceil(chunk);
    for (index, bytes) in body.chunks(chunk).enumerate() {
        writer.write_render_record(
            frame_id,
            session_sequence,
            full && index == 0,
            index + 1 == chunks,
            bytes,
        )?;
    }
    Ok(())
}

pub fn send_automation(writer: &SharedWriter, mut response: AutomationResponse) -> io::Result<()> {
    let mut encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if encoded.len() > AUTOMATION_RESPONSE_LIMIT {
        response = AutomationResponse::error(
            response.id,
            "limit_exceeded",
            "automation response exceeds the 16 MiB decoded limit",
        );
        encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
    }
    if encoded.len() <= CONTROL_MAX_BODY as usize / 2 {
        return send(writer, &ServerMessage::Automation(response));
    }
    use base64::Engine;
    let mut locked = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let chunks = encoded.chunks(AUTOMATION_CHUNK_BYTES).collect::<Vec<_>>();
    for (index, chunk) in chunks.iter().enumerate() {
        locked.send(&ServerMessage::AutomationChunk {
            request_id: response.id,
            index: index as u32,
            last: index + 1 == chunks.len(),
            base64: base64::engine::general_purpose::STANDARD.encode(chunk),
        })?;
    }
    Ok(())
}

fn decode_structured<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    serde_json::from_slice(body).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed VVMX message: {error}"),
        )
    })
}

/// Reassemble a `MediaRecord` from its binary form.
///
/// `read_raw` has already bounded the body against the negotiated ceiling, so the payload is
/// bounded before it reaches here. Every reserved field and flag bit is still checked: a peer that
/// sets one is speaking a form this build does not implement.
fn decode_media_record(mut body: Vec<u8>) -> io::Result<ServerMessage> {
    if body.len() < MEDIA_RECORD_HEADER {
        return Err(invalid("VVMX media record is shorter than its header"));
    }
    let delivery_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
    let producer = u64::from_be_bytes(body[8..16].try_into().unwrap());
    let context = u64::from_be_bytes(body[16..24].try_into().unwrap());
    let surface = u64::from_be_bytes(body[24..32].try_into().unwrap());
    let track = u64::from_be_bytes(body[32..40].try_into().unwrap());
    let record_type = u16::from_be_bytes(body[40..42].try_into().unwrap());
    let flags = u16::from_be_bytes(body[42..44].try_into().unwrap());
    let offset = u32::from_be_bytes(body[44..48].try_into().unwrap());
    let total = u32::from_be_bytes(body[48..52].try_into().unwrap());
    if u32::from_be_bytes(body[52..56].try_into().unwrap()) != 0 {
        return Err(invalid("VVMX media record reserved field is nonzero"));
    }
    if flags & !MEDIA_FLAG_LAST != 0 {
        return Err(invalid("VVMX media record uses reserved flags"));
    }
    body.drain(..MEDIA_RECORD_HEADER);
    // The chunk must fit inside the declared body, and the final chunk must end exactly at it.
    let end = (offset as u64)
        .checked_add(body.len() as u64)
        .ok_or_else(|| invalid("VVMX media record chunk extent overflows"))?;
    if end > u64::from(total) {
        return Err(invalid("VVMX media record chunk exceeds its total"));
    }
    if (flags & MEDIA_FLAG_LAST != 0) != (end == u64::from(total)) {
        return Err(invalid(
            "VVMX media record last flag disagrees with its total",
        ));
    }
    Ok(ServerMessage::MediaRecord {
        delivery_id,
        source: BridgeSourceKey {
            producer,
            context,
            surface,
            track,
        },
        record_type,
        offset,
        total,
        last: flags & MEDIA_FLAG_LAST != 0,
        bytes: body,
    })
}

fn decode_render_record(mut body: Vec<u8>) -> io::Result<ServerMessage> {
    if body.len() < RENDER_RECORD_HEADER {
        return Err(invalid("VVMX render record is shorter than its header"));
    }
    let frame_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
    let session_sequence = u64::from_be_bytes(body[8..16].try_into().unwrap());
    let flags = u16::from_be_bytes(body[16..18].try_into().unwrap());
    if u16::from_be_bytes(body[18..20].try_into().unwrap()) != 0
        || u32::from_be_bytes(body[20..24].try_into().unwrap()) != 0
    {
        return Err(invalid("VVMX render record reserved field is nonzero"));
    }
    if flags & !(RENDER_FLAG_FULL | RENDER_FLAG_LAST) != 0 {
        return Err(invalid("VVMX render record uses reserved flags"));
    }
    body.drain(..RENDER_RECORD_HEADER);
    Ok(ServerMessage::Render {
        frame_id,
        session_sequence,
        full: flags & RENDER_FLAG_FULL != 0,
        last: flags & RENDER_FLAG_LAST != 0,
        bytes: body,
    })
}

fn encode_preface(channel: ChannelKind, maximum_body: u32) -> [u8; 12] {
    let mut preface = [0_u8; 12];
    preface[0..4].copy_from_slice(MAGIC);
    preface[4..6].copy_from_slice(&VERSION.to_be_bytes());
    preface[6] = channel as u8;
    preface[7] = 0;
    preface[8..12].copy_from_slice(&maximum_body.to_be_bytes());
    preface
}

fn decode_preface(preface: &[u8; 12]) -> io::Result<(ChannelKind, u32)> {
    if &preface[0..4] != MAGIC {
        return Err(invalid("bad VVMX magic"));
    }
    if u16::from_be_bytes(preface[4..6].try_into().unwrap()) != VERSION {
        return Err(invalid(VERSION_MISMATCH));
    }
    if preface[7] != 0 {
        return Err(invalid("VVMX preface reserved byte is nonzero"));
    }
    let channel = ChannelKind::from_byte(preface[6])?;
    let maximum = u32::from_be_bytes(preface[8..12].try_into().unwrap());
    if maximum == 0 || maximum > channel.maximum() {
        return Err(invalid("invalid VVMX maximum body"));
    }
    Ok((channel, maximum))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_targets_are_explicit_and_round_trip() {
        for target in [
            AttachmentTarget::Session,
            AttachmentTarget::Pane { pane_id: 42 },
            AttachmentTarget::Agent {
                alias: "reviewer".parse().unwrap(),
            },
        ] {
            let bytes = serde_json::to_vec(&target).unwrap();
            assert_eq!(
                serde_json::from_slice::<AttachmentTarget>(&bytes).unwrap(),
                target
            );
        }
    }

    #[derive(Clone, Default)]
    struct SharedBytes(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBytes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn preface_rejects_version_reserved_and_limits() {
        let mut preface = encode_preface(ChannelKind::Control, CONTROL_MAX_BODY);
        assert_eq!(decode_preface(&preface).unwrap().0, ChannelKind::Control);
        preface[7] = 1;
        assert!(decode_preface(&preface).is_err());
        preface = encode_preface(ChannelKind::Control, CONTROL_MAX_BODY);
        preface[4..6].copy_from_slice(&VERSION.wrapping_add(1).to_be_bytes());
        let error = decode_preface(&preface).unwrap_err();
        assert!(error.to_string().contains("restart"));
    }

    #[test]
    fn plugin_pane_open_has_a_typed_vvmx_operation() {
        let method = PluginMethod::PaneOpen {
            reference: "dev.example/dashboard".into(),
        };
        let encoded = serde_json::to_value(&method).unwrap();
        assert_eq!(encoded["operation"], "pane_open");
        assert_eq!(encoded["reference"], "dev.example/dashboard");
        assert_eq!(
            serde_json::from_value::<PluginMethod>(encoded).unwrap(),
            method
        );
    }

    #[test]
    fn plugin_event_subscription_and_gap_are_typed_vvmx_operations() {
        let method = PluginMethod::EventSubscribe {
            after_sequence: Some(41),
        };
        let encoded = serde_json::to_value(&method).unwrap();
        assert_eq!(encoded["operation"], "event_subscribe");
        assert_eq!(encoded["after_sequence"], 41);
        assert_eq!(
            serde_json::from_value::<PluginMethod>(encoded).unwrap(),
            method
        );
        let gap = PluginEventEnvelope::Gap {
            from_sequence: 42,
            to_sequence: 99,
        };
        assert_eq!(serde_json::to_value(&gap).unwrap()["type"], "gap");
    }

    #[test]
    fn structured_records_round_trip_and_sequence_is_checked() {
        use std::net::{Ipv4Addr, TcpListener, TcpStream};

        fn transport(stream: TcpStream) -> Transport {
            let reader = stream.try_clone().unwrap();
            Transport::new(
                Box::new(reader),
                Box::new(stream),
                ConnectionCancel::inert(),
                Arc::new(|_| Ok(())),
            )
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let right = TcpStream::connect(address).unwrap();
        let (left, _) = listener.accept().unwrap();
        let server =
            std::thread::spawn(move || establish(transport(left), ChannelKind::Control).unwrap());
        let (mut client_reader, client_writer) =
            establish(transport(right), ChannelKind::Control).unwrap();
        let (mut server_reader, server_writer) = server.join().unwrap();
        client_writer
            .lock()
            .unwrap()
            .send(&ClientMessage::Ping)
            .unwrap();
        assert_eq!(
            server_reader.recv::<ClientMessage>().unwrap(),
            ClientMessage::Ping
        );
        let recovery = ClientMessage::BridgeNeedKeyframes(vec![BridgeKeyframeRequest {
            source: BridgeSourceKey {
                producer: 7,
                context: 3,
                surface: 5,
                track: 11,
            },
            minimum_epoch: Some(3),
            reason: crate::bridge::KEYFRAME_REASON_TRANSPORT_LOSS,
        }]);
        client_writer.lock().unwrap().send(&recovery).unwrap();
        assert_eq!(server_reader.recv::<ClientMessage>().unwrap(), recovery);
        server_writer
            .lock()
            .unwrap()
            .send(&ServerMessage::Pong)
            .unwrap();
        assert_eq!(client_reader.recv_server().unwrap(), ServerMessage::Pong);
    }

    fn test_writer(output: &SharedBytes, counters: &Arc<IpcCounters>) -> SharedWriter {
        Arc::new(Mutex::new(RecordWriter {
            stream: Box::new(output.clone()),
            next_sequence: 0,
            maximum_body: CONTROL_MAX_BODY,
            counters: counters.clone(),
        }))
    }

    fn test_reader(bytes: Vec<u8>) -> RecordReader {
        RecordReader {
            stream: Box::new(io::Cursor::new(bytes)),
            expected_sequence: 0,
            maximum_body: CONTROL_MAX_BODY,
            cancel: ConnectionCancel::inert(),
            counters: IpcCounters::shared(),
        }
    }

    /// Uniformly distributed bytes, as compressed media is: most values need three JSON digits.
    fn scrambled(length: usize) -> Vec<u8> {
        (0..length as u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect()
    }

    /// The reason this encoding exists: JSON has no byte type, so a `Vec<u8>` costs several bytes
    /// per payload byte, formatted on the session actor and parsed on the client's reader thread.
    #[test]
    fn binary_media_records_cost_their_payload_instead_of_a_json_number_array() {
        let source = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 4,
            track: 9,
        };
        let payload = scrambled(64 * 1024);

        let json = SharedBytes::default();
        let json_counters = IpcCounters::shared();
        test_writer(&json, &json_counters)
            .lock()
            .unwrap()
            .send(&ServerMessage::MediaRecord {
                delivery_id: 1,
                source,
                record_type: 0x8001,
                offset: 0,
                total: payload.len() as u32,
                last: true,
                bytes: payload.clone(),
            })
            .unwrap();

        let binary = SharedBytes::default();
        let counters = IpcCounters::shared();
        send_media_record(
            &test_writer(&binary, &counters),
            1,
            source,
            0x8001,
            &payload,
        )
        .unwrap();

        let json_bytes = json.0.lock().unwrap().len();
        let binary_bytes = binary.0.lock().unwrap().len();
        assert!(
            json_bytes > payload.len() * 3,
            "expected the JSON form to exceed 3x its payload, got {json_bytes}"
        );
        // Framing is a fixed header per chunk, so the binary form is payload-dominated.
        assert!(
            binary_bytes < payload.len() + 1024,
            "expected the binary form to be payload plus framing, got {binary_bytes}"
        );

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.media_records, 1);
        assert_eq!(snapshot.media_payload_bytes, payload.len() as u64);
        assert_eq!(snapshot.wire_bytes_written as usize, binary_bytes);
    }

    #[test]
    fn media_and_render_records_round_trip_byte_for_byte() {
        let source = BridgeSourceKey {
            producer: 7,
            context: 3,
            surface: 5,
            track: 11,
        };
        // Larger than one chunk so reassembly across records is covered.
        let media = scrambled(300 * 1024);
        let frame = scrambled(600 * 1024);

        let output = SharedBytes::default();
        let counters = IpcCounters::shared();
        let writer = test_writer(&output, &counters);
        send_media_record(&writer, 42, source, 0x8003, &media).unwrap();
        send_render_record(&writer, 5, 99, true, &frame).unwrap();

        let mut reader = test_reader(output.0.lock().unwrap().clone());
        let mut media_seen = Vec::new();
        let mut media_chunks = 0;
        loop {
            match reader.recv_server().unwrap() {
                ServerMessage::MediaRecord {
                    delivery_id,
                    source: key,
                    record_type,
                    offset,
                    total,
                    last,
                    bytes,
                } => {
                    assert_eq!((delivery_id, key, record_type), (42, source, 0x8003));
                    assert_eq!(offset as usize, media_seen.len());
                    assert_eq!(total as usize, media.len());
                    media_seen.extend_from_slice(&bytes);
                    media_chunks += 1;
                    if last {
                        break;
                    }
                }
                other => panic!("expected a media record, got {other:?}"),
            }
        }
        assert!(media_chunks > 1, "the payload should have spanned chunks");
        assert_eq!(media_seen, media);

        let mut frame_seen = Vec::new();
        let mut first = true;
        loop {
            match reader.recv_server().unwrap() {
                ServerMessage::Render {
                    frame_id,
                    session_sequence,
                    full,
                    last,
                    bytes,
                } => {
                    assert_eq!((frame_id, session_sequence), (5, 99));
                    // `full` marks the frame, not every chunk of it.
                    assert_eq!(full, first);
                    first = false;
                    frame_seen.extend_from_slice(&bytes);
                    if last {
                        break;
                    }
                }
                other => panic!("expected a render record, got {other:?}"),
            }
        }
        assert_eq!(frame_seen, frame);
    }

    /// The previous JSON encoding chunked terminal frames at a fixed 256 KiB, which expanded past
    /// the 1 MiB record ceiling for payloads of mostly high bytes and made the server drop the
    /// client. Chunking now derives from the ceiling, so no payload can produce an oversized record.
    #[test]
    fn worst_case_payloads_never_exceed_the_negotiated_record_ceiling() {
        let output = SharedBytes::default();
        let counters = IpcCounters::shared();
        let writer = test_writer(&output, &counters);
        let high_bytes = vec![0xff_u8; 4 * 1024 * 1024];
        send_render_record(&writer, 1, 1, true, &high_bytes).unwrap();
        send_media_record(
            &writer,
            1,
            BridgeSourceKey {
                producer: 1,
                context: 1,
                surface: 1,
                track: 1,
            },
            0x8001,
            &high_bytes,
        )
        .unwrap();

        let bytes = output.0.lock().unwrap().clone();
        let mut cursor = 0;
        while cursor < bytes.len() {
            let length =
                u32::from_be_bytes(bytes[cursor + 12..cursor + 16].try_into().unwrap()) as usize;
            assert!(
                length <= CONTROL_MAX_BODY as usize,
                "record body {length} exceeds the ceiling"
            );
            cursor += 16 + length;
        }
        assert_eq!(cursor, bytes.len());
    }

    #[test]
    fn binary_records_reject_reserved_fields_and_inconsistent_extents() {
        let output = SharedBytes::default();
        let counters = IpcCounters::shared();
        let writer = test_writer(&output, &counters);
        send_media_record(
            &writer,
            1,
            BridgeSourceKey {
                producer: 1,
                context: 1,
                surface: 1,
                track: 1,
            },
            0x8001,
            &[1, 2, 3, 4],
        )
        .unwrap();
        let good = output.0.lock().unwrap().clone();

        // Reserved word set.
        let mut reserved = good.clone();
        reserved[16 + 52] = 1;
        assert!(test_reader(reserved).recv_server().is_err());

        // Unknown flag bit set.
        let mut flags = good.clone();
        flags[16 + 43] = 0xff;
        assert!(test_reader(flags).recv_server().is_err());

        // `last` set while the chunk stops short of the declared total.
        let mut short = good.clone();
        short[16 + 51] = 8;
        assert!(test_reader(short).recv_server().is_err());

        // Body shorter than the fixed header.
        let mut truncated = good[..16].to_vec();
        truncated[15] = 4;
        truncated.extend_from_slice(&[0; 4]);
        assert!(test_reader(truncated).recv_server().is_err());
    }

    #[test]
    fn large_automation_responses_are_correlated_and_chunked_below_record_limit() {
        use base64::Engine;

        let output = SharedBytes::default();
        let writer = Arc::new(Mutex::new(RecordWriter {
            stream: Box::new(output.clone()),
            next_sequence: 0,
            maximum_body: CONTROL_MAX_BODY,
            counters: IpcCounters::shared(),
        }));
        let response =
            AutomationResponse::success(77, Value::String("x".repeat(CONTROL_MAX_BODY as usize)));
        send_automation(&writer, response.clone()).unwrap();

        let bytes = output.0.lock().unwrap().clone();
        let mut cursor = 0;
        let mut sequence = 0;
        let mut decoded = Vec::new();
        loop {
            let header = &bytes[cursor..cursor + 16];
            assert_eq!(
                u64::from_be_bytes(header[0..8].try_into().unwrap()),
                sequence
            );
            let length = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
            assert!(length <= CONTROL_MAX_BODY as usize);
            cursor += 16;
            let message: ServerMessage =
                serde_json::from_slice(&bytes[cursor..cursor + length]).unwrap();
            cursor += length;
            sequence += 1;
            match message {
                ServerMessage::AutomationChunk {
                    request_id,
                    index,
                    last,
                    base64,
                } => {
                    assert_eq!(request_id, 77);
                    assert_eq!(u64::from(index), sequence - 1);
                    decoded.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(base64)
                            .unwrap(),
                    );
                    if last {
                        break;
                    }
                }
                other => panic!("unexpected chunk message: {other:?}"),
            }
        }
        assert_eq!(cursor, bytes.len());
        assert_eq!(
            serde_json::from_slice::<AutomationResponse>(&decoded).unwrap(),
            response
        );
    }
}

#[cfg(test)]
mod capability_tests {
    use super::{METHOD_CAPABILITIES, MethodCapability, MethodClass};

    fn advertised(name: &str) -> Option<&'static MethodCapability> {
        METHOD_CAPABILITIES
            .iter()
            .find(|capability| capability.name == name)
    }

    /// Every `method` tag the generated schema knows, which is every `AutomationMethod` variant.
    fn schema_method_names() -> Vec<String> {
        let schema = crate::api::schema();
        let mut names = Vec::new();
        collect(&schema, &mut names);
        names.sort();
        names.dedup();
        names
    }

    /// The `method` tag appears as a single-value `const`/`enum` on each variant's object schema.
    fn collect(value: &serde_json::Value, names: &mut Vec<String>) {
        if let Some(object) = value.as_object() {
            if let Some(method) = object.get("properties").and_then(|properties| {
                properties
                    .get("method")
                    .and_then(|method| method.as_object())
            }) {
                if let Some(name) = method.get("const").and_then(|name| name.as_str()) {
                    names.push(name.to_owned());
                } else if let Some(values) = method.get("enum").and_then(|values| values.as_array())
                {
                    names.extend(
                        values
                            .iter()
                            .filter_map(|name| name.as_str())
                            .map(str::to_owned),
                    );
                }
            }
            for nested in object.values() {
                collect(nested, names);
            }
        } else if let Some(array) = value.as_array() {
            for nested in array {
                collect(nested, names);
            }
        }
    }

    /// The gate that makes the table a source of truth rather than a fourth list to forget.
    ///
    /// A new `AutomationMethod` variant changes the schema, so it fails here until it is given a
    /// class — which is how `session_snapshot` came to be advertised as `snapshot`.
    #[test]
    fn every_wire_method_is_advertised_with_a_class() {
        let mut advertised = METHOD_CAPABILITIES
            .iter()
            .map(|capability| capability.name.to_owned())
            .collect::<Vec<_>>();
        advertised.sort();
        let schema = schema_method_names();
        assert!(
            !schema.is_empty(),
            "the schema must expose the method tags this test compares against"
        );
        assert_eq!(
            schema, advertised,
            "METHOD_CAPABILITIES and the AutomationMethod schema disagree"
        );
    }

    #[test]
    fn advertised_names_are_unique_and_resolvable() {
        let mut seen = std::collections::BTreeSet::new();
        for capability in METHOD_CAPABILITIES {
            assert!(
                seen.insert(capability.name),
                "duplicate advertised method {}",
                capability.name
            );
            assert_eq!(
                advertised(capability.name).map(|found| found.class),
                Some(capability.class)
            );
        }
        assert!(advertised("no_such_method").is_none());
    }

    /// `mutating` is a projection of `class`, so the two can never disagree in an advertisement.
    #[test]
    fn mutating_agrees_with_class() {
        for capability in METHOD_CAPABILITIES {
            assert_eq!(
                capability.mutating,
                capability.class.mutating(),
                "{} advertises the wrong mutating flag",
                capability.name
            );
            assert_eq!(
                capability.class == MethodClass::Observe,
                !capability.mutating,
                "{} is the only class that may be non-mutating",
                capability.name
            );
        }
    }

    /// The wire tag serde encodes and the tag `name()` reports must be the same string.
    #[test]
    fn name_matches_the_serialized_tag() {
        let samples = [
            super::AutomationMethod::Capabilities,
            super::AutomationMethod::GetConfig,
            super::AutomationMethod::SessionSnapshot,
            super::AutomationMethod::Inspect,
            super::AutomationMethod::ClosePane,
            super::AutomationMethod::Typing {
                text: "x".into(),
                report: false,
            },
            super::AutomationMethod::Plugin(super::PluginMethod::Reload),
            super::AutomationMethod::Action(super::Action::NewTab),
        ];
        for method in samples {
            let encoded = serde_json::to_value(&method).unwrap();
            assert_eq!(
                encoded["method"].as_str(),
                Some(method.name()),
                "name() disagrees with serde for {encoded}"
            );
            assert!(
                advertised(method.name()).is_some(),
                "{} is on the wire but not advertised",
                method.name()
            );
        }
    }

    /// `session_snapshot` and `plugin` are the two the hand-written list got wrong.
    #[test]
    fn advertises_the_previously_missing_entries() {
        assert!(advertised("session_snapshot").is_some());
        assert!(advertised("plugin").is_some());
        assert!(
            advertised("snapshot").is_none(),
            "`snapshot` is the CLI spelling and was never a wire method"
        );
    }
}
