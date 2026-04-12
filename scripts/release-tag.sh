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
  1) Computes the next SemVer tag from existing v* tags
    2) Updates crate versions in Cargo.toml files
    3) Commits the version bump
    4) Creates an annotated tag on that commit
    5) Pushes the commit and tag to origin

Pushing the tag triggers .github/workflows/release.yml.
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

update_manifest_version() {
    local manifest="$1"
    local version="$2"
    local tmp

    tmp="$(mktemp)"
    awk -v version="$version" '
        BEGIN {
            in_package = 0
            updated = 0
        }
        /^\[package\]$/ {
            in_package = 1
            print
            next
        }
        /^\[/ {
            in_package = 0
        }
        in_package && /^version[[:space:]]*=/ && updated == 0 {
            print "version = \"" version "\""
            updated = 1
            next
        }
        {
            print
        }
        END {
            if (updated == 0) {
                exit 2
            }
        }
    ' "$manifest" > "$tmp"

    mv "$tmp" "$manifest"
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

git fetch --tags --quiet

latest_tag="$(latest_semver_tag)"
if [[ -z "$latest_tag" ]]; then
    latest_tag="v0.0.0"
fi

next_version="$(compute_next_version "$latest_tag" "$bump_type" "$prerelease_id")"
next_tag="v${next_version}"

if git rev-parse -q --verify "refs/tags/${next_tag}" >/dev/null; then
    echo "Tag already exists: ${next_tag}" >&2
    exit 1
fi

echo "Current tag: ${latest_tag}"
echo "Next tag:    ${next_tag}"

if [[ "$dry_run" -eq 1 ]]; then
    echo "Dry run enabled. No tag created or pushed."
    exit 0
fi

update_manifest_version "Cargo.toml" "$next_version"
update_manifest_version "exec/dcmnorm/Cargo.toml" "$next_version"

git add Cargo.toml exec/dcmnorm/Cargo.toml
git commit -m "chore(release): ${next_tag}"

git push origin HEAD

git tag -a "$next_tag" -m "Release $next_tag"
git push origin "$next_tag"

echo "Pushed ${next_tag}."
echo "GitHub Actions release workflow should now start for this tag."
