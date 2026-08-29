/*
 * Landing-page chalkboard — "What's not built yet" (docs/shep-design/
 * README.md, "Screens > 1. Landing page > Chalkboard").
 *
 * Source of truth: docs/specs/deferred.md, two of its sections, kept apart
 * on the board because they mean different things.
 *
 *   - "Named as v1.0 in spec §2/§9, not yet built" is the build queue. Its
 *     items are prose paragraphs opening with a bold name (`**OTLP export
 *     (metrics dog)**`), so the anchor is that bold text.
 *   - "Committed to v1.1+ by design (spec §2)" is the deliberate cuts, and
 *     Windows is the largest. Its items are top-level `- ` bullets, some
 *     bold and some not, so the anchor is each bullet's head: its text up
 *     to the first parenthesis, em dash, or sentence end.
 *
 * Merging the two would be the easy thing and the wrong one. A cut is not a
 * queue item, and a board that showed Windows beside OTLP with no label
 * would promise a tier the maintainer ruled out of v1 on 2026-08-15.
 *
 * Both lists are parsed out of deferred.md rather than hand-copied, and
 * `checkSync` diffs parsed against curated in both directions: an item
 * deferred.md adds, ships, or renames and this file does not follow fails
 * the build instead of shipping a stale claim. A heading-only check (two
 * guards ago) cannot catch that, since the heading survives all three.
 *
 * The curated half is only the display string. Anchors are never typed by
 * hand for their own sake — they exist so the diff has something to compare.
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

const queuedItems: ChalkboardItem[] = [
  { anchor: "OTLP export (metrics dog)", display: "OTLP export" },
];

const cutItems: ChalkboardItem[] = [
  { anchor: "HTTP/SSE MCP transport", display: "HTTP/SSE MCP transport" },
  { anchor: "cgroup v2 enforcement", display: "cgroup v2 enforcement" },
  { anchor: "@shep/io npm shim", display: "@shep/io npm shim" },
  { anchor: "vcs metadata", display: "vcs metadata" },
  { anchor: "shep web JSON status endpoint", display: "shep web JSON endpoint" },
];

/** One labelled row of pills on the board. */
export interface ChalkboardGroup {
  /** What this row of items has in common, shown above the pills. */
  label: string;
  /** Why the row exists, in one sentence, shown under the label. */
  note: string;
  /** The pill text, in the order given. */
  items: string[];
}

export const board: ChalkboardGroup[] = [
  {
    label: "Queued for v1.0",
    note: "Named in the spec, not written yet.",
    items: queuedItems.map((item) => item.display),
  },
  {
    label: "Cut from v1.0, open for v1.1+",
    note: "Decided against for the first release, not forgotten.",
    items: cutItems.map((item) => item.display),
  },
];

const QUEUED_HEADING = "## Named as v1.0 in spec §2/§9, not yet built";
const CUT_HEADING = "## Committed to v1.1+ by design (spec §2)";

/** The text of one `## ` section, throwing if deferred.md no longer has it. */
function section(source: string, heading: string): string {
  const start = source.indexOf(heading);
  if (start === -1) {
    throw new Error(
      `web/src/data/chalkboard.ts: docs/specs/deferred.md no longer has the ` +
        `"${heading}" section this list is parsed from.`,
    );
  }
  const end = source.indexOf("\n## ", start + heading.length);
  return end === -1 ? source.slice(start) : source.slice(start, end);
}

/**
 * The queue section's items, whose paragraphs open with `**anchor**`. Table
 * rows and other bold text elsewhere in the file don't start a line this
 * way, so this stays scoped to the section's own items.
 */
function parseQueuedAnchors(text: string): string[] {
  return [...text.matchAll(/^\*\*(.+?)\*\*/gm)].map((match) => match[1]);
}

/**
 * The cuts section's items, which are top-level `- ` bullets rather than
 * paragraphs. Only column-zero bullets: Windows' own sub-bullets are
 * indented two spaces and are detail about one cut, not six more of them.
 *
 * The anchor is each bullet's head, meaning its text up to the first
 * parenthesis, em dash, or sentence end, with bold and backticks stripped.
 * The rest of the bullet is reasoning that gets edited, and keying the
 * guard on it would turn every prose fix into a failed build.
 */
function parseCutAnchors(text: string): string[] {
  return [...text.matchAll(/^- (.+)$/gm)].map((match) =>
    match[1]
      .split(/ \(| — |\. /)[0]
      .replace(/\*\*/g, "")
      .replace(/`/g, "")
      .trim(),
  );
}

/**
 * Diffs parsed anchors against curated ones in both directions, throwing on
 * the first disagreement. Both directions matter and they fail differently:
 * something deferred.md gained that the board never shows, and something the
 * board still claims that deferred.md dropped, usually because it shipped.
 */
function checkSync(what: string, parsed: string[], curated: string[]): void {
  const missing = parsed.filter((anchor) => !curated.includes(anchor));
  const stale = curated.filter((anchor) => !parsed.includes(anchor));
  if (missing.length === 0 && stale.length === 0) {
    return;
  }

  const parts: string[] = [];
  if (missing.length > 0) {
    parts.push(`deferred.md names ${JSON.stringify(missing)} that ${what} has no entry for`);
  }
  if (stale.length > 0) {
    parts.push(
      `${what} still names ${JSON.stringify(stale)}, which deferred.md no ` +
        `longer has — it likely shipped`,
    );
  }
  throw new Error(
    `web/src/data/chalkboard.ts: out of sync with docs/specs/deferred.md — ` +
      `${parts.join("; ")}. Update it to match.`,
  );
}

checkSync(
  "queuedItems",
  parseQueuedAnchors(section(deferredSource, QUEUED_HEADING)),
  queuedItems.map((item) => item.anchor),
);
checkSync(
  "cutItems",
  parseCutAnchors(section(deferredSource, CUT_HEADING)),
  cutItems.map((item) => item.anchor),
);
