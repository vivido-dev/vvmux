use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;

use crate::ipc::{ClientMessage, DisplayMetrics, ServerMessage};

const WRITER_QUEUE_MESSAGES: usize = 256;

pub(crate) struct SessionAdapter {
    writer: mpsc::SyncSender<ClientMessage>,
    events: tokio_mpsc::Receiver<QueuedServerMessage>,
    cancel: crate::platform::ConnectionCancel,
    overloaded: Arc<AtomicBool>,
}

pub(crate) struct QueuedServerMessage {
    message: Option<ServerMessage>,
    size: usize,
    queued_bytes: Arc<AtomicUsize>,
}

impl QueuedServerMessage {
    pub(crate) fn take(&mut self) -> ServerMessage {
        self.message
            .take()
            .expect("queued server message is present")
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
        let (mut reader, writer, session, text_only) = tokio::task::spawn_blocking(move || {
            let (mut reader, writer) = crate::server::connect(&name)?;
            writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .send(&ClientMessage::Attach {
                    replace: takeover,
                    display,
                    vivid,
                })?;
            match reader.recv::<ServerMessage>()? {
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
                        .send(&message)
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
                while let Ok(message) = reader.recv::<ServerMessage>() {
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
                    let size = message_size(&message);
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
        crate::client::BridgeClientSender::new(move |message| {
            writer.try_send(message).map_err(|error| {
                cancel.cancel();
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("session bridge queue is unavailable: {error}"),
                )
            })
        })
    }

    pub(crate) async fn recv(&mut self) -> Option<QueuedServerMessage> {
        self.events.recv().await
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

fn message_size(message: &ServerMessage) -> usize {
    const OVERHEAD: usize = 256;
    OVERHEAD
        + match message {
            ServerMessage::Render { bytes, .. } | ServerMessage::MediaRecord { bytes, .. } => {
                bytes.len()
            }
            ServerMessage::Title(value)
            | ServerMessage::Clipboard(value)
            | ServerMessage::Status(value)
            | ServerMessage::Error(value) => value.len(),
            ServerMessage::Detached { reason } => reason.len(),
            ServerMessage::MediaSnapshot { sources, nodes, .. } => {
                sources.len().saturating_mul(1024) + nodes.len().saturating_mul(256)
            }
            _ => 0,
        }
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
    fn message_size_counts_render_payloads() {
        assert_eq!(
            message_size(&ServerMessage::Render {
                frame_id: 1,
                session_sequence: 1,
                full: true,
                last: true,
                bytes: vec![0; 10],
            }),
            266
        );
    }
}
