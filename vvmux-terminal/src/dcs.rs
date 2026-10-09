//! Device-control-string interception for DECRQSS and XTGETTCAP/XTSETTCAP requests.

use std::mem;

/// Longest DCS body intercepted; longer sequences are discarded.
pub(crate) const DCS_MAX_BYTES: usize = 4096;

#[derive(Debug, Default)]
pub(crate) struct DcsScanner {
    pub(crate) state: DcsState,
    pub(crate) body: Vec<u8>,
    pub(crate) raw: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum DcsState {
    #[default]
    Ground,
    Escape,
    Body,
    BodyEscape,
    Discarding,
    DiscardingEscape,
}

#[derive(Debug)]
pub(crate) enum DcsChunk {
    Bytes(Vec<u8>),
    Request(DcsRequest),
}

#[derive(Debug)]
pub(crate) enum DcsRequest {
    Decrqss(Vec<u8>),
    Xtgettcap(Vec<u8>),
    Xtsettcap(Vec<u8>),
}

impl DcsScanner {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<DcsChunk> {
        let mut chunks = Vec::new();
        for &byte in bytes {
            match self.state {
                DcsState::Ground if byte == 0x1b => self.state = DcsState::Escape,
                DcsState::Ground => push_dcs_bytes(&mut chunks, &[byte]),
                DcsState::Escape if byte == b'P' => {
                    self.body.clear();
                    self.raw.clear();
                    self.raw.extend_from_slice(b"\x1bP");
                    self.state = DcsState::Body;
                }
                DcsState::Escape if byte == 0x1b => {
                    push_dcs_bytes(&mut chunks, b"\x1b");
                }
                DcsState::Escape => {
                    push_dcs_bytes(&mut chunks, &[0x1b, byte]);
                    self.state = DcsState::Ground;
                }
                DcsState::Body if byte == 0x1b => {
                    self.state = DcsState::BodyEscape;
                }
                DcsState::Body => self.push_body(byte),
                DcsState::BodyEscape if byte == b'\\' => {
                    self.raw.extend_from_slice(b"\x1b\\");
                    self.finish(&mut chunks);
                }
                DcsState::BodyEscape => {
                    self.push_body(0x1b);
                    if matches!(self.state, DcsState::Body) {
                        self.push_body(byte);
                    }
                }
                DcsState::Discarding if byte == 0x1b => {
                    self.state = DcsState::DiscardingEscape;
                }
                DcsState::Discarding => {}
                DcsState::DiscardingEscape if byte == b'\\' => {
                    self.body.clear();
                    self.raw.clear();
                    self.state = DcsState::Ground;
                }
                DcsState::DiscardingEscape if byte == 0x1b => {}
                DcsState::DiscardingEscape => self.state = DcsState::Discarding,
            }
        }
        chunks
    }

    pub(crate) fn push_body(&mut self, byte: u8) {
        self.body.push(byte);
        self.raw.push(byte);
        self.state = if self.body.len() > DCS_MAX_BYTES {
            self.body.clear();
            self.raw.clear();
            DcsState::Discarding
        } else {
            DcsState::Body
        };
    }

    pub(crate) fn finish(&mut self, chunks: &mut Vec<DcsChunk>) {
        let request = self
            .body
            .strip_prefix(b"$q")
            .map(|body| DcsRequest::Decrqss(body.to_vec()))
            .or_else(|| {
                self.body
                    .strip_prefix(b"+q")
                    .map(|body| DcsRequest::Xtgettcap(body.to_vec()))
            })
            .or_else(|| {
                self.body
                    .strip_prefix(b"+p")
                    .map(|body| DcsRequest::Xtsettcap(body.to_vec()))
            });
        chunks.push(match request {
            Some(request) => DcsChunk::Request(request),
            None => DcsChunk::Bytes(mem::take(&mut self.raw)),
        });
        self.body.clear();
        self.raw.clear();
        self.state = DcsState::Ground;
    }
}

pub(crate) fn push_dcs_bytes(chunks: &mut Vec<DcsChunk>, bytes: &[u8]) {
    if let Some(DcsChunk::Bytes(previous)) = chunks.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        chunks.push(DcsChunk::Bytes(bytes.to_vec()));
    }
}
