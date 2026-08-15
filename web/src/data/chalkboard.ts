/*
 * Landing-page chalkboard — "What's not built yet" (docs/shep-design/
 * README.md, "Screens > 1. Landing page > Chalkboard").
 *
 * Source of truth: docs/specs/deferred.md, section "Named as v1.0 in spec
 * §2/§9, not yet built", plus the Windows functional tier that section
 * calls out on its own. deferred.md is prose, not a table, so this list is
 * hand-curated rather than mechanically parsed — the check below just
 * confirms the section heading it was curated from is still there, so a
 * restructure of that file doesn't silently orphan this page.
 *
 * The original design handoff (design-files/Shep Landing v3 scene.dc.html)
 * listed thirteen items here. Five have since shipped and are gone from
 * this list: `scale` (now `shep stock`), `signal`, `sendline` (now
 * `shep whisper`), the key-value store, and lambs in `describe` — all
 * confirmed shipped in deferred.md's "Not deferred" section as of the
 * 2026-08-12 audit. Re-check this array against deferred.md whenever a
 * phase ships.
 *
 * Phase 12a (merged after that audit) shipped lookout's shell and its
 * flock table pane, so the flat "the lookout TUI" entry this list used to
 * carry is now false — deferred.md's own "lookout's other three panes"
 * section says plainly that the shell and flock table exist. Narrowed to
 * name only what is still missing, matching that section's own wording.
 */
// `?raw` (see lexicon.ts's header comment for why, not node:fs +
// import.meta.url) inlines the file's text content at build time.
import deferredSource from "../../../docs/specs/deferred.md?raw";

export const notBuiltYet: string[] = [
  "lookout's bleats feed, sheep pane, host strip",
  "the whistle MCP server",
  "shep serve",
  "shep dev",
  "shep runtime",
  ".js Flockfiles",
  "a schemars config JSON schema",
  "a CLI-flag config layer",
  "openrc and BSD rc.d units",
  "OTLP export",
  "Windows, entirely",
];

const anchorHeading = "## Named as v1.0 in spec §2/§9, not yet built";
if (!deferredSource.includes(anchorHeading)) {
  throw new Error(
    `web/src/data/chalkboard.ts: docs/specs/deferred.md no longer has the ` +
      `"${anchorHeading}" section this list was curated from — re-check ` +
      `notBuiltYet against the current file.`,
  );
}
