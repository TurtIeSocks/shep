/*
 * Landing-page chalkboard — "What's not built yet" (docs/shep-design/
 * README.md, "Screens > 1. Landing page > Chalkboard").
 *
 * Source of truth: docs/specs/deferred.md, section "Named as v1.0 in spec
 * §2/§9, not yet built". That section is prose, not a table, but every item
 * in it opens its paragraph with a bold name (`**lookout actions**`, etc.),
 * so the anchor below is parsed out of the section rather than hand-copied,
 * and `notBuiltYet` pairs each anchor with a landing-page-friendly display
 * string. `parseAnchors` below diffs the parsed set against the curated
 * one in both directions — an item deferred.md adds, ships, or renames and
 * this file doesn't follow fails the build instead of shipping a stale
 * claim. A heading-only check (the previous version of this guard) cannot
 * catch that: the heading survives every one of those changes unchanged.
 *
 * The original design handoff (design-files/Shep Landing v3 scene.dc.html)
 * listed thirteen items here. Five had shipped as of the 2026-08-12 audit:
 * `scale` (now `shep stock`), `signal`, `sendline` (now `shep whisper`), the
 * key-value store, and lambs in `describe`. Phase 12b shipped three more —
 * lookout's bleats feed, sheep detail pane, and host-usage strip, plus
 * whistle, the MCP server. Phase 14/15 and the Windows scope call shipped or
 * moved seven more — serve, dev/runtime, openrc and BSD rc.d units, the
 * Windows functional tier (moved to deferred.md's "Committed to v1.1+ by
 * design" section rather than shipping — see Chalkboard.astro's own prose
 * for that one, since it is a permanent cut, not a build-queue item this
 * list tracks), `.js` Flockfile, schemars JSON-schema export, and the
 * daemon-config flags layer — so this list is down to the four items
 * deferred.md's section currently names.
 */
// `?raw` (see lexicon.ts's header comment for why, not node:fs +
// import.meta.url) inlines the file's text content at build time.
import deferredSource from "../../../docs/specs/deferred.md?raw";

interface ChalkboardItem {
  /** The bold text deferred.md's paragraph opens with, verbatim. */
  anchor: string;
  /** Landing-page phrasing shown in the chalkboard pill. */
  display: string;
}

const notBuiltYetItems: ChalkboardItem[] = [
  { anchor: "OTLP export (metrics dog)", display: "OTLP export" },
  { anchor: "lookout's search/filter", display: "lookout's search and filter" },
  { anchor: "lookout actions", display: "lookout's actions" },
  {
    anchor: "lambs in the detail pane",
    display: "lambs in lookout's detail pane",
  },
];

export const notBuiltYet: string[] = notBuiltYetItems.map((item) => item.display);

const HEADING = "## Named as v1.0 in spec §2/§9, not yet built";

function parseAnchors(source: string): string[] {
  const headingIndex = source.indexOf(HEADING);
  if (headingIndex === -1) {
    throw new Error(
      `web/src/data/chalkboard.ts: docs/specs/deferred.md no longer has the ` +
        `"${HEADING}" section this list is parsed from.`,
    );
  }
  const nextHeadingIndex = source.indexOf("\n## ", headingIndex + HEADING.length);
  const section =
    nextHeadingIndex === -1
      ? source.slice(headingIndex)
      : source.slice(headingIndex, nextHeadingIndex);

  // A paragraph in this section opens its line with `**anchor**`. Table
  // rows and other bold text elsewhere in the file don't start a line this
  // way, so this pattern stays scoped to the section's own items.
  return [...section.matchAll(/^\*\*(.+?)\*\*/gm)].map((match) => match[1]);
}

const parsedAnchors = parseAnchors(deferredSource);
const curatedAnchors = notBuiltYetItems.map((item) => item.anchor);

const missingFromChalkboard = parsedAnchors.filter(
  (anchor) => !curatedAnchors.includes(anchor),
);
const staleInChalkboard = curatedAnchors.filter(
  (anchor) => !parsedAnchors.includes(anchor),
);

if (missingFromChalkboard.length > 0 || staleInChalkboard.length > 0) {
  const parts: string[] = [];
  if (missingFromChalkboard.length > 0) {
    parts.push(
      `deferred.md names ${JSON.stringify(missingFromChalkboard)} that ` +
        `notBuiltYetItems has no entry for`,
    );
  }
  if (staleInChalkboard.length > 0) {
    parts.push(
      `notBuiltYetItems still names ${JSON.stringify(staleInChalkboard)}, ` +
        `which deferred.md's "${HEADING}" section no longer has — it ` +
        `likely shipped`,
    );
  }
  throw new Error(
    `web/src/data/chalkboard.ts: out of sync with docs/specs/deferred.md — ` +
      `${parts.join("; ")}. Update notBuiltYetItems to match.`,
  );
}
