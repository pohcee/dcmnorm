#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"

if [[ ! -d "$repo_root/exec" ]]; then
    echo "No exec directory found at $repo_root/exec" >&2
    exit 1
fi

found_any=0

while IFS= read -r manifest; do
    found_any=1
    package_dir="$(dirname "$manifest")"
    echo "Installing crate from $package_dir"
    cargo install --path "$package_dir"
done < <(find "$repo_root/exec" -mindepth 2 -maxdepth 2 -name Cargo.toml | sort)

if [[ "$found_any" -eq 0 ]]; then
    echo "No installable crates found under $repo_root/exec" >&2
    exit 1
fi
