<script lang="ts">
  // Live view of a per-item run (MASTERPLAN M9 §6.3). Three things, in the
  // order an operator needs them: how far along, who is doing the work, and
  // what went wrong. The per-node bars are the point — a bigger node
  // visibly drawing more of the work, with nobody having decided that.
  import { api, errText, fmtBytes, type NodeProgressView } from "../api";
  import type { DurationRangeView } from "../bindings/DurationRangeView";

  export type ItemResult = {
    item: string;
    exitCode: number | null;
    stdout: string;
    stderr: string;
    node: string;
    /** Where the output stayed, when it was kept in the mesh (M9 §5.5). */
    output: { id: string; size: number; holder: string } | null;
  };

  export type ReduceResult = {
    nodeName: string;
    exitCode: number | null;
    stdout: string;
    stderr: string;
    inputs: number;
  };

  let {
    total,
    done,
    failed,
    inflight,
    chunkItems,
    speculated,
    perNode,
    results,
    reduce,
    workers,
    declined,
    estimate,
    jobId,
    elapsedMicros,
    running,
    onRetry,
  }: {
    total: number;
    done: number;
    failed: number;
    inflight: number;
    chunkItems: number;
    /** Chunks re-run elsewhere to beat a straggler (M9 §5.6). */
    speculated: number;
    perNode: NodeProgressView[];
    results: ItemResult[];
    reduce: ReduceResult | null;
    workers: string[];
    declined: string[];
    /** Roughly how long this will take, from what runs like it took (R5 P4). */
    estimate: DurationRangeView | null;
    /** The run, so a completed save can release its held results (R3 P3). */
    jobId: string | null;
    elapsedMicros: number;
    running: boolean;
    onRetry: () => void;
  } = $props();

  // -- saving the outputs (R3 P1) ---------------------------------------------
  //
  // "Keep each item's output in the mesh" produced objects that only the CLI
  // could fetch, on a fifteen-minute expiry. This is the control that was
  // missing: without it, ticking that box on a transcode lost the work.
  const withOutput = $derived(results.filter((r) => r.output !== null));
  let saving = $state(false);
  let savedTo = $state<string | null>(null);
  let saveErr = $state<string | null>(null);

  async function saveOutputs() {
    saveErr = null;
    savedTo = null;
    const dir = await api.pickDirectory("Save the results into…");
    if (!dir) return;
    saving = true;
    try {
      const n = await api.parallelSaveOutputs(
        dir.token,
        withOutput.map((r) => ({ item: r.item, id: r.output!.id, holder: r.output!.holder })),
        jobId,
      );
      savedTo = `${n.toLocaleString()} file${n === 1 ? "" : "s"} → ${dir.path}`;
    } catch (e) {
      saveErr = errText(e);
    } finally {
      saving = false;
    }
  }

  function secs(n: number): string {
    if (n < 60) return `${n}s`;
    if (n < 3600) return `${Math.round(n / 60)}m`;
    const h = Math.floor(n / 3600);
    const m = Math.round((n % 3600) / 60);
    return m ? `${h}h ${m}m` : `${h}h`;
  }

  const bytes = fmtBytes;

  const kept = $derived(results.filter((r) => r.output));
  const keptBytes = $derived(kept.reduce((n, r) => n + (r.output?.size ?? 0), 0));

  let showAll = $state(false);

  const queued = $derived(Math.max(0, total - done - inflight));
  const donePct = $derived(total ? (done / total) * 100 : 0);
  const inflightPct = $derived(total ? (inflight / total) * 100 : 0);
  const failedPct = $derived(total ? (failed / total) * 100 : 0);
  const peakRate = $derived(Math.max(1, ...perNode.map((n) => n.itemsPerSec)));
  const failures = $derived(results.filter((r) => r.exitCode !== 0));
  const shown = $derived(showAll ? results : failures);
</script>

<div class="space-y-4">
  <!-- how far along -->
  <div class="card p-[22px]">
    <div class="flex items-baseline gap-3">
      <span class="num text-2xl font-semibold tracking-[-.02em] text-ink">{done.toLocaleString()}</span>
      <span class="text-sm text-ink-5">of {total.toLocaleString()} items</span>
      {#if elapsedMicros > 0}
        <span class="num ml-auto text-sm text-ink-5">{(elapsedMicros / 1e6).toFixed(1)}s</span>
      {/if}
    </div>

    <div class="mt-3 flex h-2.5 overflow-hidden rounded-full bg-surface-2">
      <div class="h-full" style={`width:${Math.max(0, donePct - failedPct)}%; background: var(--accent);`}></div>
      <div class="h-full" style={`width:${failedPct}%; background: var(--status-error);`}></div>
      <div class="h-full" style={`width:${inflightPct}%; background: var(--accent-wash);`}></div>
    </div>
    <div class="mt-2 flex flex-wrap gap-4 text-xs text-ink-5">
      <span>{(done - failed).toLocaleString()} done</span>
      <span>{inflight.toLocaleString()} in flight</span>
      <span>{queued.toLocaleString()} queued</span>
      <span style={failed > 0 ? "color: var(--status-error-ink);" : ""}>{failed.toLocaleString()} failed</span>
      {#if chunkItems > 0}<span>{chunkItems.toLocaleString()} per chunk</span>{/if}
    </div>

    <!-- A re-run is mitigation, not a fault. Naming it that way is the
         difference between "the mesh is working around a slow node" and
         "something went wrong twice" (M9 §5.6). -->
    {#if speculated > 0}
      <div class="mt-2.5 rounded-sm px-3 py-2 text-xs" style="background: var(--surface-2); color: var(--ink-4);">
        {speculated.toLocaleString()} chunk{speculated === 1 ? "" : "s"} re-run elsewhere to get
        past a slow node — whichever copy finished first is the one counted.
      </div>
    {/if}

    <!-- Who is on this job. A one-node run is fine for small work and a
         broken mesh for large work; only naming them tells the two apart. -->
    {#if workers.length > 0}
      <div class="mt-3 text-xs text-ink-5">
        on {workers.length} node{workers.length === 1 ? "" : "s"}: {workers.join(", ")}
      </div>
    {/if}
    <!-- Roughly how long, before somebody decides whether to wait (R5 P4).
         A range and the number of runs behind it, never a single time: an
         estimate that says "17 minutes" is wrong often enough to be
         untrusted forever, and one built from two runs should say so. -->
    {#if running && estimate && estimate.samples > 0}
      <div class="mt-1.5 text-xs text-ink-5">
        about {secs(estimate.p10Secs)}–{secs(estimate.p90Secs)}
        <span class="text-ink-6">(usually {secs(estimate.p50Secs)}, from {estimate.samples.toLocaleString()}
          past run{estimate.samples === 1 ? "" : "s"})</span>
      </div>
    {/if}
    {#each declined as reason}
      <div class="mt-1.5 rounded-sm px-3 py-2 text-xs" style="background: var(--surface-2); color: var(--status-warn-ink);">
        sitting this one out — {reason}
      </div>
    {/each}
  </div>

  <!-- where the output went, when it stayed in the mesh (M9 §5.5) -->
  {#if kept.length > 0}
    <div class="card p-[22px]">
      <!-- R6 P4: said as results waiting for you, not as a storage tier.
           "References, not bytes" is true and is the wrong sentence to meet
           first: what a person needs to know is that the files exist, are
           not here yet, and can be fetched. -->
      <div class="text-base font-semibold tracking-[-.01em] text-ink">Your results</div>
      <div class="mt-2 text-sm text-ink-4">
        <span class="num font-semibold text-ink">{bytes(keptBytes)}</span>
        across {kept.length.toLocaleString()} file{kept.length === 1 ? "" : "s"}, each still on the
        machine that produced it. Save them to bring them here.
      </div>
      <div class="mt-2 text-xs text-ink-5">
        Nothing was copied through this machine on the way — a merge step reads them
        straight from the machines that hold them.
      </div>
    </div>
  {/if}

  <!-- the reduce's answer (M9 §5.6) -->
  {#if reduce}
    <div class="card p-[22px]">
      <div class="flex items-baseline gap-3">
        <span class="text-base font-semibold tracking-[-.01em] text-ink">Reduced</span>
        <span class="text-xs text-ink-5">
          {reduce.inputs.toLocaleString()} output{reduce.inputs === 1 ? "" : "s"} merged on {reduce.nodeName}
          — the node that already held most of them
        </span>
        {#if reduce.exitCode !== 0}
          <span class="ml-auto num text-xs" style="color: var(--status-error-ink);">
            exit {reduce.exitCode === null ? "—" : reduce.exitCode}
          </span>
        {/if}
      </div>
      {#if (reduce.stdout || reduce.stderr).trim()}
        <pre class="selectable mono mt-3 max-h-64 overflow-auto rounded-md bg-canvas p-3 text-sm leading-normal text-ink">{(reduce.stdout || reduce.stderr).trim()}</pre>
      {/if}
    </div>
  {/if}

  <!-- who is doing the work -->
  {#if perNode.length > 0}
    <div class="card p-[22px]">
      <div class="text-base font-semibold tracking-[-.01em] text-ink">Per-node throughput</div>
      <div class="mt-3 space-y-2">
        {#each [...perNode].sort((a, b) => b.items - a.items) as n (n.nodeId)}
          <div class="flex items-center gap-2.5">
            <span class="w-28 shrink-0 truncate text-sm text-ink-3">{n.name}</span>
            <div class="h-2 flex-1 overflow-hidden rounded-full bg-surface-2">
              <div class="h-full rounded-full" style={`width:${(n.itemsPerSec / peakRate) * 100}%; background: var(--accent);`}></div>
            </div>
            <span class="num w-20 shrink-0 text-right text-xs text-ink-5">{n.itemsPerSec.toFixed(1)}/s</span>
            <span class="num w-24 shrink-0 text-right text-xs text-ink-4">{n.items.toLocaleString()} · {n.sharePct.toFixed(0)}%</span>
          </div>
        {/each}
      </div>
      <div class="mt-3 text-xs text-ink-5">
        Nobody assigned these shares. Each node pulls the next chunk when it
        finishes one, so a bigger node simply asks more often.
      </div>
    </div>
  {/if}

  <!-- what went wrong (and, on request, everything) -->
  {#if results.length > 0}
    <div class="card p-[22px]">
      <div class="flex items-center gap-3">
        <span class="text-base font-semibold tracking-[-.01em] text-ink">
          {#if failures.length > 0}
            {failures.length.toLocaleString()} failed
          {:else}
            Every item succeeded
          {/if}
        </span>
        <button
          class="text-xs font-medium text-ink-5 hover:text-accent-ink"
          onclick={() => (showAll = !showAll)}
        >
          {showAll ? "show only failures" : `show all ${results.length.toLocaleString()}`}
        </button>
        <div class="ml-auto flex items-center gap-2">
          {#if withOutput.length > 0}
            <button
              class="h-8 rounded-sm border border-edge-dashed px-3 text-xs font-semibold text-ink-3 transition-colors hover:border-accent hover:text-accent-ink disabled:cursor-not-allowed disabled:opacity-40"
              disabled={saving || running}
              title="Fetch every item's output and write it here, keeping the name it came in with"
              onclick={saveOutputs}
            >
              {saving
                ? "Saving…"
                : `Save ${withOutput.length.toLocaleString()} result${withOutput.length === 1 ? "" : "s"}…`}
            </button>
          {/if}
          {#if failures.length > 0}
            <button
              class="h-8 rounded-sm border border-edge-dashed px-3 text-xs font-semibold text-ink-3 transition-colors hover:border-accent hover:text-accent-ink disabled:cursor-not-allowed disabled:opacity-40"
              disabled={running}
              onclick={onRetry}
            >
              Re-run just these
            </button>
          {/if}
        </div>
      </div>

      {#if withOutput.length > 0 && !savedTo && !saveErr && !saving}
        <!-- The expiry is the reason this line exists: an object outlives
             its job by fifteen minutes, so "I will get them later" is not
             true and the screen should not imply that it is. -->
        <div class="mt-2 text-xs text-ink-5">
          {withOutput.length.toLocaleString()} result{withOutput.length === 1 ? " is" : "s are"} held
          on the nodes that produced {withOutput.length === 1 ? "it" : "them"}. Save
          {withOutput.length === 1 ? "it" : "them"} before they expire.
        </div>
      {/if}
      {#if savedTo}
        <div class="mt-2 text-xs" style="color: var(--accent-ink);">{savedTo}</div>
      {/if}
      {#if saveErr}
        <div class="mt-2 text-xs" style="color: var(--status-error);">{saveErr}</div>
      {/if}

      {#if shown.length > 0}
        <div class="mt-3 max-h-[420px] overflow-y-auto">
          <table class="w-full text-left text-base">
            <thead>
              <tr class="border-b border-edge">
                <th class="label px-2 py-2">Item</th>
                <th class="label px-2 py-2">Node</th>
                <th class="label px-2 py-2 text-right">Exit</th>
                <th class="label px-2 py-2">Output</th>
              </tr>
            </thead>
            <tbody>
              {#each shown.slice(-500) as r, i (i)}
                <tr class="border-b border-edge last:border-0">
                  <td class="mono px-2 py-1.5 text-xs text-ink">{r.item}</td>
                  <td class="px-2 py-1.5 text-xs text-ink-4">{r.node}</td>
                  <td
                    class="num px-2 py-1.5 text-right text-xs"
                    style={r.exitCode === 0 ? "color: var(--ink-6);" : "color: var(--status-error-ink);"}
                  >
                    {r.exitCode === null ? "—" : r.exitCode}
                  </td>
                  <td class="mono px-2 py-1.5 text-xs text-ink-4">
                    {#if r.output}
                      <span class="block max-w-[460px] truncate" title={r.output.id}>
                        {r.output.id.slice(0, 16)}… · {bytes(r.output.size)} on {r.node}
                      </span>
                    {:else}
                      <span class="block max-w-[460px] truncate">{(r.stderr || r.stdout).trim() || "—"}</span>
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        {#if shown.length > 500}
          <div class="mt-2 text-xs text-ink-5">Showing the last 500 of {shown.length.toLocaleString()}.</div>
        {/if}
      {/if}
    </div>
  {/if}
</div>
