#!/bin/bash

# Copyright 2025 Google LLC
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#      https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

set -euo pipefail

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
    # Returns 0 if $1 < $2, otherwise 1
    [[ "$(printf '%s\n' "$1" "$2" | sort -V | head -n 1)" != "$2" ]]
}

print_usage() {
    cat << EOF
Usage: $0 [VERSION]

Downloads and installs dcmnorm from GitHub releases into ./bin.

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
if [[ "${1:-}" == "-h" ]] || [[ "${1:-}" == "--help" ]]; then
    print_usage
    exit 0
fi

# Auto-detect platform
PLATFORM="${DCMNORM_PLATFORM:-$(detect_platform)}"

# Validate explicitly requested versions
if [[ "$VERSION" != "latest" ]] && version_lt "$VERSION" "$MIN_SUPPORTED_VERSION"; then
    echo "Error: Minimum supported explicit version is ${MIN_SUPPORTED_VERSION}." >&2
    echo "Use 'latest' or specify a version >= ${MIN_SUPPORTED_VERSION}." >&2
    exit 1
fi

if [[ "$VERSION" == "latest" ]]; then
    echo "Installing latest dcmnorm for $PLATFORM to $INSTALL_DIR"
else
    echo "Installing dcmnorm v$VERSION for $PLATFORM to $INSTALL_DIR"
fi

# Create installation directory
mkdir -p "$INSTALL_DIR"

# Create temporary directory
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Construct download URL
if [[ "$VERSION" == "latest" ]]; then
    DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/latest/download/dcmnorm-${PLATFORM}.tar.gz"
else
    DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/dcmnorm-${PLATFORM}.tar.gz"
fi

echo "Downloading from: $DOWNLOAD_URL"

# Download the release
if ! curl -fsSL -o "$TEMP_DIR/dcmnorm.tar.gz" "$DOWNLOAD_URL"; then
    if [[ "$VERSION" == "latest" ]]; then
        echo "Error: Failed to download latest dcmnorm for $PLATFORM" >&2
    else
        echo "Error: Failed to download dcmnorm v$VERSION for $PLATFORM" >&2
    fi
    echo "Check that the version and platform are correct." >&2
    echo "Available releases: https://github.com/$GITHUB_REPO/releases" >&2
    exit 1
fi

# Extract to temporary directory
cd "$TEMP_DIR"
tar -xzf dcmnorm.tar.gz

# Find the binary (dcmnorm projects typically extract to a single binary or dcmnorm/ directory)
if [ -f "dcmnorm" ]; then
    # Single binary file
    cp dcmnorm "$INSTALL_DIR/"
    chmod 755 "$INSTALL_DIR/dcmnorm"
    echo "✓ dcmnorm installed to $INSTALL_DIR/dcmnorm"
elif [ -d "dcmnorm" ]; then
    # Directory structure
    cp -r dcmnorm/* "$INSTALL_DIR/"
    chmod 755 "$INSTALL_DIR/dcmnorm" 2>/dev/null || true
    echo "✓ dcmnorm installed to $INSTALL_DIR/"
else
    echo "Error: Could not find dcmnorm binary in extracted archive" >&2
    echo "Archive contents:" >&2
    tar -tzf "$TEMP_DIR/dcmnorm.tar.gz" | head -20
    exit 1
fi

# Verify installation
if ! dcmnorm --help >/dev/null 2>&1; then
    echo "Warning: Could not verify installation. Ensure $INSTALL_DIR is in your PATH." >&2
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" >&2
fi

echo ""
echo "Installation complete!"
