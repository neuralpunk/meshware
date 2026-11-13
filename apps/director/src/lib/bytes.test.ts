import { describe, expect, it } from "vitest";
import { fmtBytes, fmtSizeMb } from "./bytes";

describe("fmtSizeMb", () => {
  it("matches mesh-proto's size_mb, so the spec line and the cards agree", () => {
    // The cases `sizes_read_as_prose` pins on the Rust side.
    expect(fmtSizeMb(512)).toBe("0.5 GB");
    expect(fmtSizeMb(8192)).toBe("8.0 GB");
    expect(fmtSizeMb(319_488)).toBe("312 GB");
    expect(fmtSizeMb(4_299_161)).toBe("4.1 TB");
    // The one the review caught: the header's 8.7 TB, which Capacity
    // printed as 8886.72 GB.
    expect(fmtSizeMb(8886.72 * 1024)).toBe("8.7 TB");
  });
});

describe("fmtBytes", () => {
  it("scales to the largest whole unit, one decimal below ten", () => {
    expect(fmtBytes(0)).toBe("0 B");
    expect(fmtBytes(1023)).toBe("1023 B");
    expect(fmtBytes(1536)).toBe("1.5 KB");
    expect(fmtBytes(48 * 1024 * 1024)).toBe("48 MB");
    expect(fmtBytes(8886.72 * 1024 ** 3)).toBe("8.7 TB");
    expect(fmtBytes(Number.NaN)).toBe("—");
  });
});
