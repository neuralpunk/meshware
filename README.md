# Meshware

**You have machines. Pair them. Now you have a computer.**

Install one binary on each Linux machine you have, and they pool their CPUs, memory and GPUs.
Run a command on all of them at once, keep a service running across them, or spread a
thousand-item job over whatever is free — with no server to set up first, no database, and
nothing that can be down.

- **One binary, and every machine is equal.** `meshnode` is both the node and the command
  line. There is no control plane, no database and no quorum: nothing to stand up before you
  start, and nothing to keep alive after.
- **Secure with nothing to set up.** Every machine and every person is a key. A machine joins by
  pairing with a six-digit code, or with a single-use invite; every connection is encrypted; every
  job is confined to its own directory. Nobody generates a certificate.
- **A desktop app when you want one.** Meshware Director shows the whole mesh live and does
  everything the command line does, on the Linux desktop.

| Measured | |
|---|---|
| Servers that run no work | **0** |
| 400 items across two 16-core machines | **2.8 s** — and 6.8 s with one worker frozen mid-run |
| A machine that drops off the network | noticed in **5–10 s**, back **51 ms** after it returns |
| Largest mesh tested | **1,000 nodes**, simulated before every release |

**Status:** early development. What it does not do yet is written down in
[limits](docs/operating/limits.md); what changed recently is in the [changelog](CHANGELOG.md).
**Start with [the quickstart](docs/quickstart.md)**: download to a two-machine mesh running your
commands, step by step.

## What it costs to operate

| | Kubernetes | Nomad | Slurm | **meshware** |
|---|---|---|---|---|
| Servers that run no work | 3–5 | 3–5 | 2 + a database daemon | **0** |
| External database | etcd | — | MySQL | **none** |
| Quorum to maintain | yes | yes | no | **none** |
| Upgrade procedure | ordered, documented, careful | ordered | ordered | **replace the binary** |
| Time from zero to scheduling | — | — | — | **measured, published** |

Every competitor's getting-started guide begins *"first, stand up the control plane."*
This one begins *"run this on any machine."* The competitor columns are each project's own
documented minimum topology, cited rather than re-measured — and the last row is measured
here, on every change, by a script you can run:
[`docs/operational-cost.md`](docs/operational-cost.md).

```console
$ ./scripts/cold-start.sh --install    # curl | sh → a two-node mesh → a scheduled job
```

## Quick taste

The short version of [the quickstart](docs/quickstart.md).

Without a second machine — a real three-node mesh on the one you are sitting at, gone when
you stop it:

```console
$ meshnode demo                      # real daemons on 127.0.0.1, a real fan-out, then nothing
```

And for real:

```console
$ meshnode up                        # first run: identity + mesh genesis, fully encrypted listener
$ meshnode status

# on a second machine on the same LAN — the AirDrop moment:
$ meshnode pair                      # announces itself, waits for approval
# …and on any member (or in Director):
$ meshnode pair                      # already in a mesh? the same verb approves instead
                                     # both screens show the same code → confirm → joined

# a whole rack, using the SSH you already have — once:
$ meshnode adopt ops@rack-{1..10}    # install, service, pair, start — after this SSH is never needed again
# …or, in Director: Mesh › Fleet › Add machines — paste the hosts, give it a password or a key,
# confirm each code as it appears. Same result, no terminal.

$ meshnode run 'uname -a' --on any                 # run anywhere, streamed live
$ meshnode run 'apt list --upgradable' --fanout all  # one logical job, every node, multiplexed output
```

One command per input item, spread across every core in the mesh — GNU-parallel syntax, so
existing scripts span the mesh by changing one word:

```console
$ meshnode parallel -a hosts.txt -- ./check.sh {}     # one run per host, pulled by whoever is free
$ meshnode parallel 'gzip -t {}' ::: logs/*.gz
$ seq 1 100000 | meshnode parallel --cpus 2 'process {}'

# …or run a published script once per item — the workers fetch it themselves:
$ meshnode parallel --service check-host --item-input host -a hosts.txt
```

Nothing computes a split: every node pulls the next chunk when it finishes one, so a 32-core
box does about twice the work of a 16-core one on its own. Freeze one of them mid-job and the
rest re-run its share rather than waiting it out — measured on two 16-core nodes, 400 items:
2.8 s healthy, 6.8 s with a worker `SIGSTOP`ped, 31.6 s with that turned off.

When the results are big, `map` keeps each item's output **on the node that computed it** and
sends back only a reference, so nothing funnels through the machine you launched from:

```console
$ meshnode map -a logs.txt 'gzip -dc {} | grep -c ERROR'          # output stays where it is
$ meshnode map -a logs.txt --reduce 'cat "$MESHWARE_INPUTS"/*' \
      'gzip -dc {} | grep ERROR'                                   # merged where the data already is
$ meshnode map -a frames.txt --out ./renders/ './render.sh {}'     # …or pull it down, deliberately
$ meshnode objects                                                 # what this node is holding
```

The line worth repeating: **disk pools cleanly; CPU pools cleanly via scheduling; RAM pools as
a data store, never as an address space.** What none of this will do is make an unmodified
single process use two machines — that is physics.

Some work does not spread — it has to start **together**. Eight nodes, four cores each,
simultaneously, or don't start:

```console
$ meshnode run ./train.sh --gang 8 --cpus 4
· gang-4kq2m7 holding 6/8 (round 1) — behind gang-2we9xa
→ gang-4kq2m7 started 8 member(s) together after 2 offer rounds:
    rank 0  job-9x2… hq
    rank 1  job-7c1… anvil
    …
$ meshnode gangs                       # reservations in flight, most senior first
$ meshnode gangs show gang-4kq2m7      # where the members landed, and who is ahead of it
```

Each member is told where the others are, in the environment every job already gets —
`MESHWARE_GANG_SIZE`, `MESHWARE_GANG_RANK`, `MESHWARE_GANG_NODES`, `MESHWARE_GANG_LEADER`
— so `torchrun --nnodes=$MESHWARE_GANG_SIZE --node_rank=$MESHWARE_GANG_RANK` works with no
meshware-specific code in your training script.

The interesting part is what happens when two of these collide. A gang holds a **lease** on
each node it wants: a soft reservation with a TTL, counted against that node's capacity so
nothing else can take the same cores. If it cannot get all N, it gives back what it took
rather than sitting on half the mesh. And when two large gangs want the same machines, the
**senior** one — priority first, then submission time — takes the junior one's reservations
instead of both retrying forever. That is why two six-node gangs on an eight-node mesh both
finish instead of deadlocking.

The failure model is the part worth stating plainly: **if the node that launched the gang
dies mid-negotiation, nothing has to notice.** Every lease expires on its own clock, the
resources come back, and a successor picks up the replicated plan and re-runs the offer
phase. There is no distributed cleanup path in this design because there is nothing to
clean up.

Accelerators are their own resource, not a `cap.gpus` tag with a number in it. Each node
describes what it actually has — index, model, UUID, memory, NUMA node, health — and that
description is replicated, so **any** node can answer for every node:

```console
$ meshnode nodes --devices
NODE           IDX KIND   MODEL                             MEM  NUMA    UTIL   FREE  TEMP  HELD BY  HEALTH
anvil            0 gpu    NVIDIA GeForce RTX 4090        24.0 GB     0     97%  1.2GB   78C  job-9x2…  ok
anvil            1 gpu    NVIDIA GeForce RTX 4090        24.0 GB     0      0% 23.9GB   41C  -         ok
hq               0 gpu    NVIDIA A100-SXM4-40GB          40.0 GB     1      4% 38.1GB   36C  -         degraded (1 corrected ECC error)

$ meshnode run ./train.sh --gpus 2 --vram-mb 24000        # a filter, not a preference
$ meshnode run ./infer.sh --device model=~4090            # `~x` means "contains x"
$ meshnode run ./train.sh --gang 8 --gpus 1               # eight GPUs across the mesh, all at once
```

A job is given **specific devices**, exported as `CUDA_VISIBLE_DEVICES` and its vendor
siblings, and a job that asked for none gets those set to *empty* rather than left unset —
because unset means "every card on the box" to every framework there is. The reservation lives
in the same node-local ledger a gang lease does, so a `SIGKILL`ed job's card is reusable the
instant the executor notices, and two jobs never get the same one.

A bulk job has a second half, and it is the part every other tool leaves to a shell loop:
the items that failed, the run you want to compare against, and the moment to stop.

```console
$ meshnode map -a hosts.txt --halt-on-fail 5% './patch.sh {}'   # a circuit breaker for a script with a bug in it
✗ stopped after 41 failure(s) of 800 (5%): --halt-on-fail 5%
  fix it, then `meshnode map --resume job-7k1x` re-runs what did not finish

$ meshnode map --resume job-7k1x                   # only the items that failed or never ran; same command, same selector
$ meshnode results diff job-7k1x job-9c2m          # Monday against Friday: which hosts answer differently, which broke
before   job-7k1x (./check.sh {})
after    job-9c2m (./check.sh {})
outcome  3 now failing, 12 changed, 397 same
```

The owner keeps a record of every parallel run — the request, and every item's exit code and
a digest of its output, taken at capture time — so `--resume` needs nothing re-fetched and
`diff` compares two lists of digests. A fan-out (`run --fanout`) diffs the same way, by node,
when it was launched with `--json` or `--outputs`.

### Don't run it again — nothing about it changed

Same service, same bundle, same inputs, five hundred hosts, twice. Every other scheduler does
all of it twice.

```console
$ meshnode run ./build.sh --cacheable --cache-outputs 'out/*' --explain-key
action  act-3f9c1a…

FIELD          DIGEST      VALUE
runtime        7a1c04e2    bash
entrypoint     91bb3f07    build.sh
argv           4c02de91    --target web
env            0ff2a86c    CI=1 TARGET=web
bundle_root    2b19c7fa    bndl-8c1e…
outputs        d3a70b52    out/*
…

$ meshnode run ./build.sh --cacheable --cache-outputs 'out/*'      # first time: 4m12s
$ meshnode run ./build.sh --cacheable --cache-outputs 'out/*'      # anywhere in the mesh:
⚡ cached — act-3f9c1a… (18.4 MB saved, produced by job-7k1x…)
$ meshnode cache stats
hits      312 · misses 44 · rate 88%
saved     6.1 GB
```

This is the only thing here where **the mesh gets faster as it gets bigger**: capacity scales
linearly with nodes, a shared cache scales with *use*. The key is a BLAKE3 digest over an
explicitly enumerated tuple — runtime, entrypoint, argv, declared env, bundle root, input
manifest, output globs, sandbox class, arch, epoch — enumerated by hand, because a key that
silently grows a field invalidates every entry in the mesh and one that silently misses a
field is a correctness bug that presents as a haunting.

Caching is **opt-in and never inferred**: a service declares it or a submission asks for it.
`meshnode run 'date' --cacheable` is not hermetic and is refused rather than quietly cached. Secret
references are hashed unresolved, so a key never leaks a value to anyone who can enumerate
keys. And `[cache] verify_rate = 0.01` re-runs one hit in a hundred and compares — a
disagreement drops the entry, audits it, and says so out loud.

### `make -j∞`

The action cache, pointed at the most common compute pain in a homelab that isn't AI. `meshnode
cc` is a compiler wrapper: it preprocesses each translation unit **locally** — so the machine
that compiles it needs no headers, no checkout and no toolchain beyond a compiler — then
submits it as a cacheable job.

```console
$ make -j64 CC='meshnode cc gcc' CXX='meshnode cc g++'
$ meshnode cc --verbose gcc -c src/parser.c -o parser.o
meshnode cc: parser.o <- anvil (18240 bytes)
$ meshnode cc --verbose gcc -c src/parser.c -o parser.o
meshnode cc: parser.o <- cache (18240 bytes)
```

Because an inline script's action key is the digest of its own body, the same unit with the
same flags is **one key across the whole mesh**: the second machine to build it does not build
it, and neither does the second person. That is distcc and ccache at once, minus the setup
folklore, plus the audit trail — every compile arrives as a signed request from a named user
and lands in the same log every other job does.

[Compiling across the mesh](docs/guides/compiling.md) is the guide.

It never breaks the build. A link step, a `-E`, an assembler input, several sources at once, a
stopped daemon, an empty mesh, a dropped connection — all of it falls back to compiling here,
verbatim. The one thing that does *not* fall back is a real compile error: the remote
compiler's diagnostics are the answer, and building it twice to say the same thing would double
the cost of every broken build.

### A mesh that lives with people

Fan noise is a real API. A rack in a basement can run flat out at any hour; the same machine in
a living room cannot.

```toml
[limits]
quiet = "08:00-23:00 @ 30%"          # a third of the slots while the household is awake
# quiet = ["23:00-07:00 @ 100%", "08:00-23:00 @ 0%"]   # or: nothing at all until bedtime
```

```console
$ meshnode status
  jobs      2 running, 0 queued (4 of 16 slots — quiet hours)
  quiet     08:00-23:00 @ 30%  [America/Chicago]  now: 30% until 23:00
```

Windows are read in the machine's **own** local time, including the two days a year the clocks
move. A capped node stays a full member — it gossips, holds state, owns and adopts work, and is
a perfectly good origin for jobs that run elsewhere. It simply answers `Busy` sooner, and `Busy`
is what the scheduler already spills on, so work lands on a neighbour instead of queueing until
midnight. Work already running is never killed: a cap is a promotion gate, and the verb for
stopping work is `meshnode drain`.

A cap has a cousin with an end time. `drain` finishes accepted work, refuses new work and
exits; a drain **with a deadline** is a maintenance window — the same refusal, the node stays
up, and it rejoins on its own:

```console
$ meshnode drain --until 06:00 --reason "kernel update"     # local time, later today or tomorrow
$ meshnode drain --for 2h
$ meshnode status
  draining  until 06:00 (kernel update) — running jobs finish, new work goes elsewhere
$ meshnode drain --lift                                      # done early
```

Director offers the same thing on your own peer's card: *Pause* for a duration, and *Take
work again*.

### Why did my job land there

meshware has always computed a weighted score over ten terms for every candidate on every
placement, and it used to throw all of it away except the winner. Now it keeps the top five:

```console
$ meshnode jobs explain job-7k1x8q
job       job-7k1x8q…
ran on    anvil
took      4m12s

Scored 34 candidate(s); the top 5 kept their breakdown. Lower is better.

NODE                 TOTAL      load    warmth   failure       rtt    memory      disk
→anvil               0.412     0.125     0.000     0.000     0.031     0.180     0.076
 hq                  0.688     0.500     0.000     0.000     0.024     0.110     0.054
 spare-2             1.204     0.250     0.600     0.100     0.140     0.090     0.024

The runner-up lost mostly on load (+0.375 against the winner).
```

The same structure drives Director's placement chart, and the same scorer answers the question
before you submit:

```console
$ meshnode plan --on tag=gpu --gpus 2
would land on   anvil
slots           6 free of 1 wanted
waits           starts now
runs for        12m–25m, p50 16m
finishes in     12m–25m, p50 16m
                from 41 past run(s)
```

**A range, never a time.** A planner that says "17 minutes" is wrong precisely often enough to
be untrusted forever; one that says "12–25 minutes, p50 16" is right and stays trusted. It is
a *simulation over the real scorer* rather than a second model of the scheduler — it calls the
same ranking function a submission calls — so it cannot drift from where your job will
actually go. `--dry-run` on `run` answers the other question: what it would touch — targets,
environment keys, secret names and blast radius — without dispatching anything.

### Small verbs that were missing

Each of these is one thing a person kept doing by hand.

```console
$ meshnode run ./nightly.sh --singleton nightly-backup   # refused while the last one is still going — anywhere in the mesh
$ meshnode run ./build.sh --fallback local               # no daemon, or nobody can take it: runs right here, and says so
$ meshnode run ./train.sh --gang 8 --notify              # one message when it ends: desktop, ntfy, a webhook, a command — [notify] in meshnode.toml
$ meshnode jobs wait job-9x2 --then 'say "training finished"'

$ meshnode top --follow                                  # redraws every 2s, with five minutes of CPU beside each node
$ meshnode top --devices                                 # every card: utilisation, memory and temperature as bars; --follow for history
$ meshnode nodes diff hq anvil                           # kernel, glibc, driver, python… — for "it only fails on that one box"
$ meshnode doctor --fix                                  # the safe fixes the checks already know, and an honest "cannot fix" for the rest
```

A singleton is a claim on a name, replicated like everything else and held exactly while the
job it names is non-terminal — there is no release and nothing to clean up when the origin
dies, which is the property every lease in this design shares. A fan-out is several jobs and
cannot hold a name; the flag says so rather than guessing which one counts.

Across the internet, same story — no VPN, no port forwarding on the laptop side:

```console
# on a cloud VM:   meshnode up --advertise 203.0.113.5:7777   (+ [wan] relay = "on" to help NATed peers)
$ meshnode invite create                              # token carries the public address
# at home, behind CGNAT:
$ meshnode join mwj1…  && meshnode up                 # discovers how it is seen, punches or relays
```

There is no login screen, and that is the design rather than an omission: **a person is a key on
their own machine**, every request is signed with it, and roles attach to the key. What was
missing was the conveyance — the key had to reach an admin somehow, and it went over Slack as a
52-character string. People now enrol the way machines pair:

```console
# on the new person's machine:
$ meshnode enrol --as mia
Your identity:  mwu1tt5r…  @ laptop-mia
Enrolment code: BRAVE-OTTER-4417

Show this code to a mesh admin. Waiting for approval…

# on any admin's — the request arrived on its own, no code needed in the common case:
$ meshnode users pending
NAME      CODE               FROM         ASKED    KEY
mia       BRAVE-OTTER-4417   laptop-mia   2m ago   mwu1tt5r
$ meshnode users approve BRAVE-OTTER-4417 --role operator
```

The code is a display encoding of the public key plus a nonce, derived identically on both sides
and **never transmitted** — so "read me the code on your screen" is an out-of-band confirmation,
exactly as it is when pairing a node. A request is self-signed and grants nothing: it is a row in
a queue that expires within the hour if nobody acts on it. Approving it is the same
admin-signed `UserAdd` + `GrantAdd` pair it always was. And `meshnode whoami` now answers the
question people actually have — not *who am I*, but *what do I do about it*.

Definitions replicate; binaries never do. That line is not a limitation to route around — it is
why a bad definition is recoverable and a bad binary is a site visit:

```console
$ meshnode definitions               # everything the mesh distributes, and who has caught up
$ meshnode bundles prefetch deploy-api --on tag=edge     # content is fetched lazily by default;
                                                         #   this decides *when*, per node
$ meshnode collectors status gpu     # a collector runs a script on every node it matches…
$ meshnode collectors widen gpu --to 'tag=gpu'           # …so widening its ring is a deployment
```

Nobody scp's a binary to a thousand machines — and nobody should need a release server either,
in a system whose whole point is not having a central one. **Put the binary in the mesh**, and
nodes fetch it from each other:

```console
$ meshnode upgrade --publish ./meshnode --version 0.7.0    # + its .minisig
published meshnode 0.7.0 into the mesh (bndl-22e37679, 21.0 MB)

$ meshnode upgrade --status --to 0.7.0
VERSION       COUNT  NODES
0.7.0             3  hq, forge, anvil            ← target
0.6.12          412  edge-001, edge-002, …       ← outdated

$ meshnode bundles prefetch bndl-22e37679 --on tag=dc-east # warm the ring first
$ meshnode upgrade --on tag=canary  --to 0.7.0             # 3 boxes, watch it
$ meshnode upgrade --on tag=dc-east --to 0.7.0 --batch 32  # then 400, 32 at a time
```

The binary travels as an ordinary content-addressed bundle: pulled by digest, every chunk
verified on write, from whichever peer holds it. **The peer is a CDN, not an authority** — the
receiving node checks the release key's minisign signature against a key it holds locally before
the binary goes anywhere near its own. That is the same trust model an HTTP release server always
had (a release server was never trusted either; that is why minisign exists), minus the single
point of failure. Publishing needs an admin signature *and* a release-key signature, so neither
key alone can put a binary on your fleet.

Works air-gapped, survives any server being down, and scales — 400 nodes pull from each other
rather than from one box. Point `[release] base` at a URL instead if you'd rather have a server;
both paths work.

Services, collectors, deployments and schedules are all ops: publish once, on one node, and
they are everywhere within a gossip round or two. Bundle *content* is
content-addressed and fetched on demand, which is right for a 200 MB environment most of the
fleet will never execute — `prefetch` only asks a set of nodes to pull it before somebody is
waiting on it. Binaries get a third path, and it is the content store again: `upgrade --on` publishes an
admin-signed *plan* — a ring, a version, a wave width, an expiry — and then asks each node to act
on it. Each node checks the plan against
its own state (is that admin still an admin, has the plan been withdrawn, does its selector name
me?) and fetches and verifies **its own** signed release; no binary is ever handed from node to
node, because that would be a lateral-movement primitive the design does not have. A plan
authorizes and never triggers: a node that merges one and is never asked does nothing at all.

Waves make a large fleet finishable without weakening the gate — the next wave does not start
until the previous one is *wholly* back, alive and caught up, so a bad binary costs one wave and
never the fleet. `--on any` is refused, because a mesh that can be upgraded in one command is a
mesh that can be broken in one command. Point `[release] base` at an internal mirror and nothing
leaves your network.

> Fleet self-upgrade needs the daemon to be able to write its own binary, which the shipped
> systemd unit deliberately forbids. Granting it is a one-line opt-in drop-in
> (`packaging/systemd/meshnode-fleet-upgrade.conf`); `.deb`/`.rpm` installs upgrade through apt
> or dnf instead, and meshnode says so rather than failing.

Day-2 is built in, not bolted on:

```console
$ meshnode doctor                    # connectivity, clock skew, disk, ports, sandbox
$ curl -s localhost:7776/healthz     # liveness + vitals on every node
$ curl -s localhost:7776/metrics     # Prometheus, no exporter sidecar
$ meshnode drain --move              # finish running work, move queued work, exit clean
$ meshnode backup                    # one restorable file: identity + mesh state
$ systemctl reload meshnode          # SIGHUP: apply the edited config (what reloads: docs/operating/install.md)
$ meshnode upgrade --self            # signed release → atomic swap → drain → systemd restarts it
$ meshnode upgrade --on tag=canary --to 0.7.0   # …or roll a ring, one node at a time
```

**Meshware Director** is the desktop app, and it does everything `meshnode` does, through the
same signed requests and the same roles. Its sidebar is three groups: **Machines** (*Mesh* — Fleet,
Topology, Placement, Capacity, Updates), **Work** (*Run* — Scripts, New run,
Activity — and *Services* — Deployments, Schedules, Addresses) and **System** (Access, Secrets,
Settings). [The Director guide](docs/director.md) walks through every screen.

**Adding machines and updating them** are the two jobs that used to need a terminal, and
now do not. *Mesh › Fleet › Add machines* takes a list of hosts (`ops@rack-{1..10}` works) and a
password, a key file or your ssh-agent; Director installs `meshnode`, writes the packaged
systemd unit with a service account, opens the mesh port in the firewall, pairs — showing
you both codes and taking one click per machine, or **Confirm all** — starts the service and
waits until the mesh sees each node alive. *Mesh › Updates* answers "is my fleet current" in
one sentence and a ring, fetches a newer release from the channel into the mesh (verified
against a key the fleet trusts) with one button, and installs it on every machine that is behind
with another — over SSH, a few at a time, waiting for each to come back. The remote side of both
is shared with `meshnode adopt`, so a node installed from either is the same node.

Two things live outside the window. **The glance** is a live constellation in the system tray —
one dot per node, coloured by liveness and sized by saturation, with the running-job count in
the menu — so the mesh is something you own and feel rather than a window you open; closing the
window hides to it rather than dropping the peer out of its own mesh. And **pairing** renders
its confirmation code three ways at once: the six digits, the same six digits as a word pair
(`harbor lantern` — a bijection with the code, not a summary of it, so saying the words across
a room is exactly as strong as reading the digits), and a QR. The QR is the one that matters on
a machine with no keyboard: `meshnode join --pair` draws it on the console, and a phone reads
the code off the rack unit you were about to carry a monitor to.

To get Director, see [Getting Director](docs/director.md#getting-director).

## Building from source

[CONTRIBUTING.md](CONTRIBUTING.md): building, testing without running a workstation out of
memory, what CI checks, and releases.

## Running it

**[`docs/operating/`](docs/operating/)** is the manual for the person who has to deploy this,
secure it, upgrade it and be woken up by it:

| | |
|---|---|
| **[quickstart](docs/quickstart.md)** | **Download to a two-machine mesh running your commands, step by step** |
| [what Meshware is](docs/introduction.md) | The words you will meet, and how the pieces fit |
| [running work](docs/guides/running-work.md) · [machines](docs/guides/machines.md) | Commands on one machine or many; adding, tagging, draining and removing machines |
| [compiling](docs/guides/compiling.md) | `meshnode cc`: a build spread across the mesh, and never built twice |
| [people and access](docs/guides/people-and-access.md) · [secrets](docs/guides/secrets.md) | Letting people in with the right role, and keeping passwords out of scripts |
| [Director guide](docs/director.md) | Every screen of the desktop app, and the command that does the same |
| [deployments](docs/deployments.md) · [file reference](docs/reference/deployment-file.md) | Keeping a service running across machines, and every key a deployment file takes |
| [install](docs/operating/install.md) | Four ways onto a machine, and what each leaves behind |
| [network](docs/operating/network.md) | Every port and direction — **one required port, and it is UDP** |
| [security](docs/operating/security.md) | Identity, what a compromised node can and cannot do, sandboxing **and its limits**, hardening |
| [identity](docs/operating/identity.md) | Enrolment, RBAC, service accounts, SSO, interactive access |
| [upgrade](docs/operating/upgrade.md) · [backup](docs/operating/backup.md) | Rolling a fleet and rolling it back; what state exists and how to restore it |
| [compatibility](docs/operating/compatibility.md) · [changelog](CHANGELOG.md) | Which versions work together, and what each release changed |
| [platforms](docs/operating/platforms.md) | What it runs on, what it needs, and where it has been tested |
| [uninstall](docs/operating/uninstall.md) | Taking a machine out of the mesh, and removing everything Meshware put on it |
| [air-gap](docs/operating/air-gap.md) | Running with no internet at all — a supported configuration |
| [troubleshooting](docs/operating/troubleshooting.md) | The questions that actually get asked |
| [limits](docs/operating/limits.md) | **The page that says no** — supported platforms, tested scale, and the gaps by name |
| [removed features](docs/operating/legacy.md) | What was removed from meshware, and what to use instead |
| **[reference](docs/reference/README.md)** | Every config key, environment variable, metric, file format, plugin contract and SDK call, held to the code by CI |

## How it works with no control plane

The claim in the first line of this file is the one a reviewer will push on, so it has a page
of its own — and a test suite behind it rather than an argument:

**[`docs/no-control-plane.md`](docs/no-control-plane.md)** — what is authoritative and what is
merely observed, how membership survives unreachability, who decides placement when there is no
scheduler, what happens on both sides of a partition, and **§7: what this architecture
deliberately does not promise.**

```console
$ cargo test -p mesh-core --test partition -- --test-threads=1
test a_cut_actually_drops_datagrams ... ok               # the guard: prove the cut is real
test both_halves_keep_executing_while_partitioned ... ok # no quorum → no minority that refuses
test state_from_both_halves_survives_the_heal ... ok     # the union merges, nothing lost
test a_partitioned_peer_goes_dead_then_returns ... ok    # dead ≠ evicted
test an_asymmetric_cut_converges_on_heal ... ok          # one-way cuts converge too
test a_revocation_signed_during_a_partition_applies_on_heal ... ok
```

Measured on that harness, not estimated: a peer across a silent-drop partition is **no longer
alive after 5–10 s, Dead after 10–13 s, and Alive again 51 ms after the heal.** A dropped
datagram is not a refused connection, so every probe has a deadline — and **meshware is still
quicker to forgive than to condemn.**

## Security

Vulnerability reports go through [GitHub private advisories][adv], never a public issue. [`SECURITY.md`](SECURITY.md) has the disclosure timeline, the supported-
version policy, and — the part worth reading even if you never report anything — **what the
trust model actually claims**: what a compromised node can do, what it cannot, and what is
deliberately not promised (each job is confined to its own directory by Landlock and held to
its CPU and memory by its own cgroup, but there is no seccomp filter and no per-job user, so
meshware is built for trusted administrator automation and semi-trusted team workloads, not
arbitrary untrusted code).

[adv]: https://github.com/neuralpunk/meshware/security/advisories/new

Every release is minisign-signed against a key pinned in three places that CI keeps in
agreement, ships a CycloneDX SBOM, and carries a GitHub build-provenance attestation:

```console
$ minisign -Vm meshnode-v0.7.0-x86_64-unknown-linux-musl -P "$(sed -n 2p packaging/minisign.pub)"
$ gh attestation verify meshnode-v0.7.0-x86_64-unknown-linux-musl --repo neuralpunk/meshware
```

Two gates run on every push to `main` and every pull request:

```console
$ cargo deny check                   # advisories, licences, bans, sources — deny.toml
$ ./scripts/security-posture.sh      # systemd-analyze on the shipped unit, against a ceiling
UNIT     FILE                                            SCORE   (ceiling )
base     packaging/systemd/meshnode.service                4.3   (ceiling 4.5)
strict     + meshnode-hardened.conf (opt-in)               2.8   (ceiling 3.0)
```

The shipped unit hardens everything that cannot affect a job — no capabilities, read-only
kernel state, no realtime, no setuid, native syscall ABI only. It deliberately stops short of
a syscall filter and namespace restrictions, because **this daemon spawns your commands** and
those two would be inherited by every job it runs. They live in an opt-in drop-in
(`packaging/systemd/meshnode-hardened.conf`) for fleets whose workloads are known, beside the
list of what each one breaks.

There is no telemetry. meshware makes no outbound connection you did not configure — no
analytics, no licence check, no update ping. An air-gapped mesh is a supported configuration.

## License

Meshware is free and open source, dual-licensed under either of

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option. Use it for anything — personal, commercial, at any scale — with no plans, tiers
or machine limits.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
Meshware by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
