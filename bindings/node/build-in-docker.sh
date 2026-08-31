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
      > /dev/null

    # ffmpeg-codec (dcmnorm/Cargo.toml, on by default) is the one default feature that
    # dynamically links against system libraries rather than being self-contained, so it is
    # the one feature that can fail to build for reasons outside this repo (a Debian point
    # release renaming/dropping a libav* package, network flakiness, ...). Everything else in
    # `default` (jpeg-ls-codec, jpeg-xl-codec, jpeg2000-openjpeg-encode) is a plain Rust/static
    # dependency and always builds. Rather than let a missing ffmpeg-dev hard-fail the whole
    # release (set -e would abort the script here), probe for it and build without
    # ffmpeg-codec if unavailable - the resulting addon still works for everything else, and
    # dcmnorm already reports MPEG transfer syntaxes as unsupported ("no") in
    # `--list-transfer-syntaxes` and rejects them at runtime with a clear error
    # (src/dicom_io/mpeg.rs, src/dicom_io/io.rs cfg!(feature = "ffmpeg-codec") checks) rather
    # than silently misbehaving, so this is a real degrade-gracefully path, not a silent gap.
    NAPI_FEATURES=()
    if ! apt-get install -y -qq \
      libavutil-dev libavcodec-dev libavformat-dev libswscale-dev libswresample-dev \
      > /dev/null 2>&1; then
      echo "WARNING: ffmpeg dev libraries unavailable in this build container - building" >&2
      echo "WARNING: dcmnorm-node WITHOUT ffmpeg-codec. MPEG transfer syntaxes will report" >&2
      echo "WARNING: as unsupported (see --list-transfer-syntaxes) in this build." >&2
      NAPI_FEATURES=(--no-default-features --features "jpeg-ls-codec,jpeg-xl-codec,jpeg2000-openjpeg-encode")
    fi

    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q
    . "$HOME/.cargo/env"
    npm install --no-save --no-audit --no-fund @napi-rs/cli
    npx napi build --platform --release "${NAPI_FEATURES[@]}"

    # Run the smoke test here, inside the exact container the addon was just built for,
    # rather than leaving it to release-it before:init hook to run on the host afterward
    # (package.json only runs `npm run build:docker` there now). A host run would dlopen-fail
    # for a reason that has nothing to do with the addon itself: the host system FFmpeg
    # SONAMEs (e.g. Ubuntu libavutil.so.58) do not match whatever this bookworm-based
    # container linked against (libavutil.so.57), even though both are perfectly valid FFmpeg
    # installs - node:22-slim (this container, and every real consumer Docker image) is
    # the only environment that actually needs to agree with the built binary.
    node test/smoke.js

    # The container runs as root, so everything it touched on the volume mount -
    # the .node output here *and* the shared workspace target/ dir cargo writes
    # into - would otherwise be left root-owned on the host. Hand it all back to
    # whoever invoked this script (a stray root-owned target/ file breaks the
    # next host-side `cargo build` with a permission error).
    chown "$HOST_UID:$HOST_GID" *.node
    chown -R "$HOST_UID:$HOST_GID" /repo/target
  '
