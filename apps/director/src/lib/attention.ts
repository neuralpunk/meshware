// What on this mesh needs somebody to look at it (UX-3).
//
// The capsule beside the mesh's name is the biggest status word on the
// landing screen, and it used to be about one thing only: whether *this
// peer's* engine was running or draining. So it said "Healthy", in green,
// while a machine had stopped replying, another was quarantined, a GPU was
// throttling and a deployment was failing its health checks — which is the
// exact moment an operator glances at it.
//
// This is the list it is derived from now. Pure, so the rules are tested
// rather than eyeballed, and so the capsule and the list under it can never
// disagree about the count.
//
// The rules follow what each surface already says about its own rows:
//
//   · **A machine that is not replying** — suspect or dead, and still a
//     member. A machine that left or was removed is history, not a fault.
//   · **Quarantined** — isolated as possibly compromised (E2.4).
//   · **Out of date** — behind on the log *and* no longer asking. Behind
//     alone is every mesh between gossip rounds (`SyncCell.svelte` says the
//     same, with the same thirty seconds).
//   · **An accelerator** that reports itself degraded, unavailable or
//     throttled.
//   · **A deployment** whose copies keep failing (`degraded`).

import type { NodeView } from "./bindings/NodeView";
import type { DeviceView } from "./bindings/DeviceView";
import type { DeploymentView } from "./bindings/DeploymentView";
import { vendorLabel } from "./vendor";

/** Past this a sync reading describes a conversation that has stopped. */
export const SYNC_COLD_SECS = 30;

export type AttentionItem = {
  /** Stable across polls, for keyed lists. */
  key: string;
  kind: "node" | "device" | "deployment";
  /** The thing, by the name a person knows it by. */
  subject: string;
  /** What is wrong with it, as the rest of a sentence. */
  problem: string;
  /** `error` for a fault, `warn` for something degraded but working. */
  tone: "error" | "warn";
  /** A node id or a deployment name, for the link. */
  target: string;
};

export function attentionItems(input: {
  nodes: readonly NodeView[];
  devices?: readonly DeviceView[] | null;
  deployments?: readonly DeploymentView[] | null;
}): AttentionItem[] {
  const out: AttentionItem[] = [];
  const members = input.nodes.filter((n) => !n.revoked && !n.left);

  for (const n of members) {
    const base = { kind: "node" as const, subject: n.name, target: n.nodeId };
    // Quarantine first: it is the reason to look even when the machine is
    // perfectly reachable, and a quarantined machine that also went quiet
    // is one problem to look at, not two.
    if (n.quarantined !== null) {
      out.push({
        ...base,
        key: `q:${n.nodeId}`,
        problem: n.quarantined ? `is quarantined: ${n.quarantined}` : "is quarantined",
        tone: "error",
      });
      continue;
    }
    if (n.liveness === "dead") {
      out.push({ ...base, key: `d:${n.nodeId}`, problem: "has stopped answering", tone: "error" });
      continue;
    }
    if (n.liveness === "suspect") {
      out.push({ ...base, key: `s:${n.nodeId}`, problem: "isn't replying", tone: "warn" });
      continue;
    }
    const sync = n.sync;
    if (sync && sync.behind > 0 && sync.ageMicros / 1e6 > SYNC_COLD_SECS) {
      out.push({
        ...base,
        key: `y:${n.nodeId}`,
        problem: `is ${sync.behind.toLocaleString()} change${sync.behind === 1 ? "" : "s"} behind and has stopped catching up`,
        tone: "warn",
      });
    }
  }

  // Accelerators on machines that are still members. A device on a machine
  // that left is not something anyone can go and look at.
  const memberIds = new Set(members.map((n) => n.nodeId));
  // A vendor's ordinal is only unique within that vendor: a Ryzen desktop
  // with an NVIDIA card has an AMD "GPU 0" and an NVIDIA "GPU 0"
  // (`DeviceKey`). So the key carries the vendor, and so does the name
  // wherever the bare ordinal would name two devices.
  const devices = input.devices ?? [];
  const slot = (d: DeviceView) => `${d.nodeId}:${d.kind}:${d.index}`;
  const slotCount = new Map<string, number>();
  for (const d of devices) slotCount.set(slot(d), (slotCount.get(slot(d)) ?? 0) + 1);
  for (const d of devices) {
    if (!memberIds.has(d.nodeId)) continue;
    const bad = d.health !== "ok";
    if (!bad && !d.throttled) continue;
    const kind = d.kind === "gpu" || !d.kind ? "GPU" : d.kind.toUpperCase();
    const make = (slotCount.get(slot(d)) ?? 0) > 1 ? `${vendorLabel(d.vendor)} ` : "";
    out.push({
      key: `g:${d.nodeId}:${d.vendor}:${d.kind}:${d.index}`,
      kind: "device",
      subject: `${d.nodeName} ${make}${kind} ${d.index}`,
      problem: bad
        ? `is ${d.health}${d.healthReason ? `: ${d.healthReason}` : ""}`
        : "is throttling",
      tone: bad && d.health === "unavailable" ? "error" : "warn",
      target: d.nodeId,
    });
  }

  for (const dep of input.deployments ?? []) {
    if (dep.status !== "degraded") continue;
    out.push({
      key: `p:${dep.name}`,
      kind: "deployment",
      subject: dep.name,
      problem: `is failing: ${dep.healthy} of ${dep.desired} ${dep.desired === 1 ? "copy" : "copies"} healthy`,
      tone: "error",
      target: dep.name,
    });
  }

  // The capsule's menu is a keyed list, and a repeated key makes Svelte
  // throw instead of drawing it — the capsule then does nothing when
  // clicked. An inventory that lists one device twice is the peer's bug;
  // it must not also take this menu down.
  const seen = new Set<string>();
  return out.filter((i) => !seen.has(i.key) && seen.add(i.key));
}
