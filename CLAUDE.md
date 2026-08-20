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

**~1.3s, 488 of 506 lib tests as of 2026-08-19** — the exact counts drift
every time a task adds one, so treat them as a shape, not a checksum. Three
briefs have now shipped a stale figure, and this file carried "437 of 454"
for long enough to be wrong by fifty. The 18 tests this skips live in a nested `mod slow`
inside each file's `mod tests` — `extras.rs` has 9, `watch/source.rs` 7, and
`watch/mod.rs` and `limits/sample.rs` one each — and wait on real macOS
FSEvents or real elapsed time; they are the reason the unfiltered lib run
costs ~25s instead. A mutation in `supervisor.rs` does not need them — but a
change to `watch/source.rs`'s watcher plumbing, or to timing-sensitive
behavior in `extras.rs` or the sampler, does, so run the unfiltered lib suite
when touching either.

CI runs that tier as its own serial `slow` job and skips it everywhere else,
because a contended runner cannot hold a wall clock still: the debouncer
tests were the whole of CI's red for four runs. `boot.rs`'s
`two_concurrent_boots_on_a_stale_socket_exactly_one_wins` rides along in that
job for the same reason without being in a `mod slow` — it is fast, but it
races two threads and needs the machine quiet. Add a timing- or
contention-sensitive test and it needs the same treatment; the workflow's
skip list names both groups explicitly.

From Phase 15 on, `shep` is a library with three thin `[[bin]]` targets
over it (`shep`, `shep-runtime`, `shep-dev`) rather than one bare binary — the
two container-entrypoint aliases spec §3 asks for cannot share a module tree
without a library underneath them. A **shep-scoped** run therefore needs
both halves: `cargo test -p shep --lib --bins --all-features`. `--bins`
alone now runs almost nothing, since every unit test in the crate lives in the
library.

### The task gate — run once, when the task is otherwise done

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

**If the task changed anything an operator types or sees**, the gate has a
fifth step in `web/` -- see the docs trigger below for what counts and why:

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```
```bash
cd web && npx astro build
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
toolchain; `-p shep` would, and is not in this gate — a macOS host has no
`x86_64-linux-gnu-gcc` for `ring`'s build script to call, so `cargo check -p
shep --target x86_64-unknown-linux-gnu` fails outright here, gcc or no.

**shep carries its own `#[cfg(target_os = "linux")]` code now, and this
gate does not reach it.** Phase 15 added
`crates/shep-cli/tests/init.rs::a_reparented_orphan_is_reaped` and
`reap.rs::drain_reaps_a_real_reparented_orphan`, both Linux-only, in the one
crate this gate deliberately excludes. Local checks give no signal on either
— not this gate (excludes `-p shep` for the reason above), not a bare
macOS `cargo test` (never compiles a `target_os = "linux"` item at all). What
DOES cover them: `.github/workflows/test.yml`'s `test` job, whose
`ubuntu-latest`/`ubuntu-24.04-arm` legs run `cargo test --workspace --locked
--all-features` on real Linux. That workflow runs on every
push and pull request, and has since 2026-08-16, so those two tests DO get
executed on real Linux now. This paragraph previously said the workflow was
`workflow_dispatch`-only "while the repository is private"; both halves were
stale, and the staleness cost real time on 2026-08-19 when a Phase 17 task
was written to "turn CI on" that was already on. The repository is public and
standard runners are free.

Still don't assume the local gate covers Linux: it does not, and three
separate breakages on 2026-08-19 were visible only to CI. `--all-features`
hides a feature-matrix break, a macOS `cargo test` never compiles a
`target_os = "windows"` arm, and the windows-gnu cross-check is `cargo check`,
which does not run anything. Those are not gaps in the gate; they are what
the gate is. **Read the CI result before claiming a branch is green.**

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

Five workspace members, one distributed binary (`shep`): shep-core,
shep-daemon, shep-client, shep-cli (published as `shep`), and
shep-cli-redirect, a placeholder holding the `shep-cli` name on crates.io.
Each crate's Cargo.toml `description` states its role.

**The docs site is `web/`** -- an Astro site, published, and part of the
public surface. See the docs rule below; it is not optional upkeep.

Daemonization = the binary re-execs itself with a hidden `daemon` subcommand.
Module-by-module design: map.md (see above).

## Docs — hard trigger

**The `web/` docs site is published and is part of the public surface. A
change to what an operator can type, see, or configure is not finished until
`web/` says so.** That means a new or removed verb, flag, alias, `shep.toml`
key, Flockfile field, exit code, JSON payload shape, or default value.

Two halves, and only one of them is automatic:

1. **Regenerate the CLI reference.** It is generated from the real binary's
   own `--help`, so it never needs writing by hand:

   ```bash
   cargo build --release
   ./web/scripts/generate-cli-reference.sh
   ```

   `git diff` afterwards is the check. A stale copy does not fail any build,
   which is precisely why it drifts.

2. **Read the prose pages.** `web/src/pages/docs/*.astro` are hand-written
   and no generator touches them. Grep for the thing you changed before
   assuming they are fine.

Then build the site, because it can fail on content the Rust gate never sees:

```bash
cd web && npx astro build
```

**Why this is a hard trigger rather than a nicety.** On 2026-08-19 the
generated reference was two days stale (919 lines of drift), and regenerating
it surfaced a real regression nobody had noticed: the grouped verb listing
that replaced clap's own `Commands:` block had silently dropped every
`[aliases: ...]`, so `shep --help` named none of the six working aliases for
several phases. The same audit found a sample Flockfile in `from-pm2.astro`
carrying a `reuse_port = true` line that had become a parse refusal that
morning -- copy-pasteable, and broken. **Nothing in the Rust gate can catch
either.** `cargo test` does not read `web/`, and `web/` had no mention
anywhere in this file until now.

## Code style — hard trigger

**Invoke the `shep-idiomatic-rust` skill before writing or reviewing ANY Rust
in this repo.** It fronts [docs/idiomatic-rust.md](docs/idiomatic-rust.md) —
45 numbered rules (IR-1..IR-45) distilled from rand 0.10.2. Cite rules as
`IR-<n>` in reviews. Evidence with file:line citations:
[docs/idiomatic-rust/lenses/](docs/idiomatic-rust/lenses/).

Top drift risks (all observed in baseline testing): panicking constructors
outside shep, `std::error::Error` instead of `core::error::Error`, missing
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
CLI, watch/cron/memory-limit restarts, SO_REUSEPORT reload, custom
actions over the shepherd channel (now with a correlation id), the pm2
cutover, the dogs subsystem with working metrics and bark dogs, and an
audit-debt phase. Phase 11 merged too: the six remaining daemon-surface
verbs — `shep stock` (alias `scale`), `shep signal`, `shep whisper` (alias
`sendline`), the KV store's `set`/`get`/`unset`, lambs in `describe`, and
the `channel.*` bus topic. Phase 12a merged: `shep lookout`'s shell and its
flock table pane — dependency, terminal lifecycle, palette, event loop, link
supervision, and a table that subscribes to the bus and polls every two
seconds to repair drift. Phase 12b merged too: the table grows a selected
row, and the three remaining panes go up around it — a host-usage strip, a
sheep detail pane, and a bleats feed. The feed reads the selected sheep's
log files from disk on every refresh rather than subscribing to `log.*`,
deliberately — a busy flock costs one bounded read per pane instead of
making the dashboard the highest-volume subscriber on the bus. Rendered
frames for both phases are in `docs/lookout/frames.txt`. Phase 13 merged:
`shep whistle`, the MCP server over stdio (`rmcp`) — nine tools, five
read-only and always present, four
that mutate and present only when `[whistle] allow_control = true` in
`shep.toml`; `start_sheep` narrowed to already-registered sheep; every
daemon refusal a control tool can meet reaches the model as an in-band tool
result, not a protocol error. `docs/whistle/README.md` and the generated
`docs/whistle/tools.md` are the operator contract. Phase 14 merged: config
and packaging — `.js` Flockfiles behind `shep start --flockfile` (never by
discovery, never by extension alone: `shep start server.js` still starts
`server.js`), a schemars-exported Flockfile JSON Schema
(`crates/shep-core/assets/flockfile.schema.json`, generated from the parser's
own document type, printed by the hidden `shep schema`), a `file < env <
flags` daemon-config layer (`shep daemon --log-json/--log-level/--socket/
--max-cron-sleep`), and openrc plus FreeBSD/OpenBSD `rc.d` renderers for
`shep startup`/`unstartup` — the last two rendered and pinned by
exact-string tests only, never executed on their own operating systems.
Phase 15 merged: the last three v1 verbs — a hand-rolled `shep serve` (no
axum, no tower-http; dotfiles, directory listing, and every in-docroot
symlink all refused by default), `shep runtime` (foreground, no-daemon, PID-1
via a separate init process that reaps orphans and forwards signals), and
`shep dev` (isolated `$SHEP_DEV_HOME`, forced watch, auto-exit) — plus the
`shep` library extraction the two container-entrypoint `[[bin]]` aliases
needed underneath them. Phase 16 merged too: `shep lookout`'s last three
pieces — a name filter that narrows the flock table in place, lambs in the
sheep detail pane (fetched separately with `Request::Describe`, never on the
two-second poll), and the three action keys (`x` stop, `R` restart, `L`
reload) behind the `--allow-control` gate, each arming a confirm rather than
acting on the keypress that pressed it. No wire change.

**After Phase 16** the CLI grew again, so "the v1.0 surface is closed" no
longer holds and this file will not claim it. `feat/pretty-cli` merged the
box-drawn table renderer with adaptive column dropping, colour and a sheep
face in the STATUS column, a `full`/`plain`/`bare` style dial resolved at one
seam, `shep style` persisting to `shep.toml`, and ASCII sheep in three
moments. Then 2026-08-19 added `ProcessInfo::last_exit` and an EXIT column
(a wire change), `shep bleats`' backlog and `--lines`, an opt-in
`[interpreters]` mapping with `--interpreter`, `~/` expansion in every
Flockfile path, a Flockfile app's `cwd` defaulting to its own directory, and
`reuse_port` refused rather than silently ignored. `shep init` is in flight.

**Verb count: 40 visible, not 16.** `./web/scripts/generate-cli-reference.sh`
prints the current number every time it runs, which is the only figure worth
trusting.

What's built vs. deferred to v1.1+: [docs/specs/deferred.md](docs/specs/deferred.md).
Windows is 0%, not partial — every verb prints "not yet supported" and exits.
Project memory (cross-session state) tracks decisions; docs above are the
source of truth.
