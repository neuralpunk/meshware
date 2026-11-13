import { describe, expect, it } from "vitest";
import { LineRing, OUTPUT_MAX_LINES } from "./lines";

// DIR-9: Launch and Activity keep streamed output in one bounded ring.
describe("LineRing", () => {
  it("joins a line split across chunks, and shows it while it is unfinished", () => {
    const r = new LineRing();
    r.push("hel");
    expect(r.text).toBe("hel");
    r.push("lo\nwor");
    expect(r.text).toBe("hello\nwor");
    r.push("ld\n");
    expect(r.text).toBe("hello\nworld");
  });

  it("keeps the last lines, and counts the ones it dropped", () => {
    const r = new LineRing();
    for (let i = 0; i < OUTPUT_MAX_LINES + 500; i++) r.push(`line ${i}\n`);
    const lines = r.text.split("\n");
    expect(lines).toHaveLength(OUTPUT_MAX_LINES);
    expect(lines[0]).toBe("line 500");
    expect(r.dropped).toBe(500);
  });

  it("takes one enormous chunk without keeping it", () => {
    const r = new LineRing(10);
    r.push("x\n".repeat(200_000));
    expect(r.text.split("\n")).toHaveLength(10);
    expect(r.dropped).toBe(199_990);
  });

  it("bounds a line that never ends", () => {
    const r = new LineRing(100, 1_000);
    for (let i = 0; i < 50; i++) r.push("y".repeat(700));
    expect(r.text.length).toBeLessThan(40_000);
    expect(r.text).toContain("more characters");
  });

  it("bounds the characters of many long lines", () => {
    const r = new LineRing(2_000, 16_384, 50_000);
    for (let i = 0; i < 100; i++) r.push(`${"z".repeat(9_999)}\n`);
    expect(r.text.length).toBeLessThanOrEqual(50_000);
    expect(r.dropped).toBeGreaterThan(90);
  });

  it("tails, and resets", () => {
    const r = new LineRing();
    r.push("a\nb\nc\nd");
    expect(r.tail(2)).toBe("c\nd");
    r.reset();
    expect(r.text).toBe("");
    expect(r.dropped).toBe(0);
  });
});
