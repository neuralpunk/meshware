# Adding and managing machines

Every machine in a mesh runs `meshnode`. This page covers getting machines in, seeing what they
are doing, labelling them so work goes to the right place, and taking them out again.

## Add a machine

There are four ways, and they all end the same: a new machine that knows the mesh, and a mesh that
has checked the machine is the one you meant.

**On the same network: pair.** On the new machine, and on any machine already in the mesh, run the
same command. Both show a six-digit code; if they match, confirm on the member.

```console
$ meshnode pair            # on the new machine: waits to be let in
$ meshnode pair            # on a member: finds it, shows the code, lets it in
```

**Anywhere else: an invite.** On a member, make an invite; on the new machine, join with it.

```console
$ meshnode invite create --ttl 1h        # prints the token and the command to run
$ meshnode join mwj1…                    # on the new machine
```

An invite works once by default (`--uses 5` for more), and expires. `meshnode invite ls` lists
them and `meshnode invite revoke` cancels one early.

**Machines you can already SSH to: adopt.** From a member, install and join a list of machines in
one step. It installs `meshnode` as a system service, opens the mesh port in the firewall, and
shows you each machine's code to confirm:

```console
$ meshnode adopt ops@rack-{1..10}
```

**In Director:** *Mesh › Fleet › Add machines* does the same as `adopt`, with a form: the machines,
how to log in, and one click per machine to confirm. [The quickstart](../quickstart.md) walks
through all of this with two machines.

> [!NOTE]
> A machine joining is not the same as a **person** being let in. Joining adds the computer; the
> person using it still needs a role before they can run anything from it.
> [People and access](people-and-access.md) covers that.

## See what they are doing

```console
$ meshnode nodes           # every machine: alive or not, address, capacity, tags
$ meshnode top             # live load, running jobs and queue per machine
$ meshnode status          # this machine's view of itself and the mesh
$ meshnode doctor          # checks this machine: network, clock, disk, sandbox, containers
```

**In Director:** *Mesh › Fleet* is the list of machines, with health, load and tags; select one for
its details. *Mesh › Topology* draws them, and *Mesh › Capacity* adds up what the mesh has.

A machine that stops answering is marked suspect after a few seconds and dead a few seconds later.
It is not removed: when it comes back, it catches up by itself.

## Tag machines

Tags are labels that choose where work goes: `gpu`, `ssd`, `dc=east`, `team=data`. A job asks for
`--on tag=gpu`; a deployment says `targets: tag=gpu`.

```console
$ meshnode nodes tag gpu-01 --add gpu --add dc=east
$ meshnode nodes tag gpu-01 --rm dc=east
$ meshnode nodes tag --add ssd                # this machine
```

A machine can also tag itself in its own configuration (`tags = ["gpu"]` in `meshnode.toml`), or
when it joins (`meshnode join … --tag gpu`). Some tags are added for you: a machine announces what
it can run and what hardware it has, and `meshnode runtimes` shows what it found.

**In Director:** select a machine in *Mesh › Fleet* and edit its **Tags**.

## Stop sending work to a machine

Three steps up in strength, all reversible:

| To… | Use | What happens |
|---|---|---|
| Stop new work going there | `meshnode nodes contribute web-01 --off` | Nothing new is placed on it. What runs there keeps running. `--on` undoes it |
| Empty it of services, and keep it running | `meshnode nodes drain web-01` | Its deployment copies start somewhere else first, then stop here. `--lift` undoes it |
| Isolate a machine you do not trust | `meshnode nodes quarantine web-01 --reason "…"` | No new work, no new secrets, no terminals. `--lift` undoes it |

**To take a machine down for maintenance**, drain it, then stop the service. A stop gives the
jobs still running 30 seconds to finish:

```console
$ meshnode nodes drain web-01
$ sudo systemctl stop meshnode            # on web-01
```

**In Director:** select the machine in *Mesh › Fleet*. **Drain**, **Contributing**,
**Quarantine** and **Tags** are in its panel, and you can tick several machines to act on them
together.

## Upgrade machines

`meshnode upgrade --self` upgrades one machine. For a fleet, Director's *Mesh › Updates* downloads a
release into the mesh once and installs it on every machine that is behind, a few at a time.
[Upgrading](../operating/upgrade.md) covers rolling upgrades and going back.

## Remove a machine

Drain it, stop it, then revoke it from another machine so the mesh stops trusting its key:

```console
$ meshnode nodes drain web-01
$ meshnode nodes revoke web-01 --yes
```

Revoking is permanent. [Removing Meshware](../operating/uninstall.md) lists everything to clean
up on the machine itself.

## Start the mesh over

`meshnode reset` ends the mesh. Every machine leaves it and waits to join a new one. Use it to
start again from nothing on the same machines, without reinstalling anything.

Only the founder can reset a mesh. Run it in a terminal on the founder's machine, signed with the
founder's key. It asks you to type the mesh's name and a random code, so it cannot be run from a
script:

```console
$ meshnode reset
```

If you bound a security key, your everyday key is not the founder's. Binding set the founder's
key file aside, and `reset` tells you where it is. Sign with that file:

```console
$ meshnode --user-identity ~/.meshware/identity/user.key.set-aside-mwu1… reset
```

Every machine that gets the reset:

- stops its jobs and services, and closes open terminals;
- passes the reset on to every machine it can reach;
- moves the mesh's data into `reset/<time>/` in its home (`~/.meshware`, or `/var/lib/meshware`
  for a package install). That is the op-log, the job history, content, and the mesh's genesis.
  Nothing is deleted;
- stops and waits. Its install, service, configuration and keys stay. It will not start a mesh of
  its own when its service restarts.

`reset` lists every machine and says whether each one got the reset. Then start the new mesh and
bring the machines back:

1. On the founder's machine, choose **Create a new mesh** in Director, or run
   `meshnode up --detach --new-mesh`.
2. Adopt the other machines with **Add a machine** in Director, or `meshnode adopt <host>`. They
   are still installed, so nothing is copied to them again.

Some machines can miss the reset: one that was off or unreachable, or one running a release older
than protocol 2.0. Those are still in the old mesh. Adopt each one with `--force` (**Replace an
existing install** in Director), which moves its old mesh aside first.

Once you are sure you will not need the old data, delete `reset/` from each machine's home.
