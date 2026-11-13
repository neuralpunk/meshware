import { beforeEach, describe, expect, it, vi } from "vitest";

// The engine, faked at the bridge.
//
// UX-2's contract: a refused read is kept as an error under its topic (so
// the screen can say so instead of drawing its empty state), a later read
// that works clears it, and an engine that is not answering at all is a
// pause, not a refusal on every screen.
//
// UX-6's: approvals are only polled where change control exists, and the
// bell and the Access badge count the same things.
//
// DIR-5's: who this person is follows the ops that can change it, and
// nothing about one mesh survives into the next.
vi.mock("./api", () => ({
  errText: (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : String(e)),
  onKeyPaused: () => () => {},
  api: {
    stateVersions: vi.fn(),
    status: vi.fn(),
    nodes: vi.fn(),
    jobs: vi.fn(),
    services: vi.fn(),
    whoami: vi.fn(),
    top: vi.fn(),
    totals: vi.fn(),
    bundles: vi.fn(),
    sources: vi.fn(),
    secrets: vi.fn(),
    secretProviders: vi.fn(),
    users: vi.fn(),
    grants: vi.fn(),
    bootstrapStatus: vi.fn(),
    enrolRequest: vi.fn(),
    lanWatchStop: vi.fn(),
  },
}));

import { api } from "./api";
import { app } from "./state.svelte";

const e = vi.mocked(api);
const DENIED = "permission denied: requires one of: admin, auditor";

beforeEach(() => {
  vi.clearAllMocks();
  app.invalidate();
  app.loadErrors = {};
  app.jobs = [];
  app.secrets = [];
  // No change signal, so every tick asks for every topic.
  e.stateVersions.mockRejectedValue(new Error("old engine"));
  e.status.mockResolvedValue({} as never);
  e.nodes.mockResolvedValue([]);
  e.jobs.mockResolvedValue({ rows: [], counts: {} } as never);
  e.services.mockResolvedValue([]);
  e.whoami.mockResolvedValue({ roles: [] } as never);
  e.top.mockResolvedValue([]);
  e.totals.mockResolvedValue(null as never);
  e.bundles.mockResolvedValue([]);
  e.sources.mockResolvedValue([]);
  e.secrets.mockResolvedValue([]);
  e.secretProviders.mockResolvedValue([]);
  e.users.mockResolvedValue([]);
  e.grants.mockResolvedValue([]);
  e.lanWatchStop.mockResolvedValue(undefined as never);
});

describe("load errors", () => {
  it("keeps a refused core read under its topic, and clears it once it works", async () => {
    e.jobs.mockRejectedValue(DENIED);
    await app.refresh();
    expect(app.loadErrors.jobs).toBe(DENIED);
    expect(app.loadErrors.nodes).toBeUndefined();

    e.jobs.mockResolvedValue({ rows: [{ id: "j1" }], counts: { all: 1 } } as never);
    await app.refresh();
    expect(app.loadErrors.jobs).toBeUndefined();
    expect(app.jobs).toHaveLength(1);
  });

  it("keeps a refused extended read, and a refused provider list counts as the secrets'", async () => {
    e.users.mockRejectedValue(new Error(DENIED));
    await app.refreshExtended();
    expect(app.loadErrors.users).toBe(DENIED);

    // Still owed a fetch after its refusal, with nothing else changing.
    e.users.mockResolvedValue([]);
    await app.refreshExtended();
    expect(app.loadErrors.users).toBeUndefined();
    expect(e.users).toHaveBeenCalledTimes(2);
    expect(e.secrets).toHaveBeenCalledTimes(1);

    app.invalidate();
    e.secretProviders.mockRejectedValue("no providers for you");
    await app.refreshExtended();
    expect(app.loadErrors.users).toBeUndefined();
    expect(app.loadErrors.secrets).toBe("no providers for you");
  });

  it("does not record anything when the engine itself is not answering", async () => {
    e.status.mockRejectedValue("engine restarting");
    e.jobs.mockRejectedValue("engine restarting");
    e.nodes.mockRejectedValue("engine restarting");
    await app.refresh();
    expect(app.loadErrors).toEqual({});

    e.top.mockRejectedValue("engine restarting");
    e.secrets.mockRejectedValue("engine restarting");
    await app.refreshExtended();
    expect(app.loadErrors).toEqual({});
  });

  it("keeps the last good rows when a later read is refused", async () => {
    e.secrets.mockResolvedValue([{ name: "db-pass" }] as never);
    await app.refreshExtended();
    expect(app.secrets).toHaveLength(1);

    app.invalidate();
    e.secrets.mockRejectedValue(DENIED);
    await app.refreshExtended();
    expect(app.loadErrors.secrets).toBe(DENIED);
    expect(app.secrets).toHaveLength(1);
  });

  it("retries every table at once", async () => {
    e.secrets.mockRejectedValue(DENIED);
    await app.refreshExtended();
    expect(app.loadErrors.secrets).toBe(DENIED);

    e.secrets.mockResolvedValue([]);
    await app.retryLoad();
    expect(app.loadErrors.secrets).toBeUndefined();
    expect(e.status).toHaveBeenCalled();
    expect(e.top).toHaveBeenCalled();
  });
});

describe("who you are", () => {
  const founder = {
    userId: "mwu-founder",
    registered: true,
    isFounder: true,
    roles: ["admin"],
    enrolment: "enrolled",
  } as never;
  const stranger = {
    userId: "mwu-mia",
    registered: false,
    isFounder: false,
    roles: [],
    enrolment: "none",
  } as never;
  const queued = { ...(stranger as object), enrolment: "pending", enrolmentCode: "BRAVE-OTTER-4417" } as never;

  beforeEach(() => {
    app.reset();
    vi.restoreAllMocks();
  });

  it("is read again when users, grants or enrolments move, and not otherwise", async () => {
    const topics = { nodes: 1, users: 1, grants: 1, enrolments: 1 };
    e.stateVersions.mockImplementation(async () => ({ topics: { ...topics } }) as never);
    e.whoami.mockResolvedValue(stranger);

    await app.refresh();
    await app.refresh();
    expect(e.whoami).toHaveBeenCalledTimes(1);

    // An admin's approval lands: somebody else's op, applied here.
    for (const [topic, value] of [["enrolments", 2], ["users", 2], ["grants", 2]] as const) {
      e.whoami.mockResolvedValue(topic === "enrolments" ? queued : founder);
      topics[topic] = value;
      await app.refresh();
    }
    expect(e.whoami).toHaveBeenCalledTimes(4);
    expect(app.isAdmin).toBe(true);

    // …and a quiet tick after it asks for nothing.
    await app.refresh();
    expect(e.whoami).toHaveBeenCalledTimes(4);
  });

  it("is read again on the slow timer when no op records the change", async () => {
    e.stateVersions.mockResolvedValue({ topics: { users: 1 } } as never);
    e.whoami.mockResolvedValue(queued);
    const t0 = Date.now();
    const clock = vi.spyOn(Date, "now").mockReturnValue(t0);
    await app.refresh();
    await app.refresh();
    expect(e.whoami).toHaveBeenCalledTimes(1);

    // The request expired: a clock ran out, nobody authored anything.
    e.whoami.mockResolvedValue({ ...(queued as object), enrolment: "expired" } as never);
    clock.mockReturnValue(t0 + 31_000);
    await app.refresh();
    expect(e.whoami).toHaveBeenCalledTimes(2);
    expect(app.whoami?.enrolment).toBe("expired");
  });

  it("moves to the queue as soon as access is requested", async () => {
    e.whoami.mockResolvedValue(stranger);
    await app.refresh();
    e.whoami.mockResolvedValue(queued);
    await app.refreshWhoami();
    expect(app.whoami?.enrolment).toBe("pending");
  });

  it("is forgotten, with every table, when the mesh changes", async () => {
    e.whoami.mockResolvedValue(founder);
    e.jobs.mockResolvedValue({ rows: [{ id: "j1" }], counts: { all: 1 } } as never);
    e.secrets.mockRejectedValue(DENIED);
    await app.refresh();
    await app.refreshExtended();
    app.ready = true;
    app.go("access");
    expect(app.whoami).not.toBeNull();
    expect(app.jobs).toHaveLength(1);
    expect(app.loadErrors.secrets).toBe(DENIED);

    app.reset();
    expect(app.whoami).toBeNull();
    expect(app.isAdmin).toBe(false);
    expect(app.jobs).toHaveLength(0);
    expect(app.loadErrors).toEqual({});
    expect(app.ready).toBe(false);
    expect(app.view).toBe("mesh");
    expect(app.canBack).toBe(false);
  });

  it("drops an answer the old mesh gave after the reset", async () => {
    let answer!: (v: unknown) => void;
    e.whoami.mockReturnValue(new Promise((r) => (answer = r)) as never);
    const stale = app.refresh();
    await Promise.resolve();

    app.reset();
    answer(founder);
    await stale;
    expect(app.whoami).toBeNull();

    // The next mesh's first pass is not held up by the old one's flag.
    e.whoami.mockResolvedValue(stranger);
    await app.refresh();
    expect(app.whoami).toEqual(stranger);
  });

  it("does not carry the demo's founder into a real mesh", async () => {
    // The demo: this window is its founder.
    e.whoami.mockResolvedValue(founder);
    await app.refresh();
    expect(app.isAdmin).toBe(true);

    // Stopped, back to the wizard, and paired into somebody's real mesh.
    e.bootstrapStatus.mockResolvedValue({ hasMesh: false, hostname: "mia-laptop" } as never);
    await app.toWizard();
    expect(app.needsWizard).toBe(true);
    expect(app.whoami).toBeNull();

    e.bootstrapStatus.mockResolvedValue({ hasMesh: true, hostname: "mia-laptop" } as never);
    e.whoami.mockResolvedValue(stranger);
    e.enrolRequest.mockImplementation(async () => {
      e.whoami.mockResolvedValue(queued);
      return "BRAVE-OTTER-4417";
    });
    await app.onboarded("mia");
    app.stopPolling();

    expect(app.isAdmin).toBe(false);
    expect(e.enrolRequest).toHaveBeenCalledWith("mia");
    expect(app.whoami?.enrolment).toBe("pending");
  });
});

describe("after a change made here", () => {
  const bundle = { bundleId: "bndl-new" } as never;

  beforeEach(() => app.reset());

  it("fetches what the change moved, which a plain extended pass does not", async () => {
    const topics = { bundles: 1, nodes: 1 };
    e.stateVersions.mockImplementation(async () => ({ topics: { ...topics } }) as never);
    await app.refresh();
    await app.refreshExtended();
    e.bundles.mockClear();

    // A push lands: the engine's counter moves; no core tick has run.
    topics.bundles = 2;
    e.bundles.mockResolvedValue([bundle]);
    await app.refreshExtended();
    expect(e.bundles).not.toHaveBeenCalled();

    await app.refreshAfterChange();
    expect(e.bundles).toHaveBeenCalledTimes(1);
    expect(app.bundles).toEqual([bundle]);
  });

  it("keeps a topic owed when it moves again while its fetch is out", async () => {
    const topics = { bundles: 1 };
    e.stateVersions.mockImplementation(async () => ({ topics: { ...topics } }) as never);
    await app.refresh();
    await app.refreshExtended();

    // The first change is seen, and its fetch goes out…
    topics.bundles = 2;
    await app.refresh();
    let answer!: (v: unknown) => void;
    e.bundles.mockReturnValueOnce(new Promise((r) => (answer = r)) as never);
    const pass = app.refreshExtended();
    await Promise.resolve();
    // …and a second lands before the answer, which predates it.
    topics.bundles = 3;
    await app.refresh();
    answer([]);
    await pass;

    e.bundles.mockClear();
    e.bundles.mockResolvedValue([bundle]);
    await app.refreshExtended();
    expect(e.bundles).toHaveBeenCalledTimes(1);
    expect(app.bundles).toEqual([bundle]);
  });

  it("waits out a pass already under way instead of standing aside", async () => {
    const topics = { bundles: 1 };
    e.stateVersions.mockImplementation(async () => ({ topics: { ...topics } }) as never);
    await app.refresh();
    await app.refreshExtended();

    // A tick's pass is out, holding the answer from before the push.
    let answer!: (v: unknown) => void;
    e.top.mockReturnValueOnce(new Promise((r) => (answer = r)) as never);
    const tick = app.refreshExtended();
    topics.bundles = 2;
    e.bundles.mockResolvedValue([bundle]);

    const caughtUp = app.refreshAfterChange();
    setTimeout(() => answer([]), 120);
    await caughtUp;
    await tick;
    expect(app.bundles).toEqual([bundle]);
  });
});
