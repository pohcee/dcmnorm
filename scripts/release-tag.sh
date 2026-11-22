#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
    ./scripts/release-tag.sh <patch|minor|major> [--prerelease <id>] [--dry-run]

Examples:
  ./scripts/release-tag.sh patch
  ./scripts/release-tag.sh minor --prerelease rc
  ./scripts/release-tag.sh major --dry-run

This script:
  0) Fast-forwards onto origin
  1) Computes the next SemVer tag from existing v* tags
  2) Updates the crate versions (the workspace version in Cargo.toml and the crates' exact pins
     on each other), the Claude plugin/skill version pins and the binding package versions (npm
     package.json + loader, pyproject.toml, the Maven poms and relocation poms) via
     scripts/check-versions.py --set
    3) Commits the version bump
    4) Creates an annotated tag on that commit
    5) Pushes the commit and tag to origin

Pushing the tag triggers .github/workflows/release.yml (tests and every build, then the GitHub
release and the crates.io, npm, PyPI and Maven Central packages).
EOF
}

require_clean_tree() {
    if [[ -n "$(git status --porcelain)" ]]; then
        echo "Working tree is not clean. Commit or stash changes first." >&2
        exit 1
    fi
}

latest_semver_tag() {
    git tag --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' | head -n1 || true
}

current_manifest_version() {
    local manifest="$1"
    awk '
        BEGIN {
            in_package = 0
        }
        /^\[workspace\.package\]$/ {
            in_package = 1
            next
        }
        /^\[/ {
            in_package = 0
        }
        in_package && /^version[[:space:]]*=/ {
            gsub(/^[^\"]*\"/, "", $0)
            gsub(/\".*/, "", $0)
            print $0
            exit
        }
    ' "$manifest"
}

compute_next_version() {
    local latest="$1"
    local bump="$2"
    local preid="${3:-}"

    local base="${latest#v}"
    base="${base%%-*}"

    local major minor patch
    IFS='.' read -r major minor patch <<< "$base"

    case "$bump" in
        major)
            major=$((major + 1))
            minor=0
            patch=0
            ;;
        minor)
            minor=$((minor + 1))
            patch=0
            ;;
        patch)
            patch=$((patch + 1))
            ;;
        *)
            echo "Invalid bump type: $bump" >&2
            exit 1
            ;;
    esac

    local next="${major}.${minor}.${patch}"
    if [[ -n "$preid" ]]; then
        next="${next}-${preid}.1"
    fi

    echo "$next"
}

bump_type=""
prerelease_id=""
dry_run=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        patch|minor|major)
            if [[ -n "$bump_type" ]]; then
                echo "Bump type already set: $bump_type" >&2
                exit 1
            fi
            bump_type="$1"
            shift
            ;;
        --prerelease)
            shift
            if [[ $# -eq 0 || -z "$1" ]]; then
                echo "--prerelease requires an identifier (for example: rc)" >&2
                exit 1
            fi
            prerelease_id="$1"
            shift
            ;;
        --dry-run)
            dry_run=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage
            exit 1
            ;;
    esac
done

if [[ -z "$bump_type" ]]; then
    usage
    exit 1
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

if ! command -v git >/dev/null 2>&1; then
    echo "git is required" >&2
    exit 1
fi

if [[ -z "$(git remote get-url origin 2>/dev/null || true)" ]]; then
    echo "origin remote is not configured" >&2
    exit 1
fi

require_clean_tree

git fetch --tags --force --quiet
# Build on top of whatever landed on origin since the last pull rather than failing the push below.
if git rev-parse --abbrev-ref '@{u}' >/dev/null 2>&1; then
    git fetch --quiet
    git merge --ff-only --quiet '@{u}'
fi

latest_tag="$(latest_semver_tag)"
version_source="git tag"
if [[ -z "$latest_tag" ]]; then
    manifest_version="$(current_manifest_version "Cargo.toml")"
    if [[ -z "$manifest_version" ]]; then
        echo "Could not read version from Cargo.toml" >&2
        exit 1
    fi
    latest_tag="v${manifest_version}"
    version_source="Cargo.toml"
fi

next_version="$(compute_next_version "$latest_tag" "$bump_type" "$prerelease_id")"
next_tag="v${next_version}"

if git rev-parse -q --verify "refs/tags/${next_tag}" >/dev/null; then
    echo "Tag already exists: ${next_tag}" >&2
    exit 1
fi

echo "Current version (${version_source}): ${latest_tag}"
echo "Next tag:    ${next_tag}"

# The wheels go to PyPI, whose versions only allow alpha/beta/rc prereleases; check before
# changing any file.
python3 -c 'import sys; sys.path.insert(0, "packaging"); import package; print("PyPI version:", package.pep440_version(sys.argv[1]))' "$next_version"

if [[ "$dry_run" -eq 1 ]]; then
    echo "Dry run enabled. No tag created or pushed."
    exit 0
fi

# The crates' version and their pins on each other, the Claude plugin's version and the version
# its skill installs from PyPI, and the binding packages (npm, PyPI, Maven), which all publish
# with the release version.
python3 scripts/check-versions.py --set "$next_version"

# Only the workspace's own version entries: `cargo generate-lockfile` would re-resolve every
# dependency to its newest compatible version as a side effect of each release.
cargo update --workspace --quiet

git add -u
if git diff --cached --quiet; then
    echo "Version files already at ${next_version}; creating tag on current HEAD without a new commit."
else
    git commit -m "chore(release): ${next_tag}"
fi

git push origin HEAD

git tag -a "$next_tag" -m "Release $next_tag"
git push origin "$next_tag"

echo "Pushed ${next_tag}."
echo "GitHub Actions release workflow should now start for this tag."
