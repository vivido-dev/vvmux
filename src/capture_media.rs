//! Compose a pane's retained media into one image.
//!
//! The gateway already holds every raster track's composed framebuffer, so this is an observation
//! of state vvmux has in hand rather than a request to a presenter. Nothing here touches the Vivid
//! path, and none of it needs a client to be attached — which is the whole point: a detached
//! session still knows exactly what its panes are showing.
//!
//! What this is *not* is a screenshot. Terminal text is rendered by the presenter's GPU, and vvmux
//! holds a cell grid with no font rasterizer, so the capture carries the producer's own surfaces
//! and nothing else. For a document reader or a browser that is the whole visible content, and at
//! the producer's native resolution rather than scaled into a cell rectangle.

use std::io::{self, Cursor};

use image::{ImageEncoder, ImageReader, Limits, RgbaImage};
use vivid_gateway::{CaptureContent, CaptureLayer, ClipRect, SourceKey};

/// Terminal scene geometry is fixed-point cells: one whole cell is `1 << 32`.
const CELL_FIXED_ONE: i64 = 1 << 32;

/// Hard ceiling on a composed capture, in pixels.
///
/// At RGBA8 this is roughly 128 MB for the canvas alone, before any layer is decoded beside it. A
/// scaled capture of an ordinary pane lands two orders of magnitude below it; the ceiling exists so
/// a malformed geometry or an extreme scale cannot turn a read into an allocation failure.
pub const MAX_CAPTURE_PIXELS: u64 = 32 << 20;

/// Ceiling on either dimension of a decoded layer, matching the raster ceiling elsewhere.
const MAX_LAYER_DIMENSION: u32 = 16_384;

/// The pane rectangle a capture composes into.
#[derive(Debug, Clone, Copy)]
pub struct CaptureTarget {
    pub columns: u32,
    pub rows: u32,
    pub cell_width: u32,
    pub cell_height: u32,
}

impl CaptureTarget {
    /// The pane's size in pixels, or `None` when the product does not fit.
    pub fn pixel_size(self) -> Option<(u32, u32)> {
        let width = self.columns.checked_mul(self.cell_width)?;
        let height = self.rows.checked_mul(self.cell_height)?;
        (width > 0 && height > 0).then_some((width, height))
    }
}

/// What one composed layer contributed, reported back so a caller can tell two captures apart.
#[derive(Debug, Clone)]
pub struct CapturedLayer {
    pub source: SourceKey,
    pub node_id: u64,
    pub source_width: u32,
    pub source_height: u32,
    pub frame_id: Option<u64>,
    pub epoch: Option<u32>,
    pub encoded_image: bool,
}

#[derive(Debug)]
pub struct Captured {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
    pub layers: Vec<CapturedLayer>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Fixed-point cells to pixels, rounding toward zero and refusing anything that would not fit.
fn cells_to_pixels(fixed: i64, cell: u32) -> Option<i64> {
    fixed
        .checked_mul(i64::from(cell))
        .map(|scaled| scaled / CELL_FIXED_ONE)
}

/// A layer's destination rectangle in pane pixels, already intersected with its clip.
fn destination(
    layer: &CaptureLayer,
    target: CaptureTarget,
    canvas: (u32, u32),
) -> Option<(i64, i64, i64, i64)> {
    let x = cells_to_pixels(layer.x, target.cell_width)?;
    let y = cells_to_pixels(layer.y, target.cell_height)?;
    let width = cells_to_pixels(layer.width, target.cell_width)?;
    let height = cells_to_pixels(layer.height, target.cell_height)?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let (mut left, mut top) = (x, y);
    let (mut right, mut bottom) = (x.checked_add(width)?, y.checked_add(height)?);
    if let Some(ClipRect {
        x: cx,
        y: cy,
        width: cw,
        height: ch,
    }) = layer.clip
    {
        let clip_x = cells_to_pixels(cx, target.cell_width)?;
        let clip_y = cells_to_pixels(cy, target.cell_height)?;
        let clip_w = cells_to_pixels(cw, target.cell_width)?;
        let clip_h = cells_to_pixels(ch, target.cell_height)?;
        left = left.max(clip_x);
        top = top.max(clip_y);
        right = right.min(clip_x.checked_add(clip_w)?);
        bottom = bottom.min(clip_y.checked_add(clip_h)?);
    }
    // Then the pane itself, which clips everything regardless of what a producer asked for.
    left = left.max(0);
    top = top.max(0);
    right = right.min(i64::from(canvas.0));
    bottom = bottom.min(i64::from(canvas.1));
    (right > left && bottom > top).then_some((left, top, right, bottom))
}

/// Decode one layer's retained bytes into RGBA, bounding the allocation first.
fn decode_layer(layer: &CaptureLayer) -> io::Result<(RgbaImage, Option<u64>, Option<u32>, bool)> {
    match &layer.content {
        CaptureContent::Raster(raster) => {
            let expected = u64::from(raster.width)
                .checked_mul(u64::from(raster.height))
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or_else(|| invalid("retained raster dimensions overflow"))?;
            if expected != raster.pixels.len() as u64 {
                return Err(invalid("retained raster does not match its dimensions"));
            }
            let image = RgbaImage::from_raw(raster.width, raster.height, raster.pixels.to_vec())
                .ok_or_else(|| invalid("retained raster could not be read as RGBA"))?;
            Ok((image, Some(raster.frame_id), Some(raster.epoch), false))
        }
        CaptureContent::EncodedImage(bytes) => {
            let mut reader = ImageReader::new(Cursor::new(bytes.as_ref()))
                .with_guessed_format()
                .map_err(|error| invalid(&format!("retained image is unreadable: {error}")))?;
            let mut limits = Limits::default();
            limits.max_image_width = Some(MAX_LAYER_DIMENSION);
            limits.max_image_height = Some(MAX_LAYER_DIMENSION);
            limits.max_alloc = Some(MAX_CAPTURE_PIXELS.saturating_mul(4));
            reader.limits(limits);
            let decoded = reader
                .decode()
                .map_err(|error| invalid(&format!("retained image failed to decode: {error}")))?;
            Ok((decoded.to_rgba8(), None, None, true))
        }
    }
}

/// Source-over composite of one decoded layer into the canvas.
fn blend(canvas: &mut RgbaImage, source: &RgbaImage, rect: (i64, i64, i64, i64)) {
    let (left, top, right, bottom) = rect;
    let (dest_w, dest_h) = ((right - left) as u32, (bottom - top) as u32);
    let scaled;
    let source = if source.width() == dest_w && source.height() == dest_h {
        source
    } else {
        scaled = image::imageops::resize(
            source,
            dest_w.max(1),
            dest_h.max(1),
            image::imageops::FilterType::Triangle,
        );
        &scaled
    };
    for row in 0..dest_h {
        for column in 0..dest_w {
            let src =
                source.get_pixel(column.min(source.width() - 1), row.min(source.height() - 1));
            let alpha = u32::from(src.0[3]);
            if alpha == 0 {
                continue;
            }
            let target = canvas.get_pixel_mut(left as u32 + column, top as u32 + row);
            if alpha == 255 {
                *target = *src;
                continue;
            }
            for channel in 0..3 {
                let over = u32::from(src.0[channel]) * alpha;
                let under = u32::from(target.0[channel]) * (255 - alpha);
                target.0[channel] = ((over + under) / 255) as u8;
            }
            target.0[3] = target.0[3].max(src.0[3]);
        }
    }
}

/// Compose every layer, back to front, into one PNG.
///
/// Layers arrive already ordered and already filtered to those the gateway holds pixels for, so an
/// empty result means the pane has no retained visual media rather than that something failed.
pub fn compose(layers: &[CaptureLayer], target: CaptureTarget) -> io::Result<Captured> {
    let (width, height) = target
        .pixel_size()
        .ok_or_else(|| invalid("pane geometry does not describe a drawable rectangle"))?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| invalid("pane pixel count overflows"))?;
    if pixels > MAX_CAPTURE_PIXELS {
        return Err(invalid(&format!(
            "capture of {width}x{height} exceeds the {MAX_CAPTURE_PIXELS} pixel budget"
        )));
    }
    let mut canvas = RgbaImage::new(width, height);
    let mut composed = Vec::new();
    for layer in layers {
        let Some(rect) = destination(layer, target, (width, height)) else {
            continue;
        };
        let (source, frame_id, epoch, encoded_image) = decode_layer(layer)?;
        if source.width() == 0 || source.height() == 0 {
            continue;
        }
        composed.push(CapturedLayer {
            source: layer.source,
            node_id: layer.node_id,
            source_width: source.width(),
            source_height: source.height(),
            frame_id,
            epoch,
            encoded_image,
        });
        blend(&mut canvas, &source, rect);
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            canvas.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| io::Error::other(format!("could not encode capture: {error}")))?;
    Ok(Captured {
        width,
        height,
        png,
        layers: composed,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vivid_gateway::{RetainedRaster, SourceKey};

    use super::*;

    fn target() -> CaptureTarget {
        CaptureTarget {
            columns: 10,
            rows: 4,
            cell_width: 8,
            cell_height: 16,
        }
    }

    fn source_key(track: u64) -> SourceKey {
        SourceKey {
            producer: 1,
            context: 1,
            surface: 1,
            track,
        }
    }

    fn raster_layer(
        track: u64,
        z: i64,
        cells: (i64, i64, i64, i64),
        rgba: [u8; 4],
    ) -> CaptureLayer {
        let (width, height) = ((cells.2 * 8) as u32, (cells.3 * 16) as u32);
        let pixels = rgba.repeat((width * height) as usize);
        CaptureLayer {
            source: source_key(track),
            node_id: track,
            z_index: z,
            x: cells.0 * CELL_FIXED_ONE,
            y: cells.1 * CELL_FIXED_ONE,
            width: cells.2 * CELL_FIXED_ONE,
            height: cells.3 * CELL_FIXED_ONE,
            clip: None,
            content: CaptureContent::Raster(RetainedRaster {
                epoch: 1,
                frame_id: 7,
                width,
                height,
                pixels: Arc::from(pixels),
            }),
        }
    }

    fn decode(png: &[u8]) -> RgbaImage {
        image::load_from_memory(png).unwrap().to_rgba8()
    }

    #[test]
    fn a_pane_with_no_retained_media_composes_a_blank_canvas() {
        let captured = compose(&[], target()).unwrap();
        assert_eq!((captured.width, captured.height), (80, 64));
        assert!(captured.layers.is_empty());
        assert!(decode(&captured.png).pixels().all(|pixel| pixel.0[3] == 0));
    }

    #[test]
    fn a_full_pane_raster_lands_pixel_for_pixel() {
        let captured = compose(
            &[raster_layer(4, 0, (0, 0, 10, 4), [10, 20, 30, 255])],
            target(),
        )
        .unwrap();
        let image = decode(&captured.png);
        assert_eq!(image.dimensions(), (80, 64));
        assert!(image.pixels().all(|pixel| pixel.0 == [10, 20, 30, 255]));
        assert_eq!(captured.layers.len(), 1);
        assert_eq!(captured.layers[0].frame_id, Some(7));
        assert_eq!(
            (
                captured.layers[0].source_width,
                captured.layers[0].source_height
            ),
            (80, 64)
        );
    }

    #[test]
    fn a_later_node_covers_an_earlier_one_where_they_overlap() {
        // Z order is information, not decoration: a capture that ignored it would show whichever
        // node the map happened to yield last.
        let under = raster_layer(4, 0, (0, 0, 10, 4), [255, 0, 0, 255]);
        let over = raster_layer(5, 1, (0, 0, 5, 4), [0, 255, 0, 255]);
        let captured = compose(&[under, over], target()).unwrap();
        let image = decode(&captured.png);
        assert_eq!(image.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert_eq!(image.get_pixel(39, 63).0, [0, 255, 0, 255]);
        assert_eq!(image.get_pixel(40, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn a_node_is_cut_to_its_clip_and_to_the_pane() {
        let mut layer = raster_layer(4, 0, (0, 0, 10, 4), [0, 0, 255, 255]);
        layer.clip = Some(ClipRect {
            x: 0,
            y: 0,
            width: 2 * CELL_FIXED_ONE,
            height: 4 * CELL_FIXED_ONE,
        });
        let captured = compose(&[layer], target()).unwrap();
        let image = decode(&captured.png);
        assert_eq!(image.get_pixel(15, 0).0, [0, 0, 255, 255]);
        assert_eq!(image.get_pixel(16, 0).0[3], 0, "clipped away");

        // A node reaching past the pane is cut by the pane too, not merely by its own clip.
        let overflowing = raster_layer(6, 0, (8, 0, 10, 4), [0, 0, 255, 255]);
        let captured = compose(&[overflowing], target()).unwrap();
        assert_eq!(decode(&captured.png).dimensions(), (80, 64));
    }

    #[test]
    fn a_capture_beyond_the_pixel_budget_is_refused_before_allocating() {
        let huge = CaptureTarget {
            columns: 10_000,
            rows: 10_000,
            cell_width: 8,
            cell_height: 16,
        };
        let error = compose(&[], huge).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("pixel budget"));
    }

    #[test]
    fn a_retained_raster_that_lies_about_its_size_is_rejected() {
        let mut layer = raster_layer(4, 0, (0, 0, 10, 4), [1, 2, 3, 255]);
        layer.content = CaptureContent::Raster(RetainedRaster {
            epoch: 1,
            frame_id: 7,
            width: 80,
            height: 64,
            pixels: Arc::from(vec![0_u8; 16]),
        });
        let error = compose(&[layer], target()).unwrap_err();
        assert!(error.to_string().contains("does not match its dimensions"));
    }

    #[test]
    fn a_degenerate_pane_rectangle_is_refused() {
        let empty = CaptureTarget {
            columns: 0,
            rows: 4,
            cell_width: 8,
            cell_height: 16,
        };
        assert!(compose(&[], empty).is_err());
    }
}
