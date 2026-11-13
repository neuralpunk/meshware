#!/usr/bin/env bash
# Rebuild the static meshnode binary (x86_64 musl — runs on any x86_64
# Linux, no glibc/distro requirements) and print its full path.
#
#   ./scripts/build-node.sh                     build, print path
#   ./scripts/build-node.sh user@server         …and scp it to ~/meshnode there
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET=x86_64-unknown-linux-musl

if ! command -v musl-gcc >/dev/null 2>&1; then
  echo "error: musl-gcc not found — install it first (Fedora: sudo dnf install musl-gcc)" >&2
  exit 1
fi
rustup target list --installed | grep -q "$TARGET" || rustup target add "$TARGET"

echo "==> building meshnode ($TARGET, release)"
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target "$TARGET" -p meshware-node

BIN="$ROOT/target/$TARGET/release/meshnode"
echo
echo "  binary : $BIN"
echo "  size   : $(du -h "$BIN" | cut -f1)"
echo "  sha256 : $(sha256sum "$BIN" | cut -d' ' -f1)"
echo "  verify : $("$BIN" --version | paste -sd ' ')"
echo

if [ -n "${1:-}" ]; then
  echo "==> copying to $1:~/meshnode"
  scp "$BIN" "$1:~/meshnode"
  echo "on the server, install and restart with:"
  echo "  sudo install -m 755 ~/meshnode /usr/local/bin/meshnode && sudo systemctl restart meshnode 2>/dev/null || true"
else
  echo "copy it with:"
  echo "  scp $BIN user@server:~/meshnode"
fi
