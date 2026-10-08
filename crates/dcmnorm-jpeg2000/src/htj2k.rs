//! HTJ2K (JPEG 2000 Part 15) via the vendored OpenHTJ2K library - see build.rs for how the two
//! CPU variants are built and kept apart, and src/htj2k_bridge.cpp for the C ABI.
//!
//! OpenHTJ2K keeps one process-wide worker pool, sized by the first call that needs one and shared
//! by every later decode/encode, so the thread count passed here is effectively fixed at first use:
//! [`crate::threads::configured_threads`].
//!
//! Calls are serialized through [`CODEC_LOCK`]: OpenHTJ2K is not safe for concurrent decodes or
//! encodes in one process (a thread blocked in one call's barrier runs other calls' pool tasks
//! inline, sharing per-thread scratch buffers - concurrent decodes segfault, see
//! `concurrent_decodes_are_serialized_safely`). Each call already spreads one image across the
//! whole pool, so serializing costs little throughput.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::{Mutex, OnceLock};

use crate::threads::configured_threads;
use crate::{DecodedComponent, DecodedImage};

/// Forces the portable (non-AVX2) build even on a CPU that supports x86-64-v3 - set to `base`.
/// Diagnostic escape hatch only.
pub const SIMD_ENV: &str = "DCMNORM_HTJ2K_SIMD";

#[repr(C)]
struct RawImage {
    num_components: u16,
    planes: *mut *mut i32,
    widths: *mut u32,
    heights: *mut u32,
    depths: *mut u8,
    is_signed: *mut u8,
}

impl RawImage {
    const fn empty() -> Self {
        Self {
            num_components: 0,
            planes: std::ptr::null_mut(),
            widths: std::ptr::null_mut(),
            heights: std::ptr::null_mut(),
            depths: std::ptr::null_mut(),
            is_signed: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct RawInfo {
    num_components: u16,
    width: u32,
    height: u32,
    depth: u8,
    is_signed: u8,
}

type DecodeFn = unsafe extern "C" fn(*const u8, usize, u32, *mut RawImage, *mut *mut c_char) -> c_int;
type DecodeIntoFn = unsafe extern "C" fn(
    *const u8,
    usize,
    u32,
    *mut u8,
    usize,
    u32,
    u16,
    u32,
    u32,
    *mut RawInfo,
    *mut *mut c_char,
) -> c_int;
type FreeImageFn = unsafe extern "C" fn(*mut RawImage);
type EncodeFn = unsafe extern "C" fn(
    *const *const i32,
    u16,
    u32,
    u32,
    u8,
    c_int,
    c_int,
    u32,
    *mut *mut u8,
    *mut usize,
    *mut *mut c_char,
) -> c_int;
type FreeBufferFn = unsafe extern "C" fn(*mut std::ffi::c_void);

extern "C" {
    fn dcmnorm_htj2k_base_decode(
        codestream: *const u8,
        length: usize,
        num_threads: u32,
        out: *mut RawImage,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn dcmnorm_htj2k_base_decode_into(
        codestream: *const u8,
        length: usize,
        num_threads: u32,
        out: *mut u8,
        out_len: usize,
        bytes_per_sample: u32,
        expected_components: u16,
        expected_width: u32,
        expected_height: u32,
        info: *mut RawInfo,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn dcmnorm_htj2k_base_free_image(image: *mut RawImage);
    fn dcmnorm_htj2k_base_encode(
        planes: *const *const i32,
        num_components: u16,
        width: u32,
        height: u32,
        bit_depth: u8,
        is_signed: c_int,
        rpcl: c_int,
        num_threads: u32,
        out_data: *mut *mut u8,
        out_len: *mut usize,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn dcmnorm_htj2k_base_free_buffer(buffer: *mut std::ffi::c_void);
}

#[cfg(dcmnorm_htj2k_v3)]
extern "C" {
    fn dcmnorm_htj2k_v3_decode(
        codestream: *const u8,
        length: usize,
        num_threads: u32,
        out: *mut RawImage,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn dcmnorm_htj2k_v3_decode_into(
        codestream: *const u8,
        length: usize,
        num_threads: u32,
        out: *mut u8,
        out_len: usize,
        bytes_per_sample: u32,
        expected_components: u16,
        expected_width: u32,
        expected_height: u32,
        info: *mut RawInfo,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn dcmnorm_htj2k_v3_free_image(image: *mut RawImage);
    fn dcmnorm_htj2k_v3_encode(
        planes: *const *const i32,
        num_components: u16,
        width: u32,
        height: u32,
        bit_depth: u8,
        is_signed: c_int,
        rpcl: c_int,
        num_threads: u32,
        out_data: *mut *mut u8,
        out_len: *mut usize,
        error_message: *mut *mut c_char,
    ) -> c_int;
    fn dcmnorm_htj2k_v3_free_buffer(buffer: *mut std::ffi::c_void);
}

/// Serializes every call into OpenHTJ2K - see the module docs.
static CODEC_LOCK: Mutex<()> = Mutex::new(());

struct Variant {
    name: &'static str,
    decode: DecodeFn,
    decode_into: DecodeIntoFn,
    free_image: FreeImageFn,
    encode: EncodeFn,
    free_buffer: FreeBufferFn,
}

const BASE: Variant = Variant {
    name: "base",
    decode: dcmnorm_htj2k_base_decode,
    decode_into: dcmnorm_htj2k_base_decode_into,
    free_image: dcmnorm_htj2k_base_free_image,
    encode: dcmnorm_htj2k_base_encode,
    free_buffer: dcmnorm_htj2k_base_free_buffer,
};

#[cfg(dcmnorm_htj2k_v3)]
const V3: Variant = Variant {
    name: "x86-64-v3",
    decode: dcmnorm_htj2k_v3_decode,
    decode_into: dcmnorm_htj2k_v3_decode_into,
    free_image: dcmnorm_htj2k_v3_free_image,
    encode: dcmnorm_htj2k_v3_encode,
    free_buffer: dcmnorm_htj2k_v3_free_buffer,
};

fn variant() -> &'static Variant {
    static SELECTED: OnceLock<&'static Variant> = OnceLock::new();
    SELECTED.get_or_init(|| {
        #[cfg(dcmnorm_htj2k_v3)]
        {
            let forced_base = std::env::var(SIMD_ENV)
                .map(|value| value.trim().eq_ignore_ascii_case("base"))
                .unwrap_or(false);
            // The full x86-64-v3 feature set the v3 variant was compiled for.
            if !forced_base
                && std::is_x86_feature_detected!("avx2")
                && std::is_x86_feature_detected!("fma")
                && std::is_x86_feature_detected!("bmi1")
                && std::is_x86_feature_detected!("bmi2")
                && std::is_x86_feature_detected!("lzcnt")
                && std::is_x86_feature_detected!("movbe")
                && std::is_x86_feature_detected!("f16c")
            {
                return &V3;
            }
        }
        &BASE
    })
}

/// Which compiled OpenHTJ2K variant this process uses (`"x86-64-v3"` or `"base"`).
pub fn variant_name() -> &'static str {
    variant().name
}

fn take_error(error_message: *mut c_char, free_buffer: FreeBufferFn) -> String {
    if error_message.is_null() {
        return "OpenHTJ2K failed without an error message".to_owned();
    }
    let message = unsafe { CStr::from_ptr(error_message) }.to_string_lossy().into_owned();
    unsafe { free_buffer(error_message.cast()) };
    message
}

/// Decodes a raw HTJ2K (or classic Part 1) codestream at full resolution.
pub fn decode(codestream: &[u8]) -> Result<DecodedImage, String> {
    let variant = variant();
    let mut raw = RawImage::empty();
    let mut error: *mut c_char = std::ptr::null_mut();
    let _guard = CODEC_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let status = unsafe {
        (variant.decode)(
            codestream.as_ptr(),
            codestream.len(),
            configured_threads(),
            &mut raw,
            &mut error,
        )
    };
    if status != 0 {
        return Err(take_error(error, variant.free_buffer));
    }

    let count = usize::from(raw.num_components);
    let mut components = Vec::with_capacity(count);
    for c in 0..count {
        let (width, height, precision, signed, plane) = unsafe {
            (
                *raw.widths.add(c),
                *raw.heights.add(c),
                *raw.depths.add(c),
                *raw.is_signed.add(c) != 0,
                *raw.planes.add(c),
            )
        };
        let len = width as usize * height as usize;
        let data = unsafe { std::slice::from_raw_parts(plane, len) }.to_vec();
        components.push(DecodedComponent { width, height, precision, signed, data });
    }
    unsafe { (variant.free_image)(&mut raw) };
    Ok(DecodedImage { components })
}

/// Why [`decode_into`] didn't fill the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeIntoError {
    /// The codestream's layout differs from the expected one (component count, dimensions,
    /// per-component size/depth, or a depth too wide for the sample size). Nothing was decoded;
    /// use [`decode`] instead.
    LayoutMismatch { components: u16, width: u32, height: u32, depth: u8 },
    /// The codestream couldn't be decoded.
    Failed(String),
}

/// Decodes straight into `out` as DICOM native pixel data: `components` samples interleaved per
/// pixel, `bytes_per_sample` (1 or 2) little-endian bytes each (low bits of the two's-complement
/// value for signed data). About twice as fast as [`decode`] for large images.
pub fn decode_into(
    codestream: &[u8],
    out: &mut [u8],
    bytes_per_sample: u32,
    components: u16,
    width: u32,
    height: u32,
) -> Result<(), DecodeIntoError> {
    let variant = variant();
    let mut info = RawInfo::default();
    let mut error: *mut c_char = std::ptr::null_mut();
    let _guard = CODEC_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let status = unsafe {
        (variant.decode_into)(
            codestream.as_ptr(),
            codestream.len(),
            configured_threads(),
            out.as_mut_ptr(),
            out.len(),
            bytes_per_sample,
            components,
            width,
            height,
            &mut info,
            &mut error,
        )
    };
    match status {
        0 => Ok(()),
        2 => Err(DecodeIntoError::LayoutMismatch {
            components: info.num_components,
            width: info.width,
            height: info.height,
            depth: info.depth,
        }),
        _ => Err(DecodeIntoError::Failed(take_error(error, variant.free_buffer))),
    }
}

/// Progression order for [`encode_lossless`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progression {
    Lrcp,
    Rpcl,
}

/// Lossless (reversible 5/3, single layer, no colour transform) HTJ2K encode of planar samples:
/// `planes.len()` components, each `width * height` values at `bit_depth` bits (1-16).
pub fn encode_lossless(
    planes: &[&[i32]],
    width: u32,
    height: u32,
    bit_depth: u8,
    signed: bool,
    progression: Progression,
) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize;
    if planes.is_empty() || planes.len() > u16::MAX as usize {
        return Err(format!("unsupported component count {}", planes.len()));
    }
    if let Some(bad) = planes.iter().find(|plane| plane.len() != expected) {
        return Err(format!("plane has {} samples, expected {expected}", bad.len()));
    }

    let variant = variant();
    let pointers: Vec<*const i32> = planes.iter().map(|plane| plane.as_ptr()).collect();
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len = 0usize;
    let mut error: *mut c_char = std::ptr::null_mut();
    let _guard = CODEC_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let status = unsafe {
        (variant.encode)(
            pointers.as_ptr(),
            planes.len() as u16,
            width,
            height,
            bit_depth,
            c_int::from(signed),
            c_int::from(progression == Progression::Rpcl),
            configured_threads(),
            &mut data,
            &mut len,
            &mut error,
        )
    };
    if status != 0 {
        return Err(take_error(error, variant.free_buffer));
    }
    let encoded = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    unsafe { (variant.free_buffer)(data.cast()) };
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(width: u32, height: u32, bits: u8, signed: bool) -> Vec<i32> {
        let max = (1i64 << bits) - 1;
        (0..width * height)
            .map(|i| {
                // Sharp discontinuities: a lossy codec masquerading as lossless can't fake these.
                let v = (i64::from(i).wrapping_mul(2_654_435_761) >> 7) & max;
                if signed {
                    (v - (1i64 << (bits - 1))) as i32
                } else {
                    v as i32
                }
            })
            .collect()
    }

    #[test]
    fn lossless_round_trip_is_exact() {
        for &(width, height, bits, signed, components) in &[
            (37u32, 29u32, 8u8, false, 1usize),
            (512, 512, 12, false, 1),
            (300, 200, 16, true, 1),
            (64, 48, 8, false, 3),
            (1, 1, 8, false, 1),
        ] {
            let planes: Vec<Vec<i32>> = (0..components).map(|c| {
                let mut p = pattern(width, height, bits, signed);
                let n = p.len();
                p.rotate_left(c * 7 % n);
                p
            }).collect();
            let refs: Vec<&[i32]> = planes.iter().map(Vec::as_slice).collect();
            for progression in [Progression::Lrcp, Progression::Rpcl] {
                let encoded = encode_lossless(&refs, width, height, bits, signed, progression)
                    .unwrap_or_else(|e| panic!("{width}x{height}x{components} {bits}-bit: {e}"));
                let decoded = decode(&encoded).unwrap();
                assert_eq!(decoded.components.len(), components);
                for (c, component) in decoded.components.iter().enumerate() {
                    assert_eq!((component.width, component.height), (width, height));
                    assert_eq!(component.precision, bits);
                    assert_eq!(component.signed, signed);
                    assert_eq!(component.data, planes[c], "{width}x{height} {bits}-bit c{c}");
                }
            }
        }
    }

    #[test]
    fn decode_into_matches_plane_decode_and_rejects_mismatches() {
        for &(bits, signed, components) in &[(12u8, false, 1usize), (16, true, 1), (8, false, 3), (8, true, 1)] {
            let (width, height) = (97u32, 61u32);
            let planes: Vec<Vec<i32>> = (0..components)
                .map(|c| pattern(width, height, bits, signed).into_iter().map(|v| v ^ c as i32).collect())
                .collect();
            let refs: Vec<&[i32]> = planes.iter().map(Vec::as_slice).collect();
            let encoded = encode_lossless(&refs, width, height, bits, signed, Progression::Lrcp).unwrap();
            let bps = if bits > 8 { 2usize } else { 1 };
            let mut out = vec![0u8; width as usize * height as usize * components * bps];
            decode_into(&encoded, &mut out, bps as u32, components as u16, width, height).unwrap();
            let mut expected = Vec::with_capacity(out.len());
            for i in 0..(width * height) as usize {
                for plane in &planes {
                    expected.extend_from_slice(&plane[i].to_le_bytes()[..bps]);
                }
            }
            assert_eq!(out, expected, "{bits}-bit signed={signed} x{components}");

            let mismatch = decode_into(&encoded, &mut out, bps as u32, components as u16, width + 1, height);
            assert!(matches!(mismatch, Err(DecodeIntoError::LayoutMismatch { width: 97, .. })));
        }
        assert!(matches!(decode_into(&[1, 2, 3], &mut [0u8; 4], 1, 1, 2, 2), Err(DecodeIntoError::Failed(_))));
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        assert!(decode(&[0xFF, 0x4F, 0xFF, 0x51, 0, 1, 2, 3]).is_err());
        assert!(decode(&[1, 2, 3]).is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn concurrent_decodes_are_serialized_safely() {
        let plane = pattern(1024, 768, 12, false);
        let encoded = encode_lossless(&[&plane], 1024, 768, 12, false, Progression::Lrcp).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..10 {
                        let decoded = decode(&encoded).unwrap();
                        assert_eq!(decoded.components[0].data, plane);
                        let again =
                            encode_lossless(&[&plane], 1024, 768, 12, false, Progression::Rpcl).unwrap();
                        assert_eq!(decode(&again).unwrap().components[0].data, plane);
                    }
                });
            }
        });
    }
}
