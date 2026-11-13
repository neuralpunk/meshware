// `fmtRoughDuration` is the one piece of logic behind the key-protection
// panel, and it is a mirror: `identity_protect::humanish` in the CLI has to
// say the same thing about the same unlock, or the two front ends disagree
// about how long you have left.
import { describe, expect, it } from "vitest";
import { fmtDuration, fmtRoughDuration } from "./api";

describe("fmtRoughDuration", () => {
  // The vectors that matter are the ones a *counting-down* value produces.
  // An exact division essentially never happens after the first second.
  it("names a unit for values that are not exact divisions", () => {
    expect(fmtRoughDuration(43_187)).toBe("12h");
    expect(fmtRoughDuration(600)).toBe("10m");
    expect(fmtRoughDuration(91)).toBe("2m");
    expect(fmtRoughDuration(90)).toBe("90s");
    expect(fmtRoughDuration(45)).toBe("45s");
  });

  // The band edges, because they are where the two implementations would
  // drift first. The minutes band runs to 90m, not to an hour — so 3601s
  // is "61m", which reads oddly until you know that and is what the CLI's
  // `humanish` says too.
  it("switches bands where the CLI switches them", () => {
    expect(fmtRoughDuration(90)).toBe("90s");
    expect(fmtRoughDuration(91)).toBe("2m");
    expect(fmtRoughDuration(3_601)).toBe("61m");
    expect(fmtRoughDuration(5_400)).toBe("90m");
    expect(fmtRoughDuration(5_401)).toBe("2h");
  });

  // An expired or absent unlock is not "0s" — there is nothing to report.
  it("renders an em dash rather than a zero", () => {
    expect(fmtRoughDuration(0)).toBe("—");
    expect(fmtRoughDuration(-5)).toBe("—");
    expect(fmtRoughDuration(Number.NaN)).toBe("—");
  });

  // The reason this exists beside `fmtDuration` rather than replacing it:
  // the older one is right for a *configured* value and wrong for a
  // counting one, and mixing them up is the mistake to prevent.
  it("differs from fmtDuration exactly where it should", () => {
    expect(fmtDuration(43_200)).toBe("12h");
    expect(fmtRoughDuration(43_200)).toBe("12h");
    expect(fmtDuration(43_187)).toBe("43187s");
    expect(fmtRoughDuration(43_187)).toBe("12h");
  });
});
