import { describe, expect, it } from "vitest";
import type { AuditView } from "./bindings/AuditView";
import type { DeploymentInstanceView } from "./bindings/DeploymentInstanceView";
import type { DeploymentStatusView } from "./bindings/DeploymentStatusView";
import {
  auditActivity,
  copyActivity,
  copyRows,
  describeCopy,
  explainNow,
  mergeActivity,
  slotName,
  stageOf,
} from "./deploymentStory";

function inst(slot: string, node: string, generation: number, state: string, health: string, healthDetail = ""): DeploymentInstanceView {
  return { jobId: `job-${slot}-${generation}`, slot, node, generation, state, health, healthDetail, startedAtMicros: 1 };
}

function status(over: Partial<DeploymentStatusView> = {}): DeploymentStatusView {
  return {
    name: "web",
    service: "web@1",
    count: "2",
    targets: "any",
    generation: 3,
    status: "active",
    desired: 2,
    running: 2,
    healthy: 2,
    reconciler: "forge",
    updatingFrom: null,
    degradedReason: null,
    canarying: false,
    awaitingPromotion: false,
    stalledReason: null,
    policyBlind: [],
    portLabels: [],
    placement: null,
    unreachable: 0,
    availability: "If a machine goes away, its copy is replaced after 1 min.",
    runtime: "container",
    instances: [inst("0", "forge", 3, "running", "healthy"), inst("1", "anvil", 3, "running", "healthy")],
    ...over,
  };
}

function audit(action: string, detail: string, over: Partial<AuditView> = {}): AuditView {
  return { auditId: `${action}-${detail}`, atMicros: 100, userId: "u", username: "sam", action, target: "web", nodeShort: "n", outcome: "ok", detail, ...over };
}

describe("a copy's stage", () => {
  it("reads the engine's state and health as one step on the way to serving", () => {
    expect(stageOf(inst("0", "", 3, "absent", "unknown"))).toBe("waiting");
    expect(stageOf(inst("0", "forge", 3, "queued", "unknown"))).toBe("starting");
    expect(stageOf(inst("0", "forge", 3, "running", "starting"))).toBe("checking");
    expect(stageOf(inst("0", "forge", 3, "running", "healthy"))).toBe("healthy");
    // No check defined: running is as healthy as anybody can tell.
    expect(stageOf(inst("0", "forge", 3, "running", "unknown"))).toBe("healthy");
    expect(stageOf(inst("0", "forge", 3, "running", "unhealthy"))).toBe("failing");
    expect(stageOf(inst("0", "forge", 3, "failed", "unhealthy"))).toBe("failing");
    expect(stageOf(inst("0", "forge", 3, "running", "unreachable"))).toBe("away");
  });

  it("names a numbered slot as a copy and leaves a machine's name alone", () => {
    expect(slotName("0")).toBe("Copy 1");
    expect(slotName("edge-7")).toBe("edge-7");
  });

  it("counts a copy that was just asked for, before any row speaks for it", () => {
    const { current, tally } = copyRows(status({ desired: 3 }));
    expect(current).toHaveLength(3);
    expect(tally.waiting).toBe(1);
    expect(tally.healthy).toBe(2);
  });

  it("mid-canary, a copy not yet replaced is waiting for the canary, not for a machine", () => {
    const { current, tally } = copyRows(
      status({
        canarying: true,
        awaitingPromotion: true,
        updatingFrom: 2,
        instances: [
          inst("0", "forge", 3, "running", "healthy"),
          inst("1", "", 3, "absent", "unknown"),
          inst("0", "forge", 2, "running", "healthy"),
          inst("1", "anvil", 2, "running", "healthy"),
        ],
      }),
    );
    expect(current[1].stage).toBe("next");
    expect(current[1].text).toBe("Waiting for the canary");
    expect(tally.waiting).toBe(0);
    expect(tally.next).toBe(1);
  });

  it("mid-roll, a copy the engine says it cannot place is still stuck", () => {
    const { current } = copyRows(
      status({
        updatingFrom: 2,
        instances: [
          inst("0", "forge", 3, "running", "healthy"),
          inst("1", "", 3, "absent", "unknown", "not placed — no machine has 2 GiB of memory free"),
          inst("1", "anvil", 2, "running", "healthy"),
        ],
      }),
    );
    expect(current[1].stage).toBe("waiting");
  });
});

describe("right now", () => {
  it("says everything is running only when it is", () => {
    const now = explainNow(status());
    expect(now.tone).toBe("ok");
    expect(now.headline).toBe("Everything is running");
    expect(now.detail).toContain("All 2 copies are healthy on 2 machines");
  });

  it("after a scale-up: how many are starting, and why one is not placed", () => {
    const now = explainNow(
      status({
        desired: 3,
        status: "progressing",
        instances: [
          inst("0", "forge", 3, "running", "healthy"),
          inst("1", "anvil", 3, "running", "healthy"),
          inst("2", "", 3, "absent", "unknown", "not placed — no machine has 2 GiB of memory free"),
        ],
      }),
    );
    expect(now.tone).toBe("working");
    expect(now.headline).toBe("Starting 1 copy");
    expect(now.detail).toBe("You asked for 3 copies and 2 are healthy. Of the rest: 1 waiting for a machine.");
    expect(now.why).toEqual(["No machine has 2 GiB of memory free"]);
  });

  it("a pause outranks a failure: nothing will fix it while paused", () => {
    const now = explainNow(status({ status: "paused", degradedReason: "restart budget exhausted" }));
    expect(now.tone).toBe("paused");
    expect(now.action).toBe("resume");
  });

  it("a failure is said with the engine's own reason", () => {
    const now = explainNow(
      status({
        status: "degraded",
        degradedReason: "restart budget exhausted: 5 failures in 10m on edge-7",
        instances: [inst("0", "forge", 3, "running", "healthy"), inst("1", "edge-7", 3, "failed", "unhealthy", "exit 1")],
      }),
    );
    expect(now.tone).toBe("problem");
    expect(now.why[0]).toBe("restart budget exhausted: 5 failures in 10m on edge-7");
    expect(now.next).toContain("Copy 2");
  });

  it("a roll counts new copies healthy and old ones still serving", () => {
    const now = explainNow(
      status({
        status: "updating",
        updatingFrom: 2,
        instances: [
          inst("0", "forge", 3, "running", "healthy"),
          inst("1", "anvil", 3, "running", "starting"),
          inst("1", "anvil", 2, "running", "healthy"),
        ],
      }),
    );
    expect(now.headline).toBe("Rolling out generation 3");
    expect(now.detail).toContain("1 of 2 new copies is healthy");
    expect(now.detail).toContain("1 copy of the old version is still serving");
  });

  it("a held canary asks for the one click it is waiting on", () => {
    expect(explainNow(status({ canarying: true, awaitingPromotion: true })).action).toBe("promote");
  });
});

describe("activity", () => {
  it("turns this deployment's audit rows into sentences and ignores the rest", () => {
    const rows = [
      audit("deployment.put", "generation 4", { atMicros: 300 }),
      audit("deployment.unplaced", "generation 4 slot 2: no machine has 2 GiB of memory free", { atMicros: 400 }),
      audit("deployment.status", "", { atMicros: 500 }),
      audit("deployment.put", "generation 9", { target: "other", atMicros: 600 }),
      audit("deployment.create", "generation 1", { atMicros: 50 }),
    ];
    const out = auditActivity(rows, "web", 100);
    expect(out.map((a) => a.text)).toEqual([
      "published a change — now generation 4",
      "Could not place Copy 3 — no machine has 2 GiB of memory free",
    ]);
    expect(out[0].who).toBe("sam");
    expect(out[1].who).toBeNull();
  });

  it("a create is the whole event, without the generation its row was stamped on", () => {
    expect(auditActivity([audit("deployment.create", "generation 0")], "web")[0].text).toBe("created it");
  });

  it("says a refusal is a refusal", () => {
    const [row] = auditActivity([audit("deployment.remove", "", { outcome: "denied: requires admin" })], "web");
    expect(row.tone).toBe("error");
    expect(row.text).toBe("tried to delete it and was refused — denied: requires admin");
  });

  it("walks a copy's incarnations: placed, started, and how it ended", () => {
    const out = copyActivity(
      "1",
      [{ jobId: "j", generation: 2, incarnation: 0, node: "edge-7", state: "failed", createdAtMicros: 10, startedAtMicros: 20, finishedAtMicros: 30, exitCode: 1 }],
      3,
    );
    expect(out.map((a) => a.text)).toEqual([
      "Copy 2 was placed on edge-7 (generation 2)",
      "Copy 2 started on edge-7 (generation 2)",
      "Copy 2 failed on edge-7 with exit code 1 (generation 2)",
    ]);
    expect(out[2].tone).toBe("error");
  });

  it("merges newest first, each event once", () => {
    const a = { key: "a", at: 1, tone: "info" as const, who: null, text: "a" };
    const b = { key: "b", at: 2, tone: "info" as const, who: null, text: "b" };
    expect(mergeActivity([a, b], [a]).map((x) => x.key)).toEqual(["b", "a"]);
  });
});

// A copy that keeps failing to start used to come through as `absent`, with
// no machine and no reason, and read "Waiting for a machine" for as long as
// it failed. The node now sends the failed attempt; this is what it says.
describe("a copy that fails to start", () => {
  const detail =
    "the container engine could not start it (exit 126), 4 times in the last 10m — starting it again in 5m. Its log says why";
  const failing = inst("merope", "merope", 0, "failed", "unknown", detail);

  it("reads as failing on its machine, with the node's reason", () => {
    expect(stageOf(failing)).toBe("failing");
    const said = describeCopy(failing, "failing");
    expect(said.text).toBe("Stopped on merope — failed");
    expect(said.why).toBe(
      "The container engine could not start it (exit 126), 4 times in the last 10m — starting it again in 5m. Its log says why",
    );
  });

  it("makes the header say copies are failing, not waiting for a machine", () => {
    const s = status({
      count: "system",
      generation: 0,
      desired: 3,
      running: 1,
      healthy: 1,
      instances: [inst("NIM", "NIM", 0, "running", "unknown"), failing, inst("APUS", "APUS", 0, "failed", "unknown", detail)],
    });
    const now = explainNow(s);
    expect(now.headline).toBe("2 copies failing");
    expect(now.tone).toBe("problem");
    expect(now.detail).not.toContain("waiting for a machine");
    expect(now.next).toContain("Open its logs");
  });
});
