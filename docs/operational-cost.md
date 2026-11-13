# Operational cost

**The benchmark page whose headline is not throughput.**

Every competitor's getting-started guide begins *"first, stand up the control
plane."* Meshware's begins *"run this on any machine."* That is the claim this page
makes, and it is a claim about **what you have to operate**, not about how fast
anything runs. The throughput numbers are further down, where they belong.

---

## 1. The table

| | Kubernetes | Nomad | Slurm | **meshware** |
|---|---|---|---|---|
| Servers that run no work | 3–5 (etcd + API server + scheduler + controller-manager) | 3–5 (Raft server quorum) | 2 (`slurmctld` + a backup) + a database daemon | **0** |
| External database | etcd | — | MySQL/MariaDB (`slurmdbd`) | **none** |
| Quorum to maintain | yes | yes | no (single controller, or a configured backup) | **none** |
| Upgrade procedure | ordered, documented, careful | ordered (servers before clients) | ordered (`slurmdbd` → `slurmctld` → `slurmd`) | **replace the binary** |
| Time from zero to scheduling | — | — | — | **measured, published — §3** |

Read the last column as *what is left running when nobody is running a job*. On a
meshware mesh the answer is: the machines you wanted to use, and nothing else.

### Why the competitor columns are citations and not measurements

**Standing up a five-node etcd cluster to race it is a benchmark nobody believes and
everybody can dispute.** Someone always tuned it wrong, sized it wrong, or ran it on
the wrong disk, and the argument is then about the harness rather than about the
architecture.

So every cell in the first three columns is **that project's own documentation**
about its own minimum topology and its own upgrade procedure. The claim is *about
what their docs require*, and their docs are the citation:

- **Kubernetes** — [Options for Highly Available topology][k8s-ha] describes stacked
  and external etcd topologies and states that an HA cluster needs at least three
  control-plane nodes for etcd quorum. [Operating etcd clusters for Kubernetes][k8s-etcd]
  documents etcd as the backing store. [Upgrading kubeadm clusters][k8s-upgrade]
  documents the ordered procedure.
- **Nomad** — [Reference architecture][nomad-ra] recommends three or five server
  agents for the Raft quorum. [Upgrading Nomad][nomad-upgrade] documents upgrading
  servers before clients, one at a time.
- **Slurm** — [Quick start administrator guide][slurm-admin] documents `slurmctld`
  (with an optional backup) and `slurmdbd` against a MySQL/MariaDB database.
  [Upgrade guide][slurm-upgrade] documents the `slurmdbd` → `slurmctld` → `slurmd`
  ordering and the database backup that precedes it.

[k8s-ha]: https://kubernetes.io/docs/setup/production-environment/tools/kubeadm/ha-topology/
[k8s-etcd]: https://kubernetes.io/docs/tasks/administer-cluster/configure-upgrade-etcd/
[k8s-upgrade]: https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/kubeadm-upgrade/
[nomad-ra]: https://developer.hashicorp.com/nomad/docs/architecture
[nomad-upgrade]: https://developer.hashicorp.com/nomad/docs/upgrade
[slurm-admin]: https://slurm.schedmd.com/quickstart_admin.html
[slurm-upgrade]: https://slurm.schedmd.com/upgrades.html

### What the table does *not* claim

It does not claim meshware is better at what those systems are for. Kubernetes runs
a different kind of workload, Slurm has thirty years of batch-scheduling features
meshware does not have, and Nomad's ecosystem is enormous. It claims one thing:
**there is no control plane to build, and therefore none to keep alive.** Whether
that trade is worth it is the reader's call, and the honest costs are in
[`no-control-plane.md` §7](no-control-plane.md) and [limits.md](operating/limits.md).

---

## 2. Why there is nothing to stand up

Three architectural decisions, each of which is load-bearing for the row above it:

**Every node is a server.** There is no client/server split to deploy on two sides.
A node accepts work, schedules work, executes work, stores content and serves the
API. One node is a product; a thousand nodes is the same product.

**State is a signed, replicated op-log, not a database.** Membership, services,
deployments, grants and gangs are conflict-free operations that every node holds and
merges. There is no leader to elect, so there is no quorum to lose,
and a node that was offline catches up by asking anybody.

**Identity is the key.** No CA, no certificate rotation, no PKI to run: a node's
Ed25519 public key *is* its identity, and every connection is TLS 1.3 over QUIC
between keys the mesh has admitted. The step that is usually a project of its own is not a
step.

The cost of these decisions is stated where it is paid, not hidden: eventual
consistency means a placement can be decided on a view of the mesh a gossip round
old — a node that has just filled up answers `Busy` and the work moves on — and a
mesh is a **single trust domain** — nodes are mutually
trusting peers, and multi-tenancy across a security boundary is not what this is.

---

## 3. Time from zero to scheduling

**The one row that has to be earned rather than cited.** It is measured by two
harnesses that fill the same shape, and both ship in this repository.

### The in-process harness (CI, every change)

`crates/mesh-testkit/src/coldstart.rs`, gated by
`crates/mesh-core/tests/coldstart_c3.rs`. It runs the real path — real identity,
real genesis, real QUIC on loopback, the real join handshake, the real scheduler —
and measures four phases. It cannot measure `curl | sh`, because there is no network
and no binary to fetch, so the install phase reports **nothing rather than zero**.

| phase | what it covers |
|---|---|
| `up` | `meshnode up`: identity, genesis, an encrypted listener |
| `invite` | minting a join token |
| `join` | a second machine from nothing to a member |
| `schedule` | submitting work and having an executor accept it |

Run it, and it prints the number:

```console
$ cargo test -p mesh-core --test coldstart_c3 -- --nocapture
cold start: install - · up 7 ms · invite 0 ms · join 17 ms · schedule 4 ms · total 29 ms
cold start json: {"install_micros":null,"up_micros":7274,"invite_micros":280,
                  "join_micros":17137,"schedule_micros":3896,"total_micros":28587}
```

CI gates it at **15 seconds**, which is three orders of magnitude above the measured
figure and deliberately so: the gate exists to catch a regression *in kind* — an
fsync added to the join path, a sleep in startup, a handshake that grew a round trip
— not to police milliseconds on a shared runner. A tight budget on a noisy machine
is a gate people learn to re-run until it passes, which is worse than no gate.

### The real-host script (the published number)

`scripts/cold-start.sh` runs the same five phases with a real binary on a real host,
including the real installer verifying the real signature:

```console
$ ./scripts/cold-start.sh          # a locally built binary
$ ./scripts/cold-start.sh --install   # …and the real installer, verifying the real signature

phase      wall clock
install    -  (not measured; pass --install)
up         25 ms
invite     6 ms
join       37 ms
schedule   87 ms
TOTAL      154 ms

Servers that ran no work during this: 0.
```

That run is a 16-core x86_64 Linux box, release build, no `--install`. **154 ms** from
nothing to a mesh that has accepted work — two nodes, an encrypted listener each, a signed
genesis, a completed join handshake and a scheduled job. The `--install` figure is
dominated by the download and is a property of your network, which is why it is reported
separately rather than folded into a single headline number.

`--json` prints one object per run, for a plot or a CI artifact:

```console
$ ./scripts/cold-start.sh --json
{"install_micros":null,"up_micros":25102,"invite_micros":5986,
 "join_micros":35158,"schedule_micros":87043,"total_micros":153289}
```

It is honest about what it does and does not prove. Both "machines" are the same
host in two data directories on two loopback ports, so it measures the software's
startup, handshake and scheduling costs and **it does not measure a network**. What
it is compared against is not another cluster's throughput — it is another cluster's
*setup*, which is a documented procedure and is cited as one in §1.

### The real-network run

`scripts/e2e-container.sh` runs the lifecycle between real containers on a real
network: **join → push → run → drain → kill → adopt**. It exists because the
cold-start claim needs a real host to be credible: a number measured on loopback in
one process is a number about software, and *"install this on your machines"* is a
claim about machines. It is also where the class of bug that only exists across
hosts shows up — a hard-coded loopback address, an address the mesh advertises but
nobody can reach, a binary that needs a library the container does not have.

---

## 4. Throughput, subordinate

These are good numbers and they are not the headline. They are the design's targets,
measured, and they sit **below** the operational-cost table
because every system in §1 can quote comparable ones, and none of them can quote a
zero in the first row.

| | target | how it is measured |
|---|---|---|
| Job spawn overhead, warm | < 25 ms | `crates/mesh-core/tests/day2_m6.rs` |
| First log byte, LAN | < 100 ms | `crates/mesh-core/tests/day2_m6.rs` |
| 1,000-node fan-out, fully dispatched | < 2 s | `crates/mesh-core/tests/scale.rs` |
| 1 GB bundle to 10 peers, gigabit | < 30 s | content distribution |

Scale certification runs 100 nodes on every CI run; the 1,000-node run is
on demand:

```console
$ cargo test --release --test scale -- --ignored
```

### These numbers are the build that ships

Releases are static musl. Every figure on this page used to come from a glibc
test build, which is not what anybody installs — and musl's allocator serialises
under thread contention, so the difference was not a rounding error. Measured on
a 16-core machine, 100 nodes, median of three:

| build | 100 nodes up | submit→complete |
|---|---:|---:|
| glibc | 9.0 s | 76 ms |
| musl, as shipped before 2026-09-17 | 12.6 s | 72 ms |
| **musl + mimalloc, shipped now** | **9.5 s** | **23 ms** |
| glibc + mimalloc | 9.6 s | 27 ms |

Two separate findings. musl's 40% penalty bringing a hundred nodes up is thread
contention in its allocator, and mimalloc removes it. The three-fold drop in the
dispatch round trip is **not** musl's — glibc with the same allocator measures
the same — so it is a win on the path that matters most — dispatch — and
every node gets it whatever it was built against.

The harness is linked the way the binary is, or this page would be back to
publishing a build nobody runs. Reproduce both columns with:

```console
$ ./scripts/bench-release.sh --compare
```

**One process holds all hundred nodes**, which concentrates allocator contention
far past a real deployment — one node per machine, one process each. That makes
the startup column a stress measurement rather than a forecast. It is still the
right one to watch: it is where a regression shows first.

Parallel compute has its own measured figures, and they include the ugly case:
two 16-core nodes, 400 items — **2.8 s healthy, 6.8 s with a worker `SIGSTOP`ped,
31.6 s with straggler re-runs turned off**. The middle number is the one
worth reading: a frozen worker costs about twice the wall clock rather than eleven
times it, because the mesh re-runs its share instead of waiting it out.

---

## 5. Reproducing everything on this page

```console
# the operational-cost claim
cargo test -p mesh-core --test coldstart_c3 -- --nocapture   # CI gate
./scripts/cold-start.sh --install                             # real host, real installer
./scripts/e2e-container.sh                                    # real network, full lifecycle

# the throughput claims
cargo test --release --test day2_m6
cargo test --release --test scale -- --ignored                # 1,000 nodes
```

Nothing on this page is a number somebody typed in. If a claim here cannot be
reproduced by one of those commands, it is a bug in the page.
