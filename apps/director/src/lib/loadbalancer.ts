// Load balancer configuration, written from the registry (2026-09-14).
//
// The mesh proxy is gone (a233173): a copy of a service answers on the
// machine it runs on, at the port that machine gave it, and a real name in
// front of those copies is a load balancer's job — the line Nomad draws.
// The Load balancing screen's promise is that nobody assembles that list by
// hand, so the text it hands over has to work when pasted. That is why it
// lives here, as plain functions with tests, rather than as template
// literals inside a component.
//
// Two rules:
//
//   1. **A snapshot says it is one.** Every configuration opens with a
//      comment saying when it was written and that a copy moving elsewhere
//      makes it stale. A fixed port is the cure, and the screen says so;
//      the file must not pretend otherwise.
//   2. **Names are made safe for the file they go in.** `metrics-relay` is
//      a fine nginx upstream, and HAProxy and Traefik are fussier, so every
//      identifier is reduced to what all of the formats below accept.

/** One copy a load balancer can send traffic to. */
export type Backend = { node: string; address: string; port: number };

export type LbKind = "nginx" | "haproxy" | "caddy" | "traefik" | "list";

export type LbOptions = {
  service: string;
  /** The name people will use — `example.com`. Empty means "any name". */
  domain: string;
  /** The port the load balancer itself listens on. */
  listen: number;
  /** A path that answers 2xx or 3xx on a working copy. */
  healthPath: string;
};

export const LB_KINDS: readonly { id: LbKind; label: string; note: (service: string) => string }[] = [
  {
    id: "nginx",
    label: "nginx",
    note: (s) =>
      `Save as /etc/nginx/conf.d/${s}.conf, then run nginx -s reload. Open-source nginx checks passively: a copy that fails 3 times in 10 seconds is skipped for the next 10.`,
  },
  {
    id: "haproxy",
    label: "HAProxy",
    note: () =>
      "Add to /etc/haproxy/haproxy.cfg, then run systemctl reload haproxy. HAProxy asks every copy for the health path every 5 seconds and stops sending to one that fails 3 times.",
  },
  {
    id: "caddy",
    label: "Caddy",
    note: () =>
      "Add to your Caddyfile, then run caddy reload. For a real domain Caddy fetches the HTTPS certificate itself.",
  },
  {
    id: "traefik",
    label: "Traefik",
    note: () =>
      "For Traefik's file provider: save it in the dynamic configuration directory and Traefik picks it up without a restart.",
  },
  {
    id: "list",
    label: "Address list",
    note: () =>
      "One address per line — for a cloud load balancer's target list, or anything else that takes host:port.",
  },
];

/** `host:port`, with an IPv6 address in brackets so the port is unambiguous. */
export function hostPort(address: string, port: number): string {
  const bare = address.replace(/^\[|\]$/g, "");
  return bare.includes(":") ? `[${bare}]:${port}` : `${bare}:${port}`;
}

/**
 * Whether a load balancer's list of these copies will stay put.
 *
 * `fixed` — every copy answers on one port, so the list is a list of
 * machines and changes only when a copy changes machine. `dynamic` — each
 * machine picked its own free port, so the list changes whenever a copy
 * restarts anywhere.
 */
export type PortShape =
  | { kind: "none" }
  | { kind: "fixed"; port: number }
  | { kind: "dynamic"; ports: number[] };

export function portShape(ports: readonly number[]): PortShape {
  const unique = [...new Set(ports)].sort((a, b) => a - b);
  if (unique.length === 0) return { kind: "none" };
  if (unique.length === 1) return { kind: "fixed", port: unique[0] };
  return { kind: "dynamic", ports: unique };
}

/** An identifier every format here accepts: letters, digits, underscores,
 *  not starting with a digit. */
export function ident(name: string): string {
  const s = name.replace(/[^A-Za-z0-9_]+/g, "_").replace(/^_+|_+$/g, "");
  if (s === "") return "service";
  return /^[0-9]/.test(s) ? `s_${s}` : s;
}

/** One server name per backend, unique even when two copies share a node. */
function serverNames(backends: readonly Backend[]): string[] {
  const used = new Set<string>();
  return backends.map((b) => {
    const base = ident(b.node);
    let name = base;
    for (let n = 2; used.has(name); n++) name = `${base}_${n}`;
    used.add(name);
    return name;
  });
}

function stamp(when: Date): string {
  return `${when.toISOString().slice(0, 16).replace("T", " ")} UTC`;
}

function header(backends: readonly Backend[], opts: LbOptions, when: Date): string[] {
  const n = backends.length;
  return [
    `# ${opts.service}: ${n} backend${n === 1 ? "" : "s"}, written by Meshware Director on ${stamp(when)}.`,
    "# A snapshot of where each copy answers. If a copy moves to another machine or port,",
    "# write this again — or give the service a fixed port so the list stays put.",
  ];
}

function healthPath(opts: LbOptions): string {
  const p = opts.healthPath.trim();
  return p.startsWith("/") ? p : `/${p}`;
}

function nginx(b: readonly Backend[], o: LbOptions): string[] {
  const id = ident(o.service);
  return [
    `upstream ${id} {`,
    ...b.map((x) => `    server ${hostPort(x.address, x.port)} max_fails=3 fail_timeout=10s;`),
    "}",
    "",
    "server {",
    `    listen ${o.listen};`,
    `    server_name ${o.domain || "_"};`,
    "",
    "    location / {",
    `        proxy_pass http://${id};`,
    "        proxy_set_header Host $host;",
    "        proxy_set_header X-Real-IP $remote_addr;",
    "        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;",
    "        proxy_set_header X-Forwarded-Proto $scheme;",
    "        proxy_next_upstream error timeout http_502 http_503 http_504;",
    "    }",
    "}",
  ];
}

function haproxy(b: readonly Backend[], o: LbOptions): string[] {
  const id = ident(o.service);
  const names = serverNames(b);
  return [
    `frontend ${id}_in`,
    `    bind *:${o.listen}`,
    "    mode http",
    ...(o.domain ? [`    acl is_${id} hdr(host) -i ${o.domain}`, `    use_backend ${id} if is_${id}`] : []),
    `    default_backend ${id}`,
    "",
    `backend ${id}`,
    "    mode http",
    "    balance roundrobin",
    `    option httpchk GET ${healthPath(o)}`,
    "    http-check expect status 200-399",
    ...b.map((x, i) => `    server ${names[i]} ${hostPort(x.address, x.port)} check inter 5s fall 3 rise 2`),
  ];
}

function caddy(b: readonly Backend[], o: LbOptions): string[] {
  const site = o.domain
    ? o.listen === 80 || o.listen === 443
      ? o.domain
      : `${o.domain}:${o.listen}`
    : `:${o.listen}`;
  return [
    `${site} {`,
    "    reverse_proxy {",
    ...b.map((x) => `        to ${hostPort(x.address, x.port)}`),
    "        lb_policy round_robin",
    `        health_uri ${healthPath(o)}`,
    "        health_interval 10s",
    "        health_timeout 5s",
    "    }",
    "}",
  ];
}

function traefik(b: readonly Backend[], o: LbOptions): string[] {
  const id = ident(o.service);
  return [
    `# Serve it on the entry point that listens on :${o.listen} (entryPoints in traefik.yml).`,
    "http:",
    "  routers:",
    `    ${id}:`,
    `      rule: "${o.domain ? `Host(\`${o.domain}\`)` : "PathPrefix(`/`)"}"`,
    `      service: ${id}`,
    "  services:",
    `    ${id}:`,
    "      loadBalancer:",
    "        healthCheck:",
    `          path: ${healthPath(o)}`,
    "          interval: 10s",
    "          timeout: 5s",
    "        servers:",
    ...b.map((x) => `          - url: "http://${hostPort(x.address, x.port)}"`),
  ];
}

/** The configuration for `kind`, ending in a newline. */
export function lbConfig(
  kind: LbKind,
  backends: readonly Backend[],
  opts: LbOptions,
  when: Date = new Date(),
): string {
  if (kind === "list") return backends.map((b) => hostPort(b.address, b.port)).join("\n") + (backends.length ? "\n" : "");
  const head = header(backends, opts, when);
  if (backends.length === 0) {
    return [...head, "# No copies are selected, so there is nothing to send traffic to yet."].join("\n") + "\n";
  }
  const body = { nginx, haproxy, caddy, traefik }[kind](backends, opts);
  return [...head, "", ...body].join("\n") + "\n";
}
