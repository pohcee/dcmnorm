//! Simple, dcmnorm-owned error types for this crate's read/write/meta operations.
//!
//! `dicom-object`'s equivalents are large multi-variant `snafu` enums covering every possible
//! low-level failure mode. dcmnorm never matches on a specific variant of any of them (confirmed
//! by grep across `src/dicom_io/*.rs`) - only propagates them via `?` and formats them via
//! `Display` - so there's no reason to replicate that surface here.

use std::fmt;
use std::io;

/// An error occurring while reading a DICOM data set or file.
#[derive(Debug)]
pub enum ReadError {
    Io { source: io::Error, context: &'static str },
    Dataset { source: dcmnorm_parser::dataset::read::Error },
    NotDicom,
    UnsupportedTransferSyntax { uid: String },
    UnexpectedToken { context: &'static str },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Io { source, context } => write!(f, "I/O error while {context}: {source}"),
            ReadError::Dataset { source } => write!(f, "failed to read data set: {source}"),
            ReadError::NotDicom => write!(f, "not a valid DICOM file (missing preamble/DICM magic)"),
            ReadError::UnsupportedTransferSyntax { uid } => {
                write!(f, "unsupported transfer syntax: {uid}")
            }
            ReadError::UnexpectedToken { context } => {
                write!(f, "unexpected data set token while {context}")
            }
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::Io { source, .. } => Some(source),
            ReadError::Dataset { source } => Some(source),
            _ => None,
        }
    }
}

impl From<dcmnorm_parser::dataset::read::Error> for ReadError {
    fn from(source: dcmnorm_parser::dataset::read::Error) -> Self {
        ReadError::Dataset { source }
    }
}

/// An error occurring while writing a DICOM data set or file.
#[derive(Debug)]
pub enum WriteError {
    Io { source: io::Error, context: &'static str },
    Dataset { source: dcmnorm_parser::dataset::write::Error },
    UnsupportedTransferSyntax { uid: String },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Io { source, context } => write!(f, "I/O error while {context}: {source}"),
            WriteError::Dataset { source } => write!(f, "failed to write data set: {source}"),
            WriteError::UnsupportedTransferSyntax { uid } => {
                write!(f, "unsupported transfer syntax: {uid}")
            }
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WriteError::Io { source, .. } => Some(source),
            WriteError::Dataset { source } => Some(source),
            _ => None,
        }
    }
}

impl From<dcmnorm_parser::dataset::write::Error> for WriteError {
    fn from(source: dcmnorm_parser::dataset::write::Error) -> Self {
        WriteError::Dataset { source }
    }
}

/// An error occurring while attaching/validating a file meta table.
#[derive(Debug)]
pub enum WithMetaError {
    Read { source: ReadError },
    MissingField { field: &'static str },
}

impl fmt::Display for WithMetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WithMetaError::Read { source } => write!(f, "{source}"),
            WithMetaError::MissingField { field } => {
                write!(f, "missing required file meta field: {field}")
            }
        }
    }
}

impl std::error::Error for WithMetaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WithMetaError::Read { source } => Some(source),
            WithMetaError::MissingField { .. } => None,
        }
    }
}

impl From<ReadError> for WithMetaError {
    fn from(source: ReadError) -> Self {
        WithMetaError::Read { source }
    }
}

