<script lang="ts">
  // Load balancing (N3, reworked 2026-09-14) — where every copy of a
  // service answers, and how to put a load balancer in front of them.
  //
  // This screen used to be a name-resolution status board: is the `.mesh`
  // responder answering, is it routed on this machine, and a button that
  // routed it. That was the mesh proxy's world. With the proxy gone
  // (a233173) a copy is reached where it runs — its machine's address and
  // the port that machine gave it — and getting traffic to it from a real
  // name is a load balancer's job, the line Nomad draws. So the screen now
  // answers the questions somebody pointing nginx at a deployment has, in
  // the order they have them:
  //
  //   1. what are the addresses, and is every copy healthy right now;
  //   2. will that list stay put, or change whenever a copy moves;
  //   3. what else has to be true — a firewall, a bind address, a check;
  //   4. what do I paste into my load balancer.
  //
  // DNS is still here, one fold down, for the one case it earns: a load
  // balancer that follows SRV records can track dynamic ports by itself.
  //
  // **Healthy of total is still the column that matters.** Nomad's
  // catalogue has no health field, so a service with three dead copies and
  // one with three live ones look alike there. Here the gap is drawn as
  // cells, and a copy that is not passing is left out of the configuration
  // unless you ask for it.
  import { rowAction } from "../lib/rowaction";
  import {
    api,
    errText,
    type RegistryRowView,
    type RegistryInstanceView,
    type DnsStatusViewT,
  } from "../lib/api";
  import { ago, app } from "../lib/state.svelte";
  import { staggerIn } from "../lib/motion";
  import { poll } from "../lib/poll";
  import { hueOf, hueStyle } from "../lib/hues";
  import { LB_KINDS, hostPort, lbConfig, portShape, type LbKind, type PortShape } from "../lib/loadbalancer";
  import LoadError from "../lib/components/LoadError.svelte";
  import Icon from "../lib/components/Icon.svelte";
  import Pill from "../lib/components/Pill.svelte";
  import Segments from "../lib/components/Segments.svelte";
  import Modal from "../lib/components/Modal.svelte";
  import Sheet from "../lib/components/Sheet.svelte";

  let rows = $state<RegistryRowView[]>([]);
  /** Every service's copies, fetched with the list: the port column needs
   *  them, and a mesh has a handful of services, not thousands. */
  let byService = $state<Record<string, RegistryInstanceView[]>>({});
  let dns = $state<DnsStatusViewT | null>(null);
  let err = $state<string | null>(null);
  let loaded = $state(false);
  let selected = $state<string | null>(null);

  async function load() {
    try {
      const next = await api.registry();
      const lists = await Promise.all(
        next.map((r) => api.registryService(r.service).catch(() => byService[r.service] ?? [])),
      );
      const map: Record<string, RegistryInstanceView[]> = {};
      next.forEach((r, i) => (map[r.service] = lists[i]));
      rows = next;
      byService = map;
      err = null;
      // A name being removed is "removing" until it has actually left.
      if (removing.size > 0) removing = new Set([...removing].filter((s) => next.some((r) => r.service === s)));
    } catch (e) {
      err = errText(e);
    } finally {
      loaded = true;
    }
  }

  // Copies come and go with the work behind them; an open sheet refreshes
  // in place, so a copy that fails while you are reading turns amber there.
  $effect(() => poll(load, 3000));

  $effect(() => {
    void (async () => {
      try {
        dns = await api.dnsStatus();
      } catch {
        // Only the SRV fold reads it.
      }
    })();
  });

  // A deep link from a deployment: `registry:open` names a service or the
  // deployment behind one.
  $effect(() => {
    void app.deepLink;
    if (!loaded) return;
    const want = sessionStorage.getItem("registry:open");
    if (!want) return;
    sessionStorage.removeItem("registry:open");
    const hit =
      rows.find((r) => r.service === want) ??
      rows.find((r) => (byService[r.service] ?? []).some((i) => i.deployment === want));
    if (hit) selected = hit.service;
  });

  type Tone = "ok" | "warn" | "error" | "muted";
  function verdict(r: RegistryRowView): { word: string; tone: Tone } {
    if (r.total === 0) return { word: "no copies", tone: "muted" };
    if (r.healthy === r.total) return { word: "ready", tone: "ok" };
    if (r.healthy === 0) return { word: "no healthy copy", tone: "error" };
    return { word: `${r.healthy} of ${r.total} healthy`, tone: "warn" };
  }

  function healthTone(i: RegistryInstanceView): "ok" | "warn" | "error" | "muted" {
    if (i.resolvable) return i.health === "passing" ? "ok" : "muted";
    return i.health === "starting" ? "warn" : "error";
  }

  function shapeOf(service: string): PortShape {
    return portShape((byService[service] ?? []).map((i) => i.port));
  }

  function deploymentsOf(service: string): string[] {
    return [...new Set((byService[service] ?? []).map((i) => i.deployment).filter(Boolean))];
  }

  /** `https` only where the copy says so; everything else is plain http. */
  function urlOf(i: RegistryInstanceView): string {
    const secure = i.port === 443 || /https|tls/i.test(i.label);
    return `${secure ? "https" : "http"}://${hostPort(i.address, i.port)}/`;
  }
  /** A browser is the wrong tool for a gRPC or database port. */
  function browsable(i: RegistryInstanceView): boolean {
    return !/grpc|tcp|udp|redis|postgres|mysql|mongo|amqp|mqtt|dns|smtp|db/i.test(i.label);
  }

  async function copy(text: string, what: string) {
    try {
      await api.copyText(text);
      app.flash(`Copied ${what}`);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  async function openUrl(url: string) {
    try {
      await api.openUrl(url);
    } catch (e) {
      app.flash(errText(e));
    }
  }

  function openDeployment(name: string) {
    sessionStorage.setItem("deployments:open", name);
    selected = null;
    app.go("deployments");
  }

  // -- removing a service (2026-09-14) -----------------------------------------
  //
  // A service is not a record anybody wrote down: each copy is listed
  // because a deployment or a one-off job is running under the name, and the
  // entry goes when that work stops. So "remove" is the verb for the work —
  // delete the deployment, stop the job — said plainly before it is done,
  // because deleting a deployment stops copies on other people's machines.
  let confirmRemove = $state<string | null>(null);
  let removeBusy = $state(false);
  let removeErr = $state<string | null>(null);
  /** Names whose removal was asked for and has not finished leaving. */
  let removing = $state<Set<string>>(new Set());

  function removalPlan(service: string) {
    const list = byService[service] ?? [];
    const deployments = [...new Set(list.map((i) => i.deployment).filter(Boolean))].map((name) => ({
      name,
      copies: list.filter((i) => i.deployment === name).length,
    }));
    return { deployments, oneOff: list.filter((i) => !i.deployment) };
  }
  const plan = $derived(confirmRemove ? removalPlan(confirmRemove) : null);

  async function doRemoveService() {
    const service = confirmRemove;
    const what = plan;
    if (!service || !what) return;
    removeBusy = true;
    removeErr = null;
    const failed: string[] = [];
    for (const d of what.deployments) {
      try {
        await api.deploymentRemove(d.name);
      } catch (e) {
        const message = errText(e);
        // Already deleted, with its copies still winding down: the outcome
        // asked for is on its way.
        if (!/no deployment named/.test(message)) failed.push(`${d.name}: ${message}`);
      }
    }
    for (const i of what.oneOff) {
      try {
        await api.jobCancel(i.instance);
      } catch (e) {
        failed.push(`job on ${i.node}: ${errText(e)}`);
      }
    }
    removeBusy = false;
    if (failed.length > 0) {
      removeErr = failed.join("\n");
      return;
    }
    removing = new Set([...removing, service]);
    confirmRemove = null;
    selected = null;
    app.flash(`Removing ${service} — it leaves this list once its copies have stopped`);
    void load();
  }

  // -- the page's numbers ------------------------------------------------------

  const instTotal = $derived(rows.reduce((n, r) => n + r.total, 0));
  const instHealthy = $derived(rows.reduce((n, r) => n + r.healthy, 0));
  const fixedCount = $derived(rows.filter((r) => shapeOf(r.service).kind === "fixed").length);
  const dynamicCount = $derived(rows.filter((r) => shapeOf(r.service).kind === "dynamic").length);

  // -- one service --------------------------------------------------------------

  const current = $derived(rows.find((r) => r.service === selected) ?? null);
  const instances = $derived(selected ? (byService[selected] ?? []) : []);
  const shape = $derived(portShape(instances.map((i) => i.port)));
  const labels = $derived([...new Set(instances.map((i) => i.label).filter(Boolean))]);
  const machines = $derived([...new Set(instances.map((i) => i.node))]);

  // What to write, remembered per viewer: the load balancer somebody runs
  // and the domain they serve do not change between services.
  const PREFS = "loadbalancing:prefs";
  const saved = (() => {
    try {
      return JSON.parse(localStorage.getItem(PREFS) ?? "{}") as { kind?: string; domain?: string; listen?: number };
    } catch {
      return {};
    }
  })();
  let lbKind = $state<LbKind>((LB_KINDS.find((k) => k.id === saved.kind)?.id ?? "nginx") as LbKind);
  let domain = $state(typeof saved.domain === "string" ? saved.domain : "example.com");
  let listen = $state(Number.isInteger(saved.listen) ? (saved.listen as number) : 80);
  let healthPath = $state("/");
  let includeUnhealthy = $state(false);
  $effect(() => {
    const value = JSON.stringify({ kind: lbKind, domain, listen });
    try {
      localStorage.setItem(PREFS, value);
    } catch {
      // A convenience, not state anybody depends on.
    }
  });

  const chosen = $derived(instances.filter((i) => includeUnhealthy || i.resolvable));
  const kind = $derived(LB_KINDS.find((k) => k.id === lbKind) ?? LB_KINDS[0]);
  const config = $derived(
    current
      ? lbConfig(
          lbKind,
          chosen.map((i) => ({ node: i.node, address: i.address, port: i.port })),
          { service: current.service, domain: domain.trim(), listen: Number(listen) || 80, healthPath: healthPath.trim() || "/" },
        )
      : "",
  );

  type Check = { tone: "ok" | "warn" | "info"; title: string; body: string };
  const checks = $derived.by((): Check[] => {
    if (!current || instances.length === 0) return [];
    const out: Check[] = [];
    const withheld = instances.filter((i) => !i.resolvable).length;
    if (instances.every((i) => i.health === "unknown")) {
      out.push({
        tone: "warn",
        title: "No health check is defined",
        body: "Every copy reports its health as unknown. Add a health check to the deployment so a broken copy can be told from a working one — here and in your load balancer.",
      });
    } else if (withheld === 0) {
      out.push({
        tone: "ok",
        title: `${instances.length === 1 ? "The copy is" : `All ${instances.length} copies are`} passing ${instances.length === 1 ? "its" : "their"} health check`,
        body: "Everything below is safe to send traffic to right now.",
      });
    } else {
      out.push({
        tone: "warn",
        title: `${withheld} of ${instances.length} ${instances.length === 1 ? "copy is" : "copies are"} not passing`,
        body: "They are left out of the configuration unless you include them. Open the deployment to see what is wrong with them.",
      });
    }
    if (shape.kind === "fixed") {
      out.push({
        tone: "ok",
        title: instances.length === 1 ? `It answers on port ${shape.port}` : `Every copy answers on port ${shape.port}`,
        body: "Your load balancer's list is a list of machines. It only changes when a copy moves to a different machine.",
      });
    } else if (shape.kind === "dynamic") {
      out.push({
        tone: "warn",
        title: `The copies answer on ${shape.ports.length} different ports`,
        body: "Each machine picked a free port, so this list changes whenever a copy restarts somewhere else. For a list that stays put, give the port a fixed number in the deployment (static: 8080) and run one copy per machine (count: system, or distinct_hosts) — or let your load balancer follow DNS SRV, below.",
      });
    }
    const ports = shape.kind === "fixed" ? `${shape.port}` : shape.kind === "dynamic" ? shape.ports.join(", ") : "";
    out.push({
      tone: "info",
      title: `Open port${shape.kind === "dynamic" ? "s" : ""} ${ports} on ${machines.length === 1 ? machines[0] : `${machines.length} machines`}`,
      body: "Meshware opens no firewall for a job. Allow these ports from the network your load balancer sits on.",
    });
    const envLabel = (labels[0] ?? "http").toUpperCase().replace(/[^A-Z0-9]+/g, "_");
    out.push({
      tone: "info",
      title: "Listen on every interface",
      body: `The service has to bind 0.0.0.0 — or $MESHWARE_IP_${envLabel} — not 127.0.0.1, or only its own machine can reach it.`,
    });
    return out;
  });

  /** The responder's address, split, for the SRV example. */
  const dnsAddr = $derived.by(() => {
    const bound = dns?.bound;
    if (!bound) return null;
    const at = bound.lastIndexOf(":");
    const host = bound.slice(0, at).replace(/^\[|\]$/g, "");
    return { host, port: bound.slice(at + 1), loopback: host === "127.0.0.1" || host === "::1" || host === "localhost" };
  });
</script>

{#if err}
  <LoadError class="mb-3" what="the services" error={err} retry={load} />
{/if}

<!-- The model, drawn once: whose part is whose. The middle station is the
     reader's, so it is the one in colour, outlined dashed — something they
     bring, not something Meshware runs.

     A diagram, not a stepper (UX-19): it was three numbered boxes with
     chevrons between them, which is exactly what a row of clickable steps
     looks like, and none of them did anything. Now it is three stations on
     a line with arrows, and nothing on it looks pressable. -->
<section class="lb-flow card mb-5" aria-label="How traffic reaches a service">
  <div class="lb-wrap">
    <ol class="lb-steps">
      <li class="lb-step">
        <span class="lb-step-title">People and programs</span>
        <span class="lb-step-sub">ask for <code class="num">https://example.com</code></span>
      </li>
      <li class="lb-arrow" aria-hidden="true"><span></span></li>
      <li class="lb-step lb-step-yours">
        <span class="lb-step-title">Your load balancer</span>
        <span class="lb-step-sub">nginx, HAProxy, Caddy, Traefik or a cloud one</span>
      </li>
      <li class="lb-arrow" aria-hidden="true"><span></span></li>
      <li class="lb-step">
        <span class="lb-step-title">Each copy, where it runs</span>
        <span class="lb-step-sub">a machine's address and port — <code class="num">192.168.1.31:8080</code></span>
      </li>
    </ol>
  </div>
  <p class="lb-flow-note">
    Meshware places every copy and gives it a port on the machine it runs on. There is no proxy in
    between: open a service below for its addresses, a checklist, and a configuration to paste into
    your load balancer.
  </p>
</section>

{#if rows.length > 0}
  <div class="mb-5 grid gap-3" style="grid-template-columns: repeat(3, minmax(0, 1fr));">
    <div class="card metric" title="Services with at least one registered copy">
      <div class="metric-label">Services</div>
      <div class="metric-fig">{rows.length}<span class="sub">with an address</span></div>
      <div class="metric-bar">
        {#each rows as r (r.service)}
          {@const v = verdict(r)}
          <span
            data-on={v.tone === "ok" ? "" : undefined}
            style={v.tone === "warn" ? "background: var(--status-warn);" : v.tone === "error" ? "background: var(--status-error);" : ""}
            title={`${r.service} · ${v.word}`}
          ></span>
        {/each}
      </div>
    </div>
    <div class="card metric" title="Copies passing their health check, of every copy registered">
      <div class="metric-label">Copies ready for traffic</div>
      <div class="metric-fig" style={instHealthy < instTotal ? "color: var(--status-warn-ink);" : ""}>
        {instHealthy}<span class="sub">of {instTotal}</span>
      </div>
      <div class="meter-track mt-[13px] h-[5px]">
        <div class="meter-fill" style={`width:${instTotal ? (instHealthy / instTotal) * 100 : 0}%; background: ${instHealthy < instTotal ? "var(--status-warn)" : "var(--status-ok)"};`}></div>
      </div>
    </div>
    <div class="card metric" title="Whether a load balancer's list of each service will stay put">
      <div class="metric-label">Ports</div>
      <div class="metric-fig">{fixedCount}<span class="sub">fixed · {dynamicCount} dynamic</span></div>
      <div class="metric-note">
        {dynamicCount === 0 ? "every list stays put while machines do" : "a dynamic list changes when a copy moves"}
      </div>
    </div>
  </div>
{/if}

<div class="section-head">
  <span class="section-title">Services</span>
  <span class="section-sum">{rows.length === 0 ? (err ? "could not be read" : "nothing yet") : "open one for its addresses and a configuration"}</span>
</div>

{#if rows.length === 0 && err}
  <!-- Said above: a refused read is not an empty list (UX-2). -->
{:else if rows.length === 0}
  <div class="card empty">
    <div class="empty-title">{loaded ? "Nothing to load balance yet" : "Loading…"}</div>
    {#if loaded}
      <div class="empty-sub">
        A deployment that declares a port and registers a name appears here with the address of
        every copy — the list a load balancer needs. From a terminal:
        <code class="num">meshnode registry &lt;name&gt;</code>.
      </div>
      <div class="mt-4 flex justify-center">
        <button class="btn btn-secondary btn-md" onclick={() => app.go("deployments")}>Open Deployments</button>
      </div>
    {/if}
  </div>
{:else}
  <div class="card overflow-hidden">
    <table class="w-full text-sm">
      <thead>
        <tr class="border-b border-edge">
          <th class="label px-5 py-2.5 text-left">Service</th>
          <th class="label px-3 py-2.5 text-left">From</th>
          <th class="label w-[190px] px-3 py-2.5 text-left">Copies healthy</th>
          <th class="label px-3 py-2.5 text-left">Port</th>
          <th class="label w-[150px] px-3 py-2.5 text-left">Status</th>
          <th class="label w-[44px] px-3 py-2.5"></th>
        </tr>
      </thead>
      <tbody use:staggerIn={{ selector: ":scope > tr", each: 14, y: 5 }}>
        {#each rows as r (r.service)}
          {@const v = verdict(r)}
          {@const s = shapeOf(r.service)}
          {@const deps = deploymentsOf(r.service)}
          <tr class="cursor-pointer border-b border-edge/60 transition-colors last:border-0 hover:bg-surface-2" onclick={() => (selected = r.service)} use:rowAction={() => (selected = r.service)}>
            <td class="px-5 py-3 font-semibold text-ink">
              <span class="inline-flex items-center gap-2" style={hueStyle(hueOf(r.service))}>
                <span class="name-dot"></span>{r.service}
              </span>
            </td>
            <td class="px-3 py-3 text-xs text-ink-4">
              {#if deps.length === 0}
                <span class="text-ink-5" title="A one-off job registered this, not a deployment">one-off job</span>
              {:else}
                {deps.join(", ")}
              {/if}
            </td>
            <td class="px-3 py-3">
              <span class="inline-flex items-center gap-2.5">
                <Segments total={r.total} on={r.healthy} tone={v.tone === "error" ? "var(--status-error)" : "var(--status-ok)"} />
                <span class="num text-xs" style={`color: ${v.tone === "ok" ? "var(--status-ok-ink)" : v.tone === "warn" ? "var(--status-warn-ink)" : "var(--status-error-ink)"};`}>
                  {r.healthy}/{r.total}
                </span>
              </span>
            </td>
            <td class="px-3 py-3">
              {#if s.kind === "fixed"}
                <span class="num text-ink-2">{s.port}</span>
                {#if r.total > 1}<span class="ml-1.5 text-xs text-ink-5">on every copy</span>{/if}
              {:else if s.kind === "dynamic"}
                <span class="text-xs text-ink-3">{s.ports.length} different</span>
                <span class="ml-1.5 text-xs" style="color: var(--status-warn-ink);" title="Each machine picked its own port, so the list changes when a copy moves">moves with copies</span>
              {:else}
                <span class="text-ink-5">—</span>
              {/if}
            </td>
            <td class="px-3 py-3">
              {#if removing.has(r.service)}
                <Pill tone="muted" live title="Its copies are stopping; it leaves this list when they have">removing</Pill>
              {:else}
                <Pill tone={v.tone}>{v.word}</Pill>
              {/if}
            </td>
            <td class="px-3 py-3 text-right"><span class="rowchev inline-flex"><Icon name="chevron-right" size={14} /></span></td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
  <div class="footnote">
    A copy that is not passing its health check stays listed, so you can see it, and is left out of
    the configuration a service writes for you.
  </div>
{/if}

<!-- ═══════════════ one service ═══════════════ -->
<Sheet
  open={selected !== null}
  title={current?.service ?? selected ?? ""}
  sub={current
    ? `${current.total} cop${current.total === 1 ? "y" : "ies"} on ${machines.length} machine${machines.length === 1 ? "" : "s"}${deploymentsOf(current.service).length ? ` · from ${deploymentsOf(current.service).join(", ")}` : ""}`
    : ""}
  icon="registry"
  width={920}
  onclose={() => (selected = null)}
>
  {#snippet aside()}
    {#if current}
      {@const v = verdict(current)}
      <Pill tone={v.tone} live={v.tone === "ok"}>{v.word}</Pill>
    {/if}
  {/snippet}

  {#if instances.length === 0}
    <div class="py-6 text-center text-xs text-ink-5">{current && current.total === 0 ? "Nothing is registered under this name right now." : "Loading copies…"}</div>
  {:else}
    <!-- 1. The addresses. -->
    <section aria-label="Backends">
      <div class="lb-head">
        <div class="min-w-0">
          <div class="lb-h">Backends</div>
          <div class="lb-hsub">The addresses your load balancer sends traffic to — one for each copy.</div>
        </div>
        <button
          class="btn btn-secondary btn-md ml-auto"
          disabled={chosen.length === 0}
          onclick={() => void copy(chosen.map((i) => hostPort(i.address, i.port)).join("\n"), `${chosen.length} address${chosen.length === 1 ? "" : "es"}`)}
        >
          <Icon name="copy" size={13} /> Copy {includeUnhealthy ? "all" : "healthy"} addresses
        </button>
      </div>
      <div class="overflow-hidden rounded-lg" style="box-shadow: 0 0 0 1px var(--edge-card);">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b border-edge">
              <th class="label px-4 py-2 text-left">Machine</th>
              <th class="label px-3 py-2 text-left">Address</th>
              <th class="label px-3 py-2 text-left">Health</th>
              <th class="label px-3 py-2 text-left">Since</th>
              <th class="label px-4 py-2 text-right"><span class="sr-only">Test</span></th>
            </tr>
          </thead>
          <tbody>
            {#each instances as i (i.instance + i.label)}
              <tr class="border-b border-edge/60 last:border-0" style={i.resolvable ? "" : "background: color-mix(in srgb, var(--status-warn-wash) 45%, transparent);"}>
                <td class="px-4 py-2.5">
                  <span class="mono inline-flex items-center gap-1.5 text-xs text-ink-2" style={hueStyle(hueOf(i.node))}>
                    <span class="name-dot"></span>{i.node}
                  </span>
                </td>
                <td class="px-3 py-2.5">
                  <span class="idchip text-xs">
                    {hostPort(i.address, i.port)}
                    <button title="Copy this address" aria-label={`Copy ${hostPort(i.address, i.port)}`} onclick={() => void copy(hostPort(i.address, i.port), hostPort(i.address, i.port))}>
                      <Icon name="copy" size={12} />
                    </button>
                  </span>
                  {#if i.label}<span class="hue-chip ml-1.5" style={hueStyle(hueOf(i.label))}>{i.label}</span>{/if}
                </td>
                <td class="px-3 py-2.5">
                  <Pill tone={healthTone(i)} live={i.resolvable && i.health === "passing"} title={i.resolvable ? "Ready for traffic" : "Not passing — left out of the configuration"}>
                    {i.health}
                  </Pill>
                </td>
                <td class="num px-3 py-2.5 text-xs text-ink-5">{i.sinceMicros ? ago(i.sinceMicros) : "—"}</td>
                <td class="px-4 py-2.5 text-right">
                  {#if browsable(i)}
                    <button class="btn btn-ghost h-7 px-2 text-xs" title={`Open ${urlOf(i)} in your browser`} onclick={() => void openUrl(urlOf(i))}>Open ↗</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if instances[0] && browsable(instances[0])}
        <div class="footnote">
          Or test one from a terminal: <code class="num selectable">curl -i {urlOf(chosen[0] ?? instances[0])}</code>
        </div>
      {/if}
    </section>

    <!-- 2. What has to be true. -->
    <section class="mt-6" aria-label="Checklist">
      <div class="lb-h">Before you point traffic at it</div>
      <ul class="lb-checks">
        {#each checks as c (c.title)}
          <li class="lb-check" data-tone={c.tone}>
            <span class="lb-check-mark" aria-hidden="true">
              <Icon name={c.tone === "ok" ? "check" : c.tone === "warn" ? "alert" : "info"} size={13} />
            </span>
            <span class="min-w-0">
              <span class="lb-check-title">{c.title}</span>
              <span class="lb-check-body">{c.body}</span>
            </span>
          </li>
        {/each}
      </ul>
    </section>

    <!-- 3. What to paste. -->
    <section class="mt-6" aria-label="Load balancer configuration">
      <div class="lb-head">
        <div class="min-w-0">
          <div class="lb-h">Load balancer configuration</div>
          <div class="lb-hsub">Written from the backends above, and rewritten as they change.</div>
        </div>
      </div>
      <div class="lb-config">
        <div class="lb-config-bar">
          <div class="seg" role="tablist" aria-label="Load balancer">
            {#each LB_KINDS as k (k.id)}
              <button role="tab" aria-selected={lbKind === k.id} onclick={() => (lbKind = k.id)}>{k.label}</button>
            {/each}
          </div>
          <button class="btn btn-primary btn-md ml-auto" onclick={() => void copy(config, `the ${kind.label} configuration`)}>
            <Icon name="copy" size={13} /> Copy
          </button>
        </div>
        <div class="lb-config-fields">
          {#if lbKind !== "list"}
            <label class="lb-field">
              <span class="label">Domain</span>
              <input class="lb-input" bind:value={domain} placeholder="example.com" spellcheck="false" />
            </label>
            <label class="lb-field" style="width: 96px;">
              <span class="label">Listens on</span>
              <input class="lb-input num" type="number" min="1" max="65535" bind:value={listen} />
            </label>
            {#if lbKind !== "nginx"}
              <label class="lb-field" style="width: 150px;">
                <span class="label">Health path</span>
                <input class="lb-input mono" bind:value={healthPath} placeholder="/" spellcheck="false" />
              </label>
            {/if}
          {/if}
          <label class="lb-include" title="Copies that are not passing their health check">
            <input type="checkbox" class="check" bind:checked={includeUnhealthy} />
            Include copies that are not healthy
          </label>
        </div>
        <pre class="lb-code selectable">{config}</pre>
        <div class="lb-config-note">{kind.note(current?.service ?? "service")}</div>
      </div>
    </section>

    <!-- 4. Following copies without a list. -->
    <details class="lb-more mt-6">
      <summary>Advanced: follow copies automatically with DNS SRV</summary>
      <div class="lb-more-body">
        <p class="m-0">
          A load balancer that reads SRV records can track copies wherever they land, dynamic ports
          included: <code class="num">{current?.fqdn}</code> answers with each healthy copy's
          address and port.
        </p>
        {#if dns?.enabled && dnsAddr}
          <pre class="lb-code selectable mt-3">dig @{dnsAddr.host} -p {dnsAddr.port} {current?.fqdn} SRV</pre>
          {#if dnsAddr.loopback}
            <p class="m-0 mt-2 text-ink-4">
              This machine's responder listens on <code class="num">{dns.bound}</code>, which only this
              machine can reach. To let a load balancer ask it, set <code class="num">[network] dns_bind</code>
              to an address the load balancer can reach — deliberately.
            </p>
          {/if}
        {:else}
          <p class="m-0 mt-2 text-ink-4">
            This machine's DNS responder is off (<code class="num">dns_bind</code> in Director's
            settings, or <code class="num">[network] dns</code> on a node).
          </p>
        {/if}
      </div>
    </details>
  {/if}

  {#snippet footer()}
    {#if current}
      {#each deploymentsOf(current.service) as d (d)}
        <button class="btn btn-secondary btn-md" onclick={() => openDeployment(d)}>Open deployment {d}</button>
      {/each}
      <button class="btn btn-secondary btn-md ml-auto" onclick={() => (selected = null)}>Close</button>
      <button
        class="btn btn-danger btn-md"
        disabled={removing.has(current.service) || instances.length === 0}
        title="Stop what is running under this name, so it leaves the list"
        onclick={() => {
          removeErr = null;
          confirmRemove = current.service;
        }}>{removing.has(current.service) ? "Removing…" : "Remove…"}</button
      >
    {/if}
  {/snippet}
</Sheet>

<!-- Removing a service stops work, possibly on other people's machines, so
     it is said before it is done — in the same dark confirmation Deployments
     uses for Delete, with Keep it on the left. -->
<Modal
  open={confirmRemove !== null}
  kicker="remove service"
  peer={confirmRemove ?? ""}
  onclose={() => {
    if (!removeBusy) confirmRemove = null;
  }}
>
  <div class="px-6 pt-4 pb-6">
    <h3 class="m-0 text-lg font-bold tracking-[-.02em]">Remove {confirmRemove}?</h3>
    <p class="mt-2 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-3);">
      A service is listed because something is running under its name. Removing it stops that:
    </p>
    {#if plan}
      <ul class="mt-3 mb-0 list-disc space-y-1.5 pl-5 text-sm leading-relaxed" style="color: var(--ink-code-2);">
        {#each plan.deployments as d (d.name)}
          <li>
            Delete the deployment <span class="mono">{d.name}</span> — its {d.copies} cop{d.copies === 1 ? "y" : "ies"}
            stop on every machine, and its definition is forgotten.
          </li>
        {/each}
        {#if plan.oneOff.length > 0}
          <li>Stop the {plan.oneOff.length} one-off job{plan.oneOff.length === 1 ? "" : "s"} that registered this name.</li>
        {/if}
      </ul>
    {/if}
    <p class="mt-3 mb-0 text-sm leading-relaxed" style="color: var(--ink-code-4);">
      It leaves this list once those copies have stopped, usually within a few seconds.
      {#if plan && plan.deployments.length > 0}
        To keep a deployment running without this name, edit its <span class="mono">register</span> block instead.
      {/if}
    </p>
    {#if removeErr}
      <div class="selectable mt-3 rounded-sm px-3 py-2 text-sm whitespace-pre-line" style="background: var(--status-error-wash); color: var(--status-error-ink);">{removeErr}</div>
    {/if}
    <div class="mt-5 grid grid-cols-2 gap-2.5">
      <button
        class="h-11 cursor-pointer rounded-md border text-base font-semibold text-inkcode-2"
        style="border-color: var(--edge-code); background: transparent;"
        disabled={removeBusy}
        onclick={() => (confirmRemove = null)}>Keep it</button
      >
      <button
        class="h-11 cursor-pointer rounded-md border text-base font-semibold transition-colors disabled:opacity-40"
        style="border-color: var(--status-error-edge); color: var(--status-error-ink);"
        disabled={removeBusy || !plan || (plan.deployments.length === 0 && plan.oneOff.length === 0)}
        onclick={() => void doRemoveService()}>{removeBusy ? "Removing…" : "Remove"}</button
      >
    </div>
  </div>
</Modal>

<style>
  .lb-flow {
    padding: 18px 20px 16px;
  }
  .lb-wrap {
    container-type: inline-size;
  }
  .lb-steps {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 64px minmax(0, 1fr) 64px minmax(0, 1fr);
    align-items: center;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .lb-step {
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 12px 14px;
    text-align: center;
  }
  /* The reader's part of the path: tinted, and dashed, because it is theirs. */
  .lb-step-yours {
    border: 1.5px dashed color-mix(in srgb, var(--hue, var(--accent)) 55%, transparent);
    border-radius: var(--radius-lg);
    background: var(--hue-wash, var(--accent-wash));
  }
  .lb-step-title {
    display: block;
    font-size: var(--text-sm);
    font-weight: 600;
    color: var(--ink-1);
  }
  .lb-step-sub {
    display: block;
    margin-top: 2px;
    font-size: var(--text-xs);
    line-height: 1.45;
    color: var(--ink-4);
  }
  /* A line with a head, not a chevron: chevrons between boxes are how a
     row of steps you can press is drawn. */
  .lb-arrow {
    display: flex;
    align-items: center;
    height: 100%;
    padding: 0 6px;
  }
  .lb-arrow > span {
    position: relative;
    display: block;
    width: 100%;
    height: 1.5px;
    background: var(--ink-faint);
  }
  .lb-arrow > span::after {
    content: "";
    position: absolute;
    top: 50%;
    right: -1px;
    width: 7px;
    height: 7px;
    border-top: 1.5px solid var(--ink-faint);
    border-right: 1.5px solid var(--ink-faint);
    transform: translateY(-50%) rotate(45deg);
  }
  /* Narrow: the same line, top to bottom. */
  @container (max-width: 620px) {
    .lb-steps {
      grid-template-columns: minmax(0, 1fr);
    }
    .lb-arrow {
      justify-content: center;
      height: 28px;
      padding: 4px 0;
    }
    .lb-arrow > span {
      width: 1.5px;
      height: 100%;
    }
    .lb-arrow > span::after {
      top: auto;
      right: 50%;
      bottom: -1px;
      transform: translateX(50%) rotate(135deg);
    }
  }
  .lb-flow-note {
    margin: 12px 2px 0;
    max-width: 88ch;
    font-size: var(--text-sm);
    line-height: 1.5;
    color: var(--ink-4);
  }

  .lb-head {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    gap: 12px;
    margin-bottom: 10px;
  }
  .lb-h {
    font-size: var(--text-md, 15px);
    font-weight: 650;
    letter-spacing: -0.01em;
    color: var(--ink-1);
  }
  .lb-hsub {
    margin-top: 2px;
    font-size: var(--text-sm);
    color: var(--ink-5);
  }

  .lb-checks {
    display: grid;
    gap: 8px;
    margin: 10px 0 0;
    padding: 0;
    list-style: none;
  }
  .lb-check {
    display: flex;
    gap: 11px;
    padding: 11px 13px;
    border-radius: var(--radius-md);
    background: var(--surface-subtle);
    box-shadow: inset 0 0 0 1px var(--edge);
  }
  .lb-check-mark {
    display: grid;
    flex: none;
    place-items: center;
    width: 22px;
    height: 22px;
    border-radius: 999px;
    background: var(--surface-3);
    color: var(--ink-4);
  }
  .lb-check[data-tone="ok"] .lb-check-mark {
    background: var(--status-ok-wash);
    color: var(--status-ok-ink);
  }
  .lb-check[data-tone="warn"] {
    background: color-mix(in srgb, var(--status-warn-wash) 60%, var(--surface-subtle));
  }
  .lb-check[data-tone="warn"] .lb-check-mark {
    background: var(--status-warn-wash);
    color: var(--status-warn-ink);
  }
  .lb-check-title {
    display: block;
    font-size: var(--text-sm);
    font-weight: 600;
    color: var(--ink-1);
  }
  .lb-check-body {
    display: block;
    margin-top: 2px;
    font-size: var(--text-sm);
    line-height: 1.5;
    color: var(--ink-4);
  }

  .lb-config {
    overflow: hidden;
    border-radius: var(--radius-lg);
    box-shadow: 0 0 0 1px var(--edge-card);
  }
  .lb-config-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    padding: 10px 12px;
    background: var(--surface-subtle);
    border-bottom: 1px solid var(--edge);
  }
  .lb-config-fields {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    gap: 12px;
    padding: 12px 14px;
    border-bottom: 1px solid var(--edge);
  }
  .lb-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    width: 220px;
  }
  .lb-input {
    height: 32px;
    padding: 0 10px;
    border-radius: var(--radius-md);
    border: 1px solid var(--edge);
    background: transparent;
    font-size: var(--text-sm);
    color: var(--ink-1);
    outline: none;
  }
  .lb-input:focus-visible {
    box-shadow: var(--shadow-focus);
  }
  .lb-include {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    height: 32px;
    margin-left: auto;
    font-size: var(--text-sm);
    color: var(--ink-3);
    cursor: pointer;
  }
  .lb-code {
    margin: 0;
    max-height: 340px;
    overflow: auto;
    padding: 14px 16px;
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 12px;
    line-height: 1.6;
    color: var(--ink-2);
    background: var(--surface-2);
    white-space: pre;
  }
  .lb-config-note {
    padding: 10px 14px;
    font-size: var(--text-xs);
    line-height: 1.5;
    color: var(--ink-5);
    border-top: 1px solid var(--edge);
  }

  .lb-more {
    border-radius: var(--radius-md);
    box-shadow: inset 0 0 0 1px var(--edge);
  }
  .lb-more > summary {
    padding: 10px 13px;
    font-size: var(--text-sm);
    font-weight: 600;
    color: var(--ink-3);
    cursor: pointer;
    user-select: none;
  }
  .lb-more-body {
    padding: 0 13px 13px;
    font-size: var(--text-sm);
    line-height: 1.5;
    color: var(--ink-3);
  }
  .lb-more-body .lb-code {
    border-radius: var(--radius-md);
  }
</style>
