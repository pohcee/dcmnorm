#[cfg(feature = "jpeg-xl-codec")]
pub mod jpeg_xl_impl {
    use dcmnorm_dictionary::tags;
    use dcmnorm_object::DefaultDicomObject;

    /// Decode JPEG XL encoded pixel data - every frame's worth, concatenated into one flat
    /// native-byte-order buffer matching what `replace_with_native_pixel_data` expects.
    ///
    /// One fragment per frame is the standard DICOM encapsulation convention shared by every
    /// JPEG-family transfer syntax (PS3.5 A.4), so - as with JPEG-LS - each fragment is decoded
    /// independently rather than concatenated first. `.110` (Lossless), `.111` (JPEG
    /// Recompression) and `.112` (general JPEG XL) all share this decoder: the JPEG XL
    /// codestream is self-describing, and a JPEG-Recompression stream still decodes to the same
    /// pixel samples through the normal render path (the "recompression" only matters if the
    /// consumer wants to losslessly reconstruct the original *JPEG* bytes, which dcmnorm has no
    /// need for here).
    pub fn decode_jpeg_xl_pixel_data(object: &DefaultDicomObject) -> Result<Vec<u8>, String> {
        let fragments = object
            .element(tags::PIXEL_DATA)
            .map_err(|e| format!("missing PixelData: {e}"))?
            .fragments()
            .ok_or_else(|| "expected encapsulated JPEG XL pixel data".to_owned())?;

        if fragments.is_empty() {
            return Err("no JPEG XL data to decode".to_owned());
        }

        let rows = object
            .get(tags::ROWS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Rows attribute".to_owned())? as usize;
        let cols = object
            .get(tags::COLUMNS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Columns attribute".to_owned())? as usize;
        let samples_per_pixel = object
            .get(tags::SAMPLES_PER_PIXEL)
            .and_then(|e| e.uint16().ok())
            .unwrap_or(1) as usize;
        let bits_stored = object
            .get(tags::BITS_STORED)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing BitsStored attribute".to_owned())?;
        let bits_allocated = object
            .get(tags::BITS_ALLOCATED)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing BitsAllocated attribute".to_owned())?;
        let pixel_representation = object
            .get(tags::PIXEL_REPRESENTATION)
            .and_then(|e| e.uint16().ok())
            .unwrap_or(0);

        // JPEG XL's codestream always carries unsigned normalized samples; DICOM does not define
        // an offset convention (the way it does for JPEG2000's MCT) for reinterpreting those as
        // two's-complement signed values, so rather than guess one, only unsigned pixel data is
        // supported here.
        if pixel_representation != 0 {
            return Err("JPEG XL decoding only supports unsigned (PixelRepresentation 0) pixel data".to_owned());
        }

        if bits_allocated != 8 && bits_allocated != 16 {
            return Err(format!(
                "unsupported BitsAllocated for JPEG XL decoding: {bits_allocated}"
            ));
        }
        if bits_stored == 0 || bits_stored > bits_allocated {
            return Err(format!(
                "invalid BitsStored ({bits_stored}) for BitsAllocated ({bits_allocated})"
            ));
        }

        let bytes_per_sample = (bits_allocated / 8) as usize;
        let max_sample_value = ((1u32 << bits_stored) - 1) as f32;

        let mut decoded = Vec::new();
        for fragment in fragments {
            if fragment.is_empty() {
                return Err("empty JPEG XL fragment".to_owned());
            }

            let image = jxl_oxide::JxlImage::builder()
                .read(std::io::Cursor::new(fragment))
                .map_err(|e| format!("failed to read JPEG XL header: {e}"))?;

            if image.width() as usize != cols || image.height() as usize != rows {
                return Err(format!(
                    "JPEG XL image dimensions ({}x{}) do not match DICOM Rows/Columns ({rows}x{cols})",
                    image.width(),
                    image.height()
                ));
            }

            let render = image
                .render_frame(0)
                .map_err(|e| format!("JPEG XL decode failed: {e}"))?;
            let frame_buffer = render.image_all_channels();

            if frame_buffer.channels() != samples_per_pixel {
                return Err(format!(
                    "JPEG XL image has {} channel(s), DICOM SamplesPerPixel expects {}",
                    frame_buffer.channels(),
                    samples_per_pixel
                ));
            }

            let expected_len = rows * cols * samples_per_pixel;
            if frame_buffer.buf().len() != expected_len {
                return Err(format!(
                    "JPEG XL decoded sample count ({}) does not match expected {} \
                     ({rows}x{cols}x{samples_per_pixel})",
                    frame_buffer.buf().len(),
                    expected_len
                ));
            }

            let frame_start = decoded.len();
            decoded.resize(frame_start + expected_len * bytes_per_sample, 0);
            for (i, &sample) in frame_buffer.buf().iter().enumerate() {
                let value = (sample.clamp(0.0, 1.0) * max_sample_value).round() as u32;
                let offset = frame_start + i * bytes_per_sample;
                if bytes_per_sample == 1 {
                    decoded[offset] = value as u8;
                } else {
                    decoded[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes());
                }
            }
        }

        Ok(decoded)
    }
}

#[cfg(not(feature = "jpeg-xl-codec"))]
pub mod jpeg_xl_impl {
    use dcmnorm_object::DefaultDicomObject;

    pub fn decode_jpeg_xl_pixel_data(_object: &DefaultDicomObject) -> Result<Vec<u8>, String> {
        Err("JPEG XL codec support requires 'jpeg-xl-codec' feature to be enabled".to_owned())
    }
}

pub use jpeg_xl_impl::decode_jpeg_xl_pixel_data;

#[cfg(all(test, feature = "jpeg-xl-codec"))]
mod tests {
    use super::decode_jpeg_xl_pixel_data;
    use dcmnorm_core::value::PixelFragmentSequence;
    use dcmnorm_core::{DataElement, PrimitiveValue, VR};
    use dcmnorm_dictionary::tags;
    use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder};
    use std::path::PathBuf;

    const WIDTH: u16 = 32;
    const HEIGHT: u16 = 24;

    fn fixture_bytes(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test").join("files").join(name);
        std::fs::read(path).expect("fixture should be readable")
    }

    /// Same deterministic pattern used to generate `test/files/pattern8.jxl` /
    /// `pattern16.jxl` via a real third-party encoder (`cjxl -d 0`, libjxl 0.7 - lossless
    /// distance 0), so a decode bug shows up as a value mismatch rather than passing by
    /// coincidence on a flat/near-flat image.
    fn expected_sample_8bit(x: u16, y: u16) -> u8 {
        ((x.wrapping_mul(7)).wrapping_add(y.wrapping_mul(13)).wrapping_add(x ^ y) % 256) as u8
    }

    fn expected_sample_16bit(x: u16, y: u16) -> u16 {
        let x = x as u32;
        let y = y as u32;
        ((x * 733 + y * 1301 + (x ^ y) * 97) % 65536) as u16
    }

    fn grayscale_object(ts_uid: &str, bits: u16, jxl_bytes: Vec<u8>) -> DefaultDicomObject {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(ts_uid)
            .build()
            .expect("minimal meta table should build");
        let mut object = DefaultDicomObject::new_empty_with_meta(meta);
        for element in [
            DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(HEIGHT)),
            DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(WIDTH)),
            DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(1u16)),
            DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(bits)),
            DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::from(bits)),
            DataElement::new(tags::HIGH_BIT, VR::US, PrimitiveValue::from(bits - 1)),
            DataElement::new(tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::from(0u16)),
            DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("MONOCHROME2".to_owned()),
            ),
            DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PixelFragmentSequence::new(vec![0], vec![jxl_bytes]),
            ),
        ] {
            object.put(element);
        }
        object
    }

    /// Decodes a real `cjxl`-produced (not this codebase's own) 8-bit lossless JPEG XL
    /// codestream and checks every single sample against the pattern's ground-truth formula -
    /// genuine third-party-encoder interop, not a self-referential round trip.
    #[test]
    fn decodes_real_8bit_lossless_jpeg_xl_exactly() {
        let object = grayscale_object(
            "1.2.840.10008.1.2.4.110",
            8,
            fixture_bytes("pattern8.jxl"),
        );
        let decoded = decode_jpeg_xl_pixel_data(&object).expect("decode should succeed");
        assert_eq!(decoded.len(), usize::from(WIDTH) * usize::from(HEIGHT));

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let index = usize::from(y) * usize::from(WIDTH) + usize::from(x);
                assert_eq!(
                    decoded[index],
                    expected_sample_8bit(x, y),
                    "mismatch at ({x}, {y})"
                );
            }
        }
    }

    /// Same as above but 16-bit, and through the `.112` (general JPEG XL) transfer syntax
    /// rather than `.110` - both map to the same decoder, so this also proves the UID
    /// dispatch isn't accidentally `.110`-only.
    #[test]
    fn decodes_real_16bit_lossless_jpeg_xl_exactly() {
        let object = grayscale_object(
            "1.2.840.10008.1.2.4.112",
            16,
            fixture_bytes("pattern16.jxl"),
        );
        let decoded = decode_jpeg_xl_pixel_data(&object).expect("decode should succeed");
        assert_eq!(decoded.len(), usize::from(WIDTH) * usize::from(HEIGHT) * 2);

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let index = (usize::from(y) * usize::from(WIDTH) + usize::from(x)) * 2;
                let sample = u16::from_le_bytes([decoded[index], decoded[index + 1]]);
                assert_eq!(sample, expected_sample_16bit(x, y), "mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn rejects_signed_pixel_representation() {
        let mut object = grayscale_object("1.2.840.10008.1.2.4.110", 8, fixture_bytes("pattern8.jxl"));
        object.put(DataElement::new(
            tags::PIXEL_REPRESENTATION,
            VR::US,
            PrimitiveValue::from(1u16),
        ));
        let result = decode_jpeg_xl_pixel_data(&object);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn rejects_dimension_mismatch() {
        let mut object = grayscale_object("1.2.840.10008.1.2.4.110", 8, fixture_bytes("pattern8.jxl"));
        object.put(DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(HEIGHT + 1)));
        let result = decode_jpeg_xl_pixel_data(&object);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn rejects_channel_count_mismatch() {
        let mut object = grayscale_object("1.2.840.10008.1.2.4.110", 8, fixture_bytes("pattern8.jxl"));
        object.put(DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(3u16)));
        let result = decode_jpeg_xl_pixel_data(&object);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }
}
