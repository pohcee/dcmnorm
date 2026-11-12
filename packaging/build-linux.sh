#!/usr/bin/env bash
# Builds the dcmnorm and dcmtalk release binaries for Linux inside a manylinux_2_28 container, so
# they run on any glibc 2.28+ distribution (Debian 10+, Ubuntu 20.04+, RHEL/Alma/Rocky 8+). The same
# binaries go into the release archives, the .deb packages and the PyPI wheels.
#
#   packaging/build-linux.sh [x86_64|aarch64]     (default: this machine's architecture)
#
# Binaries land in target/manylinux_2_28-<arch>/release/. Docker must be able to run the image
# for <arch> natively (GitHub's ubuntu-24.04 runner for x86_64, ubuntu-24.04-arm for aarch64).
# The script then checks the result: no glibc symbol newer than 2.28 and no shared library
# outside what the manylinux policy allows.
set -euo pipefail

ARCH="${1:-$(uname -m)}"
case "$ARCH" in
  x86_64|aarch64) ;;
  arm64) ARCH=aarch64 ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# A separate target dir: artifacts built against the host's newer glibc must never be reused.
TARGET_DIR="target/manylinux_2_28-${ARCH}"
IMAGE="quay.io/pypa/manylinux_2_28_${ARCH}"
# Created here, as the invoking user: if Docker created it for the bind mount, target/ itself
# would be root-owned and later host-side builds (cargo deb) couldn't write to it.
mkdir -p "$REPO_ROOT/$TARGET_DIR"

docker run --rm \
  -v "$REPO_ROOT":/repo -w /repo \
  -e CARGO_TARGET_DIR="/repo/${TARGET_DIR}" \
  -e CARGO_PROFILE_RELEASE_STRIP=symbols \
  -e CMAKE_TOOLCHAIN_FILE=/repo/packaging/manylinux.cmake \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  "$IMAGE" bash -euo pipefail -c '
    # The image has a C/C++ toolchain (gcc-toolset), cmake and binutils, which is everything the
    # default build needs (FFmpeg is not built, see Cargo.toml).
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal
    . "$HOME/.cargo/env"
    cargo build --release --locked -p dcmnorm-cli -p dcmtalk

    for bin in dcmnorm dcmtalk; do
      path="$CARGO_TARGET_DIR/release/$bin"
      newest=$(objdump -T "$path" | grep -o "GLIBC_[0-9.]*" | sed "s/GLIBC_//" | sort -uV | tail -1)
      echo "$bin: newest glibc symbol $newest"
      if [ "$(printf "%s\n2.28\n" "$newest" | sort -V | tail -1)" != "2.28" ]; then
        echo "$bin needs glibc $newest, newer than manylinux_2_28 allows" >&2
        exit 1
      fi
      # The manylinux policy libraries this binary may link (everything else must be static).
      extra=$(objdump -p "$path" | awk "/NEEDED/ {print \$2}" \
        | grep -v -E "^(libc|libm|libdl|libpthread|librt|libgcc_s|libstdc\+\+|ld-linux[-a-z0-9_]*)\.so" || true)
      if [ -n "$extra" ]; then
        echo "$bin links libraries outside the manylinux policy: $extra" >&2
        exit 1
      fi
      "$path" --version
    done
    chown -R "$HOST_UID:$HOST_GID" "$CARGO_TARGET_DIR"
  '

echo "Built ${TARGET_DIR}/release/{dcmnorm,dcmtalk}"
