#!/usr/bin/env bash
# One command: a new meshnode **and** a Director that ships it, both at the
# same version, ready to roll out to the fleet.
#
#   ./scripts/ship.sh 0.9.28     that version: built, and the tree moves to it
#   ./scripts/ship.sh            next dev build (0.9.27 → 0.9.28-dev.1), tree untouched
#   ./scripts/ship.sh --node     meshnode only, skip Director (~3 min faster)
#   ./scripts/ship.sh --ship     Director with fat LTO: ~55 MB smaller, ~4x slower
#
# A version you name becomes the tree's version once what it builds succeeds,
# committed on its own as "Version 0.9.28", so every build after it — a
# plain `cargo build`, `build-director-release.sh` — says 0.9.28 too. A dev
# build leaves the tree exactly as it found it.
#
# ---------------------------------------------------------------------------
# Why this exists
# ---------------------------------------------------------------------------
#
# `ship-node.sh` builds a node and `build-director-release.sh` builds the
# app, and running them in that order *almost* worked: the node came out
# 0.7.3-dev.2 and Director came out 0.7.2, because Director's version comes
# from `Cargo.toml` and the node's dev version was deliberately never
# written there.
#
# Two things then go wrong, and the second is the one that wastes an
# afternoon. The obvious one is that the About box disagrees with the binary
# in its own resources. The subtle one is that **version is the only handle
# the fleet has**: `meshnode upgrade` compares versions, a rollout names one,
# and a mesh whose Director claims to be older than the node it is holding
# has no way to express "there is something newer here".
#
# So the version is chosen once and stamped into everything that carries
# one. Both binaries report the same string because they were compiled from
# the same one.
#
# **And a named version stays.** This used to restore every file at the
# end, whatever the version, so `ship.sh 0.9.27` built a 0.9.27 and left a
# tree that said 0.7.2 — and everything built from it afterwards, this
# machine's own node included, told the fleet it was 0.7.2: older than the
# release it had just shipped, and so, to `meshnode upgrade`, out of date.
# Versions only move forward: naming one older than the tree's is refused.
#
# ---------------------------------------------------------------------------
# What it does, and what still needs a person
# ---------------------------------------------------------------------------
#
# It builds, signs, and makes sure the mesh trusts the signing key. It does
# **not** publish a release into the mesh or start a rollout: publishing is
# an op signed by an admin, and starting a rollout decides when a fleet
# restarts. Those stay a decision somebody makes in Director:
#
#   Mesh → Fleet → Updates → Publish a release → pick this build → roll out
#
# `packaging/build-release.sh` is still the pipeline for a *real* release —
# every architecture, tarballs, .deb/.rpm, SHA256SUMS.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_build-common.sh"
ROOT="$(build_root)"
cd "$ROOT"

TARGET=x86_64-unknown-linux-musl
DIST="$ROOT/dist"
RES="$ROOT/apps/director/src-tauri/resources/meshnode"
HOME_DIR="${MESHWARE_HOME:-$HOME/.meshware}"
KEY="${MINISIGN_KEY:-$HOME/.minisign/meshware-release.key}"
PUB="${KEY%.key}.pub"
PROFILE="${MESHWARE_DIRECTOR_PROFILE:-director}"
WANT_DIRECTOR=1
VERSION=""

die() { echo "error: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --node|--node-only) WANT_DIRECTOR=0 ;;
    --ship) PROFILE=release ;;
    -*) die "unknown option '$1' (try --help)" ;;
    *) [ -z "$VERSION" ] || die "version given twice"; VERSION="${1#v}" ;;
  esac
  shift
done
# A version somebody named is a release of this tree; a dev build is not.
KEEP=0
[ -n "$VERSION" ] && KEEP=1

command -v minisign >/dev/null 2>&1 \
  || die "minisign not installed (dnf install minisign / apt install minisign)"
require_musl "$TARGET"

# -- 1. one version, chosen once --------------------------------------------
stage "choosing a version"
CURRENT="$(cargo_version)"
# The four version files are supposed to agree at rest, and nothing checked:
# `tauri.conf.json` sat at 0.7.0 while the tree was 0.7.2 for long enough
# that every installer built in between carried the wrong number. A warning
# rather than a stop — this build is about to stamp them all to one value
# anyway, and the thing worth knowing is that they were not.
for vf in "apps/director/package.json" "apps/director/src-tauri/tauri.conf.json"; do
  at="$(sed -n '0,/"version":/s/.*"version": "\([^"]*\)".*/\1/p' "$ROOT/$vf")"
  [ "$at" = "$CURRENT" ] || echo "    note: $vf is at ${at}, Cargo.toml at ${CURRENT}"
done
if [ -n "$VERSION" ]; then
  echo "    ${CURRENT} → ${VERSION}"
  ! version_older "$VERSION" "$CURRENT" \
    || die "${VERSION} is older than this tree's ${CURRENT} — versions only move forward"
else
  VERSION="$(next_dev_version "$DIST" "$TARGET")"
  echo "    ${CURRENT} → ${VERSION}"
fi
[ ! -e "${DIST}/meshnode-v${VERSION}-${TARGET}" ] \
  || die "dist/ already holds ${VERSION} — pass a different version"
# A release with nothing written about it is refused (DOC-7). Dev builds
# (the default here) pass straight through, and `ship.sh 0.9.28` needs
# either a `## 0.9.28` heading or the `## Unreleased` section the notes are
# being written in — a heading per fleet build would be a changelog of build
# numbers. `packaging/build-release.sh`, which publishes, wants the heading.
"$ROOT/scripts/changelog-entry.sh" --ship "$VERSION" || exit 1

# -- 2. stamp it into everything that carries one ---------------------------
#
# `stamp_version` installs the trap that puts every version file back, so
# from here on every exit path — failure, Ctrl-C, and success for a dev
# build — leaves the tree as it was. Only `keep_and_commit`, after the
# builds, makes a named version stay.
stage "stamping the version"
# Before the stamp, so a version file already carrying somebody's own edits
# is not committed along with the version.
DIRTY=""
[ "$KEEP" -eq 1 ] && DIRTY="$(version_files_dirty)"
stamp_version "$VERSION"
echo "    Cargo.toml, Cargo.lock, package.json, tauri.conf.json → ${VERSION}"

# -- 3. meshnode -------------------------------------------------------------
stage "building meshnode ($TARGET, release)"
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --target "$TARGET" -p meshware-node
BIN="$ROOT/target/$TARGET/release/meshnode"
[ -x "$BIN" ] || die "expected a binary at $BIN"
BUILT="$("$BIN" -V | awk '{print $2}')"
[ "$BUILT" = "$VERSION" ] || die "meshnode reports ${BUILT}, expected ${VERSION}"
echo "    reports ${BUILT}"

# -- 4. a signing key --------------------------------------------------------
stage "signing key"
if [ ! -f "$KEY" ]; then
  mkdir -p "$(dirname "$KEY")"
  # `-W` writes a password-free key: one that prompts cannot be used from a
  # script, and what it protects sits on the same disk the passphrase would.
  minisign -G -W -p "$PUB" -s "$KEY" >/dev/null
  echo "    generated $KEY"
elif [ ! -f "$PUB" ]; then
  minisign -R -s "$KEY" -p "$PUB" >/dev/null
  echo "    recovered $PUB from the secret key"
fi
PUBKEY="$(grep -v '^untrusted comment' "$PUB" | tr -d '[:space:]')"
echo "    $PUBKEY"

# -- 5. into dist/, signed ---------------------------------------------------
#
# Every node verifies a minisign signature before it replaces its own
# binary, so an unsigned build is one no machine will install.
stage "writing dist/"
mkdir -p "$DIST"
ARTIFACT="meshnode-v${VERSION}-${TARGET}"
install -m 755 "$BIN" "${DIST}/${ARTIFACT}"
minisign -HSm "${DIST}/${ARTIFACT}" -s "$KEY" -t "meshware v${VERSION}" >/dev/null
echo "    dist/${ARTIFACT}"
echo "    dist/${ARTIFACT}.minisig"

# -- 6. make sure the mesh trusts the key ------------------------------------
#
# Not "add the key" — *make sure*. The signature is checked against keys
# published into the mesh, not against anything on this machine, and running
# this on every build would author an op each time to say something the mesh
# already knows.
stage "release key, mesh-wide"
NODE_ARGS=()
KEYS=""
# The home first: that is where this machine's node lives. `director/` is
# where an older Director ran one of its own, before it was adopted.
for candidate in "${HOME_DIR}" "${HOME_DIR}/director"; do
  [ -d "$candidate" ] || continue
  try=(--data-dir "$candidate")
  [ -f "${HOME_DIR}/identity/user.key" ] \
    && try+=(--user-identity "${HOME_DIR}/identity/user.key")
  if KEYS="$("$BIN" "${try[@]}" upgrade --keys 2>/dev/null)"; then
    NODE_ARGS=("${try[@]}")
    echo "    via ${candidate}"
    break
  fi
done
if [ ${#NODE_ARGS[@]} -eq 0 ]; then
  echo "    no reachable node under ${HOME_DIR} — skipped."
  echo "    Start this machine's node (meshnode up --detach), then run this once:"
  echo "      meshnode upgrade --trust-key $PUB"
elif printf '%s' "$KEYS" | grep -qF "$PUBKEY"; then
  echo "    already trusted."
elif "$BIN" "${NODE_ARGS[@]}" upgrade --trust-key "$PUB" \
       --key-label "$(hostname) build key" >/dev/null 2>&1; then
  echo "    now trusted mesh-wide, including by nodes that join later."
else
  echo "    could not add it automatically. Run this once:"
  echo "      meshnode ${NODE_ARGS[*]} upgrade --trust-key $PUB"
fi

# Make a named version the tree's own. Only once everything asked for has
# built: a failure before this point restores every file, so the tree never
# claims a version that does not exist.
keep_and_commit() {
  [ "$KEEP" -eq 1 ] || return 0
  stage "moving the tree to ${VERSION}"
  keep_versions "$VERSION"
  commit_versions "$VERSION" "$DIRTY"
}

if [ "$WANT_DIRECTOR" -eq 0 ]; then
  keep_and_commit
  stage_done
  cat <<MSG

meshnode ${VERSION} is built and signed. Director was not rebuilt (--node).

  Director → Mesh → Fleet → Updates → Publish a release
  → pick meshnode-v${VERSION}-${TARGET}, then start a rollout.

MSG
  report_binary "${DIST}/${ARTIFACT}" --probe-version
  exit 0
fi

# -- 7. bundle it inside Director --------------------------------------------
#
# Add machines needs a meshnode for the far machine's architecture, and
# looks inside Director as one of its sources. This is that: the binary just
# built, plus a VERSION file for the architectures this machine cannot run
# to ask.
stage "bundling meshnode into Director"
mkdir -p "$RES"
rm -f "$RES"/meshnode-* "$RES/VERSION"
install -m 755 "${DIST}/${ARTIFACT}" "$RES/meshnode-$TARGET"
printf '%s\n' "$VERSION" > "$RES/VERSION"
echo "    meshnode-$TARGET (version $VERSION)"
# The other architecture, if a build-release run left one that matches. A
# mismatched one is left out rather than shipped: one VERSION file covers
# both, so a dev x86_64 beside a release aarch64 would have Director
# publishing a version one of its binaries does not have.
OTHER=aarch64-unknown-linux-musl
IFS=$'\t' read -r other_ver other_src <<< "$(newest_for_target "$DIST" "$OTHER")" || true
if [ -n "${other_src:-}" ] && [ "$other_ver" = "$VERSION" ]; then
  install -m 755 "$other_src" "$RES/meshnode-$OTHER"
  echo "    meshnode-$OTHER (version $other_ver)"
elif [ -n "${other_src:-}" ]; then
  echo "    (dist/ has $OTHER at $other_ver, not $VERSION — not bundled)"
fi

# -- 8. Director --------------------------------------------------------------
#
# The bindings step is not optional: `src/lib/bindings/` is generated from
# the `#[ts(export)]` view models, so building the frontend against stale
# ones compiles a UI that disagrees with the engine it talks to.
stage "regenerating TypeScript bindings"
cargo test -p meshware-director --lib export_bindings --quiet >/dev/null

stage "building the frontend"
cd apps/director
[ -d node_modules ] || npm install
npm run build >/dev/null
cd "$ROOT"

stage "building Director ($PROFILE, embedded frontend)"
cargo build --profile "$PROFILE" -p meshware-director --features custom-protocol
keep_and_commit
stage_done

DIRBIN="$ROOT/target/$PROFILE/meshware-director"
[ -x "$DIRBIN" ] || die "expected a binary at $DIRBIN"

# -- done ---------------------------------------------------------------------
#
# For a dev build, `restore_versions` runs from the trap on the way out and
# the tree is as it was. Both binaries keep the version they were compiled
# with either way.
cat <<MSG

Both at ${VERSION}.

  meshnode : ${DIST}/${ARTIFACT}
             $(du -h "${DIST}/${ARTIFACT}" | cut -f1)  ·  $("$BIN" -V)
  Director : ${DIRBIN}
             $(du -h "$DIRBIN" | cut -f1)  ·  bundling meshnode ${VERSION}

$(if [ "$KEEP" -eq 1 ]; then echo "The tree is at ${VERSION} now (it was ${CURRENT})."; else echo "The tree is back at ${CURRENT}; nothing to commit."; fi)

Next, in Director:
  Mesh → Fleet → Updates → Publish a release → pick meshnode-v${VERSION}
  then start a rollout.

MSG
if [ "$PROFILE" != "release" ]; then
  echo "  (ThinLTO build. --ship is ~55 MB smaller and takes ~4x as long.)"
  echo
fi
