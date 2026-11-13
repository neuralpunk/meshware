#!/usr/bin/env bash
#meshware: name transcode-media
#meshware: about Re-encode one video. One item per file.
#meshware: item path
#meshware: needs cores=2
#meshware: tools ffmpeg
#meshware: reads $path
#meshware: writes $outdir
#meshware: arg path!                                              "The video to encode"
#meshware: arg codec : one of libx265|libx264|libsvtav1 = libx265 "Video encoder"
#meshware: arg crf : int = 28                                     "Quality — lower is bigger"
#meshware: arg preset : one of veryfast|faster|medium|slow = medium "Speed against size"
#meshware: arg outdir                                             "Where finished files go. Leave blank to keep them on the machine that made them."
#
# Real work: a media library re-encoded to something smaller, one file per
# machine-slot, with the result either written straight to shared storage or
# kept where it was made for you to collect.
set -uo pipefail

while [[ $# -gt 0 ]]; do
  case "$1" in
    --*=*) k="${1%%=*}"; k="${k#--}"; declare "IN_${k//-/_}=${1#*=}"; shift ;;
    --*)   k="${1#--}";  declare "IN_${k//-/_}=${2:-true}"; shift 2 ;;
    *)     shift ;;
  esac
done

SRC="${IN_path:-}"
CODEC="${IN_codec:-libx265}"
CRF="${IN_crf:-28}"
PRESET="${IN_preset:-medium}"
OUTDIR="${IN_outdir:-}"

[[ -n "$SRC" ]]  || { echo "no --path given" >&2; exit 2; }
[[ -r "$SRC" ]]  || { echo "cannot read $SRC on $(hostname -s)" >&2; exit 3; }

base="$(basename "${SRC%.*}")"
work="${TMPDIR:-/tmp}/out.mkv"
before=$(stat -c %s "$SRC")
start=$(date +%s)

ffmpeg -nostdin -v error -y -i "$SRC" \
    -c:v "$CODEC" -preset "$PRESET" -crf "$CRF" -threads 2 \
    -x265-params log-level=error -c:a copy "$work" || { echo "ffmpeg failed on $SRC" >&2; exit 5; }

after=$(stat -c %s "$work")
took=$(( $(date +%s) - start ))
pct=$(( after * 100 / (before > 0 ? before : 1) ))

if [[ -n "$OUTDIR" ]]; then
  mkdir -p "$OUTDIR" && cp "$work" "$OUTDIR/$base.mkv" \
    || { echo "cannot write into $OUTDIR" >&2; exit 6; }
  where="$OUTDIR/$base.mkv"
elif [[ -n "${MESHWARE_OUT:-}" ]]; then
  cat "$work" > "$MESHWARE_OUT"
  where="kept on $(hostname -s)"
else
  echo "nowhere to put the result: set outdir, or tick 'keep output in the mesh'" >&2
  exit 6
fi
rm -f "$work"
echo "$(basename "$SRC")  ${before} → ${after} bytes (${pct}%)  ${took}s on $(hostname -s)  → $where"
