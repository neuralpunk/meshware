<script lang="ts">
  // M19 §5.4 P6 — one row per target: what each node *answered*, as
  // distinct from what it logged.
  //
  // The whole milestone in one component. "I ran that on 600 hosts and got
  // 600 logs; where is the answer?" is answered by a table whose columns
  // are the outputs the job published, not by a wall of terminals — so the
  // columns are decided by the rows rather than fixed, and a fan-out that
  // published nothing but the implicit four does not show four empty ones.
  //
  // **Two homes (2026-09).** Inline, as a card, this was fine at four rows
  // and hopeless at four hundred: the run page became the table, and the
  // fan-out grid under it was a scroll away that nobody took. So the table
  // also has a `fill` mode for `AnswersModal`, where it owns the modal's
  // height — sticky header, the rows scrolling inside.
  //
  // **The size pass (2026-09-11).** Everything below assumes the set is
  // large until proven otherwise, because the honest ceiling is not four
  // hundred rows of one line each — it is a `tar -tvf` on six hundred
  // machines. What that changed:
  //
  //   1. **The rows are windowed.** Past `VIRTUAL_FROM` only the slice
  //      under the viewport is in the DOM, with two spacer rows standing in
  //      for the rest. Row height is fixed for exactly this reason; the one
  //      expanded row is measured, because it is the one thing that is not.
  //   2. **Nothing whole ever reaches the DOM.** A cell shows 200
  //      characters of a value, a tooltip 400, the expansion 64 KB at a
  //      time. A 40 MB stdout in a `title=` attribute is not a tooltip, it
  //      is 40 MB of string on every render of the row.
  //   3. **The JSON pane is windowed too, by line.** `rowLines` gives the
  //      height of a row's block without rendering it (see `answers.ts`),
  //      so the pane can place line 900,000 by arithmetic.
  //   4. **Copy and Save stopped sharing a giant `$derived` string.** The
  //      old one re-serialised the whole set on every keystroke in the
  //      filter, whether or not the JSON pane was even showing. Copy
  //      builds the document on demand; Save streams it to disk a
  //      megabyte at a time and never builds it at all.
  //   5. **Save works.** It used to click a synthetic `<a download>` and
  //      revoke the blob URL on the next line — which does nothing at all
  //      in a Tauri webview, because there is no download manager behind
  //      it. It goes through the OS save dialog now (`lib/save.ts`).
  import { api, errText, type ResultRowView } from "../api";
  import {
    cellPreview,
    documentChunks,
    documentText,
    escapeRe,
    estimateBytes,
    fmtBytes,
    isFailure,
    outputKeys,
    rowJson,
    rowLines,
    rowMatches,
    sliceSafe,
    suggestedName,
    valueOf,
  } from "../answers";
  import { saveStreamed } from "../save";
  import Dot from "./Dot.svelte";
  import Icon from "./Icon.svelte";
  import { staggerIn } from "../motion";
  import { untrack } from "svelte";

  let {
    rows,
    /** The `--expect` verdict, when one was asserted. */
    expected = null,
    /** Shown beside the title when this is a stored set rather than a live run. */
    caption = "",
    /** Names the saved file: a fan-out id, a job id, whatever this set is. */
    source = "",
    /** Fill the parent (a modal): the header stays, the rows scroll. */
    fill = false,
    /** Present when the table sits in something that can be closed. */
    onclose,
  }: {
    rows: ResultRowView[];
    expected?: { expression: string; passed: number; failed: string[] } | null;
    caption?: string;
    source?: string;
    fill?: boolean;
    onclose?: () => void;
  } = $props();

  // -- the numbers that decide what reaches the DOM -----------------------------
  //
  // Every one of these is a ceiling on *rendered* content and never on
  // content available: the value is always one click or one Save away
  // whole. They are here together because they are one decision.

  /** Row height, in px. The CSS below must agree — the window is arithmetic. */
  const ROW_H = 34;
  /** Line height of the JSON pane, in px. Same deal. */
  const LINE_H = 18;
  /** Sticky header height, so `ensureVisible` does not scroll a row under it. */
  const HEAD_H = 31;
  /** Rows kept above and below the viewport, so a fast scroll stays painted. */
  const OVERSCAN = 6;
  /** Below this many rows, everything is in the DOM and the cascade plays. */
  const VIRTUAL_FROM = 80;
  /** Characters of a value that reach a table cell… */
  const CELL_CHARS = 200;
  /** …its tooltip… */
  const TIP_CHARS = 400;
  /** …and one line of the JSON pane. */
  const JSON_CHARS = 640;
  /** A value at or under this, on one line, is a fact and not a document:
      the expansion lists it rather than giving it a pane of its own. */
  const BRIEF_CHARS = 120;
  /** How much of a value the expansion shows, and adds per "Show more". */
  const REVEAL_STEP = 64 * 1024;
  /** Past this the clipboard is the wrong instrument and Save is the right one. */
  const COPY_MAX = 16 * 1024 * 1024;
  /** Failed `--expect` lines rendered before the strip stops listing them. */
  const EXPECT_LINES = 60;

  const keys = $derived(outputKeys(rows));
  const asserted = $derived(rows.some((r) => r.expectPassed !== null));
  const failures = $derived(rows.reduce((n, r) => n + (isFailure(r) ? 1 : 0), 0));

  // -- narrowing ----------------------------------------------------------------
  //
  // Debounced, because at a hundred thousand rows the filter is a scan over
  // every byte the set holds and doing it per keystroke is doing it eight
  // times to answer one question. `rowMatches` does that scan without
  // allocating (see `answers.ts`); the debounce is what keeps the caret
  // responsive while it runs.
  let query = $state("");
  let applied = $state("");
  let onlyFailures = $state(false);

  $effect(() => {
    const q = query;
    const t = setTimeout(() => (applied = q), 140);
    return () => clearTimeout(t);
  });
  const narrowing = $derived(query !== applied);

  const visible = $derived.by(() => {
    const q = applied.trim();
    const failOnly = onlyFailures;
    if (!q && !failOnly) return rows;
    const re = q ? new RegExp(escapeRe(q), "i") : null;
    const out: ResultRowView[] = [];
    for (const r of rows) {
      if (failOnly && !isFailure(r)) continue;
      if (re && !rowMatches(r, re)) continue;
      out.push(r);
    }
    return out;
  });
  const narrowed = $derived(visible.length !== rows.length);

  /** Sizes come from lengths alone — no value is ever read to measure it. */
  const totalBytes = $derived(estimateBytes(rows));
  const shownBytes = $derived(narrowed ? estimateBytes(visible) : totalBytes);

  // -- what is open, and where the cursor is ------------------------------------
  let open = $state<string | null>(null);
  let cursor = $state(-1);
  const openIdx = $derived(open === null ? -1 : visible.findIndex((r) => r.jobId === open));

  /** A narrowed set is a different list; keeping a scroll position into the
      old one puts somebody at row 4,000 of 12. */
  $effect(() => {
    applied;
    onlyFailures;
    untrack(() => {
      open = null;
      cursor = -1;
      detailH = 0;
      if (scroller) scroller.scrollTop = 0;
      scrollTop = 0;
      if (jsonPane) jsonPane.scrollTop = 0;
      jScrollTop = 0;
    });
  });

  function toggle(jobId: string, idx: number) {
    cursor = idx;
    if (open === jobId) {
      open = null;
      detailH = 0;
    } else {
      open = jobId;
      detailH = 0;
      revealed = {};
    }
  }

  // -- the window ---------------------------------------------------------------
  //
  // Fixed row height plus one measured exception. The expansion is the
  // exception: it is as tall as the values in it, so its height is observed
  // and folded into the offset of every row below it. `detailH` survives the
  // expansion scrolling out of the window on purpose — the space it occupies
  // has to keep existing whether or not it is painted.
  let scroller = $state<HTMLElement | null>(null);
  let scrollTop = $state(0);
  let scrollLeft = $state(0);
  let viewH = $state(520);
  let detailH = $state(0);
  let frame = 0;

  const virtual = $derived(visible.length > VIRTUAL_FROM);
  const extraH = $derived(openIdx >= 0 ? detailH : 0);
  const totalH = $derived(visible.length * ROW_H + extraH);

  /** Where row `i` starts, measured from the first row — expansion
      included. The header sits above all of them and is `HEAD_H` tall, so
      a scroll position is `HEAD_H + offsetOf(i)`; forgetting that is a
      table whose End key stops one header short of the last row. */
  const offsetOf = (i: number) => i * ROW_H + (extraH > 0 && i > openIdx ? extraH : 0);

  const win = $derived.by(() => {
    if (!virtual) return { start: 0, end: visible.length, padTop: 0, padBottom: 0 };
    const above = Math.max(0, scrollTop - HEAD_H);
    const detailTop = (openIdx + 1) * ROW_H;
    let first: number;
    if (extraH === 0 || above < detailTop) first = Math.floor(above / ROW_H);
    else if (above < detailTop + extraH) first = openIdx;
    else first = Math.floor((above - extraH) / ROW_H);
    const start = Math.max(0, first - OVERSCAN);
    const end = Math.min(visible.length, start + Math.ceil(viewH / ROW_H) + OVERSCAN * 2);
    return {
      start,
      end,
      padTop: offsetOf(start),
      padBottom: Math.max(0, totalH - offsetOf(end)),
    };
  });
  const windowRows = $derived(visible.slice(win.start, win.end));

  function viewport(node: HTMLElement) {
    scroller = node;
    const onScroll = () => {
      if (frame) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        scrollTop = node.scrollTop;
        scrollLeft = node.scrollLeft;
      });
    };
    const ro = new ResizeObserver(() => (viewH = node.clientHeight));
    ro.observe(node);
    viewH = node.clientHeight;
    node.addEventListener("scroll", onScroll, { passive: true });
    return {
      destroy() {
        ro.disconnect();
        node.removeEventListener("scroll", onScroll);
        if (frame) cancelAnimationFrame(frame);
        scroller = null;
      },
    };
  }

  function measureDetail(node: HTMLElement) {
    const ro = new ResizeObserver(() => (detailH = node.offsetHeight));
    ro.observe(node);
    detailH = node.offsetHeight;
    return { destroy: () => ro.disconnect() };
  }

  function ensureVisible(i: number) {
    if (!scroller) return;
    const top = HEAD_H + offsetOf(i);
    // Under the sticky header rather than behind it, going up; flush with
    // the bottom edge going down.
    if (top - HEAD_H < scroller.scrollTop) scroller.scrollTop = Math.max(0, top - HEAD_H);
    else if (top + ROW_H > scroller.scrollTop + scroller.clientHeight) {
      scroller.scrollTop = top + ROW_H - scroller.clientHeight;
    }
  }

  function move(by: number) {
    if (visible.length === 0) return;
    const next = Math.min(visible.length - 1, Math.max(0, (cursor < 0 ? -1 : cursor) + by));
    cursor = next;
    ensureVisible(next);
  }

  let filterField = $state<HTMLInputElement | null>(null);
  let shell = $state<HTMLElement | null>(null);

  // The shortcuts are listened for here, on the table's own root, rather
  // than on the document — so an inline table on a page full of other
  // things never eats an arrow key that was not meant for it. In a modal
  // that means this root has to hold the focus, and it takes it on open:
  // focusing the dialog *around* it would put the listener below the
  // focused node, where a keydown never reaches it.
  $effect(() => {
    if (fill) shell?.focus({ preventScroll: true });
  });

  function onKeydown(e: KeyboardEvent) {
    const target = e.target as HTMLElement | null;
    const typing = target?.tagName === "INPUT" || target?.tagName === "TEXTAREA";
    if (typing) {
      if (e.key === "Escape" && query) {
        e.stopPropagation();
        query = "";
      }
      return;
    }
    if (pane !== "table") return;
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        move(1);
        break;
      case "ArrowUp":
        e.preventDefault();
        move(-1);
        break;
      case "PageDown":
        e.preventDefault();
        move(Math.max(1, Math.floor(viewH / ROW_H) - 1));
        break;
      case "PageUp":
        e.preventDefault();
        move(-Math.max(1, Math.floor(viewH / ROW_H) - 1));
        break;
      case "Home":
        e.preventDefault();
        move(-visible.length);
        break;
      case "End":
        e.preventDefault();
        move(visible.length);
        break;
      case "Enter":
        if (cursor >= 0 && cursor < visible.length) {
          e.preventDefault();
          toggle(visible[cursor].jobId, cursor);
        }
        break;
      case "/":
        e.preventDefault();
        filterField?.focus();
        break;
      default:
        break;
    }
  }

  // -- the two panes ------------------------------------------------------------
  //
  // A table answers "which node disagrees"; JSON answers "what exactly did
  // it say". Same rows, same order, same filter.
  let pane = $state<"table" | "json">("table");

  // Node, Exit and Expect are as wide as they need to be and no wider; the
  // output columns share whatever is left, equally. `minWidth` is the point
  // at which sharing stops and the table starts scrolling sideways instead
  // — a 24-character column is not a column, it is a hint that something
  // was there.
  const NODE_W = 230;
  const EXIT_W = 68;
  const EXPECT_W = 86;
  const keyMin = $derived(keys.length <= 2 ? 320 : keys.length <= 4 ? 240 : 184);
  const minWidth = $derived(
    NODE_W + EXIT_W + (asserted ? EXPECT_W : 0) + keys.length * keyMin,
  );
  const cols = $derived(2 + (asserted ? 1 : 0) + keys.length);

  // Who, and how it went, stay on screen while the outputs scroll past
  // them: a value in the eleventh column is not an answer if you cannot
  // see whose it is. Only when there is something to scroll — with no
  // output columns the node column takes the slack and its width is no
  // longer the fixed number these offsets are built from.
  const pinned = $derived(keys.length > 0);
  const verdictLeft = $derived(NODE_W + EXIT_W);

  const tip = (v: string) => (v.length > TIP_CHARS ? `${sliceSafe(v, TIP_CHARS)}…` : v);

  // -- the JSON pane ------------------------------------------------------------
  let jsonPane = $state<HTMLElement | null>(null);
  let jScrollTop = $state(0);
  let jViewH = $state(520);
  let jFrame = 0;

  /** Cumulative line offsets, one entry per row. O(1) per row because a
      value's newlines are escaped rather than printed — see `rowLines`. */
  const lineAt = $derived.by(() => {
    const off = new Float64Array(visible.length + 1);
    let acc = 0;
    for (let i = 0; i < visible.length; i += 1) {
      off[i] = acc;
      acc += rowLines(visible[i]);
    }
    off[visible.length] = acc;
    return off;
  });
  /** Plus the `[` and `]` that bracket them. */
  const jsonLines = $derived(lineAt[visible.length] + 2);

  /** The last row whose block starts at or before `line`. */
  function rowAtLine(line: number): number {
    let lo = 0;
    let hi = visible.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (lineAt[mid] <= line) lo = mid;
      else hi = mid - 1;
    }
    return Math.max(0, lo);
  }

  // Scrolling re-renders the window, so the blocks in it are cached. The
  // cache is dropped wholesale rather than evicted one at a time: scroll
  // locality means a bounded map of recent blocks is all it ever needs to
  // be, and a map that grows with the set would put the whole document back
  // in memory one screen at a time.
  let blocks = new Map<string, string>();
  $effect(() => {
    visible;
    untrack(() => blocks.clear());
  });

  function block(r: ResultRowView, comma: boolean): string {
    const key = `${r.jobId}${comma ? "," : ""}`;
    let text = blocks.get(key);
    if (text === undefined) {
      if (blocks.size > 400) blocks.clear();
      text = rowJson(r, "  ", JSON_CHARS, comma);
      blocks.set(key, text);
    }
    return text;
  }

  const jwin = $derived.by(() => {
    if (visible.length === 0) return { text: "[]", padTop: 0, padBottom: 0 };
    const firstLine = Math.max(0, Math.floor(jScrollTop / LINE_H) - OVERSCAN * 3);
    const lastLine = Math.ceil((jScrollTop + jViewH) / LINE_H) + OVERSCAN * 3;
    const start = rowAtLine(Math.max(0, firstLine - 1));
    const end = Math.min(visible.length, rowAtLine(Math.max(0, lastLine - 1)) + 1);
    const parts: string[] = [];
    if (start === 0) parts.push("[");
    for (let i = start; i < end; i += 1) parts.push(block(visible[i], i < visible.length - 1));
    if (end === visible.length) parts.push("]");
    return {
      text: parts.join("\n"),
      padTop: start === 0 ? 0 : (1 + lineAt[start]) * LINE_H,
      padBottom:
        end === visible.length ? 0 : Math.max(0, (jsonLines - 1 - lineAt[end]) * LINE_H),
    };
  });

  /**
   * The widest line the whole document can produce, in characters.
   *
   * From lengths alone, so the pane's horizontal extent belongs to the
   * document rather than to whichever forty rows happen to be rendered.
   * Without this the horizontal scrollbar resizes under the pointer every
   * time a long value scrolls into or out of the window, which is the kind
   * of thing that makes a virtualised pane feel broken even when it is not.
   */
  const jsonWidthCh = $derived.by(() => {
    let max = 28;
    for (const r of visible) {
      const own = Math.max(r.node.length, r.jobId.length, r.state.length) + 22;
      if (own > max) max = own;
      for (const [k, v] of r.outputs) {
        const n = k.length + Math.min(v.length, JSON_CHARS) + 26;
        if (n > max) max = n;
      }
      const d = Math.min(r.expectDetail.length, JSON_CHARS) + 26;
      if (d > max) max = d;
    }
    return Math.min(max, JSON_CHARS + 60);
  });

  /** Any value long enough that the pane is showing a shortened one. */
  const jsonShortened = $derived(
    visible.some((r) => r.outputs.some(([, v]) => v.length > JSON_CHARS)),
  );

  function jsonViewport(node: HTMLElement) {
    jsonPane = node;
    const onScroll = () => {
      if (jFrame) return;
      jFrame = requestAnimationFrame(() => {
        jFrame = 0;
        jScrollTop = node.scrollTop;
      });
    };
    const ro = new ResizeObserver(() => (jViewH = node.clientHeight));
    ro.observe(node);
    jViewH = node.clientHeight;
    node.addEventListener("scroll", onScroll, { passive: true });
    return {
      destroy() {
        ro.disconnect();
        node.removeEventListener("scroll", onScroll);
        if (jFrame) cancelAnimationFrame(jFrame);
        jsonPane = null;
      },
    };
  }

  // Colour comes from a pass over the window's text — forty rows of it,
  // never the document. The escape is not optional: node names and values
  // are strings other people's machines chose.
  const ESCAPES: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;" };
  function paint(text: string): string {
    return text
      .replace(/[&<>]/g, (c) => ESCAPES[c])
      .replace(
        /("(?:[^"\\]|\\.)*")(\s*:)?|\b(true|false|null)\b|(-?\d+(?:\.\d+)?)/g,
        (_m, str: string, colon: string, lit: string, num: string) => {
          if (str !== undefined) {
            return colon !== undefined
              ? `<span class="jkey">${str}</span><span class="jpunct">${colon}</span>`
              : `<span class="jstr">${str}</span>`;
          }
          return lit !== undefined
            ? `<span class="jlit">${lit}</span>`
            : `<span class="jnum">${num}</span>`;
        },
      );
  }

  // -- the expansion ------------------------------------------------------------
  //
  // A truncated cell answers "did it differ"; this answers "what did it
  // say". A script's stdout is the whole reason the run happened, and 280
  // px of it is not an answer — but neither is forty megabytes of it in one
  // `<pre>`, so it arrives 64 KB at a time.
  let revealed = $state<Record<string, number>>({});
  const shownOf = (k: string) => revealed[k] ?? REVEAL_STEP;

  /** One short line: a fact. The `&&` order matters — `includes` must never
      run on a value that could be forty megabytes long. */
  const isBrief = (v: string) => v.length > 0 && v.length <= BRIEF_CHARS && !v.includes("\n");

  function reveal(k: string) {
    revealed = { ...revealed, [k]: shownOf(k) + REVEAL_STEP };
  }

  // -- copy, and save -----------------------------------------------------------
  let copied = $state(false);
  let saving = $state(false);
  let savedBytes = $state(0);
  let cancelRequested = $state(false);
  let note = $state<{ kind: "ok" | "bad"; text: string } | null>(null);

  async function copyText(text: string, what: string) {
    // The same ceiling the whole document gets. One `stdout` can be past it
    // on its own, and the clipboard is not the way to move that — "save…"
    // beside it is, and it streams.
    if (text.length > COPY_MAX) {
      note = {
        kind: "bad",
        text: `${what} is about ${fmtBytes(text.length)} — more than the clipboard should carry. Use “save…” beside it instead.`,
      };
      return;
    }
    try {
      await api.copyText(text);
      copied = true;
      note = null;
      setTimeout(() => (copied = false), 1400);
    } catch (e) {
      note = { kind: "bad", text: `${what} could not be copied: ${errText(e)}` };
    }
  }

  function copyJson() {
    if (shownBytes > COPY_MAX) {
      note = {
        kind: "bad",
        text: `These answers are about ${fmtBytes(shownBytes)} — more than the clipboard should carry. Save them to a file instead.`,
      };
      return;
    }
    void copyText(documentText(visible), "The answers");
  }

  /**
   * Write the answers out.
   *
   * The filter is part of what is saved, deliberately: a copy that ignored
   * it would hand somebody a file that disagrees with the screen they were
   * looking at when they pressed the button. The footer says how many rows
   * that is.
   */
  async function saveJson() {
    if (saving) return;
    const set = visible;
    saving = true;
    savedBytes = 0;
    cancelRequested = false;
    note = null;
    try {
      const path = await saveStreamed({
        title: "Save these answers",
        suggested: suggestedName(source),
        extension: "json",
        filterName: "JSON document",
        chunks: () => documentChunks(set, null),
        onProgress: (n) => (savedBytes = n),
        cancelled: () => cancelRequested,
      });
      note = cancelRequested
        ? { kind: "bad", text: "Save cancelled — nothing was written." }
        : path
          ? {
              kind: "ok",
              text: `Saved ${set.length.toLocaleString()} ${set.length === 1 ? "row" : "rows"} to ${path}`,
            }
          : null;
    } catch (e) {
      note = { kind: "bad", text: errText(e) };
    } finally {
      saving = false;
    }
  }

  /** One value, on its own, for the case where the value *is* the answer. */
  async function saveValue(r: ResultRowView, key: string) {
    if (saving) return;
    const value = valueOf(r, key);
    saving = true;
    savedBytes = 0;
    cancelRequested = false;
    note = null;
    try {
      const path = await saveStreamed({
        title: `Save ${key} from ${r.node}`,
        suggested: `${r.node}-${key}.txt`,
        extension: "txt",
        filterName: "Text",
        // Sliced rather than handed over whole: the point of the streamed
        // sink is that a 400 MB value never exists twice.
        chunks: function* () {
          for (let i = 0; i < value.length; i += 1 << 20) yield value.slice(i, i + (1 << 20));
        },
        onProgress: (n) => (savedBytes = n),
        cancelled: () => cancelRequested,
      });
      if (path) note = { kind: "ok", text: `Saved ${key} to ${path}` };
    } catch (e) {
      note = { kind: "bad", text: errText(e) };
    } finally {
      saving = false;
    }
  }

  const savePct = $derived(
    saving && shownBytes > 0 ? Math.min(99, Math.round((savedBytes / shownBytes) * 100)) : 0,
  );

  const answered = $derived(
    `${rows.length.toLocaleString()} ${rows.length === 1 ? "node" : "nodes"} answered`,
  );
</script>

<svelte:document
  onkeydown={(e) => {
    // ⌘/Ctrl-F is what a person reaches for over a table this size, and the
    // webview has no find bar of its own to take it.
    if (e.ctrlKey && e.key === "f" && filterField) {
      e.preventDefault();
      filterField.focus();
      filterField.select();
    }
  }}
/>

<!-- The shell hosts the table's keyboard shortcuts (↑↓ ↵ /), which are
     shortcuts and not a control: every one of them is also reachable by
     tabbing to the row button or the filter field. -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  bind:this={shell}
  class="ans {fill ? 'flex min-h-0 flex-1 flex-col' : 'card overflow-hidden'} outline-none"
  role="group"
  aria-label="Answers"
  tabindex="-1"
  onkeydown={onKeydown}
>
  <!-- ── header ─────────────────────────────────────────────────────── -->
  <div class="ans-bar flex flex-none flex-wrap items-center gap-x-3 gap-y-2 px-5 py-3">
    <span class="ans-mark" aria-hidden="true"><Icon name="cat-list" size={16} /></span>
    <div class="min-w-0 flex-1">
      <h2 class="truncate text-[15px] font-semibold tracking-[-.01em] text-ink">Answers</h2>
      <div class="mt-0.5 truncate text-xs text-ink-4">
        {caption || answered}{#if failures > 0}<span class="ans-sep">·</span><span
            class="num font-semibold"
            style="color: var(--status-error-ink);">{failures.toLocaleString()} failed</span
          >{/if}{#if rows.length}<span class="ans-sep">·</span><span class="num" title="the size of the whole document"
            >≈{fmtBytes(totalBytes)}</span
          >{/if}
      </div>
    </div>

    <div class="ans-seg" role="tablist" aria-label="How to read the answers">
      {#each [["table", "Table"], ["json", "JSON"]] as [id, text] (id)}
        <button role="tab" aria-selected={pane === id} onclick={() => (pane = id as typeof pane)}>
          {text}
        </button>
      {/each}
    </div>

    <div class="ans-acts">
      <!-- Both act on what the filter left, which is what the footer counts:
           a copy that quietly ignored the filter would hand somebody a file
           that disagrees with the screen they were looking at. -->
      <button
        class="ans-act"
        onclick={copyJson}
        disabled={visible.length === 0}
        title="Copy {narrowed ? 'the filtered answers' : 'the answers'} as JSON"
      >
        <Icon name={copied ? "check" : "copy"} size={13} />
        <span>{copied ? "Copied" : "Copy"}</span>
      </button>
      <button
        class="ans-act"
        onclick={saveJson}
        disabled={saving || visible.length === 0}
        title="Write {narrowed ? 'the filtered answers' : 'the answers'} to a file"
      >
        <Icon name="save" size={13} />
        <span>{saving ? "Saving…" : "Save"}</span>
      </button>
      {#if onclose}
        <button class="iconbtn ml-1 h-7 w-7" title="Close (Esc)" aria-label="Close" onclick={onclose}>
          <Icon name="close" size={14} />
        </button>
      {/if}
    </div>
  </div>

  <!-- ── narrowing ──────────────────────────────────────────────────── -->
  {#if fill || rows.length > 8}
    <div class="ans-tools flex flex-none flex-wrap items-center gap-2.5 px-5 py-2">
      <label class="relative">
        <span class="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-ink-faint">
          <Icon name="search" size={13} />
        </span>
        <input
          bind:this={filterField}
          bind:value={query}
          class="field h-7 w-60 pr-7 pl-7 text-xs outline-none"
          placeholder="Filter node, key or value"
          aria-label="Filter answers"
          spellcheck="false"
        />
        {#if query}
          <button
            class="absolute top-1/2 right-1.5 grid h-4 w-4 -translate-y-1/2 place-items-center rounded text-ink-faint hover:text-ink-3"
            aria-label="Clear the filter"
            onclick={() => (query = "")}><Icon name="close" size={10} /></button
          >
        {/if}
      </label>

      <button
        class="ans-chip"
        class:is-on={onlyFailures}
        disabled={failures === 0}
        aria-pressed={onlyFailures}
        onclick={() => (onlyFailures = !onlyFailures)}
      >
        <Dot kind={failures ? "failed" : "queued"} size={5} />
        Failures only
        {#if failures > 0}<span class="num">{failures.toLocaleString()}</span>{/if}
      </button>

      {#if narrowed || narrowing}
        <span class="text-xs text-ink-5" aria-live="polite">
          {#if narrowing}
            filtering…
          {:else}
            <span class="num font-semibold text-ink-3">{visible.length.toLocaleString()}</span>
            of {rows.length.toLocaleString()}
          {/if}
        </span>
      {/if}

      <span class="ml-auto hidden text-xs text-ink-6 lg:inline">
        {#if pane === "table"}↑↓ move · ↵ expand · / filter{:else}long values are shortened here — Copy and Save write them whole{/if}
      </span>
    </div>
  {/if}

  <!-- ── the assertion ──────────────────────────────────────────────── -->
  {#if expected}
    <div
      class="ans-expect flex-none px-5 py-2.5 text-sm"
      style={expected.failed.length
        ? "background: var(--status-error-wash); color: var(--status-error-ink);"
        : "background: var(--accent-wash); color: var(--accent-ink);"}
    >
      <div class="mono font-semibold">{expected.expression}</div>
      {#if expected.failed.length}
        <div class="mt-1">
          {expected.failed.length.toLocaleString()} of {(expected.passed + expected.failed.length).toLocaleString()} did
          not comply:
        </div>
        <div class="max-h-24 overflow-y-auto">
          {#each expected.failed.slice(0, EXPECT_LINES) as line (line)}
            <div class="mono mt-0.5 truncate text-xs" title={line}>{line}</div>
          {/each}
          {#if expected.failed.length > EXPECT_LINES}
            <div class="mt-1 text-xs opacity-80">
              …and {(expected.failed.length - EXPECT_LINES).toLocaleString()} more — tick
              <span class="font-semibold">Failures only</span> to read them as rows.
            </div>
          {/if}
        </div>
      {:else}
        <div class="mt-0.5">all {expected.passed.toLocaleString()} passed</div>
      {/if}
    </div>
  {/if}

  <!-- ── the body ───────────────────────────────────────────────────── -->
  {#if rows.length === 0}
    <div class="ans-blank flex-1 px-5 py-14 text-center">
      <div class="text-sm font-semibold text-ink-3">No answers in this set</div>
      <div class="mt-1 text-sm text-ink-5">
        Rows exist only for a run that asked its targets for them.
      </div>
    </div>
  {:else if pane === "json"}
    <div class="ans-body ans-json {fill ? 'min-h-0 flex-1' : 'max-h-[30rem]'}" use:jsonViewport>
      <div style="height:{jwin.padTop}px" aria-hidden="true"></div>
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      <pre class="ans-code selectable" style="width: calc({jsonWidthCh}ch + 40px)">{@html paint(jwin.text)}</pre>
      <div style="height:{jwin.padBottom}px" aria-hidden="true"></div>
    </div>
  {:else if visible.length === 0}
    <div class="ans-blank flex-1 px-5 py-14 text-center">
      <div class="text-sm font-semibold text-ink-3">
        Nothing matches{query.trim() ? ` “${query.trim()}”` : ""}{onlyFailures
          ? " among the failures"
          : ""}.
      </div>
      <button
        class="mt-3 text-xs"
        style="color: var(--accent-ink);"
        onclick={() => {
          query = "";
          onlyFailures = false;
        }}>Clear the filter</button
      >
    </div>
  {:else}
    <div class="ans-body {fill ? 'min-h-0 flex-1' : 'max-h-[30rem]'}" use:viewport>
      <table class="ans-table" class:shifted={scrollLeft > 0} style="min-width:{minWidth}px">
        <colgroup>
          <col style={keys.length ? `width:${NODE_W}px` : ""} />
          <col style="width:{EXIT_W}px" />
          {#if asserted}<col style="width:{EXPECT_W}px" />{/if}
          {#each keys as k (k)}<col />{/each}
        </colgroup>
        <thead>
          <tr>
            <th class:pin={pinned} style="left:0">Node</th>
            <th class="ta-r" class:pin={pinned} class:pin-edge={pinned && !asserted} style="left:{NODE_W}px">
              Exit
            </th>
            {#if asserted}
              <th class:pin={pinned} class:pin-edge={pinned} style="left:{verdictLeft}px">Expect</th>
            {/if}
            {#each keys as k (k)}<th title={k}>{k}</th>{/each}
          </tr>
        </thead>
        <tbody use:staggerIn={{ selector: ":scope > tr", each: 12, y: 4, cap: virtual ? 0 : 16, key: `${applied}${onlyFailures}` }}>
          {#if win.padTop > 0}
            <tr class="ans-pad" aria-hidden="true" style="height:{win.padTop}px"><td colspan={cols}></td></tr>
          {/if}

          {#each windowRows as r, i (r.jobId)}
            {@const idx = win.start + i}
            {@const isOpen = open === r.jobId}
            <tr class="ans-row" class:is-open={isOpen} class:is-cursor={cursor === idx}>
              <td class:pin={pinned} style="left:0">
                <button
                  class="ans-node"
                  aria-expanded={isOpen}
                  title="{r.node} — {isOpen ? 'hide' : 'show'} every value"
                  onclick={() => toggle(r.jobId, idx)}
                >
                  <span class="ans-caret" class:is-open={isOpen}><Icon name="chevron-right" size={11} /></span>
                  <Dot kind={r.state === "succeeded" ? "succeeded" : "failed"} size={6} />
                  <span class="truncate font-semibold text-ink">{r.node}</span>
                </button>
              </td>
              <td
                class="num ta-r"
                class:pin={pinned}
                class:pin-edge={pinned && !asserted}
                style="left:{NODE_W}px; {r.exitCode === 0
                  ? 'color: var(--ink-3);'
                  : 'color: var(--status-error-ink); font-weight: 600;'}"
              >
                {r.exitCode ?? "—"}
              </td>
              {#if asserted}
                <td class:pin={pinned} class:pin-edge={pinned} style="left:{verdictLeft}px">
                  {#if r.expectPassed === true}
                    <span class="text-xs font-semibold" style="color: var(--accent-ink);">pass</span>
                  {:else if r.expectPassed === false}
                    <span class="text-xs font-semibold" style="color: var(--status-error-ink);" title={tip(r.expectDetail)}>
                      fail
                    </span>
                  {:else}
                    <span class="text-xs text-ink-6">—</span>
                  {/if}
                </td>
              {/if}
              {#each keys as k (k)}
                {@const v = valueOf(r, k)}
                <td class="mono selectable ans-val" title={v ? tip(v) : undefined}>
                  {cellPreview(v, CELL_CHARS) || "—"}
                </td>
              {/each}
            </tr>

            {#if isOpen}
              {@const brief = keys.filter((k) => isBrief(valueOf(r, k)))}
              {@const bulky = keys.filter((k) => {
                const v = valueOf(r, k);
                return v.length > 0 && !isBrief(v);
              })}
              <tr class="ans-detail" use:measureDetail>
                <td colspan={cols}>
                  {#if r.expectPassed === false && r.expectDetail}
                    <div class="mb-3 text-xs" style="color: var(--status-error-ink);">
                      {r.expectDetail}
                    </div>
                  {/if}
                  <div class="flex flex-col gap-2.5">
                    <!-- Facts first, as facts: a kernel release does not need
                         a pane of its own, and five of them stacked as panes
                         push the one value somebody opened the row for off
                         the bottom of the screen. -->
                    {#if brief.length}
                      <div class="ans-brief">
                        {#each brief as k (k)}
                          <div class="label">{k}</div>
                          <div class="mono selectable ans-brief-v">{valueOf(r, k)}</div>
                          <button
                            class="ans-copy"
                            title="Copy {k}"
                            aria-label="Copy {k}"
                            onclick={() => copyText(valueOf(r, k), k)}><Icon name="copy" size={11} /></button
                          >
                        {/each}
                      </div>
                    {/if}
                    {#each bulky as k (k)}
                      {@const v = valueOf(r, k)}
                      {@const shown = shownOf(k)}
                      <div class="ans-value">
                        <div class="ans-value-bar">
                          <span class="label">{k}</span>
                          <span class="num text-ink-6">{fmtBytes(v.length)}</span>
                          <span class="flex-1"></span>
                          <button class="ans-mini" onclick={() => copyText(v, k)}>copy</button>
                          {#if v.length > REVEAL_STEP}
                            <button class="ans-mini" onclick={() => saveValue(r, k)} disabled={saving}>
                              save…
                            </button>
                          {/if}
                        </div>
                        <pre class="ans-pre selectable">{sliceSafe(v, shown)}</pre>
                        {#if v.length > shown}
                          <div class="ans-more">
                            Showing the first {fmtBytes(shown)} of {fmtBytes(v.length)}.
                            <button class="ans-mini" onclick={() => reveal(k)}>
                              Show {fmtBytes(Math.min(REVEAL_STEP, v.length - shown))} more
                            </button>
                          </div>
                        {/if}
                      </div>
                    {/each}
                  </div>
                </td>
              </tr>
            {/if}
          {/each}

          {#if win.padBottom > 0}
            <tr class="ans-pad" aria-hidden="true" style="height:{win.padBottom}px"><td colspan={cols}></td></tr>
          {/if}
        </tbody>
      </table>
    </div>
  {/if}

  <!-- ── the status bar ─────────────────────────────────────────────── -->
  <div class="ans-foot flex flex-none flex-wrap items-center gap-x-3 gap-y-1 px-5 py-2 text-xs">
    {#if saving}
      <div class="ans-progress" role="progressbar" aria-valuenow={savePct} aria-valuemin={0} aria-valuemax={100}>
        <span style="width:{Math.max(2, savePct)}%"></span>
      </div>
      <span class="num text-ink-3">{fmtBytes(savedBytes)} written</span>
      <button class="ans-mini" onclick={() => (cancelRequested = true)}>Cancel</button>
    {:else if note}
      <span class="min-w-0 flex-1 truncate" style={note.kind === "ok" ? "color: var(--accent-ink);" : "color: var(--status-error-ink);"} title={note.text}>
        {note.text}
      </span>
      <button class="ans-mini" onclick={() => (note = null)}>Dismiss</button>
    {:else}
      <span class="text-ink-5">
        {#if narrowed}
          <span class="num font-semibold text-ink-3">{visible.length.toLocaleString()}</span> of
          <span class="num">{rows.length.toLocaleString()}</span> rows · ≈<span class="num">{fmtBytes(shownBytes)}</span
          > selected
        {:else}
          <span class="num font-semibold text-ink-3">{rows.length.toLocaleString()}</span>
          {rows.length === 1 ? "row" : "rows"} · <span class="num">{keys.length}</span>
          {keys.length === 1 ? "output" : "outputs"}
        {/if}
      </span>
      {#if virtual}
        <span class="ans-sep">·</span>
        <span class="text-ink-6" title="Only the rows under the pointer are drawn, so the table stays the same speed at any size">
          windowed
        </span>
      {/if}
      {#if pane === "json" && jsonShortened}
        <span class="ans-sep">·</span>
        <span class="text-ink-6">
          values over {JSON_CHARS.toLocaleString()} characters shortened for display
        </span>
      {/if}
    {/if}
  </div>
</div>

<style>
  /* One surface, three regions: a header that stays, a body that scrolls,
     a status bar that says what the body is showing. */
  .ans {
    background: var(--surface-1);
  }
  .ans-bar {
    border-bottom: 1px solid var(--edge);
  }
  .ans-tools {
    border-bottom: 1px solid var(--edge);
    background: var(--surface-subtle);
  }
  .ans-expect {
    border-bottom: 1px solid var(--edge);
  }
  .ans-foot {
    border-top: 1px solid var(--edge);
    background: var(--surface-subtle);
    min-height: 33px;
  }
  .ans-blank {
    display: grid;
    align-content: center;
  }
  .ans-sep {
    color: var(--ink-faintest);
    padding: 0 0.4em;
  }

  .ans-mark {
    display: grid;
    place-items: center;
    width: 30px;
    height: 30px;
    flex: none;
    border-radius: var(--radius-md);
    background: var(--hue-sky-wash);
    color: var(--hue-sky-ink);
  }

  /* Segmented: the same control the rest of the product uses, at 26px. */
  .ans-seg {
    display: flex;
    flex: none;
    gap: 2px;
    padding: 3px;
    border-radius: var(--radius-md);
    background: var(--surface-2);
  }
  .ans-seg button {
    border-radius: 5px;
    padding: 0 10px;
    height: 22px;
    font-size: var(--text-xs);
    color: var(--ink-5);
    transition:
      background-color var(--dur-fast) var(--ease),
      color var(--dur-fast) var(--ease);
  }
  .ans-seg button:hover {
    color: var(--ink-2);
  }
  .ans-seg button[aria-selected="true"] {
    background: var(--surface-1);
    color: var(--ink-1);
    font-weight: 600;
    box-shadow: var(--sh-control);
  }

  .ans-acts {
    display: flex;
    flex: none;
    align-items: center;
    gap: 2px;
  }
  .ans-act {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 28px;
    padding: 0 9px;
    border-radius: var(--radius-md);
    font-size: var(--text-xs);
    font-weight: 500;
    color: var(--ink-3);
    transition:
      background-color var(--dur-fast) var(--ease),
      color var(--dur-fast) var(--ease);
  }
  .ans-act:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--ink-1);
  }
  .ans-act:disabled {
    color: var(--ink-disabled);
    cursor: not-allowed;
  }

  .ans-chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 28px;
    padding: 0 10px;
    border: 1px solid var(--edge-strong);
    border-radius: 999px;
    background: var(--surface-1);
    font-size: var(--text-xs);
    color: var(--ink-4);
    transition:
      background-color var(--dur-fast) var(--ease),
      border-color var(--dur-fast) var(--ease),
      color var(--dur-fast) var(--ease);
  }
  .ans-chip:hover:not(:disabled) {
    border-color: var(--edge-hover);
    color: var(--ink-2);
  }
  .ans-chip.is-on {
    background: var(--status-error-wash);
    border-color: color-mix(in srgb, var(--status-error) 45%, transparent);
    color: var(--status-error-ink);
    font-weight: 600;
  }
  .ans-chip:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .ans-mini {
    font-size: 11px;
    color: var(--ink-5);
    border-bottom: 1px solid transparent;
    transition: color var(--dur-fast) var(--ease);
  }
  .ans-mini:hover:not(:disabled) {
    color: var(--accent-ink);
    border-bottom-color: color-mix(in srgb, var(--accent-ink) 40%, transparent);
  }
  .ans-mini:disabled {
    color: var(--ink-disabled);
    cursor: not-allowed;
  }

  /* ── the table ───────────────────────────────────────────────────────
     `table-layout: fixed` and explicit column widths, because a table
     whose columns are negotiated from content re-measures every cell on
     every render — and with a window over a hundred thousand rows the
     columns would then also *move* as you scrolled past a longer value.
     The last output column has no width so it takes the slack. */
  .ans-body {
    overflow: auto;
    overscroll-behavior: contain;
  }
  .ans-table {
    width: 100%;
    table-layout: fixed;
    border-collapse: separate;
    border-spacing: 0;
    font-size: var(--text-sm);
    text-align: left;
  }
  .ans-table th {
    position: sticky;
    top: 0;
    z-index: 2;
    height: 31px;
    padding: 0 12px;
    background: var(--surface-1);
    box-shadow: inset 0 -1px 0 var(--edge);
    font-family: var(--font-sans);
    font-size: var(--text-xs);
    font-weight: 500;
    color: var(--ink-4);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    user-select: none;
  }
  .ans-table th.ta-r,
  .ans-table td.ta-r {
    text-align: right;
  }
  /* Who and how it went stay put when the outputs scroll sideways. The
     hairline appears only once there is something hidden behind them —
     an edge on a table that does not scroll is a column rule nobody asked
     for. */
  .ans-table th.pin {
    z-index: 3;
  }
  .ans-table td.pin {
    position: sticky;
    z-index: 1;
  }
  .ans-table.shifted th.pin-edge {
    box-shadow:
      inset 0 -1px 0 var(--edge),
      6px 0 7px -6px rgba(15, 22, 32, 0.22);
  }
  .ans-table.shifted td.pin-edge {
    box-shadow: 6px 0 7px -6px rgba(15, 22, 32, 0.22);
  }
  .ans-row {
    height: 34px;
  }
  .ans-row > td {
    height: 34px;
    padding: 0 12px;
    background: var(--surface-1);
    border-bottom: 1px solid var(--edge-row);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    color: var(--ink-2);
  }
  .ans-row:hover > td {
    background: var(--surface-2);
  }
  .ans-row.is-cursor > td {
    background: var(--accent-select);
  }
  .ans-row.is-open > td {
    background: var(--accent-select);
    border-bottom-color: transparent;
  }
  .ans-val {
    font-size: var(--text-xs);
    color: var(--ink-3);
  }
  .ans-pad > td {
    padding: 0;
    border: 0;
    background: transparent;
  }

  .ans-node {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    height: 100%;
    text-align: left;
    min-width: 0;
  }
  .ans-caret {
    color: var(--ink-faint);
    display: inline-grid;
    place-items: center;
    transition: transform var(--dur-fast) var(--ease);
  }
  .ans-caret.is-open {
    transform: rotate(90deg);
    color: var(--ink-4);
  }

  /* ── the expansion ─────────────────────────────────────────────────── */
  .ans-detail > td {
    padding: 4px 16px 14px;
    background: var(--accent-select);
    border-bottom: 1px solid var(--edge-row);
  }
  .ans-brief {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr) max-content;
    align-items: baseline;
    gap: 5px 16px;
    padding: 10px 12px;
    border: 1px solid var(--edge);
    border-radius: var(--radius-md);
    background: var(--surface-1);
  }
  .ans-copy {
    display: inline-grid;
    place-items: center;
    width: 20px;
    height: 18px;
    border-radius: 4px;
    color: var(--ink-faint);
    transition:
      color var(--dur-fast) var(--ease),
      background-color var(--dur-fast) var(--ease);
  }
  .ans-copy:hover {
    color: var(--accent-ink);
    background: var(--surface-2);
  }
  .ans-brief-v {
    font-size: var(--text-xs);
    color: var(--ink-2);
    overflow-wrap: anywhere;
  }

  .ans-value {
    border: 1px solid var(--edge);
    border-radius: var(--radius-md);
    background: var(--surface-1);
    overflow: hidden;
  }
  .ans-value-bar {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 5px 10px;
    border-bottom: 1px solid var(--edge-inner);
    background: var(--surface-subtle);
  }
  .ans-pre {
    margin: 0;
    max-height: 18rem;
    overflow: auto;
    padding: 9px 11px;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    line-height: 1.55;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--ink-2);
  }
  .ans-more {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 11px;
    border-top: 1px solid var(--edge-inner);
    font-size: 11px;
    color: var(--ink-5);
  }

  /* ── the JSON pane ─────────────────────────────────────────────────── */
  .ans-json {
    background: var(--surface-1);
  }
  .ans-code {
    margin: 0;
    padding: 0 20px;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    /* Must equal LINE_H: the window's arithmetic is in lines. */
    line-height: 18px;
    white-space: pre;
    color: var(--ink-2);
  }
  /* `{@html}` content carries no scope attribute, so these are reached
     through the scoped parent rather than declared globally. */
  .ans-code :global(.jkey) {
    color: var(--hue-plum-ink);
  }
  .ans-code :global(.jstr) {
    color: var(--hue-green-ink);
  }
  .ans-code :global(.jnum) {
    color: var(--hue-indigo-ink);
  }
  .ans-code :global(.jlit) {
    color: var(--hue-amber-ink);
  }
  .ans-code :global(.jpunct) {
    color: var(--ink-5);
  }

  /* ── the save ──────────────────────────────────────────────────────── */
  .ans-progress {
    position: relative;
    width: 140px;
    height: 5px;
    flex: none;
    border-radius: 999px;
    background: var(--surface-3);
    overflow: hidden;
  }
  .ans-progress span {
    display: block;
    height: 100%;
    border-radius: 999px;
    background: var(--accent-solid);
    transition: width var(--dur-base) var(--ease);
  }
</style>
