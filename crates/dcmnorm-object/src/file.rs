//! Whole-file DICOM objects: a [`FileMetaTable`] plus an object (normally [`InMemDicomObject`]),
//! and the [`OpenFileOptions`] builder for reading them from disk/a byte source.

use std::fs::File;
use std::io::{BufReader, Cursor, Read, Write};
use std::path::Path;

use dcmnorm_encoding::transfer_syntax::TransferSyntaxIndex;
use dcmnorm_parser::dataset::read::DataSetReaderOptions;
use dcmnorm_transcode::TransferSyntaxRegistry;

use crate::error::{ReadError, WriteError};
use crate::mem::InMemDicomObject;
use crate::meta::{FileMetaTable, DICM_MAGIC, PREAMBLE_LEN};

/// A DICOM object with its accompanying File Meta Information.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDicomObject<T> {
    pub(crate) meta: FileMetaTable,
    pub(crate) object: T,
}

/// The default, in-memory DICOM file object type - a [`FileMetaTable`] plus an
/// [`InMemDicomObject`].
pub type DefaultDicomObject = FileDicomObject<InMemDicomObject>;

impl<T> FileDicomObject<T> {
    pub fn meta(&self) -> &FileMetaTable {
        &self.meta
    }

    pub fn meta_mut(&mut self) -> &mut FileMetaTable {
        &mut self.meta
    }

    pub fn into_inner(self) -> T {
        self.object
    }
}

impl std::ops::Deref for DefaultDicomObject {
    type Target = InMemDicomObject;
    fn deref(&self) -> &Self::Target {
        &self.object
    }
}

impl std::ops::DerefMut for DefaultDicomObject {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.object
    }
}

impl DefaultDicomObject {
    /// Construct a new, empty object already carrying the given meta table.
    pub fn new_empty_with_meta(meta: FileMetaTable) -> Self {
        FileDicomObject {
            meta,
            object: InMemDicomObject::new_empty(),
        }
    }

    /// Insert or replace a text element by tag and VR, creating it if absent.
    pub fn put_str(
        &mut self,
        tag: dcmnorm_core::Tag,
        vr: dcmnorm_core::VR,
        value: impl Into<String>,
    ) {
        self.object.put_str(tag, vr, value);
    }

    /// Read a Part 10 file (128-byte preamble + "DICM" magic + meta group + data set) from
    /// `path`.
    pub fn open_file(path: impl AsRef<Path>) -> Result<Self, ReadError> {
        let file = File::open(path.as_ref()).map_err(|source| ReadError::Io {
            source,
            context: "opening file",
        })?;
        Self::from_reader(BufReader::new(file))
    }

    /// Read a Part 10 object (128-byte preamble + "DICM" magic + meta group + data set) from
    /// any [`Read`] source.
    pub fn from_reader(mut source: impl Read) -> Result<Self, ReadError> {
        let mut meta = FileMetaTable::read_from(&mut source)?.ok_or(ReadError::NotDicom)?;
        let ts = meta
            .transfer_syntax_ts()
            .ok_or_else(|| ReadError::UnsupportedTransferSyntax {
                uid: meta.transfer_syntax.clone(),
            })?;
        let object = InMemDicomObject::read_dataset_with_ts(source, ts)?;
        backfill_media_storage_uids(&mut meta, &object);
        Ok(FileDicomObject { meta, object })
    }

    /// Write this object as a full Part 10 file (preamble + "DICM" magic + meta group + data
    /// set) to `path`.
    pub fn write_to_file(&self, path: impl AsRef<Path>) -> Result<(), WriteError> {
        let file = File::create(path.as_ref()).map_err(|source| WriteError::Io {
            source,
            context: "creating file",
        })?;
        self.write_all(&mut std::io::BufWriter::new(file))
    }

    /// Write this object as a full Part 10 stream (preamble + "DICM" magic + meta group + data
    /// set) to any [`Write`] sink.
    pub fn write_all(&self, mut to: impl Write) -> Result<(), WriteError> {
        self.meta.write_to(&mut to)?;
        let ts = TransferSyntaxRegistry
            .get(crate::meta::io_util::trim_uid(&self.meta.transfer_syntax))
            .ok_or_else(|| WriteError::UnsupportedTransferSyntax {
                uid: self.meta.transfer_syntax.clone(),
            })?;
        self.object.write_dataset_with_ts(&mut to, ts)
    }

    /// Write this object as a full Part 10 stream, delegating `(7FE0,0010) PixelData` to
    /// `write_pixel_data` at its exact tag-order position. The second argument passed to the
    /// closure is the byte offset within `to` where the data set starts (right after the Part 10
    /// preamble + File Meta Information).
    pub fn write_all_with_pixel_data<W, E, F>(
        &self,
        mut to: W,
        write_pixel_data: F,
    ) -> Result<(), E>
    where
        W: Write,
        E: From<WriteError>,
        F: for<'w> FnOnce(
            &mut dcmnorm_parser::dataset::DataSetWriter<
                Box<dyn Write + 'w>,
                dcmnorm_encoding::transfer_syntax::DynEncoder<'w, Box<dyn Write + 'w>>,
            >,
            u64,
        ) -> Result<(), E>,
    {
        let mut meta_sink = CountingWriter {
            inner: &mut to,
            count: 0,
        };
        self.meta.write_to(&mut meta_sink)?;
        let dataset_start = meta_sink.count;
        let ts = TransferSyntaxRegistry
            .get(crate::meta::io_util::trim_uid(&self.meta.transfer_syntax))
            .ok_or_else(|| WriteError::UnsupportedTransferSyntax {
                uid: self.meta.transfer_syntax.clone(),
            })?;
        self.object
            .write_dataset_with_ts_and_pixel_data(&mut to, ts, |writer| {
                write_pixel_data(writer, dataset_start)
            })
    }
}

/// Whether to require/skip the 128-byte preamble when opening a file.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum ReadPreamble {
    #[default]
    Auto,
    Always,
}

/// Builder for reading a DICOM file with non-default options (an explicit preamble
/// requirement, or an early-stop tag for partial/fast reads).
#[derive(Debug, Clone, Default)]
pub struct OpenFileOptions {
    read_preamble: ReadPreamble,
    read_until: Option<dcmnorm_core::Tag>,
    flexible_decoding: bool,
}

impl OpenFileOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read_preamble(mut self, mode: ReadPreamble) -> Self {
        self.read_preamble = mode;
        self
    }

    /// Stop reading as soon as an element with a tag greater than `tag` is encountered
    /// (inclusive of `tag` itself) - a fast path for partial/filtered reads that don't need
    /// the rest of the data set. Matches `dcmnorm`'s `--filter` CLI early-stop optimization.
    pub fn read_until(mut self, tag: dcmnorm_core::Tag) -> Self {
        self.read_until = Some(tag);
        self
    }

    /// Detect from the data set itself whether a Little Endian file is encoded with explicit or
    /// implicit VRs, instead of trusting its declared transfer syntax - for non-conformant files
    /// that declare Explicit VR Little Endian but write Implicit VR (see
    /// `DataSetReaderOptions::flexible_decoding`). Applies to [`Self::open_file`] and
    /// [`Self::from_reader`]. Off by default.
    pub fn flexible_decoding(mut self, flexible_decoding: bool) -> Self {
        self.flexible_decoding = flexible_decoding;
        self
    }

    pub fn open_file(self, path: impl AsRef<Path>) -> Result<DefaultDicomObject, ReadError> {
        let file = File::open(path.as_ref()).map_err(|source| ReadError::Io {
            source,
            context: "opening file",
        })?;
        self.from_reader(BufReader::new(file))
    }

    pub fn from_reader(self, mut source: impl Read) -> Result<DefaultDicomObject, ReadError> {
        let mut meta = self.read_meta(&mut source)?;
        let ts = meta
            .transfer_syntax_ts()
            .ok_or_else(|| ReadError::UnsupportedTransferSyntax {
                uid: meta.transfer_syntax.clone(),
            })?;

        let options = DataSetReaderOptions::default().flexible_decoding(self.flexible_decoding);
        let mut object = if let Some(stop_tag) = self.read_until {
            crate::mem::read_dataset_until(source, ts, stop_tag, options)?
        } else {
            InMemDicomObject::read_dataset_with_ts_options(source, ts, options)?
        };
        if ts.is_encapsulated_pixel_data() {
            normalize_defined_length_encapsulated_pixel_data(&mut object);
        }
        backfill_media_storage_uids(&mut meta, &object);
        Ok(FileDicomObject { meta, object })
    }

    /// Read everything in `path` up to (not including) a native PixelData value, and report
    /// where that value sits in the file instead of loading it - the building block for reading
    /// one frame of a large multi-frame object (seek to it) without paying for all the others.
    ///
    /// Returns `Ok(None)` whenever this shortcut doesn't apply and the caller should do a full
    /// read instead: an encapsulated or dataset-compressed (deflated) transfer syntax (no
    /// stable byte offsets into native pixels), or no top-level, defined-length PixelData.
    /// `read_until` is ignored. Elements after PixelData (trailing padding, digital signatures)
    /// are not read.
    pub fn open_file_until_pixel_data(
        self,
        path: impl AsRef<Path>,
    ) -> Result<Option<(DefaultDicomObject, PixelDataValueLocation)>, ReadError> {
        let file = File::open(path.as_ref()).map_err(|source| ReadError::Io {
            source,
            context: "opening file",
        })?;
        let mut source = CountingReader {
            inner: BufReader::new(file),
            count: 0,
        };
        let mut meta = self.read_meta(&mut source)?;
        let ts = meta
            .transfer_syntax_ts()
            .ok_or_else(|| ReadError::UnsupportedTransferSyntax {
                uid: meta.transfer_syntax.clone(),
            })?;
        if ts.is_encapsulated_pixel_data()
            || !matches!(ts.codec(), dcmnorm_encoding::transfer_syntax::Codec::None)
        {
            return Ok(None);
        }

        let (object, header) = crate::mem::read_dataset_until_pixel_data_header(&mut source, ts)?;
        let Some(header) = header else {
            return Ok(None);
        };
        let Some(length) = header.len.get() else {
            return Ok(None);
        };
        backfill_media_storage_uids(&mut meta, &object);
        Ok(Some((
            FileDicomObject { meta, object },
            PixelDataValueLocation {
                vr: header.vr,
                length,
                offset: source.count,
            },
        )))
    }

    /// Read `path` in full except for the large values `defer` picks out - plus, when
    /// `defer_pixel_sequences` is set, encapsulated pixel data - which are skipped by seeking
    /// past them and returned as file locations in [`DeferredValues`] instead (e.g. to emit as
    /// DICOM JSON BulkDataURIs without ever reading them). Deferred elements stay in the object
    /// with an empty placeholder value; see [`DeferredValues`].
    ///
    /// Returns `Ok(None)` when byte offsets in the file can't be relied on, so the caller should
    /// read in full instead: a dataset-compressed (deflated) transfer syntax, a data set that
    /// isn't a seekable Part 10 file, or a deferred range that runs past the end of the file
    /// (truncation - a full read reports that properly). `read_until` is ignored.
    pub fn open_file_deferring(
        self,
        path: impl AsRef<Path>,
        defer: &dyn Fn(&dcmnorm_core::header::DataElementHeader) -> bool,
        defer_pixel_sequences: bool,
    ) -> Result<Option<(DefaultDicomObject, crate::mem::DeferredValues)>, ReadError> {
        let file = File::open(path.as_ref()).map_err(|source| ReadError::Io {
            source,
            context: "opening file",
        })?;
        let file_len = file
            .metadata()
            .map_err(|source| ReadError::Io {
                source,
                context: "reading file metadata",
            })?
            .len();
        self.from_reader_deferring(
            BufReader::new(file),
            Some(file_len),
            defer,
            defer_pixel_sequences,
            false,
        )
    }

    /// [`Self::open_file_deferring`] over any seekable Part 10 source, positioned at its
    /// preamble: a byte slice in a `Cursor`, or a forward-only stream behind an adapter that
    /// implements forward `Seek` by reading and discarding (the reader only ever seeks forward,
    /// relative to its current position). `source_len`, when known, is checked against the
    /// deferred ranges as the file length is; with `None` that check is the caller's.
    ///
    /// With `stop_at_pixel_data`, nothing past the top-level PixelData header is read: PixelData
    /// is recorded as deferred where its value starts (length
    /// [`crate::DeferredValueLocation::UNDEFINED_LENGTH`] when encapsulated) and later elements
    /// are left out, as `dcm4che`'s `readDatasetUntilPixelData` does.
    pub fn from_reader_deferring<R: Read + std::io::Seek>(
        self,
        mut source: R,
        source_len: Option<u64>,
        defer: &dyn Fn(&dcmnorm_core::header::DataElementHeader) -> bool,
        defer_pixel_sequences: bool,
        stop_at_pixel_data: bool,
    ) -> Result<Option<(DefaultDicomObject, crate::mem::DeferredValues)>, ReadError> {
        let mut meta = self.read_meta(&mut source)?;
        let ts = meta
            .transfer_syntax_ts()
            .ok_or_else(|| ReadError::UnsupportedTransferSyntax {
                uid: meta.transfer_syntax.clone(),
            })?;
        if matches!(
            ts.codec(),
            dcmnorm_encoding::transfer_syntax::Codec::Dataset(_)
        ) {
            return Ok(None);
        }
        let base_offset = source.stream_position().map_err(|source| ReadError::Io {
            source,
            context: "locating data set",
        })?;

        let mut reader = dcmnorm_parser::dataset::DataSetReader::new_with_ts(source, ts)
            .map_err(|source| ReadError::Dataset { source })?;
        let (object, deferred) = crate::mem::build_dataset_deferring(
            &mut reader,
            false,
            defer,
            defer_pixel_sequences,
            stop_at_pixel_data,
            base_offset,
        )?;
        if source_len.is_some_and(|len| deferred.max_end() > len) {
            return Ok(None);
        }
        backfill_media_storage_uids(&mut meta, &object);
        Ok(Some((FileDicomObject { meta, object }, deferred)))
    }

    /// Read `path` in full except for top-level `(7FE0,0010) PixelData`, which is left on disk
    /// and described by [`crate::DeferredPixelData`] (native offset/length, or encapsulated
    /// Basic Offset Table plus per-fragment offsets/lengths). Every other element - including
    /// nested `IconImageSequence` pixels and trailing elements after `PixelData` - is loaded
    /// normally.
    ///
    /// Returns `Ok(None)` when the file cannot be read this way (dataset-compressed / deflated
    /// transfer syntax, or a deferred range running past EOF) so the caller can fall back to a
    /// full read.
    pub fn open_file_deferring_pixel_data(
        self,
        path: impl AsRef<Path>,
    ) -> Result<Option<(DefaultDicomObject, crate::mem::DeferredPixelData, File)>, ReadError> {
        use std::io::Seek;

        let mut file = File::open(path.as_ref()).map_err(|source| ReadError::Io {
            source,
            context: "opening file",
        })?;
        let file_len = file
            .metadata()
            .map_err(|source| ReadError::Io {
                source,
                context: "reading file metadata",
            })?
            .len();
        let (mut meta, object, mut pixel_data, is_encapsulated_ts) = {
            let mut source = BufReader::new(&mut file);
            let meta = self.read_meta(&mut source)?;
            let ts =
                meta.transfer_syntax_ts()
                    .ok_or_else(|| ReadError::UnsupportedTransferSyntax {
                        uid: meta.transfer_syntax.clone(),
                    })?;
            if matches!(
                ts.codec(),
                dcmnorm_encoding::transfer_syntax::Codec::Dataset(_)
            ) {
                return Ok(None);
            }
            let base_offset = source.stream_position().map_err(|source| ReadError::Io {
                source,
                context: "locating data set",
            })?;

            let mut reader = dcmnorm_parser::dataset::DataSetReader::new_with_ts(source, ts)
                .map_err(|source| ReadError::Dataset { source })?;
            let (object, pixel_data) =
                crate::mem::build_dataset_deferring_pixel_data(&mut reader, base_offset)?;
            if pixel_data.max_end() > file_len {
                return Err(ReadError::Io {
                    source: std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "deferred PixelData extends past end of file",
                    ),
                    context: "validating PixelData bounds",
                });
            }
            (meta, object, pixel_data, ts.is_encapsulated_pixel_data())
        };
        if is_encapsulated_ts {
            if let crate::mem::DeferredPixelData::Native { offset, length, .. } = pixel_data {
                if let Some(encapsulated) = try_parse_defined_length_encapsulated_file(
                    &mut file,
                    offset,
                    u64::from(length),
                )? {
                    pixel_data = encapsulated;
                }
            }
        }
        backfill_media_storage_uids(&mut meta, &object);
        Ok(Some((FileDicomObject { meta, object }, pixel_data, file)))
    }

    fn read_meta(&self, mut source: impl Read) -> Result<FileMetaTable, ReadError> {
        Ok(match self.read_preamble {
            // Peek-and-detect: falls back to treating `source` as meta-less if the preamble or
            // "DICM" magic aren't there.
            ReadPreamble::Auto => {
                FileMetaTable::read_from(&mut source)?.ok_or(ReadError::NotDicom)?
            }
            // No detection/fallback: the preamble and magic are required, and their absence (or
            // mismatch) is a hard error rather than a silent reinterpretation as meta-less -
            // matching what a caller who explicitly opted out of auto-detection would expect.
            ReadPreamble::Always => {
                let mut preamble = [0u8; PREAMBLE_LEN];
                source
                    .read_exact(&mut preamble)
                    .map_err(|source| ReadError::Io {
                        source,
                        context: "reading 128-byte preamble",
                    })?;
                let mut magic = [0u8; 4];
                source
                    .read_exact(&mut magic)
                    .map_err(|source| ReadError::Io {
                        source,
                        context: "reading \"DICM\" magic",
                    })?;
                if &magic != DICM_MAGIC {
                    return Err(ReadError::NotDicom);
                }
                FileMetaTable::read_meta_group(&mut source)?
            }
        })
    }
}

/// Where a native PixelData value sits in a file, as found by
/// [`OpenFileOptions::open_file_until_pixel_data`]: its declared VR and byte length, and the
/// absolute byte offset of its first value byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelDataValueLocation {
    pub vr: dcmnorm_core::VR,
    pub length: u32,
    pub offset: u64,
}

/// Counts bytes handed to the parser. Both the meta reader and the data set reader consume
/// exactly what they parse (no read-ahead past the element in hand), so the count after
/// stopping at PixelData's header is the file offset of its value.
struct CountingReader<R> {
    inner: R,
    count: u64,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.count += read as u64;
        Ok(read)
    }
}

struct CountingWriter<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.count += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

const SOP_CLASS_UID: dcmnorm_core::Tag = dcmnorm_core::Tag(0x0008, 0x0016);
const SOP_INSTANCE_UID: dcmnorm_core::Tag = dcmnorm_core::Tag(0x0008, 0x0018);

/// Some non-conformant DICOM writers (seen from fo-dicom-generated files) omit
/// MediaStorageSOPClassUID/MediaStorageSOPInstanceUID (0002,0002)/(0002,0003) from the file
/// meta group entirely, even though they're Type 1 per PS3.10 - `dicom-object`'s reader
/// tolerated this by backfilling them from the data set's own SOPClassUID/SOPInstanceUID
/// (0008,0016)/(0008,0018), which are almost always present redundantly. Preserved here for
/// parity - confirmed against a real fixture (`test/files/sr.dcm`, written by fo-dicom 4.0.7).
fn backfill_media_storage_uids(meta: &mut FileMetaTable, object: &InMemDicomObject) {
    if meta.media_storage_sop_class_uid.is_empty() {
        if let Some(uid) = object.get(SOP_CLASS_UID).and_then(|e| e.to_str().ok()) {
            meta.media_storage_sop_class_uid = uid.trim_end_matches(['\0', ' ']).to_owned();
        }
    }
    if meta.media_storage_sop_instance_uid.is_empty() {
        if let Some(uid) = object.get(SOP_INSTANCE_UID).and_then(|e| e.to_str().ok()) {
            meta.media_storage_sop_instance_uid = uid.trim_end_matches(['\0', ' ']).to_owned();
        }
    }
}

/// Read a bare data set (no meta group) and attach the given meta table to it. Used for
/// `dcmnorm`'s meta-less/preamble-less trial-parse fallback path.
pub fn with_meta_from_bare_dataset(
    object: InMemDicomObject,
    meta: FileMetaTable,
) -> DefaultDicomObject {
    FileDicomObject { meta, object }
}

/// Read raw bytes as a bare data set, trying Implicit VR LE, then Explicit VR LE, then
/// Explicit VR BE in that order and taking the first one that parses successfully - the same
/// permissive fallback `dcmnorm` already relies on for meta-less raw dumps.
pub fn read_dataset_trial_parse(bytes: &[u8]) -> Option<(InMemDicomObject, &'static str)> {
    const CANDIDATES: &[&str] = &[
        dcmnorm_dictionary::uids::IMPLICIT_VR_LITTLE_ENDIAN,
        dcmnorm_dictionary::uids::EXPLICIT_VR_LITTLE_ENDIAN,
        "1.2.840.10008.1.2.2",
    ];
    for uid in CANDIDATES {
        let Some(ts) = TransferSyntaxRegistry.get(uid) else {
            continue;
        };
        if let Ok(object) = InMemDicomObject::read_dataset_with_ts(Cursor::new(bytes), ts) {
            return Some((object, uid));
        }
    }
    None
}

/// Some non-conformant encoders write an encapsulated transfer syntax's `(7FE0,0010) PixelData`
/// with a defined element length instead of `0xFFFFFFFF`, while still framing its payload as a
/// Basic Offset Table item `(FFFE,E000)`, one or more fragment items `(FFFE,E000)`, and an
/// optional Sequence Delimitation Item `(FFFE,E0DD)`. When an in-memory read encounters that
/// layout, convert the primitive byte buffer into a `PixelFragmentSequence` so downstream
/// transcoders and frame readers see normal encapsulated fragments.
fn normalize_defined_length_encapsulated_pixel_data(object: &mut InMemDicomObject) {
    const PIXEL_DATA: dcmnorm_core::Tag = dcmnorm_core::Tag(0x7FE0, 0x0010);
    let Some(elem) = object.get(PIXEL_DATA) else {
        return;
    };
    if elem.value().fragments().is_some() {
        return;
    }
    let Ok(bytes) = elem.value().to_bytes() else {
        return;
    };
    let Some((offset_table, fragments)) = parse_defined_length_encapsulated_bytes(&bytes) else {
        return;
    };
    object.put(dcmnorm_core::DataElement::new(
        PIXEL_DATA,
        dcmnorm_core::VR::OB,
        dcmnorm_core::value::Value::PixelSequence(dcmnorm_core::value::PixelFragmentSequence::new(
            offset_table,
            fragments,
        )),
    ));
}

fn parse_defined_length_encapsulated_bytes(bytes: &[u8]) -> Option<(Vec<u32>, Vec<Vec<u8>>)> {
    const ITEM_TAG: [u8; 4] = [0xFE, 0xFF, 0x00, 0xE0];
    const SEQ_DELIM_TAG: [u8; 4] = [0xFE, 0xFF, 0xDD, 0xE0];
    if bytes.len() < 8 || bytes[..4] != ITEM_TAG {
        return None;
    }
    let bot_len = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
    if !bot_len.is_multiple_of(4) || 8usize.checked_add(bot_len)? > bytes.len() {
        return None;
    }
    let offset_table = bytes[8..8 + bot_len]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect();
    let mut pos = 8 + bot_len;
    let mut fragments = Vec::new();
    while pos < bytes.len() {
        if pos + 8 > bytes.len() {
            return None;
        }
        let tag = &bytes[pos..pos + 4];
        let item_len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        pos += 8;
        if tag == SEQ_DELIM_TAG && item_len == 0 {
            break;
        }
        if tag != ITEM_TAG {
            return None;
        }
        let end = pos.checked_add(item_len)?;
        if end > bytes.len() {
            return None;
        }
        fragments.push(bytes[pos..end].to_vec());
        pos = end;
    }
    if fragments.is_empty() {
        return None;
    }
    Some((offset_table, fragments))
}

fn try_parse_defined_length_encapsulated_file(
    file: &mut File,
    offset: u64,
    length: u64,
) -> Result<Option<crate::mem::DeferredPixelData>, ReadError> {
    use std::io::Seek;

    const ITEM_TAG: [u8; 4] = [0xFE, 0xFF, 0x00, 0xE0];
    const SEQ_DELIM_TAG: [u8; 4] = [0xFE, 0xFF, 0xDD, 0xE0];

    if length < 8 {
        return Ok(None);
    }
    let end = offset.saturating_add(length);
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|source| ReadError::Io {
            source,
            context: "seeking to defined-length encapsulated PixelData",
        })?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)
        .map_err(|source| ReadError::Io {
            source,
            context: "reading defined-length encapsulated PixelData BOT header",
        })?;
    if header[..4] != ITEM_TAG {
        return Ok(None);
    }
    let bot_len = u64::from(u32::from_le_bytes(header[4..8].try_into().unwrap()));
    if !bot_len.is_multiple_of(4) || offset + 8 + bot_len > end {
        return Ok(None);
    }
    let mut bot_bytes = vec![0u8; bot_len as usize];
    if !bot_bytes.is_empty() {
        file.read_exact(&mut bot_bytes)
            .map_err(|source| ReadError::Io {
                source,
                context: "reading defined-length encapsulated PixelData BOT",
            })?;
    }
    let offset_table = bot_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect();
    let mut pos = offset + 8 + bot_len;
    let mut fragments = Vec::new();
    while pos < end {
        if pos + 8 > end {
            return Ok(None);
        }
        file.read_exact(&mut header)
            .map_err(|source| ReadError::Io {
                source,
                context: "reading defined-length encapsulated PixelData item header",
            })?;
        let item_len = u32::from_le_bytes(header[4..8].try_into().unwrap());
        pos += 8;
        if header[..4] == SEQ_DELIM_TAG && item_len == 0 {
            break;
        }
        if header[..4] != ITEM_TAG {
            return Ok(None);
        }
        let next_pos = pos.saturating_add(u64::from(item_len));
        if next_pos > end {
            return Ok(None);
        }
        fragments.push(crate::mem::FragmentLocation {
            offset: pos,
            length: item_len,
        });
        pos = next_pos;
        file.seek(std::io::SeekFrom::Start(pos))
            .map_err(|source| ReadError::Io {
                source,
                context: "skipping defined-length encapsulated PixelData fragment",
            })?;
    }
    if fragments.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::mem::DeferredPixelData::Encapsulated {
        offset_table,
        fragments,
        end_offset: end,
    }))
}

#[cfg(test)]
mod tests {
    use super::{OpenFileOptions, ReadPreamble};
    use dcmnorm_dictionary::tags;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("test")
            .join("files")
            .join(name)
    }

    #[test]
    fn read_preamble_always_accepts_a_real_part10_file() {
        let object = OpenFileOptions::new()
            .read_preamble(ReadPreamble::Always)
            .open_file(fixture("mr_small.dcm"))
            .expect("a real Part 10 file has a preamble and DICM magic");
        assert_eq!(
            object
                .get(tags::MODALITY)
                .and_then(|e| e.to_str().ok())
                .as_deref(),
            Some("MR")
        );
    }

    #[test]
    fn read_preamble_always_rejects_a_file_with_no_preamble() {
        // nometa_explicit_le.dcm is a bare data set: no 128-byte preamble, no "DICM" magic.
        // ReadPreamble::Always must not silently fall back to auto-detection - it should fail
        // cleanly instead of misinterpreting the first bytes of the data set as a preamble.
        let result = OpenFileOptions::new()
            .read_preamble(ReadPreamble::Always)
            .open_file(fixture("nometa_explicit_le.dcm"));
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn read_preamble_auto_does_not_fall_back_to_bare_dataset_parsing() {
        // OpenFileOptions::from_reader/open_file never did meta-less fallback (that's a
        // separate, explicit opt-in via read_dataset_trial_parse) - Auto only controls whether
        // detection happens, not whether a failed detection is tolerated.
        let result = OpenFileOptions::new().open_file(fixture("nometa_explicit_le.dcm"));
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn explicit_vr_big_endian_file_reads_with_correct_values() {
        let object = OpenFileOptions::new()
            .open_file(fixture("explicit_vr_be.dcm"))
            .expect("should read");
        assert_eq!(
            object.meta().transfer_syntax.trim_end_matches('\0'),
            "1.2.840.10008.1.2.2"
        );
        assert_eq!(
            object
                .get(tags::MODALITY)
                .and_then(|e| e.to_str().ok())
                .as_deref(),
            Some("US")
        );
    }

    #[test]
    fn defined_length_encapsulated_pixel_data_parses_in_memory_and_deferred() {
        use crate::DeferredPixelData;
        use dcmnorm_core::{DataElement, PrimitiveValue, VR};

        // Build a minimal Part 10 JPEG Lossless file whose (7FE0,0010) uses a defined length
        // wrapping standard encapsulated item framing (BOT + fragment + Sequence Delimiter).
        let mut obj = crate::InMemDicomObject::new_empty();
        obj.put(DataElement::new(
            tags::SOP_CLASS_UID,
            VR::UI,
            PrimitiveValue::from("1.2.840.10008.5.1.4.1.1.2"),
        ));
        obj.put(DataElement::new(
            tags::SOP_INSTANCE_UID,
            VR::UI,
            PrimitiveValue::from("1.2.826.0.1.3680043.8.498.1"),
        ));
        let mut encapsulated_payload = Vec::new();
        // Empty BOT item (FFFE,E000) len=0
        encapsulated_payload.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0, 0, 0, 0]);
        // Fragment item (FFFE,E000) len=4: [0xFF, 0xD8, 0xFF, 0xD9]
        encapsulated_payload.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 4, 0, 0, 0]);
        encapsulated_payload.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xD9]);
        // Sequence Delimitation Item (FFFE,E0DD) len=0
        encapsulated_payload.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0]);
        obj.put(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PrimitiveValue::U8(encapsulated_payload.into()),
        ));

        let file_obj = obj
            .with_meta(
                crate::FileMetaTableBuilder::new()
                    .transfer_syntax("1.2.840.10008.1.2.4.70")
                    .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.2")
                    .media_storage_sop_instance_uid("1.2.826.0.1.3680043.8.498.1"),
            )
            .unwrap();
        let mut bytes = Vec::new();
        file_obj.write_all(&mut bytes).unwrap();

        // In-memory read normalizes defined-length encapsulated PixelData to fragments.
        let read = OpenFileOptions::new()
            .from_reader(std::io::Cursor::new(&bytes))
            .expect("in-memory read should succeed");
        let frags = read
            .element(tags::PIXEL_DATA)
            .unwrap()
            .fragments()
            .expect("should normalize to PixelFragmentSequence");
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0], vec![0xFF, 0xD8, 0xFF, 0xD9]);

        // Deferred read reports DeferredPixelData::Encapsulated with exact fragment offset/length.
        let tmp = std::env::temp_dir().join("dcmnorm_defined_len_encaps_test.dcm");
        std::fs::write(&tmp, &bytes).unwrap();
        let (_, deferred, mut file) = OpenFileOptions::new()
            .open_file_deferring_pixel_data(&tmp)
            .expect("deferred read should succeed")
            .expect("should defer");
        let _ = std::fs::remove_file(&tmp);
        match deferred {
            DeferredPixelData::Encapsulated {
                offset_table,
                fragments,
                ..
            } => {
                assert!(offset_table.is_empty());
                assert_eq!(fragments.len(), 1);
                assert_eq!(fragments[0].length, 4);
                use std::io::{Read, Seek, SeekFrom};
                file.seek(SeekFrom::Start(fragments[0].offset)).unwrap();
                let mut buf = [0u8; 4];
                file.read_exact(&mut buf).unwrap();
                assert_eq!(buf, [0xFF, 0xD8, 0xFF, 0xD9]);
            }
            other => panic!("expected DeferredPixelData::Encapsulated, got {other:?}"),
        }
    }
}
