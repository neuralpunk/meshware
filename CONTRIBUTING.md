# Working on Meshware

For people building Meshware from source and changing it. Running it is in the
[README](README.md) and [`docs/`](docs/).

## Building

Latest stable Rust, edition 2024. The minimum supported version is 1.96, and CI checks it.
`make help` lists everything; these are the ones you will use:

```console
$ make doctor                     # what this machine has, and what each build needs
$ make                            # meshnode + meshware-archive, release mode
$ make director                   # Director: needs Node.js and WebKitGTK's dev packages
$ make demo                       # a real 3-node mesh on loopback, to try a change by hand
$ make check                      # fmt, clippy -D warnings, unit tests — what a PR must pass
$ make test-one T=partition       # one integration suite, in a memory-capped scope
```

For the edit-compile loop, plain cargo is faster: `cargo build -p meshware-node` builds a debug
`meshnode`. `make STATIC=1` builds the static musl binary the releases ship; it needs the
`x86_64-unknown-linux-musl` target and `musl-gcc`, and says what is missing.

The release scripts under `scripts/` (`ship.sh`, `build-node-release.sh`,
`build-director-release.sh`) are what the maintainers use to cut and sign a release; see
"Releases" below.

## Testing without running out of memory

**Do not run `cargo test --workspace` on a workstation.** The integration tests start real
meshes, with QUIC listeners, gossip, op logs and job runtimes. Cargo runs test binaries in
parallel, and each binary runs its tests in parallel, so the whole workspace at once can use all
of a machine's memory. CI runs it on machines that exist for nothing else.

Run the narrowest thing that covers your change, one test binary at a time:

```console
$ cargo check --workspace --all-targets                          # cheap, and catches most things
$ cargo test -p mesh-proto                                       # unit tests
$ cargo test -p mesh-core --lib -- engines::                     # one module's unit tests
$ cargo test -p mesh-core --test service_address -- --test-threads=2
```

- One `--test <name>` per command.
- `-- --test-threads=1` or `2` for anything under `crates/mesh-core/tests/`: each of those tests
  is a mesh, not a function.
- **`crates/mesh-core/tests/resource_limits.rs` triggers an out-of-memory kill on purpose**, to
  prove memory limits bite. Run it only when you are changing that enforcement, alone, and inside
  a memory-capped scope (below). From a desktop terminal without one, systemd can stop your whole
  session when that kill happens.

**Run builds and tests in a memory-capped scope**, so that a runaway test cannot take the
desktop with it:

```console
$ systemd-run --user --scope --quiet --collect \
    -p OOMPolicy=continue -p MemoryHigh=12G -p MemoryMax=16G \
    -- env CARGO_BUILD_JOBS=8 \
    cargo test -p mesh-core --test assigned_tags -- --test-threads=2
```

Stop anything you started when you are done: nodes, `npm run dev`, the headless Chrome the
Director test harnesses launch. `pgrep -fa meshnode` shows nodes still running.

## What CI checks, and how to check it first

| Check | Command |
|---|---|
| Formatting | `cargo fmt --all --check` |
| Lints | `cargo clippy --workspace --exclude meshware-director --all-targets -- -D warnings` |
| Dependencies: advisories, licences, bans | `cargo deny check` |
| The docs: links, commands, config snippets, the deployment reference, help text | `python3 scripts/check-docs.py` (build `meshnode` first) |
| The reference pages match the code | `python3 scripts/gen-reference.py --check` |
| The HTML site builds, and every link in it resolves | `python3 scripts/build-docs-site.py` |
| The systemd unit's hardening score | `./scripts/security-posture.sh` |

`check-docs.py` runs every `meshnode` command the docs show through the real argument parser,
and fails on internal plan codes in help text or docs. `gen-reference.py --check` fails when a
config key, metric or `MESHWARE_*` variable is in the code and not in
[the reference](docs/reference/README.md).

## Documentation

The Markdown under `docs/` is what ships. Some of it is generated from the code: regenerate
rather than edit it by hand.

```console
$ python3 scripts/gen-reference.py config                   # docs/reference/config.md
$ python3 scripts/gen-reference.py metrics --scrape FILE    # docs/reference/metrics.md, from a node's /metrics
```

The same Markdown builds the HTML documentation site, with its sidebar, search and "on this
page". The build fails on any link or anchor that points nowhere:

```console
$ python3 scripts/build-docs-site.py                   # target/docs-site/index.html, one file per page
$ python3 scripts/build-docs-site.py --single out.html # everything in one file, to open anywhere
```

A new page goes in `NAV` in `scripts/build-docs-site.py`; its look is `scripts/docs-site/`.

Write for the person using Meshware. Internal plan and ticket codes belong in code comments and
commit messages, not in help text or docs.

## Releases

**There is no project-wide signing key, and none is needed to cut a release.** Every mesh signs
what it installs with its own admins' release keys (`mesh_core::release::ReleaseSigner`), so the
project's job is only to build and publish binaries people can check.

- Bump the version with `./scripts/set-version.sh 1.0.3` (every file that carries it), rename
  `## Unreleased` in the [CHANGELOG](CHANGELOG.md) to the version and date, commit, and push a
  `v1.0.3` tag.
- The `release` workflow builds static musl binaries for x86_64 and aarch64, the `.deb`/`.rpm`
  packages, the SBOM and `SHA256SUMS`, attaches a **build-provenance attestation** to each
  (GitHub signs it through Sigstore with the workflow's own identity — no secret to keep), and
  drafts the GitHub release with the CHANGELOG entry as its notes. A tag with no CHANGELOG entry
  is refused.
- A fork that wants minisign-signed builds as well sets the `MINISIGN_SECRET_KEY` secret and puts
  its public key in `packaging/minisign.pub` (and `packaging/install.sh`; CI checks the two
  agree). Without the secret, that step is skipped.
- `./scripts/ship.sh` and `packaging/build-release.sh` build and sign a release locally with a
  minisign key of your own. They are optional conveniences, not part of the release path.

## Commits

- Commit work when it is finished and verified: it compiles, its tests pass, and it has been
  tried where that is possible. Uncommitted work is one careless command from gone.
- One coherent change per commit, with the reason in the body.
- `git checkout <path>` and `git restore <path>` throw away the working tree's changes to that
  file. Do not use them to undo an edit.

## Reclaiming disk

Cargo never removes old build output. Every rebuild after an edit writes a new binary and
orphans the old one, so `target/` grows to tens of gigabytes.

```console
$ ./scripts/clean.sh                 # report only: what exists, how big, what regenerates it
$ ./scripts/clean.sh --stale         # remove artifacts untouched for 7+ days   ← the routine one
$ ./scripts/clean.sh --all           # strip to source, ready to archive or back up
$ ./scripts/clean.sh --everything    # …and the out-of-repo kernel-build scratch directory
```

`--stale` is the one to run regularly: it removes orphaned copies and keeps recent builds, so it
costs almost nothing. It never touches source, config or git history, refuses to run during a
build, and asks before deleting anything (`--yes` to skip, required when not interactive).
