#!/usr/bin/env bash
#meshware: name fleet-audit
#meshware: about Health and configuration of one machine. Run it on every node.
#meshware: needs cores=1
#meshware: arg min_disk : int = 15   "Percent of the checked filesystem that must be free"
#meshware: arg min_mem : int = 10    "Percent of memory that must be available"
#meshware: arg check_path = /        "Which filesystem to check"
#
# Real work: ask every machine the same question and keep the answer. Reads
# only /proc, /etc and coreutils — no root, no package manager, nothing that
# touches the network. Every value is published as a row, so the run diffs
# against last week and gates a pipeline today:
#
#   meshnode service run fleet-audit --fanout all --json \
#       --expect 'equals(compliant, "yes")'
set -uo pipefail

while [[ $# -gt 0 ]]; do
  case "$1" in
    --*=*) k="${1%%=*}"; k="${k#--}"; declare "IN_${k//-/_}=${1#*=}"; shift ;;
    --*)   k="${1#--}";  declare "IN_${k//-/_}=${2:-true}"; shift 2 ;;
    *)     shift ;;
  esac
done
MIND="${IN_min_disk:-15}"; MINM="${IN_min_mem:-10}"; CP="${IN_check_path:-/}"
say() { printf '::meshware-output::%s=%s\n' "$1" "$2"; }

say node   "$(hostname -s)"
say kernel "$(uname -r)"
say os     "$( . /etc/os-release 2>/dev/null && echo "${ID:-?}-${VERSION_ID:-}" || echo unknown )"
say arch   "$(uname -m)"
say cores  "$(nproc)"
say uptime_days $(( $(cut -d. -f1 /proc/uptime) / 86400 ))

mt=$(awk '/^MemTotal:/{print $2}' /proc/meminfo); ma=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo)
mem=$(( ma * 100 / mt ))
say mem_free_pct "$mem"; say mem_total_gb $(( mt / 1048576 ))

disk=$(df -P "$CP" | awk 'NR==2 {gsub("%","",$5); print 100-$5}')
say disk_free_pct "$disk"
say disk_free_gb "$(df -PBG "$CP" | awk 'NR==2 {gsub("G","",$4); print $4}')"

l1=$(cut -d' ' -f1 /proc/loadavg)
say load_per_core_x100 "$(awk -v l="$l1" -v c="$(nproc)" 'BEGIN{printf "%d",(l/c)*100}')"
say clock_synced "$(command -v timedatectl >/dev/null 2>&1 && timedatectl show -p NTPSynchronized --value 2>/dev/null || echo unknown)"
say ffmpeg "$(command -v ffmpeg >/dev/null 2>&1 && echo yes || echo no)"
if [ -f /var/run/reboot-required ] || [ -f /run/reboot-required ]; then say reboot_pending yes; else say reboot_pending no; fi

bad=""
[ "$disk" -lt "$MIND" ] && bad="$bad disk=${disk}%<${MIND}%"
[ "$mem"  -lt "$MINM" ] && bad="$bad mem=${mem}%<${MINM}%"
say findings "${bad# }"
say compliant "$([ -z "$bad" ] && echo yes || echo no)"

echo
if [ -z "$bad" ]; then
  echo "$(hostname -s): ok — disk ${disk}% free, memory ${mem}% available"
else
  echo "$(hostname -s): NOT ok —$bad"; exit 1
fi
