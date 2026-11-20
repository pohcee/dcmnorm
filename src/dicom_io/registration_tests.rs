//! Registration against synthetic anatomy with KNOWN transforms, for every CT / MR / PT pairing.
//!
//! The phantom is a torso defined analytically in the FIXED patient frame (body with a
//! subcutaneous fat layer, two unequal lungs, heart, liver, kidneys, segmented spine, bladder,
//! and for CT a table under the patient). Each modality renders it its own way: CT in HU, MR with
//! a T1-like contrast, a smooth bias field and noise, PET as blurred uptake (counts, not SUV)
//! with multiplicative noise on a coarse 4 mm grid. A moving volume is the same anatomy seen
//! through a known rigid transform T (moving -> fixed): its voxel at moving-patient point p shows
//! the anatomy at T p. Volumes differ in voxel size, slice thickness, field of view and (for one
//! MR) orientation, so the tests exercise anisotropy and resampling, not just the optimizer.
//!
//! Errors are reported as the rotation angle of `T_est * T_true^-1` and the target registration
//! error (TRE) at landmarks spread through the torso.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use rayon::prelude::*;

use super::registration::{
    estimate_translation, mat_inverse, mat_mul, register_rigid, rigid_about, rotation_angle_deg, transform_point,
    translation_matrix, Initialization, Mat4, MetricChoice, MetricKind, RegistrationError, RegistrationOptions,
    RegistrationResult, StopReason, TranslationOptions,
};
use super::volume::Volume;

type Vec3 = [f64; 3];

#[derive(Clone, Copy, PartialEq)]
enum Modality {
    Ct,
    Mr,
    Pt,
}

impl Modality {
    fn code(self) -> &'static str {
        match self {
            Self::Ct => "CT",
            Self::Mr => "MR",
            Self::Pt => "PT",
        }
    }
}

/// Soft membership of an axis-aligned ellipsoid, edge width `w` mm.
fn ellipsoid(p: Vec3, c: Vec3, s: Vec3, w: f64) -> f64 {
    let q = ((p[0] - c[0]) / s[0]).powi(2) + ((p[1] - c[1]) / s[1]).powi(2) + ((p[2] - c[2]) / s[2]).powi(2);
    let d = (q.sqrt() - 1.0) * s[0].min(s[1]).min(s[2]);
    (0.5 - d / w).clamp(0.0, 1.0)
}

fn ellipse_cylinder(p: Vec3, c: [f64; 2], a: f64, b: f64, w: f64) -> f64 {
    let q = ((p[0] - c[0]) / a).powi(2) + ((p[1] - c[1]) / b).powi(2);
    let d = (q.sqrt() - 1.0) * a.min(b);
    (0.5 - d / w).clamp(0.0, 1.0)
}

/// Tissue fractions at a fixed-frame point.
struct Tissues {
    body: f64,
    fat: f64,
    lung: f64,
    heart: f64,
    liver: f64,
    kidney: f64,
    bladder: f64,
    bone: f64,
    marrow_wave: f64,
    table: f64,
}

fn tissues(p: Vec3, w: f64) -> Tissues {
    let z = p[2];
    let a = 160.0 + 15.0 * (z / 70.0).sin();
    let b = 108.0 + 8.0 * (z / 90.0).cos();
    let caps = ((z + 230.0) / w + 0.5).clamp(0.0, 1.0) * ((210.0 - z) / w + 0.5).clamp(0.0, 1.0);
    let body = ellipse_cylinder(p, [0.0, 0.0], a, b, w) * caps;
    let inner = ellipse_cylinder(p, [0.0, 0.0], a - 16.0, b - 14.0, w) * caps;
    let heart = ellipsoid(p, [25.0, 5.0, 20.0], [45.0, 40.0, 42.0], w);
    let lung = (ellipsoid(p, [82.0, 0.0, 65.0], [45.0, 55.0, 85.0], w) + ellipsoid(p, [-75.0, -5.0, 75.0], [52.0, 62.0, 92.0], w))
        .min(1.0)
        * (1.0 - heart);
    let liver = ellipsoid(p, [-62.0, 8.0, -42.0], [75.0, 60.0, 55.0], w) * (1.0 - lung);
    let kidney = (ellipsoid(p, [66.0, 55.0, -92.0], [22.0, 18.0, 45.0], w) + ellipsoid(p, [-64.0, 58.0, -100.0], [21.0, 17.0, 44.0], w)).min(1.0);
    let bladder = ellipsoid(p, [0.0, -10.0, -185.0], [40.0, 35.0, 30.0], w);
    // Spine, plus the non-periodic bone a real torso has: scapulae and sternum up top, iliac
    // wings below (a spine alone is periodic every vertebra, ~28 mm).
    let spine = ellipse_cylinder(p, [0.0, 78.0], 17.0, 15.0, w) * caps;
    let girdle = ellipsoid(p, [-105.0, 70.0, 112.0], [30.0, 12.0, 38.0], w)
        + ellipsoid(p, [108.0, 68.0, 108.0], [28.0, 12.0, 40.0], w)
        + ellipsoid(p, [0.0, -95.0, 70.0], [12.0, 7.0, 55.0], w)
        + ellipsoid(p, [-92.0, 45.0, -128.0], [38.0, 30.0, 26.0], w)
        + ellipsoid(p, [95.0, 42.0, -132.0], [36.0, 30.0, 25.0], w);
    let bone = (spine + girdle.min(1.0) * body).min(1.0);
    let marrow_wave = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * z / 28.0).cos();
    let table = if (p[0].abs() < 220.0) && p[1] > 126.0 && p[1] < 138.0 { 1.0 } else { 0.0 };
    Tissues { body, fat: (body - inner).max(0.0), lung, heart, liver, kidney, bladder, bone, marrow_wave, table }
}

fn noise(seed: u64, index: usize) -> f64 {
    // Sum of four uniforms: roughly Gaussian, unit variance.
    let mut total = 0.0;
    for k in 0..4u64 {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (index as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ k.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
        x ^= x >> 32;
        total += (x >> 11) as f64 / (1u64 << 53) as f64;
    }
    (total - 2.0) * (3.0f64).sqrt()
}

fn value(modality: Modality, p: Vec3, seed: u64, index: usize) -> f64 {
    match modality {
        Modality::Ct => {
            let t = tissues(p, 1.0);
            let soft = 45.0;
            let mut v = -1000.0 * (1.0 - t.body) + t.body * soft;
            v += t.fat * (-100.0 - soft);
            v += t.lung * (-850.0 - soft) * t.body;
            v += t.liver * 20.0 + t.heart * (-5.0) + t.kidney * (-10.0) + t.bladder * (-35.0);
            v += t.bone * (300.0 + 450.0 * t.marrow_wave);
            v += t.table * (1.0 - t.body) * 1300.0;
            v + 8.0 * noise(seed, index)
        }
        Modality::Mr => {
            let t = tissues(p, 1.5);
            let mut v = t.body * 380.0;
            v += t.fat * (950.0 - 380.0);
            v += t.lung * (40.0 - 380.0) * t.body;
            v += t.liver * 120.0 + t.heart * (-160.0) + t.kidney * (-60.0) + t.bladder * (-300.0);
            v += t.bone * (120.0 + 350.0 * t.marrow_wave - 380.0);
            let bias = 1.0 + 0.25 * (p[0] / 150.0).sin() * (p[2] / 200.0).cos() + 0.1 * (p[1] / 120.0);
            (v * bias + 15.0 * noise(seed, index)).max(0.0)
        }
        Modality::Pt => {
            let t = tissues(p, 7.0);
            let mut suv = t.body * 1.0;
            suv += t.fat * (0.4 - 1.0);
            suv += t.lung * (0.35 - 1.0) * t.body;
            suv += t.liver * 1.5 + t.heart * 3.0 + t.kidney * 4.0 + t.bladder * 11.0 + t.bone * 0.3;
            let counts = suv * 4200.0; // Bq/ml for a 70 kg patient; nothing assumes SUV
            (counts * (1.0 + 0.12 * noise(seed, index))).max(0.0)
        }
    }
}

struct Grid {
    cols: u32,
    rows: u32,
    slices: u32,
    col_spacing: f64,
    row_spacing: f64,
    slice_spacing: f64,
    origin: Vec3,
    row_vector: Vec3,
    col_vector: Vec3,
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// An axial grid centred on `center` (moving-patient coordinates).
fn axial(cols: u32, rows: u32, slices: u32, spacing: f64, slice_spacing: f64, center: Vec3) -> Grid {
    Grid {
        cols,
        rows,
        slices,
        col_spacing: spacing,
        row_spacing: spacing,
        slice_spacing,
        origin: [
            center[0] - (cols - 1) as f64 * spacing / 2.0,
            center[1] - (rows - 1) as f64 * spacing / 2.0,
            center[2] - (slices - 1) as f64 * slice_spacing / 2.0,
        ],
        row_vector: [1.0, 0.0, 0.0],
        col_vector: [0.0, 1.0, 0.0],
    }
}

/// The anatomy seen through `moving_to_fixed`, sampled on `grid` (moving-patient coordinates).
fn synthetic_volume(modality: Modality, grid: &Grid, moving_to_fixed: &Mat4, seed: u64) -> Volume {
    let normal = cross(grid.row_vector, grid.col_vector);
    let plane = (grid.cols * grid.rows) as usize;
    let mut samples = vec![0f32; plane * grid.slices as usize];
    samples.par_chunks_mut(plane).enumerate().for_each(|(k, out)| {
        for r in 0..grid.rows as usize {
            for c in 0..grid.cols as usize {
                let p = [0, 1, 2].map(|a| {
                    grid.origin[a]
                        + grid.row_vector[a] * c as f64 * grid.col_spacing
                        + grid.col_vector[a] * r as f64 * grid.row_spacing
                        + normal[a] * k as f64 * grid.slice_spacing
                });
                let index = k * plane + r * grid.cols as usize + c;
                out[r * grid.cols as usize + c] = value(modality, transform_point(moving_to_fixed, p), seed, index) as f32;
            }
        }
    });
    let z0 = normal[0] * grid.origin[0] + normal[1] * grid.origin[1] + normal[2] * grid.origin[2];
    Volume {
        samples,
        rows: grid.rows,
        cols: grid.cols,
        num_slices: grid.slices,
        origin: grid.origin,
        row_vector: grid.row_vector,
        col_vector: grid.col_vector,
        slice_normal: normal,
        col_spacing_mm: grid.col_spacing,
        row_spacing_mm: grid.row_spacing,
        slice_zs: (0..grid.slices).map(|k| z0 + k as f64 * grid.slice_spacing).collect(),
        modality: Some(modality.code().to_owned()),
        frame_of_reference_uid: None,
        series_instance_uid: None,
        study_instance_uid: None,
        gantry_tilt_deg: 0.0,
        desheared: false,
    }
}

fn fixed_grid(modality: Modality) -> Grid {
    match modality {
        Modality::Ct => axial(256, 256, 90, 1.5, 3.0, [0.0, 10.0, 0.0]),
        Modality::Mr => axial(224, 192, 52, 1.8, 5.0, [0.0, 0.0, -10.0]),
        Modality::Pt => axial(110, 110, 72, 4.0, 4.0, [0.0, 0.0, 0.0]),
    }
}

/// Moving grids: different voxel sizes, fields of view and centres; the MR is CORONAL.
fn moving_grid(modality: Modality) -> Grid {
    match modality {
        Modality::Ct => axial(240, 240, 110, 1.7, 2.5, [5.0, 25.0, 20.0]),
        Modality::Mr => {
            // Coronal: columns along patient left, rows head -> foot, slices anterior -> posterior.
            let (cols, rows, slices, spacing, slice_spacing) = (200u32, 170u32, 56u32, 2.0, 4.0);
            Grid {
                cols,
                rows,
                slices,
                col_spacing: spacing,
                row_spacing: spacing,
                slice_spacing,
                origin: [-((cols - 1) as f64) * spacing / 2.0, -110.0, 150.0],
                row_vector: [1.0, 0.0, 0.0],
                col_vector: [0.0, 0.0, -1.0],
            }
        }
        Modality::Pt => axial(100, 100, 80, 4.0, 3.27, [10.0, 15.0, 30.0]),
    }
}

/// The known moving -> fixed transform: 3 / -4 / 6 degrees about a point off-centre, plus a
/// 14 / -11 / 32 mm shift (the 32 mm S-I offset is the current/prior case).
fn known_transform() -> Mat4 {
    rigid_about(3f64.to_radians(), -4f64.to_radians(), 6f64.to_radians(), [14.0, -11.0, 32.0], [10.0, 20.0, 0.0])
}

/// Landmarks in the fixed frame, spread through the torso the fixed and moving scans share.
const LANDMARKS: [Vec3; 9] = [
    [0.0, 0.0, 0.0],
    [80.0, 0.0, 60.0],
    [-75.0, -5.0, 70.0],
    [25.0, 5.0, 20.0],
    [-60.0, 8.0, -40.0],
    [66.0, 55.0, -80.0],
    [0.0, 78.0, 40.0],
    [-120.0, -40.0, -20.0],
    [110.0, 60.0, 10.0],
];

struct Errors {
    rotation_deg: f64,
    tre_mean: f64,
    tre_max: f64,
}

fn errors(estimated: &Mat4, truth: &Mat4) -> Errors {
    let residual = mat_mul(estimated, &mat_inverse(truth).unwrap());
    let truth_inverse = mat_inverse(truth).unwrap();
    let tre: Vec<f64> = LANDMARKS
        .iter()
        .map(|&p_fixed| {
            let p_moving = transform_point(&truth_inverse, p_fixed);
            let e = transform_point(estimated, p_moving);
            ((e[0] - p_fixed[0]).powi(2) + (e[1] - p_fixed[1]).powi(2) + (e[2] - p_fixed[2]).powi(2)).sqrt()
        })
        .collect();
    Errors {
        rotation_deg: rotation_angle_deg(&residual),
        tre_mean: tre.iter().sum::<f64>() / tre.len() as f64,
        tre_max: tre.iter().cloned().fold(0.0, f64::max),
    }
}

fn run_pair(fixed: Modality, moving: Modality) -> (RegistrationResult, Errors) {
    run_pair_with(fixed, moving, known_transform())
}

fn run_pair_with(fixed: Modality, moving: Modality, truth: Mat4) -> (RegistrationResult, Errors) {
    let identity = translation_matrix([0.0; 3]);
    let fixed_volume = synthetic_volume(fixed, &fixed_grid(fixed), &identity, 1);
    let moving_volume = synthetic_volume(moving, &moving_grid(moving), &truth, 2);
    let result = register_rigid(&fixed_volume, &moving_volume, &RegistrationOptions::default()).unwrap();
    let e = errors(&result.matrix, &truth);
    eprintln!(
        "{}->{}: metric {} {:.4} (start {:.4}) quality {:.2} {} | rotation error {:.3} deg, TRE mean {:.2} mm max {:.2} mm | init {} | {} iterations, {} evaluations, {:.0} ms, {:.0} MB working",
        moving.code(),
        fixed.code(),
        result.metric.as_str(),
        result.metric_value,
        result.initial_metric_value,
        result.quality,
        result.quality_label,
        e.rotation_deg,
        e.tre_mean,
        e.tre_max,
        result.initialization,
        result.iterations,
        result.metric_evaluations,
        result.timing_ms,
        result.working_bytes as f64 / 1e6,
    );
    (result, e)
}

fn assert_recovered(fixed: Modality, moving: Modality, max_rotation_deg: f64, max_tre_mm: f64) {
    let (result, e) = run_pair(fixed, moving);
    let expected_metric = if fixed == moving { MetricKind::Ncc } else { MetricKind::MattesMi };
    assert_eq!(result.metric, expected_metric);
    assert!(
        e.rotation_deg < max_rotation_deg && e.tre_max < max_tre_mm,
        "{}->{}: rotation error {:.3} deg, TRE max {:.2} mm",
        moving.code(),
        fixed.code(),
        e.rotation_deg,
        e.tre_max
    );
    assert!(result.quality >= 0.4, "quality {} for a correct registration", result.quality);
}

#[test]
fn ct_to_ct_recovers_a_known_rigid_transform() {
    assert_recovered(Modality::Ct, Modality::Ct, 0.5, 1.5);
}

#[test]
fn mr_to_mr_recovers_a_known_rigid_transform_from_a_coronal_stack() {
    assert_recovered(Modality::Mr, Modality::Mr, 0.75, 2.0);
}

#[test]
fn pt_to_pt_recovers_a_known_rigid_transform() {
    assert_recovered(Modality::Pt, Modality::Pt, 1.0, 3.0);
}

#[test]
fn pt_to_ct_recovers_a_known_rigid_transform() {
    assert_recovered(Modality::Ct, Modality::Pt, 1.5, 4.0);
}

#[test]
fn mr_to_ct_recovers_a_known_rigid_transform() {
    assert_recovered(Modality::Ct, Modality::Mr, 1.0, 3.0);
}

#[test]
fn pt_to_mr_recovers_a_known_rigid_transform() {
    assert_recovered(Modality::Mr, Modality::Pt, 1.5, 4.0);
}

#[test]
fn ct_to_mr_recovers_a_known_rigid_transform() {
    assert_recovered(Modality::Mr, Modality::Ct, 1.0, 3.0);
}

/// Capture range: a 12 degree in-plane rotation plus 6 / -5 degrees out of plane and a 55 mm
/// S-I offset, from the automatic initialization (translation only).
#[test]
fn large_rotations_are_recovered_for_same_and_cross_modality() {
    let truth = rigid_about(6f64.to_radians(), -5f64.to_radians(), 12f64.to_radians(), [-18.0, 9.0, 55.0], [0.0, 30.0, 10.0]);
    for (fixed, moving, max_rotation, max_tre) in [(Modality::Ct, Modality::Ct, 0.5, 1.5), (Modality::Ct, Modality::Pt, 1.5, 4.0), (Modality::Mr, Modality::Mr, 0.75, 2.0)] {
        let (_, e) = run_pair_with(fixed, moving, truth);
        assert!(e.rotation_deg < max_rotation && e.tre_max < max_tre, "{}->{}: {:.3} deg, {:.2} mm", moving.code(), fixed.code(), e.rotation_deg, e.tre_max);
    }
}

#[test]
fn translation_estimate_finds_a_large_si_offset_and_table_shift() {
    for (fixed, moving) in [(Modality::Ct, Modality::Ct), (Modality::Mr, Modality::Mr), (Modality::Pt, Modality::Pt), (Modality::Ct, Modality::Pt)] {
        let truth = translation_matrix([6.0, -14.0, 41.0]);
        let f = synthetic_volume(fixed, &fixed_grid(fixed), &translation_matrix([0.0; 3]), 3);
        let m = synthetic_volume(moving, &moving_grid(moving), &truth, 4);
        let estimate = estimate_translation(&f, &m, &TranslationOptions::default()).unwrap();
        let e = errors(&estimate.matrix, &truth);
        eprintln!(
            "translation {}->{}: [{:.2}, {:.2}, {:.2}] (truth [6, -14, 41]) confidence {:.2} peak {:.3} second {:.3} profiles {:?} {:.0} ms",
            moving.code(),
            fixed.code(),
            estimate.translation_mm[0],
            estimate.translation_mm[1],
            estimate.translation_mm[2],
            estimate.confidence,
            estimate.peak_correlation,
            estimate.second_peak_correlation,
            estimate.profiles,
            estimate.timing_ms
        );
        assert!((estimate.translation_mm[2] - 41.0).abs() < 2.0, "{}->{} dz {}", moving.code(), fixed.code(), estimate.translation_mm[2]);
        assert!(e.tre_max < 4.0, "{}->{} TRE {}", moving.code(), fixed.code(), e.tre_max);
        assert!(estimate.confidence > 0.3, "confidence {}", estimate.confidence);
    }
}

#[test]
fn registration_is_deterministic_across_runs_and_thread_counts() {
    let f = synthetic_volume(Modality::Ct, &fixed_grid(Modality::Ct), &translation_matrix([0.0; 3]), 5);
    let m = synthetic_volume(Modality::Ct, &moving_grid(Modality::Ct), &known_transform(), 6);
    let options = RegistrationOptions { levels_mm: vec![8.0, 4.0], ..Default::default() };
    let a = register_rigid(&f, &m, &options).unwrap();
    let b = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap().install(|| register_rigid(&f, &m, &options).unwrap());
    let c = rayon::ThreadPoolBuilder::new().num_threads(3).build().unwrap().install(|| register_rigid(&f, &m, &options).unwrap());
    assert_eq!(a.matrix.map(f64::to_bits), b.matrix.map(f64::to_bits));
    assert_eq!(a.matrix.map(f64::to_bits), c.matrix.map(f64::to_bits));
    assert_eq!(a.metric_evaluations, c.metric_evaluations);
}

#[test]
fn identical_volumes_register_to_identity() {
    let f = synthetic_volume(Modality::Ct, &fixed_grid(Modality::Ct), &translation_matrix([0.0; 3]), 7);
    let result = register_rigid(&f, &f, &RegistrationOptions::default()).unwrap();
    let e = errors(&result.matrix, &translation_matrix([0.0; 3]));
    assert!(e.rotation_deg < 0.05 && e.tre_max < 0.2, "rotation {} TRE {}", e.rotation_deg, e.tre_max);
    assert!(result.normalized_metric > 0.99);
    assert_eq!(result.quality_label, "good");
}

#[test]
fn cancel_and_time_limit_stop_the_registration() {
    let f = synthetic_volume(Modality::Pt, &fixed_grid(Modality::Pt), &translation_matrix([0.0; 3]), 8);
    let m = synthetic_volume(Modality::Pt, &moving_grid(Modality::Pt), &known_transform(), 9);
    let cancel = Arc::new(AtomicBool::new(true));
    let cancelled = register_rigid(&f, &m, &RegistrationOptions { cancel: Some(cancel), ..Default::default() });
    assert!(matches!(cancelled, Err(RegistrationError::Cancelled)));

    let limited = register_rigid(
        &f,
        &m,
        &RegistrationOptions { time_limit: Some(Duration::from_millis(1)), initialization: Initialization::None, ..Default::default() },
    )
    .unwrap();
    assert_eq!(limited.stop_reason, StopReason::TimeLimit);
    assert!(!limited.converged);
    // Stopped at the (wrong) identity start: the quality must say so.
    assert_eq!(limited.quality_label, "poor", "quality {} at a 32 mm misregistration", limited.quality);
}

#[test]
fn an_initial_matrix_is_used_and_invalid_options_are_rejected() {
    let f = synthetic_volume(Modality::Pt, &fixed_grid(Modality::Pt), &translation_matrix([0.0; 3]), 10);
    let m = synthetic_volume(Modality::Pt, &moving_grid(Modality::Pt), &known_transform(), 11);
    // A rough initial guess (the viewer's translation) close enough for the pyramid.
    let initial = translation_matrix([12.0, -8.0, 30.0]);
    let result = register_rigid(&f, &m, &RegistrationOptions { initial_matrix: Some(initial), ..Default::default() }).unwrap();
    assert_eq!(result.initialization, "matrix");
    assert!(errors(&result.matrix, &known_transform()).tre_max < 3.0);

    for bad in [
        RegistrationOptions { levels_mm: vec![], ..Default::default() },
        RegistrationOptions { histogram_bins: 2, ..Default::default() },
        RegistrationOptions { initial_matrix: Some([0.0; 16]), ..Default::default() },
    ] {
        assert!(matches!(register_rigid(&f, &m, &bad), Err(RegistrationError::InvalidOption(_))));
    }
    assert!(register_rigid(&f, &m, &RegistrationOptions { metric: MetricChoice::Ncc, mask: super::registration::MaskMode::None, ..Default::default() }).is_ok());
}

/// The render-server-sized benchmark: a 512 x 512 x 300 CT (0.78 mm pixels, 1.25 mm slices)
/// against a prior of the same size. `cargo test --release -p dcmnorm --lib registration_benchmark -- --ignored --nocapture`
#[test]
#[ignore]
fn registration_benchmark_512x512x300_ct() {
    let started = std::time::Instant::now();
    let f = synthetic_volume(Modality::Ct, &axial(512, 512, 300, 0.78, 1.25, [0.0, 10.0, 0.0]), &translation_matrix([0.0; 3]), 12);
    let m = synthetic_volume(Modality::Ct, &axial(512, 512, 300, 0.78, 1.25, [5.0, 25.0, 20.0]), &known_transform(), 13);
    eprintln!("synthesized two 512x512x300 volumes in {:.1} s ({} MB each)", started.elapsed().as_secs_f64(), f.samples.len() * 4 / 1_000_000);
    let estimate = estimate_translation(&f, &m, &TranslationOptions::default()).unwrap();
    eprintln!(
        "estimate_translation: {:.0} ms, confidence {:.2}, TRE max {:.2} mm against the rigid truth (translation only, so the rotation remains)",
        estimate.timing_ms,
        estimate.confidence,
        errors(&estimate.matrix, &known_transform()).tre_max
    );
    for threads in [1usize, 4, 0] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let result = pool.install(|| register_rigid(&f, &m, &RegistrationOptions::default()).unwrap());
        let e = errors(&result.matrix, &known_transform());
        eprintln!(
            "register_rigid threads={}: {:.0} ms, {} evaluations, levels {:?}, rotation error {:.3} deg, TRE max {:.2} mm, working {:.0} MB",
            if threads == 0 { rayon::current_num_threads() } else { threads },
            result.timing_ms,
            result.metric_evaluations,
            result.levels.iter().map(|l| (l.spacing_mm, l.metric_evaluations)).collect::<Vec<_>>(),
            e.rotation_deg,
            e.tre_max,
            result.working_bytes as f64 / 1e6
        );
    }
}

/// Enhanced multi-frame objects go through `build_volume` like classic series: an Enhanced CT
/// fixed and an Enhanced PET / Enhanced MR moving, each one file, register to the known transform.
#[test]
fn enhanced_multi_frame_volumes_register() {
    use super::test_support::{write_synthetic_enhanced, SyntheticStack};
    use super::volume::build_volume;
    let stack = |modality: &'static str, n: usize, spacing: f64, slices: usize, slice_spacing: f64, center: Vec3| {
        let half = (n - 1) as f64 * spacing / 2.0;
        let z0 = center[2] - (slices - 1) as f64 * slice_spacing / 2.0;
        SyntheticStack {
            rows: n as u16,
            cols: n as u16,
            row_vector: [1.0, 0.0, 0.0],
            col_vector: [0.0, 1.0, 0.0],
            pixel_spacing: [spacing, spacing],
            positions: (0..slices).map(|k| [center[0] - half, center[1] - half, z0 + k as f64 * slice_spacing]).collect(),
            modality,
            frame_of_reference_uid: super::volume_export::generate_uid(),
            study_instance_uid: super::volume_export::generate_uid(),
            series_instance_uid: super::volume_export::generate_uid(),
        }
    };
    let truth = known_transform();
    let identity = translation_matrix([0.0; 3]);
    let reversed = |n: usize| (0..n).rev().collect::<Vec<_>>();
    let fixed_stack = stack("CT", 160, 2.4, 70, 4.0, [0.0, 10.0, 0.0]);
    let fixed_path = write_synthetic_enhanced(&fixed_stack, &reversed(70), |p| value(Modality::Ct, transform_point(&identity, p), 1, 0));
    let fixed = build_volume(&[&fixed_path]).unwrap();
    for (code, modality, scale_to_int16) in [("PT", Modality::Pt, 0.5), ("MR", Modality::Mr, 1.0)] {
        let moving_stack = stack(code, 100, 4.0, 70, 4.0, [10.0, 15.0, 30.0]);
        let moving_path = write_synthetic_enhanced(&moving_stack, &reversed(70), |p| value(modality, transform_point(&truth, p), 2, 0) * scale_to_int16);
        let moving = build_volume(&[&moving_path]).unwrap();
        std::fs::remove_dir_all(moving_path.parent().unwrap()).ok();
        assert_eq!(moving.modality.as_deref(), Some(code));
        let result = register_rigid(&fixed, &moving, &RegistrationOptions::default()).unwrap();
        let e = errors(&result.matrix, &truth);
        eprintln!("enhanced {code}->CT: rotation error {:.3} deg, TRE max {:.2} mm, {:.0} ms", e.rotation_deg, e.tre_max, result.timing_ms);
        assert_eq!(result.metric, MetricKind::MattesMi);
        assert!(e.rotation_deg < 1.5 && e.tre_max < 4.0, "enhanced {code}->CT: {:.3} deg, {:.2} mm", e.rotation_deg, e.tre_max);
        let estimate = estimate_translation(&fixed, &moving, &TranslationOptions::default()).unwrap();
        assert!((estimate.translation_mm[2] - truth[11]).abs() < 6.0, "enhanced {code} dz {}", estimate.translation_mm[2]);
    }
    std::fs::remove_dir_all(fixed_path.parent().unwrap()).ok();
}

// ---------------------------------------------------------------------------------------------
// Robustness: truncation by a smaller reconstruction field of view, cropped ends, wrong seeds
// (the failure modes measured on public TCIA pairs; benchmarks/registration).
// ---------------------------------------------------------------------------------------------

/// Sets every voxel outside an in-plane disc of `radius` mm around the grid's centre to `value`:
/// what a scan reconstructed with a smaller field of view than the patient looks like.
fn truncate_to_fov(volume: &mut Volume, radius: f64, value: f32) {
    let (cols, rows) = (volume.cols as usize, volume.rows as usize);
    let (cx, cy) = ((cols - 1) as f64 / 2.0, (rows - 1) as f64 / 2.0);
    for (index, v) in volume.samples.iter_mut().enumerate() {
        let (c, r) = ((index % (cols * rows)) % cols, (index % (cols * rows)) / cols);
        let (dx, dy) = ((c as f64 - cx) * volume.col_spacing_mm, (r as f64 - cy) * volume.row_spacing_mm);
        if dx * dx + dy * dy > radius * radius {
            *v = value;
        }
    }
}

/// The CT moving grid with its top `crop` mm of slices removed.
fn cropped_ct_grid(crop: f64) -> Grid {
    let full = moving_grid(Modality::Ct);
    let dropped = (crop / full.slice_spacing).round() as u32;
    Grid { slices: full.slices - dropped, ..full }
}

#[test]
fn translation_estimate_survives_fov_truncation_and_a_cropped_end() {
    let truth = translation_matrix([6.0, -14.0, 41.0]);
    let fixed = synthetic_volume(Modality::Ct, &fixed_grid(Modality::Ct), &translation_matrix([0.0; 3]), 21);
    let mut moving = synthetic_volume(Modality::Ct, &cropped_ct_grid(60.0), &truth, 22);
    // The phantom is 320 mm wide: a 230 mm FOV cuts off both flanks and opens the lungs.
    truncate_to_fov(&mut moving, 115.0, -1000.0);
    let estimate = estimate_translation(&fixed, &moving, &TranslationOptions::default()).unwrap();
    let t = estimate.translation_mm;
    eprintln!("truncated + cropped CT: {:?} (truth [6, -14, 41]) confidence {:.2} agreement {:.2} xy {:.2}", t, estimate.confidence, estimate.channel_agreement, estimate.xy_correlation);
    assert!((t[2] - 41.0).abs() < 3.0 && (t[0] - 6.0).abs() < 4.0 && (t[1] + 14.0).abs() < 4.0, "estimate {t:?}");
    assert!(estimate.confidence >= 0.3, "confidence {}", estimate.confidence);

    // The rigid registration from it is correct, and its quality says so: the anatomy the moving
    // scan never saw does not count against it.
    let result = register_rigid(&fixed, &moving, &RegistrationOptions::default()).unwrap();
    let e = errors(&result.matrix, &truth);
    eprintln!("truncated + cropped CT rigid: TRE max {:.2} mm, quality {:.2} {} (body metric {:.3}, NCC {:.3})", e.tre_max, result.quality, result.quality_label, result.body_metric, result.normalized_metric);
    assert!(e.tre_max < 3.0, "TRE {}", e.tre_max);
    assert_eq!(result.quality_label, "good", "quality {}", result.quality);
    assert!(result.plausible && result.consistent.is_none() && result.starts.is_empty());
}

#[test]
fn translation_estimate_matches_short_mr_slabs() {
    // Two 65 mm axial MR slabs 20 mm apart: 45 mm of overlap, under the old fixed 60 mm minimum.
    let slab = |center: Vec3| axial(224, 192, 14, 1.8, 5.0, center);
    let truth = translation_matrix([4.0, -6.0, 20.0]);
    let fixed = synthetic_volume(Modality::Mr, &slab([0.0, 0.0, -150.0]), &translation_matrix([0.0; 3]), 23);
    let moving = synthetic_volume(Modality::Mr, &slab([0.0, 0.0, -150.0]), &truth, 24);
    let estimate = estimate_translation(&fixed, &moving, &TranslationOptions::default()).unwrap();
    eprintln!("MR slabs: {:?} confidence {:.2} min overlap {:.0} mm", estimate.translation_mm, estimate.confidence, estimate.min_overlap_mm);
    assert!(estimate.min_overlap_mm < 45.0);
    assert!((estimate.translation_mm[2] - 20.0).abs() < 3.0, "dz {}", estimate.translation_mm[2]);
    // An explicit minimum without the fraction is still honoured.
    let strict = TranslationOptions { min_overlap_mm: 60.0, min_overlap_fraction: None, ..Default::default() };
    assert!(estimate_translation(&fixed, &moving, &strict).map_or(true, |e| (e.translation_mm[2] - 20.0).abs() > 3.0 || e.overlap_mm >= 60.0));
}

#[test]
fn translation_confidence_is_low_without_matching_anatomy() {
    // A featureless water cylinder against the torso: nothing to match, whatever the shift.
    let fixed = synthetic_volume(Modality::Ct, &fixed_grid(Modality::Ct), &translation_matrix([0.0; 3]), 25);
    let mut moving = synthetic_volume(Modality::Ct, &moving_grid(Modality::Ct), &translation_matrix([0.0; 3]), 26);
    let (cols, rows) = (moving.cols as usize, moving.rows as usize);
    for (index, v) in moving.samples.iter_mut().enumerate() {
        let (c, r) = ((index % (cols * rows)) % cols, (index % (cols * rows)) / cols);
        let (dx, dy) = ((c as f64 - cols as f64 / 2.0) * 1.7, (r as f64 - rows as f64 / 2.0) * 1.7);
        *v = if dx * dx + dy * dy < 120.0 * 120.0 { 10.0 } else { -1000.0 };
    }
    let estimate = estimate_translation(&fixed, &moving, &TranslationOptions::default()).unwrap();
    eprintln!("featureless: confidence {:.2}", estimate.confidence);
    assert!(estimate.confidence < 0.3, "confidence {}", estimate.confidence);
}

#[test]
fn multi_start_recovers_from_a_wrong_seed_and_reports_consistency() {
    let truth = known_transform();
    let fixed = synthetic_volume(Modality::Ct, &fixed_grid(Modality::Ct), &translation_matrix([0.0; 3]), 27);
    let moving = synthetic_volume(Modality::Ct, &moving_grid(Modality::Ct), &truth, 28);
    // A seed 150 mm off in S-I (a capture-on-link of the wrong slices).
    let seed = mat_mul(&translation_matrix([0.0, 0.0, 150.0]), &truth);
    let single = register_rigid(&fixed, &moving, &RegistrationOptions { initial_matrix: Some(seed), ..Default::default() });
    if let Ok(single) = &single {
        let e = errors(&single.matrix, &truth);
        eprintln!("wrong seed, single start: TRE max {:.1} mm, quality {:.2} {}", e.tre_max, single.quality, single.quality_label);
        if e.tre_max > 10.0 {
            assert_eq!(single.quality_label, "poor", "a divergent result must be poor (quality {})", single.quality);
        }
    }
    let multi = register_rigid(&fixed, &moving, &RegistrationOptions { initial_matrix: Some(seed), initialization: Initialization::Multi, ..Default::default() }).unwrap();
    let e = errors(&multi.matrix, &truth);
    eprintln!(
        "wrong seed, multi-start: TRE max {:.2} mm, {} {:.2}, consistent {:?}, starts {:?}, {:.0} ms",
        e.tre_max,
        multi.quality_label,
        multi.quality,
        multi.consistent,
        multi.starts.iter().map(|s| (s.start.as_str(), s.quality_label, s.agrees, s.refined)).collect::<Vec<_>>(),
        multi.timing_ms
    );
    assert!(e.tre_max < 1.5 && e.rotation_deg < 0.5, "TRE {} rotation {}", e.tre_max, e.rotation_deg);
    assert_eq!(multi.consistent, Some(true));
    assert_eq!(multi.quality_label, "good");
    assert!(multi.starts.len() >= 3 && multi.starts[0].refined && multi.starts[0].agrees);
    assert!(multi.starts.iter().any(|s| s.start == "matrix"));
    // Extra starts are accepted and named.
    let with_starts = register_rigid(&fixed, &moving, &RegistrationOptions { starts: vec![seed], ..Default::default() }).unwrap();
    assert!(with_starts.starts.iter().any(|s| s.start == "start-1"));
    assert!(errors(&with_starts.matrix, &truth).tre_max < 1.5);
}

#[test]
fn implausible_rotations_are_flagged_poor() {
    let fixed = synthetic_volume(Modality::Ct, &fixed_grid(Modality::Ct), &translation_matrix([0.0; 3]), 29);
    let moving = synthetic_volume(Modality::Ct, &moving_grid(Modality::Ct), &known_transform(), 30);
    // Upside down: the optimizer stays near 180 degrees.
    let flipped = mat_mul(&rigid_about(std::f64::consts::PI, 0.0, 0.0, [0.0; 3], [0.0, 10.0, 0.0]), &known_transform());
    let result = register_rigid(&fixed, &moving, &RegistrationOptions { initial_matrix: Some(flipped), max_iterations: 10, ..Default::default() }).unwrap();
    eprintln!("flipped: rotation {:.1} deg, plausible {}, {} {:.2}", result.rotation_angle_deg, result.plausible, result.quality_label, result.quality);
    assert!(result.rotation_angle_deg > 30.0);
    assert!(!result.plausible);
    assert_eq!(result.quality_label, "poor");
    // Asked not to limit it, the same result is labelled by its metric alone (still poor here).
    let unlimited = register_rigid(&fixed, &moving, &RegistrationOptions { initial_matrix: Some(flipped), max_iterations: 10, max_rotation_deg: None, ..Default::default() }).unwrap();
    assert!(unlimited.plausible);
    assert!(register_rigid(&fixed, &moving, &RegistrationOptions { max_rotation_deg: Some(-1.0), ..Default::default() }).is_err());
}
