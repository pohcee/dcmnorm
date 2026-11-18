//! Support for RLE Lossless image decoding.
//!
//! implementation taken from Pydicom:
//! <https://github.com/pydicom/pydicom/blob/master/pydicom/pixel_data_handlers/rle_handler.py>
//!
//! Copyright 2008-2021 pydicom authors.
//!
//! License: <https://github.com/pydicom/pydicom/blob/master/LICENSE>
use byteordered::byteorder::{ByteOrder, LittleEndian};

use dcmnorm_encoding::adapters::{decode_error, DecodeResult, PixelDataObject, PixelDataReader};
use dcmnorm_encoding::snafu::prelude::*;
use std::io::{self, Read, Seek};

/// Pixel data adapter for the RLE Lossless transfer syntax.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RleLosslessAdapter;

/// The shape of one decoded frame, read from the pixel data attributes.
#[derive(Debug, Copy, Clone)]
struct FrameLayout {
    /// Rows times Columns: the number of bytes each RLE segment decodes to.
    pixels: usize,
    samples_per_pixel: usize,
    bytes_per_sample: usize,
}

impl FrameLayout {
    fn read(src: &dyn PixelDataObject) -> DecodeResult<Self> {
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

        if bits_allocated != 8 && bits_allocated != 16 && bits_allocated != 32 {
            whatever!("BitsAllocated other than 8, 16 or 32 is not supported");
        }
        ensure_whatever!(samples_per_pixel > 0, "SamplesPerPixel must not be 0");
        let layout = FrameLayout {
            pixels: rows as usize * cols as usize,
            samples_per_pixel: samples_per_pixel as usize,
            bytes_per_sample: (bits_allocated / 8) as usize,
        };
        // PS3.5 G.5: the header holds at most 15 segment offsets
        ensure_whatever!(
            layout.segments() <= 15,
            "{} samples per pixel at {} bits allocated need {} RLE segments, more than the 15 an RLE frame can hold",
            samples_per_pixel,
            bits_allocated,
            layout.segments()
        );
        Ok(layout)
    }

    /// The number of RLE segments a frame of this layout consists of (PS3.5 G.2).
    fn segments(&self) -> usize {
        self.samples_per_pixel * self.bytes_per_sample
    }

    /// The size in bytes of one decoded frame.
    fn frame_size(&self) -> usize {
        self.pixels * self.segments()
    }
}

/// Pixel data decoder for RLE Lossless (UID `1.2.840.10008.1.2.5`)
///
/// Both methods decode to native little-endian samples, pixel-interleaved (`R G B R G B ...`,
/// Planar Configuration 0) whatever the Planar Configuration of the source: an RLE frame is
/// always encoded colour-by-plane, one segment per byte of each sample (PS3.5 G.2), so that
/// attribute does not describe the compressed data, and the decoded data set is relabelled
/// as interleaved.
impl PixelDataReader for RleLosslessAdapter {
    /// Decode the DICOM image from RLE Lossless completely.
    ///
    /// See <https://dicom.nema.org/medical/dicom/2023e/output/chtml/part05/chapter_G.html>
    fn decode(&self, src: &dyn PixelDataObject, dst: &mut Vec<u8>) -> DecodeResult<()> {
        let layout = FrameLayout::read(src)?;
        // For RLE the number of fragments = number of frames
        // therefore, we can fetch the fragments one by one
        let nr_frames =
            src.number_of_fragments()
                .whatever_context("Invalid pixel data, no fragments found")? as usize;
        let frame_size = layout.frame_size();

        // extend `dst` to make room for decoded pixel data
        let base_offset = dst.len();
        // estimate the sizes of the encapsulated fragments
        let encapsulated_size = src.fragment(0).unwrap_or_default().len() * nr_frames;
        guarded_resize(dst, frame_size * nr_frames, encapsulated_size)?;

        for i in 0..nr_frames {
            let fragment = &src
                .fragment(i)
                .whatever_context("No pixel data found for frame")?;
            let frame_start = base_offset + i * frame_size;
            decode_rle_frame(
                fragment,
                layout,
                &mut dst[frame_start..frame_start + frame_size],
            )?;
        }
        Ok(())
    }

    /// Decode a single frame of the DICOM image from RLE Lossless.
    ///
    /// See <https://dicom.nema.org/medical/dicom/2023e/output/chtml/part05/chapter_G.html>
    fn decode_frame(
        &self,
        src: &dyn PixelDataObject,
        frame: u32,
        dst: &mut Vec<u8>,
    ) -> DecodeResult<()> {
        let layout = FrameLayout::read(src)?;
        // For RLE the number of fragments = number of frames
        // therefore, we can fetch the fragments one by one
        let nr_frames =
            src.number_of_fragments()
                .whatever_context("Invalid pixel data, no fragments found")? as usize;
        ensure!(
            nr_frames > frame as usize,
            decode_error::FrameRangeOutOfBoundsSnafu
        );

        let fragment = &src
            .fragment(frame as usize)
            .whatever_context("No pixel data found for frame")?;

        // extend `dst` to make room for decoded pixel data
        let frame_size = layout.frame_size();
        let base_offset = dst.len();
        guarded_resize(dst, frame_size, fragment.len())?;
        decode_rle_frame(
            fragment,
            layout,
            &mut dst[base_offset..base_offset + frame_size],
        )
    }
}

// TODO(#125) implement `encode`

/// Decode one RLE frame (one fragment) into `out`, which is exactly one decoded frame long.
///
/// PS3.5 G.2 orders the segments by sample, and within each sample by byte significance, most
/// significant byte first. For 16-bit RGB, say:
///
/// ```text
///  Segment: 0     | 1     | 2     | 3     | 4     | 5
///           R MSB | R LSB | G MSB | G LSB | B MSB | B LSB
/// ```
///
/// Each segment holds that one byte of every pixel, in pixel order. The output is native
/// little endian and pixel-interleaved, so the byte of significance `k` (0 = most significant)
/// of sample `s` of pixel `p` goes to
///
/// ```text
/// (p * samples_per_pixel + s) * bytes_per_sample + (bytes_per_sample - 1 - k)
/// ```
///
/// which for the example above is:
///
/// ```text
///    Pixel 0                             | ... Pixel N
///    Red         Green       Blue        | ...
///    LSB R MSB R LSB G MSB G LSB B MSB B | ...
///    seg 1 seg 0 seg 3 seg 2 seg 5 seg 4 | ...
/// ```
fn decode_rle_frame(fragment: &[u8], layout: FrameLayout, out: &mut [u8]) -> DecodeResult<()> {
    debug_assert_eq!(out.len(), layout.frame_size());
    let mut offsets = read_rle_header(fragment)?;
    // A header that declares a different number of segments than the pixel attributes call for
    // describes some other image (BitsAllocated or SamplesPerPixel wrong): decoding just the
    // segments the attributes ask for would succeed with the wrong samples.
    ensure_whatever!(
        offsets.len() == layout.segments(),
        "RLE frame has {} segments, but {} samples per pixel at {} bits allocated need {}",
        offsets.len(),
        layout.samples_per_pixel,
        layout.bytes_per_sample * 8,
        layout.segments()
    );
    offsets.push(fragment.len() as u32);

    let pixel_stride = layout.samples_per_pixel * layout.bytes_per_sample;
    for segment_index in 0..layout.segments() {
        let (start, end) = (
            offsets[segment_index] as usize,
            offsets[segment_index + 1] as usize,
        );
        // a segment starting inside the header would decode the header as samples
        ensure_whatever!(
            start >= 64,
            "Invalid RLE segment offset {} for segment {}",
            start,
            segment_index
        );
        let segment = fragment
            .get(start..end)
            .whatever_context("Invalid RLE segment range")?;
        let (_, decoder) = PackBitsReader::new(io::Cursor::new(segment), segment.len())
            .whatever_context("Failed to read RLE segments")?;
        let mut decoded_segment = guarded_alloc(layout.pixels, fragment.len())?;
        decoder
            .take(layout.pixels as u64)
            .read_to_end(&mut decoded_segment)
            .whatever_context("Failed to decode RLE segment")?;
        // a corrupt segment can decode short, which would leave samples unset
        ensure_whatever!(
            decoded_segment.len() == layout.pixels,
            "RLE segment decoded to {} bytes, expected {}",
            decoded_segment.len(),
            layout.pixels
        );

        let sample = segment_index / layout.bytes_per_sample;
        let significance = segment_index % layout.bytes_per_sample; // 0 = most significant
        let first = sample * layout.bytes_per_sample + (layout.bytes_per_sample - 1 - significance);
        for (byte, value) in out[first..]
            .iter_mut()
            .step_by(pixel_stride)
            .zip(decoded_segment)
        {
            *byte = value;
        }
    }
    Ok(())
}

/// Read the segment offsets from an RLE fragment's header: per PS3.5 G.5, a fixed 64 bytes
/// holding the segment count (at most 15) followed by 15 offset slots. Checked rather than
/// trusted, since the count comes from the file: a short fragment or an oversized count used to
/// panic on an out-of-range slice, or attempt an allocation of up to 16 GiB.
fn read_rle_header(fragment: &[u8]) -> DecodeResult<Vec<u32>> {
    ensure_whatever!(
        fragment.len() >= 64,
        "RLE fragment of {} bytes is too short for its 64-byte header",
        fragment.len()
    );
    let nr_segments = LittleEndian::read_u32(&fragment[0..4]) as usize;
    ensure_whatever!(
        nr_segments <= 15,
        "Invalid RLE segment count {}",
        nr_segments
    );
    let mut offsets = vec![0; nr_segments];
    LittleEndian::read_u32_into(&fragment[4..4 * (nr_segments + 1)], &mut offsets);
    Ok(offsets)
}

/// PackBits Reader from the image-tiff crate
/// Copyright 2018-2021 PistonDevelopers.
/// License: <https://github.com/image-rs/image-tiff/blob/master/LICENSE>
/// From: https://github.com/image-rs/image-tiff/blob/master/src/decoder/stream.rs
#[derive(Debug)]
struct PackBitsReader {
    buffer: io::Cursor<Vec<u8>>,
}

impl PackBitsReader {
    /// Wraps a reader
    pub fn new<R: Read + Seek>(
        mut reader: R,
        length: usize,
    ) -> io::Result<(usize, PackBitsReader)> {
        let mut buffer = Vec::new();
        let mut header: [u8; 1] = [0];
        let mut data: [u8; 1] = [0];

        let mut bytes_read = 0;
        while bytes_read < length {
            reader.read_exact(&mut header)?;
            bytes_read += 1;

            let h = header[0] as i8;
            if (-127..=-1).contains(&h) {
                let new_len = buffer.len() + (1 - h as isize) as usize;
                reader.read_exact(&mut data)?;
                buffer.resize(new_len, data[0]);
                bytes_read += 1;
            } else if h >= 0 {
                let num_vals = h as usize + 1;
                io::copy(&mut reader.by_ref().take(num_vals as u64), &mut buffer)?;
                bytes_read += num_vals;
            } else {
                // h = -128 is a no-op.
            }
        }

        Ok((
            buffer.len(),
            PackBitsReader {
                buffer: io::Cursor::new(buffer),
            },
        ))
    }
}

impl Read for PackBitsReader {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.buffer.read(buf)
    }
}

/// Largest decoded/encoded size ratio a valid RLE Lossless frame can have. Upstream dicom-rs uses
/// 32, but PackBits (PS3.5 Annex G) turns a 2-byte replicate run into up to 128 bytes, and rows are
/// encoded separately, so a valid frame - a mostly blank image, say - can approach 64:1. A ratio
/// above 64 cannot decode to the declared size, so rejecting only those never refuses a valid file.
const COMPRESSION_RATIO_THRESHOLD: u32 = 64;

/// Perform an allocation, safeguarded from extreme cases.
fn guarded_alloc(capacity: usize, fragment_size: usize) -> DecodeResult<Vec<u8>> {
    crate::alloc::guarded_alloc(capacity, fragment_size, COMPRESSION_RATIO_THRESHOLD)
        .ok()
        .whatever_context("Could not allocate RLE segment")
}

/// Perform a resize of a vector (with zeros), safeguarded from extreme cases.
fn guarded_resize(
    out: &mut Vec<u8>,
    additional_capacity: usize,
    fragment_size: usize,
) -> DecodeResult<()> {
    crate::alloc::guarded_resize(
        out,
        additional_capacity,
        fragment_size,
        COMPRESSION_RATIO_THRESHOLD,
    )
    .ok()
    .whatever_context("Could not allocate frame")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_packbits() {
        let encoded = vec![
            0xFE, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0xF7,
            0xAA,
        ];
        let encoded_len = encoded.len();

        let buff = io::Cursor::new(encoded);
        let (_, mut decoder) = PackBitsReader::new(buff, encoded_len).unwrap();

        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();

        let expected = vec![
            0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        assert_eq!(decoded, expected);
    }

    /// An RLE image whose fragments (one per frame) are given verbatim.
    struct RleImage {
        rows: u16,
        cols: u16,
        bits_allocated: u16,
        samples_per_pixel: u16,
        fragments: Vec<Vec<u8>>,
    }

    impl RleImage {
        /// A single-sample, single-frame image.
        fn mono(rows: u16, cols: u16, bits_allocated: u16, fragment: Vec<u8>) -> Self {
            RleImage {
                rows,
                cols,
                bits_allocated,
                samples_per_pixel: 1,
                fragments: vec![fragment],
            }
        }
    }

    impl PixelDataObject for RleImage {
        fn transfer_syntax_uid(&self) -> &str {
            "1.2.840.10008.1.2.5"
        }
        fn rows(&self) -> Option<u16> {
            Some(self.rows)
        }
        fn cols(&self) -> Option<u16> {
            Some(self.cols)
        }
        fn samples_per_pixel(&self) -> Option<u16> {
            Some(self.samples_per_pixel)
        }
        fn bits_allocated(&self) -> Option<u16> {
            Some(self.bits_allocated)
        }
        fn bits_stored(&self) -> Option<u16> {
            Some(self.bits_allocated)
        }
        fn photometric_interpretation(&self) -> Option<&str> {
            Some(if self.samples_per_pixel == 3 {
                "RGB"
            } else {
                "MONOCHROME2"
            })
        }
        fn number_of_frames(&self) -> Option<u32> {
            Some(self.fragments.len() as u32)
        }
        fn number_of_fragments(&self) -> Option<u32> {
            Some(self.fragments.len() as u32)
        }
        fn fragment(&self, fragment: usize) -> Option<std::borrow::Cow<'_, [u8]>> {
            self.fragments
                .get(fragment)
                .map(|f| std::borrow::Cow::Borrowed(&f[..]))
        }
        fn offset_table(&self) -> Option<std::borrow::Cow<'_, [u32]>> {
            None
        }
        fn raw_pixel_data(&self) -> Option<dcmnorm_encoding::adapters::RawPixelData> {
            None
        }
    }

    /// A 64-byte RLE header for the given segment offsets.
    fn rle_header(offsets: &[u32]) -> Vec<u8> {
        let mut header = vec![0u8; 64];
        LittleEndian::write_u32(&mut header[0..4], offsets.len() as u32);
        for (i, offset) in offsets.iter().enumerate() {
            LittleEndian::write_u32(&mut header[4 + 4 * i..8 + 4 * i], *offset);
        }
        header
    }

    /// Decode both ways (whole image, and frame by frame concatenated), which have separate
    /// code paths.
    fn decode_both(image: &RleImage) -> [DecodeResult<Vec<u8>>; 2] {
        let mut whole = Vec::new();
        let whole = RleLosslessAdapter.decode(image, &mut whole).map(|_| whole);
        let mut frames = Vec::new();
        let frames = (0..image.fragments.len() as u32)
            .try_for_each(|frame| RleLosslessAdapter.decode_frame(image, frame, &mut frames))
            .map(|_| frames);
        [whole, frames]
    }

    /// Port of upstream dicom-rs's `read_rle_dangerous_image` (10f9b6db): a 66-byte fragment
    /// declaring a ~3 GB image must be refused instead of allocated.
    #[test]
    fn refuses_to_allocate_for_an_implausibly_compressed_image() {
        let mut fragment = vec![0; 66];
        fragment[0] = 1;
        let image = RleImage::mono(51220, 58804, 8, fragment);
        for result in decode_both(&image) {
            assert!(result.is_err());
        }
    }

    /// Malformed headers and segment offsets used to panic on out-of-range slicing (or, for a huge
    /// segment count, attempt a multi-GiB allocation). Each must be a clean error instead.
    #[test]
    fn malformed_rle_fragments_are_errors_not_panics() {
        let mut huge_count = rle_header(&[64]);
        LittleEndian::write_u32(&mut huge_count[0..4], u32::MAX);
        huge_count.extend_from_slice(&[0x00, 0x00]);

        let mut offset_past_end = rle_header(&[64]);
        LittleEndian::write_u32(&mut offset_past_end[4..8], 10_000);
        offset_past_end.extend_from_slice(&[0x00, 0x00]);

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty fragment", vec![]),
            (
                "fragment shorter than its header",
                vec![1, 0, 0, 0, 64, 0, 0, 0],
            ),
            ("segment count over 15", huge_count),
            (
                "segment offset past the end of the fragment",
                offset_past_end,
            ),
            ("no segments for an 8-bit sample", rle_header(&[])),
            ("segment starting inside the header", {
                let mut fragment = rle_header(&[0]);
                fragment.extend_from_slice(&[0x01, 0x11, 0x22]);
                fragment
            }),
        ];
        for (name, fragment) in cases {
            let image = RleImage::mono(1, 2, 8, fragment);
            for result in decode_both(&image) {
                assert!(
                    result.is_err(),
                    "{}: expected an error, got {:?}",
                    name,
                    result
                );
            }
        }
    }

    /// A segment that decodes to fewer bytes than the frame needs used to overrun the
    /// interleaving loop (an index-out-of-bounds panic, found by mutating mr_rle.dcm).
    #[test]
    fn a_segment_that_decodes_short_is_an_error_not_a_panic() {
        let mut fragment = rle_header(&[64]);
        fragment.extend_from_slice(&[0x00, 0x2A]); // literal run of 1 byte, for a 2x2 image
        let image = RleImage::mono(2, 2, 8, fragment);
        for result in decode_both(&image) {
            assert!(result.is_err(), "expected an error, got {:?}", result);
        }
    }

    /// The compression-ratio guard must not refuse valid files. PackBits caps out at 64:1 (a
    /// 2-byte replicate run expands to 128 bytes, rows encoded separately), and a blank image hits
    /// that. This 4096x4096 (16 MiB, past the guard's size floor) all-zero image is ~63.9:1: it is
    /// rejected by upstream's threshold of 32, and must decode with ours.
    #[test]
    fn decodes_a_valid_maximally_compressed_large_image() {
        let (rows, cols) = (4096u16, 4096u16);
        let mut fragment = rle_header(&[64]);
        for _ in 0..rows {
            for _ in 0..cols / 128 {
                // replicate run: repeat the next byte 1 - (-127) = 128 times
                fragment.extend_from_slice(&[0x81, 0x00]);
            }
        }
        assert!(
            fragment.len() * 32 < rows as usize * cols as usize,
            "the test image should exceed 32:1"
        );

        let image = RleImage::mono(rows, cols, 8, fragment);
        for result in decode_both(&image) {
            let decoded = result.expect("a valid RLE image should decode");
            assert_eq!(decoded.len(), rows as usize * cols as usize);
            assert!(decoded.iter().all(|&b| b == 0));
        }
    }

    /// Sanity check that the hardening leaves ordinary decoding intact, including the
    /// most-significant-byte-first segment order of 16-bit samples.
    #[test]
    fn decodes_a_small_16_bit_image() {
        // two pixels, 0x1234 and 0xABCD: segment 0 holds the high bytes, segment 1 the low ones
        let mut fragment = rle_header(&[64, 67]);
        fragment.extend_from_slice(&[0x01, 0x12, 0xAB]); // literal run of 2
        fragment.extend_from_slice(&[0x01, 0x34, 0xCD]);
        let image = RleImage::mono(1, 2, 16, fragment);
        for result in decode_both(&image) {
            assert_eq!(result.unwrap(), vec![0x34, 0x12, 0xCD, 0xAB]);
        }
    }

    /// An RLE frame (PS3.5 G.5): the 64-byte header, then the segments in order. Each segment
    /// is given exactly as it is stored, including any trailing padding byte.
    fn rle_frame(segments: &[&[u8]]) -> Vec<u8> {
        let mut offsets = Vec::new();
        let mut offset = 64u32;
        for segment in segments {
            offsets.push(offset);
            offset += segment.len() as u32;
        }
        let mut frame = rle_header(&offsets);
        for segment in segments {
            frame.extend_from_slice(segment);
        }
        frame
    }

    /// Both decode paths of `image` produce exactly `expected`.
    fn assert_decodes_to(image: &RleImage, expected: &[u8]) {
        for (path, result) in ["decode", "decode_frame"].iter().zip(decode_both(image)) {
            assert_eq!(result.unwrap(), expected, "{path}");
        }
    }

    /// pohcee/dcmnorm#41: 8-bit single-sample samples were written one byte too late - a
    /// leading 0, every sample shifted by one, the last one dropped - and the decode succeeded.
    /// The issue's 2x2 frame: one segment at offset 64, a 2-byte literal run per row.
    #[test]
    fn decodes_8_bit_monochrome_samples_in_place() {
        let frame = rle_frame(&[&[0x01, 0x11, 0x22, 0x01, 0x33, 0x44]]);
        assert_eq!(frame.len(), 70);
        assert_decodes_to(&RleImage::mono(2, 2, 8, frame), &[17, 34, 51, 68]);
    }

    /// An odd width (3), a segment of odd length padded with a zero byte (PS3.5 G.5), and a
    /// replicate run (control byte 0xFE = -2: the next byte 3 times).
    #[test]
    fn decodes_8_bit_monochrome_with_odd_width_padding_and_a_replicate_run() {
        let segment: &[u8] = &[
            0x01, 10, 11, 0x00, 12, // row 0: literal [10, 11], literal [12]
            0xFE, 5,    // row 1: replicate 5 three times
            0x00, // padding to an even segment length
        ];
        let frame = rle_frame(&[segment]);
        assert_decodes_to(&RleImage::mono(2, 3, 8, frame), &[10, 11, 12, 5, 5, 5]);
    }

    /// 16-bit samples: segment 0 holds the most significant bytes, segment 1 the least; the
    /// output is little endian. (This layout was already right before #41: the old expression
    /// `samples_per_pixel - byte_offset` happens to equal `bytes_per_sample - 1 - byte_offset`
    /// when both are 1 and 2.)
    #[test]
    fn decodes_16_bit_monochrome_high_byte_segment_first() {
        // 0x0102 0x0304 / 0xFF00 0x00FF
        let msb: &[u8] = &[0x01, 0x01, 0x03, 0x01, 0xFF, 0x00];
        let lsb: &[u8] = &[0x01, 0x02, 0x04, 0x01, 0x00, 0xFF];
        let frame = rle_frame(&[msb, lsb]);
        assert_decodes_to(
            &RleImage::mono(2, 2, 16, frame),
            &[0x02, 0x01, 0x04, 0x03, 0x00, 0xFF, 0xFF, 0x00],
        );
    }

    /// 32-bit samples: four segments, most significant byte first.
    #[test]
    fn decodes_32_bit_monochrome() {
        // 0x01020304, 0xA0B0C0D0
        let frame = rle_frame(&[
            &[0x01, 0x01, 0xA0],
            &[0x01, 0x02, 0xB0],
            &[0x01, 0x03, 0xC0],
            &[0x01, 0x04, 0xD0],
        ]);
        assert_decodes_to(
            &RleImage::mono(1, 2, 32, frame),
            &[0x04, 0x03, 0x02, 0x01, 0xD0, 0xC0, 0xB0, 0xA0],
        );
    }

    /// 8-bit RGB: one segment per colour plane, decoded to interleaved pixels.
    #[test]
    fn decodes_8_bit_rgb_planes_to_interleaved_pixels() {
        // pixels (1, 2, 7) and (4, 5, 7)
        let red: &[u8] = &[0x01, 1, 4, 0x00];
        let green: &[u8] = &[0x01, 2, 5, 0x00];
        let blue: &[u8] = &[0xFF, 7]; // replicate 7 twice
        let image = RleImage {
            rows: 1,
            cols: 2,
            bits_allocated: 8,
            samples_per_pixel: 3,
            fragments: vec![rle_frame(&[red, green, blue])],
        };
        assert_decodes_to(&image, &[1, 2, 7, 4, 5, 7]);
    }

    /// 16-bit RGB: six segments (R MSB, R LSB, G MSB, ...). Before #41 the most significant
    /// byte of every sample landed first, so the decoded samples were byte-swapped.
    #[test]
    fn decodes_16_bit_rgb_to_little_endian_interleaved_pixels() {
        // pixels (0x1122, 0x3344, 0x5566) and (0x7788, 0x99AA, 0xBBCC)
        let frame = rle_frame(&[
            &[0x01, 0x11, 0x77],
            &[0x01, 0x22, 0x88],
            &[0x01, 0x33, 0x99],
            &[0x01, 0x44, 0xAA],
            &[0x01, 0x55, 0xBB],
            &[0x01, 0x66, 0xCC],
        ]);
        let image = RleImage {
            rows: 1,
            cols: 2,
            bits_allocated: 16,
            samples_per_pixel: 3,
            fragments: vec![frame],
        };
        assert_decodes_to(
            &image,
            &[
                0x22, 0x11, 0x44, 0x33, 0x66, 0x55, 0x88, 0x77, 0xAA, 0x99, 0xCC, 0xBB,
            ],
        );
    }

    /// One fragment per frame: every frame decodes into its own place, and `decode_frame`
    /// picks out the requested one.
    #[test]
    fn decodes_multi_frame_8_bit_monochrome() {
        let frame0 = rle_frame(&[&[0x01, 0x11, 0x22, 0x01, 0x33, 0x44]]);
        let frame1 = rle_frame(&[&[0xFF, 9, 0xFF, 10]]);
        let image = RleImage {
            rows: 2,
            cols: 2,
            bits_allocated: 8,
            samples_per_pixel: 1,
            fragments: vec![frame0, frame1],
        };
        assert_decodes_to(&image, &[17, 34, 51, 68, 9, 9, 10, 10]);
        let mut second = Vec::new();
        RleLosslessAdapter
            .decode_frame(&image, 1, &mut second)
            .unwrap();
        assert_eq!(second, [9, 9, 10, 10]);
        // decode_frame appends after what is already there
        let mut appended = vec![0xEE];
        RleLosslessAdapter
            .decode_frame(&image, 0, &mut appended)
            .unwrap();
        assert_eq!(appended, [0xEE, 17, 34, 51, 68]);
    }

    /// A header whose segment count disagrees with BitsAllocated x SamplesPerPixel describes a
    /// different image: decoding only the segments the attributes ask for would succeed with
    /// the wrong samples, so it is an error.
    #[test]
    fn a_segment_count_that_does_not_match_the_attributes_is_an_error() {
        let two_segments = rle_frame(&[&[0x01, 1, 2], &[0x01, 3, 4]]);
        let one_segment = rle_frame(&[&[0x01, 1, 2]]);
        let cases = [
            (
                "2 segments for 8-bit mono",
                RleImage::mono(1, 2, 8, two_segments.clone()),
            ),
            (
                "1 segment for 16-bit mono",
                RleImage::mono(1, 2, 16, one_segment.clone()),
            ),
            (
                "2 segments for 8-bit RGB",
                RleImage {
                    rows: 1,
                    cols: 2,
                    bits_allocated: 8,
                    samples_per_pixel: 3,
                    fragments: vec![two_segments],
                },
            ),
        ];
        for (name, image) in cases {
            for result in decode_both(&image) {
                assert!(
                    result.is_err(),
                    "{}: expected an error, got {:?}",
                    name,
                    result
                );
            }
        }
    }

    /// Layouts an RLE frame cannot hold (more than 15 segments) or that are not byte-sized.
    #[test]
    fn unsupported_layouts_are_errors() {
        let frame = rle_frame(&[&[0x01, 1, 2]]);
        for (bits_allocated, samples_per_pixel) in [(1, 1), (12, 1), (64, 1), (32, 4), (8, 0)] {
            let image = RleImage {
                rows: 1,
                cols: 2,
                bits_allocated,
                samples_per_pixel,
                fragments: vec![frame.clone()],
            };
            for result in decode_both(&image) {
                assert!(
                    result.is_err(),
                    "{} bits x {}: got {:?}",
                    bits_allocated,
                    samples_per_pixel,
                    result
                );
            }
        }
    }
}
