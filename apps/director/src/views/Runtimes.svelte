<script lang="ts">
  // Runtimes (T4) — what this machine can actually run, and what the rest
  // of the mesh can.
  //
  // **The panel that answers "why did my job not run here."** Before T3,
  // nothing anywhere said it. A python job went to a node with no
  // `python3`; a container job went to a node with no engine. Both were
  // placed successfully and both discovered the truth at spawn, as an
  // error with no scheduling consequence.
  //
  // Shaped after `nomad node status -verbose`, whose Drivers table is the
  // single most useful thing that output contains. The columns are the
  // three questions somebody debugging asks, in the order they ask them:
  // *can it*, *what did it find*, *what do I do about it*.
  //
  // Two distinctions this panel must never blur:
  //
  //  - **absent vs broken.** "Docker is installed but its daemon is
  //    stopped" has a different fix from "Docker is not installed".
  //    Collapsing them sends somebody to install what they already have.
  //  - **unknown vs none.** A node announcing no runtimes is on a build
  //    from before this existed. Placement treats it as able to run
  //    anything, so a panel that said "none" would contradict where the
  //    jobs actually go.
  //
  // The mesh half is a matrix (2026-09): nodes down, runtimes across, a
  // tick where the node announces it. The list of chips it replaces made
  // "which nodes can run containers" a row-by-row read; a column answers
  // it in one glance, which is the question an operator with a job that
  // will not place is actually asking.
  import { api, errText, type RuntimesViewT } from "../lib/api";
  import { poll } from "../lib/poll";
  import { staggerIn } from "../lib/motion";
  import LoadError from "../lib/components/LoadError.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Pill from "../lib/components/Pill.svelte";
  import Sheet from "../lib/components/Sheet.svelte";
  import SummaryCard from "../lib/components/SummaryCard.svelte";

  /**
   * `compact` draws the Capacity card (2026-09-22): the one line that
   * decides whether a job will place, and the whole ledger below one
   * click, in a sheet. `open` is that sheet, bindable so a deep link to
   * the retired Runtimes panel can open it.
   */
  let { compact = false, open = $bindable(false) }: { compact?: boolean; open?: boolean } = $props();

  let view = $state<RuntimesViewT | null>(null);
  let err = $state<string | null>(null);

  async function load() {
    try {
      view = await api.runtimes();
      err = null;
    } catch (e) {
      err = errText(e);
    }
  }

  // Slower than the port ledger, and deliberately: this changes when
  // somebody installs a package or a daemon falls over, not when a job
  // starts. Polling it every two seconds would spawn five processes on the
  // node every two seconds to learn nothing.
  $effect(() => poll(load, 15000));

  /** What a person calls each runtime, and what it needs. */
  const BLURB: Record<string, string> = {
    exec: "A binary from a bundle. Needs nothing but the kernel.",
    bash: "A shell script. Needs bash.",
    python: "A Python entry point, in a venv built from requirements.txt.",
    ansible: "A playbook, against an inventory of mesh nodes.",
    container: "An OCI image — nginx, postgres, your own build.",
  };

  // Warning, not error, for a runtime this machine does not have: that is
  // a fact about the machine, not a fault. Most nodes run none of ansible,
  // python or containers, and a panel that painted each one red would be
  // a panel of red nobody reads. "Not working" *is* a fault: somebody
  // meant it to work.
  const TONE: Record<string, { tone: "ok" | "muted" | "warn"; word: string }> = {
    ready: { tone: "ok", word: "ready" },
    absent: { tone: "muted", word: "not installed" },
    broken: { tone: "warn", word: "not working" },
  };

  const readyCount = $derived(view ? view.local.filter((r) => r.detection === "ready").length : 0);
  const brokenCount = $derived(view ? view.local.filter((r) => r.detection === "broken").length : 0);
  const announcing = $derived(view ? view.mesh.filter((n) => n.runtimes !== null).length : 0);

  /** The matrix's columns: this machine's probe order first, then anything
   *  another node announces that this one has never heard of. */
  const columns = $derived.by((): string[] => {
    if (!view) return [];
    const out = view.local.map((r) => r.runtime);
    for (const n of view.mesh) for (const r of n.runtimes ?? []) if (!out.includes(r)) out.push(r);
    return out;
  });

  /** How many nodes announce each runtime, for the column footer. */
  const perColumn = $derived.by((): Record<string, number> => {
    const out: Record<string, number> = {};
    if (!view) return out;
    for (const c of columns) out[c] = view.mesh.filter((n) => n.runtimes?.includes(c)).length;
    return out;
  });

  /** The card's chip colours: ready is well, broken is somebody's fault,
   *  absent is merely a fact about the machine and stays neutral. */
  const CHIP: Record<string, string> = {
    ready: "var(--status-ok)",
    broken: "var(--status-warn)",
    absent: "var(--ink-faint)",
  };
</script>

{#snippet full()}

{#if err}
  <LoadError class="mb-3" what="the runtimes" error={err} retry={load} />
{/if}

{#if view}
  <div class="section-head">
    <span class="section-title">{view.node}</span>
    <span class="section-sum">this machine, probed</span>
  </div>

  <div class="mb-5 grid gap-3" style="grid-template-columns: repeat(3, minmax(0, 1fr));">
    <div class="card metric" title="Kinds of work this machine was found able to run">
      <div class="metric-label">Ready</div>
      <div class="metric-fig">{readyCount}<span class="sub">of {view.local.length} runtimes</span></div>
      <div class="metric-bar">
        {#each view.local as r (r.runtime)}
          <span
            data-on={r.detection === "ready" ? "" : undefined}
            style={r.detection === "broken" ? "background: var(--status-warn);" : ""}
            title={`${r.runtime} · ${TONE[r.detection]?.word ?? r.detection}`}
          ></span>
        {/each}
      </div>
    </div>

    <div class="card metric" title="Installed, and not answering — the one state somebody meant to fix">
      <div class="metric-label">Not working</div>
      <div class="metric-fig" style={brokenCount > 0 ? "color: var(--status-warn-ink);" : ""}>
        {brokenCount}<span class="sub">{brokenCount === 0 ? "everything found is answering" : "installed but broken"}</span>
      </div>
      <div class="metric-note">Nothing here is configured. Install a tool and meshware finds it on the next restart.</div>
    </div>

    <div class="card metric" title="Nodes on a build new enough to say what they can run">
      <div class="metric-label">Announcing</div>
      <div class="metric-fig">{announcing}<span class="sub">of {view.mesh.length} node{view.mesh.length === 1 ? "" : "s"}</span></div>
      <div class="metric-note">
        {#if view.unknown.length === 0}
          Every node says what it can run, so placement never guesses.
        {:else}
          {view.unknown.length} older {view.unknown.length === 1 ? "build is" : "builds are"} treated as able to run anything.
        {/if}
      </div>
    </div>
  </div>

  <div class="section-head">
    <span class="section-title">On this machine</span>
    <span class="section-sum">{view.local.length} runtime{view.local.length === 1 ? "" : "s"} probed</span>
  </div>
  <div class="card mb-5 overflow-hidden">
    <table class="w-full text-sm">
      <thead>
        <tr class="border-b border-edge">
          <th class="label w-[240px] px-5 py-2.5 text-left">Runtime</th>
          <th class="label w-[130px] px-3 py-2.5 text-left">State</th>
          <th class="label w-[220px] px-3 py-2.5 text-left">Found</th>
          <th class="label px-5 py-2.5 text-left">What to do</th>
        </tr>
      </thead>
      <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 4 }}>
        {#each view.local as r (r.runtime)}
          {@const t = TONE[r.detection] ?? TONE.absent}
          <tr class="border-b border-edge/60 last:border-0" style={r.detection === "broken" ? "background: var(--status-warn-wash);" : ""}>
            <td class="px-5 py-3 align-top">
              <div class="mono font-semibold text-ink">{r.runtime}</div>
              <div class="mt-0.5 text-xs leading-snug text-ink-5">{BLURB[r.runtime] ?? ""}</div>
            </td>
            <td class="px-3 py-3 align-top"><Pill tone={t.tone}>{t.word}</Pill></td>
            <td class="mono px-3 py-3 align-top text-xs text-ink-3">{r.detail || "—"}</td>
            <!-- The remedy on the same row as the problem. A table that
                 makes somebody go and look up what "absent" means about
                 `ansible` has failed at the one job it has. -->
            <td class="px-5 py-3 align-top text-xs leading-relaxed text-ink-4">{r.remedy || "—"}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>

  {#if view.mesh.length > 1}
    <div class="section-head">
      <span class="section-title">Across the mesh</span>
      <span class="section-sum">{view.mesh.length} nodes · {columns.length} runtimes</span>
    </div>
    <div class="card overflow-hidden">
      <table class="w-full text-sm">
        <thead>
          <tr class="border-b border-edge">
            <th class="label w-[200px] px-5 py-2.5 text-left">Node</th>
            {#each columns as c (c)}
              <th class="label mono px-3 py-2.5 text-center">{c}</th>
            {/each}
          </tr>
        </thead>
        <tbody use:staggerIn={{ selector: ":scope > tr", each: 10, y: 4 }}>
          {#each view.mesh as n (n.node)}
            <tr class="border-b border-edge/60 last:border-0">
              <td class="px-5 py-2.5 align-middle">
                <div class="font-semibold text-ink">{n.node}</div>
                {#if n.runtimes === null}
                  <!-- Never "none": placement treats this node as able to
                       run anything, and a panel that said otherwise would
                       contradict where the jobs actually go. -->
                  <div class="text-xs text-ink-5">older build — announces nothing, still a candidate for everything</div>
                {:else if n.runtimes.length === 0}
                  <div class="text-xs" style="color: var(--status-warn-ink);">announces none — takes no work at all</div>
                {/if}
              </td>
              {#each columns as c (c)}
                <td class="px-3 py-2.5 text-center align-middle">
                  {#if n.runtimes === null}
                    <span class="mono text-xs text-ink-5" title="Unknown: this node does not announce its runtimes">?</span>
                  {:else if n.runtimes.includes(c)}
                    <span class="inline-grid h-5 w-5 place-items-center rounded-full" style="background: var(--status-ok-wash); color: var(--status-ok-ink);" title={`${n.node} can run ${c}`}>
                      <Icon name="check" size={11} />
                    </span>
                  {:else}
                    <span class="inline-block h-2 w-2 rounded-full" style="box-shadow: inset 0 0 0 1.5px var(--edge-strong);" title={`${n.node} does not run ${c}`}></span>
                  {/if}
                </td>
              {/each}
            </tr>
          {/each}
        </tbody>
        <tfoot>
          <tr class="border-t border-edge" style="background: var(--surface-subtle);">
            <td class="px-5 py-2 text-xs text-ink-5">nodes that can</td>
            {#each columns as c (c)}
              <td class="num px-3 py-2 text-center text-xs text-ink-4">{perColumn[c]}</td>
            {/each}
          </tr>
        </tfoot>
      </table>
    </div>
    <div class="footnote">
      This is what the scheduler reads: a job is only placed where its kind of work has a tick.
      {#if view.unknown.length > 0}
        <span class="num">{view.unknown.join(", ")}</span> {view.unknown.length === 1 ? "is" : "are"} treated as able to run anything until upgraded.
      {/if}
    </div>
  {/if}
{:else if !err}
  <div class="text-xs text-ink-5">probing this machine…</div>
{/if}
{/snippet}

{#if !compact}
  {@render full()}
{:else}
  <SummaryCard
    icon="box"
    title="Runtimes"
    tone={brokenCount > 0 ? "var(--status-warn)" : "var(--hue-plum)"}
    sub={view ? `${readyCount} of ${view.local.length} ready here${view.mesh.length > 1 ? ` · ${announcing} of ${view.mesh.length} nodes announce` : ""}` : err ? "could not probe this machine" : "probing this machine…"}
    onopen={() => (open = true)}
  >
    {#if view}
      <span class="flex flex-wrap gap-1.5">
        {#each view.local as r (r.runtime)}
          <span
            class="rt-chip"
            data-state={r.detection}
            style={`--c: ${CHIP[r.detection] ?? CHIP.absent};`}
            title={`${r.runtime} · ${TONE[r.detection]?.word ?? r.detection}`}
          >
            {#if r.detection === "ready"}<Icon name="check" size={11} />{:else}<span class="rt-dot"></span>{/if}
            {r.runtime}
          </span>
        {/each}
      </span>
      <span class="mt-3 block text-xs leading-snug text-ink-5">
        {#if brokenCount > 0}
          <span style="color: var(--status-warn-ink);">{brokenCount} installed but not answering.</span>
        {:else if view.unknown.length > 0}
          {view.unknown.length} older {view.unknown.length === 1 ? "build is" : "builds are"} treated as able to run anything.
        {:else}
          A job is only placed where its kind of work is ready.
        {/if}
      </span>
    {/if}
  </SummaryCard>

  <Sheet
    {open}
    title="Runtimes"
    sub="What each machine can actually run — the first thing to check when a job will not place."
    icon="box"
    width={900}
    onclose={() => (open = false)}
  >
    {@render full()}
  </Sheet>
{/if}

<style>
  .rt-chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    height: 24px;
    padding: 0 9px;
    border-radius: 999px;
    font-family: var(--font-mono);
    font-size: var(--text-2xs);
    font-weight: 600;
    background: color-mix(in srgb, var(--c) 14%, transparent);
    color: var(--ink-2);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--c) 26%, transparent);
  }
  .rt-chip[data-state="ready"] {
    color: var(--status-ok-ink);
  }
  .rt-chip[data-state="broken"] {
    color: var(--status-warn-ink);
  }
  .rt-chip[data-state="absent"] {
    background: transparent;
    color: var(--ink-5);
    box-shadow: inset 0 0 0 1px var(--edge-strong);
  }
  .rt-dot {
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: var(--c);
  }
</style>
