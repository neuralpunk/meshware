<script lang="ts">
  // Mesh — rebuilt against the 2026-08 console handoff.
  //
  // The screen used to be a status page: four vitals cards, a decorative
  // ring, a permanently docked pairing panel, and a list of things that had
  // happened. Everything on it was readable and nothing on it was
  // *operable* — you could not click a node, and the one action it offered
  // was the one people perform about once a month.
  //
  // Four moves fix that, and they are the whole diff:
  //
  //  1. **Identity is a strip.** The four vitals cards collapse into one row
  //     of label-over-value cells. Same seven facts, one 40px band instead
  //     of a 110px card grid.
  //  2. **Selection is the primary interaction.** The ring is a canvas now:
  //     clicking a node selects it, and a docked inspector answers "what is
  //     that machine" without leaving the screen or losing the diagram.
  //  3. **Graph *and* table.** The ring says shape; the table says facts.
  //     Both are on screen, both are live, and selection is shared — click a
  //     row and the ring follows, click a node and the row lights up.
  //  4. **Pairing is a task, so it lives in a drawer** behind the page's one
  //     primary button, rather than owning a third of the layout forever.
  //
  // The one header action the handoff specifies that has no counterpart here
  // is "Rebalance": meshware's scheduler is pull-based — nodes take work when
  // they have a free slot — so there is no placement to redistribute. A
  // button that did nothing would be worse than its absence.

  import { dialog } from "../lib/dialog";
  import { api, errText, fmtSizeMb, type CandidateView, type ApproveEvent, type NodeView } from "../lib/api";
  import { app, ago, uptime, fmtCountdown, initialsOf, backupFileName } from "../lib/state.svelte";
  import { chrome } from "../lib/chrome/chrome.svelte";
  import Hint from "../lib/components/Hint.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Spinner from "../lib/components/Spinner.svelte";
  import Ring from "../lib/components/Ring.svelte";
  import NodesView from "./Nodes.svelte";
  import Software from "../lib/components/Software.svelte";
  import CapacityView from "./Capacity.svelte";
  import PlacementView from "./Placement.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import PairCode from "../lib/components/PairCode.svelte";
  import DeploySheet from "../lib/components/DeploySheet.svelte";
  import GetStarted from "../lib/components/GetStarted.svelte";
  import {
    meter,
    roll,
    tabline,
    reveal,
    staggerIn,
    popMenu,
    fadeIn,
    slideX,
    DUR,
  } from "../lib/motion";
  import type { TopView } from "../lib/bindings/TopView";
  import { renderMeshSnapshot } from "../lib/snapshot";
  import { attentionItems, type AttentionItem } from "../lib/attention";
  import { flattenRecord } from "../lib/flatten";
  import { poll } from "../lib/poll";
  import type { DeviceView } from "../lib/bindings/DeviceView";
  import type { DeploymentView } from "../lib/bindings/DeploymentView";
  import Topology from "../lib/topology/Topology.svelte";
  import type { MeshNode } from "../lib/topology/graph";
  import brandSvg from "../lib/icons/brand-icon.svg?raw";

  // 1s heartbeat. Everything relative on this screen — gossip age, scan
  // elapsed, the code TTL — recomputes off this rather than re-fetching:
  // the copy promises "streaming, no refresh", and a clock is not a fetch.
  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });

  // The toolbar's breadcrumb says which of this screen's panels you
  // are on; it reads that off `app.sub` through `chrome/nav.ts`, so this
  // file no longer publishes it. The title below names the mesh.

  // ---------------------------------------------------------------------------
  // Membership
  // ---------------------------------------------------------------------------

  type Liveness = "alive" | "noexec" | "suspect" | "dead";

  function livenessOf(n: NodeView): Liveness {
    if (n.liveness === "dead") return "dead";
    if (n.liveness === "suspect") return "suspect";
    return n.executing ? "alive" : "noexec";
  }

  const TONE: Record<Liveness, { dot: string; fg: string; bg: string; label: string }> = {
    alive: { dot: "var(--liveness-alive)", fg: "var(--accent-ink)", bg: "var(--accent-wash)", label: "alive" },
    noexec: { dot: "var(--liveness-alive)", fg: "var(--accent-ink)", bg: "var(--accent-wash)", label: "alive" },
    suspect: {
      dot: "var(--liveness-suspect)",
      fg: "var(--status-warn-ink)",
      bg: "var(--status-warn-wash)",
      label: "suspect",
    },
    dead: { dot: "var(--liveness-dead)", fg: "var(--ink-5)", bg: "var(--surface-3)", label: "dead" },
  };

  // See `AppState.selfNode`: a console peer is not in `app.nodes`.
  const self = $derived(app.selfNode);
  const others = $derived(app.nodes.filter((n) => !n.isSelf && !n.revoked && !n.left));
  /** Everything the mesh currently counts as a member, this peer included. */
  const members = $derived(
    [...app.nodes.filter((n) => !n.revoked && !n.left)].sort((a, b) =>
      a.isSelf === b.isSelf ? a.name.localeCompare(b.name) : a.isSelf ? -1 : 1,
    ),
  );

  const lastGossipSecs = $derived(Math.max(0, Math.floor((now - app.lastSync) / 1000)));
  /**
   * `top` by node id, built once per poll (M-25).
   *
   * `topOf` is called per row, per inspector field and — through
   * `seatOf` — per animation frame, and it was a linear scan of `app.top`
   * every time. Fifty peers dragged on a laptop meant thousands of
   * comparisons per frame before a single pixel was drawn. One `Map` per
   * poll turns every one of those into a hash lookup.
   */
  const topIndex = $derived(new Map(app.top.map((t) => [t.nodeId, t])));
  const topOf = $derived((id: string) => topIndex.get(id) ?? null);
  /** Running jobs per executor *name*, likewise folded once. */
  const runningByExecutor = $derived.by(() => {
    const m = new Map<string, number>();
    for (const j of app.jobs) {
      if (j.state !== "running" || !j.executorName) continue;
      m.set(j.executorName, (m.get(j.executorName) ?? 0) + 1);
    }
    return m;
  });
  /** What a node is running right now.
   *
   *  `top` is the node's own count and is authoritative; the job list is the
   *  fallback for a peer that has not reported saturation yet. The table and
   *  the inspector must not answer this differently, which they did. */
  const jobsOn = $derived((n: NodeView) => {
    const t = topIndex.get(n.nodeId);
    if (t) return t.running;
    return runningByExecutor.get(n.name) ?? 0;
  });

  // -- heat ---------------------------------------------------------------------
  //
  // What each machine is *doing*, painted onto the diagram so a fleet can be
  // read at a glance the way a rack of blinking lights can. The reading is
  // `top.cpuPct` — live utilisation, the number that moves the moment work
  // starts — with the one-minute load per core standing in for a peer on
  // an older minor that does not report it. No reading is drawn as
  // nothing: unknown is neutral, never idle.

  /** CPU, 0–100, or null when nobody has said. */
  function cpuOf(t: TopView | null): number | null {
    if (!t) return null;
    if (t.cpuPct != null) return Math.max(0, Math.min(100, Math.round(t.cpuPct)));
    if (t.loadPerCore != null) return Math.max(0, Math.min(100, Math.round(t.loadPerCore * 100)));
    return null;
  }

  // -- selection --------------------------------------------------------------
  //
  // A docked inspector with nothing in it is a fifth of the workspace spent
  // saying "pick something", so the screen opens with a peer already chosen:
  // the first live one that is not this machine, because "what is the rest of
  // the mesh doing" is why anybody opened this page.
  let selectedId = $state<string | null>(null);
  const selected = $derived(
    app.nodes.find((n) => n.nodeId === selectedId) ??
      others.find((n) => livenessOf(n) === "alive") ??
      others[0] ??
      self,
  );
  $effect(() => {
    if (selectedId && !app.nodes.some((n) => n.nodeId === selectedId)) selectedId = null;
  });

  // `nodes` and `compute` joined this screen as panels with the Focus
  // surface: they were whole sections, and they answer the same question
  // this screen answers at a different altitude — how much machine is
  // there, and how is it.
  //
  // **They are rows in the sidebar now, not tabs in this page (6c).** The
  // list of them, and what each is called, lives in `chrome/nav.ts`, where
  // the rail and the breadcrumb both read it; this screen only asks which
  // one is open. A tab row here would have been a second navigation, in a
  // second place, in a second shape, saying what the rail already says.
  //
  // **Updates joined them (2026-09-11).** It was the second tab of the
  // node table — a rollout filed as a mode of the fleet list, one click
  // in and invisible from the rail. It is the same component, rendered
  // here, reached by its own row.
  //
  // **Five, from twelve (2026-09-14).** Members, Trust, Events and Settings
  // were the same answers as other screens; Compute, Runtimes, Ports and
  // Volumes are one panel, Capacity. `chrome/nav.ts` has the reasoning,
  // and `app.go` sends each retired id to where its content went.
  type PageTab = "nodes" | "topology" | "placement" | "capacity" | "updates";
  const tab = $derived((app.sub ?? "nodes") as PageTab);

  let inspectorTab = $state<"details" | "events" | "definition">("details");
  let inspectorOpen = $state(true);

  // ---------------------------------------------------------------------------
  // Detail strip
  // ---------------------------------------------------------------------------

  // "In sync" means *caught up on the op log*, which is what the words say
  // and what an operator wants to know. It used to count `!stale`, which is
  // the forward-compat flag — a peer on an older minor that was perfectly
  // caught up on everything it can decode still counted as out of sync,
  // and a peer 200 ops behind counted as in.
  //
  // A peer that has not synced with us since this daemon started has no
  // reading at all. It is counted as in sync rather than out: we have no
  // evidence it is behind, and inventing some would make a quiet fleet look
  // broken every time the daemon restarts.
  const inSync = $derived(
    app.nodes.filter((n) => !n.revoked && !n.left && (n.sync?.behind ?? 0) === 0).length,
  );

  // ---------------------------------------------------------------------------
  // The mesh's spec sheet (L2 P1)
  // ---------------------------------------------------------------------------
  //
  // One line, and not one this file composes. `totals.strip` is built in
  // `mesh-proto` and rendered verbatim here, by the CLI and into the shared
  // snapshot — because the failure this replaces was three surfaces each
  // summing the same two tables slightly differently, and three renderings
  // of one summation is that same failure one layer up.
  //
  // Absent until the first `totals` lands, and absent again if that read
  // fails: a spec sheet that guesses is worse than no spec sheet.
  const spec = $derived(app.totals);

  /** The poolable-disk figure, in the scale `totals.strip` states it in,
   *  so the cell and the sentence under it never disagree by a rounding. */
  const fmtDisk = fmtSizeMb;

  // -- what needs attention (UX-3) ---------------------------------------
  //
  // The capsule used to answer for this peer's engine alone, so it read
  // "Healthy" over a fleet with a machine not replying. It is derived from
  // the machines and the deployments now (`lib/attention.ts`), which needs
  // two tables the shared poll does not carry: the accelerators and the
  // deployments. Both are read here on a slow clock, and a refusal of
  // either leaves that half out rather than guessing.
  let attnDevices = $state.raw<DeviceView[] | null>(null);
  let attnDeployments = $state.raw<DeploymentView[] | null>(null);
  $effect(() =>
    poll(async () => {
      const [d, p] = await Promise.all([
        api.devices().catch(() => null),
        api.deployments().catch(() => null),
      ]);
      attnDevices = d;
      attnDeployments = p;
    }, 10_000),
  );
  const attention = $derived(
    attentionItems({ nodes: app.nodes, devices: attnDevices, deployments: attnDeployments }),
  );
  let attentionOpen = $state(false);

  /** Take a person to the thing an attention row is about. */
  function openAttention(item: AttentionItem) {
    attentionOpen = false;
    if (item.kind === "deployment") {
      sessionStorage.setItem("deployments:open", item.target);
      app.go("deployments");
      return;
    }
    sessionStorage.setItem("nodes:select", item.target);
    app.go("mesh", "nodes");
  }

  /** The one-word answer, and the colour it is said in — the capsule
   *  beside the mesh's name. This peer's own state comes first, because a
   *  stopped engine is the reason every other reading is stale. */
  const health = $derived.by(() => {
    if (app.engineStopped)
      return { label: "Stopped", tone: "var(--status-warn)", ink: "var(--status-warn-ink)" };
    if (app.status?.draining)
      return { label: "Draining", tone: "var(--status-warn)", ink: "var(--status-warn-ink)" };
    if (attention.length > 0) {
      const fault = attention.some((a) => a.tone === "error");
      return {
        label: `Needs attention · ${attention.length}`,
        tone: fault ? "var(--status-error)" : "var(--status-warn)",
        ink: fault ? "var(--status-error-ink)" : "var(--status-warn-ink)",
      };
    }
    return { label: "Healthy", tone: "var(--accent)", ink: "var(--accent-ink)" };
  });

  /**
   * The eight vitals, as two grouped surfaces of four rows (6c).
   *
   * They used to be a 52px strip of eight cells above the tab row: one
   * shape that said "dashboard" and read as none of the eight. A key/value
   * row is a sentence — *Members alive, four of four* — and four sentences
   * in a group is something a person finishes reading.
   *
   * `ok` marks the ones that are an *answer* rather than a measurement:
   * everybody is caught up. Those take the tint; the rest are ink, so the
   * tint keeps meaning "yes". A draining peer or a member that has dropped
   * out is a fact, and facts are ink.
   */
  const allSynced = $derived(members.length > 0 && inSync === members.length);
  const healthy = $derived(!app.engineStopped && !app.status?.draining);

  type Vital = { k: string; v: string; ok?: boolean };
  // Five, not eight (2026-09 declutter): Status is the capsule beside the
  // title, and running/queued are the metric card above — saying each of
  // them twice on one screen made the screen longer, not clearer. What is
  // left is what nothing else on the panel says.
  //
  // Four since UX-12: how many machines answer is the Machines tile's
  // sub-line now, and "in sync" is said against the same total the tile
  // counts — one noun, one total, on the whole screen.
  const vitalsStrip = $derived<Vital[]>([
    {
      k: "Up to date",
      v: allSynced ? `all ${members.length}` : `${inSync} of ${members.length}`,
      ok: allSynced && healthy,
    },
    { k: "Uptime", v: uptime(app.status?.uptimeSecs ?? 0) },
    { k: "Last heard", v: `${lastGossipSecs}s ago` },
    // Poolable disk is the one hardware figure that is a property of the
    // *mesh* rather than of a machine — disk pools and RAM does not — so it
    // belongs beside uptime and not in the spec sentence alone, where it
    // was the only headline number you had to read a line of prose to find.
    ...(spec ? [{ k: "Poolable disk", v: fmtDisk(spec.diskMb) }] : []),
  ]);

  // ---------------------------------------------------------------------------
  // Topology
  //
  // The picture used to be a hub with spokes: this machine in the middle of a
  // fixed 640×360 frame, every peer hung off it on r=128. It read as a
  // control plane with clients attached, which meshware is not — every node
  // gossips with every other, and the machine you happen to be sitting at is
  // not special to any of them. It also stopped working at about forty peers,
  // because a ring of chips is a ring of chips.
  //
  // What replaced it is a **peer lattice**: everyone drawn the same way,
  // placed by what they have in common. The drawing itself lives in
  // `lib/topology` — a canvas, because two thousand machines is sixteen
  // thousand SVG elements and a window that stops answering the mouse.
  //
  // There is one view, and it is this one. There used to be a second — an
  // adjacency matrix that took over above two thousand machines — and it was
  // removed rather than hidden. Two topologies means two things to learn, two
  // sets of controls that mean different things, and a preference to store
  // and explain; and in practice nobody wanted the matrix while the graph
  // still read. One drawing that holds from three machines to five thousand
  // is worth more than two that each hold half that range.
  //
  // ## What a machine looks like here
  //
  // **Fill is load**, repainted every poll, in the five bands the legend
  // names. **Shape is liveness** — solid, a dashed edge for suspect, a
  // hollowed middle for a machine told not to take work, and a plain hole for
  // one that has stopped answering. Load owns colour alone, so a busy machine
  // and a failing one never look the same.
  //
  // **Clicking one opens the inspector.** Everything it is not linked to
  // recedes, its peers keep their colour and gain names, and the panel on the
  // right spells out the fields no dot can carry.
  // ---------------------------------------------------------------------------

  /** The canvas, for the controls that act on it. Typed by the three things
   *  this screen actually calls, so the contract is visible from here. */
  let topo = $state<{
    zoomBy: (f: number) => void;
    fit: () => void;
    resettle: () => void;
    snapshotCanvas: (w?: number, h?: number) => HTMLCanvasElement;
  } | null>(null);

  /** The canvas pane's width, and whether its legend has room to sit in a
   *  line (UX-11). */
  let canvasW = $state(0);
  const legendTight = $derived(canvasW > 0 && canvasW < 640);
  let legendOpen = $state(false);

  /** Peer links. A dense fleet reads better as a cloud of machines than as a
   *  ball of wool, and this is the switch between the two. */
  let showLinks = $state(true);
  /** The fleet, flattened to what the canvas needs and nothing more: the
   *  renderer should not know what an op log is. */
  /**
   * Everything the topology draws.
   *
   * `members` comes from `app.nodes`, which **deliberately excludes
   * consoles** — a machine already running `meshnode` that then opens
   * Director is two rows describing one box, and every fleet list drops the
   * second. That is right for a list of machines and wrong for a picture of
   * the mesh: it meant this machine was missing from its own topology.
   *
   * So the rule here is the one the person asked for and the one the data
   * supports: **this machine is on the map when its owner has said it
   * contributes**. `contributes` is the signed capacity decision (and it
   * defaults to true, so a mesh that has never touched the control shows
   * everything). Peers are drawn either way — a peer nobody sends work to
   * still gossips, and it is still somebody's machine to find.
   */
  const topoNodes = $derived.by(() => {
    const out = members.filter((n) => !n.isSelf || n.contributes);
    const me = app.selfNode;
    if (me && me.contributes && !out.some((n) => n.nodeId === me.nodeId)) out.push(me);
    return out;
  });

  const meshNodes = $derived<MeshNode[]>(
    topoNodes
      .map((n) => {
        const state = livenessOf(n);
        const alive = state === "alive" || state === "noexec";
        const jobs = jobsOn(n);
        // Load only for a peer that is answering: a suspect's last reading is a
        // reading from before it went quiet, and drawing it would say the
        // machine is fine when the whole point of "suspect" is that nobody
        // knows.
        const top = topOf(n.nodeId);
        const cpu = alive && !(n.isSelf && app.engineStopped) ? cpuOf(top) : null;
        return {
          id: n.nodeId,
          name: n.name,
          isSelf: n.isSelf,
          state,
          tags: n.tags,
          cpu,
          jobs,
          kind: state === "noexec" ? ("observer" as const) : (top?.devices ?? 0) > 0 ? ("gpu" as const) : ("server" as const),
          sub:
            state === "suspect"
              ? "no reply"
              : state === "dead"
                ? "dead"
                : state === "noexec"
                  ? "no-execute"
                  : `${jobs} job${jobs === 1 ? "" : "s"}`,
        };
      }),
  );

  /** How many links the lattice is drawing, reported by the thing that built
   *  them. A tag too big to draw as a clique becomes a chain through its
   *  members, so this is a sample of the shared-tag relation rather than all
   *  of it — the legend says so, and hovering a machine still lights its
   *  whole neighbourhood. */
  let tagLinks = $state(0);
  /** Whether those links are the whole mesh or a sample of it — the legend
   *  must not say "shares a tag" about a line that is there because every
   *  machine is every other machine's peer. */
  let completeMesh = $state(false);


  function select(id: string | null) {
    selectedId = id;
    // Clicking a machine is a request to look at it, so the panel that shows
    // it had better be open. Clicking the ground clears, and the inspector
    // falls back to whatever it shows when nothing is picked.
    if (id) inspectorOpen = true;
  }

  // ---------------------------------------------------------------------------
  // The inspector's answers
  // ---------------------------------------------------------------------------

  const selTone = $derived(selected ? TONE[livenessOf(selected)] : TONE.dead);

  /** The version a peer is running. Only a peer being served a reduced op
   *  stream announces its own minor (F0 §3.3); everything else is on the
   *  mesh's current one, which is what this peer is running. */
  function versionOf(n: NodeView): { text: string; stale: boolean } {
    if (n.stale) return { text: `0.${n.stale.minor}.x`, stale: true };
    return { text: app.status?.version ?? "—", stale: false };
  }

  /** How fresh this peer's knowledge of `n` is.
   *
   *  Gossip is a whole-view exchange, not a per-peer ping, so the honest
   *  answer for anything alive is the age of the last round — the moment
   *  this machine last heard the mesh describe itself. */
  function heartbeatOf(n: NodeView): { text: string; tone: string; why: string } {
    const state = livenessOf(n);
    if (state === "dead")
      return { text: "—", tone: "var(--ink-muted)", why: "Declared dead after a long silence." };
    if (state === "suspect")
      return {
        text: "missed",
        tone: "var(--status-warn-soft)",
        why: "Heartbeats lapsed; the mesh has marked it suspect but not yet dead.",
      };
    return {
      text: `${lastGossipSecs}s ago`,
      tone: "var(--accent-ink)",
      why: "How long since this machine last heard the mesh describe itself. Machines share what they know every couple of seconds, all at once, rather than pinging one another.",
    };
  }

  /// Bytes per second at the scale it happens to be.
  const bps = (v: number) =>
    v < 1024
      ? `${v} B/s`
      : v < 1024 * 1024
        ? `${Math.round(v / 1024)} KB/s`
        : v < 1024 * 1024 * 1024
          ? `${Math.round(v / (1024 * 1024))} MB/s`
          : `${(v / (1024 * 1024 * 1024)).toFixed(1)} GB/s`;

  const detailRows = $derived.by(() => {
    const n = selected;
    if (!n) return [] as { k: string; v: string; tone?: string; title?: string }[];
    const t = topOf(n.nodeId);
    const hb = heartbeatOf(n);
    const ver = versionOf(n);
    return [
      { k: "Address", v: n.addrs[0] ?? "not announced yet" },
      {
        k: "Node type",
        v: n.isSelf ? "director · this peer" : n.executing ? "worker" : "observer",
      },
      { k: "Platform", v: n.capacity?.arch ?? "not advertised" },
      { k: "Tags", v: n.tags.length ? n.tags.join(" · ") : "none" },
      { k: "Executes jobs", v: n.executing ? "yes" : "no — no-execute" },
      { k: "Jobs running", v: `${jobsOn(n)}${t ? ` / ${t.slots}` : ""}` },
      { k: "Last heartbeat", v: hb.text, tone: hb.tone, title: hb.why },
      { k: "Version", v: ver.text, tone: ver.stale ? "var(--status-warn-soft)" : undefined },
      { k: "Trusted since", v: ago(n.joinedAtMicros) },
      // Throughput sits with the facts rather than with the meters below,
      // and the placement is the argument: a meter implies a full mark,
      // and there is no link speed this machine can honestly report to be
      // full of.
      ...(t?.netRxBps != null || t?.netTxBps != null
        ? [{ k: "Network", v: `\u2193 ${bps(t.netRxBps ?? 0)} \u00b7 \u2191 ${bps(t.netTxBps ?? 0)}` }]
        : []),
    ];
  });

  const meters = $derived.by(() => {
    const n = selected;
    if (!n) return [] as { label: string; detail: string; pct: number; color: string }[];
    const t = topOf(n.nodeId);
    const out: { label: string; detail: string; pct: number; color: string }[] = [];

    // Utilisation first, because it is the one that moves. `load1` is a
    // one-minute average of the run queue, so a machine that just went to
    // 100% reads 0.2 per core for the first minute of it — the two are
    // shown together precisely because they disagree informatively.
    if (t?.cpuPct != null) {
      const pct = Math.round(t.cpuPct);
      out.push({
        label: "CPU",
        detail:
          t.loadPerCore != null
            ? `${pct}% busy · load ${t.loadPerCore.toFixed(2)} per core`
            : `${pct}% busy`,
        pct,
        // Indigo, like the ring on the map (UX-13): a busy processor is the
        // machine doing its job, not a fault, and red here said otherwise.
        color: "var(--hue-indigo)",
      });
    } else if (t?.loadPerCore != null) {
      const pct = Math.min(100, Math.round(t.loadPerCore * 100));
      out.push({
        label: "CPU load",
        detail: `${t.loadPerCore.toFixed(2)} per core`,
        pct,
        color: "var(--hue-indigo)",
      });
    }
    if (n.capacity && t?.memAvailableMb != null && n.capacity.memMb > 0) {
      const used = Math.max(0, n.capacity.memMb - t.memAvailableMb);
      const pct = Math.min(100, Math.round((used / n.capacity.memMb) * 100));
      out.push({
        label: "Memory",
        detail: `${(used / 1024).toFixed(1)} of ${(n.capacity.memMb / 1024).toFixed(1)} GB`,
        pct,
        color: pct > 80 ? "var(--status-warn)" : "var(--accent)",
      });
    }
    if (t?.diskUsedPct != null) {
      const pct = Math.round(t.diskUsedPct);
      out.push({
        label: "Disk",
        detail: `${pct}% of the data volume`,
        pct,
        color: pct >= 90 ? "var(--status-error)" : pct >= 80 ? "var(--status-warn)" : "var(--accent)",
      });
    }
    if (t && t.slots > 0) {
      const pct = Math.min(100, Math.round((t.running / t.slots) * 100));
      out.push({
        label: "Job slots",
        detail: `${t.running} of ${t.slots} slots`,
        pct,
        // Blue, not teal: this is headroom, not health, and the two must not
        // be read off the same colour.
        color: "var(--status-running)",
      });
    }
    return out;
  });

  /** Everything this peer knows about one node, as the record it is. */
  const definitionRecord = $derived(
    selected ? { node: selected, saturation: topOf(selected.nodeId) ?? null } : null,
  );
  const definition = $derived(definitionRecord ? JSON.stringify(definitionRecord, null, 2) : "");
  /** The same record as rows (UX-13): the tab used to be the JSON above,
   *  which had every field and let you read none of them. */
  const definitionRows = $derived(definitionRecord ? flattenRecord(definitionRecord) : []);

  async function copyDefinition() {
    try {
      await api.copyText(definition);
      app.flash("Record copied as JSON");
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }

  // -- events -----------------------------------------------------------------
  type Feed = { tone: string; text: string; meta: string; at: number; count: number };

  /** The mesh's event stream, optionally narrowed to one node. Runs of the
   *  identical event roll up: a fan-out of 64 echoes produced six rows of the
   *  same sentence and buried the one failure three rows down. */
  function feed(only: NodeView | null): Feed[] {
    const out: Feed[] = [];
    const seenGroups = new Set<string>();
    for (const j of app.jobs.slice(0, 30)) {
      if (only && j.executorName !== only.name) continue;
      if (j.fanGroup) {
        if (seenGroups.has(j.fanGroup)) continue;
        seenGroups.add(j.fanGroup);
        out.push({
          tone: "var(--status-running)",
          text: `fan-out started on ${j.fanTotal} node${j.fanTotal === 1 ? "" : "s"} — ${j.cmdPreview}`,
          meta: `${j.fanGroup.slice(0, 14)} · ${ago(j.createdAtMicros)}`,
          at: j.createdAtMicros,
          count: 1,
        });
        continue;
      }
      const who = j.executorName ?? "a node";
      const verb =
        j.state === "running" || j.state === "queued"
          ? `${who} accepted job`
          : j.state === "succeeded"
            ? `${who} finished`
            : `${who} ${j.state}`;
      out.push({
        tone:
          j.state === "failed" || j.state === "timed-out"
            ? "var(--status-error)"
            : j.state === "succeeded" || j.state === "running"
              ? "var(--status-ok)"
              : "var(--liveness-dead)",
        text: `${verb} ${j.cmdPreview}`,
        meta: `${j.shortId} · ${ago(j.createdAtMicros)}`,
        at: j.createdAtMicros,
        count: 1,
      });
    }
    for (const n of only ? [only] : members) {
      out.push({
        tone: "var(--status-ok)",
        text: `${n.name} joined ${app.status?.meshName ?? "the mesh"}`,
        meta: `${n.shortId} · ${ago(n.joinedAtMicros)}`,
        at: n.joinedAtMicros,
        count: 1,
      });
      if (n.liveness === "suspect") {
        out.push({
          tone: "var(--status-warn)",
          text: `${n.name} missed heartbeats — it isn't replying`,
          meta: "heartbeat",
          at: Date.now() * 1000,
          count: 1,
        });
      } else if (n.liveness === "dead") {
        out.push({
          tone: "var(--liveness-dead)",
          text: `${n.name} stopped answering`,
          meta: "heartbeat",
          at: Date.now() * 1000 - 1,
          count: 1,
        });
      }
      if (n.stale) {
        out.push({
          tone: "var(--status-warn)",
          text: `${n.name} is on an older version — ${n.stale.opsWithheld} change${n.stale.opsWithheld === 1 ? "" : "s"} held back until it upgrades`,
          meta: "updates",
          at: Date.now() * 1000 - 2,
          count: 1,
        });
      }
    }
    const rolled: Feed[] = [];
    for (const e of out.sort((a, b) => b.at - a.at)) {
      const prev = rolled[rolled.length - 1];
      if (prev && prev.text === e.text && prev.tone === e.tone) {
        prev.count += 1;
        continue;
      }
      rolled.push({ ...e });
    }
    return rolled;
  }

  const nodeEvents = $derived(feed(selected ?? null).slice(0, 24));

  // Capacity is its own view (2026-09-22): four gauges over three summary
  // cards, each opening its detail in a sheet. It reads `capacity:focus`
  // itself, so a deep link to one of the four retired panels still lands
  // on the thing it named.

  // ---------------------------------------------------------------------------
  // Adding a node — the drawer
  // ---------------------------------------------------------------------------
  type Discovery = "idle" | "scanning" | "candidates" | "empty";
  let drawerOpen = $state(false);
  let drawerTab = $state<"discover" | "token" | "manual" | "install">("discover");
  // Add machines over SSH: the drawer's fourth way in, and the one for a
  // machine that has nothing on it yet.
  let deployOpen = $state(false);
  let discovery = $state<Discovery>("idle");
  let candidates = $state<CandidateView[]>([]);
  let scanStarted = $state(0);
  let scanGen = 0;
  const scanElapsed = $derived(Math.min(30, Math.floor((now - scanStarted) / 1000)));
  const scanLabel = $derived(
    discovery === "idle"
      ? "discovery off"
      : discovery === "scanning"
        ? "scanning"
        : discovery === "empty"
          ? "no results"
          : `${candidates.length} found`,
  );

  async function startScan() {
    const gen = ++scanGen;
    drawerTab = "discover";
    discovery = "scanning";
    candidates = [];
    scanStarted = Date.now();
    // ~30s of discovery in 2.5s sweeps; candidates surface as they announce.
    for (let sweep = 0; sweep < 12; sweep++) {
      if (scanGen !== gen) return;
      try {
        const found = await api.discoverCandidates(2500);
        if (scanGen !== gen) return;
        for (const c of found) {
          if (!candidates.some((x) => x.nodeName === c.nodeName)) candidates = [...candidates, c];
        }
        if (candidates.length > 0) discovery = "candidates";
      } catch (e) {
        app.flash(errText(e));
        discovery = "idle";
        return;
      }
    }
    if (scanGen === gen && candidates.length === 0) discovery = "empty";
  }

  function stopScan() {
    scanGen++;
    discovery = "idle";
    candidates = [];
  }

  function closeDrawer() {
    drawerOpen = false;
    stopScan();
  }

  // -- join token -------------------------------------------------------------
  let tokenTtlMins = $state(60);
  let tokenUses = $state(1);
  let token = $state<string | null>(null);
  let tokenBusy = $state(false);
  let tokenErr = $state<string | null>(null);

  async function mintToken() {
    tokenBusy = true;
    tokenErr = null;
    try {
      token = await api.inviteCreate(tokenTtlMins * 60, tokenUses, []);
    } catch (e) {
      tokenErr = errText(e);
    } finally {
      tokenBusy = false;
    }
  }

  async function copyToken() {
    if (!token) return;
    try {
      await api.copyText(token);
      app.flash("Invite copied");
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }

  // -- manual (pair by address) -----------------------------------------------
  //
  // mDNS never leaves the local subnet, so a machine on another network — a
  // cloud VM, a box behind someone else's NAT, a multicast-hostile office LAN
  // — can never announce itself into Discover. It does not have to: the
  // approver is the side that dials.
  let manualAddr = $state("");
  let manualErr = $state<string | null>(null);

  function addrLooksValid(s: string): boolean {
    const t = s.trim();
    const cut = t.lastIndexOf(":");
    if (cut <= 0 || cut === t.length - 1) return false;
    const host = t.slice(0, cut).trim();
    const port = Number(t.slice(cut + 1));
    if (!host || host.includes(" ")) return false;
    return Number.isInteger(port) && port > 0 && port < 65536;
  }

  function pairManual() {
    const addr = manualAddr.trim();
    if (!addrLooksValid(addr)) {
      manualErr =
        "Needs host:port — the port is the one the other machine printed when it started waiting.";
      return;
    }
    manualErr = null;
    manualAddr = "";
    void startApprove([addr], addr);
  }

  // ---------------------------------------------------------------------------
  // Verify pairing
  // ---------------------------------------------------------------------------
  type Step = "contacting" | "code" | "waiting" | "success" | "rejected" | "failed" | "expired";
  type Approving = {
    step: Step;
    name: string;
    peer: string;
    code: string;
    session: number;
    codeAt: number;
    joinedId?: string;
    message?: string;
  };
  let approving = $state<Approving | null>(null);
  const codeTtl = $derived(
    approving && approving.step === "code" ? 300 - Math.floor((now - approving.codeAt) / 1000) : 300,
  );
  $effect(() => {
    if (approving?.step === "code" && codeTtl <= 0) approving = { ...approving, step: "expired" };
  });
  const kicker = $derived(
    approving?.step === "success"
      ? "pairing complete"
      : approving?.step === "rejected" || approving?.step === "failed"
        ? "pairing declined"
        : approving?.step === "expired"
          ? "pairing expired"
          : "verify pairing",
  );

  async function approveCandidate(c: CandidateView) {
    if (!c.addrs.length) return;
    await startApprove(c.addrs, c.nodeName);
  }

  async function startApprove(addrs: string[], name: string) {
    const addr = addrs[0];
    const host = addr.replace(/:\d+$/, "");
    approving = {
      step: "contacting",
      name,
      peer: name === addr ? addr : `${name} · ${host}`,
      code: "",
      session: 0,
      codeAt: 0,
    };
    try {
      await api.pairApproveStart(addrs, [], (ev: ApproveEvent) => {
        if (!approving) return;
        if (ev.type === "trying") {
          approving = { ...approving, step: "contacting", peer: `${name} · ${ev.addr.replace(/:\d+$/, "")}` };
        } else if (ev.type === "code") {
          const who = ev.joinerName || approving.name;
          approving = {
            ...approving,
            step: "code",
            name: who,
            peer: `${who} · ${host}`,
            code: ev.code,
            session: ev.session,
            codeAt: Date.now(),
          };
        } else if (ev.type === "done") {
          approving = { ...approving, step: "success", name: ev.nodeName, joinedId: ev.nodeId };
          candidates = candidates.filter((x) => x.nodeName !== ev.nodeName);
          // The new member should be in the ring and the table before the
          // dialog is dismissed, not two seconds later on the next poll.
          void app.refresh();
          selectedId = ev.nodeId;
        } else if (ev.type === "failed") {
          if (approving.step !== "rejected" && approving.step !== "expired") {
            approving = { ...approving, step: "failed", message: ev.message };
          }
        }
      });
    } catch (e) {
      approving = approving ? { ...approving, step: "failed", message: errText(e) } : null;
    }
  }

  async function decide(match: boolean) {
    if (!approving) return;
    const session = approving.session;
    approving = { ...approving, step: match ? "waiting" : "rejected" };
    try {
      await api.pairApproveDecide(session, match);
    } catch (e) {
      approving = approving ? { ...approving, step: "failed", message: errText(e) } : null;
    }
  }

  // ---------------------------------------------------------------------------
  // Header and inspector actions
  // ---------------------------------------------------------------------------
  let exporting = $state(false);

  async function copyMeshId() {
    const id = app.status?.meshId;
    if (!id) return;
    try {
      await api.copyText(id);
      app.flash("Mesh id copied");
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }

  /** A signed snapshot of replicated state, written where you choose. The
   *  same thing `meshnode backup` writes — this screen is simply where
   *  somebody is standing when they decide they want one. */
  async function exportState() {
    exporting = true;
    try {
      const out = await api.backupWrite(backupFileName(app.status?.nodeName));
      if (!out) return;
      app.flash(`Backup written to ${out.path}`);
    } catch (e) {
      app.flash(errText(e));
    } finally {
      exporting = false;
    }
  }

  // ---------------------------------------------------------------------------
  // Share this mesh (L2 P2)
  // ---------------------------------------------------------------------------
  //
  // A composed card, not a screenshot: the live topology on a dark ground,
  // the mesh's name, the totals sentence, uptime and the build — and no
  // identifiers (D-L2.3). The one line under it is the payload; every share
  // carries the product's own sentence in a user's voice.
  //
  // It asks the live canvas to redraw itself at card resolution, so it is
  // only offered while the topology is the tab on screen. Exporting a
  // diagram nobody is looking at is how you ship a picture of a mesh from
  // four minutes ago.
  let sharing = $state(false);
  async function shareMesh() {
    if (sharing || !topo) return;
    sharing = true;
    try {
      const name = app.status?.meshName ?? "mesh";
      const png = await renderMeshSnapshot(
        // Called inside the card's dark-token block: the canvas reads its
        // palette when it paints, so this comes back as a dark diagram
        // without the window ever having been dark.
        (w, h) => topo!.snapshotCanvas(w, h),
        {
          meshName: name,
          // `Totals::strip()`, verbatim. The card must not re-compose the
          // sentence the band above it is showing.
          strip: spec?.strip ?? "",
          coverage: spec?.coverage ?? null,
          uptimeSecs: app.status?.uptimeSecs ?? 0,
          version: app.status?.version ?? "",
        },
        brandSvg,
      );
      const out = await api.snapshotSave(
        `${name}-${new Date().toISOString().slice(0, 10)}.png`,
        png,
      );
      if (!out) return;
      app.flash(`Wrote ${out}`);
    } catch (e) {
      app.flash(errText(e));
    } finally {
      sharing = false;
    }
  }

  let nodeMenu = $state(false);

  function runOn(n: NodeView) {
    sessionStorage.setItem("run:selector", `name=${n.name}`);
    app.go("run");
  }

  function openInNodes(n: NodeView) {
    sessionStorage.setItem("nodes:select", n.nodeId);
    app.go("nodes");
  }

  // A drain with an end (`meshnode drain --for`): a maintenance window
  // rather than a shutdown. The choices are the ones people actually
  // reach for; anything else is `--until` on the command line.
  const pauseChoices = [
    { label: "30 min", secs: 30 * 60 },
    { label: "1 h", secs: 60 * 60 },
    { label: "2 h", secs: 2 * 60 * 60 },
    { label: "4 h", secs: 4 * 60 * 60 },
    { label: "until 06:00", secs: -1 },
  ];
  let pauseSecs = $state(2 * 60 * 60);
  let pauseReason = $state("");
  let pausing = $state(false);
  /** Unix seconds for the next 06:00 on this machine's clock. */
  function nextSix(): number {
    const d = new Date();
    d.setSeconds(0, 0);
    if (d.getHours() >= 6) d.setDate(d.getDate() + 1);
    d.setHours(6, 0, 0, 0);
    return Math.floor(d.getTime() / 1000);
  }
  async function pauseSelf() {
    if (pausing) return;
    pausing = true;
    try {
      const until = pauseSecs === -1 ? nextSix() : Math.floor(Date.now() / 1000) + pauseSecs;
      const said = await api.drainTimed(until, pauseReason.trim());
      app.flash(said);
      pauseReason = "";
      void app.refresh();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      pausing = false;
    }
  }
  async function liftPause() {
    if (pausing) return;
    pausing = true;
    try {
      app.flash(await api.drainTimed(null, ""));
      void app.refresh();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      pausing = false;
    }
  }
  const pausedUntil = $derived.by(() => {
    const t = app.status?.drainUntilSecs;
    if (t == null) return null;
    const d = new Date(t * 1000);
    const today = new Date();
    const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    return d.toDateString() === today.toDateString() ? hm : `${d.toDateString().slice(0, 3)} ${hm}`;
  });

  // The toolbar renders whatever the open screen hands it, and clears it
  // when that screen goes away (`chrome.setActions`).
  $effect(() => chrome.setActions(actions));

  let draining = $state(false);
  async function drainSelf() {
    if (draining) return;
    draining = true;
    try {
      await api.drain(true, () => {});
      app.markEngineStopped();
      app.flash("This peer drained and left the rotation");
    } catch (e) {
      app.flash(errText(e));
    } finally {
      draining = false;
    }
  }
</script>

<!-- The Add-node drawer is a dialog: Escape closes it, the same as the
     scrim (`use:dialog` on the drawer, DIR-12). The attention list's Escape
     is here, and takes the key first when it is open. -->
<svelte:window
  onpointerdown={(e) => {
    const t = e.target as Element | null;
    if (attentionOpen && !t?.closest?.("[data-menu='attention']")) attentionOpen = false;
    if (legendOpen && !t?.closest?.("[data-menu='legend']")) legendOpen = false;
  }}
  onkeydown={(e) => {
    if (attentionOpen && e.key === "Escape") {
      e.preventDefault();
      attentionOpen = false;
    }
  }}
/>

<!-- The topology legend's items, drawn inline or in the popover. -->
{#snippet legendItems()}
  <!-- Colour is whether a machine is answering, the ring is its load
       (UX-13) — the legend says it in that order: the four states, then
       the one quantity. -->
  <span
    class="inline-flex items-center gap-[6px] text-[11px] whitespace-nowrap"
    style="color: var(--ink-3);"
    title="Answering: the mesh hears from this machine on schedule."
  >
    <span class="h-2.5 w-2.5 rounded-full" style="background: var(--liveness-alive);"></span>answering
  </span>
  <span
    class="inline-flex items-center gap-[6px] text-[11px] whitespace-nowrap"
    style="color: var(--ink-3);"
    title="Heartbeats have lapsed and the mesh has marked this machine suspect. Its ring keeps its last reading — the last reading is still the last reading."
  >
    <span
      class="h-2.5 w-2.5 rounded-full border-[1.5px] border-dashed"
      style="border-color: var(--status-warn); background: var(--liveness-suspect);"
    ></span>not replying
  </span>
  <span
    class="inline-flex items-center gap-[6px] text-[11px] whitespace-nowrap"
    style="color: var(--ink-3);"
    title="The owner has told this machine not to take work. It is present, it is talking to the mesh, it is reporting load — it is just not a place jobs land."
  >
    <span
      class="h-2.5 w-2.5 rounded-full border-[3px]"
      style="border-color: var(--liveness-alive); background: var(--surface-graph);"
    ></span>not taking work
  </span>
  <span
    class="inline-flex items-center gap-[6px] text-[11px] whitespace-nowrap"
    style="color: var(--ink-3);"
    title="Declared dead by the mesh. Drawn as a hole rather than a grey disc, because a disc is another colour of present."
  >
    <span
      class="h-2.5 w-2.5 rounded-full border-[1.5px]"
      style="border-color: var(--liveness-dead);"
    ></span>no answer
  </span>
  <span
    class="inline-flex items-center gap-[6px] text-[11px] whitespace-nowrap"
    style="color: var(--ink-3);"
    title="The ring around a machine is its processor load right now, filled clockwise from the top. Repainted every poll. No ring means no reading yet; on a fleet too dense for rings, a brighter disc is a busier machine."
  >
    <Ring
      size={12}
      thickness={2}
      total={100}
      arcs={[{ value: 72, color: "var(--hue-indigo)" }]}
      track="color-mix(in srgb, var(--hue-indigo) 20%, transparent)"
    />load
  </span>
  <!-- The one claim this canvas makes that needs a caveat attached to it,
       so it carries one — in words, with the count in the tooltip. -->
  {#if tagLinks > 0}
    <span
      class="inline-flex items-center gap-[6px] text-[11px]"
      style="color: var(--ink-3);"
      title={completeMesh
        ? `Every machine knows every other and talks to a fresh few each round. This mesh is small enough to draw whole, so it is — each of the ${tagLinks.toLocaleString()} lines is a pair that can talk. Lines are coloured by neighbourhood.`
        : `Machines pick new partners to talk to every round and never publish who, so these ${tagLinks.toLocaleString()} lines are what the fleet does say about itself: a shared tag. A tag with more than eight machines in it is drawn as a chain through them rather than as every pair, so the lines are a sample. A machine whose tags are unique to it is drawn beside a few others, because every machine knows every other.`}
    >
      <span class="h-px w-4" style="background: var(--graph-edge-alive);"></span>{completeMesh
        ? "lines: every pair can talk"
        : "lines: a shared tag"}
    </span>
  {/if}
{/snippet}

<!-- ═══════════════ the toolbar's actions ═══════════════ -->
<!-- The page's two actions live in the toolbar, next to the breadcrumb —
     the same place, at the same size, on every screen — so a panel never
     opens with a row of buttons of its own above the thing it is for. -->
{#snippet actions()}
  <button class="btn btn-secondary btn-md" disabled={exporting} onclick={exportState}>
    <Icon name="save" size={13} />
    {exporting ? "Backing up…" : "Back up"}
  </button>
  <button class="btn btn-primary btn-md" onclick={() => (drawerOpen = true)}>
    <Icon name="plus" size={13} /> Add machine
  </button>
{/snippet}

<!-- ═══════════════ the mesh's vitals ═══════════════ -->
<!-- One strip, one fact per cell. See `vitalsStrip`. -->
{#snippet vitals()}
  <!-- Four facts about the mesh as chips under the Fleet panel's health
     ring (2026-09-22) — the ones that are an answer rather than a
     measurement take the teal. -->
  {#each vitalsStrip as row (row.k)}
    <span class="fact" class:ok={row.ok}>{row.k}<b use:roll={row.v}>{row.v}</b></span>
  {/each}
{/snippet}

<div class="relative flex min-h-0 flex-1 flex-col">
  <!-- ═══════════════ page header ═══════════════ -->
  <!-- The mesh's name at 30px with its state beside it as a capsule, one
       line of plain words under it, and the spec sheet right-aligned. The
       three bands this replaces — a title row, a 52px strip of eight
       vitals and a one-line spec — were 150px of chrome above the panel a
       person actually came for. The vitals are two grouped surfaces in
       the Fleet panel now, where they can be read as sentences; the one
       that answers "is it well" is the capsule up here. -->
  <div class="flex flex-none items-start gap-6 px-7 pt-1 pb-5">
    <div class="min-w-0">
      <div class="flex items-center gap-3">
        <h1 class="pagetitle truncate">{app.status?.meshName ?? "—"}</h1>
        {#if attention.length > 0 && !app.engineStopped && !app.status?.draining}
          <!-- A count is a promise of a list, so the capsule opens one. -->
          <div class="relative flex-none" data-menu="attention">
            <button
              class="capsule capsule-btn"
              style={`--tone: ${health.tone}; --tone-ink: ${health.ink};`}
              aria-expanded={attentionOpen}
              aria-haspopup="true"
              onclick={() => (attentionOpen = !attentionOpen)}
            >
              {health.label}
              <Icon name="chevron-down" size={12} />
            </button>
            {#if attentionOpen}
              <div
                class="absolute top-[32px] left-0 z-40 w-[380px] overflow-hidden rounded-lg bg-surface py-1.5"
                style="box-shadow: var(--sh-pop); transform-origin: 20px 0;"
                role="menu"
                use:popMenu
              >
                <div class="microlabel px-3.5 pt-1 pb-1.5">What needs attention</div>
                {#each attention as item (item.key)}
                  <button
                    class="attn-row"
                    role="menuitem"
                    onclick={() => openAttention(item)}
                  >
                    <span
                      class="mark-sm mt-[5px] flex-none"
                      style={`--tone: ${item.tone === "error" ? "var(--status-error)" : "var(--status-warn)"};`}
                    ></span>
                    <span class="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink-2">
                      <span class="font-semibold text-ink">{item.subject}</span>
                      {item.problem}
                    </span>
                    <span class="flex-none text-ink-faint"><Icon name="chevron-right" size={13} /></span>
                  </button>
                {/each}
              </div>
            {/if}
          </div>
        {:else}
          <span class="capsule flex-none" style={`--tone: ${health.tone}; --tone-ink: ${health.ink};`}>{health.label}</span>
        {/if}
      </div>
      <div class="mt-[7px] flex flex-wrap items-center gap-3">
        <span class="pagesub">Live from this peer — updates arrive on their own.</span>
        <!-- The id is a string you copy, not a word you read, so it stops
             competing with the name for the same reading and becomes a
             chip with the copy button inside it. -->
        {#if app.status?.meshId}
          <span class="idchip flex-none">
            {app.status.meshId}
            <button title="Copy mesh id" onclick={copyMeshId}><Icon name="copy" size={12} /></button>
          </span>
        {/if}
      </div>
    </div>

    <!-- L2 P1: what the mesh *is*, as against what it is doing. Two lines
         at the top right, in the same place on every panel. "Poolable
         disk", never "storage": disk pools across a mesh and RAM does not,
         and this product's credibility is built on refusing that
         inference. -->
    {#if spec}
      <div class="ml-auto hidden max-w-[330px] flex-none pt-1 text-right lg:block">
        <div class="text-sm leading-[1.5] tabular-nums" style="color: var(--ink-5);">{spec.strip}</div>
        {#if spec.coverage}
          <!-- Shown only when the figures are a floor. A mesh where one
               machine has not advertised its hardware has an honest answer
               and a flattering one, and the flattering one is what stops a
               headline number being believed the first time anybody checks
               it against the rack. -->
          <div class="mt-[3px] text-[11px]" style="color: var(--ink-muted);">{spec.coverage}</div>
        {/if}
      </div>
    {/if}
  </div>

  <!-- ═══════════════ the document ═══════════════ -->
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#if tab === "topology"}
      <!-- workspace: canvas + inspector -->
      <!-- The workspace takes the whole document. It used to stop at 402px
           and leave the rest of the viewport as an empty field — the one
           thing a buyer reads as "unfinished" before reading a word. -->
      <div
        class="flex items-stretch bg-surface"
        style="height: 100%; min-height: 402px; border-bottom: 1px solid var(--edge);"
        use:reveal={{ y: 6, duration: DUR.slow }}
      >
        <div
          class="relative min-w-0 flex-1"
          bind:clientWidth={canvasW}
          style="background-color: var(--surface-graph); background-image: radial-gradient(var(--graph-dots) 1px, transparent 1.3px); background-size: 20px 20px; background-position: 10px 10px; border-right: 1px solid var(--edge);"
        >
          <div class="cluster absolute top-3 right-[14px] z-[2]">
            <!-- L2 P2. On the canvas rather than in the page header,
                 because it shares *this* diagram and the button should sit
                 with the thing it acts on — with the canvas's other
                 controls since UX-13, so the legend bar is only legend. -->
            <button
              class="h-7 px-2 text-[11px]"
              disabled={sharing || !spec}
              title="Save a PNG of this mesh — the diagram, the totals and nothing that identifies a machine"
              onclick={shareMesh}
            >
              {sharing ? "Rendering…" : "Share"}
            </button>
            <button
                class="h-7 px-2 text-[11px]"
                aria-pressed={showLinks}
                style={showLinks ? "" : "color: var(--ink-muted);"}
                title="Draw the peer links, or just the machines. A big fleet reads better as a cloud than as a ball of wool — and the shape of the cloud is still the shape of the mesh."
                onclick={() => (showLinks = !showLinks)}
              >
                Links
              </button>
              <button
                class="h-7 px-2 text-[11px]"
                title="Nudge every machine and let the simulation find the seats again. A layout you can disturb is a layout you can believe."
                onclick={() => topo?.resettle()}
              >
                Re-settle
              </button>
            <button class="h-7 w-7" title="Zoom in" onclick={() => topo?.zoomBy(1.25)}>
              <Icon name="zoom-in" size={14} />
            </button>
            <button class="h-7 w-7" title="Zoom out" onclick={() => topo?.zoomBy(1 / 1.25)}>
              <Icon name="zoom-out" size={14} />
            </button>
            <button class="h-7 w-7" title="Fit to the pane" onclick={() => topo?.fit()}>
              <Icon name="fit" size={14} />
            </button>
          </div>

          <!-- The graph lives under the floating controls: a machine rarely
               reaches that corner, and a control is a control. -->
          <div class="absolute inset-0 px-3 pt-2 pb-[34px]">
            <Topology
              bind:this={topo}
              nodes={meshNodes}
              {showLinks}
              {selectedId}
              onselect={select}
              onstats={(s) => {
                tagLinks = s.links;
                completeMesh = s.complete;
              }}
            />
          </div>

          {#if others.length === 0}
            <div class="pointer-events-none absolute inset-x-0 bottom-[54px] text-center">
              <div class="text-md font-semibold text-ink">
                {app.status?.meshName} has one machine
              </div>
              <div class="mt-1 text-sm" style="color: var(--ink-5);">
                A one-machine mesh is a complete product — pairing a second makes it a fleet.
              </div>
              <!-- The sentence used to be the whole nudge, with nothing to
                   press (UX-10). -->
              <button class="btn btn-primary btn-md pointer-events-auto mt-3" onclick={() => (drawerOpen = true)}>
                <Icon name="plus" size={13} /> Add a machine
              </button>
            </div>
          {/if}

          <!-- legend -->
          <!-- Below ~700px of canvas the legend is a popover (UX-11): inline,
               at the 980px minimum window, it wrapped to four lines and ran
               into the inspector. -->
          <div
            class="absolute inset-x-0 bottom-0 flex items-center gap-[14px] bg-surface px-4 py-2"
            style="border-top: 1px solid var(--edge-pre);"
          >
            {#if legendTight}
              <div class="relative" data-menu="legend">
                <button
                  class="btn btn-secondary h-7 flex-none px-[10px] text-[11px]"
                  aria-expanded={legendOpen}
                  onclick={() => (legendOpen = !legendOpen)}
                >
                  Legend
                </button>
                {#if legendOpen}
                  <div
                    class="absolute bottom-[36px] left-0 z-10 flex w-[220px] flex-col items-start gap-2 rounded-lg bg-surface p-3"
                    style="box-shadow: var(--sh-pop); transform-origin: 12px 100%;"
                    use:popMenu
                  >
                    {@render legendItems()}
                  </div>
                {/if}
              </div>
            {:else}
              {@render legendItems()}
            {/if}
            {#if canvasW >= 900}
              <span class="num ml-auto flex-none text-[10.5px] whitespace-nowrap" style="color: var(--ink-muted);">
                last heard {lastGossipSecs}s ago
              </span>
            {/if}
          </div>
        </div>

        <!-- ═══════════════ inspector ═══════════════ -->
        {#if inspectorOpen}
          <div class="flex w-[344px] flex-none flex-col bg-surface" in:slideX={{ x: 18, duration: DUR.slow }}>
            <div class="tabrow flex-none px-4" role="tablist" aria-label="Node inspector" style="border-bottom: 1px solid var(--edge);" use:tabline>
              {#each [["details", "Details"], ["events", "Events"], ["definition", "Definition"]] as [id, label] (id)}
                <button
                  class="tab-sm"
                  role="tab"
                  aria-selected={inspectorTab === id}
                  onclick={() => (inspectorTab = id as typeof inspectorTab)}>{label}</button
                >
              {/each}
              <button
                class="iconbtn ml-auto h-[22px] w-[22px] rounded-xs"
                title="Collapse the inspector"
                onclick={() => (inspectorOpen = false)}
              >
                <span style="color: var(--ink-faint);"><Icon name="chevron-right" size={15} /></span>
              </button>
            </div>

            {#if selected}
              <div class="min-h-0 flex-1 overflow-y-auto">
                <div class="px-4 pt-[15px] pb-3" style="border-bottom: 1px solid var(--edge-inner);">
                  <div class="flex items-center gap-2">
                    <span class="mark" style={`--tone:${selTone.dot};`}></span>
                    <span class="mono truncate text-md font-semibold tracking-[-.01em] text-ink" use:roll={selected.nodeId}>
                      {selected.name}
                    </span>
                    <span
                      class="statuspill flex-none"
                      style={`--tone-bg:${selTone.bg}; --tone-fg:${selTone.fg};`}
                    >
                      {selTone.label}
                    </span>
                  </div>
                  <div class="mono mt-[5px] truncate text-[11px]" style="color: var(--ink-muted);">
                    {selected.nodeId}
                  </div>
                </div>

                {#if inspectorTab === "details"}
                  <div use:staggerIn={{ selector: ".defrow", each: 16, y: 5, key: selected.nodeId }}>
                    {#each detailRows as row (row.k)}
                      <div class="defrow" title={row.title}>
                        <span class="k">{row.k}</span>
                        <span class="v" style={row.tone ? `color:${row.tone};` : ""} use:roll={row.v}>{row.v}</span>
                      </div>
                    {/each}
                  </div>

                  {#if meters.length}
                    <div class="px-4 pt-[14px] pb-1">
                      <div class="microlabel">Capacity</div>
                      <div class="mt-[10px] flex flex-col gap-[11px]">
                        {#each meters as m (m.label)}
                          <div>
                            <div class="flex items-baseline justify-between">
                              <span class="text-xs" style="color: var(--ink-3);">{m.label}</span>
                              <span class="num text-[11px]" style="color: var(--ink-5);" use:roll={m.detail}>{m.detail}</span>
                            </div>
                            <div
                              class="mt-[5px] h-[5px] overflow-hidden rounded-full"
                              style="background: var(--surface-3);"
                            >
                              <div
                                class="h-full rounded-full"
                                style={`background:${m.color}; transition: background-color var(--dur-slower) var(--ease);`}
                                use:meter={{ pct: m.pct }}
                              ></div>
                            </div>
                          </div>
                        {/each}
                      </div>
                    </div>
                  {:else}
                    <div class="px-4 pt-[14px] pb-1 text-xs" style="color: var(--ink-5);">
                      This peer has not advertised hardware yet, so there is nothing to meter.
                    </div>
                  {/if}

                  {#if selected.isSelf && !app.engineStopped}
                    <!-- A drain with an end: refuse new work for a while, keep what is
                         running, rejoin on your own. The engine stays up (idea 20). -->
                    <div class="px-4 pt-3">
                      {#if pausedUntil}
                        <div class="flex items-center gap-2 rounded-md px-3 py-2 text-[12px]" style="background: var(--surface-3); color: var(--status-warn-ink);">
                          <span class="min-w-0 flex-1 truncate">
                            Paused until {pausedUntil}{app.status?.drainReason ? ` — ${app.status.drainReason}` : ""}: running jobs finish, new work goes elsewhere.
                          </span>
                          <button class="btn btn-secondary h-[26px] text-[12px]" disabled={pausing} onclick={liftPause}>
                            {pausing ? "…" : "Take work again"}
                          </button>
                        </div>
                      {:else}
                        <div class="flex items-center gap-2">
                          <select
                            bind:value={pauseSecs}
                            class="select h-[30px] rounded-md bg-surface px-2 text-[12px] outline-none"
                            style="box-shadow: var(--sh-control);"
                            title="Refuse new work for this long, then rejoin on your own — a maintenance window, not a shutdown"
                          >
                            {#each pauseChoices as c (c.secs)}
                              <option value={c.secs}>{c.label}</option>
                            {/each}
                          </select>
                          <input
                            bind:value={pauseReason}
                            placeholder="Reason (shown in status)"
                            spellcheck="false"
                            class="h-[30px] min-w-0 flex-1 rounded-md bg-surface px-2 text-[12px] outline-none"
                            style="box-shadow: var(--sh-control);"
                          />
                          <button
                            class="btn btn-secondary h-[30px] text-[12px]"
                            disabled={pausing || draining}
                            title="Like `meshnode drain --for`: new work goes elsewhere until then, running jobs finish, and this peer rejoins on its own"
                            onclick={pauseSelf}
                          >
                            {pausing ? "Pausing…" : "Pause"}
                          </button>
                        </div>
                      {/if}
                    </div>
                  {/if}

                  <div class="relative flex gap-2 p-4">
                    <button
                      class="btn btn-secondary h-[30px] flex-1 text-[12px]"
                      disabled={!selected.isSelf || draining || app.engineStopped}
                      title={selected.isSelf
                        ? "Finish accepted work, refuse new work, and leave the rotation"
                        : "Draining is a machine's own decision — run it from Director on that machine, or `meshnode drain`."}
                      onclick={drainSelf}
                    >
                      {draining ? "Draining…" : "Drain"}
                    </button>
                    <button
                      class="btn btn-secondary h-[30px] flex-1 text-[12px]"
                      onclick={() => selected && runOn(selected)}
                    >
                      Run job
                    </button>
                    <button
                      class="btn btn-secondary h-[30px] w-[30px] flex-none p-0"
                      title="More"
                      onclick={() => (nodeMenu = !nodeMenu)}
                    >
                      <Icon name="more" size={14} />
                    </button>
                    {#if nodeMenu}
                      <!-- svelte-ignore a11y_no_static_element_interactions -->
                      <div
                        class="absolute right-4 bottom-[52px] z-10 w-[196px] overflow-hidden rounded-md bg-surface py-1"
                        style="border: 1px solid var(--edge-card); box-shadow: 0 12px 28px -14px rgba(18,24,38,.4); transform-origin: 90% 100%;"
                        use:popMenu
                        onmouseleave={() => (nodeMenu = false)}
                      >
                        <button
                          class="flex w-full items-center gap-2 px-3 py-[7px] text-left text-[12px] text-ink-2 hover:bg-canvas"
                          onclick={() => {
                            nodeMenu = false;
                            if (selected) openInNodes(selected);
                          }}
                        >
                          <Icon name="nodes" size={13} /> Open in Nodes
                        </button>
                        <button
                          class="flex w-full items-center gap-2 px-3 py-[7px] text-left text-[12px] text-ink-2 hover:bg-canvas"
                          onclick={async () => {
                            nodeMenu = false;
                            if (!selected) return;
                            try {
                              await api.copyText(selected.nodeId);
                              app.flash("Node id copied");
                            } catch {
                              app.flash("Could not reach the clipboard");
                            }
                          }}
                        >
                          <Icon name="copy" size={13} /> Copy node id
                        </button>
                        <button
                          class="flex w-full items-center gap-2 px-3 py-[7px] text-left text-[12px] text-ink-2 hover:bg-canvas"
                          onclick={() => {
                            nodeMenu = false;
                            if (selected) openInNodes(selected);
                          }}
                        >
                          <Icon name="revoke" size={13} /> Revoke…
                        </button>
                      </div>
                    {/if}
                  </div>
                {:else if inspectorTab === "events"}
                  {#each nodeEvents as ev, i (i)}
                    <div
                      class="flex gap-[10px] px-4 py-[10px]"
                      style="border-bottom: 1px solid var(--edge-row);"
                      use:reveal={{ y: 5, delay: Math.min(i, 12) * 22 }}
                    >
                      <span class="mark-sm mt-[5px]" style={`--tone:${ev.tone};`}></span>
                      <div class="min-w-0">
                        <div class="text-[12px] leading-[1.45]" style="color: var(--ink-2);">{ev.text}</div>
                        <div class="mono mt-[2px] flex items-center gap-1.5 text-[10.5px]" style="color: var(--ink-muted);">
                          <span>{ev.meta}</span>
                          {#if ev.count > 1}
                            <span
                              class="num rounded-full px-1.5 font-semibold"
                              style="background: var(--surface-2); color: var(--ink-5);">×{ev.count}</span
                            >
                          {/if}
                        </div>
                      </div>
                    </div>
                  {:else}
                    <div class="px-4 py-4 text-sm" style="color: var(--ink-5);">
                      Nothing has happened on this node yet.
                    </div>
                  {/each}
                {:else}
                  <div class="flex items-center gap-2 px-4 py-2.5" style="border-bottom: 1px solid var(--edge-row);">
                    <span class="text-xs text-ink-5">Everything this machine knows about {selected?.name}</span>
                    <button class="btn btn-secondary ml-auto h-7 flex-none px-[10px] text-[11px]" onclick={copyDefinition}>
                      <Icon name="copy" size={12} /> Copy JSON
                    </button>
                  </div>
                  {#each definitionRows as row (row.key)}
                    <div class="defrow" style="--key-w: 132px; padding: 6px 16px;">
                      <span class="k mono" style="font-size: 10.5px;">{row.key}</span>
                      <span class="v mono">{row.value}</span>
                    </div>
                  {/each}
                {/if}
              </div>
            {:else}
              <div class="grid flex-1 place-items-center p-6 text-center text-sm" style="color: var(--ink-5);">
                Click a node to inspect it.
              </div>
            {/if}
          </div>
        {:else}
          <button
            class="flex w-[34px] flex-none flex-col items-center gap-2 bg-surface pt-3 transition-colors hover:bg-canvas"
            style="border-left: 1px solid var(--edge);"
            title="Open the inspector"
            in:fadeIn={{ duration: DUR.base }}
            onclick={() => (inspectorOpen = true)}
          >
            <span style="color: var(--ink-faint);"><Icon name="chevron-left" size={14} /></span>
            <span
              class="mono text-[10.5px] tracking-[.08em] uppercase"
              style="color: var(--ink-muted); writing-mode: vertical-rl;">Inspector</span
            >
          </button>
        {/if}
      </div>
    {/if}

    <!-- Fleet and Capacity's views were whole sections before the Focus
         surface; they render here unchanged, inside the standard page
         padding they were built for. -->
    {#if tab === "nodes"}
      <!-- The mesh's vitals belong to the mesh, and the capacity tiles
           belong to the fleet — but they are read in one glance, so they
           are laid out in one column. `NodesView` takes them as a snippet
           and drops them between its metric tiles and its node table
           rather than this file rendering them above the whole panel:
           metrics, then vitals, then the machines is the order they answer
           in. -->
      <div class="px-7 pt-2 pb-7" use:reveal={{ y: 8 }}>
        <!-- A new mesh's first three steps (UX-10), above everything else
             on the page a new user lands on. -->
        <GetStarted onAddMachine={() => (drawerOpen = true)} />
        <NodesView {vitals} updatesTab={false} />
      </div>
    {/if}

    {#if tab === "placement"}
      <div class="px-7 pt-2 pb-7" use:reveal={{ y: 8 }}><PlacementView /></div>
    {/if}

    <!-- Capacity (2026-09-14): four panels that were four rail rows, read
         top to bottom in the order someone debugging a job that will not
         place reads them — how much machine, can it run this, what is
         holding the ports, what is holding the storage. -->
    {#if tab === "capacity"}
      <div class="px-7 pt-2 pb-7" use:reveal={{ y: 8 }}><CapacityView /></div>
    {/if}

    <!-- Updates renders bare: it opens with its own headline — "4 of 11
         nodes are behind" — and a section head above that would say the
         word "Updates" twice, once in the rail row that is already lit. -->
    {#if tab === "updates"}
      <div class="px-7 pt-2 pb-7" use:reveal={{ y: 8 }}><Software /></div>
    {/if}
  </div>

  <!-- ═══════════════ pairing drawer ═══════════════ -->
  {#if drawerOpen}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="absolute inset-0 z-20"
      style="background: rgba(12,16,24,.32);"
      role="presentation"
      transition:fadeIn={{ duration: DUR.base }}
      onclick={closeDrawer}
    ></div>
    <div
      class="absolute inset-y-0 right-0 z-[21] flex w-[424px] flex-col bg-surface"
      style="border-left: 1px solid var(--edge-drawer); box-shadow: var(--sh-drawer);"
      in:slideX={{ x: 36, duration: DUR.slow }}
      out:slideX={{ x: 24, duration: DUR.base }}
      role="dialog"
      aria-modal="true"
      aria-label="Add a machine"
      use:dialog={{ onescape: closeDrawer }}
    >
      <div class="flex flex-none items-start gap-3 px-[18px] pt-4 pb-[14px]" style="border-bottom: 1px solid var(--edge);">
        <div>
          <h2 class="m-0 text-[16px] font-bold tracking-[-.02em] text-ink">Add a machine</h2>
          <!-- One line per way in, so the sentence under the title is
               true for the tab that is open rather than for one of them. -->
          <p class="mt-1 mb-0 max-w-[40ch] text-sm leading-[1.5]" style="color: var(--ink-5); text-wrap: pretty;" use:roll={drawerTab}>
            {drawerTab === "install"
              ? "Put meshnode on a machine and it announces itself here."
              : drawerTab === "discover"
                ? "Machines announcing on this network can join without an invite."
                : drawerTab === "token"
                  ? "For a machine on another network."
                  : "Dial a machine that is already waiting to pair."}
          </p>
        </div>
        <button class="iconbtn ml-auto h-[26px] w-[26px] flex-none" title="Close" onclick={closeDrawer}>
          <span style="color: var(--ink-3);"><Icon name="close" size={14} /></span>
        </button>
      </div>

      <div class="tabrow flex-none px-[18px]" role="tablist" aria-label="How to add a machine" style="border-bottom: 1px solid var(--edge);" use:tabline>
        {#each [["install", "Install"], ["discover", "Discover"], ["token", "Invite"], ["manual", "Manual"]] as [id, label] (id)}
          <button
            class="tab-sm"
            role="tab"
            style="margin-right: 18px;"
            aria-selected={drawerTab === id}
            onclick={() => (drawerTab = id as typeof drawerTab)}>{label}</button
          >
        {/each}
        {#if drawerTab === "discover"}
          <span class="mono ml-auto rounded-xs px-2 py-[2px] text-[10.5px] whitespace-nowrap" style="background: var(--surface-2); color: var(--ink-5);">
            {scanLabel}
          </span>
        {/if}
      </div>

      <div class="min-h-0 flex-1 overflow-y-auto px-[18px] py-4">
        {#if drawerTab === "install"}
          <div
            class="rounded-lg px-[14px] py-[13px] text-sm leading-[1.5]"
            style="border: 1px dashed var(--edge-dashed); color: var(--ink-5);"
          >
            For machines with nothing on them yet. Give Director SSH — a password, a key, or your agent —
            and it installs meshnode, sets up the service, pairs, and starts it. You confirm each code;
            SSH is never needed again.
          </div>
          <button
            class="btn btn-primary mt-3 h-[38px] w-full text-base"
            onclick={() => {
              closeDrawer();
              deployOpen = true;
            }}
          >
            <Icon name="plus" size={14} /> Add machines over SSH
          </button>
          <p class="mt-2.5 mb-0 text-[11.5px] leading-[1.5]" style="color: var(--ink-5);">
            From a terminal, the same thing is <span class="mono" style="color: var(--accent-ink);">meshnode adopt ops@rack-{"{1..10}"}</span>.
          </p>
        {:else if drawerTab === "discover"}
          {#if discovery === "idle"}
            <div
              class="rounded-lg px-[14px] py-[13px] text-sm leading-[1.5]"
              style="border: 1px dashed var(--edge-dashed); color: var(--ink-5);"
            >
              Discovery is off. Scanning listens on the local subnet for about 30 seconds and never
              leaves this network.
            </div>
            <button class="btn btn-primary mt-3 h-[38px] w-full text-base" onclick={startScan}>
              Scan this network
            </button>
          {:else if discovery === "scanning"}
            <div
              class="relative grid h-[132px] place-items-center overflow-hidden rounded-lg"
              style="background: var(--surface-graph); border: 1px solid var(--edge-pre);"
            >
              <div
                class="absolute h-[210px] w-[210px] rounded-full"
                style="border: 1px solid color-mix(in srgb, var(--accent) 40%, transparent); animation: mw-radar var(--motion-radar);"
              ></div>
              <div
                class="absolute h-[210px] w-[210px] rounded-full"
                style="border: 1px solid color-mix(in srgb, var(--accent) 30%, transparent); animation: mw-radar var(--motion-radar); animation-delay: 1.3s;"
              ></div>
              <div class="relative text-center">
                <div class="mono text-sm" style="color: var(--accent-ink);">listening…</div>
                <div class="num mt-1 text-xs" style="color: var(--ink-muted);">{scanElapsed}s of 30s</div>
              </div>
            </div>
            <button class="btn btn-secondary mt-3 h-[38px] w-full text-base" onclick={stopScan}>
              Stop scanning
            </button>
          {:else if discovery === "candidates"}
            <div class="microlabel">Announcing on this subnet</div>
            <div class="mt-2.5 flex flex-col gap-2">
              {#each candidates as c (c.nodeName)}
                <div
                  class="fade-in flex items-center gap-[11px] rounded-lg px-3 py-2.5"
                  style="border: 1px solid var(--edge);"
                >
                  <span
                    class="mono grid h-[30px] w-[30px] flex-none place-items-center rounded-md text-xs font-semibold"
                    style="background: var(--surface-3); color: var(--ink-3);"
                  >
                    {initialsOf(c.nodeName)}
                  </span>
                  <div class="min-w-0">
                    <div class="mono truncate text-sm font-semibold text-ink">{c.nodeName}</div>
                    <div class="mono truncate text-[10.5px]" style="color: var(--ink-muted);">
                      {c.addrs[0]?.replace(/:\d+$/, "") ?? ""}{c.platform ? ` · ${c.platform}` : ""}
                    </div>
                  </div>
                  <button
                    class="btn btn-primary ml-auto h-7 flex-none px-3 text-[12px]"
                    onclick={() => approveCandidate(c)}
                  >
                    Pair
                  </button>
                </div>
              {/each}
            </div>
            <div
              class="mt-3.5 rounded-lg px-3 py-[11px] text-[12px] leading-[1.5]"
              style="background: var(--surface-graph); border: 1px solid var(--edge-pre); color: var(--ink-5);"
            >
              Not listed? Run <span class="mono" style="color: var(--accent-ink);">meshnode join --pair</span>
              on the other machine.
            </div>
            <button class="btn btn-secondary mt-3 h-[38px] w-full text-base" onclick={stopScan}>Done</button>
          {:else}
            <div
              class="rounded-lg px-4 py-[22px] text-center"
              style="border: 1px dashed var(--edge-dashed);"
            >
              <div class="text-[13px] font-semibold text-ink">Nothing announced</div>
              <div class="mt-[5px] text-[12px] leading-[1.5]" style="color: var(--ink-5);">
                Run <span class="mono" style="color: var(--accent-ink);">meshnode join --pair</span> on the
                other machine, then scan again.
              </div>
            </div>
            <button class="btn btn-secondary mt-3 h-[38px] w-full text-base" onclick={startScan}>
              Scan again
            </button>
            <!-- The likeliest reason for silence is that the machine is not on
                 this subnet at all, so the recovery is here rather than buried. -->
            <button
              class="mt-2.5 h-8 w-full text-sm font-semibold"
              style="color: var(--ink-5);"
              onclick={() => (drawerTab = "manual")}
            >
              Not on this network? Pair by address
            </button>
          {/if}
        {:else if drawerTab === "token"}
          <div
            class="rounded-lg px-[14px] py-[13px] text-sm leading-[1.5]"
            style="border: 1px dashed var(--edge-dashed); color: var(--ink-5);"
          >
            An invite lets a machine you cannot reach add itself. It is single-use by default and it
            expires. It carries no permissions, but anyone holding it before then can join a machine —
            send it the way you would send a door code.
          </div>
          <div class="mt-3.5 grid grid-cols-2 gap-3">
            <label class="block">
              <span class="mb-1.5 block text-xs" style="color: var(--ink-5);">Expires in (minutes)</span>
              <input class="field field-sunken num h-[34px] w-full px-3 text-sm outline-none" type="number" min="1" bind:value={tokenTtlMins} />
            </label>
            <label class="block">
              <span class="mb-1.5 block text-xs" style="color: var(--ink-5);">Uses</span>
              <input class="field field-sunken num h-[34px] w-full px-3 text-sm outline-none" type="number" min="1" bind:value={tokenUses} />
            </label>
          </div>
          <button class="btn btn-primary mt-3 h-[38px] w-full text-base" disabled={tokenBusy} onclick={mintToken}>
            {tokenBusy ? "Creating…" : token ? "Create another" : "Create an invite"}
          </button>
          {#if tokenErr}
            <p class="selectable mt-2.5 mb-0 text-xs leading-[1.5]" style="color: var(--status-error-ink);">{tokenErr}</p>
          {/if}
          {#if token}
            <div class="fade-in mt-3.5">
              <div class="microlabel">Hand this to the other machine</div>
              <pre class="pre mt-2 text-[11px]">{token}</pre>
              <div class="mt-2 flex gap-2">
                <button class="btn btn-secondary h-8 flex-1 text-sm" onclick={copyToken}>
                  <Icon name="copy" size={13} /> Copy invite
                </button>
              </div>
              <p class="mt-2.5 mb-0 text-[11.5px] leading-[1.5]" style="color: var(--ink-5);">
                On that machine: <span class="mono" style="color: var(--accent-ink);">meshnode join &lt;invite&gt;</span>
              </p>
            </div>
          {/if}
        {:else}
          <div
            class="flex items-center gap-1 rounded-lg px-[14px] py-[13px] text-sm leading-[1.5]"
            style="border: 1px dashed var(--edge-dashed); color: var(--ink-5);"
          >
            For a machine that cannot announce itself here — this side dials it instead.
            <Hint
              label="Why some machines never appear"
              text="Discovery never leaves the local subnet, so a cloud VM or a box behind someone else's NAT can never announce itself. It does not have to — this side dials, and the same six digits appear on both screens."
            />
          </div>
          <label class="mt-3.5 block">
            <span class="mb-1.5 block text-xs" style="color: var(--ink-5);">
              Address of a machine waiting in <span class="mono" style="color: var(--ink-3);">meshnode join --pair</span>
            </span>
            <div class="flex gap-2">
              <input
                bind:value={manualAddr}
                spellcheck="false"
                placeholder="203.0.113.5:41234"
                onkeydown={(e) => {
                  if (e.key === "Enter") pairManual();
                }}
                class="field field-sunken mono h-[38px] min-w-0 flex-1 px-3 text-sm outline-none placeholder:font-sans"
              />
              <button class="btn btn-primary h-[38px] shrink-0 px-4 text-base" disabled={!manualAddr.trim()} onclick={pairManual}>
                Pair
              </button>
            </div>
          </label>
          {#if manualErr}
            <p class="selectable mt-2 mb-0 text-xs leading-[1.5]" style="color: var(--status-error-ink);">{manualErr}</p>
          {:else}
            <p class="mt-2 mb-0 max-w-[46ch] text-xs leading-[1.5]" style="color: var(--ink-5);">
              It prints that port when it starts waiting, and it is a new one each time.
            </p>
          {/if}
        {/if}
      </div>
    </div>
  {/if}
</div>

<DeploySheet open={deployOpen} onclose={() => (deployOpen = false)} />

<!-- ═══════════════ verify pairing ═══════════════ -->
<Modal
  open={approving !== null}
  {kicker}
  peer={approving?.peer ?? ""}
  onclose={() => {
    if (approving?.step === "code") void decide(false);
    else approving = null;
  }}
>
  {#if approving?.step === "contacting"}
    <div class="px-[22px] pt-7 pb-8 text-center">
      <div class="flex justify-center">
        <Spinner size={32} thickness={2.75} color="var(--accent-bright)" track="color-mix(in srgb, var(--accent-bright) 18%, transparent)" />
      </div>
      <div class="mt-4 text-md font-semibold">Contacting node…</div>
      <div class="mt-1.5 text-sm" style="color: var(--ink-code-3);">Establishing an authenticated channel.</div>
    </div>
  {:else if approving?.step === "code"}
    <div class="px-[22px] pt-3.5 pb-[22px] text-center">
      <h3 class="m-0 text-[20px] font-bold tracking-[-.02em]">Do these codes match?</h3>
      <p class="mx-auto mt-[7px] mb-0 max-w-[34ch] text-sm leading-[1.55]" style="color: var(--ink-code-3); text-wrap: pretty;">
        Read it aloud with the person at <span class="mono" style="color: #cfe0da;">{approving.name}</span>. Both
        sides must see the same six digits — or the same two words.
      </p>
      <div class="mt-5 mb-1.5">
        <!--
          The QR starts closed here. This is the *approver's* screen, on a
          laptop somebody is already sitting in front of; the machine that
          needs photographing is the headless one at the other end. It is
          still one click away, because sometimes the headless one is this
          one.
        -->
        <PairCode code={approving.code} />
      </div>
      <div class="mono text-[11px]" style={`color:${codeTtl <= 10 ? "var(--status-warn)" : "var(--ink-code-4)"};`}>
        expires in {fmtCountdown(codeTtl)}
      </div>
      <div class="mt-5 grid gap-[9px]" style="grid-template-columns: 1fr 1.25fr;">
        <button
          class="h-10 rounded-lg text-base font-semibold transition-colors"
          style="border: 1px solid rgba(224,82,82,.35); color: #f2a3a3; background: transparent;"
          onclick={() => decide(false)}
        >
          They differ — reject
        </button>
        <button
          class="h-10 rounded-lg text-base font-bold transition-colors"
          style="background: var(--accent-bright); color: var(--accent-on-bright);"
          onclick={() => decide(true)}
        >
          They match — approve
        </button>
      </div>
    </div>
  {:else if approving?.step === "waiting"}
    <div class="px-[22px] pt-7 pb-8 text-center">
      <div class="flex justify-center">
        <Spinner size={32} thickness={2.75} color="var(--accent-bright)" track="color-mix(in srgb, var(--accent-bright) 18%, transparent)" />
      </div>
      <div class="mt-4 text-md font-semibold">Waiting for the other side</div>
      <div class="mt-1.5 text-sm" style="color: var(--ink-code-3);">
        <span class="mono">{approving.name}</span> must confirm the same code.
      </div>
    </div>
  {:else if approving?.step === "success"}
    <div class="px-[22px] pt-[26px] pb-7 text-center">
      <div
        class="mx-auto grid h-11 w-11 place-items-center rounded-full"
        style="background: color-mix(in srgb, var(--accent-bright) 14%, transparent);"
      >
        <span class="inline-grid" style="color: var(--accent-bright);"><Icon name="check" size={22} /></span>
      </div>
      <div class="mt-[15px] text-lg font-bold tracking-[-.02em]">
        {approving.name} joined {app.status?.meshName}
      </div>
      <div class="mono mt-1.5 text-xs" style="color: var(--ink-code-3);">
        {approving.joinedId ? approving.joinedId.slice(0, 15) : ""} · member
      </div>
      <button
        class="mt-5 h-10 w-full rounded-lg text-base font-semibold transition-colors"
        style="background: color-mix(in srgb, var(--accent-bright) 14%, transparent); color: var(--accent-bright);"
        onclick={() => (approving = null)}
      >
        Done
      </button>
    </div>
  {:else if approving?.step === "expired"}
    <div class="px-[22px] pt-[26px] pb-7 text-center">
      <div
        class="mx-auto grid h-11 w-11 place-items-center rounded-full text-xl"
        style="background: rgba(217,119,6,.16); color: var(--status-warn);"
      >
        •
      </div>
      <div class="mt-[15px] text-lg font-bold tracking-[-.02em]">Code expired</div>
      <div class="mt-1.5 text-sm leading-[1.5]" style="color: var(--ink-code-3);">
        Nothing was granted. Start the pairing again on both machines.
      </div>
      <button
        class="mt-5 h-10 w-full rounded-lg text-base font-semibold transition-colors"
        style="background: color-mix(in srgb, var(--accent-bright) 14%, transparent); color: var(--accent-bright);"
        onclick={() => (approving = null)}
      >
        Start over
      </button>
    </div>
  {:else if approving}
    <div class="px-[22px] pt-[26px] pb-7 text-center">
      <div class="mx-auto grid h-11 w-11 place-items-center rounded-full text-xl" style="background: rgba(224,82,82,.14); color: #f2a3a3;">
        ✕
      </div>
      <div class="mt-[15px] text-lg font-bold tracking-[-.02em]">
        {approving.step === "rejected" ? "Pairing rejected" : "Pairing failed"}
      </div>
      <div class="mt-1.5 text-sm leading-[1.5]" style="color: var(--ink-code-3);">
        {approving.step === "rejected"
          ? "No trust was granted. If the codes differed, someone else may be announcing on this network."
          : (approving.message ?? "The exchange did not complete.")}
      </div>
      <button
        class="mt-5 h-10 w-full rounded-lg text-base font-semibold"
        style="border: 1px solid rgba(147,161,179,.3); color: #cdd7e2; background: transparent;"
        onclick={() => (approving = null)}
      >
        Close
      </button>
    </div>
  {/if}
</Modal>

<style>
  /* The health capsule, when it has a list behind it (UX-3). The capsule's
     own look, plus the affordance of a control: a chevron, a pointer, and a
     ground that deepens under the cursor. */
  .capsule-btn {
    cursor: pointer;
    padding-right: 7px;
    transition: background var(--dur-fast) var(--ease);
  }
  .capsule-btn:hover,
  .capsule-btn[aria-expanded="true"] {
    background: color-mix(in srgb, var(--tone) 26%, transparent);
  }
  .attn-row {
    display: flex;
    width: 100%;
    align-items: flex-start;
    gap: 10px;
    padding: 7px 14px;
    text-align: left;
    transition: background var(--dur-fast) var(--ease);
  }
  .attn-row:hover {
    background: var(--bg-canvas);
  }

  /* Capacity's jump strip (2026-09-14): four views stacked on one panel,
     and a strip that stays at the top of the scroll so any of them is one
     press away. Bled to the panel's edges so the hairline under it reads as
     the top of the scroll region, not as a rule inside a card. */
</style>
