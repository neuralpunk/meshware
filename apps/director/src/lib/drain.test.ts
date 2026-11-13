import { describe, expect, it } from "vitest";
import { DRAIN_KEEP_SYSTEM, DRAINING, drainOf, drainWords, isDrainTag } from "./drain";

// The cases `mesh_proto::drain_of` is held to, so the panel and the
// reconciler read one set of tags one way.
describe("drainOf", () => {
  it("is nothing for a node that is not draining", () => {
    expect(drainOf(["edge", "dc=east"])).toBeNull();
  });

  it("reads a deadline, a keep-system and an admin's drain", () => {
    expect(drainOf(["drain.by=1700000000", DRAIN_KEEP_SYSTEM])).toEqual({
      deadlineSecs: 1_700_000_000,
      keepSystem: true,
      leaving: false,
    });
    expect(drainOf(["drain.by=never"])?.deadlineSecs).toBeNull();
  });

  it("reads a pre-Phase-C shutdown as a deadline already passed", () => {
    expect(drainOf([DRAINING])).toEqual({ deadlineSecs: 0, keepSystem: false, leaving: true });
  });

  it("does not wait for ever on a value it cannot read", () => {
    expect(drainOf(["drain.by=soon"])?.deadlineSecs).toBe(0);
  });

  it("knows its own tags", () => {
    expect(isDrainTag("drain.by=5")).toBe(true);
    expect(isDrainTag(DRAIN_KEEP_SYSTEM)).toBe(true);
    expect(isDrainTag(DRAINING)).toBe(false);
  });
});

describe("drainWords", () => {
  const now = 1_700_000_000;
  it("says when time is up and when there is no deadline", () => {
    expect(drainWords({ deadlineSecs: null, keepSystem: false, leaving: false }, now)).toContain("no deadline");
    expect(drainWords({ deadlineSecs: now - 1, keepSystem: false, leaving: false }, now)).toContain("passed");
    expect(drainWords({ deadlineSecs: now + 600, keepSystem: false, leaving: false }, now)).toContain("is stopped");
    expect(drainWords({ deadlineSecs: 0, keepSystem: false, leaving: true }, now)).toContain("everything now");
  });
});
