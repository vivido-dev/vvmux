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
/// Version 19 added the per-pane transparency action. `Action` rides `ClientMessage`, and an
/// unknown variant name is a decode failure rather than a negotiation, so a new client against a
/// surviving old server tore the connection down mid-session instead of saying why.
/// Version 18 reports recreated retained outer sources so their bodies can be rehydrated.
/// Version 17 added actor-owned tab-navigation, rename, and close-confirmation actions.
/// Version 16 was the hard cutover for deterministic automation waits and correlated diagnostics.
/// Agent IDs remain strings on this wire, so replacing the closed Rust enum with validated plugin
/// IDs does not change its encoding.
///
/// A mixed pair is rejected by [`VERSION_MISMATCH`] rather than negotiated down: the two encodings
/// differ in client-message framing, so accepting an older peer would misdecode bridge state.
pub const VERSION: u16 = 19;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutomationRequest {
    pub id: u64,
    pub pane_id: Option<u64>,
    pub allow_focused: bool,
    pub method: AutomationMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AutomationMethod {
    Capabilities,
    ListPanes,
    SessionInspect,
    ListTabs,
    SelectTab {
        tab_id: u64,
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
    },
    ClearAgentReport {
        source: String,
        sequence: u64,
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
    GetText {
        rows: Option<u16>,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomationCompletion {
    Outer,
    Rendered,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaTrackIdentity {
    pub producer_id: u64,
    pub context_id: u64,
    pub surface_id: u64,
    pub track_id: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

/// Where a `run` pane goes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunPlacement {
    /// Split the target pane, as `msg split` would.
    Split { axis: Axis },
    /// A floating pane over the target's tab.
    Float,
    /// A new tab of its own.
    Tab,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMessage {
    Attach {
        replace: bool,
        display: DisplayMetrics,
        vivid: bool,
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
    Clipboard(String),
    Status(String),
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
