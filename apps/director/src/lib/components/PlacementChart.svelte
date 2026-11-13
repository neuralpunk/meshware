<script lang="ts">
  // The placement chart (C0–C9 §12.3 P2) — *the screenshot that sells the
  // product*.
  //
  // meshware has computed a weighted score over eight terms for every
  // candidate on every placement since M5, and discarded all of it except
  // the winner. "Why did my job land there?" is the most common
  // operational question in distributed computing, essentially no product
  // answers it, and this one had the answer the whole time.
  //
  // One stacked bar per candidate, one segment per term. Every term is a
  // *penalty* — lower is better, throughout — so the shortest bar wins and
  // there is no per-term sign for a reader to remember.
  import { type PlacementView, type ScoreTermView } from "../api";
  import Why from "./Why.svelte";

  let { placement }: { placement: PlacementView } = $props();

  // A fixed colour per term, so the same term is the same colour on every
  // job anyone ever looks at. A palette that reshuffles per chart is a
  // palette nobody can learn.
  const TERM_COLOUR: Record<string, string> = {
    load: "var(--accent)",
    warmth: "#8b5cf6",
    failure: "#ef4444",
    rtt: "#f59e0b",
    memory: "#3b82f6",
    disk: "#64748b",
    locality: "#ec4899",
    vantage: "#14b8a6",
    device: "#a855f7",
  };

  // What each term is, in one clause — and **not written here** any more
  // (L2 P4). This file used to carry its own copy of the meanings; the
  // engine now sends `gloss` on every term, from `mesh_proto::term_gloss()`,
  // so the chart, `meshnode jobs explain` and `--output json` say the same
  // thing about `warmth` and cannot drift apart the next time somebody
  // changes what it measures.
  const glossOf = $derived((name: string): string | null =>
    placement.candidates[0]?.terms.find((t) => t.name === name)?.gloss ?? null,
  );

  const worst = $derived(
    Math.max(...placement.candidates.map((c) => c.total), Number.EPSILON),
  );
  // Terms in the order the engine emits them, taken from the first
  // candidate: every candidate carries the same term list.
  const terms = $derived(placement.candidates[0]?.terms.map((t) => t.name) ?? []);
  // A term that contributes nothing anywhere is not drawn. Nine empty
  // segments on every ordinary job would make the interesting one invisible.
  const active = $derived(
    terms.filter((name) =>
      placement.candidates.some(
        (c) => (c.terms.find((t) => t.name === name)?.weighted ?? 0) > 0,
      ),
    ),
  );

  function segment(term: ScoreTermView): string {
    return `${(100 * term.weighted) / worst}%`;
  }
  function tooltip(node: string, term: ScoreTermView): string {
    const meaning = term.gloss ?? term.name;
    const unknown = term.unknown ? " · not measurable, scored neutral" : "";
    return `${node} — ${term.name}: ${meaning}\n${term.raw.toFixed(3)} × ${term.weight} = ${term.weighted.toFixed(3)}${unknown}`;
  }
</script>

<div class="flex flex-col gap-3">
  <div class="flex items-baseline justify-between text-2xs text-ink-5">
    <span>
      Scored {placement.considered} candidate{placement.considered === 1 ? "" : "s"};
      the top {placement.candidates.length} kept their breakdown.
    </span>
    <span class="mono">lower is better</span>
  </div>

  <div class="flex flex-col gap-1.5">
    {#each placement.candidates as candidate (candidate.node)}
      <div class="flex items-center gap-2">
        <div
          class="w-32 shrink-0 truncate text-xs"
          class:font-semibold={candidate.chosen}
          style={candidate.chosen ? "color: var(--ink-1)" : "color: var(--ink-4)"}
          title={candidate.name}
        >
          {#if candidate.chosen}<span style="color: var(--accent-ink)">→ </span>{/if}
          {candidate.name}
        </div>
        <div
          class="flex h-5 flex-1 overflow-hidden rounded-sm"
          style="background: var(--surface-2)"
        >
          {#each candidate.terms.filter((t) => t.weighted > 0) as term (term.name)}
            <div
              class="h-full"
              style="width: {segment(term)}; background: {TERM_COLOUR[term.name] ??
                'var(--ink-faint)'}; opacity: {term.unknown ? 0.45 : 1}"
              title={tooltip(candidate.name, term)}
            ></div>
          {/each}
        </div>
        <div class="mono w-14 shrink-0 text-right text-2xs text-ink-4">
          {candidate.total.toFixed(2)}
        </div>
      </div>
    {/each}
  </div>

  <!-- The legend carries the meaning, not just the colour: a reader who has
       to guess what "vantage" is cannot read the chart at all. -->
  <div class="flex flex-wrap gap-x-3 gap-y-1 text-2xs text-ink-5">
    {#each active as name (name)}
      <span class="flex items-center gap-1">
        <span
          class="inline-block h-2 w-2 rounded-full"
          style="background: {TERM_COLOUR[name] ?? 'var(--ink-faint)'}"
        ></span>
        <!-- The explanation is on the word it explains (L2 P4). A reader
             who has to guess what "vantage" is cannot read the chart. -->
        {#if glossOf(name)}
          <Why text={glossOf(name) ?? ""}>{name}</Why>
        {:else}
          {name}
        {/if}
      </span>
    {/each}
    <span class="flex items-center gap-1" title="Nothing could measure this input, so the neutral default stood in.">
      <span
        class="inline-block h-2 w-2 rounded-full"
        style="background: var(--ink-faint); opacity: 0.45"
      ></span>
      unmeasured
    </span>
  </div>

  {#if placement.spilled}
    <p class="text-2xs text-ink-5">
      The best candidate answered <span class="mono">Busy</span>, so this job moved to
      the next one — which is why the winner is not the shortest bar.
    </p>
  {/if}
  <!-- The one-sentence answer, composed by `PlacementExplain::why()` and
       identical to the one `meshnode jobs explain` prints. It replaces a
       paragraph this file used to write itself, which was a second
       explanation of one placement. -->
  {#if placement.why}
    <p class="text-2xs text-ink-5">{placement.why}</p>
  {/if}
</div>
