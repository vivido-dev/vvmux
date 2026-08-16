use std::ffi::OsStr;
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

#[derive(Clone)]
pub struct PtyControl {
    inner: Arc<ControlInner>,
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
            unsafe {
                libc::kill(-self.process_group, libc::SIGHUP);
            }
        }
    }
}

pub struct PtyWaiter {
    child: Child,
}

#[derive(Debug, Clone, Copy)]
pub struct PtyExitStatus {
    pub code: Option<i64>,
    pub signal: Option<i32>,
    pub success: bool,
}

impl PtyControl {
    pub fn foreground_process_group_id(&self) -> Option<u32> {
        let group = unsafe { libc::tcgetpgrp(self.inner.resize.as_raw_fd()) };
        u32::try_from(group).ok().filter(|group| *group != 0)
    }

    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        self.resize_with_pixels(columns, rows, 0, 0)
    }

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
        if unsafe { libc::ioctl(self.inner.resize.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } == -1
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn terminate(&self) {
        if self.inner.terminating.swap(true, Ordering::AcqRel) {
            return;
        }
        let group = self.inner.process_group;
        let _ = std::thread::Builder::new()
            .name(format!("vvmux-terminate-{group}"))
            .spawn(move || {
                unsafe {
                    libc::kill(-group, libc::SIGHUP);
                }
                std::thread::sleep(Duration::from_millis(250));
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            });
    }

    pub fn terminate_blocking(&self) {
        if self.inner.terminating.swap(true, Ordering::AcqRel) {
            return;
        }
        let group = self.inner.process_group;
        unsafe {
            libc::kill(-group, libc::SIGHUP);
        }
        std::thread::sleep(Duration::from_millis(250));
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

impl PtyWaiter {
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
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            size,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }

    let master_file = unsafe { File::from_raw_fd(master) };
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
    builder.env_remove("VIVID_ENDPOINT_CONTROL");
    builder.env_remove("VIVID_TOKEN");
    builder.env_remove("VIVID_ROOT_SECRET");
    builder.env_remove("VIVID_ANCHOR_TRANSPORT");
    builder.env_remove("VIVID_SSH_ENDPOINT");
    builder.env_remove("VIVID_SSH_TOKEN");
    for (key, value) in environment {
        builder.env(key, value);
    }
    unsafe {
        builder.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::other(format!(
                    "PTY child setsid failed: {}",
                    io::Error::last_os_error()
                )));
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::other(format!(
                    "PTY child TIOCSCTTY failed: {}",
                    io::Error::last_os_error()
                )));
            }
            Ok(())
        });
    }
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
        assert_ne!(
            unsafe {
                libc::ioctl(
                    parts.control.inner.resize.as_raw_fd(),
                    libc::TIOCGWINSZ as _,
                    size.as_mut_ptr(),
                )
            },
            -1
        );
        let size = unsafe { size.assume_init() };
        assert_eq!((size.ws_col, size.ws_row), (100, 30));
        assert_eq!((size.ws_xpixel, size.ws_ypixel), (1200, 600));
        parts.control.terminate_blocking();
    }
}
