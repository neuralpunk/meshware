<script lang="ts">
  // The deployment builder: a form and the file it writes, side by side.
  //
  // **Why a form at all.** Nomad's UI gives you one big textarea and a Plan
  // button, which is a fine tool for somebody who already knows the job
  // spec and a wall for everybody else — you cannot start from an empty box
  // in a language you have not read yet. Meshware's spec is smaller, so a
  // form can cover essentially all of it without becoming a wizard.
  //
  // **Why the file is still there.** Because the file is the real artifact:
  // it goes in a repo, it gets reviewed, it is what `meshnode deployment
  // put` takes. Hiding it would teach people that Director is a different
  // product from the CLI. So the form writes it, live, and you can read
  // along — which is also how somebody learns the spec without being made
  // to study it first.
  //
  // The form is the source of truth here. `Edit as text` hands the
  // generated file to the plain editor and says plainly that it is a
  // one-way door for this session, rather than pretending at a round trip
  // that would silently drop anything the form cannot express.
  import { ui } from "../state.svelte";
  import Icon from "./Icon.svelte";

  type Port = { label: string; to: string; fixed: string };
  type EnvRow = { k: string; v: string };

  type Props = {
    /** Services already published, for the "run something published" path. */
    services: { name: string; version: number }[];
    /** Live match count for the selector, or null while unknown. */
    matches?: number | null;
    /** Called whenever the built file changes. */
    onchange: (yaml: string) => void;
    /** Hand the current file to the plain text editor. */
    ontext: (yaml: string) => void;
  };

  let { services, matches = null, onchange, ontext }: Props = $props();

  // ── what runs ────────────────────────────────────────────────────────
  let kind = $state<"container" | "service">("container");
  let name = $state("");
  let image = $state("");
  let svcName = $state("");
  let svcVersion = $state("");
  let command = $state("");
  let cmdArgs = $state("");

  // ── how many, and where ──────────────────────────────────────────────
  let perMachine = $state(false);
  let count = $state("2");
  let targets = $state("any");

  // ── ports ────────────────────────────────────────────────────────────
  let ports = $state<Port[]>([{ label: "http", to: "80", fixed: "" }]);
  let registerAs = $state("");

  // ── health ───────────────────────────────────────────────────────────
  let healthKind = $state<"none" | "tcp" | "http">("none");
  let healthPort = $state("http");
  let healthPath = $state("/");
  let healthInterval = $state("10s");
  let healthGrace = $state("10s");

  // ── resources ────────────────────────────────────────────────────────
  let cpus = $state("");
  let memoryMb = $state("");

  // ── environment ──────────────────────────────────────────────────────
  let env = $state<EnvRow[]>([]);

  // ── update policy ────────────────────────────────────────────────────
  let advanced = $state(false);
  let canary = $state("1");
  let maxUnavailable = $state("1");
  let autoRevert = $state(true);
  let progressDeadline = $state("10m");
  let minHealthy = $state("30s");

  const namedPorts = $derived(ports.filter((p) => p.label.trim()));

  /** The name a deployment would take if you have not typed one. */
  const suggested = $derived.by(() => {
    if (kind === "service") return svcName.trim();
    const ref = image.trim();
    if (!ref) return "";
    // `ghcr.io/acme/api:1.4` → `api`. The last path segment without its
    // tag is what a person calls the thing.
    const last = ref.split("/").pop() ?? ref;
    return last.split(":")[0].split("@")[0];
  });
  const effectiveName = $derived(name.trim() || suggested);

  function q(s: string): string {
    // Quote when YAML would otherwise read it as something else. Plain
    // words stay plain, because a file full of needless quotes is a file
    // that reads like it was generated.
    return /^[A-Za-z][A-Za-z0-9._\/-]*$/.test(s) ? s : JSON.stringify(s);
  }

  const yaml = $derived.by(() => {
    const L: string[] = ["deployment:"];
    L.push(`  name: ${q(effectiveName || "unnamed")}`);
    if (kind === "service" && svcName.trim()) {
      const v = svcVersion.trim();
      L.push(`  service: ${q(svcName.trim() + (v ? `@${v}` : ""))}`);
    }
    L.push(`  count: ${perMachine ? "system" : count.trim() || "1"}`);
    if (targets.trim() && targets.trim() !== "any") L.push(`  targets: ${q(targets.trim())}`);

    if (kind === "container") {
      L.push("", "  container:");
      L.push(`    image: ${q(image.trim() || "nginx:alpine")}`);
      if (command.trim()) L.push(`    command: ${q(command.trim())}`);
      const args = cmdArgs.split("\n").map((a) => a.trim()).filter(Boolean);
      if (args.length) L.push(`    args: [${args.map((a) => JSON.stringify(a)).join(", ")}]`);
    }

    if (namedPorts.length) {
      L.push("", "  network:", "    port:");
      for (const p of namedPorts) {
        L.push(`      - label: ${q(p.label.trim())}`);
        if (p.to.trim()) L.push(`        to: ${p.to.trim()}`);
        if (p.fixed.trim()) L.push(`        static: ${p.fixed.trim()}`);
      }
    }

    if (cpus.trim() || memoryMb.trim()) {
      L.push("", "  resources:");
      if (cpus.trim()) L.push(`    cpus: ${cpus.trim()}`);
      if (memoryMb.trim()) L.push(`    memory_mb: ${memoryMb.trim()}`);
    }

    if (registerAs.trim()) {
      L.push("", "  register:", `    name: ${q(registerAs.trim())}`);
    }

    if (healthKind !== "none") {
      L.push("", "  health:", `    kind: ${healthKind}`);
      if (healthPort.trim()) L.push(`    port: ${q(healthPort.trim())}`);
      if (healthKind === "http" && healthPath.trim()) L.push(`    path: ${q(healthPath.trim())}`);
      if (healthInterval.trim()) L.push(`    interval: ${q(healthInterval.trim())}`);
      if (healthGrace.trim()) L.push(`    grace: ${q(healthGrace.trim())}`);
    }

    if (advanced) {
      L.push("", "  update:");
      if (maxUnavailable.trim()) L.push(`    max_unavailable: ${maxUnavailable.trim()}`);
      if (canary.trim()) L.push(`    canary: ${canary.trim()}`);
      L.push(`    auto_revert: ${autoRevert}`);
      if (progressDeadline.trim()) L.push(`    progress_deadline: ${q(progressDeadline.trim())}`);
      if (minHealthy.trim()) L.push(`    min_healthy_time: ${q(minHealthy.trim())}`);
    }

    return L.filter((l, i) => !(l === "" && (i === 0 || L[i - 1] === ""))).join("\n") + "\n";
  });

  // Environment rides *inside* the container block, so it is spliced in
  // rather than appended — kept separate so the generator above stays a
  // straight run down the file.
  const withEnv = $derived.by(() => {
    const rows = env.filter((r) => r.k.trim());
    if (!rows.length || kind !== "container") return yaml;
    const lines = yaml.split("\n");
    const at = lines.findIndex((l) => l.startsWith("  container:"));
    if (at < 0) return yaml;
    let end = at + 1;
    while (end < lines.length && lines[end].startsWith("    ")) end += 1;
    const block = ["    env:", ...rows.map((r) => `      ${q(r.k.trim())}: ${q(r.v)}`)];
    return [...lines.slice(0, end), ...block, ...lines.slice(end)].join("\n");
  });

  $effect(() => onchange(withEnv));

  const nameProblem = $derived(
    effectiveName && !/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(effectiveName)
      ? "Letters, digits, '-', '_' or '.', up to 64."
      : null,
  );
</script>

<div class="grid gap-4" style="grid-template-columns: minmax(0,1fr) minmax(340px, 42%);">
  <!-- ══════════════ the form ══════════════ -->
  <div class="flex flex-col gap-4">
    <!-- what runs -->
    <div class="card px-5 py-4">
      <div class="microlabel mb-3">What should run?</div>

      <div class="mb-4 flex gap-2">
        <button
          class="flex-1 cursor-pointer rounded-md border px-3 py-2.5 text-left transition-colors"
          style={kind === "container"
            ? "border-color: var(--accent); background: var(--accent-wash);"
            : "border-color: var(--edge); background: transparent;"}
          onclick={() => (kind = "container")}
        >
          <span class="block text-sm font-semibold text-ink">A container image</span>
          <span class="block text-xs text-ink-5">nginx, your app, anything on a registry</span>
        </button>
        <button
          class="flex-1 cursor-pointer rounded-md border px-3 py-2.5 text-left transition-colors"
          style={kind === "service"
            ? "border-color: var(--accent); background: var(--accent-wash);"
            : "border-color: var(--edge); background: transparent;"}
          onclick={() => (kind = "service")}
        >
          <span class="block text-sm font-semibold text-ink">Something published</span>
          <span class="block text-xs text-ink-5">a script or service already on the mesh</span>
        </button>
      </div>

      {#if kind === "container"}
        <label class="block">
          <span class={ui.label}>Image</span>
          <input bind:value={image} class={ui.inputMono} placeholder="nginx:alpine" />
          <span class="mt-1 block text-xs text-ink-5">
            Nothing to push or build — the mesh pulls it. A registry host is optional;
            <code class="num">nginx:alpine</code> and
            <code class="num">ghcr.io/acme/api:1.4</code> both work.
          </span>
        </label>
      {:else}
        <div class="grid gap-3" style="grid-template-columns: minmax(0,1fr) 110px;">
          <label class="block">
            <span class={ui.label}>Service</span>
            <select bind:value={svcName} class={`${ui.input} cursor-pointer`}>
              <option value="">choose one…</option>
              {#each services as s (s.name)}<option value={s.name}>{s.name}</option>{/each}
            </select>
          </label>
          <label class="block">
            <span class={ui.label}>Version</span>
            <input bind:value={svcVersion} class={ui.inputMono} placeholder="1" inputmode="numeric" />
          </label>
        </div>
        <span class="mt-1 block text-xs text-ink-5">
          Pinned on purpose: what stays up must not be a moving target. Changing it later
          is a deliberate roll.
        </span>
      {/if}

      <label class="mt-3 block">
        <span class={ui.label}>Call it</span>
        <input bind:value={name} class={ui.inputMono} placeholder={suggested || "web"} />
        {#if nameProblem}
          <span class="mt-1 block text-xs" style="color: var(--status-error-ink);">{nameProblem}</span>
        {:else if !name.trim() && suggested}
          <span class="mt-1 block text-xs text-ink-5">Using <code class="num">{suggested}</code>.</span>
        {/if}
      </label>
    </div>

    <!-- how many, where -->
    <div class="card px-5 py-4">
      <div class="microlabel mb-3">How many, and where?</div>
      <div class="flex flex-wrap items-end gap-3">
        <label class="block w-28">
          <span class={ui.label}>Copies</span>
          <input
            bind:value={count}
            class={ui.inputMono}
            inputmode="numeric"
            disabled={perMachine}
            placeholder="2"
          />
        </label>
        <label class="flex items-center gap-2 pb-3 text-sm text-ink-3">
          <input type="checkbox" bind:checked={perMachine} />
          One per machine
        </label>
        <label class="block min-w-48 flex-1">
          <span class={ui.label}>On which machines</span>
          <input bind:value={targets} class={ui.inputMono} placeholder="any" />
        </label>
      </div>
      <div class="mt-2 text-xs text-ink-5">
        {#if perMachine}
          Every machine that matches gets one — and a fixed host port is safe, because
          there is never a second copy to collide with.
        {:else}
          The mesh spreads copies across machines. A machine can still take two if it is
          the only one that matches.
        {/if}
        {#if matches != null}
          <span class="ml-1 text-ink-4">
            · <span class="num">{matches}</span> machine{matches === 1 ? "" : "s"} match right now
          </span>
        {/if}
      </div>
    </div>

    <!-- ports -->
    <div class="card px-5 py-4">
      <div class="mb-3 flex items-center gap-2">
        <span class="microlabel">Networking</span>
        <button
          class="ml-auto cursor-pointer text-xs font-semibold"
          style="color: var(--accent-ink);"
          onclick={() => (ports = [...ports, { label: "", to: "", fixed: "" }])}>+ Add a port</button
        >
      </div>

      {#if ports.length === 0}
        <p class="m-0 text-xs text-ink-5">
          No ports. Fine for a worker that only makes outbound connections.
        </p>
      {:else}
        <div class="flex flex-col gap-2">
          {#each ports as p, i (i)}
            <div class="flex items-end gap-2">
              <label class="block w-28">
                {#if i === 0}<span class={ui.label}>Name</span>{/if}
                <input bind:value={p.label} class={ui.inputMono} placeholder="http" />
              </label>
              <label class="block w-32">
                {#if i === 0}<span class={ui.label}>App listens on</span>{/if}
                <input bind:value={p.to} class={ui.inputMono} inputmode="numeric" placeholder="80" />
              </label>
              <label class="block w-32">
                {#if i === 0}<span class={ui.label}>Fixed host port</span>{/if}
                <input bind:value={p.fixed} class={ui.inputMono} inputmode="numeric" placeholder="auto" />
              </label>
              <button
                class="mb-1 cursor-pointer px-1 text-ink-5 hover:text-ink-2"
                title="Remove this port"
                onclick={() => (ports = ports.filter((_, j) => j !== i))}
                ><Icon name="close" size={15} /></button
              >
            </div>
          {/each}
        </div>
        <div class="mt-2 text-xs text-ink-5">
          <strong class="text-ink-4">App listens on</strong> is the port inside the container —
          80 for nginx. The mesh picks a free host port and forwards to it, so the image
          runs unchanged. Leave <strong class="text-ink-4">Fixed host port</strong> empty
          unless something outside the mesh must reach a known number.
        </div>
      {/if}

      <label class="mt-3 block">
        <span class={ui.label}>Reachable as <span class="text-ink-6">(optional)</span></span>
        <input
          bind:value={registerAs}
          class={ui.inputMono}
          placeholder={effectiveName || "web"}
          disabled={namedPorts.length === 0}
        />
        <span class="mt-1 block text-xs text-ink-5">
          {#if namedPorts.length === 0}
            Needs a port first — a name with no address resolves to nothing.
          {:else}
            Answers at <code class="num">{(registerAs.trim() || effectiveName || "name")}.mesh</code>
            from anywhere on the mesh, however many copies there are.
          {/if}
        </span>
      </label>
    </div>

    <!-- health -->
    <div class="card px-5 py-4">
      <div class="microlabel mb-3">How do we know it is alive?</div>
      <div class="flex flex-wrap items-end gap-3">
        <label class="block w-56">
          <span class={ui.label}>Check</span>
          <select bind:value={healthKind} class={`${ui.input} cursor-pointer`}>
            <option value="none">Just that it is running</option>
            <option value="tcp">The port accepts</option>
            <option value="http">An HTTP request</option>
          </select>
        </label>
        {#if healthKind !== "none"}
          <label class="block w-32">
            <span class={ui.label}>Which port</span>
            <select bind:value={healthPort} class={`${ui.input} cursor-pointer`}>
              {#each namedPorts as p (p.label)}<option value={p.label}>{p.label}</option>{/each}
            </select>
          </label>
          {#if healthKind === "http"}
            <label class="block w-32">
              <span class={ui.label}>Path</span>
              <input bind:value={healthPath} class={ui.inputMono} placeholder="/" />
            </label>
          {/if}
          <label class="block w-24">
            <span class={ui.label}>Every</span>
            <input bind:value={healthInterval} class={ui.inputMono} placeholder="10s" />
          </label>
          <label class="block w-28">
            <span class={ui.label}>Settle for</span>
            <input bind:value={healthGrace} class={ui.inputMono} placeholder="10s" />
          </label>
        {/if}
      </div>
      <div class="mt-2 text-xs text-ink-5">
        {#if healthKind === "none"}
          An instance counts as alive while its process is up. True, and often not what you
          meant — a web server that is up but returning 500 passes this.
        {:else if namedPorts.length === 0}
          Add a port above first: a check needs something to probe.
        {:else}
          By <em>name</em>, not number — so it follows whatever host port the mesh allocates.
          Failures during <strong class="text-ink-4">settle for</strong> do not count, so a
          slow starter is not killed at second one.
        {/if}
      </div>
    </div>

    <!-- resources + env -->
    <div class="card px-5 py-4">
      <div class="microlabel mb-3">What does each copy need?</div>
      <div class="flex flex-wrap items-end gap-3">
        <label class="block w-28">
          <span class={ui.label}>CPUs</span>
          <input bind:value={cpus} class={ui.inputMono} inputmode="numeric" placeholder="any" />
        </label>
        <label class="block w-32">
          <span class={ui.label}>Memory (MB)</span>
          <input bind:value={memoryMb} class={ui.inputMono} inputmode="numeric" placeholder="any" />
        </label>
        <button
          class="mb-1 cursor-pointer text-xs font-semibold"
          style="color: var(--accent-ink);"
          onclick={() => (env = [...env, { k: "", v: "" }])}>+ Environment variable</button
        >
      </div>
      <div class="mt-2 text-xs text-ink-5">
        A machine without the memory is not a candidate, and where this node can enforce
        it, it is a ceiling rather than a hint. Leave empty to say nothing.
      </div>
      {#if env.length}
        <div class="mt-3 flex flex-col gap-2">
          {#each env as row, i (i)}
            <div class="flex items-center gap-2">
              <input bind:value={row.k} class={`${ui.inputMono} w-44`} placeholder="LOG_LEVEL" />
              <span class="text-ink-6">=</span>
              <input bind:value={row.v} class={`${ui.inputMono} flex-1`} placeholder="info" />
              <button
                class="cursor-pointer px-1 text-ink-5 hover:text-ink-2"
                onclick={() => (env = env.filter((_, j) => j !== i))}
                ><Icon name="close" size={15} /></button
              >
            </div>
          {/each}
        </div>
      {/if}
    </div>

    <!-- update policy -->
    <div class="card overflow-hidden">
      <button
        class="flex w-full cursor-pointer items-center gap-2.5 px-5 py-3.5 text-left"
        onclick={() => (advanced = !advanced)}
      >
        <span class="microlabel">How should changes roll out?</span>
        <span class="text-xs text-ink-5">
          {advanced ? "" : "Canary first, revert on failure — sensible defaults"}
        </span>
        <span class="ml-auto text-ink-5 transition-transform" style={advanced ? "transform: rotate(90deg);" : ""}
          ><Icon name="chevron-right" size={16} /></span
        >
      </button>
      {#if advanced}
        <div class="fade-in border-t border-edge px-5 py-4">
          <div class="flex flex-wrap items-end gap-3">
            <label class="block w-32">
              <span class={ui.label}>Prove first</span>
              <input bind:value={canary} class={ui.inputMono} inputmode="numeric" placeholder="1" />
            </label>
            <label class="block w-36">
              <span class={ui.label}>May be down</span>
              <input bind:value={maxUnavailable} class={ui.inputMono} inputmode="numeric" placeholder="1" />
            </label>
            <label class="flex items-center gap-2 pb-3 text-sm text-ink-3">
              <input type="checkbox" bind:checked={autoRevert} />
              Undo it if the new one fails
            </label>
            <label class="block w-32">
              <span class={ui.label}>Give up after</span>
              <input bind:value={progressDeadline} class={ui.inputMono} placeholder="10m" />
            </label>
            <label class="block w-32">
              <span class={ui.label}>Healthy for</span>
              <input bind:value={minHealthy} class={ui.inputMono} placeholder="30s" />
            </label>
          </div>
          <div class="mt-2 text-xs text-ink-5">
            <strong class="text-ink-4">Prove first</strong> starts that many new copies
            beside the old ones and watches them. Nothing is replaced until they pass — so
            a bad version disturbs nothing.
          </div>
        </div>
      {/if}
    </div>
  </div>

  <!-- ══════════════ the file it writes ══════════════ -->
  <div class="sticky top-0 self-start">
    <div class="card overflow-hidden">
      <div class="flex items-center gap-2 border-b border-edge px-4 py-2.5" style="background: var(--surface-2)">
        <span class="text-xs font-semibold text-ink-2">The file this writes</span>
        <div class="flex-1"></div>
        <button
          class="cursor-pointer text-xs font-semibold"
          style="color: var(--accent-ink);"
          onclick={() => ontext(withEnv)}>Edit as text →</button
        >
      </div>
      <pre
        class="dark-scroll m-0 max-h-[62vh] overflow-auto px-4 py-3 font-mono text-xs leading-relaxed"
        style="background: var(--surface-code); color: var(--ink-code-2);">{withEnv}</pre>
      <div class="border-t border-edge px-4 py-2.5 text-xs text-ink-5">
        This is exactly what <code class="num">meshnode deployment put</code> takes. Save it
        next to your code and the two front ends agree.
      </div>
    </div>
  </div>
</div>
