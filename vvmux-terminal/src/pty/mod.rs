use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{PtyControl, PtyWaiter};
#[cfg(windows)]
pub use windows::{PtyControl, PtyWaiter};

const INPUT_QUEUE_ITEMS: usize = 64;
const INPUT_QUEUE_BYTES: usize = 1024 * 1024;

pub struct PtyProcess;

pub struct PtyParts {
    pub reader: File,
    pub input: PtyInput,
    pub control: PtyControl,
    pub waiter: PtyWaiter,
}

pub struct PtyInput {
    sender: mpsc::SyncSender<Vec<u8>>,
    queued_bytes: Arc<AtomicUsize>,
}

impl PtyInput {
    fn start(mut writer: File) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE_ITEMS);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let worker_bytes = queued_bytes.clone();
        std::thread::Builder::new()
            .name("vvmux-pty-input".into())
            .spawn(move || {
                while let Ok(bytes) = receiver.recv() {
                    let length = bytes.len();
                    let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                    worker_bytes.fetch_sub(length, Ordering::AcqRel);
                    if result.is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender,
            queued_bytes,
        })
    }

    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() > INPUT_QUEUE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY input exceeds the bounded queue",
            ));
        }
        let mut queued = self.queued_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = queued.checked_add(bytes.len()) else {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "PTY input queue is full",
                ));
            };
            if next > INPUT_QUEUE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "PTY input queue is full",
                ));
            }
            match self.queued_bytes.compare_exchange_weak(
                queued,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => queued = actual,
            }
        }
        let owned = bytes.to_vec();
        if let Err(error) = self.sender.try_send(owned) {
            self.queued_bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            let kind = match error {
                mpsc::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                mpsc::TrySendError::Disconnected(_) => io::ErrorKind::BrokenPipe,
            };
            return Err(io::Error::new(kind, "PTY input queue is unavailable"));
        }
        Ok(())
    }
}

impl Write for PtyInput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.send(buffer)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PtyProcess {
    pub fn spawn(
        shell: &OsStr,
        cwd: &Path,
        columns: u16,
        rows: u16,
        environment: &[(String, String)],
    ) -> io::Result<PtyParts> {
        #[cfg(unix)]
        {
            unix::spawn(shell, cwd, columns, rows, environment)
        }
        #[cfg(windows)]
        {
            windows::spawn(shell, cwd, columns, rows, environment)
        }
    }
}

fn input(writer: File) -> io::Result<PtyInput> {
    PtyInput::start(writer)
}
