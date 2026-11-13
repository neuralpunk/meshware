<script lang="ts">
  // Access — one job: let people in, and choose what each one can do.
  //
  // ## What this screen used to be
  //
  // Six tabs (People, Terminals, Secrets, Change control, Approvals,
  // Audit), and the first thing on the first one was a form asking for a
  // *role*, a *node selector* and a *service glob*. Every noun on it was
  // real and every one of them was load-bearing somewhere; together they
  // meant a new admin's first question — "my colleague is sitting right
  // there, how do I let them in?" — was answered nowhere on the page.
  //
  // The specific failures, in the order somebody meets them:
  //
  //   * **Joining was two invisible acts.** A machine pairs; the person on
  //     it separately enrols. Nothing said so. You could pair successfully
  //     and be told, on arrival, that the mesh did not know your key.
  //   * **Nothing came to the admin.** Finding a machine meant Mesh → Add
  //     node → Discover → scan, by hand. Finding a person meant opening
  //     this screen. Both sides watched a spinner and neither was told the
  //     other existed.
  //   * **Permissions were composed, not chosen.** Role × nodes × services
  //     × expiry, with the consequence stated nowhere.
  //   * **"Terminal policy" was a top-level tab.** A noun nobody arrives
  //     knowing, for the third of three conditions on a feature they had
  //     not asked about yet.
  //
  // ## What it is now
  //
  // The join queue is at the top and it finds you — `app.startLanWatch`
  // browses in the background, the rail badges the count, and one card
  // carries a person from "a machine is announcing" to "mia is an
  // operator" without changing screens. Then two tabs: the people who are
  // here, and the record of what everyone did.
  //
  // Nothing about the security model moved. Roles are still the five the
  // engine enforces, a grant is still a signed record, pairing still shows
  // six digits both humans compare, and every decision is still made on the
  // node being asked. Only the archaeology is gone.
  import { dialog } from "../lib/dialog";

  import {
    api,
    errText,
    parseDuration,
    type AuditView,
    type GrantView,
    type SessionPolicyView,
    type UserView,
  } from "../lib/api";
  import { app, ago, until, ui, initialsOf } from "../lib/state.svelte";
  import {
    clockOf,
    groupByDay,
    namesFrom,
    storyExport,
    storyOf,
  } from "../lib/story";
  import {
    RUNNERS,
    activeGrantsFor,
    capabilitiesOf,
    grantSentence,
    livePolicies,
    personalPolicyId,
    rolesOf,
    runnerScopes,
    summaryOf,
  } from "../lib/access";
  import Modal from "../lib/components/Modal.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Hint from "../lib/components/Hint.svelte";
  import LoadError from "../lib/components/LoadError.svelte";
  import RoleMatrix from "../lib/components/RoleMatrix.svelte";
  import GrantScopeEditor from "../lib/components/GrantScopeEditor.svelte";
  import SessionPolicyEditor from "../lib/components/SessionPolicyEditor.svelte";
  import JoinQueue from "../lib/components/JoinQueue.svelte";
  import InvitePanel from "../lib/components/InvitePanel.svelte";
  import TerminalAccess from "../lib/components/TerminalAccess.svelte";
  import AttestRoots from "../lib/components/AttestRoots.svelte";
  import { ROLE_SUMMARY, ROLES, roleHue } from "../lib/roles";
  import { hueOf, hueStyle } from "../lib/hues";
  import { portal } from "../lib/portal";
  import { staggerIn, tabline } from "../lib/motion";

  // Three tabs: the people here, the machines that act as principals, and
  // the record of what everyone did. Secrets went back to being its own
  // section.
  type Tab = "people" | "machines" | "history";
  let tab = $state<Tab>("people");
  const tabList = $derived.by((): [Tab, string][] => {
    // A count is left off while its list is refused (UX-2): "People · 0"
    // is a statement about the mesh that a failed read cannot make.
    const count = (err: string | undefined, n: number) => (err ? "" : ` · ${n}`);
    const t: [Tab, string][] = [
      ["people", `People${count(app.loadErrors.users, app.users.filter((u) => !u.revoked).length)}`],
      // E1-P4. Its own tab rather than rows mixed into People, because a
      // pipeline and a person are answers to different questions: one has
      // a clock and no laptop, the other has a laptop and no clock.
      ["machines", machinesLoaded ? `Machines · ${accounts.filter((a) => !a.revoked && !a.expired).length}` : "Machines"],
    ];
    t.push(["history", "History"]);
    return t;
  });

  /** The roles reference, as a dialog rather than a destination. */
  let rolesOpen = $state(false);

  const isAdmin = $derived(app.isAdmin);

  // -- machine principals (E1-P4) and identity providers (E1-P5) ---------------
  import type { ServiceAccountView } from "../lib/bindings/ServiceAccountView";
  import type { OidcProviderView } from "../lib/bindings/OidcProviderView";
  let accounts = $state<ServiceAccountView[]>([]);
  let providers = $state<OidcProviderView[]>([]);

  // -- a security key's proof, withdrawn (Y2/Y3) --------------------------------
  //
  // Somebody's own, or — as an admin — anybody's: a proof whose device was
  // lost, or whose authority turned out not to deserve the trust. Inline
  // rather than a dialog, because it takes nothing away but a badge.
  let withdrawFor = $state<string | null>(null);
  let withdrawErr = $state<string | null>(null);
  async function withdrawProof(u: UserView) {
    withdrawErr = null;
    try {
      await api.securityKeyWithdraw(u.userId === app.whoami?.userId ? null : u.userId, "withdrawn in Access");
      withdrawFor = null;
      app.invalidate();
      void app.refreshExtended();
    } catch (e) {
      withdrawErr = errText(e);
    }
  }
  let machinesErr = $state<string | null>(null);
  /** Until the first answer, the tab's count is unknown rather than 0. */
  let machinesLoaded = $state(false);
  async function loadMachines() {
    try {
      [accounts, providers] = await Promise.all([api.serviceAccounts(), api.oidcProviders()]);
      machinesErr = null;
      machinesLoaded = true;
    } catch (e) {
      machinesErr = errText(e);
    }
  }
  // Once on arrival, whichever tab opens (UX-19): the tab's label carries
  // the count, and it said "Machines · 0" until somebody clicked it.
  $effect(() => {
    void loadMachines();
  });

  // Creating one. The key comes back **once** and is written to a file the
  // operator picks; Director keeps no copy and meshware never had one.
  let newName = $state("");
  let newRole = $state("developer");
  let newNodes = $state("any");
  let newDays = $state<number | null>(90);
  let newDesc = $state("");
  let creating = $state(false);
  let createErr = $state<string | null>(null);
  let created = $state<{ name: string; userId: string; path: string } | null>(null);

  /**
   * What is still missing, field by field.
   *
   * The button used to be `disabled` until the name was filled in, which is
   * the failure the 2026-09 pass was raised on: a disabled button gives no
   * reason, no focus and no error — pressing it is literally indis­tinguish­able
   * from the feature being broken. The button is live now, and pressing it
   * with an empty field says which field and puts the caret in it.
   */
  const svcMissing = $derived.by((): { field: string; label: string; why: string }[] => {
    const out: { field: string; label: string; why: string }[] = [];
    if (!newName.trim()) {
      out.push({
        field: "svc-name",
        label: "Name",
        why: "name the system that will hold this key — ci-deploy, backup-runner",
      });
    } else if (!/^[a-z0-9][a-z0-9._-]*$/i.test(newName.trim())) {
      out.push({
        field: "svc-name",
        label: "Name",
        why: "letters, digits, dot, dash and underscore only — it is used as an identifier",
      });
    }
    if (!newDesc.trim()) {
      out.push({
        field: "svc-desc",
        label: "What for",
        why: "a credential nobody can place is a credential nobody dares revoke",
      });
    }
    return out;
  });
  /** Which fields have been pointed at, so they are only marked after a press. */
  let svcTouched = $state<string[]>([]);

  async function createAccount() {
    if (svcMissing.length > 0) {
      svcTouched = svcMissing.map((m) => m.field);
      createErr = null;
      // Focus the first gap: the message says which field, and the caret
      // goes there, so the fix is one keystroke away rather than a hunt.
      document.getElementById(svcMissing[0].field)?.focus();
      return;
    }
    svcTouched = [];
    creating = true;
    createErr = null;
    try {
      const made = await api.serviceAccountCreate(
        newName.trim(),
        newRole,
        newNodes.trim() || "any",
        "*",
        newDays,
        newDesc.trim(),
      );
      // Ask where to put the key *before* anything else can go wrong, and
      // refuse to continue without a destination: there is no second
      // chance at this value.
      const path = await api.saveKeyFile(`${made.name}.key`, made.key);
      if (!path) {
        createErr =
          "The account was created but its key was not saved, and it cannot be shown again. " +
          "Revoke it under People and create another.";
        await loadMachines();
        return;
      }
      created = { name: made.name, userId: made.userId, path };
      newName = "";
      newDesc = "";
      svcTouched = [];
      await loadMachines();
    } catch (e) {
      createErr = errText(e);
    } finally {
      creating = false;
    }
  }

  // -- key rotation (E2.3) -----------------------------------------------------
  let rotateFor = $state<string | null>(null);
  let rotateKey = $state("");
  let rotateBusy = $state(false);
  let rotateErr = $state<string | null>(null);
  async function rotate() {
    if (!rotateFor || !rotateKey.trim()) return;
    rotateBusy = true;
    rotateErr = null;
    try {
      const n = await api.userRotateKey(rotateFor, rotateKey.trim(), "key rotation from Director");
      app.flash(`${rotateFor}: key rotated, ${n} grant(s) carried over`);
      rotateFor = null;
      rotateKey = "";
      await app.refresh();
    } catch (e) {
      rotateErr = errText(e);
    } finally {
      rotateBusy = false;
    }
  }
  const canAudit = $derived(
    app.whoami?.isFounder || app.whoami?.roles.some((r) => r === "admin" || r === "auditor"),
  );

  // -- terminal policies -------------------------------------------------------
  //
  // No longer a tab: the per-person switch owns one policy each, and the
  // hand-written globs live in an "advanced" shelf at the foot of the page
  // for the meshes that already have them.
  let policies = $state<SessionPolicyView[]>([]);
  let policiesErr = $state<string | null>(null);
  let policyEditorOpen = $state(false);
  let editingPolicy = $state<SessionPolicyView | null>(null);
  let advancedOpen = $state(false);

  async function loadSessionPolicies() {
    try {
      policies = await api.sessionPolicies();
      policiesErr = null;
    } catch (e) {
      policiesErr = errText(e);
    }
  }
  $effect(() => {
    void loadSessionPolicies();
  });

  const live = $derived(livePolicies(policies));
  /** Policies written by hand for a group rather than by the per-person switch. */
  const globPolicies = $derived(
    live.filter((p) => !app.users.some((u) => personalPolicyId(u.username) === p.id)),
  );

  /** Which person's detail is open. */
  let openUser = $state<string | null>(null);
  /** People tab sub-view: the roster, or the flat grant list. */
  let peopleView = $state<"roster" | "grants">("roster");

  const roster = $derived([
    ...app.users.filter((u) => !u.revoked),
    ...app.users.filter((u) => u.revoked),
  ]);

  // -- add person (by key) ------------------------------------------------------
  //
  // The manual path, kept because it is the only one that works when
  // somebody hands you a key out of band. It is no longer the *first* path,
  // because it never should have been: almost nobody wants to paste a
  // 52-character public key, and almost everybody wants the queue above.
  let addUserOpen = $state(false);
  let inviteOpen = $state(false);
  let uName = $state("");
  let uKey = $state("");
  let uRole = $state("operator");
  let uNodes = $state("any");
  let uServices = $state<string[]>(["*"]);
  let uExpires = $state("");
  let uValid = $state(true);
  let uBusy = $state(false);
  let uErr = $state<string | null>(null);

  async function doAddUser() {
    uBusy = true;
    uErr = null;
    try {
      const expiresSecs = parseDuration(uExpires);
      const globs = uServices.length ? uServices : ["*"];
      await api.userAdd({
        username: uName.trim(),
        key: uKey.trim(),
        role: uRole,
        nodes: uNodes.trim() || "any",
        services: globs[0],
        expiresSecs,
      });
      // One grant per chosen script beyond the first (grants add up).
      for (const s of globs.slice(1)) {
        await api.grantAdd({
          user: uName.trim(),
          role: uRole,
          nodes: uNodes.trim() || "any",
          services: s,
          expiresSecs,
        });
      }
      app.flash(`${uName.trim()} added as ${uRole}`);
      addUserOpen = false;
      uName = uKey = uExpires = "";
      uNodes = "any";
      uServices = ["*"];
      await app.refreshAfterChange();
    } catch (e) {
      uErr = errText(e);
    } finally {
      uBusy = false;
    }
  }

  // -- add grant ----------------------------------------------------------------
  let addGrantOpen = $state(false);
  let gUser = $state("");
  let gRole = $state("operator");
  let gNodes = $state("any");
  let gServices = $state<string[]>(["*"]);
  let gExpires = $state("");
  let gValid = $state(true);
  let gBusy = $state(false);
  let gErr = $state<string | null>(null);

  function openGrant(user?: UserView) {
    gUser = user?.username ?? "";
    gNodes = "any";
    gServices = ["*"];
    gExpires = "";
    gErr = null;
    // One inline form at a time: three open at once was three forms to
    // read past to find the list they sit above.
    inviteOpen = false;
    addUserOpen = false;
    addGrantOpen = true;
    tab = "people";
  }

  async function doAddGrant() {
    gBusy = true;
    gErr = null;
    try {
      const expiresSecs = parseDuration(gExpires);
      const globs = gServices.length ? gServices : ["*"];
      let last = "";
      for (const s of globs) {
        last = await api.grantAdd({
          user: gUser.trim(),
          role: gRole,
          nodes: gNodes.trim() || "any",
          services: s,
          expiresSecs,
        });
      }
      app.flash(globs.length > 1 ? `${globs.length} grants added` : `grant ${last.slice(0, 18)} added`);
      addGrantOpen = false;
      gExpires = "";
      await app.refreshAfterChange();
    } catch (e) {
      gErr = errText(e);
    } finally {
      gBusy = false;
    }
  }

  // -- revocations (dark modals) ------------------------------------------------
  let revokeUser = $state<UserView | null>(null);
  let revokeUserTyped = $state("");
  let revokeUserReason = $state("revoked from Director");
  let revokeUserBusy = $state(false);
  let revokeUserErr = $state<string | null>(null);
  const revokeUserArmed = $derived(revokeUser !== null && revokeUserTyped.trim() === revokeUser.username);

  async function doRevokeUser() {
    if (!revokeUser || !revokeUserArmed) return;
    revokeUserBusy = true;
    revokeUserErr = null;
    try {
      await api.userRevoke(revokeUser.userId, revokeUserReason.trim());
      app.flash(`${revokeUser.username} revoked`);
      revokeUser = null;
      revokeUserTyped = "";
      await app.refreshAfterChange();
    } catch (e) {
      revokeUserErr = errText(e);
    } finally {
      revokeUserBusy = false;
    }
  }

  let revokeGrant = $state<GrantView | null>(null);
  let revokeGrantBusy = $state(false);
  let revokeGrantErr = $state<string | null>(null);
  async function doRevokeGrant() {
    if (!revokeGrant) return;
    revokeGrantBusy = true;
    revokeGrantErr = null;
    try {
      await api.grantRevoke(revokeGrant.grantId);
      app.flash(`grant ${revokeGrant.grantId.slice(0, 18)} revoked`);
      revokeGrant = null;
      await app.refreshAfterChange();
    } catch (e) {
      revokeGrantErr = errText(e);
    } finally {
      revokeGrantBusy = false;
    }
  }

  // -- history (the audit log) ---------------------------------------------------
  let aUser = $state("");
  let aAction = $state("");
  let aSince = $state("24h");
  let aLimit = $state(100);
  let aRows = $state<AuditView[]>([]);
  let aBusy = $state(false);
  let aErr = $state<string | null>(null);
  let aRan = $state(false);

  /**
   * Story or table.
   *
   * The table is the record: seven columns, every field verbatim, nothing
   * resolved — which is what you want when you are checking a signature or
   * exporting a month. The story is the same rows read aloud, and it is the
   * default because the question people bring to this screen is "what
   * happened", not "what does `grant.add` mean". Neither view fetches
   * anything the other does not; the toggle is a rendering choice.
   */
  let aMode = $state<"story" | "table">("story");

  /** Keys → the names people use for them, for the sentences. */
  const aNames = $derived(namesFrom(app.users, app.nodes));
  const aDays = $derived(groupByDay(aRows));

  /** A row that was refused or wants a look gets a dot in the solid status
   *  colour; the refusal clause itself takes the ink step, which is the one
   *  that clears contrast as text. */
  const toneDot = (tone: string) =>
    tone === "refused" ? "var(--status-error)" : "var(--status-warn)";
  const REFUSAL_INK = "color: var(--status-error-ink);";

  async function copyStory() {
    if (aRows.length === 0) return;
    try {
      await api.copyText(storyExport(aRows, aNames));
      app.flash(`Copied ${aRows.length} event${aRows.length === 1 ? "" : "s"} as text`);
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }

  async function runAudit() {
    aBusy = true;
    aErr = null;
    try {
      const sinceSecs = aSince === "all" ? null : parseDuration(aSince);
      aRows = await api.audit({
        user: aUser.trim() || null,
        action: aAction.trim() || null,
        sinceSecs,
        limit: Math.max(1, Math.min(1000, aLimit)),
      });
      aRan = true;
    } catch (e) {
      aErr = errText(e);
    } finally {
      aBusy = false;
    }
  }

  $effect(() => {
    if (tab === "history" && !aRan && canAudit) void runAudit();
    // E1-P4/E1-P5: loaded when the tab is opened rather than on every
    // refresh — a service-account list is not something a person is
    // watching change, and the Access view already fetches plenty.
    if (tab === "machines") void loadMachines();
  });

  /** Jump to this person's history, from their row. */
  function historyFor(username: string) {
    aUser = username;
    aAction = "";
    aSince = "30d";
    tab = "history";
    void runAudit();
  }

  // The action prefixes worth offering, with what they mean. A datalist of
  // bare prefixes assumed the reader already knew the event taxonomy.
  const ACTION_PRESETS: [string, string][] = [
    ["", "everything"],
    ["job.", "work that was submitted, ran or was cancelled"],
    ["service.", "scripts added, changed or removed"],
    ["secret.", "secret values set, resolved or deleted"],
    ["user.", "people added or revoked"],
    ["grant.", "roles given or taken away"],
    ["node.", "machines joining, draining or revoked"],
    ["session.", "terminals opened and closed"],
    ["invite.", "invites minted and used"],
  ];

  async function copy(text: string, what: string) {
    await api.copyText(text);
    app.flash(`${what} copied`);
  }

  // -- deep links ----------------------------------------------------------------
  $effect(() => {
    // Read first, so this re-runs on every `app.go` rather than only at
    // mount: the bell sets the key and navigates to a section that may
    // already be open, and `#step` is a no-op in that case.
    void app.deepLink;
    const t = sessionStorage.getItem("access:tab");
    if (t) {
      sessionStorage.removeItem("access:tab");
      // Links from older builds name tabs this screen no longer has. Each
      // lands on whatever answers the same question now, rather than on a
      // blank view: "roles" is a dialog, "terminals" is a control on a
      // person's row, "audit" was renamed, and secrets is a section again.
      if (t === "roles") rolesOpen = true;
      else if (t === "secrets") app.go("secrets");
      else if (t === "terminals") {
        tab = "people";
        advancedOpen = true;
      } else if (t === "audit") tab = "history";
      // Change control and approvals were removed; a link naming either
      // lands on People rather than on a tab that is not there.
      else if (["people", "machines", "history"].includes(t)) tab = t as Tab;
      else tab = "people";
    }
    const o = sessionStorage.getItem("access:open");
    if (o) {
      sessionStorage.removeItem("access:open");
      if (o === "add-user") {
        tab = "people";
        addUserOpen = true;
      } else if (o === "invite") {
        tab = "people";
        inviteOpen = true;
      }
    }
  });

  const outcomeStyle = (o: string) =>
    o === "ok"
      ? "color: var(--accent-ink);"
      : o.startsWith("denied")
        ? "color: var(--status-error-ink);"
        : "color: var(--status-warn-ink);";

  /**
   * A name, in that name's own colour (`lib/hues.ts`).
   *
   * The history is four hundred sentences that each open with somebody's
   * name, and in one ink they all open the same way: "what did mia do on
   * Tuesday" means reading every line to find out where mia is. The rail
   * already hashes a name to one of the six companion hues for its
   * avatars, so reusing that hash costs nothing, needs nobody to pick a
   * colour, and makes a person the same colour in the log as they are in
   * the sidebar — which is what turns reading into recognising.
   *
   * `--hue-ink` and not `--hue`: the fill is chosen to sit *under*
   * something, and the ink is the one carrying 4.5:1 as text.
   *
   * Machine names take a colour too. `story.ts` marks the actor and the
   * thing acted upon with the same kind of piece, both are identities the
   * mesh knows by name, and giving one a colour while the other stays grey
   * would say they are different kinds of word when they are not.
   *
   * **Coral is swapped out, here and nowhere else.** Everywhere else in
   * Director a person's colour is a filled chip; in this log it is text,
   * in the one column where red already means *refused* — so a name that
   * happened to hash to coral wore the refusal colour on rows where
   * nothing was refused, and `lab-op ran the backup` read as an alarm that
   * was not there. Only coral-hashed names move, so everybody else is
   * still the exact colour of their avatar; plum is the neighbour that
   * takes them, being the nearest hue no status has claimed.
   */
  /** Your grants, less the admin grant a founder's chip already states. */
  const shownGrants = $derived(
    (app.whoami?.grants ?? []).filter((g) => !(app.whoami?.isFounder && g.role === "admin")),
  );

  const nameInk = (name: string) => {
    const h = hueOf(name);
    return `${hueStyle(h === "coral" ? "plum" : h)} color: var(--hue-ink);`;
  };
</script>

<div class="fade-in space-y-4">
  <!-- ═══════════ who you are ═══════════ -->
  {#if app.whoami}
    <div class="card px-[22px] py-4">
      <div class="flex flex-wrap items-center gap-x-4 gap-y-2 text-sm">
        <span class="label">You</span>
        <span class="font-semibold text-ink">{app.whoami.username ?? "(not recognised yet)"}</span>
        {#if app.whoami.isFounder}
          <span
            class="rounded-full px-2 py-0.5 text-2xs font-semibold"
            style="background: var(--accent-wash); color: var(--accent-ink);"
            title="You started this mesh, so you are an admin everywhere — no grant required."
          >
            founder · admin everywhere
          </span>
        {/if}
        <!-- "admin" said once (UX-19): a founder's admin role chip and admin
             grant sentence repeated what the founder chip already says. -->
        {#each app.whoami.roles.filter((r) => !(app.whoami?.isFounder && r === "admin")) as r}
          <span class="hue-chip" style={hueStyle(roleHue(r))} title={ROLE_SUMMARY[r]?.long ?? ""}>{r}</span>
        {/each}
        <button
          class="mono text-2xs text-ink-5 hover:text-accent-ink"
          title="copy your key — this is what identifies you, everywhere"
          onclick={() => copy(app.whoami?.userId ?? "", "your key")}
        >
          {app.whoami.userId.slice(0, 22)}… copy
        </button>
        {#if shownGrants.length}
          <span class="ml-auto text-xs text-ink-5">
            {#each shownGrants as g (g.grantId)}
              <span class="ml-1.5 text-ink-3">{grantSentence(g)}</span>
            {/each}
          </span>
        {/if}
      </div>
      {#if !app.whoami.registered && !app.whoami.isFounder}
        <div class="mt-2 text-sm" style="color: var(--status-warn-ink);">
          {#if app.whoami.enrolment === "pending"}
            You have asked to join and an admin has not decided yet. They see your request the
            moment they open Meshware Director — show them
            <span class="mono">{app.whoami.enrolmentCode}</span>, which is the same code they see.
          {:else}
            This mesh does not know your key yet. Use <strong>Request access</strong> in the
            banner at the top of the window — it puts you in front of an admin, with a code you
            can read aloud.
          {/if}
        </div>
      {/if}
    </div>
  {/if}

  <!-- ═══════════ people and machines waiting to be let in ═══════════ -->
  {#if isAdmin}
    <JoinQueue onchanged={loadSessionPolicies} />
  {/if}

  <!-- ═══════════ tabs ═══════════ -->
  <div class="flex flex-wrap items-stretch gap-3" style="border-bottom: 1px solid var(--edge);">
    <div class="tabrow" role="tablist" aria-label="Access" use:tabline>
      {#each tabList as [id, label] (id)}
        <button class="tab" role="tab" aria-selected={tab === id} onclick={() => (tab = id)}>
          {label}
        </button>
      {/each}
    </div>
    <div class="ml-auto flex items-center gap-2 self-center pb-1.5">
      {#if tab === "people" && !isAdmin}
        <span class="text-xs text-ink-5">only an admin can change this</span>
      {/if}
      {#if tab === "people"}
        <button class="linkish mr-1 text-xs" onclick={() => (rolesOpen = true)}>
          What can each role do?
        </button>
        <button class={ui.secondary} disabled={!isAdmin} onclick={() => { inviteOpen = !inviteOpen; addUserOpen = false; addGrantOpen = false; }}>
          Invite someone
        </button>
        <button class={ui.primary} disabled={!isAdmin} onclick={() => openGrant()}>Grant a role</button>
      {/if}
    </div>
  </div>

  <!-- ═════════════════════════ people ═════════════════════════ -->
  {#if tab === "people"}
    {#if inviteOpen}
      <InvitePanel onclose={() => (inviteOpen = false)} />
    {/if}

    {#if addGrantOpen}
      <div class="fade-in card p-[22px]">
        <div class="flex flex-wrap items-baseline gap-3">
          <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">Grant a role</h2>
          <span class="text-sm text-ink-4">grants add up — this one joins whatever they already have</span>
          <button class="ml-auto text-sm font-semibold text-ink-4 hover:text-ink" onclick={() => (addGrantOpen = false)}>
            Close
          </button>
        </div>

        <div class="mt-4 grid gap-3 lg:grid-cols-3">
          <label class="block">
            <span class={ui.label}>Who</span>
            <input
              bind:value={gUser}
              list="user-names"
              placeholder="a name on this mesh"
              class={ui.inputMono}
            />
            <datalist id="user-names">
              {#each app.users.filter((u) => !u.revoked) as u (u.userId)}<option value={u.username}></option>{/each}
            </datalist>
          </label>
        </div>

        <div class="mt-4">
          <GrantScopeEditor
            bind:role={gRole}
            bind:nodes={gNodes}
            bind:services={gServices}
            bind:expires={gExpires}
            bind:valid={gValid}
            subject={gUser.trim() || "They"}
          />
        </div>

        {#if gErr}<div class={ui.errorBox}>{gErr}</div>{/if}
        <div class="mt-4">
          <button class={ui.primary} disabled={gBusy || !gUser.trim() || !gValid} onclick={doAddGrant}>
            {gBusy ? "Granting…" : "Add grant"}
          </button>
        </div>
      </div>
    {/if}

    {#if addUserOpen}
      <div class="fade-in card p-[22px]">
        <div class="flex flex-wrap items-baseline gap-3">
          <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">Add someone by key</h2>
          <span class="text-sm text-ink-4">
            when they cannot reach this mesh yet and have sent you their key instead
          </span>
          <button class="ml-auto text-sm font-semibold text-ink-4 hover:text-ink" onclick={() => (addUserOpen = false)}>
            Close
          </button>
        </div>

        <div class="mt-4 grid gap-3 lg:grid-cols-3" use:staggerIn={{ each: 40, y: 8 }}>
          <label class="block">
            <span class={ui.label}>Call them</span>
            <input bind:value={uName} placeholder="alice" class={ui.inputMono} />
          </label>
          <label class="block lg:col-span-2">
            <span class={ui.label}>
              Their key
              <span class="text-ink-6">
                — they read it off the bottom of their own welcome screen, or run
                <span class="mono">meshnode whoami</span>
              </span>
            </span>
            <input bind:value={uKey} placeholder="mwu1…" class={ui.inputMono} />
          </label>
        </div>

        <div class="mt-4">
          <GrantScopeEditor
            bind:role={uRole}
            bind:nodes={uNodes}
            bind:services={uServices}
            bind:expires={uExpires}
            bind:valid={uValid}
            subject={uName.trim() || "They"}
          />
        </div>

        {#if uErr}<div class={ui.errorBox}>{uErr}</div>{/if}
        <div class="mt-4 flex flex-wrap items-center gap-2">
          <button
            class={ui.primary}
            disabled={uBusy || !uName.trim() || !uKey.trim() || !uValid}
            onclick={doAddUser}
          >
            {uBusy ? "Adding…" : "Add them"}
          </button>
          <span class="text-xs text-ink-5">
            Their machine still has to join the mesh separately — send them an invite for that.
          </span>
        </div>
      </div>
    {/if}

    <!-- The roster, or the flat grant list for auditing one scope across
         everybody who holds it. -->
    <div class="flex flex-wrap items-center gap-3">
      <div class="inline-flex rounded-md bg-surface-2 p-[3px]">
        {#each [["roster", "By person"], ["grants", app.loadErrors.grants ? "Every grant" : `Every grant · ${app.grants.filter((g) => !g.revoked && !g.expired).length}`]] as [id, label] (id)}
          <button
            class={`rounded-sm px-[11px] py-[5px] text-xs transition-colors ${peopleView === id ? "bg-surface font-semibold text-ink" : "font-medium text-ink-5 hover:text-ink"}`}
            style={peopleView === id ? "box-shadow: var(--sh-control);" : ""}
            onclick={() => (peopleView = id as "roster" | "grants")}
          >
            {label}
          </button>
        {/each}
      </div>
      <span class="text-xs text-ink-5">
        {peopleView === "roster"
          ? "select someone to see exactly what they can do, and where"
          : "one row per grant — for checking a scope across everyone who holds it"}
      </span>
    </div>

    {#if app.loadErrors.users}
      <LoadError what="the people on this mesh" error={app.loadErrors.users} />
    {:else if app.loadErrors.grants}
      <LoadError what="who holds which role" error={app.loadErrors.grants} />
    {/if}

    {#if peopleView === "roster"}
      <div class="flex flex-col gap-2">
        {#each roster as u (u.userId)}
          {@const roles = rolesOf(app.grants, u.userId, u.isFounder)}
          {@const grants = activeGrantsFor(app.grants, u.userId)}
          {@const caps = capabilitiesOf(u, app.grants, policies)}
          {@const scopes = runnerScopes(app.grants, u.userId)}
          {@const isOpen = openUser === u.userId}
          <div class="card overflow-hidden transition-opacity" class:opacity-50={u.revoked}>
            <button
              class="flex w-full items-center gap-3 px-5 py-3.5 text-left transition-colors hover:bg-surface-2"
              aria-expanded={isOpen}
              onclick={() => (openUser = isOpen ? null : u.userId)}
            >
              <span class="text-ink-6 transition-transform" style={isOpen ? "transform: rotate(90deg);" : ""}>
                <Icon name="chevron-right" size={12} />
              </span>
              <!-- The person's own colour, hashed from their name — the same
                   one the rail shows them in on their own machine. -->
              <span class="avatar avatar-md" style={hueStyle(hueOf(u.username))} class:opacity-60={u.revoked}>{initialsOf(u.username)}</span>
              <span class="min-w-0">
                <span class="flex flex-wrap items-center gap-2">
                  <span class="font-semibold text-ink">{u.username}</span>
                  {#if u.userId === app.whoami?.userId}
                    <span class="rounded-full px-2 py-0.5 text-2xs font-semibold" style="background: var(--accent-wash); color: var(--accent-ink);">you</span>
                  {/if}
                  {#if u.isFounder}
                    <span class="rounded-full px-2 py-0.5 text-2xs font-semibold" style="background: var(--accent-wash); color: var(--accent-ink);">founder</span>
                  {/if}
                  {#if u.hardware && !u.hardware.revoked}
                    <!-- Y2/Y3: proven, not claimed — every node checked the
                         device's certificate against a trusted maker. -->
                    <span
                      class="rounded-full bg-surface-2 px-2 py-0.5 text-2xs font-semibold text-ink-3"
                      title="Their key was made on a security key and cannot be copied off it — {u.hardware.kind.toUpperCase()} slot {u.hardware.slot}, vouched for by {u.hardware.rootLabel}{u.hardware.subject ? ` (${u.hardware.subject})` : ''}"
                    >security key</span>
                  {/if}
                  {#if u.revoked}
                    <span class="rounded-full px-2 py-0.5 text-2xs font-semibold" style="background: var(--status-error-wash); color: var(--status-error-ink);">revoked</span>
                  {/if}
                </span>
                <span class="mt-0.5 block text-xs text-ink-5">
                  {#if u.revoked}
                    their key is refused everywhere
                  {:else}
                    {summaryOf(caps)}
                  {/if}
                </span>
              </span>
              <span class="ml-auto flex items-center gap-2">
                {#each roles as r (r)}
                  <span class="hue-chip" style={hueStyle(roleHue(r))} title={ROLE_SUMMARY[r]?.long ?? ""}>{r}</span>
                {/each}
                <span class="num flex-none text-xs whitespace-nowrap text-ink-6">{ago(u.createdAtMicros)}</span>
              </span>
            </button>

            {#if isOpen}
              <div class="border-t border-edge">
                <div class="grid gap-px sm:grid-cols-2" style="background: var(--edge);">
                  <!-- What they can do, in sentences rather than role names. -->
                  <div class="bg-surface px-5 py-4">
                    <div class="label">What {u.username} can do</div>
                    <div class="mt-2.5 flex flex-col gap-2">
                      {#each caps as c (c.what)}
                        <div class="flex items-start gap-2.5">
                          {#if c.yes}
                            <span class="mt-[3px] inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full" style="background: var(--accent-wash); color: var(--accent-ink);">
                              <Icon name="check" size={9} />
                            </span>
                          {:else}
                            <span class="mt-[3px] inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-surface-2 text-ink-6">·</span>
                          {/if}
                          <span class="min-w-0">
                            <span class="text-sm" class:text-ink={c.yes} class:text-ink-5={!c.yes}>{c.what}</span>
                            <span class="mt-px block text-xs leading-relaxed text-ink-5">{c.why}</span>
                          </span>
                        </div>
                      {/each}
                    </div>

                    {#if !u.revoked}
                      <!-- The terminal switch, where the question is asked. -->
                      <div class="mt-3.5 border-t border-edge pt-3.5">
                        <TerminalAccess
                          username={u.username}
                          {policies}
                          canRunWork={u.isFounder || roles.some((r) => RUNNERS.includes(r))}
                          unrestricted={u.isFounder || scopes.some((s) => s.services === "*")}
                          disabled={!isAdmin}
                          onchanged={loadSessionPolicies}
                        />
                      </div>
                    {/if}
                  </div>

                  <!-- The grants those sentences came from. -->
                  <div class="bg-surface px-5 py-4">
                    <div class="flex flex-wrap items-baseline gap-2">
                      <div class="label">Roles they hold</div>
                      <span class="text-xs text-ink-5">they add up</span>
                      {#if !u.revoked}
                        <button class="linkish ml-auto text-xs" disabled={!isAdmin} onclick={() => openGrant(u)}>
                          Give them another…
                        </button>
                      {/if}
                    </div>
                    {#if grants.length === 0}
                      <p class="mt-2 mb-0 text-sm text-ink-5">
                        {u.isFounder
                          ? "The founder needs none — starting the mesh makes them an admin everywhere."
                          : "None. They are a known key that can do nothing yet."}
                      </p>
                    {:else}
                      <div class="mt-2 flex flex-col gap-2">
                        {#each grants as g (g.grantId)}
                          <div class="rounded-md bg-surface-2 px-3 py-2">
                            <div class="flex flex-wrap items-center gap-2 text-xs">
                              <!-- A sentence, not two globs. -->
                              <span class="text-ink-2">{grantSentence(g)}</span>
                              <span class="num ml-auto text-ink-5">{until(g.expiresAtMicros)}</span>
                              <button
                                class="linkish text-xs"
                                style="color: var(--status-error-ink);"
                                disabled={!isAdmin}
                                onclick={() => { revokeGrantErr = null; revokeGrant = g; }}
                              >
                                take away
                              </button>
                            </div>
                          </div>
                        {/each}
                      </div>
                    {/if}

                    <div class="mt-3 flex flex-wrap items-center gap-3 border-t border-edge pt-3">
                      <button class="linkish text-xs" onclick={() => historyFor(u.username)}>
                        What have they done?
                      </button>
                      <button
                        class="mono text-xs text-ink-5 hover:text-accent-ink"
                        title="copy their key"
                        onclick={() => copy(u.userId, "key")}
                      >
                        {u.shortId} · copy
                      </button>
                      {#if u.hardware && !u.hardware.revoked && (isAdmin || u.userId === app.whoami?.userId)}
                        {#if withdrawFor === u.userId}
                          <span class="flex items-center gap-2 text-xs">
                            <span class="text-ink-4">Their key stops reading as held on hardware.</span>
                            <button class={ui.rowDanger} onclick={() => withdrawProof(u)}>Withdraw</button>
                            <button class={ui.rowAction} onclick={() => (withdrawFor = null)}>Cancel</button>
                          </span>
                        {:else}
                          <button class="linkish text-xs" onclick={() => { withdrawErr = null; withdrawFor = u.userId; }}>
                            Withdraw the security-key proof…
                          </button>
                        {/if}
                        {#if withdrawErr && withdrawFor === u.userId}<span class="text-xs text-error">{withdrawErr}</span>{/if}
                      {/if}
                      {#if !u.revoked && !u.isFounder}
                        <!-- E2.3. Beside Remove because they are the two
                             things you do about a lost laptop, and they
                             are very different: this one keeps the person
                             and replaces their key, that one ends them. -->
                        <button
                          class="linkish ml-auto text-xs"
                          disabled={!isAdmin}
                          onclick={() => { rotateErr = null; rotateKey = ""; rotateFor = u.username; }}
                        >
                          Replace key…
                        </button>
                        <button
                          class="text-xs font-semibold"
                          style="color: var(--status-error);"
                          disabled={!isAdmin}
                          onclick={() => { revokeUserErr = null; revokeUserTyped = ""; revokeUser = u; }}
                        >
                          Remove them…
                        </button>
                      {/if}
                    </div>
                  </div>
                </div>
              </div>
            {/if}
          </div>
        {:else}
          <!-- Not while the list is in error (UX-2): "just you" is a claim
               about the mesh, and a refused read cannot back it. -->
          {#if !app.loadErrors.users}
            <div class="card empty">
              <div class="empty-title">It is just you so far</div>
              <div class="empty-sub">
                Nothing here needs setting up until somebody else should be allowed in.
              </div>
            </div>
          {/if}
        {/each}
      </div>

      <!-- The one paragraph this screen still owes a first-time reader.
           At the bottom, because it answers a question you have after
           looking rather than before. -->
      <div class="card px-[22px] py-4">
        <p class="m-0 max-w-3xl text-sm leading-relaxed text-ink-4">
          <strong>There are no passwords here.</strong> A person is a key on their own machine;
          everything they do is signed by it, and what it may do can be changed or taken away at any moment.
          <Hint
            label="How someone joins"
            text="Three ways, and the first covers most of them. (1) They install Meshware Director on a machine on your network and choose 'Pair on this network' — they appear at the top of this page and you name them and give them a role. (2) They are somewhere else: send them an invite from the button above, and they turn up here the same way once their machine is in. (3) You already have their key: 'Add someone by key' below, which is the only path that does not need them online."
          />
          {#if isAdmin}
            <button class="linkish ml-1 text-sm" onclick={() => { addUserOpen = true; inviteOpen = false; addGrantOpen = false; }}>
              Add someone by key…
            </button>
          {/if}
        </p>
      </div>

      <!-- Hand-written terminal rules. An escape hatch, one line high until
           somebody has one or goes looking. -->
      {#if isAdmin && (globPolicies.length > 0 || advancedOpen)}
        <div class="card overflow-hidden">
          <button
            class="flex w-full items-center gap-2 px-5 py-3 text-left transition-colors hover:bg-surface-2"
            aria-expanded={advancedOpen}
            onclick={() => (advancedOpen = !advancedOpen)}
          >
            <span class="text-ink-6 transition-transform" style={advancedOpen ? "transform: rotate(90deg);" : ""}>
              <Icon name="chevron-right" size={12} />
            </span>
            <span class="text-sm font-semibold text-ink">Terminal rules written for groups</span>
            <span class="text-xs text-ink-5">
              {globPolicies.length === 0
                ? "none — the switch on each person covers the usual case"
                : `${globPolicies.length} rule${globPolicies.length === 1 ? "" : "s"} matching a pattern like ops-*`}
            </span>
          </button>

          {#if advancedOpen}
            <div class="fade-in border-t border-edge px-5 py-4">
              {#if policyEditorOpen}
                <SessionPolicyEditor
                  existing={editingPolicy}
                  onclose={() => (policyEditorOpen = false)}
                  onsaved={async () => {
                    policyEditorOpen = false;
                    app.flash("terminal rule published");
                    await loadSessionPolicies();
                  }}
                />
              {/if}

              {#if policiesErr}
                <div class="text-sm" style="color: var(--status-error-ink);">{policiesErr}</div>
              {:else if globPolicies.length === 0}
                <p class="m-0 text-sm leading-relaxed text-ink-5">
                  Nothing here. Write one only when a rule should follow a <em>group</em> rather
                  than a person — <span class="mono">ops-*</span> on the machines tagged
                  <span class="mono">dev</span>, say — so that people joining that group get it
                  without anybody remembering to grant it.
                </p>
              {:else}
                <div class="flex flex-col gap-2">
                  {#each globPolicies as p (p.id)}
                    <div class="flex flex-wrap items-center gap-3 rounded-lg bg-surface-2 px-3.5 py-2.5">
                      <span class="min-w-0">
                        <span class="mono block text-sm font-semibold text-ink">{p.id}</span>
                        <span class="mt-0.5 block text-xs text-ink-5">
                          <span class="mono">{p.users}</span> may open a terminal on
                          <span class="mono">{p.nodes}</span>
                          {#if p.allow.includes("shell-privileged")}, root included where that machine permits it{/if}
                          {#if p.description} — {p.description}{/if}
                        </span>
                      </span>
                      <span class="num ml-auto text-xs text-ink-5">{until(p.expiresAtMicros)}</span>
                      <button class={ui.rowAction} onclick={() => { editingPolicy = p; policyEditorOpen = true; }}>
                        Edit…
                      </button>
                      <button
                        class={ui.rowDanger}
                        onclick={async () => {
                          try {
                            await api.sessionPolicyRemove(p.id);
                            app.flash(`terminal rule '${p.id}' retired`);
                            await loadSessionPolicies();
                          } catch (e) {
                            app.flash(errText(e));
                          }
                        }}
                      >
                        Retire
                      </button>
                    </div>
                  {/each}
                </div>
              {/if}

              {#if !policyEditorOpen}
                <button
                  class={`${ui.secondary} mt-3`}
                  onclick={() => { editingPolicy = null; policyEditorOpen = true; }}
                >
                  New rule for a group
                </button>
              {/if}
            </div>
          {/if}
        </div>
      {:else if isAdmin}
        <button class="linkish text-xs" onclick={() => (advancedOpen = true)}>
          Write a terminal rule for a group instead…
        </button>
      {/if}
    {:else}
      <div class="card overflow-hidden">
        <table class="w-full text-left text-base">
          <thead>
            <tr class="border-b border-edge">
              <th class="label px-5 py-3">Grant</th>
              <th class="label px-3 py-3">Who</th>
              <th class="label px-3 py-3">Role</th>
              <th class="label px-3 py-3">Machines</th>
              <th class="label px-3 py-3">Scripts</th>
              <th class="label px-3 py-3">Expires</th>
              <th class="label px-3 py-3">State</th>
              <th class="label px-3 py-3 pr-5 text-right"></th>
            </tr>
          </thead>
          <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
            {#each app.grants as g (g.grantId)}
              {@const dead = g.revoked || g.expired}
              <tr class={`h-12 border-b border-edge transition-colors hover:bg-surface-2 last:border-0 ${dead ? "opacity-45" : ""}`}>
                <td class="mono px-5 py-2.5 text-xs text-ink-4">{g.grantId.slice(0, 18)}</td>
                <td class="px-3 py-2.5 font-semibold text-ink">{g.username}</td>
                <td class="px-3 py-2.5"><span class="hue-chip" style={hueStyle(roleHue(g.role))}>{g.role}</span></td>
                <td class="mono px-3 py-2.5 text-xs text-ink-3">{g.nodes}</td>
                <td class="mono px-3 py-2.5 text-xs text-ink-3">{g.services}</td>
                <td class="num px-3 py-2.5 text-sm text-ink-4">{until(g.expiresAtMicros)}</td>
                <td class="px-3 py-2.5 text-sm">
                  {#if g.revoked}<span class="font-medium text-error">revoked</span>
                  {:else if g.expired}<span class="text-ink-5">expired</span>
                  {:else}<span style="color: var(--accent-ink);">active</span>{/if}
                </td>
                <td class="px-3 py-2.5 pr-5 text-right">
                  {#if !dead}
                    <button class={ui.rowDanger} disabled={!isAdmin} onclick={() => { revokeGrantErr = null; revokeGrant = g; }}>
                      Revoke
                    </button>
                  {/if}
                </td>
              </tr>
            {:else}
              {#if !app.loadErrors.grants}
                <tr><td colspan="8" class="px-5 py-8 text-center text-sm text-ink-5">No grants yet.</td></tr>
              {/if}
            {/each}
          </tbody>
        </table>
      </div>
    {/if}

  <!-- ═════════════════════ machines (E1-P4 / E1-P5) ═══════════════════════ -->
  {:else if tab === "machines"}
    <div class="card px-[22px] py-4">
      <div class="flex items-baseline gap-2">
        <h2 class="m-0 text-base font-semibold">Service accounts</h2>
        <Hint
          text="A pipeline should not authenticate as a person. A service account is a key that belongs to a system: same signed requests, same scoped roles, plus a clock of its own."
          label="What these are"
        />
      </div>
      <p class="mt-1 mb-0 text-xs leading-relaxed text-ink-5">
        Two clocks, and they differ. A <em>grant</em> expiring leaves a live key with no
        roles, which you can re-grant. An <em>account</em> expiring is terminal for that
        credential — re-granting does nothing. That is the one you want on a CI token.
      </p>

      {#if machinesErr}<div class="mt-2 text-xs text-error">{machinesErr}</div>{/if}

      {#if accounts.length === 0}
        <p class="mt-3 mb-0 text-sm text-ink-4">
          None yet. Anything automated — CI, a deploy pipeline, a monitoring system —
          should have one of these rather than a copy of somebody's key.
        </p>
      {:else}
        <div class="tablewrap mt-3">
          <table class="w-full text-sm">
            <thead>
              <tr>
                <th class="label text-left">NAME</th>
                <th class="label text-left">ROLES</th>
                <th class="label text-left">STATE</th>
                <th class="label text-left">EXPIRES</th>
                <th class="label text-left">FOR</th>
              </tr>
            </thead>
            <tbody>
              {#each accounts as a (a.userId)}
                <tr>
                  <td class="num">{a.name}</td>
                  <td>{a.roles.length ? a.roles.join(", ") : "—"}</td>
                  <td>
                    {#if a.revoked}
                      <span style="color: var(--status-error-ink);">revoked</span>
                    {:else if a.expired}
                      <span style="color: var(--status-warn-ink);">expired</span>
                    {:else}
                      <span style="color: var(--accent-ink);">active</span>
                    {/if}
                  </td>
                  <td class="num text-xs">
                    {a.expiresAtMicros == null
                      ? "never"
                      : new Date(Number(a.expiresAtMicros) / 1000).toLocaleDateString()}
                  </td>
                  <td class="text-xs text-ink-5">{a.description || "—"}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}

      {#if isAdmin}
        <!-- Create one.
             Two things were wrong here and they compounded. The fields
             carried `class="input"`, which is not a class this app defines —
             `ui.input` is — so five controls rendered as raw platform
             widgets in the middle of a designed form. And the button was
             disabled until the name was filled in, so the one person most
             likely to press it first found a control that did nothing and
             said nothing. Both are the same mistake: a form that does not
             answer back. -->
        <div class="mt-4 rounded-lg px-4 py-3.5" style="background: var(--surface-subtle); box-shadow: inset 0 0 0 1px var(--edge);">
          <div class="label" style="color: var(--ink-3); font-weight: 600;">Create one</div>

          <div class="mt-3 grid gap-x-3 gap-y-3" style="grid-template-columns: repeat(auto-fit, minmax(168px, 1fr));">
            <label class="block">
              <span class={ui.label}>Name</span>
              <input
                id="svc-name"
                class={ui.input}
                class:is-missing={svcTouched.includes("svc-name") && svcMissing.some((m) => m.field === "svc-name")}
                placeholder="ci-deploy"
                autocomplete="off"
                spellcheck="false"
                aria-invalid={svcTouched.includes("svc-name") && svcMissing.some((m) => m.field === "svc-name")}
                bind:value={newName}
                oninput={() => (svcTouched = svcTouched.filter((f) => f !== "svc-name"))}
                onkeydown={(e) => { if (e.key === "Enter") createAccount(); }}
              />
            </label>

            <label class="block">
              <span class={ui.label}>Role</span>
              <select class="{ui.input} select" bind:value={newRole}>
                <option value="operator">operator</option>
                <option value="developer">developer</option>
                <option value="auditor">auditor</option>
                <option value="viewer">viewer</option>
              </select>
            </label>

            <label class="block">
              <span class={ui.label}>On <span class="text-ink-6">(which nodes)</span></span>
              <input class={ui.input} placeholder="any" autocomplete="off" spellcheck="false" bind:value={newNodes} />
            </label>

            <label class="block">
              <span class={ui.label}>Expires in</span>
              <select
                class="{ui.input} select"
                value={newDays == null ? "never" : String(newDays)}
                onchange={(e) => {
                  const v = (e.currentTarget as HTMLSelectElement).value;
                  newDays = v === "never" ? null : Number(v);
                }}
              >
                <option value="30">30 days</option>
                <option value="90">90 days</option>
                <option value="365">1 year</option>
                <option value="never">never</option>
              </select>
            </label>

            <label class="block" style="grid-column: 1 / -1;">
              <span class={ui.label}>What for</span>
              <input
                id="svc-desc"
                class={ui.input}
                class:is-missing={svcTouched.includes("svc-desc") && svcMissing.some((m) => m.field === "svc-desc")}
                placeholder="GitHub Actions deploy"
                autocomplete="off"
                aria-invalid={svcTouched.includes("svc-desc") && svcMissing.some((m) => m.field === "svc-desc")}
                bind:value={newDesc}
                oninput={() => (svcTouched = svcTouched.filter((f) => f !== "svc-desc"))}
                onkeydown={(e) => { if (e.key === "Enter") createAccount(); }}
              />
            </label>
          </div>

          <!-- Live, once a press has asked for it. Not a `disabled` button:
               the whole point is that it tells you what it wants. -->
          {#if svcTouched.length > 0 && svcMissing.length > 0}
            <div
              class="mt-3 rounded-md px-3 py-2.5 text-xs leading-relaxed"
              style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
              role="alert"
            >
              <span class="font-semibold">
                {svcMissing.length === 1 ? "One field still needs filling in:" : `${svcMissing.length} fields still need filling in:`}
              </span>
              <ul class="mt-1 mb-0 list-none space-y-0.5 pl-0">
                {#each svcMissing as m (m.field)}
                  <li><span class="font-semibold">{m.label}</span> — {m.why}</li>
                {/each}
              </ul>
            </div>
          {/if}

          <div class="mt-3 flex flex-wrap items-center gap-3">
            <button class="btn btn-primary btn-md" disabled={creating} onclick={createAccount}>
              {creating ? "Creating…" : "Create account"}
            </button>
            <span class="text-xs text-ink-5">
              The key is shown once, written to a file you pick, and never stored here.
            </span>
          </div>

          {#if newDays == null}
            <p class="mt-2.5 mb-0 text-xs" style="color: var(--status-warn-ink);">
              A credential with no clock on it is the long-lived token this feature exists
              to replace. Prefer a date you will actually rotate on.
            </p>
          {/if}
          <p class="mt-2 mb-0 text-xs text-ink-5">
            Admin is not offered: an admin key can create users and grants, so a leaked
            pipeline credential holding one is permanent mesh compromise.
          </p>
          {#if createErr}<div class="mt-1.5 text-xs text-error">{createErr}</div>{/if}
          {#if created}
            <div class="mt-2 rounded px-3 py-2" style="background: var(--accent-wash);">
              <div class="text-sm font-semibold" style="color: var(--accent-ink);">
                {created.name} created
              </div>
              <div class="mt-1 text-xs text-ink-4">
                Key written to <span class="num">{created.path}</span> (mode 0600).
                meshware does not store it and cannot show it again — move it to the system
                that needs it, then delete your copy.
              </div>
            </div>
          {/if}
        </div>
      {/if}
    </div>

    <div class="card mt-4 px-[22px] py-4">
      <div class="flex items-baseline gap-2">
        <h2 class="m-0 text-base font-semibold">Identity providers</h2>
        <Hint
          text="Lets people prove who they are with your company's IdP before an admin approves them. It does not add a login — a person is still a key on their own machine."
          label="What this is"
        />
      </div>
      {#if providers.length === 0}
        <p class="mt-2 mb-0 text-sm text-ink-4">
          None configured, and a mesh works exactly the same without one — enrolment by
          code comparison is unchanged. Add one and the pending queue shows a verified
          name, email and group list instead of a key somebody pasted into chat.
        </p>
        <p class="mt-2 mb-0 text-xs text-ink-5">
          Configure with <span class="num">meshnode oidc providers put</span>. Your IdP
          being unreachable never affects the mesh: existing keys keep working and jobs
          keep running.
        </p>
      {:else}
        <div class="tablewrap mt-3">
          <table class="w-full text-sm">
            <thead>
              <tr>
                <th class="label text-left">NAME</th>
                <th class="label text-left">ISSUER</th>
                <th class="label text-left">SUGGESTS</th>
              </tr>
            </thead>
            <tbody>
              {#each providers as p (p.name)}
                <tr>
                  <td class="num">{p.name}</td>
                  <td class="text-xs text-ink-4">{p.issuer}</td>
                  <td class="text-xs">{p.groupRoles.length ? p.groupRoles.join(" ") : "—"}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>

    <!-- Y2/Y3 — whose word about hardware this mesh takes. -->
    <AttestRoots />

  <!-- ═════════════════════════ history ═════════════════════════ -->
  {:else}
    {#if !canAudit}
      <div class="card empty">
        <div class="empty-title">Reading the history needs the admin or auditor role</div>
        <div class="empty-sub">
          It records every governed action by everyone, so it is deliberately not readable by
          people who only run work.
        </div>
      </div>
    {:else}
      <div class="card px-[22px] py-4">
        <p class="mt-0 mb-3 max-w-3xl text-sm leading-relaxed text-ink-4">
          Everything anybody did, signed by the key that did it. Nothing here can be edited or
          removed — not by an admin, and not by the founder.
        </p>
        <div class="flex flex-wrap items-end gap-3">
          <label class="block w-44">
            <span class={ui.label}>Who</span>
            <input
              bind:value={aUser}
              list="user-names-audit"
              placeholder="anyone"
              class={ui.inputMono}
              onkeydown={(e) => e.key === "Enter" && runAudit()}
            />
          </label>
          <datalist id="user-names-audit">
            {#each app.users as u (u.userId)}<option value={u.username}></option>{/each}
          </datalist>
          <label class="block w-56">
            <span class={ui.label}>What</span>
            <select bind:value={aAction} class={`${ui.input} select`}>
              {#each ACTION_PRESETS as [value, label] (value)}<option {value}>{label}</option>{/each}
            </select>
          </label>
          <label class="block w-28">
            <span class={ui.label}>When</span>
            <select bind:value={aSince} class={`${ui.input} select`}>
              <option value="1h">last hour</option>
              <option value="6h">last 6 hours</option>
              <option value="24h">last day</option>
              <option value="7d">last week</option>
              <option value="30d">last month</option>
              <option value="all">everything</option>
            </select>
          </label>
          <label class="block w-24">
            <span class={ui.label}>At most</span>
            <input type="number" min="1" max="1000" bind:value={aLimit} class="num h-10 w-full rounded-md bg-canvas px-3 text-sm outline-none" style="box-shadow: var(--sh-control);" />
          </label>
          <button class={ui.primary} disabled={aBusy} onclick={runAudit}>{aBusy ? "Looking…" : "Show me"}</button>
          {#if aRan}
            <span class="num self-end text-xs leading-10 text-ink-5">{aRows.length} event{aRows.length === 1 ? "" : "s"}</span>
          {/if}
          <div class="ml-auto flex items-center gap-2 self-end">
            <div class="seg" role="tablist" aria-label="How to read the history">
              <button
                role="tab"
                aria-selected={aMode === "story"}
                onclick={() => (aMode = "story")}>Story</button
              >
              <button
                role="tab"
                aria-selected={aMode === "table"}
                onclick={() => (aMode = "table")}>Table</button
              >
            </div>
            <button class={ui.rowAction} disabled={aRows.length === 0} onclick={copyStory}>
              Copy as text
            </button>
          </div>
        </div>
        {#if aErr}<div class={ui.errorBox}>{aErr}</div>{/if}
      </div>

      {#if aMode === "story"}
        <!-- The same rows, read aloud. `story.ts` turns one row into a
             sentence; everything below is grouping by day and getting out
             of the way. The action and the raw outcome stay reachable on
             hover, because the record is still the record. -->
        <div class="card overflow-hidden">
          {#if aDays.length === 0}
            <div class="px-5 py-8 text-center text-sm text-ink-5">
              {aRan ? "Nothing matched." : "Pick a filter and press Show me."}
            </div>
          {:else}
            {#each aDays as day (day.key)}
              <div
                class="label sticky top-0 z-1 border-b border-edge bg-surface-2 px-5 py-2"
              >
                {day.label}
                <span class="num ml-2 text-ink-5">
                  {day.rows.length} event{day.rows.length === 1 ? "" : "s"}
                </span>
              </div>
              <div use:staggerIn={{ selector: ":scope > div", each: 14, y: 5 }}>
                {#each day.rows as e (e.auditId)}
                  {@const s = storyOf(e, aNames)}
                  <div
                    class="grid grid-cols-[46px_1fr] items-baseline gap-x-3 border-b border-edge px-5 py-2 leading-relaxed transition-colors last:border-0 hover:bg-surface-2 sm:grid-cols-[46px_1fr_auto]"
                    title={`${e.action} · ${e.outcome || "ok"}`}
                  >
                    <span class="num flex items-baseline gap-1.5 text-xs text-ink-5">
                      {#if s.tone !== "ok"}
                        <span
                          class="inline-block size-1.5 shrink-0 rounded-full"
                          style={`background: ${toneDot(s.tone)};`}
                          title={e.outcome}
                        ></span>
                      {/if}
                      {clockOf(e.atMicros)}
                    </span>
                    <span class="min-w-0 text-sm text-ink-3">
                      {#each s.pieces as piece, i (i)}
                        {#if piece.t === "name"}
                          <span class="font-semibold" style={nameInk(piece.v)}>{piece.v}</span>
                        {:else if piece.t === "thing"}
                          <span class="mono selectable text-[13px] text-ink-2">{piece.v}</span>
                        {:else if piece.t === "refusal"}
                          <span style={REFUSAL_INK}>{piece.v}</span>
                        {:else if piece.t === "quiet"}
                          <span class="text-ink-5">{piece.v}</span>
                        {:else}
                          {piece.v}
                        {/if}
                      {/each}
                    </span>
                    <span
                      class="num hidden text-xs whitespace-nowrap text-ink-5 sm:block"
                      title="the node that recorded it"
                    >
                      {e.nodeShort}
                    </span>
                  </div>
                {/each}
              </div>
            {/each}
          {/if}
        </div>
      {:else}
      <div class="card overflow-hidden">
        <table class="w-full text-left text-sm">
          <thead>
            <tr class="border-b border-edge">
              <th class="label px-5 py-3">When</th>
              <th class="label px-3 py-3">Who</th>
              <th class="label px-3 py-3">Did what</th>
              <th class="label px-3 py-3">To what</th>
              <th class="label px-3 py-3">Result</th>
              <th class="label px-3 py-3">On</th>
              <th class="label px-3 py-3 pr-5">Detail</th>
            </tr>
          </thead>
          <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
            {#each aRows as e (e.auditId)}
              <tr class="h-[38px] border-b border-edge transition-colors hover:bg-surface-2 last:border-0">
                <td class="num px-5 py-2 whitespace-nowrap text-ink-5">{ago(e.atMicros)}</td>
                <td class="px-3 py-2 font-semibold" style={nameInk(e.username)}>{e.username}</td>
                <td class="mono px-3 py-2 text-xs text-ink-2">{e.action}</td>
                <td class="mono max-w-48 truncate px-3 py-2 text-xs text-ink-3" title={e.target}>{e.target}</td>
                <td class="mono px-3 py-2 text-xs font-semibold" style={outcomeStyle(e.outcome)}>{e.outcome}</td>
                <td class="mono px-3 py-2 text-xs text-ink-5">{e.nodeShort}</td>
                <td class="selectable max-w-80 truncate px-3 py-2 pr-5 text-ink-4" title={e.detail}>{e.detail}</td>
              </tr>
            {:else}
              <tr><td colspan="7" class="px-5 py-8 text-center text-sm text-ink-5">{aRan ? "Nothing matched." : "Pick a filter and press Show me."}</td></tr>
            {/each}
          </tbody>
        </table>
      </div>
      {/if}
    {/if}
  {/if}
</div>

<!-- ═══════════ remove a person: the heavy modal (typed name) ═══════════ -->
<!-- Key rotation (E2.3). -->
<Modal open={rotateFor !== null} kicker="replace key" peer={rotateFor ?? ""} onclose={() => (rotateFor = null)}>
  <h2 class="m-0 text-lg font-semibold">Replace {rotateFor}'s key</h2>
  <p class="mt-2 mb-0 text-sm leading-relaxed text-ink-4">
    For a lost laptop, a suspected compromise, or a rotation policy. Their new key is
    bound to the same name, every active role moves onto it, and only then is the old
    key revoked — in that order, so they can work at every moment in between.
  </p>
  <p class="mt-2 mb-0 text-xs leading-relaxed text-ink-5">
    Ask them to run <span class="num">meshnode whoami</span> on their machine and read
    you the <span class="num">mwu1…</span> line.
  </p>
  <input
    class="{ui.inputMono} mt-3"
    placeholder="mwu1…"
    bind:value={rotateKey}
    onkeydown={(e) => { if (e.key === "Enter") rotate(); }}
  />
  <p class="mt-2 mb-0 text-xs text-ink-5">
    Their audit history stays under the old key and stays readable. Rotation is not a
    way to disown what a key did.
  </p>
  {#if rotateErr}<div class="mt-2 text-xs text-error">{rotateErr}</div>{/if}
  <div class="mt-4 flex gap-2">
    <button class="btn" disabled={rotateBusy || !rotateKey.trim()} onclick={rotate}>
      {rotateBusy ? "…" : "Replace the key"}
    </button>
    <button class="btn-ghost" onclick={() => (rotateFor = null)}>Cancel</button>
  </div>
</Modal>

<Modal open={revokeUser !== null} kicker="remove person" peer={revokeUser?.shortId ?? ""} onclose={() => (revokeUser = null)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Remove {revokeUser?.username}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      <span class="font-semibold" style="color: var(--status-error-ink);">Permanent.</span>
      Their key is refused on every machine from now on, and it cannot be undone — if they need
      to come back, they ask to join again with a new key. What they already did stays in the
      history.
    </p>
    <label class="mt-4 block">
      <span class="mb-1.5 block text-xs text-inkcode-3">Reason (recorded)</span>
      <input bind:value={revokeUserReason} class="h-10 w-full rounded-md border px-3 text-base text-inkcode outline-none" style="background: rgba(160,176,200,.08); border-color: var(--edge-code);" />
    </label>
    <label class="mt-3 block">
      <span class="mb-1.5 block text-xs text-inkcode-3">
        Type <span class="mono" style="color: var(--status-error-ink);">{revokeUser?.username}</span> to confirm
      </span>
      <input bind:value={revokeUserTyped} placeholder={revokeUser?.username} class="mono h-10 w-full rounded-md border px-3 text-base text-inkcode outline-none" style="background: rgba(160,176,200,.08); border-color: var(--edge-code);" />
    </label>
    {#if revokeUserErr}
      <div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{revokeUserErr}</div>
    {/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button class="btn btn-lg border border-edge-code font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]" onclick={() => (revokeUser = null)}>Cancel</button>
      <button class="btn btn-destructive h-11 text-base" disabled={!revokeUserArmed || revokeUserBusy} onclick={doRevokeUser}>
        {revokeUserBusy ? "Removing…" : "Remove permanently"}
      </button>
    </div>
  </div>
</Modal>

<Modal open={revokeGrant !== null} kicker="take away a role" peer={revokeGrant?.grantId.slice(0, 18) ?? ""} onclose={() => (revokeGrant = null)}>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Take this away?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      <span class="mono" style="color: var(--ink-code-2);">{revokeGrant?.username}</span> loses
      <span class="mono" style="color: var(--ink-code-2);">{revokeGrant?.role}</span> on
      <span class="mono" style="color: var(--ink-code-2);">nodes={revokeGrant?.nodes} services={revokeGrant?.services}</span>.
      Any other role they hold is untouched, and work already running finishes.
    </p>
    {#if revokeGrantErr}
      <div class="selectable mt-3 rounded-sm px-3 py-2 text-sm" style="background: var(--status-error-wash); color: var(--status-error-ink);">{revokeGrantErr}</div>
    {/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button class="btn btn-lg border border-edge-code font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)]" onclick={() => (revokeGrant = null)}>Cancel</button>
      <button class="btn btn-danger btn-lg" disabled={revokeGrantBusy} onclick={doRevokeGrant}>
        {revokeGrantBusy ? "Taking away…" : "Take it away"}
      </button>
    </div>
  </div>
</Modal>

<!-- ═══════════ the roles reference, as a dialog ═══════════ -->
<!-- Consulted while granting, not visited — so it opens on top of whatever
     raised the question and costs no tab. -->
{#if rolesOpen}
  <div
    use:portal
    class="fixed inset-0 z-50 grid place-items-center p-6"
    style="background: rgba(12,16,24,.62); backdrop-filter: blur(3px);"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) rolesOpen = false;
    }}
  >
    <div
      class="fade-in flex max-h-[86vh] w-full max-w-[960px] flex-col overflow-hidden rounded-2xl bg-surface"
      style="box-shadow: var(--sh-modal);"
      role="dialog"
      aria-modal="true"
      aria-label="What each role can do"
      use:dialog={{ onescape: () => (rolesOpen = false) }}
    >
      <div class="flex items-center gap-3 border-b border-edge px-6 py-[16px]">
        <h3 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">Five roles, and what each one means</h3>
        <span class="text-sm text-ink-5">a fixed set — you cannot invent new ones</span>
        <button class="btn btn-secondary ml-auto h-8 w-8 shrink-0 rounded-sm p-0" aria-label="Close" onclick={() => (rolesOpen = false)}>
          <Icon name="close" size={14} />
        </button>
      </div>
      <div class="overflow-y-auto px-6 py-5">
        <RoleMatrix />
        <div class="mt-4 grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
          {#each ROLES as role (role)}
            {@const held = app.users.filter((u) => !u.revoked && rolesOf(app.grants, u.userId, u.isFounder).includes(role)).length}
            <div class="rounded-lg px-4 py-3" style={`${hueStyle(roleHue(role))} background: var(--hue-wash); box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--hue) 22%, transparent);`}>
              <div class="flex items-baseline gap-2">
                <span class="mono text-sm font-semibold" style="color: var(--hue-ink);">{role}</span>
                <span class="text-xs text-ink-4">{ROLE_SUMMARY[role].one}</span>
                <!-- Ink-3, not ink-5, on the role's wash: the secondary
                     ink fell to 4.46:1 on the darkest wash in dark mode
                     (UX-16's audit). -->
                <span class="ml-auto text-xs whitespace-nowrap text-ink-3">
                  <span class="num">{held}</span>
                  {held === 1 ? "person" : "people"}
                </span>
              </div>
              <p class="mt-1 mb-0 text-xs leading-relaxed text-ink-3">{ROLE_SUMMARY[role].long}</p>
            </div>
          {/each}
        </div>
        <p class="mt-4 mb-0 text-xs leading-relaxed text-ink-5">
          Every role arrives attached to a scope, so the real question is never “what is an
          operator” but “what is an operator <em>here</em>” — a person's row answers that half,
          per person.
        </p>
      </div>
    </div>
  </div>
{/if}
