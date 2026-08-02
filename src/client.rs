use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use zeroize::Zeroizing;

#[cfg(test)]
use crate::client_input::parse_configured_action;
use crate::client_input::{self, FloatEditScanner, ParsedInput, PrefixParser};
#[cfg(all(test, unix))]
use crate::ipc::DisplayMetrics;
#[cfg(test)]
use crate::ipc::{Action, Axis, Direction, MouseEvent, MouseKind};
use crate::ipc::{
    BridgeKeyframeRequest, BridgeNode, BridgeSource, BridgeSourceKey, BridgeSourceKind,
    BridgeSurface, ClientMessage, FloatingEditCommand, ServerMessage, SharedWriter,
};
use crate::media_trace::{
    BridgeMediaTraceEvent, MediaKeyframeStage, MediaPlaybackControl, MediaTraceKind,
};
use crate::platform::ClientTerminal;

const BRIDGE_MEDIA_CHUNK: usize = 128 * 1024;
/// How often the bridge worker reports its counters to the session server.
///
/// Coarse on purpose: these are diagnostics and must not add measurable traffic to the client
/// connection they are measuring.
const METRICS_REPORT_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(windows)]
const DISPLAY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DETACH_ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// Idle wait for host terminal input when nothing time-sensitive is buffered.
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);

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

    // Keep outer credentials exclusively in the foreground process. Zeroizing guarantees root
    // secret bytes are overwritten when the Vivid bridge or this text-only client closes.
    let outer_endpoint = std::env::var("VIVID_ENDPOINT_CONTROL").ok();
    let outer_realtime_endpoint = std::env::var("VIVID_ENDPOINT_REALTIME").ok();
    let outer_bulk_endpoint = std::env::var("VIVID_ENDPOINT_BULK").ok();
    let outer_root_secret = std::env::var("VIVID_ROOT_SECRET").ok().map(Zeroizing::new);
    let vivid = outer_endpoint.is_some() && outer_root_secret.is_some();
    let terminal = ClientTerminal::enter()?;
    let display = terminal.display_metrics()?;
    let presenter_cell_size = Arc::new(AtomicU32::new(pack_cell_size(display)));
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
    let output_thread = TerminalOutput::spawn(output, writer.clone())?;
    let bridge_display = display;
    let bridge_cell_size = presenter_cell_size.clone();
    let bridge_queue_records =
        (client_config.media.ipc_queue_bytes / BRIDGE_MEDIA_CHUNK).clamp(1, 1024);
    let reader_thread = thread::Builder::new()
        .name("vvmux-render".into())
        .spawn(move || {
            let mut bridge = match (outer_endpoint, outer_root_secret) {
                (Some(endpoint), Some(root_secret)) => {
                    match crate::bridge::OuterBridge::connect_native(
                        endpoint,
                        outer_realtime_endpoint,
                        outer_bulk_endpoint,
                        root_secret,
                        bridge_display,
                    ) {
                        Ok(bridge) => {
                            let presenter_display = bridge.display_metrics();
                            bridge_cell_size
                                .store(pack_cell_size(presenter_display), Ordering::Release);
                            if presenter_display != bridge_display {
                                let _ = send_client(
                                    &read_writer,
                                    &ClientMessage::Resize(presenter_display),
                                );
                            }
                            match BridgeWorker::spawn(
                                bridge,
                                read_writer.clone(),
                                bridge_queue_records,
                                bridge_cell_size.clone(),
                            ) {
                                Ok(worker) => Some(worker),
                                Err(error) => {
                                    write_title(
                                        &output_thread,
                                        &format!("vvmux media disabled: {error}"),
                                    );
                                    None
                                }
                            }
                        }
                        Err(error) => {
                            write_title(&output_thread, &format!("vvmux media disabled: {error}"));
                            None
                        }
                    }
                }
                _ => None,
            };
            while let Ok(message) = reader.recv_server() {
                match message {
                    ServerMessage::Attached { text_only, .. } => {
                        if text_only {
                            write_title(&output_thread, "vvmux (text-only media fallback)");
                        }
                    }
                    ServerMessage::Render {
                        frame_id,
                        full,
                        last,
                        bytes,
                        ..
                    } => {
                        // Queue and return. The acknowledgement is sent by the output thread once
                        // the bytes reach the terminal, so it stays a true completion signal.
                        if !output_thread.enqueue_frame(frame_id, full, last, bytes) {
                            // The backlog was discarded, so the screen no longer matches what the
                            // server believes it drew. Only a full redraw can restore agreement.
                            let _ = send_client(&read_writer, &ClientMessage::RenderResync);
                        }
                    }
                    ServerMessage::Title(title) => {
                        write_title(&output_thread, &title);
                    }
                    ServerMessage::Bell => {
                        output_thread.enqueue_control(b"\x07".to_vec());
                    }
                    ServerMessage::Clipboard(text) => {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
                        output_thread
                            .enqueue_control(format!("\x1b]52;c;{encoded}\x1b\\").into_bytes());
                    }
                    ServerMessage::Status(status) => {
                        write_title(&output_thread, &format!("vvmux: {status}"));
                    }
                    ServerMessage::MediaSnapshot {
                        revision,
                        surfaces,
                        tracks,
                        nodes,
                        videos_needing_keyframes,
                    } => {
                        if let Some(bridge) = &mut bridge {
                            bridge.replace_snapshot(BridgeSnapshot {
                                generation: 0,
                                virtual_revision: revision,
                                surfaces,
                                tracks,
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
            output_thread.stop();
            read_stopped.store(true, Ordering::Release);
        })?;

    #[cfg(windows)]
    let resize_stopped = stopped.clone();
    #[cfg(windows)]
    let resize_writer = writer.clone();
    #[cfg(windows)]
    let resize_cell_size = presenter_cell_size;
    #[cfg(windows)]
    let resize_thread = match thread::Builder::new()
        .name("vvmux-display".into())
        .spawn(move || {
            let mut last_display = display;
            while !resize_stopped.load(Ordering::Acquire) {
                thread::sleep(DISPLAY_POLL_INTERVAL);
                if resize_stopped.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut current) = crate::platform::current_display_metrics() else {
                    continue;
                };
                apply_cell_size(&mut current, resize_cell_size.load(Ordering::Acquire));
                if current != last_display {
                    if send_client(&resize_writer, &ClientMessage::Resize(current)).is_err() {
                        resize_stopped.store(true, Ordering::Release);
                        break;
                    }
                    last_display = current;
                }
            }
        }) {
        Ok(thread) => thread,
        Err(error) => {
            ipc_cancel.cancel();
            let _ = reader_thread.join();
            return Err(error);
        }
    };

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
        stopped: stopped.clone(),
        ipc_cancel,
        reader_thread: Some(reader_thread),
        #[cfg(windows)]
        resize_thread: Some(resize_thread),
        #[cfg(unix)]
        signal_handle,
        #[cfg(unix)]
        signal_thread: Some(signal_thread),
    };

    let mut parser = PrefixParser::new(
        crate::config::parse_control_chord(&client_config.general.prefix).unwrap_or(0x02),
        &client_config.keys.prefix,
    );
    #[cfg(not(windows))]
    let mut last_display = display;
    let mut float_mode: Option<u64> = None;
    let mut float_scanner = FloatEditScanner::default();
    let mut detach_requested_at: Option<Instant> = None;
    let result = (|| -> io::Result<()> {
        while !stopped.load(Ordering::Acquire) {
            if let Some(requested_at) = detach_requested_at {
                if requested_at.elapsed() >= DETACH_ACK_TIMEOUT {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
                continue;
            }
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
            // A held bare Escape has to reach the pane on its own timescale, not on the idle poll
            // interval, or a modal editor appears to swallow the first press.
            let poll = if parser.holds_bare_escape() || float_scanner.holds_bare_escape() {
                client_input::ESCAPE_DELAY
            } else {
                INPUT_POLL_INTERVAL
            };
            if let Some(read) = terminal.read_input(&mut bytes, poll)? {
                if read == 0 {
                    let _ = send_client(&writer, &ClientMessage::Detach);
                    detach_requested_at = Some(Instant::now());
                    continue;
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
                        ParsedInput::Focus(focused) => {
                            send_client(&writer, &ClientMessage::Focus(focused))?
                        }
                        ParsedInput::Detach => {
                            send_client(&writer, &ClientMessage::Detach)?;
                            // Keep the reader alive until the session actor acknowledges Detach.
                            // Closing IPC immediately can let a new attach overtake the old detach,
                            // producing an empty alternate screen and then tearing down its newly
                            // opened Vivid connection with a Windows socket reset.
                            detach_requested_at = Some(Instant::now());
                            break;
                        }
                    }
                }
            } else {
                let now = Instant::now();
                if let Some(mode_id) = float_mode
                    && let Some(command) = float_scanner.expire(now)
                {
                    send_client(&writer, &ClientMessage::FloatingEdit { mode_id, command })?;
                    float_mode = None;
                }
                if let Some(escape) = parser.expire(now) {
                    send_client(&writer, &ClientMessage::Input(escape))?;
                }
            }
            #[cfg(not(windows))]
            {
                if let Ok(mut display) = terminal.display_metrics() {
                    // The TTY remains authoritative for its live grid, but its pixel metrics are
                    // not the geometry the outer Vivid presenter actually renders. In particular,
                    // Linux TIOCGWINSZ values can differ from Vivido's WELCOME metrics. Publishing
                    // those values makes nested raster producers size their source differently
                    // from the projected node and forces the outer renderer to resample it.
                    apply_cell_size(&mut display, presenter_cell_size.load(Ordering::Acquire));
                    if display != last_display {
                        send_client(&writer, &ClientMessage::Resize(display))?;
                        last_display = display;
                    }
                }
            }
        }
        Ok(())
    })();
    workers.stop();
    drop(terminal);
    result
}

struct ClientWorkers {
    stopped: Arc<AtomicBool>,
    ipc_cancel: crate::platform::ConnectionCancel,
    reader_thread: Option<thread::JoinHandle<()>>,
    #[cfg(windows)]
    resize_thread: Option<thread::JoinHandle<()>>,
    #[cfg(unix)]
    signal_handle: signal_hook::iterator::Handle,
    #[cfg(unix)]
    signal_thread: Option<thread::JoinHandle<()>>,
}

impl ClientWorkers {
    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.ipc_cancel.cancel();
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        #[cfg(windows)]
        if let Some(thread) = self.resize_thread.take() {
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
    pub(crate) surfaces: Vec<BridgeSurface>,
    pub(crate) tracks: Vec<BridgeSource>,
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
    media: Arc<Mutex<TrackMediaQueues>>,
    media_wakeup: Option<mpsc::SyncSender<()>>,
    queue_records_per_track: usize,
    snapshot: Arc<Mutex<Option<BridgeSnapshot>>>,
    dropped: Arc<Mutex<HashSet<u64>>>,
    generation: u64,
    /// Incremented by the reader thread when a media record cannot be queued; folded into the
    /// worker's periodic report so a drop storm is visible in `inspect-media`.
    queue_drops: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct TrackMediaQueues {
    tracks: HashMap<BridgeSourceKey, VecDeque<BridgeMedia>>,
    ready: VecDeque<BridgeSourceKey>,
    ready_set: HashSet<BridgeSourceKey>,
}

impl TrackMediaQueues {
    fn push(&mut self, media: BridgeMedia, capacity: usize) -> Result<(), BridgeMedia> {
        let key = media.source;
        let queue = self.tracks.entry(key).or_default();
        if queue.len() >= capacity {
            return Err(media);
        }
        queue.push_back(media);
        if self.ready_set.insert(key) {
            self.ready.push_back(key);
        }
        Ok(())
    }

    #[cfg(test)]
    fn pop(&mut self) -> Option<BridgeMedia> {
        self.pop_where(|_| true)
    }

    fn pop_where(
        &mut self,
        mut accepts: impl FnMut(BridgeSourceKey) -> bool,
    ) -> Option<BridgeMedia> {
        let ready = self.ready.len();
        for _ in 0..ready {
            let key = self.ready.pop_front()?;
            if !accepts(key) {
                self.ready.push_back(key);
                continue;
            }
            self.ready_set.remove(&key);
            let Some(queue) = self.tracks.get_mut(&key) else {
                continue;
            };
            let media = queue.pop_front();
            if queue.is_empty() {
                self.tracks.remove(&key);
            } else {
                self.ready.push_back(key);
                self.ready_set.insert(key);
            }
            return media;
        }
        None
    }

    fn source_keys(&self) -> HashSet<BridgeSourceKey> {
        self.tracks.keys().copied().collect()
    }
}

impl BridgeWorker {
    fn spawn(
        bridge: crate::bridge::OuterBridge,
        client_writer: SharedWriter,
        queue_records: usize,
        presenter_cell_size: Arc<AtomicU32>,
    ) -> io::Result<Self> {
        Self::spawn_inner(
            bridge,
            BridgeClientSender::new(move |message| send_client(&client_writer, &message)),
            queue_records,
            Some(presenter_cell_size),
        )
    }

    pub(crate) fn spawn_with_sender(
        bridge: crate::bridge::OuterBridge,
        client_writer: BridgeClientSender,
        queue_records: usize,
    ) -> io::Result<Self> {
        Self::spawn_inner(bridge, client_writer, queue_records, None)
    }

    fn spawn_inner(
        bridge: crate::bridge::OuterBridge,
        client_writer: BridgeClientSender,
        queue_records: usize,
        presenter_cell_size: Option<Arc<AtomicU32>>,
    ) -> io::Result<Self> {
        let bridge_instance_id = new_bridge_instance_id()?;
        let media = Arc::new(Mutex::new(TrackMediaQueues::default()));
        let (media_wakeup, receiver) = mpsc::sync_channel(1);
        let snapshot = Arc::new(Mutex::new(None));
        let dropped = Arc::new(Mutex::new(HashSet::new()));
        let queue_drops = Arc::new(AtomicU64::new(0));
        let worker_snapshot = snapshot.clone();
        let worker_dropped = dropped.clone();
        let worker_drops = queue_drops.clone();
        let worker_media = media.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let thread = thread::Builder::new()
            .name("vvmux-media-bridge".into())
            .spawn(move || {
                run_bridge_worker(
                    bridge,
                    client_writer,
                    receiver,
                    worker_media,
                    worker_snapshot,
                    worker_dropped,
                    worker_drops,
                    worker_stopped,
                    bridge_instance_id,
                    presenter_cell_size,
                )
            })?;
        Ok(Self {
            media,
            media_wakeup: Some(media_wakeup),
            queue_records_per_track: queue_records,
            snapshot,
            dropped,
            generation: 0,
            queue_drops,
            stopped,
            thread: Some(thread),
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
        let Some(wakeup) = &self.media_wakeup else {
            self.mark_dropped(delivery_id);
            return false;
        };
        let queued = self
            .media
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(media, self.queue_records_per_track)
            .is_ok();
        if !queued {
            self.mark_dropped(delivery_id);
            return false;
        }
        let _ = wakeup.try_send(());
        true
    }

    fn mark_dropped(&self, delivery_id: u64) {
        self.queue_drops.fetch_add(1, Ordering::Relaxed);
        self.dropped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(delivery_id);
    }
}

impl Drop for BridgeWorker {
    fn drop(&mut self) {
        // The bridge owns the outer Vivid connection on its worker thread. Merely dropping the
        // queue sender lets the foreground process return while that thread is still unwinding;
        // process teardown can then reset the Windows socket before OuterBridge sends GOODBYE.
        // Stop and join so detach does not complete until the protocol session is closed cleanly.
        self.stopped.store(true, Ordering::Release);
        self.media_wakeup.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn new_bridge_instance_id() -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(u64::from_le_bytes(bytes) | 1)
}

fn send_bridge_trace(
    client_writer: &BridgeClientSender,
    bridge_instance_id: u64,
    trace_started: Instant,
    source: Option<BridgeSourceKey>,
    kind: MediaTraceKind,
) {
    let _ = client_writer.send(ClientMessage::BridgeTrace {
        bridge_instance_id,
        event: BridgeMediaTraceEvent {
            origin_monotonic_us: u64::try_from(trace_started.elapsed().as_micros())
                .unwrap_or(u64::MAX),
            source,
            kind,
        },
    });
}

fn pack_cell_size(display: crate::ipc::DisplayMetrics) -> u32 {
    u32::from(display.cell_width) | (u32::from(display.cell_height) << 16)
}

fn apply_cell_size(display: &mut crate::ipc::DisplayMetrics, packed: u32) {
    display.cell_width = packed as u16;
    display.cell_height = (packed >> 16) as u16;
}

#[allow(clippy::too_many_arguments)]
fn run_bridge_worker(
    mut bridge: crate::bridge::OuterBridge,
    client_writer: BridgeClientSender,
    receiver: mpsc::Receiver<()>,
    media_queues: Arc<Mutex<TrackMediaQueues>>,
    snapshot: Arc<Mutex<Option<BridgeSnapshot>>>,
    dropped: Arc<Mutex<HashSet<u64>>>,
    queue_drops: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    mut bridge_instance_id: u64,
    presenter_cell_size: Option<Arc<AtomicU32>>,
) {
    let trace_started = Instant::now();
    let mut diagnostic_generation = bridge.diagnostic_instance_generation();
    send_bridge_trace(
        &client_writer,
        bridge_instance_id,
        trace_started,
        None,
        MediaTraceKind::BridgeClientAttached { vivid: true },
    );
    let mut metrics = crate::metrics::BridgeMetrics::default();
    let mut metrics_reported_at = Instant::now();
    let mut traced_queue_drops = 0_u64;
    // Diagnostic only: which form the in-flight raster body for each source uses.
    let mut raster_forms = HashMap::<BridgeSourceKey, bool>::new();
    let mut active_generation = 0;
    let mut minimum_media_generation = HashMap::<BridgeSourceKey, u64>::new();
    let mut retained_rehydration = HashSet::<BridgeSourceKey>::new();
    let mut active_surfaces = Vec::new();
    let mut active_sources = Vec::new();
    let mut active_nodes = Vec::new();
    let mut started_sources = HashSet::new();
    // `videos_needing_keyframes` is level-triggered projection state. Convert it to one request
    // per recovery episode: media-only revisions can otherwise enqueue hundreds of identical
    // requests before the first keyframe delivery acknowledgement reaches the session actor.
    let mut requested_virtual_keyframes = HashSet::<BridgeSourceKey>::new();
    let mut desynchronized_sources = HashSet::new();
    let mut force_sources = false;
    let mut force_replacement = false;
    let mut deferred: Option<BridgeMedia> = None;
    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        match bridge.service_session_events() {
            Ok(Some(display)) => {
                if let Some(cell_size) = &presenter_cell_size {
                    cell_size.store(pack_cell_size(display), Ordering::Release);
                }
                let _ = client_writer.send(ClientMessage::Resize(display));
            }
            Ok(None) => {}
            Err(_) => {
                force_sources = true;
                force_replacement = true;
                let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry {
                    reset_outer_session: true,
                });
            }
        }
        if bridge
            .retry_pending_activation()
            .and_then(|()| bridge.retry_pending_playback())
            .is_err()
        {
            force_sources = true;
            force_replacement = true;
            let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry {
                reset_outer_session: true,
            });
        }
        let mut eos_blocked = media_queues
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .source_keys();
        if let Some(media) = &deferred {
            eos_blocked.insert(media.source);
        }
        bridge.flush_pending_eos(&eos_blocked);
        // Report on a coarse interval rather than per record: this is diagnostic traffic and must
        // never compete with media for the client connection.
        if metrics_reported_at.elapsed() >= METRICS_REPORT_INTERVAL {
            metrics.client_queue_drops = queue_drops.load(Ordering::Relaxed);
            if metrics.client_queue_drops > traced_queue_drops {
                send_bridge_trace(
                    &client_writer,
                    bridge_instance_id,
                    trace_started,
                    None,
                    MediaTraceKind::QueueDrops {
                        dropped: metrics.client_queue_drops - traced_queue_drops,
                        total: metrics.client_queue_drops,
                    },
                );
                traced_queue_drops = metrics.client_queue_drops;
            }
            let (wait_us, wait_timeouts) = bridge.take_control_wait_stats();
            metrics.control_wait_us = metrics.control_wait_us.saturating_add(wait_us);
            metrics.control_wait_timeouts =
                metrics.control_wait_timeouts.saturating_add(wait_timeouts);
            let _ = client_writer.send(ClientMessage::BridgeMetrics(metrics));
            metrics_reported_at = Instant::now();
        }
        let media_completions = bridge.take_media_completions();
        let playback_may_be_ready = media_completions
            .iter()
            .any(|(_, delivered, _, _)| *delivered);
        for (delivery_id, delivered, _outer_record_sequence, object_id) in media_completions {
            if delivery_id != 0 {
                acknowledge_bridge_delivery(&client_writer, delivery_id, delivered);
            } else if delivered {
                // Retained hydration carries no delivery ID, so its success would otherwise be
                // invisible to the server. Report it: a retained image reaching the outer
                // presenter is the moment it is genuinely presented, and a producer waiting on
                // first visible presentation must not be released before then.
                if let Some(source) = bridge.source_for_outer_object(object_id) {
                    let _ = client_writer.send(ClientMessage::BridgeRetainedHydrated { source });
                }
            }
        }
        if playback_may_be_ready && bridge.retry_pending_playback().is_err() {
            force_sources = true;
            force_replacement = true;
            let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry {
                reset_outer_session: true,
            });
        }
        // The outer control connection is serviced here so the bridge keeps naming the target the
        // outer terminal actually has. A resize the bridge never read would make every scene
        // commit stale.
        bridge.poll_outer_session();
        let outer_keyframes = bridge.take_keyframe_requests();
        if !outer_keyframes.is_empty() {
            for request in &outer_keyframes {
                send_bridge_trace(
                    &client_writer,
                    bridge_instance_id,
                    trace_started,
                    Some(request.source),
                    MediaTraceKind::KeyframeRequest {
                        stage: MediaKeyframeStage::OuterRequested,
                        minimum_epoch: request.minimum_epoch,
                        reason: request.reason,
                    },
                );
            }
            let _ = client_writer.send(ClientMessage::BridgeNeedKeyframes(outer_keyframes));
        }
        let outer_full_frames = bridge.take_full_frame_requests();
        if !outer_full_frames.is_empty() {
            let _ = client_writer.send(ClientMessage::BridgeNeedFullFrames(outer_full_frames));
        }
        let source_losses = bridge.take_source_losses();
        if !source_losses.is_empty() {
            for source in &source_losses {
                send_bridge_trace(
                    &client_writer,
                    bridge_instance_id,
                    trace_started,
                    Some(*source),
                    MediaTraceKind::TrackLost,
                );
            }
            desynchronized_sources.extend(source_losses);
            force_sources = true;
            let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry {
                reset_outer_session: false,
            });
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
        if let Some(mut pending) = pending {
            // Ask the producer for the replacement decoder's keyframe before rebuilding that
            // decoder. The server processes this message before BridgeApplied on the same IPC
            // stream and keeps timed ingress parked until the latter, so an already-blocked old
            // packet cannot overtake the recovery request.
            let needing_virtual_keyframes = pending
                .videos_needing_keyframes
                .iter()
                .copied()
                .collect::<HashSet<_>>();
            requested_virtual_keyframes.retain(|source| needing_virtual_keyframes.contains(source));
            let mut early_keyframes = pending
                .videos_needing_keyframes
                .iter()
                .copied()
                .filter(|source| requested_virtual_keyframes.insert(*source))
                .map(|source| BridgeKeyframeRequest {
                    source,
                    minimum_epoch: None,
                    // The gateway raises the minimum epoch for this replacement handoff, so no
                    // same-epoch keyframe queued under the detached presenter can satisfy it.
                    reason: crate::bridge::KEYFRAME_REASON_TRANSPORT_LOSS,
                })
                .collect::<Vec<_>>();
            early_keyframes.sort_by_key(|request| {
                (
                    request.source.producer,
                    request.source.context,
                    request.source.surface,
                    request.source.track,
                )
            });
            if !early_keyframes.is_empty() {
                let _ = client_writer.send(ClientMessage::BridgeNeedKeyframes(early_keyframes));
            }
            let recovering_surfaces = needing_virtual_keyframes
                .iter()
                .map(|source| (source.producer, source.context, source.surface))
                .collect::<HashSet<_>>();
            for source in &mut pending.tracks {
                if recovering_surfaces.contains(&(
                    source.key.producer,
                    source.key.context,
                    source.key.surface,
                )) {
                    // Do not transiently start a replacement surface with the stale pre-handoff
                    // PLAY. Vivi publishes an exact recovery PLAY before its keyframe; keeping the
                    // rebuilt tracks paused makes that the only clock the new presenter observes.
                    source.playing = false;
                }
            }
            let change = if force_sources {
                ProjectionChange::Sources
            } else {
                compare_projection(
                    &active_surfaces,
                    &active_sources,
                    &active_nodes,
                    &pending.surfaces,
                    &pending.tracks,
                    &pending.nodes,
                )
            };
            let mut recreated = HashSet::new();
            let source_scoped_recovery = !desynchronized_sources.is_empty();
            if change == ProjectionChange::Sources && force_replacement {
                metrics.session_replacements = metrics.session_replacements.saturating_add(1);
            }
            let applied = match change {
                ProjectionChange::PlaybackOnly => {
                    bridge.update_playback(&active_sources, &pending.tracks)
                }
                // Scene-only changes (occlusion fragments, node moves) reconcile nodes in the
                // current outer session: no pause, no source work, no retained-body replay,
                // no keyframe request. A global pause here would stall playback because
                // nothing re-issues PLAY for sources that were not recreated.
                ProjectionChange::SceneOnly => bridge
                    .update_nodes(&pending.nodes)
                    .and_then(|()| bridge.update_playback(&active_sources, &pending.tracks)),
                ProjectionChange::Sources => {
                    let result = if force_replacement {
                        bridge.replace_session(&pending.surfaces, &pending.tracks, &pending.nodes)
                    } else {
                        bridge.rebuild(&pending.surfaces, &pending.tracks, &pending.nodes)
                    };
                    result.map(|sources| recreated = sources)
                }
            };
            if let Err(error) = applied {
                // A display generation can change between BEGIN_TXN and COMMIT_TXN. The bridge
                // retries that transaction in place; if the display remains unsettled through its
                // bounded retry window, request a fresh authoritative projection without tearing
                // down healthy sources or replacing the presenter session.
                let same_session_retry =
                    error.kind() == io::ErrorKind::WouldBlock || source_scoped_recovery;
                force_sources = source_scoped_recovery || !same_session_retry;
                force_replacement = !same_session_retry;
                let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry {
                    reset_outer_session: !same_session_retry,
                });
                continue;
            }
            let next_generation = bridge.diagnostic_instance_generation();
            if next_generation != diagnostic_generation {
                diagnostic_generation = next_generation;
                bridge_instance_id = bridge_instance_id.wrapping_add(2);
                send_bridge_trace(
                    &client_writer,
                    bridge_instance_id,
                    trace_started,
                    None,
                    MediaTraceKind::BridgeClientAttached { vivid: true },
                );
            }
            trace_projection_change(
                &client_writer,
                bridge_instance_id,
                trace_started,
                change,
                &active_sources,
                &pending.tracks,
                &recreated,
                &bridge.attachment_generations(),
            );
            let outer_revision = bridge.mark_projection_applied();
            let outer_attachment_generations = bridge.attachment_generations();
            let _ = client_writer.send(ClientMessage::BridgeApplied {
                bridge_instance_id,
                virtual_revision: pending.virtual_revision,
                outer_revision,
                outer_attachment_generations,
            });

            if change == ProjectionChange::Sources {
                let current = pending
                    .tracks
                    .iter()
                    .map(|source| source.key)
                    .collect::<HashSet<_>>();
                minimum_media_generation.retain(|source, _| current.contains(source));
                retained_rehydration.retain(|source| current.contains(source));
                for source in pending
                    .tracks
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
                requested_virtual_keyframes.retain(|source| current.contains(source));
            }
            force_sources = false;
            force_replacement = false;
            active_generation = pending.generation;
            let mut keyframes = HashMap::new();
            keyframes.extend(desynchronized_sources.drain().filter_map(|source| {
                pending
                    .tracks
                    .iter()
                    .any(|candidate| {
                        candidate.key == source
                            && matches!(&candidate.kind, BridgeSourceKind::Video { .. })
                    })
                    .then_some((
                        source,
                        BridgeKeyframeRequest {
                            source,
                            minimum_epoch: None,
                            reason: crate::bridge::KEYFRAME_REASON_DECODER_ERROR,
                        },
                    ))
            }));
            if change == ProjectionChange::Sources {
                keyframes.extend(pending.tracks.iter().filter_map(|source| {
                    (recreated.contains(&source.key)
                        && source.playing
                        && !requested_virtual_keyframes.contains(&source.key)
                        && matches!(source.kind, BridgeSourceKind::Video { .. }))
                    .then_some((
                        source.key,
                        BridgeKeyframeRequest {
                            source: source.key,
                            minimum_epoch: None,
                            reason: crate::bridge::KEYFRAME_REASON_INITIAL,
                        },
                    ))
                }));
            }
            active_surfaces = pending.surfaces;
            active_sources = pending.tracks;
            active_nodes = pending.nodes;
            started_sources.extend(
                active_sources
                    .iter()
                    .filter(|source| source_is_playing(&active_sources, source.key))
                    .map(|source| source.key),
            );
            if !keyframes.is_empty() {
                let mut keyframes = keyframes.into_values().collect::<Vec<_>>();
                keyframes.sort_by_key(|request| {
                    (
                        request.source.producer,
                        request.source.context,
                        request.source.surface,
                        request.source.track,
                    )
                });
                let _ = client_writer.send(ClientMessage::BridgeNeedKeyframes(keyframes));
            }
            // Fall through to the media queue rather than restarting the loop. The projection is
            // current as of here, so the queued records can be applied against it, and a session
            // that keeps publishing snapshots - an outer resize does, once per relayout - must not
            // be able to postpone them indefinitely. A queue that never drains stays full, and a
            // full queue drops records, and every dropped record asks for another snapshot: the
            // starvation would sustain itself and strand the record whose delivery a nested
            // producer is waiting on.
        }

        if complete_dropped_deliveries(&client_writer, &dropped) {
            // Reconcile sources on the existing outer session. A record this client could not
            // queue says nothing about the outer session's identities, and replacing it would
            // re-arm the retained replay for every source at once.
            force_sources = true;
        }
        let media = match deferred.take().or_else(|| {
            media_queues
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_where(|source| bridge.can_accept_media(source))
        }) {
            Some(media) => media,
            None => match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
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
        // acknowledges it after the outer presenter returns the corresponding ingress capacity,
        // replenishing the virtual presenter's initial grant so Vivi can supply enough linked
        // audio and reordered video to issue PLAY. Only reject media for a source that had
        // started and is now stopped.
        if matches!(
            media.record_type,
            vivid_protocol::messages::VIDEO_PACKET | vivid_protocol::messages::AUDIO_PACKET
        ) && !source_is_playing(&active_sources, media.source)
            && started_sources.contains(&media.source)
        {
            acknowledge_bridge_delivery(&client_writer, media.delivery_id, false);
            continue;
        }
        let is_raster = media.record_type == vivid_protocol::messages::RASTER_FRAME;
        // Only the chunk carrying the frame header reveals the form, and the counters are applied
        // on the last chunk, so remember it across a fragmented body.
        if is_raster && media.offset == 0 && media.bytes.len() >= 8 {
            let flags = u32::from_be_bytes(media.bytes[4..8].try_into().expect("checked length"));
            raster_forms.insert(
                media.source,
                flags & vivid_protocol::media::RASTER_FRAME_DELTA != 0,
            );
        }
        let is_raster_delta =
            is_raster && raster_forms.get(&media.source).copied().unwrap_or_default();
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
                metrics.outer_media_records = metrics.outer_media_records.saturating_add(1);
                metrics.outer_media_bytes =
                    metrics.outer_media_bytes.saturating_add(media.total.into());
                if is_raster {
                    metrics.inner_raster_bytes = metrics
                        .inner_raster_bytes
                        .saturating_add(media.total.into());
                    if is_raster_delta {
                        metrics.outer_raster_delta_frames =
                            metrics.outer_raster_delta_frames.saturating_add(1);
                    } else {
                        metrics.outer_raster_full_frames =
                            metrics.outer_raster_full_frames.saturating_add(1);
                    }
                }
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
    send_bridge_trace(
        &client_writer,
        bridge_instance_id,
        trace_started,
        None,
        MediaTraceKind::BridgeClientDetached,
    );
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
    previous_surfaces: &[BridgeSurface],
    previous_sources: &[BridgeSource],
    previous_nodes: &[BridgeNode],
    current_surfaces: &[BridgeSurface],
    current_sources: &[BridgeSource],
    current_nodes: &[BridgeNode],
) -> ProjectionChange {
    let surfaces_unchanged = previous_surfaces.len() == current_surfaces.len()
        && previous_surfaces
            .iter()
            .all(|previous| current_surfaces.contains(previous));
    let sources_unchanged = previous_sources.len() == current_sources.len()
        && previous_sources.iter().all(|previous| {
            current_sources
                .iter()
                .any(|current| previous.key == current.key && previous.kind == current.kind)
        });
    if !surfaces_unchanged || !sources_unchanged {
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

#[allow(clippy::too_many_arguments)]
fn trace_projection_change(
    client_writer: &BridgeClientSender,
    bridge_instance_id: u64,
    trace_started: Instant,
    change: ProjectionChange,
    previous: &[BridgeSource],
    current: &[BridgeSource],
    recreated: &HashSet<BridgeSourceKey>,
    attachment_generations: &[(BridgeSourceKey, u64)],
) {
    if change == ProjectionChange::Sources {
        let mut removed = previous
            .iter()
            .filter(|source| !current.iter().any(|next| next.key == source.key))
            .map(|source| source.key)
            .collect::<Vec<_>>();
        removed.sort_by_key(|source| {
            (
                source.producer,
                source.context,
                source.surface,
                source.track,
            )
        });
        for source in removed {
            send_bridge_trace(
                client_writer,
                bridge_instance_id,
                trace_started,
                Some(source),
                MediaTraceKind::OuterTrackRemoved,
            );
        }
    }

    let mut ordered = current.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|source| {
        (
            source.key.producer,
            source.key.context,
            source.key.surface,
            source.key.track,
        )
    });
    for source in ordered {
        if recreated.contains(&source.key) {
            let attachment_generation = attachment_generations
                .iter()
                .find_map(|(key, generation)| (*key == source.key).then_some(*generation))
                .unwrap_or(0);
            send_bridge_trace(
                client_writer,
                bridge_instance_id,
                trace_started,
                Some(source.key),
                MediaTraceKind::OuterTrackRecreated {
                    attachment_generation,
                    playing: source.playing,
                },
            );
            if source.playing {
                send_bridge_trace(
                    client_writer,
                    bridge_instance_id,
                    trace_started,
                    Some(source.key),
                    MediaTraceKind::PlaybackControl {
                        control: MediaPlaybackControl::Play,
                        request: Some(source.play_request),
                    },
                );
            }
            if source.eos_epoch.is_some() {
                send_bridge_trace(
                    client_writer,
                    bridge_instance_id,
                    trace_started,
                    Some(source.key),
                    MediaTraceKind::PlaybackControl {
                        control: MediaPlaybackControl::Eos,
                        request: None,
                    },
                );
            }
            continue;
        }
        let Some(old) = previous.iter().find(|old| old.key == source.key) else {
            continue;
        };
        if old.playing != source.playing
            || (source.playing && old.play_request != source.play_request)
        {
            send_bridge_trace(
                client_writer,
                bridge_instance_id,
                trace_started,
                Some(source.key),
                MediaTraceKind::PlaybackControl {
                    control: if source.playing {
                        MediaPlaybackControl::Play
                    } else {
                        MediaPlaybackControl::Pause
                    },
                    request: source.playing.then_some(source.play_request),
                },
            );
        }
        if old.eos_epoch != source.eos_epoch && source.eos_epoch.is_some() {
            send_bridge_trace(
                client_writer,
                bridge_instance_id,
                trace_started,
                Some(source.key),
                MediaTraceKind::PlaybackControl {
                    control: MediaPlaybackControl::Eos,
                    request: None,
                },
            );
        }
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
        // A retained body the local queue could not hold has to be replayed, but the outer
        // session that would have received it is still valid. Resetting it would discard every
        // attachment, and an unattached source is replayed on every projection sync: the drop
        // that asked for the reset would produce the replay that overflows the queue again. Ask
        // for the snapshot on the session already in place.
        let _ = client_writer.send(ClientMessage::BridgeSnapshotRetry {
            reset_outer_session: false,
        });
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

fn write_title(output: &TerminalOutput, title: &str) {
    let sanitized = title.replace(['\x07', '\x1b'], "");
    output.enqueue_control(format!("\x1b]2;{sanitized}\x1b\\").into_bytes());
}

/// One unit of work for the terminal writer thread.
enum OutputJob {
    /// A chunk of one server frame. `last` carries the frame's acknowledgement.
    Frame {
        frame_id: u64,
        last: bool,
        bytes: Vec<u8>,
    },
    /// Title, bell, or clipboard bytes, which are not part of the frame diff stream.
    Control(Vec<u8>),
}

#[derive(Default)]
struct OutputQueue {
    jobs: VecDeque<OutputJob>,
    bytes: usize,
    stopped: bool,
}

/// Terminal writes, moved off the thread that dispatches Vivid media.
///
/// Writing to the terminal blocks whenever the outer terminal is behind on reading its PTY. While
/// that write was inline on the reader thread, it also stalled `MediaSnapshot` and `MediaRecord`
/// dispatch, so a busy outer terminal froze the projected scene and video rather than just the
/// text. Frames now queue here and the reader thread returns immediately.
struct TerminalOutput {
    queue: Arc<(Mutex<OutputQueue>, Condvar)>,
}

/// Bound on queued terminal bytes.
///
/// Frame diffs are incremental, so a queued frame can never be dropped in isolation without
/// corrupting the screen. When the bound is reached the queue is cleared and the server is asked
/// for a full redraw, which is the one supersede that is safe.
const OUTPUT_QUEUE_BYTES: usize = 8 * 1024 * 1024;

impl TerminalOutput {
    fn spawn(output: Arc<Mutex<Box<dyn Write + Send>>>, writer: SharedWriter) -> io::Result<Self> {
        let queue = Arc::new((Mutex::new(OutputQueue::default()), Condvar::new()));
        let worker = queue.clone();
        thread::Builder::new()
            .name("vvmux-terminal-output".into())
            .spawn(move || run_terminal_output(&output, &writer, &worker))?;
        Ok(Self { queue })
    }

    fn enqueue_control(&self, bytes: Vec<u8>) {
        self.push(OutputJob::Control(bytes));
    }

    /// Queue one frame chunk. Returns false when the queue overflowed and the caller must ask the
    /// server to resynchronize with a full redraw.
    fn enqueue_frame(&self, frame_id: u64, full: bool, last: bool, bytes: Vec<u8>) -> bool {
        let (lock, signal) = &*self.queue;
        let mut queue = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // A full redraw makes every earlier diff irrelevant, so it is the one point where the
        // backlog can be discarded without losing screen state.
        if full {
            queue
                .jobs
                .retain(|job| matches!(job, OutputJob::Control(_)));
            queue.bytes = 0;
        }
        if queue.bytes.saturating_add(bytes.len()) > OUTPUT_QUEUE_BYTES {
            queue.jobs.clear();
            queue.bytes = 0;
            return false;
        }
        queue.bytes = queue.bytes.saturating_add(bytes.len());
        queue.jobs.push_back(OutputJob::Frame {
            frame_id,
            last,
            bytes,
        });
        signal.notify_one();
        true
    }

    fn push(&self, job: OutputJob) {
        let (lock, signal) = &*self.queue;
        let mut queue = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let OutputJob::Control(bytes) = &job {
            queue.bytes = queue.bytes.saturating_add(bytes.len());
        }
        queue.jobs.push_back(job);
        signal.notify_one();
    }

    fn stop(&self) {
        let (lock, signal) = &*self.queue;
        lock.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stopped = true;
        signal.notify_all();
    }
}

fn run_terminal_output(
    output: &Arc<Mutex<Box<dyn Write + Send>>>,
    writer: &SharedWriter,
    queue: &Arc<(Mutex<OutputQueue>, Condvar)>,
) {
    let (lock, signal) = &**queue;
    loop {
        let job = {
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                if let Some(job) = state.jobs.pop_front() {
                    let (OutputJob::Frame { bytes, .. } | OutputJob::Control(bytes)) = &job;
                    state.bytes = state.bytes.saturating_sub(bytes.len());
                    break job;
                }
                if state.stopped {
                    return;
                }
                state = signal
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        match job {
            OutputJob::Frame {
                frame_id,
                last,
                bytes,
            } => {
                if write_output(output, &bytes).is_err() {
                    return;
                }
                // Acknowledge only after the bytes reached the terminal. The server uses this as
                // flow control, so acknowledging on receipt would let it keep producing frames
                // this client has not managed to display.
                if last && send_client(writer, &ClientMessage::RenderAck(frame_id)).is_err() {
                    return;
                }
            }
            OutputJob::Control(bytes) => {
                if write_output(output, &bytes).is_err() {
                    return;
                }
            }
        }
    }
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

    fn test_surface(key: BridgeSourceKey) -> BridgeSurface {
        BridgeSurface {
            key: crate::ipc::BridgeSurfaceKey {
                producer: key.producer,
                context: key.context,
                surface: key.surface,
            },
            logical_width: 16,
            logical_height: 16,
            capture_policy: 0,
            descriptor: crate::ipc::BridgeSourceDescriptor {
                role: 1,
                title: "test surface".into(),
                content_revision: 1,
                semantic_availability: 0,
                locator: String::new(),
            },
        }
    }

    #[test]
    fn presenter_cell_size_survives_terminal_grid_polling() {
        let presenter = crate::ipc::DisplayMetrics {
            columns: 120,
            rows: 42,
            cell_width: 10,
            cell_height: 25,
        };
        let mut terminal_resize = crate::ipc::DisplayMetrics {
            columns: 121,
            rows: 43,
            cell_width: 9,
            cell_height: 24,
        };

        apply_cell_size(&mut terminal_resize, pack_cell_size(presenter));

        assert_eq!((terminal_resize.columns, terminal_resize.rows), (121, 43));
        assert_eq!(
            (terminal_resize.cell_width, terminal_resize.cell_height),
            (10, 25),
            "the TTY supplies the live grid, but nested raster pixels must use presenter cells"
        );
    }

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

    #[cfg(unix)]
    fn receive_bridge_message_until(
        receiver: &mpsc::Receiver<ClientMessage>,
        seen: &mut Vec<ClientMessage>,
        description: &str,
        predicate: impl Fn(&ClientMessage) -> bool,
    ) -> usize {
        loop {
            let message = receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|error| {
                    panic!("timed out waiting for {description}: {error}; messages={seen:#?}")
                });
            seen.push(message);
            let index = seen.len() - 1;
            if predicate(&seen[index]) {
                return index;
            }
        }
    }

    #[cfg(unix)]
    fn project_outer_timed_sources(
        presenter: &crate::media::VirtualVivid,
        expected_sources: usize,
    ) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
            if snapshot.sources.len() == expected_sources {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "outer test presenter never created {expected_sources} timed sources"
            );
            thread::sleep(Duration::from_millis(2));
        }
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
    fn recreated_playing_video_trace_orders_decoder_recreation_before_play() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let captured = messages.clone();
        let sender = BridgeClientSender::new(move |message| {
            captured.lock().unwrap().push(message);
            Ok(())
        });
        let key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
        };
        let source = BridgeSource {
            key,
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
                start_pts_us: 8_033_333,
                minimum_buffer_us: 33_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
            },
        };

        trace_projection_change(
            &sender,
            19,
            Instant::now(),
            ProjectionChange::Sources,
            &[],
            std::slice::from_ref(&source),
            &HashSet::from([key]),
            &[(key, 2)],
        );

        let messages = messages.lock().unwrap();
        assert!(matches!(
            &messages[0],
            ClientMessage::BridgeTrace {
                bridge_instance_id: 19,
                event: BridgeMediaTraceEvent {
                    source: Some(source),
                    kind: MediaTraceKind::OuterTrackRecreated {
                        attachment_generation: 2,
                        playing: true,
                    },
                    ..
                },
            } if *source == key
        ));
        assert!(matches!(
            &messages[1],
            ClientMessage::BridgeTrace {
                event: BridgeMediaTraceEvent {
                    source: Some(source),
                    kind: MediaTraceKind::PlaybackControl {
                        control: MediaPlaybackControl::Play,
                        request: Some(request),
                    },
                    ..
                },
                ..
            } if *source == key && request.start_pts_us == 8_033_333
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
            scanner.expire(Instant::now() + client_input::ESCAPE_DELAY),
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

    /// A terminal that has stopped draining must not stall the thread that dispatches Vivid
    /// media. This is the failure that made a hidden tab's image stick and video freeze while the
    /// outer terminal was behind: both travel through the same reader thread.
    #[test]
    fn a_stalled_terminal_does_not_block_frame_enqueue() {
        /// A terminal whose writes never complete until released.
        struct StalledTerminal(Arc<(Mutex<bool>, Condvar)>);

        impl Write for StalledTerminal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let (lock, signal) = &*self.0;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = signal.wait(released).unwrap();
                }
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let output: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(StalledTerminal(gate.clone()))));
        let writer = crate::ipc::test_shared_writer(Box::new(io::sink()));
        let terminal = TerminalOutput::spawn(output, writer).unwrap();

        // The writer thread is parked inside the first frame's write.
        assert!(terminal.enqueue_frame(1, true, true, vec![b'a'; 16]));
        let start = Instant::now();
        for frame in 2..64_u64 {
            assert!(
                terminal.enqueue_frame(frame, false, true, vec![b'b'; 16]),
                "queueing must not depend on the terminal draining"
            );
        }
        terminal.enqueue_control(b"\x07".to_vec());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "enqueue blocked on the stalled terminal"
        );

        // Past the byte bound the backlog is discarded and the caller is told to resynchronize,
        // because an incremental diff can never be dropped on its own.
        assert!(!terminal.enqueue_frame(64, false, true, vec![0; OUTPUT_QUEUE_BYTES + 1]));

        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        terminal.stop();
    }

    #[test]
    fn full_bridge_media_queue_drops_instead_of_blocking_input() {
        let (media_wakeup, receiver) = mpsc::sync_channel(1);
        let dropped = Arc::new(Mutex::new(HashSet::new()));
        let queue_drops = Arc::new(AtomicU64::new(0));
        let mut worker = BridgeWorker {
            media: Arc::new(Mutex::new(TrackMediaQueues::default())),
            media_wakeup: Some(media_wakeup),
            queue_records_per_track: 1,
            snapshot: Arc::new(Mutex::new(None)),
            dropped: dropped.clone(),
            generation: 1,
            queue_drops: queue_drops.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        let key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
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
        let audio_key = BridgeSourceKey { track: 8, ..key };
        assert!(
            worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id: 12,
                source: audio_key,
                record_type: vivid_protocol::messages::AUDIO_PACKET,
                offset: 0,
                total: 1,
                last: true,
                bytes: vec![0],
            }),
            "a full video-track queue must not consume the audio track's bounded queue"
        );
        assert!(
            dropped
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&11)
        );
        receiver.try_recv().unwrap();
        assert_eq!(
            worker
                .media
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop()
                .unwrap()
                .generation,
            1
        );
        // The drop is counted for `inspect-media` without changing the drop behavior itself.
        assert_eq!(queue_drops.load(Ordering::Relaxed), 1);
    }

    /// The outer terminal resizes while the nested session is live. The bridge names the target
    /// generation on every scene commit, so it has to follow the outer target: a bridge that never
    /// reads the announcement has every later commit rejected, and the recovery for that is
    /// replacing the whole outer session instead of moving one node.
    ///
    /// The commit here deliberately runs before the announcement is read, which is what a resize
    /// landing mid-transaction looks like, so the recovery is the one under test.
    #[test]
    #[cfg(unix)]
    fn a_scene_commit_that_crosses_an_outer_resize_stays_in_the_same_outer_session() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-resize.sock");
        let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping outer resize bridge socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual outer presenter start failed: {error}"),
        };
        let token = presenter.issue_pane_capability(7).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));
        let mut bridge = crate::bridge::OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new(token),
            DisplayMetrics::default(),
        )
        .unwrap();

        let key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
        };
        let surface = test_surface(key);
        let node = |width: i64| BridgeNode {
            producer: key.producer,
            node: 1,
            fragment: 0,
            surface: surface.key,
            x: 0,
            y: 0,
            width,
            height: 1_i64 << 32,
            z_index: 0,
            visible: true,
            clip: crate::ipc::BridgeClipRect {
                x: 0,
                y: 0,
                width,
                height: 1_i64 << 32,
            },
        };
        bridge
            .rebuild(std::slice::from_ref(&surface), &[], &[node(2_i64 << 32)])
            .expect("initial projection");

        for (index, (columns, rows)) in [(100_u16, 30_u16), (64, 18)].into_iter().enumerate() {
            presenter.update_metrics(7, columns, rows, (10, 20));

            let width = (4 + index as i64) << 32;
            bridge.update_nodes(&[node(width)]).unwrap_or_else(|error| {
                panic!("a node move across outer resize {index} left the outer session: {error}")
            });
        }
    }

    /// A target announcement a nested producer cannot apply is worse than none: it is discarded,
    /// and the producer keeps naming the target it read at `WELCOME` for the rest of the session.
    #[test]
    #[cfg(unix)]
    fn an_outer_target_announcement_is_one_a_nested_producer_can_apply() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-announce.sock");
        let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping outer announcement socket test: {error}");
                return;
            }
            Err(error) => panic!("virtual outer presenter start failed: {error}"),
        };
        let token = presenter.issue_pane_capability(7).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));
        let mut bridge = crate::bridge::OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new(token),
            DisplayMetrics::default(),
        )
        .unwrap();

        presenter.update_metrics(7, 100, 30, (10, 20));

        let followed = (0..200).any(|_| {
            let moved = bridge.poll_outer_session();
            if !moved {
                thread::sleep(Duration::from_millis(5));
            }
            moved
        });
        assert!(
            followed,
            "the bridge could not apply the outer target announcement"
        );
    }

    #[test]
    #[cfg(unix)]
    fn bridge_applies_outer_target_changes_before_the_next_scene_commit() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-resize.sock");
        let presenter = VirtualVivid::start(socket.clone(), MediaConfig::default()).unwrap();
        presenter.update_metrics(7, 80, 22, (10, 20));
        let secret = presenter.issue_pane_capability(7).unwrap();
        let mut bridge = crate::bridge::OuterBridge::connect(
            format!("unix:{}", socket.display()),
            Zeroizing::new(secret),
            DisplayMetrics::default(),
        )
        .unwrap();

        presenter.update_metrics(7, 120, 40, (9, 18));
        let deadline = Instant::now() + Duration::from_secs(1);
        let resized = loop {
            match bridge.service_session_events().unwrap() {
                Some(display) => break display,
                None => {
                    assert!(
                        Instant::now() < deadline,
                        "outer TARGET_CHANGED was not applied"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };
        assert_eq!(
            resized,
            DisplayMetrics {
                columns: 120,
                rows: 40,
                cell_width: 9,
                cell_height: 18,
            }
        );
        assert_eq!(bridge.display_metrics(), resized);
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
        let presenter_cell_size =
            Arc::new(AtomicU32::new(pack_cell_size(bridge.display_metrics())));
        let mut worker =
            BridgeWorker::spawn(bridge, client_writer, 8, presenter_cell_size).unwrap();
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
            context: 1,
            surface: 7,
            track: 7,
        };
        let source = BridgeSource {
            key,
            kind: BridgeSourceKind::Raster {
                width: 2,
                height: 1,
                alpha_mode: 1,
                compression_mode: 1,
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
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
            },
        };
        let node = BridgeNode {
            producer: key.producer,
            node: 1,
            fragment: 0,
            surface: test_surface(key).key,
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
            surfaces: vec![test_surface(key)],
            tracks: vec![source],
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

    /// A page-sized raster relay: geometry, compression, and one page turn's worth of pixels.
    #[cfg(unix)]
    const PAGE_WIDTH: u32 = 256;
    #[cfg(unix)]
    const PAGE_HEIGHT: u32 = 64;
    #[cfg(unix)]
    const PAGE_DELTA_OPERATIONS: u32 = 8;

    /// A document page: a light background with a few dark bands, like rendered text.
    #[cfg(unix)]
    fn page_pixels(shade: u8) -> Vec<u8> {
        let mut pixels = vec![0xf5_u8; (PAGE_WIDTH * PAGE_HEIGHT * 4) as usize];
        for (index, byte) in pixels.iter_mut().enumerate() {
            let row = index as u32 / (PAGE_WIDTH * 4);
            if index % 4 == 3 {
                *byte = 0xff;
            } else if row % 8 < 2 {
                *byte = shade;
            }
        }
        pixels
    }

    #[cfg(unix)]
    fn page_frame_body(frame_id: u64, shade: u8) -> Vec<u8> {
        media::raster_frame_body(1, frame_id, PAGE_WIDTH, PAGE_HEIGHT, &page_pixels(shade)).unwrap()
    }

    #[cfg(unix)]
    fn page_source(key: BridgeSourceKey) -> BridgeSource {
        BridgeSource {
            key,
            kind: BridgeSourceKind::Raster {
                width: PAGE_WIDTH,
                height: PAGE_HEIGHT,
                alpha_mode: 1,
                compression_mode: 1,
                delta_operation_limit: Some(PAGE_DELTA_OPERATIONS),
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
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
            },
        }
    }

    #[cfg(unix)]
    fn page_node(key: BridgeSourceKey) -> BridgeNode {
        BridgeNode {
            producer: key.producer,
            node: 1,
            fragment: 0,
            surface: test_surface(key).key,
            x: 0,
            y: 0,
            width: 8_i64 << 32,
            height: 4_i64 << 32,
            z_index: 0,
            visible: true,
            clip: crate::ipc::BridgeClipRect {
                x: 0,
                y: 0,
                width: 8_i64 << 32,
                height: 4_i64 << 32,
            },
        }
    }

    /// The outer track mirrors the nested track's compression, so the relay has to use it.
    ///
    /// A nested document reader compresses its page turns, and a rendered page compresses by more
    /// than an order of magnitude. Relaying the decoded pixels raw put a whole framebuffer on the
    /// outer connection for every page turn: free over a local socket, and the dominant cost over
    /// a forwarded one, which is where nested reading actually got slow. Scrolling and panning
    /// send deltas rather than page turns, so both forms are covered here.
    #[test]
    #[cfg(unix)]
    fn a_relayed_raster_page_keeps_its_compression_on_the_outer_link() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-page.sock");
        let (outer_media_sender, outer_media_receiver) = mpsc::sync_channel(8);
        let presenter = match VirtualVivid::start_with_events(
            socket.clone(),
            MediaConfig::default(),
            Some(outer_media_sender),
        ) {
            Ok(presenter) => presenter,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping relayed raster compression socket test: {error}");
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
        let (_server_reader, _server_writer) =
            establish(test_transport(server), ChannelKind::Control).unwrap();
        let (_client_reader, client_writer) = client_establish.join().unwrap().unwrap();
        let presenter_cell_size =
            Arc::new(AtomicU32::new(pack_cell_size(bridge.display_metrics())));
        let mut worker =
            BridgeWorker::spawn(bridge, client_writer, 8, presenter_cell_size).unwrap();

        let key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 1,
            surfaces: vec![test_surface(key)],
            tracks: vec![page_source(key)],
            nodes: vec![page_node(key)],
            videos_needing_keyframes: Vec::new(),
        });

        // The nested producer's own frame is uncompressed here on purpose: the outer hop owes the
        // link its compression regardless of the form the inner hop happened to use.
        let frame = page_frame_body(1, 0x10);
        assert!(worker.queue_media(BridgeMedia {
            generation: 0,
            delivery_id: 41,
            source: key,
            record_type: vivid_protocol::messages::RASTER_FRAME,
            offset: 0,
            total: frame.len() as u32,
            last: true,
            bytes: frame.clone(),
        }));

        let page = outer_media_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the outer presenter never received the relayed page");
        let relayed = page.body;
        let flags = u32::from_be_bytes(relayed[4..8].try_into().unwrap());
        assert_ne!(
            flags & media::RASTER_FRAME_ZSTD,
            0,
            "a relayed page must not reach the outer link as raw pixels"
        );
        assert!(
            relayed.len() * 4 < frame.len(),
            "the relayed page is {} bytes against a {} byte framebuffer, so it was not compressed \
             in any useful sense",
            relayed.len(),
            frame.len()
        );
        // The outer presenter validates every raster body it accepts by decoding it, so a body it
        // took is already legible; check that the compressed relay kept the pixels too.
        let decoded = media::decode_raster_pixels(
            media::parse_full_raster_frame(&relayed).expect("relayed page parses"),
        )
        .expect("relayed page decodes");
        assert_eq!(decoded, page_pixels(0x10), "the relayed page lost pixels");
        presenter.complete_bridge_delivery(page.delivery_id, true);

        // The same obligation on a scroll: a delta large enough to be worth compressing is one the
        // outer link must not carry raw either.
        let band = vec![0x40_u8; (PAGE_WIDTH * PAGE_HEIGHT / 2 * 4) as usize];
        let delta = media::raster_delta_frame_body(
            1,
            2,
            1,
            0,
            0,
            PAGE_WIDTH,
            PAGE_HEIGHT,
            PAGE_DELTA_OPERATIONS,
            &[media::RasterDeltaOperation::Overwrite {
                x: 0,
                y: 0,
                width: PAGE_WIDTH,
                height: PAGE_HEIGHT / 2,
                rgba: &band,
            }],
            false,
        )
        .unwrap();
        assert!(worker.queue_media(BridgeMedia {
            generation: 0,
            delivery_id: 42,
            source: key,
            record_type: vivid_protocol::messages::RASTER_FRAME,
            offset: 0,
            total: delta.len() as u32,
            last: true,
            bytes: delta.clone(),
        }));

        let relayed = outer_media_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the outer presenter never received the relayed delta")
            .body;
        let flags = u32::from_be_bytes(relayed[4..8].try_into().unwrap());
        assert_ne!(
            flags & media::RASTER_FRAME_DELTA,
            0,
            "the delta became a full frame"
        );
        assert_ne!(
            flags & media::RASTER_FRAME_ZSTD,
            0,
            "a relayed delta must not reach the outer link as raw pixels"
        );
        assert!(
            relayed.len() * 4 < delta.len(),
            "the relayed delta is {} bytes against a {} byte delta, so it was not compressed in \
             any useful sense",
            relayed.len(),
            delta.len()
        );
    }

    #[test]
    #[cfg(unix)]
    fn bridge_forwards_linked_preroll_and_applies_play_before_video() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("outer-vivid.sock");
        let (outer_media_sender, outer_media_receiver) = mpsc::sync_channel(8);
        let presenter = match VirtualVivid::start_with_events(
            socket.clone(),
            MediaConfig::default(),
            Some(outer_media_sender),
        ) {
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
        let presenter_cell_size =
            Arc::new(AtomicU32::new(pack_cell_size(bridge.display_metrics())));
        let mut worker =
            BridgeWorker::spawn(bridge, client_writer, 8, presenter_cell_size).unwrap();

        let key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
        };
        let video_key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 8,
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
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 100_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
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
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 0,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
            },
        };
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 1,
            surfaces: vec![test_surface(video_key)],
            tracks: vec![video_source.clone(), audio_source.clone()],
            nodes: Vec::new(),
            videos_needing_keyframes: vec![video_key],
        });
        project_outer_timed_sources(&presenter, 2);
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
        let (keyframe_sender, keyframe_receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(message) = server_reader.recv::<ClientMessage>() {
                match message {
                    ClientMessage::BridgeMediaAck {
                        delivery_id,
                        delivered,
                    } => {
                        let _ = ack_sender.send((delivery_id, delivered));
                    }
                    ClientMessage::BridgeNeedKeyframes(requests) => {
                        let _ = keyframe_sender.send(requests);
                    }
                    _ => {}
                }
            }
        });
        let requests = keyframe_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].source, video_key);
        assert_eq!(
            requests[0].reason,
            crate::bridge::KEYFRAME_REASON_TRANSPORT_LOSS
        );
        let outer_audio = outer_media_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("outer presenter did not receive linked-audio pre-roll");
        assert_eq!(
            outer_audio.record_type,
            vivid_protocol::messages::AUDIO_PACKET
        );
        assert!(
            ack_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "pre-PLAY delivery was acknowledged before outer ingress became reusable"
        );
        assert!(!presenter.complete_bridge_delivery(outer_audio.delivery_id, true));
        assert_eq!(
            ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            (42, true),
            "pre-PLAY media must replenish the virtual presenter's one-packet grant"
        );
        for virtual_revision in 2..=8 {
            worker.replace_snapshot(BridgeSnapshot {
                generation: 0,
                virtual_revision,
                surfaces: vec![test_surface(video_key)],
                tracks: vec![video_source.clone(), audio_source.clone()],
                nodes: Vec::new(),
                videos_needing_keyframes: vec![video_key],
            });
        }
        assert!(
            keyframe_receiver
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "level-triggered recovery state must emit one request, not one per media projection"
        );

        let mut playing_video = video_source;
        playing_video.playing = true;
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 9,
            surfaces: vec![test_surface(video_key)],
            tracks: vec![playing_video.clone(), audio_source.clone()],
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
        let outer_video = outer_media_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("outer presenter did not receive the opening video keyframe");
        assert_eq!(
            outer_video.record_type,
            vivid_protocol::messages::VIDEO_PACKET
        );
        assert!(
            ack_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "opening video was acknowledged before outer ingress became reusable"
        );
        assert!(!presenter.complete_bridge_delivery(outer_video.delivery_id, true));
        assert_eq!(
            ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            (43, true),
            "the first post-PLAY keyframe must reach the existing outer video source"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let snapshot = presenter.projection_snapshot(&HashSet::from([7]));
            if snapshot.sources.iter().any(|source| {
                source.key.track != 0
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
        let packet = media::audio_packet_body(AudioPacket {
            epoch: 1,
            packet_id: 2,
            pts_us: 20_000,
            dts_us: 20_000,
            duration_us: 20_000,
            trim_start_samples: 0,
            trim_end_samples: 0,
            data: &[0, 0, 0, 0],
        })
        .unwrap();
        assert!(worker.queue_media(BridgeMedia {
            generation: 0,
            delivery_id: 44,
            source: key,
            record_type: vivid_protocol::messages::AUDIO_PACKET,
            offset: 0,
            total: packet.len() as u32,
            last: true,
            bytes: packet,
        }));
        let outer_audio = outer_media_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("outer presenter did not receive post-PLAY audio");
        assert_eq!(
            outer_audio.record_type,
            vivid_protocol::messages::AUDIO_PACKET
        );
        assert_eq!(
            ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            (44, true),
            "post-PLAY audio retained a stop-and-wait hop instead of using outer flow control"
        );
        assert!(!presenter.complete_bridge_delivery(outer_audio.delivery_id, true));
        worker.replace_snapshot(BridgeSnapshot {
            generation: 0,
            virtual_revision: 10,
            surfaces: vec![test_surface(video_key)],
            tracks: vec![playing_video, audio_source],
            nodes: Vec::new(),
            videos_needing_keyframes: vec![video_key],
        });
        let requests = keyframe_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].source, video_key);
    }

    #[test]
    #[cfg(unix)]
    fn two_three_pane_tabs_restore_video_and_audio_after_showing_an_image() {
        for iteration in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory
                .path()
                .join(format!("outer-tab-recovery-{iteration}.sock"));
            let presenter = match VirtualVivid::start(socket.clone(), MediaConfig::default()) {
                Ok(presenter) => presenter,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    eprintln!("skipping tab recovery bridge socket test: {error}");
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
            let presenter_cell_size =
                Arc::new(AtomicU32::new(pack_cell_size(bridge.display_metrics())));
            let mut worker =
                BridgeWorker::spawn(bridge, client_writer, 8, presenter_cell_size).unwrap();

            let (message_sender, message_receiver) = mpsc::channel();
            thread::spawn(move || {
                while let Ok(message) = server_reader.recv::<ClientMessage>() {
                    let _ = message_sender.send(message);
                }
            });
            let mut seen = Vec::new();

            let tabs = [[1_u64, 2, 3], [4_u64, 5, 6]];
            let tab_area = crate::layout::Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 22,
            };
            let three_pane_tab = |panes: [u64; 3]| {
                let mut tree = crate::layout::TiledNode::leaf(panes[0]);
                tree.split(panes[0], panes[1], crate::ipc::Axis::Vertical, tab_area)
                    .unwrap();
                tree.split(panes[1], panes[2], crate::ipc::Axis::Horizontal, tab_area)
                    .unwrap();
                tree
            };
            let video_tab = three_pane_tab(tabs[0]);
            let image_tab = three_pane_tab(tabs[1]);
            assert_eq!(video_tab.pane_ids(), tabs[0]);
            assert_eq!(image_tab.pane_ids(), tabs[1]);
            let video_pane = tabs[0][1];
            let image_pane = tabs[1][1];
            let video_rect = video_tab.geometry(tab_area)[&video_pane].content();
            let image_rect = image_tab.geometry(tab_area)[&image_pane].content();
            let video_key = BridgeSourceKey {
                producer: video_pane,
                context: 1,
                surface: 7,
                track: 7,
            };
            let audio_key = BridgeSourceKey {
                producer: video_pane,
                context: 1,
                surface: 7,
                track: 8,
            };
            let play_request = crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 33_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
            };
            let video_source = |playing, play_request| BridgeSource {
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
                playing,
                eos_epoch: None,
                causation_id: None,
                play_request,
            };
            let audio_source = BridgeSource {
                key: audio_key,
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
                eos_epoch: None,
                causation_id: None,
                play_request: crate::ipc::BridgePlayRequest {
                    minimum_buffer_us: 0,
                    ..play_request
                },
            };
            let video_node = BridgeNode {
                producer: video_key.producer,
                node: 1,
                fragment: 0,
                surface: test_surface(video_key).key,
                x: i64::from(video_rect.x) << 32,
                y: i64::from(video_rect.y) << 32,
                width: i64::from(video_rect.width) << 32,
                height: i64::from(video_rect.height) << 32,
                z_index: 0,
                visible: true,
                clip: crate::ipc::BridgeClipRect {
                    x: i64::from(video_rect.x) << 32,
                    y: i64::from(video_rect.y) << 32,
                    width: i64::from(video_rect.width) << 32,
                    height: i64::from(video_rect.height) << 32,
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
            let video_packet = |packet_id, pts_us, key| {
                let key_access_unit = [0, 0, 0, 1, 0x65, 0x88];
                let delta_access_unit = [0, 0, 0, 1, 0x41, 0x9a];
                media::video_packet_body(VideoPacket {
                    epoch: 1,
                    packet_id,
                    pts_us,
                    dts_us: pts_us,
                    duration_us: 33_000,
                    key,
                    data: if key {
                        &key_access_unit
                    } else {
                        &delta_access_unit
                    },
                })
                .unwrap()
            };
            let audio_packet = |packet_id, pts_us| {
                media::audio_packet_body(AudioPacket {
                    epoch: 1,
                    packet_id,
                    pts_us,
                    dts_us: pts_us,
                    duration_us: 20_000,
                    trim_start_samples: 0,
                    trim_end_samples: 0,
                    data: &[0, 0, 0, 0],
                })
                .unwrap()
            };

            // Tab 1 becomes visible with all three panes, but only pane 2 contributes media. Deliver
            // linked-audio pre-roll before the authoritative PLAY transition.
            worker.replace_snapshot(BridgeSnapshot {
                generation: 0,
                virtual_revision: 1,
                surfaces: vec![test_surface(video_key)],
                tracks: vec![video_source(false, play_request), audio_source.clone()],
                nodes: vec![video_node.clone()],
                videos_needing_keyframes: Vec::new(),
            });
            let initial_applied = receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "initial tab 1 projection",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeApplied {
                            virtual_revision: 1,
                            ..
                        }
                    )
                },
            );
            let (bridge_instance_id, initial_attachment_generation) = match &seen[initial_applied] {
                ClientMessage::BridgeApplied {
                    bridge_instance_id,
                    outer_attachment_generations,
                    ..
                } => (
                    *bridge_instance_id,
                    outer_attachment_generations
                        .iter()
                        .find_map(|(source, generation)| {
                            (*source == video_key).then_some(*generation)
                        })
                        .unwrap(),
                ),
                _ => unreachable!(),
            };
            project_outer_timed_sources(&presenter, 2);
            let initial_audio = audio_packet(1, 0);
            assert!(worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id: 10,
                source: audio_key,
                record_type: vivid_protocol::messages::AUDIO_PACKET,
                offset: 0,
                total: u32::try_from(initial_audio.len()).unwrap(),
                last: true,
                bytes: initial_audio,
            }));
            receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "linked audio pre-roll acknowledgement",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeMediaAck {
                            delivery_id: 10,
                            delivered: true
                        }
                    )
                },
            );

            let initial_play_start = seen.len();
            worker.replace_snapshot(BridgeSnapshot {
                generation: 0,
                virtual_revision: 2,
                surfaces: vec![test_surface(video_key)],
                tracks: vec![video_source(true, play_request), audio_source.clone()],
                nodes: vec![video_node.clone()],
                videos_needing_keyframes: Vec::new(),
            });
            let initial_play_applied = receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "initial authoritative PLAY",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeApplied {
                            virtual_revision: 2,
                            ..
                        }
                    )
                },
            );
            let initial_play_trace = seen[initial_play_start..=initial_play_applied]
                .iter()
                .position(|message| {
                    matches!(
                        message,
                        ClientMessage::BridgeTrace {
                            event: BridgeMediaTraceEvent {
                                source: Some(source),
                                kind: MediaTraceKind::PlaybackControl {
                                    control: MediaPlaybackControl::Play,
                                    request: Some(request),
                                },
                                ..
                            },
                            ..
                        } if *source == video_key && *request == play_request
                    )
                });
            assert!(
                initial_play_trace.is_some(),
                "PLAY must be observable before its applied projection acknowledgement"
            );
            let opening_keyframe = video_packet(1, 0, true);
            assert!(worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id: 11,
                source: video_key,
                record_type: vivid_protocol::messages::VIDEO_PACKET,
                offset: 0,
                total: u32::try_from(opening_keyframe.len()).unwrap(),
                last: true,
                bytes: opening_keyframe,
            }));
            receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "opening video keyframe acknowledgement",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeMediaAck {
                            delivery_id: 11,
                            delivered: true
                        }
                    )
                },
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
                producer: image_pane,
                context: 1,
                surface: 9,
                track: 9,
            };
            let image_source = BridgeSource {
                key: image_key,
                kind: BridgeSourceKind::Image {
                    encoding: 1,
                    width: 1,
                    height: 1,
                    encoded_length: u32::try_from(encoded.len()).unwrap(),
                    sha256: None,
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
                    late_policy: 1,
                    loop_count: 0,
                    start_policy: 1,
                },
            };
            let image_node = BridgeNode {
                producer: image_key.producer,
                node: 2,
                fragment: 0,
                surface: test_surface(image_key).key,
                x: i64::from(image_rect.x) << 32,
                y: i64::from(image_rect.y) << 32,
                width: i64::from(image_rect.width) << 32,
                height: i64::from(image_rect.height) << 32,
                z_index: 0,
                visible: true,
                clip: crate::ipc::BridgeClipRect {
                    x: i64::from(image_rect.x) << 32,
                    y: i64::from(image_rect.y) << 32,
                    width: i64::from(image_rect.width) << 32,
                    height: i64::from(image_rect.height) << 32,
                },
            };

            // Switching tabs atomically removes tab 1's video/audio projection and replaces it with
            // the image owned by pane 2 of tab 2.
            worker.replace_snapshot(BridgeSnapshot {
                generation: 0,
                virtual_revision: 3,
                surfaces: vec![test_surface(image_key)],
                tracks: vec![image_source.clone()],
                nodes: vec![image_node.clone()],
                videos_needing_keyframes: Vec::new(),
            });
            receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "tab 2 image projection",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeApplied {
                            virtual_revision: 3,
                            ..
                        }
                    )
                },
            );
            assert!(worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id: 12,
                source: image_key,
                record_type: vivid_protocol::messages::IMAGE_DATA,
                offset: 0,
                total: u32::try_from(encoded.len()).unwrap(),
                last: true,
                bytes: encoded.clone(),
            }));
            receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "tab 2 image delivery acknowledgement",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeMediaAck {
                            delivery_id: 12,
                            delivered: true
                        }
                    )
                },
            );
            assert!(
                presenter.wait_for_retained_media(7, Duration::from_secs(2)),
                "tab 2 image never reached the outer presenter's retained state"
            );
            let image_snapshot = presenter.projection_snapshot(&HashSet::from([7]));
            assert_eq!(image_snapshot.sources.len(), 1);
            assert!(image_snapshot.sources[0].retained.is_some());
            assert!(outer_video(&image_snapshot).is_none());

            // Return to tab 1. Decoder recreation and the original authoritative PLAY must precede
            // the applied acknowledgement, and the recreated attachment must request a keyframe.
            let return_start = seen.len();
            worker.replace_snapshot(BridgeSnapshot {
                generation: 0,
                virtual_revision: 4,
                surfaces: vec![test_surface(video_key)],
                tracks: vec![video_source(true, play_request), audio_source.clone()],
                nodes: vec![video_node.clone()],
                videos_needing_keyframes: vec![video_key],
            });
            let return_applied = receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "tab 1 restored projection",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeApplied {
                            virtual_revision: 4,
                            ..
                        }
                    )
                },
            );
            let (return_bridge_instance, return_attachment_generation) = match &seen[return_applied]
            {
                ClientMessage::BridgeApplied {
                    bridge_instance_id,
                    outer_attachment_generations,
                    ..
                } => (
                    *bridge_instance_id,
                    outer_attachment_generations
                        .iter()
                        .find_map(|(source, generation)| {
                            (*source == video_key).then_some(*generation)
                        })
                        .unwrap(),
                ),
                _ => unreachable!(),
            };
            assert_eq!(
                return_bridge_instance, bridge_instance_id,
                "a tab switch reconciles sources without replacing the outer bridge session"
            );
            assert_eq!(
                initial_attachment_generation, 1,
                "the first independently allocated outer channel starts at generation one"
            );
            assert_eq!(
                return_attachment_generation, 1,
                "a replacement outer track owns a fresh identity and an independent generation"
            );
            let return_messages = &seen[return_start..=return_applied];
            project_outer_timed_sources(&presenter, 2);
            let recreated = return_messages
                .iter()
                .position(|message| {
                    matches!(
                        message,
                        ClientMessage::BridgeTrace {
                            event: BridgeMediaTraceEvent {
                                source: Some(source),
                                kind: MediaTraceKind::OuterTrackRecreated {
                                    playing: false,
                                    ..
                                },
                                ..
                            },
                            ..
                        } if *source == video_key
                    )
                })
                .expect("video decoder recreation was not traced");
            assert!(
                !return_messages.iter().any(|message| {
                    matches!(
                        message,
                        ClientMessage::BridgeTrace {
                            event: BridgeMediaTraceEvent {
                                source: Some(source),
                                kind: MediaTraceKind::PlaybackControl {
                                    control: MediaPlaybackControl::Play,
                                    ..
                                },
                                ..
                            },
                            ..
                        } if *source == video_key
                    )
                }),
                "replacement tracks must remain paused until Vivi publishes recovery PLAY"
            );
            let expected_recovery = BridgeKeyframeRequest {
                source: video_key,
                minimum_epoch: None,
                reason: crate::bridge::KEYFRAME_REASON_TRANSPORT_LOSS,
            };
            let recovery_requested = return_messages
                .iter()
                .position(|message| {
                    matches!(
                        message,
                        ClientMessage::BridgeNeedKeyframes(requests)
                            if requests == std::slice::from_ref(&expected_recovery)
                    )
                })
                .expect("restored video recovery was not requested");
            assert!(
                recovery_requested < recreated,
                "producer recovery must be requested before rebuilding and acknowledging the replacement decoder"
            );
            // Snapshot state propagates PLAY across every active surface member, so it cannot
            // reveal whether the bridge actually named video or linked audio as the clock. Clear
            // the initial playback command and inspect the exact recovery command below.
            let _ = presenter.take_play_commands();

            // Vivi's fixed recovery behavior re-bases PLAY to the same-epoch recovery keyframe before
            // submitting that packet. The worker must apply that playback-only update before media.
            let recovery_pts_us = 8_033_333;
            let recovery_play = crate::ipc::BridgePlayRequest {
                start_pts_us: recovery_pts_us,
                ..play_request
            };
            let recovery_play_start = seen.len();
            worker.replace_snapshot(BridgeSnapshot {
                generation: 0,
                virtual_revision: 5,
                surfaces: vec![test_surface(video_key)],
                tracks: vec![video_source(true, recovery_play), audio_source.clone()],
                nodes: vec![video_node],
                videos_needing_keyframes: Vec::new(),
            });
            let recovery_play_applied = receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "recovery PLAY rebase",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeApplied {
                            virtual_revision: 5,
                            ..
                        }
                    )
                },
            );
            assert!(
                seen[recovery_play_start..=recovery_play_applied]
                    .iter()
                    .any(|message| {
                        matches!(
                            message,
                            ClientMessage::BridgeTrace {
                                event: BridgeMediaTraceEvent {
                                    source: Some(source),
                                    kind: MediaTraceKind::PlaybackControl {
                                        control: MediaPlaybackControl::Play,
                                        request: Some(request),
                                    },
                                    ..
                                },
                                ..
                            } if *source == video_key && *request == recovery_play
                        )
                    }),
                "same-epoch recovery must publish its exact PLAY rebase before media"
            );

            let recovery_keyframe = video_packet(2, recovery_pts_us, true);
            assert!(worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id: 13,
                source: video_key,
                record_type: vivid_protocol::messages::VIDEO_PACKET,
                offset: 0,
                total: u32::try_from(recovery_keyframe.len()).unwrap(),
                last: true,
                bytes: recovery_keyframe,
            }));
            receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "same-epoch recovery keyframe acknowledgement",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeMediaAck {
                            delivery_id: 13,
                            delivered: true
                        }
                    )
                },
            );

            let resumed_audio = audio_packet(2, recovery_pts_us + 7_000);
            assert!(worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id: 14,
                source: audio_key,
                record_type: vivid_protocol::messages::AUDIO_PACKET,
                offset: 0,
                total: u32::try_from(resumed_audio.len()).unwrap(),
                last: true,
                bytes: resumed_audio,
            }));
            receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "resumed linked audio acknowledgement",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeMediaAck {
                            delivery_id: 14,
                            delivered: true
                        }
                    )
                },
            );

            let resumed_delta = video_packet(3, recovery_pts_us + 33_000, false);
            assert!(worker.queue_media(BridgeMedia {
                generation: 0,
                delivery_id: 15,
                source: video_key,
                record_type: vivid_protocol::messages::VIDEO_PACKET,
                offset: 0,
                total: u32::try_from(resumed_delta.len()).unwrap(),
                last: true,
                bytes: resumed_delta,
            }));
            receive_bridge_message_until(
                &message_receiver,
                &mut seen,
                "post-recovery video delta acknowledgement",
                |message| {
                    matches!(
                        message,
                        ClientMessage::BridgeMediaAck {
                            delivery_id: 15,
                            delivered: true
                        }
                    )
                },
            );

            let restored = presenter.projection_snapshot(&HashSet::from([7]));
            assert_eq!(restored.nodes.len(), 1);
            assert!(outer_video(&restored).is_some_and(|(_, playing)| playing));
            let recovery_clocks = presenter.take_play_commands();
            assert_eq!(
                recovery_clocks.len(),
                1,
                "recovery must defer PLAY until the replacement slots are active"
            );
            assert!(
                restored.sources.iter().any(|source| {
                    source.key == recovery_clocks[0]
                        && matches!(source.descriptor, crate::media::SourceDescriptor::Audio(_))
                }),
                "recovery PLAY must name linked audio so the physical output is configured and restarted"
            );
            for source in restored
                .sources
                .iter()
                .filter(|source| source.key.surface == video_key.surface && source.playing)
            {
                assert_eq!(
                    source.play_request.start_pts_us, recovery_pts_us,
                    "the outer audio/video clock must be re-based to the recovery keyframe"
                );
            }
            assert!(
                restored.sources.iter().all(|source| {
                    !matches!(source.descriptor, crate::media::SourceDescriptor::Image(_))
                }),
                "the hidden image tab must not remain in the outer projection"
            );
        }
    }

    #[test]
    fn playback_only_snapshot_preserves_projection_and_linked_audio_state() {
        let video_key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 7,
        };
        let audio_key = BridgeSourceKey {
            producer: 3,
            context: 1,
            surface: 7,
            track: 8,
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
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 100_000,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
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
            eos_epoch: None,
            causation_id: None,
            play_request: crate::ipc::BridgePlayRequest {
                start_pts_us: 0,
                minimum_buffer_us: 0,
                maximum_latency_us: 500_000,
                rate_32_32: 1_i64 << 32,
                late_policy: 1,
                loop_count: 0,
                start_policy: 1,
            },
        };
        let before = vec![video(false), audio.clone()];
        let after = vec![audio, video(true)];
        let surfaces = vec![test_surface(video_key)];

        assert_eq!(
            compare_projection(&surfaces, &before, &[], &surfaces, &after, &[]),
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
            surface: test_surface(video_key).key,
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
            compare_projection(
                &surfaces,
                &before,
                &[node(0)],
                &surfaces,
                &after,
                &[node(0)],
            ),
            ProjectionChange::PlaybackOnly
        );
        assert_eq!(
            compare_projection(
                &surfaces,
                &before,
                &[node(0)],
                &surfaces,
                &after,
                &[node(5)],
            ),
            ProjectionChange::SceneOnly,
            "a moved node must reconcile the scene without touching sources"
        );
        assert_eq!(
            compare_projection(&surfaces, &before, &[node(0)], &surfaces, &after, &[]),
            ProjectionChange::SceneOnly,
            "a removed node must reconcile the scene without touching sources"
        );
        assert_eq!(
            compare_projection(&[], &[], &[], &surfaces, &after, &[node(0)]),
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
            context: 1,
            surface: 12,
            track: 12,
        };
        let mut with_image = after.clone();
        with_image.push(BridgeSource {
            key: image_key,
            kind: BridgeSourceKind::Image {
                encoding: 1,
                width: 4,
                height: 4,
                encoded_length: 32,
                sha256: None,
            },
            capture_policy: 0,
            descriptor: None,
            playing: false,
            eos_epoch: None,
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
