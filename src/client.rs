use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use zeroize::Zeroizing;

#[cfg(test)]
use crate::client_input::parse_configured_action;
use crate::client_input::{FloatEditScanner, ParsedInput, PrefixParser};
#[cfg(all(test, unix))]
use crate::ipc::DisplayMetrics;
#[cfg(test)]
use crate::ipc::{Action, Axis, Direction, MouseEvent, MouseKind};
use crate::ipc::{
    BridgeNode, BridgeSource, BridgeSourceKey, BridgeSourceKind, ClientMessage,
    FloatingEditCommand, ServerMessage, SharedWriter,
};
use crate::platform::ClientTerminal;

const BRIDGE_MEDIA_CHUNK: usize = 128 * 1024;

pub fn attach(
    name: &str,
    replace: bool,
    create: bool,
    config_path: Option<&Path>,
) -> io::Result<()> {
    let client_config = crate::config::Config::load(config_path)?;
    let (mut reader, writer) = match crate::server::connect(name) {
        Ok(connection) => connection,
        Err(error) if create && is_missing_session(&error) => {
            spawn_server(name, config_path)?;
            wait_for_server(name)?
        }
        Err(error) => return Err(error),
    };

    // Keep outer credentials exclusively in the foreground process. Zeroizing guarantees token
    // bytes are overwritten when the Vivid bridge or this text-only client closes.
    let outer_endpoint = std::env::var("VIVID_ENDPOINT").ok();
    let outer_bulk_endpoint = std::env::var("VIVID_ENDPOINT_BULK").ok();
    let outer_token = std::env::var("VIVID_TOKEN").ok().map(Zeroizing::new);
    let vivid = outer_endpoint.is_some() && outer_token.is_some();
    let terminal = ClientTerminal::enter()?;
    let display = terminal.display_metrics()?;
    send_client(
        &writer,
        &ClientMessage::Attach {
            replace,
            display,
            vivid,
        },
    )?;

    let stopped = Arc::new(AtomicBool::new(false));
    let ipc_cancel = reader.cancel_handle();
    let read_stopped = stopped.clone();
    let read_writer = writer.clone();
    // Authoritative float-edit mode state travels from the reader thread to the input loop
    // through this bounded channel; edit keys are parsed only while a confirmed mode is active.
    let (mode_sender, mode_receiver) = mpsc::sync_channel::<(u64, bool)>(8);
    let output = terminal.output()?;
    let output = Arc::new(Mutex::new(output));
    let output_thread = output.clone();
    let bridge_display = display;
    let bridge_queue_records =
        (client_config.media.ipc_queue_bytes / BRIDGE_MEDIA_CHUNK).clamp(1, 1024);
    let reader_thread = thread::Builder::new()
        .name("vvmux-render".into())
        .spawn(move || {
            let mut bridge = match (outer_endpoint, outer_token) {
                (Some(endpoint), Some(token)) => {
                    match crate::bridge::OuterBridge::connect_with_bulk(
                        endpoint,
                        outer_bulk_endpoint,
                        token,
                        bridge_display,
                    ) {
                        Ok(bridge) => match BridgeWorker::spawn(
                            bridge,
                            read_writer.clone(),
                            bridge_queue_records,
                        ) {
                            Ok(worker) => Some(worker),
                            Err(error) => {
                                let _ = write_title(
                                    &output_thread,
                                    &format!("vvmux media disabled: {error}"),
                                );
                                None
                            }
                        },
                        Err(error) => {
                            let _ = write_title(
                                &output_thread,
                                &format!("vvmux media disabled: {error}"),
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            while let Ok(message) = reader.recv::<ServerMessage>() {
                match message {
                    ServerMessage::Attached { text_only, .. } => {
                        if text_only {
                            let _ = write_title(&output_thread, "vvmux (text-only media fallback)");
                        }
                    }
                    ServerMessage::Render {
                        frame_id,
                        last,
                        bytes,
                        ..
                    } => {
                        if write_output(&output_thread, &bytes).is_err() {
                            break;
                        }
                        if last {
                            let _ = send_client(&read_writer, &ClientMessage::RenderAck(frame_id));
                        }
                    }
                    ServerMessage::Title(title) => {
                        let _ = write_title(&output_thread, &title);
                    }
                    ServerMessage::Bell => {
                        let _ = write_output(&output_thread, b"\x07");
                    }
                    ServerMessage::Clipboard(text) => {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
                        let operation = format!("\x1b]52;c;{encoded}\x1b\\");
                        let _ = write_output(&output_thread, operation.as_bytes());
                    }
                    ServerMessage::Status(status) => {
                        let _ = write_title(&output_thread, &format!("vvmux: {status}"));
                    }
                    ServerMessage::MediaSnapshot {
                        revision,
                        sources,
                        nodes,
                        videos_needing_keyframes,
                    } => {
                        if let Some(bridge) = &mut bridge {
                            bridge.replace_snapshot(BridgeSnapshot {
                                generation: 0,
                                virtual_revision: revision,
                                sources,
                                nodes,
                                videos_needing_keyframes,
                            });
                        }
                    }
                    ServerMessage::MediaRecord {
                        delivery_id,
                        source,
                        record_type,
                        offset,
                        total,
                        last,
                        bytes,
                    } => {
                        if let Some(bridge) = &mut bridge {
                            bridge.queue_media(BridgeMedia {
                                generation: 0,
                                delivery_id,
                                source,
                                record_type,
                                offset,
                                total,
                                last,
                                bytes,
                            });
                        }
                    }
                    ServerMessage::FloatingEditMode { mode_id, pane, .. } => {
                        // Preserve every actor-authoritative transition. The input loop polls at
                        // most 100 ms, so this bounded send cannot turn into unbounded reader
                        // backpressure, while `try_send` could silently strand the client in a
                        // stale edit mode.
                        let _ = mode_sender.send((mode_id, pane.is_some()));
                    }
                    ServerMessage::Detached { .. } | ServerMessage::Error(_) => break,
                    ServerMessage::Pong => {}
                    ServerMessage::Automation(_) | ServerMessage::AutomationChunk { .. } => break,
                }
            }
            read_stopped.store(true, Ordering::Release);
        })?;

    #[cfg(unix)]
    let signal_stopped = stopped.clone();
    #[cfg(unix)]
    let mut signals =
        match signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM, libc::SIGHUP]) {
            Ok(signals) => signals,
            Err(error) => {
                ipc_cancel.cancel();
                let _ = reader_thread.join();
                return Err(error);
            }
        };
    #[cfg(unix)]
    let signal_handle = signals.handle();
    #[cfg(unix)]
    let signal_thread = match thread::Builder::new()
        .name("vvmux-client-signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                signal_stopped.store(true, Ordering::Release);
            }
        }) {
        Ok(thread) => thread,
        Err(error) => {
            signal_handle.close();
            ipc_cancel.cancel();
            let _ = reader_thread.join();
            return Err(error);
        }
    };

    let mut workers = ClientWorkers {
        ipc_cancel,
        reader_thread: Some(reader_thread),
        #[cfg(unix)]
        signal_handle,
        #[cfg(unix)]
        signal_thread: Some(signal_thread),
    };

    let mut parser = PrefixParser::new(
        crate::config::parse_control_chord(&client_config.general.prefix).unwrap_or(0x02),
        &client_config.keys.prefix,
    );
    let mut last_display = display;
    let mut float_mode: Option<u64> = None;
    let mut float_scanner = FloatEditScanner::default();
    let result = (|| -> io::Result<()> {
        while !stopped.load(Ordering::Acquire) {
            while let Ok((mode_id, active)) = mode_receiver.try_recv() {
                if active {
                    float_mode = Some(mode_id);
                    float_scanner.reset();
                } else if float_mode == Some(mode_id) {
                    float_mode = None;
                    let residue = float_scanner.reset();
                    if !residue.is_empty() {
                        send_client(&writer, &ClientMessage::Input(residue))?;
                    }
                }
            }
            let mut bytes = [0_u8; 4096];
            if let Some(read) = terminal.read_input(&mut bytes, Duration::from_millis(100))? {
                if read == 0 {
                    let _ = send_client(&writer, &ClientMessage::Detach);
                    break;
                }
                let parsed = if let Some(mode_id) = float_mode {
                    // Scan edit keys before the ordinary prefix/mouse parser. In particular, that
                    // parser buffers ESC while deciding whether it begins SGR mouse input, which
                    // would otherwise make a bare Escape unable to cancel the modal edit.
                    let (commands, forward) = float_scanner.scan(&bytes[..read]);
                    for command in commands {
                        send_client(&writer, &ClientMessage::FloatingEdit { mode_id, command })?;
                        if matches!(
                            command,
                            FloatingEditCommand::Commit | FloatingEditCommand::Cancel
                        ) {
                            float_mode = None;
                        }
                    }
                    parser.feed(&forward)
                } else {
                    parser.feed(&bytes[..read])
                };
                for command in parsed {
                    match command {
                        ParsedInput::Input(bytes) => {
                            send_client(&writer, &ClientMessage::Input(bytes))?
                        }
                        ParsedInput::Action(action) => {
                            send_client(&writer, &ClientMessage::Action(action))?
                        }
                        ParsedInput::Mouse(mouse) => {
                            send_client(&writer, &ClientMessage::Mouse(mouse))?
                        }
                        ParsedInput::Detach => {
                            send_client(&writer, &ClientMessage::Detach)?;
                            stopped.store(true, Ordering::Release);
                        }
                    }
                }
            } else if let Some(mode_id) = float_mode
                && let Some(command) = float_scanner.expire(Instant::now())
            {
                send_client(&writer, &ClientMessage::FloatingEdit { mode_id, command })?;
                float_mode = None;
            }
            if let Ok(display) = terminal.display_metrics()
                && display != last_display
            {
                send_client(&writer, &ClientMessage::Resize(display))?;
                last_display = display;
            }
        }
        Ok(())
    })();
    workers.stop();
    drop(terminal);
    result
}

struct ClientWorkers {
    ipc_cancel: crate::platform::ConnectionCancel,
    reader_thread: Option<thread::JoinHandle<()>>,
    #[cfg(unix)]
    signal_handle: signal_hook::iterator::Handle,
    #[cfg(unix)]
    signal_thread: Option<thread::JoinHandle<()>>,
}

impl ClientWorkers {
    fn stop(&mut self) {
        self.ipc_cancel.cancel();
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        #[cfg(unix)]
        {
            self.signal_handle.close();
            if let Some(thread) = self.signal_thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for ClientWorkers {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) struct BridgeSnapshot {
    pub(crate) generation: u64,
    pub(crate) virtual_revision: u64,
    pub(crate) sources: Vec<BridgeSource>,
    pub(crate) nodes: Vec<BridgeNode>,
    pub(crate) videos_needing_keyframes: Vec<BridgeSourceKey>,
}

pub(crate) struct BridgeMedia {
    pub(crate) generation: u64,
    pub(crate) delivery_id: u64,
    pub(crate) source: BridgeSourceKey,
    pub(crate) record_type: u16,
    pub(crate) offset: u32,
    pub(crate) total: u32,
    pub(crate) last: bool,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct BridgeClientSender(
    Arc<dyn Fn(ClientMessage) -> io::Result<()> + Send + Sync + 'static>,
);

impl BridgeClientSender {
    pub(crate) fn new(
        send: impl Fn(ClientMessage) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(send))
    }

    fn send(&self, message: ClientMessage) -> io::Result<()> {
        (self.0)(message)
    }
}

pub(crate) struct BridgeWorker {
    media: mpsc::SyncSender<BridgeMedia>,
    snapshot: Arc<Mutex<Option<BridgeSnapshot>>>,
    dropped: Arc<Mutex<HashSet<u64>>>,
    generation: u64,
}

impl BridgeWorker {
    fn spawn(
        bridge: crate::bridge::OuterBridge,
        client_writer: SharedWriter,
        queue_records: usize,
    ) -> io::Result<Self> {
        Self::spawn_with_sender(
            bridge,
            BridgeClientSender::new(move |message| send_client(&client_writer, &message)),
            queue_records,
        )
    }

    pub(crate) fn spawn_with_sender(
        bridge: crate::bridge::OuterBridge,
        client_writer: BridgeClientSender,
        queue_records: usize,
    ) -> io::Result<Self> {
        let (media, receiver) = mpsc::sync_channel(queue_records);
        let snapshot = Arc::new(Mutex::new(None));
        let dropped = Arc::new(Mutex::new(HashSet::new()));
        let worker_snapshot = snapshot.clone();
        let worker_dropped = dropped.clone();
        thread::Builder::new()
            .name("vvmux-media-bridge".into())
            .spawn(move || {
                run_bridge_worker(
                    bridge,
                    client_writer,
                    receiver,
                    worker_snapshot,
                    worker_dropped,
                )
            })?;
        Ok(Self {
            media,
            snapshot,
            dropped,
            generation: 0,
        })
    }

    pub(crate) fn replace_snapshot(&mut self, mut snapshot: BridgeSnapshot) {
        self.generation = self.generation.wrapping_add(1);
        snapshot.generation = self.generation;
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
    }

    pub(crate) fn queue_media(&mut self, mut media: BridgeMedia) -> bool {
        media.generation = self.generation;
        let delivery_id = media.delivery_id;
        match self.media.try_send(media) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                self.mark_dropped(delivery_id);
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.mark_dropped(delivery_id);
                false
            }
        }
    }

    fn mark_dropped(&self, delivery_id: u64) {
        self.dropped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(delivery_id);
    }
}

fn run_bridge_worker(
    mut bridge: crate::bridge::OuterBridge,
    client_writer: BridgeClientSender,
    receiver: mpsc::Receiver<BridgeMedia>,
    snapshot: Arc<Mutex<Option<BridgeSnapshot>>>,
    dropped: Arc<Mutex<HashSet<u64>>>,
) {
    let mut active_generation = 0;
    let mut minimum_media_generation = HashMap::<BridgeSourceKey, u64>::new();
    let mut retained_rehydration = HashSet::<BridgeSourceKey>::new();
    let mut active_sources = Vec::new();
    let mut active_nodes = Vec::new();
    let mut started_sources = HashSet::new();
    let mut desynchronized_sources = HashSet::new();
    let mut force_sources = false;
    let mut force_replacement = false;
    let mut deferred = None;
    loop {
        for (delivery_id, delivered, _outer_record_sequence) in bridge.take_media_completions() {
            if delivery_id != 0 {
                acknowledge_bridge_delivery(&client_writer, delivery_id, delivered);
            }
        }
        let outer_keyframes = bridge.take_keyframe_requests();
        if !outer_keyframes.is_empty() {
            let _ = client_writer.send(ClientMessage::BridgeNeedKeyframes(outer_keyframes));
        }
        let source_losses = bridge.take_source_losses();
        if !source_losses.is_empty() {
            desynchronized_sources.extend(source_losses);
            force_sources = true;
            let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry);
        }
        for changed in bridge.take_capability_changes() {
            let _ = client_writer.send(ClientMessage::BridgeCapabilitiesChanged {
                reason_mask: changed.reason_mask,
            });
        }
        for (source, playback) in bridge.take_playback_states() {
            let _ = client_writer.send(ClientMessage::BridgePlaybackState {
                source,
                state: playback.state,
                eos_state: playback.eos_state,
            });
        }
        let pending = snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pending) = pending {
            let change = if force_sources {
                ProjectionChange::Sources
            } else {
                compare_projection(
                    &active_sources,
                    &active_nodes,
                    &pending.sources,
                    &pending.nodes,
                )
            };
            let mut recreated = HashSet::new();
            let applied = match change {
                ProjectionChange::PlaybackOnly => {
                    bridge.update_playback(&active_sources, &pending.sources)
                }
                // Scene-only changes (occlusion fragments, node moves) reconcile nodes in the
                // current outer session: no pause, no source work, no retained-body replay,
                // no keyframe request. A global pause here would stall playback because
                // nothing re-issues PLAY for sources that were not recreated.
                ProjectionChange::SceneOnly => bridge
                    .update_nodes(&pending.nodes)
                    .and_then(|()| bridge.update_playback(&active_sources, &pending.sources)),
                ProjectionChange::Sources => {
                    let result = if force_replacement {
                        bridge.replace_session(&pending.sources, &pending.nodes)
                    } else {
                        bridge.rebuild(&pending.sources, &pending.nodes)
                    };
                    result.map(|sources| recreated = sources)
                }
            };
            if applied.is_err() {
                force_sources = true;
                force_replacement = true;
                let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry);
                continue;
            }
            let outer_revision = bridge.mark_projection_applied();
            let outer_attachment_generations = bridge.attachment_generations();
            let _ = client_writer.send(ClientMessage::BridgeApplied {
                virtual_revision: pending.virtual_revision,
                outer_revision,
                outer_attachment_generations,
            });

            if change == ProjectionChange::Sources {
                let current = pending
                    .sources
                    .iter()
                    .map(|source| source.key)
                    .collect::<HashSet<_>>();
                minimum_media_generation.retain(|source, _| current.contains(source));
                retained_rehydration.retain(|source| current.contains(source));
                for source in pending
                    .sources
                    .iter()
                    .filter(|source| recreated.contains(&source.key))
                {
                    started_sources.remove(&source.key);
                    minimum_media_generation.insert(source.key, pending.generation);
                    if matches!(
                        source.kind,
                        BridgeSourceKind::Raster { .. } | BridgeSourceKind::Image { .. }
                    ) {
                        retained_rehydration.insert(source.key);
                    } else {
                        retained_rehydration.remove(&source.key);
                    }
                }
                started_sources.retain(|source| current.contains(source));
            }
            force_sources = false;
            force_replacement = false;
            active_generation = pending.generation;
            let mut keyframes = pending.videos_needing_keyframes;
            keyframes.extend(desynchronized_sources.drain());
            if change == ProjectionChange::Sources {
                keyframes.extend(pending.sources.iter().filter_map(|source| {
                    (recreated.contains(&source.key)
                        && source.playing
                        && matches!(source.kind, BridgeSourceKind::Video { .. }))
                    .then_some(source.key)
                }));
                keyframes.sort_by_key(|source| (source.producer, source.source));
                keyframes.dedup();
            }
            active_sources = pending.sources;
            active_nodes = pending.nodes;
            started_sources.extend(
                active_sources
                    .iter()
                    .filter(|source| source_is_playing(&active_sources, source.key))
                    .map(|source| source.key),
            );
            if !keyframes.is_empty() {
                let _ = client_writer.send(ClientMessage::BridgeNeedKeyframes(keyframes));
            }
            continue;
        }

        if complete_dropped_deliveries(&client_writer, &dropped) {
            force_sources = true;
            force_replacement = true;
        }
        let media = match deferred.take() {
            Some(media) => media,
            None => match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(media) => media,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        if snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
        {
            deferred = Some(media);
            continue;
        }
        if media.generation
            < minimum_media_generation
                .get(&media.source)
                .copied()
                .unwrap_or(0)
            || media.generation > active_generation
        {
            acknowledge_bridge_delivery(&client_writer, media.delivery_id, false);
            continue;
        }
        if media.delivery_id == 0 && !retained_rehydration.contains(&media.source) {
            // The server sends retained image/raster bodies after every authoritative snapshot.
            // Replay only while this outer source genuinely needs rehydration. Source creation
            // and the first retained body can occupy different virtual revisions, so requiring
            // an exact generation would discard a normal image upload.
            continue;
        }
        if !active_sources
            .iter()
            .any(|source| source.key == media.source)
        {
            acknowledge_bridge_delivery(&client_writer, media.delivery_id, false);
            continue;
        }
        // Timed media for a source that has not started is legitimate pre-roll. OuterBridge
        // acknowledges it after the outer socket write, replenishing the virtual presenter's
        // one-packet grant so Vivi can buffer enough linked audio to issue PLAY. Only reject
        // media for a source that had started and is now stopped.
        if matches!(
            media.record_type,
            vivid_protocol::messages::VIDEO_PACKET | vivid_protocol::messages::AUDIO_PACKET
        ) && !source_is_playing(&active_sources, media.source)
            && started_sources.contains(&media.source)
        {
            acknowledge_bridge_delivery(&client_writer, media.delivery_id, false);
            continue;
        }
        match bridge.media_chunk(
            media.delivery_id,
            media.source,
            media.record_type,
            media.offset,
            media.total,
            media.last,
            media.bytes,
        ) {
            Ok(true) => {
                if hydrates_retained_source(media.record_type) {
                    // A live raster delivery can be the first body for a newly projected outer
                    // source. It hydrates that source just as retained delivery 0 does, so later
                    // authoritative snapshots must not replay the same frame ID.
                    retained_rehydration.remove(&media.source);
                }
            }
            Ok(false) => {}
            Err(_) => {
                dropped
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(media.delivery_id);
            }
        }
    }
}

fn hydrates_retained_source(record_type: u16) -> bool {
    matches!(
        record_type,
        vivid_protocol::messages::RASTER_FRAME | vivid_protocol::messages::IMAGE_DATA
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionChange {
    /// Identical source kinds and identical nodes: apply PLAY/PAUSE transitions only.
    PlaybackOnly,
    /// Identical source kinds but a changed node set or geometry: reconcile nodes in the
    /// current outer session without touching sources.
    SceneOnly,
    /// Source keys or descriptors changed: reconcile sources (and nodes) in the current outer
    /// session; a replacement session is built only after an uncertain reconcile failure.
    Sources,
}

#[cfg(test)]
fn recreated_source_keys(
    previous: &[BridgeSource],
    current: &[BridgeSource],
    replacing_session: bool,
) -> HashSet<BridgeSourceKey> {
    let mut recreated = current
        .iter()
        .filter(|source| {
            replacing_session
                || previous
                    .iter()
                    .find(|old| old.key == source.key)
                    .is_none_or(|old| old.kind != source.kind)
        })
        .map(|source| source.key)
        .collect::<HashSet<_>>();
    loop {
        let before = recreated.len();
        for source in current {
            if let BridgeSourceKind::Audio {
                linked_video: Some(video),
                ..
            } = source.kind
                && recreated.contains(&video)
            {
                recreated.insert(source.key);
            }
        }
        if recreated.len() == before {
            return recreated;
        }
    }
}

fn compare_projection(
    previous_sources: &[BridgeSource],
    previous_nodes: &[BridgeNode],
    current_sources: &[BridgeSource],
    current_nodes: &[BridgeNode],
) -> ProjectionChange {
    let sources_unchanged = previous_sources.len() == current_sources.len()
        && previous_sources.iter().all(|previous| {
            current_sources
                .iter()
                .any(|current| previous.key == current.key && previous.kind == current.kind)
        });
    if !sources_unchanged {
        return ProjectionChange::Sources;
    }
    let nodes_unchanged = previous_nodes.len() == current_nodes.len()
        && previous_nodes
            .iter()
            .all(|previous| current_nodes.contains(previous));
    if nodes_unchanged {
        ProjectionChange::PlaybackOnly
    } else {
        ProjectionChange::SceneOnly
    }
}

#[cfg(test)]
fn recreated_playing_video_sources(
    previous: &[BridgeSource],
    current: &[BridgeSource],
) -> Vec<BridgeSourceKey> {
    current
        .iter()
        .filter(|source| {
            source.playing
                && matches!(&source.kind, crate::ipc::BridgeSourceKind::Video { .. })
                && previous
                    .iter()
                    .find(|old| old.key == source.key)
                    .is_none_or(|old| old.kind != source.kind)
        })
        .map(|source| source.key)
        .collect()
}

fn source_is_playing(sources: &[BridgeSource], key: BridgeSourceKey) -> bool {
    let Some(source) = sources.iter().find(|source| source.key == key) else {
        return false;
    };
    if source.playing {
        return true;
    }
    let crate::ipc::BridgeSourceKind::Audio {
        linked_video: Some(video),
        ..
    } = &source.kind
    else {
        return false;
    };
    sources
        .iter()
        .any(|source| source.key == *video && source.playing)
}

fn complete_dropped_deliveries(
    client_writer: &BridgeClientSender,
    dropped: &Arc<Mutex<HashSet<u64>>>,
) -> bool {
    let delivery_ids = {
        let mut dropped = dropped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if dropped.is_empty() {
            return false;
        }
        dropped.drain().collect::<Vec<_>>()
    };
    let mut retry_snapshot = false;
    for delivery_id in delivery_ids {
        if delivery_id == 0 {
            retry_snapshot = true;
        } else {
            acknowledge_bridge_delivery(client_writer, delivery_id, false);
        }
    }
    if retry_snapshot {
        let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry);
    }
    retry_snapshot
}

fn acknowledge_bridge_delivery(
    client_writer: &BridgeClientSender,
    delivery_id: u64,
    delivered: bool,
) {
    if delivery_id != 0 {
        let _ = client_writer.send(ClientMessage::BridgeMediaAck {
            delivery_id,
            delivered,
        });
    }
}

pub fn kill(name: &str) -> io::Result<()> {
    let (_reader, writer) = crate::server::connect(name)?;
    send_client(&writer, &ClientMessage::Kill)
}

pub fn create_detached(name: &str, config_path: Option<&Path>) -> io::Result<()> {
    match crate::server::probe(name) {
        Ok(()) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "session already exists",
        )),
        Err(_) => {
            spawn_server(name, config_path)?;
            let _ = wait_for_server(name)?;
            Ok(())
        }
    }
}

#[cfg(unix)]
fn spawn_server(name: &str, config_path: Option<&Path>) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.arg("__server").arg("--session").arg(name);
    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("VIVID_ENDPOINT")
        .env_remove("VIVID_ENDPOINT_BULK")
        .env_remove("VIVID_TOKEN")
        .env_remove("VIVID_SSH_ENDPOINT")
        .env_remove("VIVID_SSH_TOKEN");
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn spawn_server(name: &str, config_path: Option<&Path>) -> io::Result<()> {
    crate::platform::DaemonLauncher::launch(name, config_path)
}

fn wait_for_server(name: &str) -> io::Result<(crate::ipc::RecordReader, SharedWriter)> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_error = None;
    while Instant::now() < deadline {
        match crate::server::connect(name) {
            Ok(connection) => return Ok(connection),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "server startup timed out")))
}

fn is_missing_session(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
    )
}

fn send_client(writer: &SharedWriter, message: &ClientMessage) -> io::Result<()> {
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(message)
}

fn write_output(output: &Arc<Mutex<Box<dyn Write + Send>>>, bytes: &[u8]) -> io::Result<()> {
    let mut output = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    output.write_all(bytes)?;
    output.flush()
}

fn write_title(output: &Arc<Mutex<Box<dyn Write + Send>>>, title: &str) -> io::Result<()> {
    let sanitized = title.replace(['\x07', '\x1b'], "");
    write_output(output, format!("\x1b]2;{sanitized}\x1b\\").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[cfg(unix)]
    use crate::config::Media as MediaConfig;
    #[cfg(unix)]
    use crate::ipc::{BridgeSourceKind, ChannelKind, establish};
    #[cfg(unix)]
    use crate::media::VirtualVivid;
    #[cfg(unix)]
    use image::ImageEncoder;
    #[cfg(unix)]
    use vivid_protocol::media::{self, AudioPacket, VideoPacket};

    #[cfg(unix)]
    fn test_transport(stream: UnixStream) -> crate::platform::Transport {
        let reader = stream.try_clone().unwrap();
        let timeout_stream = stream.try_clone().unwrap();
        crate::platform::Transport::new(
            Box::new(reader),
            Box::new(stream),
            crate::platform::ConnectionCancel::inert(),
            Arc::new(move |duration| timeout_stream.set_read_timeout(duration)),
        )
    }

    #[test]
    fn prefix_parser_sends_literal_and_actions() {
        let mut parser = PrefixParser::default();
        let commands = parser.feed(b"a\x02\x02\x02%z");
        assert!(matches!(&commands[0], ParsedInput::Input(bytes) if bytes == b"a"));
        assert!(matches!(&commands[1], ParsedInput::Input(bytes) if bytes == b"\x02"));
        assert!(matches!(
            commands[2],
            ParsedInput::Action(Action::Split(Axis::Vertical))
        ));
        assert!(matches!(&commands[3], ParsedInput::Input(bytes) if bytes == b"z"));
    }

    #[test]
    fn live_raster_hydrates_a_new_outer_source() {
        assert!(hydrates_retained_source(
            vivid_protocol::messages::RASTER_FRAME
        ));
        assert!(hydrates_retained_source(
            vivid_protocol::messages::IMAGE_DATA
        ));
        assert!(!hydrates_retained_source(
            vivid_protocol::messages::VIDEO_PACKET
        ));
    }

    #[test]
    fn prefix_arrow_can_span_reads() {
        let mut parser = PrefixParser::default();
        assert!(parser.feed(b"\x02\x1b[").is_empty());
        assert!(matches!(
            parser.feed(b"A").as_slice(),
            [ParsedInput::Action(Action::Focus(Direction::Up))]
        ));
    }

    #[test]
    fn sgr_mouse_is_decoded_without_forwarding_outer_coordinates_as_text() {
        let mut parser = PrefixParser::default();
        let commands = parser.feed(b"\x1b[<68;12;7M");
        assert!(matches!(
            commands.as_slice(),
            [ParsedInput::Mouse(MouseEvent {
                button: 0,
                x: 11,
                y: 6,
                kind: MouseKind::Wheel,
                shift: true,
            })]
        ));
    }

    #[test]
    fn configured_prefix_binding_overrides_default_key() {
        let bindings =
            std::collections::BTreeMap::from([("f".to_owned(), "split-horizontal".to_owned())]);
        let mut parser = PrefixParser::new(0x01, &bindings);
        assert!(matches!(
            parser.feed(b"\x01f").as_slice(),
            [ParsedInput::Action(Action::Split(Axis::Horizontal))]
        ));
    }

    #[test]
    fn floating_default_keys_are_case_sensitive() {
        let mut parser = PrefixParser::default();
        for (byte, action) in [
            (b"\x02f".as_slice(), Action::NewFloatingPane),
            (b"\x02F", Action::ToggleFloatingPanes),
            (b"\x02P", Action::TogglePanePinned),
            (b"\x02m", Action::EnterFloatingMoveMode),
            (b"\x02r", Action::EnterFloatingResizeMode),
        ] {
            let commands = parser.feed(byte);
            assert_eq!(commands.len(), 1, "one action per chord");
            assert!(matches!(&commands[0], ParsedInput::Action(parsed) if *parsed == action));
        }
        assert!(
            matches!(
                parser.feed(b"\x02p").as_slice(),
                [ParsedInput::Action(Action::PreviousTab)]
            ),
            "lowercase p keeps its previous-tab meaning"
        );
        for name in [
            "new-floating-pane",
            "toggle-floating-panes",
            "toggle-pane-pinned",
            "enter-floating-move-mode",
            "enter-floating-resize-mode",
        ] {
            assert!(parse_configured_action(name).is_some());
        }
    }

    #[test]
    fn float_edit_scanner_is_fragment_safe_and_preserves_non_edit_input() {
        let mut scanner = FloatEditScanner::default();
        let (commands, forward) = scanner.scan(b"x\x1b[");
        assert!(commands.is_empty());
        assert_eq!(forward, b"x");

        let (commands, forward) = scanner.scan(b"A\x1b[1;");
        assert_eq!(
            commands,
            [FloatingEditCommand::Step {
                direction: Direction::Up,
                cells: 1,
            }]
        );
        assert!(forward.is_empty());
        let (commands, forward) = scanner.scan(b"2D!");
        assert_eq!(
            commands,
            [FloatingEditCommand::Step {
                direction: Direction::Left,
                cells: 5,
            }]
        );
        assert_eq!(forward, b"!");

        let (commands, forward) = scanner.scan(b"\rtrailing");
        assert_eq!(commands, [FloatingEditCommand::Commit]);
        assert_eq!(forward, b"trailing");
    }

    #[test]
    fn float_edit_scanner_expires_only_a_bare_escape() {
        let mut scanner = FloatEditScanner::default();
        assert!(scanner.scan(b"\x1b").0.is_empty());
        assert_eq!(
            scanner.expire(Instant::now() + FloatEditScanner::ESCAPE_DELAY),
            Some(FloatingEditCommand::Cancel)
        );

        assert!(scanner.scan(b"\x1b[").0.is_empty());
        assert_eq!(
            scanner.expire(Instant::now() + Duration::from_secs(1)),
            None,
            "a fragmented arrow prefix must not become Escape"
        );
        assert_eq!(scanner.reset(), b"\x1b[");
    }

    #[test]
    fn full_bridge_media_queue_drops_instead_of_blocking_input() {
        let (media, receiver) = mpsc::sync_channel(1);
        let dropped = Arc::new(Mutex::new(HashSet::new()));
        let mut worker = BridgeWorker {
            media,
            snapshot: Arc::new(Mutex::new(None)),
            dropped: dropped.clone(),
            generation: 1,
        };
        let key = BridgeSourceKey {
            producer: 3,
            source: 7,
        };
        let record = || BridgeMedia {
            generation: 0,
            delivery_id: 11,
            source: key,
            record_type: vivid_protocol::messages::VIDEO_PACKET,
            offset: 0,
            total: 1,
            last: true,
            bytes: vec![0],
        };

        assert!(worker.queue_media(record()));
        assert!(!worker.queue_media(record()));
        assert!(
            dropped
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&11)
        );
        assert_eq!(receiver.try_recv().unwrap().generation, 1);
    }

    #[test]
    #[cfg(unix)]
    fn bridge_delivers_consecutive_raster_frames_to_one_outer_source() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-raster.sock");
        let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping bridge raster socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual outer presenter start failed: {error}"),
        };
        let token = presenter.issue_pane_capability(7).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));
        let bridge = crate::bridge::OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new(token),
            DisplayMetrics::default(),
        )
        .unwrap();

        let (client, server) = UnixStream::pair().unwrap();
        let client_establish = thread::spawn(move || {
            crate::ipc::establish(test_transport(client), ChannelKind::Control)
        });
        let (mut server_reader, _server_writer) =
            establish(test_transport(server), ChannelKind::Control).unwrap();
        let (_client_reader, client_writer) = client_establish.join().unwrap().unwrap();
        let mut worker = BridgeWorker::spawn(bridge, client_writer, 8).unwrap();
        let (ack_sender, ack_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(message) = server_reader.recv::<ClientMessage>() {
                if let ClientMessage::BridgeMediaAck {
                    delivery_id,
                    delivered,
                } = message
                {
                    let _ = ack_sender.send((delivery_id, delivered));
                }
            }
        });

        let key = BridgeSourceKey {
            producer: 3,
            source: 7,
        };
        let source = BridgeSource {
            key,
            kind: BridgeSourceKind::Raster {
                width: 2,
                height: 1,
                alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
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
        };
        let node = BridgeNode {
            producer: key.producer,
            node: 1,
            fragment: 0,
            source: key,
            x: 0,
            y: 0,
            width: 2_i64 << 32,
            height: 1_i64 << 32,
            z_index: 0,
            visible: true,
            clip: crate::ipc::BridgeClipRect {
                x: 0,
                y: 0,
                width: 2_i64 << 32,
                height: 1_i64 << 32,
            },
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 1,
            sources: vec![source],
            nodes: vec![node],
            videos_needing_keyframes: Vec::new(),
        });

        let frames = [
            media::raster_frame_body(1, 1, 2, 1, &[255, 0, 0, 255, 0, 0, 0, 255]).unwrap(),
            media::raster_frame_body(1, 2, 2, 1, &[0, 255, 0, 255, 0, 0, 0, 255]).unwrap(),
        ];
        for (index, frame) in frames.iter().enumerate() {
            let delivery_id = 41 + index as u64;
            assert!(worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id,
                source: key,
                record_type: vivid_protocol::messages::RASTER_FRAME,
                offset: 0,
                total: frame.len() as u32,
                last: true,
                bytes: frame.clone(),
            }));
            assert_eq!(
                ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
                (delivery_id, true)
            );
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
                if snapshot
                    .sources
                    .first()
                    .and_then(|source| source.retained.as_deref())
                    == Some(frame.as_slice())
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "outer raster frame was not updated"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn bridge_forwards_linked_preroll_and_applies_play_before_video() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-vivid.sock");
        let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping bridge pre-roll socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual outer presenter start failed: {error}"),
        };
        let token = presenter.issue_pane_capability(7).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));
        let bridge = crate::bridge::OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new(token),
            DisplayMetrics::default(),
        )
        .unwrap();

        let (client, server) = UnixStream::pair().unwrap();
        let client_establish = thread::spawn(move || {
            crate::ipc::establish(test_transport(client), ChannelKind::Control)
        });
        let (mut server_reader, _server_writer) =
            establish(test_transport(server), ChannelKind::Control).unwrap();
        let (_client_reader, client_writer) = client_establish.join().unwrap().unwrap();
        let mut worker = BridgeWorker::spawn(bridge, client_writer, 8).unwrap();

        let key = BridgeSourceKey {
            producer: 3,
            source: 7,
        };
        let video_key = BridgeSourceKey {
            producer: 3,
            source: 8,
        };
        let video_source = BridgeSource {
            key: video_key,
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
            playing: false,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 100_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: vivid_protocol::messages::LATE_DROP_PRESENTATION,
                loop_count: 0,
                start_policy: vivid_protocol::messages::START_AFTER_MINIMUM_BUFFER,
            },
        };
        let audio_source = BridgeSource {
            key,
            kind: BridgeSourceKind::Audio {
                codec_string: None,
                linked_video: Some(video_key),
                codec: "pcm_s16le".into(),
                packetization: "pcm-packet-v1".into(),
                extradata: Vec::new(),
                sample_rate: 48_000,
                channels: 2,
                channel_mask: 3,
                bitrate: 0,
                max_access_unit_bytes: 1024,
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
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
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 1,
            sources: vec![video_source.clone(), audio_source.clone()],
            nodes: Vec::new(),
            videos_needing_keyframes: Vec::new(),
        });
        let packet = media::audio_packet_body(AudioPacket {
            epoch: 1,
            packet_id: 1,
            pts_us: 0,
            dts_us: 0,
            duration_us: 20_000,
            trim_start_samples: 0,
            trim_end_samples: 0,
            data: &[0, 0, 0, 0],
        })
        .unwrap();
        assert!(worker.queue_media(BridgeMedia {
            generation: 0,
            delivery_id: 42,
            source: key,
            record_type: vivid_protocol::messages::AUDIO_PACKET,
            offset: 0,
            total: packet.len() as u32,
            last: true,
            bytes: packet,
        }));

        let (ack_sender, ack_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(message) = server_reader.recv::<ClientMessage>() {
                if let ClientMessage::BridgeMediaAck {
                    delivery_id,
                    delivered,
                } = message
                {
                    let _ = ack_sender.send((delivery_id, delivered));
                }
            }
        });
        assert_eq!(
            ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            (42, true),
            "pre-PLAY media must replenish the virtual presenter's one-packet grant"
        );

        let mut playing_video = video_source;
        playing_video.playing = true;
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 2,
            sources: vec![playing_video, audio_source],
            nodes: Vec::new(),
            videos_needing_keyframes: Vec::new(),
        });
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
        assert!(worker.queue_media(BridgeMedia {
            generation: 0,
            delivery_id: 43,
            source: video_key,
            record_type: vivid_protocol::messages::VIDEO_PACKET,
            offset: 0,
            total: packet.len() as u32,
            last: true,
            bytes: packet,
        }));
        assert_eq!(
            ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            (43, true),
            "the first post-PLAY keyframe must reach the existing outer video source"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
            if snapshot.sources.iter().any(|source| {
                source.key.1 != 0
                    && matches!(source.descriptor, crate::media::SourceDescriptor::Video(_))
                    && source.playing
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "outer video source never entered PLAY"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[cfg(unix)]
    fn delayed_retained_image_and_source_changes_preserve_mixed_media() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-scene.sock");
        let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping scene-only bridge socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual outer presenter start failed: {error}"),
        };
        let token = presenter.issue_pane_capability(7).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));
        let bridge = crate::bridge::OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new(token),
            DisplayMetrics::default(),
        )
        .unwrap();

        let (client, server) = UnixStream::pair().unwrap();
        let client_establish = thread::spawn(move || {
            crate::ipc::establish(test_transport(client), ChannelKind::Control)
        });
        let (mut server_reader, _server_writer) =
            establish(test_transport(server), ChannelKind::Control).unwrap();
        let (_client_reader, client_writer) = client_establish.join().unwrap().unwrap();
        let mut worker = BridgeWorker::spawn(bridge, client_writer, 8).unwrap();

        let (keyframe_sender, keyframe_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(message) = server_reader.recv::<ClientMessage>() {
                if let ClientMessage::BridgeNeedKeyframes(sources) = message {
                    let _ = keyframe_sender.send(sources);
                }
            }
        });

        let video_key = BridgeSourceKey {
            producer: 3,
            source: 7,
        };
        let video_source = BridgeSource {
            key: video_key,
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
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 33_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: vivid_protocol::messages::LATE_DROP_PRESENTATION,
                loop_count: 0,
                start_policy: vivid_protocol::messages::START_AFTER_MINIMUM_BUFFER,
            },
        };
        let node = |x: i64| BridgeNode {
            producer: 3,
            node: 1,
            fragment: 0,
            source: video_key,
            x: x << 32,
            y: 0,
            width: 8_i64 << 32,
            height: 4_i64 << 32,
            z_index: 0,
            visible: true,
            clip: crate::ipc::BridgeClipRect {
                x: x << 32,
                y: 0,
                width: 8_i64 << 32,
                height: 4_i64 << 32,
            },
        };
        let outer_video = |snapshot: &crate::media::ProjectionSnapshot| {
            snapshot
                .sources
                .iter()
                .find(|source| {
                    matches!(source.descriptor, crate::media::SourceDescriptor::Video(_))
                })
                .map(|source| (source.key, source.playing))
        };
        let wait_for = |condition: &dyn Fn(&crate::media::ProjectionSnapshot) -> bool,
                        what: &str| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
                if condition(&snapshot) {
                    break snapshot;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(2));
            }
        };

        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 1,
            sources: vec![video_source.clone()],
            nodes: vec![node(0)],
            videos_needing_keyframes: Vec::new(),
        });
        let snapshot = wait_for(
            &|snapshot| {
                outer_video(snapshot).is_some_and(|(_, playing)| playing)
                    && !snapshot.nodes.is_empty()
            },
            "initial playing video projection",
        );
        let (outer_key, _) = outer_video(&snapshot).unwrap();
        assert_eq!(
            keyframe_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            vec![video_key],
            "a newly created playing video requests one keyframe"
        );

        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 2,
            sources: vec![video_source.clone()],
            nodes: vec![node(5)],
            videos_needing_keyframes: Vec::new(),
        });
        let snapshot = wait_for(
            &|snapshot| {
                snapshot
                    .nodes
                    .first()
                    .is_some_and(|node| node.config.node.x == 5_i64 << 32)
            },
            "moved scene node",
        );
        assert_eq!(
            outer_video(&snapshot).unwrap(),
            (outer_key, true),
            "a scene-only change must not pause or recreate the playing video"
        );
        assert!(
            keyframe_receiver.try_recv().is_err(),
            "a scene-only change must not request a keyframe"
        );

        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(
                &[0xff, 0x40, 0x20, 0xff],
                1,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let image_key = BridgeSourceKey {
            producer: 4,
            source: 9,
        };
        let image_source = BridgeSource {
            key: image_key,
            kind: BridgeSourceKind::Image {
                encoding: vivid_protocol::messages::IMAGE_PNG,
                width: 1,
                height: 1,
                encoded_length: u32::try_from(encoded.len()).unwrap(),
                sha256: None,
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
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
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 3,
            sources: vec![video_source.clone(), image_source.clone()],
            nodes: vec![node(5)],
            videos_needing_keyframes: Vec::new(),
        });
        let snapshot = wait_for(
            &|snapshot| {
                snapshot.sources.iter().any(|source| {
                    matches!(source.descriptor, crate::media::SourceDescriptor::Image(_))
                })
            },
            "added image source",
        );
        assert_eq!(
            outer_video(&snapshot).unwrap(),
            (outer_key, true),
            "adding a source must reconcile in the current session without pausing playback"
        );
        assert!(keyframe_receiver.try_recv().is_err());

        let image_node = BridgeNode {
            producer: image_key.producer,
            node: 2,
            fragment: 0,
            source: image_key,
            x: 20_i64 << 32,
            y: 0,
            width: 1_i64 << 32,
            height: 1_i64 << 32,
            z_index: 0,
            visible: true,
            clip: crate::ipc::BridgeClipRect {
                x: 20_i64 << 32,
                y: 0,
                width: 1_i64 << 32,
                height: 1_i64 << 32,
            },
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 4,
            sources: vec![video_source.clone(), image_source.clone()],
            nodes: vec![node(5), image_node.clone()],
            videos_needing_keyframes: Vec::new(),
        });
        assert!(worker.queue_media(BridgeMedia {
            generation: 0,
            delivery_id: 0,
            source: image_key,
            record_type: vivid_protocol::messages::IMAGE_DATA,
            offset: 0,
            total: u32::try_from(encoded.len()).unwrap(),
            last: true,
            bytes: encoded.clone(),
        }));
        let snapshot = wait_for(
            &|snapshot| {
                snapshot
                    .sources
                    .iter()
                    .any(|source| source.key.1 != 0 && source.retained.is_some())
            },
            "image body arriving after its source-creation generation",
        );
        assert_eq!(outer_video(&snapshot).unwrap(), (outer_key, true));

        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 5,
            sources: Vec::new(),
            nodes: Vec::new(),
            videos_needing_keyframes: Vec::new(),
        });
        wait_for(
            &|snapshot| snapshot.sources.is_empty() && snapshot.nodes.is_empty(),
            "empty projection after switching away from the image tab",
        );

        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 6,
            sources: vec![image_source.clone()],
            nodes: vec![image_node.clone()],
            videos_needing_keyframes: Vec::new(),
        });
        assert!(worker.queue_media(BridgeMedia {
            generation: 0,
            delivery_id: 0,
            source: image_key,
            record_type: vivid_protocol::messages::IMAGE_DATA,
            offset: 0,
            total: u32::try_from(encoded.len()).unwrap(),
            last: true,
            bytes: encoded,
        }));
        wait_for(
            &|snapshot| {
                snapshot.sources.len() == 1
                    && snapshot.sources[0].retained.is_some()
                    && snapshot.nodes.len() == 1
            },
            "retained image after switching back to its tab",
        );

        let remaining_image_node = BridgeNode {
            x: 0,
            clip: crate::ipc::BridgeClipRect {
                x: 0,
                ..image_node.clip
            },
            ..image_node.clone()
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 7,
            sources: vec![image_source.clone()],
            nodes: vec![remaining_image_node.clone()],
            videos_needing_keyframes: Vec::new(),
        });
        let snapshot = wait_for(
            &|snapshot| {
                snapshot.sources.len() == 1
                    && snapshot.sources[0].retained.is_some()
                    && snapshot
                        .nodes
                        .first()
                        .is_some_and(|node| node.config.node.x == 0)
            },
            "remaining image after the video pane exits",
        );
        assert!(snapshot.sources[0].retained.is_some());

        let replay_key = BridgeSourceKey {
            producer: 5,
            source: 10,
        };
        let replay_video = BridgeSource {
            key: replay_key,
            kind: video_source.kind,
            capture_policy: 0,
            descriptor: None,
            playing: true,
            causation_id: None,
            play_request: video_source.play_request,
        };
        let replay_node = BridgeNode {
            producer: replay_key.producer,
            node: 3,
            source: replay_key,
            x: 20_i64 << 32,
            clip: crate::ipc::BridgeClipRect {
                x: 20_i64 << 32,
                ..image_node.clip
            },
            ..image_node.clone()
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 8,
            sources: vec![image_source, replay_video],
            nodes: vec![replay_node, remaining_image_node],
            videos_needing_keyframes: Vec::new(),
        });
        let snapshot = wait_for(
            &|snapshot| {
                snapshot.nodes.len() == 2
                    && outer_video(snapshot).is_some_and(|(_, playing)| playing)
                    && snapshot.sources.iter().any(|source| {
                        matches!(source.descriptor, crate::media::SourceDescriptor::Image(_))
                            && source.retained.is_some()
                    })
            },
            "replayed video node alongside the retained image",
        );
        assert_eq!(snapshot.nodes.len(), 2);
        assert_eq!(
            keyframe_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            vec![replay_key]
        );
    }

    #[test]
    fn playback_only_snapshot_preserves_projection_and_linked_audio_state() {
        let video_key = BridgeSourceKey {
            producer: 3,
            source: 7,
        };
        let audio_key = BridgeSourceKey {
            producer: 3,
            source: 8,
        };
        let video = |playing| BridgeSource {
            key: video_key,
            kind: crate::ipc::BridgeSourceKind::Video {
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
            playing,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 100_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: vivid_protocol::messages::LATE_DROP_PRESENTATION,
                loop_count: 0,
                start_policy: vivid_protocol::messages::START_AFTER_MINIMUM_BUFFER,
            },
        };
        let audio = BridgeSource {
            key: audio_key,
            kind: crate::ipc::BridgeSourceKind::Audio {
                codec_string: None,
                linked_video: Some(video_key),
                codec: "aac".into(),
                packetization: "aac-raw-au-v1".into(),
                extradata: Vec::new(),
                sample_rate: 48_000,
                channels: 2,
                channel_mask: 3,
                bitrate: 128_000,
                max_access_unit_bytes: 4096,
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
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
        };
        let before = vec![video(false), audio.clone()];
        let after = vec![audio, video(true)];

        assert_eq!(
            compare_projection(&before, &[], &after, &[]),
            ProjectionChange::PlaybackOnly
        );
        assert!(recreated_playing_video_sources(&before, &after).is_empty());
        assert_eq!(
            recreated_playing_video_sources(&[], &after),
            vec![video_key]
        );
        assert!(source_is_playing(&after, video_key));
        assert!(source_is_playing(&after, audio_key));

        let node = |x: i64| BridgeNode {
            producer: 3,
            node: 1,
            fragment: 0,
            source: video_key,
            x: x << 32,
            y: 0,
            width: 4_i64 << 32,
            height: 2_i64 << 32,
            z_index: 0,
            visible: true,
            clip: crate::ipc::BridgeClipRect {
                x: x << 32,
                y: 0,
                width: 4_i64 << 32,
                height: 2_i64 << 32,
            },
        };
        assert_eq!(
            compare_projection(&before, &[node(0)], &after, &[node(0)]),
            ProjectionChange::PlaybackOnly
        );
        assert_eq!(
            compare_projection(&before, &[node(0)], &after, &[node(5)]),
            ProjectionChange::SceneOnly,
            "a moved node must reconcile the scene without touching sources"
        );
        assert_eq!(
            compare_projection(&before, &[node(0)], &after, &[]),
            ProjectionChange::SceneOnly,
            "a removed node must reconcile the scene without touching sources"
        );
        assert_eq!(
            compare_projection(&[], &[], &after, &[node(0)]),
            ProjectionChange::Sources
        );
        assert!(
            recreated_source_keys(&before, &after, false).is_empty(),
            "playback-only reordering rehydrates no retained bodies"
        );
        assert_eq!(
            recreated_source_keys(&[], &after, false),
            HashSet::from([video_key, audio_key]),
            "an initial projection creates every source"
        );
        assert_eq!(
            recreated_source_keys(&before, &after, true),
            HashSet::from([video_key, audio_key]),
            "a replacement outer session recreates every source"
        );

        let image_key = BridgeSourceKey {
            producer: 3,
            source: 12,
        };
        let mut with_image = after.clone();
        with_image.push(BridgeSource {
            key: image_key,
            kind: BridgeSourceKind::Image {
                encoding: vivid_protocol::messages::IMAGE_PNG,
                width: 4,
                height: 4,
                encoded_length: 32,
                sha256: None,
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
            causation_id: None,
            play_request: after[0].play_request,
        });
        assert_eq!(
            recreated_source_keys(&after, &with_image, false),
            HashSet::from([image_key]),
            "adding an image must not replay retained bodies for unchanged sources"
        );
    }
}
