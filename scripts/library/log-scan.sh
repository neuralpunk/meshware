#!/usr/bin/env bash
#meshware: name log-scan
#meshware: about Count what matters in one log file, compressed or not.
#meshware: item path
#meshware: needs cores=1
#meshware: reads $path
#meshware: arg path!                            "The log file"
#meshware: arg patterns = ERROR,WARN,CRITICAL   "Comma-separated, counted in one pass"
#meshware: arg ignore_case : bool = true        "Match case-insensitively"
#
# Real work: pattern counts across a whole log estate. Each item answers with
# one small JSON object, so terabytes in becomes kilobytes out — tick "keep
# each item's output in the mesh" and add a merge step to fold them together.
set -uo pipefail

while [[ $# -gt 0 ]]; do
  case "$1" in
    --*=*) k="${1%%=*}"; k="${k#--}"; declare "IN_${k//-/_}=${1#*=}"; shift ;;
    --*)   k="${1#--}";  declare "IN_${k//-/_}=${2:-true}"; shift 2 ;;
    *)     shift ;;
  esac
done

F="${IN_path:-}"; PATS="${IN_patterns:-ERROR,WARN,CRITICAL}"; IC="${IN_ignore_case:-true}"
[[ -n "$F" ]] || { echo "no --path given" >&2; exit 2; }
[[ -r "$F" ]] || { echo "cannot read $F on $(hostname -s)" >&2; exit 3; }

read_it() {
  case "$(file -b --mime-type "$F" 2>/dev/null)" in
    application/gzip|application/x-gzip) gzip -dc  -- "$F" ;;
    application/zstd|application/x-zstd) zstd -dcq -- "$F" ;;
    application/x-xz)                    xz -dc    -- "$F" ;;
    application/x-bzip2)                 bzip2 -dc -- "$F" ;;
    *)                                   cat       -- "$F" ;;
  esac
}

tmp="${TMPDIR:-/tmp}/stream"
read_it > "$tmp" || { echo "could not read $F" >&2; exit 4; }
lines=$(wc -l < "$tmp")

flags=(-c); [[ "$IC" == "true" ]] && flags+=(-i)
counts=""
IFS=',' read -r -a arr <<< "$PATS"
for p in "${arr[@]}"; do
  [[ -z "$p" ]] && continue
  n=$(grep "${flags[@]}" -- "$p" "$tmp" 2>/dev/null || true)
  counts="$counts\"$p\":${n:-0},"
done
rm -f "$tmp"

echo "$(basename "$F")  ${lines} lines on $(hostname -s)"
printf '{"file":"%s","node":"%s","lines":%s,"counts":{%s}}\n' \
  "$F" "$(hostname -s)" "${lines:-0}" "${counts%,}"
