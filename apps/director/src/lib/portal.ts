/**
 * Move an element to `<body>` so that `position: fixed` means the window.
 *
 * `fixed` is only fixed to the viewport while nothing above it has claimed
 * the job of containing block. A transform, a filter, `contain`,
 * `will-change: transform` — any of them turns the ancestor into the frame
 * that `inset: 0` measures against, and a full-window modal silently
 * becomes a modal the size of whichever pane it was declared in. The
 * failure is invisible in the markup: the overlay says `fixed inset-0`, and
 * it is telling the truth about a box nobody meant to draw.
 *
 * A tab pane, a card, a canvas viewport and an entrance animation are all
 * one line of CSS away from doing this. Rather than police every ancestor
 * of every overlay for ever, the overlays leave: they are declared where
 * they belong in the component tree — next to the state and the handlers
 * that own them — and mount at the top of the document, where the only
 * containing block is the window itself.
 *
 * Only the DOM node moves. Props, context, events and Svelte's own
 * teardown all follow the component tree, which is untouched.
 */
export function portal(node: HTMLElement, target?: HTMLElement) {
  (target ?? document.body).appendChild(node);
  return {
    // Svelte removes the node itself when the block closes; this is here for
    // the case where the action is torn down on its own, and is a no-op
    // otherwise.
    destroy() {
      node.remove();
    },
  };
}
