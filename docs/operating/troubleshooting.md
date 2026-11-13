# Troubleshooting

*Reference. Describes what is in the tree.*

**The questions that actually get asked, and the command that answers each.** Start here before
reading logs.

---

## Start with these three

```console
$ meshnode doctor          # connectivity, clock skew, disk, ports, sandbox, permissions
$ meshnode status          # what this node thinks is true right now
$ meshnode nodes           # what the mesh thinks is true
```

`doctor --fix` applies the safe fixes it knows and says plainly which findings have no automatic
fix and what to do instead. Its output is also the right thing to paste into a support request.

---

## The first hour

The failures new meshes meet most, by the words on the screen. Search this page for the text you
see.

### `cannot reach daemon at … — is meshnode up running?`

```
Error: ipc error: cannot reach daemon at /run/user/1000/meshware/node-a6574d0db24a.sock — is `meshnode up` running? (No such file or directory (os error 2))
```

The command found no node to talk to on this machine.

- **No node is running.** Start one: `meshnode up` in a terminal, or
  `sudo systemctl start meshnode` for a packaged node. `systemctl status meshnode` and
  `journalctl -u meshnode` say why one stopped.
- **The node runs from another home.** The command looks in `~/.meshware`, in Director's node, and
  in `/var/lib/meshware`, but not if `MESHWARE_HOME` or `--data-dir` names somewhere else. Unset it,
  or point it at the node's home.
- **`(Connection refused)`** at the end instead means a node was there and has stopped, leaving
  its socket behind. Start it again.

### `you are not authorized to use the meshware node on this machine`

```
you are not authorized to use the meshware node on this machine.

A node is listening at /var/lib/meshware/run/meshnode.sock, but it runs under another account (uid 993) and will not accept a connection from yours.
```

A node **is** running: the packaged service, as its own `meshnode` account. Your login cannot use
its socket, and it should not have to — Meshware knows people by their key, not their login.

```console
$ meshnode whoami     # your key, and where it is kept
$ meshnode enrol      # ask an admin to let that key in
```

If you administer this machine, run commands as the node's own account instead:
`sudo -u meshnode meshnode status`. See [install.md §6](install.md#6-using-the-cli-on-a-server).

### `join failed: transport error: dial …: timed out`

```
Error: join failed: transport error: dial 10.0.0.9:7777: timed out
```

The new machine could not reach the address in the invite. Almost always **a firewall blocking
UDP 7777**: the mesh is QUIC, so opening TCP 7777 does nothing. Open UDP 7777 on every machine
that runs a node:

```console
$ sudo firewall-cmd --permanent --add-port=7777/udp && sudo firewall-cmd --reload   # Fedora, RHEL
$ sudo ufw allow 7777/udp                                                           # Ubuntu, Debian with ufw
$ sudo nft add rule inet filter input udp dport 7777 accept                         # plain nftables
```

The `nft` rule assumes the stock `inet filter` table with an `input` chain, and lasts until
reboot: add it to `/etc/nftables.conf` to keep it. `meshnode adopt` and Director's **Add
machines** open the port for you where firewalld or ufw is running.

If the port is open, check the address itself: an invite carries the addresses its node
advertises, and a machine elsewhere cannot reach a LAN address. `meshnode invite create` warns
when that is all it has. See [network.md](network.md#1-ports).

### `waiting to pair as '…'` and nothing happens

```
waiting to pair as 'build-07' — approve from Director, or `meshnode invite approve` on any member…
```

Pairing finds machines on the local network with mDNS (UDP 5353) and a broadcast beacon
(UDP 7778). If neither gets through, the new machine waits and no member ever sees it. Either
allow them on the LAN:

```console
$ sudo firewall-cmd --permanent --add-service=mdns --add-port=7778/udp && sudo firewall-cmd --reload
$ sudo ufw allow 5353/udp && sudo ufw allow 7778/udp
```

or skip discovery and name the machine from a member, with the address the waiting machine
printed:

```console
$ meshnode invite approve --addr 10.0.0.9:7777
```

Machines on different subnets never discover each other. Use `--addr`, or an invite token
(`meshnode invite create`, then `meshnode join <token>`).

### SELinux: `203/EXEC` and `Permission denied`, on a binary that is plainly there

```
meshnode.service: Main process exited, code=exited, status=203/EXEC
meshnode.service: Unable to locate executable '/usr/local/bin/meshnode': Permission denied
```

The file exists, is executable, and runs by hand, so every check points the wrong way. On a
machine where SELinux is enforcing, a binary *moved* into place (from `/tmp` or a home
directory) keeps a label that systemd may not execute. Relabel it:

```console
$ ls -Z /usr/local/bin/meshnode          # user_tmp_t or user_home_t is the problem
$ sudo restorecon -Fv /usr/local/bin/meshnode
$ sudo systemctl restart meshnode
```

`meshnode adopt` and **Add machines** label the binary themselves, and say this in so many words
when they meet a machine installed some other way.

### `sandbox NOT applied — jobs run unconfined`

```
fail  sandbox             NOT applied — jobs run unconfined. this kernel has no Landlock (…; needs 5.13+ with CONFIG_SECURITY_LANDLOCK and `landlock` in lsm=)
```

Jobs still run, but nothing stops one from reading the rest of the machine. Landlock needs a Linux
5.13 kernel or newer, built with it **and** with it enabled:

```console
$ uname -r
$ cat /sys/kernel/security/lsm           # must include landlock
```

A kernel that has Landlock but leaves it off needs `landlock` added to the `lsm=` boot parameter,
after the modules already listed there. A kernel older than 5.13 needs upgrading. To refuse jobs
rather than run them unconfined, set `[runtime] sandbox = "on"`; each job is then refused with
`[runtime] sandbox = "on", but this kernel has no Landlock …`.

### Clocks: `nothing appears to be disciplining this machine's clock`

Changes to the mesh are signed and timestamped, and every node checks the time. A machine whose
clock runs more than five minutes ahead has its changes **held by every other node** until real
time catches up: its jobs, tags and renames do not appear anywhere else, and nothing fails.
`meshnode doctor` names it, from any machine:

```
warn  clock sync          nothing appears to be disciplining this machine's clock. …
fail  clock               build-07 (~412s ahead) dated ops more than 300s ahead of this node, so this node is holding them until its clock catches up — fix NTP on that machine (or on this one)
```

A request signed on a machine with a wrong clock is refused with
`request timestamp outside the freshness window`.

Turn time synchronisation on, on the machine `doctor` names:

```console
$ sudo timedatectl set-ntp true           # systemd-timesyncd
$ timedatectl status                      # "System clock synchronized: yes"
```

or install chrony (`dnf install chrony` / `apt install chrony`) and start `chronyd`.

---

## "A node will not join"

```console
$ meshnode doctor                          # on both machines
$ meshnode ping <the-other-node>
```

In order of how often it is the answer:

1. **UDP 7777 is blocked.** The mesh is QUIC. A firewall allowing TCP 7777 gives a mesh where
   everything looks unreachable. See [network.md](network.md#1-ports).
2. **The invite expired.** Default TTL is 24h. `meshnode invite create` again.
3. **mDNS does not cross the segment.** `meshnode pair --addr 10.0.0.9:7777` skips discovery.
4. **The joiner is behind NAT and there is no reachable member.** One node needs a public
   address, or a relay. See [network.md](network.md#3-what-a-mesh-needs-from-a-network).
5. **Clock skew.** Signed envelopes have a five-minute skew window, and so do the changes a
   node makes. A node more than five minutes ahead has its changes held by every other node
   until real time catches up: its jobs, tags and renames do not appear elsewhere. `doctor`
   on any other node names it. Fix NTP on that machine.

## "A node shows as dead but the machine is fine"

Detection across a silent packet drop takes **5–10 s to Suspect and 10–13 s to Dead**: a
dropped datagram is not a refused connection, so each probe has a deadline and a failing peer is
probed every second. Recovery after the path returns is **~51 ms**.

So: wait a few seconds. If it stays dead, the path is genuinely broken — `meshnode ping` it and check
UDP.

**Dead is not evicted.** The member record survives; only an admin-signed revocation removes a
node. It rejoins by itself.

## "My job will not schedule"

```console
$ meshnode plan --on tag=gpu --gpus 2      # where would this land, and when
$ meshnode queue                           # who is waiting here, behind whom, and why
$ meshnode jobs explain <job-id>           # the top five candidates and their scores
```

Common causes: no node satisfies the requirement (`--gpus 2` with no two-GPU node free); every
candidate is draining or held back from work (`meshnode nodes contribute`); the runtime it needs
is not installed anywhere it may go (`meshnode runtimes`). `meshnode run … --dry-run` prints the
targets and blast radius without submitting.

## "Permission denied"

The message names the reason, and the reasons are different problems:

| Message | Means |
|---|---|
| `unknown user mwu1…` | The key has never been enrolled. `meshnode enrol` |
| `user … was revoked` | Terminal. That key can never be re-enrolled; generate a new identity |
| `service account … expired` | The **principal's** clock ran out. Re-granting will not help — mint a replacement |
| `requires one of: …` | Enrolled, but lacking the role. `meshnode whoami` shows what you have |

```console
$ meshnode whoami           # who am I, and what may I do about it
```

## "A job ran but I cannot find its output"

```console
$ meshnode jobs             # recent jobs and their state
$ meshnode history jobs      # this node's local job history
$ meshnode history results <fan-id>  # the rows a fan-out answered with
```

Output beyond 64 KiB spills to the object store, which is **15-minute ephemeral transport** and
not a storage tier. If you need output kept, `--outputs` or `--cache-outputs` it, or point a
sink at your warehouse.

If the mesh says *"output expired"*, retention removed it — `meshnode retention show`.

## "The mesh is using more disk than I expected"

```console
$ meshnode state            # what the op-log costs here, and what compacting would recover
$ meshnode objects          # what this node is holding
```

The op-log is replicated to every node, so its cost is (surviving history) × (nodes). Bound it
with `meshnode retention set runs=90d`. Audit has an enforced 365-day floor and cannot be
trimmed below it.

## "A node's disk filled up"

A node that runs out of room stops taking work and says so; it does not carry on half-recorded.

```console
$ curl -s localhost:7776/healthz     # 503, and "problems" names which of these it is
$ meshnode doctor
```

| `problems` says | What the node is doing |
|---|---|
| `the data directory is below its free-space floor` | Under `[disk] min_free_mb` / `min_free_pct` after reclaiming its content store. It takes no new jobs or bundles — other nodes see it as busy and place work elsewhere — and keeps no more output from the jobs already running (the log says where it stopped). Checked every ten seconds while short of room |
| `the op-log cannot be written: …` | A write to its replicated log failed. Every action that would need recording is refused with *"this node cannot write to its log right now"*, and it takes nothing its peers send it until it can store it. The write that failed took effect nowhere: no sequence number was spent, and peers are not waiting on it |

Free space on the data directory. Nothing else is needed: within about ten seconds of there being
64 MiB free, the node reopens its log and its history index, writes what it had been holding,
and `/healthz` answers 200 again. A restart works too, but is not required.

Peers keep counting a full node as alive — it is still answering — so its running jobs are not
adopted elsewhere, and its own failures to write are never counted against its peers.

What usually fills a disk is job output. Each stream is capped at `[disk] job_output_max_mb`
(256 MiB by default, `0` for no cap); past it the job keeps running, its output is read and
discarded, and `stdout.log` ends with a line saying so. Output you mean to keep belongs in
`--outputs` or a sink, not in a node's job log.

## "A job used more CPU or memory than it asked for"

Declared limits **are** enforced — but only where the node can enforce them. Check:

```console
$ meshnode doctor | grep 'resource limits'
```

`enforced (cpu, memory, pids)` means `--cpus` and `--mem` are real ceilings. `NOT enforced`
names the reason, and the usual one is a unit without `Delegate=yes` — see
[limits.md §4](limits.md#4-resource-limits-are-enforced--where-the-node-can-enforce-them).

## "A job died with exit 137 and no explanation"

137 is SIGKILL. If the job exceeded its declared memory, meshware appends the reason to its
stderr rather than leaving you with a bare signal:

```
meshware: killed by the kernel — this job exceeded the 64 MB it asked for (peak 64 MB).
```

If there is no such line, something else killed it — a cancel, a drain, or the host's own OOM
killer acting on the node rather than the job.

## "A node is refusing everything, including terminals"

Check whether it is quarantined:

```console
$ meshnode nodes            # a quarantined node says so, with the reason
$ meshnode nodes quarantine <node> --lift
```

Quarantine is deliberate isolation — no work **and** no terminals — and it is reversible.
That is different from `nodes contribute off`, which stops work but still allows a shell.

## "It only fails on that one box"

```console
$ meshnode nodes diff hq anvil     # kernel, libc, drivers, packages — everything collectors report
$ meshnode facts <node>
```

This is what `nodes diff` exists for.

## "Did somebody change something?"

```console
$ meshnode audit --since 24h
$ meshnode audit --user alice --action grant.
```

Actors marked `*` are service accounts — machines, not people.

## "Is the sink working?"

```console
$ meshnode sinks ls        # lag, sent, spooled, dropped, last status
$ meshnode sinks test <name>   # one synthetic row, response printed verbatim
```

A failing sink spends spool, never scheduler. If it was held by a restore, `meshnode sinks
resume`.

## Logs

```console
$ journalctl -u meshnode -f
$ meshnode config show     # the resolved config, so you are debugging what is actually loaded
```

Log level is hot-reloadable: edit the config and `systemctl reload meshnode`.

### Following one request through the log

Every line a node writes while handling a request carries the fields of the
request it belongs to, so you can follow one `meshnode run` through a busy
node instead of guessing by timestamp:

| Field | What it is |
|---|---|
| `trace` | One request, on one node. Minted when the request arrives and carried into everything it starts, including work that outlives the request itself. Node-local — it means nothing on another machine. |
| `req` | Which request it was: `Run`, `Jobs`, `SecretSet`. |
| `user` | Who sent it. |
| `job` | The job. **The same id on every node**, because it is what the submitter's plan replicated and what the executor journaled. |
| `group` / `gang` | The fan-out or gang, likewise the same everywhere. |

Under systemd these are structured fields rather than words in a sentence,
so they can be selected on rather than grepped for:

```console
$ journalctl -u meshnode -o json | jq -r 'select(.JOB=="7f3a1c") | .MESSAGE'
$ journalctl -u meshnode -o json | jq -r 'select(.TRACE=="1ca7000004") | .MESSAGE'
```

**Across machines, join on the job, group or gang id, not on `trace`.** A
fan-out's `group` appears in the owner's log and in the log of every node
that ran a target; `trace` does not leave the node that minted it.

meshware does not export OpenTelemetry. The exporter and its gRPC stack are
a large dependency tree for a product that ships as one static binary, so
the spans are emitted as structured log records instead, and a collector
that reads the journal gets the same fields.

## What to include in a support request

One command:

```console
$ meshnode support bundle
Wrote meshware-support-1788861515.json (mode 0600)

  meshnode      1.0.2 · protocol 2.0
  host          linux x86_64 · kernel 6.11.0
  doctor        24 ok, 1 warn, 0 fail
  nodes         1
  audit         1 entries
  metrics       120 lines
  redacted      0 values
```

It collects the version and host, every `doctor` check, this node's status, each peer and what
it is running, a tail of the audit log (`--since 24h --limit 200` by default) and a `/metrics`
snapshot — into one JSON file you can attach.

**It works when the node does not.** Every section degrades on its own, which is the point: a
daemon that will not start still produces a bundle carrying the config, the host and every
check that needs no socket, plus a `unavailable` list saying what was missing and why. That is
usually the file that answers the question.

**Read it before you send it.** Credentials are redacted by key name — recursively, so a field
added tomorrow is covered without anybody remembering to update a list — and webhook URLs keep
their host and lose their path, because for a Slack endpoint the path *is* the credential. The
summary prints how many values were removed. Node names, addresses, user ids and the audit tail
are **not** redacted: it is a redacted file, not a harmless one.

If you would rather assemble it by hand, or want the journal alongside it:

```console
$ journalctl -u meshnode --since '1 hour ago' --no-pager
```
