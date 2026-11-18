#!/usr/bin/env bash
# Packages the Java API jar and one platform's natives jar, then loads the library from that jar
# the way a consumer does (nothing on java.library.path):
#
#   bindings/java/check-natives.sh <classifier> [<dcmtalk binary>]
#
# <classifier> is the platform, e.g. linux-x86_64; its library must already be staged in
# natives/target/natives/<classifier>/native/<classifier>/ (packaging/build-binding-linux.sh on
# Linux, the release workflow on macOS and Windows). Runs NativeLoaderTest (the extraction cache),
# LoadCheck (parse + an MPEG-2 round trip, which needs FFmpeg linked in) and, given a dcmtalk
# binary, the full Smoke test (which also needs the `ffmpeg` command for renderMovie). Needs a
# JDK 11+ and Maven. Works under Git Bash on Windows.
set -euo pipefail

CLASSIFIER="${1:?usage: $0 <classifier> [<dcmtalk binary>]}"
DCMTALK="${2:-}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES="$DIR/../../test/files"
SEP=":"
native_path() { echo "$1"; }
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    # Windows Java wants C:\... paths and ; between classpath entries.
    SEP=";"
    native_path() { cygpath -w "$1"; }
    ;;
esac

mvn -B -q -f "$DIR/pom.xml" package
mvn -B -q -f "$DIR/natives/pom.xml" package
# dcmnorm-<version>.jar, not its -sources/-javadoc jars (a version always ends in a digit).
API_JAR="$(echo "$DIR"/target/dcmnorm-*[0-9].jar)"
NATIVES_JAR="$(echo "$DIR"/natives/target/dcmnorm-natives-*-"$CLASSIFIER".jar)"
for jar in "$API_JAR" "$NATIVES_JAR"; do
  [ -f "$jar" ] || { echo "not built: $jar" >&2; exit 1; }
done
for jar in "$API_JAR" "$NATIVES_JAR"; do
  echo "== $(basename "$jar")"
  jar tf "$jar" | grep -v '/$'
done
if jar tf "$API_JAR" | grep -q '^native/'; then
  echo "the API jar must not contain native libraries" >&2
  exit 1
fi

API_JAR="$(native_path "$API_JAR")"
NATIVES_JAR="$(native_path "$NATIVES_JAR")"
FIXTURES="$(native_path "$FIXTURES")"
mkdir -p "$DIR/target/test-classes"
# shellcheck disable=SC2046 # the test sources have no spaces in their paths
javac -d "$DIR/target/test-classes" -cp "$DIR/target/classes" $(find "$DIR/src/test/java" -name '*.java')
CP="$API_JAR$SEP$(native_path "$DIR/target/test-classes")"
java -cp "$CP" com.pohcee.dcmnorm.internal.NativeLoaderTest
# A fresh cache directory, so the library really is extracted from this jar.
CACHE="$(mktemp -d)"
java -Ddcmnorm.cache.dir="$(native_path "$CACHE")" -cp "$CP$SEP$NATIVES_JAR" com.pohcee.dcmnorm.LoadCheck "$FIXTURES"
if [ -n "$DCMTALK" ]; then
  java -Ddcmnorm.cache.dir="$(native_path "$CACHE")" -cp "$CP$SEP$NATIVES_JAR" com.pohcee.dcmnorm.Smoke "$FIXTURES" "$DCMTALK"
fi
echo "extracted: $(find "$CACHE" -type f)"
rm -rf "$CACHE"
