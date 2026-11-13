#!/usr/bin/env bash
# Build the **release** Meshware Director: the full pipeline, ending in one
# self-contained binary with the frontend embedded.
#
#   TS bindings (from the Rust view models) → frontend → Tauri binary
#
# The bindings step is not optional. `src/lib/bindings/` is generated from
# the `#[ts(export)]` view models, so building the frontend against stale
# bindings compiles a UI that disagrees with the engine it talks to.
#
#   ./scripts/build-director-release.sh          build, print the path
#   ./scripts/build-director-release.sh --ship   the small binary instead
#
# For both at once, at one version: ./scripts/ship.sh
#
# The default is the `director` profile: ThinLTO, every core busy, ~4
# minutes. `--ship` is the `release` profile: fat LTO, one core, ~18
# minutes, and ~55 MB less binary. Both are release builds — same
# opt-level, same assertions off, same embedded frontend — and the whole
# difference is how hard LLVM is asked to work at the end. Iterate with
# the first, cut an installer with the second.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_build-common.sh"
ROOT="$(build_root)"
cd "$ROOT"

PROFILE="${MESHWARE_DIRECTOR_PROFILE:-director}"
case "${1:-}" in
  --ship)    PROFILE=release ;;
  -h|--help) sed -n '2,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
  "")        ;;
  *)         echo "error: unknown argument '$1' (try --ship, or --help)" >&2; exit 1 ;;
esac

# `--lib` because `export_bindings` is a unit test in the library; without
# it cargo also builds a test harness for the binary, which is a second
# link of the whole graph to run nothing.
stage "regenerating TypeScript bindings from the Rust view models"
cargo test -p meshware-director --lib export_bindings --quiet >/dev/null

stage "building the frontend"
cd apps/director
[ -d node_modules ] || npm install
npm run build >/dev/null
cd "$ROOT"

# -- bundled meshnode binaries -------------------------------------------------
#
# Add machines (Director → Nodes) needs a meshnode for the far machine's
# architecture. It looks in the mesh's own releases first, then inside
# Director itself, then beside it, and only then at the release channel.
# "Inside Director" is this: any bare release binaries found in
# $MESHNODE_BUNDLE_DIR (default: dist/, what packaging/build-release.sh
# writes) are copied under src-tauri/resources/meshnode/ and shipped in the
# installer, so a fresh Director on a laptop with no network to GitHub can
# still put a node on a rack. Optional: a build with none there still
# works, it just has one fewer source.
stage "bundling meshnode binaries"
RES="$ROOT/apps/director/src-tauri/resources/meshnode"
mkdir -p "$RES"
rm -f "$RES"/meshnode-* "$RES/VERSION"
BUNDLE_DIR="${MESHNODE_BUNDLE_DIR:-$ROOT/dist}"
bundled=0
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  IFS=$'\t' read -r ver src <<< "$(newest_for_target "$BUNDLE_DIR" "$target")"
  if [ -n "${src:-}" ] && [ -f "$src" ]; then
    cp "$src" "$RES/meshnode-$target"
    chmod 755 "$RES/meshnode-$target"

    # **Every check runs before VERSION is written.** A check that fires
    # after the write leaves behind exactly the inconsistent pair it exists
    # to prevent — and the next `cargo build` would embed it without
    # re-running this script. `bad_bundle` takes the copy back out again so
    # a failed run leaves nothing rather than something wrong.
    bad_bundle() {
      rm -f "$RES/meshnode-$target"
      echo "error: $1" >&2
      shift
      for line in "$@"; do echo "  $line" >&2; done
      exit 1
    }

    # Ask the binary itself, where this machine can run it. A filename is a
    # claim; `--version` is the fact. Same check `ship-node.sh` makes after
    # its own build, for the same reason: this version is what Director
    # publishes to the fleet.
    if [ "$target" = "x86_64-unknown-linux-musl" ] && [ "$(uname -m)" = "x86_64" ]; then
      built="$("$RES/meshnode-$target" -V 2>/dev/null | awk '{print $2}')"
      if [ -n "$built" ] && [ "$built" != "$ver" ]; then
        bad_bundle "$(basename "$src") reports version '$built', not '$ver'." \
          "The filename and the binary disagree, and the filename is what would" \
          "be recorded and published. Nothing was bundled."
      fi
    fi

    # One VERSION file covers both architectures, so the two must agree.
    # They do when both come from one `packaging/build-release.sh` run; they
    # will not if dist/ has a dev build for one arch and a release for the
    # other, and shipping that quietly is how Director publishes a version
    # one of its own binaries does not have.
    if [ -s "$RES/VERSION" ] && [ "$(cat "$RES/VERSION")" != "$ver" ]; then
      bad_bundle "dist/ holds a different version per architecture: $(cat "$RES/VERSION") and $ver." \
        "Director records one VERSION for both. Build both from one run, or point" \
        "MESHNODE_BUNDLE_DIR at a directory that has."
    fi

    printf '%s\n' "$ver" > "$RES/VERSION"
    echo "    bundling $(basename "$src") as meshnode-$target (version $ver)"
    bundled=$((bundled + 1))
  fi
done
if [ "$bundled" -eq 0 ]; then
  echo "    (no meshnode binaries in $BUNDLE_DIR — Director will use the mesh's releases or the channel)"
fi

stage "building the Director binary ($PROFILE, embedded frontend)"
cargo build --profile "$PROFILE" -p meshware-director --features custom-protocol
stage_done

# No --probe-version: this is a desktop app, and running it opens a window.
BIN="$ROOT/target/$PROFILE/meshware-director"
report_binary "$BIN"

if [ "$PROFILE" != "release" ]; then
  echo "  (ThinLTO build. --ship is ~55 MB smaller and takes ~4x as long.)"
  echo
fi

echo "run it with:"
echo "  $BIN"
