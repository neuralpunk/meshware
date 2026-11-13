<script lang="ts">
  // The toolbar — 56px above the document, on the canvas rather than on a
  // surface of its own. Four things, left to right:
  //
  //   back / forward · where you are · | · what you can do here
  //
  // The 6c pass changed two of them. The breadcrumb is the whole
  // wayfinding line now (`Mesh › Fleet`) because the panels moved into the
  // rail and there is no tab row under it to repeat the second word. And
  // the right-hand side carries the *page's* actions — `Export`, `Add
  // node` — pushed up by the view through `chrome.actions`, so a screen
  // never grows a second row of buttons under the first.
  //
  // Search left, for the rail. Identity and the account left before it.
  // What is behind: the bell, because "something is waiting on you" is
  // true wherever you are.

  import { app, type ViewName } from "../state.svelte";
  import { chrome } from "./chrome.svelte";
  import { subLabel, sectionOf } from "./nav";
  import Icon from "../components/Icon.svelte";
  import { popMenu, popIn, roll, slideX } from "../motion";
  import { modKey } from "../keys";
  import { api, errText } from "../api";
  import { diagnosticsText } from "../diagnostics";

  let { label, root }: { label: string; root: ViewName } = $props();

  let helpMenu = $state(false);
  /** Everything waiting on this person — the Access badge's number too. */
  const pending = $derived(app.waitingOnYou);
  const joining = $derived(app.joinQueueCount);
  /** A bell that can never ring is furniture: somebody who is not an admin
   *  cannot be asked to let anyone in, and there is nothing else to be
   *  asked. */
  const bellShown = $derived(app.isAdmin);

  /** One step of the breadcrumb. `go` is set on every step but the last. */
  type Crumb = { text: string; kind: "id" | "place"; go?: () => void };

  /**
   * The whole wayfinding line, as steps.
   *
   * A section of panels reads `Mesh › Fleet`; a section of views
   * (`SECTIONS`, 2026-09) reads `Services › Deployments`, and with an
   * item open inside the view, `Run › Scripts › deploy.sh` — the middle
   * step stays, because "which screen is this thing on" is the question
   * a breadcrumb exists to answer and the item's name does not say.
   */
  const trail = $derived.by((): Crumb[] => {
    const out: Crumb[] = [];
    const section = sectionOf(app.view);
    if (section) {
      const home = section.home;
      out.push({ text: section.label, kind: "place", go: () => app.go(home) });
      out.push({ text: label, kind: "place", go: () => app.go(root) });
    } else {
      out.push({ text: label, kind: "place", go: () => app.go(root) });
      const panel = subLabel(app.view, app.sub);
      if (panel) out.push({ text: panel, kind: "place" });
    }
    if (chrome.detail) out.push({ text: chrome.detail, kind: chrome.detailKind });
    // The last step is where you are, and is not a link.
    delete out[out.length - 1].go;
    return out;
  });

  /**
   * Where the bell goes (UX-6): to what is actually waiting — the join
   * queue, at the top of Access › People.
   */
  function openWaiting() {
    sessionStorage.setItem("access:tab", "people");
    app.go("access");
  }

  const bellTitle = $derived.by(() => {
    if (pending === 0) return "Nothing is waiting on you";
    const parts: string[] = [];
    if (joining > 0) parts.push(`${joining} waiting to join`);
    return parts.join(" · ");
  });

  // -- the help menu (UX-7) ------------------------------------------------
  //
  // It listed two keyboard shortcuts and nothing else, so "?" answered the
  // one question nobody asks it. The four things people do open a help menu
  // for come first — read the docs, ask a person, hand that person the
  // facts, find out which build this is — and the shortcuts follow.
  async function openHelp(url: string) {
    helpMenu = false;
    try {
      await api.openUrl(url);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  async function copyDiagnostics() {
    helpMenu = false;
    const role = app.whoami?.isFounder
      ? "founder"
      : (app.whoami?.roles[0] ?? (app.whoami?.registered ? "member" : "guest"));
    try {
      await api.copyText(
        diagnosticsText({ about: app.about, status: app.status, nodes: app.nodes, role }),
      );
      app.flash("Diagnostics copied — paste them into your message to support");
    } catch (e) {
      app.flash(errText(e));
    }
  }

  // The shortcuts. There was a second list for the legacy surface, and it
  // described the workflow editor — a screen that no longer exists on
  // either surface (DIR-14).
  //
  // Spelled for the keyboard in front of the person reading it
  // (`lib/keys.ts`): `Ctrl`. The hard-coded Mac glyph this replaced sent
  // every user to the Super key, which the window manager takes before the
  // application ever sees it — so the shortcut this panel exists to teach
  // was the one that appeared broken.
  const shortcuts: [string, string][] = [
    [modKey("K"), "Search or jump to anything"],
    ["Esc", "Close whatever is on top"],
  ];
</script>

<!-- Dismissal, the same three ways every popover in the chrome closes:
     a press anywhere outside it (pointerdown, which no button's click
     handler can swallow — the rail's menu buttons stop `click` so their
     own window listener does not fire, and that used to leave this one
     open beside them), Escape, and its own button. -->
<svelte:window
  onpointerdown={(e) => {
    if (helpMenu && !(e.target as Element | null)?.closest?.("[data-menu='help']")) helpMenu = false;
  }}
  onkeydown={(e) => {
    if (helpMenu && e.key === "Escape") {
      e.preventDefault();
      helpMenu = false;
    }
  }}
/>

<header
  class="chrome-focus relative z-30 flex h-14 flex-none items-center gap-2 px-3"
  style="background: transparent;"
>
  <button
    class="mast-btn h-7 w-7"
    title={chrome.railCollapsed ? "Show the navigation rail" : "Collapse the navigation rail"}
    aria-expanded={!chrome.railCollapsed}
    onclick={() => (chrome.railCollapsed = !chrome.railCollapsed)}
  >
    <Icon name="menu" size={16} />
  </button>

  <!-- Where you have been. Two chevrons, disabled until there is somewhere
       to go: a person who has walked Mesh › Topology → Mesh › Fleet →
       Activity should be able to walk back out the way they came in,
       and a breadcrumb alone cannot do that. -->
  <div class="ml-1 flex flex-none items-center gap-px">
    <button class="navchev" title="Back" disabled={!app.canBack} onclick={() => app.back()}>
      <Icon name="chevron-left" size={16} />
    </button>
    <button class="navchev" title="Forward" disabled={!app.canForward} onclick={() => app.forward()}>
      <Icon name="chevron-right" size={16} />
    </button>
  </div>

  <nav class="crumb ml-2" aria-label="Breadcrumb">
    {#each trail as step, i (i)}
      {#if i > 0}
        <span class="crumb-sep" in:slideX={{ x: -6 }}><Icon name="chevron-right" size={13} /></span>
      {/if}
      {#if step.go}
        <button class="crumb-parent" onclick={step.go}>{step.text}</button>
      {:else}
        <span
          class={`crumb-current truncate ${step.kind === "id" ? "mono text-[13px]" : ""}`}
          in:slideX={{ x: i > 0 ? -8 : 0 }}
          use:roll={step.text}>{step.text}</span
        >
      {/if}
    {/each}
  </nav>

  <div class="ml-auto flex flex-none items-center gap-2">
    {#if chrome.chips}
      {#each chrome.chips as chip}
        <span class="chip">{chip}</span>
      {/each}
    {/if}

    {#if bellShown}
      <button
        class="mast-btn relative h-8 w-8"
        style="background: var(--ctl-bg); box-shadow: var(--sh-control);"
        title={bellTitle}
        aria-label={pending > 0 ? `${bellTitle} — open Access` : bellTitle}
        onclick={openWaiting}
      >
        <Icon name="bell" size={15} />
        {#if pending > 0}
          <span
            class="absolute top-[5px] right-[5px] h-[6px] w-[6px] rounded-full"
            style="background: var(--status-warn); box-shadow: 0 0 0 2px var(--bg-canvas);"
            transition:popIn={{ from: 0.3 }}
          ></span>
        {/if}
      </button>
    {/if}

    <div class="relative" data-menu="help">
      <button
        class="mast-btn h-8 w-8"
        style="background: var(--ctl-bg); box-shadow: var(--sh-control);"
        title="Help"
        aria-label="Help"
        aria-haspopup="menu"
        aria-expanded={helpMenu}
        onclick={(e) => {
          e.stopPropagation();
          helpMenu = !helpMenu;
        }}
      >
        <Icon name="help" size={15} />
      </button>
      {#if helpMenu}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <div
          class="absolute top-[38px] right-0 z-40 w-[288px] overflow-hidden rounded-lg bg-surface pb-3"
          style="box-shadow: var(--sh-pop); transform-origin: calc(100% - 14px) 0;"
          role="menu"
          tabindex="-1"
          use:popMenu
          onclick={(e) => e.stopPropagation()}
        >
          <div class="py-1.5" style="border-bottom: 1px solid var(--edge);">
            <button class="help-item" role="menuitem" disabled={!app.about} onclick={() => app.about && openHelp(app.about.docsUrl)}>
              <Icon name="library" size={14} /> Documentation
            </button>
            <button
              class="help-item"
              role="menuitem"
              disabled={!app.about}
              onclick={() =>
                app.about &&
                openHelp(app.about.issuesUrl)}
            >
              <Icon name="inbox" size={14} /> Report a problem
            </button>
            <button class="help-item" role="menuitem" onclick={copyDiagnostics}>
              <Icon name="copy" size={14} /> Copy diagnostics
            </button>
            <button
              class="help-item"
              role="menuitem"
              onclick={() => {
                helpMenu = false;
                app.go("settings", "about");
              }}
            >
              <Icon name="info" size={14} /> About Meshware Director
              {#if app.about}<span class="mono ml-auto text-[10.5px] text-ink-muted">{app.about.version}</span>{/if}
            </button>
          </div>
          <div class="microlabel px-3 pt-3">Shortcuts</div>
          <div class="mt-[9px] flex flex-col gap-[7px] px-3">
            {#each shortcuts as [key, what] (key)}
              <div class="flex items-baseline gap-[10px]">
                <span class="mono w-[58px] flex-none text-[10.5px] text-accent-ink" style="user-select: none;">{key}</span>
                <span class="text-[11.5px] leading-[1.4] text-ink-3">{what}</span>
              </div>
            {/each}
          </div>
        </div>
      {/if}
    </div>

    <!-- The page's own actions. Rendered here rather than in the page so a
         screen's key action is always in the same place, at the same size,
         whichever screen it is. -->
    {#if chrome.actions}
      {@render chrome.actions()}
    {/if}
  </div>
</header>

<style>
  .help-item {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 100%;
    padding: 7px 12px;
    text-align: left;
    font-size: 12.5px;
    color: var(--ink-2);
    cursor: pointer;
    transition: background-color var(--dur-fast) var(--ease);
  }
  .help-item:hover:not(:disabled) {
    background: var(--surface-2);
  }
  .help-item:disabled {
    color: var(--ink-disabled);
    cursor: default;
  }
  .help-item :global(svg) {
    color: var(--ink-faint);
  }
</style>
