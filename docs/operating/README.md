# Operating meshware

*Reference. Describes what is in the tree, not a plan.*

**The manual, for the person who has to run this**: deploying it, securing it, upgrading it and
being woken up by it.

It assumes you have read the [README](../../README.md) and nothing else. If you have not installed
anything yet, start with the [quickstart](../quickstart.md); for the desktop app, the
[Director guide](../director.md); for keeping services running, [Deployments](../deployments.md);
for letting people in, [People and access](../guides/people-and-access.md).

| | |
|---|---|
| **[install.md](install.md)** | Getting a node onto a machine, four ways, and what each one leaves behind |
| **[network.md](network.md)** | Every port, every direction, and what to tell a firewall team |
| **[security.md](security.md)** | The architecture a security review will ask about: identity, what a compromised node can and cannot do, sandboxing and its limits, hardening the daemon |
| **[identity.md](identity.md)** | People, machines and roles: enrolment, RBAC, service accounts, SSO |
| **[upgrade.md](upgrade.md)** | Rolling a fleet, and rolling it back |
| **[compatibility.md](compatibility.md)** | Which versions work together — nodes, the command line, the SDK — and what is supported |
| **[backup.md](backup.md)** | What state exists, what to back up, and how to restore a mesh |
| **[air-gap.md](air-gap.md)** | Running with no internet at all |
| **[troubleshooting.md](troubleshooting.md)** | The questions that actually get asked, with the command that answers each |
| **[runtimes.md](runtimes.md)** | What a machine can run — containers, Python, Ansible — how meshware detects it with no configuration, and what isolation each one gets |
| **[platforms.md](platforms.md)** | What each part runs on, what it needs from the machine, and where it has been tested |
| **[limits.md](limits.md)** | Supported configurations, tested scale, and the things meshware does not do |
| **[legacy.md](legacy.md)** | What was removed from meshware — workflows, change control, the host inventory, quotas and the usage report — and what to use instead |
| **[uninstall.md](uninstall.md)** | Taking a machine out of the mesh, and removing everything Meshware put on it |

What changed in each release is the [changelog](../../CHANGELOG.md). The exact answer to "what
can I set, and what does it mean" — every `meshnode.toml` key, every `MESHWARE_*` variable, every
metric and file format — is the [reference](../reference/README.md).

**Two documents live outside this directory and belong in the same reading:**
[`no-control-plane.md`](../no-control-plane.md) is the architecture — what is authoritative,
what happens in a partition, and §7's list of what meshware deliberately does not promise. And
[`operational-cost.md`](../operational-cost.md) is the benchmark page whose headline is not
throughput.
