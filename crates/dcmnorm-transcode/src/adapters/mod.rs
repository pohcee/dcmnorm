//! Root module for extended pixel data adapters.
//!
//! Additional support for certain transfer syntaxes
//! can be added via Cargo features.
//!
//! - [`jpeg`] provides JPEG decoding (baseline and lossless, via the in-house
//!   `dcmnorm-jpeg`) and encoding (baseline, via `jpeg-encoder`).
//!   Requires the `jpeg` feature, enabled by default.
//! - [`jpeg2k`] contains JPEG 2000 support,
//!   which is currently available through [OpenJPEG].
//!   Use feature `openjpeg-sys`
//!   to statically link to the OpenJPEG reference implementation,
//!   thus providing JPEG 2000 decoding.
//!   Alternatively, feature `openjp2` provides native JPEG 2000 decoding
//!   via the [Rust port of OpenJPEG][OpenJPEG-rs],
//!   which is maintained separately.
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
//! [OpenJPEG-rs]: https://crates.io/crates/openjp2
#[cfg(feature = "jpeg")]
pub mod jpeg;
#[cfg(any(feature = "openjp2", feature = "openjpeg-sys"))]
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
/// Enable either `openjp2` or `openjpeg-sys` to use this module.
#[cfg(not(any(feature = "openjp2", feature = "openjpeg-sys")))]
pub mod jpeg2k {}

/// **Note:** This module is a stub.
/// Enable the `rle` feature to use this module.
#[cfg(not(feature = "rle"))]
pub mod rle {}
