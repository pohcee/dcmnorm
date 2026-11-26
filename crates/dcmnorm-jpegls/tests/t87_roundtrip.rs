//! T.87 conformant encode/decode round-trip matrix and run-mode specifics.
//!
//! Exercises the ITU-T T.87 path across dimensions,
//! near-lossless bounds, precisions, and pixel patterns (including ones that
//! force run mode, the end-of-line rule, and run interruptions).

use dcmnorm_jpegls::{decode, encode_with_options, EncodeOptions};

fn enc_opts(near: u8, precision: u8) -> EncodeOptions {
    let mut o = EncodeOptions::default();
    o.near = near;
    o.precision = Some(precision);
    o
}

fn encode(pixels: &[u16], w: u32, h: u32, near: u8, precision: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_with_options(pixels, w, h, &enc_opts(near, precision), &mut buf)
        .expect("encode should succeed");
    buf
}

/// Assert the near-lossless property: exact when near==0, else |Δ| <= near.
fn assert_within_near(orig: &[u16], decoded: &[u16], near: u8, label: &str) {
    assert_eq!(orig.len(), decoded.len(), "{label}: length");
    for (i, (&o, &d)) in orig.iter().zip(decoded).enumerate() {
        let diff = (i32::from(o) - i32::from(d)).unsigned_abs();
        assert!(
            diff <= u32::from(near),
            "{label}: pixel {i} orig={o} decoded={d} diff={diff} > near={near}"
        );
    }
}

// --- pattern generators (all values clamped to maxval) ---------------------

fn constant(w: usize, h: usize, maxval: u16) -> Vec<u16> {
    vec![maxval / 2; w * h]
}

fn gradient(w: usize, h: usize, maxval: u16) -> Vec<u16> {
    (0..h)
        .flat_map(|y| {
            (0..w).map(move |x| {
                let v = ((x + y) as u64 * u64::from(maxval)) / ((w + h).max(1) as u64);
                v as u16
            })
        })
        .collect()
}

fn checkerboard(w: usize, h: usize, maxval: u16) -> Vec<u16> {
    let hi = maxval;
    let lo = maxval / 4;
    (0..h)
        .flat_map(|y| (0..w).map(move |x| if (x + y) % 2 == 0 { hi } else { lo }))
        .collect()
}

fn lcg(seed: u64, n: usize, maxval: u16) -> Vec<u16> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (((s >> 33) as u32) % (u32::from(maxval) + 1)) as u16
        })
        .collect()
}

/// Random with flat runs interspersed — forces run mode AND interruptions.
fn random_flat(seed: u64, w: usize, h: usize, maxval: u16) -> Vec<u16> {
    let mut base = lcg(seed, w * h, maxval);
    let flat = maxval / 3;
    for y in 0..h {
        // Make the middle third of each row a flat run.
        for x in (w / 3)..(2 * w / 3) {
            base[y * w + x] = flat;
        }
    }
    base
}

#[test]
fn roundtrip_matrix() {
    let dims: &[(u32, u32)] = &[(1, 1), (1, 16), (16, 1), (7, 5), (16, 16), (33, 9)];
    let nears: &[u8] = &[0, 1, 3];
    let precisions: &[u8] = &[8, 12, 16];

    for &(w, h) in dims {
        for &precision in precisions {
            let maxval: u16 = ((1u32 << precision) - 1) as u16;
            let (wu, hu) = (w as usize, h as usize);
            let patterns: Vec<(&str, Vec<u16>)> = vec![
                ("constant", constant(wu, hu, maxval)),
                ("gradient", gradient(wu, hu, maxval)),
                ("checkerboard", checkerboard(wu, hu, maxval)),
                (
                    "random",
                    lcg(0x1234_5678 ^ u64::from(w * 131 + h), wu * hu, maxval),
                ),
                ("random_flat", random_flat(0xC0FFEE, wu, hu, maxval)),
            ];
            for &near in nears {
                for (pname, pixels) in &patterns {
                    let label = format!("{pname} {w}x{h} p{precision} near{near}");
                    let bytes = encode(pixels, w, h, near, precision);
                    let (decoded, dw, dh) = decode(&bytes, w, h)
                        .unwrap_or_else(|e| panic!("{label}: decode failed: {e}"));
                    assert_eq!((dw, dh), (w, h), "{label}: dims");
                    assert_within_near(pixels, &decoded, near, &label);
                }
            }
        }
    }
}

#[test]
fn run_spanning_multiple_lines_is_exact() {
    // A fully constant multi-line image: rows after the first are whole-line
    // runs, exercising the EOL rule and RUNindex carry-over across lines.
    let (w, h) = (20u32, 10u32);
    let pixels = vec![1234u16; (w * h) as usize];
    let bytes = encode(&pixels, w, h, 0, 12);
    let (decoded, _, _) = decode(&bytes, w, h).unwrap();
    assert_eq!(decoded, pixels, "constant multi-line run must be exact");
}

#[test]
fn run_interruption_at_first_column() {
    // Column 0 differs on some rows -> interruption at the first sample of a
    // line (RItype exercised at the left border).
    let (w, h) = (8u32, 6u32);
    let mut pixels = vec![100u16; (w * h) as usize];
    for y in 0..h as usize {
        pixels[y * w as usize] = if y % 2 == 0 { 100 } else { 160 };
    }
    let bytes = encode(&pixels, w, h, 0, 8);
    let (decoded, _, _) = decode(&bytes, w, h).unwrap();
    assert_eq!(decoded, pixels);
}

#[test]
fn run_ending_exactly_at_eol() {
    // Width chosen so a constant row is exactly one run to the line end.
    for w in [1u32, 2, 3, 7, 16, 31] {
        let h = 4u32;
        let pixels = vec![50u16; (w * h) as usize];
        let bytes = encode(&pixels, w, h, 0, 8);
        let (decoded, _, _) = decode(&bytes, w, h).unwrap();
        assert_eq!(decoded, pixels, "w={w}");
    }
}

#[test]
fn near_lossless_various_bounds() {
    let (w, h) = (24u32, 16u32);
    let pixels = lcg(0xABCDEF, (w * h) as usize, 4095);
    for near in [1u8, 2, 5, 10] {
        let bytes = encode(&pixels, w, h, near, 12);
        let (decoded, _, _) = decode(&bytes, w, h).unwrap();
        assert_within_near(&pixels, &decoded, near, &format!("near{near}"));
    }
}

#[test]
fn encode_rejects_invalid_precision() {
    let pixels = vec![0u16; 4];
    let mut buf = Vec::new();
    for bad in [0u8, 1, 17] {
        let mut o = EncodeOptions::default();
        o.precision = Some(bad);
        assert!(
            encode_with_options(&pixels, 2, 2, &o, &mut buf).is_err(),
            "precision {bad}"
        );
    }
}

#[test]
fn encode_rejects_near_over_half_maxval() {
    // precision 8 -> maxval 255 -> near_max 127; 200 is rejected.
    let pixels = vec![10u16; 4];
    let mut buf = Vec::new();
    let mut o = EncodeOptions::default();
    o.near = 200;
    o.precision = Some(8);
    assert!(encode_with_options(&pixels, 2, 2, &o, &mut buf).is_err());
}

#[test]
fn deterministic_roundtrip_within_near_matrix() {
    let precisions = [8u8, 10, 12, 16];
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next_u64 = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    };
    for case in 0..200u64 {
        let w = (next_u64() % 40 + 1) as u32;
        let h = (next_u64() % 25 + 1) as u32;
        let precision = precisions[(next_u64() as usize) % precisions.len()];
        let near = (next_u64() % 5) as u8;
        let maxval: u16 = ((1u32 << precision) - 1) as u16;
        let pixels = lcg(next_u64() ^ case, (w * h) as usize, maxval);
        let bytes = encode(&pixels, w, h, near, precision);
        let (decoded, dw, dh) = decode(&bytes, w, h).expect("decode");
        assert_eq!((dw, dh), (w, h));
        for (&o, &d) in pixels.iter().zip(&decoded) {
            assert!((i32::from(o) - i32::from(d)).unsigned_abs() <= u32::from(near));
        }
    }
}

#[test]
fn deterministic_decode_arbitrary_and_bitflip_no_panic() {
    let mut state = 0xdead_beef_cafe_babeu64;
    let mut next_u64 = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    };
    for _ in 0..200 {
        let len = (next_u64() % 512) as usize;
        let data: Vec<u8> = (0..len).map(|_| (next_u64() >> 32) as u8).collect();
        let _ = decode(&data, 8, 8);
    }
    for case in 0..200u64 {
        let pixels = lcg(next_u64() ^ case, 64, 255);
        let mut bytes = encode(&pixels, 8, 8, 0, 8);
        let flip = (next_u64() as usize) % bytes.len();
        bytes[flip] ^= 0x40;
        let _ = decode(&bytes, 8, 8);
    }
}

#[test]
fn encode_rejects_zero_and_oversize_dims() {
    let pixels = vec![0u16; 4];
    let mut buf = Vec::new();
    assert!(encode_with_options(&pixels, 0, 2, &EncodeOptions::default(), &mut buf).is_err());
    assert!(encode_with_options(&pixels, 2, 0, &EncodeOptions::default(), &mut buf).is_err());
    assert!(encode_with_options(&[], 65536, 1, &EncodeOptions::default(), &mut buf).is_err());
}
