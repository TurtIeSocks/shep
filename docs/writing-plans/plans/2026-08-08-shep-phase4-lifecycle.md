# shep Phase 4 — Lifecycle extras Implementation Plan

> **STATUS: COMPLETE AND SUPERSEDED (2026-08-09).** All fourteen tasks shipped. The
> unticked `- [ ]` boxes below are left as written: they record the plan as it was
> drafted, not the state of the tree, and ticking them would assert a step-by-step
> equivalence the branch does not have — several tasks deviated, and the deviations
> are recorded where they were decided, not here.
>
> **Do not read this file as the state of the code.** For what actually shipped, in
> descending order of authority: the per-task briefs and reports in `.superpowers/sdd/`
> (`task-1-brief.md` … `task-14-report.md`, plus `phase4-*.md` for the cross-task
> amendments), `docs/systematic-refactor/refactor-workspace/map.md` for module-level
> behavior, and the crate `CHANGELOG.md` files for the user-visible surface.
>
> Known drift between this plan and the tree, so nobody re-derives it: the
> Architecture note below says watch and cron reach the engine through
> `SupervisorHandle::restart(ProcessSelector)`. They go through
> `restart_automatic`, which declares `CommandOrigin::Automatic` so an operator's
> `stop` can displace an automatic restart mid-kill-ladder. The budget-reset
> behavior the note relies on is unchanged.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
> **REQUIRED SKILL:** invoke `shep-idiomatic-rust` before writing ANY Rust here. Cite rules as `IR-<n>`.

**Goal:** Give the merged supervisor the four lifecycle extras spec §4 and §7 promise and the engine does not yet have: restart on file change, restart on a cron schedule, restart on a memory-limit breach, and readiness/liveness probes.

**Architecture:** Four independent subsystems, each a task (or task pair) that owns one module and one seam trait, plus one integration module that arms and disarms them. Watch and cron reach the engine through the *existing public* `SupervisorHandle::restart(ProcessSelector)` (`crates/shep-daemon/src/supervisor.rs:210`), which already resets the restart budget the way spec §4 wants a non-crash restart to, and neither adds a line to the actor. Two things do reach into the actor, and only two: readiness, because it changes what `starting → online` means, and the pid guard the memory-breach and liveness reports need — a new command that drops a stale report and otherwise delegates straight back to the same budget-resetting path, never a second respawn route (Task 12).

Parsing lives in `shep-core`: a cron pattern and a probe target are Flockfile grammar, and spec §5's "typos fail loudly at parse time" means a bad one must be rejected by `normalize`, not discovered by a worker three seconds after the daemon adopts the app.

**Tech Stack:** croner, chrono, chrono-tz (cron grammar + IANA zones), notify + notify-debouncer-full (filesystem events), globset (already a dependency), sysinfo (RSS sampling), tokio, criterion (new bench crate).

---

## Global Constraints

Every task's requirements implicitly include this section. Values here are exact and are not to be "improved".

**Rust / workspace**
- MSRV **1.88**, edition **2024**, resolver 3 (`Cargo.toml:12-16`).
- Workspace lints deny `missing_docs` and `missing_debug_implementations` (`Cargo.toml`, `[workspace.lints.rust]`). Every new public item needs a doc comment and a deliberate `Debug` decision.
- `clippy::undocumented_unsafe_blocks` is denied workspace-wide. This phase adds no `unsafe`; `shep-core` is `#![forbid(unsafe_code)]` (`crates/shep-core/src/lib.rs:21`) and `shep-daemon` is `#![deny(unsafe_code)]` (`crates/shep-daemon/src/lib.rs:186`). Neither changes.

**New dependencies — six of them, and the rehearsal that keeps their floors honest**
- Every new dependency: `default-features = false`, features named explicitly with a `# Option:`-style consequence comment (IR-2, IR-3).
- Every new dependency gets a `-Z minimal-versions` rehearsal. Where the rehearsal fails, the floor pin goes in `[workspace.dependencies]` **with a comment naming the exact API that forced it**, matching the existing block's style (`Cargo.toml`, the `annotate-snippets` / `tokio` / `tokio-stream` comments are the models — each one names a *function or type*, not a version range).
- **A `[workspace.dependencies]` entry that no member opts into with `.workspace = true` is not in the dependency graph at all**, and `cargo +nightly generate-lockfile -Z minimal-versions` will not resolve it. A task that adds manifest entries alone therefore rehearses nothing. That is why this plan has no "add the dependencies" task: **each new crate's `[workspace.dependencies]` entry lands in the same task as its first consumer**, and that task runs the rehearsal. The mapping, so no two tasks race for the same line:

  | Crate | Workspace entry owned by | First consumer |
  |---|---|---|
  | `croner`, `chrono`, `chrono-tz` | Task 1 | `shep-core/src/config/cron.rs` |
  | `sysinfo` | Task 4 | `shep-daemon/src/limits/sample.rs` |
  | `notify`, `notify-debouncer-full` | Task 10 | `shep-daemon/src/watch/source.rs` |

  `criterion` is deliberately absent from that table. The bench crate carries
  its own `[workspace]` table (Task 5), so it cannot opt into a root
  `[workspace.dependencies]` entry with `.workspace = true` and the root-level
  rehearsal never walks it. It declares criterion inline in its own manifest
  and is exempt from the rehearsal because it is outside the graph.

- **Every task that adds a dependency also runs the MSRV check, because none of
  the seven gates below does.** All of them run on stable or nightly, so a
  dependency whose own `rust-version` outruns this workspace's 1.88 resolves
  green locally and turns three CI legs red (`.github/workflows/test.yml:41`
  puts `"1.88"` in the `test` matrix across all three platforms). This is not
  hypothetical: sysinfo 0.39.x declares `rust-version = "1.95"`, which is why
  Task 4 pins the 0.38 line. Tasks 1, 4 and 10 each run, from its own exit code:

  ```
  rustup toolchain install 1.88 --profile minimal
  cargo +1.88 check --workspace --all-features --locked
  ```

  A rejection here reads `error: rustc 1.88.0 is not supported by the following
  package`, names the crate, and is a hard failure — not a warning. The fix is a
  lower pin on the offending crate, never an MSRV bump: `Cargo.toml:14` is the maintainer's
  support-window decision and is out of this phase's scope.

- **The 1.88 baseline is green locally, and that became true only on
  2026-08-08.** The toolchain was not installed on this machine before that
  date, so every earlier "MSRV is fine" claim in this repo rested on CI alone.
  It is installed now, and `cargo +1.88 test --workspace --locked` passes on
  `main`. Two consequences for the tasks below: the `rustup toolchain install`
  line is a no-op rather than a first install, and a 1.88 rejection a task sees
  is that task's own new dependency rather than an inherited red baseline. Note
  the two commands are not the same one — the baseline run was
  `test --workspace --locked` on the default feature set; the gate above stays
  the `check --workspace --all-features --locked` form, which builds less but
  reaches every feature-gated dependency.

- The research's crate table (`docs/research/phase4-lifecycle.md:9-18`) was verified against crates.io on 2026-08-07 and re-verified against docs.rs on 2026-08-08. Three of its claims did not survive; the corrected facts are stated at the task that consumes them, and the corrections are listed in this plan's report obligations (Exit criteria 7).

**Platform tiering (spec §11 — CI-enforced)**
- `.github/workflows/test.yml` puts `windows-latest` in the `test` matrix and runs `cargo test --workspace --locked` there, and the `features` job runs `cargo test --workspace --locked --all-features` on Windows too. Both legs are green on `main`. A module that does not compile on Windows does not merely fail this plan's gate — it fails CI.
- **Every module this phase adds is pure tier**, and that is a deliberate, checkable claim rather than an accident:
  - `shep-core/src/config/cron.rs` — croner, chrono and chrono-tz are portable; nothing here touches the OS beyond `chrono`'s clock.
  - `shep-daemon/src/cron.rs` — `tokio::time` only.
  - `shep-daemon/src/limits/` — sysinfo supports Windows (`ReadDirectoryChangesW` is a different subsystem; process enumeration there is `NtQuerySystemInformation`), so the sampler compiles and runs everywhere.
  - `shep-daemon/src/probes/` — `tokio::net::TcpStream` and `tokio::process::Command` are both portable, and `shep-daemon` already enables tokio's `net` and `process` features (`crates/shep-daemon/Cargo.toml:20`).
  - `shep-daemon/src/watch/` — notify's Linux backend is inotify, macOS is FSEvents, Windows is `ReadDirectoryChangesW`; all three are selected by notify itself.
  - `shep-daemon/src/extras.rs` — plain tokio task bookkeeping.
- **The only `#[cfg]` this phase writes is inside a pure-tier file**: the exec probe's shell selection (`sh -c` on unix, `cmd /C` on windows) in `probes/os.rs`. It is a `#[cfg]` on two arms of one function, never on a `mod` declaration, so the module's `#[cfg(test)]` block runs on every platform.
- Consequence for the implementer: **do not gate any new `mod` line.** If a new module will not compile on `x86_64-pc-windows-gnu`, that is a defect in the module, not a reason to reach for `#[cfg(unix)]` — the four gated modules (`boot`, `sys`, `tokio_runner`, `server`, at `crates/shep-daemon/src/lib.rs:304`, `:314`, `:323`, `:335`) are gated because `nix` and `command-fds` are `[target.'cfg(unix)'.dependencies]`, and nothing this phase adds is.
- `boot.rs` is the one `#[cfg(unix)]` file this phase edits, and it edits it only to *construct* the extras. Everything it constructs must be constructible on Windows too, or the Windows test leg loses coverage of the construction path entirely.

**The seams are `dyn`, and that is a departure from the engine's existing convention**
- `ProcessRunner` is a generic parameter on the actor (`Actor<R: ProcessRunner>`, `crates/shep-daemon/src/supervisor.rs:391`) because it is the spawn hot path and it was there first. This phase's three seams — `MemorySampler`, `LimitEnforcer`, `Prober` — are **trait objects** instead: `Arc<dyn MemorySampler>`, `Box<dyn LimitEnforcer>`, `Arc<dyn Prober>`.
- The reason is arithmetic, not taste. Threading three more generic parameters through `Actor`, `spawn_supervisor`, `boot`, `RunningDaemon` and every test harness turns `Actor<R>` into `Actor<R, S, L, P>` and every fixture into a four-parameter turbofish, to save one vtable dispatch per probe (once per `interval`, default 10s) and one per memory poll (once per 15s). At those rates the indirection is unmeasurable and the type noise is permanent.
- **`Prober` therefore may not use RPITIT.** The Phase 2a/2b/3 plans state "async trait methods returning futures use RPITIT with an explicit `+ Send` bound"; RPITIT is not dyn-compatible, so `Prober::probe` returns `Pin<Box<dyn Future<Output = ...> + Send + '_>>` instead. This is a named exception with a stated reason, not a drift — write that reason as an IR-31 `//` block comment above the trait, so the next reader does not "fix" it back into an `async fn` and break `Arc<dyn Prober>`.
- `MemorySampler` and `LimitEnforcer` have **synchronous** methods and so raise no such question. Sampling is a blocking `/proc` walk that lives on the enforcer's own task, never on the actor's; making it `async` would buy nothing and would make it dyn-hostile for the same reason.
- Each of the three gets a dyn-compatibility smoke test (IR-10): a `let _: &dyn Trait = &concrete;` binding in the module's own test block. It costs one line and it fails at compile time the moment somebody adds a generic method.

**Testing — the ten rules Phase 3 learned the expensive way**

These are not style preferences. Each one cost a review round or a full task cycle in Phase 3, and each is checkable.

1. **Every helper is declared with a full signature before it is used, test helpers included.** A helper that exists only as prose in a task's requirements cannot be type-checked by a reviewer and cannot be found by the implementer of the *next* task. The fixture roster below carries a signature for every one, with exactly one owning task.
2. **Test bodies must compile.** Two specific traps, both hit in Phase 3: a value used after a `close(self)`-style method moved it, and a fake torn down before the code under test connected to it, so the connection's mandatory first exchange could never succeed. Where this plan shows a test body it has been checked for both; where it shows only a signature, the implementer writes the body and checks it.
3. **Platform tier is stated per file** — see the section above. Every task's Files list repeats its files' tier.
4. **Every `recv().await` in a test is wrapped in `tokio::time::timeout`, with a message naming what did not arrive.** A test that fails by hanging gives CI a killed job instead of an assertion; Phase 3 shipped ten of these before the rule stuck. This phase is *more* exposed than Phase 3 was, because four of its subsystems communicate with the supervisor exclusively through channels.
5. **Every test names the broken implementation it catches.** A one-line `//` comment above the assertions, in the form "fails if <the specific wrong implementation>". Eleven Phase 3 tests turned out to guard nothing — including one whose comment described an assertion the body did not contain. A test whose comment cannot be written is a test that is not worth writing.
6. **Prefer a compile-time gate to a test wherever one exists.** Phase 3's single best fix replaced a runtime test with an exhaustive `match` that fails `cargo check` with E0004. This phase has three such places, named at their tasks: the `ProbeTarget` match in `OsProber`, the `ProbeKind` match in `ProbeTarget::parse`, and the `ReadinessSource` match in `await_ready`. **None of them may carry a `_` arm.** `ProbeKind` (`crates/shep-core/src/config/app.rs:9-15`) is not `#[non_exhaustive]`, and neither of the two new enums is, so the compiler does the work if it is allowed to.
   **`ProcessEventKind` is the counter-example and must not be mistaken for a fourth.** It *is* `#[non_exhaustive]` (`crates/shep-core/src/protocol/events.rs:11`), so a match on it in `shep-daemon` — a different crate — is required to carry a `_` arm and E0004 will never fire. Where this phase matches it, the `_` arm's behaviour is stated in a comment and is never a `todo!()`, exactly as the Phase 3 plan requires for every `#[non_exhaustive]` match.
7. **`assert_ne!` is barely an assertion.** It passes for every other value, including all the wrong ones. If the interesting claim is "this changed", assert what it changed *to*.
8. **Third-party APIs are verified against real documentation, never recalled.** Phase 3 wrote three clap facts from memory and all three were wrong. Every third-party signature in this plan was read off docs.rs on 2026-08-08 and is quoted verbatim at the task that uses it; an implementer who needs one this plan does not quote looks it up rather than guessing.
9. **One task owns each shared type, constant, and fixture.** Later tasks consume and define none. The fixture roster and the dependency-ownership table above are the record.
10. **No task-relative phrasing in code, docs, or manifests.** No `// Task 7 will…`, no `# Task 2`, no "this task adds". A reader in six months has this plan file at best and cannot locate a task number. Ownership belongs in this document; the code says what it does.
11. **Advance a paused clock in steps no larger than the shortest period of the loop under test, and make negative assertions poll a bounded window (`timeout` + `recv`) rather than a single `try_recv`.** Task 3 measured why: one `advance(3600s)` against a 60s-period loop produces exactly 1 wakeup, while walking the same span in steps small enough to match the loop's own cadence produces 59 — a single big jump resolves the task's one currently-pending sleep in a single shot and collapses every wakeup in between into that one, hiding whatever the loop does (or fails to do) on each of the wakeups it never gets a chance to take. Auto-advance (no explicit `advance()` call — the runtime just walks its own timers when nothing else is ready) is unaffected and safe; the hazard is explicit `advance()` itself, and small steps shrink it without removing it. **Of the rule's two clauses the bounded window is the one that carries the weight** — the step-size clause is necessary, not sufficient, and the measurements below are why. This governs every task below that adds a periodic loop, not just Task 3's cron worker — Task 6's memory poll, Task 7's probe intervals, and Task 11's watch debounce all inherit it.
   **Step size alone does not rescue an `advance`-driven test.** `tokio::time::advance` moves the clock and yields once, so a worker the timer just woke is *scheduled* but has not run by the time `advance` returns; the test then reads the world one scheduler pass early, and the last tick of any span is the one it misses. Measured on tokio 1.53, three ticks due in every row:

   | Crossing three 15s periods | `sleep`-then-work loop | `interval` grid loop |
   |---|---|---|
   | 3 × `advance(15s)` — steps equal to the period | **1** of 3 | 2 of 3 |
   | 45 × `advance(1s)` — steps far below the period, as the rule prescribes | **2** of 3 | 2 of 3 |
   | one `timeout(3 × 15s + 1ms)` + `recv`, auto-advance only | **3** of 3 | 3 of 3 |

   The shipped enforcer is the left column: it sleeps, then works, so its next deadline does not exist until the task has run, and a jump spanning three periods can only ever satisfy the one sleep already pending. The `+ 1ms` is not decoration — a window ending exactly on the last deadline races it, and the hair clears it. Only the bottom row observes the loop the test is about. The 59 in the rule above is the same effect seen from the other end: an hour of a 60s-period loop contains 60 occurrences, and stepping the whole way reaches 59 of them. Stepping buys back all but the last tick, which is exactly the tick a test's final assertion is usually about.
   **A `try_recv` negative is worse than imprecise: it cannot fail.** After `advance(15s)`, `try_recv` returns `Err(Empty)` while the very next bounded `recv` returns the message that was already due. So a deliberately broken worker that reports on *every* tick still **passes** a `try_recv` negative, because `Err(Empty)` is exactly what that assertion wants; the bounded form fails it, correctly. A test that cannot fail guards nothing (rule 5). The same undercount silently moves any call counter a table pins — see the table above — so the assertion form changes what a test can *count*, not merely how it waits.
   **The trade-off the carve-out claimed does not exist.** "A timeout that is expected to expire is a test that takes real time to pass" is true only on a real clock. Under `start_paused` the runtime auto-advances to the deadline when nothing else is ready, so an expiring `timeout(60s)` costs **54µs** of wall clock, ten expiring `timeout(3600s)` in sequence — ten virtual hours — cost **534µs** together, and the same 3600s window over a 60s-period loop records all **59** wakeups in **1.8ms** while still expiring. The real clock is where the objection bites: an expiring `timeout(250ms)` there costs the full **252ms**. This rule is scoped to the paused clock that IR-33 already makes the default, so the expensive case is the one the crate does not write.
   **Size the window to the span whose silence is being claimed.** A bounded window is a claim about an interval, so it should cover exactly the ticks the test says produce nothing — spanning them is the point, and a correct worker that ticks and reports nothing leaves the window to expire. The one way to get it wrong is a window that outruns a message the test expects to arrive *legitimately* later: auto-advance walks to that message and delivers it, turning a real pass into a confusing failure. Where a task's negative sits between two expected events, end the window before the second.

Beyond those: paused tokio clock where time matters (`#[tokio::test(start_paused = true)]` is this crate's default — real time needs a justifying comment, IR-33), no sleeps as synchronization, hand-rolled fakes against the real traits, a unique config literal and tempdir per test (IR-34). **No test may touch the real `$HOME` or `$SHEP_HOME`, and none may bind a real unix socket** — every fixture is rooted in its own `tempfile::TempDir`, exactly as `crate::testing::test_paths` (`crates/shep-daemon/src/lib.rs`) already does. The two tiers that legitimately touch the real OS — the filesystem-watch smoke test and the loopback HTTP probe — still bind only to a tempdir and to `127.0.0.1:0`.

**Style (from docs/idiomatic-rust.md — cite by number in reviews)**
- `impl core::error::Error`, never `std::error::Error` (IR-19). Per-module error enums whose variant docs state the precise condition, not the variant name restated (IR-18). No crate-wide umbrella error.
- Every `Result`-returning public fn carries a `# Errors` section (IR-28).
- `# Panics` and `#[track_caller]` travel together, or neither appears (IR-21).
- No panicking constructors outside `shep-cli` (IR-21) — which means none at all in this phase, since it touches only `shep-core` and `shep-daemon`.
- `#[non_exhaustive]` where growth is genuinely anticipated, with a comment saying why (IR-20). `ProbeFailure` and `CronParseError` qualify; a two-variant internal enum does not.
- **No magic numbers.** Every duration, threshold and cap is a named `const` with a comment giving the unit and the reason (IR-26). This phase introduces seven — `DEFAULT_MAX_CRON_SLEEP`, `MIN_CRON_SLEEP`, `MEMORY_POLL_INTERVAL`, `HTTP_STATUS_LINE_CAP`, `DEFAULT_WATCH_DELAY`, `DEFAULT_IGNORE_GLOBS`, `WATCH_SMOKE_DEADLINE` — and each is declared at exactly one task. The first two are the two halves of one configurable knob and are both Task 3's, but they live in different crates: the default in `shep-daemon`, because that is where it is applied, and the floor in `shep-core`, because that is where the value is validated.
- Implementation rationale is a `//` block above the item, never `///` (IR-31).
- Types carrying env or secrets get a manual redacted `Debug` plus an exact-string test (IR-41). The exec probe's `ProbeTarget::Exec` carries a command line and the spawn env carries the sheep's environment — see Task 8.

**Terminology (docs/terminology.md)**
- `sheep` = one managed process, singular only. The plural is **flock**, never "sheeps". **Lambs** are a sheep's child processes — the term the memory-limit tree summation is about.
- "the shepherd" names the daemon and nothing else.
- Destructive operations and all error text stay plain English. A user whose app just got killed for exceeding `max_memory` reads "memory limit exceeded", not a pun.

**Gates — every one from its OWN exit code, no pipelines that swallow status**
```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace
cargo test --workspace --all-features
RUSTDOCFLAGS="-Dwarnings --cfg docsrs" cargo doc --workspace --all-features --no-deps
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
cargo test --workspace --all-features -- --test-threads=1
```

The Windows gate carries `--all-features` deliberately. Without it, anything behind `test-fakes` — and anything a future feature gates — never faces the Windows compiler at all, and the leg becomes a check of the default feature set only. It is cheap and it keeps the local gate a superset of every CI leg.

The single-threaded run is the last line because two of this phase's tiers are sensitive to it in opposite directions: the filesystem-watch smoke test is slower under contention (it waits on real inotify/FSEvents delivery) and the loopback probe tests are more likely to interleave port allocations. Both must pass either way.

**Explicitly out of scope for Phase 4** — do not build these, do not stub them:
`Command::Reload` and the reload state machine (`ReloadState` at `crates/shep-daemon/src/entry.rs:102` stays data-only; there is no `Request::Reload` and adding one is a wire change), `scale`, cgroup v2 enforcement (`enforce = "kernel"` is v1.1 — the `LimitEnforcer` seam exists so it can land without touching the engine, and building it now would defeat the point of proving the seam), TLS or redirects in the HTTP probe, per-probe custom actions (spec §14.9), the metrics dog's consumption of the sampler, and any new `BusEvent` or `ProcessEventKind` variant. That last one is the load-bearing exclusion: `BusEvent` carries a `// wire format: changing existing variants is a breaking change` comment (`crates/shep-core/src/protocol/events.rs:35`) and has committed insta snapshots. Readiness timeouts and liveness failures are reported with `tracing::warn!` and by the restart they cause, not by a new event kind.

---

## File Structure

```
Cargo.toml                            workspace deps: croner, chrono, chrono-tz, sysinfo,
                                      notify, notify-debouncer-full

crates/shep-core/
  Cargo.toml                          + croner, chrono, chrono-tz
  src/config/cron.rs         [pure]   CronSchedule: parse + next_after. croner and chrono-tz
                                      are private implementation; neither appears in the
                                      public signature.
  src/config/probe.rs        [pure]   ProbeTarget: the parsed, validated form of
                                      ProbeConfig::target, one variant per ProbeKind
  src/config/normalize.rs    [pure]   MODIFY: the 5-field cron stopgap (lines 54-60) becomes
                                      a real parse; probe targets get validated
  src/config/daemon.rs       [pure]   MODIFY: the [daemon] max_cron_sleep knob, its
                                      SHEP_MAX_CRON_SLEEP override, and the floor that
                                      keeps a cron worker off a busy-spin
  src/config/mod.rs          [pure]   MODIFY: re-export the two new modules' public types

crates/shep-daemon/
  Cargo.toml                          + sysinfo; tokio gains no features (net/process/time
                                      are already on)
  src/testing.rs             [pure]   MOVED out of lib.rs's inline `mod testing`; grows every
                                      fake this phase adds
  src/cron.rs                [pure]   Clock seam + SystemClock + the cron worker loop
  src/limits/mod.rs          [pure]   LimitEnforcer seam, PollingEnforcer, LimitBreach,
                                      MEMORY_POLL_INTERVAL
  src/limits/sample.rs       [pure]   MemorySampler seam, SysinfoSampler, the process-tree sum
  src/probes/mod.rs          [pure]   Prober seam, ProbeFailure, the liveness ProbeTask
  src/probes/os.rs           [pure]   OsProber: hand-rolled HTTP/1.1 GET, TCP connect, exec
  src/probes/ready.rs        [pure]   ReadinessSource + await_ready
  src/watch/mod.rs           [pure]   WatchGroup task: glob filter, single-flight restart,
                                      queued-event re-check
  src/watch/source.rs        [pure]   the OS seam: notify + debouncer -> mpsc bridge
  src/extras.rs              [pure]   ExtrasRegistry: arms and disarms all four on lifecycle
  src/supervisor.rs          [pure]   MODIFY: SupervisorBuilder, the readiness gate, the
                                      arm/disarm call sites, and the pid-guarded
                                      extra_restart the breach and liveness reports use
  src/boot.rs                [unix]   MODIFY: construct the real seams and hand them to the
                                      builder; BootOptions carries max_cron_sleep
  tests/external_impls.rs    [pure]   compile-only proof that an outside crate can implement
                                      the three new traits (IR-38)

crates/shep-cli/
  src/commands/daemon.rs     [unix]   MODIFY: boot_options folds max_cron_sleep into
                                      BootOptions, and run_daemon's doc names the third
                                      SHEP_* variable it now reads

benches/                              outside the workspace: own [workspace], publish = false
  Cargo.toml                          criterion (declared inline), harness = false
  benches/memory_sample.rs            the number behind MEMORY_POLL_INTERVAL
```

`crates/shep-cli/src/commands/` is declared `#[cfg(unix)]` in `main.rs:10-11`, so the CLI half of the cron knob is unix-tier for the same reason the whole hidden `daemon` subcommand is. Nothing about the knob itself is: the field, its validation, `BootOptions` and the worker are all pure, and the Windows leg still compiles and tests every one of them.

`watcher.rs` and `worker.rs` — both named in map.md — are **not** created. `watcher.rs` becomes the `watch/` directory because the maintainer's 500-line split rule bites once the OS seam and the filtering logic share a file, and because the two halves have genuinely different test tiers (real filesystem versus paused clock). `worker.rs` was map.md's host for interval tasks; this phase's two interval loops live with the subsystems they serve (`cron.rs`, `limits/mod.rs`), which is one fewer indirection and one fewer place to look. `probes/` is a module map.md never named at all; spec §7 requires it and the spec wins (`docs/specs/shep-v1.md:8-9`). Task 14 records all three in map.md.

`probes/` and `limits/` are directories rather than single files for the same 500-line reason: `probes.rs` would hold a trait, three transport implementations, a threshold engine and a readiness state machine, and `limits.rs` would hold two seams plus a tree-walking sampler. Splitting them at the seam boundary also makes each task's review surface one file.

**There is no `shep-core/src/config/error.rs`.** Each new error enum lives in the module that produces it (IR-18): `CronParseError` in `cron.rs`, `ProbeTargetError` in `probe.rs`. `NormalizeError` (`crates/shep-core/src/config/normalize.rs:84`) grows variants but wraps neither of them — see the next paragraph, which is not optional reading.

**`NormalizeError` cannot wrap `croner::errors::CronError`, and the reason is a hard API fact.** `NormalizeError` derives `Debug, Clone, PartialEq, Eq` (`normalize.rs:83`) and its tests compare whole values (`normalize.rs:137-140`, `:159-160`, `:179-182`). `CronError` implements `Debug`, `Display` and `Error` — and **not** `Clone`, and **not** `PartialEq`. Wrapping it would force `NormalizeError` to drop three derives and would break six existing assertions in a module this phase is otherwise only extending. The new variants therefore carry owned `String`s: the offending pattern, plus the reason rendered from `Display`. That is also better error text — the user gets croner's sentence without the type.

---

## Fixture roster — every fake, one owner, full signature

All of these live in `crates/shep-daemon/src/testing.rs`, which is `#[cfg(test)] pub(crate) mod testing;`. They are **not** behind `test-fakes`: that feature exists to expose `fake::ScriptedRunner` to *other crates'* tests (`crates/shep-daemon/Cargo.toml:11-13`), and nothing outside this crate needs a scripted prober. Keeping them `#[cfg(test)]` also keeps `missing_docs` off them, so a fixture can carry an IR-33 `//` WHY comment instead of a `///` doc it does not need.

The module currently exists as an inline `mod testing { ... }` inside `lib.rs`. **Task 3 moves it to its own file** — unchanged, paths and all (`crate::testing::harness` keeps working) — because lib.rs is 341 lines and this phase would push it well past the maintainer's 500-line split rule. Nobody else moves it.

| Helper | Owner | Signature |
|---|---|---|
| `TestClock` | Task 3 | `pub(crate) struct TestClock { epoch: DateTime<Utc>, started: tokio::time::Instant, reads: AtomicUsize }` |
| `TestClock::starting_at` | Task 3 | `pub(crate) fn starting_at(epoch: DateTime<Utc>) -> Self` |
| `TestClock::reads` | Task 3 | `pub(crate) fn reads(&self) -> usize` — how many times `now_utc` has been called |
| `impl Clock for TestClock` | Task 3 | `fn now_utc(&self) -> DateTime<Utc>` — `epoch + (Instant::now() - started)`, so `tokio::time::advance` moves wall time; increments `reads` |
| `ScriptedSampler` | Task 4 | `pub(crate) struct ScriptedSampler { readings: Mutex<VecDeque<Vec<ProcessRss>>>, calls: AtomicUsize }` |
| `ScriptedSampler::new` | Task 4 | `pub(crate) fn new(readings: Vec<Vec<ProcessRss>>) -> Self` — the last reading repeats once exhausted |
| `ScriptedSampler::calls` | Task 4 | `pub(crate) fn calls(&self) -> usize` |
| `rss` | Task 4 | `pub(crate) fn rss(pid: u32, parent: Option<u32>, bytes: u64) -> ProcessRss` |
| `ScriptedProber` | Task 7 | `pub(crate) struct ScriptedProber { script: Mutex<VecDeque<Result<(), ProbeFailure>>>, calls: AtomicUsize }` |
| `ScriptedProber::new` | Task 7 | `pub(crate) fn new(script: Vec<Result<(), ProbeFailure>>) -> Self` — the last outcome repeats once exhausted |
| `ScriptedProber::with_delay` | Task 7 | `pub(crate) fn with_delay(self, delay: Duration) -> Self` — each `probe` call sleeps `delay` on the paused clock before returning its scripted outcome |
| `ScriptedProber::calls` | Task 7 | `pub(crate) fn calls(&self) -> usize` |
| `probe_config` | Task 7 | `pub(crate) fn probe_config(kind: ProbeKind, target: &str) -> ProbeConfig` |
| `HttpReply` | Task 8 | `pub(crate) enum HttpReply { Status(u16), Raw(String), Hang }` |
| `loopback_http` | Task 8 | `pub(crate) async fn loopback_http(script: Vec<HttpReply>) -> (SocketAddr, tokio::task::JoinHandle<()>)` — binds `127.0.0.1:0`, serves one reply per connection in order |
| `touch` | Task 10 | `pub(crate) fn touch(root: &Path, rel: &str) -> std::io::Result<PathBuf>` — creates parent dirs, writes one byte, returns the absolute path |
| `app_with` | Task 12 | `pub(crate) fn app_with(name: &str, edit: impl FnOnce(&mut AppConfig)) -> ResolvedApp` — `AppConfig::minimal(name, "./srv")`, `edit`, then `normalize().unwrap()` |
| `armed_entry` | Task 12 | `pub(crate) fn armed_entry(id: u32, instance: u32, pid: u32, app: ResolvedApp, paths: &ShepPaths) -> ProcessEntry` — an `Online` entry with `pid: Some(pid)`, log paths from `assemble`, everything else at its default |

`TestClock::reads` is a counter and not a convenience: the only externally visible difference between two `max_cron_sleep` values is how often the worker wakes, and on a paused clock a wakeup is invisible unless something counts it. It is an `AtomicUsize` rather than a `Cell` because `Clock` is bounded `Send + Sync` and `now_utc` takes `&self`; `Relaxed` ordering is enough, since the assertion happens after the worker has been driven to a quiescent point and nothing orders anything else against this count.

`ScriptedSampler` and `ScriptedProber` both repeat their final scripted value rather than panicking on exhaustion. This is deliberate and it is the difference between a useful fake and an irritating one: a liveness test that scripts three failures wants the fourth poll — if the implementation wrongly makes one — to *also* fail, so the assertion is about the threshold count and not about the fake running dry. Both expose `calls()` so a test can assert the exact number of polls, which is the claim that catches an off-by-one threshold.

**An empty script is a defined case, not an accident**, because this plan constructs one at Task 7's dyn-compatibility line and `harness` wires one sampler by default: `ScriptedProber::new(vec![])` returns `Ok(())` forever and `ScriptedSampler::new(vec![])` returns an empty table forever. That is the neutral value in both cases — a prober that never fails and a machine with no visible processes — so a fixture nobody scripted arms nothing and reports nothing. `with_delay` composes with it: an empty script still sleeps.

`ScriptedProber::with_delay` is builder-style rather than a second constructor precisely so the four threshold cases and the dyn-compatibility line keep using `new` unchanged. The delay is honoured even when it exceeds the `timeout` argument, because the fake ignores that argument — the point of the case that uses it is a probe that passes *slowly*.

`Harness` (`crates/shep-daemon/src/lib.rs`, inside `mod testing`) keeps its current shape until Task 12, which adds two fields — the breach receiver and the liveness-failure receiver. Its signature after that change is stated at Task 12 and nowhere else.

---

### Task 1: `CronSchedule` — the real cron grammar, in shep-core

**Files:**
- Modify: `Cargo.toml` (workspace deps: croner, chrono, chrono-tz) — pure
- Modify: `crates/shep-core/Cargo.toml` — pure
- Create: `crates/shep-core/src/config/cron.rs` — pure tier
- Modify: `crates/shep-core/src/config/mod.rs` — pure
- Modify: `crates/shep-core/src/config/normalize.rs` — pure
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces:**
- Consumes: `croner::Cron`, `croner::parser::{CronParser, Seconds}`, `croner::errors::CronError`, `chrono::{DateTime, Utc, TimeZone}`, `chrono_tz::Tz`
- Produces:

```rust
/// A validated `cron_restart` pattern together with the zone it is read in.
///
/// The croner and chrono-tz types are private: a cron dialect is a Flockfile
/// grammar promise, and pinning it to a dependency's public types would make
/// that dependency's next major version a breaking change to shep's own
/// config surface (IR-11).
// wire format: the accepted pattern grammar is a config contract; widening or
// narrowing it is a breaking change
#[derive(Debug, Clone)]
pub struct CronSchedule { /* private */ }

impl CronSchedule {
    /// Parses a `cron_restart` pattern and its optional `cron_timezone`.
    ///
    /// # Errors
    ///
    /// - [`CronParseError::Pattern`] — croner rejected the pattern.
    /// - [`CronParseError::Timezone`] — the name is not an IANA zone.
    pub fn parse(pattern: &str, timezone: Option<&str>) -> Result<Self, CronParseError>;

    /// The first occurrence strictly after `after`, in UTC.
    ///
    /// Returns `None` when the pattern has no further occurrence croner is
    /// willing to search for — a pattern like `0 0 30 2 *` (30 February) that
    /// can never match.
    ///
    /// # Errors
    ///
    /// - [`CronScheduleError::Search`] — croner failed the search for a reason
    ///   other than exhaustion, carrying its own sentence.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, CronScheduleError>;

    /// The pattern as written in the Flockfile.
    #[must_use]
    pub fn pattern(&self) -> &str;
}

/// Growth is expected: croner's dialect has more rejection modes than this
/// enum distinguishes today, and a future `cron_timezone` shorthand would add
/// one more (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronParseError {
    /// The pattern is not valid in shep's dialect. Carries the pattern as the
    /// user wrote it and the rendered reason — croner's own sentence where
    /// croner did the rejecting, ours where the pre-parse pass did.
    Pattern { pattern: String, reason: String },
    /// The `cron_timezone` value is not a name in the IANA database.
    Timezone { name: String },
}

/// Why a validated schedule could not produce its next occurrence.
///
/// One variant today and no `#[non_exhaustive]`: the only failure a search can
/// have that is not exhaustion is croner's own, and a second variant would be
/// a second reason, not a second rendering of this one (IR-20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronScheduleError {
    /// croner could not resolve the next occurrence; carries its rendered reason.
    Search { reason: String },
}
```

**The croner facts this task depends on, read off docs.rs on 2026-08-08 — three of them contradict the research note:**

```rust
// croner::Cron
impl FromStr for Cron { type Err = CronError; }
pub fn find_next_occurrence<Tz: TimeZone>(&self, start_time: &DateTime<Tz>, inclusive: bool)
    -> Result<DateTime<Tz>, CronError>;
pub fn as_str(&self) -> &str;

// croner::parser
pub fn CronParser::builder() -> CronParserBuilder;
impl CronParserBuilder {
    pub fn seconds(self, value: Seconds) -> Self;
    pub fn dom_and_dow(self, value: bool) -> Self;
    pub fn build(self) -> CronParser;          // NOT a Result
}
impl CronParser { pub fn parse(&self, pattern: &str) -> Result<Cron, CronError>; }
pub enum Seconds { Optional, Required, Disallowed }

// croner::errors
pub enum CronError { EmptyPattern, InvalidDate, InvalidTime, TimeSearchLimitExceeded,
                     InvalidPattern(String), IllegalCharacters(String), ComponentError(String) }
// implements Debug + Display + Error. NOT Clone. NOT PartialEq.
```

1. **`find_next_occurrence` returns `Result`, not `Option`.** The research note plans around a `None` for "never fires again" (`docs/research/phase4-lifecycle.md:213`); there is no `None`. Exhaustion arrives as `Err(CronError::TimeSearchLimitExceeded)`. `next_after` maps *that one variant* to `Ok(None)` and every other variant to `Err(CronScheduleError::Search)`, which is why its return type is `Result<Option<_>, _>` and not either half alone. Getting this backwards — treating every error as "no more occurrences" — silently turns a transient failure into a permanently disarmed cron.
2. **`CronParserBuilder::build` returns `CronParser`, not `Result<CronParser>`.** A `?` on it does not compile.
3. **`CronError` is neither `Clone` nor `PartialEq`**, which is why both new error enums carry `String`s. This is the constraint the File Structure section states; it is repeated here because this is the task that would otherwise discover it at `cargo check` time.
4. **`Cron::as_str` gives back croner's *normalized* pattern, not the input.** `0 0 * JUL WED` comes out as `0 0 * 7 3`, verified by running it. `CronSchedule::pattern` promises the pattern *as written in the Flockfile*, so it returns a stored `String` and never `as_str`; the same normalization is why an error message has to carry the user's own spelling rather than croner's rendering of it. The settled nicknames reach this the same way by a different route: shep expands `@daily` before croner sees it, so `as_str` would report `0 0 * * *` there too, and the stored string is what keeps `pattern()` honest.

Do **not** call `.dom_and_dow(true)`. croner's default is OR semantics between day-of-month and day-of-week, which is what the JS-croner dialect map.md promises does; `true` switches to AND and would silently change what an existing pattern means.

**The dialect is five-field standard cron only (the maintainer, 2026-08-08).** She chose the narrow reading over croner's full dialect: widening a grammar later is backwards-compatible, narrowing one is not, so the wide default was the expensive direction to guess.

Call `.seconds(Seconds::Disallowed)`, which rejects the six-field form for us. **That call is load-bearing, not a restatement of a default.** croner's default is `Seconds::Optional` (`croner-3.0.1/src/parser.rs:54-60`, `#[default] Optional`); a builder left unconfigured accepts the six-field form and ships the wide dialect silently, with a green suite. Verified by running croner 3.0.1's default builder against `30 0 3 * * *`: it parses.

`Seconds::Disallowed` alone pins the grammar to exactly five fields, and no `.year(...)` call is needed. croner's field-count gate is `if num_parts == 5 { parts.insert(0, "0") } else if self.seconds.is_disallowed() { return Err(...) }` (`parser.rs:122-130`), and `num_parts` is captured *before* the insert — so the disallowed arm fires for every token count that is not exactly five, ahead of croner's own year handling. Verified: `0 3 * * * 2027` is rejected under `Seconds::Disallowed`. Do not add `.year(Year::Disallowed)` as a belt-and-braces change; it is unreachable here and it would only change which sentence croner renders.

**Every pattern that reaches croner — test rows included — is therefore five-field.** A six-field pattern in a test is a failing test, not a wider case. The `@nickname` shorthands settled below are the only single-token spelling the grammar accepts, and they are expanded to five fields before croner sees them, so they are sugar over this rule rather than an exception to it.

croner still accepts `L`, `W`, `#` and `?` natively, so **rejecting those is our job, not croner's** — a pattern reaching `CronParser::parse` with them in it parses happily and we would have shipped the wide dialect by accident. Reject them before parsing, with a typed error naming the offending character. (Verified against croner 3.0.1 in their legal positions: `0 0 L * *`, `0 0 1W * *`, `0 0 * * 5#3` and `0 0 ? * *` all parse. croner rejects them only when they sit in a field where the extension does not belong, e.g. `W` in day-of-week — so its positional validation is no substitute for our own check.)

**The trap in that check, which a naïve implementation will hit:** day and month names contain those letters. `WED` contains `W`; `JUL` contains `L`. A character scan over the raw pattern rejects `0 0 * JUL WED`, which is valid standard cron.

So the check is token-aware, not character-wise: split on whitespace, and within each field treat a recognised three-letter name (`JAN`-`DEC`, `SUN`-`SAT`, case-insensitive) as opaque before looking for extension characters in what remains. `#` and `?` never occur inside a name and may be rejected anywhere; `L` and `W` may not.

Test both directions explicitly, and cover all four characters — `0 0 L * *`, `0 0 1W * *`, `0 0 * * 5#3` and `0 0 ? * *` are rejected with the character named, while `0 0 * JUL WED` and `0 0 * * MON-FRI` are accepted. `W` needs its own row rather than riding on `L`'s: it is the character most likely to be dropped from the scan, because the name trap below makes it the awkward one. A test suite that only covers the rejections would pass an implementation that rejects every name-bearing pattern.

**The `@nickname` shorthands are accepted — the vixie set, and only that set (the maintainer, 2026-08-08).** Seven spellings, and shep expands each one itself, in the pre-parse pass, before the pattern reaches croner:

| Nickname | Expands to | Meaning |
|---|---|---|
| `@yearly` | `0 0 1 1 *` | midnight, 1 January |
| `@annually` | `0 0 1 1 *` | the same schedule, the other spelling |
| `@monthly` | `0 0 1 * *` | midnight on the 1st |
| `@weekly` | `0 0 * * 0` | midnight on Sunday |
| `@daily` | `0 0 * * *` | midnight |
| `@midnight` | `0 0 * * *` | the same schedule, the other spelling |
| `@hourly` | `0 * * * *` | on the hour |

Matching is ASCII-case-insensitive, which is not a widening but the same rule the pass already applies to the three-letter day and month names below it. `@DAILY` and `@daily` are one spelling, and a test row pins that so the choice cannot drift.

**Why accepting these does not reopen the five-field decision.** That decision was made to avoid an open-ended commitment: croner's `L`, `W` and `#` extensions are the crate's own dialect and grow with it, so accepting them would bind shep's config grammar to somebody else's roadmap. The vixie set has the opposite shape — it is closed, it has been these seven spellings for decades, and accepting it commits shep to nothing further. And because we expand rather than delegate, the settled grammar stays literally five-field: croner never sees a nickname, so the accepted-pattern surface is exactly the five-field one plus seven names that are textual sugar for members of it.

**Expanding it ourselves is load-bearing, not a stylistic preference, and the reason is a verified gap in croner's own table.** croner's `handle_nicknames` (`croner-3.0.1/src/parser.rs:276-280`) has arms for `@yearly`/`@annually`, `@monthly`, `@weekly`, `@daily` and `@hourly` — and **no arm for `@midnight`**. An unmatched `@`-token falls through its `_ => pattern` arm unchanged, arrives at the field-count gate as one token, and is rejected. So delegating to croner would ship an incoherent subset of a closed set: `@daily` accepted, `@midnight` rejected, with a message about field counts. Expanding first also keeps the mapping ours rather than a private implementation detail croner's next version can widen underneath us — the same reasoning that keeps `croner::Cron` out of `CronSchedule`'s public signature (IR-11).

**`@reboot` is rejected, with its own message.** It means "run once at boot", which has no coherent meaning for a supervisor that already owns restart policy: shep decides when a sheep starts, and a pattern asking for one start at machine boot is asking the cron worker to do the thing the engine is for. The message says that, and names `@reboot`.

**A correction to the compile check's note, because the plan's report obligations track exactly this class.** That note read croner as accepting every nickname it was handed and concluded that rejecting `@reboot` was our job rather than croner's, by analogy with `L`/`W`/`#`/`?`. The three nicknames it ran were all in croner's table; `@reboot` is not, so croner *does* reject it — but with `InvalidPattern("Pattern must have 5 fields when seconds are disallowed.")`, which is a misleading sentence to hand a user who wrote a real cron nickname. What is ours is the *message*, not the rejection. Do not write a test row asserting that croner would have accepted it.

**Ordering in the pre-parse pass, which the extension-character scan constrains:** nickname expansion runs first, on the whole trimmed pattern, and only when it is a single token beginning with `@`. An `@`-leading token that is not one of the seven is rejected — `@reboot` with its own sentence, anything else naming the unrecognised nickname. Everything that survives is five-field text, and the seven expansions contain no `L`, `W`, `#` or `?`, so the character scan below runs unchanged on the expanded form. `@` therefore leaves the character scan's reject list entirely: it is handled by the table, not by a character test.

Note what this fixes in the existing stopgap (`crates/shep-core/src/config/normalize.rs:54-60`), which is a bare `pattern.split_whitespace().count() != 5`: it accepts any five whitespace-separated tokens — `99 99 99 99 99` passes today and croner rejects it with `ComponentError("Number out of bounds.")` — while rejecting everything that is not exactly five tokens, the nicknames included. It is a token counter, not a grammar.

`docs/migration.md` gets three lines, and they are three because the errors read differently. A pm2 user whose pattern used `L`, `W`, `#` or `?` now gets a config error naming the offending character. A user whose pattern carried a seconds field gets one naming the field count instead — croner's own message there is about the shape of the pattern, not about a character, and promising otherwise would be a promise the implementation cannot keep. A user on `@reboot` gets the third line: the nickname is recognised and refused on purpose, and the fix is to let shep's own restart policy do the job.

- [ ] **Step 1: Add the three dependencies**

Root `Cargo.toml`, in `[workspace.dependencies]`:

```toml
# The cron_restart dialect (spec §4). `serde` is croner's only feature and we
# do not serialize a parsed Cron — the Flockfile carries the pattern string.
croner = { version = "3.0.1", default-features = false }
# croner is generic over chrono::TimeZone; `clock` is what provides Utc::now(),
# which the daemon's cron worker reads to derive the next occurrence.
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
# Resolves cron_timezone's IANA name to a chrono::TimeZone. croner carries this
# only as a dev-dependency, so it is ours to declare.
chrono-tz = { version = "0.10.4", default-features = false, features = ["std"] }
```

`crates/shep-core/Cargo.toml` opts all three in with `.workspace = true`.

**The chrono entry is `"0.4"`, not the `0.4.42` an earlier draft pinned, and the floor pin was dropped because it is provably redundant.** croner 3.0.1 declares `chrono = "0.4.42"` itself (`croner-3.0.1/Cargo.toml`, the `[dependencies.chrono]` block — read off the vendored manifest, not the research note), and shep-core depends on croner and chrono together, so cargo unifies to the higher of the two floors whatever we write. `-Z minimal-versions` therefore resolves 0.4.42 through croner with no help from us. Nothing this phase writes needs a chrono API newer than `DateTime`, `TimeZone` and `Duration::from_std`, all of which long predate 0.4.42, so a floor pin here could not carry the comment naming an API that this manifest's other pins carry — which is the tell that it was never ours to make. Run the rehearsal anyway and confirm 0.4.42 comes out; if some other floor appears, it goes in with a comment naming the API that forced it.

For completeness, chrono-tz 0.10.4 declares only `chrono = "0.4.25"` (its own vendored manifest), so it never raises the floor and is not the reason for anything here.

- [ ] **Step 2: Write `cron.rs` with its pinned-array tests first (TDD)**

The tests are the interesting half of this task, because `next_after` is a pure function of `(pattern, zone, instant)` and therefore admits exact expected values. Required cases, each asserting a **pinned array** of successive occurrences (IR-36) rather than a single "is it after now" property:

| Case | Pattern | Zone | The broken implementation it catches |
|---|---|---|---|
| five-field baseline | `0 3 * * *` | none (UTC) | a parser configured with `Seconds::Required`, which would reject it |
| six-field rejected | `30 0 3 * * *` | none | expects `Err(CronParseError::Pattern)`. Fails if the builder was left on croner's default `Seconds::Optional` (`croner-3.0.1/src/parser.rs:56`), which accepts the seconds field and would ship the wide dialect by accident |
| year form rejected | `0 3 * * * 2027` | none | expects `Err(CronParseError::Pattern)`. Fails if somebody "simplified away" the `.seconds(Seconds::Disallowed)` call believing croner's `Year` default needed its own knob — this row is what documents that one setting closes both widenings |
| nicknames expand | each of the seven in the table above | none (UTC) | the pinned occurrence array for a nickname equals the array its stated five-field equivalent produces, asserted pair by pair rather than against a hand-written instant. Fails if any expansion is wrong — `@weekly` as `0 0 * * 1` is the likely slip — and fails if `@midnight` was left to croner, whose table has no arm for it |
| nickname case | `@DAILY` | none | the same schedule as `@daily`. Fails if the table is matched with `==` rather than an ASCII-case-insensitive compare, which would turn one of the two spellings into an unrecognised nickname |
| nickname keeps its spelling | `@daily` | none | `pattern()` returns `@daily`, not `0 0 * * *`. Fails if the expansion is stored in place of the user's own text — the same defect fact 4 below describes for `Cron::as_str` |
| `@reboot` rejected | `@reboot` | none | expects `Err(CronParseError::Pattern)` whose `pattern` field is `@reboot`. Fails if the nickname handling is a permissive "leading `@`, not obviously malformed" check rather than a closed table |
| unknown nickname rejected | `@fortnightly` | none | expects `Err(CronParseError::Pattern)`. Fails if an unrecognised `@`-token is handed to croner anyway, which rejects it with a field-count sentence that tells the user nothing about nicknames |
| zone offset | `0 3 * * *` | `Europe/Oslo` | a `next_after` that ignores the zone and returns 03:00 UTC — the two answers differ by one or two hours, and the assertion is on the exact UTC instant |
| zone across midnight | `30 23 * * *` | `Pacific/Auckland` | the same defect, in the case where the local date and the UTC date disagree |
| spring-forward gap | `30 2 * * *` | `America/New_York`, across 2026-03-08 | a fixed-time job silently skipping the day it lands in the gap. croner's documented rule fires it at the first valid instant after the gap |
| spring-forward wildcard | `*/15 * * * *` | `America/New_York`, across 2026-03-08 | an interval job that fires the gap's nominal occurrences anyway; the correct sequence skips them and resumes on the new wall clock |
| fall-back single fire | `30 1 * * *` | `America/New_York`, across 2026-11-01 | a double fire across the repeated hour |
| never matches | `0 0 30 2 *` | none | mapping every `CronError` to `Err`, which loses the `Ok(None)` this case must produce |
| bad pattern | `not a cron` | none | the reverse: mapping a genuine parse failure into `Ok(None)` |
| five tokens of garbage | `99 99 99 99 99` | none | the whole reason the stopgap is being replaced: it counts tokens, so this passes today. Fails if the new validator is still a token count |
| bad zone | `0 3 * * *` | `Mars/Olympus` | a `parse` that accepts any string and only fails later at scheduling time |

The three DST rows are five-field on purpose and their pinned instants are unaffected by that: croner inserts a literal `0` seconds component for any five-token pattern (`parser.rs:126-127`), so `30 2 * * *` normalizes to exactly the same schedule a six-field `0 30 2 * * *` would have described. The DST scenarios each row exists to pin survive the spelling unchanged.

**A rejection row asserts croner's sentence only when the sentence is ours.** croner renders "Pattern must have 5 fields when seconds are disallowed." for the six-field row, the year row *and* the `not a cron` row — three tokens, so the same field-count gate catches it — and "Number out of bounds." for the garbage row. Those four assert the **variant** and nothing else: the wording is croner's to reword and pinning it makes a patch bump a red suite. The rows the pre-parse pass rejects are the other case — the four extension characters, `@reboot` and the unknown nickname never reach croner at all, so their text is ours and asserting it is asserting our own contract.

The DST rows come from croner's documented `JobType` behaviour, and the whole point of pinning them is that this plan is not the authority on them — croner is. **Derive the expected values by running the case, read the result, and then decide whether it matches croner's documented rule before pinning it.** If it does not, that is a finding for the report, not a number to paste in. This applies to the accepted rows only: a rejection row that fails to parse is the assertion passing, not a croner defect to report.

Each case gets the one-line "fails if …" comment rule 5 requires; the table's last column is that comment.

- [ ] **Step 3: Replace the stopgap in `normalize.rs`**

Delete lines 54-60 — the field-count check and its `ponytail:` note — and put a real parse in their place. `NormalizeError` gains one variant and `InvalidCron` changes shape:

```rust
/// `cron_restart` is not valid in croner's dialect. Carries the pattern and
/// croner's own reason.
InvalidCron { pattern: String, reason: String },
/// `cron_timezone` is not a name in the IANA time-zone database.
InvalidTimezone { name: String },
```

Changing `InvalidCron(String)` into a struct variant breaks its existing test (`normalize.rs:163-171`) and its `Display` arm (`:112`); update both. The existing test's input `"not a cron"` stays rejected, so the test's *intent* survives — only its pattern-match changes.

Its "fails if" comment cannot become "fails if the validator accepts any string with five tokens", though, because `not a cron` is **three** tokens and the stopgap rejects it already. That test guards nothing new, which is exactly the rule-5 failure mode this plan exists to avoid. Give it the honest comment — fails if the reason is not carried through from croner — and add a second case alongside it whose input is five tokens of garbage (`99 99 99 99 99`), which the stopgap accepts today and croner rejects with `ComponentError("Number out of bounds.")`. That second case is the one that proves the stopgap is gone.

Update the `# Errors` doc on `normalize` (`:36-40`), which currently promises "`cron_restart` is not a 5-field pattern".

**Validate the timezone even when `cron_restart` is absent.** A Flockfile with `cron_timezone = "Mars/Olympus"` and no pattern is a typo the user wants to hear about, and spec §5's rule is that typos fail loudly. The cost is one extra branch.

- [ ] **Step 4: Run the `-Z minimal-versions` rehearsal**

```
cargo +nightly generate-lockfile -Z minimal-versions
cargo +stable test --workspace
git checkout Cargo.lock
```

Any floor this exposes goes in `[workspace.dependencies]` with a comment naming the API, matching the block already in the root manifest. Restore `Cargo.lock` afterwards — the rehearsal's lockfile is not the one that gets committed.

Then the MSRV check from Global Constraints, from its own exit code — croner declares no `rust-version` at all and chrono-tz 0.10.4 declares 1.65, so this should pass, and running it is what makes that a checked fact rather than a hopeful one:

```
rustup toolchain install 1.88 --profile minimal
cargo +1.88 check --workspace --all-features --locked
```

- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 6: CHANGELOG** — `shep-core`, under Additions and Changes.

The dialect change is user-visible and it moves in **both** directions. An earlier draft of this plan said it moved in one and told the writer not to claim otherwise; the settled nickname decision made that instruction false, and the same sentence has to be right here and in Task 14's changelog pass.

*Changes* — the tightening: patterns the stopgap accepted purely on token count (`99 99 99 99 99`) now fail with croner's own reason, and `L`, `W`, `#` and `?` are newly rejected with the offending character named. Six-field and seconds-bearing patterns were already rejected by the token count and stay rejected; the error now says why instead of saying "not a 5-field pattern". `@reboot` also stays rejected, with a message about what it means rather than one about field counts. `0 0 * JUL WED` and `0 0 * * MON-FRI` keep working.

*Additions* — the loosening, and it is exactly seven spellings: `@yearly`, `@annually`, `@monthly`, `@weekly`, `@daily`, `@midnight` and `@hourly` are accepted where the token-count stopgap rejected them, each expanded to the five-field pattern this task's table names. List the expansions in the entry; a user upgrading wants to know what `@weekly` means here without reading the source.
- [ ] **Step 7: Commit** — `feat(core): validate cron_restart against the croner dialect`

---

### Task 2: `ProbeTarget` — parsing a probe's target once, at config time

**Files:**
- Create: `crates/shep-core/src/config/probe.rs` — pure tier
- Modify: `crates/shep-core/src/config/mod.rs` — pure
- Modify: `crates/shep-core/src/config/normalize.rs` — pure
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces:**
- Consumes: `crate::config::app::{ProbeConfig, ProbeKind}` (`crates/shep-core/src/config/app.rs:15`, `:28`)
- Produces:

```rust
/// A probe's `target` after validation — the form the prober consumes.
///
/// Parsing here rather than in the daemon means a malformed target fails the
/// Flockfile, not the first poll ten seconds after the sheep is online.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTarget {
    /// `http://host[:port]/path` — port defaults to 80, path to `/`.
    Http { host: String, port: u16, path: String },
    /// `host:port`.
    Tcp { host: String, port: u16 },
    /// A command line, run through the platform shell.
    Exec { command: String },
}

impl ProbeTarget {
    /// Parses `config.target` according to `config.kind`.
    ///
    /// # Errors
    ///
    /// - [`ProbeTargetError::Empty`] — the target is empty or all whitespace.
    /// - [`ProbeTargetError::HttpsUnsupported`] — an `https://` URL.
    /// - [`ProbeTargetError::NotHttpUrl`] — no `http://` scheme.
    /// - [`ProbeTargetError::MissingHost`] — the authority has no host.
    /// - [`ProbeTargetError::MissingPort`] — a TCP target with no `:port`.
    /// - [`ProbeTargetError::BadPort`] — the port is not a `u16`.
    pub fn parse(config: &ProbeConfig) -> Result<Self, ProbeTargetError>;
}

/// Why a probe target was rejected.
///
/// Growth is expected: a future `https` probe removes one variant's reason for
/// existing and a unix-socket probe would add several (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTargetError {
    /// The target is empty or all whitespace.
    Empty,
    /// An `https://` URL. TLS probe targets are not supported; carries the target.
    HttpsUnsupported { target: String },
    /// An HTTP probe target with no `http://` scheme; carries the target.
    NotHttpUrl { target: String },
    /// The authority is empty — `http:///path`; carries the target.
    MissingHost { target: String },
    /// A TCP target with no `:port`; carries the target.
    MissingPort { target: String },
    /// The port is not a `u16`; carries the target.
    BadPort { target: String },
}
```

Each variant is spelled out rather than left as prose because the `# Errors`
list above links to all six by name, and those intra-doc links have to resolve
under the mandated `RUSTDOCFLAGS="-Dwarnings"` gate. The payload is uniform —
the offending target, owned — except `Empty`, which has nothing to carry.

**No URL crate.** The grammar spec §7 needs is `http://host[:port][/path]` and nothing more — no userinfo, no query, no fragment, no IDN, no percent-decoding. `url` 2.x would pull `idna` and its Unicode tables into a daemon whose stated goal is single-digit-MB RSS (spec §14.11), to reject strings a twenty-line `split` already rejects. Write the twenty lines and pin them with the boundary sweep below.

**`https://` gets its own error variant, and that is the honest half of decision D1.** The HTTP prober this phase builds is a hand-rolled HTTP/1.1 client over `TcpStream` (Task 8) with no TLS, and a probe that silently fails every poll would look exactly like an app that is down. Rejecting the config instead means the user learns at `shep start` rather than at 03:00. The message says what is true — that TLS probe targets are not supported — and does not promise a version.

- [ ] **Step 1: Write the boundary sweep first (IR-40), then the parser**

Required accepted inputs: `http://127.0.0.1:8080/healthz`, `http://localhost/` (port defaults to 80, path `/`), `http://localhost:3000` (path defaults to `/`), `http://[::1]:8080/x` — *or* an explicit rejection of the bracketed-IPv6 form with its own variant, decided by the implementer and stated in the report. Do not leave it accepted-but-mis-parsed: `[::1]:8080` split on the last `:` yields a host of `[::1]` and a port of `8080` only if the split is written to look for the bracket, and split on the *first* `:` yields nonsense.

Required rejected inputs, each asserting the exact variant: `""`, `"   "`, `https://x/`, `x/`, `ftp://x/`, `http:///path` (empty host), `http://host:notaport/`, `http://host:99999/` (out of `u16`), and for TCP: `"host"` (no port), `"host:"`, `":8080"`.

For `Exec`: only emptiness is rejected. A command line is whatever the shell will take, and narrowing it further would be the "widening or narrowing the input grammar beyond spec" drift CLAUDE.md names as a top risk — in the narrowing direction.

- [ ] **Step 2: Call it from `normalize`**

`normalize` (`crates/shep-core/src/config/normalize.rs:41`) validates `readiness_probe` and `liveness_probe` when present, discarding the parsed value — its job is rejection, and the daemon re-parses when it arms the probe. `NormalizeError` gains one variant:

```rust
/// A `readiness_probe` or `liveness_probe` target is malformed. Carries which
/// probe and the rendered reason.
InvalidProbe { probe: &'static str, reason: String },
```

`probe` is `"readiness_probe"` or `"liveness_probe"` — the Flockfile field name, so the error names the line the user has to edit. A `&'static str` keeps the enum's `Clone`/`Eq` derives intact.

Also reject `failure_threshold == 0`, with its own `NormalizeError` variant. A threshold of zero means "unhealthy before the first probe runs", which is not a configuration anybody wants and which would make the liveness loop restart the sheep immediately and forever. `ProbeConfig::failure_threshold` defaults to 3 (`crates/shep-core/src/config/app.rs:41`), so this only fires on an explicit `0`.

**And reject `watch = true` with no `cwd`, with a third variant (the maintainer, 2026-08-08).** This one is not about probes at all; it lands here because Task 2 owns `normalize`'s new variants and because it is what makes Task 12's watch-root derivation total.

```rust
/// `watch` is enabled but the app sets no `cwd`, so there is no directory to
/// watch. Carries the app name.
WatchWithoutCwd { name: String },
```

Both alternatives were considered and both are worse. **Defaulting to the daemon's own cwd is the dangerous one.** `AppConfig::cwd` defaults to `None` (`crates/shep-core/src/config/app.rs:78`, `:164`) and its doc calls the default "the daemon's cwd at spawn registration" — but nothing in this workspace ever chdirs (`grep -rn "set_current_dir\|chdir" crates/` is empty), so the shepherd's cwd is whichever directory the user first ran `shep start` from. Commonly `$HOME`; under a systemd unit with no `WorkingDirectory=`, it is `/`, and shep would arm a recursive watch over the entire filesystem. Short of that it exhausts Linux's default `max_user_watches` and turns unrelated churn into flock-wide restarts, on a machine where nothing in the Flockfile asked for it. A watch root must come from the Flockfile, never from invocation history.

**Arming nothing with a warning is the quieter one, and it is the silent-failure shape this project keeps rooting out.** It defers the surprise to the moment the user wonders why saving a file does nothing — the same failure mode this plan rejects for `https://` probe targets, where a silently-failing probe is indistinguishable from a down app. A config that asks for a feature and gets none of it should say so at `shep start`.

`watch` is a plain `bool` defaulting to `false` (`app.rs:112`, `:181`), so this fires only on an app that explicitly asked to be watched. An app with `watch_options` set but `watch = false` is left alone and that silence is deliberate: nothing arms, so nothing needs a root, and rejecting there would fail a Flockfile that is merely carrying options for a flag the user has not turned on yet.

Three test rows, and the third is the one that catches an over-broad rejection:

| Case | The broken implementation it catches |
|---|---|
| `watch = true`, no `cwd` → `Err(NormalizeError::WatchWithoutCwd)` carrying the app name | a validator that never looks at `watch`, or one that carries no name and so cannot tell the user which app to edit |
| `watch = true` with a `cwd` → `Ok` | a check that fires on `watch` alone |
| `watch = false`, `watch_options` set, no `cwd` → `Ok` | a check keyed on `watch_options` being non-empty rather than on `watch` being true, which would reject a Flockfile that asked for nothing |

All three of this task's new variants go into `normalize`'s `# Errors` list (`crates/shep-core/src/config/normalize.rs:36-40`) alongside the two Task 1 adds. That list is the one place a caller learns what rejection looks like, and it is currently four lines describing a validator that is about to grow to nine.

- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: CHANGELOG** — `shep-core`, Additions.
- [ ] **Step 5: Commit** — `feat(core): parse and validate probe targets at config time`

---

### Task 3: The `Clock` seam, the cron worker, and its configurable sleep bound

**Files:**
- Create: `crates/shep-daemon/src/cron.rs` — pure tier
- Create: `crates/shep-daemon/src/testing.rs` — pure tier (the inline `mod testing` in `lib.rs`, moved verbatim, plus `TestClock`)
- Modify: `crates/shep-daemon/src/lib.rs` — pure (module declarations; the inline `mod testing { ... }` block becomes `#[cfg(test)] pub(crate) mod testing;`)
- Modify: `crates/shep-daemon/Cargo.toml` — chrono opt-in
- Modify: `crates/shep-core/src/config/daemon.rs` — pure tier (the `max_cron_sleep` knob, its env override, and its floor)

**Interfaces:**
- Consumes: `shep_core::config::CronSchedule`, `shep_core::selector::ProcessSelector::Name` (`crates/shep-core/src/selector.rs:15`), `crate::supervisor::SupervisorHandle::restart` (`crates/shep-daemon/src/supervisor.rs:210`), `shep_core::values::UpDuration` (`crates/shep-core/src/values.rs:160`) — already in `shep-core`, so the config half adds no dependency and runs no rehearsal
- Produces:

```rust
/// Wall-clock reader.
///
/// Cron means wall time — 03:00 in a named zone — while every other deadline
/// in this engine is a `tokio::time::Instant` that `start_paused` can move.
/// The two cannot be the same clock, so this is the seam that lets a paused
/// test drive a cron schedule.
pub trait Clock: Send + Sync + 'static {
    /// The current instant in UTC.
    fn now_utc(&self) -> DateTime<Utc>;
}

/// `Clock` over the real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

/// Longest a cron worker sleeps before re-deriving its next occurrence, when
/// `shep.toml` names no `max_cron_sleep`.
///
/// A single `sleep_until(next)` is wrong across a laptop suspend, an NTP step
/// or a DST wall-clock shift: the sleep was computed against a wall time that
/// no longer holds, and the job fires late by however far the clock moved.
/// Re-deriving at least this often bounds that error to one minute at the cost
/// of one wakeup per minute per cron-configured sheep.
pub(crate) const DEFAULT_MAX_CRON_SLEEP: Duration = Duration::from_secs(60);

/// Runs one sheep-group's cron schedule until the handle is dropped.
///
/// `max_sleep` bounds how long the loop parks before it re-reads the clock;
/// it changes how quickly the worker recovers from a wall-clock jump, never
/// whether an occurrence fires.
///
/// Cancellation: the returned handle aborts the loop on `abort()`; the loop
/// itself holds no state that needs unwinding.
pub fn spawn_cron_worker(
    name: String,
    schedule: CronSchedule,
    clock: Arc<dyn Clock>,
    supervisor: SupervisorHandle,
    max_sleep: Duration,
) -> tokio::task::JoinHandle<()>;
```

And in `shep-core`, the knob itself. The field goes **last** in `DaemonSection`, so the derived `Debug` gains a trailing field rather than reordering the three an exact-string test already pins; and `BelowMinimum` is a struct variant rather than a third tuple form in `DaemonConfigError`, because three positional fields stop being readable — the shape `BootError::Io { path, source }` already uses:

```rust
/// Longest a cron worker sleeps before re-deriving its next occurrence.
///
/// Shorter recovers faster from a suspended laptop or an NTP step and costs
/// proportionally more wakeups per cron-configured sheep; longer is cheaper
/// and drifts further. Unset means the daemon's own default. There is no
/// upper bound: a very long value only degrades to sleeping straight through
/// to the occurrence, which still fires.
pub max_cron_sleep: Option<UpDuration>,

/// Floor on `[daemon] max_cron_sleep`.
///
/// Zero makes every sleep return immediately and turns the loop into a hot
/// spin that re-derives a schedule as fast as the runtime allows — while
/// still firing correctly, which is what makes it hard to attribute. Low
/// milliseconds are the same fault with a smaller constant. One second is a
/// floor no legitimate configuration wants to be under: a five-field cron
/// pattern cannot name anything finer than a minute, so even this is
/// sixty times more often than the tightest schedule can fire.
const MIN_CRON_SLEEP: UpDuration = UpDuration::from_millis(1_000);

/// A `[daemon]` duration is below the floor that keeps the daemon from
/// spinning. Carries the key the user actually set — the TOML key or the
/// environment variable, whichever supplied the winning value.
BelowMinimum {
    /// `max_cron_sleep` or `SHEP_MAX_CRON_SLEEP`.
    key: &'static str,
    /// The value as the user wrote it.
    value: UpDuration,
    /// The floor it failed.
    min: UpDuration,
},
```

**The loop shape, and the two ways to get it wrong:**

```
loop {
    now = clock.now_utc()
    next = match schedule.next_after(now) {
        Ok(Some(next)) => next,
        Ok(None)  => { info!(...);  return }   // never fires again
        Err(err)  => { warn!(%err); return }   // cannot resolve; retrying would spin
    }
    sleep(min(next - now, max_sleep))          // saturating: a negative delta is zero
    if clock.now_utc() >= next {
        if let Err(err) = supervisor.restart(ProcessSelector::Name(name.clone())).await {
            // NotFound: the sheep is gone but the registry has not disarmed us
            // yet — log at debug and keep the schedule. EngineStopped: the
            // actor is gone, so end the task.
            match err {
                NotFound       => debug!(...),
                SpawnFailed(_) => warn!(%err),          // this occurrence lost; keep the schedule
                EngineStopped  => { warn!(%err); return }
            }
        }
    }
}
```

Three things in that shape are load-bearing against `-D warnings` rather than stylistic. The `?` this replaces would not compile: the body returns `()`, so `Result`'s `From` conversion has nothing to convert into — and the plan's own prose two paragraphs down says the `Err` case logs and ends the task, which is a `match`, not a `?`. And `SupervisorHandle::restart` returns `Result<Vec<ProcessInfo>, SupervisorError>` (`crates/shep-daemon/src/supervisor.rs:210-213`), which is `#[must_use]` through `Result`: discarding it is `unused_must_use`, a hard error under the gate. Discarding it is also how a worker ends up looping forever against a dead actor with nothing in the log.

The third is the arm count. `SupervisorError` has **three** variants — `NotFound`, `SpawnFailed(String)`, `EngineStopped` (`supervisor.rs:138-146`) — and carries no `#[non_exhaustive]`, so a two-arm match is `E0004` and not a stylistic omission. `SpawnFailed` is the one an implementer forgets, and it is reachable: `restart` on a live sheep runs the kill ladder and respawns, and that respawn can fail. It is a lost occurrence, not a dead engine, so the schedule stands.

**The plain `restart` there is the settled behaviour, and the watch group's is identical.** Both reach every instance of the name, stopped ones included, because `ProcessSelector::Name` matches on the name alone. An earlier draft of this plan gave watch a narrower reach and left cron's open; the maintainer closed both the same way on 2026-08-08, so there is no asymmetry left to preserve and no scope parameter to thread. What actually keeps a stopped sheep down is disarming: a sheep with no armed cron worker and no armed watcher cannot be reached by either, and Task 12 disarms both as the last instance of a name leaves. An implementer who reintroduces a status filter here is reopening a settled question.

1. **The `if` after the sleep is not optional.** Without it, a capped sleep that expires before `next` fires the job early, every minute, forever. With it, a capped sleep that expires early simply loops and re-derives.
2. **Missed occurrences are not replayed.** A daemon that was suspended for six hours with an hourly cron wakes to a `next_after(now)` that is one occurrence in the future, and the sheep restarts at most once. Firing the six missed occurrences would be a restart storm; the loop's structure gives the at-most-one behaviour for free, and the reason belongs in an IR-31 `//` comment so nobody "fixes" it into a catch-up loop.

`next_after` returning `Ok(None)` — a pattern that can never fire again — logs at `info` and ends the task. Returning `Err` logs at `warn` and also ends the task: a schedule that cannot resolve its own next occurrence will not start resolving it later, and a loop that retries would spin.

**The sleep bound is a daemon-config knob, and its default stays 60s (the maintainer, 2026-08-08).** The number was never wrong, it was unjustifiable: sixty seconds is the drift a suspended laptop or an NTP step costs, and a dev machine suspending is the common case rather than the exotic one, so bounding the error at one minute is the right thing to pick for someone who has not thought about it. What a hard `const` could not do is let the person who *has* thought about it choose differently — a server that never suspends and runs a thousand cron-configured flock members wants fewer wakeups, and a laptop that suspends hourly wants faster recovery. Both are one line of `shep.toml` now.

**Where it lives, and the layering it inherits.** `DaemonSection` (`crates/shep-core/src/config/daemon.rs:15`) gains `max_cron_sleep: Option<UpDuration>`, declared after `enabled_dogs`. `Option`, not a serde default, and the reason is the precedent already in the same struct: `socket: Option<PathBuf>` (`:19`) carries "unset" all the way through to the one place that knows the default, and that is what keeps file < env < flags honest — a layer that has to write a value in order to say nothing cannot be distinguished from a layer that meant that value. So the default is applied exactly once, in `boot.rs`, and `DEFAULT_MAX_CRON_SLEEP` never appears in `shep-core`.

`UpDuration` is the type because it is already the Flockfile's duration grammar and already `Deserialize` (`crates/shep-core/src/values.rs:246-252`), so `max_cron_sleep = "5m"` parses with no new code. It is also where the sharpest footgun lives: **plain digits are milliseconds** (`values.rs:147`), so `max_cron_sleep = "60"` from a user who meant sixty seconds is sixty milliseconds.

**The environment override is `SHEP_MAX_CRON_SLEEP`.** The two `[daemon]` keys that have an override today are `log_json` and `socket`, spelled `SHEP_LOG_JSON` and `SHEP_SOCKET` — `SHEP_` plus the screaming-snake form of the key, with no exception to work around. Read it in `DaemonConfig::load` beside the other two (`:73-82`), parse it with `UpDuration`'s `FromStr`, and on failure return the existing `DaemonConfigError::BadEnvValue("SHEP_MAX_CRON_SLEEP", v)` (`:93`) — no new variant, because that is precisely the fault it already names. A bad value in the *file* needs no new code either: `UpDuration`'s `Deserialize` routes its parse error through `serde::de::Error::custom`, so `toml::from_str` fails and `load` already maps that to `DaemonConfigError::Toml`. Do not add a second check for it.

**The floor is one second, and a value below it is rejected rather than clamped.** `MIN_CRON_SLEEP` is `UpDuration::from_millis(1_000)`; anything strictly below it fails `load` with the new `BelowMinimum` variant, which reaches the user as exit code 4 through the mapping that already exists (`crates/shep-cli/src/commands/daemon.rs:149`). Five things make that the right call rather than a harsh one:

- **Zero is expressible and is a pure spin.** `max_cron_sleep = "0"` makes every sleep `Duration::ZERO`, and the loop then re-derives a next occurrence as fast as the runtime will let it — burning a core per cron-configured sheep while *still firing correctly*, because the `if clock.now_utc() >= next` guard holds. A fault with no wrong output is the one nobody attributes.
- **One second is not an arbitrary line.** The settled five-field dialect cannot express anything finer than a minute, so a one-second bound already re-derives sixty times per shortest-possible occurrence. Nothing legitimate lives below it, which is what makes rejection cheap: no real configuration is being refused.
- **Rejecting is what this file already does with a bad `shep.toml`.** A daemon that will not boot on malformed TOML but silently repairs a value that would peg a core is inconsistent in the direction that hurts.
- **Clamping is the silent-failure shape** this plan rejects twice already, for `watch = true` with no `cwd` and for `https://` probe targets. It would be quieter here than in either of those, because the daemon runs detached with its stderr redirected to `shepd.err.log` (`crates/shep-cli/src/launch.rs:74`) — a `warn!` about a clamped knob lands in a file nobody opens.
- **The loud path is genuinely loud enough.** The child exits before it answers, and `connect_or_spawn` treats any exit other than "already running" as immediately fatal rather than burning its deadline (`crates/shep-client/src/spawn.rs:307-319`), so the user's next `shep` command fails at once with `the daemon process exited before it started answering: exit status: 4`. The sentence naming the key and the floor is in `shepd.err.log`; the failure is not.

**There is no upper bound, and that is deliberate.** A very large `max_cron_sleep` degrades to the plain `sleep_until(next)` this const exists to avoid, which is a worse *recovery* story and not a correctness problem — every occurrence still fires, just late after a clock jump. Bounding a knob whose only failure mode is the behaviour we had before it existed would be ceremony. Say so in the field's doc so the asymmetry does not read as an oversight.

**There is no CLI flag either.** Spec §5's third layer is real but `DaemonArgs` carries exactly one flag today (`crates/shep-cli/src/cli.rs:146-150`), and `shep daemon` is the hidden re-exec target `launch_daemon` spawns — never typed by a person, so a flag on it would be unreachable in practice. A flag would also need its own copy of the floor check, since it lands *after* `DaemonConfig::load` has run. One validation gate, one way in.

**Two existing items go stale the moment the field lands, and both are exact-match:**

- `debug_redacts_dog_values` (`crates/shep-core/src/config/daemon.rs:169-179`) pins the whole `Debug` string. Declaring the field last means the literal gains a trailing `, max_cron_sleep: None` and nothing reorders. Update the literal — do **not** loosen the assertion into a `contains`, which is the entire point of IR-41's exact-string rule.
- `DaemonConfig::load`'s `# Errors` section (`:56-60`) names two variants and one env var. It gains `BelowMinimum` and `SHEP_MAX_CRON_SLEEP`, and so does `run_daemon`'s doc in the CLI (`crates/shep-cli/src/commands/daemon.rs:92-94`), which currently says the child reads `SHEP_LOG_JSON` and `SHEP_SOCKET` and would otherwise be a two-thirds-true list.

**Validate the effective value once, after layering, and name the layer that supplied it.** Provenance needs no tracking: if the env var was present it won, and the key is `"SHEP_MAX_CRON_SLEEP"`; otherwise the value came from the file and the key is `"max_cron_sleep"`. One `BelowMinimum` check on the winner. Validating each layer as it is read is the tempting alternative and it is worse: it makes a good `SHEP_MAX_CRON_SLEEP` unable to rescue a broken `shep.toml`, which is not what "file < env" means.

- [ ] **Step 1: Move `mod testing` into its own file**

`crates/shep-daemon/src/lib.rs` currently declares `#[cfg(test)] pub(crate) mod testing { ... }` inline with the `test_paths` and `harness` helpers and a long comment about `FD_REUSE_LOCK`. Move the whole block, comments included, to `crates/shep-daemon/src/testing.rs` and leave `#[cfg(test)] pub(crate) mod testing;` behind. No item changes name or visibility, so `crate::testing::harness` keeps resolving and no call site moves. Do this as its own commit before anything else in this task, so the diff that adds `TestClock` is readable.

**Then, in a second commit, rewrite the two comments the move carries in.** `lib.rs:215-217` ("Every test mod from Task 3 onward (and the harness in Tasks 4-5)") and `lib.rs:262-263` ("`rpc.rs`'s dispatch tests (Task 4) and the connection-server's tests (Task 5)") are Phase 1-3 provenance notes, and rule 10 forbids task-relative phrasing — carrying them verbatim into a file this phase *creates* would import two violations into new code. Say what they mean without the numbers: "IR-33: one crate-root fixture module; every test module in this crate shares this `test_paths` helper instead of hand-rolling its own" and "IR-33: the dispatch tests and the connection-server's tests need the exact same fixture — one factory, not two." Verbatim first so the move is reviewable, then the rewrite, so neither commit hides the other.

- [ ] **Step 2: Add `TestClock` to `testing.rs`, with the WHY comment IR-33 requires**

```rust
// WHY a clock derived from tokio's Instant: `start_paused = true` freezes
// `tokio::time`, but `chrono::Utc::now()` keeps reading the real system clock.
// A cron test that used the real clock would have to wait real hours. Deriving
// wall time as `epoch + elapsed-since-construction` means `tokio::time::advance`
// moves both clocks by the same amount, and a whole day of schedule fits in a
// test that takes microseconds.
pub(crate) struct TestClock {
    epoch: DateTime<Utc>,
    started: tokio::time::Instant,
    // Counts `now_utc` calls. The only observable difference between two
    // `max_sleep` values is how often the loop wakes, and on a paused clock a
    // wakeup leaves no other trace.
    reads: AtomicUsize,
}
```

`started` is captured with `tokio::time::Instant::now()` at construction, inside the paused runtime. `now_utc` is `epoch + chrono::Duration::from_std(self.started.elapsed())`, and it bumps `reads` with `Ordering::Relaxed` on the way past. The `from_std` conversion is fallible; a test clock cannot plausibly exceed `chrono::Duration`'s range, so saturate rather than panicking — a panicking fixture is a panicking constructor by another name (IR-21).

- [ ] **Step 3: Write the worker's tests, then the worker**

All under `#[tokio::test(start_paused = true)]`. Required cases, each with its "fails if" comment:

| Case | The broken implementation it catches |
|---|---|
| a `0 * * * *` schedule fires at exactly the top of each of three successive hours, asserted as a pinned array of the restarts observed | a loop that fires on the capped sleep instead of the occurrence |
| advancing the clock by 30 seconds at a time across one hour produces exactly one restart | the same defect, in the shape where the cap is shorter than the interval |
| advancing past six occurrences in one jump produces exactly one restart | a catch-up loop replaying the backlog |
| a pattern with no further occurrence ends the task without restarting | a loop that treats `Ok(None)` as "try again" and spins |
| aborting the handle stops the worker — observe one restart first, *then* abort, then advance a further hour and assert no second restart | a worker that outlives its sheep; and, because of the observe-first half, a worker that never fired at all, which would pass a bare "no restart after the abort" |
| a worker built with a ten-minute `max_sleep` and a `0 * * * *` schedule fires exactly once at the top of the hour **and** reads its clock fewer than twenty times doing it | a worker that ignores its `max_sleep` parameter and sleeps `DEFAULT_MAX_CRON_SLEEP` anyway — that path wakes sixty times and reads the clock at least a hundred and twenty |

The clock-read bound is deliberately a bound and not an equality, and rule 7 is satisfied anyway because the two sides are twelve and a hundred and twenty. The loop as written reads twice per iteration; an implementer who carries the post-sleep read forward into the next iteration's `now` gets seven, which is a better loop and must not fail this test. Name the hundred and twenty in the "fails if" comment so the margin is visible to whoever next edits the number.

**Observation is by event subscription, not by `handle.list()`, and the choice is not the implementer's.** Subscribe to the harness's event channel; assert positives with `tokio::time::timeout` naming the restart that did not arrive, and negatives with a bounded `timeout` + `recv` over the span that must stay quiet, per Global Constraints rule 11 — the same discipline Tasks 6, 7 and 11 use. A `list()` read is a poll of a value that changes asynchronously: taken before the worker task has been polled it reports 0 restarts, which passes a negative case against a worker that is merely late and fails a positive one that is merely early. Every negative case in this table is a claim that something did *not* happen, and a poll cannot make that claim. The bounded window is also what crosses the span: the abort row's "advance a further hour" is one `timeout(1h, recv)` that must expire, not sixty stepped `advance` calls, because auto-advance walks the whole hour and takes every wakeup in it — the 59 in rule 11 — for 1.8ms of wall clock.

- [ ] **Step 4: Dyn-compatibility smoke test** — `let _: &dyn Clock = &SystemClock;`, one line, in the module's test block. It fails to compile the moment somebody adds a generic method to `Clock`.

- [ ] **Step 5: The `max_cron_sleep` knob in `shep-core`, as its own commit**

The field, the `SHEP_MAX_CRON_SLEEP` read, `MIN_CRON_SLEEP`, and the `BelowMinimum` variant with its `Display` arm — the whole knob in one diff, separate from the worker's, because the two touch different crates and only one of them is about scheduling.

`DaemonConfigError`'s `Display` matches exhaustively with no `_` arm (`crates/shep-core/src/config/daemon.rs:98-101`), so the compiler demands the new arm rather than letting a variant render as nothing. Word it to the shape already there: `invalid value \`{value}\` for {key}: must be at least {min}`, which reuses the "invalid value … for …" opening `BadEnvValue` uses and adds the one fact the user needs to fix it. `UpDuration`'s own `Display` renders in the largest unit that divides the value exactly (`values.rs:227-238`), so a rejected `"999"` reads back as `999` and the floor as `1s` — the rendering is itself the hint about the grammar.

Cases, in `daemon.rs`'s existing test module, each with its "fails if" comment:

| Case | The broken implementation it catches |
|---|---|
| no `max_cron_sleep` anywhere leaves the field `None` | a serde default that invents 60s in `shep-core` and takes the "unset" state away from the layer below |
| `max_cron_sleep = "5m"` in the file parses to five minutes | a field typed as a bare integer, where `"5m"` is a TOML error and `5` is five milliseconds |
| `SHEP_MAX_CRON_SLEEP=90s` beats a file value of `"5m"` | an env read placed before the file is folded in, or omitted entirely |
| `SHEP_MAX_CRON_SLEEP=banana` is `BadEnvValue("SHEP_MAX_CRON_SLEEP", "banana")` | an env read that swallows its parse failure — `.ok()` and drop it, or an `Err` arm that only logs — leaving the file's value silently in force and the typo invisible |
| `max_cron_sleep = "1s"` is accepted; `"999"` is `BelowMinimum` naming the key `max_cron_sleep` | a floor compared with `>` instead of `>=`, and a check that silently clamps |
| `SHEP_MAX_CRON_SLEEP=0` is `BelowMinimum` naming the key `SHEP_MAX_CRON_SLEEP`, **even when the file carries a valid `"5m"`** | a check that validates the file value and never the override, and one that reports the file's key for a fault the environment introduced |
| `debug_redacts_dog_values` still pins the exact `Debug` string, now ending `, max_cron_sleep: None` | a `contains`-style loosening of the one assertion in this file that exists to catch a leak (IR-41) |

The `"999"` row is the millisecond footgun in test form: it is the value a user writes meaning "just under a second" and also the value they write meaning "999 seconds". Rejecting it is what sends them back to the grammar.

- [ ] **Step 6: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 7: Commit** — `feat(daemon): cron-scheduled restarts with a wall-clock seam` for the worker, and `feat(core): make the cron worker's sleep bound configurable` for Step 5's diff

---

### Task 4: `MemorySampler` and the process-tree sum

**Files:**
- Create: `crates/shep-daemon/src/limits/sample.rs` — pure tier
- Create: `crates/shep-daemon/src/limits/mod.rs` — pure tier (module declaration and docs only at this point; the enforcer lands in Task 6)
- Modify: `crates/shep-daemon/src/lib.rs`, `crates/shep-daemon/src/testing.rs` — pure
- Modify: `Cargo.toml`, `crates/shep-daemon/Cargo.toml` — sysinfo

**Interfaces:**
- Produces:

```rust
/// One process's resident-set reading, with the parent link that lets a caller
/// rebuild the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRss {
    /// The process's own pid.
    pub pid: u32,
    /// Its parent's pid, absent for the roots of the process table.
    pub parent: Option<u32>,
    /// Resident set size in bytes.
    pub bytes: u64,
}

/// Reads the machine's process table.
///
/// One implementation samples the real OS; the scripted one replays a fixture.
/// Sampling is synchronous on purpose: it is a bounded `/proc` walk that runs
/// on the enforcer's own task, and an `async fn` here would make the trait
/// dyn-incompatible for no gain.
pub trait MemorySampler: Send + Sync + 'static {
    /// Every process currently visible to this process's user.
    fn sample(&self) -> Vec<ProcessRss>;
}

/// `MemorySampler` over sysinfo.
pub struct SysinfoSampler { /* Mutex<sysinfo::System> */ }

impl SysinfoSampler {
    /// A sampler holding an empty process table.
    ///
    /// Construction does no refresh: the first [`MemorySampler::sample`] call
    /// performs the first walk, so building one at boot cannot block.
    #[must_use]
    pub fn new() -> Self;
}

// `Default` is not decoration: clippy's `new_without_default` is a `style` lint
// and fires under `-D warnings` the moment a `new()` takes no arguments.
impl Default for SysinfoSampler { /* forwards to new() */ }

/// Sums resident memory over `root` and every descendant in `table`.
///
/// A pid that appears in no reading contributes nothing; a cycle in the parent
/// links (which the kernel does not produce but a fixture can) terminates
/// rather than recursing forever.
#[must_use]
pub fn tree_rss(table: &[ProcessRss], root: u32) -> u64;
```

**Why the tree and not the root pid** — spec §4 defines the process group as the kill unit ("Processes a sheep spawns are its **lambs** … killed with the sheep by the process-group/Job-Object tree kill"). A limit measured on the root pid alone is trivially dodged: a sheep that forks a worker and keeps its own RSS at 20 MB never breaches, while the group holds a gigabyte, and the thing the daemon then kills is the whole group anyway. Measuring what gets killed is the only self-consistent choice. This is a deviation from pm2's single-pid behaviour and is deliberate; it gets a deviation callout in the module docs, in the same voice as spec §4's SIGTERM callout.

**`SysinfoSampler` needs `Mutex<System>`, not `&mut self`.** `System::refresh_processes_specifics` takes `&mut self`, and `MemorySampler::sample` takes `&self` so the sampler can be an `Arc<dyn MemorySampler>` shared by the enforcer and, later, by describe and the metrics dog. A `std::sync::Mutex` is right here rather than a tokio one: the critical section is a synchronous syscall walk, it is never held across an `await`, and a blocking mutex is cheaper. Say so in a `//` comment, because "std mutex in async code" reads like a bug until the reason is written down.

**The sysinfo facts, read off docs.rs on 2026-08-08:**

```rust
pub fn System::new() -> Self;
pub fn System::refresh_processes_specifics(&mut self, processes_to_update: ProcessesToUpdate<'_>,
    remove_dead_processes: bool, refresh_kind: ProcessRefreshKind) -> usize;
pub fn System::processes(&self) -> &HashMap<Pid, Process>;
pub fn ProcessRefreshKind::nothing() -> Self;      // not `new()`
pub fn ProcessRefreshKind::with_memory(self) -> Self;
pub fn Process::memory(&self) -> u64;              // BYTES, and it is RSS
pub fn Process::parent(&self) -> Option<Pid>;
```

Refresh with `ProcessesToUpdate::All` and `remove_dead_processes = true`. **`All` is not an optimisation opportunity.** Refreshing only the pids already known cannot discover a lamb the sheep forked since the last poll, which is precisely the process the tree sum exists to catch.

`sysinfo::Pid` is not `u32` on every platform; convert at the boundary (`pid.as_u32()`) so `ProcessRss` stays a plain shep type and nothing downstream names a sysinfo type.

- [ ] **Step 1: Add sysinfo**

```toml
# Process-table RSS sampling for the polling memory-limit enforcer. Held at the
# 0.38 line: sysinfo 0.39.0+ declares rust-version = "1.95" and this workspace's
# MSRV is 1.88 (Cargo.toml:14, and the 1.88 leg of the CI test matrix). 0.38.4
# carries the identical API this crate uses — refresh_processes_specifics,
# ProcessRefreshKind::nothing/with_memory, Process::memory/parent, Pid::as_u32.
# Defaults bring component/disk/network/user, none of which the daemon reads;
# the `multithread` feature pulls rayon into a daemon whose idle-footprint goal
# is single-digit MB (spec §14.11) and is not enabled.
sysinfo = { version = "0.38.4", default-features = false, features = ["system"] }
```

The pin is the patch, not the minor, for two reasons. The `-Z minimal-versions` rehearsal below resolves `"0.38"` down to 0.38.0, a version nobody has checked; and 0.38.4 is the one whose source and MSRV were read. Do **not** "upgrade" this to the 0.39 line without the maintainer: `cargo +1.88` rejects it outright with `error: rustc 1.88.0 is not supported by the following package`, three CI legs go red, and the fix would be an MSRV bump, which is a user-visible support-window decision and not Task 4's to make in passing. The root manifest's own comment at `Cargo.toml:12-13` calling 1.88 "the floor for ratatui/sysinfo/rmcp in later phases" stays true with 0.38.x pinned — leave it alone.

Run the `-Z minimal-versions` rehearsal (the three-command sequence from Task 1, Step 4), then the MSRV check from Global Constraints, each from its own exit code.

- [ ] **Step 2: `tree_rss` first, against a table fixture — it is pure and it carries the whole correctness claim**

Cases, each with its "fails if" comment: a lone root (sum is its own RSS); root with two children; a three-deep chain; a sibling subtree that must **not** be counted; a root absent from the table (sum is 0, not a panic); a self-parenting pid and a two-cycle (terminates, and the assertion is on the returned sum, not merely on "did not hang" — a `tokio::time::timeout` cannot save a synchronous infinite loop, so the guard has to be in the algorithm: track visited pids).

The sibling-subtree case is the one that catches the most likely wrong implementation — summing every process whose parent chain is non-empty, or summing the whole table.

- [ ] **Step 3: `ScriptedSampler` in `testing.rs`, and `SysinfoSampler`**

`SysinfoSampler` gets exactly one test, and it is a smoke test with a justifying comment (IR-33): sample the real machine, assert the current process's own pid appears with a non-zero `bytes`. That is the only claim about the real OS that is both true everywhere and worth making — anything stronger is a test of sysinfo, not of us. Use `std::process::id()` to know which pid to look for.

- [ ] **Step 4: Dyn-compatibility smoke test, and the `Send + Sync` assertion beside it**

```rust
let _: &dyn MemorySampler = &SysinfoSampler::new();

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SysinfoSampler>();
};
```

`MemorySampler: Send + Sync + 'static` plus `Arc<dyn MemorySampler>` needs `Mutex<System>: Sync`, i.e. `System: Send`. It holds — sysinfo asserts it itself, in `tests/send_sync.rs` under the `system` feature this manifest enables, and the assertion above was compiled against sysinfo 0.38.4 for both the host and `x86_64-pc-windows-gnu`. Keep the assertion anyway: it costs one line and it is what turns a future sysinfo bump that breaks the property into a `cargo check` failure here rather than a confusing `Arc<dyn ...>` error at the enforcer's construction site three modules away.

- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 6: Commit** — `feat(daemon): sample resident memory across a sheep's process tree`

---

### Task 5: The bench crate, and the number behind the poll interval

**Files:**
- Create: `benches/Cargo.toml` — pure
- Create: `benches/benches/memory_sample.rs` — pure
- Modify: `.github/workflows/test.yml` — pure

The root `Cargo.toml` is **not** in that list, and its absence is the point: the bench crate is outside the workspace, so it can neither be listed in `members` nor opt into `[workspace.dependencies]`. Every dependency it has, it declares itself.

**Why this is a task and not a paragraph inside Task 4:** IR-26 requires every tuning threshold to be a named const with a *benchmark-backed* comment, and this workspace has no benchmark harness at all — no `benches/` directory, no `criterion` dependency, no CI job. Building the first one is new infrastructure with its own manifest shape (IR-5: separate unpublished crate, its own `[workspace]` table, `harness = false`, deterministic fixtures, `black_box` on both ends, and CI compiles *and runs* it so it never rots). Folding that into the sampler task would put a workflow change inside a diff about `/proc` walking.

**Interfaces:** none exported. The deliverable is a committed measurement.

- [ ] **Step 1: Create the bench crate**

```toml
[package]
name = "shep-benches"
publish = false
# Spelled literally, not inherited. The `[workspace]` table below makes this
# crate its own workspace root, so `edition.workspace = true` would resolve
# against THIS file's (absent) `[workspace.package]` and the manifest would
# fail to parse. Keep both in step with the root Cargo.toml by hand.
edition = "2024"
rust-version = "1.88"

# Own workspace table: this crate is excluded from the root workspace's
# dependency unification so a bench-only dependency can never end up in a
# shipped binary's tree (IR-5).
[workspace]

[dependencies]
shep-daemon = { path = "../crates/shep-daemon" }
criterion = { version = "0.7", default-features = false, features = ["cargo_bench_support"] }

[[bench]]
name = "memory_sample"
harness = false
```

**IR-5 and workspace inheritance collide on those two keys, and the literal spelling is the resolution.** A crate cannot both be its own workspace root and inherit from the outer one. Written as `edition.workspace = true`, this manifest dies at parse with "error inheriting `edition` from workspace root manifest's `workspace.package.edition`" — reproduced with cargo before it was written down here — and nothing downstream of Step 1 runs, including Step 5's own `cargo check --manifest-path` gate. Three consequences an implementer meets next:

- `[lints] workspace = true` is unavailable for exactly the same reason, and IR-1's instinct is to add it. It fails identically. Either restate the deny list in a local `[lints.rust]`/`[lints.clippy]` block or accept that the benches build without it — and say which in a comment, so the omission reads as a decision.
- `version` is genuinely optional; cargo defaults it to `0.0.0`. Add `version = "0.1.0"` only if `shep-benches v0.0.0` in the bench output bothers you. Do not add `version.workspace = true` to "complete" the manifest.
- Root `[profile.dev]` (`Cargo.toml:131-135`) does not reach an excluded crate, and the crate builds into its own `benches/target/`. Neither is a defect, but Step 5's gate is slower than the workspace's and `.gitignore`'s bare `/target` does not cover the new directory — add `benches/target` to it.

The MSRV duplication is the price of the exclusion, and it is why `rust-version` is written twice in this repo rather than once. Note it in the report.

criterion `0.7` is the current line — verified against the vendored crate: `rust-version = "1.80"` (comfortably under this workspace's floor), `cargo_bench_support` present in `[features]`, and `default = ["rayon", "plotters", "cargo_bench_support"]`, so turning defaults off and naming that one feature back on is what keeps rayon and plotters out. The `0.5` this plan carried in an earlier draft was two majors stale. Re-check it against docs.rs anyway before writing the manifest — rule 8 is about the habit, not about this line.

- [ ] **Step 2: Benchmark two things, not one**

`tree_rss` over a synthetic table of 500 processes with a realistic tree shape (deterministic fixture, built in the bench, `black_box` in and out) — this is the part that scales with flock size.

`SysinfoSampler::sample()` against the real machine — this is the part that scales with the *host's* process count and is the number `MEMORY_POLL_INTERVAL` is really about. It is not deterministic, and that is fine for a bench whose output is a comment rather than an assertion; say so in the bench's own comment.

- [ ] **Step 3: Add the CI job**

A `bench` job in `.github/workflows/test.yml` running `cargo bench --manifest-path benches/Cargo.toml -- --test`. Criterion's `--test` mode runs each benchmark once and asserts nothing, which is exactly what "CI compiles and runs them so they never rot" needs — a real timing run on a shared runner would be noise. Match the existing jobs' shape: an explicit `rustup toolchain install`, and no `fail-fast` change (the matrix jobs already set it). Do **not** add a per-job `permissions` block — `contents: read` is declared once at workflow level (`.github/workflows/test.yml:13-14`) and none of the eight existing jobs repeats it; a ninth that did would be the odd one out.

- [ ] **Step 4: Record the measurement**

Run the bench locally and write the two numbers, with the machine they came from and the date, into the bench file's own header comment. Task 6's `MEMORY_POLL_INTERVAL` comment cites them. **Numbers do not go into this plan** — a measured value belongs next to the thing it justifies, and a plan file is not where anyone will look for it in a year.

- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code.** The workspace gates do not cover the bench crate (it is outside the workspace); run `cargo check --benches --manifest-path benches/Cargo.toml`, `cargo +1.88 check --benches --manifest-path benches/Cargo.toml` and `cargo bench --manifest-path benches/Cargo.toml -- --test` as three more, each from its own exit code. The MSRV one is not ceremony: the crate declares `rust-version = "1.88"` literally rather than inheriting it, so nothing else in the repo will ever notice if a bench dependency outruns it. criterion 0.7.0 declares 1.80 today.

**`--benches` is the whole gate, not a flag to tidy away.** This package has no `src/lib.rs` and no `[[bin]]`, so its only target is the `[[bench]]`, and plain `cargo check` builds nothing at all: it prints `Finished` in a fraction of a second over a bench file containing a type error. The MSRV form is worse, because it is the one the paragraph above calls load-bearing — `cargo +1.85 check` against this manifest exits `0` even though the crate and three of its dependencies declare `rustc 1.88`; add `--benches` and the same command fails with `error: rustc 1.85.1 is not supported by the following packages`. Both were reproduced against this exact manifest before being written down.
- [ ] **Step 6: Commit** — `bench: add the workspace's first criterion harness for memory sampling`

---

### Task 6: `LimitEnforcer` and the polling enforcer

**Files:**
- Modify: `crates/shep-daemon/src/limits/mod.rs` — pure tier
- Modify: `crates/shep-daemon/src/testing.rs` — pure
- Create: `crates/shep-daemon/tests/external_impls.rs` — pure tier

**Interfaces:**
- Consumes: `crate::limits::sample::{MemorySampler, ProcessRss, tree_rss}`, `shep_core::values::MemSize::bytes` (`crates/shep-core/src/values.rs:45`)
- Produces:

```rust
/// How often the polling enforcer samples the process table.
///
/// Spec §14.2 tightened this from 30s to 15s: sampling is cheap enough that
/// halving worst-case breach latency costs nothing measurable. See the numbers
/// in `benches/benches/memory_sample.rs`.
pub const MEMORY_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// A sheep whose process tree exceeded its `max_memory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitBreach {
    /// The sheep's id.
    pub id: u32,
    /// The pid this enforcement was armed against.
    ///
    /// Carried so the consumer can tell a breach about the process running
    /// now from one about the process it replaced: a report already queued
    /// when the sheep exits and respawns names a pid the id no longer has.
    pub root_pid: u32,
    /// What the tree was measured at.
    pub observed: MemSize,
    /// The limit it exceeded.
    pub limit: MemSize,
}

/// Watches each armed sheep's process tree for memory-limit breaches.
///
/// The mechanism is deliberately absent from this contract. The polling
/// implementation samples; the cgroup-v2 implementation planned for v1.1
/// writes `memory.max` and reads `memory.events`, and must be able to replace
/// this one without the engine noticing.
pub trait LimitEnforcer: Send + Sync + 'static {
    /// Begins enforcing `limit` against the process tree rooted at `root_pid`.
    ///
    /// Arming an already-armed id replaces the previous arming — a respawn
    /// gives the same id a new pid.
    fn arm(&self, id: u32, root_pid: u32, limit: MemSize);
    /// Stops enforcing against `id`. A no-op if it was never armed.
    fn disarm(&self, id: u32);
}

/// `LimitEnforcer` by periodic sampling.
pub struct PollingEnforcer { /* private */ }

impl PollingEnforcer {
    /// Starts the polling task; breaches arrive on `breaches`.
    ///
    /// The task ends when the returned value is dropped.
    ///
    /// Must be called from within a Tokio runtime context: it spawns the
    /// polling task immediately, the same way `spawn_supervisor` and
    /// `ProcessRunner::spawn` already document for themselves. The phrasing is
    /// prose rather than a `# Panics` section deliberately — neither of those
    /// carries one, and IR-21 wants `# Panics` and `#[track_caller]` to travel
    /// together or not at all.
    pub fn start(sampler: Arc<dyn MemorySampler>, breaches: mpsc::Sender<LimitBreach>) -> Self;
}
```

**One sample pass serves every armed sheep.** The task samples once per tick and then sums a tree per armed sheep out of that single table. A per-sheep refresh would multiply the syscall walk by the flock size for no new information, and the whole reason `MemorySampler::sample` returns the *whole* table rather than one pid's reading is to make the wrong shape hard to write.

**A breach disarms the id it reports.** Without that, the next tick — 15 seconds later, while the restart is still in flight — sees the same over-limit tree and reports again, and the sheep gets restarted twice. The re-arm happens when the sheep comes back online, which Task 12 owns. State this in the trait's `arm` doc as part of the contract, not as an implementation detail of `PollingEnforcer`, because the cgroup implementation has to honour it too.

**Who acts on a breach:** Task 12 owns the consumer. It does not call the public `restart` — it calls a pid-guarded command that *delegates* to the same manual-restart path, so the budget still resets (`supervisor.rs:210` doc: "resetting its restart budget"), which is the behaviour spec §4 wants: only exits within `min_uptime` count as unstable, and a memory-limit restart is not an exit within `min_uptime`. Task 12 states the guard set and why an unguarded `restart` resurrects a sheep the user stopped. There is one new *guard* on the way in, not a second respawn path — the "do not add a second code path" rule this task cares about is honoured in substance.

- [ ] **Step 1: Write the tests, then the enforcer**

All under `#[tokio::test(start_paused = true)]`, driven by `ScriptedSampler`. Required cases with their "fails if" comments:

| Case | The broken implementation it catches |
|---|---|
| a tree under the limit for three ticks produces no breach, and `sampler.calls()` is exactly 3 | a loop that polls on the wrong cadence, or one that reports on equality |
| a tree that crosses the limit on the third reading breaches on exactly the third tick, asserted as the `Instant` at which the breach arrives | a first-tick-immediate loop, or one that reports a tick late |
| the breach's `observed` is the **tree** sum, not the root pid's own bytes, and its `root_pid` is the pid `arm` was given | an enforcer that skipped `tree_rss`, or one that leaves `root_pid` at zero and so defeats Task 12's staleness guard |
| after a breach, the next two ticks produce no second breach for that id | the missing self-disarm above |
| two instances armed at different limits: only the one over its own limit breaches | an enforcer that compares every tree against every limit, or against the first limit armed |
| `disarm` before the next tick produces no breach | an enforcer that leaks armed entries |
| a sheep whose root pid is absent from the table produces no breach | a `tree_rss` of 0 compared with `>=` against a limit of 0, or a panic on the missing pid |

Every `breaches.recv().await` is wrapped in `tokio::time::timeout` naming the breach that did not arrive. The negative cases — "no breach" — are asserted the same way, with a bounded `timeout` + `recv` over the ticks that must stay silent, per Global Constraints rule 11. An earlier draft of this task carved out a `try_recv()` here on the reasoning that a timeout expected to expire costs real time; it does not, on a paused clock, and the carve-out silently defeated the cases it was applied to — measurements in rule 11.

This task is implemented and its tests already use the bounded form: `assert_no_breach_within` in `crates/shep-daemon/src/limits/mod.rs` takes a window and panics if a breach arrives inside it. **It is not to be "fixed" back to `try_recv`.** It also crosses ticks by letting `timeout` auto-advance the paused clock rather than by calling `advance` at all, which is the shape the first two rows of this table need. `sampler.calls()` reads 3 after three ticks that way; the same three ticks crossed by `3 × advance(15s)` read 1, and by `45 × advance(1s)` read 2. The first row pins the count at exactly 3 and the second asserts a breach on exactly the third tick, so neither row is observable through an `advance`-driven test at all — the third tick never happens.

The comparison is `observed > limit`, strictly. A tree exactly at `max_memory` has not exceeded it, and the boundary sweep asserts that a reading of exactly the limit does not breach while `limit + 1` does.

- [ ] **Step 2: The compile-only external-impl proof (IR-38)**

`crates/shep-daemon/tests/external_impls.rs` is this crate's one *compile-only* `tests/` file and proves an outside crate can implement all three new traits. IR-38 bounds compile-only files, not `tests/` files: `daemon_e2e.rs` and `real_runner.rs` are already there, both behavioural, both carrying their own IR-38 deviation note, and neither is affected by this one. Bodies are `todo!()`; the file is never run for behaviour. It carries no `#![cfg(unix)]` — every trait it names is pure tier, and gating it would remove the proof from the Windows leg, which is the leg most likely to break it.

Write all three implementations now, even though `Prober` and `MemorySampler`'s producing tasks are elsewhere: this file is one compilation unit and splitting it across three tasks would have three tasks editing the same file. It compiles against `MemorySampler` (Task 4) and `LimitEnforcer` (this task); the `Prober` implementation is added by Task 7, which is the only exception and is called out there.

- [ ] **Step 3: Dyn-compatibility smoke tests** — `&dyn LimitEnforcer` against `PollingEnforcer`.
- [ ] **Step 4: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 5: Commit** — `feat(daemon): polling memory-limit enforcement behind a mechanism-free seam`

---

### Task 7: The `Prober` seam and the liveness loop

**Files:**
- Create: `crates/shep-daemon/src/probes/mod.rs` — pure tier
- Modify: `crates/shep-daemon/src/testing.rs`, `crates/shep-daemon/src/lib.rs` — pure
- Modify: `crates/shep-daemon/tests/external_impls.rs` — pure

**Interfaces:**
- Consumes: `shep_core::config::{ProbeConfig, ProbeTarget}`
- Produces:

```rust
/// Why a probe did not pass.
///
/// Growth is expected: each new probe transport brings its own failure modes
/// (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    /// The probe did not finish inside `ProbeConfig::timeout`.
    Timeout,
    /// The transport failed before a verdict was possible — connection
    /// refused, DNS failure, the command could not be spawned. Carries the
    /// rendered reason.
    Transport(String),
    /// The probe completed and the answer was negative: a non-2xx status, or a
    /// non-zero exit. Carries the status or exit code.
    Rejected(String),
}

// A boxed future rather than RPITIT, because `Arc<dyn Prober>` is how the
// engine holds this and RPITIT is not dyn-compatible. One allocation per probe
// — once per `interval`, default 10s — against three extra generic parameters
// threaded through the actor and every fixture.
/// Runs one probe against one target.
pub trait Prober: Send + Sync + 'static {
    /// Probes `target`, giving up after `timeout`.
    fn probe<'a>(&'a self, target: &'a ProbeTarget, timeout: Duration)
        -> Pin<Box<dyn Future<Output = Result<(), ProbeFailure>> + Send + 'a>>;
}

/// A sheep whose liveness probe hit `failure_threshold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessFailure {
    /// The sheep's id.
    pub id: u32,
    /// The pid this loop was armed against, for the same reason
    /// [`LimitBreach::root_pid`](crate::limits::LimitBreach) carries one.
    pub pid: u32,
}

/// Runs a sheep's liveness probe until the returned handle is aborted.
///
/// Reports through `failures` once `failure_threshold` consecutive probes have
/// failed, then ends: a sheep that has been declared unhealthy is about to be
/// restarted, and the loop for its replacement is a new one.
pub fn spawn_liveness_task(
    id: u32,
    pid: u32,
    config: ProbeConfig,
    target: ProbeTarget,
    prober: Arc<dyn Prober>,
    failures: mpsc::Sender<LivenessFailure>,
) -> tokio::task::JoinHandle<()>;
```

`failures` is a *shared* sender cloned once per arming, not a per-loop channel. Its receiver belongs to the single reporting task Task 12 declares; an implementer who cannot find where the sender comes from should read Task 12 rather than manufacture an `mpsc::channel(1)` here, which compiles and sends every report to a dropped receiver.

**Interval is measured from probe completion, not from probe start.** `sleep(interval)` after the probe resolves, rather than a `tokio::time::interval` ticking independently, means a struggling app always gets a full `interval` of quiet *between* probes. `ProbeConfig` allows a `timeout` longer than its `interval` — `timeout` defaults to 5s and `interval` to 10s (`crates/shep-core/src/config/app.rs:35-38`), but nothing stops a user inverting them — and a `tokio::time::interval` loop under that configuration degenerates: its default `MissedTickBehavior::Burst` makes every tick overdue once a probe outlasts its period, so it probes back-to-back with no gap at all, which is the shape that turns a slow service into a dead one. Note the claim carefully — neither candidate loop can *overlap itself*, because both await the probe inline. The difference is cadence, and the test below is written against cadence rather than against something unobservable.

**The counter resets on any pass.** `failure_threshold` is consecutive failures (`app.rs:41` doc: "Consecutive failures before the probe reports unhealthy"), so a pass-fail-fail-pass-fail sequence never trips a threshold of 3.

- [ ] **Step 1: `ScriptedProber` and `probe_config` in `testing.rs`, with WHY comments**

`ScriptedProber` replays a `Vec<Result<(), ProbeFailure>>` and repeats the final entry once exhausted — see the fixture roster for why, including what an empty script does. Its `probe` implementation ignores both arguments; that is the point of a scripted fake, and the argument-ignoring is what makes it usable for HTTP, TCP and exec cases alike.

`with_delay` is the one exception to the argument-ignoring, and it is additive rather than a second constructor: `new` keeps its signature so the four threshold cases and the dyn-compatibility line at Step 4 are untouched. The delay sleeps on the paused clock, so a case using it still runs in microseconds.

- [ ] **Step 2: Write the liveness tests, then the loop**

All `#[tokio::test(start_paused = true)]`. Required cases with their "fails if" comments:

| Case | The broken implementation it catches |
|---|---|
| threshold 3, script `[Fail, Fail, Fail]`: the failure arrives after exactly three probes and at exactly `3 × interval` from arming, asserted as an `Instant` | an off-by-one threshold, and a loop that probes on a `tokio::time::interval` (which would fire the first probe at t=0 and the third at 2×interval) |
| threshold 3, script `[Fail, Fail, Pass, Fail, Fail]`: nothing is reported | a counter that accumulates non-consecutive failures |
| threshold 3, script `[Fail, Fail, Pass, Fail, Fail, Fail]`: reported after six probes | a counter that resets but then double-counts, or one that never re-arms |
| threshold 1, script `[Fail]`: reported after one probe | a `>` where `>=` belongs |
| after reporting, `prober.calls()` does not grow over a further three intervals | a loop that keeps probing a sheep it already declared dead |
| `with_delay(2 × interval)`, script of passes: after a span of `12 × interval` the prober has been called exactly 4 times | a `tokio::time::interval` loop — `MissedTickBehavior::Burst` makes it probe back-to-back every `2 × interval` and reach 7 calls in the same span |
| aborting the handle stops the probing | a task that outlives its sheep |

The arithmetic on the cadence row, so the implementer pins the right number rather than the number their loop happens to produce: the mandated loop sleeps *then* probes, so probe starts land at I, 4I, 7I, 10I — four calls by 12I. An `interval` loop starts at 0, 2I, 4I, 6I, 8I, 10I, 12I — seven. A gap of three is a guard that bites; without `with_delay` the two loops differ by exactly one call, which is a difference the first row of this table already asserts more precisely, and the case would be guarding nothing.

The `failures.recv().await` in the positive cases is wrapped in `tokio::time::timeout` naming the id that did not arrive. The negative cases assert with a bounded `timeout` + `recv` over the intervals that must stay silent, as in Task 6 and per Global Constraints rule 11.

**Two rows of this table pin a call count, and the count is only correct if the clock is crossed that way.** The cadence row's four and the no-further-probing row's "does not grow" are both readings of `prober.calls()` taken after a span. This loop sleeps *then* probes, which is the shape rule 11's left column measures: an explicit `advance` returns before the woken loop has run, so its next sleep does not yet exist, and a span crossed that way loses its last tick at best and nearly all of them at worst — three due, one observed. That would turn the pinned 4 into a smaller number the implementer then bakes in as correct, and it would let a loop that *does* keep probing a sheep it declared dead sail through the row written to catch it. Cross both spans by awaiting a bounded `timeout` that clears the final deadline by a hair, and the counter reads what the arithmetic above says it should.

- [ ] **Step 3: Add the `Prober` implementation to `tests/external_impls.rs`**

This is the one place a later task edits an earlier task's file, and it is unavoidable: IR-38 allows one compile-only file per crate, and the trait did not exist when that file was created. Add the implementation, change nothing else in the file.

- [ ] **Step 4: Dyn-compatibility smoke test** — `let _: &dyn Prober = &ScriptedProber::new(vec![]);` inside the test block.
- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 6: Commit** — `feat(daemon): liveness probing with a consecutive-failure threshold`

---

### Task 8: `OsProber` — HTTP, TCP and exec against the real world

**Files:**
- Create: `crates/shep-daemon/src/probes/os.rs` — pure tier (with two `#[cfg]` arms inside one function)
- Modify: `crates/shep-daemon/src/probes/mod.rs`, `crates/shep-daemon/src/testing.rs` — pure

**Interfaces:**
- Consumes: `tokio::net::TcpStream`, `tokio::process::Command`, `shep_core::config::ProbeTarget`
- Produces:

```rust
/// Longest status line `OsProber` will read before giving up on a response.
///
/// An HTTP/1.1 status line is a method-agnostic `HTTP/1.1 200 OK` — tens of
/// bytes. This bound exists so a probe target that is not an HTTP server
/// cannot stream unbounded data into the daemon's heap; nothing legitimate
/// comes close to it.
const HTTP_STATUS_LINE_CAP: usize = 8 * 1024;

/// `Prober` over real sockets and real processes.
pub struct OsProber {
    /// Working directory and environment for exec probes — a probe usually
    /// needs the same `PORT` the sheep was given.
    ///
    /// `Debug` does not leak environment values (IR-41).
    /* private: cwd: Option<PathBuf>, env: BTreeMap<String, String> */
}

impl OsProber {
    /// A prober that runs exec probes in `cwd` with `env`.
    #[must_use]
    pub fn new(cwd: Option<PathBuf>, env: BTreeMap<String, String>) -> Self;
}
```

**`OsProber` carries a sheep's environment, so it is an IR-41 type.** Manual `Debug` printing the key *count* and never the values, plus an exact-string test asserting that formatted output, with a comment saying the test exists so a `derive(Debug)` refactor fails CI rather than putting `DATABASE_URL` into a daemon log. Spec §10 makes env redaction a security premise, not a nicety. **Copy the existing implementation's shape rather than inventing a second one**: `AppConfig`'s manual `Debug` (`crates/shep-core/src/config/app.rs:147-156`) already does this with `.field("env", &format_args!("<{} vars>", self.env.len()))` and `.finish_non_exhaustive()`, and two different redaction spellings in one workspace is one too many.

**The HTTP probe is hand-rolled, and this is the whole of it:** connect a `TcpStream`, write

```
GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n
```

read until the first `\r\n` or `HTTP_STATUS_LINE_CAP` bytes, parse the status code out of the second space-separated token, and pass on `200..=299`. Wrap the whole thing — connect, write, read — in one `tokio::time::timeout(timeout, ...)`, because three separate timeouts add up to three times the budget the user configured.

Rejected alternatives, so nobody re-litigates: `reqwest` brings tower and a TLS stack into a daemon targeting single-digit-MB idle RSS (spec §14.11); `hyper` + `hyper-util` + `http-body-util` is three dependencies and a connection-pool abstraction to send one request with `Connection: close`; `ureq` and `minreq` are blocking, and a blocking read cannot be cancelled by `tokio::time::timeout` — the timeout would return while the thread stayed stuck. The `Prober` seam means swapping any of them in later touches one file.

**No TLS, no redirects, and both are visible to the user rather than silent.** `https://` targets are rejected at config time (Task 2). A `301` is a `Rejected` failure, not a follow — a probe that follows redirects is a probe that can pass against a completely different service.

**The `ProbeTarget` match is the compile-time gate.** `OsProber::probe` matches all three `ProbeTarget` variants with **no `_` arm**. `ProbeTarget` is deliberately not `#[non_exhaustive]` — its *error* type is, because rejection modes grow, but the transport set does not grow additively: a fourth probe transport is a spec change and every implementation must handle it. So adding one fails `cargo check` with E0004 at exactly the place that has to. That is worth more than any test, and it is rule 6's application in this task.

**Exec probes and the shell:**

```rust
#[cfg(unix)]
let mut cmd = { let mut c = Command::new("sh"); c.arg("-c").arg(command); c };
#[cfg(windows)]
let mut cmd = { let mut c = Command::new("cmd"); c.arg("/C").arg(command); c };
```

`.kill_on_drop(true)` is mandatory — without it a probe that times out leaves the command running, and a 10-second interval against a command that takes 30 seconds accumulates processes until the box falls over. Set `.current_dir` from `cwd` and `.env_clear().envs(env)` so the probe sees the sheep's environment and not the daemon's; the daemon's environment leaking into a child is exactly what `SpawnSpec::env`'s "no daemon-env leakage beyond this map" comment (`crates/shep-daemon/src/runner.rs:173`) already rules out for sheep, and a probe is not a special case. Exit status 0 passes; anything else is `Rejected` carrying the code.

- [ ] **Step 1: `HttpReply` and `loopback_http` in `testing.rs`**

The fake binds `127.0.0.1:0` (never a fixed port — a fixed port makes the suite fail under `--test-threads` greater than one and on any developer machine that happens to be using it) and serves one scripted reply per accepted connection, in order. `HttpReply::Hang` accepts the connection and then never writes, which is the only way to exercise the timeout path honestly. The returned `JoinHandle` is aborted by the test at the end; it is not detached.

**This fixture is the second Phase 3 trap in this plan's path.** A fake that is torn down before the code under test connects makes the connection fail for the wrong reason and the test pass for the wrong reason. `loopback_http` returns *after* the listener is bound — it binds before spawning the accept loop and returns the bound `SocketAddr` — so a probe against the returned address cannot race the bind. Do not restructure it into "spawn a task that binds", which reintroduces exactly that race.

- [ ] **Step 2: Tests — real time, with the justifying comment IR-33 requires**

These are the phase's second real-time tier (the first is Task 10). Real sockets need real time: `start_paused` freezes `tokio::time` while the kernel's TCP stack keeps running, so a paused test that waits for a real connection deadlocks. Write the comment saying so at the top of the test module.

Required cases with their "fails if" comments:

| Case | The broken implementation it catches |
|---|---|
| `200 OK` passes; `204` passes; `299` passes | a `== 200` check |
| `301` fails as `Rejected("301")` | a prober that follows redirects, or one that treats 3xx as success |
| `500` fails as `Rejected("500")` | a prober that only looks at whether bytes arrived |
| `HttpReply::Hang` with a short `timeout` fails as `Timeout` and returns within a small multiple of that timeout | separate per-step timeouts, whose total exceeds the configured budget |
| a port with nothing listening fails as `Transport` | collapsing a refused connection into `Timeout`, which is the failure mode that makes a down service look like a slow one |
| a garbage first line (`HttpReply::Raw("not http\r\n")`) fails as `Rejected` or `Transport`, and the test asserts which | a parser that panics on a malformed status line, or one that indexes a token that is not there |
| TCP probe against a bound `TcpListener` passes; against a closed port fails as `Transport` | a TCP probe that reports success on any resolvable address |
| exec probe `exit 0` passes; `exit 3` fails as `Rejected("3")`; a nonexistent command fails as `Transport` | conflating "the command failed" with "the command could not be run" — the first means the app is unhealthy, the second means the probe is misconfigured, and a user needs to tell them apart |
| exec probe sees an env var the prober was constructed with | `.envs()` without `.env_clear()`, or an `OsProber` that ignores its own env |
| `Debug` of an `OsProber` holding two env vars matches an exact expected string containing neither value | a `derive(Debug)` refactor (IR-41) |

The exec cases use `sh -c` on unix and must have a `#[cfg(windows)]` counterpart or be `#[cfg(unix)]`-gated **as tests**, not by gating the module. Gating two test functions keeps the Windows leg compiling and running everything else in the file; gating the module would remove `OsProber` from Windows entirely, which contradicts this phase's tiering claim.

- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: Commit** — `feat(daemon): HTTP, TCP and exec probes over real sockets and processes`

---

### Task 9: `await_ready` and the readiness gate on `starting → online`

**Files:**
- Create: `crates/shep-daemon/src/probes/ready.rs` — pure tier
- Modify: `crates/shep-daemon/src/supervisor.rs` — pure
- Modify: `crates/shep-daemon/src/probes/mod.rs` — pure

**Interfaces:**
- Consumes: `crate::probes::Prober`, `shep_core::config::{AppConfig, ProbeTarget}`, the existing `Msg::Ready { id }` (`crates/shep-daemon/src/supervisor.rs:130`)
- Produces:

```rust
/// Where a sheep's readiness signal comes from (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessSource {
    /// `wait_ready = true`: the shepherd channel's `{"kind":"ready"}`.
    Channel,
    /// `readiness_probe` is set: the first passing probe.
    Probe(ProbeConfig, ProbeTarget),
    /// Neither is configured: readiness is the deadline elapsing.
    Heuristic,
}

impl ReadinessSource {
    /// Derives the source from an app's configuration.
    ///
    /// `wait_ready` wins over `readiness_probe` when both are set: the channel
    /// is the app telling us directly, and a probe is an outside guess at the
    /// same fact.
    ///
    /// # Errors
    ///
    /// Whatever [`ProbeTarget::parse`] returns for the app's
    /// `readiness_probe` target — [`ProbeTargetError::Empty`],
    /// [`HttpsUnsupported`](ProbeTargetError::HttpsUnsupported),
    /// [`NotHttpUrl`](ProbeTargetError::NotHttpUrl),
    /// [`MissingHost`](ProbeTargetError::MissingHost),
    /// [`MissingPort`](ProbeTargetError::MissingPort) or
    /// [`BadPort`](ProbeTargetError::BadPort). In practice `normalize` has
    /// already rejected every one of them, so an `Err` here means the daemon
    /// adopted an app that never went through `normalize`.
    pub fn of(config: &AppConfig) -> Result<Self, ProbeTargetError>;
}

/// How a readiness wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The signal arrived inside the deadline.
    Ready,
    /// The deadline elapsed with no signal.
    TimedOut,
}

/// Waits for `source`'s readiness signal, giving up after `deadline`.
///
/// `channel` carries the shepherd channel's ready notification and is read
/// only by [`ReadinessSource::Channel`].
pub async fn await_ready(
    source: &ReadinessSource,
    deadline: Duration,
    channel: oneshot::Receiver<()>,
    prober: Arc<dyn Prober>,
) -> Readiness;
```

**The `ReadinessSource` match inside `await_ready` carries no `_` arm.** Three sources, three arms, and a fourth source added later fails `cargo check`. Rule 6 again.

**Readiness gates the start path only when the app configures it — this is a departure from the research note and it is deliberate.** `docs/research/phase4-lifecycle.md:161-166` has one `await_ready` serving both normal start and reload's AwaitReady, `Heuristic` included. Applied to the start path, `Heuristic` means every app that configures no readiness at all waits `listen_timeout` — 3000ms by default (`crates/shep-core/src/config/app.rs:106`, `:178`) — before reaching `online`. That is a three-second regression on every `shep start` in the default configuration, and nothing in the spec asks for it: §7 says readiness "gates reload", and §4 puts the heuristic inside reload's `AwaitReady` state. **The spec wins** (`docs/specs/shep-v1.md:8-9`). So:

- `wait_ready = true` or `readiness_probe` set → the sheep enters `starting` and reaches `online` on the signal, or on the deadline.
- Neither → `online` on spawn success, exactly as the engine behaves today. No new latency, and no existing supervisor test changes its expectations.
- `Heuristic` still exists, is still tested, and is what reload will use when reload is built. It is simply not reachable from the start path.

**On deadline elapse the sheep goes online anyway, with a `tracing::warn!` (the maintainer, 2026-08-08).** Treating a readiness timeout as a spawn failure turns a slow-starting app into a restart loop — precisely the failure mode `max_restarts` exists to contain, reached by an app that is slow rather than broken, and the one pm2 users hit constantly. The stricter reading is rejected on that basis: `errored` is arguably more honest about what happened, but it converts a latency problem into an outage. Reporting the elapse as its own `ProcessEventKind::ReadinessTimeout` is a third option and is out of reach here for an unrelated reason — a new event kind is a wire-additive change and this phase's out-of-scope list forbids one. It stays available to the phase that wants it.

**The supervisor changes, precisely:**

1. `spawn_fresh` (`crates/shep-daemon/src/supervisor.rs:523`) and `respawn` (`:628`) set `status` to `ProcStatus::Starting` instead of `Online` **when and only when** `ReadinessSource::of(spec.config())` is not `Heuristic`, and spawn a readiness task.
2. A new mailbox variant, alongside `RestartDue`'s existing epoch-guarded shape (`:117-125`):
   ```rust
   /// A readiness wait resolved.
   ReadyResult {
       /// The sheep's id.
       id: u32,
       /// The slot's epoch when the wait began; a stale result is dropped.
       epoch: u64,
       /// Whether the signal arrived or the deadline elapsed.
       readiness: Readiness,
   },
   ```
   Its handler mirrors `handle_restart_due` (`:1014`) exactly: drop if `shutting_down`, drop if the slot is gone, drop if `slot.epoch != epoch`, drop if the status is no longer `Starting`. **That guard set is not boilerplate to trim.** A sheep that exited and respawned while its readiness task was still waiting would otherwise have the old wait mark the new process online.
3. `Msg::Ready { id }`'s handler (`:431`) stops being a `tracing::debug!` that says gating "is Phase 4" and starts forwarding to the waiting readiness task through a per-slot `oneshot::Sender<()>` held in `SheepSlot`. A `Ready` for a slot with no waiting sender is dropped silently — an app is free to write `{"kind":"ready"}` whenever it likes, including twice.
4. `ProcessEventKind::Online` is emitted at the transition to `Online`, not at spawn, for gated apps. **Only the `Online` emit moves; the event that already fires at the spawn point on each path stays where it is** — `Start` in `spawn_fresh` (`:577`) and `Restart` in `respawn` (`:667`). Note that `respawn` never emits `Start` at all, so "Start still fires at spawn" is true of the fresh path only; on the restart path it is `Restart` that keeps subscribers from going blind for the whole readiness window. Deferring both would leave a gated app producing no event whatsoever between the spawn and the readiness verdict, for up to `listen_timeout`. This is a behaviour change visible on the bus and it is the right one — a subscriber watching for `Online` wants to know when the app is serving. Add a test on the respawn path asserting an event arrives *before* the readiness signal does; fails if an implementer moves both emits together.

- [ ] **Step 1: `await_ready` and its tests, before touching the supervisor**

`await_ready` is a free async function over channels and a prober, so it is testable on its own with a paused clock. Required cases with their "fails if" comments:

| Case | The broken implementation it catches |
|---|---|
| `Channel`, signal at 500ms, deadline 3s → `Ready` at 500ms | a wait that always runs the full deadline |
| `Channel`, no signal, deadline 3s → `TimedOut` at exactly 3s | a wait that never gives up, which would hang a start forever |
| `Channel`, sender dropped without signalling → `TimedOut` at the deadline, not immediately | treating a closed channel as a decision; a sheep that died is handled by the exit path, and returning early here would race it |
| `Probe`, prober scripted `[Fail, Fail, Pass]`, interval 1s → `Ready` at 2s | a probe wait that applies `failure_threshold` (a liveness concept) and gives up after three failures |
| `Probe`, first probe runs immediately, not after one interval, asserted as `Ready` at t=0 for a scripted `[Pass]` | an `interval`-first loop, which adds the full interval to every gated start |
| `Probe`, prober always fails, deadline 3s → `TimedOut` at 3s | a probe wait with no deadline |
| `Heuristic`, deadline 3s → `Ready` at exactly 3s | returning `TimedOut` — the heuristic's elapse *is* the readiness signal, and the two verdicts drive different log lines |

Note the last row carefully: `Heuristic` returns `Ready`, not `TimedOut`. That is what makes the heuristic a readiness source rather than a failure.

- [ ] **Step 2: The supervisor changes, and the existing tests**

`crates/shep-daemon/src/supervisor.rs` has 21 paused-clock tests — every `#[tokio::test]` in the file. **The default path is unchanged, so none of them should need editing.** If any does, that is evidence the gate leaked into the ungated path — fix the implementation, not the test, and say so in the report.

New supervisor tests, each with its "fails if" comment: a `wait_ready` app is `Starting` until its channel ready arrives and `Online` after; a `readiness_probe` app is `Starting` until the scripted prober passes; a gated app whose deadline elapses reaches `Online` with the warning; a gated app that *exits* while starting takes the normal exit path and never reaches `Online` (this is the epoch guard's test, and it is the one that catches the stale-wait defect); a gated app restarted manually while `Starting` does not have the old wait mark the new process online.

Every `recv().await` on the event channel in these tests is wrapped in `tokio::time::timeout` naming the event that did not arrive.

- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: Commit** — `feat(daemon): gate online on readiness for apps that configure it`

---

### Task 10: The watch source — notify and the debouncer, bridged to tokio

**Files:**
- Create: `crates/shep-daemon/src/watch/source.rs` — pure tier
- Create: `crates/shep-daemon/src/watch/mod.rs` — pure tier (module declaration and docs only at this point)
- Modify: `crates/shep-daemon/src/lib.rs`, `crates/shep-daemon/src/testing.rs` — pure
- Modify: `Cargo.toml`, `crates/shep-daemon/Cargo.toml` — notify, notify-debouncer-full

**Interfaces:**
- Produces:

```rust
/// A live filesystem watch over one directory tree.
///
/// Dropping this stops the watch: the debouncer's OS thread shuts down with
/// its guard, which drops the sender feeding the receiver below, so the
/// receiver's next `recv()` returns `None`.
///
/// **A caller that spawns a loop over the receiver must move this guard into
/// the spawned future.** A `WatchSource` left as a local in the spawning
/// function drops when that function returns — before the first event is ever
/// delivered — and the loop sees an immediate `None` and exits. Nothing warns
/// about it: Rust does not warn that a value is being dropped, and the
/// cheapest way to silence the `unused_variables` this raises under
/// `-D warnings` is to rename the binding, which preserves the bug exactly.
#[derive(Debug)]
pub struct WatchSource { /* private: the Debouncer guard */ }

/// Begins watching `root` recursively, debounced by `delay`.
///
/// Batches of changed paths arrive on the returned receiver. A batch is
/// whatever the debouncer coalesced within `delay`; it is never empty.
///
/// # Errors
///
/// - [`WatchError::Backend`] — notify could not create a watcher.
/// - [`WatchError::Watch`] — notify could not watch `root`, carrying the path.
pub fn watch_tree(root: &Path, delay: Duration)
    -> Result<(WatchSource, mpsc::UnboundedReceiver<Vec<PathBuf>>), WatchError>;

/// Why a filesystem watch could not be established.
///
/// Two variants and no `#[non_exhaustive]`: notify gives this module exactly
/// two failure points — building the backend and registering a path — and a
/// third reason would mean a third API call, not a third rendering of these
/// (IR-20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchError {
    /// notify could not construct a watcher for this platform's backend.
    /// Carries notify's rendered reason.
    Backend { reason: String },
    /// notify could not begin watching the path: it does not exist, it is not
    /// readable, or the backend's watch limit is exhausted. Carries the path
    /// and notify's rendered reason.
    Watch { path: PathBuf, reason: String },
}
```

**The payloads are owned `String`s for the same hard API reason `CronParseError`'s are.** `notify::Error` carries `#[derive(Debug)]` and nothing else (`notify-8.2.0/src/error.rs:44`) — not `Clone`, not `PartialEq` — so a variant wrapping it could not hold the `Clone, PartialEq, Eq` derives that let Task 11's boundary sweep and Task 13's non-existent-root sweep assert a whole value. Render through `Display` at the boundary, exactly as Task 1 does with `CronError`.

**The verified notify-debouncer-full 0.7 API, read off docs.rs on 2026-08-08:**

```rust
pub fn new_debouncer<F: DebounceEventHandler>(timeout: Duration, tick_rate: Option<Duration>,
    event_handler: F) -> Result<Debouncer<RecommendedWatcher, RecommendedCache>, Error>;
impl Debouncer { pub fn watch(&mut self, path: impl AsRef<Path>, recursive_mode: RecursiveMode) -> Result<()>; }
pub type DebounceEventResult = Result<Vec<DebouncedEvent>, Vec<Error>>;
pub struct DebouncedEvent { pub event: notify::Event, pub time: std::time::Instant }  // Deref<Target = Event>
```

Two details that matter:
- `Debouncer::watch` exists directly on the debouncer; there is no `.watcher()` hop.
- `DebouncedEvent` derefs to `notify::Event`, so paths are `event.paths` (a `Vec<PathBuf>`), reached through the deref.
- `DebouncedEvent::time` is a **`std::time::Instant`**, not tokio's. It is real time and it is not affected by `start_paused`. Nothing downstream may use it for scheduling; this task discards it, and the `WatchGroup` in Task 11 measures its own time with `tokio::time`.

The handler runs on the debouncer's own OS thread. `tokio::sync::mpsc::UnboundedSender::send` is non-blocking and callable from any thread, which is exactly what that context needs — a bounded sender's `blocking_send` would park an OS thread that also owns the watch, and `try_send` would drop batches under load. Unbounded is defensible here because the debouncer already coalesces: the producer's rate is bounded by `delay`, not by the filesystem's event rate.

**The notify feature line is not obvious and has one trap:**

```toml
# Filesystem events for watch-restart (spec §4). notify picks its own backend
# per platform — inotify, ReadDirectoryChangesW — except on macOS, where
# `macos_fsevent` is a DEFAULT feature: dropping default features without
# naming it again falls back to the polling watcher, and watch latency on
# macOS silently becomes seconds.
notify = { version = "8.2.0", default-features = false, features = ["macos_fsevent"] }
# Coalesces the burst a single editor save produces into one batch. On macOS
# and Windows it additionally stitches renames through a file-id cache; on
# Linux `RecommendedCache` is `NoCache` and inotify's own rename cookies do
# that job.
notify-debouncer-full = { version = "0.7.0", default-features = false }
```

notify 8.2.0 has exactly **one** default feature — `[features] default = ["macos_fsevent"]`, with `macos_fsevent = ["fsevent-sys"]` and `fsevent-sys` a macOS-target-gated optional dependency. The research note's "only default" was right; an earlier draft of this plan "corrected" it in the wrong direction. Naming `macos_fsevent` is both necessary and sufficient.

Two things a reviewer should know before reading `cargo tree`. The `RecommendedCache` claim above is platform-conditional and the platform that matters most is the one where it does the least: `notify-debouncer-full-0.7.0/src/cache.rs:61-65` types it as `NoCache` on Linux, Android and wasm, and as `FileIdMap` everywhere else. And the macOS polling-fallback trap the feature line guards against cannot actually fire from this manifest alone — notify-debouncer-full's own `[dependencies.notify]` leaves default features on, so feature unification switches `macos_fsevent` on regardless. The explicit line is belt-and-braces: it keeps the guarantee if the debouncer ever tightens its own dependency, and it is one line.

- [ ] **Step 1: Add both crates, run the `-Z minimal-versions` rehearsal** (the three-command sequence from Task 1, Step 4) **and the MSRV check from Global Constraints**, each from its own exit code. notify 8.2.0 declares 1.77 and notify-debouncer-full 0.7.0 declares 1.85, so both clear 1.88 — running it is what makes that checked rather than assumed.

**The rehearsal fails here, and the floor it wants is already known**, so this is the one dependency whose pin can be written before the command is run rather than after. notify 8.2.0 asks for `fsevent-sys = "4.0.0"` (`notify-8.2.0/Cargo.toml`, the `cfg(target_os="macos")` block) but its `fsevent.rs` calls `FSEventsGetCurrentEventId`, `FSEventStreamGetDeviceBeingWatched` and `FSEventsPurgeEventsForDeviceUpToEventId`, none of which exist before fsevent-sys 4.1.0. Under `-Z minimal-versions` on macOS that is three `E0425`s inside notify itself. The pin follows the block already in the root manifest, and needs shep-daemon's `[target.'cfg(any())'.dependencies]` opt-in to be in the graph at all:

```toml
# Transitive floor pin: notify 8.2.0 declares fsevent-sys "4.0.0", but its
# fsevent.rs needs `FSEventsGetCurrentEventId`,
# `FSEventStreamGetDeviceBeingWatched` and
# `FSEventsPurgeEventsForDeviceUpToEventId`, all added in 4.1.0. macOS only,
# but the pin is unconditional because that is where the rehearsal runs.
fsevent-sys = "4.1.0"
```

With it, the rehearsal resolves and builds clean; without it, it does not. Run the rehearsal anyway — this floor is the one that is known, not the only one that can exist.

`#[derive(Debug)]` on `WatchSource` is fine and needs no manual impl: `Debouncer<T, C>` derives `Debug` (`notify-debouncer-full-0.7.0/src/lib.rs:544`) and both `RecommendedWatcher` and `RecommendedCache` satisfy the derive's bounds. Verified by compiling the exact newtype.
- [ ] **Step 2: `touch` in `testing.rs`**
- [ ] **Step 3: The smoke tests — real time, real filesystem, with the justifying comment**

This is the phase's OS seam and it can only be tested against a real filesystem. IR-33 makes `start_paused` the default and requires a comment when a test uses real time; write it, and say what it is waiting for: a real inotify/FSEvents/`ReadDirectoryChangesW` delivery, which no fake clock can produce.

Required cases with their "fails if" comments:
- a file created under the tempdir root produces a batch containing that path — **fails if** the watch is non-recursive, or if the handler's thread-to-tokio bridge drops the batch;
- a file created in a nested subdirectory also produces a batch — **fails if** `RecursiveMode::NonRecursive` was passed;
- dropping the `WatchSource` stops delivery: after the drop, a further write produces nothing within a bounded wait — **fails if** the debouncer guard is leaked (`std::mem::forget`-equivalent, or storing it in a `static`), which would leave an OS thread per deleted sheep.

**Every wait is bounded and event-driven.** `tokio::time::timeout(WATCH_SMOKE_DEADLINE, rx.recv())` with a message naming the path that never arrived. Do not sleep a fixed duration and then assert — IR-39's "no sleeps" rule applies here even though this is not the e2e tier, because a fixed sleep is simultaneously flaky on a loaded runner and slow on an idle one. Use a generous named deadline (a few seconds) and a `delay` of tens of milliseconds, so the test is fast when it passes and still reliable under `--test-threads=1` contention.

The third case is the subtle one: proving a *negative* needs a bounded wait that is expected to expire, which is the one place this plan permits a timeout to be the passing path. Keep that deadline short and comment why it is short.

- [ ] **Step 4: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 5: Commit** — `feat(daemon): debounced filesystem watch bridged onto tokio`

---

### Task 11: `WatchGroup` — filtering, single-flight restart, and the re-check

**Files:**
- Modify: `crates/shep-daemon/src/watch/mod.rs` — pure tier

**Interfaces:**
- Consumes: `globset::{Glob, GlobSet, GlobSetBuilder}` (already a `shep-daemon` dependency, `crates/shep-daemon/Cargo.toml:29`), `crate::watch::source::{watch_tree, WatchError}`, `crate::supervisor::{SupervisorHandle, SupervisorError}`, `shep_core::selector::ProcessSelector`
- Produces:

```rust
/// Debounce window when an app sets no `watch_delay`.
///
/// Spec §4's default. Long enough to coalesce the multi-event burst a single
/// editor save produces (write to a temp file, rename over the target, chmod),
/// short enough that a save-to-restart round trip still feels immediate.
const DEFAULT_WATCH_DELAY: Duration = Duration::from_millis(500);

/// Paths ignored by every watch, before `ignore_watch` is even consulted.
///
/// Dot-entries cover editor swap files and `.git`'s own churn — a `git status`
/// would otherwise restart the flock. The log and pid directories are shep's
/// own writes, and watching them makes every restart trigger the next one.
const DEFAULT_IGNORE_GLOBS: &[&str] = &[
    "**/.*", "**/.*/**", "**/node_modules/**", "**/logs/**", "**/pids/**",
];

/// Decides whether a changed path should trigger a restart.
#[derive(Debug)]
pub struct WatchFilter { /* private: include: GlobSet, ignore: GlobSet */ }

impl WatchFilter {
    /// Builds the filter from an app's `watch_options` and `ignore_watch`.
    ///
    /// An empty `watch_options` matches every path; the default ignores always
    /// apply on top of `ignore_watch`.
    ///
    /// # Errors
    ///
    /// - [`WatchFilterError::Glob`] — a pattern the globset crate rejected,
    ///   carrying the pattern and the reason.
    pub fn new(watch_options: &[String], ignore_watch: &[String])
        -> Result<Self, WatchFilterError>;

    /// Whether `path` — relative to the watch root — triggers a restart.
    #[must_use]
    pub fn triggers(&self, path: &Path) -> bool;
}

/// Why a watch filter could not be built.
///
/// One variant and no `#[non_exhaustive]`: the only way this construction
/// fails is a pattern globset rejects (IR-20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchFilterError {
    /// A `watch_options` or `ignore_watch` pattern globset rejected. Carries
    /// the pattern as the user wrote it and globset's rendered reason.
    Glob { pattern: String, reason: String },
}

/// Runs one name-group's watch until the returned handle is aborted.
///
/// `root` is an already-canonicalized absolute directory. It comes from the
/// app's own `cwd`, which config validation requires whenever `watch` is on,
/// and never from the daemon's working directory. Aborting the handle stops
/// the OS watch as well as the loop, because the debouncer guard lives inside
/// the spawned future.
///
/// A triggering change restarts every instance of the name. Stopping a sheep
/// is what stops its watch: the last instance of a name going away disarms
/// this group, so a stopped sheep has no watcher left to restart it.
///
/// # Errors
///
/// - [`WatchError::Backend`] — notify could not create a watcher, propagated
///   from [`watch_tree`].
/// - [`WatchError::Watch`] — notify could not watch `root`, carrying the path.
pub fn spawn_watch_group(
    name: String,
    root: PathBuf,
    filter: WatchFilter,
    delay: Duration,
    supervisor: SupervisorHandle,
) -> Result<tokio::task::JoinHandle<()>, WatchError>;
```

**`WatchFilterError` carries owned `String`s by choice, not by constraint, and the distinction is worth one sentence** because the reader has just met `WatchError`'s hard version of the same shape. `globset::Error` derives `Clone, Debug, Eq, PartialEq` (`globset-0.4.20/src/lib.rs:156`) and exposes `glob()` and `kind()`, so wrapping it would keep every derive. It stays owned anyway under IR-11: a config-rejection type is shep's own contract, and pinning it to globset's public error makes globset's next major version a breaking change to shep's config surface — the same reasoning that keeps `croner::Cron` out of `CronSchedule`.

**One watch per name-group, not per instance.** Spec §4 says so, and the reason is that N instances of one app share one source tree: N debouncers over the same tree means N inotify watch sets, N copies of every event, and N restarts racing each other for one file save. The group's restart is `SupervisorHandle::restart(ProcessSelector::Name(name))` — the existing public method, unchanged. This is what makes watch state live in a `HashMap<String, _>` keyed by name rather than in `SheepSlot` — Task 12 owns that map.

**It restarts the whole name-group, stopped instances included, exactly as cron does (the maintainer, 2026-08-08).** An earlier draft of this plan filtered the restart down to instances with a live process, and specified real machinery to do it: a scope parameter threaded through `begin_manual`, a new `pub(crate) Msg` variant, and a `restart_running` handle method beside `restart`. All of that is withdrawn. Four reasons, in the order that decides it:

- **Disarming already covers the case anyone actually hits.** Task 12 arms and disarms a name's watcher on lifecycle transitions, so a fully-stopped sheep has no armed watcher and a file save cannot reach it at all. The filter would only ever have been observable for a *partially* stopped multi-instance group — some instances up, keeping the watcher armed, others individually stopped. That is a narrow corner, and it is the whole of what the machinery bought.
- **The price was engine surface in a lifecycle phase.** A parameter on the actor's manual-command path, a new message variant and a second public restart verb, all added by the phase that is supposed to be bolting four subsystems *onto* the engine without reaching into it.
- **Spec §4 says one watcher per name-group**, and a group whose restart reaches only part of the group is a quieter thing than what that wording describes.
- **It keeps watch and cron identical.** Leaving cron alone while narrowing watch was always going to leave a user-visible asymmetry that neither the code nor the docs could explain in a sentence.

What protects a stopped sheep is therefore disarming, not filtering — and that is a claim a user has to be able to find. Task 12 states it where the disarm rule lives, the `watch/` module doc states it in the header, and the changelog states it in the entry for this subsystem. It should not survive as folklore in three implementers' heads.

The alternative an implementer reaches for once the filter is gone is a `list()` read followed by per-id restarts, to reimplement it locally. It is wrong twice over: it races a `shep stop` landing between the read and the restart, and it is the asynchronous-poll shape Task 3 already rules out for observing a value that changes under you. Do not rebuild the filter here in any form.

**This is not the same question as Task 12's pid guard, and the reversal does not touch that.** A memory breach or a liveness failure carries a stale-signal risk — a report about a process that no longer exists — and Task 12 guards those on the pid. That guard stays exactly as specified. Status-based reach and pid-based staleness are different problems; withdrawing one says nothing about the other.

**The re-check needs no dirty flag, because the channel is the mechanism.** The loop is:

```
loop {
    batch = rx.recv().await                        // None => the source is gone, return
    if !batch.iter().any(|p| filter.triggers(p)) { continue }
    if let Err(err) = supervisor.restart(Name(name)).await { ... }          // cron.rs's three arms
    // events that arrived during the restart are still queued in rx;
    // the next iteration drains and re-filters them
}
```

Spec §4's "Events during an in-flight restart are re-checked after it completes" falls out of the ordering: the restart is awaited, the receiver keeps buffering, and the next `recv` returns whatever accumulated. Nothing needs to remember that a restart happened. **Do not add a dirty flag or a state machine here** — the reason it is unnecessary is worth an IR-31 `//` comment, because it looks like a missing feature until someone traces the buffering.

`NotFound` on that call means the name matches nothing at all — the sheep was deleted and the registry has not disarmed this group yet — and it takes cron.rs's `debug!` arm. The loop keeps running rather than returning: it is a race with disarm, not a fault, and the disarm is moments away. Note the case it is *not*: a name whose instances are all stopped still matches by name, so `restart` reaches them and brings them back, which is the settled behaviour above and not an error path at all.

**The consequence is a single-flight guarantee**, and it is the invariant the proptest in Task 13 checks: because the loop awaits its restart before reading the next batch, a WatchGroup can never have two restarts in flight.

**Filtering is against the path relative to the watch root**, so a user's `watch_options = ["src/**/*.rs"]` means what they think it means. notify delivers absolute paths; strip the root before matching, and when the strip fails (a path outside the root, which the OS should not deliver but a symlinked tree can produce) treat the path as non-triggering rather than matching it against the absolute form.

**Ignore wins over include.** A path matched by both `watch_options` and `ignore_watch` does not trigger. That is the only ordering that makes `ignore_watch` mean anything.

- [ ] **Step 1: `WatchFilter` and its boundary sweep — pure, no tokio, no filesystem (IR-40)**

Cases with their "fails if" comments: empty `watch_options` matches everything; `["src/**/*.rs"]` matches `src/a/b.rs` and not `src/a/b.txt` and not `other/a.rs`; a default ignore beats an explicit include (`watch_options = ["**"]` still does not trigger on `.git/index` or `node_modules/x/y.js`); an `ignore_watch` entry beats an include; a glob that matches nothing simply never triggers rather than erroring; an invalid glob (`[`) is a `WatchFilterError::Glob` carrying the pattern; a path that does not start with the root does not trigger.

The "default ignore beats explicit include" case is the one that catches the most damaging wrong implementation: an ignore set consulted only when `ignore_watch` is non-empty, which makes every app with custom `watch_options` restart on its own log writes.

- [ ] **Step 2: The group loop, paused-clock, driven by a hand-fed channel**

The loop's logic is testable without a filesystem by constructing the group around an `mpsc::UnboundedReceiver` the test owns the sender for. Structure `spawn_watch_group` so that the loop body is a separate `async fn run_group(name, filter, rx, supervisor)` and `spawn_watch_group` is `watch_tree(...)` plus `tokio::spawn(run_group(...))` — that split is what makes both halves testable, and it is the same seam shape as the rest of the phase.

**The debouncer guard goes inside the spawned future, and `run_group` keeps its four parameters:**

```rust
let (source, rx) = watch_tree(&root, delay)?;
let handle = tokio::spawn(async move {
    // The guard lives in the task, not in this function: dropping it stops the
    // OS watch, so its lifetime has to be the loop's lifetime. Aborting the
    // handle drops the future and therefore the guard, which is what makes
    // disarm-by-abort actually stop the watch rather than only the loop.
    let _source = source;
    run_group(name, filter, rx, supervisor).await;
});
Ok(handle)
```

Three ways an implementer silently reintroduces the bug, all of which compile and one of which is what `-D warnings` pushes them toward:

1. `let _ = source;` instead of `let _source = source;` — that drops immediately.
2. Binding it as `let (_source, rx) = watch_tree(..)?;` at the outer scope to silence `unused_variables`. The guard stays function-scoped and the watch dies at the end of `spawn_watch_group`.
3. Returning the guard alongside the handle for Task 12 to hold. That works, but it splits "stop the watch" into two actions, so `disarm`'s abort silently stops being sufficient and a missed drop leaks an OS thread per removed sheep — the exact leak Task 10's third smoke test exists to forbid.

Giving `run_group` a fifth `_source: WatchSource` parameter is also wrong, for a different reason: `WatchSource`'s only constructor is `watch_tree`, so the hand-fed-channel tests below would be forced onto a real filesystem and the pure/OS tier split this phase builds deliberately collapses.

**Neither Task 10's smoke tests nor these tests can catch a dropped guard**, which is why it is written into the contract rather than left to be inferred. Task 10 exercises `watch_tree` directly and owns the `WatchSource` itself; these tests never construct one. The only behavioural catch is Task 14's e2e case 1, three tasks downstream, where the failure surfaces in somebody else's module. Add one real-filesystem case here to close that gap, tiered with the same IR-33 real-time justification Task 10's smoke tests carry: spawn a group over a tempdir, touch a file, expect a restart; then abort the handle, touch again, expect none within a bounded wait.

Cases with their "fails if" comments: a batch of only-ignored paths produces no restart; a batch with one triggering path produces exactly one restart; two batches sent before the first restart completes produce exactly two restarts, not three (the second batch is drained as one); a batch arriving *during* a restart is re-checked after it (the spec §4 requirement, asserted by observing the second restart); dropping the sender ends the task.

Two more for the group's reach and for the error arm, which no other case reaches:

| Case | The broken implementation it catches |
|---|---|
| a name with two instances, one of them individually stopped: a triggering batch restarts both, and the stopped instance is `Online` afterwards with its `restarts` count up by one | a group loop that filters by status — the reach is the whole name-group, and this row is what pins it against a well-meaning reimplementation of the withdrawn filter |
| a triggering batch for a name the engine no longer knows: the resulting `NotFound` leaves the loop alive and the next batch still restarts a name that does exist | a loop that treats `NotFound` as fatal and returns, leaving the watch armed but deaf |

The first row is the one that would have gone the other way a draft ago, and it is worth saying why it is asserted so precisely: `Online` alone would pass against a group that never touched the stopped instance if something else had started it, so the `restarts` count is what makes the claim about *this* restart.

Every `recv().await` in these tests — on the event channel used to observe restarts — is inside a `tokio::time::timeout` naming the restart that did not arrive. The two cases that are claims about a restart *not* happening take the same form over a window that outlasts the debounce delay: the only-ignored batch, and the "exactly two, not three" row, whose third restart would arrive after the second and so is only excluded by waiting past it. Neither is a `try_recv` — Global Constraints rule 11 has the measurements, and the debounce timer is precisely the pending timer a bare poll reads too early.

- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: Commit** — `feat(daemon): watch-triggered restarts with glob filtering and re-check`

---

### Task 12: `ExtrasRegistry` — arming and disarming on lifecycle transitions

**Files:**
- Create: `crates/shep-daemon/src/extras.rs` — pure tier
- Modify: `crates/shep-daemon/src/supervisor.rs` — pure
- Modify: `crates/shep-daemon/src/boot.rs` — **unix tier**
- Modify: `crates/shep-daemon/src/testing.rs`, `crates/shep-daemon/src/lib.rs` — pure
- Modify: `crates/shep-cli/src/commands/daemon.rs` — **unix tier** (`boot_options` folds the configured `max_cron_sleep` into `BootOptions`, and `run_daemon`'s doc names the third `SHEP_*` variable)

**Interfaces:**
- Consumes: `crate::cron::{Clock, spawn_cron_worker, DEFAULT_MAX_CRON_SLEEP}`, `crate::limits::{LimitEnforcer, LimitBreach}`, `crate::probes::{Prober, LivenessFailure, spawn_liveness_task}`, `crate::watch::{WatchFilter, spawn_watch_group, DEFAULT_WATCH_DELAY}`, `crate::entry::ProcessEntry`, `crate::supervisor::SupervisorHandle`, `crate::runner::{ProcessRunner, SpawnSpec}`, `crate::assemble::assemble`, `shep_core::paths::ShepPaths`, `shep_core::protocol::BusEvent`, `shep_core::values::UpDuration::as_duration`
- Produces:

```rust
/// Where the lifecycle extras send the two out-of-band failure reports.
///
/// The matching receivers belong to the reporting task, never to the actor.
#[derive(Debug, Clone)]
pub struct ExtrasReports {
    /// Memory-limit breaches, from the enforcer.
    pub breaches: mpsc::Sender<LimitBreach>,
    /// Sheep whose liveness probe hit `failure_threshold`.
    pub liveness: mpsc::Sender<LivenessFailure>,
}

/// The four lifecycle extras, the seams they run on, and where their two
/// failure reports go.
///
/// Constructed once at boot and handed to the supervisor. Every *seam* is a
/// trait object so the engine's type does not grow a parameter per subsystem;
/// `reports` is the exception and is wiring rather than a seam.
pub struct Extras {
    /// Wall clock the cron workers read.
    pub clock: Arc<dyn Clock>,
    /// Memory-limit mechanism.
    pub enforcer: Box<dyn LimitEnforcer>,
    /// Longest a cron worker parks before re-reading the clock, from
    /// `[daemon] max_cron_sleep`. Already defaulted: this is a value, not an
    /// option, because the layer that knew whether the user set anything is
    /// behind us.
    pub max_cron_sleep: Duration,
    /// Cloned once per arming. The enforcer swallowed its own breach sender at
    /// construction; the liveness loops are free tasks and cannot, so the
    /// sender has to reach `arm` through here.
    pub reports: ExtrasReports,
}

impl Extras {
    /// The production wiring: system clock and polling enforcer over
    /// sysinfo.
    ///
    /// No prober: one is scoped to a single sheep's assembled environment,
    /// and boot has no sheep in scope.
    ///
    /// Must be called from within a Tokio runtime context: constructing the
    /// polling enforcer starts its sampling task immediately.
    #[must_use]
    pub fn real(reports: ExtrasReports, max_cron_sleep: Duration) -> Self;
}

/// Restarts each sheep reported over `breaches` or `liveness`, logging the
/// observed figure at `warn`.
///
/// Ends when both senders have dropped. Owns both receivers: the actor must
/// never block on anything a subsystem controls.
pub fn spawn_extras_reporter(
    breaches: mpsc::Receiver<LimitBreach>,
    liveness: mpsc::Receiver<LivenessFailure>,
    supervisor: SupervisorHandle,
) -> tokio::task::JoinHandle<()>;

/// Per-sheep and per-group task handles, armed on `online` and aborted on the
/// way out.
#[derive(Debug, Default)]
pub struct ExtrasRegistry { /* private */ }

impl ExtrasRegistry {
    /// Arms everything an entry's configuration asks for.
    ///
    /// `prober` is scoped to this instance's assembled `SpawnSpec` and is
    /// read only by the liveness loop; an entry configuring no
    /// `liveness_probe` never touches it.
    ///
    /// Idempotent per id: arming an already-armed id disarms it first, which
    /// is what a respawn needs — the new process has a new pid.
    pub fn arm(
        &mut self,
        entry: &ProcessEntry,
        prober: Arc<dyn Prober>,
        extras: &Extras,
        supervisor: &SupervisorHandle,
    );

    /// Aborts everything armed for `id`, and both of the name-group's
    /// per-name tasks — its watch and its cron worker — when this was the
    /// last armed instance of the name.
    ///
    /// This is what stops a stopped sheep from being restarted by a file save
    /// or a schedule: neither trigger filters by status, so a sheep stays down
    /// because nothing is left armed for it, not because something declined
    /// to restart it.
    ///
    /// Aborting the watch-group handle is sufficient to stop the OS watch:
    /// the debouncer guard rides inside the aborted future, so no second drop
    /// is needed and none is available.
    pub fn disarm(&mut self, id: u32, name: &str);
}
```

**`Extras` carries no prober, and an earlier draft of this plan said it did — the defect that draft described is one Task 9 has already shipped and already fixed.** `Extras::real` runs at boot with no app in scope, so the only prober it could construct is `OsProber::new(None, BTreeMap::new())`. `probe_exec` runs `env_clear().envs(&self.env)` (`crates/shep-daemon/src/probes/os.rs:160`), so a prober built from an empty map runs *every* exec probe — readiness and liveness alike — with no `PATH`, no `HOME`, no `USER`, no `LANG`, no `TZ`. The sharper half is `SHEP_INSTANCE` (or the app's `increment_var`), which `assemble` writes and nothing else does (`crates/shep-daemon/src/assemble.rs:135-136`): under a shared prober every instance of a clustered app probes whatever the unexpanded variable leaves behind, which is the same port, every time. Task 9 shipped exactly that and it was invisible, because the `&ResolvedApp` its first signature took structurally cannot reach `instance`; the fix was `readiness_prober(spec: &SpawnSpec)` (`crates/shep-daemon/src/supervisor.rs:1304-1321`), scoping the prober to the assembled spec's `cwd` and `env`, and a review adjudicated that departure correct. The rows this task declares would not have caught the same defect on the liveness path either: `ScriptedProber` ignores both `cwd` and `env`, so a shared prober would have shipped dead and untested twice.

**The `Prober` seam is unchanged; what moves is who holds it.** `spawn_liveness_task` still takes an `Arc<dyn Prober>` (`crates/shep-daemon/src/probes/mod.rs:123-130`), and Global Constraints' three-trait-objects claim still stands. A prober is per-sheep state scoped to one assembled `SpawnSpec`, not daemon-wide wiring like a clock or an enforcer, so it reaches the liveness loop as an argument to `arm` rather than as a field sitting beside them.

**`Extras` needs a manual `Debug`, and it is required rather than optional.** `Clock` and `LimitEnforcer` are both bounded `Send + Sync + 'static` and nothing more, so `#[derive(Debug)]` does not compile — while the workspace denies `missing_debug_implementations`, so omitting it does not compile either. Write the seams by role, not by value, and finish non-exhaustively:

```rust
impl fmt::Debug for Extras {
    // Roles, not values, for the seams: neither is Debug, and printing the
    // report channels would say nothing a reader wants. The sleep bound is
    // the exception and prints for real — it is a tuning knob the user set,
    // so a daemon log that dumps this struct should say what it ended up
    // being.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extras")
            .field("clock", &"<dyn Clock>")
            .field("enforcer", &"<dyn LimitEnforcer>")
            .field("max_cron_sleep", &self.max_cron_sleep)
            .finish_non_exhaustive()
    }
}
```

`SupervisorBuilder`'s `#[derive(Debug)]` below then works, and only then: a derive bounds the type parameter but requires every *field* type to be `Debug` unconditionally, and it will hold an `Extras`. `PollingEnforcer` needs the same decision made deliberately — it holds a seam that is not `Debug`. `SysinfoSampler` does **not**, and an earlier draft of this plan said it did: `sysinfo::System` implements `Debug`, so a plain `#[derive(Debug)]` over its `Mutex<System>` compiles as written, checked against 0.38.4 on the host and on `x86_64-pc-windows-gnu`. A hand-rolled impl there is work for nothing. `WatchSource` is likewise fine: `Debouncer<T, C>` derives `Debug` and both recommended type aliases satisfy the bounds, verified by compiling the newtype, so its `#[derive(Debug)]` stands as written.

**What is armed where, and why the shapes differ:**

| Extra | Keyed by | Armed when | Disarmed when |
|---|---|---|---|
| cron worker | sheep name | the first instance of a name goes online | the last instance of that name leaves |
| watch group | sheep name | ditto | ditto |
| memory limit | sheep id | every time an instance goes online (new pid) | that instance leaves |
| liveness probe | sheep id | ditto | ditto |

Cron and watch are per-name because both select by `ProcessSelector::Name` and would otherwise fire N times for N instances. Arming and reach agree here: both restart every instance of the name, and the per-name keying is exactly why one trigger produces one restart of the group rather than N of them. Memory and liveness are per-instance because each has its own pid and its own health.

**Disarm is the whole of what keeps a stopped sheep down, and this is the one place that fact is mechanised.** Once the last instance of a name goes terminal — stopped, errored or deleted — the group's watch and its cron worker are both aborted, so nothing is left armed that could restart it. There is no status filter anywhere downstream (Task 11 records the withdrawn one and why); a triggering file save or a 03:00 occurrence reaches every instance of a name that still has *something* armed. The user-visible rule is therefore short enough to put in a changelog line: stopping a sheep stops its watch. The one case where that rule and the group-wide reach disagree is a partially stopped group — `shep stop web-1` with `web-2` still up keeps the watcher armed, and the next save brings `web-1` back with it. That is the accepted consequence of the settled decision, not a gap.

**Where the watch's root, delay and filter come from.** No other task states this and `spawn_watch_group` takes all three as given, so `arm` is the only place they can be produced:

- **Armed at all** only when `entry.spec.config().watch` is true. It is a plain `bool` defaulting to `false` (`crates/shep-core/src/config/app.rs:112`, `:181`); without consulting it, every app in the flock gets a watch.
- **Root** is `std::fs::canonicalize(entry.spec.config().cwd)`. `cwd` is `Some` by construction here, because Task 2's `normalize` rejects `watch = true` with no `cwd` — read that task for why the daemon's own cwd is not an acceptable fallback. Canonicalizing is not tidiness: Task 11 matches by stripping the root prefix off notify's absolute paths, and on macOS a `TempDir` under `/var/...` is delivered by FSEvents as `/private/var/...`, so without it `strip_prefix` fails for every event and Task 11's own treat-as-non-triggering rule makes the watch fire never. Task 11's real-filesystem case and Task 14's e2e case 1 both walk straight into this. A failed canonicalize — the directory does not exist — arms nothing and logs at `warn` naming the path.
- **Delay** is `config.watch_delay.map(UpDuration::as_duration).unwrap_or(DEFAULT_WATCH_DELAY)`. `as_duration` is `const` and takes `self` (`crates/shep-core/src/values.rs:173`), so the `map` compiles as written.
- **Filter** is `WatchFilter::new(&config.watch_options, &config.ignore_watch)`. On `Err` — a glob the user mistyped — arm no watch and log at `warn`; a bad glob must not take down the arm path for that app's cron worker, enforcer and probe.

**Where the liveness prober comes from, and why `arm` takes one rather than building one.** The actor builds it at the call site, from a spec it assembles exactly the way `respawn` already does — `assemble(&entry.spec, entry.instance, &self.paths, entry.credentials)` (`crates/shep-daemon/src/supervisor.rs:711`; `Credentials` is `Copy`, which is why that line needs no clone) — and hands the result to `arm`. Three things follow, and none of them is optional:

- **Reuse the helper Task 9 shipped, and rename it.** `readiness_prober` (`crates/shep-daemon/src/supervisor.rs:1319-1321`) is one line — `Arc::new(OsProber::new(spec.cwd.clone(), spec.env.clone()))` — and its name is the only readiness-specific thing about it. Rename it `spec_prober`, widen its doc's opening line to name both callers, and change nothing else: a second spelling of that constructor is how the empty-env defect grows back on one path while the other stays fixed.
- **`arm` takes the prober, not the spec, because that is what keeps the fake usable.** A registry test constructs a `ScriptedProber` and calls `arm` directly, on the paused clock, on every platform this workspace compiles for. Hand `arm` a `&SpawnSpec` instead and it must build an `OsProber` itself, which pushes every liveness-wiring assertion onto real processes and a real clock — and the `#[cfg(unix)]` that then comes with them takes the Windows leg's coverage of this file along.
- **The construction is unconditional.** One `assemble` and one `Arc::new` per transition to `Online`, immediately after a process spawn. Skipping it for an app with no `liveness_probe` would mean `Option<Arc<dyn Prober>>` and an arm path that has to decide what a `None` means, which is more surface than the allocation costs.

**The cron worker's fifth argument comes from the same place, and it is not per-app.** `arm` passes `extras.max_cron_sleep` straight through to `spawn_cron_worker`. It is a daemon-wide knob rather than a Flockfile field on purpose — it tunes how the shepherd wakes up, not how any one app behaves — so it rides on `Extras` beside the seams instead of being read off `entry.spec.config()` like the watch's three. Read it from `extras`, never re-derive it.

**`SupervisorBuilder`, and why `spawn_supervisor` grows a builder rather than a parameter.** `spawn_supervisor(runner, paths, events)` (`crates/shep-daemon/src/supervisor.rs:287`) is called from `boot.rs:521`, from `crate::testing::harness`, and from this crate's own tests. Adding `extras` makes four positional parameters with two more subsystems visible on the roadmap (dogs, metrics), which is precisely the "many optional fields, call-site readability" case the maintainer's design rules name the builder pattern for. Add:

```rust
/// Builds a supervisor actor.
#[derive(Debug)]
pub struct SupervisorBuilder<R: ProcessRunner> { /* private */ }

impl<R: ProcessRunner> SupervisorBuilder<R> {
    /// A builder with no lifecycle extras: the engine spawns, restarts and
    /// kills, and nothing watches, schedules or probes.
    pub fn new(runner: R, paths: ShepPaths, events: broadcast::Sender<BusEvent>) -> Self;
    /// Wires in the lifecycle extras.
    #[must_use]
    pub fn extras(self, extras: Extras) -> Self;
    /// Spawns the actor.
    ///
    /// Must be called from within a Tokio runtime context.
    pub fn spawn(self) -> SupervisorHandle;
}
```

`spawn_supervisor` stays, documented as "shorthand for `SupervisorBuilder::new(runner, paths, events).spawn()`" (IR-28's thin-wrapper rule), so no existing call site changes and no test harness is rewritten. That is the whole reason for keeping it — a rename here would touch the engine's public surface for no behavioural gain (IR-16).

**The three supervisor call sites, and no more:**
1. the transition to `Online` in `spawn_fresh` (`:523`) — `arm`
2. the transition to `Online` in `respawn` (`:628`) — `arm`
3. `handle_exited`'s terminal branches (`:975` errored, `:980` delete, `:987` clean stop) — `disarm`

**There is no fourth call site, and an earlier draft of this plan invented one.** It named a `set_status` to `Stopping` "at the `:1068` region"; `set_status` is a two-line helper (`supervisor.rs:1068`) and `ProcStatus::Stopping` is never passed to it — the only occurrences of that variant in the whole workspace are its own declaration (`crates/shep-core/src/status.rs:20`, `:34`, `:52`) and one test match arm. The engine goes `Online → Stopped` with the kill ladder in between and no intermediate status. Do not add the transition to create the call site: it is a status-string and protocol-snapshot change for the benefit of one disarm, and it would need the existing status tests updated.

The consequence is load-bearing rather than cosmetic, and it is what makes the guard below necessary rather than belt-and-braces: **the enforcer and the liveness loop stay armed for the whole kill ladder**, seconds long by default (`kill_timeout` 1600ms, `graceful_timeout` 8000ms), and can produce a *fresh* report for a sheep that is already being stopped.

Note the interaction with Task 9: for a gated app the transition to `Online` happens in the `ReadyResult` handler, not in `spawn_fresh`. **Arming happens at the transition, wherever it is** — a liveness probe armed at spawn against an app that has not finished starting will fail its threshold and restart the app before it ever comes up. Put the `arm` call in one place that both paths reach, not two.

Every line number in that three-call-site list, and in the paragraph after it, predates Task 9's edits to both spawn paths and now points somewhere else — `spawn_fresh` opens at `:568`, `respawn` at `:700`, `handle_exited` at `:991`, `set_status` at `:1235`. Follow the function names; they have not moved and neither has the argument.

That single place also settles the prober question above. `handle_ready_result` (`crates/shep-daemon/src/supervisor.rs:1169-1195`) holds an `id` and a slot and nothing else — no `SpawnSpec`, because the spawn that assembled one returned long before — so the arm site cannot be fed a spec by its callers without the gated path smuggling one through the slot for the length of a readiness wait. Re-assembling from `&ProcessEntry` is what makes one arm site possible at all: `spec` and `instance` never change after registration (`crates/shep-daemon/src/entry.rs:31-33`), `credentials` is resolved once and reused for exactly this reason, and `assemble` is the same pure function both spawn paths call, so the spec it returns is the one the running process was spawned from rather than a second derivation of it.

**`ProcessEventKind` gives no compile-time gate here, and the plan says so rather than letting the implementer assume one.** It is `#[non_exhaustive]` (`crates/shep-core/src/protocol/events.rs:11`), so any match on it from `shep-daemon` must carry a `_` arm and E0004 cannot fire. If this task maps lifecycle events to arm/disarm at all, the `_` arm's behaviour is stated in a comment — arm nothing, disarm nothing, log at `debug` — and never a `todo!()`. Prefer not to match it: arming from the *transition sites* listed above is direct, drop-free and needs no event at all, whereas driving arm/disarm off the bus would also have to survive the bus's bounded drop-oldest queue (`shep-core/src/protocol/events.rs`, the `Dropped` variant), and a dropped `Stop` would leave a probe firing at a process that no longer exists.

**Breach and liveness reporting are consumed by a single task** — `spawn_extras_reporter`, declared above and living in `extras.rs` rather than `boot.rs`, because it is the code that turns a report into a restart and therefore has to be unit-testable and has to face the Windows compiler. In production it owns both receivers; the actor must never block on anything a subsystem controls. In tests the `Harness` owns them instead and no reporter is spawned, so a test asserts the raw report rather than racing a restart it did not trigger. Ownership is per tier, and stating only one of the two tiers is what made an earlier draft read as if two owners held one `mpsc::Receiver`.

**The reporter does not call the public `restart`, and this is the phase's sharpest correctness edge.** `ProcessSelector::Id` matches on the id regardless of status; `restart` on a sheep with no live process falls to `apply_immediate`, which resets the budget and respawns unconditionally (`supervisor.rs:816-819`). A breach or liveness report already sitting in its channel when the user runs `shep stop` is therefore delivered *after* the sheep is `Stopped` — and the daemon resurrects a process the user explicitly stopped, and reports success. The window is not a scheduler hiccup: the reporter awaits each restart, and for a live sheep that await spans the whole kill ladder, so a second queued report sits for seconds. The same stale-signal class covers a breach for pid P landing after a crash-and-auto-restart, which would spuriously restart the healthy replacement and reset its budget.

Every other deferred signal in this engine is generation-guarded for exactly this reason — `RestartDue` carries an epoch and is guarded four ways (`supervisor.rs:1014-1028`, rationale at `:117-125`), and Task 9's `ReadyResult` copies it. These two reports get the same treatment, keyed on the **pid** rather than the epoch: the epoch lives on the private `SheepSlot` and is not on `ProcessEntry`, whereas the pid is already in both the enforcer's `arm(&self, id, root_pid, limit)` and on the `&ProcessEntry` that `ExtrasRegistry::arm` receives. It is a natural generation token — `None` while not running, different after every respawn (`supervisor.rs:654`, nulled at `:887`).

Add one guarded command rather than widening `restart`:

```rust
/// Restarts `id` on behalf of a memory breach or a liveness failure, if the
/// process that produced the report is still the process running now.
///
/// Silently does nothing when the report is stale. There is no reply: a
/// dropped report is the intended outcome, not an error the reporter can act
/// on.
pub async fn extra_restart(&self, id: u32, pid: u32);
```

Its handler drops on any of four conditions, each logged at `debug` naming which guard fired: `self.shutting_down`; the slot is gone; `slot.entry.pid != Some(pid)`; `slot.entry.status != ProcStatus::Online`. Otherwise it falls through to `begin_manual(ProcessSelector::Id(id), ManualKind::Restart, ...)` — **not** to `respawn`. Delegating is what keeps the kill ladder, IMPORTANT-4's first-command-wins dedupe (`supervisor.rs:722-756`), the `pending_delete` interaction and the budget reset intact. Mirroring `handle_restart_due` literally would be wrong here: that handler respawns a sheep in `WaitingRestart`, which has no live process, while a breaching sheep is normally `Online` with a live pid — respawning it directly would put two live pids on one instance and break the first invariant Task 13 lists. The settled decision that breach and liveness restarts do not count against `max_restarts` is unaffected: `begin_manual` is still the budget-resetting path.

- [ ] **Step 1: `app_with` and `armed_entry` in `testing.rs`, and the `Harness` change**

`Harness` (`crates/shep-daemon/src/testing.rs`) gains two fields so tests can drive the extras:

```rust
pub(crate) struct Harness {
    pub(crate) ctx: RpcContext,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    /// Breach reports the supervisor's extras produce, for tests that assert
    /// a memory limit fired.
    pub(crate) breaches: mpsc::Receiver<LimitBreach>,
    /// Liveness-failure reports, for tests that assert a probe threshold
    /// tripped.
    pub(crate) liveness: mpsc::Receiver<LivenessFailure>,
    _dir: tempfile::TempDir,
    _events_rx: broadcast::Receiver<shep_core::protocol::BusEvent>,
}
```

**The harness holds both receivers and spawns no reporter**, which is the whole reason it can hold them at all. A test asserts the report itself — the id and pid that arrived — rather than the restart a production reporter would have caused. The reporter's own behaviour is tested separately, below, by handing it receivers directly.

`harness(scripts)` keeps its signature and wires scripted extras — a `ScriptedSampler::new(vec![])`, the neutral fixture described in the roster, so a harness nobody configured arms nothing and reports nothing. Its `max_cron_sleep` is `DEFAULT_MAX_CRON_SLEEP`, because a fixture nobody configured should behave like a daemon nobody configured. A second constructor `harness_with_extras(scripts: Vec<ProcScript>, extras: Extras) -> Harness` takes a caller-built `Extras`. Both are declared here and nowhere else.

**No prober is wired into the harness, and none can be**, since `Extras` no longer holds one: at the actor tier the prober is built from the assembled spec, so a harness-level test gets a real `OsProber` whatever it does. That is the position Task 9's supervisor tests are already in, and it takes the same answer — an exec probe over a real file, gated `#[cfg(unix)]` as a *test*, on the real clock, with `crates/shep-daemon/src/supervisor.rs:1605-1665` as the shipped model to copy rather than reinvent. Tests that want the fake call `ExtrasRegistry::arm` directly, which is the tier the fake belongs to anyway: `arm`'s job is handing a live sender and the right pid to a loop, not probing anything.

`armed_entry` builds the `ProcessEntry` those registry-tier tests need:

```rust
pub(crate) fn armed_entry(
    id: u32,
    instance: u32,
    pid: u32,
    app: ResolvedApp,
    paths: &ShepPaths,
) -> ProcessEntry
```

— an `Online` entry with `pid: Some(pid)`, `restarts: 0`, `started_at: None`, `budget: RestartBudget::default()`, `reload: ReloadState::None`, `credentials: None`, and its two log paths taken from `assemble(&app, instance, paths, None)` rather than invented, so a registry test's entry is shaped like one the actor really registered. It is declared here and nowhere else (rule 9).

- [ ] **Step 2: `ExtrasRegistry` and its tests**

Cases with their "fails if" comments: an app with no extras configured arms nothing (assert no tasks were spawned, by asserting the registry is empty); an app with `cron_restart` arms one worker for the name and a second instance of the same name arms no second worker; a `max_memory` app arms the enforcer with its **root pid**, and a respawn re-arms with the *new* pid (the assertion is on the pid passed to a scripted enforcer — this is the case that catches a registry that arms once and never updates); `disarm` on the last instance of a name stops the cron worker and the watch group, and `disarm` on a non-last instance does not; disarming an id that was never armed is a no-op rather than a panic.

Asserting "a task was aborted" is done by observing its effect stop, not by inspecting a `JoinHandle` — assert no restart across a window spanning the next occurrence, which is a bounded `timeout` + `recv` that must expire, per Global Constraints rule 11. That one call both crosses the occurrence and makes the claim; a `try_recv` after `advance` would report `Err(Empty)` whether or not the worker was ever aborted.

**Five more cases, because a declared channel with no test is a channel that can be wired to a dropped receiver.** The liveness path is the one seam in this phase that no other task's tests touch at all, and an implementer with no visible sender will reach for `let (tx, _rx) = mpsc::channel(1);` inside `arm` — which compiles, probes forever, and reports into the void.

| Case | The broken implementation it catches |
|---|---|
| `arm` called directly on an `armed_entry` with a `ScriptedProber` that fails `failure_threshold` times puts exactly one `LivenessFailure` on the receiver the test holds, carrying that entry's id **and pid** | `arm` creating a throwaway channel, or passing the id without the pid |
| a two-instance app whose liveness probe is `test -f <tempdir>/live-$SHEP_INSTANCE`, with only `live-0` present: one `LivenessFailure` arrives and it carries instance 1's id and pid, and a further window spanning another probe cycle stays empty | a prober built from `entry.spec.config().env`, or one built once at boot — both probe with `env_clear()` over an empty map, so `$SHEP_INSTANCE` expands to nothing, `live-` matches no file, and BOTH instances report; also catches an `assemble` call that hardcodes instance 0, which makes neither report |
| `spawn_extras_reporter`, fed a breach for an `Online` sheep whose pid matches, produces exactly one `Restart` bus event for that id | a reporter that drops every report, or one that restarts the wrong id |
| the same reporter, fed a breach for a `Stopped` sheep, restarts nothing: status still `Stopped`, no `Restart`/`Online` event | a reporter calling the public `restart`, which resurrects it |
| the same reporter, fed a breach carrying the *previous* pid after a respawn, restarts nothing: `restarts` unchanged | a guard that checks status but not pid |

The positive rows wrap their `recv().await` in `tokio::time::timeout` naming what did not arrive; the negative rows assert the same way, over a window the report or restart would have to land inside. The last two are the cases that fail against an unguarded reporter, and they are cheap: both are a scripted enforcer, a `stop` or a respawn, and one bounded wait that expires in microseconds.

**The second row is the one that catches the empty environment, so its shape is not free to vary.** It runs at the actor tier through the harness, because the whole claim is about what the *actor* assembles before it calls `arm`; a registry-tier version could only assert that `arm` forwarded the prober it was handed. Four constraints come with that:

- **A file, not a port.** `test -f` flips fail→pass with no listener, no reserved port and no race, so the only thing the case can fail on is the environment the probe ran with — the reasoning `crates/shep-daemon/src/supervisor.rs:1613-1615` already wrote down for the readiness twin.
- **Real clock, `#[tokio::test]` with no `start_paused`, and `#[cfg(unix)]` on the test rather than on anything else.** Each probe spawns a real `sh`, and a paused clock does not move the OS (`crates/shep-daemon/src/supervisor.rs:1617-1619`). Gating the test keeps the Windows leg compiling and running the registry-tier row above it.
- **`interval` at `MIN_PROBE_INTERVAL` and `failure_threshold = 1`.** The loop floors any shorter interval at 1000ms (`crates/shep-daemon/src/probes/mod.rs:87-102`), so a smaller number in the fixture would be a lie about what the test waits for. One second of real time buys the whole claim.
- **Both orderings must fail against the broken implementation.** Under a shared or config-scoped prober both instances report, and which one arrives first is a race — so assert on the id of the first report *and* keep the second window, which is what catches the other ordering. Per rule 11 that second window is a bounded `timeout` + `recv` spanning at least one interval plus one timeout, never a `try_recv`.

- [ ] **Step 3: `boot.rs` — construct `Extras::real` and hand it to the builder**

`BootOptions` (`crates/shep-daemon/src/boot.rs:426`) gains a fourth field, `max_cron_sleep: Option<Duration>`, documented as "unset means the default" — the same `Option` shape `socket` already uses at the top of that struct (`:428`), and for the same reason. The three existing struct literals all spread `..BootOptions::default()` (`boot.rs:1320`, `:1377`, `tests/daemon_e2e.rs:67`) and so are unaffected; the CLI's `boot_options` builds one exhaustively and therefore will not compile until it is plumbed, which is the gate doing the work for us. Then one change at `:521`, from `spawn_supervisor(runner, paths.clone(), events.clone())` to:

```rust
let (breach_tx, breach_rx) = mpsc::channel(...);
let (live_tx, live_rx)     = mpsc::channel(...);
let extras = Extras::real(
    ExtrasReports { breaches: breach_tx, liveness: live_tx },
    options.max_cron_sleep.unwrap_or(DEFAULT_MAX_CRON_SLEEP),
);
let handle = SupervisorBuilder::new(runner, paths.clone(), events.clone())
    .extras(extras)
    .spawn();
spawn_extras_reporter(breach_rx, live_rx, handle.clone());
```

The ordering is forced rather than stylistic: the reporter needs the handle the builder returns. `boot.rs` is the unix tier; `Extras::real` and `spawn_extras_reporter` are both pure, so the Windows leg still compiles and tests everything but this call.

**This `unwrap_or` is the one place `DEFAULT_MAX_CRON_SLEEP` is applied**, and it must stay the only one. A second default — a `unwrap_or` in `boot_options`, or a serde default back in `shep-core` — is how two supposedly identical constants drift apart. `shep-core` carries the floor and never the default; the daemon carries the default and never the floor.

On the CLI side, `boot_options` (`crates/shep-cli/src/commands/daemon.rs:125-131`) gains one line, `max_cron_sleep: config.daemon.max_cron_sleep.map(UpDuration::as_duration)` — the same `map(UpDuration::as_duration)` shape the watch delay uses, and `Option` all the way through so nothing invents a value on the way. Its existing test file gets one more case beside `boot_options_pass_ready_fd_none_and_the_configured_socket`: a config carrying `max_cron_sleep = "5m"` produces `Some(Duration::from_secs(300))`, and a config carrying none produces `None`. It fails if the field is dropped on the floor between the two structs, which is the entire failure mode of a knob nobody plumbed.

- [ ] **Step 4: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 5: Commit** — `feat(daemon): arm and disarm lifecycle extras across the sheep lifecycle`

---

### Task 13: Property tier and boundary sweeps

**Files:**
- Modify: `crates/shep-daemon/src/supervisor.rs` (its test module) — pure
- Modify: `crates/shep-daemon/src/watch/mod.rs`, `src/limits/mod.rs`, `src/probes/mod.rs` (their test modules) — pure

**Interfaces:** none new. `proptest` is already a `shep-daemon` dev-dependency (`crates/shep-daemon/Cargo.toml:58`) and `brain.rs:164` already has a property test to copy the shape from.

**The invariants, extended with this phase's events (IR-37):**
1. **Never two live pids for one instance.** The existing supervisor invariant, now exercised against interleavings that include readiness resolution, liveness failure and memory breach. The readiness case is the one that can break it: a `ReadyResult` for a stale epoch marking a respawned process online while its predecessor's exit is still in flight.
2. **The restart counter is monotonic.** Unchanged, but now reachable through three new trigger paths.
3. **Steady state is reached.** Every interleaving eventually stops producing transitions.
4. **A `WatchGroup` never has two restarts in flight.** This one is property-checkable on the group loop alone, without the supervisor: generate arbitrary batch sequences and arbitrary restart durations, assert the observed restart intervals never overlap.

Cap the case count in CI through proptest's env var, as IR-37 requires, matching whatever `brain.rs` already does.

**Boundary sweeps (IR-40) — one per subsystem, in each subsystem's own test module:**

| Sweep | Where |
|---|---|
| `watch_options` empty; a glob matching nothing (never triggers rather than erroring); a watch root that does not exist, asserted as `WatchError::Watch` carrying that path rather than as a bare `is_err()` | `watch/mod.rs` |
| `failure_threshold = 1`; `timeout` greater than `interval`; `interval` of zero | `probes/mod.rs` |
| `max_memory` smaller than any plausible reading (immediate breach on the first tick); `max_memory` exactly equal to the observed tree (no breach) | `limits/mod.rs` |
| a cron pattern that never fires again; `* * * * *`, whose next occurrence lands exactly on the `DEFAULT_MAX_CRON_SLEEP` boundary of 60s | `cron.rs` |
| a name-group with zero online instances (arm and immediately disarm) | `extras.rs` |

**Two of these rows are negative claims and take the bounded form of Global Constraints rule 11, not a poll.** The `limits/mod.rs` row's "no breach" is `assert_no_breach_within` — Task 6 owns that helper and this sweep consumes it rather than declaring a second one (rule 9). The cron row's "does not double-fire" is the same shape: a window that extends past where a second firing would land, asserted to expire. Both sweeps also cross their spans with that window rather than with `advance`, for the reason rule 11's table gives.

The cron row is a per-minute pattern, not a per-second one: the settled five-field dialect cannot express seconds at all, so "a pattern firing every second" — which an earlier draft asked for — is unwritable. Per-minute is the tightest granularity the dialect has, and it is also the more interesting boundary, because 60s is exactly `DEFAULT_MAX_CRON_SLEEP`: the clamp and the true next occurrence coincide, which is where an off-by-one in the clamp shows up. Build this worker with the default rather than a custom `max_sleep`, so the coincidence is real. Assert that the boundary neither drops an occurrence nor double-fires under the at-most-one-catch-up rule.

The floor on `max_cron_sleep` is not swept here: it is a `shep-core` config rejection with its own rows at Task 3, and `daemon.rs` is not one of this task's files.

An `interval` of zero deserves a decision rather than a sweep result: a zero-interval probe is a spin loop. Either `normalize` rejects it (Task 2's territory, and the cheaper fix) or the loop floors it at a named minimum. Whichever the implementer picks, the sweep asserts the chosen behaviour and the choice goes in the report. Task 3's `max_cron_sleep` floor is the same shape already resolved — a spin-prone duration, rejected at load rather than clamped — and the reasoning there is worth reading before choosing here. It does not settle this one: that is a daemon-wide knob in `shep.toml` and this is a per-app Flockfile field, and the argument for rejecting turned partly on where the resulting error text ends up.

- [ ] **Step 1: Extend the supervisor proptest with the three new event kinds**
- [ ] **Step 2: The `WatchGroup` single-flight property**
- [ ] **Step 3: The five boundary sweeps**
- [ ] **Step 4: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 5: Commit** — `test(daemon): property and boundary coverage for the lifecycle extras`

---

### Task 14: End-to-end tier, map.md sync, module docs, changelogs

**Files:**
- Modify: `crates/shep-cli/tests/cli_e2e.rs` — **unix tier** (the file opens `#![cfg(unix)]`)
- Modify: `crates/shep-daemon/tests/real_runner.rs` — **unix tier** (one signal, and its doc)
- Modify: `docs/systematic-refactor/refactor-workspace/map.md`
- Modify: `crates/shep-core/CHANGELOG.md`, `crates/shep-daemon/CHANGELOG.md`
- Modify: the module docs of every file this phase created

**E2E cases** — `assert_cmd`, a fresh `$SHEP_HOME` per test, no sleeps, and **every command chain carrying `.timeout(Duration::from_secs(30))` before `.output()`**, which is Phase 3's established template (`crates/shep-cli/tests/cli_e2e.rs`) and is what turns a regression into a failed assertion instead of a killed job:

1. **Watch restart.** Start a sheep with `watch = true` under a tempdir, write to a file in the tree, and observe the restart through `shep flock --format json`'s `restarts` count going from 0 to 1. Poll the command until it changes or a named deadline expires, exactly as `bleats_no_follow_until_written` does — do not sleep and assert once.
2. **Watch ignores what it should.** The same sheep, a write to a dot-file, and the restart count still 0 after the same deadline. This is the case that fails if the default ignores are dropped, and it is the reason case 1 alone is not enough.
3. **Readiness gates online.** A sheep with `wait_ready = true` whose script blocks on an explicit signal — a sentinel file the test creates — before writing `{"kind":"ready"}` to the shepherd channel: `shep flock` shows `starting`, then `online`. This is the only tier that exercises the real fd-3 channel end to end.

   **Do not script this as a delay.** `listen_timeout` defaults to 3000ms (`crates/shep-core/src/config/app.rs:105-106`, `:178`) and on elapse this phase takes the sheep `Online` anyway, so a delay-based script gives the test a `starting` window bounded above by three seconds — inside which it must fit a spawn, an RPC round trip and a JSON parse — and a loaded runner turns that into a red suite with no regression behind it. A sentinel makes the window as wide as the test needs. Raise the app's `listen_timeout` for this case too, so the observation window and the timeout window are not the same window.
4. **A bad cron pattern is a config error with the right exit code.** `shep start` against a Flockfile with `cron_restart = "not a cron"` exits `4` (invalid config, spec §9) and its stderr JSON carries the pattern. Assert the exit code and the presence of the pattern in the message — the exact wording is croner's and is not ours to pin.
5. **An `https://` probe target is a config error.** Same shape, exit `4`, message names the target.

Cases 4 and 5 are the cheapest and the most valuable: they are the proof that spec §5's "typos fail loudly at parse time" survived the trip from `normalize` through the RPC boundary to an exit code.

**The orphan-leak family, closed here because this task owns the tier that leaks.** shep has produced four orphan bugs across this port. Two were product bugs, fixed at the source and guarded: a wrapper script's forked child surviving a graceful `stop`, and probe grandchildren surviving a probe timeout. The other two were `shep-cli` test binaries found alive after thirteen hours, one of them parked in `start_asks_for_the_longer_deadline` — a hang rather than a leak, and that one is already fixed where it happened (`crates/shep-cli/src/commands/lifecycle.rs:574-590` records the missing-fixture cause, and its `recv` is bounded at `:613`); Global Constraints rules 4 and 11 are what keep it from recurring. What is *not* fixed is the leak half: the e2e harness's own teardown, which can leave a daemon and its whole flock running when a case panics. This task adds five cases to that tier, three of which start real sheep, so the harness gets fixed **before** the cases land rather than after the next stray `sleep` is found. Four concrete changes, each verified against the files as they stand:

1. **`DaemonGuard::drop` gives up silently on a pidfile it cannot read** (`crates/shep-cli/tests/cli_e2e.rs:224-228`): `read_to_string` and `parse::<i32>()` each `continue` on failure, so an unreadable pidfile means the daemon is never signalled and nothing says so. The empty case is reachable rather than theoretical. `PidfileLock::acquire` opens the file with `create(true)` and `truncate(false)` (`crates/shep-daemon/src/boot.rs:246-257`) and `record` writes the pid only after the socket is bound (`boot.rs:502`→`:506`), so in a fresh `$SHEP_HOME` — which is every case here — the file exists and is empty for the whole bind. Replace both silent `continue`s with a bounded retry for a parseable pid: a daemon still alive populates the file the moment `record` runs, and one that never populates it is one that already exited, so a short named deadline separates the two. When it expires, `eprintln!` naming the `$SHEP_HOME`. `Drop` must not panic — panicking while already panicking aborts the process — so a loud line is the strongest signal available here, and it is strictly better than the silence it replaces.

2. **Nothing reaps the sheep, and the guard's own doc says so** (`cli_e2e.rs:189-204`). `graceful_kill` drives the daemon's real kill ladder and every success path calls it, but a panicking case never reaches it, and SIGKILLing the daemon does not reach a sheep: `TokioRunner` deliberately gives each sheep its own process group so the daemon's `kill_tree` can target one without hitting itself. That doc calls this a gap the tier has "no RPC-free way" to close; there is one, and it costs a line. Every fixture script this file writes goes through `write_script` (`:135-142`), so having `write_test_script` and `write_logging_script` open with `echo $$ >> "<home>/fixture.pids"` — the case's own `$SHEP_HOME`, spelled absolutely, since the script's cwd is not this test's to assume — records every sheep the case ever had, one line per spawn, restarts included. `$$` in `/bin/sh` is the pid the daemon tracks and the leader of its own group, so `-pid` per line reaches its lambs too. The append goes to a file and never to stdout, so case 4's byte-exact `bleats` fixture is untouched. Two ordering constraints, both load-bearing: kill the daemon **first**, or autorestart brings back everything the sweep just killed; and gate the sweep on `std::thread::panicking()`, exactly as `crates/shep-daemon/tests/real_runner.rs`'s `Reaper` does (`:150-171`) and for the reason it already states — on the success path `graceful_kill` has proven them gone, and signalling a pid the OS may have recycled is a hazard, not a safety net. Rewrite the guard's doc: the gap it documents is the gap this closes.

3. **Case 8 sets exactly the timeout this file's own constant exists to forbid.** `home_reaches_the_spawned_daemon` (`cli_e2e.rs:905-938`) carries an inline `.timeout(Duration::from_secs(30))`, and `SPAWN_DEADLINE` is `Duration::from_secs(30)` (`crates/shep-client/src/spawn.rs:24`). `CMD_TIMEOUT`'s doc (`cli_e2e.rs:39-52`) says in as many words that a bound merely equal to `SPAWN_DEADLINE` races `assert_cmd`'s kill against the autostart path's own report, and that on a loaded machine the kill wins. When it wins, the CLI dies with a daemon it launched still booting — and that daemon survives, because `probe_until_ready` never kills or waits its child (`crates/shep-client/src/spawn.rs:283-339`) and `launch.rs:72` gave it its own process group, so `assert_cmd`'s kill reaches the CLI and stops there. Use `CMD_TIMEOUT`, add the `graceful_kill(dir.path())` this case alone omits, and rewrite the doc at `:900-904` that defends both omissions as "the brief's own given form, verbatim" — that defence is the reason the defect survived this long.

   **`spawn.rs` is deliberately not changed.** Whether an autostart that exhausts its deadline should kill the daemon it launched is a real product question with a real argument on the other side — the daemon may be a second from serving, and may not even be the one this process launched — and a test tier is the wrong place to settle it. What this task takes from it is the constraint: nothing in `assert_cmd`'s timeout reaps a daemon, so the guard is the only thing that can.

4. **`real_runner.rs`'s `Reaper` signals the leader where its sibling signals the group.** `Reaper::drop` (`crates/shep-daemon/tests/real_runner.rs:159-171`) sends SIGKILL to `Pid::from_raw(pid)`, while `daemon_e2e.rs`'s `Fixture::drop` (`:140-153`) sends it to `-pid` under a doc explaining that group-wide is what reaches the `sleep` grandchild a leader-only signal misses. The two agree on the happy path only because `a_graceful_stop_reaches_a_forked_grandchild` pushes its grandchild's pid explicitly (`real_runner.rs:241`) — but a panic anywhere between the spawn at `:217` and that push, which includes the group-leader assertion at `:225-231` and the five-second wait for the wrapper's pid at `:236-239`, leaves an untracked `sleep 30` behind with nothing tracking it. That same test asserts the leader property the group signal depends on, so `-pid` is safe exactly where it is needed. Make it `-pid`, and name the sibling file in the doc as the model.

**Acceptance: a full suite run leaves nothing new reparented to init.** Checked after **both** suite runs in Global Constraints' gate list — the default one and the `--test-threads=1` one — because interleaving is what decides which teardown wins:

```
ps -eo ppid=,command= | awk '$1 == 1' | sort > /tmp/shep-orphans-before
cargo test --workspace --all-features
ps -eo ppid=,command= | awk '$1 == 1' | sort > /tmp/shep-orphans-after
diff /tmp/shep-orphans-before /tmp/shep-orphans-after
```

`diff` must report nothing added. A before/after rather than a grep for names is deliberate: a leaked sheep is `/bin/sh <tmpdir>/sheep.sh`, a leaked lamb is a bare `sleep 60`, and a leaked daemon is `target/debug/shep daemon` — three unrelated spellings, one of which any developer machine may legitimately be running already. The diff names whatever appeared without having to predict it.

**Calibrate the check before trusting it.** All four fixes above govern the panic path, and a green suite never takes it, so a clean `diff` on a passing run proves nothing about them. Force one panic — a temporary `assert!(false)` at the end of case 1, which by then has a daemon and a live sheep — and confirm the `diff` is clean anyway. That single run is the whole evidence that the guard, the pid sweep and the ordering work; without it this criterion is a check nobody has ever seen fail.

**map.md sync — three drifts, recorded not silently fixed:**
- `probes/` is a module map.md never named; spec §7 requires it (`docs/specs/shep-v1.md:8-9` — where the two disagree the spec wins and map.md gets fixed).
- `watcher.rs` became `watch/` (two files: the OS seam and the filtering logic), under the maintainer's 500-line split rule and because the two halves have different test tiers.
- `worker.rs` was not built; its interval loops live in `cron.rs` and `limits/mod.rs` with the subsystems they serve.

Also record `limits/` and `probes/` being directories rather than files, and the three new seam traits, so map.md remains a module-level design a reader can navigate from.

**Module docs (IR-27) are decision guides, not API dumps.** Each new module's `//!` header answers the question a user of that module actually has:
- `cron.rs` — when a cron restart fires relative to wall-clock changes, why a missed occurrence is not replayed, and what `max_cron_sleep` trades: shorter recovers faster from a suspend, longer wakes less often, and neither changes whether an occurrence fires.
- `limits/` — what "the process tree" means and why it is not the root pid, with the pm2 deviation callout in spec §4's voice.
- `probes/` — which readiness source wins when more than one is configured, and what a readiness timeout does versus a liveness failure.
- `watch/` — what is ignored by default, how `watch_options` and `ignore_watch` compose, and **that a triggering change restarts every instance of the name while stopping a sheep disarms its watch**. That last clause is the one a user needs and the one this phase most nearly left as folklore; it belongs in the header, not in a comment three functions down.

Each gets its links in a bottom reference block, and each names its honest caveats inline: no TLS, no redirects, polling granularity, real-time debounce.

**Changelogs (IR-45).** `shep-core` gets the cron dialect change under both Additions and Changes, and the entry a user needs is the one Task 1 Step 6 spells out. It moves in both directions and the two must not be blurred into one bullet: under Changes, patterns that used to pass now fail (five tokens of garbage, and the `L`/`W`/`#`/`?` extensions) plus `@reboot`'s new message; under Additions, the seven vixie nicknames that the token-count stopgap rejected and shep now expands. Six-field and seconds-bearing patterns are in neither list — they were rejected before and are rejected now, only with a better sentence.

`shep-daemon` gets the four subsystems, the `Online` timing change for readiness-gated apps (bus-visible even though no wire type changed), and the new `watch = true` requires `cwd` rejection. It also gets the sentence a user needs about watch's reach, and it is worth writing carefully because it has two halves that sound contradictory until they are stated together: a triggering change restarts **every** instance of the name, and stopping a sheep disarms its watch. The consequence a partially stopped group sees — `shep stop web-1` with `web-2` still up, then a save brings `web-1` back — follows from both and should be spelled out rather than left for someone to discover.

`shep-core` gets the cron dialect entry above plus the new `[daemon] max_cron_sleep` key: what it defaults to, that `SHEP_MAX_CRON_SLEEP` overrides it, and that a value under one second is rejected rather than clamped. Note the millisecond grammar in the entry itself — `"60"` is sixty milliseconds and `"60s"` is what the reader means — because a changelog is where someone reads about a new key for the first time. The `DaemonConfigError::BelowMinimum` variant goes under Changes rather than Additions: that enum carries no `#[non_exhaustive]`, so growing it breaks any downstream `match`, and IR-45 wants that said rather than filed as a feature.

- [ ] **Step 1: Close the orphan-leak family, calibrate the acceptance check, and commit it alone** — `fix(test): reap a case's whole flock when it panics, and signal the group`

  First, because the five cases below add three more sheep-starting cases to the tier being fixed. Its own commit, because it is a repair to the harness rather than coverage of this phase, and it should be revertable and bisectable without taking this task's e2e cases with it.

- [ ] **Step 2: Write the five e2e cases, run, confirm each fails against a stub before it passes**
- [ ] **Step 3: map.md sync**
- [ ] **Step 4: Module docs pass across every new file**
- [ ] **Step 5: Both changelogs**
- [ ] **Step 6: Run the full gate list from Global Constraints, each from its own exit code, then the orphan diff after each of the two suite runs**
- [ ] **Step 7: Commit** — `test(cli): end-to-end coverage for watch, readiness and config rejection`

---

## Exit criteria

1. All fourteen tasks complete and individually reviewed.
2. Every gate in Global Constraints green from its own exit code — including `cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu` and the two bench-crate gates.
3. `grep -rn "cfg(unix)\|cfg(windows)" crates/shep-daemon/src/{cron.rs,limits,probes,watch,extras.rs}` returns **only** the two shell-selection arms in `probes/os.rs` and, on the exec *tests* in that same file, whichever gating Task 8 chose — `#[cfg(unix)]`, or a `#[cfg(unix)]`/`#[cfg(windows)]` pair. Task 8 offers both and either is fine; what the criterion forbids is a `#[cfg]` on a `mod` line, which is the shape that would mean a module failed the pure-tier claim.
4. `grep -rn "ponytail" crates/shep-core/src crates/shep-daemon/src` returns nothing for cron — the stopgap note at `normalize.rs:55-56` is gone along with the stopgap.
5. Rule 10, checked on the delta rather than on the tree. Two halves:

   ```
   grep -rn "Phase 4\|Task [0-9]" \
     crates/shep-core/src/config/cron.rs crates/shep-core/src/config/probe.rs \
     crates/shep-daemon/src/{cron.rs,extras.rs,testing.rs} \
     crates/shep-daemon/src/{limits,probes,watch} \
     crates/shep-daemon/tests/external_impls.rs \
     benches/Cargo.toml benches/benches/memory_sample.rs
   ```

   returns nothing — those are the files this phase **creates**, and `testing.rs` is on the list because Task 3 moves two Phase 1-3 task references into it and then rewrites them. The two `benches/` paths are on it because the bench crate ships (settled below) and rule 10 binds a manifest as much as a source file; they were left off an earlier draft only while shipping was still an open question. For files this phase only *modifies* (`supervisor.rs`, `lib.rs`, `boot.rs`, `normalize.rs`, `config/daemon.rs`, `config/mod.rs`, `commands/daemon.rs`, `cli_e2e.rs`, `Cargo.toml`, the CHANGELOGs), rule 10 binds the lines this phase adds; check it in review of the diff.

   **The 39 pre-existing markers on `main` that survive this phase are out of scope and must not be rewritten.** An unscoped `grep -rn "Phase 4\|Task [0-9]" crates/ Cargo.toml` returns 41 hits today — 34 Phase 1-3 task references and 7 `Phase 4` promises, no overlap — of which this phase retires exactly two, both `Phase 4` markers on the readiness path. That leaves 34 task references plus the five `Phase 4` promises the paragraph below deals with, and it is why the tree-wide form was replaced.

   The two retirements are a positive requirement, since the scoped grep no longer reaches them: Task 9 removes both `Phase 4` markers on the readiness path — the `Msg::Ready` doc at `supervisor.rs:129` and the debug line at `supervisor.rs:432`. Verify both are gone.

   Five other `Phase 4` promises survive in code this phase does not touch (`boot.rs:763`, `:884` log flush/reopen; `supervisor.rs:1223`, `:1226` child metric and child action reply; `completions.rs:12`), all of them in the out-of-scope list at the top of this plan. After Phase 4 ships without delivering them those comments become false promises: re-point them at the phase that will, or say so in the report. Do not let a grep deputize an implementer to rewrite them silently.
6. `grep -rn "assert_ne!" crates/shep-daemon/src crates/shep-core/src` returns nothing new.
7. A report to the maintainer listing: every third-party API where the real documentation differed from `docs/research/phase4-lifecycle.md`; every judgement call made on her behalf; the bench numbers and the machine they came from; and the `interval = 0` decision from Task 13. Three items are already known to belong in it and should not have to be rediscovered — the sysinfo pin held at 0.38.x for MSRV rather than tracking 0.39, the bench crate's MSRV spelled literally in a second place because IR-5's own-`[workspace]` rule forbids inheriting it, and the bracketed-IPv6 decision from Task 2. Two items an earlier draft listed here are gone because they are no longer open questions: whether the cron worker restarts stopped instances (it does, and so does watch), and whether the bench crate shipped (it does).
8. Every test added by this phase has its "fails if" comment. A reviewer spot-checking three of them at random should be able to break the implementation in the named way and watch the named test go red.
9. Neither suite run leaves a process reparented to init — the before/after `diff` at Task 14, run after `cargo test --workspace --all-features` and again after the `--test-threads=1` pass, adds nothing. Includes the calibration run Task 14 specifies: a deliberately panicking case must also leave the diff clean, since a green suite never exercises the teardown any of it governs.

## Open questions for the maintainer

**All ten are settled (the maintainer, 2026-08-08); nothing here is open.** The first four were settled when the plan was drafted. The rest closed over three later rounds — some from the original draft's open list, some surfaced by the adversarial verification pass — and the last round of three closed the section. No implementer is blocked on anything below, and none of it is theirs to revisit.

- **Cron dialect: five-field standard cron only.** Widening a grammar later is backwards-compatible, narrowing one is not, so the wide default was the expensive direction to guess. See Task 1 for the `Seconds::Disallowed` setting, the extension-character rejection, and the day/month-name trap it has to avoid.
- **`https://` probe targets: rejected at config time**, with a typed error. A probe that silently fails every poll is indistinguishable from an app that is down, so failing loudly at config time is the honest option. A user with an HTTPS health endpoint cannot use a readiness probe in v1; that is the accepted cost of keeping a TLS stack out of the daemon (decision D1).
- **Memory-limit scope: the process tree**, sheep plus lambs. The tree is what gets killed, so the tree is what gets measured, and a root-pid limit is trivially dodged by any app that forks workers. This deviates from pm2 and wants a line in `docs/migration.md`: someone migrating with `max_memory` on a forking app will see restarts pm2 never gave them.
- **Breach and liveness restarts do not count against `max_restarts`.** Both keep routing through the path that resets the budget, matching spec §4's wording that only exits inside `min_uptime` count as unstable. A memory-leak loop therefore restarts indefinitely rather than reaching `errored` — accepted, on the grounds that a supervisor's job is keeping things up. Revisit if it proves wrong in practice.
- **`watch = true` with no `cwd`: rejected at config time**, with a typed `NormalizeError::WatchWithoutCwd` naming the app. Both alternatives were rejected for named reasons. Arming nothing and logging a warning is the silent-failure shape this project keeps rooting out — the user learns their watch does nothing at the moment they wonder why saving a file changes nothing, which is the same fault the `https://` rejection exists to avoid. Defaulting to the daemon's own cwd is worse than quiet, it is dangerous: nothing in this workspace chdirs, so under a systemd unit with no `WorkingDirectory=` that default is `/` and shep would arm a recursive watch over the whole filesystem. Task 2 owns the variant and its three test rows; an app carrying `watch_options` with `watch = false` is deliberately left alone.
- **Cron nicknames: the vixie set is accepted, `@reboot` is not.** `@yearly`, `@annually`, `@monthly`, `@weekly`, `@daily`, `@midnight` and `@hourly` are accepted and **shep expands each one itself, before the pattern reaches croner** — so croner never sees a nickname and the settled grammar stays literally five-field. These are textual sugar, not a wider dialect. The reason this does not reopen the five-field decision is that the vixie set is *closed*: it has been these seven spellings for decades and commits shep to nothing further, unlike croner's `L`/`W`/`#` extensions, which are that crate's own dialect and grow with it. Expanding ourselves is also what makes the set coherent — croner's own table has no `@midnight` arm, so delegating would accept `@daily` and reject `@midnight`. `@reboot` is refused with its own message: "run once at boot" has no meaning for a supervisor that already owns restart policy. Task 1 carries the expansion table and the rows in both directions.
- **A readiness timeout on start takes the sheep online, with a warning.** Treating a slow start as a spawn failure produces exactly the restart loop `max_restarts` exists to contain, out of an app that is slow rather than broken. `errored` is the honest-but-brittle reading and is rejected on that basis; a dedicated `ReadinessTimeout` event is wire-additive and therefore out of this phase either way. Task 9 owns the behaviour and the warning.
- **Watch and cron both restart the whole name-group, stopped instances included.** An earlier round settled the opposite for watch and left cron open; this reverses it and closes both at once. The deciding argument is that disarm-on-stop already covers the case anyone hits: a fully stopped sheep has no armed watcher and no armed cron worker, so neither trigger can reach it, and the filter would only ever have been visible for a *partially* stopped multi-instance group — some instances up keeping the watcher armed, one stopped by hand. Against that corner, the skip-stopped machinery was a scope parameter on the actor's manual-command path, a new `Msg` variant and a second public restart verb: engine surface, added by the phase whose whole shape is bolting subsystems on without reaching in. Group-wide also matches spec §4's own one-watcher-per-name-group wording, and it keeps watch and cron identical, which is what leaving cron alone always implied. **The protection users actually rely on is that stopping a sheep disarms its watch** — Task 12 mechanises it, Task 11 records the withdrawn filter so nobody rebuilds it, and Task 14 puts the sentence in the `watch/` module header and the changelog so it is not folklore.

- **The bench crate ships in this phase.** Task 5 builds it as written. `MEMORY_POLL_INTERVAL`'s comment is required by IR-26 to be benchmark-backed, and the difference between "backed" and "asserted" is whether the next person can re-run the measurement: a committed criterion harness can be re-run on their machine, on a bigger flock, after a sysinfo upgrade, while a number somebody once measured and typed into a comment is unfalsifiable the day it lands. Yes, it is the only task shipping no user-visible behaviour. It is also the workspace's first bench harness, and IR-5 already specifies the shape, so the marginal cost is the run itself.

- **`max_cron_sleep` is a daemon-config knob defaulting to 60s.** Sixty seconds stays the default and the reasoning is unchanged — a suspended laptop or an NTP step costs at most that much drift, and a dev machine suspending is the common case rather than the exotic one — but it stops being a hard `const`. It lives in `[daemon]` as `max_cron_sleep: Option<UpDuration>`, overridable by `SHEP_MAX_CRON_SLEEP`, with the default applied once in `boot.rs` so "unset" survives the whole file < env < flags layering. Values under one second are **rejected** at config load, not clamped: below that the loop stops being a scheduler and becomes a spin that still fires correctly, and clamping would announce itself only in a detached daemon's log file. There is no upper bound and no CLI flag, both for stated reasons at Task 3, which owns the knob, its floor and its tests; Task 12 owns the plumbing through `BootOptions` and `Extras`. The at-most-one-catch-up rule the old open item bundled with this one is unchanged and was never really the same question: it is about what a worker does on wake, not how often it wakes. Spec §5 describes `[daemon]` as "log policy, socket overrides" — illustrative rather than exhaustive, and the spec is the maintainer's to amend if she wants the third key named there.

**Two things this section does not decide, recorded so nobody re-opens them from the old numbering.** Whether a watch should restart the group *rolling* rather than all at once is out of scope rather than undecided: a rolling restart keeps the app serving through a save, which is what reload is for, and reload is on this phase's out-of-scope list. And the `interval = 0` probe question is not here because it is not the maintainer's — Task 13 states it as the implementer's call between rejecting at `normalize` and flooring at a named minimum, with the choice going in the report either way.
