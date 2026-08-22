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
    list_transfer_syntax_support, move_scu, parse_attribute_override,
    probe_dicom_file_for_sop_class_uid, read_dicom_bytes,
    read_dicom_file, read_dicom_json, read_dicom_json_full, read_dicom_json_full_with_source,
    read_dicom_json_with_options, read_dicom_json_with_source,
    redact_dicom_pixels_to_transfer_syntax, remove_attribute, render_all_dicom_video_frames, render_dicom_frame,
    set_attribute, start_scp, store_scu, transcode_dicom_object, write_dicom_bytes, write_dicom_file,
    write_dicom_json, write_dicom_json_full, write_dicom_json_full_with_source,
    write_dicom_json_with_options, write_dicom_json_with_source, BoundingBox, BoxLength,
    DicomJsonBulkDataMode, DicomJsonFormat, DicomJsonKeyStyle, DicomJsonReadOptions,
    CancelMode, CancelSignal, DicomJsonWriteOptions, DimseError, DimseLogger, EchoScuOptions, FindScuOptions,
    Jpeg2000Backend, MoveScuOptions, RenderError, RenderOutputFormat, RenderPipelineOptions,
    ScpHandlers, ScpOptions, StoreScuOptions,
    build_volume, reformat_plane, Interpolation, PlaneParams, SlabProjection, VolumeError,
    pack_dicom_frame_stack_texture, pack_dicom_frame_texture, ContentKind, TextureCompression, TextureExportError,
};

#[test]
fn set_attribute_writes_media_storage_sop_class_uid_to_meta_not_dataset() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let mut object = read_dicom_bytes(&source).unwrap();

    let (tag, vr, value) =
        parse_attribute_override("MediaStorageSOPClassUID=1.2.840.10008.5.1.4.1.1.7").unwrap();
    set_attribute(&mut object, tag, vr, value).unwrap();

    assert_eq!(
        object.meta().media_storage_sop_class_uid,
        "1.2.840.10008.5.1.4.1.1.7",
        "set_attribute should update the real File Meta Information group"
    );
    assert!(
        object.element(tags::MEDIA_STORAGE_SOP_CLASS_UID).is_err(),
        "MediaStorageSOPClassUID must not also leak into the dataset"
    );
}

#[test]
fn set_attribute_writes_media_storage_sop_instance_uid_to_meta() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let mut object = read_dicom_bytes(&source).unwrap();

    let (tag, vr, value) = parse_attribute_override(
        "MediaStorageSOPInstanceUID=1.2.840.10008.114051.1.2.3.4.5",
    )
    .unwrap();
    set_attribute(&mut object, tag, vr, value).unwrap();

    assert_eq!(
        object.meta().media_storage_sop_instance_uid,
        "1.2.840.10008.114051.1.2.3.4.5"
    );
    assert!(object.element(tags::MEDIA_STORAGE_SOP_INSTANCE_UID).is_err());
}

#[test]
fn set_attribute_rejects_unsettable_meta_elements() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let mut object = read_dicom_bytes(&source).unwrap();
    let original_transfer_syntax = object.meta().transfer_syntax.clone();

    // TransferSyntaxUID is deliberately not settable through this path: doing
    // so would desync the meta value from the dataset's actual pixel
    // encoding, since set_attribute never transcodes pixel data.
    let (tag, vr, value) =
        parse_attribute_override("TransferSyntaxUID=1.2.840.10008.1.2.1").unwrap();
    let result = set_attribute(&mut object, tag, vr, value);

    assert!(result.is_err());
    assert_eq!(object.meta().transfer_syntax, original_transfer_syntax);
}

#[test]
fn set_attribute_still_writes_dataset_elements() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let mut object = read_dicom_bytes(&source).unwrap();

    let (tag, vr, value) = parse_attribute_override("PatientName=DOE^JOHN").unwrap();
    set_attribute(&mut object, tag, vr, value).unwrap();

    assert_eq!(
        object.element(tags::PATIENT_NAME).unwrap().to_str().unwrap(),
        "DOE^JOHN"
    );
}

#[test]
fn remove_attribute_clears_media_storage_sop_instance_uid() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let mut object = read_dicom_bytes(&source).unwrap();
    assert!(!object.meta().media_storage_sop_instance_uid.is_empty());

    let was_present = remove_attribute(&mut object, tags::MEDIA_STORAGE_SOP_INSTANCE_UID);

    assert!(was_present, "the tag was present before removal");
    assert!(
        object.meta().media_storage_sop_instance_uid.is_empty(),
        "remove_attribute should clear the real File Meta Information field"
    );
}

#[test]
fn remove_attribute_on_absent_meta_field_reports_not_present() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let mut object = read_dicom_bytes(&source).unwrap();
    remove_attribute(&mut object, tags::MEDIA_STORAGE_SOP_INSTANCE_UID);

    let was_present_second_time = remove_attribute(&mut object, tags::MEDIA_STORAGE_SOP_INSTANCE_UID);

    assert!(!was_present_second_time);
}

#[test]
fn remove_attribute_still_removes_dataset_elements() {
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let mut object = read_dicom_bytes(&source).unwrap();
    assert!(object.element(tags::PATIENT_NAME).is_ok());

    let was_present = remove_attribute(&mut object, tags::PATIENT_NAME);

    assert!(was_present);
    assert!(object.element(tags::PATIENT_NAME).is_err());
}

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

// test/files/overlay.dcm is a purpose-built synthetic fixture (see
// `generate_overlay_fixture_scratch` in this file's git history for how it was produced) - not
// derived from any real study, so it carries no PHI. It's a tiny 8x8 MONOCHROME2 image (all
// pixels black) with one overlay plane (group 0x6000, distinct `OverlayData`): a diagonal line,
// pixel (r, r) set for r in 0..8, packed LSB-first per DICOM PS3.5 (byte[r] = 1 << r).
#[test]
fn renders_first_overlay_by_default() {
    let object = read_dicom_file(fixture_path("overlay.dcm")).unwrap();
    let rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();

    assert_eq!(rendered.overlays.len(), 1);
    assert_eq!(rendered.overlays[0].index, 0);
    assert_eq!(rendered.overlays[0].group, 0x6000);
    assert_eq!(rendered.selected_overlay_index, Some(0));

    let image = image::load_from_memory(&rendered.bytes).unwrap().to_luma8();
    // (0, 0) is on the overlay's diagonal (set); default overlay color is green ([0, 255, 0]),
    // which luma-converts to 182 via the same rgb_to_luma formula draw_bounding_boxes/
    // maybe_pad_frame already use elsewhere in this file.
    assert_eq!(image.get_pixel(0, 0).0[0], 182);
    // (1, 0) is off the diagonal (clear), and the underlying image is black there.
    assert_eq!(image.get_pixel(1, 0).0[0], 0);
}

#[test]
fn overlay_can_be_disabled() {
    let object = read_dicom_file(fixture_path("overlay.dcm")).unwrap();
    let options = RenderPipelineOptions {
        show_overlays: false,
        ..RenderPipelineOptions::default()
    };
    let rendered = render_dicom_frame(&object, RenderOutputFormat::Png, &options).unwrap();

    assert_eq!(
        rendered.overlays.len(),
        1,
        "overlays should still be reported even when not rendered, so a client can offer to enable them"
    );
    assert_eq!(rendered.selected_overlay_index, None);

    let image = image::load_from_memory(&rendered.bytes).unwrap().to_luma8();
    assert_eq!(image.get_pixel(0, 0).0[0], 0, "overlay pixel should be untouched when disabled");
}

#[test]
fn overlay_color_is_configurable() {
    let object = read_dicom_file(fixture_path("overlay.dcm")).unwrap();
    let options = RenderPipelineOptions {
        overlay_color: [255, 0, 0],
        ..RenderPipelineOptions::default()
    };
    let rendered = render_dicom_frame(&object, RenderOutputFormat::Png, &options).unwrap();

    let image = image::load_from_memory(&rendered.bytes).unwrap().to_luma8();
    // Red ([255, 0, 0]) luma-converts to 54.
    assert_eq!(image.get_pixel(0, 0).0[0], 54);
}

#[test]
fn out_of_range_overlay_index_is_rejected() {
    let object = read_dicom_file(fixture_path("overlay.dcm")).unwrap();
    let options = RenderPipelineOptions {
        overlay_index: Some(5),
        ..RenderPipelineOptions::default()
    };
    let error = render_dicom_frame(&object, RenderOutputFormat::Png, &options).unwrap_err();

    assert!(matches!(
        error,
        RenderError::InvalidOverlayIndex {
            requested: 5,
            available: 1
        }
    ));
}

// test/files/overlay_multi.dcm is another purpose-built synthetic fixture (no PHI, not derived
// from a real study): an 8x8 MONOCHROME2 image with two overlay planes, both distinct
// `OverlayData` - group 0x6000 "Diagonal" (pixel (r, r) set) and group 0x6002 "Anti-diagonal"
// (pixel (r, 7-r) set), so the two are trivially distinguishable by pixel content.
#[test]
fn selects_among_multiple_overlays_by_index() {
    let object = read_dicom_file(fixture_path("overlay_multi.dcm")).unwrap();

    let default_rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();
    assert_eq!(default_rendered.overlays.len(), 2);
    assert_eq!(default_rendered.overlays[0].group, 0x6000);
    assert_eq!(default_rendered.overlays[0].label.as_deref(), Some("Diagonal"));
    assert_eq!(default_rendered.overlays[1].group, 0x6002);
    assert_eq!(default_rendered.overlays[1].label.as_deref(), Some("Anti-diagonal"));
    assert_eq!(default_rendered.selected_overlay_index, Some(0));

    let second_options = RenderPipelineOptions {
        overlay_index: Some(1),
        ..RenderPipelineOptions::default()
    };
    let second_rendered = render_dicom_frame(&object, RenderOutputFormat::Png, &second_options).unwrap();
    assert_eq!(second_rendered.selected_overlay_index, Some(1));

    let default_image = image::load_from_memory(&default_rendered.bytes).unwrap().to_luma8();
    let second_image = image::load_from_memory(&second_rendered.bytes).unwrap().to_luma8();

    // Row 0: the diagonal overlay sets column 0, the anti-diagonal overlay sets column 7.
    assert_eq!(default_image.get_pixel(0, 0).0[0], 182);
    assert_eq!(default_image.get_pixel(7, 0).0[0], 0);
    assert_eq!(second_image.get_pixel(0, 0).0[0], 0);
    assert_eq!(second_image.get_pixel(7, 0).0[0], 182);
}

// test/files/overlay_embedded.dcm is a third purpose-built synthetic fixture: an 8x8, 16-bit
// (BitsStored=14/HighBit=13, matching the headroom convention real DX/CR images use) image with
// the same diagonal pattern, but encoded the legacy way - embedded in the unused high bit 15 of
// PixelData itself (OverlayBitsAllocated == image BitsAllocated, OverlayBitPosition=15) rather
// than a distinct OverlayData element.
#[test]
fn renders_overlay_embedded_in_pixel_data_high_bit() {
    let object = read_dicom_file(fixture_path("overlay_embedded.dcm")).unwrap();

    let rendered = render_dicom_frame(
        &object,
        RenderOutputFormat::Png,
        &RenderPipelineOptions::default(),
    )
    .unwrap();
    assert_eq!(rendered.overlays.len(), 1);
    assert_eq!(rendered.selected_overlay_index, Some(0));

    let baseline = render_dicom_frame(
        &object,
        RenderOutputFormat::Png,
        &RenderPipelineOptions {
            show_overlays: false,
            ..RenderPipelineOptions::default()
        },
    )
    .unwrap();

    let with_overlay = image::load_from_memory(&rendered.bytes).unwrap().to_luma8();
    let without_overlay = image::load_from_memory(&baseline.bytes).unwrap().to_luma8();

    assert_eq!(with_overlay.get_pixel(0, 0).0[0], 182);
    assert_eq!(with_overlay.get_pixel(1, 0).0[0], 0);
    assert_ne!(
        without_overlay.get_pixel(0, 0).0[0],
        182,
        "baseline pixel should not already be overlay-colored"
    );
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
            on_log: None,
            cancel: None,
        },
    )
    .unwrap();

    assert_eq!(status, 0);
    scp_handle.join().unwrap();
}

/// `echo_scu` gained `cancel` support alongside `move_scu`'s (this file's `poll_bounded`
/// generalizes over it) - a peer that accepts the association but never responds to the
/// C-ECHO-RQ must not be able to hang the call once a caller signals cancellation, even with
/// no `timeout` configured at all.
#[test]
fn echo_scu_returns_cancelled_error_when_signalled_mid_wait() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(uids::VERIFICATION);

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let _ = dimse_recv_command(&mut association);
        // Silence: never sends a C-ECHO-RSP. Held open comfortably longer than one
        // `poll_bounded` tick (see `POLL_INTERVAL`) past the cancel below, so the association
        // being torn down here happens because the client cancelled it, not because this thread
        // dropped the connection first.
        std::thread::sleep(std::time::Duration::from_secs(2));
    });

    let cancel = CancelSignal::new();
    let cancel_setter = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel_setter.request(CancelMode::Release);
    });

    let started = std::time::Instant::now();
    let result = echo_scu(
        &addr.to_string(),
        EchoScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            timeout: None,
            on_log: None,
            cancel: Some(cancel),
        },
    );

    assert!(matches!(result, Err(DimseError::Cancelled(CancelMode::Release))), "expected Cancelled, got {result:?}");
    // Cancel is only actually observed at the next `poll_bounded` tick (up to one `POLL_INTERVAL`
    // after it's set, not immediately) - bounded well above that, well below the mock SCP's own
    // 2s silence window above.
    assert!(started.elapsed() < std::time::Duration::from_millis(900), "cancel took too long to be noticed: {:?}", started.elapsed());
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
            timeout: None,
            on_log: None,
            cancel: None,
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
            timeout: None,
            on_log: None,
            cancel: None,
        },
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, 0);
    assert_eq!(results[0].sop_instance_uid, sop_instance_uid);
    scp_handle.join().unwrap();
}

/// Regression test for a production failure: two files sharing a SOP class but with different
/// native transfer syntaxes, one of them JPEG 2000 (which this build can decode but never
/// encode - see `can_encode_transfer_syntax` in `io.rs`). An older version of `store_scu`
/// proposed only one presentation context per SOP class, seeded from whichever file of that
/// class was probed first - if that file's native transfer syntax was JPEG 2000 and a peer
/// accepted it, any other file of that class then needed an encode into JPEG 2000 that always
/// failed, aborting the whole association. `store_scu` now proposes one context per (SOP class,
/// transfer syntax) pair actually present among the files being sent, so both files here land
/// under their own native transfer syntax with no transcode at all.
///
/// Uses two real fixtures rather than a hand-built second object: an earlier version of this
/// test stripped `PixelData` from a cloned/retagged object to sidestep encapsulation mismatches,
/// but that made `transcode_dicom_object` a no-op (nothing to encode) and the test passed even
/// against the pre-fix code. `dx.dcm`'s native pixel data has to actually survive an attempted
/// transcode for the failure this guards against to be reachable at all.
#[test]
fn store_scu_avoids_transcoding_into_a_decode_only_transfer_syntax_shared_by_sop_class() {
    let path_a = fixture_path("dx2.dcm");
    let path_b = fixture_path("dx.dcm");

    let object_a = read_dicom_file(&path_a).unwrap();
    let object_b = read_dicom_file(&path_b).unwrap();
    let sop_class_uid = object_a.meta().media_storage_sop_class_uid.trim_end_matches(['\0', ' ']).to_owned();
    let sop_instance_a = object_a.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned();
    let sop_instance_b = object_b.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned();
    assert_eq!(
        object_b.meta().media_storage_sop_class_uid.trim_end_matches(['\0', ' ']),
        sop_class_uid,
        "fixtures must share a SOP class for this test to be meaningful"
    );
    assert_eq!(object_a.meta().transfer_syntax.trim_end_matches(['\0', ' ']), "1.2.840.10008.1.2.4.90");
    assert_eq!(object_b.meta().transfer_syntax.trim_end_matches(['\0', ' ']), uids::EXPLICIT_VR_LITTLE_ENDIAN);

    let expected_sop_class_uid = sop_class_uid.clone();
    let (scp_handle, addr) = spawn_mock_scp(sop_class_uid, move |association| {
        for _ in 0..2 {
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
        }
    });

    let results = store_scu(
        &addr.to_string(),
        &[path_a.clone(), path_b.clone()],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            never_transcode: false,
            timeout: None,
            on_log: None,
            cancel: None,
        },
    )
    .unwrap();

    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result.status == 0), "expected both sends to succeed: {results:?}");
    assert_eq!(results[0].sop_instance_uid, sop_instance_a);
    assert_eq!(results[1].sop_instance_uid, sop_instance_b);

    scp_handle.join().unwrap();
}

/// Regression test for a production failure: a peer that rejects a file's native (compressed)
/// transfer syntax but accepts `store_scu`'s own Explicit/Implicit VR Little Endian fallback
/// context. `select_store_presentation_context`'s fallback branch used to call
/// `can_encode_pixel_data` (via `can_encode_transfer_syntax`) to decide whether a negotiated
/// context was usable as a transcode target - but that check only looks at pixel-data *codec*
/// availability, which is unconditionally `false` for a non-encapsulated transfer syntax like
/// Explicit/Implicit VR Little Endian (there's no codec involved in writing a native dataset at
/// all). So even though the peer had just accepted the fallback context, it was never picked as
/// eligible, and the send failed with `NoAcceptablePresentationContext` - see `can_encode_transfer_syntax`
/// in `io.rs`.
#[test]
fn store_scu_transcodes_into_fallback_context_when_peer_rejects_native_transfer_syntax() {
    let source = fixture_path("dx2.dcm");
    let object = read_dicom_file(&source).unwrap();
    let sop_class_uid = object.meta().media_storage_sop_class_uid.trim_end_matches(['\0', ' ']).to_owned();
    let sop_instance_uid = object.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned();
    assert_eq!(object.meta().transfer_syntax.trim_end_matches(['\0', ' ']), "1.2.840.10008.1.2.4.90");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let expected_sop_class_uid = sop_class_uid.clone();
    let scp_handle = std::thread::spawn(move || {
        // Only the two fallback transfer syntaxes store_scu can always write - never the file's
        // own native (compressed) one - simulating a peer that can't handle JPEG 2000.
        let scp = dicom_ul::association::server::ServerAssociationOptions::new()
            .ae_title("MOCK-SCP")
            .with_abstract_syntax(expected_sop_class_uid.clone())
            .with_transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .with_transfer_syntax(uids::IMPLICIT_VR_LITTLE_ENDIAN);
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();

        let (pc_id, request, dataset_bytes) = dimse_recv_command_with_data(&mut association);
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
        dimse_send_command(&mut association, pc_id, &response);

        let pdu = association.receive().unwrap();
        assert_eq!(pdu, dicom_ul::pdu::Pdu::ReleaseRQ);
        association.send(&dicom_ul::pdu::Pdu::ReleaseRP).unwrap();
    });

    let results = store_scu(
        &addr.to_string(),
        &[source],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            never_transcode: false,
            timeout: None,
            on_log: None,
            cancel: None,
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
            on_log: None,
            cancel: None,
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
            "MOVE-DEST",
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
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
            stale_data_path: None,
            stale_data_timeout: None,
            on_log: None,
            cancel: None,
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

/// Regression test for a RamSoft PACS C-MOVE SCP: on a Failure status it sends a Type 1C
/// Identifier (Failed SOP Instance UID List, PS3.7 Table 9.3-5) as its own P-DATA-TF, separate
/// from the command PDU - unlike `find_scu`'s per-match Identifier, which `dimse_send_command`'s
/// callers combine into a single `Pdu::PData`. Before the `command_has_dataset` drain in
/// `move_scu`, that stray PDU was still on the wire when `release()` next called `receive()`
/// expecting `Pdu::ReleaseRP`, and dicom-ul rejected it as "unexpected response from peer".
#[test]
fn move_scu_drains_failed_sop_instance_uid_list_sent_as_separate_pdu() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
    let (scp_handle, addr) = spawn_mock_scp(abstract_syntax, move |association| {
        let (pc_id, request, _identifier_bytes) = dimse_recv_command_with_data(association);
        let message_id = dimse_message_id(&request);

        // Final response: status=Failure (0xA702, "refused: out of resources - unable to
        // perform sub-operations"), CommandDataSetType != 0x0101 so an Identifier follows.
        let final_response = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8021])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0001])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0xA702])),
            DataElement::new(tags::NUMBER_OF_REMAINING_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
            DataElement::new(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
            DataElement::new(tags::NUMBER_OF_FAILED_SUBOPERATIONS, VR::US, dicom_value!(U16, [1])),
            DataElement::new(tags::NUMBER_OF_WARNING_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
        ]);
        dimse_send_command(association, pc_id, &final_response);

        // The Identifier arrives as its own P-DATA-TF, not combined with the command above -
        // this is the shape that tripped up release() before the fix.
        let identifier = InMemDicomObject::from_element_iter([DataElement::new(
            tags::FAILED_SOP_INSTANCE_UID_LIST,
            VR::UI,
            dicom_value!(Str, "1.2.3.4.5.6"),
        )]);
        let mut identifier_bytes = Vec::new();
        identifier.write_dataset_with_ts(&mut identifier_bytes, dimse_implicit_vr_le()).unwrap();
        association
            .send(&dicom_ul::pdu::Pdu::PData {
                data: vec![dicom_ul::pdu::PDataValue {
                    presentation_context_id: pc_id,
                    value_type: dicom_ul::pdu::PDataValueType::Data,
                    is_last: true,
                    data: identifier_bytes,
                }],
            })
            .unwrap();
    });

    let result = move_scu(
        &addr.to_string(),
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
            stale_data_path: None,
            stale_data_timeout: None,
            on_log: None,
            cancel: None,
        },
    )
    .unwrap();

    assert_eq!(result.status, 0xA702);
    assert_eq!(result.failed, 1);
    scp_handle.join().unwrap();
}

/// Regression test for a bug found while investigating why a customer's C-MOVE association
/// failures were showing up with no log trail at all: every `*_scu` function ended with a bare
/// `assoc.release()?`, so a peer that sends anything other than `Pdu::ReleaseRP` in response to
/// our A-RELEASE-RQ (e.g. a stray PDU, mirroring the same "extra PDU where a clean handshake step
/// was expected" shape as the RamSoft case above, just at release time instead of mid-response)
/// used to fail *silently* - no "aborting association" log line, unlike every other error path in
/// this file - AND used to turn an already-successful move into a reported failure, since the
/// bare `?` discarded the already-known-good terminal result. Neither is right: the terminal
/// C-MOVE-RSP already answered whether the retrieve succeeded (status Success here); a peer being
/// sloppy about the *transport-level* teardown afterward shouldn't retroactively fail the move and
/// send a queue-backed caller off re-driving a retrieve the source already completed. So this must
/// still return `Ok` with the real terminal result, not an `Err` - the release failure is only
/// ever observable via the log, which isn't asserted here (no test plumbing captures `on_log`
/// output yet), just the return value.
#[test]
fn move_scu_still_returns_the_terminal_result_when_the_peer_does_not_send_release_rp() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(abstract_syntax);

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let (pc_id, request, _identifier_bytes) = dimse_recv_command_with_data(&mut association);
        let message_id = dimse_message_id(&request);

        // A normal terminal Success response, no trailing dataset - nothing unusual about the
        // move itself, only about what happens next.
        let final_response = InMemDicomObject::command_from_element_iter([
            DataElement::new(tags::AFFECTED_SOP_CLASS_UID, VR::UI, dicom_value!(Str, abstract_syntax)),
            DataElement::new(tags::COMMAND_FIELD, VR::US, dicom_value!(U16, [0x8021])),
            DataElement::new(tags::MESSAGE_ID_BEING_RESPONDED_TO, VR::US, dicom_value!(U16, [message_id])),
            DataElement::new(tags::COMMAND_DATA_SET_TYPE, VR::US, dicom_value!(U16, [0x0101])),
            DataElement::new(tags::STATUS, VR::US, dicom_value!(U16, [0x0000])),
            DataElement::new(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS, VR::US, dicom_value!(U16, [1])),
            DataElement::new(tags::NUMBER_OF_FAILED_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
            DataElement::new(tags::NUMBER_OF_WARNING_SUBOPERATIONS, VR::US, dicom_value!(U16, [0])),
        ]);
        dimse_send_command(&mut association, pc_id, &final_response);

        // The client now sends A-RELEASE-RQ expecting A-RELEASE-RP back - instead, send a stray
        // P-DATA-TF, the same "unexpected PDU where the handshake expected something specific"
        // shape dicom-ul's release() has no tolerance for.
        let pdu = association.receive().unwrap();
        assert_eq!(pdu, dicom_ul::pdu::Pdu::ReleaseRQ);
        let _ = association.send(&dicom_ul::pdu::Pdu::PData {
            data: vec![dicom_ul::pdu::PDataValue {
                presentation_context_id: pc_id,
                value_type: dicom_ul::pdu::PDataValueType::Data,
                is_last: true,
                data: vec![0u8; 4],
            }],
        });
    });

    let result = move_scu(
        &addr.to_string(),
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: Some(std::time::Duration::from_secs(5)),
            stale_data_path: None,
            stale_data_timeout: None,
            on_log: None,
            cancel: None,
        },
    );

    let result = result.expect("a release-phase failure must not turn an already-successful move into an Err");
    assert_eq!(result.status, 0x0000);
    assert_eq!(result.completed, 1);
    scp_handle.join().unwrap();
}

/// Regression test for the bug this whole absolute-timeout mechanism exists to fix: a peer that
/// keeps an association alive by responding with periodic pending statuses, but never actually
/// reaches a terminal status, used to hang `move_scu` forever - each individual read had its own
/// per-syscall timeout, but a fresh response before it elapsed reset that window indefinitely.
/// `options.timeout` is now an absolute deadline (via `poll_bounded`), so this must return
/// `AbsoluteTimeout` once it elapses, not hang past it.
#[test]
fn move_scu_aborts_on_absolute_timeout_despite_continued_pending_responses() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(abstract_syntax);

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let (pc_id, request, _identifier_bytes) = dimse_recv_command_with_data(&mut association);
        let message_id = dimse_message_id(&request);

        // Keeps sending "still working" pending responses well past the client's configured
        // absolute timeout - real progress never happens, but the association never goes quiet
        // either. Ignores send errors past that point: once the client's deadline trips and it
        // aborts, further sends here fail (broken pipe) - expected, not a test failure.
        for _ in 0..20 {
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
            if association.send(&dicom_ul::pdu::Pdu::PData {
                data: vec![dicom_ul::pdu::PDataValue {
                    presentation_context_id: pc_id,
                    value_type: dicom_ul::pdu::PDataValueType::Command,
                    is_last: true,
                    data: {
                        let mut buf = Vec::new();
                        pending.write_dataset_with_ts(&mut buf, dimse_implicit_vr_le()).unwrap();
                        buf
                    },
                }],
            }).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    });

    let result = move_scu(
        &addr.to_string(),
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: Some(std::time::Duration::from_millis(200)),
            stale_data_path: None,
            stale_data_timeout: None,
            on_log: None,
            cancel: None,
        },
    );

    assert!(matches!(result, Err(DimseError::AbsoluteTimeout)), "expected AbsoluteTimeout, got {result:?}");
    scp_handle.join().unwrap();
}

/// A retrieve's cache directory going quiet is a faster, more specific signal than the absolute
/// timeout that a source PACS has stopped making real progress - even if it's still nominally
/// "alive" at the protocol level (periodic pending responses, as here). No absolute `timeout` is
/// set, so only `stale_data_timeout` can end this call; it must fire well before the test would
/// otherwise hang.
#[test]
fn move_scu_aborts_when_stale_data_path_receives_no_new_files() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(abstract_syntax);

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let (pc_id, request, _identifier_bytes) = dimse_recv_command_with_data(&mut association);
        let message_id = dimse_message_id(&request);

        for _ in 0..20 {
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
            if association.send(&dicom_ul::pdu::Pdu::PData {
                data: vec![dicom_ul::pdu::PDataValue {
                    presentation_context_id: pc_id,
                    value_type: dicom_ul::pdu::PDataValueType::Command,
                    is_last: true,
                    data: {
                        let mut buf = Vec::new();
                        pending.write_dataset_with_ts(&mut buf, dimse_implicit_vr_le()).unwrap();
                        buf
                    },
                }],
            }).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    });

    // Deliberately never written to - this is what makes the connection "stale" despite the
    // peer's continued pending responses.
    let watch_dir = std::env::temp_dir().join(format!(
        "dcmnorm-stale-data-test-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&watch_dir).unwrap();

    let result = move_scu(
        &addr.to_string(),
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
            stale_data_path: Some(watch_dir.clone()),
            stale_data_timeout: Some(std::time::Duration::from_millis(200)),
            on_log: None,
            cancel: None,
        },
    );

    match &result {
        Err(DimseError::StaleDataConnection { path }) => assert_eq!(path, &watch_dir),
        other => panic!("expected StaleDataConnection, got {other:?}"),
    }
    scp_handle.join().unwrap();
    fs::remove_dir_all(&watch_dir).ok();
}

/// A source PACS (e.g. EXA) that pushes the study over its own separate C-STORE association but
/// never sends a terminal C-MOVE-RSP nor releases this one used to only ever end via
/// `options.timeout` - correct, but slow, and it reports the move as failed even though the
/// study had already fully arrived. `MoveScuOptions::cancel` lets an external caller (the API's
/// chain-rules "cancel-move" RPC, once the matching insert task confirms the study landed) end
/// the wait itself; asserts this produces a successful, `cancelled: true` result carrying the
/// last pending response's sub-operation counts, not an error - and that it doesn't depend on
/// `options.timeout` elapsing at all (`timeout: None` here).
#[test]
fn move_scu_returns_cancelled_result_when_signalled_mid_wait() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
    let cancel = CancelSignal::new();
    let cancel_setter = cancel.clone();

    let (scp_handle, addr) = spawn_mock_scp(abstract_syntax, move |association| {
        let (pc_id, request, _identifier_bytes) = dimse_recv_command_with_data(association);
        let message_id = dimse_message_id(&request);

        let send_pending = |association: &mut dicom_ul::ServerAssociation<std::net::TcpStream>| {
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
            association
                .send(&dicom_ul::pdu::Pdu::PData {
                    data: vec![dicom_ul::pdu::PDataValue {
                        presentation_context_id: pc_id,
                        value_type: dicom_ul::pdu::PDataValueType::Command,
                        is_last: true,
                        data: {
                            let mut buf = Vec::new();
                            pending.write_dataset_with_ts(&mut buf, dimse_implicit_vr_le()).unwrap();
                            buf
                        },
                    }],
                })
                .unwrap();
        };

        // Sends exactly two pending responses, then stops (never reaches, or releases at, a
        // terminal one) - only `cancel`, set once the client is known to have already seen the
        // first response, ends this. Stopping here (rather than looping indefinitely) matters:
        // any further sends racing the client's own release handshake below would corrupt it
        // with an unexpected PDU.
        send_pending(association);
        std::thread::sleep(std::time::Duration::from_millis(40));
        // Set before the second send (not after): the client's second read attempt (for this
        // response) starts as soon as it's done processing the first, likely before this sleep
        // elapses, so the flag must already be true by the time that second response arrives -
        // it's caught on the client's *third* read attempt instead.
        cancel_setter.request(CancelMode::Release);
        send_pending(association);
    });

    let result = move_scu(
        &addr.to_string(),
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
            stale_data_path: None,
            stale_data_timeout: None,
            on_log: None,
            cancel: Some(cancel),
        },
    )
    .unwrap();

    assert!(result.cancelled, "expected a cancelled result, got {result:?}");
    assert_eq!(result.cancelled_via, Some(CancelMode::Release));
    assert_eq!(result.status, 0);
    assert_eq!(result.remaining, 1);
    assert_eq!(result.completed, 0);
    scp_handle.join().unwrap();
}

/// `CancelMode::Abort` sends A-ABORT immediately instead of a graceful A-RELEASE handshake -
/// unlike the `Release` case above (which the mock SCP acks with a real `ReleaseRP`), the mock
/// SCP here never responds to anything at all, and the assertion is that the client's call still
/// returns promptly with `cancelled_via: Some(Abort)`, and that what actually arrived on the wire
/// was a real `Pdu::AbortRQ` - not a `ReleaseRQ` the peer just happened to not answer.
#[test]
fn move_scu_hard_aborts_immediately_when_cancel_mode_is_abort() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(abstract_syntax);

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let _ = dimse_recv_command_with_data(&mut association);
        // Never sends any C-MOVE-RSP at all - proves the abort doesn't wait on the peer for
        // anything, unlike the graceful `Release` case.
        let pdu = association.receive().unwrap();
        assert!(matches!(pdu, dicom_ul::pdu::Pdu::AbortRQ { .. }), "expected AbortRQ, got {pdu:?}");
    });

    let cancel = CancelSignal::new();
    let cancel_setter = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel_setter.request(CancelMode::Abort);
    });

    let started = std::time::Instant::now();
    let result = move_scu(
        &addr.to_string(),
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
            stale_data_path: None,
            stale_data_timeout: None,
            on_log: None,
            cancel: Some(cancel),
        },
    )
    .unwrap();

    assert!(result.cancelled, "expected a cancelled result, got {result:?}");
    assert_eq!(result.cancelled_via, Some(CancelMode::Abort));
    // Cancel is only actually observed at the next `poll_bounded` tick (up to one
    // `POLL_INTERVAL` after it's set, not immediately).
    assert!(started.elapsed() < std::time::Duration::from_millis(900), "abort took too long to be noticed: {:?}", started.elapsed());
    scp_handle.join().unwrap();
}

/// `find_scu` shares `move_scu`'s absolute-timeout mechanism (`poll_bounded`) - a peer that
/// accepts the association but never responds to the C-FIND-RQ at all must still be bounded by
/// `options.timeout`, not left to hang until the caller's own outer timeout (e.g. a queue
/// visibility timeout) eventually reclaims the task.
#[test]
fn find_scu_aborts_on_absolute_timeout_when_peer_never_responds() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(abstract_syntax);

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let _ = dimse_recv_command_with_data(&mut association);
        // Silence: never sends a C-FIND-RSP. Held open long enough to outlast the client's
        // configured timeout, proving the client gives up on its own rather than waiting on us.
        std::thread::sleep(std::time::Duration::from_millis(500));
    });

    let mut query = std::collections::HashMap::new();
    query.insert("PatientID".to_owned(), "MRN123".to_owned());
    let result = find_scu(
        &addr.to_string(),
        &query,
        FindScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: Some(std::time::Duration::from_millis(200)),
            on_log: None,
            cancel: None,
        },
    );

    assert!(matches!(result, Err(DimseError::AbsoluteTimeout)), "expected AbsoluteTimeout, got {result:?}");
    scp_handle.join().unwrap();
}

/// `find_scu` gained `cancel` support alongside `move_scu`'s - a peer that accepts the
/// association but never responds to the C-FIND-RQ must not be able to hang the call once a
/// caller signals cancellation, even with no `timeout` configured at all.
#[test]
fn find_scu_returns_cancelled_error_when_signalled_mid_wait() {
    let abstract_syntax = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(abstract_syntax);

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let _ = dimse_recv_command_with_data(&mut association);
        // Silence: never sends a C-FIND-RSP. Held open comfortably longer than one
        // `poll_bounded` tick past the cancel below (see the `echo_scu` cancel test).
        std::thread::sleep(std::time::Duration::from_secs(2));
    });

    let cancel = CancelSignal::new();
    let cancel_setter = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel_setter.request(CancelMode::Release);
    });

    let mut query = std::collections::HashMap::new();
    query.insert("PatientID".to_owned(), "MRN123".to_owned());
    let started = std::time::Instant::now();
    let result = find_scu(
        &addr.to_string(),
        &query,
        FindScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            timeout: None,
            on_log: None,
            cancel: Some(cancel),
        },
    );

    assert!(matches!(result, Err(DimseError::Cancelled(CancelMode::Release))), "expected Cancelled, got {result:?}");
    assert!(started.elapsed() < std::time::Duration::from_millis(900), "cancel took too long to be noticed: {:?}", started.elapsed());
    scp_handle.join().unwrap();
}

/// `store_scu` gained an absolute `timeout` (it had none at all before) alongside the same
/// `poll_bounded` mechanism as `move_scu`/`find_scu` - a peer that accepts the association but
/// never responds to the C-STORE-RQ must not be able to hang the call indefinitely.
#[test]
fn store_scu_aborts_on_absolute_timeout_when_peer_never_responds() {
    let source = fixture_path("sr.dcm");
    let object = read_dicom_file(&source).unwrap();
    let sop_class_uid = object.meta().media_storage_sop_class_uid.trim_end_matches(['\0', ' ']).to_owned();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(sop_class_uid.clone());

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let _ = dimse_recv_command_with_data(&mut association);
        // Silence: never sends a C-STORE-RSP.
        std::thread::sleep(std::time::Duration::from_millis(500));
    });

    let result = store_scu(
        &addr.to_string(),
        &[source],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            never_transcode: true,
            timeout: Some(std::time::Duration::from_millis(200)),
            on_log: None,
            cancel: None,
        },
    );

    assert!(matches!(result, Err(DimseError::AbsoluteTimeout)), "expected AbsoluteTimeout, got {result:?}");
    scp_handle.join().unwrap();
}

/// `store_scu` gained `cancel` support alongside `move_scu`'s - a peer that accepts the
/// association but never responds to the C-STORE-RQ must not be able to hang the call once a
/// caller signals cancellation, even with no `timeout` configured at all.
#[test]
fn store_scu_returns_cancelled_error_when_signalled_mid_wait() {
    let source = fixture_path("sr.dcm");
    let object = read_dicom_file(&source).unwrap();
    let sop_class_uid = object.meta().media_storage_sop_class_uid.trim_end_matches(['\0', ' ']).to_owned();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let scp = dicom_ul::association::server::ServerAssociationOptions::new()
        .ae_title("MOCK-SCP")
        .with_abstract_syntax(sop_class_uid.clone());

    let scp_handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut association = scp.establish(stream).unwrap();
        let _ = dimse_recv_command_with_data(&mut association);
        // Silence: never sends a C-STORE-RSP. Held open comfortably longer than one
        // `poll_bounded` tick past the cancel below (see the `echo_scu` cancel test).
        std::thread::sleep(std::time::Duration::from_secs(2));
    });

    let cancel = CancelSignal::new();
    let cancel_setter = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel_setter.request(CancelMode::Release);
    });

    let started = std::time::Instant::now();
    let result = store_scu(
        &addr.to_string(),
        &[source],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: Some("MOCK-SCP".to_owned()),
            max_pdu_length: 16384,
            never_transcode: true,
            timeout: None,
            on_log: None,
            cancel: Some(cancel),
        },
    );

    assert!(matches!(result, Err(DimseError::Cancelled(CancelMode::Release))), "expected Cancelled, got {result:?}");
    assert!(started.elapsed() < std::time::Duration::from_millis(900), "cancel took too long to be noticed: {:?}", started.elapsed());
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
        EchoScuOptions { calling_ae_title: "TEST-SCU".to_owned(), called_ae_title: None, timeout: None, on_log: None, cancel: None },
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
            timeout: None,
            on_log: None,
            cancel: None,
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
            on_log: None,
            cancel: None,
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
        "MOVE-DEST",
        "1.2.3.4.5",
        MoveScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: None,
            max_pdu_length: 16384,
            timeout: None,
            stale_data_path: None,
            stale_data_timeout: None,
            on_log: None,
            cancel: None,
        },
    )
    .unwrap();
    assert_eq!(move_result.status, 0);
    let move_calls = handlers.move_calls.lock().unwrap().clone();
    assert_eq!(move_calls, vec![("1.2.3.4.5".to_owned(), "MOVE-DEST".to_owned())]);

    // Association-complete fires after the C-STORE association's release completes, which can
    // race this test observing it - poll rather than assume synchronous completion.
    let completed = wait_for(|| handlers.association_complete.lock().unwrap().clone());
    assert_eq!(completed.get(&study_instance_uid).map(Vec::len), Some(1));

    scp.stop();
    fs::remove_dir_all(&cache_dir).ok();
}

struct MockLogger {
    lines: std::sync::Mutex<Vec<String>>,
}

impl DimseLogger for MockLogger {
    fn log(&self, message: String) {
        self.lines.lock().unwrap().push(message);
    }
}

/// The SCP side of this DIMSE stack used to have no `on_log` plumbing at all - association
/// accept/negotiation, per-message request/response, and release were only ever observable via
/// `*_scu`'s own logging on the SCU/client side (`establish()`/`release_and_log()` and friends).
/// Asserts `ScpOptions::on_log` now reports the same shape of detail for an inbound association,
/// mirroring the SCU side's wording closely enough that the two read as one consistent log
/// stream at `logLevel: "debug"`.
#[test]
fn scp_on_log_reports_association_negotiation_and_per_message_detail() {
    let cache_dir = std::env::temp_dir().join(format!(
        "dcmnorm-scp-onlog-test-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&cache_dir).unwrap();

    let handlers = std::sync::Arc::new(TestScpHandlers {
        find_query: std::sync::Mutex::new(None),
        find_response: Vec::new(),
        move_calls: std::sync::Mutex::new(Vec::new()),
        move_result: true,
        association_complete: std::sync::Mutex::new(None),
    });

    let logger = std::sync::Arc::new(MockLogger { lines: std::sync::Mutex::new(Vec::new()) });

    let scp = start_scp(
        0,
        cache_dir.clone(),
        handlers,
        ScpOptions { ae_title: "TEST-SCP".to_owned(), on_log: Some(logger.clone()), ..Default::default() },
    )
    .unwrap();
    let destination = format!("127.0.0.1:{}", scp.local_port());

    let status = echo_scu(
        &destination,
        EchoScuOptions { calling_ae_title: "TEST-SCU".to_owned(), called_ae_title: None, timeout: None, on_log: None, cancel: None },
    )
    .unwrap();
    assert_eq!(status, 0);

    scp.stop();
    fs::remove_dir_all(&cache_dir).ok();

    let joined = logger.lines.lock().unwrap().join("\n");
    assert!(joined.contains("accepting association from"), "missing association-accept log, got: {joined}");
    assert!(
        joined.contains("established; negotiated presentation context(s)"),
        "missing presentation-context negotiation log, got: {joined}"
    );
    assert!(joined.contains("received C-ECHO-RQ"), "missing per-message request log, got: {joined}");
    assert!(joined.contains("sending C-ECHO-RSP: status=0x0000"), "missing per-message response log, got: {joined}");
    assert!(joined.contains("released"), "missing association-release log, got: {joined}");
}

/// Regression coverage for a production incident: a source PACS reported an in-progress C-MOVE
/// and, separately, claimed (via its own UI) to have stored an instance - but the receiving
/// application's own insert queue showed no evidence any instance was ever received.
/// Diagnosing it required correlating dimse's C-STORE-RQ logging (a StudyInstanceUID, once known)
/// against separately-visible association accept/negotiate/release lines by hand, across several
/// interleaved associations from the same peer. Asserts the StudyInstanceUID is logged as soon as
/// the dataset is parsed, and that a completed association logs a one-line summary of what it
/// actually stored - both meant to make that kind of correlation immediate instead of manual.
#[test]
fn scp_on_log_reports_study_uid_and_a_completion_summary_for_a_stored_instance() {
    let cache_dir = std::env::temp_dir().join(format!(
        "dcmnorm-scp-storelog-test-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&cache_dir).unwrap();

    let handlers = std::sync::Arc::new(TestScpHandlers {
        find_query: std::sync::Mutex::new(None),
        find_response: Vec::new(),
        move_calls: std::sync::Mutex::new(Vec::new()),
        move_result: true,
        association_complete: std::sync::Mutex::new(None),
    });

    let logger = std::sync::Arc::new(MockLogger { lines: std::sync::Mutex::new(Vec::new()) });

    let scp = start_scp(
        0,
        cache_dir.clone(),
        handlers,
        ScpOptions { ae_title: "TEST-SCP".to_owned(), on_log: Some(logger.clone()), ..Default::default() },
    )
    .unwrap();
    let destination = format!("127.0.0.1:{}", scp.local_port());

    let source = fixture_path("sr.dcm");
    let object = read_dicom_file(&source).unwrap();
    let sop_instance_uid = object.meta().media_storage_sop_instance_uid.trim_end_matches(['\0', ' ']).to_owned();
    let study_instance_uid = object.element(tags::STUDY_INSTANCE_UID).unwrap().to_str().unwrap().trim().to_owned();

    let results = store_scu(
        &destination,
        &[source],
        StoreScuOptions {
            calling_ae_title: "TEST-SCU".to_owned(),
            called_ae_title: None,
            max_pdu_length: 16384,
            never_transcode: true,
            timeout: None,
            on_log: None,
            cancel: None,
        },
    )
    .unwrap();
    assert_eq!(results[0].status, 0);

    scp.stop();
    fs::remove_dir_all(&cache_dir).ok();

    let joined = logger.lines.lock().unwrap().join("\n");
    assert!(
        joined.contains(&format!("received C-STORE-RQ dataset: study {study_instance_uid}, SOP instance {sop_instance_uid}")),
        "missing per-dataset study/SOP UID log, got: {joined}"
    );
    assert!(
        joined.contains("stored 1 instance(s) across 1 study before ending"),
        "missing association-completion summary log, got: {joined}"
    );
}

/// Companion to the test above, for the other half of the same production incident: an
/// association that connects, negotiates presentation contexts, and releases without ever
/// sending a C-STORE looks - at a glance, scanning logs - identical to one whose C-STORE logging
/// simply went missing. Asserts this case gets its own explicit line instead of relying on a
/// reader to notice the *absence* of a "received C-STORE-RQ" line.
#[test]
fn scp_on_log_reports_when_an_association_ends_with_no_c_store() {
    let cache_dir = std::env::temp_dir().join(format!(
        "dcmnorm-scp-nostore-test-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&cache_dir).unwrap();

    let handlers = std::sync::Arc::new(TestScpHandlers {
        find_query: std::sync::Mutex::new(None),
        find_response: Vec::new(),
        move_calls: std::sync::Mutex::new(Vec::new()),
        move_result: true,
        association_complete: std::sync::Mutex::new(None),
    });

    let logger = std::sync::Arc::new(MockLogger { lines: std::sync::Mutex::new(Vec::new()) });

    let scp = start_scp(
        0,
        cache_dir.clone(),
        handlers,
        ScpOptions { ae_title: "TEST-SCP".to_owned(), on_log: Some(logger.clone()), ..Default::default() },
    )
    .unwrap();
    let destination = format!("127.0.0.1:{}", scp.local_port());

    // A C-ECHO association never calls handle_store at all - exactly the "association with
    // nothing stored" shape observed in production.
    let status = echo_scu(
        &destination,
        EchoScuOptions { calling_ae_title: "TEST-SCU".to_owned(), called_ae_title: None, timeout: None, on_log: None, cancel: None },
    )
    .unwrap();
    assert_eq!(status, 0);

    scp.stop();
    fs::remove_dir_all(&cache_dir).ok();

    let joined = logger.lines.lock().unwrap().join("\n");
    assert!(
        joined.contains("ended with no C-STORE received"),
        "missing no-C-STORE association summary log, got: {joined}"
    );
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
            on_log: None,
            cancel: None,
        },
    )
    .unwrap();
    assert_eq!(matches.len(), 1);
    let match_value: serde_json::Value = serde_json::from_str(&matches[0]).unwrap();
    assert_eq!(match_value["00081030"], "CT Chest \u{2013} followup");

    scp.stop();
    fs::remove_dir_all(&cache_dir).ok();
}

/// Writes `count` synthetic slice files derived from `test/files/ct.dcm`, sharing that fixture's
/// own orientation/pixel spacing but with `ImagePositionPatient`'s Z (head-foot, third component,
/// matching the fixture's axial `ImageOrientationPatient` of `1\0\0\0\1\0`) advanced by
/// `spacing_mm` per slice - a minimal, real-decode-pipeline stand-in for an actual multi-slice CT
/// series. Files are written in REVERSE spatial order on purpose, so a passing `build_volume`
/// test actually exercises its own spatial re-sort rather than trusting list order.
fn write_synthetic_ct_series(count: usize, spacing_mm: f64) -> Vec<PathBuf> {
    let base = read_dicom_file(fixture_path("ct.dcm")).unwrap();
    let base_z = 1115.0;
    let mut paths = Vec::with_capacity(count);
    for index in (0..count).rev() {
        let mut object = base.clone();
        let z = base_z + (index as f64) * spacing_mm;
        object.put(DataElement::new(
            tags::IMAGE_POSITION_PATIENT,
            VR::DS,
            PrimitiveValue::from(format!("-151.493508\\-36.6564417\\{z}")),
        ));
        let path = temp_file_path(&format!("mpr-volume-slice-{index}"));
        write_dicom_file(&mut object, &path).unwrap();
        paths.push(path);
    }
    paths
}

#[test]
fn build_volume_sorts_slices_spatially_and_reformats_the_native_axial_plane() {
    let paths = write_synthetic_ct_series(5, 1.0);

    let volume = build_volume(&paths).unwrap();
    assert_eq!(volume.rows, 512);
    assert_eq!(volume.cols, 512);
    assert_eq!(volume.num_slices, 5);
    assert_eq!(volume.slice_zs.len(), 5);
    for window in volume.slice_zs.windows(2) {
        assert!(window[1] > window[0], "slice_zs must be spatially sorted ascending: {:?}", volume.slice_zs);
    }
    // Nominal 1.0mm spacing (matching the fixture's own SliceThickness) should round-trip.
    let observed_spacing = (volume.slice_zs[4] - volume.slice_zs[0]) / 4.0;
    assert!((observed_spacing - 1.0).abs() < 1e-6, "observed spacing {observed_spacing}");

    let params = PlaneParams {
        origin: volume.center(),
        row_dir: volume.row_vector,
        col_dir: volume.col_vector,
        output_width: 64,
        output_height: 64,
        spacing_mm: volume.min_spacing_mm(),
        window_center: None,
        window_width: None,
        interpolation: Interpolation::Trilinear,
        slab_thickness_mm: 0.0,
        slab_projection: SlabProjection::MaximumIntensity,
    };
    let output = reformat_plane(&volume, &params, RenderOutputFormat::Png, 90).unwrap();
    assert_eq!(output.width, 64);
    assert_eq!(output.height, 64);
    assert!(!output.bytes.is_empty());

    for path in &paths {
        fs::remove_file(path).ok();
    }
}

#[test]
fn build_volume_rejects_a_non_parallel_orientation_in_the_stack() {
    let paths = write_synthetic_ct_series(4, 1.0);

    // Corrupt one slice's ImageOrientationPatient so it no longer shares the stack's plane -
    // e.g. simulating a gantry-tilt-inconsistent or accidentally-mixed-series file set.
    let mut tilted = read_dicom_file(&paths[1]).unwrap();
    tilted.put(DataElement::new(
        tags::IMAGE_ORIENTATION_PATIENT,
        VR::DS,
        PrimitiveValue::from("1\\0\\0\\0\\0.7071\\0.7071"),
    ));
    write_dicom_file(&mut tilted, &paths[1]).unwrap();

    let result = build_volume(&paths);
    assert!(
        matches!(result, Err(VolumeError::InconsistentGeometry(_))),
        "expected InconsistentGeometry, got {result:?}"
    );

    for path in &paths {
        fs::remove_file(path).ok();
    }
}


#[test]
fn pack_dicom_frame_texture_rejects_a_color_instance_instead_of_misreading_it_as_grayscale() {
    // us.dcm is a real RGB (SamplesPerPixel=3) fixture - the texture-export path has no notion
    // of a multi-channel pixel layout, and previously read straight past it as if it were
    // single-channel samples, producing a badly aliased/garbled image instead of an error.
    let source = fixture_bytes(fixture_path("us.dcm"));
    let object = read_dicom_bytes(&source).unwrap();

    let result = pack_dicom_frame_texture(&object, 0, None, None, TextureCompression::None);
    assert!(
        matches!(result, Err(TextureExportError::Render(RenderError::UnsupportedSamplesPerPixel(3)))),
        "expected UnsupportedSamplesPerPixel(3), got {result:?}"
    );
}

#[test]
fn pack_dicom_frame_stack_texture_packs_multiple_sources_as_layers_in_order() {
    // Same real fixture used twice as two distinct "sources" - a stand-in for a multi-image
    // series' two instance files (each contributing frame 0) - exercises the actual
    // read_dicom_file/decode_frame_grayscale_values path this function is a thin wrapper over,
    // not just the pure pack_frame_stack_texture(&[DecodedFrame]) logic already covered in
    // texture_export.rs's own unit tests.
    let source = fixture_bytes(fixture_path("dx.dcm"));
    let object = read_dicom_bytes(&source).unwrap();
    let sources = [(&object, 0usize), (&object, 0usize)];

    let packed = pack_dicom_frame_stack_texture(&sources, None, TextureCompression::None).unwrap();
    assert_eq!(packed.meta.content_kind, ContentKind::FrameStack);
    assert_eq!(packed.meta.depth, 2);
    assert!(!packed.meta.downsampled);
    // Both layers came from the same frame, so they must be byte-identical.
    let layer_bytes = packed.payload.len() / 2;
    assert_eq!(packed.payload[..layer_bytes], packed.payload[layer_bytes..]);
}

#[test]
fn pack_dicom_frame_stack_texture_fails_closed_if_any_source_is_a_color_instance() {
    // A stack with one grayscale source and one RGB source (us.dcm, SamplesPerPixel=3) must
    // reject the WHOLE stack, not silently drop just the color frame - matching
    // pack_dicom_frame_texture's own single-frame rejection above.
    let grayscale = read_dicom_bytes(&fixture_bytes(fixture_path("dx.dcm"))).unwrap();
    let color = read_dicom_bytes(&fixture_bytes(fixture_path("us.dcm"))).unwrap();
    let sources = [(&grayscale, 0usize), (&color, 0usize)];

    let result = pack_dicom_frame_stack_texture(&sources, None, TextureCompression::None);
    assert!(
        matches!(result, Err(TextureExportError::Render(RenderError::UnsupportedSamplesPerPixel(3)))),
        "expected UnsupportedSamplesPerPixel(3), got {result:?}"
    );
}
