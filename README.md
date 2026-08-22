# dcmnorm

Rust workspace for reading, writing, transcoding, and converting DICOM data.

This repository contains:

- [`dcmnorm`](src/): a library crate with DICOM file, memory, JSON conversion, and DIMSE network helpers
- [`exec/dcmnorm`](exec/dcmnorm/): a CLI for converting between DICOM, transcoded DICOM, JSON, and rendered images/raw frames
- [`exec/dcmtalk`](exec/dcmtalk/): a DIMSE network CLI (C-ECHO/C-STORE/C-FIND/C-MOVE SCU plus a storage SCP), covering the same ground as [dcmtk](https://dcmtk.org/)'s `echoscu`/`storescu`/`findscu`/`movescu`/`storescp`
- [`bindings/node`](bindings/node/): Node.js bindings (`@pohcee/dcmnorm-node`) that call the library in-process via napi-rs — see that package's own README for its API

## Contents

- [Workspace Layout](#workspace-layout)
- [Build](#build)
- [Install](#install)
- [Docker](#docker)
- [Test](#test)
- [Releasing](#releasing)
- [dcmnorm CLI Usage](#dcmnorm-cli-usage)
- [dcmtalk CLI Usage](#dcmtalk-cli-usage)
- [Thanks](#thanks)

## Workspace Layout

```text
.
├── Cargo.toml
├── src/               # dcmnorm library crate
├── exec/
│   ├── dcmnorm/       # dcmnorm-cli package (the `dcmnorm` binary)
│   └── dcmtalk/       # dcmtalk package (the `dcmtalk` binary)
├── bindings/
│   └── node/          # @pohcee/dcmnorm-node napi-rs bindings
├── scripts/           # install / release helper scripts
└── test/
    └── files/         # sample DICOM fixtures used by docs and tests
```

## Build

### Prerequisites

Default builds enable the MPEG and JPEG-LS codec features. Native prerequisites for the
default build on Debian or Ubuntu are:

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

### Building the workspace

```bash
# whole workspace, debug
cargo build --workspace

# whole workspace, release
cargo build --workspace --release

# without the default MPEG and JPEG-LS codec features
cargo build --workspace --no-default-features
```

Release binaries are written to `target/release/`.

### Building a single crate

```bash
# just dcmnorm
cargo build -p dcmnorm-cli

# just dcmtalk
cargo build -p dcmtalk

# both, release mode
cargo build -p dcmnorm-cli -p dcmtalk --release
```

### Kakadu FFI (JPEG 2000)

By default, JPEG 2000 decoding uses the bundled OpenJPEG path. To enable the optional
Kakadu FFI bridge instead:

```bash
cargo build --workspace --features kakadu-ffi
```

This requires Kakadu headers in a normal include location (`~/.local/include/kakadu`,
`/usr/local/include/kakadu`, or `/usr/include/kakadu`) so the C++ bridge can compile
automatically. If your headers live elsewhere, point the build at them explicitly:

```bash
KAKADU_INCLUDE_DIR=$HOME/.local/include/kakadu \
KAKADU_LIB_DIR=$HOME/.local/lib \
cargo build --workspace --features kakadu-ffi
```

Build-time environment variables for this feature:

- `KAKADU_INCLUDE_DIR` — explicit include directory containing Kakadu headers
- `KAKADU_LIB_DIR` — explicit library directory containing `libkdu*.so`
- `KAKADU_LIB_NAME` — optional Kakadu library base name override for linker configuration

See [JPEG 2000 codec selection](#jpeg-2000-codec-selection) for the corresponding runtime behavior.

## Install

### From source with Cargo

```bash
cargo install --path exec/dcmnorm
```

To install every CLI under `exec/` with one command, use the helper script instead:

```bash
./scripts/install-source.sh
```

This script auto-detects Kakadu headers/libraries and enables `kakadu-ffi` when available,
and verifies the default codec toolchain (`pkg-config`, `clang`, standard C headers, and the
FFmpeg development packages above) before invoking Cargo.

Either method installs into Cargo's bin directory, usually `~/.cargo/bin`. If that isn't on
your `PATH` yet, add:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

### Manual binary copy

If you'd rather not use `cargo install`, build the release binaries and copy them yourself:

```bash
cargo build --workspace --release

mkdir -p ~/.local/bin
cp target/release/dcmnorm target/release/dcmtalk ~/.local/bin/
```

If `~/.local/bin` isn't on your `PATH` yet, add:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### Pre-built release binaries

To install the latest published release binary from GitHub (or a specific version):

```bash
./scripts/install-release.sh
```

Or, generally:

```bash
curl -sSL pohcee.com/dcmnorm | sh
```

## Docker

This repository includes a multi-stage Dockerfile that builds `dcmnorm` and `dcmtalk` in a
toolchain stage and copies only the release binaries into a slim runtime stage. The final
runtime image installs `ca-certificates`, `ffmpeg`, and `libstdc++6`; build-only dependencies
(`clang`, `cmake`, `pkg-config`, FFmpeg `-dev` packages) stay in the builder stage.

Kakadu is not included in the image — see [JPEG 2000 codec selection](#jpeg-2000-codec-selection)
if you need Kakadu support and are willing to provide the headers/libraries yourself.

Build the image:

```bash
docker build -t dcmnorm .
```

Run the CLI (the image's default entrypoint is `dcmnorm`):

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

Run `dcmtalk` instead by overriding the entrypoint:

```bash
docker run --rm --entrypoint dcmtalk dcmnorm echoscu somepacs.example.com:11112

# storescp needs its listening port published
docker run --rm --entrypoint dcmtalk -p 11112:11112 \
    -v "$PWD/received":/data \
    dcmnorm storescp 11112 --cache-path /data
```

## Test

```bash
cargo test --workspace
```

## Releasing

This repository uses two GitHub Actions workflows for SemVer-based CLI releases:

- `.github/workflows/semver-tag.yml`: manually creates and pushes the next `vX.Y.Z` tag from the latest existing `v*` tag
- `.github/workflows/release.yml`: runs on pushed version tags, builds the CLI, and creates a GitHub Release with artifacts

Release flow:

1. Run the **SemVer Tag** workflow from the Actions tab and choose `patch`, `minor`, or `major`.
2. The workflow pushes a new version tag (for example `v0.1.1`).
3. The **Build and Release CLIs** workflow is triggered by that tag and publishes, for each of `dcmnorm` and `dcmtalk`:
    - `<name>-<tag>-linux-x86_64.tar.gz`
    - `<name>-<tag>-linux-x86_64.tar.gz.sha256`
    - `<name>-linux-x86_64.tar.gz` / `.sha256` (rolling "latest" alias, overwritten each release)

Prereleases are supported in the SemVer tag workflow via the `prerelease` input.

### Local tag + release trigger

If you prefer not to manually run the tag workflow in GitHub, use the local helper script:

```bash
./scripts/release-tag.sh patch          # bump types: patch, minor, major
./scripts/release-tag.sh minor --prerelease rc
./scripts/release-tag.sh patch --dry-run  # preview the computed next tag only
```

The script updates versions in `Cargo.toml`, `exec/dcmnorm/Cargo.toml`, and
`exec/dcmtalk/Cargo.toml`, then creates a release commit and pushes both the commit and the
version tag to `origin`. The pushed tag triggers `.github/workflows/release.yml` automatically.
If no `v*` tags exist yet, the script uses the root `Cargo.toml` `package.version` as the
baseline for computing the next version.

## dcmnorm CLI Usage

Get the full option reference from either help form:

```bash
dcmnorm -h
dcmnorm --help
```

Command shape:

```text
dcmnorm [OPTIONS] [INPUT]... [OUTPUT]
```

`dcmnorm` infers the conversion direction from the input and output file types:

- DICOM input + JSON output, or no output, runs DICOM to JSON
- DICOM input + DICOM output with `--transfer-syntax <UID>` runs DICOM to DICOM transcoding
- DICOM input + `.png` / `.jpg` / `.jpeg` / `.raw` output runs DICOM frame rendering
- JSON input + DICOM output runs JSON to DICOM (requires an output path)

### Options reference

Positional arguments - a single trailing list, split by convention rather than as two separate
flags (shell globs work as usual, since the shell expands them before `dcmnorm` ever sees them):

- 0 or 1 path: `[INPUT]`, with OUTPUT defaulting to stdout JSON - e.g. `dcmnorm in.dcm`
- 2 paths: `[INPUT] [OUTPUT]` - e.g. `dcmnorm in.dcm out.png`
- 3+ paths, no `--mpr`: every path is an independent `[INPUT]`, each processed on its own (same
  as piping via `-I`/`--stdin-paths` - see [Batch mode](#batch-mode-with--i---stdin-paths-or-a-file-list) below) -
  e.g. `dcmnorm *.dcm --set SOPClassUID=... --overwrite`
- 2+ paths with `--mpr`: every path but the last is an `[INPUT]` slice combined into one volume,
  the last is `[OUTPUT]` - see [Render a Multiplanar Reformation (MPR)](#render-a-multiplanar-reformation-mpr)

General:

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
- `--output-type <dicom|json|raw|png|jpeg|mpeg4|texture>`

DICOM editing:

- `--set <KEY=VALUE>`
- `--remove <KEY>`
- `--remove-private-tags`

JSON conversion:

- `--format <flat|standard>`
- `--keys <name|hex>`
- `--bulk-data <inline|uri>`
- `--bulk-data-source [<SOURCE>]`

DICOM transcoding:

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
- `--no-overlays`
- `--overlay-index <N>`
- `--overlay-color <R,G,B|#RRGGBB>`

MPR (Multiplanar Reformation):

- `--mpr <axial|coronal|sagittal|YAW,PITCH,ROLL>`
- `--mpr-origin <X,Y,Z>`
- `--mpr-depth <MM>`
- `--mpr-spacing <MM>`
- `--mpr-thickness <MM>`
- `--mpr-projection <mip|minip|average>`

Texture Export:

- `--texture-max-dim <N>`
- `--texture-compression <none|gzip>`

### JSON conversion defaults

DICOM to JSON defaults to:

- flattened JSON output
- named lookup keys where possible
- relative `BulkDataURI` bulk data output (`?offset=...&length=...`)
- `file://` `BulkDataURI` output when `--bulk-data-source` is passed without a value
- automatic `InlineBinary` fallback for bulk values of 32 bytes or less

JSON to DICOM defaults to:

- flattened JSON input
- optional `--bulk-data-source` when resolving `BulkDataURI`

### Runtime environment variables

- `DCMNORM_PERF` — enables scoped performance timing logs to stderr. Truthy values: `1`, `true`, `yes`, `on`.
- `DCMNORM_JPEG2000_CODEC` — JPEG 2000 decoder preference: `auto`, `openjpeg`, or `kakadu`. The CLI always sets this from `--jpeg2000-codec` (default `auto`).
- `DCMNORM_JPEG2000_DEBUG` — enables JPEG 2000 debug logging when truthy. `--verbose` sets this to `1`.
- `LD_LIBRARY_PATH` — used to discover Kakadu shared libraries (`libkdu*.so`) at runtime.

(Build-time Kakadu variables are covered under [Kakadu FFI](#kakadu-ffi-jpeg-2000).)

### Convert DICOM ⇄ JSON

Convert a DICOM file to flattened JSON using named keys:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm
```

Convert a DICOM file to standard JSON with hex keys and write to a file:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.json --format standard --keys hex
```

Convert JSON back to a DICOM file:

```bash
cargo run -p dcmnorm-cli -- out.json out.dcm
```

Convert JSON with `BulkDataURI` references back to DICOM using a source file:

```bash
cargo run -p dcmnorm-cli -- out.json out.dcm --bulk-data-source test/files/dx.dcm
```

### Filter attributes

Filter DICOM attributes before conversion (only filtered tags are parsed and emitted):

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm --filter StudyInstanceUID
```

Use multiple filters (repeat `--filter` or comma-separate values):

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.json --filter StudyInstanceUID,PatientID
```

`--filter` applies only to DICOM input. The parser reads until the requested attributes are
available, drops non-filtered attributes, and then continues with the normal conversion
pipeline (for example, DICOM to JSON output).

### Bulk data / `BulkDataURI`

By default, bulk data is emitted as relative `BulkDataURI` values (`?offset=...&length=...`)
when converting DICOM to JSON, and values of 32 bytes or less are automatically emitted as
`InlineBinary`.

To embed absolute `file://` URIs in `BulkDataURI`, pass `--bulk-data-source` without a value:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm --bulk-data uri --bulk-data-source
```

### Validate files with `--check-dicom`

Checks for a Part 10 header first, then falls back to dataset parsing up to `SOPClassUID`
for streams without file meta.

Single file:

```bash
cargo run -p dcmnorm-cli -- --check-dicom test/files/dx.dcm
```

Read paths from stdin (`-I` / `--stdin-paths`) and print only valid DICOM paths:

```bash
find . -type f | dcmnorm -I --check-dicom
```

Behavior:

- prints only successful (valid DICOM) paths to stdout
- suppresses per-file failure messages
- returns exit code `0` when all inputs are valid
- returns exit code `1` if any input is invalid, unreadable, or not a regular file

### Override type detection with `--input-type` / `--output-type`

Useful for files with no extension or a misleading one. Supported `--output-type` values are
`dicom`, `json`, `raw`, `png`, `jpeg`, `mpeg4`, `texture`.

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

### Edit DICOM elements with `--set`

Set one or more DICOM element values while converting by repeating `--set KEY=VALUE`. `KEY`
can be a DICOM keyword (for example, `SOPClassUID`) or a tag expression (for example,
`(0008,0016)`):

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.1 --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --set StudyDescription=Normalized
```

Use `--overwrite` to write DICOM output back to the input path — useful for in-place edits:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --overwrite
```

### Render frames

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

If `--render-fps` is omitted for `.mp4` output, `dcmnorm` uses frame-rate metadata from the
DICOM instance when available (`RecommendedDisplayFrameRate`, `CineRate`, `FrameTime`, or
`FrameTimeVector`) and falls back to 24 FPS otherwise. `.mp4` output requires `ffmpeg`
installed and available on `PATH`.

Rendering supports 1-bit, 8-bit, and 16-bit monochrome pixel data, as well as RGB data. The
render pipeline includes decompression when needed and applies modality LUT and VOI
LUT/windowing by default. Use `--no-modality-lut` and/or `--no-voi-lut` to disable those
steps, and `--window-center` / `--window-width` to override VOI windowing.

Photometric interpretations supported by rendering:

- `MONOCHROME1`
- `MONOCHROME2`
- `PALETTE COLOR`
- `RGB`

Both planar configurations are supported for RGB rendering (`PlanarConfiguration` 0 and 1).

### Export a GPU texture (`.gputex`)

`--output-type texture` (or a `.gputex` output extension) packs a frame or volume as a lossless,
GPU-upload-ready payload instead of an 8-bit windowed render: the raw `int16`/`uint16` sample
lattice (row-major, little-endian, gzip-compressed by default), plus a `rescaleSlope`/
`rescaleIntercept` pair for recovering physical values (e.g. HU). This lets a client do its own
window/level and oblique reslicing in a GPU shader instead of round-tripping to the server per
interaction - see `dicom_io::texture_export`'s module doc for the full rationale.

Every export writes two files: `OUTPUT` (the payload bytes) and `OUTPUT.json` (a `TextureMeta`
sidecar - dimensions, physical spacing/origin/orientation, rescale slope/intercept, a default
window/level, and whether/how the payload is compressed). Always read `compression` from the
sidecar rather than assuming gzip - `--texture-compression none` disables it per export.

Export a single frame as a depth-1 texture:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm frame.gputex
```

Export a specific frame with an explicit default window, capping the longest axis at 1024 samples:

```bash
cargo run -p dcmnorm-cli -- test/files/ct.dcm frame.gputex --render-frame 1 --window-center 40 --window-width 400 --texture-max-dim 1024
```

Export a whole CT/MR series as one volume texture (its own native voxel lattice, not a reformatted
plane - `--mpr`'s plane/depth/spacing flags don't apply here, only the volume-building/plane-value
is needed to select `--mpr`'s multi-file mode):

```bash
dcmnorm --mpr axial series_dir/*.dcm volume.gputex
```

Skip gzip compression (e.g. when the transport already compresses, such as a websocket permessage-deflate connection):

```bash
dcmnorm --mpr axial series_dir/*.dcm volume.gputex --texture-compression none
```

### Render a Multiplanar Reformation (MPR)

MPR builds one 3D volume from multiple DICOM slice files (sharing a parallel stack - e.g. one
CT/MR series) and reformats it into either one 2D plane or a STACK of slices, honoring each
slice's real `ImagePositionPatient`/`ImageOrientationPatient`/`PixelSpacing`/`SliceThickness`
rather than just stacking images. Depending on `OUTPUT`'s extension, the result is a rendered
2D image (or a numbered series of them), a proper multi-instance DICOM series, or a single
whole-volume NIfTI/NRRD file - see [Reformatting a STACK of
slices](#reformatting-a-stack-of-slices---mpr-depth-ranges) below. This is the same
`dicom_io::volume` code the Node bindings use for the interactive viewer, exposed directly for
scripting/testing. The whole file set is always read and combined within this single `dcmnorm`
process - one `build_volume` call sees every slice given - so a series can never end up silently
split across separate volumes.

`--mpr` supplies its input files the same way as any other multi-file operation (see
[Batch mode](#batch-mode-with--i---stdin-paths-or-a-file-list) above) - directly as positional
arguments (shell globs work as usual) or piped via `-I`/`--stdin-paths`. Either way, `--mpr` is
what says "combine these into one volume, with the last path as OUTPUT" instead of the default
"process each independently" - its value is either a canonical view or an oblique rotation, not
both combined:

```bash
dcmnorm --mpr axial series_dir/*.dcm out.png
find series_dir -name "*.dcm" | dcmnorm -I --mpr axial out.png
```

The three canonical, patient-anatomy-aligned views:

```bash
dcmnorm --mpr coronal series_dir/*.dcm coronal.png
dcmnorm --mpr sagittal series_dir/*.dcm sagittal.png
```

An arbitrary oblique camera angle - `YAW,PITCH,ROLL` (degrees, about the patient's Z/X/Y axes
respectively), applied to the volume's own native acquisition basis:

```bash
dcmnorm --mpr 15,30,0 series_dir/*.dcm oblique.png
```

`--mpr-origin X,Y,Z` (patient/LPS millimeters) recenters the reformat; it defaults to the volume's
own physical center. For the common case of stepping along the CURRENT view's own depth axis,
`--mpr-depth MM` is more convenient than recomputing a 3D point - it offsets `--mpr-origin` (or
the default center) along the resolved plane's own normal:

```bash
dcmnorm --mpr coronal --mpr-depth -15 series_dir/*.dcm coronal-anterior.png
```

By default MPR reformats an infinitely-thin plane (one voxel thick). `--mpr-thickness MM`
reformats a thick slab instead - multiple depths spanning that many millimeters, centered on the
plane, combined per `--mpr-projection` (`mip`, the default - maximum intensity projection, the
radiology-standard way to make a thin vessel or bright structure visible across a slab even if it
only crosses the exact center plane at one point; `minip` - minimum intensity projection; or
`average`):

```bash
dcmnorm --mpr coronal --mpr-thickness 20 --mpr-projection mip series_dir/*.dcm coronal-mip.png
```

`--mpr-spacing MM` sets the physical size of one output pixel (the same in both axes, so the
reformat is never distorted even when slice spacing differs from in-plane pixel spacing); it
defaults to the volume's own smallest voxel dimension. `--output-width`/`--output-height` (shared
with normal rendering) size the output image, defaulting to the volume's own row/column count.
`--window-center`/`--window-width` apply VOI windowing exactly as they do for a normal 2D render
(for `.png`/`.jpg`/`.dcm` output - see below, VOI windowing never applies to `.nii`/`.nii.gz`/
`.nrrd`, which always carry true rescaled values).

`--mpr-origin`/`--mpr-depth`/`--mpr-spacing`/`--mpr-thickness`/`--mpr-projection` all require
`--mpr` itself (they carry no "which plane" information on their own, so using them without it is
a clear error rather than a silently-ignored flag); `--mpr-projection` additionally requires a
positive `--mpr-thickness`. MPR mode is incompatible with `--filter`/`--transfer-syntax`/`--set`/
`--remove`/`--render-all-frames`/`--render-fps`/`--scale-max-size`, and fails with a clear error
(rather than a silently wrong reformat) if the given files don't share a consistent
`ImageOrientationPatient` - e.g. a genuinely gantry-tilt-inconsistent stack or an accidentally
mixed set of series.

#### Reformatting a STACK of slices - `--mpr-depth` ranges

A bare `--mpr-depth MM` (or omitting it, default `0`) offsets a single plane, as above - exactly
one output slice. `--mpr-depth` also accepts a RANGE, which switches the whole `--mpr` invocation
from "one reformatted plane" to "a stack of slices spanning that depth range":

```bash
--mpr-depth START:END          # e.g. -20:20
--mpr-depth START:END:STEP     # explicit step, e.g. -20:20:2
--mpr-depth all                # the volume's own full extent along the plane's normal
--mpr-depth all:STEP           # full extent, explicit step
```

The step defaults to `--mpr-thickness` (contiguous, non-overlapping slabs - the natural default
when you're already asking for a thick reformat) if it's set, else `--mpr-spacing`. `all` computes
the volume's own physical extent along the resolved plane's normal by projecting its bounding box
onto that normal - correct even for an oblique (rotated) plane whose normal isn't the volume's own
acquisition axis.

What a multi-slice stack produces depends entirely on `OUTPUT`'s extension:

- **`.png`/`.jpg`/`.jpeg`**: one numbered file per slice - `OUTPUT_000001.png`,
  `OUTPUT_000002.png`, ... (the same `{stem}_{NNNNNN}.{ext}` convention `--render-all-frames`
  already uses) - each one an independent, VOI-windowed, 8-bit render exactly like a single-plane
  `--mpr` output.
- **`.dcm`/`.dicom`**: one numbered, spatially-valid DICOM file per slice (see below) sharing a
  single `SeriesInstanceUID`, so any PACS/viewer groups them as one loadable series.
- **`.nii`/`.nii.gz`/`.nrrd`**: the entire stack as ONE whole-volume file (see below).

A single depth (the default, or an explicit non-range `--mpr-depth MM`) still writes exactly one
file at `OUTPUT` for every extension, including `.dcm`/`.nii`/`.nrrd`.

```bash
# 41 coronal slices, 2mm apart, as a numbered PNG stack:
dcmnorm --mpr coronal --mpr-depth -40:40:2 series_dir/*.dcm coronal.png

# The same slices as a proper multi-instance DICOM series:
dcmnorm --mpr coronal --mpr-depth -40:40:2 series_dir/*.dcm coronal.dcm

# The whole volume, reformatted to a 1mm-isotropic coronal-oriented NIfTI:
dcmnorm --mpr coronal --mpr-depth all --mpr-spacing 1 series_dir/*.dcm coronal.nii.gz
```

`--output-type` is not valid with `.nii`/`.nii.gz`/`.nrrd`/`.dcm` output - the format is
determined entirely by `OUTPUT`'s extension for these.

#### Whole-volume export - NIfTI and NRRD

`.nii`/`.nii.gz` (gzip-compressed) and `.nrrd` write the ENTIRE reformatted stack as a single
volumetric file - float32 voxels carrying true rescaled values (e.g. Hounsfield units for CT),
never VOI-windowed or 8-bit-encoded, so the export is suitable for real volumetric analysis (3D
Slicer, ITK-SNAP, FSL, etc.), not just visual inspection. Neither format is available via an
existing well-maintained Rust crate, so both are written directly by `dcmnorm`:

- **NIfTI-1** (`.nii`/`.nii.gz`) uses `sform` for orientation. NIfTI's coordinate convention is
  RAS+ (Right/Anterior/Superior), while DICOM (and `dcmnorm`'s own geometry) is LPS+
  (Left/Posterior/Superior) - every direction vector and the origin get their X/Y components
  negated on export, the same convention `dcm2niix`/`nibabel` use for DICOM-derived NIfTI files.
- **NRRD** (`.nrrd`) names its space directly (`space: left-posterior-superior`), so no axis flip
  is needed - arguably the more direct/less error-prone choice for a DICOM-derived export.

#### Multi-file DICOM series output

`.dcm`/`.dicom` output wraps each reformatted slice in a standalone, spatially-valid DICOM object
using **Multi-frame Grayscale Word Secondary Capture Image Storage**
(`1.2.840.10008.5.1.4.1.1.7.3`, `NumberOfFrames=1` per file) - not plain "Secondary Capture Image
Storage", which is 8-bit-only per its IOD and can't carry signed 16-bit rescaled values. Each
slice's `ImagePositionPatient`/`ImageOrientationPatient`/`PixelSpacing`/`SliceThickness` reflect
its actual reformatted geometry (not copied from the source series), so the output is a genuinely
reconstructable spatial series, not just a flat picture with DICOM headers bolted on. `PixelData`
is stored as signed 16-bit with `RescaleSlope=1`/`RescaleIntercept=0` - the stored value IS the
physical value - so any DICOM viewer can window it however it likes, rather than getting a
pre-baked 8-bit render. Patient/study-identifying attributes (`PatientName`, `PatientID`,
`StudyInstanceUID`, `Modality`, etc.) are copied best-effort from the first input file, so the
derived series stays associated with its source study; `SeriesDescription` is fixed to
`"MPR Reformat"` and `SeriesNumber` to `9901`, deliberately unlikely to collide with a real
acquired series. `SeriesInstanceUID` is generated once per `--mpr` invocation and shared across
every file in the output stack; `SOPInstanceUID`/`StudyInstanceUID` (when not copied from the
source) use the UUID-derived DICOM UID scheme (PS3.5 Annex B, `2.25.<uuid>`), needing no
registered organization root.

#### Overlay planes

DICOM overlay planes (group `60xx`, up to 16 per instance: `6000,eeee` .. `601E,eeee`) are
composited onto the rendered image. Both encodings defined by the standard are supported:

- **Distinct `OverlayData`** (`60xx,3000`, current standard): `OverlayBitsAllocated`=1, a
  separately-stored 1-bit-per-pixel bitmap, packed LSB-first.
- **Embedded in `PixelData`** (legacy CR/DX): `OverlayBitsAllocated` equals the image's own
  `BitsAllocated`, and `OverlayBitPosition` names a specific high bit unused by `BitsStored`.

If an instance has one or more overlays, the first available overlay (ascending by DICOM group)
renders by default:

```bash
cargo run -p dcmnorm-cli -- test/files/overlay.dcm out.png
```

Select a different overlay by its 0-based index (ordinal among the overlays present, not the raw
DICOM group), or disable overlay rendering entirely:

```bash
cargo run -p dcmnorm-cli -- test/files/overlay_multi.dcm out.png --overlay-index 1
cargo run -p dcmnorm-cli -- test/files/overlay.dcm out.png --no-overlays
```

Overlay pixels render in a fill color, `R,G,B` (0-255 each) or `#RRGGBB` hex, defaulting to
green (`0,255,0`):

```bash
cargo run -p dcmnorm-cli -- test/files/overlay.dcm out.png --overlay-color 255,0,0
```

`--overlay-index`/`--overlay-color` require overlays to be enabled (they conflict with
`--no-overlays`), and an `--overlay-index` beyond the number of overlays present is an error
rather than being silently clamped.

Three small synthetic (no PHI, not derived from any real study) fixtures exercise overlay
rendering: `test/files/overlay.dcm` (one overlay, distinct `OverlayData`), `test/files/
overlay_multi.dcm` (two overlays, distinct `OverlayData`), and `test/files/overlay_embedded.dcm`
(one overlay, legacy embedded-in-`PixelData` encoding).

Use `--verbose` to print render/conversion diagnostics — without it, external tool output
such as `ffmpeg` is suppressed unless an error occurs. For stage-by-stage performance timing,
set `DCMNORM_PERF=1` (or `true`/`yes`/`on`):

```bash
DCMNORM_PERF=1 dcmnorm test/files/mr.dcm out.jpg --output-width 920 --output-height 758

# or with an explicit render format for a file without a recognized extension
DCMNORM_PERF=1 dcmnorm test/files/mr.dcm output.img --output-type jpeg --output-width 920 --output-height 758
```

### Batch mode with `-I` / `--stdin-paths` or a file list

`dcmnorm` has two interchangeable ways to run the same options across multiple input files
instead of just one - pick whichever fits how the file list is produced. Both apply the same
options to every path; errors for individual files are printed to stderr with the filename, and
`dcmnorm` exits non-zero if any file fails. (`--mpr` repurposes the same file list to mean
something different - combine every file into one volume instead of processing each
independently - see [Render a Multiplanar Reformation (MPR)](#render-a-multiplanar-reformation-mpr).)

Give 3 or more files directly as positional arguments - no special flag needed, shell globs work
as usual since the shell expands them into separate arguments before `dcmnorm` ever sees them:

```bash
dcmnorm *.dcm
```

(2 or fewer positional arguments are always `[INPUT] [OUTPUT]`, per the single-file convention
above - batch mode only kicks in at 3+, since that shape was otherwise always a "too many
arguments" error.)

Or pipe input paths from stdin, one path per line, via `-I`/`--stdin-paths` - best for a file list
produced by another command (`find`, a database query, ...) or too large for a shell command line:

```bash
find . -name "*.dcm" | dcmnorm -I
```

`--set` also applies in batch mode, and combines with `--overwrite` to update each file in place:

```bash
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --overwrite
```

To emit `file://` `BulkDataURI` values in batch mode, also pass `--bulk-data-source` without a value:

```bash
find . -name "*.dcm" | dcmnorm -I --bulk-data uri --bulk-data-source
```

### Transcode and inspect transfer syntaxes

Transcode a DICOM file to Explicit VR Big Endian:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.2
```

List the transfer syntaxes known to the current build and whether dataset read/write and
pixel decode/encode are available:

```bash
cargo run -p dcmnorm-cli -- --list-transfer-syntaxes
```

Transfer-syntax support is build-specific. The default build in this repository enables the
MPEG and JPEG-LS codec features in addition to the DICOM library support that is available
without extra native imaging libraries:

- native uncompressed syntaxes
- deflated dataset syntaxes
- encapsulated uncompressed pixel data
- MPEG transfer syntax support via FFmpeg-backed build integration
- JPEG baseline decode/encode
- JPEG extended and JPEG lossless decode-only
- JPEG-LS transfer syntax support via CharLS-backed build integration
- JPEG 2000 decode-only
- RLE lossless decode-only

Transfer syntaxes which the current build cannot encode or decode are reported explicitly by
`--list-transfer-syntaxes` and by transcoding errors.

### JPEG 2000 codec selection

`dcmnorm` checks `LD_LIBRARY_PATH` at runtime for Kakadu libraries (`libkdu*.so`). Kakadu use
is FFI-only (Rust → C++ interop), not CLI-based, and requires the `kakadu-ffi` build feature
(see [Kakadu FFI](#kakadu-ffi-jpeg-2000)). If Kakadu FFI is not enabled or Kakadu is
unavailable, the OpenJPEG-based path remains in use. `--jpeg2000-codec`/`DCMNORM_JPEG2000_CODEC`
select between `auto`, `openjpeg`, and `kakadu` at runtime.

## dcmtalk CLI Usage

`dcmtalk` is a DIMSE (DICOM network) client/server covering the same ground as [dcmtk](https://dcmtk.org/)'s
`echoscu`/`storescu`/`findscu`/`movescu`/`storescp`, built on this repository's own DICOM
Upper Layer implementation (no [dcmtk](https://dcmtk.org/) dependency).

Get the full option reference from either help form, for the tool itself or any subcommand:

```bash
dcmtalk -h
dcmtalk --help
dcmtalk echoscu --help
```

Command shape:

```text
dcmtalk <SUBCOMMAND> [OPTIONS] <ARGS>
```

Every SCU subcommand (`echoscu`/`storescu`/`findscu`/`movescu`) shares:

- `<DESTINATION>`: peer address as `HOST:PORT`
- `-a`, `--calling-aet <AE>`: our AE title (default `DCMTALK`)
- `-c`, `--called-aet <AE>`: the peer's AE title, if it requires one to match
- `--timeout <SECONDS>`: absolute timeout for the whole operation (connect through release)
- `-v`, `--verbose`: log association negotiation, presentation contexts, and each DIMSE command/response to stderr

### C-ECHO: verify connectivity

```bash
dcmtalk echoscu somepacs.example.com:11112
dcmtalk echoscu --verbose somepacs.example.com:11112
```

### C-STORE: send files

Sends one or more DICOM files; directories are scanned recursively. Files are sent under
their native transfer syntax when the peer accepts it, transcoded to Explicit/Implicit VR
Little Endian otherwise (unless `--never-transcode`):

```bash
dcmtalk storescu somepacs.example.com:11112 test/files/dx.dcm
dcmtalk storescu somepacs.example.com:11112 test/files/ --max-pdu 65536
```

### C-FIND: query studies

Query keys are DICOM keywords as `KEY=VALUE` (match) or bare `KEY` (return key, universal
match), repeatable. Matches print as one DICOM JSON line per study to stdout:

```bash
dcmtalk findscu somepacs.example.com:11112 -k PatientID=12345 -k StudyDate
```

### C-MOVE: retrieve a study

Asks the peer to push a study to another AE title it already knows how to reach:

```bash
dcmtalk movescu somepacs.example.com:11112 MY_STORE_AE 1.2.840.113619.2.55.3.604688119.971.1600000000.123
```

### storescp: receive files

Listens for inbound associations and writes C-STORE'd instances under `--cache-path` as
`S_<StudyInstanceUID>/<Modality>_<SOPInstanceUID>.dcm`. C-FIND/C-MOVE requests are answered
"unable to process" — this is a receive-only SCP, not a full PACS:

```bash
dcmtalk storescp 11112 --ae-title MY_STORE_AE --cache-path ./received
```

Use port `0` to bind an ephemeral port (useful for tests):

```bash
dcmtalk storescp 0 --verbose
```

## Thanks

This workspace is built on the [DICOM-rs](https://github.com/Enet4/dicom-rs) project's Rust
DICOM crates (`dicom-core`, `dicom-object`, `dicom-ul`, and others) for DICOM parsing, encoding,
and the Upper Layer/association protocol.

`exec/dcmtalk`'s subcommands (`echoscu`/`storescu`/`findscu`/`movescu`/`storescp`) follow the
naming and behavior established by [DCMTK](https://dcmtk.org/), the long-standing reference
DICOM toolkit.

Thanks to both projects and their maintainers.
