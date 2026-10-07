#!/bin/bash
# Local development build: cargo builds the native library (src/lib.rs), then Maven compiles/
# packages the Java side (src/main/java). For a release artifact linked against a specific
# deploy target's glibc, use build-in-docker.sh instead (see that script, and the "Packaging"
# section of this directory's README) - this script is for fast local iteration only, the same
# role bindings/node's `npm run build:debug` and bindings/python's `maturin develop` play for
# their own bindings.
#
# Usage: ./build.sh [--release] [test]
#   --release   cargo builds in release mode (default: debug, faster to iterate)
#   test        also compiles src/test/java/.../Smoke.java and runs it against test/files and a
#               freshly-built dcmtalk (see exec/dcmtalk) acting as the DIMSE SCP side - the Java
#               counterpart of `npm test` / `python test/smoke.py`
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PROFILE="debug"
CARGO_PROFILE_FLAGS=()
RUN_TEST=false

for arg in "$@"; do
  case "$arg" in
    --release)
      PROFILE="release"
      CARGO_PROFILE_FLAGS=(--release)
      ;;
    test)
      RUN_TEST=true
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 1
      ;;
  esac
done

echo "==> cargo build -p dcmnorm-java ${CARGO_PROFILE_FLAGS[*]:-}"
cargo build -p dcmnorm-java "${CARGO_PROFILE_FLAGS[@]}" --manifest-path "$REPO_ROOT/Cargo.toml"

NATIVE_LIB_DIR="$REPO_ROOT/target/$PROFILE"
if [ ! -f "$NATIVE_LIB_DIR/libdcmnorm_java.so" ]; then
  echo "expected $NATIVE_LIB_DIR/libdcmnorm_java.so, but it was not built" >&2
  exit 1
fi

echo "==> mvn -q package"
mvn -q -f "$SCRIPT_DIR/pom.xml" package

if [ "$RUN_TEST" = true ]; then
  echo "==> cargo build -p dcmtalk (the DIMSE SCP side used by Smoke.java)"
  cargo build -p dcmtalk "${CARGO_PROFILE_FLAGS[@]}" --manifest-path "$REPO_ROOT/Cargo.toml"

  echo "==> compiling src/test/java/.../Smoke.java"
  mkdir -p "$SCRIPT_DIR/target/test-classes"
  javac -d "$SCRIPT_DIR/target/test-classes" -cp "$SCRIPT_DIR/target/classes" \
    $(find "$SCRIPT_DIR/src/test/java" -name '*.java')

  echo "==> java com.pohcee.dcmnorm.Smoke"
  java -Djava.library.path="$NATIVE_LIB_DIR" \
    -cp "$SCRIPT_DIR/target/classes:$SCRIPT_DIR/target/test-classes" \
    com.pohcee.dcmnorm.Smoke "$REPO_ROOT/test/files" "$NATIVE_LIB_DIR/dcmtalk"
fi
