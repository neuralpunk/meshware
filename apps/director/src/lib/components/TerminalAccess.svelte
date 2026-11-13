<script lang="ts">
  // "Can this person open a terminal?" — the control, where the question is.
  //
  // Underneath, terminal access is a *session policy*: a record matching a
  // username glob against a node selector, and one of three independent
  // conditions (the node must accept terminals in its own config, and the
  // person must already be able to run work there). All three are real and
  // all three matter.
  //
  // What did not work was making that the user interface. "Terminals" was a
  // top-level tab whose empty state said "nobody can open a terminal
  // anywhere", next to a button called **New terminal policy** — so the
  // first thing a new admin met on the Access screen was a noun they had
  // never heard, attached to a form asking for a policy name, a user glob
  // and a node selector, in service of a question they had not asked yet.
  //
  // The question people actually have is about a *person*. So the switch
  // lives on that person's row and owns exactly one policy, named for them.
  // Hand-written glob policies (`ops-*`, `*`) are untouched and still
  // listed here as "also covered by", because a switch that says "off"
  // while somebody still has a prompt is a lie, and this is the one screen
  // where a lie is expensive.
  import { api, errText, type SessionPolicyView } from "../api";
  import { app } from "../state.svelte";
  import { describeNodes, personalPolicyId, terminalPoliciesFor } from "../access";
  import Hint from "./Hint.svelte";

  let {
    username,
    /** Every live session policy, so inherited coverage can be shown. */
    policies,
    /** Can this person run work at all? The other gate, and the harder one. */
    canRunWork,
    /** Is their runner grant unrestricted? A terminal belongs to no script. */
    unrestricted,
    disabled = false,
    onchanged,
  }: {
    username: string;
    policies: SessionPolicyView[];
    canRunWork: boolean;
    unrestricted: boolean;
    disabled?: boolean;
    onchanged: () => Promise<void> | void;
  } = $props();

  const mine = $derived(terminalPoliciesFor(policies, username));
  const own = $derived(mine.find((p) => p.id === personalPolicyId(username)) ?? null);
  /** Coverage from a glob somebody wrote by hand — not ours to switch off. */
  const inherited = $derived(mine.filter((p) => p.id !== personalPolicyId(username)));

  const meshNodes = $derived(app.nodes.filter((n) => !n.revoked && !n.left));
  const groups = $derived([...new Set(meshNodes.flatMap((n) => n.tags))].sort());

  /** The select's value, derived from the policy that exists (or does not). */
  const current = $derived.by(() => {
    if (!own) return "off";
    const n = (own.nodes || "any").trim();
    if (n === "any") return "any";
    if (n.startsWith("tag=")) return `group:${n.slice(4)}`;
    if (n.startsWith("name=")) return `node:${n.slice(5)}`;
    return "custom";
  });

  let busy = $state(false);
  let err = $state<string | null>(null);

  /**
   * A username the policy's glob matcher would read as a pattern.
   *
   * The engine does not constrain usernames, and a policy's `users` field is
   * glob-matched (`rbac::glob_match`) — so writing `users: "ops*"` for a
   * person literally called `ops*` would quietly hand a terminal to `ops-1`,
   * `ops-2` and everyone else who matched. Vanishingly rare and silently
   * over-granting, which is the combination worth refusing outright rather
   * than escaping: this control writes rules about *one* person, and if it
   * cannot express that, it should say so instead of guessing.
   */
  const ambiguous = $derived(username.includes("*"));

  async function change(choice: string) {
    if (choice === current || ambiguous) return;
    busy = true;
    err = null;
    try {
      if (choice === "off") {
        if (own) await api.sessionPolicyRemove(own.id);
        app.flash(`${username} can no longer open a terminal`);
      } else {
        const nodes =
          choice === "any"
            ? "any"
            : choice.startsWith("group:")
              ? `tag=${choice.slice(6)}`
              : `name=${choice.slice(5)}`;
        await api.sessionPolicyPut({
          id: personalPolicyId(username),
          description: `terminals for ${username}`,
          users: username,
          nodes,
          // Root is deliberately not offered from here. It only means
          // anything on a node whose daemon runs as root *and* which opted
          // in with `[sessions] allow_root`, and a checkbox on a person's
          // row is the wrong place to make that decision quietly.
          allow: ["shell"],
          expiresSecs: null,
        });
        app.flash(`${username} can open a terminal on ${describeNodes(nodes)}`);
      }
      await onchanged();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="flex flex-wrap items-center gap-x-3 gap-y-2">
  <span class="text-sm text-ink-3">Can open a terminal on</span>
  <select
    value={current}
    disabled={disabled || busy || ambiguous}
    class="select h-8 min-w-[180px] rounded-md bg-canvas px-2.5 text-sm text-ink outline-none disabled:opacity-50"
    style="box-shadow: var(--sh-control);"
    onchange={(e) => change((e.currentTarget as HTMLSelectElement).value)}
  >
    <option value="off">nothing</option>
    <option value="any">the whole mesh</option>
    {#if groups.length > 0}
      <optgroup label="A group of machines">
        {#each groups as t (t)}<option value={`group:${t}`}>machines tagged {t}</option>{/each}
      </optgroup>
    {/if}
    {#if meshNodes.length > 0}
      <optgroup label="One machine">
        {#each meshNodes as n (n.nodeId)}<option value={`node:${n.name}`}>{n.name}</option>{/each}
      </optgroup>
    {/if}
    {#if current === "custom"}
      <option value="custom">{own?.nodes}</option>
    {/if}
  </select>
  <Hint
    label="About terminals"
    text="A terminal is an interactive shell on a machine, through the mesh — no SSH and no inbound port, with the open and the close both recorded. Three separate things have to agree before one opens: that machine has to accept terminals in its own config (its owner's call, and no admin here can override it), the person has to be able to run work there already, and this switch. That is why it can say yes and still not work — and why it can never widen anybody's access on its own."
  />
</div>

{#if ambiguous}
  <div class="mt-1.5 text-xs" style="color: var(--status-warn-ink);">
    This switch cannot be used for a name containing <span class="mono">*</span> — a rule for
    “{username}” would also cover everyone else whose name matched it. Write the rule by hand
    under <em>Terminal rules written for groups</em>, or rename them.
  </div>
{/if}

<!-- The two gates this switch does not control, named only when they are
     the reason it will not work. Silence here is what produced "I turned it
     on and nothing happened". -->
{#if current !== "off" && !ambiguous}
  {#if !canRunWork}
    <div class="mt-1.5 text-xs" style="color: var(--status-warn-ink);">
      This will not work yet: {username} cannot run work anywhere. Give them the operator,
      developer or admin role first.
    </div>
  {:else if !unrestricted}
    <div class="mt-1.5 text-xs" style="color: var(--status-warn-ink);">
      This will not work yet: their role is limited to specific scripts, and a terminal belongs
      to no script. Widen it under <em>Limit them to certain scripts</em> on their grant.
    </div>
  {:else}
    <div class="mt-1.5 text-xs text-ink-5">
      …on whichever of those machines have terminals switched on in their own config.
    </div>
  {/if}
{/if}

{#if inherited.length > 0}
  <div class="mt-1.5 text-xs text-ink-5">
    Also covered by
    {#each inherited as p, i (p.id)}<span class="mono text-ink-4">{p.id}</span>{#if i < inherited.length - 1}, {/if}{/each}
    — a rule written for a group, so switching this off will not remove it.
  </div>
{/if}

{#if err}<div class="mt-1.5 text-xs" style="color: var(--status-error-ink);">{err}</div>{/if}
