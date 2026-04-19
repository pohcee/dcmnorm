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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferSyntaxSupport {
    pub uid: String,
    pub name: String,
    pub encapsulated_pixel_data: bool,
    pub can_read_dataset: bool,
    pub can_write_dataset: bool,
    pub can_decode_pixel_data: bool,
    pub can_encode_pixel_data: bool,
}

impl TransferSyntaxSupport {
    pub fn can_transcode_to(&self) -> bool {
        self.can_write_dataset
            && (!self.encapsulated_pixel_data || self.can_encode_pixel_data)
    }
}

#[derive(Debug)]
pub enum TranscodeError {
    Read(ReadError),
    Write(WriteError),
    UnknownTransferSyntax(String),
    UnsupportedSourceTransferSyntax {
        uid: String,
        name: String,
        reason: String,
    },
    UnsupportedTargetTransferSyntax {
        uid: String,
        name: String,
        reason: String,
    },
    UnsupportedConversion {
        from_uid: String,
        to_uid: String,
        message: String,
    },
    MissingImageAttribute(&'static str),
    UnsupportedBitsAllocated(u16),
    InvalidDecodedPixelDataLength {
        bits_allocated: u16,
        length: usize,
    },
    DecodePixelData {
        uid: String,
        name: String,
        message: String,
    },
    EncodePixelData {
        uid: String,
        name: String,
        message: String,
    },
    ApplyAttribute(String),
}

impl fmt::Display for TranscodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "failed to read DICOM data: {error}"),
            Self::Write(error) => write!(formatter, "failed to write DICOM data: {error}"),
            Self::UnknownTransferSyntax(uid) => {
                write!(formatter, "unknown transfer syntax: {uid}")
            }
            Self::UnsupportedSourceTransferSyntax { uid, name, reason } => write!(
                formatter,
                "unsupported source transfer syntax {uid} ({name}): {reason}"
            ),
            Self::UnsupportedTargetTransferSyntax { uid, name, reason } => write!(
                formatter,
                "unsupported target transfer syntax {uid} ({name}): {reason}"
            ),
            Self::UnsupportedConversion {
                from_uid,
                to_uid,
                message,
            } => write!(
                formatter,
                "unsupported DICOM transcoding path {from_uid} -> {to_uid}: {message}"
            ),
            Self::MissingImageAttribute(name) => {
                write!(formatter, "missing required image attribute: {name}")
            }
            Self::UnsupportedBitsAllocated(bits) => write!(
                formatter,
                "unsupported BitsAllocated value for pixel transcoding: {bits}"
            ),
            Self::InvalidDecodedPixelDataLength {
                bits_allocated,
                length,
            } => write!(
                formatter,
                "decoded pixel data length {length} is not valid for BitsAllocated={bits_allocated}"
            ),
            Self::DecodePixelData { uid, name, message } => write!(
                formatter,
                "failed to decode encapsulated pixel data from {uid} ({name}): {message}"
            ),
            Self::EncodePixelData { uid, name, message } => write!(
                formatter,
                "failed to encode encapsulated pixel data for {uid} ({name}): {message}"
            ),
            Self::ApplyAttribute(error) => write!(
                formatter,
                "failed to apply transcoding attribute update: {error}"
            ),
        }
    }
}

impl Error for TranscodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Write(error) => Some(error),
            Self::UnknownTransferSyntax(_)
            | Self::UnsupportedSourceTransferSyntax { .. }
            | Self::UnsupportedTargetTransferSyntax { .. }
            | Self::UnsupportedConversion { .. }
            | Self::MissingImageAttribute(_)
            | Self::UnsupportedBitsAllocated(_)
            | Self::InvalidDecodedPixelDataLength { .. }
            | Self::DecodePixelData { .. }
            | Self::EncodePixelData { .. }
            | Self::ApplyAttribute(_) => None,
        }
    }
}

impl From<ReadError> for TranscodeError {
    fn from(value: ReadError) -> Self {
        Self::Read(value)
    }
}

impl From<WriteError> for TranscodeError {
    fn from(value: WriteError) -> Self {
        Self::Write(value)
    }
}

#[derive(Debug)]
pub enum RenderError {
    MissingImageAttribute(&'static str),
    UnsupportedBitsAllocated(u16),
    UnsupportedSamplesPerPixel(u16),
    UnsupportedPlanarConfiguration(u16),
    UnsupportedPhotometricInterpretation(String),
    InvalidFrameIndex { requested: usize, number_of_frames: usize },
    InvalidPixelDataLength { expected: usize, actual: usize },
    InvalidWindow(String),
    Transcode(TranscodeError),
    ImageEncoding(image::ImageError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingImageAttribute(name) => {
                write!(formatter, "missing required image attribute: {name}")
            }
            Self::UnsupportedBitsAllocated(bits) => {
                write!(formatter, "unsupported BitsAllocated value for rendering: {bits}")
            }
            Self::UnsupportedSamplesPerPixel(samples) => write!(
                formatter,
                "unsupported SamplesPerPixel value for rendering: {samples}"
            ),
            Self::UnsupportedPlanarConfiguration(value) => write!(
                formatter,
                "unsupported PlanarConfiguration value for rendering: {value}"
            ),
            Self::UnsupportedPhotometricInterpretation(value) => write!(
                formatter,
                "unsupported PhotometricInterpretation for rendering: {value}"
            ),
            Self::InvalidFrameIndex {
                requested,
                number_of_frames,
            } => write!(
                formatter,
                "frame index {requested} is out of range for NumberOfFrames={number_of_frames}"
            ),
            Self::InvalidPixelDataLength { expected, actual } => write!(
                formatter,
                "invalid PixelData length for rendered frame extraction: expected at least {expected} bytes, got {actual}"
            ),
            Self::InvalidWindow(message) => write!(formatter, "invalid VOI window configuration: {message}"),
            Self::Transcode(error) => write!(formatter, "failed to transcode DICOM data for rendering: {error}"),
            Self::ImageEncoding(error) => write!(formatter, "failed to encode rendered image: {error}"),
        }
    }
}

impl Error for RenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transcode(error) => Some(error),
            Self::ImageEncoding(error) => Some(error),
            Self::MissingImageAttribute(_)
            | Self::UnsupportedBitsAllocated(_)
            | Self::UnsupportedSamplesPerPixel(_)
            | Self::UnsupportedPlanarConfiguration(_)
            | Self::UnsupportedPhotometricInterpretation(_)
            | Self::InvalidFrameIndex { .. }
            | Self::InvalidPixelDataLength { .. }
            | Self::InvalidWindow(_) => None,
        }
    }
}

impl From<TranscodeError> for RenderError {
    fn from(value: TranscodeError) -> Self {
        Self::Transcode(value)
    }
}

impl From<image::ImageError> for RenderError {
    fn from(value: image::ImageError) -> Self {
        Self::ImageEncoding(value)
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
    /// When set, BulkDataURIs are generated as `{base}?offset=…&length=…` instead
    /// of the bare relative form `?offset=…&length=…`. Typical values are a
    /// `file:///absolute/path/to/input.dcm` URI or an `https://` URL, letting
    /// consumers resolve bulk data without a separate source argument.
    pub bulk_data_uri_base: Option<&'a str>,
}

impl Default for DicomJsonWriteOptions<'_> {
    fn default() -> Self {
        Self {
            format: DicomJsonFormat::Flat,
            bulk_data_mode: DicomJsonBulkDataMode::InlineBinary,
            key_style: DicomJsonKeyStyle::Name,
            bulk_data_source: None,
            bulk_data_uri_base: None,
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