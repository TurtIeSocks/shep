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
      { slug: "kv", label: "The KV store", built: false },
    ],
  },
  {
    // Alternate ways to reach a running flock, beyond the CLI itself — an
    // AI agent's tool call and an operator's terminal dashboard.
    label: "Interfaces",
    items: [
      { slug: "whistle", label: "Whistle (MCP)", built: false },
      { slug: "lookout", label: "Lookout", built: false },
    ],
  },
  {
    // Where the flock runs, once it's not just a laptop anymore.
    label: "Deploying",
    items: [
      { slug: "serve", label: "Serve", built: false },
      { slug: "containers", label: "Containers", built: false },
      { slug: "startup", label: "Surviving reboots", built: false },
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
 * Copy for every stub page — the original eight plus the six added when the
 * site was brought up to date with everything shep can do (whistle, lookout,
 * kv, serve, containers, startup). Blurbs are short and mostly evergreen
 * (what the page will cover, not shep's current build state) — the one
 * exception, "not-built", is checked against docs/specs/deferred.md's own
 * scope-cut and build-queue sections as of 2026-08-16; re-check it whenever
 * that file changes.
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
  kv: {
    crumb: "Concepts / The KV store",
    title: "The KV store",
    blurb:
      "Three verbs — set, get, unset — for the small stuff that has nowhere else to live: a file-locked JSON store, capped at 4 KiB a value, safe to keep in a dotfiles repo.",
    source: "docs/kv.md",
  },
  whistle: {
    crumb: "Interfaces / Whistle",
    title: "Whistle",
    blurb:
      "An MCP server over stdio. It hands an AI agent the same flock a person reaches with shep flock — five read-only tools always on, four control tools gated behind a config flag that defaults to off.",
    source: "docs/whistle/README.md",
  },
  lookout: {
    crumb: "Interfaces / Lookout",
    title: "Lookout",
    blurb:
      "A terminal dashboard over the shepherd: the flock table, a host-usage strip, and — once you select a row — that sheep's detail pane and its bleats feed.",
    source: "docs/lookout/README.md",
  },
  serve: {
    crumb: "Deploying / Serve",
    title: "Serve",
    blurb:
      "A static file server, hand-rolled rather than framework-built, run as a managed sheep. Directory listing, dotfiles, and following symlinks are all off until you ask for them.",
    source: "docs/specs/shep-v1.md",
  },
  containers: {
    crumb: "Deploying / Containers",
    title: "Containers",
    blurb:
      "shep runtime is the PID-1 entrypoint for a container: it reaps zombies, forwards signals, and exits when the flock empties. shep dev is the same idea for a laptop — an isolated $SHEP_HOME with watch forced on.",
    source: "docs/specs/shep-v1.md",
  },
  startup: {
    crumb: "Deploying / Surviving reboots",
    title: "Surviving reboots",
    blurb:
      "shep startup installs the unit that brings the shepherd, and the flock it last saved, back after a reboot — systemd, launchd, openrc, or BSD rc.d. shep unstartup takes it back out.",
    source: "docs/migration.md",
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
      "Windows, entirely — no functional tier exists yet. Past that the list is short: OTLP export for the metrics dog, lookout's search/filter and everything behind its action gate but stop, lambs in lookout's detail pane, HTTP/SSE transport for whistle, cgroup v2 enforcement, and the @shep/io npm shim.",
    source: "docs/specs/deferred.md",
  },
};
