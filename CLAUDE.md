# shep — CLAUDE.md

Clean-room Rust process manager (daemon + CLI + client lib), inspired by pm2's
*feature list only*. License: MIT OR Apache-2.0. Sheep/sheepdog branding
throughout. Published at `github.com/shep-pm/shep`; the local checkout
directory is still named `pm2-rs`, which is expected and not a rename to make.

## ⚠️ Clean-room rule (non-negotiable)

**Never open, read, or port source from `~/GitHub/pm2` during
implementation.** That repo was read once, by a dedicated trace phase, to
produce our behavior specs — implementation works from the specs alone:

- [docs/systematic-refactor/refactor-workspace/map.md](docs/systematic-refactor/refactor-workspace/map.md) — the spec for the pm2-DERIVED module set, and only that. It is
  accurate and drift-annotated through roughly Phase 10, with a partial Phase
  15 pass, and it stops there. It has no mention of lookout, whistle, `shep
  stock`, `shep signal` or shep-cli-redirect, and it still calls the TUI
  `tui.rs` and the MCP server `mcp.rs`, which are the names those two shipped
  under before Phases 12 and 13 renamed them. For anything after the pm2
  cutover, the design lives in docs/brainstorming/specs/ and the reasoning in
  [docs/decisions.md](docs/decisions.md). This line said "THE spec: every module's behavior" until an audit
  on 2026-08-29 counted the gaps, which is a bad claim to leave in the file
  every session reads first.
- [docs/systematic-refactor/refactor-workspace/](docs/systematic-refactor/refactor-workspace/) — goals.md (must-haves, constraints, open questions), assessment.md (keep/toss verdicts), trace.md + trace/ (flow inventories, known-bug list — bugs are documented so we do NOT reproduce them)

"Compat"/"contract" language in those docs means fidelity to the spec, not to
pm2's artifacts. `~/GitHub/rand` is the style reference — read freely.

## Commands

MSRV 1.88, edition 2024. The build cache works — a no-op rebuild is **0.35s**.
Slow runs are never compilation; they are test execution, and almost all of it
is one class of test.

### The inner loop — use this while iterating, including for every mutation

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

**~3.2s, 766 of 785 lib tests as of 2026-09-03**. The exact counts drift
every time a task adds one, so treat them as a shape, not a checksum. Three
briefs have now shipped a stale figure, this file carried "437 of 454"
for long enough to be wrong by fifty, then "619 of 638" while the real number
climbed by ninety-two, then "711 of 730" while the config-overrides branch
added fifty-five. The 19 tests this skips live in a nested `mod slow`
inside each file's `mod tests` — `extras.rs` has 9, `watch/source.rs` 7, and
`watch/mod.rs`, `limits/sample.rs` and `handover/mod.rs` one each — and wait on real macOS
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

**The skip list is not the first answer to a CI-only failure, and twice on
2026-09-04 it was the wrong one.**
`a_reopen_that_cannot_open_a_path_again_exits_internal` renamed a log file
after `poll_flock` said `online`, which means the daemon spawned the child
and NOT that the pump has opened the file, so the rename failed `ENOENT`;
the sibling ninety lines above it
already waits for the first line through `bleats` and calls that wait a
precondition. `a_flock_of_every_carried_kind_survives_a_daemon_reload` was
not a test problem at all: the log pump dropped a line the child wrote just
before its sheep task let go, 39 times in 64 at the seam, and quarantining
it would have hidden that. Both stay in the ordinary tier. Before reaching
for the skip list, check the failure against the `slow` tier's own
criterion, which the workflow states: a test belongs there when it asserts a
duration, a batch or a count that a contended runner cannot hold still.
"Waits twenty seconds for something that should take milliseconds" is not
that, and an EMPTY artifact where a partial one was expected is a defect
rather than a slow machine. See `docs/decisions.md`, "CI flakes, and the log
line a stop could lose".

From Phase 15 on, `shep` is a library with three thin `[[bin]]` targets
over it (`shep`, `shep-runtime`, `shep-dev`) rather than one bare binary — the
two container-entrypoint aliases spec §3 asks for cannot share a module tree
without a library underneath them. A **shep-scoped** run therefore needs
both halves: `cargo test -p shep --lib --bins --all-features`. `--bins`
alone now runs almost nothing, since every unit test in the crate lives in the
library.

`shep` has a `mod slow` of its own as of 2026-08-28, seven tests as of
2026-08-31, in `commands/lifecycle.rs`. It needs a real node to start and exit inside a
budget, which is a claim about the machine's speed rather than about shep: at
200ms it failed on four CI runners at once while passing every local run. Add
`-- --skip ::slow::` to a shep-scoped run for the same reason the daemon one
carries it. CI already covers it: the `slow` job runs `--workspace`, chosen so
a `mod slow` outside shep-daemon could not end up skipped everywhere and run
nowhere.

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

**This holds for the LOCAL gate. CI splits them, because nextest cannot run
them at all.** As of 2026-09-04 every CI leg that used to run `cargo test`
runs `cargo nextest run` plus a separate `cargo test --doc`, so a flaky
integration test can be retried without retrying anything else. That split is
forced by the tool rather than chosen, and it is cheap: measured the same day
on this machine, warm, doctests alone are **6.8s**, not the 30.9s above, which
was a colder tree. `cargo nextest run` over the same skip set is **26.1s**
against `cargo test`'s **45.2s**, so a leg is faster even paying for the
second command.

Two numbers to keep straight when a count looks wrong. `cargo test --workspace
--all-features` over the skip set reports **2525**; nextest reports **2511**,
and the missing **14** are exactly the doctests it does not run. The two
filtersets in the workflow partition the suite with nothing dropped: 2511 in
the ordinary legs plus 30 in `slow` is 2541, which is every non-doctest test.
Nothing here changes the local gate: keep running bare `cargo test`.

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
- **Every brief says to use conventional commit subjects, and says it in the
  brief rather than trusting it to be known.** `type(scope): summary`, with a
  `!` on the commit that actually breaks something, in the crate that breaks.

  This is a release-correctness rule, not a style one. release-plz walks the
  INDIVIDUAL commits and `filter_unconventional = true` drops whatever does
  not parse, so an unreadable subject contributes nothing to its crate's
  changelog and nothing to the version bump. The `!` on a pull request title
  is read by nobody: release-plz ignores merge commits, which is the opposite
  of what a `merge_commit_title = PR_TITLE` setting suggests.

  Measured 2026-09-04, and it is the reason this bullet exists. Of the 31
  commits behind `shep-core` 0.2.1, 19 were unreadable, and the split was
  exact: every readable one was written in the main thread, every unreadable
  one by an implementer subagent. One of the 19 changed `BusEvent::topic`'s
  signature, so a source break went to crates.io as a patch with an empty
  changelog section. Nothing failed. `semver_check = true` did not catch it
  either, having no lint for a changed inherent-method return type.

  Nine types are accepted, and they are what `release-plz-changelog.toml`
  handles rather than the conventional-commits list. `feat`, `fix`, `perf`
  and `refactor` produce entries; `docs`, `test`, `ci`, `chore` and `style`
  are `skip = true` and drop when not breaking, except
  `chore: update Cargo.{toml,lock} dependencies`, which has its own parser
  ahead of that skip. `revert` and `build` are refused, because they match no
  parser and `filter_commits = true` discards them as silently as it discards
  a sentence.

  **A `!` is the second half of the rule, never a shortcut past the first.**
  `protect_breaking_commits = true` outranks a `skip = true`, so `docs!:` and
  `chore!:` are kept and arrive marked BREAKING. It does not outrank a miss,
  so `revert!:` and `build!:` vanish, and neither does it rescue a subject
  that never parsed: `Tell a running dog its config changed!` produces
  nothing, measured. The commit that changed `BusEvent::topic` needed to be
  `refactor(core)!:`, and no shorter fix would have saved 0.2.1. A conventional
  type gets the commit seen; the `!` then decides the bump. All measured with
  git-cliff 2.14.1 against the real config.

  `.github/workflows/commits.yml` gates it now and `.githooks/commit-msg`
  catches it earlier, so this bullet is the explanation rather than the
  enforcement. It still belongs here, because a brief that omits the rule
  produces a branch that fails CI at the end instead of a subagent that gets
  it right at the start.

## Architecture

Seven published workspace members, one distributed binary (`shep`):
shep-core, shep-daemon, shep-client, shep-macros (the `DogConfig` derive),
shep-cli (published as `shep`), shep-channel (the client an app links to
speak the shepherd channel), and shep-cli-redirect, a placeholder holding
the `shep-cli` name on crates.io. This line said "five" on main and "six"
on the branch that added shep-channel, and both were wrong once the two
met: neither counted the other's crate.
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
```bash
cd web && npx astro check
```

**Both, and `check` is the one that catches a wrong prop.** Astro does not
typecheck during a build, so a page passing a component a prop it does not
have builds clean and renders wrong. Measured 2026-08-20: `/docs/output`
shipped two `<Callout kind="note">` against a component whose prop is
`variant`, so `variant` was `undefined`, the rendered `div` lost its variant
class and the label badge rendered empty. `astro build` was green the whole
time. `astro check` reported both, at `ts(2322)`, the moment it was run.

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
46 numbered rules (IR-1..IR-46) distilled from rand 0.10.2. Cite rules as
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
- `#![forbid(unsafe_code)]` is LIVE in core/client/cli, not planned. Unsafe
  lives in three files across two crates, each carrying its own
  `#![allow(unsafe_code)]` or `#[allow(unsafe_code)]` with per-block
  `// SAFETY:` (IR-22/23): shep-daemon's `sys.rs` (eight sites on unix) and
  `sys_windows.rs` (ten on Windows), and shep-channel's `endpoint.rs` (two
  sites, one per platform: taking the descriptor the shepherd names in
  `SHEP_CHANNEL_FD`, sound because a process-global guard makes it reachable
  at most once per process, and `PeekNamedPipe` on Windows, which
  `PipeReader`'s own doc comment exists to justify). This line said
  "planned" and named only `sys.rs` for the whole of the Windows port, then
  said "exactly two files" after shep-channel added a third, then said "one
  site" in `endpoint.rs` after it had grown a second.
- Open design decisions live at the bottom of map.md and in goals.md's open
  questions — check them before making architectural calls; if a decision is
  listed there, it is the maintainer's, not yours.

## Status / workflow

Phases 1–10 merged: shep-core, the daemon supervision engine, log plane, the
CLI, watch/cron/memory-limit restarts, overlapping reload, custom
actions over the shepherd channel (now with a correlation id), the pm2
cutover, the dogs subsystem with working metrics and bark dogs, and an
audit-debt phase.

**That reload is NOT "SO_REUSEPORT reload", which this line said until
2026-08-28.** shep never binds an app's listening socket and never sets
`SO_REUSEPORT` on one. The only socket it binds is its own control socket at
`$SHEP_HOME/run/shep.sock`, which is a different thing entirely and does fail
loudly when its path exceeds the platform limit. Whether a reload's overlap is
zero-downtime depends on the app having set `SO_REUSEPORT` on its own
listener; without it the second instance takes `EADDRINUSE`.

**The overlap stopped being unconditional later the same day**, and this file
carried the old claim for a few hours, which is worse than it sounds because
this is the file every session reads first. `reuse_port` is no longer refused
at parse time: it is the field that decides which of two reloads an app gets.
An app with a `readiness_probe` and no `reuse_port` is reloaded SERIALLY
(DrainOld, ReapOld, SpawnNew, AwaitReady), because a probe asks an address and
an address cannot say which of two overlapping instances answered it. Anything
else still overlaps (SpawnNew, AwaitReady, DrainOld, ReapOld): no probe,
`wait_ready`, or `reuse_port = true`. See `ReloadMode` in `supervisor.rs`, and
`docs/specs/deferred.md` for the three residuals that fix does not cover.
A reload also PROMOTES config a Flockfile load parked (see the config-override
paragraph below), which is new as of 2026-09-03 and is the one thing that
makes `shep reload <sheep>` about config at all. Separately, `shep daemon
reload` now validates `shep.toml` BEFORE it touches the predecessor: the
handover arm execs a successor that re-reads that file, and a value that
fails to load there used to exit the successor with the predecessor already
gone, leaving the flock running with nothing supervising it.
Phase 11 merged too: the six remaining daemon-surface
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
`reuse_port` refused rather than silently ignored (which it no longer is; see
the reload paragraph above). `shep init` shipped: it is in the CLI's `VERBS` array, has its own
module at `crates/shep-cli/src/commands/init.rs`, and is documented on the
Flockfile page.

**Phase 3b merged on 2026-08-31, and it is the one to read if a dog looks
healthy and is not.** A dog that has never completed a handshake used to
report `online` in `shep flock` and in `shep lookout`, with zero restarts,
while retrying a handshake that could never succeed. `ProcessInfo` gained
`handshook: Option<bool>` (additive, so neither `PROTOCOL_VERSION` nor
`SCHEMA_VERSION` moved) and such a dog now reads `silent`. The daemon grew
`DOG_SILENCE_BUDGET`, five seconds, so G8's one-restart ladder reaches a dog
that cannot name itself: before that, the ladder was keyed on
`Hello::dog_name`, which a client on an older protocol cannot send, so the
dogs most likely to need the ladder were the ones structurally unable to
reach it. `shep daemon reload`'s unsettled-dog report now points at `shep
bleats <dog>`, and the version-skew refusal labels its remedy.

**The repository moved to the `shep-pm` org on 2026-08-31**, along with both
dogs and the four `shep-deploy` testbeds, and the docs site moved to
`shep-pm.com`. Two things that cost real time and will again: a GitHub App
installation does NOT follow a transfer, so CodeRabbit stopped reviewing
until it was installed on the org; and the Pages custom domain DOES follow,
carrying the old value, so the site was unreachable while every setting
looked populated and every workflow ran green. A `CNAME` file under
`web/public` does not fix that second one, since Pages ignores it for an
Actions-based deploy. See `.github/workflows/pages.yml`'s header.

**Phase 4 merged on 2026-09-01.** A dog answers `--version` with the
protocol it was compiled against, `shep adopt` refuses a mismatch, and
`shep restart <dog>` warns before bringing a dog back on a binary that
cannot connect. The contract is published in `docs/dogs.md`; answering is
optional, and a dog that does not answer is adopted with its protocol
unknown rather than refused, which is every dog written before it.

Two things it deliberately does not do, both argued at their call sites. A
DAEMON-initiated restart gets no warning, since the check is CLI-side, so a
crash or an autorestart respawn still walks into G12 row 5 unannounced. And
`Child::kill` does not reach descendants, so a probe's grandchild can
outlive it; closing that needs a process group rather than a patch.

**Config overrides merged on 2026-09-03, and it changes what a Flockfile
IS.** A Flockfile is a project template committed to the app's repository,
never written by shep, and what an operator tunes afterwards lives in a
shep-owned store at `$SHEP_HOME/overrides.json` (locked and `0600`, like the
KV store). `shep start <Flockfile>` now sends `Request::ApplyConfig` and
merges the file into the sheep of the same name: additive by default, so it
appends keys nobody has established and overwrites nothing, because a
Flockfile arrives through a pull request. Widening it takes
`--reset=<mode>`, one flag with four required values rather than two
booleans: `file` puts back what the template declares and leaves `env` and
every undeclared key alone; `policy` does the same but for every key,
declared or not; `env` puts back only `env`; `all` does both. `--reset`
with no value is a usage error naming the four. Every mode is refused when
the target names a sheep, since a name reads no file, and on a bare script
path too, for the same reason. A load with NO FLAG never prunes and never
kills, and the merge itself registers nothing, though the `shep start`
carrying it still registers and starts an app the flock does not have, by
its own fresh path --
a field the running child holds parks as pending and `shep reload`/`shep
restart` promote it, re-resolving identity only when `user` or `group` moved.
**A reset can kill, and that is deliberate.** `instances` is Structural, held
out of a plain load entirely and routed through `handle_scale` under any
mode but `env`, whose `Ordering::Less` arm deletes the instances above the
new count on the same path `shep delete` takes
(`a_plain_load_never_scales_and_a_reset_does` pins it). `file` scales too,
when the template declares `instances`: it takes the count on the same
terms as every other key it declares. The sharp edge is the undeclared
case. A Flockfile that never mentions `instances` still means 1 under
`policy` or `all`, because that is the compiled default the reset falls
back to, so an app stocked to four goes to one against a file that has no
opinion about the count. `file` is the mode that survives that specific
case, because `merge_declared` never puts an undeclared `instances` in
scope, so there is nothing to put back. The overrides page carries the
warning. A `CFG`
column in `shep flock` and in `shep lookout` marks a sheep with pending
(`!N`) or overridden (`*N`) fields, and `shep describe` lists the names. A
per-app refusal exits non-zero. The four-way field classification lives in
`crates/shep-core/src/config/apply.rs` and is measured against read sites,
not guessed from field names: `kill_signal` is NextSpawn rather than Live,
and `shutdown_with_message` needs a respawn. `web/src/pages/docs/overrides.astro`
is the operator-facing account.

**`shep add` is decision 7 of that same spec, and it is the verb that makes
the template model usable.** It takes the targets `shep start` takes, runs the
same load, and spawns nothing: the app lands registered and `Stopped`, its
declared keys established, and `shep start <name>` brings it up. Without it
the first thing an operator does with a template shipping `env = { DB_HOST =
"", DB_PASSWORD = "" }` is start it, which spawns against an empty database
URL, crash-loops through the restart budget, and has to be stopped before it
can be configured. `start` and `add` are ONE code path (`lifecycle::load`,
carrying a `Load`), because a document that registered differently depending
on which verb read it is one nobody could reason about. Four places consult
it: which request a fresh app goes out as, whether an app the flock already
has is resumed after the merge, what a name target that resolves to a
registered sheep does, and the notice code. `Request::Add` /
`Response::Added` are additive and did not move `PROTOCOL_VERSION` on
their own; it later moved to 3 for an unrelated reason, recorded below, and
the paragraph below applies to `shep add` too. **The fill-in half of
"register, fill in, start" does not exist yet**: an established `env` key
today moves only through the file plus `--reset=env` (or `--reset=all`,
which also drops the override record), and editing one in place is a later
slice (spec decisions 10 and 11).

**Restart the shepherd after upgrading to it.** `PROTOCOL_VERSION` did NOT
move for `ApplyConfig` itself or for `Add` (both variants are additive, and
six precedents in shep-core's changelog agree), so an older shepherd cannot
decode either one. **This paragraph said the operator meets a dead client,
full stop, and that is wrong: it skips the skew guard, which fires first in
the common case.** `refuse_version_skew` compares the shepherd's reported
crate version against the client's own and refuses every verb but the three
in `RECOVERY_VERBS` (`kill`, `ping`, `daemon reload`), at the connect site,
before any request is sent. So the two cases are:

- **Versions differ**, which is every release upgrade through cargo or brew:
  `error[version_skew]`, naming `shep daemon reload` as the remedy. No
  request is sent and nothing is ambiguous.
- **Versions match**, which is a client built from a commit that added the
  variant against a shepherd built from an earlier commit of the same
  version, so every development build and every branch: the guard passes,
  the request goes out, and the shepherd ends the connection on an envelope
  it cannot decode. THAT is the dead client, and it is a working-tree
  hazard rather than an operator one.

`shep daemon reload` is the fix in both, and `getting-started.astro` says so
where an operator reads. `every_exempt_verb_is_one_of_the_documented_recovery_verbs`
pins `add` at `Enforce`, since it reaches that through the `_` arm rather
than by being named.

**`PROTOCOL_VERSION` moved to 3 on 2026-09-04, for `ApplyConfig`'s payload
rather than for `ApplyConfig` itself.** The two-case analysis above still
holds for `Add` and for `ApplyConfig`'s own addition. It stopped holding for
`ResetDepth::Settings`, renamed to `ResetDepth::Policy` (with `File`/`Env`
added) in the same commit: a rename changes the wire spelling of an
operation (`--reset`) that already ships, so the "versions match, same
commit lineage" case above is no longer the only hazard. A daemon at
protocol 2 that has simply not restarted since the upgrade now fails to
decode `"policy"` for what it already understood as `"settings"`, which is a
regression of live functionality rather than an unreachable new one, so the
bump closes that gap with a named `protocol_mismatch` refusal, exit 6,
instead of leaving it as an accepted cost. Not `version_skew`, exit 12,
which this said until 2026-09-04: `refuse_version_skew` runs only after
`connect_or_spawn` returns `Ok`, and a protocol refusal fails the handshake,
so it returns `Err` and that check is never reached. `docs/decisions.md`'s entry on this reverses the
"`PROTOCOL_VERSION` stayed 2" ruling that predates it.

**Verb count: 41 generated, 42 listed, and the difference is `help`.**
`./web/scripts/generate-cli-reference.sh` prints its own number every time it
runs, and its `VERBS` array holds 41 because it does not generate a page for
`help`. `shep --help`'s grouped listing shows 42 because it does. Both are
right about different questions, so neither is a bug to fix; check which one is
being asked before changing either. README.md deliberately quotes the grouping
without a count, so there is no third number to keep in step.

What's built vs. deferred to v1.1+: [docs/specs/deferred.md](docs/specs/deferred.md).

**Windows is built and runs.** This line said "0%, not partial — every verb
prints 'not yet supported' and exits" for eighteen phases, and that is no
longer true of anything. A Windows host became available, and
[windows-estimate.md](docs/specs/windows-estimate.md)'s own first
recommendation — dispatch the CI leg before scoping anything — was run: the
tree was already compile-green on native MSVC. Tier A is now implemented and
verified against a live flock on real Windows.

What that means for anyone editing this workspace:

- **`cfg(unix)` is no longer a free choice.** `shep-client`, `shep-daemon`'s
  `boot`/`server`/`tokio_runner`, and every `shep-cli` module tree are
  portable now. The OS transport lives in ONE place,
  `shep_core::transport` — a unix socket or a Windows named pipe — and
  everything above it (codec, handshake, actor, RPC dispatch) carries no
  platform gate at all. Adding one back is a design decision, not a shrug.
- **A per-sheep job object replaces the process group.** `sys_windows.rs` is
  the crate's only unsafe on that platform, mirroring `sys.rs`'s rule. It is
  stronger than the unix design: `kill.rs` documents an escaped-`setsid`
  hole that a job simply does not have.
- **Three refusals are permanent and deliberate**, each argued at its own
  call site: no graceful signal outside the shepherd channel, no
  `shep startup` (that is Tier B — an SCM service), and no `user`/`group`.
- **The local gate does not run Windows tests.** `cargo test` on a Mac never
  compiles a `cfg(windows)` item, and the `windows-gnu` cross-check is
  `cargo check`, which executes nothing. `.github/workflows/test.yml`'s
  `windows-latest` legs are what actually run this tier. Read the CI result.
- **A `cfg(windows)` arm that compiles has been checked for spelling, not
  for behaviour, and the difference has already cost a shipped bug.**
  shep-channel's named-pipe arm type-checked on every Windows CI run for as
  long as it existed and deadlocked the first time a process actually
  executed it: the shepherd hands an app ONE pipe instance, `try_clone` is
  `DuplicateHandle`, and Windows serialises every operation on a synchronous
  file object, so the reader thread parked in `ReadFile` held it against the
  writer thread's `ready()` forever. Fixed 2026-09-02 by `PipeReader`, which
  peeks rather than parks. **When a platform arm has no test that runs it,
  say so out loud rather than letting a green CI imply otherwise** — the PR
  that introduced it did say so, in as many words, which is the only reason
  anyone went looking. The same audit found the docs site's Python sample
  opening the pipe twice, which succeeds and silently discards everything
  the app writes.

The instances redesign merged too: `increment_var` is removed, and refused
with the replacement named rather than a bare serde error. Env values, args,
`out_file` and `err_file` can now carry `{{instance}}` and `{{name}}`
templates (doubled braces escape a literal brace), `SHEP_INSTANCE` and
`SHEP_NAME` are always injected and can no longer be set by hand in
`[app.env]`, and an explicit `out_file`/`err_file` on a multi-instance app is
refused unless it carries `{{instance}}` or the app sets `merge_logs`. A
sheep name can no longer contain a colon, since `name:slot` (for example
`web:2`) is now a selector that reaches one instance of a multi-instance
app. `PROTOCOL_VERSION` moved from 1 to 2, because `SelectorSpec` gained
an `Instance` variant an older daemon cannot deserialize, so it refuses a
newer client at the handshake and an operator restarts the daemon after
upgrading. The output envelope's `SCHEMA_VERSION` did NOT move and is
still 1: `ProcessInfo.instance` is purely additive, and the envelope's own
rule is that only a rename, a removal or a retype bumps it. The two
constants answer different questions and it is easy to move the wrong one. `shep flock` groups a multi-instance app under one rollup row
(`web ×3`, with `↳ :0` marker rows beneath it) in `full` and `plain` style;
`bare` and JSON still print one row per instance, with `bare` suffixing the
name and JSON carrying the slot as its own field. `shep lookout`'s flock
table gained the same group row, selectable like any other, and an action
on it reaches every instance behind a confirm naming the count. `shep
bleats` now reads a log file shared by several instances once instead of
once per instance, and labels a multi-instance app's lines with their slot.

**A dog's config moved out of `shep.toml` on 2026-09-03.** A dog's section
used to live under `[dog.<name>]` in `shep.toml`; it now lives under
`[<name>]` in a new, hand-editable `$SHEP_HOME/dogs.toml`. The daemon
migrates any old sections once, at boot, and refuses to boot rather than
guess when a name holds VALUES in both files. **Not when a name merely
exists in both**, which is what this line said and is the rule the branch
removed: an empty section is a header, not a second value, so an empty
`[dog.<name>]` in the source is skipped and an empty `[<name>]` in
`dogs.toml` is written over. Every `shep enable` older than this branch
scaffolds the first shape, and refusing on it took a mixed-version host to
a shepherd that would not boot. `RawDaemonConfig::dog` is kept on
purpose: removing it would turn an un-migrated `shep.toml` into a refused
boot under `deny_unknown_fields`, so it stays as the thing the migration
reads from. The migration itself lives in
`crates/shep-cli/src/commands/dog_migration.rs`.

Project memory (cross-session state) tracks decisions; docs above are the
source of truth.
