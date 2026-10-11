//! Direct RGB images translated into pane-local Unicode placements.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use base64::Engine;

use super::{KITTY_DECODED_TRANSFER_MAX_BYTES, keys_are, validate_kitty_command};
use crate::event::{KittyGraphicsCommand, KittyGraphicsReply};

mod diacritics;
pub(crate) use diacritics::DIACRITICS;

#[cfg(test)]
mod tests;

const MAX_IMAGES: usize = 256;
const MAX_PLACEMENTS: usize = 256;
const MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024;
static PIXEL_BYTES: AtomicUsize = AtomicUsize::new(0);
static NEXT_IMAGE: AtomicU32 = AtomicU32::new(0x0100_0000);

type Fields<'a> = BTreeMap<&'a str, &'a str>;
type Result<T> = std::result::Result<T, &'static str>;

/// Raw pixels, charged against a process-wide budget even during an incomplete upload.
struct Pixels {
    data: Vec<u8>,
    reserved: usize,
}

impl Pixels {
    fn new(bytes: usize) -> Result<Self> {
        PIXEL_BYTES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= KITTY_DECODED_TRANSFER_MAX_BYTES as usize)
            })
            .map_err(|_error| "ENOSPC:cursor image cache is full")?;
        Ok(Self {
            data: Vec::with_capacity(bytes),
            reserved: bytes,
        })
    }

    fn append(&mut self, payload: &[u8], more: bool) -> Result<()> {
        if payload.len() > super::KITTY_PAYLOAD_MAX_BYTES
            || (more && (!payload.len().is_multiple_of(4) || payload.contains(&b'=')))
        {
            return Err("EINVAL:invalid image chunk");
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
            .map_err(|_error| "EINVAL:invalid base64")?;
        if self
            .data
            .len()
            .checked_add(decoded.len())
            .is_none_or(|size| size > self.reserved)
        {
            return Err("EINVAL:too many image bytes");
        }
        self.data.extend_from_slice(&decoded);
        if !more && self.data.len() != self.reserved {
            return Err("EINVAL:incomplete image pixels");
        }
        Ok(())
    }
}

impl Drop for Pixels {
    fn drop(&mut self) {
        PIXEL_BYTES.fetch_sub(self.reserved, Ordering::Relaxed);
    }
}

struct Image {
    pixels: Pixels,
    width: u32,
    height: u32,
    format: u32,
    // Only explicit application placement IDs replace previous placements. ID zero adds one.
    placements: Vec<(u32, u32)>,
}

struct Transfer {
    image: Image,
    fields: BTreeMap<String, String>,
    reply: KittyGraphicsReply,
    display: bool,
}

#[derive(Default)]
pub(crate) struct CursorGraphics {
    images: BTreeMap<u32, Image>,
    pending: Option<Transfer>,
}

pub(crate) struct Placement {
    pub(crate) host_id: u32,
    pub(crate) columns: usize,
    pub(crate) rows: usize,
    pub(crate) keep_cursor: bool,
}

#[derive(Default)]
pub(crate) struct GraphicsOutput {
    pub(crate) commands: Vec<KittyGraphicsCommand>,
    pub(crate) clear: Vec<u32>,
    pub(crate) placement: Option<Placement>,
}

fn number(fields: &Fields<'_>, key: &str, default: u32) -> Result<u32> {
    fields.get(key).map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_error| "EINVAL:invalid numeric field")
    })
}

fn parse(body: &[u8]) -> Option<(Fields<'_>, &[u8])> {
    let separator = body.iter().position(|byte| *byte == b';');
    let (control, payload) = separator.map_or((body, &[][..]), |index| {
        (&body[..index], &body[index + 1..])
    });
    if payload.len() > super::KITTY_PAYLOAD_MAX_BYTES {
        return None;
    }
    let mut fields = BTreeMap::new();
    for field in std::str::from_utf8(control).ok()?.split(',') {
        let (key, value) = field.split_once('=')?;
        if key.len() != 1
            || !key.as_bytes()[0].is_ascii_alphabetic()
            || value.is_empty()
            || !value.is_ascii()
            || fields.insert(key, value).is_some()
        {
            return None;
        }
    }
    Some((fields, payload))
}

fn reply(fields: &Fields<'_>) -> KittyGraphicsReply {
    KittyGraphicsReply {
        image_id: number(fields, "i", 0).unwrap_or(0),
        placement_id: number(fields, "p", 0).ok().filter(|id| *id != 0),
        quiet: number(fields, "q", 0)
            .ok()
            .filter(|quiet| *quiet <= 2)
            .unwrap_or(0) as u8,
    }
}

fn packet(bytes: Vec<u8>, reply: Option<KittyGraphicsReply>) -> KittyGraphicsCommand {
    KittyGraphicsCommand::Packet {
        bytes,
        starts_transfer: true,
        more: false,
        reply,
    }
}

fn delete(host_id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={host_id},q=2\x1b\\").into_bytes()
}

impl CursorGraphics {
    pub(crate) fn process(
        &mut self,
        raw: &[u8],
        capable: bool,
        cell: (u16, u16),
    ) -> GraphicsOutput {
        let body = &raw[3..raw.len() - 2];
        let Some((fields, payload)) = parse(body) else {
            self.pending = None;
            return GraphicsOutput::default();
        };
        let mut response = self
            .pending
            .as_ref()
            .filter(|_| keys_are(&fields, &["m", "q"]))
            .map_or_else(|| reply(&fields), |transfer| transfer.reply);
        if fields.contains_key("q") {
            response.quiet = reply(&fields).quiet;
        }
        let result = if self.pending.is_some() && keys_are(&fields, &["m", "q"]) {
            self.continue_transfer(&fields, payload, capable, cell)
        } else {
            self.pending = None;
            self.command(&fields, payload, raw, capable, cell)
        };
        result.unwrap_or_else(|error| {
            let mut output = GraphicsOutput::default();
            if response.quiet != 2 && response.image_id != 0 {
                output.commands.push(KittyGraphicsCommand::Rejected {
                    image_id: response.image_id,
                    placement_id: response.placement_id,
                    error,
                });
            }
            output
        })
    }

    fn command(
        &mut self,
        fields: &Fields<'_>,
        payload: &[u8],
        raw: &[u8],
        capable: bool,
        cell: (u16, u16),
    ) -> Result<GraphicsOutput> {
        let action = fields.get("a").copied().unwrap_or("t");
        let id = number(fields, "i", 0)?;
        if action == "q"
            || number(fields, "U", 0)? == 1
            || (!fields.contains_key("a") && !fields.contains_key("s"))
        {
            return Ok(GraphicsOutput {
                commands: validate_kitty_command(&raw[3..raw.len() - 2], raw)
                    .into_iter()
                    .collect(),
                ..GraphicsOutput::default()
            });
        }
        if number(fields, "q", 0)? > 2 {
            return Err("EINVAL:invalid reply suppression");
        }
        if !capable {
            return Err("ENOTSUP:attachment has no Kitty graphics");
        }
        match action {
            "T" | "t" => self.start_transfer(fields, payload, cell),
            "p" => {
                if !payload.is_empty() {
                    return Err("EINVAL:placement payload");
                }
                self.place(id, fields, cell)
            }
            "d" => self.remove(fields, raw),
            _ => Err("ENOTSUP:unsupported graphics action"),
        }
    }

    fn start_transfer(
        &mut self,
        fields: &Fields<'_>,
        payload: &[u8],
        cell: (u16, u16),
    ) -> Result<GraphicsOutput> {
        if !keys_are(
            fields,
            &[
                "a", "i", "p", "q", "m", "t", "f", "s", "v", "x", "y", "w", "h", "c", "r", "C", "U",
            ],
        ) || fields.get("t").is_some_and(|medium| *medium != "d")
        {
            return Err("ENOTSUP:only direct RGB and RGBA images are supported");
        }
        let id = number(fields, "i", 0)?;
        let width = number(fields, "s", 0)?;
        let height = number(fields, "v", 0)?;
        let format = number(fields, "f", 32)?;
        let channels = match format {
            24 => 3,
            32 => 4,
            _ => return Err("ENOTSUP:unsupported pixel format"),
        };
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(channels))
            .filter(|size| *size > 0 && *size <= KITTY_DECODED_TRANSFER_MAX_BYTES)
            .ok_or("EINVAL:invalid image dimensions")?;
        if id == 0 {
            return Err("EINVAL:image ID is required");
        }
        if self.images.len() >= MAX_IMAGES && !self.images.contains_key(&id) {
            return Err("ENOSPC:too many images");
        }
        let more = number(fields, "m", 0)?;
        if more > 1 {
            return Err("EINVAL:invalid chunk flag");
        }
        let mut pixels = Pixels::new(size as usize)?;
        pixels.append(payload, more == 1)?;
        let transfer = Transfer {
            image: Image {
                pixels,
                width,
                height,
                format,
                placements: Vec::new(),
            },
            fields: fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            reply: reply(fields),
            display: fields.get("a").copied() == Some("T"),
        };
        if more == 1 {
            self.pending = Some(transfer);
            Ok(GraphicsOutput::default())
        } else {
            self.complete(transfer, cell)
        }
    }

    fn continue_transfer(
        &mut self,
        fields: &Fields<'_>,
        payload: &[u8],
        capable: bool,
        cell: (u16, u16),
    ) -> Result<GraphicsOutput> {
        let mut transfer = self.pending.take().ok_or("EINVAL:missing image transfer")?;
        if !capable {
            return Err("ENOTSUP:attachment has no Kitty graphics");
        }
        let more = number(fields, "m", 0)?;
        if more > 1 {
            return Err("EINVAL:invalid chunk flag");
        }
        if fields.contains_key("q") {
            let quiet = number(fields, "q", 0)?;
            if quiet > 2 {
                return Err("EINVAL:invalid reply suppression");
            }
            transfer.reply.quiet = quiet as u8;
        }
        transfer.image.pixels.append(payload, more == 1)?;
        if more == 1 {
            self.pending = Some(transfer);
            Ok(GraphicsOutput::default())
        } else {
            self.complete(transfer, cell)
        }
    }

    fn complete(&mut self, transfer: Transfer, cell: (u16, u16)) -> Result<GraphicsOutput> {
        let old = self.images.insert(transfer.reply.image_id, transfer.image);
        let mut output = if transfer.display {
            let fields = transfer
                .fields
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            match self.place(transfer.reply.image_id, &fields, cell) {
                Ok(placed) => placed,
                Err(error) => {
                    self.images.remove(&transfer.reply.image_id);
                    if let Some(old) = old {
                        self.images.insert(transfer.reply.image_id, old);
                    }
                    return Err(error);
                }
            }
        } else {
            // No host image is needed until it is placed. Session admission still gates the ACK.
            GraphicsOutput {
                commands: vec![packet(Vec::new(), Some(transfer.reply))],
                ..GraphicsOutput::default()
            }
        };
        if let Some(old) = old {
            for (_, host) in old.placements {
                output.clear.push(host);
                output.commands.insert(0, packet(delete(host), None));
            }
        }
        Ok(output)
    }

    fn place(&mut self, id: u32, fields: &Fields<'_>, cell: (u16, u16)) -> Result<GraphicsOutput> {
        if !keys_are(
            fields,
            &[
                "a", "i", "p", "q", "m", "t", "f", "s", "v", "x", "y", "w", "h", "c", "r", "C", "U",
            ],
        ) || number(fields, "U", 0)? != 0
            || number(fields, "C", 0)? > 1
        {
            return Err("ENOTSUP:unsupported placement parameters");
        }
        let image = self
            .images
            .get_mut(&id)
            .ok_or("ENOENT:image is not cached")?;
        let x = number(fields, "x", 0)?;
        let y = number(fields, "y", 0)?;
        let width = number(fields, "w", 0)?;
        let height = number(fields, "h", 0)?;
        let width = if width == 0 {
            image.width.saturating_sub(x)
        } else {
            width.min(image.width.saturating_sub(x))
        };
        let height = if height == 0 {
            image.height.saturating_sub(y)
        } else {
            height.min(image.height.saturating_sub(y))
        };
        if width == 0 || height == 0 {
            return Err("EINVAL:empty source rectangle");
        }
        let (cw, ch) = (u32::from(cell.0.max(1)), u32::from(cell.1.max(1)));
        let (mut columns, mut rows) = (number(fields, "c", 0)?, number(fields, "r", 0)?);
        if columns as usize > DIACRITICS.len() || rows as usize > DIACRITICS.len() {
            return Err("ENOTSUP:placement exceeds Unicode dimensions");
        }
        // Use u64 throughout derived geometry: image sizes and explicit cell counts are untrusted.
        match (columns, rows) {
            (0, 0) => {
                columns = width.div_ceil(cw);
                rows = height.div_ceil(ch);
            }
            (0, _) => {
                columns = u32::try_from(
                    (u64::from(rows) * u64::from(ch) * u64::from(width))
                        .div_ceil(u64::from(height) * u64::from(cw)),
                )
                .map_err(|_error| "EINVAL:placement overflow")?;
            }
            (_, 0) => {
                rows = u32::try_from(
                    (u64::from(columns) * u64::from(cw) * u64::from(height))
                        .div_ceil(u64::from(width) * u64::from(ch)),
                )
                .map_err(|_error| "EINVAL:placement overflow")?;
            }
            _ => {}
        }
        if columns == 0
            || rows == 0
            || columns as usize > DIACRITICS.len()
            || rows as usize > DIACRITICS.len()
        {
            return Err("ENOTSUP:placement exceeds Unicode dimensions");
        }
        let placement = number(fields, "p", 0)?;
        let old = image
            .placements
            .iter()
            .position(|(p, _)| placement != 0 && *p == placement);
        if old.is_none() && image.placements.len() >= MAX_PLACEMENTS {
            return Err("ENOSPC:too many placements");
        }
        let channels = (image.format / 8) as usize;
        let cropped_size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(channels))
            .ok_or("EINVAL:crop overflow")?;
        let encoded_size = cropped_size
            .div_ceil(3)
            .checked_mul(4)
            .ok_or("EINVAL:encoded image overflow")?;
        let packet_size = encoded_size
            .checked_add(encoded_size.div_ceil(4096) * 256)
            .filter(|n| *n <= MAX_ENCODED_BYTES)
            .ok_or("ENOSPC:encoded image exceeds transfer budget")?;
        let host_id = NEXT_IMAGE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_error| "ENOSPC:host image IDs exhausted")?;
        let mut bytes = Vec::with_capacity(packet_size);
        let mut chunk = Vec::with_capacity(3072);
        let stride = image.width as usize * channels;
        let row_bytes = width as usize * channels;
        let mut remaining = cropped_size;
        let mut first = true;
        for row in y..y + height {
            let start = row as usize * stride + x as usize * channels;
            let mut row_pixels = &image.pixels.data[start..start + row_bytes];
            while !row_pixels.is_empty() {
                let count = (3072 - chunk.len()).min(row_pixels.len());
                chunk.extend_from_slice(&row_pixels[..count]);
                row_pixels = &row_pixels[count..];
                remaining -= count;
                if chunk.len() == 3072 || remaining == 0 {
                    let header = if first {
                        format!(
                            "\x1b_Ga=T,i={host_id},f={},s={width},v={height},c={columns},r={rows},U=1,C=1,q=2,m={};",
                            image.format,
                            u8::from(remaining != 0)
                        )
                    } else {
                        format!("\x1b_Gq=2,m={};", u8::from(remaining != 0))
                    };
                    bytes.extend_from_slice(header.as_bytes());
                    bytes.extend_from_slice(
                        base64::engine::general_purpose::STANDARD
                            .encode(&chunk)
                            .as_bytes(),
                    );
                    bytes.extend_from_slice(b"\x1b\\");
                    chunk.clear();
                    first = false;
                }
            }
        }
        let mut output = GraphicsOutput::default();
        if let Some(index) = old {
            let (_, host) = image.placements.remove(index);
            output.clear.push(host);
            output.commands.push(packet(delete(host), None));
        }
        image.placements.push((placement, host_id));
        output.commands.push(packet(bytes, Some(reply(fields))));
        output.placement = Some(Placement {
            host_id,
            columns: columns as usize,
            rows: rows as usize,
            keep_cursor: number(fields, "C", 0)? == 1,
        });
        Ok(output)
    }

    fn remove(&mut self, fields: &Fields<'_>, raw: &[u8]) -> Result<GraphicsOutput> {
        if !keys_are(fields, &["a", "d", "i", "p", "q", "x", "y"]) {
            return Err("ENOTSUP:unsupported deletion");
        }
        let selector = fields.get("d").copied().unwrap_or("a");
        if !matches!(selector, "a" | "A" | "i" | "I" | "r" | "R") {
            return Err("ENOTSUP:unsupported deletion");
        }
        let id = number(fields, "i", 0)?;
        let placement = number(fields, "p", 0)?;
        let start = number(fields, "x", 0)?;
        let end = number(fields, "y", u32::MAX)?;
        if matches!(selector, "i" | "I") && !self.images.contains_key(&id) {
            if number(fields, "q", 0)? != 2 {
                return Ok(GraphicsOutput::default());
            }
            return Ok(GraphicsOutput {
                commands: validate_kitty_command(&raw[3..raw.len() - 2], raw)
                    .into_iter()
                    .collect(),
                ..GraphicsOutput::default()
            });
        }
        let mut output = GraphicsOutput::default();
        self.images.retain(|client, image| {
            let selected = match selector {
                "i" | "I" => *client == id,
                "r" | "R" => *client >= start && *client <= end,
                _ => true,
            };
            if !selected {
                return true;
            }
            image.placements.retain(|(p, host)| {
                if placement != 0 && *p != placement {
                    return true;
                }
                output.clear.push(*host);
                output.commands.push(packet(delete(*host), None));
                false
            });
            !selector.as_bytes()[0].is_ascii_uppercase() || !image.placements.is_empty()
        });
        Ok(output)
    }
}
