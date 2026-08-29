//! Support for JPEG 2000 image decoding.

use dcmnorm_encoding::adapters::{decode_error, DecodeResult, PixelDataObject, PixelDataReader};
use dcmnorm_encoding::snafu::prelude::*;
use jpeg2k::Image;
use std::borrow::Cow;
use tracing::warn;

// Check jpeg2k backend conflicts
#[cfg(all(feature = "openjp2", feature = "openjpeg-sys"))]
compile_error!(
    "feature \"openjp2\" and feature \"openjpeg-sys\" cannot be enabled at the same time"
);

/// Pixel data adapter for transfer syntaxes based on JPEG 2000.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Jpeg2000Adapter;

impl PixelDataReader for Jpeg2000Adapter {
    /// Decode a single frame in JPEG 2000 from a DICOM object.
    fn decode_frame(
        &self,
        src: &dyn PixelDataObject,
        frame: u32,
        dst: &mut Vec<u8>,
    ) -> DecodeResult<()> {
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

        let bytes_per_sample = bits_allocated / 8;

        // `stride` it the total number of bytes for each sample plane
        let stride: usize = bytes_per_sample as usize * cols as usize * rows as usize;
        dst.reserve_exact(samples_per_pixel as usize * stride);
        let base_offset = dst.len();
        dst.resize(base_offset + (samples_per_pixel as usize * stride), 0);

        // Common case first, checked without ever fetching the full `raw_pixel_data()` (which
        // clones every fragment) - see `jpeg.rs`'s `decode_frame` for the full rationale (called
        // once per frame for a multi-frame series, so this matters).
        let number_of_fragments = src.number_of_fragments().unwrap_or(0) as usize;
        let frame_data = if number_of_fragments == 1 || number_of_fragments == nr_frames {
            // assuming 1:1 frame-to-fragment mapping
            src.fragment(frame as usize).with_whatever_context(|| {
                format!("Missing fragment #{} for the frame requested", frame)
            })?
        } else {
            // Some embedded JPEGs might span multiple fragments. This is the rare path (most
            // real encoders keep a 1:1 mapping), so the extra clone cost of `raw_pixel_data()`
            // here is acceptable - it's genuinely needed to look up the basic offset table and
            // gather all of the frame's fragments in a single vector.
            // Note: not the most efficient way to do this,
            // consider optimizing later with byte chunk readers
            let raw = src
                .raw_pixel_data()
                .whatever_context("Expected to have raw pixel data available")?;

            let base_offset = raw.offset_table.get(frame as usize).copied();
            let base_offset = if frame == 0 {
                base_offset.unwrap_or(0) as usize
            } else {
                base_offset
                    .with_whatever_context(|| format!("Missing offset for frame #{}", frame))?
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

            Cow::Owned(fragments)
        };

        let image = Image::from_bytes(&frame_data).whatever_context("jpeg2k decoder failure")?;

        // Note: we cannot use `get_pixels`
        // because the current implementation narrows the data
        // down to 8 bits per sample
        let components = image.components();

        // write each component into the destination buffer
        for (component_i, component) in components.iter().enumerate() {
            if component_i > samples_per_pixel as usize {
                warn!(
                    "JPEG 2000 image has more components than expected ({} > {})",
                    component_i, samples_per_pixel
                );
                break;
            }

            // write in standard layout
            for (i, sample) in component.data().iter().enumerate() {
                let offset = base_offset
                    + i * samples_per_pixel as usize * bytes_per_sample as usize
                    + component_i * bytes_per_sample as usize;
                dst[offset..offset + bytes_per_sample as usize]
                    .copy_from_slice(&sample.to_le_bytes()[..bytes_per_sample as usize]);
            }
        }

        Ok(())
    }
}
