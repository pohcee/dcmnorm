# dcmnorm

Rust workspace for reading, writing, transcoding, and converting DICOM data.

This repository contains:

- `dcmnorm`: a library crate with DICOM file, memory, and JSON conversion helpers
- `exec/dcmnorm`: a CLI for converting between DICOM, transcoded DICOM, JSON, and rendered images/raw frames

## Workspace Layout

```text
.
├── Cargo.toml
├── src/
│   └── dicom_io.rs
├── exec/
│   └── dcmnorm/
└── test/
    └── files/
```

## Build

Default builds enable the MPEG and JPEG-LS codec features.

Native prerequisites for the default build on Debian or Ubuntu are:

- `build-essential`
- `clang`
- `cmake`
- `libc6-dev`
- `libclang-dev`
- `pkg-config`
- `libavutil-dev`
- `libavcodec-dev`
- `libavformat-dev`
- `libswscale-dev`
- `libswresample-dev`

The FFmpeg integration is built with a reduced `ffmpeg-next` feature set, so
`libavfilter-dev` and `libavdevice-dev` are not required for the current build.

Example install command:

```bash
sudo apt-get update
sudo apt-get install -y \
    build-essential \
    clang \
    cmake \
    libc6-dev \
    libclang-dev \
    pkg-config \
    libavutil-dev \
    libavcodec-dev \
    libavformat-dev \
    libswscale-dev \
    libswresample-dev
```

Build the entire workspace from the repository root:

```bash
cargo build --workspace
```

Build the entire workspace in release mode:

```bash
cargo build --workspace --release
```

Build the workspace with Kakadu FFI enabled:

```bash
cargo build --workspace --features kakadu-ffi
```

Build with Kakadu FFI using explicit include/lib locations:

```bash
KAKADU_INCLUDE_DIR=$HOME/.local/include/kakadu \
KAKADU_LIB_DIR=$HOME/.local/lib \
cargo build --workspace --features kakadu-ffi
```

Build without the default MPEG and JPEG-LS codec features:

```bash
cargo build --workspace --no-default-features
```

Release binaries are written to `target/release/`.

## Install Binaries

Install the CLI tools directly from the workspace using Cargo:

```bash
cargo install --path exec/dcmnorm
```

To install every CLI under `exec/` with one command, use the helper script:

```bash
./scripts/install-exec.sh
```

The install script automatically detects Kakadu headers and libraries and enables
`kakadu-ffi` when available.

The install script also verifies the default codec toolchain before invoking Cargo.
For the default build this means `pkg-config`, `clang`, standard C headers, and the
FFmpeg development packages listed above must already be installed.

This installs the binaries into Cargo's bin directory, usually `~/.cargo/bin`.

If `~/.cargo/bin` is not already on your `PATH`, add this to your shell profile:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

If you prefer not to use `cargo install`, you can still build and copy the release binaries manually.

Build the release binaries first:

```bash
cargo build --workspace --release
```

The executables will be available at:

- `target/release/dcmnorm`

To install them for the current user, copy them into a directory on your `PATH`, for example `~/.local/bin`:

```bash
mkdir -p ~/.local/bin
cp target/release/dcmnorm ~/.local/bin/
```

If `~/.local/bin` is not already on your `PATH`, add this to your shell profile:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## GitHub Releases

This repository includes two GitHub Actions workflows for SemVer-based CLI releases:

- `.github/workflows/semver-tag.yml`: manually creates and pushes the next `vX.Y.Z` tag from the latest existing `v*` tag
- `.github/workflows/release.yml`: runs on pushed version tags, builds the CLI, and creates a GitHub Release with artifacts

Release flow:

1. Run the **SemVer Tag** workflow from the Actions tab and choose `patch`, `minor`, or `major`.
2. The workflow pushes a new version tag (for example `v0.1.1`).
3. The **Build and Release CLI** workflow is triggered by that tag and publishes:
    - `dcmnorm-<tag>-linux-x86_64.tar.gz`
    - `dcmnorm-<tag>-linux-x86_64.tar.gz.sha256`

Prereleases are supported in the SemVer tag workflow via the `prerelease` input.

### Local Tag + Release Trigger

If you prefer not to manually run the tag workflow in GitHub, use the local helper script:

```bash
./scripts/release-tag.sh patch
```

Supported bump types are `patch`, `minor`, and `major`.

You can create a prerelease tag locally:

```bash
./scripts/release-tag.sh minor --prerelease rc
```

Use `--dry-run` to preview the computed next tag without creating or pushing it.

The script updates versions in:

- `Cargo.toml`
- `exec/dcmnorm/Cargo.toml`

Then it creates a release commit, pushes that commit to `origin`, and pushes the version tag.
The pushed tag triggers `.github/workflows/release.yml` automatically.

## Build The CLI

Build only `dcmnorm`:

```bash
cargo build -p dcmnorm-cli
```

Build the CLI in release mode:

```bash
cargo build -p dcmnorm-cli --release
```

## Docker

This repository includes a multi-stage Dockerfile that builds `dcmnorm` in a
toolchain stage and copies only the release binary into a slim runtime stage.

Build the image:

```bash
docker build -t dcmnorm .
```

Run the CLI:

```bash
docker run --rm dcmnorm
```

Convert a file from a bind-mounted working directory:

```bash
docker run --rm \
    -v "$PWD":/work \
    -w /work \
    dcmnorm \
    test/files/dx.dcm
```

The final runtime image installs these native packages:

- `ca-certificates`
- `ffmpeg`
- `libstdc++6`

Build-only dependencies such as `clang`, `cmake`, `pkg-config`, and FFmpeg `-dev`
packages are kept in the builder stage and are not present in the final image.

Kakadu is not included in the Docker image. If you need JPEG 2000 Kakadu support,
provide the Kakadu headers and shared libraries yourself and build with `kakadu-ffi`.

## Test

Run all tests in the workspace:

```bash
cargo test --workspace
```

## CLI Usage

Convert a DICOM file to flattened JSON using named keys:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm
```

Convert a DICOM file to standard JSON with hex keys and write to a file:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.json --format standard --keys hex
```

By default, `dcmnorm` emits bulk data as `BulkDataURI`, but values of 32 bytes or less are automatically emitted as `InlineBinary` when converting DICOM to JSON.

Convert JSON back to a DICOM file:

```bash
cargo run -p dcmnorm-cli -- out.json out.dcm
```

Convert JSON with `BulkDataURI` references back to DICOM using a source file:

```bash
cargo run -p dcmnorm-cli -- out.json out.dcm --bulk-data-source test/files/dx.dcm
```

`dcmnorm` infers the conversion direction from the input and output file types:

- DICOM input + JSON output, or no output, runs DICOM to JSON
- DICOM input + DICOM output with `--transfer-syntax <UID>` runs DICOM to DICOM transcoding
- DICOM input + `.png` / `.jpg` / `.jpeg` / `.raw` output runs DICOM frame rendering
- JSON input + DICOM output runs JSON to DICOM
- JSON to DICOM requires an output path

Render the first frame of a DICOM file to PNG:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.png
```

Render frame 2 to JPEG with explicit quality:

```bash
cargo run -p dcmnorm-cli -- test/files/ct.dcm out.jpg --render-frame 1 --jpeg-quality 95
```

Render to raw 8-bit frame bytes:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.raw
```

Render all frames from a multiframe dataset to numbered PNG files (`out_000001.png`, `out_000002.png`, ...):

```bash
cargo run -p dcmnorm-cli -- test/files/ct.dcm out.png --render-all-frames
```

Render all frames from a multiframe dataset to a single `.mp4` video:

```bash
cargo run -p dcmnorm-cli -- test/files/ct.dcm out.mp4 --render-fps 24
```

If `--render-fps` is omitted for `.mp4` output, `dcmnorm` uses frame-rate metadata
from the DICOM instance when available (`RecommendedDisplayFrameRate`, `CineRate`,
`FrameTime`, or `FrameTimeVector`) and falls back to 24 FPS otherwise.

Use `--verbose` to print render/conversion diagnostics. Without `--verbose`, external
tool output such as `ffmpeg` is suppressed unless an error occurs.

Rendering supports 1-bit, 8-bit, and 16-bit monochrome pixel data, as well as RGB data.
The render pipeline includes decompression when needed and applies modality LUT and VOI LUT/windowing by default.
Use `--no-modality-lut` and/or `--no-voi-lut` to disable those steps, and use
`--window-center` / `--window-width` to override VOI windowing.

Photometric interpretations supported by rendering include:

- `MONOCHROME1`
- `MONOCHROME2`
- `PALETTE COLOR`
- `RGB`

Both planar configurations are supported for RGB rendering (`PlanarConfiguration` 0 and 1).

`.mp4` output requires `ffmpeg` installed and available on `PATH`.

Transcode a DICOM file to Explicit VR Big Endian:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.2
```

List the transfer syntaxes known to the current build and whether dataset read/write and pixel decode/encode are available:

```bash
cargo run -p dcmnorm-cli -- --list-transfer-syntaxes
```

Transfer-syntax support is build-specific. The default build in this repository enables
the MPEG and JPEG-LS codec features in addition to the DICOM library support that is
available without extra native imaging libraries:

- native uncompressed syntaxes
- deflated dataset syntaxes
- encapsulated uncompressed pixel data
- MPEG transfer syntax support via FFmpeg-backed build integration
- JPEG baseline decode/encode
- JPEG extended and JPEG lossless decode-only
- JPEG-LS transfer syntax support via CharLS-backed build integration
- JPEG 2000 decode-only
- RLE lossless decode-only

Transfer syntaxes which the current build cannot encode or decode are reported explicitly by `--list-transfer-syntaxes` and by transcoding errors.

For JPEG 2000, `dcmnorm` checks `LD_LIBRARY_PATH` at runtime for Kakadu libraries (`libkdu*.so`).
Kakadu use is FFI-only (Rust -> C++ interop), not CLI-based.

To enable Kakadu interop, build with feature `kakadu-ffi` and make the required Kakadu headers
available in a normal include location such as `~/.local/include/kakadu`, `/usr/local/include/kakadu`,
or `/usr/include/kakadu` so the C++ bridge can be compiled automatically.

If your headers are installed in a non-standard location, you can still point the build at them with
`KAKADU_INCLUDE_DIR`.

If Kakadu FFI is not enabled or Kakadu is unavailable, the OpenJPEG-based path remains in use.

## JSON Defaults

For DICOM to JSON, `dcmnorm` defaults to:

- flattened JSON output
- named lookup keys where possible
- `BulkDataURI` bulk data output
- automatic `InlineBinary` fallback for bulk values of 32 bytes or less

For JSON to DICOM, `dcmnorm` defaults to:

- flattened JSON input
- optional `--bulk-data-source` when resolving `BulkDataURI`
