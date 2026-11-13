#!/usr/bin/env bash
# Move the tree to a version, without building anything.
#
#   ./scripts/set-version.sh 0.9.28
#
# Every file that carries the version — Cargo.toml and Cargo.lock,
# Director's package.json, package-lock.json and tauri.conf.json, the Python
# SDK and its generated reference — is set to it and committed on its own as
# "Version 0.9.28". `ship.sh 0.9.28` does exactly this after it has built;
# this is the same step for when there is nothing to build, or the tree has
# fallen behind the versions that were shipped from it.
#
# Versions only move forward: one older than the tree's is refused.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_build-common.sh"
ROOT="$(build_root)"
cd "$ROOT"

case "${1:-}" in
  "" | -h | --help) sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
esac
[ $# -eq 1 ] || { echo "error: one version, please (try --help)" >&2; exit 2; }
VERSION="${1#v}"
case "$VERSION" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "error: '$VERSION' is not a version like 0.9.28" >&2; exit 2 ;;
esac

CURRENT="$(cargo_version)"
if version_older "$VERSION" "$CURRENT"; then
  echo "error: ${VERSION} is older than this tree's ${CURRENT} — versions only move forward" >&2
  exit 1
fi

stage "${CURRENT} → ${VERSION}"
DIRTY="$(version_files_dirty)"
stamp_version "$VERSION"
keep_versions "$VERSION"
commit_versions "$VERSION" "$DIRTY"
stage_done
