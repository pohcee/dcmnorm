//! Rigid (6-DOF) registration of two volumes, and a fast translation-only estimate.
//!
//! Both work on [`Volume`]s (`build_volume`), each with its own voxel-to-patient geometry, of any
//! modality pair: CT, MR and PT with each other or themselves. The result is always a row-major
//! 4x4 matrix that maps a point in the MOVING volume's patient coordinates (LPS mm) to the FIXED
//! volume's: `p_fixed = M * [x, y, z, 1]` - the same direction as a DICOM Spatial Registration
//! item's Frame of Reference Transformation Matrix whose item frame is the moving one and whose
//! registered frame (the REG instance's own) is the fixed one.
//!
//! # How the rigid registration works
//!
//! 1. **Working images.** For each pyramid level (default 8, 4, 2 mm) both volumes are
//!    block-averaged in-plane and averaged (or interpolated) by real slice position along the
//!    normal onto a near-isotropic grid at that spacing, in the volume's own orientation. This is
//!    what makes anisotropic voxels (5 mm MR slices, 4 mm PET voxels) and differing voxel sizes
//!    or fields of view a non-issue: the metric only ever compares working images.
//! 2. **Body masks, modality aware.** CT: HU above -400, eroded one voxel, largest connected
//!    component, dilated back - which removes the table (it is separated from the patient by a
//!    low-density mattress). MR (and unknown modalities): half the Otsu threshold of the
//!    intensities. PT: 30% of the median uptake above 5% of the 99.5th percentile, opened like CT
//!    to drop the speckle outside the body; it works on raw counts, Bq/ml or SUV alike (no HU or
//!    SUV assumption). Every mask is the largest component with its holes filled (lungs, bowel
//!    gas) and a one-voxel margin for the skin edge; a CT's also includes everything between two
//!    body voxels of a slice row or column, so lungs a smaller reconstruction field of view cut
//!    open still count.
//! 3. **Intensity normalization.** Inside the mask, intensities are clamped to their 0.5-99.5
//!    percentile range and mapped to [0, 1]; outside it they are 0. That absorbs MR's arbitrary
//!    scaling and PET's counts-vs-SUV, and stops bright outliers (metal, the bladder) dominating.
//! 4. **Metric.** NCC (Pearson correlation) for the same modality, Mattes mutual information
//!    (cubic B-spline Parzen window on the moving image, 32 bins) across modalities. MR-MR starts
//!    with NCC and retries with MI when NCC stays poor (e.g. a T1 against a T2).
//! 5. **Optimizer.** Powell's direction-set method with Brent line searches over 3 rotations
//!    (about the fixed mask's centroid) and 3 translations, rotations scaled by the mask radius
//!    so one unit is about one millimetre of motion everywhere. Each level starts from the
//!    previous one; the first from `initial_matrix`, the translation estimate or the mask
//!    centroids - or, with `Initialization::Multi`, from each of several starts (`register_rigid`).
//! 6. **Quality.** The metric again, over only the fixed samples that land inside the moving
//!    BODY (`body_metric`): what one scan never saw (a smaller FOV, a cropped end) does not count
//!    against the alignment, while a misalignment still puts anatomy on anatomy that does not
//!    match. Mapped to 0..1 by a modality-aware scale calibrated on public data
//!    (`quality_score`), scaled down when the moving body covers little of the fixed one or a
//!    neighbouring transform 2 voxels away scores better (not a local optimum), and a rotation
//!    above `max_rotation_deg` (30 by default) is always "poor".
//!
//! Deterministic: sample selection is a fixed hash, every parallel reduction is summed in a fixed
//! chunk order, and nothing depends on the thread count. Bounded: the working images are capped
//! by `max_memory_bytes` (a level coarsens rather than exceeding it) and the metric uses at most
//! `max_samples` fixed points. Interruptible: every metric evaluation checks the time limit and
//! the cancel flag.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use super::volume::Volume;

/// Row-major 4x4 matrix.
pub type Mat4 = [f64; 16];
type Vec3 = [f64; 3];

const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

/// Fixed chunk size of every parallel reduction: results are summed chunk by chunk in order, so
/// a metric value is bit-identical whatever the thread count.
const REDUCTION_CHUNK: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    Ncc,
    MattesMi,
}

impl MetricKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ncc => "ncc",
            Self::MattesMi => "mattes-mi",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricChoice {
    /// NCC when both modalities are the same, Mattes MI otherwise.
    Auto,
    Ncc,
    MattesMi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaskMode {
    /// The modality-aware body mask (module docs).
    Auto,
    /// Every voxel of the field of view.
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Initialization {
    /// `estimate_translation`, falling back to the mask centroids when its confidence is low.
    Translation,
    /// Align the body-mask centroids.
    Centroid,
    /// Identity: the two frames are assumed roughly aligned already.
    None,
    /// Several starts (the initial matrix and `starts` when given, the translation estimate, the
    /// mask centroids and identity), the best kept; see `register_rigid`.
    Multi,
}

#[derive(Clone, Debug)]
pub struct RegistrationOptions {
    pub metric: MetricChoice,
    /// Overrides `Volume::modality` (which comes from the files).
    pub fixed_modality: Option<String>,
    pub moving_modality: Option<String>,
    /// Starting moving -> fixed transform. Takes precedence over `initialization`.
    pub initial_matrix: Option<Mat4>,
    pub initialization: Initialization,
    /// Working-grid spacing of each pyramid level, coarse to fine, mm.
    pub levels_mm: Vec<f64>,
    /// Powell iterations per level.
    pub max_iterations: u32,
    /// On expiry the best transform found so far is returned (`StopReason::TimeLimit`).
    pub time_limit: Option<Duration>,
    /// Set to stop as soon as possible with `RegistrationError::Cancelled`.
    pub cancel: Option<Arc<AtomicBool>>,
    pub mask: MaskMode,
    /// Optional box, in FIXED patient coordinates (min, max corners), the fixed mask is limited to.
    pub roi: Option<(Vec3, Vec3)>,
    pub histogram_bins: usize,
    pub max_samples: usize,
    /// Cap on the working images' memory; a level that would exceed it is coarsened.
    pub max_memory_bytes: u64,
    /// Extra starting moving -> fixed transforms. Non-empty means a multi-start registration.
    pub starts: Vec<Mat4>,
    /// A result rotated more than this (degrees, the rotation angle of the matrix) is labelled
    /// poor and `plausible: false`, and loses to any plausible start. `None`: no limit.
    pub max_rotation_deg: Option<f64>,
}

impl Default for RegistrationOptions {
    fn default() -> Self {
        Self {
            metric: MetricChoice::Auto,
            fixed_modality: None,
            moving_modality: None,
            initial_matrix: None,
            initialization: Initialization::Translation,
            levels_mm: vec![8.0, 4.0, 2.0],
            max_iterations: 40,
            time_limit: Some(Duration::from_secs(30)),
            cancel: None,
            mask: MaskMode::Auto,
            roi: None,
            histogram_bins: 32,
            max_samples: 100_000,
            max_memory_bytes: 256 * 1024 * 1024,
            starts: Vec::new(),
            max_rotation_deg: Some(30.0),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Converged,
    MaxIterations,
    TimeLimit,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Converged => "converged",
            Self::MaxIterations => "max-iterations",
            Self::TimeLimit => "time-limit",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LevelResult {
    pub spacing_mm: f64,
    pub iterations: u32,
    pub metric_evaluations: u32,
    pub metric_value: f64,
}

#[derive(Clone, Debug)]
pub struct RegistrationResult {
    /// Moving patient -> fixed patient, row-major.
    pub matrix: Mat4,
    /// Euler angles (degrees) of the matrix's rotation, `R = Rz * Ry * Rx`.
    pub rotation_deg: Vec3,
    /// The matrix's translation column, mm.
    pub translation_mm: Vec3,
    /// Centre of rotation used by the optimizer (fixed mask centroid), fixed patient mm.
    pub center_mm: Vec3,
    pub metric: MetricKind,
    /// NCC: Pearson correlation in [-1, 1]. MI: mutual information in nats.
    pub metric_value: f64,
    /// The same metric at the initial transform, at the finest level.
    pub initial_metric_value: f64,
    /// NCC: the correlation. MI: the entropy correlation coefficient 2 MI / (H(F) + H(M)), 0..1.
    pub normalized_metric: f64,
    /// Fraction of the fixed samples that land inside the moving volume.
    pub overlap_fraction: f64,
    /// 0..1 heuristic from `normalized_metric` and `overlap_fraction` (see `quality_score`).
    pub quality: f64,
    pub quality_label: &'static str,
    pub iterations: u32,
    pub metric_evaluations: u32,
    pub levels: Vec<LevelResult>,
    pub initialization: &'static str,
    pub converged: bool,
    pub stop_reason: StopReason,
    pub timing_ms: f64,
    /// Peak bytes of working images and samples the registration held.
    pub working_bytes: u64,
    /// The normalized metric over the fixed samples that land inside the moving BODY (not just
    /// the moving volume): what `quality` is computed from.
    pub body_metric: f64,
    /// Share of the fixed body samples that land inside the moving body.
    pub body_overlap_fraction: f64,
    /// The rotation angle of `matrix`, degrees.
    pub rotation_angle_deg: f64,
    /// False when `rotation_angle_deg` is above `max_rotation_deg`.
    pub plausible: bool,
    /// Multi-start only: whether another start converged to the same transform (within
    /// `AGREE_MM` over the fixed body). `None` for a single start.
    pub consistent: Option<bool>,
    /// Multi-start only: every start's result, in the order they were tried.
    pub starts: Vec<StartResult>,
}

/// One start of a multi-start registration.
#[derive(Clone, Debug)]
pub struct StartResult {
    /// "matrix", "translation", "centroid", "none" or "start-<n>" (`RegistrationOptions::starts`).
    pub start: String,
    /// Where this start converged (moving -> fixed).
    pub matrix: Mat4,
    pub normalized_metric: f64,
    pub quality: f64,
    pub quality_label: &'static str,
    pub rotation_angle_deg: f64,
    pub plausible: bool,
    /// Within `AGREE_MM` of the returned result.
    pub agrees: bool,
    /// Whether this start ran the finest level (only the best start does).
    pub refined: bool,
}

#[derive(Clone, Debug)]
pub struct TranslationOptions {
    pub fixed_modality: Option<String>,
    pub moving_modality: Option<String>,
    /// Smallest S-I overlap of the two scans a z shift may imply, mm (an upper bound when
    /// `min_overlap_fraction` is set).
    pub min_overlap_mm: f64,
    /// Largest |z shift| considered, mm (`None`: anything with enough overlap).
    pub max_shift_mm: Option<f64>,
    /// When set, the smallest overlap is `min(min_overlap_mm, max(fraction * shorter, 20 mm))`
    /// (the shorter scan's covered S-I extent; a 20 mm floor, itself at most 3/4 of it), so a
    /// short slab (an 80 mm prostate or head MR) can still be matched. `None`: exactly
    /// `min_overlap_mm`.
    pub min_overlap_fraction: Option<f64>,
}

impl Default for TranslationOptions {
    fn default() -> Self {
        Self {
            fixed_modality: None,
            moving_modality: None,
            min_overlap_mm: 60.0,
            max_shift_mm: None,
            min_overlap_fraction: Some(0.4),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TranslationEstimate {
    /// Moving patient -> fixed patient (a pure translation), row-major.
    pub matrix: Mat4,
    pub translation_mm: Vec3,
    /// 0..1, calibrated so a wrong answer stays below 0.3: the z peak's height (overlap factor
    /// included), its margin over the best other peak at least 20 mm away, the profile channels'
    /// agreement and the in-plane correlation.
    pub confidence: f64,
    pub peak_correlation: f64,
    pub second_peak_correlation: f64,
    pub overlap_mm: f64,
    /// The profiles that were correlated ("body", "lung", "bone", "intensity").
    pub profiles: Vec<&'static str>,
    pub timing_ms: f64,
    /// Weighted share (0..1) of the profile channels whose own best z shift is within 10 mm of
    /// the answer (1 with a single channel).
    pub channel_agreement: f64,
    /// The in-plane projections' correlation at the x/y answer.
    pub xy_correlation: f64,
    /// The smallest overlap that was allowed, mm (after `min_overlap_fraction`).
    pub min_overlap_mm: f64,
}

#[derive(Debug)]
pub enum RegistrationError {
    EmptyMask(&'static str),
    NoOverlap,
    InvalidOption(String),
    Cancelled,
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMask(which) => write!(
                formatter,
                "registration: the {which} volume has no body voxels to register"
            ),
            Self::NoOverlap => write!(
                formatter,
                "registration: fixed and moving volumes do not overlap"
            ),
            Self::InvalidOption(message) => write!(formatter, "registration: {message}"),
            Self::Cancelled => write!(formatter, "registration cancelled"),
        }
    }
}

impl std::error::Error for RegistrationError {}

// ---------------------------------------------------------------------------------------------
// Small linear algebra
// ---------------------------------------------------------------------------------------------

fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: Vec3, s: f64) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

pub fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [0.0; 16];
    for r in 0..4 {
        for c in 0..4 {
            out[r * 4 + c] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum();
        }
    }
    out
}

pub fn transform_point(m: &Mat4, p: Vec3) -> Vec3 {
    [
        m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3],
        m[4] * p[0] + m[5] * p[1] + m[6] * p[2] + m[7],
        m[8] * p[0] + m[9] * p[1] + m[10] * p[2] + m[11],
    ]
}

/// General 4x4 inverse (Gauss-Jordan with partial pivoting). `None` for a singular matrix.
pub fn mat_inverse(m: &Mat4) -> Option<Mat4> {
    let mut a = *m;
    let mut inv = IDENTITY;
    for col in 0..4 {
        let pivot =
            (col..4).max_by(|&x, &y| a[x * 4 + col].abs().total_cmp(&a[y * 4 + col].abs()))?;
        if a[pivot * 4 + col].abs() < 1e-12 {
            return None;
        }
        for k in 0..4 {
            a.swap(col * 4 + k, pivot * 4 + k);
            inv.swap(col * 4 + k, pivot * 4 + k);
        }
        let d = a[col * 4 + col];
        for k in 0..4 {
            a[col * 4 + k] /= d;
            inv[col * 4 + k] /= d;
        }
        for row in 0..4 {
            if row != col {
                let f = a[row * 4 + col];
                if f != 0.0 {
                    for k in 0..4 {
                        a[row * 4 + k] -= f * a[col * 4 + k];
                        inv[row * 4 + k] -= f * inv[col * 4 + k];
                    }
                }
            }
        }
    }
    Some(inv)
}

pub fn translation_matrix(t: Vec3) -> Mat4 {
    [
        1.0, 0.0, 0.0, t[0], 0.0, 1.0, 0.0, t[1], 0.0, 0.0, 1.0, t[2], 0.0, 0.0, 0.0, 1.0,
    ]
}

/// `Rz(rz) * Ry(ry) * Rx(rx)`, radians, as the 3x3 part of a 4x4.
pub fn rotation_matrix(rx: f64, ry: f64, rz: f64) -> Mat4 {
    let (sx, cx) = rx.sin_cos();
    let (sy, cy) = ry.sin_cos();
    let (sz, cz) = rz.sin_cos();
    [
        cz * cy,
        cz * sy * sx - sz * cx,
        cz * sy * cx + sz * sx,
        0.0,
        sz * cy,
        sz * sy * sx + cz * cx,
        sz * sy * cx - cz * sx,
        0.0,
        -sy,
        cy * sx,
        cy * cx,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
    ]
}

/// Rigid transform `x -> R (x - c) + c + t`.
pub fn rigid_about(rx: f64, ry: f64, rz: f64, t: Vec3, c: Vec3) -> Mat4 {
    let r = rotation_matrix(rx, ry, rz);
    let rc = transform_point(&r, c);
    let mut m = r;
    m[3] = c[0] - rc[0] + t[0];
    m[7] = c[1] - rc[1] + t[1];
    m[11] = c[2] - rc[2] + t[2];
    m
}

/// Euler angles (degrees) of a rotation `R = Rz * Ry * Rx` (the inverse of `rotation_matrix`).
pub fn euler_deg(m: &Mat4) -> Vec3 {
    let ry = (-m[8]).clamp(-1.0, 1.0).asin();
    let (rx, rz) = if ry.cos().abs() > 1e-9 {
        (m[9].atan2(m[10]), m[4].atan2(m[0]))
    } else {
        ((-m[6]).atan2(m[5]), 0.0)
    };
    [rx.to_degrees(), ry.to_degrees(), rz.to_degrees()]
}

/// The rotation angle (degrees) of the 3x3 part of `m` (assumed orthonormal).
pub fn rotation_angle_deg(m: &Mat4) -> f64 {
    let trace = m[0] + m[5] + m[10];
    ((trace - 1.0) / 2.0).clamp(-1.0, 1.0).acos().to_degrees()
}

// ---------------------------------------------------------------------------------------------
// Working images
// ---------------------------------------------------------------------------------------------

/// One volume resampled onto a near-isotropic grid in its own orientation (module docs, step 1).
struct WorkImage {
    /// `[slice][row][col]`, `dims = [cols, rows, slices]`.
    data: Vec<f32>,
    dims: [usize; 3],
    spacing: Vec3,
    origin: Vec3,
    /// Unit axes: column, row, slice directions (patient LPS).
    axes: [Vec3; 3],
}

impl WorkImage {
    fn len(&self) -> usize {
        self.dims[0] * self.dims[1] * self.dims[2]
    }

    fn world(&self, i: usize, j: usize, k: usize) -> Vec3 {
        add(
            add(
                add(self.origin, scale(self.axes[0], i as f64 * self.spacing[0])),
                scale(self.axes[1], j as f64 * self.spacing[1]),
            ),
            scale(self.axes[2], k as f64 * self.spacing[2]),
        )
    }

    /// Row-major 3x4 affine (as a 4x4) from patient mm to continuous voxel index `(i, j, k)`.
    fn world_to_index(&self) -> Mat4 {
        let mut m = IDENTITY;
        for axis in 0..3 {
            let a = scale(self.axes[axis], 1.0 / self.spacing[axis]);
            m[axis * 4] = a[0];
            m[axis * 4 + 1] = a[1];
            m[axis * 4 + 2] = a[2];
            m[axis * 4 + 3] = -dot(a, self.origin);
        }
        m
    }

    /// Trilinear sample at a continuous index, `None` outside `[0, n - 1]` on any axis (a
    /// one-voxel-thick axis accepts only its own plane, +-0.5).
    #[inline]
    fn sample(&self, index: Vec3) -> Option<f32> {
        let [nx, ny, nz] = self.dims;
        let mut base = [0usize; 3];
        let mut frac = [0f32; 3];
        for axis in 0..3 {
            let n = self.dims[axis];
            let v = index[axis];
            if n == 1 {
                if v.abs() > 0.5 {
                    return None;
                }
                continue;
            }
            if !(v >= 0.0 && v <= (n - 1) as f64) {
                return None;
            }
            let b = (v.floor() as usize).min(n - 2);
            base[axis] = b;
            frac[axis] = (v - b as f64) as f32;
        }
        let sx = if nx > 1 { 1 } else { 0 };
        let sy = if ny > 1 { nx } else { 0 };
        let sz = if nz > 1 { nx * ny } else { 0 };
        let i0 = base[2] * nx * ny + base[1] * nx + base[0];
        let d = &self.data;
        let (fx, fy, fz) = (frac[0], frac[1], frac[2]);
        let c00 = d[i0] + (d[i0 + sx] - d[i0]) * fx;
        let c10 = d[i0 + sy] + (d[i0 + sy + sx] - d[i0 + sy]) * fx;
        let c01 = d[i0 + sz] + (d[i0 + sz + sx] - d[i0 + sz]) * fx;
        let c11 = d[i0 + sz + sy] + (d[i0 + sz + sy + sx] - d[i0 + sz + sy]) * fx;
        let c0 = c00 + (c10 - c00) * fy;
        let c1 = c01 + (c11 - c01) * fy;
        Some(c0 + (c1 - c0) * fz)
    }
}

/// In-plane block factors and the slice spacing a working image at `spacing` mm would get.
fn work_grid(volume: &Volume, spacing: f64) -> ([usize; 2], [usize; 3], f64) {
    let kx = ((spacing / volume.col_spacing_mm).round() as usize).max(1);
    let ky = ((spacing / volume.row_spacing_mm).round() as usize).max(1);
    let nx = (volume.cols as usize).div_ceil(kx);
    let ny = (volume.rows as usize).div_ceil(ky);
    let span = volume.slice_zs[volume.slice_zs.len() - 1] - volume.slice_zs[0];
    let nominal = volume.nominal_slice_spacing_mm().max(1e-3);
    let sz = spacing.max(nominal);
    let nz = if volume.num_slices > 1 {
        (span / sz + 1e-6).floor() as usize + 1
    } else {
        1
    };
    ([kx, ky], [nx, ny, nz], sz)
}

fn work_image_bytes(volume: &Volume, spacing: f64) -> u64 {
    let (_, [nx, ny, nz], _) = work_grid(volume, spacing);
    // Final grid (f32 data, u8 masks x3, normalized copy) plus the in-plane-binned slices.
    (nx * ny * nz) as u64 * 11 + (nx * ny * volume.num_slices as usize) as u64 * 4
}

fn build_work_image(volume: &Volume, spacing: f64) -> WorkImage {
    let ([kx, ky], [nx, ny, nz], sz) = work_grid(volume, spacing);
    let cols = volume.cols as usize;
    let rows = volume.rows as usize;
    let plane = rows * cols;
    let out_plane = nx * ny;
    let slices = volume.num_slices as usize;

    // In-plane block averages of every slice.
    let mut binned = vec![0f32; out_plane * slices];
    binned
        .par_chunks_mut(out_plane)
        .enumerate()
        .for_each(|(s, out)| {
            let src = &volume.samples[s * plane..(s + 1) * plane];
            for by in 0..ny {
                let r0 = by * ky;
                let r1 = (r0 + ky).min(rows);
                for bx in 0..nx {
                    let c0 = bx * kx;
                    let c1 = (c0 + kx).min(cols);
                    let mut sum = 0f64;
                    for r in r0..r1 {
                        sum += src[r * cols + c0..r * cols + c1]
                            .iter()
                            .map(|&v| f64::from(v))
                            .sum::<f64>();
                    }
                    out[by * nx + bx] = (sum / ((r1 - r0) * (c1 - c0)) as f64) as f32;
                }
            }
        });

    // Along the normal: the mean of the slices within half a working step of each target
    // position, or linear interpolation between the two bracketing slices when none is.
    let zs = &volume.slice_zs;
    let mut data = vec![0f32; out_plane * nz];
    data.par_chunks_mut(out_plane)
        .enumerate()
        .for_each(|(k, out)| {
            let z = zs[0] + k as f64 * sz;
            let lo = zs.partition_point(|&v| v < z - sz / 2.0);
            let hi = zs.partition_point(|&v| v < z + sz / 2.0);
            if hi > lo {
                let count = (hi - lo) as f32;
                for s in lo..hi {
                    for (o, &v) in out
                        .iter_mut()
                        .zip(&binned[s * out_plane..(s + 1) * out_plane])
                    {
                        *o += v;
                    }
                }
                out.iter_mut().for_each(|o| *o /= count);
            } else {
                let upper = zs.partition_point(|&v| v < z).clamp(1, slices - 1);
                let lower = upper - 1;
                let t =
                    ((z - zs[lower]) / (zs[upper] - zs[lower]).max(1e-9)).clamp(0.0, 1.0) as f32;
                let a = &binned[lower * out_plane..(lower + 1) * out_plane];
                let b = &binned[upper * out_plane..(upper + 1) * out_plane];
                for ((o, &va), &vb) in out.iter_mut().zip(a).zip(b) {
                    *o = va + (vb - va) * t;
                }
            }
        });

    let origin = add(
        add(
            volume.origin,
            scale(
                volume.row_vector,
                (kx - 1) as f64 / 2.0 * volume.col_spacing_mm,
            ),
        ),
        scale(
            volume.col_vector,
            (ky - 1) as f64 / 2.0 * volume.row_spacing_mm,
        ),
    );
    WorkImage {
        data,
        dims: [nx, ny, nz],
        spacing: [
            kx as f64 * volume.col_spacing_mm,
            ky as f64 * volume.row_spacing_mm,
            if nz > 1 { sz } else { 1.0 },
        ],
        origin,
        axes: [volume.row_vector, volume.col_vector, volume.slice_normal],
    }
}

// ---------------------------------------------------------------------------------------------
// Masks
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModalityClass {
    Ct,
    Mr,
    Pt,
    Other,
}

fn modality_class(modality: Option<&str>) -> ModalityClass {
    match modality.map(|m| m.trim().to_ascii_uppercase()).as_deref() {
        Some("CT") => ModalityClass::Ct,
        Some("MR") => ModalityClass::Mr,
        Some("PT") | Some("NM") => ModalityClass::Pt,
        _ => ModalityClass::Other,
    }
}

/// Value at quantile `q` (0..1) of `values` (selected by `keep`), from a 4096-bin histogram.
fn quantile(values: &[f32], keep: impl Fn(usize) -> bool, q: f64) -> Option<f32> {
    let (mut lo, mut hi, mut count) = (f32::INFINITY, f32::NEG_INFINITY, 0usize);
    for (index, &v) in values.iter().enumerate() {
        if keep(index) && v.is_finite() {
            lo = lo.min(v);
            hi = hi.max(v);
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    if hi <= lo {
        return Some(lo);
    }
    const BINS: usize = 4096;
    let width = (hi - lo) / BINS as f32;
    let mut histogram = vec![0usize; BINS];
    for (index, &v) in values.iter().enumerate() {
        if keep(index) && v.is_finite() {
            histogram[(((v - lo) / width) as usize).min(BINS - 1)] += 1;
        }
    }
    let target = (q * count as f64).ceil().max(1.0) as usize;
    let mut seen = 0;
    for (bin, &n) in histogram.iter().enumerate() {
        seen += n;
        if seen >= target {
            return Some(lo + (bin as f32 + 0.5) * width);
        }
    }
    Some(hi)
}

/// Otsu's threshold of `values` within `[lo, hi]`.
fn otsu(values: &[f32], lo: f32, hi: f32) -> f32 {
    const BINS: usize = 256;
    if hi <= lo {
        return lo;
    }
    let width = (hi - lo) / BINS as f32;
    let mut histogram = [0f64; BINS];
    for &v in values {
        if v.is_finite() {
            histogram[(((v.clamp(lo, hi) - lo) / width) as usize).min(BINS - 1)] += 1.0;
        }
    }
    let total: f64 = histogram.iter().sum();
    let sum_all: f64 = histogram
        .iter()
        .enumerate()
        .map(|(i, &n)| i as f64 * n)
        .sum();
    let (mut weight_b, mut sum_b, mut best, mut best_bin) = (0.0, 0.0, -1.0, 0);
    for (bin, &n) in histogram.iter().enumerate() {
        weight_b += n;
        if weight_b == 0.0 {
            continue;
        }
        let weight_f = total - weight_b;
        if weight_f == 0.0 {
            break;
        }
        sum_b += bin as f64 * n;
        let mean_b = sum_b / weight_b;
        let mean_f = (sum_all - sum_b) / weight_f;
        let between = weight_b * weight_f * (mean_b - mean_f).powi(2);
        if between > best {
            best = between;
            best_bin = bin;
        }
    }
    lo + (best_bin as f32 + 1.0) * width
}

fn neighbors(dims: [usize; 3], index: usize, mut f: impl FnMut(usize)) {
    let [nx, ny, nz] = dims;
    let i = index % nx;
    let j = (index / nx) % ny;
    let k = index / (nx * ny);
    if i > 0 {
        f(index - 1);
    }
    if i + 1 < nx {
        f(index + 1);
    }
    if j > 0 {
        f(index - nx);
    }
    if j + 1 < ny {
        f(index + nx);
    }
    if k > 0 {
        f(index - nx * ny);
    }
    if k + 1 < nz {
        f(index + nx * ny);
    }
}

fn erode(mask: &[u8], dims: [usize; 3]) -> Vec<u8> {
    (0..mask.len())
        .map(|index| {
            if mask[index] == 0 {
                return 0;
            }
            let mut keep = 1;
            neighbors(dims, index, |n| {
                if mask[n] == 0 {
                    keep = 0;
                }
            });
            keep
        })
        .collect()
}

fn dilate(mask: &[u8], dims: [usize; 3]) -> Vec<u8> {
    (0..mask.len())
        .map(|index| {
            if mask[index] != 0 {
                return 1;
            }
            let mut set = 0;
            neighbors(dims, index, |n| {
                if mask[n] != 0 {
                    set = 1;
                }
            });
            set
        })
        .collect()
}

/// The largest 6-connected component of `mask`.
fn largest_component(mask: &[u8], dims: [usize; 3]) -> Vec<u8> {
    let mut label = vec![0u32; mask.len()];
    let mut best = (0u32, 0usize);
    let mut next = 0u32;
    let mut stack = Vec::new();
    for start in 0..mask.len() {
        if mask[start] == 0 || label[start] != 0 {
            continue;
        }
        next += 1;
        label[start] = next;
        stack.push(start);
        let mut size = 0usize;
        while let Some(index) = stack.pop() {
            size += 1;
            neighbors(dims, index, |n| {
                if mask[n] != 0 && label[n] == 0 {
                    label[n] = next;
                    stack.push(n);
                }
            });
        }
        if size > best.1 {
            best = (next, size);
        }
    }
    label
        .iter()
        .map(|&l| u8::from(l != 0 && l == best.0))
        .collect()
}

/// Fills enclosed holes: everything not reachable from the grid's side faces through
/// background. The two faces of the axis closest to patient S-I are not seeds, so a lung or
/// airway cut by the top or bottom of the scan still counts as enclosed.
fn fill_holes(mask: &[u8], dims: [usize; 3], axes: &[Vec3; 3]) -> Vec<u8> {
    let si_axis = (0..3)
        .max_by(|&a, &b| axes[a][2].abs().total_cmp(&axes[b][2].abs()))
        .unwrap_or(2);
    let [nx, ny, nz] = dims;
    let mut outside = vec![false; mask.len()];
    let mut stack = Vec::new();
    for k in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                let on_face = [(i, nx), (j, ny), (k, nz)]
                    .iter()
                    .enumerate()
                    .any(|(axis, &(v, n))| axis != si_axis && (v == 0 || v + 1 == n));
                let index = k * nx * ny + j * nx + i;
                if on_face && mask[index] == 0 && !outside[index] {
                    outside[index] = true;
                    stack.push(index);
                }
            }
        }
    }
    while let Some(index) = stack.pop() {
        neighbors(dims, index, |n| {
            if mask[n] == 0 && !outside[n] {
                outside[n] = true;
                stack.push(n);
            }
        });
    }
    outside.iter().map(|&o| u8::from(!o)).collect()
}

/// The filled body mask of a working image (module docs, step 2).
fn body_mask(image: &WorkImage, class: ModalityClass) -> Vec<u8> {
    let dims = image.dims;
    let data = &image.data;
    let threshold = match class {
        ModalityClass::Ct => -400.0,
        ModalityClass::Pt => {
            // 30 % of the median uptake of what is above 5 % of the 99.5th percentile: the
            // 5 % level alone lets the speckle outside the body of a noisy scan through.
            let floor = quantile(data, |_| true, 0.995).unwrap_or(0.0) * 0.05;
            quantile(data, |i| data[i] > floor, 0.5)
                .map_or(floor, |median| (0.3 * median).max(floor))
        }
        ModalityClass::Mr | ModalityClass::Other => {
            let lo = quantile(data, |_| true, 0.0).unwrap_or(0.0);
            let hi = quantile(data, |_| true, 0.995).unwrap_or(1.0);
            lo + (otsu(data, lo, hi) - lo) * 0.5
        }
    };
    let foreground: Vec<u8> = data.iter().map(|&v| u8::from(v > threshold)).collect();
    let body = if matches!(class, ModalityClass::Ct | ModalityClass::Pt) {
        // Opening by one voxel detaches the table (and blankets, cables; PET speckle) before
        // picking the patient as the largest component.
        dilate(&largest_component(&erode(&foreground, dims), dims), dims)
    } else {
        largest_component(&foreground, dims)
    };
    let filled = fill_holes(&body, dims, &image.axes);
    // A CT reconstructed with a smaller field of view than the patient has its lungs cut open
    // at the FOV edge; they are still inside the patient.
    if class == ModalityClass::Ct {
        fill_in_plane(&filled, dims)
    } else {
        filled
    }
}

/// Also counts as body every voxel between two body voxels of its slice's row or column: a lung
/// or bowel loop that a reconstruction field of view cut open (so `fill_holes` sees it reach the
/// outside) is still inside the patient.
fn fill_in_plane(mask: &[u8], dims: [usize; 3]) -> Vec<u8> {
    let [nx, ny, nz] = dims;
    let mut out = mask.to_vec();
    for k in 0..nz {
        let slice = &mask[k * nx * ny..(k + 1) * nx * ny];
        let target = &mut out[k * nx * ny..(k + 1) * nx * ny];
        let mut rows = vec![(usize::MAX, 0usize); ny];
        let mut cols = vec![(usize::MAX, 0usize); nx];
        for j in 0..ny {
            for i in 0..nx {
                if slice[j * nx + i] != 0 {
                    rows[j] = (rows[j].0.min(i), rows[j].1.max(i));
                    cols[i] = (cols[i].0.min(j), cols[i].1.max(j));
                }
            }
        }
        for j in 0..ny {
            for i in 0..nx {
                let (r0, r1) = rows[j];
                let (c0, c1) = cols[i];
                if (r0 <= i && i <= r1) || (c0 <= j && j <= c1) {
                    target[j * nx + i] = 1;
                }
            }
        }
    }
    out
}

fn apply_roi(mask: &mut [u8], image: &WorkImage, roi: Option<(Vec3, Vec3)>) {
    let Some((lo, hi)) = roi else { return };
    let [nx, ny, nz] = image.dims;
    for k in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                let p = image.world(i, j, k);
                if (0..3).any(|a| p[a] < lo[a].min(hi[a]) || p[a] > lo[a].max(hi[a])) {
                    mask[k * nx * ny + j * nx + i] = 0;
                }
            }
        }
    }
}

/// Intensities inside `mask` clamped to their 0.5-99.5 percentiles and mapped to [0, 1]; 0
/// outside (module docs, step 3).
fn normalize(image: &WorkImage, mask: &[u8]) -> Vec<f32> {
    let lo = quantile(&image.data, |i| mask[i] != 0, 0.005).unwrap_or(0.0);
    let hi = quantile(&image.data, |i| mask[i] != 0, 0.995).unwrap_or(1.0);
    let range = (hi - lo).max(1e-6);
    image
        .data
        .iter()
        .zip(mask)
        .map(|(&v, &m)| {
            if m == 0 {
                0.0
            } else {
                ((v - lo) / range).clamp(0.0, 1.0)
            }
        })
        .collect()
}

/// A working image with its masks, ready for the metric.
struct Prepared {
    image: WorkImage,
    /// Filled body mask (no margin).
    body: Vec<u8>,
    /// Body mask plus one voxel: where samples come from and normalized data is non-zero.
    sampling: Vec<u8>,
}

fn prepare(
    volume: &Volume,
    spacing: f64,
    class: ModalityClass,
    mask_mode: MaskMode,
    roi: Option<(Vec3, Vec3)>,
) -> Prepared {
    let mut image = build_work_image(volume, spacing);
    let mut body = match mask_mode {
        MaskMode::Auto => body_mask(&image, class),
        MaskMode::None => vec![1u8; image.len()],
    };
    apply_roi(&mut body, &image, roi);
    let sampling = if mask_mode == MaskMode::Auto {
        dilate(&body, image.dims)
    } else {
        body.clone()
    };
    image.data = normalize(&image, &sampling);
    Prepared {
        image,
        body,
        sampling,
    }
}

fn mask_centroid(prepared: &Prepared) -> Option<Vec3> {
    let [nx, ny, nz] = prepared.image.dims;
    let (mut sum, mut count) = ([0.0; 3], 0usize);
    for k in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                if prepared.body[k * nx * ny + j * nx + i] != 0 {
                    sum = add(sum, prepared.image.world(i, j, k));
                    count += 1;
                }
            }
        }
    }
    (count > 0).then(|| scale(sum, 1.0 / count as f64))
}

// ---------------------------------------------------------------------------------------------
// Metric
// ---------------------------------------------------------------------------------------------

/// splitmix64: the deterministic hash that picks the metric's sample points.
fn hash64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

struct Samples {
    points: Vec<[f32; 3]>,
    values: Vec<f32>,
}

fn select_samples(fixed: &Prepared, max_samples: usize) -> Samples {
    let image = &fixed.image;
    let candidates = fixed.sampling.iter().filter(|&&m| m != 0).count();
    let keep_one_in = candidates.div_ceil(max_samples.max(1)).max(1) as u64;
    let [nx, ny, _] = image.dims;
    let mut points = Vec::new();
    let mut values = Vec::new();
    for (index, &m) in fixed.sampling.iter().enumerate() {
        if m == 0 || (keep_one_in > 1 && !hash64(index as u64).is_multiple_of(keep_one_in)) {
            continue;
        }
        let (i, j, k) = (index % nx, (index / nx) % ny, index / (nx * ny));
        let p = image.world(i, j, k);
        points.push([p[0] as f32, p[1] as f32, p[2] as f32]);
        values.push(image.data[index]);
    }
    Samples { points, values }
}

#[derive(Clone, Copy)]
struct MetricValue {
    /// What the optimizer minimizes.
    cost: f64,
    /// NCC correlation, or MI (nats).
    value: f64,
    /// NCC correlation, or entropy correlation coefficient.
    normalized: f64,
    overlap: f64,
}

/// Cubic B-spline kernel.
#[inline]
fn bspline3(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        (4.0 - 6.0 * x * x + 3.0 * x * x * x) / 6.0
    } else if x < 2.0 {
        let t = 2.0 - x;
        t * t * t / 6.0
    } else {
        0.0
    }
}

fn evaluate_metric(
    kind: MetricKind,
    bins: usize,
    samples: &Samples,
    moving: &WorkImage,
    fixed_to_moving_index: &Mat4,
) -> MetricValue {
    let m = fixed_to_moving_index;
    let total = samples.points.len();
    let map = |p: &[f32; 3]| -> Vec3 {
        let p = [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])];
        transform_point(m, p)
    };
    match kind {
        MetricKind::Ncc => {
            let partials: Vec<[f64; 6]> = samples
                .points
                .par_chunks(REDUCTION_CHUNK)
                .zip(samples.values.par_chunks(REDUCTION_CHUNK))
                .map(|(points, values)| {
                    let mut acc = [0.0; 6];
                    for (p, &f) in points.iter().zip(values) {
                        if let Some(v) = moving.sample(map(p)) {
                            let (f, v) = (f64::from(f), f64::from(v));
                            acc[0] += 1.0;
                            acc[1] += f;
                            acc[2] += v;
                            acc[3] += f * f;
                            acc[4] += v * v;
                            acc[5] += f * v;
                        }
                    }
                    acc
                })
                .collect();
            let mut acc = [0.0; 6];
            for part in partials {
                for (a, p) in acc.iter_mut().zip(part) {
                    *a += p;
                }
            }
            let n = acc[0];
            let overlap = n / total.max(1) as f64;
            if n < 16.0 {
                return MetricValue {
                    cost: 1.0,
                    value: 0.0,
                    normalized: 0.0,
                    overlap,
                };
            }
            let cov = acc[5] - acc[1] * acc[2] / n;
            let var_f = acc[3] - acc[1] * acc[1] / n;
            let var_m = acc[4] - acc[2] * acc[2] / n;
            let corr = if var_f > 1e-12 && var_m > 1e-12 {
                cov / (var_f * var_m).sqrt()
            } else {
                0.0
            };
            // Too little overlap is never a good alignment, however well the remnant correlates.
            let penalty = if overlap < 0.1 {
                1.0 - overlap / 0.1
            } else {
                0.0
            };
            MetricValue {
                cost: -corr + penalty,
                value: corr,
                normalized: corr,
                overlap,
            }
        }
        MetricKind::MattesMi => {
            let b = bins.max(8);
            let usable = (b - 4) as f64;
            let partials: Vec<(Vec<f64>, f64)> = samples
                .points
                .par_chunks(REDUCTION_CHUNK)
                .zip(samples.values.par_chunks(REDUCTION_CHUNK))
                .map(|(points, values)| {
                    let mut joint = vec![0.0; b * b];
                    let mut n = 0.0;
                    for (p, &f) in points.iter().zip(values) {
                        if let Some(v) = moving.sample(map(p)) {
                            let fb = ((f64::from(f) * usable) as usize + 2).min(b - 3);
                            let u = f64::from(v).clamp(0.0, 1.0) * usable + 2.0;
                            let base = u.floor() as isize;
                            for offset in -1..=2isize {
                                let bin = base + offset;
                                if bin >= 0 && (bin as usize) < b {
                                    joint[fb * b + bin as usize] += bspline3(u - bin as f64);
                                }
                            }
                            n += 1.0;
                        }
                    }
                    (joint, n)
                })
                .collect();
            let mut joint = vec![0.0; b * b];
            let mut n = 0.0;
            for (part, count) in partials {
                for (a, p) in joint.iter_mut().zip(part) {
                    *a += p;
                }
                n += count;
            }
            let overlap = n / total.max(1) as f64;
            if n < 16.0 {
                return MetricValue {
                    cost: 1.0,
                    value: 0.0,
                    normalized: 0.0,
                    overlap,
                };
            }
            let sum: f64 = joint.iter().sum();
            let mut pf = vec![0.0; b];
            let mut pm = vec![0.0; b];
            for i in 0..b {
                for j in 0..b {
                    let p = joint[i * b + j] / sum;
                    joint[i * b + j] = p;
                    pf[i] += p;
                    pm[j] += p;
                }
            }
            let entropy =
                |p: &[f64]| -> f64 { p.iter().filter(|&&x| x > 0.0).map(|&x| -x * x.ln()).sum() };
            let (hf, hm) = (entropy(&pf), entropy(&pm));
            let mut mi = 0.0;
            for i in 0..b {
                for j in 0..b {
                    let p = joint[i * b + j];
                    if p > 0.0 && pf[i] > 0.0 && pm[j] > 0.0 {
                        mi += p * (p / (pf[i] * pm[j])).ln();
                    }
                }
            }
            let ecc = if hf + hm > 0.0 {
                2.0 * mi / (hf + hm)
            } else {
                0.0
            };
            let penalty = if overlap < 0.1 {
                1.0 - overlap / 0.1
            } else {
                0.0
            };
            MetricValue {
                cost: -mi + penalty * mi.abs().max(0.1),
                value: mi,
                normalized: ecc,
                overlap,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Optimizer
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
enum Stop {
    TimeLimit,
    Cancelled,
}

struct Budget {
    deadline: Option<Instant>,
    cancel: Option<Arc<AtomicBool>>,
}

impl Budget {
    fn check(&self) -> Result<(), Stop> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err(Stop::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Stop::TimeLimit);
        }
        Ok(())
    }
}

/// Cost function over the 6 scaled parameters of one level, remembering the best point seen.
struct LevelObjective<'a> {
    kind: MetricKind,
    bins: usize,
    samples: &'a Samples,
    moving: &'a WorkImage,
    moving_world_to_index: Mat4,
    /// fixed -> moving transform the level starts from.
    start: Mat4,
    center: Vec3,
    radius: f64,
    budget: &'a Budget,
    evaluations: u32,
    best: ([f64; 6], f64),
}

impl LevelObjective<'_> {
    /// fixed -> moving world transform for parameters `q`: `start * D(q)`, where `D` rotates
    /// about the centre by `q[0..3] / radius` radians and translates by `q[3..6]` mm.
    fn transform(&self, q: &[f64; 6]) -> Mat4 {
        let delta = rigid_about(
            q[0] / self.radius,
            q[1] / self.radius,
            q[2] / self.radius,
            [q[3], q[4], q[5]],
            self.center,
        );
        mat_mul(&self.start, &delta)
    }

    fn metric(&self, q: &[f64; 6]) -> MetricValue {
        let to_index = mat_mul(&self.moving_world_to_index, &self.transform(q));
        evaluate_metric(self.kind, self.bins, self.samples, self.moving, &to_index)
    }

    fn cost(&mut self, q: &[f64; 6]) -> Result<f64, Stop> {
        self.budget.check()?;
        self.evaluations += 1;
        let cost = self.metric(q).cost;
        if cost < self.best.1 {
            self.best = (*q, cost);
        }
        Ok(cost)
    }
}

fn along(p: &[f64; 6], d: &[f64; 6], t: f64) -> [f64; 6] {
    let mut out = *p;
    for i in 0..6 {
        out[i] += d[i] * t;
    }
    out
}

/// Minimizes `cost(p + t d)` over `t`: bracketing from `[0, step]`, then Brent's method to
/// `tolerance` (in parameter units). Returns `(t, cost)`.
fn line_minimize(
    objective: &mut LevelObjective,
    p: &[f64; 6],
    d: &[f64; 6],
    f0: f64,
    step: f64,
    tolerance: f64,
) -> Result<(f64, f64), Stop> {
    const GOLD: f64 = 1.618_033_988_749_895;
    const MAX_EXPANSIONS: usize = 12;
    // Bracket a < b < c (in t) with f(b) below both ends.
    let (mut a, mut fa) = (0.0, f0);
    let (mut b, mut fb) = (step, objective.cost(&along(p, d, step))?);
    if fb > fa {
        let (t, ft) = (-step, objective.cost(&along(p, d, -step))?);
        if ft >= fa {
            // The minimum is within one step of 0: bracket [-step, step].
            return brent(
                objective,
                p,
                d,
                (-step, ft),
                (0.0, f0),
                (step, fb),
                tolerance,
            );
        }
        // Downhill is the negative direction.
        b = t;
        fb = ft;
        a = 0.0;
        fa = f0;
    }
    let mut c = b + GOLD * (b - a);
    let mut fc = objective.cost(&along(p, d, c))?;
    let mut expansions = 0;
    while fc < fb && expansions < MAX_EXPANSIONS {
        a = b;
        fa = fb;
        b = c;
        fb = fc;
        c = b + GOLD * (b - a);
        fc = objective.cost(&along(p, d, c))?;
        expansions += 1;
    }
    if fc < fb {
        return Ok((c, fc));
    }
    brent(objective, p, d, (a, fa), (b, fb), (c, fc), tolerance)
}

/// Brent's parabolic/golden-section minimization within the bracket `(a, b, c)`.
fn brent(
    objective: &mut LevelObjective,
    p: &[f64; 6],
    d: &[f64; 6],
    (a, _fa): (f64, f64),
    (b, fb): (f64, f64),
    (c, _fc): (f64, f64),
    tolerance: f64,
) -> Result<(f64, f64), Stop> {
    const CGOLD: f64 = 0.381_966_011_250_105;
    const MAX_STEPS: usize = 20;
    let (mut lo, mut hi) = (a.min(c), a.max(c));
    let (mut x, mut w, mut v) = (b, b, b);
    let (mut fx, mut fw, mut fv) = (fb, fb, fb);
    let (mut step, mut previous): (f64, f64) = (0.0, 0.0);
    for _ in 0..MAX_STEPS {
        let mid = 0.5 * (lo + hi);
        let tol1 = tolerance * 0.5 + 1e-10;
        let tol2 = 2.0 * tol1;
        if (x - mid).abs() <= tol2 - 0.5 * (hi - lo) {
            break;
        }
        let mut golden = true;
        if previous.abs() > tol1 {
            let r = (x - w) * (fx - fv);
            let mut q = (x - v) * (fx - fw);
            let mut pp = (x - v) * q - (x - w) * r;
            q = 2.0 * (q - r);
            if q > 0.0 {
                pp = -pp;
            }
            q = q.abs();
            let e_old = previous;
            previous = step;
            if pp.abs() < (0.5 * q * e_old).abs() && pp > q * (lo - x) && pp < q * (hi - x) {
                step = pp / q;
                let u = x + step;
                if u - lo < tol2 || hi - u < tol2 {
                    step = if mid >= x { tol1 } else { -tol1 };
                }
                golden = false;
            }
        }
        if golden {
            previous = if x >= mid { lo - x } else { hi - x };
            step = CGOLD * previous;
        }
        let u = if step.abs() >= tol1 {
            x + step
        } else {
            x + tol1.copysign(step)
        };
        let fu = objective.cost(&along(p, d, u))?;
        if fu <= fx {
            if u >= x {
                lo = x;
            } else {
                hi = x;
            }
            v = w;
            fv = fw;
            w = x;
            fw = fx;
            x = u;
            fx = fu;
        } else {
            if u < x {
                lo = u;
            } else {
                hi = u;
            }
            if fu <= fw || w == x {
                v = w;
                fv = fw;
                w = u;
                fw = fu;
            } else if fu <= fv || v == x || v == w {
                v = u;
                fv = fu;
            }
        }
    }
    Ok((x, fx))
}

/// Powell's direction-set method from `q = 0`. Returns `(q, cost, iterations, converged)`.
fn powell(
    objective: &mut LevelObjective,
    step: f64,
    tolerance: f64,
    max_iterations: u32,
) -> Result<([f64; 6], f64, u32, bool), Stop> {
    let mut directions: Vec<[f64; 6]> = (0..6)
        .map(|i| {
            let mut d = [0.0; 6];
            d[i] = 1.0;
            d
        })
        .collect();
    let mut p = [0.0; 6];
    let mut fp = objective.cost(&p)?;
    for iteration in 1..=max_iterations {
        let (p_start, f_start) = (p, fp);
        let (mut biggest_drop, mut biggest_index) = (0.0, 0);
        for (index, d) in directions.iter().enumerate() {
            let before = fp;
            let (t, ft) = line_minimize(objective, &p, d, fp, step, tolerance)?;
            if ft < fp {
                p = along(&p, d, t);
                fp = ft;
            }
            if before - fp > biggest_drop {
                biggest_drop = before - fp;
                biggest_index = index;
            }
        }
        let moved: f64 = (0..6)
            .map(|i| (p[i] - p_start[i]).powi(2))
            .sum::<f64>()
            .sqrt();
        if 2.0 * (f_start - fp) <= 1e-6 * (f_start.abs() + fp.abs()) + 1e-9 || moved < tolerance {
            return Ok((p, fp, iteration, true));
        }
        // Powell's update: replace the direction of largest decrease by the net move, when the
        // extrapolated point says that is worthwhile.
        let mut net = [0.0; 6];
        for i in 0..6 {
            net[i] = p[i] - p_start[i];
        }
        let extrapolated = along(&p, &net, 1.0);
        let fe = objective.cost(&extrapolated)?;
        if fe < f_start {
            let t = 2.0 * (f_start - 2.0 * fp + fe) * (f_start - fp - biggest_drop).powi(2)
                - biggest_drop * (f_start - fe).powi(2);
            if t < 0.0 {
                let norm = moved.max(1e-12);
                let unit = net.map(|x| x / norm);
                let (t_line, ft) = line_minimize(objective, &p, &unit, fp, step, tolerance)?;
                if ft < fp {
                    p = along(&p, &unit, t_line);
                    fp = ft;
                }
                directions.remove(biggest_index);
                directions.push(unit);
            }
        }
    }
    Ok((p, fp, max_iterations, false))
}

// ---------------------------------------------------------------------------------------------
// Rigid registration
// ---------------------------------------------------------------------------------------------

fn parse_modality(explicit: &Option<String>, volume: &Volume) -> ModalityClass {
    modality_class(explicit.as_deref().or(volume.modality.as_deref()))
}

fn validate_options(options: &RegistrationOptions) -> Result<(), RegistrationError> {
    if options.levels_mm.is_empty()
        || options
            .levels_mm
            .iter()
            .any(|&s| !(s.is_finite() && s > 0.0))
    {
        return Err(RegistrationError::InvalidOption(
            "levelsMm must be a non-empty list of positive spacings".into(),
        ));
    }
    if !(8..=256).contains(&options.histogram_bins) {
        return Err(RegistrationError::InvalidOption(
            "histogramBins must be between 8 and 256".into(),
        ));
    }
    if options.max_samples < 1000 {
        return Err(RegistrationError::InvalidOption(
            "maxSamples must be at least 1000".into(),
        ));
    }
    if let Some(m) = &options.initial_matrix {
        if m.iter().any(|v| !v.is_finite()) || mat_inverse(m).is_none() {
            return Err(RegistrationError::InvalidOption(
                "initialMatrix must be 16 finite numbers and invertible".into(),
            ));
        }
    }
    Ok(())
}

/// Coarsens `spacing` until both working images fit in `budget` bytes.
fn fit_spacing(fixed: &Volume, moving: &Volume, spacing: f64, budget: u64) -> f64 {
    let mut s = spacing;
    for _ in 0..16 {
        if work_image_bytes(fixed, s) + work_image_bytes(moving, s) <= budget {
            break;
        }
        s *= 1.25;
    }
    s
}

/// Two multi-start results agree when they move the corners of the fixed body's central box
/// (the middle 60 % of its bounding box) by less than this many mm from each other, on average.
pub const AGREE_MM: f64 = 3.0;
/// A start closer than this (mm, same measure) to an earlier one is not run again.
const DUPLICATE_START_MM: f64 = 2.0;

/// The metric's correlation restricted to where BOTH bodies are: fixed samples whose mapped point
/// falls inside the moving body mask. `(normalized, share of the fixed samples in the moving
/// body)`. Unlike the optimizer's metric, this does not count the fixed anatomy the moving scan
/// never saw (a smaller reconstruction field of view, a cropped end) against the alignment.
fn masked_metric(
    kind: MetricKind,
    bins: usize,
    samples: &Samples,
    moving: &Prepared,
    fixed_to_moving_index: &Mat4,
) -> (f64, f64) {
    let image = &moving.image;
    let [nx, ny, nz] = image.dims;
    let in_body = |index: Vec3| -> bool {
        let (i, j, k) = (index[0].round(), index[1].round(), index[2].round());
        if i < 0.0 || j < 0.0 || k < 0.0 || i >= nx as f64 || j >= ny as f64 || k >= nz as f64 {
            return false;
        }
        moving.body[k as usize * nx * ny + j as usize * nx + i as usize] != 0
    };
    let mut points = Vec::new();
    let mut values = Vec::new();
    for (p, &v) in samples.points.iter().zip(&samples.values) {
        let index = transform_point(
            fixed_to_moving_index,
            [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])],
        );
        if in_body(index) {
            points.push(*p);
            values.push(v);
        }
    }
    let share = points.len() as f64 / samples.points.len().max(1) as f64;
    if points.len() < 64 {
        return (0.0, share);
    }
    let subset = Samples { points, values };
    (
        evaluate_metric(kind, bins, &subset, image, fixed_to_moving_index).normalized,
        share,
    )
}

/// Quality in 0..1 from the masked metric (`masked_metric`) and the share of the fixed body the
/// moving body covers (scaled down below 30 %). Calibrated on public CT, PT and MR pairs
/// (benchmarks/registration), started both near and far (100-180 mm, 60 mm in-plane, 25 and 180
/// degrees) from the truth: correct results, FOV-truncated and cropped ones included, had a
/// masked NCC of 0.92+ for CT, 0.73+ for MR and 0.52+ for PET months apart; divergent ones 0.44
/// or less for CT/MR and 0.35 or less for PET. So NCC 0.45 -> 0, 0.75 -> 1 (PET: 0.30 -> 0,
/// 0.55 -> 1); the entropy correlation coefficient of MI 0.05 -> 0, 0.30 -> 1.
fn quality_score(kind: MetricKind, pet: bool, masked: f64, share: f64) -> f64 {
    let base = match (kind, pet) {
        (MetricKind::Ncc, false) => (masked - 0.45) / 0.3,
        (MetricKind::Ncc, true) => (masked - 0.30) / 0.25,
        (MetricKind::MattesMi, _) => (masked - 0.05) / 0.25,
    };
    base.clamp(0.0, 1.0) * (share / 0.3).clamp(0.0, 1.0)
}

fn quality_label(quality: f64) -> &'static str {
    if quality >= 0.7 {
        "good"
    } else if quality >= 0.4 {
        "fair"
    } else {
        "poor"
    }
}

/// Registers `moving` to `fixed` (module docs). Returns the moving -> fixed patient transform.
///
/// With `Initialization::Multi` (or a non-empty `starts`), several starts are tried: the
/// `initial_matrix` and `starts` given, the translation estimate (whatever its confidence), the
/// body-mask centroids and identity, less any within 2 mm of an earlier one. Each runs the
/// coarse levels; the plausible one (rotation within `max_rotation_deg`) with the best metric is
/// refined at the finest level and returned, `consistent` saying whether another start converged
/// to within `AGREE_MM` of it and `starts` listing them all. The working images are built once
/// and shared, so extra starts cost only optimizer time.
pub fn register_rigid(
    fixed: &Volume,
    moving: &Volume,
    options: &RegistrationOptions,
) -> Result<RegistrationResult, RegistrationError> {
    validate_options(options)?;
    let fixed_class = parse_modality(&options.fixed_modality, fixed);
    let moving_class = parse_modality(&options.moving_modality, moving);
    let kind = match options.metric {
        MetricChoice::Ncc => MetricKind::Ncc,
        MetricChoice::MattesMi => MetricKind::MattesMi,
        MetricChoice::Auto
            if fixed_class == moving_class && fixed_class != ModalityClass::Other =>
        {
            MetricKind::Ncc
        }
        MetricChoice::Auto => MetricKind::MattesMi,
    };
    let started = Instant::now();
    let budget = Budget {
        deadline: options.time_limit.map(|limit| started + limit),
        cancel: options.cancel.clone(),
    };
    let spacings: Vec<f64> = options
        .levels_mm
        .iter()
        .map(|&s| fit_spacing(fixed, moving, s, options.max_memory_bytes))
        .collect();
    let mut levels: Vec<Option<Level>> = (0..spacings.len()).map(|_| None).collect();
    let mut context = RigidContext {
        fixed,
        moving,
        options,
        fixed_class,
        moving_class,
        spacings,
        levels: &mut levels,
        budget: &budget,
        started,
    };
    let first = context.run(kind)?;
    // MR-MR is often two different weightings (T1 vs T2), which NCC cannot align: if NCC stays
    // poor, try mutual information from the same starts and keep the better result.
    if options.metric == MetricChoice::Auto
        && kind == MetricKind::Ncc
        && fixed_class == ModalityClass::Mr
        && first.normalized_metric < 0.5
        && first.stop_reason != StopReason::TimeLimit
    {
        if let Ok(second) = context.run(MetricKind::MattesMi) {
            if second.quality > first.quality {
                return Ok(second);
            }
        }
    }
    Ok(first)
}

fn validate_rotation(options: &RegistrationOptions) -> Result<(), RegistrationError> {
    if options
        .max_rotation_deg
        .is_some_and(|d| !(d.is_finite() && d > 0.0))
    {
        return Err(RegistrationError::InvalidOption(
            "maxRotationDeg must be a positive number".into(),
        ));
    }
    if options
        .starts
        .iter()
        .any(|m| m.iter().any(|v| !v.is_finite()) || mat_inverse(m).is_none())
    {
        return Err(RegistrationError::InvalidOption(
            "starts must be matrices of 16 finite numbers, invertible".into(),
        ));
    }
    Ok(())
}

/// One pyramid level's working images and samples, built once and shared by every start.
struct Level {
    spacing: f64,
    moving: Prepared,
    samples: Samples,
    center: Vec3,
    radius: f64,
    bytes: u64,
}

/// The outcome of optimizing one start through a range of levels.
struct LevelRun {
    /// Fixed -> moving.
    fixed_to_moving: Mat4,
    levels: Vec<LevelResult>,
    iterations: u32,
    evaluations: u32,
    stop_reason: StopReason,
    converged: bool,
    metric: MetricValue,
    /// The metric at the start transform, on the last level run.
    initial_metric: f64,
    center: Vec3,
}

struct RigidContext<'a> {
    fixed: &'a Volume,
    moving: &'a Volume,
    options: &'a RegistrationOptions,
    fixed_class: ModalityClass,
    moving_class: ModalityClass,
    spacings: Vec<f64>,
    levels: &'a mut Vec<Option<Level>>,
    budget: &'a Budget,
    started: Instant,
}

impl RigidContext<'_> {
    fn level(&mut self, index: usize) -> Result<&Level, RegistrationError> {
        if self.levels[index].is_none() {
            let spacing = self.spacings[index];
            let fixed = prepare(
                self.fixed,
                spacing,
                self.fixed_class,
                self.options.mask,
                self.options.roi,
            );
            if fixed.body.iter().all(|&m| m == 0) {
                return Err(RegistrationError::EmptyMask("fixed"));
            }
            let moving = prepare(
                self.moving,
                spacing,
                self.moving_class,
                self.options.mask,
                None,
            );
            if moving.body.iter().all(|&m| m == 0) {
                return Err(RegistrationError::EmptyMask("moving"));
            }
            let samples = select_samples(&fixed, self.options.max_samples);
            let center = mask_centroid(&fixed).unwrap_or([0.0; 3]);
            // Rotations are scaled by the body's radius of gyration, so one parameter unit moves
            // the anatomy about one millimetre whichever parameter it is.
            let radius = {
                let mut sum = 0.0;
                for p in &samples.points {
                    let p = [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])];
                    sum += dot(sub(p, center), sub(p, center));
                }
                (sum / samples.points.len().max(1) as f64).sqrt().max(20.0)
            };
            let bytes = work_image_bytes(self.fixed, spacing)
                + work_image_bytes(self.moving, spacing)
                + samples.points.len() as u64 * 16;
            self.levels[index] = Some(Level {
                spacing,
                moving,
                samples,
                center,
                radius,
                bytes,
            });
        }
        Ok(self.levels[index].as_ref().expect("just built"))
    }

    fn working_bytes(&self) -> u64 {
        self.levels.iter().flatten().map(|l| l.bytes).sum()
    }

    /// Optimizes from `start` (moving -> fixed) through levels `range`.
    fn optimize(
        &mut self,
        kind: MetricKind,
        start: &Mat4,
        range: std::ops::Range<usize>,
    ) -> Result<LevelRun, RegistrationError> {
        let mut fixed_to_moving = mat_inverse(start).ok_or_else(|| {
            RegistrationError::InvalidOption("initial transform is singular".into())
        })?;
        let mut run = LevelRun {
            fixed_to_moving,
            levels: Vec::new(),
            iterations: 0,
            evaluations: 0,
            stop_reason: StopReason::Converged,
            converged: true,
            metric: MetricValue {
                cost: 1.0,
                value: 0.0,
                normalized: 0.0,
                overlap: 0.0,
            },
            initial_metric: 0.0,
            center: [0.0; 3],
        };
        let bins = self.options.histogram_bins;
        let max_iterations = self.options.max_iterations;
        let budget = self.budget;
        for index in range {
            let level = self.level(index)?;
            let mut objective = LevelObjective {
                kind,
                bins,
                samples: &level.samples,
                moving: &level.moving.image,
                moving_world_to_index: level.moving.image.world_to_index(),
                start: fixed_to_moving,
                center: level.center,
                radius: level.radius,
                budget,
                evaluations: 0,
                best: ([0.0; 6], f64::INFINITY),
            };
            let start_metric = objective.metric(&[0.0; 6]);
            if start_metric.overlap <= 0.0 {
                return Err(RegistrationError::NoOverlap);
            }
            run.initial_metric = start_metric.value;
            let step = level.spacing;
            let tolerance = (level.spacing * 0.05).max(0.02);
            let outcome = powell(&mut objective, step, tolerance, max_iterations);
            let (q, iterations, converged) = match outcome {
                Ok((q, _, iterations, converged)) => (q, iterations, converged),
                Err(Stop::Cancelled) => return Err(RegistrationError::Cancelled),
                Err(Stop::TimeLimit) => {
                    run.stop_reason = StopReason::TimeLimit;
                    (objective.best.0, 0, false)
                }
            };
            // The best point evaluated (Powell's result, or the best before a time limit hit).
            let q = if objective.best.1.is_finite() {
                objective.best.0
            } else {
                q
            };
            fixed_to_moving = objective.transform(&q);
            let metric = objective.metric(&q);
            run.iterations += iterations;
            run.evaluations += objective.evaluations;
            if !converged {
                run.converged = false;
                if run.stop_reason != StopReason::TimeLimit {
                    run.stop_reason = StopReason::MaxIterations;
                }
            }
            run.levels.push(LevelResult {
                spacing_mm: level.spacing,
                iterations,
                metric_evaluations: objective.evaluations,
                metric_value: metric.value,
            });
            run.metric = metric;
            run.center = level.center;
            run.fixed_to_moving = fixed_to_moving;
            if run.stop_reason == StopReason::TimeLimit {
                break;
            }
        }
        Ok(run)
    }

    /// The masked metric and quality of a fixed -> moving transform on the finest level built.
    /// `check_optimum`: also look 2 working voxels away along each axis; a result that is not a
    /// local maximum there (the time limit stopped it, or it is on a slope) has its quality
    /// scaled down by how much better a neighbour is.
    fn assess(
        &mut self,
        kind: MetricKind,
        fixed_to_moving: &Mat4,
        check_optimum: bool,
    ) -> Result<(f64, f64, f64), RegistrationError> {
        let finest = self
            .levels
            .iter()
            .rposition(Option::is_some)
            .unwrap_or(self.spacings.len() - 1);
        let bins = self.options.histogram_bins;
        let pet = self.fixed_class == ModalityClass::Pt || self.moving_class == ModalityClass::Pt;
        let level = self.level(finest)?;
        let world_to_index = level.moving.image.world_to_index();
        let at = |m: &Mat4| {
            masked_metric(
                kind,
                bins,
                &level.samples,
                &level.moving,
                &mat_mul(&world_to_index, m),
            )
        };
        let (masked, share) = at(fixed_to_moving);
        let mut quality = quality_score(kind, pet, masked, share);
        if check_optimum {
            let step = 2.0 * level.spacing;
            let mut gain = 0.0f64;
            for axis in 0..3 {
                for sign in [-1.0, 1.0] {
                    let mut d = [0.0; 3];
                    d[axis] = sign * step;
                    let (neighbour, _) = at(&mat_mul(fixed_to_moving, &translation_matrix(d)));
                    gain = gain.max(neighbour - masked);
                }
            }
            quality *= (1.0 - gain / 0.03).clamp(0.0, 1.0);
        }
        Ok((masked, share, quality))
    }

    /// The fixed body's central box (8 corners of the middle 60 % of its bounding box, in fixed
    /// patient mm), for comparing transforms.
    fn central_box(&mut self) -> Result<Vec<Vec3>, RegistrationError> {
        let level = self.level(0)?;
        let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
        for p in &level.samples.points {
            for a in 0..3 {
                lo[a] = lo[a].min(f64::from(p[a]));
                hi[a] = hi[a].max(f64::from(p[a]));
            }
        }
        let mut corners = Vec::new();
        for a in [0.2, 0.8] {
            for b in [0.2, 0.8] {
                for c in [0.2, 0.8] {
                    corners.push([
                        lo[0] + a * (hi[0] - lo[0]),
                        lo[1] + b * (hi[1] - lo[1]),
                        lo[2] + c * (hi[2] - lo[2]),
                    ]);
                }
            }
        }
        Ok(corners)
    }

    /// Initial transforms: (name, moving -> fixed). One for a single-start run.
    fn starts(&mut self, multi: bool) -> Result<Vec<(String, Mat4)>, RegistrationError> {
        let options = self.options;
        let mut starts: Vec<(String, Mat4)> = Vec::new();
        if let Some(m) = options.initial_matrix {
            starts.push(("matrix".into(), m));
            if !multi {
                return Ok(starts);
            }
        }
        for (index, m) in options.starts.iter().enumerate() {
            starts.push((format!("start-{}", index + 1), *m));
        }
        let estimate = if multi
            || matches!(
                options.initialization,
                Initialization::Translation | Initialization::Multi
            ) {
            estimate_translation(
                self.fixed,
                self.moving,
                &TranslationOptions {
                    fixed_modality: options.fixed_modality.clone(),
                    moving_modality: options.moving_modality.clone(),
                    ..Default::default()
                },
            )
            .ok()
        } else {
            None
        };
        let centroid = |context: &mut Self| -> Option<Mat4> {
            let spacing = context.spacings.last().copied().unwrap_or(4.0).max(4.0);
            let f = prepare(
                context.fixed,
                spacing,
                context.fixed_class,
                options.mask,
                options.roi,
            );
            let m = prepare(
                context.moving,
                spacing,
                context.moving_class,
                options.mask,
                None,
            );
            match (mask_centroid(&f), mask_centroid(&m)) {
                (Some(cf), Some(cm)) => Some(translation_matrix(sub(cf, cm))),
                _ => None,
            }
        };
        if multi {
            if let Some(e) = &estimate {
                starts.push(("translation".into(), e.matrix));
            }
            if let Some(c) = centroid(self) {
                starts.push(("centroid".into(), c));
            }
            starts.push(("none".into(), IDENTITY));
            return Ok(starts);
        }
        match options.initialization {
            Initialization::None => starts.push(("none".into(), IDENTITY)),
            Initialization::Translation | Initialization::Centroid | Initialization::Multi => {
                match estimate {
                    Some(e) if e.confidence >= 0.3 => starts.push(("translation".into(), e.matrix)),
                    _ => match centroid(self) {
                        Some(c) => starts.push(("centroid".into(), c)),
                        None => starts.push(("none".into(), IDENTITY)),
                    },
                }
            }
        }
        Ok(starts)
    }

    fn run(&mut self, kind: MetricKind) -> Result<RegistrationResult, RegistrationError> {
        let options = self.options;
        validate_rotation(options)?;
        let multi = options.initialization == Initialization::Multi || !options.starts.is_empty();
        let max_rotation = options.max_rotation_deg.unwrap_or(f64::INFINITY);
        let starts = self.starts(multi)?;
        // A cancel before the first level is an error; a time limit just leaves the start.
        if let Err(Stop::Cancelled) = self.budget.check() {
            return Err(RegistrationError::Cancelled);
        }
        let last = self.spacings.len() - 1;
        let finest = last;

        if !multi || starts.len() == 1 {
            let (name, start) = starts.into_iter().next().expect("one start");
            let run = self.optimize(kind, &start, 0..last + 1)?;
            return self.finish(kind, run, name_to_initialization(&name), None, Vec::new());
        }

        // Multi-start: every distinct start through the coarse levels.
        let coarse_end = if last == 0 { 1 } else { last };
        let box_points = self.central_box()?;
        let distance = |a: &Mat4, b: &Mat4| -> f64 {
            box_points
                .iter()
                .map(|p| {
                    let (x, y) = (transform_point(a, *p), transform_point(b, *p));
                    ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt()
                })
                .sum::<f64>()
                / box_points.len() as f64
        };
        let mut runs: Vec<(String, LevelRun)> = Vec::new();
        let mut tried: Vec<Mat4> = Vec::new();
        let mut last_error = None;
        for (name, start) in starts {
            let Some(start_inverse) = mat_inverse(&start) else {
                continue;
            };
            if tried
                .iter()
                .any(|t| distance(t, &start_inverse) < DUPLICATE_START_MM)
            {
                continue;
            }
            tried.push(start_inverse);
            if !runs.is_empty() && self.budget.check().is_err() {
                break;
            }
            match self.optimize(kind, &start, 0..coarse_end) {
                Ok(run) => {
                    let time_limited = run.stop_reason == StopReason::TimeLimit;
                    runs.push((name, run));
                    if time_limited {
                        break;
                    }
                }
                Err(RegistrationError::Cancelled) => return Err(RegistrationError::Cancelled),
                Err(error) => last_error = Some(error),
            }
        }
        if runs.is_empty() {
            return Err(last_error.unwrap_or(RegistrationError::NoOverlap));
        }
        let rotation_of = |run: &LevelRun| rotation_angle_deg(&run.fixed_to_moving);
        // The best: plausible first, then the metric (normalized, the same level for all).
        let best = (0..runs.len())
            .max_by(|&a, &b| {
                let (ra, rb) = (&runs[a].1, &runs[b].1);
                (rotation_of(ra) <= max_rotation)
                    .cmp(&(rotation_of(rb) <= max_rotation))
                    .then(ra.metric.normalized.total_cmp(&rb.metric.normalized))
                    .then(b.cmp(&a))
            })
            .expect("non-empty");
        let best_coarse = runs[best].1.fixed_to_moving;
        // Coarse results scatter by up to about their level's spacing.
        let agree_mm = AGREE_MM.max(self.spacings[coarse_end - 1]);
        let agrees: Vec<bool> = runs
            .iter()
            .map(|(_, r)| distance(&r.fixed_to_moving, &best_coarse) <= agree_mm)
            .collect();
        let consistent = agrees.iter().filter(|&&a| a).count() > 1;
        // Refine the best at the finest level (from where its coarse run ended).
        let (best_name, best_run) = runs.swap_remove(best);
        let mut agrees = agrees;
        agrees.swap_remove(best);
        let final_run = if coarse_end <= finest && best_run.stop_reason != StopReason::TimeLimit {
            let start = mat_inverse(&best_run.fixed_to_moving).expect("rigid");
            let mut refined = self.optimize(kind, &start, coarse_end..finest + 1)?;
            let mut levels = best_run.levels.clone();
            levels.append(&mut refined.levels);
            refined.levels = levels;
            refined.iterations += best_run.iterations;
            refined.evaluations += best_run.evaluations;
            refined.converged &= best_run.converged;
            refined
        } else {
            best_run
        };
        // No other start agreed at the coarse levels: refine the best of those that came close
        // (within three times the tolerance), in case they meet at the finest level.
        let mut consistent = consistent;
        let mut refined_other = None;
        if !consistent && coarse_end <= finest && final_run.stop_reason != StopReason::TimeLimit {
            let candidate = (0..runs.len())
                .filter(|&i| distance(&runs[i].1.fixed_to_moving, &best_coarse) <= 3.0 * agree_mm)
                .max_by(|&a, &b| {
                    runs[a]
                        .1
                        .metric
                        .normalized
                        .total_cmp(&runs[b].1.metric.normalized)
                        .then(b.cmp(&a))
                });
            if let Some(i) = candidate {
                let start = mat_inverse(&runs[i].1.fixed_to_moving).expect("rigid");
                if let Ok(run) = self.optimize(kind, &start, coarse_end..finest + 1) {
                    if distance(&run.fixed_to_moving, &final_run.fixed_to_moving) <= AGREE_MM {
                        consistent = true;
                        agrees[i] = true;
                    }
                    refined_other = Some((i, run));
                }
            }
        }
        let refined_index = refined_other.as_ref().map(|(i, _)| *i);
        if let Some((i, run)) = refined_other {
            runs[i].1.fixed_to_moving = run.fixed_to_moving;
            runs[i].1.metric = run.metric;
        }
        let mut start_results = Vec::new();
        for (index, ((name, run), agree)) in runs.iter().zip(&agrees).enumerate() {
            let matrix = mat_inverse(&run.fixed_to_moving).expect("rigid");
            let (_, _, quality) = self.assess(kind, &run.fixed_to_moving, false)?;
            let rotation = rotation_angle_deg(&matrix);
            start_results.push(StartResult {
                start: name.clone(),
                matrix,
                normalized_metric: run.metric.normalized,
                quality,
                quality_label: if rotation <= max_rotation {
                    quality_label(quality)
                } else {
                    "poor"
                },
                rotation_angle_deg: rotation,
                plausible: rotation <= max_rotation,
                agrees: *agree,
                refined: refined_index == Some(index),
            });
        }
        self.finish(
            kind,
            final_run,
            name_to_initialization(&best_name),
            Some((best_name, consistent)),
            start_results,
        )
    }

    fn finish(
        &mut self,
        kind: MetricKind,
        run: LevelRun,
        initialization: &'static str,
        multi: Option<(String, bool)>,
        mut start_results: Vec<StartResult>,
    ) -> Result<RegistrationResult, RegistrationError> {
        let max_rotation = self.options.max_rotation_deg.unwrap_or(f64::INFINITY);
        let matrix = mat_inverse(&run.fixed_to_moving).expect("a rigid transform is invertible");
        let (masked, share, mut quality) = self.assess(kind, &run.fixed_to_moving, true)?;
        let rotation = rotation_angle_deg(&matrix);
        let plausible = rotation <= max_rotation;
        let mut label = quality_label(quality);
        if !plausible {
            label = "poor";
            quality = quality.min(0.39);
        }
        let consistent = multi.as_ref().map(|(_, c)| *c);
        if let Some((name, _)) = &multi {
            start_results.insert(
                0,
                StartResult {
                    start: name.clone(),
                    matrix,
                    normalized_metric: run.metric.normalized,
                    quality,
                    quality_label: label,
                    rotation_angle_deg: rotation,
                    plausible,
                    agrees: true,
                    refined: true,
                },
            );
        }
        Ok(RegistrationResult {
            matrix,
            rotation_deg: euler_deg(&matrix),
            translation_mm: [matrix[3], matrix[7], matrix[11]],
            center_mm: run.center,
            metric: kind,
            metric_value: run.metric.value,
            initial_metric_value: run.initial_metric,
            normalized_metric: run.metric.normalized,
            overlap_fraction: run.metric.overlap,
            quality,
            quality_label: label,
            iterations: run.iterations,
            metric_evaluations: run.evaluations,
            levels: run.levels,
            initialization,
            converged: run.converged && run.stop_reason == StopReason::Converged,
            stop_reason: run.stop_reason,
            timing_ms: self.started.elapsed().as_secs_f64() * 1000.0,
            working_bytes: self.working_bytes(),
            body_metric: masked,
            body_overlap_fraction: share,
            rotation_angle_deg: rotation,
            plausible,
            consistent,
            starts: start_results,
        })
    }
}

fn name_to_initialization(name: &str) -> &'static str {
    match name {
        "matrix" => "matrix",
        "translation" => "translation",
        "centroid" => "centroid",
        "none" => "none",
        _ => "start",
    }
}

// ---------------------------------------------------------------------------------------------
// Translation estimate
// ---------------------------------------------------------------------------------------------
//
// z (S-I) comes from cross-correlating per-height profiles, x/y from a masked 2-D correlation of
// axial projections. Both only compare what BOTH scans could see:
//
// - Each scan's in-plane "support" is where its body mask ever is, over all its slices (dilated
//   by a cell). A scan truncated by a smaller reconstruction field of view never shows anatomy
//   beyond its FOV disc, so its support stops there. Profiles and projections are built from the
//   voxels inside the intersection of the two supports (after the current x/y estimate), so the
//   arms and flanks one scan cut off do not change the other's body area.
// - A z shift is scored by the profiles' correlation over the overlap times an overlap factor
//   (`overlap_factor`): a short end overlap needs a much better fit to beat a long one, which is
//   what stops a 100 mm end overlap with a chance correlation from winning.
// - x/y: the per-cell projections (body, lung and bone counts for CT-CT, mean normalized
//   intensity for the same modality, the body otherwise) over the matched z overlap, correlated
//   over a grid of shifts within the intersection of the supports. Unlike the body-mask centroids
//   this is not pulled by truncation: a cut-off flank is simply outside the comparison.
// - Confidence combines the peak (after the overlap factor), its margin over the best other
//   peak, and whether each profile channel on its own agrees with the combined answer; it was
//   calibrated on public CT, PT and MR pairs with cropped, shifted, rotated and FOV-truncated
//   versions (benchmarks/registration): no wrong answer scores 0.3 or more.

/// Working-grid spacing of the translation estimate, mm.
const PROFILE_GRID_MM: f64 = 3.0;
/// Profile bin width along patient S-I, mm.
const PROFILE_BIN_MM: f64 = 1.0;
/// In-plane cell of the supports and projections (patient x/y), mm.
const XY_CELL_MM: f64 = 3.0;
/// Largest in-plane offset the projection correlation searches, mm.
const XY_SEARCH_MM: f64 = 150.0;
/// The refining x/y searches look this far around the previous answer, mm.
const XY_LOCAL_MM: f64 = 30.0;

/// One body voxel of an estimate volume.
#[derive(Clone, Copy)]
struct EstimateVoxel {
    /// Absolute patient-xy cell (`floor(x / XY_CELL_MM)`, `floor(y / XY_CELL_MM)`).
    cell: [i32; 2],
    z: f32,
    /// bit 0 lung, bit 1 bone (CT).
    flags: u8,
    /// Normalized intensity (0..1), when the intensity channel is on.
    intensity: f32,
}

/// A 2-D boolean map over absolute patient-xy cells.
struct CellMap {
    x0: i32,
    y0: i32,
    nx: usize,
    ny: usize,
    on: Vec<bool>,
}

impl CellMap {
    fn get(&self, x: i32, y: i32) -> bool {
        let (i, j) = (x - self.x0, y - self.y0);
        i >= 0
            && j >= 0
            && (i as usize) < self.nx
            && (j as usize) < self.ny
            && self.on[j as usize * self.nx + i as usize]
    }
}

struct EstimateVolume {
    /// S-I (patient z) of bin 0, mm.
    z0: f64,
    bins: usize,
    /// Whether the scan fully covers each bin's height.
    valid: Vec<bool>,
    /// Patient-z footprint of one working voxel, mm.
    extent: f64,
    voxel_volume: f64,
    voxels: Vec<EstimateVoxel>,
    support: CellMap,
}

fn cell_of(v: f64) -> i32 {
    (v / XY_CELL_MM).floor() as i32
}

/// The working image, body mask and support of one volume for the estimate.
fn estimate_volume(
    volume: &Volume,
    class: ModalityClass,
    intensity_channel: bool,
    ct_channels: bool,
) -> EstimateVolume {
    let image = build_work_image(volume, PROFILE_GRID_MM);
    let body = body_mask(&image, class);
    let normalized = if intensity_channel {
        Some(normalize(&image, &body))
    } else {
        None
    };
    let [nx, ny, nz] = image.dims;
    let extent: f64 = (0..3)
        .map(|a| image.axes[a][2].abs() * image.spacing[a])
        .sum::<f64>()
        .max(PROFILE_BIN_MM);
    let voxel_volume = image.spacing[0] * image.spacing[1] * image.spacing[2];
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for &k in &[0, nz - 1] {
        for &j in &[0, ny - 1] {
            for &i in &[0, nx - 1] {
                let p = image.world(i, j, k);
                for a in 0..3 {
                    lo[a] = lo[a].min(p[a]);
                    hi[a] = hi[a].max(p[a]);
                }
            }
        }
    }
    let z0 = (lo[2] - extent).floor();
    let bins = ((hi[2] + extent - z0) / PROFILE_BIN_MM).ceil() as usize + 1;
    // Coverage of each bin by the grid, for `valid`.
    let mut coverage = vec![0.0f64; bins];
    let mut voxels = Vec::new();
    let (cx0, cy0) = (cell_of(lo[0]) - 1, cell_of(lo[1]) - 1);
    let (cnx, cny) = (
        (cell_of(hi[0]) - cx0 + 2) as usize,
        (cell_of(hi[1]) - cy0 + 2) as usize,
    );
    let mut union = vec![false; cnx * cny];
    for k in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                let index = k * nx * ny + j * nx + i;
                let p = image.world(i, j, k);
                spread(p[2], extent, z0, bins, |bin, share| coverage[bin] += share);
                if body[index] == 0 {
                    continue;
                }
                let v = image.data[index];
                let mut flags = 0u8;
                if ct_channels {
                    if v > -950.0 && v < -500.0 {
                        flags |= 1;
                    }
                    if v > 200.0 {
                        flags |= 2;
                    }
                }
                let cell = [cell_of(p[0]), cell_of(p[1])];
                union[(cell[1] - cy0) as usize * cnx + (cell[0] - cx0) as usize] = true;
                voxels.push(EstimateVoxel {
                    cell,
                    z: p[2] as f32,
                    flags,
                    intensity: normalized.as_ref().map_or(0.0, |n| n[index]),
                });
            }
        }
    }
    let full = coverage.iter().cloned().fold(0.0, f64::max);
    let valid = coverage
        .iter()
        .map(|&c| c >= 0.9 * full && full > 0.0)
        .collect();
    // The support: the union of the body over all slices, dilated by one cell.
    let mut on = union.clone();
    for y in 0..cny {
        for x in 0..cnx {
            if union[y * cnx + x] {
                continue;
            }
            let near = (y > 0 && union[(y - 1) * cnx + x])
                || (y + 1 < cny && union[(y + 1) * cnx + x])
                || (x > 0 && union[y * cnx + x - 1])
                || (x + 1 < cnx && union[y * cnx + x + 1]);
            on[y * cnx + x] = near;
        }
    }
    EstimateVolume {
        z0,
        bins,
        valid,
        extent,
        voxel_volume,
        voxels,
        support: CellMap {
            x0: cx0,
            y0: cy0,
            nx: cnx,
            ny: cny,
            on,
        },
    }
}

/// Spreads a voxel centred at `z` with footprint `extent` over the 1 mm bins: `f(bin, share)`,
/// the shares summing to 1.
#[inline]
fn spread(z: f64, extent: f64, z0: f64, bins: usize, mut f: impl FnMut(usize, f64)) {
    let (lo, hi) = (z - extent / 2.0, z + extent / 2.0);
    let first = (((lo - z0) / PROFILE_BIN_MM).floor().max(0.0)) as usize;
    let last = ((((hi - z0) / PROFILE_BIN_MM).floor()) as usize).min(bins - 1);
    for bin in first..=last {
        let b_lo = z0 + bin as f64 * PROFILE_BIN_MM;
        let overlap = (hi.min(b_lo + PROFILE_BIN_MM) - lo.max(b_lo)).max(0.0) / extent;
        if overlap > 0.0 {
            f(bin, overlap);
        }
    }
}

/// Channel layout of the estimate for a modality pair.
#[derive(Clone, Copy)]
struct Channels {
    ct: bool,
    intensity: bool,
}

impl Channels {
    fn names(self) -> Vec<&'static str> {
        let mut names = vec!["body"];
        if self.ct {
            names.extend(["lung", "bone"]);
        }
        if self.intensity {
            names.push("intensity");
        }
        names
    }

    /// The voxel's weight in each channel.
    fn weights(self, v: &EstimateVoxel) -> [f64; 4] {
        let mut w = [1.0, 0.0, 0.0, 0.0];
        let mut n = 1;
        if self.ct {
            w[1] = f64::from(v.flags & 1);
            w[2] = f64::from((v.flags >> 1) & 1);
            n = 3;
        }
        if self.intensity {
            w[n] = f64::from(v.intensity);
        }
        w
    }
}

/// Per-bin profiles (one vector per channel; cross-sectional area in mm^2 or integrated
/// intensity) of the voxels whose cell is in `own` support and, shifted by `shift` cells, in
/// the `other` support.
fn channel_profiles(
    volume: &EstimateVolume,
    channels: Channels,
    other: &CellMap,
    shift: [i32; 2],
) -> Vec<Vec<f64>> {
    let count = channels.names().len();
    let mut out = vec![vec![0.0; volume.bins]; count];
    let scale = volume.voxel_volume / PROFILE_BIN_MM;
    for v in &volume.voxels {
        if !other.get(v.cell[0] + shift[0], v.cell[1] + shift[1]) {
            continue;
        }
        let w = channels.weights(v);
        spread(
            f64::from(v.z),
            volume.extent,
            volume.z0,
            volume.bins,
            |bin, share| {
                for c in 0..count {
                    out[c][bin] += w[c] * share * scale;
                }
            },
        );
    }
    out
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    if n < 3.0 {
        return 0.0;
    }
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        cov += (x - ma) * (y - mb);
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
    }
    // A channel flat over the overlap (std under 2 % of its mean) says nothing.
    let flat = |var: f64, mean: f64| var <= (0.02 * mean.abs()).powi(2) * n || var <= 1e-12;
    if flat(va, ma) || flat(vb, mb) {
        0.0
    } else {
        cov / (va * vb).sqrt()
    }
}

/// Down-weights short overlaps: 1 when the overlap covers the shorter scan, 0.83 at half of it,
/// 0.63 at a quarter (`n / (n + L / 4) * 1.25`).
fn overlap_factor(overlap_mm: f64, shorter_mm: f64) -> f64 {
    let quarter = 0.25 * shorter_mm.max(1.0);
    (1.25 * overlap_mm / (overlap_mm + quarter)).min(1.0)
}

/// One z shift's score.
#[derive(Clone)]
struct ZScore {
    /// Fixed bin i <-> moving bin i + offset.
    offset: i64,
    /// Mean channel correlation times the overlap factor.
    score: f64,
    /// Mean channel correlation, without the overlap factor (for locating the peak precisely:
    /// the factor's slope would pull it towards more overlap).
    raw: f64,
    /// Per-channel correlation times the overlap factor.
    per_channel: Vec<f64>,
    overlap_bins: usize,
}

#[allow(clippy::too_many_arguments)]
fn search_z(
    f: &EstimateVolume,
    m: &EstimateVolume,
    fp: &[Vec<f64>],
    mp: &[Vec<f64>],
    weights: &[f64],
    min_overlap_bins: usize,
    max_shift_mm: Option<f64>,
    shorter_mm: f64,
) -> Vec<ZScore> {
    let (f_bins, m_bins) = (f.bins as i64, m.bins as i64);
    let base = (f.z0 - m.z0) / PROFILE_BIN_MM;
    let weight_sum: f64 = weights.iter().sum();
    let offsets: Vec<i64> = (-f_bins..=m_bins).collect();
    offsets
        .par_iter()
        .filter_map(|&offset| {
            let dz = (base - offset as f64) * PROFILE_BIN_MM;
            if max_shift_mm.is_some_and(|max| dz.abs() > max) {
                return None;
            }
            let pairs: Vec<(usize, usize)> = (0..f_bins)
                .filter_map(|i| {
                    let j = i + offset;
                    (j >= 0 && j < m_bins && f.valid[i as usize] && m.valid[j as usize])
                        .then_some((i as usize, j as usize))
                })
                .collect();
            if pairs.len() < min_overlap_bins.max(3) {
                return None;
            }
            let factor = overlap_factor(pairs.len() as f64 * PROFILE_BIN_MM, shorter_mm);
            let per_channel: Vec<f64> = fp
                .iter()
                .zip(mp)
                .map(|(fc, mc)| {
                    let a: Vec<f64> = pairs.iter().map(|&(i, _)| fc[i]).collect();
                    let b: Vec<f64> = pairs.iter().map(|&(_, j)| mc[j]).collect();
                    pearson(&a, &b) * factor
                })
                .collect();
            let score = per_channel
                .iter()
                .zip(weights)
                .map(|(r, w)| r * w)
                .sum::<f64>()
                / weight_sum;
            Some(ZScore {
                offset,
                score,
                raw: score / factor.max(1e-9),
                per_channel,
                overlap_bins: pairs.len(),
            })
        })
        .collect()
}

/// Index of the best score (ties: the smaller offset, i.e. deterministic).
fn best_index(scores: &[ZScore], key: impl Fn(&ZScore) -> f64) -> Option<usize> {
    (0..scores.len()).max_by(|&a, &b| {
        key(&scores[a])
            .total_cmp(&key(&scores[b]))
            .then(scores[b].offset.cmp(&scores[a].offset))
    })
}

/// The lobe of `scores[best]`: downhill both ways until the score rises again (contiguous offsets).
fn lobe(scores: &[ZScore], best: usize, key: impl Fn(&ZScore) -> f64) -> (usize, usize) {
    let (mut left, mut right) = (best, best);
    while left > 0
        && scores[left - 1].offset == scores[left].offset - 1
        && key(&scores[left - 1]) <= key(&scores[left])
    {
        left -= 1;
    }
    while right + 1 < scores.len()
        && scores[right + 1].offset == scores[right].offset + 1
        && key(&scores[right + 1]) <= key(&scores[right])
    {
        right += 1;
    }
    (left, right)
}

/// Per-cell projections (one map per channel, over the cell grid of `support`) of the voxels
/// with `z` in `[z_lo, z_hi]`.
fn projections(volume: &EstimateVolume, channels: Channels, z_lo: f64, z_hi: f64) -> Vec<Vec<f64>> {
    let count = channels.names().len();
    let s = &volume.support;
    let mut maps = vec![vec![0.0; s.nx * s.ny]; count];
    for v in &volume.voxels {
        let z = f64::from(v.z);
        if z < z_lo || z > z_hi {
            continue;
        }
        let index = (v.cell[1] - s.y0) as usize * s.nx + (v.cell[0] - s.x0) as usize;
        let w = channels.weights(v);
        for c in 0..count {
            maps[c][index] += w[c];
        }
    }
    // The intensity channel as a mean over the body (so it is not just the body count again).
    if channels.intensity {
        let body = maps[0].clone();
        for (value, &n) in maps[count - 1].iter_mut().zip(&body) {
            *value = if n > 0.0 { *value / n } else { 0.0 };
        }
    }
    maps
}

/// Masked correlation of the projections for the in-plane shift `s` (cells, moving -> fixed),
/// over the fixed support cells `f_cells` (absolute x, y, map index) inside the moving support
/// too. Returns (score, cells compared).
fn xy_score(
    m: &EstimateVolume,
    f_cells: &[(i32, i32, usize)],
    fmaps: &[Vec<f64>],
    mmaps: &[Vec<f64>],
    weights: &[f64],
    s: [i32; 2],
) -> (f64, usize) {
    let ms = &m.support;
    let count = fmaps.len();
    // Sums per channel: a, b, aa, bb, ab.
    let mut acc = [[0.0f64; 5]; 4];
    let mut n = 0usize;
    for &(x, y, fi) in f_cells {
        let (mx, my) = (x - s[0], y - s[1]);
        if !ms.get(mx, my) {
            continue;
        }
        let mi = (my - ms.y0) as usize * ms.nx + (mx - ms.x0) as usize;
        n += 1;
        for c in 0..count {
            let (a, b) = (fmaps[c][fi], mmaps[c][mi]);
            let t = &mut acc[c];
            t[0] += a;
            t[1] += b;
            t[2] += a * a;
            t[3] += b * b;
            t[4] += a * b;
        }
    }
    if n < 16 {
        return (f64::NEG_INFINITY, n);
    }
    let k = n as f64;
    let mut score = 0.0;
    for (t, w) in acc.iter().take(count).zip(weights) {
        let cov = t[4] - t[0] * t[1] / k;
        let va = t[2] - t[0] * t[0] / k;
        let vb = t[3] - t[1] * t[1] / k;
        if va > 1e-9 && vb > 1e-9 {
            score += w * cov / (va * vb).sqrt();
        }
    }
    (score / weights.iter().sum::<f64>(), n)
}

/// The in-plane shift (mm, moving -> fixed) that best aligns the projections over the given z
/// ranges, and its score. `around`: search only within `XY_LOCAL_MM` of this shift (mm) instead
/// of the whole `XY_SEARCH_MM` range.
#[allow(clippy::too_many_arguments)]
fn search_xy(
    f: &EstimateVolume,
    m: &EstimateVolume,
    channels: Channels,
    weights: &[f64],
    f_range: (f64, f64),
    m_range: (f64, f64),
    around: Option<[f64; 2]>,
) -> ([f64; 2], f64) {
    let fmaps = projections(f, channels, f_range.0, f_range.1);
    let mmaps = projections(m, channels, m_range.0, m_range.1);
    let fs = &f.support;
    let f_cells: Vec<(i32, i32, usize)> = (0..fs.nx * fs.ny)
        .filter(|&i| fs.on[i])
        .map(|i| (fs.x0 + (i % fs.nx) as i32, fs.y0 + (i / fs.nx) as i32, i))
        .collect();
    let m_cells = m.support.on.iter().filter(|&&o| o).count();
    let smaller = f_cells.len().min(m_cells).max(1) as f64;
    // A shift that leaves little of either support in the comparison is not a match.
    let scored = |s: [i32; 2]| -> f64 {
        let (score, n) = xy_score(m, &f_cells, &fmaps, &mmaps, weights, s);
        let cover = n as f64 / smaller;
        if cover < 0.3 {
            f64::NEG_INFINITY
        } else {
            score * overlap_factor(cover, 1.0)
        }
    };
    // Coarse: a 3-cell (9 mm) lattice over the range, in parallel.
    let (center, reach) = match around {
        Some(c) => (
            [
                (c[0] / XY_CELL_MM).round() as i32,
                (c[1] / XY_CELL_MM).round() as i32,
            ],
            (XY_LOCAL_MM / XY_CELL_MM).ceil() as i32,
        ),
        None => ([0, 0], (XY_SEARCH_MM / XY_CELL_MM).ceil() as i32),
    };
    let step = if around.is_some() { 2 } else { 3 };
    let coarse: Vec<[i32; 2]> = (-reach..=reach)
        .step_by(step)
        .flat_map(|y| {
            (-reach..=reach)
                .step_by(step)
                .map(move |x| [center[0] + x, center[1] + y])
        })
        .collect();
    let coarse_scores: Vec<f64> = coarse.par_iter().map(|&s| scored(s)).collect();
    let mut best = (0..coarse.len())
        .max_by(|&a, &b| {
            coarse_scores[a]
                .total_cmp(&coarse_scores[b])
                .then(b.cmp(&a))
        })
        .map(|i| coarse[i])
        .unwrap_or(center);
    let mut best_score = scored(best);
    // Fine: hill-climb on single cells.
    for _ in 0..32 {
        let neighbours: Vec<[i32; 2]> = [
            [-1, 0],
            [1, 0],
            [0, -1],
            [0, 1],
            [-1, -1],
            [1, 1],
            [-1, 1],
            [1, -1],
        ]
        .iter()
        .map(|d| [best[0] + d[0], best[1] + d[1]])
        .collect();
        let values: Vec<f64> = neighbours.par_iter().map(|&s| scored(s)).collect();
        let Some(i) = (0..8).max_by(|&a, &b| values[a].total_cmp(&values[b]).then(b.cmp(&a)))
        else {
            break;
        };
        if values[i] <= best_score {
            break;
        }
        best = neighbours[i];
        best_score = values[i];
    }
    // Sub-cell refinement per axis by a parabola.
    let mut refined = [best[0] as f64, best[1] as f64];
    for axis in 0..2 {
        let mut lo = best;
        let mut hi = best;
        lo[axis] -= 1;
        hi[axis] += 1;
        let (l, r) = (scored(lo), scored(hi));
        let denominator = l - 2.0 * best_score + r;
        if l.is_finite() && r.is_finite() && denominator < -1e-12 {
            refined[axis] += (0.5 * (l - r) / denominator).clamp(-0.5, 0.5);
        }
    }
    (
        [refined[0] * XY_CELL_MM, refined[1] * XY_CELL_MM],
        best_score,
    )
}

/// Valid z extent of an estimate volume, mm.
fn valid_extent_mm(volume: &EstimateVolume) -> f64 {
    volume.valid.iter().filter(|&&v| v).count() as f64 * PROFILE_BIN_MM
}

/// A fast moving -> fixed translation (module section "Translation estimate"): z from per-height
/// profiles over the two scans' common in-plane support, x/y from a masked correlation of their
/// projections over the matched overlap.
pub fn estimate_translation(
    fixed: &Volume,
    moving: &Volume,
    options: &TranslationOptions,
) -> Result<TranslationEstimate, RegistrationError> {
    if !(options.min_overlap_mm.is_finite() && options.min_overlap_mm > 0.0) {
        return Err(RegistrationError::InvalidOption(
            "minOverlapMm must be a positive number".into(),
        ));
    }
    if options
        .min_overlap_fraction
        .is_some_and(|f| !(f.is_finite() && f > 0.0 && f <= 1.0))
    {
        return Err(RegistrationError::InvalidOption(
            "minOverlapFraction must be in (0, 1]".into(),
        ));
    }
    let started = Instant::now();
    let fixed_class = parse_modality(&options.fixed_modality, fixed);
    let moving_class = parse_modality(&options.moving_modality, moving);
    let same = fixed_class == moving_class;
    let channels = Channels {
        ct: same && fixed_class == ModalityClass::Ct,
        intensity: same && fixed_class != ModalityClass::Ct,
    };
    let (f, m) = rayon::join(
        || estimate_volume(fixed, fixed_class, channels.intensity, channels.ct),
        || estimate_volume(moving, moving_class, channels.intensity, channels.ct),
    );
    if f.voxels.is_empty() || m.voxels.is_empty() {
        return Err(RegistrationError::EmptyMask(if f.voxels.is_empty() {
            "fixed"
        } else {
            "moving"
        }));
    }
    let names = channels.names();
    // Profile channel weights. CT: the body area is the one most changed by truncation, the lung
    // and bone areas the most specific. MR: the body is mostly the (small) field of view, the
    // intensity carries the anatomy. PT: the body outline is stable between scans months apart,
    // the uptake is not (disease, bladder, brown fat).
    let z_weights: Vec<f64> = names
        .iter()
        .map(|n| match (*n, fixed_class) {
            ("body", ModalityClass::Pt) => 1.0,
            ("intensity", ModalityClass::Pt) => 0.3,
            ("body", _) => 0.5,
            _ => 1.0,
        })
        .collect();
    let xy_weights: Vec<f64> = names.iter().map(|_| 1.0).collect();

    let shorter = valid_extent_mm(&f).min(valid_extent_mm(&m));
    let min_overlap_mm = match options.min_overlap_fraction {
        Some(fraction) => options
            .min_overlap_mm
            .min((fraction * shorter).max(20.0_f64.min(0.75 * shorter))),
        None => options.min_overlap_mm,
    };
    let min_overlap_bins = (min_overlap_mm / PROFILE_BIN_MM).ceil() as usize;

    // 1. A first in-plane offset over the whole scans, for the support intersection.
    let all = (f64::NEG_INFINITY, f64::INFINITY);
    let (xy0, _) = search_xy(&f, &m, channels, &xy_weights, all, all, None);
    // 2. z over the common support; 3. x/y over the matched overlap; 4. z again.
    let mut xy = xy0;
    let mut result = None;
    for _ in 0..2 {
        let shift = [
            (xy[0] / XY_CELL_MM).round() as i32,
            (xy[1] / XY_CELL_MM).round() as i32,
        ];
        // Fixed cell c sees moving cell c - shift.
        let fp = channel_profiles(&f, channels, &m.support, [-shift[0], -shift[1]]);
        let mp = channel_profiles(&m, channels, &f.support, shift);
        let mut scores = search_z(
            &f,
            &m,
            &fp,
            &mp,
            &z_weights,
            min_overlap_bins,
            options.max_shift_mm,
            shorter,
        );
        scores.sort_by_key(|s| s.offset);
        let Some(chosen) = best_index(&scores, |s| s.score) else {
            return Err(RegistrationError::NoOverlap);
        };
        // The overlap factor picks the peak; its exact position is the raw correlation's local
        // maximum near it.
        let window = (10.0 / PROFILE_BIN_MM) as i64;
        let best = (0..scores.len())
            .filter(|&i| (scores[i].offset - scores[chosen].offset).abs() <= window)
            .max_by(|&a, &b| scores[a].raw.total_cmp(&scores[b].raw).then(b.cmp(&a)))
            .unwrap_or(chosen);
        let base = (f.z0 - m.z0) / PROFILE_BIN_MM;
        let peak = scores[best].raw;
        let refine = if best > 0
            && best + 1 < scores.len()
            && scores[best - 1].offset + 1 == scores[best].offset
            && scores[best + 1].offset == scores[best].offset + 1
        {
            let (l, r) = (scores[best - 1].raw, scores[best + 1].raw);
            let denominator = l - 2.0 * peak + r;
            if denominator < -1e-12 {
                (0.5 * (l - r) / denominator).clamp(-0.5, 0.5)
            } else {
                0.0
            }
        } else {
            0.0
        };
        let dz = (base - (scores[best].offset as f64 + refine)) * PROFILE_BIN_MM;
        // The matched z range, in each scan's own coordinates.
        let pairs: Vec<usize> = (0..f.bins)
            .filter(|&i| {
                let j = i as i64 + scores[best].offset;
                j >= 0 && (j as usize) < m.bins && f.valid[i] && m.valid[j as usize]
            })
            .collect();
        let (lo, hi) = match (pairs.first(), pairs.last()) {
            (Some(&a), Some(&b)) => (
                f.z0 + a as f64 * PROFILE_BIN_MM,
                f.z0 + (b + 1) as f64 * PROFILE_BIN_MM,
            ),
            _ => return Err(RegistrationError::NoOverlap),
        };
        let (xy_new, xy_score) = search_xy(
            &f,
            &m,
            channels,
            &xy_weights,
            (lo, hi),
            (lo - dz, hi - dz),
            Some(xy),
        );
        let converged =
            (xy_new[0] - xy[0]).abs() < XY_CELL_MM && (xy_new[1] - xy[1]).abs() < XY_CELL_MM;
        xy = xy_new;
        result = Some((scores, chosen, dz, xy_score));
        if converged {
            break;
        }
    }
    let (scores, best, dz, xy_score) = result.expect("at least one pass");
    let peak = scores[best].score;
    // The runner-up: the best score outside the peak's lobe and at least 20 mm away.
    let (left, right) = lobe(&scores, best, |s| s.score);
    let second = scores
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            (*i < left || *i > right)
                && ((s.offset - scores[best].offset).abs() as f64 * PROFILE_BIN_MM) > 20.0
        })
        .map(|(_, s)| s.score)
        .fold(0.0, f64::max);
    // Each channel on its own: does its best shift agree with the combined one (within 10 mm)?
    let agreement = if names.len() > 1 {
        let mut agree = 0.0;
        for (c, w) in z_weights.iter().enumerate() {
            if let Some(own) = best_index(&scores, |s| s.per_channel[c]) {
                if (scores[own].offset - scores[best].offset).abs() as f64 * PROFILE_BIN_MM <= 10.0
                {
                    agree += w;
                }
            }
        }
        agree / z_weights.iter().sum::<f64>()
    } else {
        1.0
    };
    let confidence = calibrated_confidence(peak, second, agreement, xy_score);
    let translation = [xy[0], xy[1], dz];
    Ok(TranslationEstimate {
        matrix: translation_matrix(translation),
        translation_mm: translation,
        confidence,
        peak_correlation: peak,
        second_peak_correlation: second,
        overlap_mm: scores[best].overlap_bins as f64 * PROFILE_BIN_MM,
        profiles: names,
        timing_ms: started.elapsed().as_secs_f64() * 1000.0,
        channel_agreement: agreement,
        xy_correlation: xy_score,
        min_overlap_mm,
    })
}

/// Confidence in 0..1 from the z peak (overlap factor included), its margin over the runner-up,
/// the channels' agreement and the in-plane correlation (calibration: module section).
fn calibrated_confidence(peak: f64, second: f64, agreement: f64, xy_score: f64) -> f64 {
    let height = ((peak - 0.5) / 0.4).clamp(0.0, 1.0);
    let margin = ((peak - second.max(0.0)) / 0.15).clamp(0.0, 1.0);
    let xy = ((xy_score - 0.3) / 0.4).clamp(0.0, 1.0);
    height * margin * (0.5 + 0.5 * agreement) * (0.5 + 0.5 * xy)
}
