use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::platform::{ConnectionCancel, Transport};

pub const MAGIC: &[u8; 4] = b"VVMX";
pub const VERSION: u16 = 3;
pub const CONTROL_MAX_BODY: u32 = 1024 * 1024;
pub const BULK_MAX_BODY: u32 = 64 * 1024 * 1024;
const STRUCTURED_RECORD: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChannelKind {
    Control = 1,
    Bulk = 2,
}

impl ChannelKind {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Control),
            2 => Ok(Self::Bulk),
            _ => Err(invalid("unknown VVMX channel kind")),
        }
    }

    fn maximum(self) -> u32 {
        match self {
            Self::Control => CONTROL_MAX_BODY,
            Self::Bulk => BULK_MAX_BODY,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Action {
    Split(Axis),
    Focus(Direction),
    Resize(Direction),
    NewTab,
    NextTab,
    PreviousTab,
    SelectTab(usize),
    ClosePane,
    ToggleZoom,
    EnterCopyMode,
    CopyInput(Vec<u8>),
    Paste,
    NewFloatingPane,
    ToggleFloatingPanes,
    TogglePanePinned,
    EnterFloatingMoveMode,
    EnterFloatingResizeMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FloatingEditKind {
    Move,
    Resize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FloatingEditCommand {
    /// One keyboard edit step; `cells` is validated to 1 (plain arrow) or 5 (Shift-arrow).
    Step {
        direction: Direction,
        cells: u8,
    },
    Commit,
    Cancel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DisplayMetrics {
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Move,
    Wheel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: u8,
    pub x: u16,
    pub y: u16,
    pub kind: MouseKind,
    pub shift: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BridgeSourceKey {
    pub producer: u64,
    pub source: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BridgeSourceKind {
    Raster {
        width: u32,
        height: u32,
        alpha_mode: u64,
        compression_mode: u64,
    },
    Image {
        encoding: u64,
        width: u32,
        height: u32,
        encoded_length: u32,
        sha256: Option<[u8; 32]>,
    },
    Video {
        codec: String,
        packetization: String,
        extradata: Vec<u8>,
        width: u32,
        height: u32,
        profile: i32,
        level: i32,
        bitrate: u64,
        color_primaries: u64,
        transfer: u64,
        matrix: u64,
        range: u64,
        sar_num: u32,
        sar_den: u32,
        max_access_unit_bytes: u32,
    },
    Audio {
        linked_video: Option<BridgeSourceKey>,
        codec: String,
        packetization: String,
        extradata: Vec<u8>,
        sample_rate: u32,
        channels: u16,
        channel_mask: u64,
        bitrate: u64,
        max_access_unit_bytes: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeSource {
    pub key: BridgeSourceKey,
    pub kind: BridgeSourceKind,
    pub playing: bool,
    pub play_request: BridgePlayRequest,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgePlayRequest {
    pub start_pts_us: i64,
    pub minimum_buffer_us: u64,
    pub maximum_latency_us: u64,
    pub rate_32_32: i64,
    pub late_policy: u64,
    pub loop_count: u64,
    pub start_policy: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeClipRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeNode {
    pub producer: u64,
    pub node: u64,
    /// Stable fragment identity within one logical `(producer, node)`.
    pub fragment: u8,
    pub source: BridgeSourceKey,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    pub z_index: i64,
    pub visible: bool,
    pub clip: BridgeClipRect,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMessage {
    Attach {
        replace: bool,
        display: DisplayMetrics,
        vivid: bool,
    },
    Input(Vec<u8>),
    Mouse(MouseEvent),
    Resize(DisplayMetrics),
    Action(Action),
    RenderAck(u64),
    BridgeNeedKeyframes(Vec<BridgeSourceKey>),
    BridgeMediaAck {
        delivery_id: u64,
        delivered: bool,
    },
    BridgeSnapshotRetry,
    /// Keyboard float-edit input, valid only while the actor-confirmed mode `mode_id` is
    /// current; the actor ignores stale IDs.
    FloatingEdit {
        mode_id: u64,
        command: FloatingEditCommand,
    },
    Detach,
    Kill,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMessage {
    Attached {
        session: String,
        text_only: bool,
    },
    Render {
        frame_id: u64,
        full: bool,
        last: bool,
        bytes: Vec<u8>,
    },
    Title(String),
    Bell,
    Clipboard(String),
    Status(String),
    MediaSnapshot {
        revision: u64,
        sources: Vec<BridgeSource>,
        nodes: Vec<BridgeNode>,
        videos_needing_keyframes: Vec<BridgeSourceKey>,
    },
    MediaRecord {
        delivery_id: u64,
        source: BridgeSourceKey,
        record_type: u16,
        offset: u32,
        total: u32,
        last: bool,
        bytes: Vec<u8>,
    },
    Detached {
        reason: String,
    },
    /// Authoritative float-edit mode state: `pane`/`kind` are set while a mode is active and
    /// `None` after it ends. The client parses arrows/Enter/Escape only while a mode with this
    /// `mode_id` is active.
    FloatingEditMode {
        mode_id: u64,
        pane: Option<u64>,
        kind: Option<FloatingEditKind>,
    },
    Error(String),
    Pong,
}

pub struct RecordReader {
    stream: Box<dyn Read + Send>,
    expected_sequence: u64,
    maximum_body: u32,
    cancel: ConnectionCancel,
}

pub struct RecordWriter {
    stream: Box<dyn Write + Send>,
    next_sequence: u64,
    maximum_body: u32,
}

pub type SharedWriter = Arc<Mutex<RecordWriter>>;

pub fn establish(
    mut stream: Transport,
    channel: ChannelKind,
) -> io::Result<(RecordReader, SharedWriter)> {
    let preface = encode_preface(channel, channel.maximum());
    stream.writer.write_all(&preface)?;
    let mut peer = [0_u8; 12];
    stream.reader.read_exact(&mut peer)?;
    let (peer_channel, peer_maximum) = decode_preface(&peer)?;
    if peer_channel != channel {
        return Err(invalid("VVMX channel mismatch"));
    }
    let maximum = channel.maximum().min(peer_maximum);
    let cancel = stream.cancel();
    Ok((
        RecordReader {
            stream: stream.reader,
            expected_sequence: 0,
            maximum_body: maximum,
            cancel,
        },
        Arc::new(Mutex::new(RecordWriter {
            stream: stream.writer,
            next_sequence: 0,
            maximum_body: maximum,
        })),
    ))
}

impl RecordReader {
    pub fn cancel_handle(&self) -> ConnectionCancel {
        self.cancel.clone()
    }

    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        let (record_type, flags, body) = self.read_raw()?;
        if record_type != STRUCTURED_RECORD || flags != 0 {
            return Err(invalid("unexpected VVMX control record"));
        }
        serde_json::from_slice(&body).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed VVMX message: {error}"),
            )
        })
    }

    pub fn read_raw(&mut self) -> io::Result<(u16, u16, Vec<u8>)> {
        let mut header = [0_u8; 16];
        self.stream.read_exact(&mut header)?;
        let sequence = u64::from_be_bytes(header[0..8].try_into().unwrap());
        let record_type = u16::from_be_bytes(header[8..10].try_into().unwrap());
        let flags = u16::from_be_bytes(header[10..12].try_into().unwrap());
        let length = u32::from_be_bytes(header[12..16].try_into().unwrap());
        if sequence != self.expected_sequence {
            return Err(invalid("VVMX record sequence gap"));
        }
        if flags & !0x0001 != 0 {
            return Err(invalid("VVMX record uses reserved flags"));
        }
        if length > self.maximum_body {
            return Err(invalid("VVMX record body exceeds negotiated limit"));
        }
        let mut body = vec![0; length as usize];
        self.stream.read_exact(&mut body)?;
        self.expected_sequence = self.expected_sequence.wrapping_add(1);
        Ok((record_type, flags, body))
    }
}

impl RecordWriter {
    pub fn send<T: Serialize>(&mut self, message: &T) -> io::Result<()> {
        let body = serde_json::to_vec(message).map_err(io::Error::other)?;
        self.write_raw(STRUCTURED_RECORD, 0, &body)
    }

    pub fn write_raw(&mut self, record_type: u16, flags: u16, body: &[u8]) -> io::Result<()> {
        if flags & !0x0001 != 0 {
            return Err(invalid("VVMX record uses reserved flags"));
        }
        if body.len() > self.maximum_body as usize {
            return Err(invalid("VVMX record body exceeds negotiated limit"));
        }
        let mut header = [0_u8; 16];
        header[0..8].copy_from_slice(&self.next_sequence.to_be_bytes());
        header[8..10].copy_from_slice(&record_type.to_be_bytes());
        header[10..12].copy_from_slice(&flags.to_be_bytes());
        header[12..16].copy_from_slice(&(body.len() as u32).to_be_bytes());
        self.stream.write_all(&header)?;
        self.stream.write_all(body)?;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        Ok(())
    }
}

pub fn send(writer: &SharedWriter, message: &ServerMessage) -> io::Result<()> {
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(message)
}

fn encode_preface(channel: ChannelKind, maximum_body: u32) -> [u8; 12] {
    let mut preface = [0_u8; 12];
    preface[0..4].copy_from_slice(MAGIC);
    preface[4..6].copy_from_slice(&VERSION.to_be_bytes());
    preface[6] = channel as u8;
    preface[7] = 0;
    preface[8..12].copy_from_slice(&maximum_body.to_be_bytes());
    preface
}

fn decode_preface(preface: &[u8; 12]) -> io::Result<(ChannelKind, u32)> {
    if &preface[0..4] != MAGIC {
        return Err(invalid("bad VVMX magic"));
    }
    if u16::from_be_bytes(preface[4..6].try_into().unwrap()) != VERSION {
        return Err(invalid(
            "unsupported VVMX protocol version; restart the vvmux client and session server",
        ));
    }
    if preface[7] != 0 {
        return Err(invalid("VVMX preface reserved byte is nonzero"));
    }
    let channel = ChannelKind::from_byte(preface[6])?;
    let maximum = u32::from_be_bytes(preface[8..12].try_into().unwrap());
    if maximum == 0 || maximum > channel.maximum() {
        return Err(invalid("invalid VVMX maximum body"));
    }
    Ok((channel, maximum))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preface_rejects_version_reserved_and_limits() {
        let mut preface = encode_preface(ChannelKind::Control, CONTROL_MAX_BODY);
        assert_eq!(decode_preface(&preface).unwrap().0, ChannelKind::Control);
        preface[7] = 1;
        assert!(decode_preface(&preface).is_err());
        preface = encode_preface(ChannelKind::Control, CONTROL_MAX_BODY);
        preface[4..6].copy_from_slice(&VERSION.wrapping_add(1).to_be_bytes());
        let error = decode_preface(&preface).unwrap_err();
        assert!(error.to_string().contains("restart"));
    }

    #[test]
    fn structured_records_round_trip_and_sequence_is_checked() {
        use std::net::{Ipv4Addr, TcpListener, TcpStream};

        fn transport(stream: TcpStream) -> Transport {
            let reader = stream.try_clone().unwrap();
            Transport::new(
                Box::new(reader),
                Box::new(stream),
                ConnectionCancel::inert(),
                Arc::new(|_| Ok(())),
            )
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let right = TcpStream::connect(address).unwrap();
        let (left, _) = listener.accept().unwrap();
        let server =
            std::thread::spawn(move || establish(transport(left), ChannelKind::Control).unwrap());
        let (mut client_reader, client_writer) =
            establish(transport(right), ChannelKind::Control).unwrap();
        let (mut server_reader, server_writer) = server.join().unwrap();
        client_writer
            .lock()
            .unwrap()
            .send(&ClientMessage::Ping)
            .unwrap();
        assert_eq!(
            server_reader.recv::<ClientMessage>().unwrap(),
            ClientMessage::Ping
        );
        server_writer
            .lock()
            .unwrap()
            .send(&ServerMessage::Pong)
            .unwrap();
        assert_eq!(
            client_reader.recv::<ServerMessage>().unwrap(),
            ServerMessage::Pong
        );
    }
}
