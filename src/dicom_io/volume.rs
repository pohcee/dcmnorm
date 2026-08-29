//! Builds a 3D voxel volume from a set of DICOM slice files sharing a `FrameOfReference` and
//! resamples arbitrary oblique planes through it (server-side MPR - Multiplanar Reformation).
//!
//! Reuses `super::render`'s existing per-frame pixel-decode pipeline
//! (`decode_frame_grayscale_values`, which already handles decompression across transfer syntaxes
//! and modality LUT application) for every source slice, and its VOI-windowing/encode pipeline
//! (`apply_voi_window`, `encode_rendered_frame`) for the reformatted output - so a volume's voxels
//! and a reformatted plane's rendering are never a second, drifting implementation of what 2D
//! rendering already does.

use std::error::Error;
use std::fmt;
use std::path::Path;

use dcmnorm_core::Tag;
use dcmnorm_dictionary::tags;
use dcmnorm_object::DefaultDicomObject;
use rayon::prelude::*;

use super::io::read_dicom_file;
use super::render::{
    apply_voi_window, decode_frame_grayscale_values, encode_rendered_frame, RenderMetadata,
    RenderedFramePixels,
};
pub use super::render::{RenderFrameOutput, RenderOutputFormat};
use super::types::{ReadError, RenderError};

/// Tolerance, in direction-cosine units, for treating two slices' `ImageOrientationPatient` as
/// "the same plane orientation" - i.e. a parallel stack. DICOM direction cosines are typically
/// exact to several decimal places within one series; this is generous enough to absorb float
/// round-trip noise while still catching a genuinely gantry-tilted-inconsistent or multi-orientation
/// series.
const ORIENTATION_EPSILON: f64 = 1e-3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Interpolation {
    Nearest,
    Trilinear,
}

/// How a slab's samples (see `PlaneParams::slab_thickness_mm`) combine into one output pixel.
/// `MaximumIntensity` (MIP) is the conventional radiology default for a thick MPR - it's what
/// makes vessels/bright structures visible across a slab instead of only whichever thin plane
/// happens to intersect them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlabProjection {
    MaximumIntensity,
    MinimumIntensity,
    Average,
}

/// Parameters for one reformatted plane through a `Volume`. `origin`/`row_dir`/`col_dir` are in
/// patient (LPS) millimeters, matching `Volume::origin`/`row_vector`/`col_vector`.
#[derive(Clone, Debug)]
pub struct PlaneParams {
    /// World-space (mm) point at the CENTER of the output image.
    pub origin: [f64; 3],
    /// Unit vector: direction of travel across the output image as the column index increases.
    pub row_dir: [f64; 3],
    /// Unit vector: direction of travel down the output image as the row index increases.
    pub col_dir: [f64; 3],
    pub output_width: u32,
    pub output_height: u32,
    /// Physical size of one output pixel, in mm - the SAME value in both output axes
    /// (isotropic), which is what makes a coronal/sagittal/oblique reformat physically
    /// undistorted even when the volume's own row/column/slice spacings differ.
    pub spacing_mm: f64,
    pub window_center: Option<f64>,
    pub window_width: Option<f64>,
    pub interpolation: Interpolation,
    /// Total slab thickness, in mm, centered on `origin` along the plane's own normal
    /// (`cross(row_dir, col_dir)`) - `0.0` (the default) reformats an infinitely-thin single
    /// plane, matching every other field's "just a flat 2D slice" behavior. A positive value
    /// samples multiple depths spanning this thickness and combines them per `slab_projection` -
    /// e.g. a 20mm MIP slab makes a thin vessel visible even when it only crosses the exact
    /// center plane at one point.
    pub slab_thickness_mm: f64,
    /// Combination method for `slab_thickness_mm > 0.0` - ignored (but still validated as
    /// present) when thickness is `0.0`.
    pub slab_projection: SlabProjection,
}

#[derive(Debug)]
pub enum VolumeError {
    Empty,
    TooFewSlices { found: usize, required: usize },
    Read(ReadError),
    Render(RenderError),
    MissingGeometry(&'static str),
    InconsistentGeometry(String),
    InconsistentDimensions,
    UnsupportedPhotometricInterpretation(String),
    InvalidOutputDimensions,
    InvalidSlabThickness,
}

impl fmt::Display for VolumeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "no files given to build a volume from"),
            Self::TooFewSlices { found, required } => write!(
                formatter,
                "not enough slices to build a volume: found {found}, need at least {required}"
            ),
            Self::Read(error) => write!(formatter, "failed to read a DICOM file for volume building: {error}"),
            Self::Render(error) => write!(formatter, "failed to decode a slice for volume building: {error}"),
            Self::MissingGeometry(attribute) => write!(
                formatter,
                "missing or unparseable {attribute} - MPR requires full slice geometry"
            ),
            Self::InconsistentGeometry(message) => write!(formatter, "inconsistent slice geometry: {message}"),
            Self::InconsistentDimensions => write!(
                formatter,
                "slices in this series do not all share the same Rows/Columns"
            ),
            Self::UnsupportedPhotometricInterpretation(value) => write!(
                formatter,
                "unsupported PhotometricInterpretation for volume building: {value} (only MONOCHROME1/MONOCHROME2 are supported)"
            ),
            Self::InvalidOutputDimensions => write!(formatter, "requested output width/height is invalid"),
            Self::InvalidSlabThickness => write!(formatter, "slab thickness must be zero (a thin plane) or a positive number of millimeters"),
        }
    }
}

impl Error for VolumeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Render(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ReadError> for VolumeError {
    fn from(value: ReadError) -> Self {
        Self::Read(value)
    }
}

impl From<RenderError> for VolumeError {
    fn from(value: RenderError) -> Self {
        Self::Render(value)
    }
}

/// A 3D voxel volume assembled from a parallel stack of DICOM slices, with modality LUT
/// (RescaleSlope/RescaleIntercept) already applied per-slice - `samples` holds physically
/// meaningful values (e.g. Hounsfield units for CT), not raw stored integers.
pub struct Volume {
    // `Debug` is implemented manually below (omitting `samples`, which can be hundreds of MB) so
    // this type stays usable in `assert!`/`{:?}` failure messages without flooding test output.
    /// Flat `[slice][row][col]` array (slice-major, then row-major within a slice - matching how
    /// `decode_frame_grayscale_values` already returns each slice), length
    /// `rows * cols * num_slices`.
    pub samples: Vec<f32>,
    pub rows: u32,
    pub cols: u32,
    pub num_slices: u32,
    /// `ImagePositionPatient` of the spatially-first (sorted) slice, mm, patient/LPS space.
    pub origin: [f64; 3],
    /// Unit vector: direction of travel as COLUMN index increases (`ImageOrientationPatient[0..3]`).
    pub row_vector: [f64; 3],
    /// Unit vector: direction of travel as ROW index increases (`ImageOrientationPatient[3..6]`).
    pub col_vector: [f64; 3],
    /// Unit vector: direction of travel as SLICE index increases (`cross(row_vector, col_vector)`,
    /// re-derived from the actual sort order so it always points from slice 0 toward the last).
    pub slice_normal: [f64; 3],
    /// `PixelSpacing[1]` (column spacing) - physical distance between adjacent columns, i.e. the
    /// step size along `row_vector`.
    pub col_spacing_mm: f64,
    /// `PixelSpacing[0]` (row spacing) - physical distance between adjacent rows, i.e. the step
    /// size along `col_vector`.
    pub row_spacing_mm: f64,
    /// Each (sorted) slice's projected position along `slice_normal`, mm - `slice_zs[0]`
    /// corresponds to `origin`. Monotonically non-decreasing. Kept as real per-slice positions
    /// (not an assumed-uniform spacing) so any irregular gap in the acquisition is still handled
    /// correctly during resampling.
    pub slice_zs: Vec<f64>,
}

impl fmt::Debug for Volume {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Volume")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("num_slices", &self.num_slices)
            .field("origin", &self.origin)
            .field("row_vector", &self.row_vector)
            .field("col_vector", &self.col_vector)
            .field("slice_normal", &self.slice_normal)
            .field("col_spacing_mm", &self.col_spacing_mm)
            .field("row_spacing_mm", &self.row_spacing_mm)
            .field("slice_zs", &self.slice_zs)
            .field("samples_len", &self.samples.len())
            .finish()
    }
}

impl Volume {
    /// A reasonable default reformat origin: the volume's own physical center.
    pub fn center(&self) -> [f64; 3] {
        let half_col = (self.cols.saturating_sub(1)) as f64 / 2.0;
        let half_row = (self.rows.saturating_sub(1)) as f64 / 2.0;
        let mid_z = (self.slice_zs[0] + self.slice_zs[self.slice_zs.len() - 1]) / 2.0;
        let along_slices = mid_z - self.slice_zs[0];
        add3(
            add3(
                add3(self.origin, scale3(self.row_vector, half_col * self.col_spacing_mm)),
                scale3(self.col_vector, half_row * self.row_spacing_mm),
            ),
            scale3(self.slice_normal, along_slices),
        )
    }

    /// A reasonable default output pixel spacing: the volume's own smallest voxel dimension, so
    /// a reformat defaults to (at least) native resolution in every direction.
    pub fn min_spacing_mm(&self) -> f64 {
        let slice_spacing = if self.slice_zs.len() > 1 {
            let span = self.slice_zs[self.slice_zs.len() - 1] - self.slice_zs[0];
            (span / (self.slice_zs.len() - 1) as f64).abs()
        } else {
            f64::INFINITY
        };
        self.row_spacing_mm.min(self.col_spacing_mm).min(slice_spacing).max(1e-6)
    }

    fn voxel(&self, row: i64, col: i64, slice: i64) -> f32 {
        let row = row.clamp(0, self.rows as i64 - 1) as usize;
        let col = col.clamp(0, self.cols as i64 - 1) as usize;
        let slice = slice.clamp(0, self.num_slices as i64 - 1) as usize;
        let plane = self.rows as usize * self.cols as usize;
        self.samples[slice * plane + row * self.cols as usize + col]
    }
}

type Vec3 = [f64; 3];

fn dot3(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross3(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn sub3(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add3(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale3(a: Vec3, s: f64) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn norm3(a: Vec3) -> f64 {
    dot3(a, a).sqrt()
}

fn normalize3(a: Vec3) -> Vec3 {
    let n = norm3(a);
    if n < 1e-9 {
        a
    } else {
        [a[0] / n, a[1] / n, a[2] / n]
    }
}

fn vectors_close(a: Vec3, b: Vec3, tolerance: f64) -> bool {
    norm3(sub3(a, b)) <= tolerance
}

fn read_multi_f64(object: &DefaultDicomObject, tag: Tag, attribute: &'static str) -> Result<Vec<f64>, VolumeError> {
    let element = object.get(tag).ok_or(VolumeError::MissingGeometry(attribute))?;
    let text = element.to_str().map_err(|_| VolumeError::MissingGeometry(attribute))?;
    let values: Vec<f64> = text
        .split('\\')
        .filter_map(|part| part.trim().parse::<f64>().ok())
        .collect();
    if values.is_empty() {
        return Err(VolumeError::MissingGeometry(attribute));
    }
    Ok(values)
}

fn read_orientation(object: &DefaultDicomObject) -> Result<(Vec3, Vec3), VolumeError> {
    let values = read_multi_f64(object, tags::IMAGE_ORIENTATION_PATIENT, "ImageOrientationPatient")?;
    if values.len() < 6 {
        return Err(VolumeError::MissingGeometry("ImageOrientationPatient"));
    }
    let row_vector = normalize3([values[0], values[1], values[2]]);
    let col_vector = normalize3([values[3], values[4], values[5]]);
    Ok((row_vector, col_vector))
}

fn read_position(object: &DefaultDicomObject) -> Result<Vec3, VolumeError> {
    let values = read_multi_f64(object, tags::IMAGE_POSITION_PATIENT, "ImagePositionPatient")?;
    if values.len() < 3 {
        return Err(VolumeError::MissingGeometry("ImagePositionPatient"));
    }
    Ok([values[0], values[1], values[2]])
}

/// Returns `(rowSpacingMm, colSpacingMm)` - DICOM `PixelSpacing` order: value[0] is the physical
/// distance between adjacent ROWS (the step along `col_vector`), value[1] between adjacent
/// COLUMNS (the step along `row_vector`). This row/column-vs-vector pairing is the single most
/// common source of MPR aspect-ratio bugs, so it's spelled out at every call site rather than
/// left implicit.
fn read_pixel_spacing(object: &DefaultDicomObject) -> Result<(f64, f64), VolumeError> {
    let values = read_multi_f64(object, tags::PIXEL_SPACING, "PixelSpacing")?;
    if values.len() < 2 {
        return Err(VolumeError::MissingGeometry("PixelSpacing"));
    }
    Ok((values[0], values[1]))
}

struct SourceSlice {
    row_vector: Vec3,
    col_vector: Vec3,
    position: Vec3,
    row_spacing_mm: f64,
    col_spacing_mm: f64,
    metadata: RenderMetadata,
    values: Vec<f64>,
}

/// Reads every file, sorts them into spatial (not necessarily on-disk-list) order by projecting
/// `ImagePositionPatient` onto the stack's shared slice normal, and assembles a `Volume`.
/// Rejects (rather than silently mis-rendering) a series whose slices don't share a consistent
/// `ImageOrientationPatient`/dimensions - true oblique/gantry-tilt-inconsistent stacks aren't a
/// resamplable orthogonal volume without more advanced reconstruction, which is out of scope.
pub fn build_volume<P: AsRef<Path> + Sync>(file_paths: &[P]) -> Result<Volume, VolumeError> {
    if file_paths.is_empty() {
        return Err(VolumeError::Empty);
    }
    if file_paths.len() < 2 {
        return Err(VolumeError::TooFewSlices {
            found: file_paths.len(),
            required: 2,
        });
    }

    // Reading + decoding each slice is the expensive part of building a volume (DICOM parse plus
    // transfer-syntax decompression per file), and slices are independent of each other until the
    // sort/consistency checks below, so read them concurrently rather than one at a time.
    let results: Vec<Result<SourceSlice, VolumeError>> = file_paths
        .par_iter()
        .map(|path| {
            let object = read_dicom_file(path.as_ref())?;
            let (row_vector, col_vector) = read_orientation(&object)?;
            let position = read_position(&object)?;
            let (row_spacing_mm, col_spacing_mm) = read_pixel_spacing(&object)?;
            let (metadata, values) = decode_frame_grayscale_values(&object, 0)?;
            if !matches!(metadata.photometric_interpretation.as_str(), "MONOCHROME1" | "MONOCHROME2") {
                return Err(VolumeError::UnsupportedPhotometricInterpretation(
                    metadata.photometric_interpretation.clone(),
                ));
            }
            Ok(SourceSlice {
                row_vector,
                col_vector,
                position,
                row_spacing_mm,
                col_spacing_mm,
                metadata,
                values,
            })
        })
        .collect();
    let slices: Vec<SourceSlice> = results.into_iter().collect::<Result<Vec<_>, _>>()?;

    let rows = slices[0].metadata.rows;
    let cols = slices[0].metadata.cols;
    let row_vector = slices[0].row_vector;
    let col_vector = slices[0].col_vector;
    for slice in &slices[1..] {
        if slice.metadata.rows != rows || slice.metadata.cols != cols {
            return Err(VolumeError::InconsistentDimensions);
        }
        if !vectors_close(slice.row_vector, row_vector, ORIENTATION_EPSILON)
            || !vectors_close(slice.col_vector, col_vector, ORIENTATION_EPSILON)
        {
            return Err(VolumeError::InconsistentGeometry(
                "ImageOrientationPatient differs between slices (non-parallel or gantry-tilt-inconsistent stack)"
                    .to_owned(),
            ));
        }
    }

    let slice_normal = normalize3(cross3(row_vector, col_vector));

    let mut order: Vec<usize> = (0..slices.len()).collect();
    order.sort_by(|&a, &b| {
        let za = dot3(slices[a].position, slice_normal);
        let zb = dot3(slices[b].position, slice_normal);
        za.partial_cmp(&zb).unwrap_or(std::cmp::Ordering::Equal)
    });

    let slice_zs: Vec<f64> = order.iter().map(|&i| dot3(slices[i].position, slice_normal)).collect();
    let z_span = slice_zs[slice_zs.len() - 1] - slice_zs[0];
    if z_span.abs() < 1e-3 {
        return Err(VolumeError::InconsistentGeometry(
            "all slices project to the same spatial position along the stack normal".to_owned(),
        ));
    }

    let origin = slices[order[0]].position;
    let row_spacing_mm = slices[order[0]].row_spacing_mm;
    let col_spacing_mm = slices[order[0]].col_spacing_mm;

    let plane_len = rows as usize * cols as usize;
    let mut samples = Vec::with_capacity(plane_len * order.len());
    for &index in &order {
        let values = &slices[index].values;
        samples.extend(values.iter().map(|&value| value as f32));
    }

    Ok(Volume {
        samples,
        rows: rows as u32,
        cols: cols as u32,
        num_slices: order.len() as u32,
        origin,
        row_vector,
        col_vector,
        slice_normal,
        col_spacing_mm,
        row_spacing_mm,
        slice_zs,
    })
}

/// Returns the fractional slice index for a world-space projection `z` along `slice_normal`, or
/// `None` if `z` is more than one slice spacing beyond either end of the stack (treated as
/// outside the volume rather than clamped/repeated, so a plane that partly leaves the acquired
/// volume renders a clean background instead of a smeared repeat of the edge slice).
fn fractional_slice_index(slice_zs: &[f64], z: f64) -> Option<f64> {
    let n = slice_zs.len();
    let nominal_spacing = if n > 1 {
        ((slice_zs[n - 1] - slice_zs[0]) / (n - 1) as f64).abs().max(1e-6)
    } else {
        1.0
    };
    let margin = nominal_spacing;
    if z < slice_zs[0] - margin || z > slice_zs[n - 1] + margin {
        return None;
    }
    if n == 1 {
        return Some(0.0);
    }

    match slice_zs.binary_search_by(|probe| probe.partial_cmp(&z).unwrap_or(std::cmp::Ordering::Equal)) {
        Ok(index) => Some(index as f64),
        Err(index) => {
            if index == 0 {
                let t = (z - slice_zs[0]) / (slice_zs[1] - slice_zs[0]).max(1e-9);
                Some(t) // may be slightly negative within `margin`, which `Volume::voxel` clamps
            } else if index >= n {
                let last = n - 1;
                let t = (z - slice_zs[last - 1]) / (slice_zs[last] - slice_zs[last - 1]).max(1e-9);
                Some((last - 1) as f64 + t)
            } else {
                let z0 = slice_zs[index - 1];
                let z1 = slice_zs[index];
                let t = (z - z0) / (z1 - z0).max(1e-9);
                Some((index - 1) as f64 + t)
            }
        }
    }
}

fn sample_nearest(volume: &Volume, row_f: f64, col_f: f64, slice_f: f64) -> f64 {
    f64::from(volume.voxel(row_f.round() as i64, col_f.round() as i64, slice_f.round() as i64))
}

fn sample_trilinear(volume: &Volume, row_f: f64, col_f: f64, slice_f: f64) -> f64 {
    let row0 = row_f.floor();
    let col0 = col_f.floor();
    let slice0 = slice_f.floor();
    let row_t = row_f - row0;
    let col_t = col_f - col0;
    let slice_t = slice_f - slice0;
    let (row0, col0, slice0) = (row0 as i64, col0 as i64, slice0 as i64);

    let corner = |dr: i64, dc: i64, ds: i64| f64::from(volume.voxel(row0 + dr, col0 + dc, slice0 + ds));

    let c00 = corner(0, 0, 0) * (1.0 - col_t) + corner(0, 1, 0) * col_t;
    let c10 = corner(1, 0, 0) * (1.0 - col_t) + corner(1, 1, 0) * col_t;
    let c01 = corner(0, 0, 1) * (1.0 - col_t) + corner(0, 1, 1) * col_t;
    let c11 = corner(1, 0, 1) * (1.0 - col_t) + corner(1, 1, 1) * col_t;

    let c0 = c00 * (1.0 - row_t) + c10 * row_t;
    let c1 = c01 * (1.0 - row_t) + c11 * row_t;

    c0 * (1.0 - slice_t) + c1 * slice_t
}

/// Transforms one world (patient/LPS mm) point into `volume`'s own voxel index space and samples
/// it, or returns `None` if the point falls outside the volume (beyond a 1-voxel margin) or the
/// slice stack. The single-point primitive both a thin-plane reformat and a thick slab (many
/// points along the plane's normal, see `sample_slab`) are built from.
fn sample_volume_at_world_point(volume: &Volume, world: Vec3, interpolation: Interpolation) -> Option<f64> {
    let delta = sub3(world, volume.origin);
    let col_f = dot3(delta, volume.row_vector) / volume.col_spacing_mm;
    let row_f = dot3(delta, volume.col_vector) / volume.row_spacing_mm;
    let z_projection = dot3(delta, volume.slice_normal) + volume.slice_zs[0];

    let margin = 1.0;
    let in_plane_bounds = col_f >= -margin
        && col_f <= (volume.cols as f64 - 1.0) + margin
        && row_f >= -margin
        && row_f <= (volume.rows as f64 - 1.0) + margin;
    if !in_plane_bounds {
        return None;
    }

    fractional_slice_index(&volume.slice_zs, z_projection).map(|slice_f| match interpolation {
        Interpolation::Nearest => sample_nearest(volume, row_f, col_f, slice_f),
        Interpolation::Trilinear => sample_trilinear(volume, row_f, col_f, slice_f),
    })
}

/// Samples `n_samples` points spanning `thickness_mm`, centered on `center`, along `normal` -
/// note this is the REFORMAT PLANE's own normal (`cross(row_dir, col_dir)`), not necessarily
/// `volume.slice_normal`: for an oblique/coronal/sagittal reformat the two differ, so moving
/// along the slab generally shifts all three of the volume's row/col/slice coordinates at once,
/// not just depth - each sample needs the full `sample_volume_at_world_point` transform, not a
/// cheap z-only adjustment. Points outside the volume are skipped rather than treated as zero;
/// `None` only if every sample in the slab misses the volume entirely.
fn sample_slab(
    volume: &Volume,
    center: Vec3,
    normal: Vec3,
    n_samples: usize,
    step_mm: f64,
    interpolation: Interpolation,
    projection: SlabProjection,
) -> Option<f64> {
    let half_span_mm = step_mm * (n_samples - 1) as f64 / 2.0;
    let mut min_value = f64::INFINITY;
    let mut max_value = f64::NEG_INFINITY;
    let mut sum = 0.0;
    let mut count = 0usize;

    for index in 0..n_samples {
        let offset_mm = -half_span_mm + step_mm * index as f64;
        let point = add3(center, scale3(normal, offset_mm));
        if let Some(value) = sample_volume_at_world_point(volume, point, interpolation) {
            min_value = min_value.min(value);
            max_value = max_value.max(value);
            sum += value;
            count += 1;
        }
    }

    if count == 0 {
        return None;
    }

    Some(match projection {
        SlabProjection::MaximumIntensity => max_value,
        SlabProjection::MinimumIntensity => min_value,
        SlabProjection::Average => sum / count as f64,
    })
}

/// Resamples one plane (or, with `params.slab_thickness_mm > 0.0`, a thick slab projected down to
/// one plane) through `volume` and returns the RAW rescaled values (e.g. Hounsfield units for
/// CT) row-major, `params.output_width * params.output_height` long - no VOI windowing, no 8-bit
/// encoding. This is the shared sampling primitive behind both `reformat_plane` (which windows
/// and encodes the result into a normal 2D image) and whole-volume/DICOM-series export (which
/// wants the true intensity values, not a windowed render). Samples outside the volume are
/// background-filled with the minimum in-bounds value found (same convention `reformat_plane` has
/// always used), or `0.0` if every sample in the plane misses the volume entirely.
pub fn reformat_plane_values(volume: &Volume, params: &PlaneParams) -> Result<Vec<f32>, VolumeError> {
    if params.output_width == 0 || params.output_height == 0 || params.output_width > u16::MAX as u32 || params.output_height > u16::MAX as u32 {
        return Err(VolumeError::InvalidOutputDimensions);
    }
    if params.slab_thickness_mm < 0.0 {
        return Err(VolumeError::InvalidSlabThickness);
    }

    let row_dir = normalize3(params.row_dir);
    let col_dir = normalize3(params.col_dir);
    let normal = normalize3(cross3(row_dir, col_dir));
    let width = params.output_width as usize;
    let height = params.output_height as usize;
    let half_width = params.output_width as f64 / 2.0;
    let half_height = params.output_height as f64 / 2.0;

    // Sampling a thick slab at native voxel resolution keeps thin structures (e.g. a small
    // vessel in an MIP) from being skipped between samples, while the upper clamp keeps a very
    // thick slab (e.g. a whole-volume MIP) bounded in cost rather than sampling thousands of
    // points per output pixel.
    let slab_active = params.slab_thickness_mm > 0.0;
    let (slab_n_samples, slab_step_mm) = if slab_active {
        let step = volume.min_spacing_mm().max(0.05);
        let n = (((params.slab_thickness_mm / step).round() as i64) + 1).clamp(2, 128) as usize;
        (n, params.slab_thickness_mm / (n - 1) as f64)
    } else {
        (1, 0.0)
    };

    let mut values: Vec<Option<f64>> = Vec::with_capacity(width * height);
    for row in 0..height {
        for col in 0..width {
            let offset = add3(
                scale3(row_dir, (col as f64 - half_width) * params.spacing_mm),
                scale3(col_dir, (row as f64 - half_height) * params.spacing_mm),
            );
            let world = add3(params.origin, offset);

            let sample = if slab_active {
                sample_slab(volume, world, normal, slab_n_samples, slab_step_mm, params.interpolation, params.slab_projection)
            } else {
                sample_volume_at_world_point(volume, world, params.interpolation)
            };

            values.push(sample);
        }
    }

    let background = values
        .iter()
        .filter_map(|value| *value)
        .fold(f64::INFINITY, f64::min);
    let background = if background.is_finite() { background } else { 0.0 };
    Ok(values.into_iter().map(|value| value.unwrap_or(background) as f32).collect())
}

/// Resamples one plane (or, with `params.slab_thickness_mm > 0.0`, a thick slab projected down
/// to one plane) through `volume` and encodes it exactly like a normal 2D render
/// (`encode_rendered_frame`), so callers (the Node bindings, the CLI) get back the same
/// `RenderFrameOutput` shape 2D rendering already produces.
pub fn reformat_plane(
    volume: &Volume,
    params: &PlaneParams,
    output_format: RenderOutputFormat,
    jpeg_quality: u8,
) -> Result<RenderFrameOutput, VolumeError> {
    let values = reformat_plane_values(volume, params)?;
    let filled: Vec<f64> = values.into_iter().map(|value| value as f64).collect();

    let rendered = apply_voi_window(&filled, params.window_center, params.window_width);
    let frame = RenderedFramePixels {
        width: params.output_width as u16,
        height: params.output_height as u16,
        samples_per_pixel: 1,
        bytes: rendered,
    };
    encode_rendered_frame(&frame, output_format, jpeg_quality).map_err(VolumeError::from)
}

/// Resamples `volume`'s ENTIRE native voxel grid (not an oblique plane/slab - see
/// `reformat_plane_values` for that) down to a coarser `target_cols`x`target_rows`x`target_slices`
/// uniform grid, covering the same physical extent. Used by `super::texture_export` to fit a
/// volume within a client GPU's texture-size limit, or to produce a small, fast "progressive"
/// first pass before the full-resolution texture arrives.
///
/// Each target axis is sampled at points evenly spaced across `[0, native_dim - 1]` in that axis's
/// own native voxel-index space (not world space - there's no need to round-trip through
/// `sample_volume_at_world_point`'s world-to-voxel transform when we're already working in voxel
/// indices on both ends), so the corner voxels of the coarse grid land exactly on the corner
/// voxels of the native grid. A target dimension of `1` samples the native axis's midpoint.
pub fn resample_volume(volume: &Volume, target_cols: u32, target_rows: u32, target_slices: u32, interpolation: Interpolation) -> Vec<f32> {
    let target_cols = target_cols.max(1);
    let target_rows = target_rows.max(1);
    let target_slices = target_slices.max(1);

    let col_step = if target_cols > 1 { (volume.cols as f64 - 1.0) / (target_cols as f64 - 1.0) } else { 0.0 };
    let row_step = if target_rows > 1 { (volume.rows as f64 - 1.0) / (target_rows as f64 - 1.0) } else { 0.0 };
    let slice_step = if target_slices > 1 { (volume.num_slices as f64 - 1.0) / (target_slices as f64 - 1.0) } else { 0.0 };

    let axis_coordinate = |target_index: u32, target_len: u32, step: f64, native_len: u32| -> f64 {
        if target_len > 1 {
            target_index as f64 * step
        } else {
            (native_len as f64 - 1.0) / 2.0
        }
    };

    let plane = target_cols as usize * target_rows as usize;
    let mut out = vec![0f32; plane * target_slices as usize];
    for target_slice in 0..target_slices {
        let slice_f = axis_coordinate(target_slice, target_slices, slice_step, volume.num_slices);
        for target_row in 0..target_rows {
            let row_f = axis_coordinate(target_row, target_rows, row_step, volume.rows);
            for target_col in 0..target_cols {
                let col_f = axis_coordinate(target_col, target_cols, col_step, volume.cols);
                let value = match interpolation {
                    Interpolation::Nearest => sample_nearest(volume, row_f, col_f, slice_f),
                    Interpolation::Trilinear => sample_trilinear(volume, row_f, col_f, slice_f),
                };
                let index = target_slice as usize * plane + target_row as usize * target_cols as usize + target_col as usize;
                out[index] = value as f32;
            }
        }
    }
    out
}

/// The three canonical LPS-aligned views ("axial" reuses the volume's own acquisition basis;
/// "coronal"/"sagittal" are fixed patient-anatomy planes, independent of acquisition angle).
/// Matches the convention the viewer's own client-side snap buttons use, so CLI, server, and
/// client can never visually disagree about what "coronal" means. LPS convention: +X = Left,
/// +Y = Posterior, +Z = Head (consistent with the viewer's existing orientation-HUD labeling).
pub fn canonical_view_basis(view: &str, volume: &Volume) -> Result<(Vec3, Vec3), VolumeError> {
    match view {
        "axial" => Ok((volume.row_vector, volume.col_vector)),
        "coronal" => Ok(([1.0, 0.0, 0.0], [0.0, 0.0, -1.0])),
        "sagittal" => Ok(([0.0, 1.0, 0.0], [0.0, 0.0, -1.0])),
        other => Err(VolumeError::InconsistentGeometry(format!(
            "unknown MPR view '{other}'; expected axial, coronal, or sagittal"
        ))),
    }
}

/// Applies a yaw/pitch/roll rotation (degrees, about the fixed patient/LPS Z/X/Y axes
/// respectively) on top of an existing `(row_dir, col_dir)` basis - the "arbitrary oblique camera
/// angle" case for `--mpr-rotation`. Both vectors are carried through the identical fixed-axis
/// rotation sequence, which is what guarantees the result stays orthonormal (a rotation preserves
/// the angle - and thus orthogonality - between any two vectors it's applied to; composing each
/// step around the *other*, already-rotated vector instead would not).
pub fn rotate_basis(row_dir: Vec3, col_dir: Vec3, yaw_deg: f64, pitch_deg: f64, roll_deg: f64) -> (Vec3, Vec3) {
    const X_AXIS: Vec3 = [1.0, 0.0, 0.0];
    const Y_AXIS: Vec3 = [0.0, 1.0, 0.0];
    const Z_AXIS: Vec3 = [0.0, 0.0, 1.0];

    let apply = |v: Vec3| -> Vec3 {
        let v = rotate_about_axis(v, Z_AXIS, yaw_deg);
        let v = rotate_about_axis(v, X_AXIS, pitch_deg);
        rotate_about_axis(v, Y_AXIS, roll_deg)
    };

    (normalize3(apply(row_dir)), normalize3(apply(col_dir)))
}

/// Rodrigues' rotation formula: rotates vector `v` by `degrees` about unit axis `axis`.
fn rotate_about_axis(v: Vec3, axis: Vec3, degrees: f64) -> Vec3 {
    if degrees.abs() < 1e-9 {
        return v;
    }
    let radians = degrees.to_radians();
    let (sin, cos) = radians.sin_cos();
    let term1 = scale3(v, cos);
    let term2 = scale3(cross3(axis, v), sin);
    let term3 = scale3(axis, dot3(axis, v) * (1.0 - cos));
    add3(add3(term1, term2), term3)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x4x4 volume with axis-aligned geometry and `samples[idx] == idx as f32`, so every
    /// voxel has a distinct, hand-computable value - lets these tests assert exact sample
    /// identity instead of just "some plausible-looking output".
    fn axis_aligned_test_volume() -> Volume {
        let rows = 4u32;
        let cols = 4u32;
        let num_slices = 4u32;
        let plane = (rows * cols) as usize;
        let mut samples = vec![0f32; plane * num_slices as usize];
        for index in 0..samples.len() {
            samples[index] = index as f32;
        }
        Volume {
            samples,
            rows,
            cols,
            num_slices,
            origin: [0.0, 0.0, 0.0],
            row_vector: [1.0, 0.0, 0.0],
            col_vector: [0.0, 1.0, 0.0],
            slice_normal: [0.0, 0.0, 1.0],
            col_spacing_mm: 1.0,
            row_spacing_mm: 1.0,
            slice_zs: vec![0.0, 1.0, 2.0, 3.0],
        }
    }

    fn voxel_index(rows: u32, cols: u32, row: u32, col: u32, slice: u32) -> f32 {
        (slice * rows * cols + row * cols + col) as f32
    }

    #[test]
    fn voxel_clamps_out_of_range_indices() {
        let volume = axis_aligned_test_volume();
        assert_eq!(volume.voxel(-5, -5, -5), volume.voxel(0, 0, 0));
        assert_eq!(volume.voxel(100, 100, 100), volume.voxel(3, 3, 3));
    }

    #[test]
    fn sample_nearest_returns_exact_voxel_at_integer_coordinates() {
        let volume = axis_aligned_test_volume();
        for slice in 0..4u32 {
            for row in 0..4u32 {
                for col in 0..4u32 {
                    let sampled = sample_nearest(&volume, row as f64, col as f64, slice as f64);
                    assert_eq!(sampled, f64::from(voxel_index(4, 4, row, col, slice)));
                }
            }
        }
    }

    #[test]
    fn sample_trilinear_matches_nearest_at_integer_coordinates() {
        let volume = axis_aligned_test_volume();
        for (row, col, slice) in [(0.0, 0.0, 0.0), (1.0, 2.0, 3.0), (3.0, 3.0, 3.0)] {
            assert_eq!(
                sample_trilinear(&volume, row, col, slice),
                sample_nearest(&volume, row, col, slice)
            );
        }
    }

    #[test]
    fn sample_trilinear_interpolates_linearly_along_one_axis() {
        let volume = axis_aligned_test_volume();
        // Halfway between column 0 and column 1 (same row/slice) should average those two
        // voxels' values, since every other axis is held at an exact integer coordinate.
        let midpoint = sample_trilinear(&volume, 0.0, 0.5, 0.0);
        let left = f64::from(voxel_index(4, 4, 0, 0, 0));
        let right = f64::from(voxel_index(4, 4, 0, 1, 0));
        assert_eq!(midpoint, (left + right) / 2.0);
    }

    #[test]
    fn fractional_slice_index_interpolates_between_bracketing_slices() {
        let zs = [0.0, 1.0, 3.0, 6.0]; // deliberately irregular spacing
        assert_eq!(fractional_slice_index(&zs, 0.0), Some(0.0));
        assert_eq!(fractional_slice_index(&zs, 1.0), Some(1.0));
        // Halfway (by position, not by index) between slices 1 (z=1) and 2 (z=3).
        assert_eq!(fractional_slice_index(&zs, 2.0), Some(1.5));
        // Well beyond either end is out of the volume.
        assert_eq!(fractional_slice_index(&zs, -100.0), None);
        assert_eq!(fractional_slice_index(&zs, 100.0), None);
    }

    #[test]
    fn reformat_plane_axial_reproduces_expected_voxels_at_known_pixel_centers() {
        let volume = axis_aligned_test_volume();
        // output=4x4 (even) makes `(index - width/2.0)` land on exact integers, so the sampled
        // world points line up exactly on volume voxel centers with no interpolation ambiguity.
        let params = PlaneParams {
            origin: [1.0, 1.0, 1.0], // world point under output pixel (col=2, row=2)
            row_dir: [1.0, 0.0, 0.0],
            col_dir: [0.0, 1.0, 0.0],
            output_width: 4,
            output_height: 4,
            spacing_mm: 1.0,
            // Covers the fixture's full [0, 63] value range tightly, so adjacent sample values
            // map to visibly distinct bytes instead of collapsing under coarse quantization.
            window_center: Some(31.5),
            window_width: Some(64.0),
            interpolation: Interpolation::Nearest,
            slab_thickness_mm: 0.0,
            slab_projection: SlabProjection::MaximumIntensity,
        };

        let output = reformat_plane(&volume, &params, RenderOutputFormat::Raw, 90).unwrap();
        assert_eq!(output.width, 4);
        assert_eq!(output.height, 4);
        assert_eq!(output.bytes.len(), 16);

        // Center pixel (col=2,row=2) samples world=origin exactly -> volume voxel (row=1,col=1,slice=1).
        // Pixel (col=1,row=1) samples one voxel step back in every axis -> voxel (row=0,col=0,slice=1).
        let center_byte = output.bytes[2 * 4 + 2];
        let back_byte = output.bytes[1 * 4 + 1];
        let expected_center = f64::from(voxel_index(4, 4, 1, 1, 1));
        let expected_back = f64::from(voxel_index(4, 4, 0, 0, 1));
        assert!(expected_center > expected_back, "test fixture assumption");
        assert!(
            center_byte > back_byte,
            "windowed byte at the higher-valued voxel should also be higher (center={center_byte}, back={back_byte})"
        );
    }

    #[test]
    fn sample_slab_combines_multiple_depths_per_projection_method() {
        let volume = axis_aligned_test_volume();
        // For this fixture's axis-aligned basis, [0,0,1] happens to equal volume.slice_normal
        // too, so a 3-sample slab centered on voxel (row=1,col=1,slice=1) lands exactly on
        // slices 0, 1, 2 at that same row/col - voxel_index gives 5, 21, 37 respectively.
        let normal = [0.0, 0.0, 1.0];
        let center = [1.0, 1.0, 1.0];

        let mip = sample_slab(&volume, center, normal, 3, 1.0, Interpolation::Nearest, SlabProjection::MaximumIntensity);
        let minip = sample_slab(&volume, center, normal, 3, 1.0, Interpolation::Nearest, SlabProjection::MinimumIntensity);
        let average = sample_slab(&volume, center, normal, 3, 1.0, Interpolation::Nearest, SlabProjection::Average);

        assert_eq!(mip, Some(37.0));
        assert_eq!(minip, Some(5.0));
        assert_eq!(average, Some((5.0 + 21.0 + 37.0) / 3.0));
    }

    #[test]
    fn sample_slab_skips_out_of_volume_samples_and_returns_none_if_all_miss() {
        let volume = axis_aligned_test_volume();
        // Centered on the volume's edge slice with a wide slab - half the samples fall outside.
        let normal = [0.0, 0.0, 1.0];
        let center = [1.0, 1.0, 0.0];

        let result = sample_slab(&volume, center, normal, 5, 1.0, Interpolation::Nearest, SlabProjection::MaximumIntensity);
        assert!(result.is_some(), "slab should still combine the in-bounds samples");

        let far_outside = [1.0, 1.0, 1000.0];
        let none_in_bounds = sample_slab(&volume, far_outside, normal, 3, 1.0, Interpolation::Nearest, SlabProjection::Average);
        assert_eq!(none_in_bounds, None);
    }

    #[test]
    fn reformat_plane_with_a_thick_slab_differs_from_a_thin_plane() {
        let volume = axis_aligned_test_volume();
        // Fixed (not auto-normalized) window/level, so an absolute raw-value shift from the slab
        // is actually visible in the output bytes - auto min-max normalization would otherwise
        // hide it, since in this fixture every voxel value increases uniformly with slice index
        // (samples[idx] == idx), so a slab's max-vs-thin-plane difference is the SAME additive
        // shift at every pixel, which robust per-image normalization cancels out entirely.
        let base_params = PlaneParams {
            origin: [1.0, 1.0, 1.0],
            row_dir: [1.0, 0.0, 0.0],
            col_dir: [0.0, 1.0, 0.0],
            output_width: 4,
            output_height: 4,
            spacing_mm: 1.0,
            window_center: Some(31.5),
            window_width: Some(64.0),
            interpolation: Interpolation::Nearest,
            slab_thickness_mm: 0.0,
            slab_projection: SlabProjection::MaximumIntensity,
        };

        let thin = reformat_plane(&volume, &base_params, RenderOutputFormat::Raw, 90).unwrap();
        let thick_params = PlaneParams { slab_thickness_mm: 2.0, ..base_params };
        let thick_mip = reformat_plane(&volume, &thick_params, RenderOutputFormat::Raw, 90).unwrap();

        // The MIP slab spans slices {0,1,2} at every pixel (this reformat plane sits exactly on
        // volume.slice_normal), and values increase monotonically with slice index in this
        // fixture, so MIP == "the thin plane one slice further in" everywhere - strictly higher
        // at every non-clipped pixel, never lower.
        for (thick_byte, thin_byte) in thick_mip.bytes.iter().zip(thin.bytes.iter()) {
            assert!(thick_byte >= thin_byte, "MIP slab must never be dimmer than the thin plane (thick={thick_byte}, thin={thin_byte})");
        }
        assert_ne!(thick_mip.bytes, thin.bytes, "a 2mm MIP slab should differ from the thin plane somewhere");
    }

    #[test]
    fn reformat_plane_rejects_negative_slab_thickness() {
        let volume = axis_aligned_test_volume();
        let params = PlaneParams {
            origin: [1.0, 1.0, 1.0],
            row_dir: [1.0, 0.0, 0.0],
            col_dir: [0.0, 1.0, 0.0],
            output_width: 4,
            output_height: 4,
            spacing_mm: 1.0,
            window_center: None,
            window_width: None,
            interpolation: Interpolation::Nearest,
            slab_thickness_mm: -5.0,
            slab_projection: SlabProjection::MaximumIntensity,
        };

        let result = reformat_plane(&volume, &params, RenderOutputFormat::Raw, 90);
        assert!(matches!(result, Err(VolumeError::InvalidSlabThickness)));
    }

    #[test]
    fn canonical_view_basis_returns_orthonormal_vectors() {
        let volume = axis_aligned_test_volume();
        for view in ["axial", "coronal", "sagittal"] {
            let (row_dir, col_dir) = canonical_view_basis(view, &volume).unwrap();
            assert!((norm3(row_dir) - 1.0).abs() < 1e-9, "{view} row_dir not unit length");
            assert!((norm3(col_dir) - 1.0).abs() < 1e-9, "{view} col_dir not unit length");
            assert!(dot3(row_dir, col_dir).abs() < 1e-9, "{view} row_dir/col_dir not orthogonal");
        }
        assert!(canonical_view_basis("oblique-nonsense", &volume).is_err());
    }

    #[test]
    fn rotate_basis_preserves_orthonormality() {
        let (row_dir, col_dir) = ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        for (yaw, pitch, roll) in [(30.0, 0.0, 0.0), (0.0, 45.0, 0.0), (17.0, -22.0, 63.0)] {
            let (rotated_row, rotated_col) = rotate_basis(row_dir, col_dir, yaw, pitch, roll);
            assert!((norm3(rotated_row) - 1.0).abs() < 1e-9);
            assert!((norm3(rotated_col) - 1.0).abs() < 1e-9);
            assert!(
                dot3(rotated_row, rotated_col).abs() < 1e-9,
                "rotation broke orthogonality at yaw={yaw} pitch={pitch} roll={roll}"
            );
        }
    }

    #[test]
    fn resample_volume_at_native_dims_reproduces_every_voxel_exactly() {
        let volume = axis_aligned_test_volume();
        let resampled = resample_volume(&volume, 4, 4, 4, Interpolation::Nearest);
        assert_eq!(resampled, volume.samples);
    }

    #[test]
    fn resample_volume_downsamples_a_linear_ramp_correctly() {
        // Samples increase by 1 for each unit step in col (see axis_aligned_test_volume's
        // samples[idx]==idx layout with cols=4). Downsampling cols 4->2 places the two output
        // columns exactly on native columns 0 and 3 (the corners), so this also exercises that
        // corner-alignment guarantee, not just the interpolation math.
        let volume = axis_aligned_test_volume();
        let resampled = resample_volume(&volume, 2, 4, 4, Interpolation::Trilinear);
        assert_eq!(resampled.len(), 2 * 4 * 4);
        // row=0,slice=0: native col 0 -> voxel_index(...,col=0)=0; native col 3 -> voxel_index(...,col=3)=3.
        assert_eq!(resampled[0], voxel_index(4, 4, 0, 0, 0));
        assert_eq!(resampled[1], voxel_index(4, 4, 0, 3, 0));
    }

    #[test]
    fn resample_volume_to_a_single_voxel_per_axis_samples_the_midpoint() {
        let volume = axis_aligned_test_volume();
        let resampled = resample_volume(&volume, 1, 1, 1, Interpolation::Nearest);
        assert_eq!(resampled.len(), 1);
        // Midpoint of [0,3] on every axis is 1.5, which nearest-rounds to voxel (1 or 2, ...) -
        // just assert it's some genuine in-range voxel value, not a default/zeroed placeholder.
        assert!(resampled[0] >= 0.0 && resampled[0] <= 63.0);
    }

    #[test]
    fn build_volume_rejects_empty_and_single_file_lists() {
        assert!(matches!(build_volume::<&str>(&[]), Err(VolumeError::Empty)));
        assert!(matches!(
            build_volume(&["only_one.dcm"]),
            Err(VolumeError::TooFewSlices { found: 1, required: 2 })
        ));
    }
}
