import { beforeEach, describe, expect, it, vi } from "vitest";

// The sink, faked at the bridge. What is asserted here is the contract
// `filesave.rs` relies on: pieces no larger than it accepts, split where
// splitting is safe, and nothing left behind when it goes wrong.
vi.mock("./api", () => ({
  api: {
    fileSaveBegin: vi.fn(),
    fileSaveWrite: vi.fn(),
    fileSaveFinish: vi.fn(),
    fileSaveAbort: vi.fn(),
  },
}));

import { api } from "./api";
import { saveStreamed } from "./save";

const sink = vi.mocked(api);

/** Must match `CHUNK` in save.ts. */
const CHUNK = 1 << 20;

let written: string[] = [];
/** Throw on the nth write (0-based); -1 never throws. */
let failAt = -1;

beforeEach(() => {
  written = [];
  failAt = -1;
  vi.clearAllMocks();
  sink.fileSaveBegin.mockResolvedValue(1);
  sink.fileSaveWrite.mockImplementation(async (_id: number, chunk: string) => {
    if (written.length === failAt) throw new Error("disk full");
    written.push(chunk);
    return written.reduce((n, w) => n + w.length, 0);
  });
  sink.fileSaveFinish.mockResolvedValue("/home/you/answers.json");
  sink.fileSaveAbort.mockResolvedValue(undefined);
});

const run = (chunks: () => Iterable<string>, extra: Record<string, unknown> = {}) =>
  saveStreamed({ title: "t", suggested: "answers.json", chunks, ...extra });

describe("saveStreamed", () => {
  it("splits one oversized piece into pieces the sink accepts", async () => {
    // A 40 MB stdout arrives as one yield. Handing it over whole is the
    // thing the streamed sink exists to prevent — and the thing Rust
    // refuses, so this would be a save that failed rather than one that
    // was merely slow.
    const huge = "x".repeat(5 * CHUNK + 1234);
    expect(await run(() => [huge])).toBe("/home/you/answers.json");
    expect(written.length).toBeGreaterThan(5);
    for (const piece of written) expect(piece.length).toBeLessThanOrEqual(CHUNK);
    expect(written.join("")).toBe(huge);
    expect(sink.fileSaveFinish).toHaveBeenCalledTimes(1);
  });

  it("batches many small pieces rather than sending one message each", async () => {
    // The document arrives a line at a time. Twenty thousand lines must
    // not be twenty thousand round-trips.
    const lines = Array.from({ length: 20_000 }, (_, i) => `line ${i}\n`);
    await run(() => lines);
    expect(written.length).toBeLessThan(10);
    expect(written.join("")).toBe(lines.join(""));
  });

  it("never cuts a surrogate pair in half", async () => {
    const text = `${"a".repeat(CHUNK - 1)}😀${"b".repeat(40)}`;
    await run(() => [text]);
    expect(written[0]).toHaveLength(CHUNK - 1);
    for (const piece of written) {
      expect(/[\uD800-\uDBFF]$/.test(piece)).toBe(false);
      expect(/^[\uDC00-\uDFFF]/.test(piece)).toBe(false);
    }
    expect(written.join("")).toBe(text);
  });

  it("is a cancel, not an error, when nobody picks a file", async () => {
    sink.fileSaveBegin.mockResolvedValue(null);
    expect(await run(() => ["anything"])).toBeNull();
    expect(written).toHaveLength(0);
    expect(sink.fileSaveAbort).not.toHaveBeenCalled();
  });

  it("abandons the file when cancelled part-way", async () => {
    let seen = 0;
    const path = await run(
      () =>
        (function* () {
          for (let i = 0; i < 40; i += 1) {
            seen = i;
            yield "y".repeat(CHUNK);
          }
        })(),
      { cancelled: () => seen >= 3 },
    );
    expect(path).toBeNull();
    expect(sink.fileSaveAbort).toHaveBeenCalledTimes(1);
    expect(sink.fileSaveFinish).not.toHaveBeenCalled();
  });

  it("abandons the file when a write fails, and says so", async () => {
    failAt = 2;
    await expect(run(() => ["z".repeat(6 * CHUNK)])).rejects.toThrow("disk full");
    expect(sink.fileSaveAbort).toHaveBeenCalledTimes(1);
    expect(sink.fileSaveFinish).not.toHaveBeenCalled();
  });

  it("reports progress as the bytes land", async () => {
    const seen: number[] = [];
    await run(() => ["q".repeat(3 * CHUNK)], { onProgress: (n: number) => seen.push(n) });
    expect(seen).toEqual([...seen].sort((a, b) => a - b));
    expect(seen.at(-1)).toBe(3 * CHUNK);
  });
});
