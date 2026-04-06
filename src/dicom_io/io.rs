use std::io::Cursor;
use std::path::Path;

use dicom_object::file::ReadPreamble;
use dicom_object::{
    open_file, DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject, OpenFileOptions,
};

use super::types::{DicomIoError, ReadError, WriteError};

pub fn read_dicom_file<P>(path: P) -> Result<DefaultDicomObject, ReadError>
where
    P: AsRef<Path>,
{
    open_file(path)
}

pub fn read_dicom_bytes(bytes: impl AsRef<[u8]>) -> Result<DefaultDicomObject, ReadError> {
    OpenFileOptions::new()
        .read_preamble(ReadPreamble::Always)
        .from_reader(Cursor::new(bytes.as_ref()))
}

pub fn write_dicom_file<P>(object: &DefaultDicomObject, path: P) -> Result<(), WriteError>
where
    P: AsRef<Path>,
{
    object.write_to_file(path).map(|_| ())
}

pub fn write_dicom_bytes(object: &DefaultDicomObject) -> Result<Vec<u8>, WriteError> {
    let mut bytes = Vec::new();
    object.write_all(&mut bytes)?;
    Ok(bytes)
}

pub fn write_dataset_as_dicom_file<P>(
    dataset: InMemDicomObject,
    path: P,
    transfer_syntax_uid: &str,
) -> Result<(), DicomIoError>
where
    P: AsRef<Path>,
{
    let file_object = dataset
        .with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?;

    file_object.write_to_file(path)?;
    Ok(())
}

pub fn write_dataset_as_dicom_bytes(
    dataset: InMemDicomObject,
    transfer_syntax_uid: &str,
) -> Result<Vec<u8>, DicomIoError> {
    let file_object = dataset
        .with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?;

    let mut bytes = Vec::new();
    file_object.write_all(&mut bytes)?;
    Ok(bytes)
}