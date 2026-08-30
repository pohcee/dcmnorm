#[cfg(feature = "jpeg-ls-codec")]
pub mod jpeg_ls_impl {
    use dcmnorm_dictionary::tags;
    use dcmnorm_object::DefaultDicomObject;

    /// Near-lossless quantization step used for `.81` (JPEG-LS Lossy) encoding. `near=0` would be
    /// lossless (that's what `.80` uses instead); a small value keeps visible error bounded to a
    /// couple of stored-value counts per sample, matching the modest quality loss real-world
    /// JPEG-LS Lossy presets (e.g. DCMTK's `+E1`/`+ee`) typically target rather than picking an
    /// arbitrary large value that would look closer to a completely different, more lossy codec.
    pub(crate) const NEAR_LOSSLESS_STEP: i32 = 3;

    /// Decode JPEG-LS encoded pixel data - every frame's worth, concatenated into one flat
    /// native-byte-order buffer matching what `replace_with_native_pixel_data` expects.
    ///
    /// One fragment per frame (not the more flexible offset-table-based fragmentation JPEG2000
    /// allows) is the standard DICOM encapsulation convention for JPEG-LS (PS3.5 A.4), so each
    /// fragment is decoded independently - concatenating the still-*encoded* fragments before a
    /// single decode call would feed multiple independent codestreams into one decode as if they
    /// were one, silently corrupting every frame after the first on real multi-frame files.
    pub fn decode_jpeg_ls_pixel_data(object: &DefaultDicomObject) -> Result<Vec<u8>, String> {
        let fragments = object
            .element(tags::PIXEL_DATA)
            .map_err(|e| format!("missing PixelData: {e}"))?
            .fragments()
            .ok_or_else(|| "expected encapsulated JPEG-LS pixel data".to_owned())?;

        if fragments.is_empty() {
            return Err("no JPEG-LS data to decode".to_owned());
        }

        // Get image dimensions to validate against each codestream's own header before decoding
        // it - a mismatch here means either a corrupt fragment or the wrong PixelData
        // attributes, and either way is worth a clear error instead of the confusing shape
        // mismatch that would otherwise surface downstream.
        let rows = object
            .get(tags::ROWS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Rows attribute".to_owned())? as u32;

        let cols = object
            .get(tags::COLUMNS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Columns attribute".to_owned())? as u32;

        // charls decodes samples into native (little-endian) byte order, matching what every
        // other codec in this codebase already hands to `replace_with_native_pixel_data`.
        let mut decoded = Vec::new();
        for fragment in fragments {
            if fragment.is_empty() {
                return Err("empty JPEG-LS fragment".to_owned());
            }

            let frame_info = charls::CharLS::default()
                .get_frame_info(fragment)
                .map_err(|e| format!("failed to read JPEG-LS header: {e}"))?;
            if frame_info.width != cols || frame_info.height != rows {
                return Err(format!(
                    "JPEG-LS codestream dimensions ({}x{}) do not match DICOM Rows/Columns ({rows}x{cols})",
                    frame_info.width, frame_info.height
                ));
            }

            let frame_bytes = charls::CharLS::default()
                .decode(fragment)
                .map_err(|e| format!("JPEG-LS decode failed: {e}"))?;
            decoded.extend_from_slice(&frame_bytes);
        }

        Ok(decoded)
    }

    /// Encode raw native pixel data to JPEG-LS, one fragment per frame - the mirror image of
    /// `decode_jpeg_ls_pixel_data` above, and matching the "one fragment per frame" convention
    /// its own doc comment explains.
    pub fn encode_jpeg_ls_pixel_data(
        object: &DefaultDicomObject,
        lossless: bool,
    ) -> Result<Vec<Vec<u8>>, String> {
        let pixel_data = object
            .element(tags::PIXEL_DATA)
            .map_err(|e| format!("missing PixelData: {e}"))?
            .to_bytes()
            .map_err(|e| format!("failed to access pixel data: {e}"))?
            .to_vec();

        let rows = object
            .get(tags::ROWS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Rows attribute".to_owned())?;
        let cols = object
            .get(tags::COLUMNS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Columns attribute".to_owned())?;
        let samples_per_pixel =
            object.get(tags::SAMPLES_PER_PIXEL).and_then(|e| e.uint16().ok()).unwrap_or(1);
        let bits_allocated = object
            .get(tags::BITS_ALLOCATED)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing BitsAllocated attribute".to_owned())?;
        // JPEG-LS's own sample precision, not the (possibly wider) storage container - e.g.
        // 12-bit CT data allocated in 16-bit words encodes with bits_per_sample: 12. Lossless
        // round-tripping only needs this to exactly match what the encoder used, not to reflect
        // anything about signedness: JPEG-LS's predictive coding operates on the raw sample
        // bit pattern either way, and reproduces those exact bits back out on decode.
        let bits_stored = object
            .get(tags::BITS_STORED)
            .and_then(|e| e.uint16().ok())
            .unwrap_or(bits_allocated);
        let number_of_frames = object
            .get(tags::NUMBER_OF_FRAMES)
            .and_then(|e| e.to_str().ok())
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let planar_configuration =
            object.get(tags::PLANAR_CONFIGURATION).and_then(|e| e.uint16().ok()).unwrap_or(0);

        if bits_allocated == 0 || bits_allocated > 16 {
            return Err(format!("unsupported BitsAllocated for JPEG-LS encoding: {bits_allocated}"));
        }
        let bytes_per_sample = if bits_allocated <= 8 { 1usize } else { 2usize };
        let frame_len = usize::from(rows) * usize::from(cols) * usize::from(samples_per_pixel) * bytes_per_sample;
        let expected_len = frame_len * number_of_frames;
        if pixel_data.len() < expected_len {
            return Err(format!(
                "PixelData too short for {number_of_frames} frame(s) of {rows}x{cols}x{samples_per_pixel} \
                 at {bits_allocated} bits allocated: expected at least {expected_len} bytes, got {}",
                pixel_data.len()
            ));
        }

        let frame_info = charls::FrameInfo {
            width: u32::from(cols),
            height: u32::from(rows),
            bits_per_sample: i32::from(bits_stored),
            component_count: i32::from(samples_per_pixel),
        };
        let near = if lossless { 0 } else { NEAR_LOSSLESS_STEP };

        let mut fragments = Vec::with_capacity(number_of_frames);
        for frame_index in 0..number_of_frames {
            let start = frame_index * frame_len;
            let frame_bytes = &pixel_data[start..start + frame_len];

            let mut encoder = charls::CharLS::default();
            if samples_per_pixel > 1 {
                let interleave_mode = if planar_configuration == 0 {
                    charls::InterleaveMode::Sample
                } else {
                    charls::InterleaveMode::None
                };
                encoder
                    .set_interleave_mode(interleave_mode)
                    .map_err(|e| format!("failed to set JPEG-LS interleave mode: {e}"))?;
            }

            let encoded = encoder
                .encode(frame_info.clone(), near, frame_bytes)
                .map_err(|e| format!("JPEG-LS encode failed for frame {frame_index}: {e}"))?;
            fragments.push(encoded);
        }

        Ok(fragments)
    }
}

#[cfg(not(feature = "jpeg-ls-codec"))]
pub mod jpeg_ls_impl {
    use dcmnorm_object::DefaultDicomObject;

    pub fn decode_jpeg_ls_pixel_data(_object: &DefaultDicomObject) -> Result<Vec<u8>, String> {
        Err("JPEG-LS codec support requires 'jpeg-ls-codec' feature to be enabled".to_owned())
    }

    pub fn encode_jpeg_ls_pixel_data(
        _object: &DefaultDicomObject,
        _lossless: bool,
    ) -> Result<Vec<Vec<u8>>, String> {
        Err("JPEG-LS codec support requires 'jpeg-ls-codec' feature to be enabled".to_owned())
    }
}

pub use jpeg_ls_impl::{decode_jpeg_ls_pixel_data, encode_jpeg_ls_pixel_data};

#[cfg(test)]
mod tests {
    use super::{decode_jpeg_ls_pixel_data, encode_jpeg_ls_pixel_data};
    use crate::dicom_io::read_dicom_file;
    use dcmnorm_object::DefaultDicomObject;
    use std::path::PathBuf;

    fn fixture(name: &str) -> DefaultDicomObject {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test").join("files").join(name);
        read_dicom_file(path).expect("fixture should be readable")
    }

    /// Encodes `mr_small.dcm`'s own native (uncompressed) pixel data to JPEG-LS Lossless and
    /// decodes it straight back, entirely in memory (no transcode/file round trip) - the
    /// tightest possible test of `encode_jpeg_ls_pixel_data`/`decode_jpeg_ls_pixel_data`
    /// agreeing with each other, independent of `mr_jpegls_lossless.dcm`'s own third-party
    /// encoder.
    #[test]
    fn lossless_encode_then_decode_round_trips_exactly() {
        let object = fixture("mr_small.dcm");
        let original_bytes =
            object.element(dcmnorm_dictionary::tags::PIXEL_DATA).unwrap().to_bytes().unwrap().to_vec();

        let fragments = encode_jpeg_ls_pixel_data(&object, true).expect("lossless encode should succeed");
        assert_eq!(fragments.len(), 1, "single-frame source should produce exactly one fragment");

        // Rebuild an object carrying the encoded fragment as encapsulated PixelData, the same
        // shape decode_jpeg_ls_pixel_data expects from a real file.
        let mut encoded_object = object.clone();
        encoded_object.put(dcmnorm_core::DataElement::new(
            dcmnorm_dictionary::tags::PIXEL_DATA,
            dcmnorm_core::VR::OB,
            dcmnorm_core::value::PixelFragmentSequence::new(vec![0], fragments),
        ));

        let decoded = decode_jpeg_ls_pixel_data(&encoded_object).expect("decode should succeed");
        assert_eq!(decoded, original_bytes);
    }

    /// Near-lossless (.81) must bound every sample's error to at most `NEAR_LOSSLESS_STEP` -
    /// that per-sample error bound is JPEG-LS's whole point (as opposed to a general-purpose
    /// lossy codec with no per-sample guarantee), so this is checking the actual codec contract,
    /// not just "produces plausible-looking output".
    #[test]
    fn near_lossless_encode_bounds_error_to_the_configured_step() {
        let object = fixture("mr_small.dcm");
        let original_bytes =
            object.element(dcmnorm_dictionary::tags::PIXEL_DATA).unwrap().to_bytes().unwrap().to_vec();

        let fragments =
            encode_jpeg_ls_pixel_data(&object, false).expect("near-lossless encode should succeed");
        let mut encoded_object = object.clone();
        encoded_object.put(dcmnorm_core::DataElement::new(
            dcmnorm_dictionary::tags::PIXEL_DATA,
            dcmnorm_core::VR::OB,
            dcmnorm_core::value::PixelFragmentSequence::new(vec![0], fragments),
        ));
        let decoded = decode_jpeg_ls_pixel_data(&encoded_object).expect("decode should succeed");

        assert_eq!(decoded.len(), original_bytes.len());
        assert_ne!(decoded, original_bytes, "near-lossless should not be byte-identical to lossless");
        for (original, roundtripped) in original_bytes.chunks_exact(2).zip(decoded.chunks_exact(2)) {
            let a = u16::from_le_bytes([original[0], original[1]]);
            let b = u16::from_le_bytes([roundtripped[0], roundtripped[1]]);
            assert!(
                a.abs_diff(b) <= super::jpeg_ls_impl::NEAR_LOSSLESS_STEP as u16,
                "sample error {} exceeds the configured near-lossless step",
                a.abs_diff(b)
            );
        }
    }

    /// Round-trips a real *third-party* JPEG-LS Lossless file (not one this codebase encoded
    /// itself) through re-encode: decode it, re-encode the result, decode again, and confirm the
    /// second decode matches the first exactly - proof the encoder and decoder agree on a file
    /// this codebase didn't produce, not just on each other's own conventions.
    #[test]
    fn reencoding_a_real_third_party_jpeg_ls_file_round_trips_exactly() {
        let source = fixture("mr_jpegls_lossless.dcm");
        let first_decode = decode_jpeg_ls_pixel_data(&source).expect("initial decode should succeed");

        let mut native_object = source.clone();
        native_object.put(dcmnorm_core::DataElement::new(
            dcmnorm_dictionary::tags::PIXEL_DATA,
            dcmnorm_core::VR::OW,
            dcmnorm_core::PrimitiveValue::U16(
                first_decode
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect(),
            ),
        ));

        let fragments =
            encode_jpeg_ls_pixel_data(&native_object, true).expect("re-encode should succeed");
        let mut reencoded_object = native_object.clone();
        reencoded_object.put(dcmnorm_core::DataElement::new(
            dcmnorm_dictionary::tags::PIXEL_DATA,
            dcmnorm_core::VR::OB,
            dcmnorm_core::value::PixelFragmentSequence::new(vec![0], fragments),
        ));

        let second_decode =
            decode_jpeg_ls_pixel_data(&reencoded_object).expect("re-decode should succeed");
        assert_eq!(first_decode, second_decode);
    }

    #[test]
    fn encode_rejects_pixel_data_too_short_for_the_declared_shape() {
        let mut object = fixture("mr_small.dcm");
        object.put(dcmnorm_core::DataElement::new(
            dcmnorm_dictionary::tags::PIXEL_DATA,
            dcmnorm_core::VR::OW,
            dcmnorm_core::PrimitiveValue::U16(vec![0u16; 4].into()),
        ));
        let result = encode_jpeg_ls_pixel_data(&object, true);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }
}
