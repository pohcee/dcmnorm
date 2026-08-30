#[cfg(feature = "kakadu-ffi")]
use std::ffi::CStr;
#[cfg(feature = "kakadu-ffi")]
use std::os::raw::{c_char, c_int};

pub fn kakadu_ffi_enabled() -> bool {
    cfg!(feature = "kakadu-ffi")
}

#[cfg(feature = "kakadu-ffi")]
unsafe extern "C" {
    fn dcmnorm_kakadu_decode(
        codestream: *const u8,
        codestream_len: usize,
        rows: c_int,
        cols: c_int,
        samples_per_pixel: c_int,
        bits_stored: c_int,
        is_signed: c_int,
        out_data: *mut *mut u8,
        out_len: *mut usize,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn dcmnorm_kakadu_free_buffer(buffer: *mut u8, len: usize);
    fn dcmnorm_kakadu_free_error(error_message: *mut c_char);
    fn dcmnorm_kakadu_supports_htj2k() -> c_int;
    fn dcmnorm_kakadu_encode(
        pixel_data: *const u8,
        pixel_data_len: usize,
        rows: c_int,
        cols: c_int,
        samples_per_pixel: c_int,
        bits_stored: c_int,
        is_signed: c_int,
        lossless: c_int,
        lossy_compression_ratio: f64,
        out_data: *mut *mut u8,
        out_len: *mut usize,
        error_message: *mut *mut c_char,
    ) -> c_int;
}

/// Whether the linked Kakadu SDK is new enough (v8.0+) to have any HTJ2K (Part-15) support at
/// all. Versions before that don't fail cleanly on an HT-coded codestream - `kdu_codestream`
/// can hang indefinitely trying to interpret Part-15-only marker signaling it doesn't
/// understand - so this must be checked before ever attempting a Kakadu decode of `.201`/`.202`/
/// `.203`, never inferred from how a decode attempt turns out.
#[cfg(feature = "kakadu-ffi")]
pub(super) fn kakadu_supports_htj2k() -> bool {
    unsafe { dcmnorm_kakadu_supports_htj2k() != 0 }
}

#[cfg(not(feature = "kakadu-ffi"))]
pub(super) fn kakadu_supports_htj2k() -> bool {
    false
}

#[cfg(feature = "kakadu-ffi")]
fn take_error(error_message: *mut c_char) -> String {
    if error_message.is_null() {
        return "Kakadu bridge returned an unknown error".to_owned();
    }

    let message = unsafe { CStr::from_ptr(error_message) }
        .to_string_lossy()
        .to_string();
    unsafe { dcmnorm_kakadu_free_error(error_message) };
    message
}

#[cfg(feature = "kakadu-ffi")]
fn take_buffer(buffer: *mut u8, len: usize) -> Vec<u8> {
    if buffer.is_null() || len == 0 {
        return Vec::new();
    }

    let bytes = unsafe { std::slice::from_raw_parts(buffer, len) }.to_vec();
    unsafe { dcmnorm_kakadu_free_buffer(buffer, len) };
    bytes
}

#[cfg(feature = "kakadu-ffi")]
pub(super) fn decode_jpeg2000(
    codestream: &[u8],
    rows: usize,
    cols: usize,
    samples_per_pixel: usize,
    bits_stored: u16,
    is_signed: bool,
) -> Result<Vec<u8>, String> {
    let mut out_data = std::ptr::null_mut();
    let mut out_len = 0usize;
    let mut error_message = std::ptr::null_mut();

    let status = unsafe {
        dcmnorm_kakadu_decode(
            codestream.as_ptr(),
            codestream.len(),
            rows as c_int,
            cols as c_int,
            samples_per_pixel as c_int,
            bits_stored as c_int,
            is_signed as c_int,
            &mut out_data,
            &mut out_len,
            &mut error_message,
        )
    };

    if status == 0 {
        Ok(take_buffer(out_data, out_len))
    } else {
        Err(take_error(error_message))
    }
}

#[cfg(not(feature = "kakadu-ffi"))]
pub(super) fn decode_jpeg2000(
    _codestream: &[u8],
    _rows: usize,
    _cols: usize,
    _samples_per_pixel: usize,
    _bits_stored: u16,
    _is_signed: bool,
) -> Result<Vec<u8>, String> {
    Err("Kakadu FFI is not enabled in this build".to_owned())
}

/// **NOT VERIFIED CORRECT - do not call this from the encode dispatch.** On the currently
/// licensed Kakadu v7.8 SDK, this produces codestreams that decode to the wrong pixel values
/// (see `tests::known_broken_lossless_round_trip` for how this was isolated to Kakadu's own
/// `kdu_stripe_compressor`, independent of this wrapper). Left in place because the bridge
/// plumbing (build.rs detection, FFI signatures, `memory_target`) is otherwise complete and
/// correct, and because a newer Kakadu SDK or an upstream fix may resolve it - but nothing
/// should route real encode requests through this until a passing (non-`#[ignore]`d) round-trip
/// test exists.
#[cfg(feature = "kakadu-ffi")]
#[allow(clippy::too_many_arguments, dead_code)]
pub(super) fn encode_jpeg2000(
    pixel_data: &[u8],
    rows: usize,
    cols: usize,
    samples_per_pixel: usize,
    bits_stored: u16,
    is_signed: bool,
    lossless: bool,
    lossy_compression_ratio: f64,
) -> Result<Vec<u8>, String> {
    let mut out_data = std::ptr::null_mut();
    let mut out_len = 0usize;
    let mut error_message = std::ptr::null_mut();

    let status = unsafe {
        dcmnorm_kakadu_encode(
            pixel_data.as_ptr(),
            pixel_data.len(),
            rows as c_int,
            cols as c_int,
            samples_per_pixel as c_int,
            bits_stored as c_int,
            is_signed as c_int,
            lossless as c_int,
            lossy_compression_ratio,
            &mut out_data,
            &mut out_len,
            &mut error_message,
        )
    };

    if status == 0 {
        Ok(take_buffer(out_data, out_len))
    } else {
        Err(take_error(error_message))
    }
}

#[cfg(not(feature = "kakadu-ffi"))]
#[allow(clippy::too_many_arguments, dead_code)]
pub(super) fn encode_jpeg2000(
    _pixel_data: &[u8],
    _rows: usize,
    _cols: usize,
    _samples_per_pixel: usize,
    _bits_stored: u16,
    _is_signed: bool,
    _lossless: bool,
    _lossy_compression_ratio: f64,
) -> Result<Vec<u8>, String> {
    Err("Kakadu FFI is not enabled in this build".to_owned())
}

#[cfg(all(test, feature = "kakadu-ffi"))]
mod tests {
    use super::{decode_jpeg2000, encode_jpeg2000};

    const WIDTH: usize = 64;
    const HEIGHT: usize = 32;

    #[test]
    fn rejects_pixel_data_length_mismatch() {
        let result = encode_jpeg2000(&[0u8; 4], HEIGHT, WIDTH, 1, 8, false, true, 1.0);
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    /// **KNOWN BROKEN, do not wire this into the encode dispatch.** `encode_jpeg2000` (via
    /// `kdu_stripe_compressor`) produces a codestream that reconstructs to the wrong pixel
    /// values on the currently-licensed Kakadu v7.8 SDK - verified with a horizontal gradient
    /// (`decoded[x] != x`, but resembles `~x` shifted by one sample) using:
    ///   - our own bridge, decoded by our own `decode_jpeg2000` (Kakadu decode - proven correct
    ///     elsewhere against real third-party files), AND
    ///   - an independent, minimal reproduction using `kdu_simple_file_target` copied verbatim
    ///     from Kakadu's own `simple_example_c.cpp`, decoded by OpenJPEG (`opj_decompress`) -
    ///     ruling out both this wrapper and Kakadu's own decompressor as the cause.
    /// Reproduced against two independently-built copies of `libkdu_a78R.so`/`libkdu_v78R.so`
    /// (ruling out a stale/mismatched local library), with headers verified byte-identical to
    /// the full SDK source tree (ruling out a header/library ABI mismatch). This looks like a
    /// genuine defect in this SDK build's `kdu_stripe_compressor`, not a usage error - flagged
    /// for the user rather than guessed at further.
    #[test]
    #[ignore = "Kakadu v7.8 kdu_stripe_compressor produces incorrect pixel data on encode - see doc comment"]
    fn known_broken_lossless_round_trip() {
        let original: Vec<u8> = (0..WIDTH * HEIGHT).map(|i| (i % WIDTH) as u8).collect();
        let codestream = encode_jpeg2000(&original, HEIGHT, WIDTH, 1, 8, false, true, 1.0)
            .expect("lossless encode should succeed");
        let decoded = decode_jpeg2000(&codestream, HEIGHT, WIDTH, 1, 8, false)
            .expect("decode should succeed");
        assert_eq!(decoded, original);
    }
}
