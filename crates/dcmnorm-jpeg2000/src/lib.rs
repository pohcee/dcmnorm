//! Pure-Rust JPEG 2000 (Part 1) and HTJ2K (Part 15) codecs for dcmnorm, backed by `j2k`.
//!
//! - [`htj2k`]: High-Throughput JPEG 2000 (Part 15, DICOM `.201`-`.203`) decode and lossless
//!   encode.
//! - [`openjpeg`]: Classic JPEG 2000 (Part 1) decode (kept under the historical module name for
//!   downstream compatibility; backed by pure-Rust `j2k`).
//! - [`classic`]: Classic JPEG 2000 (Part 1, DICOM `.90`/`.91`) lossless and lossy encode.
//! - [`threads`]: The shared threading policy.
//! - [`codestream`]: Locating the codestream in a fragment (raw or JP2-wrapped) and reading its
//!   SIZ/COD main-header parameters without decoding.

pub mod classic;
pub mod codestream;
pub mod htj2k;
pub mod openjpeg;
pub mod threads;

/// One decoded component plane: `width * height` samples in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedComponent {
    pub width: u32,
    pub height: u32,
    pub precision: u8,
    pub signed: bool,
    pub data: Vec<i32>,
}

/// A decoded image: one plane per codestream component, in codestream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub components: Vec<DecodedComponent>,
}
