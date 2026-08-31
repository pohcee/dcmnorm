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

    # NOTE: this whole heredoc is inside a single-quoted bash -c argument at the call site
    # below - never use an apostrophe/single-quote character anywhere in these comments. One
    # slipping in silently closes that outer quote, and everything after becomes literal
    # host-shell syntax instead of container script text.
    #
    # ffmpeg-codec (dcmnorm/Cargo.toml, on by default) builds FFmpeg from source and statically
    # links it (ffmpeg-next/ffmpeg-sys-next "build" feature) rather than dynamically linking
    # whatever FFmpeg happens to be on the machine - a dynamically linked builds exact required
    # SONAMEs (e.g. libavutil.so.57) only match a different machine (a dev host, or production
    # container) by coincidence, which is exactly what broke here once already: a bookworm
    # container built dcmnorm-node addon dlopen-failed on an Ubuntu dev host, and would have in
    # production too, since Docker.tmpl final image never installed matching runtime libs.
    # Static linking needs git (ffmpeg-sys-next shallow-clones FFmpeg source) and nasm (x86 asm
    # optimizations - FFmpeg own ./configure hard-fails without it on x86_64) in addition to the
    # compiler/pkg-config already installed above. Everything else in default (jpeg-ls-codec,
    # jpeg-xl-codec, jpeg2000-openjpeg-encode) is a plain Rust/static dependency and always
    # builds regardless. Rather than let git/nasm being unavailable (or a future Debian point
    # release dropping/renaming either package) hard-fail the whole release (set -e would abort
    # the script here), probe for them and build without ffmpeg-codec if unavailable - the
    # resulting addon still works for everything else, and dcmnorm already reports MPEG transfer
    # syntaxes as unsupported ("no") in --list-transfer-syntaxes and rejects them at runtime
    # with a clear error (src/dicom_io/mpeg.rs, src/dicom_io/io.rs cfg feature ffmpeg-codec
    # checks) rather than silently misbehaving, so this is a real degrade gracefully path,
    # not a silent gap.
    NAPI_FEATURES=()
    if apt-get install -y -qq git nasm > /dev/null 2>&1; then
      : # ffmpeg-codec stays in the default feature set
    else
      echo "WARNING: git/nasm unavailable in this build container - building dcmnorm-node" >&2
      echo "WARNING: WITHOUT ffmpeg-codec. MPEG transfer syntaxes will report as unsupported" >&2
      echo "WARNING: (see --list-transfer-syntaxes) in this build." >&2
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
