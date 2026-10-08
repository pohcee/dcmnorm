//! Builds the vendored OpenHTJ2K core (vendor/openhtj2k) plus src/htj2k_bridge.cpp.
//!
//! OpenHTJ2K picks its SIMD code paths at *compile* time (`OPENHTJ2K_TRY_AVX2` only takes effect
//! when the compiler targets AVX2, i.e. `__AVX2__` is defined) and has no runtime CPU dispatch.
//! Its own CMake build simply uses `-march=native`, which is wrong for a binary that's built in
//! one place and run in another (the committed Node/Python/Java bindings, the release CLI .deb, an
//! edge VM whose hypervisor may not expose AVX2 at all). So on x86-64 Linux this builds the library
//! twice:
//!
//! - `base`: the plain x86-64 baseline, runs anywhere.
//! - `v3`: `-march=x86-64-v3` (AVX2/FMA/BMI2/...), roughly 1.7-2x faster decode/encode.
//!
//! and src/htj2k.rs picks one at runtime from the CPU's actual feature set. The two copies can't
//! just be linked side by side: OpenHTJ2K exports hundreds of symbols outside its namespace
//! (marker classes, free functions, a `ThreadPool` singleton). Each variant is therefore compiled
//! with `-fvisibility=hidden`, partially linked into a single relocatable object (`c++ -r`, with
//! COMDAT groups dissolved), and run through `objcopy --keep-global-symbols`, which leaves only the
//! bridge's variant-named `extern "C"` entry points global. Any other target - or a toolchain without `objcopy` - falls back to a
//! single `base` build (NEON is part of the aarch64 baseline, so that variant is already fast).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CORE_DIRS: &[&str] = &["codestream", "coding", "transform", "interface", "jph"];
const INCLUDE_DIRS: &[&str] = &["common", "codestream", "coding", "transform", "interface", "jph"];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dcmnorm_htj2k_v3)");
    println!("cargo:rerun-if-changed=src/htj2k_bridge.cpp");
    println!("cargo:rerun-if-changed=vendor/openhtj2k");
    println!("cargo:rerun-if-env-changed=DCMNORM_HTJ2K_SINGLE_VARIANT");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let core = manifest_dir.join("vendor/openhtj2k/source/core");
    let sources = core_sources(&core, &manifest_dir.join("src/htj2k_bridge.cpp"));
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // DCMNORM_HTJ2K_SINGLE_VARIANT=1 skips the dual build (portable variant only).
    let single_variant = env::var_os("DCMNORM_HTJ2K_SINGLE_VARIANT").is_some();
    if target_arch == "x86_64" && target_os == "linux" && !single_variant {
        match build_isolated_variants(&core, &sources, &out_dir) {
            Ok(()) => {
                println!("cargo:rustc-cfg=dcmnorm_htj2k_v3");
                link_runtime();
                return;
            }
            Err(error) => {
                println!(
                    "cargo:warning=dcmnorm-jpeg2000: falling back to a single portable OpenHTJ2K \
                     build (no AVX2 variant): {error}"
                );
            }
        }
    }

    let mut build = base_build(&core, &target_arch);
    build.define("DCMNORM_HTJ2K_VARIANT", "base");
    build.files(&sources);
    build.compile("dcmnorm_openhtj2k");
    link_runtime();
}

fn core_sources(core: &Path, bridge: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    for dir in CORE_DIRS {
        let mut files: Vec<PathBuf> = fs::read_dir(core.join(dir))
            .unwrap_or_else(|e| panic!("missing vendored OpenHTJ2K dir {dir}: {e}"))
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "cpp")
                    && !path.file_name().unwrap().to_string_lossy().ends_with("_wasm.cpp")
            })
            .collect();
        files.sort();
        sources.extend(files);
    }
    sources.push(bridge.to_path_buf());
    sources
}

fn base_build(core: &Path, target_arch: &str) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .opt_level(3)
        .warnings(false)
        .extra_warnings(false)
        .flag_if_supported("-w")
        .flag_if_supported("-fexceptions")
        .flag_if_supported("-pthread")
        .define("OPENHTJ2K_THREAD", None)
        .define("NDEBUG", None);
    for dir in INCLUDE_DIRS {
        build.include(core.join(dir));
    }
    match target_arch {
        "aarch64" => {
            build.define("OPENHTJ2K_ENABLE_ARM_NEON", None);
            build.flag_if_supported("-flax-vector-conversions");
        }
        "x86_64" => {
            // Inert unless the compiler also targets AVX2 (the v3 variant below).
            build.define("OPENHTJ2K_TRY_AVX2", None);
        }
        _ => {}
    }
    build
}

fn build_isolated_variants(core: &Path, sources: &[PathBuf], out_dir: &Path) -> Result<(), String> {
    let objcopy = env::var("OBJCOPY").unwrap_or_else(|_| "objcopy".to_owned());
    let mut variant_objects = Vec::new();

    for (variant, march) in [("base", None), ("v3", Some("-march=x86-64-v3"))] {
        let mut build = base_build(core, "x86_64");
        build
            .define("DCMNORM_HTJ2K_VARIANT", variant)
            .flag("-fvisibility=hidden")
            .flag("-fvisibility-inlines-hidden")
            // GCC emits function-local statics of inline functions (and inline variables) as
            // STB_GNU_UNIQUE, which objcopy can't localize; plain weak symbols it can.
            .flag_if_supported("-fno-gnu-unique")
            .flag("-fPIC")
            .files(sources)
            .out_dir(out_dir.join(format!("htj2k-{variant}")))
            .cargo_metadata(false);
        if let Some(march) = march {
            build.flag(march).flag("-mtune=generic");
        }
        let objects = build
            .try_compile_intermediates()
            .map_err(|e| format!("compiling the {variant} variant failed: {e}"))?;

        let combined = out_dir.join(format!("htj2k_{variant}.o"));
        let compiler = build.get_compiler();
        let status = Command::new(compiler.path())
            .arg("-r")
            .arg("-nostdlib")
            // Dissolve COMDAT groups: otherwise the final link deduplicates e.g. a std::vector
            // instantiation across the two variants by group signature and discards one copy's
            // section, which that variant's (by then localized) code still points into.
            .arg("-Wl,--force-group-allocation")
            .arg("-o")
            .arg(&combined)
            .args(&objects)
            .status()
            .map_err(|e| format!("running {} -r: {e}", compiler.path().display()))?;
        if !status.success() {
            return Err(format!("partial link of the {variant} variant failed ({status})"));
        }

        // Only the bridge's variant-named entry points stay global. --localize-hidden alone isn't
        // enough: libstdc++ declares namespace std with default visibility, so template
        // instantiations like std::vector<...>::_M_realloc_insert stay global weak symbols in
        // both variants even under -fvisibility=hidden.
        let keep = out_dir.join(format!("htj2k_{variant}.keep"));
        let entry_points = ["decode", "decode_into", "free_image", "encode", "free_buffer"]
            .map(|name| format!("dcmnorm_htj2k_{variant}_{name}\n"))
            .concat();
        fs::write(&keep, entry_points).map_err(|e| format!("writing {}: {e}", keep.display()))?;
        let status = Command::new(&objcopy)
            .arg(format!("--keep-global-symbols={}", keep.display()))
            .arg(&combined)
            .status()
            .map_err(|e| format!("running {objcopy}: {e}"))?;
        if !status.success() {
            return Err(format!("{objcopy} --keep-global-symbols failed for the {variant} variant ({status})"));
        }
        variant_objects.push(combined);
    }

    let archive = out_dir.join("libdcmnorm_openhtj2k.a");
    let _ = fs::remove_file(&archive);
    let ar = env::var("AR").unwrap_or_else(|_| "ar".to_owned());
    let status = Command::new(&ar)
        .arg("crs")
        .arg(&archive)
        .args(&variant_objects)
        .status()
        .map_err(|e| format!("running {ar}: {e}"))?;
    if !status.success() {
        return Err(format!("{ar} failed ({status})"));
    }

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=dcmnorm_openhtj2k");
    Ok(())
}

fn link_runtime() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
    if target_os != "windows" {
        println!("cargo:rustc-link-lib=dylib=pthread");
    }
}
