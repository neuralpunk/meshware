// The SSH port this operator's machines answer on, remembered between
// sheets.
//
// A fleet that moved SSH off 22 moved all of it, so this is one fact about
// one estate rather than a per-machine setting — and retyping it into every
// sheet is exactly the friction that makes a person give up on the field
// and edit each row by hand instead, which is the situation the field
// exists to end.
//
// Credentials deliberately do not survive a closed sheet. A port number is
// not a credential: it is on the wire of every connection either way, and
// knowing it grants nothing.

const KEY = "meshware:ssh-port";

/** 22 unless this operator has said otherwise. */
export function rememberedSshPort(): number {
  try {
    const n = Number(localStorage.getItem(KEY));
    return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : 22;
  } catch {
    // Private windows and locked-down browsers throw rather than answer.
    return 22;
  }
}

/** Remember a port that was actually used, so the next sheet opens on it. */
export function rememberSshPort(port: number): void {
  try {
    if (!Number.isInteger(port) || port < 1 || port > 65535) return;
    // 22 is the default rather than a preference, so it clears the note
    // instead of writing one — otherwise a fleet that moved *back* to 22
    // would keep an entry nobody can find to remove.
    if (port === 22) localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, String(port));
  } catch {
    // Nothing to do: the sheet still works, it just opens on 22 next time.
  }
}
