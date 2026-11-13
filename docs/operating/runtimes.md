# Runtimes — what a machine can run, and how meshware knows

A **runtime** is the kind of thing a job *is*: a shell script, a Python
entry point, an Ansible playbook, a compiled binary, a container image.

Every node probes itself for all five when it starts, announces what it
found, and the scheduler only sends work to machines that can run it.

**There is nothing to configure.** Install a tool and meshware finds it.

```sh
meshnode runtimes
```

```
node:      web-3

RUNTIME      STATE      FOUND
exec         ready      the kernel
bash         ready      GNU bash, version 5.2.32
python       ready      Python 3.13.1
ansible      absent     —             install ansible (`dnf install ansible`…)
container    ready      podman 5.8.4 (rootless)
```

In Director, the same table is under **Mesh › Capacity**, as Runtimes.

---

## The five runtimes

### `exec` — a binary from a bundle

| | |
|---|---|
| **What it runs** | An executable file in a pushed bundle: `./server`, `./migrate`. |
| **Needs** | Nothing but the kernel. |
| **Detection** | Always ready. |
| **Isolation** | Landlock filesystem rules and this node's cgroup both apply. |
| **Give up** | You ship the binary. No dependency resolution, no base image. |

`exec` is the only runtime that can never be missing, and that is
load-bearing rather than a convenience — see [*What an old node
means*](#what-an-old-node-means).

### `bash` — a shell script

| | |
|---|---|
| **What it runs** | `run.sh` from a bundle, or an inline `meshnode run 'uname -a'`. |
| **Needs** | `bash` on `PATH`. |
| **Detection** | `bash --version`. |
| **Isolation** | Landlock and cgroups apply. |
| **Give up** | Whatever the script assumes is installed must be installed. |

Absent `bash` is rarer than it sounds, and when it happens it is usually a
minimal container base image running `meshnode` itself.

### `python` — a Python entry point

| | |
|---|---|
| **What it runs** | `main.py` from a bundle, in a virtualenv built from the bundle's `requirements.txt`. |
| **Needs** | `python3` on `PATH`. `pip` for bundles that pin requirements. |
| **Detection** | `python3 --version`. |
| **Isolation** | Landlock and cgroups apply. |
| **Give up** | The venv is built on first use and cached by the *content hash of the requirements*, so two services pinning the same packages share one environment. First run of a new pin set is slower. |

### `ansible` — a playbook

| | |
|---|---|
| **What it runs** | `site.yml` against an inventory rendered from mesh members. |
| **Needs** | `ansible-playbook` on `PATH`, **or** a bundle whose `requirements.txt` pins ansible. |
| **Detection** | `ansible-playbook --version`. |
| **Isolation** | Landlock and cgroups apply to the playbook process. What it does over SSH is outside them, by definition. |
| **Give up** | Ansible's own dependency story is yours to manage. |

Reported `absent` is not a blocker for a bundle that pins its own ansible:
that bundle builds a venv the way a Python service does. The probe reports
the *system* tool, which is what an ad-hoc playbook uses.

### `container` — an OCI image

| | |
|---|---|
| **What it runs** | `docker.io/library/nginx:alpine`, `postgres:16`, your own build. |
| **Needs** | **podman or docker installed.** Nothing else. |
| **Detection** | `podman info`, then `docker version`. podman is preferred. |
| **Isolation** | **Not** Landlock or this node's cgroup — see below. The engine's own controls instead: `--memory`, `--cpus`, `--read-only`, `--cap-drop=ALL` plus five, `--security-opt=no-new-privileges`. |
| **Give up** | The honest limit below. |

```sh
# The whole of it, on each machine that should run containers:
dnf install podman      # or: apt install podman, or docker
systemctl restart meshnode
```

### Rootless podman needs a sub-id range

If meshnode runs as a **system user** — which it does on a package install
or a remote deploy — that user needs an `/etc/subuid` and `/etc/subgid`
range before rootless podman can unpack an image. `useradd --system`
deliberately allocates none.

Meshware's installer and `meshnode adopt` do this for you, **and they do
it for an account that already exists** — the block sits outside the
"create the user" guard, so re-running an install against a node deployed
before this existed repairs it. If the range moves they also run
`podman system migrate`, because storage built under the old empty mapping
is not fixed retroactively by adding a range.

So the supported repair for an existing node is: **deploy to it again.**
By hand, if you would rather:

```sh
sudo usermod --add-subuids 2000000-2065535 --add-subgids 2000000-2065535 meshnode
sudo loginctl enable-linger meshnode
sudo su -s /bin/sh -l -c 'podman system migrate' meshnode
sudo systemctl restart meshnode
```

**`su -l`, not `sudo -u meshnode podman …`.** The plain form keeps your
directory and your `HOME`:

```
cannot chdir to /home/you: Permission denied
Error: setting up the process
```

podman enters the working directory while setting up the user namespace and
a service account cannot enter your home; `HOME` is the quieter half, since
left unset it looks for the container storage under *your* home. `-l` gives
a login environment, which fixes both. `-s /bin/sh` is needed because the
account's shell is `nologin`.

### …and a podman service of its own

**A rootless container cannot be made from inside `meshnode.service`.** The
unit hardens the daemon with `ProtectKernelTunables=` and
`ProtectKernelLogs=`, which put read-only mounts over parts of `/proc`. A
container needs a `/proc` of its own, and the kernel refuses to mount one
inside a user namespace while any of `/proc` is covered. So podman run
from inside the unit fails every container. First it fails on the sysctl
podman sets for a container with its own network, then on `/proc` itself:

```
open `/proc/sys/net/ipv4/ping_group_range`: Read-only file system
mount `proc` to `proc`: Operation not permitted
```

No setting inside the unit changes that, and lifting the two directives
would weaken the daemon that faces the network to suit a child process that
does not.

So on a system node, **containers are made by the `meshnode` account's own
podman service**. That is podman's stock `podman.socket`, in the account's
systemd user manager, with its socket moved to
`/var/lib/meshware/run/podman.sock`, where the unit can reach it (the unit
hides `/run/user`). The node runs `podman --remote` against it. podman
there runs the way it is designed to run rootless, and Docker has always
worked like this: the daemon makes the container, not the client.

The unit keeps all of its hardening, and gets some back. The node no longer
runs `newuidmap` itself, so the unit no longer needs `NoNewPrivileges=false`
or any capabilities in its bounding set. Earlier installs added a drop-in
that granted those, and every install and upgrade now removes it.

**It is set up for you:** by `meshnode adopt`, by **Add machines** in
Director, by the `.deb` package, and by **Update over SSH** in Director
(*Mesh › Updates*). Use Update over SSH for a machine installed before
this existed, with **Reinstall** if it is already on the current version.
The installer:

- turns on lingering for the account, so its user manager runs at boot;
- writes the socket drop-in, and enables and starts `podman.socket` in that
  manager;
- gives that one manager the cgroup controllers podman needs to honour
  `--cpus` (`/etc/systemd/system/user@<uid>.service.d/`). This is podman's
  own documented setting for rootless cpu limits.

`meshnode runtimes` then says `podman 5.8.7 (rootless, via podman.socket)`.
A node that can only run podman from inside its unit and has no service
reports `container broken`, says why, and prints the commands. By hand, as
root:

```sh
loginctl enable-linger meshnode
mkdir -p /var/lib/meshware/.config/systemd/user/podman.socket.d
printf '[Socket]\nListenStream=\nListenStream=/var/lib/meshware/run/podman.sock\nSocketMode=0600\n' \
  > /var/lib/meshware/.config/systemd/user/podman.socket.d/50-meshware.conf
chown -R meshnode: /var/lib/meshware/.config
systemctl --user -M meshnode@ daemon-reload
systemctl --user -M meshnode@ enable --now podman.socket
systemctl restart meshnode
```

A node run as a person, under `systemctl --user` or on a machine without
systemd, runs podman directly, as it always has. Nothing there covers
`/proc`.

### On Ubuntu 24.04 and later: an AppArmor profile

Ubuntu defaults `kernel.apparmor_restrict_unprivileged_userns=1`, which
gates unprivileged user namespaces behind an AppArmor profile. A process
that creates one without a permitting profile is transitioned into the
restricted `unprivileged_userns` profile, and `newuidmap` is then denied the
`uid_map` it has to write:

```
newuidmap: open of uid_map failed: Permission denied
Error: cannot set up namespace using "/usr/bin/newuidmap": exit status 1
```

**This survives fixing everything else.** The range is allocated, the
helper is marked, the unit permits it — and containers still do not run.
Fedora and the RHEL-likes use SELinux and have no equivalent, so in a mixed
fleet it is the Ubuntu box and only the Ubuntu box.

**Confirm it before acting on it.** That error message is not proof: the
next section describes a second cause of the same line, and the two have
opposite fixes. AppArmor *audits* every refusal, so there is a definitive
test — a denial leaves a record, and this cause leaves nothing else:

```sh
sysctl kernel.apparmor_restrict_unprivileged_userns    # 1 on Ubuntu 24.04+
sudo journalctl -k --since -10min | grep -i 'apparmor="DENIED"'
```

A `DENIED` line naming podman, `newuidmap` or `unprivileged_userns` means it
is this one. **No line at all means it is not**, however plausible the
sysctl looks — read the next section instead.

Give the engine a profile that permits it:

```sh
sudo tee /etc/apparmor.d/podman >/dev/null <<'EOF'
abi <abi/4.0>,
include <tunables/global>

profile podman /usr/bin/podman flags=(unconfined) {
  userns,
  include if exists <local/podman>
}
EOF
sudo apparmor_parser -r /etc/apparmor.d/podman
sudo systemctl restart meshnode
```

The blunt alternative is
`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, which turns
the protection off for the whole machine rather than allowing one program.
Prefer the profile.

`meshnode runtimes` attaches this explanation to the failure when the
sysctl is on — **as an explanation, never a refusal.** The sysctl being on
is not a fault: a machine with a profile installed has it on and works
perfectly, so a node is never declared broken on the strength of it.

### …and a helper that can apply it

A range is necessary and **not sufficient**. Applying it is done by
`newuidmap` and `newgidmap`, which need privilege of their own to write
another process's id map — and **distributions grant it two different
ways**:

| Distribution | Marking | What `ls -l` shows |
| --- | --- | --- |
| Fedora, RHEL, Rocky | `cap_setuid` file capability | `-rwxr-xr-x` |
| Debian, Ubuntu | setuid-root | `-rwsr-xr-x` |

Both work. Check which one this machine uses before touching either:

```sh
ls -l /usr/bin/newuidmap /usr/bin/newgidmap   # is there an `s`?
getcap  /usr/bin/newuidmap /usr/bin/newgidmap # is there a capability?
```

**Exactly one of those should answer.** If neither does, the helper has no
privilege at all and every `podman run` fails on a line naming the helper
rather than the reason:

```
running `/usr/bin/newuidmap 171547 0 961 1 1 2000000 65536`
newuidmap: write to uid_map failed: Operation not permitted
```

The safe repair is to reinstall the package that owns the file —
`shadow-utils` on Fedora/RHEL, `uidmap` on Debian/Ubuntu — which restores
whichever marking that distribution intends. By hand it is
`setcap cap_setuid+ep /usr/bin/newuidmap` on Fedora/RHEL, or
`chmod u+s /usr/bin/newuidmap` on Debian/Ubuntu.

> **Never both.** `capabilities(7)` is explicit: a setuid-root binary that
> *also* carries effective file capabilities is **not** given the root
> capability set. The kernel says so once per boot —
> `warning: '/usr/bin/newuidmap' has both setuid-root and effective
> capabilities. Therefore not raising all capabilities.` The helper then
> runs as root with `cap_setuid` and nothing else, so it has no
> `CAP_DAC_OVERRIDE`, cannot open the caller's own mode-0644
> `/proc/<pid>/uid_map`, and fails one step earlier than the missing-marking
> case:
>
> ```
> newuidmap: open of uid_map failed: Permission denied
> ```
>
> That difference is the diagnosis. `write … Operation not permitted` is
> **EPERM** and means no privilege; `open … Permission denied` is **EACCES**
> and means the wrong kind. Undo it on a Debian/Ubuntu machine, which
> returns the files to the plain setuid-root marking the distribution
> shipped — **one path per command**, because `setcap -r` takes a single
> file and reads a second argument as a capability name, answering
> `File 'setcap' has no capablity to remove` (its typo, not yours):
>
> ```sh
> sudo setcap -r /usr/bin/newuidmap
> sudo setcap -r /usr/bin/newgidmap
> ```
>
> This page used to tell you to run `setcap` whenever `getcap` printed
> nothing, without saying which distributions that was right for. On Ubuntu
> it is how you break a machine that was working.

`meshnode runtimes` checks all of this, and distinguishes the three states —
correctly marked, unmarked, marked both ways — because they have three
different fixes.

It reports it separately from the range, too: the numbers in the failing
command are the ones from `/etc/subuid`, so a helper that cannot apply the
range looks exactly like a working range right up until it doesn't.

### …and, run directly, a unit that allows it

**This applies only where meshnode runs podman itself inside a unit that
restricts it**: a unit of your own, with no podman service. The packaged
unit and every install meshware makes use the podman service above
instead. There `newuidmap` runs in the account's user manager, which
restricts none of this.

A correctly marked helper is a *request*, and the kernel grants it at
`execve`. Two directives decide whether it will:

```
NoNewPrivileges=true     honours no setuid bit and no file capability, ever
CapabilityBoundingSet=   a ceiling on what any exec may hold; empty stops all
```

A drop-in that relaxes exactly those two works, and **the bounding set it
grants depends on the marking**:

| Helper marking | What the helper becomes | What the bounding set must hold |
| --- | --- | --- |
| `cap_setuid` file capability | the calling user, plus `CAP_SETUID` | `CAP_SETUID`, `CAP_SETGID` |
| setuid-root | root, with whatever the ceiling allows | those two, **plus `CAP_DAC_OVERRIDE` and `CAP_SYS_ADMIN`** |

A setuid-root exec takes its permitted set *from the bounding set* — the
kernel computes `pP' = bset | pI` — so a narrow ceiling leaves the helper
root with only what the ceiling allows, and **euid 0 is not, by itself,
permission to do anything.** It needs two capabilities the
capability-marked helper does not, for two different reasons, and they fail
at two different steps:

**`CAP_DAC_OVERRIDE` — the open.** The `/proc/<pid>/uid_map` it must open is
mode 0644 owned by the *service* user, not by root. Under file permissions
the kernel checks the capability, not the uid, so root without it is
refused:

```
newuidmap: open of uid_map failed: Permission denied
```

**`CAP_SYS_ADMIN` — the write.** `map_write` requires `CAP_SYS_ADMIN` over
the namespace *being mapped*, and the kernel grants every capability in a
user namespace to **the euid that owns it**. A capability-marked helper
keeps the caller's euid, so it *is* the owner and gets this for free. A
setuid-root helper is euid 0, which is not the owner — the service user is —
so the shortcut does not apply and it must hold the capability outright:

```
newuidmap: write to uid_map failed: Operation not permitted
```

That asymmetry is the whole reason the two markings need different ceilings,
and it is why Debian ships these setuid-root rather than capability-marked
in the first place: the set they need is close enough to root that naming it
was never the simpler option.

The drop-in meshware used to install shipped with two capabilities, then
three, and broke every Debian-family node both times. Four is the right
number. A Fedora or RHEL machine may leave out `CAP_DAC_OVERRIDE` and
`CAP_SYS_ADMIN` and lose nothing.

> **If `CAP_SYS_ADMIN` is too much for your fleet** — a fair objection; it
> is the large one. The narrower option is to give a Debian-family machine's
> helpers the marking Fedora uses, after which two capabilities are enough:
>
> ```sh
> sudo chmod u-s /usr/bin/newuidmap /usr/bin/newgidmap
> sudo setcap cap_setuid+ep /usr/bin/newuidmap
> sudo setcap cap_setgid+ep /usr/bin/newgidmap
> # then CapabilityBoundingSet=CAP_SETUID CAP_SETGID is sufficient
> ```
>
> Remove the setuid bit in the same breath as adding the capability — both
> at once is the one combination the kernel refuses. And a `uidmap` package
> upgrade puts the setuid bit back and drops the capability; `meshnode
> runtimes` names that state when it happens, but it will happen.

`meshnode runtimes` reads `/proc/self/status` — the daemon's observed state,
not the unit file's stated intent — names the capability that is missing,
and prints a heredoc for *this* machine:

```sh
sudo mkdir -p /etc/systemd/system/meshnode.service.d
sudo tee /etc/systemd/system/meshnode.service.d/containers.conf >/dev/null <<'EOF'
[Service]
NoNewPrivileges=false
CapabilityBoundingSet=CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_SYS_ADMIN
EOF
sudo systemctl daemon-reload && sudo systemctl restart meshnode
```

Relaxing the unit does not relax the work: the sandbox shim sets
`PR_SET_NO_NEW_PRIVS` on every sandboxed job itself. The unit must also not
cover `/proc` (no `ProtectKernelTunables=` or `ProtectKernelLogs=`), or no
container starts whatever it grants. See the section above.

### Five causes, two messages

`newuidmap` reports failure in two ways, and between them they cover five
distinct faults. **Read which step failed, not the words:**

- `open of uid_map failed: Permission denied` — **EACCES**, at the open. The
  helper is root or the caller, and either way cannot open a file it does
  not own.
- `write to uid_map failed: Operation not permitted` — **EPERM**, at the
  write. The open succeeded; the kernel refused the mapping itself.

Check the causes in this order — definite before probable:

| # | Cause | Fails at | How to see it | Fix |
| --- | --- | --- | --- | --- |
| 1 | `NoNewPrivileges=true` | open | `grep NoNewPrivs /proc/<meshnode pid>/status` | the drop-in |
| 2 | Ceiling has no `CAP_DAC_OVERRIDE` | open | `grep CapBnd /proc/<pid>/status` | add it |
| 3 | Ceiling has no `CAP_SYS_ADMIN` | **write** | same, bit 21 (`0x200000`) | add it |
| 4 | Helper marked both ways | open | `ls -l` **and** `getcap` both answer | `setcap -r`, one path per command |
| 5 | AppArmor userns gate | open | a `DENIED` line in `journalctl -k` | a profile with `userns,` |

Causes 1–3 are properties of the *service*, which is why the same
`podman run` succeeds under `sudo su -s /bin/sh -l -c … meshnode` and fails
under systemd — a login shell has no bounding set. **That one-line test is
the fastest way to split "the unit" from "everything else"**, and worth
running before anything on this list:

```sh
sudo su -s /bin/sh -l -c 'podman run --rm docker.io/library/alpine true' meshnode
```

`meshnode runtimes` walks the list in that order and reports the first
definite cause it finds; the AppArmor hint appears **only** when the first
four come back clean, because it is the one thing here a node cannot verify
about itself.

**Why neither is obvious.** `podman info` answers perfectly well without a
range *and* without the capability — it reads `/etc/subuid` rather than
trying to use it. So detection used to report `container ready` and the node
would take container work and then die deep in the image unpack:

```
potentially insufficient UIDs or GIDs available in user namespace
(requested 0:42 for /etc/shadow) … lchown /etc/shadow: invalid argument
```

`/etc/shadow` in a stock image is owned by `0:42`, and with no delegated
range there is no gid 42 to map it to. `meshnode runtimes` now checks the
mapping and reports `container broken` with this fix, so the node stops
taking work it cannot do.

The `enable-linger` line is separate and milder: without a systemd user
session podman prints four warnings on every container start and falls back
to `--cgroup-manager=cgroupfs`, which still runs but puts the container's
accounting outside the tree meshnode delegates.

`meshnode runtimes` then says `container ready — podman 5.8.4 (rootless)`,
the node announces it, and container work starts arriving.

---

## The honest limit on container isolation

`podman run` hands the workload to `conmon`, which **reparents it out of
meshnode's cgroup and out of any Landlock ruleset applied to the client
process.** A container job therefore does not get the sandbox every other
runtime gets.

This is a property of container runtimes, not of how meshware drives them.
What a container has instead are the engine's own equivalents, and meshware
passes all of them:

| Control | Flag |
|---|---|
| Memory ceiling | `--memory <mb>m` |
| CPU ceiling | `--cpus <n>` |
| Read-only root filesystem | `--read-only`, with tmpfs for `/tmp /run /var/run /var/cache /var/log` |
| Capabilities dropped | `--cap-drop=ALL`, then `CHOWN SETUID SETGID DAC_OVERRIDE NET_BIND_SERVICE` added back |
| No privilege escalation | `--security-opt=no-new-privileges` |
| Rootless | podman's default |

`meshnode doctor` reports exactly this set on the `containers` line, and a
runtime that enforced less would say so there. Those five capabilities are
added back because `--cap-drop=ALL` alone makes stock images unrunnable:
`nginx:alpine` chowns its cache directory and drops to an unprivileged user
at startup, which is the image doing the *right* thing and needing `CHOWN`,
`SETUID` and `SETGID` to do it.

---

## The three states, and why there are three

| State | Means | What to do |
|---|---|---|
| `ready` | Found and working. | Nothing. |
| `absent` | Not on this machine at all. | Install it, if you want this machine to run that work. |
| `broken` | **Installed and not working.** A daemon that is down, a socket this user cannot open. | Fix it — and the row tells you what it said. |

`absent` and `broken` are separate on purpose. *"Docker is installed but its
daemon is stopped"* has a different fix from *"Docker is not installed"*,
and collapsing them is how somebody spends an afternoon installing what they
already have.

For the **scheduler** the two are the same: both drop the runtime from what
the node announces, so work stops arriving either way. The difference is for
the person reading `doctor` at 3am.

---

## How placement uses this

A node announces what it can run as a tag on its own record:

```
cap.rt=exec,bash,python,container
```

That tag is what the scheduler reads. A container job is never sent to a
node without an engine; a Python job is never sent to one without `python3`.
When nothing can take a job, the refusal says so by name:

```
NOTHING CAN TAKE IT RIGHT NOW:
  - selector tag=edge excluded 1 of 2 node
  - 1 node cannot run container work — install podman or docker there,
    or target nodes that have one
  - no node in this mesh can take it as written
```

**Detection is a filter, not a promise.** `podman info` answering does not
guarantee the next `podman run` succeeds. The probe removes nodes that
certainly cannot; a node that turns out to be unable anyway refuses at
admission and the job spills to another — the same split ports and volumes
already make, because live node-local state is not visible from the side
doing the placing.

### What an old node means

A node running a build older than **1.26** announces no `cap.rt` at all.
Meshware reads that as **unknown**, not as *none*, and an unknown node stays
a candidate for every kind of work.

This is the opposite of how an absent port range or volume list is read, and
the difference is deliberate. A node that announces no port range genuinely
cannot allocate one. A node that announces no runtimes might be an older
build with a `container_plugin` configured, running containers perfectly —
so excluding it would mean **upgrading one node silently shrinks the mesh's
container capacity**, with a deployment shedding replicas and nothing
anywhere saying why.

`meshnode runtimes` names every node in that state:

```
2 node(s) announce no runtimes and are treated as able to run anything:
edge-4, edge-9. Upgrade them and this table stops guessing on their behalf.
```

---

## Configuration, and how little there is

### Tier 1 — nothing, on any machine

Which runtimes, which container engine, how many cores, how much memory,
which GPUs. All detected. No file on any machine mentions them.

**This is where a setting belongs unless it genuinely cannot.**

### Tier 2 — one signed record for the whole mesh

Operator decisions that apply everywhere: `meshnode shed`, retention,
node tags, grants. One admin-signed record, replicated, no file edits.

There is no Tier-2 record for runtimes, because detection removed the
decision.

### Tier 3 — genuinely one machine's business

A port range, a data directory, a volume path. Facts about *that box*, in
that box's `meshnode.toml`, written once by whatever provisions it.

Two optional keys live here, and most meshes will never set either:

```toml
[runtime]
# Prefer docker where a machine has both. Reorders the search; never
# restricts it — a preference that became an outage the day that engine
# stopped would be worse than no setting.
container_engine = "docker"

# Override the built-in engines entirely, for a runtime meshware does not
# drive: gVisor, Firecracker, a hardened wrapper of your own. Wins the
# resolution order when set. You will probably never need this.
container_plugin = "/usr/local/libexec/my-runner"
```

### The resolution order

```
1. [runtime] container_plugin is set  →  use it. An explicit override wins.
2. otherwise                          →  the engine this node detected.
3. neither                            →  refuse, naming what to install.
```

---

## Writing your own runner

The plugin contract is unchanged and stays supported. A plugin is any
executable that speaks four verbs:

```
<plugin> prepare   JSON on stdin, JSON on stdout — what command runs it
<plugin> stop      MESHWARE_JOB_ID in the environment; remove that
                   container. Idempotent: already gone is success
<plugin> list      MESHWARE_NODE_ID in the environment; print the job id of
                   every container this node started, one per line
<plugin> info      optional; what isolation controls it enforces
```

`prepare` and `stop` are required. meshware spawns the command `prepare`
names — it never lets the plugin run the container — which is what keeps a
container job's logs, cancellation, kill group, secret redaction and
accounting identical to every other job's.

`list` is how a restarting node finds what it left behind: kill `meshnode`
and conmon keeps every container it started alive, holding ports nothing
will reclaim. It must filter on the `meshware.node` label the plugin set at
`prepare` time, because two nodes on one machine each see the other's
containers.

`examples/container-plugin/meshware-podman` is a worked example of the whole
contract. It is not required to run containers, and has not been since
1.26 — read it if you are writing your own.

---

## Troubleshooting

**"this node has no container engine"** — install podman or docker and
restart meshnode. `meshnode runtimes` confirms it was found.

**A container job never lands on a machine that has podman.** Check
`meshnode runtimes` *on that machine*. A rootless podman that meshnode's
user cannot reach reports `broken` with the reason, which is usually a
missing `XDG_RUNTIME_DIR` under a bare `su`.

**`meshnode runtimes` says `ready` but jobs fail at start.** Detection is a
filter, not a promise. `meshnode jobs logs <job>` has what the engine actually
said.

**Containers survived a node restart.** They should not: a node reaps its
own on boot. They will if the node came back without an engine on its
`PATH`, or if a plugin does not answer `list` — `doctor` says which.
