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
use std::time::{Duration, SystemTime};

use base64::Engine;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{RETRY_AFTER, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, client_async_tls_with_config};

use super::identity::MachineIdentity;
use super::protocol::{MAX_FRAME_BYTES, VERSION as VVWS_VERSION};
use super::transport::{
    Frame, FrameSink, FrameStream, QuicMapping, TungsteniteSink, TungsteniteStream, TunnelStream,
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
const MAX_SEEN_LEG_IDS: usize = 4096;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);
const DEFAULT_MISS_LIMIT: u32 = 3;

/// Options for `vvmux serve --connect`.
pub(crate) struct ConnectOptions {
    pub url: String,
    pub identity_file: PathBuf,
    pub acknowledge_content_visible_gateway: bool,
    pub allow_accounts: Vec<String>,
    pub allow_kill: bool,
    pub carrier: TunnelCarrier,
    pub certificate_sha256: Vec<String>,
    /// Test and diagnostic knobs; the defaults are the VVTUN-1 constants.
    pub heartbeat: Option<Duration>,
    pub miss_limit: Option<u32>,
    pub handshake_timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum TunnelCarrier {
    Auto,
    Webtransport,
    Websocket,
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
    webtransport_url: Option<String>,
    carrier: TunnelCarrier,
    certificate_hashes: Vec<wtransport::tls::Sha256Digest>,
    identity: MachineIdentity,
    state: GatewayState,
    allow_accounts: HashSet<String>,
    allow_kill: bool,
    heartbeat: Duration,
    miss_limit: u32,
    handshake_timeout: Duration,
    legs: Arc<Semaphore>,
}

struct ConnectEndpoints {
    control_url: String,
    hostname: String,
    leg_url: String,
    webtransport_url: Option<String>,
    requires_content_acknowledgement: bool,
}

pub(crate) fn run_connect(
    config: crate::config::Config,
    config_path: Option<PathBuf>,
    options: ConnectOptions,
) -> io::Result<()> {
    let endpoints = split_urls(&options.url)?;
    if endpoints.requires_content_acknowledgement && !options.acknowledge_content_visible_gateway {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a public tunnel makes terminal and media content visible to the gateway; pass --acknowledge-content-visible-gateway to continue",
        ));
    }
    let state = super::build_state(config, config_path, HashSet::new(), None);
    let identity = MachineIdentity::load(&options.identity_file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not load identity {}: {error}; run `vvmux cloud enroll` first",
                options.identity_file.display()
            ),
        )
    })?;
    let certificate_hashes = options
        .certificate_sha256
        .iter()
        .map(|value| {
            let bytes = hex::decode(value).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid tunnel certificate hash",
                )
            })?;
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tunnel certificate SHA-256 must contain 64 hexadecimal digits",
                )
            })?;
            Ok(wtransport::tls::Sha256Digest::new(bytes))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let runner = TunnelRunner {
        url: endpoints.control_url,
        hostname: endpoints.hostname,
        leg_url: endpoints.leg_url,
        webtransport_url: endpoints.webtransport_url,
        carrier: options.carrier,
        certificate_hashes,
        identity,
        state,
        allow_accounts: options.allow_accounts.into_iter().collect(),
        allow_kill: options.allow_kill,
        heartbeat: options.heartbeat.unwrap_or(DEFAULT_HEARTBEAT),
        miss_limit: options.miss_limit.unwrap_or(DEFAULT_MISS_LIMIT),
        handshake_timeout: options.handshake_timeout.unwrap_or(HANDSHAKE_TIMEOUT),
        legs: Arc::new(Semaphore::new(MAX_TUNNEL_LEGS)),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("vvmux-tunnel")
        .build()?;
    runtime.block_on(run_reconnect_loop(&runner))
}

/// Validate a deployment base or an explicit WebSocket fallback URL and derive
/// the two W3 WebSocket endpoints. HTTPS is the canonical public form.
fn split_urls(url: &str) -> io::Result<ConnectEndpoints> {
    let parsed = url::Url::parse(url).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid --connect URL: {error}"),
        )
    })?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--connect must not contain credentials, a query, or a fragment",
        ));
    }
    let (scheme, tls, base_only, webtransport) = match parsed.scheme() {
        "https" => ("wss", true, true, true),
        "http" => ("ws", false, true, false),
        "wss" => ("wss", true, false, false),
        "ws" => ("ws", false, false, false),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--connect scheme must be https, wss, or loopback http/ws, got {other}"),
            ));
        }
    };
    let path = parsed.path();
    let valid_path = if base_only {
        path.is_empty() || path == "/"
    } else {
        path.is_empty() || path == "/" || path == "/t/v1/control"
    };
    if !valid_path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--connect must be a deployment base or the exact /t/v1/control WebSocket endpoint",
        ));
    }
    let Some(host) = parsed.host_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--connect URL has no host",
        ));
    };
    let loopback = is_loopback_host(host);
    if !tls && !loopback {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plaintext http/ws is accepted for loopback development only; use https:// across a host boundary",
        ));
    }
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--connect URL has no port"))?;
    let authority = match parsed.host() {
        Some(url::Host::Ipv6(_)) if parsed.port().is_some() => format!("[{host}]:{port}"),
        Some(url::Host::Ipv6(_)) => format!("[{host}]"),
        Some(_) if parsed.port().is_some() => format!("{host}:{port}"),
        Some(_) => host.to_owned(),
        None => unreachable!("host_str was checked above"),
    };
    Ok(ConnectEndpoints {
        control_url: format!("{scheme}://{authority}/t/v1/control"),
        hostname: host.to_owned(),
        leg_url: format!("{scheme}://{authority}/t/v1/leg"),
        webtransport_url: webtransport.then(|| format!("https://{authority}/t/v1/webtransport")),
        requires_content_acknowledgement: !loopback,
    })
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn full_jitter(cap: Duration) -> Duration {
    let cap_nanos = cap.as_nanos().min(u64::MAX as u128) as u64;
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("random backoff generation");
    let value = u64::from_ne_bytes(bytes);
    Duration::from_nanos(value % cap_nanos.saturating_add(1))
}

#[derive(Debug)]
struct TunnelAttemptError {
    error: io::Error,
    retry_after_seconds: Option<u64>,
    fallback_allowed: bool,
}

impl From<io::Error> for TunnelAttemptError {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            retry_after_seconds: None,
            fallback_allowed: true,
        }
    }
}

struct ConnectedTunnel {
    socket: TunnelStream,
    exporter: Option<[u8; 32]>,
}

async fn run_reconnect_loop(runner: &TunnelRunner) -> io::Result<()> {
    let mut backoff_cap = RECONNECT_MIN;
    loop {
        let (completed, retry_after) = match run_tunnel_once(runner).await {
            Ok(outcome) => outcome,
            Err(failure) => {
                eprintln!("vvmux tunnel: {}", failure.error);
                (false, failure.retry_after_seconds)
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

async fn run_tunnel_once(runner: &TunnelRunner) -> Result<(bool, Option<u64>), TunnelAttemptError> {
    match runner.carrier {
        TunnelCarrier::Websocket => run_websocket_tunnel_once(runner).await,
        TunnelCarrier::Webtransport => {
            let url = runner.webtransport_url.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "WebTransport requires an https deployment base",
                )
            })?;
            run_webtransport_tunnel_once(runner, url).await
        }
        TunnelCarrier::Auto => {
            if let Some(url) = runner.webtransport_url.as_deref() {
                match run_webtransport_tunnel_once(runner, url).await {
                    Ok(outcome) => Ok(outcome),
                    Err(error) if error.fallback_allowed => {
                        eprintln!(
                            "vvmux tunnel: WebTransport unavailable before authentication: {}; using WebSocket fallback",
                            error.error
                        );
                        run_websocket_tunnel_once(runner).await
                    }
                    Err(error) => Err(error),
                }
            } else {
                run_websocket_tunnel_once(runner).await
            }
        }
    }
}

/// One tunnel lifetime. Returns `(completed, retry_after_hint)`.
async fn run_websocket_tunnel_once(
    runner: &TunnelRunner,
) -> Result<(bool, Option<u64>), TunnelAttemptError> {
    let connected = connect_with_exporter(
        &runner.url,
        &[TUNNEL_SUBPROTOCOL.to_owned()],
        CONTROL_MAX_BYTES,
    )
    .await?;
    let ConnectedTunnel {
        socket: stream,
        exporter,
    } = connected;
    let exporter = exporter
        .as_ref()
        .map(|bytes| bytes.as_slice())
        .unwrap_or_default();

    let (mut sink, mut reader) = {
        let (sink, stream) = stream.split();
        (TungsteniteSink(sink), TungsteniteStream(stream))
    };

    // Handshake: challenge -> auth -> authed -> machine_status.
    let challenge = tokio::time::timeout(runner.handshake_timeout, read_control_frame(&mut reader))
        .await
        .map_err(|_| io::Error::other("initial challenge timed out"))??
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed before challenge",
            )
        })?;
    let ServerControl::Challenge {
        protocol, nonce, ..
    } = challenge
    else {
        return Err(io::Error::other("expected VVTUN challenge").into());
    };
    if protocol != 1 {
        return Err(io::Error::other("unsupported VVTUN protocol version").into());
    }
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(nonce.as_str())
        .map_err(|_| io::Error::other("challenge nonce is not base64url"))?;
    if nonce.len() != 32 {
        return Err(io::Error::other("challenge nonce must be 32 bytes").into());
    }
    let signature = runner
        .identity
        .sign_handshake(&nonce, &runner.hostname, exporter);
    send_control(
        &mut sink,
        &ClientControl::Auth {
            machine_id: runner.identity.machine_id(),
            signature,
        },
    )
    .await?;

    let authed = tokio::time::timeout(runner.handshake_timeout, read_control_frame(&mut reader))
        .await
        .map_err(|_| io::Error::other("authentication timed out"))??
        .ok_or_else(|| io::Error::other("server closed during authentication"))?;
    let ServerControl::Authed {
        protocol,
        server_version,
        reconnect_after_seconds: _,
    } = authed
    else {
        return Err(io::Error::other("expected VVTUN authed").into());
    };
    if protocol != 1 {
        return Err(io::Error::other("unsupported VVTUN protocol version").into());
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
    let mut seen_leg_ids = HashSet::new();
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
                            &mut seen_leg_ids,
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

struct WebTransportLegOffer {
    kind: LegKind,
    ticket: [u8; 32],
    subprotocols: Vec<String>,
}

struct AcceptedWebTransportLeg {
    send: wtransport::SendStream,
    recv: wtransport::RecvStream,
    generation: u64,
    leg_id: u64,
    kind: LegKind,
    ticket: [u8; 32],
}

async fn run_webtransport_tunnel_once(
    runner: &TunnelRunner,
    url: &str,
) -> Result<(bool, Option<u64>), TunnelAttemptError> {
    let client_config = if runner.certificate_hashes.is_empty() {
        wtransport::ClientConfig::default()
    } else {
        wtransport::ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes(runner.certificate_hashes.clone())
            .build()
    };
    let endpoint = wtransport::Endpoint::client(client_config).map_err(io::Error::other)?;
    let connect_options = wtransport::endpoint::ConnectOptions::builder(url)
        .add_header("Sec-WebTransport-Protocol", TUNNEL_SUBPROTOCOL)
        .build();
    let connection = Arc::new(
        endpoint
            .connect(connect_options)
            .await
            .map_err(|error| io::Error::other(format!("WebTransport connect failed: {error}")))?,
    );
    let mut exporter = [0_u8; 32];
    connection
        .export_keying_material(&mut exporter, EXPORTER_LABEL, &[])
        .map_err(io::Error::other)?;
    let (mut control_send, mut control_recv) =
        tokio::time::timeout(runner.handshake_timeout, connection.accept_bi())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control stream timed out"))?
            .map_err(io::Error::other)?;
    control_send.set_priority(120);

    let challenge = tokio::time::timeout(
        runner.handshake_timeout,
        read_webtransport_control::<ServerControl>(&mut control_recv),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "initial challenge timed out"))??;
    let ServerControl::Challenge {
        protocol, nonce, ..
    } = challenge
    else {
        return Err(io::Error::other("expected VVTUN challenge").into());
    };
    if protocol != 1 {
        return Err(io::Error::other("unsupported VVTUN protocol version").into());
    }
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(nonce)
        .map_err(|_| io::Error::other("challenge nonce is not base64url"))?;
    if nonce.len() != 32 {
        return Err(io::Error::other("challenge nonce must be 32 bytes").into());
    }
    let signature = runner
        .identity
        .sign_handshake(&nonce, &runner.hostname, &exporter);
    write_webtransport_control(
        &mut control_send,
        &ClientControl::Auth {
            machine_id: runner.identity.machine_id(),
            signature,
        },
    )
    .await?;
    let authed = tokio::time::timeout(
        runner.handshake_timeout,
        read_webtransport_control::<ServerControl>(&mut control_recv),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "authentication timed out"))??;
    let ServerControl::Authed {
        protocol,
        server_version,
        ..
    } = authed
    else {
        return Err(io::Error::other("expected VVTUN authed").into());
    };
    if protocol != 1 {
        return Err(io::Error::other("unsupported VVTUN protocol version").into());
    }
    let server_version = bounded_ascii(&server_version, 64)?;
    let authenticated = async {
        let mut capabilities = vec![
            "terminal-v1",
            "session-list-v1",
            "session-create-v1",
            "vivid-bridge-v1",
            "tunnel-attached-v1",
            "webtransport-streams-v1",
            "stream-priority-v1",
        ];
        if runner.allow_kill {
            capabilities.push("session-kill-v1");
        }
        write_webtransport_control(
            &mut control_send,
            &ClientControl::MachineStatus {
                vvmux_version: env!("CARGO_PKG_VERSION").to_owned(),
                vvws_protocol: VVWS_VERSION,
                capabilities: capabilities.into_iter().map(str::to_owned).collect(),
            },
        )
        .await?;
        println!("vvmux tunnel connected to {url} over WebTransport (server {server_version})");

        // A dedicated reader preserves length-prefixed control sequencing. Polling
        // `read_exact` directly in the select below would cancel it when a leg arrived.
        let (control_frames, mut control_frame_receiver) = mpsc::channel(128);
        let control_reader = tokio::spawn(async move {
            loop {
                let frame = read_webtransport_control::<ServerControl>(&mut control_recv).await;
                let failed = frame.is_err();
                if control_frames.send(frame).await.is_err() || failed {
                    break;
                }
            }
        });
        let (reports, mut report_receiver) = mpsc::unbounded_channel::<ClientControl>();
        let mut offers = std::collections::HashMap::<u64, WebTransportLegOffer>::new();
        let mut streams = std::collections::HashMap::<u64, AcceptedWebTransportLeg>::new();
        let mut legs = std::collections::HashMap::<u64, tokio::task::JoinHandle<()>>::new();
        let mut seen = HashSet::new();
        let mut tunnel_generation: Option<u64> = None;
        let mut heartbeat = tokio::time::interval(runner.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ping_nonce = 0_u64;
        let mut missed = 0_u32;
        let mut outcome = (true, None);

        loop {
            tokio::select! {
                incoming = control_frame_receiver.recv() => {
                    missed = 0;
                    match incoming {
                        Some(Ok(ServerControl::Ping { nonce })) => write_webtransport_control(&mut control_send, &ClientControl::Pong { nonce }).await?,
                        Some(Ok(ServerControl::Pong { .. })) => {},
                        Some(Ok(ServerControl::OpenLeg { leg_id, kind, account, ticket, subprotocols, .. })) => {
                            let offer = validate_webtransport_offer(runner, &mut seen, leg_id, &kind, &account, &ticket, subprotocols, &reports)?;
                            if let Some(offer) = offer {
                                offers.insert(leg_id, offer);
                                if let Some(stream) = streams.remove(&leg_id) {
                                    start_webtransport_leg(runner, &mut legs, &reports, stream, offers.remove(&leg_id).expect("offer inserted")).await;
                                }
                            }
                        }
                        Some(Ok(ServerControl::CloseLeg { leg_id, .. })) => {
                            offers.remove(&leg_id);
                            streams.remove(&leg_id);
                            if let Some(handle) = legs.remove(&leg_id) { handle.abort(); }
                        }
                        Some(Ok(ServerControl::GoingAway { reconnect_after_seconds, .. })) => {
                            outcome = (true, Some(reconnect_after_seconds));
                            break;
                        }
                        Some(Ok(ServerControl::Challenge { .. } | ServerControl::Authed { .. })) => {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "out-of-sequence control frame"));
                        }
                        Some(Err(error)) => return Err(error),
                        None => break,
                    }
                }
                incoming = connection.accept_bi() => {
                    if streams.len() >= MAX_TUNNEL_LEGS {
                        return Err(io::Error::new(io::ErrorKind::OutOfMemory, "too many unpaired VVTUN streams"));
                    }
                    let (send, recv) = incoming.map_err(io::Error::other)?;
                    let accepted = tokio::time::timeout(runner.handshake_timeout, read_leg_preface(send, recv))
                        .await
                        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "leg preface timed out"))??;
                    if tunnel_generation.is_some_and(|generation| generation != accepted.generation) {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "mixed VVTUN generation"));
                    }
                    tunnel_generation.get_or_insert(accepted.generation);
                    let leg_id = accepted.leg_id;
                    if streams.insert(leg_id, accepted).is_some() {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate leg stream"));
                    }
                    if let Some(offer) = offers.remove(&leg_id) {
                        let stream = streams.remove(&leg_id).expect("stream inserted");
                        start_webtransport_leg(runner, &mut legs, &reports, stream, offer).await;
                    }
                }
                report = report_receiver.recv() => match report {
                    Some(report) => write_webtransport_control(&mut control_send, &report).await?,
                    None => break,
                },
                _ = heartbeat.tick() => {
                    ping_nonce = ping_nonce.wrapping_add(1);
                    missed = missed.saturating_add(1);
                    if missed > runner.miss_limit {
                        break;
                    }
                    write_webtransport_control(&mut control_send, &ClientControl::Ping { nonce: ping_nonce }).await?;
                }
                _ = connection.closed() => break,
            }
        }
        for handle in legs.into_values() {
            handle.abort();
        }
        control_reader.abort();
        connection.close(wtransport::VarInt::from_u32(0), b"reconnect");
        drop(endpoint);
        Ok(outcome)
    };
    authenticated.await.map_err(|error| TunnelAttemptError {
        error,
        retry_after_seconds: None,
        fallback_allowed: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_webtransport_offer(
    runner: &TunnelRunner,
    seen: &mut HashSet<u64>,
    leg_id: u64,
    kind: &str,
    account: &str,
    ticket: &str,
    subprotocols: Vec<String>,
    reports: &mpsc::UnboundedSender<ClientControl>,
) -> io::Result<Option<WebTransportLegOffer>> {
    let reject = |code: &str| {
        let _ = reports.send(ClientControl::LegFailed {
            leg_id,
            code: code.to_owned(),
        });
        Ok(None)
    };
    if seen.contains(&leg_id) {
        return reject("invalid_request");
    }
    if seen.len() >= MAX_SEEN_LEG_IDS {
        return reject("capacity");
    }
    seen.insert(leg_id);
    if !runner.allow_accounts.is_empty() && !runner.allow_accounts.contains(account) {
        return reject("not_permitted");
    }
    let kind = match kind {
        "vvws" => LegKind::Vvws,
        "vivid" => LegKind::Vivid,
        _ => return reject("invalid_request"),
    };
    let Ok(ticket) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(ticket) else {
        return reject("ticket_rejected");
    };
    let Ok(ticket) = <[u8; 32]>::try_from(ticket) else {
        return reject("ticket_rejected");
    };
    if subprotocols.len() > 8
        || subprotocols
            .iter()
            .any(|value| value.is_empty() || value.len() > 256)
    {
        return reject("invalid_subprotocols");
    }
    Ok(Some(WebTransportLegOffer {
        kind,
        ticket,
        subprotocols,
    }))
}

async fn start_webtransport_leg(
    runner: &TunnelRunner,
    legs: &mut std::collections::HashMap<u64, tokio::task::JoinHandle<()>>,
    reports: &mpsc::UnboundedSender<ClientControl>,
    stream: AcceptedWebTransportLeg,
    offer: WebTransportLegOffer,
) {
    if stream.kind != offer.kind || stream.ticket != offer.ticket || legs.len() >= MAX_TUNNEL_LEGS {
        let _ = reports.send(ClientControl::LegFailed {
            leg_id: stream.leg_id,
            code: "ticket_rejected".to_owned(),
        });
        return;
    }
    let permit = match runner.legs.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            let _ = reports.send(ClientControl::LegFailed {
                leg_id: stream.leg_id,
                code: "capacity".to_owned(),
            });
            return;
        }
    };
    let state = runner.state.clone();
    let allow_kill = runner.allow_kill;
    let reports = reports.clone();
    let leg_id = stream.leg_id;
    let handle = tokio::spawn(async move {
        let _permit = permit;
        let result = run_webtransport_leg(stream, offer, &state, allow_kill).await;
        if let Err(error) = result {
            let _ = reports.send(ClientControl::LegFailed {
                leg_id,
                code: leg_failure_code(&error),
            });
        }
    });
    legs.insert(leg_id, handle);
}

async fn run_webtransport_leg(
    stream: AcceptedWebTransportLeg,
    offer: WebTransportLegOffer,
    state: &GatewayState,
    allow_kill: bool,
) -> io::Result<()> {
    match offer.kind {
        LegKind::Vvws => {
            let (sink, reader) =
                super::transport::quic(stream.send, stream.recv, QuicMapping::Framed);
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
            let (broker, kind) = super::resolve_vivid_leg(&offer.subprotocols, state)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
            let (sink, reader) =
                super::transport::quic(stream.send, stream.recv, QuicMapping::RawBinary);
            vivid::serve_socket(sink, reader, broker, kind).await
        }
    }
}

async fn read_leg_preface(
    send: wtransport::SendStream,
    mut recv: wtransport::RecvStream,
) -> io::Result<AcceptedWebTransportLeg> {
    let mut bytes = [0_u8; 64];
    recv.read_exact(&mut bytes)
        .await
        .map_err(io::Error::other)?;
    if &bytes[..8] != b"VVTLEG1\0" || bytes[29..32] != [0, 0, 0] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid VVTUN leg preface",
        ));
    }
    let generation = u64::from_be_bytes(bytes[8..16].try_into().expect("generation"));
    let leg_id = u64::from_be_bytes(bytes[16..24].try_into().expect("leg id"));
    let kind = match bytes[24] {
        1 => LegKind::Vvws,
        2 => LegKind::Vivid,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown leg kind",
            ));
        }
    };
    let priority = i32::from_be_bytes(bytes[25..29].try_into().expect("priority"));
    if !matches!(priority, 0 | 60 | 80 | 100) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid VVTUN stream priority",
        ));
    }
    send.set_priority(priority);
    let ticket = bytes[32..64].try_into().expect("ticket");
    Ok(AcceptedWebTransportLeg {
        send,
        recv,
        generation,
        leg_id,
        kind,
        ticket,
    })
}

async fn write_webtransport_control<T: Serialize>(
    send: &mut wtransport::SendStream,
    value: &T,
) -> io::Result<()> {
    let encoded = serde_json::to_vec(value).map_err(io::Error::other)?;
    if encoded.len() > CONTROL_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VVTUN control object too large",
        ));
    }
    let length = u32::try_from(encoded.len()).map_err(io::Error::other)?;
    send.write_all(&length.to_be_bytes())
        .await
        .map_err(io::Error::other)?;
    send.write_all(&encoded).await.map_err(io::Error::other)
}

async fn read_webtransport_control<T: serde::de::DeserializeOwned>(
    recv: &mut wtransport::RecvStream,
) -> io::Result<T> {
    let mut length = [0_u8; 4];
    recv.read_exact(&mut length)
        .await
        .map_err(io::Error::other)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > CONTROL_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VVTUN control object too large",
        ));
    }
    let mut encoded = vec![0_u8; length];
    recv.read_exact(&mut encoded)
        .await
        .map_err(io::Error::other)?;
    serde_json::from_slice(&encoded)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Returns `Some(reconnect_after)` when the connection should end cleanly.
async fn handle_server_frame<Si: FrameSink>(
    sink: &mut Si,
    legs: &mut std::collections::HashMap<u64, tokio::task::JoinHandle<()>>,
    seen_leg_ids: &mut HashSet<u64>,
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
                    seen_leg_ids,
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
    seen_leg_ids: &mut HashSet<u64>,
    leg_reports: &mpsc::UnboundedSender<ClientControl>,
    runner: &TunnelRunner,
    leg_id: u64,
    kind: &str,
    _route: &str,
    account: &str,
    ticket: &str,
    subprotocols: &[String],
) -> io::Result<()> {
    legs.retain(|_, handle| !handle.is_finished());
    if seen_leg_ids.contains(&leg_id) {
        let _ = leg_reports.send(ClientControl::LegFailed {
            leg_id,
            code: "invalid_request".to_owned(),
        });
        return Ok(());
    }
    if seen_leg_ids.len() >= MAX_SEEN_LEG_IDS {
        let _ = leg_reports.send(ClientControl::LegFailed {
            leg_id,
            code: "capacity".to_owned(),
        });
        return Ok(());
    }
    seen_leg_ids.insert(leg_id);
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
    if legs.len() >= MAX_TUNNEL_LEGS {
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

    let permit = match runner.legs.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            let _ = leg_reports.send(ClientControl::LegFailed {
                leg_id,
                code: "capacity".to_owned(),
            });
            return Ok(());
        }
    };
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let connected = connect_with_exporter(leg_url, &offered, max_bytes)
        .await
        .map_err(|failure| failure.error)?;
    let stream = connected.socket;
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
) -> Result<ConnectedTunnel, TunnelAttemptError> {
    connect_with_exporter_config(url, offered, max_bytes, None).await
}

async fn connect_with_exporter_config(
    url: &str,
    offered: &[String],
    max_bytes: usize,
    tls_config: Option<Arc<rustls::ClientConfig>>,
) -> Result<ConnectedTunnel, TunnelAttemptError> {
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
    let tls = match parsed.scheme() {
        "wss" => true,
        "ws" => false,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("WebSocket URL scheme must be wss or ws, got {other}"),
            )
            .into());
        }
    };
    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(io::Error::other)?;
    let connector = if tls {
        let config = match tls_config {
            Some(config) => config,
            None => {
                let mut roots = rustls::RootCertStore::empty();
                for cert in rustls_native_certs::load_native_certs().certs {
                    roots.add(cert).map_err(io::Error::other)?;
                }
                Arc::new(
                    rustls::ClientConfig::builder_with_provider(Arc::new(
                        rustls::crypto::ring::default_provider(),
                    ))
                    .with_safe_default_protocol_versions()
                    .map_err(io::Error::other)?
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
                )
            }
        };
        Some(Connector::Rustls(config))
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
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    let retry_after_seconds = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|value| parse_retry_after(value, SystemTime::now()));
                    TunnelAttemptError {
                        error: io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "WebSocket upgrade was rejected with HTTP {}",
                                response.status()
                            ),
                        ),
                        retry_after_seconds,
                        fallback_allowed: true,
                    }
                }
                other => io::Error::other(other).into(),
            })?;
    let exporter = session_exporter(&socket, tls)?;
    Ok(ConnectedTunnel { socket, exporter })
}

fn session_exporter(socket: &TunnelStream, tls_required: bool) -> io::Result<Option<[u8; 32]>> {
    let exporter = match socket.get_ref() {
        MaybeTlsStream::Rustls(tls) => {
            let mut out = [0_u8; 32];
            tls.get_ref()
                .1
                .export_keying_material(&mut out, EXPORTER_LABEL, None)
                .map_err(|_| io::Error::other("TLS exporter derivation failed"))?;
            Some(out)
        }
        _ => None,
    };
    require_exporter(tls_required, exporter)
}

fn require_exporter(
    tls_required: bool,
    exporter: Option<[u8; 32]>,
) -> io::Result<Option<[u8; 32]>> {
    if tls_required && exporter.is_none() {
        return Err(io::Error::other(
            "TLS tunnel did not provide exporter binding material",
        ));
    }
    Ok(exporter)
}

fn parse_retry_after(value: &HeaderValue, now: SystemTime) -> Option<u64> {
    let value = value.to_str().ok()?.trim();
    let seconds = match value.parse::<u64>() {
        Ok(seconds) => seconds,
        Err(_) => {
            let deadline = httpdate::parse_http_date(value).ok()?;
            deadline
                .duration_since(now)
                .unwrap_or(Duration::ZERO)
                .as_secs()
        }
    };
    Some(seconds.min(RECONNECT_MAX.as_secs()))
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

    #[allow(clippy::result_large_err)]
    fn select_test_subprotocol(
        _request: &tokio_tungstenite::tungstenite::handshake::server::Request,
        mut response: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        response.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(TUNNEL_SUBPROTOCOL),
        );
        Ok(response)
    }

    #[test]
    fn ws_scheme_is_rejected_for_non_loopback_hosts() {
        assert!(split_urls("ws://vvmux.example/t/v1/control").is_err());
        assert!(split_urls("wss://vvmux.example/t/v1/control").is_ok());
        assert!(split_urls("ws://127.0.0.1:8000/t/v1/control").is_ok());
        assert!(split_urls("http://127.0.0.1:8000").is_ok());
    }

    #[test]
    fn https_base_is_canonical_and_urls_are_strict() {
        let endpoints = split_urls("https://vvmux.example:8443").unwrap();
        assert_eq!(
            endpoints.control_url,
            "wss://vvmux.example:8443/t/v1/control"
        );
        assert_eq!(endpoints.leg_url, "wss://vvmux.example:8443/t/v1/leg");
        assert_eq!(
            endpoints.webtransport_url.as_deref(),
            Some("https://vvmux.example:8443/t/v1/webtransport")
        );
        assert_eq!(endpoints.hostname, "vvmux.example");
        assert!(endpoints.requires_content_acknowledgement);

        assert!(
            split_urls("wss://vvmux.example/t/v1/control")
                .unwrap()
                .webtransport_url
                .is_none(),
            "an explicit WebSocket endpoint must force the fallback mapping"
        );

        for bad in [
            "https://user@vvmux.example",
            "https://vvmux.example?route=secret",
            "https://vvmux.example/#fragment",
            "https://vvmux.example/unexpected",
            "wss://vvmux.example/t/v1/leg",
        ] {
            assert!(split_urls(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn tls_exporter_is_required_and_plaintext_uses_an_empty_binding() {
        assert!(require_exporter(true, None).is_err());
        assert_eq!(require_exporter(false, None).unwrap(), None);
        assert_eq!(
            require_exporter(true, Some([7; 32])).unwrap(),
            Some([7; 32])
        );
    }

    #[test]
    fn retry_after_supports_delta_and_date_and_is_capped() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            parse_retry_after(&HeaderValue::from_static("17"), now),
            Some(17)
        );
        assert_eq!(
            parse_retry_after(&HeaderValue::from_static("999999"), now),
            Some(RECONNECT_MAX.as_secs())
        );
        let deadline = httpdate::fmt_http_date(now + Duration::from_secs(23));
        assert_eq!(
            parse_retry_after(&HeaderValue::from_str(&deadline).unwrap(), now),
            Some(23)
        );
        assert_eq!(
            parse_retry_after(&HeaderValue::from_static("invalid"), now),
            None
        );
    }

    #[tokio::test]
    async fn real_rustls_session_derives_the_same_exporter_on_both_ends() {
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let certificate = cert.der().clone();
        let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der());
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key.into())
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (exporter_sender, exporter_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await.unwrap();
            let mut exporter = [0_u8; 32];
            tls.get_ref()
                .1
                .export_keying_material(&mut exporter, EXPORTER_LABEL, None)
                .unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(tls, select_test_subprotocol)
                .await
                .unwrap();
            let _ = exporter_sender.send(exporter);
            let _ = socket.next().await;
        });

        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let client_config = Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
        );
        let connected = connect_with_exporter_config(
            &format!("wss://127.0.0.1:{port}/t/v1/control"),
            &[TUNNEL_SUBPROTOCOL.to_owned()],
            CONTROL_MAX_BYTES,
            Some(client_config),
        )
        .await
        .unwrap();
        let client_exporter = connected.exporter.expect("TLS exporter missing");
        let server_exporter = exporter_receiver.await.unwrap();
        assert_eq!(client_exporter, server_exporter);
        assert_ne!(client_exporter, [0; 32]);
        drop(connected);
        server.await.unwrap();
    }

    #[test]
    fn full_jitter_stays_within_the_cap() {
        for _ in 0..64 {
            let delay = full_jitter(Duration::from_secs(2));
            assert!(delay <= Duration::from_secs(2));
        }
    }
}
