//! Classic JPEG 2000 Part 1 (`.90`/`.91`) encoder, backed by pure-Rust `dcmnorm-jpeg2000` (`j2k`).

#[cfg(feature = "jpeg2000-openjpeg-encode")]
mod imp {
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        pixel_data: &[u8],
        rows: u32,
        cols: u32,
        samples_per_pixel: u32,
        bits_stored: u32,
        is_signed: bool,
        lossless: bool,
        lossy_compression_ratio: f32,
    ) -> Result<Vec<u8>, String> {
        dcmnorm_jpeg2000::classic::encode(
            pixel_data,
            rows,
            cols,
            samples_per_pixel,
            bits_stored,
            is_signed,
            lossless,
            lossy_compression_ratio,
        )
    }
}

#[cfg(not(feature = "jpeg2000-openjpeg-encode"))]
mod imp {
    pub fn encode(
        _pixel_data: &[u8],
        _rows: u32,
        _cols: u32,
        _samples_per_pixel: u32,
        _bits_stored: u32,
        _is_signed: bool,
        _lossless: bool,
        _lossy_compression_ratio: f32,
    ) -> Result<Vec<u8>, String> {
        Err("JPEG2000 encoding requires the 'jpeg2000-openjpeg-encode' feature".to_owned())
    }
}

pub(super) use imp::encode as encode_jpeg2000_with_openjpeg;

#[cfg(all(test, feature = "jpeg2000-openjpeg-encode"))]
mod tests {
    use super::encode_jpeg2000_with_openjpeg;
    use dcmnorm_core::value::PixelFragmentSequence;
    use dcmnorm_core::{DataElement, PrimitiveValue, VR};
    use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder};

    const WIDTH: u32 = 37;
    const HEIGHT: u32 = 29;

    /// Deliberately not a gradient or anything a lossy codec would reconstruct "close enough" -
    /// sharp discontinuities are exactly what would expose a genuinely lossy encode masquerading
    /// as lossless.
    fn pattern_8bit() -> Vec<u8> {
        (0..WIDTH * HEIGHT)
            .map(|i| {
                let (x, y) = (i % WIDTH, i / WIDTH);
                ((x.wrapping_mul(37) ^ y.wrapping_mul(101)) % 256) as u8
            })
            .collect()
    }

    fn pattern_16bit() -> Vec<u8> {
        (0..WIDTH * HEIGHT)
            .flat_map(|i| {
                let (x, y) = (i % WIDTH, i / WIDTH);
                let v = ((x.wrapping_mul(733) ^ y.wrapping_mul(1301)) % 4096) as u16;
                v.to_le_bytes()
            })
            .collect()
    }

    fn decode_via_full_pipeline(ts_uid: &str, codestream: Vec<u8>, bits_stored: u16) -> Vec<u8> {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(ts_uid)
            .build()
            .unwrap();
        let mut object = DefaultDicomObject::new_empty_with_meta(meta);
        for element in [
            DataElement::new(
                dcmnorm_dictionary::tags::ROWS,
                VR::US,
                PrimitiveValue::from(HEIGHT as u16),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::COLUMNS,
                VR::US,
                PrimitiveValue::from(WIDTH as u16),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::SAMPLES_PER_PIXEL,
                VR::US,
                PrimitiveValue::from(1u16),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::BITS_ALLOCATED,
                VR::US,
                PrimitiveValue::from(if bits_stored > 8 { 16u16 } else { 8u16 }),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::BITS_STORED,
                VR::US,
                PrimitiveValue::from(bits_stored),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::HIGH_BIT,
                VR::US,
                PrimitiveValue::from(bits_stored - 1),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::PIXEL_REPRESENTATION,
                VR::US,
                PrimitiveValue::from(0u16),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("MONOCHROME2".to_owned()),
            ),
            DataElement::new(
                dcmnorm_dictionary::tags::PIXEL_DATA,
                VR::OB,
                PixelFragmentSequence::new(vec![0], vec![codestream]),
            ),
        ] {
            object.put(element);
        }

        let transcoded = super::super::transcode_dcmnorm_object(
            &object,
            dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN,
        )
        .expect("decode of our own encoded codestream should succeed");
        transcoded
            .element(dcmnorm_dictionary::tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .to_vec()
    }

    /// Encodes with our raw-FFI OpenJPEG encoder and decodes with the *already-verified*
    /// OpenJPEG-backed `Jpeg2000Adapter` (proven correct in earlier work this session) - an
    /// exact byte match proves the encoder is genuinely lossless, not merely "OpenJPEG didn't
    /// error".
    #[test]
    fn lossless_8bit_encode_round_trips_exactly_via_full_decode_pipeline() {
        let original = pattern_8bit();
        let codestream =
            encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 8, false, true, 1.0)
                .expect("lossless encode should succeed");
        let decoded = decode_via_full_pipeline("1.2.840.10008.1.2.4.90", codestream, 8);
        assert_eq!(decoded, original);
    }

    #[test]
    fn lossless_16bit_encode_round_trips_exactly_via_full_decode_pipeline() {
        let original = pattern_16bit();
        let codestream =
            encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 12, false, true, 1.0)
                .expect("lossless encode should succeed");
        let decoded = decode_via_full_pipeline("1.2.840.10008.1.2.4.90", codestream, 12);
        assert_eq!(decoded, original);
    }

    /// Lossy encoding must actually compress (smaller than the lossless codestream for the same
    /// image) and must NOT be byte-identical after round-tripping - if it were, the "lossy" path
    /// would secretly be lossless, which would be its own (differently wrong) bug.
    #[test]
    fn lossy_encode_is_smaller_and_not_byte_identical() {
        let original = pattern_8bit();
        let lossless =
            encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 8, false, true, 1.0)
                .expect("lossless encode should succeed");
        let lossy =
            encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 8, false, false, 8.0)
                .expect("lossy encode should succeed");

        assert!(
            lossy.len() < lossless.len(),
            "lossy ({} bytes) should be smaller than lossless ({} bytes)",
            lossy.len(),
            lossless.len()
        );

        let decoded = decode_via_full_pipeline("1.2.840.10008.1.2.4.91", lossy, 8);
        assert_eq!(decoded.len(), original.len());
        assert_ne!(
            decoded, original,
            "lossy output should not be byte-identical to the source"
        );
    }

    #[test]
    fn rejects_pixel_data_length_mismatch() {
        let result =
            encode_jpeg2000_with_openjpeg(&[0u8; 4], HEIGHT, WIDTH, 1, 8, false, true, 1.0);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }
}
