// Theme state. Three modes, not two: "system" is a real choice, not the
// absence of one, and it is the default because the operating system
// already knows what the room looks like.
//
// The attribute is stamped before first paint by the inline script in
// index.html; this module only owns changes made after boot.

export type ThemeMode = "system" | "light" | "dark";

const KEY = "mw:theme";

function read(): ThemeMode {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark" || v === "system") return v;
  } catch {
    /* storage disabled — system preference still works */
  }
  return "system";
}

class Theme {
  mode = $state<ThemeMode>(read());
  /** What is actually on screen right now, after resolving "system". */
  resolved = $state<"light" | "dark">("light");

  #media: MediaQueryList | null = null;

  constructor() {
    if (typeof window === "undefined") return;
    this.#media = window.matchMedia("(prefers-color-scheme: dark)");
    this.#media.addEventListener("change", () => this.#apply(false));
    this.#apply(false);
  }

  set(mode: ThemeMode) {
    this.mode = mode;
    try {
      localStorage.setItem(KEY, mode);
    } catch {
      /* nothing to persist to; the session still switches */
    }
    this.#apply(true);
  }

  #apply(userInitiated: boolean) {
    const root = document.documentElement;
    // Suppress transitions across the swap. A 240ms cross-fade of every
    // surface in the app is performative; the instant swap is what makes
    // it read as two modes rather than one mode with an effect.
    if (userInitiated) {
      root.classList.add("theme-switching");
      requestAnimationFrame(() => requestAnimationFrame(() => root.classList.remove("theme-switching")));
    }
    if (this.mode === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", this.mode);
    this.resolved =
      this.mode === "system" ? (this.#media?.matches ? "dark" : "light") : this.mode;
  }
}

export const theme = new Theme();
