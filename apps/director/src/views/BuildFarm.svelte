<script lang="ts">
  // Build farm — the Run tab you never press Run on.
  //
  // Every other mode on this page asks "what do you want to run, and
  // where". This one does not, and that is the whole reason it needed its
  // own tab rather than a checkbox somewhere: it is not a way to start a
  // job. It is a **door other tools walk through**. You set it up once, in
  // your own project, and from then on your build spreads across the mesh
  // without anybody opening Director again.
  //
  // ## One door
  //
  // `meshnode cc` is built and works today (`meshware-node/src/farm.rs`).
  // It is distcc's idea with ccache's folded in: preprocess locally so the
  // translation unit is self-contained, ship it as a job, key it by its own
  // content so the same unit compiled twice is compiled once mesh-wide, and
  // fall back to the local compiler on anything it cannot handle. It works
  // with any Makefile, today, with one variable.
  //
  // The remote-execution endpoint — the one Bazel, Buck2 and Pants speak —
  // is **not built** (M9 §10 Q4). It had a card of its own here, with a
  // "Not built yet" pill, the gRPC history and a greyed-out Bazel line, and
  // on the screen a build-farm buyer opens first that read as an unfinished
  // product (UX-1). It is one sentence at the foot of the page now: enough
  // that nobody hunts for a setting that does not exist, and no roadmap
  // bookkeeping on a customer's screen. When it lands it gets a card beside
  // the compiler wrapper, in the same shape.

  import { api, errText, fmtBytes, type CacheStatsView } from "../lib/api";
  import type { FarmBuildLine } from "../lib/bindings/FarmBuildLine";
  import { app, ui } from "../lib/state.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Terminal from "../lib/components/LazyTerminal.svelte";
  import { staggerIn } from "../lib/motion";

  let stats = $state<CacheStatsView | null>(null);
  let statsErr = $state<string | null>(null);
  let loading = $state(true);

  async function load() {
    try {
      stats = await api.cacheStats();
      statsErr = null;
    } catch (e) {
      statsErr = errText(e);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    void load();
  });

  let copied = $state<string | null>(null);
  async function copy(what: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      copied = what;
      setTimeout(() => {
        if (copied === what) copied = null;
      }, 1600);
    } catch {
      /* a blocked clipboard costs the convenience, not the text on screen */
    }
  }

  // How many cores could actually take a compile right now — the number
  // people want when deciding what to put after `-j`, and the one number
  // that makes the whole feature feel real rather than theoretical. The
  // machine count that used to sit beside it went to Run.svelte with the
  // notes it was written for.
  const cores = $derived(
    app.nodes
      .filter((n) => !n.revoked && !n.left)
      .reduce((sum, n) => sum + (n.capacity?.cores ?? 0), 0),
  );

  const suggestedJ = $derived(cores > 0 ? cores : 8);

  // -- running a build from here (the door, opened) -------------------------
  //
  // This panel used to print `make -jN CC='meshnode cc gcc' …` and leave the
  // rest to the reader. On a machine with only Director installed that line
  // is a dead end twice over: there is no `meshnode` on PATH to be `CC`, and
  // a `meshnode` that *is* installed resolves `~/.meshware` while Director's
  // node lives in `~/.meshware/director` — so it would find no daemon and
  // compile every unit locally, silently, because falling back is the
  // wrapper's contract.
  //
  // So the line is composed by the engine (`farm_build_line`), which knows
  // both facts, and the same string is the one you can copy *and* the one
  // the Run button executes. There is no second, hidden version of it.
  let projectDir = $state("");
  let buildCommand = $state("");
  let line = $state<FarmBuildLine | null>(null);
  let lineErr = $state<string | null>(null);
  let running = $state(false);
  let runKey = $state(0);

  // Seeded once the mesh's core count is known, and only while untouched —
  // retyping it should not be undone by a node joining.
  let commandTouched = $state(false);
  $effect(() => {
    if (!commandTouched) buildCommand = `make -j${suggestedJ}`;
  });

  // The line follows the two fields. Guarded against the answer to a stale
  // pair landing after the answer to the current one.
  let compose = 0;
  $effect(() => {
    const dir = projectDir.trim();
    const cmd = buildCommand.trim();
    const seq = ++compose;
    if (!dir || !cmd) {
      line = null;
      lineErr = null;
      return;
    }
    api
      .farmBuildLine(dir, cmd)
      .then((l) => {
        if (seq === compose) {
          line = l;
          lineErr = null;
        }
      })
      .catch((e) => {
        if (seq === compose) {
          line = null;
          lineErr = errText(e);
        }
      });
  });

  async function pickProject() {
    const dir = await api.pickDirectory("Choose the project to build");
    if (dir) {
      // Composing a command line reads nothing, so the path itself is
      // what goes back — shown, and quoted into the line.
      projectDir = dir.path;
    }
  }

  // The build runs on *this* machine — it is your source tree, your
  // toolchain, and your `make` deciding what to compile. What goes to the
  // mesh is each compile, one at a time, through the wrapper.
  //
  // So it is not a mesh terminal (DIR-8). It used to be one, opened on this
  // node, and a real Director refuses those — its node accepts no
  // terminals, and one that does not contribute compute is `no-execute` —
  // so the button only ever worked against the mock. `build_run` hosts it
  // here instead, as you, the way your own terminal would; the desktop
  // asks once per build line before it runs anything.
  const selfNode = $derived(app.selfNode?.name ?? "");

  function pct(v: number | null): string {
    if (v === null) return "—";
    return `${Math.round(v * 100)}%`;
  }

  const bytes = fmtBytes;
</script>

<div class="space-y-4 fade-in">
  <!-- ======================== the compiler wrapper ====================== -->
  <div class="card p-[22px]" use:staggerIn>
    <div class="flex flex-wrap items-start justify-between gap-3">
      <div>
        <h3 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Compiler wrapper</h3>
        <p class="mt-1 max-w-[70ch] text-sm leading-[1.55] text-ink-2">
          Works with any Makefile, CMake build, or anything else that calls a compiler. You
          change one variable; nothing else in your project changes, and nothing about it
          needs to know meshware exists.
        </p>
      </div>
    </div>

    <div class="mt-4 grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto]">
      <div class="min-w-0">
        <span class="micro">Project folder</span>
        <div class="mt-1.5 flex gap-2">
          <input
            bind:value={projectDir}
            placeholder="~/src/myproject"
            spellcheck="false"
            class={`${ui.inputMono} min-w-0`}
          />
          <button class={ui.secondary} onclick={pickProject}>Choose…</button>
        </div>
      </div>
      <div class="min-w-0 sm:w-[260px]">
        <span class="micro">Build command</span>
        <input
          bind:value={buildCommand}
          oninput={() => (commandTouched = true)}
          placeholder={`make -j${suggestedJ}`}
          spellcheck="false"
          class={`${ui.inputMono} mt-1.5`}
        />
      </div>
    </div>

    {#if lineErr}
      <div class="mt-3 rounded-sm px-3 py-2 text-xs" style="background: var(--status-error-wash); color: var(--status-error-ink);">
        {lineErr}
      </div>
    {/if}

    {#if line}
      {#each line.warnings as w}
        <div class="mt-3 flex items-start gap-2 rounded-sm px-3 py-2 text-xs" style="background: var(--status-warn-wash); color: var(--status-warn-ink);">
          <Icon name="alert" size={13} />
          <span>{w}</span>
        </div>
      {/each}

      <div class="mt-4">
        <div class="mb-1.5 flex items-center justify-between gap-3">
          <!-- Shown, copyable, and executed verbatim. If the Run button did
               something other than this, the panel would be lying. -->
          <span class="micro">What will run</span>
          <button class="copy-btn" onclick={() => copy("cc", line?.line ?? "")}>
            <Icon name={copied === "cc" ? "check" : "copy"} size={12} />
            <span>{copied === "cc" ? "Copied" : "Copy"}</span>
          </button>
        </div>
        <pre class="snippet snippet-wrap">{line.line}</pre>
      </div>
    {/if}

    <div class="mt-4 flex flex-wrap items-center justify-end gap-3">
      {#if !selfNode}
        <span class="text-xs text-ink-5">This node is still starting up.</span>
      {:else if !line}
        <span class="text-xs text-ink-5">Choose a project folder to run it from here.</span>
      {/if}
      <button
        class={ui.primary}
        disabled={!line || !selfNode}
        onclick={() => {
          running = true;
          runKey += 1;
        }}
      >
        {running ? "Run again" : "Run build"}
      </button>
    </div>

    {#if running && line && selfNode}
      <div class="fade-in mt-4">
        <!-- Keyed so "Run again" is a new session rather than a second
             command typed into a shell that already exited. -->
        {#key runKey}
          <Terminal
            node={selfNode}
            command={line.line}
            label={buildCommand}
            reason={`build farm: ${buildCommand}`}
            open={(size, onEvent) =>
              api.buildRun({ dir: projectDir, command: buildCommand, ...size }, onEvent)}
            onexit={() => {
              /* The panel keeps its transcript: the last screen of a build
                 is the reason anybody watched it. */
            }}
          />
        {/key}
      </div>
    {/if}

    <div class="mt-4 grid gap-x-6 gap-y-3 sm:grid-cols-3">
      {#each [["1", "Preprocessed here", "Your headers, your macros — so the machine that compiles it needs none of your project."], ["2", "Compiled anywhere", "Whichever node is free takes it. A 32-core box takes about twice the work of a 16-core one."], ["3", "Remembered", "Keyed by the content of the unit and the flags, so the same compile never happens twice in this mesh."]] as [n, head, body] (n)}
        <div class="step">
          <span class="step-n">{n}</span>
          <div>
            <div class="text-sm font-semibold text-ink">{head}</div>
            <p class="mt-0.5 text-xs leading-[1.5] text-ink-5">{body}</p>
          </div>
        </div>
      {/each}
    </div>
  </div>

  <!-- ============================ the cache ============================= -->
  <div class="card p-[22px]" use:staggerIn>
    <div class="flex flex-wrap items-start justify-between gap-3">
      <div>
        <h3 class="m-0 text-md font-bold tracking-[-.015em] text-ink">What the mesh remembers</h3>
        <p class="mt-1 max-w-[70ch] text-sm leading-[1.55] text-ink-2">
          Every compile shares one cache. A result stored by any machine answers the same work
          asked anywhere else in the mesh — which is the part that gets better as more people
          join, rather than just bigger.
        </p>
      </div>
    </div>

    {#if loading}
      <p class="mt-4 text-sm text-ink-5">Reading cache statistics…</p>
    {:else if statsErr}
      <div class="mt-4 note note-warn">
        <Icon name="alert" size={14} />
        <p>Could not read cache statistics: {statsErr}</p>
      </div>
    {:else if stats && !stats.enabled}
      <div class="mt-4 note">
        <Icon name="info" size={14} />
        <p>
          The action cache is switched off on this node, so every compile runs even when the
          mesh has done it before. Turn it on under Settings to make the second build fast.
        </p>
      </div>
    {:else if stats}
      <div class="mt-4 grid gap-3 sm:grid-cols-4">
        {#each [["Hit rate", pct(stats.hitRate), stats.hits + stats.misses === 0 ? "nothing asked yet" : `${stats.hits} of ${stats.hits + stats.misses} asks`], ["Work skipped", bytes(stats.bytesSaved), "output served instead of recomputed"], ["Entries held", `${stats.entries}`, `of ${stats.capacity} before eviction`], ["Published", `${stats.published}`, "results this node stored for others"]] as [label, value, sub] (label)}
          <div class="stat">
            <span class="micro">{label}</span>
            <span class="stat-v">{value}</span>
            <span class="stat-s">{sub}</span>
          </div>
        {/each}
      </div>

      {#if stats.mismatched > 0}
        <div class="mt-3 note note-warn">
          <Icon name="alert" size={14} />
          <p>
            <strong>{stats.mismatched}</strong>
            {stats.mismatched === 1 ? "entry" : "entries"} failed spot-checking — a cached answer
            disagreed with a fresh run of the same work. This is not a performance number; it means
            something is producing different output from identical inputs. Worth investigating before
            trusting the cache further.
          </p>
        </div>
      {/if}
    {/if}
  </div>

  <p class="px-1 text-xs text-ink-5">Remote execution for Bazel and Buck2 is on the roadmap.</p>
</div>

<style>
  .micro {
    font-size: var(--text-2xs);
    font-weight: 700;
    letter-spacing: 0.07em;
    text-transform: uppercase;
    color: var(--ink-muted);
  }

  .snippet {
    margin: 0;
    padding: 11px 13px;
    border: 1px solid var(--edge-pre);
    border-radius: var(--radius-md);
    background: var(--surface-inline);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    line-height: 1.6;
    color: var(--ink-2);
    overflow-x: auto;
    white-space: pre;
  }

  /* The composed build line is long by nature — an absolute wrapper path, an
     absolute home, and the command itself. Scrolling it sideways hides the
     half that matters (`CXX=`, and whatever the build command was), so this
     one wraps. A scoped class rather than a utility because `.snippet` is
     scoped too, and would win. */
  .snippet-wrap {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    overflow-x: visible;
  }

  .copy-btn {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 3px 8px;
    border: 1px solid var(--edge-strong);
    border-radius: var(--radius-md);
    font-size: var(--text-2xs);
    font-weight: 500;
    color: var(--ink-5);
    cursor: pointer;
    transition:
      color var(--dur-fast) var(--ease),
      border-color var(--dur-fast) var(--ease);
  }
  .copy-btn:hover {
    color: var(--accent-ink);
    border-color: var(--edge-hover);
  }

  .step {
    display: flex;
    gap: 9px;
    align-items: flex-start;
  }
  .step-n {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    flex: none;
    border-radius: 999px;
    background: var(--accent-wash);
    color: var(--accent-ink);
    font-size: var(--text-2xs);
    font-weight: 700;
  }

  .note {
    display: flex;
    gap: 9px;
    align-items: flex-start;
    padding: 11px 13px;
    border: 1px solid var(--edge-card);
    border-left: 2px solid var(--accent);
    border-radius: 0 var(--radius-md) var(--radius-md) 0;
    background: var(--surface-subtle);
    color: var(--ink-3);
  }
  .note p {
    margin: 0;
    font-size: var(--text-sm);
    line-height: 1.55;
    color: var(--ink-2);
  }
  .note-warn {
    border-left-color: var(--status-warn-ink);
  }

  .stat {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 11px 13px;
    border: 1px solid var(--edge-inner);
    border-radius: var(--radius-md);
    background: var(--surface-subtle);
  }
  .stat-v {
    font-size: var(--text-lg);
    font-weight: 700;
    letter-spacing: -0.02em;
    color: var(--ink-1);
    font-variant-numeric: tabular-nums;
  }
  .stat-s {
    font-size: var(--text-xs);
    line-height: 1.45;
    color: var(--ink-5);
  }
</style>
