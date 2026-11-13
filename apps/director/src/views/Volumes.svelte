<script lang="ts">
  // Volumes (N5) — the persistent storage this node lends, and who holds it.
  //
  // Beside Ports because it is the same shape of question — *what does this
  // machine lend, and what is holding it right now* — and the same answer:
  // **this node's ledger, and the panel says so.** A volume is a directory
  // on one machine. There is no mesh-wide volume table to show, and there
  // should not be: the whole point of the lease is that the node that owns
  // the bytes is the node that arbitrates access to them.
  //
  // The one distinction this panel must never blur is the one the view
  // model already splits in two: **"nobody is holding it" and "a writer
  // could take it" are different questions.** A read-only volume with no
  // holders is free and will never be writable. A volume held by three
  // readers is not free and is also not writable. An operator looking at a
  // job that will not place needs to see which of those they have.
  import { api, errText, type VolumeRowView } from "../lib/api";
  import { poll } from "../lib/poll";
  import { staggerIn } from "../lib/motion";
  import LoadError from "../lib/components/LoadError.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Pill from "../lib/components/Pill.svelte";
  import Sheet from "../lib/components/Sheet.svelte";
  import SummaryCard from "../lib/components/SummaryCard.svelte";

  /** `compact` draws the Capacity card; `open` is its detail sheet, bindable
   *  so a deep link to the retired Volumes panel lands on it (2026-09-22).
   *  The provision form and the remove confirmation are drawn either way,
   *  above whichever of the two is showing. */
  let { compact = false, open = $bindable(false) }: { compact?: boolean; open?: boolean } = $props();

  let rows = $state<VolumeRowView[] | null>(null);
  let err = $state<string | null>(null);

  async function load() {
    try {
      rows = await api.volumes();
      err = null;
    } catch (e) {
      err = errText(e);
    }
  }

  // Holds come and go with jobs, at the same cadence as the port ledger.
  $effect(() => poll(load, 2000));

  // N5 P2 — provisioning, the other half. Parity with
  // `meshnode volumes create`: same plugin, same validation, same refusal
  // sentences, because both go through the one Tauri command.
  //
  // In a sheet rather than unfolded on the ledger card (2026-09): the form
  // is four fields and a paragraph, and open on the page it pushed the
  // table — the thing that answers "should I" — below the fold.
  let creating = $state(false);
  let busy = $state(false);
  let actionErr = $state<string | null>(null);
  let newName = $state("");
  let newCapacity = $state("");
  let newParams = $state("");
  let newReadOnly = $state(false);

  function startCreate() {
    newName = "";
    newCapacity = "";
    newParams = "";
    newReadOnly = false;
    actionErr = null;
    creating = true;
  }

  async function create() {
    // Refused here, in words, rather than sent. `Number("512MB")` is
    // `NaN`, and `NaN` over the Tauri boundary fails to deserialize into
    // `Option<u64>` — so a person who typed a unit got an opaque codec
    // error instead of being told which field was wrong.
    const typed = newCapacity.trim();
    let mb: number | null = null;
    if (typed) {
      const n = Number(typed);
      if (!Number.isFinite(n) || n < 0) {
        actionErr = `Capacity must be a number of megabytes — "${typed}" is not one. Leave it empty to let the plugin decide.`;
        return;
      }
      mb = Math.floor(n);
    }
    busy = true;
    actionErr = null;
    try {
      await api.volumeCreate(newName, mb, newParams || null, newReadOnly);
      creating = false;
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  /// The row a *Remove* is waiting on. Removing a volume asks the plugin
  /// to delete the bytes, which is not undoable — and every other
  /// destructive verb in this app (retire a script, remove a source)
  /// confirms first. This one shipped without it.
  let removing = $state<VolumeRowView | null>(null);

  async function remove(name: string, force: boolean) {
    busy = true;
    actionErr = null;
    try {
      await api.volumeRemove(name, force);
      removing = null;
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  const node = $derived(rows?.[0]?.node ?? "");
  const free = $derived((rows ?? []).filter((v) => v.holders.length === 0).length);
  const writable = $derived((rows ?? []).filter((v) => v.writableNow).length);
  const provisioned = $derived((rows ?? []).filter((v) => v.source !== "static").length);
  const holders = $derived(new Set((rows ?? []).flatMap((v) => v.holders)).size);

  /** What a row is doing, as the State column shows it.
   *
   *  **The same order, and the same words, as `meshnode volumes`.** The
   *  first version tested `writeHeld` before `readOnly` and dropped the
   *  reader count, so a read-only volume with two readers printed
   *  `read-only` in the terminal and `reading` here — the same fact, two
   *  answers, which is the exact drift D-E3 exists to stop. */
  function holding(v: VolumeRowView): { word: string; tone: "ok" | "running" | "info" | "muted"; live: boolean } {
    if (v.readOnly) return { word: "read-only", tone: "muted", live: false };
    if (v.writeHeld) return { word: "writing", tone: "running", live: true };
    if (v.holders.length > 0) return { word: `${v.holders.length} reading`, tone: "info", live: true };
    return { word: "free", tone: "ok", live: false };
  }

  function why(v: VolumeRowView): string {
    if (v.writeHeld) return "A writer holds it, so nothing else may take it at all";
    if (v.holders.length > 0)
      return `${v.holders.length} reader${v.holders.length === 1 ? "" : "s"} — another reader may join, a writer must wait`;
    if (v.readOnly) return "Lent read-only, so no job will ever get write access";
    return "Nobody is holding it";
  }

  /** A tile's colour on the card, from the same four words as the State
   *  column — so the picture and the ledger cannot disagree. */
  const SWATCH: Record<string, { color: string; word: string }> = {
    ok: { color: "var(--status-ok)", word: "free" },
    running: { color: "var(--status-running)", word: "writing" },
    info: { color: "var(--hue-sky)", word: "reading" },
    muted: { color: "var(--ink-faint)", word: "read-only" },
  };
  const present = $derived(
    Object.entries(SWATCH).filter(([tone]) => (rows ?? []).some((v) => holding(v).tone === tone)),
  );
</script>

{#snippet full()}

{#if err}
  <LoadError class="mb-3" what="this node's volumes" error={err} retry={load} />
{/if}
{#if actionErr && !creating && !removing}
  <div class="errbox mb-3">{actionErr}</div>
{/if}

{#if rows}
  <div class="section-head">
    <span class="section-title">{node || "this node"}</span>
    <span class="section-sum">on this machine</span>
    <button class="btn btn-secondary btn-md ml-auto" disabled={busy} onclick={startCreate}>
      <Icon name="plus" size={13} /> Provision a volume
    </button>
  </div>

  <div class="mb-5 grid gap-3" style="grid-template-columns: repeat(3, minmax(0, 1fr));">
    <div class="card metric" title="Directories this node offers to jobs">
      <div class="metric-label">Lent</div>
      <div class="metric-fig">{rows.length}<span class="sub">volume{rows.length === 1 ? "" : "s"}</span></div>
      <div class="metric-note">
        {#if rows.length === 0}
          none yet
        {:else}
          {rows.length - provisioned} from meshnode.toml{provisioned > 0 ? ` · ${provisioned} provisioned` : ""}
        {/if}
      </div>
    </div>

    <div class="card metric" title="Volumes nobody is holding right now">
      <div class="metric-label">Free</div>
      <div class="metric-fig">{free}<span class="sub">of {rows.length}</span></div>
      <div class="metric-bar">
        {#each rows as v (v.name)}
          <span data-on={v.holders.length === 0 ? "" : undefined} title={`${v.name} · ${holding(v).word}`}></span>
        {/each}
      </div>
    </div>

    <!-- Said separately from "unheld" on purpose: a volume can be unheld
         and still not writable, and collapsing the two is exactly the
         confusion that sends an operator looking for a scheduler bug. -->
    <div class="card metric" title="Volumes a job asking to write could be given this instant">
      <div class="metric-label">Writable now</div>
      <div class="metric-fig">{writable}<span class="sub">could take a writer</span></div>
      <div class="metric-note">
        {#if holders > 0}
          {holders} job{holders === 1 ? " holds" : "s hold"} a lease right now
        {:else}
          no job holds a lease right now
        {/if}
      </div>
    </div>
  </div>

  <div class="section-head">
    <span class="section-title">Volumes</span>
    <span class="section-sum">{rows.length === 0 ? "none" : `${rows.length} on ${node || "this node"}`}</span>
  </div>

  {#if rows.length === 0}
    <div class="card empty">
      <div class="empty-title">This node lends no volumes</div>
      <div class="empty-sub">
        Add a <code class="num">[[volume]]</code> block to meshnode.toml — a
        <code class="num">name</code> and a <code class="num">path</code>, and
        <code class="num">read_only = true</code> where the bytes must not change.
        A job then claims one with <code class="num">--volume data</code>, or a
        deployment with <code class="num">[[deployment.volume]]</code>.
      </div>
    </div>
  {:else}
    <div class="card overflow-hidden">
      <table class="w-full text-sm">
        <thead>
          <tr class="border-b border-edge">
            <th class="label px-5 py-2.5 text-left">Volume</th>
            <th class="label px-3 py-2.5 text-left">Path</th>
            <th class="label px-3 py-2.5 text-left">Source</th>
            <th class="label px-3 py-2.5 text-left">State</th>
            <th class="label px-3 py-2.5 text-left">Held by</th>
            <th class="label w-[72px] px-5 py-2.5"></th>
          </tr>
        </thead>
        <tbody use:staggerIn={{ selector: ":scope > tr", each: 12, y: 4 }}>
          {#each rows as v (v.name)}
            {@const h = holding(v)}
            <tr class="border-b border-edge/60 last:border-0">
              <td class="mono px-5 py-3 font-semibold text-ink">{v.name}</td>
              <td class="mono px-3 py-3 text-xs text-ink-4" title={v.path}>{v.path}</td>
              <td class="px-3 py-3">
                <span class="chip" title={v.source === "static" ? "A [[volume]] block in meshnode.toml" : `Provisioned through ${v.source}`}>
                  {v.source === "static" ? "meshnode.toml" : v.source}
                </span>
              </td>
              <td class="px-3 py-3"><Pill tone={h.tone} live={h.live} title={why(v)}>{h.word}</Pill></td>
              <td class="mono px-3 py-3 text-xs text-ink-4">
                {#if v.holders.length === 0}
                  <span class="text-ink-5">—</span>
                {:else}
                  <span class="flex flex-wrap gap-1">
                    {#each v.holders as job (job)}<span class="chip">{job}</span>{/each}
                  </span>
                {/if}
              </td>
              <td class="px-5 py-3 text-right">
                <!-- Only on a provisioned row. A `[[volume]]` block is the
                     operator's own line in their own file, and a button
                     that deleted it from under them would be a button that
                     edits meshnode.toml. -->
                {#if v.source !== "static"}
                  <button
                    class="iconbtn h-7 w-7"
                    disabled={busy || v.holders.length > 0}
                    title={v.holders.length > 0
                      ? "Held right now — a lease is the authority on whether anything is using the bytes"
                      : `Remove ${v.name}: ask ${v.source} to delete it, and stop offering it`}
                    aria-label={`Remove ${v.name}`}
                    onclick={() => (removing = v)}><Icon name="trash" size={14} /></button
                  >
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <div class="footnote">
      A job claims one with <code class="num">--volume data</code>, a deployment with
      <code class="num">[[deployment.volume]]</code>. Provisioned volumes go through this node's
      <code class="num">[runtime] volume_plugin</code>.
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
    icon="disk"
    title="Volumes"
    tone="var(--hue-amber)"
    sub={rows
      ? rows.length === 0
        ? `${node || "this node"} lends no volumes`
        : `${rows.length} lent · ${free} free · ${writable} writable`
      : err
        ? "could not read the volumes"
        : "loading the ledger…"}
    onopen={() => (open = true)}
  >
    {#if rows && rows.length > 0}
      <span class="flex flex-wrap gap-1.5">
        {#each rows as v (v.name)}
          {@const h = holding(v)}
          <span
            class="vol-tile"
            style={`--c: ${SWATCH[h.tone]?.color ?? "var(--ink-faint)"};`}
            title={`${v.name} · ${h.word}`}
          >{v.name}</span>
        {/each}
      </span>
      <span class="mt-3 flex flex-wrap gap-x-3.5 gap-y-1 text-xs text-ink-5">
        {#each present as [tone, s] (tone)}
          <span class="inline-flex items-center gap-1.5"><span class="vol-sw" style={`--c: ${s.color};`}></span>{s.word}</span>
        {/each}
      </span>
    {:else if rows}
      <span class="block text-xs leading-snug text-ink-5">
        Add a <code class="num">[[volume]]</code> block to meshnode.toml, or provision one here.
      </span>
    {/if}
  </SummaryCard>

  <Sheet
    {open}
    title="Volumes"
    sub="The storage this machine lends, and who is holding it."
    icon="disk"
    width={900}
    onclose={() => (open = false)}
  >
    {@render full()}
  </Sheet>
{/if}

<!-- N5 P2. The form, in a sheet: what to call it, how big, and what to
     tell the plugin. -->
<Sheet
  open={creating}
  title="Provision a volume"
  sub={`Through ${node || "this node"}'s volume plugin. The directory is created now and lent from the next poll.`}
  icon="folder"
  width={560}
  locked={busy}
  onclose={() => (creating = false)}
>
  {#if actionErr}
    <div class="errbox mb-4">{actionErr}</div>
  {/if}
  <div class="grid gap-4" style="grid-template-columns: minmax(0, 1fr) 150px;">
    <label class="block">
      <span class="label mb-1.5 block">Name</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input class="field field-sunken mono h-10 w-full px-3 text-sm outline-none" bind:value={newName} placeholder="data" autofocus />
      <span class="mt-1.5 block text-xs text-ink-5">What a job asks for: <code class="num">--volume {newName.trim() || "data"}</code></span>
    </label>
    <label class="block">
      <span class="label mb-1.5 block">Capacity, MB</span>
      <input class="field field-sunken num h-10 w-full px-3 text-sm outline-none" bind:value={newCapacity} inputmode="numeric" placeholder="plugin decides" />
    </label>
  </div>
  <label class="mt-4 flex cursor-pointer items-center gap-2.5 text-sm text-ink-2">
    <input type="checkbox" class="check" bind:checked={newReadOnly} />
    Lend read-only
    <span class="text-xs text-ink-5">— no job will ever get write access</span>
  </label>
  <label class="mt-4 block">
    <span class="label mb-1.5 block">Plugin parameters</span>
    <textarea
      class="field field-sunken mono w-full px-3 py-2 text-xs outline-none"
      rows="4"
      bind:value={newParams}
      placeholder={"pool=fast\ntier=gold"}
    ></textarea>
    <span class="mt-1.5 block text-xs leading-relaxed text-ink-5">
      One <code class="num">key=value</code> per line, whatever the plugin documents.
      Validated before it leaves — shell metacharacters are refused here, so a plugin
      that forgets to quote is merely broken.
    </span>
  </label>

  {#snippet footer()}
    <span class="text-xs text-ink-5">Nothing on the mesh changes until you press Provision.</span>
    <button class="btn btn-secondary btn-md ml-auto" disabled={busy} onclick={() => (creating = false)}>Cancel</button>
    <button class="btn btn-primary btn-md" disabled={busy || !newName.trim()} onclick={() => void create()}>
      {busy ? "Provisioning…" : "Provision"}
    </button>
  {/snippet}
</Sheet>

<!-- Removing a volume asks the plugin to delete the bytes. Not undoable,
     and the path is repeated back so a mis-click on the wrong row is caught
     by reading rather than by recovering. -->
<Modal
  open={removing !== null}
  kicker="remove volume"
  peer={removing?.name ?? ""}
  onclose={() => (removing = null)}
>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Remove {removing?.name}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      <span class="mono">{removing?.source}</span> created it. Removing it stops
      this node offering it and asks the plugin to delete
      <span class="mono">{removing?.path}</span>.
    </p>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-4);">
      Whatever is written there goes with it. Nothing snapshots or backs up a
      volume — that is your job, exactly as it is for any directory on the
      machine.
    </p>
    {#if actionErr}<div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{actionErr}</div>{/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button
        class="h-11 cursor-pointer rounded-md border text-base font-semibold text-inkcode-2"
        style="border-color: var(--edge-code); background: transparent;"
        onclick={() => (removing = null)}>Keep it</button
      >
      <button
        class="h-11 cursor-pointer rounded-md border text-base font-semibold transition-colors disabled:opacity-40"
        style="border-color: var(--status-error-edge); color: var(--status-error-ink);"
        disabled={busy}
        onclick={() => void remove(removing!.name, false)}
        >{busy ? "Removing…" : "Remove"}</button
      >
    </div>
  </div>
</Modal>

<style>
  .vol-tile {
    display: inline-flex;
    align-items: center;
    height: 26px;
    padding: 0 10px;
    border-radius: 8px;
    font-family: var(--font-mono);
    font-size: var(--text-2xs);
    font-weight: 600;
    color: var(--ink-2);
    background: color-mix(in srgb, var(--c) 14%, transparent);
    box-shadow: inset 3px 0 0 var(--c), inset 0 0 0 1px color-mix(in srgb, var(--c) 24%, transparent);
  }
  .vol-sw {
    width: 8px;
    height: 8px;
    border-radius: 3px;
    background: var(--c);
  }
</style>
