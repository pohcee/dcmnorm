#!/bin/bash
# Builds the release native library (and the jar bundling it) inside a debian:bookworm-slim
# container so it's linked against that image's glibc, not the host's - mirrors
# bindings/node/build-in-docker.sh and bindings/python/build-in-docker.sh, which build inside
# node:22-slim and python:3.12-slim-bookworm respectively for the exact same reason (both are
# also bookworm-based, so all three bindings' native code stays safe on the same deploy targets -
# see bindings/python/build-in-docker.sh's own comment on why the "-bookworm" pin matters and
# must not be swapped for a floating tag). The resulting libdcmnorm_java.so is bundled into the
# jar at src/main/resources/native/linux-x64/ and committed to this repo (see .gitignore) rather
# than built at a consumer's Docker-image time, since a consumer's builder stage may have no Rust
# toolchain of its own - the same reasoning the other two bindings' own scripts give for
# committing their own prebuilt artifact.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

docker run --rm -v "$SCRIPT_DIR/../..":/repo -w /repo/bindings/java \
  -e HOST_UID="$HOST_UID" -e HOST_GID="$HOST_GID" \
  debian:bookworm-slim bash -c '
    set -euo pipefail
    apt-get update -qq
    apt-get install -y -qq curl build-essential clang cmake pkg-config libclang-dev \
      default-jdk-headless maven \
      > /dev/null

    # NOTE: this whole heredoc is inside a single-quoted bash -c argument at the call site below -
    # never use an apostrophe/single-quote character anywhere in these comments, the same
    # constraint bindings/node/build-in-docker.sh calls out for its own copy of this heredoc.
    #
    # Same ffmpeg-codec probe as bindings/node/build-in-docker.sh - ffmpeg-codec (on by default)
    # builds FFmpeg from source and statically links it, which needs git (shallow-clones FFmpeg
    # source) and nasm (x86 asm optimizations) in addition to the compiler/pkg-config already
    # installed above. Rather than let git/nasm being unavailable hard-fail the whole release,
    # probe for them and build without ffmpeg-codec if unavailable - the resulting library still
    # works for everything else, and dcmnorm already reports MPEG transfer syntaxes as
    # unsupported at runtime rather than silently misbehaving.
    CARGO_FEATURES=()
    if apt-get install -y -qq git nasm > /dev/null 2>&1; then
      : # ffmpeg-codec stays in the default feature set
    else
      echo "WARNING: git/nasm unavailable in this build container - building dcmnorm-java" >&2
      echo "WARNING: WITHOUT ffmpeg-codec. MPEG transfer syntaxes will report as unsupported" >&2
      echo "WARNING: (see --list-transfer-syntaxes) in this build." >&2
      CARGO_FEATURES=(--no-default-features --features jpeg-ls-codec,jpeg-xl-codec,jpeg2000-openjpeg-encode)
    fi

    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q
    . "$HOME/.cargo/env"
    # Same "force a clean release build" reasoning as bindings/python/build-in-docker.sh - the
    # shared workspace target/ dir mounted from the host can otherwise hold binary-compatible-
    # looking artifacts from a different build (a different container, or a plain host build),
    # which cargo would happily relink against instead of recompiling.
    cargo clean --release --manifest-path /repo/Cargo.toml
    cargo build --release -p dcmnorm-java "${CARGO_FEATURES[@]}" --manifest-path /repo/Cargo.toml
    cargo build --release -p dcmtalk --manifest-path /repo/Cargo.toml

    mkdir -p src/main/resources/native/linux-x64
    cp /repo/target/release/libdcmnorm_java.so src/main/resources/native/linux-x64/

    mvn -q package

    # Run the smoke test here, inside the exact container the library was just built for -
    # mirrors both other bindings running their own smoke test inside their build container
    # rather than on the host afterward, for the same glibc-mismatch reason.
    mkdir -p target/test-classes
    javac -d target/test-classes -cp target/classes $(find src/test/java -name "*.java")
    java -Djava.library.path=/repo/target/release \
      -cp target/classes:target/test-classes \
      com.pohcee.dcmnorm.Smoke /repo/test/files /repo/target/release/dcmtalk

    # The container runs as root, so everything it touched on the volume mount - this directory
    # and the shared workspace target/ dir cargo writes into - would otherwise be left root-owned
    # on the host.
    chown -R "$HOST_UID:$HOST_GID" src/main/resources/native target
    chown -R "$HOST_UID:$HOST_GID" /repo/target
  '
