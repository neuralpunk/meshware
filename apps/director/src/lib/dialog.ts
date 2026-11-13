// What every dialog owes a keyboard (DIR-12).
//
// Director's dialogs were `role="dialog" aria-modal="true"` and nothing
// more: focus stayed on the button behind the scrim, Tab walked out of the
// dialog into the page it was covering, closing it dropped focus on the
// body, and each dialog listened for Escape on the window — so one Escape
// closed every dialog open at once, a confirmation and the sheet it was
// raised from together. Enterprise procurement asks for keyboard access in
// writing; these are its basics, in one place, on every dialog.
//
//   * **In.** Focus moves into the dialog when it opens — to the field
//     marked `autofocus` if there is one, otherwise to the dialog itself,
//     so a screen reader announces its name and Tab starts at its top.
//   * **Held.** Tab and Shift-Tab wrap within it.
//   * **Out.** Escape closes the dialog on top and nothing under it, and
//     closing puts focus back on whatever opened it.
//   * **Named.** A dialog without a label is labelled by its first heading.

const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled]):not([type=hidden])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  "[tabindex]:not([tabindex='-1'])",
  "[contenteditable='true']",
].join(",");

/** The elements Tab can reach inside `root`, in order. */
export function focusables(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter(
    (el) => !el.closest("[inert]") && el.getClientRects().length > 0,
  );
}

/** Is `node` the dialog on top — the last modal dialog in the document? */
export function isTopmost(node: HTMLElement): boolean {
  const all = document.querySelectorAll('[role="dialog"][aria-modal="true"]');
  return all.length === 0 || all[all.length - 1] === node;
}

let labelSeq = 0;

export type DialogOptions = {
  /** Escape, when this dialog is on top. Leave unset for a dialog that
   *  must not close from the keyboard (a publish in flight). */
  onescape?: (() => void) | null;
  /** A name for a dialog with neither a label nor a heading to take one
   *  from — the dark modal's kicker, say. */
  label?: string | null;
};

/**
 * `use:dialog={{ onescape }}` on the element with `role="dialog"`.
 */
export function dialog(node: HTMLElement, options: DialogOptions = {}) {
  let opts = options;
  const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;

  if (!node.hasAttribute("aria-label") && !node.hasAttribute("aria-labelledby")) {
    const heading = node.querySelector<HTMLElement>("h1, h2, h3, h4");
    if (heading) {
      if (!heading.id) heading.id = `dialog-title-${++labelSeq}`;
      node.setAttribute("aria-labelledby", heading.id);
    } else if (options.label) {
      node.setAttribute("aria-label", options.label);
    }
  }
  if (!node.hasAttribute("tabindex")) node.setAttribute("tabindex", "-1");

  // After the dialog's own `autofocus` has had its turn: a field that asked
  // for focus keeps it.
  queueMicrotask(() => {
    if (!node.isConnected || node.contains(document.activeElement)) return;
    node.focus({ preventScroll: true });
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      if (e.defaultPrevented || !isTopmost(node) || !opts.onescape) return;
      e.preventDefault();
      opts.onescape();
      return;
    }
    if (e.key !== "Tab" || !isTopmost(node)) return;
    const items = focusables(node);
    if (items.length === 0) {
      e.preventDefault();
      node.focus();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const at = document.activeElement;
    const inside = at instanceof HTMLElement && node.contains(at);
    if (e.shiftKey && (at === first || at === node || !inside)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && (at === last || !inside)) {
      e.preventDefault();
      first.focus();
    }
  }
  // On the window, as the key bubbles up: something inside the dialog that
  // Escape closes first — a menu, a popover, a search box — takes it by
  // calling `preventDefault`, and the dialog stays. Tab is caught here even
  // when focus has somehow ended up outside it.
  window.addEventListener("keydown", onkeydown);

  return {
    update(next: DialogOptions = {}) {
      opts = next;
    },
    destroy() {
      window.removeEventListener("keydown", onkeydown);
      // Back where the person was — unless they have already moved on to
      // something else that is still there.
      const at = document.activeElement;
      const lost = !at || at === document.body || node.contains(at);
      if (lost && opener?.isConnected) opener.focus({ preventScroll: true });
    },
  };
}
