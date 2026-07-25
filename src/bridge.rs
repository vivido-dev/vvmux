use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::cbor::PreservedField;
use vivid_protocol::messages::{
    self, AudioSourceConfig, ClipRect, HelloConfig, ImageSourceConfig, RasterSourceConfig,
    SceneNodeConfig, VideoSourceConfig,
};
use vivid_protocol::wire::{Connection, ConnectionKind, ConnectionWriter, Endpoint, Record};
use vivid_protocol::{VIVID_MAJOR, VIVID_MINOR};
use zeroize::Zeroizing;

use crate::ipc::{BridgeNode, BridgeSource, BridgeSourceKey, BridgeSourceKind, DisplayMetrics};

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
];
const MAX_PENDING_CONTROL_REPLIES: usize = 4096;
const MEDIA_WRITER_QUEUE: usize = 32;

struct ControlState {
    replies: HashMap<u64, Record>,
    pending_requests: HashSet<u64>,
    credits: HashMap<u64, messages::CreditLedger>,
    keyframes: Vec<messages::NeedKeyframe>,
    source_losses: Vec<u64>,
    display_generation: u64,
    closed: Option<String>,
    last_inbound: Instant,
    last_probe_sent: Option<Instant>,
    unanswered_probes: u8,
    next_ping_id: u64,
    pending_pings: HashMap<u64, Instant>,
    rtt_us: Option<u64>,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            replies: HashMap::new(),
            pending_requests: HashSet::new(),
            credits: HashMap::new(),
            keyframes: Vec::new(),
            source_losses: Vec::new(),
            display_generation: 0,
            closed: None,
            last_inbound: Instant::now(),
            last_probe_sent: None,
            unanswered_probes: 0,
            next_ping_id: u64::MAX,
            pending_pings: HashMap::new(),
            rtt_us: None,
        }
    }
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
    fn start(connection: Connection, display_generation: u64) -> io::Result<Self> {
        let (mut reader, writer) = connection.split()?;
        let shared = Arc::new(SharedControl {
            state: Mutex::new(ControlState {
                display_generation,
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
                            if !state.pending_requests.remove(&request_id) {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "outer presenter replied to an unknown request",
                                ));
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
                        if now.duration_since(state.last_inbound) < Duration::from_secs(15)
                            || state.last_probe_sent.is_some_and(|sent| {
                                now.duration_since(sent) < Duration::from_secs(15)
                            })
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
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(record) = state.replies.remove(&request_id) {
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
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
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

struct MediaCompletion {
    delivery_id: u64,
    delivered: bool,
    record_sequence: u64,
}

struct SourceMediaWriter {
    sender: mpsc::SyncSender<MediaWrite>,
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
    active_sources: HashMap<BridgeSourceKey, BridgeSource>,
    node_ids: HashMap<(u64, u64, u8), u64>,
    display: DisplayMetrics,
    /// Outer presenter accepted `decoder-description-v1`; forwarding the optional CREATE fields
    /// without acceptance would violate the specification.
    decoder_description: bool,
    hello_extensions: Vec<PreservedField>,
    #[allow(dead_code)] // Retained for negotiation-aware gateway consumers and conformance tests.
    welcome_extensions: Vec<PreservedField>,
}

struct PendingBody {
    record_type: u16,
    total: usize,
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
        display: DisplayMetrics,
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
        let decoder_description =
            accepted_features.contains(&messages::FEATURE_DECODER_DESCRIPTION_V1);
        connection.set_send_body_limit(welcome.maximum_control_body)?;
        let control = ControlDispatcher::start(connection, welcome.display_generation)?;
        let (completions_tx, completions_rx) = mpsc::channel();
        Ok(Self {
            connection_factory,
            token,
            control,
            root_context: welcome.root_context_id,
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
            active_sources: HashMap::new(),
            node_ids: HashMap::new(),
            display,
            decoder_description,
            hello_extensions: hello_extensions.to_vec(),
            welcome_extensions: welcome.preserved_fields,
        })
    }

    pub fn rebuild(
        &mut self,
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<std::collections::HashSet<BridgeSourceKey>> {
        validate_snapshot(sources, nodes)?;
        if let Ok(recreated) = self.reconcile(sources, nodes) {
            return Ok(recreated);
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
        *self = replacement;
        Ok(recreated)
    }

    fn reconcile(
        &mut self,
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<std::collections::HashSet<BridgeSourceKey>> {
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
        self.reconcile_nodes(nodes)?;

        let previous_sources = self.active_sources.values().cloned().collect::<Vec<_>>();
        self.update_playback(&previous_sources, sources)?;
        for source in sources
            .iter()
            .filter(|source| recreate.contains(&source.key) && source.playing)
        {
            self.play_source(source)?;
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
        }
        drop(obsolete_media);
        self.active_sources = sources
            .iter()
            .cloned()
            .map(|source| (source.key, source))
            .collect();
        Ok(recreate)
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
            if old.playing == source.playing
                && (!source.playing || old.play_request == source.play_request)
            {
                continue;
            }
            if source.playing {
                self.play_source(source)?;
            } else {
                self.pause_source(source)?;
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
        let pending = self.pending.entry(key).or_insert_with(|| PendingBody {
            record_type,
            total: total as usize,
            bytes: Vec::with_capacity(total as usize),
        });
        if pending.record_type != record_type
            || pending.total != total as usize
            || pending.bytes.len() != offset as usize
        {
            self.pending.remove(&key);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media chunk sequence gap",
            ));
        }
        pending.bytes.extend_from_slice(&bytes);
        if pending.bytes.len() > pending.total {
            self.pending.remove(&key);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "media chunks exceed total",
            ));
        }
        if last {
            let pending = self.pending.remove(&key).unwrap();
            if pending.bytes.len() != pending.total {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete media body",
                ));
            }
            let upstream = *self.source_ids.get(&key).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "projection source missing")
            })?;
            self.media
                .get(&key)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "projection media channel missing")
                })?
                .sender
                .try_send(MediaWrite {
                    delivery_id,
                    record_type: pending.record_type,
                    object_id: upstream,
                    body: pending.bytes,
                })
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
            return Ok(true);
        }
        Ok(false)
    }

    pub fn take_media_completions(&self) -> Vec<(u64, bool, u64)> {
        self.completions_rx
            .try_iter()
            .map(|completion| {
                (
                    completion.delivery_id,
                    completion.delivered,
                    completion.record_sequence,
                )
            })
            .collect()
    }

    pub fn take_keyframe_requests(&self) -> Vec<BridgeSourceKey> {
        self.control
            .take_keyframes()
            .into_iter()
            .filter_map(|request| self.reverse_source_ids.get(&request.source_id).copied())
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
            self.source_kinds.remove(key);
        }
        losses
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
                } => (
                    messages::CREATE_RASTER,
                    ConnectionKind::Raster,
                    messages::create_raster_config(
                        request,
                        &RasterSourceConfig {
                            source_id: upstream,
                            width: *width,
                            height: *height,
                            alpha_mode: *alpha_mode,
                            compression_mode: *compression_mode,
                        },
                    ),
                ),
                BridgeSourceKind::Image {
                    encoding,
                    width,
                    height,
                    encoded_length,
                    sha256,
                } => (
                    messages::CREATE_IMAGE,
                    ConnectionKind::Blob,
                    messages::create_image(
                        request,
                        &ImageSourceConfig {
                            source_id: upstream,
                            encoding: *encoding,
                            width: *width,
                            height: *height,
                            encoded_length: *encoded_length,
                            sha256: *sha256,
                        },
                    ),
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
                    messages::create_video(
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
                        messages::create_audio(
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
                        ),
                    )
                }
            };
            self.control.write_record(record_type, 0, upstream, &body)?;
            pending.push((source.clone(), request, upstream, kind));
        }

        // All CREATE requests are now in flight on the ordered control stream. Correlate their
        // independently completed replies before attaching each source-specific media channel.
        for (source, request, upstream, kind) in pending {
            let ready = self.wait_source_ready(request, upstream)?;
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
            self.source_kinds.insert(source.key, source.kind.clone());
        }
        Ok(())
    }

    fn reconcile_nodes(&mut self, nodes: &[BridgeNode]) -> io::Result<()> {
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

        let mut next_node_ids = self.node_ids.clone();
        let mut mutation_requests = Vec::new();
        for node in nodes {
            let stable_key = (node.producer, node.node, node.fragment);
            let (record_type, node_id) = if let Some(node_id) = next_node_ids.get(&stable_key) {
                (messages::UPDATE_NODE, *node_id)
            } else {
                self.next_node = self
                    .next_node
                    .checked_add(1)
                    .ok_or_else(|| exhausted("node"))?;
                next_node_ids.insert(stable_key, self.next_node);
                (messages::CREATE_NODE, self.next_node)
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

        let current_keys = nodes
            .iter()
            .map(|node| (node.producer, node.node, node.fragment))
            .collect::<std::collections::HashSet<_>>();
        let removed = self
            .node_ids
            .iter()
            .filter(|(stable_key, _)| !current_keys.contains(stable_key))
            .map(|(stable_key, node_id)| (*stable_key, *node_id))
            .collect::<Vec<_>>();
        for (stable_key, node_id) in removed {
            let request = self.request_id()?;
            self.control.write_record(
                messages::DELETE_NODE,
                0,
                node_id,
                &messages::delete_node(request, transaction, node_id),
            )?;
            mutation_requests.push((request, node_id));
            next_node_ids.remove(&stable_key);
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
        self.wait_for(commit_request, messages::PRESENTED, 0)?;
        self.node_ids = next_node_ids;
        Ok(())
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
        self.control.write_record(
            messages::PLAY,
            0,
            upstream,
            &messages::play_request(
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
            ),
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
        self.control.write_record(
            messages::PAUSE,
            0,
            upstream,
            &messages::pause(request, upstream),
        )?;
        self.wait_for(request, messages::OK, upstream)
    }

    fn wait_source_ready(
        &self,
        request_id: u64,
        source_id: u64,
    ) -> io::Result<messages::SourceReady> {
        let record = self
            .control
            .wait_reply(request_id, messages::SOURCE_READY, source_id)?;
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
    receiver: mpsc::Receiver<MediaWrite>,
    completions: mpsc::Sender<MediaCompletion>,
) {
    while let Ok(write) = receiver.recv() {
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
        if completions
            .send(MediaCompletion {
                delivery_id: write.delivery_id,
                delivered,
                record_sequence,
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

fn reserve_outer_credit(shared: &SharedControl, source_id: u64, bytes: u64) -> io::Result<()> {
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
        state = shared
            .changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn protocol_error(body: &[u8]) -> io::Error {
    let message = messages::parse_error(body).unwrap_or_else(|_| "outer Vivid error".into());
    io::Error::other(message)
}

fn exhausted(kind: &'static str) -> io::Error {
    io::Error::other(format!("outer Vivid {kind} ID space exhausted"))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::HashSet;
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use super::*;
    #[cfg(unix)]
    use crate::config::Media as MediaConfig;
    #[cfg(unix)]
    use crate::media::VirtualVivid;

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
            },
            playing: false,
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
    #[cfg(unix)]
    use vivid_protocol::media::{self, VideoPacket};
    #[cfg(unix)]
    use vivid_protocol::wire::{HEADER_SIZE, PREFACE_SIZE, RecordHeader};

    #[cfg(unix)]
    fn read_client_record(stream: &mut std::os::unix::net::UnixStream) -> io::Result<Record> {
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

    #[cfg(unix)]
    fn write_server_record(
        stream: &mut std::os::unix::net::UnixStream,
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
    fn outer_initial_window_is_filled_without_credit_return() {
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
            media_reader.join().unwrap()
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
            playing: true,
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
                |(delivery, delivered, sequence)| delivered.then_some((delivery, sequence)),
            ));
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(completed, HashMap::from([(1, 2), (2, 3), (3, 4)]));
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
            playing: true,
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
    }
}
