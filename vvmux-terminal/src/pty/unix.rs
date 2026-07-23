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
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        if columns == 0 || rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero PTY dimensions",
            ));
        }
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
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
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
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

    let mut command = Command::new(shell);
    command
        .arg("-l")
        .current_dir(cwd)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave_file));
    command.env_remove("VIVID_ENDPOINT");
    command.env_remove("VIVID_ENDPOINT_BULK");
    command.env_remove("VIVID_TOKEN");
    command.env_remove("VIVID_SSH_ENDPOINT");
    command.env_remove("VIVID_SSH_TOKEN");
    for (key, value) in environment {
        command.env(key, value);
    }
    unsafe {
        command.pre_exec(move || {
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
    let child = command.spawn()?;
    let process_group = child.id() as i32;
    let reader = master_file.try_clone()?;
    let resize = master_file.try_clone()?;
    Ok(PtyParts {
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
