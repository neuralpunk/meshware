<script lang="ts">
  // The definition editor: paste it, pick it, or drop it — then plan it,
  // then publish it.
  //
  // Shaped after Nomad's Run Job screen, which gets the *order* right: a
  // big monospace box you can paste a whole spec into, a way to attach a
  // file if you have one, and a plan that runs before anything is
  // submitted. The plan is the part worth copying deliberately. Publishing
  // a deployment is not like saving a preference — it starts processes on
  // other people's machines — and a screen that offers "Publish" as the
  // only button teaches people to find out what a file does by doing it.
  //
  // One component for deployments and schedules because the flow is
  // identical and the only differences are the words and which command runs
  // the plan. Two copies would have drifted the first time one of them
  // learned something.
  import { errText } from "../api";
  import { api } from "../api";

  type Props = {
    /** `deployment` | `schedule` — used in prose only. */
    noun: string;
    /** The definition text. Bound, so the parent owns it. */
    value: string;
    /** Rows for the box. A spec is a page; give it a page. */
    rows?: number;
    /** Run the dry run. Resolves when the plan has been fetched. */
    onplan: () => Promise<void>;
    /** Publish for real. */
    onsubmit: () => Promise<void>;
    /** Label for the submit button — "Publish" or "Save and roll". */
    submitLabel?: string;
    busy?: boolean;
    /** True once a plan has come back clean enough to publish. */
    planned?: boolean;
    /** Parse without a mesh as the text changes (E4). Resolves to what the
     *  file says, as label/value rows; rejects with the parser's message.
     *  Absent means no inline check. */
    validate?: (source: string) => Promise<[string, string][]>;
    /** The JSON Schema for this kind of file, for the copy button (E4). */
    schema?: () => Promise<string>;
  };

  let {
    noun,
    value = $bindable(),
    rows = 16,
    onplan,
    onsubmit,
    submitLabel = "Publish",
    busy = false,
    planned = false,
    validate,
    schema,
  }: Props = $props();

  let fileErr = $state<string | null>(null);

  type Lint = { ok: true; rows: [string, string][] } | { ok: false; message: string };
  let lint = $state<Lint | null>(null);
  let schemaCopied = $state(false);

  // E4: the parser, asked as you type. Offline — no node is consulted — so
  // a missing `name` or a mistyped key is said beside the box instead of
  // being found by pressing Plan. Debounced, and an answer for text that
  // has since been replaced is dropped rather than shown over newer text.
  $effect(() => {
    const text = value;
    const check = validate;
    if (!check || !text.trim()) {
      lint = null;
      return;
    }
    let current = true;
    const timer = setTimeout(async () => {
      try {
        const rows = await check(text);
        if (current) lint = { ok: true, rows };
      } catch (e) {
        if (current) lint = { ok: false, message: errText(e) };
      }
    }, 400);
    return () => {
      current = false;
      clearTimeout(timer);
    };
  });

  async function copySchema() {
    if (!schema) return;
    fileErr = null;
    try {
      await api.copyText(await schema());
      schemaCopied = true;
      setTimeout(() => (schemaCopied = false), 2000);
    } catch (e) {
      fileErr = errText(e);
    }
  }
  let dropping = $state(false);
  let loadedFrom = $state<string | null>(null);

  /** Read a picked or dropped file — by its token (SEC-14). */
  async function loadPick(pick: { token: string; path: string }) {
    fileErr = null;
    try {
      value = await api.readSpecFile(pick.token);
      loadedFrom = pick.path;
    } catch (e) {
      fileErr = errText(e);
    }
  }

  async function pick() {
    // The picker itself can fail — no portal on this desktop, a cancelled
    // dialog, a build with no dialog plugin — and an unhandled rejection
    // here left the button doing nothing at all with no explanation.
    fileErr = null;
    try {
      const path = await api.pickFile(`Choose a ${noun} definition`, [
        "toml",
        "yaml",
        "yml",
        "json",
      ]);
      if (path) await loadPick(path);
    } catch (e) {
      fileErr = `${errText(e)} — you can still paste the definition into the box.`;
    }
  }

  // Tauri's drag-drop, not the browser's: the webview intercepts file
  // drops. Its own event drives the highlight; the file itself arrives as
  // a pick from the Rust side (`mw-dropped`), so a dropped definition
  // takes exactly the same route as a picked one (SEC-14). The same idiom
  // `Library.svelte` uses for dropped scripts.
  $effect(() => {
    const stops: (() => void)[] = [];
    let live = true;
    const keep = (stop: () => void) => (live ? stops.push(stop) : stop());
    void (async () => {
      try {
        const { getCurrentWebview } = await import("@tauri-apps/api/webview");
        keep(
          await getCurrentWebview().onDragDropEvent((ev) => {
            dropping = ev.payload.type === "over";
          }),
        );
        keep(
          await api.onDropped((picks) => {
            if (picks[0]) void loadPick(picks[0]);
          }),
        );
      } catch {
        // Not under Tauri (the mock browser build). Paste and the picker
        // still work; nothing here is load-bearing.
      }
    })();
    return () => {
      live = false;
      for (const stop of stops) stop();
    };
  });
</script>

<div
  class="rounded-lg border transition-colors"
  style="border-color: {dropping ? 'var(--accent-ink)' : 'var(--edge)'}"
>
  <div
    class="flex items-center gap-3 border-b border-edge px-4 py-2.5"
    style="background: var(--surface-2)"
  >
    <span class="text-xs font-semibold text-ink-2">Definition</span>
    <span class="text-xs text-ink-5">
      Paste TOML, YAML or JSON — or drop a file anywhere on this window.
    </span>
    <div class="flex-1"></div>
    {#if schema}
      <button
        class="btn btn-ghost h-7 px-2.5 text-xs"
        title="For an editor's YAML or JSON extension: autocomplete, and a warning on any key the parser would refuse"
        onclick={() => void copySchema()}>{schemaCopied ? "Copied" : "Copy JSON Schema"}</button
      >
    {/if}
    <button class="btn btn-secondary h-7 px-2.5 text-xs" onclick={pick}>Choose a file…</button>
  </div>

  {#if dropping}
    <div
      class="px-4 py-1.5 text-xs font-semibold"
      style="background: var(--accent-wash); color: var(--accent-ink)"
    >
      Drop to load it into the editor
    </div>
  {/if}

  <!-- Monospace, dark, and tall. A spec is read as a shape — indentation
       and block boundaries — and a proportional font in a three-line box
       hides both. -->
  <textarea
    bind:value
    {rows}
    spellcheck="false"
    autocapitalize="off"
    class="block w-full resize-y border-0 px-4 py-3 font-mono text-xs leading-relaxed outline-none"
    style="background: var(--surface); color: var(--ink-2); tab-size: 2"
  ></textarea>

  <!-- E4: what the parser makes of the text, offline, as it is typed. The
       mesh's opinion is still the plan's job; this is only the file's. -->
  {#if lint}
    <div
      class="border-t border-edge px-4 py-2 text-xs"
      style={lint.ok ? "" : "background: var(--status-error-wash);"}
    >
      {#if lint.ok}
        <span class="font-semibold" style="color: var(--status-ok-ink);">Valid</span>
        <span class="text-ink-4">
          · {lint.rows
            .filter(([k]) => k === "runs" || k === "count" || k === "failover")
            .map(([, v]) => v)
            .join(" · ")}
        </span>
      {:else}
        <span class="selectable font-mono" style="color: var(--status-error-ink); white-space: pre-wrap;">{lint.message}</span>
      {/if}
    </div>
  {/if}

  <div class="flex flex-wrap items-center gap-2 border-t border-edge px-4 py-2.5">
    <button class="btn btn-secondary btn-md" disabled={busy || !value.trim()} onclick={() => void onplan()}>
      {busy ? "Working…" : "Plan"}
    </button>
    <button class="btn btn-primary btn-md" disabled={busy || !value.trim()} onclick={() => void onsubmit()}>
      {submitLabel}
    </button>
    {#if loadedFrom}
      <span class="num text-xs text-ink-5" title={loadedFrom}>
        loaded {loadedFrom.split("/").pop()}
      </span>
    {/if}
    <div class="flex-1"></div>
    {#if !planned}
      <span class="text-xs text-ink-5">
        Plan first to see what it would do.
      </span>
    {/if}
  </div>
</div>

{#if fileErr}
  <div class="errbox mt-2 text-xs">{fileErr}</div>
{/if}
