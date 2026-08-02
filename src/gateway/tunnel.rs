//! Outbound machine tunnel client (`vvmux serve --connect`), VVTUN/1.
//!
//! The gateway dials out; it opens no listener. It authenticates with an enrolled
//! Ed25519 identity, holds one control tunnel, and dials one data leg per browser
//! socket on `open_leg`. The VVWS/1 and Vivid loops run on the legs through the
//! carrier-neutral seam of `transport.rs`, so everything the loopback gateway does
//! — list, create, attach, takeover, media — works identically over a tunnel.
//!
//! Liveness is two layers (VVTUN-1): server-originated WebSocket pings prove the
//! socket lives; gateway-originated application pings prove the state machine is
//! running. Three consecutive misses declare the peer dead. Reconnect uses
//! full-jittered exponential backoff and honors `Retry-After`.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, client_async_tls_with_config};

use super::identity::MachineIdentity;
use super::protocol::{MAX_FRAME_BYTES, VERSION as VVWS_VERSION};
use super::transport::{
    Frame, FrameSink, FrameStream, TungsteniteSink, TungsteniteStream, TunnelStream,
};
use super::{GatewayState, TunnelContext, vivid};

/// The control-tunnel subprotocol.
pub(crate) const TUNNEL_SUBPROTOCOL: &str = "vvtun.v1";
/// The data-leg subprotocol.
pub(crate) const LEG_SUBPROTOCOL: &str = "vvtun.leg.v1";
/// Prefix of the one-use leg ticket subprotocol.
pub(crate) const LEG_TICKET_PREFIX: &str = "vvtun.ticket.";
/// Label used to export the tunnel binding key from the TLS session (VVTUN-1).
const EXPORTER_LABEL: &[u8] = b"EXPORTER-VVTUN-1";

const CONTROL_MAX_BYTES: usize = 64 * 1024;
const MAX_TUNNEL_LEGS: usize = 32;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);
const DEFAULT_MISS_LIMIT: u32 = 3;

/// Options for `vvmux serve --connect`.
pub(crate) struct ConnectOptions {
    pub url: String,
    pub identity_file: PathBuf,
    pub allow_accounts: Vec<String>,
    pub allow_kill: bool,
    /// Test and diagnostic knobs; the defaults are the VVTUN-1 constants.
    pub heartbeat: Option<Duration>,
    pub miss_limit: Option<u32>,
}

/// Strict VVTUN/1 server-to-gateway control frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ServerControl {
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

/// Strict VVTUN/1 gateway-to-server control frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientControl {
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

struct TunnelRunner {
    url: String,
    hostname: String,
    leg_url: String,
    identity: MachineIdentity,
    state: GatewayState,
    allow_accounts: HashSet<String>,
    allow_kill: bool,
    heartbeat: Duration,
    miss_limit: u32,
    legs: Arc<Semaphore>,
}

pub(crate) fn run_connect(
    config: crate::config::Config,
    config_path: Option<PathBuf>,
    options: ConnectOptions,
) -> io::Result<()> {
    let state = super::build_state(config, config_path, HashSet::new(), None);
    let (base, hostname, leg_url) = split_urls(&options.url)?;
    let identity = MachineIdentity::load(&options.identity_file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not load identity {}: {error}; run `vvmux cloud enroll` first",
                options.identity_file.display()
            ),
        )
    })?;
    let runner = TunnelRunner {
        url: base + "/t/v1/control",
        hostname,
        leg_url,
        identity,
        state,
        allow_accounts: options.allow_accounts.into_iter().collect(),
        allow_kill: options.allow_kill,
        heartbeat: options.heartbeat.unwrap_or(DEFAULT_HEARTBEAT),
        miss_limit: options.miss_limit.unwrap_or(DEFAULT_MISS_LIMIT),
        legs: Arc::new(Semaphore::new(MAX_TUNNEL_LEGS)),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("vvmux-tunnel")
        .build()?;
    runtime.block_on(run_reconnect_loop(&runner))
}

/// Split a connect URL into the control URL, the hostname for the handshake
/// signature, and the leg URL. `wss://` is required across a host boundary;
/// `ws://` is accepted for loopback development only.
fn split_urls(url: &str) -> io::Result<(String, String, String)> {
    let parsed = url::Url::parse(url).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid --connect URL: {error}"),
        )
    })?;
    let (scheme, tls) = match parsed.scheme() {
        "wss" => ("wss", true),
        "ws" => ("ws", false),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--connect scheme must be wss or ws, got {other}"),
            ));
        }
    };
    if !tls {
        let host = parsed.host_str().unwrap_or_default();
        if host != "127.0.0.1" && host != "::1" && host != "localhost" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ws:// is accepted for loopback development only; use wss:// across a host boundary",
            ));
        }
    }
    let Some(host) = parsed.host_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--connect URL has no host",
        ));
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--connect URL has no port"))?;
    let host_port = match parsed.host() {
        Some(url::Host::Ipv6(_)) => format!("[{host}]:{port}"),
        _ => format!("{host}:{port}"),
    };
    Ok((
        format!("{scheme}://{host_port}"),
        host.to_owned(),
        format!("{scheme}://{host_port}/t/v1/leg"),
    ))
}

fn full_jitter(cap: Duration) -> Duration {
    let cap_nanos = cap.as_nanos().min(u64::MAX as u128) as u64;
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("random backoff generation");
    let value = u64::from_ne_bytes(bytes);
    Duration::from_nanos(value % cap_nanos.saturating_add(1))
}

async fn run_reconnect_loop(runner: &TunnelRunner) -> io::Result<()> {
    let mut backoff_cap = RECONNECT_MIN;
    loop {
        let (completed, retry_after) = match run_tunnel_once(runner).await {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("vvmux tunnel: {error}");
                (false, None)
            }
        };
        if completed {
            // A successful connection — even a short one — resets the backoff.
            backoff_cap = RECONNECT_MIN;
        }
        let delay = match retry_after {
            Some(seconds) => RECONNECT_MAX.min(Duration::from_secs(seconds)),
            None => {
                let delay = full_jitter(backoff_cap);
                backoff_cap = (backoff_cap * 2).min(RECONNECT_MAX);
                delay
            }
        };
        tokio::time::sleep(delay).await;
    }
}

/// One tunnel lifetime. Returns `(completed, retry_after_hint)`.
async fn run_tunnel_once(runner: &TunnelRunner) -> io::Result<(bool, Option<u64>)> {
    let (socket, _response) = connect_with_exporter(
        &runner.url,
        &[TUNNEL_SUBPROTOCOL.to_owned()],
        CONTROL_MAX_BYTES,
    )
    .await?;
    let (exporter, stream) = split_tunnel(socket);

    let (mut sink, mut reader) = {
        let (sink, stream) = stream.split();
        (TungsteniteSink(sink), TungsteniteStream(stream))
    };

    // Handshake: challenge -> auth -> authed -> machine_status.
    let challenge = read_control_frame(&mut reader).await?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server closed before challenge",
        )
    })?;
    let ServerControl::Challenge {
        protocol, nonce, ..
    } = challenge
    else {
        return Err(io::Error::other("expected VVTUN challenge"));
    };
    if protocol != 1 {
        return Err(io::Error::other("unsupported VVTUN protocol version"));
    }
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(nonce.as_str())
        .map_err(|_| io::Error::other("challenge nonce is not base64url"))?;
    if nonce.len() != 32 {
        return Err(io::Error::other("challenge nonce must be 32 bytes"));
    }
    let exporter = exporter
        .map(|bytes| bytes.as_slice().to_vec())
        .unwrap_or_default();
    let signature = runner
        .identity
        .sign_handshake(&nonce, &runner.hostname, &exporter);
    send_control(
        &mut sink,
        &ClientControl::Auth {
            machine_id: runner.identity.machine_id(),
            signature,
        },
    )
    .await?;

    let authed = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_control_frame(&mut reader))
        .await
        .map_err(|_| io::Error::other("authentication timed out"))??
        .ok_or_else(|| io::Error::other("server closed during authentication"))?;
    let ServerControl::Authed {
        protocol,
        server_version,
        reconnect_after_seconds: _,
    } = authed
    else {
        return Err(io::Error::other("expected VVTUN authed"));
    };
    if protocol != 1 {
        return Err(io::Error::other("unsupported VVTUN protocol version"));
    }
    let server_version = bounded_ascii(&server_version, 64)?;

    let mut capabilities = vec![
        "terminal-v1",
        "session-list-v1",
        "session-create-v1",
        "vivid-bridge-v1",
        "tunnel-attached-v1",
    ];
    if runner.allow_kill {
        capabilities.push("session-kill-v1");
    }
    send_control(
        &mut sink,
        &ClientControl::MachineStatus {
            vvmux_version: env!("CARGO_PKG_VERSION").to_owned(),
            vvws_protocol: VVWS_VERSION,
            capabilities: capabilities.into_iter().map(str::to_owned).collect(),
        },
    )
    .await?;
    println!(
        "vvmux tunnel connected to {} (server {server_version})",
        runner.url
    );

    // Control loop with the application heartbeat.
    let (leg_reports, mut report_receiver) = mpsc::unbounded_channel::<ClientControl>();
    let mut legs: std::collections::HashMap<u64, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();
    let mut heartbeat = tokio::time::interval(runner.heartbeat);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ping_nonce: u64 = 0;
    let mut missed: u32 = 0;
    let mut outcome = (true, None);

    loop {
        tokio::select! {
            incoming = reader.next_frame() => {
                match incoming {
                    Some(Ok(frame)) => {
                        missed = 0;
                        match handle_server_frame(
                            &mut sink,
                            &mut legs,
                            &leg_reports,
                            runner,
                            frame,
                        ).await {
                            Ok(Some(reconnect_after)) => {
                                outcome = (true, Some(reconnect_after));
                                break;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                eprintln!("vvmux tunnel: {error}");
                                outcome = (true, None);
                                break;
                            }
                        }
                    }
                    Some(Err(error)) => {
                        eprintln!("vvmux tunnel: {error}");
                        outcome = (true, None);
                        break;
                    }
                    None => {
                        outcome = (true, None);
                        break;
                    }
                }
            }
            report = report_receiver.recv() => {
                match report {
                    Some(control) => {
                        if let Err(error) = send_control(&mut sink, &control).await {
                            eprintln!("vvmux tunnel: {error}");
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = heartbeat.tick() => {
                ping_nonce = ping_nonce.wrapping_add(1);
                missed = missed.saturating_add(1);
                if missed > runner.miss_limit {
                    eprintln!("vvmux tunnel: peer missed {missed} heartbeats; declaring it dead");
                    outcome = (true, None);
                    break;
                }
                if let Err(error) = send_control(&mut sink, &ClientControl::Ping { nonce: ping_nonce }).await {
                    eprintln!("vvmux tunnel: {error}");
                    break;
                }
            }
        }
    }

    // Close every leg the server has not closed itself.
    for handle in legs.into_values() {
        handle.abort();
    }
    Ok(outcome)
}

/// Returns `Some(reconnect_after)` when the connection should end cleanly.
async fn handle_server_frame<Si: FrameSink>(
    sink: &mut Si,
    legs: &mut std::collections::HashMap<u64, tokio::task::JoinHandle<()>>,
    leg_reports: &mpsc::UnboundedSender<ClientControl>,
    runner: &TunnelRunner,
    frame: Frame,
) -> io::Result<Option<u64>> {
    match frame {
        Frame::Ping(bytes) => {
            sink.send_frame(Frame::Pong(bytes)).await?;
            Ok(None)
        }
        Frame::Pong(_) => Ok(None),
        Frame::Close(_) => Ok(Some(0)),
        Frame::Binary(_) => Err(io::Error::other("binary frame on the VVTUN control tunnel")),
        Frame::Text(text) => match decode_server_control(&text)? {
            ServerControl::Ping { nonce } => {
                send_control(sink, &ClientControl::Pong { nonce }).await?;
                Ok(None)
            }
            ServerControl::Pong { .. } => Ok(None),
            ServerControl::OpenLeg {
                leg_id,
                kind,
                route,
                account,
                ticket,
                subprotocols,
            } => {
                open_leg(
                    legs,
                    leg_reports,
                    runner,
                    leg_id,
                    &kind,
                    &route,
                    &account,
                    &ticket,
                    &subprotocols,
                )
                .await?;
                Ok(None)
            }
            ServerControl::CloseLeg { leg_id, .. } => {
                if let Some(handle) = legs.remove(&leg_id) {
                    handle.abort();
                }
                Ok(None)
            }
            ServerControl::GoingAway {
                reconnect_after_seconds,
                ..
            } => Ok(Some(reconnect_after_seconds)),
            ServerControl::Challenge { .. } | ServerControl::Authed { .. } => {
                Err(io::Error::other("out-of-sequence VVTUN control frame"))
            }
        },
    }
}

fn decode_server_control(text: &str) -> io::Result<ServerControl> {
    if text.len() > CONTROL_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VVTUN control frame too large",
        ));
    }
    serde_json::from_str(text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid VVTUN control frame: {error}"),
        )
    })
}

#[allow(clippy::too_many_arguments)]
async fn open_leg(
    legs: &mut std::collections::HashMap<u64, tokio::task::JoinHandle<()>>,
    leg_reports: &mpsc::UnboundedSender<ClientControl>,
    runner: &TunnelRunner,
    leg_id: u64,
    kind: &str,
    _route: &str,
    account: &str,
    ticket: &str,
    subprotocols: &[String],
) -> io::Result<()> {
    if !runner.allow_accounts.is_empty() && !runner.allow_accounts.contains(account) {
        let _ = leg_reports.send(ClientControl::LegFailed {
            leg_id,
            code: "not_permitted".to_owned(),
        });
        return Ok(());
    }
    let kind = match kind {
        "vvws" => LegKind::Vvws,
        "vivid" => LegKind::Vivid,
        _ => {
            let _ = leg_reports.send(ClientControl::LegFailed {
                leg_id,
                code: "invalid_request".to_owned(),
            });
            return Ok(());
        }
    };
    if legs.len() >= MAX_TUNNEL_LEGS || runner.legs.clone().try_acquire_owned().is_err() {
        let _ = leg_reports.send(ClientControl::LegFailed {
            leg_id,
            code: "capacity".to_owned(),
        });
        return Ok(());
    }
    // The ticket is a one-use 32-byte base64url value; anything else is refused
    // before any network cost.
    let Ok(ticket_bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(ticket) else {
        let _ = leg_reports.send(ClientControl::LegFailed {
            leg_id,
            code: "ticket_rejected".to_owned(),
        });
        return Ok(());
    };
    if ticket_bytes.len() != 32 {
        let _ = leg_reports.send(ClientControl::LegFailed {
            leg_id,
            code: "ticket_rejected".to_owned(),
        });
        return Ok(());
    }

    let permit = runner.legs.clone().acquire_owned().await;
    let state = runner.state.clone();
    let allow_kill = runner.allow_kill;
    let ticket = ticket.to_owned();
    let subprotocols = subprotocols.to_vec();
    let leg_url = runner.leg_url.clone();
    let reports = leg_reports.clone();
    let handle = tokio::spawn(async move {
        let _permit = permit;
        let result = run_leg(
            &leg_url,
            leg_id,
            kind,
            &ticket,
            &subprotocols,
            &state,
            allow_kill,
        )
        .await;
        if let Err(error) = result {
            let _ = reports.send(ClientControl::LegFailed {
                leg_id,
                code: leg_failure_code(&error),
            });
        }
    });
    legs.insert(leg_id, handle);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum LegKind {
    Vvws,
    Vivid,
}

async fn run_leg(
    leg_url: &str,
    _leg_id: u64,
    kind: LegKind,
    ticket: &str,
    subprotocols: &[String],
    state: &GatewayState,
    allow_kill: bool,
) -> io::Result<()> {
    let max_bytes = match kind {
        LegKind::Vvws => MAX_FRAME_BYTES,
        LegKind::Vivid => {
            vivid_protocol::HARD_MAX_RECORD_BODY as usize
                + vivid_protocol::wire::HEADER_SIZE
                + vivid_protocol::wire::PREFACE_SIZE
        }
    };
    let offered = [
        LEG_SUBPROTOCOL.to_owned(),
        format!("{LEG_TICKET_PREFIX}{ticket}"),
    ];
    let (socket, _response) = connect_with_exporter(leg_url, &offered, max_bytes).await?;
    let (_exporter, stream) = split_tunnel(socket);
    let (sink, reader) = {
        let (sink, stream) = stream.split();
        (TungsteniteSink(sink), TungsteniteStream(stream))
    };
    match kind {
        LegKind::Vvws => {
            super::handle_connection(
                sink,
                reader,
                state.clone(),
                None,
                Some(TunnelContext { allow_kill }),
            )
            .await;
            Ok(())
        }
        LegKind::Vivid => {
            let (broker, kind) = super::resolve_vivid_leg(subprotocols, state)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
            vivid::serve_socket(sink, reader, broker, kind).await
        }
    }
}

fn leg_failure_code(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "ticket_rejected".to_owned(),
        io::ErrorKind::InvalidData => "invalid_subprotocols".to_owned(),
        io::ErrorKind::TimedOut => "transport_error".to_owned(),
        _ => "transport_error".to_owned(),
    }
}

async fn read_control_frame<St: FrameStream>(reader: &mut St) -> io::Result<Option<ServerControl>> {
    loop {
        let Some(frame) = reader.next_frame().await else {
            return Ok(None);
        };
        let frame = frame?;
        match frame {
            Frame::Text(text) => return decode_server_control(&text).map(Some),
            Frame::Ping(_bytes) => {
                return Err(io::Error::other("server sent a ping before authentication"));
            }
            Frame::Pong(_) => continue,
            Frame::Binary(_) => {
                return Err(io::Error::other("binary frame on the VVTUN control tunnel"));
            }
            Frame::Close(_) => return Ok(None),
        }
    }
}

async fn send_control<Si: FrameSink>(sink: &mut Si, control: &ClientControl) -> io::Result<()> {
    let encoded = serde_json::to_vec(control).map_err(io::Error::other)?;
    if encoded.len() > CONTROL_MAX_BYTES {
        return Err(io::Error::other("VVTUN control frame too large to send"));
    }
    sink.send_frame(Frame::Text(String::from_utf8_lossy(&encoded).into_owned()))
        .await
}

/// Connect and return the exporter-derived binding material for the TLS session,
/// which is empty on a loopback plain `ws://` development connection.
async fn connect_with_exporter(
    url: &str,
    offered: &[String],
    max_bytes: usize,
) -> io::Result<(TunnelStream, Option<[u8; 32]>)> {
    let parsed = url::Url::parse(url).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid WebSocket URL: {error}"),
        )
    })?;
    let host = parsed
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WebSocket URL has no host"))?
        .to_owned();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WebSocket URL has no port"))?;
    let tls = parsed.scheme() == "wss";
    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(io::Error::other)?;
    let connector = if tls {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            roots.add(cert).map_err(io::Error::other)?;
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Some(Connector::Rustls(std::sync::Arc::new(config)))
    } else {
        Some(Connector::Plain)
    };
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(max_bytes);
    config.max_frame_size = Some(max_bytes);

    let mut request = url.into_client_request().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid WebSocket URL: {error}"),
        )
    })?;
    if !offered.is_empty() {
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_str(&offered.join(", ")).map_err(io::Error::other)?,
        );
    }
    let (socket, _response) =
        client_async_tls_with_config(request, stream, Some(config), connector)
            .await
            .map_err(|error| match error {
                // An HTTP rejection is an upgrade failure, not a transport one; the
                // leg path maps it to `ticket_rejected` via PermissionDenied.
                tokio_tungstenite::tungstenite::Error::Http(_) => io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "WebSocket upgrade was rejected",
                ),
                other => io::Error::other(other),
            })?;
    let (exporter, socket) = split_tunnel(socket);
    Ok((socket, exporter))
}

/// Split a connected tunnel stream into its exporter binding and its halves.
fn split_tunnel(socket: TunnelStream) -> (Option<[u8; 32]>, TunnelStream) {
    let exporter = match socket.get_ref() {
        MaybeTlsStream::Rustls(tls) => {
            let mut out = [0_u8; 32];
            if tls
                .get_ref()
                .1
                .export_keying_material(&mut out, EXPORTER_LABEL, None)
                .is_ok()
            {
                Some(out)
            } else {
                None
            }
        }
        _ => None,
    };
    (exporter, socket)
}

fn bounded_ascii(value: &str, max: usize) -> io::Result<&str> {
    if value.len() > max
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+' | b'/'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "untrusted server string",
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_scheme_is_rejected_for_non_loopback_hosts() {
        assert!(split_urls("ws://vvmux.example/t/v1/control").is_err());
        assert!(split_urls("wss://vvmux.example/t/v1/control").is_ok());
        assert!(split_urls("ws://127.0.0.1:8000/t/v1/control").is_ok());
        assert!(split_urls("http://127.0.0.1:8000/t/v1/control").is_err());
    }

    #[test]
    fn full_jitter_stays_within_the_cap() {
        for _ in 0..64 {
            let delay = full_jitter(Duration::from_secs(2));
            assert!(delay <= Duration::from_secs(2));
        }
    }
}
