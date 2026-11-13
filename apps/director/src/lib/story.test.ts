// What the sentences must not get wrong.
//
// `story.ts` is a second reading of strings the engine formatted — the
// detail of a `job.run` is `format!("{} on {}", preview, node)` in
// `daemon.rs`, and this file parses it back apart. That is a coupling
// across the language boundary with nothing but agreement holding it
// together, so these are the vectors: one per call-site shape that gets
// taken apart, plus the two properties that keep a drift survivable —
// an unknown action still produces a sentence, and a refusal still says
// it was refused.
import { describe, expect, it } from "vitest";
import type { AuditView } from "./api";
import {
  clockOf,
  dayLabelOf,
  groupByDay,
  namesFrom,
  storyExport,
  storyOf,
  storyText,
} from "./story";

const row = (over: Partial<AuditView>): AuditView => ({
  auditId: "aud-1",
  atMicros: Date.UTC(2026, 8, 9, 12, 2) * 1000,
  userId: "mwu1sam",
  username: "sam",
  action: "job.run",
  target: "job-7k1x8q2mzr",
  nodeShort: "hq",
  outcome: "ok",
  detail: "uname -a on anvil",
  ...over,
});

describe("the shapes daemon.rs formats", () => {
  it("reads a plain run back into a sentence", () => {
    expect(storyText(row({}))).toBe("sam ran uname -a on anvil · job-7k1x…");
  });

  it("reads a fan-out", () => {
    const s = storyText(row({ detail: "fanout 14 × systemctl restart app" }));
    expect(s).toBe("sam ran systemctl restart app on 14 nodes · job-7k1x…");
  });

  it("reads a gang, keeping what is in the parentheses", () => {
    const s = storyText(row({ detail: "gang 8 × ./train.sh (1 gpu each)" }));
    expect(s).toBe("sam started a gang of 8 running ./train.sh (1 gpu each) · job-7k1x…");
  });

  it("reads a parallel run", () => {
    const s = storyText(row({ action: "job.parallel", detail: "800 items over 6 node(s)" }));
    expect(s).toBe("sam spread 800 items over 6 node(s) · job-7k1x…");
  });

  it("says a cache hit did not run", () => {
    const s = storyText(
      row({
        action: "job.cached",
        detail: "./build.sh served from act-3f9c1a (produced by job-9x2m4k)",
      }),
    );
    expect(s).toBe(
      "sam did not re-run ./build.sh — served from the cache · first produced by job-9x2m4k",
    );
  });

  it("splits a grant into role, person and scope", () => {
    const s = storyText(
      row({
        action: "grant.add",
        target: "mwu1tt5rq9zz",
        detail: "operator on nodes=tag=edge services=*",
      }),
      namesFrom([{ userId: "mwu1tt5rq9zz", shortId: "mwu1tt5r", username: "mia" }], []),
    );
    expect(s).toBe("sam gave operator to mia on nodes=tag=edge services=*");
  });

  it("calls an unscoped grant what it is", () => {
    const s = storyText(row({ action: "grant.add", target: "mwu1x", detail: "admin on nodes=* services=*" }));
    expect(s).toContain("gave admin to");
    expect(s).toContain("everywhere");
  });
});

describe("names", () => {
  it("resolves a node key to the name people use", () => {
    const names = namesFrom([], [{ nodeId: "mw1kdzr7ah76y", shortId: "mw1kdzr7", name: "anvil" }]);
    const s = storyText(row({ action: "node.pair", target: "mw1kdzr7ah76y", detail: "" }), names);
    expect(s).toBe("sam paired anvil");
  });

  it("shortens an id nothing can name, rather than printing 52 characters", () => {
    const s = storyText(row({ action: "node.revoke", target: "mw1kdzr7ah76y3dnj4ccumuwrppexvs", detail: "" }));
    expect(s).toBe("sam revoked mw1kdzr7…");
  });
});

describe("outcomes", () => {
  it("keeps the refusal message and marks the tone", () => {
    const s = storyOf(row({ outcome: "denied: needs 2 approvals", detail: "", target: "(ad-hoc)" }));
    expect(s.tone).toBe("refused");
    expect(s.refusal).toBe("needs 2 approvals");
    expect(s.pieces.map((p) => p.v).join("")).toBe(
      "sam tried to run an ad-hoc command — refused: needs 2 approvals",
    );
  });

  it("never says a refused thing happened", () => {
    // The row that made this rule: `service.create` denied. Its template is
    // "published the service x", which is the one sentence the mesh spent
    // the refusal to prevent.
    const s = storyText(
      row({
        username: "lab-op",
        action: "service.create",
        target: "x",
        outcome: "denied: requires one of: admin, developer",
        detail: "",
      }),
    );
    expect(s).toBe("lab-op tried to create service x — refused: requires one of: admin, developer");
  });

  it("treats a degraded deployment as a notice, not a failure", () => {
    const s = storyOf(
      row({ action: "deployment.degraded", target: "api", outcome: "degraded", detail: "2 of 6 replicas" }),
    );
    expect(s.tone).toBe("notice");
    expect(s.pieces.map((p) => p.v).join("")).toBe(
      "The deployment api went degraded — 2 of 6 replicas · owned by sam",
    );
  });

  it("does not lead a machine-written row with a person's name", () => {
    const s = storyText(
      row({ action: "schedule.fire", target: "backup-nightly", detail: "02:00 workflow backup run wfr-3f9c" }),
    );
    expect(s.startsWith("The schedule backup-nightly fired")).toBe(true);
  });
});

describe("the fallback", () => {
  // The engine gains audit actions faster than this table does. A new one
  // must still read as a sentence — clumsy is fine, blank is not.
  it("says an unknown action in words", () => {
    const s = storyText(row({ action: "widget.frobnicate", target: "thing-1", detail: "twice" }));
    expect(s).toBe("sam did widget frobnicate on thing-1 — twice");
  });

  it("still marks an unknown action that was refused", () => {
    const s = storyText(row({ action: "widget.frobnicate", target: "thing-1", outcome: "denied: no", detail: "" }));
    expect(s).toBe("sam tried to frobnicate widget thing-1 — refused: no");
  });

  it("uses a name for a person even when the row has no username", () => {
    expect(storyText(row({ username: "", action: "jobs.clear", target: "all", detail: "12 jobs" }))).toBe(
      "somebody cleared the job history — 12 jobs",
    );
  });
});

describe("days", () => {
  const now = new Date(2026, 8, 9, 18, 0);
  const at = (y: number, m: number, d: number, h: number, min: number) =>
    new Date(y, m, d, h, min).getTime() * 1000;

  it("names today and yesterday, then dates", () => {
    expect(dayLabelOf(at(2026, 8, 9, 14, 2), now)).toBe("Today");
    expect(dayLabelOf(at(2026, 8, 8, 23, 59), now)).toBe("Yesterday");
    expect(dayLabelOf(at(2026, 8, 1, 9, 0), now)).toBe("Tue 1 Sep");
    expect(dayLabelOf(at(2025, 11, 24, 9, 0), now)).toBe("Wed 24 Dec 2025");
  });

  it("groups consecutive rows without reordering them", () => {
    const rows = [
      row({ auditId: "a", atMicros: at(2026, 8, 9, 15, 0) }),
      row({ auditId: "b", atMicros: at(2026, 8, 9, 14, 0) }),
      row({ auditId: "c", atMicros: at(2026, 8, 8, 22, 0) }),
    ];
    const days = groupByDay(rows, now);
    expect(days.map((d) => d.label)).toEqual(["Today", "Yesterday"]);
    expect(days[0].rows.map((r) => r.auditId)).toEqual(["a", "b"]);
    expect(days[1].rows).toHaveLength(1);
  });

  it("never puts one day in two places, whatever order the rows arrive in", () => {
    // Story order, as the mock's fixture was written (UX-18): a day, an
    // older day, then the first day again.
    const rows = [
      row({ auditId: "a", atMicros: at(2026, 8, 9, 15, 0) }),
      row({ auditId: "old", atMicros: at(2026, 7, 20, 9, 0) }),
      row({ auditId: "b", atMicros: at(2026, 8, 9, 14, 0) }),
      row({ auditId: "y", atMicros: at(2026, 8, 8, 10, 0) }),
    ];
    const days = groupByDay(rows, now);
    expect(new Set(days.map((d) => d.key)).size).toBe(days.length);
    expect(days.map((d) => d.label)).toEqual(["Today", "Yesterday", "Thu 20 Aug"]);
    expect(days[0].rows.map((r) => r.auditId)).toEqual(["a", "b"]);
  });

  it("prints local wall-clock time", () => {
    expect(clockOf(at(2026, 8, 9, 9, 5))).toBe("09:05");
  });

  it("exports something a person can paste into a ticket", () => {
    const text = storyExport([row({ atMicros: at(2026, 8, 9, 14, 2) })], undefined, now);
    expect(text).toBe("## Today\n14:02  sam ran uname -a on anvil · job-7k1x…  [hq]");
  });
});
