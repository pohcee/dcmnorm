//! DICOMDIR writer: builds the Basic Directory IOD (Media Storage Directory Storage,
//! `1.2.840.10008.1.3.10`) for a File-set from the headers of the files in it.
//!
//! Standards this follows (DICOM 2026d unless noted):
//!
//! - PS3.3 Annex F: the Basic Directory IOD. F.3.2.1 File-set Identification Module, F.3.2.2
//!   Directory Information Module (the record offsets and flags), F.4 Table F.4-1 (which record
//!   types may sit below which), F.5 (the keys of each record type).
//! - PS3.10 8.2 / 8.5 / 8.6: File IDs (1-8 components of 1-8 characters from `A-Z 0-9 _`), the
//!   same character set for the File-set ID, and the reserved File ID `DICOMDIR`.
//! - PS3.11 D.3.3 (STD-GEN-CD) and H.3.3 (STD-GEN-DVD) additional keys: Table D.3-2 and Table
//!   H.3-2. This writer always emits the H.3-2 keys too when the instances carry them: they are
//!   a superset of D.3-2 and harmless for a CD profile.
//! - IHE RAD TF-2 4.47.4.1.2.3.1 (PDI) Table 4.47.4-1: Type 1 record keys that are Type 2 or
//!   absent in the instances (Patient ID, Study ID, Study Date, Study Time, Modality, Series
//!   Number, Instance Number) must be synthesized, distinct per entity. Every synthesized value
//!   is reported back in [`DicomdirReport::synthesized`].
//!
//! Layout: the file is the usual Part 10 preamble + `DICM` + group 0002 meta (Explicit VR Little
//! Endian), then the four File-set elements and the Directory Record Sequence (0004,1220). Every
//! record is a defined-length item (nested sequences inside a record, e.g. Referenced Image
//! Sequence, are written with undefined length as everywhere else in dcmnorm; that is legal and
//! does not move any offset). Records are written depth first (a PATIENT, its first STUDY, that
//! study's first SERIES, the series' instances, the next SERIES, ...).
//!
//! Offsets: PS3.3 F.3.2.2 (Table F.3-3) defines (0004,1200), (0004,1202), (0004,1400) and
//! (0004,1420) as "Offset of the first byte (of the Item Data Element) of the ... Directory
//! Record ... This Offset is a number of bytes starting with the first byte of the File Meta
//! Information", with the note "This offset includes the File Preamble and the DICM Prefix". So
//! an offset is the absolute byte position of the record's (FFFE,E000) item tag in the DICOMDIR
//! file, counting the 128-byte preamble from 0. Every offset is a fixed-size UL, so the layout is
//! measured once with zero offsets, the offsets are computed from the measured item sizes, and
//! the records are encoded again with the real values (the second encoding must be the same
//! size, which is checked).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path};

use dcmnorm_core::header::{DataElement, Header, Length, Tag};
use dcmnorm_core::value::{DataSetSequence, PrimitiveValue, Value};
use dcmnorm_core::VR;
use dcmnorm_dictionary::uids;
use dcmnorm_encoding::transfer_syntax::TransferSyntaxIndex;
use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject, InMemElement};
use dcmnorm_transcode::TransferSyntaxRegistry;
use serde_json::Value as JsonValue;

use super::flat_json::read_flat_json_value;
use super::standard_json::read_standard_json_value;
use super::types::{DicomJsonError, DicomJsonFormat, ReadError, WriteError};
use super::volume_export::generate_uid;

/// SOP Class UID of the DICOMDIR itself: Media Storage Directory Storage (PS3.4 Annex I).
pub const MEDIA_STORAGE_DIRECTORY_STORAGE: &str = "1.2.840.10008.1.3.10";

/// Maximum File ID components (PS3.10 8.2).
pub const MAX_FILE_ID_COMPONENTS: usize = 8;
/// Maximum characters per File ID component (PS3.10 8.2).
pub const MAX_FILE_ID_COMPONENT_LEN: usize = 8;
/// Maximum File-set ID length: it is a CS value (PS3.3 F.3.2.1, PS3.5 CS = 16 characters).
pub const MAX_FILE_SET_ID_LEN: usize = 16;

/// Character set written into records that hold non-ASCII text. Strings are held as Unicode in
/// memory whatever the source's character set was, so the record is encoded in UTF-8 rather than
/// re-encoded into a possibly ISO 2022 source repertoire.
const UTF8_CHARACTER_SET: &str = "ISO_IR 192";

const ITEM_TAG_BYTES: [u8; 4] = [0xFE, 0xFF, 0x00, 0xE0];

// --- tags used directly (group 0004 and the record keys) -----------------------------------

const FILE_SET_ID: Tag = Tag(0x0004, 0x1130);
const OFFSET_FIRST_ROOT: Tag = Tag(0x0004, 0x1200);
const OFFSET_LAST_ROOT: Tag = Tag(0x0004, 0x1202);
const FILE_SET_CONSISTENCY_FLAG: Tag = Tag(0x0004, 0x1212);
const DIRECTORY_RECORD_SEQUENCE: Tag = Tag(0x0004, 0x1220);
const OFFSET_NEXT: Tag = Tag(0x0004, 0x1400);
const RECORD_IN_USE_FLAG: Tag = Tag(0x0004, 0x1410);
const OFFSET_LOWER: Tag = Tag(0x0004, 0x1420);
const DIRECTORY_RECORD_TYPE: Tag = Tag(0x0004, 0x1430);
const REFERENCED_FILE_ID: Tag = Tag(0x0004, 0x1500);
const REFERENCED_SOP_CLASS_UID_IN_FILE: Tag = Tag(0x0004, 0x1510);
const REFERENCED_SOP_INSTANCE_UID_IN_FILE: Tag = Tag(0x0004, 0x1511);
const REFERENCED_TRANSFER_SYNTAX_UID_IN_FILE: Tag = Tag(0x0004, 0x1512);
const REFERENCED_RELATED_GENERAL_SOP_CLASS_UID_IN_FILE: Tag = Tag(0x0004, 0x151A);

const SPECIFIC_CHARACTER_SET: Tag = Tag(0x0008, 0x0005);
const INSTANCE_CREATION_DATE: Tag = Tag(0x0008, 0x0012);
const INSTANCE_CREATION_TIME: Tag = Tag(0x0008, 0x0013);
const SOP_CLASS_UID: Tag = Tag(0x0008, 0x0016);
const SOP_INSTANCE_UID: Tag = Tag(0x0008, 0x0018);
const RELATED_GENERAL_SOP_CLASS_UID: Tag = Tag(0x0008, 0x001A);
const STUDY_DATE: Tag = Tag(0x0008, 0x0020);
const SERIES_DATE: Tag = Tag(0x0008, 0x0021);
const ACQUISITION_DATE: Tag = Tag(0x0008, 0x0022);
const CONTENT_DATE: Tag = Tag(0x0008, 0x0023);
const STUDY_TIME: Tag = Tag(0x0008, 0x0030);
const SERIES_TIME: Tag = Tag(0x0008, 0x0031);
const ACQUISITION_TIME: Tag = Tag(0x0008, 0x0032);
const CONTENT_TIME: Tag = Tag(0x0008, 0x0033);
const MODALITY: Tag = Tag(0x0008, 0x0060);
const REFERENCED_IMAGE_SEQUENCE: Tag = Tag(0x0008, 0x1140);
const PATIENT_NAME: Tag = Tag(0x0010, 0x0010);
const PATIENT_ID: Tag = Tag(0x0010, 0x0020);
const PATIENT_BIRTH_DATE: Tag = Tag(0x0010, 0x0030);
const STUDY_INSTANCE_UID: Tag = Tag(0x0020, 0x000D);
const SERIES_INSTANCE_UID: Tag = Tag(0x0020, 0x000E);
const STUDY_ID: Tag = Tag(0x0020, 0x0010);
const SERIES_NUMBER: Tag = Tag(0x0020, 0x0011);
const INSTANCE_NUMBER: Tag = Tag(0x0020, 0x0013);
const IMAGE_POSITION_PATIENT: Tag = Tag(0x0020, 0x0032);
const IMAGE_ORIENTATION_PATIENT: Tag = Tag(0x0020, 0x0037);
const ROWS: Tag = Tag(0x0028, 0x0010);
const COLUMNS: Tag = Tag(0x0028, 0x0011);
const PIXEL_SPACING: Tag = Tag(0x0028, 0x0030);
const VERIFICATION_DATE_TIME: Tag = Tag(0x0040, 0xA030);
const VERIFYING_OBSERVER_SEQUENCE: Tag = Tag(0x0040, 0xA073);
const RELATIONSHIP_TYPE: Tag = Tag(0x0040, 0xA010);
const VERIFICATION_FLAG: Tag = Tag(0x0040, 0xA493);
const CONTENT_SEQUENCE: Tag = Tag(0x0040, 0xA730);
const SHARED_FUNCTIONAL_GROUPS_SEQUENCE: Tag = Tag(0x5200, 0x9229);

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// One file of the File-set: where it is (its File ID) and what it is (its header).
#[derive(Debug, Clone)]
pub struct DicomdirEntry {
    /// The File ID components, e.g. `["DICOM", "ST000001", "SE000001", "IM000001"]`.
    pub file_id: Vec<String>,
    /// The referenced file's data set (bulk data not needed; group 0002 ignored).
    pub dataset: InMemDicomObject,
    /// Transfer Syntax UID the referenced file is encoded in (Referenced Transfer Syntax UID in
    /// File, (0004,1512)).
    pub transfer_syntax_uid: String,
    /// SOP Class UID of the referenced file; when `None`, the data set's (0008,0016).
    pub sop_class_uid: Option<String>,
    /// SOP Instance UID of the referenced file; when `None`, the data set's (0008,0018).
    pub sop_instance_uid: Option<String>,
}

impl DicomdirEntry {
    /// An entry from an already-read file: the data set, plus the transfer syntax and (when the
    /// data set lacks them) the SOP Class/Instance UIDs from its File Meta Information.
    pub fn from_file_object(file_id: Vec<String>, object: &DefaultDicomObject) -> Self {
        let meta = object.meta();
        let non_empty = |s: &str| {
            let s = trim_value(s);
            (!s.is_empty()).then(|| s.to_owned())
        };
        let dataset: InMemDicomObject = (**object).clone();
        let sop_class_uid = get_str(&dataset, SOP_CLASS_UID).or_else(|| non_empty(meta.media_storage_sop_class_uid()));
        let sop_instance_uid =
            get_str(&dataset, SOP_INSTANCE_UID).or_else(|| non_empty(meta.media_storage_sop_instance_uid()));
        DicomdirEntry {
            file_id,
            dataset,
            transfer_syntax_uid: trim_value(meta.transfer_syntax()).to_owned(),
            sop_class_uid,
            sop_instance_uid,
        }
    }

    /// An entry from a file on disk. Pixel data and the other large binary payloads (overlay,
    /// waveform and encapsulated document data, any OB/OW/OF/OD/OL/OV value over 1 KiB) are
    /// skipped while reading, not loaded. UN values are always read: a file whose keys are
    /// encoded as UN still has them.
    pub fn from_file(file_id: Vec<String>, path: impl AsRef<Path>) -> Result<Self, DicomdirError> {
        let path = path.as_ref();
        let read_error = |source| DicomdirError::Read { path: path.display().to_string(), source };
        let defer = |header: &dcmnorm_core::header::DataElementHeader| {
            let tag = header.tag;
            let payload = tag.group() >= 0x7FE0
                || tag == Tag(0x0042, 0x0011)
                || tag == Tag(0x5400, 0x1010)
                || (tag.group() & 0xFF01 == 0x6000 && tag.element() == 0x3000);
            let large_binary = matches!(header.vr, VR::OB | VR::OW | VR::OF | VR::OD | VR::OL | VR::OV)
                && header.len.get().is_some_and(|len| len > 1024);
            payload || large_binary
        };
        let deferred = dcmnorm_object::OpenFileOptions::new()
            .read_preamble(dcmnorm_object::ReadPreamble::Always)
            .open_file_deferring(path, &defer, true);
        let object = match deferred {
            Ok(Some((object, _locations))) => object,
            // A deflated data set or one whose deferred ranges can't be trusted: read it all.
            Ok(None) => super::io::read_dicom_file(path).map_err(read_error)?,
            Err(source) => return Err(read_error(source)),
        };
        Ok(Self::from_file_object(file_id, &object))
    }

    /// An entry from the referenced file's DICOM JSON (dcmnorm's flat format or PS3.18 Annex F
    /// standard JSON, as produced by `dcmnorm file.dcm` / `--format standard`). Elements given
    /// only as a `BulkDataURI` are dropped (a directory record never needs them, and the URI
    /// can't be resolved without the source file). The transfer syntax is `transfer_syntax_uid`
    /// when given, else the JSON's (0002,0010), else Explicit VR Little Endian.
    pub fn from_json(
        file_id: Vec<String>,
        json: &str,
        format: DicomJsonFormat,
        transfer_syntax_uid: Option<&str>,
    ) -> Result<Self, DicomdirError> {
        let mut value: JsonValue = serde_json::from_str(json).map_err(|e| DicomdirError::Json(DicomJsonError::from(e)))?;
        strip_bulk_data_uris(&mut value);
        let object = match format {
            DicomJsonFormat::Flat => read_flat_json_value(&value, None),
            DicomJsonFormat::Standard => read_standard_json_value(&value, None),
        }
        .map_err(DicomdirError::Json)?;
        let mut entry = Self::from_file_object(file_id, &object);
        if let Some(ts) = transfer_syntax_uid.map(trim_value).filter(|ts| !ts.is_empty()) {
            entry.transfer_syntax_uid = ts.to_owned();
        }
        Ok(entry)
    }
}

/// Options for [`build_dicomdir`] / [`write_dicomdir`].
#[derive(Debug, Clone, Default)]
pub struct DicomdirOptions {
    /// File-set ID (0004,1130): up to 16 characters from `A-Z 0-9 _` (PS3.10 8.5), or empty.
    pub file_set_id: String,
    /// SOP Instance UID of the DICOMDIR (Media Storage SOP Instance UID). Generated (`2.25.`
    /// UUID) when `None`; set it for reproducible output.
    pub sop_instance_uid: Option<String>,
}

/// A value the writer had to make up for a Type 1 key the instances didn't carry (IHE RAD TF-2
/// Table 4.47.4-1), or derived from another attribute of the instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DicomdirSynthesized {
    /// Directory Record Type of the record that holds it, e.g. `"STUDY"`.
    pub record_type: String,
    /// What identifies the entity: the Study/Series/SOP Instance UID; for a PATIENT record (the
    /// Patient ID being what was missing) the patient's name, or "patient of study <UID>".
    pub entity: String,
    /// Attribute keyword, e.g. `"StudyID"`.
    pub attribute: String,
    /// The value written.
    pub value: String,
    /// Keyword of the instance attribute the value was copied from (e.g. `"SeriesDate"` for a
    /// missing Study Date), or `None` when it was made up (a counter, `OT`, today's date).
    pub source: Option<String>,
}

/// What [`build_dicomdir`] / [`write_dicomdir`] produced.
#[derive(Debug, Clone, Default)]
pub struct DicomdirReport {
    /// Total Directory Records written.
    pub record_count: usize,
    /// Records per Directory Record Type, in first-seen order.
    pub record_types: Vec<(String, usize)>,
    /// Values synthesized for missing Type 1 keys.
    pub synthesized: Vec<DicomdirSynthesized>,
    /// Anything else worth a look (a Type 1 key missing that can't sensibly be made up, ...).
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum DicomdirError {
    /// No entries: PS3.11 D.3.3 doesn't allow a DICOMDIR without directory information.
    NoEntries,
    InvalidFileId { file_id: String, reason: String },
    InvalidFileSetId { file_set_id: String, reason: String },
    DuplicateFileId(String),
    DuplicateSopInstance { sop_instance_uid: String, file_ids: (String, String) },
    /// A Type 1 identifying attribute that can't be synthesized (a UID) is missing.
    MissingAttribute { file_id: String, attribute: &'static str },
    /// The SOP Class has no Directory Record Type in PS3.3 F.5 and doesn't look like an image.
    NoRecordType { file_id: String, sop_class_uid: String },
    /// Two different patients share a Patient ID (PS3.3 F.5.1: unique per File-set).
    PatientConflict { patient_id: String, names: (String, String) },
    /// A Study Instance UID appears under two patients.
    StudyConflict { study_instance_uid: String },
    /// A Series Instance UID appears in two studies.
    SeriesConflict { series_instance_uid: String },
    Read { path: String, source: ReadError },
    Json(DicomJsonError),
    Write(WriteError),
    Io(std::io::Error),
    /// The file tree handed to [`collect_dicomdir_entries`] has a problem (not a directory, ...).
    Tree(String),
}

impl fmt::Display for DicomdirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoEntries => write!(f, "a DICOMDIR needs at least one referenced file (PS3.11 D.3.3)"),
            Self::InvalidFileId { file_id, reason } => write!(f, "invalid File ID {file_id:?}: {reason}"),
            Self::InvalidFileSetId { file_set_id, reason } => {
                write!(f, "invalid File-set ID {file_set_id:?}: {reason}")
            }
            Self::DuplicateFileId(id) => write!(f, "File ID {id} is referenced twice"),
            Self::DuplicateSopInstance { sop_instance_uid, file_ids } => write!(
                f,
                "SOP Instance UID {sop_instance_uid} is in two files ({} and {})",
                file_ids.0, file_ids.1
            ),
            Self::MissingAttribute { file_id, attribute } => {
                write!(f, "{file_id}: {attribute} is missing or empty, and a DICOMDIR can't be built without it")
            }
            Self::NoRecordType { file_id, sop_class_uid } => write!(
                f,
                "{file_id}: SOP Class {sop_class_uid} has no Directory Record Type (PS3.3 F.5) and is not an image"
            ),
            Self::PatientConflict { patient_id, names } => write!(
                f,
                "Patient ID {patient_id:?} is used by two different patients ({:?} and {:?}); a File-set needs one Patient ID per patient (PS3.3 F.5.1)",
                names.0, names.1
            ),
            Self::StudyConflict { study_instance_uid } => write!(
                f,
                "the instances of study {study_instance_uid} name different patients (Patient ID, or name and birth date when there is no ID)"
            ),
            Self::SeriesConflict { series_instance_uid } => {
                write!(f, "Series Instance UID {series_instance_uid} appears in two different studies")
            }
            Self::Read { path, source } => write!(f, "{path}: {source}"),
            Self::Json(e) => write!(f, "invalid DICOM JSON: {e}"),
            Self::Write(e) => write!(f, "failed to write DICOMDIR: {e}"),
            Self::Io(e) => write!(f, "failed to write DICOMDIR: {e}"),
            Self::Tree(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DicomdirError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Json(e) => Some(e),
            Self::Write(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Check a File ID against PS3.10 8.2/8.5/8.6: 1-8 components, each 1-8 characters from
/// `A-Z 0-9 _`, and not the reserved File ID `DICOMDIR`.
pub fn validate_file_id<S: AsRef<str>>(components: &[S]) -> Result<(), DicomdirError> {
    let joined = || components.iter().map(|c| c.as_ref()).collect::<Vec<_>>().join("\\");
    let invalid = |reason: String| DicomdirError::InvalidFileId { file_id: joined(), reason };
    if components.is_empty() {
        return Err(invalid("a File ID needs at least one component".into()));
    }
    if components.len() > MAX_FILE_ID_COMPONENTS {
        return Err(invalid(format!(
            "{} components, at most {MAX_FILE_ID_COMPONENTS} allowed (PS3.10 8.2)",
            components.len()
        )));
    }
    for component in components {
        let component = component.as_ref();
        if component.is_empty() || component.len() > MAX_FILE_ID_COMPONENT_LEN {
            return Err(invalid(format!(
                "component {component:?} must be 1-{MAX_FILE_ID_COMPONENT_LEN} characters (PS3.10 8.2)"
            )));
        }
        if let Some(c) = component.chars().find(|c| !is_file_id_char(*c)) {
            return Err(invalid(format!(
                "component {component:?} has {c:?}; only A-Z, 0-9 and _ are allowed (PS3.10 8.5), so no lower case and no extension"
            )));
        }
    }
    if components.len() == 1 && components[0].as_ref() == "DICOMDIR" {
        return Err(invalid("DICOMDIR is the reserved File ID of the directory itself (PS3.10 8.6)".into()));
    }
    Ok(())
}

/// Check a File-set ID: a CS value of at most 16 characters from the PS3.10 8.5 set
/// (`A-Z 0-9 _`). Empty is allowed (File-set ID is Type 2).
pub fn validate_file_set_id(file_set_id: &str) -> Result<(), DicomdirError> {
    let invalid = |reason: String| DicomdirError::InvalidFileSetId { file_set_id: file_set_id.to_owned(), reason };
    if file_set_id.len() > MAX_FILE_SET_ID_LEN {
        return Err(invalid(format!("at most {MAX_FILE_SET_ID_LEN} characters")));
    }
    if let Some(c) = file_set_id.chars().find(|c| !is_file_id_char(*c)) {
        return Err(invalid(format!("{c:?} is not allowed; only A-Z, 0-9 and _ (PS3.10 8.5)")));
    }
    Ok(())
}

fn is_file_id_char(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
}

/// Split a path relative to the File-set root into File ID components, validating them.
pub fn file_id_from_relative_path(path: &Path) -> Result<Vec<String>, DicomdirError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(DicomdirError::InvalidFileId {
                    file_id: path.display().to_string(),
                    reason: "must be a path below the File-set root".into(),
                })
            }
        }
    }
    validate_file_id(&components)?;
    Ok(components)
}

/// The Directory Record Type (PS3.3 F.3.2.2 (0004,1430)) for an instance of `sop_class_uid`, per
/// the "This Directory Record shall be used to reference ..." rule of each PS3.3 F.5 record
/// definition. `modality` lets the PLAN and RADIOTHERAPY records (defined by Modality, F.5.40 /
/// F.5.46) catch SOP Classes newer than this table. Returns `None` for a SOP Class that isn't
/// one of F.5's non-image record types: the caller decides whether it is an IMAGE.
pub fn directory_record_type_for_sop_class(sop_class_uid: &str, modality: Option<&str>) -> Option<&'static str> {
    let uid = trim_value(sop_class_uid);
    if let Some(record_type) = NON_IMAGE_SOP_CLASSES.iter().find(|(u, _)| *u == uid).map(|(_, t)| *t) {
        return Some(record_type);
    }
    match modality.map(trim_value) {
        Some("PLAN") => Some("PLAN"),
        // RT Second Generation IODs (PS3.3 A.8x) all live under 1.2.840.10008.5.1.4.1.1.481.x
        // from .10 on; the explicit table above lists the ones published as of 2026d.
        Some("RTINTENT" | "RTSEGANN" | "RTRAD" | "RTRECORD") if uid.starts_with("1.2.840.10008.5.1.4.1.1.481.") => {
            Some("RADIOTHERAPY")
        }
        _ => None,
    }
}

/// Build a DICOMDIR in memory. Returns the complete Part 10 file bytes and a report.
pub fn build_dicomdir(
    entries: &[DicomdirEntry],
    options: &DicomdirOptions,
) -> Result<(Vec<u8>, DicomdirReport), DicomdirError> {
    let file_set_id = trim_value(&options.file_set_id).to_owned();
    validate_file_set_id(&file_set_id)?;
    if entries.is_empty() {
        return Err(DicomdirError::NoEntries);
    }
    let mut report = DicomdirReport::default();
    let tree = build_tree(entries, &mut report)?;
    let bytes = encode_dicomdir(&tree, &file_set_id, options.sop_instance_uid.as_deref())?;
    report.record_count = tree.records.len();
    for record in &tree.records {
        match report.record_types.iter_mut().find(|(t, _)| t == record.record_type) {
            Some((_, n)) => *n += 1,
            None => report.record_types.push((record.record_type.to_owned(), 1)),
        }
    }
    Ok((bytes, report))
}

/// Build a DICOMDIR and write it to `path` (normally `<File-set root>/DICOMDIR`).
pub fn write_dicomdir(
    entries: &[DicomdirEntry],
    path: impl AsRef<Path>,
    options: &DicomdirOptions,
) -> Result<DicomdirReport, DicomdirError> {
    let (bytes, report) = build_dicomdir(entries, options)?;
    std::fs::write(path.as_ref(), bytes).map_err(DicomdirError::Io)?;
    Ok(report)
}

/// Walk a File-set root and make an entry for every DICOM file under it, in sorted path order.
/// A top-level `DICOMDIR` is ignored; other non-DICOM files (a README.TXT, an INDEX.HTM, ...) are
/// skipped and returned as the second value. A DICOM file whose path breaks the File ID rules is an error.
pub fn collect_dicomdir_entries(root: &Path) -> Result<(Vec<DicomdirEntry>, Vec<String>), DicomdirError> {
    if !root.is_dir() {
        return Err(DicomdirError::Tree(format!("{} is not a directory", root.display())));
    }
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(DicomdirError::Io)? {
            let entry = entry.map_err(DicomdirError::Io)?;
            let file_type = entry.file_type().map_err(DicomdirError::Io)?;
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() || (file_type.is_symlink() && path.is_file()) {
                files.push(path);
            }
        }
    }
    files.sort();

    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    for path in files {
        let relative = path.strip_prefix(root).expect("walked path is below root").to_path_buf();
        if relative == Path::new("DICOMDIR") {
            continue; // the directory being (re)written
        }
        if !has_dicm_prefix(&path) {
            skipped.push(relative.display().to_string());
            continue;
        }
        let file_id = file_id_from_relative_path(&relative)?;
        entries.push(DicomdirEntry::from_file(file_id, &path)?);
    }
    Ok((entries, skipped))
}

fn has_dicm_prefix(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 132];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .map(|_| &head[128..132] == b"DICM")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// SOP Class -> Directory Record Type (PS3.3 F.5, 2026d)
// ---------------------------------------------------------------------------------------------

/// Every Storage SOP Class whose instances are referenced by a non-IMAGE record. Anything not
/// listed is an IMAGE when it looks like one (see `record_type_for_entry`). Grouped by the F.5
/// section that names the IOD in its "shall be used to reference" sentence.
const NON_IMAGE_SOP_CLASSES: &[(&str, &str)] = &[
    // F.5.19 RT DOSE
    ("1.2.840.10008.5.1.4.1.1.481.2", "RT DOSE"),
    // F.5.20 RT STRUCTURE SET
    ("1.2.840.10008.5.1.4.1.1.481.3", "RT STRUCTURE SET"),
    // F.5.21 RT PLAN: RT Plan or RT Ion Plan
    ("1.2.840.10008.5.1.4.1.1.481.5", "RT PLAN"),
    ("1.2.840.10008.5.1.4.1.1.481.8", "RT PLAN"),
    // F.5.22 RT TREAT RECORD: Beams, Brachy, Summary (and Ion Beams) Treatment Record
    ("1.2.840.10008.5.1.4.1.1.481.4", "RT TREAT RECORD"),
    ("1.2.840.10008.5.1.4.1.1.481.6", "RT TREAT RECORD"),
    ("1.2.840.10008.5.1.4.1.1.481.7", "RT TREAT RECORD"),
    ("1.2.840.10008.5.1.4.1.1.481.9", "RT TREAT RECORD"),
    // F.5.23 PRESENTATION: Presentation States (incl. volumetric) and Structured Display
    ("1.2.840.10008.5.1.4.1.1.11.1", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.2", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.3", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.4", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.5", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.6", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.7", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.8", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.9", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.10", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.11", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.11.12", "PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.131", "PRESENTATION"),
    // F.5.24 WAVEFORM
    ("1.2.840.10008.5.1.4.1.1.9.1.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.1.2", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.1.3", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.1.4", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.2.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.3.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.4.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.4.2", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.5.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.6.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.6.2", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.7.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.7.2", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.7.3", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.7.4", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.9.8.1", "WAVEFORM"),
    ("1.2.840.10008.5.1.4.1.1.601.5", "WAVEFORM"),
    // F.5.25 SR DOCUMENT: every SR Storage SOP Class but Key Object Selection
    ("1.2.840.10008.5.1.4.1.1.88.11", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.22", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.33", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.34", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.35", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.40", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.50", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.65", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.67", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.68", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.69", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.70", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.71", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.72", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.73", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.74", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.75", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.76", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.77", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.78.6", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.79.1", "SR DOCUMENT"),
    // retired SR trial classes, still found on old media
    ("1.2.840.10008.5.1.4.1.1.88.1", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.2", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.3", "SR DOCUMENT"),
    ("1.2.840.10008.5.1.4.1.1.88.4", "SR DOCUMENT"),
    // F.5.26 KEY OBJECT DOC
    ("1.2.840.10008.5.1.4.1.1.88.59", "KEY OBJECT DOC"),
    // F.5.27 SPECTROSCOPY
    ("1.2.840.10008.5.1.4.1.1.4.2", "SPECTROSCOPY"),
    // F.5.28 RAW DATA
    ("1.2.840.10008.5.1.4.1.1.66", "RAW DATA"),
    // F.5.29 REGISTRATION: Spatial Registration, Deformable Spatial Registration
    ("1.2.840.10008.5.1.4.1.1.66.1", "REGISTRATION"),
    ("1.2.840.10008.5.1.4.1.1.66.3", "REGISTRATION"),
    // F.5.30 FIDUCIAL
    ("1.2.840.10008.5.1.4.1.1.66.2", "FIDUCIAL"),
    // F.5.31 HANGING PROTOCOL (root level)
    ("1.2.840.10008.5.1.4.38.1", "HANGING PROTOCOL"),
    // F.5.32 ENCAP DOC: PDF, CDA, STL, OBJ, MTL
    ("1.2.840.10008.5.1.4.1.1.104.1", "ENCAP DOC"),
    ("1.2.840.10008.5.1.4.1.1.104.2", "ENCAP DOC"),
    ("1.2.840.10008.5.1.4.1.1.104.3", "ENCAP DOC"),
    ("1.2.840.10008.5.1.4.1.1.104.4", "ENCAP DOC"),
    ("1.2.840.10008.5.1.4.1.1.104.5", "ENCAP DOC"),
    // F.5.34 VALUE MAP
    ("1.2.840.10008.5.1.4.1.1.67", "VALUE MAP"),
    // F.5.35 STEREOMETRIC
    ("1.2.840.10008.5.1.4.1.1.77.1.5.3", "STEREOMETRIC"),
    // F.5.36 PALETTE (root level)
    ("1.2.840.10008.5.1.4.39.1", "PALETTE"),
    // F.5.37-39 IMPLANT, IMPLANT ASSY, IMPLANT GROUP (root level)
    ("1.2.840.10008.5.1.4.43.1", "IMPLANT"),
    ("1.2.840.10008.5.1.4.44.1", "IMPLANT ASSY"),
    ("1.2.840.10008.5.1.4.45.1", "IMPLANT GROUP"),
    // F.5.40 PLAN: Modality PLAN, e.g. RT Beams / RT Brachy Application Setup Delivery Instruction
    ("1.2.840.10008.5.1.4.34.7", "PLAN"),
    ("1.2.840.10008.5.1.4.34.10", "PLAN"),
    // F.5.41 MEASUREMENT: the ophthalmic measurement IODs
    ("1.2.840.10008.5.1.4.1.1.78.1", "MEASUREMENT"),
    ("1.2.840.10008.5.1.4.1.1.78.2", "MEASUREMENT"),
    ("1.2.840.10008.5.1.4.1.1.78.3", "MEASUREMENT"),
    ("1.2.840.10008.5.1.4.1.1.78.4", "MEASUREMENT"),
    ("1.2.840.10008.5.1.4.1.1.78.5", "MEASUREMENT"),
    ("1.2.840.10008.5.1.4.1.1.78.7", "MEASUREMENT"),
    ("1.2.840.10008.5.1.4.1.1.78.8", "MEASUREMENT"),
    ("1.2.840.10008.5.1.4.1.1.80.1", "MEASUREMENT"),
    // F.5.42 SURFACE
    ("1.2.840.10008.5.1.4.1.1.66.5", "SURFACE"),
    // F.5.43 SURFACE SCAN: mesh, point cloud
    ("1.2.840.10008.5.1.4.1.1.68.1", "SURFACE SCAN"),
    ("1.2.840.10008.5.1.4.1.1.68.2", "SURFACE SCAN"),
    // F.5.44 TRACT
    ("1.2.840.10008.5.1.4.1.1.66.6", "TRACT"),
    // F.5.45 ASSESSMENT
    ("1.2.840.10008.5.1.4.1.1.90.1", "ASSESSMENT"),
    // F.5.46 RADIOTHERAPY: the RT Second Generation IODs
    ("1.2.840.10008.5.1.4.1.1.481.10", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.11", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.12", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.13", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.14", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.15", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.16", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.17", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.18", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.19", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.20", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.21", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.22", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.23", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.24", "RADIOTHERAPY"),
    ("1.2.840.10008.5.1.4.1.1.481.25", "RADIOTHERAPY"),
    // F.5.47 ANNOTATION: Microscopy Bulk Simple Annotations
    ("1.2.840.10008.5.1.4.1.1.91.1", "ANNOTATION"),
    // F.5.48 INVENTORY (root level)
    ("1.2.840.10008.5.1.4.1.1.201.1", "INVENTORY"),
    // F.5.49 WF PRESENTATION: Waveform Presentation State, Waveform Acquisition Presentation State
    ("1.2.840.10008.5.1.4.1.1.9.100.1", "WF PRESENTATION"),
    ("1.2.840.10008.5.1.4.1.1.9.100.2", "WF PRESENTATION"),
];

/// Record types that sit directly in the Root Directory Entity (Table F.4-1), not under a
/// PATIENT/STUDY/SERIES.
const ROOT_LEVEL_RECORD_TYPES: &[&str] =
    &["HANGING PROTOCOL", "PALETTE", "IMPLANT", "IMPLANT ASSY", "IMPLANT GROUP", "INVENTORY"];

fn record_type_for_entry(entry: &DicomdirEntry, sop_class_uid: &str, file_id: &str) -> Result<&'static str, DicomdirError> {
    let modality = get_str(&entry.dataset, MODALITY);
    if let Some(record_type) = directory_record_type_for_sop_class(sop_class_uid, modality.as_deref()) {
        return Ok(record_type);
    }
    // IMAGE (F.5.4) "shall be used to reference an Image SOP Instance". Anything not mapped
    // above is accepted as one when it carries the Image Pixel module's Rows and Columns (Type 1
    // in every image IOD); any other SOP Class (a protocol, a DICOS report, a private non-image
    // class) has no standard record to go in.
    let has_pixels = entry.dataset.get(ROWS).is_some() && entry.dataset.get(COLUMNS).is_some();
    if has_pixels {
        return Ok("IMAGE");
    }
    Err(DicomdirError::NoRecordType { file_id: file_id.to_owned(), sop_class_uid: sop_class_uid.to_owned() })
}

// ---------------------------------------------------------------------------------------------
// Record keys (PS3.3 F.5 tables, plus PS3.11 Tables D.3-2 and H.3-2)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyType {
    /// Type 1: required with a value. When the instance lacks it, `fallbacks` are tried in
    /// order and reported as synthesized; failing those, see `synthesize_type1`.
    One,
    /// Type 2: required, empty when unknown.
    Two,
    /// Type 1C / 3: copied when the instance has a non-empty value.
    IfPresent,
}

#[derive(Clone, Copy, Debug)]
struct Key {
    tag: Tag,
    vr: VR,
    keyword: &'static str,
    kind: KeyType,
    fallbacks: &'static [Tag],
}

const fn key(group: u16, element: u16, vr: VR, keyword: &'static str, kind: KeyType) -> Key {
    Key { tag: Tag(group, element), vr, keyword, kind, fallbacks: &[] }
}

const fn key_or(group: u16, element: u16, vr: VR, keyword: &'static str, fallbacks: &'static [Tag]) -> Key {
    Key { tag: Tag(group, element), vr, keyword, kind: KeyType::One, fallbacks }
}

use KeyType::{IfPresent, One, Two};

const DATE_FALLBACKS_FOR_STUDY: &[Tag] = &[SERIES_DATE, ACQUISITION_DATE, CONTENT_DATE, INSTANCE_CREATION_DATE];
const TIME_FALLBACKS_FOR_STUDY: &[Tag] = &[SERIES_TIME, ACQUISITION_TIME, CONTENT_TIME, INSTANCE_CREATION_TIME];
const CONTENT_DATE_FALLBACKS: &[Tag] = &[INSTANCE_CREATION_DATE, SERIES_DATE, STUDY_DATE];
const CONTENT_TIME_FALLBACKS: &[Tag] = &[INSTANCE_CREATION_TIME, SERIES_TIME, STUDY_TIME];
const CONTENT_DATE_KEY: Key = key_or(0x0008, 0x0023, VR::DA, "ContentDate", CONTENT_DATE_FALLBACKS);
const CONTENT_TIME_KEY: Key = key_or(0x0008, 0x0033, VR::TM, "ContentTime", CONTENT_TIME_FALLBACKS);
const INSTANCE_NUMBER_KEY: Key = key(0x0020, 0x0013, VR::IS, "InstanceNumber", One);

const PATIENT_ID_KEY: Key = key(0x0010, 0x0020, VR::LO, "PatientID", One);
const STUDY_ID_KEY: Key = key(0x0020, 0x0010, VR::SH, "StudyID", One);
const MODALITY_KEY: Key = key(0x0008, 0x0060, VR::CS, "Modality", One);
const SERIES_NUMBER_KEY: Key = key(0x0020, 0x0011, VR::IS, "SeriesNumber", One);

/// F.5.1 Table F.5-1, plus H.3-2's Patient's Birth Date / Sex.
const PATIENT_KEYS: &[Key] = &[
    key(0x0010, 0x0010, VR::PN, "PatientName", Two),
    PATIENT_ID_KEY,
    key(0x0010, 0x0030, VR::DA, "PatientBirthDate", IfPresent),
    key(0x0010, 0x0040, VR::CS, "PatientSex", IfPresent),
];

/// F.5.2 Table F.5-2 (Study Instance UID is Type 1 there in 2026d).
const STUDY_KEYS: &[Key] = &[
    key_or(0x0008, 0x0020, VR::DA, "StudyDate", DATE_FALLBACKS_FOR_STUDY),
    key_or(0x0008, 0x0030, VR::TM, "StudyTime", TIME_FALLBACKS_FOR_STUDY),
    key(0x0008, 0x0050, VR::SH, "AccessionNumber", Two),
    key(0x0008, 0x1030, VR::LO, "StudyDescription", Two),
    key(0x0020, 0x000D, VR::UI, "StudyInstanceUID", One),
    STUDY_ID_KEY,
];

/// F.5.3 Table F.5-3, plus H.3-2's Institution Name / Address and Performing Physician's Name,
/// plus Series Description (Type 3, what a reader lists a series by).
const SERIES_KEYS: &[Key] = &[
    MODALITY_KEY,
    key(0x0008, 0x0080, VR::LO, "InstitutionName", IfPresent),
    key(0x0008, 0x0081, VR::ST, "InstitutionAddress", IfPresent),
    key(0x0008, 0x103E, VR::LO, "SeriesDescription", IfPresent),
    key(0x0008, 0x1050, VR::PN, "PerformingPhysicianName", IfPresent),
    key(0x0020, 0x000E, VR::UI, "SeriesInstanceUID", One),
    SERIES_NUMBER_KEY,
];

/// F.5.4 Table F.5-4 (Instance Number), D.3-2 (Image Type, Referenced Image Sequence) and the
/// IMAGE rows of H.3-2. Rows/Columns are Type 1 in H.3-2 (DVD), copied when present.
const IMAGE_KEYS: &[Key] = &[
    key(0x0008, 0x0008, VR::CS, "ImageType", IfPresent),
    key(0x0008, 0x002A, VR::DT, "AcquisitionDateTime", IfPresent),
    key(0x0008, 0x1140, VR::SQ, "ReferencedImageSequence", IfPresent),
    key(0x0018, 0x1800, VR::CS, "AcquisitionTimeSynchronized", IfPresent),
    INSTANCE_NUMBER_KEY,
    key(0x0020, 0x0032, VR::DS, "ImagePositionPatient", IfPresent),
    key(0x0020, 0x0037, VR::DS, "ImageOrientationPatient", IfPresent),
    key(0x0020, 0x0052, VR::UI, "FrameOfReferenceUID", IfPresent),
    key(0x0020, 0x0200, VR::UI, "SynchronizationFrameOfReferenceUID", IfPresent),
    key(0x0028, 0x0008, VR::IS, "NumberOfFrames", IfPresent),
    key(0x0028, 0x0010, VR::US, "Rows", IfPresent),
    key(0x0028, 0x0011, VR::US, "Columns", IfPresent),
    key(0x0028, 0x0030, VR::DS, "PixelSpacing", IfPresent),
    key(0x0028, 0x2112, VR::DS, "LossyImageCompressionRatio", IfPresent),
    key(0x0050, 0x0004, VR::CS, "CalibrationImage", IfPresent),
];

const RT_DOSE_KEYS: &[Key] = &[
    INSTANCE_NUMBER_KEY,
    key(0x3004, 0x0006, VR::LO, "DoseComment", IfPresent),
    key(0x3004, 0x000A, VR::CS, "DoseSummationType", One),
];

const RT_STRUCTURE_SET_KEYS: &[Key] = &[
    INSTANCE_NUMBER_KEY,
    key(0x3006, 0x0002, VR::SH, "StructureSetLabel", One),
    key(0x3006, 0x0008, VR::DA, "StructureSetDate", Two),
    key(0x3006, 0x0009, VR::TM, "StructureSetTime", Two),
];

const RT_PLAN_KEYS: &[Key] = &[
    INSTANCE_NUMBER_KEY,
    key(0x300A, 0x0002, VR::SH, "RTPlanLabel", One),
    key(0x300A, 0x0006, VR::DA, "RTPlanDate", Two),
    key(0x300A, 0x0007, VR::TM, "RTPlanTime", Two),
];

const RT_TREAT_RECORD_KEYS: &[Key] = &[
    INSTANCE_NUMBER_KEY,
    key(0x3008, 0x0250, VR::DA, "TreatmentDate", Two),
    key(0x3008, 0x0251, VR::TM, "TreatmentTime", Two),
];

const PRESENTATION_KEYS: &[Key] = &[
    key(0x0008, 0x1115, VR::SQ, "ReferencedSeriesSequence", IfPresent),
    INSTANCE_NUMBER_KEY_IF_PRESENT,
    key(0x0070, 0x0080, VR::CS, "ContentLabel", IfPresent),
    key(0x0070, 0x0081, VR::LO, "ContentDescription", IfPresent),
    key_or(0x0070, 0x0082, VR::DA, "PresentationCreationDate", CONTENT_DATE_FALLBACKS),
    key_or(0x0070, 0x0083, VR::TM, "PresentationCreationTime", CONTENT_TIME_FALLBACKS),
    key(0x0070, 0x0084, VR::PN, "ContentCreatorName", IfPresent),
    key(0x0070, 0x0402, VR::SQ, "BlendingSequence", IfPresent),
];
const INSTANCE_NUMBER_KEY_IF_PRESENT: Key = key(0x0020, 0x0013, VR::IS, "InstanceNumber", IfPresent);

const WAVEFORM_KEYS: &[Key] = &[CONTENT_DATE_KEY, CONTENT_TIME_KEY, INSTANCE_NUMBER_KEY];

/// F.5.25. Verification DateTime and Content Sequence are filled in specially (see
/// `special_leaf_keys`).
const SR_DOCUMENT_KEYS: &[Key] = &[
    CONTENT_DATE_KEY,
    CONTENT_TIME_KEY,
    INSTANCE_NUMBER_KEY,
    key(0x0040, 0xA043, VR::SQ, "ConceptNameCodeSequence", One),
    key(0x0040, 0xA491, VR::CS, "CompletionFlag", One),
    key(0x0040, 0xA493, VR::CS, "VerificationFlag", One),
];

const KEY_OBJECT_DOC_KEYS: &[Key] = &[
    CONTENT_DATE_KEY,
    CONTENT_TIME_KEY,
    INSTANCE_NUMBER_KEY,
    key(0x0040, 0xA043, VR::SQ, "ConceptNameCodeSequence", One),
];

/// F.5.27 plus H.3-2's IMAGE-or-SPECTROSCOPY rows.
const SPECTROSCOPY_KEYS: &[Key] = &[
    key(0x0008, 0x0008, VR::CS, "ImageType", One),
    CONTENT_DATE_KEY,
    CONTENT_TIME_KEY,
    key(0x0008, 0x002A, VR::DT, "AcquisitionDateTime", IfPresent),
    key(0x0008, 0x1140, VR::SQ, "ReferencedImageSequence", IfPresent),
    key(0x0008, 0x9092, VR::SQ, "ReferencedImageEvidenceSequence", IfPresent),
    key(0x0018, 0x1800, VR::CS, "AcquisitionTimeSynchronized", IfPresent),
    INSTANCE_NUMBER_KEY,
    key(0x0020, 0x0032, VR::DS, "ImagePositionPatient", IfPresent),
    key(0x0020, 0x0037, VR::DS, "ImageOrientationPatient", IfPresent),
    key(0x0020, 0x0052, VR::UI, "FrameOfReferenceUID", IfPresent),
    key(0x0020, 0x0200, VR::UI, "SynchronizationFrameOfReferenceUID", IfPresent),
    key(0x0028, 0x0008, VR::IS, "NumberOfFrames", One),
    key(0x0028, 0x0010, VR::US, "Rows", One),
    key(0x0028, 0x0011, VR::US, "Columns", One),
    key(0x0028, 0x0030, VR::DS, "PixelSpacing", IfPresent),
    key(0x0028, 0x9001, VR::UL, "DataPointRows", One),
    key(0x0028, 0x9002, VR::UL, "DataPointColumns", One),
];

const RAW_DATA_KEYS: &[Key] = &[CONTENT_DATE_KEY, CONTENT_TIME_KEY, key(0x0020, 0x0013, VR::IS, "InstanceNumber", Two)];

/// REGISTRATION, FIDUCIAL, VALUE MAP, MEASUREMENT, SURFACE, SURFACE SCAN, TRACT, ANNOTATION:
/// Content Date and Time, both Type 1.
const CONTENT_DATE_TIME_KEYS: &[Key] = &[CONTENT_DATE_KEY, CONTENT_TIME_KEY];

const HANGING_PROTOCOL_KEYS: &[Key] = &[
    key(0x0072, 0x0002, VR::SH, "HangingProtocolName", One),
    key(0x0072, 0x0004, VR::LO, "HangingProtocolDescription", One),
    key(0x0072, 0x0006, VR::CS, "HangingProtocolLevel", One),
    key(0x0072, 0x0008, VR::LO, "HangingProtocolCreator", One),
    key(0x0072, 0x000A, VR::DT, "HangingProtocolCreationDateTime", One),
    key(0x0072, 0x000C, VR::SQ, "HangingProtocolDefinitionSequence", One),
    key(0x0072, 0x000E, VR::SQ, "HangingProtocolUserIdentificationCodeSequence", Two),
    key(0x0072, 0x0014, VR::US, "NumberOfPriorsReferenced", One),
];

const ENCAP_DOC_KEYS: &[Key] = &[
    key(0x0008, 0x0023, VR::DA, "ContentDate", Two),
    key(0x0008, 0x0033, VR::TM, "ContentTime", Two),
    INSTANCE_NUMBER_KEY,
    key(0x0040, 0xA043, VR::SQ, "ConceptNameCodeSequence", Two),
    key(0x0040, 0xE001, VR::ST, "HL7InstanceIdentifier", IfPresent),
    key(0x0042, 0x0010, VR::ST, "DocumentTitle", Two),
    key(0x0042, 0x0012, VR::LO, "MIMETypeOfEncapsulatedDocument", One),
];

const PALETTE_KEYS: &[Key] = &[
    key(0x0070, 0x0080, VR::CS, "ContentLabel", One),
    key(0x0070, 0x0081, VR::LO, "ContentDescription", Two),
];

const IMPLANT_KEYS: &[Key] = &[
    key(0x0008, 0x0070, VR::LO, "Manufacturer", One),
    key(0x0022, 0x1095, VR::LO, "ImplantName", One),
    key(0x0022, 0x1097, VR::LO, "ImplantPartNumber", One),
    key(0x0068, 0x6210, VR::LO, "ImplantSize", IfPresent),
];

const IMPLANT_ASSY_KEYS: &[Key] = &[
    key(0x0008, 0x0070, VR::LO, "Manufacturer", One),
    key(0x0076, 0x0001, VR::LO, "ImplantAssemblyTemplateName", One),
    key(0x0076, 0x0020, VR::SQ, "ProcedureTypeCodeSequence", One),
];

const IMPLANT_GROUP_KEYS: &[Key] = &[
    key(0x0078, 0x0001, VR::LO, "ImplantTemplateGroupName", One),
    key(0x0078, 0x0020, VR::LO, "ImplantTemplateGroupIssuer", One),
];

const ASSESSMENT_KEYS: &[Key] = &[
    key(0x0008, 0x0012, VR::DA, "InstanceCreationDate", One),
    key(0x0008, 0x0013, VR::TM, "InstanceCreationTime", Two),
    INSTANCE_NUMBER_KEY,
];

const RADIOTHERAPY_KEYS: &[Key] = &[
    INSTANCE_NUMBER_KEY,
    key(0x0070, 0x0081, VR::LO, "ContentDescription", Two),
    key(0x0070, 0x0084, VR::PN, "ContentCreatorName", Two),
    key(0x3010, 0x0033, VR::SH, "UserContentLabel", IfPresent),
    key(0x3010, 0x0034, VR::LO, "UserContentLongLabel", IfPresent),
];

const INVENTORY_KEYS: &[Key] = &[
    CONTENT_DATE_KEY,
    CONTENT_TIME_KEY,
    key(0x0008, 0x0401, VR::CS, "InventoryPurpose", Two),
    key(0x0008, 0x0403, VR::CS, "InventoryLevel", One),
    key(0x0008, 0x0426, VR::CS, "InventoryCompletionStatus", One),
    key(0x0008, 0x0428, VR::UL, "TotalNumberOfStudyRecords", One),
];

const WF_PRESENTATION_KEYS: &[Key] = &[
    key(0x0008, 0x1115, VR::SQ, "ReferencedSeriesSequence", IfPresent),
    key_or(0x0070, 0x0082, VR::DA, "PresentationCreationDate", CONTENT_DATE_FALLBACKS),
    key_or(0x0070, 0x0083, VR::TM, "PresentationCreationTime", CONTENT_TIME_FALLBACKS),
];

/// STEREOMETRIC and PLAN have no keys beyond Specific Character Set.
const NO_KEYS: &[Key] = &[];

fn leaf_keys(record_type: &str) -> &'static [Key] {
    match record_type {
        "IMAGE" => IMAGE_KEYS,
        "RT DOSE" => RT_DOSE_KEYS,
        "RT STRUCTURE SET" => RT_STRUCTURE_SET_KEYS,
        "RT PLAN" => RT_PLAN_KEYS,
        "RT TREAT RECORD" => RT_TREAT_RECORD_KEYS,
        "PRESENTATION" => PRESENTATION_KEYS,
        "WAVEFORM" => WAVEFORM_KEYS,
        "SR DOCUMENT" => SR_DOCUMENT_KEYS,
        "KEY OBJECT DOC" => KEY_OBJECT_DOC_KEYS,
        "SPECTROSCOPY" => SPECTROSCOPY_KEYS,
        "RAW DATA" => RAW_DATA_KEYS,
        "REGISTRATION" | "FIDUCIAL" | "VALUE MAP" | "MEASUREMENT" | "SURFACE" | "SURFACE SCAN" | "TRACT"
        | "ANNOTATION" => CONTENT_DATE_TIME_KEYS,
        "HANGING PROTOCOL" => HANGING_PROTOCOL_KEYS,
        "ENCAP DOC" => ENCAP_DOC_KEYS,
        "PALETTE" => PALETTE_KEYS,
        "IMPLANT" => IMPLANT_KEYS,
        "IMPLANT ASSY" => IMPLANT_ASSY_KEYS,
        "IMPLANT GROUP" => IMPLANT_GROUP_KEYS,
        "ASSESSMENT" => ASSESSMENT_KEYS,
        "RADIOTHERAPY" => RADIOTHERAPY_KEYS,
        "INVENTORY" => INVENTORY_KEYS,
        "WF PRESENTATION" => WF_PRESENTATION_KEYS,
        _ => NO_KEYS,
    }
}

// ---------------------------------------------------------------------------------------------
// Building the record tree
// ---------------------------------------------------------------------------------------------

struct Record {
    record_type: &'static str,
    keys: InMemDicomObject,
    children: Vec<usize>,
}

struct Tree {
    records: Vec<Record>,
    roots: Vec<usize>,
}

impl Tree {
    fn push(&mut self, record: Record) -> usize {
        self.records.push(record);
        self.records.len() - 1
    }
}

/// An entry after its identifying values are checked.
struct Resolved<'a> {
    entry: &'a DicomdirEntry,
    file_id: String,
    sop_class_uid: String,
    sop_instance_uid: String,
    record_type: &'static str,
}

struct SeriesGroup {
    uid: String,
    instances: Vec<usize>,
}

struct StudyGroup {
    uid: String,
    series: Vec<SeriesGroup>,
}

struct PatientGroup {
    patient_id: Option<String>,
    name: String,
    birth_date: String,
    studies: Vec<StudyGroup>,
}

fn build_tree(entries: &[DicomdirEntry], report: &mut DicomdirReport) -> Result<Tree, DicomdirError> {
    // 1. Check every entry's identity: File ID, UIDs, record type; no duplicates.
    let mut resolved = Vec::with_capacity(entries.len());
    let mut file_ids = HashSet::new();
    let mut sop_instances: HashMap<String, String> = HashMap::new();
    for entry in entries {
        validate_file_id(&entry.file_id)?;
        let file_id = entry.file_id.join("\\");
        if !file_ids.insert(file_id.clone()) {
            return Err(DicomdirError::DuplicateFileId(file_id));
        }
        let missing = |attribute| DicomdirError::MissingAttribute { file_id: file_id.clone(), attribute };
        let sop_class_uid = entry
            .sop_class_uid
            .as_deref()
            .map(trim_value)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| get_str(&entry.dataset, SOP_CLASS_UID))
            .ok_or_else(|| missing("SOPClassUID"))?;
        let sop_instance_uid = entry
            .sop_instance_uid
            .as_deref()
            .map(trim_value)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| get_str(&entry.dataset, SOP_INSTANCE_UID))
            .ok_or_else(|| missing("SOPInstanceUID"))?;
        if trim_value(&entry.transfer_syntax_uid).is_empty() {
            return Err(missing("TransferSyntaxUID"));
        }
        if let Some(previous) = sop_instances.insert(sop_instance_uid.clone(), file_id.clone()) {
            return Err(DicomdirError::DuplicateSopInstance { sop_instance_uid, file_ids: (previous, file_id) });
        }
        let record_type = record_type_for_entry(entry, &sop_class_uid, &file_id)?;
        resolved.push(Resolved { entry, file_id, sop_class_uid, sop_instance_uid, record_type });
    }

    // 2. Group into patients > studies > series, in first-seen order, refusing a Patient ID
    //    shared by two patients, a study under two patients, a series in two studies.
    let mut patients: Vec<PatientGroup> = Vec::new();
    let mut root_level: Vec<usize> = Vec::new();
    let mut study_owner: HashMap<String, usize> = HashMap::new();
    let mut series_owner: HashMap<String, String> = HashMap::new();
    for (index, item) in resolved.iter().enumerate() {
        if ROOT_LEVEL_RECORD_TYPES.contains(&item.record_type) {
            root_level.push(index);
            continue;
        }
        let ds = &item.entry.dataset;
        let study_uid = get_str(ds, STUDY_INSTANCE_UID)
            .ok_or(DicomdirError::MissingAttribute { file_id: item.file_id.clone(), attribute: "StudyInstanceUID" })?;
        let series_uid = get_str(ds, SERIES_INSTANCE_UID)
            .ok_or(DicomdirError::MissingAttribute { file_id: item.file_id.clone(), attribute: "SeriesInstanceUID" })?;
        let patient_id = get_str(ds, PATIENT_ID);
        let name = normalize_name(&get_str(ds, PATIENT_NAME).unwrap_or_default());
        let birth_date = get_str(ds, PATIENT_BIRTH_DATE).unwrap_or_default();

        let patient_index = match study_owner.get(&study_uid) {
            Some(&p) => p,
            None => {
                // Without a Patient ID, studies are one patient when they have the same
                // (non-empty) name and birth date; with neither ID nor name, nothing says two
                // studies are the same patient, so each study is its own.
                let found = patients.iter().position(|p| match (&p.patient_id, &patient_id) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => !name.is_empty() && p.name == name && p.birth_date == birth_date,
                    _ => false,
                });
                let p = match found {
                    Some(p) => p,
                    None => {
                        patients.push(PatientGroup {
                            patient_id: patient_id.clone(),
                            name: name.clone(),
                            birth_date: birth_date.clone(),
                            studies: Vec::new(),
                        });
                        patients.len() - 1
                    }
                };
                study_owner.insert(study_uid.clone(), p);
                p
            }
        };
        let patient = &mut patients[patient_index];
        // Every instance of a study must name the same patient as the study's first instance.
        let same_patient = match (&patient.patient_id, &patient_id) {
            (Some(a), Some(b)) => a == b,
            (None, None) => patient.name == name && patient.birth_date == birth_date,
            _ => false,
        };
        if !same_patient {
            return Err(DicomdirError::StudyConflict { study_instance_uid: study_uid });
        }
        if patient.patient_id.is_some() && !names_match(&patient.name, &name) {
            return Err(DicomdirError::PatientConflict {
                patient_id: patient.patient_id.clone().unwrap_or_default(),
                names: (patient.name.clone(), name),
            });
        }
        if patient.name.is_empty() && !name.is_empty() {
            patient.name = name;
        }

        match series_owner.get(&series_uid) {
            Some(owner) if *owner != study_uid => {
                return Err(DicomdirError::SeriesConflict { series_instance_uid: series_uid })
            }
            Some(_) => {}
            None => {
                series_owner.insert(series_uid.clone(), study_uid.clone());
            }
        }
        let study = match patient.studies.iter_mut().position(|s| s.uid == study_uid) {
            Some(s) => &mut patient.studies[s],
            None => {
                patient.studies.push(StudyGroup { uid: study_uid, series: Vec::new() });
                patient.studies.last_mut().expect("just pushed")
            }
        };
        let series = match study.series.iter_mut().position(|s| s.uid == series_uid) {
            Some(s) => &mut study.series[s],
            None => {
                study.series.push(SeriesGroup { uid: series_uid, instances: Vec::new() });
                study.series.last_mut().expect("just pushed")
            }
        };
        series.instances.push(index);
    }

    // 3. Build the records.
    let mut tree = Tree { records: Vec::new(), roots: Vec::new() };
    let used_patient_ids: HashSet<String> = patients.iter().filter_map(|p| p.patient_id.clone()).collect();
    let mut synthetic_patients = 0usize;
    let mut synthetic_studies = 0usize;
    let used_study_ids: HashSet<String> = resolved
        .iter()
        .filter_map(|r| get_str(&r.entry.dataset, STUDY_ID))
        .collect();

    for patient in &patients {
        let instances: Vec<&Resolved> =
            patient.studies.iter().flat_map(|s| s.series.iter()).flat_map(|s| s.instances.iter()).map(|&i| &resolved[i]).collect();
        let mut keys = entity_keys(PATIENT_KEYS, &instances, "PATIENT", report, &patient.name);
        if patient.patient_id.is_none() {
            let id = loop {
                synthetic_patients += 1;
                let candidate = format!("SYNTH_PAT_{synthetic_patients:04}");
                if !used_patient_ids.contains(&candidate) {
                    break candidate;
                }
            };
            let entity = if patient.name.is_empty() {
                format!("patient of study {}", patient.studies[0].uid)
            } else {
                patient.name.clone()
            };
            synthesize(&mut keys, report, "PATIENT", &entity, &PATIENT_ID_KEY, id, None);
        }
        let patient_record = tree.push(Record { record_type: "PATIENT", keys, children: Vec::new() });
        tree.roots.push(patient_record);

        for study in &patient.studies {
            let instances: Vec<&Resolved> =
                study.series.iter().flat_map(|s| s.instances.iter()).map(|&i| &resolved[i]).collect();
            let mut keys = entity_keys(STUDY_KEYS, &instances, "STUDY", report, &study.uid);
            if get_str(&keys, STUDY_ID).is_none() {
                let id = loop {
                    synthetic_studies += 1;
                    let candidate = if synthetic_studies <= 9999 {
                        format!("SYNTH_STUDY_{synthetic_studies:04}")
                    } else {
                        format!("SYNTH_{synthetic_studies}")
                    };
                    if !used_study_ids.contains(&candidate) {
                        break candidate;
                    }
                };
                synthesize(&mut keys, report, "STUDY", &study.uid, &STUDY_ID_KEY, id, None);
            }
            let study_record = tree.push(Record { record_type: "STUDY", keys, children: Vec::new() });
            tree.records[patient_record].children.push(study_record);

            let mut next_series_number = max_number(study.series.iter().flat_map(|s| s.instances.iter()).map(|&i| &resolved[i]), SERIES_NUMBER) + 1;
            for series in &study.series {
                let instances: Vec<&Resolved> = series.instances.iter().map(|&i| &resolved[i]).collect();
                let mut keys = entity_keys(SERIES_KEYS, &instances, "SERIES", report, &series.uid);
                if get_str(&keys, MODALITY).is_none() {
                    synthesize(&mut keys, report, "SERIES", &series.uid, &MODALITY_KEY, "OT".into(), None);
                }
                if get_str(&keys, SERIES_NUMBER).is_none() {
                    let number = next_series_number.to_string();
                    next_series_number += 1;
                    synthesize(&mut keys, report, "SERIES", &series.uid, &SERIES_NUMBER_KEY, number, None);
                }
                let series_record = tree.push(Record { record_type: "SERIES", keys, children: Vec::new() });
                tree.records[study_record].children.push(series_record);

                let mut next_instance_number = max_number(instances.iter().copied(), INSTANCE_NUMBER) + 1;
                for item in &instances {
                    let mut keys = leaf_record(item, report);
                    let needs_number =
                        leaf_keys(item.record_type).iter().any(|k| k.tag == INSTANCE_NUMBER && k.kind == One);
                    if needs_number && get_str(&keys, INSTANCE_NUMBER).is_none() {
                        let number = next_instance_number.to_string();
                        next_instance_number += 1;
                        synthesize(&mut keys, report, item.record_type, &item.sop_instance_uid, &INSTANCE_NUMBER_KEY, number, None);
                    }
                    add_character_set(&mut keys);
                    let leaf = tree.push(Record { record_type: item.record_type, keys, children: Vec::new() });
                    tree.records[series_record].children.push(leaf);
                }
            }
        }
    }

    for &index in &root_level {
        let item = &resolved[index];
        let mut keys = leaf_record(item, report);
        add_character_set(&mut keys);
        let record = tree.push(Record { record_type: item.record_type, keys, children: Vec::new() });
        tree.roots.push(record);
    }

    for record in &mut tree.records {
        if matches!(record.record_type, "PATIENT" | "STUDY" | "SERIES") {
            add_character_set(&mut record.keys);
        }
        put_record_header(&mut record.keys, record.record_type);
    }
    Ok(tree)
}

/// Keys of a PATIENT/STUDY/SERIES record: each key from the first of the entity's instances that
/// has a non-empty value (so H.3-2's "present in any objects referenced by subordinate records"
/// holds), Type 2 keys empty when none has one, Type 1 keys through their fallbacks. Type 1
/// identifiers the caller synthesizes itself (Patient ID, Study ID, Modality, Series Number) are
/// left absent here.
fn entity_keys(
    keys: &[Key],
    instances: &[&Resolved],
    record_type: &str,
    report: &mut DicomdirReport,
    entity: &str,
) -> InMemDicomObject {
    let mut out = InMemDicomObject::new_empty();
    for key in keys {
        if let Some(element) = instances
            .iter()
            .find_map(|r| non_empty_element(&r.entry.dataset, key.tag))
            .and_then(|e| key_element(e, key))
        {
            out.put(element);
            continue;
        }
        match key.kind {
            Two => out.put(empty_element(key)),
            IfPresent => {}
            One => {
                let fallback = key
                    .fallbacks
                    .iter()
                    .find_map(|&tag| instances.iter().find_map(|r| non_empty_element(&r.entry.dataset, tag)).map(|e| (e, tag)));
                if let Some((element, from)) = fallback {
                    let value = element_string(element);
                    synthesize(&mut out, report, record_type, entity, key, value, Some(from));
                } else if let Some(value) = last_resort_value(key) {
                    synthesize(&mut out, report, record_type, entity, key, value, None);
                }
            }
        }
    }
    out
}

/// A value for a Type 1 date or time key nothing in the instance can supply: the date the
/// DICOMDIR is made, or midnight. Other Type 1 keys have no sensible stand-in.
fn last_resort_value(key: &Key) -> Option<String> {
    match (key.tag, key.vr) {
        (_, VR::DA) if key.tag == STUDY_DATE || !key.fallbacks.is_empty() => {
            Some(dcmnorm_core::chrono::Local::now().format("%Y%m%d").to_string())
        }
        (_, VR::TM) if key.tag == STUDY_TIME || !key.fallbacks.is_empty() => Some("000000".to_owned()),
        _ => None,
    }
}

/// Keys of an instance-level (or root-level) record, including the referenced-file elements.
fn leaf_record(item: &Resolved, report: &mut DicomdirReport) -> InMemDicomObject {
    let ds = &item.entry.dataset;
    let mut out = InMemDicomObject::new_empty();
    for key in leaf_keys(item.record_type) {
        if let Some(element) = lookup_leaf_key(ds, key.tag, item.record_type).and_then(|e| key_element(&e, key)) {
            out.put(element);
            continue;
        }
        match key.kind {
            Two => out.put(empty_element(key)),
            IfPresent => {}
            One => {
                if key.tag == INSTANCE_NUMBER {
                    continue; // synthesized by the caller, distinct within the series
                }
                let fallback = key.fallbacks.iter().find_map(|&tag| non_empty_element(ds, tag).map(|e| (e, tag)));
                if let Some((element, from)) = fallback {
                    let value = element_string(element);
                    synthesize(&mut out, report, item.record_type, &item.sop_instance_uid, key, value, Some(from));
                } else if let Some(value) = last_resort_value(key) {
                    synthesize(&mut out, report, item.record_type, &item.sop_instance_uid, key, value, None);
                } else {
                    out.put(empty_element(key));
                    report.warnings.push(format!(
                        "{} {}: Type 1 key {} is missing in the instance; written empty",
                        item.record_type, item.file_id, key.keyword
                    ));
                }
            }
        }
    }

    match item.record_type {
        "SR DOCUMENT" => {
            let verified = get_str(ds, VERIFICATION_FLAG).as_deref() == Some("VERIFIED");
            if verified {
                if let Some(latest) = latest_verification_datetime(ds) {
                    out.put(DataElement::new(VERIFICATION_DATE_TIME, VR::DT, PrimitiveValue::from(latest)));
                } else {
                    report.warnings.push(format!(
                        "SR DOCUMENT {}: VERIFIED but no Verifying Observer Sequence > Verification DateTime",
                        item.file_id
                    ));
                }
            }
            if let Some(content) = concept_modifier_items(ds) {
                out.put(content);
            }
        }
        "KEY OBJECT DOC" => {
            if let Some(content) = concept_modifier_items(ds) {
                out.put(content);
            }
        }
        _ => {}
    }

    out.put(DataElement::new(
        REFERENCED_FILE_ID,
        VR::CS,
        PrimitiveValue::Strs(item.entry.file_id.iter().cloned().collect()),
    ));
    out.put(DataElement::new(REFERENCED_SOP_CLASS_UID_IN_FILE, VR::UI, PrimitiveValue::from(item.sop_class_uid.clone())));
    out.put(DataElement::new(
        REFERENCED_SOP_INSTANCE_UID_IN_FILE,
        VR::UI,
        PrimitiveValue::from(item.sop_instance_uid.clone()),
    ));
    out.put(DataElement::new(
        REFERENCED_TRANSFER_SYNTAX_UID_IN_FILE,
        VR::UI,
        PrimitiveValue::from(trim_value(&item.entry.transfer_syntax_uid).to_owned()),
    ));
    if let Some(related) = ds.get(RELATED_GENERAL_SOP_CLASS_UID).filter(|e| !element_string(e).is_empty()) {
        out.put(DataElement::new(
            REFERENCED_RELATED_GENERAL_SOP_CLASS_UID_IN_FILE,
            VR::UI,
            related.value().primitive().cloned().unwrap_or(PrimitiveValue::Empty),
        ));
    }
    out
}

/// A leaf key from the instance: top level first; for H.3-2's Referenced Image Sequence, Image
/// Position/Orientation (Patient) and Pixel Spacing, also inside the Shared Functional Groups
/// Sequence (directly in its item, or one functional group macro sequence down).
fn lookup_leaf_key(ds: &InMemDicomObject, tag: Tag, record_type: &str) -> Option<InMemElement> {
    if let Some(element) = non_empty_element(ds, tag) {
        return Some(element.clone());
    }
    let shared_lookup = matches!(record_type, "IMAGE" | "SPECTROSCOPY")
        && matches!(tag, REFERENCED_IMAGE_SEQUENCE | IMAGE_POSITION_PATIENT | IMAGE_ORIENTATION_PATIENT | PIXEL_SPACING);
    if !shared_lookup {
        return None;
    }
    let shared = ds.get(SHARED_FUNCTIONAL_GROUPS_SEQUENCE)?.items()?.first()?;
    if let Some(element) = non_empty_element(shared, tag) {
        return Some(element.clone());
    }
    shared.iter().find_map(|group| {
        group.items()?.first().and_then(|item| non_empty_element(item, tag)).cloned()
    })
}

/// F.5.25/F.5.26 Content Sequence: "All, and only, Content Items with the HAS CONCEPT MOD
/// relationship from the root Content Item".
fn concept_modifier_items(ds: &InMemDicomObject) -> Option<InMemElement> {
    let items: Vec<InMemDicomObject> = ds
        .get(CONTENT_SEQUENCE)?
        .items()?
        .iter()
        .filter(|item| get_str(item, RELATIONSHIP_TYPE).as_deref() == Some("HAS CONCEPT MOD"))
        .map(|item| InMemDicomObject::from_element_iter(item.iter().map(clean_element)))
        .collect();
    (!items.is_empty()).then(|| {
        DataElement::new(CONTENT_SEQUENCE, VR::SQ, DataSetSequence::new(items, Length::UNDEFINED))
    })
}

/// F.5.25 Verification DateTime: "Most recent Date and Time of verification among those defined
/// in the Verifying Observer Sequence".
fn latest_verification_datetime(ds: &InMemDicomObject) -> Option<String> {
    ds.get(VERIFYING_OBSERVER_SEQUENCE)?
        .items()?
        .iter()
        .filter_map(|item| get_str(item, VERIFICATION_DATE_TIME))
        .max_by(|a, b| normalize_datetime(a).cmp(&normalize_datetime(b)))
}

/// Pad a DT value to full precision so lexical order is chronological (offsets ignored).
fn normalize_datetime(dt: &str) -> String {
    let core: String = dt.chars().take_while(|c| *c != '+' && *c != '-').collect();
    format!("{core:0<21}")
}

/// Put a value the instances didn't carry and report it. `from` is the instance attribute it
/// was copied from, if any.
fn synthesize(
    keys: &mut InMemDicomObject,
    report: &mut DicomdirReport,
    record_type: &str,
    entity: &str,
    key: &Key,
    value: String,
    from: Option<Tag>,
) {
    keys.put(DataElement::new(key.tag, key.vr, PrimitiveValue::from(value.clone())));
    report.synthesized.push(DicomdirSynthesized {
        record_type: record_type.into(),
        entity: entity.to_owned(),
        attribute: key.keyword.into(),
        value,
        source: from.map(tag_keyword),
    });
}

fn tag_keyword(tag: Tag) -> String {
    use dcmnorm_core::dictionary::{DataDictionary, DataDictionaryEntry};
    dcmnorm_dictionary::StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.alias().to_owned())
        .unwrap_or_else(|| format!("({:04X},{:04X})", tag.group(), tag.element()))
}

/// The highest integer value of `tag` among `instances` (0 when none has one), so synthesized
/// numbers continue after the real ones and never collide with them.
fn max_number<'r, 'e: 'r>(instances: impl IntoIterator<Item = &'r Resolved<'e>>, tag: Tag) -> i64 {
    instances
        .into_iter()
        .filter_map(|r| get_str(&r.entry.dataset, tag))
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .map(|n| n as i64)
        .max()
        .unwrap_or(0)
        .max(0)
}

/// The four Directory Elements every record starts with (offsets filled in later).
fn put_record_header(keys: &mut InMemDicomObject, record_type: &str) {
    keys.put(DataElement::new(OFFSET_NEXT, VR::UL, PrimitiveValue::from(0u32)));
    keys.put(DataElement::new(RECORD_IN_USE_FLAG, VR::US, PrimitiveValue::from(0xFFFFu16)));
    keys.put(DataElement::new(OFFSET_LOWER, VR::UL, PrimitiveValue::from(0u32)));
    keys.put(DataElement::new(DIRECTORY_RECORD_TYPE, VR::CS, PrimitiveValue::from(record_type.to_owned())));
}

/// PS3.3 F.5: Specific Character Set is "Required if an extended or replacement character set
/// is used in one of the Keys".
fn add_character_set(keys: &mut InMemDicomObject) {
    if has_non_ascii_text(keys) {
        keys.put(DataElement::new(SPECIFIC_CHARACTER_SET, VR::CS, PrimitiveValue::from(UTF8_CHARACTER_SET.to_owned())));
    } else {
        keys.remove_element(SPECIFIC_CHARACTER_SET);
    }
}

fn has_non_ascii_text(object: &InMemDicomObject) -> bool {
    object.iter().any(|element| match element.value() {
        Value::Primitive(PrimitiveValue::Str(s)) => !s.is_ascii(),
        Value::Primitive(PrimitiveValue::Strs(values)) => values.iter().any(|s| !s.is_ascii()),
        Value::Sequence(seq) => seq.items().iter().any(has_non_ascii_text),
        _ => false,
    })
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

fn encode_dicomdir(tree: &Tree, file_set_id: &str, sop_instance_uid: Option<&str>) -> Result<Vec<u8>, DicomdirError> {
    let ts = TransferSyntaxRegistry
        .get(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .expect("Explicit VR Little Endian must be registered");

    let instance_uid = sop_instance_uid.map(str::to_owned).unwrap_or_else(generate_uid);
    let meta = FileMetaTableBuilder::new()
        .information_version([0x00, 0x01])
        .media_storage_sop_class_uid(MEDIA_STORAGE_DIRECTORY_STORAGE)
        .media_storage_sop_instance_uid(instance_uid)
        .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .build()
        .map_err(|e| DicomdirError::Io(std::io::Error::other(e.to_string())))?;
    let mut meta_bytes = Vec::new();
    meta.write_to(&mut meta_bytes).map_err(DicomdirError::Write)?;

    // Depth-first order: a record, then its lower-level entity, then its next sibling.
    let mut order = Vec::with_capacity(tree.records.len());
    let mut stack: Vec<usize> = tree.roots.iter().rev().copied().collect();
    while let Some(index) = stack.pop() {
        order.push(index);
        stack.extend(tree.records[index].children.iter().rev().copied());
    }

    let header_with = |first: u32, last: u32| -> Result<Vec<u8>, DicomdirError> {
        let header = InMemDicomObject::from_element_iter([
            DataElement::new(FILE_SET_ID, VR::CS, PrimitiveValue::from(file_set_id.to_owned())),
            DataElement::new(OFFSET_FIRST_ROOT, VR::UL, PrimitiveValue::from(first)),
            DataElement::new(OFFSET_LAST_ROOT, VR::UL, PrimitiveValue::from(last)),
            DataElement::new(FILE_SET_CONSISTENCY_FLAG, VR::US, PrimitiveValue::from(0x0000u16)),
        ]);
        let mut bytes = Vec::new();
        header.write_dataset_with_ts(&mut bytes, ts).map_err(DicomdirError::Write)?;
        Ok(bytes)
    };
    let encode = |keys: &InMemDicomObject| -> Result<Vec<u8>, DicomdirError> {
        let mut bytes = Vec::new();
        keys.write_dataset_with_ts(&mut bytes, ts).map_err(DicomdirError::Write)?;
        Ok(bytes)
    };

    // Pass 1: measure. Every offset is a 4-byte UL, so sizes don't depend on offset values.
    let header_len = header_with(0, 0)?.len();
    const SEQUENCE_HEADER_LEN: usize = 12; // see the (0004,1220) header written below
    let mut offsets = vec![0u32; tree.records.len()];
    let mut position = meta_bytes.len() + header_len + SEQUENCE_HEADER_LEN;
    let mut measured = vec![0usize; tree.records.len()];
    for &index in &order {
        let len = encode(&tree.records[index].keys)?.len();
        offsets[index] = u32::try_from(position).map_err(|_| too_large())?;
        measured[index] = len;
        position += 8 + len;
    }

    // Pass 2: fill offsets and encode for real.
    let mut next_sibling = vec![None; tree.records.len()];
    for siblings in std::iter::once(&tree.roots).chain(tree.records.iter().map(|r| &r.children)) {
        for pair in siblings.windows(2) {
            next_sibling[pair[0]] = Some(pair[1]);
        }
    }
    let offset_of_first = |indices: &[usize]| indices.first().map(|&i| offsets[i]).unwrap_or(0);
    let mut items = Vec::with_capacity(position);
    for &index in &order {
        let record = &tree.records[index];
        let mut keys = record.keys.clone();
        keys.put(DataElement::new(OFFSET_LOWER, VR::UL, PrimitiveValue::from(offset_of_first(&record.children))));
        let next = next_sibling[index].map(|i| offsets[i]).unwrap_or(0);
        keys.put(DataElement::new(OFFSET_NEXT, VR::UL, PrimitiveValue::from(next)));
        let bytes = encode(&keys)?;
        assert_eq!(bytes.len(), measured[index], "a DICOMDIR record changed size when its offsets were filled in");
        items.extend_from_slice(&ITEM_TAG_BYTES);
        items.extend_from_slice(&u32::try_from(bytes.len()).map_err(|_| too_large())?.to_le_bytes());
        items.extend_from_slice(&bytes);
    }

    let first = offset_of_first(&tree.roots);
    let last = tree.roots.last().map(|&i| offsets[i]).unwrap_or(0);
    let header = header_with(first, last)?;
    debug_assert_eq!(header.len(), header_len);

    let mut out = meta_bytes;
    out.extend_from_slice(&header);
    // (0004,1220) SQ, defined length, Explicit VR Little Endian: tag, "SQ", 2 reserved bytes,
    // 32-bit length.
    out.extend_from_slice(&DIRECTORY_RECORD_SEQUENCE.group().to_le_bytes());
    out.extend_from_slice(&DIRECTORY_RECORD_SEQUENCE.element().to_le_bytes());
    out.extend_from_slice(b"SQ");
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&u32::try_from(items.len()).map_err(|_| too_large())?.to_le_bytes());
    out.extend_from_slice(&items);
    Ok(out)
}

fn too_large() -> DicomdirError {
    DicomdirError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, "DICOMDIR would exceed 4 GiB"))
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn trim_value(s: &str) -> &str {
    s.trim_matches(|c: char| c == ' ' || c == '\0')
}

fn element_string(element: &InMemElement) -> String {
    if element.vr() == VR::UN {
        // A standard attribute that arrived as UN (Implicit VR converted to Explicit VR by a
        // system that didn't know the tag): its bytes are the text value.
        return element
            .to_bytes()
            .map(|b| trim_value(&String::from_utf8_lossy(&b)).to_owned())
            .unwrap_or_default();
    }
    element.to_str().map(|s| trim_value(&s).to_owned()).unwrap_or_default()
}

/// A copy of `element` fit for a record key of type `key`: UN values of standard attributes are
/// given the key's VR (text split on backslashes, US/UL decoded little endian), so the record
/// doesn't carry UN keys. `None` for a UN value that can't be reinterpreted (a sequence).
fn key_element(element: &InMemElement, key: &Key) -> Option<InMemElement> {
    if element.vr() != VR::UN || key.vr == VR::UN {
        return Some(clean_element(element));
    }
    let bytes = element.to_bytes().ok()?;
    let value = match key.vr {
        VR::SQ => return None,
        VR::US => PrimitiveValue::U16(bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()),
        VR::UL => PrimitiveValue::U32(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()),
        _ => {
            let text = String::from_utf8_lossy(&bytes);
            let values: Vec<String> = text.split('\\').map(|v| trim_value(v).to_owned()).collect();
            if values.len() == 1 {
                PrimitiveValue::from(values.into_iter().next().unwrap_or_default())
            } else {
                PrimitiveValue::Strs(values.into_iter().collect())
            }
        }
    };
    Some(DataElement::new(key.tag, key.vr, value))
}

fn get_str(ds: &InMemDicomObject, tag: Tag) -> Option<String> {
    ds.get(tag).map(element_string).filter(|s| !s.is_empty())
}

/// The element if it has a value: a non-empty primitive, or a sequence with at least one item.
fn non_empty_element(ds: &InMemDicomObject, tag: Tag) -> Option<&InMemElement> {
    let element = ds.get(tag)?;
    let has_value = match element.value() {
        Value::Sequence(seq) => !seq.items().is_empty(),
        Value::Primitive(PrimitiveValue::Empty) => false,
        Value::Primitive(PrimitiveValue::Str(_) | PrimitiveValue::Strs(_)) => !element_string(element).is_empty(),
        Value::Primitive(_) if element.vr() == VR::UN => !element_string(element).is_empty(),
        Value::Primitive(_) => true,
        Value::PixelSequence(_) => false,
    };
    has_value.then_some(element)
}

fn empty_element(key: &Key) -> InMemElement {
    if key.vr == VR::SQ {
        DataElement::new(key.tag, VR::SQ, DataSetSequence::<InMemDicomObject>::new(Vec::new(), Length::UNDEFINED))
    } else {
        DataElement::new(key.tag, key.vr, PrimitiveValue::Empty)
    }
}

/// A copy of `element` with its text values re-padded the standard way: instances often pad
/// with NUL instead of space, or keep stray padding, and the DICOMDIR "shall be correctly encoded
/// regardless of the correctness of any referenced SOP Instances" (IHE RAD TF-2 4.47.4.1.2.3.1).
/// Trailing NUL/space is dropped from every text value (the writer pads again), leading spaces
/// too where PS3.5 calls them insignificant. Sequences are cleaned item by item.
fn clean_element(element: &InMemElement) -> InMemElement {
    let vr = element.vr();
    match element.value() {
        Value::Sequence(seq) => {
            let items: Vec<InMemDicomObject> = seq
                .items()
                .iter()
                .map(|item| InMemDicomObject::from_element_iter(item.iter().map(clean_element)))
                .collect();
            DataElement::new(element.tag(), VR::SQ, DataSetSequence::new(items, Length::UNDEFINED))
        }
        Value::Primitive(PrimitiveValue::Str(text)) => {
            DataElement::new(element.tag(), vr, PrimitiveValue::from(clean_text(text, vr)))
        }
        Value::Primitive(PrimitiveValue::Strs(values)) => DataElement::new(
            element.tag(),
            vr,
            PrimitiveValue::Strs(values.iter().map(|v| clean_text(v, vr)).collect()),
        ),
        _ => element.clone(),
    }
}

fn clean_text(text: &str, vr: VR) -> String {
    let trimmed = text.trim_end_matches(['\0', ' ']);
    let leading_insignificant = matches!(
        vr,
        VR::AE | VR::CS | VR::DA | VR::DS | VR::DT | VR::IS | VR::LO | VR::SH | VR::TM | VR::UI
    );
    if leading_insignificant { trimmed.trim_start_matches(' ') } else { trimmed }.to_owned()
}

/// Trailing component separators and padding don't make a different name ("DOE^JANE^^^" is
/// "DOE^JANE").
fn normalize_name(name: &str) -> String {
    name.split('=')
        .map(|group| group.trim_end_matches(['^', ' ']).trim())
        .collect::<Vec<_>>()
        .join("=")
        .trim_end_matches('=')
        .to_owned()
}

fn names_match(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a.eq_ignore_ascii_case(b)
}

fn strip_bulk_data_uris(value: &mut JsonValue) {
    match value {
        JsonValue::Object(map) => {
            map.retain(|_, member| !matches!(member, JsonValue::Object(m) if m.contains_key("BulkDataURI")));
            for member in map.values_mut() {
                strip_bulk_data_uris(member);
            }
        }
        JsonValue::Array(items) => items.iter_mut().for_each(strip_bulk_data_uris),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // ---- an independent reader: walks the bytes by hand (Explicit VR Little Endian) ----------

    /// One Directory Record as found in the file: the absolute offset of its item tag, and its
    /// elements' raw values by tag (nested sequences kept as raw bytes).
    #[derive(Debug)]
    struct RawRecord {
        offset: u32,
        elements: BTreeMap<(u16, u16), Vec<u8>>,
    }

    impl RawRecord {
        fn ul(&self, g: u16, e: u16) -> u32 {
            let v = &self.elements[&(g, e)];
            u32::from_le_bytes([v[0], v[1], v[2], v[3]])
        }
        fn text(&self, g: u16, e: u16) -> Option<String> {
            self.elements
                .get(&(g, e))
                .map(|v| String::from_utf8_lossy(v).trim_end_matches([' ', '\0']).to_owned())
        }
        fn record_type(&self) -> String {
            self.text(0x0004, 0x1430).unwrap()
        }
    }

    struct RawDicomdir {
        top: BTreeMap<(u16, u16), Vec<u8>>,
        records: Vec<RawRecord>,
    }

    fn u16_at(b: &[u8], p: usize) -> u16 {
        u16::from_le_bytes([b[p], b[p + 1]])
    }
    fn u32_at(b: &[u8], p: usize) -> u32 {
        u32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
    }

    /// Read one element header at `p`: (tag, vr, value length, header length).
    fn header(b: &[u8], p: usize) -> ((u16, u16), [u8; 2], u32, usize) {
        let tag = (u16_at(b, p), u16_at(b, p + 2));
        let vr = [b[p + 4], b[p + 5]];
        match &vr {
            b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN" | b"UC" | b"UR" | b"OD" | b"OL" | b"OV" | b"SV" | b"UV" => {
                (tag, vr, u32_at(b, p + 8), 12)
            }
            _ => (tag, vr, u16_at(b, p + 6) as u32, 8),
        }
    }

    /// Skip an undefined-length sequence's items starting at `p`; returns the position after
    /// its sequence delimitation item.
    fn skip_undefined_sequence(b: &[u8], mut p: usize) -> usize {
        loop {
            let tag = (u16_at(b, p), u16_at(b, p + 2));
            let len = u32_at(b, p + 4);
            p += 8;
            match tag {
                (0xFFFE, 0xE0DD) => return p,
                (0xFFFE, 0xE000) if len == 0xFFFF_FFFF => p = skip_undefined_item(b, p),
                (0xFFFE, 0xE000) => p += len as usize,
                other => panic!("unexpected {other:04X?} in sequence"),
            }
        }
    }

    fn skip_undefined_item(b: &[u8], mut p: usize) -> usize {
        loop {
            if (u16_at(b, p), u16_at(b, p + 2)) == (0xFFFE, 0xE00D) {
                return p + 8;
            }
            p = skip_element(b, p);
        }
    }

    fn skip_element(b: &[u8], p: usize) -> usize {
        let (_, vr, len, hlen) = header(b, p);
        if &vr == b"SQ" && len == 0xFFFF_FFFF {
            skip_undefined_sequence(b, p + hlen)
        } else {
            p + hlen + len as usize
        }
    }

    fn parse_elements(b: &[u8], mut p: usize, end: usize) -> BTreeMap<(u16, u16), Vec<u8>> {
        let mut out = BTreeMap::new();
        while p < end {
            let (tag, _, len, hlen) = header(b, p);
            let next = skip_element(b, p);
            let value_end = if len == 0xFFFF_FFFF { next } else { p + hlen + len as usize };
            out.insert(tag, b[p + hlen..value_end].to_vec());
            p = next;
        }
        assert_eq!(p, end, "element walk overran its container");
        out
    }

    fn parse_raw(b: &[u8]) -> RawDicomdir {
        assert_eq!(&b[128..132], b"DICM");
        let (tag, _, _, hlen) = header(b, 132);
        assert_eq!(tag, (0x0002, 0x0000));
        let group_len = u32_at(b, 132 + hlen);
        let dataset_start = 132 + hlen + 4 + group_len as usize;
        let mut top = BTreeMap::new();
        let mut records = Vec::new();
        let mut p = dataset_start;
        while p < b.len() {
            let (tag, vr, len, hlen) = header(b, p);
            if tag == (0x0004, 0x1220) {
                assert_eq!(&vr, b"SQ");
                assert_ne!(len, 0xFFFF_FFFF, "the record sequence is written with a defined length");
                let end = p + hlen + len as usize;
                let mut q = p + hlen;
                while q < end {
                    assert_eq!((u16_at(b, q), u16_at(b, q + 2)), (0xFFFE, 0xE000));
                    let item_len = u32_at(b, q + 4) as usize;
                    records.push(RawRecord { offset: q as u32, elements: parse_elements(b, q + 8, q + 8 + item_len) });
                    q += 8 + item_len;
                }
                p = end;
            } else {
                top.insert(tag, b[p + hlen..p + hlen + len as usize].to_vec());
                p += hlen + len as usize;
            }
        }
        RawDicomdir { top, records }
    }

    /// Follow the offsets from the root, depth first, returning (depth, record index) in walk
    /// order. Every offset must land exactly on a record's item tag.
    fn walk(raw: &RawDicomdir) -> Vec<(usize, usize)> {
        let by_offset: HashMap<u32, usize> = raw.records.iter().enumerate().map(|(i, r)| (r.offset, i)).collect();
        let first = u32_at(&raw.top[&(0x0004, 0x1200)], 0);
        let last = u32_at(&raw.top[&(0x0004, 0x1202)], 0);
        let mut out = Vec::new();
        fn entity(raw: &RawDicomdir, by_offset: &HashMap<u32, usize>, mut offset: u32, depth: usize, out: &mut Vec<(usize, usize)>) -> u32 {
            let mut last = 0;
            while offset != 0 {
                let index = *by_offset.get(&offset).unwrap_or_else(|| panic!("offset {offset} is not a record"));
                out.push((depth, index));
                last = offset;
                let record = &raw.records[index];
                assert_eq!(record.elements[&(0x0004, 0x1410)], vec![0xFF, 0xFF], "Record In-use Flag FFFFH");
                let lower = record.ul(0x0004, 0x1420);
                if lower != 0 {
                    entity(raw, by_offset, lower, depth + 1, out);
                }
                offset = record.ul(0x0004, 0x1400);
            }
            last
        }
        let found_last = entity(raw, &by_offset, first, 0, &mut out);
        assert_eq!(found_last, last, "(0004,1202) must be the last root record");
        out
    }

    // ---- fixtures ------------------------------------------------------------------------------

    fn ds(elements: &[(u16, u16, VR, &str)]) -> InMemDicomObject {
        InMemDicomObject::from_element_iter(
            elements.iter().map(|&(g, e, vr, v)| DataElement::new(Tag(g, e), vr, PrimitiveValue::from(v.to_owned()))),
        )
    }

    fn image(patient: &str, study: &str, series: &str, sop: &str, number: &str) -> InMemDicomObject {
        let mut out = ds(&[
            (0x0008, 0x0016, VR::UI, "1.2.840.10008.5.1.4.1.1.2"),
            (0x0008, 0x0018, VR::UI, sop),
            (0x0008, 0x0020, VR::DA, "20240102"),
            (0x0008, 0x0030, VR::TM, "101500"),
            (0x0008, 0x0050, VR::SH, "ACC1"),
            (0x0008, 0x0060, VR::CS, "CT"),
            (0x0010, 0x0010, VR::PN, "DOE^JANE"),
            (0x0010, 0x0020, VR::LO, patient),
            (0x0020, 0x000D, VR::UI, study),
            (0x0020, 0x000E, VR::UI, series),
            (0x0020, 0x0010, VR::SH, "S1"),
            (0x0020, 0x0011, VR::IS, "3"),
            (0x0020, 0x0013, VR::IS, number),
        ]);
        out.put(DataElement::new(ROWS, VR::US, PrimitiveValue::from(16u16)));
        out.put(DataElement::new(COLUMNS, VR::US, PrimitiveValue::from(16u16)));
        out
    }

    fn entry(file_id: &str, dataset: InMemDicomObject) -> DicomdirEntry {
        DicomdirEntry {
            file_id: file_id.split('/').map(str::to_owned).collect(),
            dataset,
            transfer_syntax_uid: uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned(),
            sop_class_uid: None,
            sop_instance_uid: None,
        }
    }

    fn options() -> DicomdirOptions {
        DicomdirOptions { file_set_id: "TESTSET".into(), sop_instance_uid: Some("2.25.1".into()) }
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test").join("files").join(name)
    }

    // ---- tests -----------------------------------------------------------------------------------

    #[test]
    fn offsets_link_every_record_depth_first() {
        let entries = vec![
            entry("DICOM/P1/ST1/SE1/IM1", image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "1")),
            entry("DICOM/P1/ST1/SE1/IM2", image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.2", "2")),
            entry("DICOM/P1/ST1/SE2/IM1", image("P1", "1.2.1", "1.2.1.2", "1.2.1.2.1", "1")),
            entry("DICOM/P1/ST2/SE1/IM1", image("P1", "1.2.2", "1.2.2.1", "1.2.2.1.1", "1")),
            entry("DICOM/P2/ST1/SE1/IM1", image("P2", "1.3.1", "1.3.1.1", "1.3.1.1.1", "1")),
        ];
        let (bytes, report) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        assert_eq!(raw.records.len(), 2 + 3 + 4 + 5);
        assert_eq!(report.record_count, 14);

        let walked = walk(&raw);
        assert_eq!(walked.len(), raw.records.len(), "every record reachable exactly once");
        let shape: Vec<(usize, String)> = walked.iter().map(|&(d, i)| (d, raw.records[i].record_type())).collect();
        let expected = [
            (0, "PATIENT"), (1, "STUDY"), (2, "SERIES"), (3, "IMAGE"), (3, "IMAGE"), (2, "SERIES"), (3, "IMAGE"),
            (1, "STUDY"), (2, "SERIES"), (3, "IMAGE"),
            (0, "PATIENT"), (1, "STUDY"), (2, "SERIES"), (3, "IMAGE"),
        ];
        assert_eq!(shape, expected.iter().map(|&(d, t)| (d, t.to_owned())).collect::<Vec<_>>());
        // depth-first layout: the walk order is the file order
        let file_order: Vec<usize> = walked.iter().map(|&(_, i)| i).collect();
        assert_eq!(file_order, (0..raw.records.len()).collect::<Vec<_>>());

        // file-set elements
        assert_eq!(String::from_utf8_lossy(&raw.top[&(0x0004, 0x1130)]).trim_end(), "TESTSET");
        assert_eq!(raw.top[&(0x0004, 0x1212)], vec![0, 0]);
        // leaf references
        let image = &raw.records[3];
        assert_eq!(image.text(0x0004, 0x1500).unwrap(), "DICOM\\P1\\ST1\\SE1\\IM1");
        assert_eq!(image.text(0x0004, 0x1510).unwrap(), "1.2.840.10008.5.1.4.1.1.2");
        assert_eq!(image.text(0x0004, 0x1511).unwrap(), "1.2.1.1.1");
        assert_eq!(image.text(0x0004, 0x1512).unwrap(), uids::EXPLICIT_VR_LITTLE_ENDIAN);
        assert_eq!(image.ul(0x0004, 0x1420), 0, "an IMAGE has no lower-level entity");
        // only leaves reference files
        assert!(raw.records[0].elements.get(&(0x0004, 0x1500)).is_none());
        assert!(report.synthesized.is_empty(), "{:?}", report.synthesized);
    }

    #[test]
    fn written_file_reads_back_as_media_storage_directory() {
        let entries = vec![entry("IM1", image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "1"))];
        let (bytes, _) = build_dicomdir(&entries, &options()).unwrap();
        let object = crate::dicom_io::read_dicom_bytes(&bytes).unwrap();
        assert_eq!(object.meta().media_storage_sop_class_uid(), MEDIA_STORAGE_DIRECTORY_STORAGE);
        assert_eq!(trim_value(object.meta().transfer_syntax()), uids::EXPLICIT_VR_LITTLE_ENDIAN);
        assert_eq!(object.meta().media_storage_sop_instance_uid(), "2.25.1");
        assert_eq!(object.meta().information_version, [0, 1]);
        let records = object.get(DIRECTORY_RECORD_SEQUENCE).unwrap().items().unwrap();
        assert_eq!(records.len(), 4);
    }

    #[test]
    fn required_keys_per_level() {
        let entries = vec![entry("IM1", image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "7"))];
        let (bytes, _) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        let [patient, study, series, image] = &raw.records[..] else { panic!() };
        assert_eq!(patient.text(0x0010, 0x0020).unwrap(), "P1");
        assert_eq!(patient.text(0x0010, 0x0010).unwrap(), "DOE^JANE");
        assert_eq!(study.text(0x0008, 0x0020).unwrap(), "20240102");
        assert_eq!(study.text(0x0008, 0x0030).unwrap(), "101500");
        assert_eq!(study.text(0x0020, 0x000D).unwrap(), "1.2.1");
        assert_eq!(study.text(0x0020, 0x0010).unwrap(), "S1");
        assert_eq!(study.text(0x0008, 0x0050).unwrap(), "ACC1");
        assert_eq!(study.text(0x0008, 0x1030).unwrap(), "", "Type 2 Study Description present, empty");
        assert_eq!(series.text(0x0008, 0x0060).unwrap(), "CT");
        assert_eq!(series.text(0x0020, 0x000E).unwrap(), "1.2.1.1");
        assert_eq!(series.text(0x0020, 0x0011).unwrap(), "3");
        assert_eq!(image.text(0x0020, 0x0013).unwrap(), "7");
        assert!(image.elements.contains_key(&(0x0028, 0x0010)), "H.3-2 Rows copied");
        for record in &raw.records {
            assert!(!record.elements.contains_key(&(0x0008, 0x0005)), "ASCII-only records carry no charset");
        }
    }

    #[test]
    fn synthesizes_ihe_type1_keys_distinct_per_entity() {
        let strip = |mut d: InMemDicomObject, tags: &[Tag]| {
            for t in tags {
                d.remove_element(*t);
            }
            d
        };
        let gone = [PATIENT_ID, STUDY_ID, STUDY_DATE, STUDY_TIME, MODALITY, SERIES_NUMBER, INSTANCE_NUMBER];
        let mut a = strip(image("", "1.2.1", "1.2.1.1", "1.2.1.1.1", ""), &gone);
        a.put(DataElement::new(SERIES_DATE, VR::DA, PrimitiveValue::from("20230505".to_owned())));
        let b = strip(image("", "1.2.1", "1.2.1.1", "1.2.1.1.2", ""), &gone);
        let c = strip(image("", "1.2.1", "1.2.1.2", "1.2.1.2.1", ""), &gone);
        let d = strip(image("", "1.2.2", "1.2.2.1", "1.2.2.1.1", ""), &gone);
        let entries = vec![entry("A", a), entry("B", b), entry("C", c), entry("D", d)];
        let (bytes, report) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        let of_type = |t: &str| raw.records.iter().filter(|r| r.record_type() == t).collect::<Vec<_>>();

        let patients = of_type("PATIENT");
        assert_eq!(patients.len(), 1, "same name and birth date, no ID: one patient");
        assert_eq!(patients[0].text(0x0010, 0x0020).unwrap(), "SYNTH_PAT_0001");

        let studies = of_type("STUDY");
        let ids: HashSet<_> = studies.iter().map(|s| s.text(0x0020, 0x0010).unwrap()).collect();
        assert_eq!(ids.len(), 2, "two studies get two Study IDs");
        assert_eq!(studies[0].text(0x0008, 0x0020).unwrap(), "20230505", "Study Date from Series Date");
        assert_eq!(studies[0].text(0x0008, 0x0030).unwrap(), "000000");
        assert_eq!(studies[1].text(0x0008, 0x0020).unwrap().len(), 8, "no date anywhere: today's");

        let series = of_type("SERIES");
        assert!(series.iter().all(|s| s.text(0x0008, 0x0060).unwrap() == "OT"));
        assert_eq!(series[0].text(0x0020, 0x0011).unwrap(), "1");
        assert_eq!(series[1].text(0x0020, 0x0011).unwrap(), "2", "distinct within the study");

        let images = of_type("IMAGE");
        assert_eq!(images[0].text(0x0020, 0x0013).unwrap(), "1");
        assert_eq!(images[1].text(0x0020, 0x0013).unwrap(), "2", "distinct within the series");

        let synthesized: HashSet<_> = report.synthesized.iter().map(|s| (s.record_type.as_str(), s.attribute.as_str())).collect();
        for expected in [
            ("PATIENT", "PatientID"),
            ("STUDY", "StudyID"),
            ("STUDY", "StudyDate"),
            ("STUDY", "StudyTime"),
            ("SERIES", "Modality"),
            ("SERIES", "SeriesNumber"),
            ("IMAGE", "InstanceNumber"),
        ] {
            assert!(synthesized.contains(&expected), "{expected:?} not reported: {:?}", report.synthesized);
        }
        let from_series_date = report.synthesized.iter().find(|s| s.attribute == "StudyDate" && s.value == "20230505").unwrap();
        assert_eq!(from_series_date.source.as_deref(), Some("SeriesDate"));
    }

    #[test]
    fn studies_without_patient_id_or_name_are_separate_patients() {
        let anonymous = |study: &str| {
            let mut d = image("", study, &format!("{study}.1"), &format!("{study}.1.1"), "1");
            d.remove_element(PATIENT_ID);
            d.remove_element(PATIENT_NAME);
            d
        };
        let entries = vec![entry("A", anonymous("1.5.1")), entry("B", anonymous("1.5.2"))];
        let (bytes, report) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        let ids: Vec<_> = raw
            .records
            .iter()
            .filter(|r| r.record_type() == "PATIENT")
            .map(|r| r.text(0x0010, 0x0020).unwrap())
            .collect();
        assert_eq!(ids, ["SYNTH_PAT_0001", "SYNTH_PAT_0002"]);
        assert_eq!(report.synthesized.iter().filter(|s| s.attribute == "PatientID").count(), 2);
    }

    #[test]
    fn synthesized_numbers_skip_existing_ones() {
        let mut no_number = image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.2", "");
        no_number.remove_element(INSTANCE_NUMBER);
        let entries = vec![entry("A", image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "5")), entry("B", no_number)];
        let (bytes, _) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        let numbers: Vec<_> =
            raw.records.iter().filter(|r| r.record_type() == "IMAGE").map(|r| r.text(0x0020, 0x0013).unwrap()).collect();
        assert_eq!(numbers, ["5", "6"]);
    }

    #[test]
    fn record_types_for_non_image_objects() {
        let leaf = |sop_class: &str, sop: &str, extra: &[(u16, u16, VR, &str)]| {
            let mut d = ds(&[
                (0x0008, 0x0016, VR::UI, sop_class),
                (0x0008, 0x0018, VR::UI, sop),
                (0x0008, 0x0023, VR::DA, "20240101"),
                (0x0008, 0x0033, VR::TM, "120000"),
                (0x0010, 0x0020, VR::LO, "P1"),
                (0x0020, 0x000D, VR::UI, "1.2.1"),
                (0x0020, 0x000E, VR::UI, &format!("{sop}.9")),
                (0x0020, 0x0013, VR::IS, "1"),
            ]);
            for &(g, e, vr, v) in extra {
                d.put(DataElement::new(Tag(g, e), vr, PrimitiveValue::from(v.to_owned())));
            }
            d
        };
        let mut sr = leaf("1.2.840.10008.5.1.4.1.1.88.33", "1.9.1", &[
            (0x0040, 0xA491, VR::CS, "COMPLETE"),
            (0x0040, 0xA493, VR::CS, "VERIFIED"),
        ]);
        let code = ds(&[(0x0008, 0x0100, VR::SH, "18748-4"), (0x0008, 0x0102, VR::SH, "LN"), (0x0008, 0x0104, VR::LO, "Diagnostic Imaging Report")]);
        sr.put(DataElement::new(Tag(0x0040, 0xA043), VR::SQ, DataSetSequence::new(vec![code], Length::UNDEFINED)));
        let observers = vec![
            ds(&[(0x0040, 0xA030, VR::DT, "20240101120000")]),
            ds(&[(0x0040, 0xA030, VR::DT, "20240103090000")]),
        ];
        sr.put(DataElement::new(VERIFYING_OBSERVER_SEQUENCE, VR::SQ, DataSetSequence::new(observers, Length::UNDEFINED)));
        let content = vec![
            ds(&[(0x0040, 0xA010, VR::CS, "HAS CONCEPT MOD"), (0x0040, 0xA040, VR::CS, "CODE")]),
            ds(&[(0x0040, 0xA010, VR::CS, "CONTAINS"), (0x0040, 0xA040, VR::CS, "TEXT")]),
        ];
        sr.put(DataElement::new(CONTENT_SEQUENCE, VR::SQ, DataSetSequence::new(content, Length::UNDEFINED)));

        let mut kos = leaf("1.2.840.10008.5.1.4.1.1.88.59", "1.9.2", &[]);
        let title = ds(&[(0x0008, 0x0100, VR::SH, "113000"), (0x0008, 0x0102, VR::SH, "DCM"), (0x0008, 0x0104, VR::LO, "Of Interest")]);
        kos.put(DataElement::new(Tag(0x0040, 0xA043), VR::SQ, DataSetSequence::new(vec![title], Length::UNDEFINED)));

        let cases: Vec<(&str, InMemDicomObject)> = vec![
            ("SR DOCUMENT", sr),
            ("KEY OBJECT DOC", kos),
            ("PRESENTATION", leaf("1.2.840.10008.5.1.4.1.1.11.1", "1.9.3", &[(0x0070, 0x0082, VR::DA, "20240101"), (0x0070, 0x0083, VR::TM, "1200")])),
            ("ENCAP DOC", leaf("1.2.840.10008.5.1.4.1.1.104.1", "1.9.4", &[(0x0042, 0x0012, VR::LO, "application/pdf")])),
            ("WAVEFORM", leaf("1.2.840.10008.5.1.4.1.1.9.1.1", "1.9.5", &[])),
            ("RT DOSE", leaf("1.2.840.10008.5.1.4.1.1.481.2", "1.9.6", &[(0x3004, 0x000A, VR::CS, "PLAN")])),
            ("RT STRUCTURE SET", leaf("1.2.840.10008.5.1.4.1.1.481.3", "1.9.7", &[(0x3006, 0x0002, VR::SH, "RS")])),
            ("RT PLAN", leaf("1.2.840.10008.5.1.4.1.1.481.5", "1.9.8", &[(0x300A, 0x0002, VR::SH, "PLAN1")])),
            ("REGISTRATION", leaf("1.2.840.10008.5.1.4.1.1.66.1", "1.9.9", &[])),
            ("RADIOTHERAPY", leaf("1.2.840.10008.5.1.4.1.1.481.10", "1.9.10", &[])),
            ("WF PRESENTATION", leaf("1.2.840.10008.5.1.4.1.1.9.100.1", "1.9.11", &[(0x0070, 0x0082, VR::DA, "20240101"), (0x0070, 0x0083, VR::TM, "1200")])),
            ("MEASUREMENT", leaf("1.2.840.10008.5.1.4.1.1.78.1", "1.9.12", &[])),
            ("ANNOTATION", leaf("1.2.840.10008.5.1.4.1.1.91.1", "1.9.13", &[])),
            ("IMAGE", image("P1", "1.2.1", "1.2.1.77", "1.9.14", "1")),
        ];
        let entries: Vec<DicomdirEntry> =
            cases.iter().enumerate().map(|(i, (_, d))| entry(&format!("F{i}"), d.clone())).collect();
        let (bytes, report) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        for (i, (expected, _)) in cases.iter().enumerate() {
            let record = raw.records.iter().find(|r| r.text(0x0004, 0x1500).as_deref() == Some(&format!("F{i}"))).unwrap();
            assert_eq!(record.record_type(), *expected, "F{i}");
        }
        let sr = raw.records.iter().find(|r| r.record_type() == "SR DOCUMENT").unwrap();
        assert_eq!(sr.text(0x0040, 0xA030).unwrap(), "20240103090000", "latest verification");
        assert_eq!(sr.text(0x0040, 0xA491).unwrap(), "COMPLETE");
        let content = &sr.elements[&(0x0040, 0xA730)];
        assert!(content.windows(15).any(|w| w == b"HAS CONCEPT MOD"));
        assert!(!content.windows(8).any(|w| w == b"CONTAINS"), "only HAS CONCEPT MOD items");
        let doc = raw.records.iter().find(|r| r.record_type() == "ENCAP DOC").unwrap();
        assert_eq!(doc.text(0x0042, 0x0012).unwrap(), "application/pdf");
        assert!(doc.elements.contains_key(&(0x0042, 0x0010)), "Type 2 Document Title present");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(walk(&raw).len(), raw.records.len());
    }

    #[test]
    fn root_level_records_sit_beside_patients() {
        let palette = ds(&[
            (0x0008, 0x0016, VR::UI, "1.2.840.10008.5.1.4.39.1"),
            (0x0008, 0x0018, VR::UI, "1.8.1"),
            (0x0070, 0x0080, VR::CS, "HOT_IRON"),
        ]);
        let entries = vec![entry("IM1", image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "1")), entry("PAL1", palette)];
        let (bytes, _) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        let roots: Vec<String> = walk(&raw).iter().filter(|(d, _)| *d == 0).map(|&(_, i)| raw.records[i].record_type()).collect();
        assert_eq!(roots, ["PATIENT", "PALETTE"]);
    }

    #[test]
    fn non_ascii_records_carry_utf8_character_set() {
        let mut d = image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "1");
        d.put(DataElement::new(PATIENT_NAME, VR::PN, PrimitiveValue::from("Müller^Jürgen".to_owned())));
        let (bytes, _) = build_dicomdir(&[entry("IM1", d)], &options()).unwrap();
        let raw = parse_raw(&bytes);
        let patient = &raw.records[0];
        assert_eq!(patient.text(0x0008, 0x0005).unwrap(), "ISO_IR 192");
        assert_eq!(patient.text(0x0010, 0x0010).unwrap(), "Müller^Jürgen");
        assert!(!raw.records[1].elements.contains_key(&(0x0008, 0x0005)), "the ASCII STUDY record has none");
    }

    #[test]
    fn copied_values_are_repadded() {
        let mut d = image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "12\0");
        d.put(DataElement::new(Tag(0x0008, 0x1030), VR::LO, PrimitiveValue::from("HEAD\0".to_owned())));
        d.put(DataElement::new(Tag(0x0028, 0x0030), VR::DS, PrimitiveValue::Strs(["0.5".to_owned(), "0.5\0".to_owned()].into_iter().collect())));
        let (bytes, _) = build_dicomdir(&[entry("IM1", d)], &options()).unwrap();
        assert!(!bytes.windows(5).any(|w| w == b"HEAD\0"), "NUL padding from the instance is not copied");
        let raw = parse_raw(&bytes);
        assert_eq!(raw.records[1].elements[&(0x0008, 0x1030)], b"HEAD".to_vec());
        assert_eq!(raw.records[3].elements[&(0x0020, 0x0013)], b"12".to_vec());
        assert_eq!(raw.records[3].elements[&(0x0028, 0x0030)], b"0.5\\0.5 ".to_vec());
    }

    #[test]
    fn file_id_rules() {
        assert!(validate_file_id(&["DICOM", "ST000001", "SE000001", "IM000001"]).is_ok());
        assert!(validate_file_id(&["A_1"]).is_ok());
        for bad in [
            vec!["dicom"],
            vec!["IM1.DCM"],
            vec!["TOOLONGNAME"],
            vec![""],
            vec!["DICOMDIR"],
            vec!["A", "B", "C", "D", "E", "F", "G", "H", "I"],
            vec!["SP ACE"],
        ] {
            assert!(matches!(validate_file_id(&bad), Err(DicomdirError::InvalidFileId { .. })), "{bad:?}");
        }
        let none: [&str; 0] = [];
        assert!(validate_file_id(&none).is_err());
        assert!(validate_file_set_id("SB20261114ABCD").is_ok());
        assert!(validate_file_set_id("").is_ok());
        assert!(validate_file_set_id("lower").is_err());
        assert!(validate_file_set_id("SEVENTEEN_CHARS_X").is_err());
        assert_eq!(file_id_from_relative_path(Path::new("DICOM/IM1")).unwrap(), ["DICOM", "IM1"]);
        assert!(file_id_from_relative_path(Path::new("../IM1")).is_err());
    }

    #[test]
    fn refuses_inconsistent_file_sets() {
        let e = |id: &str, d| entry(id, d);
        let err = |entries: Vec<DicomdirEntry>| build_dicomdir(&entries, &options()).unwrap_err();

        assert!(matches!(err(vec![]), DicomdirError::NoEntries));
        let one = image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.1", "1");
        assert!(matches!(err(vec![e("A", one.clone()), e("A", image("P1", "1.2.1", "1.2.1.1", "1.2.1.1.2", "2"))]), DicomdirError::DuplicateFileId(_)));
        assert!(matches!(err(vec![e("A", one.clone()), e("B", one.clone())]), DicomdirError::DuplicateSopInstance { .. }));

        let mut other_name = image("P1", "1.2.9", "1.2.9.1", "1.2.9.1.1", "1");
        other_name.put(DataElement::new(PATIENT_NAME, VR::PN, PrimitiveValue::from("ROE^RICHARD".to_owned())));
        assert!(matches!(err(vec![e("A", one.clone()), e("B", other_name)]), DicomdirError::PatientConflict { .. }));

        let study_moved = image("P2", "1.2.1", "1.2.1.5", "1.2.1.5.1", "1");
        assert!(matches!(err(vec![e("A", one.clone()), e("B", study_moved)]), DicomdirError::StudyConflict { .. }));

        let series_moved = image("P1", "1.2.7", "1.2.1.1", "1.2.7.1.1", "1");
        assert!(matches!(err(vec![e("A", one.clone()), e("B", series_moved)]), DicomdirError::SeriesConflict { .. }));

        let mut no_study = one.clone();
        no_study.remove_element(STUDY_INSTANCE_UID);
        assert!(matches!(err(vec![e("A", no_study)]), DicomdirError::MissingAttribute { attribute: "StudyInstanceUID", .. }));

        let not_an_image = ds(&[
            (0x0008, 0x0016, VR::UI, "1.2.840.10008.5.1.4.1.1.200.1"),
            (0x0008, 0x0018, VR::UI, "1.5.5"),
            (0x0020, 0x000D, VR::UI, "1.2.1"),
            (0x0020, 0x000E, VR::UI, "1.2.1.1"),
        ]);
        assert!(matches!(err(vec![e("A", not_an_image)]), DicomdirError::NoRecordType { .. }));

        let bad_set = DicomdirOptions { file_set_id: "bad id".into(), sop_instance_uid: None };
        assert!(matches!(build_dicomdir(&[e("A", one)], &bad_set), Err(DicomdirError::InvalidFileSetId { .. })));
    }

    #[test]
    fn entries_from_fixture_files_and_json() {
        let files = ["ct.dcm", "mr.dcm", "sr2.dcm", "pdf.dcm", "us2.dcm", "rtdose.dcm"];
        let mut entries = Vec::new();
        for (i, name) in files.iter().enumerate() {
            entries.push(DicomdirEntry::from_file(vec!["DICOM".into(), format!("IM{i:06}")], fixture(name)).unwrap());
        }
        // and one from JSON with a BulkDataURI that can't be resolved
        let json = crate::dicom_io::dicom_file_to_json(
            fixture("dx.dcm"),
            crate::dicom_io::DicomJsonWriteOptions {
                bulk_data_mode: crate::dicom_io::DicomJsonBulkDataMode::Uri,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(json.contains("BulkDataURI"));
        entries.push(DicomdirEntry::from_json(vec!["DICOM".into(), "IMJSON".into()], &json, DicomJsonFormat::Flat, None).unwrap());

        let (bytes, report) = build_dicomdir(&entries, &options()).unwrap();
        let raw = parse_raw(&bytes);
        assert_eq!(walk(&raw).len(), raw.records.len());
        let types: HashSet<String> = raw.records.iter().map(RawRecord::record_type).collect();
        for t in ["PATIENT", "STUDY", "SERIES", "IMAGE", "SR DOCUMENT", "ENCAP DOC", "RT DOSE"] {
            assert!(types.contains(t), "{t} missing from {types:?}");
        }
        assert!(report.synthesized.iter().any(|s| s.attribute == "PatientID"), "fixtures without Patient ID");
        let leaves = raw.records.iter().filter(|r| r.elements.contains_key(&(0x0004, 0x1500))).count();
        assert_eq!(leaves, entries.len(), "every file referenced");
    }
}
