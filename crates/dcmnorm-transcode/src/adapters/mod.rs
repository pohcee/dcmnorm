//! Root module for extended pixel data adapters.
//!
//! Additional support for certain transfer syntaxes
//! can be added via Cargo features.
//!
//! - [`jpeg`] provides JPEG decoding (baseline and lossless, via the in-house
//!   `dcmnorm-jpeg`) and encoding (baseline, via `jpeg-encoder`).
//!   Requires the `jpeg` feature, enabled by default.
//! - [`jpeg2k`] contains JPEG 2000 support: classic JPEG 2000 through [OpenJPEG] and
//!   High-Throughput JPEG 2000 through OpenHTJ2K, both provided by the `dcmnorm-jpeg2000`
//!   crate. Requires the `jpeg2000` feature.
//! - [`rle_lossless`] provides native RLE lossless decoding.
//!   Requires the `rle` feature,
//!   enabled by default.
//!
//! JPEG-LS and JPEG XL have no adapters here (removed - never enabled by any
//! real build in this workspace; see `entries.rs`'s registry-completeness
//! stubs for those transfer syntax UIDs). dcmnorm's own JPEG-LS support
//! calls `charls` directly, bypassing this registry entirely.
//!
//! [OpenJPEG]: https://github.com/uclouvain/openjpeg
#[cfg(feature = "jpeg")]
pub mod jpeg;
#[cfg(feature = "jpeg2000")]
pub mod jpeg2k;
#[cfg(feature = "rle")]
pub mod rle_lossless;
#[cfg(feature = "deflate")]
pub mod deflated;

pub mod uncompressed;

/// **Note:** This module is a stub.
/// Enable the `jpeg` feature to use this module.
#[cfg(not(feature = "jpeg"))]
pub mod jpeg {}

/// **Note:** This module is a stub.
/// Enable the `jpeg2000` feature to use this module.
#[cfg(not(feature = "jpeg2000"))]
pub mod jpeg2k {}

/// **Note:** This module is a stub.
/// Enable the `rle` feature to use this module.
#[cfg(not(feature = "rle"))]
pub mod rle {}
