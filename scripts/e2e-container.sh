#!/usr/bin/env bash
# The real-network end-to-end run (MASTERPLAN §16; C0–C9 §14.4).
#
# Everything else in this repo is tested in-process: `mesh-testkit` spawns
# real nodes with real identity, real QUIC and real stores, but they share
# a loopback interface and a process. That is the right default — it is
# fast, deterministic, and catches almost everything — and it cannot catch
# the class of bug that only exists when two nodes are on two *hosts*: a
# hard-coded 127.0.0.1, an address the mesh advertises but nobody can
# reach, a binary that needs a library the container does not have.
#
# So this runs the lifecycle on a real network between real containers:
#
#   join → push → run → drain → kill → adopt
#
#   ./scripts/e2e-container.sh              # 3 nodes, podman or docker
#   ./scripts/e2e-container.sh --keep       # leave the containers up
#
# It is part of C3 because the cold-start claim needs a real host to be
# credible: a number measured on loopback in one process is a number about
# software, and "install this on your machines" is a claim about machines.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

ENGINE="${CONTAINER_ENGINE:-}"
if [ -z "$ENGINE" ]; then
  if command -v podman >/dev/null 2>&1; then ENGINE=podman
  elif command -v docker >/dev/null 2>&1; then ENGINE=docker
  else echo "e2e-container.sh: needs podman or docker" >&2; exit 2; fi
fi

NET=meshware-e2e
NODES=(mw-a mw-b mw-c)
IMAGE=docker.io/library/alpine:3.20

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
in_node() { $ENGINE exec "$1" /usr/local/bin/meshnode "${@:2}"; }

WORK_KEY="$(mktemp)"
cleanup() {
  rm -f "$WORK_KEY"
  [ "$KEEP" = 1 ] && { echo "containers left up: ${NODES[*]}"; return; }
  for n in "${NODES[@]}"; do $ENGINE rm -f "$n" >/dev/null 2>&1 || true; done
  $ENGINE network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

# A static musl binary is the whole deployment story, so the E2E ships one
# — not a builder image, not a package. If this step needs anything but
# `cp`, the deployment story has quietly changed.
say "building the static binary"
"$ROOT/scripts/build-node-release.sh" >/dev/null
BIN="$ROOT/target/x86_64-unknown-linux-musl/release/meshnode"
[ -x "$BIN" ] || { echo "no musl binary at $BIN" >&2; exit 1; }

say "starting $ENGINE network and ${#NODES[@]} containers"
$ENGINE network create "$NET" >/dev/null 2>&1 || true
for n in "${NODES[@]}"; do
  $ENGINE rm -f "$n" >/dev/null 2>&1 || true
  $ENGINE run -d --name "$n" --network "$NET" --hostname "$n" "$IMAGE" sleep infinity >/dev/null
  $ENGINE cp "$BIN" "$n:/usr/local/bin/meshnode"
  # The binary needs nothing. The *services* it runs do: a `bash` service
  # is spawned with `bash`, and Alpine ships busybox `sh`. Installing it
  # here rather than shipping a fatter image keeps the point of the step
  # honest — the deployment story is still one static file.
  $ENGINE exec "$n" apk add --no-cache bash >/dev/null 2>&1
done

# Start a node's daemon, with its output where this script can read it.
#
# Not the container log: the daemon runs under `exec -d`, so PID 1 is still
# `sleep infinity` and `podman logs` shows nothing at all. A file inside
# the container is the only place its output exists.
start_node() {
  local name="$1"
  shift
  $ENGINE exec -d "$name" sh -c \
    "/usr/local/bin/meshnode up $* >/tmp/node.log 2>&1"
}

# The readiness signal is the daemon's own "node up" line, not
# `meshnode status`: a *joined* node's user key is not a mesh user, so
# asking it for status is a permission error — correctly — and readiness
# would look like failure forever. What matters is that the daemon reached
# the point where it can take work, and that is exactly what it logs.
wait_ready() {
  for _ in $(seq 1 300); do
    $ENGINE exec "$1" grep -q "meshware node up" /tmp/node.log 2>/dev/null && return 0
    sleep 0.2
  done
  echo "e2e: $1 never became ready" >&2
  $ENGINE exec "$1" tail -40 /tmp/node.log >&2 2>/dev/null || true
  return 1
}

say "founding the mesh on mw-a"
start_node mw-a --listen 0.0.0.0:47411 --advertise mw-a:47411 \
  --mesh-name e2e --node-name mw-a
wait_ready mw-a

say "join — mw-b and mw-c, over the container network"
TOKEN="$(in_node mw-a invite create --ttl 1h --uses 5 --addr mw-a:47411 2>/dev/null | grep -o 'mwj1[0-9a-z]*' | head -n1)"
for n in mw-b mw-c; do
  in_node "$n" join "$TOKEN" --listen 0.0.0.0:47411
  start_node "$n" --listen 0.0.0.0:47411 --advertise "$n:47411" --node-name "$n"
  wait_ready "$n"
done
for _ in $(seq 1 150); do
  [ "$(in_node mw-a nodes --output json | grep -c '"node_id"')" -ge 3 ] && break
  sleep 0.2
done
in_node mw-a nodes

# The founder's *user* key, on every container.
#
# Not a shortcut around authorization — it is the model. A person is a key
# on their own machine and every request is signed with it (MASTERPLAN
# §4.1, §7.1); a joined node generates a node identity, and its own
# accidental user key is nobody. So an operator who wants to drain mw-c
# carries their key there, exactly as they would carry it to a laptop.
# Skipping this is how the previous version of this script "discovered"
# that `meshnode drain` refuses an unknown user, which is correct
# behaviour and not a bug to route around.
$ENGINE cp mw-a:/root/.meshware/identity/user.key "$WORK_KEY"
for n in mw-b mw-c; do
  $ENGINE cp "$WORK_KEY" "$n:/root/.meshware/identity/user.key"
done

say "push — content ingested on one node and fetched by another"
$ENGINE exec mw-a sh -c 'mkdir -p /tmp/svc && printf "#!/bin/sh\necho hello from $(hostname)\n" > /tmp/svc/run.sh && chmod +x /tmp/svc/run.sh'
BUNDLE="$(in_node mw-a push /tmp/svc | grep -o 'bndl-[0-9a-f]*' | head -n1)"
[ -n "$BUNDLE" ] || { echo "e2e: push produced no bundle id" >&2; exit 1; }
in_node mw-a service create greet --runtime bash --bundle "$BUNDLE" --entrypoint run.sh
# The point of this step: mw-c never saw the directory, and the definition
# and its content reach it because it needs them, not because anyone
# copied them there.
in_node mw-a service run greet --on 'name=mw-c' --timeout 60

say "run — fan out to every node, and check every one answered"
in_node mw-a run --fanout all 'hostname' --timeout 60

say "drain — mw-c finishes what it accepted and takes nothing new"
in_node mw-c drain
sleep 2
in_node mw-a top

# The fan-out is launched **from mw-b**, so mw-b is the *origin* and not
# merely a target. Killing a target proves nothing about adoption — the
# origin simply records a failure. Killing the origin is the §10.3 path:
# the plan replicated before the first dispatch, so a survivor elected by
# rendezvous hashing picks it up and drives the rest.
say "kill — the fan-out's ORIGIN dies mid-run"
$ENGINE exec -d mw-b sh -c \
  "/usr/local/bin/meshnode run --fanout all 'sleep 15; echo survived' \
   --timeout 180 >/tmp/fan.log 2>&1"
# Wait for the plan to replicate before pulling the plug, or there would be
# nothing for anyone to adopt and the test would pass for the wrong reason.
for _ in $(seq 1 150); do
  in_node mw-a jobs --output json | grep -q '"group"' && break
  sleep 0.2
done
in_node mw-a jobs --output json | grep -q '"group"' ||
  { echo "e2e: the fan-out plan never replicated" >&2; exit 1; }
$ENGINE kill mw-b >/dev/null

say "adopt — a survivor finishes the fan-out its dead origin started"
#
# The property under test is that nothing is left *open*. The dead origin
# can never close its own group, so if the group closes it is because a
# survivor claimed ownership and closed it.
# Polled on *this* fan-out's own command, not on "any succeeded job":
# three of them already succeeded above, so a bare grep would have passed
# before the origin was even killed. A test that passes for the wrong
# reason is worse than no test.
adopted=0
for _ in $(seq 1 300); do
  survivors="$(in_node mw-a jobs --output json |
    grep -c '"cmd_preview": "sleep 15; echo survived".*' || true)"
  pending="$(in_node mw-a jobs --output json |
    tr -d ' ' | grep -A2 '"cmd_preview":"sleep15;echosurvived"' |
    grep -c '"state":"Queued"\|"state":"Running"' || true)"
  if [ "${survivors:-0}" -gt 0 ] && [ "${pending:-0}" = 0 ]; then
    adopted=1
    break
  fi
  sleep 1
done
in_node mw-a jobs
[ "$adopted" = 1 ] ||
  { echo "e2e: the orphaned fan-out never reached a terminal state" >&2; exit 1; }
in_node mw-a doctor || true

say "PASS — join, push, run, drain, kill and adopt all completed on a real network"
