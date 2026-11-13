# Supported configurations, tested scale, and what meshware does not do

*Reference. Describes what is in the tree.*

**The page that says no.** An evaluation goes faster when the limits are written down instead
of discovered.

---

## 1. Platforms

| | Supported |
|---|---|
| **Node** (`meshnode`) | **Linux only.** `x86_64` and `aarch64`, static musl — glibc version does not matter |
| **Director** | **Linux only.** It can contribute compute; with that setting off, or beside a running `meshnode`, it joins as a **non-executing peer** (full control, no job execution) |
| Kernel | 5.13+ for Landlock job sandboxing. Older kernels run jobs **unsandboxed** and `doctor` says so |
| cgroups v2 | Capacity reporting, **and enforcement** of `--cpus`/`--mem` where the subtree is delegated — see §4. Not required: without it the node degrades to scheduling inputs and says so |

What each part needs from a machine, and where it has been tested, is [platforms.md](platforms.md).

Linux-only is a deliberate decision, for Director as much as for nodes. For nodes it makes
cgroups v2, Landlock, process groups and `/proc` unconditional, which is what lets the executor
be simple.

## 2. Tested scale

| | Figure | How |
|---|---|---|
| Nodes per mesh | **1,000** | In-process simulation, a **release gate** — a release does not build unless it converges |
| Design envelope | 1 to 1,000 | The size the design is built for |
| Cold start (zero to scheduling) | Measured on every commit | `scripts/cold-start.sh`, published |
| Partition detection | Not alive 5–10 s, Dead 10–13 s | Measured, 4 nodes; tested ≤ 15 s |
| Recovery after heal | **51 ms** | Measured |

**Be precise about what "1,000 nodes" means:** a thousand in-process nodes with real identity,
real QUIC on loopback, real gossip and real stores, converging within targets. It is a genuine
test of the software and it is **not** a test of a network. The real-network multi-node E2E is
planned and not implemented.

**Untested:** packet loss, high-latency WAN links, high churn, and any mesh above 1,000. If your
environment is one of those, say so early and expect to measure together.

## 3. Job execution

- **At-least-once with idempotency keys.** Not exactly-once — nothing distributed is.
- A `--singleton` claim is unique **within a connected mesh**. Across a partition, two halves
  can each start one. See [`no-control-plane.md` §7](../no-control-plane.md).
- Job origins can die: another node adopts the work.
- Fan-out, work-stealing `parallel`/`map`, and gang scheduling are all built and tested.
- **A job's log keeps at most `[disk] job_output_max_mb` per stream** (256 MiB by default,
  `0` = no cap), and nothing at all while its node is under its disk floor. The job is not
  stopped or slowed: the rest of its output is read and dropped, and the log says where. A node
  whose disk fills refuses new work until it has room again, then recovers by itself — see
  [troubleshooting](troubleshooting.md#a-nodes-disk-filled-up).

## 4. Resource limits are enforced — where the node can enforce them

**`--cpus 2 --mem 4G` are real ceilings.** Each job gets its own cgroup with `cpu.max`,
`memory.max`, `memory.swap.max = 0` and a `pids.max`; a job that exceeds its declared memory is
killed by the kernel and told so:

```console
meshware: killed by the kernel — this job exceeded the 64 MB it asked for (peak 64 MB).
Raise --mem, or reduce what it allocates.
```

**This needs a delegated cgroup subtree**, which the shipped systemd unit asks for with
`Delegate=yes`. Without it — in a container, under a non-systemd init, or on a unit somebody
edited — limits fall back to being scheduling inputs only, exactly as before.

**The node never pretends.** Check which mode yours is in:

```console
$ meshnode doctor | grep 'resource limits'
ok    resource limits  enforced (cpu, memory, pids)
$ curl -s localhost:7776/metrics | grep cgroup_enforcing
meshware_cgroup_enforcing 1
```

A fleet where that metric is `0` on some nodes is a fleet where `--cpus` means two different
things depending on where a job lands, and it is worth alerting on.

**Still not enforced:** there is no seccomp filter for jobs, so a job may make any syscall the
service account can. See §5.

### A node in a container advertises the container's memory

`/proc/meminfo` is not namespaced: inside a container it is the *host's*. A node that trusted it
advertised the machine's memory, was sent work sized for it and was OOM-killed — and the
watchdog restarted it to do the same again.

So the memory a node announces, and the memory its admission gate reads, are the smaller of
`/proc/meminfo` and the tightest `memory.max` on the path from its own cgroup to the root
(cgroup v1's `memory.limit_in_bytes` too, which is what an older Docker uses). A limit on an
ancestor counts: it is usually the container's or the unit's cgroup, with the daemon in a child
of it.

This is not only containers. `MemoryMax=8G` on `meshnode.service` is the same statement by
another route, and it is honoured the same way. `doctor` says so when a limit is in force:

```console
$ meshnode doctor | grep 'memory limit'
ok    memory limit  8192 MB by cgroup — this node advertises that, not the machine's memory.
```

**CPU is not adjusted.** `available_parallelism` already reads the CPU affinity mask and
`cpu.max`, so a container-limited node reports the cores it can actually use.

## 5. Isolation tier

meshware is built for **trusted administrator automation and semi-trusted team workloads**.

It is **not** a multi-tenant sandbox for arbitrary untrusted code. Job confinement is Landlock
(filesystem) **plus cgroups v2** for CPU, memory and pids — §4 has the detail and the honest
failure mode. The two things whose absence keeps it out of the multi-tenant tier are **seccomp**
and **per-job user separation**. See
[security.md §5](security.md#5-job-isolation--and-its-limits-plainly).

## 6. Keys at rest — what sealing a user key does and does not do

A person's key can be sealed with a passphrase:

```console
$ meshnode identity protect      # XChaCha20-Poly1305, key from Argon2id
$ meshnode identity unlock       # once per login session
$ meshnode identity status       # protected? unlocked? for how long?
```

**What it protects against.** The key file at rest: a backup, a sync client (Dropbox, iCloud,
`rsync` to a NAS), a stolen or discarded laptop. Those all get 105 bytes of ciphertext instead
of a usable identity.

**What it does not.** Anything running as that person while the key is unlocked. The unlock
lives in `$XDG_RUNTIME_DIR` — tmpfs, mode 0600, cleared at logout, expiring after twelve hours
— and what it holds is the key-encryption key, not the signing key. But the same account that
can read that file could have read the plaintext key, so this is **protection at rest, not
hardware binding**. Hardware binding means a token that will never export the private key, and
for a person's key that is built: §6b.

**Node keys are not sealed.** A daemon has nobody to ask for a passphrase. `SECURITY.md` has
always said local root on a node is that node's identity, and this does not change it.

**Without `$XDG_RUNTIME_DIR`** — a container, cron, a non-systemd box — there is nowhere to
keep an unlock that a backup will not reach, so there is no session at all and every command
needs `MESHWARE_PASSPHRASE`. `meshnode identity status` says which case you are in.

### Where this applies, and where it does not

There are two keys in `<data_dir>/identity/`, and only one of them is ever sealed.

| | Sealed by `identity protect`? | Why |
|---|---|---|
| `node.key` — the machine | **No** | The daemon needs it at boot, unattended. There is nobody to ask for a passphrase. Local root on a node is that node's identity, and always was |
| `user.key` — a person | Yes | The key a human signs governance with. This is the one worth protecting |

**A daemon never needs the user key to be open.** `meshnode up` starts with a sealed
`user.key` and logs that it is running without a founder key — which is the ordinary state of
every node that did not found the mesh. The only thing it cannot do is materialize the
*founder's own* user record and admin grant, and that happens once, at founding.

**Founding a new mesh is the exception.** `meshnode up` on a machine with no genesis and a
locked key is refused, with a message saying to unlock first — because founding with a
substitute key would create a mesh whose admin is an identity no human holds. Joining an
existing mesh needs no unlock.

**On a remote node started by systemd, do not run `identity protect`.** The `user.key` there is
auto-generated and means nothing: no human signs anything on that machine. There is also no
`$XDG_RUNTIME_DIR` for a system service, so there would be nowhere to keep an unlock. Protect
the key on the workstation you actually type commands from.

**Director** reads the human key at `~/.meshware/identity/user.key`, the same file the CLI uses,
and the same session unlock, because both read `$XDG_RUNTIME_DIR`. Unlocking in either place
unlocks the other — there is one unlock, not two mechanisms.

With the key sealed and no unlock in the session, Director shows an **unlock screen before
anything else**, since every action it takes needs the key. Protecting, locking and removing
protection are in **Settings › This machine › Key protection**. So neither front end is the only
way to do any of it (rule D-E3).

### 6b. Hardware-bound identity — a key that cannot be copied

§6 seals a key at rest. That does not stop a process running *as you*, while
your key is unlocked, from taking the key itself. This does:

```console
$ meshnode identity hardware ls        # Ed25519 keys your agent is holding, and which are files
$ meshnode identity hardware use mwu1… # sign with that one from now on
$ meshnode identity hardware status    # where the key is, and whether it is reachable
```

The private half is generated on the device and never comes out. There is
nothing on the disk to steal, seal or back up. An attacker with code
execution on an unlocked machine can ask the agent for signatures while the
token is plugged in, and cannot walk away with the identity — which is the
difference between presence-gating and hardware binding, stated here so
nobody has to infer it.

**`meshnode` does not talk to the hardware.** It talks to an agent that
already does, over a Unix socket, in pure Rust:

| Device | Agent |
|---|---|
| OpenPGP card, firmware 5.2.3+ | `export SSH_AUTH_SOCK=$(gpgconf --list-dirs agent-ssh-socket)` |
| An Ed25519 key in PIV slot 9a, firmware 5.7+ | `ssh-add -s /usr/lib64/opensc-pkcs11.so` (or `libykcs11.so`) — Director's **Connect** does the same with the PIN |
| YubiHSM 2 | whatever agent fronts it |

meshware identities are Ed25519, so the agent has to hold an Ed25519 key:
`yubikey-agent` makes ECDSA P-256 keys, and a FIDO2 `ed25519-sk` key signs in
a different format, so neither can hold an identity. On a YubiKey 5 with
firmware 5.7 or later, make the key once:

```console
$ ykman piv keys generate -a ed25519 --pin-policy once --touch-policy never 9a meshware-key.pem
$ ykman piv certificates generate -s "CN=meshware" -d 3650 9a meshware-key.pem
```

**Slot 9a, and the PIN once per session.** Slot 9c's default PIN policy is
*always* — a PIN for every signature — and meshware signs every request, so
a 9c key asks constantly. Smart-card modules find a PIV key through the
certificate in its slot, which is what the second command is for.

`$MESHWARE_AGENT_SOCK` overrides `$SSH_AUTH_SOCK`, so a mesh identity can
live on a different agent from the one `ssh` uses.

**Failure is closed and says so.** A hardware-bound key whose token is not
plugged in produces an error naming the agent. It never falls back to a
software key — not even to another key file this account could open — and
there is no flag to make it.

**A touch on every command would be intolerable, so nothing here caches
one.** `sign_request` fires on every IPC call — `status`, `nodes`, a tab
completion — and the agent is the right place to decide how often a person
is asked. Use `gpg-agent`'s `default-cache-ttl`, or PIV's `cached` touch
policy, which covers fifteen seconds of signing per touch. Director signs
every time it refreshes — every couple of seconds while its window is open —
so a key that asks for a PIN or a touch on every signature will ask
constantly there, and Director checks for that before switching to a key.

**Director asks one signature at a time, and stops at the first refusal.**
When the agent's key needs you — a passphrase, a PIN, a touch — there is at
most one prompt on screen, however many requests are waiting behind it. If
you cancel it, or the PIN is wrong, or the key is unplugged, Director stops
asking and says so; nothing asks again until you choose **Try again**.

**Director does all of this too**, in *Settings › This machine › Security
key*, and decides with the same code. It connects a YubiKey to the agent
with its PIN, asks the key to sign twice before changing anything (to catch
a key that would ask every time), and adds the one step the CLI prints and
leaves to you: when the mesh does not know the key yet, it shows what letting
it in takes — a second admin for the founder, a rotation for any other
admin — and does that first, after you confirm. It works out the whole path
before taking the first step, so it never rotates an account onto a key and
then finds the binding refused. **Go back to your previous key** undoes a
move by restoring the key it set aside. `--force` stays in the terminal.

**Jobs cannot use this, and never will.** A headless node has no finger to
press. The executor points `MESHWARE_USER_IDENTITY` at a key file so a
running task can call back into the mesh; that stays a software key, and
service accounts (`meshnode serviceaccounts create`) are minted as software
keys with an expiry on the principal. Hardware identity is for humans.

**An agent proves nothing about where a key lives**, and this is the part
that matters. It will sign with a key on a YubiKey and with a key in
`~/.ssh/id_ed25519` and the two answers are identical bytes — and a desktop
agent such as GNOME's offers every key file in `~/.ssh`. So `hardware ls`
names a key that matches a `.pub` file in `~/.ssh` as the file it is,
`hardware use` refuses one without `--force`, and Director does not offer
them. Everything else is only *likely* on a device. What makes the claim
checkable is an attestation certificate:

```console
$ meshnode users attest-root add ca.der --label "Yubico PIV Attestation CA"   # admin, once
$ ykman piv keys attest 9a --format DER leaf.der
$ ykman piv certificates export f9 --format DER intermediate.der
$ meshnode identity hardware attest leaf.der intermediate.der --slot 9a
$ meshnode users                                                             # HARDWARE column
```

The mesh verifies the chain itself, at merge, on every node: the leaf must
certify *your* key, the chain must link, and it must end at an authority an
admin put in the op log. The claim is self-signed by its subject, so an
admin cannot attest a key into somebody else's account.

**The roots are replicated, not compiled in.** A mesh that trusts none
verifies no attestations and shows no `HARDWARE` column — an unverified
chain is worth less than no chain, because it looks like proof. An
organisation minting its own PIV cards adds its own issuing CA and needs no
new build.

**Expiry is reported, not enforced.** A certificate expiring does not make
the key it attested exportable, and a mesh that dropped a security property
on a date nobody chose would be degrading silently. The dates are printed;
rotating on them is your decision.

**A key made on a device is a new identity**, so it has to join the mesh
before this machine can sign with it. `identity hardware use` checks that
first, and refuses a key the mesh does not know, because binding one would
lock the machine out. It prints the step that lets the key in:

```console
$ meshnode users rotate mia --new-key mwu1…      # most people: grants carried, old key retired
$ meshnode users add ada-key --key mwu1… --role admin   # the founder
$ meshnode identity hardware use mwu1…
```

**The founder's steps differ** because the founding key comes from genesis:
it can be neither rotated nor revoked. The device key joins as a second
admin instead, and the founding key becomes the way back in. `use` moves it
aside, to `user.key.set-aside-mwu1…` beside the key file, and says so. Keep
it somewhere safe and off the machine: anyone holding it is an admin, for
good.

**A key the mesh still honours is never overwritten.** `use` moves it aside
and prints where it went. A key that is already revoked, which is what
`users rotate` leaves behind, is simply replaced.

**It refuses to leave the only admin on a token.** A lost token would then
be an unrecoverable mesh, and no support call can fix that. Add a second
admin first, or pass `--force` if you mean it. `--force` also binds a key
the mesh cannot vouch for, and still moves the old key aside rather than
deleting it. `meshnode users rotate` is the recovery path when there is
somebody left to run it.

### 6c. Release signing — the key that can reach every fleet

A node verifies every binary against the minisign release key and the check
cannot be turned off. That is deliberate, and it has a consequence: whoever
holds that key can push a binary to every fleet running meshware. minisign
has no hardware support, so the key cannot be moved onto a token.

What can be required instead is a *second* signature that a file cannot
produce:

```console
$ ssh-keygen -Y sign -f ~/.ssh/release_sk -n meshware-release meshnode
$ meshnode upgrade --trust-key ~/.ssh/release_sk.pub     # admin, once, mesh-wide
$ meshnode upgrade --publish meshnode --version 0.7.3
$ meshnode upgrade --cosign meshnode.sig --version 0.7.3
```

**Opt-in, and fail-closed once opted into.** A mesh that trusts no SSH-form
release key behaves exactly as it does today. The moment one is trusted,
every release needs a co-signature made with it — a valid minisign signature
alone will not install anywhere, which is precisely the check a stolen
signing key passes. `meshnode upgrade --releases` grows a `CO-SIGNED`
column, and a release marked `NO` is one nobody can install. In Director the
same list is under *Mesh › Updates › Advanced › Release keys*, where each
binary without its co-signature has **Attach co-signature…** for the `.sig`
file `ssh-keygen -Y sign` wrote.

The trusted list holds both kinds of key and tells them apart by shape: a
minisign line starts `RW`, an SSH one names its algorithm. One admin verb,
two kinds of signature.

Use `ssh-keygen -t ed25519-sk` for a FIDO2 resident credential and the
signature also carries the authenticator's user-presence bit, which the mesh
records: *a human touched the key to sign this release*. A key stolen from a
CI secret store can never set that bit.

## 7. Multi-tenancy

**Answered with meshes, not namespaces.** There is no project or namespace concept, and there
is not going to be one.

Grants scope what a person may do and where; they do **not** isolate secrets' metadata, audit
or history, which are mesh-wide. Two business units that must not see each other's work should
run **two meshes**. Federating them into one pool is designed and not built.

If that answer does not fit your organisation, it is better to know now than after a
proof-of-concept.

## 7a. Deployments and schedules

**Moved.** The refusals that used to fill this section — roughly seventy of them, plus the node
surfaces they rest on: the port ledger, volumes, `.mesh`, the plugin contracts, load shedding and
the scheduler's metrics — are now
[By design: what deployments, schedules and node surfaces do not do](../reference/by-design.md).

They are reference material for somebody already writing a deployment file, and they were burying
the part of this page an evaluator needs. Nothing was dropped or shortened in the move.

## 8. Known gaps, named

Things enterprises ask for that do not exist today, and where each one stands. *On the list*
means it is tracked as work to do, not that it has a date.

| Gap | Status |
|---|---|
| seccomp filter for jobs (cgroup enforcement of declared limits is **done**) | On the list |
| A second person's approval, and just-in-time elevation, before a terminal opens | On the list |
| Live traces | On the list |
| Compliance evidence export — the signed audit rows exist (`meshnode audit --output jsonl`); scoping them to a date range and a control set does not | On the list |
| A repository as desired state (an `apply` verb) | On the list |
| SCIM, custom roles, centralized session recording, secret versioning, Terraform provider, chargeback | On the list |
| Automatic user provisioning from SSO | Deliberately open |
| Signing with a token when no agent is running. `meshnode` reaches hardware through `gpg-agent`/`ssh-agent` (§6b) and links no USB library of its own | By design |
| A job or service account on hardware. A headless node has no finger to press (§6b) | By design |
| Node keys on a TPM. A node key also derives an X25519 scalar and terminates TLS, neither of which a token exports — a separate project | Not planned yet |
| Real-network E2E; loss/latency/churn matrix | On the list |
| FIPS position | Not planned yet |

## 9. Things meshware is deliberately not

- **Not a secrets-management platform.** It is a delivery mechanism with envelope encryption
  and provider plugins. Integrate Vault; do not expect meshware to be it.
- **Not an observability platform.** It exposes Prometheus metrics, writes structured logs to
  the journal, can export finished jobs as OpenTelemetry spans through a sink, and integrates
  with what you own.
- **Not a PAM suite.** `shell` with mandatory reasons, policies and audit is worth a lot;
  credential vaulting and privilege brokering are a different product.
- **Not SaaS.** The absence of a control plane and the absence of a cloud account are the same
  selling point.
- **Not a service mesh.** A deployment tells you which nodes its instances are on, what
  address and port each one is on, whether it is healthy, and you can resolve it by name.
  **That is the whole extension**, and what you do with it is yours. Ingress, sidecars, mTLS between workloads, intentions, gateways and a
  certificate authority are all out of scope and stay out.
- **Not an image builder, and not a registry.** `runtime = "container"` runs an image somebody
  else built and pushed. There is no `docker build`, no layer cache, no registry — the plugin
  shells out to a runtime that already has all three.
- **Not a workflow engine.** There was one — a DAG with retries, compensation and a canvas —
  and it was removed on 2026-09-12 rather than kept switched off. Multi-step orchestration is
  Nomad's, systemd's, Ansible's and CI's sport; this runs work across machines. What remains of
  it on the wire is a dozen retired op variants that exist only so an older mesh's log still
  decodes — see §8.
