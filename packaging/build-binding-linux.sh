#!/usr/bin/env bash
# Builds the Python or Java binding's native code for Linux inside a manylinux_2_28 container, the
# same image packaging/build-linux.sh builds the CLIs in, so the result runs on any glibc 2.28+
# distribution (Debian 10+, Ubuntu 20.04+, RHEL/Alma/Rocky 8+).
#
#   packaging/build-binding-linux.sh python [x86_64|aarch64]
#       -> bindings/python/dist/dcmnorm-<v>-cp39-abi3-manylinux_2_28_<arch>.whl
#          Needs the CLI binaries in bindings/python/data/scripts/ first (packaging/stage_cli.py).
#   packaging/build-binding-linux.sh java [x86_64|aarch64]
#       -> bindings/java/natives/target/natives/<classifier>/native/<classifier>/libdcmnorm_java.so
#          (+ .sha256), the layout of the dcmnorm-java-natives jar (bindings/java/README.md);
#          <classifier> is linux-x86_64 or linux-aarch_64 (the os-maven-plugin names)
#
# FFmpeg is built from source and linked in statically (the ffmpeg-codec feature, which needs git
# and nasm), with the codec set every binding uses: the crates' default features. There is no
# fallback to a build without FFmpeg. Docker must run the image for <arch> natively (GitHub's
# ubuntu-24.04 runner for x86_64, ubuntu-24.04-arm for aarch64). The result is checked: no glibc
# symbol newer than 2.28 and no shared library outside the manylinux policy.
#
# The Node binding builds in node:22-bullseye-slim instead (bindings/node/build-in-docker.sh): napi
# needs Node itself in the container, and its glibc floor (2.31) predates this script.
set -euo pipefail

BINDING="${1:-}"
ARCH="${2:-$(uname -m)}"
case "$BINDING" in
  python|java) ;;
  *) echo "usage: $0 <python|java> [x86_64|aarch64]" >&2; exit 1 ;;
esac
case "$ARCH" in
  x86_64|aarch64) ;;
  arm64) ARCH=aarch64 ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# One target dir for both bindings: they build dcmnorm with the same features, so the FFmpeg
# source build (the slow part) is shared. Never the host's target/release: cargo does not track
# the system glibc, and objects built against a newer one must not be relinked here.
TARGET_DIR="target/manylinux_2_28-${ARCH}-bindings"
IMAGE="quay.io/pypa/manylinux_2_28_${ARCH}"

if [ "$BINDING" = python ]; then
  for bin in dcmnorm dcmtalk; do
    if [ ! -x "$REPO_ROOT/bindings/python/data/scripts/$bin" ]; then
      echo "missing bindings/python/data/scripts/$bin: stage the CLI binaries first" >&2
      echo "  (python3 packaging/stage_cli.py dist/dcmnorm-<v>-py3-none-manylinux_2_28_${ARCH}.whl)" >&2
      exit 1
    fi
  done
fi
# Created as the invoking user, so Docker doesn't create root-owned bind-mount directories.
mkdir -p "$REPO_ROOT/$TARGET_DIR"

docker run --rm \
  -v "$REPO_ROOT":/repo -w /repo \
  -e BINDING="$BINDING" -e ARCH="$ARCH" \
  -e CARGO_TARGET_DIR="/repo/${TARGET_DIR}" \
  -e CARGO_PROFILE_RELEASE_STRIP=symbols \
  -e CMAKE_TOOLCHAIN_FILE=/repo/packaging/manylinux.cmake \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  "$IMAGE" bash -euo pipefail -c '
    # Everything written under the bind mount is handed back to the invoking user, also when a
    # step fails: a root-owned path breaks the next host-side build or a runner workspace clean.
    hand_back() {
      local rc=$?
      if [ "$HOST_UID" != 0 ]; then
        find /repo/target /repo/bindings -xdev -user 0 -exec chown -h "$HOST_UID:$HOST_GID" {} + || true
      fi
      exit "$rc"
    }
    trap hand_back EXIT

    # nasm: FFmpeg x86 assembly (its ./configure fails without it on x86_64); clang-devel:
    # libclang for bindgen (ffmpeg-sys-next). git (FFmpeg source clone), the gcc toolset and
    # cmake come with the image.
    dnf install -y -q nasm clang-devel > /dev/null
    curl --proto "=https" --tlsv1.2 -sSf --retry 5 https://sh.rustup.rs \
      | sh -s -- -y -q --profile minimal
    . "$HOME/.cargo/env"
    # Compilers that drop -march=native: ffmpeg-sys-next would otherwise tune FFmpeg for the CPU
    # of this build machine (packaging/portable-cc/cc-filter).
    export PATH="/repo/packaging/portable-cc:$PATH"

    check_policy() {
      local path="$1" newest extra
      newest=$(objdump -T "$path" | grep -o "GLIBC_[0-9.]*" | sed "s/GLIBC_//" | sort -uV | tail -1)
      echo "$(basename "$path"): newest glibc symbol $newest"
      if [ "$(printf "%s\n2.28\n" "$newest" | sort -V | tail -1)" != "2.28" ]; then
        echo "$path needs glibc $newest, newer than manylinux_2_28 allows" >&2
        exit 1
      fi
      extra=$(objdump -p "$path" | awk "/NEEDED/ {print \$2}" \
        | grep -v -E "^(libc|libm|libdl|libpthread|librt|libgcc_s|libstdc\+\+|ld-linux[-a-z0-9_]*)\.so" || true)
      if [ -n "$extra" ]; then
        echo "$path links libraries outside the manylinux policy (FFmpeg must be static): $extra" >&2
        exit 1
      fi
    }

    if [ "$BINDING" = python ]; then
      /opt/python/cp312-cp312/bin/python -m venv /tmp/build-venv
      /tmp/build-venv/bin/pip install --quiet "maturin>=1.8,<2.0"
      mkdir -p bindings/python/dist && rm -f bindings/python/dist/*.whl
      # --compatibility manylinux_2_28: maturin fails rather than tagging the wheel with any other
      # policy, and checks (auditwheel-style) that nothing outside it is linked.
      /tmp/build-venv/bin/maturin build --release --locked \
        --manifest-path bindings/python/Cargo.toml \
        --compatibility manylinux_2_28 --out bindings/python/dist
      wheel=$(ls bindings/python/dist/*.whl)
      rm -rf /tmp/wheel && mkdir /tmp/wheel
      (cd /tmp/wheel && /tmp/build-venv/bin/python -m zipfile -e "/repo/$wheel" .)
      check_policy "$(ls /tmp/wheel/dcmnorm/*.so)"
      echo "built $wheel"
    else
      cargo build --release --locked -p dcmnorm-java
      lib="$CARGO_TARGET_DIR/release/libdcmnorm_java.so"
      check_policy "$lib"
      classifier="linux-${ARCH/aarch64/aarch_64}"
      out="bindings/java/natives/target/natives/$classifier/native/$classifier"
      rm -rf "$out" && mkdir -p "$out"
      cp "$lib" "$out/"
      # The sidecar hash keys NativeLoader s extraction cache (a JVM start reads 64 bytes instead
      # of hashing the library).
      (cd "$out" && sha256sum libdcmnorm_java.so > libdcmnorm_java.so.sha256)
      echo "built $out/libdcmnorm_java.so"
    fi
  '
