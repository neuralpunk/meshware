<script lang="ts">
  // Choosing a role should not require already knowing what the roles are.
  //
  // A `<select>` of five words assumes the reader can expand "operator"
  // into a permission set from memory. Cards cannot be skimmed past in the
  // same way: the consequence of the choice is next to the choice, which is
  // the only place it is any use.
  import { ROLE_SUMMARY, roleHue } from "../roles";
  import { hueStyle } from "../hues";

  let {
    value = $bindable<string>("operator"),
    roles = ["admin", "operator", "developer", "auditor", "viewer"],
  }: { value?: string; roles?: readonly string[] } = $props();
</script>

<div class="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
  {#each roles as role (role)}
    {@const selected = value === role}
    <button
      type="button"
      class="rounded-lg px-3.5 py-3 text-left transition-colors"
      style={`${hueStyle(roleHue(role))} ${selected
        ? "background: var(--hue-wash); box-shadow: inset 0 0 0 1.5px var(--hue);"
        : "background: var(--surface-2);"}`}
      aria-pressed={selected}
      onclick={() => (value = role)}
    >
      <div class="flex items-baseline gap-2">
        <span
          class="mono text-sm font-semibold"
          style={selected ? "color: var(--hue-ink);" : "color: var(--ink-2);"}
        >
          {role}
        </span>
        {#if role === "admin"}
          <span
            class="rounded-full px-1.5 py-px text-2xs font-semibold"
            style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
          >
            can grant access
          </span>
        {/if}
      </div>
      <div class="mt-0.5 text-xs font-medium text-ink-3">{ROLE_SUMMARY[role].one}</div>
      <div class="mt-1 text-xs leading-relaxed text-ink-5">{ROLE_SUMMARY[role].long}</div>
    </button>
  {/each}
</div>
