use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::tags;
use dicom_dictionary_std::uids;
use serde_json::Value as JsonValue;

use super::{
    detect_jpeg2000_backend_from_search_path, kakadu_ffi_enabled,
    list_transfer_syntax_support, read_dicom_bytes,
    read_dicom_file, read_dicom_json, read_dicom_json_full, read_dicom_json_full_with_source,
    read_dicom_json_with_source, redact_dicom_pixels_to_transfer_syntax, render_dicom_frame,
    transcode_dicom_object,
    write_dicom_bytes, write_dicom_file, write_dicom_json,
    write_dicom_json_full, write_dicom_json_full_with_source,
    write_dicom_json_with_options, write_dicom_json_with_source, BoundingBox, BoxLength, DicomJsonKeyStyle,
    DicomJsonWriteOptions, Jpeg2000Backend, RenderOutputFormat, RenderPipelineOptions,
};

const PRIVATE_TAG: Tag = Tag(0x0013, 0x1010);
const EXPLICIT_VR_BIG_ENDIAN_UID: &str = "1.2.840.10008.1.2.2";
const JPEG_2000_IMAGE_COMPRESSION_UID: &str = "1.2.840.10008.1.2.4.91";

#[test]
fn reads_dicom_file_fixture() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();

    assert_eq!(object.element(tags::MODALITY).unwrap().to_str().unwrap(), "DX");
    assert!(object.element(tags::PIXEL_DATA).is_ok());
}

#[test]
fn writes_dicom_file_fixture_round_trip() {
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let output_path = temp_file_path("dicom-file-roundtrip");

    write_dicom_file(&original, &output_path).unwrap();
    let roundtrip = read_dicom_file(&output_path).unwrap();

    assert_core_fields_match(&original, &roundtrip);

    fs::remove_file(output_path).unwrap();
}

#[test]
fn reads_dicom_bytes_fixture() {
    let bytes = fixture_bytes(fixture_path("dx.dcm"));
    let object = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(object.element(tags::MODALITY).unwrap().to_str().unwrap(), "DX");
    assert!(object.element(tags::PIXEL_DATA).is_ok());
}

#[test]
fn writes_dicom_bytes_fixture_round_trip() {
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let bytes = write_dicom_bytes(&original).unwrap();
    let roundtrip = read_dicom_bytes(&bytes).unwrap();

    assert_core_fields_match(&original, &roundtrip);
}

#[test]
fn writes_flat_json_with_inline_binary_by_default() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
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
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
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
    let source = fixture_bytes(fixture_path("dx.dcm"));
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
    let source = fixture_bytes(fixture_path("ct.dcm"));
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
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
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
    let source = fixture_bytes(fixture_path("ct.dcm"));
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

#[test]
fn transcodes_native_dataset_to_big_endian() {
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let transcoded = transcode_dicom_object(&original, EXPLICIT_VR_BIG_ENDIAN_UID).unwrap();
    let bytes = write_dicom_bytes(&transcoded).unwrap();
    let roundtrip = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(roundtrip.meta().transfer_syntax(), EXPLICIT_VR_BIG_ENDIAN_UID);
    assert_dataset_fields_match(&original, &roundtrip);
    assert_eq!(
        original.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap().len(),
        roundtrip.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap().len(),
    );
}

#[test]
fn transcodes_native_dataset_to_encapsulated_uncompressed_and_back() {
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let encapsulated = transcode_dicom_object(&original, uids::ENCAPSULATED_UNCOMPRESSED_EXPLICIT_VR_LITTLE_ENDIAN)
        .unwrap();
    let rehydrated = transcode_dicom_object(&encapsulated, uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .unwrap();

    assert_eq!(
        encapsulated.meta().transfer_syntax(),
        uids::ENCAPSULATED_UNCOMPRESSED_EXPLICIT_VR_LITTLE_ENDIAN,
    );
    assert!(encapsulated.element(tags::PIXEL_DATA).unwrap().fragments().is_some());
    assert_core_fields_match(&original, &rehydrated);
    assert_eq!(
        original.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap(),
        rehydrated.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap(),
    );
}

#[test]
fn reports_jpeg_2000_transfer_syntax_capabilities() {
    let support = list_transfer_syntax_support();
    let jpeg_2000 = support
        .iter()
        .find(|entry| entry.uid == JPEG_2000_IMAGE_COMPRESSION_UID)
        .unwrap();

    assert!(jpeg_2000.can_decode_pixel_data);
    assert!(!jpeg_2000.can_encode_pixel_data);
    assert!(!jpeg_2000.can_transcode_to());

    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let error = transcode_dicom_object(&original, JPEG_2000_IMAGE_COMPRESSION_UID)
        .unwrap_err()
        .to_string();
    assert!(error.contains(JPEG_2000_IMAGE_COMPRESSION_UID));
    assert!(error.contains("unsupported target transfer syntax"));
}

#[test]
fn redacts_monochrome_pixels_in_dicom_to_dicom_path() {
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let original_native = transcode_dicom_object(&original, uids::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
    let redacted = redact_dicom_pixels_to_transfer_syntax(
        &original,
        uids::EXPLICIT_VR_LITTLE_ENDIAN,
        &[BoundingBox {
            x: 0,
            y: 0,
            width: BoxLength::Pixels(8),
            height: BoxLength::Pixels(8),
        }],
        [255, 0, 0],
    )
    .unwrap();

    assert_eq!(redacted.meta().transfer_syntax(), uids::EXPLICIT_VR_LITTLE_ENDIAN);

    let baseline_inside = mono_sample_value(&original_native, 0, 0).unwrap();
    let baseline_outside = mono_sample_value(&original_native, 16, 16).unwrap();
    let redacted_inside = mono_sample_value(&redacted, 0, 0).unwrap();
    let redacted_outside = mono_sample_value(&redacted, 16, 16).unwrap();
    let bits_stored = redacted.element(tags::BITS_STORED).unwrap().uint16().unwrap();

    assert_ne!(baseline_inside, redacted_inside);
    assert_eq!(baseline_outside, redacted_outside);
    assert_eq!(redacted_inside, scaled_u8_to_bits_stored(54, bits_stored));
}

#[test]
fn redacts_rgb_pixels_when_planar_configuration_is_one() {
    let source = read_dicom_file(fixture_path("sc.dcm")).unwrap();
    let mut object = transcode_dicom_object(&source, uids::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();

    let samples = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1);
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(0);
    if samples != 3 || bits_allocated != 8 {
        return;
    }

    let rows = object.element(tags::ROWS).unwrap().uint16().unwrap() as usize;
    let cols = object.element(tags::COLUMNS).unwrap().uint16().unwrap() as usize;
    let pixel_count = rows * cols;

    let interleaved = object.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap().into_owned();
    if interleaved.len() < pixel_count * 3 {
        return;
    }

    let mut planar = vec![0u8; pixel_count * 3];
    for index in 0..pixel_count {
        planar[index] = interleaved[index * 3];
        planar[index + pixel_count] = interleaved[index * 3 + 1];
        planar[index + 2 * pixel_count] = interleaved[index * 3 + 2];
    }

    object.put(DataElement::new(
        tags::PLANAR_CONFIGURATION,
        VR::US,
        PrimitiveValue::from(1u16),
    ));
    object.put(DataElement::new(tags::PIXEL_DATA, VR::OB, PrimitiveValue::from(planar)));

    let redacted = redact_dicom_pixels_to_transfer_syntax(
        &object,
        uids::EXPLICIT_VR_LITTLE_ENDIAN,
        &[BoundingBox {
            x: 0,
            y: 0,
            width: BoxLength::Pixels(1),
            height: BoxLength::Pixels(1),
        }],
        [12, 34, 56],
    )
    .unwrap();

    let redacted_planar = redacted
        .element(tags::PIXEL_DATA)
        .unwrap()
        .to_bytes()
        .unwrap()
        .into_owned();
    assert_eq!(redacted.element(tags::PLANAR_CONFIGURATION).unwrap().uint16().unwrap(), 1);
    assert_eq!(redacted_planar[0], 12);
    assert_eq!(redacted_planar[pixel_count], 34);
    assert_eq!(redacted_planar[2 * pixel_count], 56);
}

#[test]
fn detects_kakadu_backend_from_search_path() {
    let base = std::env::temp_dir().join(format!(
        "dcmnorm-kakadu-detect-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&base).unwrap();
    let kakadu_lib = base.join("libkdu_v84R.so");
    fs::write(&kakadu_lib, []).unwrap();

    let backend = detect_jpeg2000_backend_from_search_path(base.to_string_lossy().as_ref());
    if kakadu_ffi_enabled() {
        assert!(matches!(backend, Jpeg2000Backend::Kakadu { .. }));
    } else {
        assert_eq!(backend, Jpeg2000Backend::OpenJpeg);
    }

    fs::remove_file(kakadu_lib).unwrap();
    fs::remove_dir(base).unwrap();
}

#[test]
fn falls_back_to_openjpeg_when_kakadu_not_in_search_path() {
    let base = std::env::temp_dir().join(format!(
        "dcmnorm-openjpeg-fallback-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&base).unwrap();

    let backend = detect_jpeg2000_backend_from_search_path(base.to_string_lossy().as_ref());
    assert_eq!(backend, Jpeg2000Backend::OpenJpeg);

    fs::remove_dir(base).unwrap();
}

#[test]
fn renders_dx_frame_to_png() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    assert_eq!(&rendered.bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(rendered.width, object.element(tags::COLUMNS).unwrap().uint16().unwrap());
    assert_eq!(rendered.height, object.element(tags::ROWS).unwrap().uint16().unwrap());
}

#[test]
fn renders_dx_frame_to_jpeg() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Jpeg,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    assert_eq!(&rendered.bytes[..2], b"\xFF\xD8");
    assert!(rendered.bytes.len() > 100);
}

#[test]
fn renders_dx_frame_to_raw_u8() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Raw,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    let rows = object.element(tags::ROWS).unwrap().uint16().unwrap() as usize;
    let cols = object.element(tags::COLUMNS).unwrap().uint16().unwrap() as usize;
    let samples = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1) as usize;
    let bits_allocated = object
        .get(tags::BITS_ALLOCATED)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(8) as usize;
    let samples_per_frame = rows * cols * samples;
    let expected_len = match bits_allocated {
        1 => samples_per_frame.div_ceil(8),
        8 => samples_per_frame,
        16 => samples_per_frame * 2,
        other => panic!("unexpected BitsAllocated value in fixture: {other}"),
    };

    assert_eq!(rendered.bytes.len(), expected_len);
    assert_eq!(rendered.bits_allocated as usize, bits_allocated);
}

#[test]
fn falls_back_when_window_is_outside_pixel_domain() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let default_rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Raw,
        &RenderPipelineOptions::default(),
    )
    .unwrap();
    let no_voi_rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Raw,
        &RenderPipelineOptions {
            apply_voi_lut: false,
            ..RenderPipelineOptions::default()
        },
    )
    .unwrap();

    assert_eq!(default_rendered.bytes, no_voi_rendered.bytes);
}

#[test]
fn ignores_invalid_window_width_from_dataset() {
    let mut object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    object.put(DataElement::new(
        tags::WINDOW_CENTER,
        VR::DS,
        PrimitiveValue::from("40"),
    ));
    object.put(DataElement::new(
        tags::WINDOW_WIDTH,
        VR::DS,
        PrimitiveValue::from("0"),
    ));
    let default_rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Raw,
        &RenderPipelineOptions::default(),
    )
    .unwrap();
    let no_voi_rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Raw,
        &RenderPipelineOptions {
            apply_voi_lut: false,
            ..RenderPipelineOptions::default()
        },
    )
    .unwrap();

    assert_eq!(default_rendered.bytes, no_voi_rendered.bytes);
}

#[test]
fn rejects_invalid_user_provided_window_width() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let error = render_dicom_frame(
        &object,
        RenderOutputFormat::Raw,
        &RenderPipelineOptions {
            window_center: Some(40.0),
            window_width: Some(0.0),
            ..RenderPipelineOptions::default()
        },
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("window width must be greater than zero"));
}

#[test]
fn renders_rgb_fixture_when_present() {
    let object = read_dicom_file(fixture_path("sc.dcm")).unwrap();
    let samples = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1);

    if samples != 3 {
        return;
    }

    let rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    assert_eq!(rendered.samples_per_pixel, 3);
    assert_eq!(&rendered.bytes[..8], b"\x89PNG\r\n\x1a\n");
}

fn fixture_bytes(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files").join(name)
}

fn assert_core_fields_match(
    expected: &dicom_object::DefaultDicomObject,
    actual: &dicom_object::DefaultDicomObject,
) {
    assert_eq!(expected.meta().transfer_syntax(), actual.meta().transfer_syntax());
    assert_dataset_fields_match(expected, actual);
}

fn assert_dataset_fields_match(
    expected: &dicom_object::DefaultDicomObject,
    actual: &dicom_object::DefaultDicomObject,
) {
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

fn mono_sample_value(object: &dicom_object::DefaultDicomObject, x: usize, y: usize) -> Option<u16> {
    let cols = object.element(tags::COLUMNS).ok()?.uint16().ok()? as usize;
    let rows = object.element(tags::ROWS).ok()?.uint16().ok()? as usize;
    if x >= cols || y >= rows {
        return None;
    }

    let bits_allocated = object.element(tags::BITS_ALLOCATED).ok()?.uint16().ok()?;
    let bits_stored = object.element(tags::BITS_STORED).ok()?.uint16().ok()?;
    let bytes = object.element(tags::PIXEL_DATA).ok()?.to_bytes().ok()?.into_owned();
    let index = y * cols + x;

    match bits_allocated {
        1 => {
            let byte = *bytes.get(index / 8)?;
            let bit = 7 - (index % 8);
            Some(u16::from((byte >> bit) & 1))
        }
        8 => {
            let raw = u16::from(*bytes.get(index)?);
            let mask = if bits_stored >= 8 {
                0xFF
            } else {
                (1u16 << bits_stored).saturating_sub(1)
            };
            Some(raw & mask.max(1))
        }
        16 => {
            let base = index * 2;
            let low = *bytes.get(base)?;
            let high = *bytes.get(base + 1)?;
            let raw = u16::from_le_bytes([low, high]);
            let mask = if bits_stored >= 16 {
                0xFFFF
            } else {
                (1u16 << bits_stored).saturating_sub(1)
            };
            Some(raw & mask.max(1))
        }
        _ => None,
    }
}

fn scaled_u8_to_bits_stored(value: u8, bits_stored: u16) -> u16 {
    let bits = bits_stored.clamp(1, 16);
    let max_value = (1u32 << u32::from(bits)) - 1;
    ((u32::from(value) * max_value + 127) / 255) as u16
}
