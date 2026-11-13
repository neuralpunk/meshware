<script lang="ts">
  import { app, type ViewName } from "./lib/state.svelte";
  import Spinner from "./lib/components/Spinner.svelte";
  // **Wizard and Mesh are eager; every other view is fetched on first
  // visit** (H-12). These two are the cold-start path — the wizard is what
  // a new install shows, Mesh is the landing tab — so making them wait on a
  // second network-of-one round trip would trade a real 1.75 MB parse for
  // an artificial delay on the one screen that must be instant.
  import Wizard from "./views/Wizard.svelte";
  import UnlockGate from "./lib/components/UnlockGate.svelte";
  import KeyGate from "./lib/components/KeyGate.svelte";
  import Mesh from "./views/Mesh.svelte";
  import Lazy from "./lib/components/Lazy.svelte";
  import { installStillness } from "./lib/stillness";
  import Palette from "./lib/components/Palette.svelte";
  import Header from "./lib/chrome/Header.svelte";
  import Rail from "./lib/chrome/Rail.svelte";
  import EnrolBanner from "./lib/components/EnrolBanner.svelte";
  import DemoBanner from "./lib/components/DemoBanner.svelte";
  import CrashCard from "./lib/components/CrashCard.svelte";
  import { describeError, reportCrash, startHint } from "./lib/crashlog";
  import { reveal, riseIn, slideX, fadeIn, installPressFeedback, DUR } from "./lib/motion";
  import { VIEW_HUE, hueStyle } from "./lib/hues";
  import { isChord } from "./lib/keys";
  import { CORE_NAV } from "./lib/chrome/nav";
  import { untrack, type Component } from "svelte";

  let paletteOpen = $state(false);

  /**
   * The view loaders (H-12).
   *
   * Vite turns each `import()` here into its own chunk, so the entry
   * bundle carries the shell and nothing else. `@xterm/xterm` comes with
   * the views that use it — see `vite.config.ts`, which also splits it out
   * so two views sharing it do not each carry a copy.
   */
  // `nodes` and `compute` are panels of Mesh, and `go()` sends them there;
  // they stay view names only so that old deep links still land.
  type Routed = Exclude<ViewName, "mesh" | "nodes" | "compute">;
  const VIEWS: Record<Routed, () => Promise<{ default: Component }>> = {
    run: () => import("./views/Run.svelte"),
    jobs: () => import("./views/Jobs.svelte"),
    deployments: () => import("./views/Deployments.svelte"),
    schedules: () => import("./views/Schedules.svelte"),
    registry: () => import("./views/Registry.svelte"),
    library: () => import("./views/Library.svelte"),
    secrets: () => import("./views/Secrets.svelte"),
    access: () => import("./views/Access.svelte"),
    settings: () => import("./views/Settings.svelte"),
  };
  const viewLoader = $derived(
    app.view === "mesh"
      ? null
      : (VIEWS[app.view as Routed] ?? VIEWS.settings),
  );

  // Every control in the application acknowledges a press — one delegated
  // listener, not four hundred actions.
  $effect(() => installPressFeedback());

  // Ambient motion stops when the window is hidden or a modal is over the
  // content (H-13, M-27, M-28). One listener, not fifteen components.
  $effect(() => installStillness());

  const nav = CORE_NAV;

  /** Screens that lay out their own bands, edge to edge, and own their
   *  scrolling. Everything else still gets the standard page header and a
   *  padded scroll region. */
  const FULL_BLEED: ReadonlySet<ViewName> = new Set(["mesh"]);

  const current = $derived(nav.find((n) => n.id === app.view) ?? nav[0]);
  const fullBleed = $derived(FULL_BLEED.has(app.view));

  // Which way the page came from. Sections are an ordered list — the rail
  // draws them top to bottom and the digits follow — so moving from Mesh
  // to Jobs is a move *down* the list and the new page rises to meet it;
  // coming back, it settles from above. Same distance, opposite sign: the
  // one bit of information a fade cannot carry.
  let lastIdx = -1;
  let dir = $state(1);
  $effect.pre(() => {
    const idx = nav.findIndex((n) => n.id === app.view);
    untrack(() => {
      dir = lastIdx < 0 || idx >= lastIdx ? 1 : -1;
      lastIdx = idx;
    });
  });

  // Once, and untracked. `init` reads app state before its first await,
  // so a tracked call re-ran the moment `init` wrote that state back (the
  // About fetch did, from UX-7): two `start_engine`s raced for the op log,
  // and the loser's "Database already open" became the fatal screen over
  // an engine that had started fine.
  $effect(() => {
    untrack(() => void app.init());
  });

  function onKey(e: KeyboardEvent) {
    // ⌘K / Ctrl+K, matched on the **physical** key (`lib/keys.ts`). Testing
    // `e.key === "k"` is the spelling that works for whoever wrote it and
    // silently does nothing on Dvorak, AZERTY, or any non-Latin layout —
    // and this is the one shortcut a new user tries before reading
    // anything, so losing it loses the palette entirely.
    if (isChord(e, "KeyK")) {
      e.preventDefault();
      paletteOpen = !paletteOpen;
      return;
    }
    // No bare-key section jumps (2026-09-14). Digits and letters that
    // changed screens were easy to set off by accident and, once the rail
    // folded, no longer matched its order; the sidebar and ⌘K are the ways
    // to move.
  }
</script>

<svelte:window onkeydown={onKey} />

<!-- The window's last line of defence (DIR-11): an error in the chrome
     itself — the rail, the header, the palette — used to blank the whole
     window. Each view has a boundary of its own below; this one catches
     what is left and offers a reload. -->
<svelte:boundary onerror={(e) => reportCrash("window", e)}>
<!-- Every toast, said aloud (DIR-12). Toasts carry a good share of the
     app's errors, and one drawn into a fresh element is not announced: a
     live region has to exist before its text changes. This one always
     does, on every screen — the wizard flashes too — and is never seen. -->
<div class="sr-only" role="status" aria-live="polite" aria-atomic="true">{app.toast ?? ""}</div>
{#if app.needsUnlock && app.lock}
  <!-- Before `fatal`, and before the wizard. A sealed key stops everything
       Director does — `bootstrap_status` reads it, `start_engine` signs
       with it — so there is nothing behind this to draw yet, and nothing
       downstream can produce a message a person could act on. -->
  <UnlockGate lock={app.lock} onunlocked={(next) => app.onUnlocked(next)} />
{:else if app.needsKey}
  <!-- The same kind of door for a key on a security key that is not
       connected: nothing can be signed, so nothing can start. -->
  <KeyGate mode="start" />
{:else if app.fatal}
  <!-- A way forward, not just the OS's sentence (DIR-11): plain words for
       the failures people actually hit, and Retry, Copy details and Open
       logs — the three things anyone stuck here needs. -->
  <div class="flex h-full items-center justify-center p-7">
    <CrashCard
      eyebrow="engine failed"
      title="Director could not start"
      message={app.fatal}
      hint={startHint(app.fatal)}
      onretry={() => app.retryStart()}
    />
  </div>
{:else if app.needsWizard}
  <Wizard />
{:else if !app.ready}
  <div class="flex h-full items-center justify-center">
    <div class="pop-in flex items-center gap-3 rounded-xl bg-surface px-6 py-4 text-base text-ink-3" style="box-shadow: var(--sh-card-lg);">
      <Spinner size={18} thickness={2.5} color="var(--accent-ink)" track="var(--accent-wash)" />
      joining the mesh…
    </div>
  </div>
{:else}
  <!-- Full-bleed, two columns: the rail, floor to ceiling, and the document
       column with one 56px toolbar over it. The 2026-08 shell stacked
       three dark bands (masthead, breadcrumb strip, rail) around a light
       page; the chrome is the page's own ramp now, one tone apart, so the
       window reads as one object in either theme. -->
  <div class="flex h-full w-full overflow-hidden" style="background: var(--bg-app); min-height: 0;">
    <Rail {nav} onSearch={() => (paletteOpen = true)} />

    <!-- The document column carries the section's hue as three custom
         properties (`lib/hues.ts`), so tabs, chips, selections and headers
         inside it colour themselves without each view knowing which
         section it is. -->
    <div class="canvas-ground flex min-h-0 min-w-0 flex-1 flex-col" style={hueStyle(VIEW_HUE[current.id])}>
      <Header label={current.label} root={current.id} />

      <main class="flex min-h-0 min-w-0 flex-1 flex-col">
        <!-- The unenrolled state, everywhere rather than on one screen: a
             person whose every request is denied should not have to find
             Access to be told why (M23 §4). -->
        <EnrolBanner />

        <!-- A demo mesh is indistinguishable from a real one on purpose,
             so it says so on every screen until it is stopped (L1 P3). -->
        <DemoBanner />

        {#if app.engineStopped}
          <div
            class="flex flex-none items-center gap-3 px-[22px] py-2.5 text-sm"
            style="background: var(--status-warn-wash); color: var(--status-warn-ink); border-bottom: 1px solid var(--edge);"
            transition:riseIn={{ y: -6 }}
          >
            <span class="font-semibold">This machine's node is stopped.</span>
            It drained and left the mesh's rotation; the tables show the last known state.
            <button class="ml-auto font-semibold" style="color: var(--accent-ink);" onclick={() => app.go("settings")}>
              Start it again from Settings →
            </button>
          </div>
        {/if}

        <!-- Keyed on the view so a switch re-mounts and re-enters — and so
             a view that crashed is left behind by moving to another. -->
        {#key app.view}
          <!-- One view throwing takes that view down, not the window: the
               rail and header stay, the error goes to the log, and the
               screen offers to try again (DIR-11). -->
          <svelte:boundary onerror={(e) => reportCrash(`view:${app.view}`, e)}>
          {#if fullBleed}
            <div class="flex min-h-0 flex-1 flex-col" use:reveal={{ y: 10 * dir, duration: DUR.slow }}>
              <Mesh />
            </div>
          {:else}
            <!-- The standard page header: the title at 30px and one line
                 of plain words under it saying what the screen is for. The
                 glyph-in-a-tile that used to open the row is gone — the
                 section's tile is in the rail, four rows up and always in the
                 same place, and repeating it beside the title spent the
                 hue twice to say one thing. -->
            <div class="flex flex-none flex-col gap-[5px] px-7 pt-5 pb-4">
              <h1 class="pagetitle" use:reveal={{ x: -8, y: 0, duration: DUR.slow }}>{current.label}</h1>
              <div class="pagesub" use:reveal={{ x: -8, y: 0, delay: 40, duration: DUR.slow }}>{current.sub}</div>
            </div>
            <div
              class="min-h-0 flex-1 overflow-y-auto px-7 pt-1 pb-7"
              use:reveal={{ y: 12 * dir, duration: DUR.slow, delay: 30 }}
            >
              <!-- One `Lazy` rather than a chain of them: the loader is
                   picked by view, so adding a screen is a line in `VIEWS`
                   and nothing here. -->
              {#if viewLoader}
                <Lazy load={viewLoader} />
              {/if}
            </div>
          {/if}
          {#snippet failed(error, reset)}
            {@const d = describeError(error)}
            <div class="flex min-h-0 flex-1 items-center justify-center p-7">
              <CrashCard
                eyebrow="this screen failed"
                title={`${current.label} hit a problem`}
                message={d.message}
                stack={d.stack}
                hint="The rest of Director is fine — pick another section, or try this one again. The details are in the log."
                retryLabel="Try again"
                onretry={reset}
              />
            </div>
          {/snippet}
          </svelte:boundary>
        {/key}
      </main>
    </div>
  </div>

  <Palette bind:open={paletteOpen} />

  {#if app.keyPaused !== null}
    <!-- The security key refused and Director stopped asking (2026-09-18):
         one calm screen instead of a prompt after every refresh. -->
    <KeyGate mode="paused" />
  {/if}

  {#if app.toast}
    <div
      class="on-code fixed right-5 bottom-5 z-50 flex w-80 overflow-hidden rounded-lg bg-code text-sm text-inkcode"
      style="box-shadow: var(--sh-modal);"
      in:slideX={{ x: 22, duration: DUR.slow }}
      out:fadeIn={{ duration: DUR.base }}
    >
      <div class="w-1 flex-none" style="background: var(--accent-bright);"></div>
      <div class="selectable px-4 py-3">{app.toast}</div>
    </div>
  {/if}
{/if}

{#snippet failed(error)}
  {@const d = describeError(error)}
  <div class="flex h-full items-center justify-center p-7">
    <CrashCard
      eyebrow="window failed"
      title="Director hit a problem"
      message={d.message}
      stack={d.stack}
      hint="The mesh is unaffected — this is the window. Reloading it picks up where the engine is."
      retryLabel="Reload window"
      onretry={() => location.reload()}
    />
  </div>
{/snippet}
</svelte:boundary>
