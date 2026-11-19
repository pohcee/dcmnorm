//! DICOM Spatial Registration objects (PS3.3 A.39, SOP Class `1.2.840.10008.5.1.4.1.1.66.1`):
//! read one into JSON, build a new one from a JSON description, and check one against the IOD.
//!
//! # Matrix convention
//!
//! Every matrix here is 16 numbers, row-major, exactly as Frame of Reference Transformation
//! Matrix (3006,00C6) stores them. A Registration Sequence item's matrix maps a point in THAT
//! item's Frame of Reference into the REG instance's own Frame of Reference (C.20.2.1.1:
//! `[A] = M [B]`, B the item's RCS, A the registered RCS). When a Matrix Sequence has several
//! items they apply in order, so the composed matrix is `M_n ... M_2 M_1`.
//!
//! A registration from dcmnorm's `register_rigid` (moving -> fixed patient) is therefore written
//! as: the REG's own Frame of Reference is the FIXED one, item 1 is the fixed Frame of Reference
//! with the identity, item 2 is the MOVING Frame of Reference with the moving -> fixed matrix.
//!
//! # JSON form (`read_spatial_registration`, and what `build_spatial_registration` returns)
//!
//! ```json
//! { "sopClassUid", "sopInstanceUid", "seriesInstanceUid", "studyInstanceUid",
//!   "frameOfReferenceUid", "patientId", "patientName", "contentLabel", "contentDescription",
//!   "contentDate", "contentTime", "deformable": false,
//!   "registrations": [ { "frameOfReferenceUid", "matrix": [16], "matrixType",
//!       "matrices": [ { "matrix": [16], "matrixType" } ], "registrationType": {code} | null,
//!       "comment": string | null, "referencedImages": [ { "sopClassUid", "sopInstanceUid" } ],
//!       "isIdentity": bool } ],
//!   "referencedSeries": [ { "studyInstanceUid", "seriesInstanceUid", "instances": [...] } ],
//!   "warnings": [ "..." ] }
//! ```

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use dcmnorm_core::header::Length;
use dcmnorm_core::value::DataSetSequence;
use dcmnorm_core::{DataElement, PrimitiveValue, Tag, VR};
use dcmnorm_dictionary::{tags, uids};
use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject};
use serde_json::{json, Value as Json};

use super::volume_export::generate_uid;

pub const SPATIAL_REGISTRATION_SOP_CLASS_UID: &str = uids::SPATIAL_REGISTRATION_STORAGE;
pub const DEFORMABLE_SPATIAL_REGISTRATION_SOP_CLASS_UID: &str = uids::DEFORMABLE_SPATIAL_REGISTRATION_STORAGE;

const IDENTITY: [f64; 16] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];

#[derive(Debug)]
pub enum SpatialRegistrationError {
    NotARegistration(String),
    InvalidSpec(String),
    Meta(String),
}

impl fmt::Display for SpatialRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotARegistration(sop_class) => write!(
                formatter,
                "not a Spatial Registration object (SOP Class UID {sop_class:?}; expected {SPATIAL_REGISTRATION_SOP_CLASS_UID} or {DEFORMABLE_SPATIAL_REGISTRATION_SOP_CLASS_UID})"
            ),
            Self::InvalidSpec(message) => write!(formatter, "invalid spatial registration: {message}"),
            Self::Meta(message) => write!(formatter, "failed to build the registration's file meta: {message}"),
        }
    }
}

impl std::error::Error for SpatialRegistrationError {}

// ---------------------------------------------------------------------------------------------
// Element helpers
// ---------------------------------------------------------------------------------------------

type Item = InMemDicomObject;

fn text_of(item: &Item, tag: Tag) -> Option<String> {
    let value = item.get(tag)?.to_str().ok()?;
    let value = value.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!value.is_empty()).then(|| value.to_owned())
}

fn items_of(item: &Item, tag: Tag) -> &[Item] {
    item.get(tag).and_then(|element| element.value().items()).unwrap_or(&[])
}

fn floats_of(item: &Item, tag: Tag) -> Option<Vec<f64>> {
    let text = item.get(tag)?.to_str().ok()?;
    let values: Vec<f64> = text.split('\\').filter_map(|part| part.trim().parse::<f64>().ok()).collect();
    (!values.is_empty()).then_some(values)
}

fn str_value(value: &str) -> PrimitiveValue {
    PrimitiveValue::Strs(vec![value.to_owned()].into())
}

fn put_str(item: &mut Item, tag: Tag, vr: VR, value: &str) {
    item.put(DataElement::new(tag, vr, str_value(value)));
}

/// A type 2 attribute: present, empty when there is no value.
fn put_type2(item: &mut Item, tag: Tag, vr: VR, value: Option<&str>) {
    match value {
        Some(value) if !value.is_empty() => put_str(item, tag, vr, value),
        _ => {
            item.put(DataElement::new(tag, vr, PrimitiveValue::Empty));
        }
    }
}

fn put_sequence(item: &mut Item, tag: Tag, items: Vec<Item>) {
    item.put(DataElement::new(tag, VR::SQ, DataSetSequence::new(items, Length::UNDEFINED)));
}

fn ds_value(values: &[f64]) -> PrimitiveValue {
    // DS is at most 16 characters per value: `{:.10e}`-style would overflow, so use the shortest
    // representation that still round-trips and fits.
    PrimitiveValue::Strs(values.iter().map(|&v| format_ds(v)).collect::<Vec<_>>().into())
}

/// A decimal string of at most 16 characters (PS3.5 6.2 DS) as close to `v` as fits.
pub fn format_ds(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return "0".to_owned();
    }
    for precision in (0..=15).rev() {
        let fixed = format!("{v:.precision$}");
        let fixed = if fixed.contains('.') { fixed.trim_end_matches('0').trim_end_matches('.').to_owned() } else { fixed };
        if fixed.len() <= 16 && fixed != "-0" && fixed != "0" {
            return fixed;
        }
    }
    for precision in (0..=10).rev() {
        let scientific = format!("{v:.precision$e}");
        if scientific.len() <= 16 {
            return scientific;
        }
    }
    "0".to_owned()
}

fn mat_mul(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    let mut out = [0.0; 16];
    for r in 0..4 {
        for c in 0..4 {
            out[r * 4 + c] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum();
        }
    }
    out
}

fn is_identity(m: &[f64; 16]) -> bool {
    m.iter().zip(IDENTITY.iter()).all(|(a, b)| (a - b).abs() < 1e-6)
}

/// Problems with `m` as a `matrix_type` matrix (empty when it qualifies).
fn matrix_problems(m: &[f64; 16], matrix_type: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if m.iter().any(|v| !v.is_finite()) {
        problems.push("matrix has a non-finite value".to_owned());
        return problems;
    }
    if (m[12].abs() + m[13].abs() + m[14].abs()) > 1e-6 || (m[15] - 1.0).abs() > 1e-6 {
        problems.push("matrix bottom row is not [0, 0, 0, 1]".to_owned());
    }
    let column = |c: usize| [m[c], m[4 + c], m[8 + c]];
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let (c0, c1, c2) = (column(0), column(1), column(2));
    let orthogonal = dot(c0, c1).abs() < 1e-3 * (dot(c0, c0) * dot(c1, c1)).sqrt().max(1e-12)
        && dot(c0, c2).abs() < 1e-3 * (dot(c0, c0) * dot(c2, c2)).sqrt().max(1e-12)
        && dot(c1, c2).abs() < 1e-3 * (dot(c1, c1) * dot(c2, c2)).sqrt().max(1e-12);
    let det = c0[0] * (c1[1] * c2[2] - c1[2] * c2[1]) - c1[0] * (c0[1] * c2[2] - c0[2] * c2[1]) + c2[0] * (c0[1] * c1[2] - c0[2] * c1[1]);
    match matrix_type {
        "RIGID" => {
            let unit = [c0, c1, c2].iter().all(|c| (dot(*c, *c).sqrt() - 1.0).abs() < 1e-3);
            if !(orthogonal && unit && det > 0.0) {
                problems.push("a RIGID matrix must be a proper rotation (orthonormal, determinant +1) plus a translation".to_owned());
            }
        }
        "RIGID_SCALE" => {
            if !(orthogonal && det > 0.0) {
                problems.push("a RIGID_SCALE matrix must be a rotation with scaling (orthogonal axes, positive determinant)".to_owned());
            }
        }
        "AFFINE" => {
            if det.abs() < 1e-9 {
                problems.push("an AFFINE matrix must be invertible".to_owned());
            }
        }
        other => problems.push(format!("unknown Frame of Reference Transformation Matrix Type {other:?}")),
    }
    problems
}

fn combined_type(types: &[String]) -> String {
    if types.iter().any(|t| t == "AFFINE") {
        "AFFINE".to_owned()
    } else if types.iter().any(|t| t == "RIGID_SCALE") {
        "RIGID_SCALE".to_owned()
    } else {
        "RIGID".to_owned()
    }
}

fn code_json(item: Option<&Item>) -> Json {
    match item {
        Some(code) => json!({
            "codeValue": text_of(code, tags::CODE_VALUE),
            "codingSchemeDesignator": text_of(code, tags::CODING_SCHEME_DESIGNATOR),
            "codeMeaning": text_of(code, tags::CODE_MEANING),
        }),
        None => Json::Null,
    }
}

fn references_json(items: &[Item]) -> Vec<Json> {
    items
        .iter()
        .map(|r| json!({ "sopClassUid": text_of(r, tags::REFERENCED_SOP_CLASS_UID), "sopInstanceUid": text_of(r, tags::REFERENCED_SOP_INSTANCE_UID) }))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

fn sop_class_of(object: &DefaultDicomObject) -> String {
    text_of(object, tags::SOP_CLASS_UID).unwrap_or_else(|| object.meta().media_storage_sop_class_uid().trim_end_matches('\0').to_owned())
}

/// Reads the matrices of a Matrix Registration Sequence item: `(composed, type, per-matrix)`.
fn read_matrices(matrix_registration: &Item, warnings: &mut Vec<String>, context: &str) -> Option<([f64; 16], String, Vec<Json>)> {
    let mut composed = IDENTITY;
    let mut types = Vec::new();
    let mut each = Vec::new();
    let matrices = items_of(matrix_registration, tags::MATRIX_SEQUENCE);
    if matrices.is_empty() {
        warnings.push(format!("{context}: Matrix Sequence is missing or empty"));
        return None;
    }
    for (index, item) in matrices.iter().enumerate() {
        let Some(values) = floats_of(item, tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX) else {
            warnings.push(format!("{context}: matrix {} has no Frame of Reference Transformation Matrix", index + 1));
            return None;
        };
        if values.len() != 16 {
            warnings.push(format!("{context}: matrix {} has {} values, not 16", index + 1, values.len()));
            return None;
        }
        let mut m = [0.0; 16];
        m.copy_from_slice(&values);
        let matrix_type = text_of(item, tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX_TYPE).unwrap_or_else(|| {
            warnings.push(format!("{context}: matrix {} has no Frame of Reference Transformation Matrix Type; treated as AFFINE", index + 1));
            "AFFINE".to_owned()
        });
        for problem in matrix_problems(&m, &matrix_type) {
            warnings.push(format!("{context}: matrix {}: {problem}", index + 1));
        }
        // Items apply in order: the composed matrix is M_n ... M_1.
        composed = mat_mul(&m, &composed);
        each.push(json!({ "matrix": m.to_vec(), "matrixType": matrix_type }));
        types.push(matrix_type);
    }
    Some((composed, combined_type(&types), each))
}

/// Reads a Spatial Registration (or the rigid parts of a Deformable Spatial Registration) object
/// into the module's JSON form. `warnings` lists IOD problems (`validate_spatial_registration`)
/// and anything that could not be interpreted.
pub fn read_spatial_registration(object: &DefaultDicomObject) -> Result<Json, SpatialRegistrationError> {
    let sop_class = sop_class_of(object);
    let deformable = match sop_class.as_str() {
        SPATIAL_REGISTRATION_SOP_CLASS_UID => false,
        DEFORMABLE_SPATIAL_REGISTRATION_SOP_CLASS_UID => true,
        _ => return Err(SpatialRegistrationError::NotARegistration(sop_class)),
    };
    let mut warnings = validate_spatial_registration(object);
    let mut registrations = Vec::new();
    if !deformable {
        for (index, item) in items_of(object, tags::REGISTRATION_SEQUENCE).iter().enumerate() {
            let context = format!("registration {}", index + 1);
            let frame = text_of(item, tags::FRAME_OF_REFERENCE_UID);
            let images = references_json(items_of(item, tags::REFERENCED_IMAGE_SEQUENCE));
            let Some(matrix_registration) = items_of(item, tags::MATRIX_REGISTRATION_SEQUENCE).first() else {
                warnings.push(format!("{context}: Matrix Registration Sequence is missing"));
                continue;
            };
            let Some((matrix, matrix_type, matrices)) = read_matrices(matrix_registration, &mut warnings, &context) else { continue };
            registrations.push(json!({
                "frameOfReferenceUid": frame,
                "matrix": matrix.to_vec(),
                "matrixType": matrix_type,
                "matrices": matrices,
                "registrationType": code_json(items_of(matrix_registration, tags::REGISTRATION_TYPE_CODE_SEQUENCE).first()),
                "comment": text_of(matrix_registration, tags::FRAME_OF_REFERENCE_TRANSFORMATION_COMMENT),
                "referencedImages": images,
                "isIdentity": is_identity(&matrix),
            }));
        }
    } else {
        // Deformable: Pre- and Post-Deformation matrices only; the deformation grid is not applied.
        for (index, item) in items_of(object, tags::DEFORMABLE_REGISTRATION_SEQUENCE).iter().enumerate() {
            let context = format!("deformable registration {}", index + 1);
            let frame = text_of(item, tags::SOURCE_FRAME_OF_REFERENCE_UID);
            let mut matrix = IDENTITY;
            let mut types = Vec::new();
            let mut matrices = Vec::new();
            for sequence in [tags::PRE_DEFORMATION_MATRIX_REGISTRATION_SEQUENCE, tags::POST_DEFORMATION_MATRIX_REGISTRATION_SEQUENCE] {
                if let Some(entry) = items_of(item, sequence).first() {
                    // These items hold the matrix and its type directly (C.20.3).
                    let values = floats_of(entry, tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX).filter(|v| v.len() == 16);
                    let Some(values) = values else {
                        warnings.push(format!("{context}: a pre/post-deformation matrix is missing or not 16 values"));
                        continue;
                    };
                    let mut m = [0.0; 16];
                    m.copy_from_slice(&values);
                    let matrix_type = text_of(entry, tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX_TYPE).unwrap_or_else(|| "AFFINE".to_owned());
                    matrix = mat_mul(&m, &matrix);
                    matrices.push(json!({ "matrix": m.to_vec(), "matrixType": matrix_type }));
                    types.push(matrix_type);
                }
            }
            registrations.push(json!({
                "frameOfReferenceUid": frame,
                "matrix": matrix.to_vec(),
                "matrixType": combined_type(&types),
                "matrices": matrices,
                "registrationType": code_json(items_of(item, tags::REGISTRATION_TYPE_CODE_SEQUENCE).first()),
                "comment": text_of(item, tags::FRAME_OF_REFERENCE_TRANSFORMATION_COMMENT),
                "referencedImages": references_json(items_of(item, tags::REFERENCED_IMAGE_SEQUENCE)),
                "isIdentity": is_identity(&matrix),
                "deformationIgnored": item.get(tags::DEFORMABLE_REGISTRATION_GRID_SEQUENCE).is_some(),
            }));
        }
    }

    let own_study = text_of(object, tags::STUDY_INSTANCE_UID);
    let mut referenced_series = Vec::new();
    let series_json = |study: &Option<String>, series: &Item| {
        json!({
            "studyInstanceUid": study,
            "seriesInstanceUid": text_of(series, tags::SERIES_INSTANCE_UID),
            "instances": references_json(items_of(series, tags::REFERENCED_INSTANCE_SEQUENCE)),
        })
    };
    for series in items_of(object, tags::REFERENCED_SERIES_SEQUENCE) {
        referenced_series.push(series_json(&own_study, series));
    }
    for study in items_of(object, tags::STUDIES_CONTAINING_OTHER_REFERENCED_INSTANCES_SEQUENCE) {
        let study_uid = text_of(study, tags::STUDY_INSTANCE_UID);
        for series in items_of(study, tags::REFERENCED_SERIES_SEQUENCE) {
            referenced_series.push(series_json(&study_uid, series));
        }
    }

    Ok(json!({
        "sopClassUid": sop_class,
        "sopInstanceUid": text_of(object, tags::SOP_INSTANCE_UID),
        "seriesInstanceUid": text_of(object, tags::SERIES_INSTANCE_UID),
        "studyInstanceUid": own_study,
        "frameOfReferenceUid": text_of(object, tags::FRAME_OF_REFERENCE_UID),
        "patientId": text_of(object, tags::PATIENT_ID),
        "patientName": text_of(object, tags::PATIENT_NAME),
        "contentLabel": text_of(object, tags::CONTENT_LABEL),
        "contentDescription": text_of(object, tags::CONTENT_DESCRIPTION),
        "contentDate": text_of(object, tags::CONTENT_DATE),
        "contentTime": text_of(object, tags::CONTENT_TIME),
        "deformable": deformable,
        "registrations": registrations,
        "referencedSeries": referenced_series,
        "warnings": warnings,
    }))
}

// ---------------------------------------------------------------------------------------------
// Validate
// ---------------------------------------------------------------------------------------------

fn is_valid_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() <= 64
        && uid.split('.').all(|component| !component.is_empty() && component.bytes().all(|b| b.is_ascii_digit()) && (component == "0" || !component.starts_with('0')))
}

/// Checks `object` against the Spatial Registration IOD (PS3.3 A.39.3): every Type 1 attribute
/// of its mandatory modules present and non-empty, every Type 2 present, the conditional
/// attributes of the Registration Sequence, Modality REG, valid UIDs, well-formed matrices, and
/// every instance referenced by the Registration Sequence listed in the Common Instance
/// Reference module. Returns the problems found (empty for a conformant object). Deformable
/// objects only get the shared-module checks.
pub fn validate_spatial_registration(object: &DefaultDicomObject) -> Vec<String> {
    let mut problems = Vec::new();
    let type1 = |problems: &mut Vec<String>, item: &Item, tag: Tag, name: &str, context: &str| {
        if text_of(item, tag).is_none() && items_of(item, tag).is_empty() {
            problems.push(format!("{context}{name} {tag} is Type 1 and missing or empty"));
        }
    };
    let type2 = |problems: &mut Vec<String>, item: &Item, tag: Tag, name: &str, context: &str| {
        if item.get(tag).is_none() {
            problems.push(format!("{context}{name} {tag} is Type 2 and missing"));
        }
    };
    let uid = |problems: &mut Vec<String>, item: &Item, tag: Tag, name: &str, context: &str| {
        if let Some(value) = text_of(item, tag) {
            if !is_valid_uid(&value) {
                problems.push(format!("{context}{name} {value:?} is not a valid UID"));
            }
        }
    };

    // SOP Common, Patient, General Study, General Series, Spatial Registration Series, Frame of
    // Reference, General Equipment.
    for (tag, name) in [
        (tags::SOP_CLASS_UID, "SOPClassUID"),
        (tags::SOP_INSTANCE_UID, "SOPInstanceUID"),
        (tags::STUDY_INSTANCE_UID, "StudyInstanceUID"),
        (tags::SERIES_INSTANCE_UID, "SeriesInstanceUID"),
        (tags::MODALITY, "Modality"),
        (tags::FRAME_OF_REFERENCE_UID, "FrameOfReferenceUID"),
    ] {
        type1(&mut problems, object, tag, name, "");
    }
    for (tag, name) in [(tags::SOP_CLASS_UID, "SOPClassUID"), (tags::SOP_INSTANCE_UID, "SOPInstanceUID"), (tags::STUDY_INSTANCE_UID, "StudyInstanceUID"), (tags::SERIES_INSTANCE_UID, "SeriesInstanceUID"), (tags::FRAME_OF_REFERENCE_UID, "FrameOfReferenceUID")] {
        uid(&mut problems, object, tag, name, "");
    }
    for (tag, name) in [
        (tags::PATIENT_NAME, "PatientName"),
        (tags::PATIENT_ID, "PatientID"),
        (tags::PATIENT_BIRTH_DATE, "PatientBirthDate"),
        (tags::PATIENT_SEX, "PatientSex"),
        (tags::STUDY_DATE, "StudyDate"),
        (tags::STUDY_TIME, "StudyTime"),
        (tags::REFERRING_PHYSICIAN_NAME, "ReferringPhysicianName"),
        (tags::STUDY_ID, "StudyID"),
        (tags::ACCESSION_NUMBER, "AccessionNumber"),
        (tags::SERIES_NUMBER, "SeriesNumber"),
        (tags::POSITION_REFERENCE_INDICATOR, "PositionReferenceIndicator"),
        (tags::MANUFACTURER, "Manufacturer"),
    ] {
        type2(&mut problems, object, tag, name, "");
    }
    if let Some(modality) = text_of(object, tags::MODALITY) {
        if modality != "REG" {
            problems.push(format!("Modality is {modality:?}; the Spatial Registration Series module requires REG"));
        }
    }
    let meta_class = object.meta().media_storage_sop_class_uid().trim_end_matches('\0').to_owned();
    if let Some(class) = text_of(object, tags::SOP_CLASS_UID) {
        if !meta_class.is_empty() && meta_class != class {
            problems.push(format!("MediaStorageSOPClassUID {meta_class:?} differs from SOPClassUID {class:?}"));
        }
    }
    let meta_instance = object.meta().media_storage_sop_instance_uid().trim_end_matches('\0').to_owned();
    if let Some(instance) = text_of(object, tags::SOP_INSTANCE_UID) {
        if !meta_instance.is_empty() && meta_instance != instance {
            problems.push(format!("MediaStorageSOPInstanceUID {meta_instance:?} differs from SOPInstanceUID {instance:?}"));
        }
    }
    if sop_class_of(object) != SPATIAL_REGISTRATION_SOP_CLASS_UID {
        return problems;
    }

    // Spatial Registration module (C.20.2) with its Content Identification macro.
    type1(&mut problems, object, tags::INSTANCE_NUMBER, "InstanceNumber", "");
    type1(&mut problems, object, tags::CONTENT_LABEL, "ContentLabel", "");
    type2(&mut problems, object, tags::CONTENT_DESCRIPTION, "ContentDescription", "");
    type2(&mut problems, object, tags::CONTENT_CREATOR_NAME, "ContentCreatorName", "");
    type1(&mut problems, object, tags::CONTENT_DATE, "ContentDate", "");
    type1(&mut problems, object, tags::CONTENT_TIME, "ContentTime", "");
    if let Some(label) = text_of(object, tags::CONTENT_LABEL) {
        if label.len() > 16 || !label.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b' ') {
            problems.push(format!("ContentLabel {label:?} is not a valid CS value (A-Z, 0-9, space, underscore; 16 characters)"));
        }
    }
    let registrations = items_of(object, tags::REGISTRATION_SEQUENCE);
    if registrations.is_empty() {
        problems.push("RegistrationSequence (0070,0308) is Type 1 and missing or empty".to_owned());
    }
    let own_frame = text_of(object, tags::FRAME_OF_REFERENCE_UID);
    let mut referenced_instances: Vec<String> = Vec::new();
    for (index, item) in registrations.iter().enumerate() {
        let context = format!("RegistrationSequence item {}: ", index + 1);
        let frame = text_of(item, tags::FRAME_OF_REFERENCE_UID);
        let images = items_of(item, tags::REFERENCED_IMAGE_SEQUENCE);
        if frame.is_none() && images.is_empty() {
            problems.push(format!("{context}needs FrameOfReferenceUID or ReferencedImageSequence (both Type 1C)"));
        }
        uid(&mut problems, item, tags::FRAME_OF_REFERENCE_UID, "FrameOfReferenceUID", &context);
        for (image_index, image) in images.iter().enumerate() {
            let image_context = format!("{context}ReferencedImageSequence item {}: ", image_index + 1);
            type1(&mut problems, image, tags::REFERENCED_SOP_CLASS_UID, "ReferencedSOPClassUID", &image_context);
            type1(&mut problems, image, tags::REFERENCED_SOP_INSTANCE_UID, "ReferencedSOPInstanceUID", &image_context);
            uid(&mut problems, image, tags::REFERENCED_SOP_INSTANCE_UID, "ReferencedSOPInstanceUID", &image_context);
            if let Some(instance) = text_of(image, tags::REFERENCED_SOP_INSTANCE_UID) {
                referenced_instances.push(instance);
            }
        }
        let matrix_registrations = items_of(item, tags::MATRIX_REGISTRATION_SEQUENCE);
        if matrix_registrations.len() != 1 {
            problems.push(format!("{context}MatrixRegistrationSequence must hold exactly one item (has {})", matrix_registrations.len()));
        }
        for matrix_registration in matrix_registrations {
            type2(&mut problems, matrix_registration, tags::REGISTRATION_TYPE_CODE_SEQUENCE, "RegistrationTypeCodeSequence", &context);
            let matrices = items_of(matrix_registration, tags::MATRIX_SEQUENCE);
            if matrices.is_empty() {
                problems.push(format!("{context}MatrixSequence is Type 1 and missing or empty"));
            }
            for (matrix_index, matrix) in matrices.iter().enumerate() {
                let matrix_context = format!("{context}MatrixSequence item {}: ", matrix_index + 1);
                match floats_of(matrix, tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX) {
                    Some(values) if values.len() == 16 => {
                        let mut m = [0.0; 16];
                        m.copy_from_slice(&values);
                        match text_of(matrix, tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX_TYPE) {
                            Some(matrix_type) => {
                                for problem in matrix_problems(&m, &matrix_type) {
                                    problems.push(format!("{matrix_context}{problem}"));
                                }
                                if frame.is_some() && frame == own_frame && !is_identity(&m) {
                                    problems.push(format!("{matrix_context}the registration's own Frame of Reference must map with the identity"));
                                }
                            }
                            None => problems.push(format!("{matrix_context}FrameOfReferenceTransformationMatrixType is Type 1 and missing")),
                        }
                    }
                    Some(values) => problems.push(format!("{matrix_context}FrameOfReferenceTransformationMatrix has {} values, not 16", values.len())),
                    None => problems.push(format!("{matrix_context}FrameOfReferenceTransformationMatrix is Type 1 and missing")),
                }
            }
        }
    }

    // Common Instance Reference (C.12.2): every referenced instance listed, once per series.
    let mut listed: Vec<String> = Vec::new();
    let collect = |series_items: &[Item], listed: &mut Vec<String>, problems: &mut Vec<String>, context: &str| {
        for (index, series) in series_items.iter().enumerate() {
            let series_context = format!("{context}ReferencedSeriesSequence item {}: ", index + 1);
            type1(problems, series, tags::SERIES_INSTANCE_UID, "SeriesInstanceUID", &series_context);
            let instances = items_of(series, tags::REFERENCED_INSTANCE_SEQUENCE);
            if instances.is_empty() {
                problems.push(format!("{series_context}ReferencedInstanceSequence is Type 1 and missing or empty"));
            }
            for instance in instances {
                if let Some(uid) = text_of(instance, tags::REFERENCED_SOP_INSTANCE_UID) {
                    listed.push(uid);
                }
                if text_of(instance, tags::REFERENCED_SOP_CLASS_UID).is_none() {
                    problems.push(format!("{series_context}a ReferencedInstanceSequence item has no ReferencedSOPClassUID"));
                }
            }
        }
    };
    collect(items_of(object, tags::REFERENCED_SERIES_SEQUENCE), &mut listed, &mut problems, "");
    for (index, study) in items_of(object, tags::STUDIES_CONTAINING_OTHER_REFERENCED_INSTANCES_SEQUENCE).iter().enumerate() {
        let context = format!("StudiesContainingOtherReferencedInstancesSequence item {}: ", index + 1);
        type1(&mut problems, study, tags::STUDY_INSTANCE_UID, "StudyInstanceUID", &context);
        collect(items_of(study, tags::REFERENCED_SERIES_SEQUENCE), &mut listed, &mut problems, &context);
    }
    let missing = referenced_instances.iter().filter(|uid| !listed.contains(uid)).count();
    if missing > 0 {
        problems.push(format!("{missing} instance(s) referenced by the RegistrationSequence are not listed in the Common Instance Reference module"));
    }
    problems
}

// ---------------------------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------------------------

struct SeriesRef {
    frame_of_reference_uid: String,
    study_instance_uid: String,
    series_instance_uid: String,
    instances: Vec<(String, String)>,
}

fn spec_str<'a>(spec: &'a Json, key: &str) -> Option<&'a str> {
    spec.get(key).and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn require_uid(spec: &Json, key: &str, context: &str) -> Result<String, SpatialRegistrationError> {
    let value = spec_str(spec, key).ok_or_else(|| SpatialRegistrationError::InvalidSpec(format!("{context}.{key} is required")))?;
    if !is_valid_uid(value) {
        return Err(SpatialRegistrationError::InvalidSpec(format!("{context}.{key} {value:?} is not a valid UID")));
    }
    Ok(value.to_owned())
}

fn parse_series(spec: &Json, key: &str) -> Result<SeriesRef, SpatialRegistrationError> {
    let side = spec.get(key).filter(|v| v.is_object()).ok_or_else(|| SpatialRegistrationError::InvalidSpec(format!("{key} is required")))?;
    let mut instances = Vec::new();
    if let Some(list) = side.get("instances") {
        let list = list.as_array().ok_or_else(|| SpatialRegistrationError::InvalidSpec(format!("{key}.instances must be an array")))?;
        for (index, instance) in list.iter().enumerate() {
            let context = format!("{key}.instances[{index}]");
            instances.push((require_uid(instance, "sopClassUid", &context)?, require_uid(instance, "sopInstanceUid", &context)?));
        }
    }
    Ok(SeriesRef {
        frame_of_reference_uid: require_uid(side, "frameOfReferenceUid", key)?,
        study_instance_uid: require_uid(side, "studyInstanceUid", key)?,
        series_instance_uid: require_uid(side, "seriesInstanceUid", key)?,
        instances,
    })
}

fn code_item(value: &str, scheme: &str, meaning: &str) -> Item {
    let mut code = Item::new_empty();
    put_str(&mut code, tags::CODE_VALUE, VR::SH, value);
    put_str(&mut code, tags::CODING_SCHEME_DESIGNATOR, VR::SH, scheme);
    put_str(&mut code, tags::CODE_MEANING, VR::LO, meaning);
    code
}

fn image_references(instances: &[(String, String)]) -> Vec<Item> {
    instances
        .iter()
        .map(|(class, instance)| {
            let mut item = Item::new_empty();
            put_str(&mut item, tags::REFERENCED_SOP_CLASS_UID, VR::UI, class);
            put_str(&mut item, tags::REFERENCED_SOP_INSTANCE_UID, VR::UI, instance);
            item
        })
        .collect()
}

fn registration_item(series: &SeriesRef, matrix: &[f64; 16], matrix_type: &str, code: Option<&Item>, comment: Option<&str>) -> Item {
    let mut item = Item::new_empty();
    put_str(&mut item, tags::FRAME_OF_REFERENCE_UID, VR::UI, &series.frame_of_reference_uid);
    if !series.instances.is_empty() {
        put_sequence(&mut item, tags::REFERENCED_IMAGE_SEQUENCE, image_references(&series.instances));
    }
    let mut matrix_item = Item::new_empty();
    matrix_item.put(DataElement::new(tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX, VR::DS, ds_value(matrix)));
    put_str(&mut matrix_item, tags::FRAME_OF_REFERENCE_TRANSFORMATION_MATRIX_TYPE, VR::CS, matrix_type);
    let mut matrix_registration = Item::new_empty();
    if let Some(comment) = comment {
        put_str(&mut matrix_registration, tags::FRAME_OF_REFERENCE_TRANSFORMATION_COMMENT, VR::LO, comment);
    }
    put_sequence(&mut matrix_registration, tags::REGISTRATION_TYPE_CODE_SEQUENCE, code.into_iter().cloned().collect());
    put_sequence(&mut matrix_registration, tags::MATRIX_SEQUENCE, vec![matrix_item]);
    put_sequence(&mut item, tags::MATRIX_REGISTRATION_SEQUENCE, vec![matrix_registration]);
    item
}

fn referenced_series_item(series: &SeriesRef) -> Item {
    let mut item = Item::new_empty();
    put_str(&mut item, tags::SERIES_INSTANCE_UID, VR::UI, &series.series_instance_uid);
    let instances = series
        .instances
        .iter()
        .map(|(class, instance)| {
            let mut reference = Item::new_empty();
            put_str(&mut reference, tags::REFERENCED_SOP_CLASS_UID, VR::UI, class);
            put_str(&mut reference, tags::REFERENCED_SOP_INSTANCE_UID, VR::UI, instance);
            reference
        })
        .collect();
    put_sequence(&mut item, tags::REFERENCED_INSTANCE_SEQUENCE, instances);
    item
}

/// UTC `(YYYYMMDD, HHMMSS)` for now.
fn now_date_time() -> (String, String) {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    // Civil-from-days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        format!("{year:04}{month:02}{day:02}"),
        format!("{:02}{:02}{:02}", of_day / 3600, (of_day / 60) % 60, of_day % 60),
    )
}

/// One explicit `spec.patient` / `spec.study` field: JSON key, tag, VR.
type OverrideField = (&'static str, Tag, VR);

/// Patient / General Study / Patient Study attributes copied from the fixed study.
const COPIED_ATTRIBUTES: &[(Tag, VR)] = &[
    (tags::PATIENT_NAME, VR::PN),
    (tags::PATIENT_ID, VR::LO),
    (tags::ISSUER_OF_PATIENT_ID, VR::LO),
    (tags::PATIENT_BIRTH_DATE, VR::DA),
    (tags::PATIENT_SEX, VR::CS),
    (tags::STUDY_INSTANCE_UID, VR::UI),
    (tags::STUDY_DATE, VR::DA),
    (tags::STUDY_TIME, VR::TM),
    (tags::REFERRING_PHYSICIAN_NAME, VR::PN),
    (tags::STUDY_ID, VR::SH),
    (tags::ACCESSION_NUMBER, VR::SH),
    (tags::STUDY_DESCRIPTION, VR::LO),
    (tags::PATIENT_AGE, VR::AS),
    (tags::PATIENT_SIZE, VR::DS),
    (tags::PATIENT_WEIGHT, VR::DS),
];

/// Builds a Spatial Registration instance from `spec` (the shape is documented on the Node
/// binding's `writeSpatialRegistration`): the REG belongs to the FIXED study and Frame of
/// Reference; item 1 maps the fixed Frame of Reference with the identity, item 2 the moving one
/// with `spec.matrix` (moving -> fixed). Patient and study attributes come from `copy_from` (one
/// instance of the fixed series) overridden by `spec.patient` / `spec.study`. Rejects a spec that
/// would not make a conformant object.
pub fn build_spatial_registration(spec: &Json, copy_from: Option<&DefaultDicomObject>) -> Result<DefaultDicomObject, SpatialRegistrationError> {
    let invalid = |message: String| SpatialRegistrationError::InvalidSpec(message);
    if !spec.is_object() {
        return Err(invalid("the spec must be a JSON object".into()));
    }
    let fixed = parse_series(spec, "fixed")?;
    let moving = parse_series(spec, "moving")?;
    if fixed.frame_of_reference_uid == moving.frame_of_reference_uid {
        return Err(invalid("fixed and moving share one Frame of Reference: they are already registered (identity), no REG is needed".into()));
    }
    let values: Vec<f64> = spec
        .get("matrix")
        .and_then(Json::as_array)
        .map(|a| a.iter().filter_map(Json::as_f64).collect())
        .unwrap_or_default();
    if values.len() != 16 || spec.get("matrix").and_then(Json::as_array).map(Vec::len) != Some(16) {
        return Err(invalid("matrix must be 16 numbers (row-major, moving -> fixed)".into()));
    }
    let mut matrix = [0.0; 16];
    matrix.copy_from_slice(&values);
    let matrix_type = spec_str(spec, "matrixType").unwrap_or("RIGID").to_ascii_uppercase();
    let problems = matrix_problems(&matrix, &matrix_type);
    if !problems.is_empty() {
        return Err(invalid(problems.join("; ")));
    }
    let code = match spec_str(spec, "registrationType").unwrap_or("auto") {
        "auto" => code_item("125024", "DCM", "Image Content-based Alignment"),
        "manual" => code_item("125025", "DCM", "Visual Alignment"),
        "identity" => code_item("125021", "DCM", "Frame of Reference Identity"),
        other => return Err(invalid(format!("registrationType {other:?}; expected auto, manual or identity"))),
    };
    let content_label = spec_str(spec, "contentLabel").unwrap_or("REGISTRATION").to_owned();
    if content_label.len() > 16 || !content_label.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b' ') {
        return Err(invalid(format!("contentLabel {content_label:?} must be 1-16 characters of A-Z, 0-9, space or underscore")));
    }
    let sop_instance_uid = match spec_str(spec, "sopInstanceUid") {
        Some(_) => require_uid(spec, "sopInstanceUid", "spec")?,
        None => generate_uid(),
    };
    let series_instance_uid = match spec_str(spec, "seriesInstanceUid") {
        Some(_) => require_uid(spec, "seriesInstanceUid", "spec")?,
        None => generate_uid(),
    };
    let number = |key: &str, default: i64| -> Result<String, SpatialRegistrationError> {
        match spec.get(key) {
            None | Some(Json::Null) => Ok(default.to_string()),
            Some(value) => value
                .as_i64()
                .filter(|n| (0..=2_147_483_647).contains(n))
                .map(|n| n.to_string())
                .ok_or_else(|| invalid(format!("{key} must be a non-negative integer"))),
        }
    };
    let series_number = number("seriesNumber", 9902)?;
    let instance_number = number("instanceNumber", 1)?;

    let meta = FileMetaTableBuilder::new()
        .media_storage_sop_class_uid(SPATIAL_REGISTRATION_SOP_CLASS_UID)
        .media_storage_sop_instance_uid(sop_instance_uid.clone())
        .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .build()
        .map_err(|error| SpatialRegistrationError::Meta(error.to_string()))?;
    let mut object = DefaultDicomObject::new_empty_with_meta(meta);
    let (today, now) = now_date_time();

    // SOP Common.
    put_str(&mut object, tags::SPECIFIC_CHARACTER_SET, VR::CS, "ISO_IR 192");
    put_str(&mut object, tags::SOP_CLASS_UID, VR::UI, SPATIAL_REGISTRATION_SOP_CLASS_UID);
    put_str(&mut object, tags::SOP_INSTANCE_UID, VR::UI, &sop_instance_uid);
    put_str(&mut object, tags::INSTANCE_CREATION_DATE, VR::DA, &today);
    put_str(&mut object, tags::INSTANCE_CREATION_TIME, VR::TM, &now);

    // Patient, General Study, Patient Study: copied, then explicit overrides.
    if let Some(source) = copy_from {
        for &(tag, vr) in COPIED_ATTRIBUTES {
            if let Some(value) = text_of(source, tag) {
                put_str(&mut object, tag, vr, &value);
            }
        }
        if let Some(copied_study) = text_of(source, tags::STUDY_INSTANCE_UID) {
            if copied_study != fixed.study_instance_uid {
                return Err(invalid(format!(
                    "copyPatientStudyFrom belongs to study {copied_study}, not the fixed study {}",
                    fixed.study_instance_uid
                )));
            }
        }
    }
    let overrides: [(&str, &[OverrideField]); 2] = [
        (
            "patient",
            &[
                ("patientName", tags::PATIENT_NAME, VR::PN),
                ("patientId", tags::PATIENT_ID, VR::LO),
                ("patientBirthDate", tags::PATIENT_BIRTH_DATE, VR::DA),
                ("patientSex", tags::PATIENT_SEX, VR::CS),
            ],
        ),
        (
            "study",
            &[
                ("studyDate", tags::STUDY_DATE, VR::DA),
                ("studyTime", tags::STUDY_TIME, VR::TM),
                ("accessionNumber", tags::ACCESSION_NUMBER, VR::SH),
                ("referringPhysicianName", tags::REFERRING_PHYSICIAN_NAME, VR::PN),
                ("studyId", tags::STUDY_ID, VR::SH),
                ("studyDescription", tags::STUDY_DESCRIPTION, VR::LO),
            ],
        ),
    ];
    for (group, fields) in overrides {
        if let Some(values) = spec.get(group).and_then(Json::as_object) {
            for &(key, tag, vr) in fields {
                if let Some(value) = values.get(key).and_then(Json::as_str) {
                    put_type2(&mut object, tag, vr, Some(value));
                }
            }
        }
    }
    put_str(&mut object, tags::STUDY_INSTANCE_UID, VR::UI, &fixed.study_instance_uid);
    for (tag, vr) in [
        (tags::PATIENT_NAME, VR::PN),
        (tags::PATIENT_ID, VR::LO),
        (tags::PATIENT_BIRTH_DATE, VR::DA),
        (tags::PATIENT_SEX, VR::CS),
        (tags::STUDY_DATE, VR::DA),
        (tags::STUDY_TIME, VR::TM),
        (tags::REFERRING_PHYSICIAN_NAME, VR::PN),
        (tags::STUDY_ID, VR::SH),
        (tags::ACCESSION_NUMBER, VR::SH),
    ] {
        if object.get(tag).is_none() {
            put_type2(&mut object, tag, vr, None);
        }
    }

    // General Series + Spatial Registration Series.
    put_str(&mut object, tags::MODALITY, VR::CS, "REG");
    put_str(&mut object, tags::SERIES_INSTANCE_UID, VR::UI, &series_instance_uid);
    put_str(&mut object, tags::SERIES_NUMBER, VR::IS, &series_number);
    put_str(&mut object, tags::SERIES_DATE, VR::DA, &today);
    put_str(&mut object, tags::SERIES_TIME, VR::TM, &now);
    put_str(&mut object, tags::SERIES_DESCRIPTION, VR::LO, spec_str(spec, "seriesDescription").unwrap_or("Registration"));

    // Frame of Reference: the registered (fixed) one.
    put_str(&mut object, tags::FRAME_OF_REFERENCE_UID, VR::UI, &fixed.frame_of_reference_uid);
    put_type2(&mut object, tags::POSITION_REFERENCE_INDICATOR, VR::LO, None);

    // General Equipment.
    put_type2(&mut object, tags::MANUFACTURER, VR::LO, Some(spec_str(spec, "manufacturer").unwrap_or("dcmnorm")));
    put_str(&mut object, tags::SOFTWARE_VERSIONS, VR::LO, concat!("dcmnorm ", env!("CARGO_PKG_VERSION")));

    // Spatial Registration (Content Identification macro + Registration Sequence).
    put_str(&mut object, tags::INSTANCE_NUMBER, VR::IS, &instance_number);
    put_str(&mut object, tags::CONTENT_LABEL, VR::CS, &content_label);
    put_type2(&mut object, tags::CONTENT_DESCRIPTION, VR::LO, spec_str(spec, "contentDescription"));
    put_type2(&mut object, tags::CONTENT_CREATOR_NAME, VR::PN, spec_str(spec, "contentCreatorName"));
    put_str(&mut object, tags::CONTENT_DATE, VR::DA, &today);
    put_str(&mut object, tags::CONTENT_TIME, VR::TM, &now);
    let comment = spec_str(spec, "comment");
    put_sequence(
        &mut object,
        tags::REGISTRATION_SEQUENCE,
        vec![
            registration_item(&fixed, &IDENTITY, "RIGID", Some(&code_item("125021", "DCM", "Frame of Reference Identity")), None),
            registration_item(&moving, &matrix, &matrix_type, Some(&code), comment),
        ],
    );

    // Common Instance Reference.
    let mut own_study_series = Vec::new();
    let mut other_items: Vec<Item> = Vec::new();
    for side in [&fixed, &moving] {
        if side.instances.is_empty() {
            continue;
        }
        if side.study_instance_uid == fixed.study_instance_uid {
            own_study_series.push(referenced_series_item(side));
        } else {
            let mut study = Item::new_empty();
            put_str(&mut study, tags::STUDY_INSTANCE_UID, VR::UI, &side.study_instance_uid);
            put_sequence(&mut study, tags::REFERENCED_SERIES_SEQUENCE, vec![referenced_series_item(side)]);
            other_items.push(study);
        }
    }
    if !own_study_series.is_empty() {
        put_sequence(&mut object, tags::REFERENCED_SERIES_SEQUENCE, own_study_series);
    }
    if !other_items.is_empty() {
        put_sequence(&mut object, tags::STUDIES_CONTAINING_OTHER_REFERENCED_INSTANCES_SEQUENCE, other_items);
    }

    let problems = validate_spatial_registration(&object);
    if !problems.is_empty() {
        return Err(invalid(problems.join("; ")));
    }
    Ok(object)
}

/// Whether `uid` is the Spatial Registration or Deformable Spatial Registration SOP Class.
pub fn is_spatial_registration_sop_class(uid: &str) -> bool {
    matches!(uid.trim_end_matches('\0'), SPATIAL_REGISTRATION_SOP_CLASS_UID | DEFORMABLE_SPATIAL_REGISTRATION_SOP_CLASS_UID)
}
