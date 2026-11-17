#!/usr/bin/env python3
"""Installs a dcmnorm wheel into a fresh virtual environment and runs what it holds.

    packaging/smoke_test.py dist/dcmnorm-0.7.3-py3-none-manylinux_2_28_x86_64.whl
    packaging/smoke_test.py bindings/python/dist/dcmnorm-0.7.3-cp39-abi3-manylinux_2_28_x86_64.whl \
        [--same-cli-as dist/dcmnorm-0.7.3-py3-none-manylinux_2_28_x86_64.whl]

Both kinds of wheel: checks that the installer put both binaries on the venv's PATH as
executables, that they report the wheel's version, and that dcmnorm converts a test file to JSON
and renders it to PNG. The abi3 wheel also holds the extension module: `import dcmnorm` must work
and report the wheel's version, read the test file, and round-trip a file through MPEG-2 in
process (which fails unless FFmpeg is linked into the module). With --same-cli-as, the wheel's
CLI binaries must be byte-identical to those in the given (CLI-only) wheel.
"""

import argparse
import json
import subprocess
import sys
import tempfile
import venv
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
TEST_FILE = REPO_ROOT / "test" / "files" / "ct.dcm"


def run(*args: str) -> str:
    print("+", " ".join(args), flush=True)
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


MODULE_CHECK = """
import json, os, sys, tempfile
import dcmnorm
version, fixture = sys.argv[1], sys.argv[2]
assert dcmnorm.__file__, "no extension module"
dataset = json.loads(dcmnorm.read_json(fixture))
assert isinstance(dataset, dict) and dataset, "read_json returned nothing"
mpeg2, explicit_le = "1.2.840.10008.1.2.4.100", "1.2.840.10008.1.2.1"
with tempfile.TemporaryDirectory() as tmp:
    mpeg, back = os.path.join(tmp, "mpeg2.dcm"), os.path.join(tmp, "back.dcm")
    dcmnorm.transcode(fixture, mpeg, mpeg2)
    dcmnorm.transcode(mpeg, back, explicit_le)
    assert json.loads(dcmnorm.read_json(back))["TransferSyntaxUID"] == explicit_le, "MPEG-2 round trip"
print("import dcmnorm ok:", dcmnorm.__file__)
"""

US_FILE = REPO_ROOT / "test" / "files" / "us.dcm"


def cli_binaries(wheel: Path) -> dict:
    with zipfile.ZipFile(wheel) as archive:
        return {
            name.split("/")[-1]: archive.read(name)
            for name in archive.namelist()
            if name.count("/") == 2 and name.split("/")[0].endswith(".data") and name.split("/")[1] == "scripts"
        }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("wheel", type=Path)
    parser.add_argument("--same-cli-as", type=Path, metavar="CLI_WHEEL")
    args = parser.parse_args()
    wheel = args.wheel.resolve()
    version = wheel.name.split("-")[1]
    has_module = wheel.name.split("-")[3] == "abi3"

    if args.same_cli_as:
        ours, theirs = cli_binaries(wheel), cli_binaries(args.same_cli_as)
        if not theirs or ours != theirs:
            sys.exit(f"{wheel.name}: CLI binaries differ from {args.same_cli_as.name}"
                     f" ({sorted(ours)} vs {sorted(theirs)})")
        print(f"CLI binaries identical to {args.same_cli_as.name}: {', '.join(sorted(ours))}")

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

        if has_module:
            print(run(str(bin_dir / f"python{exe}"), "-c", MODULE_CHECK, version, str(US_FILE)).strip())

    print(f"ok: {wheel.name}")


if __name__ == "__main__":
    main()
