<script lang="ts">
  // M17 §3.12 P6 — a terminal on the node drill-in.
  //
  // The panel is deliberately thin. Everything that decides whether this is
  // allowed happens on the *far* node — it re-verifies the signed envelope
  // against its own state, checks the runner grant, and refuses a
  // `no-execute` or draining node (§3.4, §3.9) — so a refusal arrives here
  // as one `error` frame and is rendered as text rather than guessed at.
  //
  // Bytes, not strings, cross the channel: terminal output can split a
  // UTF-8 or escape sequence across two reads, and the emulator is the only
  // thing here that knows how to reassemble one.
  import { onDestroy } from "svelte";
  import { Terminal } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import "@xterm/xterm/css/xterm.css";

  import { api, errText, type TerminalEvent } from "../api";
  import { theme } from "../theme.svelte";

  let {
    /** Member to open a terminal on. */
    node,
    /** Run this instead of the login shell. */
    command = null,
    /** What to show in the header in place of `command`.
        A composed build line is hundreds of characters and belongs in the
        panel above, not across the title bar of the terminal running it. */
    label = null,
    /** Why — recorded in the audit log and covered by the user's signature. */
    reason,
    /** A container instance's job id on `node`: the terminal opens inside
        it (E3) rather than on the machine. */
    job = null,
    /** Called when the session ends, so the parent can close the panel. */
    onexit = () => {},
    /** Open it some other way than a mesh terminal on `node` — the Build
        tab's local run (DIR-8). Resolves to `null` when the person
        declined, which is not an error. */
    open = null,
  }: {
    node: string;
    command?: string | null;
    label?: string | null;
    reason: string;
    job?: string | null;
    onexit?: (code: number | null) => void;
    open?:
      | ((size: { cols: number; rows: number }, onEvent: (ev: TerminalEvent) => void) => Promise<number | null>)
      | null;
  } = $props();

  type Phase = "idle" | "opening" | "live" | "closed";
  let phase = $state<Phase>("idle");
  let error = $state<string | null>(null);
  let exitCode = $state<number | null>(null);
  /** The person said no to running it (`open` resolved `null`). */
  let declined = $state(false);

  let host = $state<HTMLDivElement | null>(null);
  let term: Terminal | null = null;
  let fit: FitAddon | null = null;
  let id: number | null = null;
  let observer: ResizeObserver | null = null;

  /** The emulator's palette follows the app's, so a terminal in a light
      window is not a black rectangle somebody has to squint at. */
  function palette() {
    const css = getComputedStyle(document.documentElement);
    const v = (name: string, fallback: string) =>
      css.getPropertyValue(name).trim() || fallback;
    return {
      background: v("--code", "#12151c"),
      foreground: v("--ink-code-1", "#e6e9ef"),
      cursor: v("--accent-bright", "#7cc4ff"),
      selectionBackground: "rgba(124,196,255,.28)",
    };
  }

  export async function start() {
    if (phase === "opening" || phase === "live") return;
    if (!host) return;
    error = null;
    exitCode = null;
    declined = false;
    phase = "opening";

    term = new Terminal({
      // The family the app actually loads (L-20). This asked for
      // "JetBrains Mono Variable", which nothing imports — so xterm
      // measured its character cell against a fallback and then rendered
      // in one, which is how a terminal ends up with subtly wrong column
      // widths. `--font-mono`'s first entry, spelled out because xterm
      // takes a string rather than a CSS variable.
      fontFamily:
        '"Geist Mono Variable", "Geist Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
      fontSize: 12.5,
      lineHeight: 1.25,
      cursorBlink: true,
      scrollback: 5000,
      theme: palette(),
      // Off: this is a remote shell, and a link the local browser opens is
      // not what someone clicking a path in it expects.
      allowProposedApi: false,
    });
    fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    fit.fit();

    term.onData((text) => {
      if (id !== null) void api.terminalSend(id, text);
    });

    observer = new ResizeObserver(() => {
      if (!fit || !term) return;
      try {
        fit.fit();
      } catch {
        // A panel mid-transition can report a zero size; the next tick is
        // the real one.
        return;
      }
      if (id !== null) void api.terminalResize(id, term.cols, term.rows);
    });
    observer.observe(host);

    try {
      id = open
        ? await open({ cols: term.cols, rows: term.rows }, onFrame)
        : await api.terminalOpen(
            { node, command, reason, cols: term.cols, rows: term.rows, job },
            onFrame,
          );
      if (id === null) {
        stop();
        declined = true;
        onexit(null);
      }
    } catch (e) {
      error = errText(e);
      phase = "closed";
    }
  }

  /** Decode a base64 frame into the bytes xterm wants (M-20). */
  function b64ToBytes(b64: string): Uint8Array {
    const bin = atob(b64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  function onFrame(ev: TerminalEvent) {
    if (ev.type === "opened") {
      phase = "live";
      term?.focus();
    } else if (ev.type === "data") {
      // Base64 across the bridge (M-20): a `Vec<u8>` arrived as a JSON
      // array of decimal numbers, roughly four bytes of JSON per byte of
      // output and a large numeric-array parse per frame.
      term?.write(b64ToBytes(ev.b64));
    } else if (ev.type === "exit") {
      exitCode = ev.code;
      phase = "closed";
      onexit(ev.code);
    } else if (ev.type === "error") {
      error = ev.message;
      phase = "closed";
      onexit(null);
    }
  }

  export function stop() {
    if (id !== null) {
      void api.terminalClose(id);
      id = null;
    }
    observer?.disconnect();
    observer = null;
    term?.dispose();
    term = null;
    fit = null;
    phase = "closed";
  }

  // Open as soon as the element exists.
  //
  // This was the bug behind the blank panel: `start()` was exported and
  // nothing ever called it, so the component mounted in `idle`, drew an
  // empty box, and the header's final `{:else}` branch reported "closed"
  // about a session that had never been opened. Nothing was wrong on the
  // far side — nothing had been asked of it.
  $effect(() => {
    if (host && phase === "idle") void start();
  });

  // A live shell is a process on somebody's machine. Leaving the view has to
  // end it — the far side kills the whole process group when the stream
  // closes, exactly as a job cancel does.
  onDestroy(stop);

  // Re-theme in place rather than tearing the session down: the shell is
  // still running, and a colour change is not a reason to lose it.
  $effect(() => {
    void theme.mode;
    if (term) term.options.theme = palette();
  });
</script>

<div class="on-code overflow-hidden rounded-xl bg-code" style="box-shadow: var(--sh-code);">
  <div class="flex h-10 items-center gap-2.5 border-b border-edge-code px-4">
    <span
      class="h-[6px] w-[6px] rounded-full {phase === 'live' ? 'breathe' : ''}"
      style={phase === "live"
        ? "background: var(--accent-bright);"
        : phase === "opening"
          ? "background: var(--status-warn);"
          : "background: var(--ink-code-5);"}
    ></span>
    <span class="mono text-xs text-inkcode-2">{node}</span>
    {#if label ?? command}
      <span class="mono max-w-[46ch] truncate text-2xs text-inkcode-4" title={command ?? undefined}
        >{label ?? command}</span
      >
    {/if}
    <span class="truncate text-2xs text-inkcode-4">{reason}</span>
    <span class="ml-auto text-2xs text-inkcode-4">
      {#if phase === "idle" || phase === "opening"}
        connecting…
      {:else if phase === "live"}
        connected
      {:else if exitCode !== null}
        exit {exitCode}
      {:else if declined}
        not run
      {:else}
        closed
      {/if}
    </span>
    {#if phase === "live"}
      <button
        class="h-6 rounded-sm border border-edge-code px-2 text-2xs font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]"
        onclick={stop}
      >
        Close
      </button>
    {:else if phase === "closed"}
      <button
        class="h-6 rounded-sm border border-edge-code px-2 text-2xs font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]"
        onclick={start}
      >
        {open ? "Run again" : "Reconnect"}
      </button>
    {/if}
  </div>

  {#if error}
    <!-- A refusal is the far node's sentence, verbatim: it already names the
         grant, the tag or the version that stopped this, and paraphrasing
         it here would lose the only actionable half. -->
    <div class="selectable px-4 py-3 text-sm" style="color: var(--status-error-on-code);">
      {error}
    </div>
  {/if}

  {#if declined}
    <div class="px-4 py-3 text-sm text-inkcode-3">Not run — it was not confirmed.</div>
  {/if}

  <div bind:this={host} class="h-[340px] px-2 py-2" class:hidden={error !== null || declined}></div>
</div>

<style>
  /* xterm sizes itself to its container; without a definite height the fit
     addon computes zero rows and draws nothing. */
  div :global(.xterm) {
    height: 100%;
  }
  div :global(.xterm-viewport)::-webkit-scrollbar {
    width: 8px;
  }
  div :global(.xterm-viewport)::-webkit-scrollbar-thumb {
    background: rgba(160, 176, 200, 0.25);
    border-radius: 4px;
  }
</style>
