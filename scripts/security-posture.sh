#!/usr/bin/env bash
# The security-posture gate (E1-P3).
#
# **An unmeasured claim is a slogan** — the same rule cold-start.sh applies
# to the operational-cost table, applied to the sentence "the shipped unit
# is hardened". `systemd-analyze security` turns that sentence into a
# number, so SECURITY.md can quote one and CI can fail when it regresses.
#
#   ./scripts/security-posture.sh              # both units, human output
#   ./scripts/security-posture.sh --json       # one object, for a plot
#   ./scripts/security-posture.sh --check      # exit 1 if a score regressed
#
# Two units are measured, because meshware ships two postures on purpose:
#
#   base    packaging/systemd/meshnode.service
#           Everything that cannot affect a job. This is what installs.
#   strict  + packaging/systemd/meshnode-hardened.conf
#           Adds a syscall filter and namespace restrictions, which are
#           inherited by every job the executor runs — so they are an
#           operator's decision about their own workloads, not ours.
#
# The ceilings below are the measured values at the time they were set.
# They are maxima: a change that improves a score should lower the ceiling
# in the same commit, so the number never quietly drifts back up.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT="$ROOT/packaging/systemd/meshnode.service"
DROPIN="$ROOT/packaging/systemd/meshnode-hardened.conf"

# Ceilings — lower is better. Set 2026-09-06 against systemd 259.
readonly BASE_MAX=4.5
readonly STRICT_MAX=3.0

MODE=human
case "${1:-}" in
  --json)  MODE=json ;;
  --check) MODE=check ;;
  "")      ;;
  *) echo "usage: $(basename "$0") [--json|--check]" >&2; exit 2 ;;
esac

if ! command -v systemd-analyze >/dev/null 2>&1; then
  # Not a failure: macOS and Windows contributors have no systemd, and this
  # gate is about the Linux package. CI runs on Linux and will catch it.
  echo "systemd-analyze not found — skipping (this check needs systemd 247+)" >&2
  exit 0
fi

# `--offline=true` scores a unit *file* without loading it into systemd.
# Added in systemd 247; without it this would need root and a live unit.
if ! systemd-analyze security --offline=true "$UNIT" >/dev/null 2>&1; then
  echo "systemd-analyze security --offline is unavailable (needs systemd 247+)" >&2
  exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Splice the drop-in's directives into a copy of the unit: systemd-analyze
# scores one file, and a drop-in is only merged for a unit systemd has
# loaded. Comments and the section header are dropped; everything else is
# inserted ahead of [Install], exactly where systemd would apply it.
python3 - "$UNIT" "$DROPIN" "$WORK/meshnode-strict.service" <<'PY'
import sys
unit, dropin, out = sys.argv[1], sys.argv[2], sys.argv[3]
base = open(unit).read()
extra = "\n".join(
    l for l in open(dropin).read().splitlines()
    if l.strip() and not l.lstrip().startswith("#") and l.strip() != "[Service]"
)
open(out, "w").write(base.replace("[Install]", extra + "\n\n[Install]"))
PY

score_of() {
  # The summary line is: "→ Overall exposure level for <unit>: 4.1 OK 🙂"
  systemd-analyze security --offline=true "$1" 2>/dev/null \
    | sed -n 's/.*[Ee]xposure level for [^:]*: \([0-9.]*\).*/\1/p' | tail -1
}

BASE_SCORE="$(score_of "$UNIT")"
STRICT_SCORE="$(score_of "$WORK/meshnode-strict.service")"

if [[ -z "$BASE_SCORE" || -z "$STRICT_SCORE" ]]; then
  echo "could not parse a score out of systemd-analyze — has its output changed?" >&2
  exit 1
fi

over() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a > b) }'; }

case "$MODE" in
  json)
    printf '{"systemd":"%s","base":{"score":%s,"max":%s},"strict":{"score":%s,"max":%s}}\n' \
      "$(systemd-analyze --version | head -1 | awk '{print $2}')" \
      "$BASE_SCORE" "$BASE_MAX" "$STRICT_SCORE" "$STRICT_MAX"
    ;;
  *)
    printf '%-8s %-46s %6s   (ceiling %s)\n' "UNIT" "FILE" "SCORE" ""
    printf '%-8s %-46s %6s   (ceiling %s)\n' "base"   "packaging/systemd/meshnode.service"        "$BASE_SCORE"   "$BASE_MAX"
    printf '%-8s %-46s %6s   (ceiling %s)\n' "strict" "  + meshnode-hardened.conf (opt-in)"       "$STRICT_SCORE" "$STRICT_MAX"
    echo
    echo "lower is better; systemd-analyze security scores 0 (locked down) to 10 (unrestricted)"
    ;;
esac

if [[ "$MODE" == check || "$MODE" == human ]]; then
  rc=0
  if over "$BASE_SCORE" "$BASE_MAX"; then
    echo "REGRESSION: base unit scores $BASE_SCORE, ceiling is $BASE_MAX" >&2; rc=1
  fi
  if over "$STRICT_SCORE" "$STRICT_MAX"; then
    echo "REGRESSION: strict unit scores $STRICT_SCORE, ceiling is $STRICT_MAX" >&2; rc=1
  fi
  [[ $rc -eq 0 ]] || {
    echo "run 'systemd-analyze security --offline=true $UNIT' to see which line changed" >&2
    exit 1
  }
fi
