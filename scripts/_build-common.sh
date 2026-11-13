#!/usr/bin/env bash
# Shared helpers for the build-* scripts. Not a command — source it.
#
# Exists so all four build scripts report a finished binary the same way:
# the thing you came for is the **full path**, and it is the first line of
# the block.

# Absolute repo root, from any working directory.
build_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd
}

# The commit `meshnode --version` and Director's About name (DOC-14). Read
# at compile time by `option_env!`, and cargo rebuilds when it changes. A
# tree with uncommitted changes says so, because a binary built from it is
# not the commit it names.
if [ -z "${MESHWARE_BUILD_COMMIT:-}" ] && command -v git >/dev/null 2>&1; then
  MESHWARE_BUILD_COMMIT="$(git -C "$(build_root)" rev-parse --short=12 HEAD 2>/dev/null || true)"
  if [ -n "$MESHWARE_BUILD_COMMIT" ] && [ -n "$(git -C "$(build_root)" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    MESHWARE_BUILD_COMMIT="${MESHWARE_BUILD_COMMIT}-modified"
  fi
fi
export MESHWARE_BUILD_COMMIT="${MESHWARE_BUILD_COMMIT:-}"

# The workspace version, which is the only version a build may claim.
#
# `packaging/build-release.sh` refuses a tag that disagrees with Cargo.toml
# — the binary reports Cargo.toml's, and a node that fetches v0.7.0 and
# finds 0.6.0 refuses it — so reading it here means nobody has to type a
# version that is already written down.
cargo_version() {
  sed -n 's/^version = "\(.*\)"/\1/p' "$(build_root)/Cargo.toml" | head -1
}

# Ensure the musl target and its linker are available.
#
# meshnode is built static against musl so one binary runs on any x86_64
# Linux — no glibc version to match, no distro to care about. That is the
# whole point of shipping a single file, so it is an error rather than a
# silent fallback to a host build that will not run on the node you are
# about to copy it to.
require_musl() {
  local target="$1"
  if ! command -v musl-gcc >/dev/null 2>&1; then
    cat >&2 <<MSG
error: musl-gcc not found, and meshnode is built static against musl so the
       same file runs on any x86_64 Linux.

  Fedora/RHEL : sudo dnf install musl-gcc
  Debian/Ubuntu: sudo apt install musl-tools
  Arch        : sudo pacman -S musl
MSG
    exit 1
  fi
  rustup target list --installed | grep -q "$target" || rustup target add "$target"
}

# Print where a finished binary is, and enough to know it is the right one.
#
#   report_binary <path> [--probe-version]
#
# `--probe-version` runs the binary with `--version`. Only for CLIs: a
# desktop app has no such flag and running it would open a window.
report_binary() {
  local bin="$1"
  local probe="${2:-}"
  if [ ! -x "$bin" ]; then
    echo "error: expected a binary at $bin, but it is not there" >&2
    exit 1
  fi
  echo
  echo "  binary : $bin"
  echo "  size   : $(du -h "$bin" | cut -f1)"
  echo "  sha256 : $(sha256sum "$bin" | cut -d' ' -f1)"
  if [ "$probe" = "--probe-version" ]; then
    echo "  verify : $("$bin" --version | paste -sd ' ')"
  fi
  echo
}
# Announce a build stage and time the previous one.
#
#   stage "building the frontend"   ...work...   stage "next thing"
#   stage_done                      # closes the last one
#
# A release build is a handful of steps with wildly different costs, and
# without this the only visible number is how long the whole thing took —
# which is no help at all when one stage quietly grows by ten minutes.
stage() {
  stage_done
  _stage_name="$*"
  _stage_t0=$SECONDS
  echo
  echo "==> $*"
}

stage_done() {
  [ -n "${_stage_name:-}" ] || return 0
  local s=$((SECONDS - _stage_t0))
  if [ "$s" -ge 60 ]; then
    printf '    %dm %02ds\n' "$((s / 60))" "$((s % 60))"
  else
    printf '    %ds\n' "$s"
  fi
  _stage_name=""
}

# ---------------------------------------------------------------------------
# Versions
# ---------------------------------------------------------------------------
#
# Four files carry the project's version and they have to agree, because
# each is read by something different: `Cargo.toml` is what
# `CARGO_PKG_VERSION` compiles into both binaries and therefore what the UI
# and `--version` report, `Cargo.lock` records the same for the workspace
# members, `tauri.conf.json` is what the installer and the updater see, and
# `package.json` is what `npm` prints. They had drifted to 0.7.2, 0.7.2,
# 0.7.0 and 0.7.2 before this existed.
#
# **The Python SDK left the list on 2026-09-20**, with the SDK itself.

# **`package-lock.json` joined the list on 2026-09-15.** Nothing here writes
# it, which is exactly why it was missed: `npm` does. The Director build
# reads the freshly stamped `package.json`, notices the lock disagrees, and
# rewrites the lock's own `version` to match — after the backup was taken,
# and to a file the restore did not know about. So every `ship.sh` run left
# the lock behind at the shipped version while `package.json` went back to
# the tree's, which is how the lock came to say 0.9.15 against a
# `package.json` saying 0.7.2, and how a script whose header promises
# "leaves the tree exactly as it found it" had been leaving `git status`
# dirty since the first dev build.
#
# It is listed for backup and restore only. `stamp_version` deliberately
# does not write it: `npm` is the thing that owns the lock's contents, and a
# `sed` into a lockfile is how you get a lockfile that no longer matches
# what it locks.
# **The four prose pages joined the list on 2026-09-20** (CUT-11). They were
# stamped by nothing and checked by nothing, and had done what this whole
# mechanism exists to prevent: the quickstart and the install page told a
# reader to download 0.9.27 while the tree was eleven releases past it. A
# version a *reader* types is as load-bearing as one a packager reads, and
# the install page is the first thing anybody runs.
#
# They are rewritten by literal substitution of the tree's current version
# rather than by a per-line `sed`, because the number appears in seven
# different shapes across them — `V=`, a tarball name, a `.deb` name, an
# `.rpm` name, a `status` banner, a wheel name — and a pattern per shape is
# a pattern that silently stops matching the day somebody rewords a line.
_VERSION_FILES=(
  "Cargo.toml"
  "Cargo.lock"
  "apps/director/package.json"
  "apps/director/package-lock.json"
  "apps/director/src-tauri/tauri.conf.json"
  "docs/quickstart.md"
  "docs/operating/install.md"
  "docs/operating/troubleshooting.md"
  "README.md"
)

# The prose pages above, and the shape the version takes in each.
_VERSION_DOCS=(
  "docs/quickstart.md"
  "docs/operating/install.md"
  "docs/operating/troubleshooting.md"
  "README.md"
)

# Set the version everywhere, and arrange for it to be put back.
#
# A dev build is not a release: it needs the version *compiled in*, and it
# has no business showing up in `git status` afterwards. So every caller
# stamps, builds, and restores — the trap runs on success, on failure, and
# on Ctrl-C, because a tree left claiming a version nobody released is the
# failure that wastes an afternoon.
#
# A version somebody *named* is the other case: `ship.sh 0.9.27` is a
# release of this tree, and the tree should say so afterwards. The caller
# then calls `keep_versions` once the build has succeeded, which makes the
# stamp permanent — and a failure before that point still puts everything
# back, so the tree never claims a version that was not built.
stamp_version() {
  local root ver prev f
  root="$(build_root)"
  ver="$1"
  # Read before anything is written: this is the version the prose pages in
  # `_VERSION_DOCS` were authored against, and what `_stamp_docs` rewrites.
  prev="$(cargo_version)"
  _VERSION_BAK="$(mktemp -d)"
  for f in "${_VERSION_FILES[@]}"; do
    [ -f "$root/$f" ] || continue
    mkdir -p "$_VERSION_BAK/$(dirname "$f")"
    cp "$root/$f" "$_VERSION_BAK/$f"
  done
  # PIPE and HUP as well as the obvious three. Piping this script into
  # `head` is an ordinary thing to do, and a SIGPIPE that skipped the
  # restore would leave the tree claiming a version nobody released — the
  # exact failure the trap exists to prevent, reachable by accident.
  trap restore_versions EXIT INT TERM HUP PIPE QUIT

  sed -i "0,/^version = \".*\"/s//version = \"${ver}\"/" "$root/Cargo.toml"
  for f in "apps/director/package.json" "apps/director/src-tauri/tauri.conf.json"; do
    [ -f "$root/$f" ] || continue
    sed -i "0,/\"version\": \"[^\"]*\"/s//\"version\": \"${ver}\"/" "$root/$f"
  done
  [ "$(cargo_version)" = "$ver" ] || {
    echo "error: could not set the version in Cargo.toml" >&2
    exit 1
  }
  # Checked, not merely written. A `sed` that matched nothing exits 0, so a
  # renamed key would have silently reintroduced exactly the drift this
  # list was extended to end.
  # The prose pages: every occurrence of the version the tree had, whatever
  # shape it was written in, becomes the version it is moving to. `prev` is
  # read before Cargo.toml is touched by the caller's ordering, so it is the
  # number those pages were written against.
  _stamp_docs "$root" "$prev" "$ver"
}

# Rewrite one version into another across the prose pages, and check it took.
#
#   _stamp_docs <root> <from> <to>
#
# A no-op when the two are equal, which is the ordinary case for a dev build
# that stamps the version the tree already has.
_stamp_docs() {
  local root from to f
  root="$1"; from="$2"; to="$3"
  [ "$from" = "$to" ] && return 0
  for f in "${_VERSION_DOCS[@]}"; do
    [ -f "$root/$f" ] || continue
    # Literal, not a pattern: the version is a dotted number and `.` in an
    # ERE would match anything. Escaped for `sed` the same way.
    sed -i "s/${from//./\\.}/${to}/g" "$root/$f"
    # A page that never mentioned the version is fine; one that still
    # mentions the *old* one after a rewrite is the bug this catches.
    if grep -qF "$from" "$root/$f"; then
      echo "error: $f still mentions ${from} after stamping ${to}" >&2
      exit 1
    fi
  done
  # Explicit, because the loop's last command is a `grep` that *fails* on
  # the healthy path, and this file is sourced by scripts running `set -e`.
  return 0
}

# One stamped file, verified. `$1` path, `$2` an anchored ERE that must
# match at least one line.
_check_stamped() {
  [ -f "$1" ] || return 0
  grep -qE "$2" "$1" || {
    echo "error: could not set the version in $1 (looked for /$2/)" >&2
    exit 1
  }
}

# Make the stamped version the tree's own: bring along what the build
# tools own rather than this script — the lockfiles — and forget the
# backup, so the trap has nothing to restore.
#
#   keep_versions <version>
keep_versions() {
  local root ver
  root="$(build_root)"
  ver="$1"
  # `npm` owns `package-lock.json` (see above): ask it, rather than `sed`.
  # It writes the same value into `package.json`, which is already there.
  if command -v npm >/dev/null 2>&1; then
    (cd "$root/apps/director" \
      && npm version "$ver" --no-git-tag-version --allow-same-version >/dev/null)
  fi
  # Workspace members only, from what is on disk: no dependency moves.
  (cd "$root" && cargo update --workspace --offline --quiet)
  command -v npm >/dev/null 2>&1 \
    && _check_stamped "$root/apps/director/package-lock.json" "^  \"version\": \"${ver}\",$"
  rm -rf "${_VERSION_BAK:-}"
  _VERSION_BAK=""
}

# Commit the version files, and nothing else, as "Version <version>".
#
#   commit_versions <version> <dirty-before>
#
# `dirty-before` is `version_files_dirty`'s answer from before the stamp: a
# version file that already had somebody's uncommitted edits in it is not
# this script's to commit, so then the files are left for them instead.
commit_versions() {
  local root ver dirty f
  root="$(build_root)"
  ver="$1"
  dirty="$2"
  git -C "$root" rev-parse --git-dir >/dev/null 2>&1 || {
    echo "    not a git checkout — the version files are left as they are"
    return 0
  }
  if [ -n "$dirty" ]; then
    echo "    not committed: these already had uncommitted changes of yours —"
    for f in $dirty; do echo "      $f"; done
    echo "    commit the version with them when you are ready."
    return 0
  fi
  local files=()
  for f in "${_VERSION_FILES[@]}"; do
    [ -f "$root/$f" ] && files+=("$f")
  done
  if git -C "$root" diff --quiet -- "${files[@]}"; then
    echo "    already at ${ver}; nothing to commit"
    return 0
  fi
  git -C "$root" commit --quiet --only -m "Version ${ver}" -- "${files[@]}"
  echo "    committed: $(git -C "$root" log -1 --format='%h %s')"
}

# The version files that have uncommitted changes right now, one per line.
version_files_dirty() {
  local root f
  root="$(build_root)"
  git -C "$root" rev-parse --git-dir >/dev/null 2>&1 || return 0
  for f in "${_VERSION_FILES[@]}"; do
    [ -f "$root/$f" ] || continue
    git -C "$root" diff --quiet HEAD -- "$f" 2>/dev/null || echo "$f"
  done
}

# Is version `a` older than version `b`?
#
#   version_older 0.9.26 0.9.27        yes
#   version_older 0.9.28-dev.1 0.9.28  yes: a pre-release is below its release
#
# `sort -V` for the numbers, and the pre-release rule by hand, since `sort`
# puts `0.9.28` *before* `0.9.28-dev.1`. `mesh_proto::version_cmp` is the
# authority; this only has to agree with it on what a person would type.
version_older() {
  local a="$1" b="$2" ab bb
  [ "$a" = "$b" ] && return 1
  ab="${a%%-*}"
  bb="${b%%-*}"
  if [ "$ab" = "$bb" ]; then
    case "$a" in *-*) ;; *) return 1 ;; esac
    case "$b" in *-*) ;; *) return 0 ;; esac
    [ "$(printf '%s\n%s\n' "$a" "$b" | sort -V | head -1)" = "$a" ]
    return
  fi
  [ "$(printf '%s\n%s\n' "$ab" "$bb" | sort -V | head -1)" = "$ab" ]
}

# Put every version file back exactly as it was found. Idempotent.
restore_versions() {
  [ -n "${_VERSION_BAK:-}" ] || return 0
  local root f
  root="$(build_root)"
  for f in "${_VERSION_FILES[@]}"; do
    [ -f "$_VERSION_BAK/$f" ] && cp "$_VERSION_BAK/$f" "$root/$f"
  done
  rm -rf "$_VERSION_BAK"
  _VERSION_BAK=""
}

# The next dev build's version: a pre-release of the next patch, counted
# past anything already in dist/.
#
#   next_dev_version <dist-dir> <target>     0.7.2 -> 0.7.3-dev.1
#
# semver puts `0.7.3-dev.1` above `0.7.2` and below `0.7.3`, which is what
# both ends need: the fleet takes a dev build as an upgrade, and cutting the
# real 0.7.3 later still supersedes every dev build before it.
# `mesh_proto::version_cmp` is where that comparison actually happens, and
# `version.rs` has a test pinning this exact ordering.
next_dev_version() {
  local dist="$1" target="$2" cur ma mi pa n
  cur="$(cargo_version)"
  IFS=. read -r ma mi pa <<< "${cur%%-*}"
  pa=$((pa + 1))
  n=1
  while [ -e "${dist}/meshnode-v${ma}.${mi}.${pa}-dev.${n}-${target}" ]; do
    n=$((n + 1))
  done
  printf '%s.%s.%s-dev.%s\n' "$ma" "$mi" "$pa" "$n"
}

# The newest build for one target in a directory, by **semver**, printed as
# "<version><TAB><path>". Nothing if there is none.
#
# `sort -V` cannot be used on these filenames directly: given
# `meshnode-v0.7.3-<target>` and `meshnode-v0.7.3-dev.1-<target>` it compares
# the *target name* against `dev.1`, so the answer depends on the target's
# first letter — right for `x86_64` ('d' < 'x'), backwards for `aarch64`.
# Sorting on the version alone with semver's `-` rewritten to `~` fixes it:
# GNU `sort -V` ranks `~` below everything, which is exactly semver's rule
# that a pre-release precedes its release.
newest_for_target() {
  local dir="$1" target="$2" f ver
  for f in "$dir"/meshnode-v*-"$target"; do
    [ -f "$f" ] || continue
    ver="$(basename "$f" | sed -n "s/^meshnode-v\(.*\)-${target}\$/\1/p")"
    [ -n "$ver" ] || continue
    printf '%s\t%s\t%s\n' "${ver/-/\~}" "$ver" "$f"
  done | sort -V | tail -n1 | cut -f2,3
}
