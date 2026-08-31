//! `--check-dicom-logic` support: cheap, decode-free checks that a DICOM object's *declared*
//! metadata is internally consistent - as opposed to `probe_dicom_file_for_sop_class_uid`
//! (`--check-dicom`), which only checks that the bytes parse as DICOM at all. Every rule here
//! reads already-parsed header/dataset values (and, for encapsulated pixel data, peeks the first
//! few bytes of the first fragment) - none of them run an actual pixel codec, so this stays cheap
//! enough for a full-archive batch scan.
//!
//! The relationships checked mirror ones this crate already enforces reactively, deep inside
//! `render`/`io`/`jpeg_ls`/`jpeg_xl`/`mpeg`/the transcode adapters, as differently-shaped errors
//! raised only once someone actually tries to decode or transcode a frame. Centralizing the
//! structural subset here lets a caller find out up front, before committing to that expensive
//! path, and gives archive-audit / ingestion-gate tooling one structured report to consume instead
//! of catching decode failures after the fact.

use dcmnorm_core::Tag;
use dcmnorm_dictionary::{tags, uids};
use dcmnorm_object::DefaultDicomObject;

use super::io::{is_jpeg2000_transfer_syntax, normalize_transfer_syntax_uid};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Severity {
    /// Will plausibly break downstream decompress/render/routing - the thing
    /// `--check-dicom-logic` exists to surface up front.
    Error,
    /// Suspicious, or non-conformant in a way that is usually survivable - worth a human look,
    /// but not on by default as a failure (see `--strict`).
    Warning,
}

#[derive(Clone, Debug)]
pub struct ConsistencyFinding {
    pub severity: Severity,
    /// Stable, greppable identifier for the rule that produced this finding, e.g.
    /// `"pixel/bits-stored-exceeds-allocated"`.
    pub rule_id: &'static str,
    pub message: String,
    /// Tags implicated in this finding, for follow-up (e.g. with `--filter`).
    pub tags: Vec<Tag>,
}

#[derive(Clone, Debug, Default)]
pub struct ConsistencyReport {
    pub findings: Vec<ConsistencyFinding>,
}

impl ConsistencyReport {
    pub fn error_count(&self) -> usize {
        self.findings.iter().filter(|finding| finding.severity == Severity::Error).count()
    }

    pub fn warning_count(&self) -> usize {
        self.findings.iter().filter(|finding| finding.severity == Severity::Warning).count()
    }

    pub fn has_errors(&self) -> bool {
        self.error_count() > 0
    }
}

/// Runs every consistency rule against `object` and returns the combined report. Always
/// succeeds - a file that's missing everything just produces a report full of `Error` findings,
/// which is the point (see the module doc).
pub fn check_dicom_logic(object: &DefaultDicomObject) -> ConsistencyReport {
    let mut findings = Vec::new();
    check_uid_sanity(object, &mut findings);
    check_pixel_structure(object, &mut findings);
    check_geometry(object, &mut findings);
    check_modality(object, &mut findings);
    ConsistencyReport { findings }
}

fn push(
    findings: &mut Vec<ConsistencyFinding>,
    severity: Severity,
    rule_id: &'static str,
    message: String,
    tags: Vec<Tag>,
) {
    findings.push(ConsistencyFinding { severity, rule_id, message, tags });
}

// ---- shared tag-reading helpers -------------------------------------------------------------
//
// These deliberately read values the same way the rest of this crate's CLI/rendering code
// already does (`to_str()` + manual backslash split for multi-valued DS/IS text, see
// `numeric_values_tag` in exec/dcmnorm/src/main.rs) rather than `to_multi_float64` - that keeps a
// malformed numeric component from turning into a hard read error, which matters here since the
// whole point is inspecting possibly-broken files.

fn text(object: &DefaultDicomObject, tag: Tag) -> Option<String> {
    let value = object.get(tag)?.to_str().ok()?.trim().to_owned();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn u16_value(object: &DefaultDicomObject, tag: Tag) -> Option<u16> {
    object.get(tag).and_then(|element| element.uint16().ok())
}

fn multi_f64(object: &DefaultDicomObject, tag: Tag) -> Option<Vec<f64>> {
    let raw = text(object, tag)?;
    let values: Vec<f64> = raw.split('\\').filter_map(|part| part.trim().parse().ok()).collect();
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn value_multiplicity(object: &DefaultDicomObject, tag: Tag) -> Option<usize> {
    let raw = text(object, tag)?;
    Some(raw.split('\\').count())
}

// ---- UID sanity --------------------------------------------------------------------------

const UID_TAGS: &[(Tag, &str)] = &[
    (tags::SOP_CLASS_UID, "SOPClassUID"),
    (tags::SOP_INSTANCE_UID, "SOPInstanceUID"),
    (tags::STUDY_INSTANCE_UID, "StudyInstanceUID"),
    (tags::SERIES_INSTANCE_UID, "SeriesInstanceUID"),
];

/// A syntactically valid DICOM UID (PS3.5 sec 9): dot-separated components, each all-digits, no
/// component with a leading zero unless the component is exactly `"0"`, at most 64 characters
/// total. Doesn't check the UID is *registered* to anyone - just that it isn't garbage (a common,
/// cheap-to-catch symptom of a corrupt or hand-edited file - see 5024468's
/// ImplementationClassUID fix, which this generalizes to any UID in any incoming file).
fn is_valid_uid(uid: &str) -> bool {
    if uid.is_empty() || uid.len() > 64 {
        return false;
    }
    let mut saw_component = false;
    for component in uid.split('.') {
        saw_component = true;
        if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        if component.len() > 1 && component.starts_with('0') {
            return false;
        }
    }
    saw_component
}

fn check_uid_sanity(object: &DefaultDicomObject, findings: &mut Vec<ConsistencyFinding>) {
    for &(tag, name) in UID_TAGS {
        if let Some(value) = text(object, tag) {
            if !is_valid_uid(&value) {
                push(
                    findings,
                    Severity::Error,
                    "uid/malformed-uid",
                    format!("{name} {tag} is not a syntactically valid UID: {value:?}"),
                    vec![tag],
                );
            }
        }
    }

    let meta = object.meta();
    let transfer_syntax_uid = meta.transfer_syntax().trim_end_matches(['\0', ' ']);
    if !transfer_syntax_uid.is_empty() {
        if !is_valid_uid(transfer_syntax_uid) {
            push(
                findings,
                Severity::Error,
                "uid/malformed-uid",
                format!(
                    "TransferSyntaxUID {} is not a syntactically valid UID: {transfer_syntax_uid:?}",
                    tags::TRANSFER_SYNTAX_UID
                ),
                vec![tags::TRANSFER_SYNTAX_UID],
            );
        } else if meta.transfer_syntax_ts().is_none() {
            push(
                findings,
                Severity::Warning,
                "uid/unknown-transfer-syntax",
                format!(
                    "TransferSyntaxUID {transfer_syntax_uid} is not a transfer syntax this build recognizes \
                     (private/vendor syntax, or corrupt)"
                ),
                vec![tags::TRANSFER_SYNTAX_UID],
            );
        }
    }

    let meta_sop_class = meta.media_storage_sop_class_uid().trim_end_matches(['\0', ' ']);
    if let (Some(dataset_sop_class), false) = (text(object, tags::SOP_CLASS_UID), meta_sop_class.is_empty()) {
        if dataset_sop_class != meta_sop_class {
            push(
                findings,
                Severity::Warning,
                "uid/sop-class-uid-meta-mismatch",
                format!(
                    "dataset SOPClassUID ({dataset_sop_class}) does not match file meta \
                     MediaStorageSOPClassUID ({meta_sop_class}) - PACS routing depends on these agreeing"
                ),
                vec![tags::SOP_CLASS_UID, tags::MEDIA_STORAGE_SOP_CLASS_UID],
            );
        }
    }

    let meta_sop_instance = meta.media_storage_sop_instance_uid().trim_end_matches(['\0', ' ']);
    if let (Some(dataset_sop_instance), false) =
        (text(object, tags::SOP_INSTANCE_UID), meta_sop_instance.is_empty())
    {
        if dataset_sop_instance != meta_sop_instance {
            push(
                findings,
                Severity::Warning,
                "uid/sop-instance-uid-meta-mismatch",
                format!(
                    "dataset SOPInstanceUID ({dataset_sop_instance}) does not match file meta \
                     MediaStorageSOPInstanceUID ({meta_sop_instance}) - PACS routing depends on these agreeing"
                ),
                vec![tags::SOP_INSTANCE_UID, tags::MEDIA_STORAGE_SOP_INSTANCE_UID],
            );
        }
    }
}

// ---- pixel structure ----------------------------------------------------------------------

/// JPEG Baseline/Extended/Lossless/JPEG-LS - all use a plain JPEG-style SOI (`FF D8`) at the
/// start of the first fragment, regardless of which specific JPEG process they carry. JPEG 2000
/// (checked separately via `is_jpeg2000_transfer_syntax`, including HTJ2K) uses a different
/// container and is NOT included here.
// Several of the hierarchical/progressive JPEG process UIDs below are marked deprecated in
// `dcmnorm_dictionary::uids` (retired by DICOM) - but retired doesn't mean nonexistent: old files
// using them are exactly the kind of thing this checker needs to recognize, not skip.
#[allow(deprecated)]
fn is_jpeg_or_jpegls_transfer_syntax(uid: &str) -> bool {
    matches!(
        normalize_transfer_syntax_uid(uid),
        uids::JPEG_BASELINE8_BIT
            | uids::JPEG_EXTENDED12_BIT
            | uids::JPEG_EXTENDED35
            | uids::JPEG_SPECTRAL_SELECTION_NON_HIERARCHICAL68
            | uids::JPEG_SPECTRAL_SELECTION_NON_HIERARCHICAL79
            | uids::JPEG_FULL_PROGRESSION_NON_HIERARCHICAL1012
            | uids::JPEG_FULL_PROGRESSION_NON_HIERARCHICAL1113
            | uids::JPEG_LOSSLESS
            | uids::JPEG_LOSSLESS_NON_HIERARCHICAL15
            | uids::JPEG_EXTENDED_HIERARCHICAL1618
            | uids::JPEG_EXTENDED_HIERARCHICAL1719
            | uids::JPEG_SPECTRAL_SELECTION_HIERARCHICAL2022
            | uids::JPEG_SPECTRAL_SELECTION_HIERARCHICAL2123
            | uids::JPEG_FULL_PROGRESSION_HIERARCHICAL2426
            | uids::JPEG_FULL_PROGRESSION_HIERARCHICAL2527
            | uids::JPEG_LOSSLESS_HIERARCHICAL28
            | uids::JPEG_LOSSLESS_HIERARCHICAL29
            | uids::JPEG_LOSSLESS_SV1
            | uids::JPEGLS_LOSSLESS
            | uids::JPEGLS_NEAR_LOSSLESS
    )
}

/// Cheap, decode-free check that the first pixel-data fragment's leading bytes actually look
/// like the codec the transfer syntax claims - no full codec run, just the container/marker a
/// real encoder always emits. `None` means "not a family this rule recognizes" (private/vendor
/// syntaxes, RLE's own header check happens separately, MPEG variants are skipped - out of scope
/// for now, see the design doc) - the caller treats that as "nothing to check", not a pass.
fn encapsulated_fragment_looks_valid(transfer_syntax_uid: &str, fragment: &[u8]) -> Option<bool> {
    if is_jpeg2000_transfer_syntax(transfer_syntax_uid) {
        const JP2_SIGNATURE_BOX: [u8; 8] = [0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20];
        let raw_codestream = fragment.len() >= 2 && fragment[0] == 0xFF && fragment[1] == 0x4F;
        let jp2_wrapped = fragment.len() >= JP2_SIGNATURE_BOX.len() && fragment[..8] == JP2_SIGNATURE_BOX;
        return Some(raw_codestream || jp2_wrapped);
    }
    if is_jpeg_or_jpegls_transfer_syntax(transfer_syntax_uid) {
        return Some(fragment.len() >= 2 && fragment[0] == 0xFF && fragment[1] == 0xD8);
    }
    if normalize_transfer_syntax_uid(transfer_syntax_uid) == uids::RLE_LOSSLESS {
        // PS3.5 Annex G: a 64-byte header starting with a little-endian u32 segment count in
        // 1..=15, followed by 15 little-endian u32 segment offsets. Checking just the count is
        // enough to catch "this obviously isn't an RLE fragment" without hand-rolling the rest.
        if fragment.len() < 4 {
            return Some(false);
        }
        let segment_count = u32::from_le_bytes([fragment[0], fragment[1], fragment[2], fragment[3]]);
        return Some((1..=15).contains(&segment_count));
    }
    None
}

fn check_pixel_structure(object: &DefaultDicomObject, findings: &mut Vec<ConsistencyFinding>) {
    let Some(pixel_data) = object.get(tags::PIXEL_DATA) else {
        // No classic PixelData element - either a non-image SOP class (SR, RTSTRUCT, ...) or one
        // using Float/DoubleFloat Pixel Data instead. Nothing in this section applies.
        return;
    };

    let required = [
        (tags::ROWS, "Rows"),
        (tags::COLUMNS, "Columns"),
        (tags::SAMPLES_PER_PIXEL, "SamplesPerPixel"),
        (tags::BITS_ALLOCATED, "BitsAllocated"),
        (tags::PHOTOMETRIC_INTERPRETATION, "PhotometricInterpretation"),
    ];
    let mut missing_required = false;
    for (tag, name) in required {
        if object.get(tag).is_none() {
            missing_required = true;
            push(
                findings,
                Severity::Error,
                "pixel/required-attribute-missing",
                format!("PixelData {} is present but {name} {tag} is missing", tags::PIXEL_DATA),
                vec![tags::PIXEL_DATA, tag],
            );
        }
    }
    // The rest of this section leans on Rows/Columns/SamplesPerPixel/BitsAllocated all being
    // present and numeric - bail out rather than let every remaining rule re-report the same gap
    // as a confusing type-conversion failure.
    if missing_required {
        return;
    }

    let rows = u16_value(object, tags::ROWS).unwrap();
    let cols = u16_value(object, tags::COLUMNS).unwrap();
    let samples_per_pixel = u16_value(object, tags::SAMPLES_PER_PIXEL).unwrap();
    let bits_allocated = u16_value(object, tags::BITS_ALLOCATED).unwrap();
    let photometric_interpretation = text(object, tags::PHOTOMETRIC_INTERPRETATION).unwrap().to_ascii_uppercase();
    let number_of_frames = text(object, tags::NUMBER_OF_FRAMES)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);

    if !matches!(bits_allocated, 1 | 8 | 16 | 32) {
        push(
            findings,
            Severity::Error,
            "pixel/bits-allocated-invalid",
            format!("BitsAllocated {} is {bits_allocated}, but PS3.5 only allows 1, 8, 16, or 32", tags::BITS_ALLOCATED),
            vec![tags::BITS_ALLOCATED],
        );
    }

    if let Some(bits_stored) = u16_value(object, tags::BITS_STORED) {
        if bits_stored == 0 || bits_stored > bits_allocated {
            push(
                findings,
                Severity::Error,
                "pixel/bits-stored-exceeds-allocated",
                format!(
                    "BitsStored ({bits_stored}) must be between 1 and BitsAllocated ({bits_allocated}) inclusive"
                ),
                vec![tags::BITS_STORED, tags::BITS_ALLOCATED],
            );
        }

        if let Some(high_bit) = u16_value(object, tags::HIGH_BIT) {
            let expected_high_bit = bits_stored.wrapping_sub(1);
            if high_bit != expected_high_bit {
                push(
                    findings,
                    Severity::Error,
                    "pixel/high-bit-mismatch",
                    format!(
                        "HighBit ({high_bit}) must equal BitsStored - 1 ({expected_high_bit}) per PS3.5 - a \
                         mismatch here means every consumer that trusts HighBit will read the wrong bits"
                    ),
                    vec![tags::HIGH_BIT, tags::BITS_STORED],
                );
            }
        }
    }

    let expected_samples = match photometric_interpretation.as_str() {
        "MONOCHROME1" | "MONOCHROME2" | "PALETTE COLOR" => Some(1u16),
        "RGB" | "YBR_FULL" | "YBR_FULL_422" | "YBR_PARTIAL_422" | "YBR_PARTIAL_420" | "YBR_RCT" | "YBR_ICT" => {
            Some(3u16)
        }
        _ => None,
    };
    if let Some(expected_samples) = expected_samples {
        if samples_per_pixel != expected_samples {
            push(
                findings,
                Severity::Error,
                "pixel/samples-per-pixel-photometric-mismatch",
                format!(
                    "PhotometricInterpretation {photometric_interpretation:?} requires SamplesPerPixel \
                     {expected_samples}, but SamplesPerPixel is {samples_per_pixel}"
                ),
                vec![tags::SAMPLES_PER_PIXEL, tags::PHOTOMETRIC_INTERPRETATION],
            );
        }
    }

    if samples_per_pixel == 1 {
        if let Some(planar_configuration) = u16_value(object, tags::PLANAR_CONFIGURATION) {
            if planar_configuration != 0 {
                push(
                    findings,
                    Severity::Warning,
                    "pixel/planar-configuration-on-monochrome",
                    format!(
                        "PlanarConfiguration is {planar_configuration} but SamplesPerPixel is 1 - \
                         PlanarConfiguration is meaningless for single-sample images"
                    ),
                    vec![tags::PLANAR_CONFIGURATION, tags::SAMPLES_PER_PIXEL],
                );
            }
        }
    }

    if photometric_interpretation == "PALETTE COLOR" {
        let lut_tags = [
            (tags::RED_PALETTE_COLOR_LOOKUP_TABLE_DATA, "RedPaletteColorLookupTableData"),
            (tags::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DATA, "GreenPaletteColorLookupTableData"),
            (tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DATA, "BluePaletteColorLookupTableData"),
        ];
        for (tag, name) in lut_tags {
            if object.get(tag).is_none() {
                push(
                    findings,
                    Severity::Error,
                    "pixel/palette-color-missing-lut",
                    format!("PhotometricInterpretation is PALETTE COLOR but {name} {tag} is missing"),
                    vec![tags::PHOTOMETRIC_INTERPRETATION, tag],
                );
            }
        }
    }

    match pixel_data.value().fragments() {
        Some(fragments) => {
            if fragments.len() < number_of_frames {
                push(
                    findings,
                    Severity::Error,
                    "pixel/encapsulated-fragment-count-too-low",
                    format!(
                        "NumberOfFrames declares {number_of_frames} frame(s), but PixelData only has \
                         {} encoded fragment(s) - there is no way to hold that many frames",
                        fragments.len()
                    ),
                    vec![tags::NUMBER_OF_FRAMES, tags::PIXEL_DATA],
                );
            }

            let transfer_syntax_uid = object.meta().transfer_syntax();
            if let Some(first_fragment) = fragments.iter().find(|fragment| !fragment.is_empty()) {
                if let Some(false) = encapsulated_fragment_looks_valid(transfer_syntax_uid, first_fragment) {
                    push(
                        findings,
                        Severity::Error,
                        "pixel/transfer-syntax-fragment-mismatch",
                        format!(
                            "TransferSyntaxUID {} claims a codec whose encoded frames don't start the way \
                             that codec always does - the file is very likely mislabeled and will fail or \
                             produce garbage on decode",
                            tags::TRANSFER_SYNTAX_UID
                        ),
                        vec![tags::TRANSFER_SYNTAX_UID, tags::PIXEL_DATA],
                    );
                }
            }
        }
        None => {
            // Native (uncompressed) encoding: PixelData is a primitive byte string whose length
            // is fully determined by Rows/Columns/NumberOfFrames/SamplesPerPixel/BitsAllocated -
            // a mismatch here means the file was hand-edited or generated wrong, not a rare
            // legitimate corner case.
            if let Ok(actual_bytes) = pixel_data.to_bytes() {
                let expected_bits = rows as u64 * cols as u64 * number_of_frames as u64
                    * samples_per_pixel as u64
                    * bits_allocated as u64;
                let expected_bytes = expected_bits.div_ceil(8);
                let expected_bytes_padded = expected_bytes + (expected_bytes % 2);
                if expected_bytes_padded > 0 && actual_bytes.len() as u64 != expected_bytes_padded {
                    push(
                        findings,
                        Severity::Error,
                        "pixel/native-length-mismatch",
                        format!(
                            "PixelData is {} byte(s), but Rows ({rows}) x Columns ({cols}) x \
                             NumberOfFrames ({number_of_frames}) x SamplesPerPixel ({samples_per_pixel}) x \
                             BitsAllocated ({bits_allocated}) implies {expected_bytes_padded} byte(s)",
                            actual_bytes.len()
                        ),
                        vec![
                            tags::PIXEL_DATA,
                            tags::ROWS,
                            tags::COLUMNS,
                            tags::NUMBER_OF_FRAMES,
                            tags::SAMPLES_PER_PIXEL,
                            tags::BITS_ALLOCATED,
                        ],
                    );
                }
            }
        }
    }
}

// ---- geometry (phase 2) --------------------------------------------------------------------

fn check_geometry(object: &DefaultDicomObject, findings: &mut Vec<ConsistencyFinding>) {
    if let Some(values) = multi_f64(object, tags::IMAGE_ORIENTATION_PATIENT) {
        if values.len() == 6 {
            let row = &values[0..3];
            let col = &values[3..6];
            let magnitude = |v: &[f64]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            let row_len = magnitude(row);
            let col_len = magnitude(col);
            const UNIT_TOLERANCE: f64 = 0.05;
            if (row_len - 1.0).abs() > UNIT_TOLERANCE || (col_len - 1.0).abs() > UNIT_TOLERANCE {
                push(
                    findings,
                    Severity::Warning,
                    "geometry/orientation-not-unit-vector",
                    format!(
                        "ImageOrientationPatient {} row/column vectors should be unit vectors, but have \
                         magnitude {row_len:.4}/{col_len:.4} - MPR and slice geometry math will be off",
                        tags::IMAGE_ORIENTATION_PATIENT
                    ),
                    vec![tags::IMAGE_ORIENTATION_PATIENT],
                );
            }

            let dot = row[0] * col[0] + row[1] * col[1] + row[2] * col[2];
            const ORTHOGONAL_TOLERANCE: f64 = 0.05;
            if dot.abs() > ORTHOGONAL_TOLERANCE {
                push(
                    findings,
                    Severity::Warning,
                    "geometry/orientation-not-orthogonal",
                    format!(
                        "ImageOrientationPatient {} row and column vectors should be orthogonal, but their \
                         dot product is {dot:.4}",
                        tags::IMAGE_ORIENTATION_PATIENT
                    ),
                    vec![tags::IMAGE_ORIENTATION_PATIENT],
                );
            }
        }
    }

    for (tag, name) in [(tags::PIXEL_SPACING, "PixelSpacing"), (tags::SLICE_THICKNESS, "SliceThickness")] {
        if let Some(values) = multi_f64(object, tag) {
            if values.iter().any(|value| *value <= 0.0) {
                push(
                    findings,
                    Severity::Error,
                    "geometry/non-positive-spacing",
                    format!("{name} {tag} has a non-positive component: {values:?}"),
                    vec![tag],
                );
            }
        }
    }
}

// ---- modality-specific (phase 2) -----------------------------------------------------------

fn check_modality(object: &DefaultDicomObject, findings: &mut Vec<ConsistencyFinding>) {
    if object.get(tags::PIXEL_DATA).is_some() && text(object, tags::MODALITY).as_deref() == Some("CT") {
        let has_slope = object.get(tags::RESCALE_SLOPE).is_some();
        let has_intercept = object.get(tags::RESCALE_INTERCEPT).is_some();
        if !has_slope || !has_intercept {
            push(
                findings,
                Severity::Warning,
                "modality/ct-missing-rescale",
                "Modality is CT but RescaleSlope/RescaleIntercept is missing - pixel values cannot be \
                 converted to Hounsfield units"
                    .to_owned(),
                vec![tags::RESCALE_SLOPE, tags::RESCALE_INTERCEPT],
            );
        }
    }

    if let (Some(center_vm), Some(width_vm)) =
        (value_multiplicity(object, tags::WINDOW_CENTER), value_multiplicity(object, tags::WINDOW_WIDTH))
    {
        if center_vm != width_vm {
            push(
                findings,
                Severity::Warning,
                "modality/window-vm-mismatch",
                format!(
                    "WindowCenter {} has {center_vm} value(s) but WindowWidth {} has {width_vm} - viewers \
                     pair them up by position, so a mismatched count leaves some presets undefined",
                    tags::WINDOW_CENTER,
                    tags::WINDOW_WIDTH
                ),
                vec![tags::WINDOW_CENTER, tags::WINDOW_WIDTH],
            );
        }

        if let Some(explanation_vm) = value_multiplicity(object, tags::WINDOW_CENTER_WIDTH_EXPLANATION) {
            if explanation_vm != center_vm {
                push(
                    findings,
                    Severity::Warning,
                    "modality/window-vm-mismatch",
                    format!(
                        "WindowCenterWidthExplanation {} has {explanation_vm} value(s) but WindowCenter {} \
                         has {center_vm} - explanations will be misaligned with their presets",
                        tags::WINDOW_CENTER_WIDTH_EXPLANATION,
                        tags::WINDOW_CENTER
                    ),
                    vec![tags::WINDOW_CENTER_WIDTH_EXPLANATION, tags::WINDOW_CENTER],
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcmnorm_core::header::Header;
    use dcmnorm_core::value::PixelFragmentSequence;
    use dcmnorm_core::DataElement;
    use dcmnorm_core::VR;
    use dcmnorm_object::{FileMetaTableBuilder, InMemDicomObject};
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/files").join(name)
    }

    fn object_with(elements: Vec<DataElement<InMemDicomObject>>, transfer_syntax_uid: &str) -> DefaultDicomObject {
        InMemDicomObject::from_element_iter(elements)
            .with_meta(
                FileMetaTableBuilder::new()
                    .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.7")
                    .media_storage_sop_instance_uid("1.2.3.4.5.6")
                    .transfer_syntax(transfer_syntax_uid),
            )
            .unwrap()
    }

    fn str_element(tag: Tag, vr: VR, value: &str) -> DataElement<InMemDicomObject> {
        DataElement::new(tag, vr, dcmnorm_core::PrimitiveValue::from(value))
    }

    fn u16_element(tag: Tag, vr: VR, value: u16) -> DataElement<InMemDicomObject> {
        DataElement::new(tag, vr, dcmnorm_core::PrimitiveValue::from(value))
    }

    /// A minimal, internally-consistent MONOCHROME2 image: 2x2, 8 bits, one frame - every pixel
    /// rule should be silent on this, so tests that add one deliberate defect can trust that any
    /// finding they see came from that defect, not baseline noise.
    fn baseline_image_elements() -> Vec<DataElement<InMemDicomObject>> {
        vec![
            str_element(tags::SOP_CLASS_UID, VR::UI, "1.2.840.10008.5.1.4.1.1.7"),
            str_element(tags::SOP_INSTANCE_UID, VR::UI, "1.2.3.4.5.6"),
            str_element(tags::STUDY_INSTANCE_UID, VR::UI, "1.2.3.4.5.7"),
            str_element(tags::SERIES_INSTANCE_UID, VR::UI, "1.2.3.4.5.8"),
            u16_element(tags::ROWS, VR::US, 2),
            u16_element(tags::COLUMNS, VR::US, 2),
            u16_element(tags::SAMPLES_PER_PIXEL, VR::US, 1),
            u16_element(tags::BITS_ALLOCATED, VR::US, 8),
            u16_element(tags::BITS_STORED, VR::US, 8),
            u16_element(tags::HIGH_BIT, VR::US, 7),
            u16_element(tags::PIXEL_REPRESENTATION, VR::US, 0),
            str_element(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2"),
            DataElement::new(tags::PIXEL_DATA, VR::OW, dcmnorm_core::PrimitiveValue::from(vec![0u8; 4])),
        ]
    }

    fn find<'a>(report: &'a ConsistencyReport, rule_id: &str) -> Option<&'a ConsistencyFinding> {
        report.findings.iter().find(|finding| finding.rule_id == rule_id)
    }

    #[test]
    fn uid_syntax_accepts_well_formed_uids_and_rejects_garbage() {
        assert!(is_valid_uid("1.2.840.10008.1.2"));
        assert!(is_valid_uid("0.1.2"));
        assert!(!is_valid_uid(""));
        assert!(!is_valid_uid("1..2"));
        assert!(!is_valid_uid("1.02.3"), "a component with a leading zero (other than a bare 0) is invalid");
        assert!(!is_valid_uid("1.2.a"));
        assert!(!is_valid_uid(&format!("1.{}", "2".repeat(64)), ));
    }

    #[test]
    fn baseline_image_has_no_findings() {
        let object = object_with(baseline_image_elements(), uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        assert!(report.findings.is_empty(), "expected no findings, got {:#?}", report.findings);
    }

    #[test]
    fn bits_stored_exceeding_bits_allocated_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.push(u16_element(tags::BITS_STORED, VR::US, 16));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "pixel/bits-stored-exceeds-allocated").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn high_bit_not_matching_bits_stored_minus_one_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.push(u16_element(tags::HIGH_BIT, VR::US, 5));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "pixel/high-bit-mismatch").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn photometric_interpretation_samples_per_pixel_mismatch_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "RGB"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "pixel/samples-per-pixel-photometric-mismatch").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn planar_configuration_on_monochrome_is_a_warning() {
        let mut elements = baseline_image_elements();
        elements.push(u16_element(tags::PLANAR_CONFIGURATION, VR::US, 1));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "pixel/planar-configuration-on-monochrome").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn palette_color_without_lut_data_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "PALETTE COLOR"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        assert_eq!(
            report.findings.iter().filter(|f| f.rule_id == "pixel/palette-color-missing-lut").count(),
            3,
            "expected one finding per missing R/G/B LUT: {:#?}",
            report.findings
        );
    }

    #[test]
    fn native_pixel_data_length_mismatch_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.retain(|element| element.tag() != tags::PIXEL_DATA);
        elements.push(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            dcmnorm_core::PrimitiveValue::from(vec![0u8; 2]),
        ));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "pixel/native-length-mismatch").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn missing_required_pixel_attributes_are_reported_and_suppress_downstream_pixel_rules() {
        let object = object_with(
            vec![DataElement::new(
                tags::PIXEL_DATA,
                VR::OW,
                dcmnorm_core::PrimitiveValue::from(vec![0u8; 4]),
            )],
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
        );
        let report = check_dicom_logic(&object);
        let missing_count =
            report.findings.iter().filter(|f| f.rule_id == "pixel/required-attribute-missing").count();
        assert_eq!(missing_count, 5, "Rows/Columns/SamplesPerPixel/BitsAllocated/PhotometricInterpretation");
        assert!(
            find(&report, "pixel/native-length-mismatch").is_none(),
            "downstream pixel rules should not run without the attributes they depend on"
        );
    }

    #[test]
    fn encapsulated_fragment_not_matching_declared_codec_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.retain(|element| element.tag() != tags::PIXEL_DATA);
        // Transfer syntax claims JPEG Baseline, but the fragment starts with a JPEG 2000 raw
        // codestream SOC marker instead of a JPEG SOI - exactly the "mislabeled file" scenario
        // this rule exists to catch before a real decode attempt fails or produces garbage.
        let bogus_fragment: Vec<u8> = vec![0xFF, 0x4F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        elements.push(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new(Vec::<u32>::new(), vec![bogus_fragment]),
        ));
        let object = object_with(elements, uids::JPEG_BASELINE8_BIT);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "pixel/transfer-syntax-fragment-mismatch").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn encapsulated_fragment_matching_declared_codec_has_no_finding() {
        let mut elements = baseline_image_elements();
        elements.retain(|element| element.tag() != tags::PIXEL_DATA);
        let real_looking_fragment: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x00, 0x00, 0x00];
        elements.push(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new(Vec::<u32>::new(), vec![real_looking_fragment]),
        ));
        let object = object_with(elements, uids::JPEG_BASELINE8_BIT);
        let report = check_dicom_logic(&object);
        assert!(find(&report, "pixel/transfer-syntax-fragment-mismatch").is_none());
    }

    #[test]
    fn fewer_fragments_than_declared_frames_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.retain(|element| element.tag() != tags::PIXEL_DATA && element.tag() != tags::NUMBER_OF_FRAMES);
        elements.push(str_element(tags::NUMBER_OF_FRAMES, VR::IS, "3"));
        let single_fragment: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xDB];
        elements.push(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new(Vec::<u32>::new(), vec![single_fragment]),
        ));
        let object = object_with(elements, uids::JPEG_BASELINE8_BIT);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "pixel/encapsulated-fragment-count-too-low").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn malformed_uid_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.retain(|element| element.tag() != tags::SOP_INSTANCE_UID);
        elements.push(str_element(tags::SOP_INSTANCE_UID, VR::UI, "not-a-uid"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "uid/malformed-uid").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn sop_instance_uid_meta_mismatch_is_a_warning_not_an_error() {
        let mut elements = baseline_image_elements();
        elements.retain(|element| element.tag() != tags::SOP_INSTANCE_UID);
        elements.push(str_element(tags::SOP_INSTANCE_UID, VR::UI, "9.9.9.9"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "uid/sop-instance-uid-meta-mismatch").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(!report.has_errors());
    }

    #[test]
    fn unrecognized_transfer_syntax_is_a_warning() {
        let object = object_with(baseline_image_elements(), "1.2.9.9.9.9");
        let report = check_dicom_logic(&object);
        let finding = find(&report, "uid/unknown-transfer-syntax").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn non_orthonormal_orientation_is_flagged() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "1\\0\\0\\1\\0\\0"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        assert!(find(&report, "geometry/orientation-not-orthogonal").is_some());
    }

    #[test]
    fn non_unit_orientation_vector_is_flagged() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "2\\0\\0\\0\\1\\0"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        assert!(find(&report, "geometry/orientation-not-unit-vector").is_some());
    }

    #[test]
    fn well_formed_orientation_has_no_geometry_findings() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::IMAGE_ORIENTATION_PATIENT, VR::DS, "1\\0\\0\\0\\1\\0"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        assert!(find(&report, "geometry/orientation-not-unit-vector").is_none());
        assert!(find(&report, "geometry/orientation-not-orthogonal").is_none());
    }

    #[test]
    fn non_positive_pixel_spacing_is_an_error() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::PIXEL_SPACING, VR::DS, "0\\0.5"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "geometry/non-positive-spacing").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Error);
    }

    #[test]
    fn ct_without_rescale_is_a_warning() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::MODALITY, VR::CS, "CT"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "modality/ct-missing-rescale").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn ct_with_rescale_has_no_finding() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::MODALITY, VR::CS, "CT"));
        elements.push(str_element(tags::RESCALE_SLOPE, VR::DS, "1"));
        elements.push(str_element(tags::RESCALE_INTERCEPT, VR::DS, "0"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        assert!(find(&report, "modality/ct-missing-rescale").is_none());
    }

    #[test]
    fn window_center_width_vm_mismatch_is_a_warning() {
        let mut elements = baseline_image_elements();
        elements.push(str_element(tags::WINDOW_CENTER, VR::DS, "40\\80"));
        elements.push(str_element(tags::WINDOW_WIDTH, VR::DS, "400"));
        let object = object_with(elements, uids::EXPLICIT_VR_LITTLE_ENDIAN);
        let report = check_dicom_logic(&object);
        let finding = find(&report, "modality/window-vm-mismatch").expect("expected a finding");
        assert_eq!(finding.severity, Severity::Warning);
    }

    #[test]
    fn real_fixture_with_deliberately_corrupt_pixel_data_is_flagged() {
        let object = super::super::io::read_dicom_file(fixture_path("bad_vr.dcm")).unwrap();
        let report = check_dicom_logic(&object);
        assert!(find(&report, "pixel/native-length-mismatch").is_some());
    }

    #[test]
    fn real_fixture_rle_file_has_no_errors() {
        let object = super::super::io::read_dicom_file(fixture_path("mr_rle.dcm")).unwrap();
        let report = check_dicom_logic(&object);
        assert!(!report.has_errors(), "expected no errors, got {:#?}", report.findings);
    }
}
