<script lang="ts">
  // Library (MASTERPLAN §14.2). One primary noun: a **script** — a named,
  // versioned, runnable thing (the engine calls it a service).
  //
  //   Scripts  — what you can run, what it runs, what it takes.
  //   Sources  — where scripts come from: Git repos and folders.
  //
  // Layout rules this view holds itself to:
  //   · Panels fill the viewport height. Content never floats in a void.
  //   · Every clickable row is a real button with a hover state, a cursor
  //     and an affordance glyph — nothing is a mystery target.
  //   · A folder header is a *container*, not a peer of the files inside it:
  //     it gets a sunken band, an icon and a count, and its files indent.
  //   · A BLAKE3 bundle id is never a name. Unnamed content is labelled as
  //     such and tucked behind a disclosure.
  import {
    api,
    errText,
    fmtBytes,
    fmtDuration,
    type BundleFileView,
    type PickedView,
    type ServiceView,
  } from "../lib/api";
  import {
    ARG_KINDS,
    buildContract,
    contractStatus,
    emptyArg,
    isDefinitionsPath,
    parseContract,
    RUNTIMES,
    runtimeMeta,
    runtimeProgram,
    splitOptions,
    type ArgSpec,
  } from "../lib/contract";
  import {
    KIND_ICON,
    KIND_LABEL,
    basename,
    forgetFolder,
    originsOf,
    rememberFolder,
    repoNameFromUrl,
    splitOrigins,
    type Origin,
  } from "../lib/origins";
  import { app, ago, ui } from "../lib/state.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Spinner from "../lib/components/Spinner.svelte";
  import LoadError from "../lib/components/LoadError.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Tag from "../lib/components/Tag.svelte";
  import Distribution from "../lib/components/Distribution.svelte";
  import BuildFarm from "./BuildFarm.svelte";

  // **R6 P1.** The Build farm moved here from Run. It was never a run
  // mode — it is the setup panel for `meshnode cc`, a thing you point your
  // build system at once and then never open again — and a tab you never
  // press Run on sitting beside four that you do is how a page teaches
  // people to distrust its own labels.
  type Tab = "scripts" | "sources" | "distribution" | "farm";
  let tab = $state<Tab>("scripts");
  // Where an old `run:tab` link lands now that the Library and the Build
  // farm live here (R6 P1).
  $effect(() => {
    const pre = sessionStorage.getItem("library:tab");
    if (!pre) return;
    sessionStorage.removeItem("library:tab");
    if (["scripts", "sources", "distribution", "farm"].includes(pre)) tab = pre as Tab;
  });

  const origins = $derived(originsOf(app.sources, app.bundles));
  const named = $derived(splitOrigins(origins).named);
  const unnamed = $derived(splitOrigins(origins).unnamed);
  const canContent = $derived(
    app.whoami?.isFounder || app.whoami?.roles.some((r) => r === "admin" || r === "developer"),
  );

  const rt = runtimeMeta;

  // ===========================================================================
  // What is actually worth running
  // ===========================================================================
  //
  // `kinds` from the daemon is an honest statement about a file *extension*:
  // every `.yml` is "ansible", every mode bit is "exec". That is the right
  // answer to "what could this be" and the wrong answer to "what should I put
  // in front of somebody who came here to add a script" — a normal repo
  // answers the first question with its CI config, its docker-compose, its
  // pre-commit hooks and its lockfiles, and a picker that lists all of those
  // as runnable is a picker people scroll past instead of read.
  //
  // So this ranks rather than filters, in three tiers, and every row says in
  // its own words why it landed where it did. Nothing is ever hidden from
  // *search* — the tiers decide what you are shown before you have said what
  // you want, which is the only moment the guess matters.

  type Tier = "entry" | "script" | "support";
  type Verdict = { tier: Tier; runtime: string; why: string };
  type Ranked = { f: BundleFileView; v: Verdict };

  /** Directories that are somebody's tooling, never somebody's automation. */
  const NOISE_DIR =
    /(^|\/)(\.git|\.github|\.gitlab|\.circleci|\.vscode|\.idea|node_modules|bower_components|vendor|target|dist|build|coverage|__pycache__|\.venv|venv|env|\.tox|\.mypy_cache|\.pytest_cache|\.ruff_cache|site-packages|\.terraform)(\/|$)/;
  /** Products and payloads: lockfiles, binaries, media, anything compiled. */
  const NOISE_NAME =
    /(^\.)|\.(lock|log|map|pyc|pyo|so|dylib|dll|o|a|class|jar|war|zip|tar|gz|tgz|bz2|xz|rar|7z|png|jpe?g|gif|bmp|svg|ico|webp|mp4|mov|woff2?|ttf|otf|eot|pdf|db|sqlite3?|bin|exe)$/i;
  /** YAML that is configuration for some *other* tool. The extension says
   *  ansible; the filename says otherwise, and the filename is right. */
  const NOT_A_PLAYBOOK =
    /^(docker-compose|compose|gitlab-ci|travis|pre-commit-config|cloudbuild|serverless|skaffold|kustomization|values|chart|codecov|renovate|netlify|vercel|appveyor|azure-pipelines|openapi|swagger|schema|config|settings|ci|cd|release-please|dependabot|mkdocs|pubspec|environment)(\.|$)/i;
  /** Names that read as "start here" in every automation repo ever written. */
  const ENTRY_NAME =
    /^(run|main|deploy|setup|install|uninstall|build|start|stop|restart|reload|entrypoint|bootstrap|provision|configure|backup|restore|healthcheck|check|migrate|rollout|rollback|release|publish|sync|cleanup|purge|rotate|report|collect)([-_.].*)?$/i;
  /** Directories people put the things they mean to run into. */
  const ENTRY_DIR = /(^|\/)(scripts?|bin|playbooks?|automation|ops|runbooks?|tasks|tools|jobs)$/i;
  const PLAYBOOK_NAME = /^(site|playbook|main|deploy|provision|setup|install|configure)\.ya?ml$/i;

  /** A file nobody came here to look at. Kept out of every tier, but still
   *  reachable by search — "I know it is in there" has to keep working. */
  function isNoise(f: BundleFileView): boolean {
    return NOISE_DIR.test(f.path) || NOISE_NAME.test(f.name);
  }

  const stemOf = (name: string) => name.replace(/\.[^.]+$/, "");
  const lastSeg = (dir: string) => dir.split("/").filter(Boolean).pop() ?? "";

  function classify(f: BundleFileView): Verdict {
    if (f.symlink) return { tier: "support", runtime: "bash", why: "a symlink" };

    const stem = stemOf(f.name);
    const inEntryDir = ENTRY_DIR.test(f.dir);
    const namedLikeEntry = ENTRY_NAME.test(stem);

    // Ansible first: it is the one where the extension lies most often.
    if (f.kinds.includes("ansible")) {
      if (PLAYBOOK_NAME.test(f.name) || /(^|\/)playbooks?$/i.test(f.dir))
        return { tier: "entry", runtime: "ansible", why: "playbook" };
      if (NOT_A_PLAYBOOK.test(stem) || f.dir.startsWith("."))
        return { tier: "support", runtime: "ansible", why: "YAML config, not a playbook" };
      if (namedLikeEntry && inEntryDir)
        return { tier: "entry", runtime: "ansible", why: `playbook in ${lastSeg(f.dir)}/` };
      return { tier: "script", runtime: "ansible", why: "YAML — check it is a playbook" };
    }

    for (const [kind, runtime, label] of [
      ["bash", "bash", "shell script"],
      ["python", "python", "Python script"],
    ] as const) {
      if (!f.kinds.includes(kind)) continue;
      if (f.executable) return { tier: "entry", runtime, why: "executable" };
      if (namedLikeEntry) return { tier: "entry", runtime, why: "named like an entry point" };
      if (inEntryDir) return { tier: "entry", runtime, why: `in ${lastSeg(f.dir)}/` };
      return { tier: "script", runtime, why: label };
    }

    // An executable with no extension Meshware recognises: a compiled tool,
    // or a script whose shebang is the only thing that says what it is.
    if (f.executable) return { tier: "entry", runtime: "exec", why: "executable" };

    if (f.kinds.includes("contract"))
      return { tier: "support", runtime: "bash", why: "JSON — data, not a program" };
    if (f.kinds.includes("text"))
      return { tier: "support", runtime: "bash", why: "notes or configuration" };
    return { tier: "support", runtime: "bash", why: "not executable" };
  }

  function guessRuntime(f: BundleFileView): string {
    return classify(f).runtime;
  }
  function suggestName(f: BundleFileView): string {
    return f.name
      .replace(/\.(sh|bash|py|yml|yaml)$/i, "")
      .replace(/[^a-zA-Z0-9_.-]+/g, "-")
      .toLowerCase();
  }
  /** A container has no file to name it after, so the source does. */
  function suggestNameFromSource(name: string): string {
    return name
      .split("/")
      .filter(Boolean)
      .pop()!
      .replace(/[^a-zA-Z0-9_.-]+/g, "-")
      .toLowerCase();
  }
  /** Grouped by folder, and within a folder the ones worth running float to
   *  the top — a directory with one deploy script and nine fixtures should
   *  not make you hunt for the deploy script. */
  const TIER_RANK: Record<Tier, number> = { entry: 0, script: 1, support: 2 };
  type DirGroup = {
    dir: string;
    /** Everything above the last segment, with its trailing slash: the part
     *  that is context rather than identity, and is dimmed to say so. */
    lead: string;
    /** The folder's own name — what you actually scan the column for. */
    leaf: string;
    /** How deep it sits, capped so a Java-shaped tree does not indent its
     *  files off the right edge of the panel. */
    depth: number;
    rows: Ranked[];
    runnable: number;
    /** One entry per runtime present, biggest share first: the folder's
     *  contents readable before it is opened. */
    runtimes: { runtime: string; n: number }[];
  };
  function groupByDir(rows: Ranked[]): DirGroup[] {
    const map = new Map<string, Ranked[]>();
    for (const r of rows) map.set(r.f.dir, [...(map.get(r.f.dir) ?? []), r]);
    return [...map.entries()]
      .sort(([a], [b]) => (a === "" ? -1 : b === "" ? 1 : a.localeCompare(b)))
      .map(([dir, rs]) => {
        const segs = dir.split("/").filter(Boolean);
        const counts = new Map<string, number>();
        for (const r of rs)
          if (r.v.tier !== "support")
            counts.set(r.v.runtime, (counts.get(r.v.runtime) ?? 0) + 1);
        return {
          dir,
          lead: segs.length > 1 ? segs.slice(0, -1).join("/") + "/" : "",
          leaf: segs.length ? segs[segs.length - 1] : "top level",
          depth: Math.min(segs.length, 4),
          rows: [...rs].sort(
            (x, y) =>
              TIER_RANK[x.v.tier] - TIER_RANK[y.v.tier] || x.f.name.localeCompare(y.f.name),
          ),
          runnable: rs.filter((r) => r.v.tier !== "support").length,
          runtimes: [...counts.entries()]
            .map(([runtime, n]) => ({ runtime, n }))
            .sort((a, b) => b.n - a.n || a.runtime.localeCompare(b.runtime)),
        };
      });
  }

  // -- which folders are open ---------------------------------------------------
  //
  // A folder header is a container, so it should behave like one: it opens
  // and it closes. Collapsed is the exception rather than the rule — the
  // point of the picker is to see the files — so this records what is *shut*
  // and everything absent from it is open. A folder with nothing runnable in
  // it is the one worth shutting first, which is what "Collapse empties"
  // does in one click.
  let shutDirs = $state<Set<string>>(new Set());
  const isShut = (dir: string) => shutDirs.has(dir);
  function toggleDir(dir: string) {
    const next = new Set(shutDirs);
    if (!next.delete(dir)) next.add(dir);
    shutDirs = next;
  }
  function shutAll(groups: DirGroup[], only: "empty" | "all") {
    shutDirs = new Set(
      groups.filter((g) => only === "all" || g.runnable === 0).map((g) => g.dir),
    );
  }
  const openAll = () => (shutDirs = new Set());

  // -- bundle file cache --------------------------------------------------------
  let filesByBundle = $state<Record<string, BundleFileView[]>>({});
  let filesLoading = $state<string | null>(null);
  let filesErr = $state<Record<string, string>>({});

  async function loadFiles(bundleId: string | null, force = false) {
    if (!bundleId || (!force && filesByBundle[bundleId])) return;
    filesLoading = bundleId;
    try {
      const files = await api.bundleFiles(bundleId);
      filesByBundle = { ...filesByBundle, [bundleId]: files };
      const { [bundleId]: _gone, ...rest } = filesErr;
      filesErr = rest;
    } catch (e) {
      filesErr = { ...filesErr, [bundleId]: errText(e) };
    } finally {
      if (filesLoading === bundleId) filesLoading = null;
    }
  }

  // ===========================================================================
  // Add a script — where · which file · details
  // ===========================================================================
  let wizard = $state(false);
  let step = $state<1 | 2 | 3>(1);
  let originKey = $state<string | null>(null);
  let fallbackBundleId = $state<string | null>(null);
  /** Which tier the picker is showing. Replaces a Runnable/All toggle whose
   *  "Runnable" half was, in any real repository, most of the repository. */
  let fileMode = $state<"suggested" | "scripts" | "all">("suggested");
  let fileQuery = $state("");
  let showUnnamed = $state(false);

  let sName = $state("");
  let sRuntime = $state<string>("bash");
  let sWorkingDir = $state("");
  let sEntrypoint = $state("");
  /** The container image, kept **separate** from `sEntrypoint` (N4).
   *
   *  They are different kinds of thing — a file inside the bundle versus a
   *  reference to an image — and sharing one box meant switching the
   *  runtime to Container silently left the file you picked on step 2
   *  sitting in the Image field. `index.html` is a plausible-enough image
   *  reference to get published, and then every instance fails to pull. */
  let sImage = $state("");
  /** What `entrypoint` actually means for the runtime in force. */
  const entrypointNow = $derived(sRuntime === "container" ? sImage : sEntrypoint);
  // N4 — the container command override. Parity with
  // `service create --command / --arg`, and the same reserved keys on the
  // other side, so a service made here and one made in the terminal are
  // the same record.
  let sCommand = $state("");
  let sCmdArgs = $state("");
  const cmdArgs = $derived(
    sCmdArgs
      .split("\n")
      .map((a) => a.trim())
      .filter(Boolean),
  );
  let sDescription = $state("");
  let sTags = $state("");
  let sArgs = $state<ArgSpec[]>([]);
  let sRaw = $state<string | null>(null);
  /** Root schema keywords the row editor does not model, preserved verbatim. */
  let sRootExtra = $state<Record<string, unknown>>({});
  let sEnv = $state<{ k: string; v: string }[]>([]);
  let sEnvOpen = $state(false);
  let sAdvOpen = $state(false);
  let sBusy = $state(false);
  let sErr = $state<string | null>(null);
  let updating = $state<string | null>(null);

  let browsing = $state<string | null>(null);

  const origin = $derived(origins.find((o) => o.key === originKey) ?? null);
  const bundleId = $derived(origin?.bundleId ?? fallbackBundleId);
  const browsed = $derived(origins.find((o) => o.key === browsing) ?? null);

  // One file browser, one piece of state: the wizard owns it while it is open,
  // the Sources panel owns it otherwise. They are never visible together.
  const activeBundle = $derived(wizard ? bundleId : (browsed?.bundleId ?? null));
  const allFiles = $derived(
    activeBundle
      ? (filesByBundle[activeBundle] ?? []).filter(
          // A definitions file is never an entrypoint — it is what the file
          // beside it declares. Showing it as something you could run is
          // offering a choice with a wrong answer in it.
          (f) => !f.symlink && !isDefinitionsPath(f.path),
        )
      : [],
  );
  const ranked = $derived<Ranked[]>(allFiles.map((f) => ({ f, v: classify(f) })));
  /** Everything a person would recognise as *their* files. */
  const signal = $derived(ranked.filter((r) => !isNoise(r.f)));
  const tierCount = $derived({
    entry: signal.filter((r) => r.v.tier === "entry").length,
    script: signal.filter((r) => r.v.tier !== "support").length,
    all: signal.length,
  });

  const pickable = $derived.by(() => {
    const q = fileQuery.trim().toLowerCase();
    // Typing a query is a statement that you already know what you want, so
    // it searches the whole bundle — noise, tooling directories and all —
    // rather than whichever tier happens to be selected. The tiers only
    // decide what you see *before* you have said anything.
    if (q) return ranked.filter((r) => r.f.path.toLowerCase().includes(q));
    if (fileMode === "suggested") return signal.filter((r) => r.v.tier === "entry");
    if (fileMode === "scripts") return signal.filter((r) => r.v.tier !== "support");
    return signal;
  });

  const chosenPath = $derived(sWorkingDir ? `${sWorkingDir}/${sEntrypoint}` : sEntrypoint);
  const nameTaken = $derived(app.services.some((s) => s.name === sName.trim()));
  /** A script is run *by name*, at a prompt as well as here — so the name
   *  is one word: letters, digits, dots, dashes and underscores. What is
   *  wrong with it, in words, or null when nothing is. */
  const nameProblem = $derived.by((): string | null => {
    const n = sName.trim();
    if (!n) return null;
    if (/\s/.test(n)) return "no spaces — people type this at a prompt";
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(n)) return "letters, digits, dots, dashes and underscores only";
    if (n.length > 64) return "keep it under 64 characters";
    return null;
  });
  const secretNames = $derived(app.secrets.filter((s) => !s.removed).map((s) => s.name));

  $effect(() => {
    void loadFiles(activeBundle);
  });

  function resetWizard() {
    step = 1;
    originKey = null;
    fallbackBundleId = null;
    fileMode = "suggested";
    fileQuery = "";
    sName = sWorkingDir = sEntrypoint = sImage = sDescription = sTags = "";
    sCommand = sCmdArgs = "";
    sRuntime = "bash";
    sArgs = [];
    sRaw = null;
    sRootExtra = {};
    sInspected = null;
    sDefs = sDefsSaved = null;
    sDefsOpen = sDefsBusy = false;
    sDefsErr = sReadErr = null;
    sEnv = [];
    sEnvOpen = sAdvOpen = false;
    sErr = null;
    updating = null;
  }

  function openWizard(at: Origin | null = null) {
    resetWizard();
    wizard = true;
    tab = "scripts";
    if (at) chooseOrigin(at);
  }

  function chooseOrigin(o: Origin) {
    originKey = o.key;
    fallbackBundleId = null;
    fileQuery = "";
    step = 2;
    void loadFiles(o.bundleId);
  }

  /** What the chosen file said about itself, for the Details step. */
  let sInspected = $state<import("../lib/bindings/ScriptSpecView").ScriptSpecView | null>(null);

  async function chooseFile(f: BundleFileView) {
    sWorkingDir = f.dir;
    sEntrypoint = f.name;
    sRuntime = guessRuntime(f);
    if (!sName.trim()) sName = suggestName(f);
    step = 3;

    // Read the script's own `#meshware:` header — the same parser
    // `meshnode add` uses (R2 P4).
    //
    // Without this the wizard set a name, a runtime and an entrypoint and
    // left Arguments empty, so a script added here registered with *no
    // declared arguments* and its Run screen had no fields — while the same
    // file added with `meshnode add` came out with all of them. Two doors,
    // two answers, and the friendlier-looking door did less.
    sInspected = null;
    sDefs = null;
    sDefsErr = null;
    sReadErr = null;
    if (!bundleId) return;
    try {
      const spec = await api.scriptInspect(bundleId, f.dir ? `${f.dir}/${f.name}` : f.name);
      sInspected = spec;
      sDefs = spec.definitions;
      sDefsSaved = spec.definitions;
      if (spec.name && !updating) sName = spec.name;
      if (spec.about) sDescription = spec.about;
      if (spec.contract) {
        const p = parseContract(spec.contract);
        if (p.rows) {
          sArgs = p.rows;
          sRootExtra = p.rootExtra;
        } else {
          sRaw = spec.contract;
        }
      }
    } catch (e) {
      // The wizard still works with nothing pre-filled — but it says so.
      // Swallowing this is how an unreachable bundle used to arrive on
      // screen as a script that simply declared no arguments.
      sReadErr = errText(e);
    }
  }

  // -- the definitions file (R2 P5) --------------------------------------------
  //
  // A `#meshware:` header is a small language invented to fit on one line,
  // and it stops where one line stops. A script may instead carry
  // `NAME.definitions` beside it — the same declaration as JSON, with room
  // for the things a comment has no room for. This is the editor for it:
  // what the script declares, in the file it declares it in, saved back to
  // the folder the bundle was pushed from.

  /** The definitions JSON as it is being edited. */
  let sDefs = $state<string | null>(null);
  /** What is on disk, so the editor knows whether it has changes. */
  let sDefsSaved = $state<string | null>(null);
  let sDefsOpen = $state(false);
  let sDefsBusy = $state(false);
  let sDefsErr = $state<string | null>(null);
  /** Why the script itself could not be read. */
  let sReadErr = $state<string | null>(null);

  /** Where a definitions file would be written, or null if we cannot. */
  const defsTarget = $derived.by(() => {
    const root = origin?.localPath ?? null;
    const bundleId = origin?.bundleId ?? null;
    if (!root || !bundleId || !sInspected) return null;
    // `root` is shown; the backend writes into the folder it recorded for
    // the bundle, never one named here (SEC-14).
    return { root, bundleId, entry: chosenPath, path: sInspected.definitionsPath };
  });
  const defsDirty = $derived(sDefs !== null && sDefs !== sDefsSaved);

  /** Does this JSON parse, and does it name the arguments it should? */
  const defsStatus = $derived.by(() => {
    if (sDefs === null || !sDefs.trim()) return null;
    try {
      const v: unknown = JSON.parse(sDefs);
      if (typeof v !== "object" || v === null || Array.isArray(v)) {
        return { ok: false, text: "the top level must be a JSON object" };
      }
      const args = (v as { args?: unknown }).args;
      const n = Array.isArray(args)
        ? args.length
        : args && typeof args === "object"
          ? Object.keys(args).length
          : 0;
      return { ok: true, text: `${n} argument${n === 1 ? "" : "s"}` };
    } catch (e) {
      return { ok: false, text: e instanceof Error ? e.message : String(e) };
    }
  });

  /**
   * Pull the form's own rows back into the JSON, so the two halves of this
   * screen never disagree. Pressing *Save* after editing an argument row
   * writes what is on screen, not what was on disk when it opened.
   */
  function definitionsFromForm(): string {
    const kindName = (a: ArgSpec) =>
      a.kind === "yesno" ? "bool" : a.kind === "integer" ? "int" : a.kind;
    const typed = (raw: string, kind: ArgSpec["kind"]): unknown => {
      const t = raw.trim();
      if (kind === "yesno") return t === "true" || t === "yes" || t === "1";
      if (kind === "integer" || kind === "number") {
        const n = Number(t);
        return Number.isFinite(n) ? n : t;
      }
      return t;
    };
    const out: Record<string, unknown> = {};
    if (sName.trim()) out.name = sName.trim();
    if (sDescription.trim()) out.about = sDescription.trim();

    // What the script declared about itself that is not an argument rides in
    // the contract's `x-meshware` block. Carry it across rather than drop it.
    const ext = sRootExtra["x-meshware"];
    if (ext && typeof ext === "object" && !Array.isArray(ext)) {
      const e = ext as Record<string, unknown>;
      if (typeof e.item === "string") out.item = e.item;
      const needs: Record<string, unknown> = {};
      for (const k of ["cores", "mem", "disk"]) if (e[k] !== undefined) needs[k] = e[k];
      if (Object.keys(needs).length) out.needs = needs;
      for (const k of ["tools", "reads", "writes"]) if (e[k] !== undefined) out[k] = e[k];
    }

    out.args = sArgs
      .filter((a) => a.name.trim())
      .map((a) => {
        const o: Record<string, unknown> = { name: a.name.trim(), type: kindName(a) };
        if (a.kind === "choice") o.choices = splitOptions(a.options);
        if (a.required) o.required = true;
        if (a.default.trim()) o.default = typed(a.default, a.kind);
        // Bounds and shape have no row of their own; they survive in the
        // schema keywords the row editor carries through untouched.
        const extra = a.extra as Record<string, unknown>;
        if (extra.minimum !== undefined) o.min = extra.minimum;
        if (extra.maximum !== undefined) o.max = extra.maximum;
        if (extra.pattern !== undefined) o.pattern = extra.pattern;
        if (a.description.trim()) o.description = a.description.trim();
        return o;
      });
    return `${JSON.stringify(out, null, 2)}\n`;
  }

  /** Replace the argument rows with what the JSON says. */
  function applyDefinitions() {
    if (!bundleId || sDefs === null) return;
    sDefsErr = null;
    void (async () => {
      sDefsBusy = true;
      try {
        // Compiled by the same parser that will read the file back off disk,
        // so what the form shows is what the file means — not a second
        // implementation in TypeScript that agrees with it most of the time.
        const spec = await api.scriptDefinitionsPreview(sDefs);
        if (spec.name && !updating) sName = spec.name;
        if (spec.about) sDescription = spec.about;
        if (spec.contract) {
          const p = parseContract(spec.contract);
          if (p.rows) {
            sArgs = p.rows;
            sRootExtra = p.rootExtra;
            sRaw = null;
          } else sRaw = spec.contract;
        } else {
          sArgs = [];
          sRootExtra = {};
        }
        sInspected = spec;
      } catch (e) {
        sDefsErr = errText(e);
      } finally {
        sDefsBusy = false;
      }
    })();
  }

  async function saveDefinitions() {
    const target = defsTarget;
    if (!target || sDefs === null) return;
    sDefsBusy = true;
    sDefsErr = null;
    try {
      const written = await api.scriptDefinitionsSave(target.bundleId, target.entry, sDefs);
      sDefsSaved = sDefs;
      app.flash(`saved ${written}`);
    } catch (e) {
      sDefsErr = errText(e);
    } finally {
      sDefsBusy = false;
    }
  }

  function editScript(s: ServiceView) {
    resetWizard();
    updating = s.name;
    sName = s.name;
    sRuntime = s.runtime;
    sWorkingDir = s.workingDir;
    // A container's stored entrypoint is an image; anything else is a file.
    if (s.runtime === "container") {
      sImage = s.entrypoint;
      sEntrypoint = "";
    } else {
      sEntrypoint = s.entrypoint;
      sImage = "";
    }
    sDescription = s.description;
    sTags = s.tags.join(", ");
    originKey = origins.find((o) => o.bundleId === s.bundleId)?.key ?? null;
    fallbackBundleId = s.bundleId;
    const { rows, rootExtra } = parseContract(s.contract);
    if (rows) {
      sArgs = rows;
      // What the script declared about itself — its item input, its
      // per-item cores, its reads and writes — is not an argument and has
      // no row. Held here so editing an argument does not delete it.
      sRootExtra = rootExtra;
    } else sRaw = s.contract ?? "";
    wizard = true;
    tab = "scripts";
    step = 3;
  }

  const compiledContract = $derived(
    sRaw !== null ? (sRaw.trim() ? sRaw.trim() : null) : buildContract(sArgs, sRootExtra),
  );
  const rawStatus = $derived(sRaw !== null ? contractStatus(sRaw) : null);
  const namedArgs = $derived(sArgs.filter((a) => a.name.trim()).length);
  const namedEnv = $derived(sEnv.filter((r) => r.k.trim()).length);

  /** The command line this will run — live, and the whole point of step 3. */
  const preview = $derived.by(() => {
    const program = runtimeProgram(sRuntime, entrypointNow || "…");
    const parts = sArgs
      .filter((a) => a.name.trim())
      .map((a) => ({
        text: `--${a.name.trim()} ${a.default.trim() || (a.kind === "choice" ? (splitOptions(a.options)[0] ?? "…") : `<${a.name.trim()}>`)}`,
        required: a.required,
      }));
    return { program, parts };
  });

  const canSave = $derived(
    !sBusy && !!sName.trim() && !nameProblem && !!entrypointNow.trim() && !!bundleId && !(rawStatus && !rawStatus.ok),
  );

  async function save() {
    sBusy = true;
    sErr = null;
    try {
      if (rawStatus && !rawStatus.ok) throw new Error(`arguments: ${rawStatus.text}`);
      if (!bundleId) throw new Error("pick where this script comes from first");
      const version = await api.serviceCreate({
        name: sName.trim(),
        runtime: sRuntime,
        bundleId,
        workingDir: sWorkingDir.trim(),
        entrypoint: entrypointNow.trim(),
        command: sRuntime === "container" && sCommand.trim() ? sCommand.trim() : null,
        args: sRuntime === "container" && cmdArgs.length > 0 ? cmdArgs : null,
        contract: compiledContract,
        env: sEnv.filter((r) => r.k.trim()).map((r) => [r.k.trim(), r.v] as [string, string]),
        description: sDescription.trim(),
        tags: sTags.split(",").map((t) => t.trim()).filter(Boolean),
      });
      const saved = sName.trim();
      app.flash(version === 1 ? `'${saved}' is ready to run` : `'${saved}' updated to v${version}`);
      wizard = false;
      selectedName = saved;
      resetWizard();
      await app.refresh();
    } catch (e) {
      sErr = errText(e);
    } finally {
      sBusy = false;
    }
  }

  // ===========================================================================
  // Connect a repo / add a folder
  // ===========================================================================
  const INTERVALS = [
    [60, "every minute"],
    [300, "every 5 minutes"],
    [900, "every 15 minutes"],
    [3600, "every hour"],
    [21600, "every 6 hours"],
    [86400, "once a day"],
  ] as const;

  let repoOpen = $state(false);
  let rUrl = $state("");
  let rBranch = $state("main");
  let rInterval = $state(300);
  let rName = $state("");
  let rMore = $state(false);
  let rBusy = $state(false);
  let rStage = $state<string | null>(null);
  let rErr = $state<string | null>(null);

  const rDerivedName = $derived(rName.trim() || repoNameFromUrl(rUrl));
  const canConnect = $derived(!rBusy && !!rUrl.trim() && !!rDerivedName);

  function openRepo() {
    rUrl = "";
    rBranch = "main";
    rInterval = 300;
    rName = "";
    rMore = false;
    rErr = null;
    rStage = null;
    repoOpen = true;
  }

  /** Register, then read it once immediately — otherwise there is nothing to
      browse until a watcher happens to poll, and the flow dead-ends. */
  async function connectRepo() {
    const name = rDerivedName;
    if (!name || !rUrl.trim()) return;
    rBusy = true;
    rErr = null;
    try {
      rStage = "Registering…";
      await api.sourceAdd({
        name,
        url: rUrl.trim(),
        branch: rBranch.trim() || "main",
        intervalSecs: rInterval,
        bump: [],
      });
      rStage = "Cloning and reading it…";
      try {
        await api.sourceSync(name);
      } catch (e) {
        rErr = `Registered '${name}', but reading it failed: ${errText(e)}`;
      }
      await app.refreshAfterChange();
      if (!rErr) {
        repoOpen = false;
        app.flash(`'${name}' connected`);
        const added = originsOf(app.sources, app.bundles).find((o) => o.key === `git:${name}`);
        if (added && wizard) chooseOrigin(added);
        else if (added) browsing = added.key;
      }
    } catch (e) {
      rErr = errText(e);
    } finally {
      rBusy = false;
      rStage = null;
    }
  }

  // -- drop a file in (R2 P4) --------------------------------------------------
  //
  // Adding a script was a three-step wizard, or `meshnode add` in a
  // terminal. Neither is wrong; both are more ceremony than "here is the
  // file". The drop arrives from the Rust side as a pick (`mw-dropped`,
  // SEC-14), so a dropped script takes exactly the same route as one picked
  // from a folder — push, infer the contract from its own header, show it
  // for confirmation. No second implementation to drift.
  let dropping = $state(false);
  let dropBusy = $state(false);

  $effect(() => {
    const stops: (() => void)[] = [];
    let live = true;
    const keep = (stop: () => void) => (live ? stops.push(stop) : stop());
    void (async () => {
      try {
        const { getCurrentWebview } = await import("@tauri-apps/api/webview");
        keep(
          await getCurrentWebview().onDragDropEvent((ev) => {
            dropping = ev.payload.type === "over";
          }),
        );
        keep(await api.onDropped((picks) => void acceptDropped(picks)));
      } catch {
        // Not running under Tauri (the mock browser build): the picker and
        // the CLI are still there, and nothing here is load-bearing.
      }
    })();
    return () => {
      live = false;
      for (const stop of stops) stop();
    };
  });

  async function acceptDropped(picks: PickedView[]) {
    const pick = picks[0];
    if (!pick || dropBusy) return;
    const first = pick.path;
    dropBusy = true;
    try {
      const r = await api.pushBundle(pick.token);
      rememberFolder(r.bundleId, first);
      app.flash(`added ${r.fileCount} file${r.fileCount === 1 ? "" : "s"} from ${first}`);
      await app.refreshAfterChange();
      const added = originsOf(app.sources, app.bundles).find((o) => o.bundleId === r.bundleId);
      if (added) {
        // Straight into the wizard on the file that was dropped, so what it
        // declares is the next thing seen and confirmed.
        tab = "scripts";
        wizard = true;
        chooseOrigin(added);
        // A dropped script is a bundle of that script and its definitions
        // file, so there is nothing to pick: stopping at the file step to
        // choose between one file and its own sidecar is a step that only
        // ever has one right answer.
        const files = await api.bundleFiles(r.bundleId);
        const dropped = basename(first);
        const only = files.filter((f) => !f.symlink && !isDefinitionsPath(f.path));
        const pick = only.find((f) => f.path === dropped) ?? (only.length === 1 ? only[0] : null);
        if (pick) await chooseFile(pick);
      }
    } catch (e) {
      app.flash(errText(e));
    } finally {
      dropBusy = false;
    }
  }

  let folderBusy = $state(false);
  async function addFolder() {
    folderBusy = true;
    try {
      const picked = await api.pickDirectory("Choose a folder of scripts");
      if (!picked) return;
      const dir = picked.path;
      const r = await api.pushBundle(picked.token);
      rememberFolder(r.bundleId, dir);
      app.flash(`added ${r.fileCount} file${r.fileCount === 1 ? "" : "s"} from ${dir}`);
      await app.refreshAfterChange();
      const added = originsOf(app.sources, app.bundles).find((o) => o.bundleId === r.bundleId);
      if (added && wizard) chooseOrigin(added);
      else if (added) {
        tab = "sources";
        browsing = added.key;
      }
    } catch (e) {
      app.flash(errText(e));
    } finally {
      folderBusy = false;
    }
  }

  // ===========================================================================
  // Lists
  // ===========================================================================
  let selectedName = $state<string | null>(null);
  let scriptQuery = $state("");
  const selected = $derived(app.services.find((s) => s.name === selectedName) ?? null);
  const selectedArgs = $derived(parseContract(selected?.contract ?? null).rows);
  const selectedOrigin = $derived(origins.find((o) => o.bundleId === selected?.bundleId) ?? null);
  const shownScripts = $derived.by(() => {
    const q = scriptQuery.trim().toLowerCase();
    if (!q) return app.services;
    return app.services.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.description.toLowerCase().includes(q) ||
        s.entrypoint.toLowerCase().includes(q) ||
        s.tags.some((t) => t.toLowerCase().includes(q)),
    );
  });

  /** total < 0 means "a schema the row editor cannot summarise". */
  function argSummary(s: ServiceView): { total: number; required: number } | null {
    if (!s.hasContract) return null;
    const rows = parseContract(s.contract).rows;
    if (!rows) return { total: -1, required: 0 };
    return { total: rows.length, required: rows.filter((r) => r.required).length };
  }

  function runIt(name: string) {
    sessionStorage.setItem("run:preselect", name);
    app.go("run");
  }

  let rollBusy = $state(false);
  async function rollback(s: ServiceView, version: number) {
    rollBusy = true;
    try {
      await api.serviceRollback(s.name, version);
      app.flash(`'${s.name}' is back on v${version}`);
      await app.refresh();
    } catch (e) {
      app.flash(errText(e));
    } finally {
      rollBusy = false;
    }
  }

  let retireTarget = $state<ServiceView | null>(null);
  let retireBusy = $state(false);
  let retireErr = $state<string | null>(null);
  async function doRetire() {
    if (!retireTarget) return;
    retireBusy = true;
    retireErr = null;
    try {
      await api.serviceRemove(retireTarget.name);
      app.flash(`'${retireTarget.name}' removed`);
      if (selectedName === retireTarget.name) selectedName = null;
      retireTarget = null;
      await app.refresh();
    } catch (e) {
      retireErr = errText(e);
    } finally {
      retireBusy = false;
    }
  }

  // -- sources ------------------------------------------------------------------
  let syncing = $state<string | null>(null);
  let removeTarget = $state<Origin | null>(null);
  let removeBusy = $state(false);
  let removeErr = $state<string | null>(null);

  async function syncNow(o: Origin) {
    if (!o.git) return;
    syncing = o.key;
    try {
      const r = await api.sourceSync(o.git.name);
      if (r.unchanged) app.flash(`'${o.name}' is already up to date`);
      else
        app.flash(
          `'${o.name}' updated to ${r.commit.slice(0, 10)}` +
            (r.bumped.length ? ` · ${r.bumped.map((b) => `${b.service} → v${b.version}`).join(", ")}` : ""),
        );
      await app.refreshAfterChange();
      if (r.bundleId) void loadFiles(r.bundleId, true);
    } catch (e) {
      app.flash(errText(e));
    } finally {
      syncing = null;
    }
  }

  async function doRemove() {
    if (!removeTarget) return;
    removeBusy = true;
    removeErr = null;
    try {
      if (removeTarget.git) await api.sourceRemove(removeTarget.git.name);
      else if (removeTarget.bundleId) forgetFolder(removeTarget.bundleId);
      app.flash(`'${removeTarget.name}' removed`);
      if (browsing === removeTarget.key) browsing = null;
      removeTarget = null;
      await app.refreshAfterChange();
    } catch (e) {
      removeErr = errText(e);
    } finally {
      removeBusy = false;
    }
  }

  function scriptsOf(o: Origin): string[] {
    return app.services.filter((s) => s.bundleId === o.bundleId).map((s) => s.name);
  }

  // -- deep links ---------------------------------------------------------------
  $effect(() => {
    const t = sessionStorage.getItem("library:tab");
    if (t) {
      sessionStorage.removeItem("library:tab");
      tab = t as Tab;
    }
    const o = sessionStorage.getItem("library:open");
    if (o) {
      sessionStorage.removeItem("library:open");
      if (o === "add-script") openWizard();
      if (o === "add-repo") {
        tab = "sources";
        openRepo();
      }
      if (o === "add-folder") void addFolder();
    }
    const b = sessionStorage.getItem("library:origin");
    if (b) {
      sessionStorage.removeItem("library:origin");
      tab = "sources";
      browsing = b;
    }
    const s = sessionStorage.getItem("library:select");
    if (s) {
      sessionStorage.removeItem("library:select");
      tab = "scripts";
      selectedName = s;
    }
  });
</script>

<!-- ============================ shared pieces ============================ -->

{#snippet runtimeBadge(runtime: string, size: number)}
  {@const r = rt(runtime)}
  <span
    class="mono grid flex-none place-items-center rounded-sm font-bold"
    style={`width:${size}px;height:${size}px;font-size:${size <= 26 ? 9.5 : 11}px;background:${r.bg};color:${r.fg};`}
    title={r.label}>{r.short}</span
  >
{/snippet}

{#snippet originAvatar(kind: string, size: number)}
  <span
    class="grid flex-none place-items-center rounded-sm"
    style={`width:${size}px;height:${size}px;background:var(--surface-2);color:var(--ink-4);`}
  >
    <Icon name={KIND_ICON[kind as keyof typeof KIND_ICON] ?? "snapshot"} size={size <= 32 ? 16 : 18} />
  </span>
{/snippet}

<!-- A file list where a folder is visibly a container, files sit inside it,
     and every row states what it is rather than leaving you to infer it from
     an extension. -->
{#snippet fileBrowser(id: string, files: Ranked[], onpick: (f: BundleFileView) => void, action: string)}
  {#if filesLoading === id && files.length === 0}
    <div class="flex h-full items-center justify-center gap-2.5 text-sm text-ink-4">
      <Spinner size={16} color="var(--accent-ink)" track="var(--accent-wash)" />
      Reading the files…
    </div>
  {:else if filesErr[id]}
    <div class="flex h-full flex-col items-center justify-center gap-2 px-8 text-center">
      <span class="text-base font-semibold" style="color: var(--status-error);">Could not read this source</span>
      <span class="selectable mono text-xs text-ink-4">{filesErr[id]}</span>
    </div>
  {:else if files.length === 0}
    <div class="flex h-full flex-col items-center justify-center gap-1.5 px-8 text-center">
      <span class="text-ink-faint"><Icon name="inbox" size={26} /></span>
      <span class="text-base font-semibold text-ink-3">
        {fileQuery.trim()
          ? `Nothing matches “${fileQuery.trim()}”`
          : fileMode === "suggested"
            ? "Nothing here looks like an entry point"
            : "Nothing to show"}
      </span>
      <span class="max-w-[46ch] text-sm text-ink-5">
        {#if fileQuery.trim()}
          Search covers every file in the source, including the ones the tabs hide.
        {:else if fileMode === "suggested"}
          Meshware looks for executables, playbooks, and files named like something
          you would start — <span class="mono">deploy.sh</span>,
          <span class="mono">site.yml</span>. Try <span class="font-semibold">Scripts</span>
          for anything a runtime could take.
        {:else}
          This source has no files Meshware can read.
        {/if}
      </span>
    </div>
  {:else}
    {#each groupByDir(files) as g (g.dir)}
      {@const shut = isShut(g.dir)}
      <!-- The header *is* the container: a chevron that turns, the folder's
           own name in front of a dimmed path, and — while it is shut — what
           is inside it, so closing a folder does not hide what it holds. -->
      <button
        type="button"
        class="dirhead group/dir sticky top-0 z-10 flex w-full cursor-pointer items-center gap-2 border-b border-edge py-[7px] pr-3 text-left"
        class:shut
        style={`background: var(--surface-2); padding-left: ${10 + g.depth * 11}px;`}
        aria-expanded={!shut}
        onclick={() => toggleDir(g.dir)}
      >
        <span class={`chev flex-none text-ink-5 transition-transform duration-150 ${shut ? "" : "rotate-90"}`}>
          <Icon name="chevron-right" size={13} />
        </span>
        <span class={`flex-none transition-colors ${shut ? "text-ink-6" : "text-accent-ink"}`}>
          <Icon name="folder" size={14} />
        </span>
        <span class="mono min-w-0 truncate text-xs">
          {#if g.lead}<span class="text-ink-6">{g.lead}</span>{/if}<span class="font-semibold text-ink-2">{g.leaf}</span>
        </span>

        <!-- Right edge: what is in here, in the same badges the rows use. -->
        <span class="ml-auto flex flex-none items-center gap-1.5">
          {#if shut}
            {#each g.runtimes.slice(0, 3) as { runtime, n } (runtime)}
              <span class="flex items-center gap-1">
                {@render runtimeBadge(runtime, 15)}
                <span class="num text-2xs text-ink-5">{n}</span>
              </span>
            {/each}
          {/if}
          {#if g.runnable > 0}
            <span
              class="num rounded-full px-[7px] py-[1px] text-2xs font-semibold"
              style="background: var(--accent-wash); color: var(--accent-ink);"
              title={`${g.runnable} of ${g.rows.length} worth running`}
            >{g.runnable}</span>
          {/if}
          <span class="num text-2xs text-ink-6">{g.rows.length}</span>
        </span>
      </button>
      {#each shut ? [] : g.rows as { f, v } (f.path)}
        {@const runnable = v.tier !== "support"}
        <button
          class={`filerow group flex w-full cursor-pointer items-center gap-3 py-[9px] pr-4 text-left transition-colors hover:bg-accent-wash ${runnable ? "" : "dim"}`}
          style={`padding-left: ${26 + g.depth * 11}px; --rail: ${14 + g.depth * 11}px;`}
          onclick={() => onpick(f)}
        >
          <!-- The badge is the tell. A runtime badge means "a runtime will
               take this"; a plain file glyph means "you can pick it, but
               Meshware is not the one suggesting it". -->
          {#if runnable}
            {@render runtimeBadge(v.runtime, 22)}
          {:else}
            <span class="grid h-[22px] w-[22px] flex-none place-items-center rounded-sm text-ink-faint" style="background: var(--surface-2);">
              <Icon name="file" size={13} />
            </span>
          {/if}
          <span class={`mono truncate text-sm ${v.tier === "entry" ? "font-semibold text-ink" : "text-ink-2"}`}>{f.name}</span>
          {#if f.executable}
            <span class="flex-none text-2xs font-bold" style="color: var(--status-ok-ink);" title="Executable">+x</span>
          {/if}
          <!-- Why this row is where it is, in words, on the row itself. -->
          <span class="flex-none truncate text-xs whitespace-nowrap" style={v.tier === "entry" ? "color: var(--accent-ink);" : "color: var(--ink-5);"}>
            {v.why}
          </span>
          <span class="num ml-auto flex-none text-xs text-ink-5">{fmtBytes(f.size)}</span>
          <span
            class="flex-none translate-x-[-4px] text-xs font-semibold whitespace-nowrap opacity-0 transition-[opacity,transform] duration-150 group-hover:translate-x-0 group-hover:opacity-100"
            style="color: var(--accent-ink);">{runnable ? action : `${action} anyway`} →</span
          >
        </button>
      {/each}
    {/each}
  {/if}
{/snippet}

{#snippet browserToolbar(id: string | null)}
  <div class="flex flex-wrap items-center gap-2">
    <div class="relative min-w-48 flex-1">
      <span class="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-ink-6"><Icon name="search" size={15} /></span>
      <input
        bind:value={fileQuery}
        placeholder="Search files…"
        class="h-9 w-full rounded-md bg-canvas pr-3 pl-9 text-sm text-ink outline-none placeholder:text-ink-6"
        style="box-shadow: var(--sh-control);"
      />
    </div>
    <!-- Three tiers, widest last, each labelled with what it actually
         contains. The old toggle offered "Runnable" and "All files", and in
         any repository with a CI config those were nearly the same list. -->
    <div class="inline-flex flex-none rounded-md bg-surface-2 p-[3px]" role="tablist" aria-label="Which files to show">
      {#each [{ id: "suggested", label: "Suggested", n: tierCount.entry, why: "Executables, playbooks, and files named like something you would start" }, { id: "scripts", label: "Scripts", n: tierCount.script, why: "Anything a runtime could take, entry point or not" }, { id: "all", label: "Everything", n: tierCount.all, why: "Every file in the source, minus build output and tooling directories" }] as { id, label, n, why } (id)}
        <button
          role="tab"
          aria-selected={fileMode === id}
          title={why}
          class={`cursor-pointer rounded-sm px-2.5 py-[5px] text-xs transition-colors ${fileMode === id ? "bg-surface font-semibold text-ink" : "font-medium text-ink-5 hover:text-ink"}`}
          style={fileMode === id ? "box-shadow: var(--sh-control);" : ""}
          onclick={() => (fileMode = id as "suggested" | "scripts" | "all")}
        >
          {label} <span class="num text-ink-5">{n}</span>
        </button>
      {/each}
    </div>
    <!-- Folders open and close, so there is a control that does it to all of
         them at once — a fifty-directory repo is not a place to click fifty
         chevrons. -->
    <div class="inline-flex flex-none items-center overflow-hidden rounded-md bg-canvas" style="box-shadow: var(--sh-control);">
      <button
        class="grid h-9 w-9 cursor-pointer place-items-center text-ink-4 transition-colors hover:text-accent-ink"
        title={shutDirs.size ? "Open every folder" : "Close every folder"}
        aria-label={shutDirs.size ? "Open every folder" : "Close every folder"}
        onclick={() =>
          shutDirs.size
            ? openAll()
            : shutAll(groupByDir(pickable), "all")}
      >
        <Icon name={shutDirs.size ? "expand" : "shrink"} size={15} />
      </button>
      <button
        class="grid h-9 w-9 cursor-pointer place-items-center text-ink-4 transition-colors hover:text-accent-ink"
        title="Re-read this source"
        aria-label="Re-read this source"
        onclick={() => loadFiles(id, true)}
      >
        <Icon name="refresh" size={15} />
      </button>
    </div>
  </div>
{/snippet}

{#snippet repoForm()}
  <div class="card fade-in p-6">
    <div class="flex items-start gap-3">
      {@render originAvatar("git", 38)}
      <div class="min-w-0">
        <h2 class="m-0 text-md font-bold tracking-[-.02em] text-ink">Connect a Git repo</h2>
        <p class="mt-0.5 mb-0 text-sm text-ink-4">Meshware keeps its own copy and re-reads it on a schedule.</p>
      </div>
      <button class="ml-auto cursor-pointer text-sm font-semibold text-ink-4 hover:text-ink" onclick={() => (repoOpen = false)}>Cancel</button>
    </div>

    <label class="mt-5 block">
      <span class={ui.label}>Repository URL</span>
      <input
        bind:value={rUrl}
        placeholder="git@github.com:you/ops-scripts.git"
        class={ui.inputMono}
        onkeydown={(e) => e.key === "Enter" && canConnect && connectRepo()}
      />
    </label>

    <div class="mt-3 grid grid-cols-2 gap-3">
      <label class="block">
        <span class={ui.label}>Branch</span>
        <input bind:value={rBranch} class={ui.inputMono} />
      </label>
      <label class="block">
        <span class={ui.label}>Check for changes</span>
        <select bind:value={rInterval} class={`${ui.input} cursor-pointer`}>
          {#each INTERVALS as [secs, label] (secs)}<option value={secs}>{label}</option>{/each}
        </select>
      </label>
    </div>

    <button class="mt-3 cursor-pointer text-sm font-medium text-ink-5 hover:text-accent-ink" onclick={() => (rMore = !rMore)}>
      {rMore ? "Hide" : "Show"} advanced
    </button>
    {#if rMore}
      <label class="mt-2 block w-1/2">
        <span class={ui.label}>Call it something else</span>
        <input bind:value={rName} placeholder={repoNameFromUrl(rUrl) || "ops-scripts"} class={ui.inputMono} />
      </label>
    {/if}

    {#if rErr}<div class={ui.errorBox}>{rErr}</div>{/if}

    <div class="mt-5 flex items-center gap-3">
      <button class={`${ui.primary} cursor-pointer`} disabled={!canConnect} onclick={connectRepo}>
        {rBusy ? (rStage ?? "Working…") : "Connect"}
      </button>
      {#if rDerivedName && !rBusy}
        <span class="text-xs text-ink-5">
          Saved as <span class="mono font-semibold text-ink-3">{rDerivedName}</span> · SSH URLs use this machine's keys.
        </span>
      {/if}
    </div>
  </div>
{/snippet}

{#snippet addTiles()}
  <div class="grid grid-cols-2 gap-2.5">
    <button
      class="tile group flex cursor-pointer items-center gap-3 border-dashed px-4 py-3.5 text-left"
      disabled={!canContent}
      onclick={openRepo}
    >
      {@render originAvatar("git", 34)}
      <span class="min-w-0">
        <span class="block text-base font-semibold text-ink">Connect a Git repo</span>
        <span class="block text-xs text-ink-4">Stays in sync as you push.</span>
      </span>
      <span class="ml-auto flex-none text-ink-faint transition-colors group-hover:text-accent-ink"><Icon name="chevron-right" size={16} /></span>
    </button>
    <button
      class="tile group flex cursor-pointer items-center gap-3 border-dashed px-4 py-3.5 text-left"
      disabled={!canContent || folderBusy}
      onclick={addFolder}
    >
      {@render originAvatar("folder", 34)}
      <span class="min-w-0">
        <span class="block text-base font-semibold text-ink">{folderBusy ? "Copying…" : "Add a folder"}</span>
        <span class="block text-xs text-ink-4">A snapshot from this computer.</span>
      </span>
      <span class="ml-auto flex-none text-ink-faint transition-colors group-hover:text-accent-ink"><Icon name="chevron-right" size={16} /></span>
    </button>
  </div>
{/snippet}

<!-- ================================ view ================================ -->

<!-- Drop a script anywhere on this page (R2 P4). The overlay only appears
     while something is actually over the window, so it costs nothing to
     look at and cannot be missed while it matters. -->
{#if dropping || dropBusy}
  <div
    class="pointer-events-none fixed inset-0 z-50 grid place-items-center"
    style="background: color-mix(in oklab, var(--bg-canvas) 82%, transparent);"
  >
    <div
      class="rounded-lg px-6 py-5 text-center"
      style="border: 2px dashed var(--accent); background: var(--surface-1);"
    >
      <div class="text-base font-semibold text-ink">
        {dropBusy ? "Adding…" : "Drop a script to add it"}
      </div>
      <div class="mt-1 text-sm text-ink-4">
        Its arguments come from the script itself — you confirm them next.
      </div>
    </div>
  </div>
{/if}

<div class="fade-in flex h-full min-h-0 flex-col gap-4">
  {#if wizard}
    <!-- ===================== add / edit a script ===================== -->
    <div class="mx-auto flex h-full min-h-0 w-full max-w-[980px] flex-col gap-4">
      <div class="flex flex-none items-center gap-2.5">
        {#each [[1, "Source"], [2, "File"], [3, "Details"]] as [n, label] (n)}
          {@const num = n as number}
          {@const done = num < step}
          <button
            class="flex items-center gap-2 rounded-full py-1 pr-3 pl-1 text-sm transition-colors disabled:cursor-default"
            style={step === num
              ? "background: var(--accent-wash); color: var(--accent-ink); font-weight: 600; cursor: default;"
              : done
                ? "color: var(--accent-ink); cursor: pointer;"
                : "color: var(--ink-6); cursor: default;"}
            disabled={num >= step}
            onclick={() => (step = num as 1 | 2 | 3)}
          >
            <span
              class="num grid h-[22px] w-[22px] place-items-center rounded-full text-xs font-bold"
              style={step === num
                ? "background: var(--accent); color: #fff;"
                : done
                  ? "background: var(--accent-wash); color: var(--accent-ink); box-shadow: inset 0 0 0 1px var(--accent);"
                  : "background: var(--surface-2); color: var(--ink-6);"}
            >
              {done ? "✓" : num}
            </span>
            {label}
          </button>
          {#if num < 3}<span class="h-px w-5 flex-none" style="background: var(--edge-dashed);"></span>{/if}
        {/each}
        <button
          class="ml-auto cursor-pointer text-sm font-semibold text-ink-4 hover:text-ink"
          onclick={() => {
            wizard = false;
            resetWizard();
          }}
        >
          Cancel
        </button>
      </div>

      {#if step === 1}
        <!-- ---------------- 1 · where it lives ---------------- -->
        <div class="card flex flex-col overflow-hidden">
          <div class="flex-none border-b border-edge px-6 pt-6 pb-5">
            <h2 class="m-0 text-lg font-bold tracking-[-.025em] text-ink">Where does this script live?</h2>
            <p class="mt-1 mb-0 text-base text-ink-4">Choose a repo or folder you have already added.</p>
          </div>

          <div class="dark-scroll min-h-0 flex-1 overflow-y-auto p-4">
            {#if named.length}
              <div class="space-y-2">
                {#each named as o (o.key)}
                  <button
                    class="tile group flex w-full cursor-pointer items-center gap-3.5 bg-surface px-4 py-3.5 text-left"
                    disabled={!o.bundleId}
                    onclick={() => chooseOrigin(o)}
                  >
                    {@render originAvatar(o.kind, 38)}
                    <span class="min-w-0 flex-1">
                      <span class="flex items-center gap-2">
                        <span class="truncate text-base font-semibold text-ink">{o.name}</span>
                        <span class="flex-none rounded-full px-1.5 py-px text-2xs font-semibold" style="background: var(--surface-2); color: var(--ink-4);">{KIND_LABEL[o.kind]}</span>
                      </span>
                      <span class="mono mt-0.5 block truncate text-xs text-ink-5">{o.detail}</span>
                    </span>
                    <span class="flex-none text-right">
                      {#if o.bundleId}
                        <span class="num block text-base font-semibold text-ink-3">{o.fileCount ?? "?"}</span>
                        <span class="block text-2xs text-ink-5">files</span>
                      {:else}
                        <span class="block text-xs font-semibold" style="color: var(--status-warn-ink);">not read yet</span>
                      {/if}
                    </span>
                    <span class="flex-none text-ink-faint transition-colors group-hover:text-accent-ink"><Icon name="chevron-right" size={17} /></span>
                  </button>
                {/each}
              </div>
            {/if}

            {#if unnamed.length}
              <button class="mt-3 cursor-pointer text-sm font-medium text-ink-5 hover:text-accent-ink" onclick={() => (showUnnamed = !showUnnamed)}>
                {showUnnamed ? "Hide" : "Show"}
                {unnamed.length} unnamed snapshot{unnamed.length === 1 ? "" : "s"}{" "}
                <span class="text-ink-6">— content added outside this Director</span>
              </button>
              {#if showUnnamed}
                <div class="mt-2 space-y-2">
                  {#each unnamed as o (o.key)}
                    <button
                      class="group flex w-full cursor-pointer items-center gap-3.5 rounded-lg bg-canvas px-4 py-3 text-left transition-colors hover:bg-surface-2"
                      onclick={() => chooseOrigin(o)}
                    >
                      {@render originAvatar(o.kind, 32)}
                      <span class="min-w-0 flex-1">
                        <span class="block truncate text-base font-semibold text-ink-2">{o.name}</span>
                        <span class="mono block truncate text-xs text-ink-5">{o.code} · {o.detail}</span>
                      </span>
                      <span class="num flex-none text-xs text-ink-5">{o.fileCount} {o.fileCount === 1 ? "file" : "files"}</span>
                      <span class="flex-none text-ink-faint transition-colors group-hover:text-accent-ink"><Icon name="chevron-right" size={16} /></span>
                    </button>
                  {/each}
                </div>
              {/if}
            {/if}

            {#if !named.length && !unnamed.length}
              <div class="flex h-full flex-col items-center justify-center gap-2 text-center">
                <span class="text-ink-faint"><Icon name="inbox" size={30} /></span>
                <span class="text-base font-semibold text-ink-2">Nothing added yet</span>
                <span class="max-w-sm text-sm text-ink-4">Connect a Git repo, or snapshot a folder from this computer.</span>
              </div>
            {/if}
          </div>

          <div class="flex-none border-t border-edge bg-canvas p-4">{@render addTiles()}</div>
        </div>
        {#if repoOpen}{@render repoForm()}{/if}
      {:else if step === 2}
        <!-- ---------------- 2 · which file ---------------- -->
        <div class="card flex min-h-0 flex-1 flex-col overflow-hidden">
          <div class="flex-none border-b border-edge px-6 pt-5 pb-4">
            <div class="flex items-center gap-3">
              {@render originAvatar(origin?.kind ?? "snapshot", 34)}
              <div class="min-w-0">
                <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">Pick the file to run</h2>
                <p class="mt-0.5 mb-0 truncate text-sm text-ink-4">
                  in <span class="font-semibold text-ink-2">{origin?.name ?? "this source"}</span>
                </p>
              </div>
              <button class="ml-auto flex-none cursor-pointer text-sm font-semibold" style="color: var(--accent-ink);" onclick={() => (step = 1)}>
                Change source
              </button>
            </div>
            <div class="mt-3.5">{@render browserToolbar(bundleId)}</div>
          </div>
          <div class="dark-scroll min-h-0 flex-1 overflow-y-auto">
            {@render fileBrowser(bundleId ?? "", pickable, chooseFile, "choose")}
          </div>
          <!-- N4. A container has no file to pick — its entry point is an
               image. Without this, the only route to a container service
               was to choose an arbitrary file, change the runtime on the
               next step, and then overwrite the field that file had set. -->
          <div class="flex-none border-t border-edge px-6 py-3">
            <button
              class="cursor-pointer text-sm font-semibold"
              style="color: var(--accent-ink);"
              onclick={() => {
                sRuntime = "container";
                sEntrypoint = "";
                sWorkingDir = "";
                if (!sName.trim() && origin?.name) sName = suggestNameFromSource(origin.name);
                step = 3;
              }}
            >
              Running a container image instead? Skip this step →
            </button>
            <span class="ml-2 text-xs text-ink-5">
              The source above still comes with it, mounted read-only at
              <code class="num">/meshware/bundle</code>.
            </span>
          </div>
          <div class="flex-none border-t border-edge px-6 py-2.5 text-xs text-ink-5">
            {#if fileQuery.trim()}
              <span class="num font-semibold text-ink-3">{pickable.length}</span>
              of {allFiles.length} file{allFiles.length === 1 ? "" : "s"} match — search covers the whole source
            {:else}
              Showing <span class="num font-semibold text-ink-3">{pickable.length}</span>
              of {allFiles.length} file{allFiles.length === 1 ? "" : "s"}{#if fileMode === "suggested"}{" — the ones that look like a place to start"}{:else if fileMode === "scripts"}{" — anything a runtime could take"}{:else}{" — build output and tooling directories excluded"}{/if}
            {/if}
          </div>
        </div>
      {:else}
        <!-- ---------------- 3 · details ---------------- -->
        <div class="dark-scroll min-h-0 flex-1 overflow-y-auto pb-1">
          <div class="grid items-start gap-4" style="grid-template-columns: minmax(0,1fr) 310px;">
            <div class="space-y-3">
              <!-- basics -->
              <div class="card p-6">
                <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">{updating ? `Edit ${updating}` : "Name it"}</h2>
                <p class="mt-1 mb-4 text-sm text-ink-4">
                  {updating
                    ? "Saving creates a new version — the current one stays available to restore."
                    : "This is the name people will type to run it."}
                </p>

                <div class="grid grid-cols-[minmax(0,1fr)_210px] gap-3">
                  <label class="block">
                    <span class={ui.label}>Name</span>
                    <input bind:value={sName} placeholder="restart-edge-agents" class={ui.inputMono} />
                  </label>
                  <label class="block">
                    <span class={ui.label}>Run it with</span>
                    <select bind:value={sRuntime} class={`${ui.input} cursor-pointer`}>
                      {#each RUNTIMES as r (r.id)}<option value={r.id}>{r.label}</option>{/each}
                    </select>
                  </label>
                  <label class="col-span-2 block">
                    <span class={ui.label}>Description <span class="text-ink-6">(optional)</span></span>
                    <input bind:value={sDescription} placeholder="what this does, for whoever runs it next" class={ui.input} />
                  </label>
                </div>

                <!-- N4. For a container these three *are* the service, so
                     they sit here rather than behind Advanced. The file
                     picker on step 2 has nothing to say about an image, and
                     leaving these one disclosure away meant the only way to
                     make a container service in Director was to open a panel
                     labelled "Advanced" and overwrite a field the previous
                     step had filled in with a filename. -->
                {#if sRuntime === "container"}
                  <div class="fade-in mt-3.5 rounded-lg border px-4 py-3.5" style="border-color: var(--accent-edge); background: var(--accent-wash-soft, var(--surface-2));">
                    <div class="mb-2.5 flex items-baseline gap-2">
                      <span class="text-sm font-semibold text-ink">Container</span>
                      <span class="text-xs text-ink-5">This runs an image. The source you picked is mounted read-only at <code class="num">/meshware/bundle</code>.</span>
                    </div>
                    <label class="block">
                      <span class={ui.label}>Image</span>
                      <input bind:value={sImage} placeholder="docker.io/library/alpine:3.20" class={ui.inputMono} />
                    </label>
                    <div class="mt-3 grid grid-cols-2 gap-3">
                      <label class="block">
                        <span class={ui.label}>Command <span class="text-ink-6">(optional)</span></span>
                        <input bind:value={sCommand} placeholder="sh" class={ui.inputMono} />
                        <span class="mt-1 block text-xs text-ink-5">
                          Overrides the image's own entry point. Leave empty to run
                          what the image was built to run.
                        </span>
                      </label>
                      <label class="block">
                        <span class={ui.label}>Arguments <span class="text-ink-6">(one per line)</span></span>
                        <textarea bind:value={sCmdArgs} rows="3" placeholder={"-c\nhttpd -f -p \"$MESHWARE_PORT_HTTP\""} class={ui.inputMono}></textarea>
                        <span class="mt-1 block text-xs text-ink-5">
                          The job's allocated port arrives as
                          <code class="num">$MESHWARE_PORT_&lt;LABEL&gt;</code>.
                        </span>
                      </label>
                    </div>
                  </div>
                {/if}

                {#if nameProblem}
                  <div class="mt-2.5 text-xs" style="color: var(--status-error-ink);">
                    That name will not work: {nameProblem}.
                  </div>
                {:else if nameTaken && !updating}
                  <div class="mt-2.5 text-xs" style="color: var(--status-warn-ink);">
                    <span class="mono font-semibold">{sName.trim()}</span> already exists — this saves a new version of it.
                  </div>
                {/if}
              </div>

              <!-- arguments -->
              <div class="card p-6">
                <div class="flex items-center gap-2.5">
                  <h3 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Arguments</h3>
                  {#if namedArgs}
                    <span class="num rounded-full px-2 py-0.5 text-2xs font-bold" style="background: var(--accent-wash); color: var(--accent-ink);">{namedArgs}</span>
                  {/if}
                  <span class="text-sm text-ink-5">Values whoever runs this fills in.</span>
                  {#if sRaw === null}
                    <button class="ml-auto flex flex-none cursor-pointer items-center gap-1 text-sm font-semibold" style="color: var(--accent-ink);" onclick={() => (sArgs = [...sArgs, emptyArg()])}>
                      <Icon name="plus" size={14} /> Add
                    </button>
                  {:else}
                    <button
                      class="ml-auto flex-none cursor-pointer text-sm font-semibold"
                      style="color: var(--accent-ink);"
                      onclick={() => {
                        const p = parseContract(sRaw);
                        if (p.rows) {
                          sArgs = p.rows;
                          sRootExtra = p.rootExtra;
                          sRaw = null;
                        }
                      }}
                    >
                      Back to the simple editor
                    </button>
                  {/if}
                </div>
                <!-- Where these came from (R2 P4). A pre-filled form that
                     does not say where it got its answers is a form people
                     re-check by hand, which costs more than typing them. -->
                {#if sReadErr}
                  <div class="mt-2 text-sm" style="color: var(--status-error);">
                    Could not read this file — nothing was pre-filled.
                    <span class="selectable mono text-xs">{sReadErr}</span>
                  </div>
                {/if}
                {#if sInspected?.error}
                  <!-- A definitions file that does not parse must say so.
                       Falling back to an empty form without a word is the
                       exact failure this whole path exists to remove. -->
                  <div class="mt-2 text-sm" style="color: var(--status-error);">
                    Its definitions file could not be read, so it was not used —
                    <span class="selectable mono text-xs">{sInspected.error}</span>
                  </div>
                {/if}
                {#if sInspected && sInspected.argCount > 0}
                  <div class="mt-2 text-sm" style="color: var(--accent-ink);">
                    {#if sInspected.source === "definitions"}
                      Read from <span class="mono">{sInspected.definitionsPath}</span>.
                      {#if sInspected.item}Each item of a per-item run fills in
                        <span class="mono">--{sInspected.item}</span>.{/if}
                    {:else if sInspected.source === "header"}
                      Read from the script's own <span class="mono">#meshware:</span> header.
                      {#if sInspected.item}Each item of a per-item run fills in
                        <span class="mono">--{sInspected.item}</span>.{/if}
                    {:else}
                      {sInspected.inferredCount} of these were inferred by reading the script —
                      check them, then <button
                        class="cursor-pointer font-semibold underline underline-offset-2"
                        onclick={() => (sDefsOpen = true)}>save a definitions file</button>
                      to say what you meant.
                    {/if}
                  </div>
                {:else if sInspected && sInspected.argCount === 0 && sEntrypoint}
                  <div class="mt-2 text-sm text-ink-5">
                    This script declares no arguments, so its Run screen will have no fields.
                    Write them below, or in a
                    <span class="mono">{sInspected.definitionsPath}</span> file beside it.
                  </div>
                {/if}

                {#if sRaw !== null}
                  <textarea
                    bind:value={sRaw}
                    rows="10"
                    spellcheck="false"
                    class="mono mt-3 w-full resize-y rounded-md bg-canvas px-3 py-2.5 text-sm leading-relaxed outline-none"
                  ></textarea>
                  {#if rawStatus}
                    <div class="mt-1 text-xs" style={rawStatus.ok ? "color: var(--accent-ink);" : "color: var(--status-error);"}>{rawStatus.text}</div>
                  {/if}
                {:else if sArgs.length === 0}
                  <button
                    class="mt-3 flex w-full cursor-pointer flex-col items-center gap-1 rounded-lg border border-dashed py-7 transition-colors"
                    style="border-color: var(--edge-dashed);"
                    onclick={() => (sArgs = [emptyArg()])}
                  >
                    <span style="color: var(--accent-ink);"><Icon name="plus" size={18} /></span>
                    <span class="text-base font-semibold text-ink-2">Add an argument</span>
                    <span class="max-w-sm text-center text-sm text-ink-5">
                      Optional. Without any, the script just runs — with them, the Run screen shows a labelled box for each.
                    </span>
                  </button>
                {:else}
                  <div class="mt-3 space-y-2">
                    {#each sArgs as a, i (i)}
                      <div class="rounded-lg bg-canvas p-3">
                        <div class="flex items-center gap-2">
                          <span class="mono flex-none text-sm font-semibold text-ink-5">--</span>
                          <input bind:value={a.name} placeholder="version" class="mono h-9 min-w-0 flex-1 rounded-sm bg-surface px-3 text-sm outline-none" style="box-shadow: var(--sh-control);" />
                          <select bind:value={a.kind} class="select h-9 w-[136px] flex-none cursor-pointer rounded-sm bg-surface px-2.5 text-sm outline-none" style="box-shadow: var(--sh-control);">
                            {#each ARG_KINDS as k (k.id)}<option value={k.id}>{k.label}</option>{/each}
                          </select>
                          <label class="flex flex-none cursor-pointer items-center gap-1.5 px-1 text-sm text-ink-4">
                            <input type="checkbox" bind:checked={a.required} class="accent-[var(--accent)]" /> required
                          </label>
                          <button
                            class="grid h-8 w-8 flex-none cursor-pointer place-items-center rounded-sm text-ink-5 transition-colors hover:bg-surface hover:text-error"
                            title="Remove this argument"
                            aria-label="Remove this argument"
                            onclick={() => (sArgs = sArgs.filter((_, j) => j !== i))}
                          >
                            <Icon name="close" size={15} />
                          </button>
                        </div>
                        <div class="mt-2 flex gap-2 pl-6">
                          <input bind:value={a.default} placeholder="default value" class="mono h-8 w-[150px] flex-none rounded-sm bg-surface px-3 text-sm outline-none" style="box-shadow: var(--sh-control);" />
                          {#if a.kind === "choice"}
                            <input bind:value={a.options} placeholder="allowed values — east, west" class="mono h-8 w-[210px] flex-none rounded-sm bg-surface px-3 text-sm outline-none" style="box-shadow: var(--sh-control);" />
                          {/if}
                          <input bind:value={a.description} placeholder="hint shown on the Run screen" class="h-8 min-w-0 flex-1 rounded-sm bg-surface px-3 text-sm outline-none" style="box-shadow: var(--sh-control);" />
                        </div>
                      </div>
                    {/each}
                    <button class="flex cursor-pointer items-center gap-1 pl-1 text-sm font-semibold" style="color: var(--accent-ink);" onclick={() => (sArgs = [...sArgs, emptyArg()])}>
                      <Icon name="plus" size={14} /> Add another
                    </button>
                  </div>
                {/if}
              </div>

              <!-- The definitions file (R2 P5).
                   The same declaration as the rows above, in the file the
                   script carries it in. Two views of one thing: `Use this`
                   compiles the JSON into the rows, `Save` writes the rows
                   back out as JSON, and the parser on both sides is the one
                   that will read the file off disk. -->
              {#if sInspected}
                <div class="mt-5 rounded-lg bg-surface-2 p-4">
                  <div class="flex items-center gap-2.5">
                    <button
                      class="flex cursor-pointer items-center gap-2"
                      aria-expanded={sDefsOpen}
                      onclick={() => (sDefsOpen = !sDefsOpen)}
                    >
                      <span class={`text-ink-5 transition-transform duration-150 ${sDefsOpen ? "rotate-90" : ""}`}>
                        <Icon name="chevron-right" size={13} />
                      </span>
                      <h3 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Definitions file</h3>
                    </button>
                    <span class="mono truncate text-xs text-ink-5">{sInspected.definitionsPath}</span>
                    {#if !sInspected.hasDefinitions}
                      <span class="flex-none rounded-full bg-canvas px-2 py-0.5 text-2xs font-bold text-ink-5">not yet written</span>
                    {:else if defsDirty}
                      <span class="flex-none rounded-full px-2 py-0.5 text-2xs font-bold" style="background: var(--accent-wash); color: var(--accent-ink);">edited</span>
                    {/if}
                  </div>

                  {#if sDefsOpen}
                    <p class="mt-2 mb-0 max-w-[70ch] text-sm text-ink-5">
                      JSON beside the script, holding what it declares — types, defaults,
                      bounds, which argument a per-item run fills in. A script with one
                      needs no <span class="mono">#meshware:</span> header; a script with
                      both is described by this file wherever they disagree.
                    </p>
                    <textarea
                      bind:value={sDefs}
                      rows="14"
                      spellcheck="false"
                      class="mono mt-3 w-full resize-y rounded-md bg-canvas px-3 py-2.5 text-sm leading-relaxed outline-none"
                    ></textarea>

                    {#if defsStatus}
                      <div class="mt-1 text-xs" style={defsStatus.ok ? "color: var(--accent-ink);" : "color: var(--status-error);"}>{defsStatus.text}</div>
                    {/if}
                    {#if sDefsErr}
                      <div class="selectable mt-1 text-xs" style="color: var(--status-error);">{sDefsErr}</div>
                    {/if}

                    <div class="mt-3 flex flex-wrap items-center gap-2">
                      <button
                        class={ui.secondary}
                        disabled={sDefsBusy || !sDefs?.trim() || defsStatus?.ok === false}
                        onclick={applyDefinitions}
                      >
                        Use this — fill the fields above
                      </button>
                      <button
                        class={ui.secondary}
                        disabled={sDefsBusy}
                        onclick={() => {
                          sDefs = definitionsFromForm();
                          sDefsErr = null;
                        }}
                      >
                        Take the fields above
                      </button>
                      {#if defsTarget}
                        <button
                          class={ui.primary}
                          disabled={sDefsBusy || !sDefs?.trim() || defsStatus?.ok === false}
                          onclick={saveDefinitions}
                        >
                          {sDefsBusy ? "Saving…" : sInspected.hasDefinitions ? "Save to disk" : "Write the file"}
                        </button>
                        <span class="mono min-w-0 truncate text-xs text-ink-5">{defsTarget.root}</span>
                      {:else}
                        <span class="max-w-[46ch] text-xs text-ink-5">
                          Director does not know where this source lives on disk, so it
                          cannot write the file — copy the JSON into
                          <span class="mono">{sInspected.definitionsPath}</span> beside the
                          script. Sources added here as a folder can be saved to directly.
                        </span>
                      {/if}
                    </div>
                  {/if}
                </div>
              {/if}

              <!-- environment & secrets -->
              <div class="card overflow-hidden">
                <button class="flex w-full cursor-pointer items-center gap-2.5 px-6 py-4 text-left" onclick={() => (sEnvOpen = !sEnvOpen)}>
                  <span class="text-ink-4"><Icon name="secrets" size={17} /></span>
                  <h3 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Environment &amp; secrets</h3>
                  {#if namedEnv}
                    <span class="num rounded-full px-2 py-0.5 text-2xs font-bold" style="background: var(--accent-wash); color: var(--accent-ink);">{namedEnv}</span>
                  {/if}
                  <span class="text-sm text-ink-5">Set on every run. The safe place for credentials.</span>
                  <span class="ml-auto text-ink-5 transition-transform" style={sEnvOpen ? "transform: rotate(90deg);" : ""}><Icon name="chevron-right" size={16} /></span>
                </button>
                {#if sEnvOpen}
                  <div class="fade-in border-t border-edge px-6 py-4">
                    {#if sEnv.length === 0}
                      <p class="mt-0 mb-3 text-sm text-ink-4">
                        Nothing set. Credentials belong here rather than in arguments — arguments are visible to anyone on the machine.
                      </p>
                    {:else}
                      <div class="space-y-2">
                        {#each sEnv as row, i (i)}
                          <div class="flex items-center gap-2">
                            <input bind:value={row.k} placeholder="API_TOKEN" class="mono h-9 w-44 flex-none rounded-sm bg-canvas px-3 text-sm outline-none" style="box-shadow: var(--sh-control);" />
                            <input bind:value={row.v} placeholder="value" class="mono h-9 min-w-0 flex-1 rounded-sm bg-canvas px-3 text-sm outline-none" style="box-shadow: var(--sh-control);" />
                            {#if secretNames.length}
                              <select
                                class="select h-9 w-[128px] flex-none cursor-pointer rounded-sm bg-canvas px-2 text-xs text-ink-4 outline-none"
                                style="box-shadow: var(--sh-control);"
                                onchange={(e) => {
                                  const n = e.currentTarget.value;
                                  if (n) row.v = `{{secret:${n}}}`;
                                  e.currentTarget.value = "";
                                }}
                              >
                                <option value="">Use a secret…</option>
                                {#each secretNames as n (n)}<option value={n}>{n}</option>{/each}
                              </select>
                            {/if}
                            <button
                              class="grid h-8 w-8 flex-none cursor-pointer place-items-center rounded-sm text-ink-5 transition-colors hover:bg-canvas hover:text-error"
                              title="Remove"
                              aria-label="Remove"
                              onclick={() => (sEnv = sEnv.filter((_, j) => j !== i))}
                            >
                              <Icon name="close" size={15} />
                            </button>
                          </div>
                        {/each}
                      </div>
                    {/if}
                    <div class="mt-3 flex items-center gap-4">
                      <button class="flex cursor-pointer items-center gap-1 text-sm font-semibold" style="color: var(--accent-ink);" onclick={() => (sEnv = [...sEnv, { k: "", v: "" }])}>
                        <Icon name="plus" size={14} /> Add a variable
                      </button>
                      {#if secretNames.length === 0}
                        <button class="cursor-pointer text-xs text-ink-5 hover:text-accent-ink" onclick={() => app.go("secrets")}>
                          No secrets stored yet — add one in Secrets →
                        </button>
                      {/if}
                    </div>
                  </div>
                {/if}
              </div>

              <!-- advanced -->
              <div class="card overflow-hidden">
                <button class="flex w-full cursor-pointer items-center gap-2.5 px-6 py-4 text-left" onclick={() => (sAdvOpen = !sAdvOpen)}>
                  <span class="text-ink-4"><Icon name="settings" size={17} /></span>
                  <h3 class="m-0 text-md font-bold tracking-[-.015em] text-ink">Advanced</h3>
                  <span class="text-sm text-ink-5">Tags, exact paths, raw JSON Schema.</span>
                  <span class="ml-auto text-ink-5 transition-transform" style={sAdvOpen ? "transform: rotate(90deg);" : ""}><Icon name="chevron-right" size={16} /></span>
                </button>
                {#if sAdvOpen}
                  <div class="fade-in border-t border-edge px-6 py-4">
                    <div class="grid grid-cols-3 gap-3">
                      <!-- N4. For a container the two fields opposite mean
                           something different, and the labels say so rather
                           than leaving a person to type an image reference
                           into a box marked "File name". -->
                      <label class="block">
                        <span class={ui.label}
                          >{sRuntime === "container"
                            ? "Folder inside the image"
                            : "Folder in the source"}</span
                        >
                        <input bind:value={sWorkingDir} placeholder={sRuntime === "container" ? "/app" : "(top level)"} class={ui.inputMono} />
                      </label>
                      {#if sRuntime !== "container"}
                        <label class="block">
                          <span class={ui.label}>File name</span>
                          <input bind:value={sEntrypoint} placeholder="run.sh" class={ui.inputMono} />
                        </label>
                      {/if}
                      <label class="block">
                        <span class={ui.label}>Tags <span class="text-ink-6">(comma separated)</span></span>
                        <input bind:value={sTags} placeholder="team=platform" class={ui.inputMono} />
                      </label>
                    </div>
                    {#if sRaw === null}
                      <button
                        class="mt-3.5 cursor-pointer text-sm font-semibold"
                        style="color: var(--accent-ink);"
                        onclick={() => (sRaw = buildContract(sArgs, sRootExtra) ?? '{\n  "type": "object",\n  "properties": {}\n}')}
                      >
                        Edit arguments as JSON Schema instead
                      </button>
                    {/if}
                  </div>
                {/if}
              </div>
            </div>

            <!-- sticky summary: what you just built -->
            <div class="sticky top-0 space-y-3">
              <div class="on-code overflow-hidden rounded-xl bg-code" style="box-shadow: var(--sh-code);">
                <div class="px-5 pt-4 pb-3">
                  <div class="mono text-2xs tracking-[.12em] uppercase" style="color: var(--ink-code-4);">what will run</div>
                  <div class="mono selectable mt-2 text-sm leading-relaxed break-all" style="color: var(--ink-code-1);">
                    <span style="color: var(--accent-bright);">{preview.program}</span>
                    {#each preview.parts as p, i (i)}<span style={p.required ? "" : "color: var(--ink-code-4);"}>{` ${p.text}`}</span>{/each}
                  </div>
                </div>
                <dl class="m-0 space-y-2 border-t px-5 py-3.5 text-xs" style="border-color: var(--edge-code);">
                  {#each [["Source", origin?.name ?? "—"], ["File", chosenPath || "—"], ["Folder", sWorkingDir || "top level"]] as [k, v] (k)}
                    <div class="flex gap-3">
                      <dt class="flex-none" style="color: var(--ink-code-4);">{k}</dt>
                      <dd class="mono m-0 ml-auto truncate text-right" style="color: var(--ink-code-2);" title={v}>{v}</dd>
                    </div>
                  {/each}
                  {#if preview.parts.some((p) => !p.required)}
                    <div class="pt-1 text-2xs" style="color: var(--ink-code-4);">Dimmed arguments are optional.</div>
                  {/if}
                </dl>
              </div>

              {#if sErr}
                <div class="selectable rounded-lg px-4 py-3 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{sErr}</div>
              {/if}

              <button class={`${ui.primary} w-full cursor-pointer`} disabled={!canSave} onclick={save}>
                {sBusy ? "Saving…" : updating || nameTaken ? "Save new version" : "Add this script"}
              </button>
              <p class="m-0 px-1 text-xs leading-relaxed text-ink-5">
                Signed with your key. Anyone in the mesh whose grants match can then run it by name.
              </p>
            </div>
          </div>
        </div>
      {/if}
    </div>
  {:else}
    <!-- ===================== tabs + primary action ===================== -->
    <div class="flex flex-none items-center gap-3">
      <div class="inline-flex rounded-md bg-surface-2 p-[3px]">
        {#each [["scripts", "Scripts", app.loadErrors.services ? null : app.services.length], ["sources", "Sources", (app.loadErrors.sources ?? app.loadErrors.bundles) ? null : origins.length], ["distribution", "Definitions", null], ["farm", "Build farm", null]] as [id, label, n] (id)}
          <button
            class={`flex cursor-pointer items-center gap-1.5 rounded-sm px-[13px] py-[6px] text-sm transition-colors ${tab === id ? "bg-surface font-semibold text-ink" : "font-medium text-ink-4 hover:text-ink"}`}
            style={tab === id ? "box-shadow: var(--sh-control);" : ""}
            onclick={() => (tab = id as Tab)}
          >
            {label}
            {#if n !== null}
              <span class="num rounded-full px-1.5 text-2xs font-semibold" style={tab === id ? "background: var(--accent-wash); color: var(--accent-ink);" : "background: var(--surface-1); color: var(--ink-5);"}>{n}</span>
            {/if}
          </button>
        {/each}
      </div>
      <div class="ml-auto flex items-center gap-2">
        {#if !canContent}
          <span class="text-xs text-ink-5">you need the developer or admin role to change these</span>
        {/if}
        {#if tab === "distribution"}
          <!-- Nothing to add here: this screen is what already replicates. -->
        {:else if tab === "scripts"}
          <button class={`${ui.primary} flex cursor-pointer items-center gap-1.5`} disabled={!canContent} onclick={() => openWizard()}>
            <Icon name="plus" size={16} /> Add a script
          </button>
        {:else}
          <button class={`${ui.secondary} cursor-pointer`} disabled={!canContent || folderBusy} onclick={addFolder}>{folderBusy ? "Copying…" : "Add a folder"}</button>
          <button class={`${ui.primary} flex cursor-pointer items-center gap-1.5`} disabled={!canContent} onclick={openRepo}>
            <Icon name="plus" size={16} /> Connect a repo
          </button>
        {/if}
      </div>
    </div>

    {#if tab === "farm"}
      <!-- ============ build farm — setup, not a run (R6 P1) ============ -->
      <div class="min-h-0 flex-1 overflow-y-auto">
        <BuildFarm />
      </div>
    {:else if tab === "distribution"}
      <!-- ===================== definitions (M24 §3.1) ===================== -->
      <div class="min-h-0 flex-1 overflow-y-auto">
        <Distribution />
      </div>
    {:else if tab === "scripts"}
      <!-- ========================= scripts ========================= -->
      {#if app.loadErrors.services}
        <LoadError class="mb-3 flex-none" what="the scripts" error={app.loadErrors.services} />
      {/if}
      {#if app.services.length === 0 && app.loadErrors.services}
        <!-- Said above (UX-2): "No scripts yet — Get started" under a
             refusal invited a person to add what they cannot see. -->
      {:else if app.services.length === 0}
        <div class="card flex min-h-0 flex-1 items-center justify-center">
          <div class="w-full max-w-md px-6 py-10 text-center">
            <span class="inline-grid h-14 w-14 place-items-center rounded-lg" style="background: var(--accent-wash); color: var(--accent-ink);">
              <Icon name="script" size={26} />
            </span>
            <h2 class="mt-4 mb-1.5 text-lg font-bold tracking-[-.025em] text-ink">No scripts yet</h2>
            <p class="mx-auto mt-0 mb-6 max-w-sm text-base leading-relaxed text-ink-4">
              Point Meshware at a Git repo or a folder, pick a file inside it, and it becomes something anyone in the mesh
              can run by name.
            </p>
            <div class="space-y-2 text-left">
              {#each [["Add where your scripts live", named.length > 0], ["Pick a file and give it a name", false]] as [text, done], i (i)}
                <div class="flex items-center gap-3 rounded-md bg-canvas px-4 py-3">
                  <span
                    class="num grid h-6 w-6 flex-none place-items-center rounded-full text-xs font-bold"
                    style={done ? "background: var(--accent); color: #fff;" : "background: var(--surface-2); color: var(--ink-5);"}
                  >
                    {done ? "✓" : i + 1}
                  </span>
                  <span class="text-base" style={done ? "color: var(--ink-5);" : "color: var(--ink-2); font-weight: 600;"}>{text}</span>
                </div>
              {/each}
            </div>
            <button class={`${ui.primary} mt-4 w-full cursor-pointer`} disabled={!canContent} onclick={() => openWizard()}>
              {named.length ? "Pick a script" : "Get started"}
            </button>
            <!-- The line that makes the button pressable (L1 P4): what it
                 touches, and what it does not do. -->
            <div class="mt-2.5 text-xs text-ink-5">
              Copies them into the mesh · nothing runs until you ask it to
            </div>
          </div>
        </div>
      {:else}
        <div class="grid min-h-0 flex-1 items-start gap-4" style={selected ? "grid-template-columns: minmax(0,1fr) 380px;" : ""}>
          <div class="card flex max-h-full min-h-0 flex-col overflow-hidden">
            {#if app.services.length > 7}
              <div class="relative flex-none border-b border-edge p-3">
                <span class="pointer-events-none absolute top-1/2 left-6 -translate-y-1/2 text-ink-6"><Icon name="search" size={15} /></span>
                <input
                  bind:value={scriptQuery}
                  placeholder="Search scripts…"
                  class="h-9 w-full rounded-md bg-canvas pr-3 pl-9 text-sm text-ink outline-none placeholder:text-ink-6"
                />
              </div>
            {/if}
            <div class="dark-scroll min-h-0 flex-1 overflow-y-auto">
              {#each shownScripts as s (s.name)}
                {@const args = argSummary(s)}
                {@const active = selectedName === s.name}
                <div
                  class="group relative flex items-stretch border-b border-edge transition-colors last:border-0"
                  style={active ? "background: var(--accent-select);" : ""}
                >
                  {#if active}<span class="absolute top-0 bottom-0 left-0 w-[3px]"></span>{/if}
                  <button
                    class={`flex min-w-0 flex-1 cursor-pointer items-center gap-3.5 py-3.5 pr-3 pl-5 text-left transition-colors ${active ? "" : "group-hover:bg-surface-2"}`}
                    onclick={() => (selectedName = active ? null : s.name)}
                  >
                    {@render runtimeBadge(s.runtime, 34)}
                    <span class="min-w-0 flex-1">
                      <span class="flex items-center gap-2">
                        <span class="truncate text-base font-semibold text-ink">{s.name}</span>
                        <span class="num flex-none rounded-full px-1.5 py-px text-2xs font-semibold" style="background: var(--surface-2); color: var(--ink-4);">
                          v{s.version}{#if s.versions.length > 1}<span style="color: var(--ink-5);">&nbsp;of {s.versions.length}</span>{/if}
                        </span>
                        {#each s.tags as t}<Tag text={t} />{/each}
                      </span>
                      {#if s.description}<span class="mt-0.5 block truncate text-sm text-ink-4">{s.description}</span>{/if}
                      <span class="mono mt-1 block truncate text-xs text-ink-5">{s.workingDir ? `${s.workingDir}/` : ""}{s.entrypoint}</span>
                    </span>
                    <span class="w-[132px] flex-none text-right">
                      {#if args === null}
                        <span class="text-xs text-ink-5">no arguments</span>
                      {:else if args.total < 0}
                        <span class="rounded-full px-2 py-1 text-xs font-semibold" style="background: var(--surface-2); color: var(--ink-4);">custom schema</span>
                      {:else}
                        <span class="num rounded-full px-2 py-1 text-xs font-semibold" style="background: var(--accent-wash); color: var(--accent-ink);">
                          {args.total} arg{args.total === 1 ? "" : "s"}{#if args.required}<span style="color: var(--status-error-ink);">&nbsp;· {args.required} req</span>{/if}
                        </span>
                      {/if}
                    </span>
                  </button>
                  <div class="flex flex-none items-center pr-4 pl-1">
                    <button
                      class="btn btn-secondary h-9 px-3.5 text-sm"
                     
                      title={`Set up a run of ${s.name}`}
                      onclick={() => runIt(s.name)}
                    >
                      New run…
                    </button>
                  </div>
                </div>
              {:else}
                <div class="flex h-full flex-col items-center justify-center gap-1.5 text-center">
                  <span class="text-ink-faint"><Icon name="search" size={26} /></span>
                  <span class="text-base font-semibold text-ink-3">No script matches “{scriptQuery.trim()}”</span>
                </div>
              {/each}
            </div>
            <div class="flex flex-none items-center gap-2 border-t border-edge px-5 py-2.5 text-xs text-ink-5">
              <span class="num">{shownScripts.length} of {app.services.length} scripts</span>
              <span class="text-ink-faint">·</span>
              <span class="num">{origins.length} source{origins.length === 1 ? "" : "s"}</span>
            </div>
          </div>

          {#if selected}
            <div class="card fade-in flex h-full min-h-0 flex-col overflow-hidden">
              <div class="flex flex-none items-start gap-3 border-b border-edge px-5 py-4">
                {@render runtimeBadge(selected.runtime, 36)}
                <div class="min-w-0 flex-1">
                  <div class="truncate text-md font-bold tracking-[-.015em] text-ink">{selected.name}</div>
                  <div class="text-xs text-ink-5">{rt(selected.runtime).label} · v{selected.version}</div>
                </div>
                <button
                  class="grid h-8 w-8 flex-none cursor-pointer place-items-center rounded-sm text-ink-5 transition-colors hover:bg-surface-2 hover:text-ink"
                  title="Close"
                  aria-label="Close"
                  onclick={() => (selectedName = null)}
                >
                  <Icon name="close" size={15} />
                </button>
              </div>

              <div class="dark-scroll min-h-0 flex-1 overflow-y-auto px-5 py-4">
                {#if selected.description}<p class="mt-0 mb-3.5 text-sm leading-relaxed text-ink-4">{selected.description}</p>{/if}

                <div class="on-code overflow-hidden rounded-md bg-code px-3.5 py-2.5">
                  <div class="mono selectable text-xs leading-relaxed break-all" style="color: var(--ink-code-1);">
                    <span style="color: var(--accent-bright);">{runtimeProgram(selected.runtime, selected.entrypoint)}</span>
                    {#if selectedArgs}{#each selectedArgs as a (a.name)}<span style={a.required ? "" : "color: var(--ink-code-4);"}>{` --${a.name} ${a.default || `<${a.name}>`}`}</span>{/each}{/if}
                  </div>
                </div>

                <div class="mt-4">
                  <div class="label mb-2">Arguments</div>
                  {#if !selected.hasContract}
                    <div class="text-sm text-ink-5">None declared — whatever you pass is forwarded as-is.</div>
                  {:else if selectedArgs}
                    <div class="space-y-1.5">
                      {#each selectedArgs as a (a.name)}
                        <div class="rounded-sm bg-canvas px-3 py-2">
                          <div class="flex items-baseline gap-2">
                            <span class="mono text-sm font-semibold text-ink-2">--{a.name}</span>
                            <span class="text-xs text-ink-5">{ARG_KINDS.find((k) => k.id === a.kind)?.label}</span>
                            {#if a.required}<span class="ml-auto text-2xs font-bold tracking-[.05em]" style="color: var(--status-error);">REQUIRED</span>{/if}
                          </div>
                          {#if a.description}<div class="mt-0.5 text-xs text-ink-4">{a.description}</div>{/if}
                          {#if a.default}<div class="mono mt-0.5 text-xs text-ink-5">default: {a.default}</div>{/if}
                        </div>
                      {/each}
                    </div>
                  {:else}
                    <pre class="mono selectable max-h-40 overflow-auto rounded-sm bg-canvas p-2.5 text-xs leading-relaxed text-ink-3">{selected.contract}</pre>
                  {/if}
                </div>

                <div class="mt-4">
                  <div class="label mb-2">Comes from</div>
                  <button
                    class="group flex w-full cursor-pointer items-center gap-2.5 rounded-md bg-canvas px-3 py-2.5 text-left transition-colors hover:bg-surface-2 disabled:cursor-default"
                    disabled={!selectedOrigin}
                    onclick={() => {
                      tab = "sources";
                      browsing = selectedOrigin?.key ?? null;
                      selectedName = null;
                    }}
                  >
                    {@render originAvatar(selectedOrigin?.kind ?? "snapshot", 28)}
                    <span class="min-w-0 flex-1">
                      <span class="block truncate text-sm font-semibold text-ink-2">{selectedOrigin?.name ?? "a snapshot"}</span>
                      <span class="mono block truncate text-2xs text-ink-5">{selected.bundleShort}</span>
                    </span>
                    {#if selectedOrigin}
                      <span class="flex-none text-ink-faint transition-colors group-hover:text-accent-ink"><Icon name="chevron-right" size={15} /></span>
                    {/if}
                  </button>
                </div>

                {#if selected.versions.length > 1}
                  <div class="mt-4">
                    <div class="label mb-2">Versions</div>
                    <div class="flex flex-wrap gap-1.5">
                      {#each [...selected.versions].sort((a, b) => b - a) as v (v)}
                        {#if v === selected.version}
                          <span class="mono rounded-full px-2.5 py-1 text-xs font-semibold" style="background: var(--accent); color: #fff;">v{v} · live</span>
                        {:else}
                          <button
                            class="mono cursor-pointer rounded-full border border-edge-dashed px-2.5 py-1 text-xs font-semibold text-ink-3 transition-colors hover:border-accent hover:text-accent-ink disabled:cursor-not-allowed disabled:opacity-40"
                            disabled={rollBusy || !canContent}
                            onclick={() => rollback(selected, v)}
                          >
                            v{v} · restore
                          </button>
                        {/if}
                      {/each}
                    </div>
                  </div>
                {/if}

                <div class="mt-4">
                  <div class="label mb-1.5">From a terminal</div>
                  <div class="mono selectable rounded-sm bg-canvas px-3 py-2 text-xs text-ink-4">meshnode service run {selected.name}</div>
                </div>
              </div>

              <div class="flex-none space-y-2 border-t border-edge p-4">
                <div class="grid grid-cols-2 gap-2">
                  <button class={`${ui.secondary} cursor-pointer`} onclick={() => runIt(selected.name)}>New run…</button>
                  <button class={`${ui.secondary} cursor-pointer`} disabled={!canContent} onclick={() => editScript(selected)}>Edit…</button>
                </div>
                <button
                  class="h-9 w-full cursor-pointer rounded-sm border text-sm font-semibold transition-colors disabled:cursor-not-allowed disabled:opacity-40"
                  style="border-color: var(--status-error-edge); color: var(--status-error);"
                  disabled={!canContent}
                  onclick={() => {
                    retireErr = null;
                    retireTarget = selected;
                  }}
                >
                  Remove this script
                </button>
              </div>
            </div>
          {/if}
        </div>
      {/if}
    {:else}
      <!-- ========================= sources ========================= -->
      {#if repoOpen}<div class="flex-none">{@render repoForm()}</div>{/if}

      {@const originsErr = app.loadErrors.sources ?? app.loadErrors.bundles}
      {#if originsErr}
        <LoadError class="mb-3 flex-none" what="where scripts are read from" error={originsErr} />
      {/if}
      {#if origins.length === 0 && originsErr}
        <!-- Said above (UX-2). -->
      {:else if origins.length === 0}
        <div class="card flex min-h-0 flex-1 items-center justify-center">
          <div class="w-full max-w-lg px-6 py-10 text-center">
            <span class="inline-grid h-14 w-14 place-items-center rounded-lg" style="background: var(--accent-wash); color: var(--accent-ink);">
              <Icon name="folder" size={26} />
            </span>
            <h2 class="mt-4 mb-1.5 text-lg font-bold tracking-[-.025em] text-ink">Nowhere to read scripts from</h2>
            <p class="mx-auto mt-0 mb-6 max-w-sm text-base leading-relaxed text-ink-4">
              A Git repo stays in sync as you push to it. A folder is a one-time snapshot of a directory on this computer.
            </p>
            {@render addTiles()}
          </div>
        </div>
      {:else}
        <div class="grid items-start gap-4" style={browsed ? "grid-template-columns: minmax(0,1fr) 420px;" : ""}>
          <div class="card flex flex-col overflow-hidden">
            <div class="dark-scroll overflow-y-auto">
              {#each origins as o (o.key)}
                {@const open = browsing === o.key}
                {@const users = scriptsOf(o)}
                <div class="group relative flex items-stretch border-b border-edge transition-colors last:border-0" style={open ? "background: var(--accent-select);" : ""}>
                  {#if open}<span class="absolute top-0 bottom-0 left-0 w-[3px]"></span>{/if}
                  <button
                    class={`flex min-w-0 flex-1 cursor-pointer items-center gap-3.5 py-3.5 pr-3 pl-5 text-left transition-colors ${open ? "" : "group-hover:bg-surface-2"}`}
                    onclick={() => (browsing = open ? null : o.key)}
                  >
                    {@render originAvatar(o.kind, 34)}
                    <span class="min-w-0 flex-1">
                      <span class="flex items-center gap-2">
                        <span class="truncate text-base font-semibold text-ink">{o.name}</span>
                        <span class="flex-none rounded-full px-1.5 py-px text-2xs font-semibold" style="background: var(--surface-2); color: var(--ink-4);">{KIND_LABEL[o.kind]}</span>
                        {#if o.code}<span class="mono flex-none text-2xs text-ink-6">{o.code}</span>{/if}
                      </span>
                      <span class="mono mt-0.5 block truncate text-xs text-ink-5">{o.detail}</span>
                      <span class="mt-1 block truncate text-xs text-ink-5">
                        {#if o.git && !o.git.lastCommit}
                          <span style="color: var(--status-warn-ink);">not read yet</span>
                        {:else}
                          <span class="num">{o.fileCount ?? "?"}</span>{o.fileCount === 1 ? " file" : " files"}{#if o.updatedAtMicros}{` · read ${ago(o.updatedAtMicros)}`}{/if}{#if o.git}{` · checks ${fmtDuration(o.git.intervalSecs)}`}{/if}
                        {/if}
                        {#if users.length}{" · used by "}<span class="text-ink-4">{users.join(", ")}</span>{/if}
                      </span>
                    </span>
                  </button>
                  <div class="flex flex-none items-center gap-1.5 pr-4 pl-1">
                    {#if o.git}
                      <button class={`${ui.rowAction} cursor-pointer`} disabled={syncing === o.key || !canContent} onclick={() => syncNow(o)}>
                        {syncing === o.key ? "Reading…" : "Check now"}
                      </button>
                    {/if}
                    <button class={`${ui.rowAction} cursor-pointer`} disabled={!o.bundleId || !canContent} onclick={() => openWizard(o)}>Add a script</button>
                    {#if o.kind !== "snapshot"}
                      <button
                        class={`${ui.rowDanger} cursor-pointer`}
                        disabled={!canContent}
                        onclick={() => {
                          removeErr = null;
                          removeTarget = o;
                        }}
                      >
                        Remove
                      </button>
                    {/if}
                  </div>
                </div>
              {/each}
            </div>
          </div>

          {#if browsed}
            <div class="card fade-in flex h-full min-h-0 flex-col overflow-hidden">
              <div class="flex-none border-b border-edge px-5 pt-4 pb-3.5">
                <div class="flex items-center gap-2.5">
                  {@render originAvatar(browsed.kind, 30)}
                  <div class="min-w-0">
                    <div class="truncate text-base font-bold tracking-[-.015em] text-ink">{browsed.name}</div>
                    <div class="text-xs text-ink-5">{pickable.length} of {allFiles.length} {allFiles.length === 1 ? "file" : "files"}</div>
                  </div>
                  <button
                    class="ml-auto grid h-8 w-8 flex-none cursor-pointer place-items-center rounded-sm text-ink-5 transition-colors hover:bg-surface-2 hover:text-ink"
                    title="Close"
                    aria-label="Close"
                    onclick={() => (browsing = null)}
                  >
                    <Icon name="close" size={15} />
                  </button>
                </div>
                <div class="mt-3">{@render browserToolbar(browsed.bundleId)}</div>
              </div>
              <div class="dark-scroll min-h-0 flex-1 overflow-y-auto">
                {@render fileBrowser(
                  browsed.bundleId ?? "",
                  pickable,
                  (f) => {
                    const target = browsed;
                    if (target) {
                      openWizard(target);
                      chooseFile(f);
                    }
                  },
                  "add as a script",
                )}
              </div>
            </div>
          {/if}
        </div>
      {/if}
    {/if}
  {/if}
</div>

<!-- remove a script -->
<Modal open={retireTarget !== null} kicker="remove script" peer={retireTarget?.name ?? ""} onclose={() => (retireTarget = null)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Remove {retireTarget?.name}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      Nobody will be able to run it by that name any more. Its {retireTarget?.versions.length} version{retireTarget?.versions.length === 1 ? "" : "s"},
      the files it came from, and everything it has already run stay exactly as they are.
    </p>
    {#if retireErr}<div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{retireErr}</div>{/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button class="h-11 cursor-pointer rounded-md border text-base font-semibold text-inkcode-2" style="border-color: var(--edge-code); background: transparent;" onclick={() => (retireTarget = null)}>Keep it</button>
      <button class="h-11 cursor-pointer rounded-md border text-base font-semibold transition-colors disabled:opacity-40" style="border-color: var(--status-error-edge); color: var(--status-error-ink);" disabled={retireBusy} onclick={doRetire}>
        {retireBusy ? "Removing…" : "Remove"}
      </button>
    </div>
  </div>
</Modal>

<!-- remove a source -->
<Modal open={removeTarget !== null} kicker="remove source" peer={removeTarget?.name ?? ""} onclose={() => (removeTarget = null)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Stop using {removeTarget?.name}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      {#if removeTarget?.git}
        Meshware stops re-reading <span class="mono" style="color: var(--ink-code-2);">{removeTarget?.git?.url}</span>.
      {:else}
        The folder itself is untouched — this only forgets where the snapshot came from.
      {/if}
    </p>
    {#if removeTarget}
      {@const used = scriptsOf(removeTarget)}
      {#if used.length}
        <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
          <span style="color: var(--status-error-ink);">{used.join(", ")}</span> keep working — they hold onto the copy they were built from.
        </p>
      {/if}
    {/if}
    {#if removeErr}<div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{removeErr}</div>{/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button class="h-11 cursor-pointer rounded-md border text-base font-semibold text-inkcode-2" style="border-color: var(--edge-code); background: transparent;" onclick={() => (removeTarget = null)}>Cancel</button>
      <button class="h-11 cursor-pointer rounded-md border text-base font-semibold transition-colors disabled:opacity-40" style="border-color: var(--status-error-edge); color: var(--status-error-ink);" disabled={removeBusy} onclick={doRemove}>
        {removeBusy ? "Removing…" : "Remove"}
      </button>
    </div>
  </div>
</Modal>

<style>
  /* --- the file browser ------------------------------------------------- */

  /* A folder header is a lid, not a row: it gets a hairline of accent down
     its leading edge while it is open, so the eye can follow which files
     belong to which container without reading a single path. */
  .dirhead {
    box-shadow: inset 2px 0 0 var(--accent-ink);
    transition:
      box-shadow 140ms ease,
      background-color 140ms ease;
  }
  .dirhead.shut {
    box-shadow: inset 2px 0 0 var(--edge);
  }
  .dirhead:hover {
    background: var(--surface-3, var(--surface-2)) !important;
  }
  .dirhead:hover .chev {
    color: var(--accent-ink);
  }

  /* The rail continues the header's edge down past its files, so a nested
     folder reads as nested even when its parent has scrolled off the top. */
  .filerow {
    position: relative;
  }
  .filerow::before {
    content: "";
    position: absolute;
    inset-block: 0;
    left: var(--rail, 14px);
    width: 1px;
    background: var(--edge);
  }
  /* The tick that meets the rail — the file is *on* the branch, not beside it. */
  .filerow::after {
    content: "";
    position: absolute;
    top: 50%;
    left: var(--rail, 14px);
    width: 7px;
    height: 1px;
    background: var(--edge);
  }
  .filerow:hover::before,
  .filerow:hover::after {
    background: var(--accent-ink);
  }

  /* Support files are dimmed because Meshware is not suggesting them — but
     hovering is a statement of intent, so they come back to full strength. */
  .filerow.dim {
    opacity: 0.55;
  }
  .filerow.dim:hover {
    opacity: 1;
  }
</style>
