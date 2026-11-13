#!/usr/bin/env bash
# Build the **debug** Meshware Director: same pipeline, unoptimized, with
# debug symbols. Much faster to compile and very large on disk.
#
# `--features custom-protocol` is kept so the frontend is embedded and the
# binary runs standalone. Drop it and use `npm run tauri dev` instead when
# you want Vite's hot reload against a dev server.
#
#   ./scripts/build-director-debug.sh        build, print the path
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_build-common.sh"
ROOT="$(build_root)"
cd "$ROOT"

echo "==> regenerating TypeScript bindings from the Rust view models"
cargo test -p meshware-director --lib export_bindings --quiet >/dev/null

echo "==> building the frontend"
cd apps/director
[ -d node_modules ] || npm install
npm run build >/dev/null
cd "$ROOT"

echo "==> building the Director binary (debug, embedded frontend)"
cargo build -p meshware-director --features custom-protocol

BIN="$ROOT/target/debug/meshware-director"
report_binary "$BIN"

echo "run it with:"
echo "  RUST_LOG=debug $BIN"
