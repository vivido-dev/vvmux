//! Fragment-safe scanning for authenticated Vivid anchor markers in pane output.

use std::cmp;
use std::mem;

/// Longest Vivid anchor marker, in bytes; longer candidates pass through as ordinary output.
pub(crate) const MAX_MARKER_BYTES: usize = 128;

#[derive(Clone, Copy)]
pub(crate) struct MarkerEnvelope {
    pub(crate) prefix: &'static [u8],
    pub(crate) terminator: &'static [u8],
    pub(crate) payload_skip: usize,
}

pub(crate) const APC_ENVELOPE: MarkerEnvelope = MarkerEnvelope {
    prefix: b"\x1b_VIVID;3;A;",
    terminator: b"\x1b\\",
    payload_skip: 2,
};

#[cfg(windows)]
pub(crate) const CONPTY_ENVELOPE: MarkerEnvelope = MarkerEnvelope {
    prefix: b"VIVID;3;A;",
    terminator: b";VIVID-END",
    payload_skip: 0,
};

pub(crate) enum VividChunk {
    Bytes(Vec<u8>),
    Marker(String),
}

#[derive(Default)]
pub(crate) struct VividMarkerScanner {
    pub(crate) pending: Vec<u8>,
}

impl VividMarkerScanner {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<VividChunk> {
        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut cursor = 0;
        loop {
            let Some((relative_start, envelope)) = find_envelope(&self.pending[cursor..]) else {
                let keep = marker_envelopes()
                    .iter()
                    .map(|envelope| partial_prefix_len(&self.pending[cursor..], envelope.prefix))
                    .max()
                    .unwrap_or(0);
                let end = self.pending.len().saturating_sub(keep);
                push_bytes(&mut chunks, &self.pending[cursor..end]);
                cursor = end;
                break;
            };
            let start = cursor + relative_start;
            push_bytes(&mut chunks, &self.pending[cursor..start]);
            if envelope.payload_skip != 0 && !self.pending[start..].starts_with(envelope.prefix) {
                if envelope.prefix.starts_with(&self.pending[start..]) {
                    cursor = start;
                    break;
                }
                push_bytes(&mut chunks, &self.pending[start..=start]);
                cursor = start + 1;
                continue;
            }
            #[cfg(windows)]
            if envelope.payload_skip == 0 {
                use vivid_protocol::anchor::conpty::{self, Scan};
                match conpty::scan(&self.pending[start..]) {
                    Scan::Complete { consumed, body } => {
                        chunks.push(VividChunk::Marker(body));
                        cursor = start + consumed;
                    }
                    Scan::Incomplete => {
                        cursor = start;
                        break;
                    }
                    Scan::Invalid => {
                        push_bytes(&mut chunks, &self.pending[start..start + 1]);
                        cursor = start + 1;
                    }
                }
                continue;
            }
            let search_start = start + envelope.prefix.len();
            let Some(relative_end) = find_bytes(&self.pending[search_start..], envelope.terminator)
            else {
                if self.pending.len() - start > MAX_MARKER_BYTES {
                    push_bytes(&mut chunks, &self.pending[start..search_start]);
                    cursor = search_start;
                    continue;
                }
                cursor = start;
                break;
            };
            let terminator = search_start + relative_end;
            let end = terminator + envelope.terminator.len();
            if end - start > MAX_MARKER_BYTES {
                push_bytes(&mut chunks, &self.pending[start..search_start]);
                cursor = search_start;
                continue;
            }
            match std::str::from_utf8(&self.pending[start + envelope.payload_skip..terminator]) {
                Ok(marker) if valid_marker_shape(marker) => {
                    chunks.push(VividChunk::Marker(marker.to_owned()));
                }
                _ => push_bytes(&mut chunks, &self.pending[start..end]),
            }
            cursor = end;
        }
        self.pending.drain(..cursor);
        chunks
    }

    pub(crate) fn finish(&mut self) -> Vec<VividChunk> {
        let pending = mem::take(&mut self.pending);
        if pending.is_empty() {
            Vec::new()
        } else {
            vec![VividChunk::Bytes(pending)]
        }
    }
}

pub(crate) fn marker_envelopes() -> &'static [MarkerEnvelope] {
    #[cfg(unix)]
    {
        std::slice::from_ref(&APC_ENVELOPE)
    }
    #[cfg(windows)]
    {
        &[CONPTY_ENVELOPE, APC_ENVELOPE]
    }
}

pub(crate) fn find_envelope(haystack: &[u8]) -> Option<(usize, MarkerEnvelope)> {
    marker_envelopes()
        .iter()
        .filter_map(|envelope| {
            // ConPTY can wrap even inside the prefix at a pane's right edge.
            let prefix = if envelope.payload_skip == 0 {
                &envelope.prefix[..1]
            } else {
                // Claim a fragmented APC introducer before its inner V can be mistaken
                // for the beginning of a printable ConPTY candidate.
                &envelope.prefix[..2]
            };
            find_bytes(haystack, prefix).map(|position| (position, *envelope))
        })
        .min_by_key(|(position, _)| *position)
}

pub(crate) fn valid_marker_shape(marker: &str) -> bool {
    if marker.len() > 124 || !marker.is_ascii() {
        return false;
    }
    let mut fields = marker.split(';');
    let valid = fields.next() == Some("VIVID")
        && fields.next() == Some("3")
        && fields.next() == Some("A")
        && fields
            .next()
            .is_some_and(|tag| tag.len() == 22 && tag.bytes().all(is_base64url))
        && fields.next().is_some_and(|id| {
            id.len() == 16
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && id.bytes().any(|byte| byte != b'0')
        })
        && fields.next().is_some_and(|id| {
            id.len() == 16
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && id.bytes().any(|byte| byte != b'0')
        })
        && fields
            .next()
            .is_some_and(|auth| auth.len() == 22 && auth.bytes().all(is_base64url));
    valid && fields.next().is_none()
}

pub(crate) fn is_base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) fn partial_prefix_len(haystack: &[u8], prefix: &[u8]) -> usize {
    (1..=cmp::min(haystack.len(), prefix.len().saturating_sub(1)))
        .rev()
        .find(|&len| haystack.ends_with(&prefix[..len]))
        .unwrap_or(0)
}

pub(crate) fn push_bytes(chunks: &mut Vec<VividChunk>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(VividChunk::Bytes(previous)) = chunks.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        chunks.push(VividChunk::Bytes(bytes.to_vec()));
    }
}
