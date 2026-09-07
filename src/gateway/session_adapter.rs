use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;

use crate::ipc::{ClientMessage, DisplayMetrics, ServerMessage};

const WRITER_QUEUE_MESSAGES: usize = 256;

pub(crate) struct SessionAdapter {
    name: String,
    writer: mpsc::SyncSender<ClientMessage>,
    events: tokio_mpsc::Receiver<QueuedServerMessage>,
    cancel: crate::platform::ConnectionCancel,
    overloaded: Arc<AtomicBool>,
}

pub(crate) struct QueuedServerMessage {
    message: Option<StoredMessage>,
    size: usize,
    queued_bytes: Arc<AtomicUsize>,
}

impl QueuedServerMessage {
    pub(crate) fn take(&mut self) -> ServerMessage {
        match self.message.take().expect("queued message is present") {
            StoredMessage::Binary(message) => *message,
            StoredMessage::Structured(bytes) => serde_json::from_slice(&bytes)
                .expect("queue contains a locally serialized server message"),
        }
    }
}

impl Drop for QueuedServerMessage {
    fn drop(&mut self) {
        self.queued_bytes.fetch_sub(self.size, Ordering::AcqRel);
    }
}

impl SessionAdapter {
    pub(crate) async fn connect(
        name: String,
        takeover: bool,
        display: DisplayMetrics,
        vivid: bool,
        outbound_queue_bytes: usize,
    ) -> io::Result<(Self, String, bool)> {
        Self::open(name, Some((takeover, display, vivid)), outbound_queue_bytes).await
    }

    /// Open a session connection for automation only, without taking its attachment.
    ///
    /// The same thing `vvmux msg` does locally: a request needs a connection, not a terminal. This
    /// exists because an automation-scoped token must be able to drive a session without evicting
    /// whoever is sitting at it — attaching is exactly the authority that token does not have.
    pub(crate) async fn connect_for_automation(
        name: String,
        outbound_queue_bytes: usize,
    ) -> io::Result<(Self, String, bool)> {
        Self::open(name, None, outbound_queue_bytes).await
    }

    async fn open(
        name: String,
        attach: Option<(bool, DisplayMetrics, bool)>,
        outbound_queue_bytes: usize,
    ) -> io::Result<(Self, String, bool)> {
        // Whether media records are worth forwarding. An automation-only connection is never a
        // presenter, so it wants none of them.
        let vivid = attach.is_some_and(|(_, _, vivid)| vivid);
        let (mut reader, writer, session, text_only) = tokio::task::spawn_blocking(move || {
            let (mut reader, writer) = crate::server::connect(&name)?;
            let Some((takeover, display, vivid)) = attach else {
                return Ok((reader, writer, name, false));
            };
            writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .send(&ClientMessage::Attach {
                    replace: takeover,
                    target: crate::ipc::AttachmentTarget::Session,
                    display,
                    vivid,
                    kitty_graphics: false,
                    // A browser or tunnelled client is not hosted by a Vivido window, so there is
                    // no outer identity to publish and a pane agent must not be told there is one.
                    outer: None,
                })?;
            match reader.recv_server()? {
                ServerMessage::Attached { session, text_only } => {
                    Ok((reader, writer, session, text_only))
                }
                ServerMessage::Error(message) => {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, message))
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session returned an unexpected attachment reply",
                )),
            }
        })
        .await
        .map_err(|error| {
            io::Error::other(format!("session attachment worker failed: {error}"))
        })??;

        let cancel = reader.cancel_handle();
        let reader_cancel = cancel.clone();
        let (writer_sender, writer_receiver) = mpsc::sync_channel(WRITER_QUEUE_MESSAGES);
        let writer_thread = writer.clone();
        thread::Builder::new()
            .name("vvmux-gateway-ipc-writer".into())
            .spawn(move || {
                while let Ok(message) = writer_receiver.recv() {
                    if writer_thread
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .send_client(&message)
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        let maximum_messages = (outbound_queue_bytes / 1024).clamp(8, 4096);
        let (event_sender, event_receiver) = tokio_mpsc::channel(maximum_messages);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let overloaded = Arc::new(AtomicBool::new(false));
        let reader_overloaded = overloaded.clone();
        let reader_bytes = queued_bytes.clone();
        let media_writer = writer_sender.clone();
        thread::Builder::new()
            .name("vvmux-gateway-ipc-reader".into())
            .spawn(move || {
                while let Ok(message) = reader.recv_server() {
                    if !vivid && let ServerMessage::MediaRecord { delivery_id, .. } = &message {
                        let _ = media_writer.try_send(ClientMessage::BridgeMediaAck {
                            delivery_id: *delivery_id,
                            delivered: false,
                        });
                        reader_cancel.cancel();
                        break;
                    }
                    if !vivid && matches!(message, ServerMessage::MediaSnapshot { .. }) {
                        reader_cancel.cancel();
                        break;
                    }
                    let (message, size) = match store_message(message, outbound_queue_bytes) {
                        Ok(value) => value,
                        Err(_) => {
                            reader_overloaded.store(true, Ordering::Release);
                            reader_cancel.cancel();
                            break;
                        }
                    };
                    if !reserve_bytes(&reader_bytes, size, outbound_queue_bytes) {
                        reader_overloaded.store(true, Ordering::Release);
                        reader_cancel.cancel();
                        break;
                    }
                    let queued = QueuedServerMessage {
                        message: Some(message),
                        size,
                        queued_bytes: reader_bytes.clone(),
                    };
                    if event_sender.try_send(queued).is_err() {
                        reader_overloaded.store(true, Ordering::Release);
                        reader_cancel.cancel();
                        break;
                    }
                }
            })?;

        Ok((
            Self {
                name: session.clone(),
                writer: writer_sender,
                events: event_receiver,
                cancel,
                overloaded,
            },
            session,
            text_only,
        ))
    }

    pub(crate) fn send(&self, message: ClientMessage) -> io::Result<()> {
        self.writer.try_send(message).map_err(|error| {
            self.cancel.cancel();
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("session client queue is unavailable: {error}"),
            )
        })
    }

    pub(crate) fn bridge_sender(&self) -> crate::client::BridgeClientSender {
        let writer = self.writer.clone();
        let cancel = self.cancel.clone();
        let shutdown = cancel.clone();
        crate::client::BridgeClientSender::new(move |message| {
            writer.try_send(message).map_err(|error| {
                cancel.cancel();
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("session bridge queue is unavailable: {error}"),
                )
            })
        })
        .with_cancel(move || shutdown.cancel())
    }

    pub(crate) async fn recv(&mut self) -> Option<QueuedServerMessage> {
        self.events.recv().await
    }

    /// The session this connection holds, as the server resolved it.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for SessionAdapter {
    fn drop(&mut self) {
        let _ = self.writer.try_send(ClientMessage::Detach);
        self.cancel.cancel();
    }
}

enum StoredMessage {
    Binary(Box<ServerMessage>),
    Structured(Vec<u8>),
}

fn store_message(message: ServerMessage, maximum: usize) -> io::Result<(StoredMessage, usize)> {
    let overhead = std::mem::size_of::<ServerMessage>() + 256;
    if let ServerMessage::Render { bytes, .. } | ServerMessage::MediaRecord { bytes, .. } = &message
    {
        let size = overhead
            .checked_add(bytes.capacity())
            .filter(|size| *size <= maximum)
            .ok_or_else(|| io::Error::other("gateway message exceeds queue budget"))?;
        return Ok((StoredMessage::Binary(Box::new(message)), size));
    }
    struct Bounded {
        bytes: Vec<u8>,
        maximum: usize,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let next = self
                .bytes
                .len()
                .checked_add(bytes.len())
                .filter(|next| *next <= self.maximum)
                .ok_or_else(|| io::Error::other("gateway message exceeds queue budget"))?;
            if next > self.bytes.capacity() {
                let capacity = next
                    .max(self.bytes.capacity().saturating_mul(2))
                    .min(self.maximum);
                self.bytes
                    .try_reserve_exact(capacity - self.bytes.len())
                    .map_err(io::Error::other)?;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut encoded = Bounded {
        bytes: Vec::new(),
        maximum: maximum.saturating_sub(overhead),
    };
    serde_json::to_writer(&mut encoded, &message).map_err(io::Error::other)?;
    let size = overhead
        .checked_add(encoded.bytes.capacity())
        .filter(|size| *size <= maximum)
        .ok_or_else(|| io::Error::other("gateway message exceeds queue budget"))?;
    Ok((StoredMessage::Structured(encoded.bytes), size))
}

fn reserve_bytes(used: &AtomicUsize, size: usize, maximum: usize) -> bool {
    let mut current = used.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(size) else {
            return false;
        };
        if next > maximum {
            return false;
        }
        match used.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_reservation_is_bounded_and_checked() {
        let used = AtomicUsize::new(0);
        assert!(reserve_bytes(&used, 10, 12));
        assert!(!reserve_bytes(&used, 3, 12));
        assert_eq!(used.load(Ordering::Relaxed), 10);
        assert!(!reserve_bytes(&used, usize::MAX, usize::MAX));
    }

    #[test]
    fn automation_payload_is_charged_to_queue_bytes() {
        let message = ServerMessage::Automation(crate::ipc::AutomationResponse::success(
            1,
            serde_json::Value::String("x".repeat(256 * 1024)),
        ));
        let (_, charged) = store_message(message, 512 * 1024).unwrap();
        assert!(charged > 256 * 1024);
        let used = AtomicUsize::new(0);
        assert!(reserve_bytes(&used, charged, 512 * 1024));
        assert!(!reserve_bytes(&used, charged, 512 * 1024));
    }
}
