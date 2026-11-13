<script lang="ts">
  // Get started (UX-10): mesh created → a second machine → a first job.
  //
  // The wizard ends at "Create mesh" and drops a new user on the densest
  // screen in the app, with nothing on it that says what to do next. Every
  // piece of the first run already exists — the add-machine drawer, the
  // script wizard, Launch — and nothing connected them. This card does, in
  // the order a first run happens, and each step ticks itself off from the
  // mesh's own state rather than from a click, so doing it any other way
  // (the CLI, another Director) counts too.
  //
  // It is for a new mesh. One that already has machines, scripts and a
  // fan-out behind it never sees the card, and once it has been completed
  // or dismissed it stays gone — per mesh, in this browser's storage.
  import { app } from "../state.svelte";
  import Icon from "./Icon.svelte";

  let { onAddMachine }: { onAddMachine: () => void } = $props();

  const key = $derived(`mw:getstarted:${app.status?.meshId ?? ""}`);

  function read(k: string): string | null {
    try {
      return localStorage.getItem(k);
    } catch {
      return null;
    }
  }
  function write(k: string, v: string) {
    try {
      localStorage.setItem(k, v);
    } catch {
      /* private mode: the card simply comes back next launch */
    }
  }

  let stored = $state<string | null>(null);
  $effect(() => {
    stored = read(key);
  });

  const machines = $derived(app.nodes.filter((n) => !n.revoked && !n.left).length);
  // The script the last step runs: one that asks for nothing, if there is
  // one, so "Run it" is one press and not a form.
  const firstScript = $derived(
    (app.services.find((s) => !s.hasContract) ?? app.services[0])?.name ?? null,
  );

  const steps = $derived([
    {
      title: "Add a second machine",
      sub:
        machines >= 2
          ? `${machines} machines in this mesh.`
          : "Pair a computer on this network, or send an invite to one that is not.",
      done: machines >= 2,
      action: "Add a machine",
      disabled: false,
      run: onAddMachine,
    },
    {
      title: "Add a script",
      sub: firstScript
        ? `${firstScript} can be run by name from any machine.`
        : "Pick a file from a folder or a Git repo and give it a name.",
      done: firstScript !== null,
      action: "Add a script",
      disabled: false,
      run: () => {
        sessionStorage.setItem("library:open", "add-script");
        app.go("library");
      },
    },
    {
      title: "Run it on every machine",
      sub: "One run, one row of output per machine, all in Activity.",
      // A fan-out is what "every machine" is; any earlier single run does
      // not count, because it did not show what the mesh is for.
      done: app.jobs.some((j) => j.fanGroup !== null),
      action: "Run it",
      disabled: firstScript === null,
      run: () => {
        if (firstScript) sessionStorage.setItem("run:preselect", firstScript);
        sessionStorage.setItem("run:spread", "all");
        app.go("run");
      },
    },
  ]);
  const doneCount = $derived(steps.filter((s) => s.done).length);
  const allDone = $derived(doneCount === steps.length);

  // Seen incomplete in this session: only then is the card this mesh's to
  // show. A mesh that arrives already complete is not a new one.
  let sawIncomplete = $state(false);
  $effect(() => {
    if (!app.ready || !app.status?.meshId || stored !== null) return;
    if (!allDone) sawIncomplete = true;
    else if (!sawIncomplete) {
      write(key, "done");
      stored = "done";
    }
  });
  const shown = $derived(stored === null && sawIncomplete);

  function close() {
    write(key, allDone ? "done" : "dismissed");
    stored = allDone ? "done" : "dismissed";
  }
</script>

{#if shown}
  <section class="card getstarted mb-5" aria-labelledby="getstarted-title">
    <div class="flex items-start gap-3">
      <div class="min-w-0 flex-1">
        <h2 id="getstarted-title" class="m-0 text-md font-bold tracking-[-.015em] text-ink">
          {allDone ? "You're set up" : "Get started"}
        </h2>
        <p class="mt-1 mb-0 text-sm text-ink-4">
          {allDone
            ? "Two machines, a script, and a run across both. Everything else builds on those three."
            : `Three steps from a new mesh to a first run · ${doneCount} of ${steps.length} done`}
        </p>
      </div>
      <button class="iconbtn h-7 w-7 flex-none rounded-md" title={allDone ? "Close" : "Hide this — it will not come back"} aria-label="Close Get started" onclick={close}>
        <Icon name="close" size={14} />
      </button>
    </div>

    <ol class="mt-4 grid gap-3 lg:grid-cols-3">
      {#each steps as step, i (step.title)}
        <li class="gs-step" data-done={step.done ? "" : undefined}>
          <span class="gs-n" aria-hidden="true">
            {#if step.done}<Icon name="check" size={12} />{:else}{i + 1}{/if}
          </span>
          <div class="min-w-0 flex-1">
            <div class="text-sm font-semibold text-ink">
              {step.title}
              {#if step.done}<span class="sr-only">(done)</span>{/if}
            </div>
            <p class="mt-0.5 mb-0 text-xs leading-[1.5] text-ink-5">{step.sub}</p>
            {#if !step.done}
              <button
                class="btn btn-secondary btn-md mt-2.5"
                disabled={step.disabled}
                title={step.disabled ? "Add a script first" : undefined}
                onclick={step.run}
              >
                {step.action}
              </button>
            {/if}
          </div>
        </li>
      {/each}
    </ol>
  </section>
{/if}

<style>
  .getstarted {
    padding: 18px 22px 20px;
    border-top: 3px solid var(--accent);
  }
  .gs-step {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 12px 14px;
    border-radius: var(--radius-md);
    background: var(--surface-subtle);
    list-style: none;
  }
  .gs-step[data-done] {
    opacity: 0.75;
  }
  .gs-n {
    display: grid;
    flex: none;
    place-items: center;
    width: 22px;
    height: 22px;
    border-radius: 999px;
    background: var(--surface-2);
    color: var(--ink-4);
    font-size: var(--text-2xs);
    font-weight: 700;
  }
  .gs-step[data-done] .gs-n {
    background: var(--accent);
    color: #fff;
  }
</style>
