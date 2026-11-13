# Installing a node

*Reference. Describes what is in the tree.*

One static binary, no runtime dependencies, no database, no control plane to stand up first.
Five ways to get it onto a machine, and what each leaves behind. New to Meshware? The
[quickstart](../quickstart.md) is the shortest path from nothing to a working mesh.

**Supported:** Linux, `x86_64` and `aarch64`, static musl — so glibc version does not matter.
Nodes are Linux-only by design, and so is Director. See [limits.md](limits.md).

---

## 1. The five ways

### The tarball — any machine, and the one to start with

```console
$ gh attestation verify meshnode-v1.1.0-x86_64-unknown-linux-musl.tar.gz --repo neuralpunk/meshware
$ tar xzf meshnode-v1.1.0-x86_64-unknown-linux-musl.tar.gz
$ sudo install -m755 meshnode /usr/local/bin/
```

The tarball carries the binary, the systemd unit (`meshnode.service`), the man page and the
licences. Verify it first: the build-provenance attestation proves the file was built from this
repository by its release workflow, which a checksum served beside it cannot — see
[security.md](security.md#verifying-a-release). This is the path the
[quickstart](../quickstart.md) follows, and the one to use air-gapped or from your own pipeline.

### `.deb` / `.rpm` — a fleet you already manage

```console
$ sudo dpkg -i meshnode_1.1.0_x86_64.deb    # or: sudo rpm -i meshnode-1.1.0.x86_64.rpm
$ sudo systemctl enable --now meshnode
```

Installs to `/usr/bin`, creates the `meshnode` system user, and drops the hardened unit. **This
is the right choice if you have configuration management**, because upgrades then go through
apt or dnf like everything else — and `meshnode upgrade --self` deliberately refuses to replace
a package-manager-owned binary, with a message naming the tool to use instead.

### `meshnode adopt` — machines you already have SSH to

```console
$ meshnode adopt ops@rack-{1..10}
```

Installs the binary, writes the systemd unit with a service account, opens the mesh port in the
firewall, pairs each machine (showing you the confirmation codes), and starts the service.
**After this, SSH is never needed again** — everything else goes through the mesh.

Director does the same thing with no terminal: *Mesh › Fleet › Add machines*.

### `install.sh` — one machine, one command

```console
$ curl -fsSL https://raw.githubusercontent.com/neuralpunk/meshware/main/packaging/install.sh | sh
```

`packaging/install.sh` detects the architecture, downloads the tarball, and installs the binary to
`/usr/local/bin` (or `~/.local/bin` without root). It does not start anything or write any config.
`MESHWARE_VERSION` pins a release.

**Be exact about what it verifies.** It always checks the download against `SHA256SUMS`, which
catches a corrupt download and nothing else, because the checksum comes from the same server as
the file. When the [GitHub CLI](https://cli.github.com) is installed and logged in, it also checks
the build-provenance attestation (`gh attestation verify`), which proves the file was built from
this repository by its release workflow; when it cannot, it says so in capitals. Set
`MESHWARE_REQUIRE_SIGNATURE=1` to make a missing provenance check a hard failure.
`MESHWARE_NO_VERIFY=1` skips even the checksum; do not use it.

### From source — `make`

```console
$ git clone https://github.com/neuralpunk/meshware && cd meshware
$ make doctor                     # checks Rust, a C compiler, and what the optional builds need
$ make                            # meshnode and meshware-archive, optimised
$ sudo make install               # binaries, man pages, bash/zsh/fish completions
$ sudo make install-service       # optional: the systemd unit and the meshnode account
```

Needs Linux, Rust (the `rust-version` in `Cargo.toml` or newer) and a C compiler. `make STATIC=1`
builds the static musl binary the releases ship, and needs `musl-gcc`. `make install-service`
installs the unit and **never starts it**: the first start of a node with no mesh founds a new
one, so a machine meant to join an existing mesh should join first. `make help` lists every
target, including `make demo` — a real mesh of three nodes on this one machine, on loopback only,
gone when you stop it.

## 2. Founding a mesh, or joining one

```console
$ meshnode up                    # first run: identity, genesis, an encrypted listener
```

That is a one-node mesh and it is already a working product. To grow it:

```console
# on the new machine and on any member, at the same time:
$ meshnode pair
# both screens show the same six digits → confirm → joined
```

Across a network, or unattended:

```console
$ meshnode invite create --ttl 24h --uses 50      # on a member
$ meshnode join mwj1…                            # on the newcomer, then `meshnode up`
```

**The invite token is a bearer credential for the duration of its TTL.** Mint it narrow —
`--uses 1` for one machine — and treat a long-lived multi-use token the way you would treat a
join token for anything else. A token that went somewhere it should not have can be taken back:

```console
$ meshnode invite ls                               # what is still open, and how much of it is used
$ meshnode invite revoke inv-7k2…                  # every node refuses it from then on
```

Director's **Invite someone** panel lists the same invites, each with a Revoke button. Revoking
an invite does not remove the machines that already joined with it — `meshnode nodes revoke` does
that. A node only admits a token for an invite the mesh has a record of; one that has not heard
of it yet asks its peers before answering.

## 3. Running it as a service

The packaged unit runs the node as an unprivileged `meshnode` system user, in `/var/lib/meshware`.
Root is never required.

```console
$ sudo systemctl enable --now meshnode
$ systemctl reload meshnode          # SIGHUP: apply the edited config — see below
```

**Start it only when you mean it.** The first start of a node with no mesh *founds a new one*. On a
machine that should join an existing mesh, join first and start second:

```console
$ sudo -u meshnode meshnode join mwj1…       # or: sudo -u meshnode meshnode pair
$ sudo systemctl enable --now meshnode
```

`meshnode adopt` and Director's *Add machines* do this in the right order for you.

### From the tarball

The `.deb` and `.rpm` create the service account and its directories. From the tarball, do the
same by hand, once:

```console
$ sudo useradd --system --home-dir /var/lib/meshware --shell /usr/sbin/nologin meshnode
$ sudo install -d -o meshnode -g meshnode -m 0750 /var/lib/meshware
$ sudo install -d /etc/meshware
$ sudo install -m755 meshnode /usr/local/bin/
$ sudo install -m644 meshnode.service /etc/systemd/system/
$ sudo mkdir -p /etc/systemd/system/meshnode.service.d
$ printf '[Service]\nExecStart=\nExecStart=/usr/local/bin/meshnode up\n' | \
      sudo tee /etc/systemd/system/meshnode.service.d/path.conf
$ sudo systemctl daemon-reload
```

The unit names `/usr/bin/meshnode`, where the packages put it; the drop-in points it at
`/usr/local/bin`. To run containers under the service, the account also needs a sub-id range —
see [runtimes.md](runtimes.md#rootless-podman-needs-a-sub-id-range).

**What a reload applies.** Log level, `tags`, `[limits]` (slots, queue, memory floor), quiet
hours, `[retention]`, `[disk]`, `[wan]`, `[sessions]` (including whether terminals are accepted
at all), secret providers, `[network]` apart from the DNS listener, `[runtime] sandbox` and
`container_plugin`, and `[compute] placement`. Each reload is compared with what the node is
running, not with what it booted with, so changing a setting back applies too. Anything else —
the listen, HTTP and DNS addresses, `node_name`, `data_dir`, log destination, the rest of
`[compute]` — needs a restart, and every reload says so in the journal until it gets one.

`Type=notify`, so `systemctl start` returns when the node is actually up rather than when the
process spawned. A start that is still replaying or repairing a large op-log asks systemd for
more time, and only while that work is moving.

The unit restarts the node after **any** exit — a crash, an OOM kill, a watchdog abort, or the
clean exit that ends a drain or an upgrade — up to ten times in five minutes, after which it
stays `failed` for you to look at. Two exits are never retried, because a restart cannot fix
them: status 78 (the configuration does not load or check) and status 65 (the data directory
was written by a newer release). `journalctl -u meshnode` says which.

The watchdog (`WatchdogSec=90`) is fed only while the node's critical loops — gossip, the
scheduler, the local IPC server, wire dispatch and the QUIC accept loop — are alive and making
progress. A loop that has *ended* stops the node at once with a failure (`the node stopped
working: …` in the journal), because it will not come back on its own; one that has *stalled*
withholds the heartbeat, and systemd restarts the node when `WatchdogSec` runs out. Either way
`GET /healthz` answers `503 Service Unavailable` with `"status": "unhealthy"` and the reason in
`"problems"`, so a load balancer or an orchestrator without systemd sees it too.

**Where things live:**

| | |
|---|---|
| Binary | `/usr/bin/meshnode` (package) or `/usr/local/bin/meshnode` (tarball) |
| Data | `/var/lib/meshware` — identity, state, jobs, content |
| Config | `/etc/meshware/meshnode.toml` — optional; a node with none runs on defaults |
| Logs | journald, or a rolling file under the data directory |
| Socket | `/var/lib/meshware/run/meshnode.sock` — how the CLI reaches the daemon |

One directory holds everything a node is. Backing it up is [backup.md](backup.md).

**The config lives in `/etc`** so configuration management finds it beside every other package's,
and it can be edited without becoming the service account. A node installed before this looked in
`/etc/meshware/meshware/meshnode.toml`, and still reads that file when `/etc/meshware/meshnode.toml`
does not exist; `meshnode doctor` names it until it is moved. `meshnode config paths` prints every
path a node uses, and whether it exists.

## 4. Fleet self-upgrade needs one opt-in

`meshnode upgrade --on <ring>` lets a ring replace its own binaries. That needs the daemon to
be able to write its own binary, which the shipped unit deliberately forbids — a daemon that
can rewrite its own binary is a daemon whose compromise is permanent.

Granting it is one drop-in, `packaging/systemd/meshnode-fleet-upgrade.conf`, with the reasoning
written beside it. Package installs do not need it and must not use it.

## 5. Verifying it worked

On a node you started yourself with `meshnode up`:

```console
$ meshnode status
$ meshnode doctor
$ meshnode run 'uname -a' --on any
```

On a server running the packaged service, run them as the service account — see §6 for why:

```console
$ sudo -u meshnode meshnode doctor
$ sudo -u meshnode meshnode status
$ journalctl -u meshnode -n 50
```

If `doctor` is quiet and a job runs, the node is working. If it is not, paste `doctor` output
into your support request — see [troubleshooting.md](troubleshooting.md).

## 6. Using the CLI on a server

A packaged node runs as the `meshnode` account, and its socket is in `/var/lib/meshware`, which only
that account can enter. So as yourself on the server, `meshnode status` finds the node and cannot
talk to it:

```console
$ meshnode status
error: you are not authorized to use the meshware node on this machine.

A node is listening at /var/lib/meshware/run/meshnode.sock, but it runs under another account
(uid 993) and will not accept a connection from yours.
…
```

There are two ways to drive a server node, and they suit different jobs.

### On the server: `sudo -u meshnode`

Running the CLI as the service account reaches the daemon with no other flags: that account's home
*is* `/var/lib/meshware`, so the CLI finds the socket, the config in `/etc/meshware/meshnode.toml`, and
the node's own key.

```console
$ sudo -u meshnode meshnode doctor           # this machine, end to end
$ sudo -u meshnode meshnode config show      # the configuration it is actually running
$ sudo -u meshnode meshnode status
```

`doctor` and `config` read the machine and need nothing else. Anything that asks the *mesh* —
`status`, `nodes`, `run`, `jobs` — is signed with the node's own key, and that key has only the rights
the mesh gave it:

- **On the machine that founded the mesh**, the node's key is the founding administrator, so
  every command works. This is how you administer a mesh that only exists on servers: found it
  with `systemctl enable --now meshnode` on the first one, then let your own key in from there
  (`sudo -u meshnode meshnode users approve …`).
- **On any other server**, the node's key is a machine, not a person, and the mesh refuses its
  requests with `unknown user`. That is deliberate: root on one server should not be an operator
  of the whole mesh by default. If you do want that machine's key to act as an operator, enrol it
  like a person — `sudo -u meshnode meshnode enrol --as web-07`, and an admin approves it with a
  role — knowing that anyone with `sudo` on that machine then holds that role.

Prefer `sudo -u meshnode` to plain `sudo`. As root, the CLI would look in `/root/.meshware`, and a
root-owned file left in `/var/lib/meshware` can stop the service writing there later.

### From your workstation — the usual way

Your workstation runs a node of its own (`meshnode up` as you) or Director, and **your** key is
enrolled with a role. Then almost nothing needs you to be on the server at all, because every node
answers for the whole mesh:

```console
$ meshnode nodes                                       # every machine, including the server
$ meshnode run 'df -h /' --on name=web-07              # run something there
$ meshnode facts web-07                                # what it reports about itself
$ meshnode nodes drain web-07                          # move its work off
$ meshnode shell web-07 --reason "INC-4412 disk full"  # a terminal on it, audited
```

`meshnode shell` needs the server to accept terminals (`[sessions] accept = "on"` in its config)
and a session policy naming you — see [identity.md §5](identity.md#5-interactive-access). Only the
things that are about *that machine's own files* need you on it: its `doctor`, its config, its
journal, and `backup` / `restore`.

## 7. Removing it

Taking a machine out of the mesh, and everything each of the four ways leaves behind:
[uninstall.md](uninstall.md).
