#!/usr/bin/env bash
# Mixed-version interop, against binaries that actually shipped (ENG-24).
#
# **The tests that existed proved the wrong thing.** `rolling_upgrade.rs`
# restarts one build five times, and the mixed-version tests set
# `[state] proto_minor_override` so that this build *pretends* to be older.
# Both are worth having and neither can catch the failure they are named
# for: a request variant added since the old release, an op body the old
# state machine materialises differently, a field a two-release-old
# decoder refuses. Only the old binary knows what the old binary did.
#
# So this runs a real mesh with real old binaries in it:
#
#   ./scripts/interop.sh                 # against published N-1 and N-2
#   ./scripts/interop.sh --from-tags     # …built from git tags instead
#   ./scripts/interop.sh --bins a,b      # …or exactly these binaries
#   ./scripts/interop.sh --json          # one object, for CI to keep
#
# ## What it proves
#
# A founder on **this** build and one joiner per old binary, then the
# things a mixed fleet does every day, in both directions:
#
#   join        an old binary joins a mesh this build founded
#   census      both ends agree on the member list — the property, as
#               opposed to "it did not crash"
#   new→old     work submitted here, executed there
#   old→new     work submitted there, executed here
#   fan-out     one submission that has to place work on every node
#
# ## When there is nothing to test against
#
# The release number is not settled (SHIP-9) and there may be no published
# N-1 yet. **This exits 0 and says why**, rather than failing a build over
# a gap in the release history: a gate that cannot tell "the old binaries
# disagree" from "there are no old binaries" is a gate nobody believes the
# first time it goes red. It becomes a real gate the day two releases
# exist, with no change here.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

AS_JSON=0
FROM_TAGS=0
BINS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --json) AS_JSON=1 ;;
    --from-tags) FROM_TAGS=1 ;;
    --bins) shift; BINS="${1:-}" ;;
    --bins=*) BINS="${1#--bins=}" ;;
    -h | --help) sed -n '2,38p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "interop.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

say() { [ "$AS_JSON" = 1 ] || printf '%s\n' "$*" >&2; }

WORK="$(mktemp -d)"
PIDS=()
cleanup() {
  # Every node this script started, whatever happened. A leftover
  # `meshnode` holding a port is how the *next* run fails for no reason.
  for pid in "${PIDS[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# `skip` is not `fail`. See the header.
skip() {
  if [ "$AS_JSON" = 1 ]; then
    printf '{"skipped":true,"reason":"%s"}\n' "${1//\"/\\\"}"
  else
    echo "interop: skipped — $1" >&2
  fi
  exit 0
}

VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"

# -- find the older binaries --------------------------------------------------

OLD_BINS=()
older_tags() {
  git tag --list 'v*' --sort=-v:refname | grep -vx "v${VERSION}" | head -2
}

if [ -n "$BINS" ]; then
  IFS=',' read -r -a OLD_BINS <<< "$BINS"
  for b in "${OLD_BINS[@]}"; do
    [ -x "$b" ] || { echo "interop.sh: --bins named $b, which is not executable" >&2; exit 2; }
  done
elif [ -n "${MESHWARE_INTEROP_BINS:-}" ]; then
  IFS=',' read -r -a OLD_BINS <<< "$MESHWARE_INTEROP_BINS"
elif [ "$FROM_TAGS" = 1 ]; then
  mapfile -t TAGS < <(older_tags)
  [ "${#TAGS[@]}" -gt 0 ] || skip "no release tags before v${VERSION}: nothing has shipped yet"
  for tag in "${TAGS[@]}"; do
    say "==> building $tag"
    tree="$WORK/src-$tag"
    git worktree add --quiet --detach "$tree" "$tag"
    # An old tree that does not build on today's toolchain is a fact about
    # that tree, not a failure of this build. Say so and carry on.
    if (cd "$tree" && cargo build --quiet --release -p meshware-node >"$WORK/$tag.log" 2>&1); then
      cp "$tree/target/release/meshnode" "$WORK/meshnode-$tag"
      OLD_BINS+=("$WORK/meshnode-$tag")
    else
      say "    $tag does not build on this toolchain — not tested against"
      say "    $(tail -3 "$WORK/$tag.log" | tr '\n' ' ')"
    fi
    git worktree remove --force "$tree" 2>/dev/null || true
  done
else
  # The published artifacts. `MESHWARE_RELEASE_BASE` is the variable the
  # daemon's own `upgrade --self` reads, so this follows whatever host
  # SHIP-1 settles on with nothing to change here.
  BASE="${MESHWARE_RELEASE_BASE:-}"
  [ -n "$BASE" ] || skip "no MESHWARE_RELEASE_BASE and no --from-tags: no published release to test against yet (SHIP-1, SHIP-9)"
  mapfile -t TAGS < <(older_tags)
  [ "${#TAGS[@]}" -gt 0 ] || skip "no release tags before v${VERSION}: nothing has shipped yet"
  for tag in "${TAGS[@]}"; do
    url="$BASE/$tag/meshnode-$tag-$(uname -m)-unknown-linux-musl"
    if curl -fsSL --retry 2 -o "$WORK/meshnode-$tag" "$url"; then
      chmod +x "$WORK/meshnode-$tag"
      OLD_BINS+=("$WORK/meshnode-$tag")
    else
      say "==> no published artifact at $url"
    fi
  done
fi

[ "${#OLD_BINS[@]}" -gt 0 ] || skip "found no older binaries to test against"

# -- and this one -------------------------------------------------------------

NEW="${MESHNODE_BIN:-$ROOT/target/release/meshnode}"
if [ ! -x "$NEW" ]; then
  say "==> building this tree (release)"
  cargo build --quiet --release -p meshware-node
  NEW="$ROOT/target/release/meshnode"
fi

say "==> new $VERSION: $NEW"
for b in "${OLD_BINS[@]}"; do
  say "==> old $("$b" --version 2>/dev/null | head -1): $b"
done

# -- the mesh -----------------------------------------------------------------

free_port() {
  python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)'
}

# Under systemd — which is where a CI runner and a developer's terminal
# both are — the daemon writes structured records to the journal rather
# than lines to its stderr, so a redirected log file stays empty and
# grepping it for "node up" waits forever. `JOURNAL_STREAM` is how systemd
# says stderr is already the journal, so clearing it for the children puts
# their logs in the files this script reads when something fails.
export -n JOURNAL_STREAM 2>/dev/null || true
unset JOURNAL_STREAM

# Readiness is asked of the daemon, not read out of a log, so it does not
# depend on how logging happens to be configured.
#
# **Only the founder can be asked directly.** A joined node's user key is
# not a mesh user, so `status` there is a permission error, correctly —
# which is why a joiner's readiness is the founder seeing it in the census
# instead (`wait_members`).
wait_up() {
  local home="$1" log="$2" deadline=$((SECONDS + 90))
  while [ $SECONDS -lt $deadline ]; do
    "$NEW" --home "$home" status >/dev/null 2>&1 && return 0
    sleep 0.05
  done
  echo "interop.sh: a daemon never came up (see $log)" >&2
  tail -20 "$log" >&2
  return 1
}

# **Alive, not merely recorded.** A member record appears the moment the
# join completes, which is before that machine's daemon is listening — so
# waiting on the count places work on a node that is not there yet.
wait_alive() {
  local home="$1" want="$2" deadline=$((SECONDS + 90))
  while [ $SECONDS -lt $deadline ]; do
    [ "$("$NEW" --home "$home" nodes --output json 2>/dev/null | grep -c '"liveness": "Alive"')" -ge "$want" ] && return 0
    sleep 0.05
  done
  "$NEW" --home "$home" nodes --output json >&2 2>/dev/null || true
  return 1
}

FAILURES=0
CHECKS=0
check() { # check <name> <command…>
  local name="$1"; shift
  CHECKS=$((CHECKS + 1))
  if "$@" >>"$WORK/checks.log" 2>&1; then
    say "    ok   $name"
  else
    FAILURES=$((FAILURES + 1))
    say "    FAIL $name"
    tail -5 "$WORK/checks.log" | sed 's/^/         /' >&2
  fi
}

A="$WORK/new"
A_PORT="$(free_port)"
mkdir -p "$A"
say "==> founding a mesh on this build"
"$NEW" --home "$A" up --listen "127.0.0.1:$A_PORT" --mesh-name interop --node-name new \
  >"$WORK/new.log" 2>&1 &
PIDS+=($!)
wait_up "$A" "$WORK/new.log"

i=0
for OLD in "${OLD_BINS[@]}"; do
  i=$((i + 1))
  OLD_VER="$("$OLD" --version 2>/dev/null | head -1)"
  say "==> $OLD_VER"
  B="$WORK/old$i"
  B_PORT="$(free_port)"
  mkdir -p "$B"

  TOKEN="$("$NEW" --home "$A" invite create --ttl 1h --uses 1 2>/dev/null | grep -o 'mwj1[0-9a-z]*' | head -n1)"
  [ -n "$TOKEN" ] || { echo "interop.sh: no invite token" >&2; exit 1; }

  # 1. The old binary joins a mesh this build founded. Everything below
  #    depends on this, so a failure here stops that node rather than
  #    producing four more failures that all say the same thing.
  if ! "$OLD" --home "$B" join "$TOKEN" --listen "127.0.0.1:$B_PORT" >"$WORK/old$i.log" 2>&1; then
    FAILURES=$((FAILURES + 1)); CHECKS=$((CHECKS + 1))
    say "    FAIL join — $OLD_VER could not join"
    tail -10 "$WORK/old$i.log" | sed 's/^/         /' >&2
    continue
  fi
  say "    ok   join"
  CHECKS=$((CHECKS + 1))

  "$OLD" --home "$B" up --listen "127.0.0.1:$B_PORT" --node-name "old$i" >>"$WORK/old$i.log" 2>&1 &
  PIDS+=($!)

  # 2. Both ends agree there are two members, both alive. "It did not
  #    crash" is not the property; "it converged" is — and the founder
  #    seeing the joiner alive is also the joiner's readiness signal,
  #    since it cannot be asked directly.
  #
  #    **The joiner is asked as the founder's user.** Joining enrols a
  #    *machine*, not a person: the user key the joiner generated for
  #    itself is not a member of this mesh, so every read and every
  #    submission from there is correctly denied. `--user-identity` points
  #    its CLI at the operator's key, which is what an operator with two
  #    machines actually has.
  check "census (new sees both, alive)" wait_alive "$A" 2
  check "census (old sees both)" bash -c \
    "'$OLD' --home '$B' --user-identity '$A/identity/user.key' nodes --output json \
       2>/dev/null | grep -c '\"node_id\"' | grep -qE '^[2-9]'"

  # 3. Work, in both directions. The submitting side builds the request
  #    frame and the other side decodes it, so each direction exercises a
  #    different encoder against a different decoder — which is the whole
  #    point of using real binaries rather than one build pretending.
  check "new → old (submit here, run there)" \
    "$NEW" --home "$A" run 'true' --on "old$i"
  check "old → new (submit there, run here)" \
    "$OLD" --home "$B" --user-identity "$A/identity/user.key" run 'true' --on new

  # 4. A fan-out has to place work on every node and then close a group
  #    whose targets ran on two different builds.
  check "fan-out across both" "$NEW" --home "$A" run 'true' --fanout all

  "$NEW" --home "$A" nodes >>"$WORK/checks.log" 2>&1 || true
done

if [ "$AS_JSON" = 1 ]; then
  printf '{"skipped":false,"version":"%s","older":%d,"checks":%d,"failures":%d}\n' \
    "$VERSION" "${#OLD_BINS[@]}" "$CHECKS" "$FAILURES"
fi

if [ "$FAILURES" != 0 ]; then
  say ""
  say "interop: $FAILURES of $CHECKS checks failed against ${#OLD_BINS[@]} older release(s)"
  exit 1
fi
say ""
say "interop: ok — $CHECKS checks, ${#OLD_BINS[@]} older release(s) interoperate with $VERSION"
