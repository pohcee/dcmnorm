# Plan: dcmnorm as a Claude plugin in Anthropic's directory

Status: written 2026-11-11, research from Anthropic's docs as of that date (recheck the submission checklist before
submitting, it changes). Built on 2026-11-12, see "As built" below; the sections after it are the original plan.

## As built (2026-11-12)

Done:

- **FFmpeg is no longer built by default.** The video transfer syntaxes (MPEG-2, MPEG-4 AVC/H.264, HEVC) go through
  the `ffmpeg` command on PATH when FFmpeg isn't linked in (`src/dicom_io/mpeg.rs`), with an install hint when it's
  missing and `ffmpeg` as the engine in `--list-transfer-syntaxes`. `ffmpeg-codec` left the default features; the
  bindings keep FFmpeg linked in. This made the CLI builds plain `cargo build`s (C/C++ toolchain and CMake only)
  on every platform, which is what made the five-platform wheel matrix cheap. A clean release build of the CLIs
  takes about a minute.
- **CLI wheels**: `packaging/` (see its README). One `dcmnorm` wheel per platform holds both binaries
  (`.data/scripts/`), built from the same binaries as the release archives. Linux builds run in manylinux_2_28
  (glibc 2.28+). Verified locally for linux-x86_64: `twine check --strict`, pip and `uv tool install`, Debian
  bullseye.
- **Workflows consolidated** into `.github/workflows/release.yml` (Build and release). PRs and main get tests plus
  every CLI build. A tag publishes the GitHub release (now all 5 platforms, plus `.deb` for both Linux
  architectures), PyPI (Trusted Publishing, gated on the `PYPI_PUBLISH` variable) and the bindings commit, only
  after the tests and builds pass. `semver-tag.yml` was removed: it tagged without bumping the Cargo versions, and
  its token-pushed tags didn't trigger the other workflows. `build-bindings.yml` was merged in.
  `deploy-test-website.yml` follows the new workflow name and deploys only after release-tag runs.
- **Plugin**: `plugin/` (`.claude-plugin/plugin.json`, the skill moved to `plugin/skills/dcmnorm/`, README), plus a
  marketplace at `.claude-plugin/marketplace.json` (`/plugin marketplace add pohcee/dcmnorm`). The skill has a
  Setup section that installs the pinned version from PyPI. `scripts/check-versions.py` keeps the pins in step
  with Cargo (CI checks it, and `release-tag.sh` sets it). `claude plugin validate --strict` passes. The install
  scripts take the skill from the new path and fall back to `skills/` for older tags.
- **No SessionStart hook.** The skill's Setup section covers a missing binary when it's actually needed; a hook
  would add a line to every session and is a review risk.

Left (owner steps):

1. A listing icon: a square PNG, 512 to 2048 px, at `plugin/.claude-plugin/icon.png`. It is the only failure under
   `claude plugin validate --profile directory --strict plugin`, and it is fixed at the first save or submission
   in the developer portal.
2. PyPI Trusted Publishing setup (`packaging/README.md`), then a release tag (`./scripts/release-tag.sh minor`).
   The skill's pinned version must exist on PyPI before the plugin is submitted.
3. Submit `plugin/` at claude.ai/directory/manage (section 5 below), tracking a release tag.

## Goal

List dcmnorm in Anthropic's public directory (claude.ai/directory) so a user can install it from the storefront
and have Claude read, edit, anonymize, transcode and render DICOM (`dcmnorm`) and talk DIMSE (`dcmtalk`) through the
existing skill in `skills/dcmnorm/SKILL.md`, in Claude Code and in claude.ai / Cowork.

## Decisions (and why)

1. **Ship a plugin, not a standalone skill.** The directory doesn't take skills on their own ("Skills aren't a
   submission type on their own. Put them in a bundle."). It takes plugin bundles and remote MCP connectors. A
   connector must be a remote HTTPS service, which doesn't fit a local CLI.
2. **Don't put the binary in the plugin.** The directory checks only allow text and images. Compiled executables are
   held for a reviewer, non-image files must be under 256 KiB, and the limit is 512 files. A top-level `bin/` also
   can't be installed on claude.ai chat or Cowork. The plugin holds the skill (plus an optional check-only hook).
3. **Distribute the native CLI through PyPI as wheels that contain the binaries** (maturin `bindings = "bin"`, the
   way ruff and uv ship). This doesn't use the Python binding: pip only puts the compiled `dcmnorm` and `dcmtalk`
   executables on PATH, and the skill keeps calling the native CLI.
   - Install in seconds with no compiler: `uv tool install dcmnorm==X.Y.Z` or `pip install dcmnorm==X.Y.Z`.
   - pip picks the right wheel for the OS and CPU.
   - `pypi.org` and `files.pythonhosted.org` are on the claude.ai sandbox's allowed list.
     `objects.githubusercontent.com`, where GitHub release downloads come from, isn't on the published list.
4. **Don't use crates.io as the install route.** `cargo install` builds from source: a Rust toolchain plus clang,
   libclang, cmake, nasm and pkg-config, and the default `ffmpeg-codec` feature git-clones and builds FFmpeg. That
   takes minutes, fails in the sandbox, and gives every user a different binary. Publishing the library crate to
   crates.io for Rust developers is fine as an extra.
5. **The skill installs the binary.** The directory has no install step, so `SKILL.md` gets a Setup section that
   checks `dcmnorm --version` and installs the pinned version when it's missing or old. In Claude Code the user
   approves that command like any other. In the sandbox it's a pip install.
6. **The optional Claude Code hook only checks and prints a hint.** A `SessionStart` hook that downloads and runs a
   binary may be held at review, and the docs don't say how review treats one. Hooks don't run on claude.ai, so the
   skill's Setup section is still the main path.

## Work

### 1. CLI wheels on PyPI

- **Package:** a maturin project with `bindings = "bin"` that packages the `dcmnorm` and `dcmtalk` binaries.
  - Name the CLI package `dcmnorm`. Publish the existing Python library under another name (e.g. `dcmnorm-py`), or
    ship one package with both the module and the binaries. Pick one and record it here.
  - Keep the same default features as the release binaries, with FFmpeg statically linked, so the wheels depend on no
    system libraries.
- **Targets:**

  | Wheel | Build notes |
  |---|---|
  | manylinux x86_64 | FFmpeg built inside the manylinux container (check whether the existing `build-in-docker.sh` path already does this) |
  | manylinux aarch64 | same as x86_64 |
  | macOS arm64 | static FFmpeg build on macOS runners |
  | macOS x86_64 | static FFmpeg build on macOS runners |
  | Windows x64 | reuse the Node addon's vcpkg `x64-windows-static-md` FFmpeg with `ffmpeg-codec-linked` |

  Today `release.yml` builds only Linux x86_64 on ubuntu-24.04. Most Claude Code users are on macOS, so the macOS
  wheels matter before a wide listing.
- **CI:** a wheel job in `release.yml`, run on the same tags as the CLI release.
  - Run `auditwheel` / `delocate` checks.
  - Smoke-test each wheel: install it in a clean venv and run `dcmnorm --version`, `dcmtalk --version`, plus one
    convert and one render on a test file.
- **PyPI publishing:** use PyPI Trusted Publishing (OIDC from GitHub Actions), so no API token gets stored.
- **Check:** `uv tool install dcmnorm==<ver>` works on a clean Linux, macOS and Windows machine with no Rust
  toolchain, and `pip install` works in a claude.ai code-execution sandbox.

### 2. Skill changes (`skills/dcmnorm/SKILL.md`)

- **Setup section** at the top, run before first use:
  - Run `dcmnorm --version` and `dcmtalk --version`.
  - If missing or older than the pinned version, run `uv tool install dcmnorm==<ver>`. Fall back to
    `pip install dcmnorm==<ver>` when uv isn't there, which is the usual case in the claude.ai sandbox.
  - Then re-check.
  - If there's no network (Enterprise orgs can turn sandbox egress off, and the Claude API sandbox has none), tell
    the user plainly that dcmnorm can't be installed there. Don't fall back to anything else.
- **Wording:** replace "is an installed CLI" with wording that points to Setup.
- **Version pin:** keep the version in the skill in step with the release. Release CI can do this, or a test can
  fail when the skill's version and `Cargo.toml` disagree.
- **Description:** keep the frontmatter `description` trigger-rich, since it decides when the skill loads. It should
  cover both dcmnorm (files) and dcmtalk (network: C-ECHO/C-STORE/C-FIND/C-MOVE, storage SCP).

### 3. Plugin manifest

- **Manifest:** add `.claude-plugin/plugin.json` at the repo root (or a `plugin/` subdirectory, if the checker
  doesn't like a Rust repo root as the plugin root).
  - Fields: name (kebab-case, e.g. `dcmnorm`; not `claude`, `anthropic`, `official`, `plugin`, `mcp` or `test`, and
    not a look-alike of an existing listing), description, version, `license: "Apache-2.0"`, homepage/repository,
    and the skills path pointing at `skills/dcmnorm`.
- **Optional hook:** a `hooks` entry with a `SessionStart` hook that runs a small text script.
  - It runs `command -v dcmnorm`. If missing, it prints one line: "dcmnorm not found; install with
    `uv tool install dcmnorm==<ver>`".
  - Nothing is downloaded.
- **Constraints:** no credentials anywhere in the plugin. Any launcher (`npx`/`uvx`) must be pinned to an exact
  version. Only text and images go in the plugin directory.
- **Check:** `claude --plugin-dir <path>` loads the skill, and in a session without dcmnorm installed, Claude
  installs it through the Setup section and completes a task such as "anonymize this .dcm and render it to PNG".

### 4. Own marketplace (optional, can go alongside the directory listing)

- **Marketplace file:** add `.claude-plugin/marketplace.json` so Claude Code users can run
  `/plugin marketplace add pohcee/dcmnorm` with no Anthropic review.
- **Binary:** that marketplace may fetch the binary at install time, from a zip URL pinned by sha256 or from a
  command source that picks the OS- and CPU-specific build.
- **Caveat:** neither of these is allowed in the directory listing.
- **Name:** don't use a reserved marketplace name (`claude-plugins-official`, `claude-community`, ...).

### 5. Submit to the directory

1. **Repo:** must be public (it is), with a LICENSE (Apache 2.0, present) and a README of at least 40 words that
   covers what the plugin does and how the binary gets installed.
2. **Submission:** go to `claude.ai/directory/manage` → Submit new → Plugin bundle. This needs a paid account, and
   on Team/Enterprise an Owner or someone the Owner gives the Directory permission.
3. **Steps:** source (repo URL, plugin path, a branch or tag to track), validation, listing details, data-handling
   questions, then compliance (contact email and four acknowledgements).
4. **Data handling:** dcmnorm works on patient images, so state that everything runs locally and nothing goes to
   any service, and that de-identification is the user's responsibility to verify.
5. **Review:** automated validation and a security scan on each version, plus a human review of a new listing. The
   docs don't say how long review takes. The limit is 10 submissions per organization per 24 hours.

The docs also mention a community marketplace (`anthropics/claude-plugins-community`, read-only mirror, PRs closed)
with the submission link `clau.de/plugin-directory-submission`. Where that link goes wasn't confirmed.

## Open questions

- PyPI package name: is `dcmnorm` free on PyPI? If not, pick a name and use it everywhere.
- Is a binary bundled in a skill zip allowed to run in the claude.ai sandbox, and what OS and CPU does the sandbox
  use? The docs don't say. Test with a pip install of the Linux wheel.
- Whether the Python library and the CLI should share one PyPI package.

## References

- Submit a plugin: https://claude.com/docs/plugins/submit.md
- Pre-submission checklist: https://claude.com/docs/plugins/pre-submission-checklist.md
- Publish to the directory: https://claude.com/docs/directory/publish.md
- Plugin feature support across platforms: https://claude.com/docs/plugins/platform-support.md
- Plugin components (`bin/`, `${CLAUDE_PLUGIN_ROOT}`, `${CLAUDE_PLUGIN_DATA}`, hooks): https://code.claude.com/docs/en/plugins/components.md
- Marketplace reference (archive/command sources): https://code.claude.com/docs/en/plugins/marketplace-reference.md
- Anthropic's marketplaces: https://code.claude.com/docs/en/plugins/anthropic-marketplaces.md
- Sandbox network access: https://support.claude.com/en/articles/12111783-create-and-edit-files-with-claude
