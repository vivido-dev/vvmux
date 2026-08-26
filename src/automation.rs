use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use clap::{Args, Subcommand, ValueEnum};

use crate::ipc::{
    Action, AutomationCompletion, AutomationMethod, AutomationRequest, AutomationResponse, Axis,
    ClientMessage, Direction, MediaTrackIdentity, MediaTrackWaitCondition, PluginMethod,
    RunPlacement, ServerMessage, TabSelector, TextSource,
};
use crate::media_trace::{
    MAX_MEDIA_TRACE_QUERY_EVENTS, MediaTraceBatch, MediaTraceCategory, MediaTraceFilter,
};
use crate::search::SearchDirection;

const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SplitAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RunPlacementArg {
    /// Split the target pane.
    Split,
    /// Float over the target's tab.
    Float,
    /// Open a new tab.
    Tab,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SearchDirectionArg {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DirectionArg {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CompletionArg {
    Outer,
    Rendered,
}

impl From<CompletionArg> for AutomationCompletion {
    fn from(value: CompletionArg) -> Self {
        match value {
            CompletionArg::Outer => Self::Outer,
            CompletionArg::Rendered => Self::Rendered,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MediaTrackConditionArg {
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

impl From<MediaTrackConditionArg> for MediaTrackWaitCondition {
    fn from(value: MediaTrackConditionArg) -> Self {
        match value {
            MediaTrackConditionArg::Visible => Self::Visible,
            MediaTrackConditionArg::Hidden => Self::Hidden,
            MediaTrackConditionArg::OuterAttached => Self::OuterAttached,
            MediaTrackConditionArg::KeyframeNeeded => Self::KeyframeNeeded,
            MediaTrackConditionArg::KeyframeRecovered => Self::KeyframeRecovered,
            MediaTrackConditionArg::Playing => Self::Playing,
            MediaTrackConditionArg::Paused => Self::Paused,
            MediaTrackConditionArg::Eos => Self::Eos,
            MediaTrackConditionArg::Lost => Self::Lost,
            MediaTrackConditionArg::QueueDrained => Self::QueueDrained,
        }
    }
}

impl From<DirectionArg> for Direction {
    fn from(direction: DirectionArg) -> Self {
        match direction {
            DirectionArg::Left => Self::Left,
            DirectionArg::Right => Self::Right,
            DirectionArg::Up => Self::Up,
            DirectionArg::Down => Self::Down,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ActionCommand {
    Split {
        #[arg(value_enum)]
        axis: SplitAxis,
    },
    Focus {
        #[arg(value_enum)]
        direction: DirectionArg,
    },
    Resize {
        #[arg(value_enum)]
        direction: DirectionArg,
    },
    NewTab,
    NextTab,
    PreviousTab,
    SelectTab {
        index: usize,
    },
    ClosePane,
    ToggleZoom,
    ToggleSyncInput,
    EnterCopyMode,
    Paste,
    NewFloatingPane,
    ToggleFloatingPanes,
    TogglePanePinned,
    EnterFloatingMoveMode,
    EnterFloatingResizeMode,
}

impl From<ActionCommand> for Action {
    fn from(action: ActionCommand) -> Self {
        match action {
            ActionCommand::Split { axis } => Self::Split(axis.into()),
            ActionCommand::Focus { direction } => Self::Focus(direction.into()),
            ActionCommand::Resize { direction } => Self::Resize(direction.into()),
            ActionCommand::NewTab => Self::NewTab,
            ActionCommand::NextTab => Self::NextTab,
            ActionCommand::PreviousTab => Self::PreviousTab,
            ActionCommand::SelectTab { index } => Self::SelectTab(index),
            ActionCommand::ClosePane => Self::ClosePane,
            ActionCommand::ToggleZoom => Self::ToggleZoom,
            ActionCommand::ToggleSyncInput => Self::ToggleSyncInput,
            ActionCommand::EnterCopyMode => Self::EnterCopyMode,
            ActionCommand::Paste => Self::Paste,
            ActionCommand::NewFloatingPane => Self::NewFloatingPane,
            ActionCommand::ToggleFloatingPanes => Self::ToggleFloatingPanes,
            ActionCommand::TogglePanePinned => Self::TogglePanePinned,
            ActionCommand::EnterFloatingMoveMode => Self::EnterFloatingMoveMode,
            ActionCommand::EnterFloatingResizeMode => Self::EnterFloatingResizeMode,
        }
    }
}

impl From<SearchDirectionArg> for SearchDirection {
    fn from(direction: SearchDirectionArg) -> Self {
        match direction {
            SearchDirectionArg::Forward => Self::Forward,
            SearchDirectionArg::Backward => Self::Backward,
        }
    }
}

impl From<SplitAxis> for Axis {
    fn from(axis: SplitAxis) -> Self {
        match axis {
            SplitAxis::Vertical => Self::Vertical,
            SplitAxis::Horizontal => Self::Horizontal,
        }
    }
}

#[derive(Debug, Args)]
pub struct PaneTarget {
    #[arg(long)]
    pane_id: Option<u64>,
}

/// Which tab a command means: a stable ID, a name, or the active one.
#[derive(Debug, Args)]
pub struct TabTarget {
    /// The tab's stable ID, from `list-tabs` or `layout`.
    #[arg(long, conflicts_with = "tab_name")]
    tab_id: Option<u64>,
    /// The tab's name, matched case-insensitively. Refused when two tabs share it.
    #[arg(long, conflicts_with = "tab_id")]
    tab_name: Option<String>,
}

impl TabTarget {
    /// The active tab when neither is given: a caller acting on "this tab" should not have to look
    /// its ID up first.
    fn selector(self) -> TabSelector {
        match (self.tab_id, self.tab_name) {
            (Some(tab_id), _) => TabSelector::Id(tab_id),
            (None, Some(name)) => TabSelector::Name(name),
            (None, None) => TabSelector::Active,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum MsgCommand {
    /// Print the server's pane-automation capabilities.
    Capabilities,
    /// Run a bounded multi-step plan over one connection.
    ///
    /// Results flow between steps by alias, so a plan does not have to be re-parsed and re-passed
    /// by hand. Emits NDJSON: one `plan_started`, one line per step, one `plan_completed`.
    RunPlan {
        /// The plan file. Omitted or `-` reads stdin.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Report what would run without connecting to anything that changes.
        #[arg(long, conflicts_with = "preflight")]
        dry_run: bool,
        /// Run only the steps that observe, skipping every mutation.
        #[arg(long, conflicts_with = "dry_run")]
        preflight: bool,
    },
    /// Re-read the session's config file now, without waiting for the watcher.
    ReloadConfig,
    /// Print the configuration this session is actually running with.
    GetConfig,
    /// Revalidate the plugin registry immediately.
    ReloadPlugins,
    /// Apply an ordinary interactive action through the automation service.
    Action {
        #[command(subcommand)]
        action: ActionCommand,
        #[arg(long, global = true)]
        pane_id: Option<u64>,
    },
    /// List every pane in deterministic pane-ID order.
    ListPanes,
    /// Inspect attachment, active selection, revisions, pending work, bridge, and queues.
    SessionInspect,
    /// List tabs with stable IDs in display order.
    ListTabs,
    /// Select a tab by its stable ID or name.
    SelectTab {
        #[command(flatten)]
        tab: TabTarget,
        #[arg(long, value_enum)]
        wait: Option<CompletionArg>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
    },
    /// Describe every tab and pane: split tree, rectangles, and directional neighbors.
    Layout,
    /// Resolve a directional route, or the global `--pane-name`, to a pane without moving focus.
    ResolvePane {
        /// One step per direction, applied in order, e.g. `--path left,down`.
        #[arg(long, value_delimiter = ',', value_enum)]
        path: Vec<DirectionArg>,
        /// Where the route starts. Defaults to the calling pane, else the tab's focused pane.
        #[arg(long)]
        pane_id: Option<u64>,
        #[command(flatten)]
        tab: TabTarget,
    },
    /// Name one pane so it stays addressable across a server restart.
    PaneRename {
        #[arg(long, conflicts_with = "clear")]
        name: Option<crate::layout::PaneName>,
        /// Release the pane's name.
        #[arg(long, conflicts_with = "name")]
        clear: bool,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Reveal a pane without moving focus or stealing the attachment.
    ActivatePane(PaneTarget),
    /// Send a mouse action to a pane, in pane-local coordinates.
    Mouse {
        #[arg(value_enum)]
        action: crate::ipc::MouseAction,
        /// Zero-based cell inside the pane's content area.
        #[arg(long, requires = "cell_row")]
        cell_column: Option<u16>,
        #[arg(long, requires = "cell_column")]
        cell_row: Option<u16>,
        /// Physical pixels inside the pane's content area. Needs an attached client.
        #[arg(long, requires = "y", conflicts_with_all = ["cell_column", "relative_x"])]
        x: Option<u32>,
        #[arg(long, requires = "x")]
        y: Option<u32>,
        /// A fraction of the pane, 0.0 through 1.0.
        #[arg(long, requires = "relative_y", conflicts_with_all = ["cell_column", "x"])]
        relative_x: Option<f32>,
        #[arg(long, requires = "relative_x")]
        relative_y: Option<f32>,
        /// Gesture points for `path`, as `COLUMN,ROW` cells. Repeatable, 2 through 1000.
        #[arg(long = "point", value_parser = parse_mouse_point)]
        points: Vec<(u16, u16)>,
        #[arg(long, value_enum, default_value_t = crate::ipc::MouseButton::Left)]
        button: crate::ipc::MouseButton,
        #[arg(long, value_enum, default_value_t = crate::ipc::MouseRoute::Application)]
        route: crate::ipc::MouseRoute,
        /// Comma-separated modifiers: `Shift`, `Alt`, `Ctrl`.
        #[arg(long, value_delimiter = ',')]
        mods: Vec<String>,
        /// Wheel notches for `scroll`: negative scrolls up, positive down.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        scroll: i16,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Run one command in a pane's own shell and wait for its real exit status.
    ///
    /// Needs the shell to emit OSC 133 markers; a pane whose shell reports no command boundaries
    /// is refused rather than guessed at.
    ShellCommand {
        command: String,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Reveal a pane, wait for it to settle, and read it, in one request.
    Capture {
        /// Read the pane where it is instead of revealing it first.
        #[arg(long)]
        no_activate: bool,
        #[arg(long)]
        after_screen: Option<u64>,
        /// Wait for the screen to stay unchanged this long before reading.
        #[arg(long, value_parser = parse_timeout)]
        stable: Option<Duration>,
        /// Also wait for the attached client to acknowledge a render.
        #[arg(long)]
        rendered: bool,
        /// Include the structured grid as well as the text.
        #[arg(long)]
        grid: bool,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Read a pane's bounded rolling window of raw output.
    Transcript {
        #[arg(long)]
        after_offset: Option<u64>,
        /// Return exact bytes, base64-encoded, instead of lossy text.
        #[arg(long)]
        base64: bool,
        #[arg(long)]
        max_bytes: Option<u32>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Give one pane an exact size instead of nudging it one step.
    ResizePane {
        #[arg(long)]
        columns: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Move a pane to another tab, swap it with a neighbour, or change its layer.
    MovePane {
        #[arg(long, conflicts_with_all = ["swap", "to_layer"])]
        to_tab: Option<u64>,
        #[arg(long, conflicts_with_all = ["swap", "to_layer"])]
        to_tab_name: Option<String>,
        #[arg(long, value_enum, conflicts_with = "to_layer")]
        swap: Option<DirectionArg>,
        #[arg(long, value_enum)]
        to_layer: Option<crate::ipc::PaneLayerRequest>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Set a pane or tab flag outright, instead of flipping it.
    SetFlag {
        #[arg(value_enum)]
        flag: crate::ipc::PaneFlag,
        #[arg(long, conflicts_with = "off")]
        on: bool,
        #[arg(long, conflicts_with = "on")]
        off: bool,
        /// Scrollback offset for `copy-mode`.
        #[arg(long)]
        offset: Option<usize>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Deliver a signal to a pane's foreground process group.
    Signal {
        #[arg(value_enum)]
        signal: crate::ipc::SignalName,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Open a tab and report the IDs it was given.
    NewTab {
        #[arg(long)]
        name: Option<String>,
    },
    /// Give a tab a name.
    RenameTab {
        #[arg(long)]
        name: String,
        #[command(flatten)]
        tab: TabTarget,
    },
    /// Drop a tab's name so it falls back to its process-derived title.
    ResetTabTitle {
        #[command(flatten)]
        tab: TabTarget,
    },
    /// Close a tab and every pane in it.
    CloseTab {
        #[command(flatten)]
        tab: TabTarget,
    },
    /// Capture one non-blocking correlated diagnostic snapshot.
    Diagnose {
        #[arg(long, conflicts_with = "all_panes")]
        pane_id: Option<u64>,
        #[arg(long)]
        all_panes: bool,
        #[arg(long, default_value_t = 128, value_parser = clap::value_parser!(u16).range(1..=512))]
        trace_limit: u16,
    },
    /// Report authoritative AI-agent state for one pane.
    ReportAgent {
        #[arg(long)]
        agent: crate::agent::AgentId,
        #[arg(long, value_enum)]
        state: crate::agent::AgentState,
        #[arg(long)]
        source: String,
        #[arg(long)]
        sequence: u64,
        /// Why the agent is blocked, shown beside it in the agent navigator.
        #[arg(long)]
        message: Option<String>,
        /// The agent's own session identifier, retained so the session can be resumed later.
        ///
        /// Reported once; later state-only reports from the same source keep it.
        #[arg(long = "agent-session-id")]
        session_id: Option<String>,
        /// The agent's own session file, for agents that identify a session by path.
        #[arg(long = "agent-session-path")]
        session_path: Option<String>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Report native agent session identity without overriding screen-classified state.
    ReportAgentSession {
        #[arg(long)]
        agent: crate::agent::AgentId,
        #[arg(long)]
        source: String,
        #[arg(long)]
        sequence: u64,
        #[arg(long = "agent-session-id", required_unless_present = "session_path")]
        session_id: Option<String>,
        #[arg(long = "agent-session-path", required_unless_present = "session_id")]
        session_path: Option<String>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Attach display-only metadata to one pane, without claiming lifecycle authority.
    ///
    /// Use this for progress and custom status text an integration wants shown; use
    /// `report-agent` only for real lifecycle state. Every option distinguishes "not given, leave
    /// alone" from "given empty, clear".
    ReportMetadata {
        #[arg(long)]
        source: String,
        #[arg(long)]
        sequence: u64,
        /// `NAME=VALUE` shown beside the agent; `NAME=` clears that token. Repeatable.
        #[arg(long = "token", value_parser = parse_metadata_token)]
        tokens: Vec<(String, Option<String>)>,
        /// Expire the tokens in this call after a delay. Untimed tokens persist.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=crate::agent::MAX_METADATA_TTL_MS))]
        ttl_ms: Option<u64>,
        /// Replace the displayed agent name; empty clears.
        #[arg(long)]
        display_agent: Option<String>,
        /// `STATUS=TEXT` renaming one status in the display; `STATUS=` clears. Repeatable.
        #[arg(long = "state-label", value_parser = parse_metadata_state_label)]
        state_labels: Vec<(crate::agent::AgentStatus, Option<String>)>,
        /// Replace the displayed pane title; empty clears.
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Clear one source's authoritative AI-agent report.
    ClearAgentReport {
        #[arg(long)]
        source: String,
        #[arg(long)]
        sequence: u64,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Explain why one pane shows the agent state it shows.
    ///
    /// Replays the live detection snapshot through the active manifest and reports which rule
    /// decided, with per-rule evidence. Use it when a pane's state looks wrong, or when writing
    /// rules for a new agent provider.
    AgentExplain {
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Report where this session's shape is persisted and when it was last written.
    Snapshot,
    /// Give the agent in one pane a name, or clear the name it has.
    ///
    /// A name is a stable target: pass `--alias NAME` to any other `msg` command instead of
    /// `--pane-id`, and it keeps working when the pane is split, moved, or renumbered. Names belong
    /// to the agent process, so one is cleared when its agent exits or is replaced.
    AgentRename {
        /// The pane holding the agent to name.
        #[arg(long)]
        pane_id: Option<u64>,
        /// The name to give it.
        #[arg(long, conflicts_with = "clear", required_unless_present = "clear")]
        name: Option<crate::agent::AgentAlias>,
        /// Remove the agent's current name.
        #[arg(long)]
        clear: bool,
    },
    /// Start a recognized agent in a pane that is sitting at a shell prompt.
    ///
    /// Types the agent's command at the pane's shell and returns once that same pane is detected
    /// running it. The pane must be an available shell: its own shell, at its prompt, with nothing
    /// else in the foreground. Arguments after `--` are passed to the agent verbatim.
    AgentStart {
        /// Agent kind, as reported by `list-panes` and `agent-explain` — for example `claude`.
        #[arg(long)]
        kind: crate::agent::AgentId,
        /// The pane to start the agent in. Required: a launch is never guessed at the focused pane.
        #[arg(long)]
        pane_id: u64,
        /// How long to wait for the agent to come up before giving up.
        #[arg(long, default_value = "30s", value_parser = parse_agent_start_timeout)]
        timeout: Duration,
        /// Arguments passed to the agent verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send text to a detected agent process and optionally wait for a status transition.
    ///
    /// This is the runtime-equivalent of typing a prompt and pressing Enter at the same
    /// pane. The Enter is intentionally delayed so the prompt does not remain embedded in the
    /// text on submit-heavy full-screen agents.
    AgentPrompt {
        /// The pane with the detected agent to receive the prompt text. Omit to use `--alias`.
        #[arg(long)]
        pane_id: Option<u64>,
        /// Text to send to the agent.
        text: String,
        /// Wait for an agent status change after submit.
        #[arg(long)]
        wait: bool,
        /// Stop waiting when the prompt results in any of these statuses.
        #[arg(long, value_enum)]
        until: Vec<crate::agent::AgentStatus>,
        /// How long to wait after submit, before reporting timeout/failure.
        #[arg(long, default_value = "30s", value_parser = parse_agent_prompt_timeout)]
        timeout: Duration,
    },
    /// Send one or more key strokes to one detected agent.
    AgentSendKeys {
        /// The pane with the detected agent. Omit to use `--alias`.
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long = "key", value_name = "KEY")]
        keys: Vec<String>,
    },
    /// Read an idle full-screen agent's application-owned scrollback.
    AgentRead {
        /// The pane with the detected agent. Omit to use `--alias`.
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u16).range(1..=1000))]
        lines: u16,
        #[arg(long)]
        json: bool,
    },
    /// Inspect one pane.
    Inspect(PaneTarget),
    /// Inspect sanitized Vivid media state owned by one pane.
    InspectMedia(PaneTarget),
    /// Read or follow the bounded media transition journal for one pane.
    TraceMedia {
        #[arg(long)]
        after: Option<u64>,
        #[arg(long, default_value_t = 128, value_parser = clap::value_parser!(u16).range(1..=i64::from(MAX_MEDIA_TRACE_QUERY_EVENTS)))]
        limit: u16,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        producer_id: Option<u64>,
        #[arg(long, requires = "producer_id")]
        context_id: Option<u64>,
        #[arg(long, requires = "context_id")]
        surface_id: Option<u64>,
        #[arg(long, requires = "surface_id")]
        track_id: Option<u64>,
        #[arg(long, value_enum)]
        category: Option<MediaTraceCategory>,
        #[arg(long)]
        recovery_only: bool,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Write the current tab and pane layout to a startup layout file.
    ///
    /// A bare name lands in the config directory as `<name>.toml`; a path is used as given.
    /// Defaults to the conventional `startup.toml`, and replaces an existing file.
    SaveLayout {
        #[arg(long)]
        path: Option<String>,
    },
    /// Split a tiled pane without changing the active tab.
    Split {
        #[arg(value_enum)]
        axis: SplitAxis,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Open a pane running one shell command.
    ///
    /// The command is passed to the shell with `-c`, so pipes and redirection work; quote it as
    /// one argument. Without `--hold` the pane closes when the command exits.
    Run {
        command: String,
        #[arg(long, value_enum, default_value_t = RunPlacementArg::Split)]
        placement: RunPlacementArg,
        /// Split direction, when `--placement split`.
        #[arg(long, value_enum, default_value_t = SplitAxis::Vertical)]
        axis: SplitAxis,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Keep the pane open after the command exits, so its output stays readable.
        #[arg(long)]
        hold: bool,
        /// Leave focus where it is instead of moving it to the new pane.
        #[arg(long)]
        no_focus: bool,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Activate and focus one pane.
    Focus {
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long, value_enum)]
        wait: Option<CompletionArg>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
    },
    /// Close one explicitly identified pane.
    ClosePane {
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Write literal UTF-8 bytes to a pane's PTY.
    Typing {
        text: String,
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long)]
        report: bool,
    },
    /// Encode and write a terminal key.
    Key {
        key: String,
        #[arg(long, value_delimiter = ',')]
        mods: Vec<String>,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=1000))]
        repeat: u16,
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long)]
        report: bool,
    },
    /// Paste text, honoring the pane's bracketed-paste mode.
    Paste {
        text: String,
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long)]
        report: bool,
    },
    /// Submit one line to a pane that is already running something.
    ///
    /// The text and its Enter reach the PTY as a single write, so a failed call cannot leave half
    /// a command sitting at the prompt. Use this instead of `typing` followed by `key Enter` in
    /// retry loops. To open a *new* pane for a command, use `run`.
    Submit {
        text: String,
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long)]
        report: bool,
    },
    /// Print pane text exactly, without a trailing newline.
    ///
    /// Without `--source`, `--rows N` reads the last N rows with soft wraps joined and no `--rows`
    /// reads the current viewport — the long-standing behavior. `--source detection` prints JSON
    /// instead of text, since it carries the OSC fields alongside the snapshot.
    GetText {
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=1000))]
        rows: Option<u16>,
        #[arg(long, value_enum)]
        source: Option<crate::ipc::TextSource>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Print a structured pane grid snapshot or viewport delta.
    GetGrid {
        #[arg(
            long,
            requires = "row_count",
            conflicts_with = "since_screen",
            allow_hyphen_values = true
        )]
        start_line: Option<isize>,
        #[arg(long, requires = "start_line", conflicts_with = "since_screen", value_parser = clap::value_parser!(u16).range(1..=1000))]
        row_count: Option<u16>,
        #[arg(long)]
        since_screen: Option<u64>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Search physical terminal rows and print structured matches.
    Search {
        #[arg(long)]
        pattern: String,
        #[arg(long)]
        regex: bool,
        #[arg(long, value_enum, default_value_t = SearchDirectionArg::Forward)]
        direction: SearchDirectionArg,
        #[arg(long, allow_hyphen_values = true)]
        start_line: Option<isize>,
        #[arg(long)]
        start_column: Option<usize>,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Enable or disable synchronized input for the target pane's tab.
    SyncInput {
        #[arg(long, conflicts_with = "off", required_unless_present = "off")]
        on: bool,
        #[arg(long, conflicts_with = "on", required_unless_present = "on")]
        off: bool,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Stream session events as NDJSON until interrupted.
    ///
    /// Event-driven alternative to polling: react to `agent.status_changed` or pane lifecycle
    /// instead of re-reading `get-text`. A `gap` record reports events that were dropped and is
    /// never filtered out, so a filtered stream still tells you when it missed something.
    Subscribe {
        /// Replay retained events after this sequence before streaming live ones.
        #[arg(long)]
        after: Option<u64>,
        /// Only stream these event names. Repeatable; unset streams every event.
        #[arg(long = "name")]
        names: Vec<String>,
        /// Only stream events belonging to this pane.
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for pane or render state.
    Wait {
        #[command(subcommand)]
        command: WaitCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum WaitCommand {
    /// Wait until visible pane text contains a literal or regular expression.
    Text {
        text: String,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for a pattern in a pane's output stream, which a screen wait can miss.
    Output {
        pattern: String,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        after_offset: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for a newer pane screen sequence.
    ScreenChange {
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait until a pane screen stays unchanged for a quiet period.
    ScreenStable {
        #[arg(long, default_value = "200ms", value_parser = parse_timeout)]
        quiet: Duration,
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for the attached client to acknowledge a composite render.
    Rendered {
        #[arg(long)]
        after_session: u64,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
    },
    /// Wait until a pane's agent reaches one of the given lifecycle states.
    ///
    /// The orchestration primitive: submit work, then block until the agent needs you or is
    /// finished, instead of polling `get-text`. `done` means the agent finished while you were not
    /// looking at it; focusing the pane in the navigator acknowledges it back to `idle`.
    AgentState {
        #[arg(long, value_enum, value_delimiter = ',', required = true)]
        until: Vec<crate::agent::AgentStatus>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for a pane process to exit.
    Exit {
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for a newer virtual or outer media projection revision.
    Media {
        #[arg(long)]
        after_virtual: Option<u64>,
        #[arg(long)]
        after_outer: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for a named state on one completely identified virtual media track.
    MediaTrack {
        #[arg(value_enum)]
        condition: MediaTrackConditionArg,
        #[arg(long)]
        producer_id: u64,
        #[arg(long)]
        context_id: u64,
        #[arg(long)]
        surface_id: u64,
        #[arg(long)]
        track_id: u64,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
}

/// Everything a `msg` invocation says about a request other than which verb it is.
///
/// Gathered into one struct because these are all global flags: which pane a request addresses and
/// what it assumes about the session are properties of the request, not of the verb.
#[derive(Debug, Default)]
pub struct RequestOptions {
    pub alias: Option<crate::agent::AgentAlias>,
    pub pane_name: Option<crate::layout::PaneName>,
    pub expect: crate::ipc::ExpectedState,
    pub idempotency_key: Option<String>,
}

pub fn run(
    explicit_target: Option<&str>,
    options: RequestOptions,
    command: MsgCommand,
) -> io::Result<()> {
    let RequestOptions {
        alias,
        pane_name,
        expect,
        idempotency_key,
    } = options;
    let target = explicit_target
        .map(ToOwned::to_owned)
        .or_else(|| {
            env::var("VVMUX_SESSION")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "default".into());
    crate::runtime::validate_session_name(&target)?;
    let inherited_pane = (env::var("VVMUX_SESSION").ok().as_deref() == Some(target.as_str()))
        .then(inherited_pane_from_environment)
        .flatten();
    // Handled before the single-request path: a plan is many requests over one connection, and
    // has no single method to build.
    if let MsgCommand::RunPlan {
        file,
        dry_run,
        preflight,
    } = command
    {
        return crate::plan::run(
            &target,
            file.as_deref(),
            crate::plan::PlanOptions { dry_run, preflight },
        );
    }
    let (method, explicit_pane, allow_focused, output) = build_request(command)?;
    for (first, second, message) in [
        (
            alias.is_some(),
            explicit_pane.is_some(),
            "--alias and --pane-id name different panes; pass one",
        ),
        (
            pane_name.is_some(),
            explicit_pane.is_some(),
            "--pane-name and --pane-id name different panes; pass one",
        ),
        (
            alias.is_some(),
            pane_name.is_some(),
            "--alias and --pane-name name different panes; pass one",
        ),
    ] {
        if first && second {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
        }
    }
    // An alias overrides an inherited pane, but an explicit `--pane-id` still wins over both. A
    // caller inside a pane who names an agent means that agent, not the pane they happen to be in.
    let pane_id = explicit_pane.or_else(|| {
        (alias.is_none() && pane_name.is_none())
            .then_some(inherited_pane)
            .flatten()
    });
    if matches!(
        &method,
        AutomationMethod::ClosePane
            | AutomationMethod::ReportAgent { .. }
            | AutomationMethod::ReportAgentSession { .. }
            | AutomationMethod::ClearAgentReport { .. }
            | AutomationMethod::ReportMetadata { .. }
    ) && pane_id.is_none()
        && alias.is_none()
        && pane_name.is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} requires --pane-id, --pane-name, --alias, or a same-session VVMUX_PANE_ID",
                method.name()
            ),
        ));
    }
    // `agent-start` needs no case of its own: it needs a pane with no agent in it, an alias only
    // ever names an agent already running, and its `--pane-id` is required — so passing `--alias`
    // to it is already refused above as the contradiction it is.
    let request = AutomationRequest {
        id: 1,
        pane_id,
        agent: alias,
        pane_name,
        allow_focused,
        expect: (!expect.is_empty()).then_some(expect),
        idempotency_key,
        method,
    };
    let (mut reader, writer) = crate::server::connect(&target)?;
    if matches!(output, Output::TraceFollow) {
        return run_trace_follow(&mut reader, &writer, request);
    }
    if matches!(output, Output::EventStream) {
        let id = request.id;
        send_request(&writer, request)?;
        return stream_events(&mut reader, id);
    }
    send_request(&writer, request.clone())?;
    let result = response_result(receive_response(&mut reader, request.id)?)?;
    match output {
        Output::Silent => Ok(()),
        Output::Text => {
            let text = result.as_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "get-text returned non-text data",
                )
            })?;
            io::stdout().write_all(text.as_bytes())
        }
        Output::Json => {
            serde_json::to_writer(io::stdout().lock(), &result).map_err(io::Error::other)?;
            println!();
            Ok(())
        }
        Output::TraceFollow | Output::EventStream => unreachable!(),
    }
}

/// Print an event subscription's records as NDJSON until the connection ends.
///
/// Shared by `msg subscribe` and `plugin events` so the two cannot drift in how they frame
/// records or report a failed subscribe.
pub(crate) fn stream_events(reader: &mut crate::ipc::RecordReader, id: u64) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut subscribed = false;
    loop {
        match reader.recv_server()? {
            ServerMessage::Automation(response) if response.id == id => {
                response_result(response)?;
                subscribed = true;
            }
            ServerMessage::PluginEvent { envelope, .. } if subscribed => {
                serde_json::to_writer(&mut stdout, &envelope).map_err(io::Error::other)?;
                writeln!(stdout)?;
                // A subscriber is usually a pipe, where stdout is block-buffered; without this an
                // event-driven consumer would wait for a full buffer rather than for an event.
                stdout.flush()?;
            }
            _ => {}
        }
    }
}

/// Execute one structured automation request without printing it.
pub(crate) fn request_json(
    target: &str,
    method: AutomationMethod,
    pane_id: Option<u64>,
    allow_focused: bool,
) -> io::Result<serde_json::Value> {
    crate::runtime::validate_session_name(target)?;
    let request = AutomationRequest {
        id: 1,
        pane_id,
        agent: None,
        pane_name: None,
        expect: None,
        idempotency_key: None,
        allow_focused,
        method,
    };
    let (mut reader, writer) = crate::server::connect(target)?;
    send_request(&writer, request.clone())?;
    response_result(receive_response(&mut reader, request.id)?)
}

pub(crate) fn send_request(
    writer: &crate::ipc::SharedWriter,
    request: AutomationRequest,
) -> io::Result<()> {
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(&ClientMessage::Automation(request))
}

pub(crate) fn response_result(response: AutomationResponse) -> io::Result<serde_json::Value> {
    if !response.ok {
        let error = response
            .error
            .map(|error| format!("{}: {}", error.code, error.message))
            .unwrap_or_else(|| "automation request failed".into());
        return Err(io::Error::other(error));
    }
    Ok(response.result.unwrap_or(serde_json::Value::Null))
}

fn run_trace_follow(
    reader: &mut crate::ipc::RecordReader,
    writer: &crate::ipc::SharedWriter,
    mut request: AutomationRequest,
) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    loop {
        send_request(writer, request.clone())?;
        let result = response_result(receive_response(reader, request.id)?)?;
        let batch: MediaTraceBatch = serde_json::from_value(result).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid media trace response: {error}"),
            )
        })?;
        if let Some(gap) = &batch.gap {
            serde_json::to_writer(&mut stdout, &serde_json::json!({"type": "gap", "gap": gap}))
                .map_err(io::Error::other)?;
            writeln!(stdout)?;
        }
        for event in &batch.events {
            serde_json::to_writer(&mut stdout, event).map_err(io::Error::other)?;
            writeln!(stdout)?;
        }
        stdout.flush()?;
        let requested_after = match &request.method {
            AutomationMethod::TraceMedia { after_sequence, .. } => *after_sequence,
            _ => unreachable!(),
        };
        let after = batch.events.last().map_or_else(
            || {
                requested_after
                    .unwrap_or(batch.current_sequence)
                    .max(batch.current_sequence)
            },
            |event| event.sequence,
        );
        let AutomationMethod::TraceMedia { after_sequence, .. } = &mut request.method else {
            unreachable!();
        };
        *after_sequence = Some(after);
        request.id = request
            .id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("automation request ID exhausted"))?;
    }
}

#[derive(Clone, Copy)]
enum Output {
    Silent,
    Text,
    Json,
    TraceFollow,
    EventStream,
}

/// A `COLUMN,ROW` gesture point.
fn parse_mouse_point(value: &str) -> Result<(u16, u16), String> {
    let (column, row) = value
        .split_once(',')
        .ok_or_else(|| format!("expected COLUMN,ROW, got `{value}`"))?;
    Ok((
        column
            .trim()
            .parse()
            .map_err(|_| format!("invalid column in `{value}`"))?,
        row.trim()
            .parse()
            .map_err(|_| format!("invalid row in `{value}`"))?,
    ))
}

/// Exactly one coordinate form, converted to the wire's pane-local position.
fn mouse_position(
    cell_column: Option<u16>,
    cell_row: Option<u16>,
    x: Option<u32>,
    y: Option<u32>,
    relative_x: Option<f32>,
    relative_y: Option<f32>,
) -> io::Result<Option<crate::ipc::MousePosition>> {
    use crate::ipc::MousePosition;
    if let (Some(column), Some(row)) = (cell_column, cell_row) {
        return Ok(Some(MousePosition::Cell { column, row }));
    }
    if let (Some(x), Some(y)) = (x, y) {
        return Ok(Some(MousePosition::Pixel { x, y }));
    }
    if let (Some(x), Some(y)) = (relative_x, relative_y) {
        // Per-mille on the wire; a fraction is what a person types.
        for value in [x, y] {
            if !(0.0..=1.0).contains(&value) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "relative coordinates run from 0.0 through 1.0",
                ));
            }
        }
        return Ok(Some(MousePosition::Relative {
            x: (x * 1000.0).round() as u16,
            y: (y * 1000.0).round() as u16,
        }));
    }
    Ok(None)
}

/// `Shift`, `Alt`, `Ctrl` in any case, as the three flags the wire carries.
fn mouse_modifiers(mods: &[String]) -> io::Result<(bool, bool, bool)> {
    let (mut shift, mut alt, mut ctrl) = (false, false, false);
    for modifier in mods {
        match modifier.trim().to_ascii_lowercase().as_str() {
            "shift" => shift = true,
            "alt" | "meta" => alt = true,
            "ctrl" | "control" => ctrl = true,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown modifier `{other}`; use Shift, Alt, or Ctrl"),
                ));
            }
        }
    }
    Ok((shift, alt, ctrl))
}

fn build_request(command: MsgCommand) -> io::Result<(AutomationMethod, Option<u64>, bool, Output)> {
    let tuple = match command {
        MsgCommand::Capabilities => (AutomationMethod::Capabilities, None, false, Output::Json),
        // Routed before `build_request` is reached; it is many requests, not one.
        MsgCommand::RunPlan { .. } => unreachable!("run-plan does not build a single request"),
        MsgCommand::ReloadConfig => (AutomationMethod::ReloadConfig, None, false, Output::Json),
        MsgCommand::GetConfig => (AutomationMethod::GetConfig, None, false, Output::Json),
        MsgCommand::ReloadPlugins => (
            AutomationMethod::Plugin(PluginMethod::Reload),
            None,
            false,
            Output::Json,
        ),
        MsgCommand::Action { action, pane_id } => (
            AutomationMethod::Action(action.into()),
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::ListPanes => (AutomationMethod::ListPanes, None, false, Output::Json),
        MsgCommand::SessionInspect => (AutomationMethod::SessionInspect, None, false, Output::Json),
        MsgCommand::ListTabs => (AutomationMethod::ListTabs, None, false, Output::Json),
        MsgCommand::SelectTab { tab, wait, timeout } => (
            AutomationMethod::SelectTab {
                tab: tab.selector(),
                wait: wait.map(Into::into),
                timeout_ms: millis(timeout),
            },
            None,
            false,
            Output::Json,
        ),
        MsgCommand::Layout => (AutomationMethod::Layout, None, false, Output::Json),
        MsgCommand::ResolvePane { path, pane_id, tab } => (
            AutomationMethod::ResolvePane {
                // Absent rather than `Active` when nothing was named: the server then starts the
                // route in the caller's own tab, which is what an agent asking for "the pane to my
                // left" means even when it is looking at a tab that is not selected.
                tab: (tab.tab_id.is_some() || tab.tab_name.is_some()).then(|| tab.selector()),
                path: path.into_iter().map(Into::into).collect(),
            },
            pane_id,
            false,
            Output::Json,
        ),
        MsgCommand::PaneRename {
            name,
            clear: _,
            pane_id,
        } => (
            AutomationMethod::PaneRename { name },
            pane_id,
            false,
            Output::Json,
        ),
        MsgCommand::Mouse {
            action,
            cell_column,
            cell_row,
            x,
            y,
            relative_x,
            relative_y,
            points,
            button,
            route,
            mods,
            scroll,
            pane_id,
        } => {
            let (shift, alt, ctrl) = mouse_modifiers(&mods)?;
            (
                AutomationMethod::Mouse {
                    action,
                    position: mouse_position(cell_column, cell_row, x, y, relative_x, relative_y)?,
                    button,
                    route,
                    shift,
                    alt,
                    ctrl,
                    scroll,
                    points: points
                        .into_iter()
                        .map(|(column, row)| crate::ipc::MousePosition::Cell { column, row })
                        .collect(),
                    duration_ms: None,
                    wait_rendered: false,
                    timeout_ms: 30_000,
                },
                pane_id,
                true,
                Output::Json,
            )
        }
        MsgCommand::ShellCommand {
            command,
            timeout,
            pane_id,
        } => (
            AutomationMethod::ShellCommand {
                command,
                timeout_ms: millis(timeout),
            },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::Capture {
            no_activate,
            after_screen,
            stable,
            rendered,
            grid,
            timeout,
            pane_id,
        } => (
            AutomationMethod::Capture {
                no_activate,
                after_screen,
                stable_ms: stable.map(millis),
                rendered,
                grid,
                timeout_ms: millis(timeout),
            },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::Transcript {
            after_offset,
            base64,
            max_bytes,
            pane_id,
        } => (
            AutomationMethod::Transcript {
                after_offset,
                base64,
                max_bytes,
            },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::ResizePane {
            columns,
            rows,
            pane_id,
        } => (
            AutomationMethod::ResizePane { columns, rows },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::MovePane {
            to_tab,
            to_tab_name,
            swap,
            to_layer,
            pane_id,
        } => (
            AutomationMethod::MovePane {
                to_tab: to_tab
                    .map(TabSelector::Id)
                    .or_else(|| to_tab_name.map(TabSelector::Name)),
                swap: swap.map(Into::into),
                to_layer,
            },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::SetFlag {
            flag,
            on,
            off,
            offset,
            pane_id,
        } => {
            if on == off {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "set-flag needs exactly one of --on or --off",
                ));
            }
            (
                AutomationMethod::SetFlag {
                    flag,
                    enabled: on,
                    offset,
                },
                pane_id,
                true,
                Output::Json,
            )
        }
        MsgCommand::Signal { signal, pane_id } => (
            AutomationMethod::Signal { signal },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::ActivatePane(target) => (
            AutomationMethod::ActivatePane,
            target.pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::NewTab { name } => {
            (AutomationMethod::NewTab { name }, None, false, Output::Json)
        }
        MsgCommand::RenameTab { name, tab } => (
            AutomationMethod::RenameTab {
                tab: tab.selector(),
                name,
            },
            None,
            false,
            Output::Json,
        ),
        MsgCommand::ResetTabTitle { tab } => (
            AutomationMethod::ResetTabTitle {
                tab: tab.selector(),
            },
            None,
            false,
            Output::Json,
        ),
        MsgCommand::CloseTab { tab } => (
            AutomationMethod::CloseTab {
                tab: tab.selector(),
            },
            None,
            false,
            Output::Json,
        ),
        MsgCommand::Diagnose {
            pane_id,
            all_panes,
            trace_limit,
        } => (
            AutomationMethod::Diagnose {
                pane_id,
                all_panes,
                trace_limit,
            },
            None,
            false,
            Output::Json,
        ),
        MsgCommand::ReportAgent {
            agent,
            state,
            source,
            sequence,
            message,
            session_id,
            session_path,
            pane_id,
        } => (
            AutomationMethod::ReportAgent {
                agent,
                state,
                source,
                sequence,
                message,
                session_id,
                session_path,
            },
            pane_id,
            false,
            Output::Json,
        ),
        MsgCommand::ReportAgentSession {
            agent,
            source,
            sequence,
            session_id,
            session_path,
            pane_id,
        } => (
            AutomationMethod::ReportAgentSession {
                agent,
                source,
                sequence,
                session_id,
                session_path,
            },
            pane_id,
            false,
            Output::Json,
        ),
        MsgCommand::ReportMetadata {
            source,
            sequence,
            tokens,
            ttl_ms,
            display_agent,
            state_labels,
            title,
            pane_id,
        } => (
            AutomationMethod::ReportMetadata {
                source,
                sequence,
                tokens,
                ttl_ms,
                // clap gives `None` when the option was not passed and `Some("")` when it was
                // passed empty, which is exactly the leave-alone/clear distinction the server
                // expects.
                display_agent: display_agent.map(|value| (!value.is_empty()).then_some(value)),
                state_labels,
                title: title.map(|value| (!value.is_empty()).then_some(value)),
            },
            pane_id,
            false,
            Output::Json,
        ),
        MsgCommand::ClearAgentReport {
            source,
            sequence,
            pane_id,
        } => (
            AutomationMethod::ClearAgentReport { source, sequence },
            pane_id,
            false,
            Output::Json,
        ),
        MsgCommand::Snapshot => (AutomationMethod::SessionSnapshot, None, false, Output::Json),
        MsgCommand::AgentExplain { pane_id } => {
            (AutomationMethod::AgentExplain, pane_id, true, Output::Json)
        }
        MsgCommand::AgentRename {
            pane_id,
            name,
            clear,
        } => (
            // `--clear` and `--name` are mutually exclusive at the parser, so `clear` set means
            // `name` is absent, and the absent alias *is* the clear.
            AutomationMethod::AgentRename {
                alias: if clear { None } else { name },
            },
            pane_id,
            false,
            Output::Json,
        ),
        MsgCommand::AgentStart {
            kind,
            pane_id,
            timeout,
            args,
        } => (
            AutomationMethod::AgentStart {
                agent: kind,
                args,
                timeout_ms: millis(timeout),
            },
            Some(pane_id),
            false,
            Output::Json,
        ),
        MsgCommand::AgentPrompt {
            pane_id,
            text,
            wait,
            until,
            timeout,
        } => {
            validate_input(&text)?;
            if !wait && !until.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "agent-prompt --until requires --wait",
                ));
            }
            let until = if wait && until.is_empty() {
                vec![
                    crate::agent::AgentStatus::Idle,
                    crate::agent::AgentStatus::Blocked,
                    crate::agent::AgentStatus::Done,
                ]
            } else {
                until
            };
            (
                AutomationMethod::AgentPrompt {
                    text,
                    wait,
                    until,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            )
        }
        MsgCommand::AgentSendKeys { pane_id, keys } => (
            AutomationMethod::AgentSendKeys { keys },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::AgentRead {
            pane_id,
            lines,
            json,
        } => (
            AutomationMethod::AgentRead { lines, json },
            pane_id,
            false,
            if json { Output::Json } else { Output::Text },
        ),
        MsgCommand::Inspect(target) => (
            AutomationMethod::Inspect,
            target.pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::InspectMedia(target) => (
            AutomationMethod::InspectMedia,
            target.pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::TraceMedia {
            after,
            limit,
            timeout,
            follow,
            producer_id,
            context_id,
            surface_id,
            track_id,
            category,
            recovery_only,
            pane_id,
        } => (
            AutomationMethod::TraceMedia {
                after_sequence: after,
                limit,
                timeout_ms: if follow { millis(timeout) } else { 0 },
                filter: MediaTraceFilter {
                    producer_id,
                    context_id,
                    surface_id,
                    track_id,
                    category,
                    recovery_only,
                },
            },
            pane_id,
            true,
            if follow {
                Output::TraceFollow
            } else {
                Output::Json
            },
        ),
        MsgCommand::Split { axis, pane_id } => (
            AutomationMethod::Split { axis: axis.into() },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::SaveLayout { path } => (
            AutomationMethod::SaveLayout { path },
            None,
            false,
            Output::Json,
        ),
        MsgCommand::Run {
            command,
            placement,
            axis,
            cwd,
            hold,
            no_focus,
            pane_id,
        } => {
            let cwd = match cwd {
                // Resolve here rather than in the session: the caller's shell is what a relative
                // path is relative to, and the daemon's working directory is not the caller's.
                Some(path) => Some(
                    std::fs::canonicalize(&path)
                        .map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("--cwd {}: {error}", path.display()),
                            )
                        })?
                        .to_string_lossy()
                        .into_owned(),
                ),
                None => None,
            };
            (
                AutomationMethod::Run {
                    command,
                    placement: match placement {
                        RunPlacementArg::Split => RunPlacement::Split { axis: axis.into() },
                        RunPlacementArg::Float => RunPlacement::Float,
                        RunPlacementArg::Tab => RunPlacement::Tab,
                    },
                    cwd,
                    hold,
                    focus: !no_focus,
                },
                pane_id,
                true,
                Output::Json,
            )
        }
        MsgCommand::Focus {
            pane_id,
            wait,
            timeout,
        } => (
            wait.map_or(AutomationMethod::Focus, |wait| {
                AutomationMethod::FocusWait {
                    wait: wait.into(),
                    timeout_ms: millis(timeout),
                }
            }),
            pane_id,
            true,
            if wait.is_some() {
                Output::Json
            } else {
                Output::Silent
            },
        ),
        MsgCommand::ClosePane { pane_id } => {
            (AutomationMethod::ClosePane, pane_id, false, Output::Silent)
        }
        MsgCommand::Typing {
            text,
            pane_id,
            report,
        } => {
            validate_input(&text)?;
            (
                AutomationMethod::Typing { text, report },
                pane_id,
                true,
                if report { Output::Json } else { Output::Silent },
            )
        }
        MsgCommand::Key {
            key,
            mods,
            repeat,
            pane_id,
            report,
        } => (
            AutomationMethod::Key {
                key,
                modifiers: mods,
                repeat,
                report,
            },
            pane_id,
            true,
            if report { Output::Json } else { Output::Silent },
        ),
        MsgCommand::Paste {
            text,
            pane_id,
            report,
        } => {
            validate_input(&text)?;
            (
                AutomationMethod::Paste { text, report },
                pane_id,
                true,
                if report { Output::Json } else { Output::Silent },
            )
        }
        MsgCommand::Submit {
            text,
            pane_id,
            report,
        } => {
            validate_input(&text)?;
            // "Submit one line" must not be ambiguous about how many commands ran.
            if text.contains(['\n', '\r']) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "submit takes one line; use paste for multi-line input",
                ));
            }
            (
                AutomationMethod::SubmitLine { text, report },
                pane_id,
                true,
                if report { Output::Json } else { Output::Silent },
            )
        }
        MsgCommand::GetText {
            rows,
            source,
            pane_id,
        } => {
            // Preserve the historical default exactly: `--rows N` has always meant "the last N
            // rows with wraps joined", and its absence "the current viewport".
            let source = source.unwrap_or(if rows.is_some() {
                TextSource::RecentUnwrapped
            } else {
                TextSource::Visible
            });
            if rows.is_some() && matches!(source, TextSource::Visible | TextSource::Detection) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--rows applies only to --source recent or recent-unwrapped",
                ));
            }
            (
                AutomationMethod::GetText { rows, source },
                pane_id,
                true,
                // Detection carries OSC fields beside the snapshot, so it cannot be bare text.
                if matches!(source, TextSource::Detection) {
                    Output::Json
                } else {
                    Output::Text
                },
            )
        }
        MsgCommand::GetGrid {
            start_line,
            row_count,
            since_screen,
            pane_id,
        } => (
            AutomationMethod::GetGrid {
                start_line,
                row_count,
                since_screen,
            },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::Search {
            pattern,
            regex,
            direction,
            start_line,
            start_column,
            limit,
            pane_id,
        } => (
            AutomationMethod::Search {
                pattern,
                regex,
                direction: direction.into(),
                start_line,
                start_column,
                limit,
            },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::SyncInput {
            on,
            off: _,
            pane_id,
        } => (
            AutomationMethod::SetSyncInput { enabled: on },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::Subscribe {
            after,
            names,
            pane_id,
        } => {
            if names.len() > crate::ipc::EventFilter::MAX_NAMES
                || names.iter().any(|name| {
                    name.is_empty() || name.len() > crate::ipc::EventFilter::MAX_NAME_BYTES
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "at most 16 event names, each 1..=64 bytes",
                ));
            }
            (
                AutomationMethod::Subscribe {
                    after_sequence: after,
                    filter: crate::ipc::EventFilter { names, pane_id },
                },
                // The pane filter narrows the stream; it is not the request's pane target, and a
                // subscription is session-wide.
                None,
                false,
                Output::EventStream,
            )
        }
        MsgCommand::Wait { command } => match command {
            WaitCommand::Output {
                pattern,
                regex,
                after_offset,
                timeout,
                pane_id,
            } => {
                if regex && pattern.len() > 8 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "regular expression exceeds 8 KiB",
                    ));
                }
                (
                    AutomationMethod::WaitOutput {
                        pattern,
                        regex,
                        after_offset,
                        timeout_ms: millis(timeout),
                    },
                    pane_id,
                    true,
                    Output::Json,
                )
            }
            WaitCommand::Text {
                text,
                regex,
                after_screen,
                timeout,
                pane_id,
            } => {
                if regex && text.len() > 8 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "regular expression exceeds 8 KiB",
                    ));
                }
                (
                    AutomationMethod::WaitText {
                        text,
                        regex,
                        after_screen,
                        timeout_ms: millis(timeout),
                    },
                    pane_id,
                    true,
                    Output::Json,
                )
            }
            WaitCommand::ScreenChange {
                after_screen,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitScreenChange {
                    after_screen,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::ScreenStable {
                quiet,
                after_screen,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitScreenStable {
                    quiet_ms: millis(quiet),
                    after_screen,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::Rendered {
                after_session,
                timeout,
            } => (
                AutomationMethod::WaitRendered {
                    after_session,
                    timeout_ms: millis(timeout),
                },
                None,
                false,
                Output::Json,
            ),
            WaitCommand::AgentState {
                until,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitAgentState {
                    until,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::Exit { timeout, pane_id } => (
                AutomationMethod::WaitExit {
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::Media {
                after_virtual,
                after_outer,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitMedia {
                    after_virtual_revision: after_virtual,
                    after_outer_revision: after_outer,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::MediaTrack {
                condition,
                producer_id,
                context_id,
                surface_id,
                track_id,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitMediaTrack {
                    identity: MediaTrackIdentity {
                        producer_id,
                        context_id,
                        surface_id,
                        track_id,
                    },
                    condition: condition.into(),
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
        },
    };
    Ok(tuple)
}

pub(crate) fn receive_response(
    reader: &mut crate::ipc::RecordReader,
    id: u64,
) -> io::Result<AutomationResponse> {
    let mut chunks = Vec::new();
    let mut next_index = 0;
    loop {
        match reader.recv_server()? {
            ServerMessage::Automation(response) if response.id == id => return Ok(response),
            ServerMessage::AutomationChunk {
                request_id,
                index,
                last,
                base64,
            } if request_id == id => {
                if index != next_index {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "automation response chunk sequence gap",
                    ));
                }
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(base64)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid response base64")
                    })?;
                if chunks.len().saturating_add(decoded.len()) > 16 * 1024 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "automation response exceeds 16 MiB",
                    ));
                }
                chunks.extend_from_slice(&decoded);
                next_index += 1;
                if last {
                    return serde_json::from_slice(&chunks).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid automation response: {error}"),
                        )
                    });
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected VVMX message on automation connection",
                ));
            }
        }
    }
}

fn validate_input(text: &str) -> io::Result<()> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input exceeds 1 MiB",
        ));
    }
    Ok(())
}

fn inherited_pane_from_environment() -> Option<u64> {
    env::var("VVMUX_PANE_ID").ok()?.parse().ok()
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// `NAME=VALUE`, where an empty value clears the token.
fn parse_metadata_token(value: &str) -> Result<(String, Option<String>), String> {
    let (name, value) = value
        .split_once('=')
        .ok_or_else(|| "expected NAME=VALUE".to_string())?;
    Ok((
        name.to_owned(),
        (!value.is_empty()).then(|| value.to_owned()),
    ))
}

/// `STATUS=TEXT`, where an empty text clears the override.
fn parse_metadata_state_label(
    value: &str,
) -> Result<(crate::agent::AgentStatus, Option<String>), String> {
    let (status, label) = value
        .split_once('=')
        .ok_or_else(|| "expected STATUS=TEXT".to_string())?;
    let status = status
        .parse::<crate::agent::AgentStatus>()
        .map_err(str::to_owned)?;
    Ok((status, (!label.is_empty()).then(|| label.to_owned())))
}

/// Parse an `agent-start` readiness timeout.
///
/// Narrower than the general wait bounds at both ends. The floor is the settle delay: a shorter
/// timeout could only ever expire before detection is allowed to conclude anything, so it would
/// report a launch failure for every launch. The ceiling keeps a mistyped unit from parking a
/// request for a day.
fn parse_agent_start_timeout(value: &str) -> Result<Duration, String> {
    let timeout = parse_timeout(value)?;
    if !(crate::agent_drive::AGENT_START_MIN_TIMEOUT..=crate::agent_drive::AGENT_START_MAX_TIMEOUT)
        .contains(&timeout)
    {
        return Err("agent start timeout must be from 3s through 300s".into());
    }
    Ok(timeout)
}

fn parse_agent_prompt_timeout(value: &str) -> Result<Duration, String> {
    parse_agent_start_timeout(value)
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else {
        (value, 1)
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration {value:?}"))?;
    let millis = number
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".to_string())?;
    if !(1..=MAX_TIMEOUT_MS).contains(&millis) {
        return Err("duration must be from 1ms through 24h".into());
    }
    Ok(Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_bounds_and_units() {
        assert_eq!(parse_timeout("1ms").unwrap(), Duration::from_millis(1));
        assert_eq!(parse_timeout("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("24h").unwrap(), Duration::from_secs(86_400));
        assert!(parse_timeout("0").is_err());
        assert!(parse_timeout("25h").is_err());
    }

    #[test]
    fn trace_follow_uses_bounded_long_polling_while_snapshot_is_immediate() {
        let command = |follow| MsgCommand::TraceMedia {
            after: Some(9),
            limit: 32,
            timeout: Duration::from_secs(2),
            follow,
            producer_id: Some(3),
            context_id: Some(5),
            surface_id: Some(7),
            track_id: None,
            category: Some(MediaTraceCategory::Recovery),
            recovery_only: true,
            pane_id: Some(2),
        };
        let (snapshot, _, _, output) = build_request(command(false)).unwrap();
        assert!(matches!(output, Output::Json));
        assert!(matches!(
            snapshot,
            AutomationMethod::TraceMedia {
                after_sequence: Some(9),
                limit: 32,
                timeout_ms: 0,
                ..
            }
        ));
        let (follow, _, _, output) = build_request(command(true)).unwrap();
        assert!(matches!(output, Output::TraceFollow));
        assert!(matches!(
            follow,
            AutomationMethod::TraceMedia {
                timeout_ms: 2_000,
                ..
            }
        ));
    }
}
