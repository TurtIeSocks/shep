/*
 * Landing-page lexicon signposts — the six terms featured in the "Fifteen
 * words carry the whole product" section (docs/shep-design/README.md,
 * "Screens > 1. Landing page > Lexicon").
 *
 * Source of truth: docs/terminology.md. That file's table is not uniformly
 * structured for card rendering — some "Where it applies" cells are one
 * clean CLI example (`shep flock` (list)), others run a paragraph of
 * caveats (the "a sheep" row) — so the term / meaning / CLI line below are
 * hand-curated prose, not a mechanical parse. What IS mechanical: the check
 * below reads terminology.md at build time and fails the build if any of
 * these six terms has been renamed or dropped from the approved lexicon, so
 * a rename in that file can't silently leave this page out of sync.
 *
 * Card styling (bg/fg/tilt) is presentational, not sourced from a doc —
 * verbatim from design-files/Shep Landing v3 scene.dc.html's `signs` array.
 */
// `?raw` is a Vite import suffix: it inlines the file's text content at
// build time, resolved against this module's own location in the source
// tree — unlike `node:fs` + `import.meta.url`, which breaks once the build
// bundles this module somewhere else on disk (its import.meta.url no
// longer points at web/src/data/, so a relative fs read 404s at build
// time even though it works fine under `astro dev`).
import terminologySource from "../../../docs/terminology.md?raw";

export interface LexiconSignpost {
  /** Display term, as it reads on the card (may include an article). */
  term: string;
  meaning: string;
  cli: string;
  bg: string;
  fg: string;
  tilt: string;
}

export const lexiconSignposts: LexiconSignpost[] = [
  {
    term: "the flock",
    meaning: "Every managed process, as a set. Always the plural term.",
    cli: "shep flock · list · ls",
    bg: "var(--fleece)",
    fg: "var(--ink)",
    tilt: "-1.4deg",
  },
  {
    term: "a sheep",
    meaning: "One managed process. Singular only, so nothing is ambiguous.",
    cli: "shep describe web",
    bg: "var(--butter)",
    fg: "var(--ink)",
    tilt: "1.1deg",
  },
  {
    term: "the shepherd",
    meaning: "The daemon. Only ever the daemon.",
    cli: "log messages, docs",
    bg: "var(--fleece)",
    fg: "var(--ink)",
    tilt: "1.3deg",
  },
  {
    term: "bleats",
    meaning: "Logs. shep logs is the same command and always will be.",
    cli: "shep bleats --follow",
    bg: "var(--grass-deep)",
    fg: "var(--fleece)",
    tilt: "-1deg",
  },
  {
    term: "a fold",
    meaning: "A namespace or group of sheep.",
    cli: "shep fold backend",
    bg: "var(--fleece)",
    fg: "var(--ink)",
    tilt: "-1.2deg",
  },
  {
    term: "muster",
    meaning: "Bring a saved flock back after a reboot.",
    cli: "shep save · shep muster",
    bg: "var(--barn)",
    fg: "var(--fleece)",
    tilt: "1.4deg",
  },
];

// Build-time guard: the doc's own bolded spelling of each term, not the
// (sometimes articled) display form above — fail loudly rather than let
// this page quietly drift from the approved lexicon.
const approvedTerms = ["the shepherd", "the flock", "a sheep", "bleats", "fold", "muster"];
for (const term of approvedTerms) {
  if (!terminologySource.includes(`**${term}**`)) {
    throw new Error(
      `web/src/data/lexicon.ts: "${term}" no longer appears bolded in ` +
        `docs/terminology.md — re-check this file's curated signposts ` +
        `against the current approved lexicon.`,
    );
  }
}
