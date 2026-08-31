/*
 * The workspace's own version, read from Cargo.toml at Astro build time.
 *
 * It used to ride along in web/src/data/cli-reference.generated.txt, as a
 * `shep --version` line the generator script dumped ahead of the help text.
 * That made a release bump the one thing in a generated file that nothing
 * regenerates: 0.1.19, 0.1.20 and 0.1.21 each shipped and each left that
 * file wrong by exactly one line, with no build failing and no diff to
 * review. Reading Cargo.toml instead means the committed dump changes only
 * when the CLI surface moves, and the version can no longer go stale at all.
 *
 * Note what this does and does not claim. The version below is the
 * workspace's, right now; the help text on the reference page is whatever
 * the last run of the generator captured. If someone changes a flag and
 * skips the docs trigger in CLAUDE.md, the page shows the new version over
 * older help — which was already true before, with the added insult that
 * the version was stale too.
 */
// `?raw` (see lexicon.ts's header comment for why, not node:fs +
// import.meta.url) inlines the file's text content at build time.
import cargoTomlSource from "../../../Cargo.toml?raw";

function fail(message: string): never {
  throw new Error(`web/src/data/workspaceVersion.ts: ${message}`);
}

/**
 * Pulls `version` out of Cargo.toml's `[workspace.package]` table.
 *
 * Scoped to that one table on purpose: every dependency entry in the file
 * has a `version` key too, so the first match in the file is a coin flip.
 * Anything unexpected throws rather than returning a guess — an empty or
 * wrong version rendered on a published page is the failure being fixed
 * here, so it must not be reachable by falling through.
 */
function parseWorkspaceVersion(source: string): string {
  const lines = source.split("\n");
  const tableIndex = lines.findIndex((line) => line.trim() === "[workspace.package]");
  if (tableIndex === -1) {
    fail("Cargo.toml has no [workspace.package] table to read the version from.");
  }

  for (const line of lines.slice(tableIndex + 1)) {
    const trimmed = line.trim();
    if (trimmed.startsWith("[")) {
      break; // The next table started; this one had no version key.
    }
    const match = /^version\s*=\s*"([^"]+)"/.exec(trimmed);
    if (match?.[1] !== undefined) {
      return match[1];
    }
  }

  fail('Cargo.toml\'s [workspace.package] table has no `version = "..."` key.');
}

/** e.g. `"0.1.21"` — no leading `v`, no `shep ` prefix. */
export const workspaceVersion: string = parseWorkspaceVersion(cargoTomlSource);
