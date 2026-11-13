import { describe, expect, it } from "vitest";
import { diagnosticsText } from "./diagnostics";
import type { AboutView } from "./bindings/AboutView";
import type { StatusView } from "./bindings/StatusView";
import type { NodeView } from "./bindings/NodeView";

const about: AboutView = {
  version: "0.7.2",
  bundledMeshnode: "0.7.2",
  commit: "22e3cc5",
  os: "linux",
  arch: "x86_64",
  licenseId: "MIT OR Apache-2.0",
  licenseName: "Open source — MIT or Apache 2.0, at your option",
  licenseText: "…",
  docsUrl: "https://example.test/docs",
  issuesUrl: "https://example.test/issues",
  sourceUrl: "https://example.test/source",
};

const status = {
  version: "0.7.2",
  proto: "1.34",
  nodeShort: "mw1sdj5gterlfav",
  nodeName: "secret-lab-box",
  meshId: "mesh-qlljxxdslh66jm3n6vk7ukouva",
  meshName: "hyacinth",
  listenAddr: "10.0.0.7:7777",
  uptimeSecs: 90,
  draining: false,
} as StatusView;

const node = (name: string, liveness: string, over: Partial<NodeView> = {}) =>
  ({ name, liveness, revoked: false, left: false, version: "0.7.2", addrs: ["10.0.0.9:7777"], ...over }) as NodeView;

describe("diagnosticsText", () => {
  const text = diagnosticsText({
    about,
    status,
    nodes: [node("forge", "alive"), node("edge-7", "suspect", { version: "0.6.0" }), node("gone", "dead", { left: true })],
    role: "founder",
    now: new Date("2026-09-17T10:00:00Z"),
  });

  it("says which build, platform and licence", () => {
    expect(text).toContain("Director   0.7.2 (22e3cc5)");
    expect(text).toContain("linux/x86_64");
    expect(text).toContain("MIT OR Apache-2.0");
    expect(text).toContain("0.7.2 · protocol 1.34");
  });

  it("describes the mesh by shape, not by names or addresses", () => {
    expect(text).toContain("2 (1 alive, 1 suspect, 0 dead)");
    expect(text).toContain("0.6.0, 0.7.2");
    for (const leak of ["forge", "edge-7", "secret-lab-box", "hyacinth", "10.0.0", "qlljxxdslh66jm3n6vk7ukouva"]) {
      expect(text).not.toContain(leak);
    }
  });

  it("still says something useful with no engine", () => {
    const t = diagnosticsText({ about: null, status: null, nodes: [], role: "guest" });
    expect(t).toContain("Engine     not running");
  });
});
