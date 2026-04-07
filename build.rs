use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=KAKADU_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=KAKADU_LIB_DIR");
    println!("cargo:rerun-if-env-changed=KAKADU_LIB_NAME");
    println!("cargo:rerun-if-env-changed=LD_LIBRARY_PATH");

    if env::var_os("CARGO_FEATURE_KAKADU_FFI").is_none() {
        return;
    }

    let include_dirs = find_include_dirs();
    let lib_dir = find_lib_dir();
    let lib_name = find_lib_name(&lib_dir);

    let mut build = cc::Build::new();
    build.cpp(true);
    build.file("src/dicom_io/kakadu_bridge.cpp");
    build.flag_if_supported("-std=c++14");
    build.flag_if_supported("-fPIC");
    for dir in &include_dirs {
        // Use -isystem instead of -I so GCC/Clang treats Kakadu headers as
        // system headers and suppresses warnings that originate inside them.
        build.flag_if_supported(&format!("-isystem{}", dir.display()));
    }
    build.compile("dcmnorm_kakadu_bridge");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib={lib_name}");
    println!("cargo:rustc-link-lib=dylib=stdc++");
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = home_dir() {
        roots.push(home.join(".local"));
    }
    roots.push(PathBuf::from("/usr/local"));
    roots.push(PathBuf::from("/usr"));
    roots.push(PathBuf::from("/opt/local"));
    roots
}

fn has_flat_headers(dir: &Path) -> bool {
    dir.join("kdu_elementary.h").is_file()
        && dir.join("kdu_messaging.h").is_file()
        && dir.join("kdu_params.h").is_file()
        && dir.join("kdu_compressed.h").is_file()
        && dir.join("kdu_sample_processing.h").is_file()
        && dir.join("kdu_stripe_compressor.h").is_file()
        && dir.join("kdu_stripe_decompressor.h").is_file()
        && dir.join("kdu_file_io.h").is_file()
}

fn split_existing_paths_var(name: &str) -> Vec<PathBuf> {
    env::var_os(name)
        .map(|value| {
            env::split_paths(&value)
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn find_include_dirs() -> Vec<PathBuf> {
    if let Some(include_dir) = env::var_os("KAKADU_INCLUDE_DIR") {
        let dir = PathBuf::from(include_dir);
        if !has_flat_headers(&dir) {
            panic!(
                "KAKADU_INCLUDE_DIR={} does not contain the required Kakadu headers",
                dir.display()
            );
        }
        return vec![dir];
    }

    for dir in split_existing_paths_var("CPLUS_INCLUDE_PATH")
        .into_iter()
        .chain(split_existing_paths_var("CPATH"))
        .chain(split_existing_paths_var("C_INCLUDE_PATH"))
    {
        if has_flat_headers(&dir) {
            return vec![dir];
        }

        let kakadu = dir.join("kakadu");
        if has_flat_headers(&kakadu) {
            return vec![kakadu];
        }
    }

    for root in candidate_roots() {
        let include = root.join("include");
        if has_flat_headers(&include) {
            return vec![include];
        }

        let include_kakadu = root.join("include/kakadu");
        if has_flat_headers(&include_kakadu) {
            return vec![include_kakadu];
        }

        let local_include = root.join("local/include");
        if has_flat_headers(&local_include) {
            return vec![local_include];
        }

        let local_include_kakadu = root.join("local/include/kakadu");
        if has_flat_headers(&local_include_kakadu) {
            return vec![local_include_kakadu];
        }
    }

    panic!(
        "Could not locate Kakadu headers. Install the required headers into a standard include directory such as ~/.local/include/kakadu or /usr/local/include/kakadu, or set KAKADU_INCLUDE_DIR when building with feature 'kakadu-ffi'."
    );
}

fn split_paths_var(name: &str) -> Vec<PathBuf> {
    env::var_os(name)
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default()
}

fn find_lib_dir() -> PathBuf {
    if let Some(lib_dir) = env::var_os("KAKADU_LIB_DIR") {
        let dir = PathBuf::from(lib_dir);
        if find_matching_lib(&dir).is_none() {
            panic!(
                "KAKADU_LIB_DIR={} does not contain libkdu*.so",
                dir.display()
            );
        }
        return dir;
    }

    for dir in split_paths_var("LD_LIBRARY_PATH") {
        if find_matching_lib(&dir).is_some() {
            return dir;
        }
    }

    for root in candidate_roots() {
        for dir in [root.clone(), root.join("lib"), root.join("lib64"), root.join("bin")] {
            if find_matching_lib(&dir).is_some() {
                return dir;
            }
        }
    }

    panic!(
        "Could not locate libkdu*.so. Ensure the Kakadu shared library is in LD_LIBRARY_PATH or set KAKADU_LIB_DIR when building with feature 'kakadu-ffi'."
    );
}

fn find_matching_lib(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut matches = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("libkdu") && name.ends_with(".so"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.into_iter().next()
}

fn find_lib_name(lib_dir: &Path) -> String {
    if let Some(name) = env::var_os("KAKADU_LIB_NAME") {
        return name.to_string_lossy().to_string();
    }

    let path = find_matching_lib(lib_dir).expect("library should exist in located Kakadu lib dir");
    let file_name = path.file_name().and_then(|name| name.to_str()).expect("valid library file name");
    file_name
        .trim_start_matches("lib")
        .trim_end_matches(".so")
        .to_string()
}
