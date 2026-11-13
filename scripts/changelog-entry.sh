#!/usr/bin/env bash
# Does CHANGELOG.md have an entry for this version? (DOC-7)
#
#   scripts/changelog-entry.sh 0.8.0          exit 0 if it does; otherwise say what to do, exit 1
#   scripts/changelog-entry.sh v0.8.0         a leading v is fine
#   scripts/changelog-entry.sh --notes 0.8.0  print that entry, for the release page
#   scripts/changelog-entry.sh --ship 0.8.0   ship.sh's rule: `## Unreleased` will do
#
# ship.sh, packaging/build-release.sh and the release workflow all ask this
# before they build anything, so a release cannot go out with nothing
# written about it. Before this, releases leaned on GitHub's generated
# notes: a list of commit titles, which is not what an operator needs to
# know before upgrading a fleet — what might stop working, and what to do.
#
# What counts as an entry:
#
#   a release, 0.8.0          a heading `## 0.8.0`, or `## [0.8.0]`, with
#                             anything after it (a date)
#   a pre-release, 0.8.0-rc.1 that, or the heading of the release it leads
#                             to (`## 0.8.0`), or `## Unreleased` — an rc is
#                             cut while the notes are still being written
#   a dev build, *-dev.*      nothing: it goes to your own machines, and
#                             ship.sh makes several a day
#
# **`--ship`: any version, against `## Unreleased`.** `ship.sh 0.9.28` builds
# for the fleet its owner runs, several times a week, each one numbered; a
# heading per build would be a changelog of build numbers. So ship.sh asks
# only that the notes exist — a heading for the version, or an Unreleased
# section being written. `packaging/build-release.sh` and the release
# workflow, which publish, keep the rule above.
set -euo pipefail

usage() { sed -n '2,7p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }

NOTES=0
SHIP=0
case "${1:-}" in
  --notes) NOTES=1; shift ;;
  --ship) SHIP=1; shift ;;
esac
[ $# -eq 1 ] || usage

VERSION="${1#v}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FILE="${CHANGELOG:-$ROOT/CHANGELOG.md}"

case "$VERSION" in
  "" | -*) usage ;;
esac

# `## 0.8.0`, `## [0.8.0]`, `## 0.8.0 — 2026-10-01`; the dots are literal.
heading_re() { printf '^## \\[?%s\\]?([[:space:]]|$)' "${1//./\\.}"; }
has_heading() { [ -f "$FILE" ] && grep -Eq "$(heading_re "$1")" "$FILE"; }

# The entry's body, heading excluded, up to the next `## ` or the rule
# that separates entries — what the release page shows. Found the same way
# the check below finds it, so the notes are never another version's.
if [ "$NOTES" -eq 1 ]; then
  for want in "$VERSION" "${VERSION%%-*}" Unreleased; do
    has_heading "$want" || continue
    # Through the environment, not `-v`: awk would read the pattern's
    # backslashes as escapes and match the wrong thing.
    RE="$(heading_re "$want")" awk '
      $0 ~ ENVIRON["RE"] { on = 1; next }
      on && (/^## / || /^---$/) { exit }
      on { print }
    ' "$FILE" | sed -e '/./,$!d'
    exit 0
  done
  echo "error: CHANGELOG.md has no entry for ${VERSION}" >&2
  exit 1
fi

case "$VERSION" in
  *-dev*) exit 0 ;;
esac

has_heading "$VERSION" && exit 0
[ "$SHIP" -eq 1 ] && has_heading "Unreleased" && exit 0
case "$VERSION" in
  *-*)
    base="${VERSION%%-*}"
    has_heading "$base" && exit 0
    has_heading "Unreleased" && exit 0
    ;;
esac

cat >&2 <<EOF
error: CHANGELOG.md has no entry for ${VERSION}.

       Every release says what changed, what might stop working, and what
       to do about it. Rename the "## Unreleased" heading to the version,
       with the date:

         ## ${VERSION} — $(date +%Y-%m-%d)

       and start a new, empty "## Unreleased" above it for what comes next.
EOF
exit 1
