#!/usr/bin/env bash
# Rebuild and run Meshware Director (the full pipeline: TS bindings →
# frontend → self-contained Tauri binary → launch).
#
#   ./scripts/director.sh            rebuild + run
#   ./scripts/director.sh --no-run   rebuild only
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> regenerating TypeScript bindings from the Rust view models"
cargo test -p meshware-director --lib export_bindings --quiet >/dev/null

echo "==> building the frontend"
cd apps/director
[ -d node_modules ] || npm install
npm run build >/dev/null
cd "$ROOT"

echo "==> building the Director binary (embedded frontend)"
cargo build -p meshware-director --features custom-protocol

BIN="$ROOT/target/debug/meshware-director"
echo "==> built: $BIN"

if [ "${1:-}" = "--no-run" ]; then
  exit 0
fi

# Two Directors would fight over the same data dir; replace a running one.
if pgrep -x meshware-director >/dev/null 2>&1; then
  echo "==> stopping the running Director"
  pkill -x meshware-director || true
  sleep 1
fi

echo "==> launching (RUST_LOG=${RUST_LOG:-info})"
exec "$BIN"
