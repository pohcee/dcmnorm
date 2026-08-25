#!/bin/bash
# Builds the release wheel inside a python:3.12-slim-bookworm container so it's linked against
# that image's glibc, not the host's - the compiled wheel is committed to this repo (see
# .gitignore) and consumed by service Docker builds that have no Rust toolchain of their own, so
# a host-built wheel risks a `GLIBC_X.XX not found` failure that only surfaces once deployed.
#
# Pinned explicitly to the "-bookworm" suffix, NOT the bare "python:3.12-slim" floating tag: the
# bare tag tracks whatever Debian release is current when it's pulled - it moved to trixie
# (glibc 2.41) after this was first written, silently producing a wheel newer deploy targets
# couldn't load. "-bookworm" pins to glibc 2.36, matching the Node bindings' own node:22-slim
# build container (also bookworm - see bindings/node/build-in-docker.sh), so both bindings' native
# code stays safe on the same deploy targets. Confirm with `docker run --rm <tag> ldd --version`
# before ever changing this tag.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

docker run --rm -v "$SCRIPT_DIR/../..":/repo -w /repo/bindings/python \
  -e HOST_UID="$HOST_UID" -e HOST_GID="$HOST_GID" \
  python:3.12-slim-bookworm bash -c '
    set -euo pipefail
    apt-get update -qq
    apt-get install -y -qq curl build-essential clang cmake pkg-config libclang-dev \
      libavutil-dev libavcodec-dev libavformat-dev libswscale-dev libswresample-dev \
      > /dev/null
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q
    . "$HOME/.cargo/env"
    # /repo/target is a shared workspace-level cache mounted straight from the host, so a stale
    # release/ dir from a build against a DIFFERENT container (e.g. this script pinned to the
    # wrong base image on a previous run, or a plain host `cargo build`) leaves binary-compatible-
    # looking .rlib/.so artifacts around that cargo will happily relink against instead of
    # recompiling - silently producing a wheel linked against THAT run'"'"'s glibc instead of this
    # one'"'"'s, with no error anywhere in this script. Force a clean release build every time so the
    # wheel this script just produced is always actually linked against THIS container'"'"'s glibc -
    # confirmed necessary empirically (this bit us once: a same-day rebuild after retagging the
    # base image below from a floating tag to "-bookworm" still came out linked against the
    # floating tag'"'"'s newer glibc, because nothing here forced a recompile).
    cargo clean --release --manifest-path /repo/Cargo.toml
    python -m venv /tmp/build-venv
    . /tmp/build-venv/bin/activate
    pip install --quiet --upgrade pip maturin patchelf
    maturin build --release --out dist
    deactivate
    # The container runs as root, so everything it touched on the volume mount - dist/ here and
    # the shared workspace target/ dir cargo writes into - would otherwise be left root-owned on
    # the host. Hand it all back to whoever invoked this script.
    chown -R "$HOST_UID:$HOST_GID" dist
    chown -R "$HOST_UID:$HOST_GID" /repo/target
  '
