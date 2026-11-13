# Security architecture

*Reference. Describes what is in the tree.*

**The page a security review asks for.** What the trust model claims, what it deliberately does
not, and what an operator can turn on.

[`SECURITY.md`](../../SECURITY.md) is the shorter version plus the disclosure process. This one
has the detail.

---

## 1. Identity: the key *is* the principal

There is no CA, no certificate to rotate, no password anywhere, and no login screen.

- A **node** is an Ed25519 keypair. Its public key is its node id.
- A **person** is an Ed25519 keypair on their own machine. Their public key is their user id.
- A **machine principal** ([service account](identity.md#3-service-accounts--a-pipeline-is-not-a-person)) is the same thing,
  marked as a machine and given a clock.

Every connection is **TLS 1.3 over QUIC in which both ends prove an Ed25519 key** — no
certificate chains to anything. A node **dialling** a member checks that the other end holds
exactly the key the mesh recorded for it. A node **accepting** a connection learns which key is
calling and treats it as a member only if the mesh has admitted that key; anyone else gets the
bounded budget in [network.md §1](network.md#1-ports). Joining, pairing and probing necessarily
dial before the other key is known — which is exactly why pairing has a confirmation code. Every API request is
an envelope **signed by the user key**, carrying a timestamp and a nonce; the daemon
authenticates the *key*, never the socket, the machine, or the OS account.

Keys are 32-byte seeds, mode 0600, under `<data_dir>/identity/`.

**A person's key can be sealed at rest.** `meshnode identity protect` encrypts
`user.key` with XChaCha20-Poly1305 under a key derived from a passphrase with Argon2id, and
`meshnode identity unlock` opens it once per login session. What the session keeps, in
`$XDG_RUNTIME_DIR` — tmpfs, mode 0600, removed at logout — is the *key-encryption key*, never
the signing key, and it expires after twelve hours.

This is worth being exact about. It protects the key **at rest**: a backup, a sync client, a
stolen or discarded disk. It does **not** protect it from a process running as you while the
identity is unlocked, which is the same account that could have read the plaintext key.

**A person's key can instead live on a hardware token**, and then there is nothing on the disk
to take: `meshnode identity hardware use` signs through an agent (`gpg-agent`, `ssh-agent`,
`yubikey-agent`) holding a key that never leaves the device. In Director it is *Settings › This
machine › Security key*, which checks the same things before it changes anything. See
[limits.md §6b](limits.md#6b-hardware-bound-identity--a-key-that-cannot-be-copied). Node keys are
neither sealed nor hardware-bound: a daemon has nobody to ask for a passphrase or a touch.

## 2. Governance is signed by people and re-verified everywhere

Every governance artifact — a user record, a role grant, a session policy, a retention window,
an identity provider, a deployment, a rollout plan — carries the **signature of the user who
authored it**, and every node verifies that signature **at merge time** against its own copy of state.

This is what makes a mesh with no control plane safe. A node does not trust an op because a
trusted peer sent it; it trusts it because it can check the signature itself.

## 3. What a compromised node can and cannot do

**Can:** read and execute whatever the jobs placed on it could, including the value of any
secret sealed for it; act for a person *through work that person placed on it*, while it holds
that work (a task calls back into the mesh with a job credential, and other nodes check it against
the person's own signed request); see the replicated state every member sees; refuse to do its
job — drop ops, fail to forward, fail to report.

**Cannot:**

- Forge another node's or another user's signature.
- Manufacture a user, a role grant, a session policy or a secret's scope.
- Rewrite audit history — entries are node-signed ops in a grow-only log, and every row exports
  with the exact bytes its signature covers, verifiable with forty lines of Ed25519 and no
  meshware at all.
- Decrypt a secret sealed for a different node.
- Put a binary on the fleet: a rollout plan needs an admin signature **and** the release needs a
  valid minisign signature checked against a key the receiving node already holds. A peer
  serving content is a CDN, never an authority.

## 4. Secrets

Sealed **client-side**, on the machine the value was typed on: encrypted with XChaCha20-Poly1305
under a random data key, and that data key wrapped per node via X25519 derived from each node's
Ed25519 key. **The plaintext never transits the daemon or replicated state** — it exists only in
the sealing client and, briefly, in the executing process.

References resolve at spawn time on the executing node. They are legal in **environment values
only, never in argv**, because `/proc/<pid>/cmdline` is world-readable. Output is redacted.

**Who can read a secret is whoever can run a job where it is sealed.** The daemon never holds a
plaintext, but a job on a node that holds a secret gets its value, and a job can print it:
`echo {{secret:db}} | base64` gets past redaction. So a secret is only as narrow as its scope:

- **Where.** By default a secret is sealed for, and healed to, every member except
  `no-execute` nodes and quarantined ones.
  `meshnode secrets set NAME --on SELECTOR` (or `secrets scope NAME --on …`) limits it to the
  nodes a selector matches, in `run --on` grammar, and a node outside that set refuses to resolve
  it even if it still holds an old copy.
- **Who.** `--user NAME` and `--role ROLE` (repeatable) limit which people's jobs may resolve it.
  Without either, anyone who may run a job on one of its nodes can read it.

A scope is admin-signed and replicated. Widening one reaches the new nodes on its own; narrowing
one does not take the current value back from nodes that already hold it, so set the value again
after narrowing. `meshnode secrets ls` shows each secret's scope. Nodes older than 1.34 carry a
scope without applying it, so scope a secret once the nodes that hold it are upgraded.

A node that joins after a secret was set is **healed**: a few nodes that can open it wrap the
data key for the newcomer. The wraps are not part of what the admin signed — they cannot be,
since they grow — so a node takes a wrap on its merits: it tries every one addressed to it and
keeps the key that opens the value. A member that plants a wrap that opens nothing therefore
locks nobody out, and a node that still cannot open a secret after a minute says so, which
widens who heals it. Copies of a secret are ordered by what the admin signed, so re-sending an
old record or an old removal changes nothing. If every node that could open a secret has left
the mesh, set it again.

## 5. Job isolation — and its limits, plainly

**This is the section to read before deciding what to run.**

Per-job confinement is **Linux Landlock** on any 5.13+ kernel, unprivileged: a job reaches its
own directory, `/tmp`, and the system paths its tools need — and nowhere else. Not the node's
state, not its content store, not another job's checkout. On a system install it cannot read
the node's identity key.

**Resource limits are also enforced**: each job gets its own cgroup with
`cpu.max`, `memory.max`, `memory.swap.max = 0` and `pids.max`, so `--cpus 2 --mem 4G` are
ceilings rather than scheduling hints. That needs `Delegate=yes` on the unit (shipped); where a
node cannot enforce, `doctor` and `meshware_cgroup_enforcing` say so rather than the node
pretending.

**A job cannot borrow the daemon's reach.** A task calls back into the mesh with its job
credential, and the daemon does nothing for it that the task could not do itself. A pushed tree
is read under the job's own sandbox, so the node's directory and the account's credential stores
are out of reach however the path is spelled. A job credential cannot open a terminal session or
write a backup. And nobody can push or back up over the HTTP API: both name a path on the node,
so they are served over the local socket only.

**A terminal is a shell as the daemon's account, confined like a job.** Granting somebody a
terminal (`[sessions] accept` plus an admin-signed session policy) gives them that account on
that machine. The shell is started under a Landlock ruleset, as jobs are. It cannot
read or write the node's own directory (the identity key, the state, other jobs) or the
account's credential stores, and it can reach everything else the account can.

Two limits follow from how Landlock grants access. First, a terminal cannot create a file
*directly* in a directory that contains the node's directory. On a system install that is
`/var/lib`; on a user install it is the home directory itself, though the home's existing
files and subdirectories stay writable. Second, container terminals are bounded by the
container rather than the ruleset.

A session that is closed — by its idle timeout, its maximum duration, an operator or a drain —
is sent `TERM` and, two seconds later, `KILL`. `[runtime] sandbox = "off"` turns the
confinement off along with the job sandbox. `"on"` refuses to open a terminal it cannot confine.

**A job runs in the environment its submitter signed.** Every node that executes a job checks
its environment variables and its timeout against the signed request, not just its command. Only
meshware's own `MESHWARE_*` keys may differ. `LD_PRELOAD`, `GLIBC_TUNABLES`, `BASH_ENV`,
`SHELLOPTS` and exported shell functions (`BASH_FUNC_*`) are refused in a job's environment
whoever signed it. If a command needs one, put it in the command, where every preview and audit
row shows it.

**A watched git source is fetched the way a job runs.** Whatever a watcher fetches is rolled out
to every service the source bumps, so `git` is run with an empty environment (plus `PATH`,
`HOME`, the SSH agent, GnuPG's home, proxies and CA bundles), cannot prompt, is stopped with
everything it started after `[git] timeout` (10 minutes), and cannot use remote helpers, hooks
or the filesystem monitor. Where the job sandbox is available it is confined like a job: the
node's own directory is out of reach and only its checkout is writable. Two things are each
node's call, in its own `meshnode.toml`:

```toml
[git]
allow_plain = false     # http:// and git:// — no server authentication, no integrity
require_signed = false  # ingest only commits `git verify-commit` accepts
timeout = "10m"
```

With `require_signed`, a commit is fetched, verified against the GnuPG or SSH allowed-signers
trust of the account the node runs as, and only then checked out and ingested.

**A cached result is indexed only for the node that computed it.** A node that publishes an
action-cache entry sends the job spec or `map` plan it ran, and the replica that indexes the entry
checks three things: the publisher ran that work, a person allowed to run it there signed it, and
the work derives the entry's key. A member cannot plant outputs under a key it did not compute. A
node that *did* run the work can still publish a wrong answer for it, as it could return one
without a cache; `verify_rate` spot-checks hits for that. Nodes older than 1.33 are not sent
entries, and their entries are not indexed.

**What is still not there today:**

- **No seccomp filter.** A job may make any syscall the service account can.
- **No per-job user separation.** Jobs on a node run as the same unprivileged account.
- **No network restriction** beyond what the host imposes.

**So: meshware is built for trusted administrator automation and semi-trusted team workloads.**
It is not a multi-tenant sandbox for arbitrary untrusted code, and you should not use it as one.
A sandbox escape *within* those bounds is a vulnerability worth reporting.

## 6. Hardening the daemon

The shipped systemd unit runs the node unprivileged with an **empty capability bounding set**,
read-only kernel state, `NoNewPrivileges`, `ProtectSystem=full`, `RestrictSUIDSGID`,
`RestrictRealtime`, `LockPersonality`, `SystemCallArchitectures=native`, a restricted
address-family set and `UMask=0077`.

It is measured, not asserted:

```console
$ ./scripts/security-posture.sh
UNIT     FILE                                            SCORE   (ceiling )
base     packaging/systemd/meshnode.service                4.3   (ceiling 4.5)
strict     + meshnode-hardened.conf (opt-in)               2.8   (ceiling 3.0)
```

**The shipped unit stops short of a syscall filter and namespace restrictions on purpose.**
This daemon spawns your commands, so a unit-level restriction is inherited by every job it
runs — and meshware does not know what your jobs legitimately call. Those two live in an opt-in
drop-in (`packaging/systemd/meshnode-hardened.conf`) for fleets whose workloads are known and
narrow, beside a list of what each one breaks.

Directives deliberately **not** set, because each would break something real: `PrivateDevices`
(hides `/dev/nvidia*` — no GPU jobs), `ProcSubset=pid` (hides `/proc/pressure` — no capacity
telemetry), `ProtectProc=invisible` (hides PID 1 — init detection degrades),
`MemoryDenyWriteExecute` (breaks every JIT a job might use).

## 7. Supply chain

- **Signed releases.** minisign, with the public key pinned in three places CI keeps in
  agreement: `packaging/minisign.pub`, `packaging/install.sh`, and the binary itself.
- **CycloneDX SBOM** per release — `meshnode-<version>.cdx.json` for the node binary and
  `meshware-<version>-sbom.cdx.tar.gz` for the workspace — covered by `SHA256SUMS` and signed.
- **Build provenance** — `gh attestation verify <file> --repo neuralpunk/meshware` ties an
  artifact to the commit and workflow that built it. A different claim from the signature and
  worth having beside it: a release key can be stolen; provenance also names the source.
- **`cargo deny` on every push to `main` and every pull request** — advisories, licences, bans, sources. CI runs
  on changes; nothing re-checks an unchanged tree on a schedule, so run `cargo deny check` before
  cutting a release from an old commit. A new advisory against
  a dependency breaks the build, which is the point.
- **No C toolchain in the node.** Static musl, pure-Rust graph apart from `ring`'s vendored
  assembly. The workspace denies `unsafe_code` in its own crates.

### Verifying a release

```console
$ minisign -Vm meshnode-v0.7.0-x86_64-unknown-linux-musl \
           -P "$(sed -n 2p packaging/minisign.pub)"
$ sha256sum -c SHA256SUMS
$ gh attestation verify meshnode-v0.7.0-x86_64-unknown-linux-musl --repo neuralpunk/meshware
```

Nodes fetching a binary through the mesh perform the minisign check themselves, against their
own local copy of the key. That check is not optional and cannot be turned off.

## 8. Audit

Every authenticated action becomes an entry in a **grow-only replicated log** — roughly 110
dotted actions, from `job.run` to `policy.override`. Each carries the actor, the action, the
target, the serving node, the outcome, and **the user's own request signature**.

Rows export with everything needed to verify them offline:

```console
$ meshnode audit --output jsonl                    # for a SIEM
$ meshnode sinks add siem --kind webhook …         # or push continuously
```

An exported row carries the authoring node's public key, its signature, *and the exact bytes
that signature covers* — deliberately carried rather than re-derived, so an auditor checks
provenance in any language, forever, with no meshware build.

**No row is dropped for being one of many.** The per-entity rate limit on the op-log does not
apply to audit rows, so a busy pipeline is recorded job for job. What keeps a flood from growing
the log instead is folding. The same user denied the same action, or any key this mesh has not
admitted doing anything, gets one full row per minute per action, then one row saying how many
more there were (with a sample of the keys for strangers). A stranger minting a new key per
request therefore costs two rows a minute. And a node that cannot write to its log refuses what
it would have to record, such as a run or a terminal, until it can write again. It does not
carry the action out unrecorded.

## 9. Telemetry

There is none. See [network.md](network.md#2-outbound) for the complete list of things that can
dial out, all of them off or on-demand.

## 10. What is not claimed

Repeated from [`no-control-plane.md` §7](../no-control-plane.md) because it belongs in a
security conversation too:

- **A mesh admin is a mesh admin.** Someone with the admin role can grant themselves anything,
  and the audit log will faithfully record that they did. Nothing in meshware is a containment
  boundary against an admin key.
- **Local root on a node is that node's identity.** A node's key is a file. There is no TPM or
  HSM binding for node keys, and `identity protect` does not change that — it raises the cost of
  a stolen *disk*, not of code running as the key's owner. (A *person's* key can live on a
  hardware token; §1.)
- **Revocation is not instantaneous across a partition.** It applies when the link returns. The
  window is bounded by reachability and visible in the audit log afterwards.
- **Two halves of a partitioned mesh can each start work the other cannot see.** That is the
  trade for having no quorum.
