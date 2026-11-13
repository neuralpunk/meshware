# Platforms

*Reference. Describes what is in the tree.*

What Meshware runs on, what each part needs from the machine, and where that has actually been
tested. This page lists only what there is evidence for. A distribution missing from it has not
been tried, which does not mean it fails.

---

## 1. In one table

| | Runs on | Needs |
|---|---|---|
| **Node** (`meshnode`) | Linux, `x86_64` and `aarch64` | Nothing to start. Some features need more from the kernel and the system: [§2](#2-what-a-node-uses-if-it-is-there) |
| **Director** | Linux desktops, the architecture you build it on | WebKitGTK, which Tauri uses to draw the window. A keyring and a tray: [§3](#3-director) |
| `meshware-archive` | Linux, like the node | Nothing |

**Linux only, on purpose.** Nodes rely on Linux for job sandboxing (Landlock), resource limits
(cgroups v2), process groups and `/proc`. Director is Linux-only too, so one set of platform code
serves both. There is no macOS or Windows build of either.

## 2. What a node uses if it is there

`meshnode` is one static binary (musl). It does not care which C library or distribution a machine
has, and it runs without any of the following. Each one turns a feature on, and
`meshnode doctor` says which are missing on a machine:

| If the machine has | The node can | Without it |
|---|---|---|
| **Kernel 5.13 or newer** (Landlock) | Sandbox each job's file access | Jobs run unsandboxed, and `doctor` says so |
| **cgroups v2**, with a delegated subtree | Enforce `--cpus` and `--mem` as hard limits | Limits guide placement but are not enforced. The shipped unit asks for the subtree with `Delegate=yes` |
| **systemd** | Run as the packaged service, restart itself, run the archive timer, and route `.mesh` names through **systemd-resolved** | Run it from a shell, or under your own supervisor |
| **podman or docker** | Run container jobs and deployments, found by itself | Container work goes to machines that have an engine |
| **Python 3** | Run Python jobs and scripts | That work goes elsewhere |
| **ansible-playbook** | Run playbooks | That work goes elsewhere |
| **NVIDIA or AMD drivers**, with `nvidia-smi` or `rocm-smi` | Report and schedule GPUs | GPU work goes elsewhere |

`meshnode runtimes` shows what a machine found. Rootless podman under the packaged service
account needs a sub-ID range and a `newuidmap` it can use. The packages set up the first;
[runtimes.md](runtimes.md) covers both, including Ubuntu's AppArmor rule for user namespaces.

**Packages** are `.deb` (Debian, Ubuntu) and `.rpm` (Fedora, RHEL and its rebuilds). They need
systemd. The tarball works everywhere else.

## 3. Director

Director is built from source for now ([director.md](../director.md#getting-director)), so it runs
where you built it. Beyond WebKitGTK it uses:

| For | It needs | Without it |
|---|---|---|
| Remembered logins for **Add machines** | A Secret Service keyring: GNOME Keyring or KWallet | Director asks each time |
| The tray icon | A tray. On GNOME, the *AppIndicator and KStatusNotifierItem Support* extension | No tray icon. The window still works |

Director drives this computer's node rather than running one of its own, so it has the node's
needs as well — it installs and starts `meshnode` as your systemd user service the first time it
needs it. With **Contribute compute** off, that node manages the mesh and runs no work.

## 4. Where it has been tested

| What | Where | What runs there |
|---|---|---|
| Continuous integration | GitHub Actions, `ubuntu-latest`, `x86_64` | Build and tests on every change; the cold-start timing with the real binary; the resource-limit tests; Director's build and tests |
| Release builds | GitHub Actions, `ubuntu-latest` | Builds `x86_64` and `aarch64` binaries and packages. The `aarch64` build is cross-compiled and not run there |
| Development | Fedora 44, kernel 7.2, `x86_64`, podman 5.8 and Docker 29 | Everything, day to day, including rootless container deployments |
| Container runtime setup | Debian-family machines and Fedora | Rootless podman under the packaged service account. [runtimes.md](runtimes.md) records what each needed |

**Not tested yet:** `aarch64` on real hardware, distributions other than those above, and kernels
older than the CI image's. Scale is tested separately and has its own limits:
[limits.md §2](limits.md#2-tested-scale).

## 5. Versions that have to match

Nodes of different releases work together, within limits. A node's command line and its daemon
must be the same release.
[Compatibility](compatibility.md) has the rules.
