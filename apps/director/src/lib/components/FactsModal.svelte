<script lang="ts">
  // M18 §4.10 P6 — the Facts panel, as a modal off any node row.
  //
  // The question this answers is "what *is* that machine", and the answer
  // has to carry its own age: a fact is only as good as the last time
  // anybody looked, and a panel that shows `kernel_release 6.1.0-18-amd64`
  // without saying when is a panel that will one day be confidently wrong.
  // So the observation timestamp is in the header, in the collector strip,
  // and again as an absolute instant on hover — not tucked in a footer.
  //
  // Three things, in the order somebody needs them:
  //   1. the keys, filterable, because a `fact.*` selector addresses them
  //      by name and copying one is the most likely next action;
  //   2. when each collector last looked;
  //   3. what changed, and when — the diff, not two documents to compare
  //      by eye.
  import { dialog } from "../dialog";
  import { api, errText, type NodeFactsView, type FactDocView } from "../api";
  import { ago, ui } from "../state.svelte";
  import Icon from "./Icon.svelte";
  import { portal } from "../portal";

  let {
    /** Node name, node id prefix, or host name. Empty = this node. */
    subject = "",
    open = false,
    onclose,
  }: {
    subject?: string;
    open?: boolean;
    onclose?: () => void;
  } = $props();

  let facts = $state<NodeFactsView | null>(null);
  let error = $state<string | null>(null);
  let loading = $state(false);
  let filter = $state("");
  let raw = $state(false);
  let copied = $state("");
  let tab = $state<"facts" | "history">("facts");

  // Re-fetch whenever the modal opens on a (possibly different) subject.
  // Keyed on both, so re-opening the same node refreshes rather than
  // showing a document from ten minutes ago.
  $effect(() => {
    if (!open) return;
    const want = subject;
    loading = true;
    error = null;
    facts = null;
    filter = "";
    raw = false;
    tab = "facts";
    api
      .nodeFacts(want)
      .then((f) => {
        if (subject === want) facts = f;
      })
      .catch((e) => {
        if (subject === want) error = errText(e);
      })
      .finally(() => {
        if (subject === want) loading = false;
      });
  });

  const docs = $derived(facts?.docs ?? []);

  /** The freshest observation across every collector — the header's age. */
  const newest = $derived(
    docs.length ? Math.max(...docs.map((d) => d.collectedAtMicros)) : 0,
  );

  /** The *stalest* one, which is the number that should worry somebody: a
      panel is only as fresh as its oldest collector. */
  const oldest = $derived(
    docs.length ? Math.min(...docs.map((d) => d.collectedAtMicros)) : 0,
  );

  const totalKeys = $derived(docs.reduce((n, d) => n + d.entries.length, 0));

  function matches(doc: FactDocView): [string, string][] {
    const q = filter.trim().toLowerCase();
    if (!q) return doc.entries;
    return doc.entries.filter(
      ([k, v]) => k.toLowerCase().includes(q) || v.toLowerCase().includes(q),
    );
  }

  const shown = $derived(docs.reduce((n, d) => n + matches(d).length, 0));

  /** An absolute UTC instant for the `title` tooltip: "40m ago" is what a
      reader wants at a glance, and the exact instant is what they want when
      they are comparing two machines. */
  function stamp(micros: number): string {
    if (!micros) return "never";
    return new Date(micros / 1000).toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
  }

  async function copy(text: string, what: string) {
    try {
      await api.copyText(text);
      copied = what;
      setTimeout(() => {
        if (copied === what) copied = "";
      }, 1400);
    } catch {
      // A denied clipboard costs the convenience, not the panel.
    }
  }

  /** The selector that would target every node with this value — the most
      likely reason anybody is reading a key at all. */
  function selectorFor(key: string, value: string): string {
    return `fact.${key}=${value}`;
  }
</script>

{#if open}
  <div
    use:portal
    class="fixed inset-0 z-50 grid place-items-center p-6"
    style="background: rgba(12,16,24,.62); backdrop-filter: blur(3px);"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) onclose?.();
    }}
  >
    <div
      class="fade-in flex max-h-[88vh] w-full max-w-[920px] flex-col overflow-hidden rounded-2xl bg-surface"
      style="box-shadow: var(--sh-modal);"
      role="dialog"
      aria-modal="true"
      aria-label={`Facts about ${facts?.subjectName ?? subject}`}
      use:dialog={{ onescape: onclose ?? null }}
    >
      <!-- header: what, and how old -->
      <div class="flex items-start gap-4 border-b border-edge px-6 py-[18px]">
        <div class="min-w-0 flex-1">
          <div class="label">Facts · observed, not typed</div>
          <div class="mt-1 flex items-baseline gap-2.5">
            <h3 class="m-0 truncate text-lg font-bold tracking-[-.02em] text-ink">
              {facts?.subjectName || subject || "this node"}
            </h3>
            {#if docs.length}
              <span class="num text-sm text-ink-5">
                {totalKeys} facts from {docs.length}
                {docs.length === 1 ? "collector" : "collectors"}
              </span>
            {/if}
          </div>
        </div>
        {#if newest > 0}
          <!-- The timestamp, given the weight it deserves. A fact without
               an age is a claim. -->
          <div class="shrink-0 text-right" title={`Newest observation: ${stamp(newest)}`}>
            <div class="label">Last gathered</div>
            <div class="num mt-1 text-base font-semibold text-ink-2">{ago(newest)}</div>
            {#if oldest && oldest !== newest}
              <div class="num text-2xs text-ink-5" title={`Oldest observation: ${stamp(oldest)}`}>
                oldest {ago(oldest)}
              </div>
            {/if}
          </div>
        {/if}
        <button
          class="btn btn-secondary h-8 w-8 shrink-0 rounded-sm p-0"
          aria-label="Close"
          onclick={() => onclose?.()}
        >
          <Icon name="close" size={14} />
        </button>
      </div>

      <!-- controls -->
      {#if docs.length}
        <div class="flex flex-wrap items-center gap-2 border-b border-edge px-6 py-2.5">
          <div class="flex overflow-hidden rounded-sm border border-edge">
            <button
              class={`h-8 px-3 text-xs font-semibold transition-colors ${tab === "facts" ? "bg-surface-2 text-ink" : "text-ink-5 hover:text-ink-3"}`}
              onclick={() => (tab = "facts")}
            >
              Facts
            </button>
            <button
              class={`h-8 border-l border-edge px-3 text-xs font-semibold transition-colors ${tab === "history" ? "bg-surface-2 text-ink" : "text-ink-5 hover:text-ink-3"}`}
              onclick={() => (tab = "history")}
            >
              What changed
              {#if facts?.history?.length}<span class="num ml-1 text-ink-5">{facts.history.length}</span>{/if}
            </button>
          </div>

          {#if tab === "facts"}
            <label class="relative ml-auto flex min-w-[200px] flex-1 items-center sm:max-w-[280px]">
              <span class="pointer-events-none absolute left-2.5 text-ink-faint"><Icon name="search" size={13} /></span>
              <input
                bind:value={filter}
                placeholder="Filter keys and values…"
                class={`${ui.inputMono} h-8 pl-8 text-xs`}
              />
            </label>
            <button
              class={ui.rowAction}
              aria-pressed={raw}
              onclick={() => (raw = !raw)}
              title="Show the document exactly as the collector printed it"
            >
              {raw ? "Table" : "JSON"}
            </button>
          {/if}
        </div>
      {/if}

      <!-- body -->
      <div class="min-h-0 flex-1 overflow-y-auto px-6 py-4">
        {#if loading}
          <div class="py-10 text-center text-sm text-ink-5">Reading facts…</div>
        {:else if error}
          <div class="selectable rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">
            {error}
          </div>
        {:else if !docs.length}
          <div class="py-10 text-center">
            <div class="text-sm text-ink-4">No facts yet.</div>
            <div class="mt-1.5 text-xs text-ink-5">
              The built-in <span class="mono">system</span> collector publishes within
              15 minutes of a node starting, and re-publishes only when the machine changes.
            </div>
          </div>
        {:else if tab === "history"}
          {#if !facts?.historyAvailable}
            <div class="py-10 text-center text-sm text-ink-5">
              This node's history index is off (<span class="mono">[history] enabled = false</span>),
              so it cannot say when these facts changed — only what they are now.
            </div>
          {:else if !facts.history.length}
            <div class="py-10 text-center text-sm text-ink-5">Nothing has changed since this node started watching.</div>
          {:else}
            <ol class="m-0 list-none space-y-4 p-0">
              {#each facts.history as step (step.collector + step.atMicros)}
                <li>
                  <div class="flex items-baseline gap-2">
                    <span class="num text-sm font-semibold text-ink-2" title={stamp(step.atMicros)}>
                      {ago(step.atMicros)}
                    </span>
                    <span class="mono text-2xs text-ink-5">{step.collector}</span>
                    <span class="num ml-auto text-2xs text-ink-faint">
                      {step.changes.length} {step.changes.length === 1 ? "key" : "keys"}
                    </span>
                  </div>
                  <div class="mt-1.5 overflow-hidden rounded-md border border-edge">
                    {#each step.changes.slice(0, 24) as c (c.key)}
                      <div class="flex items-baseline gap-2 border-b border-edge px-3 py-1.5 text-xs last:border-0">
                        <span class="mono w-44 shrink-0 truncate text-ink-3" title={c.key}>{c.key}</span>
                        {#if c.verb === "changed"}
                          <span class="mono min-w-0 flex-1 truncate text-ink-5 line-through" title={c.before ?? ""}>{c.before}</span>
                          <span class="shrink-0 text-ink-faint">→</span>
                          <span class="mono min-w-0 flex-1 truncate" style="color: var(--accent-ink);" title={c.after ?? ""}>{c.after}</span>
                        {:else if c.verb === "added"}
                          <span class="shrink-0 text-2xs font-semibold" style="color: var(--accent-ink);">added</span>
                          <span class="mono min-w-0 flex-1 truncate text-ink-3" title={c.after ?? ""}>{c.after}</span>
                        {:else}
                          <span class="shrink-0 text-2xs font-semibold text-error">removed</span>
                          <span class="mono min-w-0 flex-1 truncate text-ink-5 line-through" title={c.before ?? ""}>{c.before}</span>
                        {/if}
                      </div>
                    {/each}
                    {#if step.changes.length > 24}
                      <div class="px-3 py-1.5 text-2xs text-ink-5">
                        and {step.changes.length - 24} more
                      </div>
                    {/if}
                  </div>
                </li>
              {/each}
            </ol>
          {/if}
        {:else}
          <div class="space-y-5">
            {#each docs as doc (doc.collector)}
              {@const rows = matches(doc)}
              <section>
                <div class="flex flex-wrap items-baseline gap-2 pb-1.5">
                  <span class="mono text-sm font-semibold text-ink-2">{doc.collector}</span>
                  <span class="num text-2xs text-ink-5" title={`Observed ${stamp(doc.collectedAtMicros)}`}>
                    observed {ago(doc.collectedAtMicros)}
                  </span>
                  {#if doc.authorName && doc.authorName !== facts?.subjectName}
                    <span class="text-2xs text-ink-5">by {doc.authorName}</span>
                  {/if}
                  <button
                    class="ml-auto flex items-center gap-1.5 text-2xs text-ink-5 transition-colors hover:text-ink-3"
                    onclick={() => copy(doc.json, doc.collector)}
                  >
                    <Icon name="copy" size={12} />
                    {copied === doc.collector ? "copied" : "copy JSON"}
                  </button>
                </div>

                {#if raw}
                  <pre class="selectable mono overflow-x-auto rounded-md border border-edge px-3 py-2.5 text-xs text-ink-3">{doc.json}</pre>
                {:else if rows.length === 0}
                  <div class="rounded-md border border-edge px-3 py-3 text-xs text-ink-5">
                    Nothing here matches “{filter}”.
                  </div>
                {:else}
                  <div class="overflow-hidden rounded-md border border-edge">
                    {#each rows as [key, value] (key)}
                      <div class="group flex items-baseline gap-3 border-b border-edge px-3 py-[7px] text-sm last:border-0 hover:bg-surface-2">
                        <span class="mono w-52 shrink-0 truncate text-xs text-ink-4" title={key}>{key}</span>
                        <span class="selectable mono min-w-0 flex-1 break-all text-ink-2">{value}</span>
                        <!-- The likely next action: target every node that
                             says the same thing. -->
                        <button
                          class="shrink-0 text-2xs text-ink-faint opacity-0 transition-opacity group-hover:opacity-100 hover:text-ink-3"
                          title={`Copy selector ${selectorFor(key, value)}`}
                          onclick={() => copy(selectorFor(key, value), key)}
                        >
                          {copied === key ? "copied" : "copy selector"}
                        </button>
                      </div>
                    {/each}
                  </div>
                {/if}
              </section>
            {/each}
          </div>
        {/if}
      </div>

      <!-- footer -->
      <div class="flex flex-wrap items-center gap-2 border-t border-edge px-6 py-2.5 text-xs text-ink-5">
        {#if tab === "facts" && filter.trim() && docs.length}
          <span class="num">{shown} of {totalKeys} shown</span>
          <span class="text-ink-faint">·</span>
        {/if}
        <span>
          A fact is a property of the machine, not a measurement of it — load and free memory
          live on Nodes, and change without anything changing.
        </span>
        <button class={`${ui.rowAction} ml-auto`} onclick={() => onclose?.()}>Close</button>
      </div>
    </div>
  </div>
{/if}
