<script lang="ts" module>
  /** The CLI command that shows the same thing, where one exists. */
  const VIEW_CMD: Partial<Record<string, string>> = {
    nodes: "meshnode nodes",
    compute: "meshnode totals",
    jobs: "meshnode jobs",
    deployments: "meshnode deployment ls",
    secrets: "meshnode secrets ls",
  };
</script>

<script lang="ts">
  import { VIEW_HUE, hueStyle } from "../hues";
  // Command palette (components/palette-and-nav.md): 640px, 14vh offset,
  // sectioned results, accent-wash selection with a 2px inset bar, and a
  // footer that teaches the three keys — the convention Raycast and Linear
  // set, and the one a new user reaches for before reading anything.
  import { api, errText } from "../api";
  import { app, type ViewName } from "../state.svelte";
  import { CORE_NAV } from "../chrome/nav";
  import { KIND_LABEL, originsOf } from "../origins";
  import Icon from "./Icon.svelte";
  import { reveal, staggerIn, DUR } from "../motion";
  import { portal } from "../portal";
  import { dialog } from "../dialog";

  import { holdStill } from "../stillness";
  let { open = $bindable(false) }: { open?: boolean } = $props();

  // See Modal.svelte: the palette opens over the Mesh tab more than
  // anything else in the app, which is the worst case for a blurred scrim
  // over live animation (M-28).
  $effect(() => (open ? holdStill() : undefined));
  let query = $state("");
  let selected = $state(0);
  let inputEl = $state<HTMLInputElement | null>(null);

  type Action = {
    section: string;
    label: string;
    sub?: string;
    icon: string;
    keycap?: string;
    /**
     * The command line that does the same thing (L2 P3).
     *
     * **Present only where it can be named honestly** (D-L2.4). An entry
     * with no single CLI equivalent — a screen with seven tabs, a form, a
     * two-step registration — shows nothing rather than an approximation,
     * because a palette that teaches you a command which turns out to do
     * something adjacent is worse than one that teaches you nothing.
     *
     * Placeholders are `<angled>` and stay: the point is the shape of the
     * command, and a person pasting it into a runbook has to see what they
     * are expected to fill in.
     */
    cmd?: string;
    run: () => void;
  };

  const sources = $derived(originsOf(app.sources, app.bundles));

  /**
   * Every action the palette can offer, **without reading `query`**
   * (M-26).
   *
   * This whole builder used to sit in one `$derived` that also did the
   * filtering, so reading `query` re-ran the lot on every keystroke: a
   * fresh object per node, per service, per source — thousands of
   * allocations on a big mesh and, in the filter below, thousands of
   * `toLowerCase()` calls, synchronously, inside the input's own event
   * handler.
   *
   * Splitting it in two is the entire fix. This half depends only on the
   * data and is rebuilt when the data changes; the half below depends on
   * `query` and does nothing but compare strings that were lowercased once.
   */
  const actions = $derived.by((): Action[] => {
    const views: Action[] = (
      // No keycaps (2026-09-14): the bare-key section jumps are gone, so a
      // row names the screen and nothing else claims a key for it.
      //
      // Read off `chrome/nav.ts` rather than re-typed here. The list used
      // to be a second copy, and it had drifted: it offered "Go to Jobs"
      // on 4 while the rail said "Activity" on 5, and it was missing
      // Scripts and Secrets entirely. A palette that names different
      // sections from the sidebar is worse than no palette, and now that
      // the sidebar IS the navigation there is one table to read.
      //
      // The command column stays here, because it is about the CLI rather
      // than about the surface: it is empty for most of these on purpose
      // (D-L2.4) — Mesh is seven panels, Run is a form, Access is two
      // sections, and Benchmark's verb *runs* a benchmark rather than
      // listing them. A row that opens a screen must not advertise a
      // command that does something else.
      CORE_NAV.map(
        (n) => [n.id, `Go to ${n.label}`, VIEW_CMD[n.id] ?? ""] as [ViewName, string, string],
      )
    ).map(([id, label, cmd]) => ({
      section: "Views",
      label,
      icon: id,
      cmd: cmd || undefined,
      run: () => app.go(id),
    }));
    const quick: Action[] = [
      { section: "Actions", label: "Run a one-off command", icon: "terminal", cmd: "meshnode run '<command>'", run: () => jump("run", "run:mode", "command") },
      { section: "Actions", label: "Run one command per item", sub: "New run", icon: "compute", cmd: "meshnode map '<command> {}' ::: <items>", run: () => jump("run", "run:times", "each") },
      // "Add a script" writes a body *and* registers it — a push and a
      // create, and no one command. Named honestly by saying nothing.
      { section: "Actions", label: "Add a script", sub: "Library", icon: "plus", run: () => jump("library", "library:open", "add-script") },
      { section: "Actions", label: "Connect a Git repo", sub: "Library", icon: "plus", cmd: "meshnode sources add-git <name> --url <url>", run: () => jump("library", "library:open", "add-repo") },
      { section: "Actions", label: "Add a folder of scripts", sub: "Library", icon: "plus", cmd: "meshnode push <dir>", run: () => jump("library", "library:open", "add-folder") },
      { section: "Actions", label: "Set a secret", sub: "Secrets", icon: "plus", cmd: "meshnode secrets set <name>", run: () => jump("secrets", "secrets:open", "set") },
      // The commonest reason anybody opens Access, so it is the entry that
      // says what somebody would actually type into this box. The count is
      // in the label because a number is what makes people press it.
      {
        section: "Actions",
        label: app.joinQueueCount
          ? `Let someone in — ${app.joinQueueCount} waiting`
          : "Let someone in",
        sub: "Access",
        icon: "pair",
        run: () => jump("access", "access:tab", "people"),
      },
      { section: "Actions", label: "Invite someone to this mesh", sub: "Access", icon: "link", cmd: "meshnode invite create", run: () => jump("access", "access:open", "invite") },
      { section: "Actions", label: "Add a person by key", sub: "Access", icon: "plus", cmd: "meshnode users add <username> --key <key> --role <role>", run: () => jump("access", "access:open", "add-user") },
      { section: "Actions", label: "What everybody did", sub: "Access", icon: "history", cmd: "meshnode audit", run: () => jump("access", "access:tab", "history") },
      // The roles matrix is reference, not a listing: no verb prints it.
      { section: "Actions", label: "Roles and permissions", sub: "Access", icon: "check", run: () => jump("access", "access:tab", "roles") },
      { section: "Actions", label: "Who may open a terminal", sub: "Access", icon: "terminal", cmd: "meshnode sessions policy", run: () => jump("access", "access:tab", "terminals") },
      { section: "Actions", label: "Who is granted what", sub: "Access", icon: "check", cmd: "meshnode grants", run: () => jump("access", "access:tab", "people") },
      { section: "Actions", label: "What I may do here", sub: "Access", icon: "check", cmd: "meshnode whoami", run: () => jump("access", "access:tab", "people") },
      // A rollout is the one action here that changes what the mesh *is*, so
      // it is reachable by name rather than only by remembering which tab.
      { section: "Actions", label: "Add machines over SSH", sub: "Nodes", icon: "plus", cmd: "meshnode adopt <host>…", run: () => jump("nodes", "nodes:open", "deploy") },
      { section: "Actions", label: "Check for updates", sub: "Updates", icon: "refresh", cmd: "meshnode upgrade --status", run: toUpdates },
      { section: "Actions", label: "Which machines are out of date", sub: "Updates", icon: "refresh", cmd: "meshnode upgrade --status", run: toUpdates },
      { section: "Actions", label: "What the fleet is running", sub: "Updates", icon: "nodes", cmd: "meshnode upgrade --status", run: toUpdates },
      { section: "Actions", label: "Roll out a release", sub: "Updates", icon: "nodes", cmd: "meshnode upgrade --on <selector>", run: toUpdates },
      { section: "Actions", label: "How much machine is this", sub: "Mesh", icon: "compute", cmd: "meshnode totals", run: () => app.go("mesh", "capacity") },
      { section: "Actions", label: "How busy is every machine", sub: "Mesh", icon: "compute", cmd: "meshnode top", run: () => jump("nodes", "nodes:tab", "fleet") },
      { section: "Actions", label: "Accelerators in this mesh", sub: "Mesh", icon: "compute", cmd: "meshnode nodes --devices", run: () => jump("nodes", "nodes:tab", "fleet") },
      // Each names the pane its card is on: the panes are rail rows now,
      // and a focus key alone would land on This peer and scroll to a
      // card that is not rendered there.
      { section: "Actions", label: "Create an invite", sub: "Settings", icon: "pair", cmd: "meshnode invite create", run: () => toSettings("fleet", "invite") },
      { section: "Actions", label: "Run doctor", sub: "Settings", icon: "check", cmd: "meshnode doctor", run: () => toSettings("diagnostics", "doctor") },
      { section: "Actions", label: "Back this node up", sub: "Settings", icon: "save", cmd: "meshnode backup --out <file>", run: () => toSettings("backup", "data-safety") },
      { section: "Actions", label: "Pair a nearby node", sub: "Mesh", icon: "pair", cmd: "meshnode pair", run: () => app.go("mesh") },
    ];
    // No `cmd` on a node row. Selecting a machine here opens an inspector —
    // liveness, address, capacity, what it is running — and no single verb
    // prints that; `meshnode nodes` is the whole table and `meshnode facts`
    // is a different question about the same box. D-L2.4 says nothing
    // rather than either.
    const nodes: Action[] = app.nodes
      .filter((n) => !n.revoked && !n.left)
      .map((n) => ({
        section: "Nodes",
        label: n.name,
        sub: n.shortId,
        icon: "nodes",
        run: () => jump("nodes", "nodes:select", n.nodeId),
      }));
    const services: Action[] = app.services.flatMap((s) => [
      {
        section: "Scripts",
        label: `Run: ${s.name}`,
        sub: `${s.runtime} · v${s.version}${s.description ? ` · ${s.description}` : ""}`,
        icon: "run",
        cmd: `meshnode service run ${s.name}`,
        run: () => jump("run", "run:preselect", s.name),
      },
      {
        section: "Scripts",
        label: `Open: ${s.name}`,
        sub: `${s.workingDir ? `${s.workingDir}/` : ""}${s.entrypoint}`,
        icon: "library",
        cmd: `meshnode service show ${s.name}`,
        run: () => jump("library", "library:select", s.name),
      },
    ]);
    const bundles: Action[] = sources.map((o) => ({
      section: "Sources",
      label: `Browse: ${o.name}`,
      sub: `${KIND_LABEL[o.kind]}${o.fileCount === null ? "" : ` · ${o.fileCount} files`}`,
      icon: "library",
      // Only when there is content to browse. A registered repo that has
      // never synced has no bundle to list, and offering a command that
      // needs an id nobody has yet is the approximation D-L2.4 forbids.
      cmd: o.bundleId ? `meshnode bundles show ${o.bundleShort ?? o.bundleId}` : undefined,
      run: () => jump("library", "library:origin", o.key),
    }));
    const jobs: Action[] = app.jobs.slice(0, 12).map((j) => ({
      section: "Jobs",
      label: j.cmdPreview,
      sub: `${j.shortId} · ${j.state}`,
      icon: "jobs",
      cmd: `meshnode jobs show ${j.shortId}`,
      run: () => jump("jobs", "jobs:select", j.jobId),
    }));
    return [...views, ...quick, ...nodes, ...services, ...bundles, ...jobs];
  });

  /**
   * The haystack, lowercased once per data change rather than once per
   * keystroke per row (M-26).
   */
  const haystack = $derived(
    actions.map((a) => ({
      a,
      hay: `${a.label}\u0000${a.sub ?? ""}\u0000${a.cmd ?? ""}`.toLowerCase(),
    })),
  );

  // The command is searchable too, and that is half of what makes this
  // teach anything: somebody who knows `upgrade` finds the screen, and
  // somebody who knows the screen finds `upgrade`.
  const results = $derived.by((): Action[] => {
    const q = query.trim().toLowerCase();
    if (!q) return actions;
    return haystack.filter((h) => h.hay.includes(q)).map((h) => h.a);
  });

  /**
   * `⇧↵` — take the command instead of running the action (L2 P3).
   *
   * The palette is the one surface in Director that a person opens knowing
   * what they want, which makes it the one place where "and how would I
   * have done that from a terminal" is a question they are already halfway
   * through asking. Copying rather than navigating is the whole feature: a
   * command that lands in a runbook is worth more than a screen that
   * answers once.
   */
  async function copyCommand(action: Action | undefined): Promise<boolean> {
    if (!action?.cmd) return false;
    try {
      await api.copyText(action.cmd);
      app.flash(`copied: ${action.cmd}`);
    } catch (e) {
      app.flash(errText(e));
    }
    return true;
  }

  function jump(view: ViewName, key: string, value: string) {
    sessionStorage.setItem(key, value);
    app.go(view);
  }

  /** A Settings card: its pane, and the card to scroll to on it. */
  function toSettings(pane: string, focus: string) {
    sessionStorage.setItem("settings:focus", focus);
    app.go("settings", pane);
  }

  /** The rollout: a panel of Mesh, with its own rail row. */
  function toUpdates() {
    app.go("mesh", "updates");
  }

  $effect(() => {
    if (open) {
      query = "";
      selected = 0;
      setTimeout(() => inputEl?.focus(), 20);
    }
  });

  // Keep the highlighted row in sight: arrowing past the bottom of the
  // list used to move the selection somewhere nobody could see it.
  $effect(() => {
    if (!open) return;
    document.getElementById(`palette-opt-${selected}`)?.scrollIntoView({ block: "nearest" });
  });

  function onKey(e: KeyboardEvent) {
    // Escape is the dialog's (`use:dialog` below), like every other one.
    if (e.key === "ArrowDown") {
      e.preventDefault();
      selected = Math.min(selected + 1, results.length - 1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      selected = Math.max(selected - 1, 0);
    } else if (e.key === "Enter") {
      const action = results[selected];
      if (!action) return;
      if (e.shiftKey) {
        // Shift-Enter on a row with no command does nothing at all rather
        // than falling through to running it: a modifier that silently
        // means something else is how people stop trusting modifiers.
        e.preventDefault();
        void copyCommand(action).then((copied) => {
          if (copied) open = false;
        });
        return;
      }
      action.run();
      open = false;
    }
  }
</script>

{#if open}
  <div
    use:portal
    class="fixed inset-0 z-50 flex items-start justify-center pt-[14vh]"
    style="background: rgba(12,16,24,.55); backdrop-filter: blur(3px);"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) open = false;
    }}
  >
    <!-- A combobox over a listbox (DIR-12): focus stays in the field, the
         highlighted row is its active descendant, and a screen reader
         hears the row and how many there are rather than nothing. -->
    <div
      class="w-[640px] max-w-[92vw] overflow-hidden rounded-2xl bg-surface"
      style="box-shadow: var(--sh-modal), 0 0 0 1px var(--edge-card);"
      use:reveal={{ y: 10, scale: 0.985, duration: DUR.base }}
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
      use:dialog={{ onescape: () => (open = false) }}
    >
      <div class="flex h-[54px] items-center gap-3 border-b border-edge px-5">
        <span class="flex-none text-ink-faint" aria-hidden="true"><Icon name="search" size={16} /></span>
        <input
          bind:this={inputEl}
          bind:value={query}
          onkeydown={onKey}
          oninput={() => (selected = 0)}
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-list"
          aria-autocomplete="list"
          aria-activedescendant={results.length ? `palette-opt-${selected}` : undefined}
          aria-label="Search, or type a command"
          placeholder="Search, or type a command…"
          class="h-full w-full bg-transparent text-[15px] text-ink outline-none placeholder:text-ink-5"
          style="box-shadow: none;"
        />
        <span class="kbd flex-none" aria-hidden="true">esc</span>
      </div>
      <!-- Dealt once when the palette opens, not re-dealt as you type
           (M-26). This used to be keyed on `query`, on the argument that a
           search narrowing ten rows to two should read as an answer rather
           than as eight rows vanishing — true, and it cost a fresh 24-item
           ~290 ms stagger on every keystroke, which is the app animating
           over somebody who is still typing. The narrowing is legible
           without it. -->
      <div
        id="palette-list"
        role="listbox"
        aria-label="Results"
        class="max-h-[360px] overflow-y-auto p-1.5"
        use:staggerIn={{ selector: "button", each: 12, y: 4 }}
      >
        <!-- **Unkeyed, on purpose.** This block used to be keyed on
             `section + label + sub` — display text, over data a person
             controls — and two rows that happened to read the same took
             the whole palette down: Svelte refuses to render a keyed each
             with a duplicate key, the error lands in whatever effect set
             `open`, and the result is a palette that does not appear at
             all. Ctrl+K does nothing, the search field does nothing, and
             nothing else in the window looks wrong, so there is no
             suggestion anywhere that the fault is in a *list key*.

             It is not a hypothetical collision. `originsOf` names every
             bundle nobody has labelled "Unnamed snapshot" and folder
             origins by their basename, so a mesh holding two unlabelled
             snapshots of the same file count — or two folders pushed from
             different paths ending in the same word — produced two
             identical keys and lost its command palette. Which meshes hit
             it depended entirely on what had been pushed, which is why it
             reproduced on every fresh binary for the person it happened to
             and on none of ours.

             A semantic key would work today (the underlying ids are all
             unique) and would re-arm the trap the moment somebody adds a
             row from a new source. Index reconciliation cannot collide,
             and nothing here wants a keyed each anyway: `results` is
             derived wholesale, a row holds no state, `selected` is already
             an index, and the stagger above runs once on mount rather than
             per row. -->
        {#each results as action, i}
          {#if i === 0 || results[i - 1].section !== action.section}
            <div class="microlabel px-3 pt-2.5 pb-1.5" role="presentation" aria-hidden="true">{action.section}</div>
          {/if}
          <!-- Out of the Tab order: the field holds focus and the arrows
               move the highlight, which is what `aria-activedescendant`
               tells a screen reader. -->
          <button
            id={`palette-opt-${i}`}
            role="option"
            aria-selected={i === selected}
            tabindex="-1"
            class="relative flex h-[40px] w-full items-center gap-3 rounded-md px-3 text-left"
            style={i === selected ? "background: var(--accent-wash); color: var(--accent-ink);" : "color: var(--ink-2);"}
            onmouseenter={() => (selected = i)}
            onclick={(e) => {
              // Shift-click is the pointer's spelling of ⇧↵, so the two
              // input paths do not disagree about what the modifier means.
              if (e.shiftKey && action.cmd) {
                void copyCommand(action).then(() => (open = false));
                return;
              }
              action.run();
              open = false;
            }}
          >
            <span class="sr-only">{action.section}:</span>
            <span
              aria-hidden="true"
              class="grid h-6 w-6 flex-none place-items-center rounded-sm"
              style={`${action.icon in VIEW_HUE ? hueStyle(VIEW_HUE[action.icon as ViewName]) : ""} ${i === selected
                ? "background: var(--surface-1); color: var(--hue-ink, var(--accent-ink)); box-shadow: 0 0 0 1px var(--edge-card);"
                : "background: var(--hue-wash, var(--surface-2)); color: var(--hue-ink, var(--ink-4));"}`}
            >
              <Icon name={action.icon} size={14} />
            </span>
            <span class="truncate text-base">{action.label}</span>
            {#if action.sub}
              <span class="mono truncate text-xs" style={i === selected ? "color: var(--accent-ink); opacity: .75;" : "color: var(--ink-5);"}>{action.sub}</span>
            {/if}
            <!-- The command, right-aligned in mono: mono to paste, sans to
                 read (the design pass's rule), and right-aligned so a
                 column of them reads as one list of commands rather than
                 as ragged trailing text. -->
            {#if action.cmd}
              <span
                class="mono ml-auto hidden max-w-[46%] flex-none truncate text-2xs sm:block"
                style={i === selected ? "color: var(--accent-ink); opacity: .8;" : "color: var(--ink-faint);"}
                >{action.cmd}</span
              >
            {/if}
            {#if action.keycap}
              <span class="kbd flex-none" class:ml-auto={!action.cmd} aria-hidden="true">{action.keycap}</span>
            {:else if i === selected}
              <span class="kbd flex-none" class:ml-auto={!action.cmd} aria-hidden="true">{action.cmd ? "⇧↵" : "↵"}</span>
            {/if}
          </button>
        {:else}
          <div class="px-3 py-5 text-center text-sm text-ink-5" role="presentation">Nothing matches "{query}"</div>
        {/each}
      </div>
      <div
        class="flex items-center gap-4 px-4 py-2 text-[11px] text-ink-muted"
        style="border-top: 1px solid var(--edge); background: var(--surface-subtle);"
      >
        <span class="flex items-center gap-1.5"><span class="kbd">↑</span><span class="kbd">↓</span> navigate</span>
        <span class="flex items-center gap-1.5"><span class="kbd">↵</span> open</span>
        <!-- Both keys, documented where the other three are (L2 P3). A
             shortcut nobody is told about is a shortcut nobody uses. -->
        <span class="flex items-center gap-1.5"
          ><span class="kbd">⇧</span><span class="kbd">↵</span> copy the command</span
        >
        <span class="flex items-center gap-1.5"><span class="kbd">esc</span> close</span>
        <span class="ml-auto num" role="status">{results.length} result{results.length === 1 ? "" : "s"}</span>
      </div>
    </div>
  </div>
{/if}
