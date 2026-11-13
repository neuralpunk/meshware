# How meshware works without a control plane

*Reference. Describes what is in the tree, not a plan.*

**Every claim on this page is backed by a test that runs on every commit**
(`crates/mesh-core/tests/partition.rs`, gated in CI as *"survives a network partition"*), and
every number was measured on that harness rather than estimated. Where the answer is
uncomfortable, it is here too — §7 is the list of things this architecture deliberately does
**not** promise, and it is the most important section on the page.

---

## 1. The claim, stated precisely

meshware's headline is *"a cluster scheduler with no control plane."* That sentence is doing
real work and it is worth unpacking, because two readers hear two different things.

**What it does not mean.** It does not mean there is no state. A mesh knows its members, its
users, their grants, its policies, its services, its jobs and its audit history. All of that is
durable, replicated and authenticated.

**What it means.** There is no *node whose availability the mesh depends on.* No leader, no
quorum, no external database, no scheduler process, no API server. Every node is a full peer:
it accepts work, schedules work, executes work, stores content, serves the API and holds the
whole of the mesh's state. **One node is a product; a thousand nodes is the same product.**

The practical test is the one any demo should show: **kill any node — including the one an
operator is looking at — and the mesh does not notice.** Director is not even a peer: it is a view
onto one node, and closing it changes nothing at all.

---

## 2. What is authoritative

There are exactly three kinds of state in meshware, and they have different rules.

| | What it is | Authority | Survives a total restart |
|---|---|---|---|
| **Replicated state** | Members, users, grants, services, deployments, schedules, secrets (sealed), job records, audit entries, retention windows | The signed op-log, merged identically on every node | Yes — it is on every node's disk |
| **Node-local ledger** | Which cores, GPUs and slots are currently spoken for on *this* box | The node itself, and nothing else | No, and it must not — a restarted node has no reservations |
| **Live observation** | Who is reachable right now, current load and pressure | Whoever looked most recently; carried epidemically, always slightly stale | No |

The second row is the load-bearing one. **Nothing in the design has a distributed lock**,
because the only thing that must never be double-spent — a physical resource on a physical
machine — is arbitrated by the machine that owns it. A node cannot be talked into running two
jobs on the same GPU by any sequence of messages, because the decision is local and the ledger
is local.

### 2.1 The op-log

Replicated state is a **grow-only log of signed operations**. Each op carries its author's node
signature, a sequence number, and a hybrid logical clock reading. Nodes exchange ops by
anti-entropy: pick a peer, compare version vectors, send what the other lacks. This is
push-pull epidemic replication and it converges in O(log n) rounds.

Merges are **CRDT** merges, so they are commutative, associative and idempotent — the order ops
arrive in cannot change the result. Three shapes cover everything:

- **Last-writer-wins** for config-like values, ordered by HLC with the node id as tiebreak.
- **OR-set** for membership: an add and a remove are distinct facts, and removal requires an
  explicit signed tombstone rather than an absence.
- **Grow-only** for the audit log, which is never modified or reordered.

Because merges are order-independent, **a heal is not a special case anywhere in the code.**
There is no reconciliation pass and no partition-recovery routine to get wrong; the anti-entropy
that runs on a Tuesday afternoon is the same code that resolves a two-hour split.

### 2.2 Governance is signed by people, not by nodes

Every governance artifact — a user record, a role grant, a session policy, a retention
window, a deployment, a rollout plan — carries the **Ed25519 signature of the user who authored it**, and
every node verifies it independently at merge time against its own copy of state.

This is what makes the architecture safe without a central authority. A node does not trust an
op because a trusted peer sent it; it trusts it because it can check the signature itself. A
compromised node can drop ops, delay them or fail to report — it cannot forge one.

---

## 3. Membership

A node joins by presenting an invite token or by pairing, and either way a human confirms the
identity out of band. What the mesh records afterwards is an OR-set entry signed into the log.

**Liveness is separate from membership, and the distinction is the whole answer to "what
happens when a node disappears."**

- **Membership** is replicated, signed state. A node is a member until an **admin-signed
  revocation** says otherwise.
- **Liveness** is a local observation — `Alive`, `Suspect`, `Dead` — carried epidemically and
  never written to the log.

So a node that is merely unreachable is **still a member**. Its work is adopted, its slots stop
being offered, and its record stays exactly where it was. *A dropped packet is not a signature,
and only a signature removes a member.* That is asserted directly:
`a_partitioned_peer_goes_dead_then_returns`.

---

## 4. Scheduling without a leader

The question everyone asks is *"if there is no scheduler, who decides where a job runs?"*

**The submitting node does** — it scores every candidate it can see over ten weighted terms
(load, warmth, recent failures, round-trip time, memory, disk, CPU, and the rest) and dispatches to
the winner. The candidate then either accepts or answers `Busy`, and `Busy` — or a candidate
that cannot be reached at all — spills to the next one. `meshnode jobs explain` prints the top five with their breakdown, and `meshnode plan`
runs the same scorer as a what-if before you submit.

That is a *placement* decision, and placement being wrong is cheap — the job lands somewhere
else. The decisions that must not be wrong are handled differently:

- **Resource exclusivity** — the executing node's own ledger, as in §2. Local, so it cannot race.
- **Gangs** (start N nodes together or none) — a two-phase offer with **TTL leases**. A gang
  that cannot get all N gives back what it took. When two gangs want the same machines, the
  senior one (priority, then submission time) takes the junior one's reservations: wound-wait,
  which is why two six-node gangs on an eight-node mesh both finish instead of deadlocking.
- **Singletons** — a claim on a *name*, replicated, held exactly while the job it names is
  non-terminal.
- **Schedules** — the job an occurrence fires has an id *derived* from `(schedule, occurrence)`,
  so two nodes that both fire the same occurrence converge on one job rather than two.

**Every one of these expires on its own clock.** There is no distributed cleanup path in this
design because there is nothing to clean up: if the node holding a lease dies, the lease lapses
and the resources come back. That property is why "the node that launched the gang died
mid-negotiation" is a non-event.

---

## 5. What actually happens in a partition

This is the section the tests exist for. Split a mesh in half and:

**Both halves keep working.** Each half schedules and executes against the nodes it can still
see. There is no minority that refuses to serve, because there is no quorum to lose.
(`both_halves_keep_executing_while_partitioned`)

**Both halves keep authoring state.** Jobs run, audit entries accrue, policies can be published.

**On heal, the union survives.** Ops from both sides merge with nothing lost and no duplicate
execution, and membership is intact — nobody was evicted for being unreachable.
(`state_from_both_halves_survives_the_heal`)

**A revocation authored on one side becomes authoritative on the other.** It is a signed op in
the same log as everything else and takes effect on merge.
(`a_revocation_signed_during_a_partition_applies_on_heal`)

**Asymmetric cuts converge too.** One-way connectivity — a node that hears everyone and is
heard by nobody — is the shape that breaks naive failure detectors, and the mesh does not tear
itself apart over the disagreement. (`an_asymmetric_cut_converges_on_heal`)

### 5.1 How long it takes — measured, not estimated

On a four-node mesh, two a side, with datagrams silently dropped across the cut:

| Transition | Measured |
|---|---|
| Peer no longer **Alive** (Suspect or Dead) | **5–10 s** |
| Peer marked **Dead** | **10–13 s** |
| Peer back to **Alive** after heal | **51 ms** |

Measured over eight runs; the test asserts Dead within 15 s
(`a_silent_peer_is_noticed_within_seconds`).

**A dropped datagram is not a refused connection**, so an unreachable peer has to be given a
deadline rather than waited on. Every node visits each peer once per cycle; the first answer
of a sync has to start within 2 s; a peer that has failed is probed again every second,
alongside the ordinary gossip rather than behind it, with a 1.5 s leash, and a failed dial makes
the next few fail at once. Three failures make a peer Suspect — and a Suspect peer is no longer
chosen for work — and a peer still failing 5 s after its first failure is Dead.

Recovery, by contrast, is one successful sync — 51 milliseconds.

**meshware is slow to condemn and instant to forgive.** A silent drop is indistinguishable from
a slow network, and the costs of being wrong are asymmetric too: wrongly declaring a node dead
moves its work for no reason, while wrongly waiting costs a few seconds of stale placement. If
your environment wants faster detection, that is a tuning conversation and not an architectural
one.

---

## 6. Failure, restart and recovery

| Event | What happens |
|---|---|
| A node crashes mid-job | Its leases lapse. The job's origin (or an adopting node) sees it fail and can retry; the record is in the log |
| The **submitting** node dies mid-fan-out | Another node adopts ownership. Adoption has been tested since the first releases |
| Director is closed or killed | Nothing. It is a peer with a window attached |
| Every node restarts | State is on each node's disk. The mesh reassembles from what its members already hold |
| A node rejoins after long isolation | Anti-entropy hands it what it missed. Revocations issued while it was away apply on merge |
| The whole mesh is lost | `meshnode backup` writes identity, genesis and the full op-log to one restorable file |

---

## 7. What this architecture does **not** promise

An architecture page that only lists strengths is marketing. These are the real edges.

**Two halves can each start work the other cannot see.** This follows directly from "both halves
keep working" and is not a bug — it is the trade. If a job must be globally unique, a
`--singleton` claim guarantees that *within a connected mesh* and cannot guarantee it across a
partition, because guaranteeing it would require exactly the quorum this design does not have.
Work that genuinely cannot tolerate this needs an external lock, and meshware will not pretend
otherwise.

**Revocation is not instantaneous across a partition.** A key revoked at 14:00 on one side
reaches an unreachable node when the link returns. The window is real, it is bounded by
reachability rather than by a timer, and it is visible in the audit log afterwards. Fail-closed
alternatives were considered and rejected: a mesh that stops executing when it cannot confirm a
revocation is a mesh with a quorum, wearing a different hat.

**Liveness is stale by design.** Placement decisions are made on observations up to a gossip
round old. A node that went down two seconds ago may still be chosen; it answers `Busy` or the
dispatch fails and spills.

**"No control plane" is not "no state to protect."** Every node holds the whole replicated
state, which means every node holds the audit log, the member list and every sealed secret.
Sealed is doing real work there — a secret's plaintext is wrapped per node and the mesh only
ever replicates the sealed form — but the *metadata* is everywhere by construction.

**Detection is ~10 s under silent packet loss.** See §5.1. Documented because it is measured, and
because a buyer who needs sub-second failover should know before they deploy rather than
after.

**Deterministic time is not in the test harness.** The partition suite asserts on convergence,
which is stable, rather than on timing, which is not. Latency and partial-loss injection do not
exist either. Both are known gaps.

---

## 8. Verifying this yourself

None of the above is meant to be taken on trust.

```console
$ cargo test -p mesh-core --test partition -- --test-threads=1
running 6 tests
test a_cut_actually_drops_datagrams ... ok
test a_partitioned_peer_goes_dead_then_returns ... ok
test a_revocation_signed_during_a_partition_applies_on_heal ... ok
test an_asymmetric_cut_converges_on_heal ... ok
test both_halves_keep_executing_while_partitioned ... ok
test state_from_both_halves_survives_the_heal ... ok
```

The first of those is the one to read first. `a_cut_actually_drops_datagrams` asserts that the
harness is really dropping traffic — because every other test in the file would pass on a mesh
that had merely gone quiet, and **a partition test that cannot prove the partition happened
proves nothing at all.**

The harness itself is `crates/mesh-testkit/src/netsplit.rs`. It hands each node a real kernel
socket behind a shim that consults a directional cut table before each datagram leaves; nothing
is black-holed and every address stays dialable, so the only variable is whether the network
forwards. Cutting and healing are safe to call mid-flight, which is how a test cuts the link
*during* a job.

And for the scale question, a different test: a **1,000-node simulation** that has to converge
(`--test scale`, `thousand_node_certification`), run on demand with `--ignored`.

---

## 9. If you read one paragraph

meshware replaces a control plane with three things: **signatures**, so authority travels with
the data instead of living in a server; **CRDTs**, so state merges the same way no matter what
order it arrives in or how long it was apart; and **leases that expire on their own clock**, so
there is nothing to clean up after a failure. What is left has no leader to elect, no quorum to
lose, and no process whose death stops the mesh — which is why the answer to *"what happens
when the network splits"* is that both halves keep working and the log reconciles itself when
they meet again.

---

*Written 2026-09-06 against `v0.6.11`. Numbers re-measured on any change to the
partition suite; if this page and `crates/mesh-core/tests/partition.rs` ever disagree, the test
is right.*
