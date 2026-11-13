<script lang="ts">
  // Capacity — can this mesh take the work, and what is holding what it
  // lends (2026-09-22).
  //
  // Four panels became one on 2026-09-14 (`chrome/nav.ts`), and the one
  // they became was the four stacked end to end: nine metric tiles, five
  // tables and a matrix, 2,000px of page for a question most visits answer
  // in the first screenful. It is rebuilt as a glance and three doors:
  //
  //   - **Four gauges** — cores, slots, memory, disk — each a ring, because
  //     every one of them is a part of a whole and a ring is read before
  //     its number is.
  //   - **Three cards** — runtimes, ports, volumes — each saying the one
  //     thing that decides whether a job will place, and opening the full
  //     ledger in a sheet. The tables are unchanged; they moved one click
  //     in, where the person debugging a placement goes anyway.
  //   - **Per machine**, as bars, last: on a sixty-machine mesh it is the
  //     long part, and it must not push the three doors off the screen.
  //
  // This was `Compute.svelte` plus the stack in `Mesh.svelte`; the compute
  // half lives here now, and each ledger view keeps its own poll and draws
  // its own card (`compact`).
  import { fmtSizeMb } from "../lib/api";
  import { app, ago } from "../lib/state.svelte";
  import Hint from "../lib/components/Hint.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Ring from "../lib/components/Ring.svelte";
  import RuntimesView from "./Runtimes.svelte";
  import PortsView from "./Ports.svelte";
  import VolumesView from "./Volumes.svelte";
  import { staggerIn } from "../lib/motion";

  const t = $derived(app.totals);
  const mb = fmtSizeMb;

  // -- the retired panels' deep links ------------------------------------
  //
  // A link that named Runtimes, Ports or Volumes (`app.go` leaves the note
  // in `capacity:focus`) opens that card's ledger: the sheet is where the
  // panel it named went. Re-read on every navigation, so a second link
  // while already here still lands.
  let runtimesOpen = $state(false);
  let portsOpen = $state(false);
  let volumesOpen = $state(false);
  $effect(() => {
    void app.deepLink;
    const focus = sessionStorage.getItem("capacity:focus");
    if (!focus) return;
    sessionStorage.removeItem("capacity:focus");
    if (focus === "runtimes") runtimesOpen = true;
    else if (focus === "ports") portsOpen = true;
    else if (focus === "volumes") volumesOpen = true;
  });

  /**
   * "across N of M" — and only when N and M differ.
   *
   * A qualifier on every line trains people to stop reading qualifiers; one
   * that appears exactly when the number is a floor is what stands between
   * this screen and being quietly disbelieved the first time somebody
   * checks it against the rack.
   */
  function from(n: number): string {
    if (!t || n === t.nodesLive) return "";
    return `across ${n} of ${t.nodesLive}`;
  }

  /** Colour of a used-share arc: calm until it is worth looking at. */
  function tone(pct: number, calm: string): string {
    return pct >= 90 ? "var(--status-error)" : pct >= 75 ? "var(--status-warn)" : calm;
  }

  type Gauge = {
    id: string;
    icon: string;
    hue: string;
    label: string;
    /** 0–100, or null when nobody has said — drawn as an empty track. */
    pct: number | null;
    centre: string;
    fig: string;
    unit: string;
    note: string;
    title: string;
  };

  const gauges = $derived.by((): Gauge[] => {
    if (!t) return [];
    const busy = t.loadPerCore == null ? null : Math.round(Math.min(1, t.loadPerCore) * 100);
    const memPct =
      t.memAvailableFrom > 0 && t.memMb > 0
        ? Math.round(Math.min(100, Math.max(0, ((t.memMb - t.memAvailableMb) / t.memMb) * 100)))
        : null;
    const diskPct =
      t.diskFreeFrom > 0 && t.diskMb > 0
        ? Math.round(Math.min(100, Math.max(0, ((t.diskMb - t.diskFreeMb) / t.diskMb) * 100)))
        : null;
    return [
      {
        id: "cores",
        icon: "compute",
        hue: "var(--hue-indigo)",
        label: "Cores",
        pct: busy,
        centre: "busy",
        fig: `${t.cores}`,
        unit: "cores",
        note: busy == null ? from(t.coresFrom) || "no live load reading yet" : `${100 - busy}% idle${from(t.coresFrom) ? ` · ${from(t.coresFrom)}` : ""}`,
        title: "Mean one-minute load per core, over the machines with a fresh reading",
      },
      {
        id: "slots",
        icon: "jobs",
        hue: "var(--accent)",
        label: "Job slots",
        pct: t.slots > 0 ? t.saturationPct : null,
        centre: "in use",
        fig: `${t.slotsBusy}`,
        unit: `of ${t.slots} busy`,
        note: t.jobsQueued > 0 ? `${t.jobsQueued} waiting for a free slot` : "nothing waiting",
        title: "Slots, not cores, are what bounds concurrency — a policy number per machine",
      },
      {
        id: "memory",
        icon: "memory",
        hue: "var(--hue-sky)",
        label: "Memory",
        pct: memPct,
        centre: "used",
        fig: mb(t.memMb),
        unit: "",
        note: memPct == null ? from(t.memFrom) || "no live reading yet" : `${mb(t.memAvailableMb)} available${from(t.memFrom) ? ` · ${from(t.memFrom)}` : ""}`,
        title: "Memory each machine advertises, and what it says is available now",
      },
      {
        id: "disk",
        icon: "disk",
        hue: "var(--hue-amber)",
        label: "Disk",
        pct: diskPct,
        centre: "used",
        fig: mb(t.diskMb),
        unit: "",
        note: diskPct == null ? from(t.diskFrom) || "no live reading yet" : `${mb(t.diskFreeMb)} free${from(t.diskFrom) ? ` · ${from(t.diskFrom)}` : ""}`,
        title: "Disk pools across a mesh; memory does not",
      },
    ];
  });

  /** Live machines, busiest first — the ones a reader came here looking for. */
  const nodes = $derived(
    [...app.top]
      .filter((n) => n.liveness !== "dead")
      .sort((a, b) => b.saturationPct - a.saturationPct || a.name.localeCompare(b.name)),
  );
</script>

{#if !t}
  <div class="card empty">
    <div class="empty-title">Still counting</div>
    <div class="empty-sub">
      Capacity is summed from what each node advertises about itself. A node that has just
      joined has not said anything yet.
    </div>
  </div>
{:else}
  <div class="space-y-5">
    <!-- ═══════════ the four gauges ═══════════ -->
    <div class="grid gap-3.5" style="grid-template-columns: repeat(auto-fit, minmax(230px, 1fr));" use:staggerIn={{ each: 40, y: 8 }}>
      {#each gauges as g (g.id)}
        <div class="panel gauge" title={g.title}>
          <div class="flex items-center gap-2.5">
            <span class="icon-tile sm" style={`--tone: ${g.hue}; --tone-ink: var(--ink-2);`}><Icon name={g.icon} size={15} /></span>
            <span class="fig-label">{g.label}</span>
          </div>
          <div class="mt-3.5 flex items-center gap-4">
            <Ring
              size={88}
              thickness={9}
              total={100}
              arcs={g.pct == null ? [] : [{ value: Math.max(g.pct, 1), color: tone(g.pct, g.hue) }]}
              label={g.pct == null ? `${g.label}: unknown` : `${g.label}: ${g.pct}% ${g.centre}`}
            >
              {#if g.pct == null}
                <span class="num text-[17px] font-semibold text-ink-5">–</span>
              {:else}
                <span class="num text-[18px] font-bold text-ink">{g.pct}<span class="text-2xs font-semibold text-ink-5">%</span></span>
                <span class="mt-0.5 text-2xs text-ink-5">{g.centre}</span>
              {/if}
            </Ring>
            <div class="min-w-0">
              <div class="big-fig">{g.fig}{#if g.unit}<span class="unit">{g.unit}</span>{/if}</div>
              <div class="mt-1.5 text-xs leading-snug text-ink-5">{g.note}</div>
            </div>
          </div>
        </div>
      {/each}
    </div>

    <!-- What the four add up across, as facts rather than a sentence. -->
    <div class="flex flex-wrap items-center gap-2">
      <span class="fact"><b>{t.nodesLive}</b> live</span>
      {#if t.nodesDraining > 0}<span class="fact"><b>{t.nodesDraining}</b> draining</span>{/if}
      {#if t.nodesKnown > t.nodesLive}<span class="fact"><b>{t.nodesKnown - t.nodesLive}</b> unreachable</span>{/if}
      {#each t.arches as [arch, n] (arch)}
        <span class="fact"><span class="mono">{arch}</span> <b>×{n}</b></span>
      {/each}
      {#if t.devices > 0}
        <span class="fact"><Icon name="gpu" size={13} /><b>{t.devices}</b> accelerator{t.devices === 1 ? "" : "s"}</span>
      {/if}
      <!-- N6 §12 — what this peer's own dispatches cost. Bucket edges, so
           the chip reads as a bound rather than as precision the
           histogram does not have. -->
      {#if app.status?.dispatchCount}
        <span
          class="fact"
          title={`Time to hand out a job from this machine, over ${app.status.dispatchCount} placement${app.status.dispatchCount === 1 ? "" : "s"} — submission in, accepted somewhere, spills included.`}
        >
          <Icon name="pulse" size={13} />
          dispatch <b>p50 &lt;{Math.round(app.status.dispatchP50Ms ?? 0)}ms</b> · <b>p95 &lt;{Math.round(app.status.dispatchP95Ms ?? 0)}ms</b>
        </span>
      {/if}
      <!-- Only when it is `pack`, and as *this peer*: the policy decides
           where the node sending work puts it. -->
      {#if app.status?.placement === "pack"}
        <span
          class="fact"
          title="This peer fills a machine before starting the next one, so a mesh that looks lopsided is doing what it was told. Other peers dispatch by their own [compute] placement."
        >this peer <b>packs</b></span>
      {/if}
      <span class="ml-auto text-xs text-ink-5">
        {#if app.lastSync}updated {ago(app.lastSync * 1000)}{/if}
      </span>
    </div>

    <!-- ═══════════ what each machine lends ═══════════ -->
    <div class="grid gap-3.5 lg:grid-cols-3">
      <RuntimesView compact bind:open={runtimesOpen} />
      <PortsView compact bind:open={portsOpen} />
      <VolumesView compact bind:open={volumesOpen} />
    </div>

    <!-- ═══════════ per machine ═══════════ -->
    <div class="panel">
      <div class="panel-head">
        <span class="icon-tile" style="--tone: var(--accent);"><Icon name="nodes" size={17} /></span>
        <div class="min-w-0 flex-1">
          <h3 class="panel-title">Per machine</h3>
          <p class="panel-sub">Job slots in use, busiest first.</p>
        </div>
        <Hint
          label="About slots and unknown readings"
          text={'Slots, not cores, are what actually bounds concurrency — a policy number per node, and routinely different from the hardware one. Unknown readings show as "–", never as zero.'}
        />
      </div>
      <div class="machines">
        <div class="mrow mhead" aria-hidden="true">
          <span>Machine</span><span>Slots</span><span class="r">Cores</span><span class="r">Load/core</span><span class="r">Free mem</span><span class="r">Use</span>
        </div>
        {#each nodes as n (n.nodeId)}
          <div class="mrow">
            <span class="flex min-w-0 items-center gap-2">
              <span class="truncate font-medium text-ink-2">{n.name}</span>
              {#if n.isSelf}<span class="capsule-flat flex-none">this peer</span>{/if}
              {#if n.noExecute}<span class="capsule-flat flex-none" style="--tone: var(--status-warn); --tone-ink: var(--status-warn-ink);">draining</span>{/if}
            </span>
            <span class="flex min-w-0 items-center gap-2.5">
              <span class="meter-track h-[7px] min-w-0 flex-1">
                <span
                  class="meter-fill block"
                  data-load={n.saturationPct >= 90 ? "hot" : n.saturationPct >= 75 ? "warn" : "ok"}
                  style={`width:${n.slots > 0 ? Math.max(n.saturationPct, n.running > 0 ? 2 : 0) : 0}%;`}
                ></span>
              </span>
              <span class="num w-12 flex-none text-xs text-ink-4">{n.running}/{n.slots}</span>
            </span>
            <span class="num r text-ink-3">{n.cores || "–"}</span>
            <span class="num r text-ink-4">{n.loadPerCore == null ? "–" : n.loadPerCore.toFixed(2)}</span>
            <span class="num r text-ink-4">{n.memAvailableMb == null ? "–" : mb(n.memAvailableMb)}</span>
            <span class="num r font-semibold text-ink-2">{n.slots > 0 ? `${n.saturationPct}%` : "–"}</span>
          </div>
        {:else}
          <div class="py-6 text-center text-sm text-ink-5">No machine is answering right now.</div>
        {/each}
      </div>
    </div>
  </div>
{/if}

<style>
  .gauge {
    padding: 16px 18px 18px;
  }
  .machines {
    margin: 0 -4px;
  }
  .mrow {
    display: grid;
    grid-template-columns: minmax(150px, 1.2fr) minmax(160px, 2fr) 64px 80px 86px 56px;
    align-items: center;
    gap: 14px;
    padding: 9px 4px;
    font-size: var(--text-sm);
    border-bottom: 1px solid var(--edge-row);
  }
  .mrow:last-child {
    border-bottom: 0;
  }
  .mhead {
    padding-top: 0;
    font-size: var(--text-2xs);
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--ink-muted);
  }
  .r {
    text-align: right;
  }
</style>
