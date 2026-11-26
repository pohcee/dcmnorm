#![forbid(unsafe_code)]
//! Pure-Rust ITU-T T.87 / ISO 14495-1 JPEG-LS lossless and near-lossless codec.
//!
//! Derived from `pure_jpegls` v2.1.0 by jpfielding
//! (<https://github.com/jpfielding/dicos.rs/tree/main/crates/jpegls>, MIT OR Apache-2.0),
//! extended with line-interleaved (`ILV=1`) and sample-interleaved (`ILV=2`) multi-component
//! scans, direct DICOM byte-buffer helpers, and hot-path optimizations.
//!
//! # Conformance scope
//!
//! Supports ITU-T T.87 single-component and 3-component images across all three
//! interleave modes (`ILV = 0` planar, `ILV = 1` line-interleaved, `ILV = 2`
//! sample-interleaved), sample precision `2..=16`, LSE ID=1 presets, run mode,
//! and lossless (`NEAR = 0`) / near-lossless (`NEAR > 0`) coding.

pub mod error;

mod bitstream;
mod context;
mod decode;
mod encode;
mod predictor;
mod run_mode;

pub use decode::{
    decode, decode_image, decode_image_with_options, decode_to_bytes, decode_to_bytes_with_options,
    decode_to_interleaved_bytes_with_options, read_frame_info, DecodeImageOptions, DecodedImage,
    FrameInfo,
};
pub use encode::{encode, encode_bytes, encode_image, encode_with_options};
pub use error::CodecError;

/// DICOM Transfer Syntax UID for JPEG-LS Lossless.
pub const TRANSFER_SYNTAX_UID: &str = "1.2.840.10008.1.2.4.80";

/// Options controlling [`encode_with_options`] and [`encode_image`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EncodeOptions {
    /// Near-lossless error bound (`0` = lossless). Bounded by `min(255, MAXVAL/2)`.
    pub near: u8,
    /// Explicit sample precision in bits (`2..=16`). `None` derives it from the data (at least 8).
    pub precision: Option<u8>,
    /// Multi-component interleave mode: `0` = none (planar), `1` = line, `2` = sample.
    pub interleave_mode: u8,
}
