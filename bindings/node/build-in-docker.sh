#!/bin/bash
# Builds the release .node binary inside a node:22-slim container so it's
# linked against that image's glibc, not the host's - the compiled addon is
# committed to this repo (see .gitignore) and consumed by service Docker
# builds that have no Rust toolchain of their own, so a host-built binary
# risks a `GLIBC_X.XX not found` failure that only surfaces once deployed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

docker run --rm -v "$SCRIPT_DIR/../..":/repo -w /repo/bindings/node \
  -e HOST_UID="$HOST_UID" -e HOST_GID="$HOST_GID" \
  node:22-slim bash -c '
    set -euo pipefail
    apt-get update -qq
    apt-get install -y -qq curl build-essential clang cmake pkg-config libclang-dev \
      libavutil-dev libavcodec-dev libavformat-dev libswscale-dev libswresample-dev \
      > /dev/null
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q
    . "$HOME/.cargo/env"
    npm install --no-save --no-audit --no-fund @napi-rs/cli
    npx napi build --platform --release
    # The container runs as root, so everything it touched on the volume mount -
    # the .node output here *and* the shared workspace target/ dir cargo writes
    # into - would otherwise be left root-owned on the host. Hand it all back to
    # whoever invoked this script (a stray root-owned target/ file breaks the
    # next host-side `cargo build` with a permission error).
    chown "$HOST_UID:$HOST_GID" *.node
    chown -R "$HOST_UID:$HOST_GID" /repo/target
  '
