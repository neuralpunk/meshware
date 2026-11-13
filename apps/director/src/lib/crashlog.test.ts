import { beforeEach, describe, expect, it, vi } from "vitest";

// DIR-11: a frontend error ends up in Director's log, once, without the
// logger becoming a source of errors itself; and a start failure gets
// plain words where there are some to give.
vi.mock("./api", () => ({
  errText: (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : String(e)),
  api: {
    frontendLog: vi.fn(),
    about: vi.fn(),
    directorLogDir: vi.fn(),
  },
}));

import { api } from "./api";
import { crashDetails, describeError, installCrashLog, reportCrash, startHint } from "./crashlog";

const e = vi.mocked(api);

beforeEach(() => {
  vi.clearAllMocks();
  e.frontendLog.mockResolvedValue(undefined as never);
});

describe("start hints", () => {
  it("says what the common failures mean", () => {
    expect(startHint("Database already open. Cannot acquire lock.")).toMatch(/Another copy of Meshware/);
    expect(startHint("bind 127.0.0.1:8600: Address already in use (os error 98)")).toMatch(/network port/);
    expect(startHint("write op log: No space left on device (os error 28)")).toMatch(/disk is full/);
    expect(startHint("open identity: Permission denied (os error 13)")).toMatch(/its own files/);
  });

  it("says nothing it cannot back up", () => {
    expect(startHint("genesis: unsupported protocol 2.0")).toBeNull();
  });
});

describe("the crash log", () => {
  it("writes an error's message and stack", () => {
    reportCrash("error", new TypeError("x is undefined"));
    expect(e.frontendLog).toHaveBeenCalledWith("error", "x is undefined", expect.stringContaining("TypeError"));
  });

  it("writes a thrown string, and a thrown nothing, as something readable", () => {
    expect(describeError("engine refused")).toEqual({ message: "engine refused", stack: null });
    expect(describeError(undefined).message).toBeTruthy();
  });

  it("writes the same error from a render loop once", () => {
    for (let i = 0; i < 50; i++) reportCrash("view:mesh", new Error("loop"));
    expect(e.frontendLog).toHaveBeenCalledTimes(1);
    reportCrash("view:mesh", new Error("something else"));
    expect(e.frontendLog).toHaveBeenCalledTimes(2);
  });

  it("never throws, and never rejects, when the bridge fails", async () => {
    e.frontendLog.mockRejectedValue(new Error("no bridge"));
    expect(() => reportCrash("error", new Error("a"))).not.toThrow();
    e.frontendLog.mockImplementation(() => {
      throw new Error("no invoke");
    });
    expect(() => reportCrash("error", new Error("b"))).not.toThrow();
    await Promise.resolve();
  });

  it("forwards uncaught errors and unhandled rejections", () => {
    const target = new EventTarget();
    installCrashLog(target as unknown as Window);
    const err = Object.assign(new Event("error"), { error: new Error("uncaught"), message: "uncaught" });
    target.dispatchEvent(err);
    const rej = Object.assign(new Event("unhandledrejection"), { reason: "rejected" });
    target.dispatchEvent(rej);
    expect(e.frontendLog).toHaveBeenCalledWith("error", "uncaught", expect.anything());
    expect(e.frontendLog).toHaveBeenCalledWith("unhandledrejection", "rejected", null);
  });

  it("copies the error with the build and the log folder", async () => {
    e.about.mockResolvedValue({ version: "0.9.17", commit: "abc1234", os: "linux", arch: "x86_64" } as never);
    e.directorLogDir.mockResolvedValue("/home/you/.meshware/director/logs");
    const text = await crashDetails("Director could not start", "Database already open");
    expect(text).toContain("Director could not start\nDatabase already open");
    expect(text).toContain("Director 0.9.17 (abc1234) · linux/x86_64");
    expect(text).toContain("Log folder: /home/you/.meshware/director/logs");
  });
});
