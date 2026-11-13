<script lang="ts">
  // Nodes view on the design contract: 48px rows, hover/selected states,
  // capacity columns from `meshnode top` (slots / running / queued /
  // saturation), a drill-in dialog with a reachability ping, and the
  // deliberately heavy revoke modal (typed node name, the one place a
  // destructive button is filled).
  import { rowAction } from "../lib/rowaction";
  import { dialog } from "../lib/dialog";
  import { api, errText, fmtSizeMb, type NodeView, type PingView, type SessionView } from "../lib/api";
  import type { GangRowView } from "../lib/bindings/GangRowView";
  import type { DeviceView } from "../lib/bindings/DeviceView";
  import { app, ago, ui, uptime } from "../lib/state.svelte";
  import { vendorLabel } from "../lib/vendor";
  import Dot from "../lib/components/Dot.svelte";
  import Pill from "../lib/components/Pill.svelte";
  import Hint from "../lib/components/Hint.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Tag from "../lib/components/Tag.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Ring from "../lib/components/Ring.svelte";
  import Sheet from "../lib/components/Sheet.svelte";
  import FactsModal from "../lib/components/FactsModal.svelte";
  import SyncCell from "../lib/components/SyncCell.svelte";
  import Terminal from "../lib/components/LazyTerminal.svelte";
  import Software from "../lib/components/Software.svelte";
  import { outdatedCount } from "../lib/versions";
  import DeploySheet from "../lib/components/DeploySheet.svelte";
  import { portal } from "../lib/portal";
  import { staggerIn, meter } from "../lib/motion";
  import { poll } from "../lib/poll";
  import { drainOf, drainWords, isDrainTag } from "../lib/drain";
  import {
    FLEET_CHIPS,
    chipMatches,
    nextSort,
    SORT_STARTS_DESC,
    searchMatches,
    selectorFor,
    sortFleet,
    type FleetChip,
    type FleetSort,
    type FleetSortKey,
  } from "../lib/fleet";
  import { untrack, type Snippet } from "svelte";

  /**
   * The mesh's own vitals, handed down by `Mesh.svelte` (6c).
   *
   * This panel is the Fleet page of the Mesh section, and the eight vitals
   * read as part of it — but they are facts about the *mesh*, and Mesh
   * owns them. So they arrive as a snippet and land between the capacity
   * tiles and the node table: how much machine, then how it is, then which
   * machines. Absent when this view is opened on its own under the legacy
   * surface, where there is nothing above it to have produced them.
   */
  /**
   * Does this view still carry the Updates tab?
   *
   * Under the Focus surface it does not. Updates is a panel of Mesh with
   * its own row in the rail (`chrome/nav.ts`), so this view is the Fleet
   * panel and only that — the rollout was never a *mode* of the node
   * table, and a switcher here would now be a second way to reach a row
   * the sidebar is already showing. The legacy surface has no such row
   * — Nodes is a whole section there — so the tab stays for it.
   */
  let { vitals, updatesTab = true }: { vitals?: Snippet; updatesTab?: boolean } = $props();

  // Two questions about the same machines, and they do not fit in one
  // table. **Fleet** is the live one — who is up, what is it carrying, ping
  // it, drain it. **Updates** is the slow one: what build each node
  // announced (`cap.ver`, M24 §8) and the health-gated rollout that changes
  // it. It moved here from Settings because a rollout is not a preference,
  // and because "what is that node running" is a column question.
  type Tab = "fleet" | "software";
  let picked = $state<Tab>("fleet");
  const tab = $derived<Tab>(updatesTab ? picked : "fleet");
  const behind = $derived(outdatedCount(app.nodes, app.about?.bundledMeshnode));

  $effect(() => {
    const t = sessionStorage.getItem("nodes:tab");
    if (t === "fleet" || t === "software") {
      sessionStorage.removeItem("nodes:tab");
      picked = t;
    }
  });

  // Add machines: the one door onto a fleet that does not exist yet. A
  // palette jump (`nodes:open=deploy`) opens it on arrival.
  let deployOpen = $state(false);
  $effect(() => {
    if (sessionStorage.getItem("nodes:open") === "deploy") {
      sessionStorage.removeItem("nodes:open");
      deployOpen = true;
    }
  });

  // The drill-in is a modal, not a side rail: a rail squeezed the table
  // into folding its own columns, and the machine's details read better as
  // one dialog laid out in two columns than as a 320px strip of stacked
  // rows. `selectedId` outlives `infoOpen` so the row stays marked after
  // the dialog closes, and so the revoke confirmation still has a subject.
  let selectedId = $state<string | null>(null);
  let infoOpen = $state(false);
  const selected = $derived(app.nodes.find((n) => n.nodeId === selectedId) ?? null);
  /** `top` by node id, built once per poll rather than scanned per row
   *  (M-25) — `topOf` is called several times for every row in the table. */
  const topIndex = $derived(new Map(app.top.map((t) => [t.nodeId, t])));
  const topOf = $derived((n: NodeView) => topIndex.get(n.nodeId) ?? null);
  /** Running jobs per executor name, folded once — the fallback for a peer
   *  that has not reported saturation yet. */
  const runningByExecutor = $derived.by(() => {
    const m = new Map<string, number>();
    for (const j of app.jobs) {
      if (j.state !== "running" || !j.executorName) continue;
      m.set(j.executorName, (m.get(j.executorName) ?? 0) + 1);
    }
    return m;
  });

  // -- reservations in flight (C4 §14.3) -----------------------------------------
  //
  // The cluster view's answer to "why is that node refusing work when it has
  // free slots": something is holding it for a gang that has not started
  // yet. Shown only while there are some, the same rule the held figure in
  // the Work column and the live-sessions card follow — a permanently empty
  // panel is a panel people learn to scroll past.
  let gangs = $state<GangRowView[]>([]);
  // A mesh mid-restart is not an error worth a banner here, so the
  // rejection is left to `poll`'s own swallow (M-24).
  $effect(() => poll(async () => (gangs = await api.gangs(false)), 2000));

  // -- accelerators (C5 §14.3) ---------------------------------------------------
  //
  // Device columns in Nodes, and a panel listing what the mesh actually
  // has. Both follow the rule every conditional column in this view
  // follows: a mesh with no accelerators never sees them, because a
  // permanently empty column trains people to stop reading the row.
  //
  // Answered from replicated state, so this is the whole mesh's hardware
  // whichever node Director happens to be talking to — the property the
  // inventory entity exists for.
  let devices = $state<DeviceView[]>([]);
  $effect(() => poll(async () => (devices = await api.devices()), 5000));
  const anyDevices = $derived(devices.length > 0);

  // -- revoked rows --------------------------------------------------------------
  //
  // A revoked node is not a node any more — its key can never rejoin, and it
  // will never report load, run a job, or answer a ping. Leaving it in the
  // table as a dimmed row means the list of "machines I have" and the list
  // of "machines that exist" stop being the same list, and every count on
  // the screen has to be read twice. So it leaves the table.
  //
  // It does not leave the *record*: revocation is a governance event people
  // audit, so the footer says how many there are and puts them back on one
  // click rather than sending anyone to the ledger for a name.
  let showRevoked = $state(false);
  const revokedCount = $derived(app.nodes.filter((n) => n.revoked).length);
  const rows = $derived(showRevoked ? app.nodes : app.nodes.filter((n) => !n.revoked));

  // -- finding a machine in a big fleet (UX-8) -----------------------------------
  //
  // At sixty-two machines the table was a page over 4,000px tall, with no
  // search, no filter, no sort and no paging, and Enterprise starts at
  // fifty. A search box, the status chips a person actually filters by, a
  // sortable header, and fifty rows at a time. The rules are in
  // `lib/fleet.ts`, where they are tested; "Needs attention" is the same
  // list the mesh's health capsule counts.
  let query = $state("");
  let chip = $state<FleetChip>("all");
  let sort = $state<FleetSort>({ key: "default", desc: false });
  const PAGE = 50;
  let shown = $state(PAGE);
  const chipCounts = $derived(
    Object.fromEntries(
      FLEET_CHIPS.map((c) => [c.id, rows.filter((n) => chipMatches(c.id, n)).length]),
    ) as Record<FleetChip, number>,
  );
  const filtered = $derived(
    sortFleet(
      rows.filter((n) => chipMatches(chip, n) && searchMatches(query, n)),
      sort,
      (n) => topIndex.get(n.nodeId) ?? null,
    ),
  );
  const visible = $derived(filtered.slice(0, shown));
  const filtering = $derived(chip !== "all" || query.trim() !== "");
  // A new filter starts from the first page: "show more" was an answer to
  // the list that was on screen, not to this one.
  $effect(() => {
    void query;
    void chip;
    untrack(() => (shown = PAGE));
  });
  // A chip that has emptied under you (the last quiet machine answered)
  // lets go, rather than leaving a filter nobody can see the point of.
  $effect(() => {
    if (chip !== "all" && chipCounts[chip] === 0) chip = "all";
  });

  function sortBy(key: FleetSortKey) {
    sort = nextSort(sort, key);
  }

  // -- acting on several machines at once (UX-8) ---------------------------------
  //
  // Per-machine actions were the only kind, so labelling a rack or holding
  // back ten machines was ten dialogs. A checkbox column, and a bar with the
  // actions that make sense for a set: run something on them, tag them,
  // stop or resume sending them work. Each still goes to the engine one
  // signed op at a time, and a partial failure says which machines and why.
  let chosen = $state(new Set<string>());
  $effect(() => {
    const live = new Set(rows.map((n) => n.nodeId));
    if ([...chosen].some((id) => !live.has(id))) {
      chosen = new Set([...chosen].filter((id) => live.has(id)));
    }
  });
  /** The chosen machines that can still be acted on. */
  const chosenNodes = $derived(rows.filter((n) => chosen.has(n.nodeId) && !n.revoked && !n.left));
  const allChosen = $derived(filtered.length > 0 && filtered.every((n) => chosen.has(n.nodeId)));
  function toggleChosen(id: string) {
    const next = new Set(chosen);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    chosen = next;
  }
  function toggleAllChosen() {
    chosen = allChosen ? new Set() : new Set(filtered.map((n) => n.nodeId));
  }

  let bulkBusy = $state(false);
  let bulkErr = $state<string | null>(null);
  let bulkTagOpen = $state(false);
  let bulkTagDraft = $state("");

  async function eachChosen(act: (n: NodeView) => Promise<unknown>, said: (ok: number) => string) {
    bulkBusy = true;
    bulkErr = null;
    const failed: string[] = [];
    let ok = 0;
    for (const n of chosenNodes) {
      try {
        await act(n);
        ok++;
      } catch (e) {
        failed.push(`${n.name}: ${errText(e)}`);
      }
    }
    await app.refresh();
    bulkBusy = false;
    if (failed.length) {
      bulkErr = `${failed.length} of ${ok + failed.length} did not take — ${failed.slice(0, 3).join(" · ")}${failed.length > 3 ? " · …" : ""}`;
    }
    if (ok) app.flash(said(ok));
    return failed.length === 0;
  }

  function runOnChosen() {
    sessionStorage.setItem("run:selector", selectorFor(chosenNodes.map((n) => n.name)));
    app.go("run");
  }

  async function tagChosen() {
    const wanted = bulkTagDraft
      .split(/[,\s]+/)
      .map((t) => t.trim())
      .filter(Boolean);
    if (wanted.length === 0) return;
    const done = await eachChosen(
      async (n) => {
        const fresh = wanted.filter((t) => !n.tags.includes(t));
        if (fresh.length) await api.nodeTagsAssign(n.nodeId, [...n.assignedTags, ...fresh]);
      },
      (k) => `${k} machine${k === 1 ? "" : "s"} tagged ${wanted.join(", ")}`,
    );
    if (done) {
      bulkTagDraft = "";
      bulkTagOpen = false;
    }
  }

  function contributeChosen(allowed: boolean) {
    void eachChosen(
      (n) => api.nodeContributeSet(n.nodeId, allowed, allowed ? "" : "held back from Director"),
      (k) => (allowed ? `${k} machine${k === 1 ? "" : "s"} may take work again` : `${k} machine${k === 1 ? "" : "s"} will not be given work`),
    );
  }

  /**
   * A table wider than its card scrolls inside it, and nothing on screen
   * said so: the last columns simply were not there. This marks the edge
   * that has more behind it, and the stylesheet fades it.
   */
  function scrollHint(el: HTMLElement) {
    const update = () => {
      const more = el.scrollWidth - el.clientWidth - el.scrollLeft > 2;
      if (more) el.setAttribute("data-more-right", "");
      else el.removeAttribute("data-more-right");
    };
    update();
    el.addEventListener("scroll", update, { passive: true });
    const ro = new ResizeObserver(update);
    ro.observe(el);
    const table = el.querySelector("table");
    if (table) ro.observe(table);
    return {
      destroy() {
        el.removeEventListener("scroll", update);
        ro.disconnect();
      },
    };
  }

  // Selecting a revoked node, then hiding revoked nodes, would leave the
  // detail dialog open on a row that is no longer on screen.
  $effect(() => {
    if (!showRevoked && selected?.revoked) {
      selectedId = null;
      infoOpen = false;
    }
  });

  $effect(() => {
    // Re-read on every navigation, not only at mount: the attention list
    // on this same screen asks for a machine while Fleet is already open,
    // and `app.go` to where you already are changes nothing else.
    void app.deepLink;
    const pre = sessionStorage.getItem("nodes:select");
    if (pre) {
      sessionStorage.removeItem("nodes:select");
      selectedId = pre;
      infoOpen = true;
      picked = "fleet";
    }
  });

  /** MB as the scale operators read: 125 GB, 3.7 TB — the spec line's rule. */
  const fmtSize = fmtSizeMb;

  /** Any node advertising hardware at all (MASTERPLAN M9 §5.1)? On a mesh
      where nobody does, the capacity column is not shown empty — it is not
      shown. */
  const anyCapacity = $derived(app.nodes.some((n) => n.capacity !== null));

  /** What this machine is running, from `top` — which the engine folds
      per node — with the bounded job page as the fallback for a peer that
      has not reported yet. Mesh.svelte reads it the same way (**H-1**). */
  const runningOn = $derived((n: NodeView) => {
    const t = topIndex.get(n.nodeId);
    if (t) return t.running;
    return runningByExecutor.get(n.name) ?? 0;
  });

  /** The fleet: every machine still in the mesh. Its size is the one
   *  machine total this screen uses (UX-12). */
  const fleetNow = $derived(app.nodes.filter((n) => !n.revoked && !n.left));
  const liveCount = $derived(fleetNow.length);
  const answering = $derived(fleetNow.filter((n) => n.liveness === "alive").length);

  /** The silicon vendor's own colour, where the app has one.
   *
   *  A weak hint and nothing more: it tints one small mono word in one
   *  column so a mixed fleet can be scanned by make without reading, and it
   *  falls back to ink for anything unrecognised rather than assigning a
   *  colour to a string the app has never seen. */
  function vendorFill(v: string): string {
    const k = v.toLowerCase();
    if (k.includes("amd") || k.includes("radeon")) return "var(--vendor-amd)";
    if (k.includes("nvidia")) return "var(--vendor-nvidia)";
    if (k.includes("intel")) return "var(--vendor-intel)";
    return "var(--ink-faint)";
  }

  const totals = $derived.by(() => {
    const exec = app.top.filter((t) => !t.noExecute);
    const slots = exec.reduce((a, t) => a + t.slots, 0);
    const running = exec.reduce((a, t) => a + t.running, 0);
    const queued = exec.reduce((a, t) => a + t.queued, 0);
    return { slots, running, queued, pct: slots ? Math.min(100, Math.round((running / slots) * 100)) : 0, executors: exec.length };
  });

  /** How long this peer has been up, for the "idle since" line. Short
   *  form, because it is the tail of a sentence and not a figure. */
  const uptimeShort = $derived(uptime(app.status?.uptimeSecs ?? 0) + " uptime");

  // -- ping -----------------------------------------------------------------------
  let pingBusy = $state(false);
  let ping = $state<{ node: string; result: PingView | null; error: string | null } | null>(null);
  // By node id, never by an address off the announced list: that list is a
  // set of candidates the daemon races, and its loopback entry points at
  // *this* machine. Only the daemon knows which candidate (or relay) works.
  async function doPing(n: NodeView) {
    if (n.addrs.length === 0) {
      ping = { node: n.nodeId, result: null, error: "no dialable address announced yet" };
      return;
    }
    pingBusy = true;
    ping = null;
    try {
      const r = await api.pingNode(n.nodeId);
      ping = { node: n.nodeId, result: r, error: null };
    } catch (e) {
      ping = { node: n.nodeId, result: null, error: errText(e) };
    } finally {
      pingBusy = false;
    }
  }

  // -- address repair (M7) ----------------------------------------------------------
  //
  // The founder is the only node that may rewrite another's address record,
  // and the one situation that needs it is the one where this view has gone
  // entirely grey: every machine was renumbered at once, so every recorded
  // address is stale in every direction, nobody can dial anybody, and the
  // self-healing path — each node re-announcing itself on boot — has
  // nowhere to start. One repaired record breaks the symmetry; the rest of
  // the mesh converges on its own.
  //
  // It lives inside the Addresses block rather than behind a button in the
  // footer because it is an edit to the thing it sits under, and because
  // the moment somebody wants it is the moment they are staring at an
  // address they can see is wrong.
  let addrOpen = $state(false);
  let addrDraft = $state("");
  let addrBusy = $state(false);
  let addrErr = $state<string | null>(null);

  // Shown only where it can work. On a Director that did not found this
  // mesh the control would refuse every time, and a control that always
  // fails is worse than no control — so that case gets the founder's name
  // instead, which is the thing you actually need to know.
  const canRepair = $derived(
    (app.status?.isFounder ?? false) &&
      selected !== null &&
      !selected.isSelf &&
      !selected.revoked &&
      !selected.left,
  );
  const founderName = $derived(
    app.nodes.find((n) => n.nodeId === app.status?.founderNode)?.name ?? "the founder",
  );

  function openAddrEdit() {
    if (!selected) return;
    addrDraft = selected.addrs.join(", ");
    addrErr = null;
    addrOpen = true;
  }

  async function saveAddrs() {
    if (!selected) return;
    const addrs = addrDraft
      .split(/[,\n]/)
      .map((a) => a.trim())
      .filter((a) => a.length > 0);
    if (addrs.length === 0) {
      addrErr = "give at least one host:port address";
      return;
    }
    addrBusy = true;
    addrErr = null;
    try {
      await api.nodeAddrRepair(selected.nodeId, addrs);
      app.flash(`${selected.name} recorded at ${addrs.join(", ")} — dialling it now`);
      addrOpen = false;
      await app.refresh();
    } catch (e) {
      addrErr = errText(e);
    } finally {
      addrBusy = false;
    }
  }

  // A different node selected is a different subject; a half-typed address
  // for the last one must not survive into it.
  $effect(() => {
    void selectedId;
    addrOpen = false;
    addrErr = null;
  });

  // -- revoke -----------------------------------------------------------------------
  let revokeOpen = $state(false);
  let revokeReason = $state("revoked from Director");
  let revokeTyped = $state("");
  let revokeBusy = $state(false);
  let revokeErr = $state<string | null>(null);
  const revokeArmed = $derived(selected !== null && revokeTyped.trim() === selected.name);
  const isAdmin = $derived(app.whoami?.isFounder || app.whoami?.roles.includes("admin"));

  // -- quarantine: isolate a *suspected* node, reversibly (E2.4) ----------------
  //
  // Deliberately not folded into the contribute control below. That one is
  // a capacity decision and a node held back by it still gives an on-call
  // engineer a terminal; this one says nobody goes near the machine. Two
  // different questions, two different controls, and the second is the one
  // somebody reaches for at 3am.
  let quarantineBusy = $state(false);
  let quarantineErr = $state<string | null>(null);
  let quarantineReason = $state("");
  let quarantineOpen = $state(false);
  async function setQuarantine(node: NodeView, on: boolean) {
    if (on && !quarantineReason.trim()) {
      quarantineErr = "Say why. The next person reads it before deciding whether to lift it.";
      return;
    }
    quarantineBusy = true;
    quarantineErr = null;
    try {
      await api.nodeQuarantineSet(node.nodeId, on, on ? quarantineReason.trim() : "");
      await app.refresh();
      app.flash(
        on
          ? `${node.name} is quarantined — no work, no terminals`
          : `${node.name} is no longer quarantined`,
      );
      quarantineReason = "";
      quarantineOpen = false;
    } catch (e) {
      quarantineErr = errText(e);
    } finally {
      quarantineBusy = false;
    }
  }

  // -- tags an admin assigns to a node, in the mesh ------------------------------
  //
  // A node's tag list has two halves and only one of them is anybody
  // else's to write. What the machine announces — its hardware, its
  // config's `tags`, `draining`, its build — comes off that machine and
  // nowhere else. What an *operator* means by `prod` or `dc=east` is a
  // division of their own estate, and before this it was still only
  // settable by being on the box: labelling a fleet was an SSH loop, and a
  // machine rebuilt overnight came back unlabelled.
  //
  // So this control writes the assigned half, and only ever that half. It
  // is a total list rather than a delta — the engine converges one signed
  // list per node under one clock — which is why both add and remove
  // rebuild `assignedTags` and send the whole thing.
  let assignAdding = $state(false);
  let assignDraft = $state("");
  let assignBusy = $state(false);
  let assignErr = $state<string | null>(null);

  async function assignTags(node: NodeView, tags: string[], said: string) {
    assignBusy = true;
    assignErr = null;
    try {
      await api.nodeTagsAssign(node.nodeId, tags);
      await app.refresh();
      app.flash(said);
      assignDraft = "";
      assignAdding = false;
    } catch (e) {
      assignErr = errText(e);
    } finally {
      assignBusy = false;
    }
  }

  function addTag(node: NodeView) {
    // Comma- and space-separated, because an operator labelling a machine
    // has more than one thing to say about it and typing `prod, dc=east`
    // is what they will do whether or not it is supported.
    const wanted = assignDraft
      .split(/[,\s]+/)
      .map((t) => t.trim())
      .filter(Boolean);
    if (wanted.length === 0) return;
    const already = wanted.filter((t) => node.tags.includes(t));
    const fresh = wanted.filter((t) => !node.tags.includes(t));
    if (fresh.length === 0) {
      assignErr = `${node.name} already has ${already.join(", ")}.`;
      return;
    }
    void assignTags(
      node,
      [...node.assignedTags, ...fresh],
      `${node.name} tagged ${fresh.join(", ")}`,
    );
  }

  function removeTag(node: NodeView, tag: string) {
    void assignTags(
      node,
      node.assignedTags.filter((t) => t !== tag),
      `${tag} removed from ${node.name}`,
    );
  }

  // -- draining another node, without stopping it (Phase C, E9/U9) -------------
  //
  // `meshnode nodes drain`. Read back off the node's tags with the same rule
  // the reconciler uses, so the panel says what the mesh is doing. Only an
  // assigned drain can be lifted from here: a node shutting itself down is
  // that machine's own decision.
  const DRAIN_DEADLINES = [
    { secs: 1800, label: "within 30 minutes" },
    { secs: 3600, label: "within an hour" },
    { secs: 14400, label: "within 4 hours" },
    { secs: -1, label: "with no deadline" },
  ];
  let drainOpen = $state(false);
  let drainBusy = $state(false);
  let drainErr = $state<string | null>(null);
  let drainDeadline = $state(3600);
  let drainKeepSystem = $state(false);
  const selectedDrain = $derived(selected ? drainOf(selected.tags) : null);
  const drainAssigned = $derived(selected ? selected.assignedTags.some(isDrainTag) : false);
  async function setDrain(node: NodeView, deadlineSecs: number, keepSystem: boolean, lift: boolean) {
    drainBusy = true;
    drainErr = null;
    try {
      await api.nodeDrainSet(node.nodeId, deadlineSecs < 0 ? null : deadlineSecs, keepSystem, lift);
      await app.refresh();
      app.flash(lift ? `${node.name} takes new work again` : `${node.name} is draining — its instances are moving`);
      drainOpen = false;
    } catch (e) {
      drainErr = errText(e);
    } finally {
      drainBusy = false;
    }
  }

  // -- the owner's veto over one node (`OpBody::NodeExecutionSet`) ---------------
  let contributeBusy = $state(false);
  let contributeErr = $state<string | null>(null);
  async function setContributes(node: NodeView, allowed: boolean) {
    contributeBusy = true;
    contributeErr = null;
    try {
      await api.nodeContributeSet(
        node.nodeId,
        allowed,
        allowed ? "" : "held back from Director",
      );
      await app.refresh();
      app.flash(
        allowed
          ? `${node.name} may run work for this mesh`
          : `${node.name} will not be given work`,
      );
    } catch (e) {
      contributeErr = errText(e);
    } finally {
      contributeBusy = false;
    }
  }

  async function doRevoke() {
    if (!selected || !revokeArmed) return;
    revokeBusy = true;
    revokeErr = null;
    try {
      await api.nodeRevoke(selected.nodeId, revokeReason.trim() || "revoked from Director");
      app.flash(`${selected.name} revoked — its key can never rejoin`);
      revokeOpen = false;
      revokeTyped = "";
      selectedId = null;
      await app.refresh();
    } catch (e) {
      revokeErr = errText(e);
    } finally {
      revokeBusy = false;
    }
  }

  function runOn(n: NodeView) {
    sessionStorage.setItem("run:selector", `name=${n.name}`);
    app.go("run");
  }

  // -- facts (M18 §4.10 P6) -----------------------------------------------------
  // A button per row rather than only on the drill-in: "what *is* that box"
  // is a question people ask about a row they are already looking at, and
  // making them select it first turns one click into two.
  let factsFor = $state<string | null>(null);
  function showFacts(n: NodeView, e?: MouseEvent) {
    e?.stopPropagation();
    factsFor = n.name;
  }

  // -- the fabric (M17 §3.12 P6) -------------------------------------------------
  //
  // Three verbs on the drill-in, and they are the answer to "I can run a
  // command on that box, why can't I get a shell on it?". Every one of them
  // is authorized on the *far* node against the same grant `Run here…`
  // needs — so a button that appears is not a promise, and a refusal comes
  // back as the far node's own sentence.

  /** The node the terminal panel is open on, or null. */
  let terminalFor = $state<string | null>(null);
  let terminalReason = $state("");
  let terminalKey = $state(0);

  // The reason is asked for *before* the session opens, because it is
  // covered by the signature the far node verifies — it is the operator's
  // own statement, not something Director could fill in for them.
  let reasonOpen = $state(false);
  let reasonDraft = $state("");
  let reasonFor = $state<NodeView | null>(null);

  function askReason(n: NodeView) {
    reasonFor = n;
    reasonDraft = "";
    reasonOpen = true;
  }

  function openTerminal() {
    if (!reasonFor || !reasonDraft.trim()) return;
    // A fresh key remounts the component, which is what makes "open a
    // terminal on a different node" mean a different terminal rather than a
    // confusing reuse of the last one.
    terminalKey += 1;
    terminalReason = reasonDraft.trim();
    terminalFor = reasonFor.name;
    reasonOpen = false;
  }

  /**
   * Why a terminal on this node would be refused, or null if nothing this
   * side can see would stop it.
   *
   * Three of the gates are knowable from here — the node's own announced
   * acceptance, its reserved tags, and whether it is even alive. The fourth
   * (a session policy naming *you*) is a per-user question the far node
   * answers, so the button stays enabled and the refusal arrives in that
   * node's own words rather than being guessed at here.
   */
  function terminalBlocker(n: NodeView): string | null {
    if (n.revoked) return "This node is revoked.";
    if (n.left) return "This node has left the mesh.";
    if (n.liveness === "dead") return "This node is not reachable right now.";
    if (n.noShell)
      return `${n.name} is tagged no-shell — it runs jobs, but it is not a place to get a prompt.`;
    if (!n.executing)
      return `${n.name} is a no-execute peer — it does not run processes at all.`;
    if (!n.acceptsTerminals)
      return `${n.name} does not accept terminals. Its own operator turns them on with [sessions] accept = "on".`;
    return null;
  }
  const shellable = $derived((n: NodeView) => terminalBlocker(n) === null);

  /** A one-word status for the table and the drill-in. */
  function terminalState(n: NodeView): { label: string; tone: string } {
    if (n.noShell) return { label: "no-shell", tone: "warn" };
    if (!n.executing) return { label: "no-execute", tone: "off" };
    if (n.acceptsTerminals) return { label: "on", tone: "ok" };
    return { label: "off", tone: "off" };
  }

  // -- this peer's own no-shell tag ----------------------------------------------
  //
  // `TagsUpdate` is deliberately self-only — a node's member record may be
  // updated by that node (R8), which is what stops one machine relabelling
  // another. So this toggle exists for this peer and, for everybody else,
  // the panel hands over the exact command to run there instead.
  let tagBusy = $state(false);

  async function setNoShell(on: boolean) {
    tagBusy = true;
    try {
      await api.tagsUpdate(on ? [NO_SHELL] : [], on ? [] : [NO_SHELL]);
      app.flash(
        on
          ? "this peer is now tagged no-shell — it will refuse terminals"
          : "no-shell removed from this peer",
      );
      await app.refresh();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      tagBusy = false;
    }
  }

  const NO_SHELL = "no-shell";

  let copiedCmd = $state<string | null>(null);
  async function copyCmd(label: string, text: string) {
    try {
      await api.copyText(text);
      copiedCmd = label;
      setTimeout(() => (copiedCmd = null), 1400);
    } catch {
      app.flash("clipboard unavailable — select and copy by hand");
    }
  }

  // -- live sessions -------------------------------------------------------------
  //
  // The direction is the fact worth showing and the one nobody guesses:
  // `in` is somebody holding a prompt on *this* machine, `out` is this
  // machine reaching elsewhere. They are the same object seen from opposite
  // ends, and only one of them is a thing you might want to end.
  let sessions = $state<SessionView[]>([]);
  async function loadSessions() {
    try {
      sessions = await api.sessions();
    } catch {
      // Not worth a banner: the panel is a live view and the next tick
      // tries again.
    }
  }
  $effect(() => poll(loadSessions, 3000));

  async function closeSession(id: number) {
    try {
      await api.sessionClose(id);
      await loadSessions();
    } catch (e) {
      app.flash(errText(e));
    }
  }

  function traffic(s: SessionView): string {
    const fmt = (n: number) =>
      n < 1024 ? `${n} B` : n < 1024 * 1024 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1048576).toFixed(1)} MB`;
    return `${fmt(Number(s.bytesIn))} / ${fmt(Number(s.bytesOut))}`;
  }

  // -- the overview and the cards (2026-09-22) ----------------------------------
  //
  // The panel was eleven surfaces tall: four metric tiles, a strip of four
  // vitals, a reservations table, a thirteen-column machine table, a table
  // of every accelerator and a sessions list — every one of them read in
  // full to answer "is my fleet all right". It is three cards and a grid
  // now. **Health** is a ring of the machines by state, **Workload** a
  // gauge of the slots, **Hardware** the four things the mesh adds up to;
  // under them, one card per machine, which says what a row said in the
  // shape of a machine rather than the shape of a spreadsheet.
  //
  // Nothing went. Reservations, accelerators and live sessions are one-line
  // callouts that open their tables in a sheet — they are things a person
  // looks at for a minute when there is something in them, not furniture —
  // and the table is one click away for the operator who sorts, ticks and
  // scans sixty machines at a time. Which of the two a person last chose
  // is remembered on this machine only.
  type FleetLayout = "cards" | "table";
  function readLayout(): FleetLayout {
    try {
      return localStorage.getItem("fleet:layout") === "table" ? "table" : "cards";
    } catch {
      return "cards";
    }
  }
  let layout = $state<FleetLayout>(readLayout());
  function setLayout(next: FleetLayout) {
    layout = next;
    try {
      localStorage.setItem("fleet:layout", next);
    } catch {
      // A private window: the choice lasts as long as the page.
    }
  }

  let gangsOpen = $state(false);
  let devicesOpen = $state(false);
  let sessionsOpen = $state(false);

  /** The fleet by state, for the health ring. Quarantine wins over
   *  liveness: an isolated machine that still answers is the one a person
   *  needs to see first, and counting it as "answering" would hide it. */
  const health = $derived.by(() => {
    let well = 0;
    let quiet = 0;
    let quarantined = 0;
    let down = 0;
    for (const n of fleetNow) {
      if (n.quarantined !== null) quarantined++;
      else if (n.liveness === "dead") down++;
      else if (n.liveness === "suspect") quiet++;
      else well++;
    }
    return { well, quiet, quarantined, down };
  });

  const spec = $derived(app.totals);

  /** A load reading as a colour: the brand teal while it is comfortable,
   *  then the two status tones the meters already use. */
  function loadTone(pct: number): string {
    return pct >= 90 ? "var(--status-error)" : pct >= 70 ? "var(--status-warn)" : "var(--accent)";
  }
  /** What the machine is doing, 0–100 — live telemetry first, the mesh's
   *  own slot count when that is all there is, and null (drawn as nothing)
   *  when nobody has said. Same order as the table's Load column. */
  function busyPct(n: NodeView): number | null {
    const t = topIndex.get(n.nodeId);
    if (!t || t.noExecute) return null;
    if (t.busy != null) return Math.round(t.busy * 100);
    if (t.saturationPct > 0) return t.saturationPct;
    return null;
  }
  /** The edge a card wears: nothing while it is well. */
  function edgeOf(n: NodeView): string | null {
    if (n.revoked || n.left) return null;
    if (n.quarantined !== null) return "var(--status-error)";
    if (n.liveness === "suspect") return "var(--status-warn)";
    if (n.liveness === "dead") return "var(--liveness-dead)";
    return null;
  }
  function openNode(n: NodeView) {
    selectedId = n.nodeId;
    infoOpen = true;
  }

  /** The cards' sort, as one menu — the table sorts by its headers. */
  const CARD_SORTS: { key: FleetSortKey; label: string }[] = [
    { key: "default", label: "This peer first" },
    { key: "name", label: "Name" },
    { key: "load", label: "Busiest" },
    { key: "work", label: "Most work" },
    { key: "slots", label: "Most slots" },
    { key: "joined", label: "Newest" },
  ];
  function sortCards(key: FleetSortKey) {
    sort = { key, desc: key === "joined" ? true : SORT_STARTS_DESC[key] };
  }

  /** Accelerators, per machine, for the sheet's cards. */
  const devicesByNode = $derived.by(() => {
    const m = new Map<string, DeviceView[]>();
    for (const d of devices) m.set(d.nodeName, [...(m.get(d.nodeName) ?? []), d]);
    return [...m.entries()];
  });
  const devicesTroubled = $derived(devices.filter((d) => d.health !== "ok" || d.throttled).length);
  const gangsHolding = $derived(gangs.filter((g) => g.state === "offering").length);
  const gangsRunning = $derived(gangs.filter((g) => g.state === "running").length);
</script>

<!-- Two questions about the same machines, and they do not fit in one
     table. **Fleet** is the live one — who is up, what is it carrying,
     ping it, drain it. **Updates** is the slow one: what build each node
     announced (`cap.ver`, M24 §8) and the health-gated rollout that
     changes it. The control rides the group header of whichever table is
     showing, so it is always beside the thing it switches — and it is
     drawn only where Updates has nowhere else to be reached from, which
     since 2026-09-11 means the legacy surface alone. "Add machines" is
     not a mode and is always here. -->
<!-- A sortable column head (UX-8): the label is the button, and the
     arrow only appears on the column the table is sorted by. -->
{#snippet sortHead(key: FleetSortKey, label: string, cls: string)}
  <th
    class={`label py-3 ${cls}`}
    aria-sort={sort.key === key ? (sort.desc ? "descending" : "ascending") : "none"}
  >
    <button
      class="sorth"
      class:is-on={sort.key === key}
      title={`Sort by ${label.toLowerCase()}`}
      onclick={() => sortBy(key)}
    >
      {label}
      <span class="sorth-arrow" class:is-desc={sort.key === key && sort.desc} aria-hidden="true">
        <Icon name="sort-asc" size={11} />
      </span>
    </button>
  </th>
{/snippet}

{#snippet machineCard(n: NodeView)}
  {@const t = topOf(n)}
  {@const pct = busyPct(n)}
  {@const edge = edgeOf(n)}
  <!-- A machine, as a card: who it is, how busy, what it has, what it is
       doing. The whole card opens the machine; the tick in its corner
       chooses it, and never also opens it. -->
  <div
    class="mcard lift"
    class:is-chosen={chosen.has(n.nodeId)}
    class:is-self={n.isSelf}
    class:is-gone={n.revoked || n.left}
    class:is-open={selectedId === n.nodeId && infoOpen}
    style={edge ? `--edge-tone: ${edge};` : ""}
    role="button"
    tabindex="0"
    aria-label={`Open ${n.name}`}
    onclick={() => openNode(n)}
    onkeydown={(e) => {
      if (e.target !== e.currentTarget) return;
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        openNode(n);
      }
    }}
  >
    <div class="flex items-start gap-2">
      <div class="min-w-0 flex-1">
        <div class="flex min-w-0 items-center gap-2">
          <Dot kind={n.revoked || n.left ? "dead" : n.quarantined !== null ? "quarantined" : n.liveness} label={n.revoked ? "revoked" : n.left ? "left the mesh" : null} />
          <span class="truncate text-[15px] font-semibold tracking-[-.01em] text-ink">{n.name}</span>
          {#if n.isSelf}<span class="capsule-flat flex-none" style="--tone: var(--accent); --tone-ink: var(--accent-ink);">This peer</span>{/if}
        </div>
        <span class="mono mt-0.5 block text-[10.5px] leading-tight" style="color: var(--hue-indigo-ink);">{n.shortId}</span>
      </div>
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <span class="mcard-check" class:is-on={chosen.size > 0} onclick={(e) => e.stopPropagation()}>
        <input type="checkbox" class="check" aria-label={`Choose ${n.name}`} checked={chosen.has(n.nodeId)} onchange={() => toggleChosen(n.nodeId)} />
      </span>
    </div>

    {#if n.revoked || n.left || n.quarantined !== null}
      <div class="mt-2 flex flex-wrap gap-1.5">
        {#if n.revoked}<Pill tone="error">Revoked</Pill>{:else if n.left}<Pill tone="muted">Left the mesh</Pill>
        {:else if n.quarantined !== null}<Pill tone="error" title={n.quarantined ? `Quarantined: ${n.quarantined}` : "Quarantined — no reason given"}>Quarantined</Pill>{/if}
      </div>
    {/if}

    <div class="mt-3.5 mb-3.5 flex items-center gap-3.5">
      <Ring
        size={62}
        thickness={7}
        total={100}
        label={pct == null ? `${n.name}: no live reading` : `${n.name} is ${pct}% busy`}
        arcs={pct == null ? [] : [{ value: Math.max(pct, 1.5), color: loadTone(pct) }]}
      >
        {#if pct == null}
          <span class="text-xs text-ink-5" title={t?.noExecute ? "does not run jobs" : "no live reading from this machine yet"}>—</span>
        {:else}
          <span class="num text-[14px] font-bold tracking-[-.02em] text-ink">{pct}<span class="text-[10px] font-medium text-ink-5">%</span></span>
        {/if}
      </Ring>
      <div class="min-w-0 flex-1">
        <div class="mcard-specs">
          {#if n.capacity}
            <span title="Hardware threads"><Icon name="compute" size={13} />{n.capacity.cores} cores</span>
            <span title={t?.memAvailableMb != null ? `${fmtSize(t.memAvailableMb)} free` : "Memory"}><Icon name="memory" size={13} />{fmtSize(n.capacity.memMb)}</span>
          {:else}
            <span class="text-ink-5">hardware not advertised</span>
          {/if}
          {#if t && t.devices > 0}
            <span style="color: var(--hue-green-ink);" title={`${t.devicesBusy ?? "?"} of ${t.devices} accelerators held`}><Icon name="gpu" size={13} />{t.devices} GPU{t.devices === 1 ? "" : "s"}</span>
          {/if}
        </div>
        <div class="mt-2 flex flex-nowrap items-center gap-1 overflow-hidden">
          {#each n.tags.slice(0, 2) as tag (tag)}<Tag text={tag} />{/each}
          {#if n.tags.length > 2}
            <span class="num text-2xs whitespace-nowrap text-ink-4" title={n.tags.join(", ")}>+{n.tags.length - 2}</span>
          {/if}
        </div>
      </div>
    </div>

    <div class="mcard-foot">
      <span class="flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-0.5 text-xs">
        {#if !t}
          <span class="text-ink-5">{runningOn(n) > 0 ? `${runningOn(n)} running` : "No reading yet"}</span>
        {:else if t.noExecute}
          <span class="text-ink-5">Does not run jobs</span>
        {:else}
          {#if t.running > 0}
            <span class="inline-flex items-center gap-1.5 tabular-nums text-running-ink"><Dot kind="running" size={5} decorative />{t.running} running</span>
          {/if}
          {#if t.queued > 0}
            <span class="tabular-nums" style="color: var(--status-warn-ink);">{t.queued} waiting</span>
          {/if}
          {#if t.leases > 0}
            <span class="tabular-nums" style="color: var(--status-warn-ink);" title={`Held for ${t.leases} multi-machine job${t.leases === 1 ? "" : "s"} that ${t.leases === 1 ? "has" : "have"} not started yet`}>{t.leasedCpus}c held</span>
          {/if}
          {#if t.running === 0 && t.queued === 0 && t.leases === 0}
            <span class="text-ink-5">Idle</span>
          {/if}
          {#if t.queued === 0 && t.leases === 0}
            <span class="tabular-nums text-ink-5">· {t.slots} slot{t.slots === 1 ? "" : "s"}</span>
          {/if}
        {/if}
      </span>
      <span class="ml-auto flex-none">
        {#if n.revoked || n.left}
          <span class="text-xs text-ink-6">—</span>
        {:else}
          <SyncCell sync={n.sync} name={n.name} compact />
        {/if}
      </span>
    </div>
  </div>
{/snippet}

{#snippet modes()}
  {#if updatesTab}
  <div class="seg" role="tablist" aria-label="What to show about these machines">
    {#each [["fleet", "Fleet", null], ["software", "Updates", behind || null]] as [id, label, n] (id)}
      <button role="tab" aria-selected={tab === id} onclick={() => (picked = id as Tab)}>
        {label}
        {#if n !== null}
          <span
            class="num ml-1 rounded-full px-1.5 text-2xs font-semibold"
            title={`${n} node${n === 1 ? "" : "s"} are not on the newest build`}
            style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
          >{n}</span>
        {/if}
      </button>
    {/each}
  </div>
  {/if}
  <button class="btn btn-ghost btn-md" onclick={() => (deployOpen = true)}>
    <Icon name="plus" size={13} /> Add machines
  </button>
{/snippet}

<div class="fade-in space-y-5">
  <DeploySheet open={deployOpen} onclose={() => (deployOpen = false)} />

  {#if tab === "software"}
    <div class="section-head">
      <span class="section-title">Updates</span>
      <span class="section-sum">
        {behind} node{behind === 1 ? "" : "s"} behind the newest build
      </span>
      <div class="ml-auto flex flex-none items-center gap-2">
        {@render modes()}
      </div>
    </div>
    <Software />
  {:else}
  <!-- ═══════════ the overview: health · workload · hardware ═══════════ -->
  <div class="fleet-overview" use:staggerIn={{ each: 50, y: 8 }}>
    <!-- Health: every machine, by state, as one ring. The number in the
         middle is the fleet's one machine count (UX-12); the legend is the
         breakdown, and a state with nobody in it is not listed. -->
    <section class="panel" aria-labelledby="fleet-health">
      <div class="panel-head">
        <span class="icon-tile sm" style="--tone: var(--accent); --tone-ink: var(--accent-ink);"><Icon name="pulse" size={15} /></span>
        <div class="min-w-0 flex-1">
          <h2 id="fleet-health" class="panel-title">Health</h2>
          <p class="panel-sub">{answering === liveCount ? "Every machine is answering" : `${answering} of ${liveCount} answering`}</p>
        </div>
      </div>
      <div class="flex items-center gap-5">
        <Ring
          size={112}
          thickness={12}
          label={`${liveCount} machines: ${health.well} answering, ${health.quiet} not replying, ${health.quarantined} quarantined, ${health.down} down`}
          arcs={[
            { value: health.well, color: "var(--accent)", label: `${health.well} answering` },
            { value: health.quiet, color: "var(--status-warn)", label: `${health.quiet} not replying` },
            { value: health.quarantined, color: "var(--status-error)", label: `${health.quarantined} quarantined` },
            { value: health.down, color: "var(--liveness-dead)", label: `${health.down} down` },
          ]}
        >
          <span class="big-fig">{liveCount}</span>
          <span class="fig-label mt-1">machine{liveCount === 1 ? "" : "s"}</span>
        </Ring>
        <div class="legend flex-1">
          <div class="legend-row"><span class="sw" style="--sw: var(--accent);"></span>Healthy<span class="n">{health.well}</span></div>
          {#if health.quiet}<div class="legend-row"><span class="sw" style="--sw: var(--status-warn);"></span>Not replying<span class="n">{health.quiet}</span></div>{/if}
          {#if health.quarantined}<div class="legend-row"><span class="sw" style="--sw: var(--status-error);"></span>Quarantined<span class="n">{health.quarantined}</span></div>{/if}
          {#if health.down}<div class="legend-row"><span class="sw" style="--sw: var(--liveness-dead);"></span>Down<span class="n">{health.down}</span></div>{/if}
        </div>
      </div>
      <!-- The mesh's vitals, when this panel is inside the Mesh section. -->
      {#if vitals}<div class="mt-4 flex flex-wrap gap-1.5">{@render vitals()}</div>{/if}
    </section>

    <!-- Workload: how full the slots are, as a gauge, and what fills them. -->
    <section class="panel" aria-labelledby="fleet-work">
      <div class="panel-head">
        <span class="icon-tile sm" style="--tone: var(--status-running); --tone-ink: var(--status-running-ink);"><Icon name="jobs" size={15} /></span>
        <div class="min-w-0 flex-1">
          <h2 id="fleet-work" class="panel-title">Workload</h2>
          <p class="panel-sub">
            {#if app.top.length === 0}
              Waiting for the first reading
            {:else}
              {totals.slots} job slots on {totals.executors} machine{totals.executors === 1 ? "" : "s"}
            {/if}
          </p>
        </div>
      </div>
      <div class="flex items-center gap-5">
        <Ring
          size={112}
          thickness={12}
          total={100}
          label={`${totals.pct}% of the mesh's job slots in use`}
          arcs={[{ value: totals.pct, color: loadTone(totals.pct) }]}
        >
          <span class="big-fig">{totals.pct}<span class="unit">%</span></span>
          <span class="fig-label mt-1">in use</span>
        </Ring>
        <div class="legend flex-1">
          <div class="legend-row"><span class="sw" style="--sw: var(--status-running);"></span>Running<span class="n">{totals.running}</span></div>
          <div class="legend-row">
            <span class="sw" style={`--sw: ${totals.queued > 0 ? "var(--status-warn)" : "var(--track)"};`}></span>Waiting
            <span class="n" style={totals.queued > 0 ? "color: var(--status-warn-ink);" : ""}>{totals.queued}</span>
          </div>
          <div class="legend-row"><span class="sw" style="--sw: var(--track);"></span>Free slots<span class="n">{Math.max(0, totals.slots - totals.running)}</span></div>
        </div>
      </div>
      <p class="mt-4 mb-0 text-xs text-ink-5">
        {#if totals.running === 0 && totals.queued === 0}
          Idle — {uptimeShort}.
        {:else if totals.queued > 0}
          {totals.queued} job{totals.queued === 1 ? " is" : "s are"} waiting for a free slot.
        {:else}
          Everything submitted is running.
        {/if}
      </p>
    </section>

    <!-- Hardware: what the mesh adds up to. Figures from `totals`, the
         same summation the spec line in the header reads. -->
    <section class="panel" aria-labelledby="fleet-hw">
      <div class="panel-head">
        <span class="icon-tile sm" style="--tone: var(--hue-indigo); --tone-ink: var(--hue-indigo-ink);"><Icon name="compute" size={15} /></span>
        <div class="min-w-0 flex-1">
          <h2 id="fleet-hw" class="panel-title">Hardware</h2>
          <p class="panel-sub">{spec?.coverage ?? "What the mesh adds up to"}</p>
        </div>
      </div>
      {#if spec}
        <div class="hw-grid">
          <div class="hw-cell">
            <span class="icon-tile sm" style="--tone: var(--hue-indigo); --tone-ink: var(--hue-indigo-ink);"><Icon name="compute" size={14} /></span>
            <div><div class="hw-fig num">{spec.cores}</div><div class="fig-label">CPU cores</div></div>
          </div>
          <div class="hw-cell">
            <span class="icon-tile sm" style="--tone: var(--hue-sky); --tone-ink: var(--hue-sky-ink);"><Icon name="memory" size={14} /></span>
            <div><div class="hw-fig num">{fmtSize(spec.memMb)}</div><div class="fig-label">Memory</div></div>
          </div>
          <button class="hw-cell hw-btn" disabled={!anyDevices} onclick={() => (devicesOpen = true)} title={anyDevices ? "See every accelerator" : "No accelerators in this mesh"}>
            <span class="icon-tile sm" style="--tone: var(--hue-green); --tone-ink: var(--hue-green-ink);"><Icon name="gpu" size={14} /></span>
            <div class="min-w-0">
              <div class="hw-fig num">{spec.gpus}{#if devicesTroubled > 0}<span class="hw-warn" title={`${devicesTroubled} need attention`}>{devicesTroubled}</span>{/if}</div>
              <div class="fig-label truncate">GPUs{spec.vramMb > 0 ? ` · ${fmtSize(spec.vramMb)}` : ""}</div>
            </div>
          </button>
          <div class="hw-cell">
            <span class="icon-tile sm" style="--tone: var(--hue-amber); --tone-ink: var(--hue-amber-ink);"><Icon name="disk" size={14} /></span>
            <div><div class="hw-fig num">{fmtSize(spec.diskMb)}</div><div class="fig-label">Poolable disk</div></div>
          </div>
        </div>
      {:else}
        <p class="m-0 text-sm text-ink-5">Summed from what each machine advertises — nothing has reported yet.</p>
      {/if}
    </section>
  </div>

  <!-- Things going on right now, each one line that opens its table. Shown
       only while there is something in them — the rule every conditional
       panel here has always followed. -->
  {#if gangs.length || sessions.length || devicesTroubled > 0}
    <div class="fleet-callouts">
      {#if gangs.length}
        <button class="callout lift" style="--tone: var(--status-warn);" onclick={() => (gangsOpen = true)}>
          <span class="icon-tile sm" style="--tone: var(--status-warn); --tone-ink: var(--status-warn-ink);"><Icon name="gang" size={15} /></span>
          <span class="min-w-0 flex-1">
            <span class="block font-semibold text-ink">{gangs.length} multi-machine job{gangs.length === 1 ? "" : "s"}</span>
            <span class="block text-xs text-ink-5">{gangsHolding} holding machines · {gangsRunning} running</span>
          </span>
          <Icon name="chevron-right" size={14} />
        </button>
      {/if}
      {#if devicesTroubled > 0}
        <button class="callout lift" style="--tone: var(--status-warn);" onclick={() => (devicesOpen = true)}>
          <span class="icon-tile sm" style="--tone: var(--status-warn); --tone-ink: var(--status-warn-ink);"><Icon name="gpu" size={15} /></span>
          <span class="min-w-0 flex-1">
            <span class="block font-semibold text-ink">{devicesTroubled} accelerator{devicesTroubled === 1 ? "" : "s"} need{devicesTroubled === 1 ? "s" : ""} attention</span>
            <span class="block text-xs text-ink-5">of {devices.length} across {devicesByNode.length} machine{devicesByNode.length === 1 ? "" : "s"}</span>
          </span>
          <Icon name="chevron-right" size={14} />
        </button>
      {/if}
      {#if sessions.length}
        <button class="callout lift" style="--tone: var(--hue-plum);" onclick={() => (sessionsOpen = true)}>
          <span class="icon-tile sm" style="--tone: var(--hue-plum); --tone-ink: var(--hue-plum-ink);"><Icon name="terminal" size={15} /></span>
          <span class="min-w-0 flex-1">
            <span class="block font-semibold text-ink">{sessions.length} live session{sessions.length === 1 ? "" : "s"}</span>
            <span class="block text-xs text-ink-5">{sessions.filter((x) => x.hosting).length} into this peer · {sessions.filter((x) => !x.hosting).length} out</span>
          </span>
          <Icon name="chevron-right" size={14} />
        </button>
      {/if}
    </div>
  {/if}

  <div class="grid min-w-0 items-start gap-4">
    <div class="min-w-0">
      <!-- The group says what it holds before the table does — above the
           surface, not inside it, so the card starts at the first thing it
           actually contains. The two controls that belong to this table
           and not to the page sit on the same line: which of the two
           questions about these machines you are asking, and the door onto
           a machine that is not here yet. -->
      <div class="section-head">
        <span class="section-title">Machines</span>
        {#if app.top.length}
          <span class="section-sum">{totals.slots} job slots · {totals.pct}% in use</span>
        {/if}
        <div class="ml-auto flex flex-none items-center gap-2">
          <div class="seg" role="tablist" aria-label="Show the machines as">
            <button role="tab" aria-selected={layout === "cards"} title="Cards" aria-label="Cards" onclick={() => setLayout("cards")}><Icon name="view-cards" size={14} /></button>
            <button role="tab" aria-selected={layout === "table"} title="Table" aria-label="Table" onclick={() => setLayout("table")}><Icon name="view-rows" size={14} /></button>
          </div>
          {@render modes()}
        </div>
      </div>

      <!-- Find a machine (UX-8): search, then the chips a person filters a
           fleet by. A chip with nothing in it is disabled rather than
           hidden, so the row does not change shape as machines recover. -->
      <div class="fleet-tools">
        <label class="fleet-search">
          <Icon name="search" size={14} />
          <input
            bind:value={query}
            placeholder="Find a machine — name, tag, address, id or build"
            spellcheck="false"
            aria-label="Find a machine"
            onkeydown={(e) => e.key === "Escape" && (query = "")}
          />
          {#if query}
            <button class="fleet-clear" title="Clear" aria-label="Clear the search" onclick={() => (query = "")}>
              <Icon name="close" size={12} />
            </button>
          {/if}
        </label>
        <div class="seg" role="tablist" aria-label="Show machines that are">
          {#each FLEET_CHIPS as c (c.id)}
            {@const n = chipCounts[c.id]}
            <button
              role="tab"
              aria-selected={chip === c.id}
              title={c.title}
              disabled={n === 0 && c.id !== "all"}
              onclick={() => (chip = c.id)}
            >
              {c.label}
              <span
                class="num ml-1 rounded-full px-1.5 text-2xs font-semibold"
                style={n > 0 && c.id !== "all" && c.id !== "held"
                  ? "background: var(--status-warn-wash); color: var(--status-warn-ink);"
                  : "color: inherit;"}>{n}</span
              >
            </button>
          {/each}
        </div>
        {#if filtering}
          <span class="text-xs text-ink-5">
            {filtered.length} of {rows.length}
          </span>
        {/if}
        {#if layout === "cards"}
          <label class="ml-auto flex items-center gap-2 text-xs text-ink-5">
            Sort
            <select class="select h-8 text-xs" value={sort.key} onchange={(e) => sortCards(e.currentTarget.value as FleetSortKey)}>
              {#each CARD_SORTS as o (o.key)}<option value={o.key}>{o.label}</option>{/each}
            </select>
          </label>
        {/if}
      </div>

      <!-- Several machines at once (UX-8). Only while some are chosen: a
           bar of disabled buttons is furniture. -->
      {#if chosenNodes.length > 0}
        <div class="fleet-bulk" role="toolbar" aria-label="Act on the chosen machines">
          <span class="text-sm font-semibold text-ink">{chosenNodes.length} chosen</span>
          <button class="btn btn-secondary btn-md" disabled={bulkBusy} onclick={runOnChosen}>
            <Icon name="play" size={13} /> Run something on them
          </button>
          {#if bulkTagOpen}
            <input
              class="field field-sunken mono h-8 w-48 px-2.5 text-xs outline-none"
              placeholder="rack=b4, prod"
              bind:value={bulkTagDraft}
              onkeydown={(e) => {
                if (e.key === "Enter") void tagChosen();
                if (e.key === "Escape") bulkTagOpen = false;
              }}
            />
            <button class="btn btn-primary btn-md" disabled={bulkBusy || !bulkTagDraft.trim()} onclick={() => void tagChosen()}>
              {bulkBusy ? "Tagging…" : "Add tags"}
            </button>
          {:else}
            <button class="btn btn-secondary btn-md" disabled={bulkBusy || !isAdmin} title={isAdmin ? "Add tags to every chosen machine" : "tagging machines needs the admin role"} onclick={() => (bulkTagOpen = true)}>
              Tag…
            </button>
          {/if}
          <button class="btn btn-secondary btn-md" disabled={bulkBusy || !isAdmin} title={isAdmin ? "Stop the mesh placing new work on these machines" : "needs the admin role"} onclick={() => contributeChosen(false)}>
            Stop sending work
          </button>
          <button class="btn btn-secondary btn-md" disabled={bulkBusy || !isAdmin} title={isAdmin ? "Let the mesh place work on these machines again" : "needs the admin role"} onclick={() => contributeChosen(true)}>
            Resume work
          </button>
          <button class="linkish ml-auto text-sm" onclick={() => { chosen = new Set(); bulkTagOpen = false; bulkErr = null; }}>
            Clear
          </button>
        </div>
      {/if}
      {#if bulkErr}
        <div class="errbox mb-3">{bulkErr}</div>
      {/if}

      {#if layout === "cards"}
        <div class="mcard-grid" use:staggerIn={{ each: 24, y: 6 }}>
          {#each visible as n (n.nodeId)}
            {@render machineCard(n)}
          {:else}
            <div class="card col-span-full px-5 py-8 text-center text-sm text-ink-5">
              No machine matches{query.trim() ? ` “${query.trim()}”` : ""}{chip !== "all" ? ` in ${FLEET_CHIPS.find((c) => c.id === chip)?.label}` : ""}.
              <button class="linkish ml-1" onclick={() => { query = ""; chip = "all"; }}>Show every machine</button>
            </div>
          {/each}
        </div>
      {:else}
      <div class="card tablewrap overflow-hidden" use:scrollHint>
      <table class="w-full text-left text-base">
        <thead>
          <tr class="border-b border-edge">
            <th class="w-[34px] py-3 pr-0 pl-4">
              <input
                type="checkbox"
                class="check"
                aria-label={allChosen ? "Unchoose every machine shown" : "Choose every machine shown"}
                checked={allChosen}
                indeterminate={!allChosen && filtered.some((n) => chosen.has(n.nodeId))}
                disabled={filtered.length === 0}
                onchange={toggleAllChosen}
              />
            </th>
            {@render sortHead("name", "Machine", "px-3")}
            <th class="label px-3 py-3">Tags</th>
            {#if anyCapacity}<th class="label px-3 py-3">Hardware</th>{/if}
            {@render sortHead("slots", "Slots", "px-3 text-right")}
            {@render sortHead("work", "Work", "px-3")}
            {@render sortHead("load", "Load", "px-3")}
            {@render sortHead("sync", "Sync", "px-3")}
            {@render sortHead("joined", "Joined", "px-3")}
            <th class="label px-3 py-3 pr-5 text-right"><span class="sr-only">Open</span></th>
          </tr>
        </thead>
        <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
          {#each visible as n (n.nodeId)}
            {@const t = topOf(n)}
            <tr
              class={`row-lazy grouprow relative cursor-pointer ${n.revoked || n.left ? "opacity-45" : ""}`}
              style={selectedId === n.nodeId
                ? "background: var(--accent-select);"
                : chosen.has(n.nodeId)
                  ? "background: var(--hue-wash);"
                  : n.isSelf
                    ? "background: var(--accent-row);"
                    : ""}
              onclick={() => {
                selectedId = n.nodeId;
                infoOpen = true;
              }}
              use:rowAction={() => {
                selectedId = n.nodeId;
                infoOpen = true;
              }}
              aria-current={selectedId === n.nodeId ? "true" : undefined}
            >
              <!-- The checkbox's cell swallows the click, so choosing a row
                   never also opens it. -->
              <!-- svelte-ignore a11y_click_events_have_key_events -->
              <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
              <td class="py-2.5 pr-0 pl-4" onclick={(e) => e.stopPropagation()}>
                <input
                  type="checkbox"
                  class="check"
                  aria-label={`Choose ${n.name}`}
                  checked={chosen.has(n.nodeId)}
                  onchange={() => toggleChosen(n.nodeId)}
                />
              </td>
              <td class="relative px-3 py-2.5">
                {#if selectedId === n.nodeId}<span class="absolute top-0 bottom-0 left-0 w-0.5"></span>{/if}
                <div class="flex items-center gap-2.5">
                  <Dot kind={n.revoked || n.left ? "dead" : n.quarantined !== null ? "quarantined" : n.liveness} label={n.revoked ? "revoked" : n.left ? "left the mesh" : null} />
                  <span class="min-w-0">
                    <span class="flex items-center gap-2">
                      <span class="font-semibold whitespace-nowrap text-ink">{n.name}</span>
                      {#if n.isSelf}<span class="capsule-flat" style="--tone: var(--accent); --tone-ink: var(--accent-ink);">This peer</span>{/if}
                      {#if n.revoked}<span class="text-xs font-medium text-error">revoked</span>{:else if n.left}<span class="text-xs text-ink-5">left</span>
                      {:else if n.quarantined !== null}
                        <!-- UX-4. A machine isolated as possibly compromised
                             looked like any healthy row until it was opened. -->
                        <Pill tone="error" title={n.quarantined ? `Quarantined: ${n.quarantined}` : "Quarantined — no reason given"}>Quarantined</Pill>
                      {/if}
                    </span>
                    <!-- The identity, under the name rather than in a column
                         of its own (UX-8): it is what the machine *is*, read
                         with the name, and a column of hashes was 130px of a
                         table that did not fit. Indigo, the hue of facts
                         about hardware, so it does not read as prose. -->
                    <span class="mono block text-[10.5px] leading-tight" style="color: var(--hue-indigo-ink);">{n.shortId}</span>
                  </span>
                </div>
              </td>
              <td class="px-3 py-2.5">
                <!-- One tag and a count: the full list is a hover away and
                     in the drill-in, and a second chip was 90px a row. -->
                <div class="flex flex-nowrap items-center gap-1 overflow-hidden">
                  {#each n.tags.slice(0, 1) as tag}<Tag text={tag} />{/each}
                  {#if n.tags.length > 1}
                    <span class="num text-2xs whitespace-nowrap text-ink-4" title={n.tags.join(", ")}>+{n.tags.length - 1}</span>
                  {/if}
                  {#if n.tags.length === 0}<span class="text-xs text-ink-6">—</span>{/if}
                </div>
              </td>
              {#if anyCapacity}
                <td
                  class="px-3 py-2.5 text-xs whitespace-nowrap text-ink-4"
                  title={n.capacity && t?.memAvailableMb != null ? `${fmtSize(t.memAvailableMb)} of memory free` : undefined}
                >
                  {#if n.capacity}
                    <span class="num text-ink-3">{n.capacity.cores}c</span>
                    <span class="text-ink-5"> · {fmtSize(n.capacity.memMb)}</span>
                  {:else}
                    <span class="text-ink-5">—</span>
                  {/if}
                </td>
              {/if}
              {#if t && !t.noExecute}
                <td class="num px-3 py-2.5 text-right text-sm text-ink-3">{t.slots}</td>
                <!-- Work (UX-8): what used to be four columns — Run, Queue,
                     Held and GPU — as one cell that only says what is there.
                     Thirteen columns in a card a thousand pixels wide was
                     Joined and the row's chevron off the edge. -->
                <td class="px-3 py-2.5 text-sm">
                  <span class="flex max-w-[168px] flex-wrap items-center gap-x-2.5 gap-y-0.5 whitespace-nowrap">
                    {#if t.running > 0}
                      <span class="num inline-flex items-center gap-1.5 text-running-ink" title={`${t.running} running`}><Dot kind="running" size={5} />{t.running}</span>
                    {/if}
                    {#if t.queued > 0}
                      <span class="num" style="color: var(--status-warn-ink);" title={`${t.queued} waiting for a free slot`}>{t.queued} waiting</span>
                    {/if}
                    {#if t.leases > 0}
                      <span
                        class="num"
                        style="color: var(--status-warn-ink);"
                        title={`Held for ${t.leases} multi-machine job${t.leases === 1 ? "" : "s"} that ${t.leases === 1 ? "has" : "have"} not started yet — ${t.leasedCpus} core${t.leasedCpus === 1 ? "" : "s"}`}
                      >{t.leasedCpus}c held</span>
                    {/if}
                    {#if t.devices > 0}
                      <!-- Held over advertised. The held figure is exact for
                           this peer and unknowable for any other — a device
                           hold is node-local — so a remote row reads `?/4`
                           rather than a number nobody can stand behind. -->
                      <span
                        class="num"
                        style="color: var(--hue-indigo-ink);"
                        title={`${t.devicesBusy ?? "?"} of ${t.devices} accelerators held${t.deviceBusyPct != null ? `, idlest at ${t.deviceBusyPct}% busy` : ", utilisation unknown"}`}
                      >GPU {t.devicesBusy ?? "?"}/{t.devices}</span>
                    {/if}
                    {#if t.running === 0 && t.queued === 0 && t.leases === 0 && t.devices === 0}
                      <span class="text-ink-6">idle</span>
                    {/if}
                  </span>
                </td>
                <!-- What the *machine* is doing, not what the mesh gave it.
                     This column used to show running-jobs-over-slots, so a
                     box pegged by anything the mesh did not place — a
                     build, a database, somebody's `stress` run — read as
                     "idle" while every core was flat out. `busy` is the
                     worst of real CPU and memory; slots stay in the
                     tooltip, where the mesh's own bookkeeping belongs. -->
                <td class="px-3 py-2.5 whitespace-nowrap">
                  {#if t.busy != null}
                    {@const pct = Math.round(t.busy * 100)}
                    <div
                      class="flex items-center gap-2"
                      title={`${t.cpuPct != null ? `CPU ${Math.round(t.cpuPct)}%` : "CPU unknown"}${t.memUsedPct != null ? ` · memory ${Math.round(t.memUsedPct)}%` : ""}${t.diskUsedPct != null ? ` · disk ${Math.round(t.diskUsedPct)}%` : ""} · ${t.running} of ${t.slots} slots`}
                    >
                      <div class="meter-track h-1.5 w-12"><div class="meter-fill" data-load={pct >= 90 ? "hot" : pct >= 70 ? "warn" : "ok"} use:meter={{ pct: Math.max(2, pct) }}></div></div>
                      <span class="num text-xs text-ink-4">{pct}%</span>
                    </div>
                  {:else if t.saturationPct > 0}
                    <div class="flex items-center gap-2" title="live telemetry unknown — showing mesh slots">
                      <div class="meter-track h-1.5 w-12"><div class="meter-fill" data-load={t.saturationPct >= 90 ? "warn" : "ok"} style={`width:${t.saturationPct}%;`}></div></div>
                      <span class="num text-xs text-ink-4">{t.saturationPct}%</span>
                    </div>
                  {:else}
                    <span class="num text-xs text-ink-6" title="no live reading from this node yet">—</span>
                  {/if}
                </td>
              {:else}
                <td colspan={3} class="px-3 py-2.5 text-xs whitespace-nowrap text-ink-6">{t?.noExecute ? "does not run jobs" : runningOn(n) > 0 ? `${runningOn(n)} running` : "—"}</td>
              {/if}
              <td class="px-3 py-2.5">
                {#if n.revoked || n.left}
                  <span class="text-xs text-ink-6">—</span>
                {:else}
                  <SyncCell sync={n.sync} name={n.name} />
                {/if}
              </td>
              <td class="num px-3 py-2.5 text-sm text-ink-5 whitespace-nowrap">{ago(n.joinedAtMicros)}</td>
              <!-- The row is the target, so the last column says "this
                   opens" rather than putting a second, smaller target
                   inside it. `Facts` used to live here as a link per row;
                   it is a button in the node's own panel now, next to the
                   other things you can do to that machine, which is where
                   somebody looks for it. -->
              <td class="px-3 py-2.5 pr-4 text-right">
                <span class="rowchev inline-grid place-items-center"><Icon name="chevron-right" size={15} /></span>
              </td>
            </tr>
          {:else}
            <tr>
              <td colspan={9 + (anyCapacity ? 1 : 0)} class="px-5 py-8 text-center text-sm text-ink-5">
                No machine matches{query.trim() ? ` “${query.trim()}”` : ""}{chip !== "all" ? ` in ${FLEET_CHIPS.find((c) => c.id === chip)?.label}` : ""}.
                <button class="linkish ml-1" onclick={() => { query = ""; chip = "all"; }}>Show every machine</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      </div>
      {/if}
      <!-- Under the group, not inside it: prose ABOUT the table is not a
           row of it. Same place, same type, same colour as every other
           footnote in the product. -->
      <div class="footnote">
        {#if filtered.length > visible.length}
          <span>Showing {visible.length} of {filtered.length}.</span>
          <button class="linkish" onclick={() => (shown += PAGE)}>Show {Math.min(PAGE, filtered.length - visible.length)} more</button>
          <button class="linkish" onclick={() => (shown = filtered.length)}>Show all</button>
        {:else}
          <span>Open a machine for its details and actions. Tick several to act on them together.</span>
        {/if}
        {#if revokedCount > 0}
          <button class="linkish ml-auto" onclick={() => (showRevoked = !showRevoked)}>
            <span class="num">{revokedCount}</span> revoked — {showRevoked ? "hide" : "show"}
          </button>
        {/if}
      </div>
    </div>

  </div>

  {/if}
</div>

{#if tab === "fleet" && terminalFor}
  <div class="fade-in mt-4">
    {#key terminalKey}
      <Terminal
        node={terminalFor}
        reason={terminalReason}
        onexit={() => {
          /* The panel stays put with its transcript: an exit code and the
             last screen are usually the reason somebody opened it. */
        }}
      />
    {/key}
    <div class="mt-2 flex items-center gap-3 text-xs text-ink-5">
      <span>
        Authorized on <span class="font-semibold text-ink-4">{terminalFor}</span>, audited there as
        <span class="mono">shell.open</span> / <span class="mono">shell.close</span>.
      </span>
      <button class="linkish ml-auto text-xs" onclick={() => (terminalFor = null)}>
        Close panel
      </button>
    </div>
  </div>
{/if}

<!-- ===================== node details ===================== -->
<!-- The drill-in, as a dialog: two balanced columns instead of one long
     strip, so nothing scrolls that does not have to. Same surface language
     as the Facts modal. -->

{#if infoOpen && selected}
  {@const t = topOf(selected)}
  {@const tstate = terminalState(selected)}
  <div
    use:portal
    class="fixed inset-0 z-50 grid place-items-center p-6"
    style="background: rgba(12,16,24,.62); backdrop-filter: blur(3px);"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) infoOpen = false;
    }}
  >
    <div
      class="fade-in flex max-h-[86vh] w-full max-w-[720px] flex-col overflow-hidden rounded-2xl bg-surface"
      style="box-shadow: var(--sh-modal);"
      role="dialog"
      aria-modal="true"
      aria-label={`About ${selected.name}`}
      use:dialog={{ onescape: () => (infoOpen = false) }}
    >
      <!-- header -->
      <div class="flex items-center gap-3 border-b border-edge px-6 py-[16px]">
        <Dot kind={selected.revoked || selected.left ? "dead" : selected.liveness} label={selected.revoked ? "revoked" : selected.left ? "left the mesh" : null} />
        <h3 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">{selected.name}</h3>
        {#if selected.isSelf}
          <span class="rounded-full px-2 py-0.5 text-2xs font-semibold" style="background: var(--accent-wash); color: var(--accent-ink);">this peer</span>
        {/if}
        {#if selected.revoked}
          <span class="rounded-full px-2 py-0.5 text-2xs font-semibold" style="background: var(--status-error-wash); color: var(--status-error-ink);">revoked</span>
        {:else if selected.left}
          <span class="rounded-full bg-surface-2 px-2 py-0.5 text-2xs font-semibold text-ink-5">left</span>
        {:else}
          <span class="text-sm text-ink-5">{selected.liveness}</span>
        {/if}
        <span class="num ml-auto text-xs text-ink-5">joined {ago(selected.joinedAtMicros)}</span>
        <button class="btn btn-secondary h-8 w-8 shrink-0 rounded-sm p-0" aria-label="Close" onclick={() => (infoOpen = false)}>
          <Icon name="close" size={14} />
        </button>
      </div>

      <!-- body: two columns, so the dialog stays a dialog and not a scroll -->
      <div class="grid gap-px overflow-y-auto sm:grid-cols-2" style="background: var(--edge);">
        <!-- identity -->
        <div class="bg-surface px-6 py-4">
          <div class="label">Identity</div>
          <div class="mono selectable mt-1.5 text-xs break-all text-ink-3">{selected.nodeId}</div>

          <div class="label mt-4 flex items-center gap-2">
            <span>Addresses</span>
            {#if canRepair && !addrOpen}
              <button class="ml-auto text-xs font-medium text-ink-5 hover:text-ink-3" onclick={openAddrEdit}>Repair…</button>
            {/if}
          </div>
          {#if addrOpen}
            <div class="fade-in mt-1.5">
              <input
                class={ui.inputMono}
                bind:value={addrDraft}
                placeholder="10.20.30.11:7777"
                aria-label="Addresses for {selected.name}"
                spellcheck="false"
                autocomplete="off"
                onkeydown={(e) => {
                  if (e.key === "Enter") saveAddrs();
                  if (e.key === "Escape") addrOpen = false;
                }}
              />
              <div class="mt-1.5 text-xs text-ink-5">
                Where this node answers, comma-separated — a hostname survives the next
                renumbering, because it is re-resolved on every dial. {selected.name} takes
                this back with its own address as soon as the two can talk.
              </div>
              {#if addrErr}<div class="mt-1.5 text-xs text-error">{addrErr}</div>{/if}
              <div class="mt-2 flex gap-2">
                <button class={ui.secondary} disabled={addrBusy} onclick={saveAddrs}>
                  {addrBusy ? "Saving…" : "Save"}
                </button>
                <button class={ui.secondary} disabled={addrBusy} onclick={() => (addrOpen = false)}>Cancel</button>
              </div>
            </div>
          {:else}
            <div class="mono selectable dark-scroll mt-1.5 max-h-20 overflow-y-auto text-xs text-ink-3">
              {#each selected.addrs as a}<div>{a}</div>{:else}<div class="text-ink-6">none announced yet</div>{/each}
            </div>
            {#if !selected.isSelf && !selected.revoked && !selected.left && !(app.status?.isFounder ?? false)}
              <div class="mt-1.5 text-xs text-ink-6">Addresses are repaired from {founderName}.</div>
            {/if}
          {/if}

          <!-- Tags, in two halves.
               What the machine announces is read-only from here — it comes
               off that machine and nowhere else. What an admin assigned is
               the mesh's, so it carries an × and can be added to: a tag put
               on from here is signed, replicated, and still on that node
               after it reboots or is rebuilt, which is the whole reason it
               is not just an edit to the node's own list. -->
          <div class="label mt-4 flex items-center gap-2">
            Tags
            {#if isAdmin && !selected.revoked && !selected.left}
              <button
                class="linkish ml-auto text-xs"
                disabled={assignBusy}
                onclick={() => { assignAdding = !assignAdding; assignErr = null; assignDraft = ""; }}
              >
                {assignAdding ? "cancel" : "add a tag"}
              </button>
            {/if}
          </div>
          <div class="mt-1.5 flex flex-wrap items-center gap-1">
            {#each selected.tags as tag}
              {@const mine = selected.assignedTags.includes(tag)}
              <Tag
                text={tag}
                assigned={mine}
                title={mine
                  ? "Assigned from the mesh — removable from here, and kept through this node's reboots"
                  : `${selected.name} announces this itself — it comes off on that machine`}
                onRemove={mine && isAdmin && !selected.revoked && !selected.left
                  ? () => removeTag(selected!, tag)
                  : undefined}
              />
            {:else}<span class="text-xs text-ink-6">none</span>{/each}
          </div>
          {#if assignAdding}
            <div class="fade-in mt-2">
              <input
                class="w-full rounded border border-edge-strong bg-surface-1 px-2 py-1 text-xs"
                placeholder="prod, dc=east"
                bind:value={assignDraft}
                onkeydown={(e) => { if (e.key === "Enter") addTag(selected!); }}
              />
              <div class="mt-1.5 text-xs text-ink-5">
                Assigned in the mesh, so <code class="mono">--on tag=…</code> finds this machine
                from anywhere and the label survives its next rebuild. Separate several with
                commas.
              </div>
              <div class="mt-2 flex gap-2">
                <button class={ui.secondary} disabled={assignBusy || !assignDraft.trim()} onclick={() => addTag(selected!)}>
                  {assignBusy ? "Saving…" : "Add"}
                </button>
              </div>
            </div>
          {/if}
          {#if assignErr}<div class="mt-1.5 text-xs text-error">{assignErr}</div>{/if}

          {#if ping && ping.node === selected.nodeId}
            <div class="fade-in mt-4">
              <div class="label">Ping</div>
              <div class="mono mt-1.5 text-xs">
                {#if ping.result}
                  <span style="color: var(--accent-ink);">reachable · {(ping.result.rttMicros / 1000).toFixed(1)} ms</span>
                  <span class="text-ink-4"> · via {ping.result.via}</span>
                  {#if !ping.result.sameMesh}<span class="text-error"> · different mesh!</span>{/if}
                {:else}
                  <span class="text-error">{ping.error}</span>
                {/if}
              </div>
            </div>
          {/if}
        </div>

        <!-- work -->
        <div class="bg-surface px-6 py-4">
          <div class="label">Work</div>
          <div class="mt-1.5 text-sm font-medium" style={selected.executing ? "color: var(--accent-ink);" : "color: var(--status-warn-ink);"}>
            {selected.executing ? "executes jobs" : "no-execute peer"}
          </div>
          {#if t && !t.noExecute}
            <div class="num mt-1 text-sm text-ink-4">{t.slots} slots · {t.running} running · {t.queued} queued</div>
          {/if}

          <!-- Quarantine (E2.4). Above Contributing because it outranks
               it: a quarantined node is not merely unused, it is one
               nobody may touch. Shown even when off, so the control is
               where somebody looks for it under pressure rather than
               somewhere they have to remember. -->
          <div
            class="mt-3 rounded-lg px-3.5 py-3"
            style={selected.quarantined != null
              ? "background: var(--status-error-wash); border: 1px solid var(--status-error-ink);"
              : "background: var(--surface-2);"}
          >
            <div class="flex items-center gap-2">
              <span class="label">Quarantine</span>
              {#if selected.quarantined != null}
                <span
                  class="rounded-full px-2 py-0.5 text-2xs font-semibold"
                  style="background: var(--status-error-ink); color: white;"
                >
                  ISOLATED
                </span>
              {/if}
              {#if isAdmin && !selected.revoked && !selected.left}
                {#if selected.quarantined != null}
                  <button
                    class="linkish ml-auto text-xs"
                    disabled={quarantineBusy}
                    onclick={() => setQuarantine(selected!, false)}
                  >
                    {quarantineBusy ? "…" : "lift quarantine"}
                  </button>
                {:else}
                  <button
                    class="linkish ml-auto text-xs"
                    disabled={quarantineBusy}
                    onclick={() => { quarantineOpen = !quarantineOpen; quarantineErr = null; }}
                  >
                    {quarantineOpen ? "cancel" : "quarantine this node"}
                  </button>
                {/if}
              {/if}
            </div>
            {#if selected.quarantined != null}
              <p class="mt-1.5 mb-0 text-xs leading-relaxed" style="color: var(--status-error-ink);">
                {selected.quarantined || "No reason was given."}
              </p>
              <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
                It takes no new work and nobody can open a terminal on it. Work already
                running was not killed — use Drain for that.
              </p>
            {:else if quarantineOpen}
              <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
                Isolates a machine you suspect: no new work, and nobody gets a terminal.
                Reversible, unlike Revoke — most suspicions turn out to be nothing.
              </p>
              <input
                class="mt-2 w-full rounded border border-edge-strong bg-surface-1 px-2 py-1 text-xs"
                placeholder="Why? e.g. unexpected outbound to 5.6.7.8"
                bind:value={quarantineReason}
                onkeydown={(e) => { if (e.key === "Enter") setQuarantine(selected!, true); }}
              />
              <button
                class="btn-danger mt-2 text-xs"
                disabled={quarantineBusy || !quarantineReason.trim()}
                onclick={() => setQuarantine(selected!, true)}
              >
                {quarantineBusy ? "…" : "Quarantine"}
              </button>
            {:else}
              <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
                Not isolated. Use this when you suspect a machine and want it out of
                reach while you find out — it is reversible, and Revoke is not.
              </p>
            {/if}
            {#if quarantineErr}<div class="mt-1.5 text-xs text-error">{quarantineErr}</div>{/if}
          </div>

          <!-- The owner's half of the same question.
               `executing` above is what the *node* says about itself and
               rewrites on every boot; this is what the mesh says back, and
               a node cannot clear it. It is the control for "somebody
               joined from a machine I am happy to have a person on and not
               work on" — which, before it existed, had no answer short of
               revoking them. -->
          <div class="mt-3 rounded-lg bg-surface-2 px-3.5 py-3">
            <div class="flex items-center gap-2">
              <span class="label">Contributing</span>
              <span
                class="rounded-full px-2 py-0.5 text-2xs font-semibold"
                style={selected.contributes
                  ? "background: var(--accent-wash); color: var(--accent-ink);"
                  : "background: var(--status-warn-wash); color: var(--status-warn-ink);"}
              >
                {selected.contributes ? "allowed" : "not allowed"}
              </span>
              {#if isAdmin && !selected.revoked && !selected.left}
                <button
                  class="linkish ml-auto text-xs"
                  disabled={contributeBusy}
                  onclick={() => setContributes(selected!, !selected!.contributes)}
                >
                  {contributeBusy
                    ? "…"
                    : selected.contributes
                      ? "stop sending work here"
                      : "allow work here"}
                </button>
              {/if}
            </div>
            <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
              {#if selected.contributes}
                An administrator has not held this machine back. Whether it actually
                runs anything is still up to the node itself and to the scheduler.
              {:else}
                Held back by an administrator{selected.contributeReason.trim() ? ` — ${selected.contributeReason}` : ""}.
                It stays a full member: state, submission and terminals are unaffected.
              {/if}
            </p>
            {#if contributeErr}<div class="mt-1.5 text-xs text-error">{contributeErr}</div>{/if}
          </div>

          <!-- Draining (Phase C, U9): Nomad's drain — the instances move and
               the machine stays up. Beside Contributing because the two are
               the pair an operator reaches for before touching a box:
               "send nothing new here" and "move what is here off". -->
          <div class="mt-3 rounded-lg bg-surface-2 px-3.5 py-3">
            <div class="flex items-center gap-2">
              <span class="label">Drain</span>
              <span
                class="rounded-full px-2 py-0.5 text-2xs font-semibold"
                style={selectedDrain
                  ? "background: var(--status-warn-wash); color: var(--status-warn-ink);"
                  : "background: var(--accent-wash); color: var(--accent-ink);"}
              >
                {selectedDrain ? (selectedDrain.leaving ? "shutting down" : "draining") : "not draining"}
              </span>
              {#if isAdmin && !selected.revoked && !selected.left}
                {#if drainAssigned}
                  <button
                    class="linkish ml-auto text-xs"
                    disabled={drainBusy}
                    onclick={() => setDrain(selected!, -1, false, true)}
                  >
                    {drainBusy ? "…" : "lift the drain"}
                  </button>
                {:else if !selectedDrain}
                  <button
                    class="linkish ml-auto text-xs"
                    onclick={() => { drainOpen = !drainOpen; drainErr = null; }}
                  >
                    {drainOpen ? "cancel" : "drain this node"}
                  </button>
                {/if}
              {/if}
            </div>
            <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
              {#if selectedDrain}
                {drainWords(selectedDrain, Math.floor(Date.now() / 1000))}{selectedDrain.keepSystem ? "; its system instances stay" : ""}.
                It takes no new work meanwhile.
              {:else}
                Moves this machine's deployment instances elsewhere — each original stops once its
                replacement has proved itself — without stopping the machine.
              {/if}
            </p>
            {#if drainOpen && !selectedDrain}
              <div class="fade-in mt-2 flex flex-wrap items-center gap-2">
                <select
                  bind:value={drainDeadline}
                  class="select h-[28px] rounded-md bg-surface px-2 text-xs outline-none"
                  style="box-shadow: var(--sh-control);"
                  title="Whatever has not moved by then is stopped"
                >
                  {#each DRAIN_DEADLINES as c (c.secs)}
                    <option value={c.secs}>{c.label}</option>
                  {/each}
                </select>
                <label class="flex items-center gap-1.5 text-xs text-ink-4">
                  <input type="checkbox" bind:checked={drainKeepSystem} />
                  leave system instances
                </label>
                <button
                  class={ui.secondary}
                  disabled={drainBusy}
                  onclick={() => setDrain(selected!, drainDeadline, drainKeepSystem, false)}
                >
                  {drainBusy ? "Draining…" : "Drain"}
                </button>
              </div>
              <div class="mt-1.5 text-xs text-ink-5">
                The same as <code class="mono">meshnode nodes drain {selected.name}</code>. Other
                jobs already running there finish.
              </div>
            {/if}
            {#if drainErr}<div class="mt-1.5 text-xs text-error">{drainErr}</div>{/if}
          </div>

          {#if selected.capacity}
            <div class="label mt-4">Hardware</div>
            <div class="mt-1.5 text-sm text-ink-3">
              <span class="num">{selected.capacity.cores} cores</span>
              <span class="text-ink-5"> · {fmtSize(selected.capacity.memMb)} memory</span>
              {#if t?.memAvailableMb != null}<span class="text-ink-5"> ({fmtSize(t.memAvailableMb)} free)</span>{/if}
            </div>
          {/if}

          <!-- Terminal access, as this node itself reports it. -->
          <div class="mt-4 rounded-lg bg-surface-2 px-3.5 py-3">
            <div class="flex items-center gap-2">
              <span class="label">Terminals</span>
              <span
                class="rounded-full px-2 py-0.5 text-2xs font-semibold"
                style={tstate.tone === "ok"
                  ? "background: var(--accent-wash); color: var(--accent-ink);"
                  : tstate.tone === "warn"
                    ? "background: var(--status-warn-wash); color: var(--status-warn-ink);"
                    : "background: var(--surface); color: var(--ink-5);"}
              >
                {tstate.label}
              </span>
              <button
                class="linkish ml-auto text-xs"
                onclick={() => {
                  sessionStorage.setItem("access:tab", "terminals");
                  app.go("access");
                }}
              >
                who may open one →
              </button>
            </div>
            <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
              {#if selected.noShell}
                Tagged <span class="mono">no-shell</span>: it still runs jobs, and it refuses prompts.
              {:else if !selected.executing}
                A no-execute peer runs no processes, so it hosts no terminals either.
              {:else if selected.acceptsTerminals}
                Accepts terminals. Each one still needs a runner grant and a policy naming the person asking.
              {:else}
                Does not accept terminals — its own operator decides that, on the machine.
              {/if}
            </p>

            {#if selected.isSelf}
              {#if selected.executing}
                <label class="mt-2.5 flex cursor-pointer items-start gap-2.5">
                  <input
                    type="checkbox"
                    class="mt-[3px] h-3.5 w-3.5 accent-[var(--accent)]"
                    checked={selected.noShell}
                    disabled={tagBusy || !isAdmin}
                    onchange={(e) => setNoShell(e.currentTarget.checked)}
                  />
                  <span class="text-xs font-medium text-ink">
                    Refuse terminals on this peer
                    <Hint
                      label="What this does"
                      text="Adds the no-shell tag, which replicates — every node refuses a prompt here from then on, without anybody editing a file."
                    />
                  </span>
                </label>
              {/if}
            {:else if selected.executing && !selected.revoked && !selected.left}
              <!-- `no-shell` is a policy an operator declares about a
                   machine, not a fact the machine knows, so it is assigned
                   from here now rather than copied into an SSH session.
                   Its own tag doc always said as much: the point is not
                   editing a TOML file on six hundred boxes. -->
              {@const mine = selected.assignedTags.includes(NO_SHELL)}
              {#if isAdmin && (!selected.noShell || mine)}
                <label class="mt-2.5 flex cursor-pointer items-start gap-2.5">
                  <input
                    type="checkbox"
                    class="mt-[3px] h-3.5 w-3.5 accent-[var(--accent)]"
                    checked={selected.noShell}
                    disabled={assignBusy}
                    onchange={(e) =>
                      assignTags(
                        selected!,
                        e.currentTarget.checked
                          ? [...selected!.assignedTags, NO_SHELL]
                          : selected!.assignedTags.filter((t) => t !== NO_SHELL),
                        e.currentTarget.checked
                          ? `${selected!.name} will refuse terminals`
                          : `${selected!.name} accepts terminals again`,
                      )}
                  />
                  <span class="text-xs font-medium text-ink">
                    Refuse terminals on {selected.name}
                    <Hint
                      label="What this does"
                      text="Assigns the no-shell tag in the mesh — signed here, replicated, and still on that node after it reboots. It keeps running jobs; nobody gets a prompt. That node's own [sessions] accept = off still wins over it, because the machine's owner has the last word about their own machine."
                    />
                  </span>
                </label>
              {:else if selected.noShell}
                <div class="mt-2.5 flex items-center gap-1 text-2xs leading-relaxed text-ink-6">
                  <span class="mono">{selected.name}</span> announces
                  <span class="mono">no-shell</span> itself
                  <Hint
                    label="Why it cannot be lifted from here"
                    text="A node's own announcements are only ever written by that node — which is what stops one machine relabelling another. Remove it there with `meshnode nodes tag --rm no-shell`."
                  />
                </div>
              {/if}
              {#if !selected.acceptsTerminals}
                <!-- This half really is the machine's own: whether a node
                     accepts terminals at all is its config, and the mesh
                     does not get a vote. -->
                <div class="mt-2.5 flex flex-col gap-1.5">
                  <button
                    class="mono flex items-center gap-2 rounded-sm bg-surface px-2.5 py-1.5 text-left text-2xs text-ink-3 transition-colors hover:text-ink"
                    onclick={() => copyCmd("cfg", '[sessions]\naccept = "on"')}
                  >
                    <span class="min-w-0 flex-1 truncate">[sessions] accept = "on"</span>
                    <span class="shrink-0 text-ink-5">{copiedCmd === "cfg" ? "copied" : "copy"}</span>
                  </button>
                  <span class="flex items-center gap-1 text-2xs leading-relaxed text-ink-6">
                    run on <span class="mono">{selected.name}</span>, then
                    <span class="mono">systemctl reload meshnode</span>
                    <Hint
                      label="Why it must run there"
                      text="Whether a machine hosts prompts at all is its own operator's decision, in its own config — the mesh can forbid terminals on a node, and never grant them."
                    />
                  </span>
                </div>
              {/if}
            {/if}
          </div>
        </div>
      </div>

      <!-- actions -->
      <div class="flex flex-wrap items-center gap-2 border-t border-edge px-6 py-3.5">
        <button
          class={ui.secondary}
          onclick={() => {
            infoOpen = false;
            showFacts(selected);
          }}
        >
          Facts…
        </button>
        <button class={ui.secondary} onclick={() => runOn(selected)}>Run here…</button>
        <button
          class={ui.secondary}
          disabled={!shellable(selected)}
          title={terminalBlocker(selected) ?? "Interactive terminal on this node"}
          onclick={() => {
            infoOpen = false;
            askReason(selected);
          }}
        >
          Terminal…
        </button>
        {#if !selected.isSelf}
          <button class={ui.secondary} disabled={pingBusy} onclick={() => doPing(selected)}>
            {pingBusy ? "Pinging…" : "Ping"}
          </button>
        {:else}
          <button
            class={ui.secondary}
            onclick={() => app.go("settings", "this-peer")}>Tags & settings</button
          >
        {/if}
        {#if !selected.isSelf && !selected.revoked}
          <button
            class="btn btn-danger btn-lg ml-auto shrink-0 text-sm"
            disabled={!isAdmin}
            onclick={() => {
              infoOpen = false;
              revokeTyped = "";
              revokeOpen = true;
            }}
          >
            Revoke from mesh…
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}

<!-- ===================== the callouts' sheets ===================== -->
<Sheet
  open={gangsOpen}
  title="Multi-machine jobs"
  sub="Oldest first. A multi-machine job holds each machine until it has all of them, then they start together."
  icon="gang"
  width={860}
  onclose={() => (gangsOpen = false)}
>
  {#if gangs.length === 0}
    <div class="empty"><div class="empty-title">Nothing is holding machines</div></div>
  {:else}
      <div class="tablewrap overflow-hidden rounded-xl" style="box-shadow: 0 0 0 1px var(--edge-card);">
      <table class="w-full">
        <thead>
          <tr class="border-b border-edge">
            <th class="label px-5 py-2.5 text-left">Job</th>
            <th class="label px-3 py-2.5 text-left">Command</th>
            <th class="label px-3 py-2.5 text-left">User</th>
            <th class="label px-3 py-2.5 text-right">Machines</th>
            <th class="label px-3 py-2.5 text-left">Placement</th>
            <th class="label px-3 py-2.5 pr-5 text-left">State</th>
          </tr>
        </thead>
        <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
          {#each gangs as g (g.gang)}
            <tr class="h-10 border-b border-edge last:border-0">
              <td class="mono px-5 py-2 text-xs text-ink-3">{g.shortId}</td>
              <td class="px-3 py-2 text-sm text-ink-3"><span class="mono text-xs">{g.preview}</span></td>
              <td class="px-3 py-2 text-sm text-ink-4">{g.username}</td>
              <td class="num px-3 py-2 text-right text-sm text-ink-3">{g.held}<span class="text-ink-6">/{g.size}</span></td>
              <td class="px-3 py-2 text-xs text-ink-5">{g.placement === "pack" ? "close together" : g.placement === "spread" ? "spread out" : g.placement}</td>
              <td class="px-3 py-2 pr-5">
                {#if g.state === "offering"}
                  <span class="inline-flex items-center gap-1.5 text-xs" style="color: var(--status-warn-ink);">
                    <Dot kind="queued" size={5} />holding {g.held} of {g.size}
                  </span>
                {:else if g.state === "running"}
                  <span class="inline-flex items-center gap-1.5 text-xs text-running-ink"><Dot kind="running" size={5} decorative />running</span>
                {:else}
                  <span class="text-xs text-ink-5">{g.state}</span>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      </div>
      <!-- The mechanism behind a "?", not in the footnote (UX-9): "a senior
           gang takes reservations from a junior one" was scheduler talk on
           the first screen a buyer sees. -->
      <div class="footnote">
        <span>When two jobs want the same machines, the older one wins them and the younger one waits, rather than both holding half and retrying for ever.</span>
      </div>
  {/if}
</Sheet>

<Sheet
  open={devicesOpen}
  title="Accelerators"
  sub={`${devices.length} device${devices.length === 1 ? "" : "s"} across ${devicesByNode.length} machine${devicesByNode.length === 1 ? "" : "s"}${devicesTroubled > 0 ? ` · ${devicesTroubled} needing attention` : ""}`}
  icon="gpu"
  width={940}
  onclose={() => (devicesOpen = false)}
>
  <!-- Every card in the mesh, from replicated state, so this is the whole
       fleet's hardware whichever node Director is talking to. One card per
       device, grouped by the machine it is in. -->
  <div class="space-y-5">
    {#each devicesByNode as [machine, list] (machine)}
      <div>
        <div class="mb-2 flex items-baseline gap-2">
          <span class="text-sm font-semibold text-ink">{machine}</span>
          <span class="text-xs text-ink-5">{list.length} device{list.length === 1 ? "" : "s"}</span>
        </div>
        <div class="gpu-grid">
          {#each list as d (`${d.nodeId}:${d.kind}:${d.vendor}:${d.index}`)}
            {@const used = d.memMb > 0 && d.memFreeMb != null ? Math.round(((d.memMb - d.memFreeMb) / d.memMb) * 100) : null}
            {@const util = d.utilPct != null ? Math.round(d.utilPct) : null}
            <div class="gpu-card" style={`--vendor: ${vendorFill(d.vendor)};`}>
              <div class="flex items-start gap-2">
                <div class="min-w-0 flex-1">
                  <div class="truncate text-sm font-semibold text-ink" title={d.uuid || d.model}>{d.model}</div>
                  <div class="mt-0.5 text-xs text-ink-5">{vendorLabel(d.vendor)} · #{d.index}{d.heldBy ? ` · held by ${d.heldBy}` : ""}</div>
                </div>
                {#if d.health !== "ok"}
                  <Pill tone={d.health === "unavailable" ? "error" : "warn"} title={d.healthReason ?? ""}>{d.health}</Pill>
                {:else if d.throttled}
                  <Pill tone="warn" title="The device is reporting thermal or power throttling — it will still take work, more slowly.">Throttled</Pill>
                {:else}
                  <Pill tone="ok">ok</Pill>
                {/if}
              </div>
              <div class="mt-3 space-y-2">
                <div>
                  <div class="flex justify-between text-2xs text-ink-5"><span>Busy</span><span class="num">{util != null ? `${util}%` : "—"}</span></div>
                  <div class="meter-track mt-1 h-1.5"><div class="meter-fill" data-load={(util ?? 0) >= 90 ? "hot" : (util ?? 0) >= 70 ? "warn" : "ok"} style={`width:${util ?? 0}%;`}></div></div>
                </div>
                <div>
                  <div class="flex justify-between text-2xs text-ink-5">
                    <span>Memory</span>
                    <span class="num">{d.memMb > 0 ? `${d.memFreeMb != null ? `${fmtSize(d.memFreeMb)} free of ` : ""}${fmtSize(d.memMb)}` : "—"}</span>
                  </div>
                  <div class="meter-track mt-1 h-1.5"><div class="meter-fill" style={`width:${used ?? 0}%; background: var(--hue-indigo);`}></div></div>
                </div>
              </div>
              {#if d.health !== "ok" && d.healthReason}
                <div class="mt-2 text-xs" style="color: var(--status-warn-ink);">{d.healthReason}</div>
              {/if}
            </div>
          {/each}
        </div>
      </div>
    {/each}
  </div>
  <p class="mt-4 mb-0 text-xs text-ink-5">
    Each machine describes its own hardware. “Held by” is known for this peer only — a device hold is
    node-local, so there is no honest way to read another machine's.
  </p>
</Sheet>

<Sheet
  open={sessionsOpen}
  title="Live sessions"
  sub="In is somebody holding a prompt on this machine; out is this machine reaching elsewhere."
  icon="terminal"
  width={820}
  onclose={() => (sessionsOpen = false)}
>
  {#if sessions.length === 0}
    <div class="empty"><div class="empty-title">No live sessions</div></div>
  {:else}
      <div class="overflow-hidden rounded-xl" style="box-shadow: 0 0 0 1px var(--edge-card);">
      {#each sessions as s (s.id)}
        <div class="flex items-center gap-3 border-b border-edge px-5 py-2 text-sm last:border-0">
          <span
            class="rounded-sm px-1.5 py-0.5 text-2xs font-semibold"
            style={s.hosting
              ? "background: var(--status-warn-wash); color: var(--status-warn-ink);"
              : "background: var(--accent-wash); color: var(--accent-ink);"}
          >
            {s.hosting ? "in" : "out"}
          </span>
          <span class="mono text-xs text-ink-3">{s.kind}</span>
          <span class="font-semibold text-ink">{s.peerName}</span>
          <span class="mono min-w-0 flex-1 truncate text-xs text-ink-4">{s.reason || s.detail}</span>
          <span class="text-xs text-ink-5">{s.username}</span>
          <span class="num text-xs text-ink-5">{traffic(s)}</span>
          <span class="num text-xs text-ink-5">{ago(Number(s.openedAtMicros))}</span>
          <button class="linkish text-xs" onclick={() => closeSession(s.id)}>close</button>
        </div>
      {/each}
      </div>
  {/if}
</Sheet>

<FactsModal subject={factsFor ?? ""} open={factsFor !== null} onclose={() => (factsFor = null)} />

<Modal open={reasonOpen} kicker="open a terminal" peer={reasonFor?.name ?? ""} onclose={() => (reasonOpen = false)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Terminal on {reasonFor?.name}</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      An interactive shell on {reasonFor?.name}, through the mesh — no SSH, no inbound port.
      It runs as the daemon's own account, is refused unless a session policy names you, and
      is audited at open and close with your name and the reason below.
    </p>
    <label class="mt-4 block">
      <span class="mb-1.5 block text-xs text-inkcode-3">Reason (recorded in the audit log)</span>
      <input
        bind:value={reasonDraft}
        placeholder="INC-4821 — investigating disk pressure"
        onkeydown={(e) => e.key === "Enter" && openTerminal()}
        class="h-10 w-full rounded-md border px-3 text-base text-inkcode outline-none"
        style="background: rgba(160,176,200,.08); border-color: var(--edge-code);"
      />
    </label>
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button
        class="btn h-11 border border-edge-code text-base font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]"
        onclick={() => (reasonOpen = false)}
      >
        Cancel
      </button>
      <button class="btn btn-primary h-11 text-base" disabled={!reasonDraft.trim()} onclick={openTerminal}>
        {reasonDraft.trim() ? "Open terminal" : "A reason is required"}
      </button>
    </div>
  </div>
</Modal>

<Modal open={revokeOpen} kicker="revoke node" peer={selected?.shortId ?? ""} onclose={() => (revokeOpen = false)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Revoke {selected?.name}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      Revocation is <span class="font-semibold" style="color: var(--status-error-ink);">permanent</span>:
      this node's key can never rejoin the mesh, and every member refuses it at the transport layer.
    </p>
    <label class="mt-4 block">
      <span class="mb-1.5 block text-xs text-inkcode-3">Reason (audited)</span>
      <input bind:value={revokeReason} class="h-10 w-full rounded-md border px-3 text-base text-inkcode outline-none" style="background: rgba(160,176,200,.08); border-color: var(--edge-code);" />
    </label>
    <label class="mt-3 block">
      <span class="mb-1.5 block text-xs text-inkcode-3">Type <span class="mono" style="color: var(--status-error-ink);">{selected?.name}</span> to arm</span>
      <input bind:value={revokeTyped} placeholder={selected?.name} class="mono h-10 w-full rounded-md border px-3 text-base text-inkcode outline-none" style="background: rgba(160,176,200,.08); border-color: var(--edge-code);" />
    </label>
    {#if revokeErr}
      <div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{revokeErr}</div>
    {/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button class="btn h-11 border border-edge-code text-base font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]" onclick={() => (revokeOpen = false)}>Cancel</button>
      <button class="btn btn-destructive h-11 text-base" disabled={!revokeArmed || revokeBusy} onclick={doRevoke}>{revokeBusy ? "Revoking…" : "Revoke permanently"}</button>
    </div>
  </div>
</Modal>

<style>
  /* -- the overview (2026-09-22) ------------------------------------------
     Three cards side by side on a wide window, Hardware under the other
     two below 1180px, and a column below 760px. */
  .fleet-overview {
    display: grid;
    gap: 16px;
    grid-template-columns: repeat(3, minmax(0, 1fr));
  }
  @media (max-width: 1180px) {
    .fleet-overview {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
    .fleet-overview > :last-child {
      grid-column: 1 / -1;
    }
  }
  @media (max-width: 760px) {
    .fleet-overview {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .hw-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 10px;
  }
  .hw-cell {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
    padding: 10px 12px;
    border-radius: var(--radius-lg);
    background: var(--surface-2);
    text-align: left;
  }
  .hw-btn {
    cursor: pointer;
    transition:
      background-color var(--dur-fast) var(--ease),
      box-shadow var(--dur-fast) var(--ease);
  }
  .hw-btn:hover:not(:disabled) {
    background: var(--surface-3);
  }
  .hw-btn:disabled {
    cursor: default;
  }
  .hw-fig {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 18px;
    font-weight: 700;
    letter-spacing: -0.02em;
    color: var(--ink-1);
    line-height: 1.15;
  }
  .hw-warn {
    display: inline-grid;
    place-items: center;
    min-width: 17px;
    height: 17px;
    padding: 0 4px;
    border-radius: 999px;
    background: var(--status-warn-wash);
    color: var(--status-warn-ink);
    font-size: 10.5px;
    font-weight: 700;
    letter-spacing: 0;
  }

  .fleet-callouts {
    display: grid;
    gap: 12px;
    grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  }
  .fleet-callouts :global(.callout) {
    color: var(--ink-2);
  }

  /* -- a machine, as a card ---------------------------------------------- */
  .mcard-grid {
    display: grid;
    gap: 14px;
    grid-template-columns: repeat(auto-fill, minmax(272px, 1fr));
  }
  .mcard {
    position: relative;
    display: flex;
    flex-direction: column;
    min-width: 0;
    padding: 15px 16px 0;
    border-radius: var(--radius-xl);
    background: var(--surface-1);
    box-shadow: var(--sh-card-lg);
    overflow: hidden;
    outline: none;
  }
  /* A machine that needs a look wears its state along the top edge. */
  .mcard::before {
    content: "";
    position: absolute;
    inset: 0 0 auto 0;
    height: 3px;
    background: var(--edge-tone, transparent);
  }
  .mcard.is-self {
    background:
      linear-gradient(180deg, color-mix(in srgb, var(--accent) 7%, transparent), transparent 60%),
      var(--surface-1);
  }
  .mcard.is-chosen {
    box-shadow:
      0 0 0 2px color-mix(in srgb, var(--hue, var(--accent)) 60%, transparent),
      var(--sh-card-lg);
  }
  .mcard.is-open {
    box-shadow:
      0 0 0 2px var(--accent),
      var(--sh-card-lg);
  }
  .mcard.is-gone {
    opacity: 0.55;
  }
  .mcard:focus-visible {
    box-shadow: var(--shadow-focus), var(--sh-lift);
  }
  /* The tick shows on hover, and stays once anything is chosen. */
  .mcard-check {
    display: grid;
    place-items: center;
    opacity: 0;
    transition: opacity var(--dur-fast) var(--ease);
  }
  .mcard:hover .mcard-check,
  .mcard:focus-within .mcard-check,
  .mcard-check.is-on {
    opacity: 1;
  }
  .mcard-specs {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 12px;
    font-size: var(--text-sm);
    color: var(--ink-2);
  }
  .mcard-specs > span {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .mcard-specs > span > :global(span:first-child) {
    color: var(--ink-faint);
  }
  .mcard-foot {
    display: flex;
    align-items: center;
    gap: 10px;
    min-height: 40px;
    margin: auto -16px 0;
    padding: 8px 16px;
    border-top: 1px solid var(--edge);
    background: color-mix(in srgb, var(--surface-2) 45%, transparent);
  }

  /* -- an accelerator ------------------------------------------------------ */
  .gpu-grid {
    display: grid;
    gap: 10px;
    grid-template-columns: repeat(auto-fill, minmax(250px, 1fr));
  }
  .gpu-card {
    position: relative;
    padding: 13px 14px 14px 17px;
    border-radius: var(--radius-lg);
    background: var(--surface-1);
    box-shadow: 0 0 0 1px var(--edge-card);
    overflow: hidden;
  }
  .gpu-card::before {
    content: "";
    position: absolute;
    inset: 0 auto 0 0;
    width: 4px;
    background: var(--vendor);
  }

  .fleet-tools {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
  }
  .fleet-search {
    display: flex;
    align-items: center;
    gap: 7px;
    flex: 1 1 260px;
    max-width: 380px;
    height: 32px;
    padding: 0 8px 0 10px;
    border-radius: var(--radius-md);
    background: var(--surface-1);
    box-shadow: var(--sh-control);
    color: var(--ink-faint);
  }
  .fleet-search:focus-within {
    box-shadow:
      0 0 0 2px color-mix(in srgb, var(--accent) 35%, transparent),
      var(--sh-control);
  }
  .fleet-search input {
    min-width: 0;
    flex: 1;
    background: transparent;
    color: var(--ink-1);
    font-size: var(--text-sm);
    outline: none;
  }
  .fleet-search input::placeholder {
    color: var(--ink-muted);
  }
  .fleet-clear {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    border-radius: 999px;
    color: var(--ink-5);
  }
  .fleet-clear:hover {
    background: var(--surface-2);
    color: var(--ink-1);
  }
  .fleet-bulk {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    margin-bottom: 10px;
    padding: 8px 12px;
    border-radius: var(--radius-lg);
    background: var(--hue-wash, var(--accent-wash));
  }
  .sorth {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font: inherit;
    letter-spacing: inherit;
    text-transform: inherit;
    color: inherit;
    cursor: pointer;
  }
  .sorth:hover,
  .sorth.is-on {
    color: var(--ink-1);
  }
  .sorth-arrow {
    display: inline-grid;
    opacity: 0;
    transition:
      opacity var(--dur-fast) var(--ease),
      transform var(--dur-fast) var(--ease);
  }
  .sorth:hover .sorth-arrow {
    opacity: 0.45;
  }
  .sorth.is-on .sorth-arrow {
    opacity: 1;
  }
  .sorth-arrow.is-desc {
    transform: rotate(180deg);
  }
  /* A right-aligned head keeps its label flush with the numbers under it. */
  :global(th.text-right) > .sorth {
    flex-direction: row-reverse;
  }
  /* More table to the right than the card shows: fade the edge so the
     cut-off columns read as "scroll", not as the end of the table. */
  :global(.tablewrap[data-more-right]) {
    mask-image: linear-gradient(to right, #000 calc(100% - 48px), transparent);
  }
</style>
