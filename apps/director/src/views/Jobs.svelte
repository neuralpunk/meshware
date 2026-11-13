<script lang="ts">
  import { rowAction } from "../lib/rowaction";
  import { hueOf, hueStyle } from "../lib/hues";
  // Jobs — the screen an operator spends the most time on, and the one the
  // 2026-08 review found furthest from where it should be (§7):
  //
  //   · No duration. For a job runner "how long did it take" is arguably the
  //     primary fact, and it appeared nowhere. AGE tells you when it started.
  //   · No path to failures. The first question on a busy mesh is always
  //     "what broke", and the only answer on offer was scrolling.
  //   · Constant columns. Every row said `succeeded`, every row said the same
  //     username — 40% of the table's width spent on captions.
  //   · Ambiguous fan-out grouping. A group header followed by its members and
  //     then by ten unrelated rows reads as a twelve-member group; a container
  //     that never visibly ends swallows everything below it.
  import {
    api,
    errText,
    fmtBytes,
    type JobExplainView,
    type JobView,
    type JobsPageView,
    type HistoryJobView,
    type ResultSetView,
    type RunEvent,
    type TierView,
  } from "../lib/api";
  import { noOverride } from "../lib/api";
  import { app, ago, ui } from "../lib/state.svelte";
  import Dot from "../lib/components/Dot.svelte";
  import Empty from "../lib/components/Empty.svelte";
  import LoadError from "../lib/components/LoadError.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import PlacementChart from "../lib/components/PlacementChart.svelte";
  import Why from "../lib/components/Why.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import AnswersModal from "../lib/components/AnswersModal.svelte";
  import StateBadge from "../lib/components/StateBadge.svelte";
  import { popMenu, staggerIn, tabline } from "../lib/motion";
  import { LineRing, OUTPUT_MAX_LINES } from "../lib/lines";
  import { chrome } from "../lib/chrome/chrome.svelte";

  let selectedId = $state<string | null>(null);
  /**
   * A job's output, as a bounded ring of lines (**H-10**).
   *
   * This was one string that every chunk re-created — `logText += ev.text`
   * — so following a chatty job cost O(log size) copying per chunk, kept
   * the entire output resident, and handed the whole `<pre>` back to the
   * DOM each time. A job printing megabytes visibly stuttered the window.
   *
   * The last [`OUTPUT_MAX_LINES`] lines is what a person can actually read;
   * everything before that is what the *warehouse* is for, and the pane
   * says so when it has dropped some. The ring is `lib/lines.ts`, shared
   * with Launch since DIR-9.
   */
  const logRing = new LineRing();
  let logDropped = $state(0);
  let logText = $state("");
  let logDone = $state<string | null>(null);
  let logErr = $state<string | null>(null);
  let followingId: string | null = null;

  // 1s heartbeat so a running job's duration counts up rather than sitting.
  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });


  // -- the empty states, made pressable (L1 P4) -------------------------------
  //
  // Both of this screen's empty states used to end in a link to the Run
  // view — which is a correct instruction and still leaves a stranger with
  // a form to fill in before anything happens. These run the one command
  // that fills the view, on every machine, and say what that costs before
  // they are pressed: `uname -a` reads a string the kernel already holds.
  // Nothing is installed, nothing is written, nothing is changed.
  let starting = $state(false);
  /** Why the last press of an empty state's button produced nothing. Shown
   *  in the empty state itself rather than flashed: a toast that has already
   *  faded is indistinguishable from the button not working, which is the
   *  exact complaint this answers. */
  let startErr = $state<string | null>(null);
  const machines = $derived(app.nodes.filter((n) => n.liveness === "alive").length);

  /**
   * Run one command on every machine, from an empty state.
   *
   * `run_command` **returns as soon as the run is dispatched** — the work
   * streams back down the channel afterwards — so this has to follow the
   * stream to completion rather than treating the call's resolution as the
   * end. Two bugs lived in that gap:
   *
   *   1. The Answers tab never reloaded. `app.refresh()` refreshes the job
   *      list; the result sets behind this tab are fetched separately by
   *      `loadSets()`, and nothing called it. Pressing the button ran the
   *      command, collected the rows, and left the "no run has been asked a
   *      question yet" card sitting there — which reads exactly like a
   *      button that does nothing.
   *   2. A refusal was silent. Every frame was dropped on the floor, so a
   *      `policyDenied` or `failed` — the two most likely outcomes of a
   *      fan-out to every machine on a mesh with change control on — also
   *      produced no visible change at all.
   */
  async function fanOut(command: string, capture: boolean) {
    if (starting) return;
    starting = true;
    startErr = null;
    // Resolved by the stream, not by the dispatch call. Guarded by a
    // timeout so a run whose final frame never arrives releases the button
    // instead of pinning it on "Working…" forever.
    let settle: () => void = () => {};
    const finished = new Promise<void>((resolve) => (settle = resolve));
    const guard = setTimeout(settle, 120_000);
    try {
      await api.runCommand(
        {
          command,
          env: [],
          on: null,
          fanout: 0,
          timeoutSecs: null,
          priority: "normal",
          maxParallel: null,
          haltOnFail: null,
          ...noOverride,
          // `captureOutputs` is `--json`: it is what turns a fan-out into
          // the rows the Answers tab reads. `--expect` would assert over
          // them, which is a different question and needs an expression.
          captureOutputs: capture,
          outputs: null,
          expect: null,
          gang: null,
          gangPlacement: null,
          gangRestart: false,
          gpus: null,
          vramMb: null,
          device: null,
          gpuShare: false,
        },
        (ev) => {
          if (ev.type === "fanoutDone" || ev.type === "done") settle();
          else if (ev.type === "failed") {
            startErr = ev.message;
            settle();
          }
        },
      );
      await finished;
    } catch (e) {
      startErr = errText(e);
    } finally {
      clearTimeout(guard);
      starting = false;
      // Both tabs, always: the rows this was pressed for live behind
      // `loadSets`, and the job it dispatched lives behind `refresh`.
      setsLoaded = false;
      await Promise.all([app.refresh(), loadSets()]);
      if (!startErr && capture && sets.length === 0) {
        startErr =
          "The run finished but answered with no rows — the mesh may have had no machine free to take it. Look under Jobs for what happened to it.";
      }
    }
  }

  // -- why did it land there, and did it run at all (C8 P2, C7 P5) ------------
  //
  // Fetched when a job is selected rather than with the list: a breakdown
  // per row would be a hundred round trips to draw a table nobody has
  // clicked into yet.
  let explain = $state<JobExplainView | null>(null);
  let explainErr = $state<string | null>(null);
  let showKey = $state(false);
  type DetailTab = "output" | "placement" | "key";
  let detailTab = $state<DetailTab>("output");
  // The key tab only exists for work that has an action key — an ad-hoc
  // command is not hermetic and never gets one, and an empty tab is worse
  // than an absent one.
  const detailTabs = $derived(
    [
      { id: "output" as const, label: "Output" },
      { id: "placement" as const, label: "Why it landed there" },
      ...(explain?.actionKey ? [{ id: "key" as const, label: "Action key" }] : []),
    ],
  );
  $effect(() => {
    const id = selectedId;
    explain = null;
    explainErr = null;
    showKey = false;
    // Output first for a new job: what it printed is what you came for.
    detailTab = "output";
    if (!id) return;
    void (async () => {
      try {
        const view = await api.jobExplain(id);
        // Discard a late answer for a job the operator has already clicked
        // away from, or the panel shows the previous job's placement.
        if (selectedId === id) explain = view;
      } catch (e) {
        if (selectedId === id) explainErr = errText(e);
      }
    })();
  });

  // The one-line version of the placement chart, for the metadata strip.
  //
  // The chart itself sits below a 400px log rail, which is far enough down
  // that people who were told the feature exists still could not find it.
  // "Why did it land there" is a question you have *when you open the job*,
  // so the answer belongs where your eye already is; the chart stays below
  // for when the one-liner is not enough.
  const placedSummary = $derived.by((): string | null => {
    if (!explain) return null;
    if (explain.cacheHit) return "nothing was placed — served from the cache";
    const p = explain.placement;
    if (!p) {
      return explain.placementOn
        ? `breakdown is on ${explain.placementOn}`
        : null;
    }
    const n = p.considered;
    const of = `${n} candidate${n === 1 ? "" : "s"}`;
    // A single candidate has no runner-up, so there is nothing that "won"
    // — saying so would invent a contest that did not happen.
    if (p.spilled) return `moved to ${explain.executorName}, the first choice was busy · ${of}`;
    // The shape a GPU job takes on a mesh with one GPU box: filtering
    // happens before scoring, so there is nothing to compare against and
    // no contest to report.
    if (n <= 1) return "the only candidate";
    return p.decisive ? `won on ${p.decisive} · ${of}` : `best of ${of}`;
  });

  /**
   * Plain words for the summary's jargon (L2 P4).
   *
   * The engine's sentence, not this file's. It used to be two hand-written
   * clauses here — a second explanation of the same placement, in different
   * words from the CLI's, which is precisely the split L2 P4 exists to
   * close.
   */
  const placedGloss = $derived(explain?.placement?.why ?? null);

  // -- filter ------------------------------------------------------------------
  //
  // **The filter runs on the engine, not here** (H-1). This screen used to
  // hold every job the mesh remembered and slice it four ways per poll;
  // the shared `app.jobs` is now the newest few hundred, which would have
  // made "Failed" mean "failed, among the most recent" — a tab that lies
  // quietly. So the tab and the page are a query, and the counters come
  // from the engine's own single pass over the whole table.
  type Filter = "all" | "running" | "failed" | "succeeded";
  let filter = $state<Filter>("all");
  /** How many rows one page of this screen holds. */
  const PAGE = 200;
  let offset = $state(0);
  let page = $state<JobsPageView | null>(null);
  let pageErr = $state<string | null>(null);

  const isFailure = (j: JobView) => j.state === "failed" || j.state === "timed-out";
  const isLive = (j: JobView) => j.state === "running" || j.state === "queued";

  /** The engine's word for the tab. `failed` covers timed-out and
      cancelled too — the tab means "did not succeed", and three tabs for
      three ways of not succeeding is not what anyone is asking. */
  const queryState = (f: Filter): string | null =>
    f === "all" ? null : f === "running" ? "live" : f === "failed" ? "failed" : "succeeded";

  const counts = $derived({
    all: app.jobsCounts.all,
    running: app.jobsCounts.running + app.jobsCounts.queued,
    failed: app.jobsCounts.failed,
    succeeded: app.jobsCounts.succeeded,
  });

  /**
   * Fetch this screen's page.
   *
   * Rides `app.lastSync` rather than a timer of its own: the core tick
   * already pauses on a hidden window and catches up on return, and a
   * second interval here would defeat that (M-24). `inFlight` keeps a slow
   * engine from stacking requests.
   */
  let inFlight = false;
  $effect(() => {
    const want = queryState(filter);
    const at = offset;
    // Read the tick so this re-runs with it.
    void app.lastSync;
    if (app.engineStopped) return;
    if (inFlight) return;
    inFlight = true;
    void api
      .jobs({ filter: want, limit: PAGE, offset: at })
      .then((p) => {
        page = p;
        pageErr = null;
      })
      .catch((e) => {
        pageErr = errText(e);
      })
      .finally(() => {
        inFlight = false;
      });
  });

  // Changing tab starts at the top of the new list; a stale offset would
  // land on "showing 400–600 of 12".
  $effect(() => {
    void filter;
    offset = 0;
  });

  /** The page, or the shared table until the first page lands. */
  const visible = $derived(
    page ? page.rows : filter === "all" ? app.jobs : app.jobs.filter(matches),
  );
  const matched = $derived(page ? page.total : visible.length);
  const hasMore = $derived(offset + visible.length < matched);

  function matches(j: JobView) {
    return filter === "all"
      ? true
      : filter === "running"
        ? isLive(j)
        : filter === "failed"
          ? isFailure(j)
          : j.state === "succeeded";
  }

  /**
   * The row the detail rail is pinned to.
   *
   * Looked up in the page, then in the shared table, then in the copy kept
   * from when it was selected — a bounded page means the row you are
   * reading can page out from under you, and a detail rail that empties
   * itself mid-read is worse than one showing a row a few seconds old.
   */
  let selectedRow = $state<JobView | null>(null);
  const selected = $derived(
    visible.find((j) => j.jobId === selectedId) ??
      app.jobs.find((j) => j.jobId === selectedId) ??
      (selectedRow?.jobId === selectedId ? selectedRow : null),
  );
  $effect(() => {
    if (selected) selectedRow = selected;
  });

  /** A column whose every value is identical is not a column, it is a
      caption. Show it once above the table and take the width back. */
  const submitters = $derived(new Set(visible.map((j) => j.submitterUsername ?? "—")));
  const oneSubmitter = $derived(visible.length > 1 && submitters.size === 1);

  // -- duration ----------------------------------------------------------------
  function durMs(j: JobView): number {
    const end = j.finishedAtMicros ?? now * 1000;
    return Math.max(0, (end - j.createdAtMicros) / 1000);
  }
  function fmtDur(j: JobView): string {
    const ms = durMs(j);
    if (ms < 1000) return `${Math.round(ms)}ms`;
    const s = ms / 1000;
    if (s < 60) return `${s.toFixed(1)}s`;
    const m = Math.floor(s / 60);
    if (m < 60) return `${m}m ${Math.round(s % 60)}s`;
    return `${Math.floor(m / 60)}h ${m % 60}m`;
  }
  /** The outlier is the point: the longest run in view is the one worth
      seeing, so it is the only duration that gets full ink. */
  const slowest = $derived(Math.max(0, ...visible.filter((j) => !isLive(j)).map(durMs)));

  // -- rows, with fan-out groups that visibly close --------------------------
  type Row = {
    job: JobView;
    groupHead: boolean;
    groupSize: number;
    inGroup: boolean;
    lastInGroup: boolean;
    groupState?: { done: number; failed: number; total: number };
  };
  /**
   * Rows, with fan-out groups that visibly close.
   *
   * **Two passes, not one per group** (H-9). This used to re-scan the whole
   * visible list for every group head it met — and then scan each group
   * three more times to tally it — so a screen of fan-outs cost O(n ×
   * groups) on every poll. Bucketing once by `fanGroup` and reading the
   * buckets is the same output for O(n).
   */
  const rows = $derived.by((): Row[] => {
    const groups = new Map<string, { members: JobView[]; done: number; failed: number }>();
    for (const j of visible) {
      if (!j.fanGroup) continue;
      let g = groups.get(j.fanGroup);
      if (!g) {
        g = { members: [], done: 0, failed: 0 };
        groups.set(j.fanGroup, g);
      }
      g.members.push(j);
      // Tallied while bucketing, so the group header costs nothing extra.
      if (!isLive(j)) g.done += 1;
      if (isFailure(j)) g.failed += 1;
    }
    const out: Row[] = [];
    const seen = new Set<string>();
    for (const j of visible) {
      if (!j.fanGroup) {
        out.push({ job: j, groupHead: false, groupSize: 0, inGroup: false, lastInGroup: false });
        continue;
      }
      if (seen.has(j.fanGroup)) continue;
      seen.add(j.fanGroup);
      const g = groups.get(j.fanGroup)!;
      out.push({
        job: j,
        groupHead: true,
        groupSize: g.members.length,
        inGroup: false,
        lastInGroup: false,
        groupState: { done: g.done, failed: g.failed, total: g.members.length },
      });
      g.members.forEach((m, i) =>
        out.push({
          job: m,
          groupHead: false,
          groupSize: 0,
          inGroup: true,
          lastInGroup: i === g.members.length - 1,
        }),
      );
    }
    return out;
  });

  $effect(() => {
    // Depend on the job list, explicitly and unconditionally. A deep link
    // that arrives while this screen is *already* open (L2 P5) has nothing
    // else to wake this effect with — `sessionStorage` is not reactive and
    // `app.go("jobs")` does not remount a view you are already on — and the
    // two-second poll replacing this array is the tick that does.
    const list = app.jobs;
    const pre = sessionStorage.getItem("jobs:select");
    if (pre) {
      sessionStorage.removeItem("jobs:select");
      select(pre);
    }
    // A `director://fanouts/fan-…` link (L2 P5). The group is a header
    // row, not a selectable object, so the link lands on its first target
    // — the screen a person opening that link came to read. Held until the
    // job list has arrived rather than dropped: a link acted on before the
    // first poll would silently do nothing.
    const fan = sessionStorage.getItem("jobs:fanout");
    if (fan) {
      const first = list.find((j) => j.fanGroup === fan);
      if (first) {
        sessionStorage.removeItem("jobs:fanout");
        filter = "all";
        select(first.jobId);
      }
    }
    // Mesh's "see what failed" vital lands here already filtered.
    const f = sessionStorage.getItem("jobs:filter");
    if (f) {
      sessionStorage.removeItem("jobs:filter");
      filter = f as Filter;
    }
    // Loaded once on arrival rather than on tab switch, so the "answers →"
    // link can appear on the fan-out groups that actually have some.
    if (!setsLoaded) void loadSets();
  });

  /** Append a chunk to the ring, keeping only the last window of lines. */
  function pushChunk(text: string) {
    logRing.push(text);
    logText = logRing.text;
    logDropped = logRing.dropped;
  }

  function select(id: string) {
    // Whatever we were following, stop it *at the engine* (**H-10**).
    // Dropping the frames in JavaScript — which is all this did — left the
    // stream running on the daemon for the life of the window: click three
    // jobs and three log tails kept flowing, two of them into nothing.
    stopFollowing();
    if (selectedId === id) {
      selectedId = null;
      return;
    }
    selectedId = id;
    logRing.reset();
    logDropped = 0;
    logText = "";
    logDone = null;
    logErr = null;
    followingId = id;
    void api
      .jobLogs(id, true, (ev: RunEvent) => {
        if (followingId !== id) return;
        if (ev.type === "chunk") pushChunk(ev.text);
        else if (ev.type === "done") logDone = `${ev.state}${ev.exitCode !== null ? ` · exit ${ev.exitCode}` : ""}`;
        else if (ev.type === "failed") logErr = ev.message;
      })
      .catch((e) => (logErr = errText(e)));
  }

  /** Tell the engine to stop sending, and stop listening. */
  function stopFollowing() {
    const was = followingId;
    followingId = null;
    if (was) void api.jobLogsStop(was).catch(() => {});
  }

  // Leaving the screen with a follow open is the same leak as switching
  // rows, and this file had no teardown at all.
  $effect(() => () => stopFollowing());

  /**
   * Keep the pane pinned to the bottom while the reader is at the bottom.
   *
   * The pane scrolled nowhere, so new output from a running job landed
   * off-screen below the fold while the reader watched the top of it
   * (**H-10**). Sticky rather than forced: scroll up to read something and
   * the pane stops chasing, which is the behaviour every terminal has.
   */
  let logEl = $state<HTMLElement | null>(null);
  let logPinned = true;
  $effect(() => {
    // Depend on the text, so this runs after each chunk lands.
    void logText;
    const el = logEl;
    if (!el || !logPinned) return;
    el.scrollTop = el.scrollHeight;
  });
  function onLogScroll() {
    const el = logEl;
    if (!el) return;
    logPinned = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
  }

  async function cancel(job: JobView) {
    try {
      // The daemon says which of the two things happened — a cancel the
      // executor will report, or a forced record for an executor that
      // could not be told — and that distinction belongs in the flash.
      const msg = await api.jobCancel(job.jobId);
      app.flash(msg || `cancel requested for ${job.shortId}`);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  /** The detail rail takes 440px; the table folds its two softest columns
      rather than crushing the command it exists to show. */
  const compact = $derived(selected !== null);
  /**
   * The width the list and its detail pane share, and whether it is too
   * little for both (UX-11). At the 980px minimum window the pane left the
   * table 228px for 342px of fixed columns, and "Command" and "Executor"
   * printed on top of each other. Tight, the pane gives up 100px and the
   * table keeps State and Command — the executor and the time are the
   * first two lines of the pane beside it.
   */
  let listW = $state(0);
  const tight = $derived(compact && listW > 0 && listW < 1000);
  const paneW = $derived(tight ? 340 : 440);
  // State · Command · Executor · Took, plus User and Started when there is
  // room. It used to count three, which left the fan-out spine's tinted
  // band one cell short of the table's right edge.
  const cols = $derived(
    2 + (tight ? 0 : 2) + (compact ? 0 : 1) + (!oneSubmitter && !compact ? 1 : 0),
  );

  // -- answers (M19 §5.7 P6) ---------------------------------------------------
  //
  // The rows a fan-out produced, still queryable after the run window
  // closed. They live in *this* node's history — §5.3's honest answer —
  // so the list is "what this peer launched", and a group launched
  // elsewhere refuses by naming its owner rather than reporting zero rows.
  type Tab = "jobs" | "answers" | "history";
  let tab = $state<Tab>("jobs");
  let sets = $state<ResultSetView[]>([]);
  let setsErr = $state<string | null>(null);
  let setsLoaded = $state(false);
  let openGroup = $state<string | null>(null);

  async function loadSets() {
    try {
      sets = await api.resultSets(50);
      setsErr = null;
    } catch (e) {
      setsErr = errText(e);
    } finally {
      setsLoaded = true;
    }
  }

  function showAnswers(group: string | null) {
    tab = "answers";
    openGroup = group;
    void loadSets();
  }

  /** Which fan groups actually left rows behind — the only ones worth a link. */
  const answered = $derived(new Set(sets.map((s) => s.group)));

  const setStamp = (s: ResultSetView) => ago(s.createdAtMicros);


  // -- history: one list across both tiers (M31 B3 §14.2) --------------------
  //
  // The Jobs tab above is *live* state — what the mesh is holding right
  // now. This is the browse: `history.redb` for the recent rows, Parquet
  // for everything older, and one list rather than two screens. "Instant
  // and recent" versus "complete and slower" is a property of a *result*,
  // not a reason to make somebody guess which tier holds the job they are
  // hunting.
  //
  // **It never asks a sink.** The list, its filters and its counts come out
  // of tiers this mesh owns; a destination is read on one person's click on
  // one payload and nowhere else. That is §18's fence, and the engine test
  // that guards it asserts this page is byte-identical with every sink
  // unreachable.
  let histRows = $state<HistoryJobView[]>([]);
  let histTiers = $state<TierView | null>(null);
  let histTotal = $state<number | null>(null);
  let histErr = $state<string | null>(null);
  let histLoaded = $state(false);
  let histBusy = $state(false);
  let histFind = $state("");
  let histWindow = $state<"30" | "180" | "all">("30");
  const HIST_WINDOWS: [typeof histWindow, string][] = [
    ["30", "Last 30 days"],
    ["180", "Last 6 months"],
    ["all", "Everything kept"],
  ];

  async function loadHistory() {
    histBusy = true;
    try {
      const since =
        histWindow === "all"
          ? undefined
          : String(Math.round(Date.now() * 1000 - Number(histWindow) * 86_400e6));
      const page = await api.historyJobs({
        contains: histFind.trim() || undefined,
        sinceMicros: since,
        limit: 100,
      });
      histRows = page.rows;
      histTiers = page.tiers;
      histTotal = page.total;
      histErr = null;
    } catch (e) {
      histErr = errText(e);
    } finally {
      histBusy = false;
      histLoaded = true;
    }
  }

  // -- the payload panel (§14.3) ---------------------------------------------
  //
  // Opens **at the tail**, because the reason anyone opens a six-month-old
  // log is the thing that went wrong and it is at the end. Scrolls
  // backwards a window at a time; nothing here ever holds the whole object,
  // which for the job this phase is named after is 5 GB.
  //
  // The daemon audits the *open* — the tail read — and nothing else, so
  // "earlier" and "search" cost no audit entries. A failed read fills this
  // panel with the destination's own words and leaves the list alone.
  let openJob = $state<HistoryJobView | null>(null);
  let payLines = $state<string[]>([]);
  let payMore = $state(false);
  let payStatus = $state("");
  let payBusy = $state(false);
  let paySearch = $state("");
  let payFrom = $state(0);

  async function fetchWindow(fromEnd: number, append: boolean) {
    if (!openJob) return;
    payBusy = true;
    try {
      const w = await api.historyReplay(openJob.job, String(fromEnd), 500, paySearch.trim());
      payLines = append ? [...w.lines, ...payLines] : w.lines;
      payMore = w.moreBefore;
      payStatus = w.status;
      payFrom = fromEnd + w.lines.length;
    } catch (e) {
      // Degrades this panel, never the screen: the list behind it is still
      // right, and the record still says what happened.
      payStatus = errText(e);
      if (!append) payLines = [];
    } finally {
      payBusy = false;
    }
  }

  function openPayload(row: HistoryJobView) {
    openJob = row;
    payLines = [];
    payMore = false;
    payStatus = "";
    paySearch = "";
    payFrom = 0;
    void fetchWindow(0, false);
  }

  /** Re-open at the tail with the new needle — a search is a new read, not
   *  a filter over what is already on screen. */
  async function researchPayload() {
    payLines = [];
    payFrom = 0;
    await fetchWindow(0, false);
  }

  /** The list gives up two columns when the payload panel takes 520px —
   *  the same trade the Jobs table above makes, and for the same reason: a
   *  fixed table in a narrowed column overlaps its own headings. */
  const histCompact = $derived(openJob !== null);
  /** The same trade as `tight`, for the payload panel (UX-11). */
  let histW = $state(0);
  const histTight = $derived(histCompact && histW > 0 && histW < 1100);

  const histStamp = (row: HistoryJobView) =>
    new Date(Number(row.finishedAtMicros ?? row.createdAtMicros) / 1000).toLocaleString(
      undefined,
      histCompact
        ? { dateStyle: "medium" }
        : { dateStyle: "medium", timeStyle: "short" },
    );

  const FILTERS: [Filter, string][] = [
    ["all", "All"],
    ["running", "Running"],
    ["failed", "Failed"],
    ["succeeded", "Succeeded"],
  ];

  // -- clearing the history --------------------------------------------------
  //
  // Mesh-wide and permanent, which is the whole of why it is behind a
  // modal rather than an undo: there is nothing to restore afterwards, and
  // the rows are gone for everyone rather than hidden on this machine.
  //
  // Deliberately counted over `app.jobs` and not `visible`: the button
  // clears the history, not the filter, and a "Clear" sitting under an
  // active Failed tab that quietly took the succeeded rows too is the
  // exact surprise this screen cannot afford.
  const isAdmin = $derived(app.whoami?.isFounder || app.whoami?.roles.includes("admin"));
  const finished = $derived(
    app.jobsCounts.all - app.jobsCounts.running - app.jobsCounts.queued,
  );
  let clearOpen = $state(false);
  let clearBusy = $state(false);
  let clearErr = $state<string | null>(null);

  async function doClear() {
    clearBusy = true;
    clearErr = null;
    try {
      const message = await api.jobsClear();
      clearOpen = false;
      // The detail rail may be pinned to a row that no longer exists, and
      // Failed/Succeeded are now empty tabs the seg would disable while
      // leaving selected — so land back on the only filter with anything
      // behind it.
      selectedId = null;
      filter = "all";
      app.flash(message);
      await app.refresh();
    } catch (e) {
      clearErr = errText(e);
    } finally {
      clearBusy = false;
    }
  }

  // The toolbar slot is where each screen's main action goes (2026-09),
  // and this screen's used to be "Clear history" — the one button on it
  // that forgets everything, in the place a person reaches first (UX-17).
  // The main action is starting a run; clearing is behind "More", and
  // still behind its confirmation.
  $effect(() => chrome.setActions(actions));
  let moreOpen = $state(false);
</script>

<svelte:window
  onpointerdown={(e) => {
    if (moreOpen && !(e.target as Element | null)?.closest?.("[data-menu='activity-more']")) moreOpen = false;
  }}
  onkeydown={(e) => {
    if (moreOpen && e.key === "Escape") {
      e.preventDefault();
      moreOpen = false;
    }
  }}
/>

{#snippet actions()}
  <button class="btn btn-primary btn-md" onclick={() => app.go("run")}>
    <Icon name="plus" size={13} /> New run
  </button>
  <div class="relative" data-menu="activity-more">
    <button
      class="btn btn-secondary btn-md w-8 px-0"
      title="More"
      aria-label="More actions"
      aria-haspopup="menu"
      aria-expanded={moreOpen}
      onclick={() => (moreOpen = !moreOpen)}
    >
      <Icon name="more-h" size={14} />
    </button>
    {#if moreOpen}
      <div
        class="absolute top-[38px] right-0 z-40 w-[248px] overflow-hidden rounded-lg bg-surface py-1.5"
        style="box-shadow: var(--sh-pop); transform-origin: calc(100% - 14px) 0;"
        role="menu"
        tabindex="-1"
        use:popMenu
      >
        <button
          class="more-item"
          role="menuitem"
          disabled={finished === 0 || !isAdmin || app.engineStopped}
          title={!isAdmin
            ? "clearing job history needs the admin role"
            : finished === 0
              ? "nothing has finished yet"
              : "Forget every job that has already finished, mesh-wide"}
          onclick={() => {
            moreOpen = false;
            clearErr = null;
            clearOpen = true;
          }}
        >
          <Icon name="trash" size={13} /> Clear finished jobs…
        </button>
      </div>
    {/if}
  </div>
{/snippet}

<div class="fade-in">
  <!-- Two questions, two surfaces: "what ran" and "what did it answer".
       They are the same runs seen from opposite ends, and M19 exists
       because only the first one had a screen. -->
  <div class="tabrow mb-4" role="tablist" style="border-bottom: 1px solid var(--edge);" use:tabline>
    {#each [["jobs", "Jobs", counts.all], ["answers", "Answers", setsLoaded ? sets.length : null], ["history", "History", null]] as [id, label, n] (id)}
      {@const count = typeof n === "number" ? n : null}
      <button
        class="tab"
        role="tab"
        aria-selected={tab === id}
        onclick={() => {
          if (id === "answers") showAnswers(null);
          else if (id === "history") {
            tab = "history";
            if (!histLoaded) void loadHistory();
          } else tab = "jobs";
        }}
      >
        {label}
        <!-- The count in the tab, so "is there anything behind this"
             is answered without the click. -->
        {#if count != null && count > 0}<span class="n">{count}</span>{/if}
      </button>
    {/each}
  </div>

  {#if tab === "answers"}
    {#if setsErr}
      <LoadError what="the collected answers" error={setsErr} retry={loadSets} />
    {:else if !setsLoaded}
      <div class="card p-12 text-center text-sm text-ink-5">
        Loading…
      </div>
    {:else if sets.length === 0}
      <Empty
        title="No run has been asked a question yet"
        command="meshnode run 'df -h /' --fanout all --json"
        reassurance={`Collects one row per machine — ${machines || "every"} of them · nothing is changed`}
        action={{ label: "Ask every machine how much disk is free", run: () => fanOut("df -h /", true) }}
        busy={starting}
        error={startErr}
      >
        Rows exist only for a run that asked for them. Tick
        <span class="font-semibold text-ink-3">Collect an answer from each node</span>
        in the
        <button class="linkish" onclick={() => app.go("run")}>New run page</button>
        — for a command or a script — and every target's stdout, exit code and declared
        outputs come back as one table you can read as JSON.
      </Empty>
    {:else}
      <div class="mb-3 text-sm text-ink-5">
        Every run that asked its targets a question, one row per machine. Open a run to read the answers side by side.
      </div>
      <div class="flex flex-col gap-3">
        {#each sets as set (set.group)}
          <div class="card overflow-hidden">
            <button
              class="flex w-full items-center gap-3 px-5 py-3 text-left transition-colors hover:bg-surface-2"
              aria-haspopup="dialog"
              onclick={() => (openGroup = set.group)}
            >
              <span
                class="grid h-6 w-6 flex-none place-items-center rounded-md"
                style="background: var(--hue-sky-wash); color: var(--hue-sky-ink);"
                aria-hidden="true"><Icon name="cat-list" size={13} /></span
              >
              <span class="mono text-xs font-semibold" style="color: var(--hue-ink);">
                {set.group.slice(0, 14)}
              </span>
              <span class="mono min-w-0 flex-1 truncate text-xs text-ink-3">{set.command}</span>
              <span class="num text-xs text-ink-4">{set.rows.length} rows</span>
              {#if set.failed > 0}
                <span class="num text-xs font-semibold" style="color: var(--status-error-ink);">
                  {set.failed} failed
                </span>
              {/if}
              {#if set.missing > 0}
                <span class="num text-xs text-ink-5" title="targets in the plan that never reported">
                  {set.missing} silent
                </span>
              {/if}
              <span class="num text-xs text-ink-5">{setStamp(set)}</span>
            </button>
          </div>
        {/each}
      </div>
      <!-- The set, in a dialog rather than under its row: a stored set can be
           a thousand rows, and expanded inline it pushed every other set off
           the bottom of the page. -->
      {#each sets.filter((s) => s.group === openGroup) as set (set.group)}
        <AnswersModal
          open
          rows={set.rows}
          source={set.group}
          caption={`${set.rows.length.toLocaleString()} of ${(set.rows.length + set.missing).toLocaleString()} targets answered · owned by ${set.owner}`}
          onclose={() => (openGroup = null)}
        />
      {/each}
    {/if}
  {:else if tab === "history"}
    <!-- One list across both tiers. The row says which one answered it;
         nothing on this page asks a destination anything. -->
    <div class="mb-3.5 flex flex-wrap items-center gap-3">
      <div class="seg" role="tablist">
        {#each HIST_WINDOWS as [id, label] (id)}
          <button
            role="tab"
            aria-selected={histWindow === id}
            onclick={() => {
              histWindow = id;
              void loadHistory();
            }}
          >
            {label}
          </button>
        {/each}
      </div>
      <input
        class="input w-64"
        placeholder="Find in command…"
        bind:value={histFind}
        onkeydown={(e) => e.key === "Enter" && loadHistory()}
      />
      <button class={ui.rowAction} disabled={histBusy} onclick={() => loadHistory()}>
        {histBusy ? "Searching…" : "Search"}
      </button>
      {#if histTotal !== null}
        <span class="text-sm text-ink-5">
          {histTotal}
          {histTotal === 1 ? "match" : "matches"}
        </span>
      {/if}
    </div>

    {#if histErr}
      <LoadError class="mb-3" what="the job history" error={histErr} retry={loadHistory} />
    {/if}

    <!-- Which tier answered, said once above the list rather than guessed
         at from the rows. The slow tier is *visibly* slower, not silently
         absent — the screen that pretends otherwise is the screen that
         hangs. -->
    {#if histTiers}
      <div class="mb-3 text-sm text-ink-5">
        {#if histTiers.fromArchive > 0}
          <span class="text-ink-4">{histTiers.fromIndex}</span> from this peer's index ·
          <span class="text-ink-4">{histTiers.fromArchive}</span> from the archive{histTiers.archivePeer
            ? ` on ${histTiers.archivePeer}`
            : ""}
        {:else}
          <span class="text-ink-4">{histTiers.fromIndex}</span> from this peer's index
        {/if}
        {#if histTiers.archiveUnreachable}
          · <span style="color: var(--status-warn-ink);"
            >no archive answered{histTiers.archiveStatus ? `: ${histTiers.archiveStatus}` : ""} —
            older rows exist nowhere this browse can see</span
          >
        {/if}
      </div>
    {/if}

    <!-- "We threw it away on the schedule you set" and "nothing matched"
         must never look alike. -->
    {#if histTiers?.windowReachedPast}
      <div class="card mb-3 px-5 py-3 text-sm text-ink-5">
        Rows older than
        <span class="text-ink-4">
          {histTiers.retainedFromMicros
            ? new Date(Number(histTiers.retainedFromMicros) / 1000).toLocaleDateString()
            : "the retention window"}
        </span>
        are not in this mesh any more — the retention window dropped them. This is a known
        absence, not an empty result.
        <button
          class="linkish ml-1"
          onclick={() => {
            // Land on the pane that answers this, not at the top of
            // Settings with seven other things on it.
            app.go("settings", "history");
          }}>Change how long</button
        >
      </div>
    {/if}

    {#if !histLoaded}
      <div class="card p-12 text-center text-sm text-ink-5">Loading…</div>
    {:else if histRows.length === 0}
      {#if !histErr}
        <div class="card p-12 text-center text-sm text-ink-5">
          Nothing in this window matched.
        </div>
      {/if}
    {:else}
      <div
        class="grid items-start gap-4"
        style={openJob ? `grid-template-columns: minmax(0,1fr) ${histTight ? 340 : 520}px;` : ""}
        bind:clientWidth={histW}
      >
        <div class="card overflow-hidden">
          <table class="w-full table-fixed text-left text-base">
            <thead>
              <tr class="border-b border-edge">
                <th class="label w-[124px] px-5 py-3">State</th>
                <th class="label px-3 py-3">Command</th>
                {#if !histCompact}<th class="label w-[140px] px-3 py-3">Executor</th>{/if}
                {#if !histTight}
                  <th class={`label px-3 py-3 ${histCompact ? "w-[112px]" : "w-[178px]"}`}>When</th>
                  <th class={`label px-3 py-3 pr-5 text-right ${histCompact ? "w-[152px]" : "w-[168px]"}`}>Output</th>
                {/if}
              </tr>
            </thead>
            <tbody>
              {#each histRows as row (row.job)}
                <tr
                  class={`h-10 border-b border-edge transition-colors duration-150 last:border-0 ${
                    openJob?.job === row.job ? "" : "hover:bg-surface-2"
                  }`}
                  style={openJob?.job === row.job ? "background: var(--hue-wash);" : ""}
                >
                  <td class="py-2 pr-3 pl-5"><StateBadge state={row.state} /></td>
                  <td class="mono truncate px-3 py-2 text-sm" title={row.command}>{row.command}</td>
                  {#if !histCompact}
                    <!-- `overflow-hidden` on the cell and `truncate` on the
                         label, because `table-fixed` does not clip: an
                         over-long value renders straight across the columns
                         to its right, which is how a raw node id here made
                         When and Output unreadable. The title keeps the
                         full id one hover away. -->
                    <td class="mono overflow-hidden px-3 py-2 text-sm text-ink-5">
                      <span
                        class="inline-flex max-w-full items-center gap-1.5"
                        style={hueStyle(hueOf(row.executor))}
                        title={row.executor}
                      >
                        <span class="name-dot shrink-0"></span>
                        <span class="truncate">{app.nodeLabel(row.executor)}</span>
                      </span>
                    </td>
                  {/if}
                  {#if !histTight}
                  <td class="overflow-hidden px-3 py-2 text-sm whitespace-nowrap text-ink-4">
                    {histStamp(row)}
                    <!-- Per row, because a merged page holds both and a
                         page-level flag tells you nothing about the row you
                         are looking at. An archive row is a projection —
                         rendering it identically would claim a fidelity it
                         does not have. -->
                    {#if row.fromArchive}
                      <span
                        class="ml-1.5 rounded px-1 text-2xs font-semibold"
                        style="background: var(--surface-1); color: var(--ink-5);"
                        title="From the Parquet archive: complete, and slower than the index">archive</span
                      >
                    {/if}
                  </td>
                  <!-- A2's three states plus B3's fourth, and the phrase
                       comes from `mesh-proto` so this renderer never
                       invents one. "The bytes are in that warehouse" and
                       "we threw them away" are different facts and the cell
                       must not blur them — nor claim either about a job
                       that ran an hour ago and still has its log on disk. -->
                  <td class="px-3 py-2 pr-5 text-right whitespace-nowrap">
                    {#if row.payloadSink}
                      <button class="linkish text-sm" onclick={() => openPayload(row)}>
                        in {row.payloadSink} →
                      </button>
                    {:else if row.outputState === "available"}
                      <span class="text-sm text-ink-6" title="The log is still on the machine that ran it">on {app.nodeLabel(row.executor)}</span>
                    {:else if row.outputShort}
                      <span class="text-sm text-ink-6" title={row.outputMessage}>{row.outputShort}</span>
                    {:else}
                      <span class="text-sm text-ink-6">—</span>
                    {/if}
                  </td>
                  {/if}
                </tr>
              {/each}
            </tbody>
          </table>
        </div>

        {#if openJob}
          <!-- One payload, by id, on a person's click. Never to paint a
               page, populate a filter or count anything — that is the
               fence, and it is why this panel only exists after a click. -->
          <div class="card flex flex-col overflow-hidden" style="max-height: 78vh;">
            <div class="flex items-center gap-2 border-b border-edge px-4 py-3">
              <div class="min-w-0 flex-1">
                <div class="mono truncate text-sm text-ink-3" title={openJob.command}>{openJob.command}</div>
                <div class="text-xs text-ink-5">
                  output in <span class="mono text-ink-4">{openJob.payloadSink}</span> · opened at the
                  end, where the failure is
                </div>
              </div>
              <button class={ui.rowAction} onclick={() => (openJob = null)}>Close</button>
            </div>
            <div class="flex items-center gap-2 border-b border-edge px-4 py-2">
              <input
                class="input flex-1"
                placeholder="Search inside this output…"
                bind:value={paySearch}
                onkeydown={(e) => e.key === "Enter" && researchPayload()}
              />
              <button class={ui.rowAction} disabled={payBusy} onclick={() => researchPayload()}>
                {payBusy ? "…" : "Search"}
              </button>
            </div>
            {#if payStatus}
              <!-- The destination's own words, verbatim. A read that fails
                   degrades this panel and nothing else. -->
              <div class="border-b border-edge px-4 py-3 text-sm" style="color: var(--status-warn-ink);">
                {payStatus}
              </div>
            {/if}
            <div class="flex-1 overflow-auto px-4 py-3">
              {#if payMore}
                <button
                  class="linkish mb-2 text-sm"
                  disabled={payBusy}
                  onclick={() => fetchWindow(payFrom, true)}
                >
                  {payBusy ? "Loading…" : "Load earlier"}
                </button>
              {/if}
              <pre class="mono text-xs leading-relaxed whitespace-pre-wrap text-ink-4">{payLines.join("\n")}</pre>
            </div>
            <div class="border-t border-edge px-4 py-2 text-xs text-ink-6">
              {payLines.length} lines · fetched by the node that exported them, a window at a time
            </div>
          </div>
        {/if}
      </div>
    {/if}
  {:else if app.loadErrors.jobs && app.jobsCounts.all === 0}
    <!-- A refused read is not an empty mesh (UX-2). -->
    <LoadError what="the jobs on this mesh" error={app.loadErrors.jobs} />
  {:else if app.jobsCounts.all === 0}
    <Empty
      title="Nothing has run on this mesh yet"
      command="meshnode run 'uname -a' --fanout all"
      reassurance={`Runs on ${machines ? `all ${machines} machines` : "every machine"} · about a second · nothing is changed`}
      action={{ label: "Run something on every machine", run: () => fanOut("uname -a", false) }}
      busy={starting}
      error={startErr}
    >
      Or launch your own from the
      <button class="linkish" onclick={() => app.go("run")}>New run page</button>
      — every job streams from its first byte and stays replayable.
    </Empty>
  {:else}
    {#if app.loadErrors.jobs}
      <LoadError class="mb-3.5" what="the latest jobs" error={app.loadErrors.jobs} />
    {/if}
    <!-- One click to "what broke". The single highest-value addition on this
         screen, and the reason the STATE column can now afford to be narrow. -->
    <div class="mb-3.5 flex items-center gap-3">
      <div class="seg" role="tablist">
        {#each FILTERS as [id, label] (id)}
          {@const n = counts[id]}
          <button
            role="tab"
            aria-selected={filter === id}
            class="flex items-center gap-1.5 disabled:opacity-45"
            disabled={n === 0 && id !== "all"}
            onclick={() => (filter = id)}
          >
            {label}
            <!-- The count in the colour of the state it counts: running
                 blue, failed red, succeeded green — and grey only when the
                 tab is not chosen or the count is nothing. -->
            <span
              class="num rounded-full px-1.5 text-2xs font-semibold"
              style={n > 0 && (filter === id || id !== "all")
                ? id === "failed"
                  ? "background: var(--status-error-wash); color: var(--status-error-ink);"
                  : id === "running"
                    ? "background: var(--status-running-wash); color: var(--status-running-ink);"
                    : id === "succeeded"
                      ? "background: var(--status-ok-wash); color: var(--status-ok-ink);"
                      : "background: var(--hue-wash); color: var(--hue-ink);"
                : "background: var(--surface-1); color: var(--ink-5);"}>{n}</span
            >
          </button>
        {/each}
      </div>
      {#if oneSubmitter}
        <span class="text-sm text-ink-5">
          all submitted by <span class="mono font-semibold text-ink-4">{[...submitters][0]}</span>
        </span>
      {/if}
    </div>

    <div
      class="grid items-start gap-4"
      style={selected ? `grid-template-columns: minmax(0,1fr) ${paneW}px;` : ""}
      bind:clientWidth={listW}
    >
      <div class="card overflow-hidden">
        <table class="w-full table-fixed text-left text-base">
          <thead>
            <tr class="border-b border-edge">
              <th class={`label px-5 py-3 ${tight ? "w-[124px]" : compact ? "w-[156px]" : "w-[172px]"}`}>State</th>
              <th class="label px-3 py-3">Command</th>
              {#if !tight}<th class="label w-[100px] px-3 py-3">Executor</th>{/if}
              {#if !oneSubmitter && !compact}<th class="label w-[104px] px-3 py-3">User</th>{/if}
              {#if !tight}<th class={`label w-[86px] py-3 text-right ${compact ? "px-3 pr-5" : "px-3"}`}>Took</th>{/if}
              {#if !compact}<th class="label w-[96px] px-3 py-3 pr-5 text-right">Started</th>{/if}
            </tr>
          </thead>
          <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
            {#each rows as row (row.groupHead ? `g:${row.job.fanGroup}` : row.job.jobId)}
              {@const j = row.job}
              {#if row.groupHead}
                <tr class="border-b border-edge" style="background: var(--surface-2);">
                  <td colspan={cols} class="px-5 py-1.5">
                    <span class="mono text-xs font-semibold" style="color: var(--hue-ink);">{j.fanGroup?.slice(0, 14)}</span>
                    {#if j.fanGroup && answered.has(j.fanGroup)}
                      <button
                        class="linkish mono ml-2 text-xs"
                        onclick={() => showAnswers(j.fanGroup)}
                      >
                        answers →
                      </button>
                    {/if}
                    <span class="mono ml-2 text-xs text-ink-5">
                      fan-out × {row.groupSize}
                      {#if row.groupState && row.groupState.done < row.groupState.total}
                        · {row.groupState.done}/{row.groupState.total} done
                      {/if}
                      {#if row.groupState?.failed}
                        <span style="color: var(--status-error-ink);"> · {row.groupState.failed} failed</span>
                      {/if}
                    </span>
                  </td>
                </tr>
              {:else}
                <tr
                  class={`relative h-10 cursor-pointer border-b border-edge transition-colors duration-150 last:border-0 ${
                    selectedId === j.jobId ? "" : "hover:bg-surface-2"
                  }`}
                  style={selectedId === j.jobId ? "background: var(--hue-wash);" : ""}
                  onclick={() => select(j.jobId)}
                  use:rowAction={() => select(j.jobId)}
                  aria-current={selectedId === j.jobId ? "true" : undefined}
                >
                  <td class="relative py-2 pr-3 pl-5">
                    {#if selectedId === j.jobId}
                      <span class="absolute top-0 bottom-0 left-0 w-0.5"></span>
                    {/if}
                    <span class="inline-flex items-center gap-1.5">
                      <StateBadge state={j.state} />
                      <!-- Exit code only where it is the answer to something. -->
                      {#if j.exitCode !== null && j.exitCode !== 0}
                        <span class="num text-2xs font-semibold" style="color: var(--status-error-ink);">exit {j.exitCode}</span>
                      {/if}
                    </span>
                  </td>
                  <td class="max-w-64 px-3 py-2">
                    <div class="flex items-stretch">
                      {#if row.inGroup}
                        <!-- A spine that runs the full row height and stops at
                             the last member, so the group visibly closes. -->
                        <span
                          class={`ml-1 w-[2px] flex-none self-start rounded-b-full ${row.lastInGroup ? "h-[14px]" : "h-10"}`}
                          style="background: color-mix(in srgb, var(--accent) 32%, transparent);"
                        ></span>
                        <span class="w-2.5 flex-none"></span>
                      {/if}
                      <span class="mono self-center truncate text-xs text-ink-3">{j.cmdPreview}</span>
                    </div>
                  </td>
                  {#if !tight}
                  <td class="overflow-hidden px-3 py-2 text-sm text-ink-2">
                    {#if j.executorName}
                      <span
                        class="mono inline-flex max-w-full items-center gap-1.5"
                        style={hueStyle(hueOf(j.executorName))}
                        title={j.executorName}
                      >
                        <span class="name-dot shrink-0"></span><span class="truncate">{j.executorName}</span>
                      </span>
                    {:else}—{/if}
                  </td>
                  {/if}
                  {#if !oneSubmitter && !compact}
                    <td class="px-3 py-2 text-sm text-ink-4">{j.submitterUsername ?? "—"}</td>
                  {/if}
                  {#if !tight}
                  <td
                    class={`num py-2 text-right text-sm ${compact ? "px-3 pr-5" : "px-3"}`}
                    style={isLive(j)
                      ? "color: var(--status-running-ink);"
                      : durMs(j) === slowest && visible.length > 1
                        ? "color: var(--ink-1); font-weight: 600;"
                        : "color: var(--ink-4);"}
                    title={!isLive(j) && durMs(j) === slowest && visible.length > 1 ? "longest run in view" : undefined}
                  >
                    {fmtDur(j)}
                  </td>
                  {/if}
                  {#if !compact}
                    <td class="num px-3 py-2 pr-5 text-right text-sm text-ink-5">{ago(j.createdAtMicros)}</td>
                  {/if}
                </tr>
              {/if}
            {:else}
              <tr>
                <td colspan={cols} class="px-5 py-10 text-center text-sm text-ink-5">
                  No {filter} jobs.
                  <button class="linkish" onclick={() => (filter = "all")}>Show all {counts.all}</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
        <!-- The table terminates. 600px of undifferentiated canvas below an
             open-ended card reads as a page that failed to load. -->
        <div class="num flex items-center gap-2 border-t border-edge px-5 py-2.5 text-xs text-ink-5">
          <span>
            {#if offset > 0}{offset + 1}–{offset + visible.length} of {matched}{:else}{visible.length}
              of {matched}{/if} jobs
          </span>
          {#if counts.running > 0}
            <span class="text-ink-6">·</span><span style="color: var(--status-running-ink);">{counts.running} live</span>
          {/if}
          {#if counts.failed > 0}
            <span class="text-ink-6">·</span><span style="color: var(--status-error-ink);">{counts.failed} failed</span>
          {/if}
          {#if pageErr}
            <span class="text-ink-6">·</span><span style="color: var(--status-error-ink);">{pageErr}</span>
          {/if}
          <!-- Paging, because the list is bounded at the engine now (H-1)
               and a bound nobody can step past is a bound that hides rows. -->
          {#if offset > 0 || hasMore}
            <span class="ml-auto flex items-center gap-1.5">
              <button
                class={ui.rowAction}
                disabled={offset === 0}
                onclick={() => (offset = Math.max(0, offset - PAGE))}>Newer</button
              >
              <button class={ui.rowAction} disabled={!hasMore} onclick={() => (offset = offset + PAGE)}
                >Older</button
              >
            </span>
          {/if}
        </div>
      </div>

      {#if selected}
        <!-- One grid item, not three. The parent is a two-column grid, so
             every sibling here is placed as its own cell — which is how the
             placement breakdown ended up wrapped onto a second row *under
             the job table*, full width and nowhere near the job it
             described. The detail is one column; it has to be one child. -->
        <div class="flex flex-col">
        <!-- Identity and the tab bar: dark chrome, and the one part of the
             detail that never changes. Which job you are looking at has to
             be true on every tab, so it lives above them. -->
        <div class="on-code fade-in overflow-hidden rounded-t-xl bg-code" style="box-shadow: var(--sh-code);">
          <div class="flex h-11 items-center gap-2.5 border-b border-edge-code px-4">
            <Dot kind={selected.state} size={6} />
            <span class="mono text-sm text-inkcode-2">{selected.shortId}</span>
            <!-- L2 P4's exit demo: hovering the machine's name says why it
                 won and what the runner-up lost on, without anyone typing a
                 command. The sentence comes from the engine, so this and
                 `meshnode jobs explain` cannot say different things. -->
            {#if explain?.placement?.why}
              <span class="text-sm font-semibold text-inkcode">
                <Why text={explain.placement.why}>{selected.executorName ?? ""}</Why>
              </span>
            {:else}
              <span class="text-sm font-semibold text-inkcode">{selected.executorName ?? ""}</span>
            {/if}
            {#if explain?.cacheHit}
              <!-- A hit is visible or it is untrustworthy (§11.2): "it
                   didn't run" and "it broke" are indistinguishable unless
                   the product says which. -->
              <span
                class="mono rounded-full px-2 py-0.5 text-2xs font-semibold"
                style="background: color-mix(in srgb, var(--accent-bright) 18%, transparent); color: var(--accent-bright);"
              >
                <!-- Served from the cache is a claim, and a claim gets a
                     way to question it (L2 §10.7): which key, produced by
                     which job on which node, and what matching a key
                     means. -->
                <Why
                  text={`Served from the action cache: key ${explain.cacheHit.shortKey}, produced by job ${explain.cacheHit.producedBy} on ${explain.cacheHit.producer}. ${explain.keyWhy ?? ""}`}
                >
                  ⚡ cached
                </Why>
              </span>
            {/if}
            <span class="ml-auto"><StateBadge state={selected.state} /></span>
            {#if selected.state === "running" || selected.state === "queued"}
              <button
                class="h-7 rounded-sm border px-2.5 text-xs font-semibold transition-colors hover:bg-[color-mix(in_srgb,var(--status-error)_16%,transparent)]"
                style="border-color: var(--status-error-edge); color: var(--status-error-on-code);"
                onclick={() => cancel(selected)}
              >
                Cancel
              </button>
            {/if}
          </div>
          <div class="border-b border-edge-code px-4 py-3">
            <div class="mono selectable text-xs break-all text-inkcode-2">{selected.cmdPreview}</div>
            <div class="mono mt-2 flex flex-wrap gap-x-4 gap-y-1 text-2xs text-inkcode-4">
              <span>runtime {selected.runtime}</span>
              <span>took {fmtDur(selected)}</span>
              <span>user {selected.submitterUsername ?? "—"}</span>
              <span style={selected.exitCode ? "color: var(--status-error-on-code);" : ""}>exit {selected.exitCode ?? "—"}</span>
              {#if selected.fanGroup}
                <span style="color: var(--accent-bright);">
                  {selected.fanGroup.slice(0, 14)} · target {(selected.fanIndex ?? 0) + 1}/{selected.fanTotal}
                </span>
              {/if}
              {#if placedSummary}
                <button
                  class="underline decoration-dotted underline-offset-2 transition-colors hover:text-inkcode-2"
                  style="color: var(--accent-bright);"
                  onclick={() => (detailTab = "placement")}
                  title={placedGloss
                    ? `${placedGloss}. Open the full scoring breakdown.`
                    : "Open the full scoring breakdown"}
                >
                  {placedSummary}
                </button>
              {/if}
            </div>
          </div>
          <!-- C8 P2 — the breakdown was a second card below a 400px log
               rail, which is far enough down that people who had been told
               the feature exists still could not find it. A tab is a label
               you can see without scrolling. -->
          <div class="flex items-center gap-1 px-3 pt-2" role="tablist">
            {#each detailTabs as t (t.id)}
              <button
                role="tab"
                aria-selected={detailTab === t.id}
                title={t.id === "key" ? "Two runs whose keys match are the same work" : undefined}
                class="rounded-t-md px-3 py-1.5 text-xs font-semibold transition-colors"
                style={detailTab === t.id
                  ? "background: var(--surface-1); color: var(--ink-2);"
                  : "color: var(--ink-code-4);"}
                onclick={() => (detailTab = t.id)}
              >
                {t.label}
              </button>
            {/each}
          </div>
        </div>

        {#if detailTab === "output"}
          <div class="on-code overflow-hidden rounded-b-xl bg-code" style="box-shadow: var(--sh-code);">
          {#if logDropped > 0}
            <div class="border-b border-edge px-4 py-1.5 text-2xs text-ink-6">
              earliest {logDropped.toLocaleString()} lines not shown — the pane keeps the last
              {OUTPUT_MAX_LINES.toLocaleString()}
            </div>
          {/if}
          <pre
            bind:this={logEl}
            onscroll={onLogScroll}
            class="term dark-scroll m-0 max-h-[400px] min-h-40 overflow-y-auto px-4 py-3"
            style="color: var(--ink-code-1);">{logText || (logErr ?? " ")}</pre>
          <div class="mono flex h-10 items-center border-t border-edge-code px-4 text-xs" style="color: var(--ink-code-4);">
            {#if logErr}
              <span style="color: var(--status-error-on-code);">{logErr}</span>
            {:else if logDone}
              ← {logDone}
            {:else}
              <span class="breathe" style="color: var(--accent-bright);">following ↓</span>
            {/if}
          </div>
          </div>
        {:else if detailTab === "placement"}
          <div class="overflow-hidden rounded-b-xl border border-edge-card bg-surface-1">
            {#if explain?.cacheHit}
              <div class="border-b border-edge-inner px-4 py-2 text-2xs text-ink-5">
                Nothing was placed — this run was served from the cache.
              </div>
            {/if}
          <div class="px-4 py-3">
            {#if explainErr}
              <p class="text-2xs text-ink-5">{explainErr}</p>
            {:else if explain?.placement}
              <PlacementChart placement={explain.placement} />
            {:else if explain?.placementOn}
              <p class="text-2xs text-ink-5">
                The breakdown is job-record data on the node that submitted this job
                (<span class="mono">{explain.placementOn}</span>). This node never made
                the decision and re-scoring today's mesh would answer a different
                question.
              </p>
            {:else if explain?.cacheHit}
              <!-- Not a missing record: a cache hit places nothing. -->
              <p class="text-2xs text-ink-5">
                Nothing was placed — this job's answer came from the cache.
              </p>
            {:else if explain}
              <p class="text-2xs text-ink-5">No placement breakdown was kept for this job.</p>
            {:else}
              <p class="text-2xs text-ink-5">…</p>
            {/if}
          </div>
          </div>
        {:else}
          <div class="overflow-hidden rounded-b-xl border border-edge-card bg-surface-1">
            {#if explain?.actionKey}
            <!-- C7 P5 — "why was this a miss": the key, field by field. -->
            <div class="border-t border-edge-inner px-4 py-3">
              <!-- The toggle and the explanation are siblings, not nested:
                   a focusable hover inside a button is a keyboard trap and
                   invalid besides. The button owns the chevron and the
                   word; the key itself carries the note (L2 P4). -->
              <div class="flex w-full items-center gap-2 text-xs font-semibold text-ink-2">
                <button
                  class="flex items-center gap-2 text-left"
                  onclick={() => (showKey = !showKey)}
                >
                  <span style="color: var(--ink-faint)">{showKey ? "▾" : "▸"}</span>
                  Action key
                </button>
                {#if explain.keyWhy}
                  <span class="mono min-w-0 truncate text-2xs font-normal text-ink-5">
                    <Why text={explain.keyWhy}>{explain.actionKey}</Why>
                  </span>
                {:else}
                  <span class="mono min-w-0 truncate text-2xs font-normal text-ink-5"
                    >{explain.actionKey}</span
                  >
                {/if}
                {#if explain.cacheHit}
                  <span class="ml-auto flex-none text-2xs font-normal text-ink-5">
                    saved {fmtBytes(explain.cacheHit.bytesSaved)}
                  </span>
                {/if}
              </div>
              {#if true}
                <table class="mt-3 w-full text-2xs">
                  <thead>
                    <tr>
                      <th class="label pb-1 text-left">field</th>
                      <th
                        class="label pb-1 text-left"
                        title="A short fingerprint of the value, for comparing two runs at a glance"
                        >digest</th
                      >
                      <th class="label pb-1 text-left">value</th>
                    </tr>
                  </thead>
                  <tbody>
                    {#each explain.keyFields as field (field.name)}
                      <tr
                        class="border-t border-edge-row"
                        style={field.differs
                          ? "background: var(--accent-wash-2); font-weight: 600;"
                          : ""}
                      >
                        <td class="py-1 pr-3 text-ink-3">{field.name}</td>
                        <td class="mono py-1 pr-3 text-ink-5">{field.digest}</td>
                        <td class="mono py-1 text-ink-4">
                          <span class="line-clamp-2 break-all">{field.value}</span>
                        </td>
                      </tr>
                    {/each}
                  </tbody>
                </table>
                <p class="mt-2 text-2xs text-ink-5">
                  Two runs whose keys match are the same work. A run that missed differs
                  from the one it should have hit in exactly the highlighted fields.
                </p>
              {/if}
            </div>
          {/if}
          </div>
        {/if}
        </div>
      {/if}
    </div>
  {/if}
</div>

<!-- clear modal (dark: nothing here is recoverable) -->
<Modal
  open={clearOpen}
  kicker="clear job history"
  peer={app.status?.nodeName ?? ""}
  onclose={() => {
    if (!clearBusy) clearOpen = false;
  }}
>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">
      Forget {finished} finished job{finished === 1 ? "" : "s"}?
    </h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      This clears them <span class="font-semibold" style="color: var(--ink-code-2);">for
      everyone on the mesh</span>, not just on this peer, and takes each node's
      stored output with it. There is no undo.
    </p>
    <ul class="mt-3.5 mb-0 list-none space-y-1.5 p-0 text-sm" style="color: var(--ink-code-3);">
      <li>
        <span style="color: var(--accent-bright);">·</span>
        {counts.running} running or queued job{counts.running === 1 ? "" : "s"} — untouched, and still listed when {counts.running === 1 ? "it finishes" : "they finish"}
      </li>
      <li>
        <span style="color: var(--accent-bright);">·</span>
        Searchable history and archived results are a separate store and are kept
      </li>
    </ul>
    {#if clearErr}
      <div
        class="selectable mt-3 rounded-sm px-3 py-2 text-sm"
        style="background: var(--status-error-wash); color: var(--status-error-ink);"
      >{clearErr}</div>
    {/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button
        class="btn btn-lg border border-edge-code font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]"
        disabled={clearBusy}
        onclick={() => (clearOpen = false)}
      >Cancel</button>
      <button class="btn btn-danger btn-lg" disabled={clearBusy} onclick={doClear}>
        {clearBusy ? "Clearing…" : "Clear history"}
      </button>
    </div>
  </div>
</Modal>

<style>
  /* The overflow menu's one item is destructive, so it is said in the
     error ink — a plain row would make "forget everything" look like any
     other entry. */
  .more-item {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 100%;
    padding: 7px 12px;
    text-align: left;
    font-size: 12.5px;
    color: var(--status-error-ink);
    cursor: pointer;
  }
  .more-item:hover:not(:disabled) {
    background: var(--status-error-wash);
  }
  .more-item:disabled {
    color: var(--ink-disabled);
    cursor: default;
  }
</style>
