/*
 * Docs sidebar structure (docs/shep-design/README.md, "Screens > 2. Docs >
 * Sidebar"). This is the shape of the docs shell itself — which pages exist,
 * which route they live at, which group they're under — not a claim about
 * product state, so unlike docsLexicon.ts / docsRules.ts it isn't sourced
 * from a doc that drifts. `built` just means "has a real page", i.e. isn't
 * rendered with StubPanel; flip it here the day a stub gets written.
 */

export interface DocsNavItem {
  slug: string;
  /** Route is always `/docs/${slug}`. */
  label: string;
  built: boolean;
}

export interface DocsNavGroup {
  label: string;
  items: DocsNavItem[];
}

export const docsNav: DocsNavGroup[] = [
  {
    label: "Start here",
    items: [
      { slug: "getting-started", label: "Getting started", built: true },
      { slug: "first-flockfile", label: "Your first Flockfile", built: false },
      { slug: "from-pm2", label: "Coming from pm2", built: false },
    ],
  },
  {
    label: "Concepts",
    items: [
      { slug: "terminology", label: "Terminology", built: true },
      { slug: "folds", label: "Folds", built: false },
      { slug: "shepherd-channel", label: "The shepherd channel", built: false },
      { slug: "dogs", label: "Dogs", built: false },
    ],
  },
  {
    label: "Reference",
    items: [
      { slug: "cli", label: "CLI", built: false },
      { slug: "json-output", label: "JSON output", built: false },
      { slug: "not-built", label: "What's not built", built: false },
    ],
  },
];

export interface StubMeta {
  crumb: string;
  title: string;
  blurb: string;
  /** Repo-relative path shown in the stub panel and linked on GitHub. */
  source: string;
}

/**
 * Copy for the eight stub pages. Blurbs are short and mostly evergreen
 * (what the page will cover, not shep's current build state) — the one
 * exception, "not-built", is checked against README.md's own "What's not
 * built yet" section as of 2026-08-15; re-check it whenever that section
 * changes.
 */
export const stubMeta: Record<string, StubMeta> = {
  "first-flockfile": {
    crumb: "Start here / Your first Flockfile",
    title: "Your first Flockfile",
    blurb:
      "Every field a Flockfile understands, the ten filenames config discovery searches, and the strict grammar for durations and sizes.",
    source: "docs/specs/shep-v1.md",
  },
  "from-pm2": {
    crumb: "Start here / Coming from pm2",
    title: "Coming from pm2",
    blurb:
      "shep import reads a real dump.pm2 and writes a Flockfile. It starts nothing, and names on stderr everything that could not survive the trip unchanged.",
    source: "docs/migration.md",
  },
  folds: {
    crumb: "Concepts / Folds",
    title: "Folds",
    blurb:
      "A fold is a namespace: shep fold backend lists one, and fold = in a Flockfile puts a sheep in it.",
    source: "docs/specs/shep-v1.md",
  },
  "shepherd-channel": {
    crumb: "Concepts / The shepherd channel",
    title: "The shepherd channel",
    blurb:
      "Set channel = true and your app gets a private pipe on fd 3, speaking newline JSON. shep trigger sends a named action down it and prints what each instance answered.",
    source: "docs/shepherd-channel.md",
  },
  dogs: {
    crumb: "Concepts / Dogs",
    title: "Dogs",
    blurb:
      "A dog is a plugin the shepherd supervises for its own sake: it watches the flock rather than being part of it. metrics and bark ship inside the binary; adopt runs anyone else's.",
    source: "docs/dogs.md",
  },
  cli: {
    crumb: "Reference / CLI",
    title: "CLI reference",
    blurb:
      "Every verb, its aliases, its flags, and its exit codes — generated from the same clap tree the binary parses with.",
    source: "crates/shep-cli/src/cli.rs",
  },
  "json-output": {
    crumb: "Reference / JSON output",
    title: "JSON output",
    blurb:
      "The versioned envelope every command answers in under --format json, and what a schema_version bump does and does not promise.",
    source: "docs/specs/shep-v1.md",
  },
  "not-built": {
    crumb: "Reference / What's not built",
    title: "What's not built yet",
    blurb:
      "The whistle MCP server, shep serve, shep dev and shep runtime, .js Flockfiles, a CLI-flag config layer, openrc and BSD rc.d units, and Windows entirely — the full list, including what's held back past 1.0 on purpose.",
    source: "docs/specs/deferred.md",
  },
};
