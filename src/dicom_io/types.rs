use std::error::Error;
use std::fmt;

use dicom_core::{Tag, VR};

pub use dicom_object::{ReadError, WithMetaError, WriteError};

pub(super) const ITEM_TAG: Tag = Tag(0xFFFE, 0xE000);
pub(super) const ITEM_DELIMITATION_TAG: Tag = Tag(0xFFFE, 0xE00D);
pub(super) const SEQUENCE_DELIMITATION_TAG: Tag = Tag(0xFFFE, 0xE0DD);

#[derive(Debug)]
pub enum DicomIoError {
    PrepareMeta(WithMetaError),
    Write(WriteError),
}

impl fmt::Display for DicomIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrepareMeta(error) => {
                write!(formatter, "failed to prepare DICOM file metadata: {error}")
            }
            Self::Write(error) => write!(formatter, "failed to write DICOM file: {error}"),
        }
    }
}

impl Error for DicomIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PrepareMeta(error) => Some(error),
            Self::Write(error) => Some(error),
        }
    }
}

impl From<WithMetaError> for DicomIoError {
    fn from(value: WithMetaError) -> Self {
        Self::PrepareMeta(value)
    }
}

impl From<WriteError> for DicomIoError {
    fn from(value: WriteError) -> Self {
        Self::Write(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DicomJsonFormat {
    #[default]
    Flat,
    Standard,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DicomJsonBulkDataMode {
    Uri,
    #[default]
    InlineBinary,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DicomJsonKeyStyle {
    #[default]
    Name,
    Hex,
}

#[derive(Clone, Copy, Debug)]
pub struct DicomJsonReadOptions<'a> {
    pub format: DicomJsonFormat,
    pub bulk_data_source: Option<&'a [u8]>,
}

impl Default for DicomJsonReadOptions<'_> {
    fn default() -> Self {
        Self {
            format: DicomJsonFormat::Flat,
            bulk_data_source: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DicomJsonWriteOptions<'a> {
    pub format: DicomJsonFormat,
    pub bulk_data_mode: DicomJsonBulkDataMode,
    pub key_style: DicomJsonKeyStyle,
    pub bulk_data_source: Option<&'a [u8]>,
}

impl Default for DicomJsonWriteOptions<'_> {
    fn default() -> Self {
        Self {
            format: DicomJsonFormat::Flat,
            bulk_data_mode: DicomJsonBulkDataMode::InlineBinary,
            key_style: DicomJsonKeyStyle::Name,
            bulk_data_source: None,
        }
    }
}

#[derive(Debug)]
pub enum DicomJsonError {
    Serde(serde_json::Error),
    PrepareMeta(WithMetaError),
    UnknownAttribute(String),
    InvalidJsonRoot,
    InvalidJsonValue { keyword: String, message: String },
    InvalidStandardElement { tag: String, message: String },
    InvalidBulkDataUri(String),
    MissingBulkDataSource(String),
    BulkDataOutOfRange { uri: String, length: usize },
    BulkDataNotFound(Tag),
    UnsupportedBulkDataVr { tag: Tag, vr: VR },
    InvalidBulkDataLength { tag: Tag, vr: VR, length: usize },
    UnsupportedTransferSyntax(String),
}

impl fmt::Display for DicomJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serde(error) => write!(formatter, "failed to parse JSON: {error}"),
            Self::PrepareMeta(error) => {
                write!(formatter, "failed to prepare DICOM file metadata: {error}")
            }
            Self::UnknownAttribute(keyword) => {
                write!(formatter, "unknown DICOM attribute keyword: {keyword}")
            }
            Self::InvalidJsonRoot => write!(formatter, "expected a JSON object at the root"),
            Self::InvalidJsonValue { keyword, message } => {
                write!(formatter, "invalid JSON value for {keyword}: {message}")
            }
            Self::InvalidStandardElement { tag, message } => {
                write!(formatter, "invalid standard JSON element for {tag}: {message}")
            }
            Self::InvalidBulkDataUri(uri) => write!(formatter, "invalid BulkDataURI: {uri}"),
            Self::MissingBulkDataSource(uri) => {
                write!(formatter, "BulkDataURI requires source bytes: {uri}")
            }
            Self::BulkDataOutOfRange { uri, length } => write!(
                formatter,
                "BulkDataURI points outside the source bytes: {uri} (source length {length})"
            ),
            Self::BulkDataNotFound(tag) => {
                write!(formatter, "could not locate bulk data for tag {tag}")
            }
            Self::UnsupportedBulkDataVr { tag, vr } => {
                write!(formatter, "unsupported bulk data VR {vr:?} for tag {tag}")
            }
            Self::InvalidBulkDataLength { tag, vr, length } => write!(
                formatter,
                "invalid bulk data length {length} for tag {tag} with VR {vr:?}"
            ),
            Self::UnsupportedTransferSyntax(uid) => {
                write!(formatter, "unsupported transfer syntax for bulk data conversion: {uid}")
            }
        }
    }
}

impl Error for DicomJsonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Serde(error) => Some(error),
            Self::PrepareMeta(error) => Some(error),
            Self::UnknownAttribute(_)
            | Self::InvalidJsonRoot
            | Self::InvalidJsonValue { .. }
            | Self::InvalidStandardElement { .. }
            | Self::InvalidBulkDataUri(_)
            | Self::MissingBulkDataSource(_)
            | Self::BulkDataOutOfRange { .. }
            | Self::BulkDataNotFound(_)
            | Self::UnsupportedBulkDataVr { .. }
            | Self::InvalidBulkDataLength { .. }
            | Self::UnsupportedTransferSyntax(_) => None,
        }
    }
}

impl From<serde_json::Error> for DicomJsonError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

impl From<WithMetaError> for DicomJsonError {
    fn from(value: WithMetaError) -> Self {
        Self::PrepareMeta(value)
    }
}

pub(super) enum BulkRepresentation {
    Uri(String),
    InlineBinary(String),
}

#[derive(Clone, Copy)]
pub(super) struct ElementLocation {
    pub offset: usize,
    pub length: usize,
}

pub(super) struct ParsedHeader {
    pub tag: Tag,
    pub header_length: usize,
    pub length: Option<usize>,
}

pub(super) struct TransferSyntaxInfo {
    pub explicit_vr: bool,
    pub little_endian: bool,
}