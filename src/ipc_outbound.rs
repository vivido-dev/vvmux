//! Bounded ordered output for session clients. The actor only admits bytes.
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use crate::platform::ConnectionCancel;

const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_CHUNKS: usize = 1024;

pub(super) struct Outbound {
    sender: mpsc::SyncSender<Vec<u8>>,
    bytes: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    cancel: ConnectionCancel,
}

impl Outbound {
    pub(super) fn start(
        mut stream: Box<dyn Write + Send>,
        cancel: ConnectionCancel,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(MAX_CHUNKS);
        let bytes = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let worker_bytes = bytes.clone();
        let worker_closed = closed.clone();
        let worker_cancel = cancel.clone();
        thread::Builder::new()
            .name("vvmux-client-output".into())
            .spawn(move || {
                while let Ok(chunk) = receiver.recv() {
                    if worker_closed.load(Ordering::Acquire) {
                        break;
                    }
                    let result = stream.write_all(&chunk);
                    worker_bytes.fetch_sub(chunk.len(), Ordering::AcqRel);
                    if result.is_err() {
                        break;
                    }
                }
                worker_closed.store(true, Ordering::Release);
                worker_cancel.cancel();
            })?;
        Ok(Self {
            sender,
            bytes,
            closed,
            cancel,
        })
    }
}

impl Write for Outbound {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "client output closed",
            ));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        if self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes.len())
                    .filter(|next| *next <= MAX_BYTES)
            })
            .is_err()
        {
            self.closed.store(true, Ordering::Release);
            self.cancel.cancel();
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "client output byte budget exceeded",
            ));
        }
        let mut chunk = Vec::new();
        if let Err(error) = chunk.try_reserve_exact(bytes.len()) {
            self.bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            self.closed.store(true, Ordering::Release);
            self.cancel.cancel();
            return Err(io::Error::other(error));
        }
        chunk.extend_from_slice(bytes);
        if self.sender.try_send(chunk).is_err() {
            self.bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            self.closed.store(true, Ordering::Release);
            self.cancel.cancel();
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "client output queue unavailable",
            ));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Outbound {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
    }
}
