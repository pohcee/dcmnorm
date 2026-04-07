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

if kakadu_include_dir="$(find_kakadu_include_dir)" && kakadu_lib_dir="$(find_kakadu_lib_dir)"; then
    use_kakadu_ffi=1
    export KAKADU_INCLUDE_DIR="$kakadu_include_dir"
    export KAKADU_LIB_DIR="$kakadu_lib_dir"
    install_args+=(--features dcmnorm/kakadu-ffi)
    echo "Detected Kakadu headers at $KAKADU_INCLUDE_DIR"
    echo "Detected Kakadu libraries at $KAKADU_LIB_DIR"
    echo "Installing exec crates with feature: dcmnorm/kakadu-ffi"
else
    echo "Kakadu headers/libs not detected; installing exec crates without kakadu-ffi"
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
