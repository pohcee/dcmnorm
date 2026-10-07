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

/// Pixel data decoder for RLE Lossless (UID `1.2.840.10008.1.2.5`)
impl PixelDataReader for RleLosslessAdapter {
    /// Decode the DICOM image from RLE Lossless completely.
    ///
    /// See <https://dicom.nema.org/medical/dicom/2023e/output/chtml/part05/chapter_G.html>
    fn decode(&self, src: &dyn PixelDataObject, dst: &mut Vec<u8>) -> DecodeResult<()> {
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

        if bits_allocated != 8 && bits_allocated != 16 {
            whatever!("BitsAllocated other than 8 or 16 is not supported");
        }
        // For RLE the number of fragments = number of frames
        // therefore, we can fetch the fragments one by one
        let nr_frames =
            src.number_of_fragments()
                .whatever_context("Invalid pixel data, no fragments found")? as usize;
        let bytes_per_sample = (bits_allocated / 8) as usize;
        let samples_per_pixel = samples_per_pixel as usize;
        // `stride` is the total number of bytes for each sample plane
        let stride = bytes_per_sample * cols as usize * rows as usize;
        let frame_size = stride * samples_per_pixel;

        // extend `dst` to make room for decoded pixel data
        let base_offset = dst.len();
        // estimate the sizes of the encapsulated fragments
        let encapsulated_size = src.fragment(0).unwrap_or_default().len() * nr_frames;
        guarded_resize(dst, frame_size * nr_frames, encapsulated_size)?;

        // RLE encoded data is ordered like this (for 16-bit, 3 sample):
        //  Segment: 0     | 1     | 2     | 3     | 4     | 5
        //           R MSB | R LSB | G MSB | G LSB | B MSB | B LSB
        //  A segment contains only the MSB or LSB parts of all the sample pixels

        // As currently required,
        // we need to rearrange the pixel data to standard planar configuration.
        // (and use little endian byte ordering):
        //    Pixel 1                             | ... Pixel N
        //    Red         Green       Blue        | ...
        //    LSB R MSB R LSB G MSB G LSB B MSB B | ...

        for i in 0..nr_frames {
            let fragment = &src
                .fragment(i)
                .whatever_context("No pixel data found for frame")?;
            let mut offsets = read_rle_header(fragment)?;
            offsets.push(fragment.len() as u32);

            for sample_number in 0..samples_per_pixel {
                for byte_offset in (0..bytes_per_sample).rev() {
                    // ii is 1, 0, 3, 2, 5, 4 for the example above
                    // This is where the segment order correction occurs
                    let ii = sample_number * bytes_per_sample + byte_offset;
                    ensure_whatever!(ii + 1 < offsets.len(), "Invalid RLE segment offsets");
                    let segment_range = offsets[ii] as usize..offsets[ii + 1] as usize;
                    let segment = fragment
                        .get(segment_range)
                        .whatever_context("Invalid RLE segment range")?;
                    let buff = io::Cursor::new(segment);
                    let (_, decoder) = PackBitsReader::new(buff, segment.len())
                        .whatever_context("Failed to read RLE segments")?;
                    let mut decoded_segment =
                        guarded_alloc(rows as usize * cols as usize, fragment.len())?;
                    decoder
                        .take(rows as u64 * cols as u64)
                        .read_to_end(&mut decoded_segment)
                        .unwrap();

                    // Interleave pixels as described in the example above.
                    // in 16-bit, this is:
                    // MSB R channel: 1,  7, 13, ...
                    // LSB R channel: 0,  6, 12, ...
                    // MSB G channel: 3,  9, 15, ...
                    // LSB G channel: 2,  8, 14, ...
                    // MSB G channel: 5, 11, 17, ...
                    // LSB G channel: 4, 10, 16, ...
                    let frame_start = i * frame_size;
                    let start = frame_start
                        + if samples_per_pixel == 3 {
                            sample_number * bytes_per_sample + byte_offset
                        } else {
                            sample_number * bytes_per_sample + samples_per_pixel - byte_offset
                        };

                    let end = (i + 1) * frame_size;
                    for (decoded_index, dst_index) in (start..end)
                        .step_by(bytes_per_sample * samples_per_pixel)
                        .enumerate()
                    {
                        dst[base_offset + dst_index] = decoded_segment[decoded_index];
                    }
                }
            }
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

        if bits_allocated != 8 && bits_allocated != 16 {
            whatever!("BitsAllocated other than 8 or 16 is not supported");
        }
        // For RLE the number of fragments = number of frames
        // therefore, we can fetch the fragments one by one
        let nr_frames =
            src.number_of_fragments()
                .whatever_context("Invalid pixel data, no fragments found")? as usize;
        ensure!(
            nr_frames > frame as usize,
            decode_error::FrameRangeOutOfBoundsSnafu
        );

        let bytes_per_sample = (bits_allocated / 8) as usize;
        let samples_per_pixel = samples_per_pixel as usize;
        // `stride` is the total number of bytes for each sample plane
        let stride = bytes_per_sample * cols as usize * rows as usize;
        let frame_size = stride * samples_per_pixel;
        let base_offset = dst.len();

        // RLE encoded data is ordered like this (for 16-bit, 3 sample):
        //  Segment: 0     | 1     | 2     | 3     | 4     | 5
        //           R MSB | R LSB | G MSB | G LSB | B MSB | B LSB
        //  A segment contains only the MSB or LSB parts of all the sample pixels

        // As currently required,
        // we need to rearrange the pixel data to standard planar configuration.
        // (and use little endian byte ordering):
        //    Pixel 1                             | ... Pixel N
        //    Red         Green       Blue        | ...
        //    LSB R MSB R LSB G MSB G LSB B MSB B | ...

        let fragment = &src
            .fragment(frame as usize)
            .whatever_context("No pixel data found for frame")?;
        let mut offsets = read_rle_header(fragment)?;
        offsets.push(fragment.len() as u32);

        // extend `dst` to make room for decoded pixel data
        guarded_resize(dst, frame_size, fragment.len())?;

        for sample_number in 0..samples_per_pixel {
            for byte_offset in (0..bytes_per_sample).rev() {
                // ii is 1, 0, 3, 2, 5, 4 for the example above
                // This is where the segment order correction occurs
                let ii = sample_number * bytes_per_sample + byte_offset;
                ensure_whatever!(ii + 1 < offsets.len(), "Invalid RLE segment offsets");
                let segment_range = offsets[ii] as usize..offsets[ii + 1] as usize;
                let segment = fragment
                    .get(segment_range)
                    .whatever_context("Invalid RLE segment range")?;
                let buff = io::Cursor::new(segment);
                let (_, decoder) = PackBitsReader::new(buff, segment.len())
                    .map_err(|e| Box::new(e) as Box<_>)
                    .whatever_context("Failed to read RLE segments")?;
                let mut decoded_segment =
                    guarded_alloc(rows as usize * cols as usize, fragment.len())?;
                decoder
                    .take(rows as u64 * cols as u64)
                    .read_to_end(&mut decoded_segment)
                    .unwrap();

                // Interleave pixels as described in the example above.
                let start = if samples_per_pixel == 3 {
                    sample_number * bytes_per_sample + byte_offset
                } else {
                    sample_number * bytes_per_sample + samples_per_pixel - byte_offset
                };

                let end = frame_size;
                for (decoded_index, dst_index) in (start..end)
                    .step_by(bytes_per_sample * samples_per_pixel)
                    .enumerate()
                {
                    dst[base_offset + dst_index] = decoded_segment[decoded_index];
                }
            }
        }
        Ok(())
    }
}

// TODO(#125) implement `encode`

// Read the RLE header and return the offsets
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
    ensure_whatever!(nr_segments <= 15, "Invalid RLE segment count {}", nr_segments);
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
    crate::alloc::guarded_resize(out, additional_capacity, fragment_size, COMPRESSION_RATIO_THRESHOLD)
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


    /// A single-sample, single-frame RLE image whose one fragment is given verbatim.
    struct RleImage {
        rows: u16,
        cols: u16,
        bits_allocated: u16,
        fragment: Vec<u8>,
    }

    impl PixelDataObject for RleImage {
        fn transfer_syntax_uid(&self) -> &str { "1.2.840.10008.1.2.5" }
        fn rows(&self) -> Option<u16> { Some(self.rows) }
        fn cols(&self) -> Option<u16> { Some(self.cols) }
        fn samples_per_pixel(&self) -> Option<u16> { Some(1) }
        fn bits_allocated(&self) -> Option<u16> { Some(self.bits_allocated) }
        fn bits_stored(&self) -> Option<u16> { Some(self.bits_allocated) }
        fn photometric_interpretation(&self) -> Option<&str> { Some("MONOCHROME2") }
        fn number_of_frames(&self) -> Option<u32> { Some(1) }
        fn number_of_fragments(&self) -> Option<u32> { Some(1) }
        fn fragment(&self, fragment: usize) -> Option<std::borrow::Cow<'_, [u8]>> {
            (fragment == 0).then(|| std::borrow::Cow::Borrowed(&self.fragment[..]))
        }
        fn offset_table(&self) -> Option<std::borrow::Cow<'_, [u32]>> { None }
        fn raw_pixel_data(&self) -> Option<dcmnorm_encoding::adapters::RawPixelData> { None }
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

    /// Decode both ways (whole image, and frame 0), which have separate code paths.
    fn decode_both(image: &RleImage) -> [DecodeResult<Vec<u8>>; 2] {
        let mut whole = Vec::new();
        let whole = RleLosslessAdapter.decode(image, &mut whole).map(|_| whole);
        let mut frame = Vec::new();
        let frame = RleLosslessAdapter.decode_frame(image, 0, &mut frame).map(|_| frame);
        [whole, frame]
    }

    /// Port of upstream dicom-rs's `read_rle_dangerous_image` (10f9b6db): a 66-byte fragment
    /// declaring a ~3 GB image must be refused instead of allocated.
    #[test]
    fn refuses_to_allocate_for_an_implausibly_compressed_image() {
        let mut fragment = vec![0; 66];
        fragment[0] = 1;
        let image = RleImage { rows: 51220, cols: 58804, bits_allocated: 8, fragment };
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
            ("fragment shorter than its header", vec![1, 0, 0, 0, 64, 0, 0, 0]),
            ("segment count over 15", huge_count),
            ("segment offset past the end of the fragment", offset_past_end),
            ("no segments for an 8-bit sample", rle_header(&[])),
        ];
        for (name, fragment) in cases {
            let image = RleImage { rows: 1, cols: 2, bits_allocated: 8, fragment };
            for result in decode_both(&image) {
                assert!(result.is_err(), "{}: expected an error, got {:?}", name, result);
            }
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
        assert!(fragment.len() * 32 < rows as usize * cols as usize, "the test image should exceed 32:1");

        let image = RleImage { rows, cols, bits_allocated: 8, fragment };
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
        let image = RleImage { rows: 1, cols: 2, bits_allocated: 16, fragment };
        for result in decode_both(&image) {
            assert_eq!(result.unwrap(), vec![0x34, 0x12, 0xCD, 0xAB]);
        }
    }
}
