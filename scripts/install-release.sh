#!/bin/sh
set -eu

INSTALL_DIR="${INSTALL_DIR:-${HOME}/.cargo/bin}"
DEFAULT_VERSION="latest"
MIN_SUPPORTED_VERSION="0.1.3"
GITHUB_REPO="pohcee/dcmnorm"
PLATFORM="linux-x86_64"
INSTALL_METHOD="tarball"

# Parse command line arguments: VERSION is the one positional argument, --deb/-h/--help are flags.
VERSION="$DEFAULT_VERSION"
for ARG in "$@"; do
    case "$ARG" in
        -h|--help)
            HELP_REQUESTED=1
            ;;
        --deb)
            INSTALL_METHOD="deb"
            ;;
        *)
            VERSION="$ARG"
            ;;
    esac
done

# Detect platform if needed
detect_platform() {
    local os=$(uname -s | tr '[:upper:]' '[:lower:]')
    local arch=$(uname -m)
    
    case "$os" in
        linux)
            case "$arch" in
                x86_64) echo "linux-x86_64" ;;
                aarch64) echo "linux-aarch64" ;;
                armv7l) echo "linux-armv7" ;;
                *)
                    echo "Error: Unsupported architecture: $arch" >&2
                    exit 1
                    ;;
            esac
            ;;
        darwin)
            case "$arch" in
                x86_64) echo "macos-x86_64" ;;
                arm64) echo "macos-aarch64" ;;
                *)
                    echo "Error: Unsupported architecture: $arch" >&2
                    exit 1
                    ;;
            esac
            ;;
        *)
            echo "Error: Unsupported platform: $os" >&2
            exit 1
            ;;
    esac
}

version_lt() {
    # Returns 0 (true) if $1 < $2, otherwise 1. Pure POSIX; avoids GNU-only `sort -V`.
    awk -v a="$1" -v b="$2" '
        BEGIN {
            n = split(a, A, ".")
            split(b, B, ".")
            for (i = 1; i <= n; i++) {
                if ((A[i]+0) < (B[i]+0)) exit 0
                if ((A[i]+0) > (B[i]+0)) exit 1
            }
            exit 1
        }
    '
}

print_usage() {
    cat << EOF
Usage: $0 [VERSION] [--deb]

Downloads and installs dcmnorm and dcmtalk from GitHub releases.

By default, downloads the platform tarball and copies the binaries into $INSTALL_DIR
(no root needed). With --deb (Debian/Ubuntu, linux-x86_64 only), downloads the .deb
package instead and installs it system-wide via 'apt install', which also resolves
runtime dependencies (ffmpeg, ca-certificates) automatically; this needs root
(sudo is used automatically if not already running as root).

Arguments:
    VERSION       Version to install (default: latest GitHub release)
                                If specified, must be >= $MIN_SUPPORTED_VERSION
    --deb         Install via a downloaded .deb package instead of the tarball

Examples:
    $0                        # Install latest release to $INSTALL_DIR
    $0 $MIN_SUPPORTED_VERSION # Install a specific supported version to $INSTALL_DIR
    $0 --deb                  # Install latest release system-wide via apt/.deb
    $0 $MIN_SUPPORTED_VERSION --deb

Environment variables:
  DCMNORM_PLATFORM   Override platform detection (e.g., linux-x86_64, macos-aarch64)
  INSTALL_DIR        Override installation directory for the tarball method (default: ~/.cargo/bin)
  CLAUDE_SKILLS_DIR  Override the dcmnorm Claude Code skill's install directory
                     (default: ~/.claude/skills; also forces skill install even
                     without an existing ~/.claude directory)
  GEMINI_SKILLS_DIR  Same as CLAUDE_SKILLS_DIR, for Gemini CLI (default: ~/.gemini/skills)
  CODEX_SKILLS_DIR   Same as CLAUDE_SKILLS_DIR, for Codex CLI (default: ~/.codex/skills)
  DCMNORM_SKIP_SKILL Set to 1 to skip installing the dcmnorm skill for any agent

EOF
}

# Show help if requested
if [ "${HELP_REQUESTED:-0}" = "1" ]; then
    print_usage
    exit 0
fi

# Auto-detect platform
PLATFORM="${DCMNORM_PLATFORM:-$(detect_platform)}"

if [ "$INSTALL_METHOD" = "deb" ]; then
    if [ "$PLATFORM" != "linux-x86_64" ]; then
        echo "Error: --deb is only available for linux-x86_64 (detected/forced platform: $PLATFORM)" >&2
        exit 1
    fi
    if ! command -v apt-get >/dev/null 2>&1; then
        echo "Error: --deb requires apt-get (Debian/Ubuntu)" >&2
        exit 1
    fi
fi

# Validate explicitly requested versions
if [ "$VERSION" != "latest" ] && version_lt "$VERSION" "$MIN_SUPPORTED_VERSION"; then
    echo "Error: Minimum supported explicit version is ${MIN_SUPPORTED_VERSION}." >&2
    echo "Use 'latest' or specify a version >= ${MIN_SUPPORTED_VERSION}." >&2
    exit 1
fi

if [ "$INSTALL_METHOD" = "deb" ]; then
    if [ "$VERSION" = "latest" ]; then
        echo "Installing latest dcmnorm and dcmtalk for $PLATFORM via apt/.deb"
    else
        echo "Installing dcmnorm and dcmtalk v$VERSION for $PLATFORM via apt/.deb"
    fi
else
    if [ "$VERSION" = "latest" ]; then
        echo "Installing latest dcmnorm and dcmtalk for $PLATFORM to $INSTALL_DIR"
    else
        echo "Installing dcmnorm and dcmtalk v$VERSION for $PLATFORM to $INSTALL_DIR"
    fi

    # Create installation directory
    mkdir -p "$INSTALL_DIR"
fi

BINARIES="dcmnorm dcmtalk"

get_existing_version() {
    target_bin="$1"
    target_dir="$2"
    if [ -x "$target_dir/$target_bin" ]; then
        "$target_dir/$target_bin" --version 2>/dev/null || true
    elif [ -x "${HOME}/.cargo/bin/$target_bin" ]; then
        "${HOME}/.cargo/bin/$target_bin" --version 2>/dev/null || true
    elif command -v "$target_bin" >/dev/null 2>&1; then
        "$(command -v "$target_bin")" --version 2>/dev/null || true
    fi
}

for BINARY_NAME in $BINARIES; do
    OLD_VERSION="$(get_existing_version "$BINARY_NAME" "$INSTALL_DIR")"

    # Create temporary directory
    TEMP_DIR=$(mktemp -d)
    trap "rm -rf $TEMP_DIR" EXIT

    if [ "$INSTALL_METHOD" = "deb" ]; then
        EXT="deb"
    else
        EXT="tar.gz"
    fi

    # Construct download URL
    if [ "$VERSION" = "latest" ]; then
        DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/latest/download/${BINARY_NAME}-${PLATFORM}.${EXT}"
    else
        DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/${BINARY_NAME}-${PLATFORM}.${EXT}"
    fi

    echo "Downloading from: $DOWNLOAD_URL"

    # Download the release
    if ! curl -fsSL -o "$TEMP_DIR/${BINARY_NAME}.${EXT}" "$DOWNLOAD_URL"; then
        if [ "$VERSION" = "latest" ]; then
            echo "Error: Failed to download latest ${BINARY_NAME} for $PLATFORM" >&2
        else
            echo "Error: Failed to download ${BINARY_NAME} v$VERSION for $PLATFORM" >&2
        fi
        echo "Check that the version and platform are correct." >&2
        echo "Available releases: https://github.com/$GITHUB_REPO/releases" >&2
        exit 1
    fi

    if [ "$INSTALL_METHOD" = "deb" ]; then
        # 'apt install ./file.deb' (rather than 'dpkg -i') resolves and installs the package's
        # own runtime Depends: (ffmpeg, ca-certificates) automatically.
        if [ "$(id -u)" -eq 0 ]; then
            apt-get install -y "$TEMP_DIR/${BINARY_NAME}.${EXT}"
        else
            sudo apt-get install -y "$TEMP_DIR/${BINARY_NAME}.${EXT}"
        fi

        NEW_VERSION=""
        if command -v "$BINARY_NAME" >/dev/null 2>&1; then
            NEW_VERSION="$("$BINARY_NAME" --version 2>/dev/null || true)"
        fi

        if [ -n "$OLD_VERSION" ]; then
            echo "✓ ${BINARY_NAME} installed via apt (previous: ${OLD_VERSION}, new: ${NEW_VERSION:-v$VERSION})"
        else
            echo "✓ ${BINARY_NAME} installed via apt (version: ${NEW_VERSION:-v$VERSION})"
        fi

        rm -rf "$TEMP_DIR"
        continue
    fi

    # Extract to temporary directory
    (cd "$TEMP_DIR" && tar -xzf "${BINARY_NAME}.tar.gz")

    # Find the binary (dcmnorm/dcmtalk archives extract to a single binary or a same-named directory)
    if [ -f "$TEMP_DIR/${BINARY_NAME}" ]; then
        # Single binary file
        cp "$TEMP_DIR/${BINARY_NAME}" "$INSTALL_DIR/"
        chmod 755 "$INSTALL_DIR/${BINARY_NAME}"
    elif [ -d "$TEMP_DIR/${BINARY_NAME}" ]; then
        # Directory structure
        cp -r "$TEMP_DIR/${BINARY_NAME}"/* "$INSTALL_DIR/"
        chmod 755 "$INSTALL_DIR/${BINARY_NAME}" 2>/dev/null || true
    else
        echo "Error: Could not find ${BINARY_NAME} binary in extracted archive" >&2
        echo "Archive contents:" >&2
        tar -tzf "$TEMP_DIR/${BINARY_NAME}.tar.gz" | head -20
        exit 1
    fi

    NEW_VERSION=""
    if [ -x "$INSTALL_DIR/$BINARY_NAME" ]; then
        NEW_VERSION="$("$INSTALL_DIR/$BINARY_NAME" --version 2>/dev/null || true)"
    fi

    if [ -n "$OLD_VERSION" ]; then
        echo "✓ ${BINARY_NAME} installed to $INSTALL_DIR (previous: ${OLD_VERSION}, new: ${NEW_VERSION:-v$VERSION})"
    else
        echo "✓ ${BINARY_NAME} installed to $INSTALL_DIR (version: ${NEW_VERSION:-v$VERSION})"
    fi

    rm -rf "$TEMP_DIR"
done

# Verify installation (tarball method only - the deb method already confirmed via apt-get's own
# exit status, and the binaries land on the system PATH via /usr/bin, not $INSTALL_DIR)
if [ "$INSTALL_METHOD" != "deb" ]; then
    for BINARY_NAME in $BINARIES; do
        if ! "$BINARY_NAME" --help >/dev/null 2>&1; then
            echo "Warning: Could not verify ${BINARY_NAME} installation. Ensure $INSTALL_DIR is in your PATH." >&2
            echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" >&2
        fi
    done
fi

# Install the dcmnorm skill for any AI agent CLI detected on this machine (Claude Code, Gemini
# CLI, Codex CLI - all three use the same "<skills-dir>/dcmnorm/SKILL.md" convention), best-effort:
# only when there's some sign of that agent on this machine (or its install dir is explicitly
# overridden), and never fatal to the overall install if the download fails.
install_skill_target() {
    _target_name="$1"
    _target_dir="$2"
    _presence_dir="$3"
    _env_override="$4"
    _skill_temp="$5"

    if [ -z "$_env_override" ] && [ ! -d "$_presence_dir" ]; then
        return 0
    fi

    mkdir -p "$_target_dir/dcmnorm"
    cp -f "$_skill_temp" "$_target_dir/dcmnorm/SKILL.md"
    echo "✓ dcmnorm skill installed to $_target_dir/dcmnorm ($_target_name)"
}

if [ "${DCMNORM_SKIP_SKILL:-0}" != "1" ]; then
    CLAUDE_SKILLS_DIR_RESOLVED="${CLAUDE_SKILLS_DIR:-${HOME}/.claude/skills}"
    GEMINI_SKILLS_DIR_RESOLVED="${GEMINI_SKILLS_DIR:-${HOME}/.gemini/skills}"
    CODEX_SKILLS_DIR_RESOLVED="${CODEX_SKILLS_DIR:-${HOME}/.codex/skills}"

    if [ -n "${CLAUDE_SKILLS_DIR:-}" ] || [ -d "${HOME}/.claude" ] \
        || [ -n "${GEMINI_SKILLS_DIR:-}" ] || [ -d "${HOME}/.gemini" ] \
        || [ -n "${CODEX_SKILLS_DIR:-}" ] || [ -d "${HOME}/.codex" ]; then

        SKILL_REF="main"
        if [ "$VERSION" != "latest" ]; then
            SKILL_REF="v$VERSION"
        fi
        SKILL_URL="https://raw.githubusercontent.com/${GITHUB_REPO}/${SKILL_REF}/skills/dcmnorm/SKILL.md"

        SKILL_TEMP="$(mktemp)"
        if curl -fsSL -o "$SKILL_TEMP" "$SKILL_URL"; then
            install_skill_target "Claude Code" "$CLAUDE_SKILLS_DIR_RESOLVED" "${HOME}/.claude" "${CLAUDE_SKILLS_DIR:-}" "$SKILL_TEMP"
            install_skill_target "Gemini CLI" "$GEMINI_SKILLS_DIR_RESOLVED" "${HOME}/.gemini" "${GEMINI_SKILLS_DIR:-}" "$SKILL_TEMP"
            install_skill_target "Codex CLI" "$CODEX_SKILLS_DIR_RESOLVED" "${HOME}/.codex" "${CODEX_SKILLS_DIR:-}" "$SKILL_TEMP"
        else
            echo "Warning: could not download dcmnorm skill from $SKILL_URL (skipping)" >&2
        fi
        rm -f "$SKILL_TEMP"
    fi
fi

echo ""
echo "Installation complete!"
