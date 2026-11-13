<script lang="ts">
  // Node software (M24 §4, §8, §9) — the Updates panel of Mesh.
  //
  // Rebuilt around one question and one button. The question is "is my
  // fleet on the newest build", answered at the top in a sentence and a
  // ring. The button updates whatever is behind, in waves the daemon
  // health-gates between (M24 §4) — the rollout mechanics are unchanged,
  // what changed is that nobody types a selector or a batch size to reach
  // them. Getting a new release *into* the mesh is the same one click
  // against the release channel, verified against a key the fleet trusts.
  // Nodes that cannot replace their own binary get a way that still needs
  // nobody at a keyboard on them: Update over SSH.
  //
  // Under the answer sits the path to it, as three steps that each say
  // whether they are done: the channel, the mesh, the nodes. That is the
  // whole "how do I update" — read left to right, act on the one that is
  // not ticked. Everything the previous screen could do is still here,
  // under Advanced: ticking nodes by hand, the release keys, publishing a
  // build from a folder that Director remembers between visits.
  //
  // **Cards and sheets (2026-09-22).** The screen still said all of that,
  // but as a column of text: a small ring, a strip of steps, a list of
  // strips, then three folds that opened in place and pushed everything
  // else down. It is laid out to be read in one look now — a big ring with
  // its legend, the steps joined as a path, the fleet as a card per group —
  // and the three Advanced tasks are tiles that open in a sheet. Nothing
  // it could do went; it moved.
  //
  // It was a tab of the node table until the rail grew a row for it; the
  // arithmetic behind "how many are behind" moved out to `lib/versions.ts`
  // with it, because the rail badges that row with the same figure.
  import { api, errText, type NodeView, type ReleaseCheckView, type ReleaseFetchEvent } from "../api";
  import type { ReleaseTrustView } from "../bindings/ReleaseTrustView";
  import type { BuildCandidateView } from "../bindings/BuildCandidateView";
  import { app, ui } from "../state.svelte";
  import { versionCmp, versionGroupsOf, liveOf, fleetOf, targetOf } from "../versions";
  import Dot from "./Dot.svelte";
  import Hint from "./Hint.svelte";
  import Icon from "./Icon.svelte";
  import Spinner from "./Spinner.svelte";
  import DeploySheet from "./DeploySheet.svelte";
  import SshUpgradeSheet from "./SshUpgradeSheet.svelte";
  import FleetGrid from "./FleetGrid.svelte";
  import Ring from "./Ring.svelte";
  import Sheet from "./Sheet.svelte";

  const isAdmin = $derived(app.whoami?.isFounder || app.whoami?.roles.includes("admin"));

  // -- the fleet -----------------------------------------------------------------
  const liveNodes = $derived(liveOf(app.nodes));
  const selfNode = $derived(liveNodes.find((n) => n.isSelf) ?? null);
  const fleet = $derived(fleetOf(app.nodes));
  const versionGroups = $derived(versionGroupsOf(fleet));

  // -- the release channel and what is published --------------------------------
  let check = $state<ReleaseCheckView | null>(null);
  let checking = $state(false);
  let checkErr = $state<string | null>(null);
  let published = $state<string[]>([]);

  async function runCheck() {
    checking = true;
    checkErr = null;
    try {
      check = await api.releaseCheck();
      published = check.published.map((p) => p.version);
    } catch (e) {
      checkErr = errText(e);
    } finally {
      checking = false;
    }
  }
  $effect(() => {
    // Cheap and local first, so the screen has an answer before the
    // channel does; the channel check follows on its own.
    void api.releaseVersions().then((v) => (published = v)).catch(() => {});
    void runCheck();
  });

  /** What the fleet *should* be on: the newest thing published, running,
   *  or carried by this Director (`targetOf`, which the rail badge reads). */
  const newest = $derived.by(() => {
    const running = targetOf(app.nodes, app.about?.bundledMeshnode);
    const top = [...published, ...(running ? [running] : [])]
      .filter((v) => v && v !== "unknown")
      .sort(versionCmp)
      .at(-1);
    return top ?? running;
  });
  let upVersion = $state("");
  const target = $derived(upVersion.trim().replace(/^v/i, "") || newest);
  const outdated = $derived(target ? fleet.filter((n) => n.version !== target) : []);
  const current = $derived(target ? fleet.filter((n) => n.version === target) : []);
  const channelNewer = $derived(
    !!check?.channelLatest && (!newest || versionCmp(check.channelLatest, newest) > 0),
  );
  /** The newest version is published for every architecture the fleet has. */
  const publishedForFleet = $derived.by(() => {
    if (!check || !newest) return true;
    const rel = check.published.find((p) => p.version === newest);
    if (!rel) return false;
    return check.fleetArches.every((a) => rel.arches.length === 0 || rel.arches.includes(a));
  });

  // -- getting a release into the mesh ------------------------------------------
  let fetchBusy = $state(false);
  let fetchRows = $state<{ arch: string; stage: string; detail: string }[]>([]);
  let fetchErr = $state<string | null>(null);
  let fetchNote = $state<string | null>(null);

  async function getRelease(version: string) {
    fetchBusy = true;
    fetchErr = null;
    fetchNote = null;
    fetchRows = [];
    try {
      await api.releaseFetch(version, check?.fleetArches ?? [], (ev: ReleaseFetchEvent) => {
        if (ev.type === "progress") {
          const i = fetchRows.findIndex((r) => r.arch === ev.arch);
          const row = { arch: ev.arch, stage: ev.stage, detail: ev.detail };
          fetchRows = i >= 0 ? fetchRows.map((r, k) => (k === i ? row : r)) : [...fetchRows, row];
        } else if (ev.type === "done") {
          fetchNote = `meshnode ${ev.version} is in the mesh for ${ev.arches.join(" and ")}.`;
          upVersion = ev.version;
          fetchBusy = false;
          void runCheck();
          void app.refresh();
        } else {
          fetchErr = ev.message;
          fetchBusy = false;
        }
      });
    } catch (e) {
      fetchErr = errText(e);
      fetchBusy = false;
    }
  }

  // -- the rollout ---------------------------------------------------------------
  //
  // The plan carries a selector string and every node re-parses it and
  // checks that it names itself before touching its binary. A whole
  // version group is written as `tag=cap.ver=…` so a node somebody else
  // upgraded mid-flight drops out by itself; anything else is the names,
  // joined by `|` (M24 §10).
  let upBusy = $state(false);
  let upErr = $state<string | null>(null);
  /** Roll the target out to nodes already reporting it, instead of
   *  skipping them.
   *
   *  A node reports its own version and nothing checks the claim, so the
   *  version is the right thing to skip on right up until it is the thing
   *  in doubt — a binary replaced by hand, an install that half-finished,
   *  a rebuild that kept its number. This is the switch for that case, and
   *  it is one switch for the whole tab: the rollout, the hand-picked
   *  roll, and the SSH push all mean the same thing by it. */
  let forceUpdate = $state(false);
  let upRows = $state<{ node: string; state: string; detail: string }[]>([]);
  let upSummary = $state<string | null>(null);
  let upBatch = $state(0);

  function clauseFor(n: { name: string; shortId: string }): string {
    return /^[^,|=\s]+$/.test(n.name) ? `name=${n.name}` : `id=${n.shortId}`;
  }
  function selectorFor(nodes: NodeView[]): string {
    if (nodes.length === 0) return "";
    const whole = versionGroups.find(
      (g) => g.version !== "unknown" && g.nodes.length === nodes.length && g.nodes.every((n) => nodes.includes(n)),
    );
    if (whole && whole.nodes.length > 1) return `tag=cap.ver=${whole.version}`;
    return nodes.map(clauseFor).join("|");
  }
  /** Waves that let the health gate stop something early without making
   *  a big fleet wait on one machine at a time. */
  function autoBatch(n: number): number {
    return n <= 3 ? 1 : Math.min(8, Math.ceil(n / 4));
  }

  /** Nodes a rollout said it could not drive — the SSH path's customers. */
  let needsHands = $state<string[]>([]);
  const HANDS = /by hand|package manager|apt |dnf |read-only|cannot replace|ProtectSystem|too old|no code for/i;

  async function rollout(nodes: NodeView[], batch: number, force = forceUpdate) {
    if (!target || nodes.length === 0) return;
    upBusy = true;
    upErr = null;
    upRows = [];
    upSummary = null;
    needsHands = [];
    upBatch = batch;
    try {
      await api.upgradeFleet(selectorFor(nodes), target, batch, false, force, (ev) => {
        if (ev.type === "node") {
          upRows = [...upRows, { node: ev.node, state: ev.state, detail: ev.detail }];
          if (ev.state === "failed" && HANDS.test(ev.detail) && !needsHands.includes(ev.node)) {
            needsHands = [...needsHands, ev.node];
          }
        } else if (ev.type === "done") {
          const driver = ev.driverRestarting
            ? " This peer is restarting into the target and is not counted."
            : "";
          upSummary = ev.stoppedAt
            ? `Stopped at ${ev.stoppedAt}: ${ev.upgraded} updated, ${ev.already} already current, ${ev.remaining} not started. The mesh keeps working — the failed card says what to do.${driver}`
            : `${ev.upgraded} updated and seen back, ${ev.already} already current.${driver}`;
          upBusy = false;
          void app.refresh();
        } else {
          upErr = ev.message;
          upBusy = false;
        }
      });
    } catch (e) {
      upErr = errText(e);
      upBusy = false;
    }
  }

  // -- SSH fallback and adding machines -----------------------------------------
  let sshOpen = $state(false);
  let sshNodes = $state<NodeView[]>([]);
  let deployOpen = $state(false);

  function openSsh(nodes: NodeView[]) {
    sshNodes = nodes;
    sshOpen = true;
  }
  const handsNodes = $derived(fleet.filter((n) => needsHands.includes(n.name)));

  // -- advanced: choose by hand --------------------------------------------------
  let picked = $state(new Set<string>());
  let anchor = $state<number | null>(null);
  let manualBatch = $state(1);
  const rows = $derived(versionGroups.flatMap((g) => [...g.nodes].sort((a, b) => a.name.localeCompare(b.name))));
  $effect(() => {
    const live = new Set(fleet.map((n) => n.nodeId));
    if ([...picked].some((id) => !live.has(id))) picked = new Set([...picked].filter((id) => live.has(id)));
  });
  const pickedNodes = $derived(rows.filter((n) => picked.has(n.nodeId)));
  function togglePick(i: number, shift: boolean) {
    const next = new Set(picked);
    if (shift && anchor !== null) {
      const [from, to] = anchor < i ? [anchor, i] : [i, anchor];
      const on = !picked.has(rows[i].nodeId);
      for (let k = from; k <= to; k++) {
        if (on) next.add(rows[k].nodeId);
        else next.delete(rows[k].nodeId);
      }
    } else {
      const id = rows[i].nodeId;
      if (next.has(id)) next.delete(id);
      else next.add(id);
      anchor = i;
    }
    picked = next;
  }
  function pickGroup(g: { nodes: { nodeId: string }[] }, on: boolean) {
    const next = new Set(picked);
    for (const n of g.nodes) {
      if (on) next.add(n.nodeId);
      else next.delete(n.nodeId);
    }
    picked = next;
    anchor = null;
  }
  const suggestedRings = $derived.by(() => {
    const counts = new Map<string, number>();
    for (const n of outdated) for (const t of n.tags) counts.set(t, (counts.get(t) ?? 0) + 1);
    return [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6).map(([tag, n]) => ({ tag, n }));
  });
  function pickRing(tag: string) {
    picked = new Set(outdated.filter((n) => n.tags.includes(tag)).map((n) => n.nodeId));
    anchor = null;
  }

  // -- advanced: release keys and publishing a build (§9) -----------------------
  let trust = $state<ReleaseTrustView | null>(null);
  let trustBusy = $state(false);
  let trustErr = $state<string | null>(null);
  let keyDraft = $state("");
  let keyLabel = $state("");
  /** The dist/ folder, kept between visits so Publish is one click. The
   *  backend remembers it (SEC-14); this is only the path to show. */
  let buildDir = $state("");
  let builds = $state<BuildCandidateView[]>([]);
  let scanning = $state(false);
  let pubBusy = $state(false);
  let pubErr = $state<string | null>(null);
  let pubNote = $state<string | null>(null);

  async function loadTrust() {
    try {
      trust = await api.releaseTrust();
    } catch (e) {
      trustErr = errText(e);
    }
  }
  $effect(() => {
    void loadTrust();
    // A remembered folder is scanned on arrival: the point of remembering
    // it is that the builds are already listed when the fold opens.
    void (async () => {
      buildDir = (await api.releaseBuildDir().catch(() => null)) ?? "";
      if (buildDir) await scanBuilds();
    })();
  });
  /** Pasted key text, or a picked `.pub` file's token. */
  async function trustKey(source: string, file: string | null = null) {
    trustBusy = true;
    trustErr = null;
    try {
      await api.releaseKeyTrust(source, file, keyLabel.trim() || null);
      keyDraft = "";
      keyLabel = "";
      await loadTrust();
      await runCheck();
      if (buildDir) await scanBuilds();
    } catch (e) {
      trustErr = errText(e);
    } finally {
      trustBusy = false;
    }
  }
  async function pickKeyFile() {
    const f = await api.pickFile("Pick your release public key (.pub)", ["pub"]);
    if (f) await trustKey("", f.token);
  }
  /** An SSH-form key being trusted while none is yet: the moment every
   *  release starts needing a co-signature (Y6), which is worth saying
   *  before the click rather than at the next upgrade. */
  const firstSshKey = $derived(
    /^(ssh-ed25519|sk-ssh-ed25519@openssh\.com) /.test(keyDraft.trim()) && !trust?.cosignRequired,
  );
  let cosignBusy = $state<string | null>(null);
  let cosignNote = $state<string | null>(null);
  /** Attach the co-signature `ssh-keygen -Y sign -n meshware-release` made
   *  for one published binary. */
  async function attachCosign(digest: string) {
    trustErr = null;
    cosignNote = null;
    const f = await api.pickFile("Pick the co-signature (.sig)", ["sig"]).catch((e) => {
      trustErr = errText(e);
      return null;
    });
    if (!f) return;
    cosignBusy = digest;
    try {
      cosignNote = await api.releaseCosign(f.token, digest);
      await loadTrust();
    } catch (e) {
      trustErr = errText(e);
    } finally {
      cosignBusy = null;
    }
  }
  async function revokeKey(pubkey: string) {
    trustBusy = true;
    trustErr = null;
    try {
      await api.releaseKeyRevoke(pubkey);
      await loadTrust();
    } catch (e) {
      trustErr = errText(e);
    } finally {
      trustBusy = false;
    }
  }
  async function pickBuildDir() {
    const d = await api.pickDirectory("Pick your build output folder (dist/)");
    if (!d) return;
    buildDir = d.path;
    await scanBuilds(d.token);
  }
  function forgetBuildDir() {
    buildDir = "";
    builds = [];
    pubErr = null;
    pubNote = null;
    void api.releaseBuildDirForget().catch(() => {});
  }
  /** `token`: a folder just picked; without one, the remembered folder. */
  async function scanBuilds(token: string | null = null) {
    pubErr = null;
    pubNote = null;
    scanning = true;
    try {
      builds = await api.releaseCandidates(token);
      if (builds.length === 0) pubErr = "No meshnode builds in this folder — expected files named meshnode-v<version>-<arch>-unknown-linux-musl.";
    } catch (e) {
      builds = [];
      pubErr = errText(e);
    } finally {
      scanning = false;
    }
  }
  const publishable = $derived(builds.filter((b) => !b.blocked));
  async function publish(b: BuildCandidateView) {
    pubBusy = true;
    pubErr = null;
    pubNote = null;
    try {
      pubNote = await api.releasePublish(b.token, b.version);
      upVersion = b.version;
      await scanBuilds();
      await runCheck();
      await app.refresh();
    } catch (e) {
      pubErr = errText(e);
    } finally {
      pubBusy = false;
    }
  }

  /** Who the headline button rolls to: what is behind, or the whole fleet
   *  when the point is to reinstall what is already there. */
  const rolloutTargets = $derived(forceUpdate ? fleet : outdated);

  // -- the ring ------------------------------------------------------------------
  //
  // Three arcs, not two: a node that never announced a build is behind by
  // the same rule, but "update it" is not obviously the fix — it may be a
  // build from before versions were announced — so it is its own colour.
  const isUnknown = (n: NodeView) => !n.version || n.version === "unknown";
  const behindKnown = $derived(outdated.filter((n) => !isUnknown(n)));
  const behindUnknown = $derived(outdated.filter(isUnknown));
  const fraction = $derived(fleet.length ? current.length / fleet.length : 1);
  const allCurrent = $derived(fleet.length > 0 && outdated.length === 0);

  // -- advanced, in sheets ------------------------------------------------------
  let advOpen = $state<"manual" | "keys" | "publish" | null>(null);
  const manualSummary = $derived(pickedNodes.length ? `${pickedNodes.length} ticked` : "none ticked");
  const keysSummary = $derived(
    trust
      ? trust.localOverride
        ? "this node uses its own [release] pubkey"
        : `${trust.keys.filter((k) => !k.revoked).length} trusted, mesh-wide`
      : "loading…",
  );
  const publishSummary = $derived(
    buildDir
      ? publishable.length
        ? `${publishable.length} ready to publish`
        : builds.length
          ? `${builds.length} found, none publishable`
          : "folder remembered"
      : "no build folder yet",
  );

  // -- the three steps -----------------------------------------------------------
  //
  // "How do I update" answered as a row: is there a release, is it in the
  // mesh, is it on the nodes. Each step is one of four states and the
  // one that is `action` is the one with the button.
  type StepState = "busy" | "ok" | "action" | "warn" | "todo";
  const channelDown = $derived(!!checkErr || !!check?.channelError);
  const step1 = $derived<StepState>(
    checking && !check ? "busy" : channelDown ? "warn" : !check?.channelLatest ? "todo" : channelNewer ? "action" : "ok",
  );
  const step2 = $derived<StepState>(
    fetchBusy ? "busy" : !newest ? "todo" : channelNewer || !publishedForFleet ? "action" : "ok",
  );
  const step3 = $derived<StepState>(
    upBusy ? "busy" : fleet.length === 0 ? "todo" : outdated.length > 0 ? "action" : "ok",
  );
</script>

{#snippet stepMark(state: StepState, icon: string)}
  <span class="step-tile" data-state={state}>
    {#if state === "busy"}
      <Spinner size={16} color="var(--status-running)" track="color-mix(in srgb, var(--status-running) 22%, transparent)" />
    {:else if state === "ok"}
      <Icon name="check" size={16} />
    {:else}
      <Icon name={icon} size={17} />
    {/if}
  </span>
{/snippet}

{#snippet advTile(icon: string, title: string, summary: string, marked: boolean, onopen: () => void)}
  <button class="adv-tile lift" onclick={onopen}>
    <span class="icon-tile" style="--tone: var(--hue-indigo); --tone-ink: var(--hue-indigo-ink);"><Icon name={icon} size={16} /></span>
    <span class="min-w-0 flex-1 text-left">
      <span class="block text-sm font-semibold text-ink">{title}</span>
      <span class="block truncate text-xs" style={marked ? "color: var(--accent-ink);" : "color: var(--ink-5);"}>{summary}</span>
    </span>
    <span class="shrink-0 text-ink-faint"><Icon name="chevron-right" size={14} /></span>
  </button>
{/snippet}

<div class="fade-in space-y-4">
  <!-- ═══════════ the answer ═══════════ -->
  <div class="panel hero">
    <div class="flex flex-wrap items-center gap-7">
      <div class="flex shrink-0 items-center gap-6">
        <Ring
          size={132}
          thickness={13}
          total={Math.max(1, fleet.length)}
          arcs={[
            { value: current.length, color: "var(--status-ok)", label: `${current.length} current` },
            { value: behindKnown.length, color: "var(--status-warn)", label: `${behindKnown.length} behind` },
            { value: behindUnknown.length, color: "var(--hue-gray)", label: `${behindUnknown.length} on an unknown build` },
          ]}
          label={fleet.length ? `${Math.round(fraction * 100)}% of the fleet is current` : "No nodes yet"}
        >
          <span class="num block text-[26px] font-bold tracking-[-.03em] text-ink">{fleet.length ? Math.round(fraction * 100) : "—"}<span class="text-sm font-semibold text-ink-5">{fleet.length ? "%" : ""}</span></span>
          <span class="mt-1 block text-2xs font-medium text-ink-5">current</span>
        </Ring>
        {#if fleet.length}
          <div class="legend">
            <div class="legend-row" style="--sw: var(--status-ok);"><span class="sw"></span>Current<span class="n">{current.length}</span></div>
            <div class="legend-row" style="--sw: var(--status-warn);"><span class="sw"></span>Behind<span class="n">{behindKnown.length}</span></div>
            {#if behindUnknown.length}
              <div class="legend-row" style="--sw: var(--hue-gray);"><span class="sw"></span>Unknown build<span class="n">{behindUnknown.length}</span></div>
            {/if}
          </div>
        {/if}
      </div>

      <div class="min-w-[260px] flex-1">
        {#if target}
          <span class="fact mb-2.5" class:ok={allCurrent}>Target <b class="mono">{target}</b></span>
        {/if}
        <h2 class="m-0 text-[21px] font-bold tracking-[-.025em] text-ink">
          {#if fleet.length === 0}
            No nodes to update yet
          {:else if allCurrent}
            Every node runs meshnode {target}
          {:else}
            {outdated.length} of {fleet.length} node{fleet.length === 1 ? "" : "s"} {outdated.length === 1 ? "is" : "are"} behind
          {/if}
        </h2>
        <p class="mt-1.5 mb-0 max-w-[58ch] text-sm leading-normal text-ink-4" style="text-wrap: pretty;">
          {#if fleet.length === 0}
            Add machines and they appear here with the build each one reports.
          {:else if allCurrent}
            {#if channelNewer}
              A newer release, <span class="mono">{check?.channelLatest}</span>, is on the release channel. Get it into the mesh, then update.
            {:else if forceUpdate}
              Reinstalling puts <span class="mono">{target}</span> back on all {fleet.length} of them, whatever each one says it runs.
            {:else}
              Nothing to do. Each node reports its own build, and all of them report this one.
            {/if}
          {:else}
            Director installs <span class="mono">{target}</span> on each machine over SSH, a few at a time, and waits for the mesh to see each one come back.
          {/if}
        </p>
        <div class="mt-4 flex flex-wrap items-center gap-2">
          {#if rolloutTargets.length > 0}
            <!-- Over SSH, because that is the one that works everywhere. The
                 shipped unit runs the daemon unprivileged under
                 `ProtectSystem=full`, so a node cannot write its own binary
                 unless somebody installed the opt-in drop-in on it first.
                 In place is still here, under Advanced, for fleets that
                 did opt in. -->
            <button
              class="btn btn-primary btn-lg"
              disabled={!isAdmin || app.engineStopped}
              onclick={() => openSsh(rolloutTargets)}
              title={!isAdmin ? "changing what the mesh is needs the admin role" : ""}
            >
              <Icon name="download" size={14} />
              {forceUpdate ? "Reinstall on" : "Update"} {rolloutTargets.length} node{rolloutTargets.length === 1 ? "" : "s"}
            </button>
          {/if}
          <button class="btn btn-secondary btn-lg text-sm" disabled={checking} onclick={runCheck}>
            <Icon name="refresh" size={13} /> {checking ? "Checking…" : "Check for updates"}
          </button>
        </div>
      </div>
    </div>

    <!-- The path, as three steps joined by a line. Read left to right; the
         one that is not ticked is the one with the button. The wrapper is a
         size container, so the steps stack by the card's width rather than
         the window's (UX-11). -->
    <div class="steps-wrap">
      <ol class="steps">
        <li class="step" data-state={step1}>
          {@render stepMark(step1, "download")}
          <div class="step-title">Release channel</div>
          <div class="step-detail">
            {#if step1 === "busy"}
              Checking…
            {:else if channelDown}
              <span style="color: var(--status-warn-ink);">Did not answer</span>
            {:else if check?.channelLatest}
              Newest is <span class="mono text-ink-2">{check.channelLatest}</span>
            {:else}
              No release found
            {/if}
          </div>
          {#if step1 === "action" && isAdmin && check?.channelLatest}
            <button class={ui.rowAction} disabled={fetchBusy || !check.canVerify || app.engineStopped} onclick={() => getRelease(check?.channelLatest ?? "")} title={!check.canVerify ? "trust a release key first" : ""}>
              Get {check.channelLatest}
            </button>
          {/if}
        </li>
        <li class="step" data-state={step2}>
          {@render stepMark(step2, "mesh")}
          <div class="step-title">In the mesh</div>
          <div class="step-detail">
            {#if step2 === "busy"}
              Getting {check?.channelLatest ?? newest}…
            {:else if !newest}
              Nothing published yet
            {:else if !publishedForFleet}
              <span class="mono text-ink-2">{newest}</span> <span style="color: var(--status-warn-ink);">missing an architecture</span>
            {:else if channelNewer}
              Holds <span class="mono text-ink-2">{newest}</span>, behind the channel
            {:else}
              Holds <span class="mono text-ink-2">{newest}</span>, the newest
            {/if}
          </div>
          {#if step2 === "action" && !channelNewer && newest && isAdmin && check}
            <button class={ui.rowAction} disabled={fetchBusy || !check.canVerify || app.engineStopped} onclick={() => getRelease(newest ?? "")} title={!check.canVerify ? "trust a release key first" : `get ${newest} for ${check.fleetArches.join(" and ")}`}>
              Get {newest}
            </button>
          {/if}
        </li>
        <li class="step" data-state={step3}>
          {@render stepMark(step3, "nodes")}
          <div class="step-title">On the nodes</div>
          <div class="step-detail">
            {#if step3 === "busy"}
              Updating in place…
            {:else if fleet.length === 0}
              No nodes yet
            {:else if outdated.length === 0}
              All {fleet.length} on <span class="mono text-ink-2">{target}</span>
            {:else}
              <span class="num">{current.length}</span> of <span class="num">{fleet.length}</span> on <span class="mono text-ink-2">{target}</span>
            {/if}
          </div>
          {#if step3 === "action" && isAdmin}
            <button class={ui.rowAction} disabled={app.engineStopped} onclick={() => openSsh(outdated)}>Update {outdated.length}</button>
          {/if}
        </li>
      </ol>
    </div>
  </div>

  <!-- Anything in flight or just finished, as its own card: a rollout in
       progress is the one thing on this panel that is changing, and it
       should not be read as part of the answer above it. -->
  {#if check?.verifyNote || checkErr || check?.channelError || fetchErr || upErr || fetchRows.length || fetchNote || upRows.length || handsNodes.length}
    <div class="panel">
      <div class="panel-head">
        <span class="icon-tile sm" style={(fetchBusy || upBusy) ? "--tone: var(--status-running); --tone-ink: var(--status-running-ink);" : (fetchErr || upErr) ? "--tone: var(--status-error); --tone-ink: var(--status-error-ink);" : ""}><Icon name="pulse" size={15} /></span>
        <div class="min-w-0 flex-1">
          <h3 class="panel-title">{fetchBusy || upBusy ? "In progress" : "Latest activity"}</h3>
        </div>
      </div>
      <div class="space-y-3">
        {#if checkErr || check?.channelError}
          <div class="note" data-tone="warn">The release channel did not answer: {checkErr ?? check?.channelError}</div>
        {/if}
        {#if check?.verifyNote}
          <div class="note" data-tone="warn">{check.verifyNote}</div>
        {/if}
        {#if fetchErr}<div class="note" data-tone="error">{fetchErr}</div>{/if}
        {#if upErr}<div class="note" data-tone="error">{upErr}</div>{/if}

        {#if fetchRows.length}
          <div>
            <div class="mb-2 flex items-center gap-2 text-xs text-ink-5">
              <span class="font-semibold text-ink-3">Getting {check?.channelLatest ?? newest} into the mesh</span>
              {#if fetchBusy}<span class="ml-1 inline-flex"><Spinner size={12} thickness={1.75} color="var(--accent)" track="color-mix(in srgb, var(--accent) 22%, transparent)" /></span>{/if}
            </div>
            <div class="list">
              {#each fetchRows as r (r.arch)}
                <div class="list-row">
                  <span class="mono w-20 shrink-0 font-semibold text-ink-2">{r.arch}</span>
                  <span class="w-24 shrink-0 text-xs font-semibold" style={r.stage === "published" || r.stage === "skipped" ? "color: var(--accent-ink);" : "color: var(--status-warn-ink);"}>{r.stage}</span>
                  <span class="min-w-0 flex-1 truncate text-xs text-ink-5">{r.detail}</span>
                </div>
              {/each}
            </div>
          </div>
        {/if}
        {#if fetchNote}<div class="note" data-tone="ok">{fetchNote}</div>{/if}

        {#if upRows.length}
          <div>
            <div class="mb-2 flex items-center gap-2 text-xs text-ink-5">
              <span class="font-semibold text-ink-3">Updating in place to {target}</span>
              <span>· {upBatch} at a time</span>
              {#if upBusy}<span class="ml-1 inline-flex"><Spinner size={12} thickness={1.75} color="var(--accent)" track="color-mix(in srgb, var(--accent) 22%, transparent)" /></span>{/if}
            </div>
            <div class="list">
              {#each upRows as r, i (i)}
                <div class="list-row">
                  <span class="w-36 shrink-0 truncate font-semibold text-ink-2">{r.node}</span>
                  <span
                    class="w-24 shrink-0 text-xs font-semibold"
                    style={r.state === "failed"
                      ? "color: var(--status-error-ink);"
                      : r.state === "back" || r.state === "already"
                        ? "color: var(--accent-ink);"
                        : "color: var(--status-warn-ink);"}
                  >
                    {r.state}
                  </span>
                  <span class="min-w-0 flex-1 text-xs text-ink-5">{r.detail}</span>
                </div>
              {/each}
            </div>
            {#if upSummary}<div class="mt-2 text-sm text-ink-4">{upSummary}</div>{/if}
          </div>
        {/if}

        {#if handsNodes.length > 0}
          <div class="note flex flex-wrap items-center gap-3" data-tone="warn">
            <span class="min-w-0 flex-1 leading-normal">
              {handsNodes.length === 1 ? `${handsNodes[0].name} cannot` : `${handsNodes.length} nodes cannot`} replace
              {handsNodes.length === 1 ? "its" : "their"} own binary. Director can push it over SSH instead.
            </span>
            <button class="btn btn-primary h-8 shrink-0 px-3 text-sm" onclick={() => openSsh(handsNodes)}>Update over SSH</button>
          </div>
        {/if}
      </div>
    </div>
  {/if}

  <!-- ═══════════ the fleet ═══════════ -->
  <div class="panel">
    <div class="panel-head flex-wrap">
      <span class="icon-tile"><Icon name="nodes" size={17} /></span>
      <div class="min-w-0 flex-1">
        <h3 class="panel-title">Fleet</h3>
        <p class="panel-sub">Group by build, rack, role or architecture. Open a group for its machines, or update it from its card.</p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <button class="btn btn-secondary btn-md" onclick={() => (deployOpen = true)}>
          <Icon name="plus" size={13} /> Add machines
        </button>
        {#if fleet.length > 0}
          <button class="btn btn-secondary btn-md" disabled={!isAdmin || app.engineStopped} onclick={() => openSsh(forceUpdate || outdated.length === 0 ? fleet : outdated)}>
            <Icon name="terminal" size={13} /> Update over SSH…
          </button>
        {/if}
      </div>
    </div>
    <FleetGrid
      nodes={fleet}
      {target}
      canAct={isAdmin && !app.engineStopped}
      onupdate={(ns) => openSsh(ns)}
    />
    {#if selfNode}
      <p class="mt-4 mb-0 flex items-center gap-1.5 text-xs text-ink-5">
        <Icon name="info" size={13} />
        <span>{selfNode.name}, this machine, is not counted — Director keeps it at the meshnode it carries.</span>
        <Hint text={`${selfNode.name} runs ${selfNode.version ?? "an unknown build"}. Director keeps it at the meshnode it carries${app.about?.bundledMeshnode ? ` (${app.about.bundledMeshnode})` : ""}, updating it itself each time Director starts, so no rollout or SSH session is ever pointed at it.`} />
      </p>
    {/if}
  </div>

  <!-- ═══════════ advanced ═══════════ -->
  <div>
    <div class="mb-2.5 flex items-baseline gap-2 px-0.5">
      <h3 class="m-0 text-md font-semibold tracking-[-.015em] text-ink">Advanced</h3>
      <span class="text-xs text-ink-5">Hand-picked rollouts, release keys, and publishing your own builds.</span>
    </div>
    <div class="adv-grid">
      {@render advTile("nodes", "Choose nodes by hand", manualSummary, pickedNodes.length > 0, () => (advOpen = "manual"))}
      {@render advTile("shield", "Release keys", keysSummary, false, () => (advOpen = "keys"))}
      {#if isAdmin}
        {@render advTile("folder", "Publish a build", publishSummary, publishable.length > 0, () => (advOpen = "publish"))}
      {/if}
    </div>
  </div>
</div>

<Sheet open={advOpen === "manual"} title="Choose nodes by hand" sub={manualSummary} icon="nodes" width={820} onclose={() => (advOpen = null)}>
  <div class="flex flex-col gap-3">
    <p class="m-0 text-sm leading-normal text-ink-4">Tick the nodes to update — shift-click fills a range, a version header ticks its whole group, a tag ticks the outdated nodes carrying it. The selector below is what travels in the signed plan; every node re-checks it names itself before touching its own binary.</p>
    <div class="flex flex-wrap items-center gap-2">
      <button class={ui.rowAction} disabled={outdated.length === 0} onclick={() => { picked = new Set(outdated.map((n) => n.nodeId)); anchor = null; manualBatch = autoBatch(outdated.length); }}>
        All behind{outdated.length ? ` (${outdated.length})` : ""}
      </button>
      {#each suggestedRings as r (r.tag)}
        <button class={`${ui.rowAction} mono`} onclick={() => pickRing(r.tag)} title={`Tick the ${r.n} outdated node${r.n === 1 ? "" : "s"} tagged ${r.tag}`}>
          {r.tag} <span class="num text-ink-5">{r.n}</span>
        </button>
      {/each}
      {#if picked.size > 0}
        <button class="linkish ml-auto text-xs" onclick={() => { picked = new Set(); anchor = null; }}>Clear {picked.size}</button>
      {/if}
    </div>
    <div class="list dark-scroll max-h-[360px] overflow-y-auto overflow-x-hidden">
      {#each versionGroups as g (g.version)}
        {@const all = g.nodes.every((n) => picked.has(n.nodeId))}
        {@const some = g.nodes.some((n) => picked.has(n.nodeId))}
        <label class="sticky top-0 z-1 flex cursor-pointer items-center gap-3 border-b px-3.5 py-2 text-sm" style="border-color: var(--edge); background: var(--surface-2);">
          <input type="checkbox" class="check" checked={all} indeterminate={some && !all} aria-label={all ? `Deselect every node on ${g.version}` : `Select every node on ${g.version}`} onchange={() => pickGroup(g, !all)} />
          <span class="mono w-24 shrink-0 font-semibold text-ink-2">{g.version}</span>
          <span class="num shrink-0 text-xs text-ink-5">{g.nodes.length}</span>
        </label>
        {#each g.nodes as n (n.nodeId)}
          {@const i = rows.findIndex((r) => r.nodeId === n.nodeId)}
          <label class="flex cursor-pointer items-center gap-3 border-b px-3.5 py-1.5 text-sm transition-colors last:border-0 hover:bg-surface-2" style={`border-color: var(--edge-row);${picked.has(n.nodeId) ? " background: var(--accent-wash);" : ""}`}>
            <input type="checkbox" class="check" checked={picked.has(n.nodeId)} aria-label={`Select ${n.name}`} onclick={(e) => { e.preventDefault(); togglePick(i, e.shiftKey); }} />
            <Dot kind={n.liveness} />
            <span class="w-40 shrink-0 truncate font-medium text-ink-2">{n.name}</span>
            <span class="mono w-32 shrink-0 truncate text-xs text-ink-5">{n.shortId}</span>
            <span class="min-w-0 flex-1 truncate text-xs text-ink-6">{n.tags.join(", ")}</span>
            <span class="mono shrink-0 text-xs" style={target !== null && n.version === target ? "color: var(--accent-ink);" : "color: var(--status-warn-ink);"}>{n.version ?? "—"}</span>
          </label>
        {/each}
      {/each}
    </div>
    <div class="grid gap-3 lg:grid-cols-[1fr_1fr_auto]">
      <label class="block">
        <span class={ui.label}>To version</span>
        <input bind:value={upVersion} placeholder={newest ?? "0.6.0"} class={ui.inputMono} />
      </label>
      <label class="block">
        <span class={ui.label}>At a time <span class="text-ink-6">(in place, max 64)</span></span>
        <input type="number" min="1" max="64" bind:value={manualBatch} class={ui.inputMono} />
      </label>
      <div class="flex items-end gap-2">
        <!-- Both ways out of a hand-picked set. SSH first because it is
             the one that works without anything installed on the far
             side; in place is the mesh driving itself, which is the
             better trick and the narrower one. -->
        <button class={ui.primary} disabled={!isAdmin || pickedNodes.length === 0 || app.engineStopped} onclick={() => openSsh(pickedNodes)}>
          <Icon name="terminal" size={13} />
          {pickedNodes.length ? `${forceUpdate ? "Reinstall on" : "Update"} ${pickedNodes.length}` : "Update over SSH"}
        </button>
        <button class={ui.secondary} disabled={!isAdmin || upBusy || pickedNodes.length === 0 || !target || app.engineStopped} onclick={() => rollout(pickedNodes, Math.max(1, Math.min(64, manualBatch)))} title="Each node fetches the release from the mesh and swaps its own binary — needs the fleet-upgrade drop-in on them.">
          In place
        </button>
      </div>
    </div>
    <label class="flex cursor-pointer items-center justify-between gap-4 rounded-lg px-3.5 py-2.5" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
      <span class="min-w-0">
        <span class="flex items-center gap-1 text-sm font-medium text-ink">
          Reinstall nodes already on {target ?? "the target"}
          <Hint text="A node reports its own version and nobody checks the claim. Turn this on when the version number is what you doubt — a binary replaced by hand, an install that half-finished, a rebuild that kept its number. Every node still verifies the release signature before it swaps anything. This one switch applies to the Update button above, the SSH sheet, and in-place rollouts alike." />
        </span>
        <span class="mt-0.5 block text-xs leading-normal text-ink-5">Push the build even where the reported version already matches.</span>
      </span>
      <input type="checkbox" class="switch" bind:checked={forceUpdate} />
    </label>
    {#if pickedNodes.length}
      <div class="selectable mono truncate text-2xs text-ink-5" title={selectorFor(pickedNodes)}>
        meshnode upgrade --on '{selectorFor(pickedNodes)}' --to {target ?? "?"}{manualBatch > 1 ? ` --batch ${manualBatch}` : ""}{forceUpdate ? " --force" : ""}
      </div>
    {/if}
  </div>
</Sheet>

<Sheet open={advOpen === "keys"} title="Release keys" sub={keysSummary} icon="shield" width={720} onclose={() => (advOpen = null)}>
  <div class="flex flex-col gap-3">
    <p class="m-0 text-sm leading-normal text-ink-4">A release is only installed if a key this mesh trusts signed it. There is no project-wide key: publishing a build signs it with your own release key — derived from your identity, nothing to store — and trusts that key here the first time. Trusting a key is an admin-signed record like any other, so it replicates: no editing a config file on every machine, and a node that joins next month arrives already knowing.</p>
    {#if trust?.nothingWouldVerify}
      <div class="note" data-tone="warn">
        <strong>No node in this mesh can install a release yet.</strong> Meshware has no project-wide key, and your identity is on a security key, which has no release key to sign with. Trust a minisign key you sign builds with below — it reaches every node, including ones that join later.
      </div>
    {/if}
    {#if trust?.keys.length}
      <div class="list">
        {#each trust.keys as k (k.pubkey)}
          <div class="list-row">
            <span class="grid h-6 w-6 shrink-0 place-items-center rounded-md" style={k.revoked ? "background: var(--surface-2); color: var(--ink-faint);" : "background: var(--accent-wash); color: var(--accent-ink);"}>
              <Icon name="secrets" size={12} />
            </span>
            <span class="min-w-0 flex-1">
              <span class="block truncate text-sm font-medium {k.revoked ? 'text-ink-5 line-through' : 'text-ink-2'}">{k.label || "unlabelled key"}</span>
              <span class="mono block truncate text-2xs text-ink-5">{k.pubkey}</span>
            </span>
            {#if k.revoked}
              <span class="shrink-0 text-xs text-ink-5">withdrawn</span>
            {:else if isAdmin}
              <button class="linkish shrink-0 text-xs" disabled={trustBusy} onclick={() => revokeKey(k.pubkey)}>Withdraw</button>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
    {#if isAdmin}
      <div class="flex flex-wrap items-center gap-2">
        <input class="field field-sunken mono h-9 min-w-[260px] flex-1 px-3 text-xs outline-none" placeholder="RW… or sk-ssh-ed25519@openssh.com … (a line of your .pub)" bind:value={keyDraft} spellcheck="false" />
        <input class="field field-sunken h-9 w-40 px-3 text-xs outline-none" placeholder="Label" bind:value={keyLabel} />
        <button class={ui.secondary} disabled={trustBusy || !keyDraft.trim()} onclick={() => trustKey(keyDraft)}>Trust key</button>
        <button class="linkish text-xs" disabled={trustBusy} onclick={pickKeyFile}>or pick a .pub file</button>
      </div>
    {/if}
    {#if firstSshKey}
      <div class="note" data-tone="warn">
        This is a security key's key. Once it is trusted, every release needs a co-signature made
        with it before any node will install it — including releases already published.
      </div>
    {/if}
    {#if trust?.cosignRequired && trust.releases.length}
      <!-- Y6: each published binary, and whether the security key has
           signed it. A missing one is not a gap in a list: it is a
           release nobody can install. -->
      <div class="mt-1 text-xs text-ink-5">
        Co-signatures — make one with
        <span class="mono">ssh-keygen -Y sign -f &lt;key&gt; -n meshware-release &lt;binary&gt;</span>
      </div>
      <div class="list">
        {#each trust.releases as r (r.digest)}
          <div class="list-row">
            <span class="mono w-16 shrink-0 font-semibold text-ink-2">{r.version}</span>
            <span class="mono w-20 shrink-0 text-xs text-ink-5">{r.arch || "—"}</span>
            {#if r.cosignedBy}
              <span class="min-w-0 flex-1 truncate text-xs" style="color: var(--accent-ink);">
                Co-signed by {r.cosignedBy}{r.userPresent ? ", with a touch" : ""}
              </span>
            {:else}
              <span class="min-w-0 flex-1 truncate text-xs" style="color: var(--status-warn-ink);">Not co-signed — no node will install it</span>
              {#if isAdmin}
                <button class={ui.rowAction} disabled={cosignBusy !== null} onclick={() => attachCosign(r.digest)}>
                  {cosignBusy === r.digest ? "Attaching…" : "Attach co-signature…"}
                </button>
              {/if}
            {/if}
          </div>
        {/each}
      </div>
    {/if}
    {#if cosignNote}<div class="note" data-tone="ok">{cosignNote}</div>{/if}
    {#if trustErr}<div class="note" data-tone="error">{trustErr}</div>{/if}
  </div>
</Sheet>

{#if isAdmin}
  <Sheet open={advOpen === "publish"} title="Publish a build" sub={publishSummary} icon="folder" width={720} onclose={() => (advOpen = null)}>
    <div class="flex flex-col gap-3">
      <p class="m-0 text-sm leading-normal text-ink-4">Point at a folder of meshnode builds — what <code>make dist</code> or packaging/build-release.sh wrote. Director remembers it, so next time the builds are already listed. Each build is one click to publish, signed with your own release key unless it already carries a signature; every reason a build cannot be published is worked out before you commit to one.</p>
      {#if buildDir}
        <div class="flex flex-wrap items-center gap-3 rounded-lg px-3.5 py-2.5" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
          <span class="grid h-7 w-7 shrink-0 place-items-center rounded-md" style="background: var(--accent-wash); color: var(--accent-ink);">
            <Icon name="folder" size={14} />
          </span>
          <span class="min-w-0 flex-1">
            <span class="block text-xs text-ink-5">Build folder <span class="text-ink-6">· remembered on this machine</span></span>
            <span class="mono block truncate text-sm text-ink-2" title={buildDir}>{buildDir}</span>
          </span>
          <span class="flex shrink-0 items-center gap-1">
            <button class={ui.rowAction} disabled={scanning} onclick={() => scanBuilds()}>
              <Icon name="refresh" size={12} /> {scanning ? "Scanning…" : "Rescan"}
            </button>
            <button class={ui.rowAction} onclick={pickBuildDir}>Change…</button>
            <button class="linkish px-2 text-xs" onclick={forgetBuildDir} title="Stop remembering this folder">Forget</button>
          </span>
        </div>
        {#if builds.length}
          <div class="list">
            {#each builds as b (b.path)}
              <div class="list-row">
                <span class="mono w-16 shrink-0 font-semibold text-ink-2">{b.version}</span>
                <span class="mono w-20 shrink-0 text-xs text-ink-5">{b.arch}</span>
                <span class="num w-16 shrink-0 text-xs text-ink-5">{(Number(b.size) / 1048576).toFixed(1)} MB</span>
                {#if b.blocked}
                  <span class="min-w-0 flex-1 truncate text-xs" style="color: var(--status-warn-ink);" title={b.blocked}>{b.blocked}</span>
                {:else}
                  <span class="min-w-0 flex-1 truncate text-xs" style="color: var(--accent-ink);">{b.signed ? "Signed and verified against a key this mesh trusts" : "Publishing signs it with your release key"}</span>
                  <button class="btn btn-primary h-8 shrink-0 rounded-sm px-3 text-xs" disabled={pubBusy} onclick={() => publish(b)}>
                    {pubBusy ? "Publishing…" : `Publish ${b.version}`}
                  </button>
                {/if}
              </div>
            {/each}
          </div>
        {/if}
      {:else}
        <div class="flex flex-wrap items-center gap-4 rounded-lg px-4 py-4" style="border: 1px dashed var(--edge-dashed);">
          <span class="grid h-9 w-9 shrink-0 place-items-center rounded-lg" style="background: var(--surface-2); color: var(--ink-4);">
            <Icon name="folder" size={18} />
          </span>
          <span class="min-w-0 flex-1">
            <span class="block text-sm font-medium text-ink">No build folder yet</span>
            <span class="mt-0.5 block text-xs leading-normal text-ink-5">Pick the dist/ folder your build script writes. Director remembers it, so this is a one-time step.</span>
          </span>
          <button class={ui.primary} onclick={pickBuildDir}><Icon name="folder" size={13} /> Pick build folder</button>
        </div>
      {/if}
      {#if pubNote}<div class="note" data-tone="ok">{pubNote}</div>{/if}
      {#if pubErr}<div class="note" data-tone="error">{pubErr}</div>{/if}
      </div>
  </Sheet>
{/if}

<DeploySheet open={deployOpen} onclose={() => (deployOpen = false)} />
<SshUpgradeSheet open={sshOpen} onclose={() => (sshOpen = false)} nodes={sshNodes} version={target} startForced={forceUpdate} />

<style>
  /* The answer: the ring and its legend, the sentence and its buttons, on
     a faint wash of the brand so the one card that says "is my fleet
     current" is the one the eye lands on. */
  .hero {
    padding: 24px 26px 0;
    background:
      radial-gradient(520px 220px at 0% 0%, color-mix(in srgb, var(--accent) 9%, transparent), transparent 70%),
      var(--surface-1);
  }

  /* The path: three steps joined by a line through their tiles. */
  .steps-wrap {
    container-type: inline-size;
    margin: 22px -26px 0;
    border-top: 1px solid var(--edge);
    background: var(--surface-subtle);
    border-radius: 0 0 var(--radius-xl) var(--radius-xl);
  }
  .steps {
    position: relative;
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    margin: 0;
    padding: 20px 26px 22px;
    list-style: none;
  }
  /* The line runs tile-centre to tile-centre: the first tile sits at the
     left of its column and the last at the left of the third, so it spans
     two columns' width. */
  .steps::before {
    content: "";
    position: absolute;
    top: calc(20px + 19px);
    left: calc(26px + 20px);
    width: calc((100% - 52px) * 2 / 3);
    height: 2px;
    border-radius: 2px;
    background: var(--edge-strong);
  }
  .step {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 4px;
    min-width: 0;
    padding-right: 18px;
  }
  .step :global(.btn) {
    margin-top: 6px;
  }
  @container (max-width: 700px) {
    .steps {
      grid-template-columns: 1fr;
      gap: 16px;
    }
    .steps::before {
      display: none;
    }
  }
  .step-title {
    margin-top: 8px;
    font-size: var(--text-sm);
    font-weight: 600;
    color: var(--ink-1);
    line-height: 1.3;
  }
  .step-detail {
    font-size: var(--text-xs);
    color: var(--ink-5);
    line-height: 1.4;
    overflow-wrap: anywhere;
  }
  /* The tile is opaque so the line passes behind it, not through it. */
  .step-tile {
    position: relative;
    z-index: 1;
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: 12px;
    --t: var(--hue-gray);
    --ti: var(--hue-gray-ink);
    background: color-mix(in srgb, var(--t) 16%, var(--surface-1));
    color: var(--ti);
    box-shadow:
      0 0 0 4px var(--surface-subtle),
      inset 0 0 0 1px color-mix(in srgb, var(--t) 24%, transparent);
  }
  .step-tile[data-state="ok"] {
    --t: var(--status-ok);
    --ti: var(--status-ok-ink);
  }
  .step-tile[data-state="action"] {
    --t: var(--status-warn);
    --ti: var(--status-warn-ink);
  }
  .step-tile[data-state="warn"] {
    --t: var(--status-error);
    --ti: var(--status-error-ink);
  }
  .step-tile[data-state="busy"] {
    --t: var(--status-running);
    --ti: var(--status-running-ink);
  }

  /* The three Advanced tasks: tiles that open a sheet. */
  .adv-grid {
    display: grid;
    gap: 12px;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
  }
  .adv-tile {
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 0;
    padding: 14px 16px;
    border-radius: var(--radius-xl);
    background: var(--surface-1);
    box-shadow: var(--sh-card);
  }

  /* A grouped list: rows on the canvas, hairlines between. */
  .list {
    overflow: hidden;
    border-radius: var(--radius-lg);
    background: var(--surface-graph);
    box-shadow: 0 0 0 1px var(--edge-card);
  }
  .list-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 14px;
    font-size: var(--text-sm);
    border-bottom: 1px solid var(--edge-row);
  }
  .list-row:last-child {
    border-bottom: 0;
  }

  /* One inline note, three tones. */
  .note {
    padding: 9px 12px;
    border-radius: var(--radius-sm);
    font-size: var(--text-sm);
    line-height: 1.5;
    user-select: text;
  }
  .note[data-tone="ok"] {
    background: var(--accent-wash);
    color: var(--accent-ink);
  }
  .note[data-tone="warn"] {
    background: var(--status-warn-wash);
    color: var(--status-warn-ink);
  }
  .note[data-tone="error"] {
    background: var(--status-error-wash);
    color: var(--status-error-ink);
  }
</style>
