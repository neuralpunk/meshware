<script lang="ts">
  // First-run. The only screen every customer sees exactly once, at the
  // exact moment they decide whether the thing they paid for looks like
  // what they paid for.
  //
  // The 2026-08 build opened with a teal square with a dot in it — not the
  // brand mark — above four identical white cards on flat grey. This one
  // opens with the mark over a mesh of dots moving as one surface
  // (`MeshWave`), and the four ways in as a 2×2 grid, each with a glyph and
  // the digit that picks it. The pairing sequence — the trust moment — is
  // unchanged: it goes dark, with the same code-digit treatment as the
  // approve modal on the other side.
  //
  // The wave is the one decorative element on this screen and it is told
  // to get out of the way the moment a form opens (`quiet`); the Benchmark
  // hero still carries the static dot-field, which is also what this falls
  // back to where the webview has no WebGL.
  import { api, errText, type WizardPairEvent, type DemoEvent } from "../lib/api";
  import { app, fmtCountdown } from "../lib/state.svelte";
  import PairCode from "../lib/components/PairCode.svelte";
  import BrandMark from "../lib/components/BrandMark.svelte";
  import MeshWave from "../lib/components/MeshWave.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import { reveal, staggerIn, DUR } from "../lib/motion";
  import { untrack } from "svelte";
  import { hueStyle, type Hue } from "../lib/hues";

  type Path = "choose" | "demo" | "create" | "token" | "pair" | "restore";
  let path = $state<Path>("choose");
  let busy = $state(false);
  let error = $state<string | null>(null);

  let meshName = $state("");
  let nodeName = $state("");
  /**
   * What to call the *person*, as against the machine.
   *
   * Joining used to ask only for a node name, and then — once you were in
   * — tell you the mesh did not know your key, leaving you to work out that
   * a machine joining and a person being recognised are different things.
   * They still are, and they should be: a machine can be a member with
   * nobody sitting at it. But asking here means the admin on the other side
   * sees a name rather than `mia-laptop`, and the second half happens on
   * its own (`app.requestAccessIfNeeded`).
   */
  let personName = $state("");
  let token = $state("");
  let via = $state("");
  let contribute = $state(true);
  // The backup the person picked: a token for the Rust side, a path to show
  // (SEC-14). There is no typing one in — a path the window names is not
  // something the backend reads.
  let backup = $state<{ token: string; path: string } | null>(null);

  // The fifth path (L1 P3). Its whole job is that a person with one
  // computer can see the product work, so it asks for nothing: no name, no
  // token, no second machine. It reports progress because forty seconds of
  // spinner is forty seconds of wondering whether it hung.
  let demoStep = $state("");
  let demoDone = $state(0);
  let demoTotal = $state(0);
  let demoRunning = $state(false);

  type PairPhase = "idle" | "waiting" | "code" | "confirming" | "joined";
  let pairPhase = $state<PairPhase>("idle");
  /** A Cancel (or a Back) is on its way to the engine (DIR-6). */
  let pairCancelling = $state(false);
  /** Which pairing the events on screen belong to. Bumped by a cancel, so
   *  a code forwarded just before it cannot bring the dead one back. */
  let pairGen = 0;
  let pairAddr = $state("");
  /** Every address this machine can be dialled at, for an admin to type. */
  let pairAddrs = $state<string[]>([]);
  /** Why mDNS is not announcing, if it is not (the beacon still is). */
  let pairMdnsError = $state<string | null>(null);
  let pairCode = $state("");
  let pairMesh = $state("");
  let pairApprover = $state("");
  let codeAt = $state(0);

  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });
  const codeTtl = $derived(pairPhase === "code" ? 300 - Math.floor((now - codeAt) / 1000) : 300);

  // Seed the fields from the machine, ONCE.
  //
  // This used to be an effect that read the fields it was filling, so it
  // re-ran whenever one of them changed — and its "if empty, use the
  // hostname" rule fired the moment you backspaced the last character of
  // your name. You could not clear the field: it refilled itself with the
  // hostname on every keystroke that emptied it. The defaults are a
  // starting point, not a floor; they are written when the machine's facts
  // arrive and never again.
  let seeded = false;
  $effect(() => {
    const boot = app.boot;
    if (!boot || seeded) return;
    seeded = true;
    untrack(() => {
      if (!nodeName) nodeName = boot.hostname;
      if (!meshName) meshName = `${boot.hostname}-mesh`;
      // `mia-laptop` → `mia`: a decent first guess at a person's name from
      // the way people actually name their machines, and trivially edited
      // when it is wrong.
      if (!personName) personName = boot.hostname.split(/[-._]/)[0] ?? boot.hostname;
    });
  });

  const canContribute = $derived(!!app.boot);

  // One hue per way in (`lib/hues.ts`): founding a mesh is the brand's own
  // teal, pairing on the LAN is indigo, an invite is coral, a backup is
  // plum — and the demo, above them all, is amber. Four cards in four
  // colours read as four different things; four in one colour read as a
  // list.
  const PATHS: { id: Path; key: string; icon: string; hue: Hue; title: string; sub: string }[] = [
    {
      id: "create",
      key: "2",
      icon: "plus",
      hue: "teal",
      title: "Create a new mesh",
      sub: "This machine founds it; you become its first admin. Identity and encryption are automatic.",
    },
    {
      id: "pair",
      key: "3",
      icon: "pair",
      hue: "indigo",
      title: "Join a mesh on this network",
      sub: "You appear on their Access page within seconds; they let you in with a 6-digit code you both check.",
    },
    {
      id: "token",
      key: "4",
      icon: "link",
      hue: "coral",
      title: "Join with an invite",
      sub: "For a mesh you cannot see from here. Paste the invite an admin sent you.",
    },
    {
      id: "restore",
      key: "5",
      icon: "snapshot",
      hue: "plum",
      title: "Restore from a backup",
      sub: "Bring a machine back from a backup file (.mwb): its identity and the mesh's whole history.",
    },
  ];

  // Offered only where it will work: Linux, with a meshnode this machine
  // can run the other machines with. A card that fails when a stranger
  // picks it is worse than a card that was never there.
  const canDemo = $derived(app.boot?.demoAvailable === true);

  function onKey(e: KeyboardEvent) {
    if (path !== "choose" || e.metaKey || e.ctrlKey || e.altKey) return;
    const target = e.target as HTMLElement | null;
    if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;
    if (e.key === "1" && canDemo) {
      void startDemo();
      return;
    }
    const hit = PATHS.find((p) => p.key === e.key);
    if (hit) path = hit.id;
  }

  async function startDemo() {
    if (demoRunning) return;
    error = null;
    path = "demo";
    demoRunning = true;
    demoStep = "Starting…";
    demoDone = 0;
    demoTotal = 0;
    try {
      await api.demoStart(3, (ev: DemoEvent) => {
        if (ev.type === "step") {
          demoStep = ev.message;
          demoDone = ev.done;
          demoTotal = ev.total;
        } else if (ev.type === "ready") {
          demoStep = "Ready";
          demoDone = demoTotal;
          demoRunning = false;
          void app.onboarded();
        } else if (ev.type === "failed") {
          error = ev.message;
          demoRunning = false;
          path = "choose";
        }
      });
    } catch (e) {
      error = errText(e);
      demoRunning = false;
      path = "choose";
    }
  }

  async function doCreate() {
    busy = true;
    error = null;
    try {
      await api.createMesh(meshName.trim(), nodeName.trim(), contribute);
      await app.onboarded();
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }

  async function doJoinToken() {
    busy = true;
    error = null;
    try {
      const viaList = via.split(",").map((v) => v.trim()).filter(Boolean);
      await api.joinToken(token.trim(), nodeName.trim(), contribute, viaList);
      await app.onboarded(personName);
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }

  async function pickBackup() {
    try {
      // `mwbak` too: the Mesh screen's backup button wrote that extension
      // until UX-15, and a file somebody already has must still restore.
      const p = await api.pickFile("Choose a meshware backup", ["mwb", "mwbak"]);
      if (p) backup = { token: p.token, path: p.path };
    } catch (e) {
      error = errText(e);
    }
  }

  async function doRestore() {
    busy = true;
    error = null;
    try {
      if (!backup) return;
      const r = await api.restoreBackup(backup.token, nodeName.trim(), contribute);
      app.flash(`Restored ${r.meshName} — ${r.members} machine${r.members === 1 ? "" : "s"}, ${r.ops.toLocaleString()} changes`);
      await app.onboarded(personName);
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }

  async function startPairing() {
    error = null;
    pairPhase = "waiting";
    const gen = ++pairGen;
    try {
      await api.wizardPairStart(nodeName.trim(), contribute, personName.trim() || null, (ev: WizardPairEvent) => {
        // A pairing given up on says nothing more — except `joined`: one
        // that completed before the cancel reached it is a mesh this
        // machine is now in, and the wizard cannot pretend otherwise.
        if (gen !== pairGen && ev.type !== "joined") return;
        if (ev.type === "listening") {
          pairAddr = ev.addr;
          pairAddrs = ev.addrs;
          pairMdnsError = ev.mdnsError;
        } else if (ev.type === "code") {
          pairCode = ev.code;
          pairMesh = ev.meshName;
          pairApprover = ev.approverName;
          codeAt = Date.now();
          pairPhase = "code";
        } else if (ev.type === "joined") {
          pairPhase = "joined";
          setTimeout(() => void app.onboarded(personName), 800);
        } else if (ev.type === "failed") {
          error = ev.message;
          pairPhase = "idle";
        } else if (ev.type === "cancelled") {
          // Their own doing: back to the form, and nothing to apologise for.
          pairPhase = "idle";
        }
      });
    } catch (e) {
      error = errText(e);
      pairPhase = "idle";
    }
  }

  /**
   * Stop waiting to be let in (DIR-6).
   *
   * The wait lasts ten minutes, and this screen tells people who picked it
   * by mistake to go back and use an invite — which Back used to refuse
   * while it ran, with no Cancel beside it. The engine stops announcing and
   * lets go of its port before this returns, so the next way in does not
   * race it. A pairing that finished first stays finished: `joined` still
   * arrives and carries on into the mesh.
   */
  async function cancelPairing(): Promise<boolean> {
    if (pairCancelling) return false;
    pairCancelling = true;
    pairGen++;
    try {
      await api.wizardPairCancel();
      if (pairPhase !== "joined") pairPhase = "idle";
      return pairPhase === "idle";
    } catch (e) {
      error = errText(e);
      return false;
    } finally {
      pairCancelling = false;
    }
  }

  async function back() {
    if (path === "demo" && demoRunning) return;
    if (path === "pair" && pairPhase !== "idle") {
      if (pairPhase === "joined" || !(await cancelPairing())) return;
    }
    path = "choose";
    error = null;
  }

  async function decide(accept: boolean) {
    pairPhase = accept ? "confirming" : "idle";
    try {
      await api.wizardPairDecide(accept);
      if (!accept) pairPhase = "idle";
    } catch (e) {
      error = errText(e);
      pairPhase = "idle";
    }
  }

  async function copyKey() {
    if (!app.boot) return;
    try {
      await api.copyText(app.boot.userId);
      app.flash("Your user key is copied");
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }

  const inputBox = "field h-10 w-full px-3 text-base outline-none";
  const formTitle: Record<Exclude<Path, "choose">, string> = {
    demo: "Just show me",
    create: "Create a new mesh",
    pair: "Join a mesh on this network",
    token: "Join with an invite",
    restore: "Restore from a backup",
  };
</script>

<svelte:window onkeydown={onKey} />

<div class="relative flex h-full items-center justify-center overflow-y-auto p-6" style="background: var(--bg-canvas);">
  <!-- A wash of the companion hues under the wave — indigo one corner,
       coral the other, amber high — so the first screen is not teal on a
       flat field. Static, and behind everything; the wave is still the one
       thing that moves. -->
  <div class="wizard-aurora" class:quiet={path !== "choose"} aria-hidden="true"></div>
  <MeshWave quiet={path !== "choose"} />

  <div class="relative w-[640px] max-w-[94vw]" use:reveal={{ y: 10, duration: DUR.slower }}>
    <div class="mb-8 text-center">
      <div class="mx-auto mb-5 w-fit" style="filter: drop-shadow(0 12px 24px rgba(0, 117, 103, .28));">
        <BrandMark size={56} />
      </div>
      <h1 class="m-0 text-[28px] leading-tight font-bold tracking-[-.025em] text-ink">Welcome to Meshware</h1>
      <!-- The product line, word for word (UX-19; GO-TO-MARKET's "feeling"
           line): the first sentence a stranger reads should be the one the
           site and the pitch open with. -->
      <div class="mt-2 text-base text-ink-4">You have machines. Pair them. Now you have a computer.</div>
    </div>

    {#if error}
      <div class="errbox mb-4">{error}</div>
    {/if}

    {#if path === "choose"}
      <div class="grid grid-cols-2 gap-3" use:staggerIn={{ selector: ".path", each: 40, y: 6 }}>
        <!-- The stranger's card, first and across both columns. Every other
             way in assumes a mesh, or a second machine to make one with;
             this one assumes a laptop (L1 P3). -->
        {#if canDemo}
          <button class="path card tile relative col-span-2 flex items-center gap-4 p-5 text-left" style={hueStyle("amber")} onclick={startDemo}>
            <span class="kbd absolute top-3 right-3">1</span>
            <span
              class="grid h-9 w-9 flex-none place-items-center rounded-lg"
              style="background: var(--hue-wash); color: var(--hue-ink);"
            >
              <Icon name="play" size={18} />
            </span>
            <span class="block">
              <span class="block text-[15px] font-semibold tracking-[-.01em] text-ink">Just show me</span>
              <span class="mt-1 block text-sm leading-normal text-ink-4">
                No second machine needed. This one founds a small mesh with itself, runs a command
                across all of it, and removes every trace when you stop. Real nodes, not a preview.
              </span>
            </span>
          </button>
        {/if}
        {#each PATHS as p (p.id)}
          <button class="path card tile relative flex flex-col gap-3 p-5 text-left" style={hueStyle(p.hue)} onclick={() => (path = p.id)}>
            <span class="kbd absolute top-3 right-3">{p.key}</span>
            <span
              class="grid h-9 w-9 place-items-center rounded-lg"
              style="background: var(--hue-wash); color: var(--hue-ink);"
            >
              <Icon name={p.icon} size={18} />
            </span>
            <span class="block">
              <span class="block text-[15px] font-semibold tracking-[-.01em] text-ink">{p.title}</span>
              <span class="mt-1 block text-sm leading-normal text-ink-4">{p.sub}</span>
            </span>
          </button>
        {/each}
      </div>
    {:else}
      <div class="card p-6" use:reveal={{ y: 6 }}>
        <div class="mb-5 flex items-center gap-3">
          <button
            class="iconbtn h-7 w-7 -ml-1"
            title={path === "pair" && pairPhase !== "idle" ? "Stop waiting and go back to the ways in" : "Back to the ways in"}
            disabled={pairCancelling || pairPhase === "joined" || (path === "demo" && demoRunning)}
            onclick={back}
          >
            <Icon name="chevron-left" size={16} />
          </button>
          <div class="text-md font-semibold tracking-[-.01em] text-ink">{formTitle[path]}</div>
          <span
            class="ml-auto h-2 w-2 rounded-full"
            style={`${hueStyle(PATHS.find((p) => p.id === path)?.hue ?? "amber")} background: var(--hue);`}
            aria-hidden="true"
          ></span>
        </div>

        {#if path === "demo"}
          <div class="space-y-4">
            <div class="text-sm leading-relaxed text-ink-4">
              Founding a mesh of three machines on this one, then running something across all of
              them. They are real <span class="mono">meshnode</span> daemons on
              <span class="mono">127.0.0.1</span> — nothing is announced on your network, and
              nothing outside a scratch directory is touched.
            </div>
            <div>
              <div class="flex items-baseline justify-between">
                <span class="text-base font-medium text-ink">{demoStep}</span>
                {#if demoTotal > 0}
                  <span class="mono text-xs text-ink-5">{demoDone} of {demoTotal}</span>
                {/if}
              </div>
              <div class="mt-2 h-1.5 w-full overflow-hidden rounded-full" style="background: var(--surface-subtle);">
                <div
                  class="h-full rounded-full transition-[width] duration-500"
                  style="width: {demoTotal > 0 ? Math.round((demoDone / demoTotal) * 100) : 8}%; background: var(--accent);"
                ></div>
              </div>
            </div>
            <div class="text-xs leading-normal text-ink-5">
              You can turn this into a real mesh, or throw it away, at any time — the banner at the
              top of the app offers both.
            </div>
          </div>
        {:else if path === "create"}
          <div class="space-y-4">
            <label class="block">
              <span class="mb-1.5 block text-sm text-ink-4">Mesh name</span>
              <input bind:value={meshName} class={inputBox} />
            </label>
            {@render nodeNameField()}
            {@render contributeField()}
            <div class="flex items-center justify-end pt-1">
              <button class="btn btn-primary btn-lg" disabled={busy || !meshName.trim() || !nodeName.trim()} onclick={doCreate}>
                {busy ? "Creating…" : "Create mesh"}
              </button>
            </div>
          </div>
        {:else if path === "token"}
          <div class="space-y-4">
            <label class="block">
              <span class="mb-1.5 block text-sm text-ink-4">Invite</span>
              <textarea
                bind:value={token}
                rows="4"
                spellcheck="false"
                placeholder="mwj1…"
                class="field mono w-full resize-none px-3 py-2.5 text-sm leading-relaxed outline-none placeholder:font-sans"
              ></textarea>
            </label>
            <label class="block">
              <span class="mb-1.5 block text-sm text-ink-4">
                Dial via
                <span class="text-ink-muted">(optional — host:port to reach instead of the invite's addresses, e.g. over a VPN; comma-separated)</span>
              </span>
              <input bind:value={via} placeholder="100.64.0.5:7777" class="mono {inputBox} placeholder:font-sans" />
            </label>
            {@render personNameField()}
            {@render nodeNameField()}
            {@render contributeField()}
            <div class="flex items-center justify-end pt-1">
              <button class="btn btn-primary btn-lg" disabled={busy || !token.trim() || !nodeName.trim()} onclick={doJoinToken}>
                {busy ? "Joining…" : "Join mesh"}
              </button>
            </div>
          </div>
        {:else if path === "restore"}
          <div class="space-y-4">
            <label class="block">
              <span class="mb-1.5 block text-sm text-ink-4">Backup file</span>
              <div class="flex gap-2">
                <input value={backup?.path ?? ""} readonly placeholder="Choose a meshware-backup-….mwb file" class="mono {inputBox} placeholder:font-sans" />
                <button class="btn btn-secondary h-10 shrink-0 px-4 text-sm" onclick={pickBackup}>Browse…</button>
              </div>
              <span class="mt-1.5 block text-xs text-ink-5">
                This Director takes over the backed-up node's identity — run it in only one place at a time. Your own user key stays what it is.
              </span>
            </label>
            {@render personNameField()}
            {@render nodeNameField()}
            {@render contributeField()}
            <div class="flex items-center justify-end pt-1">
              <button class="btn btn-primary btn-lg" disabled={busy || !backup || !nodeName.trim()} onclick={doRestore}>
                {busy ? "Restoring…" : "Restore and start"}
              </button>
            </div>
          </div>
        {:else if path === "pair"}
          {#if pairPhase === "idle"}
            <div class="space-y-4">
              {@render personNameField()}
              {@render nodeNameField()}
              {@render contributeField()}
              <div class="flex items-center justify-end pt-1">
                <button class="btn btn-primary btn-lg" disabled={!nodeName.trim()} onclick={startPairing}>Start pairing</button>
              </div>
            </div>
          {:else}
            <!-- trust surface: dark, like the approve modal on the other side -->
            <div
              class="on-code pop-in -m-6 rounded-xl"
              style="background: linear-gradient(165deg, var(--surface-code-2) 0%, var(--surface-code) 100%); box-shadow: var(--sh-code);"
            >
              <div class="flex items-center gap-2.5 px-6 pt-[22px]">
                <span class="mono text-2xs tracking-[0.1em] uppercase" style="color: var(--accent-bright);">
                  {pairPhase === "joined" ? "pairing complete" : "pair this machine"}
                </span>
                {#if pairAddr}
                  <span class="mono ml-auto text-xs text-inkcode-4">{pairAddr}</span>
                {/if}
              </div>

              {#if pairPhase === "waiting"}
                <div class="px-6 pt-6 pb-8 text-center">
                  <div
                    class="relative mx-auto grid h-[116px] w-full max-w-[380px] place-items-center overflow-hidden rounded-lg"
                    style="background: color-mix(in srgb, var(--accent-bright) 6%, transparent); border: 1px solid color-mix(in srgb, var(--accent-bright) 14%, transparent);"
                  >
                    <div class="absolute h-[200px] w-[200px] rounded-full border" style="border-color: rgba(100,228,202,.45); animation: mw-radar var(--motion-radar);"></div>
                    <div class="absolute h-[200px] w-[200px] rounded-full border" style="border-color: rgba(100,228,202,.35); animation: mw-radar var(--motion-radar); animation-delay: 1.3s;"></div>
                    <div class="relative text-center">
                      <div class="mono text-sm" style="color: var(--accent-bright);">announcing…</div>
                      <div class="mt-1 text-xs text-inkcode-3">waiting for someone to let you in</div>
                    </div>
                  </div>
                  <!--
                    A radar sweep with no explanation is a progress bar that
                    might be broken. Say what the other side is seeing, so
                    "it just spins forever" becomes "nobody has it open" —
                    a thing the person can actually do something about.
                  -->
                  <div class="mt-4 text-sm leading-relaxed text-inkcode-3">
                    This machine is now visible to the mesh. Anyone already in it sees
                    <span style="color: var(--ink-code-1);">“{personName.trim() ? `${personName.trim()} on ${nodeName || "this machine"}` : nodeName || "this machine"} wants to join”</span>
                    at the top of their <span style="color: var(--ink-code-1);">Access</span> page — they press
                    a button, you both check the same six digits, and you are in.
                  </div>
                  <!-- The escape hatch, on the screen where it is needed:
                       an admin whose Director cannot hear this machine can
                       be read one of these and type it under "Pair by
                       address". Said *before* anyone is stuck. -->
                  {#if pairAddrs.length}
                    <div class="mx-auto mt-4 max-w-[380px] rounded-lg px-4 py-3 text-left" style="background: rgba(160, 176, 200, 0.06); border: 1px solid var(--edge-code);">
                      <div class="text-2xs font-semibold tracking-[.08em] uppercase" style="color: var(--ink-code-3);">
                        If they cannot see you
                      </div>
                      <div class="mt-1 text-xs leading-relaxed text-inkcode-3">
                        Ask them to press <span style="color: var(--ink-code-1);">Pair by address</span> on Access and type
                        {pairAddrs.length === 1 ? "this" : "one of these"}:
                      </div>
                      <div class="mt-1.5 flex flex-wrap gap-x-3 gap-y-1">
                        {#each pairAddrs as a (a)}
                          <span class="mono selectable text-sm" style="color: var(--accent-bright);">{a}</span>
                        {/each}
                      </div>
                      <div class="mono mt-2 text-2xs" style="color: var(--ink-code-4);">
                        announcing via {pairMdnsError ? "UDP broadcast only (mDNS: " + pairMdnsError + ")" : "mDNS and UDP broadcast"}
                      </div>
                    </div>
                  {/if}
                  <div class="mt-2.5 text-xs leading-relaxed text-inkcode-4">
                    Nothing happening? Ask them to open Meshware Director. If their machine is not on
                    this network, go back and use <span style="color: var(--ink-code-2);">Join with an invite</span>
                    instead. From a terminal, <span class="mono" style="color: var(--accent-bright);">meshnode invite approve</span>
                    does the same job.
                  </div>
                  <!-- The way out the paragraph above promises (DIR-6):
                       back to the form, to fix a name and try again. The
                       chevron at the top goes all the way back. -->
                  <button class="btn btn-secondary mt-5 h-10 px-5 text-sm" disabled={pairCancelling} onclick={cancelPairing}>
                    {pairCancelling ? "Stopping…" : "Stop waiting"}
                  </button>
                </div>
              {:else if pairPhase === "code" || pairPhase === "confirming"}
                <div class="px-6 pt-4 pb-6 text-center">
                  <h3 class="m-0 text-xl font-bold tracking-[-.02em]">Do these codes match?</h3>
                  <p class="mx-auto mt-[7px] mb-0 max-w-[32ch] text-sm leading-relaxed" style="color: var(--ink-code-3);">
                    Meshware <span class="mono" style="color: var(--ink-code-2);">{pairMesh}</span>
                    ({pairApprover}) wants to pair. Both sides must see the same six digits —
                    or the same two words.
                  </p>
                  <div class="mt-[22px] mb-1.5">
                    <!-- Open by default on this side: the wizard runs on the
                         machine that is *joining*, which is the one the other
                         person is being asked to read a code off. -->
                    <PairCode code={pairCode} qrOpen />
                  </div>
                  <div class="mono text-xs" style={`color: ${codeTtl <= 10 ? "var(--status-warn)" : "var(--ink-code-4)"};`}>
                    expires in {fmtCountdown(codeTtl)}
                  </div>
                  <div class="mt-[22px] grid gap-2.5" style="grid-template-columns: 1fr 1.25fr;">
                    <button
                      class="h-11 rounded-md border text-base font-semibold transition-colors disabled:opacity-40"
                      style="border-color: var(--status-error-edge); color: var(--status-error-ink); background: transparent;"
                      disabled={pairPhase === "confirming"}
                      onclick={() => decide(false)}
                    >
                      They differ — reject
                    </button>
                    <button
                      class="btn btn-bright h-11 text-base"
                      disabled={pairPhase === "confirming"}
                      onclick={() => decide(true)}
                    >
                      {pairPhase === "confirming" ? "Waiting for the other side…" : "They match — approve"}
                    </button>
                  </div>
                  {#if pairPhase === "confirming"}
                    <!-- Approved here, and the other side has gone quiet:
                         still a way out rather than a spinner to sit through. -->
                    <button class="mt-3 text-sm font-medium text-inkcode-3 underline-offset-2 hover:underline disabled:opacity-40" disabled={pairCancelling} onclick={cancelPairing}>
                      {pairCancelling ? "Stopping…" : "Stop waiting"}
                    </button>
                  {/if}
                </div>
              {:else if pairPhase === "joined"}
                <div class="px-6 pt-7 pb-8 text-center">
                  <div class="mx-auto grid h-[46px] w-[46px] place-items-center rounded-full" style="background: color-mix(in srgb, var(--accent-bright) 14%, transparent);">
                    <span class="inline-grid" style="color: var(--accent-bright);"><Icon name="check" size={22} /></span>
                  </div>
                  <div class="mt-4 text-lg font-bold tracking-[-.02em]">Paired — joining {pairMesh}…</div>
                  <!-- The second half, named before it happens rather than
                       discovered by being refused: this machine is a member,
                       and the person at it is a separate key that still has
                       to be recognised. It is asked for automatically. -->
                  <div class="mx-auto mt-2 max-w-[38ch] text-sm leading-relaxed" style="color: var(--ink-code-3);">
                    Next, an admin gives <span style="color: var(--ink-code-1);">{personName || "you"}</span> a
                    name and decides what you can do. The request goes to them on its own — you will
                    see where you stand at the top of the window.
                  </div>
                </div>
              {/if}
            </div>
          {/if}
        {/if}
      </div>
    {/if}

    {#if app.boot}
      <div class="mt-6 flex items-center justify-center gap-2 text-2xs text-ink-muted">
        <span>Your user key</span>
        <span class="mono selectable">{app.boot.userId.slice(0, 26)}…</span>
        <button class="iconbtn h-5 w-5" title="Copy your user key" onclick={copyKey}><Icon name="copy" size={12} /></button>
      </div>
    {/if}
  </div>
</div>

{#snippet nodeNameField()}
  <label class="block">
    <span class="mb-1.5 block text-sm text-ink-4">Node name</span>
    <input bind:value={nodeName} class={inputBox} />
  </label>
{/snippet}

<!--
  Who *you* are, as against what this machine is called. Two different
  things, and the wizard used to ask only for the second — so the admin on
  the other side got a request from `mia-laptop` and the person joining
  found out a person was a separate concept by being refused.
-->
{#snippet personNameField()}
  <label class="block">
    <span class="mb-1.5 block text-sm text-ink-4">Your name</span>
    <input bind:value={personName} class={inputBox} placeholder="mia" />
    <span class="mt-1.5 block text-xs leading-normal text-ink-5">
      What a mesh admin will see when they are asked to let you in — and the name on everything
      you do afterwards. There is no password: you are a key on this machine.
    </span>
  </label>
{/snippet}

{#snippet contributeField()}
  {#if canContribute}
    <label class="flex cursor-pointer items-center justify-between gap-4 rounded-md px-3.5 py-3" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
      <span>
        <span class="block text-base font-medium text-ink">Contribute compute</span>
        <span class="mt-0.5 block text-xs text-ink-5">
          {#if path === "create"}
            Let this machine run work for the mesh you are creating. You can change this later in
            Settings.
          {:else}
            Offer this machine's cores to the mesh. An administrator of the mesh decides whether
            to accept — a new machine is theirs to say yes to.
          {/if}
        </span>
      </span>
      <input type="checkbox" class="switch" bind:checked={contribute} />
    </label>
  {/if}
{/snippet}

<style>
  .wizard-aurora {
    position: absolute;
    inset: 0;
    z-index: 0;
    pointer-events: none;
    background:
      radial-gradient(52% 46% at 10% 14%, color-mix(in srgb, var(--hue-indigo) 20%, transparent), transparent 72%),
      radial-gradient(46% 42% at 90% 84%, color-mix(in srgb, var(--hue-coral) 16%, transparent), transparent 72%),
      radial-gradient(40% 36% at 84% 8%, color-mix(in srgb, var(--hue-amber) 13%, transparent), transparent 72%),
      radial-gradient(44% 40% at 14% 90%, color-mix(in srgb, var(--hue-plum) 12%, transparent), transparent 72%);
    opacity: 1;
    transition: opacity var(--dur-slower) var(--ease);
  }
  /* A form is open: the colour steps back with the wave. */
  .wizard-aurora.quiet {
    opacity: 0.55;
  }
</style>
