//! Synthetic DICOM fixtures for the volume, registration and Spatial Registration tests: slices
//! written from scratch (no PHI, no source file) whose pixels sample an analytic phantom at each
//! pixel's true patient position, so a test knows exactly what every voxel should hold.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use dcmnorm_core::header::Length;
use dcmnorm_core::value::DataSetSequence;
use dcmnorm_core::{DataElement, PrimitiveValue, Tag, VR};
use dcmnorm_dictionary::{tags, uids};
use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder, InMemDicomObject};

use super::io::write_dicom_file;
use super::volume_export::generate_uid;

pub(crate) type Vec3 = [f64; 3];

/// The geometry of one synthetic stack: slice `k` has `ImagePositionPatient = first_position +
/// offsets[k]` (any direction, so a gantry tilt is an offset with an in-plane component).
pub(crate) struct SyntheticStack {
    pub rows: u16,
    pub cols: u16,
    pub row_vector: Vec3,
    pub col_vector: Vec3,
    /// `PixelSpacing` order: [row spacing (along col_vector), column spacing (along row_vector)].
    pub pixel_spacing: [f64; 2],
    pub positions: Vec<Vec3>,
    pub modality: &'static str,
    pub frame_of_reference_uid: String,
    pub study_instance_uid: String,
    pub series_instance_uid: String,
}

fn ds(values: &[f64]) -> PrimitiveValue {
    PrimitiveValue::Strs(values.iter().map(|value| format!("{value:.6}")).collect::<Vec<_>>().into())
}

fn text(value: &str) -> PrimitiveValue {
    PrimitiveValue::Strs(vec![value.to_owned()].into())
}

pub(crate) fn unique_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("dcmnorm-{prefix}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes one file per slice into a fresh temp directory and returns the paths, in REVERSE
/// slice order (so `build_volume`'s spatial sort is exercised). `phantom(world)` gives the value
/// stored at each pixel (rounded to int16, RescaleSlope 1, RescaleIntercept 0).
pub(crate) fn write_synthetic_stack(stack: &SyntheticStack, phantom: impl Fn(Vec3) -> f64) -> Vec<PathBuf> {
    let dir = unique_dir("synthetic-stack");
    let mut paths = Vec::with_capacity(stack.positions.len());
    for (index, position) in stack.positions.iter().enumerate().rev() {
        let mut pixels = Vec::with_capacity(stack.rows as usize * stack.cols as usize);
        for row in 0..stack.rows as usize {
            for col in 0..stack.cols as usize {
                let world = [0, 1, 2].map(|axis| {
                    position[axis]
                        + stack.row_vector[axis] * col as f64 * stack.pixel_spacing[1]
                        + stack.col_vector[axis] * row as f64 * stack.pixel_spacing[0]
                });
                let value = phantom(world).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                pixels.push(value as u16);
            }
        }
        let sop_instance_uid = generate_uid();
        let sop_class = match stack.modality {
            "MR" => uids::MR_IMAGE_STORAGE,
            "PT" => uids::POSITRON_EMISSION_TOMOGRAPHY_IMAGE_STORAGE,
            _ => uids::CT_IMAGE_STORAGE,
        };
        let meta = FileMetaTableBuilder::new()
            .media_storage_sop_class_uid(sop_class)
            .media_storage_sop_instance_uid(sop_instance_uid.clone())
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .build()
            .unwrap();
        let mut object = DefaultDicomObject::new_empty_with_meta(meta);
        object.put(DataElement::new(tags::SOP_CLASS_UID, VR::UI, text(sop_class)));
        object.put(DataElement::new(tags::SOP_INSTANCE_UID, VR::UI, text(&sop_instance_uid)));
        object.put(DataElement::new(tags::PATIENT_NAME, VR::PN, text("SYNTHETIC^PHANTOM")));
        object.put(DataElement::new(tags::PATIENT_ID, VR::LO, text("SYN-0001")));
        object.put(DataElement::new(tags::PATIENT_BIRTH_DATE, VR::DA, text("19700101")));
        object.put(DataElement::new(tags::PATIENT_SEX, VR::CS, text("O")));
        object.put(DataElement::new(tags::STUDY_INSTANCE_UID, VR::UI, text(&stack.study_instance_uid)));
        object.put(DataElement::new(tags::STUDY_DATE, VR::DA, text("20260101")));
        object.put(DataElement::new(tags::STUDY_TIME, VR::TM, text("120000")));
        object.put(DataElement::new(tags::ACCESSION_NUMBER, VR::SH, text("SYNACC1")));
        object.put(DataElement::new(tags::STUDY_ID, VR::SH, text("1")));
        object.put(DataElement::new(tags::SERIES_INSTANCE_UID, VR::UI, text(&stack.series_instance_uid)));
        object.put(DataElement::new(tags::FRAME_OF_REFERENCE_UID, VR::UI, text(&stack.frame_of_reference_uid)));
        object.put(DataElement::new(tags::MODALITY, VR::CS, text(stack.modality)));
        object.put(DataElement::new(tags::INSTANCE_NUMBER, VR::IS, text(&(index + 1).to_string())));
        object.put(DataElement::new(tags::IMAGE_POSITION_PATIENT, VR::DS, ds(position)));
        let iop = [
            stack.row_vector[0], stack.row_vector[1], stack.row_vector[2],
            stack.col_vector[0], stack.col_vector[1], stack.col_vector[2],
        ];
        object.put(DataElement::new(tags::IMAGE_ORIENTATION_PATIENT, VR::DS, ds(&iop)));
        object.put(DataElement::new(tags::PIXEL_SPACING, VR::DS, ds(&stack.pixel_spacing)));
        object.put(DataElement::new(tags::ROWS, VR::US, PrimitiveValue::U16(vec![stack.rows].into())));
        object.put(DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::U16(vec![stack.cols].into())));
        object.put(DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::U16(vec![1u16].into())));
        object.put(DataElement::new(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, text("MONOCHROME2")));
        object.put(DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::U16(vec![16u16].into())));
        object.put(DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::U16(vec![16u16].into())));
        object.put(DataElement::new(tags::HIGH_BIT, VR::US, PrimitiveValue::U16(vec![15u16].into())));
        object.put(DataElement::new(tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::U16(vec![1u16].into())));
        object.put(DataElement::new(tags::RESCALE_SLOPE, VR::DS, ds(&[1.0])));
        object.put(DataElement::new(tags::RESCALE_INTERCEPT, VR::DS, ds(&[0.0])));
        object.put(DataElement::new(tags::PIXEL_DATA, VR::OW, PrimitiveValue::U16(pixels.into())));
        let path = dir.join(format!("slice-{index:04}.dcm"));
        write_dicom_file(&mut object, &path).unwrap();
        paths.push(path);
    }
    paths
}

pub(crate) fn remove_stack(paths: &[PathBuf]) {
    if let Some(dir) = paths.first().and_then(|path| path.parent()) {
        std::fs::remove_dir_all(dir).ok();
    }
}

fn sequence(tag: Tag, items: Vec<InMemDicomObject>) -> DataElement<InMemDicomObject> {
    DataElement::new(tag, VR::SQ, DataSetSequence::new(items, Length::UNDEFINED))
}

fn item(elements: Vec<DataElement<InMemDicomObject>>) -> InMemDicomObject {
    InMemDicomObject::from_element_iter(elements)
}

/// Writes `stack` as ONE Enhanced CT/MR/PET object (by `stack.modality`): orientation and pixel
/// measures in the Shared Functional Groups, each frame's position and its own rescale
/// (slope `1 + k / 8`, intercept `-50 k`, so a reader that ignores the per-frame Pixel Value
/// Transformation gets visibly wrong values) in the Per-frame Functional Groups. Frames are
/// stored in `frame_order` (indices into `stack.positions`), so sorting is exercised.
pub(crate) fn write_synthetic_enhanced(stack: &SyntheticStack, frame_order: &[usize], phantom: impl Fn(Vec3) -> f64) -> PathBuf {
    let dir = unique_dir("synthetic-enhanced");
    let frame_rescale = |k: usize| (1.0 + k as f64 / 8.0, -50.0 * k as f64);
    let mut pixels: Vec<u16> = Vec::new();
    let mut per_frame = Vec::new();
    for (stored, &k) in frame_order.iter().enumerate() {
        let position = stack.positions[k];
        let (slope, intercept) = frame_rescale(stored);
        for row in 0..stack.rows as usize {
            for col in 0..stack.cols as usize {
                let world = [0, 1, 2].map(|axis| {
                    position[axis]
                        + stack.row_vector[axis] * col as f64 * stack.pixel_spacing[1]
                        + stack.col_vector[axis] * row as f64 * stack.pixel_spacing[0]
                });
                let stored_value = ((phantom(world) - intercept) / slope).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                pixels.push(stored_value as u16);
            }
        }
        per_frame.push(item(vec![
            sequence(tags::PLANE_POSITION_SEQUENCE, vec![item(vec![DataElement::new(tags::IMAGE_POSITION_PATIENT, VR::DS, ds(&position))])]),
            sequence(
                tags::PIXEL_VALUE_TRANSFORMATION_SEQUENCE,
                vec![item(vec![
                    DataElement::new(tags::RESCALE_SLOPE, VR::DS, ds(&[slope])),
                    DataElement::new(tags::RESCALE_INTERCEPT, VR::DS, ds(&[intercept])),
                    DataElement::new(tags::RESCALE_TYPE, VR::LO, text(if stack.modality == "CT" { "HU" } else { "US" })),
                ])],
            ),
        ]));
    }
    let iop = [
        stack.row_vector[0], stack.row_vector[1], stack.row_vector[2],
        stack.col_vector[0], stack.col_vector[1], stack.col_vector[2],
    ];
    let shared = item(vec![
        sequence(tags::PLANE_ORIENTATION_SEQUENCE, vec![item(vec![DataElement::new(tags::IMAGE_ORIENTATION_PATIENT, VR::DS, ds(&iop))])]),
        sequence(
            tags::PIXEL_MEASURES_SEQUENCE,
            vec![item(vec![
                DataElement::new(tags::PIXEL_SPACING, VR::DS, ds(&stack.pixel_spacing)),
                DataElement::new(tags::SLICE_THICKNESS, VR::DS, ds(&[1.0])),
            ])],
        ),
    ]);
    let sop_class = match stack.modality {
        "MR" => uids::ENHANCED_MR_IMAGE_STORAGE,
        "PT" => uids::ENHANCED_PET_IMAGE_STORAGE,
        _ => uids::ENHANCED_CT_IMAGE_STORAGE,
    };
    let sop_instance_uid = generate_uid();
    let meta = FileMetaTableBuilder::new()
        .media_storage_sop_class_uid(sop_class)
        .media_storage_sop_instance_uid(sop_instance_uid.clone())
        .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .build()
        .unwrap();
    let mut object = DefaultDicomObject::new_empty_with_meta(meta);
    object.put(DataElement::new(tags::SOP_CLASS_UID, VR::UI, text(sop_class)));
    object.put(DataElement::new(tags::SOP_INSTANCE_UID, VR::UI, text(&sop_instance_uid)));
    object.put(DataElement::new(tags::PATIENT_NAME, VR::PN, text("SYNTHETIC^PHANTOM")));
    object.put(DataElement::new(tags::PATIENT_ID, VR::LO, text("SYN-0001")));
    object.put(DataElement::new(tags::STUDY_INSTANCE_UID, VR::UI, text(&stack.study_instance_uid)));
    object.put(DataElement::new(tags::SERIES_INSTANCE_UID, VR::UI, text(&stack.series_instance_uid)));
    object.put(DataElement::new(tags::FRAME_OF_REFERENCE_UID, VR::UI, text(&stack.frame_of_reference_uid)));
    object.put(DataElement::new(tags::MODALITY, VR::CS, text(stack.modality)));
    object.put(DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, text(&frame_order.len().to_string())));
    object.put(DataElement::new(tags::ROWS, VR::US, PrimitiveValue::U16(vec![stack.rows].into())));
    object.put(DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::U16(vec![stack.cols].into())));
    object.put(DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::U16(vec![1u16].into())));
    object.put(DataElement::new(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, text("MONOCHROME2")));
    object.put(DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::U16(vec![16u16].into())));
    object.put(DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::U16(vec![16u16].into())));
    object.put(DataElement::new(tags::HIGH_BIT, VR::US, PrimitiveValue::U16(vec![15u16].into())));
    object.put(DataElement::new(tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::U16(vec![1u16].into())));
    object.put(sequence(tags::SHARED_FUNCTIONAL_GROUPS_SEQUENCE, vec![shared]));
    object.put(sequence(tags::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE, per_frame));
    object.put(DataElement::new(tags::PIXEL_DATA, VR::OW, PrimitiveValue::U16(pixels.into())));
    let path = dir.join("enhanced.dcm");
    write_dicom_file(&mut object, &path).unwrap();
    path
}
