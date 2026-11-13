# CLI packaging

How the `dcmnorm` and `dcmtalk` release artifacts are built. The
[Build and release workflow](../.github/workflows/release.yml) runs all of this on every pull
request (build and smoke test only) and on every `v*.*.*` tag (then publishes).

| Platform | Runner | Build | Artifacts |
|---|---|---|---|
| `linux-x86_64` | `ubuntu-24.04` | `build-linux.sh` (manylinux_2_28 container) | `.tar.gz`, `.deb`, wheel `manylinux_2_28_x86_64` |
| `linux-aarch64` | `ubuntu-24.04-arm` | `build-linux.sh` (manylinux_2_28 container) | `.tar.gz`, `.deb`, wheel `manylinux_2_28_aarch64` |
| `macos-aarch64` | `macos-15` | `cargo build`, deployment target 11.0 | `.tar.gz`, wheel `macosx_11_0_arm64` |
| `macos-x86_64` | `macos-15-intel` | `cargo build`, deployment target 10.15 | `.tar.gz`, wheel `macosx_10_15_x86_64` |
| `windows-x86_64` | `windows-2025` | `cargo build` (MSVC) | `.zip`, wheel `win_amd64` |

The default build needs only a C/C++ toolchain and CMake: FFmpeg is not built or linked, and the
video transfer syntaxes use the `ffmpeg` command at run time (see the top-level README, "Video
transfer syntaxes"). Release builds are stripped (`CARGO_PROFILE_RELEASE_STRIP=symbols`).

## Files

- `build-linux.sh [x86_64|aarch64]` builds both CLIs in `quay.io/pypa/manylinux_2_28_<arch>`
  into `target/manylinux_2_28-<arch>/release/`, then fails if either binary needs a glibc newer
  than 2.28 or links a shared library outside the manylinux policy. Needs Docker that runs the
  image natively. `manylinux.cmake` is the CMake toolchain file it uses so CMake installs
  CharLS into `lib/`, where charls-sys looks, instead of RHEL's `lib64/`.
- `package.py --platform <platform> --bin-dir <dir> --out-dir dist` writes the release archives
  (versioned and "latest" names, each with a `.sha256`) and the wheel. The version comes from
  `exec/dcmnorm/Cargo.toml` and must match the tag when run for one.
- `smoke_test.py <wheel>` installs the wheel into a fresh virtual environment and runs both CLIs
  from it (version, DICOM to JSON, DICOM to PNG).
- `pypi-README.md` is the PyPI project description.
- `make-brand-images.py` draws the dcmnorm icon and everything made from it: the plugin's listing icon
  (`plugin/.claude-plugin/icon.png`, also the README and PyPI logo), the Node test website's favicon and
  `docs/images/social-preview.png`, which is uploaded by hand as the repository's GitHub social preview
  (Settings > General > Social preview). Edit the drawing there, not the PNGs.

The wheel (`dcmnorm` on PyPI) holds both binaries in `dcmnorm-<version>.data/scripts/`, which pip,
pipx and uv install onto `PATH` as they are. It has no Python code and no Python runtime
dependency; `Requires-Python` only reflects the installers. The in-process Python binding is a
separate package, built from `bindings/python`. Prerelease tags map to PyPI versions as
`v0.8.0-rc.1` to `0.8.0rc1` (only `alpha`, `beta` and `rc` identifiers are allowed).

To build and check the Linux wheel locally:

```bash
packaging/build-linux.sh x86_64
python3 packaging/package.py --platform linux-x86_64 --bin-dir target/manylinux_2_28-x86_64/release --out-dir dist
python3 packaging/smoke_test.py dist/dcmnorm-*-manylinux_2_28_x86_64.whl
```

## PyPI publishing (one-time setup)

Publishing uses [Trusted Publishing](https://docs.pypi.org/trusted-publishers/): PyPI accepts
uploads from this repository's workflow by OIDC, so no API token is stored anywhere. Until it is
set up, tags publish everything else and skip PyPI. The repository owner does this once:

1. On PyPI, add a pending trusted publisher (Account settings > Publishing > Add a new pending
   publisher) for project `dcmnorm`: owner `pohcee`, repository `dcmnorm`, workflow
   `release.yml`, environment `pypi`. The first successful upload creates the project.
2. On GitHub, create the `pypi` environment (Settings > Environments). Optionally limit it to
   `v*` tags under "Deployment branches and tags".
3. Set the repository variable `PYPI_PUBLISH` to `true` (Settings > Secrets and variables >
   Actions > Variables).

The next tag then publishes the wheels. A tag that already ran without PyPI can be published by
re-running the workflow for it from the Actions tab.
