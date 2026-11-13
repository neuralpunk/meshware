# Security policy

meshware executes commands on other people's machines. That is the product, and it is also
the reason this file is longer than most: a reader evaluating meshware needs to know what the
trust model actually promises before they can judge whether a given behaviour is a bug.

---

## Reporting a vulnerability

**Do not open a public issue.**

Use **GitHub private vulnerability reporting**: on
<https://github.com/neuralpunk/meshware>, open *Security → Report a vulnerability*. This opens a
private advisory only the maintainers can see, keeps the whole exchange in one place, and becomes
the published advisory when it is fixed.

If you cannot use GitHub, open a public issue that says only "I have a security report, please
contact me" — no detail — and a private channel will be arranged.

### What to put in it

Whatever you have. A vague report of something real beats a polished report of nothing. If you
can, include:

- what you were able to do that you should not have been able to do,
- the meshnode version (`meshnode --version`) and the OS/kernel,
- whether the mesh was single-node, LAN, or WAN/relayed,
- the smallest reproduction you have — a `meshnode demo` scenario is ideal, because it runs on
  one machine and needs nothing from us to reproduce.

### What to expect

meshware is currently maintained by a very small team, and these are the targets we hold
ourselves to rather than a contractual SLA:

| Stage | Target |
|---|---|
| Acknowledgement that a human has read it | 3 working days |
| Initial assessment — severity, whether we can reproduce it | 10 working days |
| Fix or documented mitigation for a critical issue | 30 days |
| Coordinated public disclosure | Within 90 days of the report, or on the fix, whichever is first |

If we go quiet, that is a failure on our side and not a request for you to stay quiet too.
Ninety days is the outer bound; after it, publish.

We do not currently run a paid bounty. We do credit reporters by name in the advisory and in
the release notes unless you ask us not to.

---

## Supported versions

meshware is pre-1.0 and ships from a single line. **Only the most recent release receives
security fixes.** There is no LTS branch and pretending otherwise would be worse than saying
this plainly.

| Version | Supported |
|---|---|
| Latest release | ✅ |
| Anything older | ❌ — upgrade |

Upgrading is deliberately cheap and is the intended answer: `meshnode upgrade --self` for one
node, `meshnode upgrade --on <ring> --to <version>` for a fleet in health-gated waves. A
supported-release policy with real dates arrives with 1.0.

---

## What the trust model actually claims

A report is judged against these. If meshware violates one of them, it is a vulnerability. If
it does something alarming that is *outside* them, it is probably a documentation bug — tell us
anyway, because a trust model nobody can find is not doing its job.

**Identity.** A node is an Ed25519 keypair and a user is an Ed25519 keypair. The key *is* the
identity: there is no CA, no certificate to expire, and no password anywhere in the system.
Every connection is TLS 1.3 over QUIC in which both ends prove an Ed25519 key. A node dialling a
member checks that the other end holds exactly the key the mesh recorded for it; a node accepting a
connection learns which key is calling, and treats it as a member only if the mesh has admitted
that key — a stranger gets the small, bounded budget in
[network.md](docs/operating/network.md#1-ports). Joining, pairing and probing necessarily dial
before the other key is known, which is what pairing's confirmation code is for. Every governance artifact — a user record, a
role grant, a session policy, a retention window, a deployment, a rollout plan — carries the
signature of the user who authored it, and every node re-verifies that signature **at merge time**
against its own copy of state.

**What a compromised node can do.** Read and execute anything the jobs placed on it could —
including the value of every secret sealed for it, which is why a secret has a scope — see the
replicated state every member sees (which does *not* include any secret's plaintext), and
refuse to do its job — drop ops, fail to forward, fail to report.

**What a compromised node cannot do.** Forge another node's or another user's signature.
Manufacture a user, a role grant or a session policy. Rewrite audit history — audit entries
are node-signed ops in a grow-only log and every row exports with the exact bytes its signature
covers. Decrypt a secret sealed for a different node. Put a binary on the fleet: a rollout plan
needs an admin signature *and* the release itself needs a valid minisign signature checked
against a key the receiving node already holds, so a peer serving content is a CDN and never an
authority.

**What is deliberately not claimed.**

- **Job isolation is filesystem-level. Resource limits are enforced.** Per-job confinement is
  Landlock (any Linux 5.13+ kernel, no root): a job reaches its own directory, `/tmp`, and the
  system paths its tools need, and nothing else — not the node's state, not its content store,
  not another job's checkout, and not the account's credential stores (`~/.ssh`, `~/.gnupg`,
  `~/.aws` and their siblings). `--cpus` and `--mem` are **ceilings rather than
  scheduling hints**: every job gets its own cgroup with `cpu.max`, `memory.max`,
  `memory.swap.max = 0` and `pids.max`, so a job that asked for two cores is confined to two
  cores and one that exceeds its memory is killed by the kernel. That needs a delegated cgroup
  subtree, which the shipped systemd unit requests with `Delegate=yes`; where a node cannot
  enforce — in a container, under a non-systemd init — limits degrade to scheduling inputs and
  **the node says so** rather than accepting a limit it will not apply (`meshnode doctor`, and
  the `meshware_cgroup_enforcing` metric). What is still absent is **a seccomp filter** and
  **per-job user separation**. meshware is built for **trusted administrator automation and
  semi-trusted team workloads**, not for running arbitrary untrusted multi-tenant code. A
  sandbox escape *within* those bounds is a vulnerability.
- **The sandbox still grants `/proc` and `/run`, and confines no network.** Both are stated
  here rather than left to be discovered. Landlock mediates *paths*, and there is no way to
  express "my own `/proc` only" in a ruleset built before the process exists — so a job can
  read `/proc/<pid>/environ` and `/proc/<pid>/fd/*` for processes of the same uid, including
  the daemon's. Per-job user separation is what closes that, and it is not here yet. `/run` is
  granted because `/etc/resolv.conf` is a symlink into it on every systemd-resolved machine,
  and because a job reaching the node's IPC socket is by design — that is how a task calls back
  into the mesh with its job credential. Landlock's `AccessNet` is unused: a job may open any
  network connection the machine can.
- **A terminal is the daemon's account on that machine, less the node's own files.** A session
  policy that grants somebody a shell gives them what that account can do there, minus the
  node's own directory and the account's credential stores. The shell runs under the same kind
  of Landlock ruleset as a job. A closed session is killed, not just asked to leave.
- **A node you give work to can act as you while it holds that work.** A task that calls back
  into the mesh signs with a per-job credential, and the node running it vouches that the
  credential speaks for you. Other nodes accept that only with proof: your own signature on the
  request that put the job there, a job that node really is running for you, and — for a
  fan-out or gang, whose request every member can read — a plan that sent the job to that very
  node. So a compromised node can act as you only if you gave it work, and for no more than a
  week after you signed that request. A deployment instance or scheduled job cannot act for its
  creator on another node at all; automation that needs to runs under a service account.
- **A mesh admin is a mesh admin.** Someone with the `admin` role can grant themselves anything
  and the audit log will faithfully record that they did. Nothing in meshware is a containment
  boundary against an admin key. (Meshware had change control — approvals, freeze windows,
  blast-radius ceilings — and it was [removed](docs/operating/legacy.md#1-what-was-removed).)
- **Anyone who can read a node's data directory has that node's identity.** Keys are 0600 files
  under `<data_dir>/identity/`. Node keys have no HSM or TPM binding (a person's key can have
  one — see below). Local root on a node is game over for that node, by design and in every
  comparable product.
- **A person's key can be sealed, and that is a claim about disks, not about processes.**
  `meshnode identity protect` encrypts `user.key` with XChaCha20-Poly1305 under an
  Argon2id-derived key, and `meshnode identity unlock` opens it once per login session, keeping
  the key-encryption key in `$XDG_RUNTIME_DIR` — tmpfs, cleared at logout, expiring after
  twelve hours. That closes the backup, the sync client and the stolen disk. It does **not**
  close a process running as that person while the key is unlocked; the same account could
  have read the plaintext key. Node keys are not sealed at all: a daemon has nobody to ask.
- **A person's key can instead live on a token, and then there is nothing on the disk to
  take.** `meshnode identity hardware use` binds an identity to a key an agent holds —
  a PIV slot, an OpenPGP card, a YubiHSM — and the private half never leaves the device. That
  closes the case sealing does not: an attacker running as that person can ask the agent for
  signatures *while the token is plugged in*, and cannot walk away with the identity. It fails
  closed: an unplugged token is an error naming the agent, never a fall back to a software key.
  **`meshnode` links no USB library of its own** — it speaks the agent protocol over a Unix
  socket, in pure Rust, which is how a static musl binary with no C toolchain reaches hardware
  at all.
- **An agent cannot say where a key lives; an attestation certificate can.** The mesh will sign
  with a key in a file and a key on a YubiKey and get identical bytes back, so "hardware" is
  not something a client gets to claim. `meshnode identity hardware attest` publishes the
  device's certificate chain, and every node verifies it at merge — the leaf must certify that
  person's own key, the chain must link, and it must end at an authority an admin put in the
  op log. Only then does `meshnode users` show a `HARDWARE` column. A mesh that trusts no
  attestation authority verifies none and shows nothing, because an unverified chain is worth
  less than no chain.
- **Hardware identity is for humans only.** A headless node has no finger to press, so jobs and
  service accounts keep software keys with an expiry on the principal. Node keys stay software
  too, and deliberately: a node key also derives an X25519 scalar for unwrapping secrets and
  exports PKCS#8 to terminate TLS, and no token gives you either. TPM binding for node keys is
  a separate project with a different risk profile, and is still not built.
- **Physical/remote access to the machine running Director** likewise carries that user's key.

---

## Scope

**In scope** — the mesh transport and wire protocol, the op-log and CRDT merge rules,
signature verification anywhere it happens, RBAC enforcement, the secrets
path (sealing, transport, resolution, redaction), the session/PTY path, the sandbox, the
release-verification path, pairing and enrolment, the local IPC socket, `/healthz` and
`/metrics`, packaging and installer scripts, and Director.

**Out of scope** — findings that require an attacker to already hold a valid admin key;
vulnerabilities in third-party dependencies with no meshware-specific exploit path (report
those upstream, and tell us so we can bump); denial of service by a legitimately authorized
user submitting expensive work; anything about a
`meshnode demo` mesh, which binds loopback only and cannot join or be joined by a real mesh;
missing hardening on a machine the operator has configured against our documentation; and
social-engineering the pairing confirmation code, which is an out-of-band human check by
design.

---

## Verifying a release

Every release artifact is signed with [minisign](https://jedisct1.github.io/minisign/). The
public key ships in this repository at [`packaging/minisign.pub`](packaging/minisign.pub), is
embedded in the binary for `meshnode upgrade --self`, and is pinned in
[`packaging/install.sh`](packaging/install.sh) — CI fails if those three ever disagree.

```console
$ minisign -Vm meshnode-v0.7.0-x86_64-unknown-linux-musl \
           -P "$(sed -n 2p packaging/minisign.pub)"
$ sha256sum -c SHA256SUMS
```

Each release also carries a **CycloneDX SBOM** — `meshnode-<version>.cdx.json` for the node
binary, and `meshware-<version>-sbom.cdx.tar.gz` for the whole workspace — and a GitHub
build-provenance attestation, so an artifact can be traced to the commit and workflow run that
produced it:

```console
$ gh attestation verify meshnode-v0.7.0-x86_64-unknown-linux-musl --repo neuralpunk/meshware
```

Nodes fetching a binary through the mesh (`meshnode upgrade --on …`) perform the minisign check
themselves, against their own local copy of the key, before the binary goes anywhere near their
own. That check is not optional and cannot be turned off.

### A second signature, from a key nothing can copy

That the check cannot be turned off has a consequence worth stating plainly: **whoever holds
the minisign release key can push a binary to every fleet running meshware.** It is the one
secret whose loss ends the project, and minisign has no hardware-token support, so the key
cannot simply be moved onto a device.

A mesh can instead require a *second* signature that a file cannot produce:

```console
$ ssh-keygen -Y sign -f ~/.ssh/release_sk -n meshware-release meshnode
$ meshnode upgrade --trust-key ~/.ssh/release_sk.pub    # admin, once, mesh-wide
$ meshnode upgrade --cosign meshnode.sig --version 0.7.3
```

**Opt-in, and fail-closed once opted into.** A mesh that trusts no SSH-form release key behaves
exactly as it always has. The moment one is trusted, a valid minisign signature alone will not
install anywhere in that mesh — which is precisely the check a stolen signing key passes. With
an `sk-ssh-ed25519` key the signature also carries the authenticator's user-presence bit, and
the mesh records it: *a human touched the key to sign this release.* A key copied out of a CI
secret store can never set that bit.

The trusted list holds both kinds of key and tells them apart by shape, so this adds no second
record type and no second admin verb.

---

## Dependency and CVE policy

- **`cargo-deny` runs on every push to `main` and every pull request** — advisories (the RustSec database that
  `cargo-audit` reads), licences, bans and sources. A new advisory affecting a dependency fails CI, which is the point: the build breaking is how we find out.
- **A critical or high-severity advisory with a reachable path in meshware gets a patch
  release**, on the timeline in the table above. We publish the advisory rather than slipping
  the fix into a feature release quietly.
- **Dependencies are chosen conservatively and the reasoning is in the tree.** The workspace
  manifest carries a written justification for the non-obvious ones, the workspace denies
  `unsafe_code` in its own crates, and a dependency that would buy `unsafe` back on a
  security-relevant path has been rejected on those grounds before — see the comment on
  `serde-saphyr` in [`Cargo.toml`](Cargo.toml).
- **No C toolchain in the node.** `meshnode` is a static musl binary with a pure-Rust
  dependency graph apart from `ring`'s vendored assembly. That is a supply-chain decision as
  much as a portability one.

---

## Hardening the daemon

The shipped systemd unit runs the node as an unprivileged system user with an empty capability
bounding set, `NoNewPrivileges`, a read-only view of the system and the kernel's tunables, and a
restricted set of address families. It deliberately has **no syscall filter**: this daemon spawns
your commands, so a filter on the unit would bind every job too. The syscall filter and namespace
restrictions live in an opt-in drop-in, `packaging/systemd/meshnode-hardened.conf`, for fleets
whose workloads are known. The posture is measurable and is checked in CI:

```console
$ ./scripts/security-posture.sh
```

Fleet self-upgrade needs the daemon to be able to write its own binary, which the shipped unit
deliberately forbids. Granting it is a one-line opt-in drop-in
([`packaging/systemd/meshnode-fleet-upgrade.conf`](packaging/systemd/meshnode-fleet-upgrade.conf))
with its reasoning written beside it — a daemon that can rewrite its own binary is a daemon
whose compromise is permanent, so that is a decision an operator makes on purpose rather than
one a feature turns on for them.

---

## Telemetry

There is none. meshware makes no outbound connection you did not configure: no analytics, no
licence check, no update ping. `meshnode upgrade --self` contacts the release channel only when
you run it, and `[release] base` re-points it at an internal mirror. History export
(`meshnode sinks`) is off until an admin signs a sink into existence.

An air-gapped mesh is a supported configuration, not a workaround.
