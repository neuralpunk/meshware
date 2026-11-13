#!/usr/bin/env bash
# Build the **debug** meshnode binary: static x86_64 musl, unoptimized,
# with debug symbols and assertions.
#
# Same musl target as the release build on purpose — a debug binary you
# cannot run on the node you are debugging is not worth building. It is
# large (hundreds of MB) and slow, which is fine locally and is why
# `meshnode upgrade --publish` will refuse it: the release channel caps a
# download at 256 MB.
#
#   ./scripts/build-node-debug.sh                 build, print the path
#   ./scripts/build-node-debug.sh user@server     …and scp it there
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_build-common.sh"
ROOT="$(build_root)"
cd "$ROOT"

TARGET=x86_64-unknown-linux-musl
require_musl "$TARGET"

echo "==> building meshnode ($TARGET, debug)"
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --target "$TARGET" -p meshware-node

BIN="$ROOT/target/$TARGET/debug/meshnode"
report_binary "$BIN" --probe-version

echo "run it with logs:"
echo "  RUST_LOG=debug $BIN up"
if [ -n "${1:-}" ]; then
  echo
  echo "==> copying to $1:~/meshnode-debug"
  scp "$BIN" "$1:~/meshnode-debug"
fi
