<script lang="ts">
  // The answer to a dry run: the fully resolved submission, and nothing
  // dispatched.
  //
  // Secrets appear here by *name* only. A preview is a thing people paste
  // into tickets, and the moment it can carry a value it becomes a way to
  // exfiltrate one.
  import type { JobPreviewView } from "../api";

  let { preview }: { preview: JobPreviewView } = $props();
</script>

<div class="space-y-3">
  <div class="rounded-lg bg-surface-2 px-4 py-3">
    <div class="flex items-baseline gap-2">
      <span class="label">would run</span>
      <span class="mono selectable text-sm text-ink">{preview.action}</span>
    </div>
    <div class="mt-2.5 grid grid-cols-2 gap-x-6 gap-y-1.5 text-sm">
      <div class="flex gap-2">
        <span class="w-20 shrink-0 text-ink-5">on</span>
        <span class="mono text-ink-3">{preview.selector}</span>
      </div>
      <div class="flex gap-2">
        <span class="w-20 shrink-0 text-ink-5">blast radius</span>
        <span class="num text-ink-2">
          {preview.blastRadius}
          {preview.blastRadius === 1 ? "node" : "nodes"}
        </span>
      </div>
      {#if preview.service}
        <div class="flex gap-2">
          <span class="w-20 shrink-0 text-ink-5">service</span>
          <span class="mono text-ink-3"
            >{preview.service}{#if preview.version !== null}@{preview.version}{/if}</span
          >
        </div>
      {/if}
      {#if preview.runtime}
        <div class="flex gap-2">
          <span class="w-20 shrink-0 text-ink-5">runtime</span>
          <span class="mono text-ink-3">{preview.runtime}</span>
        </div>
      {/if}
      {#if preview.bundleId}
        <div class="flex gap-2">
          <span class="w-20 shrink-0 text-ink-5">bundle</span>
          <span class="mono truncate text-ink-3" title={preview.bundleId}>{preview.bundleId}</span>
        </div>
      {/if}
      <div class="flex gap-2">
        <span class="w-20 shrink-0 text-ink-5">priority</span>
        <span class="text-ink-3">{preview.priority}</span>
      </div>
      {#if preview.timeoutSecs !== null}
        <div class="flex gap-2">
          <span class="w-20 shrink-0 text-ink-5">timeout</span>
          <span class="num text-ink-3">{preview.timeoutSecs}s</span>
        </div>
      {/if}
    </div>

    <div class="mt-3 space-y-1.5 text-sm">
      <div class="flex gap-2">
        <span class="w-20 shrink-0 text-ink-5">targets</span>
        <span class="mono text-ink-2">
          {preview.targets.length ? preview.targets.join(", ") : "nothing matches"}
        </span>
      </div>
      {#if preview.excluded.length}
        <!-- "It matched ten and will hit three" is precisely the surprise
             a preview exists to prevent, so the gap is named. -->
        <div class="flex gap-2">
          <span class="w-20 shrink-0 text-ink-5">excluded</span>
          <span class="mono" style="color: var(--status-warn-ink);">
            {preview.excluded.join(", ")} — your grants do not cover
            {preview.excluded.length === 1 ? "it" : "them"}
          </span>
        </div>
      {/if}
      {#if preview.envKeys.length}
        <div class="flex gap-2">
          <span class="w-20 shrink-0 text-ink-5">env</span>
          <span class="mono text-ink-3">{preview.envKeys.join(", ")}</span>
        </div>
      {/if}
      {#if preview.secrets.length}
        <div class="flex gap-2">
          <span class="w-20 shrink-0 text-ink-5">secrets</span>
          <span class="mono text-ink-3">
            {preview.secrets.join(", ")}
            <span class="text-ink-6">(names only)</span>
          </span>
        </div>
      {/if}
    </div>
  </div>

</div>
