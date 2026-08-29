//! This project's own DICOM in-memory object model and Part 10 file I/O, replacing `dicom-object`
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
pub use mem::{InMemDicomObject, InMemElement, InMemFragment, MissingElementError};
pub use meta::{FileMetaTable, FileMetaTableBuilder};

/// Fallback implementation class UID used when a [`FileMetaTableBuilder`] doesn't specify one -
/// matches dicom-object's own convention of stamping an implementation identity into files this
/// process writes.
pub const IMPLEMENTATION_CLASS_UID: &str = "2.25.1.dcmnorm";
pub const IMPLEMENTATION_VERSION_NAME: &str = concat!("DCMNORM_", env!("CARGO_PKG_VERSION"));
