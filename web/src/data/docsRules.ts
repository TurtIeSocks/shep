/*
 * Terminology page "Usage rules" cards (docs/shep-design/README.md,
 * "Screens > 2. Docs > Terminology").
 *
 * Source of truth: docs/terminology.md, "## Usage rules (readability >
 * theme)". That section is five numbered prose paragraphs, each mixing a
 * rule with its rationale and a parenthetical aside or two — not a clean
 * title/body split — so this is hand-curated rather than mechanically
 * parsed. The guard below just confirms the section heading is still there,
 * so a restructure of that file doesn't silently orphan this page.
 */
// `?raw` (see web/src/data/lexicon.ts's header comment) inlines the file's
// text content at build time.
import terminologySource from "../../../docs/terminology.md?raw";

export interface UsageRule {
  title: string;
  body: string;
}

export const usageRules: UsageRule[] = [
  {
    title: "Straight verbs always work",
    body: "start, stop, restart, list, logs and delete are first-class aliases forever. Sheep terms are the personality layer, not a wall.",
  },
  {
    title: "Destructive operations keep plain names",
    body: "kill, delete, exit codes and error messages carry zero whimsy — misreading one costs a process.",
  },
  {
    title: "Types may be themed when self-evident",
    body: "Flock, Fold and Bark are fine. Heft as a struct name is not; it is called host.",
  },
  {
    title: "Playful in prose, exact in reference",
    body: "The README can say shep keeps your flock alive. The config reference says process.",
  },
  {
    title: "Log and error output stays technical",
    body: "The dog barks in webhooks, not in stderr.",
  },
];

const anchorHeading = "## Usage rules (readability > theme)";
if (!terminologySource.includes(anchorHeading)) {
  throw new Error(
    `web/src/data/docsRules.ts: docs/terminology.md no longer has the ` +
      `"${anchorHeading}" section these cards were curated from — re-check ` +
      `usageRules against the current file.`,
  );
}
