// Every icon a screen asks for exists, and every icon is drawn to the
// same rules (2026-09-22).
//
// `Icon` draws nothing for a name it does not know, silently — which is
// how the deployment builder's two remove buttons were blank for as long
// as they existed (`name="x"`, in a set that calls it `close`). A missing
// file fails here instead.
import { describe, expect, it } from "vitest";

// Read through Vite, as the app reads them — no filesystem access, so no
// Node types in a project that has none.
const sources = import.meta.glob<string>(["../**/*.svelte", "../**/*.ts", "!../**/*.test.ts"], {
  query: "?raw",
  import: "default",
  eager: true,
});
const files = import.meta.glob<string>("./icons/*.svg", { query: "?raw", import: "default", eager: true });
const icons = new Map(
  Object.entries(files)
    .filter(([path]) => !path.includes("/brand-"))
    .map(([path, svg]) => [path.slice(path.lastIndexOf("/") + 1, -4), svg]),
);

describe("icons", () => {
  it("has a file for every icon named in the source", () => {
    expect(Object.keys(sources).length).toBeGreaterThan(50);
    const missing: string[] = [];
    // The literal spellings: `<Icon name="…">`, and the `icon` prop or
    // field every table of icons in the app uses.
    const named = /<Icon\s+name="([a-z0-9-]+)"|\bicon(?:=|:\s*)"([a-z0-9-]+)"/g;
    for (const [file, text] of Object.entries(sources)) {
      for (const m of text.matchAll(named)) {
        const name = m[1] ?? m[2];
        if (!icons.has(name)) missing.push(`${file}: ${name}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it("draws every icon on one grid, in one stroke, in the text's colour", () => {
    expect(icons.size).toBeGreaterThan(50);
    for (const [name, svg] of icons) {
      const root = svg.match(/<svg[^>]*>/)?.[0] ?? "";
      expect(root, name).toContain('viewBox="0 0 24 24"');
      expect(root, name).toContain('stroke="currentColor"');
      expect(root, name).toContain('stroke-linecap="round"');
      // Size and weight are `Icon`'s to set, not the file's.
      expect(root, name).not.toMatch(/\s(width|height|class)="/);
    }
  });
});
