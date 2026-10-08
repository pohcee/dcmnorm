//! JPEG 2000 codecs for dcmnorm.
//!
//! - [`htj2k`]: High-Throughput JPEG 2000 (Part 15, DICOM `.201`-`.203`) decode and lossless
//!   encode via the vendored OpenHTJ2K library.
//! - [`openjpeg`]: classic JPEG 2000 (Part 1) decode via OpenJPEG, with an explicit thread count.
//! - [`threads`]: the shared threading policy.
//!
//! See `docs/jpeg2000-codec-evaluation.md` at the repository root for why these two libraries
//! (and not Kakadu, the `j2k` crate, Grok or OpenJPH) were chosen.

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
