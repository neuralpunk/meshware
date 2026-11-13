**What this changes, and why**

<!-- The reason matters more than the summary: what was wrong, or missing, and for whom. -->

Fixes #

**How it was tested**

<!--
The narrow tests you ran, one `--test` at a time. Never `cargo test --workspace` on a
workstation, and never `resource_limits` outside a memory-capped scope — see CONTRIBUTING.md.
-->

**Checklist**

- [ ] Tests cover the change, and the narrow ones pass locally (`cargo test -p <crate> --test <name>`).
- [ ] `make check` passes: formatting, lints and unit tests.
- [ ] Docs are updated where behaviour, a command, a flag, a config key or an output changed.
      Generated reference pages are not edited by hand; say what changed and they are
      regenerated when the change lands.
- [ ] I am contributing this under the project's licence, MIT OR Apache-2.0 (see the end of
      README.md). No CLA, no sign-off required.
- [ ] A user-visible change has a line in `CHANGELOG.md`, under `## Unreleased` (start that
      section at the top if there is none).
