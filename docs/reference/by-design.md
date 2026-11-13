# By design — what deployments, schedules and node surfaces do not do

*Reference. Describes what is in the tree.*

**The refusals, with the reason attached to each.** What deployments and schedules *are* is in
[Deployments](../deployments.md), the [deployment file reference](deployment-file.md) and the
[schedule file reference](schedule-file.md). What they are **not** is here, along with the
node-level surfaces they rest on — the port ledger, volumes, `.mesh`, the container and volume
plugin contracts, load shedding, and what `/metrics` says about the scheduler.

Almost every entry ends with *By design* or *A known gap*, and the difference matters: the first
is a decision somebody made and would make again, the second is work that is not done. Nothing
here is a limitation discovered late and written up to excuse it.

> [!NOTE]
> This page was §7a of [limits.md](../operating/limits.md) until 2026-09-20. It moved because the
> two pages answer for two different readers: that one is for somebody deciding whether to deploy
> meshware at all, and this one is for somebody already writing a deployment file and wondering why
> it was refused. Nothing was summarised in the move.

- **Host networking only.** `[deployment.network] mode` takes `"host"` and nothing else; an unknown word is refused rather than silently given host mode. Bridge and CNI are Linux-only even in Nomad and are a much larger project *By design.*
- **A port's allocation is node-local.** `meshnode ports` shows one node's ledger in full, and is answered by that node. `deployment status` shows the port *labels* a deployment declares, not the numbers its instances bound; `meshnode registry <name>` shows those
- **Static ports and `count > 1` are refused at submit, unless `distinct_hosts` is set.** Two instances on one node cannot both bind :8080, and without `distinct_hosts` nothing promises they land on different nodes — on a one-node mesh they land on the same one. Use a dynamic port and read `MESHWARE_PORT_<LABEL>`, or set `distinct_hosts = true` *By design.*
- **A node that advertises no port range takes no port work**, and that is a refusal rather than a neutral unknown — unlike cpu, memory and disk, where an unmeasured dimension never disqualifies. Every node running this build advertises one, so an absent `cap.ports` tag means an older build or a malformed `[network] dynamic_ports`. A malformed value advertises *nothing* rather than falling back to the default *By design.*
- **A volume is one directory on one node**, never a replicated or network one. meshware arbitrates *access* to bytes it did not put there; it does not move them, copy them, or make two nodes agree about them. A deployment claiming a volume therefore pins its instances to the machines that have it *By design.*
- **Nothing snapshots, backs up or garbage-collects a volume.** The lease ends when the job does; whatever the job wrote is still there, and staying on top of that is the operator's job exactly as it is for any directory on the machine *By design.*
- **A writer excludes everything, including readers.** There is no shared-write mode and no advisory one — a lease that could be overridden is not a lease, and the failure it prevents (two jobs writing the same directory) is the one that is unrecoverable rather than merely annoying *By design.*
- **A provisioned volume is remembered by one node, in one file.** `<data_dir>/volumes.json` is not replicated and not backed up; lose the data dir and the directories are still on disk but nothing offers them until an operator writes `[[volume]]` blocks by hand *By design.*
- **meshware never verifies a plugin's quota.** A plugin is asked for `capacity_mb` and believed about what it returns. The reference plugin says outright that it enforces none, and one that claims a size it does not enforce is a plugin whose operator finds out at 3am *By design.*
- **A process runtime gets the host path, not a mount.** `MESHWARE_VOLUME_<NAME>` names the directory; only a container plugin can put it somewhere else, because only a container has a mount namespace to put it in. `path` on a claim is therefore advisory for every non-container runtime *By design.*
- **No port is reserved before the job starts.** Admission refuses a demanded static port that is already held — so the job spills to another node rather than crash-looping on `EADDRINUSE` — but a dynamic port is drawn at spawn, not at placement *By design.*
- **DNS answers on 8600, not 53, and each machine routes `.mesh` to it once.** `sudo meshnode dns install` writes the systemd-resolved drop-in (Consul's recipe for `.consul`, with `DNSSEC=false` only where the resolver validates); `meshnode adopt` does it on nodes where systemd-resolved runs. A machine with another resolver gets the dnsmasq and unbound lines instead
- **An instance is reached at its node's address and the port it was given, and nowhere else.** There is no proxy, virtual IP or ingress. `<name>.mesh` A and AAAA answers carry each healthy instance's node address, SRV carries its port, and `meshnode registry <name>` lists both. A stable name and port — `https://example.com` — is a load balancer in front of those addresses ([network.md](../operating/network.md) §7). Nomad draws the same line *By design.*
- **A job's port must be open from wherever its clients are.** meshware opens no firewall for a job — `meshnode adopt` opens UDP 7777 only — and a job bound to `127.0.0.1` is reachable from its own machine and nowhere else. `MESHWARE_IP_<label>` is the address to bind: the node's listen address, `0.0.0.0` on a node listening on every interface *By design.*
- **`http://<name>.mesh` does not work on port 80.** A name resolves to machines, not to a port; only a client that asks for SRV learns the port. A browser or `curl` needs the address and port from the registry, or a load balancer *By design.*
- **The mesh proxy is retired (2026-09-14).** `[network] proxy` and `proxy_prefix` still load and are ignored with a warning. `meshnode adopt` removes the `CAP_NET_BIND_SERVICE` drop-in it used to install; `dns install` removes that and the machine-wide port-floor sysctl, restoring 1024. A node on an older build still runs its proxy, and a connection it sends through this build's node is refused and falls back to dialling the instance directly
- **`shutdown_delay` and `kill_timeout` are recorded, not yet acted on.** Nothing in this build reads them when an instance stops, so there is no drain window between leaving DNS and being signalled *A known gap.*
- **The responder is authoritative for `.mesh` and forwards nothing.** Anything else is `REFUSED`. A forwarding responder on every node is a recursive resolver fleet nobody asked for, and a much larger attack surface *By design.*
- **Registration is opt-in.** A deployment registers with `[deployment.register]`, a job with `--register`. Nothing registers implicitly — a mesh where every ad-hoc command appeared in DNS is a mesh where DNS means nothing *By design.*
- **`shutdown_delay` is capped at 300 s.** A drain window is seconds: long enough for a load balancer to notice, short enough that one stopping instance cannot hold a rolling update open *By design.*
- **Only A, AAAA and SRV.** No PTR, no TXT, no zone transfer, no DNSSEC *By design.*
- **Rootless podman needs an `/etc/subuid` and `/etc/subgid` range for the user meshnode runs as.** `useradd --system` allocates none, so a package install or a remote deploy used to produce a node that detected its engine, announced `container`, and then failed on `lchown /etc/shadow: invalid argument`. The installer and `meshnode adopt` allocate one now; `meshnode runtimes` reports `broken` with the fix where they could not
- **A container needs podman or docker on the node**, and meshware finds whichever is there. No configuration; `[runtime] container_plugin` is an override for a runtime meshware does not drive itself, and most meshes never set it. `meshnode` still links no OCI client — the four-crate, no-C, static-musl property is load-bearing, and driving a CLI that is already on the machine costs nothing against it
- **A job only goes where its runtime is.** Every node probes itself for all five runtimes at boot and announces the result as `cap.rt`; the scheduler filters on it and the refusal names what is missing. Before this, a container job could be placed on a node with no engine and fail at spawn, with no scheduling consequence and nothing saying why
- **A node running a build below 1.26 announces no runtimes, and is treated as able to run anything.** Unknown, not none — the opposite of how an absent port range or volume list reads. A pre-1.26 node may have a `container_plugin` configured and be running containers perfectly, so excluding it would make upgrading one node silently shrink the mesh's container capacity. `meshnode runtimes` names every node in that state *By design.*
- **Detection is a filter, not a promise.** `podman info` answering does not mean the next `podman run` succeeds. The probe removes nodes that certainly cannot; one that turns out to be unable anyway refuses at admission and the job spills — the same split ports and volumes make *By design.*
- **Probes are node-local and do not replicate.** `meshnode runtimes` gives full detail for the node you ask and only the announced list for every other. A replicated diagnosis would describe a machine as it was when it booted *By design.*
- **Landlock and this node's cgroup do NOT reach a container's processes.** `podman run` hands the workload to `conmon`, which reparents it out of both — so the sandbox shim is not applied to a container runtime at all (it would constrain the client, not the workload, and it breaks rootless podman). What a container has instead is the engine's own controls, and `doctor` reports the exact set by name — including, loudly, when a plugin declares none. Moving the engine in-binary did **not** change this; it is a property of container runtimes
- **The image is the service version's `entrypoint`.** `docker.io/library/nginx:1.25`. The bundle, when there is one, is the *code* — mounted read-only at `/meshware/bundle` *By design.*
- **A container outlives the job unless the plugin removes it.** `podman run --rm` removes it when the *client* exits cleanly, and the client is not the container — conmon reparents the workload. So a plugin must answer `stop` (one container, by job id, idempotent) and should answer `list` (job ids of what it started for this node, by label). meshware calls `stop` on every job exit and `list` at boot; a plugin without `list` reaps nothing after a crash and `doctor` says so by name
- **Only containers this node labelled are reaped.** `list` filters on `meshware.node`, because two nodes on one machine each see the other's containers. A container started before the label existed, or by a plugin that does not set it, is left for an operator rather than guessed at *By design.*
- **An `exec` health check on a container runs inside the container**, through the engine meshware drives (`podman exec` / `docker exec`), so the image needs a shell. With a `[runtime] container_plugin` configured there is no engine to exec through, and the check runs on the host instead — use `tcp` or `http` there
- **A deployment's generation is decided by comparing files, not by a counter.** A put re-parses the published record's own definition and bumps the generation only if something that affects what runs has changed — service, version, targets, inputs, health, restart, the instance environment (ports, volumes, resources), or a switch between a fixed count and `system`. A new size for a fixed count is a resize, not a roll. Rollout knobs are deliberately excluded: raising `canary` must not restart a healthy service. An unchanged re-publish rolls nothing, the way `nomad job run` behaves with an unmodified file *By design.*
- **That comparison depends on the record rendering back to a file that parses.** `deployment show --output toml` is the input to it. A field the renderer drops reads as *changed* on every publish and rolls the deployment for nothing; a render that will not re-parse reads as a first publish and rolls nothing at all while reporting success. `the_whole_record_round_trips` is the test that holds this, and it asserts the whole record rather than a list of fields
- **`canary` promotes on health unless the file says `auto_promote = false`.** Then a settled canary set holds — nothing cut over, nothing further started — until `deployment promote` names its generation. The hold is bounded by `progress_deadline` like any roll: left unpromoted it stalls and, with `auto_revert`, reverts; `progress_deadline = "never"` waits indefinitely. A node below protocol minor 28 promotes on health, and `deployment status` names every member that would under *policy blind*
- **`auto_revert` or `canary` without a `progress_deadline` gets 10 minutes.** A revert fires only when a roll stalls, and a stall needs a deadline, so without one a file asking for `auto_revert` alone would ask for something that could not happen. `progress_deadline: never` declines the default; `deployment show --output toml` prints the effective value
- **`auto_revert` needs an openable user key on the elected reconciler.** The revert is a signed deployment record, and a node cannot forge a user's signature. A reconciler with no user key, or one behind a passphrase nobody has typed, says so in the log and does not revert *By design.*
- **A machine that can never fit an instance is never offered it.** Placement leaves out every machine whose permanent answer is no — too few cores, too little memory or disk for `[deployment.resources]`, no port range when ports are wanted, a volume it does not lend, a runtime it lacks. If no machine fits, nothing is placed and `deployment status` says which requirement nobody meets. Only the runtime used to be checked, and a slot whose hash chose a 2-core machine for a 4-core instance was offered to it every five seconds, forever
- **A machine that refuses an instance is passed over, not retried.** A start goes down a ranked list of machines until one accepts; each refusal steers placement away from that machine for a minute. When every machine refused, the slot waits out the minute rather than asking again every tick
- **A signed run request is good for an hour at the machine that runs it — except for a fan-out or gang target its plan names.** Such a target is accepted, on the machine the plan names, for as long as the group is open and inside the `runs` window, so a `--max-parallel` run across hundreds of machines, or one a successor finished, does not fail at minute sixty. Whether the user may still run there is asked of today's grants, so revoking them stops the next target *By design.*
- **A job is never sent to a second machine while the first may have it.** A submission that went out and got no answer is asked again of the same machine for 20 s — it acknowledges a job it already holds — and moves on only if that machine answers that it does not. No answer at all fails the job as `undetermined:` rather than risk running it twice. A machine that was never reached is safe to pass over at once *By design.*
- **An instance is never offered to a second machine while the first may have it.** An offer that went out and got no answer pins the instance to that machine for two minutes, or until its record arrives. A refusal is safe to spill on because nothing was journaled; a lost answer is not *By design.*
- **Why a slot has no instance is replicated, and recorded at most every half hour per reason.** `deployment.unplaced` audit rows carry it, `status` and Director show the newest from the last 45 minutes beside the slot. Audit rows leave the log only under a finite `audit` window (at least a year), so a slot that cannot be placed for a day adds at most 48 rows *By design.*
- **A stall caused by instances that were never started does not revert.** When every instance that did start is healthy, the stall is about machines, not the release: it is not recorded against the version and `auto_revert` does not fire *By design.*
- **Instances are placed by score, from what every node agrees on.** Load, bundle warmth and recent failures come from replicated state, plus a file's `affinity` and `spread`; round trip, memory, disk and CPU pressure are one node's measurements and count for nothing here, so whichever node is elected reconciler ranks the same way. Ties are broken by the rendezvous hash that used to decide alone, so an idle mesh places exactly as before. `[compute] placement = "pack"` on the reconciling node packs instances too *By design.*
- **`distinct_hosts` and `distinct_property` are rules; `affinity` and `spread` are preferences.** A slot the rules leave nowhere waits, and its row says which rule. Only `distinct_hosts` is checked before publishing — `count` against the machines that could take one — and a `distinct_property` with too few values is found at placement *By design.*
- **A placement rule changes where the next instance goes, never where one already runs.** Adding `distinct_hosts` to a deployment whose two instances share a machine leaves them there until one is replaced. Nomad re-evaluates running allocations against a changed constraint; this does not *By design.*
- **A drain moves an instance before it stops it.** A replacement starts elsewhere, and the original stops once the replacement has been healthy — or, with no check, running — for `migrate.min_healthy_time`; at most `migrate.max_parallel` of a deployment's instances move at once. At the drain's deadline, an hour unless said, whatever is left stops. An upgrade's drain still stops everything at once
- **`meshnode nodes drain` empties a node and leaves it running; `meshnode drain` shuts one down.** The first is two assigned tags, `drain.by=` and `drain.keep-system`, kept through the node's restarts until `--lift`; the second is the node's own announcement, cleared when it restarts. A shutdown with nowhere else for an instance to go stops it at once rather than waiting out the hour for a machine to appear; an admin's drain waits, because the node is still serving *By design.*
- **Eligibility is `nodes contribute --off`**, also spelled `nodes eligibility`: nothing new is placed on the node, and nothing running there moves
- **A node on 0.7.2 or earlier ignores `drain.by`.** As reconciler it cancels a draining node's instances at once, and it keeps placing on a node an admin is draining — which the drained node's own admission refuses, provided that node runs this build
- **Preemption is opt-in and best effort.** Only a deployment whose file names a `priority` above 50 preempts; only a job started `--preemptible`, at least 10 below it, and never another deployment's instance. A preempted job is put back on its queue. One job per start, and only when every machine was full. The slot it frees can be taken by work already queued on that machine, and the start then tries again later *By design.*
- **An older reconciler ignores placement rules, `migrate` and `priority`.** They ride the side-car's instance environment, which a node on 0.7.2 or earlier carries without reading, so while such a node is elected its instances are placed by hash and its drains cancel at once. `deployment status` does not yet name those nodes
- **Version, pattern and set selectors match nothing on an older node.** `fact.x:version>=…`, `fact.x~/…/`, `fact.x?` and `!fact.x?` degrade closed — a narrower set on an old node — which for a grant is the safe direction and for a policy is not
- **A deployment keeps its last ten versions**, and any older one a tag names. A version is a generation — the record and its side-car together — so a scale, a pause or a description edit re-published at the same generation replaces that version rather than adding one. A publish through a node below protocol minor 28 records no version, and removing a deployment ends its history and its tags *By design.*
- **`rollback --generation` and `--tag` restore the whole file except its name and whether it is paused.** Knobs (`auto_promote`) are not part of a version and stay as they are *By design.*
- **`deployment scale` has no bounds.** There is no `scaling { min, max }` block; any fixed count is accepted, and a `system` deployment has no size to scale
- **`deployment restart` restarts under the rollout policy**, `max_unavailable` at a time. There is no `--batch-size`, `--batch-wait` or `--reschedule`
- **`deployment exec` reaches container instances only**, through the engine on the instance's own node, under the same session grant, reason and audit row as `meshnode shell`. There is no exec into a script instance and no `deployment fs` *By design.*
- **Live CPU and memory are measured when asked and never stored.** A container is read from its engine's `stats`; anything else from its job's cgroup or, where cgroups are not enforced, from its main process alone — which undercounts anything it forked, and says `process` *By design.*
- **`auto_revert` goes to the newest earlier version whose own rollout never stalled**, read from the `deployment.stalled` and `deployment.auto_revert` rows the audit log already replicates. When every earlier version stalled too it goes one back and logs that it is doing so; with no published versions to consult, one back
- **An instance on a node that goes away is replaced after `lost_after`, which a fixed-count file gets as 60 s when it says nothing.** The clock starts when the node stops being Alive — 5–10 s into a silent drop — so a replacement is starting roughly a minute and ten seconds after a machine vanishes. Before `lost_after` the instance is left alone and keeps serving its own side of a partition. `lost_after: never` waits for the node however long it takes; `replace: true` starts a replacement at once. When a lost node comes back, the slot has two instances and one is retired — the original, by default, keeps its place (`disconnect.reconcile`). `system` deployments keep waiting, because their slot is the node. **A deployment published by 0.7.2 or earlier keeps waiting for its nodes until it is next `put`**, since the default lives in the signed policy record rather than in the reconciler
- **`.mesh` does not hand out instances whose node is Dead.** They stay in `meshnode registry` as `unreachable`, counted out of healthy, and `deployment status` marks them the same way. A Suspect node's instances still resolve: one missed probe on a relayed link is usually a blip, and discovery that flapped with each one would be worse than discovery that is a probe late
- **A schedule fires one job, not a fan-out.** `every` and `at` are the whole trigger vocabulary; there is no cron string, no `@reboot`, and no webhook
- **Schedules fire at most once a minute.** Cron's own resolution, and every fire costs a job record on every node *By design.*
- **`catch_up` is `skip` or `once`, never `all`.** A node down for a weekend must not wake up and fire 192 backups *By design.*
- **No timezone database.** `tz` takes a fixed UTC offset (`+02:00`), never a zone name. A DST transition that silently moves a production job by an hour is not worth the data dependency *By design.*

## `[network]` — what this node lends

```toml
[network]
# The dynamic host-port range this node will allocate from. A malformed
# value advertises *no* range, which disqualifies this node from port work
# rather than falling back to the default — a typo must not place a service
# somewhere its operator was trying to say something else about.
dynamic_ports = "20000-32000"

# Host ports never handed out, whatever the range says. For whatever else is
# already listening on the machine and did not ask meshware's permission.
reserved_ports = [22, 443, 5432]

# Answer `<service>.mesh` here (N3). On by default: every node holds the
# whole catalogue, so every node answers authoritatively with no round trip.
dns = true

# Not 53 — binding below 1024 needs CAP_NET_BIND_SERVICE. 8600 is Consul's.
dns_port = 8600
# Loopback by default: a resolver on this machine reaches it, the network
# does not. Set 0.0.0.0 deliberately.
dns_bind = "127.0.0.1"

# Which interfaces mesh traffic belongs on. Absent = every interface with a
# usable address, best first. Set it on a host that has more than one answer
# to "where is this machine": an HPC or GPU node whose management NIC holds
# the default route and whose fabric is what the mesh should use, or a build
# host whose docker0 and forty veths are all real addresses reaching nothing.
advertise_interfaces = ["ib0"]
```

A name that matches no interface on the machine **warns and advertises every
interface** rather than nothing: a typo here must not be a node that silently
cannot be dialled. `advertise_interfaces` reloads on `SIGHUP`, and the node
re-announces its addresses when it changes.

Setting `[listen] addr` to a specific address instead is the other way to say
this, and it is stronger: the node then advertises that address and only that
one, because naming it is the operator being specific.

`dynamic_ports` and `reserved_ports` reload on `SIGHUP` too. An operator who
reserved `:5432` because they just installed Postgres should not have to
bounce the daemon to be believed.

**There is normally nothing to write.** Install podman or docker and
meshnode drives it. Both keys below are optional and most meshes set
neither — see `docs/operating/runtimes.md`.

```toml
[runtime]
# Prefer docker where a machine has both. Reorders the search; never
# restricts it.
container_engine = "docker"

# Override the built-in engines for a runtime meshware does not drive:
# gVisor, Firecracker, a wrapper of your own. Wins the resolution order.
container_plugin = "/usr/local/libexec/my-runner"
```

The resolution order:

```
1. [runtime] container_plugin is set  →  use it. An explicit override wins.
2. otherwise                          →  the engine this node detected.
3. neither                            →  refuse, naming what to install.
```

```
<plugin> prepare   JSON on stdin, JSON on stdout — what command runs it
<plugin> stop      MESHWARE_JOB_ID in the environment; remove that
                   container. Idempotent: already gone is success
<plugin> list      MESHWARE_NODE_ID in the environment; print the job id of
                   every container this node started, one per line
<plugin> info      optional; what isolation controls it enforces
```

`prepare` and `stop` are required; `list` and `info` are optional and
`doctor` reports what is missing. meshware spawns the command `prepare`
names — it does not let the plugin run the container — which is what keeps a
container job's logs, cancellation, kill group, secret redaction and
accounting identical to every other job's.

`list` is how a restarting node finds what it left behind. It must filter on
the `meshware.node` label the plugin put on the container at `prepare` time
(the id arrives as `node` in the request): two nodes on one machine each see
the other's containers, and reaping by name alone would have one node remove
the other's work on every restart.

## Shedding load, mesh-wide

```sh
meshnode shed status
meshnode shed submissions --reason "draining the SAN, back by 14:00"
meshnode shed reconcile
meshnode shed clear
```

Two switches, one admin-signed record replicated to every node, and they
split on **who decided to start the work**:

| switch | stops | leaves running |
|---|---|---|
| `submissions` | work a *person* submits — a run, a service, a fan-out | everything already executing |
| `reconcile` | work the *mesh* starts on its own — deployments replacing instances, schedules firing | everything already executing |

**Neither stops running work.** A shed is about what starts next; killing
running work is `meshnode drain`, which is a different verb because it is a
different decision.

**There is no admin bypass.** A shed the person who set it can walk through
is a suggestion. Reads are untouched (`status`, `nodes`, `top`, `doctor`,
logs, the registry, the ledgers) and so are sessions — `meshnode shell` is
how you fix the thing.

> **This is the one capability gate in meshware that fails *open*.** Every
> other protocol minor leaves an older node doing *less*: no registry, no
> rollout policy, no presence requirement. A node below **1.25** carries the
> shed record unread and **keeps accepting work**. So `shed status` prints
> *honoured by N of M nodes* and names the ones that are not, the CLI warns
> at the moment you flip the switch, Director shows the same sentence, and
> `doctor` reports it as a **failure** rather than a warning — an operator
> who believes the mesh has stopped, when it has not, is the worst state
> this feature can be in.

`meshnode shed submissions` and `meshnode shed reconcile` compose: each
reads the current record and changes only its own half, so turning one on
never quietly lifts the other.

## What `/metrics` says about the scheduler

```
meshware_dispatch_seconds_bucket{le="0.005"} 1481
meshware_dispatch_seconds_sum 3.9
meshware_dispatch_seconds_count 1502
meshware_gpu_utilisation_pct{index="0",vendor="nvidia",model="…4090"} 73.5
meshware_gpu_held{index="0",…} 1
```

`meshware_dispatch_seconds` measures **submission in, accepted somewhere**:
selector, feasibility, grants, ranking, the cache lookup and
the round trip to whichever node takes it, every spill included. It stops at
*accepted* — what follows is the workload's time, not the scheduler's.
Dispatch overhead is the cost this product exists to keep small; this is the meter,
and `meshnode status` prints `p50 <5ms · p95 <5ms (12 placed)` so the number
is readable on a mesh with no Prometheus in it.

A histogram rather than a mean, because a mean hides the only thing worth
seeing: dispatch is fast and occasionally is not, and an average over a
thousand fast dispatches and ten slow ones reports the fast number.

The `meshware_gpu_*` gauges are **this node's own cards** — an exporter
reports the machine it runs on — and are **absent** for a card whose
telemetry has not arrived rather than exported as zero, because a saturated
GPU showing up as idle is worse than no series at all. `meshware_gpu_held`
is a different question from utilisation: a card leased by a job that has
not started using it reads 0% and is not available to anybody.

Two vendors' device 0 — a graphics card and an AMD integrated GPU — are two
cards with their own series, readings and leases. Since protocol 1.35 a
reading names its vendor as it crosses the mesh. A node older than that sends
the bare ordinal, so on a machine like this its cards read as unknown
elsewhere (`nodes --devices`, Director, placement) until it is upgraded.

## `[compute] placement` — pack or spread

```toml
[compute]
# "spread" (the default) or "pack".
placement = "spread"
```

**One sign flip over the same fit score.** There is no second scheduler and
no second explanation: `meshnode jobs explain` shows the same ten terms
either way, at the same weights. What changes is what four of them *mean* —
`load`, `cpu`, `memory` and `disk` are read as `1 - raw` when packing, so a
full machine becomes the preferred one.

- **`spread`** keeps every machine responsive. The right answer when the
  nodes are also somebody's desktop, which is the mesh meshware is for.
- **`pack`** fills one machine before touching the next. The right answer
  when the nodes are instances you pay for by the hour and want to be able
  to switch off: the same work on three boxes costs three, not ten.

`device` stays a penalty under **both** policies, deliberately. Filling a
machine's CPU is a choice about cost; filling its GPU makes a job slower by
an order of magnitude and then kills it with an allocation failure. There is
no mesh where "put the second training run on the busy card" is the answer,
so no policy offers it.

Three things worth knowing:

- **It is per node, and it decides where that node *sends* work.** A mesh
  with `pack` on some nodes and `spread` on others is a real state with no
  error message; `meshnode status` prints the line on a node that packs so
  you can see which you are on, and Director's *Mesh › Capacity* says the same.
- **A typo falls back to `spread` with a warning**, rather than refusing to
  start. The failure being guarded against is one mistyped word silently
  inverting every placement decision a node makes.
- **It is the one field in `[compute]` that genuinely hot-reloads.** The
  rest of the section once documented itself as reloadable and none of it
  was ever wired; `SIGHUP` says so by name, and now says so about
  everything *except* this.

## `[[volume]]` — the durable thing this node lends

```toml
[[volume]]
# The name a job claims it by. 1-48 letters, digits, '-' or '_'.
name = "data"
# The directory on this node. Absolute, or the volume is not offered.
path = "/srv/meshware/data"

[[volume]]
name = "models"
path = "/srv/models"
# Refuse write claims outright, whatever a job asks for. The operator's
# statement about the directory outranks the job's wish for it.
read_only = true
```

Declared **per node**, not centrally, because a host volume *is* a property
of one host: the mesh cannot know what is on a machine's disks, and a
central declaration would be a claim nobody could verify. A malformed entry
— a bad name, a relative path, a name declared twice — is dropped with a
warning rather than replaced by a guess, the same posture as
`dynamic_ports`: a volume nobody can claim is better than a volume pointing
somewhere its operator did not mean.

A job claims one with `meshnode run --volume data`, Director's Run screen,
or `[[deployment.volume]]`:

```toml
[[deployment.volume]]
name = "data"
# Where a *container* mounts it. A process runtime has no mount namespace to
# put it in and gets the host path in MESHWARE_VOLUME_DATA instead.
path = "/var/lib/app"
mode = "rw"     # or "ro"
```

**The claim is an exclusive lease, arbitrated by the node that owns the
bytes**, and it is a placement *filter*: only a node that offers the volume,
and can lend it in the mode asked for at that moment, is a candidate. One
writer excludes everything; any number of readers coexist. The lease is a
row in the node's own ledger — `meshnode volumes`, or Mesh › Capacity › Volumes in
Director — taken before the job starts and released the instant it exits.

A **process** runtime reaches its volume through the sandbox: the claimed
paths are added to the job's Landlock ruleset, read-only or read-write to
match the claim, and a `read_only = true` volume is never writable however
the claim was spelled. A **container** gets a real bind mount from its
plugin instead. Both are leases in the same ledger.

Both reload on `SIGHUP`, like `[network]`, and the reload names what
changed — including `volumes: none lent — this node will take no work that
claims one`. Removing a `[[volume]]` stops it being offered; it does not
evict a job already holding it.

## Volumes on demand — what a plugin must do

```toml
[runtime]
# The subprocess that provisions volumes. Absent = this node lends only the
# [[volume]] blocks it was given, which is the default.
volume_plugin = "/usr/local/libexec/meshware-mkdir"
# Handed to it verbatim as MESHWARE_VOLUME_PLUGIN_CONFIG and never read by
# meshware, which has no opinion about what your plugin needs to know.
volume_plugin_config = "/etc/meshware/volumes.conf"
```

```
<plugin> create        make it; print {"path":…,"capacity_mb":…,"detail":…}
<plugin> delete        remove it; must be idempotent
<plugin> fingerprint   optional; anything worth showing an operator
```

Inputs arrive in the **environment**, never in argv: `MESHWARE_VOLUME_NAME`,
`MESHWARE_VOLUME_CAPACITY_MB`, `MESHWARE_VOLUME_PARAMS` (the `--param`
pairs, as a JSON object), `MESHWARE_VOLUME_PATH` where meshware has an
opinion, and `MESHWARE_VOLUME_PLUGIN_CONFIG`.

This is Nomad's Dynamic Host Volume interface on purpose — any executable,
operation in `$1`, one JSON object on stdout — with three differences:

- **A declared configuration path.** Nomad's has none; the convention is a
  file beside the binary, so every deployment invents a different answer.
- **No request chooses the plugin.** A Nomad create request names a plugin,
  and one naming a path outside the plugin directory ran another program as
  the agent: CVE-2026-7474, fixed in Nomad 2.0.1, 1.11.5 and 1.10.11. A
  node here runs only the program its own `[runtime] volume_plugin` names.
- **Every value a request sends is checked before it leaves.** A name or a
  parameter with shell metacharacters is refused, so a plugin author who
  forgets to quote `$MESHWARE_VOLUME_NAME` gets a broken plugin, not a
  shell injection.

A created volume becomes an **ordinary `[[volume]]`** from that instant:
same offer, same claim, same lease, same `cap.volumes` tag, same release.
There is no second lifecycle for provisioned volumes, which is what keeps
the claim and the mount from becoming two facts that can disagree. The
record is `<data_dir>/volumes.json` — this node's own note to itself, not
op-log state, because a claim about a directory on one machine is a claim no
other node can verify.

`meshnode volumes create data --capacity-mb 512 --param pool=fast`, or Mesh
› Capacity › Volumes › *Provision a volume…*. `meshnode volumes rm data` removes it —
refused while any job holds a lease (`--force` does not override that), and
refused outright for a `[[volume]]` block, which is your own line in your
own file and not something a verb should edit.

A reference plugin is in `examples/volume-plugin/meshware-mkdir`: about 150
lines of bash and jq, most of them comments, that provision a directory under
a root you choose. It is the
documentation.

## `.mesh` — what resolves, and what does not

A registered service answers at `<name>.mesh`; one instance answers at
`<job-id>.<name>.mesh`, which is what an SRV target points at.

**Only instances discovery would hand out.** `passing` and `unknown`
resolve; `critical` and `starting` do not. The asymmetry is deliberate: an
*unchecked* service has nobody to say it is broken, so refusing to resolve
it would make declaring a health check a prerequisite for being findable —
while a *starting* one has somebody looking who has not finished, and
handing out an address that is still binding its socket is how a rolling
update drops requests.

A name nothing has registered is `NXDOMAIN`. A name whose instances are all
unhealthy is `NOERROR` with no answers — **not** `NXDOMAIN`, because
resolvers cache that negatively and the name would stay dead in caches after
the instances came back.

Answers carry a 5-second TTL, which is the reconcile tick: a cache entry is
never staler than the registry it came from.

#### Pointing a resolver at it

```
# systemd-resolved, per-interface
[Resolve]
DNS=127.0.0.1:8600
Domains=~mesh
```

#### If you want port 53

meshnode does not take a capability by default. To let it bind 53:

```ini
# /etc/systemd/system/meshnode.service.d/dns.conf
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
```

then set `[network] dns_port = 53`. `meshnode doctor` reports which port is
in force either way — a responder on 8600 that nobody has pointed a
resolver at looks exactly like a mesh where nothing is registered.

## Containers — what a plugin must do, and what it must say

The contract is the one `secrets::provider_get` and the sink writer already
use: a subprocess, JSON on stdin, JSON on stdout, non-zero exit is a
failure with its stderr tail attached.

```
<plugin> prepare   # required. JSON in, {program, args, env, controls} out
<plugin> info      # optional. {name, version, controls, detail}
```

The plugin answers *"what command would run this image?"* — it does **not**
run the container. `meshnode` spawns the command it names, which is what
keeps a container job's logs, cancellation, kill group, secret redaction and
accounting identical to every other job's.

**The `controls` block is a claim the plugin makes about the flags it
passed**, and `doctor` reports exactly that. A plugin that claims nothing is
reported as enforcing nothing, in red. Keep the two in step: an operator
will believe what `doctor` prints.

**None of this is needed to run containers.** Since 1.26 meshnode drives
podman and docker itself — install one and it is found. The plugin contract
is for a runtime meshware does not drive: gVisor, Firecracker, a hardened
wrapper of your own.

A worked example of the whole contract is in
`examples/container-plugin/meshware-podman`. Two things it does that any
plugin will need — and that the built-in engines do the same way:

- **It passes `XDG_RUNTIME_DIR` and `HOME` through in its `env` reply.**
  meshware runs every job with `env_clear()`, deliberately, and rootless
  podman does not survive that. Only the plugin knows what its runtime
  needs, so the exceptions live there rather than as a hole in the executor.
- **It passes `--cgroup-parent` only when the value is a `.slice`.** meshware
  delegates a cgroupfs path; podman under the systemd cgroup manager demands
  a slice name and fails outright on a path. Where it cannot be passed, the
  container lands in podman's own tree — `--memory` and `--cpus` still
  apply, so the limit holds and only the accounting tree is split.

## The mixed-mesh degradation, named

A node below protocol minor **1.24** publishes no `OpBody::RegistryPut`, so
its instances are absent from `meshnode registry` and from `.mesh`. They run
exactly as they always did and simply cannot be found by name — the same
answer a resolve gets for a service that has not started yet. The failure
worth naming is the quiet one: a rolling upgrade *through* this minor makes
a service's instance count appear to shrink in DNS while nothing is down.

A node below protocol minor **1.23** carries `OpBody::DeploymentPolicyPut`
without reading it. The deployment itself is a separate, unchanged op, so it
stays fully visible and fully reconcilable there — what such a node loses is
the *care* with which a roll is done: no canary, no progress deadline, no
`min_healthy_time`, no `lost_after`, no auto-revert.

This is the door-left-open failure, and it is invisible from the roll itself
because a roll without a canary looks exactly like a roll. `meshnode
deployments status <name>` therefore names every live member that is behind,
by machine name, whenever the deployment has a policy to lose — and says
nothing on a mesh where no deployment declares one.
