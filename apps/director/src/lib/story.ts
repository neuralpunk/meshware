// The audit log, as sentences.
//
// `AuditView` is seven correct columns — when, who, `grant.add`,
// `mwu1tt5r…`, `ok`, `hq`, `operator on nodes=tag=edge services=*` — and a
// person answering for the fleet does not read that. They read *"sam gave
// mia operator on tag=edge"*, which is the same row with the vocabulary
// resolved: the dotted action turned into a verb, the key turned into the
// name it belongs to, and the detail string parsed back into the shape the
// engine formatted it from.
//
// So this file is one function with a large table in it, and the table is
// the interesting part. Two rules keep it honest:
//
//   1. **Nothing is invented.** Every sentence is built only from fields
//      the row carries. Where a detail cannot be parsed it is printed
//      verbatim rather than guessed at, and an action with no template
//      falls back to "did node upgrade cancel on …" — clumsy, never wrong.
//   2. **The engine owns the vocabulary.** These strings mirror the
//      `audit_ok(…, "action", target, detail)` call sites in
//      `mesh-core/src/daemon.rs`. When a call site changes shape the
//      sentence degrades to the fallback, which is why the fallback has to
//      stay readable — and why `story.test.ts` pins the parsers.
//
// The renderer gets pieces rather than a string so a name can be a name:
// people and machines are emphasised, literals the mesh owns are set in
// mono, and the tail of a long detail is quiet.
import type { AuditView } from "./api";

/** One span of a sentence. */
export type Piece = {
  /** `name` — a person or a machine. `thing` — a literal the mesh owns (a
   *  job id, a service, a command). `quiet` — trailing context.
   *  `refusal` — the message the mesh refused with, which is the one span
   *  that must not read as ordinary context. */
  t: "text" | "name" | "thing" | "quiet" | "refusal";
  v: string;
};

/** How the row reads: it happened, it was refused, or it wants a look. */
export type Tone = "ok" | "refused" | "notice";

export interface Story {
  pieces: Piece[];
  tone: Tone;
  /** The refusal message, when the outcome carried one. */
  refusal: string | null;
}

/** Resolve an id the audit log stores to the name a person knows it by. */
export interface Names {
  (id: string): string | null;
}

const t = (v: string): Piece => ({ t: "text", v });
const nm = (v: string): Piece => ({ t: "name", v });
const th = (v: string): Piece => ({ t: "thing", v });
const q = (v: string): Piece => ({ t: "quiet", v });

/** `mwu1tt5rq9…` → `mwu1tt5r…`, for ids nothing can name. */
export function shortId(id: string): string {
  return id.length > 12 ? `${id.slice(0, 8)}…` : id;
}

/**
 * Build a `Names` from whatever the screen already has.
 *
 * Both maps are keyed by the full display string *and* the short one,
 * because the audit log stores whichever the call site had.
 */
export function namesFrom(
  users: readonly { userId: string; shortId: string; username: string }[],
  nodes: readonly { nodeId: string; shortId: string; name: string }[],
): Names {
  const by = new Map<string, string>();
  for (const u of users) {
    by.set(u.userId, u.username);
    by.set(u.shortId, u.username);
  }
  for (const n of nodes) {
    by.set(n.nodeId, n.name);
    by.set(n.shortId, n.name);
  }
  return (id) => by.get(id) ?? null;
}

interface Ctx {
  /** The row's `target`, verbatim. */
  target: string;
  /** The row's `detail`, verbatim. */
  detail: string;
  /** `target` resolved to a name, or shortened when nothing knows it. */
  who: string;
  tone: Tone;
  e: AuditView;
}

/** `ok` and `verified` happened; `denied: …` was refused; the rest — a
 *  degraded deployment, a dropped cache entry, a skipped occurrence —
 *  wants a look without being a failure. */
function toneOf(outcome: string): Tone {
  const o = outcome.trim();
  if (o === "" || o === "ok" || o === "verified") return "ok";
  if (o.startsWith("denied") || o.startsWith("refused")) return "refused";
  return "notice";
}

function refusalOf(outcome: string): string | null {
  const m = /^(?:denied|refused)\s*:\s*(.+)$/i.exec(outcome.trim());
  return m ? m[1] : null;
}

/** Everything after the first " — ", which is where the engine puts the
 *  part of a detail that is context rather than substance. */
function head(detail: string): string {
  const i = detail.indexOf(" — ");
  return i === -1 ? detail : detail.slice(0, i);
}

const opt = (v: string, pre = " — "): Piece[] => (v.trim() ? [q(pre + v.trim())] : []);

// ── The table ────────────────────────────────────────────────────────────
//
// Each entry is the sentence *after* the actor, who is prepended by
// `storyOf`. Anything the mesh did on its own account lives in `MESH`
// below and writes its own opening.

const AFTER: Record<string, (c: Ctx) => Piece[]> = {
  // -- work ---------------------------------------------------------------
  "job.run": (c) => {
    if (c.tone !== "ok") {
      const what = c.target === "(ad-hoc)" ? "an ad-hoc command" : c.target;
      return [t(" tried to run "), th(what)];
    }
    let m = /^fanout (\d+) × (.+)$/.exec(c.detail);
    if (m) return [t(" ran "), th(m[2]), t(" on "), th(`${m[1]} nodes`), q(` · ${shortId(c.target)}`)];
    m = /^gang (\d+) × (.+?)(?: \((.*)\))?$/.exec(c.detail);
    if (m) {
      return [
        t(" started a gang of "),
        th(m[1]),
        t(" running "),
        th(m[2]),
        ...(m[3] ? [q(` (${m[3]})`)] : []),
        q(` · ${shortId(c.target)}`),
      ];
    }
    m = /^(.+) on (\S+)$/.exec(c.detail);
    if (m) return [t(" ran "), th(m[1]), t(" on "), nm(m[2]), q(` · ${shortId(c.target)}`)];
    return [t(" ran "), th(c.detail || c.target)];
  },
  "job.parallel": (c) => [t(" spread "), th(c.detail || "a parallel run"), q(` · ${shortId(c.target)}`)],
  "job.cached": (c) => {
    const m = /^(.+) served from (\S+) \(produced by (\S+)\)$/.exec(c.detail);
    if (!m) return [t(" reused a cached result"), ...opt(c.detail)];
    return [
      t(" did not re-run "),
      th(m[1]),
      t(" — served from the cache"),
      q(` · first produced by ${m[3]}`),
    ];
  },
  "job.cancel": (c) => [t(" cancelled "), th(shortId(c.target))],
  "job.cancel.force": (c) => [t(" force-cancelled "), th(shortId(c.target)), q(" — the executor was unreachable")],
  "jobs.clear": (c) => [t(" cleared the job history"), ...opt(c.detail)],
  "history.replay": (c) => [t(" replayed "), th(shortId(c.target)), t(" to a history sink"), ...opt(c.detail)],

  // -- people -------------------------------------------------------------
  "user.add": (c) => [t(" added "), nm(c.detail || c.who), t(" to the mesh")],
  "user.attach": (c) => [t(" attached another key to "), nm(c.detail || c.who)],
  "user.revoke": (c) => [t(" removed "), nm(c.who), ...opt(c.detail)],
  "user.rotate": (c) => [t(" replaced "), nm(c.target), t("'s key"), ...opt(c.detail)],
  "grant.add": (c) => {
    const m = /^(\S+) on nodes=(\S*) services=(\S*)$/.exec(c.detail);
    if (!m) return [t(" granted a role to "), nm(c.who), ...opt(c.detail)];
    const scope = m[2] === "*" && m[3] === "*" ? "everywhere" : `on nodes=${m[2]} services=${m[3]}`;
    return [t(" gave "), th(m[1]), t(" to "), nm(c.who), q(` ${scope}`)];
  },
  "grant.revoke": (c) => [t(" revoked a grant"), q(` · ${shortId(c.target)}`)],
  "enrol.request": (c) => [t(" asked to join"), q(` · ${shortId(c.target)}`)],
  "enrol.decline": (c) => [t(" declined "), nm(c.who), t("'s request to join")],
  "serviceaccount.add": (c) => [t(" created the service account "), th(c.target), ...opt(c.detail)],
  "identity.token.add": (c) => [t(" issued an access token "), th(c.target)],
  "identity.token.revoke": (c) => [t(" revoked the access tokens for "), nm(c.who)],
  "oidc.login": (c) => [t(" signed in through "), th(c.target)],
  "oidc.provider.put": (c) => [t(" configured the sign-in provider "), th(c.target), ...opt(c.detail)],
  "oidc.provider.remove": (c) => [t(" removed the sign-in provider "), th(c.target)],
  "invite.create": (c) => [t(" created an invite"), q(` · ${shortId(c.target)}`)],

  // -- machines -----------------------------------------------------------
  "node.pair": (c) => [t(" paired "), nm(c.detail || c.who)],
  "node.revoke": (c) => [t(" revoked "), nm(c.who), ...opt(c.detail)],
  "node.drain": (c) => [
    t(" drained "),
    nm(c.who),
    ...(c.detail === "move" ? [t(" and moved its queued work")] : []),
  ],
  "node.contribute": (c) => [t(" set "), nm(c.who), t(" to "), th(c.detail || "run work")],
  "node.tags": (c) => [t(" retagged "), nm(c.who), ...opt(c.detail)],
  "node.tags.assign": (c) => [t(" retagged "), nm(c.who), ...opt(c.detail)],
  "node.addr": (c) => [t(" changed "), nm(c.who), t("'s address"), ...opt(c.detail)],
  "node.upgrade": (c) => {
    const m = /^(\S+)(?: \((\S+)\))?(.*)$/.exec(c.detail);
    const version = m?.[1] ?? c.detail;
    return [
      t(" started an upgrade on "),
      nm(c.target),
      t(" to "),
      th(version),
      ...(m?.[3]?.includes("forced") ? [q(" — forced")] : []),
    ];
  },
  "node.upgrade.cancel": (c) => [t(" cancelled the upgrade on "), nm(c.who)],
  "release.publish": (c) => [t(" published the release "), th(c.target)],
  "release.key.add": (c) => [t(" trusted a new release key"), q(` · ${shortId(c.detail || c.target)}`)],
  "release.key.revoke": (c) => [t(" revoked a release key"), q(` · ${shortId(c.target)}`)],
  "bundle.push": (c) => [t(" pushed the bundle "), th(shortId(c.target)), ...opt(c.detail, " from ")],

  // -- what the mesh distributes ------------------------------------------
  "service.create": (c) => [t(" published the service "), th(c.target), ...(c.detail ? [q(` ${c.detail}`)] : [])],
  "service.remove": (c) => [t(" retired the service "), th(c.target)],
  "service.rollback": (c) => [t(" rolled the service "), th(c.target), t(" back to "), th(c.detail)],
  // The workflow, schedule and agent verbs left the product, but their audit
  // rows did not: the log is append-only replicated evidence and a mesh that
  // ran them still holds them. These renderers stay so history reads as
  // sentences rather than as raw action strings.
  "workflow.create": (c) => [t(" published the workflow "), th(c.target), ...(c.detail ? [q(` ${c.detail}`)] : [])],
  "workflow.remove": (c) => [t(" removed the workflow "), th(c.target)],
  "workflow.rollback": (c) => [t(" rolled the workflow "), th(c.target), t(" back to "), th(c.detail)],
  "workflow.run": (c) => [t(" ran the workflow "), th(shortId(c.target)), ...opt(c.detail)],
  "workflow.cancel": (c) => [t(" cancelled the workflow run "), th(shortId(c.target))],
  "source.add": (c) => [t(" added the Git source "), th(c.target), ...opt(c.detail)],
  "source.remove": (c) => [t(" removed the Git source "), th(c.target)],
  "source.sync": (c) => [t(" synced the Git source "), th(c.target), ...opt(c.detail, " — commit ")],
  "schedule.remove": (c) => [t(" removed the schedule "), th(c.target)],
  "deployment.remove": (c) => [t(" removed the deployment "), th(c.target)],
  "agent.draft": (c) => [t(" drafted the workflow "), th(c.target), t(" with an agent"), ...opt(c.detail)],
  "agent.provider.set": (c) => [t(" set the agent provider"), ...opt(c.detail)],
  "agent.provider.clear": () => [t(" cleared the agent provider")],

  // -- secrets, policy, approvals -----------------------------------------
  "secret.set": (c) => [t(" set the secret "), th(c.target), ...opt(c.detail)],
  "secret.remove": (c) => [t(" removed the secret "), th(c.target)],
  "secret.resolve": (c) => [t(" resolved the secret "), th(c.target), ...opt(c.detail, " for ")],
  "policy.create": (c) => [
    t(c.detail === "replaced" ? " replaced the policy " : " created the policy "),
    th(c.target),
  ],
  "policy.remove": (c) => [t(" removed the policy "), th(c.target)],
  "policy.override": (c) => [t(" overrode a policy refusal on "), th(c.target), ...opt(c.detail)],
  "policy.refused": (c) => [t(" was refused by policy on "), th(c.target), ...opt(c.detail, " — tried to ")],
  "policy.presence.put": (c) => [
    t(" turned presence checks "),
    th(c.detail || "on"),
    t(" for "),
    th(c.target),
  ],
  "approval.request": (c) => [t(" asked for approval"), ...opt(head(c.detail), " to ")],
  "approval.granted": (c) => [t(" approved "), th(shortId(c.target)), ...opt(c.detail)],
  "approval.cancel": (c) => [t(" withdrew the approval request "), th(shortId(c.target))],

  // -- inventory, history, quotas -----------------------------------------
  "host.import": (c) => [t(" imported an inventory"), ...opt(c.detail)],
  "host.put": (c) => [t(" saved the host "), th(c.target), ...opt(c.detail)],
  "host.remove": (c) => [t(" removed the host "), th(c.target)],
  "host.group.put": (c) => [t(" saved the host group "), th(c.target), ...opt(c.detail)],
  "host.group.remove": (c) => [t(" removed the host group "), th(c.target)],
  "sink.create": (c) => [t(" created the history sink "), th(c.target)],
  "sink.remove": (c) => [t(" removed the history sink "), th(c.target)],
  "sinks.resume": (c) => [t(" resumed the history sinks"), ...opt(c.detail)],
  "retention.put": (c) => [t(" changed how long history is kept"), ...opt(c.detail)],
  "retention.reset": (c) => [t(" reset history retention"), ...opt(c.detail)],
  "cache.purge": (c) => [t(" purged the action cache"), ...opt(c.detail), q(` · ${c.target}`)],
  "quota.borrowed": (c) => [t(" borrowed idle capacity above the quota "), th(c.target), ...opt(c.detail)],
  "quota.refused": (c) => [t(" was refused capacity by the quota "), th(c.target)],

  // -- a hand on a machine -------------------------------------------------
  "shell.open": (c) => [t(" opened a shell on "), nm(c.who), ...opt(c.detail)],
  "shell.close": (c) => [t(" closed the shell on "), nm(c.who), ...opt(c.detail)],
  "session.cp": (c) => [t(" copied files to or from "), nm(c.who), ...opt(c.detail)],
  "session.forward": (c) => [t(" forwarded a port through "), nm(c.who), ...opt(c.detail)],
  "session.close": (c) => [t(" closed the session "), th(shortId(c.target))],
  "session.policy.put": (c) => [t(" changed the interactive-access policy "), th(c.target)],
  "session.policy.remove": (c) => [t(" removed the interactive-access policy "), th(c.target)],
};

/**
 * Rows nobody typed.
 *
 * A schedule that fired, a deployment that went degraded, an op the state
 * store pushed back on: the signature on these belongs to whoever created
 * the thing, not to somebody sitting at a keyboard at 02:00. Leading with
 * their name would read as an accusation, so these write their own opening
 * and the owner goes to the end, quietly.
 */
const MESH: Record<string, (c: Ctx) => Piece[]> = {
  "schedule.fire": (c) => [t("The schedule "), th(c.target), t(" fired"), ...opt(c.detail)],
  "schedule.skip": (c) => [t("The schedule "), th(c.target), t(" skipped an occurrence"), ...opt(c.detail)],
  "deployment.degraded": (c) => [t("The deployment "), th(c.target), t(" went degraded"), ...opt(c.detail)],
  "state.backpressure": (c) => [t("The mesh refused an op on "), th(c.target), ...opt(c.detail)],
  "cache.mismatch": (c) => [
    t("A cache entry disagreed on re-run and was dropped"),
    ...opt(c.detail),
    q(` · ${shortId(c.target)}`),
  ],
};

/** Who the sentence belongs to, when the mesh wrote it rather than a person. */
const OWNER_SUFFIX: Record<string, string> = {
  "schedule.fire": "owned by",
  "schedule.skip": "owned by",
  "deployment.degraded": "owned by",
};

/**
 * One row, as a sentence.
 *
 * `names` is optional: without it every id is shortened rather than
 * resolved, which is what the CLI and a copied export get.
 */
export function storyOf(e: AuditView, names?: Names): Story {
  const tone = toneOf(e.outcome);
  const refusal = refusalOf(e.outcome);
  const target = e.target ?? "";
  const ctx: Ctx = {
    target,
    detail: e.detail ?? "",
    who: (names && names(target)) || shortId(target),
    tone,
    e,
  };

  const actor = e.username?.trim() || "somebody";
  const mesh = MESH[e.action];
  let pieces: Piece[];
  if (mesh) {
    pieces = mesh(ctx);
    const suffix = OWNER_SUFFIX[e.action];
    if (suffix && actor !== "somebody") pieces = [...pieces, q(` · ${suffix} ${actor}`)];
  } else if (tone === "refused" && !OWN_REFUSAL.has(e.action)) {
    pieces = [
      nm(actor),
      t(" tried to "),
      th(refusedPhrase(e.action)),
      ...(target ? [t(" "), th(shortId(target))] : []),
    ];
  } else {
    const after = AFTER[e.action];
    pieces = [nm(actor), ...(after ? after(ctx) : fallback(ctx))];
  }

  if (refusal) pieces = [...pieces, { t: "refusal", v: ` — refused: ${refusal}` }];
  return { pieces, tone, refusal };
}

/**
 * A refused action, said in the infinitive.
 *
 * Every template below is past tense, because almost every row is
 * something that happened. A refusal is not: rendering `service.create`
 * with outcome `denied:` through its template would produce "lab-op
 * published the service x — refused: …", which states as fact the exact
 * thing the mesh stopped. So a refused row is rebuilt from the dotted
 * action instead — `service.create` → "tried to create service x" — and
 * the handful of templates that phrase their own refusal opt out.
 */
const OWN_REFUSAL = new Set(["job.run", "policy.refused", "quota.refused", "state.backpressure"]);

/** `secret.set` → `set secret`, `node.upgrade.cancel` → `cancel node upgrade`. */
function refusedPhrase(action: string): string {
  const parts = action.split(".");
  if (parts.length < 2) return action;
  const verb = parts[parts.length - 1];
  const noun = parts.slice(0, -1).join(" ");
  return `${verb} ${noun}`;
}

/** An action with no template: say the dotted name in words, and stop. */
function fallback(c: Ctx): Piece[] {
  const verb = c.e.action.replace(/\./g, " ");
  return [
    t(c.tone === "refused" ? " tried to " : " did "),
    th(verb),
    ...(c.target ? [t(" on "), th(shortId(c.target))] : []),
    ...opt(c.detail),
  ];
}

/** The sentence as plain text — for the clipboard, and for tests. */
export function storyText(e: AuditView, names?: Names): string {
  return storyOf(e, names)
    .pieces.map((p) => p.v)
    .join("");
}

// ── Days ─────────────────────────────────────────────────────────────────

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

const dayKey = (d: Date) => `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;

/** `14:02`, in the reader's own timezone — the audit log is UTC micros. */
export function clockOf(micros: number): string {
  const d = new Date(micros / 1000);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

/** `Today`, `Yesterday`, or `Mon 8 Sep` — a month never repeats a year. */
export function dayLabelOf(micros: number, now: Date = new Date()): string {
  const d = new Date(micros / 1000);
  const today = dayKey(now);
  const yesterday = dayKey(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1));
  const key = dayKey(d);
  if (key === today) return "Today";
  if (key === yesterday) return "Yesterday";
  const stem = `${DAYS[d.getDay()]} ${d.getDate()} ${MONTHS[d.getMonth()]}`;
  return d.getFullYear() === now.getFullYear() ? stem : `${stem} ${d.getFullYear()}`;
}

export interface Day {
  key: string;
  label: string;
  rows: AuditView[];
}

/**
 * Group rows into days, newest first, preserving the order the daemon
 * returned inside each day.
 *
 * Whatever order the rows arrive in (UX-18). This used to start a new day
 * whenever the key changed, which is only right for rows already sorted:
 * rows out of order put one day in two places, and the keyed list that
 * renders the days threw on the duplicate and left History on "Looking…".
 */
export function groupByDay(rows: readonly AuditView[], now: Date = new Date()): Day[] {
  const byKey = new Map<string, Day>();
  const newest = new Map<string, number>();
  for (const r of rows) {
    const key = dayKey(new Date(r.atMicros / 1000));
    const day = byKey.get(key);
    if (day) day.rows.push(r);
    else byKey.set(key, { key, label: dayLabelOf(r.atMicros, now), rows: [r] });
    newest.set(key, Math.max(newest.get(key) ?? -Infinity, r.atMicros));
  }
  return [...byKey.values()].sort((a, b) => (newest.get(b.key) ?? 0) - (newest.get(a.key) ?? 0));
}

/**
 * The whole window as text, ready for a clipboard or a ticket.
 *
 * Deliberately plain: a day heading, then `14:02  sentence`, then the node
 * that recorded it. Somebody pasting this into an incident review should
 * not have to strip markup out of it.
 */
export function storyExport(rows: readonly AuditView[], names?: Names, now: Date = new Date()): string {
  const out: string[] = [];
  for (const day of groupByDay(rows, now)) {
    out.push(`## ${day.label}`);
    for (const r of day.rows) {
      const where = r.nodeShort ? `  [${r.nodeShort}]` : "";
      out.push(`${clockOf(r.atMicros)}  ${storyText(r, names)}${where}`);
    }
    out.push("");
  }
  return out.join("\n").trimEnd();
}
