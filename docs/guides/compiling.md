# Compiling across the mesh

**`meshnode cc` is a compiler wrapper.** Put it in front of your compiler and each source file is
built on whichever machine in the mesh has room — and any file the mesh has already built, for you
or for anybody else, is not built again.

```console
$ make -j64 CC='meshnode cc gcc' CXX='meshnode cc g++'
```

That is the whole setup. Nothing to install on the other machines beyond a compiler, no headers to
copy, no checkout to share, no daemon to configure.

If you have used `distcc` and `ccache`, this is both of them at once. If you have not: `distcc`
spreads compiles across machines, `ccache` remembers results so the same file is not compiled
twice, and here they are the same feature because the mesh already has a shared result cache.

> [!IMPORTANT]
> **It never breaks the build.** Anything the wrapper cannot handle — a link step, `-E`, an
> assembler input, several sources at once, a stopped daemon, an empty mesh, a dropped connection —
> falls back to running your compiler right here, verbatim, and returns its exit status. See
> [When it stays local](#when-it-stays-local).

---

## 1. Try it on one file

```console
$ meshnode cc --verbose gcc -c src/parser.c -o parser.o
meshnode cc: parser.o <- anvil (18240 bytes)

$ meshnode cc --verbose gcc -c src/parser.c -o parser.o
meshnode cc: parser.o <- cache (18240 bytes)
```

The first line says the object was compiled on `anvil`. The second says nobody compiled it at all:
the mesh had it already. `--verbose` is the flag to run once when you are setting this up, and
never again.

## 2. Point a build at it

| Build system | What to set |
|---|---|
| Make | `make -j64 CC='meshnode cc gcc' CXX='meshnode cc g++'` |
| CMake | `cmake -DCMAKE_C_COMPILER_LAUNCHER='meshnode cc' -DCMAKE_CXX_COMPILER_LAUNCHER='meshnode cc' …` |
| Anything else | Set `CC` and `CXX`, or whatever it calls the compiler |

**Pick a `-j` much larger than your own core count.** The point is that the compiles are not
happening here — `-j64` on a four-core laptop is reasonable when the mesh has sixty-four cores in
it. Start high and come down if your own machine starts swapping; the preprocessing still happens
locally and that is what costs you memory.

## 3. Choose where compiles go

```console
$ meshnode cc --on tag=build gcc -c src/parser.c -o parser.o
```

| To send compiles… | Use |
|---|---|
| Anywhere with room (the default) | `--on any` |
| To one machine | `--on forge` |
| To machines with a tag | `--on tag=build` |

Other options:

- **`--local-only`** — never distribute, but still take a cache hit if the mesh has one. This is
  the first thing to try when a build misbehaves: if `--local-only` fixes it, the problem is
  distribution; if it does not, the problem is the cache or your build.
- **`--timeout SECS`** — per-compile limit, 600 seconds by default.
- **`--verbose`** / **`-v`** — say where each object came from, or why it stayed here.

## 4. See what it is saving

```console
$ meshnode cache stats
hits      312 · misses 44 · rate 88%
saved     6.1 GB
```

**In Director:** *Run › Scripts › Build farm*. It shows the same figures, plus the exact lines to
paste into a build, so you never have to remember the `make` incantation.

---

## How one compile travels

Worth understanding, because it explains both why the other machines need nothing and where the
limits are.

1. **Your machine preprocesses the file.** `cc -E` resolves every `#include` against *your*
   headers, so what leaves is one self-contained chunk of C. This is the trick that makes the whole
   thing work: the machine that compiles it needs no headers, no checkout, and no toolchain beyond
   a compiler of its own.
2. **It is submitted as an ordinary job**, with the preprocessed text inside it.
3. **It is cached by content.** The job's cache key is a digest of its own body — which contains
   the preprocessed source *and* the compiler flags. So the same file with the same flags is **one
   key across the whole mesh, forever**: the second machine to build it does not build it, and
   neither does the second person.
4. **The object comes back** and is written where you asked for it.

Every compile arrives at its executor as a signed request from a named user and lands in the same
audit log as every other job — which is one thing neither `distcc` nor `ccache` gives you.

### Why this gets better with more people

Most of what meshware does scales with machines: twice the nodes, twice the capacity. The cache
scales with **use**. A project's tenth build is faster than its first because of the nine before
it, whoever ran them. [Running work](running-work.md) covers the general
`--cacheable` mechanism this sits on.

---

## When it stays local

This is the contract: the wrapper would rather compile here than fail. Each of these falls back
silently (`--verbose` says which, and why):

| Case | Why |
|---|---|
| A **link** step, or `-E`, or an assembler input | There is no single translation unit to ship |
| **Several source files** in one invocation | Likewise — one job, one unit |
| The **preprocessed unit is too large** | It travels inside one signed job in one frame; over the ceiling it cannot |
| **No daemon**, no mesh, or no machine with room | Nothing to send it to |
| A **network failure** mid-compile | The answer is still owed to your build |

**A real compile error does not fall back.** If the remote compiler rejected your code, its
diagnostics are the answer, and recompiling locally to print the same errors would double the cost
of every broken build.

## What it does not do

- **It does not distribute links.** Linking needs every object and the libraries; it happens here.
- **It does not ship your toolchain.** The executing machine uses *its own* compiler. On a fleet of
  mixed distributions that can mean subtly different output, so keep a build tag (`--on tag=build`)
  for machines whose toolchain you have matched, if that matters to you.
- **It is not a build system.** It runs one compile at a time, under whatever `make`, `ninja` or
  `cmake` already decided.

## When it goes wrong

```console
$ meshnode cc --verbose gcc -c src/parser.c -o parser.o    # where did it go, and why
$ meshnode cc --local-only gcc -c src/parser.c -o parser.o # is distribution the problem?
$ meshnode cache stats                                      # is the cache doing anything?
$ meshnode runtimes                                         # can the other machines run anything?
$ meshnode doctor                                           # is this machine healthy
```

A suspected bad cache entry is the one case worth knowing about: `[cache] verify_rate = 0.01` in
`meshnode.toml` re-runs one hit in a hundred and compares the answer, and a disagreement drops the
entry and writes an audit row. `[cache] enabled = false` stops this machine *using* entries while
still letting it index other machines'. See [the configuration reference](../reference/config.md#cache).

## See also

- [Running work](running-work.md) — jobs, selectors, and the general `--cacheable` mechanism
- [Adding and managing machines](machines.md) — tagging a set of build machines
- [The Director guide](../director.md) — *Run › Scripts › Build farm*
- [Configuration reference](../reference/config.md#cache) — every `[cache]` setting
