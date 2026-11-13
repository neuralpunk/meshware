<script lang="ts">
  // Write a session policy: who may open a terminal, and where (M17 §3.4).
  //
  // The form is deliberately shaped like the sentence it produces, because
  // the underlying rule composes with two *other* rules and nobody holds
  // three conditions in their head by accident. So the panel says the whole
  // sentence back — "ops-* may open a terminal on the 3 nodes matching
  // tag=dev, if they can already run work there and those nodes accept
  // terminals" — and resolves the node half live.
  import { untrack } from "svelte";

  import { api, errText, parseDuration } from "../api";
  import type { SessionPolicyView } from "../api";
  import { ui } from "../state.svelte";
  import Icon from "./Icon.svelte";
  import ScopePreview from "./ScopePreview.svelte";

  let {
    /** Editing an existing policy re-publishes it under the same name. */
    existing = null,
    onclose,
    onsaved,
  }: {
    existing?: SessionPolicyView | null;
    onclose: () => void;
    onsaved: () => void;
  } = $props();

  // Seeded once. The panel is remounted per open, and an editor whose
  // fields moved under the cursor because a background refresh landed is a
  // worse editor than one that does not.
  const seed = untrack(() => existing);
  let id = $state(seed?.id ?? "");
  let description = $state(seed?.description ?? "");
  let users = $state(seed?.users ?? "*");
  let nodes = $state(seed?.nodes ?? "any");
  let privileged = $state(seed?.allow.includes("shell-privileged") ?? false);
  let expires = $state("");
  let busy = $state(false);
  let error = $state<string | null>(null);

  const editing = seed !== null && seed !== undefined;

  const blocker = $derived.by((): string | null => {
    if (!id.trim()) return "Name this policy.";
    if (!/^[a-z0-9][a-z0-9-]*$/i.test(id.trim()))
      return "A name is letters, digits and dashes.";
    if (!users.trim()) return "Say which people — `*` for everyone.";
    if (!nodes.trim()) return "Say which nodes — `any` for everywhere.";
    return null;
  });

  async function save() {
    busy = true;
    error = null;
    try {
      await api.sessionPolicyPut({
        id: id.trim(),
        description: description.trim(),
        users: users.trim(),
        nodes: nodes.trim(),
        allow: privileged ? ["shell", "shell-privileged"] : ["shell"],
        expiresSecs: parseDuration(expires),
      });
      onsaved();
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="card fade-in p-[22px]">
  <div class="flex items-baseline gap-3">
    <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">
      {editing ? `Edit “${seed?.id}”` : "New terminal policy"}
    </h2>
    <span class="text-sm text-ink-4">
      one policy, one sentence: who, and where
    </span>
    <button class="ml-auto text-sm font-semibold text-ink-4 hover:text-ink" onclick={onclose}>
      Close
    </button>
  </div>

  <div class="mt-4 grid gap-3 lg:grid-cols-3">
    <label class="block">
      <span class={ui.label}>Policy name</span>
      <input
        bind:value={id}
        disabled={editing}
        placeholder="dev-terminals"
        class={ui.inputMono}
      />
    </label>
    <label class="block lg:col-span-2">
      <span class={ui.label}>What it is for <span class="text-ink-6">(optional)</span></span>
      <input
        bind:value={description}
        placeholder="on-call troubleshooting on the dev tier"
        class={ui.input}
      />
    </label>
  </div>

  <div class="mt-3 grid gap-3 lg:grid-cols-2">
    <div>
      <label class="block">
        <span class={ui.label}>
          Which people <span class="text-ink-6">(username, or a glob like ops-*)</span>
        </span>
        <input
          bind:value={users}
          placeholder="*"
          class={ui.inputMono}
        />
      </label>
      <p class="mt-1.5 mb-0 text-xs text-ink-5">
        Matched against the username in their user record, so it survives a key rotation.
      </p>
    </div>
    <div>
      <label class="block">
        <span class={ui.label}>
          Which nodes <span class="text-ink-6">(any, tag=…, name=…)</span>
        </span>
        <input
          bind:value={nodes}
          placeholder="any"
          class={ui.inputMono}
        />
      </label>
      <ScopePreview
        selector={nodes}
        terminals
        placeholder="type a selector to see what it covers"
      />
    </div>
  </div>

  <div class="mt-3 grid gap-3 lg:grid-cols-2">
    <label class="flex cursor-pointer items-start gap-2.5 rounded-lg bg-surface-2 px-3.5 py-3">
      <input
        type="checkbox"
        bind:checked={privileged}
        class="mt-[3px] h-3.5 w-3.5 accent-[var(--accent)]"
      />
      <span>
        <span class="text-sm font-medium text-ink">Also allow a root terminal</span>
        <span class="mt-0.5 block text-xs leading-relaxed text-ink-5">
          Only means anything on a node whose daemon runs as root — the packaged service
          runs as an unprivileged <span class="mono">meshnode</span> user, where a terminal
          is never a root terminal. Such a node <em>also</em> has to opt in with
          <span class="mono">[sessions] allow_root</span>, so this is one of two switches
          rather than a way to get root.
        </span>
      </span>
    </label>
    <label class="block">
      <span class={ui.label}>
        Expires <span class="text-ink-6">(30d, 12h — blank means never)</span>
      </span>
      <input
        bind:value={expires}
        placeholder="never"
        class={ui.inputMono}
      />
      <p class="mt-1.5 mb-0 text-xs text-ink-5">
        An expiring policy is how temporary access stays temporary without anybody having
        to remember to take it away.
      </p>
    </label>
  </div>

  <!-- The sentence, said back. A policy is one of three conditions and the
       other two are invisible from here, so this is where they get named. -->
  <div class="mt-4 rounded-lg px-4 py-3" style="background: var(--accent-wash);">
    <div class="text-sm leading-relaxed" style="color: var(--accent-ink);">
      <span class="mono font-semibold">{users.trim() || "…"}</span>
      may open a terminal on
      <span class="mono font-semibold">{nodes.trim() || "…"}</span>{#if privileged}, including
        as root where that node permits it{/if} —
      <span class="font-semibold">provided</span> they can already run work there, and those
      nodes have terminals switched on.
    </div>
    <div class="mt-2 flex items-start gap-1.5 text-xs" style="color: var(--accent-ink); opacity: .8;">
      <span class="mt-[2px]"><Icon name="alert" size={10} /></span>
      <span>
        This policy grants nothing on its own. It is the third of three gates, and the only
        one an admin sets from here — the runner grant is on the People tab, and switching
        terminals on is done on each node.
      </span>
    </div>
  </div>

  {#if error}
    <div
      class={ui.errorBox}
      style="background: var(--status-error-wash); color: var(--status-error-ink);"
    >
      {error}
    </div>
  {/if}

  <div class="mt-4 flex items-center gap-2">
    <button class={ui.primary} disabled={busy || blocker !== null} onclick={save}>
      {busy ? "Publishing…" : blocker ?? (editing ? "Republish policy" : "Create policy")}
    </button>
    <button class={ui.secondary} onclick={onclose}>Cancel</button>
  </div>
</div>
