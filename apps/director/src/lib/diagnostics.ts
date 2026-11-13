// "Copy diagnostics" (UX-7): the block a person pastes into a bug report
// or an issue, so the first reply is an answer rather than a list of questions.
//
// Pure, and deliberately short. It says which build, which platform, and
// the shape of the mesh — never a secret, a key, an address or a
// machine's name, because the one thing a public issue must not become
// is the place a fleet's layout leaked from. Ids are the short forms.

import type { AboutView } from "./bindings/AboutView";
import type { StatusView } from "./bindings/StatusView";
import type { NodeView } from "./bindings/NodeView";

export function diagnosticsText(input: {
  about: AboutView | null;
  status: StatusView | null;
  nodes: readonly NodeView[];
  role: string;
  now?: Date;
}): string {
  const { about, status, nodes } = input;
  const members = nodes.filter((n) => !n.revoked && !n.left);
  const by = (l: string) => members.filter((n) => n.liveness === l).length;
  const versions = [...new Set(members.map((n) => n.version ?? "unknown"))].sort();
  const lines: [string, string][] = [
    ["Director", about ? `${about.version}${about.commit ? ` (${about.commit})` : ""}` : "unknown"],
    ["Platform", about ? `${about.os}/${about.arch}` : "unknown"],
    ["Engine", status ? `${status.version} · protocol ${status.proto}` : "not running"],
    ["Licence", about ? about.licenseId : "unknown"],
    ["Mesh", status?.meshId ? `${status.meshId.slice(0, 12)}…` : "none"],
    ["This node", status ? status.nodeShort : "unknown"],
    [
      "Machines",
      `${members.length} (${by("alive")} alive, ${by("suspect")} suspect, ${by("dead")} dead)`,
    ],
    ["Versions", versions.length ? versions.join(", ") : "none reported"],
    ["Uptime", status ? `${status.uptimeSecs}s` : "—"],
    ["Draining", status?.draining ? "yes" : "no"],
    ["Role", input.role],
    ["Captured", (input.now ?? new Date()).toISOString()],
  ];
  const width = Math.max(...lines.map(([k]) => k.length));
  return ["Meshware Director diagnostics", ...lines.map(([k, v]) => `${k.padEnd(width)}  ${v}`)].join("\n") + "\n";
}
