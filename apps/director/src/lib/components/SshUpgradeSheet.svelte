<script lang="ts">
  // Update over SSH — the way a fleet is updated.
  //
  // Director pushes a verified binary over SSH, swaps it, restarts the
  // service, and waits for the mesh to see the node on the new version.
  // This is the **primary** path, not a fallback: the shipped systemd unit
  // runs the daemon unprivileged under `ProtectSystem=full`, so a node
  // cannot write its own binary and self-upgrade fails by design (it is
  // opt-in, per host, in a drop-in). A path that needs an operator to
  // relax hardening on every machine before it works is not the path a
  // fleet should meet first.
  //
  // ---------------------------------------------------------------------
  // What changes at fleet size
  // ---------------------------------------------------------------------
  //
  // The form used to draw a row with an address field per node. That reads
  // fine for three machines and collapses at forty: the thing you have to
  // fill in — credentials — ends up a thousand pixels below the fold,
  // under forty inputs nobody was going to edit, because the address the
  // mesh already knows is right every time but the exception.
  //
  // So the addresses are **stated, not asked**: one line saying where they
  // came from, and a disclosure for the exception. What is in front of you
  // is the decision (which nodes, which credentials) and the button.
  import { dialog } from "../dialog";
  import { api, errText, type NodeView, type DeployEvent, type SshUpgradeDraft } from "../api";
  import { app } from "../state.svelte";
  import { rememberSshPort, rememberedSshPort } from "../sshport";
  import CredentialsForm from "./CredentialsForm.svelte";
  import { type Card, applyEvent, newCard } from "./DeployCards.svelte";
  import RolloutProgress from "./RolloutProgress.svelte";
  import Icon from "./Icon.svelte";
  import Dot from "./Dot.svelte";
  import Hint from "./Hint.svelte";
  import { portal } from "../portal";
  import { popIn, fadeIn, DUR } from "../motion";

  let {
    open = false,
    onclose,
    nodes,
    version = null,
    startForced = false,
  }: {
    open?: boolean;
    onclose: () => void;
    /** Live nodes that are behind, pre-ticked. */
    nodes: NodeView[];
    /** The release to install; null means the newest at hand. */
    version?: string | null;
    /** Open with "reinstall even if the version matches" already ticked —
     *  what the Reinstall button on the Updates card means. */
    startForced?: boolean;
  } = $props();

  type Step = "form" | "run";
  let step = $state<Step>("form");
  let picked = $state(new Set<string>());
  let hosts = $state<Record<string, string>>({});
  let auth = $state<SshUpgradeDraft["auth"]>({ user: "", method: "password", password: null, keyPath: null, passphrase: null, sudoPassword: null, remember: false, fromKeychain: false });
  /** The form's word on whether the login is filled in — the keychain
   *  counts, which the draft alone cannot show (S3). */
  let authComplete = $state(false);
  /** The SSH port for rows that do not name one. Said once for the rack;
   *  a row that spells out `host:2222` still overrides it. */
  let port = $state(rememberedSshPort());
  let force = $state(false);
  /** Host overrides, open only when somebody asks for them. */
  let hostsOpen = $state(false);
  let hostFilter = $state("");
  /** How many machines are worked at once. Four is right for a handful and
   *  slow for eighty, so it scales with the run and stays inside what the
   *  engine's semaphore allows (32) — and inside what a person's ssh-agent
   *  and network will thank them for. */
  let parallel = $state(4);
  /** A binary to push instead of the release the mesh holds. A version
   *  names bytes the mesh already has; this is for the bytes it does not —
   *  a build that has not been published, or one that never will be. */
  // A picked binary: the token goes to the backend, the path is shown
  // (SEC-14).
  let binaryPath = $state("");
  let binaryShown = $state("");
  let error = $state<string | null>(null);
  let cards = $state<Card[]>([]);
  let runId = $state<number | null>(null);
  let finished = $state<{ done: number; failed: number; cancelled: boolean } | null>(null);

  /** The host SSH reaches: the node's first real address, minus the mesh port. */
  function guessHost(n: NodeView): string {
    const a = n.addrs.find((x) => !x.startsWith("127.") && !x.startsWith("[::1]") && !x.startsWith("relay:")) ?? n.addrs[0] ?? "";
    const cut = a.lastIndexOf(":");
    const host = cut > 0 ? a.slice(0, cut) : a;
    return host.replace(/^\[|\]$/g, "");
  }

  $effect(() => {
    if (open) {
      step = "form";
      const h: Record<string, string> = {};
      for (const n of nodes) h[n.nodeId] = guessHost(n);
      // A node the mesh has no address for starts unticked rather than
      // blocking the other forty-three. It is named underneath, so it is
      // dropped in the open, not silently.
      picked = new Set(nodes.filter((n) => guessHost(n).trim()).map((n) => n.nodeId));
      hosts = h;
      auth = { user: "", method: "password", password: null, keyPath: null, passphrase: null, sudoPassword: null, remember: false, fromKeychain: false };
      force = startForced;
      port = rememberedSshPort();
      binaryPath = "";
      binaryShown = "";
      error = null;
      cards = [];
      runId = null;
      finished = null;
    }
  });

  const chosen = $derived(nodes.filter((n) => picked.has(n.nodeId)));
  const autoParallel = (n: number) => (n <= 4 ? Math.max(1, n) : Math.min(16, Math.ceil(n / 4)));
  $effect(() => {
    parallel = autoParallel(nodes.length);
  });

  /** Nodes whose address the mesh could not supply — the only ones that
   *  genuinely need typing, so they are the only ones surfaced by default. */
  const missingHost = $derived(chosen.filter((n) => !(hosts[n.nodeId] ?? "").trim()));
  /** Left out because the mesh has no address for them — not an error, but
   *  not something to discover afterwards either. */
  const unreachable = $derived(nodes.filter((n) => !picked.has(n.nodeId) && !(hosts[n.nodeId] ?? "").trim()));
  const shownHosts = $derived.by(() => {
    const q = hostFilter.trim().toLowerCase();
    const pool = nodes;
    return q ? pool.filter((n) => n.name.toLowerCase().includes(q) || (hosts[n.nodeId] ?? "").toLowerCase().includes(q)) : pool;
  });

  /** A fleet reads as racks and roles, not as fifty names. Whatever tag
   *  prefix most nodes share becomes the summary. */
  const groups = $derived.by(() => {
    const by = new Map<string, number>();
    for (const n of chosen) {
      const key = n.tags.find((t) => !t.includes("=")) ?? n.tags[0] ?? "untagged";
      by.set(key, (by.get(key) ?? 0) + 1);
    }
    return [...by.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6);
  });
  const archSpread = $derived.by(() => {
    const by = new Map<string, number>();
    for (const n of chosen) by.set(n.capacity?.arch ?? "unknown", (by.get(n.capacity?.arch ?? "unknown") ?? 0) + 1);
    return [...by.entries()].sort((a, b) => b[1] - a[1]);
  });
  const portOk = $derived(Number.isInteger(Number(port)) && Number(port) >= 1 && Number(port) <= 65535);
  const ready = $derived(
    chosen.length > 0 &&
      portOk &&
      // An empty address is not a blank to fill: the engine resolves it
      // from the node's own mesh address. Only a node the mesh has no
      // address for actually needs typing.
      missingHost.length === 0 &&
      authComplete,
  );

  function toggle(id: string) {
    const next = new Set(picked);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    picked = next;
  }

  async function start() {
    error = null;
    const draft: SshUpgradeDraft = {
      nodes: chosen.map((n) => ({ nodeId: n.nodeId, host: hosts[n.nodeId]?.trim() || null })),
      auth,
      // A picked file outranks the version: it is the operator pointing at
      // the bytes they mean.
      version: binaryPath.trim() ? null : version,
      binaryPath: binaryPath.trim() || null,
      parallel,
      port: Math.max(1, Math.min(65535, Math.trunc(Number(port) || 22))),
      force,
    };
    cards = chosen.map((n) => newCard(`${n.nodeId}|${hosts[n.nodeId]?.trim()}`, n.name));
    step = "run";
    rememberSshPort(draft.port);
    const remember = auth.remember && auth.user.trim() !== "";
    const saved = { ...auth };
    try {
      runId = await api.upgradeSshStart(draft, (ev: DeployEvent) => {
        if (ev.type === "plan") {
          runId = ev.runId;
          // The engine normalises each address — it fills in the run's
          // port — so the keys it sends back are not the ones built here.
          // Re-key on the node id, which is the half that cannot change,
          // and keep the names the person knows rather than the addresses.
          cards = ev.hosts.map((h) => {
            const id = h.split("|")[0];
            const prior = cards.find((c) => c.host.split("|")[0] === id);
            return prior ? { ...prior, host: h } : newCard(h, h.split("|")[1] ?? h);
          });
          // Remembered for the machines the run resolved, each its own.
          if (remember) api.credentialPut(saved, ev.hosts).catch((e) => app.flash(errText(e)));
          return;
        }
        if (ev.type === "finished") {
          finished = { done: ev.done, failed: ev.failed, cancelled: ev.cancelled };
          void app.refresh();
          return;
        }
        cards = applyEvent(cards, ev);
      });
    } catch (e) {
      error = errText(e);
      step = "form";
    }
  }

  async function pickBinary() {
    const f = await api.pickFile("Pick a meshnode binary");
    if (f) {
      binaryPath = f.token;
      binaryShown = f.path;
    }
  }

  /** Answer a machine's host-key question (S-M9). Same call the Add
   *  machines sheet makes; the only decision this flow ever has. */
  async function decide(host: string, accept: boolean) {
    if (runId === null) return;
    cards = cards.map((c) =>
      c.host === host ? { ...c, decided: true, stage: accept ? "connecting" : c.stage } : c,
    );
    try {
      await api.deployDecide(runId, host, accept);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  async function stop() {
    if (runId === null) return;
    try {
      await api.deployCancel(runId);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  function close() {
    if (step === "run" && !finished) return;
    onclose();
  }
</script>

{#if open}
  <div
    use:portal
    class="fixed inset-0 z-50 grid place-items-center p-4"
    style="background: rgba(12,16,24,.55); backdrop-filter: blur(3px);"
    role="presentation"
    transition:fadeIn={{ duration: DUR.base }}
    onclick={(e) => {
      if (e.target === e.currentTarget) close();
    }}
  >
    <div class="flex max-h-[92vh] w-[680px] max-w-[96vw] flex-col overflow-hidden rounded-2xl bg-surface" in:popIn={{ from: 0.94, duration: DUR.slow }} out:popIn={{ from: 0.96, duration: DUR.fast }} style="box-shadow: var(--sh-modal);" role="dialog" aria-modal="true" aria-label="Update over SSH" use:dialog={{ onescape: close }}>
      <div class="flex flex-none items-start gap-3 px-6 pt-5 pb-4" style="border-bottom: 1px solid var(--edge);">
        <span class="grid h-9 w-9 shrink-0 place-items-center rounded-lg" style="background: var(--accent-wash); color: var(--accent-ink);">
          <Icon name="refresh" size={18} />
        </span>
        <div class="min-w-0 flex-1">
          <h2 class="m-0 text-[17px] font-bold tracking-[-.02em] text-ink">Update over SSH</h2>
          <p class="mt-0.5 mb-0 text-sm leading-normal text-ink-4" style="text-wrap: pretty;">
            {#if step === "form"}
              Director pushes {binaryPath.trim() ? "your binary" : `meshnode ${version ?? "(newest)"}`} to each machine over SSH, restarts the service, and waits for the mesh to see it back. Nothing to install on them first.
            {:else if finished}
              {finished.done} updated{finished.failed ? `, ${finished.failed} did not` : ""}.
            {:else}
              {cards.filter((c) => c.stage === "done").length} of {cards.length} updated.
            {/if}
          </p>
        </div>
        <button class="iconbtn h-7 w-7 shrink-0" title="Close" disabled={step === "run" && !finished} onclick={close}>
          <Icon name="close" size={14} />
        </button>
      </div>

      <div class="min-h-0 flex-1 overflow-y-auto px-6 py-5">
        {#if error}
          <div class="errbox mb-4">{error}</div>
        {/if}
        {#if step === "form"}
          <div class="space-y-5">
            <!-- Who, as a sentence and a few chips. Fifty checkboxes is
                 not a summary; "44 nodes, 12 in rack-a, two architectures"
                 is, and it is the same information a person would read
                 aloud to a colleague before pressing the button. -->
            <section>
              <div class="mb-2 flex items-center justify-between gap-3">
                <span class="microlabel">Nodes</span>
                <button type="button" class="linkish text-xs" onclick={() => (hostsOpen = !hostsOpen)} aria-expanded={hostsOpen}>
                  {hostsOpen ? "Done" : "Edit selection, addresses or port"}
                </button>
              </div>
              <div class="panel">
                <div class="flex flex-wrap items-center gap-x-4 gap-y-1 px-4 py-3">
                  <span class="flex items-baseline gap-1.5">
                    <span class="num text-2xl font-bold tracking-[-.02em] text-ink">{chosen.length}</span>
                    <span class="text-sm text-ink-3">node{chosen.length === 1 ? "" : "s"}{chosen.length === nodes.length ? "" : ` of ${nodes.length}`}</span>
                  </span>
                  <span class="flex flex-wrap items-center gap-1.5">
                    {#each archSpread as [a, n] (a)}
                      <span class="pill mono">{a} <span class="num text-ink-5">{n}</span></span>
                    {/each}
                    {#each groups as [tag, n] (tag)}
                      {#if groups.length > 1}<span class="pill">{tag} <span class="num text-ink-5">{n}</span></span>{/if}
                    {/each}
                  </span>
                </div>
                <div class="flex flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2 text-xs text-ink-5" style="border-top: 1px solid var(--edge-row);">
                  <span>Addresses from the mesh</span>
                  <span>Port <span class="mono text-ink-3">{port}</span></span>
                  <span><span class="num text-ink-3">{parallel}</span> at a time</span>
                </div>

                {#if hostsOpen}
                  <div class="fade-in px-4 pt-3 pb-4" style="border-top: 1px solid var(--edge-row);">
                    <div class="mb-2 flex flex-wrap items-center gap-2">
                      <input
                        class="field field-sunken h-8 min-w-0 flex-1 px-2.5 text-xs outline-none"
                        bind:value={hostFilter}
                        placeholder="Filter by name or address"
                        spellcheck="false"
                        aria-label="Filter nodes"
                      />
                      <label class="flex items-center gap-2 text-xs text-ink-5">
                        <span>Port</span>
                        <input type="number" min="1" max="65535" bind:value={port}
                          class="field field-sunken mono h-8 w-20 px-2.5 text-xs outline-none"
                          class:is-missing={!portOk}
                          aria-label="SSH port for every node that does not name one" />
                      </label>
                      <label class="flex items-center gap-2 text-xs text-ink-5">
                        <span>At once</span>
                        <input type="number" min="1" max="32" bind:value={parallel}
                          class="field field-sunken mono h-8 w-16 px-2.5 text-xs outline-none"
                          aria-label="How many machines to work at once" />
                      </label>
                      <button type="button" class="linkish text-xs"
                        onclick={() => (picked = new Set(shownHosts.map((n) => n.nodeId)))}>All</button>
                      <button type="button" class="linkish text-xs"
                        onclick={() => (picked = new Set())}>None</button>
                    </div>
                    <!-- Capped and scrolled: the list is as long as the fleet,
                         the sheet is not. -->
                    <div class="dark-scroll max-h-64 overflow-y-auto overflow-hidden rounded-lg" style="background: var(--surface-1); box-shadow: 0 0 0 1px var(--edge-card);">
                      {#each shownHosts as n (n.nodeId)}
                        <div class="flex items-center gap-3 px-3 py-1.5 text-sm" style="border-bottom: 1px solid var(--edge-row);">
                          <input type="checkbox" class="check" checked={picked.has(n.nodeId)} onchange={() => toggle(n.nodeId)} aria-label={`Update ${n.name}`} />
                          <Dot kind={n.liveness} />
                          <span class="w-36 shrink-0 truncate font-medium text-ink-2">{n.name}</span>
                          <span class="mono w-14 shrink-0 text-xs text-ink-5">{n.version ?? "—"}</span>
                          <input
                            class="field field-sunken mono h-7 min-w-0 flex-1 px-2.5 text-xs outline-none"
                            bind:value={hosts[n.nodeId]}
                            placeholder="user@host"
                            spellcheck="false"
                            title="Where SSH reaches this node; a user here overrides the one below"
                          />
                        </div>
                      {:else}
                        <div class="px-3.5 py-3 text-sm text-ink-5">Nothing matches {hostFilter}.</div>
                      {/each}
                    </div>
                    <p class="mt-2 mb-0 text-xs leading-normal text-ink-5">
                      <span class="mono">host:2222</span> on a row overrides the port for that machine.
                      {#if !portOk}<span class="block" style="color: var(--status-error-ink);">A port is 1 to 65535.</span>{/if}
                    </p>
                  </div>
                {/if}
              </div>

              {#if missingHost.length}
                <div class="errbox mt-2 text-sm">
                  No address for {missingHost.map((n) => n.name).join(", ")}. Add one under <em>Edit</em>, or untick
                  {missingHost.length === 1 ? "it" : "them"}.
                </div>
              {:else if unreachable.length}
                <p class="mt-2 mb-0 text-xs leading-normal text-ink-5">
                  Left out: {unreachable.map((n) => n.name).join(", ")} — the mesh has no address for
                  {unreachable.length === 1 ? "it" : "them"}. Add one under <em>Edit</em> to include
                  {unreachable.length === 1 ? "it" : "them"}.
                </p>
              {/if}
            </section>

            <section>
              <div class="microlabel mb-2">Credentials</div>
              <CredentialsForm bind:auth bind:complete={authComplete} />
            </section>

            <!-- The two things a person changes once in a while, in one
                 group and one line each. The long explanations live in
                 the hints. -->
            <section>
              <div class="microlabel mb-2">Options</div>
              <div class="panel">
                <div class="flex flex-wrap items-center gap-3 px-4 py-3">
                  <span class="min-w-0 flex-1">
                    <span class="flex items-center gap-1 text-sm font-medium text-ink">
                      Binary
                      <Hint text="A version names bytes the mesh already holds. Point at a file when you want the ones it does not — a build you have not published, or one you never will. The architecture is read off the file name and refused if it does not match the machine. A package-managed install (apt, dnf) is refused with the command to run instead." />
                    </span>
                    {#if binaryPath.trim()}
                      <span class="mono block truncate text-xs text-ink-4" title={binaryShown}>{binaryShown}</span>
                    {:else}
                      <span class="block text-xs text-ink-5">meshnode <span class="mono">{version ?? "(newest)"}</span>, as this mesh holds it</span>
                    {/if}
                  </span>
                  {#if binaryPath.trim()}
                    <button type="button" class="linkish shrink-0 text-xs" onclick={() => { binaryPath = ""; binaryShown = ""; }}>Use the mesh's</button>
                  {/if}
                  <button type="button" class="btn btn-secondary h-8 shrink-0 rounded-sm px-3 text-xs" onclick={pickBinary}>
                    {binaryPath.trim() ? "Change file…" : "Use a file…"}
                  </button>
                </div>
                <label class="flex cursor-pointer items-center gap-3 px-4 py-3" style="border-top: 1px solid var(--edge-row);">
                  <span class="min-w-0 flex-1">
                    <span class="flex items-center gap-1 text-sm font-medium text-ink">
                      Reinstall where the version already matches
                      <Hint text="Without this, a node already reporting the target version is left alone. Turn it on when the version number is the thing you doubt — a binary replaced by hand, an install that half-finished, a rebuild that kept its number. The mesh cannot confirm a swap that changes no version, so those nodes finish on the machine's own word that the service restarted." />
                    </span>
                    <span class="block text-xs text-ink-5">Push the build to every ticked node, whatever it reports.</span>
                  </span>
                  <input type="checkbox" class="switch" bind:checked={force} />
                </label>
              </div>
            </section>
          </div>
        {:else}
          <RolloutProgress {cards} target={binaryPath.trim() ? null : version} {finished} onDecide={decide} />
        {/if}
      </div>

      <div class="flex flex-none items-center gap-2 px-6 py-4" style="border-top: 1px solid var(--edge); background: var(--surface-subtle);">
        {#if step === "form"}
          <span class="text-xs text-ink-5">Nothing changes on a machine until its signature checks out.</span>
          <button class="btn btn-secondary btn-lg ml-auto" onclick={close}>Cancel</button>
          <button class="btn btn-primary btn-lg" disabled={!ready} onclick={start}>{force ? "Reinstall on" : "Update"} {chosen.length} node{chosen.length === 1 ? "" : "s"}</button>
        {:else if finished}
          <button class="btn btn-primary btn-lg ml-auto" onclick={onclose}>Done</button>
        {:else}
          <span class="text-xs text-ink-5">Nodes mid-swap finish; the rest stop.</span>
          <button class="btn btn-secondary btn-lg ml-auto" onclick={stop}>Stop</button>
        {/if}
      </div>
    </div>
  </div>
{/if}

<style>
  /* A grouped surface inside the sheet: rows separated by hairlines. */
  .panel {
    overflow: hidden;
    border-radius: var(--radius-lg);
    background: var(--surface-subtle);
    box-shadow: 0 0 0 1px var(--edge-card);
  }
  .pill {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 2px 8px;
    border-radius: 999px;
    font-size: var(--text-2xs);
    font-weight: 500;
    color: var(--ink-3);
    background: var(--surface-2);
  }
</style>
