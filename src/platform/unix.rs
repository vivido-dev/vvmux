use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, PipeReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{ConnectionCancel, Transport};
use crate::ipc::DisplayMetrics;

pub type SessionEndpoint = PathBuf;
pub type VirtualPresenterEndpoint = PathBuf;

/// How long a launcher waits for a spawned session server's startup result.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on a startup diagnostic, so a failing server cannot flood its launcher.
const READINESS_LIMIT: usize = 4096;

/// Windows restores inherited console-interrupt state here; Unix needs nothing.
pub fn prepare_server_process() {}

/// The server side of the one-shot startup channel.
///
/// Both platforms use the same result format: `OK\n`, or `ERR\n` followed by a bounded diagnostic.
/// Windows inherits a pipe handle, Unix an inherited descriptor number. The channel is closed by
/// the first write, so a launcher blocked on it always observes either a result or EOF.
pub struct ReadinessWriter {
    file: Option<File>,
}

impl ReadinessWriter {
    pub fn from_metadata(handle: Option<usize>) -> io::Result<Self> {
        let Some(handle) = handle else {
            return Ok(Self { file: None });
        };
        let descriptor = RawFd::try_from(handle)
            .ok()
            .filter(|descriptor| *descriptor > libc::STDERR_FILENO)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "readiness descriptor is not a private inherited descriptor",
                )
            })?;
        // The flag is a hidden argument, so treat its value as untrusted: report a result only
        // over an inherited pipe, never into whatever else happens to occupy that number.
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(descriptor, &mut status) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if status.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "readiness descriptor is not a pipe",
            ));
        }
        // The launcher had to clear close-on-exec to get the descriptor here; restore it now.
        // A pane shell that inherited the startup channel would hold it open for the life of the
        // session, and the launcher waits for the channel to close before it reads a diagnostic.
        set_close_on_exec(descriptor)?;
        Ok(Self {
            file: Some(unsafe { File::from_raw_fd(descriptor) }),
        })
    }

    pub fn success(&mut self) -> io::Result<()> {
        self.write_result(b"OK\n")
    }

    pub fn failure(&mut self, error: &io::Error) {
        let mut message = format!("ERR\n{error}").into_bytes();
        message.truncate(READINESS_LIMIT);
        let _ = self.write_result(&message);
    }

    fn write_result(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        file.write_all(bytes)?;
        file.flush()
    }
}

/// The launcher side of the startup channel.
struct ReadinessReader {
    reader: PipeReader,
}

impl ReadinessReader {
    /// Turn the server's startup result into this launcher's result.
    fn wait(mut self, mut child: Child, timeout: Duration) -> io::Result<()> {
        let bytes = self.read_result(timeout)?;
        if bytes.as_slice() == b"OK\n" {
            // The server is bound and serving; it must keep running, so it is not waited on.
            return Ok(());
        }
        // Anything else means the server is on its way out. Reap it so a failed startup does not
        // leave a zombie behind, then report what it said instead of the endpoint error the caller
        // would otherwise discover on its own.
        let _ = child.wait();
        if let Some(diagnostic) = bytes.strip_prefix(b"ERR\n") {
            Err(io::Error::other(format!(
                "vvmux server startup failed: {}",
                String::from_utf8_lossy(diagnostic)
            )))
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "vvmux server exited without a readiness result",
            ))
        }
    }

    fn read_result(&mut self, timeout: Duration) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 256];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "vvmux server startup timed out",
                ));
            }
            if !self.poll_readable(remaining)? {
                continue;
            }
            match self.reader.read(&mut chunk) {
                Ok(0) => return Ok(bytes),
                Ok(read) => {
                    if bytes.len() + read > READINESS_LIMIT {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "vvmux server startup diagnostic exceeded 4 KiB",
                        ));
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    // Success is a complete fixed token, so it needs no channel close to be
                    // recognized. Only a diagnostic is read to the end.
                    if bytes.as_slice() == b"OK\n" {
                        return Ok(bytes);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn poll_readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut poll_fd = libc::pollfd {
            fd: self.reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX);
        match unsafe { libc::poll(&mut poll_fd, 1, milliseconds) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
            0 => Ok(false),
            _ => Ok(true),
        }
    }
}

pub struct DaemonLauncher;

impl DaemonLauncher {
    /// Start a detached session server and report what its startup actually did.
    ///
    /// Without the startup channel a server that exits before binding is indistinguishable from a
    /// server that has not finished starting: the client only sees its own connect error, which for
    /// a leftover endpoint file is a bare "connection refused" that names neither the session nor
    /// the reason.
    pub fn launch(name: &str, config_path: Option<&Path>) -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let (readiness, writer) = readiness_pipe()?;
        let writer_descriptor = writer.as_raw_fd();
        let mut command = Command::new(executable);
        command.arg("__server").arg("--session").arg(name);
        if let Some(path) = config_path {
            command.arg("--config").arg(path);
        }
        command
            .arg("--ready-handle")
            .arg(writer_descriptor.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        scrub_daemon_environment(&mut command, std::env::vars_os());
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                // The startup channel is the one descriptor that must survive exec. Its reading end
                // stays close-on-exec, so the launcher sees EOF the moment the server exits.
                clear_close_on_exec(writer_descriptor)
            });
        }
        let child = command.spawn()?;
        // Only the server may hold the writing end now; otherwise EOF would never arrive.
        drop(writer);
        readiness.wait(child, STARTUP_TIMEOUT)
    }
}

/// Keep outer terminal ownership out of a process that outlives the shell that started it.
///
/// The whole `VIVID_*` namespace goes, not just the credential pair, which is also what the Windows
/// launcher does. A session server inherits its environment to every pane it spawns, so a surviving
/// `VIVID_ANCHOR_TRANSPORT` from a remote `vvssh` login would make pane producers frame anchor
/// markers for the *outer* platform's pseudoconsole while they are talking to this hop's virtual
/// presenter, whose scanner does not recognize that envelope.
///
/// An outer tmux or screen identity is stale for the same reason: the daemon creates and owns a new
/// PTY boundary. If `TMUX` or `STY` reaches that PTY, Vivid producers deliberately decline text
/// anchors because an ordinary multiplexer may consume their APC marker. Under vvmux the marker is
/// instead authenticated and consumed by the virtual presenter, so inheriting the outer identity
/// turns an anchored node into an absolute cursor placement that scrolls away when the producer
/// reserves its rows. A real nested tmux or screen launched inside the pane will establish fresh
/// values itself.
fn scrub_daemon_environment(
    command: &mut Command,
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) {
    for (key, _) in environment {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("VIVID_")
            || matches!(key_text.as_ref(), "TMUX" | "TMUX_PANE" | "STY")
        {
            command.env_remove(&key);
        }
    }
}

fn readiness_pipe() -> io::Result<(ReadinessReader, OwnedFd)> {
    let (reader, writer) = io::pipe()?;
    // Keep both ends clear of the standard descriptor numbers. A writing end that landed there
    // would be replaced by the redirection `Command` applies after fork, and a reading end there
    // would take over a standard slot this process still reports its own errors through.
    let reader = PipeReader::from(relocate_above_standard_descriptors(OwnedFd::from(reader))?);
    let writer = relocate_above_standard_descriptors(OwnedFd::from(writer))?;
    Ok((ReadinessReader { reader }, writer))
}

fn relocate_above_standard_descriptors(descriptor: OwnedFd) -> io::Result<OwnedFd> {
    if descriptor.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(descriptor);
    }
    let relocated = unsafe {
        libc::fcntl(
            descriptor.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            libc::STDERR_FILENO + 1,
        )
    };
    if relocated == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(relocated) })
}

/// Async-signal-safe: only `fcntl` runs, as required between `fork` and `exec`.
fn clear_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    update_close_on_exec(descriptor, false)
}

fn set_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    update_close_on_exec(descriptor, true)
}

fn update_close_on_exec(descriptor: RawFd, enabled: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let updated = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, updated) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub struct ClientTerminal {
    original: libc::termios,
    output: File,
}

impl ClientTerminal {
    pub fn enter() -> io::Result<Self> {
        require_interactive_terminal()?;
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut output = match duplicate_fd(libc::STDOUT_FILENO) {
            Ok(output) => output,
            Err(error) => {
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
                return Err(error);
            }
        };
        if let Err(error) = output
            .write_all(
                b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h",
            )
            .and_then(|()| output.flush())
        {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
            return Err(error);
        }
        Ok(Self { original, output })
    }

    pub fn display_metrics(&self) -> io::Result<DisplayMetrics> {
        current_display_metrics()
    }

    pub fn output(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.output.try_clone()?))
    }

    pub fn read_input(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut poll_fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(error)
            }
        } else if result == 0 || poll_fd.revents & libc::POLLIN == 0 {
            Ok(None)
        } else {
            io::stdin().read(buffer).map(Some)
        }
    }
}

/// Query and validate the host terminal without changing any of its modes.
///
/// Attachment admission uses this before entering the alternate screen so a refusal is invisible
/// to the terminal other than the diagnostic printed by the command.
pub fn current_display_metrics() -> io::Result<DisplayMetrics> {
    require_interactive_terminal()?;
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ as _, &mut size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if size.ws_col == 0 || size.ws_row == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal has zero dimensions",
        ));
    }
    Ok(DisplayMetrics {
        columns: size.ws_col,
        rows: size.ws_row,
        cell_width: size.ws_xpixel.checked_div(size.ws_col).unwrap_or(0),
        cell_height: size.ws_ypixel.checked_div(size.ws_row).unwrap_or(0),
    })
}

fn require_interactive_terminal() -> io::Result<()> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1
        && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "vvmux attach requires an interactive terminal",
        ))
    }
}

impl Drop for ClientTerminal {
    fn drop(&mut self) {
        let _ = self.output.write_all(
            b"\x1b[0m\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l",
        );
        let _ = self.output.flush();
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

fn duplicate_fd(fd: RawFd) -> io::Result<File> {
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(duplicate) })
    }
}

pub struct SessionListener {
    inner: UnixListener,
}

impl SessionListener {
    pub fn bind(endpoint: &Path) -> io::Result<Self> {
        let inner = UnixListener::bind(endpoint)?;
        fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))?;
        inner.set_nonblocking(true)?;
        Ok(Self { inner })
    }

    pub fn accept(&self) -> io::Result<Transport> {
        let (stream, _) = self.inner.accept()?;
        stream.set_nonblocking(false)?;
        require_peer_owner(&stream)?;
        split_unix(stream)
    }
}

pub fn connect_session(endpoint: &Path) -> io::Result<Transport> {
    let stream = UnixStream::connect(endpoint)?;
    require_peer_owner(&stream)?;
    split_unix(stream)
}

pub fn session_is_connectable(endpoint: &Path) -> bool {
    UnixStream::connect(endpoint).is_ok()
}

pub struct VirtualPresenterListener {
    inner: UnixListener,
    endpoint: PathBuf,
}

impl VirtualPresenterListener {
    pub fn bind(endpoint: PathBuf) -> io::Result<Self> {
        if endpoint.exists() {
            fs::remove_file(&endpoint)?;
        }
        let inner = UnixListener::bind(&endpoint)?;
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
        inner.set_nonblocking(true)?;
        Ok(Self { inner, endpoint })
    }

    pub fn endpoint(&self) -> String {
        format!("unix:{}", self.endpoint.display())
    }

    pub fn accept(&self) -> io::Result<Transport> {
        let (stream, _) = self.inner.accept()?;
        stream.set_nonblocking(false)?;
        require_peer_owner(&stream)?;
        split_unix(stream)
    }
}

impl Drop for VirtualPresenterListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.endpoint);
    }
}

fn split_unix(stream: UnixStream) -> io::Result<Transport> {
    let reader = stream.try_clone()?;
    let cancel_reader = reader.try_clone()?;
    let cancel_writer = stream.try_clone()?;
    let cancel = ConnectionCancel::new(move || {
        let _ = cancel_reader.shutdown(Shutdown::Both);
        let _ = cancel_writer.shutdown(Shutdown::Both);
    });
    // Darwin rejects SO_RCVTIMEO on local-domain sockets with EINVAL. Keep the timeout in
    // userspace and poll before each read so handshake deadlines work identically on every Unix
    // platform without changing the socket into nonblocking mode.
    let read_timeout = Arc::new(Mutex::new(None));
    let timeout_value = read_timeout.clone();
    let timeout = Arc::new(move |duration: Option<Duration>| {
        *timeout_value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = duration;
        Ok(())
    });
    Ok(Transport::new(
        Box::new(PollTimeoutReader {
            stream: reader,
            timeout: read_timeout,
        }),
        Box::new(stream),
        cancel,
        timeout,
    ))
}

struct PollTimeoutReader {
    stream: UnixStream,
    timeout: Arc<Mutex<Option<Duration>>>,
}

impl Read for PollTimeoutReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let timeout = *self
            .timeout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(timeout) = timeout {
            let milliseconds = i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX);
            let mut descriptor = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                match unsafe { libc::poll(&mut descriptor, 1, milliseconds) } {
                    -1 => {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                    0 => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "local transport read timed out",
                        ));
                    }
                    _ => break,
                }
            }
        }
        self.stream.read(buffer)
    }
}

pub fn require_peer_owner(stream: &UnixStream) -> io::Result<()> {
    let expected = unsafe { libc::geteuid() };
    let actual = peer_uid(stream)?;
    if actual != expected {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local stream peer UID mismatch",
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
    use std::os::fd::AsRawFd;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(credentials.uid)
    }
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
    use std::os::fd::AsRawFd;
    let mut uid = 0;
    let mut gid = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    fn writer_for(descriptor: OwnedFd) -> (ReadinessWriter, RawFd) {
        let descriptor = descriptor.into_raw_fd();
        let writer = ReadinessWriter::from_metadata(Some(descriptor as usize)).unwrap();
        (writer, descriptor)
    }

    #[test]
    fn startup_success_is_reported_without_waiting_for_the_channel_to_close() {
        let (mut reader, descriptor) = readiness_pipe().unwrap();
        let (mut writer, descriptor) = writer_for(descriptor);
        // A pane process must not be able to hold the startup channel open, so the descriptor is
        // close-on-exec again as soon as the server owns it.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        writer.success().unwrap();
        // The writer is deliberately still alive: success must not depend on end-of-file.
        assert_eq!(
            reader.read_result(Duration::from_secs(5)).unwrap(),
            b"OK\n".to_vec()
        );
    }

    #[test]
    fn startup_failure_reports_the_servers_own_diagnostic() {
        let (reader, descriptor) = readiness_pipe().unwrap();
        let (mut writer, _) = writer_for(descriptor);
        writer.failure(&io::Error::other(
            "bind session endpoint: Permission denied",
        ));
        drop(writer);
        let child = Command::new("true").spawn().unwrap();
        let error = reader.wait(child, Duration::from_secs(5)).unwrap_err();
        let described = error.to_string();
        assert!(
            described.starts_with("vvmux server startup failed: "),
            "{described}"
        );
        assert!(described.contains("bind session endpoint"), "{described}");
    }

    #[test]
    fn a_server_that_exits_silently_is_not_reported_as_a_connect_failure() {
        let (reader, descriptor) = readiness_pipe().unwrap();
        drop(descriptor);
        let child = Command::new("true").spawn().unwrap();
        let error = reader.wait(child, Duration::from_secs(5)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn only_an_inherited_pipe_is_accepted_as_a_readiness_channel() {
        // No channel at all: a server started by hand still runs.
        assert!(ReadinessWriter::from_metadata(None).unwrap().file.is_none());
        // Standard descriptors are redirected by the launcher, so they can never be the channel.
        assert!(ReadinessWriter::from_metadata(Some(1)).is_err());
        // A hidden argument is untrusted input: never report into an unrelated open file.
        let file = tempfile::NamedTempFile::new().unwrap();
        let descriptor = file.reopen().unwrap().into_raw_fd();
        assert_eq!(
            ReadinessWriter::from_metadata(Some(descriptor as usize))
                .map(|_| ())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        unsafe { libc::close(descriptor) };
    }

    #[test]
    fn a_daemon_inherits_no_outer_presenter_or_multiplexer_state() {
        let mut command = Command::new("true");
        scrub_daemon_environment(
            &mut command,
            [
                ("VIVID_ENDPOINT_CONTROL", "unix:/tmp/outer.sock"),
                ("VIVID_ROOT_SECRET", "secret"),
                ("VIVID_ENDPOINT_BULK", "unix:/tmp/outer-bulk.sock"),
                // A Windows presenter exports this through `vvssh`; a Unix session server and its
                // panes must not keep it.
                ("VIVID_ANCHOR_TRANSPORT", "conpty"),
                ("VIVID_REMOTE", "1"),
                // These identify the shell that launched vvmux, not the PTYs owned by the new
                // session daemon. Keeping them disables Vivid text anchors inside every pane.
                ("TMUX", "/tmp/tmux-1000/default,1,0"),
                ("TMUX_PANE", "%1"),
                ("STY", "1234.outer"),
                ("PATH", "/usr/bin"),
                ("TERM", "xterm-256color"),
            ]
            .map(|(key, value)| {
                (
                    std::ffi::OsString::from(key),
                    std::ffi::OsString::from(value),
                )
            }),
        );
        let mut removed = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        removed.sort();
        assert_eq!(
            removed,
            [
                "STY",
                "TMUX",
                "TMUX_PANE",
                "VIVID_ANCHOR_TRANSPORT",
                "VIVID_ENDPOINT_BULK",
                "VIVID_ENDPOINT_CONTROL",
                "VIVID_REMOTE",
                "VIVID_ROOT_SECRET",
            ]
        );
    }

    #[test]
    fn a_readiness_channel_never_lands_on_a_standard_descriptor() {
        let (reader, writer) = readiness_pipe().unwrap();
        assert!(writer.as_raw_fd() > libc::STDERR_FILENO);
        assert!(reader.reader.as_raw_fd() > libc::STDERR_FILENO);
    }
}
