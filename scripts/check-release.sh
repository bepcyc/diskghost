#!/usr/bin/env bash
# Release gate: facts a tag alone does not prove.
#   1. The tag's version is what every package declares (workspace Cargo.toml,
#      gui/src-tauri/Cargo.toml, gui/src-tauri/tauri.conf.json).
#   2. The tagged commit is reachable from main — a release cut from an
#      unmerged side branch ships code nobody lands.
# Run it before tagging: scripts/check-release.sh v0.15.2
set -euo pipefail
cd "$(dirname "$0")/.."

want="${1#v}"
[ -n "$want" ] || {
    echo "usage: $0 v<version>" >&2
    exit 2
}

ws=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
gui_cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' gui/src-tauri/Cargo.toml | head -1)
tauri=$(sed -n 's/.*"version": "\(.*\)",/\1/p' gui/src-tauri/tauri.conf.json | head -1)
echo "tag=$want workspace=$ws gui-cargo=$gui_cargo tauri=$tauri"
for v in "$ws" "$gui_cargo" "$tauri"; do
    if [ "$v" != "$want" ]; then
        echo "FAIL: version $v does not match tag $want" >&2
        exit 1
    fi
done

# The release remote is `fork` when this is a personal fork, else `origin`.
remote=$(git remote | grep -x fork || echo origin)
git fetch --quiet --no-tags "$remote" main
git merge-base --is-ancestor HEAD FETCH_HEAD || {
    echo "FAIL: HEAD is not reachable from $remote/main — merge the release branch first" >&2
    exit 1
}
echo "OK: v$want is consistent and reachable from $remote/main"
