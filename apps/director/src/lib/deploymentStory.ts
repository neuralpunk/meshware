// A deployment, in words (2026-09-14).
//
// The deployment sheet used to print the reconciler's vocabulary back at
// the reader: `progressing`, a table of slots with `absent` / `queued` /
// `starting` in two columns, a generation number, and a reconciler name.
// Every word was true, and none of them answered what somebody opening the
// sheet is asking — *why is that copy not up, what is it doing now, and
// what did my Scale just set in motion?* So this file turns the same status
// into sentences, the way `story.ts` turns audit rows into them. Three
// things come out of it:
//
//   1. **Right now** — one headline with its reason and what happens next,
//      picked in the order a person needs to hear them: a pause outranks a
//      failure (nothing will fix it while paused), a failure outranks a roll,
//      and "everything is running" is only said when it is.
//   2. **A stage for every copy** — waiting for a machine, starting,
//      checking health, healthy, or failing / not answering — so a size
//      change is visible as copies walking along a track, not a count
//      silently changing.
//   3. **Activity** — the audit rows that name this deployment and each
//      copy's incarnations, merged into one newest-first timeline.
//
// The rule `story.ts` keeps holds here too: **nothing is invented.** Every
// sentence is built from fields the engine returned, and a reason the engine
// gave is shown as given rather than paraphrased.

import type { AuditView } from "./bindings/AuditView";
import type { DeploymentInstanceView } from "./bindings/DeploymentInstanceView";
import type { DeploymentStatusView } from "./bindings/DeploymentStatusView";
import type { IncarnationView } from "./bindings/IncarnationView";

// -- small words ---------------------------------------------------------------

function count(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}
const copies = (n: number) => count(n, "copy", "copies");
const isAre = (n: number) => (n === 1 ? "is" : "are");

/** "a, b and c". */
function list(parts: readonly string[]): string {
  if (parts.length <= 1) return parts[0] ?? "";
  return `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1]}`;
}

function capitalise(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

// -- copies --------------------------------------------------------------------

/** Where one copy is on its way to serving. */
export type Stage = "waiting" | "next" | "starting" | "checking" | "healthy" | "failing" | "away" | "stopped";

/** In the order the legend reads. */
export const STAGES: readonly { id: Stage; label: string }[] = [
  { id: "healthy", label: "Healthy" },
  { id: "checking", label: "Checking health" },
  { id: "starting", label: "Starting" },
  { id: "waiting", label: "Waiting for a machine" },
  // Not stuck: mid-roll, its old copy still serving (2026-09-14).
  { id: "next", label: "Waiting its turn" },
  { id: "failing", label: "Failing" },
  { id: "away", label: "Machine not answering" },
  { id: "stopped", label: "Stopped" },
];

/** How far along Placed → Started → Healthy a stage has got. */
export const STEP: Record<Stage, number> = {
  waiting: 0,
  next: 0,
  starting: 1,
  checking: 2,
  healthy: 3,
  failing: 2,
  away: 2,
  stopped: 3,
};

export function stageOf(i: DeploymentInstanceView): Stage {
  if (i.state === "absent") return "waiting";
  // Ahead of the state: whatever its node last said is the one thing about
  // this copy that can no longer be updated.
  if (i.health === "unreachable") return "away";
  switch (i.state) {
    case "queued":
      return "starting";
    case "running":
      if (i.health === "unhealthy") return "failing";
      if (i.health === "starting") return "checking";
      // `healthy`, or `unknown` — no check is defined, and running is as
      // healthy as the engine can tell.
      return "healthy";
    case "failed":
    case "timed-out":
      return "failing";
    default:
      return "stopped";
  }
}

/** `0` → `Copy 1`. A system deployment's slots are machine names already. */
export function slotName(slot: string): string {
  return /^\d+$/.test(slot) ? `Copy ${Number(slot) + 1}` : slot;
}

export function describeCopy(i: DeploymentInstanceView, stage: Stage): { text: string; why: string | null } {
  const on = i.node ? ` on ${i.node}` : "";
  switch (stage) {
    case "waiting":
      return {
        text: "Waiting for a machine",
        why: i.healthDetail
          ? capitalise(i.healthDetail.replace(/^not placed — /, ""))
          : "Meshware is looking for a machine that fits what this deployment asks for.",
      };
    case "starting":
      return { text: `Placed${on} — starting up`, why: null };
    case "checking":
      return { text: `Running${on} — waiting for its health check to pass`, why: null };
    case "healthy":
      return { text: i.health === "unknown" ? `Running${on} · no health check` : `Healthy${on}`, why: null };
    case "failing":
      return i.state === "running"
        ? { text: `Running${on} but failing its health check`, why: i.healthDetail || null }
        : {
            text: `Stopped${on} — ${i.state}`,
            // The node says how it ended and when it is tried again — a
            // clause, lower-case like every other detail it sends.
            why: i.healthDetail ? capitalise(i.healthDetail) : "Meshware starts it again, waiting longer each time it fails.",
          };
    case "away":
      return {
        text: `${i.node || "Its machine"} is not answering`,
        why: "The last word from that machine was that this copy was running. If it stays away, the copy is started somewhere else.",
      };
    default:
      return { text: `Stopped${on}`, why: null };
  }
}

export type CopyRow = {
  key: string;
  slot: string;
  name: string;
  /** Null for a copy that was asked for and that no row speaks for yet. */
  inst: DeploymentInstanceView | null;
  stage: Stage;
  /** A previous generation's copy, on its way out. */
  old: boolean;
  text: string;
  why: string | null;
};

export type Copies = { current: CopyRow[]; old: CopyRow[]; tally: Record<Stage, number> };

export function copyRows(s: DeploymentStatusView): Copies {
  const tally = Object.fromEntries(STAGES.map((x) => [x.id, 0])) as Record<Stage, number>;
  const current: CopyRow[] = [];
  const old: CopyRow[] = [];
  s.instances.forEach((i, n) => {
    const stage = stageOf(i);
    const isOld = i.generation !== s.generation;
    const row: CopyRow = { key: `${i.jobId}:${i.slot}:${n}`, slot: i.slot, name: slotName(i.slot), inst: i, stage, old: isOld, ...describeCopy(i, stage) };
    if (isOld) old.push(row);
    else {
      current.push(row);
      tally[stage] += 1;
    }
  });
  // A size raised a moment ago can be ahead of the rows: say the new copies
  // are waiting rather than drawing fewer copies than were asked for.
  if (s.count !== "system") {
    for (let k = current.length; k < s.desired; k++) {
      current.push({
        key: `asked:${k}`,
        slot: String(k),
        name: slotName(String(k)),
        inst: null,
        stage: "waiting",
        old: false,
        text: "Waiting for a machine",
        why: "Just asked for — Meshware places it on its next pass.",
      });
      tally.waiting += 1;
    }
  }
  // Mid-roll, a slot whose new copy has not started is not stuck: its old
  // copy is still serving, and it is replaced in its batch — or, in a canary,
  // once the canary is promoted. "Waiting for a machine" there would send
  // somebody looking for a capacity problem that does not exist. An engine
  // reason for not placing it still wins: then it *is* stuck.
  const replacing = new Set(old.map((o) => o.slot));
  for (const row of current) {
    if (row.stage !== "waiting" || row.inst?.healthDetail) continue;
    if (!(replacing.has(row.slot) || (row.inst === null && old.length > 0))) continue;
    tally.waiting -= 1;
    tally.next += 1;
    row.stage = "next";
    if (s.canarying || s.awaitingPromotion) {
      row.text = "Waiting for the canary";
      row.why = "Its old copy keeps serving until the canary is promoted.";
    } else {
      row.text = "Waiting its turn";
      row.why = "Its old copy keeps serving until this one is replaced, a few at a time.";
    }
  }
  return { current, old, tally };
}

// -- right now -----------------------------------------------------------------

export type NowTone = "ok" | "working" | "waiting" | "problem" | "paused";
export type NowAction = "promote" | "resume" | "edit" | "rollback" | "scale";

export type Now = {
  tone: NowTone;
  headline: string;
  detail: string;
  /** Reasons the engine gave, shown as given. */
  why: string[];
  next: string | null;
  action: NowAction | null;
};

export function explainNow(s: DeploymentStatusView): Now {
  const { current, old, tally } = copyRows(s);
  const desired = s.desired;
  const healthy = tally.healthy;
  const away = tally.away;
  const awayWhy = away > 0 ? [`${capitalise(copies(away))} ${isAre(away)} on a machine that is not answering. ${s.availability}`] : [];
  const firstFailing = current.find((c) => c.stage === "failing");

  if (s.status === "paused") {
    return {
      tone: "paused",
      headline: "Paused — Meshware is not looking after it",
      detail: "Copies that are running keep running, but if one stops nothing replaces it, and changes are not rolled out.",
      why: [],
      next: "Resume it to have Meshware keep it at size again.",
      action: "resume",
    };
  }

  if (s.placement) {
    const short = Math.max(0, desired - s.running);
    const looked = s.placement.considered;
    return {
      tone: s.placement.transient ? "waiting" : "problem",
      headline: short > 0 ? `No machine can take ${copies(short)} right now` : "No machine fits what this deployment asks for",
      detail: `You asked for ${copies(desired)} and ${s.running} ${isAre(s.running)} running. ${
        looked > 0 ? `None of the ${count(looked, "machine", "machines")} Meshware looked at fits:` : "No machine matches where this deployment may run:"
      }`,
      why: [...s.placement.lines, ...awayWhy],
      next: s.placement.transient
        ? "This clears on its own when a machine frees up — Meshware keeps trying."
        : "Change what it asks for or where it may run, or add a machine that fits.",
      action: s.placement.transient ? null : "edit",
    };
  }

  if (s.degradedReason) {
    return {
      tone: "problem",
      headline: "Copies keep failing",
      detail: `Meshware is trying to keep ${copies(desired)} up, but they keep stopping — so it has slowed its retries down and tries other machines where it can.`,
      why: [s.degradedReason, ...(s.stalledReason ? [s.stalledReason] : []), ...awayWhy],
      next: firstFailing ? `Open the logs of ${firstFailing.name} below to see why it stops.` : "Open a copy's logs below to see why it stops.",
      action: null,
    };
  }

  if (s.stalledReason) {
    return {
      tone: "problem",
      headline: "The update is stuck",
      detail: `The new copies of generation ${s.generation} are not becoming healthy, so Meshware has stopped replacing old ones — the copies still running keep serving.`,
      why: [s.stalledReason, ...awayWhy],
      next: "Look at the new copies' logs. If the new version is broken, roll back to the last one that worked.",
      action: "rollback",
    };
  }

  const fresh = current.filter((c) => c.inst && c.inst.state !== "absent").length;

  if (s.awaitingPromotion) {
    return {
      tone: "waiting",
      headline: "The new version is waiting for your go-ahead",
      detail: `The canary — ${count(fresh, "new copy", "new copies")} of generation ${s.generation} — is healthy beside the old version. Nothing switches over until you promote it.`,
      why: awayWhy,
      next: "Promote it to replace the rest in batches, or roll back to stay on the old version.",
      action: "promote",
    };
  }

  if (s.canarying) {
    const proving = tally.starting + tally.checking;
    return {
      tone: "working",
      headline: "Trying the new version on a canary first",
      detail: `${capitalise(count(fresh, "new copy", "new copies"))} of generation ${s.generation} ${isAre(fresh)} running beside the old version${
        proving > 0 ? `, and ${proving} ${isAre(proving)} still proving ${proving === 1 ? "itself" : "themselves"}` : ""
      }. If the canary stays healthy, the rest are replaced.`,
      why: awayWhy,
      next: "Nothing for you to do yet.",
      action: null,
    };
  }

  if (s.updatingFrom != null || old.length > 0) {
    const from = s.updatingFrom ?? Math.max(...old.map((o) => o.inst?.generation ?? 0));
    return {
      tone: "working",
      headline: `Rolling out generation ${s.generation}`,
      detail: `Replacing the copies of generation ${from} a few at a time: ${healthy} of ${desired} new ${desired === 1 ? "copy" : "copies"} ${isAre(healthy)} healthy, and ${copies(old.length)} of the old version ${isAre(old.length)} still serving.`,
      why: awayWhy,
      next: "An old copy is stopped only once its replacement passes its health check, so the service stays up the whole way.",
      action: null,
    };
  }

  if (desired === 0) {
    return s.count === "system"
      ? {
          tone: "waiting",
          headline: "No machine matches where it may run",
          detail: `It runs one copy on every machine matching ${s.targets}, and none does right now.`,
          why: [],
          next: "It starts on any machine that joins and matches.",
          action: "edit",
        }
      : { tone: "waiting", headline: "It asks for no copies", detail: "Nothing runs until it is scaled up.", why: [], next: null, action: "scale" };
  }

  if (s.running > desired && healthy >= desired) {
    const extra = s.running - desired;
    return {
      tone: "working",
      headline: `Scaling down to ${copies(desired)}`,
      detail: `Stopping ${copies(extra)} that ${extra === 1 ? "is" : "are"} no longer needed. The ${copies(desired)} that stay are not restarted.`,
      why: awayWhy,
      next: null,
      action: null,
    };
  }

  const settling = tally.waiting + tally.starting + tally.checking + tally.failing + away;
  if (healthy < desired || settling > 0) {
    const parts: string[] = [];
    if (tally.waiting) parts.push(`${tally.waiting} waiting for a machine`);
    if (tally.starting) parts.push(`${tally.starting} placed and starting`);
    if (tally.checking) parts.push(`${tally.checking} running and waiting for ${tally.checking === 1 ? "its" : "their"} health check`);
    if (tally.failing) parts.push(`${tally.failing} failing`);
    if (away) parts.push(`${away} on a machine that is not answering`);
    const stuck = [...new Set(current.filter((c) => c.stage === "waiting" && c.inst?.healthDetail).map((c) => c.why ?? ""))].filter(Boolean);
    const onlyFailing = tally.failing > 0 && tally.waiting + tally.starting + tally.checking === 0;
    return {
      tone: tally.failing > 0 ? "problem" : "working",
      headline: onlyFailing ? `${capitalise(copies(tally.failing))} failing` : `Starting ${copies(Math.max(1, desired - healthy))}`,
      detail: `You asked for ${copies(desired)} and ${healthy} ${isAre(healthy)} healthy. Of the rest: ${list(parts)}.`,
      why: [...stuck, ...awayWhy],
      next:
        tally.failing > 0
          ? "Meshware restarts a failing copy, waiting longer each time. Open its logs below to see why it stops."
          : "Nothing for you to do — Meshware keeps working on it. A copy that stays stuck says why below.",
      action: null,
    };
  }

  const machines = new Set(current.map((c) => c.inst?.node).filter(Boolean)).size;
  const noCheck = current.length > 0 && current.every((c) => c.inst?.health === "unknown");
  return {
    tone: "ok",
    headline: "Everything is running",
    detail: `${desired === 1 ? "Its one copy is" : `All ${copies(desired)} are`} ${noCheck ? "running" : "healthy"}${
      machines > 0 ? ` on ${count(machines, "machine", "machines")}` : ""
    }.${noCheck ? " There is no health check, so running is as healthy as Meshware can tell." : ""}`,
    why: [],
    next: "Meshware is watching: if a copy stops or its machine goes away, a replacement starts on its own.",
    action: null,
  };
}

// -- activity ------------------------------------------------------------------

export type ActivityTone = "ok" | "info" | "warn" | "error" | "muted";

export type Activity = {
  key: string;
  at: number;
  tone: ActivityTone;
  /** The person who did it; null when Meshware did. */
  who: string | null;
  text: string;
};

/** What a person did, and what it reads as when they were refused. */
const DONE_BY_PEOPLE: Record<string, { did: string; tried: string; tone: ActivityTone }> = {
  "deployment.create": { did: "created it", tried: "create it", tone: "ok" },
  "deployment.put": { did: "published a change", tried: "publish a change", tone: "info" },
  "deployment.pause": { did: "paused it", tried: "pause it", tone: "muted" },
  "deployment.promote": { did: "promoted the canary", tried: "promote it", tone: "ok" },
  "deployment.knobs": { did: "changed how it rolls out", tried: "change how it rolls out", tone: "info" },
  "deployment.remove": { did: "deleted it", tried: "delete it", tone: "error" },
  "deployment.tag": { did: "tagged a version", tried: "tag a version", tone: "muted" },
  "deployment.untag": { did: "removed a version's tag", tried: "remove a tag", tone: "muted" },
};

function afterGeneration(detail: string): string {
  return detail.replace(/^generation \d+[:\s]*/, "").trim();
}

function auditSentence(r: AuditView): Omit<Activity, "key" | "at"> | null {
  const person = DONE_BY_PEOPLE[r.action];
  if (person) {
    if (r.outcome !== "ok") return { tone: "error", who: r.username, text: `tried to ${person.tried} and was refused — ${r.outcome}` };
    const gen = /generation (\d+)/.exec(r.detail)?.[1];
    let text = person.did;
    if (r.action === "deployment.promote" && gen) text = `promoted generation ${gen}`;
    // A create, a pause and a delete are the whole event; the generation
    // their rows carry only says which record they were stamped on.
    else if (r.action === "deployment.create" || r.action === "deployment.pause" || r.action === "deployment.remove") text = person.did;
    else if (gen) text = `${text} — now generation ${gen}`;
    else if (r.detail) text = `${text} (${r.detail})`;
    return { tone: person.tone, who: r.username, text };
  }
  switch (r.action) {
    case "deployment.stalled":
      return { tone: "warn", who: null, text: `The rollout stalled — ${afterGeneration(r.detail)}` };
    case "deployment.degraded":
      return { tone: "error", who: null, text: `Marked degraded — ${afterGeneration(r.detail)}` };
    case "deployment.auto_revert":
      return { tone: "warn", who: null, text: `Rolled back on its own — ${afterGeneration(r.detail)}` };
    case "deployment.unplaced": {
      const m = /^generation \d+ slot ([^:]+):\s*(.*)$/.exec(r.detail);
      return { tone: "warn", who: null, text: m ? `Could not place ${slotName(m[1])} — ${m[2]}` : `Could not place a copy — ${r.detail}` };
    }
    default:
      // `deployment.status`, `.show`, `.history`: reads, not events.
      return null;
  }
}

/** The audit rows that are about `name`, from `since` on — a deployment
 *  deleted and made again under the same name starts a new story. */
export function auditActivity(rows: readonly AuditView[], name: string, since = 0): Activity[] {
  const out: Activity[] = [];
  for (const r of rows) {
    if (r.target !== name || r.atMicros < since) continue;
    const said = auditSentence(r);
    if (said) out.push({ key: `a:${r.auditId}`, at: r.atMicros, ...said });
  }
  return out;
}

/** One slot's incarnations as events: placed, started, and how it ended. */
export function copyActivity(slot: string, incarnations: readonly IncarnationView[], generation: number): Activity[] {
  const name = slotName(slot);
  const out: Activity[] = [];
  for (const inc of incarnations) {
    const gen = inc.generation !== generation ? ` (generation ${inc.generation})` : "";
    out.push({ key: `p:${inc.jobId}`, at: inc.createdAtMicros, tone: "info", who: null, text: `${name} was placed on ${inc.node}${gen}` });
    if (inc.startedAtMicros) {
      out.push({ key: `s:${inc.jobId}`, at: inc.startedAtMicros, tone: "ok", who: null, text: `${name} started on ${inc.node}${gen}` });
    }
    if (inc.finishedAtMicros) {
      const failed = inc.state === "failed" || inc.state === "timed-out";
      const text = failed
        ? `${name} ${inc.state === "timed-out" ? "timed out" : "failed"} on ${inc.node}${inc.exitCode != null ? ` with exit code ${inc.exitCode}` : ""}${gen}`
        : inc.state === "cancelled"
          ? `${name} was stopped on ${inc.node}${gen} — replaced, moved or no longer needed`
          : `${name} exited on ${inc.node}${gen}`;
      out.push({ key: `f:${inc.jobId}`, at: inc.finishedAtMicros, tone: failed ? "error" : "muted", who: null, text });
    }
  }
  return out;
}

/** Newest first, each event once. */
export function mergeActivity(...lists: Activity[][]): Activity[] {
  const seen = new Set<string>();
  const out: Activity[] = [];
  for (const a of lists.flat()) {
    if (a.at <= 0 || seen.has(a.key)) continue;
    seen.add(a.key);
    out.push(a);
  }
  return out.sort((a, b) => b.at - a.at);
}
