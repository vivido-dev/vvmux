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
    self, DisplayChanged, ImageSourceConfig, ParsedAudioSourceConfig, ParsedSceneNode,
    ParsedVideoSourceConfig, RasterSourceConfig, RasterUpdateConfig, SourceReady,
};
use vivid_protocol::revision::{ObservationSequence, SceneRevision, SourceRevision};
use vivid_protocol::trace::{TraceComponent, TraceGuard, TraceHop};
use vivid_protocol::wire::{BorrowedRecord, ConnectionKind, Record};
use vivid_protocol::{VIVID_MAJOR, VIVID_MINOR};

use crate::config::Media as MediaConfig;
use crate::ipc::{PaneMediaNodeStatus, PaneMediaSourceStatus, PaneMediaStatus};
use crate::layout::PaneId;
use crate::platform::{
    ConnectionCancel, Transport, VirtualPresenterEndpoint, VirtualPresenterListener,
};
use crate::vivid_transport::{Reader, TraceChannel, Writer};

const MAX_PRODUCERS: usize = 16;
const MAX_CONNECTIONS: usize = 64;
const MAX_SEEN_ANCHORS: usize = 4096;
// Keep at most one timed packet ahead of the virtual presenter. Besides bounding pre-roll, this
// forces Vivi to observe an unsolicited NEED_KEYFRAME within one discarded packet after a pane is
// projected again instead of consuming a large local credit window before reading control events.
const INITIAL_PACKET_CREDITS: u64 = 1;
const ROLLING_PACKET_CREDITS: u64 = 8;
const MAX_REGISTERED_WAITS: usize = 64;
const MAX_PENDING_MEDIA_BARRIERS: usize = 64;
const MEDIA_ORDER_BARRIER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const OBSERVATION_QUEUE: usize = 64;
const RASTER_DAMAGE_FRAME_EQUIVALENTS: u64 = 8;
const RASTER_DAMAGE_INTERVAL: Duration = Duration::from_millis(100);

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
    pub eos_epoch: Option<u32>,
    #[allow(dead_code)] // Kept distinct from the outer sequence for the Stage 4 EOS barrier.
    pub last_inner_record_sequence: u64,
    pub causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    pub capture_policy: u64,
    pub semantic_descriptor: Option<messages::SourceDescriptor>,
    /// Operation limit of a delta-capable inner raster source, if it negotiated one.
    pub raster_delta_operation_limit: Option<u32>,
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
    credit_bytes: u64,
    queued_bytes: usize,
}

enum IngestOutcome {
    Accepted,
    RasterDeltaRejected {
        reason: u64,
        notify: bool,
        credit: DeliveryCredit,
    },
}

struct PreparedRaster {
    body: Arc<[u8]>,
    sequence: MediaSequence,
    frame_id: u64,
    epoch: u32,
    pts_us: i64,
    damage_window_started: Instant,
    damage_pixels: u64,
}

struct Producer {
    pane: PaneId,
    tag: [u8; 16],
    anchor_key: AnchorKey,
    writer: Weak<Writer>,
    observation_sender: mpsc::SyncSender<ObservationWrite>,
    features: HashSet<u64>,
    anchors: HashMap<u64, (i32, usize)>,
    seen_anchors: HashSet<u64>,
    scene_revision: SceneRevision,
    observation_mask: u64,
    observation_sequence: ObservationSequence,
    first_lost_source_sequence: Option<ObservationSequence>,
    first_lost_scene_sequence: Option<ObservationSequence>,
    waits: HashMap<u64, PendingSourceWait>,
}

struct ObservationWrite {
    record_type: u16,
    object_id: u64,
    body: Vec<u8>,
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
    /// Epoch carried by the inner `EOS`, once ingress has closed for that epoch.
    ///
    /// The bridge needs it to close the matching outer epoch: an outer presenter only reaches
    /// `MILESTONE_PLAYBACK_ENDED` after it has seen `EOS`, so without this the inner producer
    /// waits on `WAIT_PLAYBACK_ENDED` forever.
    eos_epoch: Option<u32>,
    bridge_desynchronized: bool,
    minimum_epoch: u32,
    /// Recovery reason already sent to the producer, or `None` when desynchronization has not yet
    /// produced a request. Kept separate from `bridge_desynchronized` because hidden sources can
    /// become desynchronized before they are projected again.
    pending_keyframe_reason: Option<u64>,
    last_pts_us: Option<i64>,
    clock_started: Option<Instant>,
    clock_origin_pts_us: Option<i64>,
    last_inner_record_sequence: u64,
    revision: SourceRevision,
    attachment_state: u64,
    attachment_generation: u64,
    credit_window_bytes: u64,
    credit_window_packets: u64,
    outstanding_byte_credit: u64,
    outstanding_packet_credit: u64,
    charged_bytes: u64,
    charged_packets: u64,
    last_media_id: u64,
    milestones: u64,
    causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    capture_policy: u64,
    semantic_descriptor: Option<messages::SourceDescriptor>,
    raster_update: Option<RasterUpdateConfig>,
    raster_requires_full_reason: Option<u64>,
    raster_damage_window_started: Instant,
    raster_damage_pixels: u64,
}

struct Ticket {
    source: SourceKey,
    kind: ConnectionKind,
    maximum_body: u32,
}

struct SourceCreation {
    request_id: u64,
    object_id: u64,
    causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    capture_policy: u64,
    semantic_descriptor: Option<messages::SourceDescriptor>,
    raster_update: Option<RasterUpdateConfig>,
}

#[derive(Clone, Copy)]
struct PendingSourceWait {
    source_id: u64,
    condition: u64,
    value: Option<u64>,
}

#[derive(Clone)]
enum Mutation {
    Create(SceneNode),
    Update(SceneNode),
    Delete(ProducerId, u64),
}

struct State {
    config: MediaConfig,
    capability_generation: u64,
    trace: Option<vivid_protocol::trace::TraceEmitter>,
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
    /// Revision of the virtual-to-outer projection snapshot. This is deliberately not a
    /// producer scene revision and is never overwritten by an outer presenter revision.
    projection_revision: u64,
    projected_sources: HashSet<SourceKey>,
    active_panes: HashSet<PaneId>,
    deliveries: HashMap<u64, PendingDelivery>,
    pending_media_barriers: HashSet<(ProducerId, u64)>,
    next_delivery_id: u64,
    queued_bridge_bytes: usize,
    events: Option<mpsc::SyncSender<MediaEvent>>,
    /// Wakes the consumer after a media event is queued.
    ///
    /// The events channel is drained by the session actor, which spends most of its time parked
    /// on a different receiver. Without this nudge a frame would wait for that receiver's timeout.
    media_wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
    next_connection: u64,
    connection_cancellers: HashMap<u64, (Option<PaneId>, ConnectionCancel)>,
    /// Diagnostic counters only. Nothing in ingest, projection, or credit accounting reads these.
    delivery_metrics: crate::metrics::DeliveryMetrics,
}

pub struct VirtualVivid {
    endpoint: String,
    state: Arc<Mutex<State>>,
    delivery_changed: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
    _trace_guard: Option<TraceGuard>,
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
        let trace_guard = diagnostic_trace_guard()?;
        let trace = trace_guard.as_ref().map(TraceGuard::emitter);
        let state = Arc::new(Mutex::new(State {
            config,
            capability_generation: 1,
            trace,
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
            projection_revision: 0,
            projected_sources: HashSet::new(),
            active_panes: HashSet::new(),
            deliveries: HashMap::new(),
            pending_media_barriers: HashSet::new(),
            next_delivery_id: 0,
            queued_bridge_bytes: 0,
            events,
            media_wakeup: None,
            next_connection: 0,
            connection_cancellers: HashMap::new(),
            delivery_metrics: crate::metrics::DeliveryMetrics::default(),
        }));
        let delivery_changed = Arc::new(Condvar::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let service = Self {
            endpoint: advertised_endpoint,
            state: state.clone(),
            delivery_changed: delivery_changed.clone(),
            shutdown: shutdown.clone(),
            _trace_guard: trace_guard,
        };
        thread::Builder::new()
            .name("vvmux-vivid-listener".into())
            .spawn(move || accept_loop(listener, state, delivery_changed, shutdown))?;
        Ok(service)
    }

    /// Install the callback that nudges the media-event consumer after a queue.
    pub fn set_media_wakeup(&self, wakeup: Arc<dyn Fn() + Send + Sync>) {
        self.lock().media_wakeup = Some(wakeup);
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn notify_capabilities_changed(&self, reason_mask: u64) -> io::Result<u64> {
        if reason_mask == 0 || reason_mask & !messages::CAPS_CHANGE_REASON_MASK != 0 {
            return Err(invalid("invalid capability change reason"));
        }
        let (generation, writers) = {
            let mut state = self.lock();
            state.capability_generation = state
                .capability_generation
                .checked_add(1)
                .ok_or_else(|| invalid("capability generation exhausted"))?;
            (
                state.capability_generation,
                state
                    .producers
                    .values()
                    .filter_map(|producer| producer.writer.upgrade())
                    .collect::<Vec<_>>(),
            )
        };
        let body = messages::caps_changed(generation, reason_mask)?;
        for writer in writers {
            let _ = writer.write_record(messages::CAPS_CHANGED, 0, &body);
        }
        Ok(generation)
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
        advance_projection(&mut state);
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
            settled: true,
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
            let _ = advance_scene(
                &mut state,
                producer_id,
                messages::SCENE_CHANGED_PRODUCER_COMMIT,
            );
            advance_projection(&mut state);
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
        let producers = state
            .producers
            .iter()
            .filter_map(|(&id, producer)| (producer.pane == pane).then_some(id))
            .collect::<Vec<_>>();
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
        for producer in producers {
            let _ = advance_scene(
                &mut state,
                producer,
                messages::SCENE_CHANGED_PRODUCER_COMMIT,
            );
        }
        advance_projection(&mut state);
    }

    pub fn clear_anchors(&self, pane: PaneId) {
        let mut state = self.lock();
        let producers = state
            .producers
            .iter()
            .filter_map(|(&id, producer)| (producer.pane == pane).then_some(id))
            .collect::<Vec<_>>();
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
        for producer in producers {
            let _ = advance_scene(&mut state, producer, messages::SCENE_CHANGED_ANCHOR_GONE);
        }
        advance_projection(&mut state);
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
                eos_epoch: source.eos_epoch,
                last_inner_record_sequence: source.last_inner_record_sequence,
                causation_id: source.causation_id,
                capture_policy: source.capture_policy,
                semantic_descriptor: source.semantic_descriptor.clone(),
                raster_delta_operation_limit: source
                    .raster_update
                    .filter(|update| update.mode == messages::RASTER_FULL_FRAME_AND_DELTA)
                    .map(|update| update.operation_limit),
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
        let visibility_changes = state
            .projected_sources
            .symmetric_difference(&projected_sources)
            .copied()
            .collect::<Vec<_>>();
        state.projected_sources = projected_sources;
        state.delivery_metrics.released_hidden = state
            .delivery_metrics
            .released_hidden
            .saturating_add(hidden_deliveries.len() as u64);
        let released = take_deliveries(&mut state, &hidden_deliveries);
        state.active_panes = panes.clone();
        for key in visibility_changes {
            let _ = advance_source(&mut state, key, messages::SOURCE_CHANGED_VISIBILITY);
        }
        let snapshot = ProjectionSnapshot {
            revision: state.projection_revision,
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
        self.lock().projection_revision
    }

    pub fn pane_status(
        &self,
        pane: PaneId,
        outer_projection_revision: u64,
        outer_attachment_generations: &HashMap<crate::ipc::BridgeSourceKey, u64>,
        relay: crate::metrics::RelayMetrics,
    ) -> PaneMediaStatus {
        let state = self.lock();
        let mut owners = state
            .producers
            .iter()
            .filter_map(|(&id, producer)| (producer.pane == pane).then_some(id))
            .collect::<HashSet<_>>();
        owners.extend(
            state
                .nodes
                .values()
                .filter_map(|node| (node.pane == pane).then_some(node.producer)),
        );
        let virtual_scene_revision = owners
            .iter()
            .filter_map(|owner| {
                state
                    .producers
                    .get(owner)
                    .map(|producer| producer.scene_revision)
            })
            .map(SceneRevision::get)
            .max()
            .unwrap_or(0);
        let mut sources = state
            .sources
            .iter()
            .filter(|(key, _)| owners.contains(&key.0))
            .map(|(&key, source)| {
                let queued = state
                    .deliveries
                    .values()
                    .filter(|delivery| delivery.source == key)
                    .collect::<Vec<_>>();
                let queued_bytes = queued
                    .iter()
                    .map(|delivery| delivery.credit_bytes)
                    .sum::<u64>();
                PaneMediaSourceStatus {
                    producer_id: key.0,
                    source_id: key.1,
                    kind: match &source.descriptor {
                        SourceDescriptor::Raster(_) => "raster",
                        SourceDescriptor::Image(_) => "image",
                        SourceDescriptor::Video(_) => "video",
                        SourceDescriptor::Audio(_) => "audio",
                    }
                    .into(),
                    lifecycle: if source.ended {
                        "ended"
                    } else if source.playing {
                        "playing"
                    } else if source.attachment_state == messages::ATTACHMENT_NEVER {
                        "created"
                    } else {
                        "paused"
                    }
                    .into(),
                    source_revision: source.revision.get(),
                    epoch: source.sequence.epoch(),
                    attachment_state: source.attachment_state,
                    attachment_generation: source.attachment_generation,
                    outer_attachment_generation: outer_attachment_generations
                        .get(&crate::ipc::BridgeSourceKey {
                            producer: key.0,
                            source: key.1,
                        })
                        .copied(),
                    visible: state.projected_sources.contains(&key),
                    capture_policy: source.capture_policy,
                    descriptor: source.semantic_descriptor.as_ref().map(|descriptor| {
                        let semantic_denied = source.capture_policy
                            & messages::CAPTURE_POLICY_DENY_SEMANTIC_EXPORT
                            != 0;
                        crate::ipc::PaneMediaSourceDescriptor {
                            role: descriptor.role,
                            title: (!semantic_denied).then(|| descriptor.title.clone()),
                            content_revision: (!semantic_denied)
                                .then_some(descriptor.content_revision),
                            semantic_availability: (!semantic_denied)
                                .then_some(descriptor.semantic_availability),
                            locator: (!semantic_denied).then(|| descriptor.locator.clone()),
                        }
                    }),
                    retained_static: source.descriptor.is_static() && source.retained.is_some(),
                    keyframe_needed: source.bridge_desynchronized,
                    milestones: source.milestones,
                    queued_packets: queued.len() as u64,
                    queued_bytes,
                    available_packet_credit: source.outstanding_packet_credit,
                    available_byte_credit: source.outstanding_byte_credit,
                }
            })
            .collect::<Vec<_>>();
        sources.sort_by_key(|source| (source.producer_id, source.source_id));
        let mut nodes = state
            .nodes
            .values()
            .filter(|node| node.pane == pane)
            .map(|node| PaneMediaNodeStatus {
                producer_id: node.producer,
                node_id: node.config.node.node_id,
                source_id: node.config.node.source_id,
                visible: state
                    .projected_sources
                    .contains(&(node.producer, node.config.node.source_id)),
                x: node.config.node.x,
                y: node.config.node.y,
                width: node.config.node.width,
                height: node.config.node.height,
            })
            .collect::<Vec<_>>();
        nodes.sort_by_key(|node| (node.producer_id, node.node_id));
        PaneMediaStatus {
            virtual_projection_revision: state.projection_revision,
            virtual_scene_revision,
            outer_projection_revision,
            sources,
            nodes,
            relay: crate::metrics::RelayMetrics {
                delivery: state.delivery_metrics,
                ..relay
            },
        }
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
        let (credit, request_keyframe) = {
            let mut state = self.lock();
            let Some(pending) = state.deliveries.remove(&delivery_id) else {
                return false;
            };
            if delivered {
                state.delivery_metrics.delivered =
                    state.delivery_metrics.delivered.saturating_add(1);
            } else {
                state.delivery_metrics.failed = state.delivery_metrics.failed.saturating_add(1);
            }
            state.queued_bridge_bytes = state
                .queued_bridge_bytes
                .saturating_sub(pending.queued_bytes);
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
            if delivered
                && state.projected_sources.contains(&pending.source)
                && let Some(source) = state.sources.get_mut(&pending.source)
            {
                source.milestones |= messages::MILESTONE_FIRST_VISIBLE_PRESENTATION;
            }
            let changed_fields = if delivered {
                messages::SOURCE_CHANGED_MILESTONES | messages::SOURCE_CHANGED_CREDIT_ACCOUNTING
            } else {
                messages::SOURCE_CHANGED_CREDIT_ACCOUNTING
            };
            let _ = advance_source(&mut state, pending.source, changed_fields);
            let credits =
                prepare_credit_return(&mut state, pending.source, pending.credit_bytes, 1);
            ((writer, pending.source, credits), request_keyframe)
        };
        self.delivery_changed.notify_all();
        let source = credit.1;
        return_delivery_credits(vec![credit]);
        if request_keyframe {
            self.request_keyframe(source, None, messages::KEYFRAME_REASON_TRANSPORT_LOSS);
        }
        request_keyframe
    }

    pub fn request_keyframe(&self, source: SourceKey, minimum_epoch: Option<u32>, reason: u64) {
        request_keyframe_recoveries(&self.state, &[(source, minimum_epoch, reason)]);
    }

    /// Ask inner raster producers for a full frame.
    ///
    /// Used when the bridge's own outgoing delta chain cannot continue. Setting
    /// `raster_requires_full_reason` makes `prepare_raster` reject further inner deltas until a
    /// full frame arrives, which is the presenter obligation in specification 11.4 applied to this
    /// hop's boundary.
    pub fn request_full_frames(&self, sources: &[SourceKey], reason: u64) {
        let mut state = self.lock();
        for key in sources {
            {
                let Some(source) = state.sources.get_mut(key) else {
                    continue;
                };
                if !matches!(source.descriptor, SourceDescriptor::Raster(_)) || source.ended {
                    continue;
                }
                if source.raster_requires_full_reason == Some(reason) {
                    // Recovery is already outstanding; a second request would only add traffic.
                    continue;
                }
                source.raster_requires_full_reason = Some(reason);
            }
            if let Some(writer) = state
                .producers
                .get(&key.0)
                .and_then(|producer| producer.writer.upgrade())
                && let Ok(body) = messages::need_full_frame(key.1, reason)
            {
                let _ = writer.write_record(messages::NEED_FULL_FRAME, key.1, &body);
            }
        }
    }

    pub fn apply_outer_playback(&self, key: SourceKey, playback_state: u64, eos_state: u64) {
        let mut state = self.lock();
        let Some(source) = state.sources.get_mut(&key) else {
            return;
        };
        let mut changed = messages::SOURCE_CHANGED_PLAYBACK;
        if playback_state == messages::PLAYBACK_PLAYING {
            source.milestones |= messages::MILESTONE_PLAYBACK_STARTED;
            changed |= messages::SOURCE_CHANGED_MILESTONES;
        } else if playback_state == messages::PLAYBACK_ENDED && eos_state >= messages::EOS_ACCEPTED
        {
            source.playing = false;
            source.milestones |= messages::MILESTONE_PLAYBACK_ENDED;
            changed |= messages::SOURCE_CHANGED_LIFECYCLE | messages::SOURCE_CHANGED_MILESTONES;
            advance_projection(&mut state);
        }
        let _ = advance_source(&mut state, key, changed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn normalize_keyframe_reason(reason: u64) -> u64 {
    match reason {
        messages::KEYFRAME_REASON_INITIAL
        | messages::KEYFRAME_REASON_DECODER_ERROR
        | messages::KEYFRAME_REASON_EPOCH_DISCONTINUITY
        | messages::KEYFRAME_REASON_DEVICE_RESET
        | messages::KEYFRAME_REASON_TRANSPORT_LOSS => reason,
        _ => messages::KEYFRAME_REASON_DECODER_ERROR,
    }
}

fn request_keyframe_recoveries(
    shared: &Arc<Mutex<State>>,
    requests: &[(SourceKey, Option<u32>, u64)],
) {
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for &(key, requested_minimum, requested_reason) in requests {
        let reason = normalize_keyframe_reason(requested_reason);
        let Some(source) = state.sources.get(&key) else {
            continue;
        };
        if !matches!(source.descriptor, SourceDescriptor::Video(_)) || source.ended {
            continue;
        }

        let current_epoch = source.sequence.epoch();
        let minimum_epoch = if reason == messages::KEYFRAME_REASON_TRANSPORT_LOSS {
            // Reason 5 promises that no epoch advance is required, even if a malformed or stale
            // outer request supplied a larger value.
            current_epoch
        } else if let Some(minimum) = requested_minimum {
            minimum.max(current_epoch)
        } else if reason == messages::KEYFRAME_REASON_INITIAL {
            current_epoch
        } else {
            current_epoch.saturating_add(1)
        };

        let pending_reason = source.pending_keyframe_reason;
        if let Some(pending_reason) = pending_reason {
            let stronger_reason = pending_reason == messages::KEYFRAME_REASON_TRANSPORT_LOSS
                && reason != messages::KEYFRAME_REASON_TRANSPORT_LOSS;
            if minimum_epoch <= source.minimum_epoch && !stronger_reason {
                state.delivery_metrics.keyframe_requests_damped = state
                    .delivery_metrics
                    .keyframe_requests_damped
                    .saturating_add(1);
                continue;
            }
        }
        let emitted_minimum = source.minimum_epoch.max(minimum_epoch);
        let emitted_reason = if let Some(pending_reason) = pending_reason {
            if pending_reason == reason
                || pending_reason == messages::KEYFRAME_REASON_TRANSPORT_LOSS
            {
                reason
            } else if reason == messages::KEYFRAME_REASON_TRANSPORT_LOSS {
                pending_reason
            } else {
                messages::KEYFRAME_REASON_DECODER_ERROR
            }
        } else {
            reason
        };

        let Some(writer) = state
            .producers
            .get(&key.0)
            .and_then(|producer| producer.writer.upgrade())
        else {
            continue;
        };
        let source = state
            .sources
            .get_mut(&key)
            .expect("source was validated above");
        source.minimum_epoch = emitted_minimum;
        source.pending_keyframe_reason = Some(emitted_reason);
        source.bridge_desynchronized = true;
        state.delivery_metrics.keyframe_requests =
            state.delivery_metrics.keyframe_requests.saturating_add(1);
        let _ = writer.write_record(
            messages::NEED_KEYFRAME,
            key.1,
            &messages::need_keyframe(key.1, emitted_minimum, emitted_reason, None),
        );
    }
}

fn advance_projection(state: &mut State) {
    state.projection_revision = state.projection_revision.saturating_add(1);
}

fn advance_scene(state: &mut State, producer_id: ProducerId, reason_mask: u64) -> io::Result<()> {
    let (sender, body, sequence) = {
        let producer = state
            .producers
            .get_mut(&producer_id)
            .ok_or_else(|| invalid("producer missing"))?;
        producer.scene_revision = producer
            .scene_revision
            .advance()
            .map_err(|_| invalid("scene revision exhausted"))?;
        if producer.observation_mask & messages::OBSERVE_SCENE_CHANGES == 0 {
            return Ok(());
        }
        producer.observation_sequence = producer
            .observation_sequence
            .advance()
            .map_err(|_| invalid("observation sequence exhausted"))?;
        let event = messages::SceneChanged {
            scene_revision: producer.scene_revision,
            reason_mask,
            observation_sequence: producer.observation_sequence,
            first_lost_sequence: producer.first_lost_scene_sequence.take(),
        };
        (
            producer.observation_sender.clone(),
            messages::scene_changed(event)?,
            producer.observation_sequence,
        )
    };
    if sender
        .try_send(ObservationWrite {
            record_type: messages::SCENE_CHANGED,
            object_id: 0,
            body,
        })
        .is_err()
        && let Some(producer) = state.producers.get_mut(&producer_id)
    {
        producer.first_lost_scene_sequence.get_or_insert(sequence);
    }
    Ok(())
}

fn advance_source(
    state: &mut State,
    key: SourceKey,
    changed_fields: u64,
) -> io::Result<SourceRevision> {
    let revision = {
        let source = state
            .sources
            .get_mut(&key)
            .ok_or_else(|| invalid("source missing"))?;
        source.revision = source
            .revision
            .advance()
            .map_err(|_| invalid("source revision exhausted"))?;
        source.revision
    };
    let (
        writer,
        observation_sender,
        event_body,
        event_sequence,
        playback_body,
        playback_sequence,
        wait_replies,
    ) = {
        let visible = state.projected_sources.contains(&key);
        let source = state.sources.get(&key).unwrap();
        let producer = state
            .producers
            .get_mut(&key.0)
            .ok_or_else(|| invalid("producer missing"))?;
        let writer = producer.writer.upgrade();
        let event_body = if producer.observation_mask & messages::OBSERVE_SOURCE_TRANSITIONS != 0 {
            producer.observation_sequence = producer
                .observation_sequence
                .advance()
                .map_err(|_| invalid("observation sequence exhausted"))?;
            Some(messages::source_changed(messages::SourceChanged {
                source_id: key.1,
                source_revision: revision,
                changed_fields,
                observation_sequence: producer.observation_sequence,
                first_lost_sequence: producer.first_lost_source_sequence.take(),
            })?)
        } else {
            None
        };
        let event_sequence = producer.observation_sequence;
        let playback_body = if changed_fields & messages::SOURCE_CHANGED_PLAYBACK != 0
            && producer.observation_mask & messages::OBSERVE_PLAYBACK_TRANSITIONS != 0
            && let Some(snapshot) = playback_snapshot(source)
        {
            producer.observation_sequence = producer
                .observation_sequence
                .advance()
                .map_err(|_| invalid("observation sequence exhausted"))?;
            Some(messages::playback_state(messages::PlaybackState {
                source_id: key.1,
                snapshot,
                source_revision: revision,
                observation_sequence: producer.observation_sequence,
            })?)
        } else {
            None
        };
        let playback_sequence = producer.observation_sequence;
        let satisfied = producer
            .waits
            .iter()
            .filter_map(|(&request_id, wait)| {
                evaluate_wait(source, visible, *wait).map(|observed_value| {
                    (
                        request_id,
                        messages::wait_satisfied(
                            request_id,
                            messages::WaitSatisfied {
                                source_id: key.1,
                                source_revision: revision,
                                condition: wait.condition,
                                observed_value,
                            },
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        for (request_id, _) in &satisfied {
            producer.waits.remove(request_id);
        }
        (
            writer,
            producer.observation_sender.clone(),
            event_body,
            event_sequence,
            playback_body,
            playback_sequence,
            satisfied,
        )
    };
    if let Some(body) = event_body
        && observation_sender
            .try_send(ObservationWrite {
                record_type: messages::SOURCE_CHANGED,
                object_id: key.1,
                body,
            })
            .is_err()
        && let Some(producer) = state.producers.get_mut(&key.0)
    {
        producer
            .first_lost_source_sequence
            .get_or_insert(event_sequence);
    }
    if let Some(body) = playback_body
        && observation_sender
            .try_send(ObservationWrite {
                record_type: messages::PLAYBACK_STATE,
                object_id: key.1,
                body,
            })
            .is_err()
        && let Some(producer) = state.producers.get_mut(&key.0)
    {
        producer
            .first_lost_source_sequence
            .get_or_insert(playback_sequence);
    }
    if let Some(writer) = writer {
        for (_, body) in wait_replies {
            if let Ok(body) = body {
                let _ = writer.write_record(messages::WAIT_SATISFIED, key.1, &body);
            }
        }
    }
    Ok(revision)
}

fn emit_source_event(state: &mut State, key: SourceKey, changed_fields: u64) -> io::Result<()> {
    let revision = state
        .sources
        .get(&key)
        .ok_or_else(|| invalid("source missing"))?
        .revision;
    let producer = state
        .producers
        .get_mut(&key.0)
        .ok_or_else(|| invalid("producer missing"))?;
    if producer.observation_mask & messages::OBSERVE_SOURCE_TRANSITIONS == 0 {
        return Ok(());
    }
    producer.observation_sequence = producer
        .observation_sequence
        .advance()
        .map_err(|_| invalid("observation sequence exhausted"))?;
    let body = messages::source_changed(messages::SourceChanged {
        source_id: key.1,
        source_revision: revision,
        changed_fields,
        observation_sequence: producer.observation_sequence,
        first_lost_sequence: producer.first_lost_source_sequence.take(),
    })?;
    if producer
        .observation_sender
        .try_send(ObservationWrite {
            record_type: messages::SOURCE_CHANGED,
            object_id: key.1,
            body,
        })
        .is_err()
    {
        producer
            .first_lost_source_sequence
            .get_or_insert(producer.observation_sequence);
    }
    Ok(())
}

/// Whether a pane's display can back a spec-valid `WELCOME`.
///
/// Vivid requires a nonzero viewport, grid, and cell size, and `update_metrics` only runs from the
/// projection pass, so a pane has no metrics until a client attaches. Producers admitted before
/// then would receive a `WELCOME` they must reject as malformed.
fn usable_display(display: &DisplayChanged) -> bool {
    display.viewport_width > 0
        && display.viewport_height > 0
        && display.grid_columns > 0
        && display.grid_rows > 0
        && display.cell_width > 0
        && display.cell_height > 0
}

fn evaluate_wait(source: &Source, visible: bool, wait: PendingSourceWait) -> Option<Option<u64>> {
    let reached = match wait.condition {
        messages::WAIT_SOURCE_REVISION => source.revision.get() >= wait.value.unwrap_or(u64::MAX),
        messages::WAIT_FIRST_VISIBLE_PRESENTATION => {
            source.milestones & messages::MILESTONE_FIRST_VISIBLE_PRESENTATION != 0
        }
        messages::WAIT_RASTER_FRAME => {
            matches!(source.descriptor, SourceDescriptor::Raster(_))
                && source.last_media_id >= wait.value.unwrap_or(u64::MAX)
        }
        messages::WAIT_VIDEO_PTS => {
            matches!(source.descriptor, SourceDescriptor::Video(_))
                && source.last_pts_us.unwrap_or(i64::MIN) >= wait.value.unwrap_or(u64::MAX) as i64
        }
        messages::WAIT_PLAYBACK_STARTED => {
            source.milestones & messages::MILESTONE_PLAYBACK_STARTED != 0
        }
        messages::WAIT_PLAYBACK_ENDED => {
            source.milestones & messages::MILESTONE_PLAYBACK_ENDED != 0
        }
        messages::WAIT_MEDIA_ATTACHED => source.attachment_state >= messages::ATTACHMENT_ATTACHED,
        messages::WAIT_MEDIA_CLOSED => source.attachment_state == messages::ATTACHMENT_CLOSED,
        messages::WAIT_SOURCE_LOST => source.milestones & messages::MILESTONE_SOURCE_LOST != 0,
        _ => false,
    };
    reached.then(|| match wait.condition {
        messages::WAIT_SOURCE_REVISION => Some(source.revision.get()),
        messages::WAIT_RASTER_FRAME => Some(source.last_media_id),
        messages::WAIT_VIDEO_PTS => source.last_pts_us.map(|pts| pts.max(0) as u64),
        messages::WAIT_FIRST_VISIBLE_PRESENTATION if !visible => None,
        _ => None,
    })
}

fn source_status(state: &State, key: SourceKey) -> Option<messages::SourceStatus> {
    let source = state.sources.get(&key)?;
    let visible = state.projected_sources.contains(&key);
    let pending_packets = state
        .deliveries
        .values()
        .filter(|delivery| delivery.source == key)
        .count() as u64;
    let (kind, linked_source_id) = match &source.descriptor {
        SourceDescriptor::Video(_) => (messages::SOURCE_KIND_VIDEO, 0),
        SourceDescriptor::Raster(_) => (messages::SOURCE_KIND_RASTER, 0),
        SourceDescriptor::Image(_) => (messages::SOURCE_KIND_IMAGE, 0),
        SourceDescriptor::Audio(config) => (
            messages::SOURCE_KIND_AUDIO,
            config.linked_video_source_id.unwrap_or(0),
        ),
    };
    let lifecycle = if source.playing {
        messages::SOURCE_LIFECYCLE_ACTIVE
    } else if source.ended {
        messages::SOURCE_LIFECYCLE_ENDED
    } else if source.attachment_state == messages::ATTACHMENT_NEVER {
        messages::SOURCE_LIFECYCLE_CREATED
    } else {
        messages::SOURCE_LIFECYCLE_PAUSED
    };
    let playback = playback_snapshot(source);
    Some(messages::SourceStatus {
        source_id: key.1,
        source_revision: source.revision,
        kind,
        lifecycle,
        epoch: source.sequence.epoch(),
        attachment_state: source.attachment_state,
        attachment_generation: source.attachment_generation,
        last_media_id: source.last_media_id,
        last_media_sequence: source.last_inner_record_sequence,
        last_decoded_pts_us: source.last_pts_us.unwrap_or(i64::MIN),
        last_presented_pts_us: source.last_pts_us.unwrap_or(i64::MIN),
        last_presentation_id: source.last_media_id,
        visible,
        capture_policy: source.capture_policy,
        linked_source_id,
        milestones: source.milestones,
        outstanding_byte_credit: source.outstanding_byte_credit,
        outstanding_packet_credit: source.outstanding_packet_credit,
        ingress_queue_depth: pending_packets.min(messages::QUEUE_DEPTH_CAPACITY),
        descriptor: source.semantic_descriptor.clone().map(|descriptor| {
            if source.capture_policy & messages::CAPTURE_POLICY_DENY_SEMANTIC_EXPORT != 0 {
                messages::ReportedSourceDescriptor::RoleOnly {
                    role: descriptor.role,
                }
            } else {
                messages::ReportedSourceDescriptor::Full(descriptor)
            }
        }),
        playback,
        terminal_loss_code: None,
    })
}

fn playback_snapshot(source: &Source) -> Option<messages::PlaybackSnapshot> {
    matches!(
        source.descriptor,
        SourceDescriptor::Video(_) | SourceDescriptor::Audio(_)
    )
    .then_some(messages::PlaybackSnapshot {
        state: if source.playing {
            messages::PLAYBACK_PLAYING
        } else if source.ended {
            messages::PLAYBACK_ENDED
        } else {
            messages::PLAYBACK_PAUSED
        },
        clock_pts_us: source
            .last_pts_us
            .unwrap_or(source.play_request.start_pts_us),
        epoch: source.sequence.epoch(),
        buffered_ahead_us: 0,
        underrun_count: 0,
        late_drop_count: 0,
        eos_state: if source.ended {
            messages::EOS_ACCEPTED
        } else {
            messages::EOS_NOT_RECEIVED
        },
    })
}

type DeliveryCredit = (Option<Arc<Writer>>, SourceKey, messages::Credits);

fn consume_source_credit(state: &mut State, key: SourceKey, bytes: u64) -> io::Result<()> {
    let source = state
        .sources
        .get_mut(&key)
        .ok_or_else(|| invalid("source no longer exists"))?;
    if bytes == 0 || source.outstanding_byte_credit < bytes || source.outstanding_packet_credit == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "media record exceeds outstanding source credit",
        ));
    }
    source.outstanding_byte_credit -= bytes;
    source.outstanding_packet_credit -= 1;
    source.charged_bytes = source
        .charged_bytes
        .checked_add(bytes)
        .ok_or_else(|| invalid("charged byte credit overflow"))?;
    source.charged_packets = source
        .charged_packets
        .checked_add(1)
        .ok_or_else(|| invalid("charged packet credit overflow"))?;
    Ok(())
}

fn prepare_credit_return(
    state: &mut State,
    key: SourceKey,
    bytes: u64,
    packets: u64,
) -> messages::Credits {
    let projected = state.projected_sources.contains(&key);
    let Some(source) = state.sources.get_mut(&key) else {
        return messages::Credits {
            bytes,
            packets,
            fragments: 0,
        };
    };
    if source.charged_bytes < bytes || source.charged_packets < packets {
        return messages::Credits {
            bytes: 0,
            packets: 0,
            fragments: 0,
        };
    }
    source.charged_bytes -= bytes;
    source.charged_packets -= packets;
    source.outstanding_byte_credit = source.outstanding_byte_credit.saturating_add(bytes);
    source.outstanding_packet_credit = source.outstanding_packet_credit.saturating_add(packets);
    let mut grant = messages::Credits {
        bytes,
        packets,
        fragments: 0,
    };
    if projected {
        let top_up_bytes = source.credit_window_bytes.saturating_sub(
            source
                .outstanding_byte_credit
                .saturating_add(source.charged_bytes),
        );
        let top_up_packets = source.credit_window_packets.saturating_sub(
            source
                .outstanding_packet_credit
                .saturating_add(source.charged_packets),
        );
        source.outstanding_byte_credit =
            source.outstanding_byte_credit.saturating_add(top_up_bytes);
        source.outstanding_packet_credit = source
            .outstanding_packet_credit
            .saturating_add(top_up_packets);
        grant.bytes = grant.bytes.saturating_add(top_up_bytes);
        grant.packets = grant.packets.saturating_add(top_up_packets);
    }
    grant
}

fn prepare_credit_write(
    state: &mut State,
    key: SourceKey,
    bytes: u64,
    packets: u64,
) -> DeliveryCredit {
    let writer = state
        .producers
        .get(&key.0)
        .and_then(|producer| producer.writer.upgrade());
    let credits = prepare_credit_return(state, key, bytes, packets);
    (writer, key, credits)
}

fn take_deliveries(state: &mut State, delivery_ids: &[u64]) -> Vec<DeliveryCredit> {
    let mut released = Vec::with_capacity(delivery_ids.len());
    for delivery_id in delivery_ids {
        let Some(pending) = state.deliveries.remove(delivery_id) else {
            continue;
        };
        state.queued_bridge_bytes = state
            .queued_bridge_bytes
            .saturating_sub(pending.queued_bytes);
        released.push(prepare_credit_write(
            state,
            pending.source,
            pending.credit_bytes,
            1,
        ));
    }
    released
}

fn return_delivery_credits(released: Vec<DeliveryCredit>) {
    for credit in released {
        let _ = write_delivery_credit(credit);
    }
}

fn write_delivery_credit((writer, source, credits): DeliveryCredit) -> io::Result<()> {
    if let Some(writer) = writer
        && (credits.bytes != 0 || credits.packets != 0)
    {
        writer.write_credit(source.1, credits.bytes, credits.packets, credits.fragments)?;
    }
    Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaBarrierWait {
    Accepted,
    AttachmentChanged,
    AttachmentClosed,
    SourceLost,
    TimedOut,
}

fn wait_for_media_barrier(
    shared: &Arc<Mutex<State>>,
    changed: &Condvar,
    source: SourceKey,
    attachment_generation: u64,
    final_record_sequence: u64,
    timeout: Duration,
) -> MediaBarrierWait {
    let deadline = Instant::now() + timeout;
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        let Some(current) = state.sources.get(&source) else {
            return MediaBarrierWait::SourceLost;
        };
        if current.attachment_generation != attachment_generation {
            return MediaBarrierWait::AttachmentChanged;
        }
        if current.last_inner_record_sequence >= final_record_sequence {
            return MediaBarrierWait::Accepted;
        }
        if current.attachment_state == messages::ATTACHMENT_CLOSED {
            return MediaBarrierWait::AttachmentClosed;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return MediaBarrierWait::TimedOut;
        };
        let (next, result) = changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
        if result.timed_out() {
            return MediaBarrierWait::TimedOut;
        }
    }
}

fn apply_inner_eos(
    state: &mut State,
    source: SourceKey,
    eos_epoch: u32,
    causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
) -> io::Result<()> {
    let current = state
        .sources
        .get_mut(&source)
        .ok_or_else(|| invalid("source missing"))?;
    current.ended = true;
    current.eos_epoch = Some(eos_epoch);
    current.causation_id = causation_id;
    current.milestones |= messages::MILESTONE_EOS_ACCEPTED;
    if !current.descriptor.is_static() {
        current.retained = None;
        current.retained_bytes = 0;
    }
    advance_source(
        state,
        source,
        messages::SOURCE_CHANGED_LIFECYCLE
            | messages::SOURCE_CHANGED_PLAYBACK
            | messages::SOURCE_CHANGED_MILESTONES,
    )?;
    advance_projection(state);
    Ok(())
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
    delivery_changed: &Arc<Condvar>,
) -> io::Result<()> {
    stream.set_read_deadline(Duration::from_secs(3))?;
    let (mut reader, preface) = Reader::new(stream)?;
    if let Some(trace) = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .trace
        .clone()
    {
        reader.set_trace(TraceChannel::new(trace));
    }
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
    delivery_changed: &Arc<Condvar>,
) -> io::Result<()> {
    let writer = Arc::new(reader.writer()?);
    let hello_record = reader.read_record()?;
    if hello_record.record_type != messages::HELLO || hello_record.object_id != 0 {
        return Err(invalid("first Vivid control record is not HELLO"));
    }
    let (request_id, hello) = messages::parse_hello(&hello_record.body)?;
    if hello.validate_authentication_kind(false).is_err() {
        writer.write_record(
            messages::ERROR,
            0,
            &messages::error(
                request_id,
                messages::ERROR_UNSUPPORTED_FEATURE,
                "HELLO authentication kind is unsupported",
            ),
        )?;
        return Ok(());
    }
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
    if !offers_vivid_version(
        hello.minimum_major,
        hello.minimum_minor,
        hello.maximum_major,
        hello.maximum_minor,
    ) {
        let detail = messages::ErrorDetail::supported_version(
            u64::from(VIVID_MAJOR),
            u64::from(VIVID_MINOR),
        );
        writer.write_record(
            messages::ERROR,
            0,
            &messages::error_with_detail(
                request_id,
                messages::ERROR_UNSUPPORTED_VERSION,
                false,
                &detail,
                "Vivid 1.1 is required",
            )?,
        )?;
        return Ok(());
    }
    let features = match messages::negotiate_features(
        &hello.required_features,
        &hello.optional_features,
        supported_feature,
    ) {
        Ok(features) => features,
        Err(_) => {
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
    };
    let (producer_id, tag, root_context, display) = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Read the metrics under the same lock that admits the producer: WELCOME is built from
        // this snapshot, so checking it separately could still emit a malformed one if the pane
        // closed in between.
        let Some(display) = state.metrics.get(&pane).copied().filter(usable_display) else {
            let mut detail = messages::ErrorDetail::new();
            detail.insert_bool(messages::ERROR_DETAIL_RETRYABLE, true);
            writer.write_record(
                messages::ERROR,
                0,
                &messages::error_with_detail(
                    request_id,
                    messages::ERROR_PRECONDITION_FAILED,
                    false,
                    &detail,
                    "vvmux pane has no display metrics yet; attach a vvmux client",
                )?,
            )?;
            return Ok(());
        };
        if state.producers.len() >= MAX_PRODUCERS {
            let detail = messages::ErrorDetail::limit(
                messages::LIMIT_CONCURRENT_SESSIONS,
                state.producers.len() as u64,
                MAX_PRODUCERS as u64,
            );
            writer.write_record(
                messages::ERROR,
                0,
                &messages::error_with_detail(
                    request_id,
                    messages::ERROR_LIMIT_EXCEEDED,
                    false,
                    &detail,
                    "producer quota exceeded",
                )?,
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
        let (observation_sender, observation_receiver) =
            mpsc::sync_channel::<ObservationWrite>(OBSERVATION_QUEUE);
        let observation_writer = writer.clone();
        thread::Builder::new()
            .name(format!("vvmux-observation-{producer_id}"))
            .spawn(move || {
                while let Ok(event) = observation_receiver.recv() {
                    if observation_writer
                        .write_record(event.record_type, event.object_id, &event.body)
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        state.producers.insert(
            producer_id,
            Producer {
                pane,
                tag,
                anchor_key,
                writer: Arc::downgrade(&writer),
                observation_sender,
                features: features.iter().copied().collect(),
                anchors: HashMap::new(),
                seen_anchors: HashSet::new(),
                scene_revision: SceneRevision::ZERO,
                observation_mask: 0,
                observation_sequence: ObservationSequence::ZERO,
                first_lost_source_sequence: None,
                first_lost_scene_sequence: None,
                waits: HashMap::new(),
            },
        );
        (producer_id, tag, (producer_id << 32) | 1, display)
    };
    writer.write_record(
        messages::WELCOME,
        0,
        &messages::welcome_preserving_at_generations(
            request_id,
            producer_id,
            &tag,
            root_context,
            display,
            &features,
            shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .capability_generation,
            SceneRevision::ZERO,
            &hello.preserved_fields,
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
    delivery_changed: &Arc<Condvar>,
    writer: &Arc<Writer>,
) -> io::Result<bool> {
    let observability_record = matches!(
        record.record_type,
        messages::SET_OBSERVATION
            | messages::QUERY_SOURCE
            | messages::QUERY_SCENE
            | messages::QUERY_ANCHOR
            | messages::QUERY_LIMITS
            | messages::WAIT_SOURCE
            | messages::CANCEL_WAIT
    );
    if observability_record
        && !shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .producers
            .get(&producer)
            .is_some_and(|runtime| {
                runtime
                    .features
                    .contains(&messages::FEATURE_OBSERVABILITY_CORE_V1)
            })
    {
        let request_id = messages::decode_control(&record.body)
            .map(|envelope| envelope.request_id)
            .unwrap_or(0);
        writer.write_record(
            messages::ERROR,
            record.object_id,
            &messages::error(
                request_id,
                messages::ERROR_UNSUPPORTED_FEATURE,
                "observability was not negotiated",
            ),
        )?;
        return Ok(true);
    }
    match record.record_type {
        messages::SET_OBSERVATION => {
            let (envelope, mask) = messages::parse_set_observation(&record.body)?;
            if record.object_id != 0 {
                return Err(invalid("SET_OBSERVATION must be session-level"));
            }
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let runtime = state
                .producers
                .get_mut(&producer)
                .ok_or_else(|| invalid("producer missing"))?;
            if !runtime
                .features
                .contains(&messages::FEATURE_OBSERVABILITY_CORE_V1)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "observability was not negotiated",
                ));
            }
            runtime.observation_mask = mask;
            runtime.first_lost_source_sequence = None;
            runtime.first_lost_scene_sequence = None;
            drop(state);
            writer.write_ok(messages::OK, 0, envelope.request_id)?;
        }
        messages::QUERY_SOURCE => {
            let (envelope, source_id) = messages::parse_query_source(&record.body)?;
            if record.object_id != source_id {
                return Err(invalid("QUERY_SOURCE object ID mismatch"));
            }
            let state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(status) = source_status(&state, (producer, source_id)) else {
                let body = messages::error(
                    envelope.request_id,
                    messages::ERROR_NOT_FOUND,
                    "source does not exist in this pane context",
                );
                drop(state);
                writer.write_record(messages::ERROR, source_id, &body)?;
                return Ok(true);
            };
            let body = messages::source_status(envelope.request_id, &status)?;
            drop(state);
            writer.write_record(messages::SOURCE_STATUS, source_id, &body)?;
        }
        messages::QUERY_SCENE => {
            let (envelope, query) = messages::parse_query_scene(&record.body)?;
            if record.object_id != 0 {
                return Err(invalid("QUERY_SCENE must be session-level"));
            }
            let state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let revision = state
                .producers
                .get(&producer)
                .ok_or_else(|| invalid("producer missing"))?
                .scene_revision;
            if query
                .expected_revision
                .is_some_and(|expected| expected != revision)
                || query
                    .cursor
                    .is_some_and(|cursor| cursor.scene_revision != revision)
            {
                let mut detail = messages::ErrorDetail::new();
                detail.insert_u64(messages::ERROR_DETAIL_SCENE_REVISION, revision.get());
                let body = messages::error_with_detail(
                    envelope.request_id,
                    messages::ERROR_PRECONDITION_FAILED,
                    false,
                    &detail,
                    "virtual scene revision changed",
                )?;
                drop(state);
                writer.write_record(messages::ERROR, 0, &body)?;
                return Ok(true);
            }
            let mut nodes = state
                .nodes
                .values()
                .filter(|node| node.producer == producer)
                .map(|node| node.config.clone())
                .collect::<Vec<_>>();
            nodes.sort_by_key(|node| node.node.node_id);
            let total_nodes = nodes.len() as u64;
            let offset = query.cursor.map_or(0, |cursor| cursor.offset) as usize;
            if offset > nodes.len() {
                return Err(invalid("scene cursor offset exceeds node count"));
            }
            let maximum = query
                .maximum_nodes
                .unwrap_or(messages::MAX_SCENE_NODES as u64)
                .min(messages::MAX_SCENE_NODES as u64) as usize;
            let end = offset.saturating_add(maximum).min(nodes.len());
            let page = nodes[offset..end].to_vec();
            let cursor = (end < nodes.len()).then_some(messages::SceneCursor {
                scene_revision: revision,
                offset: end as u64,
            });
            let body = messages::scene_status(
                envelope.request_id,
                &messages::SceneStatus {
                    scene_revision: revision,
                    nodes: page,
                    cursor,
                    total_nodes,
                },
            )?;
            drop(state);
            writer.write_record(messages::SCENE_STATUS, 0, &body)?;
        }
        messages::QUERY_ANCHOR => {
            let (envelope, anchor_id) = messages::parse_query_anchor(&record.body)?;
            if record.object_id != anchor_id {
                return Err(invalid("QUERY_ANCHOR object ID mismatch"));
            }
            let state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let runtime = state
                .producers
                .get(&producer)
                .ok_or_else(|| invalid("producer missing"))?;
            let display = state
                .metrics
                .get(&runtime.pane)
                .copied()
                .unwrap_or(DisplayChanged {
                    display_generation: 0,
                    viewport_width: 0,
                    viewport_height: 0,
                    grid_columns: 0,
                    grid_rows: 0,
                    cell_width: 0,
                    cell_height: 0,
                    settled: true,
                });
            let (state_kind, row, column) = runtime.anchors.get(&anchor_id).map_or(
                (messages::ANCHOR_STATE_UNKNOWN, 0, 0),
                |(line, column)| {
                    (
                        messages::ANCHOR_STATE_READY,
                        (*line).max(0) as u64,
                        *column as u64,
                    )
                },
            );
            let body = messages::anchor_status(
                envelope.request_id,
                messages::AnchorStatus {
                    anchor_id,
                    state: state_kind,
                    column,
                    row,
                    visible: state_kind == messages::ANCHOR_STATE_READY
                        && state.active_panes.contains(&runtime.pane),
                    display_generation: display.display_generation,
                },
            )?;
            drop(state);
            writer.write_record(messages::ANCHOR_STATUS, anchor_id, &body)?;
        }
        messages::QUERY_LIMITS => {
            let envelope = messages::parse_query_limits(&record.body)?;
            if record.object_id != 0 {
                return Err(invalid("QUERY_LIMITS must be session-level"));
            }
            let state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let current_sources = state
                .sources
                .keys()
                .filter(|(owner, _)| *owner == producer)
                .count() as u64;
            let current_nodes = state
                .nodes
                .keys()
                .filter(|(owner, _)| *owner == producer)
                .count() as u64;
            let body = messages::limits_status(
                envelope.request_id,
                messages::LimitsStatus {
                    maximum_sources: state.config.max_sources as u64,
                    maximum_nodes: state.config.max_nodes as u64,
                    maximum_transactions: state.config.max_nodes as u64,
                    maximum_anchors: state.config.max_anchors as u64,
                    maximum_control_body: u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY),
                    maximum_media_body: u64::from(vivid_protocol::HARD_MAX_RECORD_BODY),
                    maximum_waits: MAX_REGISTERED_WAITS as u64,
                    maximum_pending_requests: MAX_REGISTERED_WAITS as u64,
                    rolling_byte_window: u64::from(vivid_protocol::HARD_MAX_RECORD_BODY),
                    rolling_packet_window: ROLLING_PACKET_CREDITS,
                    retained_pixel_budget: state.config.aggregate_retained_bytes / 4,
                    current_sources,
                    current_nodes,
                    current_retained_pixels: state.retained_bytes as u64 / 4,
                    image_cache_budget: None,
                },
            )?;
            drop(state);
            writer.write_record(messages::LIMITS_STATUS, 0, &body)?;
        }
        messages::WAIT_SOURCE => {
            let (envelope, wait) = messages::parse_wait_source(&record.body)?;
            if record.object_id != wait.source_id {
                return Err(invalid("WAIT_SOURCE object ID mismatch"));
            }
            let duration = Duration::from_micros(wait.timeout_us);
            if duration > MAX_WAIT_TIMEOUT {
                return Err(invalid("WAIT_SOURCE timeout exceeds limit"));
            }
            let pending = PendingSourceWait {
                source_id: wait.source_id,
                condition: wait.condition,
                value: wait.value,
            };
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(source) = state.sources.get(&(producer, wait.source_id)) else {
                let body = messages::error(
                    envelope.request_id,
                    messages::ERROR_NOT_FOUND,
                    "source does not exist in this pane context",
                );
                drop(state);
                writer.write_record(messages::ERROR, wait.source_id, &body)?;
                return Ok(true);
            };
            let visible = state
                .projected_sources
                .contains(&(producer, wait.source_id));
            if wait.condition == messages::WAIT_FIRST_VISIBLE_PRESENTATION && !visible {
                let body = messages::error(
                    envelope.request_id,
                    messages::ERROR_NOT_VISIBLE,
                    "pane source is not projected to the outer presenter",
                );
                drop(state);
                writer.write_record(messages::ERROR, wait.source_id, &body)?;
                return Ok(true);
            }
            if let Some(observed_value) = evaluate_wait(source, visible, pending) {
                let body = messages::wait_satisfied(
                    envelope.request_id,
                    messages::WaitSatisfied {
                        source_id: wait.source_id,
                        source_revision: source.revision,
                        condition: wait.condition,
                        observed_value,
                    },
                )?;
                drop(state);
                writer.write_record(messages::WAIT_SATISFIED, wait.source_id, &body)?;
                return Ok(true);
            }
            let runtime = state
                .producers
                .get_mut(&producer)
                .ok_or_else(|| invalid("producer missing"))?;
            if runtime.waits.len() >= MAX_REGISTERED_WAITS {
                return Err(invalid("source wait quota exceeded"));
            }
            if runtime.waits.insert(envelope.request_id, pending).is_some() {
                return Err(invalid("wait request ID is already registered"));
            }
            drop(state);
            let shared = shared.clone();
            let timeout_writer = writer.clone();
            let request_id = envelope.request_id;
            thread::spawn(move || {
                thread::sleep(duration);
                let expired = {
                    let mut state = shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state
                        .producers
                        .get_mut(&producer)
                        .and_then(|runtime| runtime.waits.remove(&request_id))
                };
                if let Some(wait) = expired {
                    let _ = timeout_writer.write_record(
                        messages::ERROR,
                        wait.source_id,
                        &messages::error(
                            request_id,
                            messages::ERROR_TIMEOUT,
                            "source wait timed out",
                        ),
                    );
                }
            });
        }
        messages::CANCEL_WAIT => {
            let (envelope, wait_request_id) = messages::parse_cancel_wait(&record.body)?;
            if record.object_id != 0 {
                return Err(invalid("CANCEL_WAIT must be session-level"));
            }
            let wait = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .producers
                .get_mut(&producer)
                .and_then(|runtime| runtime.waits.remove(&wait_request_id));
            writer.write_ok(messages::OK, 0, envelope.request_id)?;
            if let Some(wait) = wait {
                writer.write_record(
                    messages::ERROR,
                    wait.source_id,
                    &messages::error(
                        wait_request_id,
                        messages::ERROR_CANCELLED,
                        "source wait was cancelled",
                    ),
                )?;
            }
        }
        messages::PROBE_VIDEO_CONFIG => {
            let (envelope, config) = messages::parse_probe_video_config(&record.body)?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(invalid("video probes must be session-level"));
            }
            let supported = media::is_portable_packetization(&config.codec, &config.packetization);
            let capability_generation = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .capability_generation;
            writer.write_record(
                messages::VIDEO_SUPPORT,
                0,
                &messages::capability_support(
                    envelope.request_id,
                    supported,
                    &config.codec,
                    capability_generation,
                ),
            )?;
        }
        messages::PROBE_AUDIO_CONFIG => {
            let (envelope, config) = messages::parse_probe_audio_config(&record.body)?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(invalid("audio probes must be session-level"));
            }
            let supported = messages::audio_config_supported(&config);
            let capability_generation = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .capability_generation;
            writer.write_record(
                messages::AUDIO_SUPPORT,
                0,
                &messages::capability_support(
                    envelope.request_id,
                    supported,
                    &config.codec,
                    capability_generation,
                ),
            )?;
        }
        messages::CREATE_RASTER => {
            let (envelope, config, update, capture_policy, semantic_descriptor) =
                messages::parse_create_raster_with_update_extensions(&record.body)?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if update.mode == messages::RASTER_FULL_FRAME_AND_DELTA
                && !producer_has_feature(shared, producer, messages::FEATURE_RASTER_DELTA_V1)
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "raster delta updates were not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if envelope.payload.map_value(9).is_some()
                && !producer_has_feature(
                    shared,
                    producer,
                    messages::FEATURE_SOURCE_CAPTURE_POLICY_V1,
                )
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source capture policy was not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if envelope.payload.map_value(10).is_some()
                && !producer_has_feature(shared, producer, messages::FEATURE_SOURCE_DESCRIPTOR_V1)
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source descriptors were not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            create_source(
                shared,
                producer,
                SourceDescriptor::Raster(config.clone()),
                writer,
                SourceCreation {
                    request_id: envelope.request_id,
                    object_id: record.object_id,
                    causation_id: envelope.causation_id,
                    capture_policy,
                    semantic_descriptor,
                    raster_update: Some(update),
                },
            )?;
        }
        messages::CREATE_IMAGE => {
            let (envelope, config, cache_lookup, capture_policy, semantic_descriptor) =
                messages::parse_create_image_with_extensions(&record.body)?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if cache_lookup
                && !producer_has_feature(shared, producer, messages::FEATURE_IMAGE_CACHE_V1)
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "image cache lookup was not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if envelope.payload.map_value(9).is_some()
                && !producer_has_feature(
                    shared,
                    producer,
                    messages::FEATURE_SOURCE_CAPTURE_POLICY_V1,
                )
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source capture policy was not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if envelope.payload.map_value(10).is_some()
                && !producer_has_feature(shared, producer, messages::FEATURE_SOURCE_DESCRIPTOR_V1)
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source descriptors were not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            create_source(
                shared,
                producer,
                SourceDescriptor::Image(config.clone()),
                writer,
                SourceCreation {
                    request_id: envelope.request_id,
                    object_id: record.object_id,
                    causation_id: envelope.causation_id,
                    capture_policy,
                    semantic_descriptor,
                    raster_update: None,
                },
            )?;
        }
        messages::CREATE_VIDEO => {
            let (envelope, config, capture_policy, semantic_descriptor) =
                messages::parse_create_video_with_extensions(&record.body)?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if envelope.payload.map_value(23).is_some()
                && !producer_has_feature(
                    shared,
                    producer,
                    messages::FEATURE_SOURCE_CAPTURE_POLICY_V1,
                )
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source capture policy was not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if envelope.payload.map_value(24).is_some()
                && !producer_has_feature(shared, producer, messages::FEATURE_SOURCE_DESCRIPTOR_V1)
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source descriptors were not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if !media::is_portable_packetization(&config.codec, &config.packetization) {
                return Err(invalid("unsupported video configuration"));
            }
            create_source(
                shared,
                producer,
                SourceDescriptor::Video(config.clone()),
                writer,
                SourceCreation {
                    request_id: envelope.request_id,
                    object_id: record.object_id,
                    causation_id: envelope.causation_id,
                    capture_policy,
                    semantic_descriptor,
                    raster_update: None,
                },
            )?;
        }
        messages::CREATE_AUDIO => {
            let (envelope, config, capture_policy, semantic_descriptor) =
                messages::parse_create_audio_with_extensions(&record.body)?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if envelope.payload.map_value(12).is_some()
                && !producer_has_feature(
                    shared,
                    producer,
                    messages::FEATURE_SOURCE_CAPTURE_POLICY_V1,
                )
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source capture policy was not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if envelope.payload.map_value(13).is_some()
                && !producer_has_feature(shared, producer, messages::FEATURE_SOURCE_DESCRIPTOR_V1)
            {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source descriptors were not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            if !messages::audio_config_supported(&config) {
                return Err(invalid("unsupported audio configuration"));
            }
            create_source(
                shared,
                producer,
                SourceDescriptor::Audio(config.clone()),
                writer,
                SourceCreation {
                    request_id: envelope.request_id,
                    object_id: record.object_id,
                    causation_id: envelope.causation_id,
                    capture_policy,
                    semantic_descriptor,
                    raster_update: None,
                },
            )?;
        }
        messages::SET_SOURCE_POLICY => {
            let (envelope, source_id, requested) = messages::parse_set_source_policy(&record.body)?;
            writer.mark_source_policy(source_id, requested);
            if record.object_id != source_id {
                return Err(invalid("source policy object ID mismatch"));
            }
            if !producer_has_feature(shared, producer, messages::FEATURE_SOURCE_CAPTURE_POLICY_V1) {
                writer.write_record(
                    messages::ERROR,
                    source_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source capture policy was not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(current) = state
                .sources
                .get(&(producer, source_id))
                .map(|source| source.capture_policy)
            else {
                drop(state);
                writer.write_record(
                    messages::ERROR,
                    source_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_NOT_FOUND,
                        "source does not exist in this pane context",
                    ),
                )?;
                return Ok(true);
            };
            let tightened = match tightened_capture_policy(current, requested) {
                Ok(tightened) => tightened,
                Err(_) => {
                    drop(state);
                    writer.write_record(
                        messages::ERROR,
                        source_id,
                        &messages::error(
                            envelope.request_id,
                            messages::ERROR_BAD_STATE,
                            "capture policy cannot be relaxed",
                        ),
                    )?;
                    return Ok(true);
                }
            };
            if let Some(tightened) = tightened {
                state
                    .sources
                    .get_mut(&(producer, source_id))
                    .expect("source was resolved under the same lock")
                    .capture_policy = tightened;
                advance_source(
                    &mut state,
                    (producer, source_id),
                    messages::SOURCE_CHANGED_CAPTURE_POLICY,
                )?;
                advance_projection(&mut state);
            }
            writer.write_ok(messages::OK, source_id, envelope.request_id)?;
        }
        messages::UPDATE_SOURCE_DESCRIPTOR => {
            let (envelope, source_id, descriptor) =
                messages::parse_update_source_descriptor(&record.body)?;
            if record.object_id != source_id {
                return Err(invalid("source descriptor object ID mismatch"));
            }
            if !producer_has_feature(shared, producer, messages::FEATURE_SOURCE_DESCRIPTOR_V1) {
                writer.write_record(
                    messages::ERROR,
                    source_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_UNSUPPORTED_FEATURE,
                        "source descriptors were not negotiated",
                    ),
                )?;
                return Ok(true);
            }
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(source) = state.sources.get_mut(&(producer, source_id)) else {
                drop(state);
                writer.write_record(
                    messages::ERROR,
                    source_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_NOT_FOUND,
                        "source does not exist in this pane context",
                    ),
                )?;
                return Ok(true);
            };
            let current = source
                .semantic_descriptor
                .as_ref()
                .map_or(0, |current| current.content_revision);
            if descriptor.content_revision <= current {
                drop(state);
                writer.write_record(
                    messages::ERROR,
                    source_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_BAD_STATE,
                        "descriptor content revision must advance",
                    ),
                )?;
                return Ok(true);
            }
            source.semantic_descriptor = Some(descriptor);
            advance_source(
                &mut state,
                (producer, source_id),
                messages::SOURCE_CHANGED_DESCRIPTOR,
            )?;
            advance_projection(&mut state);
            writer.write_ok(messages::OK, source_id, envelope.request_id)?;
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
            writer.write_ok(messages::OK, 0, envelope.request_id)?;
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
            writer.write_ok(messages::OK, record.object_id, envelope.request_id)?;
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
            writer.write_ok(messages::OK, record.object_id, envelope.request_id)?;
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
            writer.write_ok(messages::PRESENTED, 0, envelope.request_id)?;
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
            writer.write_ok(messages::OK, 0, envelope.request_id)?;
        }
        messages::DESTROY_SOURCE => {
            let (envelope, source_id) = messages::parse_object_id(&record.body, "source ID")?;
            remove_source(
                &mut shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                (producer, source_id),
            )?;
            writer.write_ok(messages::OK, source_id, envelope.request_id)?;
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
            source.causation_id = envelope.causation_id;
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
                let headless = state.events.is_none();
                let source = state.sources.get_mut(&(producer, source_id)).unwrap();
                if playing && headless {
                    source.milestones |= messages::MILESTONE_PLAYBACK_STARTED;
                }
                advance_source(
                    &mut state,
                    (producer, source_id),
                    messages::SOURCE_CHANGED_PLAYBACK | messages::SOURCE_CHANGED_MILESTONES,
                )?;
                advance_projection(&mut state);
            }
            writer.write_ok(messages::OK, source_id, envelope.request_id)?;
        }
        messages::FLUSH => {
            let (envelope, source_id, epoch) = messages::parse_flush(&record.body)?;
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
            source.eos_epoch = None;
            source.bridge_desynchronized = true;
            source.last_media_id = 0;
            source.last_pts_us = None;
            source.milestones &= messages::MILESTONE_MEDIA_ATTACHED;
            source.causation_id = envelope.causation_id;
            advance_source(
                &mut state,
                (producer, source_id),
                messages::SOURCE_CHANGED_EPOCH | messages::SOURCE_CHANGED_MILESTONES,
            )?;
            writer.write_ok(messages::OK, source_id, envelope.request_id)?;
        }
        messages::EOS => {
            let (envelope, request) = messages::parse_eos(&record.body)?;
            if record.object_id != request.source_id {
                return Err(invalid("EOS object ID mismatch"));
            }
            let source_key = (producer, request.source_id);
            if let Some(barrier) = request.barrier {
                {
                    let mut state = shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let negotiated = state.producers.get(&producer).is_some_and(|producer| {
                        producer
                            .features
                            .contains(&messages::FEATURE_MEDIA_ORDER_BARRIER_V1)
                    });
                    if !negotiated {
                        writer.write_record(
                            messages::ERROR,
                            request.source_id,
                            &messages::error(
                                envelope.request_id,
                                messages::ERROR_UNSUPPORTED_FEATURE,
                                "media-order barrier was not negotiated",
                            ),
                        )?;
                        return Ok(true);
                    }
                    let Some(source) = state.sources.get(&source_key) else {
                        writer.write_record(
                            messages::ERROR,
                            request.source_id,
                            &messages::error(
                                envelope.request_id,
                                messages::ERROR_NOT_FOUND,
                                "source missing",
                            ),
                        )?;
                        return Ok(true);
                    };
                    if source.attachment_generation != barrier.attachment_generation {
                        writer.write_record(
                            messages::ERROR,
                            request.source_id,
                            &messages::error(
                                envelope.request_id,
                                messages::ERROR_BAD_STATE,
                                "EOS attachment generation is not current",
                            ),
                        )?;
                        return Ok(true);
                    }
                    if state.pending_media_barriers.len() >= MAX_PENDING_MEDIA_BARRIERS
                        || !state
                            .pending_media_barriers
                            .insert((producer, envelope.request_id))
                    {
                        writer.write_record(
                            messages::ERROR,
                            request.source_id,
                            &messages::error(
                                envelope.request_id,
                                messages::ERROR_LIMIT_EXCEEDED,
                                "media-order barrier quota exceeded",
                            ),
                        )?;
                        return Ok(true);
                    }
                }
                let worker_state = shared.clone();
                let worker_changed = delivery_changed.clone();
                let worker_writer = writer.clone();
                let barrier_request_id = envelope.request_id;
                let spawn_result = thread::Builder::new()
                    .name("vvmux-media-order-barrier".into())
                    .spawn(move || {
                        let result = wait_for_media_barrier(
                            &worker_state,
                            &worker_changed,
                            source_key,
                            barrier.attachment_generation,
                            barrier.final_record_sequence,
                            MEDIA_ORDER_BARRIER_TIMEOUT,
                        );
                        let response = match result {
                            MediaBarrierWait::Accepted => {
                                let mut state = worker_state
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                apply_inner_eos(
                                    &mut state,
                                    source_key,
                                    request.epoch,
                                    envelope.causation_id,
                                )
                                .map(|_| (messages::OK, messages::ok(envelope.request_id)))
                                .unwrap_or_else(|_| {
                                    (
                                        messages::ERROR,
                                        messages::error(
                                            envelope.request_id,
                                            messages::ERROR_BAD_STATE,
                                            "source ended before EOS was applied",
                                        ),
                                    )
                                })
                            }
                            MediaBarrierWait::TimedOut => (
                                messages::ERROR,
                                messages::error(
                                    envelope.request_id,
                                    messages::ERROR_TIMEOUT,
                                    "EOS media-order barrier timed out",
                                ),
                            ),
                            MediaBarrierWait::AttachmentChanged
                            | MediaBarrierWait::AttachmentClosed
                            | MediaBarrierWait::SourceLost => (
                                messages::ERROR,
                                messages::error(
                                    envelope.request_id,
                                    messages::ERROR_BAD_STATE,
                                    "media attachment ended before EOS barrier",
                                ),
                            ),
                        };
                        worker_state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .pending_media_barriers
                            .remove(&(producer, envelope.request_id));
                        let _ =
                            worker_writer.write_record(response.0, request.source_id, &response.1);
                    });
                if let Err(error) = spawn_result {
                    shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .pending_media_barriers
                        .remove(&(producer, barrier_request_id));
                    return Err(error);
                }
            } else {
                wait_for_source_deliveries(shared, delivery_changed, source_key);
                let mut state = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                apply_inner_eos(&mut state, source_key, request.epoch, envelope.causation_id)?;
                writer.write_ok(messages::OK, request.source_id, envelope.request_id)?;
            }
        }
        messages::PING => {
            let envelope = messages::decode_control(&record.body)?;
            if record.object_id != 0 || envelope.request_id == 0 {
                return Err(invalid("PING is not a correlated session-level request"));
            }
            writer.write_pong(envelope.request_id)?;
        }
        messages::GOODBYE => {
            let envelope = messages::decode_control(&record.body)?;
            writer.write_ok(messages::OK, 0, envelope.request_id)?;
            return Ok(false);
        }
        _ if record.flags & vivid_protocol::wire::RECORD_OPTIONAL != 0 => {}
        _ => return Err(invalid("unsupported Vivid control record")),
    }
    Ok(true)
}

fn tightened_capture_policy(current: u64, requested: u64) -> io::Result<Option<u64>> {
    messages::validate_capture_policy(requested)?;
    if requested & current != current {
        return Err(invalid("capture policy cannot be relaxed"));
    }
    Ok((requested != current).then_some(requested))
}

fn producer_has_feature(shared: &Arc<Mutex<State>>, producer: ProducerId, feature: u64) -> bool {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .producers
        .get(&producer)
        .is_some_and(|runtime| runtime.features.contains(&feature))
}

fn create_source(
    shared: &Arc<Mutex<State>>,
    producer: ProducerId,
    descriptor: SourceDescriptor,
    writer: &Arc<Writer>,
    creation: SourceCreation,
) -> io::Result<()> {
    messages::validate_capture_policy(creation.capture_policy)?;
    if let Some(descriptor) = creation.semantic_descriptor.as_ref() {
        messages::validate_source_descriptor(descriptor)?;
    }
    let source_id = match &descriptor {
        SourceDescriptor::Raster(config) => config.source_id,
        SourceDescriptor::Image(config) => config.source_id,
        SourceDescriptor::Video(config) => config.source_id,
        SourceDescriptor::Audio(config) => config.source_id,
    };
    if source_id == 0 || creation.object_id != source_id {
        return Err(invalid("source object ID mismatch"));
    }
    let mut maximum = descriptor.maximum_body()?;
    if creation
        .raster_update
        .is_some_and(|update| update.mode == messages::RASTER_FULL_FRAME_AND_DELTA)
    {
        // Delta descriptors and independently compressed overwrite payloads can exceed the
        // equivalent full-frame body even though the retained result cannot. Admit the protocol
        // hard ceiling; the parser, per-frame operation limit, credit, and retained quota remain
        // independently bounded.
        maximum = vivid_protocol::HARD_MAX_RECORD_BODY;
    }
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
            eos_epoch: None,
            sequence: MediaSequence::default(),
            retained_bytes: 0,
            playing: false,
            play_request: messages::PlayRequest::baseline(source_id, 0),
            ended: false,
            bridge_desynchronized: false,
            minimum_epoch: 0,
            pending_keyframe_reason: None,
            last_pts_us: None,
            clock_started: None,
            clock_origin_pts_us: None,
            last_inner_record_sequence: 0,
            revision: SourceRevision::new(1),
            attachment_state: messages::ATTACHMENT_NEVER,
            attachment_generation: 0,
            credit_window_bytes: u64::from(maximum),
            credit_window_packets: ROLLING_PACKET_CREDITS,
            outstanding_byte_credit: u64::from(maximum),
            outstanding_packet_credit: INITIAL_PACKET_CREDITS,
            charged_bytes: 0,
            charged_packets: 0,
            last_media_id: 0,
            milestones: 0,
            causation_id: creation.causation_id,
            capture_policy: creation.capture_policy,
            semantic_descriptor: creation.semantic_descriptor,
            raster_update: creation.raster_update,
            raster_requires_full_reason: None,
            raster_damage_window_started: Instant::now(),
            raster_damage_pixels: 0,
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
    advance_projection(&mut state);
    drop(state);
    writer.write_record(
        messages::SOURCE_READY,
        source_id,
        &messages::source_ready_with_observability(
            creation.request_id,
            &SourceReady {
                source_id,
                media_ticket: ticket.to_vec(),
                byte_credits: u64::from(maximum),
                packet_credits: INITIAL_PACKET_CREDITS,
                fragment_credits: 0,
                max_media_body: maximum,
                rolling_byte_window: u64::from(maximum),
                rolling_packet_window: ROLLING_PACKET_CREDITS,
                initial_source_revision: SourceRevision::new(1),
                media_connection_required: true,
                delta_operation_limit: creation
                    .raster_update
                    .filter(|update| update.mode == messages::RASTER_FULL_FRAME_AND_DELTA)
                    .map(|update| u64::from(update.operation_limit)),
            },
        )?,
    )?;
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    emit_source_event(
        &mut state,
        (producer, source_id),
        messages::SOURCE_CHANGED_LIFECYCLE
            | messages::SOURCE_CHANGED_ATTACHMENT
            | messages::SOURCE_CHANGED_DESCRIPTOR,
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
    let capture_policy = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source = state
            .sources
            .get_mut(&ticket.source)
            .ok_or_else(|| invalid("source missing"))?;
        source.attachment_generation = source
            .attachment_generation
            .checked_add(1)
            .ok_or_else(|| invalid("attachment generation exhausted"))?;
        source.attachment_state = messages::ATTACHMENT_ATTACHED;
        source.milestones |= messages::MILESTONE_MEDIA_ATTACHED;
        let capture_policy = source.capture_policy;
        advance_source(
            &mut state,
            ticket.source,
            messages::SOURCE_CHANGED_ATTACHMENT | messages::SOURCE_CHANGED_MILESTONES,
        )?;
        capture_policy
    };
    reader.mark_source_policy(ticket.source.1, capture_policy);
    mark_connection_pane(shared, connection_id, pane)?;
    reader.clear_read_deadline()?;
    reader.set_maximum(ticket.maximum_body);
    let mut body = Vec::new();
    let result = loop {
        let record = match reader.read_record_into(&mut body) {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(error) => break Err(error),
        };
        if record.object_id != ticket.source.1 {
            break Err(invalid("media object ID mismatch"));
        }
        match ingest_record(shared, delivery_changed, ticket.source, &record) {
            Ok(IngestOutcome::Accepted) => {}
            Ok(IngestOutcome::RasterDeltaRejected {
                reason,
                notify,
                credit,
            }) => {
                if let Some(writer) = &credit.0 {
                    writer.write_record(
                        messages::ERROR,
                        ticket.source.1,
                        &messages::error(
                            0,
                            messages::ERROR_BAD_STATE,
                            "raster delta requires a new full frame",
                        ),
                    )?;
                    if notify {
                        writer.write_record(
                            messages::NEED_FULL_FRAME,
                            ticket.source.1,
                            &messages::need_full_frame(ticket.source.1, reason)?,
                        )?;
                    }
                }
                write_delivery_credit(credit)?;
            }
            Err(error) => break Err(error),
        }
    };
    {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(source) = state.sources.get_mut(&ticket.source) {
            source.attachment_state = messages::ATTACHMENT_CLOSED;
            let _ = advance_source(
                &mut state,
                ticket.source,
                messages::SOURCE_CHANGED_ATTACHMENT,
            );
        }
    }
    delivery_changed.notify_all();
    result
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

fn canonical_raster_full_body(
    epoch: u32,
    frame_id: u64,
    pts_us: i64,
    duration_us: u64,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> io::Result<Arc<[u8]>> {
    let mut body = media::raster_frame_body(epoch, frame_id, width, height, pixels)?;
    body[24..32].copy_from_slice(&pts_us.to_be_bytes());
    body[32..40].copy_from_slice(&duration_us.to_be_bytes());
    Ok(Arc::from(body))
}

fn raster_damage_pixels(operations: &[media::ParsedRasterDeltaOperation<'_>]) -> io::Result<u64> {
    let rectangles = operations
        .iter()
        .map(|operation| match operation {
            media::ParsedRasterDeltaOperation::Overwrite {
                x,
                y,
                width,
                height,
                ..
            } => (*x, *y, *width, *height),
            media::ParsedRasterDeltaOperation::Copy {
                destination_x,
                destination_y,
                width,
                height,
                ..
            } => (*destination_x, *destination_y, *width, *height),
        })
        .collect::<Vec<_>>();
    let mut y_edges = rectangles
        .iter()
        .flat_map(|(_, y, _, height)| [*y, *y + *height])
        .collect::<Vec<_>>();
    y_edges.sort_unstable();
    y_edges.dedup();
    let mut pixels = 0_u64;
    for band in y_edges.windows(2) {
        let (top, bottom) = (band[0], band[1]);
        let mut intervals = rectangles
            .iter()
            .filter(|(_, y, _, height)| *y <= top && *y + *height >= bottom)
            .map(|(x, _, width, _)| (*x, *x + *width))
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        let mut merged = Vec::<(u32, u32)>::new();
        for interval in intervals {
            if let Some(last) = merged.last_mut()
                && interval.0 <= last.1
            {
                last.1 = last.1.max(interval.1);
            } else {
                merged.push(interval);
            }
        }
        let width = merged.into_iter().try_fold(0_u64, |total, (left, right)| {
            total.checked_add(u64::from(right - left))
        });
        let Some(width) = width else {
            return Err(invalid("raster delta damage accounting overflowed"));
        };
        pixels = pixels
            .checked_add(
                width
                    .checked_mul(u64::from(bottom - top))
                    .ok_or_else(|| invalid("raster delta damage accounting overflowed"))?,
            )
            .ok_or_else(|| invalid("raster delta damage accounting overflowed"))?;
    }
    Ok(pixels)
}

fn apply_raster_delta_operation(
    pixels: &mut [u8],
    source_width: u32,
    operation: &media::ParsedRasterDeltaOperation<'_>,
) {
    let stride = source_width as usize * 4;
    match operation {
        media::ParsedRasterDeltaOperation::Overwrite {
            x,
            y,
            width,
            height,
            rgba,
        } => {
            let row_bytes = *width as usize * 4;
            for row in 0..*height as usize {
                let destination = (*y as usize + row) * stride + *x as usize * 4;
                let source = row * row_bytes;
                pixels[destination..destination + row_bytes]
                    .copy_from_slice(&rgba[source..source + row_bytes]);
            }
        }
        media::ParsedRasterDeltaOperation::Copy {
            destination_x,
            destination_y,
            width,
            height,
            source_x,
            source_y,
        } => {
            let row_bytes = *width as usize * 4;
            if destination_y > source_y {
                for row in (0..*height as usize).rev() {
                    let source = (*source_y as usize + row) * stride + *source_x as usize * 4;
                    let destination =
                        (*destination_y as usize + row) * stride + *destination_x as usize * 4;
                    pixels.copy_within(source..source + row_bytes, destination);
                }
            } else {
                for row in 0..*height as usize {
                    let source = (*source_y as usize + row) * stride + *source_x as usize * 4;
                    let destination =
                        (*destination_y as usize + row) * stride + *destination_x as usize * 4;
                    pixels.copy_within(source..source + row_bytes, destination);
                }
            }
        }
    }
}

enum RasterPreparation {
    Accepted(PreparedRaster),
    Rejected { reason: u64, notify: bool },
}

fn prepare_raster(
    source: &Source,
    config: &RasterSourceConfig,
    body: &[u8],
    now: Instant,
) -> io::Result<RasterPreparation> {
    let flags = body
        .get(4..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| invalid("raster frame header is truncated"))?;
    if flags & media::RASTER_FRAME_DELTA == 0 {
        let parsed = media::parse_full_raster_frame(body)?;
        if (parsed.width, parsed.height) != (config.width, config.height) {
            return Err(invalid("raster dimensions changed"));
        }
        let mut sequence = source.sequence;
        let frame_id = parsed.frame_id;
        let epoch = parsed.epoch;
        let pts_us = parsed.pts_us;
        let duration_us = parsed.duration_us;
        let pixels = media::decode_raster_pixels(parsed)?;
        let body = canonical_raster_full_body(
            epoch,
            frame_id,
            pts_us,
            duration_us,
            config.width,
            config.height,
            &pixels,
        )?;
        sequence.accept(frame_id, epoch)?;
        return Ok(RasterPreparation::Accepted(PreparedRaster {
            body,
            sequence,
            frame_id,
            epoch,
            pts_us,
            damage_window_started: now,
            damage_pixels: 0,
        }));
    }
    let Some(update) = source
        .raster_update
        .filter(|update| update.mode == messages::RASTER_FULL_FRAME_AND_DELTA)
    else {
        return Err(invalid("raster delta was not enabled by CREATE_RASTER"));
    };
    let frame =
        media::parse_delta_raster_frame(body, config.width, config.height, update.operation_limit)?;
    if let Some(reason) = source.raster_requires_full_reason {
        return Ok(RasterPreparation::Rejected {
            reason,
            notify: false,
        });
    }
    let Some(retained) = source.retained.as_deref() else {
        return Ok(RasterPreparation::Rejected {
            reason: messages::NEED_FULL_FRAME_BASE_UNAVAILABLE,
            notify: true,
        });
    };
    let current = media::parse_full_raster_frame(retained)?;
    if frame.epoch != current.epoch || frame.base_frame_id != current.frame_id {
        return Ok(RasterPreparation::Rejected {
            reason: messages::NEED_FULL_FRAME_BASE_UNAVAILABLE,
            notify: true,
        });
    }
    let damaged_pixels = raster_damage_pixels(&frame.operations)?;
    let mut damage_window_started = source.raster_damage_window_started;
    let mut accumulated_damage = source.raster_damage_pixels;
    if now.duration_since(damage_window_started) >= RASTER_DAMAGE_INTERVAL {
        damage_window_started = now;
        accumulated_damage = 0;
    }
    let budget = u64::from(config.width)
        .saturating_mul(u64::from(config.height))
        .saturating_mul(RASTER_DAMAGE_FRAME_EQUIVALENTS);
    if accumulated_damage.saturating_add(damaged_pixels) > budget {
        return Ok(RasterPreparation::Rejected {
            reason: messages::NEED_FULL_FRAME_DAMAGE_BUDGET,
            notify: true,
        });
    }
    let mut pixels = media::decode_raster_pixels(current)?;
    for operation in &frame.operations {
        apply_raster_delta_operation(&mut pixels, config.width, operation);
    }
    let mut sequence = source.sequence;
    sequence.accept(frame.frame_id, frame.epoch)?;
    let canonical = canonical_raster_full_body(
        frame.epoch,
        frame.frame_id,
        frame.pts_us,
        frame.duration_us,
        config.width,
        config.height,
        &pixels,
    )?;
    Ok(RasterPreparation::Accepted(PreparedRaster {
        body: canonical,
        sequence,
        frame_id: frame.frame_id,
        epoch: frame.epoch,
        pts_us: frame.pts_us,
        damage_window_started,
        damage_pixels: accumulated_damage.saturating_add(damaged_pixels),
    }))
}

/// Verify an `IMAGE_DATA` body against its source configuration without holding the global lock.
///
/// The lock is taken only to copy the immutable image configuration; hashing and decoding then run
/// unlocked. A source's descriptor is fixed for its lifetime, so the copy cannot go stale, and the
/// caller re-checks the mutable conditions — retained state and body length — under the lock.
/// Non-image records return immediately.
fn validate_encoded_image(
    shared: &Arc<Mutex<State>>,
    key: SourceKey,
    record: &BorrowedRecord<'_>,
) -> io::Result<()> {
    if record.record_type != messages::IMAGE_DATA {
        return Ok(());
    }
    let config = {
        let state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source = state
            .sources
            .get(&key)
            .ok_or_else(|| invalid("source no longer exists"))?;
        match &source.descriptor {
            SourceDescriptor::Image(config) => config.clone(),
            _ => return Ok(()),
        }
    };
    if record.body.len() != config.encoded_length as usize {
        return Err(invalid("image body count or length is invalid"));
    }
    if let Some(expected) = config.sha256
        && Sha256::digest(record.body).as_slice() != expected
    {
        return Err(invalid("image hash mismatch"));
    }
    let format = match config.encoding {
        messages::IMAGE_PNG => image::ImageFormat::Png,
        messages::IMAGE_JPEG => image::ImageFormat::Jpeg,
        _ => return Err(invalid("unsupported image encoding")),
    };
    let decoded = image::load_from_memory_with_format(record.body, format)
        .map_err(|_| invalid("image decoder rejected body"))?;
    if (decoded.width(), decoded.height()) != (config.width, config.height) {
        return Err(invalid("decoded image dimensions mismatch"));
    }
    Ok(())
}

fn ingest_record(
    shared: &Arc<Mutex<State>>,
    delivery_changed: &Condvar,
    key: SourceKey,
    record: &BorrowedRecord<'_>,
) -> io::Result<IngestOutcome> {
    // Hashing and decoding an encoded image are the most expensive work on any ingest path — a
    // large PNG is easily hundreds of milliseconds under load. Doing it before taking the global
    // media lock keeps it off every other source and off the session actor, which needs that lock
    // for delivery completion, projection snapshots, marker observation, and anchor scrolling. An
    // image arriving in one tab used to stall rendering and video for every other pane.
    validate_encoded_image(shared, key, record)?;
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    consume_source_credit(&mut state, key, record.body.len() as u64)?;
    let projected_source = state.projected_sources.contains(&key);
    let limit = state.config.aggregate_retained_bytes as usize;
    let bridge_queue_limit = state.config.ipc_queue_bytes;
    let raster_preparation = {
        let source = state
            .sources
            .get(&key)
            .ok_or_else(|| invalid("source no longer exists"))?;
        match (&source.descriptor, record.record_type) {
            (SourceDescriptor::Raster(config), messages::RASTER_FRAME) => {
                Some(prepare_raster(source, config, record.body, Instant::now())?)
            }
            _ => None,
        }
    };
    if let Some(RasterPreparation::Rejected { reason, notify }) = raster_preparation {
        state
            .sources
            .get_mut(&key)
            .expect("source exists")
            .raster_requires_full_reason = Some(reason);
        let credit = prepare_credit_write(&mut state, key, record.body.len() as u64, 1);
        drop(state);
        delivery_changed.notify_all();
        return Ok(IngestOutcome::RasterDeltaRejected {
            reason,
            notify,
            credit,
        });
    }
    let mut prepared_raster = raster_preparation.and_then(|preparation| match preparation {
        RasterPreparation::Accepted(prepared) => Some(prepared),
        RasterPreparation::Rejected { .. } => None,
    });
    let (old_retained, retained_body, new_retained, pts, candidate_forward, forward_body) = {
        let source = state
            .sources
            .get_mut(&key)
            .ok_or_else(|| invalid("source no longer exists"))?;
        let new_retained = match (&source.descriptor, record.record_type) {
            (SourceDescriptor::Raster(_), messages::RASTER_FRAME) => {
                let prepared = prepared_raster
                    .as_ref()
                    .ok_or_else(|| invalid("raster preparation is missing"))?;
                Some(prepared.body.clone())
            }
            (SourceDescriptor::Image(config), messages::IMAGE_DATA) => {
                // The hash and decode already ran without the lock. Re-check the two conditions
                // that depend on state the lock protects, because it was released in between.
                if source.retained.is_some() || record.body.len() != config.encoded_length as usize
                {
                    return Err(invalid("image body count or length is invalid"));
                }
                source.last_media_id = 1;
                Some(Arc::<[u8]>::from(record.body))
            }
            (SourceDescriptor::Video(config), messages::VIDEO_PACKET) => {
                let packet = media::parse_video_packet(record.body)?;
                if packet.data.len() > config.max_access_unit_bytes as usize {
                    return Err(invalid("video access unit exceeds source maximum"));
                }
                source.sequence.accept(packet.packet_id, packet.epoch)?;
                source.last_pts_us = Some(packet.pts_us);
                source.last_media_id = packet.packet_id;
                let recovered = source.bridge_desynchronized
                    && packet.epoch >= source.minimum_epoch
                    && packet.flags & media::VIDEO_PACKET_KEY != 0;
                if recovered {
                    source.bridge_desynchronized = false;
                    source.pending_keyframe_reason = None;
                } else if !projected_source {
                    source.bridge_desynchronized = true;
                }
                None
            }
            (SourceDescriptor::Audio(config), messages::AUDIO_PACKET) => {
                let packet = media::parse_audio_packet(record.body)?;
                if packet.data.len() > config.max_access_unit_bytes as usize {
                    return Err(invalid("audio access unit exceeds source maximum"));
                }
                source.sequence.accept(packet.packet_id, packet.epoch)?;
                source.last_pts_us = Some(packet.pts_us);
                source.last_media_id = packet.packet_id;
                None
            }
            _ => return Err(invalid("media record type does not match source")),
        };
        let old = source.retained_bytes;
        let new = new_retained.as_ref().map_or(0, |body| body.len());
        let forward = matches!(
            source.descriptor,
            SourceDescriptor::Raster(_) | SourceDescriptor::Video(_) | SourceDescriptor::Audio(_)
        ) && projected_source
            && !source.bridge_desynchronized;
        // Raster forwards the composed canonical full frame, except when the inner record was a
        // delta that the bridge can chain onto its own outgoing frame. Forwarding the delta keeps
        // an update proportional to its damage instead of expanding it to the whole framebuffer on
        // both remaining hops; the bridge restamps the identities so no inner base crosses the
        // boundary.
        let forward_body = matches!(source.descriptor, SourceDescriptor::Raster(_))
            .then(|| {
                let is_delta = record.record_type == messages::RASTER_FRAME
                    && record.body.len() >= 8
                    && u32::from_be_bytes(record.body[4..8].try_into().expect("checked length"))
                        & media::RASTER_FRAME_DELTA
                        != 0;
                // Prefer whichever form is smaller, the same rule the specification puts on a
                // producer. For a small surface the operation descriptors can outweigh the pixels
                // they describe, and then the full frame is both cheaper and simpler downstream.
                let delta_wins = is_delta
                    && new_retained
                        .as_ref()
                        .is_none_or(|full| record.body.len() < full.len());
                if delta_wins {
                    Some(Arc::<[u8]>::from(record.body))
                } else {
                    new_retained.clone()
                }
            })
            .flatten();
        let pts = prepared_raster
            .as_ref()
            .map_or(source.last_pts_us, |prepared| Some(prepared.pts_us));
        (old, new_retained, new, pts, forward, forward_body)
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
    let forwarded = forward_body.unwrap_or_else(|| Arc::from(record.body));
    let source = state.sources.get_mut(&key).unwrap();
    if let Some(prepared) = prepared_raster.take() {
        source.sequence = prepared.sequence;
        source.last_media_id = prepared.frame_id;
        source.raster_requires_full_reason = None;
        source.raster_damage_window_started = prepared.damage_window_started;
        source.raster_damage_pixels = prepared.damage_pixels;
        debug_assert_eq!(source.sequence.epoch(), prepared.epoch);
    }
    let retained_requires_revision = matches!(source.descriptor, SourceDescriptor::Image(_));
    if let Some(retained) = retained_body {
        source.retained = Some(retained);
        source.retained_bytes = new_retained;
    }
    source.last_pts_us = pts;
    source.last_inner_record_sequence = record.sequence;
    source.milestones |= messages::MILESTONE_FIRST_MEDIA_RECORD
        | messages::MILESTONE_DECODER_INITIALIZED
        | messages::MILESTONE_FIRST_DECODED_OUTPUT;
    if projected_source {
        source.milestones |= messages::MILESTONE_FIRST_VISIBLE_PRESENTATION;
    }
    if record.record_type == messages::VIDEO_PACKET
        && media::parse_video_packet(record.body)?.flags & media::VIDEO_PACKET_KEY != 0
    {
        source.milestones |= messages::MILESTONE_RANDOM_ACCESS_ACCEPTED;
    }
    state.retained_bytes = projected;
    if retained_requires_revision && new_retained > 0 && old_retained == 0 {
        // Immutable images have no live MediaEvent, so their first retained body must trigger
        // hydration. Raster bodies are exclusively live while projected and are picked up from
        // retained state only on an independently required source/layout rebuild.
        advance_projection(&mut state);
    }
    advance_source(
        &mut state,
        key,
        messages::SOURCE_CHANGED_EPOCH
            | messages::SOURCE_CHANGED_MILESTONES
            | messages::SOURCE_CHANGED_CREDIT_ACCOUNTING,
    )?;
    let headless_delay = (!projected_source)
        .then(|| headless_playback_delay(&mut state, key, pts))
        .flatten();
    let media_wakeup = state.media_wakeup.clone();
    let mut request_keyframe = false;
    let delivery = if forward_timed
        && let Some(events) = state.events.clone()
        && (state.queued_bridge_bytes == 0
            || state.queued_bridge_bytes.saturating_add(forwarded.len()) <= bridge_queue_limit)
    {
        state.next_delivery_id = state
            .next_delivery_id
            .checked_add(1)
            .ok_or_else(|| invalid("bridge delivery IDs exhausted"))?;
        let delivery_id = state.next_delivery_id;
        state.queued_bridge_bytes = state.queued_bridge_bytes.saturating_add(forwarded.len());
        state.deliveries.insert(
            delivery_id,
            PendingDelivery {
                source: key,
                credit_bytes: record.body.len() as u64,
                queued_bytes: forwarded.len(),
            },
        );
        state.delivery_metrics.created = state.delivery_metrics.created.saturating_add(1);
        Some((delivery_id, events))
    } else {
        if forward_timed {
            state.delivery_metrics.dropped_queue_budget = state
                .delivery_metrics
                .dropped_queue_budget
                .saturating_add(1);
        }
        if forward_timed
            && let Some(source) = state.sources.get_mut(&key)
            && matches!(source.descriptor, SourceDescriptor::Video(_))
        {
            source.bridge_desynchronized = true;
            request_keyframe = true;
        }
        None
    };
    let immediate_credit = delivery
        .is_none()
        .then(|| prepare_credit_write(&mut state, key, record.body.len() as u64, 1));
    drop(state);
    delivery_changed.notify_all();
    if let Some(delay) = headless_delay {
        thread::sleep(delay);
    }
    if let Some((delivery_id, events)) = delivery {
        match events.try_send(MediaEvent {
            delivery_id,
            source: key,
            record_type: record.record_type,
            body: forwarded.to_vec(),
        }) {
            Ok(()) => {
                if let Some(wakeup) = media_wakeup {
                    wakeup();
                }
                return Ok(IngestOutcome::Accepted);
            }
            Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => {
                let credit = {
                    let mut state = shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.delivery_metrics.dropped_actor_queue_full = state
                        .delivery_metrics
                        .dropped_actor_queue_full
                        .saturating_add(1);
                    let credit = if let Some(pending) = state.deliveries.remove(&delivery_id) {
                        state.queued_bridge_bytes = state
                            .queued_bridge_bytes
                            .saturating_sub(pending.queued_bytes);
                        delivery_changed.notify_all();
                        Some(prepare_credit_write(
                            &mut state,
                            pending.source,
                            pending.credit_bytes,
                            1,
                        ))
                    } else {
                        None
                    };
                    if let Some(source) = state.sources.get_mut(&key)
                        && matches!(source.descriptor, SourceDescriptor::Video(_))
                    {
                        source.bridge_desynchronized = true;
                        request_keyframe = true;
                    }
                    credit
                };
                if let Some(credit) = credit {
                    write_delivery_credit(credit)?;
                }
            }
        }
    }
    if let Some(credit) = immediate_credit {
        write_delivery_credit(credit)?;
    }
    if request_keyframe {
        request_keyframe_recoveries(
            shared,
            &[(key, None, messages::KEYFRAME_REASON_TRANSPORT_LOSS)],
        );
    }
    Ok(IngestOutcome::Accepted)
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
    validate_scene_structure(state, &nodes)?;
    if nodes.len() > state.config.max_nodes {
        return Err(invalid("node quota exceeded"));
    }
    state.nodes = nodes;
    advance_scene(state, producer, messages::SCENE_CHANGED_PRODUCER_COMMIT)?;
    advance_projection(state);
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

fn validate_scene_structure(
    state: &State,
    nodes: &HashMap<(ProducerId, u64), SceneNode>,
) -> io::Result<()> {
    let sources = state
        .sources
        .iter()
        .map(
            |(&(producer, source_id), source)| messages::SceneValidationSource {
                key: messages::SceneValidationKey {
                    owner_id: producer,
                    object_id: source_id,
                },
                is_video: matches!(source.descriptor, SourceDescriptor::Video(_)),
                linked_video: match &source.descriptor {
                    SourceDescriptor::Audio(config) => {
                        config.linked_video_source_id.map(|source_id| {
                            messages::SceneValidationKey {
                                owner_id: producer,
                                object_id: source_id,
                            }
                        })
                    }
                    _ => None,
                },
            },
        )
        .collect::<Vec<_>>();
    let nodes = nodes
        .values()
        .map(|node| messages::SceneValidationNode {
            owner_id: node.producer,
            node_id: node.config.node.node_id,
            fragment_id: 0,
            source: messages::SceneValidationKey {
                owner_id: node.producer,
                object_id: node.config.node.source_id,
            },
            x: node.config.node.x,
            y: node.config.node.y,
            width: node.config.node.width,
            height: node.config.node.height,
            clip: node.config.clip,
        })
        .collect::<Vec<_>>();
    messages::validate_scene_snapshot(&sources, &nodes)
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
    if state.producers.contains_key(&key.0) {
        advance_scene(state, key.0, messages::SCENE_CHANGED_SOURCE_LOSS)?;
    }
    advance_projection(state);
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
    state
        .pending_media_barriers
        .retain(|(owner, _)| *owner != producer);
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
                        source.descriptor.is_static()
                            && source.retained.is_some()
                            && source.capture_policy
                                & messages::CAPTURE_POLICY_DENY_POSTER_RETENTION
                                == 0
                    }))
    });
    prune_orphaned_sources(state);
    advance_projection(state);
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
            | messages::FEATURE_DECODER_DESCRIPTION_V1
            | messages::FEATURE_OBSERVABILITY_CORE_V1
            | messages::FEATURE_SOURCE_CAPTURE_POLICY_V1
            | messages::FEATURE_SOURCE_DESCRIPTOR_V1
            | messages::FEATURE_RASTER_DELTA_V1
            | messages::FEATURE_MEDIA_ORDER_BARRIER_V1
    )
}

fn offers_vivid_version(
    minimum_major: u64,
    minimum_minor: u64,
    maximum_major: u64,
    maximum_minor: u64,
) -> bool {
    let current = (u64::from(VIVID_MAJOR), u64::from(VIVID_MINOR));
    (minimum_major, minimum_minor) <= current && (maximum_major, maximum_minor) >= current
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

fn diagnostic_trace_guard() -> io::Result<Option<TraceGuard>> {
    let Some(directory) = std::env::var_os("VIVID_DIAGNOSTIC_TRACE_DIR") else {
        return Ok(None);
    };
    let mut hint = [0_u8; 16];
    getrandom::fill(&mut hint)
        .map_err(|error| io::Error::other(format!("trace hint generation failed: {error}")))?;
    let path =
        std::path::PathBuf::from(directory).join(format!("vvmux-{}.ndjson", std::process::id()));
    TraceGuard::file(&path, TraceComponent::Vvmux, TraceHop::Inner, hint).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::wire::{Connection, Endpoint};

    #[test]
    fn vivid_version_selection_accepts_only_ranges_containing_1_1() {
        assert!(!offers_vivid_version(0, 9, 0, 9));
        assert!(!offers_vivid_version(1, 0, 1, 0));
        assert!(offers_vivid_version(1, 1, 1, 1));
        assert!(offers_vivid_version(1, 0, 1, 1));
        assert!(!offers_vivid_version(1, 2, 2, 0));
    }

    #[test]
    fn nested_capture_policy_can_only_tighten() {
        let capture = messages::CAPTURE_POLICY_DENY_CAPTURE;
        let stricter = capture | messages::CAPTURE_POLICY_DENY_POSTER_RETENTION;
        assert_eq!(
            tightened_capture_policy(capture, stricter).unwrap(),
            Some(stricter)
        );
        assert_eq!(tightened_capture_policy(stricter, stricter).unwrap(), None);
        assert!(tightened_capture_policy(stricter, capture).is_err());
        assert!(tightened_capture_policy(0, messages::CAPTURE_POLICY_MASK + 1).is_err());
    }

    #[test]
    fn semantic_descriptor_handling_contains_no_locator_io_or_terminal_output() {
        let source = include_str!("media.rs");
        for line in source
            .lines()
            .filter(|line| line.contains("semantic_descriptor"))
        {
            for forbidden in [
                "std::fs",
                "File::",
                ".open(",
                "connect(",
                "Url::parse",
                "reqwest",
                "terminal::",
                "pty::",
            ] {
                assert!(
                    !line.contains(forbidden),
                    "descriptor handling line unexpectedly contains {forbidden}: {line}"
                );
            }
        }
    }

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
            capability_generation: 1,
            trace: None,
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
            projection_revision: 0,
            projected_sources: HashSet::new(),
            active_panes: HashSet::new(),
            deliveries: HashMap::new(),
            pending_media_barriers: HashSet::new(),
            next_delivery_id: 0,
            queued_bridge_bytes: 0,
            events: None,
            media_wakeup: None,
            next_connection: 0,
            connection_cancellers: HashMap::new(),
            delivery_metrics: crate::metrics::DeliveryMetrics::default(),
        }
    }

    #[test]
    fn capability_generation_is_reoriginated_without_feature_removal() {
        let mut initial = state();
        let (observation_sender, _observation_receiver) = mpsc::sync_channel(1);
        let accepted_features = HashSet::from([
            messages::FEATURE_RASTER_RGBA8,
            messages::FEATURE_VIDEO_ACCESS_UNIT_V1,
        ]);
        initial.producers.insert(
            1,
            Producer {
                pane: 7,
                tag: [0; 16],
                anchor_key: anchor::derive_key(&[0; 32], &[0; 16]),
                writer: Weak::new(),
                observation_sender,
                features: accepted_features.clone(),
                anchors: HashMap::new(),
                seen_anchors: HashSet::new(),
                scene_revision: SceneRevision::ZERO,
                observation_mask: 0,
                observation_sequence: ObservationSequence::ZERO,
                first_lost_source_sequence: None,
                first_lost_scene_sequence: None,
                waits: HashMap::new(),
            },
        );
        let state = Arc::new(Mutex::new(initial));
        let service = VirtualVivid {
            endpoint: String::new(),
            state: state.clone(),
            delivery_changed: Arc::new(Condvar::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            _trace_guard: None,
        };

        assert!(
            service
                .notify_capabilities_changed(messages::CAPS_CHANGE_REASON_MASK << 1)
                .is_err()
        );
        assert_eq!(state.lock().unwrap().capability_generation, 1);
        assert_eq!(
            service
                .notify_capabilities_changed(messages::CAPS_CHANGE_PRESENTER_POLICY)
                .unwrap(),
            2
        );
        let state = state.lock().unwrap();
        assert_eq!(state.capability_generation, 2);
        assert_eq!(state.producers[&1].features, accepted_features);
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
            codec_string: None,
            decoder_config: None,
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

    fn raster_source() -> Source {
        let mut source = barrier_source(1);
        source.descriptor = SourceDescriptor::Raster(RasterSourceConfig {
            source_id: 1,
            width: 2,
            height: 3,
            alpha_mode: messages::ALPHA_STRAIGHT,
            compression_mode: messages::COMPRESSION_RAW_OR_ZSTD,
        });
        source.raster_update = Some(RasterUpdateConfig {
            mode: messages::RASTER_FULL_FRAME_AND_DELTA,
            operation_limit: 4,
        });
        let pixels = [
            1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255,
        ];
        let retained = canonical_raster_full_body(1, 1, 0, 0, 2, 3, &pixels).unwrap();
        source.retained_bytes = retained.len();
        source.retained = Some(retained);
        source.sequence.accept(1, 1).unwrap();
        source.last_media_id = 1;
        source
    }

    #[test]
    fn raster_delta_composition_is_overlap_exact_and_budgeted() {
        let source = raster_source();
        let delta = media::raster_delta_frame_body(
            1,
            2,
            1,
            10,
            20,
            2,
            3,
            4,
            &[
                media::RasterDeltaOperation::Copy {
                    destination_x: 0,
                    destination_y: 1,
                    width: 2,
                    height: 2,
                    source_x: 0,
                    source_y: 0,
                },
                media::RasterDeltaOperation::Overwrite {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 1,
                    rgba: &[9, 0, 0, 255, 8, 0, 0, 255],
                },
            ],
            false,
        )
        .unwrap();
        let prepared = match prepare_raster(
            &source,
            match &source.descriptor {
                SourceDescriptor::Raster(config) => config,
                _ => unreachable!(),
            },
            &delta,
            source.raster_damage_window_started,
        )
        .unwrap()
        {
            RasterPreparation::Accepted(prepared) => prepared,
            RasterPreparation::Rejected { .. } => panic!("valid delta was rejected"),
        };
        let frame = media::parse_full_raster_frame(&prepared.body).unwrap();
        assert_eq!(&prepared.body[16..24], &[0; 8]);
        assert_eq!(
            media::decode_raster_pixels(frame).unwrap(),
            [
                9, 0, 0, 255, 8, 0, 0, 255, 1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255,
            ]
        );

        let mut exhausted = source;
        exhausted.raster_damage_pixels = 2 * 3 * RASTER_DAMAGE_FRAME_EQUIVALENTS;
        let one_pixel = media::raster_delta_frame_body(
            1,
            2,
            1,
            0,
            0,
            2,
            3,
            4,
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
        assert!(matches!(
            prepare_raster(
                &exhausted,
                match &exhausted.descriptor {
                    SourceDescriptor::Raster(config) => config,
                    _ => unreachable!(),
                },
                &one_pixel,
                exhausted.raster_damage_window_started,
            )
            .unwrap(),
            RasterPreparation::Rejected {
                reason: messages::NEED_FULL_FRAME_DAMAGE_BUDGET,
                notify: true,
            }
        ));
    }

    fn barrier_source(generation: u64) -> Source {
        Source {
            owner: 1,
            descriptor: SourceDescriptor::Video(ParsedVideoSourceConfig {
                codec_string: None,
                decoder_config: None,
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
            }),
            retained: None,
            sequence: MediaSequence::default(),
            retained_bytes: 0,
            playing: false,
            play_request: messages::PlayRequest::baseline(1, 0),
            ended: false,
            eos_epoch: None,
            bridge_desynchronized: false,
            minimum_epoch: 0,
            pending_keyframe_reason: None,
            last_pts_us: None,
            clock_started: None,
            clock_origin_pts_us: None,
            last_inner_record_sequence: 0,
            revision: SourceRevision::new(1),
            attachment_state: messages::ATTACHMENT_ATTACHED,
            attachment_generation: generation,
            credit_window_bytes: 1024,
            credit_window_packets: ROLLING_PACKET_CREDITS,
            outstanding_byte_credit: 1024,
            outstanding_packet_credit: INITIAL_PACKET_CREDITS,
            charged_bytes: 0,
            charged_packets: 0,
            last_media_id: 0,
            milestones: messages::MILESTONE_MEDIA_ATTACHED,
            causation_id: None,
            capture_policy: 0,
            semantic_descriptor: None,
            raster_update: None,
            raster_requires_full_reason: None,
            raster_damage_window_started: Instant::now(),
            raster_damage_pixels: 0,
        }
    }

    #[test]
    fn active_source_credit_converges_from_cold_start_to_rolling_window() {
        let mut state = state();
        state.sources.insert((1, 1), barrier_source(1));
        state.projected_sources.insert((1, 1));

        consume_source_credit(&mut state, (1, 1), 100).unwrap();
        let source = state.sources.get(&(1, 1)).unwrap();
        assert_eq!(
            (
                source.outstanding_byte_credit,
                source.outstanding_packet_credit
            ),
            (924, 0)
        );

        let returned = prepare_credit_return(&mut state, (1, 1), 100, 1);
        assert_eq!(
            (returned.bytes, returned.packets),
            (100, ROLLING_PACKET_CREDITS)
        );
        let source = state.sources.get(&(1, 1)).unwrap();
        assert_eq!(
            (
                source.outstanding_byte_credit,
                source.outstanding_packet_credit
            ),
            (1024, ROLLING_PACKET_CREDITS)
        );
        assert!(consume_source_credit(&mut state, (1, 1), 1025).is_err());
    }

    #[test]
    fn media_order_barrier_wait_covers_accept_mismatch_close_loss_and_timeout() {
        let mut initial = state();
        initial.sources.insert((1, 1), barrier_source(1));
        let shared = Arc::new(Mutex::new(initial));
        let changed = Arc::new(Condvar::new());
        let waiter = {
            let state = shared.clone();
            let changed = changed.clone();
            thread::spawn(move || {
                wait_for_media_barrier(&state, &changed, (1, 1), 1, 2, Duration::from_secs(1))
            })
        };
        shared
            .lock()
            .unwrap()
            .sources
            .get_mut(&(1, 1))
            .unwrap()
            .last_inner_record_sequence = 2;
        changed.notify_all();
        assert_eq!(waiter.join().unwrap(), MediaBarrierWait::Accepted);

        assert_eq!(
            wait_for_media_barrier(&shared, &changed, (1, 1), 2, 3, Duration::ZERO),
            MediaBarrierWait::AttachmentChanged
        );
        shared
            .lock()
            .unwrap()
            .sources
            .get_mut(&(1, 1))
            .unwrap()
            .attachment_state = messages::ATTACHMENT_CLOSED;
        assert_eq!(
            wait_for_media_barrier(&shared, &changed, (1, 1), 1, 3, Duration::from_secs(1)),
            MediaBarrierWait::AttachmentClosed
        );
        shared.lock().unwrap().sources.remove(&(1, 1));
        assert_eq!(
            wait_for_media_barrier(&shared, &changed, (1, 1), 1, 3, Duration::from_secs(1)),
            MediaBarrierWait::SourceLost
        );
        shared
            .lock()
            .unwrap()
            .sources
            .insert((1, 1), barrier_source(1));
        assert_eq!(
            wait_for_media_barrier(&shared, &changed, (1, 1), 1, 3, Duration::ZERO),
            MediaBarrierWait::TimedOut
        );
    }

    /// Decoding an encoded image must not hold the global media lock.
    ///
    /// That lock is also needed by the session actor for delivery completion, projection
    /// snapshots, marker observation, and anchor scrolling, so a large decode used to stall
    /// rendering and video for every other pane while one pane displayed an image.
    #[test]
    fn image_validation_does_not_hold_the_global_media_lock() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid-image-lock.sock");
        let service = match VirtualVivid::start(socket, MediaConfig::default()) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("virtual Vivid failed to start: {error}"),
        };

        // Large enough that the decode dominates any lock bookkeeping around it.
        const SIDE: u32 = 1024;
        let mut encoded = Vec::new();
        image::DynamicImage::new_rgba8(SIDE, SIDE)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .unwrap();
        let key = (1_u64, 3_u64);
        {
            let mut state = service.lock();
            let mut source = barrier_source(1);
            source.descriptor = SourceDescriptor::Image(ImageSourceConfig {
                source_id: key.1,
                encoding: messages::IMAGE_PNG,
                width: SIDE,
                height: SIDE,
                encoded_length: encoded.len() as u32,
                sha256: Some(Sha256::digest(&encoded).into()),
            });
            state.sources.insert(key, source);
        }

        // Contend for the lock throughout one validation and count how often it was obtainable.
        let contender = service.state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let contender_stop = stop.clone();
        let contender_thread = thread::spawn(move || {
            let mut acquisitions = 0_u64;
            while !contender_stop.load(Ordering::Acquire) {
                drop(
                    contender
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                );
                acquisitions += 1;
                thread::sleep(Duration::from_millis(1));
            }
            acquisitions
        });

        let record = BorrowedRecord {
            record_type: messages::IMAGE_DATA,
            flags: 0,
            object_id: key.1,
            sequence: 1,
            body: &encoded,
        };
        let started = Instant::now();
        validate_encoded_image(&service.state, key, &record).unwrap();
        let decode = started.elapsed();
        stop.store(true, Ordering::Release);
        let acquisitions = contender_thread.join().unwrap();

        assert!(
            acquisitions > 4,
            "the lock was obtainable only {acquisitions} times during a {decode:?} decode, \
             so the decode is still holding it"
        );

        // The behavioural contract is unchanged: every rejection still happens.
        let mut corrupt = encoded.clone();
        *corrupt.last_mut().unwrap() ^= 0xff;
        let bad_hash = BorrowedRecord {
            body: &corrupt,
            ..record
        };
        assert!(validate_encoded_image(&service.state, key, &bad_hash).is_err());
        let truncated = BorrowedRecord {
            body: &encoded[..encoded.len() - 1],
            ..record
        };
        assert!(validate_encoded_image(&service.state, key, &truncated).is_err());
        // A record for a non-image source is not this function's concern.
        let other = BorrowedRecord {
            record_type: messages::VIDEO_PACKET,
            ..record
        };
        assert!(validate_encoded_image(&service.state, key, &other).is_ok());
    }

    /// A burst of dropped packets must produce one recovery request, not one per packet.
    ///
    /// A producer of encoded video cannot manufacture a key packet; it discards until the next
    /// natural one, so each extra request costs up to a full GOP of frozen video and pushes
    /// `minimum_epoch` further beyond what the producer will ever reach.
    #[test]
    fn repeated_keyframe_requests_are_damped_until_the_producer_catches_up() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid-keyframe-damping.sock");
        let service = match VirtualVivid::start(socket, MediaConfig::default()) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("virtual Vivid failed to start: {error}"),
        };

        let key = (1_u64, 5_u64);
        {
            let mut state = service.lock();
            let mut source = barrier_source(1);
            source.playing = true;
            source.bridge_desynchronized = true;
            source.minimum_epoch = 1;
            source.pending_keyframe_reason = Some(messages::KEYFRAME_REASON_DECODER_ERROR);
            state.sources.insert(key, source);
            state.projected_sources.insert(key);
        }

        let after_first = {
            let state = service.lock();
            (
                state.sources[&key].minimum_epoch,
                state.delivery_metrics.keyframe_requests,
                state.delivery_metrics.keyframe_requests_damped,
            )
        };
        assert_eq!(after_first.2, 0);

        for _ in 0..32 {
            service.request_keyframe(key, None, messages::KEYFRAME_REASON_DECODER_ERROR);
        }
        let state = service.lock();
        assert_eq!(
            state.sources[&key].minimum_epoch, after_first.0,
            "a damped request must not push the demanded epoch further out"
        );
        assert_eq!(
            state.delivery_metrics.keyframe_requests, after_first.1,
            "no further NEED_KEYFRAME should be emitted while recovery is outstanding"
        );
        assert_eq!(state.delivery_metrics.keyframe_requests_damped, 32);
        assert!(state.sources[&key].bridge_desynchronized);
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
        let mut unsupported = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        unsupported
            .write_record(
                messages::HELLO,
                0,
                0,
                &messages::encode_hello(
                    1,
                    &messages::HelloConfig {
                        minimum_major: 1,
                        minimum_minor: 0,
                        maximum_major: 1,
                        maximum_minor: 0,
                        token: &token,
                        producer: "unsupported-version-test",
                        producer_version: "test",
                        required_features: &[],
                        optional_features: &[],
                        maximum_record_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
                        authentication_kind: messages::AUTHENTICATION_WINDOW_ROOT,
                        preserved_fields: &[],
                    },
                ),
            )
            .unwrap();
        let rejection = unsupported.read_record().unwrap();
        assert_eq!(rejection.record_type, messages::ERROR);
        let rejection = messages::parse_error_reply(&rejection.body).unwrap();
        assert_eq!(rejection.code, messages::ERROR_UNSUPPORTED_VERSION);
        assert_eq!(rejection.supported_version, Some((1, 1)));

        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        let preserved = [vivid_protocol::cbor::PreservedField {
            key: 42,
            encoded_value: vec![0x82, 0x01, 0xf5],
        }];
        let (_, baseline_hello) = messages::parse_hello(&messages::hello(1, &token)).unwrap();
        control
            .write_record(
                messages::HELLO,
                0,
                0,
                &messages::encode_hello(
                    1,
                    &messages::HelloConfig {
                        minimum_major: 1,
                        minimum_minor: 1,
                        maximum_major: 1,
                        maximum_minor: 1,
                        token: &token,
                        producer: &baseline_hello.producer,
                        producer_version: &baseline_hello.producer_version,
                        required_features: &baseline_hello.required_features,
                        optional_features: &baseline_hello.optional_features,
                        maximum_record_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
                        authentication_kind: messages::AUTHENTICATION_WINDOW_ROOT,
                        preserved_fields: &preserved,
                    },
                ),
            )
            .unwrap();
        let welcome = control.read_record().unwrap();
        assert_eq!(welcome.record_type, messages::WELCOME);
        assert_eq!(
            messages::parse_welcome(&welcome.body)
                .unwrap()
                .preserved_fields,
            preserved
        );

        let video = messages::VideoSourceConfig {
            codec_string: None,
            decoder_config: None,
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
            codec_string: None,
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

        service.request_keyframe(key, None, messages::KEYFRAME_REASON_TRANSPORT_LOSS);
        let need_keyframe = control.read_record().unwrap();
        assert_eq!(need_keyframe.record_type, messages::NEED_KEYFRAME);
        let request = messages::parse_need_keyframe(&need_keyframe.body).unwrap();
        assert_eq!(request.minimum_epoch, 0);
        assert_eq!(request.reason, messages::KEYFRAME_REASON_TRANSPORT_LOSS);

        service.request_keyframe(key, None, messages::KEYFRAME_REASON_TRANSPORT_LOSS);
        {
            let state = service.lock();
            assert_eq!(state.delivery_metrics.keyframe_requests, 1);
            assert_eq!(state.delivery_metrics.keyframe_requests_damped, 1);
        }

        service.request_keyframe(key, None, messages::KEYFRAME_REASON_DECODER_ERROR);
        let escalated = control.read_record().unwrap();
        assert_eq!(escalated.record_type, messages::NEED_KEYFRAME);
        let escalated = messages::parse_need_keyframe(&escalated.body).unwrap();
        assert_eq!(escalated.minimum_epoch, 1);
        assert_eq!(
            escalated.reason,
            messages::KEYFRAME_REASON_DECODER_ERROR,
            "decoder loss must escalate an outstanding transport-only recovery"
        );
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
    fn hello_is_rejected_until_the_pane_has_display_metrics() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid-no-metrics.sock");
        let service = match VirtualVivid::start(socket, MediaConfig::default()) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter metrics socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        let endpoint = service_endpoint(&service);

        // No client has attached, so the projection pass has never called `update_metrics`. The
        // pane cannot back a spec-valid WELCOME, so the session must be refused outright rather
        // than answered with a zero viewport the producer would reject as malformed.
        let mut early = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        early
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))
            .unwrap();
        let rejection = early.read_record().unwrap();
        assert_eq!(rejection.record_type, messages::ERROR);
        let rejection = messages::parse_error_reply(&rejection.body).unwrap();
        assert_eq!(rejection.code, messages::ERROR_PRECONDITION_FAILED);
        assert!(!rejection.fatal, "attaching a client clears this condition");
        assert_eq!(
            rejection.detail.get_bool(messages::ERROR_DETAIL_RETRYABLE),
            Some(true)
        );

        // Once the pane reports real metrics the same capability completes the handshake, and the
        // WELCOME carries them rather than the zeroes that failed mandatory-field validation.
        service.update_metrics(7, 80, 22, (10, 20));
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))
            .unwrap();
        let accepted = control.read_record().unwrap();
        assert_eq!(accepted.record_type, messages::WELCOME);
        let welcome = messages::parse_welcome(&accepted.body).unwrap();
        assert_eq!(welcome.cell_width, 10);
        assert_eq!(welcome.cell_height, 20);
        assert_eq!(welcome.viewport_width, 800);
        assert_eq!(welcome.viewport_height, 440);
    }

    /// A realistic surface: a small damaged region should cross the hop as a delta rather than as
    /// the whole framebuffer, which is the bandwidth this stage exists to remove.
    #[test]
    fn delta_re_origination_is_used_when_it_is_smaller_than_a_full_frame() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid-raster-delta-size.sock");
        let (event_sender, event_receiver) = mpsc::sync_channel(8);
        let service = match VirtualVivid::start_with_events(
            socket,
            MediaConfig::default(),
            Some(event_sender),
        ) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("virtual Vivid failed to start: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        service.update_metrics(7, 80, 24, (10, 20));

        const WIDTH: u32 = 256;
        const HEIGHT: u32 = 256;
        let endpoint = service_endpoint(&service);
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token))
            .unwrap();
        assert_eq!(
            control.read_record().unwrap().record_type,
            messages::WELCOME
        );
        control
            .write_record(
                messages::CREATE_RASTER,
                0,
                9,
                &messages::create_raster_delta_config(
                    2,
                    &RasterSourceConfig {
                        source_id: 9,
                        width: WIDTH,
                        height: HEIGHT,
                        alpha_mode: messages::ALPHA_STRAIGHT,
                        compression_mode: messages::COMPRESSION_NONE,
                    },
                    4,
                )
                .unwrap(),
            )
            .unwrap();
        let ready = messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();
        let mut raster = Connection::open(&endpoint, ConnectionKind::Raster).unwrap();
        raster
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                9,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();

        let mut scene_writer = service.lock();
        scene_writer.active_panes.insert(7);
        drop(scene_writer);
        service.projection_snapshot(&HashSet::from([7]));

        let pixels = vec![0_u8; (WIDTH * HEIGHT * 4) as usize];
        let full = media::raster_frame_body(1, 1, WIDTH, HEIGHT, &pixels).unwrap();
        raster
            .write_record(messages::RASTER_FRAME, 0, 9, &full)
            .unwrap();
        let first = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            first.body, full,
            "the first frame of a source is always full"
        );
        service.complete_bridge_delivery(first.delivery_id, true);

        // Repaint a 16x16 corner: 1 KiB of pixels against a 256 KiB framebuffer.
        let delta = media::raster_delta_frame_body(
            1,
            2,
            1,
            10_000,
            16_000,
            WIDTH,
            HEIGHT,
            4,
            &[media::RasterDeltaOperation::Overwrite {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
                rgba: &vec![255_u8; 16 * 16 * 4],
            }],
            false,
        )
        .unwrap();
        raster
            .write_record(messages::RASTER_FRAME, 0, 9, &delta)
            .unwrap();
        let second = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            second.body, delta,
            "a delta smaller than the framebuffer must cross the hop as a delta"
        );
        assert!(
            second.body.len() * 20 < full.len(),
            "expected a large saving, got {} against {}",
            second.body.len(),
            full.len()
        );
    }

    #[test]
    fn projected_raster_frames_are_forwarded_and_bridge_paced() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "vivid-raster-paced.sock");
        let (event_sender, event_receiver) = mpsc::sync_channel(8);
        let service = match VirtualVivid::start_with_events(
            socket,
            MediaConfig::default(),
            Some(event_sender),
        ) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter raster socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token = service.issue_pane_capability(7).unwrap();
        service.update_metrics(7, 80, 22, (10, 20));
        // Match the session actor: establish the active projection before the pane producer is
        // born, so media arriving immediately after SOURCE_READY is bridge-eligible.
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
        control
            .write_record(
                messages::CREATE_RASTER,
                0,
                9,
                &messages::create_raster_delta_config(
                    2,
                    &RasterSourceConfig {
                        source_id: 9,
                        width: 2,
                        height: 2,
                        alpha_mode: messages::ALPHA_STRAIGHT,
                        compression_mode: messages::COMPRESSION_RAW_OR_ZSTD,
                    },
                    4,
                )
                .unwrap(),
            )
            .unwrap();
        let ready = messages::parse_source_ready(&control.read_record().unwrap().body).unwrap();
        assert_eq!(ready.packet_credits, 1);
        assert_eq!(ready.rolling_packet_window, ROLLING_PACKET_CREDITS);
        assert_eq!(ready.delta_operation_limit, Some(4));
        let mut raster = Connection::open(&endpoint, ConnectionKind::Raster).unwrap();
        raster
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                9,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();

        let revision_before_frames = service.revision();
        let expected_full_frame_len = media::raster_frame_body(1, 1, 2, 2, &[0; 16])
            .unwrap()
            .len();
        let first_pixels = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let first = media::raster_frame_body(1, 1, 2, 2, &first_pixels).unwrap();
        raster
            .write_record(messages::RASTER_FRAME, 0, 9, &first)
            .unwrap();
        let first_event = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(first_event.record_type, messages::RASTER_FRAME);
        assert_eq!(first_event.body, first);
        assert!(first_event.delivery_id > 0);
        assert!(!service.complete_bridge_delivery(first_event.delivery_id, true));
        let first_credit = control.read_record().unwrap();
        assert_eq!(first_credit.record_type, messages::CREDIT);
        assert_eq!(first_credit.object_id, 9);
        let first_credit_body = messages::parse_credit(&first_credit.body).unwrap();
        assert_eq!(first_credit_body.packets, ROLLING_PACKET_CREDITS);
        assert_eq!(service.revision(), revision_before_frames);

        let second = media::raster_delta_frame_body(
            1,
            2,
            1,
            10_000,
            16_000,
            2,
            2,
            4,
            &[
                media::RasterDeltaOperation::Copy {
                    destination_x: 0,
                    destination_y: 1,
                    width: 2,
                    height: 1,
                    source_x: 0,
                    source_y: 0,
                },
                media::RasterDeltaOperation::Overwrite {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    rgba: &[0, 0, 0, 255],
                },
            ],
            false,
        )
        .unwrap();
        raster
            .write_record(messages::RASTER_FRAME, 0, 9, &second)
            .unwrap();
        let second_event = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(second_event.record_type, messages::RASTER_FRAME);
        // On a 2x2 surface two operation descriptors cost more than the whole framebuffer, so the
        // composed full frame is the smaller form and wins. `delta_re_origination_is_used_when_it_
        // is_smaller_than_a_full_frame` covers the size where the delta pays.
        assert!(second.len() >= expected_full_frame_len);
        let expected_pixels = [0, 0, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255];
        let expected =
            canonical_raster_full_body(1, 2, 10_000, 16_000, 2, 2, &expected_pixels).unwrap();
        assert_eq!(second_event.body, expected.as_ref());
        let outer_form = media::parse_full_raster_frame(&second_event.body).unwrap();
        assert_eq!(outer_form.frame_id, 2);
        assert_eq!(&second_event.body[16..24], &[0; 8]);
        assert!(second_event.delivery_id > first_event.delivery_id);
        assert_eq!(
            service.revision(),
            revision_before_frames,
            "live raster content must not rebuild the outer projection per frame"
        );
        assert!(!service.complete_bridge_delivery(second_event.delivery_id, true));
        assert_eq!(control.read_record().unwrap().record_type, messages::CREDIT);

        let bad_base = media::raster_delta_frame_body(
            1,
            3,
            99,
            20_000,
            16_000,
            2,
            2,
            4,
            &[media::RasterDeltaOperation::Overwrite {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: &[1, 2, 3, 255],
            }],
            false,
        )
        .unwrap();
        raster
            .write_record(messages::RASTER_FRAME, 0, 9, &bad_base)
            .unwrap();
        let rejected = control.read_record().unwrap();
        assert_eq!(rejected.record_type, messages::ERROR);
        assert_eq!(
            messages::parse_error_reply(&rejected.body).unwrap().code,
            messages::ERROR_BAD_STATE
        );
        let need_full = control.read_record().unwrap();
        assert_eq!(need_full.record_type, messages::NEED_FULL_FRAME);
        assert_eq!(
            messages::parse_need_full_frame(&need_full.body).unwrap(),
            messages::NeedFullFrame {
                source_id: 9,
                reason: messages::NEED_FULL_FRAME_BASE_UNAVAILABLE,
            }
        );
        assert_eq!(control.read_record().unwrap().record_type, messages::CREDIT);

        let recovery_pixels = [7_u8; 16];
        let recovery = media::raster_frame_body(1, 5, 2, 2, &recovery_pixels).unwrap();
        raster
            .write_record(messages::RASTER_FRAME, 0, 9, &recovery)
            .unwrap();
        let recovery_event = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(recovery_event.body, recovery);
        assert!(!service.complete_bridge_delivery(recovery_event.delivery_id, true));
        assert_eq!(control.read_record().unwrap().record_type, messages::CREDIT);

        let snapshot = service.projection_snapshot(&HashSet::from([7]));
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(
            snapshot.sources[0].retained.as_deref(),
            Some(recovery.as_slice())
        );
        assert_eq!(snapshot.sources[0].last_inner_record_sequence, 5);
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
            codec_string: None,
            decoder_config: None,
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
        assert!(service.complete_bridge_delivery(event.delivery_id, false));
        let credit = control.read_record().unwrap();
        assert_eq!(credit.record_type, messages::CREDIT);
        let recovery = control.read_record().unwrap();
        assert_eq!(recovery.record_type, messages::NEED_KEYFRAME);
        let recovery = messages::parse_need_keyframe(&recovery.body).unwrap();
        assert_eq!(recovery.minimum_epoch, 1);
        assert_eq!(
            recovery.reason,
            messages::KEYFRAME_REASON_TRANSPORT_LOSS,
            "a failed outer delivery must preserve the current epoch"
        );

        let recovery_packet = media::video_packet_body(media::VideoPacket {
            epoch: 1,
            packet_id: 2,
            pts_us: 33_000,
            dts_us: 33_000,
            duration_us: 33_000,
            key: true,
            data: &[0, 0, 0, 1, 0x65, 0x99],
        })
        .unwrap();
        video_media
            .write_record(messages::VIDEO_PACKET, 0, 9, &recovery_packet)
            .unwrap();
        let recovery_event = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!service.complete_bridge_delivery(recovery_event.delivery_id, true));
        assert_eq!(control.read_record().unwrap().record_type, messages::CREDIT);

        let pending_packet = media::video_packet_body(media::VideoPacket {
            epoch: 1,
            packet_id: 3,
            pts_us: 66_000,
            dts_us: 66_000,
            duration_us: 33_000,
            key: false,
            data: &[0, 0, 0, 1, 0x41, 0xaa],
        })
        .unwrap();
        video_media
            .write_record(messages::VIDEO_PACKET, 0, 9, &pending_packet)
            .unwrap();
        let pending_event = event_receiver.recv_timeout(Duration::from_secs(1)).unwrap();

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
        assert!(!service.complete_bridge_delivery(pending_event.delivery_id, true));
        let (mut control, credit, eos) =
            reply_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(credit.record_type, messages::CREDIT);
        assert_eq!(credit.object_id, 9);
        assert_eq!(eos.record_type, messages::OK);
        assert_eq!(messages::request_id(&eos.body).unwrap(), 3);

        let audio = messages::AudioSourceConfig {
            codec_string: None,
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
            codec_string: None,
            decoder_config: None,
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

    #[test]
    fn observability_is_pane_scoped_and_outer_revision_is_independent() {
        let directory = tempfile::tempdir().unwrap();
        let socket = test_virtual_endpoint(&directory, "observability.sock");
        let service = match VirtualVivid::start(socket, MediaConfig::default()) {
            Ok(service) => service,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping virtual presenter socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual presenter start failed: {error}"),
        };
        let token_one = service.issue_pane_capability(7).unwrap();
        let token_two = service.issue_pane_capability(8).unwrap();
        // Match the session actor: a pane reports metrics before a producer can be admitted.
        service.update_metrics(7, 80, 22, (10, 20));
        service.update_metrics(8, 80, 22, (10, 20));
        let endpoint = service_endpoint(&service);

        let mut owner = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        owner
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token_one))
            .unwrap();
        assert_eq!(owner.read_record().unwrap().record_type, messages::WELCOME);
        owner
            .write_record(
                messages::SET_OBSERVATION,
                0,
                0,
                &messages::set_observation(
                    2,
                    messages::OBSERVE_SOURCE_TRANSITIONS | messages::OBSERVE_SCENE_CHANGES,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(owner.read_record().unwrap().record_type, messages::OK);
        owner
            .write_record(
                messages::CREATE_IMAGE,
                0,
                1,
                &messages::create_image(
                    3,
                    &messages::ImageSourceConfig {
                        source_id: 1,
                        encoding: messages::IMAGE_PNG,
                        width: 1,
                        height: 1,
                        encoded_length: 1,
                        sha256: None,
                    },
                ),
            )
            .unwrap();
        let ready = owner.read_record().unwrap();
        assert_eq!(ready.record_type, messages::SOURCE_READY);
        let ready = messages::parse_source_ready(&ready.body).unwrap();
        assert_eq!(ready.initial_source_revision, SourceRevision::new(1));
        let changed = owner.read_record().unwrap();
        assert_eq!(changed.record_type, messages::SOURCE_CHANGED);
        let changed = messages::parse_source_changed(&changed.body).unwrap();
        assert_eq!(changed.source_id, 1);
        assert_eq!(changed.source_revision, SourceRevision::new(1));

        let mut other = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        other
            .write_record(messages::HELLO, 0, 0, &messages::hello(1, &token_two))
            .unwrap();
        assert_eq!(other.read_record().unwrap().record_type, messages::WELCOME);
        other
            .write_record(
                messages::QUERY_SOURCE,
                0,
                1,
                &messages::query_source(2, 1).unwrap(),
            )
            .unwrap();
        let rejected = other.read_record().unwrap();
        assert_eq!(rejected.record_type, messages::ERROR);
        assert_eq!(
            messages::parse_error_reply(&rejected.body).unwrap().code,
            messages::ERROR_NOT_FOUND
        );

        owner
            .write_record(
                messages::WAIT_SOURCE,
                0,
                1,
                &messages::wait_source(
                    4,
                    messages::WaitSource {
                        source_id: 1,
                        condition: messages::WAIT_FIRST_VISIBLE_PRESENTATION,
                        value: None,
                        timeout_us: 1_000_000,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        let not_visible = owner.read_record().unwrap();
        assert_eq!(not_visible.record_type, messages::ERROR);
        assert_eq!(
            messages::parse_error_reply(&not_visible.body).unwrap().code,
            messages::ERROR_NOT_VISIBLE
        );

        let outer_generations = HashMap::from([(
            crate::ipc::BridgeSourceKey {
                producer: 1,
                source: 1,
            },
            7,
        )]);
        let first = service.pane_status(7, 41, &outer_generations, Default::default());
        let second = service.pane_status(7, 42, &outer_generations, Default::default());
        assert_eq!(first.virtual_scene_revision, second.virtual_scene_revision);
        assert_eq!(
            first.sources[0].source_revision, second.sources[0].source_revision,
            "outer applied revision must not perturb virtual source revision"
        );
        assert_eq!(first.sources[0].attachment_generation, 0);
        assert_eq!(first.sources[0].outer_attachment_generation, Some(7));
        assert_eq!(second.outer_projection_revision, 42);
    }
}
