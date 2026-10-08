//! Classic JPEG 2000 decode through raw `openjpeg-sys`, with an explicit thread count.
//!
//! This replaces the `jpeg2k` crate, which either never threads (its `threads` feature off) or
//! always uses every CPU it can see (feature on) - neither fits a decoder that's also called from
//! inside rayon's frame-parallel multi-frame decode. See [`crate::threads`].

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};

use openjpeg_sys as sys;

use crate::threads::per_call_threads;
use crate::{DecodedComponent, DecodedImage};

const JP2_SIGNATURE: &[u8] = &[0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A];
const J2K_SIGNATURE: &[u8] = &[0xFF, 0x4F, 0xFF, 0x51];

struct Source<'a> {
    data: &'a [u8],
    pos: usize,
}

unsafe extern "C" fn read_fn(buffer: *mut c_void, nb_bytes: usize, user_data: *mut c_void) -> usize {
    let source = unsafe { &mut *(user_data as *mut Source) };
    let remaining = source.data.len() - source.pos;
    if remaining == 0 {
        return usize::MAX; // (OPJ_SIZE_T)-1: end of stream
    }
    let n = nb_bytes.min(remaining);
    unsafe { std::ptr::copy_nonoverlapping(source.data.as_ptr().add(source.pos), buffer as *mut u8, n) };
    source.pos += n;
    n
}

unsafe extern "C" fn skip_fn(nb_bytes: i64, user_data: *mut c_void) -> i64 {
    let source = unsafe { &mut *(user_data as *mut Source) };
    if nb_bytes < 0 {
        return -1;
    }
    let n = (nb_bytes as usize).min(source.data.len() - source.pos);
    source.pos += n;
    n as i64
}

unsafe extern "C" fn seek_fn(nb_bytes: i64, user_data: *mut c_void) -> i32 {
    let source = unsafe { &mut *(user_data as *mut Source) };
    if nb_bytes < 0 || nb_bytes as usize > source.data.len() {
        return 0;
    }
    source.pos = nb_bytes as usize;
    1
}

unsafe extern "C" fn error_fn(msg: *const c_char, client_data: *mut c_void) {
    let message = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    let errors = unsafe { &mut *(client_data as *mut Vec<String>) };
    errors.push(message.trim_end().to_owned());
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

/// Decodes a J2K codestream (or a JP2-wrapped one - some encoders put JP2 boxes inside DICOM
/// fragments) at full resolution, using [`per_call_threads`] threads.
pub fn decode(data: &[u8]) -> Result<DecodedImage, String> {
    let format = if data.starts_with(JP2_SIGNATURE) {
        sys::OPJ_CODEC_FORMAT::OPJ_CODEC_JP2
    } else if data.starts_with(J2K_SIGNATURE) {
        sys::OPJ_CODEC_FORMAT::OPJ_CODEC_J2K
    } else {
        return Err("not a JPEG 2000 codestream (no J2K/JP2 signature)".to_owned());
    };

    let mut errors: Vec<String> = Vec::new();
    let fail = |what: &str, errors: &[String]| {
        if errors.is_empty() {
            format!("OpenJPEG {what} failed")
        } else {
            format!("OpenJPEG {what} failed: {}", errors.join("; "))
        }
    };

    let codec = Codec(unsafe { sys::opj_create_decompress(format) });
    if codec.0.is_null() {
        return Err("opj_create_decompress failed".to_owned());
    }
    unsafe {
        sys::opj_set_error_handler(codec.0, Some(error_fn), &mut errors as *mut Vec<String> as *mut c_void);
    }

    let mut params: sys::opj_dparameters_t = unsafe { std::mem::zeroed() };
    unsafe { sys::opj_set_default_decoder_parameters(&mut params) };
    if unsafe { sys::opj_setup_decoder(codec.0, &mut params) } == 0 {
        return Err(fail("decoder setup", &errors));
    }
    let threads = per_call_threads();
    if threads > 1 && unsafe { sys::opj_has_thread_support() } != 0 {
        unsafe { sys::opj_codec_set_threads(codec.0, threads as i32) };
    }

    let mut source = Box::new(Source { data, pos: 0 });
    let stream = Stream(unsafe { sys::opj_stream_create(1 << 20, 1) });
    if stream.0.is_null() {
        return Err("opj_stream_create failed".to_owned());
    }
    unsafe {
        sys::opj_stream_set_read_function(stream.0, Some(read_fn));
        sys::opj_stream_set_skip_function(stream.0, Some(skip_fn));
        sys::opj_stream_set_seek_function(stream.0, Some(seek_fn));
        sys::opj_stream_set_user_data_length(stream.0, data.len() as u64);
        sys::opj_stream_set_user_data(stream.0, source.as_mut() as *mut Source as *mut c_void, None);
    }

    let mut image_ptr: *mut sys::opj_image_t = std::ptr::null_mut();
    if unsafe { sys::opj_read_header(stream.0, codec.0, &mut image_ptr) } == 0 {
        return Err(fail("header read", &errors));
    }
    let image = Image(image_ptr);
    if unsafe { sys::opj_decode(codec.0, stream.0, image.0) } == 0 {
        return Err(fail("decode", &errors));
    }
    if unsafe { sys::opj_end_decompress(codec.0, stream.0) } == 0 {
        return Err(fail("end of decode", &errors));
    }

    let raw = unsafe { &*image.0 };
    let comps = unsafe { std::slice::from_raw_parts(raw.comps, raw.numcomps as usize) };
    let mut components = Vec::with_capacity(comps.len());
    for comp in comps {
        if comp.data.is_null() {
            return Err("OpenJPEG returned a component with no data".to_owned());
        }
        let len = comp.w as usize * comp.h as usize;
        let data = unsafe { std::slice::from_raw_parts(comp.data, len) }.to_vec();
        components.push(DecodedComponent {
            width: comp.w,
            height: comp.h,
            precision: comp.prec as u8,
            signed: comp.sgnd != 0,
            data,
        });
    }
    drop(stream);
    drop(source);
    Ok(DecodedImage { components })
}
