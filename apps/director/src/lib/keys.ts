// Keyboard chords — matched once, labelled once.
//
// Two bugs live in the naive spelling of a shortcut, and both of them are
// silent, which is why this is a module rather than an inline condition.
//
//   1. **`e.key` is the character the layout produces, not the key.** On
//      Dvorak the key engraved K sits where QWERTY's V is; on AZERTY the
//      whole top row moves; on a Cyrillic or Greek layout none of the
//      letters are Latin at all. A handler that tests `e.key === "k"` works
//      for the person who wrote it and silently does nothing for everybody
//      else — and the palette is the first thing a new user reaches for, so
//      it is the worst possible shortcut to lose. `e.code` is the physical
//      position and is identical on every layout, so it is what we match;
//      `e.key` stays in the test as well, so the key that *prints* k works
//      too on a layout where those are different places.
//
//   2. **`⌘` is a Mac key, and Director is a Linux app.** Here the glyph
//      names either nothing or the Super key — and Super+K is the window
//      manager's to grab, not ours. Telling a user to press a chord that
//      cannot reach the application is worse than telling them nothing:
//      they try it, it does nothing, and they conclude the feature is
//      broken. The modifier is `Ctrl`, spelled once, here.

/**
 * How to write the primary modifier.
 *
 * Used in every keycap and every tooltip that names a chord, so the app
 * never advertises a key the keyboard in front of the user does not have.
 */
export const MOD = "Ctrl";

/** A chord, spelled for the reader: `modKey("K")` → `Ctrl+K`. */
export function modKey(key: string): string {
  return `${MOD}+${key}`;
}

/**
 * Is this event Ctrl + `code`, whatever the layout prints there?
 *
 * `code` is a `KeyboardEvent.code` value (`"KeyK"`, `"KeyS"`), and the
 * letter it names is also compared against `e.key` so that both the key in
 * that *position* and the key that *prints* that letter answer to the
 * chord. Alt is excluded: `Ctrl+Alt+K` is AltGr on a European layout and
 * belongs to whatever character it composes, not to us. Super is excluded
 * with it: that one is the window manager's.
 */
export function isChord(e: KeyboardEvent, code: `Key${string}`): boolean {
  if (!e.ctrlKey || e.altKey) return false;
  if (e.code === code) return true;
  const letter = code.slice(3).toLowerCase();
  return e.key.length === 1 && e.key.toLowerCase() === letter;
}
