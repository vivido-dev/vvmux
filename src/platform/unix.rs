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
use vivid_sdk::presenter::DisplayMetrics;

pub type SessionEndpoint = PathBuf;
pub type VirtualPresenterEndpoint = PathBuf;

/// How long a launcher waits for a spawned session server's startup result.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on a startup diagnostic, so a failing server cannot flood its launcher.
const READINESS_LIMIT: usize = 4096;

/// Hand a URI to the host's registered handler, detached from vvmux entirely.
///
/// The URI is passed as one argv element and never reaches a shell: it comes from whatever wrote to
/// a pane, so `;`, `&`, and backticks must stay inert. Stdio is null because a browser writing to
/// the terminal vvmux is painting in raw mode would corrupt the frame, and `setsid` keeps the
/// handler alive past the pane and out of reach of the terminal's job-control signals.
pub fn open_external(uri: &str) -> io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = Command::new(program);
    command
        // No `--` separator: xdg-open matches it against its own `-*)` arm and fails with
        // "unexpected option". It is unnecessary anyway — the caller's scheme allow-list means the
        // URI always begins with a known scheme and can never be read as a flag.
        .arg(uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the hook runs between `fork` and `exec` and calls only the async-signal-safe
    // `setsid`, reporting failure through `last_os_error`, which reads `errno` without allocating.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    };
    let mut child = command.spawn()?;
    // `setsid` detaches the session, not the parent link: the opener is still a direct child, so
    // something has to reap it or every clicked link leaves a zombie for the daemon's lifetime.
    std::thread::Builder::new()
        .name("vvmux-opener".to_owned())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

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
        // SAFETY: `stat` is plain old data for which all-zero bytes are a valid value.
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        // SAFETY: `fstat` writes one `stat` into the live local; an invalid descriptor only yields
        // an error.
        if unsafe { libc::fstat(descriptor, &raw mut status) } == -1 {
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
            // SAFETY: the descriptor was validated above as an inherited pipe above the standard
            // descriptors, which nothing else in this freshly started process owns, so the file
            // becomes its sole owner.
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
        // SAFETY: `poll` reads and writes exactly one `pollfd`, the live local passed with a count
        // of one.
        match unsafe { libc::poll(&raw mut poll_fd, 1, milliseconds) } {
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
    pub fn launch(
        name: &str,
        config_path: Option<&Path>,
        layout_path: Option<&Path>,
    ) -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let (readiness, writer) = readiness_pipe()?;
        let writer_descriptor = writer.as_raw_fd();
        let mut command = Command::new(executable);
        command.arg("__server").arg("--session").arg(name);
        if let Some(path) = config_path {
            command.arg("--config").arg(path);
        }
        if let Some(path) = layout_path {
            command.arg("--layout").arg(path);
        }
        command.args(crate::logging::server_args());
        command
            .arg("--ready-handle")
            .arg(writer_descriptor.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        scrub_daemon_environment(&mut command, std::env::vars_os());
        // SAFETY: the hook runs between `fork` and `exec` and calls only async-signal-safe
        // functions: `setsid`, `close_stray_descriptors`, and `clear_close_on_exec`, each documented
        // as such, with errors read from `errno` without allocating.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                // A descriptor inherited by accident is held for the daemon's whole life, so
                // drop everything the server was not given on purpose before the startup channel
                // is made inheritable.
                close_stray_descriptors(writer_descriptor);
                // The startup channel is the one descriptor that must survive exec. Its reading end
                // stays close-on-exec, so the launcher sees EOF the moment the server exits.
                clear_close_on_exec(writer_descriptor)
            })
        };
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
/// The `VIVIDO_*` automation identity goes for the same reason, and it is the one an agent is most
/// likely to act on. `VIVIDO_SOCKET` and `VIVIDO_WINDOW_ID` name the Vivido window that happened to
/// start this daemon; the daemon outlives it. After a detach and a reattach to a different window
/// they address the wrong window, and under `vvssh` they address a socket on a machine the pane
/// cannot reach at all. An agent in a pane running `vivido msg` would then drive somebody else's
/// terminal, so the stale value must not survive rather than be merely discouraged. The live
/// identity belongs to whichever client is attached now, and is published by that client.
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
    let environment: Vec<_> = environment.into_iter().collect();
    let microphone_prepared = environment.iter().any(|(key, _)| key == "VVMIC_PREPARED");
    for (key, _) in environment {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("VIVID_")
            || (microphone_prepared
                && matches!(
                    key_text.as_ref(),
                    "PULSE_SOURCE" | "PULSE_SERVER" | "ALSA_CONFIG_PATH"
                ))
            || matches!(
                key_text.as_ref(),
                "VIVIDO_SOCKET"
                    | "VVMIC_PREPARED"
                    | "VVMIC_LABEL"
                    | "VIVIDO_WINDOW_ID"
                    | "VIVIDO_SESSION"
                    | "TMUX"
                    | "TMUX_PANE"
                    | "STY"
            )
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
    // SAFETY: `F_DUPFD_CLOEXEC` duplicates the descriptor owned by `descriptor`, which stays open
    // for the call.
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
    // SAFETY: `fcntl` returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(relocated) })
}

/// Close every descriptor a detached session server was not deliberately handed.
///
/// The daemon outlives the launcher that forked it, so a descriptor it inherits by accident stays
/// open for the session's whole life. That is not hypothetical: on platforms without an atomic
/// `pipe2`, `Command` opens its capture pipes and marks them close-on-exec in two steps, so a
/// launch running concurrently in another thread can fork with an unrelated pipe still
/// inheritable. The daemon would then hold that pipe's writing end forever and the thread waiting
/// on `Command::output` would never see EOF. Standard descriptors are already the null redirection
/// `Command` applied before this runs, and `keep` is the startup channel.
///
/// Async-signal-safe: only `close`, `close_range`, and `getrlimit` run, as required between `fork`
/// and `exec`. Failures are ignored — a descriptor that cannot be closed must not stop the server
/// from starting.
fn close_stray_descriptors(keep: RawFd) {
    // No `close_range`: an open descriptor is always below the soft descriptor limit, so that is a
    // real upper bound. Cap it anyway, because the limit may be effectively unlimited.
    const SCAN_LIMIT: RawFd = 64 * 1024;
    let first = libc::STDERR_FILENO + 1;
    #[cfg(target_os = "linux")]
    {
        // Two ranges, because the startup channel sits somewhere in the middle.
        let close_range = |low: RawFd, high: RawFd| -> bool {
            // SAFETY: `close_range` takes plain integers and is async-signal-safe; closing
            // descriptors cannot violate memory safety in this single-threaded child.
            low > high
                || unsafe { libc::syscall(libc::SYS_close_range, low as u32, high as u32, 0) } == 0
        };
        if close_range(first, keep - 1) && close_range(keep + 1, RawFd::MAX) {
            return;
        }
    }
    // SAFETY: `rlimit` is plain old data for which all-zero bytes are a valid value.
    let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
    // SAFETY: `getrlimit` writes one `rlimit` into the live local and is async-signal-safe.
    let bound = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == 0 {
        RawFd::try_from(limit.rlim_cur)
            .unwrap_or(SCAN_LIMIT)
            .min(SCAN_LIMIT)
    } else {
        SCAN_LIMIT
    };
    for descriptor in first..bound {
        if descriptor != keep {
            // SAFETY: this runs in the forked child before `exec`, where no Rust value still owns
            // these descriptors; closing one cannot invalidate memory.
            unsafe { libc::close(descriptor) };
        }
    }
}

/// Async-signal-safe: only `fcntl` runs, as required between `fork` and `exec`.
fn clear_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    update_close_on_exec(descriptor, false)
}

fn set_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    update_close_on_exec(descriptor, true)
}

fn update_close_on_exec(descriptor: RawFd, enabled: bool) -> io::Result<()> {
    // SAFETY: `F_GETFD` only reads descriptor flags; an invalid descriptor yields an error.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let updated = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: `F_SETFD` only changes this descriptor's close-on-exec flag.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, updated) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub struct ClientTerminal {
    original: libc::termios,
    output: File,
    messages: Option<MessageBlock>,
}

/// Write access that `mesg n` removed from the terminal device, so it can be given back.
///
/// `wall`, `journald` and other broadcasters write straight to the terminal device, bypassing
/// every pane. On the alternate screen that text lands at the cursor, scrolls the whole display
/// up and leaves the panes offset until the next full repaint. Dropping group and other write
/// access is the standard way to refuse those writes.
struct MessageBlock {
    fd: RawFd,
    removed: libc::mode_t,
}

const MESSAGE_WRITE_BITS: libc::mode_t = libc::S_IWGRP | libc::S_IWOTH;

impl MessageBlock {
    /// Best effort: a terminal the user does not own, or one that already refuses messages, is
    /// left alone.
    fn engage(fd: RawFd) -> Option<Self> {
        // SAFETY: `stat` is plain old data for which all-zero bytes are a valid value.
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        // SAFETY: `fstat` writes one `stat` into the live local; an invalid descriptor only yields
        // an error.
        if unsafe { libc::fstat(fd, &raw mut status) } == -1 {
            return None;
        }
        let removed = status.st_mode & MESSAGE_WRITE_BITS;
        // SAFETY: `fchmod` takes plain integers and only changes the terminal's permission bits.
        if removed == 0 || unsafe { libc::fchmod(fd, status.st_mode & 0o7777 & !removed) } == -1 {
            return None;
        }
        Some(Self { fd, removed })
    }

    fn release(&self) {
        // SAFETY: `stat` is plain old data for which all-zero bytes are a valid value.
        let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
        // SAFETY: `fstat` writes one `stat` into the live local; an invalid descriptor only yields
        // an error.
        if unsafe { libc::fstat(self.fd, &raw mut status) } == 0 {
            // SAFETY: `fchmod` takes plain integers and only restores the permission bits `engage`
            // removed.
            unsafe { libc::fchmod(self.fd, (status.st_mode & 0o7777) | self.removed) };
        }
    }
}

impl ClientTerminal {
    pub fn enter() -> io::Result<Self> {
        require_interactive_terminal()?;
        // SAFETY: `termios` is plain old data for which all-zero bytes are a valid value.
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        // SAFETY: `tcgetattr` writes one `termios` into the live local.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &raw mut original) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        // SAFETY: `cfmakeraw` modifies the initialized `termios` copy in place.
        unsafe { libc::cfmakeraw(&raw mut raw) };
        // SAFETY: `tcsetattr` reads one initialized `termios` from the live local.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const raw) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut output = match duplicate_fd(libc::STDOUT_FILENO) {
            Ok(output) => output,
            Err(error) => {
                // SAFETY: `original` is the initialized `termios` read from stdin above.
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const original) };
                return Err(error);
            }
        };
        if let Err(error) = output
            .write_all(
                b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h",
            )
            .and_then(|()| output.flush())
        {
            // SAFETY: `original` is the initialized `termios` read from stdin above.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const original) };
            return Err(error);
        }
        let messages = MessageBlock::engage(libc::STDOUT_FILENO);
        Ok(Self {
            original,
            output,
            messages,
        })
    }

    #[expect(
        clippy::unused_self,
        reason = "the Windows client keeps per-terminal state here, so both platforms share the method"
    )]
    pub fn display_metrics(&self) -> io::Result<DisplayMetrics> {
        current_display_metrics()
    }

    pub fn output(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.output.try_clone()?))
    }

    #[expect(
        clippy::unused_self,
        reason = "the Windows client keeps per-terminal state here, so both platforms share the method"
    )]
    pub fn read_input(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut poll_fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` reads and writes exactly one `pollfd`, the live local passed with a count
        // of one.
        let result = unsafe { libc::poll(&raw mut poll_fd, 1, timeout_ms) };
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
    // SAFETY: `TIOCGWINSZ` writes exactly one `winsize` into the live local; a non-terminal stdout
    // only yields an error.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &raw mut size) } == -1 {
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
    // SAFETY: `isatty` only inspects the descriptor number and has no memory preconditions.
    let stdin_is_terminal = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    // SAFETY: as above.
    let stdout_is_terminal = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    if stdin_is_terminal && stdout_is_terminal {
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
            b"\x1b[0m\x1b[=0u\x1b[?2004l\x1b[?1004l\x1b[?1016l\x1b[?1006l\x1b[?1003l\x1b[?1000l\x1b[?25h\x1b[?1049l",
        );
        let _ = self.output.flush();
        if let Some(messages) = &self.messages {
            messages.release();
        }
        // SAFETY: `original` is the initialized `termios` that `enter` read from stdin.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.original) };
    }
}

fn duplicate_fd(fd: RawFd) -> io::Result<File> {
    // SAFETY: `dup` takes a plain descriptor number; an invalid one only yields an error.
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `dup` returned a new descriptor that nothing else owns.
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
    let timeout_value = Arc::clone(&read_timeout);
    let timeout = Arc::new(move |duration: Option<Duration>| {
        *timeout_value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = duration;
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
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = *self
            .timeout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(timeout) = timeout {
            let milliseconds = i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX);
            let mut descriptor = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                // SAFETY: `poll` reads and writes exactly one `pollfd`, the live local passed with
                // a count of one.
                match unsafe { libc::poll(&raw mut descriptor, 1, milliseconds) } {
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
        self.stream.read(buf)
    }
}

/// The effective user ID of this process: the owner every private vvmux file must have.
pub fn effective_uid() -> libc::uid_t {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

pub fn require_peer_owner(stream: &UnixStream) -> io::Result<()> {
    let expected = effective_uid();
    let actual = peer_uid(stream)?;
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local stream peer UID mismatch",
        ))
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the option buffer is the live `credentials` local, and `length` holds its exact size.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut length,
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
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: `getpeereid` writes one ID into each live local, for a socket that `stream` keeps
    // open.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
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
    fn message_block_removes_and_restores_broadcast_write_access() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let device = temp_device();
        fs::set_permissions(&device.0, fs::Permissions::from_mode(0o620)).unwrap();
        let file = File::open(&device.0).unwrap();
        let mode = || fs::metadata(&device.0).unwrap().permissions().mode() & 0o777;
        let block = MessageBlock::engage(file.as_raw_fd()).unwrap();
        assert_eq!(mode(), 0o600, "group write must be refused while attached");
        block.release();
        assert_eq!(mode(), 0o620);
        // A terminal that already refuses messages is left exactly as found.
        fs::set_permissions(&device.0, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(MessageBlock::engage(file.as_raw_fd()).is_none());
        assert_eq!(mode(), 0o600);
    }

    struct TempPath(PathBuf);

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn temp_device() -> TempPath {
        let path = std::env::temp_dir().join(format!("vvmux-mesg-{}", std::process::id()));
        File::create(&path).unwrap();
        TempPath(path)
    }

    #[test]
    fn startup_success_is_reported_without_waiting_for_the_channel_to_close() {
        let (mut reader, descriptor) = readiness_pipe().unwrap();
        let (mut writer, descriptor) = writer_for(descriptor);
        // A pane process must not be able to hold the startup channel open, so the descriptor is
        // close-on-exec again as soon as the server owns it.
        // SAFETY: `F_GETFD` only reads descriptor flags; an invalid descriptor yields an error.
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
        // SAFETY: the test owns this raw descriptor and closes it once.
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
