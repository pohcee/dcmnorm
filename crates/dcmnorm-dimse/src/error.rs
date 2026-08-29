//! Simple, dcmnorm-owned error type for association-level failures.
//!
//! `dicom-ul`'s `association::Error` is a large multi-variant `snafu` enum; confirmed by grep
//! that `dcmnorm` never matches on a specific variant of it (or of this crate's predecessor) -
//! only propagates via `?`/`Display` (wrapped as `DimseError::Association` in `dimse.rs`) - so
//! there's no reason to replicate that surface here. See `crates/dcmnorm-object/src/error.rs`
//! for the same reasoning applied to the object-layer error types.

use std::fmt;

use crate::pdu::{AssociationRJ, Pdu};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Pdu(crate::pdu::Error),
    Connect(std::io::Error),
    /// The peer's max PDU length was outside the range this implementation accepts.
    InvalidMaxPdu(u32),
    /// No presentation context was accepted by the acceptor.
    NoAcceptedPresentationContexts,
    /// The peer's A-ASSOCIATE-RJ.
    Rejected(AssociationRJ),
    /// A PDU was received that isn't valid at this point in the association state machine.
    UnexpectedPdu(Box<Pdu>),
    /// Local protocol version doesn't match the peer's.
    ProtocolVersionMismatch { expected: u16, got: u16 },
    /// Missing at least one presentation context proposal (a `with_presentation_context` call
    /// is required before establishing).
    MissingPresentationContexts,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Pdu(e) => write!(f, "PDU error: {e}"),
            Error::Connect(e) => write!(f, "failed to connect: {e}"),
            Error::InvalidMaxPdu(v) => write!(f, "invalid max PDU length: {v}"),
            Error::NoAcceptedPresentationContexts => {
                write!(f, "no presentation context was accepted")
            }
            Error::Rejected(rj) => write!(f, "association rejected: {rj}"),
            Error::UnexpectedPdu(pdu) => write!(f, "unexpected PDU: {pdu:?}"),
            Error::ProtocolVersionMismatch { expected, got } => {
                write!(f, "protocol version mismatch: expected {expected}, got {got}")
            }
            Error::MissingPresentationContexts => {
                write!(f, "no presentation contexts were proposed")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) | Error::Connect(e) => Some(e),
            Error::Pdu(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<crate::pdu::Error> for Error {
    fn from(e: crate::pdu::Error) -> Self {
        Error::Pdu(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
