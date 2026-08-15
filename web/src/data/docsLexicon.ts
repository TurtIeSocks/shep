/*
 * Terminology page lexicon table — the full "shep says / means / where you
 * meet it / built" grid (docs/shep-design/README.md, "Screens > 2. Docs >
 * Terminology").
 *
 * Source of truth: README.md's own "## The lexicon" table. Unlike the
 * landing page's six hand-curated signposts (web/src/data/lexicon.ts,
 * sourced from docs/terminology.md's prose-heavy table), this one IS
 * mechanically parsed — README.md's table is already a clean four-column
 * grid with the exact "built?" column this page needs, and it's the
 * current, actively-maintained one (docs/terminology.md predates several
 * shipped verbs and has no built column at all). Parsing it means a rename,
 * a new row, or a yes/no/partly flip in README.md shows up here on the next
 * build with no hand-editing.
 */
// `?raw` (see web/src/data/lexicon.ts's header comment) inlines the file's
// text content at build time.
import readmeSource from "../../../README.md?raw";

export interface LexiconRow {
  /** Plain text — rendered in the term column's own font/color, no markup. */
  term: string;
  /** May contain `code` spans; rendered with set:html after mdInlineToHtml. */
  means: string;
  /** May contain `code` spans; rendered with set:html after mdInlineToHtml. */
  where: string;
  built: "yes" | "partly" | "no";
}

const HEADING = "## The lexicon";
const expectedHeader = ["shep says", "Means", "Where you meet it", "Built?"];

function splitRow(line: string): string[] {
  // Drop the leading/trailing "|" a markdown table row starts and ends
  // with, then split on the rest. Cells never contain a literal "|" in
  // this table, so no escaping to worry about.
  return line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((cell) => cell.trim());
}

function parseLexiconTable(source: string): LexiconRow[] {
  const headingIndex = source.indexOf(HEADING);
  if (headingIndex === -1) {
    throw new Error(
      `web/src/data/docsLexicon.ts: README.md no longer has a "${HEADING}" ` +
        `section — the Terminology page's table has nothing to read.`,
    );
  }
  const nextHeadingIndex = source.indexOf("\n## ", headingIndex + HEADING.length);
  const section =
    nextHeadingIndex === -1
      ? source.slice(headingIndex)
      : source.slice(headingIndex, nextHeadingIndex);

  const tableLines = section
    .split("\n")
    .filter((line) => line.trim().startsWith("|"));

  // First table line is the header row, second is the "|---|---|" divider.
  const [headerLine, dividerLine, ...bodyLines] = tableLines;
  if (!headerLine || !dividerLine) {
    throw new Error(
      `web/src/data/docsLexicon.ts: found "${HEADING}" but no markdown ` +
        `table under it in README.md.`,
    );
  }

  const header = splitRow(headerLine);
  const headerMatches = expectedHeader.every((col, i) => header[i] === col);
  if (!headerMatches) {
    throw new Error(
      `web/src/data/docsLexicon.ts: README.md's lexicon table header is ` +
        `now [${header.join(", ")}] — expected [${expectedHeader.join(", ")}]. ` +
        `Update the column mapping below to match.`,
    );
  }

  const rows: LexiconRow[] = bodyLines.map((line) => {
    const [term, means, where, built] = splitRow(line);
    if (built !== "yes" && built !== "partly" && built !== "no") {
      throw new Error(
        `web/src/data/docsLexicon.ts: lexicon row "${term}" has an ` +
          `unrecognized built value "${built}" (expected yes/partly/no).`,
      );
    }
    const builtTag: LexiconRow["built"] = built;
    return { term, means, where, built: builtTag };
  });

  if (rows.length < 15) {
    // Not a hard invariant — the lexicon grows — but a big drop is more
    // likely a parse break than a real shrink, so fail loudly rather than
    // silently ship a half-empty table.
    throw new Error(
      `web/src/data/docsLexicon.ts: parsed only ${rows.length} lexicon rows ` +
        `from README.md, expected at least 15 — check the table didn't ` +
        `change shape.`,
    );
  }

  return rows;
}

export const lexiconTable: LexiconRow[] = parseLexiconTable(readmeSource);

/**
 * Turns `` `code` `` spans into `<code>` tags for use with Astro's
 * `set:html`. README.md's table cells are plain prose plus the occasional
 * code span — no other markdown (no links, no emphasis) — so this is
 * deliberately narrow rather than a general markdown-to-HTML pass.
 */
export function mdInlineToHtml(text: string): string {
  const escaped = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  return escaped.replace(/`([^`]+)`/g, "<code>$1</code>");
}
