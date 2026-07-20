use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use vivid_protocol::wire::{
    HEADER_SIZE, PREFACE_SIZE, Preface, RECORD_KNOWN_FLAGS, Record, RecordHeader,
};
use vivid_protocol::{CONTROL_MAX_RECORD_BODY, FRAMING_MAJOR, FRAMING_MINOR, HARD_MAX_RECORD_BODY};

use crate::platform::{ConnectionCancel, Transport};

pub struct Reader {
    stream: Box<dyn Read + Send>,
    writer: Option<Box<dyn Write + Send>>,
    _cancel: ConnectionCancel,
    timeout: Arc<dyn Fn(Option<Duration>) -> io::Result<()> + Send + Sync>,
    deadline: Arc<Mutex<Option<std::time::Instant>>>,
    negotiated_maximum: u32,
    maximum: u32,
    sequence: u64,
}

impl Reader {
    pub fn new(mut transport: Transport) -> io::Result<(Self, Preface)> {
        let cancel = transport.cancel();
        let timeout = transport.timeout.clone();
        let deadline = transport.deadline.clone();
        let mut bytes = [0_u8; PREFACE_SIZE];
        transport.reader.read_exact(&mut bytes)?;
        let preface = Preface::decode(bytes)?;
        if (preface.major, preface.minor) != (FRAMING_MAJOR, FRAMING_MINOR) {
            return Err(invalid("unsupported Vivid framing version"));
        }
        let negotiated_maximum = preface.initiator_tx_body_limit.min(HARD_MAX_RECORD_BODY);
        let maximum = if preface.kind == vivid_protocol::wire::ConnectionKind::Control {
            negotiated_maximum.min(CONTROL_MAX_RECORD_BODY)
        } else {
            negotiated_maximum
        };
        Ok((
            Self {
                stream: transport.reader,
                writer: Some(transport.writer),
                _cancel: cancel,
                timeout,
                deadline,
                negotiated_maximum,
                maximum,
                sequence: 0,
            },
            preface,
        ))
    }

    pub fn read_record(&mut self) -> io::Result<Record> {
        let mut bytes = [0_u8; HEADER_SIZE];
        self.stream.read_exact(&mut bytes)?;
        let header = RecordHeader::decode(bytes);
        if header.flags & !RECORD_KNOWN_FLAGS != 0 {
            return Err(invalid("Vivid record uses reserved flags"));
        }
        if header.body_length > self.maximum || header.body_length > HARD_MAX_RECORD_BODY {
            return Err(invalid("Vivid record exceeds negotiated maximum"));
        }
        let expected = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("sequence exhausted"))?;
        if header.sequence != expected {
            return Err(invalid("Vivid record sequence gap"));
        }
        self.sequence = header.sequence;
        let mut body = vec![0; header.body_length as usize];
        self.stream.read_exact(&mut body)?;
        Ok(Record {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    pub fn writer(&mut self) -> io::Result<Writer> {
        let stream = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("Vivid writer was already taken"))?;
        Ok(Writer {
            inner: Mutex::new(WriterInner {
                stream,
                maximum: CONTROL_MAX_RECORD_BODY,
                sequence: 0,
            }),
        })
    }

    pub fn set_maximum(&mut self, maximum: u32) {
        self.maximum = self
            .negotiated_maximum
            .min(maximum)
            .min(HARD_MAX_RECORD_BODY);
    }

    pub fn clear_read_deadline(&self) -> io::Result<()> {
        *self
            .deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        (self.timeout)(None)
    }
}

pub struct Writer {
    inner: Mutex<WriterInner>,
}

struct WriterInner {
    stream: Box<dyn Write + Send>,
    maximum: u32,
    sequence: u64,
}

impl Writer {
    pub fn write_record(&self, record_type: u16, object_id: u64, body: &[u8]) -> io::Result<()> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if body.len() > inner.maximum as usize || body.len() > HARD_MAX_RECORD_BODY as usize {
            return Err(invalid("outgoing Vivid record exceeds maximum"));
        }
        inner.sequence = inner
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("sequence exhausted"))?;
        let header = RecordHeader {
            body_length: body.len() as u32,
            record_type,
            flags: 0,
            object_id,
            sequence: inner.sequence,
        };
        inner.stream.write_all(&header.encode())?;
        inner.stream.write_all(body)?;
        inner.stream.flush()
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
