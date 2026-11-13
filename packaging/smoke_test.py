#!/usr/bin/env python3
"""Installs a dcmnorm CLI wheel into a fresh virtual environment and runs the CLIs from it.

    packaging/smoke_test.py dist/dcmnorm-0.7.3-py3-none-manylinux_2_28_x86_64.whl

Checks that the installer put both binaries on the venv's PATH as executables, that they report
the wheel's version, and that dcmnorm converts a test file to JSON and renders it to PNG.
"""

import json
import subprocess
import sys
import tempfile
import venv
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TEST_FILE = REPO_ROOT / "test" / "files" / "ct.dcm"


def run(*args: str) -> str:
    print("+", " ".join(args), flush=True)
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    wheel = Path(sys.argv[1]).resolve()
    version = wheel.name.split("-")[1]

    with tempfile.TemporaryDirectory() as tmp:
        env_dir = Path(tmp) / "venv"
        venv.create(env_dir, with_pip=True)
        bin_dir = env_dir / ("Scripts" if sys.platform == "win32" else "bin")
        exe = ".exe" if sys.platform == "win32" else ""
        run(str(bin_dir / f"python{exe}"), "-m", "pip", "install", "--quiet", "--no-index", str(wheel))

        for name in ("dcmnorm", "dcmtalk"):
            out = run(str(bin_dir / f"{name}{exe}"), "--version")
            # `dcmnorm 0.7.3-<build id>`; compare the SemVer core with the wheel's version.
            reported = out.split()[1].split("-")[0]
            if not version.startswith(reported):
                sys.exit(f"{name} reports {out.strip()}, wheel is {version}")

        dcmnorm = str(bin_dir / f"dcmnorm{exe}")
        dataset = json.loads(run(dcmnorm, str(TEST_FILE)))
        if not isinstance(dataset, dict) or not dataset:
            sys.exit("dcmnorm printed no DICOM JSON")
        png = Path(tmp) / "frame.png"
        run(dcmnorm, str(TEST_FILE), str(png))
        if png.read_bytes()[:8] != b"\x89PNG\r\n\x1a\n":
            sys.exit("dcmnorm did not write a PNG")

    print(f"ok: {wheel.name}")


if __name__ == "__main__":
    main()
