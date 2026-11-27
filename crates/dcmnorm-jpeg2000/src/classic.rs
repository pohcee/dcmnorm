//! Classic JPEG 2000 Part 1 (`.90`/`.91`) lossless and lossy encoding via pure-Rust `j2k`.

use j2k::{
    encode_j2k_lossless, encode_j2k_lossy, J2kBlockCodingMode, J2kEncodeValidation,
    J2kLosslessEncodeOptions, J2kLosslessSamples, J2kLossyEncodeOptions, J2kLossySamples,
    J2kProgressionOrder, J2kRateTarget, ReversibleTransform,
};

/// Encodes one frame of native-endian, interleaved pixel samples to a classic JPEG 2000 Part 1
/// codestream (raw `.j2k`, matching DICOM `.90` lossless / `.91` lossy fragments).
#[allow(clippy::too_many_arguments)]
pub fn encode(
    pixel_data: &[u8],
    rows: u32,
    cols: u32,
    samples_per_pixel: u32,
    bits_stored: u32,
    is_signed: bool,
    lossless: bool,
    lossy_compression_ratio: f32,
) -> Result<Vec<u8>, String> {
    if samples_per_pixel != 1 && samples_per_pixel != 3 {
        return Err(format!(
            "JPEG2000 encoding only supports 1 or 3 samples per pixel, got {samples_per_pixel}"
        ));
    }
    if !(1..=16).contains(&bits_stored) {
        return Err(format!(
            "unsupported BitsStored {bits_stored} for JPEG2000 encoding (expected 1..=16)"
        ));
    }
    let bytes_per_sample = if bits_stored > 8 { 2usize } else { 1usize };
    let expected_len =
        rows as usize * cols as usize * samples_per_pixel as usize * bytes_per_sample;
    if pixel_data.len() != expected_len {
        return Err(format!(
            "pixel data length {} does not match expected {expected_len} for \
             {rows}x{cols}x{samples_per_pixel} at {bytes_per_sample} byte(s)/sample",
            pixel_data.len()
        ));
    }

    if lossless {
        let samples = J2kLosslessSamples {
            data: pixel_data,
            width: cols,
            height: rows,
            components: samples_per_pixel as u16,
            bit_depth: bits_stored as u8,
            signed: is_signed,
        };
        let options = J2kLosslessEncodeOptions::default()
            .with_block_coding_mode(J2kBlockCodingMode::Classic)
            .with_progression(J2kProgressionOrder::Lrcp)
            .with_reversible_transform(if samples_per_pixel == 3 {
                ReversibleTransform::Rct53
            } else {
                ReversibleTransform::None53
            })
            .with_max_decomposition_levels(Some(5))
            .with_validation(J2kEncodeValidation::External);

        encode_j2k_lossless(samples, &options)
            .map(|encoded| encoded.codestream)
            .map_err(|e| e.to_string())
    } else {
        let ratio = f64::from(lossy_compression_ratio.max(1.0));
        let target_bytes = ((expected_len as f64) / ratio).round().max(1.0) as u64;
        let samples = J2kLossySamples {
            data: pixel_data,
            width: cols,
            height: rows,
            components: samples_per_pixel as u16,
            bit_depth: bits_stored as u8,
            signed: is_signed,
        };
        let options = J2kLossyEncodeOptions::default()
            .with_block_coding_mode(J2kBlockCodingMode::Classic)
            .with_progression(J2kProgressionOrder::Lrcp)
            .with_max_decomposition_levels(Some(5))
            .with_rate_target(Some(J2kRateTarget::Bytes(target_bytes)))
            .with_validation(J2kEncodeValidation::External);

        encode_j2k_lossy(samples, &options)
            .map(|encoded| encoded.codestream)
            .map_err(|e| e.to_string())
    }
}
