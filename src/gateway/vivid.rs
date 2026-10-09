use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::transport::{Frame, FrameSink, FrameStream};
use base64::Engine;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use vivid_protocol::auth::Secret32;
use vivid_protocol::messages::LaneClass;
use vivid_protocol::wire::{Connection, ConnectionKind};
use zeroize::Zeroizing;

use vivid_sdk::ConnectionFactory;

pub(crate) const SUBPROTOCOL: &str = "vvmux.vivid.v1";
pub(crate) const CONNECTION_PROTOCOL_PREFIX: &str = "vvmux.connection.";
pub(crate) const AUTH_PROTOCOL_PREFIX: &str = "vvmux.auth.";
pub(crate) const KIND_PROTOCOL_PREFIX: &str = "vvmux.kind.";

/// How long a Vivid connection request waits for the browser to supply a socket.
const CONNECTION_WAIT: Duration = Duration::from_secs(30);
/// How long a Vivid WebSocket read or write may block before it fails.
const IO_WAIT: Duration = Duration::from_secs(30);
/// Chunks buffered between a Vivid socket and its bridge; small, so backpressure reaches the
/// producer quickly.
const CHANNEL_CHUNKS: usize = 4;
/// Largest Vivid WebSocket message, from the web profile.
pub(super) const MAX_SOCKET_CHUNK: usize = vivid_protocol::web::MAX_SOCKET_CHUNK as usize;

pub(crate) struct VividBroker {
    id: String,
    token: Zeroizing<[u8; 32]>,
    state: Mutex<BrokerState>,
    changed: Condvar,
    closed: AtomicBool,
    shutdown: tokio::sync::Notify,
}

#[derive(Default)]
struct BrokerState {
    available: HashMap<u8, VecDeque<VividSocketIo>>,
    next_transport: u64,
    closed: bool,
}

struct VividSocketIo {
    id: u64,
    reader: SocketReader,
    writer: SocketWriter,
}

struct IncomingChunk {
    bytes: Vec<u8>,
}

struct OutgoingChunk {
    bytes: Vec<u8>,
    completion: std_mpsc::SyncSender<io::Result<()>>,
}

struct SocketReader {
    receiver: mpsc::Receiver<IncomingChunk>,
    current: Vec<u8>,
    offset: usize,
}

struct SocketWriter {
    sender: mpsc::Sender<OutgoingChunk>,
}

impl VividBroker {
    pub(crate) fn new() -> io::Result<Arc<Self>> {
        let mut id = [0_u8; 16];
        let mut token = Zeroizing::new([0_u8; 32]);
        getrandom::fill(&mut id).map_err(|error| {
            io::Error::other(format!("could not generate Vivid route ID: {error}"))
        })?;
        getrandom::fill(token.as_mut()).map_err(|error| {
            io::Error::other(format!("could not generate Vivid route token: {error}"))
        })?;
        Ok(Arc::new(Self {
            id: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id),
            token,
            state: Mutex::new(BrokerState::default()),
            changed: Condvar::new(),
            closed: AtomicBool::new(false),
            shutdown: tokio::sync::Notify::new(),
        }))
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn encoded_token(&self) -> Zeroizing<String> {
        Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(*self.token))
    }

    /// The route secret is also the Vivid 1.5 root secret for this ephemeral broker session.
    ///
    /// Returning the non-debuggable protocol type avoids creating a printable credential.
    pub(crate) fn root_secret(&self) -> Secret32 {
        Secret32::new(*self.token)
    }

    pub(crate) fn authenticate(&self, submitted: &str) -> bool {
        let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(submitted) else {
            return false;
        };
        decoded.len() == self.token.len()
            && bool::from(decoded.as_slice().ct_eq(self.token.as_slice()))
    }

    fn register(&self, kind: ConnectionKind, mut io: VividSocketIo) -> io::Result<u64> {
        let mut state = self
            .state
            .lock()
            .map_err(|_poisoned| io::Error::other("Vivid broker lock is poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Vivid broker is closed",
            ));
        }
        state.next_transport = state.next_transport.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::OutOfMemory, "Vivid transport ID exhausted")
        })?;
        io.id = state.next_transport;
        let id = io.id;
        let queue = state.available.entry(kind as u8).or_default();
        if queue.len() >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "too many unclaimed Vivid WebSocket transports",
            ));
        }
        queue.push_back(io);
        self.changed.notify_all();
        Ok(id)
    }

    fn unregister(&self, kind: ConnectionKind, id: u64) {
        if let Ok(mut state) = self.state.lock()
            && let Some(queue) = state.available.get_mut(&(kind as u8))
            && let Some(index) = queue.iter().position(|transport| transport.id == id)
        {
            queue.remove(index);
        }
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.available.clear();
            self.changed.notify_all();
        }
        self.shutdown.notify_waiters();
    }

    fn wait_for_transport(&self, kind: ConnectionKind) -> io::Result<VividSocketIo> {
        let deadline = Instant::now() + CONNECTION_WAIT;
        let mut state = self
            .state
            .lock()
            .map_err(|_poisoned| io::Error::other("Vivid broker lock is poisoned"))?;
        loop {
            if let Some(io) = state
                .available
                .get_mut(&(kind as u8))
                .and_then(VecDeque::pop_front)
            {
                return Ok(io);
            }
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Vivid broker closed while waiting for a browser transport",
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("browser did not open Vivid connection kind {}", kind as u8),
                ));
            }
            let (next, timeout) = self
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .map_err(|_poisoned| io::Error::other("Vivid broker lock is poisoned"))?;
            state = next;
            if timeout.timed_out() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("browser did not open Vivid connection kind {}", kind as u8),
                ));
            }
        }
    }
}

impl ConnectionFactory for VividBroker {
    fn cancel(&self) {
        self.close();
    }
    fn open(&self, kind: ConnectionKind, _lane: Option<LaneClass>) -> io::Result<Connection> {
        if matches!(kind, ConnectionKind::Lane | ConnectionKind::FileTransfer) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the vvmux Vivid broker carries Control and Track connections only",
            ));
        }
        let io = self.wait_for_transport(kind)?;
        Connection::from_streams(Box::new(io.reader), Box::new(io.writer), kind)
    }
}

impl Read for SocketReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.offset == self.current.len() {
            let chunk = self.receiver.blocking_recv().ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "Vivid WebSocket input closed")
            })?;
            self.current = chunk.bytes;
            self.offset = 0;
        }
        let count = buf.len().min(self.current.len() - self.offset);
        buf[..count].copy_from_slice(&self.current[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

impl Write for SocketWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // A Vivid record can span many WebSocket messages. Accept a bounded prefix so
        // write_all preserves the byte stream without sending an oversized media message.
        let count = buf.len().min(MAX_SOCKET_CHUNK);
        let (completion, completed) = std_mpsc::sync_channel(1);
        self.sender
            .blocking_send(OutgoingChunk {
                bytes: buf[..count].to_vec(),
                completion,
            })
            .map_err(|_timeout| {
                io::Error::new(io::ErrorKind::BrokenPipe, "Vivid WebSocket output closed")
            })?;
        completed.recv_timeout(IO_WAIT).map_err(|_timeout| {
            io::Error::new(io::ErrorKind::TimedOut, "Vivid WebSocket write timed out")
        })??;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) async fn serve_socket<Si: FrameSink, St: FrameStream>(
    mut socket_writer: Si,
    mut socket_reader: St,
    broker: Arc<VividBroker>,
    kind: ConnectionKind,
) -> io::Result<()> {
    let (incoming_sender, incoming_receiver) = mpsc::channel(CHANNEL_CHUNKS);
    let (outgoing_sender, mut outgoing_receiver) = mpsc::channel(CHANNEL_CHUNKS);
    let transport_id = broker.register(
        kind,
        VividSocketIo {
            id: 0,
            reader: SocketReader {
                receiver: incoming_receiver,
                current: Vec::new(),
                offset: 0,
            },
            writer: SocketWriter {
                sender: outgoing_sender,
            },
        },
    )?;

    let (pong_sender, mut pong_receiver) = mpsc::channel(CHANNEL_CHUNKS);
    let read_socket = async move {
        while let Some(incoming) = socket_reader.next_frame().await {
            match incoming {
                Ok(Frame::Binary(bytes)) => {
                    // Waiting for capacity pauses only this WebSocket's input. The independent
                    // writer below remains live, so bounded backpressure cannot deadlock a
                    // correlated reply behind presenter output.
                    incoming_sender
                        .send(IncomingChunk {
                            bytes: bytes.to_vec(),
                        })
                        .await
                        .map_err(|_closed| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "Vivid WebSocket input reader closed",
                            )
                        })?;
                }
                Ok(Frame::Ping(bytes)) => {
                    pong_sender.send(bytes).await.map_err(|_closed| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "Vivid WebSocket output closed")
                    })?;
                }
                Ok(Frame::Pong(_)) => {}
                Ok(Frame::Close(_)) => return Ok(()),
                Ok(Frame::Text(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "text is forbidden on an established Vivid WebSocket",
                    ));
                }
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        Ok(())
    };
    let write_socket = async move {
        loop {
            tokio::select! {
                outgoing = outgoing_receiver.recv() => match outgoing {
                    Some(chunk) => {
                        let result = socket_writer
                            .send_frame(Frame::Binary(chunk.bytes.into()))
                            .await;
                        let report = result.as_ref().copied().map_err(|error| {
                            io::Error::new(error.kind(), error.to_string())
                        });
                        let _ = chunk.completion.send(report);
                        result?;
                    }
                    None => return Ok(()),
                },
                pong = pong_receiver.recv() => match pong {
                    Some(bytes) => socket_writer.send_frame(Frame::Pong(bytes)).await?,
                    None => return Ok(()),
                }
            }
        }
    };
    let shutdown = broker.shutdown.notified();
    tokio::pin!(shutdown);
    let result = if broker.closed.load(Ordering::Acquire) {
        Ok(())
    } else {
        tokio::select! {
            () = &mut shutdown => Ok(()),
            result = read_socket => result,
            result = write_socket => result,
        }
    };
    broker.unregister(kind, transport_id);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn route_tokens_are_exact_and_constant_time_compared() {
        let broker = VividBroker::new().unwrap();
        let token = broker.encoded_token();
        assert!(broker.authenticate(&token));
        assert!(!broker.authenticate(&"A".repeat(token.len())));
        assert!(!broker.authenticate("invalid"));
    }

    #[test]
    fn gateway_transport_preserves_negotiation_bytes_exactly() {
        // Includes the exact 1.5 preface followed by opaque control bytes. The broker must not
        // parse, normalize, or otherwise learn anything from an authenticated Vivid transcript.
        let mut hello = vivid_protocol::wire::encode_preface(
            ConnectionKind::Control,
            vivid_protocol::CONTROL_MAX_RECORD_BODY,
        )
        .to_vec();
        hello.extend_from_slice(&[
            0x00, 0x00, 0x00, 0x07, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xa2, 0x00, 0x01, 0x2a, 0x82, 0x01, 0xf5,
        ]);

        let (incoming_sender, incoming_receiver) = mpsc::channel(hello.len().div_ceil(7));
        for chunk in hello.chunks(7) {
            incoming_sender
                .blocking_send(IncomingChunk {
                    bytes: chunk.to_vec(),
                })
                .unwrap();
        }
        let mut reader = SocketReader {
            receiver: incoming_receiver,
            current: Vec::new(),
            offset: 0,
        };
        let mut read = vec![0; hello.len()];
        reader.read_exact(&mut read).unwrap();
        assert_eq!(read, hello);
        drop(incoming_sender);

        let (outgoing_sender, mut outgoing_receiver) =
            mpsc::channel::<OutgoingChunk>(CHANNEL_CHUNKS);
        let expected = hello.clone();
        let relay = thread::spawn(move || {
            let chunk = outgoing_receiver.blocking_recv().unwrap();
            assert_eq!(chunk.bytes, expected);
            chunk.completion.send(Ok(())).unwrap();
        });
        let mut writer = SocketWriter {
            sender: outgoing_sender,
        };
        writer.write_all(&hello).unwrap();
        relay.join().unwrap();
    }

    #[test]
    fn browser_reply_bursts_wait_for_the_reader_without_losing_chunks() {
        let (incoming_sender, incoming_receiver) = mpsc::channel(CHANNEL_CHUNKS);
        let (completed_sender, completed_receiver) = std_mpsc::sync_channel(1);
        let burst = CHANNEL_CHUNKS + 3;
        let sender = thread::spawn(move || {
            for value in 0..burst {
                incoming_sender
                    .blocking_send(IncomingChunk {
                        bytes: vec![value.try_into().unwrap()],
                    })
                    .unwrap();
            }
            completed_sender.send(()).unwrap();
        });

        let mut reader = SocketReader {
            receiver: incoming_receiver,
            current: Vec::new(),
            offset: 0,
        };
        for expected in 0..burst {
            let mut byte = [0];
            reader.read_exact(&mut byte).unwrap();
            assert_eq!(usize::from(byte[0]), expected);
        }

        completed_receiver.recv_timeout(IO_WAIT).unwrap();
        sender.join().unwrap();
    }

    #[test]
    fn large_media_writes_preserve_bytes_within_websocket_chunk_ceiling() {
        let maximum = vivid_protocol::web::MAX_SOCKET_CHUNK as usize;
        for length in [maximum, maximum + 1, maximum * 3 + 7] {
            let bytes: Vec<u8> = (0_u8..=255).cycle().take(length).collect();
            let expected = bytes.clone();
            let (sender, mut receiver) = mpsc::channel::<OutgoingChunk>(CHANNEL_CHUNKS);
            let relay = thread::spawn(move || {
                for expected_chunk in expected.chunks(maximum) {
                    let chunk = receiver.blocking_recv().unwrap();
                    assert_eq!(chunk.bytes, expected_chunk);
                    chunk.completion.send(Ok(())).unwrap();
                }
                assert!(receiver.blocking_recv().is_none());
            });
            let mut writer = SocketWriter { sender };
            writer.write_all(&bytes).unwrap();
            drop(writer);
            relay.join().unwrap();
        }
    }
}
