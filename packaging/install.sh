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
  # A project release key (minisign), if this copy of the script was made
  # by someone who signs their builds — it must equal line 2 of
  # packaging/minisign.pub (CI asserts it). Meshware itself has none: its
  # releases are checked against GitHub's build-provenance attestation, and
  # each mesh signs what it installs with its own admins' keys.
  MINISIGN_PUBKEY="RWTplaceholderplaceholderplaceholderplaceholderplaceho"

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
  say "upgrade later:       run this installer again, or (a fleet) meshnode upgrade --publish"
}

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }

# Verify the download, and be honest about it when we cannot (S-M4).
#
# Meshware has no project-wide signing key, on purpose: nobody has to keep
# one safe for every fleet to keep working. What proves a release is
# GitHub's build-provenance attestation — signed by GitHub's keyless
# Sigstore flow, tying the file to this repository's release workflow — and
# `gh attestation verify` checks it. `SHA256SUMS` proves much less: it is
# served from the same place as the artifact, so it catches a corrupt
# download and never a substituted one. This says which of the two
# happened, every time.
verify() {
  tmp="$1"; artifact="$2"; base="$3"

  if [ "${MESHWARE_NO_VERIFY:-0}" = "1" ]; then
    say ""
    say "!! MESHWARE_NO_VERIFY=1 — the download was not checked at all."
    say ""
    return 0
  fi

  # A fork that signs its builds: check the signature, and that is the whole
  # answer.
  case "$MINISIGN_PUBKEY" in
    RWTplaceholder*) ;;
    *)
      if command -v minisign >/dev/null 2>&1; then
        curl -fsSL -o "${tmp}/${artifact}.minisig" "${base}/${artifact}.minisig"
        minisign -Vm "${tmp}/${artifact}" -P "$MINISIGN_PUBKEY" >/dev/null ||
          die "signature verification FAILED — refusing to install"
        say "signature verified (minisign)"
        return 0
      fi
      ;;
  esac

  # Meshware's own releases: the checksum catches a corrupt download, and
  # GitHub's build-provenance attestation proves the tarball was built from
  # this repository by its release workflow — no key anybody has to keep.
  if curl -fsSL -o "${tmp}/SHA256SUMS" "${base}/SHA256SUMS" 2>/dev/null &&
     command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && grep " ${artifact}\$" SHA256SUMS | sha256sum -c -) >/dev/null ||
      die "checksum mismatch — refusing to install"
    say "checksum matches SHA256SUMS"
  else
    say "!! could not fetch SHA256SUMS (or no sha256sum here): not checked for corruption either"
  fi
  if command -v gh >/dev/null 2>&1 &&
     gh attestation verify "${tmp}/${artifact}" --repo "$REPO" >/dev/null 2>&1; then
    say "provenance verified: built by github.com/${REPO}'s release workflow (gh attestation)"
    return 0
  fi

  if [ "${MESHWARE_REQUIRE_SIGNATURE:-0}" = "1" ]; then
    die "could not verify provenance and MESHWARE_REQUIRE_SIGNATURE=1 — install the GitHub CLI (gh), run 'gh auth login', and retry"
  fi

  say ""
  say "!! PROVENANCE WAS NOT CHECKED."
  say "!!"
  say "!! The checksum came from the same server as the binary, so it catches"
  say "!! corruption and never tampering. To prove the tarball was built from"
  say "!! github.com/${REPO} by its release workflow, install the GitHub CLI and run:"
  say "!!   gh attestation verify ${artifact} --repo ${REPO}"
  say "!! (or run this installer again with gh installed and logged in)."
  say "!!"
  say "!! Set MESHWARE_REQUIRE_SIGNATURE=1 to make this a hard failure."
  say ""
}

main "$@"
