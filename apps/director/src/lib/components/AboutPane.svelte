<script lang="ts">
  // Settings › About & licence (UX-7).
  //
  // Which build is this, what terms does it run under, and where do I get
  // help. Everything here comes from the build (`about.rs`), the engine's
  // own status, and the fleet the rest of the app already reads; nothing on
  // this pane is typed in as a literal. Meshware is open source with no
  // plans or limits, so there is nothing to count here.
  import { api, errText } from "../api";
  import { app } from "../state.svelte";
  import { diagnosticsText } from "../diagnostics";
  import Icon from "./Icon.svelte";

  const about = $derived(app.about);
  let licenceOpen = $state(false);

  const role = $derived(
    app.whoami?.isFounder ? "founder" : (app.whoami?.roles[0] ?? (app.whoami?.registered ? "member" : "guest")),
  );

  async function open(url: string) {
    try {
      await api.openUrl(url);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  async function copyDiagnostics() {
    try {
      await api.copyText(
        diagnosticsText({ about, status: app.status, nodes: app.nodes, role }),
      );
      app.flash("Diagnostics copied — paste them into your bug report");
    } catch (e) {
      app.flash(errText(e));
    }
  }

</script>

{#if !about}
  <div class="card p-[22px] text-sm text-ink-4">
    This build did not say what it is. That only happens in a development shell with no Tauri
    runtime behind it.
  </div>
{:else}
  <!-- ─────────── this build ─────────── -->
  <div class="card p-[22px]">
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">This build</h2>
    <dl class="about-grid mt-4">
      <dt>Director</dt>
      <dd class="mono">{about.version}{#if about.commit}<span class="text-ink-5">{` · ${about.commit}`}</span>{/if}</dd>
      <dt>Engine</dt>
      <dd class="mono">
        {#if app.status}{app.status.version}<span class="text-ink-5">{` · protocol ${app.status.proto}`}</span>{:else}<span class="text-ink-5">not running</span>{/if}
      </dd>
      <dt>Platform</dt>
      <dd class="mono">{about.os} / {about.arch}</dd>
    </dl>
    <div class="mt-4 flex flex-wrap gap-2">
      <button class="btn btn-secondary btn-md" onclick={copyDiagnostics}>
        <Icon name="copy" size={13} /> Copy diagnostics
      </button>
      <button class="btn btn-secondary btn-md" onclick={() => app.go("mesh", "updates")}>
        What the fleet runs
      </button>
    </div>
  </div>

  <!-- ─────────── licence ─────────── -->
  <div class="card p-[22px]">
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Licence</h2>
    <p class="mt-1.5 mb-0 text-sm leading-normal text-ink-3">{about.licenseName}</p>
    <p class="mono mt-1 mb-0 text-xs text-ink-5">{about.licenseId}</p>
    <p class="mt-3 mb-0 text-sm leading-normal text-ink-4">
      Free for any use — personal, commercial, at any scale — with no plans, tiers or machine
      limits. Use, change and share it under either licence.
    </p>
    <button
      class="linkish mt-3 text-sm"
      aria-expanded={licenceOpen}
      onclick={() => (licenceOpen = !licenceOpen)}
    >
      {licenceOpen ? "Hide the licences" : "Read the licences"}
    </button>
    {#if licenceOpen}
      <pre class="licence selectable mt-3">{about.licenseText}</pre>
    {/if}
  </div>

  <!-- ─────────── help ─────────── -->
  <div class="card p-[22px]">
    <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Help</h2>
    <dl class="about-grid mt-4">
      <dt>Documentation</dt>
      <dd><button class="linkish" onclick={() => open(about.docsUrl)}>{about.docsUrl}</button></dd>
      <dt>Report a problem</dt>
      <dd><button class="linkish" onclick={() => open(about.issuesUrl)}>{about.issuesUrl}</button></dd>
      <dt>Source</dt>
      <dd><button class="linkish" onclick={() => open(about.sourceUrl)}>{about.sourceUrl}</button></dd>
    </dl>
    <p class="mt-3 mb-0 text-xs text-ink-5">
      Opening an issue? Copy diagnostics first and paste them in — they say which build and
      platform this is, and the shape of the mesh, without any machine names or addresses.
    </p>
  </div>
{/if}

<style>
  .about-grid {
    display: grid;
    grid-template-columns: 140px minmax(0, 1fr);
    gap: 8px 16px;
    margin-bottom: 0;
    font-size: var(--text-sm);
  }
  .about-grid dt {
    color: var(--ink-5);
  }
  .about-grid dd {
    margin: 0;
    min-width: 0;
    color: var(--ink-2);
    overflow-wrap: anywhere;
  }
  .licence {
    max-height: 360px;
    margin-bottom: 0;
    padding: 12px 14px;
    overflow: auto;
    border: 1px solid var(--edge-pre);
    border-radius: var(--radius-md);
    background: var(--surface-inline);
    color: var(--ink-2);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    line-height: 1.6;
    white-space: pre-wrap;
  }
</style>
