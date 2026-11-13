// A table row that opens something, by keyboard as well as by pointer
// (DIR-12).
//
// Machines, jobs, deployments, schedules, registry
// services and benchmark runs each open from a click on their row — and
// only from a click: the row was not focusable, so none of them could be
// opened from the keyboard at all. `use:rowAction={open}` makes the row a
// stop in the Tab order that opens on Enter or Space, and ArrowUp /
// ArrowDown move between the rows of the same table, so a list of two
// hundred machines is not two hundred presses of Tab.
//
// Only when the row itself has focus: a checkbox or a button inside the
// row keeps its own keys.

export function rowAction(node: HTMLElement, open: () => void) {
  let current = open;
  node.tabIndex = 0;
  node.dataset.rowaction = "";

  function onkeydown(e: KeyboardEvent) {
    if (e.target !== node || e.altKey || e.ctrlKey || e.metaKey) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      current();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      const rows = [...(node.parentElement?.querySelectorAll<HTMLElement>(":scope > [data-rowaction]") ?? [])];
      const at = rows.indexOf(node);
      const next = rows[e.key === "ArrowDown" ? at + 1 : at - 1];
      if (next) {
        e.preventDefault();
        next.focus();
      }
    }
  }
  node.addEventListener("keydown", onkeydown);

  return {
    update(next: () => void) {
      current = next;
    },
    destroy() {
      node.removeEventListener("keydown", onkeydown);
    },
  };
}
