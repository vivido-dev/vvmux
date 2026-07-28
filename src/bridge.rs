use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::cbor::PreservedField;
use vivid_protocol::media;
use vivid_protocol::messages::{
    self, AudioSourceConfig, ClipRect, HelloConfig, ImageSourceConfig, RasterSourceConfig,
    SceneNodeConfig, VideoSourceConfig,
};
use vivid_protocol::revision::{SceneRevision, SourceRevision};
use vivid_protocol::wire::{Connection, ConnectionKind, ConnectionWriter, Endpoint, Record};
use vivid_protocol::{VIVID_MAJOR, VIVID_MINOR};
use zeroize::Zeroizing;

use crate::ipc::{
    BridgeKeyframeRequest, BridgeNode, BridgeSource, BridgeSourceDescriptor, BridgeSourceKey,
    BridgeSourceKind, DisplayMetrics,
};

const REQUIRED_FEATURES: &[u64] = &[
    messages::FEATURE_RASTER_RGBA8,
    messages::FEATURE_SCENE_TRANSACTIONS,
    messages::FEATURE_GRID_CELL_NODES,
    messages::FEATURE_CREDIT_FLOW_CONTROL,
    messages::FEATURE_NODE_CLIP_RECT_V1,
];
const OPTIONAL_FEATURES: &[u64] = &[
    messages::FEATURE_ENCODED_IMAGE_V1,
    messages::FEATURE_RASTER_ZSTD_V1,
    messages::FEATURE_RASTER_PREMULTIPLIED_ALPHA,
    messages::FEATURE_VIDEO_ACCESS_UNIT_V1,
    messages::FEATURE_VIDEO_CONTROL_V1,
    messages::FEATURE_AUDIO_ACCESS_UNIT_V1,
    messages::FEATURE_DECODER_DESCRIPTION_V1,
    messages::FEATURE_OBSERVABILITY_CORE_V1,
    messages::FEATURE_ATOMIC_CONTROL_V1,
    messages::FEATURE_SOURCE_DESCRIPTOR_V1,
    messages::FEATURE_DELEGATED_CONTEXT_V1,
    messages::FEATURE_SOURCE_CAPTURE_POLICY_V1,
    // HELLO requires strictly increasing feature IDs; keep this list sorted by ID.
    messages::FEATURE_RASTER_DELTA_V1,
    messages::FEATURE_IMAGE_CACHE_V1,
    messages::FEATURE_MEDIA_ORDER_BARRIER_V1,
];
const MAX_PENDING_CONTROL_REPLIES: usize = 4096;
const MEDIA_WRITER_QUEUE: usize = 32;
/// How long one outer control reply may be awaited.
///
/// Generous relative to any legitimate reply — a presenter answers `COMMIT_TXN` at a compositor
/// boundary — but finite, because this wait happens on the single bridge worker thread that also
/// forwards media and applies projections.
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Browser presenters may initialize a hardware or software codec before accepting `CREATE_VIDEO`.
///
/// Some WebCodecs implementations take tens of seconds to initialize AV1 while keeping their
/// control connection responsive. Treat that bounded initialization as a slow source capability
/// check, not as a dead presenter that requires replacement.
const SOURCE_READY_REPLY_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a media writer may wait for outer credit before failing its delivery.
///
/// Longer than the control deadline: withholding credit is legitimate backpressure, not a fault.
/// The bound exists so a presenter that stops granting entirely cannot strand the delivery and
/// pin the writer thread forever.
const OUTER_CREDIT_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Bound retries on one continuously changing display.
///
/// A stale commit is recoverable in-place, but an unbounded retry loop here would monopolize the
/// single bridge worker while a browser is being resized continuously. After this many immediate
/// retries the worker asks the session actor for a fresh snapshot and tries again later.
const DISPLAY_COMMIT_RETRIES: usize = 8;

#[derive(Debug)]
struct OuterProtocolError {
    code: u64,
    diagnostic: String,
}

impl std::fmt::Display for OuterProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "presenter error {}: {}",
            self.code, self.diagnostic
        )
    }
}

impl std::error::Error for OuterProtocolError {}

struct ControlState {
    replies: HashMap<u64, Record>,
    pending_requests: HashSet<u64>,
    /// Requests whose wait hit its deadline.
    ///
    /// The request stays known so a late reply is discarded quietly instead of being treated as a
    /// reply to something never sent, which would close an otherwise healthy connection.
    abandoned_requests: HashSet<u64>,
    /// When the oldest currently outstanding request was issued.
    ///
    /// `last_inbound` cannot answer "is this request progressing": unsolicited `CREDIT` and event
    /// records keep refreshing it while a reply never arrives.
    oldest_pending_request: Option<Instant>,
    wait_us: u64,
    wait_timeouts: u64,
    credits: HashMap<u64, messages::CreditLedger>,
    keyframes: Vec<messages::NeedKeyframe>,
    full_frames: Vec<messages::NeedFullFrame>,
    source_losses: Vec<u64>,
    playback_states: Vec<messages::PlaybackState>,
    display_generation: u64,
    capability_generation: u64,
    capability_changes: Vec<messages::CapsChanged>,
    closed: Option<String>,
    last_inbound: Instant,
    last_probe_sent: Option<Instant>,
    unanswered_probes: u8,
    next_ping_id: u64,
    pending_pings: HashMap<u64, Instant>,
    rtt_us: Option<u64>,
    outer_scene_revision: SceneRevision,
    outer_source_revisions: HashMap<u64, SourceRevision>,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            replies: HashMap::new(),
            pending_requests: HashSet::new(),
            abandoned_requests: HashSet::new(),
            oldest_pending_request: None,
            wait_us: 0,
            wait_timeouts: 0,
            credits: HashMap::new(),
            keyframes: Vec::new(),
            full_frames: Vec::new(),
            source_losses: Vec::new(),
            playback_states: Vec::new(),
            display_generation: 0,
            capability_generation: 1,
            capability_changes: Vec::new(),
            closed: None,
            last_inbound: Instant::now(),
            last_probe_sent: None,
            unanswered_probes: 0,
            next_ping_id: u64::MAX,
            pending_pings: HashMap::new(),
            rtt_us: None,
            outer_scene_revision: SceneRevision::ZERO,
            outer_source_revisions: HashMap::new(),
        }
    }
}

fn apply_capability_change(
    state: &mut ControlState,
    changed: messages::CapsChanged,
) -> io::Result<()> {
    if changed.capability_generation <= state.capability_generation {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "outer capability generation did not advance",
        ));
    }
    state.capability_generation = changed.capability_generation;
    state.capability_changes.push(changed);
    Ok(())
}

struct SharedControl {
    state: Mutex<ControlState>,
    changed: Condvar,
}

struct ControlDispatcher {
    writer: ConnectionWriter,
    shared: Arc<SharedControl>,
}

impl ControlDispatcher {
    fn start(
        connection: Connection,
        display_generation: u64,
        capability_generation: u64,
    ) -> io::Result<Self> {
        let (mut reader, writer) = connection.split()?;
        let shared = Arc::new(SharedControl {
            state: Mutex::new(ControlState {
                display_generation,
                capability_generation,
                last_inbound: Instant::now(),
                last_probe_sent: None,
                unanswered_probes: 0,
                next_ping_id: u64::MAX,
                pending_pings: HashMap::new(),
                rtt_us: None,
                ..ControlState::default()
            }),
            changed: Condvar::new(),
        });
        let reader_shared = shared.clone();
        let reader_writer = writer.clone();
        thread::Builder::new()
            .name("vvmux-vivid-control".into())
            .spawn(move || {
                loop {
                    let record = match reader.read_record() {
                        Ok(record) => record,
                        Err(error) => {
                            let mut state = reader_shared
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            state.closed.get_or_insert_with(|| error.to_string());
                            reader_shared.changed.notify_all();
                            break;
                        }
                    };
                    {
                        let mut state = reader_shared
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state.last_inbound = Instant::now();
                        state.last_probe_sent = None;
                        state.unanswered_probes = 0;
                        if record.record_type != messages::PONG {
                            state.pending_pings.clear();
                        }
                    }
                    if record.record_type == messages::PING {
                        let result = messages::decode_control(&record.body).and_then(|envelope| {
                            if record.object_id != 0 || envelope.request_id == 0 {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "outer PING is not a correlated session-level request",
                                ));
                            }
                            reader_writer.write_record(
                                messages::PONG,
                                0,
                                0,
                                &messages::ok(envelope.request_id),
                            )
                        });
                        if let Err(error) = result {
                            let mut state = reader_shared
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            state.closed.get_or_insert_with(|| error.to_string());
                            reader_shared.changed.notify_all();
                            break;
                        }
                        continue;
                    }
                    let mut state = reader_shared
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let routed = match record.record_type {
                        messages::CREDIT => {
                            messages::parse_credit(&record.body).and_then(|credits| {
                                state
                                    .credits
                                    .entry(record.object_id)
                                    .or_default()
                                    .grant(credits)
                            })
                        }
                        messages::NEED_KEYFRAME => messages::parse_need_keyframe(&record.body)
                            .map(|request| state.keyframes.push(request)),
                        // Raster recovery on this hop's own delta chain. Ignoring it would leave
                        // the outer source rejecting every later delta with BAD_STATE.
                        messages::NEED_FULL_FRAME => messages::parse_need_full_frame(&record.body)
                            .and_then(|request| {
                                if request.source_id != record.object_id {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "outer NEED_FULL_FRAME source/object ID mismatch",
                                    ));
                                }
                                state.full_frames.push(request);
                                Ok(())
                            }),
                        messages::SOURCE_LOST => messages::parse_source_lost(&record.body)
                            .and_then(|lost| {
                                if lost.source_id != record.object_id {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "outer SOURCE_LOST source/object ID mismatch",
                                    ));
                                }
                                if let Some(ledger) = state.credits.get_mut(&lost.source_id) {
                                    ledger.mark_lost();
                                }
                                state.source_losses.push(lost.source_id);
                                Ok(())
                            }),
                        messages::DISPLAY_CHANGED => messages::parse_display_changed(&record.body)
                            .map(|display| state.display_generation = display.display_generation),
                        messages::CAPS_CHANGED => messages::parse_caps_changed(&record.body)
                            .and_then(|changed| apply_capability_change(&mut state, changed)),
                        messages::SOURCE_CHANGED => messages::parse_source_changed(&record.body)
                            .and_then(|event| {
                                if event.source_id != record.object_id {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "outer SOURCE_CHANGED source/object ID mismatch",
                                    ));
                                }
                                state
                                    .outer_source_revisions
                                    .insert(event.source_id, event.source_revision);
                                Ok(())
                            }),
                        messages::SCENE_CHANGED => messages::parse_scene_changed(&record.body)
                            .map(|event| state.outer_scene_revision = event.scene_revision),
                        messages::PLAYBACK_STATE => messages::parse_playback_state(&record.body)
                            .map(|event| state.playback_states.push(event)),
                        messages::PONG => {
                            messages::request_id(&record.body).and_then(|request_id| {
                                if record.object_id != 0 || request_id == 0 {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "outer PONG is not a correlated session-level reply",
                                    ));
                                }
                                if let Some(sent) = state.pending_pings.remove(&request_id) {
                                    let sample = u64::try_from(sent.elapsed().as_micros())
                                        .unwrap_or(u64::MAX);
                                    state.rtt_us = Some(state.rtt_us.map_or(sample, |current| {
                                        current.saturating_mul(7).saturating_add(sample) / 8
                                    }));
                                }
                                Ok(())
                            })
                        }
                        _ => messages::request_id(&record.body).and_then(|request_id| {
                            if request_id == 0 {
                                return Ok(());
                            }
                            // A reply that arrives after its wait gave up is expected, not a
                            // protocol violation. Discard it rather than closing the connection.
                            if state.abandoned_requests.remove(&request_id) {
                                return Ok(());
                            }
                            if !state.pending_requests.remove(&request_id) {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "outer presenter replied to an unknown request",
                                ));
                            }
                            if state.pending_requests.is_empty() {
                                state.oldest_pending_request = None;
                            }
                            if state.replies.len() >= MAX_PENDING_CONTROL_REPLIES
                                || state.replies.insert(request_id, record).is_some()
                            {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "outer control reply queue overflow or duplicate",
                                ));
                            }
                            Ok(())
                        }),
                    };
                    if let Err(error) = routed {
                        state.closed.get_or_insert_with(|| error.to_string());
                    }
                    reader_shared.changed.notify_all();
                    if state.closed.is_some() {
                        break;
                    }
                }
            })?;
        let heartbeat_shared = shared.clone();
        let heartbeat_writer = writer.clone();
        thread::Builder::new()
            .name("vvmux-vivid-heartbeat".into())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_secs(1));
                    let request = {
                        let mut state = heartbeat_shared
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if state.closed.is_some() {
                            break;
                        }
                        let now = Instant::now();
                        // Liveness is per request, not per connection: unsolicited `CREDIT` and
                        // event records keep `last_inbound` fresh while a reply this bridge is
                        // waiting on never arrives, so a presenter that has stopped answering can
                        // look perfectly healthy.
                        let request_stalled = state
                            .oldest_pending_request
                            .is_some_and(|issued| now.duration_since(issued) >= HEARTBEAT_INTERVAL);
                        if (now.duration_since(state.last_inbound) < HEARTBEAT_INTERVAL
                            && !request_stalled)
                            || state
                                .last_probe_sent
                                .is_some_and(|sent| now.duration_since(sent) < HEARTBEAT_INTERVAL)
                        {
                            continue;
                        }
                        if state.unanswered_probes >= 3 {
                            state.closed = Some("outer Vivid heartbeat timed out".into());
                            heartbeat_shared.changed.notify_all();
                            break;
                        }
                        let request = state.next_ping_id;
                        state.next_ping_id = state.next_ping_id.saturating_sub(1);
                        state.last_probe_sent = Some(now);
                        state.unanswered_probes = state.unanswered_probes.saturating_add(1);
                        state.pending_pings.insert(request, now);
                        request
                    };
                    if heartbeat_writer
                        .write_record(messages::PING, 0, 0, &messages::ok(request))
                        .is_err()
                    {
                        let mut state = heartbeat_shared
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state
                            .closed
                            .get_or_insert_with(|| "outer heartbeat write failed".into());
                        heartbeat_shared.changed.notify_all();
                        break;
                    }
                }
            })?;
        Ok(Self { writer, shared })
    }

    fn write_record(
        &self,
        record_type: u16,
        flags: u16,
        object_id: u64,
        body: &[u8],
    ) -> io::Result<()> {
        let request_id = messages::request_id(body)?;
        if request_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "outbound outer request has request ID zero",
            ));
        }
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.pending_requests.len() >= MAX_PENDING_CONTROL_REPLIES
                || !state.pending_requests.insert(request_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "outer pending request bound exceeded or request ID was reused",
                ));
            }
            state
                .oldest_pending_request
                .get_or_insert_with(Instant::now);
        }
        if let Err(error) = self
            .writer
            .write_record(record_type, flags, object_id, body)
        {
            self.shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pending_requests
                .remove(&request_id);
            return Err(error);
        }
        Ok(())
    }

    fn wait_reply(
        &self,
        request_id: u64,
        expected: u16,
        expected_object_id: u64,
    ) -> io::Result<Record> {
        self.wait_reply_with_timeout(
            request_id,
            expected,
            expected_object_id,
            CONTROL_REPLY_TIMEOUT,
        )
    }

    fn wait_reply_with_timeout(
        &self,
        request_id: u64,
        expected: u16,
        expected_object_id: u64,
        timeout: Duration,
    ) -> io::Result<Record> {
        wait_reply_on(
            &self.shared,
            request_id,
            expected,
            expected_object_id,
            timeout,
        )
    }

    /// Retire a destroyed source's credit ledger so any waiter fails instead of parking forever.
    fn retire_source(&self, source_id: u64) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Outer source IDs are allocated monotonically and never reused, so dropping the ledger
        // cannot affect a later source.
        state.credits.remove(&source_id);
        self.shared.changed.notify_all();
    }

    fn take_wait_stats(&self) -> (u64, u64) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            std::mem::take(&mut state.wait_us),
            std::mem::take(&mut state.wait_timeouts),
        )
    }

    fn register_source(&self, ready: &messages::SourceReady) -> io::Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .credits
            .insert(
                ready.source_id,
                messages::CreditLedger::new(messages::Credits {
                    bytes: ready.byte_credits,
                    packets: ready.packet_credits,
                    fragments: ready.fragment_credits,
                }),
            )
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "outer source credit ledger already exists",
            ));
        }
        self.shared.changed.notify_all();
        Ok(())
    }

    fn display_generation(&self) -> u64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .display_generation
    }

    fn take_keyframes(&self) -> Vec<messages::NeedKeyframe> {
        std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .keyframes,
        )
    }

    fn take_full_frames(&self) -> Vec<messages::NeedFullFrame> {
        std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .full_frames,
        )
    }

    fn take_source_losses(&self) -> Vec<u64> {
        std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .source_losses,
        )
    }

    fn take_playback_states(&self) -> Vec<messages::PlaybackState> {
        std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .playback_states,
        )
    }

    fn take_capability_changes(&self) -> Vec<messages::CapsChanged> {
        std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .capability_changes,
        )
    }

    fn adjusted_minimum_buffer(&self, requested_us: u64) -> u64 {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        messages::minimum_buffer_for_rtt(requested_us, state.rtt_us)
    }
}

impl Drop for ControlDispatcher {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .closed
            .get_or_insert_with(|| "outer bridge closed".into());
        self.shared.changed.notify_all();
    }
}

struct MediaWrite {
    delivery_id: u64,
    record_type: u16,
    object_id: u64,
    body: Vec<u8>,
}

enum MediaWriterCommand {
    Write(MediaWrite),
    Barrier(mpsc::SyncSender<u64>),
}

struct MediaCompletion {
    delivery_id: u64,
    delivered: bool,
    record_sequence: u64,
    /// Outer source the body was written to.
    ///
    /// Retained hydration carries delivery ID 0, so the source is the only way to report which
    /// image the outer presenter actually received.
    object_id: u64,
}

struct SourceMediaWriter {
    sender: mpsc::SyncSender<MediaWriterCommand>,
}

pub(crate) trait ConnectionFactory: Send + Sync {
    fn open(&self, kind: ConnectionKind) -> io::Result<Connection>;
}

struct EndpointConnectionFactory {
    primary: Endpoint,
    bulk: Option<Endpoint>,
}

impl ConnectionFactory for EndpointConnectionFactory {
    fn open(&self, kind: ConnectionKind) -> io::Result<Connection> {
        if kind != ConnectionKind::Control
            && let Some(bulk) = &self.bulk
        {
            return Connection::open(bulk, kind).or_else(|_| Connection::open(&self.primary, kind));
        }
        Connection::open(&self.primary, kind)
    }
}

pub struct OuterBridge {
    connection_factory: Arc<dyn ConnectionFactory>,
    token: Zeroizing<String>,
    control: ControlDispatcher,
    root_context: u64,
    next_context: u64,
    next_request: u64,
    next_source: u64,
    next_node: u64,
    next_transaction: u64,
    source_ids: HashMap<BridgeSourceKey, u64>,
    reverse_source_ids: HashMap<u64, BridgeSourceKey>,
    media: HashMap<BridgeSourceKey, SourceMediaWriter>,
    pending: HashMap<BridgeSourceKey, PendingBody>,
    completions_tx: mpsc::Sender<MediaCompletion>,
    completions_rx: mpsc::Receiver<MediaCompletion>,
    source_kinds: HashMap<BridgeSourceKey, BridgeSourceKind>,
    /// Hop-local raster identities, re-originated so an inner delta base can never leak outward.
    raster_frame_ids: HashMap<BridgeSourceKey, u64>,
    /// Per-source state of vvmux's own outgoing raster delta chain.
    raster_chains: HashMap<BridgeSourceKey, RasterChain>,
    /// Sources whose outgoing chain is broken and need a full frame from the inner producer.
    raster_needs_full: HashSet<BridgeSourceKey>,
    active_sources: HashMap<BridgeSourceKey, BridgeSource>,
    node_ids: HashMap<(u64, u64, u8), u64>,
    display: DisplayMetrics,
    /// Outer presenter accepted `decoder-description-v1`; forwarding the optional CREATE fields
    /// without acceptance would violate the specification.
    decoder_description: bool,
    hello_extensions: Vec<PreservedField>,
    #[allow(dead_code)] // Retained for negotiation-aware gateway consumers and conformance tests.
    welcome_extensions: Vec<PreservedField>,
    /// Local acknowledgement domain for successfully reconciled outer snapshots.
    outer_applied_revision: u64,
    /// Changes whenever this worker replaces its outer presenter session.
    diagnostic_instance_generation: u64,
    outer_attachment_generations: HashMap<BridgeSourceKey, u64>,
    delegated_contexts: bool,
    capture_policy: bool,
    source_descriptors: bool,
    media_order_barrier: bool,
    image_cache: bool,
    /// Outer presenter accepted `raster-delta-v1`, so this hop may re-originate deltas rather than
    /// expanding every inner update to a full frame.
    raster_delta: bool,
    /// Outer presenter accepted `raster-zstd-v1`.
    raster_zstd: bool,
    cached_images: HashSet<BridgeSourceKey>,
    pane_contexts: HashMap<u64, PaneContextMapping>,
}

#[derive(Clone, Copy)]
struct PaneContextMapping {
    context_id: u64,
    _class_mask: u64,
    _quotas: messages::ContextQuotas,
}

struct PendingBody {
    record_type: u16,
    total: usize,
    received: usize,
    cached_image: bool,
    bytes: Vec<u8>,
}

impl OuterBridge {
    #[allow(dead_code)] // Convenience entry point used by tests and non-bulk embedders.
    pub fn connect(
        endpoint: String,
        token: Zeroizing<String>,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        Self::connect_with_bulk(endpoint, None, token, display)
    }

    pub fn connect_with_bulk(
        endpoint: String,
        bulk_endpoint: Option<String>,
        token: Zeroizing<String>,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        let connection_factory = Arc::new(EndpointConnectionFactory {
            primary: Endpoint::parse(&endpoint)?,
            bulk: bulk_endpoint.as_deref().map(Endpoint::parse).transpose()?,
        });
        Self::connect_with_factory(connection_factory, token, display)
    }

    pub(crate) fn connect_with_factory(
        connection_factory: Arc<dyn ConnectionFactory>,
        token: Zeroizing<String>,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        Self::connect_with_factory_and_extensions(connection_factory, token, display, &[])
    }

    pub(crate) fn connect_with_factory_and_extensions(
        connection_factory: Arc<dyn ConnectionFactory>,
        token: Zeroizing<String>,
        _display: DisplayMetrics,
        hello_extensions: &[PreservedField],
    ) -> io::Result<Self> {
        let mut connection = connection_factory.open(ConnectionKind::Control)?;
        let hello_request = 1;
        let body = messages::encode_hello(
            hello_request,
            &HelloConfig {
                minimum_major: u64::from(VIVID_MAJOR),
                minimum_minor: u64::from(VIVID_MINOR),
                maximum_major: u64::from(VIVID_MAJOR),
                maximum_minor: u64::from(VIVID_MINOR),
                token: &token,
                producer: "vvmux",
                producer_version: env!("CARGO_PKG_VERSION"),
                required_features: REQUIRED_FEATURES,
                optional_features: OPTIONAL_FEATURES,
                maximum_record_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
                authentication_kind: messages::AUTHENTICATION_WINDOW_ROOT,
                preserved_fields: hello_extensions,
            },
        );
        connection.write_record(messages::HELLO, 0, 0, &body)?;
        let welcome = loop {
            let record = connection.read_record()?;
            match record.record_type {
                messages::WELCOME => break messages::parse_welcome(&record.body)?,
                messages::ERROR => return Err(protocol_error(&record.body)),
                _ => continue,
            }
        };
        let accepted_features =
            messages::negotiate_features(REQUIRED_FEATURES, OPTIONAL_FEATURES, |feature| {
                welcome.accepted_features.contains(&feature)
            })
            .map_err(|feature| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("outer presenter did not negotiate required feature {feature}"),
                )
            })?;
        if accepted_features != welcome.accepted_features {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "outer WELCOME accepted features are unsorted, duplicated, or were not offered",
            ));
        }
        let display = presenter_display_metrics(
            welcome.grid_columns,
            welcome.grid_rows,
            welcome.cell_width,
            welcome.cell_height,
        )?;
        let decoder_description =
            accepted_features.contains(&messages::FEATURE_DECODER_DESCRIPTION_V1);
        let delegated_contexts =
            accepted_features.contains(&messages::FEATURE_DELEGATED_CONTEXT_V1);
        let capture_policy =
            accepted_features.contains(&messages::FEATURE_SOURCE_CAPTURE_POLICY_V1);
        let source_descriptors =
            accepted_features.contains(&messages::FEATURE_SOURCE_DESCRIPTOR_V1);
        let media_order_barrier =
            accepted_features.contains(&messages::FEATURE_MEDIA_ORDER_BARRIER_V1);
        let image_cache = accepted_features.contains(&messages::FEATURE_IMAGE_CACHE_V1);
        let raster_delta = accepted_features.contains(&messages::FEATURE_RASTER_DELTA_V1);
        let raster_zstd = accepted_features.contains(&messages::FEATURE_RASTER_ZSTD_V1);
        connection.set_send_body_limit(welcome.maximum_control_body)?;
        let control = ControlDispatcher::start(
            connection,
            welcome.display_generation,
            welcome.capability_generation,
        )?;
        let (completions_tx, completions_rx) = mpsc::channel();
        let mut bridge = Self {
            connection_factory,
            token,
            control,
            root_context: welcome.root_context_id,
            next_context: welcome.root_context_id,
            next_request: hello_request,
            next_source: 0,
            next_node: 0,
            next_transaction: 0,
            source_ids: HashMap::new(),
            reverse_source_ids: HashMap::new(),
            media: HashMap::new(),
            pending: HashMap::new(),
            completions_tx,
            completions_rx,
            source_kinds: HashMap::new(),
            raster_frame_ids: HashMap::new(),
            raster_chains: HashMap::new(),
            raster_needs_full: HashSet::new(),
            active_sources: HashMap::new(),
            node_ids: HashMap::new(),
            display,
            decoder_description,
            hello_extensions: hello_extensions.to_vec(),
            welcome_extensions: welcome.preserved_fields,
            outer_applied_revision: 0,
            diagnostic_instance_generation: 1,
            outer_attachment_generations: HashMap::new(),
            delegated_contexts,
            capture_policy,
            source_descriptors,
            media_order_barrier,
            image_cache,
            raster_delta,
            raster_zstd,
            cached_images: HashSet::new(),
            pane_contexts: HashMap::new(),
        };
        if accepted_features.contains(&messages::FEATURE_OBSERVABILITY_CORE_V1) {
            let request = bridge.request_id()?;
            bridge.control.write_record(
                messages::SET_OBSERVATION,
                0,
                0,
                &messages::set_observation(request, messages::OBSERVATION_CLASS_MASK)?,
            )?;
            bridge.wait_for(request, messages::OK, 0)?;
        }
        Ok(bridge)
    }

    pub fn display_metrics(&self) -> DisplayMetrics {
        self.display
    }

    pub fn mark_projection_applied(&mut self) -> u64 {
        self.outer_applied_revision = self.outer_applied_revision.saturating_add(1);
        self.outer_applied_revision
    }

    pub fn diagnostic_instance_generation(&self) -> u64 {
        self.diagnostic_instance_generation
    }

    pub fn attachment_generations(&self) -> Vec<(BridgeSourceKey, u64)> {
        let mut generations = self
            .outer_attachment_generations
            .iter()
            .map(|(&source, &generation)| (source, generation))
            .collect::<Vec<_>>();
        generations.sort_by_key(|(source, _)| (source.producer, source.source));
        generations
    }

    pub fn rebuild(
        &mut self,
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<std::collections::HashSet<BridgeSourceKey>> {
        validate_snapshot(sources, nodes)?;
        match self.reconcile(sources, nodes) {
            Ok(recreated) => return Ok(recreated),
            // Display changes are ordinary concurrent state, not evidence that the session is
            // corrupt. Replacing the session here adds latency on native presenters and deadlocks
            // browser presenters waiting for a WebSocket that they were never asked to open.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Err(error),
            Err(_) => {}
        }
        // A failed reconciliation may have applied a prefix of ordered control requests. Stop
        // audible output from the uncertain session, close it, and rebuild exactly once from the
        // newest authoritative snapshot; the replacement session re-creates and re-plays every
        // playing source.
        let previous = self.active_sources.values().cloned().collect::<Vec<_>>();
        let _ = self.pause_playing_sources(&previous);
        let mut replacement = Self::connect_with_factory_and_extensions(
            self.connection_factory.clone(),
            Zeroizing::new((*self.token).clone()),
            self.display,
            &self.hello_extensions,
        )?;
        let recreated = replacement.reconcile(sources, nodes)?;
        replacement.diagnostic_instance_generation =
            self.diagnostic_instance_generation.saturating_add(1);
        *self = replacement;
        Ok(recreated)
    }

    /// Build a known-clean replacement session without first touching an already-uncertain
    /// control stream.
    pub fn replace_session(
        &mut self,
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<std::collections::HashSet<BridgeSourceKey>> {
        validate_snapshot(sources, nodes)?;
        let previous = self.active_sources.values().cloned().collect::<Vec<_>>();
        let _ = self.pause_playing_sources(&previous);
        let mut replacement = Self::connect_with_factory_and_extensions(
            self.connection_factory.clone(),
            Zeroizing::new((*self.token).clone()),
            self.display,
            &self.hello_extensions,
        )?;
        let recreated = replacement.reconcile(sources, nodes)?;
        replacement.diagnostic_instance_generation =
            self.diagnostic_instance_generation.saturating_add(1);
        *self = replacement;
        Ok(recreated)
    }

    fn reconcile(
        &mut self,
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<std::collections::HashSet<BridgeSourceKey>> {
        for source in sources {
            messages::validate_capture_policy(source.capture_policy)?;
            if let Some(descriptor) = source.descriptor.as_ref() {
                messages::validate_source_descriptor(&protocol_descriptor(descriptor))?;
                if !self.source_descriptors {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "outer presenter lacks source-descriptor-v1",
                    ));
                }
            }
            if source.capture_policy != 0 && !self.capture_policy {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "outer presenter lacks source-capture-policy-v1",
                ));
            }
            if let Some(previous) = self.active_sources.get(&source.key)
                && source.capture_policy & previous.capture_policy != previous.capture_policy
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "capture policy cannot be relaxed across the bridge",
                ));
            }
            if let Some(previous) = self.active_sources.get(&source.key) {
                match (&previous.descriptor, &source.descriptor) {
                    (Some(previous), Some(current))
                        if previous != current
                            && current.content_revision <= previous.content_revision =>
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "descriptor content revision must advance across the bridge",
                        ));
                    }
                    (Some(_), None) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "source descriptor cannot be removed across the bridge",
                        ));
                    }
                    _ => {}
                }
            }
        }
        self.sync_pane_contexts(sources, nodes)?;
        let requested = sources
            .iter()
            .map(|source| (source.key, source))
            .collect::<HashMap<_, _>>();
        let mut recreate = sources
            .iter()
            .filter(|source| self.source_kinds.get(&source.key) != Some(&source.kind))
            .map(|source| source.key)
            .collect::<std::collections::HashSet<_>>();
        loop {
            let previous_len = recreate.len();
            for source in sources {
                if let BridgeSourceKind::Audio {
                    linked_video: Some(video),
                    ..
                } = source.kind
                    && recreate.contains(&video)
                {
                    recreate.insert(source.key);
                }
            }
            if recreate.len() == previous_len {
                break;
            }
        }

        let obsolete = self
            .source_ids
            .iter()
            .filter_map(|(key, upstream)| {
                (!requested.contains_key(key) || recreate.contains(key))
                    .then_some((*key, *upstream))
            })
            .collect::<Vec<_>>();
        let mut obsolete_media = Vec::with_capacity(obsolete.len());
        for (key, upstream) in &obsolete {
            if let Some(writer) = self.media.remove(key) {
                // Keep the old media transport open until its scene nodes are deleted and
                // DESTROY_SOURCE is acknowledged. Presenters treat an early media EOF as
                // SOURCE_LOST and remove those nodes themselves, making the ordered scene
                // transaction fail with NOT_FOUND.
                obsolete_media.push(writer);
            }
            self.pending.remove(key);
            self.raster_frame_ids.remove(key);
            // A recreated outer source has no retained framebuffer, so its chain restarts from a
            // full frame. Dropping the chain here is what forces that.
            self.raster_chains.remove(key);
            self.raster_needs_full.remove(key);
            self.cached_images.remove(key);
            self.reverse_source_ids.remove(upstream);
            if !requested.contains_key(key) {
                self.source_ids.remove(key);
                self.source_kinds.remove(key);
            }
        }

        let new_sources = sources
            .iter()
            .filter(|source| recreate.contains(&source.key))
            .cloned()
            .collect::<Vec<_>>();
        self.create_sources(&new_sources)?;
        for source in sources
            .iter()
            .filter(|source| !recreate.contains(&source.key))
        {
            let Some(previous) = self.active_sources.get(&source.key) else {
                continue;
            };
            if source.capture_policy == previous.capture_policy {
                continue;
            }
            let upstream = self.source_ids[&source.key];
            let request = self.request_id()?;
            self.control.write_record(
                messages::SET_SOURCE_POLICY,
                0,
                upstream,
                &messages::set_source_policy(request, upstream, source.capture_policy),
            )?;
            self.wait_for(request, messages::OK, upstream)?;
        }
        for source in sources
            .iter()
            .filter(|source| !recreate.contains(&source.key))
        {
            let Some(previous) = self.active_sources.get(&source.key) else {
                continue;
            };
            if source.descriptor == previous.descriptor {
                continue;
            }
            let Some(descriptor) = source.descriptor.as_ref() else {
                continue;
            };
            let upstream = self.source_ids[&source.key];
            let request = self.request_id()?;
            self.control.write_record(
                messages::UPDATE_SOURCE_DESCRIPTOR,
                0,
                upstream,
                &messages::update_source_descriptor(
                    request,
                    upstream,
                    &protocol_descriptor(descriptor),
                ),
            )?;
            self.wait_for(request, messages::OK, upstream)?;
        }
        self.reconcile_nodes(nodes)?;

        let previous_sources = self.active_sources.values().cloned().collect::<Vec<_>>();
        self.update_playback(&previous_sources, sources)?;
        for source in sources
            .iter()
            .filter(|source| recreate.contains(&source.key) && source.playing)
        {
            self.play_source(source)?;
        }
        // A recreated outer source starts with an open epoch, so a stream that already ended
        // inner-side has to be closed again; `update_playback` above sees no transition for it.
        for source in sources
            .iter()
            .filter(|source| recreate.contains(&source.key) && source.eos_epoch.is_some())
        {
            self.end_source(source)?;
        }

        for (_, upstream) in obsolete {
            let request = self.request_id()?;
            self.control.write_record(
                messages::DESTROY_SOURCE,
                0,
                upstream,
                &messages::destroy_source(request, upstream),
            )?;
            self.wait_for(request, messages::OK, upstream)?;
            // A destroyed source will never be granted credit again. Retiring the ledger wakes a
            // media writer parked in `reserve_outer_credit` now instead of at its deadline, and
            // lets that thread exit rather than outliving the source it served.
            self.control.retire_source(upstream);
        }
        drop(obsolete_media);
        self.active_sources = sources
            .iter()
            .cloned()
            .map(|source| (source.key, source))
            .collect();
        Ok(recreate)
    }

    fn sync_pane_contexts(
        &mut self,
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<()> {
        if !self.delegated_contexts {
            return Ok(());
        }
        let producers = sources
            .iter()
            .map(|source| source.key.producer)
            .collect::<HashSet<_>>();
        let removed = self
            .pane_contexts
            .keys()
            .copied()
            .filter(|producer| !producers.contains(producer))
            .collect::<Vec<_>>();
        for producer in removed {
            let mapping = self
                .pane_contexts
                .remove(&producer)
                .expect("removed pane context exists");
            let request = self.request_id()?;
            self.control.write_record(
                messages::REVOKE_CONTEXT,
                0,
                mapping.context_id,
                &messages::revoke_context(request, mapping.context_id),
            )?;
            self.wait_for(request, messages::OK, mapping.context_id)?;
        }
        for producer in producers {
            if self.pane_contexts.contains_key(&producer) {
                continue;
            }
            self.next_context = self
                .next_context
                .checked_add(1)
                .ok_or_else(|| exhausted("context"))?;
            let context_id = self.next_context;
            let producer_sources = sources
                .iter()
                .filter(|source| source.key.producer == producer)
                .collect::<Vec<_>>();
            let maximum_retained_pixels = producer_sources
                .iter()
                .map(|source| bridge_source_pixels(source))
                .try_fold(0_u64, u64::checked_add)
                .unwrap_or(u64::MAX)
                .max(1);
            let maximum_media_bytes = producer_sources
                .iter()
                .map(|source| bridge_source_media_body(source))
                .max()
                .unwrap_or(1)
                .max(4 * 1024 * 1024)
                .saturating_mul(producer_sources.len().max(1) as u64);
            let quotas = messages::ContextQuotas {
                maximum_sources: producer_sources.len().max(1) as u64,
                maximum_nodes: nodes
                    .iter()
                    .filter(|node| node.producer == producer)
                    .count()
                    .max(1) as u64,
                maximum_retained_pixels,
                maximum_media_bytes,
                maximum_media_connections: producer_sources.len().max(1) as u64,
            };
            let class_mask =
                messages::CONTEXT_CLASS_CREATE_SOURCE | messages::CONTEXT_CLASS_MUTATE_SCENE;
            let request = self.request_id()?;
            let create = messages::create_context(
                request,
                &messages::CreateContextRequest {
                    context_id,
                    parent_context_id: self.root_context,
                    class_mask,
                    label: format!("vvmux-pane-{producer}"),
                    expiry_us: 0,
                    quotas,
                },
            )?;
            self.control
                .write_record(messages::CREATE_CONTEXT, 0, context_id, &create)?;
            let ready_record =
                self.control
                    .wait_reply(request, messages::CONTEXT_READY, context_id)?;
            let (ready_request, ready) = messages::parse_context_ready(&ready_record.body)?;
            if ready_request != request || ready.context_id != context_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "outer CONTEXT_READY correlation mismatch",
                ));
            }
            let request = self.request_id()?;
            self.control.write_record(
                messages::DELEGATE_CONTEXT,
                0,
                context_id,
                &messages::delegate_context(request, context_id),
            )?;
            let capability_record =
                self.control
                    .wait_reply(request, messages::CONTEXT_CAPABILITY, context_id)?;
            let (capability_request, capability_context, capability) =
                messages::parse_context_capability(&capability_record.body)?;
            if capability_request != request || capability_context != context_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "outer CONTEXT_CAPABILITY correlation mismatch",
                ));
            }
            // The foreground bridge proves delegation succeeded, then immediately destroys its
            // one-shot plaintext copy. Neither the hidden server nor pane IPC has a credential
            // field, and current projection remains on the owner-only outer session.
            drop(Zeroizing::new(capability));
            self.pane_contexts.insert(
                producer,
                PaneContextMapping {
                    context_id,
                    _class_mask: ready.class_mask,
                    _quotas: ready.quotas,
                },
            );
        }
        Ok(())
    }

    /// Apply playback-only changes without replacing upstream sources, decoder state, or queued
    /// media. This is the normal PLAY transition after Vivi's initial prebuffer.
    pub fn update_playback(
        &mut self,
        previous: &[BridgeSource],
        current: &[BridgeSource],
    ) -> io::Result<()> {
        validate_snapshot(current, &[])?;
        for source in current {
            let Some(old) = previous.iter().find(|old| old.key == source.key) else {
                continue;
            };
            if old.playing != source.playing
                || (source.playing && old.play_request != source.play_request)
            {
                if source.playing {
                    self.play_source(source)?;
                } else {
                    self.pause_source(source)?;
                }
            }
            if old.eos_epoch != source.eos_epoch {
                self.end_source(source)?;
            }
        }
        Ok(())
    }

    /// Apply fragment/scene-only node changes in one upstream transaction without pausing
    /// sources, recreating media connections, replaying retained bodies, or requesting
    /// keyframes.
    pub fn update_nodes(&mut self, nodes: &[BridgeNode]) -> io::Result<()> {
        let sources = self.active_sources.values().cloned().collect::<Vec<_>>();
        validate_snapshot(&sources, nodes)?;
        self.reconcile_nodes(nodes)
    }

    /// Stop audible output from the current projection before a replacement session is built.
    /// Pausing a video also pauses its linked audio in Vivido.
    fn pause_playing_sources(&mut self, sources: &[BridgeSource]) -> io::Result<()> {
        let mut playing = sources
            .iter()
            .filter(|source| source.playing)
            .collect::<Vec<_>>();
        playing.sort_by_key(|source| match source.kind {
            BridgeSourceKind::Video { .. } => 0,
            BridgeSourceKind::Audio { .. } => 1,
            BridgeSourceKind::Raster { .. } | BridgeSourceKind::Image { .. } => 2,
        });
        for source in playing {
            self.pause_source(source)?;
        }
        Ok(())
    }

    // These arguments mirror one IPC MediaChunk record and are intentionally kept explicit.
    #[allow(clippy::too_many_arguments)]
    pub fn media_chunk(
        &mut self,
        delivery_id: u64,
        key: BridgeSourceKey,
        record_type: u16,
        offset: u32,
        total: u32,
        last: bool,
        bytes: Vec<u8>,
    ) -> io::Result<bool> {
        let cached_image = self.cached_images.contains(&key);
        let pending = self.pending.entry(key).or_insert_with(|| PendingBody {
            record_type,
            total: total as usize,
            received: 0,
            cached_image,
            bytes: Vec::with_capacity(if cached_image { 0 } else { total as usize }),
        });
        if pending.record_type != record_type
            || pending.total != total as usize
            || pending.received != offset as usize
        {
            self.pending.remove(&key);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media chunk sequence gap",
            ));
        }
        let cached_image = pending.cached_image;
        pending.received = pending.received.saturating_add(bytes.len());
        if !cached_image {
            pending.bytes.extend_from_slice(&bytes);
        }
        if pending.received > pending.total {
            self.pending.remove(&key);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media chunks exceed total",
            ));
        }
        if last {
            let mut pending = self.pending.remove(&key).unwrap();
            if pending.received != pending.total {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete media body",
                ));
            }
            if cached_image {
                if pending.record_type != messages::IMAGE_DATA
                    || !matches!(
                        self.source_kinds.get(&key),
                        Some(BridgeSourceKind::Image { .. })
                    )
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cache-hit source received non-image media",
                    ));
                }
                self.completions_tx
                    .send(MediaCompletion {
                        delivery_id,
                        delivered: true,
                        record_sequence: 0,
                        object_id: self.source_ids.get(&key).copied().unwrap_or(0),
                    })
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "outer media completion receiver stopped",
                        )
                    })?;
                return Ok(true);
            }
            let upstream = *self.source_ids.get(&key).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "projection source missing")
            })?;
            let next_raster_id = if matches!(
                self.source_kinds.get(&key),
                Some(BridgeSourceKind::Raster { .. })
            ) {
                Some(
                    self.raster_frame_ids
                        .get(&key)
                        .copied()
                        .unwrap_or(0)
                        .checked_add(1)
                        .ok_or_else(|| exhausted("outer raster frame"))?,
                )
            } else {
                None
            };
            let mut pending_chain = None;
            if let Some(frame_id) = next_raster_id {
                match self.reoriginate_raster(key, frame_id, &pending.bytes)? {
                    Some(reoriginated) => {
                        pending_chain = reoriginated.chain;
                        pending.bytes = reoriginated.body;
                    }
                    // The inner delta cannot extend this hop's chain. Ask the inner producer for a
                    // full frame and drop this update rather than sending an unusable delta.
                    None => {
                        self.raster_needs_full.insert(key);
                        self.completions_tx
                            .send(MediaCompletion {
                                delivery_id,
                                delivered: false,
                                record_sequence: 0,
                                object_id: upstream,
                            })
                            .map_err(|_| {
                                io::Error::new(
                                    io::ErrorKind::BrokenPipe,
                                    "outer media completion receiver stopped",
                                )
                            })?;
                        return Ok(false);
                    }
                }
            }
            self.media
                .get(&key)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "projection media channel missing")
                })?
                .sender
                .try_send(MediaWriterCommand::Write(MediaWrite {
                    delivery_id,
                    record_type: pending.record_type,
                    object_id: upstream,
                    body: pending.bytes,
                }))
                .map_err(|error| match error {
                    mpsc::TrySendError::Full(_) => io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "outer source media writer queue is full",
                    ),
                    mpsc::TrySendError::Disconnected(_) => io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "outer source media writer stopped",
                    ),
                })?;
            // Commit the chain only after the writer accepted the body: a rejected write leaves the
            // outer presenter on the previous frame, so advancing the base here would make every
            // later delta reference a frame that was never applied.
            if let Some(frame_id) = next_raster_id {
                self.raster_frame_ids.insert(key, frame_id);
                if let Some(mut chain) = pending_chain {
                    chain.base_frame_id = frame_id;
                    self.raster_chains.insert(key, chain);
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Sources whose outgoing raster chain broke and need a full frame from the inner producer.
    ///
    /// Merges this hop's own chain breaks with `NEED_FULL_FRAME` from the outer presenter. Both
    /// mean the same thing here: the next outgoing raster body for that source must be full, so
    /// the chain is dropped and a full frame requested from the producer that can still make one.
    pub fn take_full_frame_requests(&mut self) -> Vec<BridgeSourceKey> {
        let outer = self
            .control
            .take_full_frames()
            .into_iter()
            .filter_map(|request| self.reverse_source_ids.get(&request.source_id).copied())
            .collect::<Vec<_>>();
        self.raster_needs_full.extend(outer);
        let mut requests = self.raster_needs_full.drain().collect::<Vec<_>>();
        for key in &requests {
            self.raster_chains.remove(key);
        }
        requests.sort_by_key(|key| (key.producer, key.source));
        requests
    }

    /// Choose this hop's encoding for one inner raster body.
    ///
    /// `Ok(None)` means the body was a delta that cannot extend this hop's chain; the caller
    /// recovers by asking the inner producer for a full frame.
    fn reoriginate_raster(
        &mut self,
        key: BridgeSourceKey,
        frame_id: u64,
        body: &[u8],
    ) -> io::Result<Option<ReoriginatedRaster>> {
        let (width, height, compression_mode, delta_operation_limit) =
            match self.source_kinds.get(&key) {
                Some(BridgeSourceKind::Raster {
                    width,
                    height,
                    compression_mode,
                    delta_operation_limit,
                    ..
                }) => (*width, *height, *compression_mode, *delta_operation_limit),
                _ => {
                    return Ok(Some(ReoriginatedRaster {
                        body: reoriginated_full_raster(body, frame_id)?,
                        chain: None,
                    }));
                }
            };
        let flags = body
            .get(4..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "raster frame header is truncated",
                )
            })?;
        let now = Instant::now();
        if flags & media::RASTER_FRAME_DELTA == 0 {
            // A full frame restarts the chain and clears this hop's damage window.
            let parsed = media::parse_full_raster_frame(body)?;
            let chain = RasterChain {
                epoch: parsed.epoch,
                base_frame_id: 0,
                damage_pixels: 0,
                damage_window_started: now,
            };
            return Ok(Some(ReoriginatedRaster {
                body: reoriginated_full_raster(body, frame_id)?,
                chain: Some(chain),
            }));
        }
        let (Some(operation_limit), Some(mut chain)) = (
            delta_operation_limit.filter(|_| self.raster_delta),
            self.raster_chains.get(&key).copied(),
        ) else {
            return Ok(None);
        };
        let damage = media::parse_delta_raster_frame(body, width, height, operation_limit)
            .and_then(|frame| raster_damage_pixels(&frame.operations))?;
        if now.duration_since(chain.damage_window_started) >= RASTER_DAMAGE_INTERVAL {
            chain.damage_window_started = now;
            chain.damage_pixels = 0;
        }
        let budget = u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(RASTER_DAMAGE_FRAME_EQUIVALENTS);
        if chain.damage_pixels.saturating_add(damage) > budget {
            // Past the budget a full frame is the cheaper and simpler encoding.
            return Ok(None);
        }
        let compress = compression_mode == messages::COMPRESSION_RAW_OR_ZSTD && self.raster_zstd;
        let Some(reoriginated) = reoriginated_delta_raster(
            body,
            width,
            height,
            operation_limit,
            &chain,
            frame_id,
            compress,
        )?
        else {
            return Ok(None);
        };
        // The specification requires a producer to prefer a full frame whenever the delta is not
        // smaller, so a pathological damage pattern can never cost more than the full form.
        let full_frame_len = media::rgba8_raw_frame_body_len(width, height).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "raster dimensions overflow")
        })?;
        if reoriginated.len() >= full_frame_len as usize {
            return Ok(None);
        }
        chain.damage_pixels = chain.damage_pixels.saturating_add(damage);
        Ok(Some(ReoriginatedRaster {
            body: reoriginated,
            chain: Some(chain),
        }))
    }

    pub fn take_media_completions(&self) -> Vec<(u64, bool, u64, u64)> {
        self.completions_rx
            .try_iter()
            .map(|completion| {
                (
                    completion.delivery_id,
                    completion.delivered,
                    completion.record_sequence,
                    completion.object_id,
                )
            })
            .collect()
    }

    /// Map an outer source object ID back to the projection key that owns it.
    pub fn source_for_outer_object(&self, object_id: u64) -> Option<BridgeSourceKey> {
        self.source_ids
            .iter()
            .find_map(|(key, id)| (*id == object_id).then_some(*key))
    }

    pub fn take_keyframe_requests(&self) -> Vec<BridgeKeyframeRequest> {
        self.control
            .take_keyframes()
            .into_iter()
            .filter_map(|request| {
                self.reverse_source_ids
                    .get(&request.source_id)
                    .copied()
                    .map(|source| BridgeKeyframeRequest {
                        source,
                        minimum_epoch: Some(request.minimum_epoch),
                        reason: request.reason,
                    })
            })
            .collect()
    }

    pub fn take_source_losses(&mut self) -> Vec<BridgeSourceKey> {
        let losses = self
            .control
            .take_source_losses()
            .into_iter()
            .filter_map(|source_id| self.reverse_source_ids.get(&source_id).copied())
            .collect::<Vec<_>>();
        for key in &losses {
            self.media.remove(key);
            self.cached_images.remove(key);
            self.source_kinds.remove(key);
        }
        losses
    }

    pub fn take_playback_states(&self) -> Vec<(BridgeSourceKey, messages::PlaybackSnapshot)> {
        self.control
            .take_playback_states()
            .into_iter()
            .filter_map(|event| {
                self.reverse_source_ids
                    .get(&event.source_id)
                    .copied()
                    .map(|source| (source, event.snapshot))
            })
            .collect()
    }

    pub fn take_capability_changes(&self) -> Vec<messages::CapsChanged> {
        self.control.take_capability_changes()
    }

    /// Accumulated outer-control wait time and deadline expiries, for `inspect-media`.
    pub fn take_control_wait_stats(&self) -> (u64, u64) {
        self.control.take_wait_stats()
    }

    fn create_sources(&mut self, sources: &[BridgeSource]) -> io::Result<()> {
        let mut ordered = sources.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|source| match source.kind {
            BridgeSourceKind::Video { .. } => 0,
            BridgeSourceKind::Raster { .. } | BridgeSourceKind::Image { .. } => 1,
            BridgeSourceKind::Audio { .. } => 2,
        });
        let mut pending = Vec::with_capacity(ordered.len());
        for source in ordered {
            self.next_source = self
                .next_source
                .checked_add(1)
                .ok_or_else(|| exhausted("source"))?;
            let upstream = self.next_source;
            self.source_ids.insert(source.key, upstream);
            self.reverse_source_ids.insert(upstream, source.key);
            let request = self.request_id()?;
            let (record_type, kind, body) = match &source.kind {
                BridgeSourceKind::Raster {
                    width,
                    height,
                    alpha_mode,
                    compression_mode,
                    delta_operation_limit,
                } => {
                    let config = RasterSourceConfig {
                        source_id: upstream,
                        width: *width,
                        height: *height,
                        alpha_mode: *alpha_mode,
                        compression_mode: *compression_mode,
                    };
                    let descriptor = source.descriptor.as_ref().map(protocol_descriptor);
                    // Ask for delta updates only when the inner source can produce them and the
                    // outer presenter accepted the feature. Otherwise this stays a full-frame
                    // source and behaves exactly as before.
                    let body = match delta_operation_limit.filter(|_| self.raster_delta) {
                        Some(operation_limit) => messages::create_raster_with_update_extensions(
                            request,
                            &config,
                            messages::RasterUpdateConfig {
                                mode: messages::RASTER_FULL_FRAME_AND_DELTA,
                                operation_limit,
                            },
                            source.capture_policy,
                            descriptor.as_ref(),
                        )?,
                        None => messages::create_raster_with_extensions(
                            request,
                            &config,
                            source.capture_policy,
                            descriptor.as_ref(),
                        ),
                    };
                    (messages::CREATE_RASTER, ConnectionKind::Raster, body)
                }
                BridgeSourceKind::Image {
                    encoding,
                    width,
                    height,
                    encoded_length,
                    sha256,
                } => (
                    messages::CREATE_IMAGE,
                    ConnectionKind::Blob,
                    messages::create_image_with_cache_extensions(
                        request,
                        &ImageSourceConfig {
                            source_id: upstream,
                            encoding: *encoding,
                            width: *width,
                            height: *height,
                            encoded_length: *encoded_length,
                            sha256: *sha256,
                        },
                        self.image_cache
                            && sha256.is_some()
                            && source.capture_policy & messages::CAPTURE_POLICY_DENY_CACHE == 0,
                        source.capture_policy,
                        source.descriptor.as_ref().map(protocol_descriptor).as_ref(),
                    )?,
                ),
                BridgeSourceKind::Video {
                    codec,
                    packetization,
                    extradata,
                    width,
                    height,
                    profile,
                    level,
                    bitrate,
                    color_primaries,
                    transfer,
                    matrix,
                    range,
                    sar_num,
                    sar_den,
                    max_access_unit_bytes,
                    codec_string,
                    decoder_config,
                } => (
                    messages::CREATE_VIDEO,
                    ConnectionKind::Video,
                    messages::create_video_with_extensions(
                        request,
                        &VideoSourceConfig {
                            source_id: upstream,
                            codec,
                            packetization,
                            extradata,
                            width: *width,
                            height: *height,
                            profile: *profile,
                            level: *level,
                            bitrate: (*bitrate).min(i64::MAX as u64) as i64,
                            color_primaries: *color_primaries,
                            transfer: *transfer,
                            matrix: *matrix,
                            range: *range,
                            sar_num: *sar_num,
                            sar_den: *sar_den,
                            max_access_unit_bytes: *max_access_unit_bytes,
                            codec_string: self
                                .decoder_description
                                .then_some(codec_string.as_deref())
                                .flatten(),
                            decoder_config: self
                                .decoder_description
                                .then_some(decoder_config.as_deref())
                                .flatten(),
                        },
                        source.capture_policy,
                        source.descriptor.as_ref().map(protocol_descriptor).as_ref(),
                    ),
                ),
                BridgeSourceKind::Audio {
                    linked_video,
                    codec,
                    packetization,
                    extradata,
                    sample_rate,
                    channels,
                    channel_mask,
                    bitrate,
                    max_access_unit_bytes,
                    codec_string,
                } => {
                    let linked = linked_video.and_then(|key| self.source_ids.get(&key).copied());
                    (
                        messages::CREATE_AUDIO,
                        ConnectionKind::Audio,
                        messages::create_audio_with_extensions(
                            request,
                            &AudioSourceConfig {
                                source_id: upstream,
                                linked_video_source_id: linked,
                                codec,
                                packetization,
                                extradata,
                                sample_rate: *sample_rate,
                                channels: *channels,
                                channel_mask: *channel_mask,
                                bitrate: (*bitrate).min(i64::MAX as u64) as i64,
                                max_access_unit_bytes: *max_access_unit_bytes,
                                codec_string: self
                                    .decoder_description
                                    .then_some(codec_string.as_deref())
                                    .flatten(),
                            },
                            source.capture_policy,
                            source.descriptor.as_ref().map(protocol_descriptor).as_ref(),
                        ),
                    )
                }
            };
            let body = with_causation(&body, source.causation_id)?;
            self.control.write_record(record_type, 0, upstream, &body)?;
            pending.push((source.clone(), request, upstream, kind));
        }

        // All CREATE requests are now in flight on the ordered control stream. Correlate their
        // independently completed replies before attaching each source-specific media channel.
        for (source, request, upstream, kind) in pending {
            let ready = self.wait_source_ready(
                request,
                upstream,
                source_ready_timeout(matches!(&source.kind, BridgeSourceKind::Video { .. })),
            )?;
            self.source_kinds.insert(source.key, source.kind.clone());
            let generation = self
                .outer_attachment_generations
                .entry(source.key)
                .or_default();
            *generation = generation.saturating_add(1);
            if !ready.media_connection_required {
                if kind != ConnectionKind::Blob
                    || !matches!(source.kind, BridgeSourceKind::Image { .. })
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "outer presenter returned a cache hit for a non-image source",
                    ));
                }
                self.cached_images.insert(source.key);
                continue;
            }
            self.control.register_source(&ready)?;
            let mut media = self.connection_factory.open(kind)?;
            media.set_send_body_limit(ready.max_media_body)?;
            media.write_record(
                messages::ATTACH_CHANNEL,
                0,
                upstream,
                &messages::attach_channel(&ready.media_ticket),
            )?;
            let (sender, receiver) = mpsc::sync_channel(MEDIA_WRITER_QUEUE);
            let shared = self.control.shared.clone();
            let completions = self.completions_tx.clone();
            thread::Builder::new()
                .name(format!("vvmux-media-{upstream}"))
                .spawn(move || run_media_writer(media, shared, receiver, completions))?;
            self.media.insert(source.key, SourceMediaWriter { sender });
            self.cached_images.remove(&source.key);
            if matches!(source.kind, BridgeSourceKind::Raster { .. }) {
                self.raster_frame_ids.insert(source.key, 0);
            }
        }
        Ok(())
    }

    fn reconcile_nodes(&mut self, nodes: &[BridgeNode]) -> io::Result<()> {
        let mut next_node_ids = self.node_ids.clone();
        for node in nodes {
            let stable_key = (node.producer, node.node, node.fragment);
            if let std::collections::hash_map::Entry::Vacant(entry) =
                next_node_ids.entry(stable_key)
            {
                self.next_node = self
                    .next_node
                    .checked_add(1)
                    .ok_or_else(|| exhausted("node"))?;
                entry.insert(self.next_node);
            }
        }
        let current_keys = nodes
            .iter()
            .map(|node| (node.producer, node.node, node.fragment))
            .collect::<HashSet<_>>();
        next_node_ids.retain(|stable_key, _| current_keys.contains(stable_key));

        for attempt in 0..DISPLAY_COMMIT_RETRIES {
            match self.reconcile_nodes_once(nodes, &next_node_ids) {
                Ok(()) => {
                    self.node_ids = next_node_ids;
                    return Ok(());
                }
                Err(error)
                    if protocol_error_code(&error)
                        == Some(messages::ERROR_STALE_DISPLAY_GENERATION) =>
                {
                    if attempt + 1 == DISPLAY_COMMIT_RETRIES {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "outer display kept changing during scene commit",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("display commit retry loop has a nonzero fixed bound")
    }

    fn reconcile_nodes_once(
        &mut self,
        nodes: &[BridgeNode],
        next_node_ids: &HashMap<(u64, u64, u8), u64>,
    ) -> io::Result<()> {
        self.next_transaction = self
            .next_transaction
            .checked_add(1)
            .ok_or_else(|| exhausted("transaction"))?;
        let transaction = self.next_transaction;
        let begin_request = self.request_id()?;
        self.control.write_record(
            messages::BEGIN_TXN,
            0,
            0,
            &messages::begin_transaction(begin_request, transaction),
        )?;

        let mut mutation_requests = Vec::new();
        for node in nodes {
            let stable_key = (node.producer, node.node, node.fragment);
            let node_id = next_node_ids[&stable_key];
            let record_type = if self.node_ids.contains_key(&stable_key) {
                messages::UPDATE_NODE
            } else {
                messages::CREATE_NODE
            };
            let source_id = *self
                .source_ids
                .get(&node.source)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "node source missing"))?;
            let request = self.request_id()?;
            let body = messages::create_scene_node(
                request,
                transaction,
                &SceneNodeConfig {
                    node_id,
                    source_id,
                    context_id: self.root_context,
                    x: node.x,
                    y: node.y,
                    width: node.width,
                    height: node.height,
                    text_layer: messages::TEXT_LAYER_BETWEEN_BACKGROUND_AND_GLYPH,
                    z_index: node.z_index,
                    visible: node.visible,
                    anchor_id: None,
                    clip: Some(ClipRect {
                        x: node.clip.x,
                        y: node.clip.y,
                        width: node.clip.width,
                        height: node.clip.height,
                    }),
                },
            );
            self.control.write_record(record_type, 0, node_id, &body)?;
            mutation_requests.push((request, node_id));
        }

        let removed = self
            .node_ids
            .iter()
            .filter(|(stable_key, _)| !next_node_ids.contains_key(stable_key))
            .map(|(_, node_id)| *node_id)
            .collect::<Vec<_>>();
        for node_id in removed {
            let request = self.request_id()?;
            self.control.write_record(
                messages::DELETE_NODE,
                0,
                node_id,
                &messages::delete_node(request, transaction, node_id),
            )?;
            mutation_requests.push((request, node_id));
        }

        let commit_request = self.request_id()?;
        self.control.write_record(
            messages::COMMIT_TXN,
            0,
            0,
            &messages::commit_transaction(
                commit_request,
                transaction,
                self.control.display_generation(),
            ),
        )?;

        self.wait_for(begin_request, messages::OK, 0)?;
        for (request, node_id) in mutation_requests {
            self.wait_for(request, messages::OK, node_id)?;
        }
        match self.wait_for(commit_request, messages::PRESENTED, 0) {
            Ok(()) => Ok(()),
            Err(error)
                if protocol_error_code(&error)
                    == Some(messages::ERROR_STALE_DISPLAY_GENERATION) =>
            {
                let abort_request = self.request_id()?;
                self.control.write_record(
                    messages::ABORT_TXN,
                    0,
                    0,
                    &messages::abort_transaction(abort_request, transaction),
                )?;
                self.wait_for(abort_request, messages::OK, 0)?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    fn play_source(&mut self, source: &BridgeSource) -> io::Result<()> {
        if !matches!(
            source.kind,
            BridgeSourceKind::Video { .. } | BridgeSourceKind::Audio { .. }
        ) {
            return Ok(());
        }
        let upstream = *self
            .source_ids
            .get(&source.key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "playback source missing"))?;
        let request = self.request_id()?;
        let minimum_buffer_us = self
            .control
            .adjusted_minimum_buffer(source.play_request.minimum_buffer_us);
        let body = messages::play_request(
            request,
            &messages::PlayRequest {
                source_id: upstream,
                start_pts_us: source.play_request.start_pts_us,
                minimum_buffer_us,
                maximum_latency_us: source
                    .play_request
                    .maximum_latency_us
                    .max(minimum_buffer_us),
                rate_32_32: source.play_request.rate_32_32,
                late_policy: source.play_request.late_policy,
                loop_count: source.play_request.loop_count,
                start_policy: source.play_request.start_policy,
            },
        );
        self.control.write_record(
            messages::PLAY,
            0,
            upstream,
            &with_causation(&body, source.causation_id)?,
        )?;
        self.wait_for(request, messages::OK, upstream)
    }

    /// Close the outer epoch for a source whose inner ingress has ended.
    ///
    /// `EOS` closes ingress without pausing: already-buffered media keeps playing, and the outer
    /// presenter reports `MILESTONE_PLAYBACK_ENDED` once its queue drains. Skipping this leaves an
    /// inner producer waiting on `WAIT_PLAYBACK_ENDED` for a milestone that can never arrive.
    fn end_source(&mut self, source: &BridgeSource) -> io::Result<()> {
        let Some(epoch) = source.eos_epoch else {
            return Ok(());
        };
        if !matches!(
            source.kind,
            BridgeSourceKind::Video { .. } | BridgeSourceKind::Audio { .. }
        ) {
            return Ok(());
        }
        let upstream = *self
            .source_ids
            .get(&source.key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ended source missing"))?;
        let request = self.request_id()?;
        let body = if self.media_order_barrier {
            let media = self.media.get(&source.key).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "ended source media channel missing",
                )
            })?;
            let (reply, sequence) = mpsc::sync_channel(1);
            media
                .sender
                .send(MediaWriterCommand::Barrier(reply))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "media writer stopped"))?;
            let final_record_sequence = sequence.recv().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "media writer did not report sequence",
                )
            })?;
            let attachment_generation = *self
                .outer_attachment_generations
                .get(&source.key)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "ended source attachment generation missing",
                    )
                })?;
            if final_record_sequence > 1 {
                messages::eos_with_barrier(
                    request,
                    upstream,
                    epoch,
                    attachment_generation,
                    final_record_sequence,
                )
            } else {
                messages::eos(request, upstream, epoch)
            }
        } else {
            messages::eos(request, upstream, epoch)
        };
        self.control.write_record(
            messages::EOS,
            0,
            upstream,
            &with_causation(&body, source.causation_id)?,
        )?;
        self.wait_for(request, messages::OK, upstream)
    }

    fn pause_source(&mut self, source: &BridgeSource) -> io::Result<()> {
        if !matches!(
            source.kind,
            BridgeSourceKind::Video { .. } | BridgeSourceKind::Audio { .. }
        ) {
            return Ok(());
        }
        let upstream = *self
            .source_ids
            .get(&source.key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "playback source missing"))?;
        let request = self.request_id()?;
        let body = messages::pause(request, upstream);
        self.control.write_record(
            messages::PAUSE,
            0,
            upstream,
            &with_causation(&body, source.causation_id)?,
        )?;
        self.wait_for(request, messages::OK, upstream)
    }

    fn wait_source_ready(
        &self,
        request_id: u64,
        source_id: u64,
        timeout: Duration,
    ) -> io::Result<messages::SourceReady> {
        let record = self.control.wait_reply_with_timeout(
            request_id,
            messages::SOURCE_READY,
            source_id,
            timeout,
        )?;
        let ready = messages::parse_source_ready(&record.body)?;
        if record.object_id != source_id || ready.source_id != source_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "outer SOURCE_READY object/source ID mismatch",
            ));
        }
        Ok(ready)
    }

    fn wait_for(&self, request_id: u64, expected: u16, object_id: u64) -> io::Result<()> {
        self.control
            .wait_reply(request_id, expected, object_id)
            .map(|_| ())
    }

    fn request_id(&mut self) -> io::Result<u64> {
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or_else(|| exhausted("request"))?;
        Ok(self.next_request)
    }
}

impl Drop for OuterBridge {
    fn drop(&mut self) {
        // The control dispatcher owns cloned socket halves on its reader and heartbeat threads, so
        // dropping the foreground writer alone does not close the transport. End the protocol
        // session explicitly: Vivido can then remove this bridge's scene before a reattached
        // client creates its replacement session, and the peer observes a clean close rather than
        // a later connection reset.
        let Ok(request) = self.request_id() else {
            return;
        };
        let body = messages::goodbye(request);
        if self
            .control
            .write_record(messages::GOODBYE, 0, 0, &body)
            .is_ok()
        {
            let _ = self.wait_for(request, messages::OK, 0);
        }
    }
}

fn bridge_source_pixels(source: &BridgeSource) -> u64 {
    match &source.kind {
        BridgeSourceKind::Raster { width, height, .. }
        | BridgeSourceKind::Image { width, height, .. }
        | BridgeSourceKind::Video { width, height, .. } => u64::from(*width) * u64::from(*height),
        BridgeSourceKind::Audio { .. } => 0,
    }
}

/// One inner raster body re-encoded for this hop.
struct ReoriginatedRaster {
    body: Vec<u8>,
    /// Chain state to commit once the body is accepted by the media writer, or `None` when this
    /// source does not maintain a chain.
    chain: Option<RasterChain>,
}

/// Damaged pixels described by a delta's operations, used for this hop's own damage budget.
fn raster_damage_pixels(operations: &[media::ParsedRasterDeltaOperation<'_>]) -> io::Result<u64> {
    operations.iter().try_fold(0_u64, |total, operation| {
        let (width, height) = match operation {
            media::ParsedRasterDeltaOperation::Overwrite { width, height, .. }
            | media::ParsedRasterDeltaOperation::Copy { width, height, .. } => (*width, *height),
        };
        u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|area| total.checked_add(area))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "raster damage area overflows")
            })
    })
}

fn presenter_display_metrics(
    grid_columns: u64,
    grid_rows: u64,
    cell_width: u32,
    cell_height: u32,
) -> io::Result<DisplayMetrics> {
    let columns = u16::try_from(grid_columns)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "outer grid width exceeds u16"))?;
    let rows = u16::try_from(grid_rows)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "outer grid height exceeds u16"))?;
    let cell_width = u16::try_from(cell_width)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "outer cell width exceeds u16"))?;
    let cell_height = u16::try_from(cell_height)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "outer cell height exceeds u16"))?;
    if columns == 0 || rows == 0 || cell_width == 0 || cell_height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "outer presenter reported zero display geometry",
        ));
    }
    Ok(DisplayMetrics {
        columns,
        rows,
        cell_width,
        cell_height,
    })
}

/// State of one outgoing raster delta chain.
///
/// The chain is entirely vvmux's own: `base_frame_id` is the identity this hop last wrote and had
/// accepted into its media writer, never an identity from the inner hop. `damage_pixels` enforces
/// this hop's own accumulated-damage budget, because the inner hop's budget bounds a different
/// stream of frames (specification 11.4).
#[derive(Debug, Clone, Copy)]
struct RasterChain {
    epoch: u32,
    base_frame_id: u64,
    damage_pixels: u64,
    damage_window_started: Instant,
}

/// Frame-equivalents of damage tolerated per window before a full frame is preferred, and the
/// window length. Matching the inner presenter's policy keeps the two hops from oscillating.
const RASTER_DAMAGE_FRAME_EQUIVALENTS: u64 = 4;
const RASTER_DAMAGE_INTERVAL: Duration = Duration::from_secs(1);

/// Re-encode an inner delta onto this hop's chain, or `None` when it cannot be chained.
///
/// Returning `None` is not an error: the caller falls back to requesting a full frame, which is
/// the recovery the specification defines for a delta whose base is unavailable.
fn reoriginated_delta_raster(
    body: &[u8],
    width: u32,
    height: u32,
    operation_limit: u32,
    chain: &RasterChain,
    frame_id: u64,
    compress: bool,
) -> io::Result<Option<Vec<u8>>> {
    let frame = media::parse_delta_raster_frame(body, width, height, operation_limit)?;
    // A chain carries within one epoch only. An epoch change restarts from a full frame.
    if chain.base_frame_id == 0 || frame.epoch != chain.epoch {
        return Ok(None);
    }
    let operations = frame
        .operations
        .iter()
        .map(|operation| match operation {
            media::ParsedRasterDeltaOperation::Overwrite {
                x,
                y,
                width,
                height,
                rgba,
            } => media::RasterDeltaOperation::Overwrite {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
                rgba: rgba.as_ref(),
            },
            media::ParsedRasterDeltaOperation::Copy {
                destination_x,
                destination_y,
                width,
                height,
                source_x,
                source_y,
            } => media::RasterDeltaOperation::Copy {
                destination_x: *destination_x,
                destination_y: *destination_y,
                width: *width,
                height: *height,
                source_x: *source_x,
                source_y: *source_y,
            },
        })
        .collect::<Vec<_>>();
    media::raster_delta_frame_body(
        frame.epoch,
        frame_id,
        chain.base_frame_id,
        frame.pts_us,
        frame.duration_us,
        width,
        height,
        operation_limit,
        &operations,
        compress,
    )
    .map(Some)
}

fn reoriginated_full_raster(body: &[u8], frame_id: u64) -> io::Result<Vec<u8>> {
    if frame_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "outer raster frame ID is zero",
        ));
    }
    media::parse_full_raster_frame(body)?;
    let mut body = body.to_vec();
    body[8..16].copy_from_slice(&frame_id.to_be_bytes());
    Ok(body)
}

fn bridge_source_media_body(source: &BridgeSource) -> u64 {
    let length = match &source.kind {
        BridgeSourceKind::Raster { width, height, .. } => {
            vivid_protocol::media::rgba8_raw_frame_body_len(*width, *height)
        }
        BridgeSourceKind::Image { encoded_length, .. } => Ok(*encoded_length),
        BridgeSourceKind::Video {
            max_access_unit_bytes,
            ..
        } => vivid_protocol::media::video_body_len(*max_access_unit_bytes),
        BridgeSourceKind::Audio {
            max_access_unit_bytes,
            ..
        } => vivid_protocol::media::audio_body_len(*max_access_unit_bytes),
    };
    length.map(u64::from).unwrap_or(u64::MAX)
}

fn protocol_descriptor(descriptor: &BridgeSourceDescriptor) -> messages::SourceDescriptor {
    messages::SourceDescriptor {
        role: descriptor.role,
        title: descriptor.title.clone(),
        content_revision: descriptor.content_revision,
        semantic_availability: descriptor.semantic_availability,
        locator: descriptor.locator.clone(),
    }
}

fn validate_snapshot(sources: &[BridgeSource], nodes: &[BridgeNode]) -> io::Result<()> {
    let sources = sources
        .iter()
        .map(|source| messages::SceneValidationSource {
            key: messages::SceneValidationKey {
                owner_id: source.key.producer,
                object_id: source.key.source,
            },
            is_video: matches!(source.kind, BridgeSourceKind::Video { .. }),
            linked_video: match source.kind {
                BridgeSourceKind::Audio { linked_video, .. } => {
                    linked_video.map(|video| messages::SceneValidationKey {
                        owner_id: video.producer,
                        object_id: video.source,
                    })
                }
                _ => None,
            },
        })
        .collect::<Vec<_>>();
    let nodes = nodes
        .iter()
        .map(|node| messages::SceneValidationNode {
            owner_id: node.producer,
            node_id: node.node,
            fragment_id: u64::from(node.fragment),
            source: messages::SceneValidationKey {
                owner_id: node.source.producer,
                object_id: node.source.source,
            },
            x: node.x,
            y: node.y,
            width: node.width,
            height: node.height,
            clip: Some(ClipRect {
                x: node.clip.x,
                y: node.clip.y,
                width: node.clip.width,
                height: node.clip.height,
            }),
        })
        .collect::<Vec<_>>();
    messages::validate_scene_snapshot(&sources, &nodes)
}

fn run_media_writer(
    mut connection: Connection,
    shared: Arc<SharedControl>,
    receiver: mpsc::Receiver<MediaWriterCommand>,
    completions: mpsc::Sender<MediaCompletion>,
) {
    let mut last_record_sequence = 1;
    while let Ok(command) = receiver.recv() {
        let write = match command {
            MediaWriterCommand::Write(write) => write,
            MediaWriterCommand::Barrier(reply) => {
                let _ = reply.send(last_record_sequence);
                continue;
            }
        };
        let written = reserve_outer_credit(&shared, write.object_id, write.body.len() as u64)
            .and_then(|()| {
                connection.write_record_parts(
                    write.record_type,
                    0,
                    write.object_id,
                    &[write.body.as_slice()],
                )
            });
        let delivered = written.is_ok();
        let record_sequence = written.unwrap_or(0);
        if delivered {
            last_record_sequence = record_sequence;
        }
        if completions
            .send(MediaCompletion {
                delivery_id: write.delivery_id,
                delivered,
                record_sequence,
                object_id: write.object_id,
            })
            .is_err()
        {
            break;
        }
        if !delivered {
            break;
        }
    }
}

/// Await one correlated outer reply, or fail at the supplied operation-specific deadline.
///
/// Free-standing so the deadline can be exercised against control state alone, without standing up
/// a transport.
fn wait_reply_on(
    shared: &Arc<SharedControl>,
    request_id: u64,
    expected: u16,
    expected_object_id: u64,
    timeout: Duration,
) -> io::Result<Record> {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if let Some(record) = state.replies.remove(&request_id) {
            state.wait_us = state
                .wait_us
                .saturating_add(started.elapsed().as_micros() as u64);
            if record.object_id != expected_object_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "outer reply {request_id} has object {}, expected {expected_object_id}",
                        record.object_id
                    ),
                ));
            }
            if record.record_type == messages::ERROR {
                return Err(protocol_error(&record.body));
            }
            if record.record_type != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "outer reply {} was {}, expected {}",
                        request_id,
                        messages::name(record.record_type),
                        messages::name(expected)
                    ),
                ));
            }
            return Ok(record);
        }
        if let Some(error) = &state.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
        }
        // An unbounded wait here is what turned one missing reply into a permanently frozen
        // bridge: the worker is single-threaded, so it also stops forwarding media and applying
        // projections. Failing instead lets the existing snapshot-retry and replacement-session
        // recovery run.
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            state.pending_requests.remove(&request_id);
            if state.abandoned_requests.len() < MAX_PENDING_CONTROL_REPLIES {
                state.abandoned_requests.insert(request_id);
            }
            if state.pending_requests.is_empty() {
                state.oldest_pending_request = None;
            }
            state.wait_us = state
                .wait_us
                .saturating_add(started.elapsed().as_micros() as u64);
            state.wait_timeouts = state.wait_timeouts.saturating_add(1);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("outer reply {request_id} did not arrive within the deadline"),
            ));
        };
        let (next, _) = shared
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
    }
}

fn source_ready_timeout(is_video: bool) -> Duration {
    if is_video {
        SOURCE_READY_REPLY_TIMEOUT
    } else {
        CONTROL_REPLY_TIMEOUT
    }
}

fn reserve_outer_credit(shared: &SharedControl, source_id: u64, bytes: u64) -> io::Result<()> {
    let deadline = Instant::now() + OUTER_CREDIT_TIMEOUT;
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        let ledger = state.credits.get_mut(&source_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "outer source has no credit ledger",
            )
        })?;
        if ledger.is_lost() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "outer source was lost",
            ));
        }
        if ledger.can_consume(bytes) {
            ledger.consume(bytes)?;
            return Ok(());
        }
        if let Some(error) = &state.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.clone()));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "outer source did not grant credit within the deadline",
            ));
        };
        let (next, _) = shared
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
    }
}

fn protocol_error(body: &[u8]) -> io::Error {
    match messages::parse_error_reply(body) {
        Ok(error) => io::Error::other(OuterProtocolError {
            code: error.code,
            diagnostic: error.diagnostic,
        }),
        Err(_) => io::Error::other("outer Vivid error"),
    }
}

fn protocol_error_code(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<OuterProtocolError>())
        .map(|error| error.code)
}

fn with_causation(
    body: &[u8],
    causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
) -> io::Result<Vec<u8>> {
    let Some(causation_id) = causation_id else {
        return Ok(body.to_vec());
    };
    messages::with_request_metadata(
        body,
        &messages::RequestMetadata {
            preconditions: Default::default(),
            idempotency_key: None,
            causation_id: Some(causation_id),
        },
    )
}

fn exhausted(kind: &'static str) -> io::Error {
    io::Error::other(format!("outer Vivid {kind} ID space exhausted"))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::HashSet;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    #[cfg(unix)]
    use crate::config::Media as MediaConfig;
    #[cfg(unix)]
    use crate::media::VirtualVivid;
    use vivid_protocol::wire::{HEADER_SIZE, PREFACE_SIZE, RecordHeader};

    #[test]
    fn raster_frames_are_reoriginated_as_full_with_hop_local_identity() {
        let inner = media::raster_frame_body(7, 99, 2, 1, &[1, 2, 3, 255, 4, 5, 6, 255]).unwrap();
        let outer = reoriginated_full_raster(&inner, 1).unwrap();
        let parsed = media::parse_full_raster_frame(&outer).unwrap();
        assert_eq!(parsed.epoch, 7);
        assert_eq!(parsed.frame_id, 1);
        assert_eq!(&outer[16..24], &[0; 8]);
        assert_eq!(
            media::decode_raster_pixels(parsed).unwrap(),
            [1, 2, 3, 255, 4, 5, 6, 255]
        );

        let delta = media::raster_delta_frame_body(
            7,
            100,
            99,
            0,
            0,
            2,
            1,
            1,
            &[media::RasterDeltaOperation::Overwrite {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: &[0, 0, 0, 255],
            }],
            false,
        )
        .unwrap();
        assert!(
            reoriginated_full_raster(&delta, 2).is_err(),
            "the outer hop must never forward a foreign delta base"
        );
    }

    /// Specification 11.4 "Nesting" and 16.3 rule 5: a nested presenter terminates the inner delta
    /// chain and re-encodes on its own. The operations survive; the identities must not.
    #[test]
    fn re_originated_deltas_carry_hop_local_identities_and_never_the_inner_base() {
        let inner_base = 4_242;
        let inner_frame = 4_243;
        let inner = media::raster_delta_frame_body(
            7,
            inner_frame,
            inner_base,
            10_000,
            16_000,
            4,
            2,
            4,
            &[
                media::RasterDeltaOperation::Overwrite {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    rgba: &[9, 8, 7, 255],
                },
                media::RasterDeltaOperation::Copy {
                    destination_x: 2,
                    destination_y: 1,
                    width: 2,
                    height: 1,
                    source_x: 0,
                    source_y: 0,
                },
            ],
            false,
        )
        .unwrap();

        let chain = RasterChain {
            epoch: 7,
            base_frame_id: 11,
            damage_pixels: 0,
            damage_window_started: Instant::now(),
        };
        let outer = reoriginated_delta_raster(&inner, 4, 2, 4, &chain, 12, false)
            .unwrap()
            .expect("a chained delta re-originates");
        let parsed = media::parse_delta_raster_frame(&outer, 4, 2, 4).unwrap();
        assert_eq!(parsed.frame_id, 12);
        assert_eq!(parsed.base_frame_id, 11);
        assert_ne!(parsed.frame_id, inner_frame);
        assert_ne!(parsed.base_frame_id, inner_base);
        assert_eq!(parsed.epoch, 7);
        assert_eq!(parsed.pts_us, 10_000);
        assert_eq!(parsed.operations.len(), 2);

        // No base of its own yet: the chain cannot be extended and the caller must fall back.
        let unstarted = RasterChain {
            base_frame_id: 0,
            ..chain
        };
        assert!(
            reoriginated_delta_raster(&inner, 4, 2, 4, &unstarted, 12, false)
                .unwrap()
                .is_none()
        );

        // An epoch change restarts from a full frame rather than chaining across it.
        let other_epoch = RasterChain { epoch: 8, ..chain };
        assert!(
            reoriginated_delta_raster(&inner, 4, 2, 4, &other_epoch, 12, false)
                .unwrap()
                .is_none()
        );
    }

    /// A presenter that keeps sending unsolicited records while never answering a request used to
    /// hang the bridge worker forever: `wait_reply` had no deadline and the heartbeat treated any
    /// inbound record as proof of life. The worker is single-threaded, so that also froze media
    /// forwarding and projection — the persistent form of the reported stall.
    #[test]
    fn a_reply_that_never_arrives_fails_instead_of_hanging_the_worker() {
        let shared = Arc::new(SharedControl {
            state: Mutex::new(ControlState::default()),
            changed: Condvar::new(),
        });

        // A live connection: unsolicited traffic keeps arriving for the whole wait, so
        // `last_inbound` never goes stale and the heartbeat sees a healthy peer.
        let chatter = shared.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let chatter_stop = stop.clone();
        let chatter_thread = thread::spawn(move || {
            while !chatter_stop.load(std::sync::atomic::Ordering::Acquire) {
                chatter
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .last_inbound = Instant::now();
                chatter.changed.notify_all();
                thread::sleep(Duration::from_millis(10));
            }
        });

        shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending_requests
            .insert(77);
        let started = Instant::now();
        let outcome = wait_reply_on(&shared, 77, messages::OK, 0, CONTROL_REPLY_TIMEOUT);
        let elapsed = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Release);
        chatter_thread.join().unwrap();

        let error = match outcome {
            Ok(_) => panic!("an unanswered request must not wait forever"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed >= CONTROL_REPLY_TIMEOUT && elapsed < CONTROL_REPLY_TIMEOUT * 3,
            "waited {elapsed:?}, expected about {CONTROL_REPLY_TIMEOUT:?}"
        );

        let state = shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(!state.pending_requests.contains(&77));
        // A late reply for an abandoned request is discarded, not treated as a reply to something
        // that was never sent, which would close an otherwise healthy connection.
        assert!(state.abandoned_requests.contains(&77));
        assert_eq!(state.wait_timeouts, 1);
        assert!(state.wait_us > 0);
    }

    #[test]
    fn source_creation_allows_bounded_browser_codec_initialization() {
        assert_eq!(source_ready_timeout(true), SOURCE_READY_REPLY_TIMEOUT);
        assert_eq!(source_ready_timeout(false), CONTROL_REPLY_TIMEOUT);
        assert!(SOURCE_READY_REPLY_TIMEOUT > CONTROL_REPLY_TIMEOUT);
        assert!(SOURCE_READY_REPLY_TIMEOUT < OUTER_CREDIT_TIMEOUT * 10);
    }

    #[test]
    fn outer_capability_changes_must_strictly_advance() {
        let mut state = ControlState::default();
        let changed = messages::CapsChanged {
            capability_generation: 3,
            reason_mask: messages::CAPS_CHANGE_DEVICE_AVAILABILITY,
        };
        apply_capability_change(&mut state, changed).unwrap();
        assert_eq!(state.capability_generation, 3);
        assert_eq!(state.capability_changes, vec![changed]);
        assert!(apply_capability_change(&mut state, changed).is_err());
        assert_eq!(state.capability_changes, vec![changed]);
    }

    fn test_raster_source() -> BridgeSource {
        BridgeSource {
            key: BridgeSourceKey {
                producer: 3,
                source: 4,
            },
            kind: BridgeSourceKind::Raster {
                width: 16,
                height: 16,
                alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
                delta_operation_limit: None,
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 0,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: vivid_protocol::messages::LATE_DROP_PRESENTATION,
                loop_count: 0,
                start_policy: vivid_protocol::messages::START_AFTER_MINIMUM_BUFFER,
            },
        }
    }

    #[test]
    fn pane_projection_ipc_has_no_outer_capability_field_or_bytes() {
        let mut source = test_raster_source();
        source.capture_policy = messages::CAPTURE_POLICY_DENY_CAPTURE;
        let capability = [0xa5_u8; messages::CONTEXT_CAPABILITY_BYTES];
        let encoded = serde_json::to_vec(&source).unwrap();
        assert!(
            !encoded
                .windows(capability.len())
                .any(|window| window == capability)
        );
        let decoded: BridgeSource = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            decoded.capture_policy,
            messages::CAPTURE_POLICY_DENY_CAPTURE
        );
        let text = String::from_utf8(encoded).unwrap();
        assert!(!text.contains("capability"));
        assert!(!text.contains("token"));
    }

    #[test]
    fn nested_bridge_preserves_causation_without_deriving_it_from_credentials() {
        let causation_id = [0xa5; messages::CAUSATION_ID_BYTES];
        let body = messages::destroy_source(7, 9);
        let forwarded = with_causation(&body, Some(causation_id)).unwrap();
        let envelope = messages::decode_control(&forwarded).unwrap();
        assert_eq!(envelope.causation_id, Some(causation_id));
        assert!(envelope.idempotency_key.is_none());
        assert!(envelope.preconditions.is_empty());
    }

    fn test_fragment(logical: u64, fragment: u8, x: i64) -> BridgeNode {
        BridgeNode {
            producer: 3,
            node: logical,
            fragment,
            source: BridgeSourceKey {
                producer: 3,
                source: 4,
            },
            x,
            y: 0,
            width: 8_i64 << 32,
            height: 4_i64 << 32,
            z_index: 0,
            visible: true,
            clip: crate::ipc::BridgeClipRect {
                x,
                y: 0,
                width: 4_i64 << 32,
                height: 4_i64 << 32,
            },
        }
    }

    #[test]
    fn snapshot_validation_rejects_fragment_aliases_limits_and_bad_sources() {
        let source = test_raster_source();
        let fragment = test_fragment(1, 0, 0);
        validate_snapshot(
            std::slice::from_ref(&source),
            std::slice::from_ref(&fragment),
        )
        .unwrap();
        assert!(
            validate_snapshot(
                std::slice::from_ref(&source),
                &[fragment.clone(), fragment.clone()]
            )
            .is_err()
        );
        let nine = (0..9)
            .map(|id| test_fragment(1, id, i64::from(id) << 32))
            .collect::<Vec<_>>();
        assert!(validate_snapshot(std::slice::from_ref(&source), &nine).is_err());
        let mut foreign = fragment.clone();
        foreign.source.source = 99;
        assert!(validate_snapshot(std::slice::from_ref(&source), &[foreign]).is_err());
        let mut invalid = fragment;
        invalid.clip.width = 0;
        assert!(validate_snapshot(std::slice::from_ref(&source), &[invalid]).is_err());

        let mut maximum = (0..256)
            .map(|logical| test_fragment(logical, 0, i64::try_from(logical).unwrap() << 32))
            .collect::<Vec<_>>();
        validate_snapshot(std::slice::from_ref(&source), &maximum).unwrap();
        maximum.push(test_fragment(256, 0, 256_i64 << 32));
        assert!(validate_snapshot(&[source], &maximum).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn fragment_reconciliation_uses_one_source_and_monotonic_node_ids() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-fragments.sock");
        let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping fragment bridge socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual outer presenter start failed: {error}"),
        };
        let token = presenter.issue_pane_capability(7).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));
        let mut bridge = OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new(token),
            DisplayMetrics::default(),
        )
        .unwrap();
        let source = test_raster_source();
        bridge
            .rebuild(
                std::slice::from_ref(&source),
                &[test_fragment(9, 0, 0), test_fragment(9, 1, 5_i64 << 32)],
            )
            .unwrap();
        let first_id = bridge.node_ids[&(3, 9, 0)];
        let removed_id = bridge.node_ids[&(3, 9, 1)];
        let next_node_after_create = bridge.next_node;
        assert_ne!(first_id, removed_id);
        let outer = presenter.projection_snapshot(&HashSet::from([7]));
        assert_eq!(
            outer.sources.len(),
            1,
            "fragments share one upstream source"
        );
        assert_eq!(outer.nodes.len(), 2);

        for step in 1..=32_i64 {
            bridge
                .update_nodes(&[
                    test_fragment(9, 0, step << 32),
                    test_fragment(9, 1, (step + 5) << 32),
                ])
                .unwrap();
            assert_eq!(bridge.node_ids.len(), 2);
            assert_eq!(bridge.next_node, next_node_after_create);
        }

        bridge
            .update_nodes(&[
                test_fragment(9, 0, 2_i64 << 32),
                test_fragment(9, 2, 7_i64 << 32),
            ])
            .unwrap();
        assert_eq!(bridge.node_ids[&(3, 9, 0)], first_id);
        assert!(!bridge.node_ids.contains_key(&(3, 9, 1)));
        assert!(bridge.node_ids[&(3, 9, 2)] > removed_id);
        let outer = presenter.projection_snapshot(&HashSet::from([7]));
        assert_eq!(outer.sources.len(), 1);
        assert_eq!(outer.nodes.len(), 2);

        bridge.update_nodes(&[]).unwrap();
        let outer = presenter.projection_snapshot(&HashSet::from([7]));
        assert!(outer.nodes.is_empty());
        assert_eq!(
            outer.sources.len(),
            1,
            "deleting the last fragment leaves the source alive"
        );
        bridge.rebuild(&[], &[]).unwrap();
        assert!(
            presenter
                .projection_snapshot(&HashSet::from([7]))
                .sources
                .is_empty()
        );
    }

    #[test]
    fn outer_presenter_metrics_replace_client_console_font_metrics() {
        let display = presenter_display_metrics(120, 42, 11, 23).unwrap();
        assert_eq!(
            display,
            DisplayMetrics {
                columns: 120,
                rows: 42,
                cell_width: 11,
                cell_height: 23,
            }
        );
        assert!(presenter_display_metrics(u64::from(u16::MAX) + 1, 42, 11, 23).is_err());
        assert!(presenter_display_metrics(120, 42, 0, 23).is_err());
    }

    #[cfg(unix)]
    use vivid_protocol::media::{self, VideoPacket};
    fn read_client_record(stream: &mut impl Read) -> io::Result<Record> {
        let mut header = [0; HEADER_SIZE];
        stream.read_exact(&mut header)?;
        let header = RecordHeader::decode(header);
        let mut body = vec![0; header.body_length as usize];
        stream.read_exact(&mut body)?;
        Ok(Record {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    fn write_server_record(
        stream: &mut impl Write,
        sequence: &mut u64,
        record_type: u16,
        object_id: u64,
        body: &[u8],
    ) -> io::Result<()> {
        *sequence += 1;
        stream.write_all(
            &RecordHeader {
                body_length: body.len() as u32,
                record_type,
                flags: 0,
                object_id,
                sequence: *sequence,
            }
            .encode(),
        )?;
        stream.write_all(body)?;
        stream.flush()
    }

    #[test]
    fn dropping_bridge_worker_finishes_goodbye_before_returning() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (goodbye_sender, goodbye_receiver) = mpsc::sync_channel(1);
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut control, _) = listener.accept()?;
            control.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut preface = [0; PREFACE_SIZE];
            control.read_exact(&mut preface)?;
            let hello = read_client_record(&mut control)?;
            let request = messages::request_id(&hello.body)?;
            let mut sequence = 0;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::WELCOME,
                0,
                &messages::welcome(
                    request,
                    1,
                    &[1; 16],
                    1,
                    messages::DisplayChanged {
                        display_generation: 1,
                        viewport_width: 800,
                        viewport_height: 600,
                        grid_columns: 80,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                    REQUIRED_FEATURES,
                ),
            )?;

            let goodbye = read_client_record(&mut control)?;
            assert_eq!(goodbye.record_type, messages::GOODBYE);
            assert_eq!(goodbye.object_id, 0);
            let request = messages::request_id(&goodbye.body)?;
            goodbye_sender.send(()).unwrap();
            reply_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
            write_server_record(
                &mut control,
                &mut sequence,
                messages::OK,
                0,
                &messages::ok(request),
            )
        });

        let bridge = OuterBridge::connect(
            format!("tcp:{address}"),
            Zeroizing::new("11".repeat(32)),
            DisplayMetrics::default(),
        )
        .unwrap();
        assert_eq!(
            bridge.display_metrics(),
            DisplayMetrics {
                columns: 80,
                rows: 24,
                cell_width: 10,
                cell_height: 25,
            },
            "the bridge must use the outer presenter's pixels, not client console font metrics"
        );

        let worker = crate::client::BridgeWorker::spawn_with_sender(
            bridge,
            crate::client::BridgeClientSender::new(|_| Ok(())),
            1,
        )
        .unwrap();
        let (dropped_sender, dropped_receiver) = mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(worker);
            dropped_sender.send(()).unwrap();
        });
        goodbye_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(
            matches!(dropped_receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "bridge worker drop returned before the outer GOODBYE completed"
        );
        reply_sender.send(()).unwrap();
        dropped_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        dropper.join().unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn stale_display_commit_aborts_and_retries_on_the_same_session() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut control, _) = listener.accept()?;
            control.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut preface = [0; PREFACE_SIZE];
            control.read_exact(&mut preface)?;
            let hello = read_client_record(&mut control)?;
            let mut sequence = 0;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::WELCOME,
                0,
                &messages::welcome(
                    messages::request_id(&hello.body)?,
                    1,
                    &[1; 16],
                    1,
                    messages::DisplayChanged {
                        display_generation: 1,
                        viewport_width: 800,
                        viewport_height: 600,
                        grid_columns: 80,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                    REQUIRED_FEATURES,
                ),
            )?;

            let begin = read_client_record(&mut control)?;
            let commit = read_client_record(&mut control)?;
            assert_eq!(begin.record_type, messages::BEGIN_TXN);
            assert_eq!(commit.record_type, messages::COMMIT_TXN);
            write_server_record(
                &mut control,
                &mut sequence,
                messages::OK,
                0,
                &messages::ok(messages::request_id(&begin.body)?),
            )?;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::DISPLAY_CHANGED,
                0,
                &messages::display_changed(
                    0,
                    messages::DisplayChanged {
                        display_generation: 2,
                        viewport_width: 810,
                        viewport_height: 600,
                        grid_columns: 81,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                ),
            )?;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::ERROR,
                0,
                &messages::error(
                    messages::request_id(&commit.body)?,
                    messages::ERROR_STALE_DISPLAY_GENERATION,
                    "display generation is stale",
                ),
            )?;

            let abort = read_client_record(&mut control)?;
            assert_eq!(abort.record_type, messages::ABORT_TXN);
            write_server_record(
                &mut control,
                &mut sequence,
                messages::OK,
                0,
                &messages::ok(messages::request_id(&abort.body)?),
            )?;

            let retried_begin = read_client_record(&mut control)?;
            let retried_commit = read_client_record(&mut control)?;
            assert_eq!(retried_begin.record_type, messages::BEGIN_TXN);
            assert_eq!(retried_commit.record_type, messages::COMMIT_TXN);
            assert_eq!(
                messages::decode_control(&retried_commit.body)?.expected_generation,
                Some(2),
                "the retry must use the DISPLAY_CHANGED generation sent before the stale error"
            );
            write_server_record(
                &mut control,
                &mut sequence,
                messages::OK,
                0,
                &messages::ok(messages::request_id(&retried_begin.body)?),
            )?;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::PRESENTED,
                0,
                &messages::presented(
                    messages::request_id(&retried_commit.body)?,
                    SceneRevision::new(1),
                ),
            )?;

            let goodbye = read_client_record(&mut control)?;
            assert_eq!(goodbye.record_type, messages::GOODBYE);
            write_server_record(
                &mut control,
                &mut sequence,
                messages::OK,
                0,
                &messages::ok(messages::request_id(&goodbye.body)?),
            )
        });

        let mut bridge = OuterBridge::connect(
            format!("tcp:{address}"),
            Zeroizing::new("11".repeat(32)),
            DisplayMetrics::default(),
        )
        .unwrap();
        let started = Instant::now();
        bridge.update_nodes(&[]).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a stale display generation must not incur session-replacement latency"
        );
        drop(bridge);
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn outer_negotiation_emits_and_retains_preserved_fields() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-negotiation-preservation.sock");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping outer negotiation preservation socket test: {error}");
                return;
            }
            Err(error) => panic!("fake outer presenter bind failed: {error}"),
        };
        let hello_extensions = vec![PreservedField {
            key: 42,
            encoded_value: vec![0x82, 0x01, 0xf5],
        }];
        let welcome_extensions = vec![PreservedField {
            key: 43,
            encoded_value: vec![0xa1, 0x00, 0x07],
        }];
        let expected_hello = hello_extensions.clone();
        let sent_welcome = welcome_extensions.clone();
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut control, _) = listener.accept()?;
            let mut preface = [0; PREFACE_SIZE];
            control.read_exact(&mut preface)?;
            let hello_record = read_client_record(&mut control)?;
            let (request, hello) = messages::parse_hello(&hello_record.body)?;
            assert_eq!(hello.preserved_fields, expected_hello);
            let mut sequence = 0;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::WELCOME,
                0,
                &messages::welcome_preserving(
                    request,
                    1,
                    &[1; 16],
                    1,
                    messages::DisplayChanged {
                        display_generation: 1,
                        viewport_width: 800,
                        viewport_height: 600,
                        grid_columns: 80,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                    REQUIRED_FEATURES,
                    &sent_welcome,
                ),
            )
        });
        let factory = Arc::new(EndpointConnectionFactory {
            primary: Endpoint::parse(&format!("unix:{}", socket.display())).unwrap(),
            bulk: None,
        });
        let bridge = OuterBridge::connect_with_factory_and_extensions(
            factory,
            Zeroizing::new("11".repeat(32)),
            DisplayMetrics::default(),
            &hello_extensions,
        )
        .unwrap();
        assert_eq!(bridge.hello_extensions, hello_extensions);
        assert_eq!(bridge.welcome_extensions, welcome_extensions);
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn outer_image_cache_hit_skips_media_connection_and_acknowledges_rehydration() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-image-cache.sock");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping outer image-cache socket test: {error}");
                return;
            }
            Err(error) => panic!("fake outer presenter bind failed: {error}"),
        };
        let server = thread::spawn(move || -> io::Result<bool> {
            let (mut control, _) = listener.accept()?;
            let mut preface = [0; PREFACE_SIZE];
            control.read_exact(&mut preface)?;
            let hello_record = read_client_record(&mut control)?;
            let (request, hello) = messages::parse_hello(&hello_record.body)?;
            assert!(
                hello
                    .optional_features
                    .contains(&messages::FEATURE_IMAGE_CACHE_V1)
            );
            let accepted = [
                messages::FEATURE_RASTER_RGBA8,
                messages::FEATURE_SCENE_TRANSACTIONS,
                messages::FEATURE_GRID_CELL_NODES,
                messages::FEATURE_CREDIT_FLOW_CONTROL,
                messages::FEATURE_ENCODED_IMAGE_V1,
                messages::FEATURE_NODE_CLIP_RECT_V1,
                messages::FEATURE_IMAGE_CACHE_V1,
            ];
            let mut sequence = 0;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::WELCOME,
                0,
                &messages::welcome(
                    request,
                    1,
                    &[1; 16],
                    1,
                    messages::DisplayChanged {
                        display_generation: 1,
                        viewport_width: 800,
                        viewport_height: 600,
                        grid_columns: 80,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                    &accepted,
                ),
            )?;

            let create = read_client_record(&mut control)?;
            assert_eq!(create.record_type, messages::CREATE_IMAGE);
            let (envelope, _, cache_lookup, _, _) =
                messages::parse_create_image_with_extensions(&create.body)?;
            assert!(cache_lookup);
            write_server_record(
                &mut control,
                &mut sequence,
                messages::SOURCE_READY,
                create.object_id,
                &messages::source_ready_with_observability(
                    envelope.request_id,
                    &messages::SourceReady {
                        source_id: create.object_id,
                        media_ticket: Vec::new(),
                        byte_credits: 0,
                        packet_credits: 0,
                        fragment_credits: 0,
                        max_media_body: 0,
                        rolling_byte_window: 0,
                        rolling_packet_window: 0,
                        initial_source_revision: SourceRevision::new(1),
                        media_connection_required: false,
                        delta_operation_limit: None,
                    },
                )?,
            )?;

            for expected in [messages::BEGIN_TXN, messages::COMMIT_TXN] {
                let record = read_client_record(&mut control)?;
                assert_eq!(record.record_type, expected);
                let reply = if expected == messages::COMMIT_TXN {
                    messages::PRESENTED
                } else {
                    messages::OK
                };
                write_server_record(
                    &mut control,
                    &mut sequence,
                    reply,
                    record.object_id,
                    &messages::ok(messages::request_id(&record.body)?),
                )?;
            }
            listener.set_nonblocking(true)?;
            Ok(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock))
        });

        let mut bridge = match OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new("11".repeat(32)),
            DisplayMetrics::default(),
        ) {
            Ok(bridge) => bridge,
            Err(error) => panic!(
                "outer bridge connection failed: {error}; server: {:?}",
                server.join().unwrap()
            ),
        };
        let source = BridgeSource {
            key: BridgeSourceKey {
                producer: 3,
                source: 9,
            },
            kind: BridgeSourceKind::Image {
                encoding: messages::IMAGE_PNG,
                width: 1,
                height: 1,
                encoded_length: 4,
                sha256: Some([7; 32]),
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 0,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: messages::LATE_DROP_PRESENTATION,
                loop_count: 0,
                start_policy: messages::START_AFTER_MINIMUM_BUFFER,
            },
        };
        bridge.rebuild(std::slice::from_ref(&source), &[]).unwrap();
        assert!(bridge.cached_images.contains(&source.key));
        assert!(
            bridge
                .media_chunk(
                    77,
                    source.key,
                    messages::IMAGE_DATA,
                    0,
                    4,
                    true,
                    vec![1, 2, 3, 4],
                )
                .unwrap()
        );
        let completions = bridge.take_media_completions();
        assert_eq!(completions.len(), 1);
        let (delivery_id, delivered, record_sequence, _object_id) = completions[0];
        assert_eq!((delivery_id, delivered, record_sequence), (77, true, 0));
        assert!(
            server.join().unwrap().unwrap(),
            "cache hit must not open or attach an outer blob connection"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pane_context_mapping_delegates_once_and_revokes_on_teardown() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-pane-context.sock");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping pane context mapping socket test: {error}");
                return;
            }
            Err(error) => panic!("fake outer presenter bind failed: {error}"),
        };
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut control, _) = listener.accept()?;
            let mut preface = [0; PREFACE_SIZE];
            control.read_exact(&mut preface)?;
            let hello = read_client_record(&mut control)?;
            let request = messages::request_id(&hello.body)?;
            let accepted = [
                messages::FEATURE_RASTER_RGBA8,
                messages::FEATURE_SCENE_TRANSACTIONS,
                messages::FEATURE_GRID_CELL_NODES,
                messages::FEATURE_CREDIT_FLOW_CONTROL,
                messages::FEATURE_NODE_CLIP_RECT_V1,
                messages::FEATURE_DELEGATED_CONTEXT_V1,
            ];
            let mut sequence = 0;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::WELCOME,
                0,
                &messages::welcome(
                    request,
                    1,
                    &[1; 16],
                    100,
                    messages::DisplayChanged {
                        display_generation: 1,
                        viewport_width: 800,
                        viewport_height: 600,
                        grid_columns: 80,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                    &accepted,
                ),
            )?;
            let create = read_client_record(&mut control)?;
            assert_eq!(create.record_type, messages::CREATE_CONTEXT);
            let (envelope, requested) = messages::parse_create_context(&create.body)?;
            assert_eq!(requested.parent_context_id, 100);
            assert_eq!(requested.class_mask & messages::CONTEXT_CLASS_ADMINISTER, 0);
            write_server_record(
                &mut control,
                &mut sequence,
                messages::CONTEXT_READY,
                requested.context_id,
                &messages::context_ready(
                    envelope.request_id,
                    messages::ContextReady {
                        context_id: requested.context_id,
                        class_mask: requested.class_mask,
                        quotas: requested.quotas,
                        expiry_us: 0,
                    },
                )?,
            )?;
            let delegate = read_client_record(&mut control)?;
            assert_eq!(delegate.record_type, messages::DELEGATE_CONTEXT);
            let (envelope, context_id) = messages::parse_object_id(&delegate.body, "context ID")?;
            let capability = [0xa5; messages::CONTEXT_CAPABILITY_BYTES];
            write_server_record(
                &mut control,
                &mut sequence,
                messages::CONTEXT_CAPABILITY,
                context_id,
                &messages::context_capability(envelope.request_id, context_id, &capability),
            )?;
            let revoke = read_client_record(&mut control)?;
            assert_eq!(revoke.record_type, messages::REVOKE_CONTEXT);
            let (envelope, revoked_context) =
                messages::parse_object_id(&revoke.body, "context ID")?;
            assert_eq!(revoked_context, context_id);
            write_server_record(
                &mut control,
                &mut sequence,
                messages::OK,
                context_id,
                &messages::ok(envelope.request_id),
            )
        });
        let factory = Arc::new(EndpointConnectionFactory {
            primary: Endpoint::parse(&format!("unix:{}", socket.display())).unwrap(),
            bulk: None,
        });
        let mut bridge = OuterBridge::connect_with_factory(
            factory,
            Zeroizing::new("11".repeat(32)),
            DisplayMetrics::default(),
        )
        .unwrap();
        let source = test_raster_source();
        bridge
            .sync_pane_contexts(std::slice::from_ref(&source), &[])
            .unwrap();
        assert!(bridge.pane_contexts.contains_key(&source.key.producer));
        bridge.sync_pane_contexts(&[], &[]).unwrap();
        assert!(bridge.pane_contexts.is_empty());
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn source_removal_keeps_media_open_until_nodes_and_source_are_destroyed() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-removal-order.sock");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping outer removal-order socket test: {error}");
                return;
            }
            Err(error) => panic!("fake outer presenter bind failed: {error}"),
        };
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut control, _) = listener.accept()?;
            let mut preface = [0; PREFACE_SIZE];
            control.read_exact(&mut preface)?;
            let hello = read_client_record(&mut control)?;
            let request = messages::request_id(&hello.body)?;
            let features = [
                messages::FEATURE_RASTER_RGBA8,
                messages::FEATURE_SCENE_TRANSACTIONS,
                messages::FEATURE_GRID_CELL_NODES,
                messages::FEATURE_CREDIT_FLOW_CONTROL,
                messages::FEATURE_NODE_CLIP_RECT_V1,
            ];
            let mut sequence = 0;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::WELCOME,
                0,
                &messages::welcome(
                    request,
                    1,
                    &[1; 16],
                    1,
                    messages::DisplayChanged {
                        display_generation: 1,
                        viewport_width: 800,
                        viewport_height: 600,
                        grid_columns: 80,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                    &features,
                ),
            )?;

            let create = read_client_record(&mut control)?;
            assert_eq!(create.record_type, messages::CREATE_RASTER);
            let create_request = messages::request_id(&create.body)?;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::SOURCE_READY,
                create.object_id,
                &messages::source_ready(
                    create_request,
                    create.object_id,
                    &[7; 32],
                    messages::Credits {
                        bytes: 4096,
                        packets: 1,
                        fragments: 0,
                    },
                    4096,
                ),
            )?;

            let (mut media, _) = listener.accept()?;
            media.read_exact(&mut preface)?;
            let attached = read_client_record(&mut media)?;
            assert_eq!(attached.record_type, messages::ATTACH_CHANNEL);
            let (media_closed_tx, media_closed_rx) = mpsc::channel();
            let media_reader = thread::spawn(move || {
                let mut byte = [0_u8; 1];
                while media.read(&mut byte).is_ok_and(|read| read != 0) {}
                let _ = media_closed_tx.send(());
            });

            for expected in [
                messages::BEGIN_TXN,
                messages::CREATE_NODE,
                messages::COMMIT_TXN,
            ] {
                let record = read_client_record(&mut control)?;
                assert_eq!(record.record_type, expected);
                let request = messages::request_id(&record.body)?;
                let response = if expected == messages::COMMIT_TXN {
                    messages::PRESENTED
                } else {
                    messages::OK
                };
                write_server_record(
                    &mut control,
                    &mut sequence,
                    response,
                    record.object_id,
                    &messages::ok(request),
                )?;
            }

            assert!(
                media_closed_rx
                    .recv_timeout(Duration::from_millis(50))
                    .is_err(),
                "outer media closed before the node-removal transaction began"
            );
            for expected in [
                messages::BEGIN_TXN,
                messages::DELETE_NODE,
                messages::COMMIT_TXN,
                messages::DESTROY_SOURCE,
            ] {
                let record = read_client_record(&mut control)?;
                assert_eq!(record.record_type, expected);
                let request = messages::request_id(&record.body)?;
                let response = if expected == messages::COMMIT_TXN {
                    messages::PRESENTED
                } else {
                    messages::OK
                };
                write_server_record(
                    &mut control,
                    &mut sequence,
                    response,
                    record.object_id,
                    &messages::ok(request),
                )?;
            }
            media_closed_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "outer media remained open after DESTROY_SOURCE",
                    )
                })?;
            media_reader.join().unwrap();
            Ok(())
        });

        let mut bridge = OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new("11".repeat(32)),
            DisplayMetrics::default(),
        )
        .unwrap();
        bridge
            .rebuild(&[test_raster_source()], &[test_fragment(9, 0, 0)])
            .unwrap();
        bridge.rebuild(&[], &[]).unwrap();
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn outer_initial_window_and_reoriginated_eos_barrier_use_outer_sequences() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-window.sock");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping outer bridge window socket test: {error}");
                return;
            }
            Err(error) => panic!("fake outer presenter bind failed: {error}"),
        };
        let (media_seen_tx, media_seen_rx) = mpsc::channel();
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut control, _) = listener.accept()?;
            let mut preface = [0; PREFACE_SIZE];
            control.read_exact(&mut preface)?;
            let hello = read_client_record(&mut control)?;
            let request = messages::request_id(&hello.body)?;
            let features = [
                messages::FEATURE_RASTER_RGBA8,
                messages::FEATURE_SCENE_TRANSACTIONS,
                messages::FEATURE_GRID_CELL_NODES,
                messages::FEATURE_CREDIT_FLOW_CONTROL,
                messages::FEATURE_VIDEO_ACCESS_UNIT_V1,
                messages::FEATURE_VIDEO_CONTROL_V1,
                messages::FEATURE_AUDIO_ACCESS_UNIT_V1,
                messages::FEATURE_NODE_CLIP_RECT_V1,
                messages::FEATURE_MEDIA_ORDER_BARRIER_V1,
            ];
            let mut sequence = 0;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::WELCOME,
                0,
                &messages::welcome(
                    request,
                    1,
                    &[1; 16],
                    1,
                    messages::DisplayChanged {
                        display_generation: 1,
                        viewport_width: 800,
                        viewport_height: 600,
                        grid_columns: 80,
                        grid_rows: 24,
                        cell_width: 10,
                        cell_height: 25,
                        settled: true,
                    },
                    &features,
                ),
            )?;

            let create = read_client_record(&mut control)?;
            let create_request = messages::request_id(&create.body)?;
            write_server_record(
                &mut control,
                &mut sequence,
                messages::SOURCE_READY,
                create.object_id,
                &messages::source_ready(
                    create_request,
                    create.object_id,
                    &[7; 32],
                    messages::Credits {
                        bytes: 4096,
                        packets: 3,
                        fragments: 0,
                    },
                    4096,
                ),
            )?;

            let (mut media, _) = listener.accept()?;
            media.read_exact(&mut preface)?;
            let attached = read_client_record(&mut media)?;
            assert_eq!(attached.record_type, messages::ATTACH_CHANNEL);
            let media_reader = thread::spawn(move || -> io::Result<()> {
                for _ in 0..3 {
                    let record = read_client_record(&mut media)?;
                    if record.record_type != messages::VIDEO_PACKET {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "fake presenter received a non-video record",
                        ));
                    }
                    media_seen_tx.send(()).map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "media observer closed")
                    })?;
                }
                Ok(())
            });

            for _ in 0..3 {
                let record = read_client_record(&mut control)?;
                let request = messages::request_id(&record.body)?;
                let reply = if record.record_type == messages::COMMIT_TXN {
                    messages::PRESENTED
                } else {
                    messages::OK
                };
                write_server_record(
                    &mut control,
                    &mut sequence,
                    reply,
                    record.object_id,
                    &messages::ok(request),
                )?;
            }
            media_reader.join().unwrap()?;
            let eos = read_client_record(&mut control)?;
            assert_eq!(eos.record_type, messages::EOS);
            let (envelope, request) = messages::parse_eos(&eos.body)?;
            assert_eq!(
                request.barrier,
                Some(messages::MediaOrderBarrier {
                    attachment_generation: 1,
                    final_record_sequence: 4,
                }),
                "the outer EOS must name the outer attachment and its own final record sequence"
            );
            write_server_record(
                &mut control,
                &mut sequence,
                messages::OK,
                eos.object_id,
                &messages::ok(envelope.request_id),
            )
        });

        let mut bridge = OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new("11".repeat(32)),
            DisplayMetrics::default(),
        )
        .unwrap();
        let source = BridgeSource {
            key: BridgeSourceKey {
                producer: 1,
                source: 2,
            },
            kind: BridgeSourceKind::Video {
                codec_string: None,
                decoder_config: None,
                codec: "h264".into(),
                packetization: "h264-annexb-au-v1".into(),
                extradata: Vec::new(),
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
            },
            capture_policy: 0,
            descriptor: None,
            playing: true,
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 0,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: messages::LATE_DROP_PRESENTATION,
                loop_count: 0,
                start_policy: messages::START_AFTER_MINIMUM_BUFFER,
            },
        };
        bridge.rebuild(std::slice::from_ref(&source), &[]).unwrap();
        for delivery_id in 1..=3 {
            bridge
                .media_chunk(
                    delivery_id,
                    source.key,
                    messages::VIDEO_PACKET,
                    0,
                    64,
                    true,
                    vec![delivery_id as u8; 64],
                )
                .unwrap();
        }
        for _ in 0..3 {
            media_seen_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut completed = HashMap::new();
        while completed.len() < 3 && Instant::now() < deadline {
            completed.extend(bridge.take_media_completions().into_iter().filter_map(
                |(delivery, delivered, sequence, _object)| {
                    delivered.then_some((delivery, sequence))
                },
            ));
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(completed, HashMap::from([(1, 2), (2, 3), (3, 4)]));
        let mut ended = source.clone();
        ended.eos_epoch = Some(1);
        bridge
            .update_playback(std::slice::from_ref(&source), std::slice::from_ref(&ended))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn rebuild_starts_projected_video_playback() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-vivid.sock");
        let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping outer bridge socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual outer presenter start failed: {error}"),
        };
        let token = presenter.issue_pane_capability(7).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));

        let endpoint = format!("unix:{}", socket.display());
        let mut bridge =
            OuterBridge::connect(endpoint, Zeroizing::new(token), DisplayMetrics::default())
                .unwrap();
        let source = BridgeSource {
            key: BridgeSourceKey {
                producer: 4,
                source: 9,
            },
            kind: BridgeSourceKind::Video {
                codec_string: None,
                decoder_config: None,
                codec: "h264".into(),
                packetization: "h264-annexb-au-v1".into(),
                extradata: Vec::new(),
                width: 640,
                height: 360,
                profile: 100,
                level: 40,
                bitrate: 1_000_000,
                color_primaries: 1,
                transfer: 1,
                matrix: 1,
                range: 1,
                sar_num: 1,
                sar_den: 1,
                max_access_unit_bytes: 1_048_576,
            },
            capture_policy: 0,
            descriptor: None,
            playing: true,
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 33_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: messages::LATE_DROP_PRESENTATION,
                loop_count: 0,
                start_policy: messages::START_AFTER_MINIMUM_BUFFER,
            },
        };
        let key = source.key;
        bridge.rebuild(std::slice::from_ref(&source), &[]).unwrap();
        let packet = media::video_packet_body(VideoPacket {
            epoch: 1,
            packet_id: 1,
            pts_us: 0,
            dts_us: 0,
            duration_us: 33_000,
            key: true,
            data: &[0, 0, 0, 1, 0x65, 0x88],
        })
        .unwrap();
        bridge
            .media_chunk(
                1,
                key,
                messages::VIDEO_PACKET,
                0,
                packet.len() as u32,
                true,
                packet,
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        let snapshot = loop {
            let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
            if !snapshot.videos_needing_keyframes.is_empty() || Instant::now() >= deadline {
                break snapshot;
            }
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(snapshot.sources.len(), 1);
        assert!(snapshot.sources[0].playing);
        assert_eq!(snapshot.sources[0].play_request.minimum_buffer_us, 33_000);
        assert_eq!(snapshot.videos_needing_keyframes.len(), 1);

        let mut rebased = source.clone();
        rebased.play_request.start_pts_us = 30_000_000;
        bridge
            .update_playback(
                std::slice::from_ref(&source),
                std::slice::from_ref(&rebased),
            )
            .unwrap();
        let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
        assert_eq!(
            snapshot.sources[0].play_request.start_pts_us, 30_000_000,
            "a playback-only update must re-base the outer PLAY without a rebuild"
        );

        // Inner ingress ends. The outer epoch has to be closed explicitly: a presenter only
        // reaches its playback-ended milestone after EOS, so without this the inner producer
        // waits on WAIT_PLAYBACK_ENDED for a milestone that never arrives.
        let mut ended = rebased.clone();
        ended.eos_epoch = Some(1);
        bridge
            .update_playback(std::slice::from_ref(&rebased), std::slice::from_ref(&ended))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let snapshot = loop {
            let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
            if snapshot.sources[0].eos_epoch.is_some() || Instant::now() >= deadline {
                break snapshot;
            }
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(
            snapshot.sources[0].eos_epoch,
            Some(1),
            "the outer presenter must receive EOS for the ended epoch"
        );
        assert!(
            snapshot.sources[0].playing,
            "EOS closes ingress without pausing already-buffered playback"
        );

        // The transition is edge-triggered, so a repeated snapshot must not re-send EOS.
        bridge
            .update_playback(std::slice::from_ref(&ended), std::slice::from_ref(&ended))
            .unwrap();
    }
}
