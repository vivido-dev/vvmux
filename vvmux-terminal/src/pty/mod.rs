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
pub use unix::{PtyControl, PtyExitStatus, PtyWaiter};
#[cfg(windows)]
pub use windows::{PtyControl, PtyExitStatus, PtyWaiter};

const INPUT_QUEUE_ITEMS: usize = 64;
const INPUT_QUEUE_BYTES: usize = 1024 * 1024;

pub struct PtyProcess;

pub struct PtyParts {
    pub reader: File,
    pub input: PtyInput,
    pub control: PtyControl,
    pub waiter: PtyWaiter,
}

struct InputMessage {
    bytes: Vec<u8>,
    completion: Option<mpsc::Sender<io::Result<()>>>,
}

pub struct PtyInput {
    sender: mpsc::SyncSender<InputMessage>,
    queued_bytes: Arc<AtomicUsize>,
}

impl PtyInput {
    fn start(mut writer: File) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<InputMessage>(INPUT_QUEUE_ITEMS);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let worker_bytes = queued_bytes.clone();
        std::thread::Builder::new()
            .name("vvmux-pty-input".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    let length = message.bytes.len();
                    let result = writer
                        .write_all(&message.bytes)
                        .and_then(|()| writer.flush());
                    worker_bytes.fetch_sub(length, Ordering::AcqRel);
                    if let Some(completion) = message.completion {
                        let reported = match &result {
                            Ok(()) => Ok(()),
                            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
                        };
                        let _ = completion.send(reported);
                    }
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
        self.enqueue(bytes, None)
    }

    pub fn send_with_completion(&self, bytes: &[u8]) -> io::Result<mpsc::Receiver<io::Result<()>>> {
        let (sender, receiver) = mpsc::channel();
        if bytes.is_empty() {
            let _ = sender.send(Ok(()));
            return Ok(receiver);
        }
        self.enqueue(bytes, Some(sender))?;
        Ok(receiver)
    }

    fn enqueue(
        &self,
        bytes: &[u8],
        completion: Option<mpsc::Sender<io::Result<()>>>,
    ) -> io::Result<()> {
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
        let message = InputMessage {
            bytes: bytes.to_vec(),
            completion,
        };
        if let Err(error) = self.sender.try_send(message) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn completion_fires_after_bytes_are_flushed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pty-input");
        let file = File::create(&path).unwrap();
        let input = PtyInput::start(file).unwrap();
        let completion = input.send_with_completion(b"written").unwrap();
        completion
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"written");
    }
}
