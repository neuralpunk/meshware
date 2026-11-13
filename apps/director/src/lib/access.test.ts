// The three properties this screen must not get wrong.
//
// `access.ts` exists to *explain* authorization, never to decide it — the
// decision is always made on the node being asked, by
// `crates/mesh-core/src/rbac.rs`. That makes a drift here silent: the mesh
// keeps doing the right thing while the screen tells somebody the wrong
// thing about what they can do, and nothing fails until a person acts on the
// sentence they read.
//
// So these pin the two places a drift would actually come from: the glob
// implementation that was copied across the language boundary, and the
// role arithmetic the roster and the grant editor share.
import { describe, expect, it } from "vitest";
import { activeGrantsFor, capabilitiesFor, globMatch, rolesOf } from "./access";
import type { GrantView } from "./api";

describe("globMatch mirrors rbac::glob_match", () => {
  // Byte-for-byte the assertions in `mesh-core/src/rbac.rs::glob_semantics`,
  // which is the point: this file and that one are two implementations of
  // one rule, and the only thing that keeps them together is that both
  // suites carry the same vectors. Add a case there, add it here.
  it("agrees with the engine on every vector the engine pins", () => {
    expect(globMatch("*", "")).toBe(true);
    expect(globMatch("*", "anything")).toBe(true);
    expect(globMatch("deploy-*", "deploy-web")).toBe(true);
    expect(globMatch("deploy-*", "restart-web")).toBe(false);
    expect(globMatch("deploy-*", "")).toBe(false);
    expect(globMatch("a*b*c", "a-x-b-y-c")).toBe(true);
    expect(globMatch("a*b*c", "acb")).toBe(false);
    expect(globMatch("exact", "exact")).toBe(true);
    expect(globMatch("exact", "exactly")).toBe(false);
  });
});

const grant = (over: Partial<GrantView> = {}): GrantView =>
  ({
    userId: "mia",
    role: "operator",
    nodes: "any",
    services: "*",
    revoked: false,
    expired: false,
    expiresAtMicros: null,
    ...over,
  }) as GrantView;

describe("roles in force", () => {
  // A revoked or expired grant that still conferred a role on screen would
  // show somebody as an operator after their access was taken away — the
  // one failure mode of a roster that matters.
  it("ignores revoked and expired grants, and covers the founder", () => {
    const grants = [
      grant({ role: "operator" }),
      grant({ role: "developer", revoked: true }),
      grant({ role: "auditor", expired: true }),
      grant({ userId: "sam", role: "admin" }),
    ];

    expect(activeGrantsFor(grants, "mia")).toHaveLength(1);
    expect(rolesOf(grants, "mia", false)).toEqual(["operator"]);

    // The founder is an admin from genesis, with no grant to point at.
    expect(rolesOf([], "mia", true)).toEqual(["admin"]);
  });
});

describe("capabilitiesFor", () => {
  // A role narrowed to a service glob can start *those* scripts and nothing
  // else. A terminal belongs to no script, so it needs the unrestricted
  // scope — and telling a scoped developer they can open one is the
  // sentence that sends somebody to a button that will refuse them.
  it("refuses a terminal to a scope that is not wide open", () => {
    const scoped = capabilitiesFor({
      roles: ["developer"],
      scopes: [{ nodes: "any", services: "deploy-*" }],
      isFounder: false,
      terminalPolicies: [],
    });

    const can = (what: string) => scoped.find((c) => c.what === what)?.yes;
    expect(can("Run work")).toBe(true);
    expect(can("Open a terminal")).toBe(false);
    expect(can("Decide who else gets in")).toBe(false);

    // Widening the scope alone is still not enough — somebody has to switch
    // terminals on for them.
    const wide = capabilitiesFor({
      roles: ["developer"],
      scopes: [{ nodes: "any", services: "*" }],
      isFounder: false,
      terminalPolicies: [],
    });
    expect(wide.find((c) => c.what === "Open a terminal")?.yes).toBe(false);
  });
});
