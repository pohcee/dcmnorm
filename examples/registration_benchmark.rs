//! Registration accuracy benchmark over a local set of real series (benchmarks/registration/README.md).
//!
//! ```text
//! cargo run --release --example registration_benchmark -- <data-root> [cases.json] [--only NAME,...] [--json]
//! ```
//!
//! `cases.json` (default `benchmarks/registration/tcia-cases.json`) lists `{ name, fixed, moving, truth }`: series
//! directories relative to `<data-root>`, and the true moving -> fixed matrix as a JSON file `{ "matrix": [16] }`
//! relative to `<data-root>` (or `null` for "no known truth"). For every case it runs `estimate_translation`, the
//! default `register_rigid` and the multi-start `register_rigid`, and prints the error of each: the mean and max
//! displacement (mm) of 27 points spread over the fixed volume's central 60 %, and the residual rotation (degrees).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use dcmnorm::dicom_io::{
    build_volume, estimate_translation, mat_inverse, mat_mul, register_rigid, rigid_about, rotation_angle_deg, transform_point, Initialization, Mat4,
    RegistrationOptions, TranslationOptions, Volume,
};

fn series_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "dcm"))
        .collect();
    files.sort();
    files
}

fn grid_points(volume: &Volume) -> Vec<[f64; 3]> {
    let mut points = Vec::new();
    let (c, r, s) = (volume.cols as f64, volume.rows as f64, volume.num_slices as f64);
    let m = volume.voxel_to_patient();
    for a in [0.2, 0.5, 0.8] {
        for b in [0.2, 0.5, 0.8] {
            for k in [0.1, 0.5, 0.9] {
                points.push(transform_point(&m, [a * c, b * r, k * s]));
            }
        }
    }
    points
}

/// (mean mm, max mm, rotation deg) of `estimate` against `truth`, over fixed-space `points`.
fn error(estimate: &Mat4, truth: &Mat4, points: &[[f64; 3]]) -> (f64, f64, f64) {
    let inverse_truth = mat_inverse(truth).expect("invertible truth");
    let residual = mat_mul(&inverse_truth, estimate);
    let (mut sum, mut max) = (0.0f64, 0.0f64);
    for p in points {
        let pm = transform_point(&inverse_truth, *p);
        let q = transform_point(&residual, pm);
        let e = ((q[0] - pm[0]).powi(2) + (q[1] - pm[1]).powi(2) + (q[2] - pm[2]).powi(2)).sqrt();
        sum += e;
        max = max.max(e);
    }
    (sum / points.len() as f64, max, rotation_angle_deg(&residual))
}

fn read_matrix(path: &Path) -> Mat4 {
    let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).unwrap();
    let list: Vec<f64> = value["matrix"].as_array().expect("matrix").iter().map(|v| v.as_f64().unwrap()).collect();
    let mut m = [0.0; 16];
    m.copy_from_slice(&list[..16]);
    m
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut only: Option<Vec<String>> = None;
    let mut json = false;
    let mut diverge = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--only" => only = iter.next().map(|v| v.split(',').map(str::to_owned).collect()),
            "--json" => json = true,
            // Also run single starts far from the truth, to see what divergent results score.
            "--diverge" => diverge = true,
            _ => positional.push(arg.clone()),
        }
    }
    let root = PathBuf::from(positional.first().expect("usage: registration_benchmark <data-root> [cases.json] [--only a,b] [--json]"));
    let cases_path = positional
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/registration/tcia-cases.json"));
    let cases: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cases_path).expect("cases file")).expect("cases JSON");
    let cases_dir = cases_path.parent().map(Path::to_path_buf).unwrap_or_default();

    if !json {
        println!(
            "{:<22} | {:>26} | {:>40} | {:>44}",
            "case", "estimate: err [floor] / conf", "rigid (default): err mm / rot / label", "rigid (multi-start): err mm / label / cons."
        );
    }
    let mut rows = Vec::new();
    for case in cases["cases"].as_array().expect("cases[]") {
        let name = case["name"].as_str().unwrap();
        if only.as_ref().is_some_and(|o| !o.iter().any(|n| n == name)) {
            continue;
        }
        let fixed = build_volume(&series_files(&root.join(case["fixed"].as_str().unwrap()))).expect("fixed volume");
        let moving = build_volume(&series_files(&root.join(case["moving"].as_str().unwrap()))).expect("moving volume");
        let truth = case["truth"].as_str().map(|t| {
            let p = cases_dir.join(t);
            if p.exists() { read_matrix(&p) } else { read_matrix(&root.join(t)) }
        });
        let points = grid_points(&fixed);
        // The best a pure translation can do (the truth's rotation sets a floor for the estimate).
        let floor = truth.map(|t| {
            let inverse = mat_inverse(&t).unwrap();
            let mut mean = [0.0; 3];
            for p in &points {
                let q = transform_point(&inverse, *p);
                for a in 0..3 {
                    mean[a] += (p[a] - q[a]) / points.len() as f64;
                }
            }
            error(&dcmnorm::dicom_io::translation_matrix(mean), &t, &points).0
        });
        let fmt_err = |m: &Mat4| truth.map(|t| error(m, &t, &points));

        if diverge {
            if let Some(t) = truth {
                let offsets = [
                    ("z+120", rigid_about(0.0, 0.0, 0.0, [0.0, 0.0, 120.0], [0.0; 3])),
                    ("z-180", rigid_about(0.0, 0.0, 0.0, [0.0, 0.0, -180.0], [0.0; 3])),
                    ("xy+60", rigid_about(0.0, 0.0, 0.0, [60.0, 60.0, 0.0], [0.0; 3])),
                    ("rz25+y40", rigid_about(0.0, 0.0, 25f64.to_radians(), [0.0, 40.0, 0.0], points[13])),
                    ("rx180", rigid_about(std::f64::consts::PI, 0.0, 0.0, [0.0; 3], points[13])),
                ];
                for (label, offset) in offsets {
                    let start = mat_mul(&offset, &t);
                    let r = register_rigid(&fixed, &moving, &RegistrationOptions { initial_matrix: Some(start), max_rotation_deg: None, ..Default::default() });
                    match r {
                        Ok(r) => {
                            let (mean, _, rot) = error(&r.matrix, &t, &points);
                            println!("DIVERGE {name:<22} {label:<9} err {mean:7.1} rotErr {rot:6.1} nm {:.3} q {:.2} {}", r.normalized_metric, r.quality, r.quality_label);
                        }
                        Err(e) => println!("DIVERGE {name:<22} {label:<9} ERR {e}"),
                    }
                    // The same seed with the built-in multi-start.
                    let multi = register_rigid(&fixed, &moving, &RegistrationOptions { initial_matrix: Some(start), initialization: Initialization::Multi, ..Default::default() });
                    match multi {
                        Ok(r) => {
                            let (mean, _, rot) = error(&r.matrix, &t, &points);
                            println!("MULTI   {name:<22} {label:<9} err {mean:7.1} rotErr {rot:6.1} q {:.2} {} consistent {:?} from {}", r.quality, r.quality_label, r.consistent, r.starts.first().map_or("-", |s| s.start.as_str()));
                        }
                        Err(e) => println!("MULTI   {name:<22} {label:<9} ERR {e}"),
                    }
                }
            }
            continue;
        }
        let t0 = Instant::now();
        let estimate = estimate_translation(&fixed, &moving, &TranslationOptions::default());
        let est_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let rigid = register_rigid(&fixed, &moving, &RegistrationOptions::default());
        let rigid_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let t2 = Instant::now();
        let multi = register_rigid(&fixed, &moving, &RegistrationOptions { initialization: Initialization::Multi, ..Default::default() });
        let multi_ms = t2.elapsed().as_secs_f64() * 1000.0;

        let est_cell = match &estimate {
            Ok(e) => match fmt_err(&e.matrix) {
                Some((mean, _, _)) => format!("{mean:5.1} [{:4.1}] / {:.2} ({:.0} ms)", floor.unwrap_or(f64::NAN), e.confidence, est_ms),
                None => format!("   -    / {:.2} ({:.0} ms)", e.confidence, est_ms),
            },
            Err(e) => format!("ERR {e}").chars().take(26).collect(),
        };
        let rigid_cell = |r: &Result<dcmnorm::dicom_io::RegistrationResult, _>, ms: f64, multi: bool| match r {
            Ok(r) => {
                let (mean, _, rot) = fmt_err(&r.matrix).unwrap_or((f64::NAN, f64::NAN, f64::NAN));
                let extra = if multi { format!(" {}", match r.consistent { Some(true) => "yes", Some(false) => "no", None => "-" }) } else { format!(" rot {rot:.1}") };
                format!("{mean:7.1}{extra} {:<4} q {:.2} ({:.0} ms)", r.quality_label, r.quality, ms)
            }
            Err(e) => format!("ERR {e}").chars().take(40).collect(),
        };
        if json {
            let est = estimate.as_ref().ok();
            rows.push(serde_json::json!({
                "case": name,
                "estimate": est.map(|e| serde_json::json!({
                    "errorMm": fmt_err(&e.matrix).map(|x| x.0), "translationFloorMm": floor, "confidence": e.confidence, "overlapMm": e.overlap_mm,
                    "translationMm": e.translation_mm.to_vec(), "ms": est_ms })),
                "rigid": rigid.as_ref().ok().map(|r| serde_json::json!({
                    "errorMm": fmt_err(&r.matrix).map(|x| x.0), "rotErrDeg": fmt_err(&r.matrix).map(|x| x.2), "label": r.quality_label,
                    "quality": r.quality, "normalizedMetric": r.normalized_metric, "ms": rigid_ms })),
                "multi": multi.as_ref().ok().map(|r| serde_json::json!({
                    "errorMm": fmt_err(&r.matrix).map(|x| x.0), "rotErrDeg": fmt_err(&r.matrix).map(|x| x.2), "label": r.quality_label,
                    "quality": r.quality, "consistent": r.consistent, "ms": multi_ms,
                    "starts": r.starts.iter().map(|s| serde_json::json!({ "start": s.start, "errorMm": fmt_err(&s.matrix).map(|x| x.0),
                        "normalizedMetric": s.normalized_metric, "label": s.quality_label, "agrees": s.agrees })).collect::<Vec<_>>() })),
            }));
        } else {
            println!("{:<22} | {:>26} | {:>40} | {:>44}", name, est_cell, rigid_cell(&rigid, rigid_ms, false), rigid_cell(&multi, multi_ms, true));
            if std::env::var_os("BENCH_VERBOSE").is_some() {
                if let Ok(e) = &estimate {
                    println!("    estimate t {:?} peak {:.3} second {:.3} overlap {:.0}", e.translation_mm.map(|v| (v * 10.0).round() / 10.0), e.peak_correlation, e.second_peak_correlation, e.overlap_mm);
                }
                if let Ok(r) = &multi {
                    for s in &r.starts {
                        println!("    start {:<12} err {:6.1} nm {:.3} {:<4} agrees {}", s.start, fmt_err(&s.matrix).map_or(f64::NAN, |x| x.0), s.normalized_metric, s.quality_label, s.agrees);
                    }
                }
            }
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rows).unwrap());
    }
}
