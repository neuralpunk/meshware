#!/usr/bin/env bash
# Build the **release** meshnode binary: static x86_64 musl, optimized —
# the one you deploy, and the one `meshnode upgrade --publish` expects.
#
#   ./scripts/build-node-release.sh                 build, print the path
#   ./scripts/build-node-release.sh user@server     …and scp it there
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_build-common.sh"
ROOT="$(build_root)"
cd "$ROOT"

TARGET=x86_64-unknown-linux-musl
require_musl "$TARGET"

echo "==> building meshnode ($TARGET, release)"
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --target "$TARGET" -p meshware-node

BIN="$ROOT/target/$TARGET/release/meshnode"
report_binary "$BIN" --probe-version

if [ -n "${1:-}" ]; then
  echo "==> copying to $1:~/meshnode"
  scp "$BIN" "$1:~/meshnode"
  echo "on the server, install and restart with:"
  echo "  sudo install -m 755 ~/meshnode /usr/local/bin/meshnode && sudo systemctl restart meshnode 2>/dev/null || true"
else
  echo "deploy it by hand:"
  echo "  scp $BIN user@server:~/meshnode"
  echo
  echo "…or put it in the mesh and let the fleet pull it from each other (M24 §9):"
  echo "  meshnode upgrade --publish $BIN --version \$(\"$BIN\" -V | awk '{print \$2}')"
fi
