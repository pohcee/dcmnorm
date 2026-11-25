//! JPEG-LS encoder (ITU-T T.87).
//!
//! Writes a conformant JPEG-LS bitstream (SOI, SOF55, SOS, entropy-coded
//! scan data, EOI) for grayscale and 3-component images (`ILV = 0, 1, 2`).

use std::io::Write;

use crate::bitstream::BitWriter;
use crate::context::ContextModel;
use crate::error::CodecError;
use crate::predictor::{clamp, predict_med};
use crate::run_mode;
use crate::EncodeOptions;

// ---------------------------------------------------------------------------
// JPEG-LS markers
// ---------------------------------------------------------------------------

const MARKER_SOI: u16 = 0xFFD8;
const MARKER_EOI: u16 = 0xFFD9;
const MARKER_SOS: u16 = 0xFFDA;
const MARKER_SOF55: u16 = 0xFFF7;

/// Default statistics reset threshold (T.87 A.2.1 RESET).
const DEFAULT_RESET: i32 = 64;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Encode a 16-bit grayscale image into JPEG-LS lossless format.
///
/// `pixels` is a row-major pixel buffer of length `width * height`.
/// The output is written to `w`. Uses [`EncodeOptions::default`]
/// ([`Profile::T87`], lossless).
pub fn encode(
    pixels: &[u16],
    width: u32,
    height: u32,
    w: &mut dyn Write,
) -> Result<(), CodecError> {
    encode_with_options(pixels, width, height, &EncodeOptions::default(), w)
}

/// Encode a 16-bit grayscale image with explicit [`EncodeOptions`].
pub fn encode_with_options(
    pixels: &[u16],
    width: u32,
    height: u32,
    opts: &EncodeOptions,
    w: &mut dyn Write,
) -> Result<(), CodecError> {
    // Dimensions: JPEG-LS SOF55 stores X/Y as 16-bit, and 0 is illegal.
    if width == 0 || width > 65535 {
        return Err(CodecError::InvalidParameter {
            name: "width",
            value: i64::from(width),
            allowed: "1..=65535",
        });
    }
    if height == 0 || height > 65535 {
        return Err(CodecError::InvalidParameter {
            name: "height",
            value: i64::from(height),
            allowed: "1..=65535",
        });
    }

    let width_usize = width as usize;
    let height_usize = height as usize;
    let expected = width_usize * height_usize;
    if pixels.len() != expected {
        return Err(CodecError::DimensionMismatch {
            expected,
            actual: pixels.len(),
        });
    }

    // Precision: explicit override (validated 2..=16) or derived from data.
    let max_pixel = pixels.iter().copied().max().unwrap_or(0);
    let precision = match opts.precision {
        Some(p) => {
            if !(2..=16).contains(&p) {
                return Err(CodecError::InvalidParameter {
                    name: "precision",
                    value: i64::from(p),
                    allowed: "2..=16",
                });
            }
            u32::from(p)
        }
        None => effective_precision(max_pixel),
    };
    let max_val: i32 = (1i32 << precision) - 1;

    if i32::from(max_pixel) > max_val {
        return Err(CodecError::InvalidParameter {
            name: "precision",
            value: i64::from(precision as u16),
            allowed: "large enough to hold every sample",
        });
    }

    // NEAR: bounded by min(255, MAXVAL/2) (T.87 C.2.4.1.1).
    let near = i32::from(opts.near);
    let near_max = (max_val / 2).min(255);
    if near > near_max {
        return Err(CodecError::InvalidParameter {
            name: "near",
            value: i64::from(opts.near),
            allowed: "0..=min(255, MAXVAL/2)",
        });
    }

    write_headers(w, precision, height as u16, width as u16, opts.near)?;
    let ctx = ContextModel::new(max_val, near, DEFAULT_RESET);
    encode_scan_t87(w, pixels, width_usize, height_usize, ctx)?;
    write_marker(w, MARKER_EOI)?;

    w.flush()?;
    Ok(())
}

/// Encode a native little-endian DICOM pixel buffer to a JPEG-LS codestream.
///
/// - For `bits_per_sample <= 8`, `data` holds 1 byte per sample (`u8`).
/// - For `bits_per_sample > 8`, `data` holds 2 bytes per sample little-endian (`u16`).
/// - For `components == 3`:
///   - `interleave_mode == 0`: `data` is component-planar (`RRR…GGG…BBB…`).
///   - `interleave_mode == 1` or `2`: `data` is sample-interleaved (`RGBRGB…`).
pub fn encode_bytes(
    data: &[u8],
    width: u32,
    height: u32,
    components: u8,
    bits_per_sample: u8,
    near: u8,
    interleave_mode: u8,
) -> Result<Vec<u8>, CodecError> {
    if !(2..=16).contains(&bits_per_sample) {
        return Err(CodecError::InvalidParameter {
            name: "bits_per_sample",
            value: i64::from(bits_per_sample),
            allowed: "2..=16",
        });
    }
    if components != 1 && components != 3 {
        return Err(CodecError::InvalidParameter {
            name: "components",
            value: i64::from(components),
            allowed: "1 or 3",
        });
    }
    let n = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| CodecError::InvalidData("image dimensions overflow".into()))?;
    let spp = components as usize;
    let bps = if bits_per_sample <= 8 { 1 } else { 2 };
    let expected_bytes = n * spp * bps;
    if data.len() != expected_bytes {
        return Err(CodecError::DimensionMismatch {
            expected: expected_bytes,
            actual: data.len(),
        });
    }

    let mut planar = vec![0u16; n * spp];
    if spp == 1 || interleave_mode == 0 {
        if bps == 1 {
            for (dst, &b) in planar.iter_mut().zip(data.iter()) {
                *dst = u16::from(b);
            }
        } else {
            for (dst, chunk) in planar.iter_mut().zip(data.as_chunks::<2>().0.iter()) {
                *dst = u16::from_le_bytes(*chunk);
            }
        }
    } else if bps == 1 {
        for i in 0..n {
            planar[i] = u16::from(data[i * 3]);
            planar[n + i] = u16::from(data[i * 3 + 1]);
            planar[2 * n + i] = u16::from(data[i * 3 + 2]);
        }
    } else {
        let chunks = data.as_chunks::<2>().0;
        for i in 0..n {
            planar[i] = u16::from_le_bytes(chunks[i * 3]);
            planar[n + i] = u16::from_le_bytes(chunks[i * 3 + 1]);
            planar[2 * n + i] = u16::from_le_bytes(chunks[i * 3 + 2]);
        }
    }

    let opts = EncodeOptions {
        near,
        precision: Some(bits_per_sample),
        interleave_mode: if components == 1 { 0 } else { interleave_mode },
    };
    let mut out = Vec::with_capacity(expected_bytes / 2 + 64);
    encode_image(&planar, width, height, components, &opts, &mut out)?;
    Ok(out)
}

/// Encode a 1- or 3-component image as a T.87 JPEG-LS stream (`ILV = 0, 1, or 2`).
///
/// `samples` is component-planar — all of component 0, then component 1, … —
/// of length `width * height * components`.
pub fn encode_image(
    samples: &[u16],
    width: u32,
    height: u32,
    components: u8,
    opts: &EncodeOptions,
    w: &mut dyn Write,
) -> Result<(), CodecError> {
    if components != 1 && components != 3 {
        return Err(CodecError::InvalidParameter {
            name: "components",
            value: i64::from(components),
            allowed: "1 or 3",
        });
    }
    if width == 0 || width > 65535 {
        return Err(CodecError::InvalidParameter {
            name: "width",
            value: i64::from(width),
            allowed: "1..=65535",
        });
    }
    if height == 0 || height > 65535 {
        return Err(CodecError::InvalidParameter {
            name: "height",
            value: i64::from(height),
            allowed: "1..=65535",
        });
    }

    let (wu, hu) = (width as usize, height as usize);
    let plane = wu * hu;
    let expected = plane * components as usize;
    if samples.len() != expected {
        return Err(CodecError::DimensionMismatch {
            expected,
            actual: samples.len(),
        });
    }

    let max_pixel = samples.iter().copied().max().unwrap_or(0);
    let precision = match opts.precision {
        Some(p) => {
            if !(2..=16).contains(&p) {
                return Err(CodecError::InvalidParameter {
                    name: "precision",
                    value: i64::from(p),
                    allowed: "2..=16",
                });
            }
            u32::from(p)
        }
        None => effective_precision(max_pixel),
    };
    let max_val: i32 = (1i32 << precision) - 1;
    if i32::from(max_pixel) > max_val {
        return Err(CodecError::InvalidParameter {
            name: "precision",
            value: i64::from(precision as u16),
            allowed: "large enough to hold every sample",
        });
    }

    let near = i32::from(opts.near);
    let near_max = (max_val / 2).min(255);
    if near > near_max {
        return Err(CodecError::InvalidParameter {
            name: "near",
            value: i64::from(opts.near),
            allowed: "0..=min(255, MAXVAL/2)",
        });
    }

    write_marker(w, MARKER_SOI)?;
    write_sof(w, precision as u8, height as u16, width as u16, components)?;
    let ilv = if components == 1 {
        0
    } else {
        opts.interleave_mode
    };
    match ilv {
        0 => {
            for c in 0..components as usize {
                write_sos_component(w, (c + 1) as u8, opts.near)?;
                let ctx = ContextModel::new(max_val, near, DEFAULT_RESET);
                encode_scan_t87(w, &samples[c * plane..(c + 1) * plane], wu, hu, ctx)?;
            }
        }
        1 => {
            write_sos_multi(w, components, opts.near, 1)?;
            let ctx = ContextModel::new(max_val, near, DEFAULT_RESET);
            encode_scan_ilv1(w, samples, wu, hu, ctx)?;
        }
        2 => {
            write_sos_multi(w, components, opts.near, 2)?;
            let ctx = ContextModel::new(max_val, near, DEFAULT_RESET);
            encode_scan_ilv2(w, samples, wu, hu, ctx)?;
        }
        other => {
            return Err(CodecError::InvalidParameter {
                name: "interleave_mode",
                value: i64::from(other),
                allowed: "0 (none), 1 (line), or 2 (sample)",
            });
        }
    }
    write_marker(w, MARKER_EOI)?;

    w.flush()?;
    Ok(())
}

/// Write SOI, SOF55, and SOS headers for a single-component image.
fn write_headers(
    w: &mut dyn Write,
    precision: u32,
    height: u16,
    width: u16,
    near: u8,
) -> Result<(), CodecError> {
    write_marker(w, MARKER_SOI)?;
    write_sof(w, precision as u8, height, width, 1)?;
    write_sos(w, 1, near)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// T.87 conformant scan (regular + run mode)
// ---------------------------------------------------------------------------

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn encode_regular_sample<const LOSSLESS: bool, W: Write>(
    bw: &mut BitWriter<W>,
    ctx: &mut ContextModel,
    ix: i32,
    ra: i32,
    rb: i32,
    rc: i32,
    d1: i32,
    d2: i32,
    d3: i32,
) -> Result<i32, CodecError> {
    let near = if LOSSLESS { 0 } else { ctx.near };
    let max_val = ctx.max_val;
    let range = ctx.range;
    let (q, sign) = ctx.get_context_index(d1, d2, d3);
    let e = ctx.ctx[q];
    let k = ContextModel::compute_k_from(e.n, e.a);
    let mut px = predict_med(ra, rb, rc) + sign * e.c;
    px = clamp(px, 0, max_val);

    let err_q = if LOSSLESS {
        run_mode::modulo_range((ix - px) * sign, range)
    } else {
        run_mode::modulo_range(run_mode::quantize((ix - px) * sign, near), range)
    };
    let correction = if LOSSLESS && k == 0 && 2 * e.b <= -e.n {
        -1
    } else {
        0
    };
    let mapped = run_mode::map_error(err_q, correction);
    bw.write_limited_golomb(k, mapped, ctx.limit, ctx.qbpp)?;
    if LOSSLESS {
        ctx.update_stats_lossless(q, err_q);
        Ok(ix)
    } else {
        ctx.update_stats(q, err_q);
        Ok(run_mode::fix_reconstructed(
            px + sign * err_q * (2 * near + 1),
            near,
            range,
            max_val,
        ))
    }
}

#[inline(always)]
fn encode_line_t87<const LOSSLESS: bool, W: Write>(
    bw: &mut BitWriter<W>,
    ctx: &mut ContextModel,
    src_row: &[i32],
    cur: &mut [i32],
    prev: &mut [i32],
    width: usize,
) -> Result<(), CodecError> {
    let near = if LOSSLESS { 0 } else { ctx.near };
    cur[0] = prev[1];
    prev[width + 1] = prev[width];

    let mut x = 0usize;
    let mut ra = cur[0];
    let mut rc = prev[0];
    let mut rb = prev[1];
    while x < width {
        let rd = prev[x + 2];
        let d1 = rd - rb;
        let d2 = rb - rc;
        let d3 = rc - ra;

        let in_run = if LOSSLESS {
            (d1 | d2 | d3) == 0
        } else {
            d1.abs() <= near && d2.abs() <= near && d3.abs() <= near
        };
        if in_run {
            x = run_mode::encode_run(bw, ctx, src_row, cur, prev, x, width)?;
            if x < width {
                ra = cur[x];
                rc = prev[x];
                rb = prev[x + 1];
            }
            continue;
        }

        let recon =
            encode_regular_sample::<LOSSLESS, _>(bw, ctx, src_row[x], ra, rb, rc, d1, d2, d3)?;
        cur[x + 1] = recon;
        ra = recon;
        rc = rb;
        rb = rd;
        x += 1;
    }
    Ok(())
}

/// Encode the entropy-coded scan for a single-component image (T.87 A.4-A.7).
pub(crate) fn encode_scan_t87(
    out: &mut dyn Write,
    pixels: &[u16],
    width: usize,
    height: usize,
    mut ctx: ContextModel,
) -> Result<(), CodecError> {
    let mut bw = BitWriter::new(out);
    let mut prev = vec![0i32; width + 2];
    let mut cur = vec![0i32; width + 2];
    let mut src_row = vec![0i32; width];

    if ctx.near == 0 {
        for y in 0..height {
            for (dst, &px) in src_row.iter_mut().zip(&pixels[y * width..(y + 1) * width]) {
                *dst = i32::from(px);
            }
            encode_line_t87::<true, _>(&mut bw, &mut ctx, &src_row, &mut cur, &mut prev, width)?;
            std::mem::swap(&mut prev, &mut cur);
        }
    } else {
        for y in 0..height {
            for (dst, &px) in src_row.iter_mut().zip(&pixels[y * width..(y + 1) * width]) {
                *dst = i32::from(px);
            }
            encode_line_t87::<false, _>(&mut bw, &mut ctx, &src_row, &mut cur, &mut prev, width)?;
            std::mem::swap(&mut prev, &mut cur);
        }
    }

    bw.flush()?;
    Ok(())
}

/// Encode a 3-component line-interleaved (`ILV = 1`) scan from component-planar `samples`.
fn encode_scan_ilv1(
    out: &mut dyn Write,
    samples: &[u16],
    width: usize,
    height: usize,
    mut ctx: ContextModel,
) -> Result<(), CodecError> {
    let mut bw = BitWriter::new(out);
    let plane = width * height;
    let mut prev = [
        vec![0i32; width + 2],
        vec![0i32; width + 2],
        vec![0i32; width + 2],
    ];
    let mut cur = [
        vec![0i32; width + 2],
        vec![0i32; width + 2],
        vec![0i32; width + 2],
    ];
    let mut src_row = vec![0i32; width];

    let mut run_indices = [0usize; 3];
    if ctx.near == 0 {
        for y in 0..height {
            for c in 0..3 {
                ctx.run_index = run_indices[c];
                let off = c * plane + y * width;
                for (dst, &px) in src_row.iter_mut().zip(&samples[off..off + width]) {
                    *dst = i32::from(px);
                }
                encode_line_t87::<true, _>(
                    &mut bw,
                    &mut ctx,
                    &src_row,
                    &mut cur[c],
                    &mut prev[c],
                    width,
                )?;
                run_indices[c] = ctx.run_index;
                std::mem::swap(&mut prev[c], &mut cur[c]);
            }
        }
    } else {
        for y in 0..height {
            for c in 0..3 {
                ctx.run_index = run_indices[c];
                let off = c * plane + y * width;
                for (dst, &px) in src_row.iter_mut().zip(&samples[off..off + width]) {
                    *dst = i32::from(px);
                }
                encode_line_t87::<false, _>(
                    &mut bw,
                    &mut ctx,
                    &src_row,
                    &mut cur[c],
                    &mut prev[c],
                    width,
                )?;
                run_indices[c] = ctx.run_index;
                std::mem::swap(&mut prev[c], &mut cur[c]);
            }
        }
    }

    bw.flush()?;
    Ok(())
}

/// Encode a 3-component sample-interleaved (`ILV = 2`) scan from component-planar `samples`.
fn encode_scan_ilv2(
    out: &mut dyn Write,
    samples: &[u16],
    width: usize,
    height: usize,
    ctx: ContextModel,
) -> Result<(), CodecError> {
    if ctx.near == 0 {
        encode_scan_ilv2_inner::<true>(out, samples, width, height, ctx)
    } else {
        encode_scan_ilv2_inner::<false>(out, samples, width, height, ctx)
    }
}

fn encode_scan_ilv2_inner<const LOSSLESS: bool>(
    out: &mut dyn Write,
    samples: &[u16],
    width: usize,
    height: usize,
    mut ctx: ContextModel,
) -> Result<(), CodecError> {
    let mut bw = BitWriter::new(out);
    let near = if LOSSLESS { 0 } else { ctx.near };
    let plane = width * height;
    let mut prev = vec![[0i32; 3]; width + 2];
    let mut cur = vec![[0i32; 3]; width + 2];
    let mut src_row = vec![[0i32; 3]; width];

    for y in 0..height {
        cur[0] = prev[1];
        prev[width + 1] = prev[width];
        let row_base = y * width;
        let s0 = &samples[row_base..row_base + width];
        let s1 = &samples[plane + row_base..plane + row_base + width];
        let s2 = &samples[2 * plane + row_base..2 * plane + row_base + width];
        for x in 0..width {
            src_row[x] = [i32::from(s0[x]), i32::from(s1[x]), i32::from(s2[x])];
        }

        let mut x = 0usize;
        let mut ra = cur[0];
        let mut rc = prev[0];
        let mut rb = prev[1];
        while x < width {
            let rd = prev[x + 2];
            let d1_0 = rd[0] - rb[0];
            let d2_0 = rb[0] - rc[0];
            let d3_0 = rc[0] - ra[0];
            let d1_1 = rd[1] - rb[1];
            let d2_1 = rb[1] - rc[1];
            let d3_1 = rc[1] - ra[1];
            let d1_2 = rd[2] - rb[2];
            let d2_2 = rb[2] - rc[2];
            let d3_2 = rc[2] - ra[2];

            let all_in_run = if LOSSLESS {
                (d1_0 | d2_0 | d3_0 | d1_1 | d2_1 | d3_1 | d1_2 | d2_2 | d3_2) == 0
            } else {
                d1_0.abs() <= near
                    && d2_0.abs() <= near
                    && d3_0.abs() <= near
                    && d1_1.abs() <= near
                    && d2_1.abs() <= near
                    && d3_1.abs() <= near
                    && d1_2.abs() <= near
                    && d2_2.abs() <= near
                    && d3_2.abs() <= near
            };

            if all_in_run {
                let mut run_len = 0usize;
                while x + run_len < width {
                    let s = src_row[x + run_len];
                    let matches = if LOSSLESS {
                        s == ra
                    } else {
                        (s[0] - ra[0]).abs() <= near
                            && (s[1] - ra[1]).abs() <= near
                            && (s[2] - ra[2]).abs() <= near
                    };
                    if !matches {
                        break;
                    }
                    cur[x + run_len + 1] = ra;
                    run_len += 1;
                }
                let end_of_line = x + run_len == width;
                run_mode::encode_run_pixels(&mut bw, &mut ctx, run_len, end_of_line)?;
                x += run_len;
                if end_of_line {
                    break;
                }
                let ix = src_row[x];
                let rb_i = prev[x + 1];
                let mut recon = [0i32; 3];
                for c in 0..3 {
                    recon[c] = run_mode::encode_run_interruption_ilv2(
                        &mut bw, &mut ctx, ix[c], ra[c], rb_i[c],
                    )?;
                }
                cur[x + 1] = recon;
                ctx.decrement_run_index();
                x += 1;
                if x < width {
                    ra = recon;
                    rc = prev[x];
                    rb = prev[x + 1];
                }
                continue;
            }

            let ix = src_row[x];
            let r0 = encode_regular_sample::<LOSSLESS, _>(
                &mut bw, &mut ctx, ix[0], ra[0], rb[0], rc[0], d1_0, d2_0, d3_0,
            )?;
            let r1 = encode_regular_sample::<LOSSLESS, _>(
                &mut bw, &mut ctx, ix[1], ra[1], rb[1], rc[1], d1_1, d2_1, d3_1,
            )?;
            let r2 = encode_regular_sample::<LOSSLESS, _>(
                &mut bw, &mut ctx, ix[2], ra[2], rb[2], rc[2], d1_2, d2_2, d3_2,
            )?;
            let recon = [r0, r1, r2];
            cur[x + 1] = recon;
            ra = recon;
            rc = rb;
            rb = rd;
            x += 1;
        }
        std::mem::swap(&mut prev, &mut cur);
    }

    bw.flush()?;
    Ok(())
}

fn write_sos_multi(w: &mut dyn Write, components: u8, near: u8, ilv: u8) -> Result<(), CodecError> {
    write_marker(w, MARKER_SOS)?;
    let length: u16 = 6 + 2 * u16::from(components);
    w.write_all(&length.to_be_bytes())?;
    w.write_all(&[components])?;
    for c in 1..=components {
        w.write_all(&[c, 0])?; // component ID, mapping table selector = 0
    }
    w.write_all(&[near, ilv, 0])?; // NEAR, ILV, Ah/Al = 0
    Ok(())
}

// ---------------------------------------------------------------------------
// Marker writers
// ---------------------------------------------------------------------------

fn write_marker(w: &mut dyn Write, marker: u16) -> Result<(), CodecError> {
    w.write_all(&marker.to_be_bytes())?;
    Ok(())
}

fn write_sof(
    w: &mut dyn Write,
    precision: u8,
    height: u16,
    width: u16,
    components: u8,
) -> Result<(), CodecError> {
    write_marker(w, MARKER_SOF55)?;

    // Length: 2 + 1(P) + 2(Y) + 2(X) + 1(Nf) + Nf*3
    let length: u16 = 8 + u16::from(components) * 3;
    w.write_all(&length.to_be_bytes())?;

    w.write_all(&[precision])?;
    w.write_all(&height.to_be_bytes())?;
    w.write_all(&width.to_be_bytes())?;
    w.write_all(&[components])?;

    for i in 0..components {
        w.write_all(&[i + 1])?; // Component ID
        w.write_all(&[0x11])?; // H=1, V=1
        w.write_all(&[0x00])?; // Tq=0
    }
    Ok(())
}

fn write_sos(w: &mut dyn Write, _components: u8, near: u8) -> Result<(), CodecError> {
    write_sos_component(w, 1, near)
}

fn write_sos_component(w: &mut dyn Write, component_id: u8, near: u8) -> Result<(), CodecError> {
    write_marker(w, MARKER_SOS)?;
    w.write_all(&8u16.to_be_bytes())?;
    w.write_all(&[1, component_id, 0, near, 0, 0])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Determine the effective bit depth needed for the maximum pixel value.
///
/// JPEG-LS supports 2..=16 bits per sample.  We always use at least 8.
fn effective_precision(max_pixel: u16) -> u32 {
    if max_pixel == 0 {
        return 8;
    }
    let bits_needed = 16 - max_pixel.leading_zeros(); // ceil(log2(max+1))
    bits_needed.clamp(8, 16)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_precision_8bit() {
        assert_eq!(effective_precision(0), 8);
        assert_eq!(effective_precision(1), 8);
        assert_eq!(effective_precision(255), 8);
    }

    #[test]
    fn effective_precision_more_than_8() {
        assert_eq!(effective_precision(256), 9);
        assert_eq!(effective_precision(1023), 10);
        assert_eq!(effective_precision(4095), 12);
        assert_eq!(effective_precision(65535), 16);
    }

    #[test]
    fn encode_rejects_empty_image() {
        let pixels: Vec<u16> = vec![];
        let mut buf = Vec::new();
        assert!(encode(&pixels, 0, 0, &mut buf).is_err());
    }

    #[test]
    fn encode_produces_soi_and_eoi() {
        let pixels = vec![42u16; 4];
        let mut buf = Vec::new();
        encode(&pixels, 2, 2, &mut buf).unwrap();

        assert_eq!(buf[0], 0xFF);
        assert_eq!(buf[1], 0xD8);
        let n = buf.len();
        assert_eq!(buf[n - 2], 0xFF);
        assert_eq!(buf[n - 1], 0xD9);
    }

    #[test]
    fn encode_uniform_image() {
        let pixels = vec![0u16; 16];
        let mut buf = Vec::new();
        encode(&pixels, 4, 4, &mut buf).unwrap();
        assert!(!buf.is_empty());
    }

    #[test]
    fn encode_rejects_oversize_dimension() {
        let pixels = vec![0u16; 4];
        let mut buf = Vec::new();
        assert!(matches!(
            encode_with_options(&pixels, 65536, 1, &EncodeOptions::default(), &mut buf),
            Err(CodecError::InvalidParameter { name: "width", .. })
        ));
    }
}
