# Changelog

What changed in each release of Meshware, newest first: what is new, what might stop working, and
what to do about it.

One version number covers everything: `meshnode`, `meshware-archive` and Director are released
together. Nodes also speak a **protocol**, versioned separately as major.minor. Nodes with the
same major version work together, and a release that raises the minor says below what a mesh with
older nodes in it can and cannot do. [Compatibility](docs/operating/compatibility.md) has the
rules.

A release cannot be cut without an entry here: the release workflow refuses.

---

## 1.1.0 — 2026-11-13

**Meshware is open source.** This is the first public release, under your choice of the
[MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE) licence, with no plans, tiers or machine
limits — free for any use, personal or commercial, at any scale. It speaks **protocol 2.x**, the
same as 1.0, so 1.0 and 1.1 nodes work together.

The other large change is about trust. **There is no longer a project-wide signing key.** Your
mesh installs a new `meshnode` only when one of *your* admins has signed it, so nobody outside
your mesh — Meshware's maintainers included — can push a binary onto it, and nothing stops working
if the project goes quiet.

### What might stop working

- **`meshnode upgrade --self` no longer upgrades from GitHub on its own.** It verified downloads
  against a key compiled into the binary, and there is none now: Meshware's releases are proven
  by GitHub's build-provenance attestation instead (below). Put a new version on your machines
  by publishing it into the mesh — it is signed with your own release key — and rolling it out:

  ```console
  $ meshnode upgrade --publish ./meshnode-v1.1.0-x86_64-unknown-linux-musl --version 1.1.0
  $ meshnode upgrade --on tag=canary --to 1.1.0
  ```

  `--self` still works against a release server you run and sign for. On a single machine with
  no mesh, reinstall with `install.sh`, a package, or `make install`.

- **Upgrading a 1.0 mesh to 1.1:** publish with the **1.1.0** `meshnode` (the binary you are
  publishing can do it: `./meshnode-v1.1.0-… upgrade --publish ./meshnode-v1.1.0-… --version
  1.1.0`). It trusts your release key mesh-wide as it publishes, and 1.0 nodes read that list,
  so they accept the release.

- **`install.sh` checks provenance, not a minisign signature.** It always checks `SHA256SUMS`,
  and when the GitHub CLI is installed and logged in it also runs `gh attestation verify`. With
  `MESHWARE_REQUIRE_SIGNATURE=1` it stops if it could not.

### Added

- **Your release key.** Every admin has one, derived from their identity: nothing new to store,
  back up or lose. `meshnode upgrade --publish` signs with it when the binary has no `.minisig`
  beside it, and trusts it mesh-wide the first time. `meshnode upgrade --my-release-key` prints
  it as a minisign `.pub`. The signatures are ordinary minisign, so `minisign -V` checks them too.
  A person whose identity is held on a security key signs with minisign as before.
- **Director signs releases the same way.** *Publish a build* lists unsigned builds as ready to
  publish, and *Get* on the Updates screen downloads a release from GitHub, checks it against the
  release's `SHA256SUMS`, and signs it with your key.
- **`make`.** Build, install, try and test Meshware with one front door — `make help` lists it
  all:
  - `make` builds `meshnode` and `meshware-archive`; `make STATIC=1` builds the static musl
    binaries the releases ship.
  - `make director` builds Director (`WITH_NODE=1` carries a `meshnode` inside it), and
    `make director-package` its `.deb` and `.rpm`.
  - `make demo` runs a real three-node mesh on this machine, gone when you stop it.
  - `make doctor` says what this machine is missing and which package provides it.
  - `sudo make install`, `install-service` and `install-director` install what was built, with
    man pages and shell completions; `install-service` never starts the node.
  - `make check` is what a pull request must pass, and `make test-one T=<suite>` runs one
    integration suite in a memory-capped scope.
  - `make dist` collects the binaries, named so they can be published as they are.
- **A website and a user guide** at <https://neuralpunk.github.io/meshware/>.
- **Director as a package.** Releases now carry Director as a `.deb` and an `.rpm` for x86_64,
  each with that release's `meshnode` inside, beside `meshnode`'s own tarballs and packages for
  x86_64 and aarch64. Linux only; there is no AppImage.

### Changed

- **Licence: MIT OR Apache-2.0**, replacing the proprietary licence. Director's *About & licence*
  shows both texts; the "Director Personal" plan, its five-machine ceiling, the sales button and
  every warning about it are gone. Release tarballs, `.deb` and `.rpm` packages carry the licence
  texts.
- **The project's home is <https://github.com/neuralpunk/meshware>.** `upgrade --self`,
  Director's release channel and updater, `install.sh`, the systemd units and the docs point
  there. Security reports go through GitHub's private vulnerability reporting.
- Director's help menu says *Report a problem* and opens the GitHub issues; *Copy diagnostics*
  says which licence a build is under instead of which plan.
- The documentation's screenshots are regenerated from the current build
  (`apps/director/scripts/docs-shots.mjs` remakes them), and the example machines in tests and
  docs have neutral names.

### Fixed

- The tree is clean under Rust 1.100's `rustfmt` and `clippy -D warnings`, so CI passes on
  today's stable toolchain.
- **Security updates in the dependencies**, clearing every open RustSec advisory: `quinn-proto`
  0.11.19 (peer-triggered panics in the QUIC transport), `rustls` 0.23.46 and `rustls-webpki`
  0.103.17, `tokio` 1.54, `zstd`, `russh` 0.65 in Director, and others. Director's updater no
  longer pulls in `zip` at all.
- **A node could crash while a mesh was forming.** Choosing which peers to ask about this
  node's public address sorted them by live connection state, re-read on every comparison; when
  a connection opened mid-sort the order stopped being consistent and the sort panicked. Seen on
  a loaded machine joining 100 nodes. Each peer is now read once before sorting.
- The `list.sort` task in `number` mode no longer panics on a list containing `NaN`.
- **Gangs: `pack` puts a gang on the fewest machines.** It filled machines in load order, so an
  idle four-core node took the first member beside a sixteen-core node that could hold the whole
  gang. It now fills the machine with the most room first.
- A script's saved run defaults (`env`, `inputs`) read back in the same order whatever the
  binary was built with.
- A test still expected the workflow, schedule, policy and host rows removed in 1.0.

---

## 1.0.2 — 2026-09-22

- Director: one icon family throughout, drawn crisp at every size.
- Director: every progress ring and spinner drawn as exact geometry, so none is a pixel off at
  any zoom.

## 1.0.1 — 2026-09-22

- Director: Fleet, Capacity and Updates laid out as cards, on a cooler colour theme.

---

## 1.0.0 — 2026-09-20

**Protocol 2.0.** Nodes from any 1.x release and nodes from this one **do not work together**, and
do not try: the handshake refuses on the major version. A data directory written by 1.x is refused
too, rather than migrated. See *The protocol major went to 2* below for what to do — it is short,
and it has to be done before upgrading.

The largest release so far. **Deployments** grew into a full way to keep services running:
containers, ports, health checks, careful rollouts, names in DNS, and more. **Security** had a full
review, and every finding is fixed. **The node** got sturdier: it keeps its work across a restart,
rolls back a bad upgrade by itself, and notices a dead machine in seconds. **Director** was
reorganised, and much of it rewritten for clarity. And the workflow engine is gone.

### What might stop working

Read this before upgrading.

- **The protocol major went to 2, and there is no in-place upgrade.**

  The September removals — workflows, change control, the host inventory, quotas — each left
  their op kinds behind as inert names, because the op-log identifies a kind by its *position*
  in a list, and deleting one moves every kind after it. Thirty such names had accumulated,
  about a quarter of the list, along with the payload types they carried.

  They are now deleted. Everything after the first deletion renumbered, so an op written by a
  1.x node reads here as a different kind entirely — silently, and then fails its signature
  check on data that was never corrupt. That is what the major version exists to prevent, and
  it is spent here once, deliberately, before this is published: after publication the same
  saving would cost every mesh in existence a reset to collect.

  **What to do**, on each machine, *before* installing this release:

  ```console
  $ meshnode reset          # on the founder, with the 1.x binary still installed
  ```

  Then install this release and found the mesh again (`meshnode up`, then `meshnode adopt` for
  the rest). If you would rather not reset, move the old data directory aside and start fresh —
  `~/.meshware`, or `/var/lib/meshware` for a package install. A 1.x backup can only be restored
  by a 1.x binary, so take one *before* upgrading if you want the old history readable at all.

  A node that is started on a 1.x data directory refuses to start and says all of this, rather
  than reading it wrongly.

- **The Python SDK is removed.** `sdk/python`, the `meshware` wheel that
  shipped with each release, the generated wire types, the API reference
  page and the CI job that gated them are all gone. So is the copy of the
  library that was baked into `meshnode` and offered into every Python
  job's virtualenv — `import meshware` no longer resolves inside a job, and
  `pip install meshware` was never a thing, so there is nothing to install
  instead.

  A Python task that wants to talk to the mesh still can: `MESHWARE_SOCKET`
  and `MESHWARE_USER_IDENTITY` are still in every job's environment, and
  `$MESHWARE_BIN` is the `meshnode` that spawned it. Shell out to it.

  `mesh.map(fn)` went with it, and so did the machinery underneath: the
  `MESHWARE_ANON_*` environment keys and the anonymous-bundle execution
  path in the daemon, which nothing but the SDK ever set. `meshnode
  parallel --service <name>` is the supported way to run a published thing
  per item.

- **Workflows are removed.** So are the `meshnode workflow`, `ai` and `runs` commands, the old
  `meshnode schedule` that started workflows on a clock, and Director's Workflows screen. Scripts
  that call them fail. Run a script or a service instead, and use the new
  [`meshnode schedule`](docs/reference/schedule-file.md) to run a service on a clock. The history of
  runs that already happened stays readable.
- **Change control is removed.** Policies, approvals, freeze windows, blast-radius ceilings,
  `--break-glass` and touch-to-approve are gone, with the `meshnode policy`, `meshnode approvals`
  and `meshnode identity token` commands, `meshnode audit --approvals`, and Director's Change
  control, Approvals and Security keys panels. A policy a mesh already has stops gating work on
  each node as that node is upgraded. `--dry-run` still shows what a run would touch; it no
  longer prints a plan hash or a verdict, and it exits 0. The Python SDK's `PolicyDenied` is gone.
  A security key can still hold your identity: `meshnode identity hardware`.
- **The host inventory is removed.** So are the `meshnode hosts` command, `--hosts` on `run`,
  `parallel` and `map`, `--inventory` on `service run`, fact collectors with `subject: host`,
  Director's Inventory screen and host search, and the `HOSTS` column in `meshnode top`. An
  Ansible service still runs, against the inventory in its own bundle. `[inventory]` in
  `meshnode.toml` is ignored, with a warning when the node starts.
- **Quotas and the usage report are removed**, with the `meshnode quotas` and `meshnode usage`
  commands and Director's Fairness screen. Fair share still orders a busy queue, from the same
  counters, now weighing every person equally; a quota a mesh already has stops applying on each
  node as that node is upgraded. For accounting, a sink still exports the `usage` table.
- **`MESHWARE_LEGACY` is gone.** Nothing is left behind it, so setting it does nothing, and
  neither `meshnode help --all` nor Director has a legacy surface any more.
- **Director runs on Linux only.** The macOS and Windows builds are gone.
- **Director no longer runs a node of its own.** It is a view onto this computer's node — the same
  `meshnode` every other machine runs, as your systemd user service — which it installs and starts
  the first time it needs it (`~/.local/bin/meshnode`, `~/.config/systemd/user/meshnode.service`).
  The node it used to run from `~/.meshware/director` moves to `~/.meshware` on the first start,
  keeping its identity, name and history. Quitting Director no longer takes the computer out of the
  mesh; drain it, or `systemctl --user stop meshnode`, to do that.
- **The Python SDK works only with a node of its own release.** It checks when it connects and
  stops with `VersionMismatch` otherwise. Install the SDK from the same release as your nodes.
- **A secret with no scope no longer reaches guests, or machines that run no work** (including a
  Director that only manages the mesh). Work on those machines can no longer use it. To send a
  secret there, give it a scope that names them: `meshnode secrets set NAME --on SELECTOR`.
- **Jobs cannot set loader or shell start-up variables**: `LD_PRELOAD`, `LD_AUDIT`, `LD_PROFILE`,
  `GLIBC_TUNABLES`, `BASH_ENV`, `SHELLOPTS`, `BASHOPTS` and `BASH_FUNC_*`. A job that sets one is
  refused before it is sent.
- **A job's environment and timeout are part of what you sign.** A node that is not the one you
  sent a job to can no longer change them on the way.
- **A node reads another machine's job output only with your signed request.** Nodes older than
  this release can no longer read the output of work they did not start from nodes that have it.
- **Invite tokens are checked against the invite.** A node that has never heard of an invite
  refuses its token instead of letting anyone in with it.
- **Work submitted through the HTTP API needs a node from this release to run it.** Older nodes
  refuse it as having an invalid signature.
- **Containers now see their environment.** A deployment's `container.env`, its secrets and the
  `MESHWARE_*` variables used to stop short of the container; now they reach it. An image that
  behaves differently when a variable is set will now see it set.
- **`meshnode history` shows jobs, and a new sink exports `jobs`.** Both used to default to
  workflow runs, which nothing records any more. `history runs` and `--tables runs` still work.
- **The global `--timeout` is now `--ipc-timeout`.** Nine commands have a `--timeout` of their
  own (`run`, `jobs wait` and others: how long the work may take), and each one hid the global
  option. Theirs is unchanged. Elsewhere, `--timeout` is refused with a pointer to
  `--ipc-timeout`, and `MESHWARE_IPC_TIMEOUT` still works.
- **`meshnode --version` prints more lines**: the protocol, the target and the commit. The first
  line is unchanged. A script that wants only the version can use `meshnode -V`.
- **The history archive moved** on packaged nodes, from `/var/lib/meshware/.meshware/archive` to
  `/var/lib/meshware/archive`. See [Upgrading](#upgrading-from-072).

### Upgrading from 0.7.2

1. **Upgrade the nodes in rings**, as always ([upgrade.md](docs/operating/upgrade.md)). 0.7.2 and
   this release work together while you do.
2. **Then turn on the features that need every node**: secret scopes, canaries that wait to be
   promoted, and load shedding. Until every node that acts on them is upgraded, older nodes ignore
   them.
3. **On each machine, upgrade `meshnode` and anything else that calls it together**, and install
   the Python SDK from this release.
4. **Packaged nodes**: the configuration now lives in `/etc/meshware/meshnode.toml`. The old place,
   `/etc/meshware/meshware/meshnode.toml`, still works when the new one does not exist, and
   `meshnode doctor` reminds you to move it.
5. **If the archive timer ran** on a packaged node, move what it wrote:
   `sudo mv /var/lib/meshware/.meshware/archive /var/lib/meshware/archive`.
6. **If a machine ran a development build** that had the mesh proxy, run `sudo meshnode dns install`
   on it once. It removes the proxy's low-port setting and its systemd drop-in.
7. **On a desktop that ran Director**, quit the old Director before starting the new one — or
   before running `meshnode up` — so its node can be moved into `~/.meshware`. If `meshnode up` was
   run there earlier and founded a mesh of its own in `~/.meshware`, move that home's `mesh/`,
   `state/` and `identity/node.key` aside first; the node Director ran is then the one adopted.
8. **On machines that should run containers**, run **Update over SSH** once (Director, *Mesh ›
   Updates*; tick **Reinstall** if a machine is already on this version), or upgrade the package.
   Containers are now made by a podman service that only an install sets up, and a fleet upgrade
   replaces only the binary. Until then, `meshnode runtimes` on the machine says so and how to
   fix it.

### Added

**Deployments**

- **Containers**, described in the deployment file: image, command, arguments, working directory
  and environment. Podman and Docker are found and used without any setup.
- **Ports**: each copy gets its own port, given to it as `MESHWARE_PORT_<LABEL>`, or a fixed one.
  A container's port can be published to the one its program listens on.
- **Health checks** by port label, and **names**: `<name>.mesh` answers with every healthy copy's
  address and port. `meshnode dns install` makes `.mesh` names work on a machine.
- **Careful rollouts**: a canary set, a progress deadline, automatic rollback, and canaries that
  wait for a person to promote them (`meshnode deployment promote`).
- **Versions**: a deployment keeps its last ten versions, and a tag keeps any other.
  `deployment history`, `tag` and `rollback --tag`.
- **Change without editing the file**: `deployment scale` and `deployment restart`.
- **One copy up close**: `deployment instance`, `logs`, and `exec` for a terminal inside a running
  container.
- **Placement**: rules for where copies may go, which machines they prefer, and how to spread them;
  a priority, so important work can take a lower-priority job's place.
- **Failover**: a copy on a machine that stays down is started elsewhere.
- **Draining**: `meshnode nodes drain` moves a machine's copies elsewhere before anything stops.
- **Volumes**: directories a machine lends to copies, claimed so two writers never share one.
- **Checking a file before it runs**: `deployment validate`, `init` for a commented starting file,
  `schema` for editors, and `put --dry-run` for what would change.
- **Load shedding**: `meshnode shed` stops the whole mesh taking new work, without touching a node.

**Running things**

- **Schedules**: `meshnode schedule` runs a service on a clock.
- **Runtimes**: each machine finds what it can run (containers, Python, Ansible, and more) and only
  gets work it can do. `meshnode runtimes` shows what a machine found.
- **Port and volume tables**: `meshnode ports` and `meshnode volumes`.
- **The node keeps its work across a restart**: running jobs get time to finish, and queued jobs
  wait instead of failing.
- **`meshnode run --stream`** prints every machine's lines of a fan-out the moment they arrive,
  for output you want to watch live, such as `tail -f` or a long build.
- **`--color auto|always|never`**, and `NO_COLOR` and `CLICOLOR_FORCE`, decide whether
  `meshnode` uses colour.

**Security and access**

- **Secret scopes**: which machines receive a secret, and which people and roles may use it.
- **Invites can be listed and revoked**: `meshnode invite ls` and `invite revoke`, and a Revoke
  button in Director.
- **A passphrase on your key**: `meshnode identity protect`, then `unlock` once per login.
  Director asks for it at start.
- **Keys on hardware**: `meshnode identity hardware` keeps your key on a security token, reached
  through an ssh-agent, instead of in a file.

**Operating it**

- **A support bundle**: `meshnode support` collects one redacted file that answers the first hour
  of a support question, even when the node is not running.
- **Upgrades that roll themselves back**: a new binary that does not stay healthy is replaced by the
  previous one, by the node itself.
- **Disk**: `meshnode disk` shows free space on every node, and `meshnode content` what a node's
  content store holds. A node whose disk fills refuses new work until it has room.
- **Advertised addresses**: `[network] advertise_interfaces` picks the network the mesh uses on a
  machine with several.
- **`meshnode up --detach`** runs the node in the background as your systemd user service —
  installed on first use, started at every login — and returns once it answers.
- **Many new metrics**, and the old ones corrected. [Every metric](docs/reference/metrics.md) is
  documented.
- **Packaged nodes read `/etc/meshware/meshnode.toml`**, beside every other package's configuration.
- **Start the mesh over**: `meshnode reset` ends the mesh. It is for the founder only, run at a
  terminal, and asks you to type a confirmation. Every machine stops its work, moves the mesh's
  data aside (nothing is deleted) and waits to be adopted into a new mesh, with its install,
  service and keys kept. A machine that missed the reset is adopted with `--force`.
  `meshnode up --new-mesh`, or Director's **Create a new mesh**, starts the new mesh.
  [Start the mesh over](docs/guides/machines.md#start-the-mesh-over). Needs protocol 1.36 on
  every node.

**Director**

- A **Get started** card that walks through the first run.
- **Load balancing**, **Placement** and machine-by-machine views of what runs where; draining a
  machine from its panel.
- The **Fleet** table works with hundreds of machines, and marks quarantined ones.
- **About & licence**, a help menu, and a way forward when Director cannot start.
- An accessibility pass, with a check that keeps it.
- **Security keys, without a terminal.** *Settings › This machine › Security key* shows where your
  key is and what your agent holds, and moves you onto a YubiKey: it says what that takes before
  doing it — a second admin for the founder, a key rotation for any other admin — and does it on
  your confirmation. It publishes and withdraws the proof the device made. *Access* marks people
  whose key is proven to be on a security key, and *Access › Machines › Security key makers* is
  where an admin chooses whose proofs the mesh believes. Files beside your key file that may hold
  a private key are listed, so they can be moved off the machine. Setting up is a few plain steps:
  put a key on the YubiKey, **Connect** it with its PIN, check it signs, and switch — with **Go
  back to your previous key** to undo it. *Mesh › Updates › Release keys*
  shows which releases a security key has co-signed and attaches the co-signature to one that
  lacks it, and warns before trusting the first security-key release key that every release will
  need one.

**Documentation and the SDK**

- A [quickstart](docs/quickstart.md), a [Director guide](docs/director.md), a
  [deployments guide](docs/deployments.md), and a [reference](docs/reference/README.md) for every
  setting, variable, metric and file format, which CI keeps current.
- Task-first guides: [what Meshware is](docs/introduction.md),
  [running work](docs/guides/running-work.md), [machines](docs/guides/machines.md),
  [people and access](docs/guides/people-and-access.md) and [secrets](docs/guides/secrets.md).
- **An HTML documentation site**, built from the same Markdown: a sidebar, search, "on this page",
  light and dark themes, and pages that open straight from disk. Each release ships it as a site
  and as a single file.
- Pages on [compatibility](docs/operating/compatibility.md),
  [platforms](docs/operating/platforms.md) and [removing Meshware](docs/operating/uninstall.md).
- The **Python SDK** is published as a wheel with each release, and finds the node the way
  `meshnode` does.

### Changed

- **A dead machine is noticed in seconds**, not about 50.
- **A job ends when its main process does**, even if it left children running.
- **A node reads its memory limit from its cgroup**, so it plans for what it may really use.
- **The node uses a faster memory allocator** (mimalloc).
- **A job that cannot reach its first node goes elsewhere** instead of failing.
- **Mixed-version meshes**: a request an older node cannot read is refused before it is sent, and
  the work goes elsewhere. A command line newer than its daemon says so, with both versions,
  instead of reporting a malformed request.
- **Director's navigation** was reorganised: Machines, Work and System, with Settings as panes.
- Commands that need `MESHWARE_LEGACY=1` are marked in `meshnode --help`.
- Help puts the options every command takes under **Global options**, every option has a line of
  help, and `--output` has the short form `-o`.
- `meshnode services` is no longer listed beside `meshnode service`, a different command. It
  still works.
- **`meshnode run` is laid out for a terminal.** In a fan-out, each machine's output is printed as
  one block when that machine finishes, and a board at the bottom of the terminal shows the
  machines still running, with each one's latest line. When the run ends, the board becomes a
  table. Outcomes are coloured: green when it worked, red when your command failed, yellow when
  the mesh got no answer. The summary names the machines ("failed on NIM") instead of counting
  them. Output sent to a pipe or a file is unchanged, byte for byte.
- **Colour across the command line.** Tables dim their headers, ids and times, show each row's
  name in bold, and colour states by what they mean: green when something works, red when it
  failed or was revoked, yellow when it needs a look. Reports dim their labels so the values
  read, commands to type are cyan, `meshnode doctor` shows passing checks quietly and problems
  loudly, errors start with a red `error:`, and `--help` is coloured. Output sent to a pipe or a
  file, and output under `NO_COLOR`, is unchanged.

### Removed

- **The workflow engine**, workflow schedules and the drafting assistant (see
  [What might stop working](#what-might-stop-working)).
- **Change control**: policies, approvals, `--break-glass`, and touch-to-approve (see
  [What might stop working](#what-might-stop-working)). Director no longer links a USB HID
  library, which was the one place the build needed a C compiler for anything but the webview.
- **The host inventory**, its reachability probes, and the scheduler's vantage-point term (see
  [What might stop working](#what-might-stop-working)).
- **Director for macOS and Windows.**
- **The mesh proxy**, which appeared in development builds after 0.7.2 and never in a release. A
  copy is reached at its machine's address and port. Put a load balancer in front for a real
  domain. `[network] proxy` and `proxy_prefix` are still accepted, with a warning.

### Security

A full security review of the node, Director, the packages, the installer and the SDK. Every
finding is fixed:

- A job that acts for its user must prove the user asked (protocol 1.30).
- The signature on a job covers its environment and timeout, and loader variables are refused.
- A member can no longer read or stop work it did not place (protocol 1.32).
- A quarantined machine gets no new work and no new secrets.
- Secret scopes limit where a secret goes and who may use it (protocol 1.34).
- Invites can be revoked, and a planted secret copy can no longer lock a machine out.
- Result-cache entries are accepted only from the machine that did the work (protocol 1.33).
- The HTTP API binds each request to one node, limits requests per address, and no longer stalls
  under streaming (protocol 1.31).
- Strangers on the mesh port get no place in the member list, and their cost is bounded.
- The audit log is never dropped, flooded or skipped, and an operation's timestamp must be
  believable.
- A terminal opens only for a member, on the machine it names, sandboxed like a job, and closes
  when you close it.
- Director's window never hands the rest of the app a file path it could misuse.
- Git sources are fetched the way a job runs, over the transports the node allows.
- Every parser that reads untrusted input is fuzzed.
- `rustls` updated past RUSTSEC-2026-0285.
- **With a security key unplugged, the CLI signed as somebody else.** A key file pointing at a
  token whose agent could not produce the key fell through to the next key the account could open,
  or a freshly made one. It now stops and says to plug the token in.
- **`meshnode identity hardware use` could delete the only admin key.** A key made on a device is
  a new identity, and `use` wrote over the key file before the mesh knew the new one, which could
  lock a founder out of their own mesh. It now refuses a key the mesh does not know, and prints the
  step that lets it in: `users rotate` for most people, and a second admin for the founder, whose
  key can never be rotated. A key the mesh still honours is moved aside, never overwritten.

### Fixed

- **Containers never started on a machine added with `adopt`, Director or the package.** Every one
  failed inside podman, first with `open /proc/sys/net/ipv4/ping_group_range: Read-only file
  system` and then with `mount proc to proc: Operation not permitted`. The node's service puts
  read-only mounts over parts of `/proc`, and a rootless container cannot be made from inside it.
  Containers are now made by the `meshnode` account's own podman service, podman's
  `podman.socket`, which the installer sets up. The node drives it with `podman --remote`. The
  node's service also keeps protection it used to give up for containers: it no longer needs
  `NoNewPrivileges=false` or any capabilities, and the drop-in that granted them is removed.
  [Runtimes](docs/operating/runtimes.md#and-a-podman-service-of-its-own) has the details.
- **A copy that kept failing to start looked like one waiting for a machine.** Director said
  "Waiting for a machine" and `meshnode deployment status` said `absent`, but the copy was being
  started, failing, and retried with a longer wait each time. Now it shows as failing on its
  machine, with how it ended, how often, and when it is tried again, and Director offers its logs.
  A container engine's own exit statuses (125–127) are named.
- **A copy that had failed 64 times was never tried again**, and its status showed it as never
  placed. It is now retried however many times it has failed.
- **Moving onto a security key in Director could end in a flood of password prompts.** With a key
  behind an agent that asked for a passphrase or PIN per signature, every one of Director's
  refreshes raised a prompt, and cancelling only brought the next. Director now asks for one
  signature at a time, and the first refusal stops the asking until you choose **Try again**. A key
  that is not plugged in when Director starts gets a screen that says so, with a PIN box to connect
  it, instead of an error.
- **Director called any key an agent held "a security key", and offered key files.** GNOME's agent
  lists every key file in `~/.ssh`, and one could be chosen as "your YubiKey". Keys that are files
  in `~/.ssh` are now named as files and not offered; `meshnode identity hardware ls` marks them and
  `use` refuses one without `--force`. If you moved onto one, Director says so and offers to go
  back to your previous key, removing the account the move added.
- **The YubiKey instructions pointed at PIV slot 9c**, whose default is a PIN for every signature,
  and at `yubikey-agent`, which makes keys meshware cannot use. They now make an Ed25519 key in slot
  9a with the PIN asked once per session, and the attestation example's `ykman` syntax is fixed.
  `identity hardware attest --slot` defaults to `9a`.
- **Director's Key protection panel called a key on a security key "a plaintext 32-byte seed"**,
  and offered to encrypt it. It now says the key is on the security key and offers nothing.
- **A fan-out listed the same job id for several machines.** The short ids it printed were the
  time each job was made, and a fan-out makes its jobs together. The list is gone; the fan-out id
  finds them all.
- **A `--race` whose copies all finished called every one of them the winner.** Only the first to
  finish is.
- **`meshnode up` on a computer whose mesh Director had been running founded a second mesh.** It
  now runs that mesh, and a node that is not running says which mesh the computer is in and how to
  start it, instead of suggesting a command that would found a new one.
- **Restoring a backup over a home that already held a user key replaced it.** The key already
  there is now set aside, and a passphrase-protected key holding the same identity is kept.
- **A container deployment's environment, secrets and `MESHWARE_*` variables now reach the
  container.** Before, the container got none of them.
- **A setting that no longer does anything was never reported.** `meshnode up` resolved its
  config before it had a logger, so the warning for `[network] proxy` went nowhere. Retired
  settings are now logged once the node is up.
- **`meshnode parallel` and `map` could panic** in a debug build on an argument conflict the
  CLI's own test did not check; the test now checks every command as the one being run.
- **A port label with a `-`** (`admin-api`) gave a variable no shell could read, and a container
  got no port at all. It is `MESHWARE_PORT_ADMIN_API`.
- A restored node catches up with the mesh before it writes anything.
- Backups of passphrase-protected and hardware-backed admin keys restore.
- A co-signed fleet rollout no longer fails before it checks anything.
- The Landlock sandbox keeps working after the binary is replaced in place.
- A crash no longer leaks jobs, state or processes; jobs whose runner is gone are settled.
- The replicated log stays bounded on every node, and full-log scans are off the hot path.
- A deployment removed after log compaction no longer freezes the log.
- A registered service no longer outlives the work behind it.
- A deployment or schedule file in TOML may start with comments.
- `sinks add` takes `--database` (the ClickHouse database, or the OpenSearch index prefix), as
  Director always could. A sink with no database gets tables and rows in the same place: `sinks
  ddl` printed `meshware.jobs` while the rows went to the default database.
- `sinks add` refuses `--plugin` on a warehouse and `--database` on a plugin, instead of
  quietly misreading them.
- `doctor` found no archive on packaged nodes, whose archive timer wrote somewhere else.
- In Director: a cancelled save no longer destroys the file it would have replaced; you can stop
  waiting to pair; a remembered login works without retyping; and many screens show errors
  instead of empty pages.

---

## 0.7.2 — 2026-09-08

The first tagged release, speaking protocol 1.19. This changelog starts here.
