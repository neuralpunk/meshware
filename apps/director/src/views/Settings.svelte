<script lang="ts">
  import { hueStyle, type Hue } from "../lib/hues";
  import Spinner from "../lib/components/Spinner.svelte";
  import Icon from "../lib/components/Icon.svelte";
  // Seven panes of white cards used to be seven of the same thing. Each pane
  // now has a hue, carried as a hairline on the left of its cards.
  const PANE_HUE: Record<string, Hue> = {
    "this-peer": "teal",
    director: "indigo",
    fleet: "sky",
    facts: "plum",
    diagnostics: "amber",
    history: "coral",
    storage: "sky",
    backup: "teal",
    about: "indigo",
  };
  // Settings on the design contract: light cards, the spec'd 38×22 toggle
  // (disabled state requires an inline explanation), this peer's tags,
  // invite minting, WAN reachability, and the day-2 verbs — doctor,
  // resolved config, backup, drain (MASTERPLAN §12, §14.2).
  import {
    api,
    errText,
    fmtBytes,
    parseDuration,
    type DoctorCheckView,
    type DrainEvent,
    type FactCollectorView,
    type DirectorUpdateView,
    type DirectorUpdateEvent,
    type DoctorFixView,
    type ShedView,
  } from "../lib/api";
  import { untrack } from "svelte";
  import { app, ui, backupFileName } from "../lib/state.svelte";
  import Hint from "../lib/components/Hint.svelte";
  import KeyProtection from "../lib/components/KeyProtection.svelte";
  import SecurityKey from "../lib/components/SecurityKey.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Tag from "../lib/components/Tag.svelte";
  import { staggerIn } from "../lib/motion";
  import { poll } from "../lib/poll";
  import { SUBNAV } from "../lib/chrome/nav";
  import AboutPane from "../lib/components/AboutPane.svelte";

  const isAdmin = $derived(app.whoami?.isFounder || app.whoami?.roles.includes("admin"));
  const canDrain = $derived(isAdmin || app.whoami?.roles.includes("operator"));

  // -- Director itself -----------------------------------------------------------
  //
  // The app, as against the peer it runs: launch at login, its own
  // updates, where its log is. Mesh → Updates covers every other machine;
  // this card covers the one the person is sitting at.
  let autostart = $state<boolean | null>(null);
  let autostartErr = $state<string | null>(null);
  let update = $state<DirectorUpdateView | null>(null);
  let updateBusy = $state(false);
  let updateErr = $state<string | null>(null);
  let updateProgress = $state<{ downloaded: number; total: number | null } | null>(null);
  let updateInstalled = $state<string | null>(null);
  let logDir = $state("");
  /**
   * Whether the tray icon is actually drawn on this desktop.
   *
   * Registering a tray item and having one displayed are different things
   * on Linux — stock GNOME does the first and not the second — and both
   * "Start at login" and close-to-hide depend on the second. Re-read on the
   * same slow tick the engine polls on, so enabling the GNOME extension
   * updates this card without restarting Director.
   */
  let tray = $state<import("../lib/api").TrayStatusView | null>(null);

  $effect(() => {
    void api.autostartGet().then((v) => (autostart = v)).catch((e) => (autostartErr = errText(e)));
    void api.directorLogDir().then((d) => (logDir = d)).catch(() => {});
    void checkDirectorUpdate();
    return poll(async () => (tray = await api.trayStatus()), 5000);
  });

  async function setAutostart(on: boolean) {
    autostartErr = null;
    try {
      await api.autostartSet(on);
      autostart = on;
      app.flash(on ? "Director will start at login, in the tray" : "Director no longer starts at login");
    } catch (e) {
      autostartErr = errText(e);
    }
  }

  async function checkDirectorUpdate() {
    updateBusy = true;
    updateErr = null;
    try {
      update = await api.directorUpdateCheck();
    } catch (e) {
      updateErr = errText(e);
    } finally {
      updateBusy = false;
    }
  }

  async function installDirectorUpdate() {
    updateBusy = true;
    updateErr = null;
    updateProgress = { downloaded: 0, total: null };
    try {
      await api.directorUpdateInstall((ev: DirectorUpdateEvent) => {
        if (ev.type === "progress") updateProgress = { downloaded: ev.downloaded, total: ev.total };
        else if (ev.type === "installed") updateInstalled = ev.version;
        else {
          updateErr = ev.message;
          updateBusy = false;
          updateProgress = null;
        }
      });
    } catch (e) {
      updateErr = errText(e);
      updateBusy = false;
      updateProgress = null;
    }
  }

  async function copyLogDir() {
    try {
      await api.copyText(logDir);
      app.flash("Log folder path copied");
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }

  // -- fact collectors (M18 §4.5, §4.6) ------------------------------------------
  //
  // Here rather than on Nodes, because this is a statement about the
  // *fleet's* configuration — what every node is asked to report — and not
  // about any one machine. What must not be buried is the flapping
  // warning: a collector publishing faster than once an hour would write an
  // unbounded, uncompactable op-log, so it is a failure with the offending
  // keys named, never a badge.
  let collectors = $state<FactCollectorView[] | null>(null);
  let collectorsErr = $state<string | null>(null);
  let collectorBusy = $state("");

  async function loadCollectors() {
    collectorsErr = null;
    try {
      collectors = await api.factCollectors();
    } catch (e) {
      collectorsErr = errText(e);
    }
  }

  $effect(() => {
    if (!app.engineStopped && collectors === null) void loadCollectors();
  });

  const flapping = $derived((collectors ?? []).filter((c) => c.flappingKeys.length > 0));

  // -- the canary ring (M24 §3.3) ------------------------------------------------
  //
  // A collector runs a script on every node it matches, unprompted — and
  // unlike a bad workflow, nobody pressed Run. So widening its selector is
  // a *deployment*, and the surface treats it as one: the ring is written
  // `tag=gpu, tag=canary`, it is watched, and widening it is deleting the
  // canary clause. The mechanism is the ordinary `on:` selector; what was
  // missing was a screen that named the step.
  let widening = $state<FactCollectorView | null>(null);
  let widenTo = $state("");
  let widenBusy = $state(false);
  let widenErr = $state<string | null>(null);
  let widenReach = $state<number | null>(null);

  /** The obvious next ring: drop the `tag=canary` clause. */
  function widened(on: string): string | null {
    if (!on.includes(",")) return null;
    const parts = on.split(",").map((p) => p.trim()).filter(Boolean);
    const kept = parts.filter((p) => p.toLowerCase() !== "tag=canary");
    return kept.length && kept.length < parts.length ? kept.join(", ") : null;
  }

  function openWiden(c: FactCollectorView) {
    widening = c;
    widenTo = widened(c.on) ?? `${c.on}, tag=canary`;
    widenErr = null;
    widenReach = null;
    void previewRing();
  }

  async function previewRing() {
    try {
      widenReach = (await api.targetPreview(widenTo.trim() || "any")).length;
    } catch {
      widenReach = null;
    }
  }

  async function doWiden() {
    if (!widening) return;
    widenBusy = true;
    widenErr = null;
    try {
      await api.collectorWiden(widening.name, widenTo.trim());
      app.flash(`'${widening.name}' now runs on ${widenTo.trim()}`);
      widening = null;
      await loadCollectors();
    } catch (e) {
      widenErr = errText(e);
    } finally {
      widenBusy = false;
    }
  }

  async function setPaused(c: FactCollectorView, paused: boolean) {
    collectorBusy = c.name;
    try {
      await api.collectorSetPaused(c.name, paused);
      app.flash(`collector '${c.name}' ${paused ? "paused" : "resumed"}`);
      await loadCollectors();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      collectorBusy = "";
    }
  }

  /** `900` → `15m`, the way an interval is written in a definition. */
  function every(secs: number): string {
    if (secs % 3600 === 0) return `${secs / 3600}h`;
    if (secs % 60 === 0) return `${secs / 60}m`;
    return `${secs}s`;
  }

  // -- contribute ---------------------------------------------------------------
  let toggleBusy = $state(false);
  async function toggleContribute() {
    if (!app.status) return;
    toggleBusy = true;
    try {
      app.status = await api.setContribute(!app.status.executing);
      app.flash(app.status.executing ? "this machine now runs mesh jobs" : "this machine is now no-execute");
      await app.refresh();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      toggleBusy = false;
    }
  }

  // -- tags ------------------------------------------------------------------------
  // `app.selfNode`, not a search of the fleet: this peer is absent from
  // the fleet list when it is a console (a Director beside a `meshnode`).
  const self = $derived(app.selfNode);
  const ownTags = $derived((self?.tags ?? []).filter((t) => t !== "no-execute"));
  let newTag = $state("");
  let tagBusy = $state(false);
  async function addTag() {
    const t = newTag.trim();
    if (!t) return;
    tagBusy = true;
    try {
      app.status = await api.tagsUpdate([t], []);
      newTag = "";
      app.flash(`tag ${t} added — every machine will see it within a few seconds`);
      await app.refresh();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      tagBusy = false;
    }
  }
  async function removeTag(t: string) {
    tagBusy = true;
    try {
      app.status = await api.tagsUpdate([], [t]);
      app.flash(`tag ${t} removed`);
      await app.refresh();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      tagBusy = false;
    }
  }

  // -- invites -------------------------------------------------------------------
  let inviteTtl = $state("24h");
  let inviteUses = $state(1);
  let inviteTags = $state("");
  let inviteAddrs = $state("");
  let inviteToken = $state<string | null>(null);
  let inviteBusy = $state(false);
  let inviteErr = $state<string | null>(null);

  async function mintInvite() {
    inviteBusy = true;
    inviteErr = null;
    inviteToken = null;
    try {
      const ttl = parseDuration(inviteTtl) ?? 86400;
      inviteToken = await api.inviteCreate(
        ttl,
        inviteUses,
        inviteTags.split(",").map((t) => t.trim()).filter(Boolean),
        inviteAddrs.split(",").map((t) => t.trim()).filter(Boolean),
      );
    } catch (e) {
      inviteErr = errText(e);
    } finally {
      inviteBusy = false;
    }
  }

  async function copyToken() {
    if (inviteToken) {
      await api.copyText(`meshnode join ${inviteToken}`);
      app.flash("join command copied");
    }
  }

  // -- doctor ---------------------------------------------------------------------
  let checks = $state<DoctorCheckView[] | null>(null);
  let doctorBusy = $state(false);
  let doctorErr = $state<string | null>(null);
  async function runDoctor() {
    doctorBusy = true;
    doctorErr = null;
    try {
      checks = await api.doctor();
    } catch (e) {
      doctorErr = errText(e);
    } finally {
      doctorBusy = false;
    }
  }
  const doctorSummary = $derived.by(() => {
    if (!checks) return null;
    const fail = checks.filter((c) => c.verdict === "fail").length;
    const warn = checks.filter((c) => c.verdict === "warn").length;
    return { fail, warn, ok: checks.length - fail - warn };
  });
  // `doctor --fix`: the safe fixes the checks already know, then the
  // checks again. What could not be fixed is listed with the reason —
  // that half is as much the feature as the other.
  let fixes = $state<DoctorFixView[] | null>(null);
  let fixBusy = $state(false);
  async function runFix() {
    fixBusy = true;
    doctorErr = null;
    try {
      const report = await api.doctorFix();
      fixes = report.fixes;
      checks = report.after;
      const applied = report.fixes.filter((f) => f.applied).length;
      app.flash(applied === 0 ? "Nothing here has an automatic fix" : `${applied} fixed — checks run again`);
    } catch (e) {
      doctorErr = errText(e);
    } finally {
      fixBusy = false;
    }
  }

  // -- config ---------------------------------------------------------------------
  let configJson = $state<string | null>(null);
  let configOpen = $state(false);
  async function showConfig() {
    configOpen = !configOpen;
    if (configOpen && !configJson) {
      try {
        configJson = await api.configView();
      } catch (e) {
        configJson = errText(e);
      }
    }
  }

  // -- backup ---------------------------------------------------------------------
  let backupBusy = $state(false);
  let backupResult = $state<string | null>(null);
  let backupErr = $state<string | null>(null);
  async function doBackup() {
    backupBusy = true;
    backupErr = null;
    backupResult = null;
    try {
      const r = await api.backupWrite(backupFileName(app.status?.nodeName));
      if (!r) return;
      backupResult = `Backup written to ${r.path} (${fmtBytes(r.bytes)}). It contains this machine's private keys: treat it like a key file.`;
    } catch (e) {
      backupErr = errText(e);
    } finally {
      backupBusy = false;
    }
  }

  // -- M31: how long we keep it, and who else gets a copy ------------------------
  //
  // Two cards, one paragraph. Retention is **every** mesh's business, so it
  // renders its numbers unprompted. History export is almost **nobody's**,
  // so it is one row saying "not configured" until somebody chooses
  // otherwise — the Optional rule (§18 decision 01) expressed as layout
  // rather than as a warning.

  let retention = $state<import("../lib/api").RetentionView | null>(null);
  let retentionErr = $state<string | null>(null);
  let retentionBusy = $state(false);
  let retentionEdit = $state(false);
  /** `table → window`, as typed. Only the tables the operator actually
   *  changed are sent: silence about a table is not a reset, and the table
   *  it would most likely reset is `audit`. */
  let windowDraft = $state<Record<string, string>>({});

  let sinks = $state<import("../lib/api").SinkView[] | null>(null);
  let sinkKinds = $state<import("../lib/api").SinkKindView[]>([]);
  let sinksErr = $state<string | null>(null);
  let sinkBusy = $state<string | null>(null);
  let sinkForm = $state<SinkFormState | null>(null);
  let sinkTestResult = $state<{ ok: boolean; text: string } | null>(null);
  /** The confirmation §19 Q6 asked for: moving an export between machines
   *  is safe, and is also the control most likely to be changed by someone
   *  who does not know why it was set. So the dialog names both sides. */
  let moveConfirm = $state<{ from: string; to: string } | null>(null);

  type SinkFormState = {
    editing: string | null;
    name: string;
    kind: string;
    url: string;
    database: string;
    credential: string;
    /** M31 B3 §14.3 item 3: a *second* reference, for reading output back.
     *  Separate because the token that may INSERT should not have to
     *  SELECT — and absent, replay is refused and says which permission is
     *  missing rather than quietly widening the write grant. */
    readCredential: string;
    tables: string[];
    runOn: string;
    payloads: boolean;
    payloadMaxMb: number;
    holdForExportDays: number;
    description: string;
    /** `runOn` as it was when the form opened, so a change can be named. */
    runOnWas: string;
  };

  const selectedKind = $derived(sinkKinds.find((k) => k.kind === sinkForm?.kind));

  /** What reading back will actually do **as configured** (§14.4 rule 3).
   *
   *  Three answers, not two, and the third is the point: a destination that
   *  can replay but has no read credential is not "unsupported" — it is one
   *  permission away, and saying "unsupported" sends the operator to the
   *  wrong place entirely. */
  const replayNote = $derived(
    !selectedKind
      ? ""
      : !selectedKind.replayPayloads
        ? "Director cannot open past job output from here. Once the local copy ages out, you will read it in the destination's own tools."
        : sinkForm?.readCredential
          ? "Director can open past job output from here — fetched by the machine that exported it."
          : "Replay available — needs a read credential. Without one it is refused, rather than quietly reusing the write token.",
  );

  /**
   * Load this pane's data once, then on a slow timer.
   *
   * **Nothing reactive may be read in here**, and that is not a style
   * preference — it is the bug this shape exists to make impossible. The
   * vault fetch below writes `app.secrets`; reading it *tracked* made this
   * effect depend on a value it also sets, so a mesh with no secrets fed
   * `[]` back into its own dependency on every resolve and re-ran the
   * whole block as fast as the IPC round trip allowed. Four commands per
   * iteration, thousands a second, which is what "Doctor hangs" and
   * "Signing… forever" actually were: not one broken command, but every
   * command starved behind a storm this screen was generating.
   *
   * So: `untrack` around the guard, and the refresh is an explicit timer
   * like every other live panel in the app.
   */
  $effect(() => {
    void api.sinkKinds().then((k) => (sinkKinds = k)).catch(() => {});
    // The credential field is a picker over the mesh vault (C6 — there is
    // no free-text password box), so Settings has to have the vault. It is
    // not otherwise a Settings concern, which is why it is fetched here
    // rather than assumed.
    if (untrack(() => app.secrets.length) === 0) {
      void api.secrets().then((rows) => (app.secrets = rows)).catch(() => {});
    }
    // Awaited rather than fired and forgotten, so `poll`'s one-at-a-time
    // guard actually applies to it (M-24) — still in parallel, so the
    // three reads cost one round-trip's latency and not three.
    const tick = () =>
      Promise.all([
        // Not while somebody is editing: replacing the rows under a form
        // is how a half-typed window silently becomes the old one again.
        untrack(() => retentionEdit) ? null : loadRetention(),
        loadContent(),
        loadDisks(),
        loadPressure(),
        untrack(() => sinkForm) ? null : loadSinks(),
      ]);
    return poll(tick, 5000);
  });

  // -- M31 A2: the content store, and the disk under it ---------------------
  //
  // Two questions on one pane because they are one question asked twice:
  // "what is this machine keeping" and "is it about to run out of room".
  // Splitting them would put the number on one screen and the consequence
  // on another.
  let content = $state<import("../lib/api").ContentStatusView | null>(null);
  let disks = $state<import("../lib/api").DiskView[]>([]);
  let contentErr = $state<string | null>(null);
  let contentBusy = $state(false);
  /** What a reclaim *would* take, refreshed whenever the level changes. */
  let preview = $state<import("../lib/api").ContentPruneView | null>(null);
  let capMb = $state("");
  /** The level the radio list shows, which may lead the saved one while a
      round trip is in flight — so the click feels instant and a failure
      snaps it back rather than leaving the UI lying. */
  let pendingPolicy = $state<string | null>(null);

  async function loadContent() {
    try {
      content = await api.contentStatus();
      contentErr = null;
      if (pendingPolicy === null) pendingPolicy = content.policy;
      if (capMb === "") capMb = content.maxBytes ? String(Math.round(content.maxBytes / 1048576)) : "";
    } catch (e) {
      contentErr = errText(e) || "the engine did not answer";
    }
  }

  async function loadDisks() {
    try {
      disks = await api.disk();
    } catch {
      // An engine too old to answer is not a red box on this screen.
    }
  }

  async function pickPolicy(id: string) {
    const previous = pendingPolicy;
    pendingPolicy = id;
    contentBusy = true;
    try {
      content = await api.setContentPolicy(id, Number(capMb) || 0);
      contentErr = null;
      // The new level usually changes what is reclaimable, and a stale
      // "would free 0 B" beside a freshly chosen `ephemeral` reads as the
      // setting having done nothing.
      preview = await api.contentPrune(true, false);
    } catch (e) {
      pendingPolicy = previous;
      contentErr = errText(e) || "could not save that";
    } finally {
      contentBusy = false;
    }
  }

  async function previewPrune() {
    contentBusy = true;
    try {
      preview = await api.contentPrune(true, false);
      contentErr = null;
    } catch (e) {
      contentErr = errText(e) || "could not measure";
    } finally {
      contentBusy = false;
    }
  }

  async function runPrune(aggressive: boolean) {
    contentBusy = true;
    try {
      preview = await api.contentPrune(false, aggressive);
      contentErr = null;
      await loadContent();
    } catch (e) {
      contentErr = errText(e) || "could not reclaim";
    } finally {
      contentBusy = false;
    }
  }

  async function loadRetention() {
    try {
      retention = await api.retention();
      // Cleared on success, never on the attempt: a box that blanks itself
      // the moment the next poll starts is a box nobody can read.
      retentionErr = null;
    } catch (e) {
      retentionErr = errText(e) || "the mesh did not answer";
    }
  }

  // M31 A3. The other half of retention's question: the window says where
  // the log settles, this says whether anything is currently trying to
  // outrun it. Silent on a healthy mesh — a panel reporting a control
  // nobody has tripped is furniture, and furniture is what people learn to
  // scroll past.
  let pressure = $state<import("../lib/api").BackpressureView | null>(null);
  async function loadPressure() {
    try {
      pressure = await api.backpressure();
    } catch {
      // A daemon too old to answer is not a health problem worth a red box
      // on this screen; the version skew has its own line in `doctor`.
      pressure = null;
    }
  }
  const pressureStamp = (micros: number) =>
    new Date(micros / 1000).toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit",
    });

  async function loadSinks() {
    sinksErr = null;
    try {
      sinks = await api.sinks();
    } catch (e) {
      sinksErr = errText(e);
    }
  }

  /** Edit the windows the mesh currently has.
   *
   *  Refuses when there is nothing loaded, because the alternative is what
   *  it used to do: hide the Edit button, render no rows (they come from
   *  `retention`), render no Save and no Cancel — a click that removed the
   *  only control on the card and put nothing in its place. */
  function startRetentionEdit() {
    if (!retention) {
      retentionErr = retentionErr ?? "still reading the current policy — try again in a moment";
      return;
    }
    windowDraft = {};
    for (const w of retention.windows) windowDraft[w.table] = w.window;
    retentionEdit = true;
    retentionErr = null;
  }

  async function saveRetention() {
    retentionBusy = true;
    retentionErr = null;
    try {
      // Only what actually changed. The engine edits on top of the live
      // policy, so an unsent table keeps whatever it had.
      const changed = (retention?.windows ?? [])
        .filter((w) => windowDraft[w.table] && windowDraft[w.table] !== w.window)
        .map((w) => `${w.table}=${windowDraft[w.table]}`);
      if (!changed.length) { retentionEdit = false; return; }
      retention = await api.retentionPut(changed);
      retentionEdit = false;
      app.flash("Retention policy signed");
    } catch (e) {
      retentionErr = errText(e);
    } finally {
      retentionBusy = false;
    }
  }

  async function resetRetention() {
    retentionBusy = true;
    retentionErr = null;
    try {
      retention = await api.retentionReset();
      retentionEdit = false;
      app.flash("Back to the shipped defaults");
    } catch (e) {
      retentionErr = errText(e);
    } finally {
      retentionBusy = false;
    }
  }

  function newSink() {
    sinkTestResult = null;
    sinksErr = null;
    sinkForm = {
      editing: null,
      name: "",
      kind: "clickhouse",
      url: "",
      database: "",
      credential: "",
      readCredential: "",
      tables: ["jobs"],
      runOn: "",
      payloads: false,
      payloadMaxMb: 64,
      holdForExportDays: 0,
      description: "",
      runOnWas: "",
    };
  }

  function editSink(s: import("../lib/api").SinkView) {
    sinkTestResult = null;
    sinksErr = null;
    sinkForm = {
      editing: s.name,
      name: s.name,
      kind: s.kind,
      url: s.target.startsWith("http") ? s.target : "",
      database: s.database,
      // The credential is a reference the engine holds; Director never had
      // the value and does not pretend to. Left blank means "unchanged" is
      // not expressible, so re-picking is required — which is the honest
      // cost of never holding a secret.
      credential: "",
      readCredential: "",
      tables: [...s.tables],
      runOn: "",
      payloads: s.payloadsSent > 0 || s.holdForExportDays > 0 ? true : false,
      payloadMaxMb: 64,
      holdForExportDays: s.holdForExportDays,
      description: s.description,
      runOnWas: "",
    };
  }

  async function copyPath(path: string) {
    try {
      await api.copyText(path);
      app.flash("Path copied");
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }

  /** Choosing a destination applies its own payload default (§19 Q2). */
  function pickKind(kind: string) {
    if (!sinkForm) return;
    sinkForm.kind = kind;
    sinkForm.payloads = sinkKinds.find((k) => k.kind === kind)?.payloadsByDefault ?? false;
    sinkTestResult = null;
  }

  function toggleTable(table: string) {
    if (!sinkForm) return;
    sinkForm.tables = sinkForm.tables.includes(table)
      ? sinkForm.tables.filter((t) => t !== table)
      : [...sinkForm.tables, table];
  }

  function draftOf(f: SinkFormState): import("../lib/api").SinkDraft {
    return {
      name: f.name,
      kind: f.kind,
      url: f.url,
      database: f.database,
      credential: f.credential,
      readCredential: f.readCredential,
      tables: f.tables,
      runOn: f.runOn,
      payloads: f.payloads,
      payloadMaxMb: Math.max(0, f.payloadMaxMb),
      holdForExportDays: f.holdForExportDays,
      enabled: true,
      description: f.description,
    };
  }

  /** Test the form as typed, **before** it is saved — which is the only
   *  version of "test before save" that helps. */
  async function testSink() {
    if (!sinkForm) return;
    sinkBusy = "test";
    sinkTestResult = null;
    try {
      sinkTestResult = { ok: true, text: await api.sinkTest(draftOf(sinkForm)) };
    } catch (e) {
      sinkTestResult = { ok: false, text: errText(e) };
    } finally {
      sinkBusy = null;
    }
  }

  function requestSave() {
    if (!sinkForm) return;
    // §19 Q6, decided: editable, behind a confirmation that names the
    // machines it moves between.
    if (sinkForm.editing && sinkForm.runOn !== sinkForm.runOnWas) {
      moveConfirm = {
        from: sinkForm.runOnWas || "every live executor",
        to: sinkForm.runOn || "every live executor",
      };
      return;
    }
    void saveSink();
  }

  async function saveSink() {
    if (!sinkForm) return;
    moveConfirm = null;
    sinkBusy = "save";
    sinksErr = null;
    try {
      await api.sinkPut(draftOf(sinkForm));
      sinkForm = null;
      await loadSinks();
      app.flash("History export saved");
    } catch (e) {
      sinksErr = errText(e);
    } finally {
      sinkBusy = null;
    }
  }

  async function removeSink(name: string) {
    sinkBusy = name;
    sinksErr = null;
    try {
      await api.sinkRemove(name);
      await loadSinks();
    } catch (e) {
      sinksErr = errText(e);
    } finally {
      sinkBusy = null;
    }
  }

  async function resumeSinks() {
    sinkBusy = "resume";
    try {
      await api.sinksResume();
      await loadSinks();
      app.flash("Exporting resumed");
    } catch (e) {
      sinksErr = errText(e);
    } finally {
      sinkBusy = null;
    }
  }

  function lagText(secs: number): string {
    if (!secs) return "—";
    if (secs < 60) return `${secs}s`;
    if (secs < 3600) return `${Math.floor(secs / 60)}m`;
    if (secs < 86_400) return `${Math.floor(secs / 3600)}h`;
    return `${Math.floor(secs / 86_400)}d`;
  }

  // -- drain ------------------------------------------------------------------------
  let drainOpen = $state(false);
  let drainMove = $state(true);
  // N6 §12 — the mesh-wide shed.
  let shed = $state<ShedView | null>(null);
  let shedErr = $state<string | null>(null);
  let shedBusy = $state(false);
  let shedReason = $state("");

  /// True once the reason box has been edited and not yet written.
  ///
  /// The first version keyed this off `document.activeElement`, which was
  /// wrong twice: focusing *any other* input on this page froze the sync,
  /// and blurring the reason box handed the next poll a free hand to
  /// overwrite what had just been typed. A draft flag is the honest state.
  let shedDirty = $state(false);

  async function loadShed() {
    try {
      shed = await api.shed();
      if (!shedDirty) shedReason = shed.reason;
      shedErr = null;
    } catch (e) {
      shedErr = errText(e);
    }
  }
  $effect(() => poll(loadShed, 5000));

  async function setShed(submissions: boolean, reconcile: boolean) {
    shedBusy = true;
    shedErr = null;
    try {
      shed = await api.shedSet(submissions, reconcile, shedReason);
      shedDirty = false;
    } catch (e) {
      shedErr = errText(e);
      // Re-read, so a refused write does not leave the checkboxes showing
      // a state the mesh is not in.
      await loadShed();
    } finally {
      shedBusy = false;
    }
  }

  let drainPhase = $state<"idle" | "draining" | "done" | "failed">("idle");
  let drainProgress = $state<{ running: number; queued: number; moved: number } | null>(null);
  let drainErr = $state<string | null>(null);
  async function doDrain() {
    drainPhase = "draining";
    drainProgress = null;
    drainErr = null;
    try {
      await api.drain(drainMove, (ev: DrainEvent) => {
        if (ev.type === "progress") drainProgress = { running: ev.running, queued: ev.queued, moved: ev.moved };
        else if (ev.type === "done") {
          drainPhase = "done";
          app.markEngineStopped();
        } else if (ev.type === "failed") {
          drainPhase = "failed";
          drainErr = ev.message;
        }
      });
    } catch (e) {
      drainPhase = "failed";
      drainErr = errText(e);
    }
  }
  let restartBusy = $state(false);
  async function restart() {
    restartBusy = true;
    try {
      await app.restartEngine();
      drainOpen = false;
      drainPhase = "idle";
      app.flash("node started — back in the mesh");
    } catch (e) {
      app.flash(errText(e));
    } finally {
      restartBusy = false;
    }
  }

  // -- deep links ---------------------------------------------------------------------
  $effect(() => {
    const f = sessionStorage.getItem("settings:focus");
    if (f) {
      sessionStorage.removeItem("settings:focus");
      if (f === "doctor") void runDoctor();
      setTimeout(() => document.getElementById(`settings-${f}`)?.scrollIntoView({ behavior: "smooth", block: "start" }), 50);
    }
  });

  const wan = $derived(app.status?.wan ?? null);

  // -- panes -----------------------------------------------------------
  //
  // Settings used to be three groups on one document, with a scroll-spy
  // index telling you which one you were in. That index answered "where am
  // I" and never answered the question people actually arrive with, which
  // is **"where is the thing I came for"** — and on a page eleven cards
  // long, the honest answer was "scroll". So the groups became panes.
  //
  // **The panes are rows in the rail since 2026-09-14** (`chrome/nav.ts`,
  // `SUBNAV.settings`), and which one is open is `app.sub` — navigation,
  // living where the rest of it lives. A deep link is `app.go("settings",
  // "history")` rather than a sessionStorage note this screen had to read
  // and clear, and back and forward walk panes the way they walk Mesh's.
  //
  // Sizes are deliberately uneven. `Diagnostics` is one card and `History`
  // is four; padding the small ones out to match would be furniture, and
  // splitting the big one further would put "how long we keep it" and "who
  // else gets a copy" on different screens when the whole point is that
  // they are one decision.
  const PANES = SUBNAV.settings ?? [];
  const pane = $derived(app.sub ?? PANES[0]?.id ?? "this-peer");
  const current = $derived(PANES.find((p) => p.id === pane) ?? PANES[0]);

  // A pane switch is a new page: start it at the top rather than halfway
  // down because the last one was scrolled. The scroll region is the
  // page's, not this component's, so walk up to it.
  let scrollEl = $state<HTMLElement | null>(null);
  $effect(() => {
    void pane;
    untrack(() => {
      let el: HTMLElement | null = scrollEl;
      while (el && !/auto|scroll/.test(getComputedStyle(el).overflowY)) {
        el = el.parentElement;
      }
      el?.scrollTo({ top: 0 });
    });
  });
</script>

<!-- One pane, the whole width of the column. The panes are rows in the
     sidebar now (2026-09-14): the 168px index that used to sit to the left
     of this was a second navigation, in a second place, in a second shape,
     saying what the rail can say. Left-aligned under the title and capped
     at the width the pane had beside that index, so a card's line length
     did not grow by a fifth when its neighbour left. -->
<div class="settings-pane fade-in min-w-0 max-w-[824px] space-y-4" style={hueStyle(PANE_HUE[pane] ?? "teal")} bind:this={scrollEl}>
  <!-- The engine-stopped banner sits outside the panes: it is true on every
       one of them, and a warning you can navigate away from is a warning
       that gets navigated away from. -->
  {#if app.engineStopped}
    <div class="card p-[22px]" style="background: var(--status-warn-wash);">
      <h2 class="m-0 text-md font-bold tracking-[-.015em]" style="color: var(--status-warn-ink);">
        This machine's node is stopped
        <Hint text="Starting it again re-announces the same identity, name and tags." label="About restarting" />
      </h2>
      <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-3">It <span title="stopped accepting work, finished what it had, and shut down">drained</span> and left the mesh's rotation, and stays stopped until it is started — here, or with <span class="mono">systemctl --user start meshnode</span>.</p>
      <button class={`${ui.primary} mt-4`} disabled={restartBusy} onclick={restart}>{restartBusy ? "Starting…" : "Start it"}</button>
    </div>
  {/if}

  <!-- One line saying what this pane is for. The rail gives the noun; this
       gives the question, in the same voice the main nav's `sub` uses. -->
  <div class="pt-1 pb-1">
    <h2 class="m-0 text-md font-semibold tracking-[-.01em] text-ink">{current?.label}</h2>
    <p class="mt-0.5 mb-0 text-sm text-ink-5">{current?.sub}</p>
  </div>

  {#if pane === "this-peer"}

  <!-- identity -->
  <!-- No "Identity" heading (UX-19): the pane's own heading just above says
       "This peer — who this machine is in the mesh", so the card's first
       line was the third title in a row for one subject. -->
  <div class={ui.card}>
    <dl class="m-0 space-y-3.5 text-sm">
      <div>
        <dt class="label">Mesh</dt>
        <dd class="mt-1 ml-0 text-ink-2">
          {app.status?.meshName}
          <span class="mono selectable ml-2 text-2xs text-ink-5">{app.status?.meshId}</span>
        </dd>
      </div>
      <div>
        <dt class="label">Node</dt>
        <dd class="mt-1 ml-0 text-ink-2">
          {app.status?.nodeName}
          <span class="num ml-2 text-xs text-ink-5">{app.status?.slots} slots · {app.status?.jobsRunning} running · {app.status?.jobsQueued} queued</span>
          <div class="mono selectable mt-0.5 break-all text-2xs text-ink-5">{app.status?.nodeId}</div>
        </dd>
      </div>
      <div>
        <dt class="label">You</dt>
        <dd class="mt-1 ml-0 text-ink-2">
          {app.whoami?.username ?? "(unregistered)"}
          {#if app.whoami?.isFounder}
            <span class="ml-1.5 rounded-full px-2 py-0.5 text-2xs font-semibold" style="background: var(--accent-wash); color: var(--accent-ink);">founder</span>
          {/if}
          {#each app.whoami?.roles ?? [] as r}
            <span class="mono ml-1 rounded-full bg-surface-2 px-2 py-0.5 text-2xs font-semibold text-ink-4">{r}</span>
          {/each}
          <div class="mono selectable mt-0.5 break-all text-2xs text-ink-5">{app.whoami?.userId}</div>
          {#if app.whoami && !app.whoami.registered && !app.whoami.isFounder}
            <div class="mt-1.5 text-xs font-medium" style="color: var(--status-warn-ink);">
              Not yet a mesh user. There is no password to set — ask an admin to recognise this
              key with <strong>Request access</strong> at the top of the window, or
              <span class="mono">meshnode enrol</span>.
            </div>
          {/if}
        </dd>
      </div>
      <div>
        <dt class="label">Listening</dt>
        <dd class="mono mt-1 ml-0 text-ink-3">{app.status?.listenAddr}</dd>
      </div>
      <div>
        <dt class="label">Data dir</dt>
        <dd class="mono selectable mt-1 ml-0 text-ink-3">{app.boot?.dataDir}</dd>
      </div>
      {#if wan}
        <div>
          <dt class="label">WAN</dt>
          <dd class="mono mt-1 ml-0 text-xs text-ink-3">
            {wan.reachability}{wan.reflexiveAddr ? ` · seen as ${wan.reflexiveAddr}` : ""}{wan.advertise.length ? ` · advertises ${wan.advertise.join(", ")}` : ""}{wan.relayServer ? ` · relaying for ${wan.relayClients}` : ""}{wan.relays.length ? ` · via relays ${wan.relays.join(", ")}` : ""}
          </dd>
        </div>
      {/if}
    </dl>
  </div>

  <!-- key protection (B9) — beside Identity, because it is about that key -->
  <KeyProtection />

  <!-- the security key (Y2/Y3) — the other answer to "who could take it" -->
  <SecurityKey />

  <!-- contribute -->
  <div class={ui.card}>
    <div class="flex items-center justify-between gap-6">
      <div>
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          Contribute compute
          <Hint text="When off, this machine carries the no-execute capability: full control, no workloads." label="About contributing compute" />
        </h2>
        <p class="mt-1.5 mb-0 max-w-md text-sm leading-normal text-ink-4">
          When on, this machine executes mesh jobs like any node.
        </p>
      </div>
      <button
        class="relative h-[22px] w-[38px] shrink-0 rounded-full transition-colors disabled:cursor-not-allowed"
        style={`background: ${app.status?.executing ? "var(--accent)" : "var(--surface-2)"}; ${!app.status?.executing ? "box-shadow: inset 0 0 0 1px var(--edge-strong);" : ""} transition-duration: var(--dur-fast);`}
        disabled={toggleBusy || app.engineStopped}
        onclick={toggleContribute}
        role="switch"
        aria-checked={app.status?.executing}
        aria-label="contribute compute"
      >
        <!-- `transform`, not `left` (L-21): the knob slid by animating a
             layout property, which relayouts the row every frame for a
             16px journey. A translate is composited and looks identical. -->
        <span class="absolute top-[3px] left-[3px] h-4 w-4 rounded-full transition-transform" style={`transform: translateX(${app.status?.executing ? "16px" : "0px"}); background: var(--surface-1); box-shadow: var(--sh-control); transition-duration: var(--dur-fast);`}></span>
      </button>
    </div>
  </div>

  <!-- tags -->
  <div class={ui.card}>
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
      This peer's tags
      <Hint
        text="Tags are free-form; k=v is a convention. git-sync elects this peer to watch Git sources; no-execute is the toggle above."
        label="About tags"
      />
    </h2>
    <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4">
      Selectors and grants match on tags (<span class="mono">tag=dc=east</span>).
    </p>
    <div class="mt-3 flex flex-wrap items-center gap-1.5">
      {#each ownTags as t}
        <span class="inline-flex items-center gap-1 rounded-full bg-surface-2 pr-1.5">
          <Tag text={t} />
          <button class="text-xs text-ink-5 hover:text-error disabled:opacity-40" title="remove" disabled={tagBusy || !isAdmin || app.engineStopped} onclick={() => removeTag(t)}>✕</button>
        </span>
      {:else}
        <span class="text-sm text-ink-6">no tags</span>
      {/each}
    </div>
    <div class="mt-3 flex items-center gap-2">
      <input bind:value={newTag} placeholder="role=lab" class="field field-sunken mono h-9 w-56 px-3 text-sm outline-none" onkeydown={(e) => e.key === "Enter" && addTag()} disabled={!isAdmin || app.engineStopped} />
      <button class={ui.secondary} disabled={tagBusy || !newTag.trim() || !isAdmin || app.engineStopped} onclick={addTag}>Add tag</button>
      {#if !isAdmin}<span class="text-xs text-ink-5">tagging needs the admin role</span>{/if}
    </div>
  </div>

  {:else if pane === "director"}

  <!-- Director itself.
       Its own pane, and the split is not cosmetic: everything above is the
       *node* — an identity in the mesh, capacity it lends, tags other
       people's selectors match. This is the desktop app that happens to be
       looking at it, and "should this window open at login" has nothing to
       do with "should this machine run jobs". They shared a page for as
       long as the page was one scroll. -->
  <div class={ui.card}>
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
      Director
      <Hint text="The app, as against the peer it runs. Every other machine's updates live under Mesh → Updates; this card is the one you are sitting at." label="About this card" />
    </h2>
    <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4">
      Version <span class="mono">{app.status?.version ?? update?.current ?? ""}</span>
      {#if update?.available}
        · <span style="color: var(--accent-ink);">{update.available} is available</span>
      {:else if update && update.canVerify}
        · up to date
      {/if}
    </p>

    <label class="mt-4 flex cursor-pointer items-center justify-between gap-4 rounded-md px-3.5 py-3" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
      <span>
        <span class="block text-base font-medium text-ink">Start at login</span>
        <span class="mt-0.5 block text-xs text-ink-5">Into the tray, not over the desktop. Director is a peer: the mesh sees this machine as long as it runs.</span>
      </span>
      <input type="checkbox" class="switch" checked={autostart ?? false} disabled={autostart === null} onchange={(e) => setAutostart((e.currentTarget as HTMLInputElement).checked)} />
    </label>
    {#if autostartErr}<div class="mt-2 text-xs" style="color: var(--status-error-ink);">{autostartErr}</div>{/if}

    <!--
      Said here rather than nowhere. A tray-resident app whose tray is
      invisible is one whose window disappears on close and whose "start at
      login" starts nothing you can see — and on stock GNOME that is the
      default state, with no error anywhere to explain it.
    -->
    {#if tray && !tray.visible}
      <div class="mt-3 rounded-md px-3.5 py-3 text-xs leading-relaxed" style="background: var(--status-warn-wash); color: var(--status-warn-ink);">
        <span class="font-semibold">There is no tray icon on this desktop.</span>
        {tray.hint}
        <div class="mt-1.5" style="opacity: .85;">
          Until then Director closes rather than hiding, and starting at login opens the window.
        </div>
      </div>
    {/if}

    <!--
      The background LAN watch that puts "somebody wants to join" on the
      Access screen. On by default, because a queue you have to go looking
      for is a queue that leaves people waiting — but it is this Director
      sending multicast queries every few seconds, and an admin on a network
      where that is unwelcome should be able to say so without reading the
      source to find out it was happening.
    -->
    {#if app.isAdmin}
      <label class="mt-3 flex cursor-pointer items-center justify-between gap-4 rounded-md px-3.5 py-3" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
        <span>
          <span class="block text-base font-medium text-ink">Watch for machines wanting to join</span>
          <span class="mt-0.5 block text-xs text-ink-5">
            Looks around this network every few seconds so anyone starting Director nearby turns
            up on Access by themselves. Off means they need an invite instead.
          </span>
        </span>
        <input
          type="checkbox"
          class="switch"
          checked={app.watchingLan}
          onchange={(e) => {
            app.watchingLan = (e.currentTarget as HTMLInputElement).checked;
            if (!app.watchingLan) app.stopLanWatch();
            else app.startLanWatch();
          }}
        />
      </label>
    {/if}

    <div class="mt-4 flex flex-wrap items-center gap-2">
      {#if update?.available && !updateInstalled}
        <button class={ui.primary} disabled={updateBusy} onclick={installDirectorUpdate}>
          {updateProgress ? "Installing…" : `Install ${update.available} and restart`}
        </button>
      {/if}
      <button class={ui.secondary} disabled={updateBusy} onclick={checkDirectorUpdate}>
        {updateBusy && !updateProgress ? "Checking…" : "Check for Director updates"}
      </button>
      {#if logDir}
        <button class={ui.secondary} onclick={copyLogDir} title={logDir}>Copy log folder path</button>
      {/if}
    </div>
    {#if updateProgress}
      <div class="mt-3 h-[3px] overflow-hidden rounded-full bg-surface-2">
        <div class="h-full transition-[width]" style={`width: ${updateProgress.total ? Math.round((updateProgress.downloaded / updateProgress.total) * 100) : 30}%; background: var(--accent);`}></div>
      </div>
      <div class="mt-1.5 text-xs text-ink-5">
        {#if updateInstalled}
          {updateInstalled} installed — restarting.
        {:else}
          {fmtBytes(updateProgress.downloaded)}{updateProgress.total ? ` of ${fmtBytes(updateProgress.total)}` : ""}
        {/if}
      </div>
    {/if}
    {#if update?.notes && update.available}
      <pre class="mt-3 max-h-40 overflow-auto rounded-md px-3.5 py-3 text-xs leading-relaxed whitespace-pre-wrap" style="background: var(--surface-subtle); color: var(--ink-3);">{update.notes}</pre>
    {/if}
    {#if update?.note}<div class="mt-2 text-xs text-ink-5">{update.note}</div>{/if}
    {#if updateErr}<div class="mt-2 text-xs" style="color: var(--status-error-ink);">{updateErr}</div>{/if}
    {#if logDir}
      <div class="mono selectable mt-3 text-2xs text-ink-5">log: {logDir}</div>
    {/if}
  </div>

  {:else if pane === "fleet"}

  <!-- invites -->
  <div id="settings-invite" class={ui.card}>
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
      Invite a machine
      <Hint text="LAN machines pair from the Mesh view instead." label="About invites" />
    </h2>
    <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4">
      For machines not on this network: create an invite and run <span class="mono">meshnode join &lt;invite&gt;</span> there.
    </p>
    <div class="mt-4 grid grid-cols-4 gap-3" use:staggerIn={{ each: 40, y: 8 }}>
      <label class="block">
        <span class={ui.label}>Valid for</span>
        <input bind:value={inviteTtl} placeholder="24h" class={ui.inputMono} />
      </label>
      <label class="block">
        <span class={ui.label}>Uses</span>
        <input type="number" min="1" bind:value={inviteUses} class={`${ui.input} num`} />
      </label>
      <label class="col-span-2 block">
        <span class={ui.label}>Tags stamped on the joiner <span class="text-ink-6">(comma-separated)</span></span>
        <input bind:value={inviteTags} placeholder="role=worker, dc=east" class={ui.inputMono} />
      </label>
      <label class="col-span-4 block">
        <span class={ui.label}>Extra bootstrap address <span class="text-ink-6">(optional — a public host:port the joiner can reach that this peer does not know about)</span></span>
        <input bind:value={inviteAddrs} placeholder="203.0.113.5:7777" class={ui.inputMono} />
      </label>
    </div>
    <div class="mt-3">
      <button class={ui.primary} disabled={inviteBusy || !isAdmin || app.engineStopped} onclick={mintInvite}>{inviteBusy ? "Creating…" : "Create invite"}</button>
      {#if !isAdmin}<span class="ml-3 text-xs text-ink-5">creating invites needs the admin role</span>{/if}
    </div>
    {#if inviteErr}
      <div class={ui.errorBox}>{inviteErr}</div>
    {/if}
    {#if inviteToken}
      <div class="mt-3.5">
        <div class="on-code mono selectable dark-scroll max-h-28 overflow-y-auto rounded-md px-3.5 py-3 text-xs leading-relaxed break-all" style="background: var(--surface-code); color: var(--ink-code-2);">
          meshnode join {inviteToken}
        </div>
        <button class="mt-2 text-sm font-semibold" style="color: var(--accent-ink);" onclick={copyToken}>Copy join command</button>
      </div>
    {/if}
  </div>

  {:else if pane === "diagnostics"}

  <!-- doctor -->
  <div id="settings-doctor" class={ui.card}>
    <div class="flex items-center gap-3">
      <div>
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          Diagnostics
          <Hint
            text="It checks the data dir, disk, identity, genesis signature, runtimes, sandbox probes, peer reachability and clock skew."
            label="About diagnostics"
          />
        </h2>
        <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4"><span class="mono">meshnode doctor</span>: this machine's health, end to end.</p>
      </div>
      <div class="ml-auto flex items-center gap-2">
        {#if checks && doctorSummary && doctorSummary.fail + doctorSummary.warn > 0}
          <button
            class={ui.secondary}
            disabled={fixBusy || doctorBusy || app.engineStopped}
            title="meshnode doctor --fix: create the data dir, resume sinks a restore held, restart a daemon systemd manages — and say which findings have no automatic fix"
            onclick={runFix}
          >{fixBusy ? "Fixing…" : "Fix what can be fixed"}</button>
        {/if}
        <button class={ui.secondary} disabled={doctorBusy || fixBusy || app.engineStopped} onclick={runDoctor}>{doctorBusy ? "Probing…" : checks ? "Run again" : "Run doctor"}</button>
      </div>
    </div>
    {#if doctorErr}<div class={ui.errorBox}>{doctorErr}</div>{/if}
    {#if fixes}
      <div class="mt-3 overflow-hidden rounded-lg bg-canvas">
        {#each fixes as f (f.name + f.detail)}
          <div class="flex items-start gap-3 border-b px-3.5 py-2 text-sm last:border-0" style="border-color: var(--edge);">
            <span class="mono w-10 flex-none text-2xs font-semibold uppercase" style={f.applied ? "color: var(--accent-ink);" : "color: var(--ink-5);"}>{f.applied ? "fixed" : "—"}</span>
            <span class="mono w-28 flex-none text-ink-2">{f.name}</span>
            <span class="selectable text-ink-3">{f.detail}</span>
          </div>
        {/each}
      </div>
    {/if}
    {#if checks}
      <div class="mt-3 flex items-center gap-3 text-xs">
        <span class="num" style="color: var(--accent-ink);">{doctorSummary?.ok} ok</span>
        {#if doctorSummary?.warn}<span class="num" style="color: var(--status-warn-ink);">{doctorSummary.warn} warn</span>{/if}
        {#if doctorSummary?.fail}<span class="num text-error">{doctorSummary.fail} failed</span>{/if}
      </div>
      <div class="mt-2 overflow-hidden rounded-lg bg-canvas">
        <!-- Unkeyed: `name + detail` looked unique enough and is not
             guaranteed to be — two checks reporting the same thing about the
             same subject would take the whole doctor panel down. -->
        {#each checks as c}
          <div class="flex items-start gap-3 border-b px-3.5 py-2 text-sm last:border-0" style="border-color: var(--edge);">
            <span class="mono w-10 flex-none text-2xs font-semibold uppercase" style={c.verdict === "ok" ? "color: var(--accent-ink);" : c.verdict === "warn" ? "color: var(--status-warn-ink);" : "color: var(--status-error);"}>{c.verdict}</span>
            <span class="mono w-28 flex-none text-ink-2">{c.name}</span>
            <span class="selectable text-ink-3">{c.detail}</span>
          </div>
        {/each}
      </div>
    {/if}
  </div>

  {:else if pane === "facts"}

  <!-- fact collectors (M18) -->
  <div class={ui.card}>
    <div class="flex items-center gap-3">
      <div class="min-w-0">
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          Fact collectors
          <Hint
            text="The built-in system collector runs in process on every node and cannot be turned off — a node that cannot say what it is cannot be selected, patched or diagnosed. Facts are published only when the document changes, so a stable machine writes one record and then goes quiet."
            label="About fact collectors"
          />
        </h2>
        <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4">
          What every node is asked to report about itself.
        </p>
      </div>
      <button class={`${ui.secondary} ml-auto`} disabled={app.engineStopped} onclick={loadCollectors}>
        Refresh
      </button>
    </div>

    {#if collectorsErr}
      <div class={ui.errorBox}>{collectorsErr}</div>
    {/if}

    {#if flapping.length}
      <div class="mt-3 rounded-md px-3.5 py-2.5 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">
        <div class="font-semibold">
          {flapping.length} {flapping.length === 1 ? "collector is" : "collectors are"} publishing faster than once an hour.
        </div>
        <div class="mt-1 leading-relaxed">
          A fact is a property of the machine, not a measurement of it. These keys changed when nothing changed:
          {#each flapping as c (c.name)}
            <div class="mono mt-1 text-xs">{c.name} → {c.flappingKeys.join(", ")}</div>
          {/each}
        </div>
      </div>
    {/if}

    {#if collectors?.length}
      <div class="mt-3 overflow-hidden rounded-lg bg-canvas">
        {#each collectors as c (c.name)}
          <div class="flex items-center gap-3 border-b px-3.5 py-2.5 text-sm last:border-0" style="border-color: var(--edge);">
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-2">
                <span class="mono font-semibold text-ink-2">{c.name}</span>
                {#if c.builtIn}
                  <span class="rounded-full px-2 py-0.5 text-2xs font-semibold whitespace-nowrap" style="background: var(--accent-wash); color: var(--accent-ink);">built in</span>
                {:else if c.paused}
                  <span class="text-2xs font-semibold" style="color: var(--status-warn-ink);">paused</span>
                {/if}
                {#if c.flappingKeys.length}
                  <span class="text-2xs font-semibold text-error">flapping</span>
                {/if}
              </div>
              {#if c.description}
                <div class="mt-0.5 truncate text-xs text-ink-5">{c.description}</div>
              {/if}
            </div>
            <div class="hidden shrink-0 text-xs text-ink-5 sm:block">
              <span class="mono">{c.on}</span>
              <span class="text-ink-faint"> · </span>
              <span class="num">every {every(c.intervalSecs)}</span>
              <span class="text-ink-faint"> · </span>
              <span class="num">{c.subjects} {c.subjects === 1 ? "subject" : "subjects"}</span>
            </div>
            {#if !c.builtIn}
              <button class={ui.rowAction} onclick={() => openWiden(c)}>
                {widened(c.on) ? "Widen the ring…" : "Change who runs it…"}
              </button>
              <button
                class={ui.rowAction}
                disabled={collectorBusy === c.name}
                onclick={() => setPaused(c, !c.paused)}
              >
                {collectorBusy === c.name ? "…" : c.paused ? "Resume" : "Pause"}
              </button>
            {/if}
          </div>
        {/each}
      </div>
      <p class="mt-2.5 mb-0 text-xs text-ink-5">
        Publish new collectors with <span class="mono">meshnode collectors add</span>.
        <Hint
          text="Pausing and widening are both edits: the record is re-signed and re-published, so nothing changes what a fleet reports — or what it runs — without a signature behind it."
          label="About pausing and widening"
        />
      </p>

      {#if widening}
        <div class="fade-in mt-3 rounded-lg bg-canvas p-4">
          <div class="flex items-baseline gap-2">
            <h3 class="m-0 text-sm font-bold text-ink">
              Who runs <span class="mono">{widening.name}</span>
            </h3>
            <span class="text-xs text-ink-5">
              it runs <span class="mono">{widening.run}</span> on every node this matches, on a
              timer, with nobody pressing Run
            </span>
            <button class="ml-auto text-xs font-semibold text-ink-4 hover:text-ink" onclick={() => (widening = null)}>Close</button>
          </div>
          <div class="mt-3 grid gap-3 sm:grid-cols-2">
            <div>
              <span class={ui.label}>Now</span>
              <div class="mono mt-1.5 text-sm text-ink-3">{widening.on}</div>
              <div class="num mt-0.5 text-xs text-ink-5">
                {widening.subjects} {widening.subjects === 1 ? "node is" : "nodes are"} reporting
              </div>
            </div>
            <label class="block">
              <span class={ui.label}>After</span>
              <input
                bind:value={widenTo}
                oninput={previewRing}
                placeholder="tag=gpu"
                class={ui.inputMono}
              />
              {#if widenReach !== null}
                <div class="mt-1.5 text-xs" style={widenReach > widening.subjects ? "color: var(--status-warn-ink);" : "color: var(--ink-5);"}>
                  {widenReach} node{widenReach === 1 ? "" : "s"} match
                  {#if widenReach > widening.subjects}
                    — that starts the script on {widenReach - widening.subjects} machine{widenReach - widening.subjects === 1 ? "" : "s"} that are not running it
                  {/if}
                </div>
              {/if}
            </label>
          </div>
          {#if widenErr}
            <div class={ui.errorBox}>{widenErr}</div>
          {/if}
          <div class="mt-3 flex items-center gap-2">
            <button class={ui.primary} disabled={widenBusy || !widenTo.trim() || !isAdmin} onclick={doWiden}>
              {widenBusy ? "Publishing…" : "Publish the new ring"}
            </button>
            <span class="text-xs text-ink-5">takes effect on every machine within a few seconds</span>
          </div>
        </div>
      {/if}
    {:else if collectors}
      <p class="mt-3 mb-0 text-sm text-ink-5">No collectors are published yet.</p>
    {/if}
  </div>

  {:else if pane === "history"}

  <!-- M31 A1: how long this mesh keeps things.
       Above History export on purpose: "how long do we keep it" and "who
       else gets a copy" are two lines of the same paragraph, and reading
       them in that order is what makes the second one make sense. -->
  <div class={ui.card}>
    <div class="flex items-center gap-3">
      <div class="min-w-0">
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          How long this mesh keeps things
          <Hint
            text="Every node stores the whole replicated log, so its size is (surviving history) × (nodes). A finished job's record supersedes nothing, so without a window it is kept for ever. The window is one admin-signed policy for the whole mesh — two nodes reading different horizons would answer the same question differently. A node may keep less than the mesh window and never more."
            label="About retention"
          />
        </h2>
        <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4">
          Closed history leaves the replicated log after these windows.
        </p>
      </div>
      {#if !retentionEdit}
        <button
          class={`${ui.secondary} ml-auto`}
          disabled={!isAdmin || app.engineStopped || !retention}
          title={!retention ? "reading the current policy" : ""}
          onclick={startRetentionEdit}
        >Edit…</button>
      {/if}
    </div>

    {#if retentionErr}<div class={ui.errorBox}>{retentionErr}</div>{/if}

    {#if !retention && !retentionErr}
      <p class="mt-3 mb-0 text-sm text-ink-5">Reading the current policy…</p>
    {/if}

    {#if retention}
      <div class="mt-3 overflow-hidden rounded-lg bg-canvas">
        {#each retention.windows as w (w.table)}
          <div class="flex items-center gap-3 border-b px-3.5 py-2.5 text-sm last:border-0" style="border-color: var(--edge);">
            <span class="mono w-28 shrink-0 font-semibold text-ink-2">{w.table}</span>
            {#if retentionEdit}
              <input class={`${ui.inputMono} h-8 w-28`} bind:value={windowDraft[w.table]} placeholder="90d" />
            {:else}
              <span class="num w-28 text-ink">{w.window}</span>
            {/if}
            {#if w.floor}
              <span class="text-2xs text-ink-5">floor {w.floor}</span>
            {/if}
            {#if w.boundsTheOpLog}
              <span class="rounded-full px-2 py-0.5 text-2xs font-semibold whitespace-nowrap" style="background: var(--accent-wash); color: var(--accent-ink);">bounds the replicated log</span>
            {/if}
            {#if w.effectiveHere !== w.window}
              <span class="ml-auto text-2xs text-ink-5">this peer keeps {w.effectiveHere}</span>
            {/if}
          </div>
        {/each}
      </div>

      {#if retentionEdit}
        <div class="mt-3 flex flex-wrap items-center gap-2">
          <button class={ui.primary} disabled={retentionBusy} onclick={saveRetention}>{retentionBusy ? "Signing…" : "Sign and publish"}</button>
          <button class={ui.secondary} disabled={retentionBusy} onclick={() => (retentionEdit = false)}>Cancel</button>
          <button class={`${ui.secondary} ml-auto`} disabled={retentionBusy} onclick={resetRetention}>Back to defaults</button>
        </div>
        <p class="mt-2.5 mb-0 text-xs text-ink-5">
          <span class="mono">90d</span>, <span class="mono">1y</span>, <span class="mono">12w</span> or <span class="mono">forever</span>.
          The audit window may be raised and never lowered below a year — an audit log you can shorten to nothing is not an audit log.
        </p>
      {:else}
        <!-- The projection, and it is not decoration: a knob whose effect
             nobody can predict is a knob nobody turns. -->
        <div class="mt-3 flex flex-wrap items-baseline gap-x-6 gap-y-1 text-sm">
          <span class="text-ink-4">history now <span class="num font-semibold text-ink">{fmtBytes(Number(retention.oplogBytes))}</span></span>
          {#if Number(retention.retirableBytesPerDay) > 0}
            <span class="text-ink-4">closed records <span class="num text-ink-2">~{fmtBytes(Number(retention.retirableBytesPerDay))}/day</span></span>
          {/if}
          {#if Number(retention.permanentBytesPerDay) > 0}
            <span class="text-ink-4">
              permanent <span class="num text-ink-2">~{fmtBytes(Number(retention.permanentBytesPerDay))}/day</span>
              <Hint text="The audit log and other grow-only records. This grows whatever window you set, which is why it is shown apart — retention cannot fix it, and a projection that hid it would be a surprise later." label="About permanent growth" />
            </span>
          {/if}
        </div>
        <div class="mt-2 text-sm">
          {#if retention.settlesAtBytes !== null}
            <span class="text-ink-4">settles at</span>
            <span class="num font-semibold text-ink">~{fmtBytes(Number(retention.settlesAtBytes))}</span>
            <span class="text-ink-4">after a year</span>
          {:else}
            <span style="color: var(--status-warn-ink);">It does not settle — <span class="mono">runs</span> is <span class="mono">forever</span>.</span>
          {/if}
        </div>
        <p class="mt-2.5 mb-0 text-xs text-ink-5">
          {#if retention.signedBy}
            Signed by <span class="font-semibold text-ink-4">{retention.signedBy}</span>{retention.signedAtMicros ? ` · ${new Date(Number(retention.signedAtMicros) / 1000).toLocaleDateString()}` : ""}
          {:else}
            The shipped defaults — nobody has signed a policy. They are finite, so this mesh is already bounded.
          {/if}
        </p>

        <!-- M31 A3. The window above governs the steady state; this is the
             bad day. It appears only when something is actually being
             refused, because a control nobody has tripped is furniture. -->
        {#if pressure && (pressure.entities.length > 0 || pressure.flapping.length > 0)}
          <div class="mt-3 rounded-md p-3" style="background: var(--status-warn-wash);">
            <div class="text-sm font-semibold" style="color: var(--status-warn-ink);">
              {#if pressure.entities.length}
                Something is authoring records faster than this peer will carry
              {:else}
                A collector is changing its mind faster than it is worth recording
              {/if}
            </div>
            <p class="mt-1 mb-0 text-xs leading-relaxed text-ink-4">
              Refused here, on the machine that produced them — so the bug gets fixed rather
              than replicated to every peer. The limit is
              <span class="num">{pressure.limitPerMinute}</span> records a minute per thing.
            </p>
            {#each pressure.entities as row (row.entity)}
              <div class="mt-2 flex flex-wrap items-baseline gap-x-3 text-sm">
                <span class="mono font-semibold text-ink-3">{row.entity}</span>
                <span class="num text-ink-4">{row.observedPerMinute}/min</span>
                <span class="text-ink-4">{Number(row.refusedTotal).toLocaleString()} refused</span>
                {#if Number(row.pausedUntilMicros) > 0}
                  <!-- A brake, not a tombstone: nobody has to un-pause it. -->
                  <span class="text-ink-4">
                    paused until {pressureStamp(Number(row.pausedUntilMicros))} · clears itself
                  </span>
                {/if}
              </div>
            {/each}
            {#each pressure.flapping as key}
              <div class="mt-2 text-sm">
                <span class="mono font-semibold text-ink-3">{key}</span>
                <span class="ml-2 text-ink-4">held back — it changed again inside its publish window</span>
              </div>
            {/each}
          </div>
        {/if}
      {/if}
    {/if}
  </div>

  <!-- M31 B1/B2: history export. One row when nothing is configured, and
       nothing anywhere else in the app — §18 decision 01 as layout. -->
  <div class={ui.card}>
    <div class="flex items-center gap-3">
      <div class="min-w-0">
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          History export
          {#if !sinks?.length}
            <span class="ml-1 text-sm font-normal text-ink-5">— not configured</span>
          {/if}
          <Hint
            text="Optional, and most meshes never turn it on: this mesh keeps its own history and answers its own questions. Export is for teams who need every run record in a warehouse they already own. A sink never blocks a job — when its destination is unreachable it fills a bounded spool, then drops the oldest and says so."
            label="About history export"
          />
        </h2>
        <p class="mt-1.5 mb-0 max-w-lg text-sm leading-normal text-ink-4">
          Copy closed run and job records to a system you already own. Nothing here is required.
        </p>
      </div>
      {#if !sinkForm}
        <button class={`${ui.secondary} ml-auto`} disabled={!isAdmin || app.engineStopped} onclick={newSink}>
          {sinks?.length ? "Add a destination…" : "Set up…"}
        </button>
      {/if}
    </div>

    {#if sinksErr}<div class={ui.errorBox}>{sinksErr}</div>{/if}

    {#if sinks?.some((s) => s.pausedByRestore)}
      <div class="mt-3 rounded-md px-3.5 py-2.5 text-sm" style="background: var(--status-warn-wash); color: var(--status-warn-ink);">
        <div class="font-semibold">Nothing is exporting.</div>
        <div class="mt-1 leading-relaxed">
          This peer's data was restored from a backup, so every sink is held — a restored mesh must not
          silently re-push history into a live warehouse, and a forked one must not push into the warehouse
          it forked away from. Review the destinations below, then resume.
        </div>
        <button class={`${ui.secondary} mt-2.5`} disabled={sinkBusy === "resume"} onclick={resumeSinks}>
          {sinkBusy === "resume" ? "Resuming…" : "Resume exporting"}
        </button>
      </div>
    {/if}

    {#if sinks?.length}
      <div class="mt-3 overflow-hidden rounded-lg bg-canvas">
        {#each sinks as s (s.name)}
          <div class="border-b px-3.5 py-2.5 text-sm last:border-0" style="border-color: var(--edge);">
            <div class="flex items-center gap-3">
              <div class="min-w-0 flex-1">
                <div class="flex items-center gap-2">
                  <span class="mono font-semibold text-ink-2">{s.name}</span>
                  <span class="text-2xs text-ink-5">{s.kind}</span>
                  {#if !s.enabled}<span class="text-2xs font-semibold" style="color: var(--status-warn-ink);">off</span>{/if}
                </div>
                <!-- The resolved destination, and for a file that is the
                     answer to "where is it" — so it is copyable rather than
                     merely displayed and truncated. -->
                <div class="mt-0.5 flex min-w-0 items-center gap-2 text-xs text-ink-5">
                  <span class="mono selectable truncate" title={s.target}>{s.target}</span>
                  <!-- Retired file sinks still exist and still export, so
                       their path is still worth copying where there is one. -->
                  {#if s.kind === "ndjson" && (s.statusIsLocal || s.database.trim())}
                    <button class="shrink-0 cursor-pointer rounded-sm px-1.5 py-0.5 text-2xs text-ink-5 transition-colors hover:bg-surface-2 hover:text-ink-3" onclick={() => copyPath(s.target)}>copy path</button>
                  {/if}
                </div>
              </div>
              <!-- Counters, but only when they are this node's to report.
                   The cursor and spool are files on the exporting node, so
                   drawing our empty copy of them renders a destination that
                   has been failing every thirty seconds as "0 behind, 0
                   sent" — healthy-looking, and wrong. -->
              <div class="hidden shrink-0 text-xs text-ink-5 sm:block">
                {#if s.statusIsLocal}
                  <span class="num">{lagText(s.lagSecs)} behind</span>
                  <span class="text-ink-faint"> · </span>
                  <span class="num">{Number(s.sent).toLocaleString()} sent</span>
                  {#if Number(s.spooledBytes) > 0}
                    <span class="text-ink-faint"> · </span>
                    <span class="num">{fmtBytes(Number(s.spooledBytes))} spooled</span>
                  {/if}
                {:else}
                  <!-- Prose, so it takes the row's ink. --ink-faint is a
                       glyph token (app.css) and this is a sentence. -->
                  <span>counters on {s.elected || "the exporter"}</span>
                {/if}
              </div>
              <button class={ui.rowAction} disabled={!isAdmin} onclick={() => editSink(s)}>Edit…</button>
              <button class={ui.rowDanger} disabled={!isAdmin || sinkBusy === s.name} onclick={() => removeSink(s.name)}>
                {sinkBusy === s.name ? "…" : "Remove"}
              </button>
            </div>

            <!-- Everything below the row is a thing gone wrong, in the
                 order an operator should act on it. -->
            {#if Number(s.forcedGaps) > 0}
              <div class="mt-2 rounded-sm px-3 py-2 text-xs leading-relaxed" style="background: var(--status-error-wash); color: var(--status-error-ink);">
                <span class="font-semibold">{Number(s.forcedGaps).toLocaleString()} record(s) were deleted before this destination took them.</span>
                Its {s.holdForExportDays}-day hold expired while the destination was unreachable, so the mesh kept
                deleting. <span class="font-semibold">That export gap is permanent.</span>
              </div>
            {/if}
            {#if !s.elected && s.enabled}
              <div class="mt-2 rounded-sm px-3 py-2 text-xs leading-relaxed" style="background: var(--status-warn-wash); color: var(--status-warn-ink);">
                No live node is eligible to export this. Check which machines it runs on, and that they run a
                build new enough to have sinks.
              </div>
            {:else if Number(s.dropped) > 0}
              <div class="mt-2 rounded-sm px-3 py-2 text-xs leading-relaxed" style="background: var(--status-warn-wash); color: var(--status-warn-ink);">
                {Number(s.dropped).toLocaleString()} batch(es) dropped — the spool filled while the destination was
                failing. Those rows are gone.
              </div>
            {/if}
            {#if Number(s.payloadsTooLarge) > 0}
              <div class="mt-1.5 text-xs text-ink-5">
                {Number(s.payloadsTooLarge).toLocaleString()} job output(s) were over the size cap — the record carries
                the reference, the bytes stayed on the node that produced them.
              </div>
            {/if}
            <div class="mt-1.5 flex flex-wrap items-center gap-x-3 text-xs text-ink-5">
              <span>tables <span class="mono">{s.tables.join(", ")}</span></span>
              {#if s.elected}<span>exporting from <span class="font-semibold text-ink-4">{s.elected}</span></span>{/if}
              {#if s.lastStatus}<span class="mono">last: {s.lastStatus}</span>{/if}
              <!-- The sentence that would have saved fifteen minutes: this
                   card is not silent because the destination is fine, it is
                   silent because the reading lives on another machine. -->
              {#if !s.statusIsLocal && s.elected && s.enabled}
                <span style="color: var(--status-warn-ink);">
                  status is on {s.elected} — a failure here would not show on this screen
                </span>
              {/if}
              {#if !s.replayPayloads}<span>Director cannot read this back</span>{/if}
            </div>
          </div>
        {/each}
      </div>
    {/if}

    {#if sinkForm}
      <div class="fade-in mt-3 rounded-lg bg-canvas p-4">
        <div class="text-sm font-semibold text-ink">
          {sinkForm.editing ? `Edit ${sinkForm.editing}` : "New destination"}
        </div>

        <!-- Say what you get back **before** they choose. Rendered from the
             capability triple on the enum, so it cannot drift from the code. -->
        <div class="mt-3">
          <span class={ui.label}>Destination</span>
          <div class="grid gap-1.5 sm:grid-cols-2">
            {#each sinkKinds as k (k.kind)}
              <button
                class="rounded-md border px-3 py-2.5 text-left transition-colors"
                style="border-color: {sinkForm.kind === k.kind ? 'var(--accent)' : 'var(--edge)'}; background: {sinkForm.kind === k.kind ? 'var(--accent-wash)' : 'transparent'};"
                onclick={() => pickKind(k.kind)}
              >
                <div class="text-sm font-semibold text-ink">{k.label}</div>
                <div class="mt-0.5 text-xs leading-relaxed text-ink-5">{k.capability}</div>
              </button>
            {/each}
          </div>
          <!-- And once chosen, what reading back will do **as configured** —
               so a write-only token reads as one permission missing rather
               than as the destination's fault (§14.4 rule 3). -->
          {#if replayNote}
            <p class="mt-2 mb-0 text-xs leading-relaxed text-ink-5">{replayNote}</p>
          {/if}
        </div>

        <div class="mt-3 grid gap-3 sm:grid-cols-2">
          <div>
            <span class={ui.label}>Name</span>
            <input class={ui.inputMono} bind:value={sinkForm.name} placeholder="warehouse" disabled={!!sinkForm.editing} />
          </div>
          {#if selectedKind?.needsUrl}
            <div>
              <span class={ui.label}>URL</span>
              <input class={ui.inputMono} bind:value={sinkForm.url} placeholder="https://warehouse.corp:8443" />
            </div>
            <div>
              <span class={ui.label}>Database or index prefix</span>
              <input class={ui.inputMono} bind:value={sinkForm.database} placeholder="meshware" />
            </div>
            <div>
              <span class={ui.label}>
                Credential
                <Hint text="A secret from this mesh's vault, by name. The value never enters Director and never enters the sink record — the machine that connects resolves it at connect time." label="About the credential" />
              </span>
              <select class={ui.input} bind:value={sinkForm.credential}>
                <option value="">none</option>
                {#each app.secrets ?? [] as sec (sec.name)}
                  <option value={sec.name}>{sec.name}</option>
                {/each}
              </select>
            </div>
            {#if selectedKind?.replayPayloads}
              <div>
                <span class={ui.label}>
                  Read credential
                  <Hint text="A second secret, used only to read job output back into Director. The token that may INSERT should not have to SELECT — so this is separate, and leaving it empty means replay is refused rather than falling back to the write token." label="About the read credential" />
                </span>
                <select class={ui.input} bind:value={sinkForm.readCredential}>
                  <option value="">none — replay refused</option>
                  {#each app.secrets ?? [] as sec (sec.name)}
                    <option value={sec.name}>{sec.name}</option>
                  {/each}
                </select>
              </div>
            {/if}
          {:else if sinkForm.kind === "plugin"}
            <div class="sm:col-span-2">
              <span class={ui.label}>Program</span>
              <input class={ui.inputMono} bind:value={sinkForm.database} placeholder="/usr/local/bin/mw-ship" />
            </div>
          {/if}
        </div>

        <div class="mt-3">
          <span class={ui.label}>Which tables</span>
          <div class="flex flex-wrap gap-2">
            {#each ["jobs", "audit", "usage"] as t (t)}
              <button
                class="rounded-full border px-3 py-1 text-xs transition-colors"
                style="border-color: {sinkForm.tables.includes(t) ? 'var(--accent)' : 'var(--edge)'}; background: {sinkForm.tables.includes(t) ? 'var(--accent-wash)' : 'transparent'}; color: {sinkForm.tables.includes(t) ? 'var(--accent-ink)' : 'var(--ink-5)'};"
                onclick={() => toggleTable(t)}
              >{t}</button>
            {/each}
          </div>
          {#if sinkForm.tables.includes("audit")}
            <p class="mt-1.5 mb-0 text-xs" style="color: var(--status-warn-ink);">
              The audit trail is the most sensitive thing this mesh holds. Sending it somewhere else is a
              decision worth being sure about.
            </p>
          {/if}
        </div>

        <div class="mt-3 grid gap-3 sm:grid-cols-2">
          <div>
            <span class={ui.label}>
              Export from
              <Hint text="Only some machines usually have a route to a warehouse — a firewall exception, a VPC, a proxy. Blank means every live executor." label="About which nodes export" />
            </span>
            <input class={ui.inputMono} bind:value={sinkForm.runOn} placeholder="tag=sink" />
          </div>
          <div>
            <span class={ui.label}>
              Hold deletion for
              <Hint text="Days past the retention window that a record is kept while this destination has not taken it. At the ceiling the mesh keeps deleting and reports the gap as permanent — a stalled destination must never pin the log open on every node for ever. Maximum 30 days; 0 is no hold." label="About the export hold" />
            </span>
            <input class={`${ui.inputMono} num`} type="number" min="0" max="30" bind:value={sinkForm.holdForExportDays} />
          </div>
        </div>

        <label class="mt-3 flex items-start gap-2.5 text-sm">
          <input type="checkbox" class="mt-0.5" bind:checked={sinkForm.payloads} />
          <span>
            <span class="text-ink">Include raw job output</span>
            <span class="block text-xs leading-relaxed text-ink-5">
              stdout and stderr leave from the machine that produced them, capped at {sinkForm.payloadMaxMb} MB per job.
              {#if !selectedKind?.payloadsByDefault}
                Think before turning this on for a destination priced per gigabyte.
              {/if}
            </span>
          </span>
        </label>

        {#if sinkTestResult}
          <div class="mt-3 rounded-sm px-3 py-2 text-sm" style="background: {sinkTestResult.ok ? 'var(--accent-wash)' : 'var(--status-error-wash)'}; color: {sinkTestResult.ok ? 'var(--accent-ink)' : 'var(--status-error-ink)'};">
            <span class="mono selectable">{sinkTestResult.text}</span>
          </div>
        {/if}

        <div class="mt-3 flex flex-wrap items-center gap-2">
          <button class={ui.primary} disabled={sinkBusy === "save" || !sinkForm.name.trim()} onclick={requestSave}>
            {sinkBusy === "save" ? "Signing…" : "Save"}
          </button>
          {#if selectedKind?.needsUrl}
            <button class={ui.secondary} disabled={sinkBusy === "test" || !sinkForm.url.trim()} onclick={testSink}>
              {sinkBusy === "test" ? "Testing…" : "Test connection"}
            </button>
          {/if}
          <button class={ui.secondary} onclick={() => { sinkForm = null; sinkTestResult = null; }}>Cancel</button>
        </div>
        <p class="mt-2.5 mb-0 text-xs text-ink-5">
          Creating a destination is an admin action and is recorded in the audit log with the host named in full.
        </p>
      </div>
    {/if}
  </div>

  {:else if pane === "storage"}

  <!-- M31 A2: the content store. The one thing a node keeps that grows
       without a bound of its own — every release, every source sync, every
       bundle anyone ever deployed here. Until A2 nothing ever deleted a
       byte of it, and the first symptom was a full disk. -->
  <div id="settings-content" class={ui.card}>
    <div class="flex items-center gap-3">
      <div class="min-w-0">
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          What this machine keeps
          <Hint
            text="Bundles are the code and data the mesh distributes: services, git source checkouts, meshnode's own releases. They arrive here when something runs and stay until reclaimed. A reclaim never touches a bundle something still needs — a live service version, a deployment's pinned version, a source head, or this node's own release — at any level, including the aggressive one."
            label="About the content store"
          />
        </h2>
        <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4">
          How long unused bundles stay on this machine before they are reclaimed.
        </p>
      </div>
    </div>

    {#if contentErr}<div class={ui.errorBox}>{contentErr}</div>{/if}

    {#if !content && !contentErr}
      <p class="mt-3 mb-0 text-sm text-ink-5">Reading the content store…</p>
    {:else if content}
      <!-- The three numbers, as a bar plus its legend rather than a line of
           prose with bold bits in it. What is held splits exactly two ways —
           still needed, and reclaimable — so a stacked bar is not decoration
           here, it *is* the sentence: the reclaimable slice is the part of
           the disk pressing "Reclaim now" gives back. -->
      {@const held = Math.max(1, content.bytes)}
      {@const freeable = Math.min(content.reclaimableBytes, content.bytes)}
      {@const pinned = Math.max(0, content.bytes - freeable)}
      <div class="mt-4">
        <div class="flex items-baseline justify-between gap-3">
          <span class="num text-2xl font-bold tracking-[-.03em] text-ink">{fmtBytes(content.bytes)}</span>
          <span class="text-sm text-ink-5">
            in <span class="num">{content.bundles}</span> bundle{content.bundles === 1 ? "" : "s"}
          </span>
        </div>
        <div class="mt-2.5 flex h-2 w-full gap-[2px] overflow-hidden rounded-full" style="background: var(--track);">
          <span style={`width:${(pinned / held) * 100}%; background: var(--accent); border-radius: 999px;`}></span>
          <span style={`width:${(freeable / held) * 100}%; background: var(--ink-6); border-radius: 999px;`}></span>
        </div>
        <div class="mt-2.5 flex flex-wrap items-center gap-x-5 gap-y-1.5 text-sm">
          <span class="flex items-center gap-[7px] text-ink-4">
            <span class="h-2 w-2 rounded-full" style="background: var(--accent);"></span>
            <span class="num font-semibold text-ink">{fmtBytes(pinned)}</span> still needed
            <span class="text-ink-6">({content.protected} bundle{content.protected === 1 ? "" : "s"})</span>
          </span>
          <span class="flex items-center gap-[7px] text-ink-4">
            <span class="h-2 w-2 rounded-full" style="background: var(--ink-6);"></span>
            <span class="num font-semibold text-ink">{fmtBytes(content.reclaimableBytes)}</span> reclaimable now
          </span>
        </div>
      </div>

      <!-- The ladder comes from the engine, so this screen and
           `meshnode content --help` cannot disagree about what a rung
           means.

           The selected rung used to be painted with `var(--surface-sel)`,
           which is not a token this app defines — so the one control on the
           pane whose whole job is to show a choice showed it with a 0.5px
           shadow and no fill at all, in either theme. It is a proper card
           now: accent hairline, accent tint, a filled radio with a ring,
           and its retention spelled out on the right so the rungs can be
           compared without reading four sentences. -->
      <div class="policy-ladder mt-4" role="radiogroup" aria-label="Reclaim policy">
        {#each content.options as opt (opt.id)}
          {@const on = pendingPolicy === opt.id}
          <button
            type="button"
            role="radio"
            aria-checked={on}
            disabled={contentBusy || app.engineStopped}
            class="policy-opt"
            class:is-on={on}
            onclick={() => pickPolicy(opt.id)}
          >
            <span class="policy-dot" aria-hidden="true"></span>
            <span class="min-w-0 flex-1">
              <span class="block text-sm font-semibold text-ink">{opt.label}</span>
              <span class="mt-[3px] block text-sm leading-snug text-ink-4">{opt.description}</span>
            </span>
            <span class="policy-when num">
              {#if opt.immediate}
                at once
              {:else if opt.days === 0}
                forever
              {:else}
                {opt.days}d
              {/if}
            </span>
          </button>
        {/each}
      </div>

      {#if content.cappedByMesh}
        <p class="mt-3 mb-0 text-sm text-ink-5">
          The mesh retention policy caps this at <span class="num">{content.windowDays}d</span>.
          A node may keep less than the mesh window and never more —
          <button class="font-semibold" style="color: var(--accent-ink);" onclick={() => app.goSub("history")}>change it for the whole mesh</button>.
        </p>
      {/if}

      <div class="mt-4 flex flex-wrap items-end gap-3 border-t border-edge pt-4">
        <label class="block">
          <span class={ui.label}>Also cap the store at <span class="text-ink-6">(MB, blank for no cap)</span></span>
          <input
            bind:value={capMb}
            inputmode="numeric"
            placeholder="no cap"
            class="{ui.input} num w-36"
            onchange={() => pickPolicy(pendingPolicy ?? content?.policy ?? "balanced")}
          />
        </label>
        <button class={ui.secondary} disabled={contentBusy || app.engineStopped} onclick={previewPrune}>
          {contentBusy ? "Measuring…" : "What would this free?"}
        </button>
        <button
          class={ui.primary}
          disabled={contentBusy || app.engineStopped || content.reclaimableBundles === 0}
          onclick={() => runPrune(false)}
        >Reclaim now</button>
      </div>

      {#if preview}
        <p class="mt-3 mb-0 text-sm text-ink-4">
          {#if preview.dryRun}
            Would free <span class="num font-semibold text-ink">{fmtBytes(preview.bytesReclaimed)}</span>
            from {preview.bundlesRemoved} bundle{preview.bundlesRemoved === 1 ? "" : "s"}.
          {:else}
            Freed <span class="num font-semibold text-ink">{fmtBytes(preview.bytesReclaimed)}</span>
            from {preview.bundlesRemoved} bundle{preview.bundlesRemoved === 1 ? "" : "s"}.
          {/if}
          {#if preview.orphanChunks > 0}
            A further {fmtBytes(preview.orphanBytes)} in {preview.orphanChunks} orphaned chunks.
          {/if}
          {preview.protected} kept because something still needs them.
        </p>
      {/if}
    {/if}
  </div>

  <!-- Room left, here and everywhere. The promise A2 is actually making:
       meshware cleans up after itself before it asks for help, and stops
       adding when it cannot. -->
  <div id="settings-disk" class={ui.card}>
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
      Room left
      <Hint
        text="Below the floor, this node reclaims its content store and then refuses to fetch new bundles rather than fill the volume. The floor is the larger of [disk] min_free_mb and min_free_pct of the filesystem, capped so a large array does not reserve an absurd amount."
        label="About the disk floor"
      />
    </h2>

    {#if content}
      <!-- Used, free, and where the floor sits — on one bar, because the
           only question this card answers is "how close is the fill to the
           line". Two numbers side by side made that a subtraction. -->
      {@const total = content.diskTotalBytes}
      {@const used = total > 0 ? Math.max(0, total - content.diskFreeBytes) : 0}
      {@const usedPct = total > 0 ? (used / total) * 100 : 0}
      {@const floorPct = total > 0 ? Math.min(100, ((total - content.diskFloorBytes) / total) * 100) : null}
      <div class="mt-3">
        <div class="flex flex-wrap items-baseline gap-x-2 gap-y-1">
          <span class="num text-2xl font-bold tracking-[-.03em] text-ink">{fmtBytes(content.diskFreeBytes)}</span>
          <span class="text-sm text-ink-5">
            free on this machine{#if total > 0}{" "}of <span class="num">{fmtBytes(total)}</span>{/if}
          </span>
        </div>
        {#if total > 0}
          <div class="relative mt-2.5 h-2 w-full overflow-hidden rounded-full" style="background: var(--track);">
            <span
              class="absolute inset-y-0 left-0 rounded-full transition-[width] duration-500"
              style={`width:${usedPct}%; background:${content.diskCritical ? "var(--status-error)" : usedPct > 85 ? "var(--status-warn)" : "var(--accent)"};`}
            ></span>
            {#if floorPct !== null}
              <!-- The floor, as a tick on the bar. Past it, this node stops
                   fetching bundles rather than filling the volume. -->
              <span
                class="absolute inset-y-[-2px] w-[2px] rounded-full"
                style={`left:${floorPct}%; background: var(--ink-3);`}
                title={`floor — ${fmtBytes(content.diskFloorBytes)} kept free`}
              ></span>
            {/if}
          </div>
        {/if}
        <div class="mt-2 text-sm text-ink-5">
          Reclaims below <span class="num font-semibold text-ink-3">{fmtBytes(content.diskFloorBytes)}</span> free,
          then refuses new bundles rather than fill the volume.
        </div>
      </div>
      {#if content.diskCritical}
        <div class={ui.errorBox}>
          Under the floor after reclaiming — this node is refusing new bundles until there
          is room. Free space on the data dir, or lower <span class="mono">min_free_mb</span>.
        </div>
      {/if}
    {/if}

    {#if disks.length > 0}
      <!-- The rest of the fleet. A bare four-column table read as a data
           dump of numbers that are only interesting relative to each other,
           so every row carries its own fill: the eye finds the full disk
           before it reads a single figure. -->
      <div class="mt-4 border-t border-edge pt-3.5">
        <div class="mb-1.5 flex items-center gap-3">
          <span class="microlabel min-w-0 flex-1">Every machine</span>
          <span class="w-24 flex-none sm:w-32"></span>
          <span class="microlabel w-24 flex-none text-right">Free</span>
          <span class="microlabel w-24 flex-none text-right">Total</span>
          <span class="w-20 flex-none"></span>
        </div>
        <div class="flex flex-col">
          {#each disks as d (d.nodeId)}
            {@const pct = d.totalBytes > 0 ? Math.max(0, Math.min(100, ((d.totalBytes - d.freeBytes) / d.totalBytes) * 100)) : null}
            <div class="flex items-center gap-3 py-[7px]" style="border-top: 1px solid var(--edge);">
              <span class="min-w-0 flex-1 truncate text-sm text-ink">
                {d.name}{#if !d.online}<span class="ml-1.5 text-xs text-ink-6">offline</span>{/if}
              </span>
              <span class="h-[6px] w-24 flex-none overflow-hidden rounded-full sm:w-32" style="background: var(--track);">
                {#if pct !== null}
                  <span
                    class="block h-full rounded-full"
                    style={`width:${pct}%; background:${d.state === "low" ? "var(--status-error)" : d.state === "warn" ? "var(--status-warn)" : "var(--accent)"};`}
                  ></span>
                {/if}
              </span>
              <span class="num w-24 flex-none text-right text-sm whitespace-nowrap text-ink-3">
                {d.state === "unknown" ? "—" : fmtBytes(d.freeBytes)}
              </span>
              <span class="num w-24 flex-none text-right text-sm whitespace-nowrap text-ink-5">
                {d.totalBytes > 0 ? fmtBytes(d.totalBytes) : "—"}
              </span>
              <span
                class="w-20 flex-none text-right text-xs font-semibold whitespace-nowrap"
                style={d.state === "low"
                  ? "color: var(--status-error-ink);"
                  : d.state === "warn"
                    ? "color: var(--status-warn-ink);"
                    : "color: var(--ink-6);"}
              >{d.state === "ok" ? "" : d.state}</span>
            </div>
          {/each}
        </div>
      </div>
    {/if}
  </div>

  {:else if pane === "backup"}

  <!-- config + backup -->
  <div class={ui.card}>
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
      Engine configuration & backup
      <Hint
        text="Built in: an ephemeral port, capacity derived from this machine, default retention, no scrape endpoint. A backup is one restorable file: this machine's identity and the mesh's signed history of every change (meshnode backup / restore)."
        label="About configuration and backup"
      />
    </h2>
    <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-4">
      Director's embedded peer runs with built-in settings.
    </p>
    <div class="mt-4 flex flex-wrap items-center gap-2">
      <button class={ui.secondary} disabled={app.engineStopped} onclick={showConfig}>{configOpen ? "Hide resolved config" : "Show resolved config"}</button>
      <button class={ui.secondary} disabled={backupBusy || !isAdmin || app.engineStopped} onclick={doBackup}>{backupBusy ? "Writing…" : "Back up this node…"}</button>
      {#if !isAdmin}<span class="text-xs text-ink-5">backup needs the admin role</span>{/if}
    </div>
    {#if configOpen && configJson}
      <pre class="on-code mono selectable dark-scroll mt-3 max-h-80 overflow-auto rounded-md px-3.5 py-3 text-xs leading-relaxed" style="background: var(--surface-code); color: var(--ink-code-2);">{configJson}</pre>
    {/if}
    {#if backupErr}<div class={ui.errorBox}>{backupErr}</div>{/if}
    {#if backupResult}<div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--accent-wash); color: var(--accent-ink);">{backupResult}</div>{/if}
  </div>

  <!-- N6 §12 — the mesh-wide shed.
       Above Drain deliberately, and the pair is the point: draining is
       *this peer leaving politely*, shedding is *the whole mesh stopping
       taking work*. They sit together because an operator arrives looking
       for one and often wants the other, and apart in wording because
       confusing them at 3am is expensive. -->
  <div class={ui.card}>
    <div class="flex items-start gap-3">
      <div>
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Shed load, mesh-wide</h2>
        <p class="mt-1.5 mb-0 max-w-lg text-sm leading-normal text-ink-4">
          Stop the whole mesh taking new work, without going round the nodes.
          <strong>Running jobs are unaffected</strong> — a shed is about what starts
          next.
        </p>
      </div>
      {#if shed?.submissions || shed?.reconcile}
        <button
          class="btn btn-secondary btn-md ml-auto"
          disabled={!isAdmin || shedBusy}
          onclick={() => void setShed(false, false)}>Lift everything</button
        >
      {/if}
    </div>

    {#if shedErr}<div class={ui.errorBox}>{shedErr}</div>{/if}

    <div class="mt-3 flex flex-col gap-2">
      <label class="flex items-start gap-2.5 text-sm">
        <input
          type="checkbox"
          class="mt-0.5"
          disabled={!isAdmin || shedBusy}
          checked={shed?.submissions ?? false}
          onchange={(e) =>
            void setShed(e.currentTarget.checked, shed?.reconcile ?? false)}
        />
        <span>
          <span class="font-semibold text-ink">Submissions</span>
          <span class="block text-xs text-ink-4">
            Work people submit — a run, a service, a fan-out, a benchmark. Refused
            at the origin with your reason attached.
          </span>
        </span>
      </label>
      <label class="flex items-start gap-2.5 text-sm">
        <input
          type="checkbox"
          class="mt-0.5"
          disabled={!isAdmin || shedBusy}
          checked={shed?.reconcile ?? false}
          onchange={(e) =>
            void setShed(shed?.submissions ?? false, e.currentTarget.checked)}
        />
        <span>
          <span class="font-semibold text-ink">Reconcile</span>
          <span class="block text-xs text-ink-4">
            Work the mesh starts on its own: deployments replacing instances,
            schedules firing. The half that keeps starting things while you are
            trying to stop them.
          </span>
        </span>
      </label>
      <label class="mt-1 block">
        <span class={ui.label}>Reason</span>
        <input
          class={ui.input}
          disabled={!isAdmin || shedBusy}
          bind:value={shedReason}
          placeholder="draining the SAN, back by 14:00"
          oninput={() => (shedDirty = true)}
          onblur={() => {
            // Only when it actually changed. Writing on every blur authored
            // a fresh signed op — and an audit entry — each time somebody
            // clicked away from the field without touching it.
            if (!shedDirty || !shed) return;
            if (shedReason === shed.reason) {
              shedDirty = false;
              return;
            }
            // And only when something is shed: a reason with no shed is a
            // note nobody will ever read, and writing one would turn an
            // empty record into a set one.
            if (shed.submissions || shed.reconcile) {
              void setShed(shed.submissions, shed.reconcile);
            } else {
              shedDirty = false;
            }
          }}
        />
        <span class="mt-1 block text-xs text-ink-5">
          Shown in every refusal this causes. A job refused with no reason is a bug
          report; one refused with a sentence is an answer.
        </span>
      </label>
    </div>

    {#if shed && (shed.submissions || shed.reconcile)}
      <div class="mt-3 text-xs text-ink-4">
        honoured by <span class="num">{shed.honouring}</span> of
        <span class="num">{shed.members}</span> nodes{shed.byName
          ? ` · set by ${shed.byName}`
          : ""}
      </div>
      <!-- Not a nicety. This is the one capability gate in meshware that
           fails *open*: a node too old to read the record keeps taking
           work, which is the opposite of what was asked for. -->
      {#if shed.warning}
        <div class={ui.errorBox}>{shed.warning}</div>
      {/if}
    {/if}
    {#if !isAdmin}<div class="mt-2 text-xs text-ink-5">shedding load needs the admin role</div>{/if}
  </div>

  <!-- drain -->
  <div class={ui.card}>
    <div class="flex items-center gap-3">
      <div>
        <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          Drain this machine
          <Hint text="Director stays open; start again from here." label="About draining" />
        </h2>
        <p class="mt-1.5 mb-0 max-w-lg text-sm leading-normal text-ink-4">Stop accepting work, finish (or move) what was accepted, then stop this machine's node — the graceful way out before maintenance.</p>
      </div>
      <button
        class="btn btn-danger btn-md ml-auto"
        disabled={!canDrain || app.engineStopped}
        onclick={() => { drainPhase = "idle"; drainErr = null; drainOpen = true; }}
      >
        Drain…
      </button>
    </div>
    {#if !canDrain}<div class="mt-2 text-xs text-ink-5">draining needs the operator or admin role</div>{/if}
  </div>

  {:else if pane === "about"}
    <AboutPane />
  {/if}

  <!-- The build this peer runs is the line people arrive at looking for the
       *fleet's* builds, so it is where the pointer belongs. Outside the
       panes: it is a property of the machine, not of whichever one you
       happen to be reading. -->
  <!-- The licence is the build's (UX-7): this line said GPL-3.0-or-later,
       as a literal, for a week after the licence stopped being that. -->
  <div class="pb-3 text-center text-2xs text-ink-5">
    meshware {app.status?.version} · protocol {app.status?.proto}{#if app.about}{" · "}<button
        class="linkish text-2xs"
        onclick={() => app.go("settings", "about")}>{app.about.licenseName}</button
      >{/if}
    <button
      class="linkish ml-1.5 text-2xs"
      onclick={() => app.go("mesh", "updates")}
    >
      what the fleet runs →
    </button>
  </div>
</div>

<!-- §19 Q6, decided: `run_on` stays editable, behind a dialog that names
     both sides. Moving an export is genuinely safe — the cursor is a
     property of the data, so the new machine re-reads it and continues —
     and the real risk is somebody changing it without knowing why it was
     set. Naming the machines is what addresses that, rather than a warning
     that says "are you sure". -->
<Modal open={!!moveConfirm} kicker="move export" peer={sinkForm?.name ?? ""} onclose={() => (moveConfirm = null)}>
  <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Move this export?</h3>
  <div class="mt-3 overflow-hidden rounded-lg bg-canvas text-sm">
    <div class="flex gap-3 border-b px-3.5 py-2.5" style="border-color: var(--edge);">
      <span class="w-14 shrink-0 text-ink-5">now</span>
      <span class="mono text-ink-2">{moveConfirm?.from}</span>
    </div>
    <div class="flex gap-3 px-3.5 py-2.5">
      <span class="w-14 shrink-0 text-ink-5">then</span>
      <span class="mono text-ink-2">{moveConfirm?.to}</span>
    </div>
  </div>
  <p class="mt-3 mb-0 text-sm leading-relaxed text-ink-4">
    Exporting continues from the same place: the cursor is derived from the data, so the new machine
    reads it and carries on. Nothing is re-sent and nothing is lost.
  </p>
  <div class="mt-4 flex gap-2">
    <button class={ui.primary} disabled={sinkBusy === "save"} onclick={saveSink}>
      {sinkBusy === "save" ? "Signing…" : "Move export"}
    </button>
    <button class={ui.secondary} onclick={() => (moveConfirm = null)}>Cancel</button>
  </div>
</Modal>

<!-- drain modal (dark: this takes the peer out of rotation) -->
<Modal open={drainOpen} kicker="drain peer" peer={app.status?.nodeName ?? ""} onclose={() => { if (drainPhase !== "draining") drainOpen = false; }}>
  <div class="px-6 pt-4 pb-6">
    {#if drainPhase === "idle" || drainPhase === "failed"}
      <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Drain {app.status?.nodeName}?</h3>
      <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
        This machine's node announces <span class="mono" style="color: var(--ink-code-2);">draining</span>, refuses new work, and waits for {app.status?.jobsRunning ?? 0} running job{(app.status?.jobsRunning ?? 0) === 1 ? "" : "s"} to finish. Then this machine's node stops, and stays stopped until you start it.
      </p>
      <label class="mt-4 flex cursor-pointer items-center gap-3 rounded-md px-3.5 py-3 text-sm" style="background: rgba(160,176,200,.08);">
        <input type="checkbox" class="check" bind:checked={drainMove} />
        <span>Move queued jobs to other nodes first <span class="mono" style="color: var(--ink-code-3);">(--move)</span></span>
      </label>
      {#if drainErr}<div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{drainErr}</div>{/if}
      <div class="mt-5 grid grid-cols-2 gap-2.5">
        <button class="btn btn-lg border border-edge-code font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]" onclick={() => (drainOpen = false)}>Cancel</button>
        <button class="btn btn-danger btn-lg" onclick={doDrain}>Drain</button>
      </div>
    {:else if drainPhase === "draining"}
      <div class="px-0 pt-[18px] pb-2 text-center">
        <div class="flex justify-center"><Spinner size={34} thickness={2.75} color="var(--accent-bright)" track="color-mix(in srgb, var(--accent-bright) 18%, transparent)" /></div>
        <div class="mt-[18px] text-md font-semibold">Draining…</div>
        <div class="num mt-1.5 text-sm" style="color: var(--ink-code-3);">
          {drainProgress ? `${drainProgress.running} running · ${drainProgress.queued} queued${drainProgress.moved ? ` · ${drainProgress.moved} moved` : ""}` : "refusing new work"}
        </div>
      </div>
    {:else}
      <div class="px-0 pt-3 pb-2 text-center">
        <div class="mx-auto grid h-[46px] w-[46px] place-items-center rounded-full" style="background: color-mix(in srgb, var(--accent-bright) 14%, transparent);">
          <span class="inline-grid" style="color: var(--accent-bright);"><Icon name="check" size={22} /></span>
        </div>
        <div class="mt-4 text-lg font-bold tracking-[-.02em]">Drained — node stopped</div>
        <div class="mt-1.5 text-sm" style="color: var(--ink-code-3);">No work was killed. The mesh sees this machine as gone until you start it again.</div>
        <div class="mt-[22px] grid grid-cols-2 gap-2.5">
          <button class="btn btn-lg border border-edge-code font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]" onclick={() => (drainOpen = false)}>Stay stopped</button>
          <button class="btn btn-bright btn-lg" disabled={restartBusy} onclick={restart}>{restartBusy ? "Starting…" : "Start it again"}</button>
        </div>
      </div>
    {/if}
  </div>
</Modal>

<style>
  /* -- the reclaim ladder (Storage) ---------------------------------------
     A choice between four levels, so each level is a card and the chosen
     one is unmistakable: accent hairline, accent tint, a filled dot with a
     ring around it. The old treatment reached for a token that does not
     exist (`--surface-sel`), which left the selected rung indistinguishable
     from the other three at a glance — the one thing a radio group must
     never be. Sizes are the app's own: `--radius-md` controls, hairlines
     drawn as inset shadows so nothing shifts by a pixel when it is chosen. */
  .policy-ladder {
    display: grid;
    gap: 8px;
  }
  .policy-opt {
    display: flex;
    align-items: flex-start;
    gap: 11px;
    width: 100%;
    padding: 11px 13px;
    text-align: left;
    cursor: pointer;
    border-radius: var(--radius-md);
    background: var(--surface-subtle);
    box-shadow: inset 0 0 0 1px var(--edge);
    transition:
      background-color var(--dur-fast) var(--ease),
      box-shadow var(--dur-fast) var(--ease);
  }
  .policy-opt:hover:not(:disabled) {
    background: var(--surface-2);
    box-shadow: inset 0 0 0 1px var(--edge-strong);
  }
  .policy-opt:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .policy-opt.is-on,
  .policy-opt.is-on:hover:not(:disabled) {
    background: var(--accent-wash-2);
    box-shadow:
      inset 0 0 0 1.5px var(--accent),
      var(--sh-control);
  }

  /* The dot: an empty ring at rest, an accent disc with a halo when
     chosen. Drawn rather than an <input type=radio>, for the reason the
     `.check` in app.css is drawn — three toolkits, three answers. */
  .policy-dot {
    position: relative;
    flex: none;
    width: 15px;
    height: 15px;
    margin-top: 2px;
    border-radius: 999px;
    box-shadow: inset 0 0 0 1.5px var(--ink-6);
    transition:
      box-shadow var(--dur-fast) var(--ease),
      background-color var(--dur-fast) var(--ease);
  }
  .policy-opt:hover:not(:disabled) .policy-dot {
    box-shadow: inset 0 0 0 1.5px var(--ink-5);
  }
  .policy-opt.is-on .policy-dot {
    background: var(--accent-solid);
    box-shadow: inset 0 0 0 1.5px var(--accent-solid);
  }
  .policy-opt.is-on .policy-dot::after {
    content: "";
    position: absolute;
    inset: 4px;
    border-radius: 999px;
    background: var(--accent-on-solid);
  }

  /* How long this rung keeps things, right-aligned so four rungs can be
     compared down a column instead of read as four sentences. */
  .policy-when {
    flex: none;
    align-self: center;
    min-width: 54px;
    text-align: right;
    font-size: var(--text-xs);
    font-weight: 600;
    color: var(--ink-5);
  }
  .policy-opt.is-on .policy-when {
    color: var(--accent-ink);
  }
</style>
