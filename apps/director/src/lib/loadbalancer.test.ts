import { describe, expect, it } from "vitest";
import { hostPort, ident, lbConfig, portShape, type Backend, type LbOptions } from "./loadbalancer";

const WHEN = new Date("2026-09-14T10:32:00Z");
const OPTS: LbOptions = { service: "metrics-relay", domain: "example.com", listen: 80, healthPath: "healthz" };
const TWO: Backend[] = [
  { node: "forge", address: "192.168.1.31", port: 8080 },
  { node: "anvil", address: "192.168.1.32", port: 8080 },
];

describe("addresses", () => {
  it("brackets an IPv6 address and leaves IPv4 alone", () => {
    expect(hostPort("192.168.1.31", 8080)).toBe("192.168.1.31:8080");
    expect(hostPort("fd00::31", 8080)).toBe("[fd00::31]:8080");
    expect(hostPort("[fd00::31]", 8080)).toBe("[fd00::31]:8080");
  });

  it("tells a list that stays put from one that moves", () => {
    expect(portShape([])).toEqual({ kind: "none" });
    expect(portShape([8080, 8080, 8080])).toEqual({ kind: "fixed", port: 8080 });
    expect(portShape([20007, 20001, 20001])).toEqual({ kind: "dynamic", ports: [20001, 20007] });
  });

  it("makes identifiers every format accepts", () => {
    expect(ident("metrics-relay")).toBe("metrics_relay");
    expect(ident("9lives")).toBe("s_9lives");
    expect(ident("--")).toBe("service");
  });
});

describe("configurations", () => {
  it("nginx: one server line per copy, and the health path is not invented", () => {
    const out = lbConfig("nginx", TWO, OPTS, WHEN);
    expect(out).toContain("upstream metrics_relay {");
    expect(out).toContain("    server 192.168.1.31:8080 max_fails=3 fail_timeout=10s;");
    expect(out).toContain("    server 192.168.1.32:8080 max_fails=3 fail_timeout=10s;");
    expect(out).toContain("server_name example.com;");
    expect(out).toContain("2026-09-14 10:32 UTC");
    expect(out.endsWith("\n")).toBe(true);
  });

  it("HAProxy: server names stay unique when two copies share a machine", () => {
    const same: Backend[] = [
      { node: "forge", address: "192.168.1.31", port: 20001 },
      { node: "forge", address: "192.168.1.31", port: 20002 },
    ];
    const out = lbConfig("haproxy", same, OPTS, WHEN);
    expect(out).toContain("server forge 192.168.1.31:20001 check");
    expect(out).toContain("server forge_2 192.168.1.31:20002 check");
    // A path typed without its slash is still a path.
    expect(out).toContain("option httpchk GET /healthz");
  });

  it("Caddy: a non-standard listen port is part of the site address", () => {
    expect(lbConfig("caddy", TWO, { ...OPTS, listen: 8443 }, WHEN)).toContain("example.com:8443 {");
    expect(lbConfig("caddy", TWO, { ...OPTS, domain: "" }, WHEN)).toContain(":80 {");
  });

  it("the address list is only addresses — it gets pasted into a form field", () => {
    expect(lbConfig("list", TWO, OPTS, WHEN)).toBe("192.168.1.31:8080\n192.168.1.32:8080\n");
  });

  it("with nothing selected, says so instead of writing an empty upstream", () => {
    const out = lbConfig("nginx", [], OPTS, WHEN);
    expect(out).not.toContain("upstream");
    expect(out).toContain("No copies are selected");
  });
});
