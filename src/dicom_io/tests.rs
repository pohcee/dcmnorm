#[test]
fn writes_flat_json_with_source_uri_mode_for_pixeldata_bulkdatauri() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let object = read_dicom_bytes(&source).unwrap();

    let json = write_dicom_json_with_source(&object, &source).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let pixel = &value["PixelData"];
    let uri = pixel.get("BulkDataURI").and_then(|v| v.as_str()).unwrap();
    assert!(
        uri.contains("offset=") && uri.contains("length="),
        "PixelData BulkDataURI must include offset and length, got: {}",
        uri
    );
    assert!(
        !uri.contains("tag="),
        "PixelData BulkDataURI must not fallback to tag-based URI, got: {}",
        uri
    );
}
#[test]
fn writes_flat_json_with_source_uri_mode_for_large_meta_without_part10_header() {
    let source = fixture_bytes(repo_root_path("nometa.dcm"));
    let object = read_dicom_bytes(&source).unwrap();

    // Synthesize a large meta field (simulate >32B value)
    use dicom_core::value::PrimitiveValue;
    use dicom_core::DataElement;
    use dicom_dictionary_std::tags;
    let big_bytes = vec![0xAB; 64];
    let mut object = object;
    object.put(DataElement::new(
        tags::IMPLEMENTATION_VERSION_NAME,
        dicom_core::VR::SH,
        PrimitiveValue::from(big_bytes.clone()),
    ));

    let json = write_dicom_json_with_source(&object, &source).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let meta = &value["ImplementationVersionName"];
    assert!(
        meta.get("BulkDataURI").is_none(),
        "non-bulk meta should not emit offsetless BulkDataURI"
    );
}

#[test]
fn writes_flat_json_with_source_uri_mode_for_nometa_pixeldata_uses_offset_length() {
    let source = fixture_bytes(repo_root_path("nometa.dcm"));
    let object = read_dicom_bytes(&source).unwrap();

    let json = write_dicom_json_with_source(&object, &source).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let uri = value["PixelData"]["BulkDataURI"].as_str().unwrap();

    assert!(uri.contains("offset="), "missing offset in URI: {uri}");
    assert!(uri.contains("length="), "missing length in URI: {uri}");
    assert!(!uri.contains("tag="), "unexpected tag-based URI: {uri}");
}

#[test]
fn writes_flat_json_with_source_uri_mode_for_nested_bulk_value() {
    let mut source = fixture_bytes(fixture_path("dx.dcm"));
    let nested_payload = vec![0x5A; 64];
    append_nested_icc_profile_sequence(&mut source, &nested_payload);

    let object = read_dicom_bytes(&source).unwrap();
    let json = write_dicom_json_with_source(&object, &source).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    let icc = &value["OpticalPathSequence"][0]["ICCProfile"];
    let uri = icc["BulkDataURI"]
        .as_str()
        .expect("nested ICCProfile should emit BulkDataURI");
    assert!(uri.contains("offset="), "missing offset in URI: {uri}");
    assert!(uri.contains("length="), "missing length in URI: {uri}");
    assert!(
        icc["InlineBinary"].is_null(),
        "nested ICCProfile should not fallback to InlineBinary"
    );
}

fn append_nested_icc_profile_sequence(bytes: &mut Vec<u8>, payload: &[u8]) {
    let sequence_tag = Tag(0x0048, 0x0105); // OpticalPathSequence
    let icc_profile_tag = Tag(0x0028, 0x2000); // ICCProfile

    append_explicit_vr_header(bytes, sequence_tag, *b"SQ", u32::MAX);
    append_item_header(bytes, u32::MAX);
    append_explicit_vr_header(bytes, icc_profile_tag, *b"OB", payload.len() as u32);
    bytes.extend_from_slice(payload);
    append_item_delimitation(bytes);
    append_sequence_delimitation(bytes);
}

fn append_explicit_vr_header(bytes: &mut Vec<u8>, tag: Tag, vr: [u8; 2], len: u32) {
    bytes.extend_from_slice(&tag.group().to_le_bytes());
    bytes.extend_from_slice(&tag.element().to_le_bytes());
    bytes.extend_from_slice(&vr);

    if uses_32_bit_vr_length(vr) {
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&len.to_le_bytes());
    } else {
        bytes.extend_from_slice(&(len as u16).to_le_bytes());
    }
}

fn append_item_header(bytes: &mut Vec<u8>, len: u32) {
    bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
    bytes.extend_from_slice(&0xE000u16.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
}

fn append_item_delimitation(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
    bytes.extend_from_slice(&0xE00Du16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
}

fn append_sequence_delimitation(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
    bytes.extend_from_slice(&0xE0DDu16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
}

fn uses_32_bit_vr_length(vr: [u8; 2]) -> bool {
    matches!(
        &vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UR" | b"UT" | b"UN"
    )
}

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dicom_core::{dicom_value, DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::tags;
use dicom_dictionary_std::uids;
use dicom_object::mem::InMemDicomObject;
use serde_json::Value as JsonValue;

use super::{
    detect_jpeg2000_backend_from_search_path, echo_scu, find_scu, kakadu_ffi_enabled,
    list_transfer_syntax_support, move_scu, probe_dicom_file_for_sop_class_uid, read_dicom_bytes,
    read_dicom_file, read_dicom_json, read_dicom_json_full, read_dicom_json_full_with_source,
    read_dicom_json_with_options, read_dicom_json_with_source,
    redact_dicom_pixels_to_transfer_syntax, render_all_dicom_video_frames, render_dicom_frame,
    start_scp, store_scu, transcode_dicom_object, write_dicom_bytes, write_dicom_file,
    write_dicom_json, write_dicom_json_full, write_dicom_json_full_with_source,
    write_dicom_json_with_options, write_dicom_json_with_source, BoundingBox, BoxLength,
    DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    DicomJsonWriteOptions, EchoScuOptions, FindScuOptions, Jpeg2000Backend, MoveScuOptions,
    RenderOutputFormat, RenderPipelineOptions, ScpHandlers, ScpOptions, StoreScuOptions,
};

const PRIVATE_TAG: Tag = Tag(0x0013, 0x1010);
const EXPLICIT_VR_BIG_ENDIAN_UID: &str = "1.2.840.10008.1.2.2";
const JPEG_2000_IMAGE_COMPRESSION_UID: &str = "1.2.840.10008.1.2.4.91";

#[test]
fn transcodes_rle_ybr_full_preserving_photometric_interpretation() {
    let Some(path) = optional_repo_fixture_path("test.dcm") else {
        return;
    };
    let source = read_dicom_file(path).unwrap();
    let transcoded = transcode_dicom_object(&source, uids::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();

    let photometric = transcoded
        .element(tags::PHOTOMETRIC_INTERPRETATION)
        .unwrap()
        .to_str()
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(photometric, "YBR_FULL");

    let planar = transcoded
        .element(tags::PLANAR_CONFIGURATION)
        .unwrap()
        .uint16()
        .unwrap();
    assert_eq!(planar, 0);
}

#[test]
fn renders_rle_ybr_full_without_pink_cast() {
    let Some(path) = optional_repo_fixture_path("test.dcm") else {
        return;
    };
    let source = read_dicom_file(path).unwrap();
    let rendered = render_dicom_frame(
        &source,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    let image = image::load_from_memory(&rendered.bytes).unwrap().to_rgb8();
    let first = image.get_pixel(0, 0).0;

    // Top-left pixel is background and should render near black, not cyan/magenta.
    assert!(first[0] <= 5, "expected low red channel, got {}", first[0]);
    assert!(
        first[1] <= 5,
        "expected low green channel, got {}",
        first[1]
    );
    assert!(first[2] <= 5, "expected low blue channel, got {}", first[2]);
}

#[test]
fn reads_dicom_file_fixture() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();

    assert_eq!(
        object.element(tags::MODALITY).unwrap().to_str().unwrap(),
        "DX"
    );
    assert!(object.element(tags::PIXEL_DATA).is_ok());
}

#[test]
fn reads_dicom_file_without_part10_header() {
    let object = read_dicom_file(repo_root_path("nometa.dcm")).unwrap();

    assert_eq!(
        object.element(tags::MODALITY).unwrap().to_str().unwrap(),
        "DX"
    );
}

#[test]
fn probes_dicom_file_with_part10_header() {
    let is_valid = probe_dicom_file_for_sop_class_uid(fixture_path("dx.dcm")).unwrap();
    assert!(is_valid);
}

#[test]
fn probes_dicom_file_without_part10_header() {
    let is_valid = probe_dicom_file_for_sop_class_uid(repo_root_path("nometa.dcm")).unwrap();
    assert!(is_valid);
}

#[test]
fn probe_rejects_non_dicom_file() {
    let is_valid = probe_dicom_file_for_sop_class_uid(fixture_path("notdicom.txt")).unwrap();
    assert!(!is_valid);
}

#[test]
fn probe_rejects_directory_path() {
    let is_valid = probe_dicom_file_for_sop_class_uid(fixture_path(""))
        .expect("directory metadata should be readable");
    assert!(!is_valid);
}

#[test]
fn remove_private_tags_removes_all_private_tags() {
    let mut object = read_dicom_bytes(include_bytes!("../../test/files/dx.dcm")).unwrap();

    let private_tag = Tag(0x0013, 0x1010);
    object.put(DataElement::new(
        private_tag,
        VR::LO,
        PrimitiveValue::from("PRIVATE"),
    ));

    let standard_tag = tags::PATIENT_NAME;
    object.put(DataElement::new(
        standard_tag,
        VR::PN,
        PrimitiveValue::from("John^Doe"),
    ));

    super::remove_private_tags_inplace(&mut object);

    assert!(
        object.element(private_tag).is_err(),
        "private tag was not removed"
    );
    assert_eq!(
        object.element(standard_tag).unwrap().to_str().unwrap(),
        "John^Doe"
    );
}

#[test]
fn writes_dicom_file_fixture_round_trip() {
    let mut original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let output_path = temp_file_path("dicom-file-roundtrip");

    write_dicom_file(&mut original, &output_path).unwrap();
    let roundtrip = read_dicom_file(&output_path).unwrap();

    assert_core_fields_match(&original, &roundtrip);

    fs::remove_file(output_path).unwrap();
}

#[test]
fn reads_dicom_bytes_fixture() {
    let bytes = fixture_bytes(fixture_path("dx.dcm"));
    let object = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(
        object.element(tags::MODALITY).unwrap().to_str().unwrap(),
        "DX"
    );
    assert!(object.element(tags::PIXEL_DATA).is_ok());
}

#[test]
fn writes_dicom_bytes_fixture_round_trip() {
    let mut original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let bytes = write_dicom_bytes(&mut original).unwrap();
    let roundtrip = read_dicom_bytes(&bytes).unwrap();

    assert_core_fields_match(&original, &roundtrip);
}

#[test]
fn writes_dicom_file_with_explicit_length_nested_sequences_stays_readable() {
    // sr.dcm encodes its ContentSequence and nested CONTAINER/CONTAINS items
    // with explicit (defined) lengths rather than undefined-length
    // delimiters, and its file meta group omits MediaStorageSOPClassUID and
    // MediaStorageSOPInstanceUID (they get inferred from the data set on
    // read). Editing and rewriting such a file must still produce a fully
    // parseable data set, not just one that passes a shallow SOP Class UID
    // probe.
    let mut object = read_dicom_file(fixture_path("sr.dcm")).unwrap();
    object.put_str(tags::PATIENT_NAME, dicom_core::VR::PN, "TEST");

    let bytes = write_dicom_bytes(&mut object).unwrap();
    let roundtrip = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(
        roundtrip
            .element(tags::PATIENT_NAME)
            .unwrap()
            .to_str()
            .unwrap(),
        "TEST"
    );
    assert_eq!(
        roundtrip
            .element(tags::CONTENT_SEQUENCE)
            .unwrap()
            .items()
            .unwrap()
            .len(),
        1
    );
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
    assert_eq!(
        json["00020010"],
        JsonValue::String("1.2.840.10008.1.2.1".to_owned())
    );

    let roundtrip = read_dicom_json(&json_text).unwrap();
    assert_core_fields_match(&object, &roundtrip);
    assert_eq!(
        roundtrip.meta().transfer_syntax(),
        object.meta().transfer_syntax()
    );
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
        original
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .len(),
        roundtrip
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .len(),
    );
}

#[test]
fn reads_flat_json_with_file_bulk_data_uri_without_source() {
    let source_path = fixture_path("dx.dcm");
    let source = fixture_bytes(source_path.clone());
    let original = read_dicom_bytes(&source).unwrap();

    let canonical = source_path.canonicalize().unwrap();
    let uri_base = format!(
        "file://{}",
        canonical
            .to_string_lossy()
            .replace('%', "%25")
            .replace(' ', "%20")
    );

    let json = write_dicom_json_with_options(
        &original,
        DicomJsonWriteOptions {
            bulk_data_mode: DicomJsonBulkDataMode::Uri,
            bulk_data_source: Some(&source),
            bulk_data_uri_base: Some(uri_base.as_str()),
            ..DicomJsonWriteOptions::default()
        },
    )
    .unwrap();

    let roundtrip = read_dicom_json(&json).unwrap();
    assert_core_fields_match(&original, &roundtrip);
    assert_eq!(
        original
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .len(),
        roundtrip
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .len(),
    );
}

fn icc_bytes(object: &dicom_object::DefaultDicomObject) -> Vec<u8> {
    let sequence_tag = Tag(0x0048, 0x0105); // OpticalPathSequence
    let icc_profile_tag = Tag(0x0028, 0x2000); // ICCProfile

    let dicom_core::value::Value::Sequence(sequence) =
        object.element(sequence_tag).unwrap().value()
    else {
        panic!("OpticalPathSequence did not decode as a sequence");
    };

    sequence.items()[0]
        .element(icc_profile_tag)
        .unwrap()
        .to_bytes()
        .unwrap()
        .into_owned()
}

#[test]
fn reads_json_with_bulk_data_source_and_separate_file_bulk_data_uri_together() {
    // Regression test for resolve_bulk_data_uri_with_optional_source: it must
    // check a "file://" BulkDataURI before falling back to bulk_data_source,
    // not the other way around, so a document can reference bulk data from
    // more than one place in the same read - some elements via "?offset=..
    // &length=.." into bulk_data_source, others via their own separate
    // "file://" path. This is exactly index-file's "iw" transform's shape:
    // pre-existing elements resolved against the original .dcm, freshly
    // attached PixelData resolved from its own temp file. Getting the
    // priority backwards doesn't just miss the second file - it feeds the
    // "file://...?offset=..&length=.." URI's offset/length to the WRONG
    // buffer (bulk_data_source instead of the file it names).
    let mut source = fixture_bytes(fixture_path("dx.dcm"));
    let nested_payload = vec![0x5A; 64];
    append_nested_icc_profile_sequence(&mut source, &nested_payload);
    let original = read_dicom_bytes(&source).unwrap();

    // Both PixelData and ICCProfile come out as bare "?offset=..&length=.."
    // references into `source` (no bulk_data_uri_base given).
    let json = write_dicom_json_with_options(
        &original,
        DicomJsonWriteOptions {
            bulk_data_mode: DicomJsonBulkDataMode::Uri,
            bulk_data_source: Some(&source),
            ..DicomJsonWriteOptions::default()
        },
    )
    .unwrap();

    // Move ICCProfile's payload into its own separate file instead, referenced
    // by a "file://" BulkDataURI wholly independent of `source`.
    let icc_path = temp_file_path("dcmnorm-test-icc-separate-source");
    fs::write(&icc_path, &nested_payload).unwrap();
    let mut value: JsonValue = serde_json::from_str(&json).unwrap();
    value["OpticalPathSequence"][0]["ICCProfile"] = serde_json::json!({
        "BulkDataURI": format!(
            "file://{}?offset=0&length={}",
            icc_path.canonicalize().unwrap().to_string_lossy(),
            nested_payload.len(),
        )
    });
    let modified_json = serde_json::to_string(&value).unwrap();

    let roundtrip = read_dicom_json_with_options(
        &modified_json,
        DicomJsonReadOptions {
            format: DicomJsonFormat::Flat,
            bulk_data_source: Some(&source),
        },
    );
    fs::remove_file(&icc_path).ok();
    let roundtrip = roundtrip.unwrap();

    // PixelData still resolves against `source` ...
    assert_eq!(
        original
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .into_owned(),
        roundtrip
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .into_owned(),
    );
    // ... while ICCProfile resolves from its own separate file, not `source`.
    assert_eq!(icc_bytes(&roundtrip), nested_payload);
}

#[test]
fn reads_json_with_two_independent_file_bulk_data_uris() {
    // Two bulk elements, each referenced by its own "file://" BulkDataURI, with
    // no bulk_data_source at all - each must resolve from its own file without
    // being confused with the other's.
    let mut source = fixture_bytes(fixture_path("dx.dcm"));
    let nested_payload = vec![0x5A; 64];
    append_nested_icc_profile_sequence(&mut source, &nested_payload);
    let original = read_dicom_bytes(&source).unwrap();

    let pixel_data = original
        .element(tags::PIXEL_DATA)
        .unwrap()
        .to_bytes()
        .unwrap()
        .into_owned();

    let pixel_path = temp_file_path("dcmnorm-test-pixeldata-separate-source");
    let icc_path = temp_file_path("dcmnorm-test-icc-separate-source-2");
    fs::write(&pixel_path, &pixel_data).unwrap();
    fs::write(&icc_path, &nested_payload).unwrap();

    let json = write_dicom_json_with_options(
        &original,
        DicomJsonWriteOptions {
            bulk_data_mode: DicomJsonBulkDataMode::InlineBinary,
            ..DicomJsonWriteOptions::default()
        },
    )
    .unwrap();
    let mut value: JsonValue = serde_json::from_str(&json).unwrap();
    value["PixelData"] = serde_json::json!({
        "BulkDataURI": format!(
            "file://{}?offset=0&length={}",
            pixel_path.canonicalize().unwrap().to_string_lossy(),
            pixel_data.len(),
        )
    });
    value["OpticalPathSequence"][0]["ICCProfile"] = serde_json::json!({
        "BulkDataURI": format!(
            "file://{}?offset=0&length={}",
            icc_path.canonicalize().unwrap().to_string_lossy(),
            nested_payload.len(),
        )
    });
    let modified_json = serde_json::to_string(&value).unwrap();

    let roundtrip = read_dicom_json_with_options(
        &modified_json,
        DicomJsonReadOptions {
            format: DicomJsonFormat::Flat,
            bulk_data_source: None,
        },
    );
    fs::remove_file(&pixel_path).ok();
    fs::remove_file(&icc_path).ok();
    let roundtrip = roundtrip.unwrap();

    assert_eq!(
        roundtrip
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .into_owned(),
        pixel_data,
    );
    assert_eq!(icc_bytes(&roundtrip), nested_payload);
}

#[test]
fn writes_flat_json_with_source_for_file_without_part10_header() {
    let source = fixture_bytes(repo_root_path("nometa.dcm"));
    let object = read_dicom_bytes(&source).unwrap();

    let json = write_dicom_json_with_source(&object, &source).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();

    assert!(value["FileMetaInformationVersion"]["InlineBinary"].is_string());
    assert_eq!(
        value["Modality"],
        JsonValue::String(
            object
                .element(tags::MODALITY)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        )
    );
}

#[test]
fn writes_and_reads_flat_json_with_bulk_data_uri_for_ct() {
    let source = fixture_bytes(fixture_path("ct.dcm"));
    let original = read_dicom_bytes(&source).unwrap();
    let json = write_dicom_json_with_source(&original, &source).unwrap();

    let mut roundtrip = read_dicom_json_with_source(&json, &source).unwrap();
    let bytes = write_dicom_bytes(&mut roundtrip).unwrap();
    let rewritten = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(
        roundtrip.meta().transfer_syntax(),
        original.meta().transfer_syntax()
    );
    assert_eq!(
        rewritten
            .element(tags::PIXEL_DATA)
            .unwrap()
            .fragments()
            .unwrap()
            .len(),
        original
            .element(tags::PIXEL_DATA)
            .unwrap()
            .fragments()
            .unwrap()
            .len(),
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
    assert_eq!(
        value["00080060"]["Keyword"],
        JsonValue::String("Modality".to_owned())
    );
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
    assert_eq!(
        value["7FE00010"]["Keyword"],
        JsonValue::String("PixelData".to_owned())
    );

    let roundtrip = read_dicom_json_full_with_source(&json, &source).unwrap();
    assert_eq!(
        original.element(tags::MODALITY).unwrap().to_str().unwrap(),
        roundtrip.element(tags::MODALITY).unwrap().to_str().unwrap(),
    );
    assert_eq!(
        original
            .element(tags::PIXEL_DATA)
            .unwrap()
            .fragments()
            .unwrap()
            .len(),
        roundtrip
            .element(tags::PIXEL_DATA)
            .unwrap()
            .fragments()
            .unwrap()
            .len(),
    );
    assert_eq!(
        original.meta().transfer_syntax(),
        roundtrip.meta().transfer_syntax()
    );
}

#[test]
fn writes_full_json_with_source_uri_mode_for_nested_bulk_value() {
    let mut source = fixture_bytes(fixture_path("dx.dcm"));
    let nested_payload = vec![0x5A; 64];
    append_nested_icc_profile_sequence(&mut source, &nested_payload);

    let object = read_dicom_bytes(&source).unwrap();
    let json = write_dicom_json_full_with_source(&object, &source).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();

    let icc = &value["00480105"]["Value"][0]["00282000"];
    let uri = icc["BulkDataURI"]
        .as_str()
        .expect("nested ICCProfile should emit BulkDataURI in standard JSON too");
    assert!(uri.contains("offset="), "missing offset in URI: {uri}");
    assert!(uri.contains("length="), "missing length in URI: {uri}");
    assert!(
        icc["InlineBinary"].is_null(),
        "nested ICCProfile should not fallback to InlineBinary"
    );

    let roundtrip = read_dicom_json_full_with_source(&json, &source).unwrap();
    let optical_path = roundtrip
        .element(Tag(0x0048, 0x0105))
        .unwrap()
        .items()
        .unwrap();
    let icc_profile = optical_path[0].element(Tag(0x0028, 0x2000)).unwrap();
    assert_eq!(icc_profile.to_bytes().unwrap().into_owned(), nested_payload);
}

#[test]
fn writes_and_reads_full_json_pn_with_ideographic_and_phonetic() {
    let mut object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    object.put(DataElement::new(
        tags::PATIENT_NAME,
        VR::PN,
        PrimitiveValue::from("Yamada^Tarou=山田^太郎=やまだ^たろう"),
    ));

    let json = write_dicom_json_full(&object).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();
    let name = &value["00100010"]["Value"][0];
    assert_eq!(name["Alphabetic"], JsonValue::String("Yamada^Tarou".to_owned()));
    assert_eq!(
        name["Ideographic"],
        JsonValue::String("山田^太郎".to_owned())
    );
    assert_eq!(
        name["Phonetic"],
        JsonValue::String("やまだ^たろう".to_owned())
    );

    let roundtrip = read_dicom_json_full(&json).unwrap();
    assert_eq!(
        roundtrip
            .element(tags::PATIENT_NAME)
            .unwrap()
            .to_str()
            .unwrap(),
        "Yamada^Tarou=山田^太郎=やまだ^たろう"
    );
}

#[test]
fn writes_and_reads_full_json_numeric_nan_and_infinity() {
    let mut object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    object.put(DataElement::new(
        Tag(0x0018, 0x9432), // ReconstructionAngle, VR FD
        VR::FD,
        PrimitiveValue::from([f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    ));

    let json = write_dicom_json_full(&object).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();
    let values = value["00189432"]["Value"].as_array().unwrap();
    assert_eq!(values[0], JsonValue::String("NaN".to_owned()));
    assert_eq!(values[1], JsonValue::String("inf".to_owned()));
    assert_eq!(values[2], JsonValue::String("-inf".to_owned()));

    let roundtrip = read_dicom_json_full(&json).unwrap();
    let restored = roundtrip
        .element(Tag(0x0018, 0x9432))
        .unwrap()
        .to_multi_float64()
        .unwrap();
    assert!(restored[0].is_nan());
    assert_eq!(restored[1], f64::INFINITY);
    assert_eq!(restored[2], f64::NEG_INFINITY);
}

#[test]
fn writes_and_reads_full_json_large_integer_falls_back_to_string() {
    let mut object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let large_value = 876_543_245_678u64;
    object.put(DataElement::new(
        Tag(0x0028, 0x9422), // PixelOffsetTableUV placeholder tag, VR UV
        VR::UV,
        PrimitiveValue::U64(vec![large_value].into()),
    ));

    let json = write_dicom_json_full(&object).unwrap();
    let value: JsonValue = serde_json::from_str(&json).unwrap();
    assert_eq!(
        value["00289422"]["Value"][0],
        JsonValue::String(large_value.to_string())
    );

    let roundtrip = read_dicom_json_full(&json).unwrap();
    assert_eq!(
        roundtrip
            .element(Tag(0x0028, 0x9422))
            .unwrap()
            .to_multi_int::<u64>()
            .unwrap()[0],
        large_value
    );
}

#[test]
fn transcodes_native_dataset_to_big_endian() {
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let mut transcoded = transcode_dicom_object(&original, EXPLICIT_VR_BIG_ENDIAN_UID).unwrap();
    let bytes = write_dicom_bytes(&mut transcoded).unwrap();
    let roundtrip = read_dicom_bytes(&bytes).unwrap();

    assert_eq!(
        roundtrip.meta().transfer_syntax(),
        EXPLICIT_VR_BIG_ENDIAN_UID
    );
    assert_dataset_fields_match(&original, &roundtrip);
    assert_eq!(
        original
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .len(),
        roundtrip
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap()
            .len(),
    );
}

#[test]
fn transcodes_native_dataset_to_encapsulated_uncompressed_and_back() {
    let original = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let encapsulated = transcode_dicom_object(
        &original,
        uids::ENCAPSULATED_UNCOMPRESSED_EXPLICIT_VR_LITTLE_ENDIAN,
    )
    .unwrap();
    let rehydrated =
        transcode_dicom_object(&encapsulated, uids::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();

    assert_eq!(
        encapsulated.meta().transfer_syntax(),
        uids::ENCAPSULATED_UNCOMPRESSED_EXPLICIT_VR_LITTLE_ENDIAN,
    );
    assert!(encapsulated
        .element(tags::PIXEL_DATA)
        .unwrap()
        .fragments()
        .is_some());
    assert_core_fields_match(&original, &rehydrated);
    assert_eq!(
        original
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap(),
        rehydrated
            .element(tags::PIXEL_DATA)
            .unwrap()
            .to_bytes()
            .unwrap(),
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
    let original_native =
        transcode_dicom_object(&original, uids::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
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

    assert_eq!(
        redacted.meta().transfer_syntax(),
        uids::EXPLICIT_VR_LITTLE_ENDIAN
    );

    let baseline_inside = mono_sample_value(&original_native, 0, 0).unwrap();
    let baseline_outside = mono_sample_value(&original_native, 16, 16).unwrap();
    let redacted_inside = mono_sample_value(&redacted, 0, 0).unwrap();
    let redacted_outside = mono_sample_value(&redacted, 16, 16).unwrap();
    let bits_stored = redacted
        .element(tags::BITS_STORED)
        .unwrap()
        .uint16()
        .unwrap();

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

    let interleaved = object
        .element(tags::PIXEL_DATA)
        .unwrap()
        .to_bytes()
        .unwrap()
        .into_owned();
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
    object.put(DataElement::new(
        tags::PIXEL_DATA,
        VR::OB,
        PrimitiveValue::from(planar),
    ));

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
    assert_eq!(
        redacted
            .element(tags::PLANAR_CONFIGURATION)
            .unwrap()
            .uint16()
            .unwrap(),
        1
    );
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
    if kakadu_ffi_enabled() {
        assert!(matches!(backend, Jpeg2000Backend::Kakadu { .. }));
    } else {
        assert_eq!(backend, Jpeg2000Backend::OpenJpeg);
    }

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
    assert_eq!(
        rendered.width,
        object.element(tags::COLUMNS).unwrap().uint16().unwrap()
    );
    assert_eq!(
        rendered.height,
        object.element(tags::ROWS).unwrap().uint16().unwrap()
    );
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
fn renders_dx_video_frames_as_raw_u8_with_consistent_shape() {
    let object = read_dicom_file(fixture_path("dx.dcm")).unwrap();
    let rendered =
        render_all_dicom_video_frames(&object, &RenderPipelineOptions::default()).unwrap();

    assert!(!rendered.is_empty());

    let rows = object.element(tags::ROWS).unwrap().uint16().unwrap();
    let cols = object.element(tags::COLUMNS).unwrap().uint16().unwrap();

    for frame in &rendered {
        assert_eq!(frame.format, RenderOutputFormat::Raw);
        assert_eq!(frame.bits_allocated, 8);
        assert_eq!(frame.width, cols);
        assert_eq!(frame.height, rows);
        assert_eq!(frame.samples_per_pixel, 1);
        assert_eq!(frame.bytes.len(), usize::from(rows) * usize::from(cols));
    }
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

#[test]
fn renders_single_frame_with_stale_ybr_rct_after_decode() {
    let source = read_dicom_file(fixture_path("sc.dcm")).unwrap();
    let mut object = transcode_dicom_object(
        &source,
        uids::ENCAPSULATED_UNCOMPRESSED_EXPLICIT_VR_LITTLE_ENDIAN,
    )
    .unwrap();

    let samples = object
        .get(tags::SAMPLES_PER_PIXEL)
        .and_then(|element| element.uint16().ok())
        .unwrap_or(1);
    if samples != 3 {
        return;
    }

    object.put(DataElement::new(
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        PrimitiveValue::from("YBR_RCT"),
    ));

    let rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Jpeg,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    assert_eq!(&rendered.bytes[..2], b"\xFF\xD8");
}

#[test]
fn renders_jpeg_baseline_frame_mislabeled_ybr_full_422_without_pink_cast() {
    // Real-world JPEG-baseline secondary-capture images are frequently tagged
    // YBR_FULL_422 even though the JPEG codec already decodes to RGB. The
    // single-frame fast path (try_decode_single_frame_object) must not
    // re-apply a YCbCr->RGB conversion on top of already-RGB bytes -
    // regression test for a bug that produced a magenta/green corrupted
    // image for exactly this combination.
    let Some(path) = optional_fixture_path("wsi.dcm") else {
        return;
    };

    let source = read_dicom_file(&path).unwrap();
    assert_eq!(
        source
            .meta()
            .transfer_syntax()
            .trim_end_matches(['\0', ' ']),
        "1.2.840.10008.1.2.4.50",
        "fixture must be JPEG baseline to exercise the single-frame decode path"
    );

    let reference = render_dicom_frame(
        &source,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();
    let reference_pixel = *image::load_from_memory(&reference.bytes)
        .unwrap()
        .to_rgb8()
        .get_pixel(0, 0);

    let mut mislabeled = source.clone();
    mislabeled.put(DataElement::new(
        tags::PHOTOMETRIC_INTERPRETATION,
        VR::CS,
        PrimitiveValue::from("YBR_FULL_422"),
    ));

    let mislabeled_rendered = render_dicom_frame(
        &mislabeled,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();
    let mislabeled_pixel = *image::load_from_memory(&mislabeled_rendered.bytes)
        .unwrap()
        .to_rgb8()
        .get_pixel(0, 0);

    for channel in 0..3 {
        let diff = (i16::from(reference_pixel[channel]) - i16::from(mislabeled_pixel[channel]))
            .abs();
        assert!(
            diff <= 4,
            "channel {} diverged after YBR_FULL_422 mislabel: reference={:?} mislabeled={:?}",
            channel,
            reference_pixel,
            mislabeled_pixel,
        );
    }
}

#[test]
fn renders_wsi_fixture_without_green_seam() {
    let Some(path) = optional_fixture_path("wsi.dcm") else {
        return;
    };

    assert_rendered_wsi_has_no_green_seam(&path);
}

#[test]
fn renders_ybr_wsi_fixture_without_green_seam() {
    let Some(path) = optional_fixture_path("wsi_ybr.dcm") else {
        return;
    };

    assert_rendered_wsi_has_no_green_seam(&path);
}

fn assert_rendered_wsi_has_no_green_seam(path: &Path) {
    let source = read_dicom_file(path).unwrap();
    let rendered = render_dicom_frame(
        &source,
        RenderOutputFormat::Jpeg,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    let image = image::load_from_memory(&rendered.bytes).unwrap().to_rgb8();
    let mid_row = usize::from(rendered.height) / 2;
    assert!(mid_row > 0, "rendered image height must be > 1");

    let upper = average_row_rgb(&image, mid_row - 1);
    let lower = average_row_rgb(&image, mid_row);

    let lower_green_dominance = lower.1 - lower.0.max(lower.2);
    assert!(
        lower_green_dominance < 80.0,
        "unexpected green-dominant seam in {} at row {}: upper={:?} lower={:?}",
        path.display(),
        mid_row,
        upper,
        lower,
    );

    let seam_delta = (upper.0 - lower.0)
        .abs()
        .max((upper.1 - lower.1).abs())
        .max((upper.2 - lower.2).abs());
    assert!(
        seam_delta < 120.0,
        "unexpected hard color seam in {} at row {}: upper={:?} lower={:?}",
        path.display(),
        mid_row,
        upper,
        lower,
    );
}

fn average_row_rgb(image: &image::RgbImage, row: usize) -> (f64, f64, f64) {
    let y = u32::try_from(row).expect("row index must fit u32");
    let width = image.width();
    let mut red = 0u64;
    let mut green = 0u64;
    let mut blue = 0u64;

    for x in 0..width {
        let pixel = image.get_pixel(x, y).0;
        red += u64::from(pixel[0]);
        green += u64::from(pixel[1]);
        blue += u64::from(pixel[2]);
    }

    let count = f64::from(width.max(1));
    (red as f64 / count, green as f64 / count, blue as f64 / count)
}

fn fixture_bytes(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn optional_fixture_path(name: &str) -> Option<PathBuf> {
    let path = fixture_path(name);
    path.exists().then_some(path)
}

fn optional_repo_fixture_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    path.exists().then_some(path)
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test/files")
        .join(name)
}

fn repo_root_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test/files")
        .join(name)
}

fn assert_core_fields_match(
    expected: &dicom_object::DefaultDicomObject,
    actual: &dicom_object::DefaultDicomObject,
) {
    assert_eq!(
        expected.meta().transfer_syntax(),
        actual.meta().transfer_syntax()
    );
    assert_dataset_fields_match(expected, actual);
}

fn assert_dataset_fields_match(
    expected: &dicom_object::DefaultDicomObject,
    actual: &dicom_object::DefaultDicomObject,
) {
    assert_eq!(
        expected
            .element(tags::SOP_CLASS_UID)
            .unwrap()
            .to_str()
            .unwrap(),
        actual
            .element(tags::SOP_CLASS_UID)
            .unwrap()
            .to_str()
            .unwrap(),
    );
    assert_eq!(
        expected
            .element(tags::SOP_INSTANCE_UID)
            .unwrap()
            .to_str()
            .unwrap(),
        actual
            .element(tags::SOP_INSTANCE_UID)
            .unwrap()
            .to_str()
            .unwrap(),
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
    let bytes = object
        .element(tags::PIXEL_DATA)
        .ok()?
        .to_bytes()
        .ok()?
        .into_owned();
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

// ---------------------------------------------------------------------------------------------
// DIMSE (echo/store/find/move SCU) - in-process mock SCP round trips
// ---------------------------------------------------------------------------------------------

fn dimse_implicit_vr_le() -> &'static dicom_encoding::TransferSyntax {
    use dicom_encoding::TransferSyntaxIndex;
    dicom_transfer_syntax_registry::TransferSyntaxRegistry
        .get(uids::IMPLICIT_VR_LITTLE_ENDIAN)
        .unwrap()
}

/// Binds a mock SCP to a loopback port, accepting a single association proposing
/// `abstract_syntax` (with the default Explicit/Implicit VR LE transfer syntaxes), and running
/// `handler` against it before handling the client's release request. Returns the join handle
/// (call `.join().unwrap()` after the client-side call under test completes) and the address to
/// connect to.
fn spawn_mock_scp(
    abstract_syntax: impl Into<String>,
    handler: impl FnOnce(&mut dicom_ul::ServerAssociation<std::net::TcpStream>) + Send + 'static,
) -> (std::thread::JoinHandle<()>, std::net::SocketAddr) {
    let abstract_syntax = abstract_syntax.into();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(abstract_syntax);

    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        handler(&mut association);
        let pdu = association.receive().unwrap();
        assert_eq!(pdu, dicom_ul::pdu::Pdu::ReleaseRQ);
        association.send(&dicom_ul::pdu::Pdu::ReleaseRP).unwrap();
    });
    (handle, addr)
}

/// Reads one Command PDV off the wire (assumes it's alone in its PDU - true for every request
/// this replaces except `store_scu`'s combined command+data PDU, handled separately below).
fn dimse_recv_command(
    association: &mut dicom_ul::ServerAssociation<std::net::TcpStream>,
) -> InMemDicomObject {
    match association.receive().unwrap() {
        dicom_ul::pdu::Pdu::PData { data } => {
            InMemDicomObject::read_dataset_with_ts(data[0].data.as_slice(), dimse_implicit_vr_le()).unwrap()
        }
        other => panic!("expected a Command P-Data PDU, got {other:?}"),
    }
}

fn dimse_send_command(
    association: &mut dicom_ul::ServerAssociation<std::net::TcpStream>,
    pc_id: u8,
    command: &InMemDicomObject,
) {
    let mut data = Vec::new();
    command.write_dataset_with_ts(&mut data, dimse_implicit_vr_le()).unwrap();
    association
        .send(&dicom_ul::pdu::Pdu::PData {
            data: vec![dicom_ul::pdu::PDataValue {
                presentation_context_id: pc_id,
                value_type: dicom_ul::pdu::PDataValueType::Command,
                is_last: true,
                data,
            }],
        })
        .unwrap();
}

fn dimse_message_id(command: &InMemDicomObject) -> u16 {
    command.element(tags::MESSAGE_ID).unwrap().to_int().unwrap()
}

#[test]
fn echo_scu_round_trips_success_status() {
    let (scp_handle, addr) = spawn_mock_scp(uids::VERIFICATION, |association| {
        let request = dimse_recv_command(association);
        let message_id = dimse_message_id(&request);
        let pc_id = association.presentation_contexts()[0].id;

        let response = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, uids::VERIFICATION)),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8030])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
        ]);
        dimse_send_command(association, pc_id, &response);
    });

    let status = echo_scu(
        &addr.to_string(),
        EchoScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            timeout: None,
        },
    )
    .unwrap();

    assert_eq!(status, 0);
    scp_handle.join().unwrap();
}

/// Receives a Command + its Data Set (C-STORE's dataset, or C-FIND/C-MOVE's Identifier),
/// tolerating either of `send_command_with_data`'s two send paths: small enough combines both
/// into one PDU (2 PDataValues in a single `receive()`), while anything too large for one PDU
/// (store_scu's only, in practice - find/move identifiers are always small) sends the Command
/// alone, then streams the Data separately via `send_pdata` (arriving as its own PData PDU(s),
/// read here with `receive_pdata()`).
fn dimse_recv_command_with_data(
    association: &mut dicom_ul::ServerAssociation<std::net::TcpStream>,
) -> (u8, InMemDicomObject, Vec<u8>) {
    match association.receive().unwrap() {
        dicom_ul::pdu::Pdu::PData { data } if data.len() == 2 => {
            let command =
                InMemDicomObject::read_dataset_with_ts(data[0].data.as_slice(), dimse_implicit_vr_le()).unwrap();
            (data[0].presentation_context_id, command, data[1].data.clone())
        }
        dicom_ul::pdu::Pdu::PData { data } if data.len() == 1 => {
            let pc_id = data[0].presentation_context_id;
            let command =
                InMemDicomObject::read_dataset_with_ts(data[0].data.as_slice(), dimse_implicit_vr_le()).unwrap();
            let mut reader = association.receive_pdata();
            let mut dataset_bytes = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut dataset_bytes).unwrap();
            (pc_id, command, dataset_bytes)
        }
        other => panic!("expected a Command P-Data PDU, got {other:?}"),
    }
}

#[test]
fn store_scu_round_trips_status_per_file() {
    // Small enough to exercise store_scu's combined command+data PDU path deterministically;
    // a large-file (split/streamed) run is covered by store_scu_streams_data_for_large_files.
    let source = fixture_path("sr.dcm");
    let object = read_dicom_file(&source).unwrap();
    let sop_class_uid = object.meta().media_storage_sop_class_uid.trim_end_matches(['\0', ' ']).to_owned();
    let sop_instance_uid = object.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned();

    let expected_sop_class_uid = sop_class_uid.clone();
    let (scp_handle, addr) = spawn_mock_scp(sop_class_uid, move |association| {
        let (pc_id, request, dataset_bytes) = dimse_recv_command_with_data(association);
        assert!(!dataset_bytes.is_empty());
        assert_eq!(
            request.element(tags::AFFECTED_SOP_CLASS_UID).unwrap().to_str().unwrap(),
            expected_sop_class_uid,
        );
        let message_id = dimse_message_id(&request);

        let response = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, expected_sop_class_uid.clone())),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8001])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
        ]);
        dimse_send_command(association, pc_id, &response);
    });

    let results = store_scu(
        &addr.to_string(),
        &[source],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            never_transcode: true,
        },
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, 0);
    assert_eq!(results[0].sop_instance_uid, sop_instance_uid);
    scp_handle.join().unwrap();
}

#[test]
fn store_scu_streams_data_for_large_files() {
    // Large enough that command+data won't fit in one PDU under the default max PDU length,
    // forcing store_scu's send_pdata streaming path (vs. the combined-PDU path exercised by
    // store_scu_round_trips_status_per_file).
    let source = fixture_path("dx.dcm");
    let object = read_dicom_file(&source).unwrap();
    let sop_class_uid = object.meta().media_storage_sop_class_uid.trim_end_matches(['\0', ' ']).to_owned();
    let sop_instance_uid = object.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned();

    let expected_sop_class_uid = sop_class_uid.clone();
    let (scp_handle, addr) = spawn_mock_scp(sop_class_uid, move |association| {
        let (pc_id, request, dataset_bytes) = dimse_recv_command_with_data(association);
        assert!(dataset_bytes.len() > 16384, "expected a large streamed dataset, got {} bytes", dataset_bytes.len());
        assert_eq!(
            request.element(tags::AFFECTED_SOP_CLASS_UID).unwrap().to_str().unwrap(),
            expected_sop_class_uid,
        );
        let message_id = dimse_message_id(&request);

        let response = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, expected_sop_class_uid.clone())),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8001])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
        ]);
        dimse_send_command(association, pc_id, &response);
    });

    let results = store_scu(
        &addr.to_string(),
        &[source],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            never_transcode: true,
        },
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, 0);
    assert_eq!(results[0].sop_instance_uid, sop_instance_uid);
    scp_handle.join().unwrap();
}

#[test]
fn find_scu_collects_pending_matches_until_success_status() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;
    let (scp_handle, addr) = spawn_mock_scp(abstract_syntax, move |association| {
        let pc = association.presentation_contexts()[0].clone();
        let ts = {
            use dicom_encoding::TransferSyntaxIndex;
            dicom_transfer_syntax_registry::TransferSyntaxRegistry.get(&pc.transfer_syntax).unwrap()
        };

        let (_pc_id, request, identifier_bytes) = dimse_recv_command_with_data(association);
        let message_id = dimse_message_id(&request);
        let query = InMemDicomObject::read_dataset_with_ts(identifier_bytes.as_slice(), ts).unwrap();
        assert_eq!(query.element(tags::QUERY_RETRIEVE_LEVEL).unwrap().to_str().unwrap(), "STUDY");
        assert_eq!(query.element(tags::PATIENT_ID).unwrap().to_str().unwrap(), "MRN123");

        // one pending match: Command (status=pending) + Data (the matched Identifier)
        let pending_command = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8020])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0001])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0xFF00])),
        ]);
        dimse_send_command(association, pc.id, &pending_command);

        let matched = InMemDicomObject::from_element_iter([
            DataElement::new(tags::STUDY_INSTANCE_UID, VR::UI, dicom_value!(Str, "1.2.3.4.5")),
            DataElement::new(tags::PATIENT_NAME, VR::PN, dicom_value!(Str, "DOE^JANE")),
        ]);
        let mut matched_bytes = Vec::new();
        matched.write_dataset_with_ts(&mut matched_bytes, ts).unwrap();
        association
            .send(&dicom_ul::pdu::Pdu::PData {
                data: vec![dicom_ul::pdu::PDataValue {
                    presentation_context_id: pc.id,
                    value_type: dicom_ul::pdu::PDataValueType::Data,
                    is_last: true,
                    data: matched_bytes,
                }],
            })
            .unwrap();

        // final response: Command only, status=success
        let final_command = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8020])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
        ]);
        dimse_send_command(association, pc.id, &final_command);
    });

    let mut query = std::collections::HashMap::new();
    query.insert("PatientID".to_owned(), "MRN123".to_owned());
    let matches = find_scu(
        &addr.to_string(),
        &query,
        FindScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
        },
    )
    .unwrap();

    assert_eq!(matches.len(), 1);
    let value: serde_json::Value = serde_json::from_str(&matches[0]).unwrap();
    assert_eq!(value["0020000D"], "1.2.3.4.5");
    assert_eq!(value["00100010"], "DOE^JANE");
    scp_handle.join().unwrap();
}

#[test]
fn move_scu_collects_suboperation_progress_until_terminal_status() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
    let (scp_handle, addr) = spawn_mock_scp(abstract_syntax, move |association| {
        let (pc_id, request, _identifier_bytes) = dimse_recv_command_with_data(association);
        let message_id = dimse_message_id(&request);
        assert_eq!(
            request.element(tags::MOVE_DESTINATION).unwrap().to_str().unwrap(),
            "GENERICAE",
        );

        let pending = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8021])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0xFF00])),
            DataElement::new(tags::NUMBER_OF_REMAINING_SUBOPERATIONS, VR::US, dicom_value!(U16, [1])),
            DataElement::new(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
            DataElement::new(tags::NUMBER_OF_FAILED_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
            DataElement::new(tags::NUMBER_OF_WARNING_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
        ]);
        dimse_send_command(association, pc_id, &pending);

        let final_response = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8021])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
            DataElement::new(tags::NUMBER_OF_REMAINING_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
            DataElement::new(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS, VR::US, dicom_value!(U16, [1])),
            DataElement::new(tags::NUMBER_OF_FAILED_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
            DataElement::new(tags::NUMBER_OF_WARNING_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
        ]);
        dimse_send_command(association, pc_id, &final_response);
    });

    let result = move_scu(
        &addr.to_string(),
        "GENERICAE",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
        },
    )
    .unwrap();

    assert_eq!(result.status, 0);
    assert_eq!(result.completed, 1);
    assert_eq!(result.failed, 0);
    assert_eq!(result.warning, 0);
    assert_eq!(result.remaining, 0);
    scp_handle.join().unwrap();
}

// ---------------------------------------------------------------------------------------------
// DICOM SCP - end-to-end round trip against the same SCU functions tested above
// ---------------------------------------------------------------------------------------------

struct TestScpHandlers {
    find_query: std::sync::Mutex<Option<std::collections::HashMap<String, String>>>,
    find_response: Vec<std::collections::HashMap<String, String>>,
    move_calls: std::sync::Mutex<Vec<(String, String)>>,
    move_result: bool,
    association_complete: std::sync::Mutex<Option<std::collections::HashMap<String, Vec<String>>>>,
}

impl ScpHandlers for TestScpHandlers {
    fn on_find(
        &self,
        filter: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<std::collections::HashMap<String, String>>, String> {
        *self.find_query.lock().unwrap() = Some(filter.clone());
        Ok(self.find_response.clone())
    }

    fn on_move(&self, study_instance_uid: &str, move_destination_ae: &str) -> Result<bool, String> {
        self.move_calls.lock().unwrap().push((study_instance_uid.to_owned(), move_destination_ae.to_owned()));
        Ok(self.move_result)
    }

    fn on_association_complete(&self, stored_instances_by_study: &std::collections::HashMap<String, Vec<String>>) {
        *self.association_complete.lock().unwrap() = Some(stored_instances_by_study.clone());
    }
}

fn wait_for<T: Clone>(mut poll: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(value) = poll() {
            return value;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for condition");
}

#[test]
fn scp_round_trips_echo_store_find_move_against_the_scu_functions() {
    let cache_dir = std::env::temp_dir().join(format!(
        "dcmnorm-scp-test-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&cache_dir).unwrap();

    let mut find_match = std::collections::HashMap::new();
    find_match.insert("StudyInstanceUID".to_owned(), "1.2.3.4.5".to_owned());
    find_match.insert("PatientName".to_owned(), "DOE^JANE".to_owned());

    let handlers = std::sync::Arc::new(TestScpHandlers {
        find_query: std::sync::Mutex::new(None),
        find_response: vec![find_match],
        move_calls: std::sync::Mutex::new(Vec::new()),
        move_result: true,
        association_complete: std::sync::Mutex::new(None),
    });

    let scp = start_scp(
        0,
        cache_dir.clone(),
        handlers.clone(),
        ScpOptions { ae_title: "TEST-SCP".to_owned(), ..Default::default() },
    )
    .unwrap();
    let destination = format!("127.0.0.1:{}", scp.local_port());

    // C-ECHO
    let status = echo_scu(
        &destination,
        EchoScuOptions { calling_ae_title: "TEST-SCU".to_owned(), called_ae_title: None, timeout: None },
    )
    .unwrap();
    assert_eq!(status, 0);

    // C-STORE
    let source = fixture_path("sr.dcm");
    let object = read_dicom_file(&source).unwrap();
    let sop_instance_uid = object.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned();
    let study_instance_uid = object.element(tags::STUDY_INSTANCE_UID).unwrap().to_str().unwrap().trim().to_owned();
    let modality = object.element(tags::MODALITY).unwrap().to_str().unwrap().trim().to_owned();

    let results = store_scu(
        &destination,
        &[source],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: None,
            max_pdu_length: 16384,
            never_transcode: true,
        },
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, 0);
    assert_eq!(results[0].sop_instance_uid, sop_instance_uid);

    let expected_path = cache_dir.join(format!("S_{study_instance_uid}")).join(format!("{modality}_{sop_instance_uid}.dcm"));
    assert!(expected_path.exists(), "expected stored file at {expected_path:?}");
    // The file that got written must itself be valid, readable DICOM with the same SOP instance.
    let stored_object = read_dicom_file(&expected_path).unwrap();
    assert_eq!(
        stored_object.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']),
        sop_instance_uid
    );

    // C-FIND
    let mut query = std::collections::HashMap::new();
    query.insert("PatientID".to_owned(), "MRN123".to_owned());
    let matches = find_scu(
        &destination,
        &query,
        FindScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: None,
            max_pdu_length: 16384,
            timeout: None,
        },
    )
    .unwrap();
    assert_eq!(matches.len(), 1);
    let match_value: serde_json::Value = serde_json::from_str(&matches[0]).unwrap();
    assert_eq!(match_value["0020000D"], "1.2.3.4.5");
    let seen_query = handlers.find_query.lock().unwrap().clone().unwrap();
    assert_eq!(seen_query.get("PatientID").map(String::as_str), Some("MRN123"));

    // C-MOVE
    let move_result = move_scu(
        &destination,
        "GENERICAE",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: None,
            max_pdu_length: 16384,
            timeout: None,
        },
    )
    .unwrap();
    assert_eq!(move_result.status, 0);
    let move_calls = handlers.move_calls.lock().unwrap().clone();
    assert_eq!(move_calls, vec![("1.2.3.4.5".to_owned(), "GENERICAE".to_owned())]);

    // Association-complete fires after the C-STORE association's release completes, which can
    // race this test observing it - poll rather than assume synchronous completion.
    let completed = wait_for(|| handlers.association_complete.lock().unwrap().clone());
    assert_eq!(completed.get(&study_instance_uid).map(Vec::len), Some(1));

    scp.stop();
    fs::remove_dir_all(&cache_dir).ok();
}

/// Regression test for a production incident: a C-FIND match whose text contained a non-ASCII
/// character (an en dash, "–") made `handle_find` fail to encode that one match's dataset -
/// aborting the whole C-FIND response and, since that error propagated out of the association's
/// handling loop, closing the connection out from under the client. The client-side symptom was
/// indistinguishable from the peer dropping the connection for any other reason ("DICOM
/// association error: Connection closed by peer"), which is what made this hard to place
/// initially. Fixed by declaring SpecificCharacterSet (ISO_IR 192 / UTF-8) on every outgoing
/// match dataset - see the comment at its call site in `handle_find`.
#[test]
fn scp_find_response_with_non_ascii_text_does_not_close_the_connection() {
    let cache_dir = std::env::temp_dir().join(format!(
        "dcmnorm-scp-nonascii-test-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&cache_dir).unwrap();

    let mut find_match = std::collections::HashMap::new();
    find_match.insert("StudyInstanceUID".to_owned(), "1.2.3.4.5".to_owned());
    find_match.insert("StudyDescription".to_owned(), "CT Chest \u{2013} followup".to_owned());

    let handlers = std::sync::Arc::new(TestScpHandlers {
        find_query: std::sync::Mutex::new(None),
        find_response: vec![find_match],
        move_calls: std::sync::Mutex::new(Vec::new()),
        move_result: true,
        association_complete: std::sync::Mutex::new(None),
    });

    let scp = start_scp(
        0,
        cache_dir.clone(),
        handlers.clone(),
        ScpOptions { ae_title: "TEST-SCP".to_owned(), ..Default::default() },
    )
    .unwrap();
    let destination = format!("127.0.0.1:{}", scp.local_port());

    let matches = find_scu(
        &destination,
        &std::collections::HashMap::new(),
        FindScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: None,
            max_pdu_length: 16384,
            timeout: None,
        },
    )
    .unwrap();
    assert_eq!(matches.len(), 1);
    let match_value: serde_json::Value = serde_json::from_str(&matches[0]).unwrap();
    assert_eq!(match_value["00081030"], "CT Chest \u{2013} followup");

    scp.stop();
    fs::remove_dir_all(&cache_dir).ok();
}
