<script lang="ts">
  // "What can this person do?" — one editor, used everywhere the question
  // is asked: letting somebody in, adding a person by key, and granting an
  // extra role to somebody who is already here.
  //
  // **The shape is role first, consequence second, scope third.** The role
  // names are the real ones (`admin`, `operator`, …) because they are what
  // the CLI prints, what the audit log records and what every doc says —
  // renaming them in the UI only would mean two vocabularies for one thing.
  // What was missing was never the name; it was the sentence after it. So
  // every choice now answers itself: pick a role and the list underneath
  // says, in plain words, what that person will and will not be able to do,
  // recomputed live from the same rules the nodes enforce.
  //
  // **Script restrictions moved behind a disclosure.** They ride the
  // mechanism RBAC already enforces (`rbac::can_run_on`): a grant's service
  // glob must match the service name, and an ad-hoc command — which belongs
  // to no service — only passes an unrestricted `*` scope. So "only these
  // scripts" is spelled as one grant per chosen script (grants add up), and
  // typed commands and terminals are refused as a matter of arithmetic
  // rather than UI promises. It is the right control and the wrong first
  // question: most people want "operator, everywhere", and asking them to
  // rule on service globs before they get there is what made this form
  // feel like a firewall console.
  import { untrack } from "svelte";

  import { app } from "../state.svelte";
  import { ROLE_SUMMARY, ROLES } from "../roles";
  import { capabilitiesFor } from "../access";
  import RolePicker from "./RolePicker.svelte";
  import ScopePreview from "./ScopePreview.svelte";
  import Icon from "./Icon.svelte";

  let {
    role = $bindable("operator"),
    /** The node selector the grant will carry: `any`, `tag=…`, `name=…`. */
    nodes = $bindable("any"),
    /** Service globs, one grant each. `["*"]` means everything. */
    services = $bindable<string[]>(["*"]),
    /** Duration text (`30d`, `12h`); empty means never. */
    expires = $bindable(""),
    /** False when the current choices cannot be submitted yet. */
    valid = $bindable(true),
    /** Who the sentences are about. */
    subject = "They",
    showRole = true,
    /**
     * Tight layout for a card that has to fit next to a pairing code: the
     * role becomes a select with its one-liner under it rather than five
     * cards. Same choices, same consequences, a third of the height.
     */
    compact = false,
    /**
     * A field to sit beside the role picker — in practice the person's
     * name, on the join card. It belongs *in* here rather than in a column
     * beside the whole editor: the editor is tall and a name field is one
     * line, so putting them in sibling columns leaves a third of the panel
     * empty. And it has to be a snippet rather than a second editor
     * instance, because two of these would each be writing the same bound
     * scope from their own local state.
     */
    lead,
  }: {
    role?: string;
    nodes?: string;
    services?: string[];
    expires?: string;
    valid?: boolean;
    subject?: string;
    showRole?: boolean;
    compact?: boolean;
    lead?: import("svelte").Snippet;
  } = $props();

  const RUNNER_ROLES = ["admin", "operator", "developer"];
  const isRunner = $derived(RUNNER_ROLES.includes(role));

  // -- what may they run --------------------------------------------------------
  let runScope = $state<"everything" | "scripts">(
    services.length === 1 && services[0] === "*" ? "everything" : "scripts",
  );
  let picked = $state<string[]>(services.filter((s) => s !== "*" && !s.includes("*")));
  let pattern = $state(services.find((s) => s !== "*" && s.includes("*")) ?? "");
  /** The disclosure starts open when it is already narrowing something —
   *  read once, at construction, rather than tracked: it is the initial
   *  state of a panel, not a mirror of the toggle inside it. */
  let limitOpen = $state(untrack(() => runScope) === "scripts");

  const liveServices = $derived(app.services);
  const chosen = $derived([...picked, ...(pattern.trim() ? [pattern.trim()] : [])]);

  function toggle(name: string) {
    picked = picked.includes(name) ? picked.filter((p) => p !== name) : [...picked, name];
  }

  // -- where --------------------------------------------------------------------
  const meshNodes = $derived(app.nodes.filter((n) => !n.revoked && !n.left));
  const groups = $derived([...new Set(meshNodes.flatMap((n) => n.tags))].sort());

  let whereSel = $state(
    nodes === "any" || nodes === ""
      ? "any"
      : nodes.startsWith("tag=")
        ? `group:${nodes.slice(4)}`
        : nodes.startsWith("name=")
          ? `node:${nodes.slice(5)}`
          : "custom",
  );
  let customSel = $state(nodes !== "any" && !nodes.startsWith("tag=") && !nodes.startsWith("name=") ? nodes : "");

  // -- for how long -------------------------------------------------------------
  const PRESETS = ["24h", "7d", "30d", "90d"];
  let expiresSel = $state(expires === "" ? "never" : PRESETS.includes(expires) ? expires : "custom");
  let expiresCustom = $state(expires && !PRESETS.includes(expires) ? expires : "");

  // The UI state is the source of truth; the bound props are its output.
  $effect(() => {
    services = !isRunner || runScope === "everything" ? ["*"] : chosen.length ? chosen : [];
    nodes =
      whereSel === "any"
        ? "any"
        : whereSel === "custom"
          ? customSel.trim() || "any"
          : whereSel.startsWith("group:")
            ? `tag=${whereSel.slice(6)}`
            : `name=${whereSel.slice(5)}`;
    expires = expiresSel === "never" ? "" : expiresSel === "custom" ? expiresCustom.trim() : expiresSel;
    valid = !isRunner || runScope === "everything" || chosen.length > 0;
  });

  const who = $derived(subject.trim() || "They");

  /**
   * How many machines the choice actually reaches, so the consequence list
   * can say "all 4 machines" instead of "the whole mesh". The count is the
   * thing that makes a scope feel real.
   */
  const reach = $derived.by(() => {
    if (whereSel === "any") return meshNodes.length;
    if (whereSel.startsWith("group:")) {
      const tag = whereSel.slice(6);
      return meshNodes.filter((n) => n.tags.includes(tag)).length;
    }
    if (whereSel.startsWith("node:")) return 1;
    return null; // a custom selector: ScopePreview resolves it properly
  });

  // What this choice would mean, from the same rules the nodes enforce.
  // Terminal policies are left out on purpose: nobody has one yet at the
  // moment this form is being filled in, and claiming otherwise would be
  // the most misleading line on the screen.
  const caps = $derived(
    capabilitiesFor({
      roles: [role],
      scopes: isRunner ? [{ nodes, services: runScope === "everything" ? "*" : chosen.join(",") }] : [],
      isFounder: false,
      nodeCount: reach,
    }),
  );
</script>

{#if showRole}
  {#if compact}
    <div class="grid items-start gap-4 lg:grid-cols-2">
      {#if lead}{@render lead()}{/if}
      <div>
        <span class="mb-1.5 block text-sm text-ink-4">What they are</span>
        <select
          bind:value={role}
          class="select h-10 w-full rounded-md bg-canvas px-3 text-base text-ink outline-none"
          style="box-shadow: var(--sh-control);"
        >
          {#each ROLES as r (r)}
            <option value={r}>{r} — {ROLE_SUMMARY[r].one.toLowerCase()}</option>
          {/each}
        </select>
        <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">{ROLE_SUMMARY[role]?.long}</p>
      </div>
    </div>
  {:else}
    {#if lead}<div class="mb-4">{@render lead()}</div>{/if}
    <div>
      <span class="mb-1.5 block text-sm text-ink-4">What they are</span>
      <RolePicker bind:value={role} />
    </div>
  {/if}
{:else if lead}
  {@render lead()}
{/if}

<!-- The consequence, next to the choice. This is the part that was missing:
     five role names and two glob fields never told anybody what they had
     just decided. -->
<div class="mt-4 rounded-lg px-4 py-3.5" style="background: var(--surface-2);">
  <div class="label">{who} will be able to</div>
  <div class="mt-2 grid gap-x-6 gap-y-1.5 sm:grid-cols-2">
    {#each caps as c (c.what)}
      <div class="flex items-start gap-2">
        {#if c.yes}
          <span
            class="mt-[3px] inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full"
            style="background: var(--accent-wash); color: var(--accent-ink);"
          >
            <Icon name="check" size={9} />
          </span>
        {:else}
          <span class="mt-[3px] inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-surface text-ink-6">·</span>
        {/if}
        <span class="min-w-0">
          <span class="text-sm" class:text-ink={c.yes} class:text-ink-5={!c.yes}>{c.what}</span>
          <span class="mt-px block text-xs leading-relaxed text-ink-5">{c.why}</span>
        </span>
      </div>
    {/each}
  </div>
</div>

<div class="mt-4 grid gap-3 sm:grid-cols-2">
  <div>
    <span class="mb-1.5 block text-sm text-ink-4">Where?</span>
    <select
      bind:value={whereSel}
      class="select h-10 w-full rounded-md bg-canvas px-3 text-base text-ink outline-none"
      style="box-shadow: var(--sh-control);"
    >
      <option value="any">The whole mesh</option>
      {#if groups.length > 0}
        <optgroup label="A group of machines">
          {#each groups as t (t)}<option value={`group:${t}`}>machines tagged {t}</option>{/each}
        </optgroup>
      {/if}
      {#if meshNodes.length > 0}
        <optgroup label="One machine">
          {#each meshNodes as n (n.nodeId)}<option value={`node:${n.name}`}>{n.name}</option>{/each}
        </optgroup>
      {/if}
      <option value="custom">A selector expression…</option>
    </select>
    {#if whereSel === "custom"}
      <input
        bind:value={customSel}
        placeholder="any | tag=gpu | fact.cpu_cores>=32"
        class="mono mt-2 h-10 w-full rounded-md bg-canvas px-3 text-sm outline-none placeholder:font-sans placeholder:text-ink-6"
        style="box-shadow: var(--sh-control);"
      />
      <ScopePreview selector={nodes} />
    {/if}
  </div>
  <div>
    <span class="mb-1.5 block text-sm text-ink-4">For how long?</span>
    <select
      bind:value={expiresSel}
      class="select h-10 w-full rounded-md bg-canvas px-3 text-base text-ink outline-none"
      style="box-shadow: var(--sh-control);"
    >
      <option value="never">Until it is revoked</option>
      <option value="24h">24 hours</option>
      <option value="7d">7 days</option>
      <option value="30d">30 days</option>
      <option value="90d">90 days</option>
      <option value="custom">Something else…</option>
    </select>
    {#if expiresSel === "custom"}
      <input
        bind:value={expiresCustom}
        placeholder="12h, 45d…"
        class="mono mt-2 h-10 w-full rounded-md bg-canvas px-3 text-sm outline-none placeholder:font-sans placeholder:text-ink-6"
        style="box-shadow: var(--sh-control);"
      />
    {/if}
    {#if expiresSel !== "never"}
      <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
        An expiry is how temporary access stays temporary without anybody having to remember
        to take it away.
      </p>
    {/if}
  </div>
</div>

<!-- The advanced half: narrowing a role to specific scripts. Right control,
     wrong first question — so it is one line until somebody wants it. -->
{#if isRunner}
  <div class="mt-3">
    <button
      type="button"
      class="flex items-center gap-1.5 text-xs font-semibold text-ink-4 transition-colors hover:text-ink"
      aria-expanded={limitOpen}
      onclick={() => (limitOpen = !limitOpen)}
    >
      <span class="text-ink-6 transition-transform" style={limitOpen ? "transform: rotate(90deg);" : ""}>
        <Icon name="chevron-right" size={11} />
      </span>
      Limit them to certain scripts
      {#if runScope === "scripts"}
        <span
          class="mono rounded-full px-2 py-0.5 text-2xs font-semibold"
          style="background: var(--accent-wash); color: var(--accent-ink);"
        >
          {chosen.length || "none"} chosen
        </span>
      {:else}
        <span class="font-normal text-ink-5">— they can run anything by default</span>
      {/if}
    </button>

    {#if limitOpen}
      <div class="fade-in mt-2.5 rounded-lg bg-canvas p-3.5">
        <div class="inline-flex rounded-md bg-surface-2 p-[3px]">
          {#each [["everything", "Anything"], ["scripts", "Only chosen scripts"]] as [id, label] (id)}
            <button
              type="button"
              class={`rounded-sm px-[13px] py-[6px] text-sm transition-colors ${runScope === id ? "bg-surface font-semibold text-ink" : "font-medium text-ink-5 hover:text-ink"}`}
              style={runScope === id ? "box-shadow: var(--sh-control);" : ""}
              onclick={() => (runScope = id as typeof runScope)}
            >
              {label}
            </button>
          {/each}
        </div>

        {#if runScope === "scripts"}
          <div class="fade-in mt-3">
            {#if liveServices.length > 0}
              <div class="flex flex-wrap gap-1.5">
                {#each liveServices as s (s.name)}
                  {@const on = picked.includes(s.name)}
                  <button
                    type="button"
                    class="mono rounded-full px-2.5 py-1 text-xs font-semibold transition-colors"
                    style={on
                      ? "background: var(--accent-wash); color: var(--accent-ink); box-shadow: inset 0 0 0 1.5px var(--accent);"
                      : "background: var(--surface-2); color: var(--ink-4);"}
                    title={s.description || s.name}
                    aria-pressed={on}
                    onclick={() => toggle(s.name)}
                  >
                    {s.name}
                  </button>
                {/each}
              </div>
            {:else}
              <div class="text-xs text-ink-5">No saved scripts on this mesh yet — a name pattern still works.</div>
            {/if}
            <label class="mt-3 flex flex-wrap items-center gap-2 text-xs text-ink-4">
              or any script whose name matches
              <input
                bind:value={pattern}
                placeholder="deploy-*"
                class="mono h-8 w-40 rounded-sm bg-surface px-2.5 text-xs outline-none placeholder:text-ink-6"
                style="box-shadow: var(--sh-control);"
              />
            </label>
            <div class="mt-2.5 border-t border-edge pt-2 text-xs leading-relaxed text-ink-5">
              Typed commands and terminals are excluded automatically: they belong to no script, so
              a narrowed role refuses both. Every node enforces that, not this screen.
              {#if chosen.length > 1}
                <br />This writes {chosen.length} grants — one per script. They add up, and each can be
                revoked on its own.
              {/if}
            </div>
          </div>
        {/if}
      </div>
    {/if}
  </div>
{/if}
