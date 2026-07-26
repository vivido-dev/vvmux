pub(crate) mod auth;
mod protocol;
mod session_adapter;
mod vivid;

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::header::{ORIGIN, SEC_WEBSOCKET_PROTOCOL};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

use crate::client::{BridgeMedia, BridgeSnapshot, BridgeWorker};
use crate::client_input::{FloatEditScanner, ParsedInput, PrefixParser};
use crate::config::Config;
use crate::ipc::{ClientMessage, FloatingEditCommand, FloatingEditKind, ServerMessage};
use protocol::{
    ClientControl, MAX_FRAME_BYTES, MAX_INPUT_BYTES, SUBPROTOCOL, ServerControl, SessionInfo,
    VERSION, VividAccess, decode_control, encode_control,
};
use session_adapter::{QueuedServerMessage, SessionAdapter};

#[cfg(not(windows))]
const TERMINAL_ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h";
#[cfg(windows)]
const TERMINAL_ENTER: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004l";
const TERMINAL_LEAVE: &[u8] =
    b"\x1b[0m\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l";

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an attached client may stay silent, once another client is waiting for its session.
const CLIENT_STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to keep flushing an outbound queue after the connection loop has finished with it.
const CLIENT_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// How often an attached client is pinged while it owes us nothing else.
///
/// A browser answers at the protocol level, so silence for a full [`CLIENT_STALL_TIMEOUT`] means
/// the peer has stopped reading its socket rather than merely having nothing to say.
const CLIENT_PING_INTERVAL: Duration = Duration::from_secs(1);
/// How long a refused attachment keeps a session marked as wanted by someone else.
const CONTENTION_WINDOW: Duration = Duration::from_secs(30);
/// Cap on remembered contended sessions, so refused attachments cannot grow the map without bound.
const MAX_CONTENDED_SESSIONS: usize = 64;
const STALLED_CLIENT: &str = "terminal client is too slow";
const CAPABILITIES: &[&str] = &[
    "terminal-v1",
    "session-list-v1",
    "session-create-v1",
    "vivid-bridge-v1",
];

#[derive(Debug, Default)]
pub(crate) struct ServeOverrides {
    pub listen: Option<SocketAddr>,
    pub allowed_origins: Vec<String>,
    pub auth_file: Option<PathBuf>,
}

#[derive(Clone)]
struct GatewayState {
    config: Arc<Config>,
    config_path: Option<PathBuf>,
    auth_file: PathBuf,
    allowed_origins: Arc<HashSet<String>>,
    connections: Arc<Semaphore>,
    vivid_sessions: Arc<Mutex<HashMap<String, Arc<vivid::VividBroker>>>>,
    contended_sessions: Arc<Mutex<HashMap<String, Instant>>>,
}

struct VividRegistration {
    sessions: Arc<Mutex<HashMap<String, Arc<vivid::VividBroker>>>>,
    broker: Arc<vivid::VividBroker>,
}

impl Drop for VividRegistration {
    fn drop(&mut self) {
        self.broker.close();
        if let Ok(mut sessions) = self.sessions.lock()
            && sessions
                .get(self.broker.id())
                .is_some_and(|current| Arc::ptr_eq(current, &self.broker))
        {
            sessions.remove(self.broker.id());
        }
    }
}

pub(crate) fn run(
    config: Config,
    config_path: Option<PathBuf>,
    overrides: ServeOverrides,
) -> io::Result<()> {
    let listen = overrides.listen.unwrap_or_else(|| {
        config
            .server
            .listen
            .parse()
            .expect("validated server address")
    });
    if !listen.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "vvmux serve accepts loopback listen addresses only",
        ));
    }
    let origins = if overrides.allowed_origins.is_empty() {
        config.server.allowed_origins.clone()
    } else {
        overrides.allowed_origins
    };
    let allowed_origins = validate_origins(origins)?;
    let auth_file = overrides
        .auth_file
        .or_else(|| config.server.auth_file.clone())
        .map(Ok)
        .unwrap_or_else(auth::default_auth_path)?;
    auth::validate_record(&auth_file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not load authentication record {}: {error}; run `vvmux token create` first",
                auth_file.display()
            ),
        )
    })?;

    let state = GatewayState {
        connections: Arc::new(Semaphore::new(config.server.max_connections)),
        config: Arc::new(config),
        config_path,
        auth_file,
        allowed_origins: Arc::new(allowed_origins),
        vivid_sessions: Arc::new(Mutex::new(HashMap::new())),
        contended_sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("vvmux-gateway")
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen).await?;
        println!("vvmux gateway listening on {}", listener.local_addr()?);
        let app = Router::new()
            .route("/v1/ws", get(websocket_upgrade))
            .route("/v1/vivid", get(vivid_upgrade))
            .with_state(state);
        axum::serve(listener, app).await.map_err(io::Error::other)
    })
}

async fn websocket_upgrade(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    if !offered_subprotocol(&headers) {
        return (StatusCode::BAD_REQUEST, "VVWS/1 subprotocol is required").into_response();
    }
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return (StatusCode::FORBIDDEN, "WebSocket Origin is required").into_response();
    };
    if !state.allowed_origins.contains(origin) {
        return (StatusCode::FORBIDDEN, "WebSocket Origin is not allowed").into_response();
    }
    let Ok(permit) = state.connections.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway connection limit reached",
        )
            .into_response();
    };
    websocket
        .max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .protocols([SUBPROTOCOL])
        .on_upgrade(move |socket| handle_connection(socket, state, permit))
}

async fn vivid_upgrade(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return (StatusCode::FORBIDDEN, "WebSocket Origin is required").into_response();
    };
    if !state.allowed_origins.contains(origin) {
        return (StatusCode::FORBIDDEN, "WebSocket Origin is not allowed").into_response();
    }
    if !offered_protocol(&headers, vivid::SUBPROTOCOL) {
        return (
            StatusCode::BAD_REQUEST,
            "Vivid WebSocket subprotocol is required",
        )
            .into_response();
    }
    let Some(connection) = protocol_parameter(&headers, vivid::CONNECTION_PROTOCOL_PREFIX) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Vivid WebSocket authentication failed",
        )
            .into_response();
    };
    let Some(token) = protocol_parameter(&headers, vivid::AUTH_PROTOCOL_PREFIX) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Vivid WebSocket authentication failed",
        )
            .into_response();
    };
    let Some(kind) = protocol_parameter(&headers, vivid::KIND_PROTOCOL_PREFIX)
        .and_then(|value| value.parse::<u8>().ok())
        .and_then(|value| vivid_protocol::wire::ConnectionKind::try_from(value).ok())
    else {
        return (StatusCode::BAD_REQUEST, "invalid Vivid connection kind").into_response();
    };
    let broker = state
        .vivid_sessions
        .lock()
        .ok()
        .and_then(|sessions| sessions.get(connection).cloned());
    let Some(broker) = broker.filter(|broker| broker.authenticate(token)) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Vivid WebSocket authentication failed",
        )
            .into_response();
    };
    let Ok(permit) = state.connections.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway connection limit reached",
        )
            .into_response();
    };
    let maximum = vivid_protocol::HARD_MAX_RECORD_BODY as usize
        + vivid_protocol::wire::HEADER_SIZE
        + vivid_protocol::wire::PREFACE_SIZE;
    websocket
        .max_frame_size(maximum)
        .max_message_size(maximum)
        .protocols([vivid::SUBPROTOCOL])
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let _ = vivid::serve_socket(socket, broker, kind).await;
        })
}

async fn handle_connection(
    mut socket: WebSocket,
    state: GatewayState,
    _permit: OwnedSemaphorePermit,
) {
    if authenticate_connection(&mut socket, &state).await.is_err() {
        let _ = close_socket(&mut socket, 1008, "authentication failed").await;
        return;
    }
    let broker = match vivid::VividBroker::new() {
        Ok(broker) => broker,
        Err(_) => {
            let _ = close_socket(&mut socket, 1011, "could not initialize Vivid routing").await;
            return;
        }
    };
    let registered = state.vivid_sessions.lock().is_ok_and(|mut sessions| {
        if sessions.len() >= state.config.server.max_connections
            || sessions.contains_key(broker.id())
        {
            false
        } else {
            sessions.insert(broker.id().to_owned(), broker.clone());
            true
        }
    });
    if !registered {
        let _ = close_socket(&mut socket, 1013, "Vivid route capacity reached").await;
        return;
    }
    let _vivid_registration = VividRegistration {
        sessions: state.vivid_sessions.clone(),
        broker: broker.clone(),
    };
    let vivid_token = broker.encoded_token();
    if send_control_socket(
        &mut socket,
        &ServerControl::Hello {
            protocol: VERSION,
            server_version: env!("CARGO_PKG_VERSION"),
            capabilities: CAPABILITIES,
            vivid: VividAccess {
                endpoint: "/v1/vivid",
                subprotocol: vivid::SUBPROTOCOL,
                connection: broker.id(),
                token: &vivid_token,
            },
        },
    )
    .await
    .is_err()
    {
        return;
    }

    // The queue is sized from the same budget as the session-side queue, so a client that stops
    // reading is bounded by one configured amount rather than two independent ones.
    let outbound_messages = (state.config.server.outbound_queue_bytes / 1024).clamp(8, 4096);
    let (outbound, mut outbound_receiver) =
        tokio::sync::mpsc::channel::<Message>(outbound_messages);
    let writer = ClientWriter { outbound };
    let (mut sink, mut stream) = socket.split();
    let mut writer_task = tokio::spawn(async move {
        while let Some(message) = outbound_receiver.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    let prefix = crate::config::parse_control_chord(&state.config.general.prefix).unwrap_or(0x02);
    let mut input = PrefixParser::new(prefix, &state.config.keys.prefix);
    let mut float_scanner = FloatEditScanner::default();
    let mut float_mode = None;
    let mut session: Option<SessionAdapter> = None;
    let mut bridge: Option<BridgeWorker> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_seen = Instant::now();
    let mut last_ping = Instant::now();

    loop {
        let incoming = if let Some(adapter) = session.as_mut() {
            tokio::select! {
                message = stream.next() => Incoming::Socket(message),
                event = adapter.recv() => Incoming::Session(event),
                _ = tick.tick() => Incoming::Tick,
            }
        } else {
            let message = stream.next().await;
            Incoming::Socket(message)
        };

        let result = match incoming {
            Incoming::Socket(Some(Ok(message))) => {
                last_seen = Instant::now();
                handle_socket_message(
                    &writer,
                    message,
                    &state,
                    &broker,
                    &mut session,
                    &mut bridge,
                    &mut input,
                    &mut float_scanner,
                    &mut float_mode,
                )
                .await
            }
            Incoming::Socket(Some(Err(error))) => Err(io::Error::other(error)),
            Incoming::Socket(None) => break,
            Incoming::Session(Some(event)) => {
                handle_session_message(
                    &writer,
                    event,
                    &mut session,
                    &mut bridge,
                    &mut input,
                    &mut float_scanner,
                    &mut float_mode,
                )
                .await
            }
            Incoming::Session(None) => {
                let overloaded = session.as_ref().is_some_and(SessionAdapter::overloaded);
                let _ = writer.send(Message::Binary(TERMINAL_LEAVE.into()));
                if overloaded {
                    let _ = close(&writer, 1013, STALLED_CLIENT);
                    break;
                }
                session = None;
                release_bridge(&mut bridge);
                send_control(
                    &writer,
                    &ServerControl::Detached {
                        reason: "session connection closed".into(),
                    },
                )
            }
            Incoming::Tick => {
                match expire_float_mode(&session, &mut float_scanner, &mut float_mode) {
                    Ok(()) => check_client_liveness(
                        &writer,
                        &state,
                        session.as_ref().map(SessionAdapter::name),
                        &mut last_seen,
                        &mut last_ping,
                    ),
                    error => error,
                }
            }
        };
        if let Err(error) = result {
            if is_stalled_client(&error) {
                // Release the session before the close handshake. The socket we would negotiate
                // the close on is the one that stalled, and a client waiting to take this session
                // over must not be held behind another write to it.
                if let Some(adapter) = session.take() {
                    let _ = adapter.send(ClientMessage::Detach);
                    adapter.cancel();
                }
                let _ = close(&writer, 1013, STALLED_CLIENT);
            }
            break;
        }
    }
    if session.is_some() {
        let _ = writer.send(Message::Binary(TERMINAL_LEAVE.into()));
    }
    if let Some(adapter) = session.take() {
        let _ = adapter.send(ClientMessage::Detach);
        adapter.cancel();
    }
    release_bridge(&mut bridge);
    // Let the queued frames, including any close, reach a peer that is still reading, but never
    // wait on one that is not.
    drop(writer);
    if tokio::time::timeout(CLIENT_FLUSH_TIMEOUT, &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
    }
}

enum Incoming {
    Socket(Option<Result<Message, axum::Error>>),
    Session(Option<QueuedServerMessage>),
    Tick,
}

async fn authenticate_connection(socket: &mut WebSocket, state: &GatewayState) -> io::Result<()> {
    let first = tokio::time::timeout(AUTH_TIMEOUT, socket.next())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "authentication timed out"))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"))?
        .map_err(io::Error::other)?;
    let Message::Text(text) = first else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "first VVWS/1 message must be hello",
        ));
    };
    let ClientControl::Hello { protocol, token } = decode_control(&text)? else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "first VVWS/1 message must be hello",
        ));
    };
    if protocol != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported VVWS protocol version",
        ));
    }
    let token = Zeroizing::new(token);
    if !auth::authenticate(&state.auth_file, &token)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authentication failed",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_socket_message(
    writer: &ClientWriter,
    message: Message,
    state: &GatewayState,
    broker: &Arc<vivid::VividBroker>,
    session: &mut Option<SessionAdapter>,
    bridge: &mut Option<BridgeWorker>,
    input: &mut PrefixParser,
    float_scanner: &mut FloatEditScanner,
    float_mode: &mut Option<u64>,
) -> io::Result<()> {
    match message {
        Message::Text(text) => {
            let control = match decode_control(&text) {
                Ok(control) => control,
                Err(error) => {
                    send_error(writer, None, "invalid_request", error.to_string())?;
                    return Ok(());
                }
            };
            match control {
                ClientControl::Hello { .. } => {
                    send_error(
                        writer,
                        None,
                        "invalid_state",
                        "hello is valid only as the first message".into(),
                    )?;
                }
                ClientControl::ListSessions { request_id } => {
                    if session.is_some() {
                        send_error(
                            writer,
                            Some(request_id),
                            "invalid_state",
                            "detach before listing sessions".into(),
                        )?;
                    } else {
                        match tokio::task::spawn_blocking(list_live_sessions).await {
                            Ok(Ok(sessions)) => {
                                send_control(
                                    writer,
                                    &ServerControl::Sessions {
                                        request_id,
                                        sessions,
                                    },
                                )?;
                            }
                            Ok(Err(error)) => {
                                send_error(
                                    writer,
                                    Some(request_id),
                                    "session_unavailable",
                                    error.to_string(),
                                )?;
                            }
                            Err(error) => return Err(io::Error::other(error)),
                        }
                    }
                }
                ClientControl::CreateSession { request_id, name } => {
                    if session.is_some() {
                        send_error(
                            writer,
                            Some(request_id),
                            "invalid_state",
                            "detach before creating a session".into(),
                        )?;
                    } else if let Err(error) = crate::runtime::validate_session_name(&name) {
                        send_error(
                            writer,
                            Some(request_id),
                            "invalid_session",
                            error.to_string(),
                        )?;
                    } else {
                        let created_name = name.clone();
                        let config_path = state.config_path.clone();
                        match tokio::task::spawn_blocking(move || {
                            crate::client::create_detached(&created_name, config_path.as_deref())
                        })
                        .await
                        {
                            Ok(Ok(())) => {
                                send_control(writer, &ServerControl::Created { request_id, name })?;
                            }
                            Ok(Err(error)) => {
                                let code = if error.kind() == io::ErrorKind::AlreadyExists {
                                    "session_exists"
                                } else {
                                    "session_unavailable"
                                };
                                send_error(writer, Some(request_id), code, error.to_string())?;
                            }
                            Err(error) => return Err(io::Error::other(error)),
                        }
                    }
                }
                ClientControl::Attach {
                    request_id,
                    name,
                    display,
                    takeover,
                    vivid,
                } => {
                    if session.is_some() {
                        send_error(
                            writer,
                            Some(request_id),
                            "invalid_state",
                            "a session is already attached".into(),
                        )?;
                    } else if let Err(error) = crate::runtime::validate_session_name(&name) {
                        send_error(
                            writer,
                            Some(request_id),
                            "invalid_session",
                            error.to_string(),
                        )?;
                    } else {
                        let display = match display.validate() {
                            Ok(display) => display,
                            Err(error) => {
                                send_error(
                                    writer,
                                    Some(request_id),
                                    "invalid_display",
                                    error.to_string(),
                                )?;
                                return Ok(());
                            }
                        };
                        let requested = name.clone();
                        match SessionAdapter::connect(
                            name,
                            takeover,
                            display,
                            vivid,
                            state.config.server.outbound_queue_bytes,
                        )
                        .await
                        {
                            Ok((adapter, attached_name, text_only)) => {
                                let browser_bridge = if vivid {
                                    let connection_factory: Arc<
                                        dyn crate::bridge::ConnectionFactory,
                                    > = broker.clone();
                                    match tokio::task::spawn_blocking(move || {
                                        crate::bridge::OuterBridge::connect_with_factory(
                                            connection_factory,
                                            Zeroizing::new("0".repeat(64)),
                                            display,
                                        )
                                    })
                                    .await
                                    {
                                        Ok(Ok(browser_bridge)) => Some(browser_bridge),
                                        Ok(Err(error)) => {
                                            adapter.cancel();
                                            send_error(
                                                writer,
                                                Some(request_id),
                                                "vivid_unavailable",
                                                error.to_string(),
                                            )?;
                                            return Ok(());
                                        }
                                        Err(error) => {
                                            adapter.cancel();
                                            return Err(io::Error::other(error));
                                        }
                                    }
                                } else {
                                    None
                                };
                                if let Some(browser_bridge) = browser_bridge {
                                    let queue_records = (state.config.media.ipc_queue_bytes
                                        / (128 * 1024))
                                        .clamp(1, 1024);
                                    match BridgeWorker::spawn_with_sender(
                                        browser_bridge,
                                        adapter.bridge_sender(),
                                        queue_records,
                                    ) {
                                        Ok(worker) => *bridge = Some(worker),
                                        Err(error) => {
                                            adapter.cancel();
                                            send_error(
                                                writer,
                                                Some(request_id),
                                                "vivid_unavailable",
                                                error.to_string(),
                                            )?;
                                            return Ok(());
                                        }
                                    }
                                }
                                // This client now holds the session, so any earlier refusal has
                                // been resolved and must not count against it.
                                clear_contention(&state.contended_sessions, adapter.name());
                                send_control(
                                    writer,
                                    &ServerControl::Attached {
                                        request_id,
                                        name: attached_name,
                                        text_only,
                                    },
                                )?;
                                writer.send(Message::Binary(TERMINAL_ENTER.into()))?;
                                *input = PrefixParser::new(
                                    crate::config::parse_control_chord(
                                        &state.config.general.prefix,
                                    )
                                    .unwrap_or(0x02),
                                    &state.config.keys.prefix,
                                );
                                *float_scanner = FloatEditScanner::default();
                                *float_mode = None;
                                *session = Some(adapter);
                            }
                            Err(error) => {
                                let code = if error.to_string().contains("attached client") {
                                    // Let the holder know it is wanted: if it has also stopped
                                    // answering, it gives the session up rather than keeping it
                                    // out of reach.
                                    record_contention(&state.contended_sessions, &requested);
                                    "session_occupied"
                                } else if matches!(
                                    error.kind(),
                                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                                ) {
                                    "session_not_found"
                                } else {
                                    "session_unavailable"
                                };
                                send_error(writer, Some(request_id), code, error.to_string())?;
                            }
                        }
                    }
                }
                ClientControl::Resize { display } => match session.as_ref() {
                    Some(adapter) => match display.validate() {
                        Ok(display) => adapter.send(ClientMessage::Resize(display))?,
                        Err(error) => {
                            send_error(writer, None, "invalid_display", error.to_string())?;
                        }
                    },
                    None => {
                        send_error(
                            writer,
                            None,
                            "invalid_state",
                            "no session is attached".into(),
                        )?;
                    }
                },
                ClientControl::Action { action } => match session.as_ref() {
                    Some(adapter) => match action.into_ipc() {
                        Ok(action) => adapter.send(ClientMessage::Action(action))?,
                        Err(error) => {
                            send_error(writer, None, "invalid_action", error.to_string())?;
                        }
                    },
                    None => {
                        send_error(
                            writer,
                            None,
                            "invalid_state",
                            "no session is attached".into(),
                        )?;
                    }
                },
                ClientControl::Detach => match session.as_ref() {
                    Some(adapter) => adapter.send(ClientMessage::Detach)?,
                    None => {
                        send_error(
                            writer,
                            None,
                            "invalid_state",
                            "no session is attached".into(),
                        )?;
                    }
                },
            }
        }
        Message::Binary(bytes) => {
            if bytes.len() > MAX_INPUT_BYTES {
                send_error(
                    writer,
                    None,
                    "input_too_large",
                    "terminal input exceeds 64 KiB".into(),
                )?;
            } else if let Some(adapter) = session.as_ref() {
                dispatch_input(adapter, input, float_scanner, float_mode, &bytes)?;
            } else {
                send_error(
                    writer,
                    None,
                    "invalid_state",
                    "no session is attached".into(),
                )?;
            }
        }
        Message::Ping(bytes) => {
            writer.send(Message::Pong(bytes))?;
        }
        Message::Pong(_) => {}
        Message::Close(_) => return Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
    }
    Ok(())
}

async fn handle_session_message(
    writer: &ClientWriter,
    mut event: QueuedServerMessage,
    session: &mut Option<SessionAdapter>,
    bridge: &mut Option<BridgeWorker>,
    input: &mut PrefixParser,
    float_scanner: &mut FloatEditScanner,
    float_mode: &mut Option<u64>,
) -> io::Result<()> {
    match event.take() {
        ServerMessage::Render {
            frame_id,
            last,
            bytes,
            ..
        } => {
            if bytes.is_empty() {
                writer.send(Message::Binary(bytes.into()))?;
            } else {
                for chunk in bytes.chunks(MAX_FRAME_BYTES) {
                    writer.send(Message::Binary(chunk.to_vec().into()))?;
                }
            }
            if last && let Some(adapter) = session.as_ref() {
                adapter.send(ClientMessage::RenderAck(frame_id))?;
            }
        }
        ServerMessage::Title(title) => {
            send_control(writer, &ServerControl::Title { title })?;
        }
        ServerMessage::Bell => send_control(writer, &ServerControl::Bell)?,
        ServerMessage::Clipboard(text) => {
            send_control(writer, &ServerControl::Clipboard { text })?;
        }
        ServerMessage::Status(message) => {
            send_control(writer, &ServerControl::Status { message })?;
        }
        ServerMessage::FloatingEditMode {
            mode_id,
            pane,
            kind,
        } => {
            let active = pane.is_some();
            if active {
                *float_mode = Some(mode_id);
                let _ = float_scanner.reset();
            } else if *float_mode == Some(mode_id) {
                *float_mode = None;
                let residue = float_scanner.reset();
                if !residue.is_empty()
                    && let Some(adapter) = session.as_ref()
                {
                    dispatch_parsed(adapter, input.feed(&residue))?;
                }
            }
            let kind = match kind {
                Some(FloatingEditKind::Move) => Some("move"),
                Some(FloatingEditKind::Resize) => Some("resize"),
                None => None,
            };
            send_control(
                writer,
                &ServerControl::FloatingEditState {
                    mode_id,
                    active,
                    pane,
                    kind,
                },
            )?;
        }
        ServerMessage::Detached { reason } => {
            writer.send(Message::Binary(TERMINAL_LEAVE.into()))?;
            send_control(writer, &ServerControl::Detached { reason })?;
            *session = None;
            release_bridge(bridge);
            *float_mode = None;
            *float_scanner = FloatEditScanner::default();
        }
        ServerMessage::Error(message) => {
            writer.send(Message::Binary(TERMINAL_LEAVE.into()))?;
            send_error(writer, None, "session_error", message)?;
            *session = None;
            release_bridge(bridge);
        }
        ServerMessage::Pong => {}
        ServerMessage::MediaSnapshot {
            revision,
            sources,
            nodes,
            videos_needing_keyframes,
        } => {
            let Some(bridge) = bridge.as_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "text-only session sent an unexpected media snapshot",
                ));
            };
            bridge.replace_snapshot(BridgeSnapshot {
                generation: 0,
                virtual_revision: revision,
                sources,
                nodes,
                videos_needing_keyframes,
            });
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
            let Some(bridge) = bridge.as_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "text-only session sent an unexpected media record",
                ));
            };
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
        ServerMessage::Attached { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session sent a duplicate attachment reply",
            ));
        }
        ServerMessage::Automation(_) | ServerMessage::AutomationChunk { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session sent automation data to an attached gateway",
            ));
        }
    }
    Ok(())
}

fn dispatch_input(
    adapter: &SessionAdapter,
    input: &mut PrefixParser,
    float_scanner: &mut FloatEditScanner,
    float_mode: &mut Option<u64>,
    bytes: &[u8],
) -> io::Result<()> {
    let parsed = if let Some(mode_id) = *float_mode {
        let (commands, forward) = float_scanner.scan(bytes);
        for command in commands {
            adapter.send(ClientMessage::FloatingEdit { mode_id, command })?;
            if matches!(
                command,
                FloatingEditCommand::Commit | FloatingEditCommand::Cancel
            ) {
                *float_mode = None;
            }
        }
        input.feed(&forward)
    } else {
        input.feed(bytes)
    };
    dispatch_parsed(adapter, parsed)
}

fn dispatch_parsed(adapter: &SessionAdapter, parsed: Vec<ParsedInput>) -> io::Result<()> {
    for command in parsed {
        let message = match command {
            ParsedInput::Input(bytes) => ClientMessage::Input(bytes),
            ParsedInput::Action(action) => ClientMessage::Action(action),
            ParsedInput::Mouse(mouse) => ClientMessage::Mouse(mouse),
            ParsedInput::Detach => ClientMessage::Detach,
        };
        adapter.send(message)?;
    }
    Ok(())
}

fn expire_float_mode(
    session: &Option<SessionAdapter>,
    scanner: &mut FloatEditScanner,
    mode_id: &mut Option<u64>,
) -> io::Result<()> {
    if let (Some(adapter), Some(current_mode), Some(command)) = (
        session.as_ref(),
        *mode_id,
        scanner.expire(std::time::Instant::now()),
    ) {
        adapter.send(ClientMessage::FloatingEdit {
            mode_id: current_mode,
            command,
        })?;
        *mode_id = None;
    }
    Ok(())
}

fn list_live_sessions() -> io::Result<Vec<SessionInfo>> {
    Ok(crate::runtime::list_registries()?
        .into_iter()
        .filter(|registry| crate::server::probe(&registry.name).is_ok())
        .map(|registry| SessionInfo {
            name: registry.name,
            pid: registry.pid,
        })
        .collect())
}

/// The write half of a client connection, owned by a task of its own.
///
/// Writes must not be able to park the connection loop. A client that stops reading its writer
/// backs the write up, and a loop parked in `send` also stops reading, so the client's own input
/// is what gets held: the interrupt that would end the flood cannot reach the pane producing it.
/// Queueing here keeps the loop free to read no matter how far behind the peer falls, and a queue
/// that fills anyway is the delivery failure that ends the connection.
struct ClientWriter {
    outbound: tokio::sync::mpsc::Sender<Message>,
}

impl ClientWriter {
    fn send(&self, message: Message) -> io::Result<()> {
        self.outbound
            .try_send(message)
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::TimedOut, STALLED_CLIENT)
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "client connection closed")
                }
            })
    }
}

async fn send_socket(socket: &mut WebSocket, message: Message) -> io::Result<()> {
    socket.send(message).await.map_err(io::Error::other)
}

/// Send a control frame before the write half has been handed to its own task.
async fn send_control_socket(
    socket: &mut WebSocket,
    message: &ServerControl<'_>,
) -> io::Result<()> {
    let encoded = encode_control(message)?;
    send_socket(socket, Message::Text(encoded.into())).await
}

async fn close_socket(socket: &mut WebSocket, code: u16, reason: &'static str) -> io::Result<()> {
    send_socket(
        socket,
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })),
    )
    .await
}

/// Give up a bridge worker without blocking the runtime.
///
/// `BridgeWorker`'s teardown joins its worker thread so the outer Vivid session closes cleanly,
/// and that join can take seconds. On a runtime thread it stalls this connection's timers and
/// queued writes, which is exactly when the client is waiting to be told it was detached, so the
/// join belongs on a blocking thread instead.
fn release_bridge(bridge: &mut Option<BridgeWorker>) {
    if let Some(worker) = bridge.take() {
        tokio::task::spawn_blocking(move || drop(worker));
    }
}

/// Remember that a client was refused because someone else holds this session.
///
/// Repeated refusals keep the original instant. The holder is judged on how long it has been
/// unreachable *since someone started waiting*, so a client that retries in a tight loop must not
/// be able to keep pushing that deadline out of reach.
fn record_contention(sessions: &Mutex<HashMap<String, Instant>>, name: &str) {
    let Ok(mut contended) = sessions.lock() else {
        return;
    };
    contended.retain(|_, since: &mut Instant| since.elapsed() < CONTENTION_WINDOW);
    if contended.len() >= MAX_CONTENDED_SESSIONS && !contended.contains_key(name) {
        return;
    }
    contended
        .entry(name.to_owned())
        .or_insert_with(Instant::now);
}

/// Forget a session's contention once a client has successfully taken it.
fn clear_contention(sessions: &Mutex<HashMap<String, Instant>>, name: &str) {
    if let Ok(mut contended) = sessions.lock() {
        contended.remove(name);
    }
}

/// When another client first went away empty-handed, if it is still plausibly waiting.
fn contended_since(sessions: &Mutex<HashMap<String, Instant>>, name: &str) -> Option<Instant> {
    sessions
        .lock()
        .ok()
        .and_then(|contended| contended.get(name).copied())
        .filter(|since| since.elapsed() < CONTENTION_WINDOW)
}

/// Ping an attached client that has gone quiet, and give its session up once it stops answering
/// while another client is waiting for that session.
///
/// Silence is the only evidence available. A client that stops reading its socket is otherwise
/// invisible here: terminal frames are small and rendering is gated on acknowledgement, so neither
/// the outbound queue nor the socket buffer comes near the limits that would surface the peer as
/// slow. Silence alone is not enough to disconnect on, though — a healthy client that simply has
/// nothing to say looks identical — so the attachment is only taken away when a refused client is
/// actually waiting for it. An idle client nobody is waiting for keeps its session.
///
/// The wait is timed from when contention began, not from the holder's last word, so a client that
/// has merely been quiet for a while still gets a full interval of pings to answer before it loses
/// the session.
fn check_client_liveness(
    writer: &ClientWriter,
    state: &GatewayState,
    session: Option<&str>,
    last_seen: &mut Instant,
    last_ping: &mut Instant,
) -> io::Result<()> {
    if last_seen.elapsed() >= CLIENT_STALL_TIMEOUT
        && session
            .and_then(|name| contended_since(&state.contended_sessions, name))
            .is_some_and(|since| since.elapsed() >= CLIENT_STALL_TIMEOUT)
    {
        return Err(io::Error::new(io::ErrorKind::TimedOut, STALLED_CLIENT));
    }
    if last_ping.elapsed() < CLIENT_PING_INTERVAL {
        return Ok(());
    }
    *last_ping = Instant::now();
    writer.send(Message::Ping(Vec::new().into()))
}

fn is_stalled_client(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::TimedOut && error.to_string() == STALLED_CLIENT
}

fn send_control(writer: &ClientWriter, message: &ServerControl<'_>) -> io::Result<()> {
    let encoded = encode_control(message)?;
    writer.send(Message::Text(encoded.into()))
}

fn send_error(
    writer: &ClientWriter,
    request_id: Option<u64>,
    code: &'static str,
    message: String,
) -> io::Result<()> {
    send_control(
        writer,
        &ServerControl::Error {
            request_id,
            code,
            message,
        },
    )
}

fn close(writer: &ClientWriter, code: u16, reason: &'static str) -> io::Result<()> {
    writer.send(Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    })))
}

fn offered_subprotocol(headers: &HeaderMap) -> bool {
    offered_protocol(headers, SUBPROTOCOL)
}

fn offered_protocol(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim() == expected)
}

fn protocol_parameter<'a>(headers: &'a HeaderMap, prefix: &str) -> Option<&'a str> {
    let mut matches = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| value.trim().strip_prefix(prefix));
    let first = matches.next().filter(|value| !value.is_empty())?;
    matches.next().is_none().then_some(first)
}

fn validate_origins(origins: Vec<String>) -> io::Result<HashSet<String>> {
    if origins.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "vvmux serve requires at least one exact --allow-origin or [server].allowed_origins entry",
        ));
    }
    let mut validated = HashSet::new();
    for origin in origins {
        let authority = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"));
        if origin.len() > 2048
            || matches!(origin.as_str(), "*" | "null")
            || authority.is_none_or(|value| {
                value.is_empty() || value.contains(['/', '?', '#']) || value.contains('@')
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid exact browser origin {origin:?}"),
            ));
        }
        validated.insert(origin);
    }
    Ok(validated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_is_timed_from_the_first_refusal_and_ends_when_the_session_is_taken() {
        let sessions = Mutex::new(HashMap::new());
        assert_eq!(contended_since(&sessions, "work"), None);

        record_contention(&sessions, "work");
        let first = contended_since(&sessions, "work").expect("a refused client is waiting");

        // A client that retries in a tight loop must not push the holder's deadline out of reach:
        // every later refusal keeps the instant the waiting started.
        record_contention(&sessions, "work");
        record_contention(&sessions, "work");
        assert_eq!(contended_since(&sessions, "work"), Some(first));

        // Taking the session over resolves the contention, so the new holder is not judged against
        // the refusal it just won.
        clear_contention(&sessions, "work");
        assert_eq!(contended_since(&sessions, "work"), None);
    }

    #[test]
    fn contention_expires_and_stays_bounded() {
        let sessions = Mutex::new(HashMap::new());
        {
            let mut guard = sessions.lock().expect("fresh lock");
            guard.insert(
                "stale".into(),
                Instant::now() - CONTENTION_WINDOW - Duration::from_secs(1),
            );
        }
        assert_eq!(
            contended_since(&sessions, "stale"),
            None,
            "a refusal older than the window no longer counts as someone waiting"
        );

        for index in 0..(MAX_CONTENDED_SESSIONS * 2) {
            record_contention(&sessions, &format!("session-{index}"));
        }
        let recorded = sessions.lock().expect("lock").len();
        assert!(
            recorded <= MAX_CONTENDED_SESSIONS,
            "refused attachments grew the map without bound: {recorded}"
        );
        assert!(
            !sessions.lock().expect("lock").contains_key("stale"),
            "recording prunes entries that have aged out"
        );
    }

    #[test]
    fn a_silent_holder_only_loses_its_session_once_someone_is_waiting() {
        let sessions: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
        let silent = Instant::now() - CLIENT_STALL_TIMEOUT - Duration::from_secs(1);

        let stalled = |sessions: &Mutex<HashMap<String, Instant>>, last_seen: Instant| {
            last_seen.elapsed() >= CLIENT_STALL_TIMEOUT
                && contended_since(sessions, "work")
                    .is_some_and(|since| since.elapsed() >= CLIENT_STALL_TIMEOUT)
        };

        assert!(
            !stalled(&sessions, silent),
            "an idle client nobody is waiting for keeps its session"
        );

        record_contention(&sessions, "work");
        assert!(
            !stalled(&sessions, silent),
            "a holder gets a full interval to answer after the waiting starts"
        );

        {
            let mut guard = sessions.lock().expect("lock");
            guard.insert(
                "work".into(),
                Instant::now() - CLIENT_STALL_TIMEOUT - Duration::from_secs(1),
            );
        }
        assert!(
            stalled(&sessions, silent),
            "a holder that stayed silent while another client waited gives the session up"
        );
        assert!(
            !stalled(&sessions, Instant::now()),
            "a holder that answered keeps its session even while contended"
        );
    }

    #[test]
    fn origins_are_exact_and_web_only() {
        assert!(validate_origins(Vec::new()).is_err());
        assert!(validate_origins(vec!["*".into()]).is_err());
        assert!(validate_origins(vec!["null".into()]).is_err());
        assert!(validate_origins(vec!["file://local".into()]).is_err());
        assert!(validate_origins(vec!["http://localhost:3000/path".into()]).is_err());
        assert!(
            validate_origins(vec!["http://localhost:3000".into()])
                .unwrap()
                .contains("http://localhost:3000")
        );
    }

    #[test]
    fn required_subprotocol_is_detected_without_partial_matches() {
        let mut headers = HeaderMap::new();
        headers.insert(SEC_WEBSOCKET_PROTOCOL, "chat, vvmux.v1".parse().unwrap());
        assert!(offered_subprotocol(&headers));
        headers.insert(SEC_WEBSOCKET_PROTOCOL, "vvmux.v10".parse().unwrap());
        assert!(!offered_subprotocol(&headers));
    }

    #[test]
    fn vivid_protocol_parameters_must_be_present_once() {
        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            "vvmux.vivid.v1, vvmux.connection.abc, vvmux.auth.secret, vvmux.kind.2"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            protocol_parameter(&headers, vivid::CONNECTION_PROTOCOL_PREFIX),
            Some("abc")
        );
        assert_eq!(
            protocol_parameter(&headers, vivid::KIND_PROTOCOL_PREFIX),
            Some("2")
        );
        headers.append(
            SEC_WEBSOCKET_PROTOCOL,
            "vvmux.connection.duplicate".parse().unwrap(),
        );
        assert_eq!(
            protocol_parameter(&headers, vivid::CONNECTION_PROTOCOL_PREFIX),
            None
        );
    }
}
