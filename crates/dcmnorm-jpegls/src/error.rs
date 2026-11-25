/// Errors from JPEG-LS codec encode/decode operations.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid data: {0}")]
    InvalidData(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("dimension mismatch: expected {expected} pixels, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    /// The entropy-coded scan ended before all samples were decoded (a marker
    /// or end-of-data was hit mid-scan). `offset` is the byte position within
    /// the input where decoding stopped; `context` names the phase.
    #[error("truncated JPEG-LS stream at offset {offset} ({context})")]
    Truncated {
        offset: usize,
        context: &'static str,
    },

    /// A caller-supplied or in-stream parameter was outside the permitted
    /// range.
    #[error("invalid parameter {name}={value} (allowed: {allowed})")]
    InvalidParameter {
        name: &'static str,
        value: i64,
        allowed: &'static str,
    },

    /// A JPEG marker (0xFFxx, xx >= 0x80) was encountered while decoding the
    /// entropy-coded scan. Carries the full marker code so the caller can
    /// distinguish EOI from LSE/RSTn etc.
    #[error("marker 0x{0:04X} encountered in entropy-coded scan")]
    Marker(u16),
}
