//! dcmnorm's own DICOM in-memory object model and Part 10 file I/O, replacing `dicom-object`
//! (+ `dicom-parser`'s tree-building layer). See README.md for the design rationale.

mod error;
mod file;
mod mem;
mod meta;
mod pixel;

pub use error::{ReadError, WithMetaError, WriteError};
pub use file::{
    read_dataset_trial_parse, with_meta_from_bare_dataset, DefaultDicomObject, FileDicomObject,
    OpenFileOptions, ReadPreamble,
};
pub use mem::{ApplyOpError, InMemDicomObject, InMemElement, InMemFragment, MissingElementError};
pub use meta::{FileMetaTable, FileMetaTableBuilder};

/// Fallback implementation class UID used when a [`FileMetaTableBuilder`] doesn't specify one -
/// matches dicom-object's own convention of stamping an implementation identity into files this
/// process writes. UUID-derived (PS3.5 Annex B: `2.25.<uuid-as-decimal>`), generated once and
/// fixed forever - a UID may contain only digits and `.` (PS3.5 6.2), so this can't be a
/// readable "dcmnorm" path the way a private enterprise OID root would allow.
pub const IMPLEMENTATION_CLASS_UID: &str = "2.25.118267540888616781180626367073156920815";
pub const IMPLEMENTATION_VERSION_NAME: &str = concat!("DCMNORM_", env!("CARGO_PKG_VERSION"));
