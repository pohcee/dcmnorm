#!/bin/bash
# Builds the release .node addon for this machine's architecture (x64 or arm64) inside a
# node:22-bullseye-slim container (Debian 11: glibc 2.31, GCC 10 -> GLIBCXX <= 3.4.28), so it's
# linked against an OLD glibc, not the host's. The addon is published to npm as
# @dcmnorm/linux-<arch>-gnu and runs on any host with glibc >= 2.31 (Ubuntu 20.04+,
# Debian 11+, and the bookworm-based Node images). A binary linked against a newer glibc risks a
# `GLIBC_X.XX not found` failure that only surfaces once deployed, so a hard symbol-version check
# below fails the build if the floor is ever raised by accident. Docker must run the image
# natively (GitHub's ubuntu-24.04 runner for x64, ubuntu-24.04-arm for arm64).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

docker run --rm -v "$SCRIPT_DIR/../..":/repo -w /repo/bindings/node \
  -e HOST_UID="$HOST_UID" -e HOST_GID="$HOST_GID" \
  node:22-bullseye-slim bash -c '
    set -euo pipefail
    # The container runs as root, so everything it writes on the volume mount (node_modules
    # from the npm install below, the .node output, the generated index.js/index.d.ts, the
    # shared workspace target/ dir cargo writes into, a package-lock.json) would otherwise be
    # left root-owned on the host. Hand back every root-owned path under /repo to whoever
    # invoked this script, from an EXIT trap so it also runs when a step below fails. A stray
    # root-owned path breaks the next host-side cargo build, npm install or clean -fdx of the
    # checkout (a CI runner without sudo cannot remove it at all).
    hand_back() {
      local rc=$?
      if [ "$HOST_UID" != "0" ]; then
        find /repo -xdev -user 0 -exec chown -h "$HOST_UID:$HOST_GID" {} + || true
      fi
      exit "$rc"
    }
    trap hand_back EXIT
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
    # ffmpeg-codec (on by default) builds FFmpeg from source and links it statically
    # (ffmpeg-next/ffmpeg-sys-next "build" feature) rather than dynamically linking whatever FFmpeg
    # is on the build machine, whose SONAMEs (e.g. libavutil.so.57) only match another machine by
    # coincidence. The source build needs git (shallow-clones FFmpeg) and nasm (x86 assembly;
    # FFmpeg own ./configure fails without it on x86_64). There is deliberately no fallback to a
    # build without FFmpeg: a missing package fails the build, as it does for the other bindings.
    apt-get install -y -qq git nasm > /dev/null

    curl --proto "=https" --tlsv1.2 -sSf --retry 5 --retry-all-errors https://sh.rustup.rs | sh -s -- -y -q
    . "$HOME/.cargo/env"
    # Compilers that drop -march=native: ffmpeg-sys-next would otherwise tune FFmpeg for the CPU
    # of this build machine (packaging/portable-cc/cc-filter).
    export PATH="/repo/packaging/portable-cc:$PATH"
    npm install --no-save --no-audit --no-fund @napi-rs/cli
    # Dedicated target dir: cargo fingerprints do not track the system glibc, so reusing a
    # target/ populated by host or bookworm builds could silently link objects built against
    # a newer glibc (and mixed-toolchain caches broke builds with E0463).
    export CARGO_TARGET_DIR="/repo/target/node-bullseye-$(uname -m)"
    npx napi build --platform --release
    # napi names the platform packages dcmnorm-<platform>; the loader requires @dcmnorm/<platform>.
    node prepare-npm.js --fix-loader
    # FFmpeg must be in the addon: no libav*/libsw* shared-library dependency.
    if objdump -p dcmnorm-node.linux-*-gnu.node | grep -E "NEEDED +lib(av|sw|postproc)"; then
      echo "ERROR: addon links FFmpeg dynamically" >&2; exit 1
    fi

    # Run the smoke test here, inside the container the addon was just built in, rather than on
    # the host. FFmpeg is statically linked, so the only runtime requirement is glibc/libstdc++;
    # load-check.js also round-trips a file through MPEG-2, which fails without FFmpeg linked in.
    node test/smoke.js
    node test/registration.js
    node load-check.js

    # Hard glibc floor check: fail if the addon references any GLIBC_ symbol version above 2.31
    # or GLIBCXX_ above 3.4.28 (Ubuntu 20.04 libstdc++). Run on the binary just built.
    case "$(uname -m)" in
      x86_64) ADDON=dcmnorm-node.linux-x64-gnu.node ;;
      aarch64) ADDON=dcmnorm-node.linux-arm64-gnu.node ;;
      *) echo "ERROR: unsupported architecture $(uname -m)" >&2; exit 1 ;;
    esac
    test -f "$ADDON"
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

    # Ownership of everything written under /repo is handed back by the EXIT trap at the top.
  '
