<script lang="ts">
  // The rail — 240px, floor to ceiling, and the only navigation in the
  // window. The 6c pass moved two more things into it and took one out of
  // the content area entirely:
  //
  //   1. **The search field**, at the top, under the mesh. It used to be a
  //      button in the header dressed as a field; here it is the first
  //      thing in the sidebar, which is the first place a person looks.
  //   2. **The sub-sections**, indented under the section that owns them
  //      on a 1px rule (`chrome/nav.ts` says which section has which).
  //      Mesh's eight panels were a tab row inside the page; a tab row is
  //      a second navigation, in a second place, in a second shape, and
  //      the screen with the most panels was the screen that needed the
  //      rail's help most.
  //   3. …and the account stays at the bottom: who you are, your role, and
  //      behind it the theme control and the build.
  //
  // **Rows with children fold (2026-09-14).** Mesh, Settings, Run and
  // Services each carry a chevron, and pressing the row opens or closes
  // what is under it — it does not navigate; the rows under it are the
  // destinations. Before, a section's rows appeared only once you were in
  // it, so "what is under Mesh" cost a page change to answer, and nothing
  // on a closed row said it had anything under it at all.
  //
  // Every section wears a 20px rounded tile in its own colour
  // (`lib/hues.ts`) with its glyph knocked out of it. That is the whole
  // wayfinding system: after a week the tile IS the label, which is what
  // lets the words beside them be set in one weight and one colour.
  //
  // The current row — section, or sub-section — is a solid teal fill with
  // near-black on it, and it is the only saturated area in the window.

  import { api } from "../api";
  import { app, initialsOf, type ViewName } from "../state.svelte";
  import { chrome } from "./chrome.svelte";
  import { SUBNAV, SECTIONS, sectionOf, type NavItem, type NavSection } from "./nav";
  import { untrack } from "svelte";
  import { theme, type ThemeMode } from "../theme.svelte";
  import Icon from "../components/Icon.svelte";
  import BrandMark from "../components/BrandMark.svelte";
  import { popMenu, roll } from "../motion";
  import { VIEW_HUE, hueStyle, hueOf } from "../hues";
  import { roleHue } from "../roles";
  import { modKey } from "../keys";
  import { outdatedCount } from "../versions";

  let { nav, onSearch }: { nav: NavItem[]; onSearch: () => void } = $props();

  let meshMenu = $state(false);
  let acctMenu = $state(false);

  /** A row of the rail: a section that is one view, or a section that is
   *  several (`SECTIONS`) with its member views nested under it. */
  type Row =
    | { kind: "view"; item: NavItem }
    | { kind: "section"; section: NavSection; items: NavItem[] };

  const groups = $derived.by(() => {
    const out: { name: string; rows: Row[] }[] = [];
    for (const item of nav) {
      let group = out[out.length - 1];
      if (!group || group.name !== item.group) {
        group = { name: item.group, rows: [] };
        out.push(group);
      }
      const section = SECTIONS.find((s) => s.views.includes(item.id));
      if (!section) {
        group.rows.push({ kind: "view", item });
        continue;
      }
      // Members are nested in the section's own order, not the nav
      // table's: the section says what it lands on, and that row goes
      // first.
      const row = group.rows.find((r) => r.kind === "section" && r.section.id === section.id);
      if (row && row.kind === "section") row.items.push(item);
      else group.rows.push({ kind: "section", section, items: [item] });
    }
    for (const g of out)
      for (const r of g.rows)
        if (r.kind === "section")
          r.items.sort((a, b) => r.section.views.indexOf(a.id) - r.section.views.indexOf(b.id));
    return out;
  });

  /** The fold a view sits in: its section's, or its own when it has panels. */
  function foldOf(view: ViewName): string | null {
    const section = sectionOf(view);
    if (section) return `section:${section.id}`;
    return SUBNAV[view] ? `view:${view}` : null;
  }

  // Wherever you land, the row you are on is visible. The palette, a deep
  // link or the back button can all put you inside a
  // closed section, and a rail whose current row is folded away has lost
  // the one thing it is for. Only *arriving* opens a fold — closing the
  // one you are in stays closed until you move.
  //
  // The first pass is launch rather than arrival: when the rail remembers
  // what was open, that memory wins, and the parent row still marks where
  // you are (`data-here`).
  let launched = false;
  $effect(() => {
    const fold = foldOf(app.view);
    void app.sub;
    untrack(() => {
      const first = !launched;
      launched = true;
      if (fold && !(first && chrome.railOpenStored)) chrome.railToggle(fold, true);
    });
  });

  // From the engine's own tally rather than counted here (**H-1**):
  // `app.jobs` is the newest few hundred, so counting it would have made
  // the badge quietly wrong on a busy mesh.
  const runningJobs = $derived(app.jobsCounts.running);
  const activeNodes = $derived(app.nodes.filter((n) => !n.revoked && !n.left).length);
  const collapsed = $derived(chrome.railCollapsed);

  /** The trailing count on an entry, or null when it would only say "0". */
  function countFor(id: ViewName): number | null {
    if (id === "nodes") return activeNodes > 0 ? activeNodes : null;
    if (id === "library") return app.services.length > 0 ? app.services.length : null;
    // Running jobs are the mesh doing its job — a fact, not a summons — so
    // they are a quiet count rather than a pulse.
    if (id === "jobs") return runningJobs > 0 ? runningJobs : null;
    return null;
  }

  /**
   * The one number a panel row carries: how many nodes are behind the
   * newest build the mesh knows about.
   *
   * It rode the Updates tab inside the Fleet table until Updates became a
   * row out here, and it is the whole reason to open that row — so it
   * comes with it. Warn-tinted rather than muted, because an out-of-date
   * fleet is not neutral; static rather than pulsing, because nobody is
   * waiting on you for it. Same reading of `cap.ver` as the panel itself
   * (`lib/versions.ts`), so the rail and the screen cannot disagree.
   */
  const behind = $derived(outdatedCount(app.nodes, app.about?.bundledMeshnode));

  /** The live badges: somebody waiting on *you*. The only rail signals that
   *  are a summons rather than a fact, so the only ones allowed to pulse.
   *
   *  The same number the header bell shows (`app.waitingOnYou`, UX-6). The
   *  two used to count different things — the badge the join queue, the
   *  bell approvals — which was two answers to "is anything waiting on me"
   *  on one screen. */
  function liveFor(id: ViewName): { n: number; tone: "ok" | "warn" } | null {
    if (id !== "access") return null;
    const n = app.waitingOnYou;
    return n > 0 ? { n, tone: "warn" } : null;
  }

  /** "Is this thing actually connected", answered from any screen. */
  const live = $derived.by((): { label: string; tone: string; pulse: boolean } => {
    if (app.engineStopped) return { label: "Stopped", tone: "var(--status-warn)", pulse: false };
    if (app.status?.draining) return { label: "Draining", tone: "var(--status-warn)", pulse: false };
    return { label: "Live", tone: "var(--accent)", pulse: true };
  });
  const activity = $derived.by(() => {
    if (app.engineStopped) return "Drained and stopped — the tables show the last known state.";
    if (app.status?.draining) return "Draining: finishing accepted work, refusing new.";
    const recent = app.jobs.filter((j) => Date.now() - j.createdAtMicros / 1000 < 600_000).length;
    if (recent === 0) return "Nothing submitted in the last 10 minutes.";
    return `${recent} job${recent === 1 ? "" : "s"} submitted in the last 10 minutes.`;
  });

  const avatar = $derived(initialsOf(app.whoami?.username ?? "me"));
  const role = $derived(
    app.whoami?.isFounder ? "founder" : (app.whoami?.roles[0] ?? (app.whoami?.registered ? "member" : "guest")),
  );

  async function copyMeshId() {
    const id = app.status?.meshId;
    if (!id) return;
    try {
      await api.copyText(id);
      app.flash("Mesh id copied");
    } catch {
      app.flash("Could not reach the clipboard");
    }
    meshMenu = false;
  }

  function closeMenus() {
    meshMenu = false;
    acctMenu = false;
  }
</script>

<!-- A press outside either menu closes it — `pointerdown`, so a click that
     some other control swallows still counts — and so does Escape. -->
<svelte:window
  onpointerdown={(e) => {
    if (!meshMenu && !acctMenu) return;
    if ((e.target as Element | null)?.closest?.("[data-menu='rail']")) return;
    closeMenus();
  }}
  onkeydown={(e) => {
    if ((meshMenu || acctMenu) && e.key === "Escape") {
      e.preventDefault();
      closeMenus();
    }
  }}
/>

<aside
  class="chrome-focus relative flex flex-none flex-col overflow-visible"
  style={`width:${collapsed ? 56 : 240}px; background: var(--bg-sidebar); border-right: 1px solid var(--edge); transition: width var(--dur-base) var(--ease);`}
>
  <!-- ─────────── identity: the mesh switcher ─────────── -->
  <div class="relative flex-none px-2 pt-2" data-menu="rail">
    <button
      class="switch-btn"
      class:justify-center={collapsed}
      class:is-open={meshMenu}
      title={collapsed ? (app.status?.meshName ?? "This mesh") : "This mesh"}
      aria-expanded={meshMenu}
      onclick={(e) => {
        e.stopPropagation();
        acctMenu = false;
        meshMenu = !meshMenu;
      }}
    >
      <BrandMark size={24} />
      {#if !collapsed}
        <span class="min-w-0 flex-1 text-left leading-tight">
          <span class="flex items-center gap-[6px]">
            <span class="truncate text-[13px] font-semibold text-ink">{app.status?.meshName ?? "—"}</span>
            <span class="relative inline-block h-[6px] w-[6px] flex-none rounded-full" style={`background:${live.tone};`}>
              {#if live.pulse}
                <span class="absolute inset-0 rounded-full" style={`background:${live.tone}; animation: mw-pulse 2.4s ease-out infinite;`}></span>
              {/if}
            </span>
          </span>
          <span class="mt-[1px] block truncate text-[11px] text-ink-muted">Meshware Director</span>
        </span>
        <span class="flex-none text-ink-faint"><Icon name="chevron-down" size={12} /></span>
      {/if}
    </button>

    {#if meshMenu}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <div
        class="menu absolute top-[50px] left-2 z-40 w-[264px]"
        style="transform-origin: 12px 0;"
        use:popMenu
        onclick={(e) => e.stopPropagation()}
      >
        <div class="px-3 pt-[11px] pb-[10px]" style="border-bottom: 1px solid var(--edge);">
          <div class="flex items-center justify-between">
            <span class="mono text-[13px] font-semibold text-ink">{app.status?.meshName ?? "—"}</span>
            <span class="text-[11px] font-semibold" style={`color:${live.tone === "var(--accent)" ? "var(--accent-ink)" : "var(--status-warn-ink)"};`}>{live.label}</span>
          </div>
          <div class="mono mt-[3px] truncate text-[10.5px] text-ink-muted">{app.status?.meshId ?? ""}</div>
          <div class="mt-[8px] text-[11.5px] leading-[1.45] text-ink-4">{activity}</div>
        </div>
        <button class="menu-item" onclick={copyMeshId}><Icon name="copy" size={14} /> Copy mesh id</button>
        <button
          class="menu-item"
          onclick={() => {
            meshMenu = false;
            app.go("mesh");
          }}><Icon name="mesh" size={14} /> Open the mesh</button
        >
        <div class="px-3 pt-[8px] pb-[10px] text-[11px] leading-[1.45] text-ink-muted" style="border-top: 1px solid var(--edge);">
          This machine belongs to one mesh. A machine joins another by pairing from that mesh's side.
        </div>
      </div>
    {/if}
  </div>

  <!-- ─────────── search ─────────── -->
  <!-- A doorway to the palette, shaped like a field and placed where a
       field goes. It does not accept typing in place — the palette does
       the typing, and it opens on the first keystroke — so it is a button
       with a text cursor rather than an input that would swallow the first
       character and then hand the rest to something else. -->
  {#if !collapsed}
    <div class="flex-none px-2 pt-2">
      <button class="rail-field" title={`Search or jump to anything  ·  ${modKey("K")}`} onclick={onSearch}>
        <span class="rail-field-glyph"><Icon name="search" size={14} /></span>
        <span class="flex-1 text-left">Search</span>
        <span class="kbd">{modKey("K")}</span>
      </button>
    </div>
  {:else}
    <div class="flex-none px-2 pt-2">
      <button class="mast-btn h-8 w-full" title={`Search  ·  ${modKey("K")}`} onclick={onSearch}>
        <Icon name="search" size={16} />
      </button>
    </div>
  {/if}

  <!-- ─────────── the sections ─────────── -->
  <nav class="relative flex min-h-0 flex-1 flex-col overflow-y-auto px-2 pt-3 pb-2">
    {#each groups as group, gi (group.name)}
      {#if collapsed}
        {#if gi > 0}
          <div class="mx-[10px] my-2 h-px flex-none" style="background: var(--edge);"></div>
        {/if}
      {:else}
        <div class="rail-group flex-none" style={`padding-top:${gi === 0 ? 4 : 16}px;`}>
          {group.name}
        </div>
      {/if}

      {#each group.rows as row (row.kind === "view" ? row.item.id : `s:${row.section.id}`)}
        {#if row.kind === "view"}
          {@const item = row.item}
          {@const count = countFor(item.id)}
          {@const live = liveFor(item.id)}
          {@const panels = SUBNAV[item.id]}
          {@const here = app.view === item.id}
          {@const foldId = `rail-fold-view-${item.id}`}
          <!-- Only the full-width rail folds. The icon strip has no room for
               a row under a row, so there the tile still goes straight to
               the section, as it always did. -->
          {@const folds = !!panels && !collapsed}
          {@const open = folds && chrome.railIsOpen(`view:${item.id}`)}
          <!-- The one number a closed Mesh would otherwise hide: nodes behind
               the newest build rides the Updates row, and the row is folded
               away. -->
          {@const hidden = folds && !open && item.id === "mesh" && behind > 0 ? behind : null}
          <button
            class="rail-item"
            class:justify-center={collapsed}
            style={`${hueStyle(VIEW_HUE[item.id])} ${collapsed ? "padding: 0;" : ""}`}
            data-nav={item.id}
            data-here={folds && here && !open ? "" : undefined}
            aria-current={here && !panels ? "page" : undefined}
            aria-expanded={folds ? open : undefined}
            aria-controls={folds ? foldId : undefined}
            title={collapsed ? item.label : undefined}
            onclick={() => (folds ? chrome.railToggle(`view:${item.id}`) : app.go(item.id))}
          >
            <!-- `rail-glyph` is kept on the tile because the marker's landing
                 animation pops whatever carries it (`motion.ts`). -->
            <span class="rail-tile rail-glyph"><Icon name={item.id} size={13} /></span>
            {#if !collapsed}
              <!-- A section holding the open panel is bold; it is not the
                   filled row, because one of its panels is. -->
              <span class="min-w-0 flex-1 truncate" class:font-semibold={here && !!panels}>{item.label}</span>
              {#if live}
                <span
                  class="rail-count mono inline-flex flex-none items-center gap-1 rounded-full px-[6px] py-px text-[10.5px] font-semibold"
                  style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
                >
                  <span class="breathe h-1 w-1 rounded-full" style="background: var(--status-warn);"></span><span use:roll={live.n}>{live.n}</span>
                </span>
              {:else if hidden != null}
                <span
                  class="rail-count num flex-none rounded-full px-[6px] py-px text-[10.5px] font-semibold"
                  title={`${hidden} node${hidden === 1 ? "" : "s"} are not on the newest build`}
                  style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
                  use:roll={hidden}>{hidden}</span
                >
              {:else if count != null}
                <span class="rail-count num flex-none text-[11px]" use:roll={count}>{count}</span>
              {/if}
              {#if folds}
                <span class="rail-chev" data-open={open ? "" : undefined} aria-hidden="true"><Icon name="chevron-right" size={12} /></span>
              {/if}
            {:else if live}
              <span
                class="absolute top-[4px] right-[8px] h-[6px] w-[6px] rounded-full"
                style="background: var(--status-warn); box-shadow: 0 0 0 2px var(--bg-sidebar);"
              ></span>
            {/if}
          </button>

          <!-- The panels, indented on a 1px rule, in a fold that stays mounted
               so it can animate shut; `inert` while closed keeps Tab and a
               screen reader off rows nobody can see. -->
          {#if folds && panels}
            <div class="rail-fold" id={foldId} data-open={open ? "" : undefined} inert={!open}>
              <div class="rail-fold-inner">
                <div class="rail-sub">
                  {#each panels as panel (panel.id)}
                    <button
                      class="rail-subitem"
                      data-nav={`${item.id}:${panel.id}`}
                      aria-current={here && app.sub === panel.id ? "page" : undefined}
                      onclick={() => app.go(item.id, panel.id)}
                    >
                      <span class="truncate">{panel.label}</span>
                      {#if panel.id === "updates" && behind > 0}
                        <span
                          class="rail-count num flex-none rounded-full px-[6px] py-px text-[10.5px] font-semibold"
                          title={`${behind} node${behind === 1 ? "" : "s"} are not on the newest build`}
                          style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
                          use:roll={behind}>{behind}</span
                        >
                      {/if}
                    </button>
                  {/each}
                </div>
              </div>
            </div>
          {/if}
        {:else}
          <!-- A section of whole views (`SECTIONS`). The row is the
               section — its tile, its name, bold while one of its views is
               open — and the views nest under it the way Mesh's panels do,
               each with its own count, because each is still a view. -->
          {@const section = row.section}
          {@const home = section.home}
          {@const here = section.views.includes(app.view)}
          {@const foldId = `rail-fold-section-${section.id}`}
          {@const open = !collapsed && chrome.railIsOpen(`section:${section.id}`)}
          {@const summons = row.items.map((i) => liveFor(i.id)).find((l) => l !== null) ?? null}
          <button
            class="rail-item"
            class:justify-center={collapsed}
            style={`${hueStyle(VIEW_HUE[home])} ${collapsed ? "padding: 0;" : ""}`}
            data-nav={`section:${section.id}`}
            data-here={!collapsed && here && !open ? "" : undefined}
            aria-expanded={collapsed ? undefined : open}
            aria-controls={collapsed ? undefined : foldId}
            title={collapsed ? section.label : undefined}
            onclick={() => (collapsed ? app.go(home) : chrome.railToggle(`section:${section.id}`))}
          >
            <span class="rail-tile rail-glyph"><Icon name={section.icon} size={13} /></span>
            {#if !collapsed}
              <span class="min-w-0 flex-1 truncate" class:font-semibold={here}>{section.label}</span>
              <!-- Closed, the section carries the one number a member would
                   pulse for; open, the member carries it itself. -->
              {#if summons && !open}
                <span
                  class="rail-count mono inline-flex flex-none items-center gap-1 rounded-full px-[6px] py-px text-[10.5px] font-semibold"
                  style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
                >
                  <span class="breathe h-1 w-1 rounded-full" style="background: var(--status-warn);"></span><span use:roll={summons.n}>{summons.n}</span>
                </span>
              {/if}
              <span class="rail-chev" data-open={open ? "" : undefined} aria-hidden="true"><Icon name="chevron-right" size={12} /></span>
            {:else if summons && !here}
              <span
                class="absolute top-[4px] right-[8px] h-[6px] w-[6px] rounded-full"
                style="background: var(--status-warn); box-shadow: 0 0 0 2px var(--bg-sidebar);"
              ></span>
            {/if}
          </button>

          {#if collapsed}
            {#if here}
              <!-- Collapsed, the members are tiles of their own, smaller
                   and inset, so the strip still reaches every view. -->
              <div class="mb-1 flex flex-none flex-col items-center gap-px">
                {#each row.items as item (item.id)}
                  <button
                    class="rail-item h-7 w-7 justify-center p-0"
                    style={hueStyle(VIEW_HUE[item.id])}
                    data-nav={item.id}
                    aria-current={app.view === item.id ? "page" : undefined}
                    title={item.label}
                    onclick={() => app.go(item.id)}
                  >
                    <span class="rail-glyph inline-grid place-items-center" style="color: var(--hue-ink);"><Icon name={item.id} size={13} /></span>
                  </button>
                {/each}
              </div>
            {/if}
          {:else}
            <div class="rail-fold" id={foldId} data-open={open ? "" : undefined} inert={!open}>
              <div class="rail-fold-inner">
                <div class="rail-sub">
                  {#each row.items as item (item.id)}
                    {@const count = countFor(item.id)}
                    {@const live = liveFor(item.id)}
                    <button
                      class="rail-subitem"
                      data-nav={item.id}
                      aria-current={app.view === item.id ? "page" : undefined}
                      onclick={() => app.go(item.id)}
                    >
                      <span class="truncate">{item.label}</span>
                      {#if live}
                        <span
                          class="rail-count mono inline-flex flex-none items-center gap-1 rounded-full px-[6px] py-px text-[10.5px] font-semibold"
                          style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
                        >
                          <span class="breathe h-1 w-1 rounded-full" style="background: var(--status-warn);"></span><span use:roll={live.n}>{live.n}</span>
                        </span>
                      {:else if count != null}
                        <span class="rail-count num flex-none text-[11px]" use:roll={count}>{count}</span>
                      {/if}
                    </button>
                  {/each}
                </div>
              </div>
            </div>
          {/if}
        {/if}
      {/each}
    {/each}
  </nav>

  <!-- ─────────── the account ─────────── -->
  <div class="relative flex-none px-2 pb-2" data-menu="rail">
    <button
      class="switch-btn"
      class:justify-center={collapsed}
      class:is-open={acctMenu}
      title={collapsed ? (app.whoami?.username ?? "Account") : "Account, theme and build"}
      aria-expanded={acctMenu}
      onclick={(e) => {
        e.stopPropagation();
        meshMenu = false;
        acctMenu = !acctMenu;
      }}
    >
      <!-- The avatar's colour is the person's, hashed from their name — so
           two people at two machines see themselves in different colours
           without a settings page. -->
      <span
        class="mono relative grid h-6 w-6 flex-none place-items-center rounded-full text-[10.5px] font-semibold"
        style={`${hueStyle(hueOf(app.whoami?.username ?? "me"))} background: var(--hue-wash); color: var(--hue-ink); user-select: none;`}
      >
        {avatar}
      </span>
      {#if !collapsed}
        <span class="min-w-0 flex-1 text-left leading-tight">
          <span class="block truncate text-[13px] font-medium text-ink">{app.whoami?.username ?? "…"}</span>
          <span class="block truncate text-[11px] font-medium" style={`${hueStyle(roleHue(role))} color: var(--hue-ink);`}>{role}</span>
        </span>
        <span class="flex-none text-ink-faint"><Icon name="more-h" size={14} /></span>
      {/if}
    </button>

    {#if acctMenu}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <div
        class="menu absolute bottom-[50px] left-2 z-40 w-[264px]"
        style="transform-origin: 12px 100%;"
        use:popMenu
        onclick={(e) => e.stopPropagation()}
      >
        <div class="px-3 pt-[11px] pb-[10px]" style="border-bottom: 1px solid var(--edge);">
          <div class="text-[13px] font-semibold text-ink">{app.whoami?.username ?? "…"}</div>
          <div class="mono mt-[3px] truncate text-[10.5px] text-ink-muted">{app.whoami?.userId ?? ""}</div>
        </div>
        <div class="flex items-center justify-between px-3 py-[9px]">
          <span class="text-[12px] text-ink-3">Theme</span>
          <div class="seg" role="radiogroup" aria-label="Theme">
            {#each [["system", "Auto"], ["light", "Light"], ["dark", "Dark"]] as [id, label] (id)}
              <button
                role="radio"
                aria-checked={theme.mode === id}
                data-selected={theme.mode === id}
                onclick={() => theme.set(id as ThemeMode)}
              >
                {label}
              </button>
            {/each}
          </div>
        </div>
        <button
          class="menu-item"
          onclick={() => {
            acctMenu = false;
            app.go("access");
          }}><Icon name="access" size={14} /> Your access and grants</button
        >
        <button
          class="menu-item"
          onclick={() => {
            acctMenu = false;
            app.go("settings", "this-peer");
          }}><Icon name="settings" size={14} /> This machine's settings</button
        >
        <div
          class="mono flex items-center gap-2 px-3 pt-[8px] pb-[10px] text-[10.5px] text-ink-muted"
          style="border-top: 1px solid var(--edge);"
        >
          <!-- Director's own version beside the engine's: they are separate
               builds, and a bug report needs both. One string, so it
               truncates as a line rather than wrapping cell by cell. -->
          <span class="truncate">
            {[
              app.about ? `Director ${app.about.version}` : null,
              app.status?.version ? `engine ${app.status.version}` : null,
              app.status?.proto ? `proto ${app.status.proto}` : null,
            ]
              .filter(Boolean)
              .join(" · ")}
          </span>
        </div>
      </div>
    {/if}
  </div>
</aside>

<style>
  /* The two switcher rows (mesh at the top, account at the bottom): a
     40px button that reads as a row at rest and as a control on hover. */
  .switch-btn {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    height: 40px;
    padding: 0 8px;
    border-radius: var(--radius-md);
    cursor: pointer;
    transition:
      background-color var(--dur-fast) var(--ease),
      box-shadow var(--dur-fast) var(--ease);
  }
  .switch-btn:hover,
  .switch-btn.is-open {
    background: var(--surface-2);
  }
  .switch-btn.justify-center {
    padding: 0;
  }

  .menu {
    overflow: hidden;
    border-radius: var(--radius-lg);
    background: var(--surface-1);
    box-shadow: var(--sh-pop);
  }
  .menu-item {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 100%;
    padding: 8px 12px;
    text-align: left;
    font-size: 12.5px;
    color: var(--ink-2);
    cursor: pointer;
    transition: background-color var(--dur-fast) var(--ease);
  }
  .menu-item:hover {
    background: var(--surface-2);
  }
  .menu-item :global(svg) {
    color: var(--ink-faint);
  }
</style>
