#!/bin/bash
# Builds the release .node binary inside a node:22-bullseye-slim container (Debian 11:
# glibc 2.31, GCC 10 -> GLIBCXX <= 3.4.28) so it's linked against an OLD glibc, not the
# host's - the compiled addon is committed to this repo (see .gitignore) and consumed by
# service Docker builds that have no Rust toolchain of their own (bookworm-based images,
# glibc 2.36) AND by the native Linux edge install (no Docker) on any host with
# glibc >= 2.31 (Ubuntu 20.04+, Debian 11+). A binary linked against a newer glibc risks a
# `GLIBC_X.XX not found` failure that only surfaces once deployed. A hard symbol-version
# check below fails the build if the floor is ever raised by accident.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

docker run --rm -v "$SCRIPT_DIR/../..":/repo -w /repo/bindings/node \
  -e HOST_UID="$HOST_UID" -e HOST_GID="$HOST_GID" \
  node:22-bullseye-slim bash -c '
    set -euo pipefail
    # Debian 11 is EOL and its security pool is gone from deb.debian.org (404s), so pin apt to
    # the snapshot.debian.org archive (the same dates the image ships commented out) - this is
    # also reproducible. Release files are old, hence Check-Valid-Until=false.
    printf "%s\n" \
      "deb http://snapshot.debian.org/archive/debian/20260824T000000Z bullseye main" \
      "deb http://snapshot.debian.org/archive/debian-security/20260824T000000Z bullseye-security main" \
      "deb http://snapshot.debian.org/archive/debian/20260824T000000Z bullseye-updates main" \
      > /etc/apt/sources.list
    echo "Acquire::Check-Valid-Until false;" > /etc/apt/apt.conf.d/99snapshot
    echo "Acquire::Retries 5;" >> /etc/apt/apt.conf.d/99snapshot
    apt-get update -qq
    apt-get install -y -qq curl ca-certificates build-essential clang pkg-config libclang-dev \
      python3-pip > /dev/null
    # bullseye ships cmake 3.18, older than some C deps (CharLS/jpeg-xl) want, so take a current
    # one from pip instead of raising the glibc floor by building on a newer distro.
    pip3 install --quiet "cmake>=3.27,<4"
    cmake --version | head -1

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

    curl --proto "=https" --tlsv1.2 -sSf --retry 5 --retry-all-errors https://sh.rustup.rs | sh -s -- -y -q
    . "$HOME/.cargo/env"
    npm install --no-save --no-audit --no-fund @napi-rs/cli
    # Dedicated target dir: cargo fingerprints do not track the system glibc, so reusing a
    # target/ populated by host or bookworm builds could silently link objects built against
    # a newer glibc (and mixed-toolchain caches broke builds with E0463).
    export CARGO_TARGET_DIR=/repo/target/node-bullseye
    npx napi build --platform --release "${NAPI_FEATURES[@]}"

    # Run the smoke test here, inside the exact container the addon was just built for,
    # rather than leaving it to release-it before:init hook to run on the host afterward
    # (package.json only runs `npm run build:docker` there now). A host run would dlopen-fail
    # for a reason that has nothing to do with the addon itself: the host system FFmpeg
    # SONAMEs (e.g. Ubuntu libavutil.so.58) do not match whatever this bullseye-based
    # container linked against, even though both are perfectly valid FFmpeg installs.
    # FFmpeg is statically linked, so the only runtime requirement is glibc/libstdc++:
    # the addon runs natively on hosts with glibc >= 2.31 and in the bookworm-based service images.
    node test/smoke.js

    # Hard glibc floor check: fail if the addon references any GLIBC_ symbol version above 2.31
    # or GLIBCXX_ above 3.4.28 (Ubuntu 20.04 libstdc++). Run on the binary just built.
    ADDON=dcmnorm-node.linux-x64-gnu.node
    SYMS="$(objdump -T "$ADDON")"
    MAX_GLIBC="$(echo "$SYMS" | grep -oE "GLIBC_[0-9.]+" | sed s/GLIBC_// | sort -Vu | tail -1)"
    MAX_GLIBCXX="$(echo "$SYMS" | grep -oE "GLIBCXX_[0-9.]+" | sed s/GLIBCXX_// | sort -Vu | tail -1)"
    echo "symbol floor: GLIBC_${MAX_GLIBC:-none} GLIBCXX_${MAX_GLIBCXX:-none}"
    if [ -n "$MAX_GLIBC" ] && [ "$(printf "%s\n%s\n" "$MAX_GLIBC" 2.31 | sort -V | tail -1)" != "2.31" ]; then
      echo "ERROR: addon needs GLIBC_$MAX_GLIBC (> 2.31)" >&2; exit 1
    fi
    if [ -n "$MAX_GLIBCXX" ] && [ "$(printf "%s\n%s\n" "$MAX_GLIBCXX" 3.4.28 | sort -V | tail -1)" != "3.4.28" ]; then
      echo "ERROR: addon needs GLIBCXX_$MAX_GLIBCXX (> 3.4.28)" >&2; exit 1
    fi

    # The container runs as root, so everything it touched on the volume mount -
    # the .node output here *and* the shared workspace target/ dir cargo writes
    # into - would otherwise be left root-owned on the host. Hand it all back to
    # whoever invoked this script (a stray root-owned target/ file breaks the
    # next host-side `cargo build` with a permission error).
    chown "$HOST_UID:$HOST_GID" *.node
    chown -R "$HOST_UID:$HOST_GID" /repo/target
  '
