<script lang="ts">
  // Definitions (M24 §3.1, §3.2).
  //
  // Two things that must never become one thing. **Definitions** —
  // workflows, services, collectors, policies, inventory — replicate
  // through the op-log automatically: publish once, on one node, and they
  // are everywhere within a gossip round or two. **Binaries** never do, and
  // **bundle content** is fetched lazily by the node that needs it.
  //
  // Nothing on this screen is a new capability. Everything here already
  // worked; there was simply no surface that said so, which is why people
  // who had already used the feature were still asking for it.
  import { api, errText, fmtBytes, type DefinitionView, type PrefetchEvent } from "../api";
  import { app, ago, ui } from "../state.svelte";
  import Icon from "./Icon.svelte";
  import ScopePreview from "./ScopePreview.svelte";
  import { staggerIn } from "../motion";
  import { poll } from "../poll";

  let rows = $state<DefinitionView[]>([]);
  let err = $state<string | null>(null);

  async function load() {
    try {
      rows = await api.definitions();
      err = null;
    } catch (e) {
      err = errText(e);
    }
  }
  $effect(() => poll(load, 20_000));

  // -- catch-up ---------------------------------------------------------------
  //
  // Not new machinery: a node's op-log position *is* its definition
  // version, so a node that is caught up has every definition by
  // construction. This is the sync reading the Nodes view already shows,
  // filtered — and phrased as *this node's* observation, because a gossip
  // mesh has no vantage point from which to claim more.
  const peers = $derived(app.nodes.filter((n) => !n.isSelf && !n.left && !n.revoked));
  const caughtUp = $derived(peers.filter((n) => n.sync !== null && n.sync.behind === 0));
  const behind = $derived(peers.filter((n) => n.sync !== null && n.sync.behind > 0));
  const silent = $derived(peers.filter((n) => n.sync === null));

  // -- push to nodes (§3.2) ----------------------------------------------------
  let pushBundle = $state("");
  let pushSelector = $state("any");
  let pushing = $state(false);
  let pushErr = $state<string | null>(null);
  let pushRows = $state<{ node: string; line: string; done: boolean; skipped: boolean }[]>([]);
  let pushSummary = $state<string | null>(null);

  const canContent = $derived(
    app.whoami?.isFounder || app.whoami?.roles.some((r) => r === "admin" || r === "developer"),
  );

  function note(ev: Extract<PrefetchEvent, { type: "node" }>): string {
    if (ev.unreachable) return `not pushed — ${ev.unreachable}`;
    if (ev.complete) return `has it (${fmtBytes(ev.totalBytes)})`;
    if (ev.totalChunks === 0) return "starting…";
    return `${ev.haveChunks}/${ev.totalChunks} chunks · ${fmtBytes(ev.haveBytes)}`;
  }

  async function push() {
    if (!pushBundle) return;
    pushing = true;
    pushErr = null;
    pushRows = [];
    pushSummary = null;
    try {
      await api.bundlePrefetch(pushBundle, pushSelector.trim() || "any", (ev) => {
        if (ev.type === "node") {
          const line = note(ev);
          const existing = pushRows.find((r) => r.node === ev.node);
          if (existing) {
            existing.line = line;
            existing.done = ev.complete;
            existing.skipped = ev.unreachable !== null;
          } else {
            pushRows = [
              ...pushRows,
              { node: ev.node, line, done: ev.complete, skipped: ev.unreachable !== null },
            ];
          }
        } else if (ev.type === "done") {
          pushSummary = `${ev.complete} have it, ${ev.fetching} still fetching, ${ev.skipped} not pushed`;
          pushing = false;
        } else {
          pushErr = ev.message;
          pushing = false;
        }
      });
    } catch (e) {
      pushErr = errText(e);
      pushing = false;
    }
  }
</script>

<div class="space-y-4">
  <div class="card px-[22px] py-4">
    <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">What this mesh distributes</h2>
    <p class="mt-1.5 mb-0 max-w-3xl text-sm leading-relaxed text-ink-4">
      Everything below reaches every machine <strong>automatically</strong>. Publish once,
      on one machine, and it is on every machine within a few seconds — no copying, no agent,
      no push. Changing these changes what the mesh <em>does</em>.
    </p>
    <p class="mt-2 mb-0 max-w-3xl text-sm leading-relaxed text-ink-5">
      The binary is the other thing, and it deliberately does not travel this way. Changing it
      changes what the mesh <em>is</em>, so it has its own path with a rollback — a bad definition
      is recoverable; a bad binary is a site visit.
    </p>
  </div>

  {#if err}
    <div class={ui.errorBox} style="background: var(--status-error-wash); color: var(--status-error-ink);">{err}</div>
  {/if}

  <div class="card overflow-hidden">
    <table class="w-full text-left text-base">
      <thead>
        <tr class="border-b border-edge">
          <th class="label px-5 py-3">Definition</th>
          <th class="label px-3 py-3 text-right">Count</th>
          <th class="label px-3 py-3">Last change</th>
          <th class="label px-3 py-3 pr-5">Replicated by</th>
        </tr>
      </thead>
      <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
        {#each rows as r (r.kind)}
          <tr class="border-b border-edge transition-colors last:border-0 hover:bg-surface-2">
            <td class="px-5 py-3">
              <div class="flex items-center gap-2">
                <span class="font-semibold text-ink">{r.label}</span>
                {#if r.executes}
                  <!--
                    The rows where widening a selector is a deployment: they
                    run on nodes without anybody pressing Run (§3.3).
                  -->
                  <span
                    class="rounded-full px-2 py-0.5 text-2xs font-semibold"
                    style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
                    title="runs on nodes unprompted — widening its selector is a deployment"
                  >
                    runs on nodes
                  </span>
                {/if}
              </div>
              <div class="mt-0.5 text-xs leading-relaxed text-ink-5">{r.what}</div>
            </td>
            <td class="num px-3 py-3 text-right text-ink-2">{r.count}</td>
            <td class="px-3 py-3 text-sm text-ink-4">
              {#if r.lastChangedWhat}
                <span class="text-ink-3">{r.lastChangedWhat}</span>
                <span class="num ml-1.5 text-xs text-ink-5">{ago(r.lastChangedMicros)}</span>
              {:else}
                <span class="text-ink-6">never</span>
              {/if}
            </td>
            <td class="mono px-3 py-3 pr-5 text-xs text-ink-5">{r.op}</td>
          </tr>
        {:else}
          <tr><td colspan="4" class="px-5 py-8 text-center text-sm text-ink-5">Reading the mesh…</td></tr>
        {/each}
      </tbody>
    </table>
  </div>

  <!-- ---------- catch-up ---------- -->
  <div class="card px-[22px] py-4">
    <div class="flex items-baseline gap-2">
      <h3 class="m-0 text-md font-bold tracking-[-.02em] text-ink">Who has caught up</h3>
      <span class="text-xs text-ink-5">
        as each machine last told this one — no single machine can know more than that
      </span>
    </div>
    {#if peers.length === 0}
      <p class="mt-2 mb-0 text-sm text-ink-4">
        This is the only node, so everything above is already everywhere it goes.
      </p>
    {:else}
      <p class="mt-2 mb-0 text-sm text-ink-4">
        <strong class="text-ink">{caughtUp.length} of {peers.length}</strong> other nodes had
        every definition at their last exchange.
        {#if silent.length}
          {silent.length} have not asked since this daemon started.
        {/if}
      </p>
      {#if behind.length}
        <div class="mt-2 flex flex-col gap-1">
          {#each behind as n (n.nodeId)}
            <div class="flex items-center gap-2 text-xs">
              <span class="font-semibold text-ink-2">{n.name}</span>
              <span class="num text-ink-5">{n.sync?.behind} changes behind</span>
              <span class="text-ink-6">·</span>
              <span class="num text-ink-5">{ago(Date.now() * 1000 - (n.sync?.ageMicros ?? 0))}</span>
            </div>
          {/each}
        </div>
      {/if}
    {/if}
  </div>

  <!-- ---------- push a bundle (§3.2) ---------- -->
  <div class="card px-[22px] py-4">
    <div class="flex items-baseline gap-2">
      <h3 class="m-0 text-md font-bold tracking-[-.02em] text-ink">Push a bundle to nodes</h3>
      <span class="text-xs text-ink-5">the one thing that does <em>not</em> replicate on its own</span>
    </div>
    <p class="mt-1.5 mb-0 max-w-3xl text-sm leading-relaxed text-ink-4">
      Bundle content is fetched lazily, by the node that needs to run something — the right
      default for a 200 MB environment most of the fleet will never execute, and the reason a
      first run is slow. This asks a set of nodes to fetch it <em>now</em>. It changes nothing
      about what is authoritative: the bundle is already addressed by digest, and a node that
      misses this still fetches the moment it needs the content.
    </p>

    <div class="mt-4 grid gap-3 lg:grid-cols-3">
      <label class="block">
        <span class={ui.label}>Which bundle</span>
        <select bind:value={pushBundle} class={ui.inputMono}>
          <option value="">choose…</option>
          {#each app.bundles as b (b.bundleId)}
            <option value={b.bundleId}>{b.shortId} · {fmtBytes(b.totalSize)} · {b.holders} holder{b.holders === 1 ? "" : "s"}</option>
          {/each}
        </select>
      </label>
      <div>
        <label class="block">
          <span class={ui.label}>To which nodes</span>
          <input bind:value={pushSelector} placeholder="any" class={ui.inputMono} />
        </label>
        <ScopePreview selector={pushSelector} />
      </div>
      <div class="flex items-end">
        <button class={ui.primary} disabled={!canContent || pushing || !pushBundle} onclick={push}>
          {pushing ? "Pushing…" : "Push to nodes"}
        </button>
      </div>
    </div>
    {#if !canContent}
      <div class="mt-2 text-xs text-ink-5">pushing content needs the developer or admin role</div>
    {/if}

    {#if pushErr}
      <div class={ui.errorBox} style="background: var(--status-error-wash); color: var(--status-error-ink);">{pushErr}</div>
    {/if}

    {#if pushRows.length}
      <div class="mt-3 flex flex-col gap-1">
        {#each pushRows as r (r.node)}
          <div class="flex items-center gap-2 text-xs">
            <span class="inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full"
              style={r.done
                ? "background: var(--accent-wash); color: var(--accent-ink);"
                : r.skipped
                  ? "background: var(--status-warn-wash); color: var(--status-warn-ink);"
                  : "background: var(--surface-2); color: var(--ink-6);"}>
              {#if r.done}<Icon name="check" size={9} />{:else}·{/if}
            </span>
            <span class="w-40 truncate font-semibold text-ink-2">{r.node}</span>
            <span class="text-ink-5">{r.line}</span>
          </div>
        {/each}
      </div>
    {/if}
    {#if pushSummary}
      <div class="mt-2 text-sm text-ink-4">{pushSummary}</div>
    {/if}
  </div>
</div>
