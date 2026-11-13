<script lang="ts">
  import { hueOf, hueStyle } from "../lib/hues";
  // Run (MASTERPLAN §14.2, and M9 §6 folded in). Two questions, in order:
  //
  //   1. What do you want to run?   a saved script, or a one-off command
  //   2. Where should it run?       anywhere / one node / a group of nodes
  //   …then Go, with a plain sentence saying what happens.
  //
  // "How many times?" used to be a third numbered question between these
  // two, and it confused more people than it served: almost every run is
  // run once, and a question whose answer is nearly always "the default"
  // is not a question — it is a checkbox. So per-item mode (what used to
  // be a separate Compute section, M9 §6.1) is now one checkbox at the end
  // of "what", revealing the item list only when ticked. An operator
  // restarting nginx on fifty boxes never sees it; a person with a list
  // finds it exactly where their list goes.
  //
  // The target selector DSL (`any` | `name=x` | `tag=t` | `id=…`) is the
  // engine's; it is not something to type from memory, so "where" is one
  // dropdown listing the choices the mesh actually has — anywhere, each
  // group, each node — with the raw expression kept as its last entry for
  // the people who know it. Everything that is not needed to press Run —
  // timeout, priority, batching, halt thresholds, per-run env — lives
  // under Options.
  import {
    api,
    errText,
    parseDuration,
    type NodeView,
    type RunEvent,
    type ParallelEvent,
    type NodeProgressView,
    type Priority,
    type JobPreviewView,
    type ResultRowView,
  } from "../lib/api";
  import { argvOf, contractFields, runtimeMeta, runtimeProgram } from "../lib/contract";
  import { originsOf } from "../lib/origins";
  import type { DurationRangeView } from "../lib/bindings/DurationRangeView";
  import { app, ui } from "../lib/state.svelte";
  import Dot from "../lib/components/Dot.svelte";
  import Hint from "../lib/components/Hint.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import StateBadge from "../lib/components/StateBadge.svelte";
  import ParallelResults, {
    type ItemResult,
    type ReduceResult,
  } from "../lib/components/ParallelResults.svelte";
  import AnswersModal from "../lib/components/AnswersModal.svelte";
  import PreviewPanel from "../lib/components/PreviewPanel.svelte";
  import LibraryView from "./Library.svelte";
  import BuildFarm from "./BuildFarm.svelte";
  import RunModeIntro from "../lib/components/RunModeIntro.svelte";
  import { tabline, staggerIn, reveal } from "../lib/motion";
  import { LineRing, OUTPUT_MAX_LINES } from "../lib/lines";
  import { onDestroy } from "svelte";

  type Mode = "script" | "command";
  type Where = "any" | "node" | "group" | "custom";

  // -- 1. what ------------------------------------------------------------------
  let mode = $state<Mode>("script");
  let command = $state("");
  let serviceName = $state("");
  let inputs = $state<Record<string, string>>({});
  let extraInputs = $state<{ k: string; v: string }[]>([]);
  let env = $state<{ k: string; v: string }[]>([]);

  let scriptFilter = $state("");

  const service = $derived(app.services.find((s) => s.name === serviceName) ?? null);
  const fields = $derived(contractFields(service?.contract ?? null));
  const shownServices = $derived.by(() => {
    const q = scriptFilter.trim().toLowerCase();
    if (!q) return app.services;
    return app.services.filter(
      (s) => s.name.toLowerCase().includes(q) || s.description.toLowerCase().includes(q) || s.tags.some((t) => t.toLowerCase().includes(q)),
    );
  });

  // Seed declared defaults whenever the chosen script (or its version) changes.
  let seededFor: string | null = null;
  $effect(() => {
    const key = `${serviceName}:${service?.version ?? 0}`;
    if (seededFor === key) return;
    seededFor = key;
    const defaults: Record<string, string> = {};
    for (const f of fields) if (f.default) defaults[f.name] = f.default;
    inputs = defaults;
    extraInputs = [];
  });

  const runInputs = $derived.by((): [string, string][] => {
    const declared = fields.length
      ? (Object.entries(inputs).filter(([, v]) => v !== "") as [string, string][])
      : [];
    const free = extraInputs.filter((r) => r.k.trim()).map((r) => [r.k.trim(), r.v] as [string, string]);
    return [...declared, ...free];
  });

  // A <select> bound to an undefined key draws blank instead of its
  // "choose…" row, so every declared field starts as the empty string.
  $effect(() => {
    for (const f of fields) if (inputs[f.name] === undefined) inputs[f.name] = "";
  });
  const missing = $derived(fields.filter((f) => f.required && !(inputs[f.name] ?? "").trim()).map((f) => f.name));

  // -- once per item (folded into "what" — M9 §5.4) -----------------------------
  let repeat = $state(false);
  let itemText = $state("");
  /** On Once, the list field is folded until asked for: the offer stays
   *  (R1 P2) as one line, the five-row textarea does not. */
  let listOpen = $state(false);
  /** Which of a script's inputs receives each item (M9 §6.4). */
  let itemInput = $state("");

  const items = $derived(
    itemText
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l.length > 0),
  );

  /** Per-item mode needs a node that advertises its hardware (M9 §5.1). On a
      mesh of older nodes the checkbox is hidden rather than offered and
      then refused. */
  const meshDoesCompute = $derived(app.nodes.some((n) => n.capacity !== null));
  const perItem = $derived(repeat && meshDoesCompute);

  // -- what the script said about itself (R1 P3, via R2's header) ---------------
  //
  // `meshnode add` writes the non-argument half of a `#meshware:` header into
  // the contract under `x-meshware`: which input a list fills in, and what one
  // item needs. Reading it here is the difference between a person having to
  // know that a transcode wants two cores per item and the page already
  // knowing. Absent — a service registered by hand, or before this existed —
  // every field is undefined and nothing below changes.
  const declared = $derived.by(() => {
    const raw = service?.contract;
    if (!raw) return null;
    try {
      const x = JSON.parse(raw)["x-meshware"];
      return x && typeof x === "object" ? (x as Record<string, unknown>) : null;
    } catch {
      return null;
    }
  });
  const declaredItem = $derived(
    typeof declared?.item === "string" ? (declared.item as string) : null,
  );
  const declaredCores = $derived(
    typeof declared?.cores === "number" ? String(declared.cores) : null,
  );

  // -- how this script was set up last time (2026-09) --------------------------
  //
  // `x-meshware.run` is the other half of what a script says about itself:
  // not what it takes, but how it is run. Getting a script running *right*
  // — once per item, over a tag, four cores an item, output left where it
  // lands, a 45-minute limit — is a dozen decisions across four
  // disclosures, and every one of them used to be thrown away the moment
  // the run finished.
  //
  // Read here, applied by `applyDefaults` below, and written back by the
  // checkbox beside Run. Absent leaves this screen exactly as it was.
  const savedRun = $derived.by((): Record<string, unknown> | null => {
    const r = declared?.run;
    return r && typeof r === "object" && !Array.isArray(r) ? (r as Record<string, unknown>) : null;
  });
  /** A remembered string, or null — never "" (which would clear a field
   *  the file was simply silent about). */
  function rs(v: unknown): string | null {
    return typeof v === "string" && v.trim() ? v.trim() : null;
  }
  function rn(v: unknown): number | null {
    const n = typeof v === "number" ? v : typeof v === "string" ? Number(v) : NaN;
    return Number.isFinite(n) ? n : null;
  }
  function rb(v: unknown): boolean | null {
    return typeof v === "boolean" ? v : null;
  }
  /** A destination the script declared and this run supplied a value for. */
  const declaredOut = $derived.by(() => {
    const writes = declared?.writes;
    if (!Array.isArray(writes) || writes.length === 0) return null;
    const first = String(writes[0]);
    const arg = first.startsWith("$") ? first.slice(1).replace(/[{}]/g, "") : null;
    const value = arg ? (inputs[arg] ?? "").trim() : first;
    return value || null;
  });

  // A sensible default for which input takes the item: what the script
  // declared, else the first required text field, else the first field.
  // Re-seeded when the script changes.
  $effect(() => {
    const names = fields.map((f) => f.name);
    if (itemInput && names.includes(itemInput)) return;
    itemInput =
      (declaredItem && names.includes(declaredItem) ? declaredItem : null) ??
      fields.find((f) => f.required && f.type === "string")?.name ??
      names[0] ??
      "";
  });

  /** The exact command the first item will run, so an hour is not spent on
      a typo. Scripts show their argv; commands show the substitution. */
  const firstItemPreview = $derived.by(() => {
    const first = items[0];
    if (first === undefined) return null;
    if (mode === "command") {
      if (!command.trim()) return null;
      return command.includes("{}") ? command.replaceAll("{}", first) : `${command} ${first}`;
    }
    if (!service) return null;
    const withItem = itemInput
      ? [...runInputs.filter(([k]) => k !== itemInput), [itemInput, first] as [string, string]]
      : runInputs;
    return `${runtimeProgram(service.runtime, service.entrypoint)} ${argvOf(withItem)}`.trim();
  });

  // -- 2. where -----------------------------------------------------------------
  let where = $state<Where>("any");
  let nodeName = $state("");
  let groupTag = $state("");
  let custom = $state("");
  /** one = the scheduler picks; all = every match; some = the best N. */
  let spread = $state<"one" | "all" | "some">("one");
  let someCount = $state(2);
  let batch = $state("");

  const liveNodes = $derived(app.nodes.filter((n) => !n.revoked && !n.left && n.executing));
  const groups = $derived([...new Set(liveNodes.flatMap((n) => n.tags))].filter((t) => t !== "no-execute").sort());

  const selector = $derived.by(() => {
    if (where === "node") return nodeName ? `name=${nodeName}` : "";
    if (where === "group") return groupTag ? `tag=${groupTag}` : "";
    if (where === "custom") return custom.trim();
    return "any";
  });

  // Live target preview (debounced — Doherty: the sentence must keep up).
  let preview = $state<NodeView[]>([]);
  let previewErr = $state<string | null>(null);
  /**
   * Has the mesh actually answered for the selector on screen?
   *
   * An empty list means two entirely different things — *"I asked and
   * nothing matches"* and *"I have not asked yet"* — and this screen used
   * to print the first sentence for both. "No node matches this." on a
   * mesh with five nodes in it is not a slow render, it is a lie, and it
   * is the one this view is read for. So the unanswered state is its own
   * state, and every sentence downstream of the preview waits for it.
   */
  let previewKnown = $state(false);
  $effect(() => {
    const sel = selector || "any";
    previewKnown = false;
    const t = setTimeout(async () => {
      try {
        const rows = await api.targetPreview(sel);
        preview = rows;
        previewErr = null;
      } catch (e) {
        preview = [];
        // Never an empty string: a falsy error would fall through to the
        // "nothing matches" branch and blame the mesh for a failed call.
        previewErr = errText(e) || "could not ask the mesh which nodes match";
      } finally {
        previewKnown = true;
      }
    }, 200);
    return () => clearTimeout(t);
  });

  // C4 §8: a gang is N nodes that start at the same moment or not at all —
  // the shape a training run or anything with a barrier needs, and the
  // opposite question from a fan-out. Off unless someone asks for it, and
  // mutually exclusive with spreading for exactly that reason.
  let gangSize = $state("");
  let gangPlacement = $state<"pack" | "spread">("pack");
  let gangRestart = $state(false);
  // C5 §9: accelerators. A device request is a *filter* — a node without
  // the card is not a candidate, not a bad one — so the preview below
  // counts the mesh against it exactly as the memory one does.
  // N2 — the ports this job needs, one per line. Parity with
  // `meshnode run --port`, and the same reserved key on the other side, so
  // a job launched from here and one launched from the terminal ask for a
  // port in exactly the same way.
  let portsText = $state("");
  const ports = $derived(
    portsText
      .split(/[\n,]/)
      .map((p) => p.trim())
      .filter(Boolean),
  );

  // N3 — register this job under a name while it runs. Parity with
  // `meshnode run --register`, and refused without a port on both sides:
  // a name with no address resolves to nothing.
  let registerAs = $state("");

  // N5 — the volumes this job claims, one per line. Parity with
  // `meshnode run --volume`, same reserved key, same three spellings:
  // `data`, `data:ro`, `data:/mnt/data:rw`.
  let volumesText = $state("");
  const volumes = $derived(
    volumesText
      .split(/[\n,]/)
      .map((v) => v.trim())
      .filter(Boolean),
  );

  let gpus = $state("");
  let vram = $state("");
  let deviceModel = $state("");
  let gpuShare = $state(false);
  const gpusN = $derived(gpus.trim() ? Math.max(1, Math.floor(Number(gpus))) : null);
  const vramMb = $derived(sizeMb(vram));
  const gang = $derived.by((): number | null => {
    const n = Number(gangSize.trim());
    return gangSize.trim() && Number.isFinite(n) && n >= 2 ? Math.floor(n) : null;
  });
  /** Waiting for reservations: `held` of `size`, and who is ahead. */
  let gangWait = $state<{ held: number; size: number; round: number; behind: string | null } | null>(null);

  /** The spread control only appears when there is a choice to make, so a
      stale "all" from a wider selector never survives narrowing to one node. */
  const spreading = $derived(!perItem && !gang && spread !== "one" && preview.length > 1);
  /** null = one scheduler-picked node; 0 = every match; n = the best n. */
  const fanout = $derived(!spreading ? null : spread === "all" ? 0 : Math.max(1, someCount));
  /** How many nodes this run actually lands on. */
  const hitCount = $derived.by(() => {
    if (!spreading) return Math.min(1, preview.length);
    if (spread === "all") return preview.length;
    return Math.min(Math.max(1, someCount), preview.length);
  });

  // -- options ------------------------------------------------------------------
  let optionsOpen = $state(false);
  let priority = $state<Priority>("normal");
  let timeout = $state("");
  let haltOnFail = $state("");
  // Per-item only (M9 §5.3, §5.4).
  let cpus = $state("");
  let mem = $state("");
  let disk = $state("");
  let chunkItems = $state("");
  // M9 §5.5/§5.6: keep each item's output where it was produced, and
  // optionally merge those outputs where most of them already are. Off by
  // default — the operator restarting nginx on fifty boxes has nothing to
  // keep, and should never be shown a data plane.
  let keepOutput = $state(false);
  let reduce = $state("");
  let reduceOn = $state("");

  /** `48G` / `512M` / a bare number of MB, as the CLI's --mem takes. */
  function sizeMb(raw: string): number | null {
    const text = raw.trim().replace(/b$/i, "");
    if (!text) return null;
    const m = /^([0-9]*\.?[0-9]+)\s*([kmgt]?)$/i.exec(text);
    if (!m) return NaN;
    const scale = { "": 1, k: 1 / 1024, m: 1, g: 1024, t: 1024 * 1024 }[m[2].toLowerCase()] ?? 1;
    return Math.round(Number(m[1]) * scale);
  }

  const memMb = $derived(sizeMb(mem));
  const diskMb = $derived(sizeMb(disk));
  const cpusN = $derived(cpus.trim() ? Math.max(1, Math.floor(Number(cpus))) : null);

  // Seed "what one item needs" from what the script declared (R1 P3).
  //
  // This is the setting that decides whether sixteen encoders fight over
  // eight cores, and it was a blank box next to the word "any". A script
  // that knows it wants two cores per item can say so once, in itself, and
  // never again. Seeded, not forced: a person who types over it is answered
  // to, and clearing the field goes back to "whatever is free".
  let coresSeededFor = $state<string | null>(null);
  $effect(() => {
    const key = service?.name ?? null;
    if (key === coresSeededFor) return;
    coresSeededFor = key;
    if (declaredCores) cpus = declaredCores;
  });
  const sizeError = $derived(
    Number.isNaN(memMb) ? "memory" : Number.isNaN(diskMb) ? "disk" : null,
  );

  /** Which matching nodes could hold one task at all (M9 §5.3): a node
      without the RAM is not a candidate, not a bad one. */
  const fits = $derived.by(() => {
    if (!perItem) return { ok: preview.length, of: preview.length };
    const ok = preview.filter((n) => {
      const c = n.capacity;
      if (!c) return true; // unknown capacity never disqualifies
      if (cpusN && c.cores > 0 && cpusN > c.cores) return false;
      if (memMb && !Number.isNaN(memMb) && c.memMb > 0 && memMb > c.memMb) return false;
      if (diskMb && !Number.isNaN(diskMb) && c.diskMb > 0 && diskMb > c.diskMb) return false;
      return true;
    }).length;
    return { ok, of: preview.length };
  });

  const timeoutSecs = $derived.by(() => {
    try {
      return parseDuration(timeout);
    } catch {
      return undefined; // invalid — surfaced next to the field
    }
  });

  // M9 §6.4: Run stays exactly as it was, but when a one-off command looks
  // list-shaped — a `for` loop, an xargs pipeline, a `{}` placeholder — the
  // operator probably wants per-item mode. One hint, dismissible, never a
  // modal, and only where the mesh can actually do per-item work.
  let hintDismissed = $state(false);
  const listShaped = $derived(
    !hintDismissed &&
      !repeat &&
      mode === "command" &&
      app.nodes.some((n) => n.capacity !== null) &&
      /(^|\s)(for\s+\w+\s+in\s|xargs\b|parallel\b)|\{\}/.test(command),
  );

  // Secret refs are env-only (argv is world-readable via /proc).
  const secretInArgv = $derived(
    (mode === "command" && /\{\{secret(_file)?:/.test(command)) ||
      (mode === "script" && runInputs.some(([, v]) => /\{\{secret(_file)?:/.test(v))),
  );

  // The Library folded in here with the Focus surface: a shelf of named
  // scripts belongs one tab from the button that runs them, not in its own
  // nav section.
  // The page used to be one form plus a Library tab, and every mode beyond
  // "run this once" was a control hidden inside it — per-item was a
  // checkbox two thirds of the way down "what", and a gang was a text field
  // under Options that you had to already know existed.
  //
  // That is the wrong shape, because these are not settings on one idea.
  // They are four different ideas about what "run" means, and a person who
  // does not already know which one they want cannot discover the answer
  // from a checkbox. So each is a tab, each opens by saying what it is and
  // what it will not do, and the everyday case — run one thing, once —
  // stays exactly where it has always been, first and unchanged.
  //
  // The tabs *drive* the underlying state rather than sitting beside it:
  // choosing "Per item" is what sets `repeat`, and leaving it is what
  // clears it. There is no second way to reach these modes any more, which
  // is the point — two routes to one behaviour is how the old layout got
  // confusing in the first place.
  type Page = "once" | "peritem" | "together" | "farm" | "library";
  let page = $state<Page>("once");

  // -- inferring the shape (R1 P2) ---------------------------------------------
  //
  // **The shape follows from what you supplied, not from which tab you
  // found.** A list of things is a run per thing; one thing is one run.
  // D-R1.1 says so and until now the page only *offered* the switch as a
  // link in the sentence, which is a hint, not an inference.
  //
  // The two rules that keep it safe:
  //
  //   * it only ever fires *towards* per-item, and only from `once`. A
  //     person who chose Together or came back to Once deliberately is not
  //     overruled by a paste;
  //   * it is stated the moment it happens, in the sentence above the tabs,
  //     where the same click undoes it. D-R1.2 is the whole reason inference
  //     is allowed here at all — the system never acts on a conclusion it
  //     has not shown you in words you can edit.
  let shapeInferred = $state(false);
  /** Set when a person overrules the inference; cleared when the list is. */
  let shapeOverridden = $state(false);
  $effect(() => {
    if (items.length <= 1) {
      shapeOverridden = false;
      shapeInferred = false;
      return;
    }
    if (page === "once" && !shapeOverridden) {
      shapeInferred = true;
      page = "peritem";
    }
  });

  // -- the plan, as a sentence (R1 P1 + P2) ------------------------------------
  //
  // The page asks five questions in five tabs and never says what it
  // concluded, so a person who picks the wrong tab finds out from the
  // result. "Run it on all my nodes" and "run it over all my files" are the
  // same words and different jobs: the first sends one payload to every
  // machine, which for a transcode means six machines encoding the same file.
  //
  // The fix is not a sixth tab. It is to state the plan in words, from the
  // state that will actually be submitted, above everything else — and to
  // make every claim in it a control. An inference nobody can see is a guess;
  // an inference stated in a sentence you can click is a proposal.
  type Span = { text: string; go?: () => void; title?: string };

  const planSpans = $derived.by((): Span[] => {
    const what: Span = {
      text: mode === "script" ? (serviceName || "a script") : "your command",
      title: "what runs",
    };
    const target =
      spread === "all"
        ? `every matching node (${preview.length})`
        : spread === "some"
          ? `${someCount} node${someCount === 1 ? "" : "s"}`
          : selector.trim() && selector.trim() !== "any"
            ? selector.trim()
            : "any node with room";

    if (page === "peritem") {
      const n = items.length;
      const each: Span = {
        text: n === 0 ? "each item you list" : `each of ${n.toLocaleString()} item${n === 1 ? "" : "s"}`,
        go: () => (page = "peritem"),
        title: "one run per line in the list",
      };
      const spans: Span[] = [{ text: "Run " }, what, { text: " once for " }, each];
      if (shapeInferred) {
        spans.push({
          text: " (because you gave it a list)",
          go: () => {
            // Sticky: without this the effect would put the tab straight
            // back, which is an argument rather than an interface.
            shapeOverridden = true;
            shapeInferred = false;
            page = "once";
          },
          title: "run it once instead",
        });
      }
      if (itemInput) {
        spans.push({ text: ", filling in " }, { text: `--${itemInput}`, title: "which argument each item becomes" });
      }
      spans.push({ text: ", spread across " }, { text: target, go: () => (page = "peritem") });
      if (cpusN) {
        spans.push(
          { text: ", " },
          {
            text: `${cpusN} core${cpusN === 1 ? "" : "s"} each`,
            title: declaredCores && cpus === declaredCores ? "declared by the script" : "what one item needs",
          },
        );
      }
      // Where the results end up, said out loud (R3 P4).
      //
      // This is the last place a person could still send output somewhere
      // they did not mean to. "Keep each item's output in the mesh" is a
      // checkbox two disclosures down whose consequence — the results are
      // not here, and they expire — was invisible until you went looking
      // for files that were gone. A sentence that ends "…and the results
      // stay on the machines that made them" cannot be misread the same way.
      spans.push({ text: ", writing results " });
      if (declaredOut) {
        spans.push({ text: `to ${declaredOut}`, title: "the script's own outdir argument" });
      } else if (keepOutput) {
        spans.push({
          text: "to the machines that make them",
          go: () => (optionsOpen = true),
          title: "kept where they are produced — save them before they expire",
        });
      } else {
        spans.push({
          text: "back to this machine",
          go: () => (optionsOpen = true),
          title: "each item's output comes back with its answer",
        });
      }
      spans.push({ text: "." });
      return spans;
    }
    if (page === "together") {
      return [
        { text: "Run " },
        what,
        { text: " on " },
        { text: gang ? `${gang} machines at once` : "several machines at once", go: () => (page = "together") },
        { text: ", all starting together or not at all." },
      ];
    }
    if (spread === "all" || spread === "some") {
      return [
        { text: "Run " },
        what,
        { text: " once on " },
        { text: target },
        { text: " — the same job on each. " },
        {
          text: "Running it over a list instead?",
          go: () => (page = "peritem"),
          title: "one run per item, spread across the mesh",
        },
      ];
    }
    return [{ text: "Run " }, what, { text: " once, on " }, { text: target }, { text: "." }];
  });

  // **R6 P2.** Five tabs became three, and the two that left were never
  // modes at all: the Library is a *place* (now the Scripts section) and
  // the Build farm is a *setup panel* for something that runs itself. What
  // is left is the only question the page was ever asking — how many times
  // does this run, and against what — and the plan sentence above already
  // answers it in words.
  const RUN_TABS: readonly { id: Page; label: string }[] = [
    { id: "once", label: "Once" },
    { id: "peritem", label: "Per item" },
    { id: "together", label: "Together" },
  ];

  $effect(() => {
    // One direction only: the tab is the source of truth for which mode
    // this is. Nothing else writes `repeat`, and only the Together tab
    // leaves `gangSize` alone.
    if (page === "peritem") {
      repeat = true;
      gangSize = "";
    } else if (page === "together") {
      repeat = false;
      // **A gang is no longer command-shaped (R1 P4).** This used to force
      // `mode = "command"`, on the reasoning that the service path was a
      // different question and offering it here would dead-end anyone who
      // picked a script first. It was true, and the dead end was the bug:
      // a gang is requested through reserved environment keys, not through
      // a payload, so a service could always have been one — only the CLI
      // and this line said otherwise. The things people actually run as a
      // gang are published, versioned and reviewed, and a pasted shell line
      // is none of those.
      //
      // Entering this tab used to force Options open, because gang size and
      // layout lived in there and a disclosure is exactly where you would
      // never find the field the tab exists to set. C4 §8 moved that
      // question out — it is numbered step 2 on this page now — so the
      // force-open bought nothing and cost the thing a disclosure is for:
      // Together opened with time limits, priority and environment already
      // spilled down the page. Options starts closed here like everywhere
      // else, and the person who wants it opens it.
    } else if (page === "once") {
      repeat = false;
      gangSize = "";
    }
  });

  // -- applying what the script remembers (2026-09) ----------------------------
  //
  // Everything above this point is the page's own state, and the point of
  // saved defaults is that a script can arrive with opinions about most of
  // it. Applied once per script, on the way in, and then never again — the
  // moment a person touches a control it is theirs, and an effect that
  // kept re-asserting the file would be an argument rather than a
  // convenience.
  //
  // Silence is not an instruction. Every field is applied only when the
  // script actually said something about it, because a `run` block written
  // by an older build (or half-filled by hand) must leave the rest of this
  // screen exactly as it found it — never reset to a default that looks
  // deliberate and is not.
  let appliedFor = $state<string | null>(null);
  /** True for a moment after applying, so the page can say it did. */
  let appliedNote = $state<string | null>(null);

  $effect(() => {
    const key = serviceName || null;
    if (key === appliedFor) return;
    appliedFor = key;
    appliedNote = null;
    saveDefaults = false;
    saveResult = null;
    saveErr = null;
    if (!key) return;
    const r = savedRun;
    if (!r) return;

    let touched = 0;
    const put = <T,>(v: T | null, set: (v: T) => void) => {
      if (v === null) return;
      touched++;
      set(v);
    };

    // The shape first: the tab drives `repeat` and `gangSize` through the
    // effect above, so anything those own has to be set after it.
    const mode = rs(r.mode);
    if (mode === "once" || mode === "peritem" || mode === "together") {
      touched++;
      page = mode;
      // A remembered shape is a decision, so a pasted list must not quietly
      // overrule it the way it overrules an unconsidered default.
      shapeOverridden = mode !== "peritem";
    }

    // Where.
    const on = rs(r.on);
    if (on !== null) {
      touched++;
      if (on === "any") where = "any";
      else if (on.startsWith("name=")) {
        where = "node";
        nodeName = on.slice(5);
      } else if (on.startsWith("tag=")) {
        where = "group";
        groupTag = on.slice(4);
      } else {
        where = "custom";
        custom = on;
      }
    }
    const sp = rs(r.spread);
    if (sp === "one" || sp === "all" || sp === "some") {
      touched++;
      spread = sp;
    }
    put(rn(r.count), (v) => (someCount = Math.max(1, Math.round(v))));
    put(rn(r.batch), (v) => (batch = String(Math.max(1, Math.round(v)))));
    put(rs(r.halt_on_fail), (v) => (haltOnFail = v));

    // How.
    const pr = rs(r.priority);
    if (pr === "low" || pr === "normal" || pr === "high") {
      touched++;
      priority = pr;
    }
    put(rs(r.timeout), (v) => (timeout = v));
    put(rs(r.item_input), (v) => (itemInput = v));

    // Together.
    put(rn(r.gang), (v) => (gangSize = String(Math.max(2, Math.round(v)))));
    const gp = rs(r.gang_placement);
    if (gp === "pack" || gp === "spread") {
      touched++;
      gangPlacement = gp;
    }
    put(rb(r.gang_restart), (v) => (gangRestart = v));

    // Accelerators.
    put(rn(r.gpus), (v) => (gpus = String(Math.max(1, Math.round(v)))));
    put(rs(r.vram), (v) => (vram = v));
    put(rs(r.device), (v) => (deviceModel = v));
    put(rb(r.gpu_share), (v) => (gpuShare = v));

    // Per item, and where its output goes.
    put(rn(r.chunk), (v) => (chunkItems = String(Math.max(1, Math.round(v)))));
    put(rb(r.keep_output), (v) => (keepOutput = v));
    put(rs(r.reduce), (v) => (reduce = v));
    put(rs(r.reduce_on), (v) => (reduceOn = v));

    // Answers.
    put(rb(r.capture), (v) => (capture = v));
    put(rs(r.expect), (v) => (expect = v));

    // Per-run environment and any inputs the contract does not declare.
    const kv = (v: unknown): { k: string; v: string }[] =>
      v && typeof v === "object" && !Array.isArray(v)
        ? Object.entries(v as Record<string, unknown>).map(([k, val]) => ({ k, v: String(val) }))
        : [];
    const savedEnv = kv(r.env);
    if (savedEnv.length) {
      touched++;
      env = savedEnv;
    }
    const savedExtra = kv(r.inputs);
    if (savedExtra.length) {
      touched++;
      extraInputs = savedExtra;
    }

    // "What one item needs" is a declaration and lives in `x-meshware`
    // proper, not in the run block — read from there, like cores always was.
    if (typeof declared?.mem === "string") mem = declared.mem;
    if (typeof declared?.disk === "string") disk = declared.disk;

    if (touched > 0) {
      appliedNote = `${touched} setting${touched === 1 ? "" : "s"} filled in from how this script was last set up.`;
    }
  });

  // The copy each mode opens with. Kept here rather than inline in the
  // markup so the three modes can be read against each other in one place —
  // the whole value of these notes is that they draw distinctions, and a
  // distinction is hard to maintain when its two halves are 400 lines apart.
  const INTRO = {
    once: {
      title: "Run once",
      lead: "Run one thing, one time — a saved script or a command you type — on a machine you pick, or on whichever one is least busy.",
      good: [
        { text: "The everyday case", detail: "Restart a service, check a version, run a backup, look at a log." },
        { text: "Anything you'd normally SSH somewhere to do", detail: "Except you don't pick the machine by hand, and the output streams back here." },
      ],
      wont: [
        {
          text: "Make one program use more than one machine",
          detail: "Whatever you run here runs whole, in one place, using that machine's cores. Splitting work across machines is what the other tabs are for.",
        },
        {
          text: "Split a big single job into pieces for you",
          detail: "If the work is one long-running thing, this runs it as one long-running thing.",
        },
      ],
      most: [
        { text: "Leave it on \u201canywhere\u201d unless you care", detail: "The mesh picks the machine with room, which is usually a better choice than yours." },
        { text: "Use Preview before anything that changes state", detail: "It shows exactly what would run and where, without running it." },
      ],
      example: "meshnode run 'systemctl restart nginx' --on any",
    },
    peritem: {
      title: "Run per item",
      lead: "You have a list \u2014 hosts, files, image names, one per line. This runs your script once for every line, spread across the whole mesh, with each machine pulling the next item as soon as it finishes one.",
      good: [
        {
          text: "Hundreds or thousands of independent things",
          detail: "400 images to resize, 10,000 logs to grep, one check against every host.",
        },
        {
          text: "This is where \u201cuse all the cores\u201d is literally true",
          detail: "Nothing computes a split in advance \u2014 a 32-core box simply does about twice the work of a 16-core one.",
        },
        { text: "Work where one machine dying shouldn't matter", detail: "Its unfinished share is re-run by somebody else rather than waited on." },
      ],
      wont: [
        {
          text: "Split the work inside a single item",
          detail: "Each item still runs whole, on one machine. Ten thousand items across ten machines is fast; one enormous item is not.",
        },
        {
          text: "Pay off on items that finish instantly",
          detail: "Sending work across the network costs a fraction of a second. If an item takes less than that, it is cheaper to do it locally.",
        },
      ],
      most: [
        {
          text: "Aim for at least a second of real work per item",
          detail: "Group tiny items together into bigger ones if you have to.",
        },
        {
          text: "Keep big output where it lands",
          detail: "Under Options, so results stay on the machine that produced them instead of funnelling back through this one.",
        },
      ],
      example: "meshnode parallel -a hosts.txt -- ./check.sh {}",
    },
    together: {
      title: "Run together",
      lead: "Some work has to start on several machines at the same moment or not at all. This holds a reservation until enough machines are free, starts them together, and tells each one where the others are.",
      good: [
        { text: "Distributed training", detail: "torchrun and friends \u2014 each member is handed its rank and the address of the others." },
        { text: "Anything using MPI, or a shared barrier", detail: "Work whose parts talk to each other while running." },
        { text: "Benchmarks that need simultaneous load", detail: "Where staggered starts would measure the wrong thing." },
      ],
      wont: [
        {
          text: "Help work that could run one item at a time",
          detail: "If the parts don't need to talk, Per item is faster and far more forgiving. Holding machines idle while you wait for the last one is pure waste.",
        },
        {
          text: "Teach your program to use several machines",
          detail: "It has to already know how. This gets them started at the same time and introduces them; the talking is your program's job.",
        },
        {
          text: "Wait forever holding half the mesh",
          detail: "If it can't get every machine it asked for, it gives back what it took rather than sitting on it.",
        },
      ],
      most: [
        { text: "Ask for the smallest number that works", detail: "A job that needs four machines starts far sooner than one that needs twelve." },
        { text: "Pack for speed, spread for safety", detail: "Packing keeps the path between members short; spreading survives losing a machine." },
      ],
      example: "meshnode run ./train.sh --gang 8 --cpus 4 --follow",
    },
  } as const;

  // The Build farm's notes live here rather than in BuildFarm.svelte for
  // one reason: the pill they hang off is in the tab row, and the tab row
  // belongs to this file. Help that moves depending on which tab you are
  // on is help nobody finds twice (WCAG 2.2 consistent-help), so there is
  // one pill, in one place, for every mode.
  //
  // Derived rather than a literal because its last column quotes the mesh
  // you actually have — a -j suggestion is worth reading precisely when it
  // is your core count and not an example someone wrote once.
  const farmNodes = $derived(app.nodes.filter((n) => !n.revoked && !n.left));
  const farmCores = $derived(
    farmNodes.reduce((sum, n) => sum + (n.capacity?.cores ?? 0), 0),
  );

  const FARM_INTRO = $derived({
    title: "Build farm",
    lead: "Point a build tool at the mesh and its compile steps go out to whatever machines are free \u2014 and anything the mesh has already compiled comes straight back instead of being compiled again.",
    good: [
      {
        text: "Large C and C++ builds",
        detail: "One compile per source file is close to a perfect errand: small in, small out, a second or two of real work between.",
      },
      {
        text: "Anything you compile more than once",
        detail: "The second build of unchanged code is answered from the cache, from any machine in the mesh, not just the one that built it.",
      },
      { text: "Shared CI and shared laptops", detail: "One person's build warms the cache for everybody else's." },
    ],
    wont: [
      {
        text: "Speed up a build that isn't parallel already",
        detail: "The mesh routes parallelism, it doesn't create it. Without -j there is nothing to spread.",
      },
      {
        text: "Distribute linking, or configure steps",
        detail: "Those need the whole tree and run locally. That is normal \u2014 they are a small share of a big build.",
      },
      {
        text: "Break your build when something goes wrong",
        detail: "Anything it can't handle \u2014 a link step, an assembler input, no mesh, a network failure mid-flight \u2014 quietly runs the compiler locally instead.",
      },
    ],
    most: [
      {
        text: "Use a -j well above your own core count",
        detail: `You have ${farmCores || "\u2026"} cores across ${farmNodes.length} ${farmNodes.length === 1 ? "machine" : "machines"}, so -j${farmCores > 0 ? farmCores : 8} is a sensible starting point.`,
      },
      {
        text: "Keep compiler versions matching",
        detail: "A remote machine with a different gcc produces different objects. Same distro and version across the mesh is the easy path.",
      },
    ],
    example: null as string | null,
  });

  // What the tab row's pill opens, for whichever tab is showing. `null` on
  // Library — that one is a place, not a mode, and the screen says so itself.
  const notes = $derived(
    page === "farm"
      ? FARM_INTRO
      : page in INTRO
        ? INTRO[page as keyof typeof INTRO]
        : null,
  );


  // -- deep links ---------------------------------------------------------------
  $effect(() => {
    // `run:tab` used to mean "the Library tab, or nothing", because those
    // were the only two. Every mode is a tab now, so it names any of them —
    // which is also what makes `?mock=1&view=run&tab=farm` screenshot one
    // mode on its own, the way every other view already could.
    const pt = sessionStorage.getItem("run:tab");
    if (pt) {
      sessionStorage.removeItem("run:tab");
      // An old link naming a tab this page no longer has goes where the tab
      // went (R6): the Library and the Build farm are both in Scripts now.
      if (pt === "library" || pt === "farm") {
        sessionStorage.setItem("library:tab", pt === "farm" ? "farm" : "scripts");
        app.go("library");
        return;
      }
      if (RUN_TABS.some((t) => t.id === pt)) page = pt as Page;
    }
    const pre = sessionStorage.getItem("run:preselect");
    if (pre) {
      sessionStorage.removeItem("run:preselect");
      mode = "script";
      serviceName = pre;
    }
    // The palette's "Run one command per item" (Palette.svelte). It used to
    // set `repeat` directly; the tab owns that now, so setting it here would
    // be undone by the sync effect on the very next flush. Move the tab and
    // let it drive, which is the same rule the rest of this view follows.
    const t = sessionStorage.getItem("run:times");
    if (t) {
      sessionStorage.removeItem("run:times");
      page = t === "each" ? "peritem" : "once";
      mode = "command";
    }
    const m = sessionStorage.getItem("run:mode");
    if (m) {
      sessionStorage.removeItem("run:mode");
      mode = m === "command" ? "command" : "script";
    }
    // Fleet's Get started card (UX-10): "Run it on every machine".
    const sp = sessionStorage.getItem("run:spread");
    if (sp) {
      sessionStorage.removeItem("run:spread");
      if (sp === "all" || sp === "one" || sp === "some") {
        where = "any";
        spread = sp;
      }
    }
    const sel = sessionStorage.getItem("run:selector");
    if (sel) {
      sessionStorage.removeItem("run:selector");
      // Land on the friendly picker when the selector is one it can express
      // (Nodes → "Run here" sends name=…), otherwise show it as typed.
      // One name only: `name=a|name=b` from Fleet's "Run something on
      // them" (UX-8) is several machines, which the node picker cannot
      // hold, so it lands as the selector it is.
      if (sel.startsWith("name=") && !/[|,]/.test(sel)) {
        where = "node";
        nodeName = sel.slice(5);
      } else if (sel.startsWith("tag=")) {
        where = "group";
        groupTag = sel.slice(4);
      } else if (sel === "any" || sel === "") {
        where = "any";
      } else {
        where = "custom";
        custom = sel;
      }
    }
  });

  // -- running ------------------------------------------------------------------
  type Cell = {
    name: string;
    jobId: string;
    /** What the pane shows: the ring's text as of the last frame drawn. */
    lines: string;
    /** Lines the ring let go of, for the pane to say so. */
    dropped: number;
    state: string | null;
    code: number | null;
    startedAt: number;
    endedAt: number | null;
  };
  let phase = $state<"idle" | "running" | "done">("idle");
  let runErr = $state<string | null>(null);
  // A dry run dispatches nothing, so it gets a surface of its own: a run
  // that silently does nothing is worse than one that fails.
  let jobPreview = $state<JobPreviewView | null>(null);
  let single = $state<Cell | null>(null);
  let grid = $state<Cell[]>([]);

  // -- live output, bounded (DIR-9) --------------------------------------------
  //
  // Every chunk was appended to one string per pane — `lines + ev.text` —
  // and the whole block re-rendered, so a long, chatty command froze the
  // window while its memory grew, and a fan-out did it once per machine.
  // Each pane now keeps the last lines in a ring (`lib/lines.ts`, the one
  // Activity uses), and a burst of chunks is drawn once per frame rather
  // than once per chunk. The whole log stays in Activity, a click away.
  let singleRing: LineRing | null = null;
  let gridRings: LineRing[] = [];
  let singleDirty = false;
  const gridDirty = new Set<number>();
  let frame = 0;

  function scheduleDraw() {
    if (frame === 0) frame = requestAnimationFrame(draw);
  }

  /** Copy every ring with new output into its pane. */
  function draw() {
    if (frame !== 0) cancelAnimationFrame(frame);
    frame = 0;
    if (singleDirty && single && singleRing) {
      single = { ...single, lines: singleRing.text, dropped: singleRing.dropped };
    }
    singleDirty = false;
    for (const i of gridDirty) {
      const cell = grid[i];
      const ring = gridRings[i];
      if (cell && ring) grid[i] = { ...cell, lines: ring.text, dropped: ring.dropped };
    }
    gridDirty.clear();
  }

  function resetOutput() {
    if (frame !== 0) cancelAnimationFrame(frame);
    frame = 0;
    singleRing = null;
    gridRings = [];
    singleDirty = false;
    gridDirty.clear();
  }
  onDestroy(resetOutput);

  function newCells(targets: { nodeName: string; jobId: string }[]): Cell[] {
    gridRings = targets.map(() => new LineRing());
    gridDirty.clear();
    return targets.map((t) => ({ name: t.nodeName, jobId: t.jobId, lines: "", dropped: 0, state: null, code: null, startedAt: Date.now(), endedAt: null }));
  }
  let summary = $state<{ ok: number; failed: number; skipped: number } | null>(null);
  let expanded = $state<number | null>(null);

  // -- answers (M19 §5.4) -----------------------------------------------------
  //
  // Off by default, and that is the design: capturing outputs means the
  // origin reads each target's log back, so a run nobody asked a question
  // of costs exactly what it cost before M19.
  let capture = $state(false);
  let expect = $state("");
  let rows = $state<ResultRowView[]>([]);
  let expected = $state<{ expression: string; passed: number; failed: string[] } | null>(null);
  /** The answers dialog. Opens on its own once the run has finished with
   *  rows in hand — the moment somebody wants to read them — and stays
   *  closed for a run that is still streaming, so the grid can be watched. */
  let answersOpen = $state(false);
  $effect(() => {
    if (phase === "done" && rows.length > 0) answersOpen = true;
  });
  const answersFailed = $derived(rows.filter((r) => r.exitCode !== 0 || r.expectPassed === false).length);
  const expectError = $derived.by((): string | null => {
    const e = expect.trim();
    if (!e) return null;
    if (!/^[\w.]+\s*\(.*\)$/.test(e))
      return 'An expectation is a call: contains(stdout, "…"), equals(exit_code, "0"), matches(stdout, "ok*").';
    return null;
  });

  function onEvent(ev: RunEvent) {
    if (ev.type === "started") {
      singleRing = new LineRing();
      singleDirty = false;
      single = { name: ev.executorName, jobId: ev.jobId, lines: "", dropped: 0, state: null, code: null, startedAt: Date.now(), endedAt: null };
    } else if (ev.type === "fanoutStarted") {
      grid = newCells(ev.targets);
    } else if (ev.type === "chunk") {
      if (ev.index === null) {
        if (!singleRing) return;
        singleRing.push(ev.text);
        singleDirty = true;
      } else {
        const ring = gridRings[ev.index];
        if (!ring) return;
        ring.push(ev.text);
        gridDirty.add(ev.index);
      }
      scheduleDraw();
    } else if (ev.type === "done") {
      // The last of the output first, so a finished pane is never a frame
      // behind its own ending.
      draw();
      if (ev.index === null) {
        if (single) single = { ...single, state: ev.state, code: ev.exitCode, endedAt: Date.now() };
        phase = "done";
      } else {
        const cell = grid[ev.index];
        if (cell) grid[ev.index] = { ...cell, state: ev.state, code: ev.exitCode, endedAt: Date.now() };
      }
    } else if (ev.type === "fanoutDone") {
      summary = { ok: ev.succeeded, failed: ev.failed, skipped: ev.skipped };
      phase = "done";
      // -- C4 gangs ---------------------------------------------------------
      //
      // Waiting is the *normal* state of a gang submission, so it gets a
      // frame of its own: a screen that showed nothing while the offer
      // rounds ran would look hung exactly when the scheduler was working.
    } else if (ev.type === "gangWaiting") {
      gangWait = { held: ev.held, size: ev.size, round: ev.round, behind: ev.behind };
    } else if (ev.type === "gangStarted") {
      gangWait = null;
      grid = newCells(ev.members);
    } else if (ev.type === "gangDone") {
      gangWait = null;
      summary = { ok: ev.succeeded, failed: ev.failed, skipped: 0 };
      phase = "done";
    } else if (ev.type === "result") {
      rows = [...rows, ev.row];
    } else if (ev.type === "expected") {
      expected = { expression: ev.expression, passed: ev.passed, failed: ev.failed };
    } else if (ev.type === "preview") {
      jobPreview = ev.preview;
      phase = "done";
    } else if (ev.type === "failed") {
      runErr = ev.message;
      phase = "done";
    }
  }

  const blocker = $derived.by((): string | null => {
    if (phase === "running") return null;
    if (mode === "command" && !command.trim()) return "Type a command first.";
    if (mode === "script" && !serviceName) return "Pick a script first.";
    // A required input that each item will fill is not missing.
    const stillMissing = perItem ? missing.filter((m) => m !== itemInput) : missing;
    if (mode === "script" && stillMissing.length)
      return `Fill in ${stillMissing.map((m) => `“${m}”`).join(", ")} — ${stillMissing.length === 1 ? "it is" : "they are"} required.`;
    if (perItem && items.length === 0) return "Add the list of items to run for.";
    if (where === "node" && !nodeName) return "Choose which node.";
    if (where === "group" && !groupTag) return "Choose which group.";
    if (expectError) return expectError;
    if (previewErr) return previewErr;
    if (!previewKnown) return "Checking which nodes match…";
    if (preview.length === 0) return "Nothing matches — no node is available to run this.";
    if (secretInArgv)
      return "Secrets can only go in environment variables — arguments are visible to everyone on the machine.";
    if (timeoutSecs === undefined) return `“${timeout}” is not a duration — try 30s, 5m or 2h.`;
    if (perItem && sizeError) return `“${sizeError === "memory" ? mem : disk}” is not a size — try 512M or 48G.`;
    if (perItem && fits.ok === 0) return "No matching node is big enough for one item.";
    return null;
  });
  const canRun = $derived(phase !== "running" && blocker === null);

  // -- remembering this setup (2026-09) ---------------------------------------
  //
  // The checkbox beside Run. Ticked, a successful run writes everything on
  // this page back to the script: the arguments' defaults, and the two
  // dozen settings that are not arguments — shape, target, spread, limits,
  // gang, accelerators, chunking, where output lives, what is collected.
  //
  // Only on success, and only for a script. A command typed into the box
  // has nothing to write to, and remembering the settings of a run that
  // was refused would be remembering a mistake.
  let saveDefaults = $state(false);
  let saveBusy = $state(false);
  let saveResult = $state<string | null>(null);
  let saveErr = $state<string | null>(null);

  /** Publishing a contract is the CONTENT gate: admin or developer. */
  const canSaveDefaults = $derived(
    app.isAdmin || (app.whoami?.roles.includes("developer") ?? false),
  );

  /** Where this script's bundle was pushed from, when Director knows —
   *  the only route to the `.definitions` file on disk. */
  const scriptOrigin = $derived.by((): string | null => {
    if (!service) return null;
    const all = originsOf(app.sources, app.bundles);
    return all.find((o) => o.bundleId === service.bundleId)?.localPath ?? null;
  });

  /** Everything on this page that is not an argument, as the `run` block. */
  function runDefaultsPayload(): Record<string, unknown> {
    const num = (raw: string): number | undefined => {
      const n = Number(raw.trim());
      return raw.trim() && Number.isFinite(n) ? Math.round(n) : undefined;
    };
    const str = (raw: string): string | undefined => raw.trim() || undefined;
    const pairs = (rows: { k: string; v: string }[]): Record<string, string> | undefined => {
      const out: Record<string, string> = {};
      for (const r of rows) if (r.k.trim()) out[r.k.trim()] = r.v;
      return Object.keys(out).length ? out : undefined;
    };
    // Written from the state that would actually be submitted, not from
    // the controls — a gang size sitting in a field on the Once tab is not
    // part of this run and must not be remembered as though it were.
    const body: Record<string, unknown> = {
      mode: page,
      on: selector.trim() || "any",
      spread: perItem || gang ? undefined : spread,
      count: spread === "some" && !perItem && !gang ? Math.max(1, someCount) : undefined,
      batch: spreading ? num(batch) : undefined,
      halt_on_fail: spreading ? str(haltOnFail) : undefined,
      priority: perItem ? undefined : priority,
      timeout: str(timeout),
      item_input: perItem ? str(itemInput) : undefined,
      gang: gang ?? undefined,
      gang_placement: gang ? gangPlacement : undefined,
      gang_restart: gang ? gangRestart : undefined,
      gpus: gpusN ?? undefined,
      ports: ports.length > 0 ? ports : undefined,
      register: registerAs.trim() || undefined,
      volumes: volumes.length > 0 ? volumes : undefined,
      vram: str(vram),
      device: str(deviceModel),
      gpu_share: gpusN !== null || deviceModel.trim() ? gpuShare : undefined,
      chunk: perItem ? num(chunkItems) : undefined,
      cores: perItem ? (cpusN ?? undefined) : undefined,
      mem: perItem ? str(mem) : undefined,
      disk: perItem ? str(disk) : undefined,
      keep_output: perItem ? keepOutput : undefined,
      reduce: perItem && keepOutput ? str(reduce) : undefined,
      reduce_on: perItem && keepOutput ? str(reduceOn) : undefined,
      capture: perItem ? undefined : capture,
      expect: perItem ? undefined : str(expect),
      env: mode === "command" ? pairs(env) : undefined,
      inputs: pairs(extraInputs),
    };
    // `undefined` would serialise as null and read back as "remembered, as
    // nothing" — which is not the same as "not remembered".
    for (const k of Object.keys(body)) if (body[k] === undefined) delete body[k];
    return body;
  }

  /** Armed by a real (non-preview) dispatch of a script, disarmed once
   *  the save has been attempted — so one run writes at most one version. */
  let saveArmed = $state(false);

  $effect(() => {
    if (phase !== "done" || !saveArmed) return;
    // A preview dispatched nothing, and a failed dispatch is not a setup
    // worth remembering. The exit code of
    // what *did* run is deliberately not consulted: the script failing is
    // the script's business, and it says nothing about whether it was
    // pointed at the right machines with the right limits.
    if (jobPreview || runErr) return;
    saveArmed = false;
    void saveRunDefaults();
  });

  async function saveRunDefaults() {
    if (!service || saveBusy) return;
    saveBusy = true;
    saveErr = null;
    saveResult = null;
    try {
      // The declared inputs become the arguments' own defaults. An
      // argument's default is an argument's business, and putting them in
      // the run block would give one value two homes.
      //
      // `String(…)`, and not a cast: `bind:value` on an `<input
      // type="number">` hands back a **number**, and on a checkbox a
      // boolean, whatever `inputs` is declared to hold. Sending one to a
      // command whose signature is `Vec<(String, String)>` fails
      // deserialization before the command body ever runs, and the error a
      // person sees is about JSON rather than about their script.
      const argDefaults = fields.map(
        (f) => [f.name, inputs[f.name] == null ? "" : String(inputs[f.name])] as [string, string],
      );
      // The folder on disk, when there is one, is the backend's to know
      // (SEC-14): it wrote down where this script's bundle came from.
      const saved = await api.serviceSaveRunDefaults(service.name, runDefaultsPayload(), argDefaults);
      const parts: string[] = [];
      parts.push(
        saved.version === null
          ? "already how this script is set up"
          : `saved to ${service.name} v${saved.version}`,
      );
      if (saved.definitionsPath) parts.push(`and to ${saved.definitionsPath}`);
      saveResult = parts.join(" ");
      // Not fatal, and not hidden either: the contract is saved by now,
      // and the file is the copy somebody commits.
      if (saved.definitionsError) saveErr = `the file on disk was not updated — ${saved.definitionsError}`;
      await app.refresh();
    } catch (e) {
      saveErr = errText(e);
    } finally {
      saveBusy = false;
    }
  }

  async function run(opts: { items?: string[]; dryRun?: boolean } = {}) {
    saveArmed = saveDefaults && mode === "script" && !!service && !opts.dryRun;
    saveResult = null;
    saveErr = null;
    runErr = null;
    jobPreview = null;
    resetOutput();
    single = null;
    grid = [];
    summary = null;
    rows = [];
    expected = null;
    expanded = null;
    gangWait = null;
    phase = "running";
    if (perItem) {
      await runPerItem(opts.items ?? items);
      return;
    }
    const dispatch = {
      on: selector && selector !== "any" ? selector : null,
      fanout,
      timeoutSecs: timeoutSecs ?? null,
      priority,
      maxParallel: spreading && batch.trim() ? Math.max(1, Number(batch)) : null,
      haltOnFail: spreading && haltOnFail.trim() ? haltOnFail.trim() : null,
      dryRun: opts.dryRun ?? false,
    };
    try {
      if (mode === "command") {
        const kv = env.filter((r) => r.k.trim()).map((r) => [r.k.trim(), r.v] as [string, string]);
        await api.runCommand(
          {
            command,
            env: kv,
            ...dispatch,
            // M19 §5.2: an expectation implies capture — asserting over
            // rows that were never captured would assert over nothing, and
            // silently passing is the one thing a guard must never do.
            captureOutputs: capture || expect.trim().length > 0,
            outputs: null,
            expect: expect.trim() || null,
            gang,
            gangPlacement: gang ? gangPlacement : null,
            gangRestart: gang ? gangRestart : false,
            gpus: gpusN,
            ports: ports.length > 0 ? ports : null,
            register: registerAs.trim() || null,
            volumes: volumes.length > 0 ? volumes : null,
            vramMb: Number.isNaN(vramMb) ? null : vramMb,
            device: deviceModel.trim() ? `model=~${deviceModel.trim()}` : null,
            gpuShare: gpuShare && (gpusN !== null || !!deviceModel.trim()),
          },
          onEvent,
        );
      } else {
        await api.runService(
          {
            name: serviceName,
            inputs: runInputs,
            ...dispatch,
            // A script is the case that needs this most — it is the thing
            // people write *in order to* produce an answer — and until
            // now it was the one run shape that could not ask for one.
            captureOutputs: capture || expect.trim().length > 0,
            outputs: null,
            expect: expect.trim() || null,
            // A published, versioned script is exactly the shape gang work
            // takes — training, a coordinated burn-in, anything that has to
            // start together. It could not ask to be one until R1 P4.
            gang,
            gangPlacement: gang ? gangPlacement : null,
            gangRestart: gang ? gangRestart : false,
            gpus: gpusN,
            ports: ports.length > 0 ? ports : null,
            register: registerAs.trim() || null,
            volumes: volumes.length > 0 ? volumes : null,
            vramMb: Number.isNaN(vramMb) ? null : vramMb,
            device: deviceModel.trim() ? `model=~${deviceModel.trim()}` : null,
            gpuShare: gpuShare && (gpusN !== null || !!deviceModel.trim()),
          },
          onEvent,
        );
      }
    } catch (e) {
      runErr = errText(e);
      phase = "done";
    }
  }

  // -- per-item runs -------------------------------------------------------------
  let par = $state({
    jobId: "",
    total: 0,
    done: 0,
    failed: 0,
    inflight: 0,
    chunkItems: 0,
    speculated: 0,
    elapsedMicros: 0,
    perNode: [] as NodeProgressView[],
    results: [] as ItemResult[],
    reduce: null as ReduceResult | null,
    workers: [] as string[],
    declined: [] as string[],
    estimate: null as DurationRangeView | null,
  });
  let parStartedAt = 0;

  function onParallel(ev: ParallelEvent) {
    if (ev.type === "started") {
      par = {
        ...par,
        jobId: ev.jobId,
        total: Number(ev.total),
        workers: ev.workers,
        declined: ev.declined,
        estimate: ev.estimate ?? null,
      };
    } else if (ev.type === "item") {
      par.results = [
        ...par.results,
        {
          item: ev.item,
          exitCode: ev.exitCode,
          stdout: ev.stdout,
          stderr: ev.stderr,
          node: ev.nodeName,
          output: ev.output ? { ...ev.output, size: Number(ev.output.size) } : null,
        },
      ];
    } else if (ev.type === "progress") {
      par = {
        ...par,
        total: Number(ev.total),
        done: Number(ev.done),
        failed: Number(ev.failed),
        inflight: Number(ev.inflight),
        chunkItems: ev.chunkItems,
        speculated: ev.speculated,
        perNode: ev.perNode,
        elapsedMicros: (Date.now() - parStartedAt) * 1000,
      };
    } else if (ev.type === "reduced") {
      par = {
        ...par,
        reduce: {
          nodeName: ev.nodeName,
          exitCode: ev.exitCode,
          stdout: ev.stdout,
          stderr: ev.stderr,
          inputs: Number(ev.inputs),
        },
      };
    } else if (ev.type === "done") {
      par = {
        ...par,
        total: Number(ev.total),
        done: Number(ev.total),
        failed: Number(ev.failed),
        inflight: 0,
        speculated: ev.speculated,
        perNode: ev.perNode,
        elapsedMicros: Number(ev.micros),
      };
      phase = "done";
      app.flash(ev.failed > 0 ? `${ev.failed} of ${ev.total} items failed` : `${ev.total} items done`);
    } else if (ev.type === "preview") {
      jobPreview = ev.preview;
      phase = "done";
    } else if (ev.type === "failed") {
      runErr = ev.message;
      phase = "done";
    }
  }

  async function runPerItem(list: string[]) {
    par = {
      jobId: "",
      total: list.length,
      done: 0,
      failed: 0,
      inflight: 0,
      chunkItems: 0,
      speculated: 0,
      elapsedMicros: 0,
      perNode: [],
      results: [],
      reduce: null,
      workers: [],
      declined: [],
      estimate: null,
    };
    parStartedAt = Date.now();
    try {
      await api.parallelRun(
        {
          command: mode === "command" ? command : "",
          service: mode === "script" ? serviceName : null,
          // The item's own input is filled per item on the executor, so it
          // is not sent as a fixed value here.
          inputs: mode === "script" ? runInputs.filter(([k]) => k !== itemInput) : [],
          itemInput: mode === "script" && itemInput ? itemInput : null,
          items: list,
          on: selector && selector !== "any" ? selector : null,
          env: mode === "command" ? env.filter((r) => r.k.trim()).map((r) => [r.k.trim(), r.v] as [string, string]) : [],
          timeoutSecs: timeoutSecs ?? null,
          cpus: cpusN,
          memMb: Number.isNaN(memMb) ? null : memMb,
          diskMb: Number.isNaN(diskMb) ? null : diskMb,
          chunk: chunkItems.trim() ? Math.max(1, Number(chunkItems)) : null,
          keepOutput,
          reduce: keepOutput && reduce.trim() ? reduce.trim() : null,
          reduceOn: keepOutput && reduceOn.trim() ? reduceOn.trim() : null,
        },
        onParallel,
      );
    } catch (e) {
      runErr = errText(e);
      phase = "done";
    }
  }

  /** Re-run only the items that failed, as a fresh job (M9 §6.3). */
  function retryFailures() {
    const again = par.results.filter((r) => r.exitCode !== 0).map((r) => r.item);
    if (again.length === 0) return;
    itemText = again.join("\n");
    void run({ items: again });
  }

  function openInJobs(jobId: string) {
    sessionStorage.setItem("jobs:select", jobId);
    app.go("jobs");
  }

  function tail(text: string, n = 9): string {
    const lines = text.split("\n");
    return lines.slice(Math.max(0, lines.length - n)).join("\n");
  }

  function dur(c: Cell): string {
    return `${(((c.endedAt ?? Date.now()) - c.startedAt) / 1000).toFixed(1)}s`;
  }

  const secretNames = $derived(app.secrets.filter((s) => !s.removed).map((s) => s.name));

  /** The plain-English sentence above the Run button. */
  const targetSentence = $derived.by(() => {
    if (!previewKnown) return "checking which nodes match";
    if (preview.length === 0) return "no node matches";
    const names = preview.slice(0, 3).map((n) => n.name);
    if (!spreading)
      return preview.length === 1 ? `on ${names[0]}` : `on whichever of ${preview.length} nodes is free`;
    if (spread === "some") return `on ${hitCount} of the ${preview.length} matching nodes`;
    if (preview.length <= 3) return `on ${names.join(", ")}`;
    return `on all ${preview.length} matching nodes`;
  });
</script>

{#snippet gangQuestion()}
  <!-- C4 §8. This lived under Options, which was defensible when a gang was
       a rare modifier on an ordinary run. It is a *tab* now, and the field
       that decides how many machines start together cannot be the fourth
       thing inside a disclosure on the screen whose whole subject it is —
       so on Together it is a numbered question like any other, and it is
       not rendered anywhere else. -->
  <div class="mt-3 rounded-lg border border-edge-card bg-canvas p-4">
    <div class="flex flex-wrap items-end gap-3">
      <label class="block w-36">
        <span class={ui.label}>Machines at once</span>
        <input
          bind:value={gangSize}
          placeholder="2 or more"
          class="field field-sunken num h-10 w-full px-3 text-sm outline-none"
        />
      </label>
      {#if gang}
        <label class="block w-52">
          <span class={ui.label}>Placement</span>
          <select bind:value={gangPlacement} class="field field-sunken select h-10 w-full px-3 text-sm outline-none">
            <option value="pack">close together — fewest machines</option>
            <option value="spread">spread out — one per machine</option>
          </select>
        </label>
        <label class="flex h-10 cursor-pointer items-center gap-2 text-sm text-ink-3">
          <input type="checkbox" class="check" bind:checked={gangRestart} />
          Restart if one of them fails
        </label>
      {/if}
    </div>
    <p class="mt-2.5 max-w-prose text-xs leading-relaxed text-ink-5">
      {#if gang}
        All {gang} start at the same moment or none of them does.
        {gangPlacement === "pack" ? "Close together keeps the path between them short." : "Spreading out survives losing a machine, at the cost of that."}
        {gangRestart ? "" : " One of them failing ends the whole job — a job waiting on a part that will never arrive is not worth the GPUs."}
      {:else}
        Two or more machines, held together — for a training run, or anything whose
        parts must all start at once.
      {/if}
    </p>
  </div>
{/snippet}

{#snippet heading(n: string, text: string, hint?: string)}
  <!-- The three questions are the page's whole structure, so they get real
       weight: a washed accent chip and a card-title size, not a 20px dot
       beside body text that vanished into the white. -->
  <div class="flex items-center gap-3">
    <span
      class="num grid h-[26px] w-[26px] flex-none place-items-center rounded-full text-sm font-bold"
      style="background: var(--hue-wash); color: var(--hue-ink); box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--hue) 24%, transparent);">{n}</span
    >
    <span class="text-md font-bold tracking-[-.015em] text-ink">{text}</span>
    {#if hint}<span class="text-sm text-ink-5">{hint}</span>{/if}
  </div>
{/snippet}

<!-- The plan, in words, before anything else on the page (R1 P1).
     It is rendered from the state that will actually be submitted, so it
     cannot describe a run other than the one the button starts. Every claim
     that has somewhere to go is a button; the rest is text. -->
{#if page !== "library" && page !== "farm"}
  <div
    class="mb-3 rounded-md px-4 py-3 text-base leading-relaxed"
    style="background: var(--accent-wash-2); border: 1px solid var(--edge);"
    data-testid="run-plan"
  >
    {#each planSpans as span, i (i)}
      {#if span.go}
        <button
          class="cursor-pointer font-semibold underline decoration-dotted underline-offset-4"
          style="color: var(--accent-ink);"
          title={span.title}
          onclick={span.go}
        >{span.text}</button>
      {:else if span.title}
        <span class="font-semibold text-ink" title={span.title}>{span.text}</span>
      {:else}
        <span class="text-ink-2">{span.text}</span>
      {/if}
    {/each}
  </div>
{/if}

<!-- The tab row carries the help for whichever mode is showing: one pill,
     one place, every tab. It sits beside the tablist rather than inside it
     because a tablist may only contain tabs, and the rule ("what is this?")
     is not one. -->
<div class="mb-4 flex items-center gap-4" style="border-bottom: 1px solid var(--edge);">
  <div class="tabrow" role="tablist" aria-label="Run" use:tabline>
    {#each RUN_TABS as t (t.id)}
      <button class="tab" role="tab" aria-selected={page === t.id} onclick={() => (page = t.id)}>
        {t.label}
      </button>
    {/each}
  </div>
  {#if notes}
    <div class="ml-auto">
      <RunModeIntro
        id={page}
        title={notes.title}
        lead={notes.lead}
        good={[...notes.good]}
        wont={[...notes.wont]}
        most={[...notes.most]}
        example={notes.example}
      />
    </div>
  {/if}
</div>

{#if page === "library"}
  <LibraryView />
{:else if page === "farm"}
  <BuildFarm />
{:else}
<div class="fade-in space-y-4">
  <div class="card p-[22px]">
    <!-- ===================== 1. what ===================== -->
    {@render heading("1", "What do you want to run?")}

    <div class="mt-2.5 inline-flex rounded-md bg-surface-2 p-[3px]">
      {#each [["script", "A saved script"], ["command", "A one-off command"]] as [id, label] (id)}
        <button
          class={`rounded-sm px-[13px] py-[6px] text-sm transition-colors ${mode === id ? "bg-surface font-semibold text-ink" : "font-medium text-ink-5 hover:text-ink"}`}
          style={mode === id ? "box-shadow: var(--sh-control);" : ""}
          onclick={() => (mode = id as Mode)}
        >
          {label}
        </button>
      {/each}
    </div>

    {#if mode === "command"}
      <div class="on-code mt-3 flex overflow-hidden rounded-md bg-code [box-shadow:var(--sh-control)] focus-within:[box-shadow:var(--shadow-focus)]">
        <div class="mono flex-none pt-2.5 pr-1 pl-3.5 text-base select-none" style="color: var(--ink-code-4);">$</div>
        <textarea
          bind:value={command}
          rows="3"
          spellcheck="false"
          placeholder="echo hello from $(hostname)"
          class="mono min-h-20 w-full resize-y bg-transparent py-2.5 pr-3.5 pl-1 text-base leading-normal outline-none focus-visible:shadow-none"
          style="color: var(--ink-code-1); caret-color: var(--accent-bright);"
        ></textarea>
      </div>
      <div class="mt-1.5 text-xs text-ink-5">Runs through bash on the node, with a clean environment.</div>
      {#if listShaped}
        <!-- One unobtrusive, dismissible hint (M9 §6.4). It flips the toggle
             in place rather than sending anyone anywhere: the command they
             already typed stays exactly as typed. -->
        <div class="mt-2 flex items-center gap-2 rounded-sm px-3 py-2 text-xs" style="background: var(--accent-wash); color: var(--accent-ink);">
          <Icon name="compute" size={14} />
          <span>Looks like one run per item. The mesh can spread those across every node.</span>
          <button class="ml-auto font-semibold underline-offset-2 hover:underline" onclick={() => (repeat = true)}>Run once per item</button>
          <button class="opacity-60 hover:opacity-100" aria-label="dismiss" onclick={() => (hintDismissed = true)}><Icon name="close" size={12} /></button>
        </div>
      {/if}
    {:else if app.services.length === 0}
      <div class="mt-3 rounded-lg border border-dashed border-edge-dashed px-4 py-3.5 text-sm text-ink-5">
        No scripts yet.
        <button class="font-semibold" style="color: var(--accent-ink);" onclick={() => app.go("library")}>Add one in the Library</button>
        — or switch to a one-off command.
      </div>
    {:else}
      {#if app.services.length > 8}
        <input
          bind:value={scriptFilter}
          aria-label="Filter scripts"
          placeholder="filter scripts…"
          class="mt-3 h-9 w-64 field field-sunken px-3 text-sm text-ink outline-none"
        />
      {/if}
      <div class="mt-3 flex flex-wrap gap-2">
        {#each shownServices as s (s.name)}
          {@const m = runtimeMeta(s.runtime)}
          <button
            class="tile flex cursor-pointer items-center gap-2.5 px-3 py-2 text-left"
            style={serviceName === s.name
              ? "border-color: var(--hue); background: var(--hue-wash);"
              : ""}
            onclick={() => (serviceName = serviceName === s.name ? "" : s.name)}
          >
            <span
              class="mono grid h-7 w-7 flex-none place-items-center rounded-sm text-2xs font-bold"
              style={`background:${m.bg};color:${m.fg};`}
              title={m.label}>{m.short}</span
            >
            <span class="min-w-0">
              <span class="block truncate text-sm font-semibold" style={serviceName === s.name ? "color: var(--hue-ink);" : "color: var(--ink-2);"}>{s.name}</span>
              <span class="block max-w-[220px] truncate text-xs text-ink-4">{s.description || m.label}</span>
            </span>
          </button>
        {/each}
      </div>

      {#if service}
        <!-- what this script will actually do -->
        <div class="on-code mt-3 overflow-hidden rounded-md bg-code px-4 py-2.5">
          <div class="mono selectable text-sm leading-relaxed break-all" style="color: var(--ink-code-1);">
            <span style="color: var(--accent-bright);">{runtimeProgram(service.runtime, service.entrypoint)}</span>
            {#if runInputs.length}<span> {argvOf(runInputs)}</span>{/if}
          </div>
          <!-- R6 P4: "bundle" is the content-addressed store's word for
               where a script's files live. A person choosing what to run
               needs to know which *version* they are about to run; the
               storage id is an advanced detail and reads as noise here. -->
          <div class="mt-1 text-xs" style="color: var(--ink-code-4);" title={service.bundleShort}>version {service.version}</div>
        </div>

        {#if fields.length > 0}
          <div class="mt-3 grid grid-cols-2 gap-x-5 gap-y-4 rounded-lg border border-edge-card bg-canvas p-4">
            {#each fields as f (f.name)}
              <label class="block">
                <span class="mb-1.5 block text-sm text-ink-4">
                  <span class="mono font-semibold text-ink-3">{f.name}</span>
                  {#if f.required}<span style="color: var(--status-error);">*</span>{/if}
                  {#if f.description}<span class="text-ink-5"> — {f.description}</span>{/if}
                </span>
                {#if f.options}
                  <select bind:value={inputs[f.name]} class="field field-sunken select h-10 w-full px-3 text-sm text-ink outline-none">
                    <option value="">choose…</option>
                    {#each f.options as o}<option value={o}>{o}</option>{/each}
                  </select>
                {:else if f.type === "boolean"}
                  <select bind:value={inputs[f.name]} class="field field-sunken select h-10 w-full px-3 text-sm text-ink outline-none">
                    <option value="">choose…</option>
                    <option value="true">yes</option>
                    <option value="false">no</option>
                  </select>
                {:else}
                  <input
                    bind:value={inputs[f.name]}
                    type={f.type === "number" || f.type === "integer" ? "number" : "text"}
                    min={f.min ?? undefined}
                    max={f.max ?? undefined}
                    class="field field-sunken h-10 w-full px-3 text-sm text-ink outline-none"
                  />
                  {#if f.pattern}
                    <span class="mono mt-1.5 block text-2xs text-ink-5">must match {f.pattern}</span>
                  {:else if f.min !== null || f.max !== null}
                    <span class="mt-1.5 block text-2xs text-ink-5">
                      {f.min !== null && f.max !== null ? `between ${f.min} and ${f.max}` : f.min !== null ? `${f.min} or more` : `${f.max} or fewer`}
                    </span>
                  {/if}
                {/if}
              </label>
            {/each}
          </div>
        {:else}
          <div class="mt-2.5 flex items-center gap-2 text-xs text-ink-5">
            <span>This script takes no declared arguments.</span>
            <button class="font-semibold" style="color: var(--accent-ink);" onclick={() => (extraInputs = [...extraInputs, { k: "", v: "" }])}>
              Pass one anyway
            </button>
          </div>
          {#if extraInputs.length}
            <div class="mt-2 space-y-2">
              {#each extraInputs as row, i (i)}
                <div class="flex gap-2">
                  <input bind:value={row.k} aria-label="Variable name" placeholder="name" class="field field-sunken mono h-9 w-40 px-3 text-sm outline-none" />
                  <input bind:value={row.v} aria-label={`Value of ${row.k || "the variable"}`} placeholder="value" class="field field-sunken mono h-9 flex-1 px-3 text-sm outline-none" />
                  <button class="px-1 text-base text-ink-5 hover:text-error" title="remove" onclick={() => (extraInputs = extraInputs.filter((_, j) => j !== i))}>✕</button>
                </div>
              {/each}
            </div>
          {/if}
        {/if}
      {/if}
    {/if}

    <!-- The item list. This was a checkbox here plus a panel under it; the
         checkbox *is* the "Per item" tab now, so all that survives is the
         one field the mode actually adds. -->
    <!-- R1 P2: the list is on Once as well, because the shape is supposed
         to follow from what you supplied. With the field only on Per item
         there was nothing to infer *from*: you had to already know which
         tab you wanted, which is the thing the sentence exists to stop. -->
    {#if page === "peritem" || page === "once"}
      {#if !meshDoesCompute && page === "peritem"}
        <div class="mt-4 rounded-lg border border-edge-card bg-canvas p-3.5 text-sm leading-[1.55] text-ink-2">
          No node in this mesh says how much hardware it has yet, so there is nothing to
          spread a list across. Per-item runs need at least one node new enough to advertise
          its cores; upgrade a node and this mode starts working.
        </div>
      {:else if page === "once" && !listOpen && items.length === 0}
        <div class="mt-4">
          <button class="linkish text-sm" onclick={() => (listOpen = true)}>Running it over a list instead?</button>
        </div>
      {:else}
        <div class="fade-in mt-4 rounded-lg border border-edge-card bg-canvas p-4">
          <!-- The single field that per-item mode adds. Everything else it
               needs has a default worth keeping. -->
          <span class={ui.label}>
            {page === "once" ? "Running it over a list? One per line" : "One item per line"}
            {#if items.length > 0}<span class="num text-ink-6"> — {items.length.toLocaleString()} item{items.length === 1 ? "" : "s"}</span>{/if}
          </span>
          <textarea
            bind:value={itemText}
            rows="5"
            spellcheck="false"
            placeholder={"web-01\nweb-02\nweb-03"}
            class="mono min-h-24 w-full resize-y field field-sunken p-3 text-sm leading-normal text-ink outline-none"
          ></textarea>

          {#if mode === "script" && fields.length > 0}
            <div class="mt-2.5 flex flex-wrap items-center gap-2.5">
              <span class="text-sm text-ink-4">Each item becomes</span>
              <select bind:value={itemInput} aria-label="Each item becomes" class="select h-9 field field-sunken px-2.5 text-sm text-ink outline-none">
                {#each fields as f (f.name)}<option value={f.name}>{f.name}</option>{/each}
                <option value="">(nothing — read MESHWARE_ITEM)</option>
              </select>
              <span class="text-xs text-ink-5">
                The rest of the inputs stay the same for every item.
              </span>
            </div>
          {:else if mode === "command"}
            <div class="mt-2 text-xs text-ink-5">
              <span class="mono">{"{}"}</span> in the command becomes the item — with no
              <span class="mono">{"{}"}</span> it is appended, as GNU parallel does. Every item is
              also in <span class="mono">$MESHWARE_ITEM</span>.
            </div>
          {/if}

          {#if firstItemPreview}
            <div class="mono mt-2 truncate rounded-sm bg-surface-2 px-3 py-2 text-xs text-ink-4">
              first item runs: {firstItemPreview}
            </div>
          {/if}
        </div>
      {/if}
    {/if}

    <!-- On Together, "how many, at once" is the question the tab exists to
         ask, so it goes here as a numbered step rather than inside Options,
         and "where" becomes the third question instead of the second. -->
    {#if page === "together"}
      <div class="mt-6 border-t border-edge pt-5">
        {@render heading("2", "How many machines, together?")}
        {@render gangQuestion()}
      </div>
    {/if}

    <!-- ===================== 2. where ===================== -->
    <div class="mt-6 border-t border-edge pt-5">
      {@render heading(page === "together" ? "3" : "2", "Where should it run?")}

      <!-- One dropdown listing the choices this mesh actually has —
           anywhere, each group, each node — instead of a mode switch plus
           a second picker. The raw selector expression is its last entry. -->
      <div class="mt-2.5 flex flex-wrap items-center gap-2.5">
        <select
          aria-label="Where it runs"
          class="field field-sunken select h-10 min-w-72 px-3 text-sm text-ink outline-none"
          value={where === "any" ? "any" : where === "node" ? `node:${nodeName}` : where === "group" ? `group:${groupTag}` : "custom"}
          onchange={(e) => {
            const v = e.currentTarget.value;
            if (v === "any") where = "any";
            else if (v === "custom") where = "custom";
            else if (v.startsWith("node:")) {
              where = "node";
              nodeName = v.slice(5);
            } else if (v.startsWith("group:")) {
              where = "group";
              groupTag = v.slice(6);
            }
          }}
        >
          <option value="any">Anywhere — the mesh picks a free node</option>
          {#if groups.length > 0}
            <optgroup label="A group of nodes">
              {#each groups as t (t)}<option value={`group:${t}`}>every node tagged {t}</option>{/each}
            </optgroup>
          {/if}
          {#if liveNodes.length > 0}
            <optgroup label="One specific node">
              {#each liveNodes as n (n.nodeId)}<option value={`node:${n.name}`}>{n.name}{n.isSelf ? " (this one)" : ""}</option>{/each}
            </optgroup>
          {/if}
          <option value="custom">A selector expression…</option>
        </select>

        {#if where === "custom"}
          <input
            bind:value={custom}
            placeholder="any | name=… | tag=… | fact.cpu_cores>=32"
            class="field field-sunken mono h-10 w-72 px-3 text-sm outline-none"
            title="A selector is a short expression that picks nodes — any, tag=web, name=nim, fact.cpu_cores>=32"
          />
        {/if}
      </div>

      <!-- Only a once-run chooses how many nodes to land on. Per item, the
           mesh decides: every node that can take a chunk takes chunks. -->
      {#if preview.length > 1 && !perItem}
        <div class="mt-3 flex flex-wrap items-center gap-2.5">
          <span class="text-sm text-ink-4">on</span>
          <select bind:value={spread} aria-label="On how many of the matching machines" class="select h-9 field field-sunken px-2.5 text-sm text-ink outline-none">
            <option value="one">one of them — whichever is free</option>
            <option value="all">all {preview.length} of them</option>
            <option value="some">a number of them</option>
          </select>
          {#if spread === "some"}
            <input type="number" min="1" max={preview.length} bind:value={someCount} aria-label="How many machines" class="num h-9 w-16 field field-sunken px-2.5 text-sm outline-none" />
          {/if}
          <span class="text-xs text-ink-5">
            {spreading ? `${hitCount} separate runs, one per node.` : "One run, on the least busy node."}
          </span>
        </div>
      {/if}

      <div class="mt-2.5 flex flex-wrap items-center gap-1.5 text-xs">
        {#if previewErr}
          <span style="color: var(--status-error);">{previewErr}</span>
        {:else if !previewKnown}
          <span class="text-ink-5">Checking which nodes match…</span>
        {:else if preview.length === 0}
          <span class="font-medium" style="color: var(--status-warn-ink);">No node matches this.</span>
        {:else}
          <span class="text-ink-4">Matches:</span>
          {#each preview.slice(0, 8) as n (n.nodeId)}
            <span class="hue-chip" style={hueStyle(hueOf(n.name))}>{n.name}</span>
          {/each}
          {#if preview.length > 8}<span class="text-ink-6">+{preview.length - 8} more</span>{/if}
        {/if}
      </div>
    </div>

    <!-- ===================== options ===================== -->
    <div class="mt-5 border-t border-edge pt-4">
      <button
        class="flex cursor-pointer items-center gap-1.5 rounded-sm px-1.5 py-1 text-sm font-semibold text-ink-3 transition-colors hover:bg-surface-2 hover:text-ink"
        onclick={() => (optionsOpen = !optionsOpen)}
      >
        <span class="transition-transform duration-150" style={optionsOpen ? "transform: rotate(90deg);" : ""}><Icon name="chevron-right" size={14} /></span>
        Options
        <span class="font-normal text-ink-5">
          {`— time limit${perItem ? ", what each item needs, chunking, where output lives" : ", priority"}${spreading ? ", batching" : ""}${mode === "command" ? ", environment" : ""}`}
        </span>
      </button>

      {#if optionsOpen}
        <div class="fade-in mt-3 space-y-4 rounded-lg border border-edge-card bg-canvas p-4">
          <div class="flex flex-wrap items-end gap-3">
            <label class="block w-36">
              <span class={ui.label}>Give up after</span>
              <input bind:value={timeout} placeholder="no limit" class="mono h-10 w-full field field-sunken px-3 text-sm outline-none" />
            </label>
            <div class={perItem ? "hidden" : ""}>
              <span class={ui.label}>Priority <span class="text-ink-6">(when a node is busy)</span></span>
              <div class="inline-flex h-10 items-center rounded-md bg-surface-2 p-[3px]">
                {#each ["low", "normal", "high"] as p (p)}
                  <button
                    class={`rounded-sm px-[11px] py-[5px] text-sm transition-colors ${priority === p ? "bg-surface font-semibold text-ink" : "font-medium text-ink-5 hover:text-ink"}`}
                    style={priority === p ? "box-shadow: var(--sh-control);" : ""}
                    onclick={() => (priority = p as Priority)}
                  >
                    {p}
                  </button>
                {/each}
              </div>
            </div>
            {#if timeout.trim() && timeoutSecs === undefined}
              <span class="pb-2.5 text-xs" style="color: var(--status-error);">try 30s, 5m or 2h</span>
            {/if}
          </div>

          <!-- Answers (M19 §5.4). Off by default and stated plainly: an
               operator restarting nginx on fifty boxes has no question to
               ask, and capture costs a log read per target. A script gets
               the same offer as a command — a Python task fanned out over
               the mesh is the shape of run that most obviously *has* an
               answer, and hiding the checkbox from it was the bug. -->
          {#if !perItem}
            <div class="border-t border-edge pt-3.5">
              <div class="microlabel mb-2.5">Answers</div>
              <label class="flex cursor-pointer items-start gap-2.5">
                <input type="checkbox" class="check mt-[2px]" bind:checked={capture} />
                <span>
                  <span class="flex items-center gap-1 text-sm font-medium text-ink">
                    Collect an answer from each node
                    <Hint
                      label="What comes back"
                      text="A table with one row per node — stdout, stdout_json when the whole of it parses, stderr and the exit code — instead of a wall of terminals. A script can publish more with ::meshware-output:: key=value."
                    />
                  </span>
                  <span class="mt-0.5 block text-xs text-ink-5">
                    One table, one row per node, instead of a wall of terminals.
                  </span>
                </span>
              </label>

              <label class="mt-3 block">
                <span class="mb-1.5 block text-xs text-ink-5">
                  Require of every row <span class="text-ink-6">(optional — a failing row fails the run)</span>
                </span>
                <input
                  bind:value={expect}
                  placeholder={'contains(stdout, "permitrootlogin no")'}
                  class="mono h-10 w-full field field-sunken px-3 text-sm outline-none"
                />
                {#if expectError}
                  <span class="mt-1 block text-xs" style="color: var(--status-error);">{expectError}</span>
                {:else}
                  <span class="mt-1 block text-xs text-ink-6">
                    contains · equals · starts_with · matches · gt / lt — or any built-in task id,
                    like <span class="mono">json.query(stdout_json, "$.total")</span>.
                  </span>
                {/if}
              </label>
            </div>
          {/if}

          {#if perItem}
            <div class="border-t border-edge pt-3.5">
              <div class="flex items-baseline gap-2">
                <span class={ui.label}>What one item needs</span>
                <span class="text-xs text-ink-6">
                  a node without this much is not a candidate — leave blank for “whatever is free”
                </span>
              </div>
              <div class="flex flex-wrap items-end gap-3">
                <label class="block w-28">
                  <span class="mb-1.5 block text-xs text-ink-5">Cores</span>
                  <input bind:value={cpus} inputmode="numeric" placeholder="any" class="num h-10 w-full field field-sunken px-3 text-sm outline-none" />
                </label>
                <label class="block w-32">
                  <span class="mb-1.5 block text-xs text-ink-5">Memory</span>
                  <input bind:value={mem} placeholder="any" class="mono h-10 w-full field field-sunken px-3 text-sm outline-none" />
                </label>
                <label class="block w-32">
                  <span class="mb-1.5 block text-xs text-ink-5">Scratch disk</span>
                  <input bind:value={disk} placeholder="any" class="mono h-10 w-full field field-sunken px-3 text-sm outline-none" />
                </label>
                <label class="block w-36">
                  <span class="mb-1.5 block text-xs text-ink-5">Items per chunk</span>
                  <input bind:value={chunkItems} inputmode="numeric" placeholder="automatic" class="num h-10 w-full field field-sunken px-3 text-sm outline-none" />
                </label>
                <span class="basis-full text-xs leading-relaxed" style={fits.ok < fits.of ? "color: var(--status-warn-ink);" : "color: var(--ink-5);"}>
                  {#if sizeError}
                    that {sizeError} is not a size — try 512M or 48G
                  {:else if cpusN || memMb || diskMb}
                    fits on {fits.ok} of {fits.of} matching node{fits.of === 1 ? "" : "s"}
                  {:else}
                    Chunking tunes itself from how long items actually take.
                  {/if}
                </span>
              </div>
            </div>

            <!-- The data plane (M9 §5.5). One checkbox, off by default:
                 an operator restarting nginx on fifty boxes has nothing to
                 keep and should never be shown this. Switching it on is
                 what turns a `parallel` into a `map`. -->
            <div class="border-t border-edge pt-3.5">
              <div class="microlabel mb-2.5">Output</div>
              <label class="flex cursor-pointer items-start gap-2.5">
                <input type="checkbox" bind:checked={keepOutput} class="mt-[3px] h-3.5 w-3.5 accent-[var(--accent)]" />
                <span>
                  <span class="flex items-center gap-1 text-sm font-medium text-ink">
                    Keep each item's output in the mesh
                    <Hint
                      label="Where output lives"
                      text="Output stays on the node that produced it and only a reference comes back, so large results never funnel through this machine. An item may write to $MESHWARE_OUT; anything else it prints is kept instead."
                    />
                  </span>
                  <span class="mt-0.5 block text-xs text-ink-5">
                    Results stay where they were made; only references come back.
                  </span>
                </span>
              </label>

              {#if keepOutput}
                <div class="mt-3 flex flex-wrap items-end gap-3">
                  <label class="block min-w-72 flex-1">
                    <span class="mb-1.5 block text-xs text-ink-5">
                      Then merge them with <span class="text-ink-6">— optional; reads them from
                      <span class="mono">$MESHWARE_INPUTS</span></span>
                    </span>
                    <input
                      bind:value={reduce}
                      spellcheck="false"
                      placeholder={'cat "$MESHWARE_INPUTS"/* | sort -u'}
                      class="mono h-10 w-full field field-sunken px-3 text-sm outline-none"
                    />
                  </label>
                  <label class="block w-52">
                    <span class="mb-1.5 block text-xs text-ink-5">Merge on</span>
                    <input
                      bind:value={reduceOn}
                      placeholder="wherever the data is"
                      class="mono h-10 w-full field field-sunken px-3 text-sm outline-none"
                    />
                  </label>
                </div>
                <div class="mt-2 text-xs text-ink-5">
                  Left blank, the merge runs on whichever node already holds most of the output —
                  which is the least data moved.
                </div>
              {/if}
            </div>
          {/if}


          <!-- C5 §9: accelerators. Offered for a saved script as well as a
               command — a service is the shape that gets a contract and a
               cache, so it is the shape GPU work actually takes, and
               command-only would have meant the cacheable path could not
               ask for a card. Still not per-item: that request has nowhere
               to ride yet. -->
          <!-- N2 — ports. Above Accelerators because far more work wants a
               port than wants a card, and inside the same `!perItem` guard
               for the same reason: a per-item fan-out has nowhere to put a
               per-item port. -->
          {#if !perItem}
            <!-- `items-start`, not `items-end`: the two fields carry hints of
                 different heights, so bottom-aligning the groups pushed the
                 *inputs* out of line with each other by the difference. -->
            <div class="flex flex-wrap items-start gap-3 border-t border-edge pt-3.5">
              <div class="microlabel w-full">Ports</div>
              <label class="block flex-1 min-w-48">
                <span class={ui.label}>Host ports</span>
                <input
                  bind:value={portsText}
                  class={ui.input}
                  placeholder="http:9095, admin:8080!"
                />
                <span class="mt-1 block text-xs text-ink-5">
                  One per line or comma-separated. A trailing
                  <code class="num">!</code> demands that exact host port; without
                  it the node allocates one. The job reads
                  <code class="num">MESHWARE_PORT_HTTP</code>.
                </span>
              </label>
              <label class="block w-56">
                <span class={ui.label}>Register as</span>
                <input
                  bind:value={registerAs}
                  class={ui.input}
                  placeholder="webapp"
                  disabled={ports.length === 0}
                />
                <span class="mt-1 block text-xs text-ink-5">
                  {#if ports.length === 0}
                    Needs a port first — a name with no address resolves to
                    nothing.
                  {:else}
                    Answers at <code class="num">{registerAs.trim() || "name"}.mesh</code>
                    while it runs.
                  {/if}
                </span>
              </label>
            </div>
          {/if}

          <!-- N5 — volumes. Below Ports because they are the same kind of
               ask, and under the same `!perItem` guard for the same reason:
               a per-item fan-out has nowhere to put a per-item claim. -->
          {#if !perItem}
            <div class="flex flex-wrap items-end gap-3 border-t border-edge pt-3.5">
              <div class="microlabel w-full">Volumes</div>
              <label class="block flex-1 min-w-48">
                <span class={ui.label}>Claim</span>
                <input
                  bind:value={volumesText}
                  class={ui.input}
                  placeholder="data, models:ro"
                />
                <span class="mt-1 block text-xs text-ink-5">
                  One per line or comma-separated.
                  <code class="num">name</code>,
                  <code class="num">name:ro</code>, or
                  <code class="num">name:/mnt/path:rw</code> to say where a
                  container mounts it. A claim is a <em>filter</em>: only a node
                  that lends the volume — and can lend it in that mode right now
                  — is a candidate. Mesh › Capacity lists this node's.
                </span>
              </label>
            </div>
          {/if}

          {#if !perItem}
            <div class="flex flex-wrap items-end gap-3 border-t border-edge pt-3.5">
              <div class="microlabel w-full">Accelerators</div>
              <label class="block w-32">
                <span class={ui.label}>GPUs</span>
                <input
                  bind:value={gpus}
                  inputmode="numeric"
                  placeholder="none"
                  class="num h-10 w-full field field-sunken px-3 text-sm outline-none"
                />
              </label>
              <label class="block w-32">
                <span class={ui.label}>Each with</span>
                <input
                  bind:value={vram}
                  placeholder="any VRAM"
                  class="mono h-10 w-full field field-sunken px-3 text-sm outline-none"
                />
              </label>
              <label class="block w-40">
                <span class={ui.label}>Model contains</span>
                <input
                  bind:value={deviceModel}
                  placeholder="any model"
                  class="mono h-10 w-full field field-sunken px-3 text-sm outline-none"
                />
              </label>
              {#if gpusN || deviceModel.trim()}
                <label class="flex items-center gap-2 pb-2.5 text-sm text-ink-3">
                  <input type="checkbox" class="check" bind:checked={gpuShare} />
                  Allow co-tenants
                </label>
              {/if}
              <span class="basis-full text-xs leading-relaxed text-ink-5">
                {#if gpusN || deviceModel.trim() || vramMb}
                  {gpuShare
                    ? "Shared: another job may hold the same card. Only jobs that also opted in."
                    : "Exclusive. The job is given specific devices and sees only those."}
                  {vramMb ? " The VRAM figure is per device — four 12 GB cards do not add up to 40 GB." : ""}
                {:else}
                  A node without the card is not a candidate, not a bad one.
                {/if}
              </span>
            </div>
          {/if}

          {#if spreading}
            <div class="flex flex-wrap items-end gap-3 border-t border-edge pt-3.5">
              <div class="microlabel w-full">Rollout</div>
              <label class="block w-40">
                <span class={ui.label}>At a time</span>
                <input bind:value={batch} placeholder="all at once" class="num h-10 w-full field field-sunken px-3 text-sm outline-none" />
              </label>
              <label class="block w-40">
                <span class={ui.label}>Stop after failures</span>
                <input bind:value={haltOnFail} placeholder="never stop" class="num h-10 w-full field field-sunken px-3 text-sm outline-none" />
              </label>
              <span class="basis-full text-xs leading-relaxed text-ink-5">A count or a percentage — “3” or “10%”. Nodes already running finish.</span>
            </div>
          {/if}

          {#if mode === "command"}
            <div class="border-t border-edge pt-3.5">
              <div class="flex items-baseline gap-2">
                <span class={ui.label}>Environment variables</span>
                <span class="text-xs text-ink-6">use <span class="mono">{"{{secret:name}}"}</span> to pull in a secret — resolved on the node, redacted from logs</span>
                <button class="ml-auto text-xs font-semibold" style="color: var(--accent-ink);" onclick={() => (env = [...env, { k: "", v: "" }])}>+ variable</button>
              </div>
              {#if env.length}
                <div class="mt-2 space-y-2">
                  {#each env as row, i (i)}
                    <div class="flex gap-2">
                      <input bind:value={row.k} aria-label="Environment variable name" placeholder="KEY" class="mono h-9 w-44 field field-sunken px-3 text-sm outline-none" />
                      <input bind:value={row.v} list="secret-refs" aria-label={`Value of ${row.k || "the variable"}`} placeholder="value" class="mono h-9 flex-1 field field-sunken px-3 text-sm outline-none" />
                      <button class="px-1 text-base text-ink-5 hover:text-error" title="remove" onclick={() => (env = env.filter((_, j) => j !== i))}>✕</button>
                    </div>
                  {/each}
                  <datalist id="secret-refs">{#each secretNames as n (n)}<option value={`{{secret:${n}}}`}></option><option value={`{{secret_file:${n}}}`}></option>{/each}</datalist>
                </div>
              {/if}
            </div>
          {/if}

        </div>
      {/if}
    </div>

    <!-- ============ remember this setup (2026-09) ============ -->
    <!--
      Getting a script running *right* is a dozen decisions across four
      disclosures, and until now every one of them was thrown away the
      moment the run finished. The definitions file beside a script already
      held what its arguments are; ticking this makes it hold how the
      script is run as well, and the next person to open this page — on any
      machine — finds the form already filled in.

      Placed immediately above Run rather than inside Options, because it
      is a decision *about this press of Run* and a disclosure is where you
      put the things that are not.
    -->
    {#if mode === "script" && service}
      <div class="mt-5 border-t border-edge pt-4">
        <label
          class="flex cursor-pointer items-start gap-2.5 rounded-md px-2 py-1.5 transition-colors hover:bg-surface-2"
          class:opacity-55={!canSaveDefaults}
        >
          <input type="checkbox" class="check mt-[2px]" bind:checked={saveDefaults} disabled={!canSaveDefaults} />
          <span class="min-w-0">
            <span class="flex flex-wrap items-center gap-x-1.5 text-sm font-medium text-ink">
              Save run parameters as default
              <Hint
                label="What gets saved"
                text="Everything on this page: the arguments' values, which of the three shapes this is, where it runs and how widely, time limit and priority, what one item needs, how many machines start together and where, accelerators, chunking, where output lives, and what is collected. It is written to the script's definitions file — created if there is not one — and into the script itself in the mesh, so it follows the script to every machine. The list of items is not saved: that is the run's input, not a setting."
              />
            </span>
            <span class="mt-0.5 block text-xs text-ink-5">
              {#if !canSaveDefaults}
                Needs the developer or admin role — saving publishes a new version of the script.
              {:else if scriptOrigin}
                Writes <span class="mono">{serviceName}</span>'s definitions file and publishes a new version, so this page opens filled in next time.
              {:else}
                Publishes a new version of <span class="mono">{serviceName}</span> carrying these settings. Director does not know the folder this was pushed from, so the file on disk is not touched.
              {/if}
            </span>
          </span>
        </label>

        {#if appliedNote && !saveResult && !saveErr}
          <!-- Stated, not silent. A page that quietly pre-fills itself is a
               page a person cannot trust; one that says what it did and
               where from is one they can correct. -->
          <div class="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 pl-2 text-xs text-ink-5">
            <Icon name="history" size={12} />
            <span>{appliedNote}</span>
            <button class="linkish text-xs" onclick={() => (appliedNote = null)}>Dismiss</button>
          </div>
        {/if}
        {#if saveBusy}
          <div class="mt-2 pl-2 text-xs text-ink-5">Saving these settings to {serviceName}…</div>
        {/if}
        {#if saveResult}
          <div class="mt-2 pl-2 text-xs" style="color: var(--accent-ink);">
            Run parameters saved — {saveResult}.
          </div>
        {/if}
        {#if saveErr}
          <div class="mt-2 pl-2 text-xs" style="color: var(--status-warn-ink);">{saveErr}</div>
        {/if}
      </div>
    {/if}

    <!-- ===================== go ===================== -->
    <div class="mt-5 flex items-center gap-4 border-t border-edge pt-4">
      <div class="min-w-0 text-sm">
        {#if blocker}
          <span style="color: {(previewKnown && preview.length === 0) || secretInArgv || previewErr ? 'var(--status-error)' : 'var(--ink-4)'};">{blocker}</span>
        {:else}
          <span class="text-ink-4">Runs</span>
          <span class="font-semibold text-ink">
            {mode === "command" ? "your command" : serviceName}
          </span>
          {#if perItem}
            <span class="text-ink-4">once for each of</span>
            <span class="num font-semibold text-ink">{items.length.toLocaleString()}</span>
            <span class="text-ink-4">items, spread across {fits.ok} node{fits.ok === 1 ? "" : "s"}</span>
          {:else}
            <span class="text-ink-4">{targetSentence}</span>
            {#if hitCount > 1}<span class="text-ink-5"> · {hitCount} runs</span>{/if}
          {/if}
        {/if}
      </div>
      <!-- Preview is a verb, not a checkbox beside Run (M11 §5.2): it
           resolves the same submission and dispatches nothing, which is
           useful whether or not any policy is watching. -->
      {#if !perItem}
        <button
          class="btn btn-secondary ml-auto h-[44px] flex-none px-5 text-base"
          disabled={!canRun}
          onclick={() => run({ dryRun: true })}
          title="resolve targets, env keys, secret names and blast radius — dispatch nothing"
        >
          Preview
        </button>
      {/if}
      <button
        class={`btn btn-primary h-[44px] flex-none px-8 text-base${perItem ? " ml-auto" : ""}`}
        disabled={!canRun}
        onclick={() => run()}
      >
        {phase === "running" ? "Running…" : "Run"}
      </button>
    </div>
  </div>

  {#if jobPreview}
    <div class="card fade-in p-[22px]">
      <div class="mb-3 flex items-baseline gap-3">
        <h2 class="m-0 text-md font-bold tracking-[-.02em] text-ink">Nothing was dispatched</h2>
        <span class="text-sm text-ink-4">this is what pressing Run would have done</span>
        <button class="ml-auto text-sm font-semibold text-ink-4 hover:text-ink" onclick={() => (jobPreview = null)}>Dismiss</button>
      </div>
      <PreviewPanel preview={jobPreview} />
      <button class={`${ui.primary} mt-4`} onclick={() => run()}>Run it</button>
    </div>
  {/if}

  {#if runErr}
    <div class="selectable rounded-lg px-4 py-3 text-base" style="background: var(--status-error-wash); color: var(--status-error-ink);">
      {runErr}
    </div>
  {/if}

  <!-- per-item results (M9 §6.3) -->
  {#if perItem && par.total > 0}
    <ParallelResults
      total={par.total}
      done={par.done}
      failed={par.failed}
      inflight={par.inflight}
      chunkItems={par.chunkItems}
      speculated={par.speculated}
      perNode={par.perNode}
      results={par.results}
      reduce={par.reduce}
      workers={par.workers}
      declined={par.declined}
      estimate={par.estimate}
      jobId={par.jobId || null}
      elapsedMicros={par.elapsedMicros}
      running={phase === "running"}
      onRetry={retryFailures}
    />
  {/if}

  <!-- C4 §8: the reservation, while it is still only a reservation.
       Nothing has started, nothing is failing, and the mesh is negotiating
       — a screen that showed a spinner and no explanation here would be a
       screen people cancel out of. -->
  {#if gangWait}
    <div class="card fade-in px-5 py-4">
      <div class="flex items-center gap-3">
        <span class="breathe text-sm font-semibold" style="color: var(--status-warn-ink);">
          holding {gangWait.held} of {gangWait.size}
        </span>
        <span class="num text-xs text-ink-5">round {gangWait.round}</span>
      </div>
      <div class="mt-1.5 text-sm text-ink-4">
        {#if gangWait.behind}
          Behind the senior gang <span class="mono text-xs">{gangWait.behind}</span>. It runs
          first; this one takes the reservations back when it releases.
        {:else}
          Waiting for room. Every member starts together or none of them does, so
          nothing has been dispatched yet.
        {/if}
      </div>
    </div>
  {/if}

  <!-- single terminal pane -->
  {#if single}
    <div class="on-code overflow-hidden rounded-xl bg-code" style="box-shadow: var(--sh-code);">
      <div class="flex h-11 items-center gap-2.5 px-4" style="border-bottom: 1px solid var(--edge-code);">
        <span class="text-sm font-semibold text-inkcode">{single.name}</span>
        <button class="mono text-xs text-inkcode-4 hover:text-inkcode-2" title="open in Jobs" onclick={() => openInJobs(single?.jobId ?? "")}>{single.jobId.slice(0, 18)} ↗</button>
        <span class="ml-auto"><StateBadge state={single.state ?? "running"} /></span>
        {#if !single.state}
          <button class="h-7 rounded-sm border px-2.5 text-xs font-semibold transition-colors" style="border-color: var(--status-error-edge); color: var(--status-error-ink);" onclick={() => api.jobCancel(single?.jobId ?? "").then((m) => app.flash(m)).catch((e) => app.flash(errText(e)))}>Stop</button>
        {/if}
      </div>
      {#if single.dropped > 0}
        <div class="border-b border-edge-code px-4 py-1.5 text-2xs text-inkcode-4">
          earliest {single.dropped.toLocaleString()} lines not shown — the pane keeps the last
          {OUTPUT_MAX_LINES.toLocaleString()}; the whole log is in Activity (↗ above)
        </div>
      {/if}
      <pre class="term dark-scroll m-0 max-h-96 overflow-y-auto px-4 py-3.5" style="color: var(--ink-code-1);">{single.lines || " "}</pre>
      <div class="mono flex h-10 items-center gap-3 px-4 text-xs" style="border-top: 1px solid var(--edge-code); color: var(--ink-code-4);">
        {#if single.state}
          <span style={single.code === 0 ? "color: var(--accent-bright);" : single.code !== null ? "color: var(--status-error-ink);" : ""}>
            {single.code === 0 ? "finished cleanly" : single.code !== null ? `failed — exit ${single.code}` : single.state}
          </span>
          <span>{dur(single)}</span>
        {:else}
          <span class="breathe" style="color: var(--accent-bright);">running…</span>
        {/if}
      </div>
    </div>
  {/if}

  <!-- the answers (M19 §5.4). Above the logs, deliberately: the whole
       milestone is that the answer should not have to be read out of six
       hundred terminals. One line here — how many answered, how many did
       not — and the table itself in a dialog (`AnswersModal`), because a
       thousand rows inline would BE the page. -->
  {#if rows.length > 0}
    <div class="card flex flex-wrap items-center gap-4 px-5 py-3.5" use:reveal={{ y: 6 }}>
      <span
        class="grid h-9 w-9 flex-none place-items-center rounded-lg"
        style="background: var(--hue-sky-wash); color: var(--hue-sky-ink);"
        aria-hidden="true"><Icon name="cat-list" size={18} /></span
      >
      <div class="min-w-0 flex-1">
        <div class="text-base font-semibold tracking-[-.01em] text-ink">Answers</div>
        <div class="mt-0.5 text-sm text-ink-4">
          <span class="num font-semibold text-ink">{rows.length.toLocaleString()}</span>
          {rows.length === 1 ? "node" : "nodes"} answered{#if phase === "running"}<span class="breathe"> so far…</span>{/if}{#if answersFailed > 0}
            · <span class="num font-semibold" style="color: var(--status-error-ink);">{answersFailed} failed</span>{/if}{#if expected}
            · <span style={expected.failed.length ? "color: var(--status-error-ink);" : "color: var(--accent-ink);"}>
              {expected.failed.length ? `${expected.failed.length} did not comply` : `all ${expected.passed} passed`}
            </span>{/if}
        </div>
      </div>
      <button class="btn btn-primary" onclick={() => (answersOpen = true)}>Open answers</button>
    </div>
    <AnswersModal open={answersOpen} {rows} {expected} onclose={() => (answersOpen = false)} />
  {/if}

  <!-- the fan-out grid -->
  {#if grid.length > 0}
    <div>
      <div class="mb-2.5 flex items-center justify-between text-sm">
        <span class="text-ink-4">
          <span class="num font-semibold text-ink">{grid.length}</span> node{grid.length === 1 ? "" : "s"}
          {#if batch.trim()}<span class="text-ink-6"> · {batch.trim()} at a time</span>{/if}
          {#if haltOnFail.trim()}<span class="text-ink-6"> · stops after {haltOnFail.trim()} fail</span>{/if}
        </span>
        {#if summary}
          <span class="num">
            <span style="color: var(--accent-ink);">{summary.ok} succeeded</span>
            {#if summary.failed > 0}· <span class="text-error">{summary.failed} failed</span>{/if}
            {#if summary.skipped > 0}· <span class="text-ink-4">{summary.skipped} skipped</span>{/if}
          </span>
        {/if}
      </div>
      <div class="grid grid-cols-2 gap-3.5 2xl:grid-cols-3" use:staggerIn={{ each: 30, y: 8 }}>
        {#each grid as cell, i (cell.jobId)}
          <button class="on-code overflow-hidden rounded-xl bg-code text-left" style="box-shadow: var(--sh-code);" onclick={() => (expanded = expanded === i ? null : i)}>
            <div class="flex h-10 items-center gap-2.5 px-3.5" style="border-bottom: 1px solid var(--edge-code);">
              <Dot kind={cell.state ?? "running"} size={6} />
              <span class="mono text-sm font-semibold text-inkcode">{cell.name}</span>
              <span class="mono ml-2 text-2xs text-inkcode-4">{cell.state ? dur(cell) : ""}</span>
              <span class="ml-auto"><StateBadge state={cell.state ?? "running"} /></span>
            </div>
            <pre
              class="term dark-scroll m-0 overflow-y-auto px-3.5 py-2.5 text-xs transition-[max-height] duration-200"
              style={`color: var(--ink-code-1); max-height: ${expanded === i ? 420 : 180}px; min-height: 64px; transition-duration: var(--dur-base);`}>{expanded === i ? cell.lines || " " : tail(cell.lines) || " "}</pre>
            {#if expanded === i && cell.dropped > 0}
              <div class="px-3.5 pb-2 text-2xs text-inkcode-4">
                earliest {cell.dropped.toLocaleString()} lines not shown — the whole log is in Activity
              </div>
            {/if}
          </button>
        {/each}
      </div>
    </div>
  {/if}
</div>
{/if}
