<script lang="ts">
  // Add machines.
  //
  // The whole pitch of the product in one sheet: name some machines, give
  // Director a way in, and watch them become nodes. Three screens — the
  // form, a review that says exactly what will happen, and the live cards
  // — and the one thing that is never automated: the pairing code. Both
  // codes are shown, Director says whether they agree, and the person's
  // click is what joins the machine. **Confirm all** is that click for
  // every card whose codes agree; it cannot confirm a card whose codes
  // do not.
  import { dialog } from "../dialog";
  import { api, errText, type DeployDraft, type DeployPreviewView, type DeployEvent } from "../api";
  import { app } from "../state.svelte";
  import { rememberSshPort, rememberedSshPort } from "../sshport";
  import CredentialsForm from "./CredentialsForm.svelte";
  import DeployCards, { type Card, applyEvent, awaiting, newCard } from "./DeployCards.svelte";
  import Fold from "./Fold.svelte";
  import Hint from "./Hint.svelte";
  import Icon from "./Icon.svelte";
  import { portal } from "../portal";
  import { popIn, fadeIn, DUR } from "../motion";

  let {
    open = false,
    onclose,
    initialTargets = "",
  }: { open?: boolean; onclose: () => void; initialTargets?: string } = $props();

  type Step = "form" | "review" | "run";
  let step = $state<Step>("form");
  let targets = $state("");
  let tags = $state("");
  let draft = $state<DeployDraft>(blankDraft());
  let preview = $state<DeployPreviewView | null>(null);
  let previewing = $state(false);
  let error = $state<string | null>(null);
  let cards = $state<Card[]>([]);
  let runId = $state<number | null>(null);
  let finished = $state<{ done: number; failed: number; cancelled: boolean } | null>(null);

  function blankDraft(): DeployDraft {
    return {
      targets: [],
      auth: { user: "", method: "password", password: null, keyPath: null, passphrase: null, sudoPassword: null, remember: false, fromKeychain: false },
      tags: [],
      installPath: null,
      binaryPath: null,
      fleetUpgrade: true,
      openFirewall: true,
      parallel: 4,
      replaceExisting: false,
      port: rememberedSshPort(),
    };
  }

  // Fresh every time it opens: a password must not survive a closed sheet.
  $effect(() => {
    if (open) {
      step = "form";
      targets = initialTargets;
      tags = "";
      draft = blankDraft();
      preview = null;
      error = null;
      cards = [];
      runId = null;
      finished = null;
    }
  });

  const targetLines = $derived(targets.split(/\n/).map((l) => l.trim()).filter(Boolean));
  const hostCount = $derived(preview?.hosts.length ?? 0);
  /** A port, or 22 — the engine takes the same view of a nonsense one. */
  const portOf = (p: number) => Math.max(1, Math.min(65535, Math.trunc(Number(p) || 22)));
  const portOk = $derived(Number.isInteger(Number(draft.port)) && Number(draft.port) >= 1 && Number(draft.port) <= 65535);

  // A live count under the box, debounced: the preview also validates.
  let previewTimer: ReturnType<typeof setTimeout> | null = null;
  $effect(() => {
    const lines = targetLines;
    const bin = draft.binaryPath;
    const p = draft.port;
    if (!open) return;
    if (previewTimer) clearTimeout(previewTimer);
    previewTimer = setTimeout(() => void loadPreview(lines, bin, p), 250);
  });

  async function loadPreview(lines: string[], bin: string | null, p = draft.port) {
    if (lines.length === 0) {
      preview = null;
      return;
    }
    previewing = true;
    try {
      preview = await api.deployPreview(lines, bin, portOf(p));
    } catch (e) {
      error = errText(e);
    } finally {
      previewing = false;
    }
  }

  /** The form's word on whether the login is filled in — the keychain
   *  counts, which the draft alone cannot show (S3). */
  let authComplete = $state(false);
  const canContinue = $derived(
    hostCount > 0 &&
      portOk &&
      !previewing &&
      (preview?.problems.length ?? 0) === 0 &&
      authComplete,
  );

  async function toReview() {
    error = null;
    await loadPreview(targetLines, draft.binaryPath, draft.port);
    if ((preview?.problems.length ?? 0) > 0) return;
    step = "review";
  }

  // `draft.binaryPath` carries the pick's token; this is what to show
  // (SEC-14). There is no typing a path in.
  let binaryShown = $state<string | null>(null);
  async function pickBinary() {
    const f = await api.pickFile("Pick a meshnode binary");
    if (f) {
      draft.binaryPath = f.token;
      binaryShown = f.path;
    }
  }
  function clearBinary() {
    draft.binaryPath = null;
    binaryShown = null;
  }

  async function start() {
    error = null;
    const d: DeployDraft = {
      ...draft,
      targets: targetLines,
      tags: tags.split(",").map((t) => t.trim()).filter(Boolean),
      installPath: draft.installPath?.trim() || null,
      binaryPath: draft.binaryPath?.trim() || null,
      parallel: Math.max(1, Math.min(32, Number(draft.parallel) || 4)),
      port: portOf(draft.port),
    };
    rememberSshPort(d.port);
    cards = (preview?.hosts ?? targetLines).map((h) => newCard(h, h));
    step = "run";
    // The keychain, as the deploy starts: the person committed to these
    // values by pressing the button, and a failed deploy is not a reason
    // to forget a correct password. Saved for the machines the run names,
    // each with its own record.
    const remember = d.auth.remember && d.auth.user.trim() !== "";
    try {
      runId = await api.deployStart(d, (ev: DeployEvent) => {
        if (ev.type === "plan") {
          runId = ev.runId;
          if (remember) api.credentialPut(d.auth, ev.hosts).catch((e) => app.flash(errText(e)));
        }
        if (ev.type === "finished") {
          finished = { done: ev.done, failed: ev.failed, cancelled: ev.cancelled };
          void app.refresh();
          if (ev.done > 0) app.flash(`${ev.done} machine${ev.done === 1 ? "" : "s"} joined ${app.status?.meshName ?? "the mesh"}`);
          return;
        }
        cards = applyEvent(cards, ev);
      });
    } catch (e) {
      error = errText(e);
      step = "review";
    }
  }

  async function decide(host: string, accept: boolean) {
    if (runId === null) return;
    cards = cards.map((c) =>
      c.host === host
        ? {
            ...c,
            decided: true,
            // A host key accepted (S-M9) puts the machine back where it
            // was — nothing has happened on it yet. A pairing code
            // accepted is what joins it.
            stage: accept ? (c.keyPrompt && !c.code ? "connecting" : "joining") : c.stage,
          }
        : c,
    );
    try {
      await api.deployDecide(runId, host, accept);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  async function confirmAll() {
    for (const c of awaiting(cards)) await decide(c.host, true);
  }

  async function stop() {
    if (runId === null) return;
    try {
      await api.deployCancel(runId);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  const pending = $derived(awaiting(cards).length);
  const joined = $derived(cards.filter((c) => c.stage === "done").length);
  const failed = $derived(cards.filter((c) => c.stage === "failed").length);
  const progress = $derived(cards.length ? (joined + failed) / cards.length : 0);

  function close() {
    if (step === "run" && !finished) return;
    onclose();
  }

  const SOURCE_LABEL: Record<string, string> = {
    file: "the file you picked",
    mesh: "this mesh",
    bundled: "bundled with Director",
    local: "installed beside Director",
    channel: "the release channel",
    none: "nowhere yet",
  };
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
    <div
      class="flex max-h-[92vh] w-[720px] max-w-[96vw] flex-col overflow-hidden rounded-2xl bg-surface"
      in:popIn={{ from: 0.94, duration: DUR.slow }}
      out:popIn={{ from: 0.96, duration: DUR.fast }}
      style="box-shadow: var(--sh-modal);"
      role="dialog"
      aria-modal="true"
      aria-label="Add machines"
      use:dialog={{ onescape: close }}
    >
      <!-- header -->
      <div class="flex flex-none items-start gap-3 px-6 pt-5 pb-4" style="border-bottom: 1px solid var(--edge);">
        <span class="grid h-9 w-9 shrink-0 place-items-center rounded-lg" style="background: var(--accent-wash); color: var(--accent-ink);">
          <Icon name="nodes" size={18} />
        </span>
        <div class="min-w-0 flex-1">
          <h2 class="m-0 text-[17px] font-bold tracking-[-.02em] text-ink">
            {#if step === "run"}
              {finished ? "Done" : "Adding machines"}
            {:else}
              Add machines
            {/if}
          </h2>
          <p class="mt-0.5 mb-0 text-sm leading-normal text-ink-4" style="text-wrap: pretty;">
            {#if step === "form"}
              Name the machines and a way in. Director installs meshnode, sets up the service, pairs — you confirm each code — and starts it.
            {:else if step === "review"}
              Here is exactly what will happen. Nothing has been sent yet.
            {:else if finished}
              {finished.done} joined{finished.failed ? `, ${finished.failed} did not` : ""}{finished.cancelled ? " — stopped early" : ""}.
            {:else}
              {joined} of {cards.length} joined{pending ? ` · ${pending} waiting for you` : ""}.
            {/if}
          </p>
        </div>
        <button class="iconbtn h-7 w-7 shrink-0" title="Close" disabled={step === "run" && !finished} onclick={close}>
          <Icon name="close" size={14} />
        </button>
      </div>

      <!-- progress bar under the header while running -->
      {#if step === "run"}
        <div class="h-[3px] flex-none bg-surface-2">
          <div class="h-full transition-[width] duration-500" style={`width: ${Math.round(progress * 100)}%; background: ${finished && failed === 0 ? "var(--status-ok)" : "var(--accent)"};`}></div>
        </div>
      {/if}

      <!-- body -->
      <div class="min-h-0 flex-1 overflow-y-auto px-6 py-5">
        {#if error}
          <div class="errbox mb-4">{error}</div>
        {/if}

        {#if step === "form"}
          <div class="space-y-5">
            <label class="block">
              <span class="mb-1.5 flex items-center gap-1 text-sm text-ink-4">
                Machines
                <Hint text="One per line, or separated by spaces. user@host, host:2222, IPv6 in brackets, and ranges: ops@rack-&#123;1..10&#125;, &#123;web,db&#125;.local, pi@shelf-&#123;01..04&#125;. Lines starting with # are ignored." />
              </span>
              <textarea
                bind:value={targets}
                rows="4"
                spellcheck="false"
                autocapitalize="off"
                placeholder={"ops@rack-1\nops@rack-{2..8}\n10.0.0.5"}
                class="field field-sunken mono w-full resize-y px-3 py-2.5 text-sm leading-relaxed outline-none placeholder:font-sans"
              ></textarea>
              <span class="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1.5 text-xs text-ink-5">
                <span class="flex items-center gap-2">
                  <span>SSH port</span>
                  <input
                    type="number"
                    min="1"
                    max="65535"
                    bind:value={draft.port}
                    class="field field-sunken mono h-7 w-20 px-2 text-xs outline-none"
                    aria-label="SSH port for every machine that does not name one"
                    title="Applies to every line that does not name its own port — write host:2222 on a line to override it there."
                  />
                </span>
                {#if !portOk}
                  <span style="color: var(--status-error-ink);">A port is 1 to 65535.</span>
                {:else if preview?.problems.length}
                  <span style="color: var(--status-error-ink);">{preview.problems[0]}</span>
                {:else if hostCount > 0}
                  <span><span class="num font-semibold text-ink-2">{hostCount}</span> machine{hostCount === 1 ? "" : "s"}</span>
                {:else}
                  <span>Ranges expand: <span class="mono">rack-{"{1..10}"}</span> is ten machines.</span>
                {/if}
              </span>
            </label>

            <div>
              <div class="microlabel mb-2">Credentials</div>
              <CredentialsForm bind:auth={draft.auth} bind:complete={authComplete} />
            </div>

            <label class="block">
              <span class="mb-1.5 block text-sm text-ink-4">Tags <span class="text-ink-muted">(optional, comma-separated)</span></span>
              <input bind:value={tags} class="field field-sunken mono h-10 w-full px-3 text-sm outline-none placeholder:font-sans" placeholder="gpu, dc=east" spellcheck="false" />
            </label>

            <Fold title="Options" summary={`${draft.fleetUpgrade ? "self-updating" : "SSH updates only"} · ${draft.openFirewall ? "firewall opened" : "firewall untouched"} · ${draft.parallel} at a time`}>
              <label class="flex cursor-pointer items-center justify-between gap-4 rounded-md px-3.5 py-3" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
                <span>
                  <span class="block text-sm font-medium text-ink">Let nodes update themselves</span>
                  <span class="mt-0.5 block text-xs leading-normal text-ink-5">Writes the one systemd drop-in that lets a node replace its own binary, so a rollout from Updates reaches it with no visit. Off means every upgrade of it goes over SSH.</span>
                </span>
                <input type="checkbox" class="switch" bind:checked={draft.fleetUpgrade} />
              </label>
              <label class="flex cursor-pointer items-center justify-between gap-4 rounded-md px-3.5 py-3" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
                <span>
                  <span class="block text-sm font-medium text-ink">Open the mesh port in the firewall</span>
                  <span class="mt-0.5 block text-xs leading-normal text-ink-5">UDP 7777 in firewalld or ufw, where one is running. A machine with no firewall is left alone.</span>
                </span>
                <input type="checkbox" class="switch" bind:checked={draft.openFirewall} />
              </label>
              <label class="flex cursor-pointer items-center justify-between gap-4 rounded-md px-3.5 py-3" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
                <span>
                  <span class="block text-sm font-medium text-ink">Replace an existing install</span>
                  <span class="mt-0.5 block text-xs leading-normal text-ink-5">A machine that already belongs to a mesh is normally refused. This moves its old identity aside (never deleted) and pairs it fresh.</span>
                </span>
                <input type="checkbox" class="switch" bind:checked={draft.replaceExisting} />
              </label>
              <div class="grid gap-3 sm:grid-cols-[1fr_auto]">
                <label class="block">
                  <span class="mb-1.5 block text-sm text-ink-4">Install path <span class="text-ink-muted">(blank for the default)</span></span>
                  <input bind:value={draft.installPath} class="field field-sunken mono h-9 w-full px-3 text-sm outline-none placeholder:font-sans" placeholder="/usr/local/bin/meshnode" spellcheck="false" />
                </label>
                <label class="block">
                  <span class="mb-1.5 block text-sm text-ink-4">At a time</span>
                  <input type="number" min="1" max="32" bind:value={draft.parallel} class="field field-sunken num h-9 w-24 px-3 text-sm outline-none" />
                </label>
              </div>
              <label class="block">
                <span class="mb-1.5 block text-sm text-ink-4">Binary <span class="text-ink-muted">(normally found for you)</span></span>
                <div class="flex gap-2">
                  <input value={binaryShown ?? ""} readonly title={binaryShown ?? ""} class="field field-sunken mono h-9 min-w-0 flex-1 px-3 text-sm outline-none placeholder:font-sans" placeholder="a release binary, when you want a specific one" spellcheck="false" />
                  {#if binaryShown}
                    <button type="button" class="linkish shrink-0 text-xs" onclick={clearBinary}>Clear</button>
                  {/if}
                  <button type="button" class="btn btn-secondary h-9 shrink-0 px-3 text-sm" onclick={pickBinary}>Browse…</button>
                </div>
              </label>
            </Fold>
          </div>
        {:else if step === "review"}
          <div class="space-y-4">
            <div>
              <div class="microlabel mb-2">{hostCount} machine{hostCount === 1 ? "" : "s"}</div>
              <div class="flex flex-wrap gap-1.5">
                {#each preview?.hosts ?? [] as h}
                  <span class="mono rounded-full bg-surface-2 px-2.5 py-1 text-xs font-medium text-ink-2">{h}</span>
                {/each}
              </div>
            </div>

            <div>
              <div class="microlabel mb-2">What will happen on each</div>
              <ol class="m-0 list-none space-y-2 p-0 text-sm leading-normal text-ink-2">
                {#each [
                  ["Log in", `as ${draft.auth.user || "the user in each target"} with ${draft.auth.method === "password" ? "the password" : draft.auth.method === "key" ? "the key file" : "ssh-agent"}. A machine seen for the first time has its host key recorded; a machine whose key changed is refused.`],
                  ["Install meshnode", "sent over the same connection, run once to prove it is the right architecture, then moved into place."],
                  ["Set up the service", `a meshnode service account, the packaged systemd unit${draft.fleetUpgrade ? ", the self-update drop-in" : ""}${draft.openFirewall ? ", and UDP 7777 opened in the firewall" : ""}. Without root, a user service inside that account's home instead.`],
                  ["Pair", "both codes come back here. Director compares them and you confirm — one click per machine, or Confirm all."],
                  ["Start and watch", "the service starts, and the card turns green when the mesh sees the node alive."],
                ] as [title, body], i (i)}
                  <li class="flex gap-3">
                    <span class="mono mt-0.5 grid h-5 w-5 shrink-0 place-items-center rounded-full bg-surface-2 text-2xs font-semibold text-ink-4">{i + 1}</span>
                    <span><span class="font-semibold text-ink">{title}</span> — {body}</span>
                  </li>
                {/each}
              </ol>
            </div>

            <div>
              <div class="microlabel mb-2">The build</div>
              <div class="overflow-hidden rounded-lg" style="box-shadow: 0 0 0 1px var(--edge-card);">
                {#each preview?.sources ?? [] as s (s.arch)}
                  <div class="flex items-baseline gap-3 px-3.5 py-2.5 text-sm" style="border-bottom: 1px solid var(--edge-row);">
                    <span class="mono w-16 shrink-0 font-semibold text-ink-2">{s.arch}</span>
                    {#if s.kind === "none"}
                      <span class="min-w-0 flex-1 text-xs leading-normal" style="color: var(--status-warn-ink);">{s.detail}</span>
                    {:else}
                      <span class="mono shrink-0 text-ink">{s.version}</span>
                      <span class="min-w-0 flex-1 truncate text-xs text-ink-5">from {SOURCE_LABEL[s.kind] ?? s.kind}{s.kind === "channel" ? " · downloaded and verified when a machine needs it" : ""}</span>
                    {/if}
                  </div>
                {/each}
              </div>
              <p class="mt-2 mb-0 text-xs leading-normal text-ink-5">
                Each machine gets the build for its own architecture. Host keys are recorded in <span class="mono">{preview?.knownHosts}</span>.
              </p>
            </div>
          </div>
        {:else}
          <div class="space-y-3">
            {#if pending > 0}
              <div class="flex items-center gap-3 rounded-lg px-3.5 py-2.5 text-sm" style="background: var(--status-warn-wash); color: var(--status-warn-ink);">
                <span class="h-2 w-2 shrink-0 rounded-full" style="background: var(--status-warn); animation: mw-breathe var(--motion-breathe);"></span>
                <span class="min-w-0 flex-1">{pending} machine{pending === 1 ? " is" : "s are"} waiting for you to confirm a code.</span>
                <button class="btn btn-primary h-8 shrink-0 px-3 text-sm" onclick={confirmAll}>
                  Confirm all {pending > 1 ? `(${pending})` : ""}
                </button>
              </div>
            {/if}
            <DeployCards {cards} onDecide={decide} />
          </div>
        {/if}
      </div>

      <!-- footer -->
      <div class="flex flex-none items-center gap-2 px-6 py-4" style="border-top: 1px solid var(--edge); background: var(--surface-subtle);">
        {#if step === "form"}
          <span class="text-xs text-ink-5">Nothing is sent until you review it.</span>
          <button class="btn btn-secondary btn-lg ml-auto" onclick={close}>Cancel</button>
          <button class="btn btn-primary btn-lg" disabled={!canContinue} onclick={toReview}>
            Continue <Icon name="chevron-right" size={14} />
          </button>
        {:else if step === "review"}
          <button class="btn btn-ghost btn-lg" onclick={() => (step = "form")}>
            <Icon name="chevron-left" size={14} /> Back
          </button>
          <button class="btn btn-primary btn-lg ml-auto" disabled={(preview?.sources ?? []).every((s) => s.kind === "none")} onclick={start}>
            Add {hostCount} machine{hostCount === 1 ? "" : "s"}
          </button>
        {:else if finished}
          <span class="text-xs text-ink-5">
            {#if finished.failed > 0}
              The ones that failed say why on their card — fix it and add them again.
            {:else}
              They are in the mesh. Nothing further is needed on those machines — SSH was for this once.
            {/if}
          </span>
          <button class="btn btn-primary btn-lg ml-auto" onclick={onclose}>Done</button>
        {:else}
          <span class="text-xs text-ink-5">Machines already mid-install finish their step; the rest stop.</span>
          <button class="btn btn-secondary btn-lg ml-auto" onclick={stop}>Stop</button>
        {/if}
      </div>
    </div>
  </div>
{/if}
