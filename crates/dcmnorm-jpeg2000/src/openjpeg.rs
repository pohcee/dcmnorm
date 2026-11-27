//! Classic JPEG 2000 decode (kept under the `openjpeg` module name for compatibility; backed by
//! pure-Rust `j2k`).

use crate::DecodedImage;

/// Decodes a J2K codestream or JP2-wrapped codestream at full resolution using pure-Rust `j2k`,
/// respecting [`crate::threads::per_call_threads`].
pub fn decode(data: &[u8]) -> Result<DecodedImage, String> {
    crate::htj2k::decode(data)
}
