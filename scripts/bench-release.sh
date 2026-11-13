#!/usr/bin/env bash
# Benchmark the build that actually ships (ENG-25).
#
# **Every published performance number came from a glibc test build.**
# Releases are static musl, and musl's allocator serialises under thread
# contention — so the proof points described a build nobody runs. This
# measures the shipped target, and measures the alternative beside it, so
# the difference is a number rather than an opinion.
#
#   ./scripts/bench-release.sh                 # the shipped target (musl)
#   ./scripts/bench-release.sh --compare       # …and glibc, for the delta
#   ./scripts/bench-release.sh --json
#
# ## What it measures, and what that is worth
#
# The same two harnesses the operational-cost page publishes:
#
#   100 nodes up     a hundred nodes joining and starting, concurrently
#   convergence      membership, then the op log, across all hundred
#   submit→complete  one job's round trip on the loaded mesh
#
# Built for the release target and run natively, so the libc and the
# allocator are the ones that ship. It is **one process holding a hundred
# nodes**, which concentrates allocator contention far past what a real
# deployment sees — one node per machine, one process each. That makes the
# startup figure a stress measurement rather than a forecast, and it is
# still the right one to watch: it is the number the scale claim is made
# from, and a regression shows here first.
#
# The numbers on a 16-core developer machine, 2026-09-17:
#
#   build                100 nodes up   submit→complete
#   glibc                      9.0 s          76 ms
#   musl (as shipped then)    12.6 s          72 ms
#   musl + mimalloc            9.5 s          25 ms
#   glibc + mimalloc           9.6 s          26 ms
#
# Two findings, and they are separate. musl's 40% startup penalty is the
# contention the item suspected; mimalloc removes it. The three-fold drop
# in the round trip is *not* musl's — glibc + mimalloc measures the same —
# so it is an allocator win on the path bet #3 calls the product.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="${MESHWARE_BENCH_TARGET:-x86_64-unknown-linux-musl}"
COMPARE=0
AS_JSON=0
for arg in "$@"; do
  case "$arg" in
    --compare) COMPARE=1 ;;
    --json) AS_JSON=1 ;;
    -h | --help) sed -n '2,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "bench-release.sh: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

say() { [ "$AS_JSON" = 1 ] || printf '%s\n' "$*" >&2; }

# musl needs a C compiler for ring's assembly and for the allocator. The
# release workflow uses cargo-zigbuild; a developer machine usually has
# `musl-gcc` from the distribution's musl-devel, and either is fine.
if [ -z "${CC_x86_64_unknown_linux_musl:-}" ] && command -v musl-gcc >/dev/null; then
  export CC_x86_64_unknown_linux_musl=musl-gcc
fi

# `cargo test --no-run` prints the path of what it built; taking it from
# there rather than globbing `target/*/deps/scale-*` avoids running a
# stale binary from an earlier build, which is the one way this script
# could report a number for code that is not in the tree.
build_scale() { # build_scale [--target <triple>] → echoes the test binary
  local out
  out="$(cargo test --release --quiet "$@" -p mesh-core --test scale --no-run --message-format=json 2>/dev/null \
    | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == "scale":
        print(m["executable"])
')"
  echo "$out" | tail -1
}

# One run is a sample, not a measurement: a 100-node start on a shared
# machine varies by a second either way. Three, reported as the median,
# which is what a reader would do with three numbers anyway.
RUNS="${MESHWARE_BENCH_RUNS:-3}"

median() { sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'; }

bench() { # bench <label> <binary> → "<label> <start_s> <round_trip_ms>"
  local label="$1" bin="$2"
  local starts=() trips=()
  for _ in $(seq "$RUNS"); do
    local out
    out="$("$bin" --nocapture --test-threads=1 hundred_node_convergence 2>&1 || true)"
    starts+=("$(sed -n 's/.*joined+started in \([0-9.]*\)s.*/\1/p' <<< "$out" | head -1)")
    trips+=("$(sed -n 's/.*round trip on the loaded mesh: \([0-9.]*\)ms.*/\1/p' <<< "$out" | head -1)")
  done
  local s t
  s="$(printf '%s\n' "${starts[@]}" | median)"
  t="$(printf '%s\n' "${trips[@]}" | median)"
  say "$(printf '  %-22s %8.1f s %10.1f ms' "$label" "$s" "$t")"
  echo "$label $s $t"
}

say "==> building for $TARGET (the shipped target)"
SHIPPED="$(build_scale --target "$TARGET")"
[ -x "$SHIPPED" ] || { echo "bench-release.sh: no test binary for $TARGET" >&2; exit 1; }

say ""
say "  build                  100 up      round trip"
say "  ---------------------- ---------- ------------"
read -r _ SHIP_START SHIP_TRIP <<< "$(bench "$TARGET" "$SHIPPED")"

HOST_START=null
HOST_TRIP=null
if [ "$COMPARE" = 1 ]; then
  say "==> …and the host toolchain, for the delta"
  HOST="$(build_scale)"
  read -r _ HOST_START HOST_TRIP <<< "$(bench "host (glibc)" "$HOST")"
fi

if [ "$AS_JSON" = 1 ]; then
  printf '{"target":"%s","runs":%s,"shipped":{"start_secs":%s,"round_trip_ms":%s},"host":{"start_secs":%s,"round_trip_ms":%s}}\n' \
    "$TARGET" "$RUNS" "$SHIP_START" "$SHIP_TRIP" "$HOST_START" "$HOST_TRIP"
else
  say ""
  say "  median of $RUNS run(s). These are the numbers docs/operational-cost.md"
  say "  publishes; update it when they move."
fi
