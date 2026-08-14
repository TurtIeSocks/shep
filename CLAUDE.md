# shep — CLAUDE.md

Clean-room Rust process manager (daemon + CLI + client lib), inspired by pm2's
*feature list only*. License: MIT OR Apache-2.0. Sheep/sheepdog branding
throughout. Published at `github.com/TurtIeSocks/shep`; the local checkout
directory is still named `pm2-rs`, which is expected and not a rename to make.

## ⚠️ Clean-room rule (non-negotiable)

**Never open, read, or port source from `/Users/rin/GitHub/pm2` during
implementation.** That repo was read once, by a dedicated trace phase, to
produce our behavior specs — implementation works from the specs alone:

- [docs/systematic-refactor/refactor-workspace/map.md](docs/systematic-refactor/refactor-workspace/map.md) — THE spec: every module's behavior, actions, notes
- [docs/systematic-refactor/refactor-workspace/](docs/systematic-refactor/refactor-workspace/) — goals.md (must-haves, constraints, open questions), assessment.md (keep/toss verdicts), trace.md + trace/ (flow inventories, known-bug list — bugs are documented so we do NOT reproduce them)

"Compat"/"contract" language in those docs means fidelity to the spec, not to
pm2's artifacts. `/Users/rin/GitHub/rand` is the style reference — read freely.

## Commands

MSRV 1.88, edition 2024. The build cache works — a no-op rebuild is **0.35s**.
Slow runs are never compilation; they are test execution, and almost all of it
is one class of test.

### The inner loop — use this while iterating, including for every mutation

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

**~1.3s, 437 of 454 lib tests** — the exact counts drift every time a task
adds one, so treat them as a shape, not a checksum; two briefs have now
shipped a stale figure. The 17 tests this skips live in a nested `mod slow`
inside each file's `mod tests` (in `watch/source.rs`, `watch/mod.rs`, and
`extras.rs`) and wait on real macOS FSEvents or real elapsed time; they are
the reason the unfiltered lib run costs ~25s instead. A mutation in
`supervisor.rs` does not need them — but a change to `watch/source.rs`'s
watcher plumbing, or to timing-sensitive behavior in `extras.rs` or the
sampler, does, so run the unfiltered lib suite when touching either.

### The task gate — run once, when the task is otherwise done

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Each from its own command with `$?` captured directly, never through a pipe —
in zsh a pipeline's `$?` is the last command's and `${PIPESTATUS[0]}` is empty.
**One cargo command at a time**: the workspace shares one target-dir build
lock, so concurrent runs block rather than parallelise. (A separate worktree,
or `benches/`, has its own lock and may run alongside.)

### The two cross-checks — run once per phase, not per task

```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

One cargo command at a time, as everywhere else, and give them their own
`CARGO_TARGET_DIR` if you want the host cache left alone.

**Linux.** `notify.rs`'s abstract-namespace branch and its test are both
`#[cfg(target_os = "linux")]`, so a macOS `cargo test` compiles neither. That
branch is what a systemd `Type=notify` unit — the unit `shep startup` installs
— depends on for readiness reporting, and it went five phases without a
compiler ever reading it (platform audit #3). `--all-targets` is what reaches
the test. shep-daemon has no `ring` in its tree, so this needs no cross C
toolchain; `-p shep-cli` would, and is not in this gate.

**Windows.** Every plan through Phase 6 carried this one; Phases 7-9 dropped
it without saying so, and it never reached this file, which is why nothing
noticed for three phases. Restored in Phase 10 after being measured green
(`EXIT=0`, 8.42s, 2026-08-13). It needs a C toolchain for the target —
`brew install mingw-w64` — because `ring`'s build script runs `cc`; a host
without `x86_64-w64-mingw32-gcc` cannot run it, and that is presumably how it
came to be dropped.

`cargo check`, deliberately, not `clippy -- -D warnings`: shep-daemon's
`boot`/`sys`/`server`/`tokio_runner` are `cfg(unix)`-gated, so on Windows 51
dead-code warnings fall out of code that is not dead anywhere we ship. The
question this gate asks is whether the tree still compiles for a target nobody
has implemented yet. Silencing those warnings would mean `#[allow(dead_code)]`
on live code.

### Doctests are not the cost here — do not split them out

Measured 2026-08-12 on this machine: bare `cargo test --workspace
--all-features` **89.3s**; `--all-targets` (same minus doctests) **82.7s**;
the three crates' doctests run alone **30.9s**. They overlap rather than add,
so splitting them out of the task gate buys ~6.5s and costs a second command.

The global rule to prefer `--lib --bins` over bare `--workspace` was measured
on a project where doctests dominated. It does not transfer: this workspace's
cost is the integration tier (`cli_e2e` ~47s, `daemon_e2e` ~22s), which
`--lib --bins` would skip entirely rather than speed up. Keep the bare form.

### The phase gate — run at a merge, not per task

The four above, plus `cargo test --workspace --all-features -- --test-threads=1`
and both `benches/` gates. The serial run is not ceremony: it was red on `main`
before Phase 5 and it caught a real regression in Phase 6.

### Measuring a mutation's blast radius

Use the inner loop. Escalate to `cargo test --workspace --all-features
--no-fail-fast` **only if the targeted run shows a radius above 1**, or if the
change crosses a crate boundary. Without `--no-fail-fast` cargo stops at the
first failing binary and a radius of 3 reads as 1.

Bounded waits on real children produce **false radii under load** — an earlier
task saw 9 failures that were all load artefacts. Confirm any radius above 1 by
re-running that suite in isolation with the mutation still applied.

## Subagent dispatch

- **Writing plans:** Opus, extra thinking. Plans carry the design work; a thin
  plan spends its cost later, in review loops.
- **Implementing a written plan:** Sonnet, high thinking. The design decisions
  are already made.

## Architecture

Four crates, one distributed binary (`shep`): shep-core, shep-daemon,
shep-client, shep-cli — each crate's Cargo.toml `description` states its role.

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
**dogs** (plugin processes — metrics, bark — supervised by the daemon; the
daemon itself is only ever "the shepherd"), **lambs** (child processes of a
sheep — process-tree members). `sheep` = ONE managed user process (singular
only); the plural is always **flock**, never bare "sheep"/"sheeps". Rules: straight verbs
(`start`/`stop`/`list`) stay
first-class aliases; destructive ops and error text stay plain — the theme
never costs clarity.

## Gotchas

- Every new public item needs docs and a deliberate Debug decision (redacted
  for anything carrying env/secrets, with an exact-string test — IR-41).
- `#![forbid(unsafe_code)]` planned for core/client/cli; unsafe only in
  `shep-daemon/src/sys.rs` with per-block `// SAFETY:` (IR-22/23).
- Open design decisions live at the bottom of map.md and in goals.md's open
  questions — check them before making architectural calls; if a decision is
  listed there, it is Rin's, not yours.

## Status / workflow

Phases 1–10 merged: shep-core, the daemon supervision engine, log plane, the
16-verb CLI, watch/cron/memory-limit restarts, SO_REUSEPORT reload, custom
actions over the shepherd channel (now with a correlation id), the pm2
cutover, the dogs subsystem with working metrics and bark dogs, and an
audit-debt phase. Phase 11 merged too: the six remaining daemon-surface
verbs — `shep stock` (alias `scale`), `shep signal`, `shep whisper` (alias
`sendline`), the KV store's `set`/`get`/`unset`, lambs in `describe`, and
the `channel.*` bus topic.

What's built vs. deferred to v1.1+: [docs/specs/deferred.md](docs/specs/deferred.md).
Windows is 0%, not partial — every verb prints "not yet supported" and exits.
Project memory (cross-session state) tracks decisions; docs above are the
source of truth.
