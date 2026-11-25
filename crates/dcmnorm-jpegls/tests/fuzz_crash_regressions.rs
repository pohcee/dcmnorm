//! Inputs found by the `decode` fuzz target, pinned so they stay fixed.
//!
//! Each fixture is a raw libFuzzer input: the first 4 bytes are two
//! little-endian `u16` dimensions (clamped to `1..=512`), the rest is the
//! JPEG-LS body, exactly as `fuzz/fuzz_targets/decode.rs` consumes it.
//! Test builds have overflow checks on, so a regression panics here.

use dcmnorm_jpegls::{decode, CodecError};

/// Weekly fuzz-smoke run 40324461717 (2026-10-26): `i32` overflow in the
/// run-context A accumulator (`context.rs` `update_run_stats`).
const CRASH_0AC3D3B: &[u8] =
    include_bytes!("fixtures/fuzz/decode-crash-0ac3d3b29c36ae16b19af9329dd3bc2ffa0b3484");

fn split(data: &[u8]) -> (&[u8], u32, u32) {
    let dim = |lo: u8, hi: u8| u32::from(u16::from_le_bytes([lo, hi])).clamp(1, 512);
    (&data[4..], dim(data[0], data[1]), dim(data[2], data[3]))
}

#[test]
fn crash_0ac3d3b_is_rejected() {
    let (body, w, h) = split(CRASH_0AC3D3B);
    // Its SOF55 declares Ls = 5, too short for any frame header.
    assert!(matches!(
        decode(body, w, h),
        Err(CodecError::InvalidData(_))
    ));
}

#[test]
fn crash_0ac3d3b_with_valid_sof_is_rejected_without_overflow() {
    // Repair the SOF55 so the scan is actually decoded: this reaches the
    // corrupt Golomb codes that overflowed A before the Errval bound.
    let mut data = CRASH_0AC3D3B.to_vec();
    assert_eq!(&data[6..10], &[0xFF, 0xF7, 0x00, 0x05]);
    data[9] = 11; // Ls = 8 + 3*Nf
    data.splice(16..16, [1u8, 0x11, 0x00]); // Ci, 1:1 sampling, Tq 0
                                            // Repair the minimized SOS length and selector as well, so header
                                            // validation cannot mask the entropy-code regression.
    let sos = data.windows(2).position(|b| b == [0xff, 0xda]).unwrap();
    data[sos + 2..sos + 4].copy_from_slice(&8u16.to_be_bytes());
    data[sos + 5] = 1;
    let (body, w, h) = split(&data);
    let result = decode(body, w, h);
    assert!(
        matches!(result, Err(CodecError::Truncated { .. })),
        "{result:?}"
    );
}
