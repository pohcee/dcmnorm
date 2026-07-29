#!/bin/sh
set -eu

INSTALL_DIR="${HOME}/.cargo/bin"
DEFAULT_VERSION="latest"
MIN_SUPPORTED_VERSION="0.1.3"
GITHUB_REPO="pohcee/dcmnorm"
PLATFORM="linux-x86_64"

# Parse command line arguments
VERSION="${1:-$DEFAULT_VERSION}"

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
Usage: $0 [VERSION]

Downloads and installs dcmnorm and dcmtalk from GitHub releases into ./bin.

Arguments:
    VERSION       Version to install (default: latest GitHub release)
                                If specified, must be >= $MIN_SUPPORTED_VERSION

Examples:
    $0                        # Install latest release to ./bin
    $0 $MIN_SUPPORTED_VERSION # Install a specific supported version to ./bin

Environment variables:
  DCMNORM_PLATFORM  Override platform detection (e.g., linux-x86_64, macos-aarch64)

EOF
}

# Show help if requested
if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    print_usage
    exit 0
fi

# Auto-detect platform
PLATFORM="${DCMNORM_PLATFORM:-$(detect_platform)}"

# Validate explicitly requested versions
if [ "$VERSION" != "latest" ] && version_lt "$VERSION" "$MIN_SUPPORTED_VERSION"; then
    echo "Error: Minimum supported explicit version is ${MIN_SUPPORTED_VERSION}." >&2
    echo "Use 'latest' or specify a version >= ${MIN_SUPPORTED_VERSION}." >&2
    exit 1
fi

if [ "$VERSION" = "latest" ]; then
    echo "Installing latest dcmnorm and dcmtalk for $PLATFORM to $INSTALL_DIR"
else
    echo "Installing dcmnorm and dcmtalk v$VERSION for $PLATFORM to $INSTALL_DIR"
fi

# Create installation directory
mkdir -p "$INSTALL_DIR"

BINARIES="dcmnorm dcmtalk"

for BINARY_NAME in $BINARIES; do
    # Create temporary directory
    TEMP_DIR=$(mktemp -d)
    trap "rm -rf $TEMP_DIR" EXIT

    # Construct download URL
    if [ "$VERSION" = "latest" ]; then
        DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/latest/download/${BINARY_NAME}-${PLATFORM}.tar.gz"
    else
        DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/${BINARY_NAME}-${PLATFORM}.tar.gz"
    fi

    echo "Downloading from: $DOWNLOAD_URL"

    # Download the release
    if ! curl -fsSL -o "$TEMP_DIR/${BINARY_NAME}.tar.gz" "$DOWNLOAD_URL"; then
        if [ "$VERSION" = "latest" ]; then
            echo "Error: Failed to download latest ${BINARY_NAME} for $PLATFORM" >&2
        else
            echo "Error: Failed to download ${BINARY_NAME} v$VERSION for $PLATFORM" >&2
        fi
        echo "Check that the version and platform are correct." >&2
        echo "Available releases: https://github.com/$GITHUB_REPO/releases" >&2
        exit 1
    fi

    # Extract to temporary directory
    (cd "$TEMP_DIR" && tar -xzf "${BINARY_NAME}.tar.gz")

    # Find the binary (dcmnorm/dcmtalk archives extract to a single binary or a same-named directory)
    if [ -f "$TEMP_DIR/${BINARY_NAME}" ]; then
        # Single binary file
        cp "$TEMP_DIR/${BINARY_NAME}" "$INSTALL_DIR/"
        chmod 755 "$INSTALL_DIR/${BINARY_NAME}"
        echo "✓ ${BINARY_NAME} installed to $INSTALL_DIR/${BINARY_NAME}"
    elif [ -d "$TEMP_DIR/${BINARY_NAME}" ]; then
        # Directory structure
        cp -r "$TEMP_DIR/${BINARY_NAME}"/* "$INSTALL_DIR/"
        chmod 755 "$INSTALL_DIR/${BINARY_NAME}" 2>/dev/null || true
        echo "✓ ${BINARY_NAME} installed to $INSTALL_DIR/"
    else
        echo "Error: Could not find ${BINARY_NAME} binary in extracted archive" >&2
        echo "Archive contents:" >&2
        tar -tzf "$TEMP_DIR/${BINARY_NAME}.tar.gz" | head -20
        exit 1
    fi

    rm -rf "$TEMP_DIR"
done

# Verify installation
for BINARY_NAME in $BINARIES; do
    if ! "$BINARY_NAME" --help >/dev/null 2>&1; then
        echo "Warning: Could not verify ${BINARY_NAME} installation. Ensure $INSTALL_DIR is in your PATH." >&2
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" >&2
    fi
done

echo ""
echo "Installation complete!"
