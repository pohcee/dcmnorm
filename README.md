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
dcmnorm [OPTIONS] [INPUT] [OUTPUT]
```

`dcmnorm` infers the conversion direction from the input and output file types:

- DICOM input + JSON output, or no output, runs DICOM to JSON
- DICOM input + DICOM output with `--transfer-syntax <UID>` runs DICOM to DICOM transcoding
- DICOM input + `.png` / `.jpg` / `.jpeg` / `.raw` output runs DICOM frame rendering
- JSON input + DICOM output runs JSON to DICOM (requires an output path)

### Options reference

Positional arguments:

- `[INPUT]`: input DICOM or JSON file
- `[OUTPUT]`: output DICOM, JSON, or rendered file

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
- `--output-type <dicom|json|raw|png|jpeg|mpeg4>`

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
`dicom`, `json`, `raw`, `png`, `jpeg`, `mpeg4`.

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

### Piped / batch mode with `-I` / `--stdin-paths`

Pipe input paths from stdin, one path per line. The same options apply to every path; errors
for individual files are printed to stderr with the filename, and `dcmnorm` exits non-zero if
any file fails:

```bash
find . -name "*.dcm" | dcmnorm -I
```

`--set` also applies in piped mode, and combines with `--overwrite` to update each file in place:

```bash
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2
find . -name "*.dcm" | dcmnorm -I --set SOPClassUID=1.2.840.10008.5.1.4.1.1.2 --overwrite
```

To emit `file://` `BulkDataURI` values in piped mode, also pass `--bulk-data-source` without a value:

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
