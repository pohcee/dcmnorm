//! JPEG 2000 pixel data decoding.
//!
//! - [`Jpeg2000Adapter`]: classic JPEG 2000 (`.90`-`.93`) via OpenJPEG.
//! - [`Htj2kAdapter`]: High-Throughput JPEG 2000 (`.201`-`.203`) via OpenHTJ2K, falling back to
//!   OpenJPEG if OpenHTJ2K rejects a codestream. OpenJPEG alone can't decode HTJ2K that uses
//!   several quality layers (placeholder passes), which real devices emit.
//!
//! Both codecs live in the `dcmnorm-jpeg2000` crate, which also owns the threading policy.

use dcmnorm_encoding::adapters::{decode_error, DecodeResult, PixelDataObject, PixelDataReader};
use dcmnorm_encoding::snafu::prelude::*;
use dcmnorm_jpeg2000::htj2k::{self, DecodeIntoError};
use dcmnorm_jpeg2000::{openjpeg, DecodedImage};
use std::borrow::Cow;
use tracing::warn;

/// Pixel data adapter for classic JPEG 2000 transfer syntaxes, decoded with OpenJPEG.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Jpeg2000Adapter;

/// Pixel data adapter for High-Throughput JPEG 2000 transfer syntaxes, decoded with OpenHTJ2K.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Htj2kAdapter;

/// The frame geometry the DICOM header declares, plus the destination slot for it in `dst`.
struct FrameLayout {
    rows: u32,
    cols: u32,
    samples_per_pixel: u16,
    bytes_per_sample: usize,
    base_offset: usize,
}

/// Validates the header, appends a zeroed slot for one frame to `dst`, and returns that frame's
/// codestream bytes.
fn prepare_frame<'a>(
    src: &'a dyn PixelDataObject,
    frame: u32,
    dst: &mut Vec<u8>,
) -> DecodeResult<(FrameLayout, Cow<'a, [u8]>)> {
    let cols = src
        .cols()
        .context(decode_error::MissingAttributeSnafu { name: "Columns" })?;
    let rows = src
        .rows()
        .context(decode_error::MissingAttributeSnafu { name: "Rows" })?;
    let samples_per_pixel =
        src.samples_per_pixel()
            .context(decode_error::MissingAttributeSnafu {
                name: "SamplesPerPixel",
            })?;
    let bits_allocated = src
        .bits_allocated()
        .context(decode_error::MissingAttributeSnafu {
            name: "BitsAllocated",
        })?;

    ensure_whatever!(
        bits_allocated == 8 || bits_allocated == 16,
        "BitsAllocated other than 8 or 16 is not supported"
    );

    let nr_frames = src.number_of_frames().unwrap_or(1) as usize;

    ensure!(
        nr_frames > frame as usize,
        decode_error::FrameRangeOutOfBoundsSnafu
    );

    let bytes_per_sample = (bits_allocated / 8) as usize;
    let frame_len = samples_per_pixel as usize * bytes_per_sample * cols as usize * rows as usize;
    dst.reserve_exact(frame_len);
    let base_offset = dst.len();
    dst.resize(base_offset + frame_len, 0);

    let layout = FrameLayout {
        rows: u32::from(rows),
        cols: u32::from(cols),
        samples_per_pixel,
        bytes_per_sample,
        base_offset,
    };
    Ok((layout, frame_codestream(src, frame, nr_frames)?))
}

/// The codestream for `frame`: its single fragment in the common 1:1 frame-to-fragment layout,
/// otherwise the fragments the basic offset table assigns to it.
fn frame_codestream(
    src: &dyn PixelDataObject,
    frame: u32,
    nr_frames: usize,
) -> DecodeResult<Cow<'_, [u8]>> {
    // Common case first, checked without ever fetching the full `raw_pixel_data()` (which
    // clones every fragment) - see `jpeg.rs`'s `decode_frame` for the full rationale (called
    // once per frame for a multi-frame series, so this matters).
    let number_of_fragments = src.number_of_fragments().unwrap_or(0) as usize;
    if number_of_fragments == 1 || number_of_fragments == nr_frames {
        // assuming 1:1 frame-to-fragment mapping
        return src.fragment(frame as usize).with_whatever_context(|| {
            format!("Missing fragment #{} for the frame requested", frame)
        });
    }

    // Some embedded codestreams span multiple fragments. This is the rare path (most real
    // encoders keep a 1:1 mapping), so the extra clone cost of `raw_pixel_data()` here is
    // acceptable - it's genuinely needed to look up the basic offset table and gather all of the
    // frame's fragments in a single vector.
    let raw = src
        .raw_pixel_data()
        .whatever_context("Expected to have raw pixel data available")?;

    let base_offset = raw.offset_table.get(frame as usize).copied();
    let base_offset = if frame == 0 {
        base_offset.unwrap_or(0) as usize
    } else {
        base_offset.with_whatever_context(|| format!("Missing offset for frame #{}", frame))?
            as usize
    };
    let next_offset = raw.offset_table.get(frame as usize + 1);

    let mut offset = 0;
    let mut fragments = Vec::new();
    for fragment in &raw.fragments {
        // include it
        if offset >= base_offset {
            fragments.extend_from_slice(fragment);
        }
        offset += fragment.len() + 8;
        if let Some(&next_offset) = next_offset {
            if offset >= next_offset as usize {
                // next fragment is for the next frame
                break;
            }
        }
    }

    Ok(Cow::Owned(fragments))
}

/// Writes decoded component planes into the frame's slot in standard (interleaved) layout.
fn write_components(image: &DecodedImage, layout: &FrameLayout, dst: &mut [u8]) {
    let spp = layout.samples_per_pixel as usize;
    let bps = layout.bytes_per_sample;
    let frame_end = layout.base_offset + spp * bps * layout.cols as usize * layout.rows as usize;
    for (component_i, component) in image.components.iter().enumerate() {
        if component_i >= spp {
            warn!(
                "JPEG 2000 image has more components than expected ({} > {})",
                image.components.len(),
                spp
            );
            break;
        }
        for (i, sample) in component.data.iter().enumerate() {
            let offset = layout.base_offset + i * spp * bps + component_i * bps;
            if offset + bps > frame_end {
                break;
            }
            dst[offset..offset + bps].copy_from_slice(&sample.to_le_bytes()[..bps]);
        }
    }
}

impl PixelDataReader for Jpeg2000Adapter {
    /// Decode a single frame in JPEG 2000 from a DICOM object.
    fn decode_frame(
        &self,
        src: &dyn PixelDataObject,
        frame: u32,
        dst: &mut Vec<u8>,
    ) -> DecodeResult<()> {
        let (layout, codestream) = prepare_frame(src, frame, dst)?;
        let image = match openjpeg::decode(&codestream) {
            Ok(image) => image,
            Err(error) => {
                whatever!("jpeg2k decoder failure: {}", error);
            }
        };
        write_components(&image, &layout, dst);
        Ok(())
    }
}

impl PixelDataReader for Htj2kAdapter {
    /// Decode a single frame in High-Throughput JPEG 2000 from a DICOM object.
    fn decode_frame(
        &self,
        src: &dyn PixelDataObject,
        frame: u32,
        dst: &mut Vec<u8>,
    ) -> DecodeResult<()> {
        let (layout, codestream) = prepare_frame(src, frame, dst)?;
        let frame_len = layout.samples_per_pixel as usize
            * layout.bytes_per_sample
            * layout.cols as usize
            * layout.rows as usize;
        let slot = &mut dst[layout.base_offset..layout.base_offset + frame_len];

        let htj2k_error = match htj2k::decode_into(
            &codestream,
            slot,
            layout.bytes_per_sample as u32,
            layout.samples_per_pixel,
            layout.cols,
            layout.rows,
        ) {
            Ok(()) => return Ok(()),
            // Header and codestream disagree (e.g. a component count the caller didn't correct
            // for): decode to planes and lay out whatever matches, like the classic adapter does.
            Err(DecodeIntoError::LayoutMismatch { .. }) => match htj2k::decode(&codestream) {
                Ok(image) => {
                    write_components(&image, &layout, dst);
                    return Ok(());
                }
                Err(error) => error,
            },
            Err(DecodeIntoError::Failed(error)) => error,
        };

        let image = match openjpeg::decode(&codestream) {
            Ok(image) => image,
            Err(openjpeg_error) => {
                whatever!(
                    "HTJ2K decoder failure: {}; OpenJPEG fallback also failed: {}",
                    htj2k_error,
                    openjpeg_error
                );
            }
        };
        warn!("OpenHTJ2K failed ({htj2k_error}); decoded with OpenJPEG instead");
        write_components(&image, &layout, dst);
        Ok(())
    }
}
