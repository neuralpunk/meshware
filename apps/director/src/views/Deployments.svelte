<script lang="ts">
  // Deployments (C2 §6.4 P6) — the services that are supposed to stay up,
  // and whether they are. The reader arrives with one question — is this
  // converged, or is something wrong — so the list leads with healthy /
  // desired and a few words on what is happening, and a click opens the
  // deployment itself.
  //
  // Two sheets, no expanded rows (2026-09). The builder is 1,200px of form
  // and opened *above* the table; a row opened *inside* it and pushed the
  // rows under it off the screen. Both are things you look at and close,
  // and the list is the page.
  //
  // **The deployment sheet tells you what is going on (2026-09-14).** It
  // used to print the engine's vocabulary — a slot table with `absent`,
  // `queued` and `starting` in two columns, a reconciler name, a generation
  // — and every word was true while none of them answered what somebody
  // opening it asks: *why is that copy not up, what is Meshware doing now,
  // and what did my Scale just set in motion?* So it opens on **Right now**,
  // one headline with its reason and what happens next; then every copy
  // walking Placed → Started → Healthy; then what just happened. The words
  // come from `lib/deploymentStory.ts`, which is tested, so the sheet and
  // the sentences cannot quietly drift apart. Versions and the definition
  // are a tab away — they are the other days' questions.
  import { rowAction } from "../lib/rowaction";
  import { untrack } from "svelte";
  import {
    api,
    errText,
    type DeploymentView,
    type DeploymentStatusView,
    type DeploymentCheckView,
    type RunEvent,
    type DeploymentInstanceDetailView,
    type DeploymentHistoryView,
  } from "../lib/api";
  import type { AuditView } from "../lib/bindings/AuditView";
  import {
    STAGES,
    STEP,
    auditActivity,
    copyActivity,
    copyRows,
    explainNow,
    mergeActivity,
    type Activity,
    type CopyRow,
    type NowAction,
  } from "../lib/deploymentStory";
  import SpecEditor from "../lib/components/SpecEditor.svelte";
  import Terminal from "../lib/components/LazyTerminal.svelte";
  import DeploymentBuilder from "../lib/components/DeploymentBuilder.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Spinner from "../lib/components/Spinner.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Pill from "../lib/components/Pill.svelte";
  import Segments from "../lib/components/Segments.svelte";
  import Sheet from "../lib/components/Sheet.svelte";
  import LoadError from "../lib/components/LoadError.svelte";
  import { chrome } from "../lib/chrome/chrome.svelte";
  import { ago, app } from "../lib/state.svelte";
  import { staggerIn } from "../lib/motion";
  import { poll } from "../lib/poll";
  import { hueOf, hueStyle } from "../lib/hues";

  let rows = $state<DeploymentView[]>([]);
  let err = $state<string | null>(null);
  let selected = $state<string | null>(null);
  let status = $state<DeploymentStatusView | null>(null);
  let busy = $state(false);
  /** The open row's definition. Fetched per-row for the reason the list is
   *  fetched without it: a definition per row, re-encoded across the
   *  webview boundary every two seconds, for text nobody is reading. */
  let detail = $state<DeploymentView | null>(null);

  // Rollout dialog state.
  let rollingOut = $state<string | null>(null);
  let rolloutVersion = $state<number>(1);
  let actionErr = $state<string | null>(null);

  // The editor (N1, D-E3). Until this existed the screen could roll, pause
  // and delete a deployment but not *make* one, and its own empty state
  // sent the reader to the terminal — the exact shape of gap D-E3 names.
  //
  // One textarea, a dry run, then publish. The same parser the CLI uses is
  // on the other side of `deploymentPut`, so a `[deployment.disconnect]`
  // block that works in the terminal works here; a second parser would
  // make parity a claim rather than a fact.
  const EXAMPLE = `[deployment]
name    = "web"
service = "collector@1"
count   = 3
targets = "any"

[deployment.update]
max_unavailable = 1
canary          = 1
auto_revert     = true
progress_deadline = "10m"
min_healthy_time  = "30s"

[deployment.disconnect]
lost_after = "10m"
replace    = true
reconcile  = "keep_original"
`;

  let editing = $state<"new" | "edit" | null>(null);
  /** `form` is the guided builder; `text` is the raw file. */
  let mode = $state<"form" | "text">("form");
  let source = $state(EXAMPLE);
  /** Published services, for the builder's "something published" path. */
  const serviceOptions = $derived(
    (app.services ?? []).map((s) => ({ name: s.name, version: s.version })),
  );
  let check = $state<DeploymentCheckView | null>(null);

  function startNew() {
    editing = "new";
    mode = "form";
    source = EXAMPLE;
    check = null;
    actionErr = null;
  }

  /** U14: the example `meshnode deployment init` prints, opened as a file
   *  and waiting for a plan — the fastest way to see what a deployment is. */
  async function startExample() {
    editing = "new";
    mode = "text";
    check = null;
    actionErr = null;
    try {
      source = await api.deploymentExample();
    } catch (e) {
      source = EXAMPLE;
      actionErr = e instanceof Error ? e.message : String(e);
    }
  }

  /** Load an existing definition to edit. The list is fetched without the
   *  source — a definition per row on a screen that polls every two
   *  seconds is a lot of TOML for text nobody is reading — so the editor
   *  asks for the one it needs. */
  async function startEdit(name: string) {
    actionErr = null;
    check = null;
    try {
      // Reuse the open panel's copy where there is one; only the closed
      // case costs a round trip.
      const row = detail?.name === name ? detail : await api.deployment(name);
      source = row.source ?? "";
      selected = null;
      editing = "edit";
      mode = "text";
    } catch (e) {
      actionErr = errText(e);
    }
  }

  async function doCheck() {
    busy = true;
    actionErr = null;
    try {
      check = await api.deploymentPut(source, true);
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
      await api.deploymentPut(source, false);
      const published = check?.name ?? null;
      editing = null;
      check = null;
      await load();
      // The Director half of P1's "watch it roll". The CLI blocks on a
      // redrawing table; here the screen already polls every two seconds,
      // so the equivalent is to open the one that is rolling rather than
      // leave the reader to find it in the list.
      if (published) void open(published);
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  async function load() {
    try {
      rows = await api.deployments();
      err = null;
      if (selected) await loadStatus(selected);
    } catch (e) {
      err = errText(e);
    }
  }

  async function loadStatus(name: string) {
    try {
      status = await api.deploymentStatus(name);
    } catch (e) {
      actionErr = errText(e);
    }
  }

  type SheetTab = "overview" | "activity" | "versions" | "definition";
  const TABS: { id: SheetTab; label: string }[] = [
    { id: "overview", label: "Overview" },
    { id: "activity", label: "Activity" },
    { id: "versions", label: "Versions" },
    { id: "definition", label: "Definition" },
  ];
  let tab = $state<SheetTab>("overview");

  async function open(name: string) {
    selected = name;
    status = null;
    detail = null;
    actionErr = null;
    rollingOut = null;
    history = null;
    diffFor = null;
    tagFor = null;
    tab = "overview";
    activity = [];
    activityLoaded = false;
    activityNote = null;
    void loadStatus(name);
    try {
      detail = await api.deployment(name);
    } catch (e) {
      actionErr = errText(e);
    }
  }

  /** The open row, live from the polled list. */
  const current = $derived(rows.find((d) => d.name === selected) ?? null);
  /** The open deployment's status, once it is the open deployment's. */
  const live = $derived(status && status.name === selected ? status : null);
  const story = $derived(live ? explainNow(live) : null);
  const copies = $derived(live ? copyRows(live) : null);

  /** A step's word for where the copy is on it: past tense once done, the
   *  -ing form while it is happening, and what went wrong where it did — so
   *  a copy with no machine never reads "Placed", and a failing one never
   *  reads "Healthy" in red. */
  const STEP_WORDS = [
    { done: "Placed", now: "Placing", bad: "Not placed", warn: "Placed", todo: "Placed" },
    { done: "Started", now: "Starting", bad: "Stopped", warn: "Started", todo: "Started" },
    { done: "Healthy", now: "Checking", bad: "Failing", warn: "No answer", todo: "Healthy" },
  ] as const;
  type StepState = "done" | "now" | "bad" | "warn" | "todo";
  function stepState(c: CopyRow, k: number): StepState {
    const step = STEP[c.stage];
    if (k < step) return "done";
    if (k > step) return "todo";
    if (c.stage === "failing") return "bad";
    if (c.stage === "away") return "warn";
    // Waiting its turn, or on its way out: nothing is happening to it yet.
    if (c.stage === "next" || c.old) return "todo";
    return "now";
  }

  // Deployments move on their own — instances come and go, rolls advance —
  // so this screen polls, unlike the static Fairness report.
  $effect(() => poll(load, 2000));

  // A deep link from another screen — Load balancing names the deployment
  // behind a service — read once the list is here so the sheet has a row.
  $effect(() => {
    void app.deepLink;
    if (rows.length === 0) return;
    const o = sessionStorage.getItem("deployments:open");
    if (o) {
      sessionStorage.removeItem("deployments:open");
      if (rows.some((d) => d.name === o)) void open(o);
    }
  });

  // The page's action lives in the toolbar, next to the breadcrumb — the
  // same place, at the same size, on every screen.
  $effect(() => chrome.setActions(actions));

  const ROLLING = new Set(["updating", "canarying", "progressing"]);
  function tone(s: string): "ok" | "warn" | "error" | "muted" | "neutral" {
    if (s === "active") return "ok";
    if (s === "degraded") return "error";
    if (ROLLING.has(s)) return "warn";
    if (s === "paused") return "muted";
    return "neutral";
  }

  /** The list's few words: what the status word means for this row. */
  function rowSentence(d: DeploymentView): string {
    switch (d.status) {
      case "active":
        return d.desired === 0 ? "nothing to run" : d.desired === 1 ? "its copy is healthy" : `all ${d.desired} healthy`;
      case "progressing":
        return d.running > d.desired ? `stopping ${d.running - d.desired}` : `starting ${Math.max(1, d.desired - d.healthy)}`;
      case "updating":
        return "rolling out a new version";
      case "canarying":
        return "trying the new version on a canary";
      case "degraded":
        return "copies keep failing";
      case "paused":
        return "not being kept up";
      default:
        return "";
    }
  }

  const desired = $derived(rows.reduce((n, d) => n + d.desired, 0));
  const healthy = $derived(rows.reduce((n, d) => n + d.healthy, 0));
  const rolling = $derived(rows.filter((d) => ROLLING.has(d.status)).length);
  const degraded = $derived(rows.filter((d) => d.status === "degraded").length);

  async function doRollout() {
    if (!rollingOut) return;
    busy = true;
    actionErr = null;
    try {
      await api.deploymentRollout(rollingOut, rolloutVersion);
      rollingOut = null;
      tab = "overview";
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  async function doRollback(name: string) {
    busy = true;
    actionErr = null;
    try {
      await api.deploymentRollback(name);
      tab = "overview";
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  // U2: the versions a deployment keeps — what changed in each, a tag that
  // keeps one past the last ten, and a way back to any of them. Re-read when
  // the generation or status moves, which is when a version appears or
  // changes.
  let history = $state<DeploymentHistoryView | null>(null);
  let historyErr = $state<string | null>(null);
  let diffFor = $state<number | null>(null);
  let tagFor = $state<number | null>(null);
  let tagName = $state("");

  async function loadHistory(name: string) {
    try {
      const h = await api.deploymentHistory(name);
      if (selected !== name) return;
      history = h;
      historyErr = null;
    } catch (e) {
      historyErr = errText(e);
    }
  }

  $effect(() => {
    const name = selected;
    void current?.generation;
    void current?.status;
    if (name) void loadHistory(name);
  });

  async function doRollbackTo(generation: number) {
    if (!selected) return;
    busy = true;
    actionErr = null;
    try {
      await api.deploymentRollback(selected, undefined, { generation });
      diffFor = null;
      tab = "overview";
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  async function doTag(generation: number) {
    const name = selected;
    if (!name || !tagName.trim()) return;
    busy = true;
    actionErr = null;
    try {
      await api.deploymentTag(name, tagName.trim(), { generation });
      tagFor = null;
      tagName = "";
      await loadHistory(name);
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  async function doUntag(tag: string) {
    const name = selected;
    if (!name) return;
    busy = true;
    actionErr = null;
    try {
      await api.deploymentTag(name, tag, { remove: true });
      await loadHistory(name);
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  // E2/U7: the one click a held canary set waits for — for the generation on
  // screen, so a stale screen cannot promote a roll nobody looked at.
  async function doPromote() {
    const s = status;
    if (!s) return;
    busy = true;
    actionErr = null;
    try {
      await api.deploymentPromote(s.name, s.generation);
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  function effectTone(effect: string): "warn" | "neutral" | "muted" {
    if (effect === "rolls") return "warn";
    return effect === "record only" ? "muted" : "neutral";
  }

  // U7: Scale and Restart — the same fetch, edit, re-sign the CLI's
  // `deployment scale` and `deployment restart` make.
  let scaling = $state<string | null>(null);
  let scaleCount = $state<number>(1);
  let restarting = $state<string | null>(null);

  async function doScale() {
    if (!scaling) return;
    busy = true;
    actionErr = null;
    try {
      await api.deploymentScale(scaling, scaleCount);
      scaling = null;
      // Back to where the new copies can be watched arriving.
      tab = "overview";
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  /** Every instance when `slot` is absent; one slot's instance otherwise. */
  async function doRestart(name: string, slot?: string) {
    busy = true;
    actionErr = null;
    try {
      await api.deploymentRestart(name, slot);
      restarting = null;
      tab = "overview";
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  /** The button Right now offers, when it offers one. */
  function actNow(action: NowAction) {
    const d = current;
    if (!d) return;
    switch (action) {
      case "promote":
        void doPromote();
        break;
      case "resume":
        void doPause(d.name, false);
        break;
      case "edit":
        void startEdit(d.name);
        break;
      case "rollback":
        void doRollback(d.name);
        break;
      case "scale":
        scaling = d.name;
        scaleCount = Math.max(1, d.desired);
        break;
    }
  }

  // -- activity -------------------------------------------------------------
  //
  // What people did (the audit rows naming this deployment) and what the
  // copies did (each slot's incarnations), newest first. Re-read every few
  // seconds while the sheet is open, and at once when the set of placed
  // slots changes — which is exactly when something new has happened.
  let activity = $state<Activity[]>([]);
  let activityLoaded = $state(false);
  let activityNote = $state<string | null>(null);

  const slotKey = $derived(
    live ? [...new Set(live.instances.filter((i) => i.state !== "absent").map((i) => i.slot))].join("\u0000") : "",
  );

  $effect(() => {
    const name = selected;
    const slots = slotKey ? slotKey.split("\u0000").slice(0, 32) : [];
    if (!name) return;
    let alive = true;
    const tick = async () => {
      const since = untrack(() => current?.createdAtMicros ?? 0);
      const generation = untrack(() => status?.generation ?? 0);
      const [audit, details] = await Promise.all([
        api
          .audit({ user: null, action: "deployment.", sinceSecs: null, limit: 1000 })
          .then((got) => ({ got, failed: false }))
          .catch(() => ({ got: [] as AuditView[], failed: true })),
        Promise.all(
          slots.map((slot) =>
            api
              .deploymentInstance(name, slot)
              .then((d) => ({ slot, d }))
              .catch(() => null),
          ),
        ),
      ]);
      if (!alive || selected !== name) return;
      const copyEvents = details.flatMap((x) => (x ? copyActivity(x.slot, x.d.incarnations, generation) : []));
      // A second's grace before `created`: the create row and the record
      // are stamped a moment apart.
      activity = mergeActivity(auditActivity(audit.got, name, since - 1_000_000), copyEvents).slice(0, 200);
      activityNote = audit.failed
        ? "Only what the copies did is shown: who changed what is in the audit log, and your access does not include reading it."
        : null;
      activityLoaded = true;
    };
    // `poll`, not a raw interval (DIR-15): up to 33 requests a tick — the
    // audit read and one per copy — kept flowing every five seconds while
    // the window was hidden, and stacked on a slow engine. `poll` skips a
    // hidden window's ticks and never overlaps one with the last.
    const stop = poll(tick, 5000);
    return () => {
      alive = false;
      stop();
    };
  });

  // U1: one instance up close — its timeline, its health, and what it is
  // using, re-measured every few seconds while the panel is open. The trails
  // are kept here, in the panel, because a measurement is never stored
  // anywhere else: it is answered and forgotten.
  let instanceSlot = $state<string | null>(null);
  let instance = $state<DeploymentInstanceDetailView | null>(null);
  let instanceErr = $state<string | null>(null);
  let cpuTrail = $state<number[]>([]);
  let memTrail = $state<number[]>([]);
  const TRAIL = 30;

  function openInstance(slot: string) {
    if (instanceSlot === slot) {
      closeInstance();
      return;
    }
    instanceSlot = slot;
    instance = null;
    instanceErr = null;
    cpuTrail = [];
    memTrail = [];
  }

  function closeInstance() {
    instanceSlot = null;
    instance = null;
  }

  $effect(() => {
    const name = selected;
    const slot = instanceSlot;
    if (!name || !slot) return;
    let alive = true;
    const tick = async () => {
      try {
        const d = await api.deploymentInstance(name, slot);
        if (!alive) return;
        instance = d;
        instanceErr = null;
        const cpu = d.stats?.cpuPct;
        const mem = d.stats?.memBytes;
        if (cpu != null) cpuTrail = [...cpuTrail, cpu].slice(-TRAIL);
        if (mem != null) memTrail = [...memTrail, mem].slice(-TRAIL);
      } catch (e) {
        if (alive) instanceErr = errText(e);
      }
    };
    const stop = poll(tick, 3000);
    return () => {
      alive = false;
      stop();
    };
  });

  $effect(() => {
    if (selected === null && instanceSlot) closeInstance();
  });

  /** A polyline over the trail, scaled to `ceiling` when there is one. */
  function sparkPoints(values: number[], ceiling?: number | null): string {
    if (values.length < 2) return "";
    const top = Math.max(ceiling ?? 0, ...values, 1e-9);
    return values
      .map((v, i) => `${((i / (TRAIL - 1)) * 100).toFixed(2)},${(23 - (v / top) * 21).toFixed(2)}`)
      .join(" ");
  }

  function mib(bytes: number | null | undefined): string {
    return bytes == null ? "—" : `${Math.round(bytes / 1048576)} MiB`;
  }

  // E3: a terminal inside one container instance — the same session, grant,
  // reason and audit as a node's terminal; only what the far node spawns
  // differs. A reason comes first, because the audit log needs one.
  type ShellTarget = { jobId: string; slot: string; node: string };
  let shellFor = $state<ShellTarget | null>(null);
  let shellReason = $state("");
  let shellOpen = $state(false);

  function askShell(inst: ShellTarget) {
    if (shellFor?.jobId === inst.jobId) {
      closeShell();
      return;
    }
    shellFor = { jobId: inst.jobId, slot: inst.slot, node: inst.node };
    shellReason = "";
    shellOpen = false;
  }

  /** Unmounting the terminal is what closes the session on the far node. */
  function closeShell() {
    shellFor = null;
    shellOpen = false;
  }

  $effect(() => {
    if (selected === null && shellFor) closeShell();
  });

  // E3: one instance's output, streamed into the sheet — the same stream
  // Activity follows for any job, because an instance *is* a job.
  type LogTarget = { jobId: string; slot: string; node: string };
  let logFor = $state<LogTarget | null>(null);
  let logText = $state("");
  let logEnd = $state<string | null>(null);
  /** Keep the tail: a chatty instance followed for an hour must not grow
   *  the page without bound. */
  const LOG_CAP = 200_000;

  function openLogs(inst: LogTarget) {
    const was = logFor?.jobId;
    if (was) void api.jobLogsStop(was).catch(() => {});
    if (was === inst.jobId) {
      logFor = null;
      return;
    }
    logFor = { jobId: inst.jobId, slot: inst.slot, node: inst.node };
    logText = "";
    logEnd = null;
    const id = inst.jobId;
    void api
      .jobLogs(id, true, (ev: RunEvent) => {
        if (logFor?.jobId !== id) return;
        if (ev.type === "chunk") {
          const next = logText + ev.text;
          logText = next.length > LOG_CAP ? next.slice(next.length - LOG_CAP) : next;
        } else if (ev.type === "done") {
          logEnd = `${ev.state}${ev.exitCode !== null ? ` · exit ${ev.exitCode}` : ""}`;
        } else if (ev.type === "failed") {
          logEnd = ev.message;
        }
      })
      .catch((e) => (logEnd = errText(e)));
  }

  /** Stop the follow on the engine too — a follow runs until the job ends,
   *  and a pane nobody is looking at must not keep one open. */
  function closeLogs() {
    const was = logFor?.jobId;
    logFor = null;
    if (was) void api.jobLogsStop(was).catch(() => {});
  }

  // Closing the sheet closes its log; leaving the screen closes it too.
  $effect(() => {
    if (selected === null && logFor) closeLogs();
  });
  $effect(() => () => closeLogs());

  async function doPause(name: string, paused: boolean) {
    busy = true;
    actionErr = null;
    try {
      await api.deploymentSetPaused(name, paused);
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  /// The deployment a *Delete* is waiting on.
  ///
  /// Deleting one stops processes on other people's machines and cannot be
  /// undone. Every other destructive verb in this app confirms first; this
  /// one shipped as a single click next to *Edit…*, which is exactly the
  /// neighbour you do not want it to have.
  let deleting = $state<DeploymentView | null>(null);

  async function doRemove(name: string) {
    busy = true;
    actionErr = null;
    try {
      await api.deploymentRemove(name);
      deleting = null;
      if (selected === name) selected = null;
      // An editor left open on the deleted file is one Publish away from
      // making it again (2026-09-14): a delete that "comes back" was a
      // fresh create each time in the op log, never a lost removal.
      editing = null;
      check = null;
      await load();
    } catch (e) {
      actionErr = errText(e);
    } finally {
      busy = false;
    }
  }

  /** Where its copies answer, on Load balancing. */
  function openReach(name: string) {
    sessionStorage.setItem("registry:open", name);
    selected = null;
    app.go("registry");
  }

  // Parse `collector@7` -> 7, so the rollout dialog can default one past it.
  function currentVersion(service: string): number {
    const at = service.lastIndexOf("@");
    return at >= 0 ? Number(service.slice(at + 1)) || 1 : 1;
  }
</script>

{#snippet actions()}
  <button class="btn btn-primary btn-md" onclick={startNew}>
    <Icon name="plus" size={13} /> New deployment
  </button>
{/snippet}

{#if err}
  <LoadError class="mb-3" what="the deployments" error={err} retry={load} />
{/if}
{#if actionErr && !editing && !selected && !deleting}
  <div class="errbox mb-3">{actionErr}</div>
{/if}

{#if rows.length > 0}
  <div class="mb-5 grid gap-3" style="grid-template-columns: repeat(4, minmax(0, 1fr));">
    <div class="card metric" title="Definitions the mesh is keeping up">
      <div class="metric-label">Deployments</div>
      <div class="metric-fig">{rows.length}<span class="sub">kept up</span></div>
      <div class="metric-bar">
        {#each rows as d (d.name)}
          {@const t = tone(d.status)}
          <span
            data-on={t === "ok" ? "" : undefined}
            style={t === "warn" ? "background: var(--status-warn);" : t === "error" ? "background: var(--status-error);" : ""}
            title={`${d.name} · ${d.status}`}
          ></span>
        {/each}
      </div>
    </div>
    <div class="card metric" title="Copies passing their health check, of every copy asked for">
      <div class="metric-label">Copies healthy</div>
      <div class="metric-fig" style={healthy < desired ? "color: var(--status-warn-ink);" : ""}>
        {healthy}<span class="sub">of {desired} asked for</span>
      </div>
      <div class="meter-track mt-[13px] h-[5px]">
        <div class="meter-fill" style={`width:${desired ? (healthy / desired) * 100 : 0}%; background: ${healthy < desired ? "var(--status-warn)" : "var(--status-ok)"};`}></div>
      </div>
    </div>
    <div class="card metric" title="Deployments starting copies or rolling out a new version">
      <div class="metric-label">In motion</div>
      <div class="metric-fig" style={rolling > 0 ? "color: var(--status-warn-ink);" : ""}>
        {rolling}<span class="sub">{rolling === 0 ? "all settled" : "changing"}</span>
      </div>
      <div class="metric-note">{rolling === 0 ? "every deployment is where it should be" : "open one to watch what it is doing"}</div>
    </div>
    <div class="card metric" title="Deployments whose copies keep failing">
      <div class="metric-label">Need a look</div>
      <div class="metric-fig" style={degraded > 0 ? "color: var(--status-error-ink);" : ""}>
        {degraded}<span class="sub">{degraded === 0 ? "none" : "failing"}</span>
      </div>
      <div class="metric-note">{degraded === 0 ? "nothing is failing" : "open one to see why"}</div>
    </div>
  </div>
{/if}

<div class="section-head">
  <span class="section-title">Deployments</span>
  <span class="section-sum">{rows.length === 0 ? (err ? "could not be read" : "none yet") : `${rows.length} defined · open one to see what it is doing`}</span>
</div>

{#if rows.length === 0 && err}
  <!-- The error above is the whole answer (UX-2): "Nothing is being kept
       up" and a Deploy button, under a refusal, told a person the mesh was
       empty and invited them to fill it. -->
{:else if rows.length === 0}
  <div class="card empty">
    <div class="empty-title">Nothing is being kept up</div>
    <div class="empty-sub">
      A deployment declares that N copies of something should stay running, and Meshware keeps
      them there. Add one with <strong>New deployment</strong> above, or with
      <code class="num">meshnode deployments put</code>.
    </div>
    <!-- U14: one click to a file that works, planned before anything starts. -->
    <div class="mt-4 flex justify-center">
      <button class="btn btn-primary btn-md" onclick={() => void startExample()}>Deploy an example</button>
    </div>
    <div class="empty-sub mt-2">
      Two nginx copies with a health check, a canary and a name — as a file you plan first. The
      same one <code class="num">meshnode deployment init</code> prints.
    </div>
  </div>
{:else}
  <div class="card overflow-hidden">
    <table class="w-full text-sm">
      <thead>
        <tr class="border-b border-edge">
          <th class="label px-5 py-2.5 text-left">Deployment</th>
          <th class="label px-3 py-2.5 text-left">Runs</th>
          <th class="label px-3 py-2.5 text-left">Where</th>
          <th class="label w-[170px] px-3 py-2.5 text-left">Copies healthy</th>
          <th class="label w-[240px] px-3 py-2.5 text-left">What it is doing</th>
          <th class="label w-[44px] px-3 py-2.5"></th>
        </tr>
      </thead>
      <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
        {#each rows as d (d.name)}
          {@const t = tone(d.status)}
          <tr
            class="cursor-pointer border-b border-edge/60 transition-colors last:border-0 hover:bg-surface-2"
            style={d.status === "degraded" ? "box-shadow: inset 3px 0 0 var(--status-error);" : ""}
            onclick={() => void open(d.name)}
            use:rowAction={() => void open(d.name)}
          >
            <td class="px-5 py-3 font-semibold text-ink">{d.name}</td>
            <td class="px-3 py-3">
              <span class="mono inline-flex items-center gap-1.5 text-xs text-ink-3" style={hueStyle(hueOf(d.service.split("@")[0]))}>
                <span class="name-dot"></span>{d.service}
              </span>
            </td>
            <td class="px-3 py-3">
              <span class="chip">{d.count === "system" ? `every ${d.targets === "any" ? "machine" : d.targets}` : d.targets === "any" ? "any machine" : d.targets}</span>
            </td>
            <td class="px-3 py-3">
              <span class="inline-flex items-center gap-2.5" title="healthy of asked for; amber cells are running and not yet healthy">
                <Segments
                  total={Math.max(d.desired, d.running)}
                  on={d.healthy}
                  warn={Math.max(0, d.running - d.healthy)}
                  tone={t === "error" ? "var(--status-error)" : "var(--status-ok)"}
                />
                <span class="num text-xs" style={`color: ${d.healthy >= d.desired ? "var(--status-ok-ink)" : t === "error" ? "var(--status-error-ink)" : "var(--status-warn-ink)"};`}>
                  {d.healthy}<span class="text-ink-5">/{d.desired}</span>
                </span>
              </span>
            </td>
            <td class="px-3 py-3">
              <span class="flex flex-wrap items-center gap-x-2 gap-y-1">
                <Pill tone={t} live={t === "warn"}>{d.status}</Pill>
                <span class="text-xs text-ink-4">{rowSentence(d)}</span>
              </span>
            </td>
            <td class="px-3 py-3 text-right"><span class="rowchev inline-flex"><Icon name="chevron-right" size={14} /></span></td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
  <div class="footnote">
    Open a deployment for what Meshware is doing with it right now, why, and what happens next.
  </div>
{/if}

<!-- ═══════════════ one deployment, up close ═══════════════ -->

<!-- One copy: who it is, how far along Placed → Started → Healthy it has
     got, what that means in words, and the verbs for it. The panels it opens
     — logs, shell, the close-up — open under the copy they belong to rather
     than at the bottom of the sheet, where they used to land out of sight. -->
{#snippet copyRow(c: CopyRow, s: DeploymentStatusView)}
  {@const i = c.inst}
  {@const placed = !!i && i.state !== "absent"}
  <div class="dp-copy" data-stage={c.stage} class:dp-copy-old={c.old}>
    <div class="dp-copy-row">
      <div class="dp-copy-id">
        <span class="dp-copy-name">{c.name}</span>
        {#if i?.node}
          <span class="dp-copy-node mono" style={hueStyle(hueOf(i.node))}><span class="name-dot"></span>{i.node}</span>
        {:else}
          <span class="dp-copy-node">no machine yet</span>
        {/if}
      </div>
      <ol class="dp-track" aria-label={`${c.name}: ${c.text}`}>
        {#each STEP_WORDS as words, k (k)}
          {@const state = stepState(c, k)}
          <li class="dp-step" data-state={state}>
            <span class="dp-step-dot"></span><span class="dp-step-label">{words[state]}</span>
          </li>
        {/each}
      </ol>
      <div class="dp-copy-text">
        <span class="dp-copy-say">{c.text}</span>
        {#if c.why}<span class="dp-copy-why">{c.why}</span>{/if}
        {#if i?.startedAtMicros && placed}<span class="dp-copy-when num">started {ago(i.startedAtMicros)}</span>{/if}
      </div>
      <div class="dp-copy-actions">
        {#if placed && i}
          <button
            class="btn btn-ghost h-7 px-2 text-xs"
            title="Stream this copy's output — `meshnode deployment logs --slot`"
            onclick={() => openLogs({ jobId: i.jobId, slot: i.slot, node: i.node })}>{logFor?.jobId === i.jobId ? "Hide logs" : "Logs"}</button
          >
          {#if !c.old}
            {#if s.runtime === "container" && i.state === "running"}
              <button
                class="btn btn-ghost h-7 px-2 text-xs"
                title="A terminal inside this copy's container — `meshnode deployment exec --slot`"
                onclick={() => askShell({ jobId: i.jobId, slot: i.slot, node: i.node })}>{shellFor?.jobId === i.jobId ? "Close shell" : "Shell"}</button
              >
            {/if}
            <button
              class="btn btn-ghost h-7 px-2 text-xs"
              disabled={busy}
              title="Replace just this copy — Meshware starts another in its place"
              onclick={() => void doRestart(s.name, i.slot)}>Restart</button
            >
            <button
              class="btn btn-ghost h-7 px-2 text-xs"
              aria-expanded={instanceSlot === c.slot}
              title="This copy up close: what it is using, and every time it has been started"
              onclick={() => openInstance(c.slot)}>{instanceSlot === c.slot ? "Less" : "More"}</button
            >
          {/if}
        {/if}
      </div>
    </div>

    {#if i && logFor?.jobId === i.jobId}
      <div class="dp-panel">
        <div class="dp-panel-bar">
          <span class="font-semibold text-ink-2">Logs</span>
          <span class="mono text-ink-5">{logFor.node}</span>
          {#if logEnd}
            <span class="text-ink-4">· {logEnd}</span>
          {:else}
            <Pill tone="running" live>following</Pill>
          {/if}
          <button class="btn btn-ghost ml-auto h-7 px-2 text-xs" onclick={closeLogs}>Close</button>
        </div>
        <pre class="pre selectable m-0 max-h-[320px] overflow-auto px-4 py-3 font-mono text-xs leading-relaxed text-ink-3" style="background: var(--surface);">{logText || "Waiting for output…"}</pre>
      </div>
    {/if}

    <!-- E3: the reason is asked for first and travels in the signature; the
         far node checks the grant and that the job is a container it runs. -->
    {#if i && shellFor?.jobId === i.jobId}
      <div class="dp-panel">
        <div class="dp-panel-bar">
          <span class="font-semibold text-ink-2">Shell</span>
          <span class="mono text-ink-5">inside its container on {shellFor.node}</span>
          <button class="btn btn-ghost ml-auto h-7 px-2 text-xs" onclick={closeShell}>{shellOpen ? "End session" : "Cancel"}</button>
        </div>
        {#if !shellOpen}
          <div class="flex flex-wrap items-center gap-2 px-4 py-3">
            <input
              class="h-9 min-w-[260px] flex-1 rounded-md border border-edge bg-transparent px-3 text-sm text-ink outline-none"
              placeholder="Why — a ticket, an incident, a sentence. Recorded in the audit log."
              bind:value={shellReason}
              onkeydown={(e) => {
                if (e.key === "Enter" && shellReason.trim()) shellOpen = true;
              }}
            />
            <button class="btn btn-primary btn-md" disabled={!shellReason.trim()} onclick={() => (shellOpen = true)}>Open shell</button>
          </div>
        {:else}
          <Terminal node={shellFor.node} job={shellFor.jobId} reason={shellReason} label={`copy ${shellFor.slot}`} />
        {/if}
      </div>
    {/if}

    <!-- U1: what it is using, and every incarnation the slot has had. -->
    {#if !c.old && instanceSlot === c.slot && placed}
      <div class="dp-panel">
        {#if instanceErr}
          <div class="errbox m-3 text-xs">{instanceErr}</div>
        {:else if !instance}
          <div class="px-4 py-4 text-xs text-ink-5">loading…</div>
        {:else}
          <div class="grid gap-4 px-4 py-3" style="grid-template-columns: repeat(2, minmax(0, 1fr));">
            <div>
              <div class="microlabel mb-1">CPU</div>
              <div class="num text-sm text-ink-2">{instance.stats?.cpuPct != null ? `${instance.stats.cpuPct.toFixed(1)}%` : "—"}</div>
              <svg viewBox="0 0 100 24" preserveAspectRatio="none" class="mt-1 h-6 w-full" aria-hidden="true">
                <polyline points={sparkPoints(cpuTrail, 100)} fill="none" stroke="var(--accent-ink)" stroke-width="1.5" vector-effect="non-scaling-stroke" />
              </svg>
            </div>
            <div>
              <div class="microlabel mb-1">Memory</div>
              <div class="num text-sm text-ink-2">
                {mib(instance.stats?.memBytes)}{instance.stats?.memLimitBytes != null ? ` of ${mib(instance.stats.memLimitBytes)}` : ""}
              </div>
              <svg viewBox="0 0 100 24" preserveAspectRatio="none" class="mt-1 h-6 w-full" aria-hidden="true">
                <polyline points={sparkPoints(memTrail, instance.stats?.memLimitBytes)} fill="none" stroke="var(--accent-ink)" stroke-width="1.5" vector-effect="non-scaling-stroke" />
              </svg>
            </div>
          </div>
          <div class="px-4 pb-2 text-xs text-ink-5">
            {#if !instance.stats}
              Not measured — nothing is running here, or its machine could not be asked.
            {:else if instance.stats.source === "process"}
              Measured from its main process only; anything it forked is not counted.
            {:else if instance.stats.source}
              Measured from its {instance.stats.source} on {instance.current?.node}, every few seconds while this is open.
            {:else}
              Its machine could not read what it is using.
            {/if}
          </div>
          <div class="microlabel mt-2 mb-1 px-4">Every time it has been started</div>
          <table class="w-full text-xs">
            <tbody>
              {#each instance.incarnations as inc (inc.jobId)}
                <tr class="border-t border-edge/60">
                  <td class="num px-4 py-1.5 text-ink-5">gen {inc.generation} · #{inc.incarnation + 1}</td>
                  <td class="mono px-3 py-1.5 text-ink-3">{inc.node}</td>
                  <td class="px-3 py-1.5">
                    <Pill tone={inc.state === "running" ? "running" : inc.state === "failed" || inc.state === "timed-out" ? "error" : "muted"}>{inc.state}</Pill>
                  </td>
                  <td class="num px-3 py-1.5 text-ink-5">{inc.startedAtMicros ? `started ${ago(inc.startedAtMicros)}` : "not started"}</td>
                  <td class="num px-4 py-1.5 text-right text-ink-5">
                    {inc.finishedAtMicros ? `ended ${ago(inc.finishedAtMicros)}${inc.exitCode != null ? ` · exit ${inc.exitCode}` : ""}` : ""}
                  </td>
                </tr>
              {:else}
                <tr><td class="px-4 py-3 text-ink-5">It has not been started yet.</td></tr>
              {/each}
            </tbody>
          </table>
        {/if}
      </div>
    {/if}
  </div>
{/snippet}

{#snippet timeline(items: Activity[])}
  <ol class="dp-timeline">
    {#each items as a (a.key)}
      <li class="dp-event" data-tone={a.tone}>
        <span class="dp-event-dot" aria-hidden="true"></span>
        <span class="dp-event-text">{#if a.who}<b>{a.who}</b>{" "}{/if}{a.text}</span>
        <time class="dp-event-when num" title={new Date(a.at / 1000).toLocaleString()}>{ago(a.at)}</time>
      </li>
    {/each}
  </ol>
{/snippet}

<Sheet
  open={selected !== null && !editing}
  title={current?.name ?? selected ?? ""}
  sub={current
    ? `${current.service} · ${current.count === "system" ? "one copy on every matching machine" : `${current.desired} cop${current.desired === 1 ? "y" : "ies"}`} · ${current.targets === "any" ? "may run on any machine" : `may run on ${current.targets}`}`
    : ""}
  icon="deployments"
  width={980}
  onclose={() => (selected = null)}
>
  {#snippet aside()}
    {#if current}
      {@const t = tone(current.status)}
      <Pill tone={t} live={t === "warn"}>{current.status}</Pill>
      <span class="chip" title="Generation: goes up by one every time what it runs changes">generation {current.generation}</span>
    {/if}
  {/snippet}

  <div class="dp-tabs" role="tablist" aria-label="About this deployment">
    {#each TABS as t (t.id)}
      <button class="dp-tab" role="tab" aria-selected={tab === t.id} onclick={() => (tab = t.id)}>
        {t.label}
        {#if t.id === "versions" && history}<span class="dp-tab-n num">{history.versions.length}</span>{/if}
      </button>
    {/each}
  </div>

  {#if actionErr}
    <div class="errbox mb-4">{actionErr}</div>
  {/if}

  {#if current && scaling === current.name}
    <!-- U7: the size, as one line — how many, and go. -->
    <div class="dp-inline" style="background: var(--hue-wash, var(--accent-wash));">
      <span class="text-sm font-semibold text-ink">Run</span>
      <input type="number" min="1" bind:value={scaleCount} class="num h-9 w-20 rounded-md border border-edge bg-transparent px-2 text-sm text-ink outline-none" />
      <span class="text-sm font-semibold text-ink">copies</span>
      <span class="text-xs text-ink-5">now {current.desired} · the copies that stay are not restarted</span>
      <button class="btn btn-primary btn-md ml-auto" disabled={busy || scaleCount < 1 || scaleCount === current.desired} onclick={() => void doScale()}>{busy ? "Scaling…" : "Scale"}</button>
      <button class="btn btn-secondary btn-md" disabled={busy} onclick={() => (scaling = null)}>Cancel</button>
    </div>
  {/if}

  {#if current && restarting === current.name}
    <!-- U7: replacing every copy is worth one confirmation. -->
    <div class="dp-inline" style="background: var(--status-warn-wash, var(--accent-wash));">
      <span class="text-sm font-semibold text-ink">Restart every copy?</span>
      <span class="text-xs text-ink-5">Replaced a few at a time, each only once its replacement is healthy. What it runs does not change.</span>
      <button
        class="btn btn-primary btn-md ml-auto"
        disabled={busy}
        onclick={() => {
          if (restarting) void doRestart(restarting);
        }}>{busy ? "Restarting…" : "Restart all"}</button
      >
      <button class="btn btn-secondary btn-md" disabled={busy} onclick={() => (restarting = null)}>Cancel</button>
    </div>
  {/if}

  {#if current && rollingOut === current.name}
    <!-- The roll, as one line: which version, and go. -->
    <div class="dp-inline" style="background: var(--hue-wash, var(--accent-wash));">
      <span class="text-sm font-semibold text-ink">Roll {current.name} out to</span>
      <span class="mono inline-flex items-center overflow-hidden rounded-md" style="box-shadow: 0 0 0 1px var(--edge-strong); background: var(--surface-1);">
        <span class="px-2.5 text-sm text-ink-4">{current.service.split("@")[0]}@</span>
        <input type="number" min="1" bind:value={rolloutVersion} class="num h-9 w-16 border-0 border-l border-edge bg-transparent px-2 text-sm text-ink outline-none" />
      </span>
      <span class="text-xs text-ink-5">from {current.service}</span>
      <button class="btn btn-primary btn-md ml-auto" disabled={busy} onclick={() => void doRollout()}>{busy ? "Starting…" : "Start the roll"}</button>
      <button class="btn btn-secondary btn-md" disabled={busy} onclick={() => (rollingOut = null)}>Cancel</button>
    </div>
  {/if}

  {#if tab === "overview"}
    {#if !live || !story || !copies}
      <div class="dp-skeleton" aria-busy="true">
        <div class="dp-skel dp-skel-hero"></div>
        <div class="dp-skel dp-skel-bar"></div>
        <div class="dp-skel dp-skel-row"></div>
        <div class="dp-skel dp-skel-row"></div>
      </div>
    {:else}
      <!-- Right now: the one thing to know, why, and what happens next. -->
      <section class="dp-now" data-tone={story.tone} aria-live="polite">
        <span class="dp-now-mark" aria-hidden="true">
          {#if story.tone === "ok"}
            <Icon name="check" size={18} />
          {:else if story.tone === "problem"}
            <Icon name="alert" size={18} />
          {:else if story.tone === "paused"}
            <Icon name="pause" size={18} />
          {:else if story.tone === "waiting" && story.action}
            <Icon name="history" size={18} />
          {:else}
            <Spinner size={18} thickness={2.5} color="var(--tone)" track="color-mix(in srgb, var(--tone) 22%, transparent)" />
          {/if}
        </span>
        <div class="min-w-0 flex-1">
          <div class="microlabel">Right now</div>
          <h3 class="dp-now-head">{story.headline}</h3>
          <p class="dp-now-detail">{story.detail}</p>
          {#if story.why.length > 0}
            <ul class="dp-now-why">
              {#each story.why as w (w)}<li>{w}</li>{/each}
            </ul>
          {/if}
          {#if story.next}
            <p class="dp-now-next"><span>What happens next</span>{story.next}</p>
          {/if}
        </div>
        {#if story.action}
          <button
            class={`btn ${story.action === "promote" || story.action === "resume" ? "btn-primary" : "btn-secondary"} btn-md dp-now-act`}
            disabled={busy}
            onclick={() => actNow(story.action!)}
          >
            {story.action === "promote"
              ? `Promote generation ${live.generation}`
              : story.action === "resume"
                ? "Resume"
                : story.action === "rollback"
                  ? "Roll back"
                  : story.action === "scale"
                    ? "Scale…"
                    : "Edit…"}
          </button>
        {/if}
      </section>

      <!-- Where the copies are: one cell per copy, coloured by how far along
           it is, so a Scale is seen arriving rather than a count changing. -->
      <section class="dp-block" aria-label="Where the copies are">
        <div class="dp-h">
          <span>Where the copies are</span>
          <span class="dp-hsub">
            {copies.tally.healthy} of {live.desired} healthy{copies.old.length > 0 ? ` · ${copies.old.length} of the old version still serving` : ""}
          </span>
        </div>
        <div class="dp-bar">
          {#each copies.current as c (c.key)}
            <span class="dp-cell" data-stage={c.stage} title={`${c.name} · ${c.text}`}></span>
          {/each}
          {#each copies.old as c (c.key)}
            <span class="dp-cell dp-cell-old" data-stage={c.stage} title={`${c.name} (old version) · ${c.text}`}></span>
          {/each}
        </div>
        <div class="dp-legend">
          {#each STAGES.filter((st) => copies.tally[st.id] > 0 || st.id === "healthy") as st (st.id)}
            <span class="dp-legend-item"><i class="dp-dot" data-stage={st.id}></i>{st.label} <b class="num">{copies.tally[st.id]}</b></span>
          {/each}
          {#if copies.old.length > 0}
            <span class="dp-legend-item"><i class="dp-dot dp-cell-old" data-stage="healthy"></i>Old version <b class="num">{copies.old.length}</b></span>
          {/if}
        </div>
      </section>

      <section class="dp-block" aria-label="Copies">
        <div class="dp-h">
          <span>Copies</span>
          <span class="dp-hsub">each one goes placed → started → healthy</span>
        </div>
        <div class="dp-copies">
          {#each copies.current as c (c.key)}
            {@render copyRow(c, live)}
          {:else}
            <div class="px-4 py-6 text-center text-xs text-ink-5">It asks for no copies.</div>
          {/each}
          {#if copies.old.length > 0}
            <div class="dp-subhead">
              Being replaced — generation {live.updatingFrom ?? copies.old[0].inst?.generation}
              <span>stopped once their replacements are healthy</span>
            </div>
            {#each copies.old as c (c.key)}
              {@render copyRow(c, live)}
            {/each}
          {/if}
        </div>
      </section>

      <section class="dp-block" aria-label="Recently">
        <div class="dp-h">
          <span>Recently</span>
          {#if activity.length > 5}
            <button class="linkish dp-h-link" onclick={() => (tab = "activity")}>All activity →</button>
          {/if}
        </div>
        {#if activity.length === 0}
          <div class="text-xs text-ink-5">{activityLoaded ? "Nothing recorded yet." : "Loading…"}</div>
        {:else}
          {@render timeline(activity.slice(0, 5))}
        {/if}
      </section>

      {#if live.policyBlind.length > 0}
        <div class="errbox mt-5 text-xs">
          <span class="font-semibold">Some machines are too old for this deployment's rollout settings:</span>
          {live.policyBlind.join(", ")}. If one of them ends up in charge of it, it rolls out the old way —
          without a canary, a progress deadline, lost_after or manual promotion.
        </div>
      {/if}

      <section class="dp-block" aria-label="How it is set up">
        <div class="dp-h"><span>How it is set up</span></div>
        <div class="dp-facts" style="--key-w: 180px;">
          <div class="defrow"><span class="k">Runs</span><span class="v mono">{live.service}{live.runtime ? ` · ${live.runtime}` : ""}</span></div>
          <div class="defrow">
            <span class="k">Copies</span>
            <span class="v">{live.count === "system" ? "One on every machine it may run on" : `${live.desired}`}</span>
          </div>
          <div class="defrow">
            <span class="k">May run on</span>
            <span class="v">{#if live.targets === "any"}Any machine{:else}<code class="num">{live.targets}</code>{/if}</span>
          </div>
          {#if live.portLabels.length > 0}
            <div class="defrow">
              <span class="k">Ports</span>
              <span class="v">
                {#each live.portLabels as p (p)}<span class="hue-chip mr-1" style={hueStyle(hueOf(p))}>{p}</span>{/each}
                <button class="linkish ml-1 text-sm" onclick={() => openReach(live.name)}>Where it answers →</button>
              </span>
            </div>
          {/if}
          <div class="defrow"><span class="k">If a machine goes away</span><span class="v">{live.availability}</span></div>
          <div class="defrow">
            <span class="k">Looking after it</span>
            <span class="v">
              {#if live.reconciler}
                <b>{live.reconciler}</b> <span class="text-ink-5">— the machine currently in charge of keeping it at size</span>
              {:else}
                <span style="color: var(--status-warn-ink);">No machine is in charge right now — changes wait until one is</span>
              {/if}
            </span>
          </div>
          {#if current}
            <div class="defrow" style="border-bottom: none;"><span class="k">Created</span><span class="v">by {current.createdBy}, {ago(current.createdAtMicros)}</span></div>
          {/if}
        </div>
      </section>
    {/if}
  {:else if tab === "activity"}
    <div class="dp-h">
      <span>Activity</span>
      <span class="dp-hsub">what people and Meshware did, newest first</span>
    </div>
    {#if activityNote}
      <div class="dp-note">{activityNote}</div>
    {/if}
    {#if activity.length === 0}
      <div class="py-6 text-center text-xs text-ink-5">{activityLoaded ? "Nothing recorded yet." : "Loading…"}</div>
    {:else}
      {@render timeline(activity)}
    {/if}
  {:else if tab === "versions"}
    <!-- U2: the versions it keeps. A rollback here goes back to a whole
         version — image, ports, resources and rollout settings as they were
         published together — and a tag keeps one past the last ten. -->
    <section aria-label="Versions">
      <div class="dp-h">
        <span>Versions</span>
        <span class="dp-hsub">the last 10 are kept, and any a tag names</span>
      </div>
      {#if historyErr}
        <div class="errbox mt-2 text-xs">{historyErr}</div>
      {:else if !history}
        <div class="py-3 text-xs text-ink-5">loading…</div>
      {:else}
        <div class="dp-copies overflow-x-auto">
          <table class="w-full text-xs">
            <tbody>
              {#each history.versions as v (v.generation)}
                <tr class="border-t border-edge/60 first:border-0">
                  <td class="num whitespace-nowrap py-2 pr-3 pl-4 text-ink-3">gen {v.generation}</td>
                  <td class="mono whitespace-nowrap px-3 py-2 text-ink-2">{v.service} · {v.count === "system" ? "system" : `×${v.count}`}</td>
                  <td class="px-3 py-2">
                    <div class="flex flex-wrap items-center gap-1.5">
                      {#if v.current}<Pill tone="ok">running</Pill>{/if}
                      {#each v.tags as t (t)}
                        <button class="btn btn-ghost mono h-6 px-1.5 text-xs" title={`Remove the tag ${t}`} disabled={busy} onclick={() => void doUntag(t)}>{t} ×</button>
                      {/each}
                      {#if v.keptByTag}<span class="text-ink-5">kept by its tag</span>{/if}
                    </div>
                  </td>
                  <td class="num whitespace-nowrap px-3 py-2 text-ink-5">{v.publishedBy} · {ago(v.publishedAtMicros)}</td>
                  <td class="whitespace-nowrap py-2 pr-4 pl-3 text-right">
                    {#if v.changes.length > 0}
                      <button class="btn btn-ghost h-7 px-2 text-xs" aria-expanded={diffFor === v.generation} onclick={() => (diffFor = diffFor === v.generation ? null : v.generation)}>{v.changes.length} {v.changes.length === 1 ? "change" : "changes"}</button>
                    {/if}
                    <button
                      class="btn btn-ghost h-7 px-2 text-xs"
                      onclick={() => {
                        tagFor = tagFor === v.generation ? null : v.generation;
                        tagName = "";
                      }}>Tag…</button
                    >
                    {#if !v.current}
                      <button class="btn btn-secondary h-7 px-2 text-xs" disabled={busy} onclick={() => void doRollbackTo(v.generation)}>Roll back to this</button>
                    {/if}
                  </td>
                </tr>
                {#if diffFor === v.generation}
                  <tr>
                    <td colspan="5" class="px-4 pb-2">
                      <div class="rounded-md px-3 py-2" style="background: var(--surface-2);">
                        {#each v.changes as c (c.field)}
                          <div class="flex flex-wrap items-baseline gap-x-2 py-0.5">
                            <span class="mono text-ink-2">{c.field}</span>
                            <span class="mono text-ink-5">{c.from || "(unset)"} → {c.to || "(unset)"}</span>
                            <Pill tone={effectTone(c.effect)}>{c.effect}</Pill>
                          </div>
                        {/each}
                      </div>
                    </td>
                  </tr>
                {/if}
                {#if tagFor === v.generation}
                  <tr>
                    <td colspan="5" class="px-4 pb-2">
                      <div class="flex flex-wrap items-center gap-2">
                        <input
                          id="version-tag"
                          class="h-8 min-w-[200px] flex-1 rounded-md border border-edge bg-transparent px-3 text-sm text-ink outline-none"
                          placeholder="A name for generation {v.generation} — golden, pre-migration"
                          bind:value={tagName}
                          onkeydown={(e) => {
                            if (e.key === "Enter") void doTag(v.generation);
                          }}
                        />
                        <button class="btn btn-primary h-8 px-3 text-xs" disabled={busy || !tagName.trim()} onclick={() => void doTag(v.generation)}>Tag it</button>
                      </div>
                    </td>
                  </tr>
                {/if}
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </section>
  {:else if tab === "definition"}
    <div class="dp-h">
      <span>Definition</span>
      <span class="dp-hsub">the file as it was published</span>
      {#if current}
        <button class="btn btn-secondary dp-h-link h-8 px-3 text-xs" disabled={busy} onclick={() => void startEdit(current.name)}>Edit…</button>
      {/if}
    </div>
    {#if detail?.source}
      <pre class="pre selectable m-0 overflow-x-auto rounded-lg px-4 py-3 font-mono text-xs leading-relaxed text-ink-3" style="background: var(--surface-2);">{detail.source}</pre>
    {:else}
      <div class="py-6 text-center text-xs text-ink-5">{detail ? "No definition was kept for this deployment." : "loading…"}</div>
    {/if}
  {/if}

  {#snippet footer()}
    {#if current}
      <button
        class="btn btn-secondary btn-md"
        disabled={busy || current.count === "system"}
        title={current.count === "system" ? "One copy on every matching machine — there is no size to change; change where it may run" : "Change how many copies run; none that stays restarts"}
        onclick={() => {
          scaling = current.name;
          scaleCount = current.desired;
          restarting = null;
          rollingOut = null;
          actionErr = null;
        }}>Scale…</button
      >
      <button
        class="btn btn-secondary btn-md"
        disabled={busy || current.status === "paused"}
        title={current.status === "paused" ? "Paused — resume it first, or the restart waits for nobody" : "Replace every copy a few at a time, without changing what it runs"}
        onclick={() => {
          restarting = current.name;
          scaling = null;
          rollingOut = null;
          actionErr = null;
        }}>Restart…</button
      >
      <button
        class="btn btn-secondary btn-md"
        disabled={busy || rollingOut === current.name}
        onclick={() => {
          rollingOut = current.name;
          rolloutVersion = currentVersion(current.service) + 1;
          scaling = null;
          restarting = null;
          actionErr = null;
        }}>Roll out…</button
      >
      <button class="btn btn-secondary btn-md" disabled={busy || current.generation === 0} title={current.generation === 0 ? "Nothing to roll back to yet" : "Return to the previous version"} onclick={() => void doRollback(current.name)}>Roll back</button>
      <button class="btn btn-secondary btn-md" disabled={busy} onclick={() => void doPause(current.name, current.status !== "paused")}>
        {current.status === "paused" ? "Resume" : "Pause"}
      </button>
      <button class="btn btn-secondary btn-md ml-auto" disabled={busy} onclick={() => void startEdit(current.name)}>Edit…</button>
      <button class="btn btn-danger btn-md" disabled={busy} onclick={() => (deleting = current)}>Delete</button>
    {/if}
  {/snippet}
</Sheet>

<!-- ═══════════════ the editor ═══════════════ -->
<!-- Form or file, and the form is the default for a *new* one. Editing an
     existing deployment opens on the text, because what is already
     published is a file somebody wrote and re-deriving a form from it
     would silently drop anything the form cannot express. -->
<Sheet
  open={editing !== null}
  title={editing === "edit" ? "Edit deployment" : "New deployment"}
  sub={mode === "form"
    ? "Describe what should stay up. Plan first to see where it would land — nothing starts until you publish."
    : "One file, the same one the CLI takes. Plan first to see where it would land — nothing starts until you publish."}
  icon="deployments"
  width={1040}
  locked={busy}
  onclose={() => {
    editing = null;
    check = null;
  }}
>
  {#snippet aside()}
    {#if editing === "new"}
      <div class="seg" role="tablist" aria-label="How to write it">
        <button role="tab" aria-selected={mode === "form"} onclick={() => (mode = "form")}>Guided</button>
        <button role="tab" aria-selected={mode === "text"} onclick={() => (mode = "text")}>File</button>
      </div>
    {/if}
  {/snippet}

  {#if actionErr}
    <div class="errbox mb-4">{actionErr}</div>
  {/if}

  {#if mode === "form"}
    <DeploymentBuilder
      services={serviceOptions}
      matches={check?.matchingNodes ?? null}
      onchange={(y) => (source = y)}
      ontext={(y) => {
        source = y;
        mode = "text";
      }}
    />
  {:else}
    <SpecEditor
      noun="deployment"
      validate={api.deploymentValidate}
      schema={api.deploymentSchema}
      bind:value={source}
      rows={18}
      {busy}
      planned={check != null}
      submitLabel={editing === "edit" ? "Save and roll" : "Publish"}
      onplan={doCheck}
      onsubmit={doPublish}
    />
  {/if}

  {#if check}
    <div class="microlabel mt-5 mb-2">The plan</div>
    <div class="overflow-hidden rounded-lg" style="box-shadow: 0 0 0 1px var(--edge-card);">
      <div class="defrow" style="--key-w: 120px;"><span class="k">Service</span><span class="v mono">{check.service}</span></div>
      <div class="defrow" style="--key-w: 120px;"><span class="k">Count</span><span class="v">{check.count}</span></div>
      <div class="defrow" style="--key-w: 120px;">
        <span class="k">Targets</span>
        <span class="v"><span class="mono">{check.targets}</span> — matches {check.matchingNodes} node{check.matchingNodes === 1 ? "" : "s"} now</span>
      </div>
      <div class="defrow" style="--key-w: 120px;">
        <span class="k">Health check</span>
        <span class="v">{check.health ?? "none (instances judged live by their job alone)"}</span>
      </div>
      <!-- Phase A, U6: said before publishing, defaults included. -->
      <div class="defrow" style="--key-w: 120px; border-bottom: none;">
        <span class="k">If a node goes</span>
        <span class="v">{check.availability}</span>
      </div>
    </div>

    <!-- E5 / U3: what publishing changes, against what is published — and
         the one distinction worth reading first, whether it restarts. -->
    <div class="microlabel mt-4 mb-2">What changes</div>
    {#if check.firstPublish}
      <div class="text-sm text-ink-4">First publish — nothing by this name is running yet.</div>
    {:else if check.changes.length === 0}
      <div class="text-sm text-ink-4">Nothing. Publishing this changes nothing and rolls nothing.</div>
    {:else}
      <div class="mb-2 text-sm" style={check.rolls ? "color: var(--status-warn-ink);" : "color: var(--ink-3);"}>
        {check.rolls ? "Publishing this rolls every instance, in batches." : "No instance restarts."}
      </div>
      <div class="overflow-hidden rounded-lg" style="box-shadow: 0 0 0 1px var(--edge-card);">
        <table class="w-full text-xs">
          <tbody>
            {#each check.changes as c (c.field)}
              <tr class="border-b border-edge/60 last:border-0">
                <td class="mono px-3 py-2 text-ink-2">{c.field}</td>
                <td class="mono px-3 py-2 text-ink-5">{c.from || "—"}</td>
                <td class="py-2 text-ink-5">→</td>
                <td class="mono px-3 py-2 text-ink-2">{c.to || "—"}</td>
                <td class="px-3 py-2 text-right">
                  <Pill tone={c.effect === "rolls" ? "warn" : c.effect === "record only" ? "muted" : "neutral"}>{c.effect}</Pill>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
    {#if check.placement}
      <div class="mt-3 rounded-lg px-4 py-3 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">
        <div class="font-semibold">Nothing can take it right now</div>
        <ul class="m-0 mt-1 list-none p-0 text-xs leading-relaxed">
          {#each check.placement.lines as line}
            <li>· {line}</li>
          {/each}
        </ul>
      </div>
    {/if}
    {#if check.issues.length > 0}
      <ul class="mt-3 mb-0 list-none space-y-1 p-0 text-sm" style="color: var(--status-error-ink);">
        {#each check.issues as issue}
          <li>· {issue}</li>
        {/each}
      </ul>
    {:else if !check.placement}
      <div class="mt-3 flex items-center gap-2 text-sm" style="color: var(--status-ok-ink);">
        <Icon name="check" size={14} /> No problems — this deployment is ready to publish.
      </div>
    {:else}
      <div class="mt-3 text-sm text-ink-5">The definition is valid; the mesh has nowhere to run it yet.</div>
    {/if}
  {/if}

  {#snippet footer()}
    {#if mode === "form"}
      <span class="text-xs text-ink-5">{check ? "Planned — publish when it reads right." : "Plan first to see what it would do — nothing starts."}</span>
      <button class="btn btn-secondary btn-md ml-auto" disabled={busy} onclick={() => void doCheck()}>{busy ? "Planning…" : "Plan"}</button>
      <button class="btn btn-primary btn-md" disabled={busy} onclick={() => void doPublish()}>{editing === "edit" ? "Save and roll" : "Publish"}</button>
    {:else}
      <span class="text-xs text-ink-5">Plan and publish are in the editor above.</span>
      <button class="btn btn-secondary btn-md ml-auto" disabled={busy} onclick={() => { editing = null; check = null; }}>Close</button>
    {/if}
  {/snippet}
</Sheet>

<!-- Deleting a deployment stops its instances everywhere. The count is
     repeated back because that is the number people misjudge: "delete
     log-shipper" reads much smaller than "stop 4 running instances".

     Styled to `Library.svelte`'s idiom, not invented: `Modal` is an
     inverse surface — dark in both themes — so the page's `text-ink`
     classes render as low-contrast mud on it, and it carries no padding of
     its own. Keep-it on the left, the destructive one on the right. -->
<Modal
  open={deleting !== null}
  kicker="delete deployment"
  peer={deleting?.name ?? ""}
  onclose={() => (deleting = null)}
>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Delete {deleting?.name}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      It keeps {deleting?.running ?? 0} cop{(deleting?.running ?? 0) === 1 ? "y" : "ies"}
      of <span class="mono">{deleting?.service}</span> running. Deleting it stops
      them and forgets the definition.
    </p>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-4);">
      To stop it keeping them up without losing the definition, use
      <strong>Pause</strong> instead.
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

<style>
  /* -- tabs ----------------------------------------------------------------
     Pinned under the sheet's header while its body scrolls: the negative
     margins undo the body's padding so the rule runs edge to edge. */
  .dp-tabs {
    position: sticky;
    top: -20px;
    z-index: 5;
    display: flex;
    gap: 2px;
    margin: -20px -24px 18px;
    padding: 0 18px;
    background: var(--surface-1);
    border-bottom: 1px solid var(--edge);
  }
  .dp-tab {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 42px;
    padding: 0 10px;
    font-size: var(--text-sm);
    font-weight: 500;
    color: var(--ink-4);
    cursor: pointer;
    transition: color var(--dur-fast) var(--ease);
  }
  .dp-tab:hover {
    color: var(--ink-1);
  }
  .dp-tab[aria-selected="true"] {
    color: var(--ink-1);
    font-weight: 600;
  }
  .dp-tab[aria-selected="true"]::after {
    content: "";
    position: absolute;
    left: 8px;
    right: 8px;
    bottom: -1px;
    height: 2px;
    border-radius: 2px;
    background: var(--hue, var(--accent));
  }
  .dp-tab-n {
    padding: 0 6px;
    border-radius: 999px;
    font-size: 10.5px;
    background: var(--surface-2);
    color: var(--ink-5);
  }

  .dp-inline {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    margin-bottom: 16px;
    padding: 12px 16px;
    border-radius: var(--radius-lg);
  }

  /* -- right now ----------------------------------------------------------- */
  .dp-now {
    --tone: var(--ink-5);
    --tone-wash: var(--surface-2);
    --tone-ink: var(--ink-3);
    display: flex;
    align-items: flex-start;
    gap: 14px;
    padding: 16px 18px;
    border-radius: var(--radius-lg);
    background: color-mix(in srgb, var(--tone-wash) 62%, var(--surface-1));
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--tone) 24%, transparent);
  }
  .dp-now[data-tone="ok"] {
    --tone: var(--status-ok);
    --tone-wash: var(--status-ok-wash);
    --tone-ink: var(--status-ok-ink);
  }
  .dp-now[data-tone="working"] {
    --tone: var(--status-running);
    --tone-wash: var(--status-running-wash);
    --tone-ink: var(--status-running-ink);
  }
  .dp-now[data-tone="waiting"] {
    --tone: var(--status-warn);
    --tone-wash: var(--status-warn-wash);
    --tone-ink: var(--status-warn-ink);
  }
  .dp-now[data-tone="problem"] {
    --tone: var(--status-error);
    --tone-wash: var(--status-error-wash);
    --tone-ink: var(--status-error-ink);
  }
  .dp-now-mark {
    display: grid;
    flex: none;
    place-items: center;
    width: 38px;
    height: 38px;
    border-radius: 999px;
    background: var(--tone-wash);
    color: var(--tone-ink);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--tone) 30%, transparent);
  }
  .dp-now-head {
    margin: 3px 0 0;
    font-size: 18px;
    font-weight: 700;
    letter-spacing: -0.02em;
    line-height: 1.3;
    color: var(--ink-1);
  }
  .dp-now-detail {
    margin: 5px 0 0;
    max-width: 78ch;
    font-size: var(--text-sm);
    line-height: 1.55;
    color: var(--ink-3);
    text-wrap: pretty;
  }
  .dp-now-why {
    display: grid;
    gap: 4px;
    margin: 9px 0 0;
    padding: 0;
    list-style: none;
  }
  .dp-now-why li {
    position: relative;
    padding-left: 14px;
    font-size: var(--text-sm);
    line-height: 1.5;
    color: var(--ink-2);
  }
  .dp-now-why li::before {
    content: "";
    position: absolute;
    left: 2px;
    top: 0.62em;
    width: 5px;
    height: 5px;
    border-radius: 999px;
    background: var(--tone);
  }
  .dp-now-next {
    margin: 11px 0 0;
    font-size: var(--text-sm);
    line-height: 1.5;
    color: var(--ink-4);
  }
  .dp-now-next > span {
    margin-right: 7px;
    font-weight: 600;
    color: var(--ink-2);
  }
  .dp-now-act {
    flex: none;
    align-self: center;
  }

  /* -- sections -------------------------------------------------------------- */
  .dp-block {
    margin-top: 24px;
  }
  .dp-h {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 10px;
    margin-bottom: 10px;
    font-size: var(--text-sm);
    font-weight: 650;
    color: var(--ink-1);
  }
  .dp-hsub {
    font-size: var(--text-xs);
    font-weight: 400;
    color: var(--ink-5);
  }
  .dp-h-link {
    margin-left: auto;
    font-size: var(--text-xs);
    font-weight: 500;
  }
  .dp-note {
    margin-bottom: 12px;
    padding: 9px 12px;
    border-radius: var(--radius-md);
    font-size: var(--text-xs);
    line-height: 1.5;
    color: var(--ink-4);
    background: var(--surface-2);
  }

  /* -- stage colours, shared by cells, legend dots and copy rows ------------ */
  [data-stage="healthy"] {
    --stage: var(--status-ok);
  }
  [data-stage="checking"] {
    --stage: var(--status-running);
  }
  [data-stage="starting"] {
    --stage: color-mix(in srgb, var(--status-running) 50%, var(--track));
  }
  [data-stage="waiting"] {
    --stage: transparent;
  }
  [data-stage="next"] {
    --stage: color-mix(in srgb, var(--ink-6) 40%, var(--track));
  }
  [data-stage="failing"] {
    --stage: var(--status-error);
  }
  [data-stage="away"] {
    --stage: var(--status-warn);
  }
  [data-stage="stopped"] {
    --stage: var(--ink-6);
  }

  .dp-bar {
    display: flex;
    gap: 3px;
    height: 12px;
  }
  .dp-cell {
    flex: 1 1 0;
    min-width: 3px;
    max-width: 120px;
    border-radius: 3px;
    background: var(--stage, var(--ink-6));
    transition: background-color var(--dur-base) var(--ease);
  }
  .dp-cell[data-stage="waiting"],
  .dp-dot[data-stage="waiting"] {
    box-shadow: inset 0 0 0 1.5px var(--edge-strong);
    background-image: repeating-linear-gradient(-45deg, transparent 0 3px, color-mix(in srgb, var(--ink-6) 45%, transparent) 3px 4px);
  }
  .dp-cell[data-stage="checking"],
  .dp-cell[data-stage="starting"] {
    animation: dp-breathe 1.8s ease-in-out infinite;
  }
  .dp-cell-old {
    opacity: 0.4;
  }
  .dp-legend {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 16px;
    margin-top: 10px;
    font-size: var(--text-xs);
    color: var(--ink-4);
  }
  .dp-legend-item {
    display: inline-flex;
    align-items: center;
  }
  .dp-legend-item b {
    margin-left: 5px;
    font-weight: 600;
    color: var(--ink-2);
  }
  .dp-dot {
    display: inline-block;
    width: 9px;
    height: 9px;
    margin-right: 6px;
    border-radius: 2px;
    background: var(--stage, var(--ink-6));
  }

  /* -- copies ----------------------------------------------------------------- */
  .dp-copies {
    overflow: hidden;
    border-radius: var(--radius-lg);
    box-shadow: 0 0 0 1px var(--edge-card);
  }
  .dp-copy + .dp-copy,
  .dp-subhead + .dp-copy {
    border-top: 1px solid var(--edge-row);
  }
  .dp-copy[data-stage="failing"] {
    box-shadow: inset 3px 0 0 var(--status-error);
  }
  .dp-copy[data-stage="away"] {
    box-shadow: inset 3px 0 0 var(--status-warn);
  }
  .dp-copy[data-stage="waiting"] {
    box-shadow: inset 3px 0 0 var(--edge-strong);
  }
  .dp-copy-old {
    background: var(--surface-subtle);
  }
  .dp-copy-row {
    display: grid;
    grid-template-columns: 150px 236px minmax(0, 1fr) auto;
    align-items: center;
    column-gap: 16px;
    padding: 11px 14px 11px 16px;
  }
  .dp-copy-id {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 2px;
  }
  .dp-copy-name {
    overflow: hidden;
    font-size: var(--text-sm);
    font-weight: 600;
    color: var(--ink-1);
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dp-copy-node {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    overflow: hidden;
    font-size: var(--text-xs);
    color: var(--ink-4);
    white-space: nowrap;
  }
  .dp-copy-text {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 2px;
  }
  .dp-copy-say {
    font-size: var(--text-sm);
    color: var(--ink-2);
  }
  .dp-copy[data-stage="failing"] .dp-copy-say {
    color: var(--status-error-ink);
  }
  .dp-copy[data-stage="away"] .dp-copy-say {
    color: var(--status-warn-ink);
  }
  .dp-copy-why {
    font-size: var(--text-xs);
    line-height: 1.45;
    color: var(--ink-4);
  }
  .dp-copy-when {
    font-size: var(--text-xs);
    color: var(--ink-5);
  }
  .dp-copy-actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 2px;
  }
  .dp-subhead {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 10px;
    padding: 8px 16px;
    font-size: var(--text-xs);
    font-weight: 600;
    color: var(--ink-3);
    background: var(--surface-2);
    border-top: 1px solid var(--edge-row);
  }
  .dp-subhead > span {
    font-weight: 400;
    color: var(--ink-5);
  }

  /* The journey: three dots on a line. Done is green, the step a copy is
     on now breathes, and a failure sits on the step it failed at. */
  .dp-track {
    display: flex;
    align-items: center;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .dp-step {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    font-size: 11px;
    color: var(--ink-5);
    white-space: nowrap;
  }
  .dp-step + .dp-step::before {
    content: "";
    width: 12px;
    /* 2px, not 1.5: a hairline that thin lands on a fractional pixel on
       every other row and anti-aliases a done step's green to grey. */
    height: 2px;
    margin: 0 6px 0 3px;
    border-radius: 1px;
    background: var(--edge-strong);
  }
  .dp-step[data-state="done"] + .dp-step::before {
    background: var(--status-ok);
  }
  .dp-step-dot {
    flex: none;
    width: 9px;
    height: 9px;
    border-radius: 999px;
    box-shadow: inset 0 0 0 1.5px var(--ink-6);
  }
  .dp-step[data-state="done"] {
    color: var(--ink-3);
  }
  .dp-step[data-state="done"] .dp-step-dot {
    background: var(--status-ok);
    box-shadow: none;
  }
  .dp-step[data-state="now"] {
    font-weight: 600;
    color: var(--status-running-ink);
  }
  .dp-step[data-state="now"] .dp-step-dot {
    background: var(--status-running);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--status-running) 22%, transparent);
    animation: dp-breathe 1.6s ease-in-out infinite;
  }
  .dp-step[data-state="bad"] {
    font-weight: 600;
    color: var(--status-error-ink);
  }
  .dp-step[data-state="bad"] .dp-step-dot {
    background: var(--status-error);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--status-error) 20%, transparent);
  }
  .dp-step[data-state="warn"] {
    font-weight: 600;
    color: var(--status-warn-ink);
  }
  .dp-step[data-state="warn"] .dp-step-dot {
    background: var(--status-warn);
  }

  .dp-panel {
    margin: 0 14px 12px 16px;
    overflow: hidden;
    border-radius: var(--radius-md);
    box-shadow: 0 0 0 1px var(--edge-card);
    background: var(--surface-1);
  }
  .dp-panel-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    padding: 7px 12px;
    font-size: var(--text-xs);
    background: var(--surface-2);
    border-bottom: 1px solid var(--edge);
  }

  /* -- activity ----------------------------------------------------------------- */
  .dp-timeline {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .dp-event {
    position: relative;
    display: grid;
    grid-template-columns: 16px minmax(0, 1fr) auto;
    align-items: start;
    column-gap: 10px;
    padding: 6px 0;
  }
  /* The line through the dots, broken at the ends. */
  .dp-event::before {
    content: "";
    position: absolute;
    left: 4px;
    top: 0;
    bottom: 0;
    width: 1px;
    background: var(--edge);
  }
  .dp-event:first-child::before {
    top: 14px;
  }
  .dp-event:last-child::before {
    bottom: calc(100% - 14px);
  }
  .dp-event:only-child::before {
    display: none;
  }
  .dp-event-dot {
    position: relative;
    z-index: 1;
    width: 9px;
    height: 9px;
    margin-top: 5px;
    border-radius: 999px;
    background: var(--ink-6);
    box-shadow: 0 0 0 3px var(--surface-1);
  }
  .dp-event[data-tone="ok"] .dp-event-dot {
    background: var(--status-ok);
  }
  .dp-event[data-tone="info"] .dp-event-dot {
    background: var(--status-running);
  }
  .dp-event[data-tone="warn"] .dp-event-dot {
    background: var(--status-warn);
  }
  .dp-event[data-tone="error"] .dp-event-dot {
    background: var(--status-error);
  }
  .dp-event-text {
    font-size: var(--text-sm);
    line-height: 1.5;
    color: var(--ink-2);
  }
  .dp-event[data-tone="error"] .dp-event-text {
    color: var(--status-error-ink);
  }
  .dp-event-text b {
    font-weight: 600;
    color: var(--ink-1);
  }
  .dp-event-when {
    padding-top: 2px;
    font-size: var(--text-xs);
    color: var(--ink-5);
    white-space: nowrap;
  }

  .dp-facts {
    overflow: hidden;
    border-radius: var(--radius-lg);
    box-shadow: 0 0 0 1px var(--edge-card);
  }

  /* -- loading ---------------------------------------------------------------- */
  .dp-skeleton {
    display: grid;
    gap: 12px;
  }
  .dp-skel {
    border-radius: var(--radius-lg);
    background: var(--surface-2);
    animation: dp-breathe 1.4s ease-in-out infinite;
  }
  .dp-skel-hero {
    height: 118px;
  }
  .dp-skel-bar {
    height: 12px;
    margin-top: 12px;
    border-radius: 3px;
  }
  .dp-skel-row {
    height: 56px;
  }

  @keyframes dp-breathe {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.55;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .dp-cell[data-stage="checking"],
    .dp-cell[data-stage="starting"],
    .dp-step[data-state="now"] .dp-step-dot,
    .dp-skel {
      animation: none;
    }
  }
</style>
