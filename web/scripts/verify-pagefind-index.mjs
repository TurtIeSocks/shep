#!/usr/bin/env node
// Regression guard for the docs search feature (`DocsSearch.astro`).
//
// `npm run build` runs `astro build && pagefind --site dist` — pagefind is a
// separate CLI step that walks the already-built HTML and writes an index,
// silently, with no non-zero exit code if it finds nothing to index (a typo
// in `data-pagefind-body`, a layout change that drops the attribute, or the
// runtime bundle failing to write would all pass `pagefind`'s own exit code
// and only show up as docs search quietly returning nothing on a live site).
// This is the same class of problem `ReferencePills.astro` and
// `docs/cli.astro` already guard against at build time for other content —
// this is that pattern applied to the one part of the pipeline that runs
// after Astro's own build and isn't covered by `astro check`.
//
// Checked, in order a real regression would most likely trip them:
//   1. The runtime bundle DocsSearch.astro dynamically imports exists.
//   2. `pagefind-entry.json` parses and reports at least one indexed page —
//      this is the number that goes to zero if `data-pagefind-body` stops
//      matching anything.
//   3. That page count is in the right ballpark for this docs set, so a
//      count that's technically non-zero but collapsed (index built from
//      one page instead of sixteen) still fails loudly instead of shipping.

import { readFile, stat } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const distDir = fileURLToPath(new URL("../dist", import.meta.url));
const pagefindDir = `${distDir}/pagefind`;

// The docs set this guards is small and grows slowly — bump this alongside
// any deliberate page count change (see docs/*.md under src/content) rather
// than deleting the check.
const MIN_EXPECTED_PAGES = 10;

function fail(message) {
  console.error(`[verify-pagefind-index] ${message}`);
  process.exit(1);
}

const runtimePath = `${pagefindDir}/pagefind.js`;
const runtimeStat = await stat(runtimePath).catch(() => null);
if (!runtimeStat || runtimeStat.size === 0) {
  fail(
    `${runtimePath} is missing or empty — DocsSearch.astro's dynamic ` +
      `import of "/pagefind/pagefind.js" would 404 on every visitor, and ` +
      `the swallowed-error path would report it as "unavailable in dev" ` +
      `even on the built site. Did the pagefind CLI step run and succeed?`,
  );
}

const entryPath = `${pagefindDir}/pagefind-entry.json`;
const entryRaw = await readFile(entryPath, "utf-8").catch(() => null);
if (!entryRaw) {
  fail(`${entryPath} is missing — the pagefind CLI step did not write an index.`);
}

let entry;
try {
  entry = JSON.parse(entryRaw);
} catch (err) {
  fail(`${entryPath} is not valid JSON: ${err.message}`);
}

const languages = Object.values(entry.languages ?? {});
const pageCount = languages.reduce((sum, lang) => sum + (lang.page_count ?? 0), 0);

if (pageCount === 0) {
  fail(
    `pagefind indexed 0 pages. Every /docs/* page should carry ` +
      `data-pagefind-body (see DocsLayout.astro) — check that it's still ` +
      `there and that pagefind's own build log (above) doesn't already ` +
      `say "Ignoring pages without this tag" for everything.`,
  );
}

if (pageCount < MIN_EXPECTED_PAGES) {
  fail(
    `pagefind indexed only ${pageCount} page(s), fewer than the ` +
      `${MIN_EXPECTED_PAGES} expected for this docs set — that's non-zero ` +
      `but still looks like most pages silently dropped out of the index. ` +
      `If the docs set genuinely shrank, lower MIN_EXPECTED_PAGES here ` +
      `alongside that change.`,
  );
}

console.log(
  `[verify-pagefind-index] OK — ${pageCount} page(s) indexed, runtime bundle present.`,
);
