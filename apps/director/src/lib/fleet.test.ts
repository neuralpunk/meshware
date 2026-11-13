import { describe, expect, it } from "vitest";
import { chipMatches, nextSort, searchMatches, selectorFor, sortFleet, takesWork } from "./fleet";
import type { NodeView } from "./bindings/NodeView";
import type { TopView } from "./bindings/TopView";

function node(name: string, over: Partial<NodeView> = {}): NodeView {
  return {
    nodeId: `mw1${name}xxxxxxxxxxxx`,
    shortId: `mw1${name}`.slice(0, 15),
    name,
    addrs: ["192.168.1.10:7777"],
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
    version: "0.7.2",
    contributes: true,
    quarantined: null,
    contributeReason: "",
    assignedTags: [],
    ...over,
  };
}

const top = (slots: number, running: number, queued: number, busy: number | null) =>
  ({ slots, running, queued, busy, noExecute: false }) as TopView;

describe("fleet chips", () => {
  const quiet = node("edge", { liveness: "suspect" });
  const jailed = node("anvil", { quarantined: "under investigation" });
  const draining = node("old", { tags: ["draining"] });
  const observer = node("laptop", { executing: false });
  const fine = node("forge");

  it("sort machines into the chips they belong to", () => {
    expect(chipMatches("quiet", quiet)).toBe(true);
    expect(chipMatches("quiet", fine)).toBe(false);
    expect(chipMatches("quarantined", jailed)).toBe(true);
    expect(chipMatches("attention", quiet)).toBe(true);
    expect(chipMatches("attention", jailed)).toBe(true);
    expect(chipMatches("attention", fine)).toBe(false);
    for (const n of [jailed, draining, observer]) expect(chipMatches("held", n)).toBe(true);
    expect(chipMatches("held", fine)).toBe(false);
    expect(chipMatches("all", fine)).toBe(true);
  });

  it("a machine held back by its owner does not take work", () => {
    expect(takesWork(node("x", { contributes: false }))).toBe(false);
    expect(takesWork(fine)).toBe(true);
  });
});

describe("fleet search", () => {
  const n = node("forge-02", { tags: ["role=lab", "dc=east"], addrs: ["10.1.2.3:7777"] });

  it("matches a name, a tag, an address, an id and a build, ignoring case", () => {
    for (const q of ["FORGE", "dc=east", "10.1.2", "mw1forge", "0.7.2", "  "]) {
      expect(searchMatches(q, n)).toBe(true);
    }
    expect(searchMatches("anvil", n)).toBe(false);
  });

  it("treats several words as all of them", () => {
    expect(searchMatches("forge east", n)).toBe(true);
    expect(searchMatches("forge west", n)).toBe(false);
  });
});

describe("fleet sort", () => {
  const me = node("zeta", { isSelf: true });
  const a = node("alpha", { joinedAtMicros: 3 });
  const b = node("bravo", { joinedAtMicros: 1 });
  const c = node("charlie", { joinedAtMicros: 2 });
  const tops = new Map<string, TopView>([
    [a.nodeId, top(8, 1, 0, 0.9)],
    [b.nodeId, top(32, 6, 4, 0.2)],
  ]);
  const topOf = (n: NodeView) => tops.get(n.nodeId) ?? null;
  const names = (rows: NodeView[]) => rows.map((r) => r.name);

  it("defaults to this peer first, then by name", () => {
    expect(names(sortFleet([c, a, me, b], { key: "default", desc: false }, topOf))).toEqual([
      "zeta",
      "alpha",
      "bravo",
      "charlie",
    ]);
  });

  it("sorts numbers biggest first and puts machines with no reading last either way", () => {
    expect(names(sortFleet([c, a, b], { key: "slots", desc: true }, topOf))).toEqual(["bravo", "alpha", "charlie"]);
    expect(names(sortFleet([c, a, b], { key: "slots", desc: false }, topOf))).toEqual(["alpha", "bravo", "charlie"]);
    expect(names(sortFleet([c, a, b], { key: "work", desc: true }, topOf))).toEqual(["bravo", "alpha", "charlie"]);
    expect(names(sortFleet([c, a, b], { key: "load", desc: true }, topOf))).toEqual(["alpha", "bravo", "charlie"]);
  });

  it("sorts by join date and by name", () => {
    expect(names(sortFleet([a, b, c], { key: "joined", desc: false }, topOf))).toEqual(["bravo", "charlie", "alpha"]);
    expect(names(sortFleet([b, c, a], { key: "name", desc: true }, topOf))).toEqual(["charlie", "bravo", "alpha"]);
  });

  it("cycles a header through its direction, the reverse, and back to default", () => {
    let s = nextSort({ key: "default", desc: false }, "load");
    expect(s).toEqual({ key: "load", desc: true });
    s = nextSort(s, "load");
    expect(s).toEqual({ key: "load", desc: false });
    s = nextSort(s, "load");
    expect(s).toEqual({ key: "default", desc: false });
    expect(nextSort({ key: "load", desc: true }, "name")).toEqual({ key: "name", desc: false });
  });
});

describe("selectorFor", () => {
  it("names exactly the chosen machines", () => {
    expect(selectorFor(["forge"])).toBe("name=forge");
    expect(selectorFor(["forge", "anvil"])).toBe("name=forge|name=anvil");
  });
});
