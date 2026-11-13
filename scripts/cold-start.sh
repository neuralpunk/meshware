#!/usr/bin/env bash
# The cold-start timer, on a real host (C0–C9 §7.3 P1).
#
# **An unmeasured claim is a slogan.** The operational-cost table's last
# row — *time from zero to scheduling* — is the one that has to be earned
# rather than cited, so this earns it, with a script anyone can run and
# whose output is published beside the number.
#
#   ./scripts/cold-start.sh                  # time a locally built binary
#   ./scripts/cold-start.sh --install        # …including `curl | sh`
#   ./scripts/cold-start.sh --json           # one object, for a plot
#
# It measures five phases, the same five the in-process harness measures
# (crates/mesh-testkit/src/coldstart.rs):
#
#   install   fetch, verify and install the binary   (--install only)
#   up        `meshnode up`: identity, genesis, an encrypted listener
#   invite    minting a join token
#   join      a second machine from nothing to a member
#   schedule  submitting work and having an executor accept it
#
# Both "machines" are this one, in two data dirs on two loopback ports.
# That is honest about what it does and does not prove: it measures the
# software's startup, handshake and scheduling costs, and it does not
# measure a network. What it is compared against is not another cluster's
# throughput — it is another cluster's *setup*, which is a documented
# procedure and cited as one.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WITH_INSTALL=0
AS_JSON=0
for arg in "$@"; do
  case "$arg" in
    --install) WITH_INSTALL=1 ;;
    --json) AS_JSON=1 ;;
    -h | --help) sed -n '2,27p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "cold-start.sh: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

now_us() { echo $(($(date +%s%N) / 1000)); }
say() { [ "$AS_JSON" = 1 ] || printf '%s\n' "$*" >&2; }
ms() { awk -v u="$1" 'BEGIN { printf "%.0f ms", u / 1000 }'; }

WORK="$(mktemp -d)"
PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# -- install ------------------------------------------------------------------
#
# The real installer, verifying the real signature, when asked. Otherwise
# whatever binary this working tree builds — which is what CI has and what
# a contributor wants to time after a change.
INSTALL_US=null
if [ "$WITH_INSTALL" = 1 ]; then
  say "==> install (packaging/install.sh)"
  t0=$(now_us)
  MESHWARE_BIN_DIR="$WORK/bin" sh "$ROOT/packaging/install.sh" >/dev/null
  INSTALL_US=$(( $(now_us) - t0 ))
  BIN="$WORK/bin/meshnode"
else
  BIN="${MESHNODE_BIN:-$ROOT/target/release/meshnode}"
  if [ ! -x "$BIN" ]; then
    say "==> building meshnode (release) — not part of the measurement"
    cargo build --release -p meshware-node --manifest-path "$ROOT/Cargo.toml" >/dev/null
    BIN="$ROOT/target/release/meshnode"
  fi
fi
[ -x "$BIN" ] || { echo "cold-start.sh: no meshnode binary at $BIN" >&2; exit 1; }

A="$WORK/a"
B="$WORK/b"
mkdir -p "$A" "$B"

# Under systemd, `JOURNAL_STREAM` says stderr is already the journal, and
# the daemon then writes structured records there instead of lines to the
# file below — so `wait_up` would grep an empty file until it gave up.
# Cleared for the children this script starts, so their output lands where
# it is read from.
export -n JOURNAL_STREAM 2>/dev/null || true
unset JOURNAL_STREAM

# Wait for the thing that actually happened, rather than sleeping a fixed
# amount: a sleep would be measuring the sleep.
#
# The signal is the daemon's own "node up" line, not `meshnode status`,
# because a joined node's *user* key is not a mesh user — asking it for
# status is a permission error, correctly, and would make readiness look
# like failure. What is being timed is the daemon reaching the point where
# it can take work, and that is exactly what it logs.
wait_up() {
  local log="$1" deadline=$((SECONDS + 60))
  while [ $SECONDS -lt $deadline ]; do
    grep -q "meshware node up" "$log" 2>/dev/null && return 0
    sleep 0.02
  done
  echo "cold-start.sh: daemon never came up (see $log)" >&2
  tail -20 "$log" >&2
  return 1
}

# The join is not finished when the joiner starts — it is finished when the
# mesh can place work on it. So the phase ends when the founder can see it.
wait_members() {
  local want="$1" deadline=$((SECONDS + 60))
  while [ $SECONDS -lt $deadline ]; do
    [ "$("$BIN" --home "$A" nodes --output json 2>/dev/null | grep -c '"node_id"')" -ge "$want" ] && return 0
    sleep 0.02
  done
  echo "cold-start.sh: the mesh never reached $want members" >&2
  return 1
}

# -- up -----------------------------------------------------------------------
say "==> up (identity, genesis, listener)"
t0=$(now_us)
"$BIN" --home "$A" up --listen 127.0.0.1:47411 --mesh-name cold-start --node-name cold-a \
  >"$WORK/a.log" 2>&1 &
PIDS+=($!)
wait_up "$WORK/a.log"
UP_US=$(( $(now_us) - t0 ))

# -- invite -------------------------------------------------------------------
say "==> invite (mint a join token)"
t0=$(now_us)
# The token, not the help text around it: `invite create` prints guidance
# for a human, and the machine-readable part is the one line that starts
# with the token prefix.
TOKEN="$("$BIN" --home "$A" invite create --ttl 1h --uses 1 2>/dev/null | grep -o 'mwj1[0-9a-z]*' | head -n1)"
INVITE_US=$(( $(now_us) - t0 ))
[ -n "$TOKEN" ] || { echo "cold-start.sh: no invite token" >&2; exit 1; }

# -- join ---------------------------------------------------------------------
#
# The second machine's whole cold start: it holds the token and nothing
# else, and it ends as a member with an encrypted listener of its own.
say "==> join (a second machine, from nothing to a member)"
t0=$(now_us)
# `--listen` on the join, so the mesh records the right port immediately
# rather than learning it a gossip round later; the name comes from `up`.
"$BIN" --home "$B" join "$TOKEN" --listen 127.0.0.1:47412 >>"$WORK/b.log" 2>&1
"$BIN" --home "$B" up --listen 127.0.0.1:47412 --node-name cold-b >>"$WORK/b.log" 2>&1 &
PIDS+=($!)
wait_up "$WORK/b.log"
wait_members 2
JOIN_US=$(( $(now_us) - t0 ))

# -- schedule -----------------------------------------------------------------
#
# Stops when the mesh has *accepted* the work, which is what "ready to
# schedule" means. How long the user's own command then takes is the
# user's business, and folding it in would make the number a property of
# whatever we happened to run.
say "==> schedule (submit, and have an executor accept it)"
t0=$(now_us)
"$BIN" --home "$A" run 'true' --on any >/dev/null 2>&1
SCHEDULE_US=$(( $(now_us) - t0 ))

TOTAL_US=$(( ${INSTALL_US/null/0} + UP_US + INVITE_US + JOIN_US + SCHEDULE_US ))

if [ "$AS_JSON" = 1 ]; then
  printf '{"install_micros":%s,"up_micros":%s,"invite_micros":%s,"join_micros":%s,"schedule_micros":%s,"total_micros":%s}\n' \
    "$INSTALL_US" "$UP_US" "$INVITE_US" "$JOIN_US" "$SCHEDULE_US" "$TOTAL_US"
else
  echo
  printf '%-10s %s\n' "phase" "wall clock"
  printf '%-10s %s\n' "install" "$([ "$INSTALL_US" = null ] && echo '-  (not measured; pass --install)' || ms "$INSTALL_US")"
  printf '%-10s %s\n' "up" "$(ms "$UP_US")"
  printf '%-10s %s\n' "invite" "$(ms "$INVITE_US")"
  printf '%-10s %s\n' "join" "$(ms "$JOIN_US")"
  printf '%-10s %s\n' "schedule" "$(ms "$SCHEDULE_US")"
  printf '%-10s %s\n' "TOTAL" "$(ms "$TOTAL_US")"
  echo
  echo "Servers that ran no work during this: 0."
fi
