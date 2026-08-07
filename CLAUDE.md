# shep — CLAUDE.md

Clean-room Rust process manager (daemon + CLI + client lib), inspired by pm2's
*feature list only*. License: MIT OR Apache-2.0. Sheep/sheepdog branding
throughout. Repo dir is still `pm2-rs` pending GitHub rename.

## ⚠️ Clean-room rule (non-negotiable)

**Never open, read, or port source from `/Users/rin/GitHub/pm2` during
implementation.** That repo was read once, by a dedicated trace phase, to
produce our behavior specs — implementation works from the specs alone:

- [docs/systematic-refactor/refactor-workspace/map.md](docs/systematic-refactor/refactor-workspace/map.md) — THE spec: every module's behavior, actions, notes
- [docs/systematic-refactor/refactor-workspace/](docs/systematic-refactor/refactor-workspace/) — goals.md (must-haves, constraints, open questions), assessment.md (keep/toss verdicts), trace.md + trace/ (flow inventories, known-bug list — bugs are documented so we do NOT reproduce them)

"Compat"/"contract" language in those docs means fidelity to the spec, not to
pm2's artifacts. `/Users/rin/GitHub/rand` is the style reference — read freely.

## Commands

```bash
cargo check --workspace          # fast gate
cargo clippy --workspace -- -D warnings
cargo fmt --all --check
cargo test --workspace
```

All four must be green before any task is called done. MSRV 1.85, edition 2024.

## Architecture

Four crates, one distributed binary (`shep`):

| Crate | Role |
|---|---|
| `crates/shep-core` | shared types, config, paths, wire protocol — depends on no sibling |
| `crates/shep-daemon` | supervisor lib (spawn/kill/reload/watch/metrics/alerts) — no bin; embedded in CLI |
| `crates/shep-client` | async RPC client + programmatic API; re-exports shep-core |
| `crates/shep-cli` | the `shep` binary: clap surface, TUI, serve, MCP, runtime/dev modes |

Daemonization = the binary re-execs itself with a hidden `daemon` subcommand.
Module-by-module design: map.md (see above).

## Code style — hard trigger

**Invoke the `shep-idiomatic-rust` skill before writing or reviewing ANY Rust
in this repo.** It fronts [docs/idiomatic-rust.md](docs/idiomatic-rust.md) —
45 numbered rules (IR-1..IR-45) distilled from rand 0.10.2. Cite rules as
`IR-<n>` in reviews. Evidence with file:line citations:
[docs/idiomatic-rust/lenses/](docs/idiomatic-rust/lenses/).

Top drift risks (all observed in baseline testing): panicking constructors
outside shep-cli, `std::error::Error` instead of `core::error::Error`, missing
`# Errors` doc sections, `# Panics` without `#[track_caller]`, widening input
grammars beyond spec.

## Terminology

[docs/terminology.md](docs/terminology.md) is the lexicon: flock, fold,
Flockfile, bleats, bark (webhooks), whistle (MCP), muster, lookout (TUI),
**lambs** (first-party plugin processes — metrics, bark — supervised by the
daemon itself). `sheep` = ONE managed user process (singular only); the plural
is always **flock**, never bare "sheep"/"sheeps". Rules: straight verbs
(`start`/`stop`/`list`) stay
first-class aliases; destructive ops and error text stay plain — the theme
never costs clarity.

## Gotchas

- Workspace lints deny `missing_docs` and `missing_debug_implementations` —
  every new public item needs docs and a deliberate Debug decision (redacted
  for anything carrying env/secrets, with an exact-string test — IR-41).
- `#![forbid(unsafe_code)]` planned for core/client/cli; unsafe only in
  `shep-daemon/src/sys.rs` with per-block `// SAFETY:` (IR-22/23).
- Open design decisions live at the bottom of map.md and in goals.md's open
  questions — check them before making architectural calls; if a decision is
  listed there, it is Rin's, not yours.

## Status / workflow

Scaffold phase: crates compile, no implementation yet. Next phase =
brainstorming → spec → plan off map.md. Project memory (cross-session state)
tracks decisions; docs above are the source of truth.
