#!/bin/sh
# meshware installer: curl | sh to a working node in
# under two minutes. POSIX sh, no surprises: detect arch, fetch the static
# musl binary, verify its minisign signature, install, done.
#
#   curl -fsSL https://raw.githubusercontent.com/neuralpunk/meshware/main/packaging/install.sh | sh
#
# Environment overrides:
#   MESHWARE_VERSION            release tag to install (default: latest)
#   MESHWARE_BIN_DIR            install dir (default: /usr/local/bin, else ~/.local/bin)
#   MESHWARE_REQUIRE_SIGNATURE  set to 1 to refuse to install unless the
#                               signature was actually checked
#   MESHWARE_NO_VERIFY          set to 1 to skip verification entirely (not advised)
#
# **The whole script is a function, called on the last line** (S-M4). A
# `curl | sh` that is cut off mid-transfer — a dropped connection, a proxy
# that truncates — hands `sh` a prefix of this file, and `sh` runs a prefix
# happily. Wrapped like this, a truncated download defines part of a
# function and then reaches EOF without ever calling it, so a half-installer
# does nothing instead of doing half.

set -eu

main() {
  REPO="neuralpunk/meshware"
  # The meshware release signing key (minisign) — must equal line 2 of
  # packaging/minisign.pub (CI asserts it; `meshnode upgrade --self` embeds
  # the same file). Rotations are announced in release notes and the
  # previous key signs the announcement.
  MINISIGN_PUBKEY="RWQa4oJEOptFbp8PrU2VnyoJqZFG36/x4p1goF965ZvZxsXH7mUYxwux"

  command -v curl >/dev/null 2>&1 || die "curl is required"

  case "$(uname -s)" in
    Linux) ;;
    *) die "meshnode runs on Linux only (D10)" ;;
  esac

  case "$(uname -m)" in
    x86_64 | amd64) ARCH="x86_64" ;;
    aarch64 | arm64) ARCH="aarch64" ;;
    *) die "unsupported architecture: $(uname -m) (x86_64 and aarch64 ship)" ;;
  esac

  VERSION="${MESHWARE_VERSION:-latest}"
  if [ "$VERSION" = "latest" ]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" |
      grep '"tag_name"' | head -n1 | cut -d'"' -f4) || true
    [ -n "$VERSION" ] || die "cannot determine the latest release"
  fi

  ARTIFACT="meshnode-${VERSION}-${ARCH}-unknown-linux-musl.tar.gz"
  BASE="https://github.com/${REPO}/releases/download/${VERSION}"

  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT INT TERM

  say "downloading meshnode ${VERSION} (${ARCH})…"
  curl -fsSL -o "${TMP}/${ARTIFACT}" "${BASE}/${ARTIFACT}"

  verify "$TMP" "$ARTIFACT" "$BASE"

  tar -xzf "${TMP}/${ARTIFACT}" -C "$TMP"

  BIN_DIR="${MESHWARE_BIN_DIR:-}"
  if [ -z "$BIN_DIR" ]; then
    if [ -w /usr/local/bin ]; then
      BIN_DIR=/usr/local/bin
    else
      BIN_DIR="${HOME}/.local/bin"
      mkdir -p "$BIN_DIR"
    fi
  fi
  install -m 0755 "${TMP}/meshnode" "${BIN_DIR}/meshnode"
  say "installed ${BIN_DIR}/meshnode"

  case ":${PATH}:" in
    *:"$BIN_DIR":*) ;;
    *) say "note: ${BIN_DIR} is not on your PATH" ;;
  esac

  say ""
  say "start a mesh:        meshnode up"
  say "join one:            meshnode join <token>   (mint with: meshnode invite create)"
  say "check the machine:   meshnode doctor"
  say "upgrade later:       meshnode upgrade --self"
}

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }

# Verify the download, and be honest about it when we cannot (S-M4).
#
# `minisign` proves the release came from whoever holds the meshware signing
# key. `SHA256SUMS` proves nothing of the sort: it is served from the same
# place as the artifact, so anyone able to substitute one can substitute the
# other. It detects a corrupted download and it is worth doing for that — it
# is *not* a substitute for a signature, and the previous version of this
# script said "checksum verified", which reads to a person like "this is the
# real thing".
#
# `minisign` is not installed by default on any mainstream distribution, so
# the unsigned path is the ordinary one. It therefore says so, in as many
# words, every time.
verify() {
  tmp="$1"; artifact="$2"; base="$3"

  if [ "${MESHWARE_NO_VERIFY:-0}" = "1" ]; then
    say ""
    say "!! MESHWARE_NO_VERIFY=1 — the download was not checked at all."
    say ""
    return 0
  fi

  case "$MINISIGN_PUBKEY" in
    RWTplaceholder*)
      # Before the first signed release the key in this file is a
      # placeholder, so a machine *with* minisign would fail hard while a
      # machine without it sailed through — exactly the wrong way round.
      # Say what is actually true instead.
      say ""
      say "!! This build of install.sh carries a PLACEHOLDER signing key, so no"
      say "!! signature can be checked. Do not use it to install on anything you"
      say "!! care about."
      say ""
      [ "${MESHWARE_REQUIRE_SIGNATURE:-0}" != "1" ] ||
        die "MESHWARE_REQUIRE_SIGNATURE=1 and there is no real key to verify against"
      return 0
      ;;
  esac

  if command -v minisign >/dev/null 2>&1; then
    curl -fsSL -o "${tmp}/${artifact}.minisig" "${base}/${artifact}.minisig"
    minisign -Vm "${tmp}/${artifact}" -P "$MINISIGN_PUBKEY" >/dev/null ||
      die "signature verification FAILED — refusing to install"
    say "signature verified (minisign)"
    return 0
  fi

  # No verifier on this machine. Check the checksum anyway — a truncated or
  # corrupted download is worth catching — and then say plainly that no
  # signature was checked.
  if curl -fsSL -o "${tmp}/SHA256SUMS" "${base}/SHA256SUMS" 2>/dev/null &&
     command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && grep " ${artifact}\$" SHA256SUMS | sha256sum -c -) >/dev/null ||
      die "checksum mismatch — refusing to install"
    checked="the download matches the published checksum (so it is not corrupt)"
  else
    checked="the download could not be checked at all"
  fi

  if [ "${MESHWARE_REQUIRE_SIGNATURE:-0}" = "1" ]; then
    die "minisign is not installed and MESHWARE_REQUIRE_SIGNATURE=1 — install minisign and retry"
  fi

  say ""
  say "!! NO SIGNATURE WAS CHECKED."
  say "!!"
  say "!! minisign is not installed, so ${checked} —"
  say "!! and the checksum came from the same server as the binary, so it"
  say "!! catches corruption and never tampering."
  say "!!"
  say "!! To verify properly, install minisign and run this again:"
  say "!!   Debian/Ubuntu  apt install minisign"
  say "!!   Fedora/RHEL    dnf install minisign"
  say "!!   Arch           pacman -S minisign"
  say "!!   Alpine         apk add minisign"
  say "!!"
  say "!! Set MESHWARE_REQUIRE_SIGNATURE=1 to make this a hard failure."
  say ""
}

main "$@"
