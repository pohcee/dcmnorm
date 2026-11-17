#!/bin/bash
# Builds the Linux native library for this machine's architecture in a manylinux_2_28 container
# (glibc 2.28+, FFmpeg linked in statically) and stages it where natives/pom.xml packages it:
# natives/target/natives/linux-<arch>/native/linux-<arch>/libdcmnorm_java.so (+ .sha256). See
# packaging/build-binding-linux.sh. Then `./check-natives.sh linux-<arch> [<dcmtalk>]` packages
# the API and natives jars and runs the load checks against them.
set -euo pipefail
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/packaging/build-binding-linux.sh" java "$@"
