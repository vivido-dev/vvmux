mod automation;
mod bridge;
mod client;
mod client_input;
mod config;
#[cfg(feature = "server-capability")]
mod gateway;
mod ipc;
mod layout;
mod media;
mod media_trace;
mod metrics;
mod platform;
mod region;
mod runtime;
mod screen;
mod server;
mod session;

use std::io;
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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new session.
    New {
        #[arg(short = 's', long = "session", default_value = "default")]
        session: String,
        #[arg(short = 'd', long)]
        detached: bool,
    },
    /// Attach to an existing session by exact name.
    Attach {
        #[arg(short = 't', long = "target", default_value = "default")]
        target: String,
        #[arg(long)]
        replace: bool,
    },
    /// List live sessions owned by this user.
    List,
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
        command: automation::MsgCommand,
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

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("vvmux: {error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> io::Result<()> {
    match cli.command {
        None => client::attach("default", false, true, cli.config.as_deref()),
        Some(Command::New { session, detached }) => {
            runtime::validate_session_name(&session)?;
            client::create_detached(&session, cli.config.as_deref())?;
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
        Some(Command::List) => {
            for session in runtime::list_registries()? {
                match server::probe(&session.name) {
                    Ok(()) => println!("{}\tpid {}", session.name, session.pid),
                    Err(error) => eprintln!(
                        "vvmux: ignoring unresponsive session {} (pid {}): {error}",
                        session.name, session.pid
                    ),
                }
            }
            Ok(())
        }
        Some(Command::KillSession { target }) => {
            runtime::validate_session_name(&target)?;
            client::kill(&target)
        }
        Some(Command::Msg { target, command }) => automation::run(target.as_deref(), command),
        #[cfg(feature = "server-capability")]
        Some(Command::Serve {
            listen,
            allow_origins,
            auth_file,
            connect,
            acknowledge_content_visible_gateway,
            allow_accounts,
            allow_kill,
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
            ready_handle,
        }) => {
            runtime::validate_session_name(&session)?;
            server::run(session, cli.config, ready_handle)
        }
        #[cfg(windows)]
        Some(Command::ConsoleSelfTest) => platform::console_restoration_self_test(),
    }
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
