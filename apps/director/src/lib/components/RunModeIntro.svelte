<script lang="ts">
  // What a Run mode is — behind one small pill, in a modal.
  //
  // ## Why this exists at all
  //
  // Run has four modes behind its tab row, and they are not four settings —
  // they are four different mental models. "Once" runs a whole program in
  // one place. "Per item" runs the same program many times in many places. "Together"
  // holds machines until it can start them simultaneously. "Build farm" is
  // not something you press Run on at all; it is a door other tools walk
  // through. (Library, the fifth tab, is a place rather than a mode and has
  // no notes.) Somebody meeting these for the first time cannot tell them
  // apart from their names, and the cost of guessing wrong is not a bad
  // setting — it is a wrong mental model that takes a week to shake.
  //
  // So each mode says three things, always in the same three places:
  //
  //   Good for       — reach for it when your work looks like this
  //   Won't do       — the thing people expect and do not get
  //   Get the most   — the one or two habits that make it work well
  //
  // The **Won't do** column is the load-bearing one, and it is why this is
  // a component rather than a paragraph per tab. Every distributed-compute
  // product in this market oversells, and the specific way they oversell is
  // by never printing the limit — so the user discovers it themselves, six
  // hours in, and concludes the product is broken rather than that they
  // aimed it wrong. M9 §1 already made this a project rule for the docs
  // ("meshware must not claim otherwise, in docs or in marketing"). This is
  // that rule, in the interface, where the decision is actually made.
  //
  // ## Why it is a modal and not the panel it used to be
  //
  // This was an expandable card sitting above the form, open by default and
  // collapsible per mode. The argument for that was teaching: a newcomer
  // should meet the limits without going looking. The argument against it
  // is the one Hint.svelte already makes and this component talked itself
  // out of — a paragraph that is always visible is paid for on *every*
  // glance, and an operator glances at Run two hundred times a day. Three
  // columns of prose above the form is the single largest block of screen
  // on the page, and it is the block that is read once.
  //
  // So the words are not deleted, they are moved one deliberate click away:
  // Tesler's law relocates complexity rather than removing it, and WCAG 2.2
  // consistent-help asks that the affordance look and sit the same
  // everywhere. The pill lives in the tab row, in one fixed place, for
  // every mode — the exact place the "which tab do I want" question is
  // asked. Nothing is remembered and nothing auto-opens: a modal that
  // appears unbidden would be the clutter this replaced, wearing a hat.

  import Icon from "./Icon.svelte";
  import Modal from "./Modal.svelte";
  import { ui } from "../state.svelte";

  export type IntroPoint = {
    /** The claim, in one short line. Sentence case, no trailing period. */
    text: string;
    /** Optional second line that qualifies the first. */
    detail?: string;
  };

  let {
    /** Stable key for this mode, e.g. "per-item". Ties the pill to its dialog. */
    id,
    /** The mode's name, as the tab spells it. */
    title,
    /** One sentence, plain language, no jargon. What this mode *is*. */
    lead,
    good = [],
    wont = [],
    most = [],
    /** An optional shell line that makes the mode concrete. */
    example = null,
    /** What the example line is, e.g. "the same thing from a terminal". */
    exampleNote = null,
  }: {
    id: string;
    title: string;
    lead: string;
    good?: IntroPoint[];
    wont?: IntroPoint[];
    most?: IntroPoint[];
    example?: string | null;
    exampleNote?: string | null;
  } = $props();

  let open = $state(false);

  // One instance is reused across tabs. Closing on the way out means
  // switching modes never leaves the previous mode's notes on screen.
  $effect(() => {
    void id;
    open = false;
  });

  const columns = $derived(
    [
      { kind: "good" as const, label: "Good for", icon: "check", points: good },
      { kind: "wont" as const, label: "What it won't do", icon: "close", points: wont },
      { kind: "most" as const, label: "Getting the most out of it", icon: "chevron-right", points: most },
    ].filter((c) => c.points.length > 0),
  );
</script>

<button
  type="button"
  class="notes-pill"
  aria-haspopup="dialog"
  aria-expanded={open}
  title={`What ${title} is, what it won't do, and how to get the most out of it`}
  onclick={() => (open = true)}
>
  <Icon name="help" size={12} />
  <span class="notes-pill-label">What is this?</span>
</button>

<Modal {open} kicker="what is this" width={900} onclose={() => (open = false)}>
  <div class="notes">
    <h3 class="notes-title">{title}</h3>
    <p class="notes-lead">{lead}</p>

    <div class="notes-cols">
      {#each columns as col (col.kind)}
        <div class={`notes-col notes-col-${col.kind}`}>
          <div class="notes-col-head">
            <span class={`notes-badge notes-badge-${col.kind}`}>
              <Icon name={col.icon} size={11} />
            </span>
            <span class="notes-col-label">{col.label}</span>
          </div>
          <ul class="notes-list">
            {#each col.points as p (p.text)}
              <li>
                <span class="notes-point">{p.text}</span>
                {#if p.detail}<span class="notes-detail">{p.detail}</span>{/if}
              </li>
            {/each}
          </ul>
        </div>
      {/each}
    </div>

    {#if example}
      <div class="notes-example">
        <span class="notes-example-label">
          {exampleNote ?? "the same thing from a terminal"}
        </span>
        <code>{example}</code>
      </div>
    {/if}
  </div>

  <div class="notes-actions">
    <button class={ui.primary} onclick={() => (open = false)}>Done</button>
  </div>
</Modal>

<style>
  /* ---- the pill, in the tab row ------------------------------------- */

  /* Sized to sit inside a tab row without setting its height: the tabs are
     ~40px and this is 24px, so the row measures the tabs and the underline
     stays where it was. */
  .notes-pill {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    flex: none;
    height: 24px;
    padding: 0 9px;
    border: 1px solid var(--edge-strong);
    border-radius: 999px;
    background: var(--surface-1);
    font-size: var(--text-xs);
    font-weight: 500;
    color: var(--ink-5);
    cursor: pointer;
    transition:
      color var(--dur-fast) var(--ease),
      border-color var(--dur-fast) var(--ease),
      background-color var(--dur-fast) var(--ease);
  }
  .notes-pill:hover,
  .notes-pill[aria-expanded="true"] {
    color: var(--accent-ink);
    border-color: var(--edge-hover);
    background: var(--accent-wash-2);
  }

  /* Below the tab row's own breathing room the label is the first thing to
     go — the "?" alone still reads as help, and the tabs matter more. */
  @media (max-width: 720px) {
    .notes-pill-label {
      display: none;
    }
    .notes-pill {
      padding: 0 6px;
    }
  }

  /* ---- the note itself ---------------------------------------------- */

  /* Bounded so a long mode scrolls inside the dialog rather than growing
     it past the viewport; `Modal` clips its own corners, so the scroll has
     to live in here. */
  .notes {
    max-height: min(62vh, 620px);
    overflow-y: auto;
    padding: 14px 26px 4px;
  }

  .notes-title {
    margin: 0 0 4px;
    font-size: var(--text-lg);
    font-weight: 700;
    letter-spacing: -0.02em;
    color: var(--ink-1);
  }

  .notes-lead {
    margin: 0;
    max-width: 78ch;
    font-size: var(--text-sm);
    line-height: 1.6;
    color: var(--ink-2);
  }

  /* One column by default, three when the dialog is actually wide enough
     for three — never two. At 900px a wrapped third column would sit in
     row 2 under the first, where its heading reads as a continuation of
     "Good for" rather than a column of its own. */
  .notes-cols {
    display: grid;
    grid-template-columns: 1fr;
    gap: 20px;
    margin-top: 18px;
    border-top: 1px solid var(--edge);
    padding-top: 16px;
  }
  @media (min-width: 940px) {
    .notes-cols {
      grid-template-columns: repeat(3, minmax(0, 1fr));
      gap: 0 26px;
    }
  }

  .notes-col {
    min-width: 0;
  }

  .notes-col-head {
    display: flex;
    align-items: center;
    gap: 7px;
    margin-bottom: 9px;
  }

  .notes-badge {
    display: inline-grid;
    place-items: center;
    width: 16px;
    height: 16px;
    flex: none;
    border-radius: 999px;
  }
  /* Semantic, and deliberately not the accent: "won't do" is the column
     people must actually read, and washing it in the brand colour is how
     it stops being read. */
  .notes-badge-good {
    background: var(--accent-wash);
    color: var(--accent-ink);
  }
  .notes-badge-wont {
    background: var(--status-warn-wash);
    color: var(--status-warn-ink);
  }
  .notes-badge-most {
    background: var(--surface-2);
    color: var(--ink-3);
  }

  .notes-col-label {
    font-size: var(--text-2xs);
    font-weight: 700;
    letter-spacing: 0.07em;
    text-transform: uppercase;
    /* `--ink-muted` is a light-ground token that `.on-code` does not
       re-point, so it would stay #5b6671 on near-black in light theme.
       Everything in this dialog has to come from the re-pointed ramp. */
    color: var(--ink-3);
  }

  .notes-list {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: 9px;
  }
  .notes-list li {
    font-size: var(--text-sm);
    line-height: 1.5;
  }

  .notes-point {
    display: block;
    color: var(--ink-2);
  }
  .notes-detail {
    display: block;
    margin-top: 2px;
    font-size: var(--text-xs);
    line-height: 1.5;
    color: var(--ink-5);
  }

  .notes-example {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 10px;
    margin-top: 18px;
    padding-top: 14px;
    border-top: 1px solid var(--edge);
  }
  .notes-example-label {
    font-size: var(--text-2xs);
    font-weight: 700;
    letter-spacing: 0.07em;
    text-transform: uppercase;
    color: var(--ink-3);
    flex: none;
  }
  .notes-example code {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--ink-2);
    /* Not `--surface-inline`/`--edge-pre`: those are the light-ground
       inline-code pair, and `.on-code` leaves them alone — a near-white
       chip under light-on-dark text. `--surface-2` travels with the ramp. */
    background: var(--surface-2);
    border: 1px solid var(--edge);
    border-radius: var(--radius-xs);
    padding: 2px 7px;
    overflow-x: auto;
    max-width: 100%;
    white-space: pre;
  }

  .notes-actions {
    display: flex;
    justify-content: flex-end;
    padding: 16px 26px 20px;
  }
</style>
