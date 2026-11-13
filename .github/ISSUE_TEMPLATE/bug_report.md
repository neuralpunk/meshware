---
name: Bug report
about: Something Meshware did that it should not have, or did not do that it should
title: ""
labels: bug
---

<!--
A security problem? Do not report it here. Follow SECURITY.md: use GitHub's private
vulnerability reporting (Security → Report a vulnerability).
-->

**What happened**

What you ran, and what it printed. Paste the output as text, not a screenshot.

```console
$ meshnode …
```

**What you expected**

**How to reproduce it**

The smallest set of steps you have. A `meshnode demo` mesh is ideal: it runs on one machine and
needs nothing from anyone else to reproduce.

1.
2.
3.

**The machine and the mesh**

- `meshnode --version` (all four lines):
- Linux distribution and kernel (`uname -sr`):
- How it was installed: tarball, `.deb`/`.rpm`, `meshnode adopt`, `install.sh`, `make` from source, Director:
- The mesh: one node, several on a LAN, or across the internet (relayed):
- Do the nodes run different versions? (`meshnode upgrade --status`, `meshnode nodes`):

**What `meshnode doctor` says**

```console
$ meshnode doctor
```

<!--
For anything harder to pin down, `meshnode support bundle` writes one file that answers the
first hour of questions. Read it before you attach it: it describes your machine and your mesh.
-->
