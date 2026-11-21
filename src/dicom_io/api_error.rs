//! One error type for the language bindings (#6): an [`ApiErrorKind`] each binding maps onto its
//! own exception classes, plus the transfer syntax / frame context a decode or encode failure
//! carries. The kind is derived from dcmnorm's own error enums here, once, so the JNI, napi and
//! PyO3 layers never classify an error by matching on its message.

use std::fmt;

use super::dimse::DimseError;
use super::types::{DicomJsonError, ReadError, RenderError, TranscodeError, WriteError};

/// What kind of failure an [`ApiError`] is. The names are what the bindings expose: Java maps
/// each onto a `DcmnormException` subclass, Node sets it as the error's `code`
/// (`ERR_DCMNORM_<KIND>`), Python onto a `DcmnormError` subclass.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ApiErrorKind {
    /// Not DICOM, or malformed DICOM (bad preamble, truncated or unparseable data set).
    InvalidDicom,
    /// A transfer syntax this build can't read, decode or encode, or an unknown UID.
    UnsupportedTransferSyntax,
    /// Pixel data could not be decoded from its (encapsulated) transfer syntax.
    Decode,
    /// Pixel data could not be encoded into the target transfer syntax, or the file not written.
    Encode,
    /// A frame could not be rendered (bad frame index, unsupported photometric interpretation,
    /// invalid window, ...).
    Render,
    /// A DIMSE association or operation failed.
    Dimse,
    /// Reading or writing a file or stream failed.
    Io,
    /// An argument or option the caller passed is invalid.
    InvalidArgument,
    /// A panic inside dcmnorm (a bug), caught at the binding boundary.
    Internal,
}

impl ApiErrorKind {
    /// The kind's stable name, as the bindings expose it (`"InvalidDicom"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::InvalidDicom => "InvalidDicom",
            Self::UnsupportedTransferSyntax => "UnsupportedTransferSyntax",
            Self::Decode => "Decode",
            Self::Encode => "Encode",
            Self::Render => "Render",
            Self::Dimse => "Dimse",
            Self::Io => "Io",
            Self::InvalidArgument => "InvalidArgument",
            Self::Internal => "Internal",
        }
    }
}

/// A classified error with its context. `source_transfer_syntax_uid` / `target_transfer_syntax_uid`
/// are set where the failure involves them (decode: source; encode: target; an unsupported
/// conversion: both), `frame_index` when it happened while rendering or decoding one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
    pub source_transfer_syntax_uid: Option<String>,
    pub target_transfer_syntax_uid: Option<String>,
    pub frame_index: Option<usize>,
}

impl ApiError {
    pub fn new(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source_transfer_syntax_uid: None,
            target_transfer_syntax_uid: None,
            frame_index: None,
        }
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ApiErrorKind::InvalidArgument, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ApiErrorKind::Io, message)
    }

    /// A caught panic's payload as an `Internal` error, with the same message prefix the
    /// bindings have always used.
    pub fn from_panic(payload: Box<dyn std::any::Any + Send>) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "dcmnorm panicked while processing this call".to_string());
        Self::new(
            ApiErrorKind::Internal,
            format!("dcmnorm internal error: {message}"),
        )
    }

    /// Fill in the source transfer syntax if the error doesn't name one already.
    pub fn with_source_ts(mut self, uid: Option<&str>) -> Self {
        if self.source_transfer_syntax_uid.is_none() {
            self.source_transfer_syntax_uid =
                uid.map(|uid| uid.trim_end_matches(['\0', ' ']).to_owned());
        }
        self
    }

    /// Fill in the target transfer syntax if the error doesn't name one already.
    pub fn with_target_ts(mut self, uid: Option<&str>) -> Self {
        if self.target_transfer_syntax_uid.is_none() {
            self.target_transfer_syntax_uid =
                uid.map(|uid| uid.trim_end_matches(['\0', ' ']).to_owned());
        }
        self
    }

    pub fn with_frame_index(mut self, frame_index: usize) -> Self {
        self.frame_index.get_or_insert(frame_index);
        self
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// Run `f`, turning a panic into an [`ApiErrorKind::Internal`] error - the shared form of each
/// binding's `guarded()`.
pub fn catch_api_panic<T>(f: impl FnOnce() -> Result<T, ApiError>) -> Result<T, ApiError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(ApiError::from_panic(payload)),
    }
}

impl From<ReadError> for ApiError {
    fn from(error: ReadError) -> Self {
        let message = error.to_string();
        match error {
            ReadError::UnsupportedTransferSyntax { uid } => {
                let mut api = Self::new(ApiErrorKind::UnsupportedTransferSyntax, message);
                api.source_transfer_syntax_uid = Some(uid);
                api
            }
            ReadError::Io { ref source, .. }
                if !matches!(
                    source.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
                ) =>
            {
                Self::new(ApiErrorKind::Io, message)
            }
            _ => Self::new(ApiErrorKind::InvalidDicom, message),
        }
    }
}

impl From<WriteError> for ApiError {
    fn from(error: WriteError) -> Self {
        Self::new(ApiErrorKind::Encode, error.to_string())
    }
}

impl From<TranscodeError> for ApiError {
    fn from(error: TranscodeError) -> Self {
        let message = error.to_string();
        match error {
            TranscodeError::Read(read) => read.into(),
            TranscodeError::Write(write) => write.into(),
            TranscodeError::UnknownTransferSyntax(uid) => {
                let mut api = Self::new(ApiErrorKind::UnsupportedTransferSyntax, message);
                api.target_transfer_syntax_uid = Some(uid);
                api
            }
            TranscodeError::UnsupportedSourceTransferSyntax { uid, .. } => {
                let mut api = Self::new(ApiErrorKind::UnsupportedTransferSyntax, message);
                api.source_transfer_syntax_uid = Some(uid);
                api
            }
            TranscodeError::UnsupportedTargetTransferSyntax { uid, .. } => {
                let mut api = Self::new(ApiErrorKind::UnsupportedTransferSyntax, message);
                api.target_transfer_syntax_uid = Some(uid);
                api
            }
            TranscodeError::UnsupportedConversion {
                from_uid, to_uid, ..
            } => {
                let mut api = Self::new(ApiErrorKind::UnsupportedTransferSyntax, message);
                api.source_transfer_syntax_uid = Some(from_uid);
                api.target_transfer_syntax_uid = Some(to_uid);
                api
            }
            TranscodeError::DecodePixelData { uid, .. } => {
                let mut api = Self::new(ApiErrorKind::Decode, message);
                api.source_transfer_syntax_uid = Some(uid);
                api
            }
            TranscodeError::EncodePixelData { uid, .. } => {
                let mut api = Self::new(ApiErrorKind::Encode, message);
                api.target_transfer_syntax_uid = Some(uid);
                api
            }
            TranscodeError::MissingImageAttribute(_) => {
                Self::new(ApiErrorKind::InvalidDicom, message)
            }
            TranscodeError::UnsupportedBitsAllocated(_)
            | TranscodeError::InvalidDecodedPixelDataLength { .. }
            | TranscodeError::ApplyAttribute(_) => Self::new(ApiErrorKind::Encode, message),
        }
    }
}

impl From<RenderError> for ApiError {
    fn from(error: RenderError) -> Self {
        match error {
            RenderError::Transcode(transcode) => transcode.into(),
            RenderError::InvalidFrameIndex { requested, .. } => {
                Self::new(ApiErrorKind::Render, error.to_string()).with_frame_index(requested)
            }
            RenderError::MissingImageAttribute(_) => {
                Self::new(ApiErrorKind::InvalidDicom, error.to_string())
            }
            other => Self::new(ApiErrorKind::Render, other.to_string()),
        }
    }
}

impl From<DimseError> for ApiError {
    fn from(error: DimseError) -> Self {
        Self::new(ApiErrorKind::Dimse, error.to_string())
    }
}

impl From<DicomJsonError> for ApiError {
    fn from(error: DicomJsonError) -> Self {
        let message = error.to_string();
        match error {
            DicomJsonError::UnsupportedTransferSyntax(uid) => {
                let mut api = Self::new(ApiErrorKind::UnsupportedTransferSyntax, message);
                api.source_transfer_syntax_uid = Some(uid);
                api
            }
            DicomJsonError::Serde(_)
            | DicomJsonError::InvalidJsonRoot
            | DicomJsonError::InvalidJsonValue { .. }
            | DicomJsonError::InvalidStandardElement { .. }
            | DicomJsonError::UnknownAttribute(_)
            | DicomJsonError::InvalidBulkDataUri(_)
            | DicomJsonError::MissingBulkDataSource(_)
            | DicomJsonError::BulkDataOutOfRange { .. } => {
                Self::new(ApiErrorKind::InvalidArgument, message)
            }
            _ => Self::new(ApiErrorKind::InvalidDicom, message),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::new(ApiErrorKind::Io, error.to_string())
    }
}

impl From<super::types::WithMetaError> for ApiError {
    fn from(error: super::types::WithMetaError) -> Self {
        Self::new(ApiErrorKind::InvalidArgument, error.to_string())
    }
}

impl From<super::types::DicomIoError> for ApiError {
    fn from(error: super::types::DicomIoError) -> Self {
        Self::new(ApiErrorKind::Encode, error.to_string())
    }
}

impl ApiError {
    /// Classify a boxed error from one of dcmnorm's `Box<dyn Error>` APIs (e.g.
    /// `dicom_file_to_json`) by its concrete type, falling back to `fallback`.
    pub fn from_boxed(
        error: Box<dyn std::error::Error + Send + Sync>,
        fallback: ApiErrorKind,
    ) -> Self {
        let error = match error.downcast::<ReadError>() {
            Ok(read) => return (*read).into(),
            Err(other) => other,
        };
        let error = match error.downcast::<DicomJsonError>() {
            Ok(json) => return (*json).into(),
            Err(other) => other,
        };
        let error = match error.downcast::<TranscodeError>() {
            Ok(transcode) => return (*transcode).into(),
            Err(other) => other,
        };
        let error = match error.downcast::<RenderError>() {
            Ok(render) => return (*render).into(),
            Err(other) => other,
        };
        match error.downcast::<std::io::Error>() {
            Ok(io) => (*io).into(),
            Err(other) => Self::new(fallback, other.to_string()),
        }
    }
}
