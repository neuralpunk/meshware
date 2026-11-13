// The five roles, in words a person can act on.
//
// A plain module rather than exports from a component, because three
// different surfaces need the same sentences — the reference table, the
// picker, and the per-person summary — and a role whose description depends
// on which screen you are looking at is worse than no description.
//
// These are prose *about* `crates/mesh-core/src/rbac.rs`. The permission
// sets themselves live in `RoleMatrix.svelte`, next to the table that
// renders them, so a reviewer can diff the two by eye.

import type { Hue } from "./hues";

export const ROLES = ["admin", "operator", "developer", "auditor", "viewer"] as const;

export type RoleName = (typeof ROLES)[number];

/**
 * One hue per role, everywhere a role is named — the chip on a person, the
 * picker, the grants table, the rail's account row. Admin is plum (the one
 * that can change who else gets in), operator indigo, developer sky,
 * auditor amber (watches, touches nothing), viewer the brand's own teal.
 * Same word, same colour, on every screen: that is what lets a person read
 * "who can do what" off a list without reading the list.
 */
const ROLE_HUE: Record<RoleName, Hue> = {
  admin: "plum",
  operator: "indigo",
  developer: "sky",
  auditor: "amber",
  viewer: "teal",
};

/** The hue for a role name that may not be one of the five (a founder, an unregistered key). */
export function roleHue(name: string): Hue {
  if (name === "founder") return "plum";
  return (ROLE_HUE as Record<string, Hue>)[name] ?? "teal";
}

export const ROLE_SUMMARY: Record<string, { one: string; long: string }> = {
  admin: {
    one: "Runs the mesh",
    long: "Everything, including the things that decide what everyone else can do: users, grants, invites, secrets, terminal policies, and revoking a node's key forever.",
  },
  operator: {
    one: "Keeps things running",
    long: "Runs and cancels work, drains nodes, manages schedules. Cannot change who has access, and cannot read the audit log.",
  },
  developer: {
    one: "Ships the things that run",
    long: "Everything an operator does with work, plus the content it runs: bundles, services, contracts and Git sources. Can see that a secret exists, never its value.",
  },
  auditor: {
    one: "Watches, touches nothing",
    long: "Reads everything, including the audit log and the names of secrets — and runs nothing at all. The role for somebody who must be able to answer questions without being able to change answers.",
  },
  viewer: {
    one: "Reads the mesh",
    long: "Nodes, jobs and their history. No secrets, no audit log, and nothing that starts work.",
  },
};
