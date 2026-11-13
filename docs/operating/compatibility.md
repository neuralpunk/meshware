# Compatibility

*Reference. Describes what is in the tree.*

Which versions of Meshware work together, and what to expect when they differ.

**The short version:**

- **Nodes** of different releases work together. The supported range is two releases apart, which
  is how far a rolling upgrade can leave a mesh.
- **The command line and the daemon on one machine** must be the same release.
- **Only the latest release gets security fixes.**

Every part of Meshware carries one version number: `meshnode`, `meshware-archive` and Director
are built together, at the same version. `meshnode --version` answers for a machine,
and `meshnode upgrade --status` answers for the whole mesh.

---

## 1. Between nodes

Nodes talk a protocol with a major and a minor version, such as 2.36. The minor goes up whenever a
release adds something to the protocol, and each connection agrees on the lower of the two.
**Nodes with the same major version always work together**: they share the mesh's state, and they
run work for each other.

> [!IMPORTANT]
> **The major went to 2 on 2026-09-20, and 1.x does not interoperate with it.** Thirty op kinds
> left behind by the September removals were deleted rather than kept as inert names, and the
> op-log identifies a kind by its position in that list — so every kind after the first deletion
> moved. A 1.x op read by a 2.x node would decode as the wrong kind entirely.
>
> A 2.x node therefore refuses a 1.x peer at the handshake, and refuses a data directory written
> by 1.x rather than migrating it. There is no in-place upgrade: reset the mesh with a 1.x binary
> (`meshnode reset`) and re-found it on 2.x, or start from a fresh data directory. The
> [changelog](../../CHANGELOG.md) has the steps.

A node never sends a request its peer is too old to read. It refuses before sending, names the
version the request needs, and the work goes to a node that can take it. `/metrics` counts these as
`meshware_dispatch_failures_total{reason="too-old"}`, which falls to zero as an upgrade finishes.

### The supported range

**Two releases.** A node on release N works with nodes on N-1 and N-2, and that is what CI tests
against real older binaries. Wider gaps are not refused, and still work, but nobody has tested them.
In practice the range is what a rolling upgrade may leave half done: a mesh partway through an
upgrade for a week is inside it, and a node nobody has touched in a year is not.
[upgrade.md §1](upgrade.md#1-mixed-versions-are-fine) covers what you see when a node is behind.

### Features that need every node

Most new features work as soon as the nodes using them are upgraded. A few are rules the whole mesh
has to follow, and an older node goes on doing what it did before. **Upgrade every node before you
rely on these:**

| Protocol | Feature | What an older node does |
|---|---|---|
| 1.23 | A deployment's rollout rules: canaries, progress deadline, automatic rollback | Rolls the deployment out all at once if it is the one in charge of it. `doctor` names such nodes |
| 1.24 | Finding a copy by name (`<name>.mesh`) | Its copies run, but cannot be found by name |
| 1.25 | Load shedding (`meshnode shed`) | Ignores it and keeps taking work. `meshnode shed status` lists these nodes |
| 1.28 | Canaries that wait for a person to promote them | Promotes them as soon as they are healthy |
| 1.31 | Work submitted through the HTTP API | Refuses it as having an invalid signature |
| 1.32 | Reading the output of work another machine started | Can no longer read it from a newer node |
| 1.34 | [Secret scopes](security.md#4-secrets) | Ignores the scope: it passes the secret to every node, and lets anyone use it |
| 1.36 | [Starting the mesh over](../guides/machines.md#start-the-mesh-over) (`meshnode reset`) | Stays in the old mesh. Adopt it into the new one with `--force` |

A node that is too old to follow something the mesh is doing is marked in `meshnode nodes`, and
`meshnode doctor` says how far behind it is. The [changelog](../../CHANGELOG.md) says what each
release adds.

## 2. The command line and the daemon

`meshnode` is both the command line and the daemon, and on one machine **they must be the same
release**. They talk over a local socket with no version negotiation. A command from a newer
release can send something the daemon has no word for. When that happens it says so, and prints
both versions, instead of failing strangely.

Installed from a package or with `meshnode upgrade`, they are one file, so they always match. The
case to watch is a second copy of `meshnode`, on your `PATH` or in a script, that nobody upgraded.

## 3. Director

Director is a view onto this computer's node, not a node of its own. It installs the `meshnode`
it carries — of Director's release — the first time it needs one, and then talks to it the way the
command line does, so [§2](#2-the-command-line-and-the-daemon) applies: Director and the node on
one machine are the same release because they are the same file.

**Add machines** installs that same `meshnode` on each remote machine. The **Updates** screen
publishes a newer release into the mesh and rolls it out; Director itself is updated separately.

## 4. What a node keeps on disk

| | Rule |
|---|---|
| **Data directory** | A node will not start on a data directory written by a newer release. It stops with exit status 65 and says why, and systemd does not keep restarting it. To go back to an older release, [restore a backup](backup.md) taken on that release. Straight after an upgrade, `meshnode.prev` is the previous binary. |
| **`meshnode.toml`** | Unknown keys are refused, so a file that uses a key from a newer release fails `meshnode config check` on an older one. A key that a release retires is still accepted, with a warning. |
| **Deployment and schedule files** | Unknown keys are refused. Check a file with the release that will run it: `meshnode deployment validate` and `meshnode schedule put --dry-run`. `meshnode deployment schema` prints what that release accepts. |

## 5. What is supported

Meshware is before version 1.0. **A release can change how something behaves.** When it does, the
[changelog](../../CHANGELOG.md) lists the change under *What might stop working*, with what to do.

**Only the latest release gets security fixes.** There is no long-term support branch. Upgrading is
meant to be cheap: `meshnode upgrade --self` for one node, and a rolling upgrade for a fleet
([upgrade.md](upgrade.md)). See [SECURITY.md](../../SECURITY.md#supported-versions). A support
window with dates comes with 1.0.
