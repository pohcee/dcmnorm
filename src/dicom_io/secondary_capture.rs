//! Wraps a raw reformatted MPR plane (see `super::volume::reformat_plane_values`) into a valid,
//! spatially-correct single-frame DICOM object - the multi-file `--mpr ... out.dcm` CLI output.
//!
//! Uses **Multi-frame Grayscale Word Secondary Capture Image Storage**
//! (`1.2.840.10008.5.1.4.1.1.7.3`, `NumberOfFrames=1` per file) rather than plain "Secondary
//! Capture Image Storage" (`...7`), which is 8-bit-only per its IOD and can't carry the signed
//! 16-bit rescaled (e.g. Hounsfield unit) values a reformatted CT/MR plane actually has.

use std::error::Error;
use std::fmt;
use std::path::Path;

use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_object::{DefaultDicomObject, FileMetaTableBuilder};

use super::io::write_dicom_file;
use super::types::WriteError;
use super::volume_export::generate_uid;

/// Multi-frame Grayscale Word Secondary Capture Image Storage - see module docs for why this SOP
/// class (not plain Secondary Capture) is used.
pub const REFORMATTED_SLICE_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.7.3";

#[derive(Debug)]
pub enum SecondaryCaptureError {
    Write(WriteError),
    Meta(String),
    InvalidDimensions,
    SampleCountMismatch { expected: usize, found: usize },
}

impl fmt::Display for SecondaryCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Write(error) => write!(formatter, "failed to write reformatted DICOM slice: {error}"),
            Self::Meta(message) => write!(formatter, "failed to build DICOM file meta for reformatted slice: {message}"),
            Self::InvalidDimensions => write!(formatter, "reformatted DICOM slice dimensions must be non-zero"),
            Self::SampleCountMismatch { expected, found } => write!(
                formatter,
                "reformatted DICOM slice sample count ({found}) does not match its declared dimensions ({expected} expected)"
            ),
        }
    }
}

impl Error for SecondaryCaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Write(error) => Some(error),
            _ => None,
        }
    }
}

/// The per-slice spatial geometry written into the output DICOM object - `position` is
/// `ImagePositionPatient` (center of voxel (0,0), LPS mm), `row_dir`/`col_dir` become
/// `ImageOrientationPatient`, `row_spacing_mm`/`col_spacing_mm` become `PixelSpacing` (in DICOM's
/// own `[row_spacing, column_spacing]` order).
#[derive(Clone, Copy, Debug)]
pub struct SliceGeometry {
    pub position: [f64; 3],
    pub row_dir: [f64; 3],
    pub col_dir: [f64; 3],
    pub row_spacing_mm: f64,
    pub col_spacing_mm: f64,
    pub slice_thickness_mm: f64,
}

fn ds(values: &[f64]) -> PrimitiveValue {
    PrimitiveValue::Strs(values.iter().map(|value| format!("{value:.9}")).collect::<Vec<_>>().into())
}

fn copy_str_attribute(dest: &mut DefaultDicomObject, source: &DefaultDicomObject, tag: Tag, vr: VR) {
    if let Some(value) = source.get(tag).and_then(|element| element.to_str().ok()) {
        dest.put(DataElement::new(tag, vr, PrimitiveValue::Strs(vec![value.into_owned()].into())));
    }
}

/// Writes one reformatted MPR plane as a standalone, spatially-valid DICOM object. `samples` is
/// raw rescaled values (row-major, `rows * cols` long, same convention as
/// `reformat_plane_values`) - stored as signed 16-bit (`RescaleSlope=1`/`RescaleIntercept=0`, so
/// the stored value IS the physical value, e.g. Hounsfield units) rather than a windowed/leveled
/// 8-bit render, so any DICOM viewer can window it however it likes.
///
/// `series_instance_uid` should be the SAME value across every slice of one CLI invocation's
/// output stack, so viewers group them as one series; `instance_number` should be unique and
/// ascending within that stack (1-based, matching the depth order).
#[allow(clippy::too_many_arguments)]
pub fn write_reformatted_dicom_slice(
    source_object: &DefaultDicomObject,
    samples: &[f32],
    rows: u32,
    cols: u32,
    series_instance_uid: &str,
    instance_number: u32,
    geometry: &SliceGeometry,
    window_center: Option<f64>,
    window_width: Option<f64>,
    path: &Path,
) -> Result<(), SecondaryCaptureError> {
    if rows == 0 || cols == 0 {
        return Err(SecondaryCaptureError::InvalidDimensions);
    }
    let expected = rows as usize * cols as usize;
    if samples.len() != expected {
        return Err(SecondaryCaptureError::SampleCountMismatch { expected, found: samples.len() });
    }

    let sop_instance_uid = generate_uid();
    let meta = FileMetaTableBuilder::new()
        .media_storage_sop_class_uid(REFORMATTED_SLICE_SOP_CLASS_UID)
        .media_storage_sop_instance_uid(sop_instance_uid.clone())
        .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
        .build()
        .map_err(|error| SecondaryCaptureError::Meta(error.to_string()))?;

    let mut object = DefaultDicomObject::new_empty_with_meta(meta);

    // Best-effort: associate the derived series with the original study/patient rather than
    // orphaning it. Missing source attributes are simply skipped (not an error) - a reformatted
    // slice is still valid without, say, a PatientBirthDate.
    for (tag, vr) in [
        (tags::PATIENT_NAME, VR::PN),
        (tags::PATIENT_ID, VR::LO),
        (tags::PATIENT_BIRTH_DATE, VR::DA),
        (tags::PATIENT_SEX, VR::CS),
        (tags::STUDY_INSTANCE_UID, VR::UI),
        (tags::STUDY_DATE, VR::DA),
        (tags::STUDY_TIME, VR::TM),
        (tags::STUDY_DESCRIPTION, VR::LO),
        (tags::ACCESSION_NUMBER, VR::SH),
        (tags::MODALITY, VR::CS),
    ] {
        copy_str_attribute(&mut object, source_object, tag, vr);
    }

    object.put(DataElement::new(tags::SOP_CLASS_UID, VR::UI, PrimitiveValue::Strs(vec![REFORMATTED_SLICE_SOP_CLASS_UID.to_string()].into())));
    object.put(DataElement::new(tags::SOP_INSTANCE_UID, VR::UI, PrimitiveValue::Strs(vec![sop_instance_uid].into())));
    object.put(DataElement::new(tags::SERIES_INSTANCE_UID, VR::UI, PrimitiveValue::Strs(vec![series_instance_uid.to_string()].into())));
    object.put(DataElement::new(tags::SERIES_DESCRIPTION, VR::LO, PrimitiveValue::Strs(vec!["MPR Reformat".to_string()].into())));
    // High fixed series number, deliberately unlikely to collide with a real acquired series.
    object.put(DataElement::new(tags::SERIES_NUMBER, VR::IS, PrimitiveValue::Strs(vec!["9901".to_string()].into())));
    object.put(DataElement::new(tags::INSTANCE_NUMBER, VR::IS, PrimitiveValue::Strs(vec![instance_number.to_string()].into())));
    object.put(DataElement::new(tags::NUMBER_OF_FRAMES, VR::IS, PrimitiveValue::Strs(vec!["1".to_string()].into())));

    object.put(DataElement::new(tags::ROWS, VR::US, PrimitiveValue::U16(vec![rows as u16].into())));
    object.put(DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::U16(vec![cols as u16].into())));
    object.put(DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::U16(vec![1u16].into())));
    object.put(DataElement::new(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, PrimitiveValue::Strs(vec!["MONOCHROME2".to_string()].into())));
    object.put(DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::U16(vec![16u16].into())));
    object.put(DataElement::new(tags::BITS_STORED, VR::US, PrimitiveValue::U16(vec![16u16].into())));
    object.put(DataElement::new(tags::HIGH_BIT, VR::US, PrimitiveValue::U16(vec![15u16].into())));
    object.put(DataElement::new(tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::U16(vec![1u16].into())));

    object.put(DataElement::new(tags::IMAGE_POSITION_PATIENT, VR::DS, ds(&geometry.position)));
    object.put(DataElement::new(
        tags::IMAGE_ORIENTATION_PATIENT,
        VR::DS,
        ds(&[
            geometry.row_dir[0], geometry.row_dir[1], geometry.row_dir[2],
            geometry.col_dir[0], geometry.col_dir[1], geometry.col_dir[2],
        ]),
    ));
    // DICOM's own order: [row spacing, column spacing].
    object.put(DataElement::new(tags::PIXEL_SPACING, VR::DS, ds(&[geometry.row_spacing_mm, geometry.col_spacing_mm])));
    object.put(DataElement::new(tags::SLICE_THICKNESS, VR::DS, ds(&[geometry.slice_thickness_mm])));

    // Stored value == the physical (e.g. Hounsfield unit) value directly - no separate rescale
    // needed since `samples` already holds true rescaled values (see `reformat_plane_values`).
    object.put(DataElement::new(tags::RESCALE_SLOPE, VR::DS, ds(&[1.0])));
    object.put(DataElement::new(tags::RESCALE_INTERCEPT, VR::DS, ds(&[0.0])));

    if let (Some(center), Some(width)) = (window_center, window_width) {
        object.put(DataElement::new(tags::WINDOW_CENTER, VR::DS, ds(&[center])));
        object.put(DataElement::new(tags::WINDOW_WIDTH, VR::DS, ds(&[width])));
    }

    let pixel_words: Vec<u16> = samples
        .iter()
        .map(|value| value.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16 as u16)
        .collect();
    object.put(DataElement::new(tags::PIXEL_DATA, VR::OW, PrimitiveValue::U16(pixel_words.into())));

    write_dicom_file(&mut object, path).map_err(SecondaryCaptureError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::io::read_dicom_file;
    use std::fs;
    use std::path::PathBuf;

    fn ct_fixture_object() -> DefaultDicomObject {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files/ct.dcm");
        read_dicom_file(&path).expect("test/files/ct.dcm fixture should be readable")
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("dcmnorm-secondary-capture-test-{name}-{}.dcm", std::process::id()));
        path
    }

    fn test_geometry() -> SliceGeometry {
        SliceGeometry {
            position: [1.5, -2.5, 3.5],
            row_dir: [1.0, 0.0, 0.0],
            col_dir: [0.0, 1.0, 0.0],
            row_spacing_mm: 0.7,
            col_spacing_mm: 0.8,
            slice_thickness_mm: 2.5,
        }
    }

    #[test]
    fn rejects_a_sample_count_mismatch() {
        let source = ct_fixture_object();
        let path = temp_path("mismatch");
        let error = write_reformatted_dicom_slice(&source, &[0.0; 3], 2, 2, "1.2.3", 1, &test_geometry(), None, None, &path).unwrap_err();
        assert!(matches!(error, SecondaryCaptureError::SampleCountMismatch { expected: 4, found: 3 }));
    }

    #[test]
    fn round_trips_sop_class_geometry_and_pixel_data_through_the_dicom_reader() {
        let source = ct_fixture_object();
        let path = temp_path("round-trip");
        let rows = 3u32;
        let cols = 4u32;
        let samples: Vec<f32> = (0..(rows * cols)).map(|index| index as f32 * 10.0 - 15.0).collect();

        write_reformatted_dicom_slice(
            &source,
            &samples,
            rows,
            cols,
            "1.2.826.0.1.3680043.test.series",
            7,
            &test_geometry(),
            Some(40.0),
            Some(400.0),
            &path,
        )
        .unwrap();

        let read_back = read_dicom_file(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(read_back.meta().media_storage_sop_class_uid(), REFORMATTED_SLICE_SOP_CLASS_UID);
        assert_eq!(
            read_back.element(tags::SERIES_INSTANCE_UID).unwrap().to_str().unwrap(),
            "1.2.826.0.1.3680043.test.series"
        );
        assert_eq!(read_back.element(tags::INSTANCE_NUMBER).unwrap().to_str().unwrap(), "7");
        assert_eq!(read_back.element(tags::ROWS).unwrap().uint16().unwrap(), rows as u16);
        assert_eq!(read_back.element(tags::COLUMNS).unwrap().uint16().unwrap(), cols as u16);
        assert_eq!(read_back.element(tags::BITS_ALLOCATED).unwrap().uint16().unwrap(), 16);
        assert_eq!(read_back.element(tags::PIXEL_REPRESENTATION).unwrap().uint16().unwrap(), 1);

        let position_text = read_back.element(tags::IMAGE_POSITION_PATIENT).unwrap().to_str().unwrap();
        let position: Vec<f64> = position_text.split('\\').map(|s| s.parse().unwrap()).collect();
        assert!((position[0] - 1.5).abs() < 1e-6);
        assert!((position[1] - (-2.5)).abs() < 1e-6);
        assert!((position[2] - 3.5).abs() < 1e-6);

        assert_eq!(read_back.element(tags::RESCALE_SLOPE).unwrap().to_str().unwrap(), "1.000000000");
        assert_eq!(read_back.element(tags::RESCALE_INTERCEPT).unwrap().to_str().unwrap(), "0.000000000");
        assert_eq!(read_back.element(tags::WINDOW_CENTER).unwrap().to_str().unwrap(), "40.000000000");

        // Patient-level attributes copied best-effort from the source object.
        if let Some(source_patient_id) = source.get(tags::PATIENT_ID).and_then(|element| element.to_str().ok()) {
            let read_patient_id = read_back.element(tags::PATIENT_ID).unwrap().to_str().unwrap();
            assert_eq!(read_patient_id, source_patient_id);
        }

        // Compare as the exact stored 16-bit WORDS (not reinterpreted as signed) - PixelData is
        // stored via PrimitiveValue::U16 with PixelRepresentation=1 marking it signed, the same
        // "raw words + a separate signedness flag" split every DICOM reader relies on.
        let stored: Vec<u16> = read_back.element(tags::PIXEL_DATA).unwrap().to_multi_int::<u16>().unwrap();
        let expected: Vec<u16> = samples.iter().map(|value| value.round() as i16 as u16).collect();
        assert_eq!(stored, expected);
    }
}
