/*
 * Docs sidebar structure (docs/shep-design/README.md, "Screens > 2. Docs >
 * Sidebar"). This is the shape of the docs shell itself — which pages exist,
 * which route they live at, which group they're under — not a claim about
 * product state, so unlike docsLexicon.ts / docsRules.ts it isn't sourced
 * from a doc that drifts. `built` just means "has a real page" — every item
 * below is one today, but the field stays live rather than getting deleted:
 * a page can still be added to the sidebar (and linked from elsewhere) the
 * day it's planned, before it's written, the same way the fourteen below
 * were. Flip it false for a genuinely unwritten page and the sidebar's
 * "soon" tag picks it back up on its own.
 *
 * `source`/`spec`/`api` back the reference pills under each page's title
 * (see ReferencePills.astro) — one shared component driven by this data so
 * a new page can't ship without at least a Source pill.
 */

/** A docs/specs/shep-v1.md section this page is drawn from. */
export interface SpecRef {
  /** GitHub's own heading-slug algorithm, e.g. "5-configuration" for "## 5. Configuration". */
  anchor: string;
  /** Short label, e.g. "§5 Configuration". */
  label: string;
}

/** A docs.rs type this page is genuinely about — only where shep-core's own API is the subject. */
export interface ApiRef {
  /** Path under docs.rs/shep-core/latest/shep_core/, e.g. "config/struct.Flockfile.html". */
  path: string;
  /** Short label, e.g. "shep_core::config::Flockfile". */
  label: string;
}

export interface DocsNavItem {
  slug: string;
  /** Route is always `/docs/${slug}`. */
  label: string;
  built: boolean;
  /**
   * Repo-relative path this page's material is drawn from — the Source pill.
   *
   * An array where a page's verbs genuinely span more than one module, which
   * renders one Source pill per file. The Logs page is the case that forced
   * it: `bleats` lives in `commands/bleats.rs` and `reopen`/`flush` in
   * `commands/logs.rs`, so either path alone under-represents the page.
   */
  source: string | string[];
  spec?: SpecRef;
  api?: ApiRef;
}

export interface DocsNavGroup {
  label: string;
  items: DocsNavItem[];
}

export const docsNav: DocsNavGroup[] = [
  {
    label: "Start here",
    items: [
      {
        slug: "getting-started",
        label: "Getting started",
        built: true,
        source: "README.md",
      },
      {
        slug: "first-flockfile",
        label: "Your first Flockfile",
        built: true,
        source: "crates/shep-core/src/config/flockfile.rs",
        spec: { anchor: "5-configuration", label: "§5 Configuration" },
        api: {
          path: "config/struct.Flockfile.html",
          label: "shep_core::config::Flockfile",
        },
      },
      {
        slug: "from-pm2",
        label: "Coming from pm2",
        built: true,
        source: "docs/migration.md",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "examples",
        label: "Examples",
        built: true,
        source: "examples/",
        spec: { anchor: "7-readiness--health", label: "§7 Readiness & health" },
      },
    ],
  },
  {
    label: "Concepts",
    items: [
      { slug: "terminology", label: "Terminology", built: true, source: "docs/terminology.md" },
      {
        slug: "folds",
        label: "Folds",
        built: true,
        source: "crates/shep-core/src/config/app.rs",
        spec: { anchor: "5-configuration", label: "§5 Configuration" },
      },
      {
        slug: "shepherd-channel",
        label: "The shepherd channel",
        built: true,
        source: "docs/shepherd-channel.md",
        spec: { anchor: "7-readiness--health", label: "§7 Readiness & health" },
        api: {
          path: "protocol/channel/index.html",
          label: "shep_core::protocol::channel",
        },
      },
      {
        slug: "talking-to-a-sheep",
        label: "Talking to a sheep",
        built: true,
        source: "crates/shep-core/src/signals.rs",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
        api: {
          path: "signals/enum.OperatorSignal.html",
          label: "shep_core::signals::OperatorSignal",
        },
      },
      {
        slug: "lifecycle",
        label: "Stopping and replacing a sheep",
        built: true,
        source: [
          "crates/shep-daemon/src/kill.rs",
          "crates/shep-daemon/src/supervisor.rs",
          "crates/shep-daemon/src/snapshot.rs",
        ],
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "dogs",
        label: "Dogs",
        built: true,
        source: "docs/dogs.md",
        spec: { anchor: "8-dogs-plugins", label: "§8 Dogs" },
      },
      {
        slug: "community-dogs",
        label: "Community dogs",
        built: true,
        source: "docs/dogs.md",
      },
      {
        slug: "kv",
        label: "The KV store",
        built: true,
        source: "docs/kv.md",
        spec: { anchor: "5-configuration", label: "§5 Configuration" },
      },
    ],
  },
  {
    // Alternate ways to reach a running flock, beyond the CLI itself — an
    // AI agent's tool call and an operator's terminal dashboard.
    label: "Interfaces",
    items: [
      {
        slug: "whistle",
        label: "Whistle (MCP)",
        built: true,
        source: "docs/whistle/README.md",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "lookout",
        label: "Lookout",
        built: true,
        source: "docs/lookout/README.md",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
    ],
  },
  {
    // Where the flock runs, once it's not just a laptop anymore.
    label: "Deploying",
    items: [
      {
        slug: "serve",
        label: "Serve",
        built: true,
        source: "crates/shep-cli/src/commands/serve.rs",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "containers",
        label: "Containers",
        built: true,
        source: "crates/shep-cli/src/commands/runtime.rs",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "startup",
        label: "Surviving reboots",
        built: true,
        source: "docs/migration.md",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
    ],
  },
  {
    label: "Reference",
    items: [
      {
        slug: "cli",
        label: "CLI",
        built: true,
        source: "crates/shep-cli/src/cli.rs",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "output",
        label: "Terminal output",
        built: true,
        source: "crates/shep-cli/src/style.rs",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "json-output",
        label: "JSON output",
        built: true,
        source: "crates/shep-cli/src/output/mod.rs",
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
        api: {
          path: "protocol/request/struct.ProcessInfo.html",
          label: "shep_core::protocol::request::ProcessInfo",
        },
      },
      {
        slug: "logs",
        label: "Logs",
        built: true,
        source: [
          "crates/shep-cli/src/commands/bleats.rs",
          "crates/shep-cli/src/commands/logs.rs",
        ],
        spec: { anchor: "9-cli-surface-sheep-native", label: "§9 CLI surface" },
      },
      {
        slug: "not-built",
        label: "What's not built",
        built: true,
        source: "docs/specs/deferred.md",
        spec: { anchor: "2-versioned-scope", label: "§2 Versioned scope" },
      },
    ],
  },
];

/**
 * Whether each pill *kind* has anywhere real to send a reader yet. Both
 * start false: the repo is private (a GitHub link 404s for anyone without
 * access) and no crate has published (docs.rs has nothing to show). The
 * pills still render — with the real, final URL already in their href —
 * dimmed and inert instead of clickable, rather than either shipping a
 * confident-looking link that 404s or hiding the sourcing entirely.
 *
 * Flip one flag the day it stops being true and every pill of that kind
 * goes live with no other code change: the repo going public is one
 * boolean, shep-core's first docs.rs publish is the other.
 */
export const pillTargetsLive = {
  // The repository went public on 2026-08-16, so every Source and Spec pill
  // resolves. docs.rs stays gated until the first `cargo publish`: the crate
  // has no page there yet, and a pill that looks authoritative and 404s is
  // worse than one that says why it is waiting.
  github: true,
  docsRs: false,
};
