#!/usr/bin/env bash
# Publishes the workspace's crates to crates.io at the workspace version (the release job runs it;
# packaging/README.md, "crates.io").
#
#   packaging/publish-crates.sh            publish every crate whose version crates.io lacks
#   packaging/publish-crates.sh --dry-run  list them and package + verify them; upload nothing
#
# The crates are every workspace member without `publish = false` (the bindings and
# dcmnorm-codegen have it). cargo orders them by their dependencies and waits for each to reach
# the index before publishing the ones that need it. A version already on crates.io is skipped,
# so a re-run finishes a partial publish (crates.io never accepts the same version twice).
# Needs CARGO_REGISTRY_TOKEN unless --dry-run.
set -euo pipefail
cd "$(dirname "$0")/.."

dry_run=0
case "${1:-}" in
  --dry-run) dry_run=1 ;;
  "") ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

# "name version" for each publishable member.
mapfile -t crates < <(
  cargo metadata --format-version 1 --no-deps --locked |
    python3 -c '
import json, sys
meta = json.load(sys.stdin)
members = set(meta["workspace_members"])
for p in meta["packages"]:
    if p["id"] in members and p["publish"] != []:
        print(p["name"], p["version"])
'
)

# The sparse index path of a crate: https://doc.rust-lang.org/cargo/reference/registry-index.html
index_url() {
  local name="${1,,}"
  case "${#name}" in
    1) echo "https://index.crates.io/1/$name" ;;
    2) echo "https://index.crates.io/2/$name" ;;
    3) echo "https://index.crates.io/3/${name:0:1}/$name" ;;
    *) echo "https://index.crates.io/${name:0:2}/${name:2:2}/$name" ;;
  esac
}

published() {
  local name="$1" version="$2" body status
  body="$(mktemp)"
  status="$(curl -sS -o "$body" -w '%{http_code}' "$(index_url "$name")")"
  case "$status" in
    200) python3 -c 'import json, sys; sys.exit(0 if any(json.loads(l)["vers"] == sys.argv[2] for l in open(sys.argv[1]) if l.strip()) else 1)' "$body" "$version"; local rc=$?; rm -f "$body"; return $rc ;;
    404) rm -f "$body"; return 1 ;;
    *) rm -f "$body"; echo "crates.io index returned HTTP $status for $name" >&2; exit 1 ;;
  esac
}

todo=()
for entry in "${crates[@]}"; do
  read -r name version <<< "$entry"
  if published "$name" "$version"; then
    echo "skip    $name $version (already on crates.io)"
  else
    echo "publish $name $version"
    todo+=(-p "$name")
  fi
done

if [[ ${#todo[@]} -eq 0 ]]; then
  echo "Every crate is already on crates.io."
  exit 0
fi

if [[ "$dry_run" -eq 1 ]]; then
  # Packages and builds each crate from its .crate file, against the others' local packages.
  exec cargo package --locked "${todo[@]}"
fi

: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is not set}"
exec cargo publish --locked "${todo[@]}"
