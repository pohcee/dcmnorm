//! HTJ2K (JPEG 2000 Part 15) and Part 1 decode and lossless HTJ2K encode via pure-Rust `j2k`.

use j2k::{
    encode_j2k_lossless, CpuDecodeParallelism, DecodeSettings, J2kBlockCodingMode, J2kDecoder,
    J2kEncodeValidation, J2kLosslessEncodeOptions, J2kLosslessSamples, J2kProgressionOrder,
    J2kView, ReversibleTransform,
};

use crate::threads::per_call_threads;
use crate::{DecodedComponent, DecodedImage};

/// Historical diagnostic env var kept for API compatibility.
pub const SIMD_ENV: &str = "DCMNORM_HTJ2K_SIMD";

/// Which compiled JPEG 2000 backend variant this process uses.
pub fn variant_name() -> &'static str {
    "j2k-rust"
}

fn decode_parallelism() -> CpuDecodeParallelism {
    if per_call_threads() <= 1 {
        CpuDecodeParallelism::Serial
    } else {
        CpuDecodeParallelism::Auto
    }
}

fn parse_view(codestream: &[u8]) -> Result<J2kView<'_>, String> {
    J2kView::parse(codestream).or_else(|strict_err| {
        // Some JP2-wrapped DICOM fragments carry non-strict container metadata boxes; allow
        // lenient JP2/JPH metadata recovery only for JP2/JPH containers, never raw codestreams.
        if codestream.starts_with(&crate::codestream::JP2_SIGNATURE) {
            J2kView::parse_with_settings(codestream, DecodeSettings::lenient())
                .map_err(|e| e.to_string())
        } else {
            Err(strict_err.to_string())
        }
    })
}

fn native_plane_to_i32(plane: &j2k::J2kNativeComponentPlane) -> Result<Vec<i32>, String> {
    let data = plane.data();
    let signed = plane.signed();
    match plane.bytes_per_sample() {
        1 => Ok(data
            .iter()
            .map(|&b| {
                if signed {
                    i32::from(b as i8)
                } else {
                    i32::from(b)
                }
            })
            .collect()),
        2 => Ok(data
            .chunks_exact(2)
            .map(|c| {
                if signed {
                    i32::from(i16::from_le_bytes([c[0], c[1]]))
                } else {
                    i32::from(u16::from_le_bytes([c[0], c[1]]))
                }
            })
            .collect()),
        4 => Ok(data
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()),
        other => Err(format!("unsupported decoded bytes_per_sample: {other}")),
    }
}

/// Decodes a raw HTJ2K, classic Part 1, or JP2-wrapped codestream at full resolution.
pub fn decode(codestream: &[u8]) -> Result<DecodedImage, String> {
    let view = parse_view(codestream)?;
    let mut decoder = J2kDecoder::from_view(view).map_err(|e| e.to_string())?;
    decoder.set_cpu_decode_parallelism(decode_parallelism());
    let decoded = decoder
        .decode_native_components()
        .map_err(|e| format!("j2k decode failed: {e}"))?;

    let mut components = Vec::with_capacity(decoded.planes().len());
    for plane in decoded.planes() {
        let (width, height) = plane.dimensions();
        components.push(DecodedComponent {
            width,
            height,
            precision: plane.bit_depth(),
            signed: plane.signed(),
            data: native_plane_to_i32(plane)?,
        });
    }
    Ok(DecodedImage { components })
}

/// Why [`decode_into`] didn't fill the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeIntoError {
    /// The codestream's layout differs from the expected one (component count, dimensions,
    /// per-component size/depth, or a depth too wide for the sample size). Nothing was decoded;
    /// use [`decode`] instead.
    LayoutMismatch {
        components: u16,
        width: u32,
        height: u32,
        depth: u8,
    },
    /// The codestream couldn't be decoded.
    Failed(String),
}

/// Decodes straight into `out` as DICOM native pixel data: `components` samples interleaved per
/// pixel, `bytes_per_sample` (1 or 2) little-endian bytes each (low bits of the two's-complement
/// value for signed data).
pub fn decode_into(
    codestream: &[u8],
    out: &mut [u8],
    bytes_per_sample: u32,
    components: u16,
    width: u32,
    height: u32,
) -> Result<(), DecodeIntoError> {
    if bytes_per_sample != 1 && bytes_per_sample != 2 {
        return Err(DecodeIntoError::Failed(format!(
            "unsupported bytes_per_sample {bytes_per_sample} (expected 1 or 2)"
        )));
    }

    let view = parse_view(codestream).map_err(DecodeIntoError::Failed)?;
    let info = view.info();
    let (actual_w, actual_h) = info.dimensions;
    let actual_components = info.components;
    let (actual_depth, mixed_depth, subsampled) = if let Some(support) = view.support_info() {
        let depth = support.components.first().map_or(0, |c| c.bit_depth);
        (
            depth,
            support.has_mixed_bit_depths(),
            support.has_component_subsampling(),
        )
    } else {
        (0, false, false)
    };

    if actual_components != components
        || actual_w != width
        || actual_h != height
        || mixed_depth
        || subsampled
        || actual_depth == 0
        || u32::from(actual_depth) > bytes_per_sample * 8
    {
        return Err(DecodeIntoError::LayoutMismatch {
            components: actual_components,
            width: actual_w,
            height: actual_h,
            depth: actual_depth,
        });
    }

    let expected_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(usize::from(components)))
        .and_then(|n| n.checked_mul(bytes_per_sample as usize))
        .ok_or_else(|| DecodeIntoError::Failed("output buffer size overflow".to_owned()))?;
    if out.len() < expected_bytes {
        return Err(DecodeIntoError::Failed(format!(
            "output buffer too small: got {}, expected at least {expected_bytes}",
            out.len()
        )));
    }

    let mut decoder =
        J2kDecoder::from_view(view).map_err(|e| DecodeIntoError::Failed(e.to_string()))?;
    decoder.set_cpu_decode_parallelism(decode_parallelism());
    let decoded = decoder
        .decode_native_components()
        .map_err(|e| DecodeIntoError::Failed(format!("j2k decode failed: {e}")))?;

    let planes = decoded.planes();
    if planes.len() != usize::from(components)
        || planes.iter().any(|p| p.dimensions() != (width, height))
    {
        let first = planes.first();
        let (w, h) = first.map_or((0, 0), |p| p.dimensions());
        return Err(DecodeIntoError::LayoutMismatch {
            components: planes.len() as u16,
            width: w,
            height: h,
            depth: first.map_or(0, |p| p.bit_depth()),
        });
    }

    let dst = &mut out[..expected_bytes];
    let bps = bytes_per_sample as usize;
    let n_pix = width as usize * height as usize;

    // Fast path for single-component grayscale: copy straight from the fused grayscale IDWT plane.
    if planes.len() == 1 {
        let plane = &planes[0];
        let src = plane.data();
        if usize::from(plane.bytes_per_sample()) == bps {
            dst.copy_from_slice(src);
        } else if plane.bytes_per_sample() == 1 && bps == 2 {
            let signed = plane.signed();
            for (s, d) in src.iter().zip(dst.chunks_exact_mut(2)) {
                let v: [u8; 2] = if signed {
                    i16::from(*s as i8).to_le_bytes()
                } else {
                    u16::from(*s).to_le_bytes()
                };
                d.copy_from_slice(&v);
            }
        } else {
            return Err(DecodeIntoError::LayoutMismatch {
                components: 1,
                width,
                height,
                depth: plane.bit_depth(),
            });
        }
        return Ok(());
    }

    // Multi-component interleave into caller buffer.
    let spp = planes.len();
    if bps == 1 {
        for (c, plane) in planes.iter().enumerate() {
            if plane.bytes_per_sample() != 1 {
                return Err(DecodeIntoError::LayoutMismatch {
                    components: spp as u16,
                    width,
                    height,
                    depth: plane.bit_depth(),
                });
            }
            for (i, &b) in plane.data().iter().enumerate().take(n_pix) {
                dst[i * spp + c] = b;
            }
        }
    } else {
        for (c, plane) in planes.iter().enumerate() {
            let src = plane.data();
            match plane.bytes_per_sample() {
                2 => {
                    for (i, pair) in src.chunks_exact(2).enumerate().take(n_pix) {
                        let off = (i * spp + c) * 2;
                        dst[off] = pair[0];
                        dst[off + 1] = pair[1];
                    }
                }
                1 => {
                    let signed = plane.signed();
                    for (i, &b) in src.iter().enumerate().take(n_pix) {
                        let off = (i * spp + c) * 2;
                        let v: [u8; 2] = if signed {
                            i16::from(b as i8).to_le_bytes()
                        } else {
                            u16::from(b).to_le_bytes()
                        };
                        dst[off] = v[0];
                        dst[off + 1] = v[1];
                    }
                }
                _ => {
                    return Err(DecodeIntoError::LayoutMismatch {
                        components: spp as u16,
                        width,
                        height,
                        depth: plane.bit_depth(),
                    });
                }
            }
        }
    }

    Ok(())
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
    if planes.is_empty() || planes.len() > u16::MAX as usize {
        return Err(format!("unsupported component count {}", planes.len()));
    }
    if !(1..=16).contains(&bit_depth) {
        return Err(format!(
            "unsupported bit_depth {bit_depth} (expected 1..=16)"
        ));
    }
    let expected = width as usize * height as usize;
    if let Some(bad) = planes.iter().find(|plane| plane.len() != expected) {
        return Err(format!(
            "plane has {} samples, expected {expected}",
            bad.len()
        ));
    }

    let spp = planes.len();
    let bps = if bit_depth <= 8 { 1usize } else { 2usize };
    let mut interleaved = Vec::with_capacity(expected * spp * bps);

    if bps == 1 {
        if spp == 1 {
            for &v in planes[0] {
                interleaved.push(if signed { (v as i8) as u8 } else { v as u8 });
            }
        } else {
            for i in 0..expected {
                for plane in planes {
                    let v = plane[i];
                    interleaved.push(if signed { (v as i8) as u8 } else { v as u8 });
                }
            }
        }
    } else if spp == 1 {
        for &v in planes[0] {
            let bytes = if signed {
                (v as i16).to_le_bytes()
            } else {
                (v as u16).to_le_bytes()
            };
            interleaved.extend_from_slice(&bytes);
        }
    } else {
        for i in 0..expected {
            for plane in planes {
                let v = plane[i];
                let bytes = if signed {
                    (v as i16).to_le_bytes()
                } else {
                    (v as u16).to_le_bytes()
                };
                interleaved.extend_from_slice(&bytes);
            }
        }
    }

    let samples = J2kLosslessSamples {
        data: &interleaved,
        width,
        height,
        components: spp as u16,
        bit_depth,
        signed,
    };
    let options = J2kLosslessEncodeOptions::default()
        .with_block_coding_mode(J2kBlockCodingMode::HighThroughput)
        .with_progression(match progression {
            Progression::Lrcp => J2kProgressionOrder::Lrcp,
            Progression::Rpcl => J2kProgressionOrder::Rpcl,
        })
        .with_reversible_transform(ReversibleTransform::None53)
        .with_max_decomposition_levels(Some(5))
        .with_validation(J2kEncodeValidation::External);

    encode_j2k_lossless(samples, &options)
        .map(|encoded| encoded.codestream)
        .map_err(|e| e.to_string())
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
            (1u32, 1u32, 8u8, false, 1usize),
            (2, 2, 12, false, 1),
            (7, 5, 16, false, 1),
            (16, 16, 16, true, 1),
            (37, 29, 8, false, 1),
            (64, 64, 16, false, 1),
            (512, 512, 12, false, 1),
            (300, 200, 16, true, 1),
            (64, 48, 8, false, 3),
        ] {
            let planes: Vec<Vec<i32>> = (0..components)
                .map(|c| {
                    let mut p = pattern(width, height, bits, signed);
                    let n = p.len();
                    p.rotate_left(c * 7 % n);
                    p
                })
                .collect();
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
                    assert_eq!(
                        component.data, planes[c],
                        "{width}x{height} {bits}-bit c{c}"
                    );
                }
            }
        }
    }

    #[test]
    fn decode_into_matches_plane_decode_and_rejects_mismatches() {
        for &(bits, signed, components) in &[
            (12u8, false, 1usize),
            (16, true, 1),
            (8, false, 3),
            (8, true, 1),
        ] {
            let (width, height) = (97u32, 61u32);
            let planes: Vec<Vec<i32>> = (0..components)
                .map(|c| {
                    pattern(width, height, bits, signed)
                        .into_iter()
                        .map(|v| v ^ c as i32)
                        .collect()
                })
                .collect();
            let refs: Vec<&[i32]> = planes.iter().map(Vec::as_slice).collect();
            let encoded =
                encode_lossless(&refs, width, height, bits, signed, Progression::Lrcp).unwrap();
            let bps = if bits > 8 { 2usize } else { 1 };
            let mut out = vec![0u8; width as usize * height as usize * components * bps];
            decode_into(
                &encoded,
                &mut out,
                bps as u32,
                components as u16,
                width,
                height,
            )
            .unwrap();
            let mut expected = Vec::with_capacity(out.len());
            for i in 0..(width * height) as usize {
                for plane in &planes {
                    expected.extend_from_slice(&plane[i].to_le_bytes()[..bps]);
                }
            }
            assert_eq!(out, expected, "{bits}-bit signed={signed} x{components}");

            let mismatch = decode_into(
                &encoded,
                &mut out,
                bps as u32,
                components as u16,
                width + 1,
                height,
            );
            assert!(matches!(
                mismatch,
                Err(DecodeIntoError::LayoutMismatch { width: 97, .. })
            ));
        }
        assert!(matches!(
            decode_into(&[1, 2, 3], &mut [0u8; 4], 1, 1, 2, 2),
            Err(DecodeIntoError::Failed(_))
        ));
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        assert!(decode(&[0xFF, 0x4F, 0xFF, 0x51, 0, 1, 2, 3]).is_err());
        assert!(decode(&[1, 2, 3]).is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn truncated_codestream_is_rejected() {
        let plane = pattern(128, 128, 12, false);
        let encoded = encode_lossless(&[&plane], 128, 128, 12, false, Progression::Lrcp).unwrap();
        assert!(encoded.len() > 64);

        let mut out = vec![0u8; 128 * 128 * 2];
        // Cut right before EOC (missing EOC marker) and mid-payload.
        for cut in [encoded.len() - 2, encoded.len() / 2] {
            let truncated = &encoded[..cut];
            let err = decode(truncated).expect_err("truncated codestream must fail decode()");
            assert!(
                err.contains("truncated") || err.contains("failed"),
                "unexpected error for cut={cut}: {err}"
            );
            assert!(
                matches!(
                    decode_into(truncated, &mut out, 2, 1, 128, 128),
                    Err(DecodeIntoError::Failed(_))
                ),
                "truncated codestream must fail decode_into() at cut={cut}"
            );
        }
    }

    #[test]
    fn concurrent_calls_are_thread_safe() {
        let plane_a = pattern(1024, 768, 12, false);
        let encoded_a =
            encode_lossless(&[&plane_a], 1024, 768, 12, false, Progression::Lrcp).unwrap();
        let plane_b = pattern(256, 256, 16, true);
        let encoded_b =
            encode_lossless(&[&plane_b], 256, 256, 16, true, Progression::Lrcp).unwrap();
        let expected_b: Vec<u8> = plane_b
            .iter()
            .flat_map(|v| v.to_le_bytes()[..2].to_vec())
            .collect();

        std::thread::scope(|scope| {
            for t in 0..8 {
                let plane_a = &plane_a;
                let encoded_a = &encoded_a;
                let plane_b = &plane_b;
                let encoded_b = &encoded_b;
                let expected_b = &expected_b;
                scope.spawn(move || {
                    let mut out_b = vec![0u8; expected_b.len()];
                    for i in 0..10 {
                        if (t + i) % 2 == 0 {
                            let decoded = decode(encoded_a).unwrap();
                            assert_eq!(decoded.components[0].data, *plane_a);
                            let again = encode_lossless(
                                &[plane_a],
                                1024,
                                768,
                                12,
                                false,
                                Progression::Rpcl,
                            )
                            .unwrap();
                            assert_eq!(decode(&again).unwrap().components[0].data, *plane_a);
                        } else {
                            decode_into(encoded_b, &mut out_b, 2, 1, 256, 256).unwrap();
                            assert_eq!(out_b, *expected_b);
                            let again =
                                encode_lossless(&[plane_b], 256, 256, 16, true, Progression::Lrcp)
                                    .unwrap();
                            decode_into(&again, &mut out_b, 2, 1, 256, 256).unwrap();
                            assert_eq!(out_b, *expected_b);
                        }
                    }
                });
            }
        });

        // Concurrent Rayon workers exercising single-threaded per_call_threads() == 1 mode.
        use rayon::prelude::*;
        (0..16).into_par_iter().for_each(|_| {
            assert_eq!(per_call_threads(), 1);
            let mut out_b = vec![0u8; expected_b.len()];
            decode_into(&encoded_b, &mut out_b, 2, 1, 256, 256).unwrap();
            assert_eq!(out_b, expected_b);
            let again =
                encode_lossless(&[&plane_b], 256, 256, 16, true, Progression::Lrcp).unwrap();
            decode_into(&again, &mut out_b, 2, 1, 256, 256).unwrap();
            assert_eq!(out_b, expected_b);
        });
    }
}
