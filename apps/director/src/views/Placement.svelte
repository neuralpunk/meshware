<script lang="ts">
  // Placement (Phase C, U8) — where every deployment's instances run,
  // machine by machine.
  //
  // **The question it answers is "why is everything on one box"**, before
  // anybody has to reach for `jobs explain`. Each machine is drawn as what it
  // has — cores and memory — with a block inside for each instance, as wide
  // as what that instance asked for and coloured by its deployment, so the
  // same deployment is the same colour on every machine and a lopsided mesh
  // is visible at a glance.
  //
  // Nomad's Topology view is the model. What this adds is the two things a
  // meshware operator arrives with that Nomad does not surface: a machine
  // being drained (and by when), and a machine that takes no new work.
  //
  // An instance that asked for nothing is still drawn — a thin block, and
  // said in words — because an empty bar on a machine running six things
  // would be the one lie this panel could tell.
  import { api, errText, fmtSizeMb, type PlacementNodeView } from "../lib/api";
  import { poll } from "../lib/poll";
  import { staggerIn } from "../lib/motion";
  import { hueOf } from "../lib/hues";
  import LoadError from "../lib/components/LoadError.svelte";
  import Pill from "../lib/components/Pill.svelte";

  let rows = $state<PlacementNodeView[] | null>(null);
  let err = $state<string | null>(null);

  async function load() {
    try {
      rows = await api.placementView();
      err = null;
    } catch (e) {
      err = errText(e);
    }
  }

  // An instance moves when something starts or stops, which is seconds at
  // the fastest; the Deployments screen polls status at two and this reads
  // every deployment at once.
  $effect(() => poll(load, 5000));

  const deployments = $derived.by((): string[] => {
    const names = new Set<string>();
    for (const n of rows ?? []) for (const i of n.instances) names.add(i.deployment);
    return [...names].sort();
  });
  const placed = $derived((rows ?? []).reduce((sum, n) => sum + n.instances.length, 0));
  const draining = $derived((rows ?? []).filter((n) => n.draining).length);
  const holding = $derived((rows ?? []).filter((n) => !n.eligible && !n.draining).length);

  /** Machines with instances first, fullest first; then the rest by name. */
  const ordered = $derived(
    [...(rows ?? [])].sort((a, b) => {
      const used = (n: PlacementNodeView) =>
        n.cores > 0 ? n.instances.reduce((s, i) => s + i.cpus, 0) / n.cores : n.instances.length;
      return used(b) - used(a) || b.instances.length - a.instances.length || a.name.localeCompare(b.name);
    }),
  );

  const mb = fmtSizeMb;

  /** Width of one instance's block, as a share of what the machine has. A
   *  request of nothing, or a machine that advertises nothing, draws a thin
   *  block rather than no block. */
  function share(asked: number, has: number): string {
    if (asked <= 0 || has <= 0) return "6px";
    return `${Math.max(1.5, Math.min(100, (asked / has) * 100))}%`;
  }

  function hue(deployment: string): string {
    return `var(--hue-${hueOf(deployment)})`;
  }

  function stateWord(n: PlacementNodeView): { tone: "ok" | "warn" | "muted"; word: string } | null {
    if (n.draining) {
      if (n.leaving) return { tone: "warn", word: "shutting down" };
      if (n.drainDeadlineSecs == null) return { tone: "warn", word: "draining, no deadline" };
      const d = new Date(n.drainDeadlineSecs * 1000);
      const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
      return { tone: "warn", word: `draining until ${hm}` };
    }
    if (!n.eligible) return { tone: "muted", word: "takes no new work" };
    if (n.liveness !== "alive") return { tone: "warn", word: n.liveness };
    return null;
  }
</script>

{#if err}
  <LoadError class="mb-3" what="where work is placed" error={err} retry={load} />
{/if}

{#if rows}
  <div class="mb-5 grid gap-3" style="grid-template-columns: repeat(3, minmax(0, 1fr));">
    <div class="card metric" title="Instances of every deployment, on the machines running them">
      <div class="metric-label">Placed</div>
      <div class="metric-fig">{placed}<span class="sub">instance{placed === 1 ? "" : "s"} of {deployments.length} deployment{deployments.length === 1 ? "" : "s"}</span></div>
    </div>
    <div class="card metric" title="Machines whose instances are moving off them">
      <div class="metric-label">Draining</div>
      <div class="metric-fig" style={draining > 0 ? "color: var(--status-warn-ink);" : ""}>
        {draining}<span class="sub">of {rows.length} machine{rows.length === 1 ? "" : "s"}</span>
      </div>
      <div class="metric-note">An instance there moves once its replacement proves itself elsewhere.</div>
    </div>
    <div class="card metric" title="Machines an administrator is keeping new work away from, without moving what runs there">
      <div class="metric-label">Held back</div>
      <div class="metric-fig">{holding}<span class="sub">take{holding === 1 ? "s" : ""} no new work</span></div>
      <div class="metric-note">What already runs there stays.</div>
    </div>
  </div>

  {#if deployments.length > 0}
    <div class="mb-3 flex flex-wrap items-center gap-x-4 gap-y-1.5 text-xs text-ink-4" aria-label="Deployments">
      {#each deployments as d (d)}
        <span class="flex items-center gap-1.5">
          <span class="inline-block h-2.5 w-2.5 rounded-sm" style="background: {hue(d)};"></span>
          <span class="mono">{d}</span>
        </span>
      {/each}
    </div>
  {:else}
    <div class="card mb-5 px-5 py-4 text-sm text-ink-4">
      Nothing is deployed yet, so every machine below is empty. Deployments place their instances here.
    </div>
  {/if}

  <div class="grid gap-3" style="grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));" use:staggerIn={{ selector: ":scope > div", each: 12, y: 4 }}>
    {#each ordered as n (n.nodeId)}
      {@const cpus = n.instances.reduce((s, i) => s + i.cpus, 0)}
      {@const mem = n.instances.reduce((s, i) => s + i.memMb, 0)}
      {@const said = stateWord(n)}
      <div class="card px-4 py-3.5" style={n.draining ? "box-shadow: inset 3px 0 0 var(--status-warn);" : ""}>
        <div class="flex items-center gap-2">
          <span class="font-semibold text-ink">{n.name}</span>
          {#if said}<Pill tone={said.tone}>{said.word}</Pill>{/if}
          <span class="num ml-auto text-xs text-ink-5">{n.instances.length} instance{n.instances.length === 1 ? "" : "s"}</span>
        </div>

        <div class="mt-3 text-2xs uppercase tracking-wide text-ink-5">
          Cores <span class="num normal-case">{cpus} of {n.cores > 0 ? n.cores : "?"} asked for</span>
        </div>
        <div class="mt-1 flex h-3.5 w-full gap-px overflow-hidden rounded bg-surface-3" role="img" aria-label={`${n.name}: ${cpus} of ${n.cores} cores asked for`}>
          {#each n.instances as i (i.deployment + "/" + i.slot)}
            <span
              class="h-full"
              style="width: {share(i.cpus, n.cores)}; background: {hue(i.deployment)}; opacity: {i.state === 'running' ? 1 : 0.5};"
              title={`${i.deployment}/${i.slot} · ${i.cpus > 0 ? `${i.cpus} core${i.cpus === 1 ? "" : "s"}` : "asked for no cores"} · ${i.state}`}
            ></span>
          {/each}
        </div>

        <div class="mt-2.5 text-2xs uppercase tracking-wide text-ink-5">
          Memory <span class="num normal-case">{mb(mem)} of {n.memMb > 0 ? mb(n.memMb) : "?"} asked for</span>
        </div>
        <div class="mt-1 flex h-3.5 w-full gap-px overflow-hidden rounded bg-surface-3" role="img" aria-label={`${n.name}: ${mb(mem)} of ${mb(n.memMb)} memory asked for`}>
          {#each n.instances as i (i.deployment + "/" + i.slot)}
            <span
              class="h-full"
              style="width: {share(i.memMb, n.memMb)}; background: {hue(i.deployment)}; opacity: {i.state === 'running' ? 1 : 0.5};"
              title={`${i.deployment}/${i.slot} · ${i.memMb > 0 ? mb(i.memMb) : "asked for no memory"} · ${i.state}`}
            ></span>
          {/each}
        </div>

        {#if n.instances.length > 0}
          <div class="mt-3 flex flex-col gap-1">
            {#each n.instances as i (i.deployment + "/" + i.slot)}
              <div class="flex min-w-0 items-center gap-2 text-xs">
                <span class="inline-block h-2 w-2 shrink-0 rounded-sm" style="background: {hue(i.deployment)};"></span>
                <span class="mono min-w-0 truncate text-ink-2" title={`${i.deployment}/${i.slot}`}>{i.deployment}/{i.slot}</span>
                <span class="shrink-0 whitespace-nowrap text-ink-5">{i.state}{i.health !== "unknown" ? ` · ${i.health}` : ""}</span>
                <span class="num ml-auto shrink-0 whitespace-nowrap text-ink-5">
                  {i.cpus > 0 || i.memMb > 0 ? `${i.cpus > 0 ? `${i.cpus}c` : ""}${i.cpus > 0 && i.memMb > 0 ? " · " : ""}${i.memMb > 0 ? mb(i.memMb) : ""}` : "no request"}
                </span>
              </div>
            {/each}
          </div>
        {:else}
          <div class="mt-3 text-xs text-ink-5">
            {n.eligible ? "Nothing deployed here." : "Nothing deployed here, and nothing new will be."}
          </div>
        {/if}
      </div>
    {/each}
  </div>
{/if}
