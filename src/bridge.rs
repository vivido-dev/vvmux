use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::auth::Secret32;
use vivid_protocol::cbor::Value;
use vivid_protocol::media::{self, AudioPacket, RasterDeltaOperation, VideoPacket};
use vivid_protocol::messages::{self, LaneClass};
use vivid_protocol::registry;
use vivid_protocol::track::{
    AudioConfiguration, ImageConfiguration, KindConfiguration, RasterConfiguration,
    TrackConfiguration, TrackMode, VideoConfiguration,
};
use vivid_sdk::{
    ChannelEvent, CoordinateModel, Fit, ProducerAuthentication, ProducerConfig, RequestMetadata,
    SceneNode, SessionEvent, SlotBinding, Surface, SurfaceDefinition, SurfaceDescriptor,
    SurfaceRole, Track, TrackChannel,
};
use zeroize::Zeroizing;

use crate::ipc::{
    BridgeKeyframeRequest, BridgeNode, BridgeSource, BridgeSourceKey, BridgeSourceKind,
    BridgeSurface, BridgeSurfaceKey, DisplayMetrics,
};

pub(crate) use vivid_sdk::ConnectionFactory;

const SLOT_VIDEO: u64 = 1;
const SLOT_AUDIO: u64 = 2;
const SLOT_RASTER: u64 = 3;
const SLOT_POSTER: u64 = 4;
pub(crate) const KEYFRAME_REASON_INITIAL: u64 = 1;
pub(crate) const KEYFRAME_REASON_DECODER_ERROR: u64 = 2;
pub(crate) const KEYFRAME_REASON_TRANSPORT_LOSS: u64 = 5;
/// How long a scene commit keeps following a moving outer target before it asks for a fresh
/// projection instead.
const TARGET_FOLLOW_TIMEOUT: Duration = Duration::from_millis(500);
/// Pause between attempts while the announcement that explains a stale reply is still in flight.
const TARGET_FOLLOW_POLL: Duration = Duration::from_millis(5);

/// One scene mutation, named so a stale reply can be retried against the target that caused it.
#[derive(Debug, Clone, Copy)]
enum NodeCommit<'a> {
    Create(&'a SceneNode),
    Update(&'a SceneNode),
    Delete { context_id: u64, node_id: u64 },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PlaybackSnapshot {
    pub state: u64,
    pub eos_state: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CapabilityChange {
    pub reason_mask: u64,
}

#[derive(Clone)]
struct OuterTrack {
    surface_key: BridgeSurfaceKey,
    track: Track,
    channel: Arc<TrackChannel>,
    kind: BridgeSourceKind,
    next_media_id: u64,
    outer_epoch: u32,
    inner_epoch: u32,
    last_raster_id: u64,
    activated: bool,
    playing: bool,
    eos: bool,
    reported_eos_state: u64,
}

struct PendingBody {
    record_type: u16,
    total: usize,
    received: usize,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct MediaCompletion {
    delivery_id: u64,
    delivered: bool,
    sequence: u64,
    object_id: u64,
}

/// Vivid 1.5 producer side of the nested presenter.
///
/// Every object allocated here belongs exclusively to the outer session. Inner IDs are lookup
/// keys only and never become outer Vivid IDs, revisions, generations, epochs, or media IDs.
pub struct OuterBridge {
    session: vivid_sdk::Session,
    authentication: Secret32,
    connection_factory: Option<Arc<dyn ConnectionFactory>>,
    endpoint_control: Option<String>,
    endpoint_realtime: Option<String>,
    endpoint_bulk: Option<String>,
    display: DisplayMetrics,
    surfaces: HashMap<BridgeSurfaceKey, Surface>,
    tracks: HashMap<BridgeSourceKey, OuterTrack>,
    active_sources: HashMap<BridgeSourceKey, BridgeSource>,
    nodes: HashMap<(u64, u64, u8), (u64, SceneNode)>,
    pending: HashMap<BridgeSourceKey, PendingBody>,
    completions: Vec<MediaCompletion>,
    keyframes: Vec<BridgeKeyframeRequest>,
    full_frames: HashSet<BridgeSourceKey>,
    losses: HashSet<BridgeSourceKey>,
    playback: Vec<(BridgeSourceKey, PlaybackSnapshot)>,
    outer_applied_revision: u64,
    diagnostic_generation: u64,
}

impl OuterBridge {
    #[allow(dead_code)]
    pub fn connect(
        endpoint: String,
        root_secret: Zeroizing<String>,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        Self::connect_with_bulk(endpoint, None, root_secret, display)
    }

    pub fn connect_with_bulk(
        endpoint: String,
        bulk_endpoint: Option<String>,
        root_secret: Zeroizing<String>,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        Self::connect_native(endpoint, None, bulk_endpoint, root_secret, display)
    }

    pub fn connect_native(
        control_endpoint: String,
        realtime_endpoint: Option<String>,
        bulk_endpoint: Option<String>,
        root_secret: Zeroizing<String>,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        let authentication = parse_root_secret(&root_secret)?;
        let config = producer_config(
            Some(control_endpoint.clone()),
            realtime_endpoint.clone(),
            bulk_endpoint.clone(),
            &authentication,
        );
        let session = vivid_sdk::Session::connect(config)?;
        Self::from_session(
            session,
            authentication,
            None,
            Some(control_endpoint),
            realtime_endpoint,
            bulk_endpoint,
            display,
        )
    }

    #[allow(dead_code)]
    pub(crate) fn connect_with_factory(
        connection_factory: Arc<dyn ConnectionFactory>,
        root_secret: Zeroizing<String>,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        let authentication = parse_root_secret(&root_secret)?;
        Self::connect_with_factory_secret(connection_factory, authentication, display)
    }

    pub(crate) fn connect_with_factory_secret(
        connection_factory: Arc<dyn ConnectionFactory>,
        authentication: Secret32,
        display: DisplayMetrics,
    ) -> io::Result<Self> {
        let config = producer_config(None, None, None, &authentication);
        let session = vivid_sdk::Session::connect_with_factory(config, connection_factory.clone())?;
        Self::from_session(
            session,
            authentication,
            Some(connection_factory),
            None,
            None,
            None,
            display,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_session(
        session: vivid_sdk::Session,
        authentication: Secret32,
        connection_factory: Option<Arc<dyn ConnectionFactory>>,
        endpoint_control: Option<String>,
        endpoint_realtime: Option<String>,
        endpoint_bulk: Option<String>,
        fallback_display: DisplayMetrics,
    ) -> io::Result<Self> {
        let display = display_from_target(&session, fallback_display)?;
        Ok(Self {
            session,
            authentication,
            connection_factory,
            endpoint_control,
            endpoint_realtime,
            endpoint_bulk,
            display,
            surfaces: HashMap::new(),
            tracks: HashMap::new(),
            active_sources: HashMap::new(),
            nodes: HashMap::new(),
            pending: HashMap::new(),
            completions: Vec::new(),
            keyframes: Vec::new(),
            full_frames: HashSet::new(),
            losses: HashSet::new(),
            playback: Vec::new(),
            outer_applied_revision: 0,
            diagnostic_generation: 1,
        })
    }

    pub fn display_metrics(&self) -> DisplayMetrics {
        self.display
    }

    pub fn mark_projection_applied(&mut self) -> u64 {
        self.outer_applied_revision = self.outer_applied_revision.saturating_add(1);
        self.outer_applied_revision
    }

    pub fn diagnostic_instance_generation(&self) -> u64 {
        self.diagnostic_generation
    }

    pub fn attachment_generations(&self) -> Vec<(BridgeSourceKey, u64)> {
        let mut values = self
            .tracks
            .iter()
            .map(|(key, track)| (*key, track.track.channel_generation().get()))
            .collect::<Vec<_>>();
        values.sort_by_key(|(key, _)| (key.producer, key.context, key.surface, key.track));
        values
    }

    pub fn rebuild(
        &mut self,
        surfaces: &[BridgeSurface],
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<HashSet<BridgeSourceKey>> {
        validate_snapshot(surfaces, sources, nodes)?;
        self.reconcile_surfaces(surfaces)?;
        let current = sources
            .iter()
            .map(|source| source.key)
            .collect::<HashSet<_>>();
        let removed = self
            .tracks
            .keys()
            .copied()
            .filter(|key| !current.contains(key))
            .collect::<Vec<_>>();
        for key in removed {
            self.remove_track(key)?;
        }

        let mut recreated = HashSet::new();
        for source in sources {
            let changed = self
                .tracks
                .get(&source.key)
                .is_some_and(|track| track.kind != source.kind);
            if changed {
                self.remove_track(source.key)?;
            }
            if !self.tracks.contains_key(&source.key) {
                self.create_outer_track(source)?;
                recreated.insert(source.key);
            }
        }
        self.active_sources = sources
            .iter()
            .cloned()
            .map(|source| (source.key, source))
            .collect();
        self.reconcile_nodes(nodes)?;
        self.update_playback(&[], sources)?;
        self.remove_absent_surfaces(surfaces)?;
        Ok(recreated)
    }

    pub fn replace_session(
        &mut self,
        surfaces: &[BridgeSurface],
        sources: &[BridgeSource],
        nodes: &[BridgeNode],
    ) -> io::Result<HashSet<BridgeSourceKey>> {
        validate_snapshot(surfaces, sources, nodes)?;
        let config = producer_config(
            self.endpoint_control.clone(),
            self.endpoint_realtime.clone(),
            self.endpoint_bulk.clone(),
            &self.authentication,
        );
        let session = match &self.connection_factory {
            Some(factory) => vivid_sdk::Session::connect_with_factory(config, factory.clone())?,
            None => vivid_sdk::Session::connect(config)?,
        };
        self.session = session;
        self.display = display_from_target(&self.session, self.display)?;
        self.surfaces.clear();
        self.tracks.clear();
        self.nodes.clear();
        self.pending.clear();
        self.active_sources.clear();
        self.diagnostic_generation = self.diagnostic_generation.saturating_add(1);
        self.rebuild(surfaces, sources, nodes)
    }

    pub fn update_playback(
        &mut self,
        previous: &[BridgeSource],
        current: &[BridgeSource],
    ) -> io::Result<()> {
        for source in current {
            let old = previous
                .iter()
                .find(|candidate| candidate.key == source.key);
            if source.playing
                && old.is_none_or(|old| !old.playing || old.play_request != source.play_request)
            {
                self.try_start_surface(source.key)?;
            } else if !source.playing
                && old.is_some_and(|old| old.playing)
                && let Some(track) = self.tracks.get(&source.key)
            {
                self.session.pause(&track.track)?;
            }
            if source.eos_epoch.is_some()
                && old.is_none_or(|old| old.eos_epoch != source.eos_epoch)
                && let Some(track) = self.tracks.get_mut(&source.key)
                && !track.eos
            {
                let sequence = track.channel.eos()?;
                track.eos = true;
                self.session.drain(&track.track)?;
                self.completions.push(MediaCompletion {
                    delivery_id: 0,
                    delivered: true,
                    sequence,
                    object_id: track.track.id(),
                });
            }
        }
        self.active_sources = current
            .iter()
            .cloned()
            .map(|source| (source.key, source))
            .collect();
        Ok(())
    }

    /// Recheck output readiness for a previously accepted PLAY intent.
    ///
    /// Track media and control replies travel on independent connections, so the query immediately
    /// following a successful media write may race the presenter's readiness update. The bridge
    /// worker calls this without blocking while the intent remains authoritative.
    pub fn retry_pending_playback(&mut self) -> io::Result<()> {
        let pending = self
            .active_sources
            .values()
            .filter(|source| {
                source.playing
                    && self
                        .tracks
                        .get(&source.key)
                        .is_some_and(|track| !track.playing)
            })
            .map(|source| source.key)
            .collect::<Vec<_>>();
        for key in pending {
            self.try_start_surface(key)?;
        }
        Ok(())
    }

    /// Recheck static-track readiness without assuming that a completed socket write has already
    /// been observed by the independently serviced outer control connection.
    pub fn retry_pending_activation(&mut self) -> io::Result<()> {
        let pending = self
            .tracks
            .iter()
            .filter_map(|(key, track)| {
                (!track.activated
                    && matches!(
                        track.kind,
                        BridgeSourceKind::Image { .. } | BridgeSourceKind::Raster { .. }
                    ))
                .then_some(*key)
            })
            .collect::<Vec<_>>();
        for key in pending {
            self.try_activate_static(key)?;
        }
        Ok(())
    }

    pub fn update_nodes(&mut self, nodes: &[BridgeNode]) -> io::Result<()> {
        self.reconcile_nodes(nodes)
    }

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
        let total = usize::try_from(total)
            .map_err(|_| invalid_data("media body length does not fit usize"))?;
        let pending = self.pending.entry(key).or_insert_with(|| PendingBody {
            record_type,
            total,
            received: 0,
            bytes: Vec::with_capacity(total),
        });
        if pending.record_type != record_type
            || pending.total != total
            || pending.received != offset as usize
        {
            self.pending.remove(&key);
            return Err(invalid_data("media chunk sequence gap"));
        }
        pending.received = pending
            .received
            .checked_add(bytes.len())
            .ok_or_else(|| invalid_data("media chunk length overflow"))?;
        if pending.received > pending.total {
            self.pending.remove(&key);
            return Err(invalid_data("media chunks exceed declared body length"));
        }
        pending.bytes.extend_from_slice(&bytes);
        if !last {
            return Ok(false);
        }
        let pending = self
            .pending
            .remove(&key)
            .ok_or_else(|| invalid_data("missing pending media body"))?;
        if pending.received != pending.total {
            return Err(invalid_data("incomplete media body"));
        }
        let (sequence, object_id) = self.forward_media(key, pending.record_type, &pending.bytes)?;
        self.completions.push(MediaCompletion {
            delivery_id,
            delivered: true,
            sequence,
            object_id,
        });
        self.poll_channel_events(key)?;
        if self
            .active_sources
            .get(&key)
            .is_some_and(|source| source.playing)
        {
            let _ = self.try_start_surface(key);
        }
        Ok(true)
    }

    pub fn take_media_completions(&mut self) -> Vec<(u64, bool, u64, u64)> {
        self.completions
            .drain(..)
            .map(|value| {
                (
                    value.delivery_id,
                    value.delivered,
                    value.sequence,
                    value.object_id,
                )
            })
            .collect()
    }

    pub fn source_for_outer_object(&self, object_id: u64) -> Option<BridgeSourceKey> {
        self.tracks
            .iter()
            .find_map(|(key, track)| (track.track.id() == object_id).then_some(*key))
    }

    pub fn take_keyframe_requests(&mut self) -> Vec<BridgeKeyframeRequest> {
        let keys = self.tracks.keys().copied().collect::<Vec<_>>();
        for key in keys {
            let _ = self.poll_channel_events(key);
        }
        std::mem::take(&mut self.keyframes)
    }

    pub fn take_full_frame_requests(&mut self) -> Vec<BridgeSourceKey> {
        let keys = self.tracks.keys().copied().collect::<Vec<_>>();
        for key in keys {
            let _ = self.poll_channel_events(key);
        }
        let mut values = self.full_frames.drain().collect::<Vec<_>>();
        values.sort_by_key(|key| (key.producer, key.context, key.surface, key.track));
        values
    }

    pub fn take_source_losses(&mut self) -> Vec<BridgeSourceKey> {
        let mut values = self.losses.drain().collect::<Vec<_>>();
        values.sort_by_key(|key| (key.producer, key.context, key.surface, key.track));
        values
    }

    pub fn take_playback_states(&mut self) -> Vec<(BridgeSourceKey, PlaybackSnapshot)> {
        self.poll_playback_progress();
        std::mem::take(&mut self.playback)
    }

    pub fn take_capability_changes(&mut self) -> Vec<CapabilityChange> {
        Vec::new()
    }

    pub fn take_control_wait_stats(&mut self) -> (u64, u64) {
        (0, 0)
    }

    fn create_outer_track(&mut self, source: &BridgeSource) -> io::Result<()> {
        let surface_key = surface_key(source);
        let surface = self
            .surfaces
            .get(&surface_key)
            .ok_or_else(|| invalid_data("outer surface was not created"))?
            .clone();
        let configuration = track_configuration(&self.session, &surface, source)?;
        let mut probe = configuration.clone();
        probe.track_id = 0;
        if !self.session.probe_track(&probe)?.supported {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "outer presenter rejected relayed track configuration",
            ));
        }
        let track = self
            .session
            .create_track(configuration, &RequestMetadata::default())?;
        let channel = Arc::new(self.session.open_track_channel(&track)?);
        self.tracks.insert(
            source.key,
            OuterTrack {
                surface_key,
                track,
                channel,
                kind: source.kind.clone(),
                next_media_id: 0,
                outer_epoch: 0,
                inner_epoch: 0,
                last_raster_id: 0,
                activated: false,
                playing: false,
                eos: false,
                reported_eos_state: 0,
            },
        );
        Ok(())
    }

    fn remove_track(&mut self, key: BridgeSourceKey) -> io::Result<()> {
        self.pending.remove(&key);
        if let Some(track) = self.tracks.remove(&key) {
            let _ = track.channel.close();
            self.session
                .destroy_track(&track.track, &RequestMetadata::default())?;
        }
        Ok(())
    }

    fn reconcile_surfaces(&mut self, desired: &[BridgeSurface]) -> io::Result<()> {
        for surface in desired {
            if let Some(existing) = self.surfaces.get(&surface.key).cloned() {
                let definition = surface_definition(&self.session, surface, Some(&existing))?;
                if existing.definition()? != definition {
                    self.session.update_surface(
                        &existing,
                        definition,
                        &RequestMetadata::default(),
                    )?;
                }
            } else {
                let definition = surface_definition(&self.session, surface, None)?;
                let outer = self
                    .session
                    .create_surface(definition, &RequestMetadata::default())?;
                self.surfaces.insert(surface.key, outer);
            }
        }
        Ok(())
    }

    fn remove_absent_surfaces(&mut self, desired: &[BridgeSurface]) -> io::Result<()> {
        let desired_keys = desired
            .iter()
            .map(|surface| surface.key)
            .collect::<HashSet<_>>();
        let removed = self
            .surfaces
            .keys()
            .copied()
            .filter(|key| !desired_keys.contains(key))
            .collect::<Vec<_>>();
        for key in removed {
            if let Some(surface) = self.surfaces.remove(&key) {
                self.session
                    .destroy_surface(&surface, &RequestMetadata::default())?;
            }
        }
        Ok(())
    }

    /// Read the outer control connection and apply what the bridge owns on it.
    ///
    /// Nothing else drains this connection, so the target generation the SDK names on every scene
    /// commit only follows the outer terminal from here. Returns whether the target moved.
    pub fn poll_outer_session(&mut self) -> bool {
        let mut moved = false;
        loop {
            match self.session.take_event() {
                Ok(Some(SessionEvent::TargetChanged(payload))) => {
                    match self.session.apply_target_changed(&payload) {
                        Ok(_) => moved = true,
                        Err(error) => {
                            log::debug!("ignored unusable outer TARGET_CHANGED: {error}")
                        }
                    }
                }
                Ok(Some(SessionEvent::TrackLost { payload, .. })) => {
                    // Matched on the complete context/surface/track identity: another context on
                    // this session may legitimately reuse the numeric track ID.
                    let field = |key: u64| payload_u64(&payload, key);
                    if let Some(key) = self.tracks.iter().find_map(|(key, track)| {
                        let configuration = track.track.configuration().ok()?;
                        (field(0) == Some(configuration.context_id)
                            && field(1) == Some(configuration.surface_id)
                            && field(2) == Some(configuration.track_id))
                        .then_some(*key)
                    }) {
                        self.losses.insert(key);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    log::debug!("outer control connection is unreadable: {error}");
                    break;
                }
            }
        }
        moved
    }

    /// Run one scene commit, following the outer presentation target while it moves.
    ///
    /// A commit names the target generation it was planned against, so an outer resize that lands
    /// between planning and committing is answered with `STALE_TARGET_GENERATION`. The presenter
    /// announces every change that causes one, so apply what has arrived and commit again against
    /// the target the outer terminal has now. A target that is still moving when the window closes
    /// is reported as `WouldBlock`, which asks for a fresh projection in this same outer session
    /// rather than tearing down healthy sources.
    fn commit_node_following_target(&mut self, commit: NodeCommit<'_>) -> io::Result<()> {
        let deadline = Instant::now() + TARGET_FOLLOW_TIMEOUT;
        loop {
            let metadata = RequestMetadata::default();
            let attempt = match commit {
                NodeCommit::Create(node) => self.session.create_node(node, &metadata).map(|_| ()),
                NodeCommit::Update(node) => self.session.update_node(node, &metadata).map(|_| ()),
                NodeCommit::Delete {
                    context_id,
                    node_id,
                } => self
                    .session
                    .delete_node(context_id, node_id, &metadata)
                    .map(|_| ()),
            };
            match attempt {
                Ok(()) => return Ok(()),
                Err(error)
                    if presenter_code(&error) == Some(registry::error::STALE_TARGET_GENERATION) =>
                {
                    if !self.poll_outer_session() {
                        if Instant::now() >= deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                format!("outer target is still moving: {error}"),
                            ));
                        }
                        thread::sleep(TARGET_FOLLOW_POLL);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn reconcile_nodes(&mut self, nodes: &[BridgeNode]) -> io::Result<()> {
        let desired = nodes
            .iter()
            .map(|node| ((node.producer, node.node, node.fragment), node))
            .collect::<HashMap<_, _>>();
        let removed = self
            .nodes
            .keys()
            .copied()
            .filter(|key| !desired.contains_key(key))
            .collect::<Vec<_>>();
        for key in removed {
            let Some((node_id, old)) = self.nodes.remove(&key) else {
                continue;
            };
            self.commit_node_following_target(NodeCommit::Delete {
                context_id: old.owning_context_id,
                node_id,
            })?;
        }
        for (stable, node) in desired {
            let surface = self
                .surfaces
                .get(&node.surface)
                .ok_or_else(|| invalid_data("scene node references a missing outer surface"))?;
            let node_id = self
                .nodes
                .get(&stable)
                .map(|(id, _)| *id)
                .unwrap_or(self.session.allocate_id()?);
            let replacement =
                scene_node(self.session.info().root_context_id, node_id, surface, node);
            match self.nodes.get(&stable) {
                Some((_, old)) if old == &replacement => {}
                Some(_) => {
                    self.commit_node_following_target(NodeCommit::Update(&replacement))?;
                    self.nodes.insert(stable, (node_id, replacement));
                }
                None => {
                    self.commit_node_following_target(NodeCommit::Create(&replacement))?;
                    self.nodes.insert(stable, (node_id, replacement));
                }
            }
        }
        Ok(())
    }

    fn forward_media(
        &mut self,
        key: BridgeSourceKey,
        record_type: u16,
        body: &[u8],
    ) -> io::Result<(u64, u64)> {
        let track = self
            .tracks
            .get_mut(&key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "outer track is missing"))?;
        if track.eos {
            return Err(invalid_data("media arrived after outer channel EOS"));
        }
        let object_id = track.track.id();
        let sequence = match record_type {
            messages::IMAGE_DATA => track.channel.send_image(body)?,
            messages::VIDEO_PACKET => {
                let packet = media::parse_video_packet(body)?;
                let (epoch, id) = next_outer_identity(track, packet.epoch)?;
                track.channel.send_video(VideoPacket {
                    epoch,
                    packet_id: id,
                    pts_us: packet.pts_us,
                    dts_us: packet.dts_us,
                    duration_us: packet.duration_us,
                    key: packet.flags & media::VIDEO_PACKET_KEY != 0,
                    data: packet.data,
                })?
            }
            messages::AUDIO_PACKET => {
                let packet = media::parse_audio_packet(body)?;
                let (epoch, id) = next_outer_identity(track, packet.epoch)?;
                track.channel.send_audio(AudioPacket {
                    epoch,
                    packet_id: id,
                    pts_us: packet.pts_us,
                    dts_us: packet.dts_us,
                    duration_us: packet.duration_us,
                    trim_start_samples: packet.trim_start_samples,
                    trim_end_samples: packet.trim_end_samples,
                    data: packet.data,
                })?
            }
            messages::RASTER_FRAME => {
                let flags = body
                    .get(4..8)
                    .and_then(|value| value.try_into().ok())
                    .map(u32::from_be_bytes)
                    .ok_or_else(|| invalid_data("raster header is truncated"))?;
                if flags & media::RASTER_FRAME_DELTA == 0 {
                    let frame = media::parse_full_raster_frame(body)?;
                    let pixels = media::decode_raster_pixels(frame)?;
                    let (epoch, id) = next_outer_identity(track, frame.epoch)?;
                    track.last_raster_id = id;
                    track.channel.send_raster(epoch, id, &pixels, false)?
                } else {
                    let (width, height, limit) = match &track.kind {
                        BridgeSourceKind::Raster {
                            width,
                            height,
                            delta_operation_limit: Some(limit),
                            ..
                        } => (*width, *height, *limit),
                        _ => {
                            self.full_frames.insert(key);
                            return Err(invalid_data(
                                "raster delta arrived for a non-delta outer track",
                            ));
                        }
                    };
                    let frame = media::parse_delta_raster_frame(body, width, height, limit)?;
                    if track.last_raster_id == 0 {
                        self.full_frames.insert(key);
                        return Err(invalid_data("outer raster delta has no reusable base"));
                    }
                    let (epoch, id) = next_outer_identity(track, frame.epoch)?;
                    let operations = frame
                        .operations
                        .iter()
                        .map(parsed_delta_operation)
                        .collect::<Vec<_>>();
                    let sequence = track.channel.send_raster_delta(
                        epoch,
                        id,
                        track.last_raster_id,
                        frame.pts_us,
                        frame.duration_us,
                        &operations,
                        false,
                    )?;
                    track.last_raster_id = id;
                    sequence
                }
            }
            _ => return Err(invalid_data("unsupported relayed media record type")),
        };
        let needs_static_activation = matches!(
            track.kind,
            BridgeSourceKind::Image { .. } | BridgeSourceKind::Raster { .. }
        ) && !track.activated;
        if needs_static_activation {
            self.try_activate_static(key)?;
        }
        Ok((sequence, object_id))
    }

    fn try_activate_static(&mut self, key: BridgeSourceKey) -> io::Result<()> {
        let Some(track) = self.tracks.get(&key) else {
            return Ok(());
        };
        if track.activated
            || !matches!(
                track.kind,
                BridgeSourceKind::Image { .. } | BridgeSourceKind::Raster { .. }
            )
        {
            return Ok(());
        }
        let status = self.session.query_track(&track.track)?;
        if status.milestones & vivid_sdk::MILESTONE_OUTPUT_READY == 0 {
            return Ok(());
        }
        let surface = self
            .surfaces
            .get(&track.surface_key)
            .ok_or_else(|| invalid_data("outer visual surface is missing"))?;
        self.session.activate_tracks(
            surface,
            &[SlotBinding {
                slot: slot_for_kind(&track.kind),
                track_id: track.track.id(),
                expected_channel_generation: track.track.channel_generation(),
                required_milestone: vivid_sdk::MILESTONE_OUTPUT_READY,
            }],
            &RequestMetadata::default(),
        )?;
        if let Some(track) = self.tracks.get_mut(&key) {
            track.activated = true;
        }
        Ok(())
    }

    fn try_start_surface(&mut self, key: BridgeSourceKey) -> io::Result<()> {
        let surface_key = self
            .tracks
            .get(&key)
            .map(|track| track.surface_key)
            .ok_or_else(|| invalid_data("playback track is missing"))?;
        let member_keys = self
            .tracks
            .iter()
            .filter_map(|(candidate, track)| {
                (track.surface_key == surface_key).then_some(*candidate)
            })
            .collect::<Vec<_>>();
        let mut bindings = Vec::new();
        let mut clock = key;
        for member in member_keys {
            let track = &self.tracks[&member];
            if !matches!(
                track.kind,
                BridgeSourceKind::Video { .. } | BridgeSourceKind::Audio { .. }
            ) {
                continue;
            }
            let source_playing = self
                .active_sources
                .get(&member)
                .is_some_and(|source| source.playing)
                || matches!(
                    &track.kind,
                    BridgeSourceKind::Audio {
                        linked_video: Some(video),
                        ..
                    } if self.active_sources.get(video).is_some_and(|source| source.playing)
                );
            if !source_playing {
                continue;
            }
            let status = self.session.query_track(&track.track)?;
            if status.milestones & vivid_sdk::MILESTONE_OUTPUT_READY == 0 {
                // Readiness is driven by media on an independent connection. Keep the intent and
                // retry after the next track delivery instead of blocking the bridge worker.
                return Ok(());
            }
            bindings.push(SlotBinding {
                slot: if matches!(track.kind, BridgeSourceKind::Audio { .. }) {
                    clock = member;
                    SLOT_AUDIO
                } else {
                    slot_for_kind(&track.kind)
                },
                track_id: track.track.id(),
                expected_channel_generation: track.track.channel_generation(),
                required_milestone: vivid_sdk::MILESTONE_OUTPUT_READY,
            });
        }
        if bindings.is_empty() {
            return Ok(());
        }
        let already_playing = self.tracks.get(&clock).is_some_and(|track| track.playing);
        if already_playing {
            return Ok(());
        }
        let surface = self
            .surfaces
            .get(&surface_key)
            .ok_or_else(|| invalid_data("outer playback surface is missing"))?;
        self.session
            .activate_tracks(surface, &bindings, &RequestMetadata::default())?;
        let request = self
            .active_sources
            .get(&key)
            .map(|source| source.play_request)
            .unwrap_or_else(default_play_request);
        let clock_track = self
            .tracks
            .get(&clock)
            .ok_or_else(|| invalid_data("outer playback clock is missing"))?
            .track
            .clone();
        self.session.play(
            &clock_track,
            request.start_pts_us,
            request.minimum_buffer_us.max(1),
            request.maximum_latency_us.max(1),
        )?;
        for binding in bindings {
            if let Some((_, track)) = self
                .tracks
                .iter_mut()
                .find(|(_, track)| track.track.id() == binding.track_id)
            {
                track.activated = true;
                track.playing = true;
            }
        }
        self.playback.push((
            key,
            PlaybackSnapshot {
                state: 2,
                eos_state: 0,
            },
        ));
        Ok(())
    }

    fn poll_channel_events(&mut self, key: BridgeSourceKey) -> io::Result<()> {
        let Some(track) = self.tracks.get(&key) else {
            return Ok(());
        };
        while let Some(event) = track.channel.take_event()? {
            match event {
                ChannelEvent::NeedKeyframe(payload) => {
                    self.keyframes.push(BridgeKeyframeRequest {
                        source: key,
                        minimum_epoch: payload_u64(&payload, 4)
                            .and_then(|value| u32::try_from(value).ok()),
                        reason: payload_u64(&payload, 5).unwrap_or(KEYFRAME_REASON_DECODER_ERROR),
                    });
                }
                ChannelEvent::NeedFullFrame(_) => {
                    self.full_frames.insert(key);
                }
                ChannelEvent::Error(_) => {
                    self.losses.insert(key);
                }
            }
        }
        Ok(())
    }

    fn poll_playback_progress(&mut self) {
        let candidates = self
            .tracks
            .iter()
            .filter(|(_, track)| track.eos)
            .map(|(key, track)| {
                (
                    *key,
                    track.track.clone(),
                    track.playing,
                    track.reported_eos_state,
                )
            })
            .collect::<Vec<_>>();
        for (key, track, playing, previous_eos) in candidates {
            let Ok(status) = self.session.query_track(&track) else {
                continue;
            };
            let eos_state = if status.milestones & vivid_sdk::MILESTONE_BUFFERED_ENDED != 0 {
                2
            } else if status.milestones & vivid_sdk::MILESTONE_EOS_ACCEPTED != 0 {
                1
            } else {
                0
            };
            if eos_state <= previous_eos {
                continue;
            }
            if let Some(current) = self.tracks.get_mut(&key) {
                current.reported_eos_state = eos_state;
            }
            self.playback.push((
                key,
                PlaybackSnapshot {
                    state: if playing { 2 } else { 1 },
                    eos_state,
                },
            ));
        }
    }
}

impl Drop for OuterBridge {
    fn drop(&mut self) {
        for track in self.tracks.values() {
            let _ = track.channel.close();
        }
    }
}

fn producer_config(
    endpoint_control: Option<String>,
    endpoint_realtime: Option<String>,
    endpoint_bulk: Option<String>,
    root_secret: &Secret32,
) -> ProducerConfig {
    ProducerConfig {
        endpoint_control,
        endpoint_realtime,
        endpoint_bulk,
        authentication: ProducerAuthentication::Root {
            root_secret: Secret32::new(*root_secret.expose()),
        },
        producer_name: "vvmux".into(),
        producer_version: env!("CARGO_PKG_VERSION").into(),
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

fn parse_root_secret(value: &str) -> io::Result<Secret32> {
    Secret32::from_hex(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("VIVID_ROOT_SECRET is invalid: {error}"),
        )
    })
}

fn display_from_target(
    session: &vivid_sdk::Session,
    fallback: DisplayMetrics,
) -> io::Result<DisplayMetrics> {
    let payload = &session.info().target_descriptor;
    let read = |key| payload_u64(payload, key);
    let columns = read(2).and_then(|value| u16::try_from(value).ok());
    let rows = read(3).and_then(|value| u16::try_from(value).ok());
    let cell_width = read(4).and_then(|value| u16::try_from(value).ok());
    let cell_height = read(5).and_then(|value| u16::try_from(value).ok());
    match (columns, rows, cell_width, cell_height) {
        (Some(columns), Some(rows), Some(cell_width), Some(cell_height))
            if columns > 0 && rows > 0 && cell_width > 0 && cell_height > 0 =>
        {
            Ok(DisplayMetrics {
                columns,
                rows,
                cell_width,
                cell_height,
            })
        }
        _ if fallback.columns > 0
            && fallback.rows > 0
            && fallback.cell_width > 0
            && fallback.cell_height > 0 =>
        {
            Ok(fallback)
        }
        _ => Err(invalid_data(
            "outer WELCOME has an invalid terminal target descriptor",
        )),
    }
}

fn surface_key(source: &BridgeSource) -> BridgeSurfaceKey {
    BridgeSurfaceKey {
        producer: source.key.producer,
        context: source.key.context,
        surface: source.key.surface,
    }
}

fn surface_definition(
    session: &vivid_sdk::Session,
    surface: &BridgeSurface,
    existing: Option<&Surface>,
) -> io::Result<SurfaceDefinition> {
    let descriptor = &surface.descriptor;
    Ok(SurfaceDefinition {
        context_id: session.info().root_context_id,
        surface_id: match existing {
            Some(existing) => existing.id(),
            None => session.allocate_id()?,
        },
        semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
        coordinate_model: CoordinateModel::DesktopLogicalPixels,
        logical_width: surface.logical_width,
        logical_height: surface.logical_height,
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
        descriptor: SurfaceDescriptor {
            role: SurfaceRole::try_from(descriptor.role)
                .ok()
                .unwrap_or(SurfaceRole::Figure),
            title: if descriptor.title.is_empty() {
                {
                    format!(
                        "nested surface {}:{}:{}",
                        surface.key.producer, surface.key.context, surface.key.surface
                    )
                }
            } else {
                descriptor.title.clone()
            },
            semantic_content_revision: descriptor.content_revision,
            semantic_availability: descriptor.semantic_availability,
            locator_hint: descriptor.locator.clone(),
        },
        policy: surface.capture_policy,
        profile_parameters: vec![],
    })
}

fn track_configuration(
    session: &vivid_sdk::Session,
    surface: &Surface,
    source: &BridgeSource,
) -> io::Result<TrackConfiguration> {
    let (kind, mode, lane, maximum_record_body, rate, bits, records, inflight, pixels) =
        match &source.kind {
            BridgeSourceKind::Raster {
                width,
                height,
                alpha_mode,
                compression_mode,
                delta_operation_limit,
            } => {
                let body =
                    media::rgba8_raw_frame_body_len(*width, *height).map_err(io::Error::other)?;
                (
                    KindConfiguration::Raster(RasterConfiguration {
                        width: *width,
                        height: *height,
                        alpha_mode: *alpha_mode,
                        delta_enabled: delta_operation_limit.is_some(),
                        maximum_delta_operations: delta_operation_limit
                            .and_then(|value| u8::try_from(value).ok())
                            .unwrap_or(1),
                        zstd_enabled: *compression_mode != 0,
                    }),
                    TrackMode::Live,
                    LaneClass::Bulk,
                    body,
                    120_000,
                    u64::from(body).saturating_mul(8).saturating_mul(120),
                    120,
                    u64::from(body).saturating_mul(2),
                    u64::from(*width).saturating_mul(u64::from(*height)),
                )
            }
            BridgeSourceKind::Image {
                encoding,
                width,
                height,
                encoded_length,
                sha256,
            } => (
                KindConfiguration::EncodedImage(ImageConfiguration {
                    encoding: *encoding,
                    width: *width,
                    height: *height,
                    encoded_length: *encoded_length,
                    sha256: *sha256,
                    cache_lookup: false,
                }),
                TrackMode::Live,
                LaneClass::Bulk,
                *encoded_length,
                1_000,
                u64::from(*encoded_length).saturating_mul(8),
                1,
                u64::from(*encoded_length),
                u64::from(*width).saturating_mul(u64::from(*height)),
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
            } => {
                let body =
                    media::video_body_len(*max_access_unit_bytes).map_err(io::Error::other)?;
                (
                    KindConfiguration::Video(VideoConfiguration {
                        codec: codec.clone(),
                        packetization: packetization.clone(),
                        extradata: extradata.clone(),
                        coded_width: *width,
                        coded_height: *height,
                        profile: *profile,
                        level: *level,
                        maximum_reorder_depth: 16,
                        color_primaries: *color_primaries,
                        transfer: *transfer,
                        matrix: *matrix,
                        signal_range: *range,
                        aspect_numerator: u64::from(*sar_num),
                        aspect_denominator: u64::from(*sar_den),
                        maximum_access_unit_bytes: *max_access_unit_bytes,
                        codec_string: codec_string.clone(),
                        decoder_configuration: decoder_config.clone(),
                    }),
                    TrackMode::Timed,
                    LaneClass::Realtime,
                    body,
                    240_000,
                    (*bitrate).max(u64::from(body).saturating_mul(8)),
                    240,
                    u64::from(body).saturating_mul(16),
                    u64::from(*width).saturating_mul(u64::from(*height)),
                )
            }
            BridgeSourceKind::Audio {
                codec,
                packetization,
                extradata,
                sample_rate,
                channels,
                channel_mask,
                bitrate,
                max_access_unit_bytes,
                codec_string,
                ..
            } => {
                let body =
                    media::audio_body_len(*max_access_unit_bytes).map_err(io::Error::other)?;
                (
                    KindConfiguration::Audio(AudioConfiguration {
                        codec: codec.clone(),
                        packetization: packetization.clone(),
                        extradata: extradata.clone(),
                        sample_rate: *sample_rate,
                        channels: u8::try_from(*channels)
                            .map_err(|_| invalid_data("audio channel count exceeds u8"))?,
                        channel_mask: *channel_mask,
                        maximum_access_unit_bytes: *max_access_unit_bytes,
                        codec_string: codec_string.clone(),
                    }),
                    TrackMode::Timed,
                    LaneClass::Realtime,
                    body,
                    1_000_000,
                    (*bitrate).max(u64::from(body).saturating_mul(8)),
                    1_000,
                    u64::from(body).saturating_mul(64),
                    0,
                )
            }
        };
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: session.allocate_id()?,
        slot: match &kind {
            KindConfiguration::Video(_) => SLOT_VIDEO,
            KindConfiguration::Audio(_) => SLOT_AUDIO,
            KindConfiguration::Raster(_) => SLOT_RASTER,
            KindConfiguration::EncodedImage(_) => SLOT_POSTER,
        },
        mode,
        lane,
        maximum_record_body,
        maximum_rate_millihertz: rate,
        maximum_encoded_bits_per_second: bits.max(1),
        maximum_records_per_second: records,
        maximum_inflight_body_bytes: inflight.max(u64::from(maximum_record_body)),
        kind,
        target_latency_us: if mode == TrackMode::Timed { 20_000 } else { 0 },
        maximum_latency_us: if mode == TrackMode::Timed {
            1_000_000
        } else {
            100_000
        },
        retained_pixel_charge: pixels,
    })
}

fn slot_for_kind(kind: &BridgeSourceKind) -> u64 {
    match kind {
        BridgeSourceKind::Video { .. } => SLOT_VIDEO,
        BridgeSourceKind::Audio { .. } => SLOT_AUDIO,
        BridgeSourceKind::Raster { .. } => SLOT_RASTER,
        BridgeSourceKind::Image { .. } => SLOT_POSTER,
    }
}

fn scene_node(
    root_context: u64,
    node_id: u64,
    surface: &Surface,
    source: &BridgeNode,
) -> SceneNode {
    SceneNode {
        owning_context_id: root_context,
        node_id,
        surface_context_id: surface.context_id(),
        surface_id: surface.id(),
        geometry: vec![
            (0, Value::Unsigned(1)),
            (1, signed(source.x)),
            (2, signed(source.y)),
            (3, signed(source.width)),
            (4, signed(source.height)),
            (5, Value::Unsigned(1)),
        ],
        fit: Fit::Contain,
        linear_sampling: true,
        z_index: source.z_index,
        visible: source.visible,
        opacity: u16::MAX,
        clip: Some(vec![
            (0, signed(source.clip.x)),
            (1, signed(source.clip.y)),
            (2, signed(source.clip.width)),
            (3, signed(source.clip.height)),
        ]),
    }
}

fn next_outer_identity(track: &mut OuterTrack, inner_epoch: u32) -> io::Result<(u32, u64)> {
    if track.outer_epoch == 0 {
        track.outer_epoch = 1;
        track.inner_epoch = inner_epoch;
    } else if inner_epoch > track.inner_epoch {
        track.outer_epoch = track
            .outer_epoch
            .checked_add(1)
            .ok_or_else(|| io::Error::other("outer media epoch exhausted"))?;
        track.inner_epoch = inner_epoch;
        track.last_raster_id = 0;
    } else if inner_epoch < track.inner_epoch {
        return Err(invalid_data("inner media epoch moved backward"));
    }
    track.next_media_id = track
        .next_media_id
        .checked_add(1)
        .ok_or_else(|| io::Error::other("outer media ID exhausted"))?;
    Ok((track.outer_epoch, track.next_media_id))
}

fn parsed_delta_operation<'a>(
    operation: &'a media::ParsedRasterDeltaOperation<'a>,
) -> RasterDeltaOperation<'a> {
    match operation {
        media::ParsedRasterDeltaOperation::Overwrite {
            x,
            y,
            width,
            height,
            rgba,
        } => RasterDeltaOperation::Overwrite {
            x: *x,
            y: *y,
            width: *width,
            height: *height,
            rgba: match rgba {
                Cow::Borrowed(value) => value,
                Cow::Owned(value) => value.as_slice(),
            },
        },
        media::ParsedRasterDeltaOperation::Copy {
            destination_x,
            destination_y,
            width,
            height,
            source_x,
            source_y,
        } => RasterDeltaOperation::Copy {
            destination_x: *destination_x,
            destination_y: *destination_y,
            width: *width,
            height: *height,
            source_x: *source_x,
            source_y: *source_y,
        },
    }
}

/// The registered error code behind a presenter rejection, if the failure came from the presenter.
fn presenter_code(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<vivid_sdk::PresenterError>())
        .map(|error| error.code)
}

fn payload_u64(payload: &[(u64, Value)], key: u64) -> Option<u64> {
    payload
        .iter()
        .find_map(|(candidate, value)| (*candidate == key).then(|| value.as_u64()).flatten())
}

fn validate_snapshot(
    surfaces: &[BridgeSurface],
    sources: &[BridgeSource],
    nodes: &[BridgeNode],
) -> io::Result<()> {
    let surface_keys = surfaces
        .iter()
        .map(|surface| surface.key)
        .collect::<HashSet<_>>();
    if surface_keys.len() != surfaces.len()
        || surfaces.iter().any(|surface| {
            surface.key.producer == 0
                || surface.key.context == 0
                || surface.key.surface == 0
                || surface.logical_width == 0
                || surface.logical_height == 0
        })
    {
        return Err(invalid_data(
            "bridge snapshot contains duplicate or incomplete surface identity",
        ));
    }
    let keys = sources
        .iter()
        .map(|source| source.key)
        .collect::<HashSet<_>>();
    if keys.len() != sources.len()
        || keys
            .iter()
            .any(|key| key.producer == 0 || key.context == 0 || key.surface == 0 || key.track == 0)
    {
        return Err(invalid_data(
            "bridge snapshot contains duplicate or incomplete track identity",
        ));
    }
    for source in sources {
        if !surface_keys.contains(&surface_key(source)) {
            return Err(invalid_data("bridge track references a missing surface"));
        }
        if let BridgeSourceKind::Audio {
            linked_video: Some(video),
            ..
        } = source.kind
            && (!keys.contains(&video)
                || video.producer != source.key.producer
                || video.context != source.key.context
                || video.surface != source.key.surface)
        {
            return Err(invalid_data(
                "linked audio references a missing or foreign video track",
            ));
        }
    }
    let mut node_keys = HashSet::new();
    for node in nodes {
        if !surface_keys.contains(&node.surface)
            || !node_keys.insert((node.producer, node.node, node.fragment))
            || node.width <= 0
            || node.height <= 0
            || node.clip.width <= 0
            || node.clip.height <= 0
        {
            return Err(invalid_data(
                "bridge snapshot contains an invalid scene node",
            ));
        }
    }
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn signed(value: i64) -> Value {
    if value >= 0 {
        Value::Unsigned(value as u64)
    } else {
        Value::Negative(value)
    }
}

fn default_play_request() -> crate::ipc::BridgePlayRequest {
    crate::ipc::BridgePlayRequest {
        start_pts_us: 0,
        minimum_buffer_us: 1,
        maximum_latency_us: 1_000_000,
        rate_32_32: 1_i64 << 32,
        late_policy: 1,
        loop_count: 0,
        start_policy: 1,
    }
}
