use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{ConnectionCancel, Transport};
use crate::ipc::DisplayMetrics;

pub type SessionEndpoint = PathBuf;
pub type VirtualPresenterEndpoint = PathBuf;

/// Windows restores inherited console-interrupt state here; Unix needs nothing.
pub fn prepare_server_process() {}

pub struct ReadinessWriter;

impl ReadinessWriter {
    pub fn from_metadata(_handle: Option<usize>) -> io::Result<Self> {
        Ok(Self)
    }

    pub fn success(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn failure(&mut self, _error: &io::Error) {}
}

pub struct ClientTerminal {
    original: libc::termios,
    output: File,
}

impl ClientTerminal {
    pub fn enter() -> io::Result<Self> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1
            || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "vvmux attach requires an interactive terminal",
            ));
        }
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
    let timeout_stream = stream.try_clone()?;
    let cancel_reader = reader.try_clone()?;
    let cancel_writer = stream.try_clone()?;
    let cancel = ConnectionCancel::new(move || {
        let _ = cancel_reader.shutdown(Shutdown::Both);
        let _ = cancel_writer.shutdown(Shutdown::Both);
    });
    let timeout =
        Arc::new(move |duration: Option<Duration>| timeout_stream.set_read_timeout(duration));
    Ok(Transport::new(
        Box::new(reader),
        Box::new(stream),
        cancel,
        timeout,
    ))
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
