use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use vivid_protocol::anchor::{self, AnchorKey};
use vivid_protocol::media::{self, MediaSequence};
use vivid_protocol::messages::{
    self, Credits, DisplayChanged, ImageSourceConfig, ParsedAudioSourceConfig, ParsedSceneNode,
    ParsedVideoSourceConfig, RasterSourceConfig,
};
use vivid_protocol::wire::{ConnectionKind, Record};

use crate::config::Media as MediaConfig;
use crate::layout::PaneId;
use crate::platform::{
    ConnectionCancel, Transport, VirtualPresenterEndpoint, VirtualPresenterListener,
};
use crate::vivid_transport::{Reader, Writer};

const MAX_PRODUCERS: usize = 16;
const MAX_CONNECTIONS: usize = 64;
const MAX_SEEN_ANCHORS: usize = 4096;
// Keep at most one timed packet ahead of the virtual presenter. Besides bounding pre-roll, this
// forces Vivi to observe an unsolicited NEED_KEYFRAME within one discarded packet after a pane is
// projected again instead of consuming a large local credit window before reading control events.
const INITIAL_PACKET_CREDITS: u64 = 1;

pub type ProducerId = u64;
pub type SourceKey = (ProducerId, u64);

#[derive(Debug, Clone)]
pub enum SourceDescriptor {
    Raster(RasterSourceConfig),
    Image(ImageSourceConfig),
    Video(ParsedVideoSourceConfig),
    Audio(ParsedAudioSourceConfig),
}

impl SourceDescriptor {
    fn maximum_body(&self) -> io::Result<u32> {
        match self {
            Self::Raster(config) => media::rgba8_raw_frame_body_len(config.width, config.height)
                .map_err(io::Error::other),
            Self::Image(config) => Ok(config.encoded_length),
            Self::Video(config) => {
                media::video_body_len(config.max_access_unit_bytes).map_err(io::Error::other)
            }
            Self::Audio(config) => {
                media::audio_body_len(config.max_access_unit_bytes).map_err(io::Error::other)
            }
        }
    }

    fn kind(&self) -> ConnectionKind {
        match self {
            Self::Raster(_) => ConnectionKind::Raster,
            Self::Image(_) => ConnectionKind::Blob,
            Self::Video(_) => ConnectionKind::Video,
            Self::Audio(_) => ConnectionKind::Audio,
        }
    }

    fn is_static(&self) -> bool {
        matches!(self, Self::Raster(_) | Self::Image(_))
    }
}

#[derive(Debug, Clone)]
pub struct SceneNode {
    pub producer: ProducerId,
    pub pane: PaneId,
    pub config: ParsedSceneNode,
    pub(crate) retained_anchor: Option<(i32, usize)>,
}

#[derive(Debug, Clone)]
pub struct ProjectionSnapshot {
    pub revision: u64,
    pub sources: Vec<SnapshotSource>,
    pub nodes: Vec<SceneNode>,
    /// Every logical scene-node key in the virtual session, including inactive tabs. The
    /// session uses this only to retire fragment-ID maps after true node destruction; hidden
    /// nodes keep their stable assignments.
    pub live_nodes: Vec<(ProducerId, u64)>,
    pub videos_needing_keyframes: Vec<SourceKey>,
}

#[derive(Debug, Clone)]
pub struct SnapshotSource {
    pub key: SourceKey,
    pub descriptor: SourceDescriptor,
    pub retained: Option<Arc<[u8]>>,
    pub playing: bool,
    pub play_request: messages::PlayRequest,
}

#[derive(Debug)]
pub struct MediaEvent {
    pub delivery_id: u64,
    pub source: SourceKey,
    pub record_type: u16,
    pub body: Vec<u8>,
}

struct PendingDelivery {
    source: SourceKey,
    bytes: u64,
}

struct Producer {
    pane: PaneId,
    tag: [u8; 16],
    anchor_key: AnchorKey,
    writer: Weak<Writer>,
    features: HashSet<u64>,
    anchors: HashMap<u64, (i32, usize)>,
    seen_anchors: HashSet<u64>,
}

struct Source {
    owner: ProducerId,
    descriptor: SourceDescriptor,
    retained: Option<Arc<[u8]>>,
    sequence: MediaSequence,
    retained_bytes: usize,
    playing: bool,
    play_request: messages::PlayRequest,
    ended: bool,
    bridge_desynchronized: bool,
    minimum_epoch: u32,
    last_pts_us: Option<i64>,
    clock_started: Option<Instant>,
    clock_origin_pts_us: Option<i64>,
}

struct Ticket {
    source: SourceKey,
    kind: ConnectionKind,
    maximum_body: u32,
}

#[derive(Clone)]
enum Mutation {
    Create(SceneNode),
    Update(SceneNode),
    Delete(ProducerId, u64),
}

struct State {
    config: MediaConfig,
    capabilities: HashMap<PaneId, [u8; 32]>,
    metrics: HashMap<PaneId, DisplayChanged>,
    producers: HashMap<ProducerId, Producer>,
    sources: HashMap<SourceKey, Source>,
    nodes: HashMap<(ProducerId, u64), SceneNode>,
    transactions: HashMap<(ProducerId, u64), Vec<Mutation>>,
    tickets: HashMap<[u8; 32], Ticket>,
    next_producer: ProducerId,
    retained_bytes: usize,
    connections: usize,
    revision: u64,
    projected_sources: HashSet<SourceKey>,
    active_panes: HashSet<PaneId>,
    deliveries: HashMap<u64, PendingDelivery>,
    next_delivery_id: u64,
    queued_bridge_bytes: usize,
    events: Option<mpsc::SyncSender<MediaEvent>>,
    next_connection: u64,
    connection_cancellers: HashMap<u64, (Option<PaneId>, ConnectionCancel)>,
}

pub struct VirtualVivid {
    endpoint: String,
    state: Arc<Mutex<State>>,
    delivery_changed: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
}

impl VirtualVivid {
    #[cfg(test)]
    pub fn start(endpoint: VirtualPresenterEndpoint, config: MediaConfig) -> io::Result<Self> {
        Self::start_with_events(endpoint, config, None)
    }

    pub fn start_with_events(
        endpoint: VirtualPresenterEndpoint,
        config: MediaConfig,
        events: Option<mpsc::SyncSender<MediaEvent>>,
    ) -> io::Result<Self> {
        let listener = VirtualPresenterListener::bind(endpoint)?;
        let advertised_endpoint = listener.endpoint();
        let state = Arc::new(Mutex::new(State {
            config,
            capabilities: HashMap::new(),
            metrics: HashMap::new(),
            producers: HashMap::new(),
            sources: HashMap::new(),
            nodes: HashMap::new(),
            transactions: HashMap::new(),
            tickets: HashMap::new(),
            next_producer: 0,
            retained_bytes: 0,
            connections: 0,
            revision: 0,
            projected_sources: HashSet::new(),
            active_panes: HashSet::new(),
            deliveries: HashMap::new(),
            next_delivery_id: 0,
            queued_bridge_bytes: 0,
            events,
            next_connection: 0,
            connection_cancellers: HashMap::new(),
        }));
        let delivery_changed = Arc::new(Condvar::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let service = Self {
            endpoint: advertised_endpoint,
            state: state.clone(),
            delivery_changed: delivery_changed.clone(),
            shutdown: shutdown.clone(),
        };
        thread::Builder::new()
            .name("vvmux-vivid-listener".into())
            .spawn(move || accept_loop(listener, state, delivery_changed, shutdown))?;
        Ok(service)
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn issue_pane_capability(&self, pane: PaneId) -> io::Result<String> {
        let mut token = [0_u8; 32];
        getrandom::fill(&mut token)
            .map_err(|error| io::Error::other(format!("capability generation failed: {error}")))?;
        self.lock().capabilities.insert(pane, token);
        Ok(hex(&token))
    }

    pub fn revoke_pane(&self, pane: PaneId) {
        let mut state = self.lock();
        state.capabilities.remove(&pane);
        let producers = state
            .producers
            .iter()
            .filter_map(|(id, producer)| (producer.pane == pane).then_some(*id))
            .collect::<Vec<_>>();
        for producer in producers {
            cleanup_producer(&mut state, producer, false);
        }
        state.nodes.retain(|_, node| node.pane != pane);
        prune_orphaned_sources(&mut state);
        state.metrics.remove(&pane);
        state.revision = state.revision.wrapping_add(1);
        let cancellers = state
            .connection_cancellers
            .values()
            .filter_map(|(owner, cancel)| (*owner == Some(pane)).then_some(cancel.clone()))
            .collect::<Vec<_>>();
        drop(state);
        for cancel in cancellers {
            cancel.cancel();
        }
    }

    pub fn update_metrics(&self, pane: PaneId, columns: u16, rows: u16, cell: (u16, u16)) {
        let mut state = self.lock();
        let previous = state.metrics.get(&pane).copied();
        let generation = previous.map_or(1, |metrics| metrics.display_generation.wrapping_add(1));
        let display = DisplayChanged {
            display_generation: generation,
            viewport_width: u32::from(columns) * u32::from(cell.0),
            viewport_height: u32::from(rows) * u32::from(cell.1),
            grid_columns: u32::from(columns),
            grid_rows: u32::from(rows),
            cell_width: u32::from(cell.0),
            cell_height: u32::from(cell.1),
        };
        state.metrics.insert(pane, display);
        for producer in state
            .producers
            .values()
            .filter(|producer| producer.pane == pane)
        {
            if let Some(writer) = producer.writer.upgrade() {
                let _ = writer.write_record(
                    messages::DISPLAY_CHANGED,
                    0,
                    &messages::display_changed(0, display),
                );
            }
        }
    }

    pub fn observe_marker(&self, pane: PaneId, marker: &str, line: i32, column: usize) -> bool {
        let Ok(parsed) = anchor::parse_marker(marker) else {
            return false;
        };
        let writer = {
            let mut state = self.lock();
            let producer_id = state.producers.iter().find_map(|(id, producer)| {
                (producer.pane == pane
                    && producer.tag == parsed.session_tag
                    && anchor::verify_marker(&producer.anchor_key, &parsed))
                .then_some(*id)
            });
            let Some(producer_id) = producer_id else {
                return false;
            };
            let producer = state.producers.get_mut(&producer_id).unwrap();
            if producer.seen_anchors.len() >= MAX_SEEN_ANCHORS
                || producer.seen_anchors.contains(&parsed.anchor_id)
            {
                return false;
            }
            let Some(writer) = producer.writer.upgrade() else {
                return false;
            };
            producer.seen_anchors.insert(parsed.anchor_id);
            producer.anchors.insert(parsed.anchor_id, (line, column));
            state.revision = state.revision.wrapping_add(1);
            writer
        };
        writer
            .write_record(
                messages::ANCHOR_READY,
                parsed.anchor_id,
                &messages::anchor_event(parsed.anchor_id),
            )
            .is_ok()
    }

    pub fn scroll_anchors(&self, pane: PaneId, lines: i32) {
        let mut state = self.lock();
        for producer in state
            .producers
            .values_mut()
            .filter(|producer| producer.pane == pane)
        {
            for (line, _) in producer.anchors.values_mut() {
                *line = line.saturating_sub(lines);
            }
        }
        for node in state.nodes.values_mut().filter(|node| node.pane == pane) {
            if let Some((line, _)) = &mut node.retained_anchor {
                *line = line.saturating_sub(lines);
            }
        }
        state.revision = state.revision.wrapping_add(1);
    }

    pub fn clear_anchors(&self, pane: PaneId) {
        let mut state = self.lock();
        for producer in state
            .producers
            .values_mut()
            .filter(|producer| producer.pane == pane)
        {
            producer.anchors.clear();
        }
        state
            .nodes
            .retain(|_, node| node.pane != pane || node.retained_anchor.is_none());
        prune_orphaned_sources(&mut state);
        state.revision = state.revision.wrapping_add(1);
    }

    pub fn projection_snapshot(&self, panes: &HashSet<PaneId>) -> ProjectionSnapshot {
        let mut state = self.lock();
        let mut live_nodes = state.nodes.keys().copied().collect::<Vec<_>>();
        live_nodes.sort_unstable();
        let active_producers = state
            .producers
            .iter()
            .filter_map(|(id, producer)| panes.contains(&producer.pane).then_some(*id))
            .collect::<HashSet<_>>();
        let nodes = state
            .nodes
            .values()
            .filter(|node| panes.contains(&node.pane))
            .filter_map(|node| {
                let mut resolved = node.clone();
                if let Some(anchor_id) = resolved.config.node.anchor_id {
                    let (line, column) = state
                        .producers
                        .get(&node.producer)
                        .and_then(|producer| producer.anchors.get(&anchor_id).copied())
                        .or(node.retained_anchor)?;
                    let offset_x = (column as i64) << 32;
                    let offset_y = i64::from(line) << 32;
                    resolved.config.node.x = resolved.config.node.x.checked_add(offset_x)?;
                    resolved.config.node.y = resolved.config.node.y.checked_add(offset_y)?;
                    if let Some(clip) = &mut resolved.config.clip {
                        clip.x = clip.x.checked_add(offset_x)?;
                        clip.y = clip.y.checked_add(offset_y)?;
                    }
                    resolved.config.node.anchor_id = None;
                }
                Some(resolved)
            })
            .collect::<Vec<_>>();
        let referenced_sources = nodes
            .iter()
            .map(|node| (node.producer, node.config.node.source_id))
            .collect::<HashSet<_>>();
        let mut sources = Vec::new();
        let mut videos_needing_keyframes = Vec::new();
        for (key, source) in &mut state.sources {
            if !active_producers.contains(&source.owner) && !referenced_sources.contains(key) {
                continue;
            }
            sources.push(SnapshotSource {
                key: *key,
                descriptor: source.descriptor.clone(),
                retained: source.retained.clone(),
                playing: source.playing,
                play_request: source.play_request,
            });
            if matches!(source.descriptor, SourceDescriptor::Video(_))
                && source.playing
                && !source.ended
                && source.bridge_desynchronized
            {
                videos_needing_keyframes.push(*key);
            }
        }
        let projected_sources = sources
            .iter()
            .map(|source| source.key)
            .collect::<HashSet<_>>();
        let hidden_deliveries = state
            .deliveries
            .iter()
            .filter_map(|(delivery_id, pending)| {
                (!projected_sources.contains(&pending.source)).then_some(*delivery_id)
            })
            .collect::<Vec<_>>();
        let released = take_deliveries(&mut state, &hidden_deliveries);
        state.projected_sources = projected_sources;
        state.active_panes = panes.clone();
        let snapshot = ProjectionSnapshot {
            revision: state.revision,
            sources,
            nodes,
            live_nodes,
            videos_needing_keyframes,
        };
        drop(state);
        if !released.is_empty() {
            self.delivery_changed.notify_all();
        }
        return_delivery_credits(released);
        snapshot
    }

    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    pub fn deactivate_bridge(&self) {
        let released = {
            let mut state = self.lock();
            state.projected_sources.clear();
            for source in state.sources.values_mut() {
                if matches!(source.descriptor, SourceDescriptor::Video(_)) && source.playing {
                    source.bridge_desynchronized = true;
                }
            }
            let delivery_ids = state.deliveries.keys().copied().collect::<Vec<_>>();
            take_deliveries(&mut state, &delivery_ids)
        };
        if !released.is_empty() {
            self.delivery_changed.notify_all();
        }
        return_delivery_credits(released);
    }

    pub fn complete_bridge_delivery(&self, delivery_id: u64, delivered: bool) -> bool {
        let (writer, pending, request_keyframe) = {
            let mut state = self.lock();
            let Some(pending) = state.deliveries.remove(&delivery_id) else {
                return false;
            };
            state.queued_bridge_bytes = state
                .queued_bridge_bytes
                .saturating_sub(pending.bytes as usize);
            let writer = state
                .producers
                .get(&pending.source.0)
                .and_then(|producer| producer.writer.upgrade());
            let request_keyframe = !delivered
                && state.projected_sources.contains(&pending.source)
                && state
                    .sources
                    .get_mut(&pending.source)
                    .is_some_and(|source| {
                        if matches!(source.descriptor, SourceDescriptor::Video(_)) && !source.ended
                        {
                            source.bridge_desynchronized = true;
                            true
                        } else {
                            false
                        }
                    });
            (writer, pending, request_keyframe)
        };
        self.delivery_changed.notify_all();
        if let Some(writer) = writer {
            let _ = writer.write_record(
                messages::CREDIT,
                pending.source.1,
                &messages::credit(pending.bytes, 1, 0),
            );
        }
        if request_keyframe {
            self.request_keyframes(&[pending.source]);
        }
        request_keyframe
    }

    pub fn request_keyframes(&self, sources: &[SourceKey]) {
        let mut state = self.lock();
        for key in sources {
            let minimum_epoch = {
                let Some(source) = state.sources.get_mut(key) else {
                    continue;
                };
                if !matches!(source.descriptor, SourceDescriptor::Video(_)) || source.ended {
                    continue;
                }
                source.minimum_epoch = source
                    .minimum_epoch
                    .max(source.sequence.epoch())
                    .saturating_add(1);
                source.bridge_desynchronized = true;
                source.minimum_epoch
            };
            if let Some(writer) = state
                .producers
                .get(&key.0)
                .and_then(|producer| producer.writer.upgrade())
            {
                let _ = writer.write_record(
                    messages::NEED_KEYFRAME,
                    key.1,
                    &messages::need_keyframe(key.1, minimum_epoch, 2, None),
                );
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

type DeliveryCredit = (Option<Arc<Writer>>, PendingDelivery);

fn take_deliveries(state: &mut State, delivery_ids: &[u64]) -> Vec<DeliveryCredit> {
    let mut released = Vec::with_capacity(delivery_ids.len());
    for delivery_id in delivery_ids {
        let Some(pending) = state.deliveries.remove(delivery_id) else {
            continue;
        };
        state.queued_bridge_bytes = state
            .queued_bridge_bytes
            .saturating_sub(pending.bytes as usize);
        let writer = state
            .producers
            .get(&pending.source.0)
            .and_then(|producer| producer.writer.upgrade());
        released.push((writer, pending));
    }
    released
}

fn return_delivery_credits(released: Vec<DeliveryCredit>) {
    for (writer, pending) in released {
        if let Some(writer) = writer {
            let _ = writer.write_record(
                messages::CREDIT,
                pending.source.1,
                &messages::credit(pending.bytes, 1, 0),
            );
        }
    }
}

fn cancel_source_deliveries(
    shared: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
    source: SourceKey,
) {
    let released = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let delivery_ids = state
            .deliveries
            .iter()
            .filter_map(|(delivery_id, pending)| (pending.source == source).then_some(*delivery_id))
            .collect::<Vec<_>>();
        take_deliveries(&mut state, &delivery_ids)
    };
    if !released.is_empty() {
        delivery_changed.notify_all();
    }
    return_delivery_credits(released);
}

fn wait_for_source_deliveries(
    shared: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
    source: SourceKey,
) {
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while state
        .deliveries
        .values()
        .any(|pending| pending.source == source)
    {
        state = delivery_changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

impl Drop for VirtualVivid {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let cancellers = self
            .lock()
            .connection_cancellers
            .values()
            .map(|(_, cancel)| cancel.clone())
            .collect::<Vec<_>>();
        for cancel in cancellers {
            cancel.cancel();
        }
    }
}

fn accept_loop(
    listener: VirtualPresenterListener,
    state: Arc<Mutex<State>>,
    delivery_changed: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok(stream) => {
                let cancel = stream.cancel();
                let connection_id = {
                    let mut state = state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state.connections >= MAX_CONNECTIONS {
                        None
                    } else {
                        state.connections += 1;
                        state.next_connection = state.next_connection.wrapping_add(1).max(1);
                        let id = state.next_connection;
                        state
                            .connection_cancellers
                            .insert(id, (None, cancel.clone()));
                        Some(id)
                    }
                };
                let Some(connection_id) = connection_id else {
                    cancel.cancel();
                    continue;
                };
                let state = state.clone();
                let delivery_changed = delivery_changed.clone();
                thread::spawn(move || {
                    if let Err(_error) =
                        handle_connection(stream, connection_id, &state, &delivery_changed)
                    {
                        #[cfg(test)]
                        eprintln!("virtual Vivid connection failed: {_error}");
                    }
                    let mut state = state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.connection_cancellers.remove(&connection_id);
                    state.connections = state.connections.saturating_sub(1);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn handle_connection(
    stream: Transport,
    connection_id: u64,
    state: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
) -> io::Result<()> {
    stream.set_read_deadline(Duration::from_secs(3))?;
    let (mut reader, preface) = Reader::new(stream)?;
    match preface.kind {
        ConnectionKind::Control => {
            handle_control(&mut reader, connection_id, state, delivery_changed)
        }
        kind => handle_media(&mut reader, connection_id, state, delivery_changed, kind),
    }
}

fn mark_connection_pane(
    state: &Arc<Mutex<State>>,
    connection_id: u64,
    pane: PaneId,
) -> io::Result<()> {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !state.capabilities.contains_key(&pane) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pane capability was revoked during authentication",
        ));
    }
    let Some((owner, _)) = state.connection_cancellers.get_mut(&connection_id) else {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "Vivid connection was revoked during authentication",
        ));
    };
    *owner = Some(pane);
    Ok(())
}

fn handle_control(
    reader: &mut Reader,
    connection_id: u64,
    shared: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
) -> io::Result<()> {
    let writer = Arc::new(reader.writer()?);
    let hello_record = reader.read_record()?;
    if hello_record.record_type != messages::HELLO || hello_record.object_id != 0 {
        return Err(invalid("first Vivid control record is not HELLO"));
    }
    let (request_id, hello) = messages::parse_hello(&hello_record.body)?;
    let token =
        anchor::decode_token(&hello.token).map_err(|_| invalid("invalid pane capability"))?;
    let pane = {
        let state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        authenticate_pane(&state.capabilities, &token)
    };
    let Some(pane) = pane else {
        writer.write_record(
            messages::ERROR,
            0,
            &messages::error(
                request_id,
                messages::ERROR_AUTH_FAILED,
                "pane capability rejected",
            ),
        )?;
        return Ok(());
    };
    mark_connection_pane(shared, connection_id, pane)?;
    reader.clear_read_deadline()?;
    let unsupported = hello
        .required_features
        .iter()
        .any(|feature| !supported_feature(*feature));
    if unsupported {
        writer.write_record(
            messages::ERROR,
            0,
            &messages::error(
                request_id,
                messages::ERROR_UNSUPPORTED_FEATURE,
                "required feature unsupported by vvmux",
            ),
        )?;
        return Ok(());
    }
    let mut features = hello.required_features.clone();
    features.extend(
        hello
            .optional_features
            .iter()
            .copied()
            .filter(|feature| supported_feature(*feature)),
    );
    features.sort_unstable();
    features.dedup();
    let (producer_id, tag, root_context, display) = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.producers.len() >= MAX_PRODUCERS {
            writer.write_record(
                messages::ERROR,
                0,
                &messages::error(
                    request_id,
                    messages::ERROR_LIMIT_EXCEEDED,
                    "producer quota exceeded",
                ),
            )?;
            return Ok(());
        }
        state.next_producer = state
            .next_producer
            .checked_add(1)
            .ok_or_else(|| invalid("producer IDs exhausted"))?;
        let producer_id = state.next_producer;
        let mut tag = [0; 16];
        getrandom::fill(&mut tag).map_err(|error| io::Error::other(error.to_string()))?;
        let anchor_key = anchor::derive_key(&token, &tag);
        state.producers.insert(
            producer_id,
            Producer {
                pane,
                tag,
                anchor_key,
                writer: Arc::downgrade(&writer),
                features: features.iter().copied().collect(),
                anchors: HashMap::new(),
                seen_anchors: HashSet::new(),
            },
        );
        let display = state.metrics.get(&pane).copied().unwrap_or(DisplayChanged {
            display_generation: 1,
            viewport_width: 0,
            viewport_height: 0,
            grid_columns: 80,
            grid_rows: 22,
            cell_width: 0,
            cell_height: 0,
        });
        (producer_id, tag, (producer_id << 32) | 1, display)
    };
    writer.write_record(
        messages::WELCOME,
        0,
        &messages::welcome(
            request_id,
            producer_id,
            &tag,
            root_context,
            display,
            &features,
        ),
    )?;
    reader.set_maximum(hello.maximum_record_body);

    let result = loop {
        let record = match reader.read_record() {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(error) => break Err(error),
        };
        match dispatch_control(
            &record,
            producer_id,
            root_context,
            shared,
            delivery_changed,
            &writer,
        ) {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(error) => {
                let request = messages::decode_control(&record.body)
                    .map(|envelope| envelope.request_id)
                    .unwrap_or(0);
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(request, messages::ERROR_BAD_MESSAGE, &error.to_string()),
                )?;
            }
        }
    };
    cleanup_producer(
        &mut shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        producer_id,
        true,
    );
    result
}

fn dispatch_control(
    record: &Record,
    producer: ProducerId,
    root_context: u64,
    shared: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
    writer: &Arc<Writer>,
) -> io::Result<bool> {
    match record.record_type {
        messages::PROBE_VIDEO_CONFIG => {
            let (envelope, config) = messages::parse_create_video(&record.body)?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(invalid("video probes must be session-level"));
            }
            let supported = media::is_portable_packetization(&config.codec, &config.packetization);
            writer.write_record(
                messages::VIDEO_SUPPORT,
                0,
                &messages::video_support(envelope.request_id, supported, &config.codec),
            )?;
        }
        messages::PROBE_AUDIO_CONFIG => {
            let (envelope, config) = messages::parse_create_audio(&record.body)?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(invalid("audio probes must be session-level"));
            }
            let supported = messages::audio_config_supported(&config);
            writer.write_record(
                messages::AUDIO_SUPPORT,
                0,
                &messages::audio_support(envelope.request_id, supported, &config.codec),
            )?;
        }
        messages::CREATE_RASTER => {
            let (envelope, config) = messages::parse_create_raster(&record.body)?;
            create_source(
                shared,
                producer,
                SourceDescriptor::Raster(config.clone()),
                writer,
                envelope.request_id,
                record.object_id,
            )?;
        }
        messages::CREATE_IMAGE => {
            let (envelope, config) = messages::parse_create_image(&record.body)?;
            create_source(
                shared,
                producer,
                SourceDescriptor::Image(config.clone()),
                writer,
                envelope.request_id,
                record.object_id,
            )?;
        }
        messages::CREATE_VIDEO => {
            let (envelope, config) = messages::parse_create_video(&record.body)?;
            if !media::is_portable_packetization(&config.codec, &config.packetization) {
                return Err(invalid("unsupported video configuration"));
            }
            create_source(
                shared,
                producer,
                SourceDescriptor::Video(config.clone()),
                writer,
                envelope.request_id,
                record.object_id,
            )?;
        }
        messages::CREATE_AUDIO => {
            let (envelope, config) = messages::parse_create_audio(&record.body)?;
            if !messages::audio_config_supported(&config) {
                return Err(invalid("unsupported audio configuration"));
            }
            create_source(
                shared,
                producer,
                SourceDescriptor::Audio(config.clone()),
                writer,
                envelope.request_id,
                record.object_id,
            )?;
        }
        messages::BEGIN_TXN => {
            let envelope = messages::decode_control(&record.body)?;
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| invalid("transaction ID missing"))?;
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state
                .transactions
                .insert((producer, transaction), Vec::new())
                .is_some()
            {
                return Err(invalid("transaction already exists"));
            }
            writer.write_record(messages::OK, 0, &messages::ok(envelope.request_id))?;
        }
        messages::CREATE_NODE | messages::UPDATE_NODE => {
            let (envelope, node) = messages::parse_scene_node(&record.body)?;
            if node.node.context_id != root_context || node.node.node_id != record.object_id {
                return Err(invalid("node object or context mismatch"));
            }
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| invalid("transaction ID missing"))?;
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let features = &state
                .producers
                .get(&producer)
                .ok_or_else(|| invalid("producer missing"))?
                .features;
            if node.clip.is_some() && !features.contains(&messages::FEATURE_NODE_CLIP_RECT_V1) {
                return Err(invalid("node clip feature was not negotiated"));
            }
            let pane = state
                .producers
                .get(&producer)
                .ok_or_else(|| invalid("producer missing"))?
                .pane;
            let mutation = if record.record_type == messages::CREATE_NODE {
                Mutation::Create(SceneNode {
                    producer,
                    pane,
                    config: node,
                    retained_anchor: None,
                })
            } else {
                Mutation::Update(SceneNode {
                    producer,
                    pane,
                    config: node,
                    retained_anchor: None,
                })
            };
            state
                .transactions
                .get_mut(&(producer, transaction))
                .ok_or_else(|| invalid("transaction has not begun"))?
                .push(mutation);
            writer.write_record(
                messages::OK,
                record.object_id,
                &messages::ok(envelope.request_id),
            )?;
        }
        messages::DELETE_NODE => {
            let (envelope, node_id) = messages::parse_object_id(&record.body, "node ID")?;
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| invalid("transaction ID missing"))?;
            shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .transactions
                .get_mut(&(producer, transaction))
                .ok_or_else(|| invalid("transaction has not begun"))?
                .push(Mutation::Delete(producer, node_id));
            writer.write_record(
                messages::OK,
                record.object_id,
                &messages::ok(envelope.request_id),
            )?;
        }
        messages::COMMIT_TXN => {
            let envelope = messages::decode_control(&record.body)?;
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| invalid("transaction ID missing"))?;
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mutations = state
                .transactions
                .remove(&(producer, transaction))
                .ok_or_else(|| invalid("transaction has not begun"))?;
            apply_transaction(&mut state, producer, mutations)?;
            writer.write_record(messages::PRESENTED, 0, &messages::ok(envelope.request_id))?;
        }
        messages::ABORT_TXN => {
            let envelope = messages::decode_control(&record.body)?;
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| invalid("transaction ID missing"))?;
            shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .transactions
                .remove(&(producer, transaction));
            writer.write_record(messages::OK, 0, &messages::ok(envelope.request_id))?;
        }
        messages::DESTROY_SOURCE => {
            let (envelope, source_id) = messages::parse_object_id(&record.body, "source ID")?;
            remove_source(
                &mut shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                (producer, source_id),
            )?;
            writer.write_record(messages::OK, source_id, &messages::ok(envelope.request_id))?;
        }
        messages::PLAY | messages::PAUSE | messages::DRAIN => {
            let playing = record.record_type == messages::PLAY;
            let (envelope, source_id, play_request) = if playing {
                let (envelope, play) = messages::parse_play(&record.body)?;
                (envelope, play.source_id, Some(play))
            } else {
                let (envelope, source_id) = messages::parse_object_id(&record.body, "source ID")?;
                (envelope, source_id, None)
            };
            if !playing {
                cancel_source_deliveries(shared, delivery_changed, (producer, source_id));
            }
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let source = state
                .sources
                .get_mut(&(producer, source_id))
                .ok_or_else(|| invalid("source missing"))?;
            if !matches!(
                source.descriptor,
                SourceDescriptor::Video(_) | SourceDescriptor::Audio(_)
            ) {
                return Err(invalid("playback controls require a timed source"));
            }
            // A PLAY that changes the request while the source stays playing (keyframe
            // recovery re-basing the start PTS to the resume position) must reach the outer
            // bridge, or the outer presenter keeps a stale clock origin and schedules every
            // recovered frame far in the future.
            let changed = source.playing != playing
                || play_request.is_some_and(|request| request != source.play_request);
            source.playing = playing;
            if let Some(play_request) = play_request {
                source.play_request = play_request;
                if changed || source.clock_started.is_none() {
                    source.clock_started = Some(Instant::now());
                    source.clock_origin_pts_us = Some(play_request.start_pts_us);
                }
            } else {
                source.clock_started = None;
                source.clock_origin_pts_us = None;
            }
            if changed {
                state.revision = state.revision.wrapping_add(1);
            }
            writer.write_record(messages::OK, source_id, &messages::ok(envelope.request_id))?;
        }
        messages::FLUSH => {
            let (envelope, source_id, epoch) = messages::parse_eos(&record.body)?;
            cancel_source_deliveries(shared, delivery_changed, (producer, source_id));
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let source = state
                .sources
                .get_mut(&(producer, source_id))
                .ok_or_else(|| invalid("source missing"))?;
            source.sequence = MediaSequence::default();
            source.minimum_epoch = epoch;
            source.ended = false;
            source.bridge_desynchronized = true;
            writer.write_record(messages::OK, source_id, &messages::ok(envelope.request_id))?;
        }
        messages::EOS => {
            let (envelope, source_id, _epoch) = messages::parse_eos(&record.body)?;
            wait_for_source_deliveries(shared, delivery_changed, (producer, source_id));
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let source = state
                .sources
                .get_mut(&(producer, source_id))
                .ok_or_else(|| invalid("source missing"))?;
            source.ended = true;
            // EOS closes ingress but does not pause presentation. Vivi submits ahead, then keeps
            // the session alive while already-buffered media plays. Retain the current PLAY state
            // so projection reconciliation does not translate EOS into an outer PAUSE that stops
            // both video and linked audio before their queues are presented.
            if !source.descriptor.is_static() {
                source.retained = None;
                source.retained_bytes = 0;
            }
            state.revision = state.revision.wrapping_add(1);
            writer.write_record(messages::OK, source_id, &messages::ok(envelope.request_id))?;
        }
        messages::PING => {
            let envelope = messages::decode_control(&record.body)?;
            if record.object_id != 0 || envelope.request_id == 0 {
                return Err(invalid("PING is not a correlated session-level request"));
            }
            writer.write_record(messages::PONG, 0, &messages::ok(envelope.request_id))?;
        }
        messages::GOODBYE => {
            let envelope = messages::decode_control(&record.body)?;
            writer.write_record(messages::OK, 0, &messages::ok(envelope.request_id))?;
            return Ok(false);
        }
        _ if record.flags & vivid_protocol::wire::RECORD_OPTIONAL != 0 => {}
        _ => return Err(invalid("unsupported Vivid control record")),
    }
    Ok(true)
}

fn create_source(
    shared: &Arc<Mutex<State>>,
    producer: ProducerId,
    descriptor: SourceDescriptor,
    writer: &Arc<Writer>,
    request_id: u64,
    object_id: u64,
) -> io::Result<()> {
    let source_id = match &descriptor {
        SourceDescriptor::Raster(config) => config.source_id,
        SourceDescriptor::Image(config) => config.source_id,
        SourceDescriptor::Video(config) => config.source_id,
        SourceDescriptor::Audio(config) => config.source_id,
    };
    if source_id == 0 || object_id != source_id {
        return Err(invalid("source object ID mismatch"));
    }
    let maximum = descriptor.maximum_body()?;
    let mut ticket = [0; 32];
    getrandom::fill(&mut ticket).map_err(|error| io::Error::other(error.to_string()))?;
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.sources.len() >= state.config.max_sources {
        return Err(invalid("source quota exceeded"));
    }
    if maximum as usize > state.config.aggregate_retained_bytes as usize {
        return Err(invalid("source cannot receive one maximum legal record"));
    }
    if state.sources.contains_key(&(producer, source_id)) {
        return Err(invalid("source ID already exists"));
    }
    let kind = descriptor.kind();
    state.sources.insert(
        (producer, source_id),
        Source {
            owner: producer,
            descriptor,
            retained: None,
            sequence: MediaSequence::default(),
            retained_bytes: 0,
            playing: false,
            play_request: messages::PlayRequest::baseline(source_id, 0),
            ended: false,
            bridge_desynchronized: false,
            minimum_epoch: 0,
            last_pts_us: None,
            clock_started: None,
            clock_origin_pts_us: None,
        },
    );
    state.tickets.insert(
        ticket,
        Ticket {
            source: (producer, source_id),
            kind,
            maximum_body: maximum,
        },
    );
    // Media can arrive on the source channel before the session actor publishes the next
    // projection snapshot. A producer's prebuffer (which carries the opening keyframe) must not
    // be discarded as hidden in that window, so a source born into a projected pane is projected
    // immediately; the next snapshot recomputes the set authoritatively.
    if state.events.is_some()
        && state
            .producers
            .get(&producer)
            .is_some_and(|runtime| state.active_panes.contains(&runtime.pane))
    {
        state.projected_sources.insert((producer, source_id));
    }
    state.revision = state.revision.wrapping_add(1);
    drop(state);
    writer.write_record(
        messages::SOURCE_READY,
        source_id,
        &messages::source_ready(
            request_id,
            source_id,
            &ticket,
            Credits {
                bytes: u64::from(maximum),
                packets: INITIAL_PACKET_CREDITS,
                fragments: 0,
            },
            maximum,
        ),
    )
}

fn handle_media(
    reader: &mut Reader,
    connection_id: u64,
    shared: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
    kind: ConnectionKind,
) -> io::Result<()> {
    let attach = reader.read_record()?;
    if attach.record_type != messages::ATTACH_CHANNEL {
        return Err(invalid("media channel did not begin with ATTACH_CHANNEL"));
    }
    let ticket_bytes = messages::parse_attach_channel(&attach.body)?;
    let ticket_array: [u8; 32] = ticket_bytes
        .try_into()
        .map_err(|_| invalid("invalid media ticket"))?;
    let (ticket, pane) = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ticket = state
            .tickets
            .remove(&ticket_array)
            .ok_or_else(|| invalid("media ticket missing or already used"))?;
        let pane = state
            .producers
            .get(&ticket.source.0)
            .map(|producer| producer.pane)
            .ok_or_else(|| invalid("media ticket producer no longer exists"))?;
        (ticket, pane)
    };
    if ticket.kind != kind {
        return Err(invalid("media channel kind does not match ticket"));
    }
    mark_connection_pane(shared, connection_id, pane)?;
    reader.clear_read_deadline()?;
    reader.set_maximum(ticket.maximum_body);
    loop {
        let record = match reader.read_record() {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if record.object_id != ticket.source.1 {
            return Err(invalid("media object ID mismatch"));
        }
        ingest_record(shared, delivery_changed, ticket.source, &record)?;
    }
}

fn headless_playback_delay(
    state: &mut State,
    key: SourceKey,
    pts_us: Option<i64>,
) -> Option<Duration> {
    let pts_us = pts_us.filter(|pts| *pts != i64::MIN)?;
    let clock_key = match &state.sources.get(&key)?.descriptor {
        SourceDescriptor::Audio(config) => config
            .linked_video_source_id
            .map_or(key, |source_id| (key.0, source_id)),
        _ => key,
    };
    let clock = state.sources.get_mut(&clock_key)?;
    if !clock.playing {
        return None;
    }
    let started = clock.clock_started?;
    let origin = clock
        .clock_origin_pts_us
        .unwrap_or(clock.play_request.start_pts_us);
    playback_delay(started, origin, pts_us, Instant::now())
}

fn playback_delay(started: Instant, origin: i64, pts_us: i64, now: Instant) -> Option<Duration> {
    let relative_us = pts_us.saturating_sub(origin).max(0) as u64;
    started
        .checked_add(Duration::from_micros(relative_us))?
        .checked_duration_since(now)
}

fn ingest_record(
    shared: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
    key: SourceKey,
    record: &Record,
) -> io::Result<()> {
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let projected_source = state.projected_sources.contains(&key);
    let limit = state.config.aggregate_retained_bytes as usize;
    let bridge_queue_limit = state.config.ipc_queue_bytes;
    let writer = state
        .producers
        .get(&key.0)
        .and_then(|producer| producer.writer.upgrade());
    let (old_retained, new_retained, pts, candidate_forward) = {
        let source = state
            .sources
            .get_mut(&key)
            .ok_or_else(|| invalid("source no longer exists"))?;
        let new_retained = match (&source.descriptor, record.record_type) {
            (SourceDescriptor::Raster(config), messages::RASTER_FRAME) => {
                let parsed = media::parse_full_raster_frame(&record.body)?;
                if (parsed.width, parsed.height) != (config.width, config.height) {
                    return Err(invalid("raster dimensions changed"));
                }
                source.sequence.accept(parsed.frame_id, parsed.epoch)?;
                media::decode_raster_pixels(parsed)?;
                source.last_pts_us = Some(parsed.pts_us);
                Some(Arc::<[u8]>::from(record.body.clone()))
            }
            (SourceDescriptor::Image(config), messages::IMAGE_DATA) => {
                if source.retained.is_some() || record.body.len() != config.encoded_length as usize
                {
                    return Err(invalid("image body count or length is invalid"));
                }
                if let Some(expected) = config.sha256
                    && Sha256::digest(&record.body).as_slice() != expected
                {
                    return Err(invalid("image hash mismatch"));
                }
                let format = match config.encoding {
                    messages::IMAGE_PNG => image::ImageFormat::Png,
                    messages::IMAGE_JPEG => image::ImageFormat::Jpeg,
                    _ => return Err(invalid("unsupported image encoding")),
                };
                let decoded = image::load_from_memory_with_format(&record.body, format)
                    .map_err(|_| invalid("image decoder rejected body"))?;
                if (decoded.width(), decoded.height()) != (config.width, config.height) {
                    return Err(invalid("decoded image dimensions mismatch"));
                }
                Some(Arc::<[u8]>::from(record.body.clone()))
            }
            (SourceDescriptor::Video(config), messages::VIDEO_PACKET) => {
                let packet = media::parse_video_packet(&record.body)?;
                if packet.data.len() > config.max_access_unit_bytes as usize {
                    return Err(invalid("video access unit exceeds source maximum"));
                }
                source.sequence.accept(packet.packet_id, packet.epoch)?;
                source.last_pts_us = Some(packet.pts_us);
                let recovered = source.bridge_desynchronized
                    && packet.epoch >= source.minimum_epoch
                    && packet.flags & media::VIDEO_PACKET_KEY != 0;
                if recovered {
                    source.bridge_desynchronized = false;
                } else if !projected_source {
                    source.bridge_desynchronized = true;
                }
                None
            }
            (SourceDescriptor::Audio(config), messages::AUDIO_PACKET) => {
                let packet = media::parse_audio_packet(&record.body)?;
                if packet.data.len() > config.max_access_unit_bytes as usize {
                    return Err(invalid("audio access unit exceeds source maximum"));
                }
                source.sequence.accept(packet.packet_id, packet.epoch)?;
                source.last_pts_us = Some(packet.pts_us);
                None
            }
            _ => return Err(invalid("media record type does not match source")),
        };
        let old = source.retained_bytes;
        let new = new_retained.as_ref().map_or(0, |body| body.len());
        let forward = matches!(
            source.descriptor,
            SourceDescriptor::Video(_) | SourceDescriptor::Audio(_)
        ) && projected_source
            && !source.bridge_desynchronized;
        (old, new, source.last_pts_us, forward)
    };
    let forward_timed = candidate_forward
        && match &state.sources.get(&key).expect("source exists").descriptor {
            SourceDescriptor::Audio(config) => config
                .linked_video_source_id
                .and_then(|source_id| state.sources.get(&(key.0, source_id)))
                .is_none_or(|video| !video.bridge_desynchronized),
            _ => true,
        };
    let projected = state
        .retained_bytes
        .saturating_sub(old_retained)
        .saturating_add(new_retained);
    if projected > limit {
        return Err(invalid("aggregate retained media quota exceeded"));
    }
    let source = state.sources.get_mut(&key).unwrap();
    if new_retained > 0 {
        source.retained = Some(Arc::from(record.body.clone()));
        source.retained_bytes = new_retained;
    }
    source.last_pts_us = pts;
    state.retained_bytes = projected;
    if new_retained > 0 {
        state.revision = state.revision.wrapping_add(1);
    }
    let headless_delay = (!projected_source)
        .then(|| headless_playback_delay(&mut state, key, pts))
        .flatten();
    let delivery = if forward_timed
        && let Some(events) = state.events.clone()
        && (state.queued_bridge_bytes == 0
            || state.queued_bridge_bytes.saturating_add(record.body.len()) <= bridge_queue_limit)
    {
        state.next_delivery_id = state
            .next_delivery_id
            .checked_add(1)
            .ok_or_else(|| invalid("bridge delivery IDs exhausted"))?;
        let delivery_id = state.next_delivery_id;
        state.queued_bridge_bytes = state.queued_bridge_bytes.saturating_add(record.body.len());
        state.deliveries.insert(
            delivery_id,
            PendingDelivery {
                source: key,
                bytes: record.body.len() as u64,
            },
        );
        Some((delivery_id, events))
    } else {
        if forward_timed
            && let Some(source) = state.sources.get_mut(&key)
            && matches!(source.descriptor, SourceDescriptor::Video(_))
        {
            source.bridge_desynchronized = true;
        }
        None
    };
    drop(state);
    if let Some(delay) = headless_delay {
        thread::sleep(delay);
    }
    if let Some((delivery_id, events)) = delivery {
        match events.try_send(MediaEvent {
            delivery_id,
            source: key,
            record_type: record.record_type,
            body: record.body.clone(),
        }) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => {
                let mut state = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(pending) = state.deliveries.remove(&delivery_id) {
                    state.queued_bridge_bytes = state
                        .queued_bridge_bytes
                        .saturating_sub(pending.bytes as usize);
                    delivery_changed.notify_all();
                }
                if let Some(source) = state.sources.get_mut(&key)
                    && matches!(source.descriptor, SourceDescriptor::Video(_))
                {
                    source.bridge_desynchronized = true;
                }
            }
        }
    }
    if let Some(writer) = writer {
        writer.write_record(
            messages::CREDIT,
            key.1,
            &messages::credit(record.body.len() as u64, 1, 0),
        )?;
    }
    Ok(())
}

fn apply_transaction(
    state: &mut State,
    producer: ProducerId,
    mutations: Vec<Mutation>,
) -> io::Result<()> {
    let mut nodes = state.nodes.clone();
    for mutation in mutations {
        match mutation {
            Mutation::Create(node) => {
                validate_node(state, producer, &node)?;
                let key = (producer, node.config.node.node_id);
                if nodes.insert(key, node).is_some() {
                    return Err(invalid("node ID already exists"));
                }
            }
            Mutation::Update(node) => {
                validate_node(state, producer, &node)?;
                let key = (producer, node.config.node.node_id);
                if !nodes.contains_key(&key) {
                    return Err(invalid("updated node does not exist"));
                }
                nodes.insert(key, node);
            }
            Mutation::Delete(owner, node_id) => {
                if nodes.remove(&(owner, node_id)).is_none() {
                    return Err(invalid("deleted node does not exist"));
                }
            }
        }
    }
    if nodes.len() > state.config.max_nodes {
        return Err(invalid("node quota exceeded"));
    }
    state.nodes = nodes;
    state.revision = state.revision.wrapping_add(1);
    Ok(())
}

fn validate_node(state: &State, producer: ProducerId, node: &SceneNode) -> io::Result<()> {
    if node.producer != producer
        || !state
            .sources
            .contains_key(&(producer, node.config.node.source_id))
    {
        return Err(invalid("node source is outside producer scope"));
    }
    if let Some(anchor_id) = node.config.node.anchor_id {
        let producer = state
            .producers
            .get(&producer)
            .ok_or_else(|| invalid("producer missing"))?;
        // ConPTY can hold the marker until the producer emits more output, so a Windows
        // producer commits the node without waiting for ANCHOR_READY. Permit the commit;
        // projection snapshots keep the node hidden until the matching marker registers.
        if !producer.anchors.contains_key(&anchor_id) && cfg!(not(windows)) {
            return Err(invalid("node anchor does not exist"));
        }
    }
    Ok(())
}

fn remove_source(state: &mut State, key: SourceKey) -> io::Result<()> {
    let source = state
        .sources
        .remove(&key)
        .ok_or_else(|| invalid("source does not exist"))?;
    state.retained_bytes = state.retained_bytes.saturating_sub(source.retained_bytes);
    state
        .nodes
        .retain(|_, node| node.producer != key.0 || node.config.node.source_id != key.1);
    state.revision = state.revision.wrapping_add(1);
    Ok(())
}

fn cleanup_producer(state: &mut State, producer: ProducerId, preserve_anchored_static: bool) {
    let Some(runtime) = state.producers.remove(&producer) else {
        return;
    };
    state
        .transactions
        .retain(|(owner, _), _| *owner != producer);
    state
        .tickets
        .retain(|_, ticket| ticket.source.0 != producer);
    for node in state
        .nodes
        .values_mut()
        .filter(|node| node.producer == producer)
    {
        node.retained_anchor = node
            .config
            .node
            .anchor_id
            .and_then(|anchor_id| runtime.anchors.get(&anchor_id).copied());
    }
    state.nodes.retain(|_, node| {
        node.producer != producer
            || (preserve_anchored_static
                && node.retained_anchor.is_some()
                && state
                    .sources
                    .get(&(producer, node.config.node.source_id))
                    .is_some_and(|source| {
                        source.descriptor.is_static() && source.retained.is_some()
                    }))
    });
    prune_orphaned_sources(state);
    state.revision = state.revision.wrapping_add(1);
}

fn prune_orphaned_sources(state: &mut State) {
    let referenced = state
        .nodes
        .values()
        .map(|node| (node.producer, node.config.node.source_id))
        .collect::<HashSet<_>>();
    let removed = state
        .sources
        .iter()
        .filter(|(key, _)| !state.producers.contains_key(&key.0) && !referenced.contains(key))
        .map(|(key, source)| (*key, source.retained_bytes))
        .collect::<Vec<_>>();
    for (key, bytes) in removed {
        state.sources.remove(&key);
        state.retained_bytes = state.retained_bytes.saturating_sub(bytes);
    }
}

fn supported_feature(feature: u64) -> bool {
    matches!(
        feature,
        messages::FEATURE_RASTER_RGBA8
            | messages::FEATURE_SCENE_TRANSACTIONS
            | messages::FEATURE_GRID_CELL_NODES
            | messages::FEATURE_CREDIT_FLOW_CONTROL
            | messages::FEATURE_ENCODED_IMAGE_V1
            | messages::FEATURE_RASTER_ZSTD_V1
            | messages::FEATURE_RASTER_PREMULTIPLIED_ALPHA
            | messages::FEATURE_VISIBILITY_EVENTS_V1
            | messages::FEATURE_VIDEO_ACCESS_UNIT_V1
            | messages::FEATURE_VIDEO_CONTROL_V1
            | messages::FEATURE_TEXT_ANCHORS_V2
            | messages::FEATURE_AUDIO_ACCESS_UNIT_V1
            | messages::FEATURE_NODE_CLIP_RECT_V1
    )
}

fn authenticate_pane(capabilities: &HashMap<PaneId, [u8; 32]>, token: &[u8; 32]) -> Option<PaneId> {
    let mut result = None;
    for (pane, capability) in capabilities {
        let difference = capability
            .iter()
            .zip(token.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            });
        if difference == 0 {
            result = Some(*pane);
        }
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::wire::{Connection, Endpoint};

    fn test_virtual_endpoint(
        directory: &tempfile::TempDir,
        name: &str,
    ) -> VirtualPresenterEndpoint {
        #[cfg(unix)]
        {
            directory.path().join(name)
        }
        #[cfg(windows)]
        {
            let _ = (directory, name);
            std::path::PathBuf::new()
        }
    }

    fn service_endpoint(service: &VirtualVivid) -> Endpoint {
        Endpoint::parse(&service.endpoint()).unwrap()
    }

    fn state() -> State {
        State {
            config: MediaConfig::default(),
            capabilities: HashMap::new(),
            metrics: HashMap::new(),
            producers: HashMap::new(),
            sources: HashMap::new(),
            nodes: HashMap::new(),
            transactions: HashMap::new(),
            tickets: HashMap::new(),
            next_producer: 0,
            retained_bytes: 0,
            connections: 0,
            revision: 0,
            projected_sources: HashSet::new(),
            active_panes: HashSet::new(),
            deliveries: HashMap::new(),
            next_delivery_id: 0,
            queued_bridge_bytes: 0,
            events: None,
            next_connection: 0,
            connection_cancellers: HashMap::new(),
        }
    }

    #[test]
    fn capability_lookup_is_pane_scoped() {
        let mut state = state();
        state.capabilities.insert(7, [1; 32]);
        state.capabilities.insert(9, [2; 32]);
        assert_eq!(authenticate_pane(&state.capabilities, &[1; 32]), Some(7));
        assert_eq!(authenticate_pane(&state.capabilities, &[3; 32]), None);
    }

    #[test]
    fn transaction_failure_is_atomic() {
        let mut state = state();
        let node = SceneNode {
            producer: 1,
            pane: 1,
            config: ParsedSceneNode {
                node: messages::ParsedNodeConfig {
                    node_id: 1,
                    source_id: 99,
                    context_id: 1,
                    x: 0,
                    y: 0,
                    width: 1_i64 << 32,
                    height: 1_i64 << 32,
                    text_layer: 1,
                    z_index: 0,
                    visible: true,
                    anchor_id: None,
                },
                clip: Some(messages::ClipRect {
                    x: 0,
                    y: 0,
                    width: 1_i64 << 32,
                    height: 1_i64 << 32,
                }),
            },
            retained_anchor: None,
        };
        assert!(apply_transaction(&mut state, 1, vec![Mutation::Create(node)]).is_err());
        assert!(state.nodes.is_empty());
    }

    #[test]
    fn timed_media_has_no_retained_payload_contract() {
        let descriptor = SourceDescriptor::Video(ParsedVideoSourceConfig {
            source_id: 1,
            codec: "h264".into(),
            packetization: "annex-b-au-v1".into(),
            extradata: Vec::new(),
            width: 10,
            height: 10,
            profile: 0,
            level: 0,
            bitrate: 0,
            color_primaries: 0,
            transfer: 0,
            matrix: 0,
            range: 0,
            sar_num: 1,
            sar_den: 1,
            max_access_unit_bytes: 1024,
        });
        assert!(!descriptor.is_static());
    }

    #[test]
    fn hidden_media_credit_follows_the_playback_clock() {
        let started = Instant::now();
        let now = started + Duration::from_millis(75);
        assert_eq!(
            playback_delay(started, 1_000_000, 1_200_000, now),
            Some(Duration::from_millis(125))
        );
        assert_eq!(
            playback_delay(started, 1_000_000, 1_050_000, now),
            None,
            "late hidden media must be discarded without adding latency"
        );
    }

    #[test]
    fn virtual_presenter_negotiates_video_and_rekeys_only_after_bridge_loss() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid.sock");
        let service = match VirtualVivid::start(socket, MediaConfig::default()) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        service.update_metrics(7, 80, 22, (10, 20));

        let endpoint = service_endpoint(&service);
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))
            .unwrap();
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::WELCOME
        );

        let video = messages::VideoSourceConfig {
            source_id: 0,
            codec: "h264",
            packetization: "h264-annexb-au-v1",
            extradata: &[],
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
        };
        control
            .write_record(
                messages::PROBE_VIDEO_CONFIG,
                0,
                0,
                &messages::probe_video_config(2, &video),
            )
            .unwrap();
        let support = control.read_record().unwrap();
        assert_eq!(
            support.record_type,
            messages::VIDEO_SUPPORT,
            "{:?}",
            messages::parse_error_reply(&support.body)
        );
        assert!(messages::parse_video_support(&support.body).unwrap());

        let audio = messages::AudioSourceConfig {
            source_id: 0,
            linked_video_source_id: None,
            codec: "pcm_s16le",
            packetization: "pcm-packet-v1",
            extradata: &[],
            sample_rate: 48_000,
            channels: 2,
            channel_mask: 3,
            bitrate: 0,
            max_access_unit_bytes: 4096,
        };
        control
            .write_record(
                messages::PROBE_AUDIO_CONFIG,
                0,
                0,
                &messages::probe_audio_config(3, &audio),
            )
            .unwrap();
        let support = control.read_record().unwrap();
        assert_eq!(support.record_type, messages::AUDIO_SUPPORT);
        assert!(messages::parse_audio_support(&support.body).unwrap());

        let video = messages::VideoSourceConfig {
            source_id: 9,
            ..video
        };
        control
            .write_record(
                messages::CREATE_VIDEO,
                0,
                9,
                &messages::create_video(4, &video),
            )
            .unwrap();
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::SOURCE_READY
        );
        let before_play = service.projection_snapshot(&HashSet::from([7]));
        assert!(before_play.videos_needing_keyframes.is_empty());

        control
            .write_record(messages::PLAY, 0, 9, &messages::play(5, 9, 250_000))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        let playing = service.projection_snapshot(&HashSet::from([7]));
        assert_ne!(playing.revision, before_play.revision);
        assert!(playing.videos_needing_keyframes.is_empty());
        assert!(playing.sources[0].playing);
        assert_eq!(playing.sources[0].play_request.minimum_buffer_us, 250_000);
        let key = playing.sources[0].key;

        service.request_keyframes(&[key]);
        let need_keyframe = control.read_record().unwrap();
        assert_eq!(need_keyframe.record_type, messages::NEED_KEYFRAME);
        let recovering = service.projection_snapshot(&HashSet::from([7]));
        assert_eq!(recovering.videos_needing_keyframes, vec![key]);
        let recovery_revision = recovering.revision;

        control
            .write_record(messages::FLUSH, 0, 9, &messages::flush(6, 9, 2))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        control
            .write_record(messages::PLAY, 0, 9, &messages::play(7, 9, 250_000))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        assert_eq!(
            service.revision(),
            recovery_revision,
            "keyframe recovery FLUSH/PLAY must not trigger another bridge rebuild"
        );

        let mut rebased = messages::PlayRequest::baseline(9, 250_000);
        rebased.start_pts_us = 30_000_000;
        control
            .write_record(messages::PLAY, 0, 9, &messages::play_request(8, &rebased))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        assert_ne!(
            service.revision(),
            recovery_revision,
            "a recovery PLAY re-basing the start PTS must reach the outer bridge"
        );
        let rebased_snapshot = service.projection_snapshot(&HashSet::from([7]));
        assert_eq!(
            rebased_snapshot.sources[0].play_request.start_pts_us,
            30_000_000
        );

        control
            .write_record(messages::GOODBYE, 0, 0, &messages::goodbye(9))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
    }

    #[test]
    fn projected_video_credit_waits_for_bridge_and_hidden_audio_is_discarded() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid-paced.sock");
        let (event_sender, event_receiver) = mpsc::sync_channel(8);
        let service = match VirtualVivid::start_with_events(
            socket,
            MediaConfig::default(),
            Some(event_sender),
        ) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        service.update_metrics(7, 80, 22, (10, 20));

        let endpoint = service_endpoint(&service);
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
        };
        control
            .write_record(
                messages::CREATE_VIDEO,
                0,
                9,
                &messages::create_video(2, &video),
            )
            .unwrap();
        let ready = messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();
        assert_eq!(ready.packet_credits, 1);
        service.projection_snapshot(&HashSet::from([7]));
        let mut video_media = Connection::open(&endpoint, ConnectionKind::Video).unwrap();
        video_media
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                9,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();
        let packet = media::video_packet_body(media::VideoPacket {
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
            .write_record(messages::VIDEO_PACKET, 0, 9, &packet)
            .unwrap();
        let event = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(event.source.1, 9);

        let (reply_sender, reply_receiver) = mpsc::channel();
        thread::spawn(move || {
            control
                .write_record(messages::EOS, 0, 9, &messages::eos(3, 9, 1))
                .unwrap();
            let credit = control.read_record().unwrap();
            let eos = control.read_record().unwrap();
            reply_sender.send((control, credit, eos)).unwrap();
        });
        assert!(
            reply_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "video credit and EOS must wait for outer bridge completion"
        );
        assert!(!service.complete_bridge_delivery(event.delivery_id, true));
        let (mut control, credit, eos) =
            reply_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(credit.record_type, messages::CREDIT);
        assert_eq!(credit.object_id, 9);
        assert_eq!(eos.record_type, messages::OK);
        assert_eq!(messages::request_id(&eos.body).unwrap(), 3);

        let audio = messages::AudioSourceConfig {
            source_id: 10,
            linked_video_source_id: None,
            codec: "pcm_s16le",
            packetization: "pcm-packet-v1",
            extradata: &[],
            sample_rate: 48_000,
            channels: 2,
            channel_mask: 3,
            bitrate: 0,
            max_access_unit_bytes: 4096,
        };
        control
            .write_record(
                messages::CREATE_AUDIO,
                0,
                10,
                &messages::create_audio(4, &audio),
            )
            .unwrap();
        let ready = messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();
        service.projection_snapshot(&HashSet::from([8]));
        let mut audio_media = Connection::open(&endpoint, ConnectionKind::Audio).unwrap();
        audio_media
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                10,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();
        let packet = media::audio_packet_body(media::AudioPacket {
            epoch: 1,
            packet_id: 1,
            pts_us: 0,
            dts_us: 0,
            duration_us: 20_000,
            trim_start_samples: 0,
            trim_end_samples: 0,
            data: &[0; 16],
        })
        .unwrap();
        audio_media
            .write_record(messages::AUDIO_PACKET, 0, 10, &packet)
            .unwrap();
        let credit = control.read_record().unwrap();
        assert_eq!(credit.record_type, messages::CREDIT);
        assert_eq!(credit.object_id, 10);
        assert!(
            event_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );

        let hidden_video = messages::VideoSourceConfig {
            source_id: 11,
            ..video
        };
        control
            .write_record(
                messages::CREATE_VIDEO,
                0,
                11,
                &messages::create_video(5, &hidden_video),
            )
            .unwrap();
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::SOURCE_READY
        );
        control
            .write_record(messages::PLAY, 0, 11, &messages::play(6, 11, 100_000))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);

        let linked_audio = messages::AudioSourceConfig {
            source_id: 12,
            linked_video_source_id: Some(11),
            ..audio
        };
        control
            .write_record(
                messages::CREATE_AUDIO,
                0,
                12,
                &messages::create_audio(7, &linked_audio),
            )
            .unwrap();
        let ready = messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();
        service.projection_snapshot(&HashSet::from([8]));
        let mut linked_audio_media = Connection::open(&endpoint, ConnectionKind::Audio).unwrap();
        linked_audio_media
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                12,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();
        for (packet_id, pts_us) in [(1, 0), (2, 150_000)] {
            let packet = media::audio_packet_body(media::AudioPacket {
                epoch: 1,
                packet_id,
                pts_us,
                dts_us: pts_us,
                duration_us: 20_000,
                trim_start_samples: 0,
                trim_end_samples: 0,
                data: &[0; 16],
            })
            .unwrap();
            let started = Instant::now();
            linked_audio_media
                .write_record(messages::AUDIO_PACKET, 0, 12, &packet)
                .unwrap();
            let credit = control.read_record().unwrap();
            assert_eq!(credit.record_type, messages::CREDIT);
            assert_eq!(credit.object_id, 12);
            if packet_id == 2 {
                assert!(
                    started.elapsed() >= Duration::from_millis(100),
                    "hidden linked audio must advance at media time instead of racing to EOS"
                );
            }
        }
    }

    #[test]
    fn virtual_presenter_acknowledges_anchor_and_retains_static_media() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid.sock");
        let service = match VirtualVivid::start(socket, MediaConfig::default()) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        service.update_metrics(7, 80, 22, (10, 20));

        let endpoint = service_endpoint(&service);
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))
            .unwrap();
        let welcome_record = control.read_record().unwrap();
        assert_eq!(welcome_record.record_type, messages::WELCOME);
        let welcome = messages::parse_welcome(&welcome_record.body).unwrap();
        assert_eq!((welcome.grid_columns, welcome.grid_rows), (80, 22));

        let token_bytes = anchor::decode_token(&token).unwrap();
        let session_tag: [u8; 16] = welcome.session_tag.as_slice().try_into().unwrap();
        let anchor_key = anchor::derive_key(&token_bytes, &session_tag);
        let marker = anchor::encode_marker(&anchor_key, &session_tag, 77).unwrap();
        let marker_payload = &marker[2..marker.len() - 2];
        assert!(service.observe_marker(7, marker_payload, 3, 4));
        let anchor_ready = control.read_record().unwrap();
        assert_eq!(anchor_ready.record_type, messages::ANCHOR_READY);
        assert_eq!(anchor_ready.object_id, 77);
        assert_eq!(
            messages::parse_anchor_event(&anchor_ready.body).unwrap(),
            77
        );
        assert!(
            !service.observe_marker(7, marker_payload, 9, 9),
            "authenticated anchor markers are single-use"
        );

        let mut encoded = Vec::new();
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .unwrap();
        let image_hash: [u8; 32] = Sha256::digest(&encoded).into();
        control
            .write_record(
                messages::CREATE_IMAGE,
                0,
                1,
                &messages::create_image(
                    2,
                    &messages::ImageSourceConfig {
                        source_id: 1,
                        encoding: messages::IMAGE_PNG,
                        width: 1,
                        height: 1,
                        encoded_length: encoded.len() as u32,
                        sha256: Some(image_hash),
                    },
                ),
            )
            .unwrap();
        let image_ready =
            messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();
        let transaction_id = 9;
        control
            .write_record(
                messages::BEGIN_TXN,
                0,
                0,
                &messages::begin_transaction(3, transaction_id),
            )
            .unwrap();
        control
            .write_record(
                messages::CREATE_NODE,
                0,
                10,
                &messages::create_node(
                    4,
                    transaction_id,
                    messages::NodeConfig {
                        node_id: 10,
                        source_id: 1,
                        context_id: welcome.root_context_id,
                        columns: 1,
                        rows: 1,
                        anchor_id: Some(77),
                    },
                ),
            )
            .unwrap();
        control
            .write_record(
                messages::COMMIT_TXN,
                0,
                0,
                &messages::commit_transaction(5, transaction_id, welcome.display_generation),
            )
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::PRESENTED
        );
        let mut image_connection = Connection::open(&endpoint, ConnectionKind::Blob).unwrap();
        image_connection
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                1,
                &messages::attach_channel(&image_ready.media_ticket),
            )
            .unwrap();
        image_connection
            .write_record(messages::IMAGE_DATA, 0, 1, &encoded)
            .unwrap();
        let image_credit = control.read_record().unwrap();
        assert_eq!(image_credit.record_type, messages::CREDIT);
        assert_eq!(image_credit.object_id, 1);
        let credit = messages::parse_credit(&image_credit.body).unwrap();
        assert_eq!(credit.bytes, encoded.len() as u64);
        assert_eq!(credit.packets, 1);
        assert_eq!(credit.fragments, 0);

        control
            .write_record(
                messages::CREATE_RASTER,
                0,
                2,
                &messages::create_raster(6, 2, 2, 1),
            )
            .unwrap();
        let ready_record = control.read_record().unwrap();
        assert_eq!(ready_record.record_type, messages::SOURCE_READY);
        let ready = messages::parse_source_ready(&ready_record.body).unwrap();
        let mut media_connection = Connection::open(&endpoint, ConnectionKind::Raster).unwrap();
        media_connection
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                2,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();
        let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
        let body = media::raster_frame_body(0, 1, 2, 1, &pixels).unwrap();
        media_connection
            .write_record(messages::RASTER_FRAME, 0, 2, &body)
            .unwrap();
        let raster_credit = control.read_record().unwrap();
        assert_eq!(raster_credit.record_type, messages::CREDIT);
        assert_eq!(raster_credit.object_id, 2);

        let mut retained = false;
        for _ in 0..50 {
            let snapshot = service.projection_snapshot(&HashSet::from([7]));
            let image = snapshot.sources.iter().find(|source| source.key.1 == 1);
            let raster = snapshot.sources.iter().find(|source| source.key.1 == 2);
            if image.is_some_and(|source| source.retained.is_some())
                && raster.is_some_and(|source| source.retained.is_some())
            {
                assert_eq!(image.unwrap().retained.as_deref(), Some(encoded.as_slice()));
                assert_eq!(raster.unwrap().retained.as_deref(), Some(body.as_slice()));
                retained = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(retained, "virtual presenter did not retain static media");

        control
            .write_record(messages::GOODBYE, 0, 0, &messages::goodbye(7))
            .unwrap();
        let goodbye = control.read_record().unwrap();
        assert_eq!(goodbye.record_type, messages::OK);
        assert_eq!(messages::request_id(&goodbye.body).unwrap(), 7);
        drop(control);

        for _ in 0..50 {
            if service.lock().producers.is_empty() {
                let snapshot = service.projection_snapshot(&HashSet::from([7]));
                let image = snapshot.sources.iter().find(|source| source.key.1 == 1);
                assert_eq!(
                    image.and_then(|source| source.retained.as_deref()),
                    Some(encoded.as_slice())
                );
                assert_eq!(snapshot.nodes.len(), 1);
                assert_eq!(snapshot.nodes[0].config.node.source_id, 1);
                assert_eq!(snapshot.nodes[0].config.node.anchor_id, None);
                assert_eq!(snapshot.nodes[0].config.node.x, 4_i64 << 32);
                assert_eq!(snapshot.nodes[0].config.node.y, 3_i64 << 32);
                let other_tab = service.projection_snapshot(&HashSet::from([8]));
                assert!(other_tab.nodes.is_empty());
                assert!(other_tab.sources.is_empty());
                assert_eq!(
                    other_tab.live_nodes.len(),
                    1,
                    "inactive-tab nodes remain live for fragment identity"
                );
                let first_tab = service.projection_snapshot(&HashSet::from([7]));
                assert_eq!(first_tab.nodes.len(), 1);
                assert_eq!(first_tab.sources.len(), 1);
                service.clear_anchors(7);
                let cleared = service.projection_snapshot(&HashSet::from([7]));
                assert!(cleared.nodes.is_empty());
                assert!(cleared.sources.is_empty());
                assert!(cleared.live_nodes.is_empty());
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("producer did not disconnect after acknowledged GOODBYE");
    }

    #[test]
    fn prebuffer_video_is_forwarded_before_the_next_projection_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid-prebuffer.sock");
        let (event_sender, event_receiver) = mpsc::sync_channel(8);
        let service = match VirtualVivid::start_with_events(
            socket,
            MediaConfig::default(),
            Some(event_sender),
        ) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        service.update_metrics(7, 80, 22, (10, 20));
        // The session keeps the projection current, so the last snapshot predates this producer.
        service.projection_snapshot(&HashSet::from([7]));

        let endpoint = service_endpoint(&service);
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
        };
        control
            .write_record(
                messages::CREATE_VIDEO,
                0,
                9,
                &messages::create_video(2, &video),
            )
            .unwrap();
        let ready = messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();

        // No projection snapshot runs between SOURCE_READY and the prebuffer. The opening
        // keyframe must reach the bridge anyway instead of being discarded as hidden, which
        // would leave the stream desynchronized until a later keyframe and skip the start.
        let mut video_media = Connection::open(&endpoint, ConnectionKind::Video).unwrap();
        video_media
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                9,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();
        let packet = media::video_packet_body(media::VideoPacket {
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
            .write_record(messages::VIDEO_PACKET, 0, 9, &packet)
            .unwrap();
        let event = event_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("prebuffer keyframe was discarded before the first snapshot");
        assert_eq!(event.source.1, 9);
        assert_eq!(event.record_type, messages::VIDEO_PACKET);
    }

    #[cfg(windows)]
    #[test]
    fn windows_node_commits_before_conpty_anchor_marker_arrives() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid.sock");
        let service = match VirtualVivid::start(socket, MediaConfig::default()) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        service.update_metrics(7, 80, 22, (10, 20));

        let endpoint = service_endpoint(&service);
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))
            .unwrap();
        let welcome_record = control.read_record().unwrap();
        assert_eq!(welcome_record.record_type, messages::WELCOME);
        let welcome = messages::parse_welcome(&welcome_record.body).unwrap();

        let mut encoded = Vec::new();
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .unwrap();
        let image_hash: [u8; 32] = Sha256::digest(&encoded).into();
        control
            .write_record(
                messages::CREATE_IMAGE,
                0,
                1,
                &messages::create_image(
                    2,
                    &messages::ImageSourceConfig {
                        source_id: 1,
                        encoding: messages::IMAGE_PNG,
                        width: 1,
                        height: 1,
                        encoded_length: encoded.len() as u32,
                        sha256: Some(image_hash),
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::SOURCE_READY
        );

        let transaction_id = 9;
        control
            .write_record(
                messages::BEGIN_TXN,
                0,
                0,
                &messages::begin_transaction(3, transaction_id),
            )
            .unwrap();
        control
            .write_record(
                messages::CREATE_NODE,
                0,
                10,
                &messages::create_node(
                    4,
                    transaction_id,
                    messages::NodeConfig {
                        node_id: 10,
                        source_id: 1,
                        context_id: welcome.root_context_id,
                        columns: 1,
                        rows: 1,
                        anchor_id: Some(91),
                    },
                ),
            )
            .unwrap();
        control
            .write_record(
                messages::COMMIT_TXN,
                0,
                0,
                &messages::commit_transaction(5, transaction_id, welcome.display_generation),
            )
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::PRESENTED
        );

        let hidden = service.projection_snapshot(&HashSet::from([7]));
        assert!(
            hidden.nodes.is_empty(),
            "node with an unseen ConPTY anchor must stay hidden"
        );
        assert_eq!(hidden.live_nodes.len(), 1);

        let token_bytes = anchor::decode_token(&token).unwrap();
        let session_tag: [u8; 16] = welcome.session_tag.as_slice().try_into().unwrap();
        let anchor_key = anchor::derive_key(&token_bytes, &session_tag);
        let marker = anchor::encode_marker(&anchor_key, &session_tag, 91).unwrap();
        assert!(service.observe_marker(7, &marker[2..marker.len() - 2], 5, 6));
        let anchor_ready = control.read_record().unwrap();
        assert_eq!(anchor_ready.record_type, messages::ANCHOR_READY);
        assert_eq!(anchor_ready.object_id, 91);

        let resolved = service.projection_snapshot(&HashSet::from([7]));
        assert_eq!(resolved.nodes.len(), 1);
        assert_eq!(resolved.nodes[0].config.node.anchor_id, None);
        assert_eq!(resolved.nodes[0].config.node.x, 6_i64 << 32);
        assert_eq!(resolved.nodes[0].config.node.y, 5_i64 << 32);
    }
}
