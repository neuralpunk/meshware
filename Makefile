# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Build, install and test Meshware from source.  `make help` lists everything.
#
#   make                     build meshnode (node + CLI) and meshware-archive
#   make demo                try it: a real 3-node mesh on this machine, gone when you stop it
#   make director            build Director, the desktop app (needs Node.js + WebKitGTK)
#   sudo make install        install what you built, with man pages and shell completions
#   make check               what a pull request has to pass
#
# Needs Linux, Rust (Cargo.toml's `rust-version` or newer — https://rustup.rs)
# and a C compiler. Director also needs Node.js 20+ and WebKitGTK 4.1.

SHELL := /bin/sh
.DEFAULT_GOAL := all
.SUFFIXES:

# -- Where `make install` puts things ----------------------------------------
#
# The defaults suit a machine you administer. A package build sets
# PREFIX=/usr and DESTDIR=<staging root>.
PREFIX      ?= /usr/local
BINDIR      ?= $(PREFIX)/bin
DATAROOTDIR ?= $(PREFIX)/share
MANDIR      ?= $(DATAROOTDIR)/man
DOCDIR      ?= $(DATAROOTDIR)/doc/meshware
BASHCOMPDIR ?= $(DATAROOTDIR)/bash-completion/completions
ZSHCOMPDIR  ?= $(DATAROOTDIR)/zsh/site-functions
FISHCOMPDIR ?= $(DATAROOTDIR)/fish/vendor_completions.d
UNITDIR     ?= /etc/systemd/system
DESTDIR     ?=

# -- How it is built ----------------------------------------------------------
CARGO        ?= cargo
CARGOFLAGS   ?=
NPM          ?= npm
PYTHON       ?= python3
TEST_THREADS ?= 2
# Nodes in `make demo` (2–16).
NODES        ?= 3
# The integration suite `make test-one` runs, e.g. `make test-one T=partition`.
T            ?=
CRATE        ?= mesh-core

# STATIC=1 builds a static musl binary, the kind the releases ship: one file
# that runs on any Linux of the same architecture, whatever its glibc. Needs
# musl-gcc (Debian/Ubuntu: musl-tools; Fedora: musl-gcc; Arch: musl). Pass
# the same STATIC=1 to `make install`.
STATIC ?=

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
MSRV    := $(shell sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
ARCH    := $(shell uname -m)
MUSL    := $(ARCH)-unknown-linux-musl

ifeq ($(STATIC),1)
  OUT         := target/$(MUSL)/release
  CARGO_BUILD := CC_$(subst -,_,$(MUSL))=musl-gcc $(CARGO) build --release --locked --target $(MUSL)
else
  OUT         := target/release
  CARGO_BUILD := $(CARGO) build --release --locked
endif

# Director builds with its own profile (ThinLTO: ~4 minutes rather than ~18,
# for a larger binary). SHIP=1 uses `release` instead — the small one.
ifeq ($(SHIP),1)
  DIRECTOR_PROFILE := release
else
  DIRECTOR_PROFILE := director
endif
DIRECTOR_OUT := target/$(DIRECTOR_PROFILE)
DIRECTOR_RES := apps/director/src-tauri/resources/meshnode

# Integration tests start real meshes — QUIC listeners, gossip, job runtimes —
# and can take a lot of memory. Where a systemd user session is available,
# test runs go in a memory-capped scope so a runaway test cannot take the
# desktop with it. MEMCAP=0 turns that off.
MEMCAP ?= 1
ifeq ($(MEMCAP),1)
  SCOPE := $(shell command -v systemd-run >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1 && \
             echo 'systemd-run --user --scope --quiet --collect -p OOMPolicy=continue -p MemoryHigh=12G -p MemoryMax=16G --')
endif

# The commit `meshnode --version` names. Only this checkout's own commit: a
# source tarball unpacked inside some other repository must not claim that
# repository's. A tree with uncommitted changes says so. Set it to override.
ifeq ($(origin MESHWARE_BUILD_COMMIT),undefined)
  ifeq ($(shell git -C '$(CURDIR)' rev-parse --show-toplevel 2>/dev/null),$(CURDIR))
    MESHWARE_BUILD_COMMIT := $(shell git -C '$(CURDIR)' rev-parse --short=12 HEAD 2>/dev/null)
    ifneq ($(MESHWARE_BUILD_COMMIT),)
      ifneq ($(shell git -C '$(CURDIR)' status --porcelain --untracked-files=no 2>/dev/null),)
        MESHWARE_BUILD_COMMIT := $(MESHWARE_BUILD_COMMIT)-modified
      endif
    endif
  endif
endif
export MESHWARE_BUILD_COMMIT

# Colour only when talking to a terminal.
ifneq ($(shell [ -t 2 ] && echo tty),)
  B := \033[1m
  C := \033[36m
  G := \033[32m
  Z := \033[0m
endif

.PHONY: all everything meshnode archive static director director-dev director-package \
        demo demo-stop doctor deps preflight preflight-director \
        install install-service install-director uninstall uninstall-service uninstall-director \
        test test-director test-one check fmt lint docs site dist clean help

# -- Build --------------------------------------------------------------------

all: meshnode archive ## Build meshnode and meshware-archive (optimised; takes a few minutes)

everything: all director ## Build all three: meshnode, meshware-archive and Director

# The node and its CLI, then the man pages and completions it generates.
# Generated here, as you, so that `sudo make install` only copies files.
meshnode: preflight ## Build just meshnode, the node and its CLI
	$(CARGO_BUILD) -p meshware-node --bin meshnode $(CARGOFLAGS)
	@rm -rf '$(OUT)/man' '$(OUT)/completions'
	@mkdir -p '$(OUT)/man' '$(OUT)/completions'
	@'$(OUT)/meshnode' manpage --dir '$(OUT)/man' >/dev/null
	@'$(OUT)/meshnode' completion bash > '$(OUT)/completions/meshnode.bash'
	@'$(OUT)/meshnode' completion zsh  > '$(OUT)/completions/_meshnode'
	@'$(OUT)/meshnode' completion fish > '$(OUT)/completions/meshnode.fish'
	@printf '\n  $(G)built$(Z)  %s  (%s)\n\n' '$(OUT)/meshnode' "$$('$(OUT)/meshnode' --version | head -n 1)"

# Read-only SQL over a node's archived history, and the ingest that fills it.
# A separate binary, so the node never links the query engine.
archive: preflight ## Build just meshware-archive, the history archive reader
	$(CARGO_BUILD) -p mesh-archive --features cli --bin meshware-archive $(CARGOFLAGS)
	@printf '\n  $(G)built$(Z)  %s\n\n' '$(OUT)/meshware-archive'

static: ## Build static musl binaries, as the releases ship (needs musl-gcc)
	@$(MAKE) --no-print-directory all STATIC=1

# Director: the frontend (Svelte, built by Vite) embedded in a Tauri binary.
# WITH_NODE=1 also builds a static meshnode and carries it inside Director, as
# a release does, so Director can install and upgrade this machine's node.
director: preflight preflight-director ## Build Director, the desktop app (WITH_NODE=1 to carry a meshnode)
ifeq ($(WITH_NODE),1)
	@$(MAKE) --no-print-directory meshnode STATIC=1
	@mkdir -p '$(DIRECTOR_RES)'
	@rm -f '$(DIRECTOR_RES)'/meshnode-* '$(DIRECTOR_RES)/VERSION'
	install -m 0755 'target/$(MUSL)/release/meshnode' '$(DIRECTOR_RES)/meshnode-$(MUSL)'
	@'target/$(MUSL)/release/meshnode' -V | awk '{print $$2}' > '$(DIRECTOR_RES)/VERSION'
endif
	cd apps/director && { [ -d node_modules ] || $(NPM) ci --no-audit --no-fund; } && $(NPM) run build
	$(CARGO) build --locked --profile $(DIRECTOR_PROFILE) -p meshware-director --features custom-protocol $(CARGOFLAGS)
	@printf '\n  $(G)built$(Z)  %s\n' '$(DIRECTOR_OUT)/meshware-director'
	@if [ ! -s '$(DIRECTOR_RES)/VERSION' ]; then \
	  printf '          (carries no meshnode — `make director WITH_NODE=1` bundles one)\n'; fi
	@echo

director-dev: preflight-director ## Run Director with live reload (development)
	cd apps/director && { [ -d node_modules ] || $(NPM) ci --no-audit --no-fund; } && $(NPM) run tauri dev

# The same packages a release carries. WITH_NODE=1 puts a static meshnode
# inside them first, as a release does.
director-package: preflight preflight-director ## Build Director's .deb and .rpm (WITH_NODE=1 to carry a meshnode)
ifeq ($(WITH_NODE),1)
	@$(MAKE) --no-print-directory meshnode STATIC=1
	@mkdir -p '$(DIRECTOR_RES)'
	@rm -f '$(DIRECTOR_RES)'/meshnode-* '$(DIRECTOR_RES)/VERSION'
	install -m 0755 'target/$(MUSL)/release/meshnode' '$(DIRECTOR_RES)/meshnode-$(MUSL)'
	@'target/$(MUSL)/release/meshnode' -V | awk '{print $$2}' > '$(DIRECTOR_RES)/VERSION'
endif
	cd apps/director && { [ -d node_modules ] || $(NPM) ci --no-audit --no-fund; } && $(NPM) run tauri build -- --bundles deb,rpm
	@printf '\n  $(G)packages$(Z)  target/release/bundle/\n\n'

# -- Try it -------------------------------------------------------------------

# Uses the meshnode already built, if there is one: a demo should not wait on
# a release relink because a doc changed. `make meshnode` first to rebuild.
demo: ## Start a real mesh of NODES=3 nodes on this machine, on loopback only
	@test -x '$(OUT)/meshnode' || $(MAKE) --no-print-directory meshnode
	@printf '\n  $(B)A demo mesh of $(NODES) real nodes, on 127.0.0.1 only.$(Z) It cannot join, or be\n'
	@printf '  joined by, a real mesh. Ctrl-C (or `make demo-stop`) removes every trace.\n\n'
	'$(OUT)/meshnode' demo --nodes $(NODES)

demo-stop: ## Stop a running demo and remove its scratch directory
	@test -x '$(OUT)/meshnode' || { echo "nothing built at $(OUT)/meshnode — no demo to stop" >&2; exit 1; }
	'$(OUT)/meshnode' demo --stop

# -- Check the machine --------------------------------------------------------

doctor: ## Check what this machine has, and what each build needs
	@printf '$(B)Meshware $(VERSION) — build doctor$(Z)\n\n'
	@ok=1; \
	have() { command -v "$$1" >/dev/null 2>&1; }; \
	row() { printf '  %-6s %-14s %s\n' "$$1" "$$2" "$$3"; }; \
	if [ "$$(uname -s)" = Linux ]; then row ok Linux "$$(uname -sr)"; else row MISSING Linux "meshnode builds and runs on Linux only"; ok=0; fi; \
	if have $(CARGO); then row ok Rust "$$(rustc --version 2>/dev/null) (needs $(MSRV)+)"; \
	  else row MISSING Rust "install from https://rustup.rs"; ok=0; fi; \
	if have cc || have gcc; then row ok "C compiler" "$$(cc --version 2>/dev/null | head -n1)"; \
	  else row MISSING "C compiler" "Debian/Ubuntu: build-essential · Fedora: gcc"; ok=0; fi; \
	echo; echo "  for STATIC=1 (release-style binaries):"; \
	if have musl-gcc; then row ok musl-gcc "$$(command -v musl-gcc)"; \
	  else row - musl-gcc "Debian/Ubuntu: musl-tools · Fedora: musl-gcc · Arch: musl"; fi; \
	echo; echo "  for Director:"; \
	if have node; then row ok Node.js "$$(node --version)"; else row - Node.js "Node.js 20+ (nodejs.org, or your distribution)"; fi; \
	if have $(NPM); then row ok npm "$$($(NPM) --version)"; else row - npm "comes with Node.js"; fi; \
	if pkg-config --exists webkit2gtk-4.1 2>/dev/null; then row ok WebKitGTK "$$(pkg-config --modversion webkit2gtk-4.1)"; \
	  else row - WebKitGTK "Debian/Ubuntu: libwebkit2gtk-4.1-dev · Fedora: webkit2gtk4.1-devel · Arch: webkit2gtk-4.1"; fi; \
	echo; echo "  optional:"; \
	if have minisign; then row ok minisign "verifies release downloads"; else row - minisign "to verify release signatures"; fi; \
	if have podman || have docker; then row ok containers "$$(command -v podman || command -v docker)"; \
	  else row - containers "podman or docker, for container deployments"; fi; \
	echo; \
	if [ $$ok = 1 ]; then printf '  $(G)Ready:$(Z) run `make` to build meshnode and meshware-archive.\n'; \
	  else printf '  Install what is MISSING above, then run `make` again.\n'; exit 1; fi

deps: doctor

preflight:
	@if [ "$$(uname -s)" != Linux ]; then \
	  echo "error: meshnode runs on Linux only, and builds there too." >&2; exit 1; fi
	@command -v $(CARGO) >/dev/null 2>&1 || { \
	  echo "error: $(CARGO) not found. Install Rust $(MSRV) or newer from https://rustup.rs," >&2; \
	  echo "       then run make again. (\`make doctor\` checks everything.)" >&2; exit 1; }
	@command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || { \
	  echo "error: no C compiler. The allocator and the crypto library compile C." >&2; \
	  echo "       Debian/Ubuntu: sudo apt install build-essential" >&2; \
	  echo "       Fedora/RHEL:   sudo dnf install gcc" >&2; exit 1; }
ifeq ($(STATIC),1)
	@command -v musl-gcc >/dev/null 2>&1 || { \
	  echo "error: STATIC=1 needs musl-gcc." >&2; \
	  echo "       Debian/Ubuntu: sudo apt install musl-tools" >&2; \
	  echo "       Fedora/RHEL:   sudo dnf install musl-gcc" >&2; \
	  echo "       Arch:          sudo pacman -S musl" >&2; exit 1; }
	@if command -v rustup >/dev/null 2>&1; then \
	  rustup target list --installed | grep -qx '$(MUSL)' || rustup target add '$(MUSL)'; \
	fi
endif

preflight-director:
	@command -v $(NPM) >/dev/null 2>&1 || { \
	  echo "error: Director needs Node.js 20+ and npm (https://nodejs.org, or your distribution's nodejs)." >&2; exit 1; }
	@pkg-config --exists webkit2gtk-4.1 2>/dev/null || { \
	  echo "error: Director needs WebKitGTK 4.1's development files." >&2; \
	  echo "       Debian/Ubuntu: sudo apt install libwebkit2gtk-4.1-dev libssl-dev librsvg2-dev" >&2; \
	  echo "       Fedora:        sudo dnf install webkit2gtk4.1-devel openssl-devel librsvg2-devel" >&2; \
	  echo "       Arch:          sudo pacman -S webkit2gtk-4.1" >&2; exit 1; }

# -- Install ------------------------------------------------------------------
#
# Copies what `make` built and runs nothing else: no cargo, no compiler, so it
# is safe under sudo and never leaves root-owned files in target/.

install: ## Install meshnode (+ meshware-archive if built), man pages, completions
	@test -x '$(OUT)/meshnode' || { \
	  echo "error: nothing to install at $(OUT)/meshnode. Run 'make$(if $(filter 1,$(STATIC)), STATIC=1)' first, as yourself." >&2; \
	  exit 1; }
	install -d '$(DESTDIR)$(BINDIR)' '$(DESTDIR)$(MANDIR)/man1' '$(DESTDIR)$(DOCDIR)' \
	  '$(DESTDIR)$(BASHCOMPDIR)' '$(DESTDIR)$(ZSHCOMPDIR)' '$(DESTDIR)$(FISHCOMPDIR)'
	install -m 0755 '$(OUT)/meshnode' '$(DESTDIR)$(BINDIR)/meshnode'
	@if [ -x '$(OUT)/meshware-archive' ]; then \
	  echo "install -m 0755 $(OUT)/meshware-archive $(DESTDIR)$(BINDIR)/meshware-archive"; \
	  install -m 0755 '$(OUT)/meshware-archive' '$(DESTDIR)$(BINDIR)/meshware-archive'; \
	fi
	install -m 0644 '$(OUT)'/man/*.1 '$(DESTDIR)$(MANDIR)/man1/'
	install -m 0644 '$(OUT)/completions/meshnode.bash' '$(DESTDIR)$(BASHCOMPDIR)/meshnode'
	install -m 0644 '$(OUT)/completions/_meshnode' '$(DESTDIR)$(ZSHCOMPDIR)/_meshnode'
	install -m 0644 '$(OUT)/completions/meshnode.fish' '$(DESTDIR)$(FISHCOMPDIR)/meshnode.fish'
	install -m 0644 README.md LICENSE-MIT LICENSE-APACHE '$(DESTDIR)$(DOCDIR)/'
	@printf '\n  $(G)installed$(Z)  %s\n' '$(DESTDIR)$(BINDIR)/meshnode'
	@printf '  next: `meshnode up` to try it, or `sudo make install-service` to run it as a service.\n\n'

# The systemd units, pointed at $(BINDIR), and the account they run as.
# Never enables or starts anything: the first start of a node with no mesh
# founds a new one, so a machine that should join an existing mesh must join
# first (docs/operating/install.md, "Running it as a service").
install-service: install ## …and the systemd units and the meshnode account (never starts it)
	install -d '$(DESTDIR)$(UNITDIR)'
	sed 's|^ExecStart=/usr/bin/meshnode |ExecStart=$(BINDIR)/meshnode |' \
	  packaging/systemd/meshnode.service > '$(DESTDIR)$(UNITDIR)/meshnode.service'
	@chmod 0644 '$(DESTDIR)$(UNITDIR)/meshnode.service'
	@if [ -x '$(OUT)/meshware-archive' ]; then \
	  echo "installing meshware-archive.service and .timer (not enabled)"; \
	  sed 's|^ExecStart=/usr/bin/meshware-archive |ExecStart=$(BINDIR)/meshware-archive |' \
	    packaging/systemd/meshware-archive.service > '$(DESTDIR)$(UNITDIR)/meshware-archive.service'; \
	  install -m 0644 packaging/systemd/meshware-archive.timer '$(DESTDIR)$(UNITDIR)/meshware-archive.timer'; \
	  chmod 0644 '$(DESTDIR)$(UNITDIR)/meshware-archive.service'; \
	fi
	@if [ -z '$(DESTDIR)' ]; then \
	  if ! getent passwd meshnode >/dev/null; then \
	    echo "useradd --system --home-dir /var/lib/meshware --shell /usr/sbin/nologin meshnode"; \
	    useradd --system --home-dir /var/lib/meshware --shell /usr/sbin/nologin meshnode; \
	  fi; \
	  install -d -o meshnode -g meshnode -m 0750 /var/lib/meshware; \
	  install -d /etc/meshware; \
	  systemctl daemon-reload; \
	  echo; \
	  echo "  The service is installed and NOT started. Start it only when you mean it:"; \
	  echo "  the first start of a node with no mesh founds a new one."; \
	  echo; \
	  echo "    found a new mesh:   sudo systemctl enable --now meshnode"; \
	  echo "    join an existing:   sudo -u meshnode meshnode join <token>   (or: pair)"; \
	  echo "                        sudo systemctl enable --now meshnode"; \
	  echo; \
	fi

install-director: ## Install Director (run `make director` first)
	@test -x '$(DIRECTOR_OUT)/meshware-director' || { \
	  echo "error: nothing to install at $(DIRECTOR_OUT)/meshware-director. Run 'make director' first, as yourself." >&2; \
	  exit 1; }
	install -d '$(DESTDIR)$(BINDIR)'
	install -m 0755 '$(DIRECTOR_OUT)/meshware-director' '$(DESTDIR)$(BINDIR)/meshware-director'
	@printf '\n  $(G)installed$(Z)  %s\n' '$(DESTDIR)$(BINDIR)/meshware-director'
	@printf '  Start it from a terminal once (`meshware-director`); it adds itself to your app menu.\n\n'

# Removes what `make install` put there, and nothing a node wrote: its data
# directory holds its identity and your mesh's history.
uninstall: ## Remove what `make install` installed (a node's data is kept)
	rm -f '$(DESTDIR)$(BINDIR)/meshnode' '$(DESTDIR)$(BINDIR)/meshware-archive'
	rm -f '$(DESTDIR)$(MANDIR)/man1/meshnode.1' '$(DESTDIR)$(MANDIR)'/man1/meshnode-*.1
	rm -f '$(DESTDIR)$(BASHCOMPDIR)/meshnode' '$(DESTDIR)$(ZSHCOMPDIR)/_meshnode' \
	  '$(DESTDIR)$(FISHCOMPDIR)/meshnode.fish'
	rm -rf '$(DESTDIR)$(DOCDIR)'

# Refuses while the node is running, rather than stopping a member of
# somebody's mesh as a side effect. Leaves the account, /var/lib/meshware and
# /etc/meshware: see docs/operating/uninstall.md for those.
uninstall-service: ## Remove the systemd units (refuses while meshnode runs)
	@if [ -z '$(DESTDIR)' ] && systemctl is-active --quiet meshnode 2>/dev/null; then \
	  echo "error: meshnode is running. Stop it first: sudo systemctl disable --now meshnode" >&2; \
	  exit 1; fi
	rm -f '$(DESTDIR)$(UNITDIR)/meshnode.service' \
	  '$(DESTDIR)$(UNITDIR)/meshware-archive.service' '$(DESTDIR)$(UNITDIR)/meshware-archive.timer'
	@if [ -z '$(DESTDIR)' ]; then systemctl daemon-reload; fi

uninstall-director: ## Remove Director's binary
	rm -f '$(DESTDIR)$(BINDIR)/meshware-director'

# -- Test ---------------------------------------------------------------------
#
# `make test` runs the unit tests only. The integration suites under
# crates/*/tests each start real meshes of several nodes; run them one at a
# time with `make test-one T=<name>`. Never `cargo test --workspace` on a
# workstation: all of them at once can exhaust its memory.

test: preflight ## Unit tests (fast; no meshes are started)
	$(SCOPE) $(CARGO) test --locked -p mesh-proto $(CARGOFLAGS)
	$(SCOPE) $(CARGO) test --locked -p mesh-core --lib $(CARGOFLAGS) -- --test-threads=4
	$(SCOPE) $(CARGO) test --locked -p meshware-node --bins $(CARGOFLAGS)
	$(SCOPE) $(CARGO) test --locked -p mesh-archive --lib $(CARGOFLAGS)
	@printf '\n  $(G)unit tests passed$(Z) — integration suites: `make test-one T=<name>` (see `ls crates/mesh-core/tests`)\n\n'

test-director: preflight-director ## Director's frontend tests and type check
	cd apps/director && { [ -d node_modules ] || $(NPM) ci --no-audit --no-fund; } && $(NPM) run check && $(NPM) test

test-one: preflight ## One integration suite: T=<name> [CRATE=mesh-core], e.g. T=partition
	@test -n '$(T)' || { echo "usage: make test-one T=<suite> [CRATE=mesh-core]" >&2; \
	  echo "suites: $$(ls crates/$(CRATE)/tests/*.rs 2>/dev/null | xargs -n1 basename | sed 's/\.rs$$//' | grep -v '^resource_limits$$' | tr '\n' ' ')" >&2; exit 1; }
	@if [ '$(T)' = resource_limits ] && [ -z '$(SCOPE)' ]; then \
	  echo "error: resource_limits triggers an OOM kill on purpose. Run it only inside a memory-capped" >&2; \
	  echo "       systemd scope (needs a systemd user session), never from a bare desktop terminal." >&2; exit 1; fi
	$(SCOPE) $(CARGO) test --locked -p $(CRATE) --test $(T) $(CARGOFLAGS) -- --test-threads=$(TEST_THREADS)

check: preflight ## Formatting, lints and unit tests: what a pull request needs
	$(CARGO) fmt --all --check
	$(SCOPE) $(CARGO) clippy --locked --workspace --exclude meshware-director --all-targets --features mesh-archive/cli -- -D warnings
	@$(MAKE) --no-print-directory test

fmt: ## Format the Rust code
	$(CARGO) fmt --all

lint: preflight ## Clippy, warnings as errors, as CI runs it
	$(SCOPE) $(CARGO) clippy --locked --workspace --exclude meshware-director --all-targets --features mesh-archive/cli -- -D warnings

# -- Docs and the website -----------------------------------------------------

docs: ## Build the HTML documentation from docs/ into target/docs-site
	$(PYTHON) scripts/build-docs-site.py
	@printf '\n  $(G)docs$(Z)  target/docs-site/index.html\n\n'

site: ## Serve the project website (site/) at http://127.0.0.1:8000
	@printf '  serving site/ at http://127.0.0.1:8000 — Ctrl-C to stop\n'
	$(PYTHON) -m http.server 8000 --bind 127.0.0.1 --directory site

# -- Collect the outputs ------------------------------------------------------

# The node is named as a release names it — meshnode-v<version>-<target> —
# so `meshnode upgrade --publish` and Director's "Publish a build" read its
# version and architecture off the file name.
NODE_DIST := meshnode-v$(VERSION)-$(ARCH)-unknown-linux-$(if $(filter 1,$(STATIC)),musl,gnu)

dist: ## Copy what was built into dist/ (node named for publishing), with SHA256SUMS
	@mkdir -p dist
	@n=0; \
	if [ -x '$(OUT)/meshnode' ]; then cp '$(OUT)/meshnode' 'dist/$(NODE_DIST)'; echo "  dist/$(NODE_DIST)"; n=$$((n+1)); fi; \
	for f in '$(OUT)/meshware-archive' '$(DIRECTOR_OUT)/meshware-director'; do \
	  if [ -x "$$f" ]; then cp "$$f" dist/; echo "  dist/$$(basename "$$f")"; n=$$((n+1)); fi; done; \
	if [ $$n = 0 ]; then echo "nothing built yet — run 'make' first" >&2; exit 1; fi
	@cd dist && sha256sum '$(NODE_DIST)' meshware-archive meshware-director 2>/dev/null > SHA256SUMS; echo "  dist/SHA256SUMS"
	@printf '\n  put it on your fleet:  meshnode upgrade --publish dist/$(NODE_DIST) --version $(VERSION)\n\n'

# -- Housekeeping -------------------------------------------------------------

clean: ## Remove everything the build made (target/, Director's frontend build)
	$(CARGO) clean
	rm -rf apps/director/dist '$(DIRECTOR_RES)'/meshnode-* '$(DIRECTOR_RES)/VERSION'

help: ## List every target and setting
	@printf '$(B)Meshware $(VERSION)$(Z) — build from source.  Needs Linux, Rust $(MSRV)+ and a C compiler.\n\n'
	@printf '$(B)Start here$(Z)\n'
	@printf '  make doctor               check this machine has what the build needs\n'
	@printf '  make                      build meshnode and meshware-archive\n'
	@printf '  make demo                 try it: a real mesh of 3 nodes on this machine\n'
	@printf '  sudo make install         install it\n\n'
	@printf '$(B)Every target$(Z)\n'
	@grep -hE '^[a-z][a-z-]*:.*## ' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*## "}; {printf "  $(C)%-24s$(Z) %s\n", $$1, $$2}'
	@printf '\n$(B)Settings$(Z) (make VAR=value)\n'
	@printf '  STATIC=1                  static musl binaries, as releases ship (build and install)\n'
	@printf '  WITH_NODE=1               director: carry a static meshnode inside Director\n'
	@printf '  SHIP=1                    director: smallest binary (slower build)\n'
	@printf '  NODES=$(NODES)                   demo: how many nodes (2–16)\n'
	@printf '  T=<suite> CRATE=<crate>   test-one: which integration suite\n'
	@printf '  MEMCAP=0                  run tests without the memory-capped systemd scope\n'
	@printf '  PREFIX=$(PREFIX)  BINDIR=$(BINDIR)  DESTDIR=<staging root, for packagers>\n'
	@printf '  CARGO=$(CARGO)  CARGOFLAGS=<extra cargo flags>  NPM=$(NPM)\n'
