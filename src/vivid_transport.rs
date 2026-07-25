use std::collections::HashSet;
use std::io::{self, IoSlice, Read, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use vivid_protocol::messages;
use vivid_protocol::trace::{TraceDirection, TraceEmitter, TraceOutcome};
use vivid_protocol::wire::{
    BorrowedRecord, HEADER_SIZE, PREFACE_SIZE, Preface, RECORD_KNOWN_FLAGS, Record, RecordHeader,
    accept_preface,
};
use vivid_protocol::{CONTROL_MAX_RECORD_BODY, HARD_MAX_RECORD_BODY};

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
    trace: Option<TraceChannel>,
}

#[derive(Clone)]
pub struct TraceChannel {
    emitter: TraceEmitter,
    restricted_sources: Arc<Mutex<HashSet<u64>>>,
}

impl TraceChannel {
    pub fn new(emitter: TraceEmitter) -> Self {
        Self {
            emitter,
            restricted_sources: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn mark_source_policy(&self, source_id: u64, capture_policy: u64) {
        if capture_policy & messages::CAPTURE_POLICY_REDUCE_DIAGNOSTICS != 0 {
            self.restricted_sources
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(source_id);
        }
    }

    fn emit(&self, direction: TraceDirection, header: RecordHeader, body: &[u8]) {
        let restricted = header.record_type == messages::ATTACH_CHANNEL
            || (header.object_id != 0
                && (self
                    .restricted_sources
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains(&header.object_id)
                    || body_restricts_trace(header.record_type, body)));
        if restricted {
            self.emitter.emit_restricted_source(
                direction,
                header.record_type,
                u64::from(header.body_length),
                header.sequence,
            );
        } else if header.record_type >= messages::ATTACH_CHANNEL {
            self.emitter.emit(
                direction,
                header.record_type,
                u64::from(header.body_length),
                header.sequence,
                vivid_protocol::trace::object_kind(header.record_type, header.object_id),
                (header.object_id != 0).then_some(header.object_id),
                None,
                None,
                TraceOutcome::Ok,
            );
        } else {
            self.emitter.emit_control(
                direction,
                header.record_type,
                body,
                header.sequence,
                vivid_protocol::trace::object_kind(header.record_type, header.object_id),
                (header.object_id != 0).then_some(header.object_id),
                TraceOutcome::Ok,
            );
        }
    }
}

impl Reader {
    pub fn new(mut transport: Transport) -> io::Result<(Self, Preface)> {
        let cancel = transport.cancel();
        let timeout = transport.timeout.clone();
        let deadline = transport.deadline.clone();
        let mut bytes = [0_u8; PREFACE_SIZE];
        transport.reader.read_exact(&mut bytes)?;
        let preface = accept_preface(bytes, transport.writer.as_mut())?;
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
                trace: None,
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
        if let Some(trace) = &self.trace {
            trace.emit(TraceDirection::Receive, header, body);
        }
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
            trace: self.trace.clone(),
        })
    }

    pub fn set_maximum(&mut self, maximum: u32) {
        self.maximum = self
            .negotiated_maximum
            .min(maximum)
            .min(HARD_MAX_RECORD_BODY);
    }

    pub fn set_trace(&mut self, trace: TraceChannel) {
        self.trace = Some(trace);
    }

    pub fn mark_source_policy(&self, source_id: u64, capture_policy: u64) {
        if let Some(trace) = &self.trace {
            trace.mark_source_policy(source_id, capture_policy);
        }
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
    trace: Option<TraceChannel>,
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
        let sequence = inner.sequence;
        drop(inner);
        if let Some(trace) = &self.trace {
            trace.emit(
                TraceDirection::Send,
                header,
                parts.first().copied().unwrap_or_default(),
            );
        }
        Ok(sequence)
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

    pub fn mark_source_policy(&self, source_id: u64, capture_policy: u64) {
        if let Some(trace) = &self.trace {
            trace.mark_source_policy(source_id, capture_policy);
        }
    }
}

fn body_restricts_trace(record_type: u16, body: &[u8]) -> bool {
    let policy = match record_type {
        messages::CREATE_RASTER => messages::parse_create_raster_with_extensions(body)
            .ok()
            .map(|(_, _, policy, _)| policy),
        messages::CREATE_IMAGE => messages::parse_create_image_with_extensions(body)
            .ok()
            .map(|(_, _, policy, _)| policy),
        messages::CREATE_VIDEO => messages::parse_create_video_with_extensions(body)
            .ok()
            .map(|(_, _, policy, _)| policy),
        messages::CREATE_AUDIO => messages::parse_create_audio_with_extensions(body)
            .ok()
            .map(|(_, _, policy, _)| policy),
        messages::SET_SOURCE_POLICY => messages::parse_set_source_policy(body)
            .ok()
            .map(|(_, _, policy)| policy),
        _ => None,
    };
    policy.is_some_and(|policy| policy & messages::CAPTURE_POLICY_REDUCE_DIAGNOSTICS != 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use vivid_protocol::trace::{
        TraceComponent, TraceDirection, TraceGuard, TraceHop, TraceOutcome,
    };
    use vivid_protocol::wire::{ConnectionKind, encode_preface};

    use crate::platform::ConnectionCancel;

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn transport(preface: [u8; PREFACE_SIZE], writer: SharedWriter) -> Transport {
        Transport::new(
            Box::new(Cursor::new(preface)),
            Box::new(writer),
            ConnectionCancel::new(|| {}),
            Arc::new(|_| Ok(())),
        )
    }

    #[test]
    fn version_mismatch_is_typed_but_malformed_preface_is_silent() {
        let mut mismatch = encode_preface(ConnectionKind::Control, 1024);
        mismatch[5] = 0;
        let output = SharedWriter::default();
        let error = match Reader::new(transport(mismatch, output.clone())) {
            Ok(_) => panic!("mismatched preface was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        let bytes = output.0.lock().unwrap();
        let header = RecordHeader::decode(bytes[..HEADER_SIZE].try_into().unwrap());
        assert_eq!(header.record_type, messages::ERROR);
        assert_eq!(bytes.len(), HEADER_SIZE + header.body_length as usize);
        let rejection = messages::parse_error_reply(&bytes[HEADER_SIZE..]).unwrap();
        assert_eq!(rejection.code, messages::ERROR_UNSUPPORTED_VERSION);
        assert_eq!(rejection.supported_version, Some((1, 1)));
        drop(bytes);

        let mut malformed = encode_preface(ConnectionKind::Control, 1024);
        malformed[0] = b'X';
        let silent = SharedWriter::default();
        assert!(Reader::new(transport(malformed, silent.clone())).is_err());
        assert!(silent.0.lock().unwrap().is_empty());
    }

    #[test]
    fn diagnostic_trace_never_serializes_control_secrets_or_restricted_source_ids() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let output = records.clone();
        let guard = TraceGuard::callback(
            TraceComponent::Vvmux,
            TraceHop::Inner,
            [0x66; 16],
            move |record| output.lock().unwrap().push(record),
        )
        .unwrap();
        let channel = TraceChannel::new(guard.emitter());
        let token = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
        let hello = messages::encode_hello(
            8,
            &messages::HelloConfig {
                minimum_major: 1,
                minimum_minor: 1,
                maximum_major: 1,
                maximum_minor: 1,
                token,
                producer: "confidential pane title",
                producer_version: "1",
                required_features: &[],
                optional_features: &[],
                maximum_record_body: 4096,
                authentication_kind: messages::AUTHENTICATION_WINDOW_ROOT,
                preserved_fields: &[],
            },
        );
        channel.emit(
            TraceDirection::Receive,
            RecordHeader {
                body_length: hello.len() as u32,
                record_type: messages::HELLO,
                flags: 0,
                object_id: 0,
                sequence: 1,
            },
            &hello,
        );
        channel.mark_source_policy(92, messages::CAPTURE_POLICY_REDUCE_DIAGNOSTICS);
        channel.emit(
            TraceDirection::Receive,
            RecordHeader {
                body_length: 32,
                record_type: messages::VIDEO_PACKET,
                flags: 0,
                object_id: 92,
                sequence: 2,
            },
            b"private-media-body-is-never-traced",
        );
        let cbor_shaped_media = messages::ok(999);
        channel.emit(
            TraceDirection::Receive,
            RecordHeader {
                body_length: cbor_shaped_media.len() as u32,
                record_type: messages::VIDEO_PACKET,
                flags: 0,
                object_id: 94,
                sequence: 3,
            },
            &cbor_shaped_media,
        );
        drop(channel);
        drop(guard);

        let records = records.lock().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].outcome, TraceOutcome::Restricted);
        assert_eq!(records[1].object_id, None);
        assert_eq!(
            records[2].request_id, None,
            "media bytes are never decoded as control CBOR"
        );
        let trace = records
            .iter()
            .map(|record| record.ndjson_line())
            .collect::<String>();
        for forbidden in [
            token,
            "confidential pane title",
            "private-media-body",
            "\"object_id\":92",
        ] {
            assert!(!trace.contains(forbidden), "trace leaked {forbidden}");
        }
    }
}
