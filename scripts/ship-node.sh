#!/usr/bin/env bash
# One command from "I changed some code" to "Director offers it as an
# upgrade". Builds meshnode, gives it a version the fleet has not seen,
# signs it, drops it in dist/, and makes sure this mesh trusts the key.
#
#   ./scripts/ship-node.sh          next dev build (0.7.2 → 0.7.3-dev.1)
#   ./scripts/ship-node.sh 0.7.3    that exact version instead
#
# Leaves the tree exactly as it found it — see "Versions and git" below.
#
# For both at once, at one version: ./scripts/ship.sh
#
# ---------------------------------------------------------------------------
# Why this is more than a copy into dist/
# ---------------------------------------------------------------------------
#
# Three things make a binary selectable in Director, and missing any one of
# them produces a file that looks finished and is not:
#
#   1. **A version the mesh has not published.** A version names one binary
#      per architecture for the life of the mesh, and `meshnode upgrade`
#      compares versions — so a rebuilt 0.6.0 is refused by the publish
#      path, and nodes already on 0.6.0 would report "already up to date"
#      regardless. Hence the automatic bump: the point of building is that
#      the fleet should take it.
#   2. **A minisign signature next to it.** Every node verifies one before
#      it replaces its own binary, so an unsigned build is one no machine
#      will install. The key is generated here if there isn't one.
#   3. **A mesh that trusts that key.** The signature is checked against
#      keys published *into the mesh*, not against anything on this
#      machine. Done once, automatically, and skipped when already true.
#
# ---------------------------------------------------------------------------
# Versions and git
# ---------------------------------------------------------------------------
#
# This script needs a new version on every run — a version names one binary
# per architecture for the life of the mesh — and git needs versions to mean
# releases. Those pulled against each other: the script used to bump
# `Cargo.toml` and leave it bumped, so the workspace version was really a
# build counter, and every dev build showed up as a tree change.
#
# The two are separated by *what kind* of version each one gets:
#
#   dev build   0.7.3-dev.1, 0.7.3-dev.2, …   this script, never committed
#   release     0.7.3                          you, committed and tagged
#
# semver already orders these the way both ends need. `0.7.3-dev.1` is above
# `0.7.2`, so nodes on the release accept a dev build as an upgrade; and it
# is below `0.7.3`, so cutting the real release later still supersedes every
# dev build that preceded it. `mesh_proto::version_cmp` is the one place
# that comparison is made, and it uses the `semver` crate rather than
# splitting on dots — which is exactly why this works.
#
# `Cargo.toml` still has to carry the version at build time (the binary
# reports `CARGO_PKG_VERSION`, and every node compares against it), so it is
# edited — and then **put straight back**, along with `Cargo.lock`. Running
# this leaves `git status` clean.
#
# To cut an actual release: set the version in `Cargo.toml` yourself, commit
# it, tag `v<version>`, and let `.github/workflows/release.yml` do the rest.
# `packaging/build-release.sh` is the same pipeline locally — every
# architecture, tarballs, .deb/.rpm, SHA256SUMS.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_build-common.sh"
ROOT="$(build_root)"
cd "$ROOT"

TARGET=x86_64-unknown-linux-musl
DIST="$ROOT/dist"
HOME_DIR="${MESHWARE_HOME:-$HOME/.meshware}"
KEY="${MINISIGN_KEY:-$HOME/.minisign/meshware-release.key}"
PUB="${KEY%.key}.pub"

STEP=0
step() { STEP=$((STEP + 1)); echo; echo "==> [$STEP/6] $*"; }
die()  { echo "error: $*" >&2; exit 1; }

case "${1:-}" in
  -h|--help) sed -n '2,9p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
esac

command -v minisign >/dev/null 2>&1 \
  || die "minisign not installed (dnf install minisign / apt install minisign)"
require_musl "$TARGET"

# -- 1. a version the fleet has not seen ------------------------------------
step "choosing a version"
CURRENT="$(cargo_version)"
if [ -n "${1:-}" ]; then
  VERSION="${1#v}"
  echo "    ${VERSION} (asked for)"
else
  # A pre-release of the *next* patch, counted past anything already in
  # dist/ — building twice in a row should not hand you a version that
  # collides with the one you built a minute ago. `${CURRENT%%-*}` so this
  # still works if somebody left a pre-release in the manifest by hand.
  IFS=. read -r MA MI PA <<< "${CURRENT%%-*}"
  PA=$((PA + 1))
  N=1
  while [ -e "${DIST}/meshnode-v${MA}.${MI}.${PA}-dev.${N}-${TARGET}" ]; do
    N=$((N + 1))
  done
  VERSION="${MA}.${MI}.${PA}-dev.${N}"
  echo "    ${CURRENT} → ${VERSION}"
fi
[ ! -e "${DIST}/meshnode-v${VERSION}-${TARGET}" ] \
  || die "dist/ already holds ${VERSION} — pass a different version"

# -- 2. write it down -------------------------------------------------------
#
# The binary reports Cargo.toml's version and every node compares against
# it, so the bump has to happen before the build, not after. It is put back
# either way — after a failure, so the tree never claims a version it did
# not produce, and after a success, because a dev build is not a release
# and has no business showing up in `git status`.
#
# Cargo.lock carries the workspace members' versions too, so it is part of
# the same backup. Restoring only the manifest would leave the lockfile
# dirty and look like the script half-worked.
step "setting the workspace version"
TOML_BAK="$(mktemp)"
LOCK_BAK="$(mktemp)"
cp Cargo.toml "$TOML_BAK"
cp Cargo.lock "$LOCK_BAK"
restore() {
  if [ -f "$TOML_BAK" ]; then cp "$TOML_BAK" Cargo.toml; rm -f "$TOML_BAK"; fi
  if [ -f "$LOCK_BAK" ]; then cp "$LOCK_BAK" Cargo.lock; rm -f "$LOCK_BAK"; fi
}
trap restore EXIT
sed -i "0,/^version = \"${CURRENT}\"/s//version = \"${VERSION}\"/" Cargo.toml
[ "$(cargo_version)" = "$VERSION" ] || die "could not set the version in Cargo.toml"
echo "    Cargo.toml: ${VERSION}"

# -- 3. build ---------------------------------------------------------------
step "building meshnode ($TARGET, release)"
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --target "$TARGET" -p meshware-node
BIN="$ROOT/target/$TARGET/release/meshnode"
[ -x "$BIN" ] || die "expected a binary at $BIN"
BUILT="$("$BIN" -V | awk '{print $2}')"
[ "$BUILT" = "$VERSION" ] \
  || die "built binary reports ${BUILT}, expected ${VERSION}"

# The binary carries the version now, so the tree does not have to. Put it
# back immediately rather than at exit: everything below talks to a running
# node and can fail, and none of it should be able to strand the manifest
# on a version that was never released.
restore
trap - EXIT
echo "    Cargo.toml back at ${CURRENT}"

# -- 4. a signing key -------------------------------------------------------
step "signing key"
if [ ! -f "$KEY" ]; then
  mkdir -p "$(dirname "$KEY")"
  # `-W` writes a password-free key. A release key that prompts cannot be
  # used from a script, and the thing it protects is on the same disk as
  # the passphrase would be.
  minisign -G -W -p "$PUB" -s "$KEY" >/dev/null
  echo "    generated $KEY"
elif [ ! -f "$PUB" ]; then
  # Only the secret half survived. minisign can recompute the public one.
  minisign -R -s "$KEY" -p "$PUB" >/dev/null
  echo "    recovered $PUB from the secret key"
fi
PUBKEY="$(grep -v '^untrusted comment' "$PUB" | tr -d '[:space:]')"
echo "    $PUBKEY"

# -- 5. into dist/, signed --------------------------------------------------
step "writing dist/"
mkdir -p "$DIST"
ARTIFACT="meshnode-v${VERSION}-${TARGET}"
install -m 755 "$BIN" "${DIST}/${ARTIFACT}"
minisign -HSm "${DIST}/${ARTIFACT}" -s "$KEY" -t "meshware v${VERSION}" >/dev/null
echo "    dist/${ARTIFACT}"
echo "    dist/${ARTIFACT}.minisig"

# -- 6. make sure the mesh trusts the key -----------------------------------
#
# Not "add the key" — *make sure*. Running this on every build would author
# an op every time to say something the mesh already knows.
step "release key, mesh-wide"
# Director runs its own node in `<home>/director` and serves the CLI socket
# beside it; a plain daemon lives in `<home>`. Asking each of them for the
# key list is both the reachability probe and the answer.
NODE_ARGS=()
KEYS=""
for candidate in "${HOME_DIR}/director" "${HOME_DIR}"; do
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
  echo "    no running node under ${HOME_DIR} — skipped."
  echo "    Start Director, then run this once:"
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

# -- done -------------------------------------------------------------------
report_binary "${DIST}/${ARTIFACT}" --probe-version
cat <<MSG
meshnode ${VERSION} is ready to roll out.

  Director → Mesh → Fleet → Updates → Publish a release
  → pick meshnode-v${VERSION}-${TARGET}, then start a rollout.

MSG
