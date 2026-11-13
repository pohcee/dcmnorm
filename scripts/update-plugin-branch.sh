#!/usr/bin/env bash
# Points the `claude-plugin` branch at the plugin/ folder of a commit (default: HEAD).
#
#   scripts/update-plugin-branch.sh [<commit>] [--push]
#
# The Claude plugin directory installs a plugin by downloading one ref of its repository and caps the
# archive's size; this repository (committed binding binaries, test files) is far over it. So the
# directory tracks the `claude-plugin` branch, whose tree is only plugin/ at its root. Each update is
# one new commit whose tree is <commit>:plugin, on top of the branch's previous tip, so the branch
# only moves forward. Nothing is checked out or changed in the working tree.
#
# The release workflow runs this with --push on every v* tag; run it by hand only to repair the
# branch. Never commit to the branch directly: it is rebuilt from plugin/.
set -euo pipefail

BRANCH="claude-plugin"
SOURCE="HEAD"
PUSH=0
for arg in "$@"; do
    case "$arg" in
        --push) PUSH=1 ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) SOURCE="$arg" ;;
    esac
done

cd "$(dirname "$0")/.."
git fetch --quiet origin "+refs/heads/${BRANCH}:refs/remotes/origin/${BRANCH}" 2>/dev/null || true

source_commit="$(git rev-parse --verify "${SOURCE}^{commit}")"
tree="$(git rev-parse --verify "${source_commit}:plugin")"
parent="$(git rev-parse --verify --quiet "refs/remotes/origin/${BRANCH}" || true)"

if [[ -n "$parent" && "$(git rev-parse "${parent}^{tree}")" == "$tree" ]]; then
    echo "${BRANCH} already has plugin/ from ${source_commit:0:7}; nothing to do."
    exit 0
fi

describe="$(git describe --tags --always "$source_commit")"
message="Claude plugin from ${describe} (${source_commit:0:7}:plugin)

This branch holds only the plugin/ folder of the main branch, at its root,
for the Claude plugin directory (which downloads a whole ref and caps its
size). Don't commit here: scripts/update-plugin-branch.sh rebuilds it."

commit="$(git commit-tree "$tree" ${parent:+-p "$parent"} -m "$message")"
echo "${BRANCH}: ${parent:0:7} -> ${commit:0:7} (plugin/ from ${describe})"

if [[ "$PUSH" -eq 1 ]]; then
    git push origin "${commit}:refs/heads/${BRANCH}"
else
    echo "Dry run; pass --push to update origin/${BRANCH}."
fi
