# Building and testing

## Workspace Layout

```text
.
├── Cargo.toml
├── src/               # dcmnorm library crate
├── exec/
│   ├── dcmnorm/       # dcmnorm-cli package (the `dcmnorm` binary)
│   └── dcmtalk/       # dcmtalk package (the `dcmtalk` binary)
├── crates/            # in-house library crates (DICOM core/object/encoding/transcode/...,
│                      # dcmnorm-jpeg2000: OpenJPEG + vendored OpenHTJ2K)
├── docs/              # design notes and evaluations
├── bindings/
│   ├── node/          # napi-rs bindings (npm: dcmnorm, @dcmnorm/<platform>)
│   ├── python/        # PyO3 bindings (PyPI: dcmnorm, with the CLIs)
│   └── java/          # jni bindings (Maven: com.pohcee:dcmnorm + -natives)
├── scripts/           # install / release helper scripts
└── test/
    └── files/         # sample DICOM fixtures used by docs and tests
```

## Build

### Prerequisites

The default build (the library and both CLIs) needs only a C/C++ toolchain. On Debian
or Ubuntu:

- `build-essential`
- `pkg-config`

FFmpeg is not built or linked by default; see [Video transfer syntaxes](cli.md#video-transfer-syntaxes).
The bindings (`bindings/node`, `bindings/python`, `bindings/java`) and any build with
`--features ffmpeg-codec` compile FFmpeg from source and link it in, which also needs `clang`,
`libclang-dev`, `nasm` and `git`, and takes much longer.

The vendored OpenHTJ2K (`crates/dcmnorm-jpeg2000`) needs a C++17 compiler, plus `ar` and
`objcopy` for its dual-variant x86-64 build. All three come with `build-essential`. Without
`objcopy`, the build falls back to a single portable variant and prints a cargo warning.

Example install command:

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config
# only for the bindings or --features ffmpeg-codec:
sudo apt-get install -y clang libclang-dev nasm git
```

### Building the workspace

```bash
# whole workspace, debug
cargo build --workspace

# whole workspace, release
cargo build --workspace --release

# without the default JPEG-LS / JPEG XL / JPEG 2000 encode codec features
cargo build --workspace --no-default-features

# with FFmpeg linked in (built from source) instead of the `ffmpeg` command
cargo build -p dcmnorm-cli --release --features dcmnorm/ffmpeg-codec
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

By default, classic JPEG 2000 decoding uses the bundled OpenJPEG path (HTJ2K always uses the
bundled OpenHTJ2K). To enable the optional Kakadu FFI bridge for classic JPEG 2000 instead:

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

See [JPEG 2000 codec selection](cli.md#jpeg-2000-codec-selection) for the corresponding runtime behavior.

## Test

```bash
cargo test --workspace
```
