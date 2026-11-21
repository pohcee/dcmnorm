#!/usr/bin/env bash
# The checks of .github/workflows/lint.yml: rustfmt and clippy (-D warnings), on the same crates.
#   scripts/lint.sh          check only (what CI runs)
#   scripts/lint.sh --fix    apply cargo fmt and clippy's automatic fixes first, then check
#   scripts/lint.sh --fmt    rustfmt check only (fast; used by the pre-commit hook)
set -euo pipefail
cd "$(dirname "$0")/.."

exclude=(--exclude dcmnorm-node --exclude dcmnorm-python --exclude dcmnorm-java)

case "${1:-}" in
  --fmt)
    exec cargo fmt --all --check ;;
  --fix)
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged --release --locked --workspace --all-targets "${exclude[@]}"
    cargo fmt --all ;;
  "") ;;
  *) echo "usage: $0 [--fix|--fmt]" >&2; exit 2 ;;
esac

cargo fmt --all --check
cargo clippy --release --locked --workspace --all-targets "${exclude[@]}" -- -D warnings
