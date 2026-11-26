//! Mutation / robustness tests.
//!
//! Property under test: no matter how the encoded stream is corrupted, the
//! decoder must **never panic** and must **never return a wrong-sized `Ok`**.
//! Every decode of a mutated stream is therefore either an `Err` (a well-typed
//! rejection) or an `Ok` holding exactly `W * H` samples with matching
//! dimensions. A panic fails the test naturally (no `catch_unwind`).
//!
//! The base stream is produced by the crate's own encoder using the default
//! (ITU-T T.87 conformant) profile.

use dcmnorm_jpegls::{decode, encode, CodecError};

const W: u32 = 16;
const H: u32 = 16;

fn sample_image() -> Vec<u16> {
    (0..H)
        .flat_map(|y| (0..W).map(move |x| ((x * 7 + y * 13) & 0xFF) as u16))
        .collect()
}

fn base_stream() -> Vec<u8> {
    let mut buf = Vec::new();
    encode(&sample_image(), W, H, &mut buf).expect("encode base image");
    buf
}

fn assert_wellformed(res: Result<(Vec<u16>, u32, u32), CodecError>) {
    if let Ok((px, dw, dh)) = res {
        assert_eq!((dw, dh), (W, H), "Ok returned wrong dimensions");
        assert_eq!(px.len(), (W * H) as usize, "Ok returned wrong sample count");
    }
}

/// (a) Every truncation prefix decodes to `Err` or a correctly-sized `Ok`.
#[test]
fn truncation_prefixes_never_panic() {
    let base = base_stream();
    for len in 0..base.len() {
        assert_wellformed(decode(&base[..len], W, H));
    }
}

/// (b) Every single-bit flip in the first 64 and last 16 bytes is safe.
#[test]
fn bit_flips_never_panic() {
    let base = base_stream();
    let len = base.len();

    let head = 64.min(len);
    for byte in 0..head {
        for bit in 0..8u8 {
            let mut m = base.clone();
            m[byte] ^= 1 << bit;
            assert_wellformed(decode(&m, W, H));
        }
    }

    let tail_start = len.saturating_sub(16);
    for byte in tail_start..len {
        for bit in 0..8u8 {
            let mut m = base.clone();
            m[byte] ^= 1 << bit;
            assert_wellformed(decode(&m, W, H));
        }
    }
}

/// (c) The trailing EOI marker is mandatory: chopping it must yield `Err`.
#[test]
fn chopped_trailing_marker_is_err() {
    let base = base_stream();
    assert_eq!(
        &base[base.len() - 2..],
        &[0xFF, 0xD9],
        "expected the stream to end with the EOI marker (FF D9)"
    );
    let chopped = &base[..base.len() - 2];
    assert!(
        decode(chopped, W, H).is_err(),
        "decode must reject a stream missing its trailing EOI marker"
    );
}
