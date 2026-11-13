// What one person can actually do, in sentences — the shared answer.
//
// Three surfaces have to agree about this: the roster row ("mia can run
// work and see logs"), the grant editor's live preview ("Mia will be able
// to…"), and the join dialog where an admin picks a role for somebody they
// have never granted anything before. When those three disagreed, the
// screen taught one thing and the mesh did another, which is worse than
// saying nothing at all.
//
// Everything here is *explanation*. Every real decision is made on the node
// being asked, by `crates/mesh-core/src/rbac.rs`. A drift between this file
// and that one shows a wrong sentence; it can never grant a wrong thing.

import type { GrantView, SessionPolicyView, UserView } from "./api";

/** The role sets from `crates/mesh-core/src/rbac.rs`, mirrored. */
export const RUNNERS = ["admin", "operator", "developer"];
export const CONTENT = ["admin", "developer"];
export const SECRET_READ = ["admin", "developer", "auditor"];
export const AUDIT_READ = ["admin", "auditor"];

/**
 * `rbac::glob_match`, mirrored — client-side, and only ever to explain.
 */
export function globMatch(pattern: string, text: string): boolean {
  if (!pattern.includes("*")) return pattern === text;
  const parts = pattern.split("*");
  const first = parts[0];
  const last = parts[parts.length - 1];
  if (!text.startsWith(first)) return false;
  if (text.length < first.length + last.length) return false;
  if (!text.endsWith(last)) return false;
  let rest = text.slice(first.length, text.length - last.length);
  for (const part of parts.slice(1, -1)) {
    if (!part) continue;
    const i = rest.indexOf(part);
    if (i < 0) return false;
    rest = rest.slice(i + part.length);
  }
  return true;
}

export type Capability = {
  /** The plain-words thing somebody wants to know they can do. */
  what: string;
  yes: boolean;
  /** Why — the grant it came from, or the grant that is missing. */
  why: string;
};

export type Scope = { nodes: string; services: string };

/**
 * The capability list, from role names and scopes alone.
 *
 * Split out from the roster so the grant editor can call it with a role
 * nobody holds yet — "here is what this choice would mean" — which is the
 * only moment the answer actually changes anybody's mind.
 */
export function capabilitiesFor(opts: {
  roles: string[];
  scopes: Scope[];
  isFounder: boolean;
  /** Terminal policies naming this person. Omit while previewing. */
  terminalPolicies?: SessionPolicyView[];
  /** How many nodes the scope resolves to, when it is known. */
  nodeCount?: number | null;
}): Capability[] {
  const { roles, scopes, isFounder } = opts;
  const has = (set: string[]) => roles.some((r) => set.includes(r));
  const wideOpen = isFounder || scopes.some((s) => s.services === "*");
  const mine = opts.terminalPolicies ?? [];

  const where = isFounder
    ? "every machine"
    : scopes.length === 0
      ? ""
      : scopes.map((s) => describeNodes(s.nodes)).join(", ");
  const runWhere =
    opts.nodeCount != null && opts.nodeCount >= 0
      ? `${opts.nodeCount} machine${opts.nodeCount === 1 ? "" : "s"}`
      : where;

  return [
    {
      what: "Run work",
      yes: has(RUNNERS),
      // A grant narrowed to a service glob can start *those* scripts and
      // nothing else — not a typed command, and not a shell. Saying "run
      // work" without that qualifier is how people end up surprised.
      why: has(RUNNERS)
        ? wideOpen
          ? `any script or typed command, on ${runWhere || "every machine"}`
          : `only the scripts matching ${scopes.map((s) => s.services).join(", ")} — not typed commands`
        : "needs the admin, operator or developer role",
    },
    {
      what: "Open a terminal",
      yes: has(RUNNERS) && wideOpen && (isFounder || mine.length > 0),
      why: !has(RUNNERS)
        ? "needs a role that can run work first"
        : !wideOpen
          ? `their role is limited to ${scopes.map((s) => s.services).join(", ")} — a terminal belongs to no script, so it needs the unrestricted scope`
          : isFounder
            ? "the founder is covered everywhere"
            : mine.length === 0
              ? "nobody has switched it on for them yet"
              : `on ${mine.map((p) => describeNodes(p.nodes)).join(", ")}, where those machines accept terminals`,
    },
    {
      what: "Upload and change scripts",
      yes: has(CONTENT),
      why: has(CONTENT)
        ? "bundles, services, contracts and Git sources"
        : "needs the admin or developer role",
    },
    {
      what: "See that a secret exists",
      yes: has(SECRET_READ),
      why: has(SECRET_READ)
        ? "names and metadata only — never a value, for anyone"
        : "needs the admin, developer or auditor role",
    },
    {
      what: "Read the history",
      yes: has(AUDIT_READ),
      why: has(AUDIT_READ) ? "every governed action, by everyone" : "needs the admin or auditor role",
    },
    {
      what: "Decide who else gets in",
      yes: has(["admin"]),
      why: has(["admin"])
        ? "people, roles, terminals, invites and revocation"
        : "needs the admin role",
    },
  ];
}

/** A node selector as words: `tag=gpu` → "machines tagged gpu". */
export function describeNodes(selector: string): string {
  const s = (selector || "any").trim();
  if (s === "any" || s === "" || s === "*") return "every machine";
  if (s.startsWith("tag=")) return `machines tagged ${s.slice(4)}`;
  if (s.startsWith("name=")) return s.slice(5);
  return `machines matching ${s}`;
}

/**
 * A grant as a sentence rather than two globs.
 *
 * `crates/meshware-node/src/format.rs::describe_grant`, mirrored — the CLI
 * and this screen have to say the same thing about the same grant.
 */
export function grantSentence(g: {
  role: string;
  nodes: string;
  services: string;
  expiresAtMicros: number | null;
}): string {
  let out = g.role.charAt(0).toUpperCase() + g.role.slice(1);
  const nodes = (g.nodes || "any").trim();
  if (nodes !== "any" && nodes !== "") {
    if (nodes.startsWith("tag=")) out += `, on nodes tagged ${nodes.slice(4)}`;
    else if (nodes.startsWith("name=")) out += `, on ${nodes.slice(5)}`;
    else out += `, on nodes matching ${nodes}`;
  }
  const services = (g.services || "*").trim();
  if (services !== "*" && services !== "") out += `, for services matching ${services}`;
  if (g.expiresAtMicros !== null) {
    out += `, until ${new Date(g.expiresAtMicros / 1000).toLocaleDateString(undefined, {
      day: "numeric",
      month: "long",
    })}`;
  }
  return out;
}

/** The grants that are actually in force for one person. */
export function activeGrantsFor(grants: GrantView[], userId: string): GrantView[] {
  return grants.filter((g) => g.userId === userId && !g.revoked && !g.expired);
}

/** Roles in force. The founder is an admin from genesis, grant or no grant. */
export function rolesOf(grants: GrantView[], userId: string, isFounder: boolean): string[] {
  const roles = new Set(activeGrantsFor(grants, userId).map((g) => g.role));
  if (isFounder) roles.add("admin");
  return [...roles];
}

/** Node/service scopes this person holds a *runner* grant for. */
export function runnerScopes(grants: GrantView[], userId: string): Scope[] {
  return activeGrantsFor(grants, userId)
    .filter((g) => RUNNERS.includes(g.role))
    .map((g) => ({ nodes: g.nodes, services: g.services }));
}

/** Live (unexpired) policies only. */
export function livePolicies(policies: SessionPolicyView[], nowMs = Date.now()): SessionPolicyView[] {
  return policies.filter((p) => p.expiresAtMicros === null || p.expiresAtMicros > nowMs * 1000);
}

/** Terminal policies that name this username. */
export function terminalPoliciesFor(
  policies: SessionPolicyView[],
  username: string,
): SessionPolicyView[] {
  return livePolicies(policies).filter((p) => globMatch(p.users, username));
}

/**
 * The policy this screen writes when somebody flips "can open a terminal"
 * on one person.
 *
 * Terminal access is a glob-matched policy underneath, which is the right
 * primitive for `ops-*` and the wrong shape for "let Mia in". So the
 * per-person switch owns exactly one policy per person, named for them, and
 * leaves every hand-written glob policy alone — including in the list of
 * what covers somebody, so nobody flips a switch off and wonders why they
 * still have a prompt.
 */
export function personalPolicyId(username: string): string {
  return `terminal-${username.replace(/[^a-zA-Z0-9-]/g, "-")}`;
}

/** The whole capability list for a real, existing person. */
export function capabilitiesOf(
  u: UserView,
  grants: GrantView[],
  policies: SessionPolicyView[],
): Capability[] {
  return capabilitiesFor({
    roles: rolesOf(grants, u.userId, u.isFounder),
    scopes: runnerScopes(grants, u.userId),
    isFounder: u.isFounder,
    terminalPolicies: terminalPoliciesFor(policies, u.username),
  });
}

/** The one-line summary under a name in the roster. */
export function summaryOf(caps: Capability[]): string {
  const yes = caps.filter((c) => c.yes).map((c) => c.what.toLowerCase());
  if (yes.length === 0) return "nothing yet — they are a known key with no role";
  return yes.join(" · ");
}
