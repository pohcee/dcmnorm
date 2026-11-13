#!/usr/bin/env python3
"""Package built dcmnorm/dcmtalk binaries for release: archives plus a PyPI wheel.

    packaging/package.py --platform linux-x86_64 --bin-dir target/release --out-dir dist

For each CLI it writes the GitHub release archives (versioned and "latest" names, each with a
.sha256 file in `sha256sum` format), and one wheel holding both binaries:

    dcmnorm-v0.7.3-linux-x86_64.tar.gz      dcmnorm-linux-x86_64.tar.gz      (.zip on Windows)
    dcmtalk-v0.7.3-linux-x86_64.tar.gz      dcmtalk-linux-x86_64.tar.gz
    dcmnorm-0.7.3-py3-none-manylinux_2_28_x86_64.whl

The wheel is how `pip install dcmnorm` / `uv tool install dcmnorm` deliver the native CLIs: the
binaries sit in `<name>.data/scripts/`, which installers put on PATH as-is. There is no Python
code in it and no Python at run time (the in-process Python binding is a separate package,
bindings/python). Only the standard library is used, so this runs on any runner's Python 3.8+.

The version comes from exec/dcmnorm/Cargo.toml. When GITHUB_REF is a tag, it must match.
"""

import argparse
import base64
import hashlib
import io
import os
import re
import stat
import sys
import tarfile
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
BINARIES = ("dcmnorm", "dcmtalk")
WHEEL_NAME = "dcmnorm"

# Release platform name -> wheel platform tag. The Linux tag must match the glibc the binaries
# were built against (packaging/build-linux.sh builds in manylinux_2_28 and checks it); the macOS
# tags match MACOSX_DEPLOYMENT_TARGET in the release workflow.
WHEEL_PLATFORM_TAGS = {
    "linux-x86_64": "manylinux_2_28_x86_64",
    "linux-aarch64": "manylinux_2_28_aarch64",
    "macos-x86_64": "macosx_10_15_x86_64",
    "macos-aarch64": "macosx_11_0_arm64",
    "windows-x86_64": "win_amd64",
}

# Fixed timestamp for archive members, so rebuilding the same binaries gives the same files.
ZIP_DATE = (2020, 1, 1, 0, 0, 0)
TAR_MTIME = 1577836800  # 2020-01-01T00:00:00Z


def cargo_version() -> str:
    manifest = (REPO_ROOT / "exec" / "dcmnorm" / "Cargo.toml").read_text(encoding="utf-8")
    in_package = False
    for line in manifest.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package = stripped == "[package]"
            continue
        match = re.match(r'version\s*=\s*"([^"]+)"', stripped)
        if in_package and match:
            return match.group(1)
    sys.exit("could not read the version from exec/dcmnorm/Cargo.toml")


def pep440_version(semver: str) -> str:
    """0.7.3 -> 0.7.3, 0.7.3-rc.1 -> 0.7.3rc1 (scripts/release-tag.sh's prerelease form)."""
    match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-([0-9A-Za-z.-]+))?", semver)
    if not match:
        sys.exit(f"not a SemVer version: {semver}")
    base, pre = match.groups()
    if not pre:
        return base
    pre_match = re.fullmatch(r"(alpha|a|beta|b|rc)\.?(\d+)", pre)
    if not pre_match:
        sys.exit(f"prerelease '{pre}' has no PyPI equivalent (use alpha.N, beta.N or rc.N)")
    kind, number = pre_match.groups()
    kind = {"alpha": "a", "beta": "b"}.get(kind, kind)
    return f"{base}{kind}{number}"


def sha256_file(path: Path, out_dir: Path) -> None:
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    (out_dir / f"{path.name}.sha256").write_text(f"{digest}  {path.name}\n", encoding="utf-8")


def write_archive(path: Path, binary: Path, binary_name: str) -> None:
    members = [(binary_name, binary, 0o755), ("README.md", REPO_ROOT / "README.md", 0o644),
               ("LICENSE", REPO_ROOT / "LICENSE", 0o644)]
    if path.suffix == ".zip":
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
            for name, source, mode in members:
                info = zipfile.ZipInfo(name, ZIP_DATE)
                info.external_attr = (stat.S_IFREG | mode) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, source.read_bytes())
    else:
        with tarfile.open(path, "w:gz", format=tarfile.PAX_FORMAT) as archive:
            for name, source, mode in members:
                data = source.read_bytes()
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = mode
                info.mtime = TAR_MTIME
                archive.addfile(info, io.BytesIO(data))


def wheel_metadata(version: str) -> str:
    description = (REPO_ROOT / "packaging" / "pypi-README.md").read_text(encoding="utf-8")
    return "\n".join([
        "Metadata-Version: 2.1",
        f"Name: {WHEEL_NAME}",
        f"Version: {version}",
        "Summary: DICOM read/edit/transcode/render (dcmnorm) and DICOM network (dcmtalk) command-line tools",
        "Author: Jason Klotzer",
        "License: Apache-2.0",
        "Project-URL: Homepage, https://github.com/pohcee/dcmnorm",
        "Project-URL: Source, https://github.com/pohcee/dcmnorm",
        "Project-URL: Issues, https://github.com/pohcee/dcmnorm/issues",
        "Keywords: dicom,medical-imaging,pacs,dimse,anonymization,cli",
        "Classifier: Development Status :: 4 - Beta",
        "Classifier: Environment :: Console",
        "Classifier: Intended Audience :: Developers",
        "Classifier: Intended Audience :: Healthcare Industry",
        "Classifier: Intended Audience :: Science/Research",
        "Classifier: License :: OSI Approved :: Apache Software License",
        "Classifier: Operating System :: MacOS",
        "Classifier: Operating System :: Microsoft :: Windows",
        "Classifier: Operating System :: POSIX :: Linux",
        "Classifier: Programming Language :: Rust",
        "Classifier: Topic :: Scientific/Engineering :: Medical Science Apps.",
        "Requires-Python: >=3.8",
        "Description-Content-Type: text/markdown",
        "",
        description,
    ])


def write_wheel(out_dir: Path, version: str, platform_tag: str, binaries: dict) -> Path:
    tag = f"py3-none-{platform_tag}"
    dist_info = f"{WHEEL_NAME}-{version}.dist-info"
    data_scripts = f"{WHEEL_NAME}-{version}.data/scripts"
    files = []  # (archive name, bytes, mode)
    for name, path in binaries.items():
        files.append((f"{data_scripts}/{name}", path.read_bytes(), 0o755))
    files.append((f"{dist_info}/LICENSE", (REPO_ROOT / "LICENSE").read_bytes(), 0o644))
    files.append((f"{dist_info}/METADATA", wheel_metadata(version).encode("utf-8"), 0o644))
    wheel_file = "\n".join([
        "Wheel-Version: 1.0",
        "Generator: dcmnorm packaging/package.py",
        "Root-Is-Purelib: false",
        f"Tag: {tag}",
        "",
    ])
    files.append((f"{dist_info}/WHEEL", wheel_file.encode("utf-8"), 0o644))

    record_lines = []
    for name, data, _ in files:
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode("ascii")
        record_lines.append(f"{name},sha256={digest},{len(data)}")
    record_lines.append(f"{dist_info}/RECORD,,")
    files.append((f"{dist_info}/RECORD", ("\n".join(record_lines) + "\n").encode("utf-8"), 0o644))

    path = out_dir / f"{WHEEL_NAME}-{version}-{tag}.whl"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as wheel:
        for name, data, mode in files:
            info = zipfile.ZipInfo(name, ZIP_DATE)
            # Installers keep the executable bit from here, which the binaries need.
            info.external_attr = (stat.S_IFREG | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            wheel.writestr(info, data)
    return path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--platform", required=True, choices=sorted(WHEEL_PLATFORM_TAGS))
    parser.add_argument("--bin-dir", required=True, type=Path, help="directory holding the built binaries")
    parser.add_argument("--out-dir", required=True, type=Path)
    args = parser.parse_args()

    version = cargo_version()
    ref = os.environ.get("GITHUB_REF", "")
    if ref.startswith("refs/tags/") and ref != f"refs/tags/v{version}":
        sys.exit(f"tag {ref[len('refs/tags/'):]} does not match exec/dcmnorm/Cargo.toml version {version}")
    tag = f"v{version}"

    windows = args.platform.startswith("windows-")
    exe = ".exe" if windows else ""
    binaries = {}
    for name in BINARIES:
        path = args.bin_dir / f"{name}{exe}"
        if not path.is_file():
            sys.exit(f"missing binary: {path}")
        binaries[f"{name}{exe}"] = path

    args.out_dir.mkdir(parents=True, exist_ok=True)
    ext = "zip" if windows else "tar.gz"
    for name, path in binaries.items():
        stem = name[: -len(exe)] if exe else name
        for archive_name in (f"{stem}-{tag}-{args.platform}.{ext}", f"{stem}-{args.platform}.{ext}"):
            archive = args.out_dir / archive_name
            write_archive(archive, path, name)
            sha256_file(archive, args.out_dir)
            print(archive)

    wheel = write_wheel(args.out_dir, pep440_version(version), WHEEL_PLATFORM_TAGS[args.platform], binaries)
    print(wheel)


if __name__ == "__main__":
    main()
