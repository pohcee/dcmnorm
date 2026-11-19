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
//!    intensities. PT: 5% of the 99.5th percentile, which works on raw counts, Bq/ml or SUV alike
//!    (no HU or SUV assumption). Every mask is the largest component with its holes filled
//!    (lungs, bowel gas) and a one-voxel margin for the skin edge.
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
//!    centroids.
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

const IDENTITY: Mat4 = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];

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
}

#[derive(Clone, Debug)]
pub struct TranslationOptions {
    pub fixed_modality: Option<String>,
    pub moving_modality: Option<String>,
    /// Smallest S-I overlap of the two scans a z shift may imply, mm.
    pub min_overlap_mm: f64,
    /// Largest |z shift| considered, mm (`None`: anything with enough overlap).
    pub max_shift_mm: Option<f64>,
}

impl Default for TranslationOptions {
    fn default() -> Self {
        Self { fixed_modality: None, moving_modality: None, min_overlap_mm: 60.0, max_shift_mm: None }
    }
}

#[derive(Clone, Debug)]
pub struct TranslationEstimate {
    /// Moving patient -> fixed patient (a pure translation), row-major.
    pub matrix: Mat4,
    pub translation_mm: Vec3,
    /// 0..1: the correlation peak's height and its margin over the best peak at least 20 mm away.
    pub confidence: f64,
    pub peak_correlation: f64,
    pub second_peak_correlation: f64,
    pub overlap_mm: f64,
    /// The profiles that were correlated ("body", "lung", "bone", "intensity").
    pub profiles: Vec<&'static str>,
    pub timing_ms: f64,
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
            Self::EmptyMask(which) => write!(formatter, "registration: the {which} volume has no body voxels to register"),
            Self::NoOverlap => write!(formatter, "registration: fixed and moving volumes do not overlap"),
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
        let pivot = (col..4).max_by(|&x, &y| a[x * 4 + col].abs().total_cmp(&a[y * 4 + col].abs()))?;
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
    [1.0, 0.0, 0.0, t[0], 0.0, 1.0, 0.0, t[1], 0.0, 0.0, 1.0, t[2], 0.0, 0.0, 0.0, 1.0]
}

/// `Rz(rz) * Ry(ry) * Rx(rx)`, radians, as the 3x3 part of a 4x4.
pub fn rotation_matrix(rx: f64, ry: f64, rz: f64) -> Mat4 {
    let (sx, cx) = rx.sin_cos();
    let (sy, cy) = ry.sin_cos();
    let (sz, cz) = rz.sin_cos();
    [
        cz * cy, cz * sy * sx - sz * cx, cz * sy * cx + sz * sx, 0.0,
        sz * cy, sz * sy * sx + cz * cx, sz * sy * cx - cz * sx, 0.0,
        -sy, cy * sx, cy * cx, 0.0,
        0.0, 0.0, 0.0, 1.0,
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
            add(add(self.origin, scale(self.axes[0], i as f64 * self.spacing[0])), scale(self.axes[1], j as f64 * self.spacing[1])),
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
    let nz = if volume.num_slices > 1 { (span / sz + 1e-6).floor() as usize + 1 } else { 1 };
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
    binned.par_chunks_mut(out_plane).enumerate().for_each(|(s, out)| {
        let src = &volume.samples[s * plane..(s + 1) * plane];
        for by in 0..ny {
            let r0 = by * ky;
            let r1 = (r0 + ky).min(rows);
            for bx in 0..nx {
                let c0 = bx * kx;
                let c1 = (c0 + kx).min(cols);
                let mut sum = 0f64;
                for r in r0..r1 {
                    sum += src[r * cols + c0..r * cols + c1].iter().map(|&v| f64::from(v)).sum::<f64>();
                }
                out[by * nx + bx] = (sum / ((r1 - r0) * (c1 - c0)) as f64) as f32;
            }
        }
    });

    // Along the normal: the mean of the slices within half a working step of each target
    // position, or linear interpolation between the two bracketing slices when none is.
    let zs = &volume.slice_zs;
    let mut data = vec![0f32; out_plane * nz];
    data.par_chunks_mut(out_plane).enumerate().for_each(|(k, out)| {
        let z = zs[0] + k as f64 * sz;
        let lo = zs.partition_point(|&v| v < z - sz / 2.0);
        let hi = zs.partition_point(|&v| v < z + sz / 2.0);
        if hi > lo {
            let count = (hi - lo) as f32;
            for s in lo..hi {
                for (o, &v) in out.iter_mut().zip(&binned[s * out_plane..(s + 1) * out_plane]) {
                    *o += v;
                }
            }
            out.iter_mut().for_each(|o| *o /= count);
        } else {
            let upper = zs.partition_point(|&v| v < z).clamp(1, slices - 1);
            let lower = upper - 1;
            let t = ((z - zs[lower]) / (zs[upper] - zs[lower]).max(1e-9)).clamp(0.0, 1.0) as f32;
            let a = &binned[lower * out_plane..(lower + 1) * out_plane];
            let b = &binned[upper * out_plane..(upper + 1) * out_plane];
            for ((o, &va), &vb) in out.iter_mut().zip(a).zip(b) {
                *o = va + (vb - va) * t;
            }
        }
    });

    let origin = add(
        add(volume.origin, scale(volume.row_vector, (kx - 1) as f64 / 2.0 * volume.col_spacing_mm)),
        scale(volume.col_vector, (ky - 1) as f64 / 2.0 * volume.row_spacing_mm),
    );
    WorkImage {
        data,
        dims: [nx, ny, nz],
        spacing: [kx as f64 * volume.col_spacing_mm, ky as f64 * volume.row_spacing_mm, if nz > 1 { sz } else { 1.0 }],
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
    let sum_all: f64 = histogram.iter().enumerate().map(|(i, &n)| i as f64 * n).sum();
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
    label.iter().map(|&l| u8::from(l != 0 && l == best.0)).collect()
}

/// Fills enclosed holes: everything not reachable from the grid's side faces through
/// background. The two faces of the axis closest to patient S-I are not seeds, so a lung or
/// airway cut by the top or bottom of the scan still counts as enclosed.
fn fill_holes(mask: &[u8], dims: [usize; 3], axes: &[Vec3; 3]) -> Vec<u8> {
    let si_axis = (0..3).max_by(|&a, &b| axes[a][2].abs().total_cmp(&axes[b][2].abs())).unwrap_or(2);
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
        ModalityClass::Pt => quantile(data, |_| true, 0.995).unwrap_or(0.0) * 0.05,
        ModalityClass::Mr | ModalityClass::Other => {
            let lo = quantile(data, |_| true, 0.0).unwrap_or(0.0);
            let hi = quantile(data, |_| true, 0.995).unwrap_or(1.0);
            lo + (otsu(data, lo, hi) - lo) * 0.5
        }
    };
    let foreground: Vec<u8> = data.iter().map(|&v| u8::from(v > threshold)).collect();
    let body = if class == ModalityClass::Ct {
        // Opening by one voxel detaches the table (and blankets, cables) before picking the
        // patient as the largest component.
        dilate(&largest_component(&erode(&foreground, dims), dims), dims)
    } else {
        largest_component(&foreground, dims)
    };
    fill_holes(&body, dims, &image.axes)
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
        .map(|(&v, &m)| if m == 0 { 0.0 } else { ((v - lo) / range).clamp(0.0, 1.0) })
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

fn prepare(volume: &Volume, spacing: f64, class: ModalityClass, mask_mode: MaskMode, roi: Option<(Vec3, Vec3)>) -> Prepared {
    let mut image = build_work_image(volume, spacing);
    let mut body = match mask_mode {
        MaskMode::Auto => body_mask(&image, class),
        MaskMode::None => vec![1u8; image.len()],
    };
    apply_roi(&mut body, &image, roi);
    let sampling = if mask_mode == MaskMode::Auto { dilate(&body, image.dims) } else { body.clone() };
    image.data = normalize(&image, &sampling);
    Prepared { image, body, sampling }
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

fn evaluate_metric(kind: MetricKind, bins: usize, samples: &Samples, moving: &WorkImage, fixed_to_moving_index: &Mat4) -> MetricValue {
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
                return MetricValue { cost: 1.0, value: 0.0, normalized: 0.0, overlap };
            }
            let cov = acc[5] - acc[1] * acc[2] / n;
            let var_f = acc[3] - acc[1] * acc[1] / n;
            let var_m = acc[4] - acc[2] * acc[2] / n;
            let corr = if var_f > 1e-12 && var_m > 1e-12 { cov / (var_f * var_m).sqrt() } else { 0.0 };
            // Too little overlap is never a good alignment, however well the remnant correlates.
            let penalty = if overlap < 0.1 { 1.0 - overlap / 0.1 } else { 0.0 };
            MetricValue { cost: -corr + penalty, value: corr, normalized: corr, overlap }
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
                return MetricValue { cost: 1.0, value: 0.0, normalized: 0.0, overlap };
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
            let entropy = |p: &[f64]| -> f64 { p.iter().filter(|&&x| x > 0.0).map(|&x| -x * x.ln()).sum() };
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
            let ecc = if hf + hm > 0.0 { 2.0 * mi / (hf + hm) } else { 0.0 };
            let penalty = if overlap < 0.1 { 1.0 - overlap / 0.1 } else { 0.0 };
            MetricValue { cost: -mi + penalty * mi.abs().max(0.1), value: mi, normalized: ecc, overlap }
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
        if self.cancel.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(Stop::Cancelled);
        }
        if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
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
        let delta = rigid_about(q[0] / self.radius, q[1] / self.radius, q[2] / self.radius, [q[3], q[4], q[5]], self.center);
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
fn line_minimize(objective: &mut LevelObjective, p: &[f64; 6], d: &[f64; 6], f0: f64, step: f64, tolerance: f64) -> Result<(f64, f64), Stop> {
    const GOLD: f64 = 1.618_033_988_749_895;
    const MAX_EXPANSIONS: usize = 12;
    // Bracket a < b < c (in t) with f(b) below both ends.
    let (mut a, mut fa) = (0.0, f0);
    let (mut b, mut fb) = (step, objective.cost(&along(p, d, step))?);
    if fb > fa {
        let (t, ft) = (-step, objective.cost(&along(p, d, -step))?);
        if ft >= fa {
            // The minimum is within one step of 0: bracket [-step, step].
            return brent(objective, p, d, (-step, ft), (0.0, f0), (step, fb), tolerance);
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
        let u = if step.abs() >= tol1 { x + step } else { x + tol1.copysign(step) };
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
fn powell(objective: &mut LevelObjective, step: f64, tolerance: f64, max_iterations: u32) -> Result<([f64; 6], f64, u32, bool), Stop> {
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
        let moved: f64 = (0..6).map(|i| (p[i] - p_start[i]).powi(2)).sum::<f64>().sqrt();
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
            let t = 2.0 * (f_start - 2.0 * fp + fe) * (f_start - fp - biggest_drop).powi(2) - biggest_drop * (f_start - fe).powi(2);
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
    if options.levels_mm.is_empty() || options.levels_mm.iter().any(|&s| !(s.is_finite() && s > 0.0)) {
        return Err(RegistrationError::InvalidOption("levelsMm must be a non-empty list of positive spacings".into()));
    }
    if !(8..=256).contains(&options.histogram_bins) {
        return Err(RegistrationError::InvalidOption("histogramBins must be between 8 and 256".into()));
    }
    if options.max_samples < 1000 {
        return Err(RegistrationError::InvalidOption("maxSamples must be at least 1000".into()));
    }
    if let Some(m) = &options.initial_matrix {
        if m.iter().any(|v| !v.is_finite()) || mat_inverse(m).is_none() {
            return Err(RegistrationError::InvalidOption("initialMatrix must be 16 finite numbers and invertible".into()));
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

/// Quality heuristic in 0..1: how far the final metric is above what a misregistration gives
/// (NCC 0.4 -> 0, 0.9 -> 1; ECC 0.05 -> 0, 0.30 -> 1), scaled down when under half the fixed
/// body lands in the moving volume.
fn quality_score(kind: MetricKind, normalized: f64, overlap: f64) -> f64 {
    let base = match kind {
        MetricKind::Ncc => (normalized - 0.4) / 0.5,
        MetricKind::MattesMi => (normalized - 0.05) / 0.25,
    };
    base.clamp(0.0, 1.0) * (overlap / 0.5).clamp(0.0, 1.0)
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
pub fn register_rigid(fixed: &Volume, moving: &Volume, options: &RegistrationOptions) -> Result<RegistrationResult, RegistrationError> {
    validate_options(options)?;
    let fixed_class = parse_modality(&options.fixed_modality, fixed);
    let moving_class = parse_modality(&options.moving_modality, moving);
    let kind = match options.metric {
        MetricChoice::Ncc => MetricKind::Ncc,
        MetricChoice::MattesMi => MetricKind::MattesMi,
        MetricChoice::Auto if fixed_class == moving_class && fixed_class != ModalityClass::Other => MetricKind::Ncc,
        MetricChoice::Auto => MetricKind::MattesMi,
    };
    let started = Instant::now();
    let budget = Budget { deadline: options.time_limit.map(|limit| started + limit), cancel: options.cancel.clone() };
    let first = run_rigid(fixed, moving, options, kind, fixed_class, moving_class, &budget, started)?;
    // MR-MR is often two different weightings (T1 vs T2), which NCC cannot align: if NCC stays
    // poor, try mutual information from the same start and keep the better result.
    if options.metric == MetricChoice::Auto
        && kind == MetricKind::Ncc
        && fixed_class == ModalityClass::Mr
        && first.normalized_metric < 0.5
        && first.stop_reason != StopReason::TimeLimit
    {
        if let Ok(second) = run_rigid(fixed, moving, options, MetricKind::MattesMi, fixed_class, moving_class, &budget, started) {
            if second.quality > first.quality {
                return Ok(second);
            }
        }
    }
    Ok(first)
}

#[allow(clippy::too_many_arguments)]
fn run_rigid(
    fixed: &Volume,
    moving: &Volume,
    options: &RegistrationOptions,
    kind: MetricKind,
    fixed_class: ModalityClass,
    moving_class: ModalityClass,
    budget: &Budget,
    started: Instant,
) -> Result<RegistrationResult, RegistrationError> {
    let spacings: Vec<f64> = options.levels_mm.iter().map(|&s| fit_spacing(fixed, moving, s, options.max_memory_bytes)).collect();
    let finest = *spacings.last().expect("validated non-empty");

    // Initial moving -> fixed transform.
    let (initial, initialization): (Mat4, &'static str) = if let Some(m) = options.initial_matrix {
        (m, "matrix")
    } else {
        match options.initialization {
            Initialization::None => (IDENTITY, "none"),
            Initialization::Translation | Initialization::Centroid => {
                let estimate = if options.initialization == Initialization::Translation {
                    estimate_translation(
                        fixed,
                        moving,
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
                match estimate {
                    Some(estimate) if estimate.confidence >= 0.3 => (estimate.matrix, "translation"),
                    _ => {
                        let spacing = finest.max(4.0);
                        let f = prepare(fixed, spacing, fixed_class, options.mask, options.roi);
                        let m = prepare(moving, spacing, moving_class, options.mask, None);
                        match (mask_centroid(&f), mask_centroid(&m)) {
                            (Some(cf), Some(cm)) => (translation_matrix(sub(cf, cm)), "centroid"),
                            _ => (IDENTITY, "none"),
                        }
                    }
                }
            }
        }
    };
    // A cancel before the first level is an error; a time limit just leaves the initial transform.
    if let Err(Stop::Cancelled) = budget.check() {
        return Err(RegistrationError::Cancelled);
    }

    let mut fixed_to_moving = mat_inverse(&initial).ok_or_else(|| RegistrationError::InvalidOption("initial transform is singular".into()))?;
    let mut levels = Vec::new();
    let mut total_iterations = 0;
    let mut total_evaluations = 0;
    let mut stop_reason = StopReason::Converged;
    let mut converged_all = true;
    let mut working_bytes = 0u64;
    let mut final_metric = None;
    let mut initial_metric_value = 0.0;
    let mut center = [0.0; 3];

    for (level, &spacing) in spacings.iter().enumerate() {
        let fixed_prepared = prepare(fixed, spacing, fixed_class, options.mask, options.roi);
        if fixed_prepared.body.iter().all(|&m| m == 0) {
            return Err(RegistrationError::EmptyMask("fixed"));
        }
        let moving_prepared = prepare(moving, spacing, moving_class, options.mask, None);
        if moving_prepared.body.iter().all(|&m| m == 0) {
            return Err(RegistrationError::EmptyMask("moving"));
        }
        let samples = select_samples(&fixed_prepared, options.max_samples);
        working_bytes = working_bytes.max(
            work_image_bytes(fixed, spacing) + work_image_bytes(moving, spacing) + samples.points.len() as u64 * 16,
        );
        center = mask_centroid(&fixed_prepared).unwrap_or([0.0; 3]);
        // Rotations are scaled by the body's radius of gyration, so one parameter unit moves the
        // anatomy about one millimetre whichever parameter it is.
        let radius = {
            let mut sum = 0.0;
            for p in &samples.points {
                let p = [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])];
                sum += dot(sub(p, center), sub(p, center));
            }
            (sum / samples.points.len().max(1) as f64).sqrt().max(20.0)
        };
        let mut objective = LevelObjective {
            kind,
            bins: options.histogram_bins,
            samples: &samples,
            moving: &moving_prepared.image,
            moving_world_to_index: moving_prepared.image.world_to_index(),
            start: fixed_to_moving,
            center,
            radius,
            budget,
            evaluations: 0,
            best: ([0.0; 6], f64::INFINITY),
        };
        let start_metric = objective.metric(&[0.0; 6]);
        if start_metric.overlap <= 0.0 {
            return Err(RegistrationError::NoOverlap);
        }
        if level + 1 == spacings.len() {
            initial_metric_value = start_metric.value;
        }
        let step = spacing;
        let tolerance = (spacing * 0.05).max(0.02);
        let outcome = powell(&mut objective, step, tolerance, options.max_iterations);
        let (q, iterations, converged) = match outcome {
            Ok((q, _, iterations, converged)) => (q, iterations, converged),
            Err(Stop::Cancelled) => return Err(RegistrationError::Cancelled),
            Err(Stop::TimeLimit) => {
                stop_reason = StopReason::TimeLimit;
                (objective.best.0, 0, false)
            }
        };
        // The best point evaluated (Powell's result, or the best before a time limit hit).
        let q = if objective.best.1.is_finite() { objective.best.0 } else { q };
        fixed_to_moving = objective.transform(&q);
        let metric = objective.metric(&q);
        total_iterations += iterations;
        total_evaluations += objective.evaluations;
        if !converged {
            converged_all = false;
            if stop_reason != StopReason::TimeLimit {
                stop_reason = StopReason::MaxIterations;
            }
        }
        levels.push(LevelResult { spacing_mm: spacing, iterations, metric_evaluations: objective.evaluations, metric_value: metric.value });
        final_metric = Some(metric);
        if stop_reason == StopReason::TimeLimit {
            break;
        }
    }

    let metric = final_metric.expect("at least one level ran");
    let matrix = mat_inverse(&fixed_to_moving).expect("a rigid transform is invertible");
    let quality = quality_score(kind, metric.normalized, metric.overlap);
    Ok(RegistrationResult {
        matrix,
        rotation_deg: euler_deg(&matrix),
        translation_mm: [matrix[3], matrix[7], matrix[11]],
        center_mm: center,
        metric: kind,
        metric_value: metric.value,
        initial_metric_value,
        normalized_metric: metric.normalized,
        overlap_fraction: metric.overlap,
        quality,
        quality_label: quality_label(quality),
        iterations: total_iterations,
        metric_evaluations: total_evaluations,
        levels,
        initialization,
        converged: converged_all && stop_reason == StopReason::Converged,
        stop_reason,
        timing_ms: started.elapsed().as_secs_f64() * 1000.0,
        working_bytes,
    })
}

// ---------------------------------------------------------------------------------------------
// Translation estimate
// ---------------------------------------------------------------------------------------------

/// Working-grid spacing of the translation estimate's profiles, mm.
const PROFILE_GRID_MM: f64 = 3.0;
/// Profile bin width along patient S-I, mm.
const PROFILE_BIN_MM: f64 = 1.0;

struct Profiles {
    /// S-I (patient z) of bin 0, mm.
    z0: f64,
    /// Channels, each one value per bin: cross-sectional area (mm^2) or integrated intensity.
    channels: Vec<(&'static str, Vec<f64>)>,
    /// Whether the scan fully covers each bin's height.
    valid: Vec<bool>,
    /// Body voxels' (x, y) sums and counts per bin, for the in-plane centroid.
    xy: Vec<(f64, f64, f64)>,
}

/// Per-bin profiles of one volume along patient z (S-I), whatever its acquisition orientation:
/// every working voxel spreads over the bins its own z extent covers.
fn profiles(volume: &Volume, class: ModalityClass, intensity_channel: bool, ct_channels: bool) -> Profiles {
    let image = build_work_image(volume, PROFILE_GRID_MM);
    let body = body_mask(&image, class);
    let normalized = if intensity_channel { Some(normalize(&image, &body)) } else { None };
    let [nx, ny, nz] = image.dims;
    let extent: f64 = (0..3).map(|a| image.axes[a][2].abs() * image.spacing[a]).sum::<f64>().max(PROFILE_BIN_MM);
    let voxel_volume = image.spacing[0] * image.spacing[1] * image.spacing[2];
    // Range of z over the grid's corners.
    let mut z_lo = f64::INFINITY;
    let mut z_hi = f64::NEG_INFINITY;
    for &k in &[0, nz - 1] {
        for &j in &[0, ny - 1] {
            for &i in &[0, nx - 1] {
                let z = image.world(i, j, k)[2];
                z_lo = z_lo.min(z);
                z_hi = z_hi.max(z);
            }
        }
    }
    let z0 = (z_lo - extent).floor();
    let bins = ((z_hi + extent - z0) / PROFILE_BIN_MM).ceil() as usize + 1;
    let mut names: Vec<&'static str> = vec!["body"];
    if ct_channels {
        names.extend(["lung", "bone"]);
    }
    if intensity_channel {
        names.push("intensity");
    }
    let mut channels: Vec<Vec<f64>> = vec![vec![0.0; bins]; names.len()];
    let mut coverage = vec![0.0; bins];
    let mut xy = vec![(0.0, 0.0, 0.0); bins];
    for k in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                let index = k * nx * ny + j * nx + i;
                let p = image.world(i, j, k);
                let (lo, hi) = (p[2] - extent / 2.0, p[2] + extent / 2.0);
                let first = (((lo - z0) / PROFILE_BIN_MM).floor().max(0.0)) as usize;
                let last = ((((hi - z0) / PROFILE_BIN_MM).floor()) as usize).min(bins - 1);
                let in_body = body[index] != 0;
                let v = image.data[index];
                let mut weights = [0.0f64; 4];
                weights[0] = f64::from(u8::from(in_body));
                if ct_channels {
                    weights[1] = f64::from(u8::from(in_body && v > -950.0 && v < -500.0));
                    weights[2] = f64::from(u8::from(in_body && v > 200.0));
                }
                if let Some(n) = &normalized {
                    weights[names.len() - 1] = f64::from(n[index]);
                }
                for bin in first..=last {
                    let b_lo = z0 + bin as f64 * PROFILE_BIN_MM;
                    let overlap = (hi.min(b_lo + PROFILE_BIN_MM) - lo.max(b_lo)).max(0.0) / extent;
                    if overlap <= 0.0 {
                        continue;
                    }
                    let share = voxel_volume * overlap / PROFILE_BIN_MM;
                    coverage[bin] += share;
                    for (channel, &w) in channels.iter_mut().zip(&weights) {
                        channel[bin] += w * share;
                    }
                    if in_body {
                        xy[bin].0 += p[0] * overlap;
                        xy[bin].1 += p[1] * overlap;
                        xy[bin].2 += overlap;
                    }
                }
            }
        }
    }
    let full = coverage.iter().cloned().fold(0.0, f64::max);
    let valid = coverage.iter().map(|&c| c >= 0.9 * full && full > 0.0).collect();
    Profiles { z0, channels: names.into_iter().zip(channels).collect(), valid, xy }
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
    if va <= 1e-12 || vb <= 1e-12 {
        0.0
    } else {
        cov / (va * vb).sqrt()
    }
}

/// A fast moving -> fixed translation: z from cross-correlating per-height profiles (body area;
/// plus lung and bone area for CT-CT, and integrated normalized intensity for MR-MR / PT-PT),
/// x/y from the body-mask centroids over the matched overlap.
pub fn estimate_translation(fixed: &Volume, moving: &Volume, options: &TranslationOptions) -> Result<TranslationEstimate, RegistrationError> {
    if !(options.min_overlap_mm.is_finite() && options.min_overlap_mm > 0.0) {
        return Err(RegistrationError::InvalidOption("minOverlapMm must be a positive number".into()));
    }
    let started = Instant::now();
    let fixed_class = parse_modality(&options.fixed_modality, fixed);
    let moving_class = parse_modality(&options.moving_modality, moving);
    let same = fixed_class == moving_class;
    let ct = same && fixed_class == ModalityClass::Ct;
    let intensity = same && !ct;
    let (f, m) = rayon::join(|| profiles(fixed, fixed_class, intensity, ct), || profiles(moving, moving_class, intensity, ct));

    let f_bins = f.valid.len() as i64;
    let m_bins = m.valid.len() as i64;
    let min_overlap = (options.min_overlap_mm / PROFILE_BIN_MM).ceil() as usize;
    // dz (mm) maps moving z to fixed z. Fixed bin i <-> moving bin j = i + offset, where
    // offset = (f.z0 - m.z0 - dz) / bin.
    let base = (f.z0 - m.z0) / PROFILE_BIN_MM;
    let mut scores: Vec<(i64, f64, usize)> = Vec::new();
    for offset in -(f_bins)..=m_bins {
        let dz = (base - offset as f64) * PROFILE_BIN_MM;
        if options.max_shift_mm.is_some_and(|max| dz.abs() > max) {
            continue;
        }
        let pairs: Vec<(usize, usize)> = (0..f_bins)
            .filter_map(|i| {
                let j = i + offset;
                (j >= 0 && j < m_bins && f.valid[i as usize] && m.valid[j as usize]).then_some((i as usize, j as usize))
            })
            .collect();
        if pairs.len() < min_overlap {
            continue;
        }
        let mut score = 0.0;
        for ((_, fc), (_, mc)) in f.channels.iter().zip(&m.channels) {
            let a: Vec<f64> = pairs.iter().map(|&(i, _)| fc[i]).collect();
            let b: Vec<f64> = pairs.iter().map(|&(_, j)| mc[j]).collect();
            score += pearson(&a, &b);
        }
        scores.push((offset, score / f.channels.len() as f64, pairs.len()));
    }
    let profiles_used: Vec<&'static str> = f.channels.iter().map(|(name, _)| *name).collect();
    let Some(&(best_offset, peak, overlap_bins)) = scores.iter().max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0))) else {
        return Err(RegistrationError::NoOverlap);
    };
    // Sub-bin refinement by a parabola through the peak and its neighbours.
    let score_at = |offset: i64| scores.iter().find(|s| s.0 == offset).map(|s| s.1);
    let refine = match (score_at(best_offset - 1), score_at(best_offset + 1)) {
        (Some(left), Some(right)) => {
            let denominator = left - 2.0 * peak + right;
            if denominator < -1e-12 { (0.5 * (left - right) / denominator).clamp(-0.5, 0.5) } else { 0.0 }
        }
        _ => 0.0,
    };
    let offset_f = best_offset as f64 + refine;
    let dz = (base - offset_f) * PROFILE_BIN_MM;
    // The runner-up is the best OTHER local maximum at least 20 mm away (the main peak's own
    // shoulders are not an alternative answer).
    let second = scores
        .iter()
        .enumerate()
        .filter(|(index, s)| {
            let left = index.checked_sub(1).and_then(|i| scores.get(i)).filter(|l| l.0 == s.0 - 1).map_or(f64::NEG_INFINITY, |l| l.1);
            let right = scores.get(index + 1).filter(|r| r.0 == s.0 + 1).map_or(f64::NEG_INFINITY, |r| r.1);
            s.1 >= left && s.1 >= right && (s.0 - best_offset).abs() as f64 * PROFILE_BIN_MM > 20.0
        })
        .map(|(_, s)| s.1)
        .fold(f64::NEG_INFINITY, f64::max);
    let second = if second.is_finite() { second } else { 0.0 };
    let confidence = peak.clamp(0.0, 1.0) * ((peak - second.max(0.0)) / 0.3).clamp(0.0, 1.0);

    // In-plane: body centroids over the overlapping bins.
    let (mut fx, mut fy, mut fw, mut mx, mut my, mut mw) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for i in 0..f_bins {
        let j = i + best_offset;
        if j < 0 || j >= m_bins || !f.valid[i as usize] || !m.valid[j as usize] {
            continue;
        }
        let (a, b) = (f.xy[i as usize], m.xy[j as usize]);
        fx += a.0;
        fy += a.1;
        fw += a.2;
        mx += b.0;
        my += b.1;
        mw += b.2;
    }
    let (dx, dy) = if fw > 0.0 && mw > 0.0 { (fx / fw - mx / mw, fy / fw - my / mw) } else { (0.0, 0.0) };
    let translation = [dx, dy, dz];
    Ok(TranslationEstimate {
        matrix: translation_matrix(translation),
        translation_mm: translation,
        confidence,
        peak_correlation: peak,
        second_peak_correlation: second,
        overlap_mm: overlap_bins as f64 * PROFILE_BIN_MM,
        profiles: profiles_used,
        timing_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}
