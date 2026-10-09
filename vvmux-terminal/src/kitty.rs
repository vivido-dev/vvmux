//! Kitty graphics protocol interception and validation.

use std::collections::BTreeMap;
use std::mem;

use base64::Engine;

use crate::event::KittyGraphicsCommand;
use crate::marker::{find_bytes, partial_prefix_len};

/// Largest single Kitty graphics escape sequence accepted, in bytes; larger packets are discarded.
pub(crate) const KITTY_PACKET_MAX_BYTES: usize = 8 * 1024;
/// Largest base64 payload in one Kitty graphics chunk, as the protocol specifies for chunked
/// transfers.
pub(crate) const KITTY_PAYLOAD_MAX_BYTES: usize = 4096;
/// Largest decoded image one Kitty transfer may declare.
pub(crate) const KITTY_DECODED_TRANSFER_MAX_BYTES: u32 = 64 * 1024 * 1024;

pub(crate) enum KittyChunk {
    Bytes(Vec<u8>),
    Command(KittyGraphicsCommand),
}

#[derive(Default)]
pub(crate) struct KittyGraphicsScanner {
    pub(crate) pending: Vec<u8>,
}

impl KittyGraphicsScanner {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<KittyChunk> {
        const PREFIX: &[u8] = b"\x1b_G";
        const TERMINATOR: &[u8] = b"\x1b\\";

        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut cursor = 0;
        loop {
            let Some(relative_start) = find_bytes(&self.pending[cursor..], PREFIX) else {
                let keep = partial_prefix_len(&self.pending[cursor..], PREFIX);
                let end = self.pending.len().saturating_sub(keep);
                push_kitty_bytes(&mut chunks, &self.pending[cursor..end]);
                cursor = end;
                break;
            };
            let start = cursor + relative_start;
            push_kitty_bytes(&mut chunks, &self.pending[cursor..start]);
            let body_start = start + PREFIX.len();
            let Some(relative_end) = find_bytes(&self.pending[body_start..], TERMINATOR) else {
                if self.pending.len().saturating_sub(start) > KITTY_PACKET_MAX_BYTES {
                    push_kitty_bytes(&mut chunks, &self.pending[start..body_start]);
                    cursor = body_start;
                    continue;
                }
                cursor = start;
                break;
            };
            let terminator = body_start + relative_end;
            let end = terminator + TERMINATOR.len();
            if end.saturating_sub(start) > KITTY_PACKET_MAX_BYTES {
                push_kitty_bytes(&mut chunks, &self.pending[start..body_start]);
                cursor = body_start;
                continue;
            }
            let raw = &self.pending[start..end];
            match validate_kitty_command(&self.pending[body_start..terminator], raw) {
                Some(command) => chunks.push(KittyChunk::Command(command)),
                None => push_kitty_bytes(&mut chunks, raw),
            }
            cursor = end;
        }
        self.pending.drain(..cursor);
        chunks
    }

    pub(crate) fn finish(&mut self) -> Vec<KittyChunk> {
        let pending = mem::take(&mut self.pending);
        if pending.is_empty() {
            Vec::new()
        } else {
            vec![KittyChunk::Bytes(pending)]
        }
    }
}

pub(crate) fn validate_kitty_command(body: &[u8], raw: &[u8]) -> Option<KittyGraphicsCommand> {
    let separator = body.iter().position(|byte| *byte == b';');
    let (control, payload) = separator.map_or((body, &[][..]), |index| {
        (&body[..index], &body[index + 1..])
    });
    let control = std::str::from_utf8(control).ok()?;
    if !control.is_ascii() || payload.len() > KITTY_PAYLOAD_MAX_BYTES {
        return None;
    }

    let mut fields = BTreeMap::new();
    for field in control.split(',') {
        let (key, value) = field.split_once('=')?;
        if key.is_empty()
            || value.is_empty()
            || !key.bytes().all(|byte| byte.is_ascii_alphabetic())
            || !value.is_ascii()
            || fields.insert(key, value).is_some()
        {
            return None;
        }
    }
    let number = |key: &str| fields.get(key).and_then(|value| value.parse::<u32>().ok());
    let more = number("m").unwrap_or(0);
    if more > 1 || number("q") != Some(2) {
        return None;
    }

    match fields.get("a").copied() {
        Some("q") => {
            if !keys_are(&fields, &["a", "i", "s", "v", "t", "f", "q"])
                || number("i") == Some(0)
                || number("i").is_none()
                || number("s") != Some(1)
                || number("v") != Some(1)
                || number("f") != Some(24)
                || fields.get("t").copied() != Some("d")
                || payload != b"AAAA"
            {
                return None;
            }
            Some(KittyGraphicsCommand::Query {
                image_id: number("i")?,
            })
        }
        Some("T" | "t") => {
            let action = fields.get("a").copied()?;
            let allowed = if action == "T" {
                &["a", "i", "f", "s", "v", "c", "r", "U", "C", "q", "m", "t"][..]
            } else {
                &["a", "i", "f", "s", "v", "q", "m", "t"][..]
            };
            let format = number("f")?;
            let width = number("s")?;
            let height = number("v")?;
            let bytes_per_pixel = match format {
                24 => 3,
                32 => 4,
                _ => return None,
            };
            let decoded_bytes = width.checked_mul(height)?.checked_mul(bytes_per_pixel)?;
            if !keys_are(&fields, allowed)
                || fields.get("t").is_some_and(|transport| *transport != "d")
                || width == 0
                || height == 0
                || decoded_bytes > KITTY_DECODED_TRANSFER_MAX_BYTES
                || payload.is_empty()
                || !valid_base64_payload(payload, more == 1)
            {
                return None;
            }
            if action == "T"
                && (number("i").is_none_or(|value| value == 0)
                    || number("U") != Some(1)
                    || number("c").is_none_or(|value| value == 0)
                    || number("r").is_none_or(|value| value == 0))
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: true,
                more: more == 1,
            })
        }
        Some("p") => {
            if !keys_are(&fields, &["a", "i", "c", "r", "U", "C", "q"])
                || number("i").is_none_or(|value| value == 0)
                || number("U") != Some(1)
                || number("c").is_none_or(|value| value == 0)
                || number("r").is_none_or(|value| value == 0)
                || !payload.is_empty()
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: true,
                more: false,
            })
        }
        Some("d") => {
            if !keys_are(&fields, &["a", "d", "i", "q"])
                || !matches!(fields.get("d").copied(), Some("i" | "I"))
                || number("i").is_none_or(|value| value == 0)
                || !payload.is_empty()
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: true,
                more: false,
            })
        }
        None => {
            if !keys_are(&fields, &["q", "m"])
                || separator.is_none()
                || payload.is_empty()
                || !valid_base64_payload(payload, more == 1)
            {
                return None;
            }
            Some(KittyGraphicsCommand::Packet {
                bytes: raw.to_vec(),
                starts_transfer: false,
                more: more == 1,
            })
        }
        _ => None,
    }
}

pub(crate) fn keys_are(fields: &BTreeMap<&str, &str>, allowed: &[&str]) -> bool {
    fields.keys().all(|key| allowed.contains(key))
}

pub(crate) fn valid_base64_payload(payload: &[u8], more: bool) -> bool {
    payload.len() <= KITTY_PAYLOAD_MAX_BYTES
        && (!more || payload.len().is_multiple_of(4))
        && payload
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        && base64::engine::general_purpose::STANDARD
            .decode(payload)
            .is_ok()
}

pub(crate) fn push_kitty_bytes(chunks: &mut Vec<KittyChunk>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(KittyChunk::Bytes(previous)) = chunks.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        chunks.push(KittyChunk::Bytes(bytes.to_vec()));
    }
}
