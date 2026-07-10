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
./scripts/install-source.sh
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

To install the latest published release binary from GitHub (or a specific version), use:

```bash
./scripts/install-release.sh
```

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

If no `v*` tags exist yet, the script uses the root `Cargo.toml` `package.version` as the baseline for computing the next version.

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

Get the full option reference from either help form:

```bash
dcmnorm -h
dcmnorm --help
```

`dcmnorm` command shape:

```text
dcmnorm [OPTIONS] [INPUT] [OUTPUT]
```

Positional arguments:

- `[INPUT]`: input DICOM or JSON file
- `[OUTPUT]`: output DICOM, JSON, or rendered file

General options:

- `-h`, `--help`
- `-V`, `--version`
- `--list-transfer-syntaxes`
- `--check-dicom`
- `--jpeg2000-codec <auto|openjpeg|kakadu>`
- `--verbose`
- `-I`, `--stdin-paths`
- `--filter <KEY>`
- `--overwrite`
- `--input-type <dicom|json>`
- `--output-type <dicom|json|raw|png|jpeg|mpeg4>`

DICOM Editing:

- `--set <KEY=VALUE>`
- `--remove <KEY>`
- `--remove-private-tags`

JSON Conversion:

- `--format <flat|standard>`
- `--keys <name|hex>`
- `--bulk-data <inline|uri>`
- `--bulk-data-source [<SOURCE>]`

DICOM Transcoding:

- `--transfer-syntax <UID>`

Rendering:

- `--render-frame <N>`
- `--render-all-frames`
- `--render-fps <FPS>`
- `--no-modality-lut`
- `--no-voi-lut`
- `--no-icc-profile`
- `--window-center <FLOAT>`
- `--window-width <FLOAT>`
- `--jpeg-quality <1-100>`
- `--output-width <PIXELS>`
- `--output-height <PIXELS>`
- `--scale-max-size <PIXELS>`
- `--redact-box <X,Y,W,H>`
- `--redact-color <R,G,B|#RRGGBB>`
- `--pad`
- `--pad-color <R,G,B|#RRGGBB>`

### Environment Variables

Runtime environment variables:

- `DCMNORM_PERF`
    - Enables scoped performance timing logs to stderr.
    - Truthy values: `1`, `true`, `yes`, `on`.
- `DCMNORM_JPEG2000_CODEC`
    - JPEG 2000 decoder preference: `auto`, `openjpeg`, or `kakadu`.
    - The CLI always sets this from `--jpeg2000-codec` (default `auto`).
- `DCMNORM_JPEG2000_DEBUG`
    - Enables JPEG 2000 debug logging when truthy.
    - `--verbose` sets this to `1`.
- `LD_LIBRARY_PATH`
    - Used to discover Kakadu shared libraries (`libkdu*.so`) at runtime.

Build-time environment variables (primarily for `--features kakadu-ffi`):

- `KAKADU_INCLUDE_DIR`
    - Explicit include directory containing Kakadu headers.
- `KAKADU_LIB_DIR`
    - Explicit library directory containing `libkdu*.so`.
- `KAKADU_LIB_NAME`
    - Optional Kakadu library base name override for linker configuration.

Convert a DICOM file to flattened JSON using named keys:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm
```

Convert a DICOM file to standard JSON with hex keys and write to a file:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.json --format standard --keys hex
```

Filter DICOM attributes before conversion (only filtered tags are parsed and emitted):

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm --filter StudyInstanceUID
```

Use multiple filters (repeat `--filter` or comma-separate values):

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.json --filter StudyInstanceUID,PatientID
```

`--filter` applies only to DICOM input. The parser reads until the requested
attributes are available, drops non-filtered attributes, and then continues with
the normal conversion pipeline (for example, DICOM to JSON output).

By default, `dcmnorm` emits bulk data as relative `BulkDataURI` values (`?offset=...&length=...`) when converting DICOM to JSON, and values of 32 bytes or less are automatically emitted as `InlineBinary`.

To embed absolute `file://` URIs in `BulkDataURI`, pass `--bulk-data-source` without a value:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm --bulk-data uri --bulk-data-source
```

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

### Validate Files with `--check-dicom`

Use `--check-dicom` to validate DICOM files by checking for a Part 10 header first,
then falling back to dataset parsing up to `SOPClassUID` for streams without file meta.

Single file:

```bash
cargo run -p dcmnorm-cli -- --check-dicom test/files/dx.dcm
```

Read paths from stdin (`-I` / `--stdin-paths`) and print only valid DICOM paths:

```bash
find . -type f | dcmnorm -I --check-dicom
```

`--check-dicom` behavior:

- prints only successful (valid DICOM) paths to stdout
- suppresses per-file failure messages
- returns exit code `0` when all inputs are valid
- returns exit code `1` if any input is invalid, unreadable, or not a regular file

### Overriding Type Detection with `--input-type` and `--output-type`

Use `--input-type` to explicitly specify the input file type (useful for files without or with incorrect extensions):

```bash
cargo run -p dcmnorm-cli -- noextension --input-type dicom --output-type json
```

Use `--output-type` to explicitly specify the output file type, including render formats:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm output --output-type json
```

Supported `--output-type` values are: `dicom`, `json`, `raw`, `png`, `jpeg`, `mpeg4`

This allows you to process files that are missing extensions or have misleading names:

```bash
# Convert a DICOM file with no extension to JSON
cargo run -p dcmnorm-cli -- dicom_data --input-type dicom

# Write DICOM output without an extension
cargo run -p dcmnorm-cli -- input.json output --output-type dicom

# Render a DICOM file to an arbitrary extension as PNG
cargo run -p dcmnorm-cli -- test/files/dx.dcm frame.img --output-type png

# Render a DICOM file as MPEG4 without a recognized extension
cargo run -p dcmnorm-cli -- test/files/ct.dcm output.video --output-type mpeg4 --render-fps 24
```

Set one or more DICOM element values while converting by repeating `--set KEY=VALUE`:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.1 --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --set StudyDescription=Normalized
```

`KEY` can be a DICOM keyword (for example, `SOPClassUID`) or a tag expression (for example, `(0008,0016)`).

Use `--overwrite` to write DICOM output back to the input path. This is useful for in-place edits with `--set`:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --overwrite
```

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

For stage-by-stage performance timing, set `DCMNORM_PERF=1` (or `true`/`yes`/`on`).
This prints scoped timings to stderr, for example:

```bash
DCMNORM_PERF=1 dcmnorm test/files/mr.dcm out.jpg --output-width 920 --output-height 758
```

Or with an explicit render format for a file without a recognized extension:

```bash
DCMNORM_PERF=1 dcmnorm test/files/mr.dcm output.img --output-type jpeg --output-width 920 --output-height 758
```

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

Pipe input paths from stdin using `-I` / `--stdin-paths`, one path per line:

```bash
find . -name "*.dcm" | dcmnorm -I
```

This applies the same options as single-file mode to every path. Errors for individual files are printed to stderr with the filename, and `dcmnorm` exits non-zero if any file fails.

`--set` also applies in piped mode. The same element updates are applied to each input path:

```bash
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2
```

To update each input file in place in piped mode, combine `--set` with `--overwrite`:

```bash
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --overwrite
```

To emit `file://` `BulkDataURI` values in piped mode, also pass `--bulk-data-source` without a value:

```bash
find . -name "*.dcm" | dcmnorm -I --bulk-data uri --bulk-data-source
```

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
- relative `BulkDataURI` bulk data output (`?offset=...&length=...`)
- `file://` `BulkDataURI` output when `--bulk-data-source` is passed without a value
- automatic `InlineBinary` fallback for bulk values of 32 bytes or less

For JSON to DICOM, `dcmnorm` defaults to:

- flattened JSON input
- optional `--bulk-data-source` when resolving `BulkDataURI`
