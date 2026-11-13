<script module lang="ts">
  // The live cards: one machine, one card, every step it goes through.
  //
  // Both sheets that drive machines over SSH — Add machines and Update
  // over SSH — read the same event stream (`DeployEvent`) and draw the
  // same cards, so a person learns the shape once. The reducer is
  // exported with the type: the sheet owns the array, this component
  // draws it.
  import type { DeployEvent } from "../api";

  export type Card = {
    /** The key: what the person typed for this machine. */
    host: string;
    /** What the card is titled. */
    label: string;
    stage: string;
    detail: string;
    fingerprint?: string;
    keyStatus?: string;
    hostname?: string;
    distro?: string;
    arch?: string;
    scope?: string;
    note?: string | null;
    code?: { ours: string; theirs: string; matched: boolean; nodeName: string; at: number };
    /** A machine Director has never seen, asking about its key (S-M9).
     *  Set before anything is authenticated — declining sends nothing. */
    keyPrompt?: { fingerprint: string; algorithm: string; at: number };
    decided?: boolean;
    done?: { nodeName: string; nodeId: string; version: string; service: string; note: string | null };
    failed?: { stage: string; message: string; hint: string | null };
  };

  export function newCard(host: string, label: string): Card {
    return { host, label, stage: "queued", detail: "waiting its turn" };
  }

  /** Fold one event into the cards. Pure, so the sheet can keep `cards` in `$state`. */
  export function applyEvent(cards: Card[], ev: DeployEvent): Card[] {
    const patch = (host: string, f: (c: Card) => Card) =>
      cards.map((c) => (c.host === host ? f(c) : c));
    switch (ev.type) {
      case "plan":
        return ev.hosts.map((h) => cards.find((c) => c.host === h) ?? newCard(h, h));
      case "stage":
        return patch(ev.host, (c) => ({ ...c, stage: ev.stage, detail: ev.detail }));
      case "hostKey":
        return patch(ev.host, (c) => ({
          ...c,
          fingerprint: ev.fingerprint,
          keyStatus: ev.status,
          keyPrompt: undefined,
        }));
      case "hostKeyPrompt":
        return patch(ev.host, (c) => ({
          ...c,
          stage: "confirming",
          keyPrompt: { fingerprint: ev.fingerprint, algorithm: ev.algorithm, at: Date.now() },
          decided: false,
        }));
      case "machine":
        return patch(ev.host, (c) => ({
          ...c,
          hostname: ev.hostname,
          distro: ev.distro,
          arch: ev.arch,
          scope: ev.scope,
          note: ev.note,
        }));
      case "code":
        return patch(ev.host, (c) => ({
          ...c,
          stage: ev.matched ? "confirming" : "failed",
          code: { ours: ev.ours, theirs: ev.theirs, matched: ev.matched, nodeName: ev.nodeName, at: Date.now() },
          decided: false,
        }));
      case "done":
        return patch(ev.host, (c) => ({
          ...c,
          stage: "done",
          detail: "",
          done: { nodeName: ev.nodeName, nodeId: ev.nodeId, version: ev.version, service: ev.service, note: ev.note },
        }));
      case "failed":
        return patch(ev.host, (c) => ({
          ...c,
          stage: "failed",
          failed: { stage: ev.stage, message: ev.message, hint: ev.hint },
        }));
      default:
        return cards;
    }
  }

  export const STAGE_LABEL: Record<string, string> = {
    queued: "Waiting its turn",
    connecting: "Connecting",
    inspecting: "Looking at the machine",
    preparing: "Choosing a build",
    installing: "Installing",
    configuring: "Setting up the service",
    pairing: "Pairing",
    confirming: "Needs you",
    joining: "Joining the mesh",
    starting: "Starting",
    verifying: "Waiting for it to report in",
    done: "Done",
    failed: "Failed",
  };

  /** Cards waiting on a click: a matching pairing code, or a new machine's
   *  host key. Both are the same question — "is this the machine you meant?"
   *  — asked at the two moments it can be answered. */
  export function awaiting(cards: Card[]): Card[] {
    return cards.filter(
      (c) => c.stage === "confirming" && !c.decided && (c.code?.matched || c.keyPrompt),
    );
  }
</script>

<script lang="ts">
  import { fmtCountdown } from "../state.svelte";
  import Icon from "./Icon.svelte";
  import Spinner from "./Spinner.svelte";

  let {
    cards,
    mode = "deploy",
    onDecide,
  }: {
    cards: Card[];
    /** `deploy` says "joined"; `upgrade` says "updated". */
    mode?: "deploy" | "upgrade";
    onDecide: (host: string, accept: boolean) => void;
  } = $props();

  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
  /** A pairing code lives five minutes; the sheet stops asking at ~280s. */
  const ttl = (c: Card) => (c.code ? 280 - Math.floor((now - c.code.at) / 1000) : 0);
  /** A host-key question waits `HOST_KEY_DECISION` (180s) and then declines. */
  const keyTtl = (c: Card) => (c.keyPrompt ? 180 - Math.floor((now - c.keyPrompt.at) / 1000) : 0);

  const grouped = (code: string) => (code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code);

  function tone(c: Card): string {
    if (c.stage === "done") return "var(--status-ok)";
    if (c.stage === "failed") return "var(--status-error)";
    if (c.stage === "confirming") return "var(--status-warn)";
    if (c.stage === "queued") return "var(--ink-faint)";
    return "var(--status-running)";
  }
</script>

<div class="space-y-2">
  {#each cards as c (c.host)}
    <div
      class="fade-in rounded-lg px-4 py-3"
      style={`background: var(--surface-1); box-shadow: 0 0 0 1px ${c.stage === "confirming" ? "var(--status-warn-ring)" : c.stage === "failed" ? "var(--status-error-edge)" : "var(--edge-card)"};`}
    >
      <div class="flex items-start gap-3">
        <!-- status glyph -->
        <div class="mt-[3px] grid h-5 w-5 shrink-0 place-items-center">
          {#if c.stage === "done"}
            <span class="grid h-5 w-5 place-items-center rounded-full" style="background: var(--status-ok-wash); color: var(--status-ok-ink);">
              <Icon name="check" size={12} />
            </span>
          {:else if c.stage === "failed"}
            <span class="grid h-5 w-5 place-items-center rounded-full" style="background: var(--status-error-wash); color: var(--status-error-ink);">
              <Icon name="close" size={12} />
            </span>
          {:else if c.stage === "queued"}
            <span class="h-2 w-2 rounded-full" style="background: var(--ink-faint);"></span>
          {:else if c.stage === "confirming"}
            <span class="h-2.5 w-2.5 rounded-full" style="background: var(--status-warn); animation: mw-breathe var(--motion-breathe);"></span>
          {:else}
            <Spinner size={16} color="var(--status-running)" track="color-mix(in srgb, var(--status-running) 22%, transparent)" />
          {/if}
        </div>

        <div class="min-w-0 flex-1">
          <!-- title row -->
          <div class="flex flex-wrap items-baseline gap-x-2 gap-y-1">
            <span class="mono truncate text-[13.5px] font-semibold text-ink">{c.label}</span>
            {#if c.hostname && c.hostname !== c.label}
              <span class="truncate text-xs text-ink-4">{c.hostname}</span>
            {/if}
            {#if c.distro}
              <span class="truncate text-xs text-ink-5">{c.distro}</span>
            {/if}
            {#if c.arch}
              <span class="mono rounded-full bg-surface-2 px-1.5 py-px text-2xs font-semibold text-ink-4">{c.arch}</span>
            {/if}
            {#if c.scope === "user"}
              <span class="rounded-full px-1.5 py-px text-2xs font-semibold" style="background: var(--status-warn-wash); color: var(--status-warn-ink);" title="No root there: the node lives in the account's home and runs as that account">home install</span>
            {/if}
          </div>

          <!-- stage line -->
          {#if c.stage !== "done" && c.stage !== "failed"}
            <div class="mt-0.5 flex items-baseline gap-2 text-sm">
              <span class="font-medium" style={`color: ${tone(c)};`}>{STAGE_LABEL[c.stage] ?? c.stage}</span>
              {#if c.detail}<span class="truncate text-ink-5">{c.detail}</span>{/if}
            </div>
          {/if}

          <!-- fingerprint -->
          {#if c.fingerprint}
            <div class="mt-1 flex items-center gap-2 text-2xs text-ink-5">
              <span class="mono truncate" title="The machine's SSH host key">{c.fingerprint}</span>
              {#if c.keyStatus === "new"}
                <span class="shrink-0 rounded-full px-1.5 py-px font-semibold" style="background: var(--tint-blue-bg); color: var(--tint-blue-fg);" title="First time Director has seen this machine; its key is now on record">new machine</span>
              {:else if c.keyStatus === "known"}
                <span class="shrink-0 rounded-full bg-surface-2 px-1.5 py-px font-semibold text-ink-5">known</span>
              {/if}
            </div>
          {/if}

          <!-- a machine nobody here has met (S-M9) -->
          {#if c.keyPrompt && c.stage === "confirming" && !c.decided}
            <div
              class="pop-in mt-3 rounded-xl px-4 pt-3.5 pb-4"
              style="background: var(--surface-2); box-shadow: 0 0 0 1px var(--status-warn-ring);"
            >
              <div class="flex items-center gap-2">
                <span class="mono text-2xs tracking-[0.1em] uppercase" style="color: var(--status-warn-ink);">
                  first time here
                </span>
                <span class="mono ml-auto text-2xs text-ink-5">
                  {keyTtl(c) > 0 ? `expires in ${fmtCountdown(keyTtl(c))}` : "expired"}
                </span>
              </div>
              <div class="mono mt-2.5 break-all text-[13px] text-ink">{c.keyPrompt.fingerprint}</div>
              <div class="mt-0.5 text-2xs text-ink-5">{c.keyPrompt.algorithm}</div>
              <p class="mt-2.5 mb-0 text-xs leading-relaxed text-ink-4">
                Director has never talked to {c.label} before, so there is nothing to check this key
                against. Nothing has been sent yet — not the password, not a command. Compare it with
                what the machine itself says
                (<span class="mono">ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub</span>), or
                accept it if you know the network you are on. It is recorded, and every later
                connection is checked against it.
              </p>
              <div class="mt-3 grid gap-2" style="grid-template-columns: 1fr 1.4fr;">
                <button
                  type="button"
                  class="btn btn-secondary h-9 text-sm"
                  onclick={() => onDecide(c.host, false)}
                >
                  Stop
                </button>
                <button type="button" class="btn btn-primary h-9 text-sm" onclick={() => onDecide(c.host, true)}>
                  Trust this machine
                </button>
              </div>
            </div>
          {/if}

          <!-- the trust moment -->
          {#if c.code && c.stage === "confirming" && !c.decided}
            <div
              class="on-code pop-in mt-3 rounded-xl px-4 pt-3.5 pb-4"
              style="background: linear-gradient(165deg, var(--surface-code-2) 0%, var(--surface-code) 100%); box-shadow: var(--sh-code); color: var(--ink-code-1);"
            >
              <div class="flex items-center gap-2">
                <span class="mono text-2xs tracking-[0.1em] uppercase" style="color: var(--accent-bright);">do these codes match?</span>
                <span class="mono ml-auto text-2xs" style={`color: ${ttl(c) <= 20 ? "var(--status-warn)" : "var(--ink-code-4)"};`}>
                  {ttl(c) > 0 ? `expires in ${fmtCountdown(ttl(c))}` : "expired"}
                </span>
              </div>
              <div class="mt-3 grid gap-3 sm:grid-cols-2">
                <div class="rounded-lg px-3 py-2.5" style="background: rgba(94,234,212,.06); border: 1px solid rgba(94,234,212,.18);">
                  <div class="text-2xs" style="color: var(--ink-code-3);">this Director derived</div>
                  <div class="mono mt-1 text-[26px] font-semibold tracking-[.12em]" style="color: var(--brand-mark); font-variant-numeric: tabular-nums;">{grouped(c.code.ours)}</div>
                </div>
                <div class="rounded-lg px-3 py-2.5" style="background: rgba(94,234,212,.06); border: 1px solid rgba(94,234,212,.18);">
                  <div class="truncate text-2xs" style="color: var(--ink-code-3);">{c.code.nodeName || c.hostname || c.label} printed over SSH</div>
                  <div class="mono mt-1 text-[26px] font-semibold tracking-[.12em]" style="color: var(--brand-mark); font-variant-numeric: tabular-nums;">{grouped(c.code.theirs)}</div>
                </div>
              </div>
              <p class="mt-2.5 mb-0 text-xs leading-relaxed" style="color: var(--ink-code-3);">
                Director read both and they agree. Your confirmation is what joins it — the same six digits, seen twice, on a channel nobody in the middle of the pairing controls.
              </p>
              <div class="mt-3 grid gap-2" style="grid-template-columns: 1fr 1.4fr;">
                <button
                  type="button"
                  class="h-9 rounded-md text-sm font-semibold transition-colors"
                  style="border: 1px solid rgba(224,82,82,.35); color: #f2a3a3; background: transparent;"
                  onclick={() => onDecide(c.host, false)}
                >
                  Reject
                </button>
                <button type="button" class="btn btn-bright h-9 text-sm" onclick={() => onDecide(c.host, true)}>
                  They match — confirm
                </button>
              </div>
            </div>
          {:else if c.code && c.decided && c.stage !== "done" && c.stage !== "failed"}
            <div class="mt-1.5 text-xs text-ink-5">code confirmed · <span class="mono">{grouped(c.code.ours)}</span></div>
          {/if}

          <!-- outcome -->
          {#if c.done}
            <div class="mt-1 text-sm text-ink-2">
              {#if mode === "deploy"}
                Joined as <span class="font-semibold text-ink">{c.done.nodeName}</span>
              {:else}
                <span class="font-semibold text-ink">{c.done.nodeName}</span> updated
              {/if}
              {#if c.done.version}· meshnode <span class="mono">{c.done.version}</span>{/if}
              {#if c.done.service && c.done.service !== "none"}
                · <span class="mono text-ink-4">{c.done.service}</span>
              {/if}
            </div>
            {#if c.done.note}
              <div class="mt-1 text-xs leading-relaxed" style="color: var(--status-warn-ink);">{c.done.note}</div>
            {/if}
          {:else if c.failed}
            <div class="mt-1 text-sm leading-relaxed" style="color: var(--status-error-ink);">
              <span class="text-xs font-semibold uppercase tracking-wide opacity-80">{c.failed.stage}</span>
              · {c.failed.message}
            </div>
            {#if c.failed.hint}
              <div class="mt-1 text-xs leading-relaxed text-ink-4">{c.failed.hint}</div>
            {/if}
          {:else if c.note}
            <div class="mt-1 text-xs leading-relaxed text-ink-5">{c.note}</div>
          {/if}
        </div>
      </div>
    </div>
  {/each}
</div>
