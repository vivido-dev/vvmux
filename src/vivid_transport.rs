use std::io::{self, IoSlice, Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use vivid_protocol::messages;
use vivid_protocol::wire::{
    BorrowedRecord, HEADER_SIZE, PREFACE_SIZE, Preface, RECORD_KNOWN_FLAGS, Record, RecordHeader,
};
use vivid_protocol::{CONTROL_MAX_RECORD_BODY, HARD_MAX_RECORD_BODY, VIVID_MAJOR, VIVID_MINOR};

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
        if (preface.major, preface.minor) != (VIVID_MAJOR, VIVID_MINOR) {
            return Err(invalid("unsupported Vivid version"));
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
        let mut body = Vec::new();
        let header = self.read_record_body_into(&mut body)?;
        Ok(Record {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    pub fn read_record_into<'a>(
        &mut self,
        body: &'a mut Vec<u8>,
    ) -> io::Result<BorrowedRecord<'a>> {
        let header = self.read_record_body_into(body)?;
        Ok(BorrowedRecord {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    fn read_record_body_into(&mut self, body: &mut Vec<u8>) -> io::Result<RecordHeader> {
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
        body.resize(header.body_length as usize, 0);
        self.stream.read_exact(body)?;
        Ok(header)
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
            control_body: Mutex::new(Vec::with_capacity(64)),
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
    control_body: Mutex<Vec<u8>>,
}

struct WriterInner {
    stream: Box<dyn Write + Send>,
    maximum: u32,
    sequence: u64,
}

impl Writer {
    pub fn write_record(&self, record_type: u16, object_id: u64, body: &[u8]) -> io::Result<()> {
        self.write_record_sequence(record_type, object_id, body)
            .map(|_| ())
    }

    pub fn write_record_sequence(
        &self,
        record_type: u16,
        object_id: u64,
        body: &[u8],
    ) -> io::Result<u64> {
        self.write_record_parts_sequence(record_type, object_id, &[body])
    }

    pub fn write_record_parts_sequence(
        &self,
        record_type: u16,
        object_id: u64,
        parts: &[&[u8]],
    ) -> io::Result<u64> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let body_length = parts.iter().try_fold(0_usize, |total, part| {
            total.checked_add(part.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "record body length overflows")
            })
        })?;
        if body_length > inner.maximum as usize || body_length > HARD_MAX_RECORD_BODY as usize {
            return Err(invalid("outgoing Vivid record exceeds maximum"));
        }
        inner.sequence = inner
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("sequence exhausted"))?;
        let header = RecordHeader {
            body_length: body_length as u32,
            record_type,
            flags: 0,
            object_id,
            sequence: inner.sequence,
        };
        write_parts(inner.stream.as_mut(), &header.encode(), parts)?;
        inner.stream.flush()?;
        Ok(inner.sequence)
    }

    pub fn write_ok(&self, record_type: u16, object_id: u64, request_id: u64) -> io::Result<()> {
        let mut body = self
            .control_body
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        messages::ok_into(&mut body, request_id);
        self.write_record(record_type, object_id, &body)
    }

    pub fn write_pong(&self, request_id: u64) -> io::Result<()> {
        let mut body = self
            .control_body
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        messages::pong_into(&mut body, request_id);
        self.write_record(messages::PONG, 0, &body)
    }

    pub fn write_credit(
        &self,
        object_id: u64,
        bytes: u64,
        packets: u64,
        fragments: u64,
    ) -> io::Result<()> {
        let mut body = self
            .control_body
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        messages::credit_into(&mut body, bytes, packets, fragments);
        self.write_record(messages::CREDIT, object_id, &body)
    }
}

fn write_parts(stream: &mut dyn Write, header: &[u8], parts: &[&[u8]]) -> io::Result<()> {
    let mut part_index = 0;
    let mut part_offset = 0;
    while part_index <= parts.len() {
        let mut slices = [IoSlice::new(&[]); 16];
        let mut slice_count = 0;
        for logical_index in part_index..=parts.len() {
            let part = if logical_index == 0 {
                header
            } else {
                parts[logical_index - 1]
            };
            let offset = if logical_index == part_index {
                part_offset
            } else {
                0
            };
            if offset < part.len() {
                slices[slice_count] = IoSlice::new(&part[offset..]);
                slice_count += 1;
                if slice_count == slices.len() {
                    break;
                }
            }
        }
        if slice_count == 0 {
            return Ok(());
        }
        let written = stream.write_vectored(&slices[..slice_count])?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write Vivid record parts",
            ));
        }
        let mut remaining = written;
        while part_index <= parts.len() {
            let part = if part_index == 0 {
                header
            } else {
                parts[part_index - 1]
            };
            let available = part.len().saturating_sub(part_offset);
            if remaining < available {
                part_offset += remaining;
                break;
            }
            remaining -= available;
            part_index += 1;
            part_offset = 0;
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
