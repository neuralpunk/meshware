#!/usr/bin/env bash
#meshware: name archive-check
#meshware: about Verify one archive really is intact. One item per file.
#meshware: item path
#meshware: needs cores=1
#meshware: reads $path
#meshware: arg path!                              "The archive to verify"
#meshware: arg hash : one of none|sha256 = none   "Also digest it, so you can compare runs"
#meshware: arg max_age_days : int = 0             "Fail anything older than this. 0 = no age check."
#
# Real work: the job worth scheduling nightly. A backup nobody has ever
# restored is a rumour. Picks the verifier from the file's actual bytes, so a
# .tar.gz that is really something else is caught rather than trusted.
set -uo pipefail

while [[ $# -gt 0 ]]; do
  case "$1" in
    --*=*) k="${1%%=*}"; k="${k#--}"; declare "IN_${k//-/_}=${1#*=}"; shift ;;
    --*)   k="${1#--}";  declare "IN_${k//-/_}=${2:-true}"; shift 2 ;;
    *)     shift ;;
  esac
done

F="${IN_path:-}"; HASH="${IN_hash:-none}"; MAXAGE="${IN_max_age_days:-0}"
[[ -n "$F" ]] || { echo "no --path given" >&2; exit 2; }
[[ -r "$F" ]] || { echo "MISSING $F on $(hostname -s)" >&2; exit 3; }

bytes=$(stat -c %s "$F"); age=$(( ( $(date +%s) - $(stat -c %Y "$F") ) / 86400 ))
if [[ "$MAXAGE" -gt 0 && "$age" -gt "$MAXAGE" ]]; then
  echo "STALE $F — $age days old, limit $MAXAGE" >&2; exit 4
fi

kind="$(file -b --mime-type "$F" 2>/dev/null || echo unknown)"
case "$kind" in
  application/gzip|application/x-gzip) gzip -t "$F"    2>/dev/null || { echo "CORRUPT $F (gzip)" >&2; exit 5; } ;;
  application/zstd|application/x-zstd) zstd -t -q "$F" 2>/dev/null || { echo "CORRUPT $F (zstd)" >&2; exit 5; } ;;
  application/x-xz)                    xz -t "$F"      2>/dev/null || { echo "CORRUPT $F (xz)"   >&2; exit 5; } ;;
  application/x-bzip2)                 bzip2 -t "$F"   2>/dev/null || { echo "CORRUPT $F (bzip2)" >&2; exit 5; } ;;
  application/zip)                     unzip -tqq "$F" 2>/dev/null || { echo "CORRUPT $F (zip)"  >&2; exit 5; } ;;
  application/x-tar)                   tar -tf "$F" >/dev/null 2>&1 || { echo "CORRUPT $F (tar)" >&2; exit 5; } ;;
  *) echo "NOT AN ARCHIVE $F — mime $kind" >&2; exit 6 ;;
esac

digest=""
[[ "$HASH" == "sha256" ]] && digest=" $(sha256sum "$F" | cut -c1-16)…"
echo "OK $(basename "$F")  ${bytes} bytes  ${age}d old  ${kind}${digest}  on $(hostname -s)"
