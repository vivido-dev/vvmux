//! A minimal VVTUN/1 server for integration tests, in-process.
//!
//! It serves the enrollment contract, the control tunnel, and the data-leg
//! endpoint, and hands the test control of both ends. It reimplements the
//! handshake verification with the domain constant duplicated from
//! `src/gateway/identity.rs`, because this is a binary crate and tests cannot
//! import it; the tunnel tests prove the two implementations agree.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{get, post};
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

pub const AUTH_DOMAIN: &[u8] = b"vvmux tunnel auth v1\0";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerControl {
    Challenge {
        protocol: u16,
        nonce: String,
        hostname: String,
    },
    Authed {
        protocol: u16,
        server_version: String,
        #[serde(default)]
        reconnect_after_seconds: u64,
    },
    Ping {
        #[serde(default)]
        nonce: u64,
    },
    Pong {
        #[serde(default)]
        nonce: u64,
    },
    OpenLeg {
        leg_id: u64,
        kind: String,
        route: String,
        account: String,
        ticket: String,
        #[serde(default)]
        subprotocols: Vec<String>,
    },
    CloseLeg {
        leg_id: u64,
        #[serde(default)]
        reason: String,
    },
    GoingAway {
        #[serde(default)]
        reason: String,
        #[serde(default)]
        reconnect_after_seconds: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientControl {
    Auth {
        machine_id: String,
        signature: String,
    },
    MachineStatus {
        vvmux_version: String,
        vvws_protocol: u16,
        capabilities: Vec<String>,
    },
    Ping {
        #[serde(default)]
        nonce: u64,
    },
    Pong {
        #[serde(default)]
        nonce: u64,
    },
    LegFailed {
        leg_id: u64,
        code: String,
    },
}

/// A connected leg's server-side halves, handed to the test.
pub struct LegSocket {
    pub sink: futures_util::stream::SplitSink<WebSocket, Message>,
    pub stream: futures_util::stream::SplitStream<WebSocket>,
}

#[derive(Default)]
struct HarnessState {
    /// machine_id -> public key, populated by the enrollment endpoint.
    machine_key: Mutex<HashMap<String, VerifyingKey>>,
    /// One-time enrollment codes.
    enroll_code: Mutex<Option<String>>,
    /// One-use leg tickets minted for an open_leg.
    tickets: Mutex<HashMap<String, u64>>,
    /// The live control tunnel's writer (the test sends frames through it).
    control_writer: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Kills the live control session, closing the gateway's connection.
    control_kill: Mutex<Option<mpsc::UnboundedSender<()>>>,
    /// Frames the gateway sent on the control tunnel.
    control_in: Mutex<Option<mpsc::UnboundedReceiver<ClientControl>>>,
    /// leg_id -> oneshot receiving the leg socket, claimed by the test.
    leg_sockets: Mutex<HashMap<u64, oneshot::Receiver<LegSocket>>>,
    /// The hostname this harness challenges with; the signature must match it.
    hostname: Mutex<String>,
    /// Whether a control connection receives its initial challenge.
    challenge_enabled: AtomicBool,
    /// Control upgrade attempts, including rejected ones.
    control_attempts: AtomicUsize,
    control_attempt_times: Mutex<Vec<Instant>>,
    /// Optional Retry-After value for the next control upgrade rejection.
    reject_next_control: Mutex<Option<u64>>,
    /// Optional deliberately incorrect enrollment response machine ID.
    enrollment_machine_id_override: Mutex<Option<String>>,
    last_enrollment_host: Mutex<Option<String>>,
}

pub struct TunnelHarness {
    pub addr: SocketAddr,
    state: Arc<HarnessState>,
    _shutdown: tokio::sync::broadcast::Sender<()>,
}

impl TunnelHarness {
    pub async fn start(enroll_code: &str) -> Self {
        let state = Arc::new(HarnessState {
            enroll_code: Mutex::new(Some(enroll_code.to_owned())),
            // The gateway signs the hostname from its connect URL; for loopback
            // tests that is "127.0.0.1", so that is the default here.
            hostname: Mutex::new("127.0.0.1".to_owned()),
            challenge_enabled: AtomicBool::new(true),
            ..HarnessState::default()
        });
        let (shutdown, _) = tokio::sync::broadcast::channel(1);
        let shutdown_task = shutdown.clone();
        let app = Router::new()
            .route("/api/v1/machines/enroll", post(enroll_handler))
            .route("/t/v1/control", get(control_upgrade))
            .route("/t/v1/leg", get(leg_upgrade))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_task.subscribe().recv().await;
                })
                .await;
        });
        TunnelHarness {
            addr,
            state,
            _shutdown: shutdown,
        }
    }

    pub fn enroll_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_hostname(&self, hostname: &str) {
        *self.state.hostname.lock().unwrap() = hostname.to_owned();
    }

    pub fn set_challenge_enabled(&self, enabled: bool) {
        self.state
            .challenge_enabled
            .store(enabled, Ordering::SeqCst);
    }

    pub fn control_attempts(&self) -> usize {
        self.state.control_attempts.load(Ordering::SeqCst)
    }

    pub fn control_attempt_times(&self) -> Vec<Instant> {
        self.state.control_attempt_times.lock().unwrap().clone()
    }

    pub fn reject_next_control(&self, retry_after_seconds: u64) {
        *self.state.reject_next_control.lock().unwrap() = Some(retry_after_seconds);
    }

    pub fn set_enrollment_machine_id_override(&self, machine_id: &str) {
        *self.state.enrollment_machine_id_override.lock().unwrap() = Some(machine_id.to_owned());
    }

    pub fn last_enrollment_host(&self) -> Option<String> {
        self.state.last_enrollment_host.lock().unwrap().clone()
    }

    /// Wait for the gateway's next control frame.
    pub async fn next_control(&self) -> Option<ClientControl> {
        loop {
            let receiver = self.state.control_in.lock().unwrap().take();
            let Some(mut receiver) = receiver else {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            };
            let frame = match receiver.recv().await {
                Some(frame) => frame,
                // The session ended; wait for the next one.
                None => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    continue;
                }
            };
            // Put the receiver back for the next call.
            *self.state.control_in.lock().unwrap() = Some(receiver);
            match frame {
                ClientControl::Ping { .. } => continue,
                other => return Some(other),
            }
        }
    }

    /// Send a control frame to the gateway.
    pub fn send_control(&self, frame: serde_json::Value) {
        let lock = self.state.control_writer.lock().unwrap();
        if let Some(writer) = lock.as_ref() {
            let _ = writer.send(serde_json::to_string(&frame).unwrap());
        }
    }

    /// Mint a ticket, send open_leg, and wait for the gateway to dial the leg.
    pub async fn open_leg(&self, leg_id: u64, kind: &str, subprotocols: Vec<String>) -> String {
        let ticket = random_ticket();
        self.state
            .tickets
            .lock()
            .unwrap()
            .insert(ticket.clone(), leg_id);
        self.send_control(serde_json::json!({
            "type": "open_leg",
            "leg_id": leg_id,
            "kind": kind,
            "route": "test-route",
            "account": "issuer#subject",
            "ticket": ticket,
            "subprotocols": subprotocols,
        }));
        ticket
    }

    /// Send open_leg with a ticket the leg endpoint will reject.
    pub fn open_leg_bad_ticket(&self, leg_id: u64, kind: &str) {
        self.send_control(serde_json::json!({
            "type": "open_leg",
            "leg_id": leg_id,
            "kind": kind,
            "route": "test-route",
            "account": "issuer#subject",
            "ticket": random_ticket(),
            "subprotocols": [],
        }));
    }

    pub fn close_leg(&self, leg_id: u64) {
        self.send_control(serde_json::json!({
            "type": "close_leg",
            "leg_id": leg_id,
            "reason": "test close",
        }));
    }

    /// Wait for the gateway's leg connection and take its server-side halves.
    pub async fn accept_leg(&self, leg_id: u64) -> LegSocket {
        loop {
            let receiver = {
                let mut sockets = self.state.leg_sockets.lock().unwrap();
                sockets.remove(&leg_id)
            };
            if let Some(receiver) = receiver {
                return tokio::time::timeout(std::time::Duration::from_secs(10), receiver)
                    .await
                    .expect("gateway did not dial the leg")
                    .expect("leg socket sender dropped");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// Drop the control tunnel; the gateway must detect the loss and reconnect.
    pub fn drop_control(&self) {
        *self.state.control_writer.lock().unwrap() = None;
        if let Some(kill) = self.state.control_kill.lock().unwrap().take() {
            let _ = kill.send(());
        }
    }

    /// The public key registered for a machine, after enrollment.
    pub fn registered_key(&self, machine_id: &str) -> Option<VerifyingKey> {
        self.state
            .machine_key
            .lock()
            .unwrap()
            .get(machine_id)
            .copied()
    }
}

fn random_ticket() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).unwrap();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

async fn enroll_handler(
    axum::extract::State(state): axum::extract::State<Arc<HarnessState>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    *state.last_enrollment_host.lock().unwrap() = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let code = body
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let public_key = body
        .get("public_key")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let valid_code = state.enroll_code.lock().unwrap().take();
    if valid_code.as_deref() != Some(code) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "unknown or used code"})),
        )
            .into_response();
    }
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(public_key) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "bad public key"})),
        )
            .into_response();
    };
    let Ok(key) = VerifyingKey::from_bytes(&decoded.try_into().unwrap_or([0; 32])) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "bad public key"})),
        )
            .into_response();
    };
    let machine_id = public_key.to_owned();
    state
        .machine_key
        .lock()
        .unwrap()
        .insert(machine_id.clone(), key);
    let returned_machine_id = state
        .enrollment_machine_id_override
        .lock()
        .unwrap()
        .clone()
        .unwrap_or(machine_id);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"machine_id": returned_machine_id})),
    )
        .into_response()
}

async fn control_upgrade(
    axum::extract::State(state): axum::extract::State<Arc<HarnessState>>,
    ws: WebSocketUpgrade,
) -> Response {
    state.control_attempts.fetch_add(1, Ordering::SeqCst);
    state
        .control_attempt_times
        .lock()
        .unwrap()
        .push(Instant::now());
    if let Some(seconds) = state.reject_next_control.lock().unwrap().take() {
        let mut response = (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response();
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_str(&seconds.to_string()).unwrap(),
        );
        return response;
    }
    ws.protocols(["vvtun.v1"])
        .max_frame_size(64 * 1024)
        .max_message_size(64 * 1024)
        .on_upgrade(move |socket| control_session(state, socket))
}

async fn control_session(state: Arc<HarnessState>, socket: WebSocket) {
    let (mut writer, mut reader) = socket.split();
    if !state.challenge_enabled.load(Ordering::SeqCst) {
        while reader.next().await.is_some() {}
        return;
    }
    // Challenge with a fresh nonce.
    let nonce = random_ticket();
    let hostname = state.hostname.lock().unwrap().clone();
    let challenge = serde_json::to_string(&ServerControl::Challenge {
        protocol: 1,
        nonce: nonce.clone(),
        hostname: hostname.clone(),
    })
    .unwrap();
    if writer.send(Message::Text(challenge.into())).await.is_err() {
        return;
    }

    // Read and verify the auth.
    let first = match tokio::time::timeout(std::time::Duration::from_secs(10), reader.next()).await
    {
        Ok(Some(Ok(Message::Text(text)))) => text.as_str().to_owned(),
        _ => return,
    };
    let Ok(ClientControl::Auth {
        machine_id,
        signature,
    }) = serde_json::from_str::<ClientControl>(&first)
    else {
        return;
    };
    let verified = state
        .machine_key
        .lock()
        .unwrap()
        .get(&machine_id)
        .copied()
        .and_then(|key| {
            let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&nonce)
                .ok()?;
            let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&signature)
                .ok()?;
            let signature = Signature::from_slice(&signature).ok()?;
            let mut message = Vec::new();
            message.extend_from_slice(AUTH_DOMAIN);
            message.extend_from_slice(&nonce);
            message.extend_from_slice(hostname.as_bytes());
            // Loopback ws:// development tunnels export no keying material.
            message.extend_from_slice(&[]);
            message.extend_from_slice(machine_id.as_bytes());
            key.verify_strict(&message, &signature).ok()
        });
    if verified.is_none() {
        let _ = writer.send(Message::Close(None)).await;
        return;
    }

    let authed = serde_json::to_string(&ServerControl::Authed {
        protocol: 1,
        server_version: "test-server".to_owned(),
        reconnect_after_seconds: 0,
    })
    .unwrap();
    if writer.send(Message::Text(authed.into())).await.is_err() {
        return;
    }

    // Wire the two channels: gateway -> test and test -> gateway.
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ClientControl>();
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<()>();
    *state.control_writer.lock().unwrap() = Some(writer_tx);
    *state.control_in.lock().unwrap() = Some(in_rx);
    *state.control_kill.lock().unwrap() = Some(kill_tx);

    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(encoded) = writer_rx.recv() => {
                    if writer.send(Message::Text(encoded.into())).await.is_err() {
                        break;
                    }
                }
                Some(json) = pong_rx.recv() => {
                    if writer.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                else => break,
            }
        }
    });
    loop {
        tokio::select! {
            message = reader.next() => {
                let Some(Ok(message)) = message else { break };
                match message {
            Message::Text(text) => {
                let Ok(frame) = serde_json::from_str::<ClientControl>(&text) else {
                    break;
                };
                match frame {
                    ClientControl::Ping { .. } => {
                        let _ = pong_tx.send(
                            serde_json::to_string(&ServerControl::Pong { nonce: 0 }).unwrap(),
                        );
                        continue;
                    }
                    other => {
                        if in_tx.send(other).is_err() {
                            break;
                        }
                    }
                }
            }
            Message::Ping(bytes) => {
                let payload = String::from_utf8_lossy(&bytes).into_owned();
                let _ = pong_tx.send(payload);
            }
            Message::Close(_) | Message::Binary(_) => break,
            _ => {}
        }
            }
            _ = kill_rx.recv() => break,
        }
    }
    writer_task.abort();
    *state.control_writer.lock().unwrap() = None;
    *state.control_in.lock().unwrap() = None;
    *state.control_kill.lock().unwrap() = None;
}

async fn leg_upgrade(
    axum::extract::State(state): axum::extract::State<Arc<HarnessState>>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Exactly `vvtun.leg.v1` and one `vvtun.ticket.<value>`.
    let mut has_leg = false;
    let mut ticket = None;
    for value in headers.get_all("sec-websocket-protocol").iter() {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for part in value.split(',') {
            let part = part.trim();
            if part == "vvtun.leg.v1" {
                has_leg = true;
            } else if let Some(rest) = part.strip_prefix("vvtun.ticket.") {
                if ticket.is_some() {
                    return (StatusCode::BAD_REQUEST, "duplicate ticket").into_response();
                }
                ticket = Some(rest.to_owned());
            }
        }
    }
    let Some(ticket) = ticket else {
        return (StatusCode::BAD_REQUEST, "ticket subprotocol required").into_response();
    };
    if !has_leg {
        return (StatusCode::BAD_REQUEST, "leg subprotocol required").into_response();
    }
    let Some(leg_id) = state.tickets.lock().unwrap().remove(&ticket) else {
        // Unknown, expired, or reused ticket.
        return (StatusCode::UNAUTHORIZED, "unknown ticket").into_response();
    };
    let (sender, receiver) = oneshot::channel();
    state.leg_sockets.lock().unwrap().insert(leg_id, receiver);
    ws.protocols(["vvtun.leg.v1"])
        .max_frame_size(1024 * 1024)
        .max_message_size(1024 * 1024)
        .on_upgrade(move |socket| async move {
            let (sink, stream) = socket.split();
            let _ = sender.send(LegSocket { sink, stream });
            // The test owns both halves; keep the connection alive until the
            // harness drops it.
            std::future::pending::<()>().await;
        })
}

/// Run a `vvmux` binary command with an isolated runtime directory.
///
/// `HOME` is isolated along with the XDG directories, and it is not optional. Pane shells are login
/// shells (`vvmux-terminal/src/pty/unix.rs:146`), so each one sources the invoking user's
/// `~/.profile` — and the conventional profile prepends `$HOME/bin` and `$HOME/.local/bin` to `PATH`
/// when those directories exist. A test that puts a fixture executable first on the `PATH` it passes
/// here would therefore have its ordering silently rewritten by the developer's own profile, and a
/// real binary of the same name would win instead. That failure is invisible on a machine that
/// happens not to have one installed, which is exactly how it survived unnoticed.
///
/// `XDG_STATE_HOME` is pinned for a second reason: without it, persisted session state falls back to
/// `$HOME/.local/state`, and an integration test would write snapshots into the developer's real
/// state directory.
pub fn vvmux_command(runtime: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_vvmux"));
    command.env("XDG_RUNTIME_DIR", runtime);
    command.env("XDG_CONFIG_HOME", runtime);
    command.env("XDG_STATE_HOME", runtime);
    command.env("HOME", runtime);
    command
}
