# Installing dcmnorm

## Install

Pre-built binaries are published for Linux (x86_64, aarch64; glibc 2.28+), macOS (Apple silicon,
Intel) and Windows (x86_64), on PyPI and as GitHub release archives.

### From PyPI

```bash
uv tool install dcmnorm        # or: pipx install dcmnorm / pip install dcmnorm
```

This installs the native `dcmnorm` and `dcmtalk` binaries for your platform; the CLIs don't run
in Python. On CPython 3.9+ the same package also provides `import dcmnorm`, the in-process
binding (see [bindings/python](../bindings/python/)).

### Agent plugin (Claude, Gemini CLI, Antigravity)

The [`plugin/`](../plugin/) directory is one plugin with the dcmnorm skill, with a manifest for each of
Claude, Gemini CLI and Google Antigravity: the agent uses the CLIs for DICOM tasks and installs them
from PyPI on first use.

```bash
# Claude Code
/plugin marketplace add pohcee/dcmnorm
/plugin install dcmnorm@dcmnorm

# Gemini CLI (from the latest release; --ref claude-plugin installs from the plugin branch)
gemini extensions install https://github.com/pohcee/dcmnorm

# Any agent that reads Agent Skills (Codex, Cursor, Copilot and others), via skills.sh
npx skills add pohcee/dcmnorm

# Antigravity: import the Gemini CLI extension, or install a checkout of the plugin
agy plugin import gemini
git clone --branch claude-plugin https://github.com/pohcee/dcmnorm.git dcmnorm-plugin
agy plugin install "$PWD/dcmnorm-plugin"
```

The skill itself is also at [`skills/dcmnorm/`](../skills/dcmnorm/), a copy of the plugin's (checked in CI),
where skill indexers look. The `claude-plugin` branch holds just `plugin/` and is rebuilt on every release; each release also
attaches it as `<platform>.dcmnorm.tar.gz`, which is what Gemini CLI installs. The install scripts
below also put the skill into Claude Code, Gemini CLI and Codex CLI.

### Video transfer syntaxes need `ffmpeg`

MPEG-2, MPEG-4 AVC/H.264 and HEVC pixel data and `.mp4`/`.mov` output use the `ffmpeg` command;
install FFmpeg for those (see [Video transfer syntaxes](cli.md#video-transfer-syntaxes)). Everything
else works without it.

### From source with Cargo

From crates.io (needs a C/C++ toolchain and CMake; the binaries are named `dcmnorm` and `dcmtalk`):

```bash
cargo install dcmnorm-cli dcmtalk
```

Or from a checkout:

```bash
cargo install --path exec/dcmnorm
```

To install every CLI under `exec/` with one command, use the helper script instead:

```bash
./scripts/install-source.sh
```

This script auto-detects Kakadu headers/libraries and enables `kakadu-ffi` when available,
and checks for the build prerequisites listed under [Build](building.md#build) before invoking Cargo. If it detects Claude Code and/or
Gemini CLI on the machine (an existing `~/.claude` and/or `~/.gemini` directory), it also
installs the [`plugin/skills/dcmnorm`](../plugin/skills/dcmnorm/) skill to `~/.claude/skills/dcmnorm` and/or
`~/.gemini/skills/dcmnorm` respectively — set `DCMNORM_SKIP_SKILL=1` to skip this, or
`CLAUDE_SKILLS_DIR`/`GEMINI_SKILLS_DIR` to install elsewhere (and to force the install even
without the corresponding `~/.claude`/`~/.gemini` directory).

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

This script also installs the `dcmnorm` skill (downloaded from this repo's
`plugin/skills/dcmnorm/SKILL.md`) for any of Claude Code, Gemini CLI, or Codex CLI it detects on the
machine — same `DCMNORM_SKIP_SKILL`/`CLAUDE_SKILLS_DIR`/`GEMINI_SKILLS_DIR`/`CODEX_SKILLS_DIR`
overrides as above.

On Debian/Ubuntu (`linux-x86_64` or `linux-aarch64`), pass `--deb` to install a downloaded `.deb` package via
`apt install` instead of copying binaries into `INSTALL_DIR` — this resolves runtime dependencies
(`ffmpeg`, `ca-certificates`) automatically and needs root (the script `sudo`s automatically if not
already running as root):

```bash
./scripts/install-release.sh --deb
./scripts/install-release.sh 0.2.1 --deb
```

## Docker

This repository includes a multi-stage Dockerfile that builds `dcmnorm` and `dcmtalk` in a
toolchain stage and copies only the release binaries into a slim runtime stage. The final
runtime image installs `ca-certificates`, `ffmpeg` (video transfer syntaxes and `.mp4` output),
and `libstdc++6`; build-only dependencies (`build-essential`, `cmake`, `pkg-config`) stay in the
builder stage.

Kakadu is not included in the image — see [JPEG 2000 codec selection](cli.md#jpeg-2000-codec-selection)
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
