#!/usr/bin/env bash
#meshware: name burn-in
#meshware: about Every machine at full load in the same second.
#meshware: needs cores=1
#meshware: arg seconds : int = 60   "How long to hold full load"
#meshware: arg workers : int = 0    "Busy processes per machine. 0 = one per thread."
#meshware: arg align : int = 5      "Members wait for the next multiple of this on the wall clock"
#
# Real work, and the one honest non-GPU reason to want a gang. Staggered
# stress proves each machine is fine alone; simultaneous stress tests the
# things they share — the circuit, the UPS, the room's air, the switch. A rack
# that passes one at a time and browns out all at once is a real failure and
# invisible to any scheduler that runs these back to back.
#
# Reports throughput, average clock and peak CPU temperature, so the machine
# that quietly throttles shows up as a low number beside its siblings.
set -uo pipefail

while [[ $# -gt 0 ]]; do
  case "$1" in
    --*=*) k="${1%%=*}"; k="${k#--}"; declare "IN_${k//-/_}=${1#*=}"; shift ;;
    --*)   k="${1#--}";  declare "IN_${k//-/_}=${2:-true}"; shift 2 ;;
    *)     shift ;;
  esac
done
S="${IN_seconds:-60}"; W="${IN_workers:-0}"; A="${IN_align:-5}"
[ "$W" -gt 0 ] || W=$(nproc)
NODE="$(hostname -s)"; RANK="${MESHWARE_GANG_RANK:-0}"; SIZE="${MESHWARE_GANG_SIZE:-1}"

now=$(date +%s); at=$(( (now / A + 1) * A ))
echo "$NODE rank $RANK/$SIZE: $W worker(s) for ${S}s, starting together at $(date -d @$at +%H:%M:%S)"
while [ "$(date +%s)" -lt "$at" ]; do sleep 0.1; done

d="${TMPDIR:-/tmp}/burn"; mkdir -p "$d"; rm -f "$d"/w.*
t0=$(date +%s.%N)
for i in $(seq 1 "$W"); do
  ( python3 -c "
import hashlib,time,sys
end=time.time()+float(sys.argv[1]); h=b'meshware'; n=0
while time.time()<end:
    for _ in range(20000): h=hashlib.sha256(h).digest()
    n+=20000
print(n)" "$S" > "$d/w.$i" ) &
done
wait
t1=$(date +%s.%N)

el=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.1f", b-a}')
tot=$(cat "$d"/w.* 2>/dev/null | awk '{s+=$1} END{print s+0}')
rate=$(awk -v t="$tot" -v e="$el" 'BEGIN{printf "%.0f", (e>0)? t/e : 0}')
rm -f "$d"/w.*
mhz=$(awk '/^cpu MHz/{s+=$4;n++} END{printf "%d",(n>0)?s/n:0}' /proc/cpuinfo 2>/dev/null || echo 0)
tmp=0
for h in /sys/class/hwmon/hwmon*; do
  case "$(cat "$h/name" 2>/dev/null)" in
    k10temp|coretemp|zenpower|cpu_thermal) for f in "$h"/temp*_input; do
        t=$(cat "$f" 2>/dev/null || echo 0); [ "$t" -gt "$tmp" ] && tmp=$t; done ;;
  esac
done
[ "$tmp" -eq 0 ] && for z in /sys/class/thermal/thermal_zone*/temp; do
  t=$(cat "$z" 2>/dev/null || echo 0); [ "$t" -gt "$tmp" ] && tmp=$t; done

printf '::meshware-output::node=%s\n' "$NODE"
printf '::meshware-output::rank=%s\n' "$RANK"
printf '::meshware-output::rate_per_sec=%s\n' "$rate"
printf '::meshware-output::rate_per_worker=%s\n' "$(( rate / W ))"
printf '::meshware-output::avg_mhz=%s\n' "$mhz"
printf '::meshware-output::peak_temp_c=%s\n' "$(( tmp / 1000 ))"
echo "$NODE rank $RANK: ${rate}/s total, $(( rate / W ))/s per worker, ${mhz} MHz, $(( tmp / 1000 ))C"
