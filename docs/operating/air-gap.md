# Air-gapped operation

*Reference. Describes what is in the tree.*

**Self-hosted. No cloud control plane. No telemetry. Air-gap capable.** Every word of that is
literal, and this page is how to hold it.

An air-gapped mesh is a **supported configuration**, not a workaround.

---

## 1. What meshware needs from the internet

Nothing.

- **No cloud account.** There is nothing to sign up for.
- **No licence check.** Nothing phones home to decide whether you may run.
- **No telemetry.** No analytics, no crash reporting, no usage ping.
- **No update check.** `meshnode upgrade --self` contacts a release channel *only when you run
  it*.
- **No external database.** State is the replicated op-log, on every node.

The complete list of things that *can* dial out is in
[network.md §2](network.md#2-outbound), and every one is off or on-demand.

## 2. Installing with no internet

Carry three files across: the binary, its `.minisig`, and `packaging/minisign.pub`.

```console
$ minisign -Vm meshnode-v0.7.0-x86_64-unknown-linux-musl -P "$(sed -n 2p minisign.pub)"
$ sudo install -m755 meshnode-v0.7.0-x86_64-unknown-linux-musl /usr/local/bin/meshnode
```

Or carry the `.deb`/`.rpm` into your internal repository and install from there, which is
usually the better answer because upgrades then follow your existing process.

## 3. Upgrading with no internet

**This is the part that is genuinely nicer than the alternatives.** Put the binary in the mesh
once and nodes fetch it from each other:

```console
$ meshnode upgrade --publish ./meshnode --version 0.7.0
$ meshnode upgrade --on tag=canary --to 0.7.0
$ meshnode upgrade --on tag=floor --to 0.7.0 --batch 32
```

No release server, no internal mirror, no scp to four hundred machines. The receiving node
verifies the minisign signature against a key it already holds, so peer-to-peer distribution
costs nothing in trust — the peer is a CDN, never an authority.

If you would rather have a server, point `[release] base` at an internal URL and nothing leaves
your network.

## 4. Content and runtimes

Bundles are content-addressed and replicate through the mesh, so a Python environment or an
Ansible collection pushed once is available everywhere:

```console
$ meshnode push ./deploy-api
$ meshnode bundles prefetch deploy-api --on tag=edge     # decide *when*, per node
```

**Python environments are the one thing to plan for.** The `uv`/venv runtime resolves
dependencies from an index. With no index reachable, pre-build the environment into the bundle
you push rather than expecting resolution at execution time.

## 5. Identity and access

Everything works: enrolment, RBAC, service accounts, session policies, audit. None of it consults
anything outside the mesh.

**SSO is the exception, and it is the obvious one** — an identity provider is by definition
somewhere else. If your IdP is reachable on the internal network, configure it normally. If
there is no IdP at all, enrolment by out-of-band code comparison is the path and it was the
original one.

## 6. History and compliance

The mesh keeps its own history and answers its own questions with no external dependency:

```console
$ meshnode history jobs       # what this node has run
$ meshnode audit …            # the replicated audit log, from any node
$ meshnode audit --output jsonl > evidence.jsonl
```

If you must land history in an internal system, sinks point wherever you say — ClickHouse,
VictoriaLogs, or a webhook or plugin on your own network. See
[backup.md §7](backup.md#7-keeping-history-somewhere-else).

## 7. What to check before you disconnect

```console
$ meshnode doctor
$ meshnode config show          # the resolved config, including anything that would dial out
$ meshnode sinks ls             # should be empty unless you meant it
```

`doctor` flags a retention window that implies an archive nobody is filling, and otherwise stays
quiet.

## 8. The one thing to decide up front

**How you will get a new binary in.** Everything else about an air-gapped mesh is the same
product; the release path is the only workflow that changes. Pick one:

- carry the binary and `upgrade --publish` it into the mesh (no server, works everywhere), or
- mirror releases to an internal URL and set `[release] base` (fits an existing artifact
  process).

Both are supported and neither needs meshware to reach anything.
