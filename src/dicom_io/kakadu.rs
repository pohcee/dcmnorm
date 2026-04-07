#[cfg(feature = "kakadu-ffi")]
use std::ffi::CStr;
#[cfg(feature = "kakadu-ffi")]
use std::os::raw::{c_char, c_int};

pub fn kakadu_ffi_enabled() -> bool {
    cfg!(feature = "kakadu-ffi")
}

#[cfg(feature = "kakadu-ffi")]
unsafe extern "C" {
    fn dcmnorm_kakadu_encode(
        pixels: *const u8,
        pixels_len: usize,
        rows: c_int,
        cols: c_int,
        samples_per_pixel: c_int,
        bits_stored: c_int,
        is_signed: c_int,
        reversible: c_int,
        out_data: *mut *mut u8,
        out_len: *mut usize,
        error_message: *mut *mut c_char,
    ) -> c_int;
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
pub(super) fn encode_jpeg2000(
    pixels: &[u8],
    rows: usize,
    cols: usize,
    samples_per_pixel: usize,
    bits_stored: u16,
    is_signed: bool,
    reversible: bool,
) -> Result<Vec<u8>, String> {
    let mut out_data = std::ptr::null_mut();
    let mut out_len = 0usize;
    let mut error_message = std::ptr::null_mut();

    let status = unsafe {
        dcmnorm_kakadu_encode(
            pixels.as_ptr(),
            pixels.len(),
            rows as c_int,
            cols as c_int,
            samples_per_pixel as c_int,
            bits_stored as c_int,
            is_signed as c_int,
            reversible as c_int,
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
pub(super) fn encode_jpeg2000(
    _pixels: &[u8],
    _rows: usize,
    _cols: usize,
    _samples_per_pixel: usize,
    _bits_stored: u16,
    _is_signed: bool,
    _reversible: bool,
) -> Result<Vec<u8>, String> {
    Err("Kakadu FFI is not enabled in this build".to_owned())
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
