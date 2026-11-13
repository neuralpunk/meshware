import { describe, expect, it } from "vitest";
import { attentionItems } from "./attention";
import type { NodeView } from "./bindings/NodeView";
import type { DeviceView } from "./bindings/DeviceView";
import type { DeploymentView } from "./bindings/DeploymentView";

function node(name: string, over: Partial<NodeView> = {}): NodeView {
  return {
    nodeId: `id-${name}`,
    shortId: name,
    name,
    addrs: [],
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
    version: null,
    contributes: true,
    quarantined: null,
    contributeReason: "",
    assignedTags: [],
    ...over,
  };
}

function device(nodeName: string, over: Partial<DeviceView> = {}): DeviceView {
  return {
    nodeId: `id-${nodeName}`,
    nodeName,
    index: 0,
    kind: "gpu",
    vendor: "NVIDIA",
    model: "A100",
    uuid: "",
    memMb: 0,
    numaNode: null,
    health: "ok",
    healthReason: null,
    heldBy: null,
    utilPct: null,
    memFreeMb: null,
    tempC: null,
    throttled: false,
    ...over,
  };
}

function deployment(name: string, status: string): DeploymentView {
  return {
    name,
    service: name,
    count: "3",
    targets: "*",
    generation: 1,
    status,
    desired: 3,
    running: 3,
    healthy: 1,
    createdBy: "root",
    createdAtMicros: 0,
    source: null,
  };
}

const sync = (behind: number, ageSecs: number) => ({ behind, total: 100, authors: 1, ageMicros: ageSecs * 1e6, pct: 90 });

describe("attentionItems", () => {
  it("is empty for a well mesh", () => {
    expect(attentionItems({ nodes: [node("a"), node("b", { sync: sync(0, 90) })] })).toEqual([]);
  });

  it("names machines that are not replying, and ignores the ones that left", () => {
    const items = attentionItems({
      nodes: [
        node("edge-7", { liveness: "suspect" }),
        node("old", { liveness: "dead" }),
        node("gone", { liveness: "dead", left: true }),
        node("removed", { liveness: "dead", revoked: true }),
      ],
    });
    expect(items.map((i) => `${i.subject} ${i.problem}`)).toEqual([
      "edge-7 isn't replying",
      "old has stopped answering",
    ]);
    expect(items[1].tone).toBe("error");
  });

  it("says quarantine once, with the reason, even for a machine that also went quiet", () => {
    const items = attentionItems({
      nodes: [node("anvil", { quarantined: "possible key theft", liveness: "suspect" })],
    });
    expect(items).toHaveLength(1);
    expect(items[0].problem).toBe("is quarantined: possible key theft");
  });

  it("counts a lagging peer only once it has stopped asking", () => {
    const items = attentionItems({
      nodes: [node("busy", { sync: sync(40, 3) }), node("stuck", { sync: sync(1, 45) })],
    });
    expect(items.map((i) => i.subject)).toEqual(["stuck"]);
    expect(items[0].problem).toBe("is 1 change behind and has stopped catching up");
  });

  it("lists unhealthy and throttled accelerators on member machines", () => {
    const items = attentionItems({
      nodes: [node("forge"), node("gone", { left: true })],
      devices: [
        device("forge", { index: 0, throttled: true }),
        device("forge", { index: 1 }),
        device("forge", { index: 3, health: "degraded", healthReason: "1 corrected ECC error" }),
        device("gone", { health: "unavailable" }),
      ],
    });
    expect(items.map((i) => `${i.subject} ${i.problem}`)).toEqual([
      "forge GPU 0 is throttling",
      "forge GPU 3 is degraded: 1 corrected ECC error",
    ]);
  });

  it("tells an AMD GPU 0 from an NVIDIA GPU 0 on the same machine", () => {
    // Both throttled is the shape the daemon produces today: live readings
    // are matched to devices by the bare ordinal.
    const items = attentionItems({
      nodes: [node("nim")],
      devices: [
        device("nim", { vendor: "nvidia", throttled: true }),
        device("nim", { vendor: "amd", model: "Radeon 680M", throttled: true }),
        device("nim", { vendor: "nvidia", index: 1, throttled: true }),
      ],
    });
    expect(new Set(items.map((i) => i.key)).size).toBe(3);
    expect(items.map((i) => i.subject)).toEqual(["nim NVIDIA GPU 0", "nim AMD GPU 0", "nim GPU 1"]);
  });

  it("never repeats a key, even when a peer lists one device twice", () => {
    const twice = device("forge", { health: "degraded" });
    const items = attentionItems({ nodes: [node("forge")], devices: [twice, { ...twice }] });
    expect(items).toHaveLength(1);
  });

  it("lists failing deployments only", () => {
    const items = attentionItems({
      nodes: [],
      deployments: [deployment("api", "degraded"), deployment("web", "progressing"), deployment("db", "active")],
    });
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ kind: "deployment", target: "api", problem: "is failing: 1 of 3 copies healthy" });
  });
});
