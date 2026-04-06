use dicom_object::{DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject};
use serde_json::Value as JsonValue;

use super::flat_json::{read_flat_json_value, write_flat_json_value};
use super::standard_json::{read_standard_json_value, write_standard_json_value};
use super::types::{
    DicomJsonBulkDataMode, DicomJsonError, DicomJsonFormat, DicomJsonReadOptions,
    DicomJsonWriteOptions,
};

pub fn read_dicom_json(json: &str) -> Result<DefaultDicomObject, DicomJsonError> {
    read_dicom_json_with_options(json, DicomJsonReadOptions::default())
}

pub fn read_dicom_json_with_source(
    json: &str,
    bulk_data_source: impl AsRef<[u8]>,
) -> Result<DefaultDicomObject, DicomJsonError> {
    read_dicom_json_with_options(
        json,
        DicomJsonReadOptions {
            bulk_data_source: Some(bulk_data_source.as_ref()),
            ..DicomJsonReadOptions::default()
        },
    )
}

pub fn read_dicom_json_full(json: &str) -> Result<DefaultDicomObject, DicomJsonError> {
    read_dicom_json_with_options(
        json,
        DicomJsonReadOptions {
            format: DicomJsonFormat::Standard,
            ..DicomJsonReadOptions::default()
        },
    )
}

pub fn read_dicom_json_full_with_source(
    json: &str,
    bulk_data_source: impl AsRef<[u8]>,
) -> Result<DefaultDicomObject, DicomJsonError> {
    read_dicom_json_with_options(
        json,
        DicomJsonReadOptions {
            format: DicomJsonFormat::Standard,
            bulk_data_source: Some(bulk_data_source.as_ref()),
        },
    )
}

pub fn read_dicom_json_with_options(
    json: &str,
    options: DicomJsonReadOptions<'_>,
) -> Result<DefaultDicomObject, DicomJsonError> {
    let value: JsonValue = serde_json::from_str(json)?;
    match options.format {
        DicomJsonFormat::Flat => read_flat_json_value(&value, options.bulk_data_source),
        DicomJsonFormat::Standard => read_standard_json_value(&value, options.bulk_data_source),
    }
}

pub fn write_dicom_json(object: &DefaultDicomObject) -> Result<String, DicomJsonError> {
    write_dicom_json_with_options(object, DicomJsonWriteOptions::default())
}

pub fn write_dicom_json_with_source(
    object: &DefaultDicomObject,
    bulk_data_source: impl AsRef<[u8]>,
) -> Result<String, DicomJsonError> {
    write_dicom_json_with_options(
        object,
        DicomJsonWriteOptions {
            bulk_data_mode: DicomJsonBulkDataMode::Uri,
            bulk_data_source: Some(bulk_data_source.as_ref()),
            ..DicomJsonWriteOptions::default()
        },
    )
}

pub fn write_dicom_json_full(object: &DefaultDicomObject) -> Result<String, DicomJsonError> {
    write_dicom_json_with_options(
        object,
        DicomJsonWriteOptions {
            format: DicomJsonFormat::Standard,
            ..DicomJsonWriteOptions::default()
        },
    )
}

pub fn write_dicom_json_full_with_source(
    object: &DefaultDicomObject,
    bulk_data_source: impl AsRef<[u8]>,
) -> Result<String, DicomJsonError> {
    write_dicom_json_with_options(
        object,
        DicomJsonWriteOptions {
            format: DicomJsonFormat::Standard,
            bulk_data_mode: DicomJsonBulkDataMode::Uri,
            bulk_data_source: Some(bulk_data_source.as_ref()),
            ..DicomJsonWriteOptions::default()
        },
    )
}

pub fn write_dicom_json_with_options(
    object: &DefaultDicomObject,
    options: DicomJsonWriteOptions<'_>,
) -> Result<String, DicomJsonError> {
    let value = match options.format {
        DicomJsonFormat::Flat => write_flat_json_value(object, options)?,
        DicomJsonFormat::Standard => write_standard_json_value(object, options)?,
    };

    Ok(serde_json::to_string(&value)?)
}

pub fn write_dataset_as_dicom_json(
    dataset: InMemDicomObject,
    transfer_syntax_uid: &str,
) -> Result<String, DicomJsonError> {
    write_dataset_as_dicom_json_with_options(
        dataset,
        transfer_syntax_uid,
        DicomJsonWriteOptions::default(),
    )
}

pub fn write_dataset_as_dicom_json_full(
    dataset: InMemDicomObject,
    transfer_syntax_uid: &str,
) -> Result<String, DicomJsonError> {
    write_dataset_as_dicom_json_with_options(
        dataset,
        transfer_syntax_uid,
        DicomJsonWriteOptions {
            format: DicomJsonFormat::Standard,
            ..DicomJsonWriteOptions::default()
        },
    )
}

pub fn write_dataset_as_dicom_json_with_options(
    dataset: InMemDicomObject,
    transfer_syntax_uid: &str,
    mut options: DicomJsonWriteOptions<'_>,
) -> Result<String, DicomJsonError> {
    let file_object = dataset
        .with_meta(FileMetaTableBuilder::new().transfer_syntax(transfer_syntax_uid))?;

    if options.bulk_data_mode == DicomJsonBulkDataMode::Uri && options.bulk_data_source.is_none() {
        options.bulk_data_mode = DicomJsonBulkDataMode::InlineBinary;
    }

    write_dicom_json_with_options(&file_object, options)
}