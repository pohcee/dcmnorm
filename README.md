# dcmnorm

Rust workspace for reading, writing, transcoding, and converting DICOM data.

This repository contains:

- `dcmnorm`: a library crate with DICOM file, memory, and JSON conversion helpers
- `exec/dcmnorm`: a CLI for converting between DICOM, transcoded DICOM, and JSON

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

## Build The CLI

Build only `dcmnorm`:

```bash
cargo build -p dcmnorm-cli
```

Build the CLI in release mode:

```bash
cargo build -p dcmnorm-cli --release
```

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
- JSON input + DICOM output runs JSON to DICOM
- JSON to DICOM requires an output path

Transcode a DICOM file to Explicit VR Big Endian:

```bash
cargo run -p dcmnorm-cli -- test/files/dx.dcm out.dcm --transfer-syntax 1.2.840.10008.1.2.2
```

List the transfer syntaxes known to the current build and whether dataset read/write and pixel decode/encode are available:

```bash
cargo run -p dcmnorm-cli -- --list-transfer-syntaxes
```

Transfer-syntax support is build-specific. This workspace enables the DICOM library support that is available without extra native imaging libraries first:

- native uncompressed syntaxes
- deflated dataset syntaxes
- encapsulated uncompressed pixel data
- JPEG baseline decode/encode
- JPEG extended and JPEG lossless decode-only
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
