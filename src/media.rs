//! Vivid 1.5 virtual presenter used by panes.
//!
//! This module terminates the inner session. Nothing secret-bearing or authoritative is relayed:
//! projection snapshots contain validated semantic state and portable media bodies only.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use vivid_protocol::anchor::{self, AnchorKey};
use vivid_protocol::auth::{self, Secret32};
use vivid_protocol::cbor::Value;
use vivid_protocol::identity::{PresenterInstanceId, SessionIdentity};
use vivid_protocol::media;
use vivid_protocol::messages::{
    self, ChannelOpen, Envelope, ErrorDetail, ErrorReply, Hello, HelloAuthentication, StrictMap,
    Welcome, WelcomeAuthentication,
};
use vivid_protocol::registry;
use vivid_protocol::resource::{Resource, ResourceContract};
use vivid_protocol::revision::{
    ChannelGeneration, SceneRevision, SurfaceGeneration, SurfaceRevision, TargetGeneration,
};
use vivid_protocol::scene::SceneNode as ProtocolSceneNode;
use vivid_protocol::surface::{SurfaceDefinition, SurfaceDescriptor, SurfaceState};
use vivid_protocol::track::{
    ImageConfiguration, KindConfiguration, MILESTONE_BUFFERED_ENDED, MILESTONE_CHANNEL_ACCEPTED,
    MILESTONE_CHANNEL_DETACHED, MILESTONE_CLOCK_STARTED, MILESTONE_DECODER_INITIALIZED,
    MILESTONE_EOS_ACCEPTED, MILESTONE_OUTPUT_READY, MILESTONE_PRESENTED, RasterConfiguration,
    TrackConfiguration, TrackState, VideoConfiguration,
};
use vivid_protocol::wire::{ConnectionKind, RECORD_OPTIONAL, Record};

use crate::config::Media as MediaConfig;
use crate::ipc::{
    PaneMediaNodeStatus, PaneMediaStatus, PaneMediaSurfaceStatus, PaneMediaTrackStatus,
};
use crate::layout::PaneId;
use crate::platform::{VirtualPresenterEndpoint, VirtualPresenterListener};
use crate::vivid_transport::{Reader, Writer};

const MAX_CONNECTIONS: usize = 64;
const MAX_SESSIONS: usize = 16;
const MAX_WAITS: usize = 64;
const CHANNEL_OPEN_DEADLINE_US: u64 = 30_000_000;
const MAX_WAIT_US: u64 = 24 * 60 * 60 * 1_000_000;
const INITIAL_FLOW_RECORDS: u64 = 1;
const MAX_ACTIVE_ANCHORS: usize = 256;

pub type ProducerId = u64;
pub type SourceKey = crate::ipc::BridgeSourceKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframeRequestOutcome {
    Forwarded,
    Damped,
    Ignored,
}

pub struct OuterMediaProjection<'a> {
    pub compatibility_revision: u64,
    pub apply_sequence: u64,
    pub bridge_instance_id: Option<u64>,
    pub bridge_local_revision: u64,
    pub attachment_generations: &'a HashMap<crate::ipc::BridgeSourceKey, u64>,
}

#[derive(Debug, Clone)]
pub struct AudioSourceConfig {
    pub linked_video_source_id: Option<u64>,
    pub codec: String,
    pub packetization: String,
    pub extradata: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_mask: u64,
    pub bitrate: u64,
    pub max_access_unit_bytes: u32,
    pub codec_string: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SourceDescriptor {
    Raster(RasterConfiguration),
    Image(ImageConfiguration),
    Video(VideoConfiguration),
    Audio(AudioSourceConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayRequest {
    pub start_pts_us: i64,
    pub minimum_buffer_us: u64,
    pub maximum_latency_us: u64,
    pub rate_32_32: i64,
    pub late_policy: u64,
    pub loop_count: u64,
    pub start_policy: u64,
}

impl PlayRequest {
    fn baseline() -> Self {
        Self {
            start_pts_us: 0,
            minimum_buffer_us: 1,
            maximum_latency_us: 1_000_000,
            rate_32_32: 1_i64 << 32,
            late_policy: 1,
            loop_count: 0,
            start_policy: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SemanticDescriptor {
    pub role: u64,
    pub title: String,
    pub content_revision: u64,
    pub semantic_availability: u64,
    pub locator: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeConfig {
    pub node_id: u64,
    pub track: SourceKey,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub z_index: i64,
    pub visible: bool,
    pub anchor_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneNodeConfig {
    pub node: NodeConfig,
    pub clip: Option<ClipRect>,
}

#[derive(Debug, Clone)]
pub struct SceneNode {
    pub producer: ProducerId,
    pub pane: PaneId,
    pub config: SceneNodeConfig,
}

#[derive(Debug, Clone)]
pub struct ProjectionSnapshot {
    pub revision: u64,
    pub surfaces: Vec<SnapshotSurface>,
    pub sources: Vec<SnapshotSource>,
    pub nodes: Vec<SceneNode>,
    pub live_nodes: Vec<(ProducerId, u64)>,
    pub videos_needing_keyframes: Vec<SourceKey>,
}

#[derive(Debug, Clone)]
pub struct SnapshotSurface {
    pub producer: ProducerId,
    pub context: u64,
    pub surface: u64,
    pub logical_width: u64,
    pub logical_height: u64,
    pub capture_policy: u64,
    pub semantic_descriptor: SemanticDescriptor,
}

#[derive(Debug, Clone)]
pub struct SnapshotSource {
    pub key: SourceKey,
    pub descriptor: SourceDescriptor,
    pub retained: Option<Arc<[u8]>>,
    pub first_visible_presented: bool,
    pub playing: bool,
    pub play_request: PlayRequest,
    pub eos_epoch: Option<u32>,
    #[allow(dead_code)]
    pub last_inner_record_sequence: u64,
    pub causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    pub capture_policy: u64,
    pub semantic_descriptor: Option<SemanticDescriptor>,
    pub raster_delta_operation_limit: Option<u32>,
}

#[derive(Debug)]
pub struct MediaEvent {
    pub delivery_id: u64,
    pub source: SourceKey,
    pub record_type: u16,
    pub recovered_keyframe: Option<(u32, i64)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SurfaceKey {
    session: u64,
    context: u64,
    surface: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TrackKey {
    surface: SurfaceKey,
    track: u64,
}

fn bridge_track_key(key: TrackKey) -> SourceKey {
    SourceKey {
        producer: key.surface.session,
        context: key.surface.context,
        surface: key.surface.surface,
        track: key.track,
    }
}

fn inner_track_key(key: SourceKey) -> TrackKey {
    TrackKey {
        surface: SurfaceKey {
            session: key.producer,
            context: key.context,
            surface: key.surface,
        },
        track: key.track,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NodeKey {
    session: u64,
    context: u64,
    node: u64,
}

struct SessionRuntime {
    pane: PaneId,
    closed: bool,
    session_tag: [u8; messages::SESSION_TAG_BYTES],
    channel_key: Secret32,
    anchor_key: AnchorKey,
    writer: Arc<Writer>,
    root_context: u64,
    scene_revision: SceneRevision,
    target_generation: TargetGeneration,
    anchors: HashMap<(u64, u64), (i32, usize)>,
    seen_anchors: HashSet<(u64, u64)>,
    cancelled_waits: HashSet<u64>,
    pending_waits: usize,
}

struct SurfaceEntry {
    state: SurfaceState,
    active_slots: HashMap<u64, u64>,
}

struct TrackEntry {
    configuration: TrackConfiguration,
    state: TrackState,
    channel_writer: Option<Arc<Writer>>,
    retained: Option<Arc<[u8]>>,
    playing: bool,
    play_request: PlayRequest,
    eos_epoch: Option<u32>,
    last_record_sequence: u64,
    last_pts_us: i64,
    outer_presented: bool,
    recovery_pending: bool,
    causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
}

#[derive(Clone)]
enum NodeMutation {
    Create(ProtocolSceneNode),
    Update(ProtocolSceneNode),
    Delete(NodeKey),
}

struct NodeEntry {
    pane: PaneId,
    node: ProtocolSceneNode,
}

struct PendingDelivery {
    track: TrackKey,
    bytes: u64,
}

#[derive(Clone)]
struct CachedMutation {
    fingerprint: [u8; 32],
    record_type: u16,
    object_id: u64,
    body: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Metrics {
    generation: u64,
    viewport_width: u32,
    viewport_height: u32,
    columns: u32,
    rows: u32,
    cell_width: u32,
    cell_height: u32,
}

struct State {
    config: MediaConfig,
    presenter: PresenterInstanceId,
    capabilities: HashMap<PaneId, Secret32>,
    metrics: HashMap<PaneId, Metrics>,
    sessions: HashMap<u64, SessionRuntime>,
    surfaces: HashMap<SurfaceKey, SurfaceEntry>,
    tracks: HashMap<TrackKey, TrackEntry>,
    nodes: HashMap<NodeKey, NodeEntry>,
    transactions: HashMap<(u64, u64, u64), Vec<NodeMutation>>,
    next_session: u64,
    projection_revision: u64,
    projected_sources: HashSet<SourceKey>,
    deliveries: HashMap<u64, PendingDelivery>,
    idempotency: HashMap<(u64, [u8; messages::IDEMPOTENCY_KEY_BYTES]), CachedMutation>,
    idempotency_order: std::collections::VecDeque<(u64, [u8; messages::IDEMPOTENCY_KEY_BYTES])>,
    next_delivery: u64,
    events: Option<mpsc::SyncSender<MediaEvent>>,
    media_wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
    connections: usize,
    delivery_metrics: crate::metrics::DeliveryMetrics,
}

pub struct VirtualVivid {
    endpoint: String,
    state: Arc<Mutex<State>>,
    delivery_changed: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
}

impl VirtualVivid {
    #[allow(dead_code)]
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
        let mut presenter = [0_u8; 16];
        getrandom::fill(&mut presenter).map_err(io::Error::other)?;
        let state = Arc::new(Mutex::new(State {
            config,
            presenter: PresenterInstanceId(presenter),
            capabilities: HashMap::new(),
            metrics: HashMap::new(),
            sessions: HashMap::new(),
            surfaces: HashMap::new(),
            tracks: HashMap::new(),
            nodes: HashMap::new(),
            transactions: HashMap::new(),
            next_session: 0,
            projection_revision: 0,
            projected_sources: HashSet::new(),
            deliveries: HashMap::new(),
            idempotency: HashMap::new(),
            idempotency_order: std::collections::VecDeque::new(),
            next_delivery: 0,
            events,
            media_wakeup: None,
            connections: 0,
            delivery_metrics: crate::metrics::DeliveryMetrics::default(),
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
            .name("vvmux-vivid-1.5-listener".into())
            .spawn(move || accept_loop(listener, state, delivery_changed, shutdown))?;
        Ok(service)
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn set_media_wakeup(&self, wakeup: Arc<dyn Fn() + Send + Sync>) {
        lock(&self.state).media_wakeup = Some(wakeup);
    }

    pub fn issue_pane_capability(&self, pane: PaneId) -> io::Result<String> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        lock(&self.state)
            .capabilities
            .insert(pane, Secret32::new(bytes));
        Ok(hex(&bytes))
    }

    pub fn revoke_pane(&self, pane: PaneId) {
        let mut state = lock(&self.state);
        state.capabilities.remove(&pane);
        let sessions = state
            .sessions
            .iter()
            .filter_map(|(id, session)| (session.pane == pane).then_some(*id))
            .collect::<Vec<_>>();
        for session in sessions {
            cleanup_session(&mut state, session);
        }
        state.metrics.remove(&pane);
        advance_projection(&mut state);
    }

    pub fn update_metrics(&self, pane: PaneId, columns: u16, rows: u16, cell: (u16, u16)) {
        if columns == 0 || rows == 0 || cell.0 == 0 || cell.1 == 0 {
            return;
        }
        let mut state = lock(&self.state);
        let generation = state
            .metrics
            .get(&pane)
            .and_then(|metrics| metrics.generation.checked_add(1))
            .unwrap_or(1);
        let metrics = Metrics {
            generation,
            viewport_width: u32::from(columns) * u32::from(cell.0),
            viewport_height: u32::from(rows) * u32::from(cell.1),
            columns: u32::from(columns),
            rows: u32::from(rows),
            cell_width: u32::from(cell.0),
            cell_height: u32::from(cell.1),
        };
        state.metrics.insert(pane, metrics);
        for session in state
            .sessions
            .values_mut()
            .filter(|session| session.pane == pane && !session.closed)
        {
            session.target_generation = TargetGeneration::new(generation);
            // The descriptor is inline: keys 0..=8 are the target itself, 9 its generation, and
            // 10 the reason mask. A nested producer validates that shape exactly, so a wrapped
            // descriptor leaves it stuck on the generation it read at WELCOME and every scene
            // commit it makes afterwards is rejected as stale.
            let mut payload = target_descriptor(metrics);
            payload.push((9, Value::Unsigned(generation)));
            payload.push((10, Value::Unsigned(0x1f)));
            let body = Envelope::new(0, payload).encode();
            if let Ok(body) = body {
                let _ = session
                    .writer
                    .write_record(messages::TARGET_CHANGED, 0, &body);
            }
        }
    }

    pub fn notify_capabilities_changed(&self, reason_mask: u64) -> io::Result<u64> {
        let state = lock(&self.state);
        let body = Envelope::new(
            0,
            vec![(0, Value::Unsigned(1)), (1, Value::Unsigned(reason_mask))],
        )
        .encode()?;
        for session in state.sessions.values().filter(|session| !session.closed) {
            let _ = session
                .writer
                .write_record(messages::CAPS_CHANGED, 0, &body);
        }
        Ok(1)
    }

    pub fn observe_marker(&self, pane: PaneId, value: &str, row: i32, column: usize) {
        let marker = anchor::parse_marker(value).or_else(|_| anchor::parse_conpty_marker(value));
        let Ok(marker) = marker else {
            return;
        };
        let mut state = lock(&self.state);
        let Some(session) = state.sessions.values_mut().find(|session| {
            !session.closed
                && session.pane == pane
                && session.session_tag == marker.session_tag
                && anchor::verify_marker(&session.anchor_key, &marker)
        }) else {
            return;
        };
        let key = (marker.context_id, marker.anchor_id);
        if session.seen_anchors.len() >= MAX_ACTIVE_ANCHORS && !session.seen_anchors.contains(&key)
        {
            return;
        }
        session.seen_anchors.insert(key);
        session.anchors.insert(key, (row, column));
        let body = Envelope::new(
            0,
            vec![
                (0, Value::Unsigned(marker.context_id)),
                (1, Value::Unsigned(marker.anchor_id)),
                (
                    2,
                    Value::Unsigned(u64::try_from(column).unwrap_or(u64::MAX)),
                ),
                (3, nonnegative(row)),
                (4, Value::Bool(true)),
                (5, Value::Unsigned(session.target_generation.get())),
            ],
        )
        .encode();
        if let Ok(body) = body {
            let _ = session
                .writer
                .write_record(messages::ANCHOR_READY, marker.anchor_id, &body);
        }
        advance_projection(&mut state);
    }

    pub fn scroll_anchors(&self, pane: PaneId, lines: i32) {
        let mut state = lock(&self.state);
        for session in state
            .sessions
            .values_mut()
            .filter(|session| session.pane == pane)
        {
            for (line, _) in session.anchors.values_mut() {
                *line = line.saturating_sub(lines);
            }
        }
        advance_projection(&mut state);
    }

    pub fn clear_anchors(&self, pane: PaneId) {
        let mut state = lock(&self.state);
        for session in state
            .sessions
            .values_mut()
            .filter(|session| session.pane == pane)
        {
            let anchors = std::mem::take(&mut session.anchors);
            for ((context, anchor), _) in anchors {
                if let Ok(body) = Envelope::new(
                    0,
                    vec![
                        (0, Value::Unsigned(context)),
                        (1, Value::Unsigned(anchor)),
                        (2, Value::Unsigned(1)),
                    ],
                )
                .encode()
                {
                    let _ = session
                        .writer
                        .write_record(messages::ANCHOR_GONE, anchor, &body);
                }
            }
        }
        advance_projection(&mut state);
    }

    pub fn pane_for_source(&self, source: SourceKey) -> Option<PaneId> {
        let state = lock(&self.state);
        state
            .sessions
            .get(&source.producer)
            .map(|session| session.pane)
    }

    pub fn revision(&self) -> u64 {
        lock(&self.state).projection_revision
    }

    #[allow(dead_code)]
    pub fn projection_snapshot(&self, panes: &HashSet<PaneId>) -> ProjectionSnapshot {
        self.projection_snapshot_with_viewports(panes, &HashMap::new())
    }

    pub fn projection_snapshot_with_viewports(
        &self,
        panes: &HashSet<PaneId>,
        viewport_offsets: &HashMap<PaneId, usize>,
    ) -> ProjectionSnapshot {
        let mut state = lock(&self.state);
        let sessions = state
            .sessions
            .iter()
            .filter_map(|(id, session)| panes.contains(&session.pane).then_some(*id))
            .collect::<HashSet<_>>();
        let surfaces = state
            .surfaces
            .iter()
            .filter(|(key, _)| sessions.contains(&key.session))
            .map(|(key, surface)| SnapshotSurface {
                producer: key.session,
                context: key.context,
                surface: key.surface,
                logical_width: surface.state.definition.logical_width,
                logical_height: surface.state.definition.logical_height,
                capture_policy: surface.state.definition.policy,
                semantic_descriptor: semantic_descriptor(&surface.state.definition.descriptor),
            })
            .collect::<Vec<_>>();
        let sources = state
            .tracks
            .iter()
            .filter(|(key, _)| sessions.contains(&key.surface.session))
            .map(|(key, track)| SnapshotSource {
                key: bridge_track_key(*key),
                descriptor: source_descriptor(&state.tracks, *key, track),
                retained: track.retained.clone(),
                first_visible_presented: track.outer_presented,
                playing: track.playing,
                play_request: track.play_request,
                eos_epoch: track.eos_epoch,
                last_inner_record_sequence: track.last_record_sequence,
                causation_id: track.causation_id,
                capture_policy: state
                    .surfaces
                    .get(&key.surface)
                    .map_or(0, |surface| surface.state.definition.policy),
                semantic_descriptor: state
                    .surfaces
                    .get(&key.surface)
                    .map(|surface| semantic_descriptor(&surface.state.definition.descriptor)),
                raster_delta_operation_limit: match &track.configuration.kind {
                    KindConfiguration::Raster(config) if config.delta_enabled => {
                        Some(u32::from(config.maximum_delta_operations))
                    }
                    _ => None,
                },
            })
            .collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for (key, entry) in &state.nodes {
            if !sessions.contains(&key.session) {
                continue;
            }
            let surface = SurfaceKey {
                session: key.session,
                context: entry.node.surface_context_id,
                surface: entry.node.surface_id,
            };
            let Some(track_key) = selected_visual_track(&state, surface) else {
                continue;
            };
            let Some(session) = state.sessions.get(&key.session) else {
                continue;
            };
            let Some(config) = projected_node_config(
                &entry.node,
                bridge_track_key(track_key),
                session,
                *viewport_offsets.get(&entry.pane).unwrap_or(&0),
            ) else {
                continue;
            };
            nodes.push(SceneNode {
                producer: key.session,
                pane: entry.pane,
                config,
            });
        }
        let live_nodes = state
            .nodes
            .keys()
            .map(|key| (key.session, key.node))
            .collect::<Vec<_>>();
        let videos_needing_keyframes = state
            .tracks
            .iter()
            .filter_map(|(key, track)| {
                (sessions.contains(&key.surface.session)
                    && track.recovery_pending
                    && matches!(track.configuration.kind, KindConfiguration::Video(_)))
                .then_some(bridge_track_key(*key))
            })
            .collect::<Vec<_>>();
        state.projected_sources = sources.iter().map(|source| source.key).collect();
        ProjectionSnapshot {
            revision: state.projection_revision,
            surfaces,
            sources,
            nodes,
            live_nodes,
            videos_needing_keyframes,
        }
    }

    pub fn deactivate_bridge(&self) {
        let mut state = lock(&self.state);
        state.projected_sources.clear();
        for track in state.tracks.values_mut() {
            if matches!(track.configuration.kind, KindConfiguration::Video(_)) && track.playing {
                track.recovery_pending = true;
            }
        }
        let deliveries = std::mem::take(&mut state.deliveries);
        drop(state);
        if !deliveries.is_empty() {
            self.delivery_changed.notify_all();
        }
    }

    pub fn complete_bridge_delivery(&self, delivery_id: u64, delivered: bool) -> bool {
        let mut state = lock(&self.state);
        let Some(delivery) = state.deliveries.remove(&delivery_id) else {
            return false;
        };
        let mut resync = !delivered;
        if let Some(track) = state.tracks.get_mut(&delivery.track) {
            if delivered {
                track.outer_presented = true;
                track.state.milestones |= MILESTONE_PRESENTED;
                let maxima_bytes = track
                    .state
                    .flow
                    .maximum_body_bytes
                    .saturating_add(delivery.bytes);
                let maxima_records = track.state.flow.maximum_media_records.saturating_add(1);
                track.state.flow.raise_maxima(maxima_bytes, maxima_records);
                send_flow_update(delivery.track, track);
            } else {
                track.recovery_pending = true;
                resync = true;
            }
        }
        drop(state);
        self.delivery_changed.notify_all();
        resync
    }

    pub fn complete_retained_hydration(&self, source: SourceKey) {
        let mut state = lock(&self.state);
        if let Some(track) = state.tracks.get_mut(&inner_track_key(source)) {
            track.outer_presented = true;
            track.state.milestones |= MILESTONE_PRESENTED;
        }
    }

    pub fn request_keyframe(
        &self,
        source: SourceKey,
        minimum_epoch: Option<u32>,
        reason: u64,
    ) -> KeyframeRequestOutcome {
        let mut state = lock(&self.state);
        let key = inner_track_key(source);
        let Some(track) = state.tracks.get_mut(&key) else {
            return KeyframeRequestOutcome::Ignored;
        };
        if !matches!(track.configuration.kind, KindConfiguration::Video(_)) {
            return KeyframeRequestOutcome::Ignored;
        }
        if track.recovery_pending {
            return KeyframeRequestOutcome::Damped;
        }
        track.recovery_pending = true;
        send_need_keyframe(
            key,
            track,
            minimum_epoch.unwrap_or(track.state.media_epoch),
            reason,
        );
        KeyframeRequestOutcome::Forwarded
    }

    pub fn request_full_frames(&self, sources: &[SourceKey], _reason: u64) {
        let mut state = lock(&self.state);
        for source in sources {
            let key = inner_track_key(*source);
            let Some(track) = state.tracks.get_mut(&key) else {
                continue;
            };
            if matches!(track.configuration.kind, KindConfiguration::Raster(_)) {
                track.recovery_pending = true;
                send_need_full_frame(key, track);
            }
        }
    }

    pub fn apply_outer_playback(&self, source: SourceKey, state_value: u64, eos_state: u64) {
        let mut state = lock(&self.state);
        let key = inner_track_key(source);
        if let Some(track) = state.tracks.get_mut(&key) {
            if state_value >= 2 {
                track.state.milestones |= MILESTONE_CLOCK_STARTED;
            }
            if eos_state >= 1 {
                track.state.milestones |= MILESTONE_EOS_ACCEPTED;
            }
            if eos_state >= 2 {
                track.state.milestones |= MILESTONE_BUFFERED_ENDED;
            }
        }
    }

    pub fn pane_status(
        &self,
        pane: PaneId,
        outer: OuterMediaProjection<'_>,
        relay: crate::metrics::RelayMetrics,
    ) -> PaneMediaStatus {
        let state = lock(&self.state);
        let sessions = state
            .sessions
            .iter()
            .filter_map(|(id, session)| (session.pane == pane).then_some(*id))
            .collect::<HashSet<_>>();
        let mut surfaces = state
            .surfaces
            .iter()
            .filter(|(key, _)| sessions.contains(&key.session))
            .map(|(key, surface)| {
                let descriptor = &surface.state.definition.descriptor;
                let mut active_slots = surface
                    .active_slots
                    .iter()
                    .map(|(slot, track)| (*slot, *track))
                    .collect::<Vec<_>>();
                active_slots.sort_unstable();
                PaneMediaSurfaceStatus {
                    producer_id: key.session,
                    context_id: key.context,
                    surface_id: key.surface,
                    lifecycle: "live".into(),
                    surface_revision: surface.state.revision.get(),
                    surface_generation: surface.state.generation.get(),
                    visible: state.projected_sources.iter().any(|track| {
                        track.producer == key.session
                            && track.context == key.context
                            && track.surface == key.surface
                    }),
                    capture_policy: surface.state.definition.policy,
                    descriptor: Some(crate::ipc::PaneMediaSurfaceDescriptor {
                        role: descriptor.role as u64,
                        title: Some(descriptor.title.clone()),
                        content_revision: Some(descriptor.semantic_content_revision),
                        semantic_availability: Some(descriptor.semantic_availability),
                        locator: Some(descriptor.locator_hint.clone()),
                    }),
                    active_slots,
                }
            })
            .collect::<Vec<_>>();
        surfaces
            .sort_by_key(|surface| (surface.producer_id, surface.context_id, surface.surface_id));
        let mut tracks = state
            .tracks
            .iter()
            .filter(|(key, _)| sessions.contains(&key.surface.session))
            .map(|(key, track)| {
                let source = bridge_track_key(*key);
                PaneMediaTrackStatus {
                    producer_id: source.producer,
                    context_id: source.context,
                    surface_id: source.surface,
                    track_id: source.track,
                    kind: kind_name(&track.configuration.kind).into(),
                    lifecycle: if track.state.lost {
                        "lost"
                    } else if track.eos_epoch.is_some() {
                        "ended"
                    } else if track.playing {
                        "playing"
                    } else {
                        "live"
                    }
                    .into(),
                    track_revision: track.state.revision.get(),
                    epoch: track.state.media_epoch,
                    channel_state: if track.channel_writer.is_some() { 1 } else { 0 },
                    inner_channel_generation: track.state.channel_generation.get(),
                    outer_channel_generation: outer.attachment_generations.get(&source).copied(),
                    outer_mapping_fresh: outer.bridge_instance_id.is_some(),
                    visible: state.projected_sources.contains(&source),
                    retained_static: track.retained.is_some(),
                    keyframe_needed: track.recovery_pending,
                    milestones: track.state.milestones,
                    queued_packets: state
                        .deliveries
                        .values()
                        .filter(|delivery| delivery.track == *key)
                        .count() as u64,
                    queued_bytes: state
                        .deliveries
                        .values()
                        .filter(|delivery| delivery.track == *key)
                        .map(|delivery| delivery.bytes)
                        .sum(),
                    available_packet_credit: track
                        .state
                        .flow
                        .maximum_media_records
                        .saturating_sub(track.state.flow.sent_media_records),
                    available_byte_credit: track
                        .state
                        .flow
                        .maximum_body_bytes
                        .saturating_sub(track.state.flow.sent_body_bytes),
                }
            })
            .collect::<Vec<_>>();
        tracks.sort_by_key(|track| {
            (
                track.producer_id,
                track.context_id,
                track.surface_id,
                track.track_id,
            )
        });
        let mut nodes = state
            .nodes
            .iter()
            .filter(|(key, entry)| sessions.contains(&key.session) && entry.pane == pane)
            .filter_map(|(key, entry)| {
                let surface = SurfaceKey {
                    session: key.session,
                    context: entry.node.surface_context_id,
                    surface: entry.node.surface_id,
                };
                let track = selected_visual_track(&state, surface)?;
                let config = projected_node_config(
                    &entry.node,
                    bridge_track_key(track),
                    state.sessions.get(&key.session)?,
                    0,
                )?;
                Some(PaneMediaNodeStatus {
                    producer_id: key.session,
                    context_id: entry.node.owning_context_id,
                    node_id: key.node,
                    surface_context_id: entry.node.surface_context_id,
                    surface_id: entry.node.surface_id,
                    visible: config.node.visible,
                    x: config.node.x,
                    y: config.node.y,
                    width: config.node.width,
                    height: config.node.height,
                })
            })
            .collect::<Vec<_>>();
        nodes.sort_by_key(|node| (node.producer_id, node.node_id));
        let virtual_scene_revision = state
            .sessions
            .iter()
            .filter(|(_, session)| session.pane == pane)
            .map(|(_, session)| session.scene_revision.get())
            .max()
            .unwrap_or(0);
        PaneMediaStatus {
            virtual_projection_revision: state.projection_revision,
            virtual_scene_revision,
            outer_projection_revision: outer.compatibility_revision,
            outer_apply_sequence: outer.apply_sequence,
            bridge_instance_id: outer.bridge_instance_id,
            bridge_local_revision: outer.bridge_local_revision,
            surfaces,
            tracks,
            nodes,
            relay: crate::metrics::RelayMetrics {
                delivery: state.delivery_metrics,
                ..relay
            },
        }
    }

    #[cfg(test)]
    pub fn wait_for_retained_media(&self, pane: PaneId, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.state);
        loop {
            if state.tracks.iter().any(|(key, track)| {
                track.retained.is_some()
                    && state
                        .sessions
                        .get(&key.surface.session)
                        .is_some_and(|session| session.pane == pane)
            }) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timed) = self
                .delivery_changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if timed.timed_out() {
                return false;
            }
        }
    }
}

impl Drop for VirtualVivid {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn accept_loop(
    listener: VirtualPresenterListener,
    state: Arc<Mutex<State>>,
    changed: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok(stream) => {
                {
                    let mut state = lock(&state);
                    if state.connections >= MAX_CONNECTIONS {
                        continue;
                    }
                    state.connections += 1;
                }
                let state_clone = state.clone();
                let changed_clone = changed.clone();
                let _ = thread::Builder::new()
                    .name("vvmux-vivid-1.5-connection".into())
                    .spawn(move || {
                        if let Err(error) = handle_connection(stream, &state_clone, &changed_clone)
                        {
                            log::debug!("inner Vivid connection closed: {error}");
                        }
                        let mut state = lock(&state_clone);
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
    stream: crate::platform::Transport,
    state: &Arc<Mutex<State>>,
    changed: &Arc<Condvar>,
) -> io::Result<()> {
    stream
        .set_read_deadline(Duration::from_secs(3))
        .map_err(|error| with_context(error, "setting handshake deadline"))?;
    let (mut reader, preface, preface_bytes) =
        Reader::new(stream).map_err(|error| with_context(error, "reading Vivid preface"))?;
    match preface.kind {
        ConnectionKind::Control => handle_control(&mut reader, &preface_bytes, state),
        ConnectionKind::Track => handle_track(&mut reader, state, changed),
        ConnectionKind::Lane => {
            let writer = reader.writer();
            let first = reader.read_record(ConnectionKind::Lane)?;
            let request = messages::decode_control(&first.body)
                .map(|envelope| envelope.request_id)
                .unwrap_or(0);
            writer.write_record(
                messages::ERROR,
                first.object_id,
                &protocol_error(
                    request,
                    messages::ERROR_UNSUPPORTED_PROFILE,
                    true,
                    "vvmux does not support Vivid lane connections",
                )?,
            )?;
            Ok(())
        }
    }
}

fn handle_control(
    reader: &mut Reader,
    preface: &[u8; 16],
    shared: &Arc<Mutex<State>>,
) -> io::Result<()> {
    let writer = reader.writer();
    let first = reader
        .read_record(ConnectionKind::Control)
        .map_err(|error| with_context(error, "reading HELLO"))?;
    let (request_id, hello) = Hello::decode(&first.body)
        .map_err(|error| io::Error::other(format!("decoding HELLO: {error}")))?;
    let (session_id, maximum) =
        establish_session(shared, writer.clone(), preface, &hello, request_id)
            .map_err(|error| with_context(error, "establishing root session"))?;
    reader
        .set_maximum(maximum)
        .map_err(|error| with_context(error, "setting control receive maximum"))?;
    writer
        .set_maximum(maximum)
        .map_err(|error| with_context(error, "setting control send maximum"))?;
    reader
        .clear_read_deadline()
        .map_err(|error| with_context(error, "clearing handshake deadline"))?;
    let mut clean = false;
    loop {
        let record = match reader.read_record(ConnectionKind::Control) {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        match dispatch_control(shared, session_id, &record) {
            Ok(Some((record_type, object_id, body))) => {
                writer.write_record(record_type, object_id, &body)?;
            }
            Ok(None) => {}
            Err(error) => {
                let request = messages::decode_control(&record.body)
                    .map(|envelope| envelope.request_id)
                    .unwrap_or(0);
                let fatal = request == 0;
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &protocol_error(request, error.code, fatal, error.message)?,
                )?;
                if fatal {
                    break;
                }
            }
        }
        if record.record_type == messages::GOODBYE {
            clean = true;
            break;
        }
    }
    let mut state = lock(shared);
    if clean {
        detach_session(&mut state, session_id);
        advance_projection(&mut state);
    } else {
        cleanup_session(&mut state, session_id);
    }
    Ok(())
}

fn establish_session(
    shared: &Arc<Mutex<State>>,
    writer: Arc<Writer>,
    preface: &[u8; 16],
    hello: &Hello,
    request_id: u64,
) -> io::Result<(u64, u32)> {
    let proof = match &hello.authentication {
        HelloAuthentication::Root { proof } => proof,
        _ => {
            return Err(send_fatal(
                &writer,
                request_id,
                messages::ERROR_AUTH_FAILED,
                "vvmux accepts root authentication only",
            ));
        }
    };
    let authless = hello.authless_payload()?;
    let mut state = lock(shared);
    let matches = state
        .capabilities
        .iter()
        .filter_map(|(pane, secret)| {
            auth::verify_root_hello_proof(secret, preface, &authless, proof).then_some(*pane)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        writer.write_record(
            messages::ERROR,
            0,
            &protocol_error(
                request_id,
                messages::ERROR_AUTH_FAILED,
                true,
                "root authentication failed",
            )?,
        )?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "root authentication failed",
        ));
    }
    let pane = matches[0];
    let secret = state
        .capabilities
        .get(&pane)
        .map(|secret| Secret32::new(*secret.expose()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "pane was revoked"))?;
    if hello.target_profile != registry::TERMINAL_SURFACE {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_UNSUPPORTED_PROFILE,
            "vvmux is a terminal-surface-v1 target",
        ));
    }
    let supported = [
        registry::CORE_CONTROL,
        registry::LIVE_MEDIA,
        registry::OBSERVABILITY,
        registry::TERMINAL_SURFACE,
        registry::TIMED_MEDIA,
    ];
    if hello
        .required_profiles
        .iter()
        .any(|profile| !supported.contains(&profile.as_str()))
    {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_UNSUPPORTED_PROFILE,
            "required Vivid profile is unsupported",
        ));
    }
    let Some(metrics) = state.metrics.get(&pane).copied() else {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_BAD_STATE,
            "pane target metrics are not ready",
        ));
    };
    if state
        .sessions
        .values()
        .filter(|session| !session.closed)
        .count()
        >= MAX_SESSIONS
    {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_LIMIT_EXCEEDED,
            "inner session capacity is exhausted",
        ));
    }
    let mut accepted = hello.required_profiles.clone();
    accepted.extend(
        hello
            .optional_profiles
            .iter()
            .filter(|profile| supported.contains(&profile.as_str()))
            .cloned(),
    );
    accepted.sort();
    accepted.dedup();
    registry::validate_profile_set(accepted.iter().map(String::as_str))
        .map_err(io::Error::other)?;
    state.next_session = state
        .next_session
        .checked_add(1)
        .ok_or_else(|| io::Error::other("inner session ID exhausted"))?;
    let session_id = state.next_session;
    let identity = SessionIdentity::new(state.presenter, session_id).map_err(io::Error::other)?;
    let root_context = identity.context(1).map_err(io::Error::other)?.context_id;
    let mut server_nonce = [0_u8; auth::NONCE_BYTES];
    let mut session_tag = [0_u8; messages::SESSION_TAG_BYTES];
    getrandom::fill(&mut server_nonce).map_err(io::Error::other)?;
    getrandom::fill(&mut session_tag).map_err(io::Error::other)?;
    let prk = auth::extract_handshake_prk(&secret, &hello.client_nonce, &server_nonce, &[0; 32]);
    let (keys, anchor_key) = auth::derive_session_keys(&prk, session_id, 0, &session_tag);
    let maximum = hello
        .maximum_control_body
        .min(vivid_protocol::CONTROL_MAX_RECORD_BODY);
    let mut welcome = Welcome {
        session_id,
        session_tag,
        root_context_id: root_context,
        target_generation: metrics.generation,
        target_profile: registry::TERMINAL_SURFACE.into(),
        target_descriptor: target_descriptor(metrics),
        accepted_profiles: accepted,
        maximum_control_body: maximum,
        server_nonce,
        authentication: WelcomeAuthentication {
            kind: messages::AUTHENTICATION_ROOT,
            confirmation: [0; 32],
            lease_state: 0,
            activation_attempt_status: 0,
        },
        session_revision: 1,
        scene_revision: 0,
        resource_contract: presenter_contract(&state.config),
        establishment_state: 0,
        resume_generation: 0,
        extensions: vec![],
    };
    welcome.confirm(&prk)?;
    writer.write_record(messages::WELCOME, 0, &welcome.encode(request_id)?)?;
    state.sessions.insert(
        session_id,
        SessionRuntime {
            pane,
            closed: false,
            session_tag,
            channel_key: Secret32::new(*keys.channel_key()),
            anchor_key,
            writer,
            root_context,
            scene_revision: SceneRevision::ZERO,
            target_generation: TargetGeneration::new(metrics.generation),
            anchors: HashMap::new(),
            seen_anchors: HashSet::new(),
            cancelled_waits: HashSet::new(),
            pending_waits: 0,
        },
    );
    Ok((session_id, maximum))
}

#[derive(Debug)]
struct ControlError {
    code: u64,
    message: &'static str,
}

impl ControlError {
    fn bad(message: &'static str) -> Self {
        Self {
            code: messages::ERROR_BAD_MESSAGE,
            message,
        }
    }
    fn state(message: &'static str) -> Self {
        Self {
            code: messages::ERROR_BAD_STATE,
            message,
        }
    }
    fn missing(message: &'static str) -> Self {
        Self {
            code: messages::ERROR_NOT_FOUND,
            message,
        }
    }
}

type ControlReply = Option<(u16, u64, Vec<u8>)>;

fn is_idempotent_mutation(record_type: u16) -> bool {
    matches!(
        record_type,
        messages::SET_OBSERVATION
            | messages::CREATE_SURFACE
            | messages::UPDATE_SURFACE
            | messages::DESTROY_SURFACE
            | messages::CREATE_TRACK
            | messages::DESTROY_TRACK
            | messages::ADVANCE_CHANNEL
            | messages::ACTIVATE_TRACK
            | messages::BEGIN_TXN
            | messages::CREATE_NODE
            | messages::UPDATE_NODE
            | messages::DELETE_NODE
            | messages::ABORT_TXN
            | messages::COMMIT_TXN
            | messages::CANCEL_WAIT
            | messages::PLAY
            | messages::PAUSE
            | messages::FLUSH
            | messages::DRAIN
    )
}

fn mutation_fingerprint(record_type: u16, object_id: u64, envelope: &Envelope) -> [u8; 32] {
    let mut canonical = envelope.clone();
    canonical.request_id = 1;
    let mut digest = Sha256::new();
    digest.update(record_type.to_be_bytes());
    digest.update(object_id.to_be_bytes());
    if let Ok(encoded) = canonical.encode() {
        digest.update(encoded);
    }
    digest.finalize().into()
}

fn recorrelate_cached_reply(body: &[u8], request_id: u64) -> io::Result<Vec<u8>> {
    let mut envelope = messages::decode_control(body)?;
    envelope.request_id = request_id;
    envelope.encode().map_err(io::Error::other)
}

fn dispatch_control(
    shared: &Arc<Mutex<State>>,
    session_id: u64,
    record: &Record,
) -> Result<ControlReply, ControlError> {
    if record.flags & !RECORD_OPTIONAL != 0 {
        return Err(ControlError::bad("unknown control record flags"));
    }
    let envelope = messages::decode_control(&record.body)
        .map_err(|_| ControlError::bad("invalid strict control envelope"))?;
    envelope
        .validate_request()
        .map_err(|_| ControlError::bad("request ID must be nonzero"))?;
    let request_id = envelope.request_id;
    let value = Value::Map(envelope.payload.clone());
    let mut state = lock(shared);
    if !state.sessions.contains_key(&session_id) {
        return Err(ControlError::missing("session does not exist"));
    }
    let mutation_cache = if is_idempotent_mutation(record.record_type) {
        envelope.idempotency_key.map(|key| {
            (
                key,
                mutation_fingerprint(record.record_type, record.object_id, &envelope),
            )
        })
    } else {
        None
    };
    if let Some((key, fingerprint)) = mutation_cache
        && let Some(cached) = state.idempotency.get(&(session_id, key))
    {
        if cached.fingerprint != fingerprint {
            return Err(ControlError::bad(
                "idempotency key was reused with different mutation bytes",
            ));
        }
        let body = recorrelate_cached_reply(&cached.body, request_id)
            .map_err(|_| ControlError::bad("cached mutation reply is invalid"))?;
        return Ok(Some((cached.record_type, cached.object_id, body)));
    }
    let reply = match record.record_type {
        messages::PING => (
            messages::PONG,
            0,
            Envelope::new(request_id, envelope.payload).encode(),
        ),
        messages::GOODBYE => (messages::OK, 0, Ok(messages::ok(request_id))),
        messages::SET_OBSERVATION => (messages::OK, 0, Ok(messages::ok(request_id))),
        messages::CREATE_SURFACE => {
            let definition = SurfaceDefinition::decode_create(record.object_id, &value)
                .map_err(|_| ControlError::bad("invalid surface definition"))?;
            require_root_context(&state, session_id, definition.context_id)?;
            let key = SurfaceKey {
                session: session_id,
                context: definition.context_id,
                surface: definition.surface_id,
            };
            if state.surfaces.contains_key(&key) {
                return Err(ControlError::state("surface identity is already live"));
            }
            let surface =
                SurfaceState::new(definition).map_err(|_| ControlError::bad("invalid surface"))?;
            let payload = surface_ready_payload(key, &surface);
            state.surfaces.insert(
                key,
                SurfaceEntry {
                    state: surface,
                    active_slots: HashMap::new(),
                },
            );
            advance_projection(&mut state);
            (
                messages::SURFACE_READY,
                record.object_id,
                Envelope::new(request_id, payload).encode(),
            )
        }
        messages::UPDATE_SURFACE => {
            let map = StrictMap::new(
                "UPDATE_SURFACE",
                &value,
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            )
            .map_err(|_| ControlError::bad("invalid surface update"))?;
            let key = surface_key_from_map(session_id, &map)?;
            let current = state
                .surfaces
                .get_mut(&key)
                .ok_or_else(|| ControlError::missing("surface does not exist"))?;
            let replacement = SurfaceDefinition {
                context_id: key.context,
                surface_id: key.surface,
                semantic_profile: current.state.definition.semantic_profile.clone(),
                coordinate_model: current.state.definition.coordinate_model,
                logical_width: required_u64(&map, 4)?,
                logical_height: required_u64(&map, 5)?,
                scale_numerator: required_u64(&map, 6)?,
                scale_denominator: required_u64(&map, 7)?,
                rotation: u16::try_from(required_u64(&map, 8)?)
                    .map_err(|_| ControlError::bad("invalid rotation"))?,
                descriptor: SurfaceDescriptor::from_value(
                    map.required(9)
                        .map_err(|_| ControlError::bad("missing descriptor"))?,
                )
                .map_err(|_| ControlError::bad("invalid descriptor"))?,
                policy: required_u64(&map, 10)?,
                profile_parameters: map
                    .required_map(11)
                    .map_err(|_| ControlError::bad("invalid profile parameters"))?
                    .to_vec(),
            };
            current
                .state
                .replace_mutable(
                    SurfaceRevision::new(required_u64(&map, 2)?),
                    SurfaceGeneration::new(required_u64(&map, 3)?),
                    replacement,
                )
                .map_err(|_| ControlError::state("stale surface revision or generation"))?;
            advance_projection(&mut state);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::DESTROY_SURFACE => {
            let map = StrictMap::new("surface identity", &value, &[0, 1])
                .map_err(|_| ControlError::bad("invalid surface identity"))?;
            let key = surface_key_from_map(session_id, &map)?;
            if state.surfaces.remove(&key).is_none() {
                return Err(ControlError::missing("surface does not exist"));
            }
            remove_surface_children(&mut state, key);
            advance_projection(&mut state);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::QUERY_SURFACE => {
            let map = StrictMap::new("surface identity", &value, &[0, 1])
                .map_err(|_| ControlError::bad("invalid surface identity"))?;
            let key = surface_key_from_map(session_id, &map)?;
            let surface = state
                .surfaces
                .get(&key)
                .ok_or_else(|| ControlError::missing("surface does not exist"))?;
            (
                messages::SURFACE_STATUS,
                record.object_id,
                Envelope::new(request_id, surface_status_payload(key, surface)).encode(),
            )
        }
        messages::PROBE_TRACK_CONFIG => {
            let configuration = TrackConfiguration::decode(0, &value, true)
                .map_err(|_| ControlError::bad("invalid track probe"))?;
            let supported = supports_track(&configuration);
            (
                messages::TRACK_SUPPORT,
                0,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Bool(supported)),
                        (
                            1,
                            Value::Text(if supported {
                                "vvmux-relay".into()
                            } else {
                                "unsupported".into()
                            }),
                        ),
                        (2, Value::Unsigned(1)),
                        (
                            3,
                            Value::Map(configuration.payload(true).unwrap_or_default()),
                        ),
                    ],
                )
                .encode(),
            )
        }
        messages::CREATE_TRACK => {
            let configuration = TrackConfiguration::decode(record.object_id, &value, false)
                .map_err(|_| ControlError::bad("invalid track configuration"))?;
            let surface = SurfaceKey {
                session: session_id,
                context: configuration.context_id,
                surface: configuration.surface_id,
            };
            if !state.surfaces.contains_key(&surface) {
                return Err(ControlError::missing("owning surface does not exist"));
            }
            if !supports_track(&configuration) {
                return Err(ControlError {
                    code: messages::ERROR_UNSUPPORTED_CONFIG,
                    message: "track configuration is unsupported",
                });
            }
            let key = TrackKey {
                surface,
                track: configuration.track_id,
            };
            if state.tracks.contains_key(&key) {
                return Err(ControlError::state("track identity is already live"));
            }
            if state.tracks.len() >= state.config.max_sources {
                return Err(ControlError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "track capacity is exhausted",
                });
            }
            let track_state = TrackState::new();
            let payload = track_ready_payload(key, &configuration, &track_state);
            state.tracks.insert(
                key,
                TrackEntry {
                    configuration,
                    state: track_state,
                    channel_writer: None,
                    retained: None,
                    playing: false,
                    play_request: PlayRequest::baseline(),
                    eos_epoch: None,
                    last_record_sequence: 0,
                    last_pts_us: 0,
                    outer_presented: false,
                    recovery_pending: true,
                    causation_id: envelope.causation_id,
                },
            );
            advance_projection(&mut state);
            (
                messages::TRACK_READY,
                record.object_id,
                Envelope::new(request_id, payload).encode(),
            )
        }
        messages::DESTROY_TRACK => {
            let key = track_key_from_value(session_id, &value)?;
            remove_track(&mut state, key)?;
            advance_projection(&mut state);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::QUERY_TRACK => {
            let key = track_key_from_value(session_id, &value)?;
            let track = state
                .tracks
                .get(&key)
                .ok_or_else(|| ControlError::missing("track does not exist"))?;
            (
                messages::TRACK_STATUS,
                record.object_id,
                Envelope::new(request_id, track_status_payload(key, track)).encode(),
            )
        }
        messages::ADVANCE_CHANNEL => {
            let map = StrictMap::new("ADVANCE_CHANNEL", &value, &[0, 1, 2, 3, 4, 5])
                .map_err(|_| ControlError::bad("invalid channel advance"))?;
            let key = track_key_from_map(session_id, &map)?;
            let track = state
                .tracks
                .get_mut(&key)
                .ok_or_else(|| ControlError::missing("track does not exist"))?;
            track
                .state
                .advance_channel(
                    ChannelGeneration::new(required_u64(&map, 3)?),
                    ChannelGeneration::new(required_u64(&map, 4)?),
                )
                .map_err(|_| ControlError::state("channel advance is stale"))?;
            track.channel_writer = None;
            track.recovery_pending = true;
            (
                messages::CHANNEL_ADVANCED,
                record.object_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(key.surface.context)),
                        (1, Value::Unsigned(key.surface.surface)),
                        (2, Value::Unsigned(key.track)),
                        (3, Value::Unsigned(track.state.channel_generation.get())),
                        (4, Value::Unsigned(CHANNEL_OPEN_DEADLINE_US)),
                        (5, Value::Unsigned(track.state.revision.get())),
                    ],
                )
                .encode(),
            )
        }
        messages::ACTIVATE_TRACK => {
            let map = StrictMap::new("ACTIVATE_TRACK", &value, &[0, 1, 2, 3])
                .map_err(|_| ControlError::bad("invalid activation"))?;
            let surface_key = surface_key_from_map(session_id, &map)?;
            let expected_revision = SurfaceRevision::new(required_u64(&map, 3)?);
            let bindings = map
                .required(2)
                .map_err(|_| ControlError::bad("missing bindings"))?
                .as_array()
                .ok_or_else(|| ControlError::bad("bindings are not an array"))?;
            let mut active = HashMap::new();
            for value in bindings {
                let binding = StrictMap::new("slot binding", value, &[0, 1, 2, 3])
                    .map_err(|_| ControlError::bad("invalid slot binding"))?;
                let slot = required_u64(&binding, 0)?;
                let track_id = required_u64(&binding, 1)?;
                if track_id == 0 {
                    continue;
                }
                let track = state
                    .tracks
                    .get(&TrackKey {
                        surface: surface_key,
                        track: track_id,
                    })
                    .ok_or_else(|| ControlError::missing("activation track is absent"))?;
                if track.configuration.slot != slot
                    || track.state.channel_generation.get() != required_u64(&binding, 2)?
                    || track.state.milestones & required_u64(&binding, 3)? == 0
                {
                    return Err(ControlError::state(
                        "activation generation or milestone is not ready",
                    ));
                }
                active.insert(slot, track_id);
            }
            let surface = state
                .surfaces
                .get_mut(&surface_key)
                .ok_or_else(|| ControlError::missing("surface does not exist"))?;
            if surface.state.revision != expected_revision {
                return Err(ControlError::state("surface activation revision is stale"));
            }
            surface.state.revision = surface
                .state
                .revision
                .advance()
                .map_err(|_| ControlError::state("surface revision exhausted"))?;
            surface.active_slots = active.clone();
            let surface_revision = surface.state.revision;
            let mut active_payload = active
                .iter()
                .map(|(slot, track)| (*slot, Value::Unsigned(*track)))
                .collect::<Vec<_>>();
            active_payload.sort_by_key(|(slot, _)| *slot);
            advance_projection(&mut state);
            (
                messages::TRACK_ACTIVATED,
                record.object_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(surface_key.context)),
                        (1, Value::Unsigned(surface_key.surface)),
                        (2, Value::Map(active_payload)),
                        (3, Value::Unsigned(surface_revision.get())),
                        (4, Value::Unsigned(surface_revision.get())),
                    ],
                )
                .encode(),
            )
        }
        messages::BEGIN_TXN => {
            let map = StrictMap::new("BEGIN_TXN", &value, &[0, 1])
                .map_err(|_| ControlError::bad("invalid transaction"))?;
            let context = required_u64(&map, 0)?;
            require_root_context(&state, session_id, context)?;
            let transaction = required_u64(&map, 1)?;
            if state
                .transactions
                .insert((session_id, context, transaction), Vec::new())
                .is_some()
            {
                return Err(ControlError::state("transaction is already live"));
            }
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::CREATE_NODE | messages::UPDATE_NODE => {
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| ControlError::bad("node mutation omits transaction"))?;
            let node = ProtocolSceneNode::decode(record.object_id, &value)
                .map_err(|_| ControlError::bad("invalid scene node"))?;
            let key = (session_id, node.owning_context_id, transaction);
            let pending = state
                .transactions
                .get_mut(&key)
                .ok_or_else(|| ControlError::missing("transaction does not exist"))?;
            pending.push(if record.record_type == messages::CREATE_NODE {
                NodeMutation::Create(node)
            } else {
                NodeMutation::Update(node)
            });
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::DELETE_NODE => {
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| ControlError::bad("node deletion omits transaction"))?;
            let map = StrictMap::new("DELETE_NODE", &value, &[0, 1])
                .map_err(|_| ControlError::bad("invalid node deletion"))?;
            let context = required_u64(&map, 0)?;
            let node = required_u64(&map, 1)?;
            let pending = state
                .transactions
                .get_mut(&(session_id, context, transaction))
                .ok_or_else(|| ControlError::missing("transaction does not exist"))?;
            pending.push(NodeMutation::Delete(NodeKey {
                session: session_id,
                context,
                node,
            }));
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::ABORT_TXN => {
            let transaction = envelope.transaction_id.unwrap_or(record.object_id);
            state
                .transactions
                .retain(|(session, _, txn), _| *session != session_id || *txn != transaction);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::COMMIT_TXN => {
            let transaction = envelope.transaction_id.unwrap_or(record.object_id);
            let session = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| ControlError::missing("session does not exist"))?;
            // The two preconditions carry different registered codes and different producer
            // recoveries: a moved target is re-planned against the announcement that caused it,
            // while a failed revision precondition needs the scene re-read. Reporting both as one
            // makes a producer retry a commit that can never succeed.
            if envelope.expected_target_generation != Some(session.target_generation.get()) {
                return Err(ControlError {
                    code: messages::ERROR_STALE_TARGET_GENERATION,
                    message: "scene commit names a stale target generation",
                });
            }
            if envelope
                .preconditions
                .iter()
                .find_map(|(key, value)| (*key == 0).then(|| value.as_u64()).flatten())
                != Some(session.scene_revision.get())
            {
                return Err(ControlError {
                    code: messages::ERROR_PRECONDITION_FAILED,
                    message: "scene commit names a stale scene revision",
                });
            }
            let transaction_key = state
                .transactions
                .keys()
                .find(|(session, _, txn)| *session == session_id && *txn == transaction)
                .copied()
                .ok_or_else(|| ControlError::missing("transaction does not exist"))?;
            let pending = state
                .transactions
                .get(&transaction_key)
                .cloned()
                .unwrap_or_default();
            validate_node_mutations(&state, session_id, &pending)?;
            apply_node_mutations(&mut state, session_id, pending);
            state.transactions.remove(&transaction_key);
            let session = state.sessions.get_mut(&session_id).unwrap();
            session.scene_revision = session
                .scene_revision
                .advance()
                .map_err(|_| ControlError::state("scene revision exhausted"))?;
            let revision = session.scene_revision;
            let target = session.target_generation;
            advance_projection(&mut state);
            (
                messages::SCENE_PRESENTED,
                record.object_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(revision.get())),
                        (1, Value::Unsigned(target.get())),
                    ],
                )
                .encode(),
            )
        }
        messages::QUERY_ANCHOR => {
            let map = StrictMap::new("QUERY_ANCHOR", &value, &[0, 1])
                .map_err(|_| ControlError::bad("invalid anchor query"))?;
            let context = required_u64(&map, 0)?;
            let anchor = required_u64(&map, 1)?;
            let session = state.sessions.get(&session_id).unwrap();
            let position = session.anchors.get(&(context, anchor));
            let mut payload = vec![
                (0, Value::Unsigned(context)),
                (1, Value::Unsigned(anchor)),
                (2, Value::Unsigned(if position.is_some() { 1 } else { 0 })),
            ];
            if let Some((row, column)) = position {
                payload.push((3, Value::Unsigned(*column as u64)));
                payload.push((4, nonnegative(*row)));
                payload.push((5, Value::Bool(true)));
            }
            payload.push((6, Value::Unsigned(session.target_generation.get())));
            (
                messages::ANCHOR_STATUS,
                record.object_id,
                Envelope::new(request_id, payload).encode(),
            )
        }
        messages::WAIT_TRACK => {
            let map = StrictMap::new("WAIT_TRACK", &value, &[0, 1, 2, 3, 4, 5, 6])
                .map_err(|_| ControlError::bad("invalid track wait"))?;
            let key = track_key_from_map(session_id, &map)?;
            let condition = required_u64(&map, 3)?;
            let condition_value = map
                .optional_u64(4)
                .map_err(|_| ControlError::bad("invalid wait value"))?;
            let timeout = required_u64(&map, 5)?;
            let generation = required_u64(&map, 6)?;
            if timeout == 0 || timeout > MAX_WAIT_US {
                return Err(ControlError::bad("invalid wait timeout"));
            }
            let track = state
                .tracks
                .get(&key)
                .ok_or_else(|| ControlError::missing("track does not exist"))?;
            if track.state.channel_generation.get() != generation {
                return Err(ControlError {
                    code: messages::ERROR_STALE_CHANNEL_GENERATION,
                    message: "track wait generation is stale",
                });
            }
            if let Some(observed) = evaluate_wait(track, condition, condition_value) {
                (
                    messages::WAIT_SATISFIED,
                    record.object_id,
                    Envelope::new(request_id, wait_payload(key, track, condition, observed))
                        .encode(),
                )
            } else {
                let session = state.sessions.get_mut(&session_id).unwrap();
                if session.pending_waits >= MAX_WAITS {
                    return Err(ControlError {
                        code: messages::ERROR_LIMIT_EXCEEDED,
                        message: "track wait capacity is exhausted",
                    });
                }
                session.pending_waits += 1;
                let writer = session.writer.clone();
                drop(state);
                spawn_wait(
                    shared.clone(),
                    writer,
                    session_id,
                    key,
                    request_id,
                    record.object_id,
                    condition,
                    condition_value,
                    generation,
                    timeout,
                );
                return Ok(None);
            }
        }
        messages::CANCEL_WAIT => {
            let map = StrictMap::new("CANCEL_WAIT", &value, &[0])
                .map_err(|_| ControlError::bad("invalid wait cancellation"))?;
            state
                .sessions
                .get_mut(&session_id)
                .unwrap()
                .cancelled_waits
                .insert(required_u64(&map, 0)?);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        messages::PLAY | messages::PAUSE | messages::FLUSH | messages::DRAIN => {
            let key = track_key_from_value(session_id, &value)?;
            let mut linked_play = None;
            let mut linked_pause = false;
            let track = state
                .tracks
                .get_mut(&key)
                .ok_or_else(|| ControlError::missing("track does not exist"))?;
            match record.record_type {
                messages::PLAY => {
                    let map = StrictMap::new("PLAY", &value, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
                        .map_err(|_| ControlError::bad("invalid PLAY"))?;
                    let request = PlayRequest {
                        start_pts_us: map
                            .required(3)
                            .map_err(|_| ControlError::bad("missing start PTS"))?
                            .as_i64()
                            .ok_or_else(|| ControlError::bad("invalid start PTS"))?,
                        minimum_buffer_us: required_u64(&map, 4)?,
                        maximum_latency_us: required_u64(&map, 5)?,
                        rate_32_32: map
                            .required(6)
                            .map_err(|_| ControlError::bad("missing rate"))?
                            .as_i64()
                            .ok_or_else(|| ControlError::bad("invalid rate"))?,
                        late_policy: required_u64(&map, 7)?,
                        loop_count: required_u64(&map, 8)?,
                        start_policy: required_u64(&map, 9)?,
                    };
                    if request.minimum_buffer_us > request.maximum_latency_us
                        || request.rate_32_32 != 1_i64 << 32
                        || required_u64(&map, 10)? != track.state.channel_generation.get()
                    {
                        return Err(ControlError::state("PLAY policy or generation is invalid"));
                    }
                    track.playing = true;
                    track.play_request = request;
                    track.state.milestones |= MILESTONE_CLOCK_STARTED;
                    linked_play = Some(request);
                }
                messages::PAUSE => {
                    track.playing = false;
                    linked_pause = true;
                }
                messages::FLUSH => {
                    let map = StrictMap::new("FLUSH", &value, &[0, 1, 2, 3])
                        .map_err(|_| ControlError::bad("invalid FLUSH"))?;
                    let epoch = u32::try_from(required_u64(&map, 3)?)
                        .map_err(|_| ControlError::bad("invalid FLUSH epoch"))?;
                    if epoch <= track.state.media_epoch {
                        return Err(ControlError::state("FLUSH epoch did not advance"));
                    }
                    track.state.media_epoch = epoch;
                    track.state.last_media_id = 0;
                    track.recovery_pending = true;
                    track.retained = None;
                }
                messages::DRAIN => {
                    if track.eos_epoch.is_none() {
                        return Err(ControlError::state("DRAIN requires channel EOS"));
                    }
                }
                _ => unreachable!(),
            }
            if linked_play.is_some() || linked_pause {
                let active_tracks = state
                    .surfaces
                    .get(&key.surface)
                    .map(|surface| surface.active_slots.values().copied().collect::<Vec<_>>())
                    .unwrap_or_default();
                for track_id in active_tracks {
                    let member = TrackKey {
                        surface: key.surface,
                        track: track_id,
                    };
                    let Some(track) = state.tracks.get_mut(&member) else {
                        continue;
                    };
                    if let Some(request) = linked_play {
                        track.playing = true;
                        track.play_request = request;
                        track.state.milestones |= MILESTONE_CLOCK_STARTED;
                    } else {
                        track.playing = false;
                    }
                }
            }
            advance_projection(&mut state);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        }
        _ if record.flags & RECORD_OPTIONAL != 0 => return Ok(None),
        _ => {
            return Err(ControlError {
                code: messages::ERROR_UNSUPPORTED_PROFILE,
                message: "control record is not implemented by vvmux",
            });
        }
    };
    let body = reply
        .2
        .map_err(|_| ControlError::bad("reply encoding failed"))?;
    let response = (reply.0, reply.1, body);
    if let Some((key, fingerprint)) = mutation_cache {
        let cache_key = (session_id, key);
        if !state.idempotency.contains_key(&cache_key) {
            while state.idempotency.len() >= 256 {
                let Some(oldest) = state.idempotency_order.pop_front() else {
                    break;
                };
                state.idempotency.remove(&oldest);
            }
            state.idempotency_order.push_back(cache_key);
        }
        state.idempotency.insert(
            cache_key,
            CachedMutation {
                fingerprint,
                record_type: response.0,
                object_id: response.1,
                body: response.2.clone(),
            },
        );
    }
    Ok(Some(response))
}

fn handle_track(
    reader: &mut Reader,
    shared: &Arc<Mutex<State>>,
    changed: &Arc<Condvar>,
) -> io::Result<()> {
    let writer = reader.writer();
    let first = reader.read_record(ConnectionKind::Track)?;
    let envelope = messages::decode_control(&first.body)?;
    let request_id = envelope.request_id;
    let open = ChannelOpen::decode(first.object_id, &first.body)?;
    let key = TrackKey {
        surface: SurfaceKey {
            session: open.session_id,
            context: open.context_id,
            surface: open.surface_id,
        },
        track: open.track_id,
    };
    {
        let mut state = lock(shared);
        let session = state
            .sessions
            .get(&open.session_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session does not exist"))?;
        if session.closed {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session is no longer live",
            ));
        }
        let expected = auth::channel_tag(
            session.channel_key.expose(),
            open.session_id,
            open.context_id,
            open.surface_id,
            open.track_id,
            open.channel_generation,
            open.track_kind as u32,
            open.lane as u32,
            &open.client_nonce,
        );
        if !auth::verify_tag(&expected, &open.authentication_tag) {
            writer.write_record(
                messages::ERROR,
                open.track_id,
                &protocol_error(
                    request_id,
                    messages::ERROR_AUTH_FAILED,
                    true,
                    "channel authentication failed",
                )?,
            )?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "channel authentication failed",
            ));
        }
        let track = state
            .tracks
            .get_mut(&key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "track does not exist"))?;
        if track.state.channel_generation.get() != open.channel_generation
            || track.configuration.kind.kind() != open.track_kind
            || track.configuration.lane != open.lane
        {
            writer.write_record(
                messages::ERROR,
                open.track_id,
                &protocol_error(
                    request_id,
                    messages::ERROR_STALE_CHANNEL_GENERATION,
                    true,
                    "CHANNEL_OPEN does not match the live track generation",
                )?,
            )?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stale channel generation",
            ));
        }
        if track.channel_writer.is_some() {
            writer.write_record(
                messages::ERROR,
                open.track_id,
                &protocol_error(
                    request_id,
                    messages::ERROR_CHANNEL_BUSY,
                    true,
                    "track generation already has a live channel",
                )?,
            )?;
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "track channel is busy",
            ));
        }
        let maximum_bytes = u64::from(track.configuration.maximum_record_body);
        track
            .state
            .accept_channel(
                ChannelGeneration::new(open.channel_generation),
                maximum_bytes,
                INITIAL_FLOW_RECORDS,
                track.configuration.maximum_record_body,
            )
            .map_err(io::Error::other)?;
        track.channel_writer = Some(writer.clone());
        writer.write_record(
            messages::CHANNEL_ACCEPTED,
            open.track_id,
            &Envelope::new(
                request_id,
                vec![
                    (0, Value::Unsigned(open.context_id)),
                    (1, Value::Unsigned(open.surface_id)),
                    (2, Value::Unsigned(open.track_id)),
                    (3, Value::Unsigned(open.channel_generation)),
                    (4, Value::Unsigned(maximum_bytes)),
                    (5, Value::Unsigned(INITIAL_FLOW_RECORDS)),
                    (
                        6,
                        Value::Unsigned(u64::from(track.configuration.maximum_record_body)),
                    ),
                    (7, Value::Unsigned(track.state.revision.get())),
                ],
            )
            .encode()?,
        )?;
        reader.set_maximum(track.configuration.maximum_record_body)?;
    }
    reader.clear_read_deadline()?;
    let result = track_loop(reader, shared, changed, key);
    let mut state = lock(shared);
    let mut changed_payload = None;
    if let Some(track) = state.tracks.get_mut(&key) {
        track.channel_writer = None;
        let _ = track.state.detach();
        if result.is_err() || track.eos_epoch.is_none() {
            let _ = track.state.lose();
            track.recovery_pending = true;
        }
        changed_payload = Envelope::new(0, track_status_payload(key, track))
            .encode()
            .ok();
    }
    let control = state
        .sessions
        .get(&key.surface.session)
        .map(|session| session.writer.clone());
    advance_projection(&mut state);
    drop(state);
    if let Some(payload) = changed_payload
        && let Some(control) = control
    {
        let _ = control.write_record(messages::TRACK_CHANGED, key.track, &payload);
    }
    if result.is_err()
        && let Ok(body) = protocol_error(
            0,
            messages::ERROR_BAD_MESSAGE,
            true,
            "track channel failed validation",
        )
    {
        let _ = writer.write_record(messages::ERROR, key.track, &body);
    }
    result
}

fn track_loop(
    reader: &mut Reader,
    shared: &Arc<Mutex<State>>,
    changed: &Arc<Condvar>,
    key: TrackKey,
) -> io::Result<()> {
    loop {
        let record = match reader.read_record(ConnectionKind::Track) {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if record.object_id != key.track {
            return Err(invalid("track record object ID is not the accepted track"));
        }
        if record.record_type == messages::CHANNEL_EOS {
            let envelope = messages::decode_control(&record.body)?;
            if envelope.request_id != 0 {
                return Err(invalid("CHANNEL_EOS must be uncorrelated"));
            }
            let value = Value::Map(envelope.payload);
            let eos = StrictMap::new("CHANNEL_EOS", &value, &[0, 1, 2, 3, 4, 5])
                .map_err(io::Error::other)?;
            let mut state = lock(shared);
            let track = state
                .tracks
                .get_mut(&key)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "track disappeared"))?;
            if eos.required_u64(0).ok() != Some(key.surface.context)
                || eos.required_u64(1).ok() != Some(key.surface.surface)
                || eos.required_u64(2).ok() != Some(key.track)
                || eos.required_u64(3).ok() != Some(track.state.channel_generation.get())
                || eos.required_u64(5).ok() != Some(record.sequence.saturating_sub(1))
            {
                return Err(invalid("CHANNEL_EOS identity or sequence is invalid"));
            }
            let epoch = eos
                .required_u64(4)
                .ok()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| invalid("CHANNEL_EOS epoch is invalid"))?;
            if epoch < track.state.media_epoch {
                return Err(invalid("CHANNEL_EOS epoch is stale"));
            }
            track.eos_epoch = Some(epoch);
            track.state.milestones |= MILESTONE_EOS_ACCEPTED;
            track.last_record_sequence = record.sequence;
            advance_projection(&mut state);
            continue;
        }

        let mut state = lock(shared);
        let events = state.events.clone();
        let wakeup = state.media_wakeup.clone();
        let configuration = state
            .tracks
            .get(&key)
            .map(|track| track.configuration.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "track disappeared"))?;
        let (epoch, media_id, random_access, pts, retained) =
            validate_media_record(&configuration, &record)?;
        {
            let track = state.tracks.get_mut(&key).unwrap();
            track
                .state
                .admit_media(
                    track.state.channel_generation,
                    u32::try_from(record.body.len())
                        .map_err(|_| invalid("media body exceeds u32"))?,
                    epoch,
                    media_id,
                    random_access,
                )
                .map_err(io::Error::other)?;
            track.state.milestones |= MILESTONE_DECODER_INITIALIZED | MILESTONE_OUTPUT_READY;
            track.last_record_sequence = record.sequence;
            track.last_pts_us = pts;
            if random_access {
                track.recovery_pending = false;
            }
            if retained {
                track.retained = Some(Arc::from(record.body.clone()));
            }
        }
        if events.is_none() {
            // The eventless constructor is used by focused presenter/bridge tests. It terminates
            // the media locally, so successful validation is immediately reusable flow.
            let track = state.tracks.get_mut(&key).unwrap();
            track.outer_presented = true;
            track.state.milestones |= MILESTONE_PRESENTED;
            track.state.flow.raise_maxima(
                track
                    .state
                    .flow
                    .maximum_body_bytes
                    .saturating_add(record.body.len() as u64),
                track.state.flow.maximum_media_records.saturating_add(1),
            );
            send_flow_update(key, track);
            advance_projection(&mut state);
            drop(state);
            changed.notify_all();
            continue;
        }
        state.next_delivery = state
            .next_delivery
            .checked_add(1)
            .ok_or_else(|| io::Error::other("delivery ID exhausted"))?;
        let delivery_id = state.next_delivery;
        let source = bridge_track_key(key);
        state.deliveries.insert(
            delivery_id,
            PendingDelivery {
                track: key,
                bytes: record.body.len() as u64,
            },
        );
        let recovered_keyframe = (matches!(configuration.kind, KindConfiguration::Video(_))
            && random_access)
            .then_some((epoch, pts));
        let event = MediaEvent {
            delivery_id,
            source,
            record_type: record.record_type,
            recovered_keyframe,
            body: record.body,
        };
        let queued = events
            .as_ref()
            .is_some_and(|sender| sender.try_send(event).is_ok());
        if !queued {
            state.deliveries.remove(&delivery_id);
            let track = state.tracks.get_mut(&key).unwrap();
            track.recovery_pending = true;
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bounded bridge media queue is full",
            ));
        }
        advance_projection(&mut state);
        drop(state);
        changed.notify_all();
        if let Some(wakeup) = wakeup {
            wakeup();
        }
    }
}

fn validate_media_record(
    configuration: &TrackConfiguration,
    record: &Record,
) -> io::Result<(u32, u64, bool, i64, bool)> {
    match (&configuration.kind, record.record_type) {
        (KindConfiguration::Video(config), messages::VIDEO_PACKET) => {
            let packet = media::parse_video_packet(&record.body)?;
            if packet.data.len() > config.maximum_access_unit_bytes as usize {
                return Err(invalid("video packet exceeds immutable configuration"));
            }
            Ok((
                packet.epoch,
                packet.packet_id,
                packet.flags & media::VIDEO_PACKET_KEY != 0,
                packet.pts_us,
                false,
            ))
        }
        (KindConfiguration::Audio(config), messages::AUDIO_PACKET) => {
            let packet = media::parse_audio_packet(&record.body)?;
            if packet.data.len() > config.maximum_access_unit_bytes as usize {
                return Err(invalid("audio packet exceeds immutable configuration"));
            }
            Ok((packet.epoch, packet.packet_id, true, packet.pts_us, false))
        }
        (KindConfiguration::Raster(config), messages::RASTER_FRAME) => {
            let flags = record
                .body
                .get(4..8)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_be_bytes)
                .ok_or_else(|| invalid("raster record is truncated"))?;
            if flags & media::RASTER_FRAME_DELTA == 0 {
                let frame = media::parse_full_raster_frame(&record.body)?;
                if (frame.width, frame.height) != (config.width, config.height) {
                    return Err(invalid("raster dimensions differ from track configuration"));
                }
                let _ = media::decode_raster_pixels(frame)?;
                Ok((frame.epoch, frame.frame_id, true, frame.pts_us, true))
            } else {
                if !config.delta_enabled {
                    return Err(invalid("raster delta was not negotiated"));
                }
                let frame = media::parse_delta_raster_frame(
                    &record.body,
                    config.width,
                    config.height,
                    u32::from(config.maximum_delta_operations),
                )?;
                Ok((frame.epoch, frame.frame_id, false, frame.pts_us, false))
            }
        }
        (KindConfiguration::EncodedImage(config), messages::IMAGE_DATA) => {
            if record.body.len() != config.encoded_length as usize
                || config.sha256.is_some_and(|expected| {
                    let actual: [u8; 32] = Sha256::digest(&record.body).into();
                    actual != expected
                })
            {
                return Err(invalid(
                    "image length or hash differs from track configuration",
                ));
            }
            Ok((0, 1, true, 0, true))
        }
        _ => Err(invalid("media record type does not match its track kind")),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_wait(
    shared: Arc<Mutex<State>>,
    writer: Arc<Writer>,
    session_id: u64,
    key: TrackKey,
    request_id: u64,
    object_id: u64,
    condition: u64,
    condition_value: Option<u64>,
    generation: u64,
    timeout_us: u64,
) {
    thread::spawn(move || {
        enum WaitOutcome {
            Satisfied(Vec<u8>),
            Failed(u64, &'static str),
        }
        let deadline = Instant::now() + Duration::from_micros(timeout_us);
        loop {
            let outcome = {
                let mut state = lock(&shared);
                let cancelled = state
                    .sessions
                    .get_mut(&session_id)
                    .is_some_and(|session| session.cancelled_waits.remove(&request_id));
                if cancelled {
                    Some(WaitOutcome::Failed(
                        messages::ERROR_CANCELLED,
                        "track wait was cancelled",
                    ))
                } else {
                    match state.tracks.get(&key) {
                        None => Some(WaitOutcome::Failed(
                            messages::ERROR_NOT_FOUND,
                            "track was destroyed while waiting",
                        )),
                        Some(track) if track.state.channel_generation.get() != generation => {
                            Some(WaitOutcome::Failed(
                                messages::ERROR_STALE_CHANNEL_GENERATION,
                                "channel generation changed while waiting",
                            ))
                        }
                        Some(track) => {
                            evaluate_wait(track, condition, condition_value).map(|observed| {
                                match Envelope::new(
                                    request_id,
                                    wait_payload(key, track, condition, observed),
                                )
                                .encode()
                                {
                                    Ok(body) => WaitOutcome::Satisfied(body),
                                    Err(_) => WaitOutcome::Failed(
                                        messages::ERROR_BAD_MESSAGE,
                                        "wait reply encoding failed",
                                    ),
                                }
                            })
                        }
                    }
                }
            };
            if let Some(outcome) = outcome {
                match outcome {
                    WaitOutcome::Satisfied(body) => {
                        let _ = writer.write_record(messages::WAIT_SATISFIED, object_id, &body);
                    }
                    WaitOutcome::Failed(code, diagnostic) => {
                        if let Ok(body) = protocol_error(request_id, code, false, diagnostic) {
                            let _ = writer.write_record(messages::ERROR, object_id, &body);
                        }
                    }
                }
                break;
            }
            if Instant::now() >= deadline {
                if let Ok(body) = protocol_error(
                    request_id,
                    messages::ERROR_TIMEOUT,
                    false,
                    "track wait timed out",
                ) {
                    let _ = writer.write_record(messages::ERROR, object_id, &body);
                }
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        if let Some(session) = lock(&shared).sessions.get_mut(&session_id) {
            session.pending_waits = session.pending_waits.saturating_sub(1);
        }
    });
}

fn evaluate_wait(track: &TrackEntry, condition: u64, value: Option<u64>) -> Option<u64> {
    match condition {
        1 => (track.state.revision.get() > value?).then_some(track.state.revision.get()),
        2 => {
            let mask = value?;
            (mask != 0 && track.state.milestones & mask == mask).then_some(track.state.milestones)
        }
        3 => (track.outer_presented && track.state.last_media_id >= value?)
            .then_some(track.state.last_media_id),
        4 => {
            let pts = i64::try_from(value?).ok()?;
            (track.outer_presented && track.last_pts_us >= pts)
                .then_some(track.last_pts_us.max(0) as u64)
        }
        5 => track.playing.then_some(1),
        6 => (track.state.milestones & MILESTONE_BUFFERED_ENDED != 0).then_some(1),
        7 => (track.state.milestones & MILESTONE_CHANNEL_ACCEPTED != 0).then_some(1),
        8 => (track.state.milestones & MILESTONE_CHANNEL_DETACHED != 0).then_some(1),
        9 => track.state.lost.then_some(1),
        _ => None,
    }
}

fn wait_payload(
    key: TrackKey,
    track: &TrackEntry,
    condition: u64,
    observed: u64,
) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(key.surface.context)),
        (1, Value::Unsigned(key.surface.surface)),
        (2, Value::Unsigned(key.track)),
        (3, Value::Unsigned(track.state.revision.get())),
        (4, Value::Unsigned(track.state.channel_generation.get())),
        (5, Value::Unsigned(condition)),
        (6, Value::Unsigned(observed)),
    ]
}

fn validate_node_mutations(
    state: &State,
    session_id: u64,
    mutations: &[NodeMutation],
) -> Result<(), ControlError> {
    let mut live = state
        .nodes
        .iter()
        .filter_map(|(key, entry)| {
            (key.session == session_id).then_some((*key, entry.node.clone()))
        })
        .collect::<HashMap<_, _>>();
    for mutation in mutations {
        match mutation {
            NodeMutation::Create(node) => {
                let key = NodeKey {
                    session: session_id,
                    context: node.owning_context_id,
                    node: node.node_id,
                };
                if live.contains_key(&key) {
                    return Err(ControlError::state("scene node identity is already live"));
                }
                validate_node_surface(state, session_id, node)?;
                live.insert(key, node.clone());
            }
            NodeMutation::Update(node) => {
                let key = NodeKey {
                    session: session_id,
                    context: node.owning_context_id,
                    node: node.node_id,
                };
                if !live.contains_key(&key) {
                    return Err(ControlError::missing("scene node does not exist"));
                }
                validate_node_surface(state, session_id, node)?;
                live.insert(key, node.clone());
            }
            NodeMutation::Delete(key) => {
                if live.remove(key).is_none() {
                    return Err(ControlError::missing("scene node does not exist"));
                }
            }
        }
    }
    if live.len() > state.config.max_nodes {
        return Err(ControlError {
            code: messages::ERROR_LIMIT_EXCEEDED,
            message: "scene node capacity is exhausted",
        });
    }
    Ok(())
}

fn validate_node_surface(
    state: &State,
    session_id: u64,
    node: &ProtocolSceneNode,
) -> Result<(), ControlError> {
    let surface = SurfaceKey {
        session: session_id,
        context: node.surface_context_id,
        surface: node.surface_id,
    };
    if !state.surfaces.contains_key(&surface) {
        return Err(ControlError::missing(
            "scene node references a missing surface",
        ));
    }
    if node.owning_context_id
        != state
            .sessions
            .get(&session_id)
            .map_or(0, |session| session.root_context)
    {
        return Err(ControlError::state(
            "scene node is outside the root context",
        ));
    }
    Ok(())
}

fn apply_node_mutations(state: &mut State, session_id: u64, mutations: Vec<NodeMutation>) {
    let pane = state
        .sessions
        .get(&session_id)
        .map_or(0, |session| session.pane);
    for mutation in mutations {
        match mutation {
            NodeMutation::Create(node) | NodeMutation::Update(node) => {
                state.nodes.insert(
                    NodeKey {
                        session: session_id,
                        context: node.owning_context_id,
                        node: node.node_id,
                    },
                    NodeEntry { pane, node },
                );
            }
            NodeMutation::Delete(key) => {
                state.nodes.remove(&key);
            }
        }
    }
}

fn selected_visual_track(state: &State, surface: SurfaceKey) -> Option<TrackKey> {
    let active = state.surfaces.get(&surface)?.active_slots.values();
    for track in active {
        let key = TrackKey {
            surface,
            track: *track,
        };
        if state
            .tracks
            .get(&key)
            .is_some_and(|entry| !matches!(entry.configuration.kind, KindConfiguration::Audio(_)))
        {
            return Some(key);
        }
    }
    state.tracks.iter().find_map(|(key, entry)| {
        (key.surface == surface && !matches!(entry.configuration.kind, KindConfiguration::Audio(_)))
            .then_some(*key)
    })
}

fn projected_node_config(
    node: &ProtocolSceneNode,
    track: SourceKey,
    session: &SessionRuntime,
    viewport_offset: usize,
) -> Option<SceneNodeConfig> {
    let geometry_value = Value::Map(node.geometry.clone());
    let geometry = StrictMap::new(
        "terminal scene geometry",
        &geometry_value,
        &[0, 1, 2, 3, 4, 5, 6, 7],
    )
    .ok()?;
    let kind = geometry.required_u64(0).ok()?;
    let mut x = geometry.required(1).ok()?.as_i64()?;
    let mut y = geometry.required(2).ok()?.as_i64()?;
    let width = geometry.required(3).ok()?.as_i64()?;
    let height = geometry.required(4).ok()?.as_i64()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let anchor_id = if kind == 2 {
        let context = geometry.required_u64(6).ok()?;
        let anchor = geometry.required_u64(7).ok()?;
        let (row, column) = session.anchors.get(&(context, anchor)).copied()?;
        x = x.checked_add(i64::try_from(column).ok()?.checked_shl(32)?)?;
        y = y.checked_add(i64::from(row).checked_shl(32)?)?;
        Some(anchor)
    } else if kind == 1 {
        None
    } else {
        return None;
    };
    y = y.checked_sub(i64::try_from(viewport_offset).ok()?.checked_shl(32)?)?;
    let clip = node.clip.as_ref().and_then(|clip| {
        let clip_value = Value::Map(clip.clone());
        let clip = StrictMap::new("terminal clip", &clip_value, &[0, 1, 2, 3]).ok()?;
        Some(ClipRect {
            x: clip.required(0).ok()?.as_i64()?,
            y: clip.required(1).ok()?.as_i64()?,
            width: clip.required(2).ok()?.as_i64()?,
            height: clip.required(3).ok()?.as_i64()?,
        })
    });
    Some(SceneNodeConfig {
        node: NodeConfig {
            node_id: node.node_id,
            track,
            x,
            y,
            width,
            height,
            z_index: node.z_index,
            visible: node.visible,
            anchor_id,
        },
        clip,
    })
}

fn source_descriptor(
    tracks: &HashMap<TrackKey, TrackEntry>,
    key: TrackKey,
    track: &TrackEntry,
) -> SourceDescriptor {
    match &track.configuration.kind {
        KindConfiguration::Raster(config) => SourceDescriptor::Raster(config.clone()),
        KindConfiguration::EncodedImage(config) => SourceDescriptor::Image(config.clone()),
        KindConfiguration::Video(config) => SourceDescriptor::Video(config.clone()),
        KindConfiguration::Audio(config) => {
            let linked_video_source_id = tracks.iter().find_map(|(candidate, entry)| {
                (candidate.surface == key.surface
                    && candidate.track != key.track
                    && matches!(entry.configuration.kind, KindConfiguration::Video(_)))
                .then_some(candidate.track)
            });
            SourceDescriptor::Audio(AudioSourceConfig {
                linked_video_source_id,
                codec: config.codec.clone(),
                packetization: config.packetization.clone(),
                extradata: config.extradata.clone(),
                sample_rate: config.sample_rate,
                channels: u16::from(config.channels),
                channel_mask: config.channel_mask,
                bitrate: track.configuration.maximum_encoded_bits_per_second,
                max_access_unit_bytes: config.maximum_access_unit_bytes,
                codec_string: config.codec_string.clone(),
            })
        }
    }
}

fn semantic_descriptor(descriptor: &SurfaceDescriptor) -> SemanticDescriptor {
    SemanticDescriptor {
        role: descriptor.role as u64,
        title: descriptor.title.clone(),
        content_revision: descriptor.semantic_content_revision,
        semantic_availability: descriptor.semantic_availability,
        locator: descriptor.locator_hint.clone(),
    }
}

fn supports_track(configuration: &TrackConfiguration) -> bool {
    (1..=4).contains(&configuration.slot)
        && match &configuration.kind {
            KindConfiguration::Video(video) => {
                media::is_portable_packetization(&video.codec, &video.packetization)
            }
            KindConfiguration::Audio(audio) => media::validate_audio_initialization(
                &audio.codec,
                &audio.packetization,
                &audio.extradata,
                audio.sample_rate,
                u16::from(audio.channels),
            )
            .is_ok(),
            KindConfiguration::Raster(_) | KindConfiguration::EncodedImage(_) => true,
        }
}

fn surface_ready_payload(key: SurfaceKey, surface: &SurfaceState) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(key.context)),
        (1, Value::Unsigned(key.surface)),
        (2, Value::Unsigned(surface.revision.get())),
        (3, Value::Unsigned(surface.generation.get())),
        (4, Value::Unsigned(surface.definition.policy)),
        (5, Value::Map(surface.definition.profile_parameters.clone())),
    ]
}

fn surface_status_payload(key: SurfaceKey, surface: &SurfaceEntry) -> Vec<(u64, Value)> {
    let definition = &surface.state.definition;
    vec![
        (0, Value::Unsigned(key.context)),
        (1, Value::Unsigned(key.surface)),
        (2, Value::Unsigned(surface.state.revision.get())),
        (3, Value::Unsigned(surface.state.generation.get())),
        (4, Value::Text(definition.semantic_profile.clone())),
        (5, Value::Unsigned(definition.coordinate_model as u64)),
        (6, Value::Unsigned(definition.logical_width)),
        (7, Value::Unsigned(definition.logical_height)),
        (8, Value::Unsigned(definition.scale_numerator)),
        (9, Value::Unsigned(definition.scale_denominator)),
        (10, Value::Unsigned(u64::from(definition.rotation))),
        (
            11,
            definition
                .descriptor
                .to_value()
                .unwrap_or(Value::Map(vec![])),
        ),
        (12, Value::Unsigned(definition.policy)),
        (
            13,
            Value::Map(
                surface
                    .active_slots
                    .iter()
                    .map(|(slot, track)| (*slot, Value::Unsigned(*track)))
                    .collect(),
            ),
        ),
        (14, Value::Unsigned(1)),
        (15, Value::Map(definition.profile_parameters.clone())),
    ]
}

fn track_ready_payload(
    key: TrackKey,
    configuration: &TrackConfiguration,
    state: &TrackState,
) -> Vec<(u64, Value)> {
    let mut payload = vec![
        (0, Value::Unsigned(key.surface.context)),
        (1, Value::Unsigned(key.surface.surface)),
        (2, Value::Unsigned(key.track)),
        (3, Value::Unsigned(state.revision.get())),
        (4, Value::Unsigned(state.channel_generation.get())),
        (5, Value::Unsigned(CHANNEL_OPEN_DEADLINE_US)),
        (
            6,
            Value::Unsigned(u64::from(configuration.maximum_record_body)),
        ),
        (
            7,
            Value::Map(configuration.payload(false).unwrap_or_default()),
        ),
        (8, Value::Bool(true)),
    ];
    if let KindConfiguration::Raster(raster) = &configuration.kind
        && raster.delta_enabled
    {
        payload.push((
            9,
            Value::Unsigned(u64::from(raster.maximum_delta_operations)),
        ));
    }
    payload
}

fn track_status_payload(key: TrackKey, track: &TrackEntry) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(key.surface.context)),
        (1, Value::Unsigned(key.surface.surface)),
        (2, Value::Unsigned(key.track)),
        (3, Value::Unsigned(track.state.revision.get())),
        (4, Value::Unsigned(track.configuration.kind.kind() as u64)),
        (5, Value::Unsigned(track.configuration.mode as u64)),
        (6, Value::Unsigned(if track.state.lost { 6 } else { 1 })),
        (7, Value::Unsigned(track.state.channel_generation.get())),
        (
            8,
            Value::Unsigned(if track.channel_writer.is_some() { 1 } else { 0 }),
        ),
        (9, Value::Unsigned(track.state.milestones)),
        (10, Value::Unsigned(u64::from(track.state.media_epoch))),
        (11, Value::Unsigned(track.state.last_media_id)),
        (12, Value::Unsigned(track.last_record_sequence)),
        (13, signed(track.last_pts_us)),
        (
            14,
            signed(if track.outer_presented {
                track.last_pts_us
            } else {
                0
            }),
        ),
        (15, Value::Unsigned(u64::from(track.outer_presented))),
        (16, Value::Unsigned(track.state.flow.sent_body_bytes)),
        (17, Value::Unsigned(track.state.flow.sent_media_records)),
        (18, Value::Unsigned(track.state.flow.maximum_body_bytes)),
        (19, Value::Unsigned(track.state.flow.maximum_media_records)),
        (20, Value::Unsigned(0)),
    ]
}

fn surface_key_from_map(session: u64, map: &StrictMap<'_>) -> Result<SurfaceKey, ControlError> {
    Ok(SurfaceKey {
        session,
        context: required_u64(map, 0)?,
        surface: required_u64(map, 1)?,
    })
}

fn track_key_from_map(session: u64, map: &StrictMap<'_>) -> Result<TrackKey, ControlError> {
    Ok(TrackKey {
        surface: surface_key_from_map(session, map)?,
        track: required_u64(map, 2)?,
    })
}

fn track_key_from_value(session: u64, value: &Value) -> Result<TrackKey, ControlError> {
    let map = StrictMap::new("track identity", value, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
        .map_err(|_| ControlError::bad("invalid track identity"))?;
    track_key_from_map(session, &map)
}

fn required_u64(map: &StrictMap<'_>, key: u64) -> Result<u64, ControlError> {
    map.required_u64(key)
        .map_err(|_| ControlError::bad("missing or invalid unsigned field"))
}

fn require_root_context(
    state: &State,
    session_id: u64,
    context_id: u64,
) -> Result<(), ControlError> {
    if state
        .sessions
        .get(&session_id)
        .is_some_and(|session| session.root_context == context_id)
    {
        Ok(())
    } else {
        Err(ControlError::state(
            "vvmux exposes only its finite root context",
        ))
    }
}

fn remove_track(state: &mut State, key: TrackKey) -> Result<(), ControlError> {
    state
        .tracks
        .remove(&key)
        .ok_or_else(|| ControlError::missing("track does not exist"))?;
    if let Some(surface) = state.surfaces.get_mut(&key.surface) {
        surface
            .active_slots
            .retain(|_, track_id| *track_id != key.track);
    }
    state.deliveries.retain(|_, delivery| delivery.track != key);
    Ok(())
}

fn remove_surface_children(state: &mut State, surface: SurfaceKey) {
    let tracks = state
        .tracks
        .keys()
        .copied()
        .filter(|key| key.surface == surface)
        .collect::<Vec<_>>();
    for track in tracks {
        let _ = remove_track(state, track);
    }
    state.nodes.retain(|key, node| {
        !(key.session == surface.session
            && node.node.surface_context_id == surface.context
            && node.node.surface_id == surface.surface)
    });
}

fn cleanup_session(state: &mut State, session: u64) {
    state.sessions.remove(&session);
    let surfaces = state
        .surfaces
        .keys()
        .copied()
        .filter(|key| key.session == session)
        .collect::<Vec<_>>();
    for surface in surfaces {
        state.surfaces.remove(&surface);
        remove_surface_children(state, surface);
    }
    state.nodes.retain(|key, _| key.session != session);
    state
        .transactions
        .retain(|(owner, _, _), _| *owner != session);
    state.idempotency.retain(|(owner, _), _| *owner != session);
    state
        .idempotency_order
        .retain(|(owner, _)| *owner != session);
    state
        .projected_sources
        .retain(|source| source.producer != session);
}

fn detach_session(state: &mut State, session: u64) {
    let Some(runtime) = state.sessions.get_mut(&session) else {
        return;
    };
    runtime.closed = true;
    runtime.cancelled_waits.clear();
    runtime.pending_waits = 0;
    let anchors = runtime.anchors.clone();

    let tracks = state
        .tracks
        .keys()
        .copied()
        .filter(|key| key.surface.session == session)
        .collect::<Vec<_>>();
    for key in tracks {
        let retain = state.tracks.get(&key).is_some_and(|track| {
            track.retained.is_some()
                && matches!(
                    track.configuration.kind,
                    KindConfiguration::EncodedImage(_) | KindConfiguration::Raster(_)
                )
        });
        if retain {
            if let Some(track) = state.tracks.get_mut(&key) {
                track.channel_writer = None;
                track.playing = false;
            }
        } else {
            let _ = remove_track(state, key);
        }
    }

    let static_surfaces = state
        .tracks
        .keys()
        .filter(|key| key.surface.session == session)
        .map(|key| key.surface)
        .collect::<HashSet<_>>();
    let retained_surfaces = state
        .nodes
        .values()
        .filter_map(|node| {
            let surface = SurfaceKey {
                session,
                context: node.node.surface_context_id,
                surface: node.node.surface_id,
            };
            (node_uses_live_anchor(&node.node, &anchors) && static_surfaces.contains(&surface))
                .then_some(surface)
        })
        .collect::<HashSet<_>>();
    let tracks = state
        .tracks
        .keys()
        .copied()
        .filter(|key| key.surface.session == session && !retained_surfaces.contains(&key.surface))
        .collect::<Vec<_>>();
    for track in tracks {
        let _ = remove_track(state, track);
    }
    state
        .surfaces
        .retain(|key, _| key.session != session || retained_surfaces.contains(key));
    state.nodes.retain(|key, node| {
        key.session != session
            || (node_uses_live_anchor(&node.node, &anchors)
                && retained_surfaces.contains(&SurfaceKey {
                    session,
                    context: node.node.surface_context_id,
                    surface: node.node.surface_id,
                }))
    });
    state
        .transactions
        .retain(|(owner, _, _), _| *owner != session);
    state.idempotency.retain(|(owner, _), _| *owner != session);
    state
        .idempotency_order
        .retain(|(owner, _)| *owner != session);
}

fn node_uses_live_anchor(
    node: &ProtocolSceneNode,
    anchors: &HashMap<(u64, u64), (i32, usize)>,
) -> bool {
    let geometry = Value::Map(node.geometry.clone());
    let Ok(geometry) = StrictMap::new(
        "terminal scene geometry",
        &geometry,
        &[0, 1, 2, 3, 4, 5, 6, 7],
    ) else {
        return false;
    };
    geometry.required_u64(0).ok() == Some(2)
        && geometry
            .required_u64(6)
            .ok()
            .zip(geometry.required_u64(7).ok())
            .is_some_and(|anchor| anchors.contains_key(&anchor))
}

fn send_flow_update(key: TrackKey, track: &TrackEntry) {
    let Some(writer) = &track.channel_writer else {
        return;
    };
    if let Ok(body) = Envelope::new(
        0,
        vec![
            (0, Value::Unsigned(key.surface.context)),
            (1, Value::Unsigned(key.surface.surface)),
            (2, Value::Unsigned(key.track)),
            (3, Value::Unsigned(track.state.channel_generation.get())),
            (4, Value::Unsigned(track.state.flow.maximum_body_bytes)),
            (5, Value::Unsigned(track.state.flow.maximum_media_records)),
        ],
    )
    .encode()
    {
        let _ = writer.write_record(messages::MAX_CHANNEL_DATA, key.track, &body);
    }
}

fn send_need_keyframe(key: TrackKey, track: &TrackEntry, minimum_epoch: u32, reason: u64) {
    let Some(writer) = &track.channel_writer else {
        return;
    };
    if let Ok(body) = Envelope::new(
        0,
        vec![
            (0, Value::Unsigned(key.surface.context)),
            (1, Value::Unsigned(key.surface.surface)),
            (2, Value::Unsigned(key.track)),
            (3, Value::Unsigned(track.state.channel_generation.get())),
            (4, Value::Unsigned(u64::from(minimum_epoch))),
            (5, Value::Unsigned(reason)),
        ],
    )
    .encode()
    {
        let _ = writer.write_record(messages::NEED_KEYFRAME, key.track, &body);
    }
}

fn send_need_full_frame(key: TrackKey, track: &TrackEntry) {
    let Some(writer) = &track.channel_writer else {
        return;
    };
    if let Ok(body) = Envelope::new(
        0,
        vec![
            (0, Value::Unsigned(key.surface.context)),
            (1, Value::Unsigned(key.surface.surface)),
            (2, Value::Unsigned(key.track)),
            (3, Value::Unsigned(track.state.channel_generation.get())),
            (4, Value::Unsigned(1)),
        ],
    )
    .encode()
    {
        let _ = writer.write_record(messages::NEED_FULL_FRAME, key.track, &body);
    }
}

fn presenter_contract(config: &MediaConfig) -> ResourceContract {
    let mut contract = ResourceContract::denied();
    for (resource, value) in [
        (Resource::Surfaces, config.max_sources as u64),
        (Resource::Tracks, config.max_sources as u64),
        (Resource::Nodes, config.max_nodes as u64),
        (Resource::VideoTracks, config.max_sources as u64),
        (Resource::AudioTracks, config.max_sources as u64),
        (Resource::RasterTracks, config.max_sources as u64),
        (Resource::ImageTracks, config.max_sources as u64),
        (Resource::DecoderInstances, config.max_sources as u64),
        (Resource::CodedPixelsPerTrack, 8192 * 8192),
        (Resource::DecodedPixelsPerSecond, 8192 * 8192 * 60),
        (Resource::EncodedBitsPerSecond, 1_000_000_000),
        (Resource::MediaRecordsPerSecond, 4_000),
        (Resource::AudioSampleRate, 192_000),
        (Resource::AudioChannelsPerTrack, 8),
        (Resource::InflightMediaBytes, config.ipc_queue_bytes as u64),
        (Resource::TrackConnections, config.max_sources as u64),
        (
            Resource::RetainedPixels,
            config.aggregate_retained_bytes / 4,
        ),
        (
            Resource::MediaRecordBody,
            u64::from(vivid_protocol::HARD_MAX_RECORD_BODY),
        ),
        (
            Resource::ControlRecordBody,
            u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY),
        ),
        (Resource::PendingRequests, 256),
        (Resource::RegisteredWaits, MAX_WAITS as u64),
        (Resource::IdempotencyEntries, 256),
        (Resource::ChildSessionLeases, 0),
        (Resource::DisconnectGraceUs, 0),
        (Resource::InputEventsPerSecond, 0),
        (Resource::ObservationQueueEntries, 64),
        (Resource::ImageCacheBytes, config.aggregate_retained_bytes),
        (Resource::OpenSceneTransactions, config.max_nodes as u64),
        (Resource::ChildContexts, 0),
        (Resource::SuspendedChildSessions, 0),
        (
            Resource::PendingChannelOpenAttempts,
            config.max_sources as u64,
        ),
        (Resource::ActiveTerminalAnchors, config.max_anchors as u64),
        (Resource::SeenTerminalAnchorIds, config.max_anchors as u64),
    ] {
        contract.set(resource, value);
    }
    contract
}

fn target_descriptor(metrics: Metrics) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(u64::from(metrics.viewport_width))),
        (1, Value::Unsigned(u64::from(metrics.viewport_height))),
        (2, Value::Unsigned(u64::from(metrics.columns))),
        (3, Value::Unsigned(u64::from(metrics.rows))),
        (4, Value::Unsigned(u64::from(metrics.cell_width))),
        (5, Value::Unsigned(u64::from(metrics.cell_height))),
        (6, Value::Bool(true)),
        (7, Value::Unsigned(3)),
        (8, Value::Unsigned(MAX_ACTIVE_ANCHORS as u64)),
    ]
}

fn protocol_error(
    request_id: u64,
    code: u64,
    fatal: bool,
    diagnostic: impl Into<String>,
) -> io::Result<Vec<u8>> {
    ErrorReply {
        code,
        request_id,
        detail: ErrorDetail::new(vec![]).map_err(io::Error::other)?,
        fatal,
        diagnostic: diagnostic.into(),
    }
    .encode()
    .map_err(io::Error::other)
}

fn send_fatal(writer: &Writer, request_id: u64, code: u64, diagnostic: &'static str) -> io::Error {
    if let Ok(body) = protocol_error(request_id, code, true, diagnostic) {
        let _ = writer.write_record(messages::ERROR, 0, &body);
    }
    io::Error::new(io::ErrorKind::InvalidData, diagnostic)
}

fn advance_projection(state: &mut State) {
    state.projection_revision = state.projection_revision.saturating_add(1);
}

fn kind_name(kind: &KindConfiguration) -> &'static str {
    match kind {
        KindConfiguration::Video(_) => "video",
        KindConfiguration::Audio(_) => "audio",
        KindConfiguration::Raster(_) => "raster",
        KindConfiguration::EncodedImage(_) => "image",
    }
}

fn nonnegative(value: i32) -> Value {
    if value >= 0 {
        Value::Unsigned(value as u64)
    } else {
        Value::Negative(i64::from(value))
    }
}

fn signed(value: i64) -> Value {
    if value >= 0 {
        Value::Unsigned(value as u64)
    } else {
        Value::Negative(value)
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(64);
    for byte in bytes {
        value.push(DIGITS[(byte >> 4) as usize] as char);
        value.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    value
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn with_context(error: io::Error, context: &'static str) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ipc::BridgeSourceKey;
    use vivid_protocol::track::{KindConfiguration, RasterConfiguration, TrackMode};
    use vivid_sdk::{
        CoordinateModel, Fit, LaneClass, ProducerAuthentication, ProducerConfig, RequestMetadata,
        SceneNode, SurfaceDefinition, SurfaceDescriptor, SurfaceRole,
    };

    fn producer(endpoint: String, secret: &str) -> ProducerConfig {
        ProducerConfig {
            endpoint_control: Some(endpoint),
            authentication: ProducerAuthentication::root_hex(secret).unwrap(),
            producer_name: "vvmux-inner-test".into(),
            producer_version: "1.5".into(),
            target_profile: vivid_sdk::TERMINAL_SURFACE.into(),
            required_profiles: vec![
                vivid_sdk::LIVE_MEDIA.into(),
                vivid_sdk::OBSERVABILITY.into(),
                vivid_sdk::TERMINAL_SURFACE.into(),
                vivid_sdk::TIMED_MEDIA.into(),
                vivid_sdk::CORE_CONTROL.into(),
            ],
            optional_profiles: vec![],
            ..ProducerConfig::default()
        }
    }

    fn surface(context_id: u64, surface_id: u64) -> SurfaceDefinition {
        SurfaceDefinition {
            context_id,
            surface_id,
            semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
            coordinate_model: CoordinateModel::DesktopLogicalPixels,
            logical_width: 2,
            logical_height: 2,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
            descriptor: SurfaceDescriptor {
                role: SurfaceRole::Figure,
                title: "nested-test".into(),
                semantic_content_revision: 1,
                semantic_availability: 0,
                locator_hint: String::new(),
            },
            policy: 0,
            profile_parameters: vec![],
        }
    }

    fn raster(context_id: u64, surface_id: u64, track_id: u64) -> TrackConfiguration {
        TrackConfiguration {
            context_id,
            surface_id,
            track_id,
            slot: 3,
            mode: TrackMode::Live,
            lane: LaneClass::Bulk,
            maximum_record_body: media::rgba8_raw_frame_body_len(2, 2).unwrap(),
            maximum_rate_millihertz: 60_000,
            maximum_encoded_bits_per_second: 1_000_000,
            maximum_records_per_second: 60,
            maximum_inflight_body_bytes: 1024,
            kind: KindConfiguration::Raster(RasterConfiguration {
                width: 2,
                height: 2,
                alpha_mode: 1,
                delta_enabled: false,
                maximum_delta_operations: 1,
                zstd_enabled: false,
            }),
            target_latency_us: 16_000,
            maximum_latency_us: 100_000,
            retained_pixel_charge: 4,
        }
    }

    #[test]
    fn idempotency_fingerprint_ignores_only_request_correlation() {
        let mut first = Envelope::new(41, vec![(0, Value::Unsigned(7))]);
        first.idempotency_key = Some([3; messages::IDEMPOTENCY_KEY_BYTES]);
        let mut retried = first.clone();
        retried.request_id = 99;
        assert_eq!(
            mutation_fingerprint(messages::PAUSE, 17, &first),
            mutation_fingerprint(messages::PAUSE, 17, &retried)
        );

        retried.payload = vec![(0, Value::Unsigned(8))];
        assert_ne!(
            mutation_fingerprint(messages::PAUSE, 17, &first),
            mutation_fingerprint(messages::PAUSE, 17, &retried)
        );

        let cached = messages::ok(41);
        let recorrelated = recorrelate_cached_reply(&cached, 99).unwrap();
        assert_eq!(
            messages::decode_control(&recorrelated).unwrap().request_id,
            99
        );
    }

    #[test]
    fn root_authenticated_sdk_session_relays_one_priming_raster() {
        let directory = tempfile::tempdir().unwrap();
        let (events, received) = mpsc::sync_channel(4);
        let presenter = VirtualVivid::start_with_events(
            directory.path().join("vivid.sock"),
            MediaConfig::default(),
            Some(events),
        )
        .unwrap();
        presenter.update_metrics(7, 80, 24, (8, 16));
        let secret = presenter.issue_pane_capability(7).unwrap();
        let mut client =
            vivid_sdk::Session::connect(producer(presenter.endpoint(), &secret)).unwrap();
        let context = client.info().root_context_id;
        client
            .create_surface(surface(context, 9), &RequestMetadata::default())
            .unwrap();
        let track = client
            .create_track(raster(context, 9, 11), &RequestMetadata::default())
            .unwrap();
        let channel = client.open_track_channel(&track).unwrap();
        channel
            .send_raster(0, 1, &[0, 0, 0, 255].repeat(4), false)
            .unwrap();
        let event = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            event.source,
            BridgeSourceKey {
                producer: client.info().session_id,
                context,
                surface: 9,
                track: 11,
            }
        );
        assert_eq!(event.record_type, messages::RASTER_FRAME);
        assert!(!presenter.complete_bridge_delivery(event.delivery_id, true));
        client.close().unwrap();
    }

    #[test]
    fn clean_goodbye_retains_only_anchored_static_content() {
        let directory = tempfile::tempdir().unwrap();
        let (events, received) = mpsc::sync_channel(4);
        let presenter = VirtualVivid::start_with_events(
            directory.path().join("vivid.sock"),
            MediaConfig::default(),
            Some(events),
        )
        .unwrap();
        presenter.update_metrics(7, 80, 24, (8, 16));
        let secret = presenter.issue_pane_capability(7).unwrap();
        let mut client =
            vivid_sdk::Session::connect(producer(presenter.endpoint(), &secret)).unwrap();
        let session = client.info().session_id;
        let context = client.info().root_context_id;
        let surface = client
            .create_surface(surface(context, 9), &RequestMetadata::default())
            .unwrap();
        let track = client
            .create_track(raster(context, 9, 11), &RequestMetadata::default())
            .unwrap();
        let channel = client.open_track_channel(&track).unwrap();
        channel
            .send_raster(0, 1, &[0, 0, 0, 255].repeat(4), false)
            .unwrap();
        let event = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!presenter.complete_bridge_delivery(event.delivery_id, true));

        let marker = client.anchor_marker(context, 13).unwrap();
        presenter.observe_marker(7, &marker[2..marker.len() - 2], 2, 3);
        client
            .create_node(
                &SceneNode {
                    owning_context_id: context,
                    node_id: 17,
                    surface_context_id: surface.context_id(),
                    surface_id: surface.id(),
                    geometry: vec![
                        (0, Value::Unsigned(2)),
                        (1, Value::Unsigned(0)),
                        (2, Value::Unsigned(0)),
                        (3, Value::Unsigned(2_u64 << 32)),
                        (4, Value::Unsigned(2_u64 << 32)),
                        (5, Value::Unsigned(1)),
                        (6, Value::Unsigned(context)),
                        (7, Value::Unsigned(13)),
                    ],
                    fit: Fit::Contain,
                    linear_sampling: true,
                    z_index: 0,
                    visible: true,
                    opacity: u16::MAX,
                    clip: None,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        client.close().unwrap();

        let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
        assert_eq!(snapshot.surfaces.len(), 1);
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(snapshot.nodes.len(), 1);
        assert_eq!(snapshot.sources[0].key.producer, session);
        assert!(snapshot.sources[0].retained.is_some());
        assert_eq!(snapshot.nodes[0].config.node.anchor_id, Some(13));
    }

    #[test]
    fn reused_numeric_identities_are_isolated_by_session_and_pane() {
        let directory = tempfile::tempdir().unwrap();
        let presenter =
            VirtualVivid::start(directory.path().join("vivid.sock"), MediaConfig::default())
                .unwrap();
        presenter.update_metrics(7, 80, 24, (8, 16));
        presenter.update_metrics(8, 80, 24, (8, 16));
        let first_secret = presenter.issue_pane_capability(7).unwrap();
        let second_secret = presenter.issue_pane_capability(8).unwrap();
        let mut first =
            vivid_sdk::Session::connect(producer(presenter.endpoint(), &first_secret)).unwrap();
        let mut second =
            vivid_sdk::Session::connect(producer(presenter.endpoint(), &second_secret)).unwrap();
        for client in [&mut first, &mut second] {
            let context = client.info().root_context_id;
            client
                .create_surface(surface(context, 9), &RequestMetadata::default())
                .unwrap();
            client
                .create_track(raster(context, 9, 11), &RequestMetadata::default())
                .unwrap();
        }
        presenter.revoke_pane(7);
        let snapshot = presenter.projection_snapshot(&HashSet::from([8]));
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(snapshot.sources[0].key.producer, second.info().session_id);
        assert_eq!(
            snapshot.sources[0].key.context,
            second.info().root_context_id
        );
        assert_eq!(snapshot.sources[0].key.surface, 9);
        assert_eq!(snapshot.sources[0].key.track, 11);
        second.close().unwrap();
    }

    #[test]
    fn wrong_and_revoked_pane_secrets_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let presenter =
            VirtualVivid::start(directory.path().join("vivid.sock"), MediaConfig::default())
                .unwrap();
        presenter.update_metrics(7, 80, 24, (8, 16));
        let revoked = presenter.issue_pane_capability(7).unwrap();
        presenter.revoke_pane(7);
        assert!(vivid_sdk::Session::connect(producer(presenter.endpoint(), &revoked)).is_err());

        presenter.update_metrics(7, 80, 24, (8, 16));
        let valid = presenter.issue_pane_capability(7).unwrap();
        let mut wrong = valid.into_bytes();
        wrong[0] = if wrong[0] == b'0' { b'1' } else { b'0' };
        let wrong = String::from_utf8(wrong).unwrap();
        assert!(vivid_sdk::Session::connect(producer(presenter.endpoint(), &wrong)).is_err());
    }
}
