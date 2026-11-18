//! The operations behind the bindings' typed dataset API (#6), shared so that the JNI, napi and
//! PyO3 layers only convert arguments and results: reading a source (a file, bytes or a
//! forward-only stream) into a [`DatasetDocument`], writing DICOM JSON from a source or a
//! document, writing a document back to DICOM, rendering one frame to raw pixels, and
//! dictionary lookups. Every function reports an [`ApiError`].

use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;

use dcmnorm_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dcmnorm_core::{Tag, VR};
use dcmnorm_dictionary::{tags, StandardDataDictionary};
use dcmnorm_encoding::transfer_syntax::TransferSyntaxIndex;
use dcmnorm_object::{
    DefaultDicomObject, DeferredValues, FileMetaTable, FileMetaTableBuilder, InMemDicomObject,
    OpenFileOptions, ReadPreamble,
};
use dcmnorm_transcode::TransferSyntaxRegistry;
use serde_json::Value as JsonValue;

use super::api_error::{ApiError, ApiErrorKind};
use super::dataset_tree::{
    file_object_to_document, tag_from_u32, tag_u32, tree_to_object, BulkReader, BulkRefHandling,
    BulkSource, DatasetDocument, DatasetTree,
};
use super::render::{render_dicom_frame_pixels, RenderFrameOutput, RenderPipelineOptions};
use super::types::{
    BulkDataUriBuilder, DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonWriteOptions,
};

/// Where a DICOM object is read from.
pub enum DicomSource<'a> {
    /// A file, read with seeks: values left in the file are skipped, not read.
    Path(&'a Path),
    /// A complete object in memory.
    Bytes(&'a [u8]),
    /// A forward-only stream (an object-storage download, a socket): read once, front to back.
    /// Skipped values are read and discarded; with `until_pixel_data` nothing past the
    /// PixelData header is read at all.
    Reader(&'a mut dyn Read),
}

/// How [`read_dataset_document`] reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct DatasetReadOptions {
    /// Stop at the top-level PixelData header (dcm4che's `readDatasetUntilPixelData`): PixelData
    /// is a [`super::dataset_tree::BulkDataRef`] to where its value starts, and nothing after it
    /// is read.
    pub until_pixel_data: bool,
    /// Read every bulk value into memory instead of leaving the large ones (bulk VRs longer than
    /// 32 bytes, and encapsulated pixel data) in the source as references.
    pub load_bulk_data: bool,
}

// -------------------------------------------------------------------------------------------
// Stream adapters
// -------------------------------------------------------------------------------------------

/// A forward-only reader made seekable for the deferring parser, which only ever seeks forward
/// (relative to where it is, or to its own position): a seek reads and discards. A seek that
/// runs past the end of the stream is recorded in `short` (the deferring read can't tell, the
/// way it can check a file's length).
struct ForwardSeekReader<R> {
    inner: R,
    position: u64,
    short: bool,
}

impl<R: Read> Read for ForwardSeekReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl<R: Read> Seek for ForwardSeekReader<R> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            SeekFrom::Current(delta) if delta >= 0 => self.position + delta as u64,
            SeekFrom::Start(target) if target >= self.position => target,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "a stream source can only be read forward",
                ))
            }
        };
        let wanted = target - self.position;
        let skipped = std::io::copy(&mut (&mut self.inner).take(wanted), &mut std::io::sink())?;
        if skipped < wanted {
            self.short = true;
        }
        self.position = target;
        Ok(target)
    }
}

/// Records what is read through it (the stream's Part 10 header, so it can be replayed).
struct Recorder<'a> {
    inner: &'a mut dyn Read,
    recorded: Vec<u8>,
}

impl Read for Recorder<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.recorded.extend_from_slice(&buffer[..read]);
        Ok(read)
    }
}

type Deferred = Option<(DefaultDicomObject, DeferredValues)>;

fn read_all(reader: &mut dyn Read, mut prefix: Vec<u8>) -> Result<Vec<u8>, ApiError> {
    reader
        .read_to_end(&mut prefix)
        .map_err(|error| ApiError::io(format!("reading the DICOM stream: {error}")))?;
    Ok(prefix)
}

/// A stream's deferring read, or its complete bytes when it can't be read that way (no Part 10
/// header, or a deflated data set).
enum StreamRead {
    Deferred(DefaultDicomObject, DeferredValues),
    Complete(Vec<u8>),
}

fn read_stream_deferring(
    reader: &mut dyn Read,
    defer: &dyn Fn(&dcmnorm_core::header::DataElementHeader) -> bool,
    defer_pixel_sequences: bool,
    stop_at_pixel_data: bool,
) -> Result<StreamRead, ApiError> {
    let mut recorder = Recorder {
        inner: &mut *reader,
        recorded: Vec::with_capacity(512),
    };
    let meta = FileMetaTable::read_from(&mut recorder);
    let prefix = recorder.recorded;
    let deferrable = match &meta {
        Ok(Some(meta)) => meta.transfer_syntax_ts().is_some_and(|ts| {
            !matches!(
                ts.codec(),
                dcmnorm_encoding::transfer_syntax::Codec::Dataset(_)
            )
        }),
        _ => false,
    };
    if !deferrable {
        return Ok(StreamRead::Complete(read_all(reader, prefix)?));
    }

    let mut source = ForwardSeekReader {
        inner: Cursor::new(prefix).chain(reader),
        position: 0,
        short: false,
    };
    let result = OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .from_reader_deferring(
            &mut source,
            None,
            defer,
            defer_pixel_sequences,
            stop_at_pixel_data,
        )?;
    if source.short {
        return Err(ApiError::new(
            ApiErrorKind::InvalidDicom,
            "the DICOM stream ended inside a value it declares (truncated data)",
        ));
    }
    match result {
        Some((object, deferred)) => Ok(StreamRead::Deferred(object, deferred)),
        None => Err(ApiError::new(
            ApiErrorKind::InvalidDicom,
            "the DICOM stream could not be read",
        )),
    }
}

fn deferring_read_file(
    path: &Path,
    defer: &dyn Fn(&dcmnorm_core::header::DataElementHeader) -> bool,
    defer_pixel_sequences: bool,
    stop_at_pixel_data: bool,
) -> Result<Deferred, ApiError> {
    let file = std::fs::File::open(path)
        .map_err(|error| ApiError::io(format!("opening {}: {error}", path.display())))?;
    let length = file
        .metadata()
        .map_err(|error| ApiError::io(error.to_string()))?
        .len();
    Ok(OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .from_reader_deferring(
            std::io::BufReader::new(file),
            Some(length),
            defer,
            defer_pixel_sequences,
            stop_at_pixel_data,
        )?)
}

fn deferring_read_bytes(
    bytes: &[u8],
    defer: &dyn Fn(&dcmnorm_core::header::DataElementHeader) -> bool,
    defer_pixel_sequences: bool,
    stop_at_pixel_data: bool,
) -> Result<Deferred, ApiError> {
    Ok(OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .from_reader_deferring(
            Cursor::new(bytes),
            Some(bytes.len() as u64),
            defer,
            defer_pixel_sequences,
            stop_at_pixel_data,
        )?)
}

fn never_defer(_: &dcmnorm_core::header::DataElementHeader) -> bool {
    false
}

// -------------------------------------------------------------------------------------------
// Reading a document
// -------------------------------------------------------------------------------------------

/// Read `source` into a [`DatasetDocument`]. A Part 10 source is read in one pass that leaves
/// large bulk values (and, with `until_pixel_data`, everything from PixelData on) in the source
/// as references; a data set without File Meta Information, or a deflated one, is read in full
/// instead, with its bulk values in memory (and, with `until_pixel_data`, PixelData and what
/// follows it left out).
pub fn read_dataset_document(
    source: DicomSource<'_>,
    options: DatasetReadOptions,
) -> Result<DatasetDocument, ApiError> {
    let defer: &dyn Fn(&dcmnorm_core::header::DataElementHeader) -> bool = if options.load_bulk_data
    {
        &never_defer
    } else {
        &super::io::defer_bulk_value
    };
    let defer_pixel_sequences = !options.load_bulk_data;
    let stop = options.until_pixel_data;

    let deferred_or_full =
        |deferred: Result<Deferred, ApiError>,
         full: &dyn Fn() -> Result<DefaultDicomObject, ApiError>| {
            match deferred {
                Ok(Some((object, deferred))) => {
                    Ok(file_object_to_document(&object, Some(&deferred)))
                }
                Ok(None) | Err(_) => {
                    let object = full()?;
                    Ok(complete_document(&object, stop))
                }
            }
        };

    match source {
        DicomSource::Path(path) => deferred_or_full(
            deferring_read_file(path, defer, defer_pixel_sequences, stop),
            &|| Ok(super::io::read_dicom_file(path)?),
        ),
        DicomSource::Bytes(bytes) => deferred_or_full(
            deferring_read_bytes(bytes, defer, defer_pixel_sequences, stop),
            &|| Ok(super::io::read_dicom_bytes(bytes)?),
        ),
        DicomSource::Reader(reader) => {
            match read_stream_deferring(reader, defer, defer_pixel_sequences, stop)? {
                StreamRead::Deferred(object, deferred) => {
                    Ok(file_object_to_document(&object, Some(&deferred)))
                }
                StreamRead::Complete(bytes) => Ok(complete_document(
                    &super::io::read_dicom_bytes(&bytes)?,
                    stop,
                )),
            }
        }
    }
}

fn complete_document(object: &DefaultDicomObject, until_pixel_data: bool) -> DatasetDocument {
    let mut document = file_object_to_document(object, None);
    if until_pixel_data {
        let pixel_data = tag_u32(tags::PIXEL_DATA);
        document
            .dataset
            .elements
            .retain(|element| element.tag < pixel_data);
    }
    document
}

// -------------------------------------------------------------------------------------------
// DICOM JSON
// -------------------------------------------------------------------------------------------

/// Options for [`source_to_dicom_json`] / [`document_to_dicom_json`]: PS3.18 Annex F JSON
/// ("standard" format) of the data set.
#[derive(Clone, Copy, Debug)]
pub struct DicomJsonExportOptions<'a> {
    /// Only these top-level attributes (empty: all).
    pub include_tags: &'a [u32],
    /// Leave these top-level attributes out.
    pub exclude_tags: &'a [u32],
    /// `Uri` (default): bulk values longer than 32 bytes, and PixelData, as `BulkDataURI`
    /// (never read from the source); `InlineBinary`: every value inline, base64.
    pub bulk_data_mode: DicomJsonBulkDataMode,
    pub bulk_data_uri_base: Option<&'a str>,
    pub bulk_data_uri_builder: Option<BulkDataUriBuilder<'a>>,
    /// Include the File Meta Information (group 0002). Off by default: it isn't part of the
    /// data set a DICOMweb metadata response describes.
    pub include_file_meta: bool,
    /// Add dcmnorm's `Keyword` and `VM` members to each top-level attribute (not part of
    /// PS3.18 Annex F).
    pub keyword_and_vm: bool,
}

impl Default for DicomJsonExportOptions<'_> {
    fn default() -> Self {
        Self {
            include_tags: &[],
            exclude_tags: &[],
            bulk_data_mode: DicomJsonBulkDataMode::Uri,
            bulk_data_uri_base: None,
            bulk_data_uri_builder: None,
            include_file_meta: false,
            keyword_and_vm: false,
        }
    }
}

impl<'a> DicomJsonExportOptions<'a> {
    fn write_options(&self) -> DicomJsonWriteOptions<'a> {
        DicomJsonWriteOptions {
            format: DicomJsonFormat::Standard,
            bulk_data_mode: self.bulk_data_mode,
            bulk_data_uri_base: self.bulk_data_uri_base,
            bulk_data_uri_builder: self.bulk_data_uri_builder,
            omit_keyword_vm: !self.keyword_and_vm,
            ..DicomJsonWriteOptions::default()
        }
    }

    fn filter(&self, object: &mut InMemDicomObject) {
        if !self.include_tags.is_empty() {
            let keep: Vec<Tag> = object
                .iter()
                .map(|element| element.header().tag)
                .filter(|tag| self.include_tags.contains(&tag_u32(*tag)))
                .collect();
            let all: Vec<Tag> = object.iter().map(|element| element.header().tag).collect();
            for tag in all {
                if !keep.contains(&tag) {
                    object.remove_element(tag);
                }
            }
        }
        for tag in self.exclude_tags {
            object.remove_element(tag_from_u32(*tag));
        }
    }

    fn finish(&self, value: JsonValue) -> Result<String, ApiError> {
        let value = match value {
            JsonValue::Object(mut map) if !self.include_file_meta => {
                map.retain(|key, _| !key.starts_with("0002"));
                JsonValue::Object(map)
            }
            other => other,
        };
        serde_json::to_string(&value)
            .map_err(|error| ApiError::new(ApiErrorKind::Internal, error.to_string()))
    }
}

fn write_json(
    object: &mut DefaultDicomObject,
    deferred: Option<&DeferredValues>,
    bulk_data_source: Option<&[u8]>,
    options: &DicomJsonExportOptions<'_>,
) -> Result<String, ApiError> {
    options.filter(object);
    let bulk_scan_failed = std::cell::Cell::new(false);
    let bulk_scan_cursor = std::cell::Cell::new(0usize);
    let mut write_options = options.write_options();
    write_options.deferred_bulk_data = deferred;
    if let Some(source) = bulk_data_source {
        write_options.bulk_data_source = Some(source);
        write_options.bulk_scan_failed = Some(&bulk_scan_failed);
        write_options.bulk_scan_cursor = Some(&bulk_scan_cursor);
    }
    let value = super::standard_json::write_standard_json_value(object, write_options)?;
    options.finish(value)
}

/// PS3.18 Annex F DICOM JSON of `source`. In `Uri` mode bulk values are never read (a file or
/// bytes are read with seeks past them; a stream reads and discards them), and each
/// `BulkDataURI` names where its value is in the source (or what `bulk_data_uri_builder` makes of
/// that). A source without File Meta Information, or deflated, is read in full and its bulk
/// values located by a scan of its bytes, as `dcmnorm` does.
pub fn source_to_dicom_json(
    source: DicomSource<'_>,
    options: &DicomJsonExportOptions<'_>,
) -> Result<String, ApiError> {
    if options.bulk_data_mode == DicomJsonBulkDataMode::InlineBinary {
        let mut object = match source {
            DicomSource::Path(path) => super::io::read_dicom_file(path)?,
            DicomSource::Bytes(bytes) => super::io::read_dicom_bytes(bytes)?,
            DicomSource::Reader(reader) => {
                super::io::read_dicom_bytes(read_all(reader, Vec::new())?)?
            }
        };
        return write_json(&mut object, None, None, options);
    }

    let defer = super::io::defer_bulk_value;
    let complete = |bytes: &[u8]| -> Result<String, ApiError> {
        let mut object = super::io::read_dicom_bytes(bytes)?;
        write_json(&mut object, None, Some(bytes), options)
    };
    match source {
        DicomSource::Path(path) => {
            match deferring_read_file(path, &defer, true, false) {
                Ok(Some((mut object, deferred))) => {
                    write_json(&mut object, Some(&deferred), None, options)
                }
                _ => complete(&std::fs::read(path).map_err(|error| {
                    ApiError::io(format!("reading {}: {error}", path.display()))
                })?),
            }
        }
        DicomSource::Bytes(bytes) => match deferring_read_bytes(bytes, &defer, true, false) {
            Ok(Some((mut object, deferred))) => {
                write_json(&mut object, Some(&deferred), None, options)
            }
            _ => complete(bytes),
        },
        DicomSource::Reader(reader) => match read_stream_deferring(reader, &defer, true, false)? {
            StreamRead::Deferred(mut object, deferred) => {
                write_json(&mut object, Some(&deferred), None, options)
            }
            StreamRead::Complete(bytes) => complete(&bytes),
        },
    }
}

/// PS3.18 Annex F DICOM JSON of a document (e.g. one a binding read and edited). Its bulk data
/// references are written as `BulkDataURI`s in `Uri` mode; in `InlineBinary` mode they are read
/// from `bulk_source`.
pub fn document_to_dicom_json(
    document: &DatasetDocument,
    bulk_source: &BulkSource<'_>,
    options: &DicomJsonExportOptions<'_>,
) -> Result<String, ApiError> {
    let (mut object, deferred) = if options.bulk_data_mode == DicomJsonBulkDataMode::Uri {
        let (dataset, deferred) =
            tree_to_object(&document.dataset, &mut BulkRefHandling::Defer, true)?;
        (with_document_meta(document, dataset, None)?, Some(deferred))
    } else {
        (document_object(document, bulk_source)?, None)
    };
    write_json(&mut object, deferred.as_ref(), None, options)
}

// -------------------------------------------------------------------------------------------
// Writing DICOM
// -------------------------------------------------------------------------------------------

fn effective_transfer_syntax(document: &DatasetDocument) -> &str {
    let uid = document.transfer_syntax_uid.trim_end_matches(['\0', ' ']);
    if uid.is_empty() {
        dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN
    } else {
        uid
    }
}

fn first_string(tree: &DatasetTree, tag: Tag) -> Option<String> {
    match tree.get(tag_u32(tag)).map(|element| &element.value) {
        Some(super::dataset_tree::TreeValue::Strings(values)) => values.first().cloned(),
        _ => None,
    }
}

/// File Meta Information for `dataset`, created as dcm4che's `createFileMetaInformation` does:
/// the Media Storage SOP Class/Instance UIDs from the data set's SOP Class/Instance UIDs, the
/// transfer syntax given, and dcmnorm as the implementation - keeping the source's Source
/// Application Entity Title, if it had one.
fn with_document_meta(
    document: &DatasetDocument,
    dataset: InMemDicomObject,
    transfer_syntax_uid: Option<&str>,
) -> Result<DefaultDicomObject, ApiError> {
    let mut builder = FileMetaTableBuilder::new().transfer_syntax(
        transfer_syntax_uid.unwrap_or_else(|| effective_transfer_syntax(document)),
    );
    if let Some(uid) = first_string(&document.dataset, tags::SOP_CLASS_UID) {
        builder = builder.media_storage_sop_class_uid(uid);
    } else if let Some(uid) = first_string(&document.file_meta, tags::MEDIA_STORAGE_SOP_CLASS_UID) {
        builder = builder.media_storage_sop_class_uid(uid);
    }
    if let Some(uid) = first_string(&document.dataset, tags::SOP_INSTANCE_UID) {
        builder = builder.media_storage_sop_instance_uid(uid);
    } else if let Some(uid) =
        first_string(&document.file_meta, tags::MEDIA_STORAGE_SOP_INSTANCE_UID)
    {
        builder = builder.media_storage_sop_instance_uid(uid);
    }
    if let Some(title) = first_string(&document.file_meta, tags::SOURCE_APPLICATION_ENTITY_TITLE) {
        builder = builder.source_application_entity_title(title);
    }
    Ok(dataset.with_meta(builder)?)
}

/// `document` as an object in its own transfer syntax, bulk data references read from
/// `bulk_source`.
fn document_object(
    document: &DatasetDocument,
    bulk_source: &BulkSource<'_>,
) -> Result<DefaultDicomObject, ApiError> {
    let source_uid = effective_transfer_syntax(document);
    let source_ts = TransferSyntaxRegistry.get(source_uid);
    let big_endian = source_ts
        .is_some_and(|ts| ts.endianness() == dcmnorm_encoding::transfer_syntax::Endianness::Big);
    let encapsulated = source_ts.is_some_and(|ts| ts.is_encapsulated_pixel_data());
    let mut reader = BulkReader::new(bulk_source);
    let (dataset, _) = tree_to_object(
        &document.dataset,
        &mut BulkRefHandling::Resolve {
            source: &mut reader,
            big_endian,
            encapsulated,
        },
        true,
    )?;
    with_document_meta(document, dataset, None)
}

/// How [`write_document`] writes.
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteDocumentOptions<'a> {
    /// Transcode to this transfer syntax (default: the document's own).
    pub transfer_syntax_uid: Option<&'a str>,
}

/// `document` as a file object ready to write: File Meta Information created for it, bulk data
/// references read from `bulk_source` (straight from the source into this object, never through
/// the caller), and transcoded when `options.transfer_syntax_uid` differs from its own.
pub fn document_to_file_object(
    document: &DatasetDocument,
    bulk_source: &BulkSource<'_>,
    options: &WriteDocumentOptions<'_>,
) -> Result<DefaultDicomObject, ApiError> {
    let object = document_object(document, bulk_source)?;
    let source_uid = effective_transfer_syntax(document).to_owned();
    match options
        .transfer_syntax_uid
        .map(|uid| uid.trim_end_matches(['\0', ' ']))
    {
        Some(target) if !target.is_empty() && target != source_uid => {
            super::io::transcode_dcmnorm_object_owned(object, target).map_err(|error| {
                ApiError::from(error)
                    .with_source_ts(Some(&source_uid))
                    .with_target_ts(Some(target))
            })
        }
        _ => Ok(object),
    }
}

/// Write `document` as a DICOM Part 10 file to `out` (see [`document_to_file_object`]).
pub fn write_document(
    document: &DatasetDocument,
    bulk_source: &BulkSource<'_>,
    options: &WriteDocumentOptions<'_>,
    out: &mut dyn Write,
) -> Result<(), ApiError> {
    let object = document_to_file_object(document, bulk_source, options)?;
    object.write_all(out)?;
    Ok(())
}

/// [`write_document`] to a file.
pub fn write_document_to_path(
    document: &DatasetDocument,
    bulk_source: &BulkSource<'_>,
    options: &WriteDocumentOptions<'_>,
    path: &Path,
) -> Result<(), ApiError> {
    let file = std::fs::File::create(path)
        .map_err(|error| ApiError::io(format!("creating {}: {error}", path.display())))?;
    let mut out = std::io::BufWriter::new(file);
    write_document(document, bulk_source, options, &mut out)?;
    out.flush().map_err(|error| ApiError::io(error.to_string()))
}

/// Transcode `source` to `target_transfer_syntax_uid`, with the error classified and naming both
/// transfer syntaxes.
pub fn transcode_source(
    source: DicomSource<'_>,
    target_transfer_syntax_uid: &str,
) -> Result<DefaultDicomObject, ApiError> {
    let object = read_full(source)?;
    let source_uid = object
        .meta()
        .transfer_syntax()
        .trim_end_matches(['\0', ' '])
        .to_owned();
    super::io::transcode_dcmnorm_object_owned(object, target_transfer_syntax_uid).map_err(|error| {
        ApiError::from(error)
            .with_source_ts(Some(&source_uid))
            .with_target_ts(Some(target_transfer_syntax_uid))
    })
}

fn read_full(source: DicomSource<'_>) -> Result<DefaultDicomObject, ApiError> {
    Ok(match source {
        DicomSource::Path(path) => super::io::read_dicom_file(path)?,
        DicomSource::Bytes(bytes) => super::io::read_dicom_bytes(bytes)?,
        DicomSource::Reader(reader) => super::io::read_dicom_bytes(read_all(reader, Vec::new())?)?,
    })
}

// -------------------------------------------------------------------------------------------
// Rendering
// -------------------------------------------------------------------------------------------

/// Render frame `options.frame_index` of `source` to 8-bit display pixels, through the same
/// pipeline as `dcmnorm`'s PNG/JPEG output but without encoding them (see
/// [`render_dicom_frame_pixels`]). A file source reads only that frame of native pixel data
/// where it can.
pub fn render_source_frame(
    source: DicomSource<'_>,
    options: &RenderPipelineOptions,
) -> Result<RenderFrameOutput, ApiError> {
    let object = match source {
        DicomSource::Path(path) => {
            match super::render::read_dicom_file_for_frame(path, options.frame_index) {
                Ok(Some(object)) => object,
                _ => super::io::read_dicom_file(path)?,
            }
        }
        other => read_full(other)?,
    };
    let source_uid = object
        .meta()
        .transfer_syntax()
        .trim_end_matches(['\0', ' '])
        .to_owned();
    render_dicom_frame_pixels(&object, options).map_err(|error| {
        let error = ApiError::from(error).with_source_ts(Some(&source_uid));
        match error.kind {
            ApiErrorKind::Decode | ApiErrorKind::Render => {
                error.with_frame_index(options.frame_index)
            }
            _ => error,
        }
    })
}

// -------------------------------------------------------------------------------------------
// Dictionary
// -------------------------------------------------------------------------------------------

/// The standard keyword of `tag` (`"StudyInstanceUID"`), including repeating groups (60xx).
pub fn keyword_for_tag(tag: u32) -> Option<&'static str> {
    StandardDataDictionary
        .by_tag(tag_from_u32(tag))
        .map(|entry| entry.alias())
}

/// The tag of a standard keyword, or of a tag expression (`"(0020,000D)"`, `"0020000D"`).
pub fn tag_for_keyword(keyword: &str) -> Option<u32> {
    StandardDataDictionary.parse_tag(keyword).map(tag_u32)
}

/// The dictionary VR of `tag` (for an attribute with several possible VRs, the one dcmnorm
/// reads it as: US for US/SS, OW for OB/OW).
pub fn dictionary_vr(tag: u32) -> Option<VR> {
    StandardDataDictionary
        .by_tag(tag_from_u32(tag))
        .map(|entry| entry.vr().relaxed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dicom_io::dataset_tree::{decode_document, encode_document, BulkDataRef, TreeValue};

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test/files")
            .join(name)
    }

    /// A reader with no Seek, standing in for a network stream.
    struct OnlyRead<R>(R);

    impl<R: Read> Read for OnlyRead<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            // short reads, as sockets give
            let limit = buffer.len().min(1000);
            self.0.read(&mut buffer[..limit])
        }
    }

    fn read_path(name: &str, options: DatasetReadOptions) -> DatasetDocument {
        read_dataset_document(DicomSource::Path(&fixture(name)), options).unwrap()
    }

    fn read_stream(name: &str, options: DatasetReadOptions) -> DatasetDocument {
        let bytes = std::fs::read(fixture(name)).unwrap();
        let mut stream = OnlyRead(Cursor::new(bytes));
        read_dataset_document(DicomSource::Reader(&mut stream), options).unwrap()
    }

    fn value(document: &DatasetDocument, tag: Tag) -> &TreeValue {
        &document
            .dataset
            .get(tag_u32(tag))
            .unwrap_or_else(|| panic!("{tag} missing"))
            .value
    }

    const UNTIL_PIXEL_DATA: DatasetReadOptions = DatasetReadOptions {
        until_pixel_data: true,
        load_bulk_data: false,
    };

    #[test]
    fn until_pixel_data_references_native_pixel_data_where_it_is() {
        let document = read_path("mr_small.dcm", UNTIL_PIXEL_DATA);
        assert_eq!(document.transfer_syntax_uid, "1.2.840.10008.1.2.1");
        assert_eq!(
            value(&document, tags::PIXEL_DATA),
            &TreeValue::BulkRef(BulkDataRef {
                offset: 1500,
                length: Some(8192)
            })
        );
        assert_eq!(document.dataset.elements.last().unwrap().tag, 0x7FE0_0010);
        assert_eq!(value(&document, tags::ROWS), &TreeValue::Ints(vec![64]));
        assert_eq!(
            value(&document, tags::PATIENT_NAME),
            &TreeValue::Strings(vec!["CompressedSamples^MR1".into()])
        );
        // the referenced range is the value the parser reads
        let bytes = std::fs::read(fixture("mr_small.dcm")).unwrap();
        let full = crate::dicom_io::read_dicom_file(fixture("mr_small.dcm")).unwrap();
        let pixels = full
            .get(tags::PIXEL_DATA)
            .unwrap()
            .value()
            .to_bytes()
            .unwrap()
            .into_owned();
        assert_eq!(&bytes[1500..1500 + 8192], &pixels[..]);
        assert!(!document.file_meta.elements.is_empty());
    }

    #[test]
    fn stream_reads_match_file_reads() {
        for name in [
            "mr_small.dcm",
            "mr_rle.dcm",
            "sr.dcm",
            "ct.dcm",
            "rtdose.dcm",
            "explicit_vr_be.dcm",
        ] {
            for options in [UNTIL_PIXEL_DATA, DatasetReadOptions::default()] {
                assert_eq!(
                    read_path(name, options),
                    read_stream(name, options),
                    "{name} {options:?}"
                );
            }
            let bytes = std::fs::read(fixture(name)).unwrap();
            let from_bytes =
                read_dataset_document(DicomSource::Bytes(&bytes), DatasetReadOptions::default())
                    .unwrap();
            assert_eq!(
                from_bytes,
                read_path(name, DatasetReadOptions::default()),
                "{name} bytes"
            );
        }
    }

    #[test]
    fn encapsulated_pixel_data_is_referenced_and_resolved() {
        let until = read_path("mr_rle.dcm", UNTIL_PIXEL_DATA);
        let TreeValue::BulkRef(stopped) = value(&until, tags::PIXEL_DATA) else {
            panic!()
        };
        assert_eq!(stopped.length, None);
        let full = read_path("mr_rle.dcm", DatasetReadOptions::default());
        let TreeValue::BulkRef(whole) = value(&full, tags::PIXEL_DATA) else {
            panic!()
        };
        assert_eq!((whole.offset, whole.length), (1516, Some(6128)));
        assert_eq!(stopped.offset, whole.offset);

        let path = fixture("mr_rle.dcm");
        let original = crate::dicom_io::read_dicom_file(&path).unwrap();
        for document in [&until, &full] {
            let object =
                document_to_file_object(document, &BulkSource::Path(&path), &Default::default())
                    .unwrap();
            assert_eq!(
                object.get(tags::PIXEL_DATA).unwrap().value(),
                original.get(tags::PIXEL_DATA).unwrap().value()
            );
            assert_eq!(object.meta().transfer_syntax(), "1.2.840.10008.1.2.5");
        }
    }

    #[test]
    fn written_document_round_trips_with_pixel_data_from_the_source() {
        for name in ["mr_small.dcm", "explicit_vr_be.dcm", "rtdose.dcm", "us.dcm"] {
            let path = fixture(name);
            let document = read_path(name, DatasetReadOptions::default());
            let mut out = Vec::new();
            write_document(
                &document,
                &BulkSource::Path(&path),
                &Default::default(),
                &mut out,
            )
            .unwrap();
            let written = crate::dicom_io::read_dicom_bytes(&out).unwrap();
            let original = crate::dicom_io::read_dicom_file(&path).unwrap();
            assert_eq!(
                written
                    .get(tags::PIXEL_DATA)
                    .unwrap()
                    .value()
                    .to_bytes()
                    .unwrap(),
                original
                    .get(tags::PIXEL_DATA)
                    .unwrap()
                    .value()
                    .to_bytes()
                    .unwrap(),
                "{name}"
            );
            // File Meta Information is created from the data set, as dcm4che's createFileMetaInformation does
            assert_eq!(
                written.meta().media_storage_sop_instance_uid(),
                original
                    .get(tags::SOP_INSTANCE_UID)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .trim_end_matches('\0')
            );
            // and reading it back gives the same data set
            let again = read_dataset_document(
                DicomSource::Bytes(&out),
                DatasetReadOptions {
                    load_bulk_data: true,
                    ..Default::default()
                },
            )
            .unwrap();
            let loaded = read_path(
                name,
                DatasetReadOptions {
                    load_bulk_data: true,
                    ..Default::default()
                },
            );
            assert_eq!(again.dataset, loaded.dataset, "{name}");
        }
    }

    #[test]
    fn write_without_a_source_for_its_references_is_an_argument_error() {
        let document = read_path("mr_small.dcm", UNTIL_PIXEL_DATA);
        let error = write_document(
            &document,
            &BulkSource::None,
            &Default::default(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::InvalidArgument);
    }

    #[test]
    fn write_can_transcode() {
        let path = fixture("mr_small.dcm");
        let document = read_path("mr_small.dcm", DatasetReadOptions::default());
        let options = WriteDocumentOptions {
            transfer_syntax_uid: Some("1.2.840.10008.1.2"),
        };
        let object =
            document_to_file_object(&document, &BulkSource::Path(&path), &options).unwrap();
        assert_eq!(object.meta().transfer_syntax(), "1.2.840.10008.1.2");
        let error = document_to_file_object(
            &document,
            &BulkSource::Path(&path),
            &WriteDocumentOptions {
                transfer_syntax_uid: Some("1.2.3.4.5.6"),
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::UnsupportedTransferSyntax);
        assert_eq!(
            error.target_transfer_syntax_uid.as_deref(),
            Some("1.2.3.4.5.6")
        );
        assert_eq!(
            error.source_transfer_syntax_uid.as_deref(),
            Some("1.2.840.10008.1.2.1")
        );
    }

    #[test]
    fn binary_form_round_trips() {
        for name in ["sr.dcm", "ct.dcm", "mr_rle.dcm", "overlay.dcm"] {
            for options in [
                UNTIL_PIXEL_DATA,
                DatasetReadOptions {
                    load_bulk_data: true,
                    until_pixel_data: false,
                },
            ] {
                let document = read_path(name, options);
                assert_eq!(
                    decode_document(&encode_document(&document)).unwrap(),
                    document,
                    "{name}"
                );
            }
        }
        assert!(decode_document(b"DCMT\x01\x00").is_err());
    }

    #[test]
    fn values_are_typed() {
        let ct = read_path("ct.dcm", UNTIL_PIXEL_DATA);
        assert_eq!(
            value(&ct, tags::IMAGE_TYPE),
            &TreeValue::Strings(vec![
                "ORIGINAL".into(),
                "PRIMARY".into(),
                "AXIAL".into(),
                "HELIX".into()
            ])
        );
        assert_eq!(
            value(&ct, tags::PIXEL_SPACING),
            &TreeValue::Strings(vec!["0.58984375".into(), "0.58984375".into()])
        );
        let rtdose = read_path("rtdose.dcm", UNTIL_PIXEL_DATA);
        assert_eq!(
            value(&rtdose, tags::FRAME_INCREMENT_POINTER),
            &TreeValue::Tags(vec![0x3004_000C])
        );
        assert_eq!(
            value(&rtdose, tags::NUMBER_OF_FRAMES),
            &TreeValue::Strings(vec!["15".into()])
        );
        let sr = read_path("sr.dcm", UNTIL_PIXEL_DATA);
        let TreeValue::Sequence(items) = value(&sr, tags::CONTENT_SEQUENCE) else {
            panic!("ContentSequence")
        };
        assert!(!items.is_empty());
    }

    #[test]
    fn metaless_and_deflated_sources_are_read_in_full() {
        // a bare data set (no preamble, no meta group)
        let full = std::fs::read(fixture("mr_small.dcm")).unwrap();
        let object = crate::dicom_io::read_dicom_bytes(&full).unwrap();
        let mut bare = Vec::new();
        let implicit = TransferSyntaxRegistry.get("1.2.840.10008.1.2").unwrap();
        object.write_dataset_with_ts(&mut bare, implicit).unwrap();
        let nometa = read_dataset_document(DicomSource::Bytes(&bare), UNTIL_PIXEL_DATA).unwrap();
        assert!(nometa.dataset.get(tag_u32(tags::PIXEL_DATA)).is_none());
        assert_eq!(value(&nometa, tags::ROWS), &TreeValue::Ints(vec![64]));
        let deflated = read_stream("deflated.dcm", DatasetReadOptions::default());
        assert_eq!(deflated.transfer_syntax_uid, "1.2.840.10008.1.2.1.99");
        assert!(matches!(
            value(&deflated, tags::PIXEL_DATA),
            TreeValue::Bytes(_)
        ));
    }

    #[test]
    fn errors_are_classified() {
        let error = read_dataset_document(
            DicomSource::Path(&fixture("notdicom.txt")),
            Default::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::InvalidDicom, "{error}");
        let error = read_dataset_document(
            DicomSource::Path(&fixture("does-not-exist.dcm")),
            Default::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::Io, "{error}");
        let error = render_source_frame(
            DicomSource::Path(&fixture("mr_small.dcm")),
            &RenderPipelineOptions {
                frame_index: 7,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            (error.kind, error.frame_index),
            (ApiErrorKind::Render, Some(7))
        );
        let error =
            transcode_source(DicomSource::Path(&fixture("mr_small.dcm")), "1.2.3.4").unwrap_err();
        assert_eq!(error.kind, ApiErrorKind::UnsupportedTransferSyntax);
    }

    #[test]
    fn render_gives_raw_pixels() {
        let gray = render_source_frame(
            DicomSource::Path(&fixture("mr_small.dcm")),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            (gray.width, gray.height, gray.samples_per_pixel),
            (64, 64, 1)
        );
        assert_eq!(gray.bytes.len(), 64 * 64);
        let rgb = render_source_frame(
            DicomSource::Path(&fixture("sc_jpeg_baseline.dcm")),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            (rgb.width, rgb.height, rgb.samples_per_pixel),
            (100, 100, 3)
        );
        assert_eq!(rgb.bytes.len(), 100 * 100 * 3);
        let bytes = std::fs::read(fixture("mr_small.dcm")).unwrap();
        let from_bytes =
            render_source_frame(DicomSource::Bytes(&bytes), &Default::default()).unwrap();
        assert_eq!(from_bytes.bytes, gray.bytes);
        let last = render_source_frame(
            DicomSource::Path(&fixture("us2.dcm")),
            &RenderPipelineOptions {
                frame_index: 226,
                output_width: Some(180),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            (last.width, last.height, last.bytes.len()),
            (180, 180, 180 * 180)
        );
    }

    #[test]
    fn json_from_source_is_the_standard_writer_output() {
        let path = fixture("mr_small.dcm");
        let json: JsonValue = serde_json::from_str(
            &source_to_dicom_json(DicomSource::Path(&path), &Default::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(json["7FE00010"]["BulkDataURI"], "?offset=1500&length=8192");
        assert_eq!(
            json["00100010"]["Value"][0]["Alphabetic"],
            "CompressedSamples^MR1"
        );
        assert!(json
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("0002")));
        assert!(json["00100010"].get("Keyword").is_none());

        let existing: JsonValue = serde_json::from_str(
            &crate::dicom_io::dicom_file_to_json(
                &path,
                DicomJsonWriteOptions {
                    format: DicomJsonFormat::Standard,
                    bulk_data_mode: DicomJsonBulkDataMode::Uri,
                    omit_keyword_vm: true,
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .unwrap();
        let mut existing = existing.as_object().unwrap().clone();
        existing.retain(|key, _| !key.starts_with("0002"));
        assert_eq!(json, JsonValue::Object(existing));

        let builder = |request: &crate::dicom_io::BulkDataUriRequest| {
            format!(
                "https://pacs/bulk/{:08X}?o={}&l={:?}",
                tag_u32(request.tag),
                request.offset,
                request.length
            )
        };
        let filtered: JsonValue = serde_json::from_str(
            &source_to_dicom_json(
                DicomSource::Path(&path),
                &DicomJsonExportOptions {
                    include_tags: &[0x0010_0010, 0x7FE0_0010, 0x0028_0010],
                    exclude_tags: &[0x0028_0010],
                    bulk_data_uri_builder: Some(BulkDataUriBuilder(&builder)),
                    include_file_meta: true,
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .unwrap();
        let keys: Vec<&String> = filtered
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| !key.starts_with("0002"))
            .collect();
        assert_eq!(keys, ["00100010", "7FE00010"]);
        assert_eq!(
            filtered["7FE00010"]["BulkDataURI"],
            "https://pacs/bulk/7FE00010?o=1500&l=Some(8192)"
        );
        assert!(filtered.get("00020010").is_some());

        // a stream gives the same JSON
        let mut stream = OnlyRead(std::fs::File::open(&path).unwrap());
        let streamed: JsonValue = serde_json::from_str(
            &source_to_dicom_json(DicomSource::Reader(&mut stream), &Default::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(streamed, json);
    }

    #[test]
    fn json_from_a_document_matches_json_from_its_source() {
        let path = fixture("mr_rle.dcm");
        let from_source =
            source_to_dicom_json(DicomSource::Path(&path), &Default::default()).unwrap();
        let document = read_path("mr_rle.dcm", DatasetReadOptions::default());
        let from_document =
            document_to_dicom_json(&document, &BulkSource::None, &Default::default()).unwrap();
        let a: JsonValue = serde_json::from_str(&from_source).unwrap();
        let b: JsonValue = serde_json::from_str(&from_document).unwrap();
        assert_eq!(a["7FE00010"], b["7FE00010"]);
        assert_eq!(a["00100010"], b["00100010"]);
        assert_eq!(a["00280030"], b["00280030"]);
        let inline = document_to_dicom_json(
            &document,
            &BulkSource::Path(&path),
            &DicomJsonExportOptions {
                bulk_data_mode: DicomJsonBulkDataMode::InlineBinary,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(inline.contains("InlineBinary"));
    }

    #[test]
    fn dictionary_lookups() {
        assert_eq!(keyword_for_tag(0x0020_000D), Some("StudyInstanceUID"));
        assert_eq!(tag_for_keyword("StudyInstanceUID"), Some(0x0020_000D));
        assert_eq!(dictionary_vr(0x0010_0010), Some(VR::PN));
        assert_eq!(keyword_for_tag(0x6002_3000), Some("OverlayData"));
        assert_eq!(keyword_for_tag(0x0009_1001), None);
    }
}
