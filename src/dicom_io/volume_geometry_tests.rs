//! Geometry of `build_volume` against analytic phantoms: gantry-tilted (sheared) stacks, tilted
//! non-axial stacks, irregular slice spacing, and the GPU texture export of each. Every slice is
//! written from scratch by `test_support`, so the expected value at any patient point is known
//! exactly and a shear or stretch shows up as a measurable position error, not a vague "looks
//! plausible".

use super::test_support::{
    remove_stack, write_synthetic_enhanced, write_synthetic_stack, SyntheticStack, Vec3,
};
use super::texture_export::{pack_volume_texture, TextureCompression};
use super::volume::{
    build_volume, reformat_plane_values, resample_volume, Interpolation, PlaneParams,
    SlabProjection,
};

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn scale(a: Vec3, s: f64) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// A vertical (patient S-I) Gaussian rod at x = 5, y = -8 (sigma 4 mm, peak 1000) plus a
/// Gaussian blob at (-10, 6, 30) (sigma 3 mm, peak 800). Smooth, so trilinear interpolation
/// error stays well below the shear being tested for.
fn phantom(p: Vec3) -> f64 {
    let rod = 1000.0 * (-((p[0] - 5.0).powi(2) + (p[1] + 8.0).powi(2)) / (2.0 * 16.0)).exp();
    let blob = 800.0
        * (-((p[0] + 10.0).powi(2) + (p[1] - 6.0).powi(2) + (p[2] - 30.0).powi(2)) / (2.0 * 9.0))
            .exp();
    rod + blob
}

/// Value-weighted centroid of a reformatted axial-in-LPS plane at height `z`, restricted to the
/// rod (a window around x = 5, y = -8), in patient mm.
fn rod_centroid_at(volume: &super::volume::Volume, z: f64) -> (f64, f64) {
    let params = PlaneParams {
        origin: [5.0, -8.0, z],
        row_dir: [1.0, 0.0, 0.0],
        col_dir: [0.0, 1.0, 0.0],
        output_width: 40,
        output_height: 40,
        spacing_mm: 0.5,
        window_center: None,
        window_width: None,
        interpolation: Interpolation::Trilinear,
        slab_thickness_mm: 0.0,
        slab_projection: SlabProjection::MaximumIntensity,
    };
    let values = reformat_plane_values(volume, &params).unwrap();
    let (mut sx, mut sy, mut sw) = (0.0, 0.0, 0.0);
    for row in 0..40 {
        for col in 0..40 {
            let w = f64::from(values[row * 40 + col]).max(0.0);
            sx += w * (5.0 + (col as f64 - 20.0) * 0.5);
            sy += w * (-8.0 + (row as f64 - 20.0) * 0.5);
            sw += w;
        }
    }
    (sx / sw, sy / sw)
}

/// An axial-ish CT tilted `tilt_deg` about the patient L-R axis: the slice plane is rotated, but
/// the table still moves along patient z, so `ImagePositionPatient` steps straight along z.
fn tilted_axial_stack(tilt_deg: f64, spacings: &[f64]) -> SyntheticStack {
    let t = tilt_deg.to_radians();
    let row_vector = [1.0, 0.0, 0.0];
    let col_vector = [0.0, t.cos(), -t.sin()];
    let mut positions = Vec::new();
    let mut z = -10.0;
    positions.push([-32.0, -40.0, z]);
    for &gap in spacings {
        z += gap;
        positions.push([-32.0, -40.0, z]);
    }
    SyntheticStack {
        rows: 72,
        cols: 64,
        row_vector,
        col_vector,
        pixel_spacing: [1.0, 1.0],
        positions,
        modality: "CT",
        frame_of_reference_uid: "2.25.1001".into(),
        study_instance_uid: "2.25.1002".into(),
        series_instance_uid: "2.25.1003".into(),
    }
}

#[test]
fn a_gantry_tilted_stack_is_detected_desheared_and_samples_the_true_anatomy() {
    let stack = tilted_axial_stack(20.0, &[2.0; 39]);
    let paths = write_synthetic_stack(&stack, phantom);
    let volume = build_volume(&paths).unwrap();
    remove_stack(&paths);

    assert!(volume.desheared, "a 20 degree tilt must be desheared");
    assert!(
        (volume.gantry_tilt_deg - 20.0).abs() < 0.05,
        "tilt {}",
        volume.gantry_tilt_deg
    );
    // The desheared grid covers the union of every slice's footprint: 78 mm of table travel x
    // sin 20 = 26.7 mm of in-plane drift along the column direction, so 27 extra rows.
    assert_eq!(volume.cols, 64);
    assert_eq!(volume.rows, 72 + 27);
    // The normal is the tilted plane's own normal and slices are spaced 2 cos(20) mm along it.
    let normal = cross(stack.row_vector, stack.col_vector);
    assert!(
        (volume.slice_normal[1] - normal[1]).abs() < 1e-5
            && (volume.slice_normal[2] - normal[2]).abs() < 1e-5
    );
    let expected_gap = 2.0 * 20f64.to_radians().cos();
    for pair in volume.slice_zs.windows(2) {
        assert!((pair[1] - pair[0] - expected_gap).abs() < 1e-4);
    }

    // The rod is vertical in patient space, so every axial (LPS) cut must find it at the same
    // x/y. A sheared volume moves it by z * tan(tilt) per mm of height: ~18 mm over this range.
    for z in [8.0, 20.0, 35.0, 50.0] {
        let (x, y) = rod_centroid_at(&volume, z);
        eprintln!("tilt 20 deg: rod centroid at z={z}: ({x:.3}, {y:.3}), truth (5, -8)");
        assert!(
            (x - 5.0).abs() < 0.1 && (y + 8.0).abs() < 0.1,
            "rod at z={z} found at ({x:.3}, {y:.3})"
        );
    }

    // Point samples throughout the interior match the analytic phantom as well as an untilted
    // stack with the same slice spacing does (the residual is plain trilinear interpolation
    // error on 1 mm pixels / 1.9 mm slices, which the control measures).
    let points = [
        (5.0, -8.0, 12.0),
        (-10.0, 6.0, 30.0),
        (0.0, 0.0, 25.0),
        (7.0, -5.0, 45.0),
        (-12.0, 4.0, 28.0),
        (-9.0, 7.5, 31.0),
    ];
    let worst_error = |volume: &super::volume::Volume| -> f64 {
        points
            .iter()
            .map(|&(x, y, z)| {
                (volume.sample_world([x, y, z]).expect("inside the volume") - phantom([x, y, z]))
                    .abs()
            })
            .fold(0.0, f64::max)
    };
    let control_stack = SyntheticStack {
        positions: (0..40)
            .map(|k| [-32.0, -40.0, -10.0 + k as f64 * expected_gap])
            .collect(),
        col_vector: [0.0, 1.0, 0.0],
        ..tilted_axial_stack(0.0, &[])
    };
    let control_paths = write_synthetic_stack(&control_stack, phantom);
    let control = build_volume(&control_paths).unwrap();
    remove_stack(&control_paths);
    let (tilted_error, control_error) = (worst_error(&volume), worst_error(&control));
    eprintln!("tilt 20 deg: worst point error {tilted_error:.1}, untilted control {control_error:.1} (peak 1000)");
    assert!(
        tilted_error < control_error * 1.5 + 5.0,
        "tilted worst error {tilted_error:.1} vs untilted control {control_error:.1} (of a 1000 peak)"
    );
}

#[test]
fn an_untilted_stack_is_left_exactly_as_read() {
    let stack = tilted_axial_stack(0.0, &[2.0; 9]);
    let paths = write_synthetic_stack(&stack, phantom);
    let volume = build_volume(&paths).unwrap();
    remove_stack(&paths);
    assert!(!volume.desheared);
    assert_eq!(volume.gantry_tilt_deg, 0.0);
    assert_eq!((volume.rows, volume.cols), (72, 64));
    assert_eq!(volume.origin, [-32.0, -40.0, -10.0]);
}

#[test]
fn deshear_also_handles_tilted_non_axial_stacks_with_irregular_spacing() {
    // A sagittal-ish stack (rows run anterior->posterior... columns head->foot), rotated 12
    // degrees off true sagittal, stepping along a direction 15 degrees off its own normal, with
    // a missing slice (one 4 mm gap among 2 mm gaps).
    let a = 12f64.to_radians();
    let row_vector = [a.sin(), a.cos(), 0.0];
    let col_vector = [0.0, 0.0, -1.0];
    let normal = cross(row_vector, col_vector);
    let tilt = 15f64.to_radians();
    let step_dir = add(scale(normal, tilt.cos()), scale(col_vector, tilt.sin()));
    let start = [30.0, -40.0, 70.0];
    let mut positions = vec![start];
    let mut along = 0.0;
    for gap in [
        2.0, 2.0, 2.0, 4.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0,
    ] {
        along += gap;
        positions.push(add(start, scale(step_dir, along)));
    }
    let stack = SyntheticStack {
        rows: 80,
        cols: 72,
        row_vector,
        col_vector,
        pixel_spacing: [1.0, 1.0],
        positions,
        modality: "MR",
        frame_of_reference_uid: "2.25.2001".into(),
        study_instance_uid: "2.25.2002".into(),
        series_instance_uid: "2.25.2003".into(),
    };
    let paths = write_synthetic_stack(&stack, phantom);
    let volume = build_volume(&paths).unwrap();
    remove_stack(&paths);

    assert!(volume.desheared);
    assert!(
        (volume.gantry_tilt_deg - 15.0).abs() < 0.05,
        "tilt {}",
        volume.gantry_tilt_deg
    );
    assert!(volume.has_irregular_spacing());
    assert_eq!(volume.modality.as_deref(), Some("MR"));
    assert_eq!(volume.frame_of_reference_uid.as_deref(), Some("2.25.2001"));

    // The rod is along patient z and the stack covers x in about [-4, 30]: sample the rod's
    // axis and its surroundings at several heights; a sheared volume would be off by up to
    // 34 mm * tan(15) = 9 mm here.
    let mut worst: f64 = 0.0;
    for z in [40.0, 50.0, 60.0] {
        for (dx, dy) in [(0.0, 0.0), (2.0, 1.0), (-3.0, 2.0)] {
            let p = [5.0 + dx, -8.0 + dy, z];
            if let Some(sampled) = volume.sample_world(p) {
                worst = worst.max((sampled - phantom(p)).abs());
            } else {
                panic!("{p:?} should be inside the volume");
            }
        }
    }
    assert!(worst < 20.0, "worst point error {worst} (of a 1000 peak)");
}

#[test]
fn texture_export_of_an_irregular_stack_is_a_uniform_lattice_by_position() {
    // Slices at z = 0, 2, 4, 8, 10 (one missing). A phantom that is linear in z makes the
    // expected value of every exported texture slice exact: value = 100 * z.
    let stack = SyntheticStack {
        rows: 8,
        cols: 8,
        row_vector: [1.0, 0.0, 0.0],
        col_vector: [0.0, 1.0, 0.0],
        pixel_spacing: [1.0, 1.0],
        positions: [0.0, 2.0, 4.0, 8.0, 10.0]
            .iter()
            .map(|&z| [0.0, 0.0, z])
            .collect(),
        modality: "CT",
        frame_of_reference_uid: "2.25.3001".into(),
        study_instance_uid: "2.25.3002".into(),
        series_instance_uid: "2.25.3003".into(),
    };
    let paths = write_synthetic_stack(&stack, |p| 100.0 * p[2]);
    let volume = build_volume(&paths).unwrap();
    remove_stack(&paths);
    assert!(volume.has_irregular_spacing());

    let uniform = resample_volume(&volume, 8, 8, 5, Interpolation::Trilinear);
    for slice in 0..5 {
        let expected = 100.0 * 2.5 * slice as f32; // uniform 2.5 mm across the 10 mm span
        assert!(
            (uniform[slice * 64 + 27] - expected).abs() < 1e-3,
            "slice {slice}: {}",
            uniform[slice * 64 + 27]
        );
    }

    let packed = pack_volume_texture(&volume, None, None, TextureCompression::None).unwrap();
    assert_eq!(packed.meta.depth, 5);
    assert!((packed.meta.slice_spacing_mm - 2.5).abs() < 1e-9);
    // Texel -> physical value through the texture's own rescale; texels are int16 or uint16
    // little-endian depending on the range.
    let texel = |index: usize| -> f64 {
        let bytes = [packed.payload[index * 2], packed.payload[index * 2 + 1]];
        let raw = if packed.meta.sample_format.as_str() == "int16" {
            f64::from(i16::from_le_bytes(bytes))
        } else {
            f64::from(u16::from_le_bytes(bytes))
        };
        raw * packed.meta.rescale_slope + packed.meta.rescale_intercept
    };
    for slice in 0..5 {
        let expected = 250.0 * slice as f64;
        assert!(
            (texel(slice * 64 + 27) - expected).abs() < 1.0,
            "texture slice {slice}: {}",
            texel(slice * 64 + 27)
        );
    }
}

#[test]
fn downsampled_texture_spacing_spans_the_native_extent_exactly() {
    let stack = tilted_axial_stack(0.0, &[1.0; 29]);
    let paths = write_synthetic_stack(&stack, phantom);
    let volume = build_volume(&paths).unwrap();
    remove_stack(&paths);
    let packed = pack_volume_texture(&volume, Some(36), None, TextureCompression::None).unwrap();
    assert!(packed.meta.downsampled);
    let (w, h, d) = (packed.meta.width, packed.meta.height, packed.meta.depth);
    // First and last exported voxel centres coincide with the native ones on every axis.
    assert!(((w - 1) as f64 * packed.meta.col_spacing_mm - 63.0).abs() < 1e-9);
    assert!(((h - 1) as f64 * packed.meta.row_spacing_mm - 71.0).abs() < 1e-9);
    assert!(((d - 1) as f64 * packed.meta.slice_spacing_mm - 29.0).abs() < 1e-9);
}

/// One Enhanced CT/MR/PET file is a whole volume: frames stored out of spatial order, each with
/// its own rescale, positions in the per-frame groups and orientation in the shared group. The
/// volume must match a classic one-file-per-slice build of the same phantom voxel for voxel.
#[test]
fn an_enhanced_multi_frame_object_builds_the_same_volume_as_classic_slices() {
    for modality in ["CT", "MR", "PT"] {
        let classic_stack = SyntheticStack {
            modality,
            ..tilted_axial_stack(0.0, &[2.0; 11])
        };
        let classic_paths = write_synthetic_stack(&classic_stack, phantom);
        let classic = build_volume(&classic_paths).unwrap();
        remove_stack(&classic_paths);

        let order = [5, 0, 11, 3, 7, 1, 9, 2, 10, 4, 8, 6];
        let enhanced_path = write_synthetic_enhanced(&classic_stack, &order, phantom);
        let enhanced = build_volume(&[&enhanced_path]).unwrap();
        std::fs::remove_dir_all(enhanced_path.parent().unwrap()).ok();

        assert_eq!(enhanced.modality.as_deref(), Some(modality));
        assert_eq!(
            (enhanced.rows, enhanced.cols, enhanced.num_slices),
            (classic.rows, classic.cols, classic.num_slices)
        );
        assert_eq!(enhanced.origin, classic.origin);
        assert_eq!(enhanced.slice_zs, classic.slice_zs);
        // Both builds round to stored integers: the classic one by 0.5, the per-frame rescaled one
        // by up to slope / 2 (slope <= 2.375 here).
        let worst = enhanced
            .samples
            .iter()
            .zip(&classic.samples)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst <= 1.7, "{modality}: worst voxel difference {worst}");
    }
}

#[test]
fn a_tilted_enhanced_object_and_a_list_of_enhanced_objects_are_handled() {
    // Tilted: deshear applies to frames exactly like to classic slices.
    let stack = tilted_axial_stack(15.0, &[2.0; 23]);
    let order: Vec<usize> = (0..24).rev().collect();
    let path = write_synthetic_enhanced(&stack, &order, phantom);
    let volume = build_volume(&[&path]).unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
    assert!(volume.desheared && (volume.gantry_tilt_deg - 15.0).abs() < 0.05);
    let (x, y) = rod_centroid_at(&volume, 20.0);
    assert!(
        (x - 5.0).abs() < 0.1 && (y + 8.0).abs() < 0.1,
        "rod found at ({x:.3}, {y:.3})"
    );

    // Two enhanced files (two halves of one stack) plus a repeated frame at an existing
    // position (a second temporal position): one volume, the duplicate dropped.
    let stack = tilted_axial_stack(0.0, &[2.0; 9]);
    let first = write_synthetic_enhanced(&stack, &[0, 1, 2, 3, 4], phantom);
    let second = write_synthetic_enhanced(&stack, &[5, 6, 7, 8, 9, 4], phantom);
    let volume = build_volume(&[&second, &first]).unwrap();
    for path in [&first, &second] {
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
    assert_eq!(volume.num_slices, 10);
    assert!(!volume.has_irregular_spacing());
}
