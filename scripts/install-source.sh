#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"

if [[ ! -d "$repo_root/exec" ]]; then
    echo "No exec directory found at $repo_root/exec" >&2
    exit 1
fi

has_flat_headers() {
    local dir="$1"
    [[ -f "$dir/kdu_elementary.h" ]] &&
    [[ -f "$dir/kdu_messaging.h" ]] &&
    [[ -f "$dir/kdu_params.h" ]] &&
    [[ -f "$dir/kdu_compressed.h" ]] &&
    [[ -f "$dir/kdu_sample_processing.h" ]] &&
    [[ -f "$dir/kdu_stripe_compressor.h" ]] &&
    [[ -f "$dir/kdu_stripe_decompressor.h" ]] &&
    [[ -f "$dir/kdu_file_io.h" ]]
}

split_paths_var() {
    local var_name="$1"
    local value="${!var_name:-}"
    if [[ -z "$value" ]]; then
        return 0
    fi
    tr ':' '\n' <<< "$value"
}

require_command() {
    local cmd="$1"
    local hint="$2"

    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        echo "$hint" >&2
        exit 1
    fi
}

check_apt_build_deps_for_all_features() {
    # Apt package checks are only relevant on Debian/Ubuntu style systems.
    if ! command -v dpkg-query >/dev/null 2>&1; then
        return 0
    fi

    local -a required_packages=(
        build-essential
        ca-certificates
        clang
        cmake
        libc6-dev
        libavcodec-dev
        libavformat-dev
        libavutil-dev
        libclang-dev
        libswresample-dev
        libswscale-dev
        pkg-config
    )
    local -a missing_packages=()
    local pkg

    for pkg in "${required_packages[@]}"; do
        if ! dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q '^install ok installed$'; then
            missing_packages+=("$pkg")
        fi
    done

    if [[ "${#missing_packages[@]}" -gt 0 ]]; then
        echo "Missing apt packages required for full-feature exec builds:" >&2
        printf '  - %s\n' "${missing_packages[@]}" >&2
        echo "Install them with: sudo apt-get update && sudo apt-get install -y ${missing_packages[*]}" >&2
        exit 1
    fi
}

check_clang_can_find_std_headers() {
    local tmp_source
    tmp_source="$(mktemp)"
    printf '#include <limits.h>\n' > "$tmp_source"

    if ! clang -fsyntax-only -x c "$tmp_source" >/dev/null 2>&1; then
        rm -f "$tmp_source"
        echo "Clang is installed but cannot find standard C headers (for example limits.h)." >&2
        echo "Install the system C toolchain headers, such as build-essential/libc6-dev on Debian or Ubuntu, so bindgen can compile FFmpeg bindings." >&2
        exit 1
    fi

    rm -f "$tmp_source"
}

check_ffmpeg_dev_pkg() {
    local pkg="$1"

    if ! pkg-config --exists "$pkg"; then
        echo "Missing FFmpeg development package: $pkg" >&2
        echo "Default builds now enable dcmnorm/ffmpeg-codec. Install FFmpeg development headers and pkg-config, or build with --no-default-features if you need a reduced build." >&2
        exit 1
    fi
}

find_kakadu_include_dir() {
    if [[ -n "${KAKADU_INCLUDE_DIR:-}" ]] && has_flat_headers "$KAKADU_INCLUDE_DIR"; then
        echo "$KAKADU_INCLUDE_DIR"
        return 0
    fi

    local dir
    while IFS= read -r dir; do
        [[ -z "$dir" ]] && continue
        [[ -d "$dir" ]] || continue
        if has_flat_headers "$dir"; then
            echo "$dir"
            return 0
        fi
        if has_flat_headers "$dir/kakadu"; then
            echo "$dir/kakadu"
            return 0
        fi
    done < <(
        split_paths_var CPLUS_INCLUDE_PATH
        split_paths_var CPATH
        split_paths_var C_INCLUDE_PATH
        printf '%s\n' "$HOME/.local/include" "/usr/local/include" "/usr/include" "/opt/local/include"
    )

    return 1
}

find_kakadu_lib_dir() {
    if [[ -n "${KAKADU_LIB_DIR:-}" ]] && ls "$KAKADU_LIB_DIR"/libkdu*.so >/dev/null 2>&1; then
        echo "$KAKADU_LIB_DIR"
        return 0
    fi

    local dir
    while IFS= read -r dir; do
        [[ -z "$dir" ]] && continue
        [[ -d "$dir" ]] || continue
        if ls "$dir"/libkdu*.so >/dev/null 2>&1; then
            echo "$dir"
            return 0
        fi
    done < <(
        split_paths_var LD_LIBRARY_PATH
        printf '%s\n' \
            "$HOME/.local/lib" \
            "$HOME/.local/lib64" \
            "/usr/local/lib" \
            "/usr/local/lib64" \
            "/usr/lib" \
            "/usr/lib64" \
            "/opt/local/lib" \
            "/opt/local/lib64"
    )

    return 1
}

use_kakadu_ffi=0
install_args=()

check_apt_build_deps_for_all_features
require_command cargo "Install Rust and Cargo before running this script."
require_command pkg-config "Install pkg-config so the default FFmpeg codec support can find system FFmpeg libraries."
require_command clang "Install clang so bindgen can generate FFmpeg bindings during the default build."
check_clang_can_find_std_headers
check_ffmpeg_dev_pkg libavutil
check_ffmpeg_dev_pkg libavcodec
check_ffmpeg_dev_pkg libavformat
check_ffmpeg_dev_pkg libswscale
check_ffmpeg_dev_pkg libswresample

if kakadu_include_dir="$(find_kakadu_include_dir)" && kakadu_lib_dir="$(find_kakadu_lib_dir)"; then
    use_kakadu_ffi=1
    export KAKADU_INCLUDE_DIR="$kakadu_include_dir"
    export KAKADU_LIB_DIR="$kakadu_lib_dir"
    install_args+=(--features kakadu-ffi)
    echo "Detected Kakadu headers at $KAKADU_INCLUDE_DIR"
    echo "Detected Kakadu libraries at $KAKADU_LIB_DIR"
    echo "Installing exec crates with default codec features plus: kakadu-ffi"
else
    echo "Kakadu headers/libs not detected; installing exec crates with default codec features only"
fi

found_any=0

while IFS= read -r manifest; do
    found_any=1
    package_dir="$(dirname "$manifest")"
    echo "Installing crate from $package_dir"
    cargo install --path "$package_dir" "${install_args[@]}"
done < <(find "$repo_root/exec" -mindepth 2 -maxdepth 2 -name Cargo.toml | sort)

if [[ "$found_any" -eq 0 ]]; then
    echo "No installable crates found under $repo_root/exec" >&2
    exit 1
fi
