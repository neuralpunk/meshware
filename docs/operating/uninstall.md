# Removing Meshware

*Reference. Describes what is in the tree.*

Taking Meshware off a machine has two halves:

1. **Tell the mesh the machine is leaving**, so no work is lost and its key stops being trusted.
2. **Remove what was installed.** That depends on how it got there.

There is no central server to clean up. A mesh is its machines, so removing the last one removes
the mesh.

To keep the machines and start a new mesh on them instead, see
[Start the mesh over](../guides/machines.md#start-the-mesh-over).

---

## 1. Take the machine out of the mesh

Run these from any machine where you are an admin. `web-01` is the machine that is leaving.

```console
$ meshnode nodes contribute web-01 --off     # nothing new is placed there
$ meshnode nodes drain web-01                # its deployment copies start elsewhere first
$ meshnode jobs ls --state running           # wait until nothing is running on web-01
```

Then, once Meshware is stopped on that machine (next section):

```console
$ meshnode nodes revoke web-01 --yes
```

**Revoking is permanent.** The mesh stops trusting that machine's key for good. If you ever put the
machine back, it joins as a new node with a new key, which is what you want.

**Revoking does not take back what the machine already has.** It held copies of the secrets that
were sealed for it. If the machine is being thrown away, wiping the disk (step 2) is enough. If it
is going somewhere you do not control, rotate those secrets with `meshnode secrets set`.

**Was it someone's workstation?** Revoking a machine does not revoke the person who used it. If
they no longer need access, revoke them too: `meshnode users revoke <name> --yes`. See
[identity.md §6](identity.md#6-revocation).

## 2. Remove what was installed

Stop the service first. On a packaged node, or one added by `meshnode adopt` or Director's
**Add machines**:

```console
$ sudo systemctl disable --now meshnode
```

A stop gives jobs that are still running 30 seconds to finish (`[shutdown] grace_secs`), then ends
them.

### Installed from a `.deb` or `.rpm`

```console
$ sudo apt purge meshnode          # Debian, Ubuntu
$ sudo dnf remove meshnode         # Fedora, RHEL, Rocky, Alma
```

The package removes the binary, the unit, the man page and the icons. It does **not** remove
anything the node made or anything added later, so go on to [what is left](#3-what-is-left-on-the-machine).

### Added with `meshnode adopt` or Director's Add machines

These write the same files a package does, with the binary in `/usr/local/bin` and the unit in
`/etc/systemd/system`:

```console
$ sudo rm -f /usr/local/bin/meshnode /usr/local/bin/meshnode.prev /usr/local/bin/meshnode.failed
$ sudo rm -f /etc/systemd/system/meshnode.service
$ sudo rm -rf /etc/systemd/system/meshnode.service.d
$ sudo systemctl daemon-reload
```

The `.prev` and `.failed` files exist only if the machine was upgraded. `meshnode.service.d` holds
the drop-ins for fleet upgrades and containers.

If the machine was added **without root**, everything is in the account that ran it:

```console
$ systemctl --user disable --now meshnode
$ rm -f ~/.config/systemd/user/meshnode.service ~/.local/bin/meshnode
$ systemctl --user daemon-reload
$ rm -rf ~/.meshware
$ loginctl disable-linger          # only if nothing else of yours needs to run while logged out
```

### Installed from the tarball or `install.sh`

Remove the binary from where you put it: `/usr/local/bin/meshnode`, or `~/.local/bin/meshnode`
when `install.sh` ran without root. If you set up the service by hand from
[install.md](install.md#from-the-tarball), remove it as for **Add machines** above.

## 3. What is left on the machine

These are the same however it was installed. Check each one; not every machine has all of them.

| What | Where | Remove with |
|---|---|---|
| **The node itself**: its key, the mesh's state, job history, logs, content, volumes lent from inside it | `/var/lib/meshware` | `sudo rm -rf /var/lib/meshware` |
| The configuration | `/etc/meshware` | `sudo rm -rf /etc/meshware` |
| The service account | the `meshnode` user and group | `sudo userdel meshnode` |
| Its container storage and sub-ID range | `/etc/subuid`, `/etc/subgid` | `sudo usermod --del-subuids 2000000-2065535 --del-subgids 2000000-2065535 meshnode`, **before** `userdel` |
| Lingering, so its containers could run | `/var/lib/systemd/linger/meshnode` | `sudo loginctl disable-linger meshnode`, **before** `userdel` |
| `.mesh` names routed to this machine | `/etc/systemd/resolved.conf.d/meshware.conf` | `sudo rm` it, then `sudo systemctl restart systemd-resolved` |
| The firewall rule for the mesh port | UDP 7777 | `sudo firewall-cmd --permanent --remove-port=7777/udp && sudo firewall-cmd --reload`, or `sudo ufw delete allow 7777/udp` |
| The archive timer | `meshware-archive.service`, `meshware-archive.timer` | `sudo systemctl disable --now meshware-archive.timer`, then remove the files where you installed them |

Delete `/var/lib/meshware` last, and only once you are sure. **It is the node.** With it gone, the
machine's identity is gone. Keep a [backup](backup.md) first if there is any chance you want this
node back as it was.

A machine set up by an older release may also have these. Remove them if they are there:

| What | Where |
|---|---|
| The retired mesh proxy's low-port setting | `/etc/sysctl.d/60-meshware-proxy.conf`. Remove it, then run `sudo sysctl -w net.ipv4.ip_unprivileged_port_start=1024` |
| The retired mesh proxy's drop-in | `/etc/systemd/system/meshnode.service.d/proxy.conf` |
| An old configuration location | `/etc/meshware/meshware/meshnode.toml`, removed with `/etc/meshware` |

The firewall row applies only where `meshnode adopt` or **Add machines** opened the port, or where
you did. Using a different mesh port? Put it in place of 7777.

## 4. Director

Director is a single file, so removing it is removing that file. Before you do, in Director:

- **Take your key with you**, or make sure you can do without it. Director's node and your user
  key live in `~/.meshware`. If you are the only admin, **back it up first**
  (**Settings › Backup & stopping**). Without an admin key, nobody can manage the mesh.
- Turn off **Start at login** in **Settings › Director** if you turned it on.

Then remove what Director made in your account:

| What | Where |
|---|---|
| This computer's node, if you are leaving the mesh | `systemctl --user disable --now meshnode`, then `~/.config/systemd/user/meshnode.service`, `~/.local/bin/meshnode` and `~/.meshware` |
| Director's own settings | `~/.meshware/director` |
| Your user key, if you no longer need it | `~/.meshware/identity` |
| The menu entry and icons | `~/.local/share/applications/meshware-director.desktop`, and `meshware-director.png` / `.svg` under `~/.local/share/icons/hicolor/` |
| Saved logins for **Add machines** | your desktop keyring, under `io.meshware.director`. Remove them with `secret-tool clear service io.meshware.director` |

Removing all of `~/.meshware` removes both Director's node and any node you ran yourself with
`meshnode up`.

Director's node is a machine in the mesh like any other. From another admin machine, revoke it as
in [step 1](#1-take-the-machine-out-of-the-mesh).

## 5. The last machine

When the last machine goes, so does the mesh: there is nothing anywhere else to delete. The only
copies left are ones you made:

- [backups](backup.md) of a node's directory, which hold its key and the mesh's state;
- a history [archive](../querying-the-archive.md) or a copy sent to a [sink](../sending-history-anywhere.md);
- published releases, if you ran your own release server.

Delete those when you no longer need them.
