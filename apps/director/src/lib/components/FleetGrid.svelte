<script module lang="ts">
  // The fleet, at a size where a row per machine is not an answer.
  //
  // The old panel drew one row per node under a version header. At five
  // nodes that is a table; at sixty it is a three-thousand-pixel wall you
  // scroll past to reach the controls underneath, and the question it was
  // built to answer — *what do I update, and in what order* — is not
  // answerable from it at all, because the shape an operator stages a
  // rollout by (a rack, a region, an architecture) is not the shape the
  // rows are in.
  //
  // So: **groups first, machines on request.** Pick the dimension, read
  // the counts, act on a group. The node rows still exist — they are one
  // click down, capped, scrolled and searchable, which is where a list
  // that can be a hundred long belongs.
  //
  // **Cards, and the rows in a sheet (2026-09-22).** The groups were a
  // stack of collapsible strips, each opening in place and pushing the
  // Advanced controls further down. A group is a card now — its name, a
  // ring of how much of it is current, a few of its machines by name — and
  // the full list opens in a sheet over the page, so reading one group
  // never rearranges the others.
  import type { NodeView } from "../api";

  export type GroupKey = "version" | "rack" | "role" | "arch" | "status";

  export type Group = {
    key: string;
    label: string;
    nodes: NodeView[];
    /** Nodes in this group that are not on the target build. */
    behind: NodeView[];
  };

  /** The tag families a fleet is actually staged by. `rack-a`, `dc=east`,
   *  `role=worker` — read off the tags rather than configured, because the
   *  fleet already named them and asking again would be asking twice. */
  function tagOf(n: NodeView, family: "rack" | "role"): string {
    if (family === "role") {
      return n.tags.find((t) => t.startsWith("role="))?.slice(5) ?? "untagged";
    }
    return (
      n.tags.find((t) => /^rack/.test(t)) ??
      n.tags.find((t) => t.startsWith("dc="))?.slice(3) ??
      "unplaced"
    );
  }

  export function groupsOf(nodes: NodeView[], by: GroupKey, target: string | null): Group[] {
    const behindOf = (n: NodeView) => (target ? n.version !== target : false);
    const key = (n: NodeView): string => {
      switch (by) {
        case "version": return n.version ?? "unknown";
        case "arch": return n.capacity?.arch ?? "unknown";
        case "status": return n.liveness;
        default: return tagOf(n, by);
      }
    };
    const by_ = new Map<string, NodeView[]>();
    for (const n of nodes) by_.set(key(n), [...(by_.get(key(n)) ?? []), n]);
    const out = [...by_.entries()].map(([k, group]) => ({
      key: k,
      label: k,
      nodes: [...group].sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true })),
      behind: group.filter(behindOf),
    }));
    // Whatever is most behind goes first: the list is sorted by how much
    // of it is work.
    return out.sort((a, b) => b.behind.length - a.behind.length || b.nodes.length - a.nodes.length);
  }
</script>

<script lang="ts">
  import Dot from "./Dot.svelte";
  import Icon from "./Icon.svelte";
  import Ring from "./Ring.svelte";
  import Sheet from "./Sheet.svelte";

  let {
    nodes,
    target = null,
    canAct = true,
    onupdate,
  }: {
    nodes: NodeView[];
    /** The build everything should be on; groups are measured against it. */
    target?: string | null;
    canAct?: boolean;
    onupdate: (nodes: NodeView[]) => void;
  } = $props();

  let by = $state<GroupKey>("version");
  let query = $state("");
  /** The group whose machines are open in the sheet, by key. */
  let openKey = $state<string | null>(null);

  const DIMENSIONS: { key: GroupKey; label: string }[] = [
    { key: "version", label: "Build" },
    { key: "rack", label: "Rack" },
    { key: "role", label: "Role" },
    { key: "arch", label: "Arch" },
    { key: "status", label: "Status" },
  ];
  const NOUN: Record<GroupKey, string> = {
    version: "build",
    rack: "rack",
    role: "role",
    arch: "architecture",
    status: "status",
  };

  const filtered = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (!q) return nodes;
    return nodes.filter(
      (n) =>
        n.name.toLowerCase().includes(q) ||
        (n.version ?? "").includes(q) ||
        n.tags.some((t) => t.toLowerCase().includes(q)) ||
        (n.capacity?.arch ?? "").includes(q),
    );
  });
  const groups = $derived(groupsOf(filtered, by, target));
  const totalBehind = $derived(filtered.filter((n) => target && n.version !== target).length);
  const opened = $derived(groups.find((g) => g.key === openKey) ?? null);

  // A new grouping is a different set of cards; a sheet open on one of the
  // old ones has no subject any more.
  $effect(() => {
    void by;
    openKey = null;
  });

  const CHIPS = 5;
</script>

<div>
  <div class="mb-4 flex flex-wrap items-center gap-2">
    <div class="seg shrink-0" role="tablist" aria-label="Group the fleet by">
      {#each DIMENSIONS as d (d.key)}
        <button role="tab" aria-selected={by === d.key} onclick={() => (by = d.key)}>{d.label}</button>
      {/each}
    </div>
    <label class="grid-search">
      <Icon name="search" size={13} />
      <input
        bind:value={query}
        placeholder="Find a node, a tag, a build…"
        spellcheck="false"
        aria-label="Filter the fleet"
        onkeydown={(e) => e.key === "Escape" && (query = "")}
      />
    </label>
    {#if query.trim()}
      <span class="num shrink-0 text-xs text-ink-5">{filtered.length} of {nodes.length}</span>
    {/if}
    {#if totalBehind > 0 && canAct}
      <button class="btn btn-ghost btn-md ml-auto" onclick={() => onupdate(filtered.filter((n) => target && n.version !== target))}>
        Update all {totalBehind} behind{query.trim() ? " that match" : ""}
      </button>
    {/if}
  </div>

  {#if groups.length === 0}
    <div class="rounded-xl px-4 py-6 text-center text-sm text-ink-5" style="border: 1px dashed var(--edge-dashed);">
      {query.trim() ? `Nothing matches ${query}.` : "No nodes besides this Director. Add machines to see their builds here."}
    </div>
  {:else}
    <div class="group-grid">
      {#each groups as g (g.key)}
        {@const current = g.nodes.length - g.behind.length}
        {@const allCurrent = target !== null && g.behind.length === 0}
        {@const tone = allCurrent ? "var(--status-ok)" : g.behind.length ? "var(--status-warn)" : "var(--hue-gray)"}
        <!-- The card is the target; the Update button inside it stops the
             click so acting on a group never also opens it. -->
        <div
          class="group-card lift"
          role="button"
          tabindex="0"
          aria-label={`${NOUN[by]} ${g.label}: ${g.nodes.length} node${g.nodes.length === 1 ? "" : "s"}, ${g.behind.length} behind`}
          onclick={() => (openKey = g.key)}
          onkeydown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              openKey = g.key;
            }
          }}
        >
          <div class="flex items-start gap-3">
            <Ring
              size={46}
              thickness={6}
              total={g.nodes.length}
              arcs={[
                { value: current, color: "var(--status-ok)" },
                { value: g.behind.length, color: "var(--status-warn)" },
              ]}
            >
              <span class="num text-[13px] font-bold text-ink">{g.nodes.length}</span>
            </Ring>
            <div class="min-w-0 flex-1">
              <div class="microlabel">{NOUN[by]}</div>
              <div class="mono mt-0.5 truncate text-[15px] font-semibold text-ink" title={g.label}>{g.label}</div>
              <div class="mt-0.5 text-xs" style={`color: ${allCurrent ? "var(--status-ok-ink)" : g.behind.length ? "var(--status-warn-ink)" : "var(--ink-5)"};`}>
                {#if allCurrent}
                  all current
                {:else if g.behind.length === g.nodes.length}
                  {g.behind.length === 1 ? "behind" : `all ${g.behind.length} behind`}
                {:else if g.behind.length}
                  {g.behind.length} of {g.nodes.length} behind
                {:else}
                  no target build yet
                {/if}
              </div>
            </div>
            <span class="h-2 w-2 shrink-0 rounded-full" style={`background: ${tone}; margin-top: 4px;`} aria-hidden="true"></span>
          </div>

          <div class="mt-3 flex flex-wrap gap-1.5">
            {#each g.nodes.slice(0, CHIPS) as n (n.nodeId)}
              {@const behind = target !== null && n.version !== target}
              <span class="node-chip" title={`${n.name} · ${n.version ?? "unknown build"}`}>
                <span class="h-1.5 w-1.5 shrink-0 rounded-full" style={`background: ${behind ? "var(--status-warn)" : "var(--status-ok)"};`} aria-hidden="true"></span>
                {n.name}
              </span>
            {/each}
            {#if g.nodes.length > CHIPS}
              <span class="node-chip num text-ink-5">+{g.nodes.length - CHIPS}</span>
            {/if}
          </div>

          {#if g.behind.length > 0 && canAct}
            <div class="mt-3 flex items-center pt-3" style="border-top: 1px solid var(--edge);">
              <span class="text-xs text-ink-5">Open for the list</span>
              <button
                class="btn btn-secondary ml-auto h-7 rounded-md px-2.5 text-xs"
                onclick={(e) => {
                  e.stopPropagation();
                  onupdate(g.behind);
                }}
              >
                <Icon name="download" size={12} /> Update {g.behind.length}
              </button>
            </div>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<!-- One group's machines. Capped and scrolled by the sheet: a group can be
     a hundred machines, and the page it opened from stays where it was. -->
<Sheet
  open={opened !== null}
  title={opened ? `${NOUN[by][0].toUpperCase()}${NOUN[by].slice(1)} ${opened.label}` : ""}
  sub={opened ? `${opened.nodes.length} node${opened.nodes.length === 1 ? "" : "s"}${target ? ` · ${opened.behind.length ? `${opened.behind.length} not on ${target}` : `all on ${target}`}` : ""}` : ""}
  icon="nodes"
  width={620}
  onclose={() => (openKey = null)}
>
  {#if opened}
    <div class="overflow-hidden rounded-xl" style="box-shadow: 0 0 0 1px var(--edge-card);">
      {#each opened.nodes as n (n.nodeId)}
        {@const behind = target !== null && n.version !== target}
        <div class="flex items-center gap-3 px-4 py-2.5 text-sm" style="border-bottom: 1px solid var(--edge-row);">
          <Dot kind={n.liveness} />
          <span class="min-w-0 flex-1">
            <span class="block truncate font-medium text-ink-2">{n.name}</span>
            <span class="block truncate text-xs text-ink-5">
              {[n.capacity?.arch, ...n.tags].filter(Boolean).join(" · ") || "no tags"}
            </span>
          </span>
          <span
            class="mono shrink-0 rounded-md px-2 py-0.5 text-xs font-semibold"
            style={behind
              ? "background: var(--status-warn-wash); color: var(--status-warn-ink);"
              : "background: var(--status-ok-wash); color: var(--status-ok-ink);"}
          >{n.version ?? "unknown"}</span>
          {#if behind && canAct}
            <button class="btn btn-secondary h-7 shrink-0 rounded-md px-2.5 text-xs" onclick={() => onupdate([n])}>Update</button>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
  {#snippet footer()}
    {#if opened && opened.behind.length > 0 && canAct}
      <button class="btn btn-primary btn-md" onclick={() => opened && onupdate(opened.behind)}>
        <Icon name="download" size={13} /> Update {opened.behind.length} behind
      </button>
    {/if}
    <button class="btn btn-secondary btn-md ml-auto" onclick={() => (openKey = null)}>Done</button>
  {/snippet}
</Sheet>

<style>
  .group-grid {
    display: grid;
    gap: 12px;
    grid-template-columns: repeat(auto-fill, minmax(236px, 1fr));
  }
  .group-card {
    padding: 14px 16px;
    border-radius: var(--radius-lg);
    background: var(--surface-1);
    box-shadow: 0 0 0 1px var(--edge-card), 0 1px 2px rgba(15, 23, 42, 0.03);
    text-align: left;
  }
  .node-chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    max-width: 140px;
    height: 22px;
    padding: 0 8px;
    overflow: hidden;
    border-radius: 999px;
    background: var(--surface-2);
    font-size: var(--text-2xs);
    font-weight: 500;
    color: var(--ink-3);
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .grid-search {
    display: flex;
    align-items: center;
    gap: 7px;
    flex: 1 1 200px;
    max-width: 320px;
    height: 32px;
    padding: 0 10px;
    border-radius: var(--radius-md);
    background: var(--surface-2);
    color: var(--ink-faint);
  }
  .grid-search:focus-within {
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 35%, transparent);
  }
  .grid-search input {
    min-width: 0;
    flex: 1;
    background: transparent;
    color: var(--ink-1);
    font-size: var(--text-xs);
    outline: none;
  }
  .grid-search input::placeholder {
    color: var(--ink-muted);
  }
</style>
