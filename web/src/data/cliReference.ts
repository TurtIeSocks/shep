/*
 * CLI reference page data — every verb's usage line, about text and full
 * `--help` output, parsed from a real run of the binary rather than
 * hand-typed. Same shape as web/src/data/docsLexicon.ts (parse a checked-in
 * generated text file into structured rows at Astro build time), and same
 * reason: a hand-written CLI reference drifts from crates/shep-cli/src/cli.rs
 * the first time a flag changes and nobody remembers to update prose too.
 *
 * Source of truth: web/src/data/cli-reference.generated.txt, produced by
 * web/scripts/generate-cli-reference.sh running `shep --help` and
 * `shep <verb> --help` for every verb against target/release/shep. Re-run
 * that script after any change to the verb list, its aliases, or any verb's
 * flags — see the script's own header for the exact command.
 */
// `?raw` (see web/src/data/lexicon.ts's header comment) inlines the file's
// text content at build time.
import generatedSource from "./cli-reference.generated.txt?raw";

export interface CliVerb {
  name: string;
  /** Visible aliases only — a verb with none is `[]`. */
  aliases: string[];
  /** About text, unwrapped into flowing paragraphs, HTML-escaped with `code`/`strong` spans applied. */
  aboutHtml: string[];
  /** e.g. "shep start [OPTIONS] <TARGET>" — the "Usage: " prefix stripped. */
  usage: string;
  /** This verb's own `--help` output, byte-for-byte as clap rendered it. */
  helpText: string;
}

export interface CliReferenceData {
  /** `shep --help`, byte-for-byte, for the page's own top-level block. */
  topLevelHelp: string;
  verbs: CliVerb[];
}

// The verb order the generator script runs in — also the declaration order
// in the Commands enum, and the order `shep --help` itself lists them.
// Kept here (rather than re-derived from the generated file) so a missing
// or misspelled `@@VERB:...@@` marker is a loud parse error, not a silently
// short verb list.
//
// It was silently short by three anyway, from whenever `init`, `style` and
// `welcome` shipped until 2026-09-03. Nothing caught it: this list is what
// the page renders FROM, so a verb absent here is a verb the reference
// simply does not have, and `cli.astro`'s own count check compares the
// groups against this list rather than against the generated file, so the
// two agreed with each other while both disagreed with the binary. The
// generator's `VERBS` array has a Rust test holding it to the real command
// tree (`verbs_the_reference_generator_misses`); this list has nothing, and
// the only thing standing between it and the same drift is the build error
// you get when the groups and this list disagree.
const VERB_NAMES = [
  "start",
  "add",
  "serve",
  "stop",
  "restart",
  "reload",
  "delete",
  "stock",
  "flock",
  "dogs",
  "enable",
  "disable",
  "adopt",
  "rehome",
  "describe",
  "trigger",
  "signal",
  "whisper",
  "fold",
  "bleats",
  "lookout",
  "whistle",
  "reopen",
  "flush",
  "barks",
  "set",
  "get",
  "unset",
  "ping",
  "kill",
  "save",
  "muster",
  "runtime",
  "dev",
  "import",
  "startup",
  "unstartup",
  "completions",
  "init",
  "style",
  "welcome",
] as const;

function fail(message: string): never {
  throw new Error(`web/src/data/cliReference.ts: ${message}`);
}

/** Escapes HTML, then applies `code` and **bold** inline spans. */
function inlineToHtml(text: string): string {
  const escaped = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  return escaped
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/`([^`]+)`/g, "<code>$1</code>");
}

/**
 * clap hard-wraps prose to a fixed column width when stdout isn't a tty.
 * Un-wraps it back into flowing paragraphs: blank lines split paragraphs,
 * single line breaks within a paragraph are just wrap points and get
 * joined with a space.
 */
function unwrapParagraphs(block: string): string[] {
  return block
    .split(/\n\s*\n/)
    .map((para) =>
      para
        .split("\n")
        .map((line) => line.trim())
        .filter(Boolean)
        .join(" "),
    )
    .filter(Boolean);
}

/** Extracts the `[alias: x]` / `[aliases: x, y]` suffix clap prints on a Commands: entry, if any. */
function parseAliases(entryText: string): string[] {
  const match = entryText.match(/\[alias(?:es)?:\s*([^\]]+)]\s*$/);
  if (!match) return [];
  return match[1].split(",").map((s) => s.trim());
}

function parseVerbBlock(name: string, block: string): CliVerb {
  const usageIndex = block.indexOf("\nUsage: ");
  if (usageIndex === -1) {
    fail(`verb "${name}" has no "Usage: " line — cli-reference.generated.txt may be stale or truncated.`);
  }
  const aboutBlock = block.slice(0, usageIndex).trim();
  const rest = block.slice(usageIndex + 1).trim();
  const usageLine = rest.split("\n", 1)[0];
  const usage = usageLine.replace(/^Usage:\s*/, "");

  return {
    name,
    aliases: [],
    aboutHtml: unwrapParagraphs(aboutBlock).map(inlineToHtml),
    usage,
    helpText: block.trim(),
  };
}

function parseAliasesFromTopLevel(topLevelHelp: string, names: readonly string[]): Map<string, string[]> {
  // shep's top-level --help uses a hand-written template with grouped verb
  // lines, not clap's generated `Commands:` block, so there is no per-verb
  // description line carrying `[aliases: ...]` to read. The template names
  // them on a single line instead:
  //
  //   Aliases          flock: list, ls   bleats: logs   stock: scale
  //
  // Pinned on the Rust side by `the_help_template_names_every_visible_alias`,
  // which derives the expected content from clap itself, so this parser and
  // the CLI cannot drift apart without that test failing first.
  const line = topLevelHelp
    .split("\n")
    .find((l) => l.startsWith("Aliases"));
  if (line === undefined) {
    fail("top-level --help has no Aliases line to read aliases from.");
  }

  const nameSet = new Set<string>(names);
  const result = new Map<string, string[]>();

  // `verb: a, b` groups, separated by runs of whitespace. Splitting on the
  // colon rather than on whitespace keeps multi-alias lists together.
  const body = line.replace(/^Aliases\s*/, "");
  const parts = body.split(/\s{2,}/).filter((p) => p.trim().length > 0);
  for (const part of parts) {
    const colon = part.indexOf(":");
    if (colon === -1) {
      continue;
    }
    const verb = part.slice(0, colon).trim();
    if (!nameSet.has(verb)) {
      fail(`top-level --help names aliases for "${verb}", which is not a known verb.`);
    }
    const aliases = part
      .slice(colon + 1)
      .split(",")
      .map((a) => a.trim())
      .filter((a) => a.length > 0);
    result.set(verb, aliases);
  }

  if (result.size === 0) {
    fail("top-level --help has an Aliases line but no verb: alias entries in it.");
  }
  return result;
}

// The generator emits nothing ahead of it, so this marker opens the file
// rather than being something to search for. There is deliberately no
// version section any more — see the generator's own comment for why, and
// web/src/data/workspaceVersion.ts for where the page reads the version now.
const TOP_LEVEL_MARKER = "@@TOPLEVEL@@\n";

function parse(source: string): CliReferenceData {
  if (!source.startsWith(TOP_LEVEL_MARKER)) {
    fail("does not open with the @@TOPLEVEL@@ marker — re-run generate-cli-reference.sh.");
  }

  const firstVerbMarker = `\n@@VERB:${VERB_NAMES[0]}@@\n`;
  const firstVerbIndex = source.indexOf(firstVerbMarker);
  if (firstVerbIndex === -1) {
    fail(`missing marker for the first verb "${VERB_NAMES[0]}" — re-run generate-cli-reference.sh.`);
  }
  const topLevelHelp = source.slice(TOP_LEVEL_MARKER.length, firstVerbIndex).trim();

  const aliasesByVerb = parseAliasesFromTopLevel(topLevelHelp, VERB_NAMES);

  const verbs: CliVerb[] = VERB_NAMES.map((name, i) => {
    const marker = `\n@@VERB:${name}@@\n`;
    const start = source.indexOf(marker);
    if (start === -1) {
      fail(`missing marker for verb "${name}" — re-run generate-cli-reference.sh.`);
    }
    const blockStart = start + marker.length;
    const nextName = VERB_NAMES[i + 1];
    const end = nextName ? source.indexOf(`\n@@VERB:${nextName}@@\n`) : source.length;
    const block = source.slice(blockStart, end === -1 ? source.length : end);
    const verb = parseVerbBlock(name, block);
    verb.aliases = aliasesByVerb.get(name) ?? [];
    return verb;
  });

  return { topLevelHelp, verbs };
}

export const cliReference: CliReferenceData = parse(generatedSource);
