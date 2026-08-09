use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::ipc::{self, ChannelKind, ClientMessage, SharedWriter};
use crate::platform::{self, SessionListener, Transport};
use crate::runtime::{RuntimePaths, registry_process_matches};
use crate::session::{self, ActorEvent, ActorHandle};

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_CLIENTS: AtomicUsize = AtomicUsize::new(0);
const MAX_CLIENTS: usize = 32;

struct ClientSlot;

impl ClientSlot {
    fn acquire() -> Option<Self> {
        ACTIVE_CLIENTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CLIENTS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        ACTIVE_CLIENTS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn run(
    name: String,
    config_path: Option<PathBuf>,
    ready_handle: Option<usize>,
) -> io::Result<()> {
    platform::prepare_server_process();
    let mut readiness = platform::ReadinessWriter::from_metadata(ready_handle)?;
    match run_inner(name, config_path, &mut readiness) {
        Ok(()) => Ok(()),
        Err(error) => {
            readiness.failure(&error);
            Err(error)
        }
    }
}

fn run_inner(
    name: String,
    config_path: Option<PathBuf>,
    readiness: &mut platform::ReadinessWriter,
) -> io::Result<()> {
    let paths =
        RuntimePaths::for_session(&name).map_err(|error| context("runtime paths", error))?;
    clean_stale(&paths).map_err(|error| context("stale-session check", error))?;
    let listener = SessionListener::bind(&paths.socket)
        .map_err(|error| context("bind session endpoint", error))?;

    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)
        .map_err(|error| io::Error::other(format!("could not create session nonce: {error}")))?;
    let registry = paths
        .write_registry(&name, &nonce)
        .map_err(|error| context("write session registry", error))?;
    let (config, resolved_config_path) =
        Config::load_with_path(config_path.as_deref()).map_err(|error| {
            paths.remove_instance(&registry);
            context("load session config", error)
        })?;
    let actor = session::start(session::SessionOptions {
        name,
        config,
        config_path: resolved_config_path,
        vivid_endpoint: paths.vivid_socket.clone(),
    })
    .map_err(|error| {
        paths.remove_instance(&registry);
        context("start session actor", error)
    })?;
    install_signal_forwarder(actor.clone())
        .map_err(|error| context("install signal handler", error))?;
    readiness.success()?;

    while !actor.shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok(stream) => {
                let actor = actor.clone();
                thread::Builder::new()
                    .name("vvmux-client-ipc".into())
                    .spawn(move || handle_client(stream, actor))?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                paths.remove_instance(&registry);
                return Err(error);
            }
        }
    }
    paths.remove_instance(&registry);
    Ok(())
}

fn context(operation: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

pub fn connect(name: &str) -> io::Result<(ipc::RecordReader, SharedWriter)> {
    let paths = RuntimePaths::for_session(name)?;
    let stream = platform::connect_session(&paths.socket)?;
    ipc::establish(stream, ChannelKind::Control)
        .map_err(|error| describe_peer_version(&paths, error))
}

/// Name the server behind a VVMX version mismatch so the required restart is unambiguous.
///
/// A session server outlives the binary that spawned it, so rebuilding across a version bump
/// leaves the previous daemon holding the socket. Reporting its PID and version turns "restart the
/// session server" into an instruction the reader can act on without hunting for the process.
fn describe_peer_version(paths: &RuntimePaths, error: io::Error) -> io::Error {
    if error.kind() != io::ErrorKind::InvalidData || error.to_string() != ipc::VERSION_MISMATCH {
        return error;
    }
    let detail = match paths.foreign_registry_identity() {
        Some((pid, Some(version))) => {
            format!("; session server PID {pid} speaks VVMX v{version}")
        }
        Some((pid, None)) => format!("; session server PID {pid} predates VVMX version recording"),
        None => String::new(),
    };
    io::Error::new(
        error.kind(),
        format!(
            "{}{detail}, this build speaks VVMX v{}",
            ipc::VERSION_MISMATCH,
            ipc::VERSION
        ),
    )
}

pub fn probe(name: &str) -> io::Result<()> {
    let (mut reader, writer) = connect(name)?;
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(&ClientMessage::Ping)?;
    match reader.recv()? {
        crate::ipc::ServerMessage::Pong => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected session probe reply",
        )),
    }
}

fn handle_client(stream: Transport, actor: ActorHandle) {
    let Some(_slot) = ClientSlot::acquire() else {
        return;
    };
    let Ok((mut reader, writer)) = ipc::establish(stream, ChannelKind::Control) else {
        return;
    };
    let cancel = reader.cancel_handle();
    let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    while let Ok(message) = reader.recv::<ClientMessage>() {
        if actor
            .sender
            .send(ActorEvent::Client {
                id,
                writer: writer.clone(),
                cancel: cancel.clone(),
                message,
            })
            .is_err()
        {
            break;
        }
    }
    let _ = actor.sender.send(ActorEvent::Disconnected(id));
}

fn clean_stale(paths: &RuntimePaths) -> io::Result<()> {
    if !paths.artifacts_exist() {
        return Ok(());
    }
    if platform::session_is_connectable(&paths.socket) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "session already exists",
        ));
    }
    discard_unusable_registry(paths)?;
    match paths.read_registry() {
        Ok(registry) => {
            if registry_process_matches(&registry) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "session owner is alive but its endpoint is unavailable",
                ));
            }
            paths.remove_instance(&registry);
            if paths.artifacts_exist() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "could not remove the exact stale session instance",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(context("validate stale session registry", error)),
    }
    #[cfg(unix)]
    paths.remove_stale_endpoints();
    Ok(())
}

/// Reclaim a dead session whose registry this build cannot validate.
///
/// A registry written by a different VVMX version, a newer schema, or a truncated write cannot be
/// matched against an exact instance, and the caller has already proven that the recorded endpoint
/// accepts nothing. Refusing to start here would strand the session name until someone deleted the
/// runtime files by hand — and because the client only sees the stale endpoint's own connect error,
/// the reason would never reach the user. Reclaim the name instead, unless the process that wrote
/// the registry is still running.
fn discard_unusable_registry(paths: &RuntimePaths) -> io::Result<()> {
    let error = match paths.read_registry() {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => error,
    };
    if let Some(registry) = paths.read_unvalidated_registry()
        && registry_process_matches(&registry)
    {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "session server PID {} {} is running but its endpoint is unavailable; stop it before starting this build",
                registry.pid,
                describe_recorded_version(registry.vvmx_version)
            ),
        ));
    }
    paths.remove_unusable_registry();
    match paths.read_registry() {
        Ok(_) => Ok(()),
        Err(retry) if retry.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(context("validate stale session registry", error)),
    }
}

fn describe_recorded_version(version: Option<u16>) -> String {
    match version {
        Some(version) => format!("(VVMX v{version})"),
        None => "(no recorded VVMX version)".to_owned(),
    }
}

#[cfg(unix)]
fn install_signal_forwarder(actor: ActorHandle) -> io::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGUSR1,
    ])?;
    thread::Builder::new()
        .name("vvmux-signals".into())
        .spawn(move || {
            // Match on the signal: SIGUSR1 asks for a config reload, so it must not fall into the
            // shutdown path the way an unexamined "any signal" check would put it.
            for signal in signals.forever() {
                if signal == libc::SIGUSR1 {
                    let _ = actor.sender.try_send(session::ActorEvent::ConfigChanged);
                    continue;
                }
                actor.shutdown.store(true, Ordering::Release);
                break;
            }
        })?;
    Ok(())
}

#[cfg(windows)]
fn install_signal_forwarder(_actor: ActorHandle) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    /// Writes a registry recording a foreign VVMX version, as a server built before a version bump
    /// would have left behind, and removes its uniquely named artifacts afterwards.
    struct ForeignRegistry {
        paths: RuntimePaths,
    }

    impl ForeignRegistry {
        fn write(name_prefix: &str, version: u16, pid: u32) -> Self {
            let name = unique_fixture_name(name_prefix);
            let paths = RuntimePaths::for_session(&name).unwrap();
            let body = format!(
                "{{\"schema\":2,\"name\":\"{name}\",\"pid\":{pid},\
                 \"instance_nonce\":\"{}\",\"vvmx_version\":{version}}}",
                "01".repeat(32)
            );
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&paths.registry)
                .unwrap();
            file.write_all(body.as_bytes()).unwrap();
            Self { paths }
        }
    }

    impl Drop for ForeignRegistry {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.paths.registry);
            self.paths.remove_stale_endpoints();
        }
    }

    fn unique_fixture_name(prefix: &str) -> String {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        format!("{prefix}-{:032x}", u128::from_ne_bytes(random))
    }

    fn mismatch() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, ipc::VERSION_MISMATCH)
    }

    #[test]
    fn version_mismatch_names_the_stale_session_server() {
        let fixture = ForeignRegistry::write("vvmx-mismatch-fixture", ipc::VERSION - 1, 424_242);
        let described = describe_peer_version(&fixture.paths, mismatch()).to_string();
        assert!(described.starts_with(ipc::VERSION_MISMATCH), "{described}");
        assert!(described.contains("PID 424242"), "{described}");
        assert!(
            described.contains(&format!("VVMX v{}", ipc::VERSION - 1)),
            "{described}"
        );
        assert!(
            described.contains(&format!("this build speaks VVMX v{}", ipc::VERSION)),
            "{described}"
        );
    }

    #[test]
    fn version_mismatch_without_a_registry_still_reports_this_build() {
        let name = unique_fixture_name("vvmx-absent-fixture");
        let paths = RuntimePaths::for_session(&name).unwrap();
        let described = describe_peer_version(&paths, mismatch()).to_string();
        assert!(described.starts_with(ipc::VERSION_MISMATCH), "{described}");
        assert!(!described.contains("PID"), "{described}");
        assert!(
            described.contains(&format!("this build speaks VVMX v{}", ipc::VERSION)),
            "{described}"
        );
    }

    #[test]
    fn unrelated_handshake_errors_are_left_alone() {
        let fixture = ForeignRegistry::write("vvmx-passthrough-fixture", ipc::VERSION - 1, 7);
        let original = io::Error::new(io::ErrorKind::InvalidData, "bad VVMX magic");
        let described = describe_peer_version(&fixture.paths, original).to_string();
        assert_eq!(described, "bad VVMX magic");
    }

    /// A leftover endpoint file with no listener, exactly as a server killed without cleanup
    /// leaves behind. Dropping the listener does not unlink the path, so connecting to it is
    /// refused rather than reported as missing.
    fn leave_stale_endpoint(paths: &RuntimePaths) -> Option<()> {
        match std::os::unix::net::UnixListener::bind(&paths.socket) {
            Ok(listener) => {
                drop(listener);
                for _ in 0..100 {
                    if !platform::session_is_connectable(&paths.socket) {
                        return Some(());
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                panic!("staged session endpoint remained connectable after its listener closed");
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping stale-endpoint socket test: {error}");
                None
            }
            Err(error) => panic!("could not stage a stale session endpoint: {error}"),
        }
    }

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn a_dead_session_recorded_by_another_version_is_reclaimed() {
        let fixture = ForeignRegistry::write("vvmx-reclaim-fixture", ipc::VERSION - 1, dead_pid());
        let Some(()) = leave_stale_endpoint(&fixture.paths) else {
            return;
        };
        assert!(!platform::session_is_connectable(&fixture.paths.socket));

        clean_stale(&fixture.paths).expect("a dead foreign session must not block startup");

        // Both the unusable registry and the endpoint nobody answers on are gone, so the very
        // next bind succeeds instead of leaving every client to report "connection refused".
        assert!(!fixture.paths.artifacts_exist());
        SessionListener::bind(&fixture.paths.socket).unwrap();
    }

    #[test]
    fn a_live_session_recorded_by_another_version_is_never_reclaimed() {
        let fixture = ForeignRegistry::write(
            "vvmx-live-foreign-fixture",
            ipc::VERSION - 1,
            std::process::id(),
        );
        let Some(()) = leave_stale_endpoint(&fixture.paths) else {
            return;
        };

        let error = clean_stale(&fixture.paths).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        let described = error.to_string();
        assert!(
            described.contains(&format!("PID {}", std::process::id())),
            "{described}"
        );
        assert!(
            described.contains(&format!("VVMX v{}", ipc::VERSION - 1)),
            "{described}"
        );
        assert!(
            fixture.paths.registry.exists(),
            "a running owner's registry must survive"
        );
    }
}
