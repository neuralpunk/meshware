# Upgrading, and rolling back

*Reference. Describes what is in the tree.*

**Replace the binary.** There is no ordered control-plane upgrade procedure because there is no
control plane — but there is still a right way to roll a fleet, and it is health-gated.

---

## 1. Mixed versions are fine

Nodes negotiate a protocol minor per feature, so a 1.12 node and a 1.18 node still gossip and
still schedule for each other. An op of a kind an older node does not understand is **stored,
verified and forwarded** rather than wedging its sync — and that node reports exactly what it
cannot see.

This is why a rolling upgrade does not need a maintenance window.

### The supported skew window

**Two releases.** A node running release N interoperates with nodes running N-1 and N-2, and
that is the range CI tests. Older than that is not refused — the negotiation accepts any 1.x,
and a 1.4 node and a 1.29 node do still gossip — it is simply not a combination anybody has
run.

The practical meaning of the window is **what a rolling upgrade may leave half-done.** A fleet
that has been part-way through an upgrade for a week is inside it; one that has a node nobody
has touched in a year is not.

What you see when a node is too old for something the mesh is doing:

- The submission moves to another node. A request the older node's protocol cannot carry is
  refused **before it is sent**, with the minor it would need, and the scheduler places the work
  somewhere that can take it. It shows on `/metrics` as
  `meshware_dispatch_failures_total{reason="too-old"}`, which should fall to zero as a rollout
  finishes.
- Features arrive per minor, not per release. `meshnode nodes` marks a node whose protocol
  cannot follow the mesh's op log, and `doctor` says how far behind it is.

Which features need every node upgraded first is in [compatibility.md](compatibility.md#features-that-need-every-node),
and what each release changed is in the [changelog](../../CHANGELOG.md).

**The CLI and the daemon are a separate question.** They talk over a local socket with no
version negotiation, so a `meshnode` from one release against a daemon from another can send a
request the daemon has no variant for. It says so, with both versions, rather than reporting a
malformed payload. Keep them matched on a machine; on a packaged install they are one file.

## 2. One node

```console
$ meshnode upgrade --self
```

Fetches the signed release, **verifies the minisign signature against a key it already holds**,
swaps the binary atomically, drains, and exits clean — systemd starts the new one.

The binary it replaced is kept beside it as `meshnode.prev`, and the new one is **on probation**
until it has run healthy for a minute: every critical loop alive, the watchdog fed. A version
that fails to get there in three starts — it crashes, the watchdog kills it, or it refuses the
configuration the old one was running with — is swapped back by the node itself, which then
starts the previous binary in its place. The one that failed is kept as `meshnode.failed`, the
journal says what happened, and `upgrade --self` reports it and exits 1 instead of claiming
success.

Package installs upgrade through apt or dnf instead; `upgrade --self` refuses to replace a
package-manager-owned binary and says which tool to use.

## 3. A fleet

Put the binary in the mesh once, then roll rings:

```console
$ meshnode upgrade --publish ./meshnode --version 0.7.0     # + its .minisig
$ meshnode upgrade --status --to 0.7.0                      # who is where
$ meshnode bundles prefetch bndl-22e3… --on tag=dc-east      # warm the ring first
$ meshnode upgrade --on tag=canary  --to 0.7.0              # 3 boxes, watch it
$ meshnode upgrade --on tag=dc-east --to 0.7.0 --batch 32   # then 400, 32 at a time
```

**How this stays safe:**

- The binary travels as an ordinary content-addressed bundle, pulled by digest from whichever
  peer holds it, every chunk verified on write. **The peer is a CDN, not an authority** — the
  receiving node checks the release signature against its *own* local key copy before the binary
  goes near its own.
- Publishing needs an admin signature **and** a valid release-key signature, so neither key
  alone can put a binary on your fleet.
- **A mesh can also require a security key's co-signature** on every binary, which a key
  stolen from a CI secret store cannot make: trust an SSH-form release key, then publish each
  co-signature with `meshnode upgrade --cosign <file> --version <v>` — or, in Director, *Mesh ›
  Updates › Advanced › Release keys*, where a binary without one has **Attach co-signature…**.
  Once such a key is trusted, a release without its co-signature installs nowhere.
  [limits.md §6c](limits.md#6c-release-signing--the-key-that-can-reach-every-fleet) has the
  details.
- A rollout is an admin-signed **plan**: a ring, a version, a wave width, an expiry. A plan
  *authorizes* and never *triggers* — a node that merges one and is never asked does nothing.
  Each node checks the plan against its own state before acting.
- **Waves are health-gated.** The next wave does not start until the previous one is wholly
  back, alive and caught up. A bad binary costs one wave, never the fleet.
- **`--on any` is refused.** A mesh that can be upgraded in one command is a mesh that can be
  broken in one command.

Fleet self-upgrade needs the one-line opt-in drop-in — see [install.md](install.md#4-fleet-self-upgrade-needs-one-opt-in).

## 4. Rolling back

One node, by hand, at any time after an `upgrade --self`:

```console
$ meshnode upgrade --rollback            # meshnode.prev goes back, and the daemon restarts on it
```

It checks that the previous binary runs before it swaps anything, keeps the one it replaces as
`meshnode.failed`, and can be used once per upgrade. A node does the same by itself when a new
version does not become healthy (§2), and a fleet rollout that reaches such a node stops there
with the node's own explanation rather than at its deadline.

A binary rolled back past a release that changed the on-disk format refuses the data directory
(exit status 65) rather than guessing; that needs the newer binary, or a restore from backup.

A fleet, or a node with nothing kept: roll the previous version the same way as any other:

```console
$ meshnode upgrade --on tag=dc-east --to 0.6.12 --batch 32
```

There is no separate rollback path because there does not need to be one: a version is a
version, and the wave gate protects a downgrade exactly as it protects an upgrade.

Services have their own rollback, which is a different thing and often the one you want:

```console
$ meshnode service rollback deploy-api 3      # back to version 3
```

## 5. Taking a machine out politely

```console
$ meshnode drain --move                       # finish accepted work, move queued work, exit
$ meshnode drain --until 06:00 --reason "kernel update"   # a maintenance window
$ meshnode drain --for 2h
$ meshnode drain --lift                       # done early
```

A drain with a deadline is a maintenance window: the node refuses new work, keeps what is
running, stays up, and rejoins on its own. Work already running is never killed by a drain
without `--move`.

## 5a. What a plain `systemctl restart` does to work in flight

A package upgrade ends in `systemctl restart meshnode`, so a restart is an upgrade path and is
held to the same promise as a drain — with a bound on it, because a restart is meant to be
quick.

**Running jobs are waited for, not killed.** On SIGTERM the node stops accepting work (a peer
dispatching to it gets `Busy` and places the job elsewhere, exactly as for a full node) and
waits for the jobs it already owns, up to `[shutdown] grace_secs` — 30 seconds by default:

```toml
[shutdown]
# Seconds a stop waits for running jobs. 0 = don't wait.
# Keep it below the unit's TimeoutStopSec (60), or systemd's SIGKILL
# arrives first and the grace was only a delay.
grace_secs = 30
```

A job still running when the grace ends is killed with the rest of the cgroup and marked
`failed` on the next start, with a line in the journal saying that is what happened. Raise
`grace_secs` and `TimeoutStopSec` together on a node whose jobs are minutes long, or take it out
with `meshnode drain` first (§5), which has no deadline at all.

**Queued jobs survive the restart.** Work that was accepted but had not started is written to
the spool before it is queued, and is put back on the queue — oldest first — when the node comes
back. It is not re-admitted against the queue cap: it was admitted once already, and failing a
job for having outlived a restart is what this replaced.

None of this works with the stock `KillMode`. systemd's default signals *every* process in the
service's cgroup, so every job would be killed directly before the daemon heard anything; the
shipped unit sets `KillMode=mixed` so SIGTERM reaches the daemon alone. An operator who has
overridden that has also overridden this.

## 6. What "zero killed jobs" means

The rolling-upgrade demo — upgrading a 5-node mesh under load with zero killed jobs — is a test
(`rolling_upgrade.rs`), not a slogan.

A submission whose chosen node is draining or cannot be reached goes to the next candidate:
a draining node answers `Busy`, and a node that is already down cannot have been handed
anything. A submission that went out and got no answer is asked again of the **same** node for
up to 20 seconds — safe, because a node acknowledges a job it already holds — and moves on only
if that node says it does not have it. If the node never answers, the job fails with
`undetermined:` and is not sent anywhere else, because it may be running there.

A fan-out cannot move a target to another node, so it waits: a target that cannot be reached is
tried again for 30 seconds, which is longer than a node takes to restart into a new release.
`meshware_dispatch_failures_total{reason}` and `meshware_dispatch_spills_total` on `/metrics`
count what happened.

## 7. Checking a fleet's version

```console
$ meshnode upgrade --status
VERSION       COUNT  NODES
0.7.0             3  hq, forge, anvil            ← target
0.6.12          412  edge-001, edge-002, …       ← outdated
```

Director's **Mesh › Updates** is the same thing with a button.
