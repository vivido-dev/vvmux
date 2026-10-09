use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{PtyParts, input};

/// How long a pane's process group gets between SIGHUP and SIGKILL.
///
/// Long enough for an interactive shell to save history and for well-behaved children to exit on
/// hangup; short enough that closing a pane, or tearing down a session with many panes, does not
/// visibly stall. Raising it delays session shutdown by up to this amount per blocking close.
const TERMINATE_GRACE: Duration = Duration::from_millis(250);

/// Shared handle for resizing, signalling, and terminating one pane's process group.
///
/// Clones share the same pane. Dropping the last clone sends SIGHUP to the process group when no
/// termination has started, so an abandoned pane never keeps its PTY alive.
#[derive(Clone)]
pub struct PtyControl {
    inner: Arc<ControlInner>,
}

impl fmt::Debug for PtyControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyControl")
            .field("process_group", &self.inner.process_group)
            .field(
                "terminating",
                &self.inner.terminating.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

struct ControlInner {
    resize: File,
    process_group: i32,
    terminating: AtomicBool,
}

impl Drop for ControlInner {
    fn drop(&mut self) {
        // Startup errors and actor panics can drop a pane before the ordinary close path runs.
        // Do not leave its process group holding a PTY indefinitely; the normal termination paths
        // set this flag first and retain their SIGHUP-to-SIGKILL grace period.
        if !self.terminating.swap(true, Ordering::AcqRel) {
            signal_group(self.process_group, libc::SIGHUP);
        }
    }
}

/// Owner of the pane's child process, used to collect its exit status.
#[derive(Debug)]
pub struct PtyWaiter {
    child: Child,
}

/// How a pane's process ended.
#[derive(Debug, Clone, Copy)]
pub struct PtyExitStatus {
    /// The exit code, when the process exited normally.
    pub code: Option<i64>,
    /// The terminating signal, when a signal ended the process.
    pub signal: Option<i32>,
    /// Whether the process reported success.
    pub success: bool,
}

/// Send `signal` to every process in `group`.
///
/// A failure is not reported: the group may already be gone, which is the outcome the callers
/// want. Process-group IDs are reused only after every member has exited and been reaped, so a
/// stale ID can reach an unrelated group only after this pane's processes have all ended.
fn signal_group(group: i32, signal: i32) {
    // SAFETY: `kill` has no memory-safety preconditions; a negative PID addresses a process group.
    unsafe {
        libc::kill(-group, signal);
    }
}

impl PtyControl {
    /// Return the process group that currently owns the terminal, if any.
    #[must_use]
    pub fn foreground_process_group_id(&self) -> Option<u32> {
        // SAFETY: `resize` is an open PTY master descriptor owned by `self` for this whole call.
        let group = unsafe { libc::tcgetpgrp(self.inner.resize.as_raw_fd()) };
        u32::try_from(group).ok().filter(|group| *group != 0)
    }

    /// Resize the terminal to `columns` by `rows` cells, without pixel dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for a zero dimension, or the OS error when the
    /// PTY rejects the size.
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        self.resize_with_pixels(columns, rows, 0, 0)
    }

    /// Resize the terminal and report its pixel size to the application.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for a zero cell dimension, or the OS error when
    /// the PTY rejects the size.
    pub fn resize_with_pixels(
        &self,
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> io::Result<()> {
        if columns == 0 || rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero PTY dimensions",
            ));
        }
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: pixel_width,
            ws_ypixel: pixel_height,
        };
        // SAFETY: `resize` is an open PTY master descriptor, and `TIOCSWINSZ` reads exactly one
        // `winsize` from the pointer, which refers to a live local for the duration of the call.
        let result = unsafe {
            libc::ioctl(
                self.inner.resize.as_raw_fd(),
                libc::TIOCSWINSZ,
                &raw const size,
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Deliver one signal to the pane's foreground process group.
    ///
    /// The foreground group, not the child: a shell running `cargo test` puts that job in its own
    /// group and hands it the terminal, so signalling the child would reach the shell and leave the
    /// job running — which is the opposite of what a caller asking to interrupt means. Falls back
    /// to the pane's own group when nothing has claimed the terminal, so a signal still reaches a
    /// pane whose shell is sitting at its prompt.
    ///
    /// Refused once termination has started: the teardown path owns SIGHUP-then-SIGKILL, and a
    /// signal racing it would be delivered to a group that is already going away.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::BrokenPipe`] once termination has started,
    /// [`io::ErrorKind::NotFound`] when the pane has no process group, or the OS error from
    /// delivering the signal.
    pub fn signal(&self, signal: i32) -> io::Result<u32> {
        if self.inner.terminating.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pane is terminating",
            ));
        }
        let group = self
            .foreground_process_group_id()
            .and_then(|group| i32::try_from(group).ok())
            .unwrap_or(self.inner.process_group);
        if group <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "pane has no process group",
            ));
        }
        // SAFETY: `kill` has no memory-safety preconditions; `-group` addresses a process group.
        if unsafe { libc::kill(-group, signal) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(group as u32)
    }

    /// Start terminating the pane in the background: SIGHUP, then SIGKILL after a grace period.
    ///
    /// Returns immediately. Only the first termination request on a pane has any effect.
    pub fn terminate(&self) {
        if self.inner.terminating.swap(true, Ordering::AcqRel) {
            return;
        }
        let group = self.inner.process_group;
        let _ = std::thread::Builder::new()
            .name(format!("vvmux-terminate-{group}"))
            .spawn(move || {
                signal_group(group, libc::SIGHUP);
                std::thread::sleep(TERMINATE_GRACE);
                signal_group(group, libc::SIGKILL);
            });
    }

    /// Terminate the pane on the calling thread: SIGHUP, the grace period, then SIGKILL.
    ///
    /// Blocks for the grace period. Only the first termination request on a pane has any effect.
    pub fn terminate_blocking(&self) {
        if self.inner.terminating.swap(true, Ordering::AcqRel) {
            return;
        }
        let group = self.inner.process_group;
        signal_group(group, libc::SIGHUP);
        std::thread::sleep(TERMINATE_GRACE);
        signal_group(group, libc::SIGKILL);
    }
}

impl PtyWaiter {
    /// Block until the pane's process exits and report how it ended.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the exit status cannot be collected.
    pub fn wait(mut self) -> io::Result<PtyExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        let status = self.child.wait()?;
        Ok(PtyExitStatus {
            code: status.code().map(i64::from),
            signal: status.signal(),
            success: status.success(),
        })
    }
}

pub(super) fn spawn(
    shell: &OsStr,
    command: Option<&OsStr>,
    cwd: &Path,
    columns: u16,
    rows: u16,
    environment: &[(String, String)],
) -> io::Result<PtyParts> {
    let mut builder = Command::new(shell);
    // `-c <command>` runs one command and exits; `-l` is the ordinary interactive login shell.
    match command {
        Some(command) => {
            builder.arg("-c").arg(command);
        }
        None => {
            builder.arg("-l");
        }
    }
    spawn_command(builder, cwd, columns, rows, environment)
}

pub(super) fn spawn_argv(
    program: &OsStr,
    arguments: &[impl AsRef<OsStr>],
    cwd: &Path,
    columns: u16,
    rows: u16,
    environment: &[(String, String)],
) -> io::Result<PtyParts> {
    let mut builder = Command::new(program);
    builder.args(arguments.iter().map(AsRef::as_ref));
    spawn_command(builder, cwd, columns, rows, environment)
}

fn spawn_command(
    mut builder: Command,
    cwd: &Path,
    columns: u16,
    rows: u16,
    environment: &[(String, String)],
) -> io::Result<PtyParts> {
    if columns == 0 || rows == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero PTY dimensions",
        ));
    }
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // A raw pointer rather than `&mut size`: this argument is `*mut winsize` on macOS and
    // `*const winsize` on Linux, and only the raw form compiles on both without tripping
    // `clippy::unnecessary_mut_passed` on the platform that takes a const pointer.
    let size = &raw mut size;
    // SAFETY: the two descriptor pointers refer to live locals that `openpty` writes once each;
    // null name and termios pointers are documented as "not requested"; and `size` points to an
    // initialized `winsize` that outlives the call.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            size,
        )
    };
    if opened == -1 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `openpty` succeeded, so both descriptors are open and owned by nobody else; each is
    // wrapped exactly once and closed by its `File`.
    let master_file = unsafe { File::from_raw_fd(master) };
    // SAFETY: as above, for the slave side.
    let slave_file = unsafe { File::from_raw_fd(slave) };
    let stdin = slave_file.try_clone()?;
    let stdout = slave_file.try_clone()?;
    let slave_fd = slave_file.as_raw_fd();

    builder
        .current_dir(cwd)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave_file));
    builder.env_remove("VIVID_ENDPOINT");
    builder.env_remove("VIVID_ENDPOINT_BULK");
    builder.env_remove("VIVID_ENDPOINT_REALTIME");
    if std::env::var_os("VVMIC_PREPARED").is_some() {
        for name in [
            "PULSE_SOURCE",
            "PULSE_SERVER",
            "ALSA_CONFIG_PATH",
            "VVMIC_PREPARED",
            "VVMIC_LABEL",
        ] {
            builder.env_remove(name);
        }
    }
    builder.env_remove("VIVID_ENDPOINT_CONTROL");
    builder.env_remove("VIVID_TOKEN");
    builder.env_remove("VIVID_ROOT_SECRET");
    builder.env_remove("VIVID_ANCHOR_TRANSPORT");
    builder.env_remove("VIVID_SSH_ENDPOINT");
    builder.env_remove("VIVID_SSH_TOKEN");
    for (key, value) in environment {
        builder.env(key, value);
    }
    // `TIOCSCTTY` is `c_uint` on macOS but already the request type elsewhere, so this widening
    // is a no-op on Linux.
    #[allow(
        clippy::useless_conversion,
        reason = "the conversion is needed only on targets whose constant is narrower"
    )]
    let set_controlling_terminal = libc::TIOCSCTTY.into();
    // SAFETY: the hook runs in the forked child before `exec`, where only async-signal-safe work
    // is permitted. It calls only `setsid` and `ioctl`, and reports failure through
    // `last_os_error`, which reads `errno` without allocating; `slave_fd` stays open in the child
    // because the parent holds it until `spawn` returns. `std` forwards only the raw OS error to
    // the parent, so a formatted message here would be both unsafe and invisible.
    unsafe {
        builder.pre_exec(move || {
            if libc::setsid() == -1 || libc::ioctl(slave_fd, set_controlling_terminal, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    };
    let child = builder.spawn()?;
    let process_group = child.id() as i32;
    let reader = master_file.try_clone()?;
    let resize = master_file.try_clone()?;
    Ok(PtyParts {
        child_pid: process_group as u32,
        reader,
        input: input(master_file)?,
        control: PtyControl {
            inner: Arc::new(ControlInner {
                resize,
                process_group,
                terminating: AtomicBool::new(false),
            }),
        },
        waiter: PtyWaiter { child },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_propagates_pixel_dimensions() {
        let parts = spawn(
            OsStr::new("/bin/sh"),
            Some(OsStr::new("sleep 5")),
            Path::new("/tmp"),
            80,
            24,
            &[],
        )
        .unwrap();
        parts
            .control
            .resize_with_pixels(100, 30, 1200, 600)
            .unwrap();
        let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
        // SAFETY: the descriptor is the pane's open PTY master, and `TIOCGWINSZ` writes exactly
        // one `winsize` into the uninitialized local.
        let result = unsafe {
            libc::ioctl(
                parts.control.inner.resize.as_raw_fd(),
                libc::TIOCGWINSZ,
                size.as_mut_ptr(),
            )
        };
        assert_ne!(result, -1);
        // SAFETY: the successful `TIOCGWINSZ` above initialized every field.
        let size = unsafe { size.assume_init() };
        assert_eq!((size.ws_col, size.ws_row), (100, 30));
        assert_eq!((size.ws_xpixel, size.ws_ypixel), (1200, 600));
        parts.control.terminate_blocking();
    }

    #[test]
    fn debug_names_the_process_group_only() {
        let parts = spawn(
            OsStr::new("/bin/sh"),
            Some(OsStr::new("sleep 5")),
            Path::new("/tmp"),
            80,
            24,
            &[("VVMUX_TEST_SECRET".into(), "do-not-print".into())],
        )
        .unwrap();
        let rendered = format!("{:?}", parts.control);
        assert!(rendered.contains("process_group"), "{rendered}");
        assert!(!rendered.contains("do-not-print"), "{rendered}");
        parts.control.terminate_blocking();
    }
}
