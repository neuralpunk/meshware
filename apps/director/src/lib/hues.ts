// The companion hues — the colours that keep the brand teal company.
//
// Director's first commercial pass gave it one hue and a family of cool
// greys, and the result read as a monochrome: teal on white by day, teal
// on black by night, and nothing else anywhere. A single accent is the
// right rule for *emphasis* — one accent pixel per screen is what makes
// the eye land — but identity needs more than emphasis. These are five
// neighbours picked off the teal's own wheel (its complement, the warm
// pair either side of it, and the two cool ones): coral, amber, indigo,
// plum and sky. Each is a triplet in `app.css` — a fill, an ink that
// clears 4.5:1 on its wash, and the wash — re-pointed per theme like every
// other token, never inverted.
//
// Where they go is decided once, here, so a section is the same colour on
// the rail, in the page header, in the palette and on the wizard's cards:
// a person learns that Run is indigo the way they learn that Jobs is the
// fourth item. Status colours are NOT in this set — ok/warn/error/running
// keep their own tokens and their own meaning.
import type { ViewName } from "./state.svelte";

export type Hue =
  | "teal"
  | "coral"
  | "amber"
  | "indigo"
  | "plum"
  | "sky"
  | "green"
  | "gray";

/**
 * The rotation `hueOf` picks a person's colour from — the six that read as
 * *somebody's* colour. Green and gray are section colours only: a grey
 * avatar reads as a disabled account, and green is spoken for by "running".
 */
const HUES: readonly Hue[] = ["teal", "coral", "amber", "indigo", "plum", "sky"];

/**
 * One hue per section — and, since the 6c pass, one *tile* per section:
 * these are the 20px rounded squares down the sidebar, which is the only
 * place in the product a hue is spent as a fill.
 *
 * That made the assignments load-bearing in a way they had not been. Two
 * adjacent entries in the same colour used to cost nothing, because the
 * colour was a 16px glyph at 64% mix; as two solid tiles a hand's width
 * apart it costs the whole point of having them. Scripts and Run were both
 * indigo and sit next to each other; Settings was teal, which is the mesh's
 * colour and the brand's. Eight sections, eight colours, and Settings takes
 * the grey — it is the one section that is furniture rather than a place
 * in the mesh, and System Settings says so about its own the same way.
 *
 * `nodes` and `compute` are panels of Mesh now, and take the hue of the
 * panel they became, so an old deep link still reads the same.
 */
export const VIEW_HUE: Record<ViewName, Hue> = {
  mesh: "teal",
  nodes: "teal",
  compute: "amber",
  // The Run section (`chrome/nav.ts` SECTIONS) is green, and its three
  // views take the section's colour the way Mesh's panels all take teal:
  // one tile in the rail, one hue on the page, whichever of the three is
  // open. Scripts used to be coral and Activity sky; a page that changed
  // colour as you moved between rows of one section made the section
  // look like three.
  run: "green",
  library: "green",
  jobs: "green",
  // The Services section is sky, for the same reason.
  registry: "sky",
  deployments: "sky",
  schedules: "sky",
  access: "indigo",
  secrets: "plum",
  settings: "gray",
};

/**
 * The inline style that points the three `--hue*` custom properties at a
 * hue, so a component can be written once against `var(--hue)`,
 * `var(--hue-ink)` and `var(--hue-wash)` and coloured by its caller.
 */
export function hueStyle(h: Hue): string {
  return `--hue: var(--hue-${h}); --hue-ink: var(--hue-${h}-ink); --hue-wash: var(--hue-${h}-wash);`;
}

/** A stable hue for a name — avatars, mostly. Same name, same colour, on
 *  every machine, without anybody having to choose one. */
export function hueOf(name: string): Hue {
  let h = 0;
  for (const ch of name) h = (h * 31 + ch.codePointAt(0)!) >>> 0;
  return HUES[h % HUES.length];
}
