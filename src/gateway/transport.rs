//! Carrier-neutral framing for the VVWS/1 control loop and the Vivid relay.
//!
//! The gateway loops used to name `axum::extract::ws::WebSocket` directly, which
//! tied them to an accepted inbound connection. They now speak [`Frame`] over the
//! [`FrameSink`] and [`FrameStream`] halves, so the same loops can be driven by an
//! accepted socket today and by an outbound tunnel leg once connect mode exists.
//!
//! Nothing about VVWS/1 or Vivid changes here. This is a transport seam, not a
//! protocol one: the state machine, the input parser, the liveness and contention
//! rules, and the byte-transparent Vivid relay are all untouched.

use std::future::Future;
use std::io;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};

/// A payload owned by a frame. Cheap to move; conversion to and from the axum
/// representation does not copy.
pub(crate) type Payload = axum::body::Bytes;

/// One WebSocket frame, independent of the implementation carrying it.
#[derive(Debug, Clone)]
pub(crate) enum Frame {
    Text(String),
    Binary(Payload),
    Ping(Payload),
    Pong(Payload),
    Close(Option<(u16, &'static str)>),
}

/// The write half of a carrier.
pub(crate) trait FrameSink: Send + Unpin + 'static {
    fn send_frame(&mut self, frame: Frame) -> impl Future<Output = io::Result<()>> + Send;
}

/// The read half of a carrier.
///
/// **`next_frame` must be cancel-safe.** The control loop polls it inside
/// `tokio::select!` alongside the session channel and the tick timer, so a future
/// dropped mid-poll must not have consumed a frame. Every implementation here
/// delegates to a cancel-safe primitive; one that buffered internally without
/// restoring on drop would silently lose terminal input, which no test would
/// obviously catch.
pub(crate) trait FrameStream: Send + Unpin + 'static {
    fn next_frame(&mut self) -> impl Future<Output = Option<io::Result<Frame>>> + Send;
}

impl From<Frame> for Message {
    fn from(frame: Frame) -> Self {
        match frame {
            Frame::Text(text) => Message::Text(text.into()),
            Frame::Binary(bytes) => Message::Binary(bytes),
            Frame::Ping(bytes) => Message::Ping(bytes),
            Frame::Pong(bytes) => Message::Pong(bytes),
            Frame::Close(None) => Message::Close(None),
            Frame::Close(Some((code, reason))) => Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })),
        }
    }
}

impl From<Message> for Frame {
    fn from(message: Message) -> Self {
        match message {
            Message::Text(text) => Frame::Text(text.as_str().to_owned()),
            Message::Binary(bytes) => Frame::Binary(bytes),
            Message::Ping(bytes) => Frame::Ping(bytes),
            Message::Pong(bytes) => Frame::Pong(bytes),
            // A peer's close reason is its own text and is not retained: the loop
            // treats any close as a close, and the static reasons here are ours.
            Message::Close(_) => Frame::Close(None),
        }
    }
}

/// The accepted-socket write half.
pub(crate) struct AxumSink(pub(crate) SplitSink<WebSocket, Message>);

/// The accepted-socket read half.
pub(crate) struct AxumStream(pub(crate) SplitStream<WebSocket>);

/// Split an accepted socket into the two halves the gateway loops consume.
pub(crate) fn split(socket: WebSocket) -> (AxumSink, AxumStream) {
    let (sink, stream) = socket.split();
    (AxumSink(sink), AxumStream(stream))
}

impl FrameSink for AxumSink {
    async fn send_frame(&mut self, frame: Frame) -> io::Result<()> {
        self.0
            .send(Message::from(frame))
            .await
            .map_err(io::Error::other)
    }
}

impl FrameStream for AxumStream {
    // `StreamExt::next` is cancel-safe, which is what the trait requires.
    async fn next_frame(&mut self) -> Option<io::Result<Frame>> {
        self.0
            .next()
            .await
            .map(|message| message.map(Frame::from).map_err(io::Error::other))
    }
}

/// The stream type an outbound tunnel leg connects with.
pub(crate) type TunnelStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The outbound-leg write half.
type TungsteniteSinkInner =
    futures_util::stream::SplitSink<TunnelStream, tokio_tungstenite::tungstenite::Message>;
pub(crate) struct TungsteniteSink(pub(crate) TungsteniteSinkInner);

/// The outbound-leg read half.
type TungsteniteStreamInner = futures_util::stream::SplitStream<TunnelStream>;
pub(crate) struct TungsteniteStream(pub(crate) TungsteniteStreamInner);

impl FrameSink for TungsteniteSink {
    async fn send_frame(&mut self, frame: Frame) -> io::Result<()> {
        let message = match frame {
            Frame::Text(text) => tokio_tungstenite::tungstenite::Message::text(text),
            // tungstenite 0.29 carries `bytes::Bytes`, the same type as the axum
            // payload, so binary frames cross the seam without copying.
            Frame::Binary(bytes) => tokio_tungstenite::tungstenite::Message::Binary(bytes),
            Frame::Ping(bytes) => tokio_tungstenite::tungstenite::Message::Ping(bytes),
            Frame::Pong(bytes) => tokio_tungstenite::tungstenite::Message::Pong(bytes),
            Frame::Close(None) => tokio_tungstenite::tungstenite::Message::Close(None),
            Frame::Close(Some((code, reason))) => {
                let code =
                    tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(code);
                tokio_tungstenite::tungstenite::Message::Close(Some(
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code,
                        reason: reason.into(),
                    },
                ))
            }
        };
        self.0.send(message).await.map_err(io::Error::other)
    }
}

impl FrameStream for TungsteniteStream {
    // `StreamExt::next` is cancel-safe, which is what the trait requires.
    async fn next_frame(&mut self) -> Option<io::Result<Frame>> {
        self.0.next().await.map(|message| {
            message
                .map_err(io::Error::other)
                .and_then(|message| match message {
                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                        Ok(Frame::Text(text.as_str().to_owned()))
                    }
                    tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                        Ok(Frame::Binary(bytes))
                    }
                    tokio_tungstenite::tungstenite::Message::Ping(bytes) => Ok(Frame::Ping(bytes)),
                    tokio_tungstenite::tungstenite::Message::Pong(bytes) => Ok(Frame::Pong(bytes)),
                    // A peer's close text is not retained, matching the axum path.
                    tokio_tungstenite::tungstenite::Message::Close(_) => Ok(Frame::Close(None)),
                    tokio_tungstenite::tungstenite::Message::Frame(_) => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "raw frame reached the message layer",
                    )),
                })
        })
    }
}

/// VVTUN WebTransport leg representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuicMapping {
    Framed,
    RawBinary,
}

pub(crate) struct QuicSink {
    send: wtransport::SendStream,
    mapping: QuicMapping,
}

pub(crate) struct QuicStream {
    recv: wtransport::RecvStream,
    mapping: QuicMapping,
    buffer: Vec<u8>,
}

pub(crate) fn quic(
    send: wtransport::SendStream,
    recv: wtransport::RecvStream,
    mapping: QuicMapping,
) -> (QuicSink, QuicStream) {
    (
        QuicSink { send, mapping },
        QuicStream {
            recv,
            mapping,
            buffer: vec![0_u8; 64 * 1024],
        },
    )
}

impl FrameSink for QuicSink {
    async fn send_frame(&mut self, frame: Frame) -> io::Result<()> {
        if self.mapping == QuicMapping::RawBinary {
            return match frame {
                Frame::Binary(bytes) => self.send.write_all(&bytes).await.map_err(io::Error::other),
                Frame::Close(_) => self.send.finish().await.map_err(io::Error::other),
                Frame::Ping(_) | Frame::Pong(_) => Ok(()),
                Frame::Text(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "text frame on raw Vivid WebTransport leg",
                )),
            };
        }
        let (kind, bytes): (u8, Payload) = match frame {
            Frame::Text(text) => (1, Payload::copy_from_slice(text.as_bytes())),
            Frame::Binary(bytes) => (2, bytes),
            Frame::Ping(bytes) => (3, bytes),
            Frame::Pong(bytes) => (4, bytes),
            Frame::Close(_) => (5, Payload::new()),
        };
        if bytes.len() > super::protocol::MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VVTUN leg frame too large",
            ));
        }
        let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
        let mut header = [0_u8; 5];
        header[0] = kind;
        header[1..].copy_from_slice(&length.to_be_bytes());
        self.send
            .write_all(&header)
            .await
            .map_err(io::Error::other)?;
        self.send.write_all(&bytes).await.map_err(io::Error::other)
    }
}

impl FrameStream for QuicStream {
    async fn next_frame(&mut self) -> Option<io::Result<Frame>> {
        if self.mapping == QuicMapping::RawBinary {
            return match self.recv.read(&mut self.buffer).await {
                Ok(Some(0)) => Some(Ok(Frame::Binary(Payload::new()))),
                Ok(Some(count)) => Some(Ok(Frame::Binary(Payload::copy_from_slice(
                    &self.buffer[..count],
                )))),
                Ok(None) => None,
                Err(error) => Some(Err(io::Error::other(error))),
            };
        }
        let mut header = [0_u8; 5];
        if let Err(error) = self.recv.read_exact(&mut header).await {
            return Some(Err(io::Error::other(error)));
        }
        let length = u32::from_be_bytes(header[1..].try_into().expect("four-byte length")) as usize;
        if length > super::protocol::MAX_FRAME_BYTES || (header[0] == 5 && length != 0) {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid VVTUN leg frame length",
            )));
        }
        let mut bytes = vec![0_u8; length];
        if let Err(error) = self.recv.read_exact(&mut bytes).await {
            return Some(Err(io::Error::other(error)));
        }
        let bytes = Payload::from(bytes);
        Some(match header[0] {
            1 => String::from_utf8(bytes.to_vec())
                .map(Frame::Text)
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 text frame")
                }),
            2 => Ok(Frame::Binary(bytes)),
            3 => Ok(Frame::Ping(bytes)),
            4 => Ok(Frame::Pong(bytes)),
            5 => Ok(Frame::Close(None)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown VVTUN leg frame kind",
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_through_the_axum_representation() {
        let cases = [
            Frame::Text("hello".into()),
            Frame::Binary(Payload::from_static(b"VIVD")),
            Frame::Ping(Payload::from_static(b"p")),
            Frame::Pong(Payload::from_static(b"p")),
        ];
        for frame in cases {
            let restored = Frame::from(Message::from(frame.clone()));
            assert_eq!(
                format!("{frame:?}"),
                format!("{restored:?}"),
                "frame must survive the carrier representation unchanged"
            );
        }
    }

    #[test]
    fn a_close_frame_carries_our_code_outward_and_no_peer_text_inward() {
        let Message::Close(Some(frame)) = Message::from(Frame::Close(Some((1013, "too slow"))))
        else {
            panic!("close frame lost its payload")
        };
        assert_eq!(frame.code, 1013);
        assert_eq!(frame.reason.as_str(), "too slow");

        assert!(
            matches!(
                Frame::from(Message::Close(Some(CloseFrame {
                    code: 1000,
                    reason: "peer supplied".into(),
                }))),
                Frame::Close(None)
            ),
            "a peer's close text is not retained"
        );
    }
}
