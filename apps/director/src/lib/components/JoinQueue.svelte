<script lang="ts">
  // "Somebody wants to join." The one thing this product has to make easy.
  //
  // ## What was wrong
  //
  // Joining a mesh is two acts, and the product used to make you do them on
  // two screens with no hint that the second one existed:
  //
  //   1. a **machine** becomes a member (pairing, or an invite token), and
  //   2. the **person** on that machine gets their key recognised
  //      (enrolment), which is what actually lets them do anything.
  //
  // Then, once both halves were on one screen, the first half still failed
  // silently more often than it worked: the machine list was a 2.5-second
  // mDNS sweep run from the browser, paused whenever the window was hidden,
  // that hid any machine whose name it had seen before — and when it showed
  // nothing it could not say whether that was because nobody was waiting or
  // because multicast never arrived. "I pressed Join and nothing appeared."
  //
  // ## What this is
  //
  // One queue that finds you. The engine listens for as long as an admin is
  // around, on mDNS *and* a UDP broadcast beacon (`pairing::LanWatch`), and
  // this screen reads its table. Every row says everything the admin needs
  // before pressing anything: who is at the machine, what it is called,
  // its address, whether the mesh has seen that key or that name before,
  // and how long they have been waiting. When the queue is empty it says
  // whether the two ears are working, and offers the address path for the
  // network where neither does.
  //
  // A person can be let in three ways: as a new user, as a second key for
  // somebody who already exists (the same person at a new machine), or not
  // at all. The trust moment is not softened for any of that: pairing
  // still shows six digits both humans compare, and an enrolment still
  // shows the word-pair code derived from the joiner's own key.
  import { untrack } from "svelte";

  import { api, errText, parseDuration, type CandidateView, type EnrolmentView, type NodeView, type UserView } from "../api";
  import { app, ago, until, ui, fmtCountdown, initialsOf, type KnownCandidate } from "../state.svelte";
  import { activeGrantsFor, grantSentence, rolesOf } from "../access";
  import { hueOf, hueStyle } from "../hues";
  import { ROLE_SUMMARY, roleHue } from "../roles";
  import Icon from "./Icon.svelte";
  import Spinner from "./Spinner.svelte";
  import PairCode from "./PairCode.svelte";
  import GrantScopeEditor from "./GrantScopeEditor.svelte";
  import Modal from "./Modal.svelte";
  import { staggerIn } from "../motion";
  import { poll } from "../poll";

  let { onchanged }: { onchanged?: () => void } = $props();

  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });

  /** "48s", "3m 10s" — how long somebody has been standing there. */
  function waiting(sinceMicros: number): string {
    const s = Math.max(0, Math.floor((now - sinceMicros / 1000) / 1000));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
    return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  }

  /** The key one machine is filed under, matching the engine's own. */
  const keyOf = (c: CandidateView) => c.nodeId ?? `name:${c.nodeName}`;

  // -- what is waiting ---------------------------------------------------------
  //
  // Three shapes, one list, oldest first so a card never jumps out from
  // under a cursor that is already on it.
  type Row =
    | { kind: "machine"; key: string; item: KnownCandidate }
    | { kind: "settling"; key: string; nodeName: string; at: number }
    | { kind: "person"; key: string; row: EnrolmentView };

  /** Candidates dismissed with "Not now" — for this window only, because a
   *  machine that is still announcing has not changed its mind. */
  let ignored = $state<string[]>([]);
  /** Machines that paired and whose person has not turned up yet. */
  let settling = $state<{ nodeName: string; at: number }[]>([]);

  /** A paired machine stops waiting for its person after this long. */
  const SETTLE_MS = 180_000;

  const people = $derived(app.pendingEnrolments);

  $effect(() => {
    // A settled machine's person has arrived (or it has waited long
    // enough). Either way the placeholder has done its job.
    //
    // `settling` is read through `untrack` because this effect writes it:
    // tracking its own output would re-enter on every prune, which happens
    // to converge but only because the second pass finds nothing to do.
    // The inputs that should re-run this are the queue and the clock.
    const names = new Set(people.map((p) => p.fromNode));
    void now;
    untrack(() => {
      const live = settling.filter((s) => !names.has(s.nodeName) && Date.now() - s.at < SETTLE_MS);
      if (live.length !== settling.length) settling = live;
    });
  });

  $effect(() => {
    // While a machine is settling, poll for its person faster than the
    // app's own cadence. Five seconds of a spinner after you have just
    // pressed the button is exactly the gap that made joining feel like
    // something had gone wrong, and it costs one cheap call to close it.
    if (settling.length === 0) return;
    return poll(() => app.refreshEnrolments(), 1200);
  });

  const rows = $derived.by((): Row[] => {
    const out: Row[] = [];
    for (const item of app.joinableCandidates) {
      const c = item.candidate;
      if (ignored.includes(keyOf(c))) continue;
      if (settling.some((s) => s.nodeName === c.nodeName)) continue;
      out.push({ kind: "machine", key: `m:${keyOf(c)}`, item });
    }
    for (const s of settling) out.push({ kind: "settling", key: `s:${s.nodeName}`, ...s });
    for (const p of people) out.push({ kind: "person", key: `p:${p.userId}`, row: p });
    return out;
  });

  // A machine announcing and a person asking are different things, and a
  // heading that calls two machines "2 people" is the same conflation this
  // whole screen exists to undo. Rows that are already settled are counted
  // out: nobody is waiting on the admin for those.
  const heading = $derived.by(() => {
    const machines = rows.filter((r) => r.kind === "machine").length;
    const persons = rows.filter((r) => r.kind === "person").length;
    const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
    if (machines && persons) return `${plural(machines, "machine", "machines")} and ${plural(persons, "person", "people")} want to join`;
    if (persons) return persons === 1 ? "Someone wants to join" : `${persons} people want to join`;
    if (machines) return machines === 1 ? "A machine wants to join" : `${machines} machines want to join`;
    if (rows.length) return "Letting a machine in…";
    return "Nobody is waiting right now";
  });

  /** True while nothing needs a decision — only a hand-off is in flight. */
  const onlySettling = $derived(rows.length > 0 && rows.every((r) => r.kind === "settling"));

  // -- the watch itself ------------------------------------------------------
  //
  // Said out loud, because an empty queue has two meanings and only one of
  // them is "nobody is waiting".
  const watch = $derived(app.lanWatch);
  const watchOk = $derived(!!watch?.running && (!watch.mdnsError || !watch.beaconError));
  const watchDead = $derived(!!watch?.running && !!watch.mdnsError && !!watch.beaconError);
  const polledAgo = $derived(app.lanPolledAt ? Math.max(0, Math.floor((now - app.lanPolledAt) / 1000)) : null);

  /** The relationship line for a machine the mesh has seen before. */
  function relationNote(item: KnownCandidate): { text: string; tone: "info" | "warn" | "error" } | null {
    const n = item.known;
    if (!n) return null;
    switch (item.relation) {
      case "returning":
        return {
          text: `The same machine as ${n.name}, which ${n.left ? "left the mesh" : "stopped answering"} — letting it in makes it a member again with its old key.`,
          tone: "info",
        };
      case "revoked":
        return {
          text: `This is the key of ${n.name}, which was revoked. Revocation is permanent: the machine must start over with a fresh identity before it can join.`,
          tone: "error",
        };
      case "name-clash":
        return {
          text: `A different machine called ${n.name} is already a member. This one has a new key — a rebuilt machine, or two with the same name.`,
          tone: "warn",
        };
      case "name-reuse":
        return {
          text: `A member called ${n.name} left earlier; this machine reuses the name with a new key.`,
          tone: "info",
        };
      default:
        return null;
    }
  }

  // ---------------------------------------------------------------------------
  // Half one: the machine
  // ---------------------------------------------------------------------------
  type PairStep = "contacting" | "code" | "waiting" | "failed";
  let pairing = $state<{
    /** The row this belongs to, or `addr:…` for a typed address. */
    key: string;
    nodeName: string;
    step: PairStep;
    code: string;
    session: number;
    codeAt: number;
    /** While contacting: which address is being dialled. */
    trying?: string;
    message?: string;
  } | null>(null);

  const codeTtl = $derived(
    pairing?.step === "code" ? 300 - Math.floor((now - pairing.codeAt) / 1000) : 300,
  );
  $effect(() => {
    if (pairing?.step === "code" && codeTtl <= 0) {
      pairing = { ...pairing, step: "failed", message: "The code expired — ask them to try again." };
    }
  });

  async function letMachineIn(key: string, nodeName: string, addrs: string[]) {
    if (!addrs.length) return;
    pairing = { key, nodeName, step: "contacting", code: "", session: 0, codeAt: 0, trying: addrs[0] };
    try {
      await api.pairApproveStart(addrs, [], (ev) => {
        if (!pairing || pairing.key !== key) return;
        if (ev.type === "trying") {
          pairing = { ...pairing, step: "contacting", trying: ev.addr };
        } else if (ev.type === "code") {
          pairing = {
            ...pairing,
            step: "code",
            nodeName: ev.joinerName || pairing.nodeName,
            code: ev.code,
            session: ev.session,
            codeAt: Date.now(),
          };
        } else if (ev.type === "done") {
          // The machine is a member. Its person is a separate key that has
          // to be recognised too — the card stays and says so, rather than
          // declaring victory and leaving them locked out.
          app.dropCandidate(ev.nodeName);
          settling = [...settling, { nodeName: ev.nodeName, at: Date.now() }];
          pairing = null;
          app.flash(`${ev.nodeName} joined the mesh`);
          void app.refresh();
          onchanged?.();
        } else if (ev.type === "failed") {
          pairing = pairing ? { ...pairing, step: "failed", message: ev.message } : null;
        }
      });
    } catch (e) {
      pairing = pairing ? { ...pairing, step: "failed", message: errText(e) } : null;
    }
  }

  async function decidePair(match: boolean) {
    if (!pairing) return;
    const session = pairing.session;
    if (!match) {
      pairing = null;
      try {
        await api.pairApproveDecide(session, false);
      } catch {
        // The other side gave up first; nothing to tell anybody.
      }
      return;
    }
    pairing = { ...pairing, step: "waiting" };
    try {
      await api.pairApproveDecide(session, true);
    } catch (e) {
      pairing = pairing ? { ...pairing, step: "failed", message: errText(e) } : null;
    }
  }

  // -- pair by address ----------------------------------------------------------
  //
  // For the network where neither ear works: the joiner's wizard shows its
  // own address, and the admin types it here. Same exchange, same code.
  let addrOpen = $state(false);
  let manualAddr = $state("");
  let manualErr = $state<string | null>(null);

  function addrLooksValid(t: string): boolean {
    const cut = t.lastIndexOf(":");
    if (cut <= 0) return false;
    const host = t.slice(0, cut).trim();
    const port = Number(t.slice(cut + 1));
    if (!host || host.includes(" ")) return false;
    return Number.isInteger(port) && port > 0 && port < 65536;
  }

  function pairManual() {
    const addr = manualAddr.trim();
    if (!addrLooksValid(addr)) {
      manualErr = "Needs host:port — the address the other machine shows on its pairing screen.";
      return;
    }
    manualErr = null;
    void letMachineIn(`addr:${addr}`, addr, [addr]);
  }

  // ---------------------------------------------------------------------------
  // Half two: the person
  // ---------------------------------------------------------------------------
  type Panel = "create" | "attach";
  let openPerson = $state<string | null>(null);
  let panel = $state<Panel>("create");
  let pName = $state("");
  let pRole = $state("operator");
  let pNodes = $state("any");
  let pServices = $state<string[]>(["*"]);
  let pExpires = $state("");
  let pValid = $state(true);
  /** Attach: the existing user the key is being added to. */
  let attachTo = $state<string>("");
  let busy = $state(false);
  let err = $state<string | null>(null);
  let declining = $state<EnrolmentView | null>(null);
  let declineReason = $state("");

  /** People a new key can be attached to: here, and not revoked. The
   *  founder is a key, not a name — their authority is in genesis. */
  const attachable = $derived(app.users.filter((u) => !u.revoked && !u.isFounder));
  const attachTarget = $derived(attachable.find((u) => u.username === attachTo) ?? null);
  const inherited = $derived(attachTarget ? activeGrantsFor(app.grants, attachTarget.userId) : []);

  function openPanel(r: EnrolmentView, which: Panel) {
    if (openPerson === r.userId && panel === which) {
      openPerson = null;
      return;
    }
    openPerson = r.userId;
    panel = which;
    err = null;
    if (which === "create") {
      // Seeded to the commonest answer rather than the safest-looking one:
      // somebody being let in almost always needs to be able to run things,
      // and an admin who wanted `viewer` will say so. `viewer` as a default
      // produces a person who joins and can do nothing, which reads to
      // everyone involved as a failure of the product.
      pName = r.suggestedUsername;
      pRole = "operator";
      pNodes = "any";
      pServices = ["*"];
      pExpires = "";
    } else {
      // The likeliest match first: a request that suggests a name somebody
      // already has is almost always that somebody at a new machine.
      const guess = attachable.find((u) => u.username.toLowerCase() === r.suggestedUsername.toLowerCase());
      attachTo = guess?.username ?? attachable[0]?.username ?? "";
    }
  }

  async function approvePerson(r: EnrolmentView) {
    busy = true;
    err = null;
    try {
      const expiresSecs = parseDuration(pExpires);
      const globs = pServices.length ? pServices : ["*"];
      const username = pName.trim();
      await api.enrolApprove({
        user: r.userId,
        username,
        role: pRole,
        nodes: pNodes.trim() || "any",
        services: globs[0],
        expiresSecs,
      });
      // "Only these scripts" is one grant per script — the shape RBAC
      // enforces natively; grants add up and each is revocable alone.
      for (const s of globs.slice(1)) {
        await api.grantAdd({
          user: username,
          role: pRole,
          nodes: pNodes.trim() || "any",
          services: s,
          expiresSecs,
        });
      }
      app.flash(`${username} is in — ${pRole}`);
      openPerson = null;
      await settle();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }

  async function attachPerson(r: EnrolmentView) {
    if (!attachTarget) return;
    busy = true;
    err = null;
    try {
      const copied = await api.enrolAttach(r.userId, attachTarget.username);
      app.flash(
        `${attachTarget.username} can now use ${r.fromNode} — ${copied} grant${copied === 1 ? "" : "s"} carried over`,
      );
      openPerson = null;
      await settle();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }

  async function settle() {
    await app.refreshEnrolments();
    await app.refreshAfterChange();
    onchanged?.();
  }

  async function doDecline() {
    const r = declining;
    if (!r) return;
    busy = true;
    try {
      await api.enrolDecline(r.userId, declineReason.trim());
      app.flash("request declined");
      declining = null;
      declineReason = "";
      await app.refreshEnrolments();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }

  /** The node a person is asking from, if the mesh knows it. */
  // Consoles too: this is a *display* lookup ("where is this person
  // sitting"), not a fleet one, and the person most likely to be enrolling
  // is on a Director — which on a machine that already runs `meshnode` is
  // a console and therefore deliberately not in `app.nodes`.
  const nodeOf = (name: string) =>
    app.nodes.find((n) => n.name === name) ??
    null;

  // -- the owner's veto, at the moment it is being decided ----------------------
  const isAdmin = $derived(app.whoami?.isFounder || app.whoami?.roles.includes("admin"));
  let holdBusy = $state<string | null>(null);
  let holdErr = $state<string | null>(null);
  async function holdBack(node: NodeView, allowed: boolean) {
    holdBusy = node.name;
    holdErr = null;
    try {
      await api.nodeContributeSet(
        node.nodeId,
        allowed,
        allowed ? "" : "held back when it joined",
      );
      await app.refresh();
      app.flash(
        allowed ? `${node.name} may run work` : `${node.name} will not be given work`,
      );
    } catch (e) {
      holdErr = errText(e);
    } finally {
      holdBusy = null;
    }
  }

  const rolesFor = (u: UserView) => rolesOf(app.grants, u.userId, u.isFounder);
</script>

<div
  class="fade-in card overflow-hidden"
  style={rows.length
    ? "box-shadow: var(--sh-card-lg), inset 0 0 0 1.5px var(--accent);"
    : "box-shadow: var(--sh-card-lg);"}
>
  <!-- ─────────── heading ─────────── -->
  <div class="flex flex-wrap items-baseline gap-x-2.5 gap-y-1 px-5 pt-4 pb-3">
    <span class="relative inline-flex h-2 w-2 self-center rounded-full" style={`background: ${rows.length ? "var(--accent)" : watchDead ? "var(--status-error)" : watchOk ? "var(--hue-sky)" : "var(--ink-faint)"};`}>
      {#if rows.length || watchOk}
        <span class="absolute inset-0 rounded-full" style={`background: ${rows.length ? "var(--accent)" : "var(--hue-sky)"}; animation: mw-pulse 2.4s ease-out infinite;`}></span>
      {/if}
    </span>
    <h2 class="m-0 text-md font-bold tracking-[-.02em] text-ink">{heading}</h2>
    <span class="text-xs text-ink-5">
      {#if rows.length}
        {onlySettling
          ? "almost there — finishing off on their side"
          : "they are waiting on you right now — nobody can do anything here until you say yes"}
      {:else}
        anyone on this network who chooses <em>Join a mesh on this network</em> appears here within a few seconds
      {/if}
    </span>
    <button class="linkish ml-auto text-xs" onclick={() => (addrOpen = !addrOpen)}>
      {addrOpen ? "Hide" : "Pair by address…"}
    </button>
  </div>

  {#if addrOpen}
    <div class="fade-in border-t border-edge bg-surface-2 px-5 py-3">
      <div class="flex flex-wrap items-center gap-2">
        <span class="text-sm text-ink-3">
          Their pairing screen shows an address — type it here if this page cannot see them.
        </span>
        <input
          bind:value={manualAddr}
          placeholder="192.168.1.42:7777"
          class="{ui.inputMono} ml-auto max-w-[240px]"
          onkeydown={(e) => e.key === "Enter" && pairManual()}
        />
        <button class={ui.primary} disabled={!manualAddr.trim() || !!pairing} onclick={pairManual}>Pair</button>
      </div>
      {#if manualErr}<div class="mt-2 text-xs" style="color: var(--status-error-ink);">{manualErr}</div>{/if}
    </div>
    {#if pairing && pairing.key.startsWith("addr:")}
      <div class="px-5 pb-5 pt-3">
        {@render pairSurface()}
      </div>
    {/if}
  {/if}

  <!-- ─────────── the rows ─────────── -->
  <div use:staggerIn={{ selector: ":scope > div", each: 50, y: 6 }}>
    {#each rows as r (r.key)}
      <div class="border-t border-edge">
        <!-- ─────────── a machine on the network ─────────── -->
        {#if r.kind === "machine"}
          {@const c = r.item.candidate}
          {@const who = c.person?.trim() || ""}
          {@const isPairing = pairing?.key === r.key}
          {@const note = relationNote(r.item)}
          {@const blocked = r.item.relation === "revoked"}
          {@const hue = hueOf(who || c.nodeName)}
          <!-- A container, so the row lays itself out by its own width
               (UX-11): at the 980px minimum the buttons beside the facts
               squeezed the key, the computer and the expiry to a few
               characters each. Narrow, the buttons go underneath. -->
          <div class="@container px-5 py-4">
            <div class="flex flex-wrap items-start gap-3">
              <span
                class="mono grid h-10 w-10 flex-none place-items-center rounded-lg text-xs font-semibold"
                style={`${hueStyle(hue)} background: var(--hue-wash); color: var(--hue-ink);`}
              >
                {#if who}{initialsOf(who)}{:else}<Icon name="pair" size={18} />{/if}
              </span>
              <span class="min-w-0 flex-1">
                <span class="flex flex-wrap items-center gap-x-2 gap-y-1">
                  {#if who}
                    <span class="text-[15px] font-semibold text-ink">{who}</span>
                    <span class="text-sm text-ink-5">on</span>
                    <span class="mono text-sm font-semibold text-ink">{c.nodeName}</span>
                  {:else}
                    <span class="mono text-[15px] font-semibold text-ink">{c.nodeName}</span>
                    <span class="text-sm text-ink-5">wants to join — nobody gave a name</span>
                  {/if}
                  <span class="hue-chip" style={hueStyle("sky")}>waiting {waiting(c.firstSeenMicros)}</span>
                  {#if r.item.relation === "returning"}
                    <span class="hue-chip" style={hueStyle("teal")}>coming back</span>
                  {:else if r.item.relation === "name-clash"}
                    <span class="hue-chip" style={hueStyle("amber")}>name in use</span>
                  {:else if r.item.relation === "revoked"}
                    <span class="hue-chip" style="--hue: var(--status-error); --hue-ink: var(--status-error-ink); --hue-wash: var(--status-error-wash);">revoked key</span>
                  {:else if r.item.relation === "new"}
                    <span class="hue-chip" style={hueStyle("indigo")}>new machine</span>
                  {/if}
                </span>

                <!-- Everything the joiner claims, laid out so it can be
                     read off in one glance — and *claims* is the word;
                     the code is what proves any of it. -->
                <dl class="mt-2 grid gap-x-6 gap-y-1 text-xs @min-[520px]:grid-cols-2">
                  <div class="flex gap-2">
                    <dt class="w-[68px] flex-none text-ink-5">Address</dt>
                    <dd class="mono m-0 min-w-0 text-ink-2">
                      {c.addrs[0] ?? "—"}
                      {#if c.addrs.length > 1}
                        <span class="whitespace-nowrap text-ink-5" title={c.addrs.slice(1).join("\n")}>&nbsp;+{c.addrs.length - 1} more</span>
                      {/if}
                    </dd>
                  </div>
                  <div class="flex gap-2">
                    <dt class="w-[68px] flex-none text-ink-5">Computer</dt>
                    <dd class="mono m-0 min-w-0 truncate text-ink-2" title={c.hostname ?? ""}>
                      {c.hostname && c.hostname !== c.nodeName ? c.hostname : c.nodeName}
                      {#if c.platform}<span class="text-ink-5"> · {c.platform}</span>{/if}
                    </dd>
                  </div>
                  <div class="flex gap-2">
                    <dt class="w-[68px] flex-none text-ink-5">Node key</dt>
                    <dd class="mono m-0 min-w-0 truncate text-ink-2" title={c.nodeId ?? ""}>
                      {#if c.nodeId}{c.nodeId.slice(0, 15)}…{:else}<span class="text-ink-5">not announced</span>{/if}
                    </dd>
                  </div>
                  <div class="flex gap-2">
                    <dt class="w-[68px] flex-none text-ink-5">Heard via</dt>
                    <dd class="m-0 text-ink-2">
                      {[c.viaMdns ? "mDNS" : null, c.viaBeacon ? "broadcast" : null].filter(Boolean).join(" + ") || "—"}
                      <span class="text-ink-5"> · last {ago(c.lastSeenMicros)}</span>
                    </dd>
                  </div>
                  {#if r.item.known}
                    <div class="flex gap-2 @min-[520px]:col-span-2">
                      <dt class="w-[68px] flex-none text-ink-5">Known as</dt>
                      <dd class="mono m-0 min-w-0 text-ink-2">
                        {r.item.known.name}
                        <span class="text-ink-5">
                          · {r.item.known.revoked ? "revoked" : r.item.known.left ? "left" : r.item.known.liveness}
                          · joined {ago(r.item.known.joinedAtMicros)}
                        </span>
                      </dd>
                    </div>
                  {/if}
                </dl>

                {#if note}
                  <div
                    class="mt-2.5 rounded-md px-3 py-2 text-xs leading-relaxed"
                    style={note.tone === "error"
                      ? "background: var(--status-error-wash); color: var(--status-error-ink);"
                      : note.tone === "warn"
                        ? "background: var(--status-warn-wash); color: var(--status-warn-ink);"
                        : "background: var(--accent-wash); color: var(--accent-ink);"}
                  >
                    {note.text}
                  </div>
                {/if}
              </span>
              {#if !isPairing}
                <span class="flex items-center gap-2 self-center @max-[720px]:basis-full @max-[720px]:justify-end">
                  <!-- One verb per act across the queue (UX-19): "Let in…" on
                       every row, whoever it is; "Not now" only hides, and
                       "Decline…" (people) records a no. -->
                  <button class={ui.secondary} title="Hide this machine here until it asks again" onclick={() => (ignored = [...ignored, keyOf(c)])}>
                    Not now
                  </button>
                  <button
                    class={ui.primary}
                    disabled={blocked || !!pairing}
                    title={blocked ? "A revoked key cannot come back" : ""}
                    onclick={() => letMachineIn(r.key, c.nodeName, c.addrs)}
                  >
                    Let in…
                  </button>
                </span>
              {/if}
            </div>

            {#if isPairing}
              <div class="mt-4">{@render pairSurface()}</div>
            {/if}
          </div>

        <!-- ─────────── paired; its person has not arrived yet ─────────── -->
        {:else if r.kind === "settling"}
          <div class="flex flex-wrap items-center gap-3 px-5 py-4">
            <span
              class="grid h-10 w-10 flex-none place-items-center rounded-lg"
              style="background: var(--accent-wash); color: var(--accent-ink);"
            >
              <Icon name="check" size={16} />
            </span>
            <span class="min-w-0">
              <span class="block font-semibold text-ink"><span class="mono">{r.nodeName}</span> is in the mesh</span>
              <span class="mt-0.5 block text-xs text-ink-5">
                Now waiting for the person using it to ask for an account — their Director
                does that on its own, within a few seconds of joining.
              </span>
            </span>
            <span class="ml-auto flex items-center gap-2">
              <Spinner size={16} color="var(--accent-ink)" track="var(--accent-wash)" />
              <button
                class={ui.secondary}
                onclick={() => (settling = settling.filter((s) => s.nodeName !== r.nodeName))}
              >
                Dismiss
              </button>
            </span>
            <!-- The moment the decision is actually live, put in front of
                 the person making it. A machine somebody just let in is a
                 machine whose cores the mesh will start using, and "let
                 them in" and "run our work on their hardware" are not the
                 same permission. Offered here rather than only on Fleet
                 because this is the one screen where the admin is already
                 thinking about this machine. -->
            {#if isAdmin}
              {@const joined = nodeOf(r.nodeName)}
              {#if joined}
              <div class="w-full rounded-md px-3 py-2 text-xs leading-relaxed" style="background: var(--surface-subtle);">
                {#if joined.contributes}
                  The mesh will place work on it. If that is not what you want —
                  it is somebody's laptop, or hardware you do not own —
                  <button
                    class="linkish"
                    disabled={holdBusy === r.nodeName}
                    onclick={() => holdBack(joined, false)}
                  >{holdBusy === r.nodeName ? "…" : "hold it back"}</button>. Reversible from Fleet at any time.
                {:else}
                  Held back: the mesh will not place work on it. It is a full member otherwise.
                  <button
                    class="linkish"
                    disabled={holdBusy === r.nodeName}
                    onclick={() => holdBack(joined, true)}
                  >{holdBusy === r.nodeName ? "…" : "let it run work"}</button>
                {/if}
                {#if holdErr}<span class="text-error"> {holdErr}</span>{/if}
              </div>
              {/if}
            {/if}
          </div>

        <!-- ─────────── a person asking to be recognised ─────────── -->
        {:else}
          {@const isOpen = openPerson === r.row.userId}
          {@const name = r.row.suggestedUsername || ""}
          {@const from = nodeOf(r.row.fromNode)}
          {@const existing = attachable.find((u) => u.username.toLowerCase() === name.toLowerCase())}
          <div class="@container px-5 py-4">
            <div class="flex flex-wrap items-start gap-3">
              <span
                class="mono grid h-10 w-10 flex-none place-items-center rounded-full text-xs font-semibold"
                style={`${hueStyle(hueOf(name || r.row.shortId))} background: var(--hue-wash); color: var(--hue-ink);`}
              >
                {initialsOf(name || r.row.shortId.slice(4))}
              </span>
              <span class="min-w-0 flex-1">
                <span class="flex flex-wrap items-center gap-x-2 gap-y-1">
                  <span class="text-[15px] font-semibold text-ink">{name || "(no name given)"}</span>
                  <span class="text-sm text-ink-5">wants an account on this mesh</span>
                  <span class="hue-chip" style={hueStyle("sky")}>asked {ago(r.row.requestedAtMicros)}</span>
                  {#if existing}
                    <span class="hue-chip" style={hueStyle("amber")} title="Somebody with this name is already here — probably them, at a new machine">name already here</span>
                  {/if}
                </span>
                <dl class="mt-2 grid gap-x-6 gap-y-1 text-xs @min-[520px]:grid-cols-2">
                  <div class="flex gap-2">
                    <dt class="w-[68px] flex-none text-ink-5">From</dt>
                    <dd class="mono m-0 min-w-0 truncate text-ink-2">
                      {r.row.fromNode}
                      {#if from?.addrs[0]}<span class="text-ink-5"> · {from.addrs[0].replace(/:\d+$/, "")}</span>{/if}
                    </dd>
                  </div>
                  <div class="flex gap-2">
                    <dt class="w-[68px] flex-none text-ink-5">Their key</dt>
                    <dd class="mono m-0 min-w-0 truncate text-ink-2" title={r.row.userId}>{r.row.userId.slice(0, 15)}…</dd>
                  </div>
                  <div class="flex gap-2">
                    <dt class="w-[68px] flex-none text-ink-5">Expires</dt>
                    <dd class="m-0 whitespace-nowrap text-ink-2">{until(r.row.expiresAtMicros)}&nbsp;<span class="text-ink-5">unless answered</span></dd>
                  </div>
                </dl>
              </span>
              <span class="flex flex-wrap items-center gap-2 self-center @max-[720px]:basis-full @max-[720px]:justify-end">
                <!-- "Decline" stays put while a panel is open. Hiding it
                     meant the only way to say no was to first cancel out of
                     saying yes, which is a strange thing to ask of somebody
                     who has decided. -->
                <button class={ui.secondary} title="Refuse this request — recorded, and they are told" onclick={() => { declineReason = ""; declining = r.row; }}>
                  Decline…
                </button>
                {#if attachable.length}
                  <button
                    class={isOpen && panel === "attach" ? ui.secondary : ui.secondary}
                    style={isOpen && panel === "attach" ? "box-shadow: inset 0 0 0 1.5px var(--hue-plum);" : ""}
                    title="The same person at another machine: add this key to their existing account"
                    onclick={() => openPanel(r.row, "attach")}
                  >
                    {isOpen && panel === "attach" ? "Cancel" : "Already here…"}
                  </button>
                {/if}
                <button class={isOpen && panel === "create" ? ui.secondary : ui.primary} onclick={() => openPanel(r.row, "create")}>
                  {isOpen && panel === "create" ? "Cancel" : "Let in…"}
                </button>
              </span>
            </div>
          </div>

          {#if isOpen}
            <div class="fade-in border-t border-edge bg-surface-2 px-5 py-4">
              <!-- The check that makes this safe, said plainly. A code
                   nobody is told to compare is a code nobody compares. -->
              <div class="flex flex-wrap items-center gap-3 rounded-lg px-4 py-3" style="background: var(--accent-wash);">
                <span class="text-sm" style="color: var(--accent-ink);">
                  <strong>Check it is really them.</strong> Ask them to read out the code on their
                  screen. Neither side sent it — you both worked it out from their key.
                </span>
                <span
                  class="mono ml-auto rounded-full px-3 py-1 text-sm font-semibold"
                  style="background: var(--surface); color: var(--accent-ink);"
                >
                  {r.row.code}
                </span>
              </div>

              {#if panel === "create"}
                <!-- Two decisions, side by side, and everything under them
                     is the consequence of the second one. The name field
                     rides *inside* the editor rather than in a column
                     beside it, which is what left a third of the panel
                     empty — and it has to be one editor, because two would
                     each be writing the same bound scope. -->
                <div class="mt-4">
                  <GrantScopeEditor
                    compact
                    bind:role={pRole}
                    bind:nodes={pNodes}
                    bind:services={pServices}
                    bind:expires={pExpires}
                    bind:valid={pValid}
                    subject={pName.trim() || "They"}
                  >
                    {#snippet lead()}
                      <label class="block">
                        <span class={ui.label}>Call them</span>
                        <input
                          bind:value={pName}
                          placeholder="mia"
                          class={ui.inputMono}
                        />
                        <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
                          The name on every job they run and every line of history.
                          {#if existing && pName.trim().toLowerCase() === existing.username.toLowerCase()}
                            <span style="color: var(--status-warn-ink);">
                              That name is taken — if this is the same person, use <em>Already here</em> instead.
                            </span>
                          {/if}
                        </p>
                      </label>
                    {/snippet}
                  </GrantScopeEditor>
                </div>

                {#if err}<div class={ui.errorBox}>{err}</div>{/if}
                <div class="mt-4 flex flex-wrap items-center gap-2">
                  <button
                    class={ui.primary}
                    disabled={busy || !pName.trim() || !pValid}
                    onclick={() => approvePerson(r.row)}
                  >
                    {busy ? "Letting them in…" : `Let ${pName.trim() || "them"} in`}
                  </button>
                  <span class="text-xs text-ink-5">
                    You can change or take this away at any time, and everything they do is
                    recorded under their name.
                  </span>
                </div>
              {:else}
                <!-- The same person, a new machine. Their new key joins
                     the account they already have and inherits what it can
                     do; nothing about the old key changes. -->
                <div class="mt-4 grid gap-4 md:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)]">
                  <label class="block">
                    <span class={ui.label}>This is</span>
                    <select bind:value={attachTo} class="{ui.input} appearance-none">
                      {#each attachable as u (u.userId)}
                        <option value={u.username}>{u.username}</option>
                      {/each}
                    </select>
                    <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
                      Their new machine gets a key of its own — this adds it to the account they
                      already have, under the same name. The old machine keeps working.
                    </p>
                  </label>
                  <div class="rounded-md bg-surface px-4 py-3" style="box-shadow: var(--sh-card);">
                    <div class="flex flex-wrap items-center gap-2">
                      <span class={ui.label}>What {attachTarget?.username ?? "they"} can do</span>
                      {#if attachTarget}
                        {#each rolesFor(attachTarget) as role (role)}
                          <span class="hue-chip" style={hueStyle(roleHue(role))} title={ROLE_SUMMARY[role]?.long ?? ""}>{role}</span>
                        {/each}
                      {/if}
                    </div>
                    {#if inherited.length === 0}
                      <p class="mt-2 mb-0 text-sm text-ink-5">
                        Nothing yet — the new key will be recognised, and can be given a role afterwards.
                      </p>
                    {:else}
                      <ul class="mt-2 mb-0 list-none space-y-1 p-0 text-sm text-ink-2">
                        {#each inherited as g (g.grantId)}
                          <li class="flex items-start gap-2">
                            <span class="mt-[7px] h-1.5 w-1.5 flex-none rounded-full" style={`${hueStyle(roleHue(g.role))} background: var(--hue);`}></span>
                            <span>{grantSentence(g)}<span class="num text-xs text-ink-5"> · {until(g.expiresAtMicros)}</span></span>
                          </li>
                        {/each}
                      </ul>
                      <p class="mt-2 mb-0 text-xs text-ink-5">Each is copied to the new key as its own grant, revocable on its own.</p>
                    {/if}
                  </div>
                </div>

                {#if err}<div class={ui.errorBox}>{err}</div>{/if}
                <div class="mt-4 flex flex-wrap items-center gap-2">
                  <button class={ui.primary} disabled={busy || !attachTarget} onclick={() => attachPerson(r.row)}>
                    {busy ? "Attaching…" : `This is ${attachTarget?.username ?? "them"} — add the key`}
                  </button>
                  <span class="text-xs text-ink-5">
                    Both keys show under one name; History says which machine did what.
                  </span>
                </div>
              {/if}
            </div>
          {/if}
        {/if}
      </div>
    {/each}
  </div>

  <!-- ─────────── the watch: is anybody listening? ─────────── -->
  <div
    class="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-edge px-5 py-2.5 text-xs"
    style={watchDead ? "background: var(--status-error-wash);" : "background: var(--surface-subtle);"}
  >
    {#if !watch || !watch.running}
      <span class="text-ink-5">
        {app.watchingLan ? "Starting to listen on this network…" : "Not watching this network — switch it on in Settings, or pair by address."}
      </span>
    {:else if watchDead}
      <span class="font-semibold" style="color: var(--status-error-ink);">Cannot listen on this network.</span>
      <span style="color: var(--status-error-ink);">
        mDNS: {watch.mdnsError} · broadcast: {watch.beaconError}. Use <em>Pair by address</em> above.
      </span>
    {:else}
      <span class="flex items-center gap-1.5 text-ink-4">
        <Icon name="search" size={12} />
        Listening on this network
      </span>
      <span class="mono" style={watch.mdnsError ? "color: var(--status-warn-ink);" : "color: var(--accent-ink);"} title={watch.mdnsError ?? "multicast DNS, the standard way — port udp/5353"}>
        mDNS {watch.mdnsError ? "off" : "on"}
      </span>
      <span class="mono" style={watch.beaconError ? "color: var(--status-warn-ink);" : "color: var(--accent-ink);"} title={watch.beaconError ?? `a broadcast beacon on udp/${watch.beaconPort}, for networks that drop multicast`}>
        broadcast udp/{watch.beaconPort} {watch.beaconError ? "off" : "on"}
      </span>
      {#if watch.localAddrs.length}
        <span class="mono text-ink-5" title="Your addresses — a firewall on this machine must let udp/5353 and udp/{watch.beaconPort} in on one of these">
          this machine {watch.localAddrs[0]}{watch.localAddrs.length > 1 ? ` +${watch.localAddrs.length - 1}` : ""}
        </span>
      {/if}
      {#if polledAgo != null}
        <span class="num ml-auto text-ink-5">checked {polledAgo}s ago</span>
      {/if}
    {/if}
  </div>
  {#if !rows.length && watch?.running && !watchDead}
    <!-- The one sentence that turns "nothing appears" into something a
         person can act on. Both ears are open on this side; what is
         usually shut is this machine's own firewall. -->
    <div class="px-5 pb-3 pt-0 text-xs leading-relaxed text-ink-5" style="background: var(--surface-subtle);">
      If a machine on this network does not appear within ten seconds of choosing <em>Join a mesh on
      this network</em>, this computer's firewall is probably not letting
      <span class="mono">udp/5353</span> or <span class="mono">udp/{watch.beaconPort}</span> in —
      allow them, or use <em>Pair by address</em> with the address their screen shows.
    </div>
  {/if}
</div>

<!-- The trust surface, dark, exactly as the joining side sees it — the
     two screens are meant to look like one act. -->
{#snippet pairSurface()}
  {#if pairing}
    <div
      class="on-code fade-in rounded-xl"
      style="background: linear-gradient(165deg, var(--surface-code-2) 0%, var(--surface-code) 100%); box-shadow: var(--sh-code);"
    >
      {#if pairing.step === "contacting"}
        <div class="px-6 py-7 text-center">
          <div class="mono text-sm" style="color: var(--accent-bright);">contacting {pairing.nodeName}…</div>
          {#if pairing.trying}
            <div class="mono mt-1.5 text-xs" style="color: var(--ink-code-4);">dialling {pairing.trying}</div>
          {/if}
          <button class="btn mt-4 h-8 border border-edge-code px-3 text-xs text-inkcode-2" onclick={() => (pairing = null)}>
            Give up
          </button>
        </div>
      {:else if pairing.step === "code" || pairing.step === "waiting"}
        <div class="px-6 pt-5 pb-6 text-center">
          <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Do these codes match?</h3>
          <p class="mx-auto mt-[7px] mb-0 max-w-[36ch] text-sm leading-relaxed" style="color: var(--ink-code-3);">
            Ask the person at <span class="mono" style="color: var(--ink-code-2);">{pairing.nodeName}</span>
            to read out what is on their screen. Both sides must see the same six
            digits — or the same two words.
          </p>
          <div class="mt-4 mb-1.5"><PairCode code={pairing.code} /></div>
          <div class="mono text-xs" style={`color: ${codeTtl <= 10 ? "var(--status-warn)" : "var(--ink-code-4)"};`}>
            expires in {fmtCountdown(codeTtl)}
          </div>
          <div class="mx-auto mt-5 grid max-w-[440px] gap-2.5" style="grid-template-columns: 1fr 1.25fr;">
            <button
              class="h-11 rounded-md border text-base font-semibold transition-colors disabled:opacity-40"
              style="border-color: var(--status-error-edge); color: var(--status-error-ink); background: transparent;"
              disabled={pairing.step === "waiting"}
              onclick={() => decidePair(false)}
            >
              They differ — reject
            </button>
            <button
              class="btn btn-bright h-11 text-base"
              disabled={pairing.step === "waiting"}
              onclick={() => decidePair(true)}
            >
              {pairing.step === "waiting" ? "Waiting for the other side…" : "They match — let it in"}
            </button>
          </div>
        </div>
      {:else}
        <div class="px-6 py-6 text-center">
          <div class="text-sm font-semibold" style="color: var(--status-error-ink);">
            {pairing.message ?? "Pairing failed."}
          </div>
          <button class="btn btn-bright mt-4 h-9 px-4 text-sm" onclick={() => (pairing = null)}>
            Close
          </button>
        </div>
      {/if}
    </div>
  {/if}
{/snippet}

<!--
  Declining is not a revocation and does not read like one: nothing is
  destroyed, the person is simply told no. It writes a tombstone so the
  request stops reappearing on every admin's screen — and they can ask
  again, which the copy says out loud so nobody treats this as a ban.
-->
<Modal open={declining !== null} kicker="decline request" peer={declining?.code ?? ""} onclose={() => (declining = null)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Decline this request?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      <span class="mono" style="color: var(--ink-code-2);">{declining?.suggestedUsername || declining?.shortId}</span>
      is told no, and the request stops appearing here. Nothing is revoked — they are not a
      user yet — and they can ask again from
      <span class="mono" style="color: var(--ink-code-2);">{declining?.fromNode}</span>.
    </p>
    <label class="mt-4 block">
      <span class="text-2xs font-semibold tracking-[.08em] uppercase" style="color: var(--ink-code-3);">Reason (optional)</span>
      <input
        bind:value={declineReason}
        placeholder="not this quarter"
        class="mono mt-1.5 h-10 w-full rounded-md border border-edge-code bg-transparent px-3 text-sm"
        style="color: var(--ink-code-1);"
      />
    </label>
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button
        class="btn btn-lg border border-edge-code font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]"
        onclick={() => (declining = null)}
      >
        Cancel
      </button>
      <button class="btn btn-danger btn-lg" disabled={busy} onclick={doDecline}>
        {busy ? "Declining…" : "Decline"}
      </button>
    </div>
  </div>
</Modal>
