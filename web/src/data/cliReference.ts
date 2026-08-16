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
  version: string;
  /** `shep --help`, byte-for-byte, for the page's own top-level block. */
  topLevelHelp: string;
  verbs: CliVerb[];
}

// The verb order the generator script runs in — also the declaration order
// in the Commands enum, and the order `shep --help` itself lists them.
// Kept here (rather than re-derived from the generated file) so a missing
// or misspelled `@@VERB:...@@` marker is a loud parse error, not a silently
// short verb list.
const VERB_NAMES = [
  "start",
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
  const commandsIndex = topLevelHelp.indexOf("\nCommands:\n");
  const optionsIndex = topLevelHelp.indexOf("\nOptions:\n", commandsIndex);
  if (commandsIndex === -1 || optionsIndex === -1) {
    fail("top-level --help has no Commands:/Options: sections to read aliases from.");
  }
  const section = topLevelHelp.slice(commandsIndex, optionsIndex);
  const nameSet = new Set<string>(names);

  // Each entry starts with a line of the shape "  <verb>   <description...>"
  // and may continue onto further indented lines with no verb name. Group
  // continuation lines into the entry above them before checking for an
  // alias suffix, since a wrapped description can put "[alias: x]" on its
  // own trailing line.
  const lines = section.split("\n").slice(1).filter((l) => l.trim().length > 0);
  const entries: string[] = [];
  let currentName = "";
  for (const line of lines) {
    const m = line.match(/^ {2}(\S+)\s+(.*)$/);
    const firstWord = m?.[1];
    if (firstWord && nameSet.has(firstWord)) {
      currentName = firstWord;
      entries.push(`${currentName} ${m![2]}`);
    } else if (currentName) {
      const i = entries.length - 1;
      entries[i] = `${entries[i]} ${line.trim()}`;
    }
  }

  const result = new Map<string, string[]>();
  for (const entry of entries) {
    const spaceIndex = entry.indexOf(" ");
    const entryName = spaceIndex === -1 ? entry : entry.slice(0, spaceIndex);
    const text = spaceIndex === -1 ? "" : entry.slice(spaceIndex + 1);
    result.set(entryName, parseAliases(text));
  }
  return result;
}

function parse(source: string): CliReferenceData {
  const versionIndex = source.indexOf("@@VERSION@@\n");
  const topLevelIndex = source.indexOf("\n@@TOPLEVEL@@\n");
  if (versionIndex === -1 || topLevelIndex === -1) {
    fail("missing @@VERSION@@ or @@TOPLEVEL@@ marker — re-run generate-cli-reference.sh.");
  }
  const version = source.slice(versionIndex + "@@VERSION@@\n".length, topLevelIndex).trim();

  const firstVerbMarker = `\n@@VERB:${VERB_NAMES[0]}@@\n`;
  const firstVerbIndex = source.indexOf(firstVerbMarker);
  if (firstVerbIndex === -1) {
    fail(`missing marker for the first verb "${VERB_NAMES[0]}" — re-run generate-cli-reference.sh.`);
  }
  const topLevelHelp = source
    .slice(topLevelIndex + "\n@@TOPLEVEL@@\n".length, firstVerbIndex)
    .trim();

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

  return { version, topLevelHelp, verbs };
}

export const cliReference: CliReferenceData = parse(generatedSource);
