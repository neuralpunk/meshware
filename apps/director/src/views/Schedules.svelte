<script lang="ts">
  // Schedules (N1 P5) — work that runs itself, on a clock, with nobody
  // watching. The reader arrives with one question, and it is never "what
  // is the cron expression": it is *did it run, and when does it run next*.
  // So the table leads with those two and the expression is a middle
  // column, in the words the file used rather than in a five-field string
  // nobody parses on sight.
  //
  // The second question is the one this feature fails at silently: a
  // schedule that has quietly done nothing for three weeks. `orphaned` is
  // its own status word for exactly that — the service it names is gone,
  // it is not paused and not active, it is broken — and the skip reasons
  // are audited so not-firing is as visible as firing. It is also the one
  // number on the metric strip that goes red.
  //
  // Two sheets, no expanded rows (2026-09). The editor opened *above* the
  // table and a row opened *inside* it, so writing a schedule pushed the
  // list down and reading one pushed its neighbours away. Both are things
  // you look at and close; neither is the page.
  import { rowAction } from "../lib/rowaction";
  import {
    api,
    errText,
    type ServiceScheduleView,
    type ServiceScheduleCheckView,
  } from "../lib/api";
  import LoadError from "../lib/components/LoadError.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Pill from "../lib/components/Pill.svelte";
  import Sheet from "../lib/components/Sheet.svelte";
  import SpecEditor from "../lib/components/SpecEditor.svelte";
  import { chrome } from "../lib/chrome/chrome.svelte";
  import { ago, until } from "../lib/state.svelte";
  import { staggerIn } from "../lib/motion";
  import { poll } from "../lib/poll";
  import { hueOf, hueStyle } from "../lib/hues";

  let rows = $state<ServiceScheduleView[]>([]);
  let err = $state<string | null>(null);
  let selected = $state<string | null>(null);
  let busy = $state(false);
  let actionErr = $state<string | null>(null);
  /** The open row's definition. The list is fetched *without* sources — a
   *  TOML blob per row on a screen that polls every two seconds is a lot of
   *  text nobody is reading — so the panel asks for the one it shows. */
  let detail = $state<ServiceScheduleView | null>(null);

  async function open(name: string) {
    selected = name;
    detail = null;
    actionErr = null;
    try {
      detail = await api.schedule(name);
    } catch (e) {
      actionErr = errText(e);
    }
  }

  /** The open row, live: the list polls, and the sheet reads the row from
   *  it so a countdown keeps counting while the sheet is up. */
  const current = $derived(rows.find((r) => r.name === selected) ?? null);

  // The editor: one textarea, a dry run, then publish. The same parser the
  // CLI uses is on the other side of `schedulePut`, so a file that works in
  // one front end works in the other (D-E3).
  const EXAMPLE = `[schedule]
name    = "nightly-backup"
service = "backup"
at      = "03:00"
days    = "mon-fri"
targets = "any"
`;

  let editing = $state<"new" | "edit" | null>(null);
  let source = $state(EXAMPLE);
  let check = $state<ServiceScheduleCheckView | null>(null);

  function startNew() {
    editing = "new";
    source = EXAMPLE;
    check = null;
    actionErr = null;
  }

  /** Load an existing definition to edit — the Director equivalent of
   *  `schedule show --output toml | schedule put`, which is how the CLI
   *  edits one. A `put` under an existing name replaces it. */
  async function startEdit(name: string) {
    actionErr = null;
    check = null;
    try {
      const row = detail?.name === name && detail.source ? detail : await api.schedule(name);
      source = row.source ?? "";
      selected = null;
      editing = "edit";
    } catch (e) {
      actionErr = errText(e);
    }
  }

  async function load() {
    try {
      rows = await api.schedules();
      err = null;
    } catch (e) {
      err = errText(e);
    }
  }

  // A schedule's "next" is a countdown, so this screen polls even though
  // the records themselves rarely change.
  $effect(() => poll(load, 2000));

  // A deep link from another screen (`schedules:open`), read once the list
  // is here so the sheet has a row to show.
  $effect(() => {
    if (rows.length === 0) return;
    const o = sessionStorage.getItem("schedules:open");
    if (o) {
      sessionStorage.removeItem("schedules:open");
      if (rows.some((r) => r.name === o)) void open(o);
    }
  });

  // The page's action lives in the toolbar, next to the breadcrumb — the
  // same place, at the same size, on every screen.
  $effect(() => chrome.setActions(actions));

  function tone(s: string): "ok" | "muted" | "error" | "neutral" {
    switch (s) {
      case "active":
        return "ok";
      case "paused":
        return "muted";
      case "orphaned":
        return "error";
      default:
        return "neutral";
    }
  }

  /** A paused schedule has a next occurrence arithmetically and will not
   *  fire at it. Printing one would be this table's only lie. */
  function nextText(r: ServiceScheduleView): string {
    if (r.status === "paused") return "—";
    if (r.nextMicros == null) return "never";
    return until(r.nextMicros);
  }

  const active = $derived(rows.filter((r) => r.status === "active").length);
  const paused = $derived(rows.filter((r) => r.status === "paused").length);
  const orphaned = $derived(rows.filter((r) => r.status === "orphaned").length);
  /** The one firing soonest — the row worth the full ink in the Next column. */
  const soonest = $derived.by((): string | null => {
    let best: ServiceScheduleView | null = null;
    for (const r of rows) {
      if (r.status !== "active" || r.nextMicros == null) continue;
      if (!best || r.nextMicros < best.nextMicros!) best = r;
    }
    return best?.name ?? null;
  });
  const soonestRow = $derived(rows.find((r) => r.name === soonest) ?? null);

  async function doCheck() {
    busy = true;
    actionErr = null;
    try {
      check = await api.schedulePut(source, true);
    } catch (e) {
      actionErr = errText(e);
      check = null;
    } finally {
      busy = false;
    }
  }

  async function doPublish() {
    busy = true;
    actionErr = null;
    try {
      await api.schedulePut(source, false);
      editing = null;
      check = null;
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  async function doPause(name: string, paused: boolean) {
    busy = true;
    actionErr = null;
    try {
      await api.scheduleSetPaused(name, paused);
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  /// The schedule a *Delete* is waiting on. Same reason the other two
  /// have one: this is the neighbour of *Edit…*, and it is not undoable.
  let deleting = $state<ServiceScheduleView | null>(null);

  async function doRemove(name: string) {
    busy = true;
    actionErr = null;
    try {
      await api.scheduleRemove(name);
      deleting = null;
      selected = null;
      detail = null;
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }
</script>

{#snippet actions()}
  <button class="btn btn-primary btn-md" onclick={startNew}>
    <Icon name="plus" size={13} /> New schedule
  </button>
{/snippet}

{#if err}
  <LoadError class="mb-3" what="the schedules" error={err} retry={load} />
{/if}
{#if actionErr && !editing && !selected && !deleting}
  <div class="errbox mb-3">{actionErr}</div>
{/if}

{#if rows.length > 0}
  <div class="mb-5 grid gap-3" style="grid-template-columns: repeat(4, minmax(0, 1fr));">
    <div class="card metric" title="Schedules that will fire at their next occurrence">
      <div class="metric-label">Active</div>
      <div class="metric-fig">{active}<span class="sub">of {rows.length}</span></div>
      <div class="metric-bar">
        {#each rows as r (r.name)}
          <span
            data-on={r.status === "active" ? "" : undefined}
            style={r.status === "orphaned" ? "background: var(--status-error);" : ""}
            title={`${r.name} · ${r.status}`}
          ></span>
        {/each}
      </div>
    </div>
    <div class="card metric" title="The next thing that will run on its own">
      <div class="metric-label">Fires next</div>
      {#if soonestRow}
        <div class="metric-fig">{until(soonestRow.nextMicros!)}</div>
        <div class="metric-note truncate" title={soonestRow.name}>{soonestRow.name} · {soonestRow.when}</div>
      {:else}
        <div class="metric-fig text-ink-5">—</div>
        <div class="metric-note">nothing is due</div>
      {/if}
    </div>
    <div class="card metric" title="Definitions kept, not firing">
      <div class="metric-label">Paused</div>
      <div class="metric-fig">{paused}<span class="sub">{paused === 1 ? "kept, not firing" : "kept, not firing"}</span></div>
      <div class="metric-note">{paused === 0 ? "everything defined is live" : "resume from the row"}</div>
    </div>
    <div class="card metric" title="Schedules naming a service that no longer exists — not paused, not active, broken">
      <div class="metric-label">Needs attention</div>
      <div class="metric-fig" style={orphaned > 0 ? "color: var(--status-error-ink);" : ""}>
        {orphaned}<span class="sub">orphaned</span>
      </div>
      <div class="metric-note">
        {orphaned === 0 ? "every schedule names a service that exists" : "the service it names is gone"}
      </div>
    </div>
  </div>
{/if}

<div class="section-head">
  <span class="section-title">Schedules</span>
  <span class="section-sum">{rows.length === 0 ? (err ? "could not be read" : "none yet") : `${rows.length} defined · open one to pause, edit or delete`}</span>
</div>

{#if rows.length === 0 && err}
  <!-- Said above: a refused read is not an empty list (UX-2). -->
{:else if rows.length === 0}
  <div class="card empty">
    <div class="empty-title">Nothing runs itself yet</div>
    <div class="empty-sub">
      A schedule runs a service on a clock, with nobody watching — a nightly
      backup, an hourly report. Add one with <strong>New schedule</strong>
      above, or with <code class="num">meshnode schedule put</code>.
    </div>
  </div>
{:else}
  <div class="card overflow-hidden">
    <table class="w-full text-sm">
      <thead>
        <tr class="border-b border-edge">
          <th class="label px-5 py-2.5 text-left">Schedule</th>
          <th class="label px-3 py-2.5 text-left">Runs</th>
          <th class="label px-3 py-2.5 text-left">When</th>
          <th class="label w-[110px] px-3 py-2.5 text-left">Last</th>
          <th class="label w-[110px] px-3 py-2.5 text-left">Next</th>
          <th class="label w-[120px] px-3 py-2.5 text-left">Status</th>
          <th class="label w-[44px] px-3 py-2.5"></th>
        </tr>
      </thead>
      <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
        {#each rows as r (r.name)}
          <tr
            class="cursor-pointer border-b border-edge/60 transition-colors last:border-0 hover:bg-surface-2"
            style={r.status === "orphaned" ? "box-shadow: inset 3px 0 0 var(--status-error);" : ""}
            onclick={() => void open(r.name)}
            use:rowAction={() => void open(r.name)}
          >
            <td class="px-5 py-3 font-semibold text-ink">{r.name}</td>
            <td class="px-3 py-3">
              <span class="mono inline-flex items-center gap-1.5 text-xs text-ink-3" style={hueStyle(hueOf(r.service.split("@")[0]))}>
                <span class="name-dot"></span>{r.service}
              </span>
            </td>
            <td class="px-3 py-3 text-ink-4">{r.when}</td>
            <td class="num px-3 py-3 text-xs text-ink-4">{r.lastMicros != null ? ago(r.lastMicros) : "—"}</td>
            <td
              class="num px-3 py-3 text-xs"
              style={r.name === soonest ? "color: var(--ink-1); font-weight: 600;" : "color: var(--ink-4);"}
              title={r.name === soonest ? "the next schedule to fire" : undefined}>{nextText(r)}</td
            >
            <td class="px-3 py-3"><Pill tone={tone(r.status)} live={r.status === "active"}>{r.status}</Pill></td>
            <td class="px-3 py-3 text-right"><span class="rowchev inline-flex"><Icon name="chevron-right" size={14} /></span></td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
  <div class="footnote">
    Next is the countdown to the next firing. A paused schedule shows none, because it will not fire at it.
  </div>
{/if}

<!-- ═══════════════ one schedule, up close ═══════════════ -->
<Sheet
  open={selected !== null && !editing}
  title={current?.name ?? selected ?? ""}
  sub={current ? `Runs ${current.service}, ${current.when}, on ${current.targets === "any" ? "any node" : current.targets}.` : ""}
  icon="schedules"
  width={640}
  onclose={() => (selected = null)}
>
  {#snippet aside()}
    {#if current}<Pill tone={tone(current.status)} live={current.status === "active"}>{current.status}</Pill>{/if}
  {/snippet}

  {#if actionErr}
    <div class="errbox mb-4">{actionErr}</div>
  {/if}
  {#if current?.status === "orphaned"}
    <div class="errbox mb-4">
      The service this names is gone. It will not fire until that is fixed —
      republish the service, or retire the schedule.
    </div>
  {/if}

  {#if current}
    <div class="overflow-hidden rounded-lg" style="box-shadow: 0 0 0 1px var(--edge-card);">
      <div class="defrow" style="--key-w: 96px;">
        <span class="k">Next</span>
        <span class="v num">{nextText(current)}</span>
      </div>
      <div class="defrow" style="--key-w: 96px;">
        <span class="k">Last ran</span>
        <span class="v num">{current.lastMicros != null ? ago(current.lastMicros) : "never"}</span>
      </div>
      <div class="defrow" style="--key-w: 96px;" title="The machine currently responsible for noticing this schedule is due and starting it">
        <span class="k">Started by</span>
        <span class="v">{current.firer ?? "— none live"}</span>
      </div>
      <div class="defrow" style="--key-w: 96px;">
        <span class="k">Targets</span>
        <span class="v mono">{current.targets}</span>
      </div>
      <div class="defrow" style="--key-w: 96px; border-bottom: none;">
        <span class="k">Created</span>
        <span class="v">{current.createdBy}, {ago(current.createdAtMicros)}</span>
      </div>
    </div>

    <div class="microlabel mt-5 mb-2">Definition</div>
    {#if detail?.source}
      <pre class="pre selectable m-0 overflow-x-auto rounded-lg px-4 py-3 font-mono text-xs leading-relaxed text-ink-3" style="background: var(--surface-2);">{detail.source}</pre>
    {:else}
      <div class="text-xs text-ink-5">loading definition…</div>
    {/if}
  {/if}

  {#snippet footer()}
    {#if current}
      <button class="btn btn-secondary btn-md" disabled={busy} onclick={() => void doPause(current.name, current.status !== "paused")}>
        {current.status === "paused" ? "Resume" : "Pause"}
      </button>
      <button class="btn btn-secondary btn-md" disabled={busy} onclick={() => void startEdit(current.name)}>Edit…</button>
      <button class="btn btn-danger btn-md ml-auto" disabled={busy} onclick={() => (deleting = current)}>Delete</button>
    {/if}
  {/snippet}
</Sheet>

<!-- ═══════════════ the editor ═══════════════ -->
<Sheet
  open={editing !== null}
  title={editing === "edit" ? "Edit schedule" : "New schedule"}
  sub="One file, the same one the CLI takes. Check it first to see when it would fire and how many nodes match; nothing starts until you publish."
  icon="schedules"
  width={720}
  locked={busy}
  onclose={() => {
    editing = null;
    check = null;
  }}
>
  {#if actionErr}
    <div class="errbox mb-4">{actionErr}</div>
  {/if}
  <SpecEditor
    noun="schedule"
    bind:value={source}
    rows={12}
    {busy}
    planned={check != null}
    submitLabel={editing === "edit" ? "Save" : "Publish"}
    onplan={doCheck}
    onsubmit={doPublish}
  />
  {#if check}
    <div class="mt-4 overflow-hidden rounded-lg" style="box-shadow: 0 0 0 1px var(--edge-card);">
      <div class="defrow" style="--key-w: 110px;"><span class="k">Service</span><span class="v mono">{check.service}</span></div>
      <div class="defrow" style="--key-w: 110px;"><span class="k">When</span><span class="v">{check.when}</span></div>
      <div class="defrow" style="--key-w: 110px;">
        <span class="k">Matches now</span>
        <span class="v">{check.matchingNodes} node{check.matchingNodes === 1 ? "" : "s"}</span>
      </div>
      <div class="defrow" style="--key-w: 110px; border-bottom: none;">
        <span class="k">Would fire</span>
        <span class="v num">{check.nextMicros != null ? until(check.nextMicros) : "never"}</span>
      </div>
    </div>
    {#if check.issues.length > 0}
      <ul class="mt-3 mb-0 list-none space-y-1 p-0 text-sm" style="color: var(--status-error-ink);">
        {#each check.issues as issue}
          <li>· {issue}</li>
        {/each}
      </ul>
    {:else}
      <div class="mt-3 flex items-center gap-2 text-sm" style="color: var(--status-ok-ink);">
        <Icon name="check" size={14} /> No problems — this schedule is ready to publish.
      </div>
    {/if}
  {/if}
</Sheet>

<!-- Deleting a schedule is quiet in a way deleting a deployment is not:
     nothing stops, so the absence shows up weeks later as work that did
     not happen. Pause is offered here for the same reason. -->
<Modal
  open={deleting !== null}
  kicker="delete schedule"
  peer={deleting?.name ?? ""}
  onclose={() => (deleting = null)}
>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Delete {deleting?.name}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      It runs <span class="mono">{deleting?.service}</span>{deleting?.when
        ? `, ${deleting.when}`
        : ""}. Deleting it forgets the definition; nothing that has already run
      is affected.
    </p>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-4);">
      Nothing stops when a schedule goes — which is why its absence tends to
      show up weeks later as work that did not happen. To keep the definition
      and stop it firing, use <strong>Pause</strong>.
    </p>
    {#if actionErr}<div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{actionErr}</div>{/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button
        class="h-11 cursor-pointer rounded-md border text-base font-semibold text-inkcode-2"
        style="border-color: var(--edge-code); background: transparent;"
        onclick={() => (deleting = null)}>Keep it</button
      >
      <button
        class="h-11 cursor-pointer rounded-md border text-base font-semibold transition-colors disabled:opacity-40"
        style="border-color: var(--status-error-edge); color: var(--status-error-ink);"
        disabled={busy}
        onclick={() => void doRemove(deleting!.name)}
        >{busy ? "Deleting…" : "Delete"}</button
      >
    </div>
  </div>
</Modal>
