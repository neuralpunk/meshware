#!/usr/bin/env bash
# Build release artifacts for meshnode (MASTERPLAN §15): static musl
# binaries, tarballs, the bare binary (what `meshnode upgrade --self`
# downloads and what Director publishes into the mesh, M24 §9), .deb/.rpm
# packages, SHA256SUMS, and minisign signatures. CI runs this; it also
# works locally.
#
#   packaging/build-release.sh v0.6.0 [dist-dir]
#
# Requirements: rustup targets *-unknown-linux-musl, a musl cross linker
# (musl-gcc / aarch64-linux-gnu-gcc or `cargo zigbuild`), and optionally
# cargo-deb, cargo-generate-rpm.
#
# ---------------------------------------------------------------------------
# Signing
# ---------------------------------------------------------------------------
#
# **A release is signed or it is not a release.** Every node verifies the
# minisign signature before it replaces its own binary, so an unsigned
# artifact is one no machine will install — and a script that produces one
# silently has produced nothing but a surprise for later. So the key is
# resolved once, up front, and a build that cannot sign says so *before* it
# spends four minutes compiling:
#
#   1. $MINISIGN_KEY
#   2. [signing] key in packaging/release.conf  (git-ignored, per-machine)
#   3. ~/.minisign/meshware-release.key
#
# `--unsigned` is the deliberate escape hatch for a throwaway dev build, and
# it prints what that costs.
#
# ---------------------------------------------------------------------------
# Targets
# ---------------------------------------------------------------------------
#
# A machine that can only build one architecture is the normal case
# locally, and it used to take the whole script down at the *second*
# target — after the first had built, before anything was checksummed or
# signed, leaving a dist/ that looked complete and was not. Each target now
# stands alone: a toolchain that isn't there is reported and skipped, and
# what did build is signed and usable. $TARGETS overrides the list.

set -euo pipefail

usage() {
  cat >&2 <<EOF
usage: build-release.sh <version-tag> [dist-dir] [--unsigned] [--force]

  <version-tag>   v0.6.0 (or 0.6.0 — a leading v is added)
  [dist-dir]      default: dist
  --unsigned      build without signing (dev only; no node will install it)
  --force         overwrite artifacts already in dist-dir for this version

  TARGETS="x86_64-unknown-linux-musl"   build one architecture
  MINISIGN_KEY=/path/to/key             signing key (see packaging/release.conf)
EOF
  exit 2
}

VERSION=""
DIST="dist"
UNSIGNED=0
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --unsigned) UNSIGNED=1 ;;
    --force) FORCE=1 ;;
    -h|--help) usage ;;
    -*) echo "unknown option: $arg" >&2; usage ;;
    *) if [ -z "$VERSION" ]; then VERSION="$arg"; else DIST="$arg"; fi ;;
  esac
done
[ -n "$VERSION" ] || usage
case "$VERSION" in v*) ;; *) VERSION="v${VERSION}" ;; esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# The version in Cargo.toml is what the built binary reports and what every
# node compares against. A tag that disagrees produces a release no node
# will install — caught here rather than on the fleet.
CARGO_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [ "${VERSION#v}" != "$CARGO_VERSION" ]; then
  echo "error: building ${VERSION} but Cargo.toml says ${CARGO_VERSION}." >&2
  echo "       Bump the workspace version first — the binary reports Cargo.toml's," >&2
  echo "       and a node that fetches ${VERSION} and finds ${CARGO_VERSION} refuses it." >&2
  exit 1
fi

# The commit `meshnode --version` names (DOC-14), read at compile time.
if [ -z "${MESHWARE_BUILD_COMMIT:-}" ]; then
  MESHWARE_BUILD_COMMIT="$(git rev-parse --short=12 HEAD 2>/dev/null || true)"
fi
export MESHWARE_BUILD_COMMIT

# …and one nobody wrote release notes for is refused too (DOC-7): what
# changed, what might stop working and what to do about it, in
# CHANGELOG.md, before four minutes of compiling rather than after.
scripts/changelog-entry.sh "$VERSION" || exit 1

# A version is a promise about a *specific* binary, and dist/ is where the
# ones already handed out live. Rebuilding over them replaces a signed
# artifact with a different one carrying the same version — which nothing
# downstream can notice: `upgrade --self` compares versions and reports
# "already up to date", so the fleet quietly keeps the old build while
# dist/ says otherwise. Bumping the workspace version is nearly always the
# real intent; --force is for the case where it genuinely is not.
if [ "$FORCE" -eq 0 ] && compgen -G "${DIST}/meshnode-${VERSION}-*" >/dev/null 2>&1; then
  echo "error: ${DIST}/ already holds ${VERSION} artifacts:" >&2
  for f in "${DIST}/meshnode-${VERSION}"-*; do
    case "$f" in *.minisig) continue ;; esac
    echo "         $(basename "$f")" >&2
  done
  cat >&2 <<EOF

       Rebuilding over them ships a different binary under a version the
       fleet already has, and \`meshnode upgrade\` compares versions: nodes
       on ${VERSION#v} will report "already up to date" and keep the old one.

       Bump the workspace version in Cargo.toml and build that instead:

         sed -i 's/^version = "${VERSION#v}"/version = "<next>"/' Cargo.toml
         packaging/build-release.sh v<next>

       Or, if replacing these really is what you mean: --force
EOF
  exit 1
fi

IFS=' ' read -r -a TARGET_LIST <<< "${TARGETS:-x86_64-unknown-linux-musl aarch64-unknown-linux-musl}"

# -- signing key, resolved before anything is built -------------------------
CONF="${REPO_ROOT}/packaging/release.conf"
if [ -z "${MINISIGN_KEY:-}" ] && [ -f "$CONF" ]; then
  # shellcheck disable=SC1090
  MINISIGN_KEY="$(sed -n 's/^[[:space:]]*key[[:space:]]*=[[:space:]]*//p' "$CONF" | head -1 | tr -d '"')"
  MINISIGN_KEY="${MINISIGN_KEY/#\~/$HOME}"
fi
if [ -z "${MINISIGN_KEY:-}" ] && [ -f "$HOME/.minisign/meshware-release.key" ]; then
  MINISIGN_KEY="$HOME/.minisign/meshware-release.key"
fi

if [ "$UNSIGNED" -eq 1 ]; then
  echo "== --unsigned: artifacts will NOT be signed."
  echo "   No node will install them: meshnode verifies every release"
  echo "   before it replaces a binary. Dev builds only."
  MINISIGN_KEY=""
else
  if [ -z "${MINISIGN_KEY:-}" ]; then
    cat >&2 <<EOF
error: no signing key, and a release is signed or it is not a release.

Generate one (once):

  mkdir -p ~/.minisign
  minisign -G -p ~/.minisign/meshware-release.pub \\
              -s ~/.minisign/meshware-release.key

Then either leave it at that path, export MINISIGN_KEY, or write
packaging/release.conf (git-ignored):

  key = ~/.minisign/meshware-release.key

Publish the *public* half to the mesh once, from Director → Updates →
Release keys, and every node — including ones that join later — will
trust builds signed with it.

To build without signing anyway: --unsigned
EOF
    exit 1
  fi
  if [ ! -f "$MINISIGN_KEY" ]; then
    echo "error: signing key not found: $MINISIGN_KEY" >&2
    exit 1
  fi
  command -v minisign >/dev/null 2>&1 || {
    echo "error: minisign not installed (dnf install minisign / apt install minisign)" >&2
    exit 1
  }
  echo "== signing with $MINISIGN_KEY"
fi

mkdir -p "$DIST"

build() {
  local target="$1"
  rustup target add "$target" >/dev/null 2>&1 || return 1
  if command -v cargo-zigbuild >/dev/null 2>&1; then
    cargo zigbuild -p meshware-node --release --target "$target"
  else
    # The same `CC_<target>` the dev script sets, so the two share a cargo
    # fingerprint. Without it, alternating between
    # `scripts/build-node-release.sh` and this script rebuilt ring — and
    # therefore everything above it — every single time, for no change in
    # output. Only set when the linker is actually there; `cc` finds its
    # own compiler otherwise, and a wrong value is worse than none.
    local var="CC_${target//-/_}"
    case "$target" in
      x86_64-unknown-linux-musl)
        command -v musl-gcc >/dev/null 2>&1 && export "$var=musl-gcc" ;;
      aarch64-unknown-linux-musl)
        command -v aarch64-linux-gnu-gcc >/dev/null 2>&1 \
          && export "$var=aarch64-linux-gnu-gcc" ;;
    esac
    cargo build -p meshware-node --release --target "$target"
  fi
}

BUILT=()
SKIPPED=()
for target in "${TARGET_LIST[@]}"; do
  echo "== building $target"
  # Deliberately not fatal: see the header. One arch that cannot build here
  # must not discard the one that can.
  if ! build "$target"; then
    echo "   !! $target could not be built here — skipping it."
    echo "      (missing rustup target or musl cross-linker; cargo install cargo-zigbuild fixes most cases)"
    SKIPPED+=("$target")
    continue
  fi
  arch="${target%%-*}"
  artifact="meshnode-${VERSION}-${target}.tar.gz"
  tar -czf "${DIST}/${artifact}" \
    -C "target/${target}/release" meshnode \
    -C "${REPO_ROOT}/packaging/systemd" meshnode.service \
    -C "${REPO_ROOT}" LICENSE-MIT LICENSE-APACHE \
    $([ -f target/man/meshnode.1 ] && echo "-C ${REPO_ROOT}/target/man meshnode.1")
  echo "   ${DIST}/${artifact}"
  # Bare binary: `meshnode upgrade --self` fetches exactly this file (plus
  # its .minisig), and it is the file Director publishes into the mesh — no
  # tar, no extraction, verified then renamed into place.
  cp "target/${target}/release/meshnode" "${DIST}/meshnode-${VERSION}-${target}"
  echo "   ${DIST}/meshnode-${VERSION}-${target}"
  BUILT+=("$target")

  # Man pages, from the first build this machine can run. They are the
  # same text for every architecture (clap's definitions), and the .deb /
  # .rpm metadata ships target/man/meshnode.1 — so a foreign-only build
  # on a host that cannot run it packages without one.
  if [ ! -f target/man/meshnode.1 ] && "target/${target}/release/meshnode" --version >/dev/null 2>&1; then
    "target/${target}/release/meshnode" manpage --dir target/man
    mkdir -p "${DIST}/man"
    cp target/man/*.1 "${DIST}/man/"
    echo "   ${DIST}/man/ ($(ls target/man | wc -l) pages)"
  fi

  # -- meshware-archive (M31 B3/B4) ----------------------------------------
  #
  # Its **own** tarball, never inside meshnode's. This binary links
  # DataFusion and Arrow and is an order of magnitude larger than the
  # daemon; folding it in would trade the thing that makes meshnode worth
  # deploying — one small binary you can drop on anything — for a feature
  # that belongs on whichever machine keeps the archive.
  #
  # It is shipped rather than left to `cargo install` because two things
  # already assume it exists: a node serving a peer's History browse spawns
  # it (B3), and the archive timer runs it (B4). Not building it is not
  # fatal — a release without it is a release without an archive reader,
  # which is a smaller problem than no release.
  # **Errors are not discarded.** This used to end `2>/dev/null`, so a
  # binary that stopped compiling — as this one did when the usage-report
  # IPC was retired — produced the same one-line "skipping it" as a missing
  # cross-target toolchain, and every release for weeks shipped without the
  # archive reader that a node spawns to answer History.
  if cargo build -p mesh-archive --features cli --bin meshware-archive \
       --release --target "$target"; then
    tar -czf "${DIST}/meshware-archive-${VERSION}-${target}.tar.gz" \
      -C "target/${target}/release" meshware-archive \
      -C "${REPO_ROOT}/packaging/systemd" meshware-archive.service meshware-archive.timer \
      -C "${REPO_ROOT}" LICENSE-MIT LICENSE-APACHE
    echo "   ${DIST}/meshware-archive-${VERSION}-${target}.tar.gz"
  else
    echo "   !! meshware-archive could not be built for $target — skipping it."
  fi

  if command -v cargo-deb >/dev/null 2>&1; then
    cargo deb -p meshware-node --target "$target" --no-build \
      -o "${DIST}/meshnode_${VERSION#v}_${arch}.deb" || true
  fi
  if command -v cargo-generate-rpm >/dev/null 2>&1; then
    cargo generate-rpm -p crates/meshware-node --target "$target" \
      -o "${DIST}/meshnode-${VERSION#v}.${arch}.rpm" || true
  fi
done

if [ ${#BUILT[@]} -eq 0 ]; then
  echo "error: nothing built — every target failed." >&2
  exit 1
fi

# -- the Python SDK (DOC-8) ------------------------------------------------
#
# One wheel per release, at the release's version, beside the binaries it
# speaks to: `pip install meshware-<version>-py3-none-any.whl`. The SDK
# refuses a daemon from any other release, so this is the only place it can
# usefully live. Not fatal, like a foreign target: a machine without pip
# still produces a release, and says what it left out.
echo "== python sdk"
if ! "$REPO_ROOT/scripts/build-python-sdk.sh" "$DIST" "${VERSION#v}"; then
  echo "   !! the Python SDK wheel could not be built here — skipping it."
fi

# Checksums over every artifact (tarballs, bare binaries, .deb, .rpm, the
# SDK wheel).
(cd "$DIST" && sha256sum meshnode-* meshnode_* meshware-*.whl 2>/dev/null > SHA256SUMS || sha256sum meshnode-* > SHA256SUMS)

# Release signing: minisign (MASTERPLAN open question #1 — resolved in the
# M6 amendment). `-H` prehashes (BLAKE2b) so signing large binaries
# streams; `meshnode upgrade --self`, install.sh and Director's publish
# path all accept prehashed signatures.
if [ -n "${MINISIGN_KEY:-}" ]; then
  # One invocation for every artifact. minisign takes a file list, and a
  # password-protected key otherwise prompts once *per file* — four
  # prompts for one release, which is both tedious and unscriptable. A key
  # with no password (`minisign -C -s <key> -W`) makes this silent.
  TO_SIGN=()
  for f in "$DIST"/meshnode-* "$DIST"/meshnode_* "$DIST"/meshware-*.whl "$DIST/SHA256SUMS"; do
    [ -e "$f" ] || continue
    case "$f" in *.minisig) continue ;; esac
    TO_SIGN+=("$f")
  done
  if [ ${#TO_SIGN[@]} -gt 0 ]; then
    minisign -HSm "${TO_SIGN[@]}" -s "$MINISIGN_KEY" -t "meshware ${VERSION}"
    echo "== ${#TO_SIGN[@]} artifacts signed with minisign (prehashed)"
  fi
fi

# -- what Director publishes ------------------------------------------------
#
# A machine-readable index of this build, so publishing is a *choice from a
# list* rather than a path typed from memory. Director reads it to offer
# "publish this build" without anyone naming a file.
{
  echo "{"
  echo "  \"version\": \"${VERSION#v}\","
  echo "  \"tag\": \"${VERSION}\","
  echo "  \"signed\": $([ -n "${MINISIGN_KEY:-}" ] && echo true || echo false),"
  echo "  \"artifacts\": ["
  first=1
  for target in "${BUILT[@]}"; do
    bare="meshnode-${VERSION}-${target}"
    [ -e "${DIST}/${bare}" ] || continue
    [ $first -eq 1 ] || echo ","
    first=0
    printf '    {"arch": "%s", "target": "%s", "binary": "%s", "minisig": "%s"}' \
      "${target%%-*}" "$target" "$bare" \
      "$([ -e "${DIST}/${bare}.minisig" ] && echo "${bare}.minisig" || echo "")"
  done
  echo
  echo "  ]"
  echo "}"
} > "${DIST}/release.json"

echo "== done: $(ls "$DIST" | wc -l) files in ${DIST}/"
if [ ${#SKIPPED[@]} -gt 0 ]; then
  echo "== NOT built: ${SKIPPED[*]}"
  echo "   Nodes on those architectures cannot be upgraded from this build."
fi
if [ -n "${MINISIGN_KEY:-}" ]; then
  echo
  echo "Publish it: Director → Nodes → Updates → Publish a release,"
  echo "and pick ${DIST}/release.json (or the bare binary directly)."
fi
