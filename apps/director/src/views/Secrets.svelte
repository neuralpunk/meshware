<script lang="ts">
  import Icon from "../lib/components/Icon.svelte";
  // Secrets (MASTERPLAN §14.2 / §7.3): write-only values, sealed in this
  // process with the human's key for every active member node; the engine
  // and replicated state only ever see ciphertext. Names, versions and
  // decryptability are the whole visible surface.
  import { api, errText, type SecretView } from "../lib/api";
  import { app, ago, ui } from "../lib/state.svelte";
  import Empty from "../lib/components/Empty.svelte";
  import LoadError from "../lib/components/LoadError.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Hint from "../lib/components/Hint.svelte";
  import Fold from "../lib/components/Fold.svelte";
  import { staggerIn } from "../lib/motion";

  let setOpen = $state(false);
  let name = $state("");
  let value = $state("");
  let reveal = $state(false);
  let multiline = $state(false);
  let busy = $state(false);
  let err = $state<string | null>(null);
  let fileEl = $state<HTMLInputElement | null>(null);

  const existing = $derived(app.secrets.filter((s) => !s.removed));
  const isRotate = $derived(existing.some((s) => s.name === name.trim()));
  const canManage = $derived(app.whoami?.isFounder || app.whoami?.roles.includes("admin"));
  const canList = $derived(
    app.whoami?.isFounder || app.whoami?.roles.some((r) => r === "admin" || r === "developer" || r === "auditor"),
  );
  const activeNodes = $derived(app.nodes.filter((n) => !n.revoked && !n.left).length);

  function openSet(prefillName = "") {
    name = prefillName;
    value = "";
    err = null;
    reveal = false;
    setOpen = true;
  }

  function onFile(e: Event) {
    const file = (e.currentTarget as HTMLInputElement).files?.[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      let text = String(reader.result ?? "");
      if (text.endsWith("\n")) text = text.slice(0, -1); // almost always an artifact
      value = text;
      multiline = text.includes("\n");
    };
    reader.readAsText(file);
  }

  async function doSet() {
    busy = true;
    err = null;
    try {
      const r = await api.secretSet(name.trim(), value);
      app.flash(`secret '${r.name}' v${r.version} sealed for ${r.recipients} node(s) — reference it as {{secret:${r.name}}}`);
      setOpen = false;
      value = "";
      await app.refreshAfterChange();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }

  let removeTarget = $state<SecretView | null>(null);
  let removeBusy = $state(false);
  let removeErr = $state<string | null>(null);
  async function doRemove() {
    if (!removeTarget) return;
    removeBusy = true;
    removeErr = null;
    try {
      await api.secretRemove(removeTarget.name);
      app.flash(`secret '${removeTarget.name}' removed`);
      removeTarget = null;
      await app.refreshAfterChange();
    } catch (e) {
      removeErr = errText(e);
    } finally {
      removeBusy = false;
    }
  }

  async function copyRef(s: SecretView, file = false) {
    const ref = file ? `{{secret_file:${s.name}}}` : `{{secret:${s.name}}}`;
    await api.copyText(ref);
    app.flash(`${ref} copied`);
  }

  $effect(() => {
    const o = sessionStorage.getItem("secrets:open");
    if (o) {
      sessionStorage.removeItem("secrets:open");
      openSet();
    }
  });
</script>

<div class="fade-in space-y-4">
  <div class="flex items-center gap-3">
    <!-- The sentence that used to live here is the page header's now that
         Secrets is a section again rather than a tab inside Access; only
         the affordance behind it still belongs on the page. -->
    <div class="flex items-center gap-1.5 text-sm text-ink-4">
      <Hint
        label="How secrets are used"
        text={"Values are sealed on this machine and resolved only on the node that runs the job; logs are scrubbed of them. Reference one from any env value as {{secret:name}} or {{secret_file:name}} (a 0400 file, shredded after the job)."}
      />
      What is a secret, and how do I use one?
    </div>
    <div class="ml-auto flex items-center gap-2">
      {#if !canManage}<span class="text-xs text-ink-5">setting or removing secrets needs the admin role</span>{/if}
      <button class={ui.primary} disabled={!canManage} onclick={() => openSet()}>Set a secret</button>
    </div>
  </div>

  {#if setOpen}
    <div class="card fade-in p-[22px]">
      <div class="flex items-baseline gap-3">
        <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">{isRotate ? `Rotate ${name.trim()}` : "Set a secret"}</h2>
        <span class="text-sm text-ink-4" title="A machine that joins after this is set gets the secret sealed for its own key, automatically">
          sealed for {activeNodes} active machine{activeNodes === 1 ? "" : "s"}; machines that join later get it automatically
        </span>
        <button class="ml-auto text-sm font-semibold text-ink-4 hover:text-ink" onclick={() => (setOpen = false)}>Close</button>
      </div>
      <div class="mt-4 grid gap-3" style="grid-template-columns: 260px minmax(0,1fr);">
        <label class="block">
          <span class={ui.label}>Name</span>
          <input bind:value={name} placeholder="db-pass" class={ui.inputMono} />
          {#if isRotate}<span class="mt-1 block text-xs" style="color: var(--status-warn-ink);">a new version replaces the current one</span>{/if}
        </label>
        <div class="block">
          <div class="flex items-baseline gap-3">
            <span id="secret-value-label" class={ui.label}>Value</span>
            <button class="text-xs font-medium text-ink-5 hover:text-accent-ink" onclick={() => (reveal = !reveal)}>{reveal ? "hide" : "show"}</button>
            <button class="text-xs font-medium text-ink-5 hover:text-accent-ink" onclick={() => (multiline = !multiline)}>{multiline ? "single line" : "multi-line"}</button>
            <button class="ml-auto text-xs font-semibold" style="color: var(--accent-ink);" onclick={() => fileEl?.click()}>Load from file…</button>
            <input bind:this={fileEl} type="file" class="hidden" onchange={onFile} />
          </div>
          {#if multiline}
            <textarea
              bind:value={value}
              rows="5"
              spellcheck="false"
              aria-labelledby="secret-value-label"
              class="mono w-full resize-y rounded-md bg-canvas px-3 py-2 text-sm leading-relaxed outline-none"
              style={reveal ? "" : "-webkit-text-security: disc;"}
            ></textarea>
          {:else}
            <input
              bind:value={value}
              type={reveal ? "text" : "password"}
              autocomplete="off"
              aria-labelledby="secret-value-label"
              class={ui.inputMono}
              onkeydown={(e) => e.key === "Enter" && name.trim() && value && doSet()}
            />
          {/if}
          <span class="mt-1 block text-xs text-ink-5">{value.length} byte{value.length === 1 ? "" : "s"} · never written to disk in the clear, never logged</span>
        </div>
      </div>
      {#if err}<div class={ui.errorBox}>{err}</div>{/if}
      <div class="mt-4">
        <button class={ui.primary} disabled={busy || !name.trim() || !value} onclick={doSet}>{busy ? "Sealing…" : isRotate ? "Seal new version" : "Seal secret"}</button>
      </div>
    </div>
  {/if}

  <!-- Before the list, and instead of the empty state (UX-2): a refused
       read used to land on "No secrets yet — Set a secret", which tells a
       person without the grant that nothing is there and to make some. -->
  {#if canList && app.loadErrors.secrets}
    <LoadError what="the secrets" error={app.loadErrors.secrets} />
  {/if}

  {#if !canList}
    <div class="card empty">
      <div class="empty-title">Secrets are not visible to your role</div>
      <div class="empty-sub">Listing secret names needs admin, developer or auditor. Jobs you run can still use references a developer put in a service's env.</div>
    </div>
  {:else if existing.length === 0 && app.loadErrors.secrets}
    <!-- Said above. -->
  {:else if existing.length === 0}
    <!-- The empty state that used to be a grey line inside an otherwise
         empty table. A person who has never set a secret needs the button,
         and needs to be told where the value goes before they type one
         (L1 P4). -->
    <Empty
      title="No secrets yet"
      sub="A value you set here is sealed in this window, for every member node, before it goes anywhere. Jobs reference it by name and never print it."
      command="meshnode secrets set DB_PASSWORD"
      reassurance={`Sealed on this machine before it leaves it · wrapped for ${activeNodes} node${activeNodes === 1 ? "" : "s"} · nothing runs`}
      action={canManage ? { label: "Set a secret", run: () => openSet() } : undefined}
    />
  {:else}
    <!-- A size container (UX-11): at the 980px minimum the table wrapped
         names mid-word and pushed Remove off the card. Narrow, it drops the
         two things said elsewhere — who set it (the name's tooltip) and the
         file-reference chip (the plain reference copies either way) — and
         the actions keep a column of their own. -->
    <div class="@container card overflow-hidden">
      <table class="w-full text-left text-base">
        <thead>
          <tr class="border-b border-edge">
            <th class="label px-5 py-3">Secret</th>
            <th class="label px-3 py-3">Version</th>
            <th class="label px-3 py-3 @max-[820px]:hidden">Set by</th>
            <th class="label px-3 py-3">Decryptable</th>
            <th class="label px-3 py-3">Reference</th>
            <th class="label w-[172px] px-3 py-3 pr-5 text-right"><span class="sr-only">Actions</span></th>
          </tr>
        </thead>
        <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
          {#each existing as s (s.name)}
            {@const pct = s.members === 0 ? 0 : Math.round((s.wrappedFor / s.members) * 100)}
            <tr class="h-12 border-b border-edge transition-colors hover:bg-surface-2 last:border-0">
              <td class="px-5 py-2.5">
                <div class="flex items-center gap-2.5">
                  <span class="grid h-6 w-6 flex-none place-items-center rounded-md" style="background: var(--hue-wash); color: var(--hue-ink);" aria-hidden="true">
                    <Icon name="secrets" size={13} />
                  </span>
                  <span class="mono font-semibold whitespace-nowrap text-ink" title={`set by ${s.createdByName} ${ago(s.createdAtMicros)}`}>{s.name}</span>
                </div>
              </td>
              <td class="num px-3 py-2.5 text-sm text-ink-3">v{s.version}</td>
              <td class="px-3 py-2.5 text-sm whitespace-nowrap text-ink-3 @max-[820px]:hidden">{s.createdByName} <span class="text-ink-5">· {ago(s.createdAtMicros)}</span></td>
              <td class="px-3 py-2.5">
                <div class="flex items-center gap-2">
                  <div class="h-1.5 w-20 flex-none overflow-hidden rounded-full bg-surface-2 @max-[820px]:w-12">
                    <div class="h-full rounded-full" style={`width:${pct}%; background:${pct === 100 ? "var(--accent)" : "var(--status-warn)"};`}></div>
                  </div>
                  <span class="num text-xs whitespace-nowrap" style={pct === 100 ? "color: var(--accent-ink);" : "color: var(--status-warn-ink);"}>{s.wrappedFor}/{s.members} machines</span>
                </div>
              </td>
              <td class="px-3 py-2.5 whitespace-nowrap">
                <button class="mono rounded-xs bg-surface-2 px-1.5 py-0.5 text-xs text-ink-3 hover:text-accent-ink" title="copy" onclick={() => copyRef(s)}>{"{{secret:"}{s.name}{"}}"}</button>
                <button class="mono ml-1 rounded-xs bg-surface-2 px-1.5 py-0.5 text-xs text-ink-5 hover:text-accent-ink @max-[820px]:hidden" title="copy file reference" onclick={() => copyRef(s, true)}>file</button>
              </td>
              <td class="w-[172px] px-3 py-2.5 pr-5 text-right whitespace-nowrap">
                <button class={ui.rowAction} disabled={!canManage} onclick={() => openSet(s.name)}>Rotate</button>
                <button class={`${ui.rowDanger} ml-1`} disabled={!canManage} onclick={() => { removeErr = null; removeTarget = s; }}>Remove</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}

  <!-- providers -->
  <!-- Advanced, and folded (UX-17). The card used to sit open under every
       visit, first ones included, with a line of TOML in it — the one thing
       on this screen a new admin does not need. It opens by itself only
       where a provider is actually configured. -->
  <div class="px-1">
    <Fold
      title="Advanced: external secret providers"
      summary={app.providers.length === 0 ? "none on this machine" : `${app.providers.length} configured`}
      help={"A provider is a plugin a node asks for a value when a job starts, instead of a value sealed here. Reference one as {{secret:<provider>:<path>#<key>}}."}
      startOpen={app.providers.length > 0}
    >
      {#if app.providers.length === 0}
        <div class="pt-2 text-sm text-ink-5">
          None configured. A node adds one with <span class="mono">[secrets.providers.&lt;name&gt;] plugin = "/path/to/plugin"</span> in its <span class="mono">meshnode.toml</span> — this machine's included.
        </div>
      {:else}
        <div class="flex flex-wrap gap-2 pt-2">
          {#each app.providers as p (p.name)}
            <span class="rounded-sm bg-canvas px-3 py-1.5 text-sm"><span class="mono font-semibold text-ink">{p.name}</span> <span class="mono text-ink-5">{p.plugin}</span></span>
          {/each}
        </div>
      {/if}
    </Fold>
  </div>
</div>

<Modal open={removeTarget !== null} kicker="remove secret" peer={removeTarget?.name ?? ""} onclose={() => (removeTarget = null)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Remove {removeTarget?.name}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      Jobs whose env references <span class="mono" style="color: var(--ink-code-2);">{"{{secret:"}{removeTarget?.name}{"}}"}</span> will fail to start until it is set again. Past versions become unresolvable everywhere.
    </p>
    {#if removeErr}<div class="errbox mt-3">{removeErr}</div>{/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button class="btn btn-lg border border-edge-code font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]" onclick={() => (removeTarget = null)}>Cancel</button>
      <button class="btn btn-danger btn-lg" disabled={removeBusy} onclick={doRemove}>{removeBusy ? "Removing…" : "Remove secret"}</button>
    </div>
  </div>
</Modal>
