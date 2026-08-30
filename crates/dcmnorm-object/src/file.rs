//! Whole-file DICOM objects: a [`FileMetaTable`] plus an object (normally [`InMemDicomObject`]),
//! and the [`OpenFileOptions`] builder for reading them from disk/a byte source.

use std::fs::File;
use std::io::{BufReader, Cursor, Read, Write};
use std::path::Path;

use dcmnorm_encoding::transfer_syntax::TransferSyntaxIndex;
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
        FileDicomObject { meta, object: InMemDicomObject::new_empty() }
    }

    /// Insert or replace a text element by tag and VR, creating it if absent.
    pub fn put_str(&mut self, tag: dcmnorm_core::Tag, vr: dcmnorm_core::VR, value: impl Into<String>) {
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
        let ts = meta.transfer_syntax_ts().ok_or_else(|| ReadError::UnsupportedTransferSyntax {
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

    pub fn open_file(self, path: impl AsRef<Path>) -> Result<DefaultDicomObject, ReadError> {
        let file = File::open(path.as_ref()).map_err(|source| ReadError::Io {
            source,
            context: "opening file",
        })?;
        self.from_reader(BufReader::new(file))
    }

    pub fn from_reader(self, mut source: impl Read) -> Result<DefaultDicomObject, ReadError> {
        let mut meta = match self.read_preamble {
            // Peek-and-detect: falls back to treating `source` as meta-less if the preamble or
            // "DICM" magic aren't there.
            ReadPreamble::Auto => FileMetaTable::read_from(&mut source)?.ok_or(ReadError::NotDicom)?,
            // No detection/fallback: the preamble and magic are required, and their absence (or
            // mismatch) is a hard error rather than a silent reinterpretation as meta-less -
            // matching what a caller who explicitly opted out of auto-detection would expect.
            ReadPreamble::Always => {
                let mut preamble = [0u8; PREAMBLE_LEN];
                source.read_exact(&mut preamble).map_err(|source| ReadError::Io {
                    source,
                    context: "reading 128-byte preamble",
                })?;
                let mut magic = [0u8; 4];
                source.read_exact(&mut magic).map_err(|source| ReadError::Io {
                    source,
                    context: "reading \"DICM\" magic",
                })?;
                if &magic != DICM_MAGIC {
                    return Err(ReadError::NotDicom);
                }
                FileMetaTable::read_meta_group(&mut source)?
            }
        };
        let ts = meta.transfer_syntax_ts().ok_or_else(|| ReadError::UnsupportedTransferSyntax {
            uid: meta.transfer_syntax.clone(),
        })?;

        let object = if let Some(stop_tag) = self.read_until {
            crate::mem::read_dataset_until(source, ts, stop_tag)?
        } else {
            InMemDicomObject::read_dataset_with_ts(source, ts)?
        };
        backfill_media_storage_uids(&mut meta, &object);
        Ok(FileDicomObject { meta, object })
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
        let Some(ts) = TransferSyntaxRegistry.get(uid) else { continue };
        if let Ok(object) = InMemDicomObject::read_dataset_with_ts(Cursor::new(bytes), ts) {
            return Some((object, uid));
        }
    }
    None
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
        assert_eq!(object.get(tags::MODALITY).and_then(|e| e.to_str().ok()).as_deref(), Some("MR"));
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
        let object =
            OpenFileOptions::new().open_file(fixture("explicit_vr_be.dcm")).expect("should read");
        assert_eq!(object.meta().transfer_syntax.trim_end_matches('\0'), "1.2.840.10008.1.2.2");
        assert_eq!(object.get(tags::MODALITY).and_then(|e| e.to_str().ok()).as_deref(), Some("US"));
    }
}
