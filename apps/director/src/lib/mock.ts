// Dev-only fake engine for working on the UI without a Tauri shell or a
// mesh: `npm run dev`, then open http://localhost:1420/?mock=1 in a browser.
// It installs the `window.__TAURI_INTERNALS__` surface that
// @tauri-apps/api's `invoke` / `Channel` call into, and answers every
// Director command from an in-memory mesh with the same view-model shapes
// the Rust side produces (src/lib/bindings). Never loaded in production
// builds (guarded by import.meta.env.DEV in main.ts).

import type { StatusView } from "./bindings/StatusView";
import type { NodeView } from "./bindings/NodeView";
import type { CandidateView } from "./bindings/CandidateView";
import type { LanWatchView } from "./bindings/LanWatchView";
import type { TopView } from "./bindings/TopView";
import type { DeviceView } from "./bindings/DeviceView";
import type { TotalsView } from "./bindings/TotalsView";
import type { JobView } from "./bindings/JobView";
import type { ServiceView } from "./bindings/ServiceView";
import type { BundleView } from "./bindings/BundleView";
import type { SourceView } from "./bindings/SourceView";
import type { SecretView } from "./bindings/SecretView";
import type { UserView } from "./bindings/UserView";
import type { AgentKeyView } from "./bindings/AgentKeyView";
import type { AttestRootView } from "./bindings/AttestRootView";
import type { GrantView } from "./bindings/GrantView";
import type { AuditView } from "./bindings/AuditView";
import type { WhoAmIView } from "./bindings/WhoAmIView";
import type { EnrolmentView } from "./bindings/EnrolmentView";
import type { DefinitionView } from "./bindings/DefinitionView";
import type { RunEvent } from "./bindings/RunEvent";
import type { ParallelEvent } from "./bindings/ParallelEvent";
import type { NodeProgressView } from "./bindings/NodeProgressView";
import type { DrainEvent } from "./bindings/DrainEvent";
import type { PlacementNodeView } from "./bindings/PlacementNodeView";
import { drainOf, isDrainTag } from "./drain";
import type { RetentionView } from "./bindings/RetentionView";
import type { SinkView } from "./bindings/SinkView";
import type { SinkKindView } from "./bindings/SinkKindView";
import type { HistoryJobView } from "./bindings/HistoryJobView";
import type { HistoryJobPageView } from "./bindings/HistoryJobPageView";
import type { PayloadWindowView } from "./bindings/PayloadWindowView";
import type { NodeFactsView } from "./bindings/NodeFactsView";
import type { FactCollectorView } from "./bindings/FactCollectorView";
import type { ResultSetView } from "./bindings/ResultSetView";
import type { DeploymentView } from "./bindings/DeploymentView";
import type { DeploymentStatusView } from "./bindings/DeploymentStatusView";
// N1–N6, the Nomad track.
import type { DeploymentCheckView } from "./bindings/DeploymentCheckView";
import type { ServiceScheduleView } from "./bindings/ServiceScheduleView";
import type { ServiceScheduleCheckView } from "./bindings/ServiceScheduleCheckView";
import type { PortLedgerViewT } from "./bindings/PortLedgerViewT";
import type { RuntimesViewT } from "./bindings/RuntimesViewT";
import type { RegistryRowView } from "./bindings/RegistryRowView";
import type { RegistryInstanceView } from "./bindings/RegistryInstanceView";
import type { DnsStatusViewT } from "./bindings/DnsStatusViewT";
import type { VolumeRowView } from "./bindings/VolumeRowView";
import type { ShedView } from "./bindings/ShedView";
import type { DeploymentInstanceView } from "./bindings/DeploymentInstanceView";
import type { QueueRowView } from "./bindings/QueueRowView";
import type { GangRowView } from "./bindings/GangRowView";
import type { CacheStatsView } from "./bindings/CacheStatsView";
import type { KeyFieldView } from "./bindings/KeyFieldView";
import type { RememberedCredentialView } from "./bindings/RememberedCredentialView";
import type { DeployAuthDraft } from "./bindings/DeployAuthDraft";
import type { JobExplainView } from "./bindings/JobExplainView";
import type { PlacementView } from "./bindings/PlacementView";
import type { CandidateScoreView } from "./bindings/CandidateScoreView";
import type { ScoreTermView } from "./bindings/ScoreTermView";
import type { PlanView } from "./bindings/PlanView";

type Args = Record<string, unknown>;

/**
 * One fleet, one build (UX-18).
 *
 * The fixtures had grown four versions of Director and meshnode between
 * them — 0.2.0 in the status line, 0.5.x and 0.6.x on the nodes, 0.7.2 in
 * the manifest — and a demo recorded against them showed all four. Every
 * version in here is one of these: what this build is, the two a fleet mid-
 * rollout is still on, and the one the release channel is offering. The
 * protocol is `mesh-proto`'s.
 */
const V = { current: "0.7.2", previous: "0.7.1", older: "0.7.0", next: "0.7.3", proto: "1.35" };


/// M17: two live terminals and the policies that permit them.
let sessions = [
  {
    id: 4,
    kind: "shell",
    detail: "shell",
    hosting: true,
    peerName: "anvil",
    username: "ada",
    reason: "INC-4821 investigating disk pressure",
    osUser: "meshnode",
    openedAtMicros: Date.now() * 1000 - 11 * 60e6,
    bytesIn: 2_143,
    bytesOut: 48_512,
  },
];

let sessionPolicies = [
  {
    id: "dev-terminals",
    description: "on-call troubleshooting on the dev tier",
    users: "*",
    nodes: "tag=dev",
    allow: ["shell"],
    expiresAtMicros: null as number | null,
    createdAtMicros: Date.now() * 1000 - 9 * 24 * 3600e6,
    createdByUsername: "root",
    coversThisNode: true,
  },
  {
    id: "oncall-prod",
    description: "the prod on-call rotation, renewed each quarter",
    users: "ops-*",
    nodes: "tag=prod",
    allow: ["shell"],
    expiresAtMicros: Date.now() * 1000 + 40 * 24 * 3600e6,
    createdAtMicros: Date.now() * 1000 - 30 * 24 * 3600e6,
    createdByUsername: "root",
    coversThisNode: false,
  },
];
type ChannelLike = { onmessage: (m: unknown) => void };

const now = () => Date.now() * 1000;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const b32 = "abcdefghijklmnopqrstuvwxyz234567";
const rid = (prefix: string, n: number) =>
  prefix + Array.from({ length: n }, () => b32[Math.floor(Math.random() * 32)]).join("");

const FOUNDER = "mwu1zmqqlw5kdeaslg54nkbtgxlmednkg7nniqyg6ff2l2jzgsvv5nya";
const SELF = "mw1sdj5gterlfavarnvrbfu4co3m6idggrhwjikv3lhoodb6h7kzwhq";

let nodes: NodeView[] = [
  { nodeId: SELF, shortId: "mw1sdj5gterlfav", name: "director", addrs: ["192.168.1.20:40986"], tags: ["role=control"], liveness: "alive", isSelf: true, revoked: false, left: false, joinedAtMicros: now() - 3.6e9, executing: true, capacity: { cores: 8, memMb: 32_768, diskMb: 480_000, arch: "x86_64" }, acceptsTerminals: false, noShell: false, stale: null, sync: null, version: V.current , contributes: true, contributeReason: "", assignedTags: [], quarantined: null },
  { nodeId: rid("mw1", 52), shortId: "mw1k7q2v9xm4c3p", name: "forge", addrs: ["192.168.1.31:7777"], tags: ["role=lab", "gpu", "dc=home"], liveness: "alive", isSelf: false, revoked: false, left: false, joinedAtMicros: now() - 7.2e9, executing: true, capacity: { cores: 32, memMb: 131_072, diskMb: 3_800_000, arch: "x86_64" }, acceptsTerminals: true, noShell: false, stale: null, sync: { behind: 0, total: 1_284, authors: 0, ageMicros: 2.1e6, pct: 100 }, version: V.current , contributes: true, contributeReason: "", assignedTags: [], quarantined: null },
  { nodeId: rid("mw1", 52), shortId: "mw1b2n8e5ry7wqa", name: "anvil", addrs: ["192.168.1.32:7777"], tags: ["role=lab", "dc=home", "git-sync"], liveness: "alive", isSelf: false, revoked: false, left: false, joinedAtMicros: now() - 6.1e9, executing: true, capacity: { cores: 16, memMb: 65_536, diskMb: 900_000, arch: "x86_64" }, acceptsTerminals: true, noShell: false, stale: null, sync: { behind: 6, total: 1_284, authors: 1, ageMicros: 4.4e6, pct: 99 }, version: V.previous , contributes: true, contributeReason: "", assignedTags: [], quarantined: "unexpected outbound to 5.6.7.8 — under investigation" },
  { nodeId: rid("mw1", 52), shortId: "mw1zx4p6hq9dm2s", name: "edge-7", addrs: ["203.0.113.5:7777"], tags: ["role=edge", "dc=east", "no-shell"], liveness: "suspect", isSelf: false, revoked: false, left: false, joinedAtMicros: now() - 86400e6, executing: true, capacity: { cores: 4, memMb: 8_192, diskMb: 120_000, arch: "aarch64" }, acceptsTerminals: true, noShell: true, stale: null, sync: { behind: 212, total: 1_284, authors: 3, ageMicros: 47e6, pct: 83 }, version: V.previous , contributes: true, contributeReason: "", assignedTags: [], quarantined: null },
  { nodeId: rid("mw1", 52), shortId: "mw1tt5r8lkc2v7b", name: "laptop-mia", addrs: [], tags: ["no-execute"], liveness: "alive", isSelf: false, revoked: false, left: false, joinedAtMicros: now() - 1200e6, executing: false, capacity: null, acceptsTerminals: false, noShell: false, stale: null, sync: { behind: 0, total: 1_284, authors: 0, ageMicros: 9.8e6, pct: 100 }, version: null, contributes: true, contributeReason: "", assignedTags: [], quarantined: null },
  // Somebody opened Director on `forge`, which is already in the mesh
  // through its own `meshnode`. Same machine, second member — the shape
  // that used to put one box in the fleet twice and report it outdated
  // twice, because Director announces its *bundled* build. Every fleet
  // surface must leave this row out; Access is where the person appears.
  { nodeId: rid("mw1", 52), shortId: "mw1cq3f8w2jt6nh", name: "forge", addrs: ["192.168.1.31:41022"], tags: ["no-execute", "console"], liveness: "alive", isSelf: false, revoked: false, left: false, joinedAtMicros: now() - 900e6, executing: false, capacity: { cores: 32, memMb: 131_072, diskMb: 3_800_000, arch: "x86_64" }, acceptsTerminals: false, noShell: false, stale: null, sync: { behind: 0, total: 1_284, authors: 0, ageMicros: 1.2e6, pct: 100 }, version: V.current, contributes: true, contributeReason: "", assignedTags: [], quarantined: null },
];

// Machine principals and identity providers, so the Access view's Machines
// tab is reviewable without a mesh (E1-P4, E1-P5).
let serviceAccounts: any[] = [
  { userId: rid("mwu1", 52), shortId: "mwu1ktdzhe6xkf", name: "ci-deploy", description: "GitHub Actions deploy", createdAtMicros: now() - 40 * 86_400e6, expiresAtMicros: now() + 50 * 86_400e6, expired: false, revoked: false, roles: ["developer"] },
  { userId: rid("mwu1", 52), shortId: "mwu1ba6clsqw7w", name: "nightly-backup", description: "cron on hq", createdAtMicros: now() - 200 * 86_400e6, expiresAtMicros: now() - 3 * 86_400e6, expired: true, revoked: false, roles: [] },
];
const oidcProviders: any[] = [
  { name: "entra", issuer: "https://login.microsoftonline.com/8c1e/v2.0", clientId: "meshware-fleet", groupsClaim: "groups", groupRoles: ["platform=operator", "sre-*=developer"], configuredAtMicros: now() - 12 * 86_400e6 },
];

// -- machines announcing themselves on the LAN --------------------------------
//
// What `discover_candidates` finds: machines sitting in the wizard's "Join
// a mesh on this network", waiting for somebody to let them in. Access
// browses for these in the background, so the mock needs more than one to
// show that the queue is a list rather than a special case.
let candidates: CandidateView[] = [
  {
    nodeName: "mia-laptop",
    nodeId: rid("mw1", 52),
    platform: "linux/x86_64",
    addrs: ["192.168.1.42:7777", "100.64.0.9:7777"],
    person: "Mia",
    hostname: "mia-laptop",
    viaMdns: true,
    viaBeacon: true,
    firstSeenMicros: now() - 48e6,
    lastSeenMicros: now() - 1e6,
  },
  {
    nodeName: "forge-2",
    nodeId: null,
    platform: "linux/x86_64",
    addrs: ["192.168.1.40:7777"],
    person: null,
    hostname: "forge-2",
    viaMdns: false,
    viaBeacon: true,
    firstSeenMicros: now() - 9e6,
    lastSeenMicros: now() - 2e6,
  },
  // A machine that was a member, left, and is back with the same key —
  // the queue says so instead of hiding it.
  {
    nodeName: "edge-7",
    nodeId: nodes[4].nodeId,
    platform: "linux/aarch64",
    addrs: ["203.0.113.5:7777"],
    person: "ops",
    hostname: "edge-7",
    viaMdns: true,
    viaBeacon: false,
    firstSeenMicros: now() - 120e6,
    lastSeenMicros: now() - 1e6,
  },
];
/** Whether the engine-side watch is on (`lan_watch_start` / `_stop`). */
let lanWatching = false;

/** The in-flight pairing exchange, so decide() can finish what start() began. */
let pairing: { ch: ChannelLike; name: string } | null = null;
/** The same, for the wizard's side of it (this Director as the joiner). */
let wizardPair: ChannelLike | null = null;

const pairingNameFor = (addrs: string[]) =>
  candidates.find((c) => c.addrs.some((a) => addrs.includes(a)))?.nodeName ?? addrs[0] ?? "?";

/** A machine that just paired, as the members table would then have it. */
function mockJoinedNode(nodeId: string, name: string): NodeView {
  candidates = candidates.filter((c) => c.nodeName !== name);
  return {
    nodeId,
    shortId: nodeId.slice(0, 15),
    name,
    addrs: [],
    tags: [],
    liveness: "alive",
    isSelf: false,
    revoked: false,
    left: false,
    joinedAtMicros: now(),
    executing: false,
    capacity: null,
    acceptsTerminals: false,
    noShell: false,
    stale: null,
    sync: { behind: 0, total: 1_284, authors: 0, ageMicros: 1e6, pct: 100 },
    version: V.current,
    contributes: true,
    contributeReason: "",
    assignedTags: [],
    quarantined: null,
  };
}

// -- a fleet worth designing against -----------------------------------------
//
// Five machines is a home lab, and every screen looks fine at five. The
// screens that matter are the ones somebody opens with sixty machines
// behind them, so `?mock=1&fleet=N` conjures that fleet: racks, regions,
// architectures, a version spread, and a handful of nodes that are down.
// Deliberately not uniform — a rollout screen that only reads well when
// every row is identical has not been designed, it has been arranged.
const FLEET_RACKS = ["rack-a", "rack-b", "rack-c", "rack-d"];
const FLEET_REGIONS = ["dc=east", "dc=west", "dc=central"];
const FLEET_ROLES = ["role=worker", "role=lab", "role=edge", "role=build"];
const FLEET_VERSIONS = [V.current, V.previous, V.previous, V.older, V.current, V.previous];

function syntheticFleet(count: number): NodeView[] {
  const out: NodeView[] = [];
  for (let i = 0; i < count; i++) {
    const rack = FLEET_RACKS[i % FLEET_RACKS.length];
    const region = FLEET_REGIONS[i % FLEET_REGIONS.length];
    const role = FLEET_ROLES[i % FLEET_ROLES.length];
    const arch = i % 7 === 3 ? "aarch64" : "x86_64";
    // A few nodes down, a few merely suspect: the states a rollout has to
    // have an answer for before it starts, not after.
    const liveness = i % 17 === 5 ? "dead" : i % 11 === 4 ? "suspect" : "alive";
    out.push({
      nodeId: rid("mw1", 52),
      shortId: rid("mw1", 12),
      name: `${rack}-${String(i + 1).padStart(2, "0")}`,
      addrs: [`10.20.${30 + (i % 4)}.${10 + i}:7777`],
      tags: [role, region, rack, ...(arch === "aarch64" ? ["arm"] : [])],
      liveness,
      isSelf: false,
      revoked: false,
      left: false,
      joinedAtMicros: now() - (2e9 + i * 3e7),
      executing: true,
      capacity: { cores: arch === "aarch64" ? 8 : 32, memMb: arch === "aarch64" ? 16_384 : 131_072, diskMb: 900_000, arch },
      acceptsTerminals: true,
      noShell: false,
      stale: null,
      sync: { behind: i % 9, total: 1_284, authors: 0, ageMicros: 3e6, pct: 100 },
      version: liveness === "dead" ? FLEET_VERSIONS[2] : FLEET_VERSIONS[i % FLEET_VERSIONS.length],
      // One machine in the synthetic fleet the owner has held back, so the
      // control has a state to be designed against rather than only a
      // happy path.
      contributes: i % 23 !== 7,
      contributeReason: i % 23 === 7 ? "not ours — contractor's laptop" : "",
      assignedTags: [],
      quarantined: null,
    });
  }
  return out;
}

{
  // The ceiling used to be 400, because that is roughly where the old
  // hub-and-spoke SVG stopped being a diagram and started being a hang —
  // and a mock you cannot ask for a big fleet is a mock that cannot show
  // you the screen you are worried about. The topology is a canvas now and
  // holds five thousand, so the cap is only here to stop a typo allocating
  // a gigabyte. Note that the Nodes *table* is the next thing that will
  // complain at these sizes; that is a real finding, not a reason to keep
  // the fleet small.
  const want = Number(new URLSearchParams(location.search).get("fleet") ?? 0);
  if (Number.isFinite(want) && want > 0) nodes.push(...syntheticFleet(Math.min(want, 20_000)));
}

// -- M18 facts ---------------------------------------------------------------
//
// Deliberately *not* uniform: a fleet where every box runs the same kernel
// is a fleet with no reason to want facts. The point of the panel is the
// one node that drifted.
const factDocs: Record<string, Record<string, unknown>> = {
  director: {
    arch: "x86_64", os: "linux", os_id: "debian", os_name: "Debian GNU/Linux 12 (bookworm)",
    os_version: "12", kernel_release: "6.1.0-18-amd64", cpu_cores: 8,
    cpu_model: "AMD Ryzen 7 5800X 8-Core Processor", memory_mb: 32768, disk_total_mb: 480000,
    data_dir_fstype: "ext4", package_manager: "apt", init_system: "systemd",
    boot_id: "6f0e2a41-8c1b-4d33-9f70-2a1c5b8e4d90", meshware_version: V.current,
  },
  forge: {
    arch: "x86_64", os: "linux", os_id: "debian", os_name: "Debian GNU/Linux 12 (bookworm)",
    os_version: "12", kernel_release: "6.1.0-18-amd64", cpu_cores: 32,
    cpu_model: "AMD Ryzen Threadripper PRO 5975WX", memory_mb: 131072, disk_total_mb: 3800000,
    data_dir_fstype: "zfs", package_manager: "apt", init_system: "systemd",
    boot_id: "b2d4e719-0f55-42a8-91cc-7e3a6d0f1b24", meshware_version: V.current,
  },
  anvil: {
    arch: "x86_64", os: "linux", os_id: "debian", os_name: "Debian GNU/Linux 12 (bookworm)",
    os_version: "12", kernel_release: "6.1.0-13-amd64", cpu_cores: 16,
    cpu_model: "Intel(R) Xeon(R) E-2288G CPU @ 3.70GHz", memory_mb: 65536, disk_total_mb: 900000,
    data_dir_fstype: "ext4", package_manager: "apt", init_system: "systemd",
    boot_id: "3a7c1d55-6b90-4e12-8f43-cc2e9a70b5d1", meshware_version: V.previous,
  },
  // The odd one out, and the reason anybody opens this panel.
  "edge-7": {
    arch: "aarch64", os: "linux", os_id: "ubuntu", os_name: "Ubuntu 22.04.4 LTS",
    os_version: "22.04", kernel_release: "5.15.0-1052-raspi", cpu_cores: 4,
    cpu_model: "Cortex-A76", memory_mb: 8192, disk_total_mb: 120000,
    data_dir_fstype: "ext4", package_manager: "apt", init_system: "systemd",
    virtualization: null, boot_id: "9e5f0b23-4a17-4c88-b6d2-1f8e3c47a029",
    meshware_version: V.previous,
  },
  "laptop-mia": {
    arch: "x86_64", os: "linux", os_id: "debian", os_name: "Debian GNU/Linux 12 (bookworm)",
    os_version: "12", kernel_release: "6.1.0-18-amd64", cpu_cores: 8,
    cpu_model: "Intel Core i7-1365U", memory_mb: 16384, disk_total_mb: 994000,
    data_dir_fstype: "ext4", package_manager: "apt", init_system: "systemd",
    boot_id: "a4c8d1f2-6b39-4e07-8d52-3f1a9c07be64", meshware_version: V.current,
  },
};

const factExtras: Record<string, Record<string, unknown>> = {
  forge: { gpu_model: "NVIDIA A100-SXM4-40GB", gpu_count: 2, driver_version: "550.54.14" , contributes: true, contributeReason: "", quarantined: null },
};

/** A fact document flattened the way the Rust side flattens it. */
function flattenDoc(doc: Record<string, unknown>): [string, string][] {
  const out: [string, string][] = [];
  const walk = (prefix: string, v: unknown) => {
    if (v === null || v === undefined) return;
    if (Array.isArray(v)) {
      out.push([prefix, v.map(String).join(",")]);
      return;
    }
    if (typeof v === "object") {
      for (const [k, child] of Object.entries(v as Record<string, unknown>)) {
        walk(prefix ? `${prefix}.${k}` : k, child);
      }
      return;
    }
    out.push([prefix, String(v)]);
  };
  walk("", doc);
  return out.sort((a, b) => a[0].localeCompare(b[0]));
}

function mockNodeFacts(subject: string): NodeFactsView {
  const name = subject.trim() === "" ? "director" : subject;
  const base = factDocs[name];
  if (!base) throw `mock: no node or host named "${name}"`;
  const observed = now() - 40 * 60e6;
  const docs = [
    {
      collector: "system",
      subjectName: name,
      entries: flattenDoc(base),
      json: JSON.stringify(base, null, 2),
      digest: rid("", 64),
      collectedAtMicros: observed,
      publishedAtMicros: observed + 400_000,
      authorName: name,
    },
  ];
  if (factExtras[name]) {
    const extra = factExtras[name];
    docs.push({
      collector: "gpu",
      subjectName: name,
      entries: flattenDoc(extra),
      json: JSON.stringify(extra, null, 2),
      digest: rid("", 64),
      collectedAtMicros: now() - 12 * 60e6,
      publishedAtMicros: now() - 12 * 60e6 + 300_000,
      authorName: name,
    });
  }
  // A short, plausible history: the baseline, then one real change.
  const history =
    name === "anvil"
      ? [
          {
            collector: "system",
            atMicros: now() - 3 * 86400e6,
            changes: [
              { key: "kernel_release", verb: "changed", before: "6.1.0-10-amd64", after: "6.1.0-13-amd64" },
              { key: "boot_id", verb: "changed", before: "1c8f…", after: "3a7c…" },
            ],
          },
          {
            collector: "system",
            atMicros: now() - 61 * 86400e6,
            changes: flattenDoc(base).map(([key, after]) => ({ key, verb: "added", before: null, after })),
          },
        ]
      : [
          {
            collector: "system",
            atMicros: observed,
            changes: flattenDoc(base).map(([key, after]) => ({ key, verb: "added", before: null, after })),
          },
        ];
  return { subjectName: name, docs, history, historyAvailable: true };
}

const factCollectors: FactCollectorView[] = [
  {
    name: "system", run: "(built in)", on: "any", intervalSecs: 900, subject: "node",
    paused: false, builtIn: true, subjects: 5, flappingKeys: [], source: "",
    description: "OS, kernel, CPU, memory, disk and boot identity, read in process on every node",
  },
  {
    name: "gpu", run: "bash script", on: "tag=gpu", intervalSecs: 3600, subject: "node",
    paused: false, builtIn: false, subjects: 1, flappingKeys: [],
    description: "GPU model and driver version",
    source: "collector: gpu\non: tag=gpu\nevery: 3600s\nbash: |-\n  nvidia-smi --query-gpu=name,driver_version --format=csv,noheader\n",
  },
];

// `?mock=1&peers=N` trims the mesh so the small-population layouts (the
// topology ring at n=1..3, the one-node empty state) can actually be seen
// without unplugging hardware.
{
  const raw = new URLSearchParams(location.search).get("peers");
  const want = raw === null ? null : Number(raw);
  if (want !== null && Number.isFinite(want) && want >= 0) nodes.splice(want + 1);
}

/** The synthetic fleet's live load.
 *
 * The first four machines keep their hand-chosen readings — the 96%-CPU,
 * 0.31-load-per-core pairing is a deliberate case other surfaces are built
 * against, and drifting it would take that case away. Everything past them
 * gets a spread that looks like a real fleet (a wide bulk of moderate load
 * with a hot tail) and, more to the point, one that *moves* between polls.
 *
 * Topology colours machines by load. Against a constant 12% it can never
 * show that it is live, so a mock whose load never changes is a mock that
 * cannot exercise the thing the view exists to do.
 */
function syntheticLoad(i: number, atMs: number): number {
  const seed = Math.sin(i * 12.9898) * 43758.5453;
  const base = seed - Math.floor(seed); // 0..1, stable for this machine
  const bulk =
    base < 0.68
      ? 9 + (base / 0.68) * 38
      : base < 0.89
        ? 47 + ((base - 0.68) / 0.21) * 26
        : 82 + ((base - 0.89) / 0.11) * 16;
  // Two periods, out of phase per machine, so the fleet shimmers rather than
  // breathing in step — which would read as an animation, not as telemetry.
  const drift = Math.sin(atMs / 9000 + i * 1.7) * 9 + Math.sin(atMs / 3100 + i * 0.6) * 4;
  return Math.max(1, Math.min(99, Math.round(bulk + drift)));
}

// The census `top`, `totals` and the member counts fold over, matching the
// engine's (`top_rows`): a member that left or was revoked is not a machine
// this mesh has.
const census = (n: NodeView) => !n.revoked && !n.left;

const top: TopView[] = nodes.map((n, i) => ({
  nodeId: n.nodeId,
  shortId: n.shortId,
  name: n.name,
  liveness: n.liveness,
  isSelf: n.isSelf,
  noExecute: !n.executing,
  slots: n.executing ? [16, 32, 8, 4][i] ?? 8 : 0,
  running: n.executing ? [0, 9, 2, 0][i] ?? 0 : 0,
  queued: n.executing ? [0, 3, 0, 0][i] ?? 0 : 0,
  saturationPct: n.executing ? [0, 28, 25, 0][i] ?? 0 : 0,
  cores: n.capacity?.cores ?? 0,
  loadPerCore: n.executing ? [0.04, 0.31, 0.22, 0.02][i] ?? 0.05 : null,
  memAvailableMb: n.capacity ? Math.round(n.capacity.memMb * 0.62) : null,
  // A gang holding two of these nodes, so the Held column and the
  // reservations panel are visible in the mock rather than only on a mesh
  // that happens to be running one (C4 P1).
  leases: n.executing ? [0, 1, 1, 0][i] ?? 0 : 0,
  leasedCpus: n.executing ? [0, 4, 4, 0][i] ?? 0 : 0,
  // Two GPU boxes, so the device column and the accelerator panel are
  // visible in the mock rather than only on a mesh that happens to have
  // hardware in it (C5 P7). `devicesBusy` is null off this peer, which is
  // the honest shape: a device hold is node-local.
  devices: n.executing ? [0, 4, 2, 0][i] ?? 0 : 0,
  devicesBusy: n.isSelf ? 0 : null,
  deviceBusyPct: n.executing ? [null, 82, 4, null][i] ?? null : null,
  // Live telemetry, and deliberately *not* agreeing with `loadPerCore`.
  // The pair that made this whole thing necessary is a machine at 96% CPU
  // whose one-minute load average still reads 0.31 per core, so the mock
  // has one — otherwise a surface can be built that only ever sees the two
  // signals telling the same story, which is the case that never needed
  // the second one.
  cpuPct: n.executing ? ([3, 96, 41, 1][i] ?? syntheticLoad(i, Date.now())) : 2,
  memUsedPct: n.executing ? [22, 71, 38, 9][i] ?? 30 : 14,
  diskUsedPct: n.executing ? [31, 44, 92, 12][i] ?? 40 : 20,
  busy: n.executing ? [0.22, 0.96, 0.41, 0.09][i] ?? 0.3 : 0.14,
  uptimeSecs: 3600 * ([26, 412, 91, 3][i] ?? 2),
  // A node moving real traffic, so the surface is built against one.
  netRxBps: n.executing ? [1_200_000, 431_000_000, 8_400_000, 90_000][i] ?? 500_000 : 12_000,
  netTxBps: n.executing ? [900_000, 26_000_000, 2_100_000, 40_000][i] ?? 300_000 : 8_000,
}));

// One accelerator per row, as `devices_view` returns them (C5 §9.1).
const devices: DeviceView[] = top.flatMap((t) =>
  Array.from({ length: t.devices }, (_, index) => ({
    nodeId: t.nodeId,
    nodeName: t.name,
    index,
    kind: "gpu",
    vendor: "nvidia",
    model: t.devices >= 4 ? "NVIDIA A100-SXM4-40GB" : "NVIDIA GeForce RTX 4090",
    uuid: `GPU-${t.shortId}-${index}`,
    memMb: t.devices >= 4 ? 40_960 : 24_564,
    numaNode: index < 2 ? 0 : 1,
    health: t.devices >= 4 && index === 3 ? "degraded" : "ok",
    healthReason: t.devices >= 4 && index === 3 ? "1 corrected ECC error" : null,
    heldBy: t.devices >= 4 && index < 2 ? `job ${t.shortId.slice(0, 6)}` : null,
    utilPct: t.devices >= 4 ? [97, 94, 12, 0][index] ?? 0 : [4, 0][index] ?? 0,
    memFreeMb: t.devices >= 4 ? 2_048 : 23_900,
    tempC: t.devices >= 4 ? 78 : 41,
    throttled: t.devices >= 4 && index === 0,
  })),
);

// The shape that found the identity bug: a desktop with a discrete NVIDIA
// card and the CPU's integrated AMD GPU. Both call themselves index 0, so
// this row is what keeps the vendor column — and the keyed-each — honest.
if (devices.some((d) => d.nodeName === "anvil")) {
  const anvil = top.find((t) => t.name === "anvil")!;
  devices.push({
    nodeId: anvil.nodeId,
    nodeName: anvil.name,
    index: 0,
    kind: "gpu",
    vendor: "amd",
    model: "RAPHAEL",
    uuid: "0x0",
    memMb: 512,
    numaNode: 0,
    health: "ok",
    healthReason: null,
    heldBy: null,
    utilPct: 0,
    memFreeMb: 480,
    tempC: 39,
    throttled: false,
  });
  devices.sort((a, b) =>
    a.nodeName.localeCompare(b.nodeName) ||
    a.vendor.localeCompare(b.vendor) ||
    a.index - b.index,
  );
}

// -- the mesh's spec sheet (L2 P1) -------------------------------------------
//
// A fold over the three fixtures above, in the same shape the daemon folds
// replicated state: dead nodes contribute nothing, and a node that
// advertised no capacity is excluded from the totals *and* from their
// denominator rather than averaged in as a zero.
//
// **`mesh-proto`'s `Totals::strip()` is the authority for the wording** —
// this is a transliteration of it so the guide's screenshots show the band
// the product ships. If the sentence changes there, it changes here; the
// Rust side has the test that pins the copy.
function mb(v: number): string {
  const gb = v / 1024;
  if (gb >= 1024) return `${(gb / 1024).toFixed(1)} TB`;
  if (gb >= 10) return `${gb.toFixed(0)} GB`;
  return `${gb.toFixed(1)} GB`;
}

function totals(): TotalsView {
  const live = nodes.filter((n) => census(n) && n.liveness !== "dead");
  const ids = new Set(live.map((n) => n.nodeId));
  const withCap = live.filter((n) => n.capacity);
  const sum = (f: (n: NodeView) => number) => withCap.reduce((a, n) => a + f(n), 0);
  // Schedulable devices, which is what the daemon counts: a driver marks a
  // card degraded and it still runs work; only "unavailable" is not there.
  const gpus = devices.filter((d) => ids.has(d.nodeId) && d.kind === "gpu" && d.health !== "unavailable");
  const runningTop = top.filter((t) => ids.has(t.nodeId));
  const cores = sum((n) => n.capacity!.cores);
  const memMb = sum((n) => n.capacity!.memMb);
  const diskMb = sum((n) => n.capacity!.diskMb);
  const vramMb = gpus.reduce((a, d) => a + d.memMb, 0);
  const jobsRunning = runningTop.reduce((a, t) => a + t.running, 0);
  const jobsQueued = runningTop.reduce((a, t) => a + t.queued, 0);
  const slots = runningTop.reduce((a, t) => a + t.slots, 0);
  const slotsBusy = runningTop.reduce((a, t) => a + t.running, 0);
  const arches = [...withCap.reduce((m, n) => m.set(n.capacity!.arch, (m.get(n.capacity!.arch) ?? 0) + 1), new Map<string, number>())]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])) as [string, number][];

  const parts = [`${live.length} machine${live.length === 1 ? "" : "s"}`];
  if (withCap.length) parts.push(`${cores} cores`);
  if (withCap.length) parts.push(mb(memMb));
  if (gpus.length) {
    parts.push(
      vramMb === 0
        ? `${gpus.length} GPU${gpus.length === 1 ? "" : "s"}`
        : `${gpus.length} GPU${gpus.length === 1 ? "" : "s"} (${mb(vramMb)} VRAM)`,
    );
  }
  if (withCap.length) parts.push(`${mb(diskMb)} poolable disk`);
  if (jobsRunning) parts.push(`${jobsRunning} jobs running`);

  return {
    nodesKnown: nodes.filter(census).length,
    nodesLive: live.length,
    nodesDraining: live.filter((n) => !n.executing).length,
    cores,
    coresFrom: withCap.length,
    slots,
    slotsBusy,
    saturationPct: slots ? Math.min(100, Math.round((slotsBusy / slots) * 100)) : 0,
    memMb,
    memFrom: withCap.length,
    memAvailableMb: Math.round(memMb * 0.62),
    memAvailableFrom: withCap.length,
    diskMb,
    diskFrom: withCap.length,
    diskFreeMb: Math.round(diskMb * 0.48),
    diskFreeFrom: withCap.length,
    devices: gpus.length,
    devicesBusyHere: 0,
    jobsRunning,
    jobsQueued,
    arches,
    loadPerCore: 0.19,
    gpus: gpus.length,
    vramMb,
    capacityFrom: withCap.length,
    strip: parts.join(" · "),
    coverage:
      withCap.length < live.length
        ? `${withCap.length} of ${live.length} machines reporting`
        : null,
  };
}

const bundles: BundleView[] = [
  { bundleId: "bndl-0e8aef1d1e6d8c8587d13c4d417edaea5b8323014e88783382e2ba4c7df9b0c5", shortId: "bndl-0e8aef1d1e6d", totalSize: 1315, fileCount: 5, chunkCount: 5, holders: 3, local: true },
  { bundleId: "bndl-0e54ccda49fa1b2c3d4e5f60718293a4b5c6d7e8f901234567890abcdef1234", shortId: "bndl-0e54ccda49fa", totalSize: 228, fileCount: 1, chunkCount: 1, holders: 2, local: true },
  { bundleId: "bndl-7a1c9e0d2b3f4a5c6d7e8f9012345678901234567890abcdef1234567890abcd", shortId: "bndl-7a1c9e0d2b3f", totalSize: 48_212_001, fileCount: 312, chunkCount: 1104, holders: 1, local: false },
];

// Files per bundle — what `bundle_files` returns (shape: BundleFileView).
function fileRow(path: string, size: number, executable: boolean) {
  const i = path.lastIndexOf("/");
  const dir = i >= 0 ? path.slice(0, i) : "";
  const name = i >= 0 ? path.slice(i + 1) : path;
  const ext = name.toLowerCase().split(".").pop() ?? "";
  const kinds: string[] = [];
  if (ext === "sh" || ext === "bash") kinds.push("bash");
  else if (ext === "py") kinds.push("python");
  else if (ext === "yml" || ext === "yaml") kinds.push("ansible");
  else if (ext === "json") kinds.push("contract");
  else if (["md", "txt", "toml", "cfg", "ini", "env"].includes(ext)) kinds.push("text");
  if (executable) kinds.push("exec");
  return { path, dir, name, size, executable, symlink: null, kinds };
}
const bundleFiles: Record<string, ReturnType<typeof fileRow>[]> = {};
const bundleTexts: Record<string, string> = {};
function seedBundleFiles() {
  bundleFiles[bundles[0].bundleId] = [
    fileRow("README.md", 1802, false),
    fileRow("VERSION", 6, false),
    fileRow("scripts/hello.sh", 412, true),
    fileRow("scripts/deploy.sh", 980, true),
    fileRow("scripts/fleet-report.sh", 733, true),
    fileRow("app/main.py", 512, true),
    fileRow("app/sysinfo.py", 420, true),
    fileRow("bin/tool", 150, true),
    fileRow("ansible/site.yml", 290, false),
    fileRow("contracts/deploy.schema.json", 505, false),
  ];
  // A folder added from this computer, which is the case where Director
  // knows a local path and can therefore *write* a definitions file back.
  // Modelling it matters: the save button and the "Director does not know
  // where this source lives" fallback are different screens, and only one
  // of them gets designed if the mock has no folder in it.
  bundleFiles[bundles[1].bundleId] = [
    fileRow("scripts/hello.sh", 228, true),
    fileRow("transcode-media.sh", 2356, true),
    fileRow("transcode-media.sh.definitions", 792, false),
  ];
  bundleFiles[bundles[2].bundleId] = [
    fileRow("bin/agent", 41_000_000, true),
    fileRow("bin/agentctl", 7_200_000, true),
    fileRow("etc/agent.toml", 1200, false),
    fileRow("install.sh", 3100, true),
  ];
  // A definitions file is a file in the bundle like any other, so the mock
  // lists it — the picker filtering it back out is behaviour worth seeing.
  bundleFiles[bundles[0].bundleId].push(
    fileRow("scripts/fleet-report.sh.definitions", 486, false),
  );

  bundleTexts[`${bundles[0].bundleId}:contracts/deploy.schema.json`] = CONTRACT;
  // Three scripts, three ways of saying what they take — because the wizard
  // shows a different thing for each, and a mock that models only the happy
  // one is a mock the other two get designed without.
  bundleTexts[`${bundles[0].bundleId}:scripts/hello.sh`] =
    '#!/usr/bin/env bash\necho "hello from $(hostname)"\nWHO="${IN_who:-world}"\nLOUD="${IN_loud:-false}"\n';
  bundleTexts[`${bundles[0].bundleId}:scripts/deploy.sh`] = [
    "#!/usr/bin/env bash",
    "#meshware: name deploy",
    "#meshware: about Roll one release out to a machine",
    "#meshware: item host",
    "#meshware: needs cores=1",
    '#meshware: arg host!                            "Where to deploy"',
    '#meshware: arg version!                         "Release tag, e.g. v1.2.3"',
    '#meshware: arg region : one of east|west = east "Which side of the country"',
    '#meshware: arg dry_run : bool = false           "Print what it would do"',
    "set -uo pipefail",
    "",
  ].join("\n");
  bundleTexts[`${bundles[0].bundleId}:scripts/fleet-report.sh`] =
    '#!/usr/bin/env bash\nFORMAT="${IN_format:-text}"\n';
  bundleTexts[`${bundles[1].bundleId}:transcode-media.sh`] =
    '#!/usr/bin/env bash\nPATHIN="${IN_path}"\nCRF="${IN_crf:-28}"\n';
  bundleTexts[`${bundles[1].bundleId}:transcode-media.sh.definitions`] = JSON.stringify(
    {
      name: "transcode-media",
      about: "Re-encode one media file",
      item: "path",
      needs: { cores: 2, mem: "512M" },
      tools: ["ffmpeg", "ffprobe"],
      reads: ["$path"],
      writes: ["$outdir"],
      args: [
        { name: "path", type: "text", required: true, description: "The video to encode" },
        { name: "crf", type: "int", default: 28, min: 0, max: 51, description: "Lower is bigger" },
        {
          name: "codec",
          type: "choice",
          choices: ["libx265", "libx264", "libsvtav1"],
          default: "libx265",
          description: "Video encoder",
        },
        { name: "outdir", type: "text", description: "Where finished files go" },
      ],
    },
    null,
    2,
  );
  bundleTexts[`${bundles[0].bundleId}:scripts/fleet-report.sh.definitions`] = JSON.stringify(
    {
      name: "fleet-report",
      about: "What every machine is, in one table",
      needs: { cores: 1 },
      args: [
        {
          name: "format",
          type: "choice",
          choices: ["text", "json", "csv"],
          default: "text",
          description: "How to print it",
        },
        {
          name: "top",
          type: "int",
          default: 20,
          min: 1,
          max: 500,
          description: "How many machines to list",
        },
      ],
    },
    null,
    2,
  );
}

/**
 * The mock's own reader for what a script declares. Deliberately smaller
 * than `mesh_core::script_spec` — it covers the shapes the mock's files use
 * and no more — but honest about the three answers it can give, which is
 * the part the wizard is designed against.
 */
function mockScriptSpec(source: string, definitions: string | undefined, defsPath: string) {
  type Row = {
    name: string;
    type: string;
    required: boolean;
    default?: unknown;
    description?: string;
    enum?: string[];
    minimum?: number;
    maximum?: number;
    inferred: boolean;
  };
  const rows = new Map<string, Row>();
  let name: string | null = null;
  let about = "";
  let item: string | null = null;
  let cores: number | null = null;
  // The rest of the non-argument half: carried whole, because dropping it
  // here would make the mock disagree with the engine about what a script
  // declares — which is the one thing a mock must not do.
  const extras: Record<string, unknown> = {};
  let source_ = "none";
  let error: string | null = null;

  // 1. The header.
  for (const raw of source.split("\n")) {
    const line = raw.trimStart();
    if (!line.startsWith("#meshware:")) continue;
    const rest = line.slice("#meshware:".length).trim();
    const sp = rest.search(/\s/);
    const directive = sp === -1 ? rest : rest.slice(0, sp);
    const value = sp === -1 ? "" : rest.slice(sp).trim();
    if (directive === "name") name = value;
    else if (directive === "about" || directive === "description") about = value;
    else if (directive === "item") item = value.replace(/^--/, "");
    else if (directive === "needs") {
      const m = /(?:cores|cpus)=(\d+)/.exec(value);
      if (m) cores = Number(m[1]);
    } else if (directive === "arg") {
      const q = value.indexOf('"');
      const head = (q === -1 ? value : value.slice(0, q)).trim();
      const description = q === -1 ? "" : value.slice(q + 1, value.lastIndexOf('"'));
      const [lhs, def] = head.split("=").map((x) => x?.trim());
      const [rawName, rawType] = lhs.split(":").map((x) => x?.trim());
      const required = rawName.endsWith("!");
      const argName = rawName.replace(/!$/, "").replace(/^--/, "");
      const t = (rawType ?? "text").trim();
      const choices = t.startsWith("one of")
        ? t.slice("one of".length).split("|").map((c) => c.trim()).filter(Boolean)
        : null;
      const type = choices
        ? "string"
        : t === "int" || t === "integer"
          ? "integer"
          : t === "num" || t === "number" || t === "float"
            ? "number"
            : t === "bool" || t === "boolean" || t === "yesno"
              ? "boolean"
              : "string";
      rows.set(argName, {
        name: argName,
        type,
        required,
        default: def ? mockCoerce(def, type) : undefined,
        description,
        enum: choices ?? undefined,
        inferred: false,
      });
      source_ = "header";
    }
  }
  if (name || about || item) source_ = "header";

  // 2. What the script visibly parses, for anything not declared.
  for (const raw of source.split("\n")) {
    if (raw.trimStart().startsWith("#")) continue;
    for (const m of raw.matchAll(/\$\{IN_([A-Za-z0-9_]+)(?::-([^}]*))?\}/g)) {
      const argName = m[1];
      if (rows.has(argName)) continue;
      const def = m[2];
      const type =
        def === "true" || def === "false"
          ? "boolean"
          : def !== undefined && def !== "" && Number.isFinite(Number(def))
            ? Number.isInteger(Number(def))
              ? "integer"
              : "number"
            : "string";
      rows.set(argName, {
        name: argName,
        type,
        required: false,
        default: def ? mockCoerce(def, type) : undefined,
        inferred: true,
      });
      if (source_ === "none") source_ = "inferred";
    }
  }

  // 3. The definitions file, which wins.
  if (definitions !== undefined && definitions.trim()) {
    try {
      const v: unknown = JSON.parse(definitions);
      if (typeof v !== "object" || v === null || Array.isArray(v)) {
        throw new Error("the top level must be a JSON object");
      }
      const d = v as Record<string, unknown>;
      if (typeof d.name === "string") name = d.name;
      if (typeof d.about === "string") about = d.about;
      if (typeof d.item === "string") item = d.item;
      const needs = (d.needs ?? {}) as Record<string, unknown>;
      if (typeof needs.cores === "number") cores = needs.cores;
      for (const k of ["mem", "disk"]) if (needs[k] !== undefined) extras[k] = needs[k];
      for (const k of ["tools", "reads", "writes"]) if (d[k] !== undefined) extras[k] = d[k];
      const list: Record<string, unknown>[] = Array.isArray(d.args)
        ? (d.args as Record<string, unknown>[])
        : Object.entries((d.args ?? {}) as Record<string, Record<string, unknown>>).map(
            ([k, o]) => ({ ...o, name: k }) as Record<string, unknown>,
          );
      for (const a of list) {
        const argName = String(a.name ?? "");
        if (!argName) throw new Error('an argument has no "name"');
        const choices = Array.isArray(a.choices) ? (a.choices as string[]).map(String) : null;
        const t = String(a.type ?? (choices ? "choice" : typeof a.default === "boolean" ? "bool" : typeof a.default === "number" ? "int" : "text"));
        const type = choices
          ? "string"
          : t === "int" || t === "integer"
            ? "integer"
            : t === "number" || t === "num" || t === "float"
              ? "number"
              : t === "bool" || t === "boolean"
                ? "boolean"
                : "string";
        rows.set(argName, {
          name: argName,
          type,
          required: a.required === true,
          default: a.default,
          description: typeof a.description === "string" ? a.description : undefined,
          enum: choices ?? undefined,
          minimum: typeof a.min === "number" ? a.min : undefined,
          maximum: typeof a.max === "number" ? a.max : undefined,
          inferred: false,
        });
      }
      if (rows.size || name || about || item) source_ = "definitions";
    } catch (e) {
      error = `${defsPath}: ${e instanceof Error ? e.message : String(e)}`;
    }
  }

  const args = [...rows.values()].sort((a, b) => a.name.localeCompare(b.name));
  const properties: Record<string, unknown> = {};
  const required: string[] = [];
  for (const a of args) {
    const p: Record<string, unknown> = { type: a.type };
    if (a.description) p.description = a.description;
    if (a.enum) p.enum = a.enum;
    if (a.default !== undefined) p.default = a.default;
    if (a.minimum !== undefined) p.minimum = a.minimum;
    if (a.maximum !== undefined) p.maximum = a.maximum;
    properties[a.name] = p;
    if (a.required) required.push(a.name);
  }
  const ext: Record<string, unknown> = { ...extras };
  if (item) ext.item = item;
  if (cores !== null) ext.cores = cores;
  const schema: Record<string, unknown> = { type: "object" };
  if (about) schema.description = about;
  schema.properties = properties;
  if (required.length) schema.required = required;
  schema.additionalProperties = false;
  if (Object.keys(ext).length) schema["x-meshware"] = ext;

  const asDefinitions: Record<string, unknown> = {};
  if (name) asDefinitions.name = name;
  if (about) asDefinitions.about = about;
  if (item) asDefinitions.item = item;
  const needsOut: Record<string, unknown> = {};
  if (cores !== null) needsOut.cores = cores;
  for (const k of ["mem", "disk"]) if (extras[k] !== undefined) needsOut[k] = extras[k];
  if (Object.keys(needsOut).length) asDefinitions.needs = needsOut;
  for (const k of ["tools", "reads", "writes"]) if (extras[k] !== undefined) asDefinitions[k] = extras[k];
  asDefinitions.args = args.map((a) => {
    const o: Record<string, unknown> = {
      name: a.name,
      type: a.enum
        ? "choice"
        : a.type === "integer"
          ? "int"
          : a.type === "boolean"
            ? "bool"
            : a.type === "number"
              ? "number"
              : "text",
    };
    if (a.enum) o.choices = a.enum;
    if (a.required) o.required = true;
    if (a.default !== undefined) o.default = a.default;
    if (a.minimum !== undefined) o.min = a.minimum;
    if (a.maximum !== undefined) o.max = a.maximum;
    if (a.description) o.description = a.description;
    return o;
  });

  return {
    name,
    about,
    item,
    declared: source_ === "definitions" || source_ === "header",
    source: source_,
    inferredCount: args.filter((a) => a.inferred).length,
    argCount: args.length,
    contract: args.length ? JSON.stringify(schema, null, 2) : null,
    definitions:
      error && definitions !== undefined
        ? definitions
        : `${JSON.stringify(asDefinitions, null, 2)}\n`,
    hasDefinitions: definitions !== undefined,
    definitionsPath: defsPath,
    error,
  };
}

function mockCoerce(raw: string, type: string): unknown {
  if (type === "integer" || type === "number") {
    const n = Number(raw);
    return Number.isFinite(n) ? n : raw;
  }
  if (type === "boolean") return raw === "true" || raw === "yes" || raw === "1";
  return raw;
}

const CONTRACT = JSON.stringify(
  {
    type: "object",
    properties: {
      version: { type: "string", pattern: "^v[0-9]+\\.[0-9]+\\.[0-9]+$", description: "release tag, e.g. v1.2.3" },
      replicas: { type: "integer", minimum: 1, maximum: 50, default: 1 },
      dry_run: { type: "boolean", default: false },
      region: { type: "string", enum: ["east", "west"] },
    },
    required: ["version"],
    additionalProperties: false,
  },
  null,
  2,
);

seedBundleFiles();

const services: ServiceView[] = [
  { name: "deploy", runtime: "bash", version: 1, bundleId: bundles[0].bundleId, bundleShort: bundles[0].shortId, workingDir: "scripts", entrypoint: "deploy.sh", description: "roll a release to the edge fleet", tags: ["team=platform"], versions: [1], hasContract: true, contract: CONTRACT },
  { name: "hello", runtime: "bash", version: 3, bundleId: bundles[1].bundleId, bundleShort: bundles[1].shortId, workingDir: "scripts", entrypoint: "hello.sh", description: "bash hello", tags: [], versions: [1, 2, 3], hasContract: false, contract: null },
  { name: "pyhello", runtime: "python", version: 1, bundleId: bundles[0].bundleId, bundleShort: bundles[0].shortId, workingDir: "app", entrypoint: "main.py", description: "", tags: [], versions: [1], hasContract: false, contract: null },
  { name: "site", runtime: "ansible", version: 1, bundleId: bundles[0].bundleId, bundleShort: bundles[0].shortId, workingDir: "ansible", entrypoint: "site.yml", description: "", tags: [], versions: [1], hasContract: false, contract: null },
];

const sources: SourceView[] = [
  { name: "ops-scripts", url: "git@github.com:meshware/ops-scripts.git", branch: "main", intervalSecs: 300, bump: ["hello", "deploy"], createdByName: "neuralpunk", lastCommit: "b7405ad9981f0df69c1c5c1b0096e6a6f8f0e1c2", lastBundle: bundles[1].bundleId, lastBundleShort: bundles[1].shortId, lastSyncedAtMicros: now() - 240e6, watcher: "anvil" },
];

let secrets: SecretView[] = [
  { name: "db-pass", version: 2, createdByName: "neuralpunk", createdAtMicros: now() - 3600e6, wrappedFor: 5, members: 5, removed: false },
  { name: "tls-key", version: 1, createdByName: "neuralpunk", createdAtMicros: now() - 2 * 86400e6, wrappedFor: 4, members: 5, removed: false },
];

let users: UserView[] = [
  { userId: FOUNDER, shortId: "mwu1zmqqlw5kdeas", username: "neuralpunk", isFounder: true, revoked: false, roles: ["admin"], createdAtMicros: now() - 7 * 86400e6, hardware: null },
  { userId: rid("mwu1", 52), shortId: "mwu17bi6y4ptjadh", username: "lab-op", isFounder: false, revoked: false, roles: ["operator"], createdAtMicros: now() - 2 * 86400e6, hardware: null },
  // Y2/Y3: one person whose key is proven to be on a security key, so the
  // badge and its withdraw are there to look at.
  {
    userId: rid("mwu1", 52), shortId: "mwu1c4n9r2kx8pq", username: "mia", isFounder: false, revoked: false, roles: ["developer", "auditor"], createdAtMicros: now() - 86400e6,
    hardware: { kind: "piv", slot: "9c", rootLabel: "Yubico PIV Attestation CA", subject: "CN=YubiKey PIV Attestation 9c, serialNumber=23847312", attestedAtMicros: now() - 20 * 3600e6, notAfterMicros: now() + 3650 * 86400e6, revoked: false, revokedReason: "" },
  },
  { userId: rid("mwu1", 52), shortId: "mwu1h8j3w7lv5tz", username: "contractor", isFounder: false, revoked: true, roles: [], createdAtMicros: now() - 30 * 86400e6, hardware: null },
];

// -- Y2/Y3 security keys -----------------------------------------------------
//
// The agent holds an ordinary SSH key from ~/.ssh — which the card must name
// as a file, and not offer — and, once connected, a YubiKey's PIV key.
//
//   ?agent=0      no agent to ask
//   ?yubikey=0    the YubiKey isn't connected yet: the setup starts at
//                 "put a key on your YubiKey"; PIN 123456 connects it
//   ?asks=1       the key asks for its PIN on every signature
//   ?onfile=1     already moved onto the key file (the 2026-09-18 state)
//   ?keyout=1     moved onto the YubiKey, which is unplugged at start
//   ?keypause=1   the first refresh finds the key refusing
const YUBIKEY_ID = "mwu1ykp4v9c2x7qhdn3m8rjt5w6ksbzfgl4a2e9uyc7xhq3vnd5mza";
const FILE_KEY_ID = "mwu1h4tz5uh356r7mfn2mssjnmklr5bfc6u5nig6motj4i5hjfw6c7ma";
const mockQ = new URLSearchParams(typeof location === "undefined" ? "" : location.search);
let yubikeyLoaded = mockQ.get("yubikey") !== "0";
const agentKeys = (): AgentKeyView[] => [
  ...(yubikeyLoaded
    ? [{ userId: YUBIKEY_ID, shortId: YUBIKEY_ID.slice(0, 16), comment: "PIV AUTH key", current: false, origin: "device", file: null }]
    : []),
  { userId: FILE_KEY_ID, shortId: FILE_KEY_ID.slice(0, 16), comment: "neuralpunk@NIM", current: false, origin: "file", file: "~/.ssh/meshware" },
];
let keyOn: string | null = mockQ.has("onfile")
  ? FILE_KEY_ID
  : mockQ.has("keyout") || mockQ.has("keypause")
    ? YUBIKEY_ID
    : null;
if (mockQ.has("keyout")) yubikeyLoaded = false;
let spareKept = keyOn !== null;
let mockKeyPaused = mockQ.has("keypause");
let attestRoots: AttestRootView[] = [
  { id: "9f3c2a7e5b1d4c6f8a0e2d4b6c8a0e2d4f6b8c0a2e4d6f8b0c2a4e6d8f0b2c4a", shortId: "9f3c2a7e5b1d", label: "Yubico PIV Attestation CA", addedBy: "neuralpunk", addedAtMicros: now() - 21 * 86400e6, removed: false, attesting: 1 },
];

const grants: GrantView[] = [
  { grantId: "grant-w7henzv3akay4", userId: FOUNDER, username: "neuralpunk", role: "admin", nodes: "any", services: "*", expiresAtMicros: null, revoked: false, expired: false, createdAtMicros: now() - 7 * 86400e6 },
  { grantId: "grant-mele4nkyoficw", userId: users[1].userId, username: "lab-op", role: "operator", nodes: "tag=role=lab", services: "hel*", expiresAtMicros: now() + 20 * 86400e6, revoked: false, expired: false, createdAtMicros: now() - 2 * 86400e6 },
  { grantId: "grant-q2x8v5n1bdj7e", userId: users[2].userId, username: "mia", role: "developer", nodes: "any", services: "*", expiresAtMicros: null, revoked: false, expired: false, createdAtMicros: now() - 86400e6 },
  { grantId: "grant-a9c3t6y2pf4km", userId: users[2].userId, username: "mia", role: "auditor", nodes: "any", services: "*", expiresAtMicros: null, revoked: false, expired: false, createdAtMicros: now() - 86400e6 },
  { grantId: "grant-z1k5m9b3rh7wn", userId: users[3].userId, username: "contractor", role: "operator", nodes: "tag=role=edge", services: "deploy", expiresAtMicros: now() - 3 * 86400e6, revoked: true, expired: true, createdAtMicros: now() - 30 * 86400e6 },
];

const jobs: JobView[] = [];

/** See the `state_versions_view` case: the mock reports constant change. */
let mockTick = 0;
const MOCK_TOPICS = [
  "nodes", "jobs", "runs", "workflows", "services", "bundles", "sources",
  "users", "grants", "secrets", "policies", "approvals", "enrolments",
  "hosts", "schedules", "deployments", "facts", "settings", "releases",
  "background",
];
function addJob(partial: Partial<JobView> & { cmdPreview: string }): JobView {
  const id = rid("job-", 26);
  const j: JobView = {
    jobId: id,
    shortId: id.slice(0, 14),
    runtime: "bash",
    state: "succeeded",
    exitCode: 0,
    executorName: "forge",
    submitterUsername: "neuralpunk",
    createdAtMicros: now(),
    finishedAtMicros: now(),
    fanGroup: null,
    fanIndex: null,
    fanTotal: null,
    // M31 A2: where this job's output is. Available is the ordinary case;
    // the fixture below includes an expired one, because "the log is gone
    // and the record is not" is a state worth designing against.
    outputState: "available",
    outputMessage: "",
    ...partial,
  };
  jobs.unshift(j);
  return j;
}
// history
addJob({ cmdPreview: "deploy (bash/deploy.sh)", createdAtMicros: now() - 40e6, finishedAtMicros: now() - 38e6 });
addJob({ cmdPreview: "apt list --upgradable", state: "failed", exitCode: 100, executorName: "edge-7", submitterUsername: "lab-op", createdAtMicros: now() - 600e6, finishedAtMicros: now() - 598e6 });
addJob({ cmdPreview: "sleep 10", state: "timed-out", exitCode: null, executorName: "anvil", createdAtMicros: now() - 900e6, finishedAtMicros: now() - 890e6 });
addJob({ cmdPreview: "pyhello (python/main.py)", runtime: "python", createdAtMicros: now() - 1800e6, finishedAtMicros: now() - 1790e6 });
// A2: a job whose record is complete and whose log has aged out. It
// succeeded, and it still says so — which is the whole point.
addJob({
  cmdPreview: "nightly-backup (bash/backup.sh)",
  createdAtMicros: now() - 95 * 86400e6,
  finishedAtMicros: now() - 95 * 86400e6 + 4e6,
  outputState: "expired",
  outputMessage:
    "output expired 2026-06-07 — the node that ran it keeps job output for a limited time (`[retention]`). The record below is complete.",
});
const fan = rid("fan-", 26);
for (let i = 0; i < 3; i++)
  addJob({ cmdPreview: "systemctl is-active nginx", executorName: ["forge", "anvil", "edge-7"][i], state: i === 2 ? "running" : "succeeded", exitCode: i === 2 ? null : 0, finishedAtMicros: i === 2 ? null : now() - 5e6, createdAtMicros: now() - 8e6, fanGroup: fan, fanIndex: i, fanTotal: 3 });

// `?fresh`: a mesh in its first minute (UX-10) — no scripts, nothing run
// yet — so the Get started card has something to guide. With `peers=0` it
// is a mesh of one, which is what "Create mesh" leaves behind.
// `?fresh=jobs` keeps the scripts, for the card's last step.
const FRESH = new URLSearchParams(location.search).get("fresh");
if (FRESH !== null) {
  if (FRESH !== "jobs") services.splice(0);
  jobs.splice(0);
}

const audit: AuditView[] = [
  { auditId: rid("audit-", 20), atMicros: now() - 40e6, userId: FOUNDER, username: "neuralpunk", action: "job.run", target: jobs[jobs.length - 1]?.jobId ?? "job-…", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "deploy (bash/deploy.sh) on forge" },
  { auditId: rid("audit-", 20), atMicros: now() - 120e6, userId: users[1].userId, username: "lab-op", action: "service.create", target: "x", nodeShort: "mw1sdj5gterlfav", outcome: "denied: requires one of: admin, developer", detail: "" },
  { auditId: rid("audit-", 20), atMicros: now() - 240e6, userId: FOUNDER, username: "neuralpunk", action: "source.sync", target: "ops-scripts", nodeShort: "mw1b2n8e5ry7wqa", outcome: "ok", detail: "b7405ad9981f → bndl-0e54ccda49fa, bumped hello→v3" },
  { auditId: rid("audit-", 20), atMicros: now() - 3600e6, userId: FOUNDER, username: "neuralpunk", action: "secret.set", target: "db-pass", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "v2 sealed for 5 node(s)" },
  { auditId: rid("audit-", 20), atMicros: now() - 86400e6, userId: FOUNDER, username: "neuralpunk", action: "user.add", target: users[2].userId, nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "mia" },
  // Deployment history, so a deployment's Activity has a story to tell
  // (2026-09-14): who made each one, what changed, and what the engine
  // recorded when it could not keep up. The details are the shapes the
  // daemon writes — `generation N`, and `generation N slot S: why`.
  { auditId: rid("audit-", 20), atMicros: now() - 12 * 86400e6, userId: FOUNDER, username: "neuralpunk", action: "deployment.create", target: "edge-collector", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "generation 0" },
  { auditId: rid("audit-", 20), atMicros: now() - 9 * 86400e6, userId: FOUNDER, username: "neuralpunk", action: "deployment.put", target: "edge-collector", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "generation 4" },
  { auditId: rid("audit-", 20), atMicros: now() - 30 * 86400e6, userId: FOUNDER, username: "neuralpunk", action: "deployment.create", target: "metrics-relay", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "generation 0" },
  { auditId: rid("audit-", 20), atMicros: now() - 150e6, userId: FOUNDER, username: "neuralpunk", action: "deployment.knobs", target: "metrics-relay", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "auto_promote = false" },
  { auditId: rid("audit-", 20), atMicros: now() - 145e6, userId: FOUNDER, username: "neuralpunk", action: "deployment.put", target: "metrics-relay", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "generation 9" },
  { auditId: rid("audit-", 20), atMicros: now() - 3 * 86400e6, userId: users[1].userId, username: "lab-op", action: "deployment.create", target: "log-shipper", nodeShort: "mw1b2n8e5ry7wqa", outcome: "ok", detail: "generation 0" },
  { auditId: rid("audit-", 20), atMicros: now() - 2 * 86400e6, userId: users[1].userId, username: "lab-op", action: "deployment.put", target: "log-shipper", nodeShort: "mw1b2n8e5ry7wqa", outcome: "ok", detail: "generation 3" },
  { auditId: rid("audit-", 20), atMicros: now() - 11 * 60e6, userId: users[1].userId, username: "lab-op", action: "deployment.degraded", target: "log-shipper", nodeShort: "mw1sdj5gterlfav", outcome: "degraded", detail: "generation 3 restart budget exhausted: 5 failures in 10m on edge-7" },
  { auditId: rid("audit-", 20), atMicros: now() - 7 * 60e6, userId: users[1].userId, username: "lab-op", action: "deployment.stalled", target: "log-shipper", nodeShort: "mw1sdj5gterlfav", outcome: "stalled", detail: "generation 3 no progress for 10m — 3 of 4 healthy since 09:14 (version 3)" },
  { auditId: rid("audit-", 20), atMicros: now() - 6 * 60e6, userId: users[1].userId, username: "lab-op", action: "deployment.unplaced", target: "log-shipper", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "generation 3 slot edge-7/3: edge-7 is past its restart budget and no other machine has room" },
  { auditId: rid("audit-", 20), atMicros: now() - 6 * 86400e6, userId: FOUNDER, username: "neuralpunk", action: "deployment.create", target: "webapp", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "generation 0" },
  { auditId: rid("audit-", 20), atMicros: now() - 4 * 86400e6, userId: FOUNDER, username: "neuralpunk", action: "deployment.put", target: "webapp", nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: "generation 2" },
];

// ── C0–C9: the cluster milestones ───────────────────────────────────────────
//
// Deployments, arbitration, gangs, the action cache and the placement
// breakdown. Each is seeded mid-story rather than idle, because the empty
// state of every one of these screens is a screen with nothing to look at,
// and the guide photographs them.

// -- C2 deployments ---------------------------------------------------------
// `DeploymentStatusView` minus the list-only fields, so TypeScript checks
// the fixture against what the engine actually returns.
//
// It used to be `DeploymentView & {…}` with a cast at the call site, and
// the cast was the bug: N1 and N2 appended `canarying`, `stalledReason`,
// `policyBlind`, `portLabels` and `placement`, the fixture did not grow
// them, and the cast said nothing. The panel reads `status.portLabels
// .length` — so opening any deployment row in mock mode threw
// `Cannot read properties of undefined`.
type Dep = DeploymentView & Omit<DeploymentStatusView, keyof DeploymentView>;

function inst(slot: string, node: string, generation: number, state: string, health: string, healthDetail: string, agoMicros: number | null): DeploymentInstanceView {
  const id = rid("job-", 26);
  return { jobId: id, slot, node, generation, state, health, healthDetail, startedAtMicros: agoMicros === null ? null : now() - agoMicros };
}

/// A deliberately dumb TOML reader for the spec editors.
///
/// It exists to make Plan → Publish feel real in the browser, not to be a
/// parser: it pulls `key = "value"` pairs out of whatever is pasted and
/// ignores the structure. The engine has the real one, and the mock's job
/// is the *flow*.
function parseSpec(src: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#") || line.startsWith("[") || line.startsWith("-")) continue;
    // **Both spellings.** It read only `key = value` and the builder writes
    // YAML, so every plan came back claiming the file said nothing at all —
    // a mock that disagreed with the engine about a file the engine parses
    // fine, which is worse than no mock.
    const eq = line.indexOf("=");
    const colon = line.indexOf(":");
    const at =
      eq >= 0 && colon >= 0 ? Math.min(eq, colon) : eq >= 0 ? eq : colon;
    if (at < 0) continue;
    const k = line.slice(0, at).trim();
    const v = line
      .slice(at + 1)
      .trim()
      .replace(/^["']|["']$/g, "");
    // A bare `container:` or `network:` is a block header, not a value.
    if (k && v && !(k in out)) out[k] = v;
  }
  return out;
}

function DEPLOYMENT_EXAMPLE(d?: { name: string; service: string; count: string; targets: string }): string {
  return `[deployment]\nname    = "${d?.name ?? "webapp"}"\nservice = "${d?.service ?? "webapp@3"}"\ncount   = ${JSON.stringify(d?.count ?? "3")}\ntargets = "${d?.targets ?? "any"}"\n`;
}

// ---------------------------------------------------------------------------
// The N-track: schedules, ports, the registry, volumes, the shed
// ---------------------------------------------------------------------------
//
// Added because none of it was here. Every screen N1–N6 grew — Schedules,
// Services, Mesh › Ports, Mesh › Volumes, Settings › Shed — threw
// `mock: unhandled command` the moment it loaded, so the whole dev and
// demo path for six milestones of work was dark. The fixtures below are
// chosen to show the *interesting* state of each screen rather than the
// empty one: a schedule that has quietly stopped firing, a port range
// filling up, a service with a dead instance, a volume somebody is
// writing to.

let schedules: ServiceScheduleView[] = [
  {
    name: "nightly-backup", service: "backup@4", targets: "tag=role=lab",
    when: "03:00 mon-fri", status: "active",
    nextMicros: now() + 7.5 * 3600e6, lastMicros: now() - 16.5 * 3600e6,
    firer: "forge", createdBy: "neuralpunk", createdAtMicros: now() - 40 * 86400e6,
    source: `[schedule]\nname    = "nightly-backup"\nservice = "backup@4"\nat      = "03:00"\ndays    = "mon-fri"\ntargets = "tag=role=lab"\n`,
  },
  {
    name: "hourly-rollup", service: "rollup@2", targets: "any",
    when: "every hour", status: "paused",
    nextMicros: now() + 22 * 60e6, lastMicros: now() - 38 * 60e6,
    firer: "anvil", createdBy: "lab-op", createdAtMicros: now() - 9 * 86400e6,
    source: `[schedule]\nname    = "hourly-rollup"\nservice = "rollup@2"\nevery   = "1h"\ntargets = "any"\n`,
  },
  {
    // The failure this screen exists to make visible: it is not paused, it
    // is not active, and it has quietly done nothing for three weeks
    // because the service it names was retired.
    name: "weekly-report", service: "reporter@1", targets: "any",
    when: "09:00 mon", status: "orphaned",
    nextMicros: null, lastMicros: now() - 21 * 86400e6,
    firer: null, createdBy: "mia", createdAtMicros: now() - 120 * 86400e6,
    source: `[schedule]\nname    = "weekly-report"\nservice = "reporter@1"\nat      = "09:00"\ndays    = "mon"\n`,
  },
];

// T4. Deliberately *not* a mesh where everything works: the demo's whole
// job is to show the three states apart, because telling them apart is the
// panel's reason for existing. `director` has podman and no ansible;
// `edge-2` announces nothing, which is the pre-1.26 case the "unknown is
// neutral" rule exists for.
const runtimesView: RuntimesViewT = {
  node: "director",
  local: [
    { runtime: "exec", detection: "ready", detail: "the kernel", remedy: "" },
    { runtime: "bash", detection: "ready", detail: "GNU bash, version 5.2.32", remedy: "" },
    { runtime: "python", detection: "ready", detail: "Python 3.13.1", remedy: "" },
    {
      runtime: "ansible",
      detection: "absent",
      detail: "",
      remedy:
        "install ansible (`dnf install ansible`, `pipx install ansible`) — or pin it in a bundle's requirements.txt, which this node builds into a venv per service",
    },
    {
      runtime: "container",
      detection: "ready",
      detail: "podman 5.8.4 (rootless)",
      remedy: "",
    },
  ],
  mesh: [
    { node: "director", runtimes: ["exec", "bash", "python", "container"] },
    { node: "edge-1", runtimes: ["exec", "bash"] },
    { node: "edge-2", runtimes: null },
    { node: "gpu-1", runtimes: ["exec", "bash", "python", "ansible", "container"] },
  ],
  unknown: ["edge-2"],
};

const portLedger: PortLedgerViewT = {
  node: "director",
  range: "20000-32000", rangeMin: 20000, rangeMax: 32000,
  reserved: [22, 443, 5432],
  // 12001 in the range, four held. The headline divides these, so a figure
  // that does not subtract cleanly reads as an off-by-one in the engine.
  free: 11_997,
  held: [
    { port: 9095, label: "http", holder: "metrics-relay/0", kind: "job" },
    { port: 8080, label: "admin", holder: "metrics-relay/0", kind: "job" },
    { port: 20001, label: "http", holder: "job-agqjnadh7o", kind: "job" },
    { port: 20014, label: "grpc", holder: "gang-4f2a", kind: "gang" },
  ],
};

const registryRows: RegistryRowView[] = [
  { service: "webapp", healthy: 3, total: 3, nodes: 3, tags: ["prod", "http"], fqdn: "webapp.mesh" },
  // The row the whole phase exists for: Nomad's catalogue has no health
  // field, so this and the one above would look identical there.
  // `nodes` counts every node carrying a registration, healthy or not
  // (`registry::rows`), and `healthy` counts the *resolvable* ones — which
  // `starting` is not. Both are computed that way in the engine, so the
  // fixture matches rather than merely looking plausible.
  { service: "metrics-relay", healthy: 2, total: 3, nodes: 3, tags: ["lab"], fqdn: "metrics-relay.mesh" },
  { service: "backup", healthy: 0, total: 1, nodes: 1, tags: [], fqdn: "backup.mesh" },
];

const registryInstances: Record<string, RegistryInstanceView[]> = {
  webapp: [
    { instance: "webapp-0", node: "forge", address: "192.168.1.31", port: 20001, label: "http", health: "passing", sinceMicros: now() - 4 * 86400e6, tags: ["prod"], deployment: "webapp", resolvable: true },
    { instance: "webapp-1", node: "anvil", address: "192.168.1.32", port: 20007, label: "http", health: "passing", sinceMicros: now() - 4 * 86400e6, tags: ["prod"], deployment: "webapp", resolvable: true },
    { instance: "webapp-2", node: "edge-7", address: "203.0.113.5", port: 20002, label: "http", health: "passing", sinceMicros: now() - 90 * 60e6, tags: ["prod"], deployment: "webapp", resolvable: true },
  ],
  "metrics-relay": [
    { instance: "relay-0", node: "forge", address: "192.168.1.31", port: 9095, label: "http", health: "passing", sinceMicros: now() - 140e6, tags: ["lab"], deployment: "metrics-relay", resolvable: true },
    { instance: "relay-1", node: "anvil", address: "192.168.1.32", port: 9095, label: "http", health: "passing", sinceMicros: now() - 6 * 86400e6, tags: ["lab"], deployment: "metrics-relay", resolvable: true },
    // Critical, so discovery withholds it: it is registered and it will
    // not be handed out, which is exactly the state a catalogue without a
    // health field cannot express.
    { instance: "relay-2", node: "edge-7", address: "203.0.113.5", port: 9095, label: "http", health: "critical", sinceMicros: now() - 400e6, tags: ["lab"], deployment: "metrics-relay", resolvable: false },
  ],
  backup: [
    { instance: "backup-0", node: "forge", address: "192.168.1.31", port: 20044, label: "http", health: "starting", sinceMicros: now() - 12e6, tags: [], deployment: "", resolvable: false },
  ],
};

/**
 * A registration goes when the work behind it stops (2026-09-14). The mock
 * drops the copies a removed deployment or a cancelled job registered — a
 * moment later, as the engine does on its next registry tick — and a name
 * with none left goes with them.
 */
function dropRegistered(match: (i: RegistryInstanceView) => boolean) {
  setTimeout(() => {
    for (const [service, list] of Object.entries(registryInstances)) {
      const kept = list.filter((i) => !match(i));
      if (kept.length === list.length) continue;
      registryInstances[service] = kept;
      const at = registryRows.findIndex((r) => r.service === service);
      if (at < 0) continue;
      if (kept.length === 0) registryRows.splice(at, 1);
      else
        Object.assign(registryRows[at], {
          total: kept.length,
          healthy: kept.filter((i) => i.resolvable).length,
          nodes: new Set(kept.map((i) => i.node)).size,
        });
    }
  }, 2500);
}

const dnsStatus: DnsStatusViewT = {
  enabled: true, bound: "127.0.0.1:8600", port: 8600,
  privileged: false, suffix: "mesh", names: 3,
  detail: "",
  resolverRouted: false,
};

let volumes: VolumeRowView[] = [
  { name: "data", node: "director", path: "/srv/meshware/data", readOnly: false, source: "static", holders: ["job-agqjnadh7o"], writeHeld: true, writableNow: false },
  { name: "models", node: "director", path: "/srv/models", readOnly: true, source: "static", holders: ["job-6tp2xrcf4r", "job-ba4tdvosmr"], writeHeld: false, writableNow: false },
  { name: "scratch", node: "director", path: "/srv/meshware/volumes/scratch", readOnly: false, source: "meshware-mkdir", holders: [], writeHeld: false, writableNow: true },
];

let shed: ShedView = {
  submissions: false, reconcile: false, reason: "",
  byName: null, atMicros: 0,
  members: 5, honouring: 5, behind: [], warning: null,
};

/** Tags set in this session, by deployment (E1). */
const mockTags: Record<string, { tag: string; generation: number; description: string }[]> = {};

let deployments: Dep[] = [
  {
    name: "edge-collector", service: "collector@7", count: "system", targets: "tag=role=edge",
    generation: 4, status: "active", desired: 3, running: 3, healthy: 3,
    createdBy: "neuralpunk", createdAtMicros: now() - 12 * 86400e6, source: null,
    reconciler: "forge", updatingFrom: null, degradedReason: null,
    canarying: false, awaitingPromotion: false, stalledReason: null, policyBlind: [], portLabels: [], placement: null, unreachable: 0, runtime: "container", availability: "One instance per matching node: a node that goes away takes its instance with it, and a node that joins gets one.",
    instances: [
      inst("edge-7/0", "edge-7", 4, "running", "healthy", "http 200 in 6ms", 9 * 86400e6),
      inst("anvil/0", "anvil", 4, "running", "healthy", "http 200 in 3ms", 9 * 86400e6),
      inst("forge/0", "forge", 4, "running", "healthy", "http 200 in 2ms", 4 * 86400e6),
    ],
  },
  {
    name: "metrics-relay", service: "relay@9", count: "2", targets: "tag=role=lab",
    generation: 9, status: "updating", desired: 2, running: 3, healthy: 2,
    createdBy: "neuralpunk", createdAtMicros: now() - 30 * 86400e6, source: null,
    reconciler: "forge", updatingFrom: 8, degradedReason: null,
    // Mid-canary with a port request, which is the N1+N2 case worth having
    // on screen: the new instance is up beside the old ones and nothing has
    // been cut over yet.
    canarying: true, awaitingPromotion: true, stalledReason: null, policyBlind: [],
    portLabels: ["http", "admin"], placement: null, unreachable: 0, runtime: "container", availability: "If a node running an instance goes away, the instance is replaced on another node after 1 min unreachable.",
    // Surge-then-cutover (C2 §6.5 amendment 5): the new instance is up and
    // passing health while its predecessor still serves. Availability never
    // dips — `max_unavailable` bounds how many slots may be mid-roll, not
    // how many are down.
    instances: [
      inst("forge/0", "forge", 9, "running", "healthy", "tcp 9095 open", 140e6),
      inst("forge/0", "forge", 8, "running", "healthy", "tcp 9095 open", 6 * 86400e6),
      inst("anvil/1", "anvil", 8, "running", "healthy", "tcp 9095 open", 6 * 86400e6),
    ],
  },
  {
    name: "log-shipper", service: "shipper@3", count: "4", targets: "any",
    generation: 3, status: "degraded", desired: 4, running: 3, healthy: 3,
    createdBy: "lab-op", createdAtMicros: now() - 3 * 86400e6, source: null,
    reconciler: "forge", updatingFrom: null,
    degradedReason: "restart budget exhausted: 5 failures in 10m on edge-7",
    canarying: false, awaitingPromotion: false,
    stalledReason: "no progress for 10m — 3 of 4 healthy since 09:14",
    // A half-upgraded mesh silently rolls without a canary. N1 P3's whole
    // point is that this is *said* rather than discovered.
    policyBlind: ["edge-7"], portLabels: [], placement: null, unreachable: 0, runtime: "container", availability: "If a node running an instance goes away, the instance is replaced on another node after 1 min unreachable.",
    instances: [
      inst("forge/0", "forge", 3, "running", "healthy", "exit 0 in 40ms", 3 * 86400e6),
      inst("anvil/1", "anvil", 3, "running", "healthy", "exit 0 in 51ms", 3 * 86400e6),
      inst("director/2", "director", 3, "running", "healthy", "exit 0 in 38ms", 3 * 86400e6),
      inst("edge-7/3", "edge-7", 3, "failed", "unhealthy", "exit 1 · backing off, budget spent", 420e6),
    ],
  },
  // A plain, healthy, fixed-size deployment with a port (2026-09-14): the
  // one to Scale in a demo, because nothing else is going on in it — and the
  // deployment behind Load balancing's `webapp`, which named a deployment
  // the fixture did not have. Slots are numbered, as the engine numbers them.
  {
    name: "webapp", service: "webapp@4", count: "3", targets: "any",
    generation: 2, status: "active", desired: 3, running: 3, healthy: 3,
    createdBy: "neuralpunk", createdAtMicros: now() - 6 * 86400e6, source: null,
    reconciler: "forge", updatingFrom: null, degradedReason: null,
    canarying: false, awaitingPromotion: false, stalledReason: null, policyBlind: [],
    portLabels: ["http"], placement: null, unreachable: 0, runtime: "container", availability: "If a node running an instance goes away, the instance is replaced on another node after 1 min unreachable.",
    instances: [
      inst("0", "forge", 2, "running", "healthy", "http 200 in 3ms", 4 * 86400e6),
      inst("1", "anvil", 2, "running", "healthy", "http 200 in 5ms", 4 * 86400e6),
      inst("2", "edge-7", 2, "running", "healthy", "http 200 in 9ms", 90 * 60e6),
    ],
  },
];

/**
 * Walk a copy the mock was just asked for along the path the engine reports
 * (2026-09-14): not placed, placed and starting, running and checking, then
 * healthy. The deployment sheet exists to make that walk visible after a
 * Scale, so the fixture has to take it rather than jump to the end.
 */
function walkCopy(d: Dep, row: DeploymentInstanceView, node: string) {
  const step = (ms: number, apply: () => void) =>
    setTimeout(() => {
      if (!deployments.includes(d) || !d.instances.includes(row)) return;
      apply();
      settle(d);
    }, ms);
  step(1500, () => {
    row.state = "queued";
    row.node = node;
  });
  step(4000, () => {
    row.state = "running";
    row.health = "starting";
    row.startedAtMicros = now();
  });
  step(10000, () => {
    row.health = "healthy";
    row.healthDetail = "http 200 in 4ms";
  });
}

/** Recount a deployment from its rows, as `deployments::counts` does. */
function settle(d: Dep) {
  const cur = d.instances.filter((i) => i.generation === d.generation);
  d.running = cur.filter((i) => i.state === "running").length;
  d.healthy = cur.filter((i) => i.state === "running" && i.health === "healthy").length;
  if (d.status === "progressing" && d.healthy >= d.desired) d.status = "active";
}

// -- C1 arbitration: the queue ----------------------------------------------
//
// Ordered the way the node will actually run them: priority first, then
// furthest behind its share, then arrival (C1 §5.5, "priority stays the
// outer sort key").
const queueRows: QueueRowView[] = [
  { position: 1, job: rid("job-", 26), user: users[1].userId, username: "lab-op", priority: "high", sharePct: 13 },
  { position: 2, job: rid("job-", 26), user: users[2].userId, username: "mia", priority: "normal", sharePct: 31 },
  { position: 3, job: rid("job-", 26), user: users[2].userId, username: "mia", priority: "normal", sharePct: 31 },
  { position: 4, job: rid("job-", 26), user: FOUNDER, username: "neuralpunk", priority: "normal", sharePct: 52 },
  { position: 5, job: rid("job-", 26), user: FOUNDER, username: "neuralpunk", priority: "normal", sharePct: 52 },
  { position: 6, job: rid("job-", 26), user: FOUNDER, username: "neuralpunk", priority: "low", sharePct: 52 },
];

// -- C4 gangs ---------------------------------------------------------------
const gangRows: GangRowView[] = [
  {
    gang: rid("gang-", 26), shortId: "", preview: "torchrun train.py (4 × 1 gpu)", size: 4,
    placement: "pack", username: "mia", ownerName: "forge", state: "running", held: 4,
    priority: "normal", createdAtMicros: now() - 2_400e6,
  },
  {
    gang: rid("gang-", 26), shortId: "", preview: "mpirun solve (3 × 8 cpu)", size: 3,
    placement: "spread", username: "neuralpunk", ownerName: "director", state: "offering", held: 2,
    priority: "normal", createdAtMicros: now() - 18e6,
  },
];
for (const g of gangRows) g.shortId = g.gang.slice(0, 14);

// -- C7 the action cache ----------------------------------------------------
let cacheStats: CacheStatsView = {
  enabled: true, entries: 1_284, capacity: 4_096,
  hits: 3_907, misses: 612, hitRate: 3_907 / (3_907 + 612),
  bytesSaved: 41_284_996_608, published: 1_402, evicted: 118,
  verified: 39, mismatched: 0, epoch: 2, verifyRate: 0.01,
  byService: [
    { service: "build-image", hits: 1_902, misses: 141, entries: 604, bytesSaved: 30_064_771_072, hitRate: 1_902 / 2_043 },
    { service: "unit-tests", hits: 1_486, misses: 302, entries: 512, bytesSaved: 8_589_934_592, hitRate: 1_486 / 1_788 },
    { service: "lint", hits: 519, misses: 169, entries: 168, bytesSaved: 2_630_290_944, hitRate: 519 / 688 },
    { service: "site", hits: 0, misses: 0, entries: 0, bytesSaved: 0, hitRate: null },
  ],
};

// -- C8 legibility: the placement breakdown ---------------------------------
//
// Every term is a *penalty*, so the shortest bar wins. The weights are the
// scheduler's own (crates/mesh-core/src/scheduler.rs) — a chart drawn from
// different numbers than the ranking is a chart that lies.
const W: Record<string, number> = {
  load: 1.0, warmth: 0.6, failure: 0.8, rtt: 0.2, memory: 0.5,
  disk: 0.2, locality: 1.5, device: 1.2,
};
// What each term measures — `mesh_proto::term_gloss()`'s words, so the
// mock's hovers say what the product's do (L2 P4).
const GLOSS: Record<string, string> = {
  load: "Jobs already on that machine, against the slots it advertises.",
  warmth: "Whether it already holds this job's bundle. A cold node fetches first.",
  failure: "How many of that node's recent jobs ended badly.",
  rtt: "Round-trip time from this node to that one.",
  memory: "How much of its advertised memory is already spoken for.",
  disk: "How much of its data volume is already spoken for.",
  locality: "How much of this job's declared input it does not already hold.",
  device: "How busy the best accelerator on it is that could serve this job.",
};
function mkTerms(raw: Record<string, number>, unknown: string[] = []): ScoreTermView[] {
  return Object.keys(W).map((name) => ({
    name, raw: raw[name] ?? 0, weight: W[name],
    weighted: (raw[name] ?? 0) * W[name], unknown: unknown.includes(name),
    gloss: GLOSS[name] ?? null,
  }));
}
function mkCandidate(node: string, name: string, raw: Record<string, number>, chosen: boolean, unknown: string[] = []): CandidateScoreView {
  const terms = mkTerms(raw, unknown);
  return { node, name, total: terms.reduce((a, t) => a + t.weighted, 0), chosen, terms };
}
function mkPlacement(chosenName: string): PlacementView {
  const candidates = [
    mkCandidate(nodes[1].nodeId, "forge", { load: 0.28, warmth: 0, failure: 0.01, rtt: 0.11, memory: 0.38, disk: 0.06, device: 0.12 }, chosenName === "forge"),
    mkCandidate(nodes[2].nodeId, "anvil", { load: 0.25, warmth: 1, failure: 0.03, rtt: 0.14, memory: 0.41, disk: 0.22, device: 0.9 }, chosenName === "anvil"),
    mkCandidate(nodes[0].nodeId, "director", { load: 0.0, warmth: 1, failure: 0, rtt: 0, memory: 0.29, disk: 0.11, device: 1 }, chosenName === "director"),
    mkCandidate(nodes[3].nodeId, "edge-7", { load: 0.0, warmth: 1, failure: 0.34, rtt: 0.88, memory: 0.52, disk: 0.44, device: 1 }, chosenName === "edge-7", ["rtt"]),
  ];
  // Ranked, because the engine keeps the top five *candidates* and a
  // ranking is what makes them the top five.
  candidates.sort((a, b) => a.total - b.total);
  const winner = candidates[0];
  const runner = candidates[1];
  // The term that cost the runner-up the most, and by how much: the
  // one-line answer under the chart.
  let decisive: string | null = null;
  let delta = 0;
  for (const t of winner.terms) {
    const d = (runner.terms.find((x) => x.name === t.name)?.weighted ?? 0) - t.weighted;
    if (d > delta) { delta = d; decisive = t.name; }
  }
  return {
    // The best candidate answered Busy and the job went to the next one.
    // Derived, never asserted: a chart that showed the runner-up chosen
    // without saying why would look like the scheduler ignored its own
    // arithmetic (C8 §12.4 amendment 2).
    candidates, considered: 4, spilled: !winner.chosen,
    decisive, decisiveDelta: decisive ? delta : null,
    maxWeighted: Math.max(...candidates.flatMap((c) => c.terms.map((t) => t.weighted))),
    // `PlacementExplain::why()`'s sentence, transliterated for the same
    // reason the totals strip's is: the guide's screenshots have to show
    // the hover the product ships. The Rust side owns the wording.
    why: !winner.chosen
      ? `${chosenName} took the work after ${winner.name}, the best of 4 candidates, answered Busy.`
      : decisive
        ? `${winner.name} scored lowest of 4 candidates, and lower is better. The runner-up, ${runner.name}, lost mostly on ${decisive} (+${delta.toFixed(2)}).`
        : `${winner.name} scored lowest of 4 candidates, and lower is better. Nothing separated it from ${runner.name} — the tiebreak was the node id.`,
  };
}

const KEY_FIELDS: KeyFieldView[] = [
  { name: "runtime", value: "bash", digest: "a41c9e2f", differs: false },
  { name: "entrypoint", value: "scripts/build.sh", digest: "7d02b8c1", differs: false },
  { name: "argv", value: "--release", digest: "11f6ad30", differs: false },
  { name: "env", value: "4 declared keys (volatile excluded)", digest: "93be5107", differs: true },
  { name: "bundle root", value: "bndl-0e8aef1d1e6d", digest: "0e8aef1d", differs: false },
  { name: "inputs", value: "12 objects", digest: "c5901b7a", differs: false },
  { name: "outputs", value: "dist/**", digest: "2f7e4d68", differs: false },
  { name: "sandbox", value: "default", digest: "5a0cc9b4", differs: false },
  { name: "arch", value: "x86_64", digest: "e10d3f92", differs: false },
  { name: "epoch", value: "2", digest: "88a12c05", differs: false },
];

let executing = true;
let drained = false;
const startedAt = Date.now();

function status(): StatusView {
  return {
    version: V.current,
    proto: V.proto,
    nodeId: SELF,
    nodeShort: "mw1sdj5gterlfav",
    nodeName: "director",
    meshId: "mesh-qlljxxdslh66jm3n6vk7ukouva",
    meshName: "hyacinth",
    listenAddr: "0.0.0.0:40986",
    uptimeSecs: Math.floor((Date.now() - startedAt) / 1000) + 5400,
    membersAlive: nodes.filter((n) => census(n) && n.liveness === "alive").length,
    membersTotal: nodes.filter(census).length,
    jobsRunning: jobs.filter((j) => j.state === "running").length,
    jobsQueued: 3,
    slots: 16,
    draining: drained,
    executing,
    wan: { reachability: "natted", reflexiveAddr: "198.51.100.7:40986", advertise: [], relayServer: false, relayClients: 0, relays: ["mw1zx4p"] },
    // The mock Director founded its mock mesh, so the founder-only
    // affordances (address repair) are exercised rather than hidden.
    founderNode: SELF,
    isFounder: true,
    drainUntilSecs: null,
    drainReason: null,
    placement: "spread",
    dispatchCount: 1_481,
    dispatchP50Ms: 5,
    dispatchP95Ms: 50,
  };
}

/**
 * Where this person stands with the mesh (DIR-5).
 *
 * `?unenrolled` opens as a key the mesh has never heard of, and pairing in
 * through the wizard lands there too — a person who joins is not the
 * founder. "Request access" moves it on to `pending`, so the banner's
 * transitions can be watched without a second machine.
 */
let mockEnrolment: "enrolled" | "none" | "pending" = new URLSearchParams(location.search).has("unenrolled")
  ? "none"
  : "enrolled";

function whoami(): WhoAmIView {
  if (mockEnrolment !== "enrolled") {
    return {
      userId: "mwu1tt5r8lkc2v7bqm3xw9v4dphs6n2ye8ka5tzc4rj7f0gu",
      username: mockEnrolment === "pending" ? "mia" : null,
      registered: false,
      isFounder: false,
      roles: [],
      grants: [],
      nodeName: "mia-laptop",
      enrolment: mockEnrolment,
      enrolmentCode: mockEnrolment === "pending" ? "BRAVE-OTTER-4417" : null,
    };
  }
  // After the Security key card binds a key, this window signs as it: the
  // founder's second admin, not the founder (Y2/Y3).
  const bound = keyOn ? users.find((u) => u.userId === keyOn) : undefined;
  if (bound) {
    return {
      userId: bound.userId,
      username: bound.username,
      registered: true,
      isFounder: false,
      roles: bound.roles,
      grants: grants.filter((g) => g.userId === bound.userId),
      nodeName: nodes[0]?.name ?? "director",
      enrolment: "enrolled",
      enrolmentCode: null,
    };
  }
  return {
    userId: FOUNDER,
    username: "neuralpunk",
    registered: true,
    isFounder: true,
    roles: ["admin"],
    grants: [grants[0]],
    nodeName: nodes[0]?.name ?? "director",
    enrolment: "enrolled",
    enrolmentCode: null,
  };
}

// Two people waiting to be recognised (M23 §3.2) — one fresh ask and one
// an admin already declined, so the queue's two states are both visible
// while working on the screen.
let enrolments: EnrolmentView[] = [
  {
    userId: "mwu1tt5r8lkc2v7bqm3xw9v4dphs6n2ye8ka5tzc4rj7f0gu",
    shortId: "mwu1tt5r",
    suggestedUsername: "mia",
    fromNode: "laptop-mia",
    code: "BRAVE-OTTER-4417",
    requestedAtMicros: Date.now() * 1000 - 140 * 1_000_000,
    expiresAtMicros: Date.now() * 1000 + 3400 * 1_000_000,
    declined: false,
    declinedBy: null,
  },
  {
    userId: "mwu1q8w3e7r2t5y9u0i4o6p1a3s5d7f9g2h4j6k8l0z2x4c6",
    shortId: "mwu1q8w3",
    suggestedUsername: "contractor",
    fromNode: "dev-box-3",
    code: "QUIET-HERON-0912",
    requestedAtMicros: Date.now() * 1000 - 1800 * 1_000_000,
    expiresAtMicros: Date.now() * 1000 + 1700 * 1_000_000,
    declined: true,
    declinedBy: "neuralpunk",
  },
];

/** Everything this mesh distributes (M24 §3.1). */
function definitions(): DefinitionView[] {
  const now = Date.now() * 1000;
  return [
    {
      kind: "services",
      label: "Scripts and services",
      what: "What a named piece of work is: runtime, bundle, entry point, contract",
      op: "ServiceVersionAdd / ServiceHead",
      count: services.length,
      lastChangedMicros: now - 240 * 1_000_000,
      lastChangedWhat: "deploy-api v7",
      executes: false,
    },
    {
      kind: "collectors",
      label: "Fact collectors",
      what: "What each node runs to describe itself — a script, on a timer, unprompted",
      op: "FactCollectorPut",
      count: 3,
      lastChangedMicros: now - 2400 * 1_000_000,
      lastChangedWhat: "collector gpu",
      executes: true,
    },
    {
      kind: "schedules",
      label: "Schedules",
      what: "Standing authorizations to run a workflow at an instant nobody is present for",
      op: "SchedulePut",
      count: 2,
      lastChangedMicros: now - 86_400 * 1_000_000,
      lastChangedWhat: "schedule nightly-upgrade",
      executes: true,
    },
    {
      kind: "sources",
      label: "Git sources",
      what: "Git repositories a watcher node turns into bundles",
      op: "SourceAdd",
      count: sources.length,
      lastChangedMicros: now - 600 * 1_000_000,
      lastChangedWhat: "source automation",
      executes: true,
    },
  ];
}

function matchSelector(sel: string): NodeView[] {
  const s = (sel || "any").trim();
  const live = nodes.filter((n) => n.executing && !n.revoked && !n.left);
  if (s === "any") return live;
  // `|` is disjunction (M24 §10.1) — what a hand-picked set of ticks signs
  // as. Modelled here because the rollout screen produces it constantly.
  if (s.includes("|")) {
    const parts = s.split("|").map((p) => p.trim()).filter(Boolean);
    const seen = new Set<string>();
    return parts
      .flatMap((p) => matchSelector(p))
      .filter((n) => (seen.has(n.nodeId + n.name) ? false : seen.add(n.nodeId + n.name)));
  }
  if (s.startsWith("tag=")) {
    const tag = s.slice(4);
    // `cap.ver` is announced by every node and stripped from the tags a
    // surface *shows* (it is capacity, not a label somebody applied), so a
    // version ring is matched against the build the node reports — which is
    // what the daemon does with the full tag list.
    if (tag.startsWith("cap.ver=")) return live.filter((n) => n.version === tag.slice(8));
    return live.filter((n) => n.tags.includes(tag));
  }
  if (s.startsWith("name=")) return live.filter((n) => n.name === s.slice(5));
  if (s.startsWith("id=")) return live.filter((n) => n.nodeId.startsWith(s.slice(3)));
  return live.filter((n) => n.name === s || n.nodeId.startsWith(s));
}

/**
 * A mock parallel run (M9 §5.4). Items are drained by per-node "lanes"
 * whose count follows each node's advertised cores, so the mock reproduces
 * the property the real engine has and the Compute section exists to show:
 * a bigger node ends up with proportionally more of the work, without
 * anything computing the split.
 */
async function streamParallel(args: Args, ch: ChannelLike) {
  const send = (ev: ParallelEvent) => ch.onmessage(ev);
  const items = (args.items as string[]) ?? [];
  const service = (args.service as string | null) ?? null;
  const command = service ? `${service} {}` : String(args.command ?? "");
  const targets = matchSelector(String(args.on ?? "any")).filter((n) => n.executing);
  if (items.length === 0 || targets.length === 0) {
    send({ type: "failed", message: targets.length === 0 ? "no live node matches this selector" : "the item list is empty" });
    return;
  }
  const jobId = rid("job-", 10);
  const job = addJob({ cmdPreview: `parallel ×${items.length} — ${command}`, executorName: nodes[0].name, state: "running", exitCode: null, finishedAtMicros: null });
  job.jobId = jobId;
  job.runtime = "parallel";
  send({
    type: "started",
    jobId,
    total: items.length,
    workers: targets.map((t) => t.name),
    declined: matchSelector(String(args.on ?? "any"))
      .filter((n) => !n.executing)
      .map((n) => `${n.name}: not a placement candidate`),
    // A plausible history, so the preview is designed against something
    // that looks like a real one rather than against `null` (R5 P4).
    estimate: { p10Secs: 42, p50Secs: 68, p90Secs: 155, samples: 41 },
  });

  const stats = new Map<string, { name: string; items: number; chunks: number; micros: number }>(
    targets.map((t) => [t.nodeId, { name: t.name, items: 0, chunks: 0, micros: 0 }]),
  );
  const perNode = (): NodeProgressView[] =>
    [...stats.entries()].map(([nodeId, s]) => ({
      nodeId,
      name: s.name,
      items: s.items,
      chunks: s.chunks,
      itemsPerSec: s.items / Math.max(0.001, (Date.now() - started) / 1000),
      sharePct: items.length ? (s.items / items.length) * 100 : 0,
    }));

  const started = Date.now();
  let cursor = 0;
  let done = 0;
  let failed = 0;
  let inflight = 0;
  const chunkItems = (args.chunk as number | null) ?? 1;
  // M9 §5.5: with output kept in the mesh, only a reference comes back.
  const keepOutput = Boolean(args.keepOutput);
  const reduce = (args.reduce as string | null) ?? null;
  let speculated = 0;
  // Lanes in proportion to advertised cores, scaled down so the mock does
  // not spawn sixty promises: the ratio is what matters, not the count.
  const lanes = targets.flatMap((t) =>
    Array.from({ length: Math.max(1, Math.round((t.capacity?.cores ?? 2) / 4)) }, () => t),
  );

  const ticker = setInterval(() => {
    // The tail gets a re-run or two, as a real straggler would (§5.6).
    if (done > items.length * 0.95 && speculated === 0) speculated = 1;
    send({
      type: "progress",
      total: items.length,
      done,
      failed,
      inflight,
      chunkItems,
      speculated,
      perNode: perNode(),
    });
  }, 350);

  await Promise.all(
    lanes.map(async (node) => {
      for (;;) {
        if (cursor >= items.length) return;
        const take = items.slice(cursor, cursor + chunkItems);
        cursor += take.length;
        inflight += take.length;
        const cost = 90 + Math.random() * 60;
        await sleep(cost);
        inflight -= take.length;
        const s = stats.get(node.nodeId);
        for (const item of take) {
          // A tenth of the items fail, so Results and the retry path have
          // something real to show.
          const bad = Math.random() < 0.1;
          if (bad) failed++;
          done++;
          if (s) {
            s.items++;
            s.micros += cost * 1000;
          }
          const rendered = command.includes("{}")
            ? command.replaceAll("{}", item)
            : `${command} ${item}`;
          send({
            type: "item",
            item,
            exitCode: bad ? 1 : 0,
            stdout: bad || keepOutput ? "" : `${rendered}\n`,
            stderr: bad ? `gzip: ${item}: unexpected end of file\n` : "",
            nodeName: node.name,
            output:
              bad || !keepOutput
                ? null
                : {
                    id: `obj-${hashish(item)}`,
                    size: rendered.length + 1,
                    holder: node.nodeId,
                  },
          });
        }
        if (s) s.chunks++;
      }
    }),
  );
  clearInterval(ticker);
  job.state = failed > 0 ? "failed" : "succeeded";
  job.exitCode = failed > 0 ? 1 : 0;
  job.finishedAtMicros = now();
  if (keepOutput && reduce && failed === 0) {
    const on = targets[0]?.name ?? "this node";
    await sleep(200);
    send({
      type: "reduced",
      nodeName: on,
      exitCode: 0,
      stdout: `${items.length} outputs merged\n`,
      stderr: "",
      inputs: items.length,
    });
  }
  send({
    type: "done",
    total: items.length,
    succeeded: items.length - failed,
    failed,
    micros: (Date.now() - started) * 1000,
    speculated,
    perNode: perNode(),
  });
}

/** A stable, obviously-fake content address for the mock's outputs. */
function hashish(item: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < item.length; i++) {
    h = Math.imul(h ^ item.charCodeAt(i), 0x01000193) >>> 0;
  }
  return h.toString(16).padStart(8, "0").repeat(8).slice(0, 64);
}

async function streamRun(args: Args, ch: ChannelLike, label: string) {
  const send = (ev: RunEvent) => ch.onmessage(ev);
  const targets = matchSelector(String(args.on ?? "any"));
  const fanout = args.fanout as number | null;
  // A `--dry-run` answers with the resolved submission, and
  // runs nothing — the engine's contract. The mock used to fall straight
  // through to a real run here, so Preview on the Run screen launched the
  // job it was meant to describe.
  if (args.dryRun) {
    await sleep(220);
    const svc = args.name ? services.find((s) => s.name === args.name) : null;
    const chosen = fanout == null ? targets.slice(0, 1) : targets;
    send({
      type: "preview",
      preview: {
        kind: svc ? "service" : "command",
        action: label,
        service: svc?.name ?? null,
        version: svc?.version ?? null,
        bundleId: svc?.bundleId ?? null,
        runtime: svc?.runtime ?? null,
        selector: String(args.on ?? "any"),
        targets: chosen.map((n) => n.name),
        excluded: [],
        envKeys: ((args.env as [string, string][] | undefined) ?? []).map(([k]) => k),
        secrets: [],
        priority: String(args.priority ?? "normal"),
        timeoutSecs: (args.timeoutSecs as number | null) ?? null,
        blastRadius: chosen.length,
      },
    });
    return;
  }
  const lines = [
    `hello from ${label} on ${"{node}"}`,
    "argv: --who there",
    "GREETING=hola  TARGET=unset",
    "cwd: …/jobs/job-…/checkout/scripts",
  ];
  // M19: capture is opt-in, so the mock only produces rows when asked —
  // the same shape the real path has.
  const capture = Boolean(args.captureOutputs) || Boolean(args.expect);
  const expectExpr = String(args.expect ?? "").trim();
  const rowsOut: { node: string; ok: boolean; text: string; jobId: string }[] = [];
  const emitRow = (node: string, jobId: string, ok: boolean, text: string) => {
    if (!capture) return;
    rowsOut.push({ node, ok, text, jobId });
    send({
      type: "result",
      row: {
        node,
        jobId,
        state: ok ? "succeeded" : "failed",
        exitCode: ok ? 0 : 255,
        outputs: [
          ["stdout", text],
          ["stderr", ok ? "" : "ssh: connect to host timed out"],
          ["exit_code", ok ? "0" : "255"],
        ],
        expectPassed: expectExpr ? ok : null,
        expectDetail: expectExpr && !ok ? `${text} does not satisfy ${expectExpr}` : "",
      },
    });
  };
  const emitExpected = () => {
    if (!capture || !expectExpr) return;
    const bad = rowsOut.filter((r) => !r.ok);
    send({
      type: "expected",
      expression: expectExpr,
      passed: rowsOut.length - bad.length,
      failed: bad.map((r) => `${r.node}: ${r.text} does not satisfy ${expectExpr}`),
    });
  };
  // `seq N`: a long, chatty command (DIR-9) — N lines, five hundred to a
  // chunk and as fast as the channel takes them, the load Launch's pane
  // used to freeze under.
  const flood = /^seq (\d+)$/.exec(label.trim());
  if (flood && (fanout === null || fanout === undefined)) {
    const t = targets[0] ?? nodes[1];
    const job = addJob({ cmdPreview: label, executorName: t.name, state: "running", exitCode: null, finishedAtMicros: null });
    send({ type: "started", jobId: job.jobId, executorName: t.name });
    const total = Math.min(Number(flood[1]), 5_000_000);
    for (let n = 1; n <= total; n += 500) {
      const upto = Math.min(total, n + 499);
      let text = "";
      for (let k = n; k <= upto; k++) text += `${k}\n`;
      send({ type: "chunk", index: null, stream: 1, text });
      if (n % 5000 === 1) await sleep(1);
    }
    job.state = "succeeded";
    job.exitCode = 0;
    job.finishedAtMicros = now();
    send({ type: "done", index: null, state: "succeeded", exitCode: 0 });
    return;
  }
  if (fanout === null || fanout === undefined) {
    const t = targets[0] ?? nodes[1];
    const job = addJob({ cmdPreview: label, executorName: t.name, state: "running", exitCode: null, finishedAtMicros: null });
    send({ type: "started", jobId: job.jobId, executorName: t.name });
    for (const l of lines) {
      await sleep(180);
      send({ type: "chunk", index: null, stream: 1, text: l.replace("{node}", t.name) + "\n" });
    }
    await sleep(200);
    job.state = "succeeded";
    job.exitCode = 0;
    job.finishedAtMicros = now();
    emitRow(t.name, job.jobId, true, lines[0].replace("{node}", t.name));
    emitExpected();
    send({ type: "done", index: null, state: "succeeded", exitCode: 0 });
    return;
  }
  const chosen = fanout === 0 ? targets : targets.slice(0, fanout);
  const group = rid("fan-", 26);
  const js = chosen.map((t, i) => addJob({ cmdPreview: label, executorName: t.name, state: "running", exitCode: null, finishedAtMicros: null, fanGroup: group, fanIndex: i, fanTotal: chosen.length }));
  send({ type: "fanoutStarted", group, targets: chosen.map((t, i) => ({ jobId: js[i].jobId, nodeId: t.nodeId, nodeName: t.name })) });
  let failed = 0;
  await Promise.all(
    chosen.map(async (t, i) => {
      await sleep(100 + i * 120);
      for (const l of lines) {
        await sleep(150);
        send({ type: "chunk", index: i, stream: 1, text: l.replace("{node}", t.name) + "\n" });
      }
      const fail = t.liveness === "suspect";
      if (fail) {
        failed++;
        send({ type: "chunk", index: i, stream: 2, text: "ssh: connect to host timed out\n" });
      }
      js[i].state = fail ? "failed" : "succeeded";
      js[i].exitCode = fail ? 255 : 0;
      js[i].finishedAtMicros = now();
      emitRow(t.name, js[i].jobId, !fail, lines[0].replace("{node}", t.name));
      send({ type: "done", index: i, state: js[i].state, exitCode: js[i].exitCode });
    }),
  );
  emitExpected();
  send({ type: "fanoutDone", succeeded: chosen.length - failed, failed, skipped: 0 });
}

async function streamLogs(args: Args, ch: ChannelLike) {
  const job = jobs.find((j) => j.jobId === args.jobId);
  const send = (ev: RunEvent) => ch.onmessage(ev);
  if (!job) {
    send({ type: "failed", message: "no such job" });
    return;
  }
  for (const l of ["started", `running ${job.cmdPreview} on ${job.executorName}`, "…"]) {
    await sleep(120);
    send({ type: "chunk", index: null, stream: 1, text: l + "\n" });
  }
  if (job.state !== "running") send({ type: "done", index: null, state: job.state, exitCode: job.exitCode });
}

async function streamDrain(args: Args, ch: ChannelLike) {
  const send = (ev: DrainEvent) => ch.onmessage(ev);
  for (const p of [
    { running: 2, queued: 3, moved: args.movePending ? 3 : 0, devicesHeld: 2 },
    { running: 1, queued: 0, moved: args.movePending ? 3 : 0, devicesHeld: 1 },
    { running: 0, queued: 0, moved: args.movePending ? 3 : 0, devicesHeld: 0 },
  ]) {
    await sleep(700);
    send({ type: "progress", ...p });
  }
  await sleep(400);
  drained = true;
  send({ type: "done" });
}

const doctor = [
  { name: "config", verdict: "ok", detail: "defaults (no config file)" },
  { name: "home", verdict: "ok", detail: "~/.meshware/director writable" },
  { name: "layout", verdict: "ok", detail: "everything under ~/.meshware/director" },
  { name: "disk", verdict: "ok", detail: "32.5 GB free at the data dir" },
  { name: "identity", verdict: "ok", detail: "node + user keys present" },
  { name: "genesis", verdict: "ok", detail: "mesh 'hyacinth' — signature valid" },
  { name: "bash", verdict: "ok", detail: "bash found" },
  { name: "git", verdict: "ok", detail: "git (Git sources) found" },
  { name: "python env", verdict: "warn", detail: "uv not found; python3 (uv fallback) found" },
  { name: "cgroups v2", verdict: "ok", detail: "available" },
  { name: "landlock", verdict: "ok", detail: "available" },
  { name: "daemon", verdict: "ok", detail: `up 5412s, version ${V.current}` },
  { name: "wan", verdict: "ok", detail: "natted (seen as 198.51.100.7:40986), via relays mw1zx4p" },
  { name: "clock", verdict: "ok", detail: "no significant skew observed in the mesh" },
  { name: "members", verdict: "warn", detail: "1 member(s) observed suspect (of 4)" },
  { name: "peer", verdict: "ok", detail: "forge reachable (812 µs rtt)" },
  { name: "peer", verdict: "fail", detail: "edge-7: unreachable" },
];


// -- releases (M24 §9) ---------------------------------------------------------
//
// A mesh that has trusted one signing key and published two versions, with a
// third sitting in a build folder ready to go — the state the guide's
// screenshots describe.
let releaseKeys = [
  {
    pubkey: "RWQa4oJEOptFbp8PrU2VnyoJqZFG36xx4p1goF965ZvZxsXH7mUYxwux",
    label: "hyacinth release signing key",
    addedBy: "mia",
    addedAtMicros: String(now() - 86_400e6 * 12),
    revoked: false,
    inUseHere: true,
  },
];
let publishedReleases = [V.current, V.previous];
/** Digests a security key has co-signed this session (Y6). */
const mockCosigned = new Set<string>();
let builds = [
  {
    path: `/home/mia/meshware/dist/meshnode-v${V.next}-x86_64-unknown-linux-musl`,
    token: "mock-build-1",
    fileName: `meshnode-v${V.next}-x86_64-unknown-linux-musl`,
    version: V.next,
    arch: "x86_64",
    size: String(22_222_640),
    signed: true,
    verified: true,
    published: false,
    blocked: null as string | null,
  },
  {
    path: `/home/mia/meshware/dist/meshnode-v${V.next}-aarch64-unknown-linux-musl`,
    token: "mock-build-2",
    fileName: `meshnode-v${V.next}-aarch64-unknown-linux-musl`,
    version: V.next,
    arch: "aarch64",
    size: String(21_004_112),
    signed: false,
    verified: false,
    published: false,
    blocked:
      `not signed — no node will install it. Sign it with \`minisign -HSm meshnode-v${V.next}-aarch64-unknown-linux-musl -s <your key>\`, or rebuild with packaging/build-release.sh.`,
  },
];


// -- M31: retention and history export ----------------------------------------
//
// The default fixture is the honest one: **the shipped defaults, nobody has
// signed anything, and there are no sinks.** That is what almost every mesh
// looks like, and designing against a screen full of warehouses would be
// designing for the exception.

function freshRetention(): RetentionView {
  return {
    windows: [
      { table: "runs", window: "90d", floor: null, effectiveHere: "90d", boundsTheOpLog: true },
      { table: "facts", window: "1y", floor: null, effectiveHere: "1y", boundsTheOpLog: false },
      { table: "job-output", window: "30d", floor: null, effectiveHere: "30d", boundsTheOpLog: false },
      { table: "audit", window: "forever", floor: "1y", effectiveHere: "forever", boundsTheOpLog: false },
      { table: "archive", window: "5y", floor: null, effectiveHere: "5y", boundsTheOpLog: false },
    ],
    signedBy: null,
    signedAtMicros: 0,
    oplogBytes: 412_700_000,
    retirableBytesPerDay: 3_100_000,
    permanentBytesPerDay: 184_000,
    settlesAtBytes: 3_100_000 * 90 + 184_000 * 365,
  };
}

let retention: RetentionView = freshRetention();

function settledBytes(): number | null {
  const runs = retention.windows.find((w) => w.table === "runs");
  if (!runs || runs.window === "forever") return null;
  const days = runs.window.endsWith("y")
    ? parseInt(runs.window) * 365
    : parseInt(runs.window);
  return retention.retirableBytesPerDay * days + retention.permanentBytesPerDay * 365;
}

/** Mirrors `SinkKind::capability_sentence()` — kept in step by the shots
 *  pass, and wrong here is only wrong in the mock. */
const sinkKinds: SinkKindView[] = [
  { kind: "plugin", label: "Your own program", capability: "Hands each batch to your own program. What happens next is yours; Director cannot read it back.", needsUrl: false, payloadsByDefault: false, archives: true, replayPayloads: false },
  { kind: "clickhouse", label: "ClickHouse", capability: "Stores your history where your analysts already query it, and Director can open past job output from here.", needsUrl: true, payloadsByDefault: false, archives: true, replayPayloads: true },
  { kind: "victorialogs", label: "VictoriaLogs", capability: "Stores history and job output in a log store built for it. Director can open past job output from here, and search inside it.", needsUrl: true, payloadsByDefault: true, archives: true, replayPayloads: true },
  { kind: "opensearch", label: "OpenSearch / Elasticsearch", capability: "Stores your history in OpenSearch or Elasticsearch. Director can open past job output from here, and search inside it.", needsUrl: true, payloadsByDefault: false, archives: true, replayPayloads: true },
  { kind: "webhook", label: "Webhook", capability: "POSTs each batch to a URL you control. meshware knows nothing about what happens to it and cannot read it back.", needsUrl: true, payloadsByDefault: false, archives: false, replayPayloads: false },
  { kind: "postgres", label: "Postgres / TimescaleDB", capability: "Writes into a Postgres you already run, where your BI tool already points. Director cannot read it back — you will query it in Postgres.", needsUrl: true, payloadsByDefault: false, archives: true, replayPayloads: false },
  { kind: "splunk-hec", label: "Splunk (HEC)", capability: "Sends events to Splunk, where your security team already looks. Best paired with the audit table alone: HEC is priced per gigabyte. Director cannot read it back.", needsUrl: true, payloadsByDefault: false, archives: true, replayPayloads: false },
  { kind: "otlp", label: "OpenTelemetry (OTLP)", capability: "Sends each workflow to your observability stack as a trace, one span per step — Grafana, Datadog, Honeycomb and the rest. This is instrumentation rather than an archive: it is not a copy of your history and cannot be read back.", needsUrl: true, payloadsByDefault: false, archives: false, replayPayloads: false },
];

/** Empty by default — the state almost every mesh is in. Add one from the
 *  UI, or start with `?mock=1&sinks=1` to design the configured state. */
let sinks: SinkView[] = new URLSearchParams(location.search).has("sinks")
  ? [
      {
        name: "warehouse", kind: "clickhouse", target: "https://warehouse.corp:8443",
        database: "meshware", tables: ["jobs"], enabled: true,
        elected: "anvil", electedHere: false, statusIsLocal: false, runOn: "",
        lagSecs: 124, sent: 1_284_902,
        spooledBytes: 0, dropped: 0, lastStatus: "ok", lastAtMicros: Date.now() * 1000,
        pausedByRestore: false, payloadsSent: 0, payloadsTooLarge: 0,
        holdForExportDays: 7, forcedGaps: 0, archives: true, replayPayloads: true,
        createdBy: "ada", createdAtMicros: (Date.now() - 86_400_000 * 40) * 1000,
        description: "",
      },
      {
        name: "siem", kind: "opensearch", target: "https://search.corp:9200",
        database: "acme", tables: ["audit"], enabled: true,
        elected: "", electedHere: false, statusIsLocal: true, runOn: "",
        lagSecs: 14_400, sent: 900,
        spooledBytes: 12_400_000, dropped: 3, lastStatus: "HTTP 503 upstream connect error",
        lastAtMicros: Date.now() * 1000, pausedByRestore: false,
        payloadsSent: 0, payloadsTooLarge: 0, holdForExportDays: 7, forcedGaps: 2,
        archives: true, replayPayloads: true, createdBy: "ada",
        createdAtMicros: (Date.now() - 86_400_000 * 12) * 1000, description: "",
      },
    ]
  : [];


// -- M31 B3: history across the tiers, and reading a payload back -----------
//
// The fixture exists to make the *interesting* states designable without a
// six-month-old mesh: recent rows out of the index, older rows out of
// Parquet, one job whose output went to a warehouse, and one whose output
// simply aged out. A mock that only shows the happy row designs only the
// happy row.
const DAY_US = 86_400e6;
function histJob(
  i: number,
  command: string,
  agoDays: number,
  over: Partial<HistoryJobView> = {},
): HistoryJobView {
  const finished = now() - agoDays * DAY_US;
  return {
    job: rid("job-", 26),
    command,
    runtime: "bash",
    executor: ["forge", "anvil", "edge-7"][i % 3],
    submitterUser: ["neuralpunk", "lab-op", "mia"][i % 3],
    state: "succeeded",
    exitCode: 0,
    createdAtMicros: String(Math.round(finished - 6e6)),
    finishedAtMicros: String(Math.round(finished)),
    durationMs: "6000",
    // Anything past the index's window came out of Parquet. 30 days is the
    // shipped `[history]` window, so the boundary in the fixture is the
    // boundary a real mesh has.
    fromArchive: agoDays > 30,
    payloadSink: null,
    // A2's states, as the daemon computes them: a job that ran this week
    // still has its log on the machine that ran it; a six-month-old one
    // does not, and says so with a date rather than a shrug.
    outputState: agoDays > 30 ? "expired" : "available",
    outputShort: agoDays > 30 ? `expired ${new Date(finished / 1000).toISOString().slice(0, 10)}` : "—",
    outputMessage:
      agoDays > 30
        ? "output expired — the node that ran it keeps job output for a limited time (`[retention]`). The record is complete."
        : "",
    ...over,
  };
}

const historyJobs: HistoryJobView[] = [
  histJob(0, "deploy (bash/deploy.sh)", 0.02),
  histJob(1, "apt list --upgradable", 0.4, { state: "failed", exitCode: 100 }),
  histJob(2, "systemctl is-active nginx", 1.2),
  histJob(0, "pgbench -c 32 -T 600 warehouse", 12),
  histJob(1, "nightly-backup (bash/backup.sh)", 34),
  // The six-month question, and the reason B3 exists: the record is here,
  // the 5 GB of stdout is in the warehouse, and the panel can open it.
  histJob(2, "etl-march (python/etl.py)", 183, {
    state: "failed",
    exitCode: 1,
    durationMs: "5340000",
    payloadSink: "warehouse",
    outputState: "elsewhere",
    outputShort: "in warehouse",
    outputMessage: "output is in `warehouse` — open it from there, or click through to fetch it.",
  }),
  // Same age, no sink at the time: **expired**, and the destination's
  // existence must never soften that into a promise (§14.3 item 7).
  histJob(0, "reindex (bash/reindex.sh)", 191),
];

/** Two tiers' worth of lines, so the payload panel can be designed with a
 *  tail, a "load earlier", and a search that actually filters. */
const PAYLOAD_LINES = Array.from({ length: 4000 }, (_, i) =>
  i % 397 === 396
    ? `2026-03-04T02:1${i % 10}:00Z ERROR chunk ${i} failed: connection reset by peer`
    : `2026-03-04T02:1${i % 10}:00Z INFO  processed chunk ${i} (${1000 + (i % 97)} rows)`,
);


// -- M8 schedules -----------------------------------------------------------

/** Minutes east of UTC a draft's `tz` names. */


/** The link this mock has already served, so the slot empties like the
 *  real one — and refills when the `link=` parameter changes, which is how
 *  a second link arriving while Director runs is exercised. */
let deepLinkServed: string | null = null;

/** Whether the mock's key has been unlocked this page load (B9). */
let mockUnlocked = false;
/** A demo started from the wizard is running (`demo_start`, UX-18). */
let mockDemo = false;

/** The content store's reclaim level and cap, so the Storage pane's radio
 *  ladder and cap field behave like controls rather than decoration. */
let contentPolicy = "balanced";
let contentCapBytes = 0;

/**
 * The IPC boundary, as the real one behaves: every answer crosses it as a
 * **copy**. Tauri serialises each result, so the app never holds a
 * reference into the engine's own tables; this mock used to hand its
 * arrays back by reference, and a `$state.raw` reassigned to the *same*
 * array is a no-op — the rail's Scripts count sat at 4 after a fifth
 * script was added, while the tab beside it said 5. Cloning here keeps
 * the mock honest about that for every command at once. Channels and
 * functions cannot be cloned and are passed through untouched.
 */
/** Picks the mock pickers handed out, token → the path they stand for. */
const mockPicks = new Map<string, string>();
/** `?startfail` has refused its one start (DIR-11). */
let mockStartFailed = false;
/** Scripted build terminals handed out (`build_run`, DIR-8). */
let mockBuildTerm = 0;

/** Mock picks that name a file rather than a folder — anything with an
 *  extension, which is every file the mock hands out. */
function mockPickIsFile(path: string): boolean {
  return /\.[A-Za-z0-9]+$/.test(path.split("/").at(-1) ?? "");
}

/** A path chosen or dropped, as the Rust side hands it to the window. */
function mockPick(path: string) {
  const token = `mock-${mockPicks.size + 1}`;
  mockPicks.set(token, path);
  return { token, name: path.split("/").at(-1) ?? path, path };
}
/** The remembered build folder the backend would hold. */
let mockBuildDir: string | null = null;
/** The OS keychain (`credentials.rs`): which secrets each user has, never
 *  the secrets (S3). One key-file account, so the form's "filled in from
 *  your keychain" path — and "type a different one" — can be driven. */
const mockKeychain = new Map<string, Omit<RememberedCredentialView, "keyToken">>([
  ["ops", { user: "ops", method: "key", keyPath: "/home/you/.ssh/id_ed25519", hasPassword: false, hasPassphrase: true, hasSudoPassword: true }],
]);

async function invoke(cmd: string, args: Args = {}): Promise<unknown> {
  const out = await invokeRaw(cmd, args);
  try {
    return structuredClone(out);
  } catch {
    return out;
  }
}

/**
 * The reads `?deny` refuses when it names none (UX-2): the lists a member
 * without a grant is turned away from, and whose screens each have an
 * empty state that must not stand in for the refusal.
 */
const DENIED_BY_DEFAULT = [
  "jobs_view",
  "secrets_view",
  "users_view",
  "grants_view",
  "deployments_view",
  "schedules_view",
  "registry_view",
];

async function invokeRaw(cmd: string, args: Args = {}): Promise<unknown> {
  await sleep(cmd.endsWith("_view") || cmd === "status_view" ? 15 : 250);
  // `?keypause`: a refresh finds the security key refusing, the way the
  // engine's KeyGate answers once it has stopped asking.
  if (mockKeyPaused && cmd === "status_view") {
    throw "security-key-paused: the agent refused to sign with your key";
  }
  // `?deny` refuses reads the way the engine refuses a member without the
  // grant — `?deny` alone for the usual lists, `?deny=secrets_view,…` for
  // exactly those — with the words Director prints for `IpcResponse::Denied`.
  const deny = new URLSearchParams(location.search).get("deny");
  if (deny !== null && (deny ? deny.split(",") : DENIED_BY_DEFAULT).includes(cmd)) {
    throw "permission denied: requires one of: admin, auditor";
  }
  switch (cmd) {
    // B9 — the key-at-rest gate and the Settings panel. `?locked` draws
    // the unlock gate; `?protected` draws an unlocked-but-sealed key, which
    // is what Settings looks like most of the time.
    case "identity_lock_status": {
      const q = new URLSearchParams(location.search);
      // `mockUnlocked` is what makes `?locked` coherent rather than a
      // stuck screen: unlock once and the rest of the session agrees,
      // the way the real session cache does.
      const locked = q.has("locked") && !mockUnlocked;
      const sealed = q.has("locked") || q.has("protected");
      return {
        path: "~/.meshware/identity/user.key",
        exists: true,
        hardware: keyOn !== null,
        hardwareReady: keyOn === null || agentKeys().some((k) => k.userId === keyOn),
        protected: sealed,
        unlocked: sealed && !locked,
        unlockedForSecs: sealed && !locked ? "41231" : null,
        sessionStore: q.has("nosession") ? null : "/run/user/1000/meshware/unlock-ca89a432e5b6",
      };
    }
    case "identity_unlock": {
      // The wrong passphrase has to be reachable in the mock, or the error
      // path is a thing nobody ever looks at.
      if (String((args as { passphrase?: string }).passphrase ?? "") !== "hunter2") {
        throw "wrong passphrase, or this key file has been modified";
      }
      mockUnlocked = true;
      return {
        path: "~/.meshware/identity/user.key",
        exists: true,
        hardware: keyOn !== null,
        hardwareReady: keyOn === null || agentKeys().some((k) => k.userId === keyOn),
        protected: true,
        unlocked: true,
        unlockedForSecs: "43200",
        sessionStore: "/run/user/1000/meshware/unlock-ca89a432e5b6",
      };
    }
    case "identity_lock_now":
      mockUnlocked = false;
      return {
        path: "~/.meshware/identity/user.key",
        exists: true,
        hardware: keyOn !== null,
        hardwareReady: keyOn === null || agentKeys().some((k) => k.userId === keyOn),
        protected: true,
        unlocked: false,
        unlockedForSecs: null,
        sessionStore: "/run/user/1000/meshware/unlock-ca89a432e5b6",
      };
    case "identity_protect":
      return {
        path: "~/.meshware/identity/user.key",
        exists: true,
        hardware: keyOn !== null,
        hardwareReady: keyOn === null || agentKeys().some((k) => k.userId === keyOn),
        protected: true,
        unlocked: true,
        unlockedForSecs: "43200",
        sessionStore: "/run/user/1000/meshware/unlock-ca89a432e5b6",
      };
    case "identity_unprotect":
      return {
        path: "~/.meshware/identity/user.key",
        exists: true,
        hardware: keyOn !== null,
        hardwareReady: keyOn === null || agentKeys().some((k) => k.userId === keyOn),
        protected: false,
        unlocked: false,
        unlockedForSecs: null,
        sessionStore: "/run/user/1000/meshware/unlock-ca89a432e5b6",
      };
    // -- Y2/Y3 security keys ------------------------------------------------
    //
    // The founder's path by default: the YubiKey's key is not a member, so
    // the plan is to add it as a second admin. Using it binds for the rest
    // of the session, and the founding key is "set aside".
    case "security_key_status": {
      const noAgent = mockQ.get("agent") === "0";
      const me = keyOn ?? FOUNDER;
      const keys = noAgent ? [] : agentKeys().map((k) => ({ ...k, current: k.userId === me }));
      const current = keys.find((k) => k.current);
      return {
        path: "~/.meshware/identity/user.key",
        held: keyOn ? "hardware" : "software",
        origin: keyOn ? (keyOn === FILE_KEY_ID ? "file" : "device") : null,
        file: keyOn === FILE_KEY_ID ? "~/.ssh/meshware" : null,
        identity: me,
        agent: noAgent ? null : "/run/user/1000/gcr/ssh",
        agentError: noAgent ? "no agent to ask: neither MESHWARE_AGENT_SOCK nor SSH_AUTH_SOCK is set" : null,
        keys,
        reachable: keyOn !== null && current !== undefined,
        canConnect: true,
        setAside: spareKept ? [{ name: "user.key.set-aside-mwu1zmqqlw5kdeas", shortId: "mwu1zmqqlw5kdeas" }] : [],
        beside: ["user.key.from-director"],
        paused: mockKeyPaused ? "the agent refused to sign" : null,
      };
    }
    case "security_key_connect": {
      if (String(args.pin) !== "123456") {
        throw "That didn't work. Check that your YubiKey is plugged in and the PIN is right.";
      }
      yubikeyLoaded = true;
      mockKeyPaused = false;
      return invoke("security_key_status", {});
    }
    case "security_key_resume":
      mockKeyPaused = false;
      return null;
    case "security_key_test": {
      const key = agentKeys().find((k) => k.userId.startsWith(String(args.key)));
      if (!key) throw "Director can't see that key any more. Is your security key still plugged in?";
      if (key.origin === "file") return { ok: false, asksEveryTime: false, message: "That's ~/.ssh/meshware, a key file on this computer — not a key on your security key. Moving to it wouldn't protect anything." };
      if (mockQ.has("asks")) return { ok: false, asksEveryTime: true, message: "Your key asked for you a second time. Director checks in with your mesh every few seconds, so it would keep asking you. A key that asks for its PIN once per session works well — the commands below make one." };
      return { ok: true, asksEveryTime: false, message: "Your key signed, and it's ready to use." };
    }
    case "security_key_restore":
      keyOn = null;
      spareKept = false;
      return null;
    case "security_key_plan":
    case "security_key_use": {
      const key = agentKeys().find((k) => k.userId.startsWith(String(args.key)));
      if (!key) throw "Director can't see that key any more. Is your security key still plugged in?";
      if (key.origin === "file") {
        const message = "That's ~/.ssh/meshware, a key file on this computer — not a key on your security key. Moving to it wouldn't protect anything.";
        if (cmd === "security_key_use") throw message;
        return { key: key.userId, comment: key.comment, plan: "file", username: "", newUsername: null, oldKey: null, message };
      }
      const known = users.some((u) => u.userId === key.userId && !u.revoked);
      const plan = key.userId === keyOn ? "same" : known ? "bind" : "add-admin";
      if (cmd === "security_key_plan") {
        return {
          key: key.userId, comment: key.comment, plan, username: "neuralpunk",
          newUsername: plan === "add-admin" ? "neuralpunk-key" : null,
          oldKey: plan === "same" ? "same" : "live", message: "",
        };
      }
      if (plan === "add-admin") {
        users.push({ userId: key.userId, shortId: key.shortId, username: "neuralpunk-key", isFounder: false, revoked: false, roles: ["admin"], createdAtMicros: now(), hardware: null });
        grants.push({ grantId: rid("grant-", 13), userId: key.userId, username: "neuralpunk-key", role: "admin", nodes: "any", services: "*", expiresAtMicros: null, revoked: false, expired: false, createdAtMicros: now() });
      }
      const moved = keyOn ? null : "~/.meshware/identity/user.key.set-aside-mwu1zmqqlw5kdeas";
      keyOn = key.userId;
      spareKept = true;
      return { identity: key.userId, movedTo: moved };
    }
    case "security_key_attest": {
      const me = users.find((u) => u.userId === (keyOn ?? FOUNDER));
      if (!me) throw "not a member";
      if (!attestRoots.some((r) => !r.removed)) throw "no trusted authority vouches for that chain";
      me.hardware = {
        kind: String(args.kind), slot: String(args.slot), rootLabel: attestRoots[0].label,
        subject: "CN=YubiKey PIV Attestation 9c, serialNumber=23847312",
        attestedAtMicros: now(), notAfterMicros: now() + 3650 * 86400e6, revoked: false, revokedReason: "",
      };
      users = [...users];
      return null;
    }
    case "security_key_withdraw": {
      const who = args.user ? String(args.user) : (keyOn ?? FOUNDER);
      const u = users.find((x) => x.userId === who);
      if (u?.hardware) u.hardware = { ...u.hardware, revoked: true, revokedReason: String(args.reason) };
      users = [...users];
      return null;
    }
    case "attest_roots_view":
      return attestRoots;
    case "attest_root_add": {
      const id = Array.from({ length: 64 }, () => "0123456789abcdef"[Math.floor(Math.random() * 16)]).join("");
      attestRoots = [...attestRoots, { id, shortId: id.slice(0, 12), label: String(args.label) || "attestation authority", addedBy: "neuralpunk", addedAtMicros: now(), removed: false, attesting: 0 }];
      return null;
    }
    case "attest_root_remove":
      attestRoots = attestRoots.map((r) => (r.id === args.id ? { ...r, removed: true } : r));
      return null;
    case "bootstrap_status":
      return { hasMesh: mockDemo || !new URLSearchParams(location.search).has("wizard"), running: true, hostname: "director", userId: FOUNDER, dataDir: "~/.meshware",
        // The guide's screenshots are of a real mesh, never a demo — but
        // `?wizard&demo` draws the fifth card so it can be shot too.
        demo: mockDemo || new URLSearchParams(location.search).has("demo"), demoAvailable: true };
    case "start_engine":
      // `?startfail` is the first-install failure DIR-11 is about: the
      // first start refuses the way a second copy holding the op log does,
      // and Retry then gets through.
      if (new URLSearchParams(location.search).has("startfail") && !mockStartFailed) {
        mockStartFailed = true;
        throw "opening the op log: Database already open. Cannot acquire lock.";
      }
      drained = false;
      return status();
    case "status_view":
      return status();
    case "nodes_view":
      // Copies, because the real boundary makes them.
      //
      // Over Tauri every call re-parses JSON, so a row the app is holding
      // and the row the next refresh returns are different objects and any
      // change shows. The mock kept mutating the same objects in place and
      // handing the same references back, so `app.nodes` changed identity
      // (the `.filter()` in `refresh`) while every row inside it did not —
      // and a control that had plainly worked, toast and all, left the
      // panel showing what it said a moment ago. That is a mock that lies
      // about a bug rather than about a feature, which is the worse
      // direction, so it copies what it hands out.
      return nodes.map((n) => ({
        ...n,
        addrs: [...n.addrs],
        tags: [...n.tags],
        assignedTags: [...n.assignedTags],
      }));
    case "top_view":
      // Re-read, not re-serve: the array is the same objects, and the poll is
      // where a real daemon's numbers would have moved too.
      const at = Date.now();
      for (let i = 4; i < top.length; i++) {
        if (top[i].noExecute) continue;
        const pct = syntheticLoad(i, at);
        top[i].cpuPct = pct;
        top[i].busy = pct / 100;
        top[i].loadPerCore = Math.round(pct * 1.4) / 100;
      }
      return top;
    case "totals_view":
      return totals();
    // The `director://` slot (L2 P5). `?mock=1&link=jobs/job-…` puts one
    // in it, which is the only way to exercise the routing table without
    // a desktop that has the scheme registered.
    case "deep_link_take": {
      const raw = new URLSearchParams(location.search).get("link");
      if (!raw || raw === deepLinkServed) return null;
      const [kind, id] = raw.split("/");
      if (!kind || !id) return null;
      deepLinkServed = raw;
      return { kind, id };
    }
    case "devices_view":
      return devices;

    // ── C0–C9 ─────────────────────────────────────────────────────────────
    case "deployments_view":
      if (FRESH !== null) return [];
      return deployments.map(({ instances: _i, reconciler: _r, updatingFrom: _u, degradedReason: _d, ...row }) => row);
    case "deployment_status": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      const { createdBy: _c, createdAtMicros: _a, source: _s, ...rest } = d;
      // No cast: `Dep` is the status shape, so a field appended on the
      // Rust side fails here at build time instead of at render time.
      const out: DeploymentStatusView = rest;
      return out;
    }
    case "deployment_rollout": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      const v = Number(args.version);
      d.service = `${d.service.split("@")[0]}@${v}`;
      d.updatingFrom = d.generation;
      d.generation += 1;
      d.status = "updating";
      return null;
    }
    case "deployment_rollback": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      // A rollback is a forward roll to an older, immutable version — one
      // service version back, or (E1) a whole kept version by generation or tag.
      const cur = Number(d.service.split("@")[1] ?? 1);
      const tagged = args.tag != null ? (mockTags[d.name] ?? []).find((t) => t.tag === args.tag)?.generation : undefined;
      const toGen = args.generation ?? tagged;
      const back = toGen != null ? Math.max(1, cur - (d.generation - Number(toGen))) : Math.max(1, cur - 1);
      d.service = `${d.service.split("@")[0]}@${back}`;
      d.updatingFrom = d.generation;
      d.generation += 1;
      d.status = "updating";
      return null;
    }
    case "deployment_promote": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      if (!d.awaitingPromotion) throw new Error(`${d.name} has nothing holding for promotion`);
      d.awaitingPromotion = false;
      d.canarying = false;
      return null;
    }
    case "deployment_set_paused": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      d.status = args.paused ? "paused" : "active";
      return null;
    }
    // Phase B, E3.
    case "deployment_scale": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      if (d.count === "system")
        throw new Error(`${d.name} runs one instance on every matching node; there is no size to scale`);
      const want = Number(args.count);
      const cur = d.instances.filter((i) => i.generation === d.generation);
      d.count = String(want);
      d.desired = want;
      if (want < cur.length) {
        // The highest slots go; the copies that stay are not touched.
        const gone = new Set(cur.slice(want));
        d.instances = d.instances.filter((i) => !gone.has(i));
      } else {
        const machines = ["anvil", "forge", "edge-7", "director"];
        for (let k = cur.length; k < want; k++) {
          const node = machines[k % machines.length];
          // Numbered like the slots already there: the engine's replicas are
          // `0`, `1`, … and the older fixtures spell them `node/n`.
          const numbered = cur.length > 0 && cur.every((i) => /^\d+$/.test(i.slot));
          const row = inst(numbered ? String(k) : `${node}/${k}`, "", d.generation, "absent", "unknown", "", null);
          d.instances.push(row);
          walkCopy(d, row, node);
        }
      }
      if (d.status === "active" || d.status === "progressing") d.status = want > cur.length ? "progressing" : "active";
      settle(d);
      audit.unshift({ auditId: rid("audit-", 20), atMicros: now(), userId: FOUNDER, username: "neuralpunk", action: "deployment.put", target: d.name, nodeShort: "mw1sdj5gterlfav", outcome: "ok", detail: `generation ${d.generation}` });
      return null;
    }
    case "deployment_restart": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      if (args.slot == null) {
        d.generation += 1;
        d.status = "updating";
      }
      return null;
    }
    // Phase B, E3/U1: a slot with a history worth showing — one incarnation
    // lost with its node, and the replacement that took over.
    case "deployment_history": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      const [svc, ver] = d.service.split("@");
      const tags = mockTags[d.name] ?? [];
      const n = Math.min(d.generation + 1, 10);
      const versions = Array.from({ length: n }, (_, i) => {
        const generation = d.generation - i;
        const version = Math.max(1, Number(ver ?? 1) - i);
        return {
          generation,
          service: `${svc}@${version}`,
          count: d.count,
          targets: d.targets,
          description: "",
          publishedBy: d.createdBy,
          publishedAtMicros: now() - (i * 2 + 1) * 86400e6,
          tags: tags.filter((t) => t.generation === generation).map((t) => t.tag),
          current: i === 0,
          keptByTag: false,
          changes:
            i === n - 1
              ? []
              : [{ field: "service", from: `"${svc}@${Math.max(1, version - 1)}"`, to: `"${svc}@${version}"`, effect: "rolls" }],
          source: `[deployment]\nname    = "${d.name}"\nservice = "${svc}@${version}"\ncount   = ${d.count === "system" ? '"system"' : d.count}\ntargets = "${d.targets}"\ngeneration = ${generation}\n`,
        };
      });
      return {
        deployment: d.name,
        versions,
        tags: tags.map((t) => ({ ...t, setBy: d.createdBy, setAtMicros: now() - 3600e6 })),
      };
    }
    case "deployment_tag": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      const list = (mockTags[d.name] = mockTags[d.name] ?? []);
      const at = list.findIndex((t) => t.tag === args.tag);
      if (args.remove) {
        if (at < 0) throw new Error(`${d.name} has no tag "${args.tag}"`);
        list.splice(at, 1);
      } else {
        const entry = { tag: String(args.tag), generation: Number(args.generation ?? d.generation), description: String(args.description ?? "") };
        if (at >= 0) list[at] = entry;
        else list.push(entry);
      }
      return null;
    }
    case "deployment_instance": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      const current =
        d.instances.find((i) => i.slot === args.slot && i.generation === d.generation) ?? null;
      const placed = current && current.state !== "absent" ? current : null;
      // One slot with a history worth showing — it failed once and was
      // started again — and every other slot with the one start it has had.
      // It used to give *every* slot that failure, so each deployment's
      // Activity read as though every copy had crashed an hour ago.
      const restarted = placed !== null && d.name === "edge-collector" && placed.slot === "edge-7/0";
      const ended = placed !== null && (placed.state === "failed" || placed.state === "timed-out");
      return {
        deployment: d.name,
        slot: String(args.slot),
        generation: d.generation,
        runtime: d.runtime,
        current,
        incarnations: placed
          ? [
              ...(restarted
                ? [{ jobId: "job-lostexample0", generation: d.generation, incarnation: 0, node: placed.node, state: "failed", createdAtMicros: now() - 3600e6, startedAtMicros: now() - 3590e6, finishedAtMicros: now() - 1800e6, exitCode: 137 }]
                : []),
              {
                jobId: placed.jobId,
                generation: placed.generation,
                incarnation: restarted ? 1 : 0,
                node: placed.node,
                state: placed.state,
                createdAtMicros: (placed.startedAtMicros ?? now()) - 2e6,
                startedAtMicros: placed.startedAtMicros,
                finishedAtMicros: ended ? now() - 60e6 : null,
                exitCode: ended ? 1 : null,
              },
            ]
          : [],
        stats: placed && placed.state === "running"
          ? { jobId: placed.jobId, cpuPct: 2 + Math.random() * 6, memBytes: 38e6 + Math.random() * 4e6, memLimitBytes: 128 * 1048576, source: "container" }
          : null,
      };
    }
    case "deployment_remove":
      deployments = deployments.filter((x) => x.name !== args.name);
      dropRegistered((i) => i.deployment === args.name);
      return null;

    // ── N1–N6: the Nomad track ────────────────────────────────────────────
    case "deployment_detail": {
      const d = deployments.find((x) => x.name === args.name);
      if (!d) throw new Error(`no deployment named ${args.name}`);
      const { instances: _i, reconciler: _r, updatingFrom: _u, degradedReason: _dr,
              canarying: _c, awaitingPromotion: _ap, stalledReason: _sr, policyBlind: _pb,
              portLabels: _pl, placement: _p, ...row } = d;
      return { ...row, source: row.source ?? DEPLOYMENT_EXAMPLE(d) };
    }
    // Phase B, E4. The real ones run mesh_core's parser; the mock only
    // needs to be shaped like it.
    case "deployment_validate": {
      const parsed = parseSpec(String(args.source ?? ""));
      if (!parsed.name) throw new Error("deployment has no `name`");
      return [
        ["name", parsed.name],
        ["runs", parsed.image ? `image ${parsed.image}` : parsed.service || "(nothing)"],
        ["count", parsed.count || "1"],
        ["targets", parsed.targets || "any"],
        ["failover", "If a node running an instance goes away, the instance is replaced on another node after 1 min unreachable."],
      ] as [string, string][];
    }
    case "deployment_example":
      return `deployment:\n  name: ${String(args.name ?? "web")}\n  count: 2\n  container:\n    image: ${String(args.image ?? "docker.io/library/nginx:alpine")}\n`;
    case "deployment_schema":
      return '{ "$schema": "https://json-schema.org/draft/2020-12/schema", "title": "meshware deployment" }';
    case "deployment_put": {
      const parsed = parseSpec(String(args.source ?? ""));
      // `[deployment.container]` (N6): a file that carries an image names
      // no service, because it *is* one — meshware derives a service named
      // after the deployment. The mock has to know, or the plan reports the
      // very service it is about to create as missing.
      const inlineImage = parsed.image ?? null;
      if (inlineImage) parsed.service = `${parsed.name}@1`;
      if (args.dryRun) {
        const issues: string[] = [];
        if (!parsed.service && !inlineImage)
          issues.push(
            "this deployment says nothing about what to run. Either name a published service (`service = \"api@3\"`) or describe an image inline:\n\n  [deployment.container]\n  image = \"nginx:alpine\"",
          );
        if (!parsed.name) issues.push("no `name`");
        const check: DeploymentCheckView = {
          name: parsed.name || "(unnamed)",
          service: parsed.service || "(none)",
          count: parsed.count || "1",
          targets: parsed.targets || "any",
          matchingNodes: parsed.targets === "tag=role=edge" ? 1 : 4,
          health: parsed.health ?? null,
          issues,
          placement: null,
          availability: "If a node running an instance goes away, the instance is replaced on another node after 1 min unreachable.",
          // E5: a mock has no published record to diff against beyond the
          // list, so it reports the one distinction it can make honestly.
          changes: [],
          rolls: false,
          firstPublish: !deployments.some((d) => d.name === parsed.name),
        };
        return check;
      }
      if (!parsed.name) throw new Error("no `name` in the definition");
      const existing = deployments.find((x) => x.name === parsed.name);
      if (existing) {
        existing.service = parsed.service || existing.service;
        existing.count = parsed.count || existing.count;
        existing.targets = parsed.targets || existing.targets;
        existing.source = String(args.source ?? "");
        existing.generation += 1;
        existing.status = "updating";
      } else {
        deployments = [
          ...deployments,
          {
            name: parsed.name, service: parsed.service || "unknown@1",
            count: parsed.count || "1", targets: parsed.targets || "any",
            generation: 0, status: "progressing",
            desired: Number(parsed.count) || 1, running: 0, healthy: 0,
            createdBy: "neuralpunk", createdAtMicros: now(),
            source: String(args.source ?? ""),
            reconciler: "director", updatingFrom: null, degradedReason: null,
            canarying: false, awaitingPromotion: false, stalledReason: null, policyBlind: [],
            portLabels: [], placement: null, unreachable: 0, runtime: "container", availability: "If a node running an instance goes away, the instance is replaced on another node after 1 min unreachable.", instances: [],
          },
        ];
      }
      return null;
    }
    case "read_spec_file":
      // The picker hands back a token; in the browser there is no file
      // behind it, so this answers with something shaped like one rather
      // than failing — the point of the mock is the flow.
      return `# loaded from ${mockPicks.get(String(args.token)) ?? "a picked file"}\n${DEPLOYMENT_EXAMPLE()}`;

    case "schedules_view":
      return schedules.map(({ source: _s, ...row }) => ({ ...row, source: null }));
    case "schedule_detail": {
      const r = schedules.find((x) => x.name === args.name);
      if (!r) throw new Error(`no schedule named ${args.name}`);
      return r;
    }
    case "schedule_put": {
      const parsed = parseSpec(String(args.source ?? ""));
      if (args.dryRun) {
        const issues: string[] = [];
        if (!parsed.service) issues.push("no `service` — a schedule must name one");
        if (!parsed.at && !parsed.every) issues.push("no `at` or `every` — when should it run?");
        const check: ServiceScheduleCheckView = {
          name: parsed.name || "(unnamed)",
          service: parsed.service || "(none)",
          targets: parsed.targets || "any",
          when: parsed.at ? `${parsed.at}${parsed.days ? ` ${parsed.days}` : ""}` : `every ${parsed.every ?? "?"}`,
          matchingNodes: 4,
          nextMicros: issues.length ? null : now() + 3600e6,
          issues,
        };
        return check;
      }
      if (!parsed.name) throw new Error("no `name` in the definition");
      const when = parsed.at ? `${parsed.at}${parsed.days ? ` ${parsed.days}` : ""}` : `every ${parsed.every ?? "1h"}`;
      const row: ServiceScheduleView = {
        name: parsed.name, service: parsed.service || "unknown@1",
        targets: parsed.targets || "any", when, status: "active",
        nextMicros: now() + 3600e6, lastMicros: null, firer: "director",
        createdBy: "neuralpunk", createdAtMicros: now(),
        source: String(args.source ?? ""),
      };
      const at = schedules.findIndex((x) => x.name === row.name);
      if (at >= 0) schedules[at] = row;
      else schedules = [...schedules, row];
      return null;
    }
    case "schedule_set_paused": {
      const r = schedules.find((x) => x.name === args.name);
      if (!r) throw new Error(`no schedule named ${args.name}`);
      r.status = args.paused ? "paused" : "active";
      return null;
    }
    case "schedule_remove":
      schedules = schedules.filter((x) => x.name !== args.name);
      return null;

    case "ports_view":
      return portLedger;

    case "runtimes_view":
      return runtimesView;

    // Phase C, U8: every current-generation instance, on the machine
    // running it, with made-up requests — two deployments that asked and
    // one that did not, so both kinds of block are on screen.
    case "placement_view": {
      const asked: Record<string, [number, number]> = {
        "edge-collector": [1, 256],
        "metrics-relay": [2, 2048],
      };
      const out: PlacementNodeView[] = nodes.map((n) => {
          const drain = drainOf(n.tags);
          return {
            nodeId: n.nodeId,
            name: n.name,
            liveness: n.liveness,
            cores: n.capacity?.cores ?? 0,
            memMb: n.capacity?.memMb ?? 0,
            eligible: n.executing && n.contributes && drain === null,
            draining: drain !== null,
            leaving: drain?.leaving ?? false,
            drainDeadlineSecs: drain?.deadlineSecs ?? null,
            instances: deployments.flatMap((d) =>
              d.instances
                .filter((i) => i.node === n.name && i.generation === d.generation && i.state !== "failed")
                .map((i) => ({
                  deployment: d.name,
                  slot: i.slot.split("/").pop() ?? i.slot,
                  generation: i.generation,
                  state: i.state,
                  health: i.health,
                  cpus: asked[d.name]?.[0] ?? 0,
                  memMb: asked[d.name]?.[1] ?? 0,
                })),
            ),
          };
        });
      return out;
    }

    // Phase C, U9: the same two assigned tags the engine writes.
    case "node_drain_set": {
      const n = nodes.find((x) => x.nodeId === args.nodeId);
      if (!n) throw new Error("no such node");
      const announced = n.tags.filter((t) => !n.assignedTags.includes(t));
      const kept = n.assignedTags.filter((t) => !isDrainTag(t));
      const added = args.lift
        ? []
        : [
            args.deadlineSecs == null
              ? "drain.by=never"
              : `drain.by=${Math.floor(Date.now() / 1000) + Number(args.deadlineSecs)}`,
            ...(args.keepSystem ? ["drain.keep-system"] : []),
          ];
      n.assignedTags = [...kept, ...added];
      n.tags = [...announced, ...n.assignedTags.filter((t) => !announced.includes(t))];
      return null;
    }

    case "registry_view":
      return registryRows;
    case "registry_service":
      return registryInstances[String(args.service)] ?? [];
    case "dns_status":
      return dnsStatus;
    case "volumes_view":
      return volumes;
    case "volume_create": {
      const name = String(args.name ?? "").trim();
      if (!/^[A-Za-z0-9][A-Za-z0-9_-]{0,47}$/.test(name))
        throw new Error(`${JSON.stringify(name)} must be 1-48 letters, digits, '-' or '_', starting with a letter or digit`);
      if (volumes.some((v) => v.name === name))
        throw new Error(`this node already lends ${JSON.stringify(name)} — \`meshnode volumes\` says where it came from`);
      for (const line of String(args.params ?? "").split("\n").map((l) => l.trim()).filter(Boolean)) {
        if (!line.includes("=")) throw new Error(`${JSON.stringify(line)} is not key=value`);
        // Shell metacharacters, refused on this side too so the mock
        // teaches the same lesson the engine does.
        if (/[;&|$`<>'"\\*?(){}\[\]!]/.test(line))
          throw new Error(`volume parameter ${JSON.stringify(line)} contains a character meshware will not hand a plugin`);
      }
      const row: VolumeRowView = {
        name, node: "director",
        path: `/srv/meshware/volumes/${name}`,
        readOnly: Boolean(args.readOnly), source: "meshware-mkdir",
        holders: [], writeHeld: false, writableNow: !args.readOnly,
      };
      volumes = [...volumes, row];
      return row;
    }
    case "volume_remove": {
      const v = volumes.find((x) => x.name === args.name);
      if (!v) throw new Error(`this node did not provision ${JSON.stringify(args.name)}. A [[volume]] block is your own line in your own meshnode.toml`);
      if (v.source === "static")
        throw new Error(`this node did not provision ${JSON.stringify(args.name)}. A [[volume]] block is your own line in your own meshnode.toml — remove it there and reload; nothing here edits that file.`);
      if (v.holders.length > 0 && !args.force)
        throw new Error(`${JSON.stringify(v.name)} is held by ${v.holders.join(", ")} — a lease is the authority on whether anything is using the bytes, and --force does not override it`);
      volumes = volumes.filter((x) => x.name !== v.name);
      return `${JSON.stringify(v.name)} is no longer offered`;
    }

    case "shed_view":
      return shed;
    case "shed_set": {
      const submissions = Boolean(args.submissions);
      const reconcile = Boolean(args.reconcile);
      const on = submissions || reconcile;
      shed = {
        submissions, reconcile,
        reason: on ? String(args.reason ?? "").trim() : "",
        byName: on ? "neuralpunk" : null,
        atMicros: on ? now() : 0,
        // One node on an older build, so the panel shows the warning that
        // is the whole reason this readout exists — the only capability
        // gate in meshware that fails *open*.
        members: 5, honouring: 4, behind: on ? ["edge-7"] : [],
        warning: on
          ? "4 of 5 nodes will honour this shed. 1 will keep accepting work: edge-7. Unlike every other capability gate in meshware, this one fails *open* — upgrade them or drain them."
          : null,
      };
      return shed;
    }

    case "gangs_view":
      if (FRESH !== null) return [];
      return gangRows.filter((g) => args.all || g.state === "offering" || g.state === "running");
    case "queue_view":
      return queueRows;

    case "cache_stats":
      return cacheStats;
    // The two facts the real command exists to supply — which binary is the
    // wrapper, and which mesh home it talks to — are the two the frontend
    // cannot know. The mock stands in for a Director installed at the usual
    // place so the panel can be worked on without one.
    case "farm_build_line": {
      const dir = String(args.dir).replace(/^~/, "/home/you");
      const command = String(args.command).trim();
      const wrapper = "/usr/bin/meshware-director";
      const home = "/home/you/.meshware/director";
      const q = (v: string) => `'${v.replace(/'/g, "'\\''")}'`;
      const cc = `${wrapper} cc gcc`;
      const cxx = `${wrapper} cc g++`;
      const makeStyle = ["make", "gmake", "bmake"].includes(command.split(/\s+/)[0] ?? "");
      let line =
        `cd ${q(dir)} && export MESHWARE_HOME=${q(home)} CC=${q(cc)} CXX=${q(cxx)} && ${command}`;
      if (makeStyle) line += ` CC=${q(cc)} CXX=${q(cxx)}`;
      return {
        line,
        wrapper,
        home,
        makeStyle,
        warnings: makeStyle
          ? []
          : [
              "CC and CXX are exported for this command rather than passed as arguments — a build that hardcodes its compiler will ignore them",
            ],
      };
    }
    case "cache_purge": {
      const forgotten = cacheStats.entries;
      cacheStats = { ...cacheStats, entries: 0, byService: cacheStats.byService.map((r) => ({ ...r, entries: 0 })) };
      return `forgot ${forgotten} entries`;
    }

    case "job_explain": {
      const j = jobs.find((x) => x.jobId === args.job || x.shortId === args.job) ?? jobs[0];
      // Two shapes worth photographing: a placement breakdown, and a cache
      // hit — which has no placement at all, because nothing was placed.
      const isHit = jobs.indexOf(j) === 3;
      return {
        job: j.jobId,
        shortId: j.shortId,
        cmdPreview: j.cmdPreview,
        state: j.state,
        executorName: j.executorName,
        durationMicros: j.finishedAtMicros ? j.finishedAtMicros - j.createdAtMicros : null,
        placement: isHit ? null : mkPlacement(j.executorName ?? "forge"),
        placementOn: null,
        // Only work whose author claimed it is hermetic gets a key. An
        // ad-hoc command is not cacheable and never derives one, which is
        // what makes the Action key tab conditional rather than empty.
        actionKey:
          j.runtime === "python"
            ? "act-7c1f9b40e2a8d5309e6a1b8c4f27"
            : null,
        cacheHit: isHit
          ? { key: "act-7c1f9b40e2a8d5309e6a1b8c4f27", shortKey: "act-7c1f9b40e2a8", producedBy: jobs[jobs.length - 1].jobId, producer: "anvil", bytesSaved: 184_549_376, verified: true }
          : null,
        keyFields: KEY_FIELDS.map((f) => ({ ...f, differs: args.compare ? f.differs : false })),
        // `mesh_proto::key_why()`'s sentence (L2 P4).
        keyWhy: `${KEY_FIELDS.length} fields make this key, and two runs whose keys match are the same work. Compare it against another run's key to see which field made them differ.`,
      } as JobExplainView;
    }
    case "plan_view": {
      const on = (args.on as string | null) ?? "any";
      if (on === "tag=nope") {
        return { placement: null, chosenName: null, slotsFree: 0, slotsWanted: 1, waitSecs: 0, duration: null, finish: null, samples: null, delays: [], refusal: "no live node matches selector Tag(\"nope\")", durationWhy: null } as PlanView;
      }
      return {
        placement: mkPlacement("forge"),
        chosenName: "forge",
        slotsFree: 23,
        slotsWanted: 1,
        waitSecs: 0,
        duration: "4m–11m, p50 6m",
        finish: "4m–11m from now, p50 6m",
        samples: 214,
        delays: [
          { name: "mia", jobs: 2, bySecs: 360 },
          { name: "lab-op", jobs: 1, bySecs: 90 },
        ],
        // `DurationRange::why()`'s sentence (L2 P4).
        durationWhy:
          "4m–11m, p50 6m — from 214 past runs. A range, never a time: an estimate that names one number is wrong often enough to stop being read.",
      } as PlanView;
    }

    case "job_logs_stop":
      return undefined;
    case "state_versions_view": {
      // The mock mutates its arrays in place (a mock run pushes a job),
      // with no op-log to notice it. Reporting "everything just changed"
      // keeps the dev surface behaving exactly as it did before the
      // change signal existed (H-8) — the alternative is a mock that
      // silently stops updating, which would read as a bug in the app.
      mockTick += 1;
      const topics: Record<string, number> = {};
      for (const t of MOCK_TOPICS) topics[t] = mockTick;
      return { topics };
    }
    case "jobs_view": {
      // Mirrors the engine's `JobsQuery` (**H-1**): filter, then page,
      // and count over everything regardless of the filter.
      const want = (args.filter as string | null) ?? null;
      const needle = ((args.contains as string | null) ?? "").toLowerCase();
      const limit = (args.limit as number | null) || 200;
      const offset = (args.offset as number | null) || 0;
      const live = (st: string) => st === "queued" || st === "running";
      const matches = (j: JobView) => {
        if (want && want !== "all") {
          const ok =
            want === "live" || want === "active"
              ? live(j.state)
              : want === "terminal" || want === "finished"
                ? !live(j.state)
                : j.state === want;
          if (!ok) return false;
        }
        if (needle && !j.cmdPreview.toLowerCase().includes(needle)) return false;
        return true;
      };
      const counts = {
        all: jobs.length,
        queued: jobs.filter((j) => j.state === "queued").length,
        running: jobs.filter((j) => j.state === "running").length,
        succeeded: jobs.filter((j) => j.state === "succeeded").length,
        failed: jobs.filter((j) => j.state === "failed").length,
        cancelled: jobs.filter((j) => j.state === "cancelled").length,
        timedOut: jobs.filter((j) => j.state === "timed-out").length,
      };
      const hits = jobs
        .filter(matches)
        .slice()
        .sort((a, b) => b.createdAtMicros - a.createdAtMicros);
      return {
        rows: hits.slice(offset, offset + limit),
        total: hits.length,
        counts,
      };
    }
    case "services_view":
      return services;
    case "bundles_view":
      return bundles;
    case "sources_view":
      return sources;
    case "secrets_view":
      return secrets;
    case "secret_providers_view":
      return [];
    case "users_view":
      return users;
    case "grants_view":
      return grants;
    // Newest first, as the engine answers (UX-18). The fixture is written
    // in story order, and handing it back unsorted put one day in two
    // places — which Access › History's keyed day list threw on.
    case "audit_query":
      return audit
        .filter((a) => !args.user || a.username === args.user)
        .filter((a) => !args.action || a.action.startsWith(String(args.action)))
        .sort((a, b) => b.atMicros - a.atMicros);
    case "whoami_view":
      return whoami();
    case "enrolments_view":
      return enrolments;
    case "enrol_approve": {
      const row = enrolments.find((e) => e.userId === args.user);
      if (row) {
        users = [
          ...users,
          {
            userId: row.userId,
            shortId: row.shortId,
            username: String(args.username),
            isFounder: false,
            revoked: false,
            roles: [String(args.role)],
            createdAtMicros: Date.now() * 1000,
            hardware: null,
          },
        ];
        enrolments = enrolments.filter((e) => e.userId !== row.userId);
      }
      return null;
    }
    case "enrol_attach": {
      const row = enrolments.find((e) => e.userId === args.user);
      const existing = users.find((u) => u.username === String(args.username) && !u.revoked);
      if (!row) throw "no such request";
      if (!existing) throw `no user is called "${String(args.username)}"`;
      const inherited = grants.filter((g) => g.username === existing.username && !g.revoked && !g.expired);
      users = [
        ...users,
        {
          userId: row.userId,
          shortId: row.shortId,
          username: existing.username,
          isFounder: false,
          revoked: false,
          roles: [...new Set(inherited.map((g) => g.role))],
          createdAtMicros: now(),
          hardware: null,
        },
      ];
      for (const g of inherited) {
        grants.push({ ...g, grantId: rid("grant-", 13), userId: row.userId, createdAtMicros: now() });
      }
      enrolments = enrolments.filter((e) => e.userId !== row.userId);
      return inherited.length;
    }
    case "enrol_decline":
      enrolments = enrolments.map((e) =>
        e.userId === args.user ? { ...e, declined: true, declinedBy: "neuralpunk" } : e,
      );
      return null;
    case "enrol_request":
      if (mockEnrolment === "none") mockEnrolment = "pending";
      return "BRAVE-OTTER-4417";
    case "definitions_view":
      return definitions();
    case "collector_widen":
      return null;
    case "bundle_prefetch": {
      // The mock pushes to whatever the selector matches, one frame each,
      // so the panel can be worked on without a fleet.
      const ch = args.channel as { onmessage?: (m: unknown) => void } | undefined;
      const targets = matchSelector(String(args.selector ?? "any"));
      const size = bundles.find((b) => b.bundleId === args.bundle)?.totalSize ?? 1024;
      setTimeout(() => {
        for (const n of targets) {
          ch?.onmessage?.({
            type: "node",
            node: n.name,
            haveChunks: 4,
            totalChunks: 4,
            haveBytes: size,
            totalBytes: size,
            complete: true,
            unreachable: null,
          });
        }
        ch?.onmessage?.({ type: "done", complete: targets.length, fetching: 0, skipped: 0 });
      }, 300);
      return null;
    }
    // -- releases: the channel, and getting one into the mesh ------------------
    //
    // The channel is one tag ahead of the mesh, so the Updates page shows
    // every step of the path: get it, then update. Fetching "publishes" it.
    case "release_check":
      return {
        directorVersion: V.current,
        fleetNewest: V.current,
        published: publishedReleases.map((version) => ({ version, arches: ["x86_64", "aarch64"] })),
        channelLatest: V.next,
        channelError: null,
        canVerify: releaseKeys.some((k) => !k.revoked),
        verifyNote: releaseKeys.some((k) => !k.revoked) ? null : "trust a release key before getting a release",
        fleetArches: ["x86_64", "aarch64"],
      };
    case "release_fetch": {
      const ch = args.channel as ChannelLike | undefined;
      const version = String(args.version ?? V.next);
      const arches = (args.arches as string[] | undefined)?.length ? (args.arches as string[]) : ["x86_64", "aarch64"];
      void (async () => {
        for (const stage of ["downloading", "verifying", "published"]) {
          await new Promise((r) => setTimeout(r, 350));
          for (const arch of arches) ch?.onmessage({ type: "progress", arch, stage, detail: stage === "published" ? "in the mesh" : `${stage} meshnode-v${version}-${arch}-unknown-linux-musl` });
        }
        publishedReleases = [version, ...publishedReleases.filter((v) => v !== version)];
        ch?.onmessage({ type: "done", version, arches, note: "" });
      })();
      return null;
    }
    // -- releases: trust, and publishing a build (M24 §9) ----------------------
    case "release_trust": {
      // Y6: trusting an SSH-form key (`sk-ssh-ed25519@openssh.com …`) turns
      // co-signatures on, and every published binary shows its standing.
      const ssh = releaseKeys.find((k) => !k.revoked && /^(sk-)?ssh-/.test(k.pubkey));
      return {
        keys: releaseKeys,
        localOverride: null,
        embeddedIsPlaceholder: true,
        nothingWouldVerify: releaseKeys.every((k) => k.revoked),
        cosignRequired: !!ssh,
        releases: publishedReleases.flatMap((version) =>
          ["x86_64", "aarch64"].map((arch) => {
            const digest = `${version}-${arch}`.padEnd(64, "0").replace(/[^0-9a-f]/g, "0");
            return { version, arch, digest, cosignedBy: mockCosigned.has(digest) ? (ssh?.label ?? "release key") : null, userPresent: mockCosigned.has(digest) };
          }),
        ),
      };
    }
    case "release_cosign":
      mockCosigned.add(String(args.digest));
      return "Co-signed by a security key, with a touch recorded.";
    case "release_key_trust": {
      const raw = String(args.source ?? "").trim();
      const line =
        raw
          .split("\n")
          .map((l) => l.trim())
          .filter((l) => l && !l.startsWith("untrusted comment"))
          .at(-1) ?? "";
      releaseKeys = [
        {
          pubkey: line || "RWTn2xY8kLpQ4vC7dR1sW9eF6gH3jK0mN5bV8xZ2aS4dF7gH1jK3lM6n",
          label: String(args.label ?? "") || "release signing key",
          addedBy: "you",
          addedAtMicros: String(now()),
          revoked: false,
          inUseHere: true,
        },
        ...releaseKeys,
      ];
      return null;
    }
    case "release_key_revoke": {
      const key = String(args.pubkey ?? "");
      releaseKeys = releaseKeys.map((k) =>
        k.pubkey === key ? { ...k, revoked: true, inUseHere: false } : k,
      );
      return null;
    }
    case "release_versions":
      return [...publishedReleases];
    case "release_candidates":
      if (args.token) mockBuildDir = mockPicks.get(String(args.token)) ?? mockBuildDir;
      if (!mockBuildDir) return [];
      return releaseKeys.some((k) => !k.revoked)
        ? builds
        : builds.map((b) => ({
            ...b,
            verified: false,
            blocked:
              "this mesh trusts no release key yet — add one above, then this becomes publishable.",
          }));
    case "release_publish": {
      const version = String(args.version ?? "");
      publishedReleases = [version, ...publishedReleases.filter((v) => v !== version)];
      builds = builds.map((b) =>
        b.version === version && b.verified
          ? {
              ...b,
              published: true,
              blocked: `${version} is already published in this mesh — a version names one binary for the life of the mesh.`,
            }
          : b,
      );
      return `published meshnode ${version} into the mesh (bndl-8136a590e3d9, 21 MB) — nodes fetch it from each other`;
    }
    // The SSH rollout, at fleet size — the whole point of the mock. A
    // handful of machines never exercises the run view; sixty does, and
    // the shape that matters is the one with two failures buried in it.
    case "upgrade_ssh_start": {
      const ch = args.channel as ChannelLike | undefined;
      const draft = args.draft as {
        nodes: { nodeId: string; host: string | null }[];
        version: string | null;
        parallel: number;
        port: number;
        force: boolean;
      };
      const picked = draft.nodes
        .map((d) => ({ d, n: nodes.find((x) => x.nodeId === d.nodeId) }))
        .filter((x) => x.n) as { d: { nodeId: string; host: string | null }; n: NodeView }[];
      const version = draft.version ?? V.current;
      const hosts = picked.map(({ d, n }) => `${n.nodeId}|${d.host ?? "10.0.0.1"}:${draft.port}`);
      const parallel = Math.max(1, Math.min(32, draft.parallel || 4));
      void (async () => {
        ch?.onmessage({ type: "plan", runId: 1, hosts });
        let done = 0;
        let failed = 0;
        let cursor = 0;
        const STAGES: [string, string][] = [
          ["connecting", "opening a session"],
          ["inspecting", "looking at the machine"],
          ["preparing", `meshnode ${version} · release published in this mesh`],
          ["installing", `sending meshnode ${version} (26.4 MB)`],
          ["starting", "swapping the binary and restarting"],
          ["verifying", "waiting for it to report in"],
        ];
        const one = async (i: number) => {
          const { n } = picked[i];
          const host = hosts[i];
          for (const [stage, detail] of STAGES) {
            ch?.onmessage({ type: "stage", host, stage, detail });
            await sleep(200 + Math.random() * 260);
          }
          // A couple of real-looking failures, because a run view that has
          // only ever been seen succeeding has not been designed for the
          // moment anybody actually reads it.
          if (i % 19 === 7) {
            failed++;
            ch?.onmessage({
              type: "failed", host, stage: "starting",
              message: `${n.name} is owned by dpkg, so the package manager upgrades it`,
              hint: "On that machine: apt install meshnode, then systemctl restart meshnode.",
            });
            return;
          }
          if (i % 23 === 11) {
            failed++;
            ch?.onmessage({
              type: "failed", host, stage: "connecting",
              message: "connection refused after 10s", hint: "Check the SSH port and that sshd is up there.",
            });
            return;
          }
          done++;
          ch?.onmessage({
            type: "done", host, nodeName: n.name, nodeId: n.nodeId, version,
            service: "systemd",
            note: n.version === version && !draft.force ? `already on ${version}` : null,
          });
        };
        const lanes = Array.from({ length: Math.min(parallel, picked.length) }, async () => {
          for (;;) {
            const i = cursor++;
            if (i >= picked.length) return;
            await one(i);
          }
        });
        await Promise.all(lanes);
        ch?.onmessage({ type: "finished", done, failed, cancelled: false });
      })();
      return 1;
    }

    case "deploy_cancel":
      return null;

    case "upgrade_fleet": {
      const ch = args.channel as ChannelLike | undefined;
      const sel = String(args.selector ?? "");
      const version = String(args.version ?? "");
      const batch = Math.max(1, Math.min(64, Number(args.batch) || 1));
      const force = !!args.force;
      const targets = matchSelector(sel);
      void (async () => {
        let upgraded = 0;
        let already = 0;
        for (let i = 0; i < targets.length; i += batch) {
          const wave = targets.slice(i, i + batch);
          if (wave.length > 1) {
            ch?.onmessage({
              type: "node",
              node: `wave of ${wave.length}`,
              state: "wave",
              detail: wave.map((n) => n.name).join(", "),
            });
          }
          for (const n of wave) {
            if (n.version === version && !force) {
              already++;
              ch?.onmessage({ type: "node", node: n.name, state: "already", detail: `already runs ${version}` });
              continue;
            }
            ch?.onmessage({
              type: "node",
              node: n.name,
              state: "asked",
              detail: `fetching and verifying ${version}, then draining`,
            });
          }
          await sleep(900);
          for (const n of wave) {
            if (n.version === version && !force) continue;
            n.version = version;
            upgraded++;
            ch?.onmessage({
              type: "node",
              node: n.name,
              state: "back",
              detail: `runs ${version}, alive, and caught up`,
            });
          }
        }
        ch?.onmessage({
          type: "done",
          upgraded,
          already,
          driverRestarting: false,
          remaining: 0,
          stoppedAt: null,
        });
      })();
      return null;
    }
    case "target_preview":
      return matchSelector(String(args.selector ?? "any"));
    case "set_contribute":
      executing = Boolean(args.contribute);
      nodes[0].executing = executing;
      nodes[0].tags = executing ? nodes[0].tags.filter((t) => t !== "no-execute") : [...nodes[0].tags, "no-execute"];
      return status();
    case "tags_update": {
      const add = (args.add as string[]) ?? [];
      const rm = (args.remove as string[]) ?? [];
      nodes[0].tags = [...nodes[0].tags.filter((t) => !rm.includes(t)), ...add.filter((t) => !nodes[0].tags.includes(t))];
      return status();
    }
    case "ping_node":
      return { peerNodeId: nodes[1].nodeId, peerShort: nodes[1].shortId, peerNodeName: "forge", peerMeshName: "hyacinth", sameMesh: true, rttMicros: 812, via: nodes[1].addrs[0] ?? "192.168.2.105:7777" };
    case "run_command":
      void streamRun(args, args.channel as ChannelLike, String(args.command).split("\n")[0]);
      return null;
    case "run_service":
      void streamRun(args, args.channel as ChannelLike, `${args.name} (${services.find((s) => s.name === args.name)?.runtime ?? "bash"}/…)`);
      return null;
    case "parallel_run":
      void streamParallel(args, args.channel as ChannelLike);
      return null;
    case "job_logs":
      void streamLogs(args, args.channel as ChannelLike);
      return null;
    case "job_cancel": {
      const j = jobs.find((x) => x.jobId === args.jobId);
      if (j) {
        j.state = "cancelled";
        j.finishedAtMicros = now();
      }
      dropRegistered((i) => i.instance === args.jobId);
      return "cancel requested — the executor will report it";
    }
    case "jobs_clear": {
      // The same discriminator the engine uses: finished *before* now, so
      // anything still running survives its own clear.
      const at = now();
      const gone = jobs.filter(
        (x) => x.state !== "queued" && x.state !== "running" && (x.finishedAtMicros ?? at) < at,
      );
      for (const j of gone) jobs.splice(jobs.indexOf(j), 1);
      return `cleared ${gone.length} finished job${gone.length === 1 ? "" : "s"}; running and queued work is untouched`;
    }
    // What the engine does with a pick (DIR-7): a folder is its tree, a
    // single file is a bundle of that file. This used to answer three files
    // for anything at all — which is how the real backend refusing a
    // dropped file went unnoticed in the browser — and then could not list
    // them, so nothing after the push could be tried here either.
    case "push_bundle": {
      const path = mockPicks.get(String(args.token));
      if (!path) throw "that file is no longer selected — pick it again";
      const id = rid("bndl-", 64);
      const name = path.split("/").at(-1) ?? path;
      const single = mockPickIsFile(path);
      const files = single
        ? [fileRow(name, 684, true)]
        : [fileRow("scripts/hello.sh", 228, true), fileRow("scripts/rotate-logs.sh", 911, true), fileRow("README.md", 1200, false)];
      bundleFiles[id] = files;
      if (single) {
        bundleTexts[`${id}:${name}`] = [
          "#!/usr/bin/env bash",
          `#meshware: name ${name.replace(/\.[^.]+$/, "")}`,
          "#meshware: about Rotate and compress one machine's logs",
          '#meshware: arg keep_days:int=14   "Days of logs to keep"',
          'find /var/log -name "*.log" -mtime +"${IN_keep_days:-14}" -delete',
          "",
        ].join("\n");
      }
      const totalSize = files.reduce((n, f) => n + f.size, 0);
      bundles.unshift({ bundleId: id, shortId: id.slice(0, 17), totalSize, fileCount: files.length, chunkCount: files.length, holders: 1, local: true });
      return { bundleId: id, shortId: id.slice(0, 17), fileCount: files.length, chunkCount: files.length, totalSize };
    }
    case "bundle_files": {
      const files = bundleFiles[String(args.bundleId)];
      if (!files) throw `no reachable holder for bundle ${String(args.bundleId).slice(0, 17)}`;
      return files;
    }
    case "script_inspect": {
      // R2 P4/P5: what a script says about itself, from the two places it
      // can say it. A small reader over the mock's own files rather than a
      // canned answer for one filename — the wizard shows a different thing
      // for a definitions file, a header, and a guess, and only one of the
      // three gets designed if the other two are never returned here.
      const id = String(args.bundleId);
      const path = String(args.path);
      const defsPath = `${path}.definitions`;
      const source = bundleTexts[`${id}:${path}`] ?? "";
      const definitions = bundleTexts[`${id}:${defsPath}`];
      return mockScriptSpec(source, definitions, defsPath);
    }
    case "script_definitions_preview": {
      const text = String(args.definitions);
      const spec = mockScriptSpec("", text, "");
      if (spec.error) throw spec.error;
      return spec;
    }
    case "script_definitions_save": {
      const text = String(args.definitions);
      try {
        JSON.parse(text);
      } catch (e) {
        throw `not valid JSON: ${e instanceof Error ? e.message : String(e)}`;
      }
      // Nothing to write to in a browser; the answer is the path it would
      // have taken, which is what the caller shows.
      return `/home/you/ops-scripts/${String(args.entry)}.definitions`;
    }
    case "service_create": {
      const name = String(args.name);
      const existing = services.find((s) => s.name === name);
      const version = existing ? Math.max(...existing.versions) + 1 : 1;
      const b = bundles.find((x) => x.bundleId === args.bundleId);
      if (!b) throw `unknown bundle ${String(args.bundleId).slice(0, 17)} — push it first`;
      const row: ServiceView = { name, runtime: String(args.runtime), version, bundleId: b.bundleId, bundleShort: b.shortId, workingDir: String(args.workingDir), entrypoint: String(args.entrypoint), description: String(args.description), tags: args.tags as string[], versions: existing ? [...existing.versions, version] : [1], hasContract: !!args.contract, contract: (args.contract as string) ?? null };
      if (existing) Object.assign(existing, row);
      else services.push(row);
      return version;
    }
    case "service_rollback": {
      const s = services.find((x) => x.name === args.name);
      if (s) s.version = Number(args.version);
      return null;
    }
    case "service_remove": {
      const i = services.findIndex((x) => x.name === args.name);
      if (i >= 0) services.splice(i, 1);
      return null;
    }
    case "source_add":
      sources.push({ name: String(args.name), url: String(args.url), branch: String(args.branch), intervalSecs: Number(args.intervalSecs), bump: args.bump as string[], createdByName: "neuralpunk", lastCommit: null, lastBundle: null, lastBundleShort: null, lastSyncedAtMicros: null, watcher: "anvil" });
      return null;
    case "source_remove": {
      const i = sources.findIndex((x) => x.name === args.name);
      if (i >= 0) sources.splice(i, 1);
      return null;
    }
    case "source_sync": {
      const s = sources.find((x) => x.name === args.name);
      if (s) {
        s.lastCommit = rid("", 40).replace(/[^a-f0-9]/g, "c");
        s.lastSyncedAtMicros = now();
        s.lastBundle = bundles[1].bundleId;
        s.lastBundleShort = bundles[1].shortId;
      }
      return { commit: s?.lastCommit ?? "deadbeef", bundleId: bundles[1].bundleId, bundleShort: bundles[1].shortId, unchanged: false, bumped: (s?.bump ?? []).map((svc) => ({ service: svc, version: 4 })) };
    }
    case "secret_set": {
      const name = String(args.name);
      const cur = secrets.find((s) => s.name === name);
      const version = (cur?.version ?? 0) + 1;
      secrets = [...secrets.filter((s) => s.name !== name), { name, version, createdByName: "neuralpunk", createdAtMicros: now(), wrappedFor: 5, members: 5, removed: false }];
      return { name, version, recipients: 5 };
    }
    case "secret_remove":
      secrets = secrets.filter((s) => s.name !== args.name);
      return null;
    case "user_add": {
      const id = rid("mwu1", 52);
      users.push({ userId: id, shortId: id.slice(0, 16), username: String(args.username), isFounder: false, revoked: false, roles: [String(args.role)], createdAtMicros: now(), hardware: null });
      grants.push({ grantId: rid("grant-", 13), userId: id, username: String(args.username), role: String(args.role), nodes: String(args.nodes), services: String(args.services), expiresAtMicros: args.expiresSecs ? now() + Number(args.expiresSecs) * 1e6 : null, revoked: false, expired: false, createdAtMicros: now() });
      return null;
    }
    case "user_revoke": {
      const u = users.find((x) => x.userId === args.user || x.username === args.user);
      if (u) {
        u.revoked = true;
        u.roles = [];
      }
      return null;
    }
    case "grant_add": {
      const u = users.find((x) => x.username === args.user || x.userId === args.user);
      if (!u) throw `no user matches "${String(args.user)}"`;
      const id = rid("grant-", 13);
      grants.push({ grantId: id, userId: u.userId, username: u.username, role: String(args.role), nodes: String(args.nodes), services: String(args.services), expiresAtMicros: args.expiresSecs ? now() + Number(args.expiresSecs) * 1e6 : null, revoked: false, expired: false, createdAtMicros: now() });
      if (!u.roles.includes(String(args.role))) u.roles.push(String(args.role));
      return id;
    }
    case "grant_revoke": {
      const g = grants.find((x) => x.grantId === args.grantId);
      if (g) g.revoked = true;
      return null;
    }
    case "node_revoke": {
      const n = nodes.find((x) => x.nodeId === args.nodeId);
      if (n) n.revoked = true;
      return null;
    }
    // -- E2.4 / E1-P4 / E1-P5 / E2.3 ------------------------------------------
    case "node_quarantine_set": {
      const n = nodes.find((x) => x.nodeId === args.nodeId);
      if (n) n.quarantined = args.quarantined ? String(args.reason ?? "") : null;
      return null;
    }
    // The owner's veto over one node. Missing until UX-8 gave Fleet a bulk
    // "Stop sending work", which is how its absence showed.
    case "node_contribute_set": {
      const n = nodes.find((x) => x.nodeId === args.nodeId);
      if (!n) throw new Error("no such node");
      n.contributes = Boolean(args.allowed);
      n.contributeReason = args.allowed ? "" : String(args.reason ?? "");
      return null;
    }
    // The assignment is total, and the merged list is what every other
    // surface reads — so the mock does the same union the engine does
    // rather than only recording the assigned half.
    case "node_tags_assign": {
      const n = nodes.find((x) => x.nodeId === args.nodeId);
      if (!n) throw new Error("no such node");
      const wanted = (args.tags as string[]).map((t) => t.trim()).filter(Boolean);
      for (const t of wanted) {
        if (t.startsWith("cap."))
          throw new Error(`\`${t}\` is hardware the node counts for itself, so it cannot be assigned from here`);
      }
      const announced = n.tags.filter((t) => !n.assignedTags.includes(t));
      n.assignedTags = [...new Set(wanted)];
      n.tags = [...announced, ...n.assignedTags.filter((t) => !announced.includes(t))];
      return null;
    }
    case "service_accounts":
      return serviceAccounts;
    case "service_account_create": {
      const days = args.expiresInDays as number | null;
      const row = {
        userId: rid("mwu1", 52),
        shortId: rid("mwu1", 12),
        name: String(args.name),
        description: String(args.description ?? ""),
        createdAtMicros: now(),
        expiresAtMicros: days == null ? null : now() + days * 86_400e6,
        expired: false,
        revoked: false,
        roles: [String(args.role)],
      };
      serviceAccounts.push(row);
      return {
        name: row.name,
        userId: row.userId,
        // 32 bytes, so the shape matches what `save_key_file` validates.
        key: Array.from({ length: 32 }, () => Math.floor(Math.random() * 256)),
        expiresAtMicros: row.expiresAtMicros,
      };
    }
    case "save_key_file":
      return `/home/you/${String(args.suggested)}`;
    case "oidc_providers":
      return oidcProviders;
    case "user_rotate_key": {
      const u = users.find((x) => x.username === args.username && !x.revoked);
      if (!u) throw new Error(`no active user called ${args.username}`);
      u.revoked = true;
      users.push({ ...u, userId: String(args.newKey), shortId: String(args.newKey).slice(0, 16), revoked: false });
      return 1;
    }
    // The *joining* side of pairing (the wizard). Unimplemented until now,
    // which meant the one screen every new customer sees first could not be
    // reviewed without two real machines.
    // The wizard's "Just show me" (UX-18): three steps and a ready, on the
    // cadence of the real one's progress, then a mesh that says it is a
    // demo until it is stopped.
    case "demo_start": {
      const ch = args.channel as ChannelLike;
      const machines = Number(args.machines) || 3;
      void (async () => {
        const steps = [
          `Starting ${machines} machines on this computer`,
          "Pairing them into one mesh",
          "Running a first job on each",
        ];
        for (let i = 0; i < steps.length; i++) {
          ch.onmessage({ type: "step", message: steps[i], done: i, total: steps.length });
          await sleep(900);
        }
        mockDemo = true;
        ch.onmessage({ type: "ready", meshName: "hyacinth", machines });
      })();
      return null;
    }
    case "demo_stop":
      mockDemo = false;
      return null;
    case "wizard_pair_start": {
      const ch = args.channel as ChannelLike;
      wizardPair = ch;
      void (async () => {
        await sleep(600);
        if (wizardPair !== ch) return;
        ch.onmessage({
          type: "listening",
          addr: "192.168.1.55:7777",
          addrs: ["192.168.1.55:7777", "100.64.0.12:7777"],
          mdnsError: null,
        });
        // `?pairwait` leaves it announcing, so the wait can be looked at.
        if (new URLSearchParams(location.search).has("pairwait")) return;
        await sleep(2600);
        if (wizardPair !== ch) return;
        ch.onmessage({ type: "code", code: "482917", meshName: "hyacinth", approverName: "neuralpunk" });
      })();
      return null;
    }
    // DIR-6: given up from the wizard. Everything the script above still
    // had to say is dropped with it.
    case "wizard_pair_cancel": {
      const ch = wizardPair;
      wizardPair = null;
      await sleep(250);
      ch?.onmessage({ type: "cancelled" });
      return null;
    }
    case "wizard_pair_decide": {
      const ch = wizardPair;
      if (!ch) return null;
      if (!args.accept) {
        wizardPair = null;
        ch.onmessage({ type: "failed", message: "you rejected the code" });
        return null;
      }
      void (async () => {
        await sleep(1100);
        if (wizardPair !== ch) return;
        wizardPair = null;
        mockEnrolment = "none";
        ch.onmessage({ type: "joined", meshName: "hyacinth" });
      })();
      return null;
    }
    case "invite_create":
      return "mwj1" + rid("", 180);
    case "discover_candidates":
      await sleep(1500);
      return candidates;
    case "lan_watch_start":
      lanWatching = true;
      return null;
    case "lan_watch_stop":
      lanWatching = false;
      return null;
    case "lan_watch_view":
      return {
        running: lanWatching,
        candidates: lanWatching ? candidates.map((c) => ({ ...c, lastSeenMicros: now() - 1e6 })) : [],
        mdnsError: null,
        beaconError: null,
        beaconPort: 7778,
        localAddrs: ["192.168.1.20", "100.64.0.3"],
      } satisfies LanWatchView;
    case "pair_approve_start": {
      // The channel is held so `pair_approve_decide` can finish the
      // exchange: the join queue's whole point is that approving a machine
      // hands straight over to recognising its person, and a mock that
      // stops at the code cannot exercise that hand-off at all.
      const addrs = (args.addrs as string[] | undefined) ?? [];
      pairing = { ch: args.channel as ChannelLike, name: pairingNameFor(addrs) };
      void (async () => {
        pairing?.ch.onmessage({ type: "trying", addr: addrs[0] ?? "?" });
        await sleep(900);
        pairing?.ch.onmessage({
          type: "code",
          session: 1,
          code: "482917",
          joinerName: pairing.name,
          joinerId: rid("mw1", 52),
        });
      })();
      return null;
    }
    case "pair_approve_decide": {
      const session = pairing;
      pairing = null;
      if (!session) return null;
      if (!args.accept) {
        session.ch.onmessage({ type: "failed", message: "the codes did not match" });
        return null;
      }
      void (async () => {
        await sleep(700);
        const nodeId = rid("mw1", 52);
        nodes = [...nodes, mockJoinedNode(nodeId, session.name)];
        session.ch.onmessage({ type: "done", nodeId, nodeName: session.name });
        // …and, a beat later, the person sitting at it asks to be
        // recognised — which is what `app.requestAccessIfNeeded` does for
        // real on the joining side.
        await sleep(1800);
        enrolments = [
          ...enrolments,
          {
            userId: rid("mwu1", 44),
            shortId: "mwu1" + rid("", 4),
            suggestedUsername: session.name.split(/[-._]/)[0] ?? session.name,
            fromNode: session.name,
            code: "SILVER-KESTREL-2038",
            requestedAtMicros: Date.now() * 1000,
            expiresAtMicros: Date.now() * 1000 + 3600 * 1_000_000,
            declined: false,
            declinedBy: null,
          },
        ];
      })();
      return null;
    }
    case "doctor_run":
      await sleep(600);
      return doctor;
    case "config_view":
      return JSON.stringify({ node_name: "director", data_dir: "~/.local/share/meshware/director", listen: "0.0.0.0:0", mesh_name: "hyacinth", log_level: "info", tags: nodes[0].tags, limits: { slots: 16, queue: 64, min_free_mem_mb: 256 }, http_addr: null, retention: { job_days: 30, job_max_bytes: 1073741824 }, wan: { advertise: [], relay: false, relay_client: true } }, null, 2);
    case "backup_write":
      return { path: `/home/you/${String(args.suggested)}`, ops: 132, bytes: 54085 };
    case "drain":
      void streamDrain(args, args.channel as ChannelLike);
      return null;
    // -- M18 facts --------------------------------------------------------
    case "node_facts":
      return mockNodeFacts(String(args.subject ?? ""));
    case "fact_collectors_view":
      return factCollectors;
    case "collector_set_paused": {
      const row = factCollectors.find((c) => c.name === args.name);
      if (!row) throw `mock: no collector ${args.name}`;
      if (row.builtIn) throw "mock: the built-in collector cannot be paused";
      row.paused = Boolean(args.paused);
      return null;
    }
    // Saving a run's settings as the script's defaults (2026-09). The
    // payload is parked on `window` as well as applied, so the wiring
    // between this screen's controls and the `run` block the engine reads
    // can be asserted rather than eyeballed.
    case "service_save_run_defaults": {
      const name = String(args.name ?? "");
      const svc = services.find((x) => x.name === name);
      if (!svc) throw `mock: no service ${name}`;
      const run = (args.run ?? {}) as Record<string, unknown>;
      const argDefaults = (args.argDefaults ?? []) as [string, string][];
      (globalThis as Record<string, unknown>).__mwSavedRunDefaults = { name, run, argDefaults };
      const contract = svc.contract ? JSON.parse(svc.contract) : { type: "object", properties: {} };
      contract["x-meshware"] = { ...(contract["x-meshware"] ?? {}), run };
      for (const [k, v] of argDefaults) {
        const prop = contract.properties?.[k];
        if (!prop) continue;
        if (!v.trim()) delete prop.default;
        else prop.default = prop.type === "integer" || prop.type === "number" ? Number(v) : prop.type === "boolean" ? v === "true" : v;
      }
      svc.contract = JSON.stringify(contract, null, 2);
      svc.version += 1;
      svc.versions = [...svc.versions, svc.version];
      return { version: svc.version, definitionsPath: `/home/you/scripts/${svc.entrypoint}.definitions`, definitionsError: null };
    }

    // -- M31 A2: the content store and the disk under it -------------------
    //
    // Absent until the 2026-09 pass, which meant Settings > Storage — a
    // pane with a radio ladder, two bars and a fleet table on it — could
    // not be looked at in the mock at all, and so was never looked at.
    case "content_status": {
      const bytes = 18_442_240_000;
      return {
        policy: contentPolicy,
        windowDays: 30,
        cappedByMesh: false,
        bundles: 61,
        chunks: 4_812,
        bytes,
        protected: 23,
        reclaimableBundles: 38,
        reclaimableBytes: Math.round(bytes * 0.41),
        maxBytes: contentCapBytes,
        diskFreeBytes: 96_500_000_000,
        diskTotalBytes: 480_000_000_000,
        diskFloorBytes: 24_000_000_000,
        diskWarnBytes: 48_000_000_000,
        diskCritical: false,
        options: [
          { id: "keep", label: "Keep everything", description: "Nothing is ever reclaimed. The store grows with every release and every sync.", days: 0, immediate: false },
          { id: "balanced", label: "Balanced", description: "Unused bundles go after a month. The default, and right for almost every node.", days: 30, immediate: false },
          { id: "tidy", label: "Tidy", description: "A week. For a node with a small disk that runs a lot of different work.", days: 7, immediate: false },
          { id: "ephemeral", label: "Keep nothing spare", description: "Reclaimed as soon as nothing needs it. For a runner that should hold no history.", days: 0, immediate: true },
        ],
      };
    }
    case "set_content_policy": {
      contentPolicy = String(args.policy ?? "balanced");
      contentCapBytes = Number(args.maxMb ?? 0) * 1_048_576;
      return invoke("content_status", {});
    }
    case "content_prune": {
      const dry = Boolean(args.dryRun);
      return {
        dryRun: dry,
        bundlesRemoved: 38,
        bytesReclaimed: 7_561_318_400,
        orphanChunks: 112,
        orphanBytes: 41_900_000,
        protected: 23,
      };
    }
    case "disk_view":
      return nodes
        .filter((n) => !n.revoked && !n.left)
        .map((n, i) => {
          const total = (n.capacity?.diskMb ?? 0) * 1_048_576;
          const freePct = [0.2, 0.61, 0.08, 0.44, 0.35][i] ?? 0.5;
          return {
            nodeId: n.nodeId,
            name: n.name,
            online: n.liveness === "alive",
            freeBytes: Math.round(total * freePct),
            totalBytes: total,
            state: total === 0 ? "unknown" : freePct < 0.1 ? "low" : freePct < 0.25 ? "warn" : "ok",
          };
        });

    // -- M19 results ------------------------------------------------------
    case "result_sets_view": {
      // `?noanswers` is the empty state, which is the half of this tab
      // that has a button on it — and so the half worth looking at.
      const params = new URLSearchParams(location.search);
      if (params.has("noanswers")) return [];
      // `?answers=20000` is the other end, and the one the table is
      // actually designed against: a fan-out nobody would read row by
      // row, with a handful of stdouts far too large for a cell and one
      // that is far too large for a pane. Both ends have to be right, and
      // only one of them is reachable from a five-machine fake mesh.
      const want = Number(params.get("answers") ?? "0");
      if (Number.isFinite(want) && want > 0) {
        const set = bigAnswerSet(Math.min(Math.trunc(want), 200_000));
        return [set];
      }
      const rows = nodes.slice(0, 4).map((n, i) => ({
        node: n.name,
        jobId: rid("job-", 12),
        state: i === 3 ? "failed" : "succeeded",
        exitCode: i === 3 ? 1 : 0,
        outputs: [
          ["stdout", i === 3 ? "PermitRootLogin yes" : "PermitRootLogin no"] as [string, string],
          ["exit_code", i === 3 ? "1" : "0"] as [string, string],
        ],
        expectPassed: i !== 3,
        expectDetail: i === 3 ? '"PermitRootLogin yes" contains "permitrootlogin no" → false' : "",
      }));
      const set: ResultSetView = {
        group: rid("fan-", 12),
        owner: "director",
        command: "sshd -T | grep -i permitrootlogin",
        rows,
        succeeded: 3,
        failed: 1,
        missing: 0,
        createdAtMicros: now() - 15 * 60e6,
      };
      return [set];
    }
    // -- M17 the fabric ---------------------------------------------------
    case "sessions_view":
      return sessions;
    case "session_close": {
      sessions = sessions.filter((s) => s.id !== Number(args.id));
      return null;
    }
    case "session_policies_view":
      return sessionPolicies;
    case "session_policy_put": {
      const allow = (args.allow as string[]) ?? ["shell"];
      const next = {
        id: String(args.id),
        description: String(args.description ?? ""),
        users: String(args.users ?? "*"),
        nodes: String(args.nodes ?? "any"),
        allow,
        expiresAtMicros: args.expiresSecs
          ? now() + Number(args.expiresSecs) * 1e6
          : null,
        createdAtMicros: now(),
        createdByUsername: "you",
        coversThisNode: args.nodes === "any" || String(args.nodes).startsWith("tag="),
      };
      const at = sessionPolicies.findIndex((p) => p.id === next.id);
      if (at >= 0) sessionPolicies[at] = next;
      else sessionPolicies.push(next);
      return null;
    }
    case "session_policy_remove": {
      sessionPolicies = sessionPolicies.filter((p) => p.id !== String(args.id));
      return null;
    }
    case "terminal_open":
      throw "mock: a terminal needs a real node";
    // Quietly nothing, as the real ones are once a session has gone — and
    // the mock's only terminal is the scripted build below.
    case "terminal_send":
    case "terminal_resize":
    case "terminal_close":
      return null;
    // The Build tab's Run (DIR-8): a scripted make, so the panel can be
    // looked at without a toolchain. `?nobuild` is the person pressing
    // Cancel on the desktop's confirmation.
    case "build_run": {
      if (new URLSearchParams(location.search).has("nobuild")) return null;
      const ch = args.channel as ChannelLike;
      const id = ++mockBuildTerm;
      void (async () => {
        const say = (s: string) => ch.onmessage({ type: "data", b64: btoa(s.replace(/\n/g, "\r\n")) });
        ch.onmessage({ type: "opened" });
        for (const unit of ["main.o", "parse.o", "eval.o", "print.o"]) {
          await sleep(350);
          say(`meshnode cc gcc -c -O2 src/${unit.replace(".o", ".c")} -o build/${unit}\n`);
        }
        await sleep(300);
        say("gcc build/*.o -o build/app\n");
        ch.onmessage({ type: "exit", code: 0 });
      })();
      return id;
    }
    // -- M31: retention and history export ----------------------------------
    case "retention_view":
      return retention;
    // M31 A3. Empty by default — the state a healthy mesh is in. Start with
    // `?mock=1&pressure=1` to design the bad day.
    case "backpressure_view":
      return new URLSearchParams(location.search).has("pressure")
        ? {
            limitPerMinute: 240,
            entities: [
              {
                entity: "run:wf-nightly-etl",
                observedPerMinute: 1_412,
                refusedTotal: 8_204,
                pausedUntilMicros: (Date.now() + 240_000) * 1000,
              },
              {
                entity: "job:job-9f2c1a",
                observedPerMinute: 388,
                refusedTotal: 143,
                pausedUntilMicros: 0,
              },
            ],
            flapping: ["disk-usage:edge-7"],
          }
        : { limitPerMinute: 240, entities: [], flapping: [] };
    case "retention_put": {
      // Same rule the engine follows: only the tables named change, and
      // the projection moves with the `runs` window.
      for (const pair of (args.windows as string[]) ?? []) {
        const [table, window] = pair.split("=");
        const row = retention.windows.find((w) => w.table === table);
        if (row) { row.window = window; row.effectiveHere = window; }
      }
      retention.signedBy = "you";
      retention.signedAtMicros = Date.now() * 1000;
      retention.settlesAtBytes = settledBytes();
      return retention;
    }
    case "retention_reset":
      retention = freshRetention();
      return retention;
    case "sinks_view":
      return sinks;
    case "sink_kinds":
      return sinkKinds;
    case "sink_put": {
      const d = args.draft as Record<string, unknown>;
      const name = String(d.name);
      const kind = String(d.kind);
      const meta = sinkKinds.find((k) => k.kind === kind);
      const row: SinkView = {
        name,
        kind,
        target: String(d.url || d.database || ""),
        database: String(d.database ?? ""),
        tables: (d.tables as string[]) ?? ["jobs"],
        enabled: d.enabled !== false,
        elected: String(d.runOn ?? "") === "" ? "this peer" : "anvil",
        electedHere: String(d.runOn ?? "") === "",
        runOn: String(d.runOn ?? ""),
        // Mirrors the engine: the cursor is a file on the exporting node,
        // so a sink elected elsewhere has no counters to show here.
        statusIsLocal: String(d.runOn ?? "") === "",
        lagSecs: 0,
        sent: 0,
        spooledBytes: 0,
        dropped: 0,
        lastStatus: "",
        lastAtMicros: 0,
        pausedByRestore: false,
        payloadsSent: 0,
        payloadsTooLarge: 0,
        holdForExportDays: Number(d.holdForExportDays ?? 0),
        forcedGaps: 0,
        archives: meta?.archives ?? true,
        replayPayloads: meta?.replayPayloads ?? false,
        createdBy: "you",
        createdAtMicros: Date.now() * 1000,
        description: String(d.description ?? ""),
      };
      sinks = [...sinks.filter((s) => s.name !== name), row];
      return null;
    }
    case "sink_remove":
      sinks = sinks.filter((s) => s.name !== args.name);
      return null;
    case "sink_test": {
      // Answers from the **draft**, the way the engine does — so the mock
      // exercises test-before-save rather than a path that only works once
      // the mistake has been signed into the mesh.
      const d = args.draft as Record<string, unknown>;
      const url = String(d.url ?? "");
      if (!url.startsWith("http")) {
        throw new Error(`a ${d.kind} sink has nothing to connect to — it writes locally`);
      }
      if (url.startsWith("http://") && !/localhost|127\.|10\.|192\.168\./.test(url)) {
        throw new Error(`${url} is plain HTTP on a public address. A sink carries this mesh's whole history and its credential; use https://, or point at a destination on your own network.`);
      }
      // Wrong often enough that the failure path gets designed too.
      if (!d.credential) throw new Error("HTTP 401 authentication failed");
      return `accepted by ${new URL(url).host}`;
    }
    case "history_jobs": {
      // The fence, in the mock too: this filters local fixtures and never
      // touches `sinks`. If a browse ever needed a destination to answer,
      // this case would have to reach for one.
      const needle = String(args.contains ?? "").toLowerCase();
      const wanted = String(args.jobState ?? "");
      const since = Number(args.sinceMicros ?? 0);
      const matched = historyJobs.filter(
        (j) =>
          (!needle || j.command.toLowerCase().includes(needle)) &&
          (!wanted || j.state === wanted) &&
          (!since || Number(j.finishedAtMicros ?? 0) >= since),
      );
      const offset = Number(args.offset ?? 0);
      const limit = Number(args.limit ?? 50);
      const rows = matched.slice(offset, offset + limit);
      return {
        rows,
        total: matched.length,
        tiers: {
          fromIndex: rows.filter((r) => !r.fromArchive).length,
          fromArchive: rows.filter((r) => r.fromArchive).length,
          // Somebody else's Parquet, which is the ordinary shape: the
          // archive is built on one machine and browsed from any of them.
          archivePeer: "anvil",
          archiveUnreachable: false,
          archiveStatus: "",
          // 150 days, so that "Last 6 months" actually trips the
          // known-absence note. At 200 none of the three window options
          // could reach past it, which left the one state this note exists
          // for impossible to see — a mock that can only produce the happy
          // path designs only the happy path.
          retainedFromMicros: String(Math.round(now() - 150 * DAY_US)),
          windowReachedPast: Boolean(since) && since < now() - 150 * DAY_US,
        },
      } as HistoryJobPageView;
    }
    case "history_replay": {
      const row = historyJobs.find((j) => j.job === args.job);
      if (!row?.payloadSink) {
        return {
          lines: [],
          moreBefore: false,
          status:
            "this job's output was never exported to a destination, so there is nothing to read back — the record still says what happened",
        } as PayloadWindowView;
      }
      const search = String(args.search ?? "").toLowerCase();
      const pool = search
        ? PAYLOAD_LINES.filter((l) => l.toLowerCase().includes(search))
        : PAYLOAD_LINES;
      const fromEnd = Number(args.fromEnd ?? 0);
      const limit = Number(args.limit ?? 500);
      // Tail first, and windowed backwards from it — the same arithmetic
      // the real reader does against ClickHouse's LIMIT/OFFSET.
      const end = Math.max(0, pool.length - fromEnd);
      const start = Math.max(0, end - limit);
      return {
        lines: pool.slice(start, end),
        moreBefore: start > 0,
        status: search && pool.length === 0 ? `no lines matched "${args.search}"` : "",
      } as PayloadWindowView;
    }
    case "sinks_resume":
      sinks = sinks.map((s) => ({ ...s, pausedByRestore: false }));
      return null;
    // The palette's ⇧↵ (L2 P3) and every other copy affordance. There is
    // no OS clipboard behind the mock, so it goes to the browser's.
    // DIR-11: no file manager and no log file behind the mock.
    case "open_log_dir":
      return null;
    case "frontend_log":
      console.debug("[frontend_log]", args.kind, args.message);
      return null;
    case "copy_text":
      await navigator.clipboard?.writeText(String(args.text)).catch(() => {});
      return null;
    case "open_url":
      window.open(String(args.url), "_blank", "noopener");
      return null;
    // The pickers run on the Rust side and answer with a token (SEC-14).
    case "pick_file":
    case "pick_directory":
      return mockPick(cmd === "pick_directory" ? "/home/you/ops-scripts" : "/home/you/deploy.schema.json");
    case "credential_users":
      return [...mockKeychain.keys()];
    case "credential_get": {
      // The key file comes back as a pick too, as `view_of` makes it.
      const r = mockKeychain.get(String(args.user));
      return r ? { ...r, keyToken: r.keyPath ? mockPick(r.keyPath).token : null } : null;
    }
    case "credential_forget":
      mockKeychain.delete(String(args.user));
      return null;
    case "credential_put": {
      const a = args.auth as DeployAuthDraft;
      const was = mockKeychain.get(a.user);
      // A draft filled from the keychain arrives with its secrets blank;
      // re-saving it must not blank the record (see `credential_put`).
      const kept = (typed: string | null, had: boolean | undefined) =>
        !!typed || (a.fromKeychain && !!had);
      mockKeychain.set(a.user, {
        user: a.user,
        method: a.method,
        keyPath: a.keyPath ? (mockPicks.get(a.keyPath) ?? a.keyPath) : a.fromKeychain ? (was?.keyPath ?? null) : null,
        hasPassword: kept(a.password, was?.hasPassword),
        hasPassphrase: kept(a.passphrase, was?.hasPassphrase),
        hasSudoPassword: kept(a.sudoPassword, was?.hasSudoPassword),
      });
      return null;
    }
    case "release_build_dir":
      return mockBuildDir;
    case "release_build_dir_forget":
      mockBuildDir = null;
      return null;
    // The mock has no disk to write to, so the shared snapshot (L2 P2)
    // opens in a tab instead. That is also the only way to *look* at the
    // export while working on it, which is the whole reason this case is
    // more than a `return path`.
    case "snapshot_save": {
      const raw = atob(String(args.pngBase64));
      const bytes = Uint8Array.from(raw, (c) => c.charCodeAt(0));
      const url = URL.createObjectURL(new Blob([bytes], { type: "image/png" }));
      window.open(url, "_blank");
      // `snapshot_save` takes `suggested`, not `path` — this said
      // "Wrote undefined" for as long as the mock has had the command.
      return `${args.suggested} (opened in a tab — the mock has no disk)`;
    }

    // The streamed save (`src-tauri/src/filesave.rs`). There is no disk
    // behind the mock, but there *is* a browser download — so Save here
    // exercises the whole path (chooser, chunking, progress, cancel) and
    // ends with a real file, which is the only way to check that what the
    // sink was fed is what a person opens.
    case "file_save_begin": {
      const id = ++mockSaveId;
      mockSaves.set(id, { name: String(args.suggested), parts: [], bytes: 0 });
      return id;
    }
    case "file_save_write": {
      const sink = mockSaves.get(Number(args.id));
      if (!sink) throw "that save is no longer open";
      const chunk = String(args.chunk);
      sink.parts.push(chunk);
      sink.bytes += new Blob([chunk]).size;
      return sink.bytes;
    }
    case "file_save_finish": {
      const id = Number(args.id);
      const sink = mockSaves.get(id);
      if (!sink) throw "that save is no longer open";
      mockSaves.delete(id);
      const url = URL.createObjectURL(new Blob(sink.parts, { type: "application/json" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = sink.name;
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 30_000);
      return `~/Downloads/${sink.name} (the browser downloaded it — the mock has no disk)`;
    }
    case "file_save_abort":
      mockSaves.delete(Number(args.id));
      return null;


    default:
      throw `mock: unhandled command ${cmd}`;
  }
}

// ---------------------------------------------------------------------------
// The streamed save sink, and the answer set that stresses it
// ---------------------------------------------------------------------------

/** Saves in flight, keyed the way `filesave.rs` keys them. */
const mockSaves = new Map<number, { name: string; parts: string[]; bytes: number }>();
let mockSaveId = 0;

/**
 * A fan-out big enough to be worth designing against (`?mock=1&answers=20000`).
 *
 * The five-machine fiction the rest of this file lives in is the right
 * default and the wrong test: everything the answers table does — windowing
 * its rows, windowing the JSON by line, shortening a value before it
 * reaches a cell, streaming a save rather than building it — is invisible
 * at four rows and is the whole component at twenty thousand. So the shape
 * here is deliberately awkward:
 *
 *   · five output columns, one of which is usually empty;
 *   · roughly one row in nine failed, so "failures only" has something to do;
 *   · one row in ninety-seven with ~60 KB of stdout, which is what a cell
 *     must not try to draw;
 *   · and row 1 with several megabytes of it, which is what the expansion
 *     must not try to draw either.
 */
function bigAnswerSet(n: number): ResultSetView {
  const line = "the quick brown fox jumps over the lazy dog and files a ticket about it\n";
  const chunky = `# a build log nobody wants in a table cell\n${line.repeat(850)}`;
  const enormous = `# and one that is past what any pane should render\n${line.repeat(60_000)}`;
  const rows = Array.from({ length: n }, (_, i) => {
    const bad = i % 9 === 4;
    const stdout =
      i === 1 ? enormous : i % 97 === 3 ? chunky : bad ? "PermitRootLogin yes" : "PermitRootLogin no";
    return {
      node: `node-${String(i).padStart(5, "0")}.${["ams", "fra", "iad", "sfo"][i % 4]}`,
      jobId: `job-${String(i).padStart(8, "0")}`,
      state: bad ? "failed" : "succeeded",
      exitCode: bad ? 1 : 0,
      outputs: [
        ["stdout", stdout],
        ["stderr", bad ? "sshd_config line 34: unsupported option" : ""],
        ["exit_code", bad ? "1" : "0"],
        ["kernel", `6.1.0-${18 + (i % 6)}-amd64`],
        ["disk_free", `${(18 + ((i * 7) % 430) / 10).toFixed(1)}G`],
        ["uptime", `${3 + (i % 400)}d ${i % 24}h`],
        ["address", `10.${(i >> 16) % 256}.${(i >> 8) % 256}.${i % 256}`],
        ["agent", `meshnode 0.7.${i % 3} (${["glibc", "musl"][i % 2]})`],
      ] as [string, string][],
      expectPassed: !bad,
      expectDetail: bad ? '"PermitRootLogin yes" contains "permitrootlogin no" → false' : "",
    };
  });
  const failed = rows.filter((r) => r.exitCode !== 0).length;
  return {
    group: `fan-${String(n).padStart(6, "0")}`,
    owner: "director",
    command: "sshd -T | grep -i permitrootlogin",
    rows,
    succeeded: rows.length - failed,
    failed,
    missing: Math.round(n * 0.04),
    createdAtMicros: now() - 15 * 60e6,
  };
}



export function installMock() {
  // URL → deep links: ?view=library&library:open=create-service (any
  // "<view>:<key>" param lands in sessionStorage, like a palette jump).
  const q = new URLSearchParams(location.search);
  const view = q.get("view");
  if (view) sessionStorage.setItem("app:view", view);
  for (const [k, v] of q) if (k.includes(":")) sessionStorage.setItem(k, v);

  const callbacks = new Map<number, (raw: unknown) => void>();
  let nextId = 1;

  // Enough of Tauri's event plugin to deliver what the Rust side emits
  // (DIR-7) — `mw-dropped` above all, so dropping a file onto the window
  // can be tried here: `__mockDrop("/home/you/rotate-logs.sh")` in the
  // console is what the OS drop sends.
  const listeners = new Map<number, { event: string; handler: number }>();
  let nextEvent = 1;
  const shim = (cmd: string, args: Args = {}) => {
    if (cmd === "plugin:event|listen") {
      const id = nextEvent++;
      listeners.set(id, { event: String(args.event), handler: Number(args.handler) });
      return Promise.resolve(id);
    }
    if (cmd === "plugin:event|unlisten") {
      listeners.delete(Number(args.eventId));
      return Promise.resolve(null);
    }
    return invoke(cmd, args);
  };
  const emit = (event: string, payload: unknown) => {
    for (const [id, l] of listeners) {
      if (l.event === event) callbacks.get(l.handler)?.({ event, id, payload });
    }
  };
  const w = window as unknown as Record<string, unknown>;
  w.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  w.__mockDrop = (...paths: string[]) => emit("mw-dropped", paths.map(mockPick));

  (window as unknown as { __TAURI_INTERNALS__: unknown }).__TAURI_INTERNALS__ = {
    invoke: shim,
    transformCallback(cb: (raw: unknown) => void) {
      const id = nextId++;
      callbacks.set(id, cb);
      return id;
    },
    unregisterCallback(id: number) {
      callbacks.delete(id);
    },
    convertFileSrc: (p: string) => p,
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
  };
  // One source labelled as a folder from this computer, so the wizard has a
  // local path to offer "Save to disk" against. Only set if absent, so a
  // label the person made themselves is never overwritten.
  try {
    const KEY = "meshware.folderLabels";
    const labels: Record<string, string> = JSON.parse(localStorage.getItem(KEY) ?? "{}");
    if (!labels[bundles[0].bundleId]) {
      labels[bundles[0].bundleId] = "/home/you/ops";
      localStorage.setItem(KEY, JSON.stringify(labels));
    }
  } catch {
    // Private mode: the folder branch simply will not be modelled.
  }
  console.info("%cmeshware director: mock engine installed (?mock=1)", "color:#0e9384;font-weight:600");
}
