#!/usr/bin/env python3
"""Copies the CLI binaries out of a CLI-only wheel into the Python binding's maturin data dir.

    packaging/stage_cli.py dist/dcmnorm-0.11.2-py3-none-manylinux_2_28_x86_64.whl

The `dcmnorm` PyPI release has two wheels per platform: the CLI-only one packaging/package.py
writes (py3-none) and the one maturin builds from bindings/python (cp39-abi3), which holds the
`import dcmnorm` extension module and the same CLIs. This puts the CLI wheel's
`dcmnorm-<v>.data/scripts/*` into bindings/python/data/scripts/ (cleared first) with their
executable bit, so maturin packs the very same binaries, and copies the repository LICENSE to
bindings/python/LICENSE (pyproject.toml's license-files). Standard library only.
"""

import shutil
import stat
import sys
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEST = REPO_ROOT / "bindings" / "python" / "data" / "scripts"


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    wheel = Path(sys.argv[1])
    if DEST.exists():
        shutil.rmtree(DEST)
    DEST.mkdir(parents=True)
    staged = []
    with zipfile.ZipFile(wheel) as archive:
        for info in archive.infolist():
            parts = info.filename.split("/")
            if len(parts) == 3 and parts[0].endswith(".data") and parts[1] == "scripts" and parts[2]:
                target = DEST / parts[2]
                target.write_bytes(archive.read(info))
                target.chmod(target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
                staged.append(parts[2])
    names = {name.removesuffix(".exe") for name in staged}
    if names != {"dcmnorm", "dcmtalk"}:
        sys.exit(f"{wheel.name}: expected dcmnorm and dcmtalk in .data/scripts, found {sorted(staged)}")
    shutil.copyfile(REPO_ROOT / "LICENSE", REPO_ROOT / "bindings" / "python" / "LICENSE")
    for name in sorted(staged):
        print(DEST / name)


if __name__ == "__main__":
    main()
