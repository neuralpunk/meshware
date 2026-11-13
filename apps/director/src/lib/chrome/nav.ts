// The navigation surface, as data.
//
// This lived in `App.svelte` until the 6c pass, which is where it belonged
// while the rail drew a flat list and each screen drew its own tab row.
// Nesting the sub-sections under their section ended that: the rail now
// needs to know that Mesh has eight panels and what they are called, and
// Mesh needs to know which one the rail has selected. Two components
// reading one table is a module, not a prop.
//
// `sub` is the header's one-line answer to "what is this screen for" —
// plain words, no jargon; the long answers live behind the same "?" everywhere.

import type { ViewName } from "../state.svelte";

export type NavItem = {
  id: ViewName;
  label: string;
  group: string;
  sub: string;
};

/** A panel of a section: a row indented under it in the rail. `sub` is the
 *  one-line question the panel answers, for screens that print it under
 *  the panel's name (Settings does). */
export type SubItem = { id: string; label: string; sub?: string };

// MASTERPLAN §14.2 surfaces: Mesh · Nodes · Run · Jobs · Workflows ·
// Library · Secrets · Access (+Audit) · Settings. Distributed compute is
// not a section — it is the "once per item" answer on Run (M9 §6,
// amended) — and Compute is a panel of Mesh, where "how much machine is
// this" is asked.
//
// The Focus surface (2026-08): sections answering one of three questions
// — *what is my mesh* (Mesh), *run something on it* (Scripts,
// Run, Activity), *who may* (Access, Secrets) — plus this peer's own
// Settings.
//
// **R6 (2026-09-08).** Three surfaces named for what a person does, not
// for how the scheduler is built: Scripts (what I can run), Run (do it),
// Activity (what happened, and where my results are).
//
// **UX-14 (2026-09-17).** The rail's group headings were the same words as
// the rows under them — "Mesh" over Mesh, "Run" over Run — so they are
// named for what the group holds: Machines, Work, System.
export const CORE_NAV: NavItem[] = [
  { id: "mesh", label: "Mesh", group: "Machines",
    sub: "The machines, live: who is in, how healthy, and what they add up to." },
  { id: "library", label: "Scripts", group: "Work",
    sub: "What this mesh can run by name. Drop a file in to add one." },
  // "Launch" since the rail nested it under a section called Run
  // (2026-09): `Run › Run` in the breadcrumb was the rail saying one word
  // twice about two different things. "New run" since UX-14: Launch was a
  // verb that needed a footnote on Scripts to say its own Run button did
  // not launch anything. This is the page where a run is set up.
  { id: "run", label: "New run", group: "Work",
    sub: "Run one thing on any of it: what, and where." },
  { id: "jobs", label: "Activity", group: "Work",
    sub: "What ran, what it answered, and what is waiting to be collected." },
  // N1, 2026-09-12 (D-N1). `deployment` came out from behind the old
  // legacy switch, and landmine 5 of the N-track was that un-gating a verb
  // means *this list*, not just the CLI — a capability that reaches one
  // front end is a capability half the product does not have (D-E3).
  { id: "deployments", label: "Deployments", group: "Work",
    sub: "Work that is supposed to stay up, kept up — and what happened the last time it rolled." },
  { id: "schedules", label: "Schedules", group: "Work",
    sub: "Work that runs itself, on a clock, with nobody watching." },
  // N3. In the Run group rather than Mesh: a registry is about the work,
  // not the machines — the question it answers is "where is my service",
  // and the answer happens to mention nodes.
  // "Load balancing" since the mesh proxy went (2026-09-14). A copy answers
  // on the machine it runs on, at the port that machine gave it, and the
  // step after reading this screen is pasting those addresses into a load
  // balancer — so the row is named for that step, not for the catalogue
  // behind it. The view id stays `registry`: deep links name the data,
  // and the data did not change.
  // "Addresses" since UX-14: "Load balancing" read as a load balancer
  // Meshware ships, on a page that says there is no proxy. The page is the
  // list of where each copy answers — the addresses you give yours — and
  // it sits under Services, which says whose addresses they are.
  { id: "registry", label: "Addresses", group: "Work",
    sub: "Where each copy of a service answers, and how to put a load balancer in front of them." },
  { id: "access", label: "Access", group: "System",
    sub: "Let people in, and choose what each one can do." },
  { id: "secrets", label: "Secrets", group: "System",
    sub: "Values jobs can use without ever seeing them printed." },
  { id: "settings", label: "Settings", group: "System",
    sub: "This machine, this app, and what the mesh keeps." },
];

/**
 * The panels that hang off a section.
 *
 * Mesh and Settings have them. Mesh is several views of one subject rather
 * than one view of several; Settings joined it on 2026-09-14, when its
 * panes left the 168px index inside the page for rows in the rail — a
 * second navigation, in a second place, in a second shape, was exactly the
 * argument that moved Mesh's tab row out here, and Settings was the one
 * screen still making it. The other screens' tabs are *modes* of a single
 * thing — details/events/definition in an inspector — and a mode does not
 * belong in a navigation tree, because you cannot arrive at it.
 *
 * **Updates joined them (2026-09-11).** It was the second tab of the node
 * table, which read as a mode of that table and was not one: a rollout is
 * a place you go, on a different day, to answer a different question —
 * what is the fleet running, and put it on the newest build. Buried one
 * click inside Fleet, nobody found it. It is a row in the rail now, with
 * the count of nodes behind on it, which is the whole reason to look.
 *
 * The order here is the order in the rail and the order the panels are
 * written in `views/Mesh.svelte`; the labels are read by the rail and by
 * the breadcrumb, so the two can never disagree about what a panel is
 * called.
 */
export const SUBNAV: Partial<Record<ViewName, readonly SubItem[]>> = {
  // **Five, from twelve (2026-09-14).** Twelve rows under one section was a
  // rail that had stopped being navigation and become a table of contents,
  // and half of the twelve were the same answer on a second page:
  //
  //   - **Members** was the machines Fleet already lists — status, sync,
  //     joined, tags, load — in a second table with fewer columns and no
  //     way into a machine that Fleet does not also have.
  //   - **Trust** was that list again with "trusted N ago", which is
  //     Fleet's Joined column; its Add node button is the toolbar's.
  //   - **Events** was the newest thirty jobs, which Run › Activity shows
  //     properly, with filters and logs. The Topology inspector keeps its
  //     per-machine events, where the machine is the filter.
  //   - **Settings** was the mesh's name, id, build and listen address —
  //     Settings › This peer, one click away, with a link to it.
  //
  // Compute, Runtimes, Ports and Volumes became one panel, Capacity. They
  // were four rows for one question — *can this mesh take the work, and
  // what is holding what it lends* — and an operator debugging a job that
  // will not place reads all four, in that order, anyway.
  //
  // Old ids still land: `app.go` sends each retired panel to the one that
  // absorbed it (`state.svelte.ts`).
  mesh: [
    { id: "nodes", label: "Fleet" },
    { id: "topology", label: "Topology" },
    // Phase C, U8. Beside Topology because it is the same machines drawn
    // for a different question: not "who can reach whom" but "what runs
    // where, and how full is each box".
    { id: "placement", label: "Placement" },
    { id: "capacity", label: "Capacity" },
    { id: "updates", label: "Updates" },
  ],
  // The panes of Settings, in the order they have always been in. Seven
  // items rather than three is the point: each is one question an operator
  // turns up with, the pane is the whole answer, and a rail with three
  // entries and a mile of content under each is a table of contents
  // pretending to be navigation.
  settings: [
    { id: "this-peer", label: "This machine", sub: "Who this machine is in the mesh, and what it lends." },
    { id: "director", label: "Director", sub: "This app: how it starts, what it watches, where its logs go." },
    // "Invites" since UX-15: this pane is the invite form, and "Fleet" was
    // already the name of the machine table under Mesh. The id stays, for
    // the deep links that name it.
    { id: "fleet", label: "Invites", sub: "Bring in a machine that is not on this network." },
    { id: "facts", label: "Facts", sub: "What every node is asked to report about itself." },
    { id: "diagnostics", label: "Diagnostics", sub: "This machine's health, end to end." },
    // "Retention & export", not "History" (2026-09-20, CUT-3). Four
    // different things in this product answer to "history" — the op-log,
    // the node's index, the Parquet archive and a sink — and this pane is
    // none of them: it sets how long records are kept and where copies go.
    // A person looking for *what ran* went here and found a retention
    // window, which is the reader's time spent on our naming.
    { id: "history", label: "Retention & export", sub: "How long the mesh keeps things, and who else gets a copy." },
    { id: "storage", label: "Storage", sub: "What this machine keeps on disk, and how much room it leaves." },
    // N6 put a *mesh-wide* switch in a pane whose label promised only this
    // peer. Renamed rather than moved: shedding and draining are the two
    // ways to stop work and people arrive looking for one wanting the
    // other, so they belong on one screen — but the label has to say that
    // one of them reaches every node.
    { id: "backup", label: "Backup & stopping", sub: "This machine's node: its configuration, and stopping work safely — here, or across the whole mesh." },
    // UX-7. Last, because it is the pane nobody opens to change anything:
    // which build this is, the terms it runs under, and who to ask.
    { id: "about", label: "About & licence", sub: "Which build this is, the terms it runs under, and who to ask for help." },
  ],
};

/**
 * A rail section made of whole views (2026-09).
 *
 * Mesh nests because it is eleven panels of one subject, and its panels
 * are modes of one component. The Run group was six *views* in a flat
 * run — Scripts, Run, Activity, Deployments, Schedules, Services — and at
 * six rows the group had stopped reading as a group. They split on the
 * question a person arrives with:
 *
 *   - **Run** — do one thing, now: what I can run, run it, what happened.
 *   - **Services** — work the mesh keeps going without me: what is kept
 *     up, what runs itself, and what is answering by name.
 *
 * Unlike Mesh's panels these stay separate views with their own deep links
 * and pages; the section is a rail row and a breadcrumb parent
 * and nothing else.
 */
export type NavSection = {
  id: string;
  label: string;
  /** The glyph on the section's tile. */
  icon: string;
  /** In rail order — the order a person works in, not the order they land. */
  views: readonly ViewName[];
  /** Where the section row lands you. */
  home: ViewName;
};

export const SECTIONS: readonly NavSection[] = [
  // What I can run, run it, what happened — and the row lands on the
  // verb the section is named for.
  { id: "work", label: "Run", icon: "run", views: ["library", "run", "jobs"], home: "run" },
  // What is kept up, what runs itself, and where its copies answer. Home
  // is Deployments since 2026-09-14: the last row stopped being a status
  // board of names resolving (the mesh proxy it watched is gone) and
  // became the page you open once, to wire a load balancer — so the
  // definitions are the everyday question here now.
  { id: "services", label: "Services", icon: "registry", views: ["deployments", "schedules", "registry"], home: "deployments" },
];

/** The rail section a view sits under, if any. */
export function sectionOf(view: ViewName): NavSection | null {
  return SECTIONS.find((s) => s.views.includes(view)) ?? null;
}

/** The panel a section opens on when nothing else has been asked for. */
export const SUB_DEFAULT: Partial<Record<ViewName, string>> = {
  // Fleet, not Topology. The ring is the picture and the fleet is the
  // answer — and with the vitals living in the Fleet panel's two About
  // groups, opening on Topology would land a person on the one panel of
  // the seven that has no numbers on it.
  mesh: "nodes",
  settings: "this-peer",
};

/**
 * Mesh panels that were folded into others (2026-09-14), and where each
 * one's deep links land now. A panel id is written into sessionStorage
 * keys, palette entries and the engine's own deep links; renaming the
 * rail must not turn any of them into a click that does nothing.
 *
 * `view` sends the link off Mesh entirely, to the screen that answers it
 * properly; `focus` is the section of Capacity to scroll to.
 */
export const RETIRED_PANELS: Record<string, { view?: ViewName; sub?: string; focus?: string }> = {
  members: { sub: "nodes" },
  trust: { sub: "nodes" },
  events: { view: "jobs" },
  settings: { view: "settings" },
  compute: { sub: "capacity", focus: "compute" },
  runtimes: { sub: "capacity", focus: "runtimes" },
  ports: { sub: "capacity", focus: "ports" },
  volumes: { sub: "capacity", focus: "volumes" },
};

/** What a panel is called, for the breadcrumb. */
export function subLabel(view: ViewName, sub: string | null): string | null {
  if (!sub) return null;
  return SUBNAV[view]?.find((s) => s.id === sub)?.label ?? null;
}
