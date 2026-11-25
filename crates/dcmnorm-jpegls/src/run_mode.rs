//! T.87 run mode and shared regular-mode sample arithmetic.
//!
//! Run mode is entered from the regular scan loop whenever all three local
//! gradients are within `NEAR` (T.87 A.7). This module holds:
//! - the run-length coding (A.7.1) and run-interruption coding (A.7.2), wired
//!   into both scan loops via [`encode_run`] / [`decode_run`];
//! - the shared error mapping (A.5.2), near-lossless quantization/modulo
//!   (A.4.4/A.4.5), and reconstruction (A.4.6) helpers used by regular mode
//!   too, so the encoder and decoder cannot drift.
//!
//! # Line buffers
//!
//! `cur` and `prev` are `width + 2` element `i32` buffers holding reconstructed
//! samples with a one-slot offset: logical sample `x` lives at index `x + 1`.
//! Logical index `-1` is slot `0` (the left border) and logical index `width`
//! is slot `width + 1` (the right border). This realizes the T.87 A.2.1 edge
//! rules: `Ra(0,y) = Rb`, `Rc(0,y)` retains the prior line's seed, and
//! `Rd(width-1,y) = Rb`.

use std::io::Write;

use crate::bitstream::{BitReader, BitWriter};
use crate::context::ContextModel;
use crate::error::CodecError;
use crate::predictor::clamp;

/// Run-interruption context base index (T.87: contexts 365/366).
const RUN_CTX_BASE: usize = 365;

// ---------------------------------------------------------------------------
// Shared sample arithmetic (regular + run mode)
// ---------------------------------------------------------------------------

/// Map a signed error to a non-negative code (T.87 A.5.2).
///
/// `correction` is `0` normally or `-1` (all-ones) when the `k == 0 && NEAR == 0`
/// bias-flip case applies; the XOR realizes both the flip and its inverse.
#[inline]
pub(crate) fn map_error(err_val: i32, correction: i32) -> u32 {
    let v = correction ^ err_val;
    if v >= 0 {
        (2 * v) as u32
    } else {
        (-2 * v - 1) as u32
    }
}

/// Inverse of [`map_error`] (T.87 A.5.2).
#[cfg(test)]
#[inline]
pub(crate) fn unmap_error(mapped: u32, correction: i32) -> i32 {
    let m = mapped as i32;
    let v = (m >> 1) ^ -(m & 1);
    correction ^ v
}

/// Near-lossless error quantization (T.87 A.4.4). Identity when `near == 0`.
#[inline]
pub(crate) fn quantize(e: i32, near: i32) -> i32 {
    if near == 0 {
        return e;
    }
    if e >= 0 {
        (e + near) / (2 * near + 1)
    } else {
        -((near - e) / (2 * near + 1))
    }
}

/// Modulo reduction of the (quantized) error into the coding range (A.4.5).
#[inline]
pub(crate) fn modulo_range(mut e: i32, range: i32) -> i32 {
    if e < 0 {
        e += range;
    }
    if e >= (range + 1) / 2 {
        e -= range;
    }
    e
}

/// Reject a decoded error value no conformant encoder can produce.
///
/// The encoder reduces every error modulo RANGE (A.4.5), so `|Errval| <=
/// RANGE/2`. A larger value can only come from a corrupt Golomb code; left
/// unchecked it inflates the context's A accumulator, which raises k, which
/// lets the next code decode larger still, until A overflows `i32`. Within
/// the bound A stays below `RESET * RANGE/2 <= 65535 * 32768 < i32::MAX`.
#[cold]
#[inline(never)]
fn errval_out_of_range(err: i32, range: i32) -> CodecError {
    CodecError::InvalidData(format!(
        "decoded error {err} outside +/-RANGE/2 (RANGE = {range})"
    ))
}

#[inline(always)]
pub(crate) fn check_errval(err: i32, range: i32) -> Result<(), CodecError> {
    if err.unsigned_abs() > (range / 2) as u32 {
        return Err(errval_out_of_range(err, range));
    }
    Ok(())
}

/// Reconstruct a sample value, folding it back into `[0, max_val]` (A.4.6).
#[inline]
pub(crate) fn fix_reconstructed(value: i32, near: i32, range: i32, max_val: i32) -> i32 {
    let mut v = value;
    if v < -near {
        v += range * (2 * near + 1);
    } else if v > max_val + near {
        v -= range * (2 * near + 1);
    }
    clamp(v, 0, max_val)
}

// ---------------------------------------------------------------------------
// Run-length coding (T.87 A.7.1)
// ---------------------------------------------------------------------------

/// Emit the run-length code for a run of `run_length` samples (A.7.1.2).
pub(crate) fn encode_run_pixels<W: Write>(
    bw: &mut BitWriter<W>,
    ctx: &mut ContextModel,
    mut run_length: usize,
    end_of_line: bool,
) -> Result<(), CodecError> {
    while run_length >= (1usize << ctx.j[ctx.run_index]) {
        bw.write_bit(1)?;
        run_length -= 1usize << ctx.j[ctx.run_index];
        ctx.increment_run_index();
    }
    if end_of_line {
        // A.15 end-of-line rule: a residual run emits a single 1-bit and does
        // NOT advance RUNindex.
        if run_length != 0 {
            bw.write_bit(1)?;
        }
    } else {
        bw.write_bit(0)?;
        let jbits = ctx.j[ctx.run_index];
        if jbits > 0 {
            bw.write_bits(run_length as u32, jbits)?;
        }
    }
    Ok(())
}

/// Decode a run-length code, returning the number of run samples (A.7.1.2).
pub(crate) fn decode_run_pixels(
    br: &mut BitReader<'_>,
    ctx: &mut ContextModel,
    pixel_count: usize,
) -> Result<usize, CodecError> {
    let mut index = 0usize;
    while br.read_bit()? == 1 {
        let block = 1usize << ctx.j[ctx.run_index];
        let count = block.min(pixel_count - index);
        index += count;
        if count == block {
            ctx.increment_run_index();
        }
        if index == pixel_count {
            return Ok(index);
        }
    }
    let jbits = ctx.j[ctx.run_index];
    if jbits > 0 {
        index += br.read_bits(jbits)? as usize;
    }
    if index > pixel_count {
        return Err(CodecError::InvalidData(
            "run length exceeds line width".into(),
        ));
    }
    Ok(index)
}

// ---------------------------------------------------------------------------
// Run-interruption coding (T.87 A.7.2, code segments A.19-A.23)
// ---------------------------------------------------------------------------

/// Select the run-interruption parameters (T.87 A.7.2): `(RItype, Px, sign)`.
#[inline]
fn interruption_params(ra: i32, rb: i32, near: i32) -> (usize, i32, i32) {
    if (ra - rb).abs() <= near {
        (1, ra, 1)
    } else {
        (0, rb, if rb > ra { 1 } else { -1 })
    }
}

/// Run-interruption error-mapping predicate `map` (T.87 code segment A.21).
#[inline]
fn run_map(err: i32, k: i32, nn: i32, n: i32) -> bool {
    (k == 0 && err > 0 && 2 * nn < n) || (err < 0 && 2 * nn >= n) || (err < 0 && k != 0)
}

/// Encode one run-interruption sample (A.7.2); returns the reconstructed value.
pub(crate) fn encode_run_interruption<W: Write>(
    bw: &mut BitWriter<W>,
    ctx: &mut ContextModel,
    ix: i32,
    ra: i32,
    rb: i32,
) -> Result<i32, CodecError> {
    let near = ctx.near;
    let (ri_type, px, s) = interruption_params(ra, rb, near);
    encode_run_interruption_with(bw, ctx, ix, ri_type, px, s)
}

/// Encode one sample-interleaved (`ILV=2`) run-interruption component (T.87 A.7.3).
/// Always uses `RItype = 0`, `Px = Rb`, and `sign = if Rb >= Ra { 1 } else { -1 }`.
pub(crate) fn encode_run_interruption_ilv2<W: Write>(
    bw: &mut BitWriter<W>,
    ctx: &mut ContextModel,
    ix: i32,
    ra: i32,
    rb: i32,
) -> Result<i32, CodecError> {
    let s = if rb >= ra { 1 } else { -1 };
    encode_run_interruption_with(bw, ctx, ix, 0, rb, s)
}

#[inline(always)]
fn encode_run_interruption_with<W: Write>(
    bw: &mut BitWriter<W>,
    ctx: &mut ContextModel,
    ix: i32,
    ri_type: usize,
    px: i32,
    s: i32,
) -> Result<i32, CodecError> {
    let near = ctx.near;
    let ctx_idx = RUN_CTX_BASE + ri_type;

    let err_q = modulo_range(quantize((ix - px) * s, near), ctx.range);

    let k = ctx.compute_k_run(ri_type);
    let map = run_map(err_q, k, ctx.nn[ri_type], ctx.ctx[ctx_idx].n);
    let e_mapped = (2 * err_q.abs() - ri_type as i32 - i32::from(map)) as u32;

    let glimit = ctx.limit - ctx.j[ctx.run_index] - 1;
    bw.write_limited_golomb(k, e_mapped, glimit, ctx.qbpp)?;
    ctx.update_run_stats(ri_type, e_mapped, err_q < 0);

    Ok(fix_reconstructed(
        px + s * err_q * (2 * near + 1),
        near,
        ctx.range,
        ctx.max_val,
    ))
}

/// Decode one run-interruption sample (A.7.2); returns the reconstructed value.
pub(crate) fn decode_run_interruption(
    br: &mut BitReader<'_>,
    ctx: &mut ContextModel,
    ra: i32,
    rb: i32,
) -> Result<i32, CodecError> {
    let near = ctx.near;
    let (ri_type, px, s) = interruption_params(ra, rb, near);
    decode_run_interruption_with(br, ctx, ri_type, px, s)
}

/// Decode one sample-interleaved (`ILV=2`) run-interruption component (T.87 A.7.3).
pub(crate) fn decode_run_interruption_ilv2(
    br: &mut BitReader<'_>,
    ctx: &mut ContextModel,
    ra: i32,
    rb: i32,
) -> Result<i32, CodecError> {
    let s = if rb >= ra { 1 } else { -1 };
    decode_run_interruption_with(br, ctx, 0, rb, s)
}

#[inline(always)]
fn decode_run_interruption_with(
    br: &mut BitReader<'_>,
    ctx: &mut ContextModel,
    ri_type: usize,
    px: i32,
    s: i32,
) -> Result<i32, CodecError> {
    let near = ctx.near;
    let ctx_idx = RUN_CTX_BASE + ri_type;

    let k = ctx.compute_k_run(ri_type);
    let glimit = ctx.limit - ctx.j[ctx.run_index] - 1;
    let e_mapped = br.read_limited_golomb(k, glimit, ctx.qbpp)?;

    let temp = e_mapped as i32 + ri_type as i32;
    let abs_e = (temp + (temp & 1)) >> 1;
    let map_positive = k == 0 && 2 * ctx.nn[ri_type] < ctx.ctx[ctx_idx].n;
    let err_q = if (temp & 1) == i32::from(map_positive) {
        abs_e
    } else {
        -abs_e
    };

    check_errval(err_q, ctx.range)?;
    ctx.update_run_stats(ri_type, e_mapped, err_q < 0);

    Ok(fix_reconstructed(
        px + s * err_q * (2 * near + 1),
        near,
        ctx.range,
        ctx.max_val,
    ))
}

// ---------------------------------------------------------------------------
// Run mode entry points (called from the scan loops)
// ---------------------------------------------------------------------------

/// Encode a run starting at logical sample `start`; returns the next sample.
///
/// `src` is the source row (length `width`); `cur`/`prev` are the offset-1
/// reconstructed line buffers described in the module docs.
pub(crate) fn encode_run<W: Write>(
    bw: &mut BitWriter<W>,
    ctx: &mut ContextModel,
    src: &[u16],
    cur: &mut [u16],
    prev: &[u16],
    start: usize,
    width: usize,
) -> Result<usize, CodecError> {
    let near = ctx.near;
    let ra_u16 = cur[start]; // logical sample start-1 (border at start==0)
    let ra = i32::from(ra_u16);

    let mut run_length = 0usize;
    while start + run_length < width && (i32::from(src[start + run_length]) - ra).abs() <= near {
        cur[start + run_length + 1] = ra_u16;
        run_length += 1;
    }
    let end = start + run_length;
    let end_of_line = end == width;
    encode_run_pixels(bw, ctx, run_length, end_of_line)?;
    if end_of_line {
        return Ok(end);
    }

    let rb = i32::from(prev[end + 1]);
    let recon = encode_run_interruption(bw, ctx, i32::from(src[end]), ra, rb)?;
    cur[end + 1] = recon as u16;
    ctx.decrement_run_index();
    Ok(end + 1)
}

/// Decode a run starting at logical sample `start`; returns the next sample.
pub(crate) fn decode_run(
    br: &mut BitReader<'_>,
    ctx: &mut ContextModel,
    cur: &mut [u16],
    prev: &[u16],
    start: usize,
    width: usize,
) -> Result<usize, CodecError> {
    let ra_u16 = cur[start];
    let ra = i32::from(ra_u16);

    let run_length = decode_run_pixels(br, ctx, width - start)?;
    cur[start + 1..start + 1 + run_length].fill(ra_u16);
    let end = start + run_length;
    if end == width {
        return Ok(end);
    }

    let rb = i32::from(prev[end + 1]);
    let recon = decode_run_interruption(br, ctx, ra, rb)?;
    cur[end + 1] = recon as u16;
    ctx.decrement_run_index();
    Ok(end + 1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_unmap_roundtrip_all_corrections() {
        for correction in [0i32, -1] {
            for err in -300..=300 {
                let mapped = map_error(err, correction);
                assert_eq!(unmap_error(mapped, correction), err, "corr={correction}");
            }
        }
    }

    #[test]
    fn quantize_near0_identity() {
        for e in -50..=50 {
            assert_eq!(quantize(e, 0), e);
        }
    }

    #[test]
    fn quantize_near_symmetric() {
        // near=2 -> divisor 5; |e|<=2 -> 0, 3..=7 -> 1, etc.
        assert_eq!(quantize(0, 2), 0);
        assert_eq!(quantize(2, 2), 0);
        assert_eq!(quantize(-2, 2), 0);
        assert_eq!(quantize(3, 2), 1);
        assert_eq!(quantize(-3, 2), -1);
        assert_eq!(quantize(7, 2), 1);
        assert_eq!(quantize(8, 2), 2);
    }

    #[test]
    fn fix_reconstructed_lossless_wraps() {
        // near=0, max_val=255, range=256.
        assert_eq!(fix_reconstructed(-1, 0, 256, 255), 255);
        assert_eq!(fix_reconstructed(256, 0, 256, 255), 0);
        assert_eq!(fix_reconstructed(128, 0, 256, 255), 128);
    }
}
