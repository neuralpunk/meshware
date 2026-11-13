<script lang="ts">
  // What the five roles actually mean, as one table.
  //
  // This exists because the honest answer to "what can an operator do?" was
  // previously a one-line hint inside a form somebody only sees while
  // *creating* a user — which is the moment they most need to have already
  // known. Roles are a small fixed set and they never change at runtime, so
  // the reference can simply be present.
  //
  // **Every row is a real permission set from `crates/mesh-core/src/rbac.rs`,
  // not a paraphrase.** The constant is named in `gate` so the two can be
  // diffed by eye when somebody changes one; if a row here and a row there
  // ever disagree, this file is the one that is wrong.
  import Icon from "./Icon.svelte";
  import { ROLES } from "../roles";

  let {
    /** Dim every column but this one. */
    highlight = null,
    /** Rows the reader can act on rather than only read. */
    compact = false,
  }: { highlight?: string | null; compact?: boolean } = $props();

  type Row = {
    what: string;
    detail: string;
    /** The `rbac.rs` constant this row is. */
    gate: string;
    roles: string[];
    /** True when holding the role is necessary but not sufficient. */
    caveat?: string;
  };

  const ROWS: Row[] = [
    {
      what: "See the mesh",
      detail: "Machines, jobs, services, deployments and their history.",
      gate: "READ",
      roles: ["admin", "operator", "developer", "auditor", "viewer"],
    },
    {
      what: "Run and cancel work",
      detail: "Ad-hoc commands, scripts, services, fan-outs and parallel jobs.",
      gate: "RUNNERS",
      roles: ["admin", "operator", "developer"],
      caveat: "only on nodes and services this person's grant covers",
    },
    {
      what: "Open a terminal",
      detail: "An interactive shell on a node, through the mesh.",
      gate: "RUNNERS + session policy",
      roles: ["admin", "operator", "developer"],
      caveat: "also needs the node to accept terminals and a session policy naming them",
    },
    {
      what: "Drain a node",
      detail: "Stop taking new work and finish what is already accepted.",
      gate: "RUNNERS",
      roles: ["admin", "operator", "developer"],
    },
    {
      what: "Manage content",
      detail: "Push bundles, publish services and contracts, register Git sources.",
      gate: "CONTENT",
      roles: ["admin", "developer"],
    },
    {
      what: "Manage schedules",
      detail: "The timetables that run services on their own, and fact collectors.",
      gate: "SCHEDULES / COLLECTORS",
      roles: ["admin", "operator", "developer"],
    },
    {
      what: "See that a secret exists",
      detail: "Names and metadata only. No role can read a value through Director.",
      gate: "SECRET_READ",
      roles: ["admin", "developer", "auditor"],
    },
    {
      what: "Read the audit log",
      detail: "Every governed action, by whom, on what, and how it ended.",
      gate: "AUDIT_READ",
      roles: ["admin", "auditor"],
    },
    {
      what: "Decide who has access",
      detail: "Add users, write grants and session policies, create invites, revoke a machine.",
      gate: "ADMIN_ONLY",
      roles: ["admin"],
    },
  ];

  const rows = $derived(compact ? ROWS.filter((r) => r.gate !== "SCHEDULES / COLLECTORS") : ROWS);
  const dim = (role: string) => highlight !== null && highlight !== role;
</script>

<div class="card overflow-hidden">
  <div class="overflow-x-auto">
    <table class="w-full text-left text-sm">
      <thead>
        <tr class="border-b border-edge">
          <th class="label px-5 py-3">Can they…</th>
          {#each ROLES as role (role)}
            <th
              class="label px-3 py-3 text-center whitespace-nowrap transition-opacity"
              class:opacity-30={dim(role)}
            >
              {role}
            </th>
          {/each}
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.what)}
          <tr class="border-b border-edge last:border-0 hover:bg-surface-2">
            <td class="px-5 py-3">
              <div class="font-semibold text-ink">{row.what}</div>
              <div class="mt-0.5 text-xs leading-relaxed text-ink-5">{row.detail}</div>
              {#if row.caveat}
                <div class="mt-1 flex items-start gap-1.5 text-xs" style="color: var(--status-warn-ink);">
                  <span class="mt-[3px]"><Icon name="alert" size={10} /></span>
                  <span>{row.caveat}</span>
                </div>
              {/if}
            </td>
            {#each ROLES as role (role)}
              {@const yes = row.roles.includes(role)}
              <td class="px-3 py-3 text-center transition-opacity" class:opacity-30={dim(role)}>
                {#if yes}
                  <span
                    class="inline-flex h-5 w-5 items-center justify-center rounded-full"
                    style="background: var(--accent-wash); color: var(--accent-ink);"
                    title="{role} can"
                  >
                    <Icon name="check" size={11} />
                  </span>
                {:else}
                  <span class="text-ink-6" title="{role} cannot">·</span>
                {/if}
              </td>
            {/each}
          </tr>
        {/each}
      </tbody>
    </table>
  </div>

  <!-- The two things that surprise people, said once, where they will be
       read. Both are real trapdoors: a grant can look correct and grant
       nothing at all. -->
  <div class="grid gap-px border-t border-edge sm:grid-cols-2" style="background: var(--edge);">
    <div class="bg-surface px-5 py-3.5">
      <div class="flex items-center gap-2">
        <span style="color: var(--status-warn-ink);"><Icon name="alert" size={12} /></span>
        <span class="text-sm font-semibold text-ink">A role alone does nothing</span>
      </div>
      <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
        Every role arrives attached to a <strong>scope</strong>: which nodes, and which
        services. An operator scoped to <span class="mono">tag=dev</span> is an operator on
        the dev nodes and a stranger everywhere else. Grants are additive — someone with two
        grants has the union of both.
      </p>
    </div>
    <div class="bg-surface px-5 py-3.5">
      <div class="flex items-center gap-2">
        <span style="color: var(--status-warn-ink);"><Icon name="alert" size={12} /></span>
        <span class="text-sm font-semibold text-ink">
          Narrowing services blocks ad-hoc commands
        </span>
      </div>
      <p class="mt-1.5 mb-0 text-xs leading-relaxed text-ink-5">
        A typed command and a terminal belong to no service, so they are only allowed by an
        unrestricted <span class="mono">*</span> scope. A grant limited to
        <span class="mono">deploy-*</span> lets that person run <em>those services</em> and
        nothing typed by hand — which is either exactly what you meant, or a surprise at 3am.
      </p>
    </div>
  </div>
</div>
