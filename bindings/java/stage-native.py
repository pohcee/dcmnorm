#!/usr/bin/env python3
"""Stages a built native library where natives/pom.xml packages it, with its SHA-256 sidecar.

    bindings/java/stage-native.py <classifier> <library>
    bindings/java/stage-native.py osx-aarch_64 target/release/libdcmnorm_java.dylib

Writes natives/target/natives/<classifier>/native/<classifier>/<library name> and
<library name>.sha256 (`sha256sum` format; NativeLoader keys its extraction cache by it). The
Linux builds stage theirs inside the container (packaging/build-binding-linux.sh); the release
workflow uses this on macOS and Windows. Standard library only.
"""

import hashlib
import shutil
import sys
from pathlib import Path

NATIVES = Path(__file__).resolve().parent / "natives" / "target" / "natives"
EXPECTED = {"linux": "libdcmnorm_java.so", "osx": "libdcmnorm_java.dylib", "windows": "dcmnorm_java.dll"}


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    classifier, library = sys.argv[1], Path(sys.argv[2])
    expected = EXPECTED.get(classifier.split("-")[0])
    if expected != library.name:
        sys.exit(f"{classifier} needs {expected}, not {library.name}")
    out = NATIVES / classifier / "native" / classifier
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    shutil.copyfile(library, out / library.name)
    digest = hashlib.sha256((out / library.name).read_bytes()).hexdigest()
    (out / f"{library.name}.sha256").write_text(f"{digest}  {library.name}\n", encoding="ascii")
    print(out / library.name)


if __name__ == "__main__":
    main()
