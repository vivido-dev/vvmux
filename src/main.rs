mod bridge;
mod client;
mod config;
mod ipc;
mod layout;
mod media;
mod platform;
mod region;
mod runtime;
mod screen;
mod server;
mod session;
mod vivid_transport;

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
