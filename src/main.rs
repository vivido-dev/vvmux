mod agent;
mod agent_drive;
mod alt_read;
mod automation;
mod bridge;
mod client;
mod client_input;
mod config;
mod config_watch;
#[cfg(feature = "server-capability")]
mod gateway;
mod integration;
mod ipc;
mod layout;
mod layout_file;
mod media;
mod media_trace;
mod metrics;
mod notify;
mod platform;
mod plugin;
mod plugin_component;
mod plugin_supervisor;
mod region;
mod runtime;
mod screen;
mod search;
mod server;
mod session;
mod session_state;
mod theme;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "vvmux",
    version,
    about = "A detachable Vivid-aware terminal multiplexer"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Print the release-matched vvmux automation skill.
    #[arg(long, global = true)]
    skill: bool,
    #[command(subcommand)]
    command: Option<Box<Command>>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new session.
    New {
        #[arg(short = 's', long = "session", default_value = "default")]
        session: String,
        #[arg(short = 'd', long)]
        detached: bool,
        /// Startup layout name from the config layouts directory, or a TOML path.
        #[arg(long)]
        layout: Option<String>,
    },
    /// Attach to an existing session by exact name.
    Attach {
        #[arg(short = 't', long = "target", default_value = "default")]
        target: String,
        #[arg(long)]
        replace: bool,
    },
    /// List live sessions owned by this user.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Check registry identity, IPC responsiveness, attachment, queues, and revisions.
    Doctor {
        #[arg(short = 't', long = "target", default_value = "default")]
        target: String,
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Write an atomic, privacy-preserving diagnostic ZIP.
    DebugBundle {
        #[arg(short = 't', long = "target", default_value = "default")]
        target: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        include_grid: bool,
        #[arg(long)]
        include_text: bool,
        #[arg(long)]
        include_logs: bool,
    },
    /// Terminate a session and all of its panes.
    KillSession {
        #[arg(short = 't', long = "target")]
        target: String,
    },
    /// Control and inspect individual panes in a running session.
    Msg {
        #[arg(short = 't', long = "target")]
        target: Option<String>,
        #[command(subcommand)]
        command: Box<automation::MsgCommand>,
    },
    /// Install, inspect, and invoke user plugins.
    Plugin {
        #[command(subcommand)]
        command: plugin::PluginCommand,
    },
    /// Manage optional AI-agent lifecycle integrations.
    Integration {
        #[command(subcommand)]
        command: integration::IntegrationCommand,
    },
    /// Run the authenticated loopback VVWS/1 session gateway, or connect mode
    /// (`--connect`) which opens no listener and serves through a VVTUN/1 tunnel.
    #[cfg(feature = "server-capability")]
    Serve {
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        #[arg(long = "allow-origin")]
        allow_origins: Vec<String>,
        #[arg(long)]
        auth_file: Option<PathBuf>,
        #[arg(long)]
        connect: Option<String>,
        /// Confirm that the public gateway can observe terminal and media content.
        #[arg(long)]
        acknowledge_content_visible_gateway: bool,
        #[arg(long = "allow-account")]
        allow_accounts: Vec<String>,
        #[arg(long)]
        allow_kill: bool,
        /// Machine tunnel carrier. Auto prefers WebTransport and falls back before authentication.
        #[arg(long, value_enum, default_value = "auto")]
        tunnel_carrier: gateway::tunnel::TunnelCarrier,
        /// Pin an ephemeral/self-hosted WebTransport certificate by SHA-256 (64 hex digits).
        #[arg(long = "tunnel-certificate-sha256", hide = true)]
        tunnel_certificate_sha256: Vec<String>,
        #[arg(long)]
        identity_file: Option<PathBuf>,
        #[arg(long, hide = true)]
        tunnel_heartbeat_ms: Option<u64>,
        #[arg(long, hide = true)]
        tunnel_miss_limit: Option<u32>,
        #[arg(long, hide = true)]
        tunnel_handshake_timeout_ms: Option<u64>,
    },
    /// Enroll this machine with a vvmux_server deployment.
    #[cfg(feature = "server-capability")]
    Cloud {
        #[command(subcommand)]
        command: CloudCommand,
    },
    /// Manage the network gateway authentication token.
    #[cfg(feature = "server-capability")]
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    #[command(name = "__server", hide = true)]
    Server {
        #[arg(long)]
        session: String,
        #[arg(long)]
        layout: Option<PathBuf>,
        #[arg(long, hide = true)]
        ready_handle: Option<usize>,
    },
    #[cfg(windows)]
    #[command(name = "__console-self-test", hide = true)]
    ConsoleSelfTest,
}

#[cfg(feature = "server-capability")]
#[derive(Debug, Subcommand)]
enum CloudCommand {
    /// Register this machine with a vvmux_server deployment.
    ///
    /// Generates an Ed25519 identity, submits the public key under a one-time
    /// code from the deployment's website, and stores the private key in an
    /// owner-only file. The private key never leaves the machine.
    Enroll {
        /// The deployment's bare scheme://host[:port] URL.
        #[arg(long)]
        server: String,
        /// Read the enrollment code from this file, or from stdin when PATH is `-`.
        /// Without this option, read it from a no-echo terminal prompt.
        #[arg(long, value_name = "PATH")]
        code_file: Option<PathBuf>,
        #[arg(long)]
        identity_file: Option<PathBuf>,
    },
}

#[cfg(feature = "server-capability")]
#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Create a 256-bit bearer token, printing the token exactly once.
    Create {
        #[arg(long)]
        rotate: bool,
        #[arg(long)]
        auth_file: Option<PathBuf>,
    },
}

#[cfg(not(windows))]
fn main() {
    main_entry();
}

// clap's generated parser visits the complete nested automation/plugin command tree. Windows gives
// the process main thread a much smaller stack than the worker-thread default, so keep that parser
// off the 1 MiB startup stack as the additive command surface grows.
#[cfg(windows)]
fn main() {
    let worker = std::thread::Builder::new()
        .name("vvmux-main".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(main_entry)
        .unwrap_or_else(|error| {
            eprintln!("vvmux: unable to start main worker: {error}");
            std::process::exit(1);
        });
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

fn main_entry() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("vvmux: {error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> io::Result<()> {
    if cli.skill {
        print!("{}", include_str!("../skills/vvmux/SKILL.md"));
        return Ok(());
    }
    match cli.command.map(|command| *command) {
        None => client::attach("default", false, true, cli.config.as_deref()),
        Some(Command::New {
            session,
            detached,
            layout,
        }) => {
            runtime::validate_session_name(&session)?;
            client::create_detached(&session, cli.config.as_deref(), layout.as_deref())?;
            if detached {
                println!("created vvmux session {session}");
                Ok(())
            } else {
                client::attach(&session, false, false, cli.config.as_deref())
            }
        }
        Some(Command::Attach { target, replace }) => {
            runtime::validate_session_name(&target)?;
            client::attach(&target, replace, false, cli.config.as_deref())
        }
        Some(Command::List { json }) => list_sessions(json),
        Some(Command::Doctor { target, json }) => doctor(&target, json),
        Some(Command::DebugBundle {
            target,
            output,
            include_grid,
            include_text,
            include_logs,
        }) => debug_bundle(&target, &output, include_grid, include_text, include_logs),
        Some(Command::KillSession { target }) => {
            runtime::validate_session_name(&target)?;
            client::kill(&target)
        }
        Some(Command::Msg { target, command }) => automation::run(target.as_deref(), *command),
        Some(Command::Plugin { command }) => plugin::run(command),
        Some(Command::Integration { command }) => integration::run(command),
        #[cfg(feature = "server-capability")]
        Some(Command::Serve {
            listen,
            allow_origins,
            auth_file,
            connect,
            acknowledge_content_visible_gateway,
            allow_accounts,
            allow_kill,
            tunnel_carrier,
            tunnel_certificate_sha256,
            identity_file,
            tunnel_heartbeat_ms,
            tunnel_miss_limit,
            tunnel_handshake_timeout_ms,
        }) => {
            let config = config::Config::load(cli.config.as_deref())?;
            if let Some(connect) = connect {
                if listen.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--connect and --listen are mutually exclusive",
                    ));
                }
                let identity_file =
                    identity_file.or(gateway::identity::default_identity_path().ok());
                let identity_file = identity_file.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "no identity file; run `vvmux cloud enroll` first",
                    )
                })?;
                gateway::tunnel::run_connect(
                    config,
                    cli.config,
                    gateway::tunnel::ConnectOptions {
                        url: connect,
                        identity_file,
                        acknowledge_content_visible_gateway,
                        allow_accounts,
                        allow_kill,
                        carrier: tunnel_carrier,
                        certificate_sha256: tunnel_certificate_sha256,
                        heartbeat: tunnel_heartbeat_ms.map(std::time::Duration::from_millis),
                        miss_limit: tunnel_miss_limit,
                        handshake_timeout: tunnel_handshake_timeout_ms
                            .map(std::time::Duration::from_millis),
                    },
                )
            } else {
                gateway::run(
                    config,
                    cli.config,
                    gateway::ServeOverrides {
                        listen,
                        allowed_origins: allow_origins,
                        auth_file,
                    },
                )
            }
        }
        #[cfg(feature = "server-capability")]
        Some(Command::Cloud { command }) => match command {
            CloudCommand::Enroll {
                server,
                code_file,
                identity_file,
            } => {
                let path = identity_file
                    .or(gateway::identity::default_identity_path().ok())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "no configuration directory")
                    })?;
                let reservation = gateway::identity::IdentityReservation::new(&path)?;
                let code = gateway::identity::read_enrollment_code(code_file.as_deref())?;
                let identity = gateway::identity::MachineIdentity::new_random()?;
                gateway::identity::enroll(&server, &code, &identity.public_key())?;
                reservation.store(&identity)?;
                println!("enrolled machine {} with {}", identity.machine_id(), server);
                println!(
                    "start the gateway with `vvmux serve --connect {server} --acknowledge-content-visible-gateway`"
                );
                Ok(())
            }
        },
        #[cfg(feature = "server-capability")]
        Some(Command::Token { command }) => match command {
            TokenCommand::Create { rotate, auth_file } => {
                let config = config::Config::load(cli.config.as_deref())?;
                let path = auth_file.as_deref().or(config.server.auth_file.as_deref());
                let token = gateway::auth::create_token(path, rotate)?;
                println!("{}", token.as_str());
                Ok(())
            }
        },
        Some(Command::Server {
            session,
            layout,
            ready_handle,
        }) => {
            runtime::validate_session_name(&session)?;
            server::run(session, cli.config, layout, ready_handle)
        }
        #[cfg(windows)]
        Some(Command::ConsoleSelfTest) => platform::console_restoration_self_test(),
    }
}

fn list_sessions(json_output: bool) -> io::Result<()> {
    let mut sessions = Vec::new();
    for registry in runtime::list_registries()? {
        match server::probe(&registry.name) {
            Ok(()) => sessions.push(serde_json::json!({
                "name": registry.name,
                "pid": registry.pid,
                "instance_nonce": registry.instance_nonce,
                "vvmx_version": registry.vvmx_version,
                "responsive": true,
            })),
            Err(error) if json_output => sessions.push(serde_json::json!({
                "name": registry.name,
                "pid": registry.pid,
                "instance_nonce": registry.instance_nonce,
                "vvmx_version": registry.vvmx_version,
                "responsive": false,
                "error": error.to_string(),
            })),
            Err(error) => eprintln!(
                "vvmux: ignoring unresponsive session {} (pid {}): {error}",
                registry.name, registry.pid
            ),
        }
    }
    if json_output {
        serde_json::to_writer(
            io::stdout().lock(),
            &serde_json::json!({"schema_version": 1, "sessions": sessions}),
        )
        .map_err(io::Error::other)?;
        println!();
    } else {
        for session in sessions {
            println!(
                "{}\tpid {}",
                session["name"].as_str().unwrap_or("?"),
                session["pid"]
            );
        }
    }
    Ok(())
}

fn doctor(target: &str, json_output: bool) -> io::Result<()> {
    runtime::validate_session_name(target)?;
    let registry = runtime::RuntimePaths::for_session(target)?.read_registry()?;
    let inspect =
        automation::request_json(target, ipc::AutomationMethod::SessionInspect, None, false)?;
    let attached = !inspect["attachment"].is_null();
    let pending_projections = inspect["pending"]["media_projections"]
        .as_u64()
        .unwrap_or(0);
    let report = serde_json::json!({
        "schema_version": 1,
        "status": if pending_projections == 0 { "ok" } else { "warning" },
        "target": target,
        "checks": {
            "registry_identity": "ok",
            "ipc_responsive": "ok",
            "attached_client": attached,
            "pending_media_projections": pending_projections,
        },
        "registry": {
            "pid": registry.pid,
            "instance_nonce": registry.instance_nonce,
            "vvmx_version": registry.vvmx_version,
        },
        "session": inspect,
    });
    if json_output {
        serde_json::to_writer(io::stdout().lock(), &report).map_err(io::Error::other)?;
        println!();
    }
    Ok(())
}

fn debug_bundle(
    target: &str,
    output: &std::path::Path,
    include_grid: bool,
    include_text: bool,
    include_logs: bool,
) -> io::Result<()> {
    let diagnose = automation::request_json(
        target,
        ipc::AutomationMethod::Diagnose {
            pane_id: None,
            all_panes: true,
            trace_limit: if include_logs { 512 } else { 128 },
        },
        None,
        false,
    )?;
    let manifest = serde_json::json!({
        "schema_version": 1,
        "product": "vvmux",
        "target": target,
        "metadata_only_default": true,
        "included": {"grid": include_grid, "text": include_text, "logs": include_logs},
        "privacy": {
            "process_arguments": false,
            "environment": false,
            "credentials": false,
            "media_payloads": false,
            "frame_hashes": false,
            // A bundle is assembled from named entries, never by scanning a directory, so persisted
            // session state is excluded by construction. Declared anyway: the snapshot records
            // working directories and resumable agent session identity, and the pane-history file
            // records whatever scrolled past, so their absence is a promise worth stating in the
            // artifact rather than only in the code that builds it.
            "session_snapshot": false,
            "pane_history": false,
        },
    });
    let mut entries = vec![
        ("manifest.json".to_owned(), json_pretty(&manifest)?),
        ("diagnose.json".to_owned(), json_pretty(&diagnose)?),
    ];
    if include_grid || include_text {
        for pane in diagnose["panes"].as_array().into_iter().flatten() {
            let Some(pane_id) = pane["pane"]["pane_id"].as_u64() else {
                continue;
            };
            if include_grid {
                let grid = automation::request_json(
                    target,
                    ipc::AutomationMethod::GetGrid {
                        start_line: None,
                        row_count: None,
                        since_screen: None,
                    },
                    Some(pane_id),
                    false,
                )?;
                entries.push((
                    format!("content/pane-{pane_id}-grid.json"),
                    json_pretty(&grid)?,
                ));
            }
            if include_text {
                let text = automation::request_json(
                    target,
                    ipc::AutomationMethod::GetText {
                        rows: None,
                        source: ipc::TextSource::Visible,
                    },
                    Some(pane_id),
                    false,
                )?;
                entries.push((
                    format!("content/pane-{pane_id}.txt"),
                    text.as_str().unwrap_or_default().as_bytes().to_vec(),
                ));
            }
        }
    }
    write_stored_zip(output, &entries)?;
    println!("{}", output.display());
    Ok(())
}

fn json_pretty(value: &serde_json::Value) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(value).map_err(io::Error::other)
}

fn write_stored_zip(path: &std::path::Path, entries: &[(String, Vec<u8>)]) -> io::Result<()> {
    #[cfg(unix)]
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("diagnostic bundle already exists: {}", path.display()),
        ));
    }
    let temporary = path.with_extension(format!("zip.{}.tmp", std::process::id()));
    #[cfg(unix)]
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    #[cfg(windows)]
    let mut file = platform::create_secure_windows_registry_file(&temporary)?;
    let result = (|| {
        let mut central = Vec::new();
        let mut offset = 0_u32;
        for (name, bytes) in entries {
            let name = name.as_bytes();
            let name_length = u16::try_from(name.len())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP name is too long"))?;
            let size = u32::try_from(bytes.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry exceeds 4 GiB")
            })?;
            let crc = crc32(bytes);
            let mut local = Vec::with_capacity(30 + name.len());
            local.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
            local.extend_from_slice(&20_u16.to_le_bytes());
            local.extend_from_slice(&[0; 8]);
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&size.to_le_bytes());
            local.extend_from_slice(&size.to_le_bytes());
            local.extend_from_slice(&name_length.to_le_bytes());
            local.extend_from_slice(&0_u16.to_le_bytes());
            local.extend_from_slice(name);
            file.write_all(&local)?;
            file.write_all(bytes)?;

            central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&name_length.to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
            offset = offset
                .checked_add(u32::try_from(local.len() + bytes.len()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "ZIP archive exceeds 4 GiB")
                })?)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "ZIP archive exceeds 4 GiB")
                })?;
        }
        file.write_all(&central)?;
        let central_size = u32::try_from(central.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP index is too large"))?;
        let count = u16::try_from(entries.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many ZIP entries"))?;
        file.write_all(&0x0605_4b50_u32.to_le_bytes())?;
        file.write_all(&[0; 4])?;
        file.write_all(&count.to_le_bytes())?;
        file.write_all(&count.to_le_bytes())?;
        file.write_all(&central_size.to_le_bytes())?;
        file.write_all(&offset.to_le_bytes())?;
        file.write_all(&0_u16.to_le_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pane_automation_commands_and_enforces_cli_bounds() {
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "msg",
                "--target",
                "agent",
                "get-grid",
                "--start-line",
                "-5",
                "--row-count",
                "5",
                "--pane-id",
                "7",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["vvmux", "msg", "key", "Enter", "--repeat", "1001",]).is_err()
        );
        assert!(Cli::try_parse_from(["vvmux", "msg", "get-grid", "--start-line", "0",]).is_err());
        assert!(
            Cli::try_parse_from(["vvmux", "msg", "wait", "screen-stable", "--quiet", "25h",])
                .is_err()
        );
        assert!(Cli::try_parse_from(["vvmux", "msg", "inspect-media", "--pane-id", "7"]).is_ok());
        assert!(Cli::try_parse_from(["vvmux", "msg", "sync-input", "--on"]).is_ok());
        assert!(Cli::try_parse_from(["vvmux", "msg", "sync-input", "--off"]).is_ok());
        assert!(Cli::try_parse_from(["vvmux", "msg", "sync-input"]).is_err());
        assert!(Cli::try_parse_from(["vvmux", "msg", "sync-input", "--on", "--off"]).is_err());
        assert!(
            Cli::try_parse_from(["vvmux", "msg", "action", "toggle-zoom", "--pane-id", "7",])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["vvmux", "--skill"]).is_ok());
        assert!(
            Cli::try_parse_from(["vvmux", "plugin", "catalog", "--target", "work", "--json",])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["vvmux", "plugin", "catalog", "--json"]).is_err());
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "plugin",
                "invoke",
                "dev.example/run",
                "--target",
                "work",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["vvmux", "plugin", "invoke", "dev.example/run"]).is_err());
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "plugin",
                "pane",
                "open",
                "dev.example/dashboard",
                "--target",
                "work",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["vvmux", "plugin", "pane", "open", "dev.example/dashboard",])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "msg",
                "report-agent",
                "--agent",
                "opencode",
                "--state",
                "working",
                "--source",
                "opencode-plugin",
                "--sequence",
                "42",
                "--pane-id",
                "7",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "msg",
                "report-agent",
                "--agent",
                "opencode",
                "--state",
                "done",
                "--source",
                "opencode-plugin",
                "--sequence",
                "42",
                "--pane-id",
                "7",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "msg",
                "clear-agent-report",
                "--source",
                "opencode-plugin",
                "--sequence",
                "43",
                "--pane-id",
                "7",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "msg",
                "trace-media",
                "--pane-id",
                "7",
                "--follow",
                "--producer-id",
                "3",
                "--context-id",
                "4",
                "--surface-id",
                "8",
                "--track-id",
                "9",
                "--category",
                "recovery",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["vvmux", "msg", "trace-media", "--source-id", "9",]).is_err());
        assert!(
            Cli::try_parse_from([
                "vvmux",
                "msg",
                "wait",
                "media",
                "--after-virtual",
                "4",
                "--after-outer",
                "9",
                "--pane-id",
                "7",
            ])
            .is_ok()
        );
    }
}
