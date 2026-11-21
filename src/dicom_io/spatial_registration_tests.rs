//! Spatial Registration: build -> DICOM bytes -> read round trips, the standard DICOM JSON form,
//! IOD validation (conformant and broken objects), and a REG shaped like the ones treatment
//! planning systems and dual-FoR PET/CT scanners write (implicit VR, items in "wrong" order, a
//! two-matrix chain, image-only references, a deformable object). Synthetic UIDs and names only.

use dcmnorm_core::header::Length;
use dcmnorm_core::value::DataSetSequence;
use dcmnorm_core::{DataElement, PrimitiveValue, Tag, VR};
use dcmnorm_dictionary::{tags, uids};
use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject};
use serde_json::{json, Value as Json};

use super::io::{read_dicom_bytes, read_dicom_file, write_dicom_bytes};
use super::json::{read_dicom_json_full, write_dicom_json_with_options};
use super::registration::{mat_inverse, mat_mul, rigid_about, transform_point};
use super::spatial_registration::{
    build_spatial_registration, format_ds, read_spatial_registration,
    validate_spatial_registration, SpatialRegistrationError, SPATIAL_REGISTRATION_SOP_CLASS_UID,
};
use super::test_support::{remove_stack, write_synthetic_stack, SyntheticStack};
use super::types::{DicomJsonFormat, DicomJsonKeyStyle, DicomJsonWriteOptions};

const FIXED_FOR: &str = "2.25.111111111111111111111111111111111111";
const MOVING_FOR: &str = "2.25.222222222222222222222222222222222222";
const FIXED_STUDY: &str = "2.25.333333333333333333333333333333333333";
const MOVING_STUDY: &str = "2.25.444444444444444444444444444444444444";

fn matrix_of(value: &Json) -> [f64; 16] {
    let values: Vec<f64> = value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let mut m = [0.0; 16];
    m.copy_from_slice(&values);
    m
}

fn close(a: &[f64; 16], b: &[f64; 16], tolerance: f64) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tolerance)
}

fn rigid() -> [f64; 16] {
    rigid_about(
        0.031,
        -0.052,
        0.104,
        [14.25, -11.5, 32.125],
        [10.0, 20.0, 0.0],
    )
}

fn spec() -> Json {
    json!({
        "matrix": rigid().to_vec(),
        "fixed": {
            "frameOfReferenceUid": FIXED_FOR, "studyInstanceUid": FIXED_STUDY, "seriesInstanceUid": "2.25.5001",
            "instances": [
                { "sopClassUid": uids::CT_IMAGE_STORAGE, "sopInstanceUid": "2.25.5001001" },
                { "sopClassUid": uids::CT_IMAGE_STORAGE, "sopInstanceUid": "2.25.5001002" }
            ]
        },
        "moving": {
            "frameOfReferenceUid": MOVING_FOR, "studyInstanceUid": MOVING_STUDY, "seriesInstanceUid": "2.25.6001",
            "instances": [ { "sopClassUid": uids::CT_IMAGE_STORAGE, "sopInstanceUid": "2.25.6001001" } ]
        },
        "patient": { "patientName": "SYNTHETIC^PATIENT", "patientId": "SYN-REG-1", "patientBirthDate": "19700101", "patientSex": "O" },
        "study": { "studyDate": "20261101", "studyTime": "101500", "accessionNumber": "SYNACC9" },
        "contentDescription": "Rigid registration of the prior to the current",
        "comment": "NCC, quality 0.97"
    })
}

#[test]
fn format_ds_fits_sixteen_characters_and_round_trips_closely() {
    for v in [
        0.0,
        1.0,
        -1.0,
        0.999_999_999_999_9,
        1e-12,
        -123_456.789_012_345,
        std::f64::consts::PI,
        1.5e20,
        -0.000_123_456_789_123,
    ] {
        let text = format_ds(v);
        assert!(text.len() <= 16, "{v} -> {text:?}");
        let back: f64 = text.parse().unwrap();
        assert!(
            (back - v).abs() <= 1e-9 * v.abs().max(1.0) || (v.abs() < 1e-10 && back.abs() < 1e-10),
            "{v} -> {text}"
        );
    }
}

#[test]
fn a_built_registration_is_conformant_and_round_trips_through_dicom_bytes() {
    let mut object = build_spatial_registration(&spec(), None).unwrap();
    assert!(
        validate_spatial_registration(&object).is_empty(),
        "{:?}",
        validate_spatial_registration(&object)
    );
    let bytes = write_dicom_bytes(&mut object).unwrap();
    let read_back = read_dicom_bytes(&bytes).unwrap();
    assert_eq!(
        read_back
            .meta()
            .media_storage_sop_class_uid()
            .trim_end_matches('\0'),
        SPATIAL_REGISTRATION_SOP_CLASS_UID
    );
    let json = read_spatial_registration(&read_back).unwrap();

    assert_eq!(json["frameOfReferenceUid"], FIXED_FOR);
    assert_eq!(json["studyInstanceUid"], FIXED_STUDY);
    assert_eq!(json["patientId"], "SYN-REG-1");
    assert_eq!(json["contentLabel"], "REGISTRATION");
    assert_eq!(json["deformable"], false);
    assert_eq!(
        json["warnings"].as_array().unwrap().len(),
        0,
        "{}",
        json["warnings"]
    );
    let registrations = json["registrations"].as_array().unwrap();
    assert_eq!(registrations.len(), 2);
    assert_eq!(registrations[0]["frameOfReferenceUid"], FIXED_FOR);
    assert_eq!(registrations[0]["isIdentity"], true);
    assert_eq!(registrations[1]["frameOfReferenceUid"], MOVING_FOR);
    assert_eq!(registrations[1]["matrixType"], "RIGID");
    assert_eq!(registrations[1]["registrationType"]["codeValue"], "125024");
    assert_eq!(registrations[1]["comment"], "NCC, quality 0.97");
    assert_eq!(
        registrations[1]["referencedImages"][0]["sopInstanceUid"],
        "2.25.6001001"
    );
    // DS keeps at most 16 characters per value: the matrix survives to ~1e-9 relative.
    assert!(close(
        &matrix_of(&registrations[1]["matrix"]),
        &rigid(),
        1e-8
    ));
    // Common Instance Reference: the fixed series in this study, the moving one in another.
    let series = json["referencedSeries"].as_array().unwrap();
    assert_eq!(series.len(), 2);
    assert_eq!(series[0]["studyInstanceUid"], FIXED_STUDY);
    assert_eq!(series[0]["instances"].as_array().unwrap().len(), 2);
    assert_eq!(series[1]["studyInstanceUid"], MOVING_STUDY);
    assert_eq!(series[1]["seriesInstanceUid"], "2.25.6001");
}

#[test]
fn the_standard_dicom_json_form_reads_back_to_the_same_registration() {
    let object = build_spatial_registration(&spec(), None).unwrap();
    let dataset = write_dicom_json_with_options(
        &object,
        DicomJsonWriteOptions {
            format: DicomJsonFormat::Standard,
            key_style: DicomJsonKeyStyle::Hex,
            ..Default::default()
        },
    )
    .unwrap();
    let parsed: Json = serde_json::from_str(&dataset).unwrap();
    assert_eq!(parsed["00080016"]["vr"], "UI");
    assert_eq!(
        parsed["00080016"]["Value"][0],
        SPATIAL_REGISTRATION_SOP_CLASS_UID
    );
    assert_eq!(parsed["00700308"]["vr"], "SQ");
    let from_json = read_dicom_json_full(&dataset).unwrap();
    let a = read_spatial_registration(&object).unwrap();
    let b = read_spatial_registration(&from_json).unwrap();
    assert_eq!(a["registrations"], b["registrations"]);
    assert_eq!(a["referencedSeries"], b["referencedSeries"]);
}

#[test]
fn patient_and_study_are_copied_from_a_fixed_instance_and_overrides_win() {
    let stack = SyntheticStack {
        rows: 4,
        cols: 4,
        row_vector: [1.0, 0.0, 0.0],
        col_vector: [0.0, 1.0, 0.0],
        pixel_spacing: [1.0, 1.0],
        positions: vec![[0.0, 0.0, 0.0]],
        modality: "CT",
        frame_of_reference_uid: FIXED_FOR.into(),
        study_instance_uid: FIXED_STUDY.into(),
        series_instance_uid: "2.25.5001".into(),
    };
    let paths = write_synthetic_stack(&stack, |_| 0.0);
    let source = read_dicom_file(&paths[0]).unwrap();
    remove_stack(&paths);

    let mut s = spec();
    s.as_object_mut().unwrap().remove("patient");
    s["study"] = json!({ "accessionNumber": "OVERRIDE1" });
    let object = build_spatial_registration(&s, Some(&source)).unwrap();
    let text = |tag: Tag| {
        object
            .get(tag)
            .and_then(|e| e.to_str().ok())
            .map(|v| v.trim().to_owned())
    };
    assert_eq!(
        text(tags::PATIENT_NAME).as_deref(),
        Some("SYNTHETIC^PHANTOM")
    );
    assert_eq!(text(tags::PATIENT_ID).as_deref(), Some("SYN-0001"));
    assert_eq!(text(tags::STUDY_DATE).as_deref(), Some("20260101"));
    assert_eq!(text(tags::ACCESSION_NUMBER).as_deref(), Some("OVERRIDE1"));
    assert!(validate_spatial_registration(&object).is_empty());

    // A file from another study is refused.
    let mut other = spec();
    other["fixed"]["studyInstanceUid"] = json!("2.25.999");
    assert!(matches!(
        build_spatial_registration(&other, Some(&source)),
        Err(SpatialRegistrationError::InvalidSpec(_))
    ));
}

#[test]
fn invalid_specs_are_rejected() {
    type Mutation = Box<dyn Fn(&mut Json)>;
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "same frame",
            Box::new(|s: &mut Json| s["moving"]["frameOfReferenceUid"] = json!(FIXED_FOR)),
        ),
        (
            "15 values",
            Box::new(|s: &mut Json| {
                s["matrix"]
                    .as_array_mut()
                    .unwrap()
                    .pop()
                    .map(|_| ())
                    .unwrap()
            }),
        ),
        (
            "not rigid",
            Box::new(|s: &mut Json| s["matrix"][0] = json!(1.2)),
        ),
        (
            "bad bottom row",
            Box::new(|s: &mut Json| s["matrix"][12] = json!(0.5)),
        ),
        (
            "bad uid",
            Box::new(|s: &mut Json| s["fixed"]["seriesInstanceUid"] = json!("1.2.03")),
        ),
        (
            "missing moving",
            Box::new(|s: &mut Json| {
                s.as_object_mut().unwrap().remove("moving");
            }),
        ),
        (
            "bad label",
            Box::new(|s: &mut Json| s["contentLabel"] = json!("lower case")),
        ),
        (
            "bad type",
            Box::new(|s: &mut Json| s["registrationType"] = json!("guess")),
        ),
    ];
    for (name, mutate) in cases {
        let mut s = spec();
        mutate(&mut s);
        assert!(
            matches!(
                build_spatial_registration(&s, None),
                Err(SpatialRegistrationError::InvalidSpec(_))
            ),
            "{name} should be rejected"
        );
    }
    // A scaled matrix is fine as RIGID_SCALE, and a sheared one as AFFINE.
    let mut scaled = spec();
    for i in [0, 5, 10] {
        scaled["matrix"][i] = json!(scaled["matrix"][i].as_f64().unwrap() * 1.1);
    }
    scaled["matrix"][1] = json!(0.0);
    scaled["matrix"][2] = json!(0.0);
    scaled["matrix"][4] = json!(0.0);
    scaled["matrix"][6] = json!(0.0);
    scaled["matrix"][8] = json!(0.0);
    scaled["matrix"][9] = json!(0.0);
    scaled["matrixType"] = json!("RIGID_SCALE");
    assert!(build_spatial_registration(&scaled, None).is_ok());
    let mut affine = spec();
    affine["matrix"][1] = json!(0.3);
    affine["matrixType"] = json!("AFFINE");
    assert!(build_spatial_registration(&affine, None).is_ok());
}

#[test]
fn validation_reports_broken_objects() {
    let mut object = build_spatial_registration(&spec(), None).unwrap();
    object.put(DataElement::new(
        tags::MODALITY,
        VR::CS,
        PrimitiveValue::from("OT"),
    ));
    object.remove_element(tags::CONTENT_LABEL);
    object.remove_element(tags::MANUFACTURER);
    object.remove_element(tags::STUDIES_CONTAINING_OTHER_REFERENCED_INSTANCES_SEQUENCE);
    let problems = validate_spatial_registration(&object);
    let joined = problems.join("\n");
    assert!(joined.contains("requires REG"), "{joined}");
    assert!(joined.contains("ContentLabel"), "{joined}");
    assert!(joined.contains("Manufacturer"), "{joined}");
    assert!(
        joined.contains("not listed in the Common Instance Reference"),
        "{joined}"
    );
    // read still works and reports them.
    let json = read_spatial_registration(&object).unwrap();
    assert!(json["warnings"].as_array().unwrap().len() >= 4);
}

#[test]
fn a_non_registration_is_refused() {
    let object = read_dicom_file(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files/ct.dcm"),
    )
    .unwrap();
    assert!(matches!(
        read_spatial_registration(&object),
        Err(SpatialRegistrationError::NotARegistration(_))
    ));
}

fn sq(tag: Tag, items: Vec<InMemDicomObject>) -> DataElement<InMemDicomObject> {
    DataElement::new(tag, VR::SQ, DataSetSequence::new(items, Length::UNDEFINED))
}

fn st(tag: Tag, vr: VR, value: &str) -> DataElement<InMemDicomObject> {
    DataElement::new(tag, vr, PrimitiveValue::from(value))
}

fn item(elements: Vec<DataElement<InMemDicomObject>>) -> InMemDicomObject {
    InMemDicomObject::from_element_iter(elements)
}

fn ds_text(m: &[f64; 16]) -> String {
    // Vendors write DS in all sorts of ways: exponent form, padding, plus signs.
    m.iter()
        .map(|v| format!("{v:+.7e}"))
        .collect::<Vec<_>>()
        .join("\\")
}

/// A REG the way third-party systems write them: Implicit VR Little Endian, the moving frame's
/// item FIRST, a two-matrix chain (a table correction, then the registration) as RIGID_SCALE,
/// a third item with only image references and no Frame of Reference, exponent-form DS, and a
/// Used Fiducials Sequence nobody here reads.
#[test]
fn a_third_party_shaped_registration_is_read_correctly() {
    let table_correction = rigid_about(0.0, 0.0, 0.0, [0.0, -42.0, 0.0], [0.0; 3]);
    let mut registration = rigid();
    for i in [0, 1, 2, 4, 5, 6, 8, 9, 10] {
        registration[i] *= 1.02; // a slight uniform scale: RIGID_SCALE
    }
    let meta = FileMetaTableBuilder::new()
        .media_storage_sop_class_uid(SPATIAL_REGISTRATION_SOP_CLASS_UID)
        .media_storage_sop_instance_uid("2.25.7777")
        .transfer_syntax(uids::IMPLICIT_VR_LITTLE_ENDIAN)
        .build()
        .unwrap();
    let mut object = DefaultDicomObject::new_empty_with_meta(meta);
    let matrix_item = |m: &[f64; 16], kind: &str| {
        item(vec![
            st(
                tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX,
                VR::DS,
                &ds_text(m),
            ),
            st(
                tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX_TYPE,
                VR::CS,
                kind,
            ),
        ])
    };
    let identity = rigid_about(0.0, 0.0, 0.0, [0.0; 3], [0.0; 3]);
    for element in [
        st(
            tags::SOP_CLASS_UID,
            VR::UI,
            SPATIAL_REGISTRATION_SOP_CLASS_UID,
        ),
        st(tags::SOP_INSTANCE_UID, VR::UI, "2.25.7777"),
        st(tags::STUDY_INSTANCE_UID, VR::UI, FIXED_STUDY),
        st(tags::SERIES_INSTANCE_UID, VR::UI, "2.25.7778"),
        st(tags::MODALITY, VR::CS, "REG"),
        st(tags::FRAME_OF_REFERENCE_UID, VR::UI, FIXED_FOR),
        st(tags::PATIENT_NAME, VR::PN, "SYNTHETIC^TPS"),
        st(tags::PATIENT_ID, VR::LO, "SYN-TPS-1"),
        st(tags::CONTENT_LABEL, VR::CS, "TPS_REG"),
        st(tags::CONTENT_DATE, VR::DA, "20260315"),
        st(tags::CONTENT_TIME, VR::TM, "093000.123"),
        st(tags::INSTANCE_NUMBER, VR::IS, "1"),
    ] {
        object.put(element);
    }
    object.put(sq(
        tags::REGISTRATION_SEQUENCE,
        vec![
            item(vec![
                st(tags::FRAME_OF_REFERENCE_UID, VR::UI, MOVING_FOR),
                sq(
                    tags::MATRIX_REGISTRATION_SEQUENCE,
                    vec![item(vec![
                        sq(
                            tags::REGISTRATION_TYPE_CODE_SEQUENCE,
                            vec![item(vec![
                                st(tags::CODE_VALUE, VR::SH, "125022"),
                                st(tags::CODING_SCHEME_DESIGNATOR, VR::SH, "DCM"),
                                st(tags::CODE_MEANING, VR::LO, "Fiducial Alignment"),
                            ])],
                        ),
                        sq(
                            tags::MATRIX_SEQUENCE,
                            vec![
                                matrix_item(&table_correction, "RIGID"),
                                matrix_item(&registration, "RIGID_SCALE"),
                            ],
                        ),
                    ])],
                ),
                sq(
                    tags::USED_FIDUCIALS_SEQUENCE,
                    vec![item(vec![st(tags::FIDUCIAL_UID, VR::UI, "2.25.8888")])],
                ),
            ]),
            item(vec![
                st(tags::FRAME_OF_REFERENCE_UID, VR::UI, FIXED_FOR),
                sq(
                    tags::MATRIX_REGISTRATION_SEQUENCE,
                    vec![item(vec![
                        sq(tags::REGISTRATION_TYPE_CODE_SEQUENCE, vec![]),
                        sq(tags::MATRIX_SEQUENCE, vec![matrix_item(&identity, "RIGID")]),
                    ])],
                ),
            ]),
            item(vec![
                sq(
                    tags::REFERENCED_IMAGE_SEQUENCE,
                    vec![item(vec![
                        st(
                            tags::REFERENCED_SOP_CLASS_UID,
                            VR::UI,
                            uids::MR_IMAGE_STORAGE,
                        ),
                        st(tags::REFERENCED_SOP_INSTANCE_UID, VR::UI, "2.25.9001"),
                    ])],
                ),
                sq(
                    tags::MATRIX_REGISTRATION_SEQUENCE,
                    vec![item(vec![
                        sq(tags::REGISTRATION_TYPE_CODE_SEQUENCE, vec![]),
                        sq(
                            tags::MATRIX_SEQUENCE,
                            vec![matrix_item(&table_correction, "RIGID")],
                        ),
                    ])],
                ),
            ]),
        ],
    ));
    let mut object_for_bytes = object.clone();
    let bytes = write_dicom_bytes(&mut object_for_bytes).unwrap();
    let read_back = read_dicom_bytes(&bytes).unwrap();
    let json = read_spatial_registration(&read_back).unwrap();

    let registrations = json["registrations"].as_array().unwrap();
    assert_eq!(registrations.len(), 3);
    assert_eq!(registrations[0]["frameOfReferenceUid"], MOVING_FOR);
    assert_eq!(registrations[0]["matrixType"], "RIGID_SCALE");
    assert_eq!(registrations[0]["matrices"].as_array().unwrap().len(), 2);
    assert_eq!(registrations[0]["registrationType"]["codeValue"], "125022");
    // Applied in order: the table correction first, then the registration.
    let expected = mat_mul(&registration, &table_correction);
    assert!(close(
        &matrix_of(&registrations[0]["matrix"]),
        &expected,
        1e-5
    ));
    let p = [12.0, -30.0, 44.0];
    let mapped = transform_point(&matrix_of(&registrations[0]["matrix"]), p);
    let by_hand = transform_point(&registration, transform_point(&table_correction, p));
    assert!((0..3).all(|a| (mapped[a] - by_hand[a]).abs() < 1e-4));
    assert_eq!(registrations[1]["isIdentity"], true);
    assert!(registrations[2]["frameOfReferenceUid"].is_null());
    assert_eq!(
        registrations[2]["referencedImages"][0]["sopInstanceUid"],
        "2.25.9001"
    );
    // This sample leaves out Type 2 attributes and the Common Instance Reference: reported, not fatal.
    let warnings: Vec<String> = json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_owned())
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains("Manufacturer")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Common Instance Reference")),
        "{warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w.contains("RIGID_SCALE matrix")),
        "{warnings:?}"
    );
}

#[test]
fn a_deformable_registration_yields_its_rigid_parts() {
    let pre = rigid();
    let meta = FileMetaTableBuilder::new()
        .media_storage_sop_class_uid(uids::DEFORMABLE_SPATIAL_REGISTRATION_STORAGE)
        .media_storage_sop_instance_uid("2.25.4242")
        .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .build()
        .unwrap();
    let mut object = DefaultDicomObject::new_empty_with_meta(meta);
    object.put(st(
        tags::SOP_CLASS_UID,
        VR::UI,
        uids::DEFORMABLE_SPATIAL_REGISTRATION_STORAGE,
    ));
    object.put(st(tags::SOP_INSTANCE_UID, VR::UI, "2.25.4242"));
    object.put(st(tags::FRAME_OF_REFERENCE_UID, VR::UI, FIXED_FOR));
    object.put(sq(
        tags::DEFORMABLE_REGISTRATION_SEQUENCE,
        vec![item(vec![
            st(tags::SOURCE_FRAME_OF_REFERENCE_UID, VR::UI, MOVING_FOR),
            sq(
                tags::PRE_DEFORMATION_MATRIX_REGISTRATION_SEQUENCE,
                vec![item(vec![
                    st(
                        tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX,
                        VR::DS,
                        &ds_text(&pre),
                    ),
                    st(
                        tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX_TYPE,
                        VR::CS,
                        "RIGID",
                    ),
                ])],
            ),
            sq(
                tags::DEFORMABLE_REGISTRATION_GRID_SEQUENCE,
                vec![item(vec![])],
            ),
        ])],
    ));
    let json = read_spatial_registration(&object).unwrap();
    assert_eq!(json["deformable"], true);
    let registration = &json["registrations"][0];
    assert_eq!(registration["frameOfReferenceUid"], MOVING_FOR);
    assert_eq!(registration["deformationIgnored"], true);
    assert!(close(&matrix_of(&registration["matrix"]), &pre, 1e-5));
}

/// End to end: a registration result, written as a REG, read back, maps moving points onto the
/// same fixed points.
#[test]
fn a_registration_result_written_as_reg_maps_points_identically() {
    let result_matrix = rigid();
    let mut s = spec();
    s["matrix"] = json!(result_matrix.to_vec());
    let mut object = build_spatial_registration(&s, None).unwrap();
    let read_back = read_dicom_bytes(write_dicom_bytes(&mut object).unwrap()).unwrap();
    let json = read_spatial_registration(&read_back).unwrap();
    let stored = matrix_of(&json["registrations"][1]["matrix"]);
    for p in [
        [0.0, 0.0, 0.0],
        [150.0, -120.0, 300.0],
        [-200.0, 90.0, -400.0],
    ] {
        let a = transform_point(&result_matrix, p);
        let b = transform_point(&stored, p);
        assert!(
            (0..3).all(|axis| (a[axis] - b[axis]).abs() < 1e-5),
            "{a:?} vs {b:?}"
        );
    }
    assert!(mat_inverse(&stored).is_some());
}
