//! Raw `openjpeg-sys` FFI encoder for classic JPEG 2000 (`.90`/`.91`).
//!
//! The `jpeg2k` crate this codebase already depends on for JPEG2000 *decode* only ever
//! constructs an `Image` by decoding existing codestream bytes - it has no way to build one
//! from raw pixel samples for encoding (its `sys` module wrapping `openjpeg-sys` is also
//! private, so it can't be reused directly either). This module talks to `openjpeg-sys`
//! directly instead, the same way `mpeg.rs` talks to `ffmpeg-next`'s raw `ffi` module where the
//! safe wrapper crate doesn't cover what's needed.

#[cfg(feature = "jpeg2000-openjpeg-encode")]
mod imp {
    use openjpeg_sys as sys;
    use std::os::raw::c_void;

    /// A growable in-memory buffer that OpenJPEG can write to, seek within, and skip ahead in -
    /// J2K codestream writing isn't purely sequential (e.g. length fields get patched after the
    /// fact), so all four stream callbacks need to behave like a real seekable byte sink, not
    /// just an append-only one.
    struct MemoryTarget {
        buffer: Vec<u8>,
        pos: usize,
    }

    unsafe extern "C" fn write_fn(
        buffer: *mut c_void,
        num_bytes: sys::OPJ_SIZE_T,
        user_data: *mut c_void,
    ) -> sys::OPJ_SIZE_T {
        let target = unsafe { &mut *(user_data as *mut MemoryTarget) };
        let n = num_bytes as usize;
        let src = unsafe { std::slice::from_raw_parts(buffer as *const u8, n) };
        let end = target.pos + n;
        if end > target.buffer.len() {
            target.buffer.resize(end, 0);
        }
        target.buffer[target.pos..end].copy_from_slice(src);
        target.pos = end;
        num_bytes
    }

    unsafe extern "C" fn skip_fn(num_bytes: sys::OPJ_OFF_T, user_data: *mut c_void) -> sys::OPJ_OFF_T {
        let target = unsafe { &mut *(user_data as *mut MemoryTarget) };
        if num_bytes < 0 {
            return -1;
        }
        let end = target.pos + num_bytes as usize;
        if end > target.buffer.len() {
            target.buffer.resize(end, 0);
        }
        target.pos = end;
        num_bytes
    }

    unsafe extern "C" fn seek_fn(num_bytes: sys::OPJ_OFF_T, user_data: *mut c_void) -> sys::OPJ_BOOL {
        let target = unsafe { &mut *(user_data as *mut MemoryTarget) };
        if num_bytes < 0 {
            return 0;
        }
        let pos = num_bytes as usize;
        if pos > target.buffer.len() {
            target.buffer.resize(pos, 0);
        }
        target.pos = pos;
        1
    }

    unsafe extern "C" fn error_fn(msg: *const std::os::raw::c_char, client_data: *mut c_void) {
        let msg = unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy();
        let errors = unsafe { &mut *(client_data as *mut Vec<String>) };
        errors.push(msg.trim_end().to_owned());
    }

    struct Codec(*mut sys::opj_codec_t);
    impl Drop for Codec {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { sys::opj_destroy_codec(self.0) };
            }
        }
    }

    struct Stream(*mut sys::opj_stream_t);
    impl Drop for Stream {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { sys::opj_stream_destroy(self.0) };
            }
        }
    }

    struct Image(*mut sys::opj_image_t);
    impl Drop for Image {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { sys::opj_image_destroy(self.0) };
            }
        }
    }

    /// Encodes one frame's worth of raw, native-byte-order, interleaved pixel samples to a
    /// classic JPEG 2000 codestream (raw `.j2k`, no JP2 box wrapper - matching what DICOM
    /// PixelData fragments for `.90`/`.91` expect).
    ///
    /// `lossless=true` uses OpenJPEG's defaults, which - per `opj_j2k_setup_encoder`'s own
    /// normalization of a zeroed `tcp_numlayers`/`tcp_rates` - already means a single,
    /// rate-unconstrained layer with the reversible 5/3 wavelet (`irreversible=0`): genuinely
    /// lossless, not merely "high quality". `lossless=false` switches to the irreversible 9/7
    /// wavelet and targets `lossy_compression_ratio` (e.g. `10.0` for 10:1), OpenJPEG's own
    /// convention for `tcp_rates` (original-size-to-compressed-size ratio, not a byte count).
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
        if samples_per_pixel != 1 && samples_per_pixel != 3 {
            return Err(format!(
                "JPEG2000 encoding only supports 1 or 3 samples per pixel, got {samples_per_pixel}"
            ));
        }
        let bytes_per_sample = if bits_stored > 8 { 2usize } else { 1usize };
        let expected_len =
            rows as usize * cols as usize * samples_per_pixel as usize * bytes_per_sample;
        if pixel_data.len() != expected_len {
            return Err(format!(
                "pixel data length {} does not match expected {expected_len} for \
                 {rows}x{cols}x{samples_per_pixel} at {bytes_per_sample} byte(s)/sample",
                pixel_data.len()
            ));
        }

        let comp_params: Vec<sys::opj_image_cmptparm_t> = (0..samples_per_pixel)
            .map(|_| sys::opj_image_cmptparm_t {
                dx: 1,
                dy: 1,
                w: cols,
                h: rows,
                x0: 0,
                y0: 0,
                prec: bits_stored,
                bpp: bits_stored,
                sgnd: is_signed as sys::OPJ_UINT32,
            })
            .collect();

        let color_space = if samples_per_pixel == 1 {
            sys::OPJ_COLOR_SPACE::OPJ_CLRSPC_GRAY
        } else {
            sys::OPJ_COLOR_SPACE::OPJ_CLRSPC_SRGB
        };

        let image = unsafe {
            sys::opj_image_create(samples_per_pixel, comp_params.as_ptr() as *mut _, color_space)
        };
        if image.is_null() {
            return Err("opj_image_create failed".to_owned());
        }
        let image = Image(image);

        unsafe {
            (*image.0).x0 = 0;
            (*image.0).y0 = 0;
            (*image.0).x1 = cols;
            (*image.0).y1 = rows;

            let comps = std::slice::from_raw_parts_mut((*image.0).comps, samples_per_pixel as usize);
            for (c, comp) in comps.iter_mut().enumerate() {
                let data = std::slice::from_raw_parts_mut(comp.data, (rows * cols) as usize);
                for (i, sample) in data.iter_mut().enumerate() {
                    let offset = (i * samples_per_pixel as usize + c) * bytes_per_sample;
                    *sample = if bytes_per_sample == 1 {
                        if is_signed {
                            pixel_data[offset] as i8 as sys::OPJ_INT32
                        } else {
                            pixel_data[offset] as sys::OPJ_INT32
                        }
                    } else {
                        let raw = u16::from_le_bytes([pixel_data[offset], pixel_data[offset + 1]]);
                        if is_signed {
                            raw as i16 as sys::OPJ_INT32
                        } else {
                            raw as sys::OPJ_INT32
                        }
                    };
                }
            }
        }

        let codec = unsafe { sys::opj_create_compress(sys::OPJ_CODEC_FORMAT::OPJ_CODEC_J2K) };
        if codec.is_null() {
            return Err("opj_create_compress failed".to_owned());
        }
        let codec = Codec(codec);
        let mut errors: Vec<String> = Vec::new();
        unsafe {
            sys::opj_set_error_handler(
                codec.0,
                Some(error_fn),
                &mut errors as *mut Vec<String> as *mut c_void,
            );
        }

        let mut params: sys::opj_cparameters_t = unsafe { std::mem::zeroed() };
        unsafe { sys::opj_set_default_encoder_parameters(&mut params) };
        // The default of 6 wavelet decomposition levels needs the smallest image dimension to
        // be at least 2^5=32 (with the default single-tile layout); DICOM images can be smaller
        // than that (icon/thumbnail frames, tiny secondary captures), so scale it down rather
        // than let OpenJPEG reject an otherwise perfectly encodable small image.
        let smallest_dim = rows.min(cols).max(1);
        params.numresolution = (32 - smallest_dim.leading_zeros()).clamp(1, 6) as i32;
        if !lossless {
            params.irreversible = 1;
            params.cp_disto_alloc = 1;
            params.tcp_numlayers = 1;
            params.tcp_rates[0] = lossy_compression_ratio;
        }

        let setup_ok = unsafe { sys::opj_setup_encoder(codec.0, &mut params, image.0) };
        if setup_ok == 0 {
            return Err(format!("opj_setup_encoder failed: {}", errors.join("; ")));
        }

        let stream = unsafe { sys::opj_stream_create(1024 * 1024, 0) };
        if stream.is_null() {
            return Err("opj_stream_create failed".to_owned());
        }
        let stream = Stream(stream);

        let mut target = Box::new(MemoryTarget { buffer: Vec::new(), pos: 0 });
        unsafe {
            sys::opj_stream_set_write_function(stream.0, Some(write_fn));
            sys::opj_stream_set_skip_function(stream.0, Some(skip_fn));
            sys::opj_stream_set_seek_function(stream.0, Some(seek_fn));
            sys::opj_stream_set_user_data(
                stream.0,
                target.as_mut() as *mut MemoryTarget as *mut c_void,
                None,
            );
        }

        let ok = unsafe {
            sys::opj_start_compress(codec.0, image.0, stream.0) != 0
                && sys::opj_encode(codec.0, stream.0) != 0
                && sys::opj_end_compress(codec.0, stream.0) != 0
        };
        if !ok {
            return Err(format!("OpenJPEG JPEG2000 encode failed: {}", errors.join("; ")));
        }

        Ok(target.buffer)
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
        Err("JPEG2000 OpenJPEG encoding requires the 'jpeg2000-openjpeg-encode' feature".to_owned())
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
        let meta = FileMetaTableBuilder::new().transfer_syntax(ts_uid).build().unwrap();
        let mut object = DefaultDicomObject::new_empty_with_meta(meta);
        for element in [
            DataElement::new(dcmnorm_dictionary::tags::ROWS, VR::US, PrimitiveValue::from(HEIGHT as u16)),
            DataElement::new(dcmnorm_dictionary::tags::COLUMNS, VR::US, PrimitiveValue::from(WIDTH as u16)),
            DataElement::new(dcmnorm_dictionary::tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(1u16)),
            DataElement::new(dcmnorm_dictionary::tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(if bits_stored > 8 { 16u16 } else { 8u16 })),
            DataElement::new(dcmnorm_dictionary::tags::BITS_STORED, VR::US, PrimitiveValue::from(bits_stored)),
            DataElement::new(dcmnorm_dictionary::tags::HIGH_BIT, VR::US, PrimitiveValue::from(bits_stored - 1)),
            DataElement::new(dcmnorm_dictionary::tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::from(0u16)),
            DataElement::new(dcmnorm_dictionary::tags::PHOTOMETRIC_INTERPRETATION, VR::CS, PrimitiveValue::from("MONOCHROME2".to_owned())),
            DataElement::new(dcmnorm_dictionary::tags::PIXEL_DATA, VR::OB, PixelFragmentSequence::new(vec![0], vec![codestream])),
        ] {
            object.put(element);
        }

        let transcoded = super::super::transcode_dcmnorm_object(&object, dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN)
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
        let codestream = encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 8, false, true, 1.0)
            .expect("lossless encode should succeed");
        let decoded = decode_via_full_pipeline("1.2.840.10008.1.2.4.90", codestream, 8);
        assert_eq!(decoded, original);
    }

    #[test]
    fn lossless_16bit_encode_round_trips_exactly_via_full_decode_pipeline() {
        let original = pattern_16bit();
        let codestream = encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 12, false, true, 1.0)
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
        let lossless = encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 8, false, true, 1.0)
            .expect("lossless encode should succeed");
        let lossy = encode_jpeg2000_with_openjpeg(&original, HEIGHT, WIDTH, 1, 8, false, false, 8.0)
            .expect("lossy encode should succeed");

        assert!(
            lossy.len() < lossless.len(),
            "lossy ({} bytes) should be smaller than lossless ({} bytes)",
            lossy.len(),
            lossless.len()
        );

        let decoded = decode_via_full_pipeline("1.2.840.10008.1.2.4.91", lossy, 8);
        assert_eq!(decoded.len(), original.len());
        assert_ne!(decoded, original, "lossy output should not be byte-identical to the source");
    }

    #[test]
    fn rejects_pixel_data_length_mismatch() {
        let result = encode_jpeg2000_with_openjpeg(&[0u8; 4], HEIGHT, WIDTH, 1, 8, false, true, 1.0);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }
}
