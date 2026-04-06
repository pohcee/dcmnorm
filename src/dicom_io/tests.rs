use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dicom_core::{Tag, VR};
use dicom_dictionary_std::tags;
use serde_json::Value as JsonValue;

use super::{
    read_dicom_bytes, read_dicom_file, read_dicom_json, read_dicom_json_full,
    read_dicom_json_full_with_source, read_dicom_json_with_source, write_dicom_bytes,
    write_dicom_file, write_dicom_json, write_dicom_json_full, write_dicom_json_full_with_source,
    write_dicom_json_with_options, write_dicom_json_with_source, DicomJsonKeyStyle,
    DicomJsonWriteOptions,
};

const DX_FIXTURE: &str = "/home/jklotzer/code/dcmnorm/test/files/dx.dcm";
const CT_FIXTURE: &str = "/home/jklotzer/code/dcmnorm/test/files/ct.dcm";
const PRIVATE_TAG: Tag = Tag(0x0013, 0x1010);

#[test]
fn reads_dicom_file_fixture() {
    let object = read_dicom_file(DX_FIXTURE).unwrap();

    assert_eq!(object.element(tags::MODALITY).unwrap().to_str().unwrap(), "DX");
    assert!(object.element(tags::PIXEL_DATA).is_ok());
}

#[test]
fn writes_dicom_file_fixture_round_trip() {
    let original = read_dicom_file(DX_FIXTURE).unwrap();
    let output_path = temp_file_path("dicom-file-roundtrip");

    write_dicom_file(&original, &output_path).unwrap();
    let roundtrip = read_dicom_file(&output_path).unwrap();

    assert_core_fields_match(&original, &roundtrip);

    fs::remove_file(output_path).unwrap();
}

#[test]
fn reads_dicom_bytes_fixture() {
    let bytes = fixture_bytes(DX_FIXTURE);
    let object = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(object.element(tags::MODALITY).unwrap().to_str().unwrap(), "DX");
    assert!(object.element(tags::PIXEL_DATA).is_ok());
}

#[test]
fn writes_dicom_bytes_fixture_round_trip() {
    let original = read_dicom_file(DX_FIXTURE).unwrap();
    let bytes = write_dicom_bytes(&original).unwrap();
    let roundtrip = read_dicom_bytes(&bytes).unwrap();

    assert_core_fields_match(&original, &roundtrip);
}

#[test]
fn writes_flat_json_with_inline_binary_by_default() {
    let object = read_dicom_file(DX_FIXTURE).unwrap();
    let json_text = write_dicom_json(&object).unwrap();
    let json: JsonValue = serde_json::from_str(&json_text).unwrap();

    assert_eq!(json["Modality"], JsonValue::String("DX".to_owned()));
    assert!(json["PixelData"]["InlineBinary"].is_string());
    assert_eq!(json["00131010"]["vr"], JsonValue::String("LO".to_owned()));
    assert!(json["00131010"]["Value"].is_string());

    let roundtrip = read_dicom_json(&json_text).unwrap();
    assert_core_fields_match(&object, &roundtrip);
    assert_eq!(roundtrip.element(PRIVATE_TAG).unwrap().vr(), VR::LO);
}

#[test]
fn writes_flat_json_keys_as_hex_when_requested() {
    let object = read_dicom_file(DX_FIXTURE).unwrap();
    let json_text = write_dicom_json_with_options(
        &object,
        DicomJsonWriteOptions {
            key_style: DicomJsonKeyStyle::Hex,
            ..DicomJsonWriteOptions::default()
        },
    )
    .unwrap();
    let json: JsonValue = serde_json::from_str(&json_text).unwrap();

    assert_eq!(json["00080060"], JsonValue::String("DX".to_owned()));
    assert_eq!(json["00131010"]["vr"], JsonValue::String("LO".to_owned()));
    assert_eq!(json["00020010"], JsonValue::String("1.2.840.10008.1.2.1".to_owned()));

    let roundtrip = read_dicom_json(&json_text).unwrap();
    assert_core_fields_match(&object, &roundtrip);
    assert_eq!(roundtrip.meta().transfer_syntax(), object.meta().transfer_syntax());
}

#[test]
fn writes_and_reads_flat_json_with_bulk_data_uri() {
    let source = fixture_bytes(DX_FIXTURE);
    let original = read_dicom_bytes(&source).unwrap();
    let json = write_dicom_json_with_source(&original, &source).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();

    assert!(value["FileMetaInformationVersion"]["InlineBinary"].is_string());
    assert!(value["FileMetaInformationVersion"]["BulkDataURI"].is_null());
    let pixel_uri = value["PixelData"]["BulkDataURI"].as_str().unwrap();
    assert!(pixel_uri.contains("offset="));
    assert!(pixel_uri.contains("length="));

    let roundtrip = read_dicom_json_with_source(&json, &source).unwrap();
    assert_core_fields_match(&original, &roundtrip);
    assert_eq!(
        original.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap().len(),
        roundtrip.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap().len(),
    );
}

#[test]
fn writes_and_reads_flat_json_with_bulk_data_uri_for_ct() {
    let source = fixture_bytes(CT_FIXTURE);
    let original = read_dicom_bytes(&source).unwrap();
    let json = write_dicom_json_with_source(&original, &source).unwrap();

    let roundtrip = read_dicom_json_with_source(&json, &source).unwrap();
    let bytes = write_dicom_bytes(&roundtrip).unwrap();
    let rewritten = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(roundtrip.meta().transfer_syntax(), original.meta().transfer_syntax());
    assert_eq!(
        rewritten.element(tags::PIXEL_DATA).unwrap().fragments().unwrap().len(),
        original.element(tags::PIXEL_DATA).unwrap().fragments().unwrap().len(),
    );
    assert_eq!(
        rewritten
            .element(tags::REQUEST_ATTRIBUTES_SEQUENCE)
            .unwrap()
            .items()
            .unwrap()
            .len(),
        original
            .element(tags::REQUEST_ATTRIBUTES_SEQUENCE)
            .unwrap()
            .items()
            .unwrap()
            .len(),
    );
}

#[test]
fn writes_and_reads_full_json_with_inline_binary() {
    let original = read_dicom_file(DX_FIXTURE).unwrap();
    let json = write_dicom_json_full(&original).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();

    assert_eq!(value["00080060"]["vr"], JsonValue::String("CS".to_owned()));
    assert_eq!(value["00080060"]["Keyword"], JsonValue::String("Modality".to_owned()));
    assert!(value["7FE00010"]["InlineBinary"].is_string());
    assert_eq!(value["7FE00010"]["VM"], JsonValue::Number(1.into()));

    let roundtrip = read_dicom_json_full(&json).unwrap();
    assert_core_fields_match(&original, &roundtrip);
}

#[test]
fn writes_and_reads_full_json_with_bulk_data_uri() {
    let source = fixture_bytes(CT_FIXTURE);
    let original = read_dicom_bytes(&source).unwrap();
    let json = write_dicom_json_full_with_source(&original, &source).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();

    assert!(value["00020001"]["InlineBinary"].is_string());
    assert!(value["00020001"]["BulkDataURI"].is_null());
    let pixel_uri = value["7FE00010"]["BulkDataURI"].as_str().unwrap();
    assert!(pixel_uri.contains("offset="));
    assert!(pixel_uri.contains("length="));
    assert_eq!(value["7FE00010"]["Keyword"], JsonValue::String("PixelData".to_owned()));

    let roundtrip = read_dicom_json_full_with_source(&json, &source).unwrap();
    assert_eq!(
        original.element(tags::MODALITY).unwrap().to_str().unwrap(),
        roundtrip.element(tags::MODALITY).unwrap().to_str().unwrap(),
    );
    assert_eq!(
        original.element(tags::PIXEL_DATA).unwrap().fragments().unwrap().len(),
        roundtrip.element(tags::PIXEL_DATA).unwrap().fragments().unwrap().len(),
    );
    assert_eq!(original.meta().transfer_syntax(), roundtrip.meta().transfer_syntax());
}

fn fixture_bytes(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn assert_core_fields_match(
    expected: &dicom_object::DefaultDicomObject,
    actual: &dicom_object::DefaultDicomObject,
) {
    assert_eq!(expected.meta().transfer_syntax(), actual.meta().transfer_syntax());
    assert_eq!(
        expected.element(tags::SOP_CLASS_UID).unwrap().to_str().unwrap(),
        actual.element(tags::SOP_CLASS_UID).unwrap().to_str().unwrap(),
    );
    assert_eq!(
        expected.element(tags::SOP_INSTANCE_UID).unwrap().to_str().unwrap(),
        actual.element(tags::SOP_INSTANCE_UID).unwrap().to_str().unwrap(),
    );
    assert_eq!(
        expected.element(tags::MODALITY).unwrap().to_str().unwrap(),
        actual.element(tags::MODALITY).unwrap().to_str().unwrap(),
    );
    assert_eq!(
        expected.element(tags::ROWS).unwrap().uint16().unwrap(),
        actual.element(tags::ROWS).unwrap().uint16().unwrap(),
    );
    assert_eq!(
        expected.element(tags::COLUMNS).unwrap().uint16().unwrap(),
        actual.element(tags::COLUMNS).unwrap().uint16().unwrap(),
    );
}

fn temp_file_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    std::env::temp_dir().join(format!("{prefix}-{nanos}.dcm"))
}