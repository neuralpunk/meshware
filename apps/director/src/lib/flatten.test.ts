import { describe, expect, it } from "vitest";
import { flattenRecord } from "./flatten";

describe("flattenRecord", () => {
  it("turns a nested record into dotted rows", () => {
    expect(
      flattenRecord({
        node: { name: "forge", tags: ["gpu", "dc=home"], capacity: { cores: 32 }, stale: null, addrs: [] },
        saturation: { devices: [{ index: 0 }, { index: 1 }], note: "" },
      }),
    ).toEqual([
      { key: "node.name", value: "forge" },
      { key: "node.tags", value: "gpu, dc=home" },
      { key: "node.capacity.cores", value: "32" },
      { key: "node.stale", value: "—" },
      { key: "node.addrs", value: "none" },
      { key: "saturation.devices[0].index", value: "0" },
      { key: "saturation.devices[1].index", value: "1" },
      { key: "saturation.note", value: "(empty)" },
    ]);
  });

  it("says a missing reading rather than dropping it", () => {
    expect(flattenRecord({ saturation: null })).toEqual([{ key: "saturation", value: "—" }]);
    expect(flattenRecord({ a: true, b: {} })).toEqual([
      { key: "a", value: "true" },
      { key: "b", value: "none" },
    ]);
  });
});
