// App-wide reactive state (Svelte 5 runes). Live-by-default: tables poll
// the embedded engine (in-process, cheap); job output streams over
// channels (MASTERPLAN §14.3 — no refresh buttons).

import {
  api,
  onKeyPaused,
  errText,
  type BootstrapView,
  type IdentityLockView,
  type StatusView,
  type NodeView,
  type TopView,
  type TotalsView,
  type JobView,
  type JobCountsView,
  type ServiceView,
  type BundleView,
  type SourceView,
  type SecretView,
  type ProviderView,
  type UserView,
  type GrantView,
  type WhoAmIView,
  type DeepLinkView,
  type EnrolmentView,
  type CandidateView,
  type LanWatchView,
  type AboutView,
} from "./api";
import { SUBNAV, SUB_DEFAULT, RETIRED_PANELS } from "./chrome/nav";

export type ViewName =
  | "mesh"
  | "nodes"
  | "run"
  | "jobs"
  | "deployments"
  | "schedules"
  | "registry"
  | "compute"
  | "library"
  | "secrets"
  | "access"
  | "settings";

const POLL_MS = 2000;
/**
 * How many job rows the shared table holds (**H-1**).
 *
 * `jobs` used to be *every* job the mesh remembered — rebuilt by the
 * engine, serialised, bridged and re-proxied every two seconds, and past
 * roughly twenty thousand jobs a request that failed outright rather than
 * merely being slow. Everything on this side of the bridge that reads the
 * list wants recent work (the rail's activity line, the palette, a
 * workflow draft, a node's running count), so the shared copy is the
 * newest few hundred and `jobsCounts` carries the honest totals. The Jobs
 * screen asks for its own filtered page when it needs one.
 */
const JOBS_PAGE = 500;

const NO_JOB_COUNTS: JobCountsView = {
  all: 0,
  queued: 0,
  running: 0,
  succeeded: 0,
  failed: 0,
  cancelled: 0,
  timedOut: 0,
};
/** Views whose data lives outside the core poll (library / access / …). */
const EXTENDED_VIEWS: ReadonlySet<ViewName> = new Set([
  "compute",
  "nodes",
  "library",
  "secrets",
  "access",
  "settings",
]);
/**
 * Views that watch the machines breathe.
 *
 * The topology's heat rings and the Fleet tab's busy bars are drawn from
 * `top`, which only the extended poll refreshed — and Mesh is not an
 * extended view, so under the Focus surface a CPU reading on the mesh
 * page was whatever the daemon said at boot. `top` and `totals` are one
 * gossip-table fold each; they ride the core tick here rather than
 * dragging bundles, grants and policies along every two seconds.
 */
const VITALS_VIEWS: ReadonlySet<ViewName> = new Set(["mesh"]);

/**
 * The join queue's cadences.
 *
 * `ENROL_POLL_MS` is faster than the extended poll and slower than the core
 * one: somebody is standing at another machine waiting, but the answer is a
 * row in a replicated log rather than a live signal.
 *
 * The LAN watch lives in the engine (`pairing::LanWatch`); the frontend
 * only reads its table, every `LAN_POLL_MS`. Cheap enough to keep going
 * while the window is hidden, which is the point: the badge is right the
 * moment the window comes back.
 */
const ENROL_POLL_MS = 5_000;
const LAN_POLL_MS = 2_000;
const LAN_IDLE_MS = 4_000;

/**
 * Who this person is, re-read (DIR-5).
 *
 * `whoami` was fetched once, while it was still `null`, and never again —
 * so "Request access" never became "you are in the queue", an approval
 * never cleared the banner, and admin-only controls stayed wrong until a
 * restart. It is re-read whenever one of `WHOAMI_TOPICS` moves, which is
 * every op that can change the answer, and on this slow timer for the
 * changes no op records: an enrolment request expiring, a time-boxed grant
 * lapsing.
 */
const WHOAMI_POLL_MS = 30_000;
const WHOAMI_TOPICS = ["users", "grants", "enrolments"] as const;

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/**
 * The shared tables a load can fail for, one error each (UX-2).
 *
 * Every per-topic fetch below used to end in `.catch(() => null)`, so a
 * refusal and an empty table looked the same to the views: a person
 * without the grant was told "No secrets yet — Set a secret", on a product
 * sold on access control. A failure is kept here instead, under the topic
 * it failed for, until a fetch of that topic succeeds.
 */
export type LoadTopic =
  | "nodes"
  | "jobs"
  | "services"
  | "bundles"
  | "sources"
  | "secrets"
  | "users"
  | "grants";

/** One settled fetch: its value, or why there is none. */
type Settled<T> = { ok: true; value: T } | { ok: false; error: string };

function settle<T>(p: Promise<T>): Promise<Settled<T>> {
  return p.then(
    (value) => ({ ok: true as const, value }),
    (e) => ({ ok: false as const, error: errText(e) }),
  );
}


/**
 * A machine waiting to pair, and its relationship to the mesh:
 *   `new`        — never seen;
 *   `returning`  — the same key as a member that left or went dead;
 *   `revoked`    — the same key as a member whose key was revoked;
 *   `name-clash` — a different key using the name of a current member;
 *   `name-reuse` — a different key using the name of a member that left.
 */
export type KnownCandidate = {
  candidate: CandidateView;
  known: NodeView | null;
  relation: "new" | "returning" | "revoked" | "name-clash" | "name-reuse";
};

class AppState {
  boot = $state<BootstrapView | null>(null);
  status = $state<StatusView | null>(null);
  /**
   * The fleet: machines this mesh has.
   *
   * **Consoles are not in here**, and that is the point. Director is a peer
   * of its own (D6), so there is one member per machine and no second row
   * to filter out.
   */
  //
  // **`$state.raw`, here and for every collection below** (**H-8**).
  //
  // Nothing in this class ever mutates a row or splices an array — every
  // poll replaces the whole thing — so the deep reactive proxy was wrapping
  // each of ~11 000 objects per tick to provide a granularity no reader
  // used. `raw` keeps the reassignment reactive and drops the per-object
  // cost. The rule for anything added here: if it is replaced wholesale,
  // it is `raw`; if a component edits a field in place, it is not.
  nodes = $state.raw<NodeView[]>([]);

  /**
   * A node id as a person reads it: the machine's name.
   *
   * History rows carry `NodeId::to_display_string()` — 55 characters of
   * base32 — because the index stores an id and an id is what survives a
   * rename. Every screen that shows one still has to show the *name*, and
   * the fallback is the short id rather than the full one: a column that
   * cannot fit a name certainly cannot fit fifty-five characters, and a
   * cell that overflows a fixed table paints over its neighbours.
   *
   * Consoles are searched too. A job submitted from a Director peer would
   * otherwise be the one row in the list still showing raw base32.
   */
  nodeLabel(id: string): string {
    if (!id) return "—";
    const hit = this.nodes.find((n) => n.nodeId === id);
    return hit?.name || hit?.shortId || id.slice(0, 15);
  }
  /** This peer's own row. */
  selfNode = $state<NodeView | null>(null);
  top = $state.raw<TopView[]>([]);
  totals = $state<TotalsView | null>(null);
  /**
   * The newest [`JOBS_PAGE`] jobs, mesh-wide.
   *
   * Bounded since **H-1**, and `$state.raw` because nothing ever mutates a
   * row in place — every poll replaces the array wholesale, so the deep
   * reactive proxy was paying to wrap ten thousand objects a tick for a
   * granularity nobody read.
   */
  jobs = $state.raw<JobView[]>([]);
  /** State tallies over the *whole* job table — see [`JOBS_PAGE`]. */
  jobsCounts = $state.raw<JobCountsView>(NO_JOB_COUNTS);
  services = $state.raw<ServiceView[]>([]);
  bundles = $state.raw<BundleView[]>([]);
  sources = $state.raw<SourceView[]>([]);
  secrets = $state.raw<SecretView[]>([]);
  providers = $state.raw<ProviderView[]>([]);
  users = $state.raw<UserView[]>([]);
  grants = $state.raw<GrantView[]>([]);
  whoami = $state<WhoAmIView | null>(null);
  /**
   * People waiting to be let in, and machines announcing themselves.
   *
   * These ride the *core* poll and a background LAN watch rather than the
   * Access screen: somebody standing at another machine waiting to be
   * admitted is the most time-sensitive
   * thing in the product, and a queue you only see if you happen to open
   * the right screen is a queue that leaves people waiting. The rail
   * badges it from anywhere.
   */
  enrolments = $state.raw<EnrolmentView[]>([]);
  /** Machines on this LAN sitting in the wizard's "Pair on this network". */
  lanCandidates = $state.raw<CandidateView[]>([]);
  /** The watch itself: running or not, and whether each ear is working —
   *  so an empty queue can say *why* it is empty. */
  lanWatch = $state<LanWatchView | null>(null);
  /** When the watch table was last read, for a "checked 2s ago" line. */
  lanPolledAt = $state(0);
  /** Whether the background LAN watch is running (Settings can stop it). */
  watchingLan = $state(true);

  view = $state<ViewName>("mesh");
  /**
   * Which panel of the current section is open (6c).
   *
   * The panels of a section are rows in the sidebar now, indented under
   * it, rather than a tab row inside the content area — so which one is
   * open is navigation, and navigation lives here beside `view` and
   * `go()`. `null` for a section with no panels; only Mesh has any
   * (`chrome/nav.ts`).
   */
  sub = $state<string | null>(SUB_DEFAULT.mesh ?? null);
  ready = $state(false);
  needsWizard = $state(false);
  /**
   * The key is sealed and there is no unlock in this login session (B9).
   *
   * Checked before anything else, because *everything* else needs the key:
   * `bootstrap_status` reads it, `start_engine` signs with it. Before this
   * gate existed a sealed key produced a dead window whose only error said
   * to run a terminal command.
   *
   * Deliberately separate from `fatal`. A locked key is not a failure — it
   * is the feature working, and the screen it deserves has a passphrase box
   * on it rather than a stack trace.
   */
  needsUnlock = $state(false);
  /**
   * This account signs with a key an agent holds, and the agent is not
   * holding it — a YubiKey that is unplugged or not connected yet. Like
   * `needsUnlock`, a door rather than a crash: Director cannot start
   * without the key, and the screen for it says how to connect it.
   */
  needsKey = $state(false);
  /**
   * Director stopped asking the security key to sign, because it refused
   * — a prompt cancelled, a wrong PIN, a key unplugged — and why
   * (`KeyGate` in engine.rs). Polling stops with it: nothing asks again
   * until the person says to.
   */
  keyPaused = $state<string | null>(null);
  /** The lock state itself, for the gate and the Settings panel. */
  lock = $state<IdentityLockView | null>(null);
  /** The embedded engine was drained and stopped from Settings. */
  engineStopped = $state(false);
  fatal = $state<string | null>(null);
  toast = $state<string | null>(null);
  /** Last successful engine sync (drives the "last gossip" freshness line). */
  lastSync = $state(Date.now());
  /**
   * Why a shared table could not be read, by topic (UX-2).
   *
   * Only written once the engine has answered the same tick's `status` (or
   * `top`) call: an engine that is restarting fails every topic at once,
   * and that is a pause, not a refusal worth a box on every screen. So an
   * entry here means the engine is up and said no to this one thing.
   * Replaced wholesale, like the tables themselves.
   */
  loadErrors = $state.raw<Partial<Record<LoadTopic, string>>>({});

  /**
   * This build and its licence (UX-7). Read once, before the
   * engine: it is compiled into the app, and the About pane is where a
   * person goes precisely when the mesh will not come up.
   */
  about = $state<AboutView | null>(null);

  /** Can the person at this Director act on a join request? */
  get isAdmin(): boolean {
    return !!(this.whoami?.isFounder || this.whoami?.roles.includes("admin"));
  }

  /** Join requests from someone whose key has reached this mesh. */
  get pendingEnrolments(): EnrolmentView[] {
    return this.enrolments.filter((e) => !e.declined);
  }

  /**
   * Machines announcing on the LAN, each paired with what this mesh
   * already knows about it.
   *
   * A candidate keeps announcing until it pairs, so one whose *id* is a
   * live member is a stale record, not a request, and is left out. Every
   * other overlap is shown and *said*: the same id as a member that left
   * (a machine coming back), or the same name as a current member with a
   * different key (a rebuilt machine, or a clash) — because hiding those
   * was exactly how "I pressed Join and nothing appeared" happened.
   */
  get joinableCandidates(): KnownCandidate[] {
    const byId = new Map(this.nodes.map((n) => [n.nodeId, n]));
    const byName = new Map(this.nodes.map((n) => [n.name, n]));
    const out: KnownCandidate[] = [];
    for (const c of this.lanCandidates) {
      const sameId = c.nodeId ? byId.get(c.nodeId) : undefined;
      if (sameId && !sameId.left && !sameId.revoked && sameId.liveness !== "dead") continue;
      const sameName = byName.get(c.nodeName);
      const known = sameId ?? sameName ?? null;
      const relation: KnownCandidate["relation"] = !known
        ? "new"
        : sameId
          ? sameId.revoked
            ? "revoked"
            : "returning"
          : sameName && !sameName.left && !sameName.revoked
            ? "name-clash"
            : "name-reuse";
      out.push({ candidate: c, known, relation });
    }
    return out;
  }

  /** The rail badge: everything waiting on an admin's yes or no. */
  get joinQueueCount(): number {
    if (!this.isAdmin) return 0;
    return this.pendingEnrolments.length + this.joinableCandidates.length;
  }

  /**
   * Everything waiting on *this person*, as one number (UX-6): the join
   * queue. The header bell and the Access badge both read it, so the app
   * has one "waiting on you" count rather than two that disagree.
   */
  get waitingOnYou(): number {
    return this.joinQueueCount;
  }

  #timer: ReturnType<typeof setInterval> | null = null;
  #extendedInFlight = false;
  #enrolInFlight = false;
  #enrolAt = 0;
  /** When `whoami` last landed — see [`WHOAMI_POLL_MS`]. */
  #whoamiAt = 0;
  /** The LAN watch's generation, so stopping it actually stops it. */
  #lanGen = 0;
  /**
   * Which mesh the tables belong to, bumped by [`reset`] (DIR-5).
   *
   * Every fetch notes it before its first await and drops its answer if it
   * has moved by the time the answer lands: a tick that set off against
   * the demo's engine must not write the demo's founder back into the
   * state the next mesh starts from.
   */
  #epoch = 0;

  async init() {
    if (!this.about) this.about = await api.about().catch(() => null);
    try {
      // First, before anything that needs the key. A sealed key with no
      // session unlock stops here and shows the gate; everything below
      // would otherwise fail one call later with a message a GUI user
      // cannot act on.
      this.lock = await api.identityLockStatus().catch(() => null);
      if (this.lock?.protected && !this.lock.unlocked) {
        this.needsUnlock = true;
        return;
      }
      // A key on a security key that is not connected: say so, rather
      // than failing to start with the agent's sentence.
      if (this.lock?.hardware && !this.lock.hardwareReady) {
        this.needsKey = true;
        return;
      }
      this.needsKey = false;
      this.boot = await api.bootstrapStatus();
      if (!this.boot.hasMesh) {
        this.needsWizard = true;
        return;
      }
      this.status = await api.startEngine();
      await this.refresh();
      await this.refreshExtended();
      this.ready = true;
      this.startPolling();
      // Deep link into a view (palette jumps, dev mock URLs).
      const jump = sessionStorage.getItem("app:view");
      if (jump) {
        sessionStorage.removeItem("app:view");
        this.go(jump as ViewName);
      }
      // …and a `director://` link that started this window (L2 P5), which
      // wins over the jump above because somebody asked for it by name.
      await this.followDeepLink();
    } catch (e) {
      this.fatal = errText(e);
    }
  }

  /**
   * "Retry" on the could-not-start screen (DIR-11).
   *
   * The whole start again rather than the step that failed: `init` is
   * written to be re-run (the unlock gate already does), `start_engine`
   * hands back an engine that did come up, and the likeliest fixes — close
   * the other copy, free some disk — change what the first step would see.
   */
  async retryStart() {
    this.fatal = null;
    await this.init();
  }

  /**
   * Called by the unlock gate once the key is open.
   *
   * Re-runs `init` rather than continuing from where it stopped: nothing
   * had run yet, so there is no half-built state to reconcile, and one
   * start path is easier to keep correct than two.
   */
  async onUnlocked(lock: IdentityLockView) {
    this.lock = lock;
    this.needsUnlock = false;
    await this.init();
  }

  /** The security key is connected: start the way a normal start does. */
  async onKeyConnected() {
    this.needsKey = false;
    await this.init();
  }

  /**
   * Director stopped asking the security key (`api.onKeyPaused`). Stop
   * polling too: every refresh would only be answered "paused", and the
   * screen that matters now is the one asking what to do.
   */
  pauseForKey(why: string) {
    if (this.keyPaused !== null) return;
    this.keyPaused = why;
    this.stopPolling();
  }

  /** "Try again": ask the security key to sign again, and carry on. */
  async resumeKey() {
    await api.securityKeyResume();
    this.keyPaused = null;
    if (!this.ready) return;
    this.invalidate();
    this.startPolling();
    await this.refresh();
    await this.refreshWhoami();
  }

  /**
   * The key file changed to a different key altogether — going back to a
   * key a binding set aside. Everything this window knows was told to the
   * other key, so it starts again from the top: the unlock screen, if the
   * key that came back is sealed, and then the mesh.
   */
  async restartIdentity() {
    this.reset();
    this.needsKey = false;
    this.keyPaused = null;
    this.fatal = null;
    await this.init();
  }

  /**
   * Called by the wizard once a mesh exists and the engine runs.
   *
   * `suggestedName` is what the person called themselves on the way in. It
   * is carried through to [`requestAccessIfNeeded`] so that joining a mesh
   * is one act from their side too — see the note there.
   */
  async onboarded(suggestedName?: string) {
    // Whatever this window showed before is another mesh's, or nothing.
    // After a demo it is the demo's founder, and keeping it meant joining a
    // real mesh as somebody the real mesh had never heard of (DIR-5).
    this.reset();
    this.needsWizard = false;
    try {
      this.boot = await api.bootstrapStatus();
      await this.refresh();
      await this.refreshExtended();
      this.ready = true;
      this.startPolling();
      await this.requestAccessIfNeeded(suggestedName);
    } catch (e) {
      this.fatal = errText(e);
    }
  }

  /**
   * Put this person in front of an admin, if they are not already known.
   *
   * Joining a mesh is two acts — a *machine* becomes a member, and the
   * *person* on it gets their key recognised — and the product used to
   * make you discover the second one by being refused. You paired
   * successfully, landed in the app, and were told the mesh did not know
   * your key, with a banner offering a button you had no reason to trust
   * more than the thing that had just failed.
   *
   * So the second act now happens on its own, the moment the first one
   * finishes: an admin sees "somebody wants to join" within a couple of
   * seconds of the machine appearing, and the person joining sees a queue
   * position instead of a refusal.
   *
   * This grants nothing and cannot: a request is a self-signed row saying
   * "this key would like to be recognised". Every authorization still comes
   * from an admin-signed grant. Guarded on `enrolment === "none"`, so
   * somebody who has already asked is not re-queued, and somebody who was
   * declined does not silently ask again.
   */
  async requestAccessIfNeeded(suggestedName?: string) {
    const me = this.whoami;
    if (!me || me.registered || me.isFounder || me.enrolment !== "none") return;
    try {
      await api.enrolRequest((suggestedName ?? "").trim() || this.boot?.hostname || "");
      await this.refreshWhoami();
    } catch {
      // An older peer with no enrolment op, or an engine still settling.
      // The banner's "Request access" button is the manual path and says
      // the same thing, so there is nothing to report here.
    }
  }

  /**
   * Re-read who this person is, now (DIR-5).
   *
   * For the moment somebody has just changed the answer from this window —
   * pressed "Request access", say — and should not wait for the change
   * signal to come round to show them where they stand.
   */
  async refreshWhoami() {
    const epoch = this.#epoch;
    const me = await api.whoami().catch(() => null);
    if (!me || epoch !== this.#epoch) return;
    this.whoami = me;
    this.#whoamiAt = Date.now();
  }

  /**
   * This account's key file changed under the window (Y2/Y3): it signs as
   * a different key now. Everything that describes *who you are* is read
   * again, and every table is asked for afresh — the old answers were
   * given to the old key.
   */
  async identityChanged() {
    this.invalidate();
    this.lock = await api.identityLockStatus().catch(() => this.lock);
    this.boot = await api.bootstrapStatus().catch(() => this.boot);
    await this.refreshWhoami();
    void this.refresh();
  }

  /**
   * Forget the mesh this window was showing (DIR-5).
   *
   * Stops every poll and clears every table, who this person is included,
   * and puts navigation back on the landing screen. Called when the demo
   * stops and before a mesh is joined or founded: the tables are the old
   * mesh's until then, and a `whoami` carried across is the old mesh's
   * idea of who you are — the demo's founder, on a mesh that has never
   * heard of them, with no enrolment banner and every request refused.
   *
   * `boot`, the lock and the About facts describe this machine rather than
   * a mesh, and are left for the caller to re-read.
   */
  reset() {
    this.stopPolling();
    // Anything already in flight belongs to the old mesh; see `#epoch`.
    this.#epoch += 1;
    this.#refreshInFlight = false;
    this.#extendedInFlight = false;
    this.#vitalsInFlight = false;
    this.#enrolInFlight = false;
    this.invalidate();
    this.#whoamiAt = 0;
    this.#enrolAt = 0;

    this.ready = false;
    this.engineStopped = false;
    this.status = null;
    this.nodes = [];
    this.selfNode = null;
    this.top = [];
    this.totals = null;
    this.jobs = [];
    this.jobsCounts = NO_JOB_COUNTS;
    this.services = [];
    this.bundles = [];
    this.sources = [];
    this.secrets = [];
    this.providers = [];
    this.users = [];
    this.grants = [];
    this.whoami = null;
    this.enrolments = [];
    this.lanCandidates = [];
    this.lanWatch = null;
    this.lanPolledAt = 0;
    this.loadErrors = {};

    this.view = "mesh";
    this.sub = SUB_DEFAULT.mesh ?? null;
    this.#back = [];
    this.#fwd = [];
  }

  /**
   * Back to the wizard with no mesh — the demo was stopped (DIR-5).
   * The engine is already gone by the time this runs.
   */
  async toWizard() {
    this.reset();
    this.boot = await api.bootstrapStatus().catch(() => this.boot);
    this.needsWizard = true;
  }

  /** Bring a drained engine back (Settings → "Start engine"). */
  async restartEngine() {
    this.status = await api.startEngine();
    this.engineStopped = false;
    // A restarted engine's change counters start again from zero, and the
    // vector we remember is from the process that is gone (**H-8**).
    this.invalidate();
    await this.refresh();
    await this.refreshExtended();
    this.startPolling();
  }

  /** The engine stopped after a drain: freeze the tables, stop polling. */
  markEngineStopped() {
    this.engineStopped = true;
    this.stopPolling();
  }

  /**
   * Act on a `director://` link, if one is waiting (L2 P5).
   *
   * **The routing table.** Each kind lands on the screen that opens on
   * *that object* — the same guarantee `meshnode` makes when it prints the
   * link. A kind with no screen is refused in the engine (`deeplink.rs`)
   * rather than shrugged at here, so anything that reaches this function
   * has somewhere to go.
   */
  async followDeepLink() {
    let link: DeepLinkView | null = null;
    try {
      link = await api.deepLinkTake();
    } catch {
      // No engine yet, or a build without the scheme registered. Nothing
      // to follow, and nothing worth telling anybody about.
      return;
    }
    if (!link) return;
    switch (link.kind) {
      case "jobs":
        sessionStorage.setItem("jobs:select", link.id);
        this.go("jobs");
        break;
      case "fanouts":
        sessionStorage.setItem("jobs:fanout", link.id);
        this.go("jobs");
        break;
      case "nodes":
        sessionStorage.setItem("nodes:select", link.id);
        this.go("nodes");
        break;
      case "scripts":
        sessionStorage.setItem("library:select", link.id);
        this.go("library");
        break;
      // Phase B, U12: the sheet the Services screen already opens by name.
      case "deployments":
        sessionStorage.setItem("deployments:open", link.id);
        this.go("deployments");
        break;
      default:
        // Unreachable while `deeplink.rs` and this table agree, and a
        // silent no-op if they ever do not.
        break;
    }
  }

  startPolling() {
    if (this.#timer) return;
    this.#timer = setInterval(() => {
      // A hidden window is not a window anybody is reading. Polling one
      // spends a core and a battery keeping pixels fresh behind another
      // application; `visibilitychange` catches the tables up on the way
      // back, which is the only moment the freshness is worth anything.
      if (typeof document !== "undefined" && document.hidden) return;
      void this.followDeepLink();
      void this.refresh();
      if (EXTENDED_VIEWS.has(this.view)) void this.refreshExtended();
      else if (VITALS_VIEWS.has(this.view)) void this.refreshVitals();
      // Deliberately not gated on the current view: the rail badge is the
      // whole point, and a queue that only fills in while you are looking
      // at it cannot tell you to look.
      if (Date.now() - this.#enrolAt >= ENROL_POLL_MS) void this.refreshEnrolments();
    }, POLL_MS);
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", this.#onVisibility);
    }
    this.startLanWatch();
  }

  stopPolling() {
    if (this.#timer) clearInterval(this.#timer);
    this.#timer = null;
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", this.#onVisibility);
    }
    this.stopLanWatch();
  }

  /** Catch up the moment the window comes back, so the pause is invisible. */
  #onVisibility = () => {
    if (document.hidden || !this.ready || this.engineStopped) return;
    void this.refresh();
    void this.refreshEnrolments();
    if (EXTENDED_VIEWS.has(this.view)) void this.refreshExtended();
    else if (VITALS_VIEWS.has(this.view)) void this.refreshVitals();
  };

  // ── where you have been ────────────────────────────────────────────
  //
  // The toolbar opens with a back and a forward chevron, so there has to
  // be somewhere for them to go. A stop is a section *and* its open panel:
  // Mesh › Topology and Mesh › Fleet are two places, and a back button
  // that could not tell them apart would be a back button that does
  // nothing on the screen people spend the most time in.
  #back = $state<{ view: ViewName; sub: string | null }[]>([]);
  #fwd = $state<{ view: ViewName; sub: string | null }[]>([]);

  get canBack() {
    return this.#back.length > 0;
  }
  get canForward() {
    return this.#fwd.length > 0;
  }

  /** The panel a section opens on, honouring an explicit ask. */
  #resolveSub(view: ViewName, want?: string | null): string | null {
    const panels = SUBNAV[view];
    if (!panels) return null;
    if (want && panels.some((p) => p.id === want)) return want;
    return SUB_DEFAULT[view] ?? panels[0].id;
  }

  /**
   * Switch view — and, for a section that has panels, which panel.
   *
   * Extended data refreshes immediately for the views that need it. Every
   * move through here is recorded; `#travel` is the one path that is not,
   * because replaying a stop is not a new stop.
   */
  /** Bumped by every {@link go}, and read by the views that honour a
   *  `sessionStorage` deep link (**Access → approvals**, from the bell).
   *
   *  Without it those `$effect`s run once, at mount. `#step` returns early
   *  when the view has not changed, so pressing the bell while already on
   *  Access set the key and changed nothing reactive — the effect never
   *  re-ran and the button did nothing. `sessionStorage` is not reactive
   *  and cannot be; this is the signal that says *a navigation was just
   *  asked for*, whether or not it moved. */
  deepLink = $state(0);

  go(view: ViewName, sub?: string | null) {
    this.deepLink += 1;
    // The Focus surface folds four old sections into panels of the ones
    // that remain. Every existing deep link — the palette, a "node →
    // facts" jump, an old sessionStorage key — keeps working: it lands on
    // the panel the section became, rather than on a screen the rail no
    // longer has.
    if (view === "nodes" || view === "compute") {
      sub = view;
      view = "mesh";
    }
    // **R6 P1 reverses this for the Library.** It was folded into Run on
    // the argument that a shelf of scripts belongs one tab from the
    // button that runs them. True, and it made Run five tabs deep, four
    // of which were modes and one of which was a place. Scripts is a
    // section again — the thing you do on a different day from running —
    // and the old `run:tab=library` deep link lands there instead of the
    // other way round.
    // Secrets is a section again. It rode inside Access on the argument
    // that "who may act" and "what values they may use" are one
    // governance question — true, and not worth a seventh thing on the
    // one screen a new admin has to understand in a hurry.
    //
    // A panel asked for by `sessionStorage` — the palette and the dev URL
    // form still write `mesh:tab`, and so does the engine's deep link
    // handler — is honoured once and then forgotten.
    if (sub == null && SUBNAV[view]) {
      const pre = sessionStorage.getItem("mesh:tab");
      if (pre) {
        sessionStorage.removeItem("mesh:tab");
        sub = pre;
      }
    }
    const to = this.#retired(view, sub ?? null);
    this.#step(to.view, this.#resolveSub(to.view, to.sub), true);
  }

  /** Move to a panel of the section already open. */
  goSub(sub: string) {
    const to = this.#retired(this.view, sub);
    this.#step(to.view, this.#resolveSub(to.view, to.sub), true);
  }

  /**
   * A retired Mesh panel, sent where its content went (2026-09-14,
   * `RETIRED_PANELS`). Capacity is four views stacked, so a link that
   * named one of them leaves a note saying which to scroll to.
   */
  #retired(view: ViewName, sub: string | null): { view: ViewName; sub: string | null } {
    // Mesh only. `go("compute")` has already become Mesh › compute above.
    if (view !== "mesh") return { view, sub };
    const to = sub ? RETIRED_PANELS[sub] : undefined;
    if (!to) return { view, sub };
    if (to.view) return { view: to.view, sub: null };
    if (to.focus) sessionStorage.setItem("capacity:focus", to.focus);
    return { view: "mesh", sub: to.sub ?? null };
  }

  back() {
    this.#travel(true);
  }
  forward() {
    this.#travel(false);
  }

  #travel(back: boolean) {
    const from = back ? this.#back : this.#fwd;
    if (from.length === 0) return;
    const stop = from[from.length - 1];
    const here = { view: this.view, sub: this.sub };
    if (back) {
      this.#back = this.#back.slice(0, -1);
      this.#fwd = [...this.#fwd, here];
    } else {
      this.#fwd = this.#fwd.slice(0, -1);
      this.#back = [...this.#back, here];
    }
    this.#step(stop.view, stop.sub, false);
  }

  /** The one place `view` and `sub` change. */
  #step(view: ViewName, sub: string | null, record: boolean) {
    if (this.view === view && this.sub === sub) return;
    if (record) {
      // A forward stack survives nothing but a `forward()`, which is what
      // every browser does and what everybody already expects.
      this.#back = [...this.#back.slice(-59), { view: this.view, sub: this.sub }];
      this.#fwd = [];
    }
    this.view = view;
    this.sub = sub;
    if (!this.ready || this.engineStopped) return;
    if (EXTENDED_VIEWS.has(view)) void this.refreshExtended();
    else if (VITALS_VIEWS.has(view)) void this.refreshVitals();
  }

  /**
   * The core tick's fetch, driven by the engine's change signal (**H-8**).
   *
   * This used to re-download the world every two seconds: eight whole
   * collections — every node, every job, every service, every workflow,
   * fifty runs, a hundred approvals — each one built by the engine under
   * its state read lock, serialised, bridged across the webview boundary
   * and parsed again, whether or not a single thing had changed. There was
   * no way to ask whether anything *had*, so the app asked for the answer
   * instead of the question.
   *
   * Now it asks the question first. `stateVersions()` is one small request
   * that builds nothing, and its counters say which topics have moved
   * since the last tick. On an idle mesh that is the entire cost of a
   * tick.
   *
   * Two things are deliberately fetched every time regardless:
   *
   *   · **`status`** is not op-derived — draining, sync position and peer
   *     counts change without anything being authored — so a counter
   *     cannot speak for it.
   *   · **the first tick**, because there is no previous vector to compare
   *     against and "nothing changed since never" is not true.
   */
  /**
   * Fold the engine's change signal into the pending sets (**H-8**).
   *
   * **The diff is accumulated, never consumed by whoever reads it first.**
   * `refresh` and `refreshExtended` are fired from the same tick without
   * awaiting each other, so a single shared "what changed this tick" value
   * is a race: whichever ran second could read the set the first had
   * already replaced, skip a fetch it needed, and — because the counter
   * had by then been recorded as seen — never be told again. That table
   * would sit stale until something else happened to bump the same topic.
   *
   * So each consumer owns a pending set that grows when a topic moves and
   * shrinks only when *that* consumer has successfully fetched it. `null`
   * means "everything", which is the honest state before the first
   * reading and after any failure.
   */
  async #pollSignal() {
    const epoch = this.#epoch;
    try {
      const versions = await api.stateVersions();
      if (epoch !== this.#epoch) return;
      const prev = this.#versions;
      this.#versions = versions.topics;
      if (prev === null) {
        // Nothing to diff against: "unchanged since never" is not true.
        this.#pending = null;
        this.#pendingExt = null;
        return;
      }
      const names = new Set([...Object.keys(versions.topics), ...Object.keys(prev)]);
      for (const t of names) {
        // Inequality, not ">": the counters are node-local and restart at
        // zero when the daemon does, so a *smaller* number is a change.
        if (versions.topics[t] === prev[t]) continue;
        this.#moves.set(t, (this.#moves.get(t) ?? 0) + 1);
        this.#pending?.add(t);
        this.#pendingExt?.add(t);
      }
    } catch {
      if (epoch !== this.#epoch) return;
      // An engine that cannot answer the cheap question gets asked the
      // expensive ones — the old behaviour, as the fallback rather than as
      // the design.
      this.#versions = null;
      this.#pending = null;
      this.#pendingExt = null;
    }
  }

  /**
   * The core tick's fetch, driven by the change signal (**H-8**).
   *
   * This used to re-download the world every two seconds: eight whole
   * collections — every node, every job, every service, every workflow,
   * fifty runs, a hundred approvals — each one built by the engine under
   * its state read lock, serialised, bridged across the webview boundary
   * and parsed again, whether or not a single thing had changed. There was
   * no way to ask whether anything *had*, so the app asked for the answer
   * instead of the question.
   *
   * `status` is fetched every tick regardless: draining, sync position and
   * peer counts are not op-derived, so no counter can speak for them.
   */
  async refresh() {
    if (this.engineStopped || this.#refreshInFlight) return;
    // The guard `refreshExtended` always had and this did not: on a mesh
    // slow enough to matter, the two-second timer started a second full
    // refresh before the first had returned, and the pile-up made it
    // slower still.
    this.#refreshInFlight = true;
    const epoch = this.#epoch;
    try {
      await this.#pollSignal();
      if (epoch !== this.#epoch) return;
      const pending = this.#pending;
      const want = (topic: string) => pending === null || pending.has(topic);
      const at = new Map(this.#moves);
      /** Mark a topic satisfied — only on a fetch that actually returned,
       *  and only if it has not moved again since the fetch went out. */
      const done = (topic: string) => {
        if (this.#moves.get(topic) === at.get(topic)) this.#pending?.delete(topic);
      };
      // DIR-5: who this person is follows the ops that can change it, and
      // the slow timer for the changes no op records.
      const wantWhoami =
        this.whoami === null ||
        WHOAMI_TOPICS.some(want) ||
        Date.now() - this.#whoamiAt >= WHOAMI_POLL_MS;

      const [status, nodesR, jobsR, servicesR, whoami] = await Promise.all([
        api.status(),
        want("nodes") ? settle(api.nodes()) : null,
        want("jobs") ? settle(api.jobs({ limit: JOBS_PAGE })) : null,
        want("services") ? settle(api.services()) : null,
        wantWhoami ? api.whoami().catch(() => null) : null,
      ]);
      if (epoch !== this.#epoch) return;
      // The first successful pass is what turns "everything" into a set
      // that can be narrowed; until then every tick refetches. Whatever
      // moved while it was out is still owed.
      if (pending === null) this.#pending = this.#movedSince(at);
      this.status = status;
      const nodes = this.#take("nodes", nodesR, this.#pending);
      const jobs = this.#take("jobs", jobsR, this.#pending);
      const services = this.#take("services", servicesR, this.#pending);
      if (nodes) {
        this.nodes = nodes;
        this.selfNode = nodes.find((n) => n.isSelf) ?? null;
        done("nodes");
      }
      if (jobs) {
        this.jobs = jobs.rows;
        this.jobsCounts = jobs.counts;
        done("jobs");
      }
      if (services) {
        this.services = services;
        done("services");
      }
      if (whoami) {
        this.whoami = whoami;
        this.#whoamiAt = Date.now();
        for (const t of WHOAMI_TOPICS) done(t);
      }
      this.lastSync = Date.now();
    } catch {
      // Transient (engine restarting); the next tick retries.
    } finally {
      // A `reset` while this was out already cleared the flag, and may have
      // started the next mesh's first pass under it.
      if (epoch === this.#epoch) this.#refreshInFlight = false;
    }
  }

  /**
   * Forget the change signal, so the next tick refetches everything.
   *
   * For the moments a counter cannot speak for: the engine being stopped
   * and restarted under us, and a restore or import that replaces the log
   * wholesale. An ordinary mutation does *not* need this — it authors an
   * op, which bumps a counter, which is exactly the signal working.
   */
  invalidate() {
    this.#versions = null;
    this.#pending = null;
    this.#pendingExt = null;
  }

  /** Library / secrets / access / capacity data (MASTERPLAN §14.2 surfaces). */
  async refreshExtended() {
    if (this.engineStopped || this.#extendedInFlight) return;
    this.#extendedInFlight = true;
    const epoch = this.#epoch;
    try {
      // Same change signal the core tick uses (**H-8**). These are the
      // RBAC and library tables — the ones that change a few times a day
      // and were being rebuilt every two seconds. `top`/`totals` are
      // exempt: they are folds over live gossip (saturation, load,
      // liveness), which no op records, so a counter cannot speak for
      // them.
      const pending = this.#pendingExt;
      const want = (topic: string) => pending === null || pending.has(topic);
      const at = new Map(this.#moves);
      const done = (topic: string) => {
        if (this.#moves.get(topic) === at.get(topic)) this.#pendingExt?.delete(topic);
      };
      const [top, totals, bundlesR, sourcesR, secretsR, providersR, usersR, grantsR] =
        await Promise.all([
          api.top(),
          // Totals is a fold over the same membership `top` reports, so the
          // two are fetched together: fetching them on different ticks is
          // how a headline number and the table under it come to disagree.
          api.totals().catch(() => null),
          want("bundles") ? settle(api.bundles()) : null,
          want("sources") ? settle(api.sources()) : null,
          want("secrets") ? settle(api.secrets()) : null,
          want("secrets") ? settle(api.secretProviders()) : null,
          want("users") ? settle(api.users()) : null,
          want("grants") ? settle(api.grants()) : null,
        ]);
      if (epoch !== this.#epoch) return;
      if (pending === null) this.#pendingExt = this.#movedSince(at);
      this.top = top;
      this.totals = totals;
      const bundles = this.#take("bundles", bundlesR, this.#pendingExt);
      const sources = this.#take("sources", sourcesR, this.#pendingExt);
      // Two calls, one topic: the names and the providers they may come
      // from are one screen, and either refusal is that screen's error.
      // The providers are only looked at once the names came back, so a
      // refused list is not cleared again by a providers call that worked.
      const secrets = this.#take("secrets", secretsR, this.#pendingExt);
      const providers = secrets ? this.#take("secrets", providersR, this.#pendingExt) : null;
      const users = this.#take("users", usersR, this.#pendingExt);
      const grants = this.#take("grants", grantsR, this.#pendingExt);
      if (bundles) {
        this.bundles = bundles;
        done("bundles");
      }
      if (sources) {
        this.sources = sources;
        done("sources");
      }
      if (secrets && providers) {
        this.secrets = secrets;
        this.providers = providers;
        done("secrets");
      }
      if (users) {
        this.users = users;
        done("users");
      }
      if (grants) {
        this.grants = grants;
        done("grants");
      }
    } catch {
      // Same as refresh(): transient, retried on the next tick.
    } finally {
      if (epoch === this.#epoch) this.#extendedInFlight = false;
    }
  }

  /**
   * A settled topic's value, with its error recorded or cleared (UX-2).
   * `null` in means the topic was not asked for this tick, and changes
   * nothing.
   *
   * A failed topic goes back into `pending`, the consumer's set of topics
   * still owed a fetch. Without that, a topic refused on the *first* pass
   * — when the set is built fresh — was never asked for again until its
   * counter moved, so a grant made afterwards left the refusal on screen.
   */
  #take<T>(topic: LoadTopic, got: Settled<T> | null, pending: Set<string> | null): T | null {
    if (!got) return null;
    const had = this.loadErrors[topic];
    if (got.ok) {
      if (had !== undefined) {
        const { [topic]: _, ...rest } = this.loadErrors;
        this.loadErrors = rest;
      }
      return got.value;
    }
    if (had !== got.error) this.loadErrors = { ...this.loadErrors, [topic]: got.error };
    pending?.add(topic);
    return null;
  }

  /**
   * The Retry button on a load error: ask for every table again, now.
   *
   * Forgets the change signal first. A refused topic is still pending, so
   * the next tick would ask anyway; the point of the button is not having
   * to wait for it, and a grant made a moment ago is exactly the change a
   * person presses Retry to see.
   */
  async retryLoad() {
    this.invalidate();
    // The in-flight guards return early rather than queueing, so wait for
    // a pass already under way before starting the one that was asked for.
    await this.#settled();
    await Promise.all([this.refresh(), this.refreshExtended()]);
  }

  /**
   * Catch every table up with a change this window just made (DIR-7).
   *
   * What a view calls after it pushes, grants, sets or removes something.
   * `refreshExtended` alone fetches only the topics the change signal has
   * already said moved, and the signal is read by the core tick — so
   * straight after a push nothing had said so yet, the new bundle was not
   * fetched, and dropping a script onto Scripts added it without opening
   * the wizard on it. It also returns at once when a tick's pass is out,
   * leaving the caller with that pass's answer from before the change.
   *
   * So: read the signal now, wait for any pass already under way, fetch,
   * and if a tick's pass started in between and ours stood aside for it,
   * wait for that one too — it started after the signal was read, so its
   * answer includes the change.
   */
  async refreshAfterChange() {
    if (this.engineStopped) return;
    await this.#pollSignal();
    await this.#settled();
    await Promise.all([this.refresh(), this.refreshExtended()]);
    await this.#settled();
  }

  /** Until no core or extended pass is out, for at most two seconds. */
  async #settled() {
    for (let i = 0; i < 40 && (this.#refreshInFlight || this.#extendedInFlight); i++) {
      await sleep(50);
    }
  }

  #refreshInFlight = false;
  /** Last change-signal vector seen; `null` means "refetch everything". */
  #versions: Record<string, number> | null = null;
  /** Topics `refresh` still owes a fetch for. `null` = all of them. */
  #pending: Set<string> | null = null;
  /** The same, for `refreshExtended` — see [`#pollSignal`] on why the two
   *  cannot share one set. */
  #pendingExt: Set<string> | null = null;
  /**
   * How many times the signal has said each topic moved (DIR-7).
   *
   * The signal and a pass's fetches run side by side on every tick, so a
   * topic can move again while a fetch answered *before* that move is
   * still out. Marking the topic seen when that fetch lands dropped the
   * second change until the topic happened to move a third time — for
   * grants or bundles, which move a few times a day, a table left stale
   * indefinitely. A pass notes these counts when its fetches go out and
   * marks a topic seen only if it has not moved since.
   */
  #moves = new Map<string, number>();

  /** Topics that have moved since `at` was noted. */
  #movedSince(at: Map<string, number>): Set<string> {
    const out = new Set<string>();
    for (const [t, n] of this.#moves) if (at.get(t) !== n) out.add(t);
    return out;
  }

  #vitalsInFlight = false;

  /** Saturation and telemetry alone — see [`VITALS_VIEWS`]. */
  async refreshVitals() {
    if (this.engineStopped || this.#vitalsInFlight || this.#extendedInFlight) return;
    this.#vitalsInFlight = true;
    const epoch = this.#epoch;
    try {
      const [top, totals] = await Promise.all([api.top(), api.totals().catch(() => null)]);
      if (epoch !== this.#epoch) return;
      this.top = top;
      this.totals = totals;
    } catch {
      // Transient; the next tick retries.
    } finally {
      if (epoch === this.#epoch) this.#vitalsInFlight = false;
    }
  }

  // -------------------------------------------------------------------------
  // Letting people in
  //
  // Joining used to be two unrelated acts on two unrelated screens: a
  // *machine* paired (Mesh → Add node → Discover → scan, by hand, while the
  // other person watched a spinner), and then the *person* on it had to
  // separately ask to be recognised. Nobody was told about the second half,
  // and nothing on the admin's side ever spoke up about either.
  //
  // Both are now one queue that finds the admin rather than waiting to be
  // found: enrolment requests off the poll, machines off a background mDNS
  // watch. Neither costs anything on a mesh where nobody is joining.
  // -------------------------------------------------------------------------

  /** Who is asking to be recognised. Cheap, and only an admin can answer. */
  async refreshEnrolments() {
    if (this.engineStopped || this.#enrolInFlight || !this.isAdmin) {
      if (!this.isAdmin) this.enrolments = [];
      return;
    }
    this.#enrolInFlight = true;
    const epoch = this.#epoch;
    try {
      const enrolments = await api.enrolments();
      if (epoch !== this.#epoch) return;
      this.enrolments = enrolments;
      this.#enrolAt = Date.now();
    } catch {
      // Older peer, or a transient. Retried on the next tick.
    } finally {
      if (epoch === this.#epoch) this.#enrolInFlight = false;
    }
  }

  /**
   * Read the engine's LAN watch, and keep it running while it should be.
   *
   * The listening itself happens in the engine (`pairing::LanWatch`) —
   * one mDNS browse and one beacon socket for as long as this peer is an
   * admin — and this loop only copies its table over. It used to *be* the
   * listener, sweeping the network in 2.5s windows from here and pausing
   * whenever the window was hidden, which is how a joiner could announce
   * for a minute and never be seen.
   */
  startLanWatch() {
    if (typeof window === "undefined") return;
    const gen = ++this.#lanGen;
    void (async () => {
      let started = false;
      while (this.#lanGen === gen) {
        const idle = !this.ready || this.engineStopped || !this.isAdmin || !this.watchingLan;
        if (idle) {
          if (started) {
            started = false;
            await api.lanWatchStop().catch(() => {});
          }
          if (this.lanCandidates.length) this.lanCandidates = [];
          this.lanWatch = null;
          await sleep(LAN_IDLE_MS);
          continue;
        }
        try {
          if (!started) {
            await api.lanWatchStart();
            started = true;
          }
          const view = await api.lanWatch();
          if (this.#lanGen !== gen) return;
          this.lanWatch = view;
          this.lanCandidates = view.candidates;
          this.lanPolledAt = Date.now();
        } catch {
          // The engine is restarting, or an older build without the
          // watch. Nothing to report loudly; the next tick retries.
          started = false;
        }
        await sleep(LAN_POLL_MS);
      }
    })();
  }

  stopLanWatch() {
    this.#lanGen++;
    this.lanCandidates = [];
    this.lanWatch = null;
    void api.lanWatchStop().catch(() => {});
  }

  /** Forget one candidate immediately — it just paired, or was dismissed. */
  dropCandidate(nodeName: string) {
    this.lanCandidates = this.lanCandidates.filter((c) => c.nodeName !== nodeName);
  }

  flash(message: string) {
    this.toast = message;
    setTimeout(() => {
      if (this.toast === message) this.toast = null;
    }, 3500);
  }
}

export const app = new AppState();
onKeyPaused((why) => app.pauseForKey(why));


// ---------------------------------------------------------------------------
// Small formatting helpers shared by views
// ---------------------------------------------------------------------------

export function ago(micros: number | null): string {
  if (!micros) return "—";
  const secs = Math.max(0, Date.now() / 1000 - micros / 1e6);
  if (secs < 5) return "now";
  if (secs < 60) return `${Math.floor(secs)}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

export function until(micros: number | null): string {
  if (!micros) return "never";
  const secs = micros / 1e6 - Date.now() / 1000;
  if (secs <= 0) return "expired";
  if (secs < 3600) return `in ${Math.ceil(secs / 60)}m`;
  if (secs < 86400) return `in ${Math.floor(secs / 3600)}h`;
  return `in ${Math.floor(secs / 86400)}d`;
}

export function uptime(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
}

export function fmtCountdown(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/**
 * The file name a backup is offered under (UX-15).
 *
 * One name and one extension wherever a backup is written: the Mesh
 * screen's button used to offer `<mesh>-<date>.mwbak` while Settings and
 * the CLI wrote `meshware-backup-<node>.mwb` — the same bytes, from the same
 * `backup_write`, under two types the wizard's restore did not both accept.
 */
export function backupFileName(nodeName: string | null | undefined): string {
  return `meshware-backup-${(nodeName || "director").replace(/[^a-zA-Z0-9_.-]/g, "_")}.mwb`;
}

export function initialsOf(name: string): string {
  return name.replace(/[^a-zA-Z0-9]/g, "").slice(0, 2).toLowerCase() || "??";
}

/** Shared fragments so every form in the app reads as one system. The
 *  identities (`card`, `errbox`, `btn-lg`…) live in app.css — these are
 *  just the compositions a form reaches for. */
export const ui = {
  input: "field field-sunken h-10 w-full px-3 text-base outline-none",
  inputMono: "field field-sunken mono h-10 w-full px-3 text-sm outline-none",
  label: "mb-1.5 block text-sm text-ink-4",
  primary: "btn btn-primary btn-lg shrink-0",
  secondary: "btn btn-secondary btn-lg shrink-0 text-sm",
  rowAction: "btn btn-secondary h-8 rounded-sm px-3 text-xs",
  rowDanger: "btn btn-danger h-8 rounded-sm px-3 text-xs",
  card: "card p-[22px]",
  errorBox: "errbox mt-3",
};
