<script lang="ts">
  // Ports (N2 P1) — what this node lends, and what is holding it.
  //
  // **This node, and the panel says so.** A dynamic port is chosen on the
  // executor at spawn and does not replicate until the service registry
  // lands (N3), so there is no mesh-wide answer to give — and inventing one
  // by fanning out to peers would turn a read an operator expects to be
  // instant into a network round trip per node. Director embeds a node, so
  // the ledger shown is the machine the app is running on, which is the one
  // a person sitting at it is asking about.
  //
  // The one distinction the panel must never blur: **"0 free" and
  // "advertises no range" mean opposite things.** The first is a node that
  // is full; the second is a node that takes no port work at all, usually
  // because it is running an older build or its operator typo'd
  // `[network] dynamic_ports`. An operator debugging a job that will not
  // place needs to know which one they are looking at.
  //
  // Laid out the way the Fleet panel is (2026-09): the three numbers as
  // metric tiles, then the holders as a grouped table with a section head
  // that counts them. The old single card put the headline, the range, a
  // meter and the reserved list in one stack that read as a paragraph.
  import { api, errText, type PortLedgerViewT } from "../lib/api";
  import { poll } from "../lib/poll";
  import { staggerIn } from "../lib/motion";
  import { hueOf, hueStyle } from "../lib/hues";
  import LoadError from "../lib/components/LoadError.svelte";
  import Pill from "../lib/components/Pill.svelte";
  import Ring from "../lib/components/Ring.svelte";
  import Sheet from "../lib/components/Sheet.svelte";
  import SummaryCard from "../lib/components/SummaryCard.svelte";

  /** `compact` draws the Capacity card; `open` is its detail sheet, bindable
   *  so a deep link to the retired Ports panel lands on it (2026-09-22). */
  let { compact = false, open = $bindable(false) }: { compact?: boolean; open?: boolean } = $props();

  let ledger = $state<PortLedgerViewT | null>(null);
  let err = $state<string | null>(null);

  async function load() {
    try {
      ledger = await api.ports();
      err = null;
    } catch (e) {
      err = errText(e);
    }
  }

  // Holds come and go with jobs, so this breathes with the work.
  $effect(() => poll(load, 2000));

  const total = $derived(
    ledger && ledger.rangeMin != null && ledger.rangeMax != null
      ? Math.max(0, ledger.rangeMax - ledger.rangeMin + 1)
      : 0,
  );
  const usedPct = $derived(total > 0 ? Math.min(100, ((total - (ledger?.free ?? 0)) / total) * 100) : 0);
  const byJobs = $derived((ledger?.held ?? []).filter((h) => h.kind === "job").length);
  const byGangs = $derived((ledger?.held ?? []).filter((h) => h.kind === "gang").length);

  /** The held list in port order — the number is what somebody is
   *  matching against a `ss -ltn` or a firewall rule. */
  const held = $derived([...(ledger?.held ?? [])].sort((a, b) => a.port - b.port));

  const fmt = (n: number) => n.toLocaleString();

  /** One line for the card — "0 free" and "no range" are opposite facts,
   *  and the card must not blur them any more than the ledger does. */
  const cardSub = $derived(
    !ledger
      ? err
        ? "could not read the ledger"
        : "loading the ledger…"
      : !ledger.range
        ? "advertises no dynamic port range"
        : `${held.length === 0 ? "nothing" : fmt(held.length)} in use on ${ledger.node}`,
  );
</script>

{#snippet full()}

{#if err}
  <LoadError class="mb-3" what="this node's ports" error={err} retry={load} />
{/if}

{#if ledger}
  <div class="section-head">
    <span class="section-title">{ledger.node}</span>
    <span class="section-sum">on this machine{ledger.range ? ` · hands out ports ${ledger.range}` : ""}</span>
  </div>

  {#if ledger.range}
    <div class="mb-5 grid gap-3" style="grid-template-columns: repeat(3, minmax(0, 1fr));">
      <div class="card metric" title="Ports this machine hands out that nothing is using">
        <div class="metric-label">Free</div>
        <div class="metric-fig">{fmt(ledger.free)}<span class="sub">of {fmt(total)}</span></div>
        <div class="meter-track mt-[13px] h-[5px]">
          <div class="meter-fill" style={`width:${Math.max(usedPct, ledger.free < total ? 1.5 : 0)}%; background: var(--accent);`}></div>
        </div>
        <div class="metric-note">{usedPct < 1 ? "under 1%" : `${Math.round(usedPct)}%`} in use</div>
      </div>

      <div class="card metric" title="Ports a job, or a multi-machine job waiting to start, is holding right now">
        <div class="metric-label">In use</div>
        <div class="metric-fig">{fmt(held.length)}<span class="sub">right now</span></div>
        <div class="metric-note">
          {#if held.length === 0}
            nothing is holding a port
          {:else}
            {byJobs} by {byJobs === 1 ? "a job" : "jobs"}{byGangs > 0 ? ` · ${byGangs} held for multi-machine ${byGangs === 1 ? "job" : "jobs"}` : ""}
          {/if}
        </div>
      </div>

      <div class="card metric" title="Ports this node will never hand out, whatever the range says">
        <div class="metric-label">Reserved</div>
        <div class="metric-fig">{fmt(ledger.reserved.length)}<span class="sub">never handed out</span></div>
        <div class="metric-note flex flex-wrap gap-1.5">
          {#if ledger.reserved.length === 0}
            none
          {:else}
            {#each ledger.reserved as p (p)}
              <span class="chip">{p}</span>
            {/each}
          {/if}
        </div>
      </div>
    </div>
  {:else}
    <!-- Not "0 free". A node that advertises no range is disqualified
         from port work entirely, which is a different problem with a
         different fix. -->
    <div class="card mb-5 px-5 py-4" style="border-left: 3px solid var(--status-error);">
      <div class="text-sm font-semibold" style="color: var(--status-error-ink);">
        This node advertises no dynamic port range
      </div>
      <div class="mt-1 text-sm leading-relaxed text-ink-4">
        It will take no work that needs a port. Set
        <code class="num">[network] dynamic_ports</code> in meshnode.toml — a
        malformed value advertises nothing rather than falling back to the
        default, so a typo lands here.
      </div>
      {#if ledger.reserved.length > 0}
        <div class="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-ink-5">
          reserved, never handed out:
          {#each ledger.reserved as p (p)}<span class="chip">{p}</span>{/each}
        </div>
      {/if}
    </div>
  {/if}

  <div class="section-head">
    <span class="section-title">Holding a port</span>
    <span class="section-sum">{held.length === 0 ? "nobody" : `${held.length} port${held.length === 1 ? "" : "s"}`}</span>
  </div>

  {#if held.length === 0}
    <div class="card empty">
      <div class="empty-title">Nothing is holding a port</div>
      <div class="empty-sub">
        A job asks with <code class="num">--port http:9095</code>, a deployment
        with a <code class="num">[deployment.network]</code> block. The
        allocation reaches the job as
        <code class="num">MESHWARE_PORT_HTTP</code>.
      </div>
    </div>
  {:else}
    <div class="card overflow-hidden">
      <table class="w-full text-sm">
        <thead>
          <tr class="border-b border-edge">
            <th class="label w-[110px] px-5 py-2.5 text-right">Port</th>
            <th class="label w-[140px] px-3 py-2.5 text-left">Label</th>
            <th class="label w-[120px] px-3 py-2.5 text-left">Held as</th>
            <th class="label px-5 py-2.5 text-left">Holder</th>
          </tr>
        </thead>
        <tbody use:staggerIn={{ selector: ":scope > tr", each: 12, y: 4 }}>
          {#each held as h (h.port)}
            <tr class="border-b border-edge/60 last:border-0">
              <td class="num px-5 py-2.5 text-right font-semibold text-ink">{h.port}</td>
              <td class="px-3 py-2.5">
                <!-- The label is what the job reads it back as
                     (`MESHWARE_PORT_HTTP`), so it takes a stable hue per
                     name like a tag does: every `http` the same colour. -->
                <span class="hue-chip" style={hueStyle(hueOf(h.label))}>{h.label}</span>
              </td>
              <td class="px-3 py-2.5">
                {#if h.kind === "gang"}
                  <Pill tone="info" title="Held by a reservation that expires on its own clock">reservation</Pill>
                {:else}
                  <Pill tone="running" live title="Held by a running job; released the instant it exits">job</Pill>
                {/if}
              </td>
              <td class="mono px-5 py-2.5 text-xs text-ink-3">{h.holder}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <div class="footnote">
      A job asks with <code class="num">--port http:9095</code>; the number it
      was given reaches it as <code class="num">MESHWARE_PORT_&lt;LABEL&gt;</code>.
    </div>
  {/if}
{:else if !err}
  <div class="text-xs text-ink-5">loading the ledger…</div>
{/if}
{/snippet}

{#if !compact}
  {@render full()}
{:else}
  <SummaryCard
    icon="port"
    title="Ports"
    tone={ledger && !ledger.range ? "var(--status-error)" : "var(--hue-sky)"}
    sub={cardSub}
    onopen={() => (open = true)}
  >
    {#if ledger && ledger.range}
      <span class="flex items-center gap-4">
        <Ring
          size={68}
          thickness={8}
          total={total}
          arcs={[
            { value: ledger.free, color: "var(--status-ok)", label: `${fmt(ledger.free)} free` },
            { value: total - ledger.free, color: "var(--status-running)", label: `${fmt(total - ledger.free)} in use` },
          ]}
          label={`${fmt(ledger.free)} of ${fmt(total)} ports free`}
        >
          <span class="num text-[15px] font-bold text-ink">{fmt(held.length)}</span>
          <span class="mt-0.5 text-[10px] text-ink-5">in use</span>
        </Ring>
        <span class="min-w-0">
          <span class="big-fig block">{fmt(ledger.free)}<span class="unit">free of {fmt(total)}</span></span>
          <span class="mt-1 block text-xs text-ink-5">hands out <span class="mono">{ledger.range}</span></span>
        </span>
      </span>
      {#if ledger.reserved.length > 0}
        <span class="mt-3 flex flex-wrap items-center gap-1.5 text-xs text-ink-5">
          reserved
          {#each ledger.reserved as p (p)}<span class="chip">{p}</span>{/each}
        </span>
      {/if}
    {:else if ledger}
      <span class="block text-sm leading-snug" style="color: var(--status-error-ink);">
        Takes no work that needs a port. Set <code class="num">[network] dynamic_ports</code> in meshnode.toml.
      </span>
    {/if}
  </SummaryCard>

  <Sheet
    {open}
    title="Ports"
    sub="The ports this machine lends, and what is holding each one."
    icon="port"
    width={860}
    onclose={() => (open = false)}
  >
    {@render full()}
  </Sheet>
{/if}
