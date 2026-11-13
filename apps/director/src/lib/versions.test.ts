import { describe, expect, it } from "vitest";
import { fleetOf, newerOf, outdatedCount, targetOf } from "./versions";
import type { NodeView } from "./bindings/NodeView";

function node(name: string, version: string, over: Partial<NodeView> = {}): NodeView {
  return {
    nodeId: `mw1${name}xxxxxxxxxxxx`,
    shortId: `mw1${name}`.slice(0, 15),
    name,
    addrs: ["192.168.20.10:7777"],
    tags: [],
    liveness: "alive",
    isSelf: false,
    revoked: false,
    left: false,
    joinedAtMicros: 0,
    executing: true,
    capacity: null,
    stale: null,
    acceptsTerminals: false,
    noShell: false,
    sync: null,
    version,
    contributes: true,
    quarantined: null,
    contributeReason: "",
    assignedTags: [],
    ...over,
  };
}

describe("which nodes are behind", () => {
  // The mesh this was found on: Director carries 0.9.30, every node —
  // this machine's included — still runs 0.9.27.
  const others = [node("apus", "0.9.27"), node("ivy", "0.9.27"), node("merope", "0.9.27")];
  const nim = (version: string) => node("nim", version, { isSelf: true });

  it("counts the fleet against what Director carries, even when every node agrees", () => {
    const nodes = [nim("0.9.27"), ...others];
    // Measured against the newest version *running* alone, nothing was
    // behind and the badge disappeared.
    expect(targetOf(nodes, "0.9.30")).toBe("0.9.30");
    expect(outdatedCount(nodes, "0.9.30")).toBe(3);
    expect(outdatedCount(nodes, null)).toBe(0);
  });

  it("never counts or offers this machine: Director keeps it current itself", () => {
    const nodes = [nim("0.9.27"), ...others];
    expect(fleetOf(nodes).map((n) => n.name)).toEqual(["apus", "ivy", "merope"]);
    // …and once it has, the other three are exactly what is left to do.
    expect(outdatedCount([nim("0.9.30"), ...others], "0.9.30")).toBe(3);
    expect(outdatedCount([nim("0.9.30"), ...others.map((n) => ({ ...n, version: "0.9.30" }))], "0.9.30")).toBe(0);
  });

  it("follows the newest running build when Director carries an older one", () => {
    const nodes = [nim("0.9.30"), node("apus", "0.9.31"), node("ivy", "0.9.30")];
    expect(targetOf(nodes, "0.9.30")).toBe("0.9.31");
    expect(outdatedCount(nodes, "0.9.30")).toBe(1);
  });

  it("picks the newer of two versions, either possibly unknown", () => {
    expect(newerOf("0.9.27", "0.9.30")).toBe("0.9.30");
    expect(newerOf("0.9.30-dev.1", "0.9.30")).toBe("0.9.30");
    expect(newerOf(null, "unknown")).toBeNull();
    expect(newerOf(undefined, "0.9.2")).toBe("0.9.2");
  });
});
