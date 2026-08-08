# shep Phase 4 — Lifecycle extras Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
> **REQUIRED SKILL:** invoke `shep-idiomatic-rust` before writing ANY Rust here. Cite rules as `IR-<n>`.

**Goal:** Give the merged supervisor the four lifecycle extras spec §4 and §7 promise and the engine does not yet have: restart on file change, restart on a cron schedule, restart on a memory-limit breach, and readiness/liveness probes.

**Architecture:** Four independent subsystems, each a task (or task pair) that owns one module and one seam trait, plus one integration module that arms and disarms them. None of them reaches into the supervisor actor's state: watch, cron and limits all trigger a restart through the *existing public* `SupervisorHandle::restart(ProcessSelector)` (`crates/shep-daemon/src/supervisor.rs:210`), which already resets the restart budget the way spec §4 wants a non-crash restart to. Only readiness needs actor surgery, because only readiness changes what `starting → online` means.

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
  | `criterion` | Task 5 | `benches/` |
  | `notify`, `notify-debouncer-full` | Task 10 | `shep-daemon/src/watch/source.rs` |

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

Beyond those: paused tokio clock where time matters (`#[tokio::test(start_paused = true)]` is this crate's default — real time needs a justifying comment, IR-33), no sleeps as synchronization, hand-rolled fakes against the real traits, a unique config literal and tempdir per test (IR-34). **No test may touch the real `$HOME` or `$SHEP_HOME`, and none may bind a real unix socket** — every fixture is rooted in its own `tempfile::TempDir`, exactly as `crate::testing::test_paths` (`crates/shep-daemon/src/lib.rs`) already does. The two tiers that legitimately touch the real OS — the filesystem-watch smoke test and the loopback HTTP probe — still bind only to a tempdir and to `127.0.0.1:0`.

**Style (from docs/idiomatic-rust.md — cite by number in reviews)**
- `impl core::error::Error`, never `std::error::Error` (IR-19). Per-module error enums whose variant docs state the precise condition, not the variant name restated (IR-18). No crate-wide umbrella error.
- Every `Result`-returning public fn carries a `# Errors` section (IR-28).
- `# Panics` and `#[track_caller]` travel together, or neither appears (IR-21).
- No panicking constructors outside `shep-cli` (IR-21) — which means none at all in this phase, since it touches only `shep-core` and `shep-daemon`.
- `#[non_exhaustive]` where growth is genuinely anticipated, with a comment saying why (IR-20). `ProbeFailure` and `CronParseError` qualify; a two-variant internal enum does not.
- **No magic numbers.** Every duration, threshold and cap is a named `const` with a comment giving the unit and the reason (IR-26). This phase introduces six — `MAX_CRON_SLEEP`, `MEMORY_POLL_INTERVAL`, `HTTP_STATUS_LINE_CAP`, `DEFAULT_WATCH_DELAY`, `DEFAULT_IGNORE_GLOBS`, `WATCH_SMOKE_DEADLINE` — and each is declared at exactly one task.
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
                                      notify, notify-debouncer-full, criterion; benches member

crates/shep-core/
  Cargo.toml                          + croner, chrono, chrono-tz
  src/config/cron.rs         [pure]   CronSchedule: parse + next_after. croner and chrono-tz
                                      are private implementation; neither appears in the
                                      public signature.
  src/config/probe.rs        [pure]   ProbeTarget: the parsed, validated form of
                                      ProbeConfig::target, one variant per ProbeKind
  src/config/normalize.rs    [pure]   MODIFY: the 5-field cron stopgap (lines 54-60) becomes
                                      a real parse; probe targets get validated
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
  src/supervisor.rs          [pure]   MODIFY: SupervisorBuilder, the readiness gate, four
                                      arm/disarm call sites
  src/boot.rs                [unix]   MODIFY: construct the real seams and hand them to the
                                      builder
  tests/external_impls.rs    [pure]   compile-only proof that an outside crate can implement
                                      the three new traits (IR-38)

benches/                              new workspace member, publish = false, own [workspace]
  Cargo.toml                          criterion, harness = false
  benches/memory_sample.rs            the number behind MEMORY_POLL_INTERVAL
```

`watcher.rs` and `worker.rs` — both named in map.md — are **not** created. `watcher.rs` becomes the `watch/` directory because Rin's 500-line split rule bites once the OS seam and the filtering logic share a file, and because the two halves have genuinely different test tiers (real filesystem versus paused clock). `worker.rs` was map.md's host for interval tasks; this phase's two interval loops live with the subsystems they serve (`cron.rs`, `limits/mod.rs`), which is one fewer indirection and one fewer place to look. `probes/` is a module map.md never named at all; spec §7 requires it and the spec wins (`docs/specs/shep-v1.md:8-9`). Task 14 records all three in map.md.

`probes/` and `limits/` are directories rather than single files for the same 500-line reason: `probes.rs` would hold a trait, three transport implementations, a threshold engine and a readiness state machine, and `limits.rs` would hold two seams plus a tree-walking sampler. Splitting them at the seam boundary also makes each task's review surface one file.

**There is no `shep-core/src/config/error.rs`.** Each new error enum lives in the module that produces it (IR-18): `CronParseError` in `cron.rs`, `ProbeTargetError` in `probe.rs`. `NormalizeError` (`crates/shep-core/src/config/normalize.rs:84`) grows variants but wraps neither of them — see the next paragraph, which is not optional reading.

**`NormalizeError` cannot wrap `croner::errors::CronError`, and the reason is a hard API fact.** `NormalizeError` derives `Debug, Clone, PartialEq, Eq` (`normalize.rs:83`) and its tests compare whole values (`normalize.rs:137-140`, `:159-160`, `:179-182`). `CronError` implements `Debug`, `Display` and `Error` — and **not** `Clone`, and **not** `PartialEq`. Wrapping it would force `NormalizeError` to drop three derives and would break six existing assertions in a module this phase is otherwise only extending. The new variants therefore carry owned `String`s: the offending pattern, plus the reason rendered from `Display`. That is also better error text — the user gets croner's sentence without the type.

---

## Fixture roster — every fake, one owner, full signature

All of these live in `crates/shep-daemon/src/testing.rs`, which is `#[cfg(test)] pub(crate) mod testing;`. They are **not** behind `test-fakes`: that feature exists to expose `fake::ScriptedRunner` to *other crates'* tests (`crates/shep-daemon/Cargo.toml:11-13`), and nothing outside this crate needs a scripted prober. Keeping them `#[cfg(test)]` also keeps `missing_docs` off them, so a fixture can carry an IR-33 `//` WHY comment instead of a `///` doc it does not need.

The module currently exists as an inline `mod testing { ... }` inside `lib.rs`. **Task 3 moves it to its own file** — unchanged, paths and all (`crate::testing::harness` keeps working) — because lib.rs is 341 lines and this phase would push it well past Rin's 500-line split rule. Nobody else moves it.

| Helper | Owner | Signature |
|---|---|---|
| `TestClock` | Task 3 | `pub(crate) struct TestClock { epoch: DateTime<Utc>, started: tokio::time::Instant }` |
| `TestClock::starting_at` | Task 3 | `pub(crate) fn starting_at(epoch: DateTime<Utc>) -> Self` |
| `impl Clock for TestClock` | Task 3 | `fn now_utc(&self) -> DateTime<Utc>` — `epoch + (Instant::now() - started)`, so `tokio::time::advance` moves wall time |
| `ScriptedSampler` | Task 4 | `pub(crate) struct ScriptedSampler { readings: Mutex<VecDeque<Vec<ProcessRss>>>, calls: AtomicUsize }` |
| `ScriptedSampler::new` | Task 4 | `pub(crate) fn new(readings: Vec<Vec<ProcessRss>>) -> Self` — the last reading repeats once exhausted |
| `ScriptedSampler::calls` | Task 4 | `pub(crate) fn calls(&self) -> usize` |
| `rss` | Task 4 | `pub(crate) fn rss(pid: u32, parent: Option<u32>, bytes: u64) -> ProcessRss` |
| `ScriptedProber` | Task 7 | `pub(crate) struct ScriptedProber { script: Mutex<VecDeque<Result<(), ProbeFailure>>>, calls: AtomicUsize }` |
| `ScriptedProber::new` | Task 7 | `pub(crate) fn new(script: Vec<Result<(), ProbeFailure>>) -> Self` — the last outcome repeats once exhausted |
| `ScriptedProber::calls` | Task 7 | `pub(crate) fn calls(&self) -> usize` |
| `probe_config` | Task 7 | `pub(crate) fn probe_config(kind: ProbeKind, target: &str) -> ProbeConfig` |
| `HttpReply` | Task 8 | `pub(crate) enum HttpReply { Status(u16), Raw(String), Hang }` |
| `loopback_http` | Task 8 | `pub(crate) async fn loopback_http(script: Vec<HttpReply>) -> (SocketAddr, tokio::task::JoinHandle<()>)` — binds `127.0.0.1:0`, serves one reply per connection in order |
| `touch` | Task 10 | `pub(crate) fn touch(root: &Path, rel: &str) -> std::io::Result<PathBuf>` — creates parent dirs, writes one byte, returns the absolute path |
| `app_with` | Task 12 | `pub(crate) fn app_with(name: &str, edit: impl FnOnce(&mut AppConfig)) -> ResolvedApp` — `AppConfig::minimal(name, "./srv")`, `edit`, then `normalize().unwrap()` |

`ScriptedSampler` and `ScriptedProber` both repeat their final scripted value rather than panicking on exhaustion. This is deliberate and it is the difference between a useful fake and an irritating one: a liveness test that scripts three failures wants the fourth poll — if the implementation wrongly makes one — to *also* fail, so the assertion is about the threshold count and not about the fake running dry. Both expose `calls()` so a test can assert the exact number of polls, which is the claim that catches an off-by-one threshold.

`Harness` (`crates/shep-daemon/src/lib.rs`, inside `mod testing`) keeps its current shape until Task 12, which adds one field for the extras handle. Its signature after that change is stated at Task 12 and nowhere else.

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
    /// The pattern is not valid in croner's dialect. Carries the pattern and
    /// croner's own rendered reason.
    Pattern { pattern: String, reason: String },
    /// The `cron_timezone` value is not a name in the IANA database.
    Timezone { name: String },
}

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

Do **not** call `.dom_and_dow(true)`. croner's default is OR semantics between day-of-month and day-of-week, which is what the JS-croner dialect map.md promises does; `true` switches to AND and would silently change what an existing pattern means.

**The dialect is five-field standard cron only (Rin, 2026-08-08).** She chose the narrow reading over croner's full dialect: widening a grammar later is backwards-compatible, narrowing one is not, so the wide default was the expensive direction to guess.

Call `.seconds(Seconds::Disallowed)`, which rejects the six-field form for us.

croner still accepts `L`, `W`, `#` and `?` natively, so **rejecting those is our job, not croner's** — a pattern reaching `CronParser::parse` with them in it parses happily and we would have shipped the wide dialect by accident. Reject them before parsing, with a typed error naming the offending character.

**The trap in that check, which a naïve implementation will hit:** day and month names contain those letters. `WED` contains `W`; `JUL` contains `L`. A character scan over the raw pattern rejects `0 0 * JUL WED`, which is valid standard cron.

So the check is token-aware, not character-wise: split on whitespace, and within each field treat a recognised three-letter name (`JAN`-`DEC`, `SUN`-`SAT`, case-insensitive) as opaque before looking for extension characters in what remains. `#` and `?` never occur inside a name and may be rejected anywhere; `L` and `W` may not.

Test both directions explicitly: `0 0 L * *` and `0 0 * * 5#3` and `0 0 ? * *` are rejected with the character named, while `0 0 * JUL WED` and `0 0 * * MON-FRI` are accepted. A test suite that only covers the rejections would pass an implementation that rejects every name-bearing pattern.

Note what this fixes in the existing stopgap, which is wrong in both directions: it accepts `not a cron five` (five whitespace-separated tokens) and rejects `0 30 2 * *` for reasons unrelated to the grammar.

`docs/migration.md` gets a line: a pm2 user whose pattern used `L`, `W`, `#`, `?` or a seconds field now gets a config error naming the character, rather than silently different behaviour.

- [ ] **Step 1: Add the three dependencies**

Root `Cargo.toml`, in `[workspace.dependencies]`:

```toml
# The cron_restart dialect (spec §4). `serde` is croner's only feature and we
# do not serialize a parsed Cron — the Flockfile carries the pattern string.
croner = { version = "3.0.1", default-features = false }
# croner is generic over chrono::TimeZone; `clock` is what provides Utc::now(),
# which the daemon's cron worker reads to derive the next occurrence.
chrono = { version = "0.4.42", default-features = false, features = ["clock", "std"] }
# Resolves cron_timezone's IANA name to a chrono::TimeZone. croner carries this
# only as a dev-dependency, so it is ours to declare.
chrono-tz = { version = "0.10.4", default-features = false, features = ["std"] }
```

`crates/shep-core/Cargo.toml` opts all three in with `.workspace = true`.

The chrono floor of `0.4.42` is croner 3.0.1's own declared minimum per the research note. **Verify it during the rehearsal rather than trusting it**: if `-Z minimal-versions` resolves chrono below 0.4.42 and the build succeeds, drop our floor to `0.4` and let croner's own bound do the work — an unnecessary floor pin is a maintenance burden with no comment that can honestly justify it. If it fails, the pin stays and its comment names the *API* that forced it, not the version.

- [ ] **Step 2: Write `cron.rs` with its pinned-array tests first (TDD)**

The tests are the interesting half of this task, because `next_after` is a pure function of `(pattern, zone, instant)` and therefore admits exact expected values. Required cases, each asserting a **pinned array** of successive occurrences (IR-36) rather than a single "is it after now" property:

| Case | Pattern | Zone | The broken implementation it catches |
|---|---|---|---|
| five-field baseline | `0 3 * * *` | none (UTC) | a parser configured with `Seconds::Required`, which would reject it |
| six-field seconds | `30 0 3 * * *` | none | a parser configured with `Seconds::Disallowed` (croner's default), which is what the dialect promise forbids |
| zone offset | `0 3 * * *` | `Europe/Oslo` | a `next_after` that ignores the zone and returns 03:00 UTC — the two answers differ by one or two hours, and the assertion is on the exact UTC instant |
| zone across midnight | `30 23 * * *` | `Pacific/Auckland` | the same defect, in the case where the local date and the UTC date disagree |
| spring-forward gap | `0 30 2 * * *` | `America/New_York`, across 2026-03-08 | a fixed-time job silently skipping the day it lands in the gap. croner's documented rule fires it at the first valid instant after the gap |
| spring-forward wildcard | `0 */15 * * * *` | `America/New_York`, across 2026-03-08 | an interval job that fires the gap's nominal occurrences anyway; the correct sequence skips them and resumes on the new wall clock |
| fall-back single fire | `0 30 1 * * *` | `America/New_York`, across 2026-11-01 | a double fire across the repeated hour |
| never matches | `0 0 30 2 *` | none | mapping every `CronError` to `Err`, which loses the `Ok(None)` this case must produce |
| bad pattern | `not a cron` | none | the reverse: mapping a genuine parse failure into `Ok(None)` |
| bad zone | `0 3 * * *` | `Mars/Olympus` | a `parse` that accepts any string and only fails later at scheduling time |

The DST rows come from croner's documented `JobType` behaviour, and the whole point of pinning them is that this plan is not the authority on them — croner is. **Derive the expected values by running the case, read the result, and then decide whether it matches croner's documented rule before pinning it.** If it does not, that is a finding for the report, not a number to paste in.

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

Changing `InvalidCron(String)` into a struct variant breaks its existing test (`normalize.rs:163-171`) and its `Display` arm (`:112`); update both. The existing test's input `"not a cron"` stays rejected, so the test's *intent* survives — only its pattern-match changes. Its "fails if" comment becomes: fails if the validator accepts any string with five tokens.

Update the `# Errors` doc on `normalize` (`:36-40`), which currently promises "`cron_restart` is not a 5-field pattern".

**Validate the timezone even when `cron_restart` is absent.** A Flockfile with `cron_timezone = "Mars/Olympus"` and no pattern is a typo the user wants to hear about, and spec §5's rule is that typos fail loudly. The cost is one extra branch.

- [ ] **Step 4: Run the `-Z minimal-versions` rehearsal**

```
cargo +nightly generate-lockfile -Z minimal-versions
cargo +stable test --workspace
git checkout Cargo.lock
```

Any floor this exposes goes in `[workspace.dependencies]` with a comment naming the API, matching the block already in the root manifest. Restore `Cargo.lock` afterwards — the rehearsal's lockfile is not the one that gets committed.

- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 6: CHANGELOG** — `shep-core`, under Additions and Changes. The dialect change is user-visible: patterns that were accepted (any five tokens) now fail, and patterns that were rejected (six-field) now pass.
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

/// Growth is expected: a future `https` probe removes one variant's reason for
/// existing and a unix-socket probe would add several (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTargetError { /* the six above, each carrying the offending target */ }
```

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

- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: CHANGELOG** — `shep-core`, Additions.
- [ ] **Step 5: Commit** — `feat(core): parse and validate probe targets at config time`

---

### Task 3: The `Clock` seam and the cron worker

**Files:**
- Create: `crates/shep-daemon/src/cron.rs` — pure tier
- Create: `crates/shep-daemon/src/testing.rs` — pure tier (the inline `mod testing` in `lib.rs`, moved verbatim, plus `TestClock`)
- Modify: `crates/shep-daemon/src/lib.rs` — pure (module declarations; the inline `mod testing { ... }` block becomes `#[cfg(test)] pub(crate) mod testing;`)
- Modify: `crates/shep-daemon/Cargo.toml` — chrono opt-in

**Interfaces:**
- Consumes: `shep_core::config::CronSchedule`, `shep_core::selector::ProcessSelector::Name` (`crates/shep-core/src/selector.rs:15`), `crate::supervisor::SupervisorHandle::restart` (`crates/shep-daemon/src/supervisor.rs:210`)
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

/// Longest a cron worker sleeps before re-deriving its next occurrence.
///
/// A single `sleep_until(next)` is wrong across a laptop suspend, an NTP step
/// or a DST wall-clock shift: the sleep was computed against a wall time that
/// no longer holds, and the job fires late by however far the clock moved.
/// Re-deriving at least this often bounds that error to one minute at the cost
/// of one wakeup per minute per cron-configured sheep.
const MAX_CRON_SLEEP: Duration = Duration::from_secs(60);

/// Runs one sheep-group's cron schedule until the handle is dropped.
///
/// Cancellation: the returned handle aborts the loop on `abort()`; the loop
/// itself holds no state that needs unwinding.
pub fn spawn_cron_worker(
    name: String,
    schedule: CronSchedule,
    clock: Arc<dyn Clock>,
    supervisor: SupervisorHandle,
) -> tokio::task::JoinHandle<()>;
```

**The loop shape, and the two ways to get it wrong:**

```
loop {
    now = clock.now_utc()
    next = schedule.next_after(now)?          // Ok(None) => log and return
    sleep(min(next - now, MAX_CRON_SLEEP))    // saturating: a negative delta is zero
    if clock.now_utc() >= next {
        supervisor.restart(ProcessSelector::Name(name.clone())).await
    }
}
```

1. **The `if` after the sleep is not optional.** Without it, a capped sleep that expires before `next` fires the job early, every minute, forever. With it, a capped sleep that expires early simply loops and re-derives.
2. **Missed occurrences are not replayed.** A daemon that was suspended for six hours with an hourly cron wakes to a `next_after(now)` that is one occurrence in the future, and the sheep restarts at most once. Firing the six missed occurrences would be a restart storm; the loop's structure gives the at-most-one behaviour for free, and the reason belongs in an IR-31 `//` comment so nobody "fixes" it into a catch-up loop.

`next_after` returning `Ok(None)` — a pattern that can never fire again — logs at `info` and ends the task. Returning `Err` logs at `warn` and also ends the task: a schedule that cannot resolve its own next occurrence will not start resolving it later, and a loop that retries would spin.

- [ ] **Step 1: Move `mod testing` into its own file**

`crates/shep-daemon/src/lib.rs` currently declares `#[cfg(test)] pub(crate) mod testing { ... }` inline with the `test_paths` and `harness` helpers and a long comment about `FD_REUSE_LOCK`. Move the whole block, comments included, to `crates/shep-daemon/src/testing.rs` and leave `#[cfg(test)] pub(crate) mod testing;` behind. No item changes name or visibility, so `crate::testing::harness` keeps resolving and no call site moves. Do this as its own commit before anything else in this task, so the diff that adds `TestClock` is readable.

- [ ] **Step 2: Add `TestClock` to `testing.rs`, with the WHY comment IR-33 requires**

```rust
// WHY a clock derived from tokio's Instant: `start_paused = true` freezes
// `tokio::time`, but `chrono::Utc::now()` keeps reading the real system clock.
// A cron test that used the real clock would have to wait real hours. Deriving
// wall time as `epoch + elapsed-since-construction` means `tokio::time::advance`
// moves both clocks by the same amount, and a whole day of schedule fits in a
// test that takes microseconds.
pub(crate) struct TestClock { epoch: DateTime<Utc>, started: tokio::time::Instant }
```

`started` is captured with `tokio::time::Instant::now()` at construction, inside the paused runtime. `now_utc` is `epoch + chrono::Duration::from_std(self.started.elapsed())`. The `from_std` conversion is fallible; a test clock cannot plausibly exceed `chrono::Duration`'s range, so saturate rather than panicking — a panicking fixture is a panicking constructor by another name (IR-21).

- [ ] **Step 3: Write the worker's tests, then the worker**

All under `#[tokio::test(start_paused = true)]`. Required cases, each with its "fails if" comment:

| Case | The broken implementation it catches |
|---|---|
| a `0 * * * *` schedule fires at exactly the top of each of three successive hours, asserted as a pinned array of the restarts observed | a loop that fires on the capped sleep instead of the occurrence |
| advancing the clock by 30 seconds at a time across one hour produces exactly one restart | the same defect, in the shape where the cap is shorter than the interval |
| advancing past six occurrences in one jump produces exactly one restart | a catch-up loop replaying the backlog |
| a pattern with no further occurrence ends the task without restarting | a loop that treats `Ok(None)` as "try again" and spins |
| dropping/aborting the handle stops the worker: no restart after the abort, asserted by advancing a further hour | a worker that outlives its sheep |

Observing "a restart happened" needs no new plumbing: build the worker against a `SupervisorHandle` from `crate::testing::harness(...)` and assert on the flock's `restarts` count via `handle.list().await`, or subscribe to the harness's event channel. **If the test subscribes, every `recv().await` is wrapped in `tokio::time::timeout` with a message naming the event that did not arrive** — the paused clock makes an un-timed `recv` on a bug hang until CI kills the job.

- [ ] **Step 4: Dyn-compatibility smoke test** — `let _: &dyn Clock = &SystemClock;`, one line, in the module's test block. It fails to compile the moment somebody adds a generic method to `Clock`.
- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 6: Commit** — `feat(daemon): cron-scheduled restarts with a wall-clock seam`

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
# Process-table RSS sampling for the polling memory-limit enforcer. Defaults
# bring component/disk/network/user, none of which the daemon reads; the
# `multithread` feature pulls rayon into a daemon whose idle-footprint goal is
# single-digit MB (spec §14.11) and is not enabled.
sysinfo = { version = "0.39.6", default-features = false, features = ["system"] }
```

Run the `-Z minimal-versions` rehearsal (the three-command sequence from Task 1, Step 4).

- [ ] **Step 2: `tree_rss` first, against a table fixture — it is pure and it carries the whole correctness claim**

Cases, each with its "fails if" comment: a lone root (sum is its own RSS); root with two children; a three-deep chain; a sibling subtree that must **not** be counted; a root absent from the table (sum is 0, not a panic); a self-parenting pid and a two-cycle (terminates, and the assertion is on the returned sum, not merely on "did not hang" — a `tokio::time::timeout` cannot save a synchronous infinite loop, so the guard has to be in the algorithm: track visited pids).

The sibling-subtree case is the one that catches the most likely wrong implementation — summing every process whose parent chain is non-empty, or summing the whole table.

- [ ] **Step 3: `ScriptedSampler` in `testing.rs`, and `SysinfoSampler`**

`SysinfoSampler` gets exactly one test, and it is a smoke test with a justifying comment (IR-33): sample the real machine, assert the current process's own pid appears with a non-zero `bytes`. That is the only claim about the real OS that is both true everywhere and worth making — anything stronger is a test of sysinfo, not of us. Use `std::process::id()` to know which pid to look for.

- [ ] **Step 4: Dyn-compatibility smoke test** — `let _: &dyn MemorySampler = &SysinfoSampler::new();`
- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 6: Commit** — `feat(daemon): sample resident memory across a sheep's process tree`

---

### Task 5: The bench crate, and the number behind the poll interval

**Files:**
- Create: `benches/Cargo.toml` — pure
- Create: `benches/benches/memory_sample.rs` — pure
- Modify: `Cargo.toml` (workspace members, criterion) — pure
- Modify: `.github/workflows/test.yml` — pure

**Why this is a task and not a paragraph inside Task 4:** IR-26 requires every tuning threshold to be a named const with a *benchmark-backed* comment, and this workspace has no benchmark harness at all — no `benches/` directory, no `criterion` dependency, no CI job. Building the first one is new infrastructure with its own manifest shape (IR-5: separate unpublished crate, its own `[workspace]` table, `harness = false`, deterministic fixtures, `black_box` on both ends, and CI compiles *and runs* it so it never rots). Folding that into the sampler task would put a workflow change inside a diff about `/proc` walking.

**Interfaces:** none exported. The deliverable is a committed measurement.

- [ ] **Step 1: Create the bench crate**

```toml
[package]
name = "shep-benches"
publish = false
edition.workspace = true
rust-version.workspace = true

# Own workspace table: this crate is excluded from the root workspace's
# dependency unification so a bench-only dependency can never end up in a
# shipped binary's tree (IR-5).
[workspace]

[dependencies]
shep-daemon = { path = "../crates/shep-daemon" }
criterion = { version = "0.5", default-features = false, features = ["cargo_bench_support"] }

[[bench]]
name = "memory_sample"
harness = false
```

Add `benches` to the root `[workspace] members` list, or leave it excluded and out of the workspace entirely — the two are mutually exclusive and IR-5's "own `[workspace]`" phrasing means the second. Pick the second: a crate with its own `[workspace]` table is not a member of the outer one, and listing it in `members` is an error. Note in the report which way it went and why, because IR-5's wording admits both readings.

Verify criterion's current version and its feature names against docs.rs before writing this manifest rather than taking `0.5` from this plan — it is the one dependency here whose exact version has not been checked, deliberately, so that the implementer performs the check rule 8 exists for.

- [ ] **Step 2: Benchmark two things, not one**

`tree_rss` over a synthetic table of 500 processes with a realistic tree shape (deterministic fixture, built in the bench, `black_box` in and out) — this is the part that scales with flock size.

`SysinfoSampler::sample()` against the real machine — this is the part that scales with the *host's* process count and is the number `MEMORY_POLL_INTERVAL` is really about. It is not deterministic, and that is fine for a bench whose output is a comment rather than an assertion; say so in the bench's own comment.

- [ ] **Step 3: Add the CI job**

A `bench` job in `.github/workflows/test.yml` running `cargo bench --manifest-path benches/Cargo.toml -- --test`. Criterion's `--test` mode runs each benchmark once and asserts nothing, which is exactly what "CI compiles and runs them so they never rot" needs — a real timing run on a shared runner would be noise. Match the existing jobs' shape: `permissions: contents: read`, an explicit `rustup toolchain install`, and no `fail-fast` change (the matrix jobs already set it).

- [ ] **Step 4: Record the measurement**

Run the bench locally and write the two numbers, with the machine they came from and the date, into the bench file's own header comment. Task 6's `MEMORY_POLL_INTERVAL` comment cites them. **Numbers do not go into this plan** — a measured value belongs next to the thing it justifies, and a plan file is not where anyone will look for it in a year.

- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code.** The workspace gates do not cover the bench crate (it is outside the workspace); run `cargo check --manifest-path benches/Cargo.toml` and `cargo bench --manifest-path benches/Cargo.toml -- --test` as two more, each from its own exit code.
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
    /// What the tree was measured at.
    pub observed: MemSize,
    /// The limit it exceeded.
    pub limit: MemSize,
}

/// Watches armed sheep for memory-limit breaches.
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
    pub fn start(sampler: Arc<dyn MemorySampler>, breaches: mpsc::Sender<LimitBreach>) -> Self;
}
```

**One sample pass serves every armed sheep.** The task samples once per tick and then sums a tree per armed sheep out of that single table. A per-sheep refresh would multiply the syscall walk by the flock size for no new information, and the whole reason `MemorySampler::sample` returns the *whole* table rather than one pid's reading is to make the wrong shape hard to write.

**A breach disarms the id it reports.** Without that, the next tick — 15 seconds later, while the restart is still in flight — sees the same over-limit tree and reports again, and the sheep gets restarted twice. The re-arm happens when the sheep comes back online, which Task 12 owns. State this in the trait's `arm` doc as part of the contract, not as an implementation detail of `PollingEnforcer`, because the cgroup implementation has to honour it too.

**Who acts on a breach:** Task 12 wires `breaches` to a task that calls `SupervisorHandle::restart(ProcessSelector::Id(id))`. That path already resets the restart budget (`supervisor.rs:210` doc: "resetting its restart budget"), which is the behaviour spec §4 wants — only exits within `min_uptime` count as unstable, and a memory-limit restart is not an exit within `min_uptime`. Do not add a second code path to make that true.

- [ ] **Step 1: Write the tests, then the enforcer**

All under `#[tokio::test(start_paused = true)]`, driven by `ScriptedSampler`. Required cases with their "fails if" comments:

| Case | The broken implementation it catches |
|---|---|
| a tree under the limit for three ticks produces no breach, and `sampler.calls()` is exactly 3 | a loop that polls on the wrong cadence, or one that reports on equality |
| a tree that crosses the limit on the third reading breaches on exactly the third tick, asserted as the `Instant` at which the breach arrives | a first-tick-immediate loop, or one that reports a tick late |
| the breach's `observed` is the **tree** sum, not the root pid's own bytes | an enforcer that skipped `tree_rss` |
| after a breach, the next two ticks produce no second breach for that id | the missing self-disarm above |
| two sheep armed at different limits: only the one over its own limit breaches | an enforcer that compares every tree against every limit, or against the first limit armed |
| `disarm` before the next tick produces no breach | an enforcer that leaks armed entries |
| a sheep whose root pid is absent from the table produces no breach | a `tree_rss` of 0 compared with `>=` against a limit of 0, or a panic on the missing pid |

Every `breaches.recv().await` is wrapped in `tokio::time::timeout` naming the breach that did not arrive. The negative cases — "no breach" — are asserted with `try_recv()` returning `Err(Empty)` after advancing the clock, **not** with a timeout, because a timeout that is *expected* to expire is a test that takes real time to pass.

The comparison is `observed > limit`, strictly. A tree exactly at `max_memory` has not exceeded it, and the boundary sweep asserts that a reading of exactly the limit does not breach while `limit + 1` does.

- [ ] **Step 2: The compile-only external-impl proof (IR-38)**

`crates/shep-daemon/tests/external_impls.rs` is this crate's one `tests/` file and proves an outside crate can implement all three new traits. Bodies are `todo!()`; the file is never run for behaviour. It carries no `#![cfg(unix)]` — every trait it names is pure tier, and gating it would remove the proof from the Windows leg, which is the leg most likely to break it.

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

/// Runs a sheep's liveness probe until the returned handle is aborted.
///
/// Reports through `failures` once `failure_threshold` consecutive probes have
/// failed, then ends: a sheep that has been declared unhealthy is about to be
/// restarted, and the loop for its replacement is a new one.
pub fn spawn_liveness_task(
    id: u32,
    config: ProbeConfig,
    target: ProbeTarget,
    prober: Arc<dyn Prober>,
    failures: mpsc::Sender<u32>,
) -> tokio::task::JoinHandle<()>;
```

**Interval is measured from probe completion, not from probe start.** `sleep(interval)` after the probe resolves, rather than a `tokio::time::interval` ticking independently, means a probe whose `timeout` exceeds its `interval` cannot overlap itself. `ProbeConfig` allows exactly that combination — `timeout` defaults to 5s and `interval` to 10s (`crates/shep-core/src/config/app.rs:35-38`), but nothing stops a user inverting them — and overlapping probes against a struggling app is the shape that turns a slow service into a dead one.

**The counter resets on any pass.** `failure_threshold` is consecutive failures (`app.rs:41` doc: "Consecutive failures before the probe reports unhealthy"), so a pass-fail-fail-pass-fail sequence never trips a threshold of 3.

- [ ] **Step 1: `ScriptedProber` and `probe_config` in `testing.rs`, with WHY comments**

`ScriptedProber` replays a `Vec<Result<(), ProbeFailure>>` and repeats the final entry once exhausted — see the fixture roster for why. Its `probe` implementation ignores both arguments; that is the point of a scripted fake, and the argument-ignoring is what makes it usable for HTTP, TCP and exec cases alike.

- [ ] **Step 2: Write the liveness tests, then the loop**

All `#[tokio::test(start_paused = true)]`. Required cases with their "fails if" comments:

| Case | The broken implementation it catches |
|---|---|
| threshold 3, script `[Fail, Fail, Fail]`: the failure arrives after exactly three probes and at exactly `3 × interval` from arming, asserted as an `Instant` | an off-by-one threshold, and a loop that probes on a `tokio::time::interval` (which would fire the first probe at t=0 and the third at 2×interval) |
| threshold 3, script `[Fail, Fail, Pass, Fail, Fail]`: nothing is reported | a counter that accumulates non-consecutive failures |
| threshold 3, script `[Fail, Fail, Pass, Fail, Fail, Fail]`: reported after six probes | a counter that resets but then double-counts, or one that never re-arms |
| threshold 1, script `[Fail]`: reported after one probe | a `>` where `>=` belongs |
| after reporting, `prober.calls()` does not grow over a further three intervals | a loop that keeps probing a sheep it already declared dead |
| `timeout` longer than `interval`, script of slow passes: probes never overlap, asserted by `calls()` after a fixed span | a `tokio::time::interval` loop |
| aborting the handle stops the probing | a task that outlives its sheep |

The `failures.recv().await` in the positive cases is wrapped in `tokio::time::timeout` naming the id that did not arrive. The negative cases assert with `try_recv()` after advancing, as in Task 6.

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
    #[must_use]
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

**Readiness gates the start path only when the app configures it — this is a departure from the research note and it is deliberate.** `docs/research/phase4-lifecycle.md:161-166` has one `await_ready` serving both normal start and reload's AwaitReady, `Heuristic` included. Applied to the start path, `Heuristic` means every app that configures no readiness at all waits `listen_timeout` — 3000ms by default (`crates/shep-core/src/config/app.rs:107`) — before reaching `online`. That is a three-second regression on every `shep start` in the default configuration, and nothing in the spec asks for it: §7 says readiness "gates reload", and §4 puts the heuristic inside reload's `AwaitReady` state. **The spec wins** (`docs/specs/shep-v1.md:8-9`). So:

- `wait_ready = true` or `readiness_probe` set → the sheep enters `starting` and reaches `online` on the signal, or on the deadline.
- Neither → `online` on spawn success, exactly as the engine behaves today. No new latency, and no existing supervisor test changes its expectations.
- `Heuristic` still exists, is still tested, and is what reload will use when reload is built. It is simply not reachable from the start path.

**On deadline elapse the sheep goes online anyway, with a `tracing::warn!`.** The alternative — treating a readiness timeout as a spawn failure — turns a slow-starting app into a restart loop, which is the failure mode `max_restarts` exists to contain and which users hit constantly in pm2. This is a judgement call; it is in Open Questions.

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
4. `ProcessEventKind::Online` is emitted at the transition to `Online`, not at spawn, for gated apps. `ProcessEventKind::Start` still fires at spawn. This is a behaviour change visible on the bus and it is the right one — a subscriber watching for `Online` wants to know when the app is serving.

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

`crates/shep-daemon/src/supervisor.rs` has 22 paused-clock tests. **The default path is unchanged, so none of them should need editing.** If any does, that is evidence the gate leaked into the ungated path — fix the implementation, not the test, and say so in the report.

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
/// its guard.
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
```

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
# Coalesces the burst a single editor save produces into one batch, and tracks
# renames through its file-id cache.
notify-debouncer-full = { version = "0.7.0", default-features = false }
```

notify 8.2.0 has two default features, `macos_fsevent` and `fsevent-sys`; the second is the implicit optional-dependency feature the first enables, so naming `macos_fsevent` is sufficient. The research note says `macos_fsevent` is notify's "only default" — it is one of two, and the distinction does not change the line but does change what a reviewer should expect `cargo tree` to show.

- [ ] **Step 1: Add both crates and run the `-Z minimal-versions` rehearsal** (the three-command sequence from Task 1, Step 4)
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
- Consumes: `globset::{Glob, GlobSet, GlobSetBuilder}` (already a `shep-daemon` dependency, `crates/shep-daemon/Cargo.toml:29`), `crate::watch::source::watch_tree`, `SupervisorHandle::restart`
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

/// Runs one name-group's watch until the returned handle is aborted.
pub fn spawn_watch_group(
    name: String,
    root: PathBuf,
    filter: WatchFilter,
    delay: Duration,
    supervisor: SupervisorHandle,
) -> Result<tokio::task::JoinHandle<()>, WatchError>;
```

**One watch per name-group, not per instance.** Spec §4 says so, and the reason is that N instances of one app share one source tree: N debouncers over the same tree means N inotify watch sets, N copies of every event, and N restarts racing each other for one file save. The group's restart is `SupervisorHandle::restart(ProcessSelector::Name(name))`, which restarts every instance. This is what makes watch state live in a `HashMap<String, _>` keyed by name rather than in `SheepSlot` — Task 12 owns that map.

**The re-check needs no dirty flag, because the channel is the mechanism.** The loop is:

```
loop {
    batch = rx.recv().await                        // None => the source is gone, return
    if !batch.iter().any(|p| filter.triggers(p)) { continue }
    supervisor.restart(Name(name)).await
    // events that arrived during the restart are still queued in rx;
    // the next iteration drains and re-filters them
}
```

Spec §4's "Events during an in-flight restart are re-checked after it completes" falls out of the ordering: `restart` is awaited, the receiver keeps buffering, and the next `recv` returns whatever accumulated. Nothing needs to remember that a restart happened. **Do not add a dirty flag or a state machine here** — the reason it is unnecessary is worth an IR-31 `//` comment, because it looks like a missing feature until someone traces the buffering.

**The consequence is a single-flight guarantee**, and it is the invariant the proptest in Task 13 checks: because the loop awaits its restart before reading the next batch, a WatchGroup can never have two restarts in flight.

**Filtering is against the path relative to the watch root**, so a user's `watch_options = ["src/**/*.rs"]` means what they think it means. notify delivers absolute paths; strip the root before matching, and when the strip fails (a path outside the root, which the OS should not deliver but a symlinked tree can produce) treat the path as non-triggering rather than matching it against the absolute form.

**Ignore wins over include.** A path matched by both `watch_options` and `ignore_watch` does not trigger. That is the only ordering that makes `ignore_watch` mean anything.

- [ ] **Step 1: `WatchFilter` and its boundary sweep — pure, no tokio, no filesystem (IR-40)**

Cases with their "fails if" comments: empty `watch_options` matches everything; `["src/**/*.rs"]` matches `src/a/b.rs` and not `src/a/b.txt` and not `other/a.rs`; a default ignore beats an explicit include (`watch_options = ["**"]` still does not trigger on `.git/index` or `node_modules/x/y.js`); an `ignore_watch` entry beats an include; a glob that matches nothing simply never triggers rather than erroring; an invalid glob (`[`) is a `WatchFilterError::Glob` carrying the pattern; a path that does not start with the root does not trigger.

The "default ignore beats explicit include" case is the one that catches the most damaging wrong implementation: an ignore set consulted only when `ignore_watch` is non-empty, which makes every app with custom `watch_options` restart on its own log writes.

- [ ] **Step 2: The group loop, paused-clock, driven by a hand-fed channel**

The loop's logic is testable without a filesystem by constructing the group around an `mpsc::UnboundedReceiver` the test owns the sender for. Structure `spawn_watch_group` so that the loop body is a separate `async fn run_group(name, filter, rx, supervisor)` and `spawn_watch_group` is `watch_tree(...)` plus `tokio::spawn(run_group(...))` — that split is what makes both halves testable, and it is the same seam shape as the rest of the phase.

Cases with their "fails if" comments: a batch of only-ignored paths produces no restart; a batch with one triggering path produces exactly one restart; two batches sent before the first restart completes produce exactly two restarts, not three (the second batch is drained as one); a batch arriving *during* a restart is re-checked after it (the spec §4 requirement, asserted by observing the second restart); dropping the sender ends the task.

Every `recv().await` in these tests — on the event channel used to observe restarts — is inside a `tokio::time::timeout` naming the restart that did not arrive.

- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: Commit** — `feat(daemon): watch-triggered restarts with glob filtering and re-check`

---

### Task 12: `ExtrasRegistry` — arming and disarming on lifecycle transitions

**Files:**
- Create: `crates/shep-daemon/src/extras.rs` — pure tier
- Modify: `crates/shep-daemon/src/supervisor.rs` — pure
- Modify: `crates/shep-daemon/src/boot.rs` — **unix tier**
- Modify: `crates/shep-daemon/src/testing.rs`, `crates/shep-daemon/src/lib.rs` — pure

**Interfaces:**
- Produces:

```rust
/// The four lifecycle extras, and the seams they run on.
///
/// Constructed once at boot and handed to the supervisor. Every field is a
/// trait object so the engine's type does not grow a parameter per subsystem.
pub struct Extras {
    /// Wall clock the cron workers read.
    pub clock: Arc<dyn Clock>,
    /// Memory-limit mechanism.
    pub enforcer: Box<dyn LimitEnforcer>,
    /// Probe transport for readiness and liveness.
    pub prober: Arc<dyn Prober>,
}

impl Extras {
    /// The production wiring: system clock, polling enforcer over sysinfo, and
    /// probes over real sockets.
    #[must_use]
    pub fn real(breaches: mpsc::Sender<LimitBreach>) -> Self;
}

/// Per-sheep and per-group task handles, armed on `online` and aborted on the
/// way out.
#[derive(Debug, Default)]
pub struct ExtrasRegistry { /* private */ }

impl ExtrasRegistry {
    /// Arms everything an entry's configuration asks for.
    ///
    /// Idempotent per id: arming an already-armed id disarms it first, which
    /// is what a respawn needs — the new process has a new pid.
    pub fn arm(&mut self, entry: &ProcessEntry, extras: &Extras, supervisor: &SupervisorHandle);

    /// Aborts everything armed for `id`, and the name-group's watch when this
    /// was its last armed instance.
    pub fn disarm(&mut self, id: u32, name: &str);
}
```

**What is armed where, and why the shapes differ:**

| Extra | Keyed by | Armed when | Disarmed when |
|---|---|---|---|
| cron worker | sheep name | the first instance of a name goes online | the last instance of that name leaves |
| watch group | sheep name | ditto | ditto |
| memory limit | sheep id | every time an instance goes online (new pid) | that instance leaves |
| liveness probe | sheep id | ditto | ditto |

Cron and watch are per-name because a cron restart and a watch restart both target `ProcessSelector::Name` and would otherwise fire N times for N instances. Memory and liveness are per-instance because each has its own pid and its own health.

**`SupervisorBuilder`, and why `spawn_supervisor` grows a builder rather than a parameter.** `spawn_supervisor(runner, paths, events)` (`crates/shep-daemon/src/supervisor.rs:287`) is called from `boot.rs:521`, from `crate::testing::harness`, and from this crate's own tests. Adding `extras` makes four positional parameters with two more subsystems visible on the roadmap (dogs, metrics), which is precisely the "many optional fields, call-site readability" case Rin's design rules name the builder pattern for. Add:

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

**The four supervisor call sites, and no more:**
1. the transition to `Online` in `spawn_fresh` (`:523`) — `arm`
2. the transition to `Online` in `respawn` (`:628`) — `arm`
3. `handle_exited`'s terminal branches (`:975` errored, `:987` clean stop) — `disarm`
4. `set_status` to `Stopping` (`:1068` region) — `disarm`, so a probe does not fire against a sheep in the middle of its kill ladder

Note the interaction with Task 9: for a gated app the transition to `Online` happens in the `ReadyResult` handler, not in `spawn_fresh`. **Arming happens at the transition, wherever it is** — a liveness probe armed at spawn against an app that has not finished starting will fail its threshold and restart the app before it ever comes up. Put the `arm` call in one place that both paths reach, not two.

**`ProcessEventKind` gives no compile-time gate here, and the plan says so rather than letting the implementer assume one.** It is `#[non_exhaustive]` (`crates/shep-core/src/protocol/events.rs:11`), so any match on it from `shep-daemon` must carry a `_` arm and E0004 cannot fire. If this task maps lifecycle events to arm/disarm at all, the `_` arm's behaviour is stated in a comment — arm nothing, disarm nothing, log at `debug` — and never a `todo!()`. Prefer not to match it: arming from the *transition sites* listed above is direct, drop-free and needs no event at all, whereas driving arm/disarm off the bus would also have to survive the bus's bounded drop-oldest queue (`shep-core/src/protocol/events.rs`, the `Dropped` variant), and a dropped `Stop` would leave a probe firing at a process that no longer exists.

**Breach and liveness reporting are consumed by a single task**, spawned alongside the supervisor: it reads the `LimitBreach` receiver and the liveness-failure receiver and calls `SupervisorHandle::restart(ProcessSelector::Id(id))` for each, logging at `warn` with the observed figure. That task, not the actor, owns those receivers — the actor must never block on anything a subsystem controls.

- [ ] **Step 1: `app_with` in `testing.rs`, and the `Harness` change**

`Harness` (`crates/shep-daemon/src/testing.rs`) gains one field so tests can drive the extras:

```rust
pub(crate) struct Harness {
    pub(crate) ctx: RpcContext,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    /// Breach reports the supervisor's extras produce, for tests that assert
    /// a memory limit fired.
    pub(crate) breaches: mpsc::Receiver<LimitBreach>,
    _dir: tempfile::TempDir,
    _events_rx: broadcast::Receiver<shep_core::protocol::BusEvent>,
}
```

`harness(scripts)` keeps its signature and wires scripted extras; a second constructor `harness_with_extras(scripts: Vec<ProcScript>, extras: Extras) -> Harness` takes a caller-built `Extras`. Both are declared here and nowhere else.

- [ ] **Step 2: `ExtrasRegistry` and its tests**

Cases with their "fails if" comments: an app with no extras configured arms nothing (assert no tasks were spawned, by asserting the registry is empty); an app with `cron_restart` arms one worker for the name and a second instance of the same name arms no second worker; a `max_memory` app arms the enforcer with its **root pid**, and a respawn re-arms with the *new* pid (the assertion is on the pid passed to a scripted enforcer — this is the case that catches a registry that arms once and never updates); `disarm` on the last instance of a name stops the cron worker and the watch group, and `disarm` on a non-last instance does not; disarming an id that was never armed is a no-op rather than a panic.

Asserting "a task was aborted" is done by observing its effect stop, not by inspecting a `JoinHandle` — advance the clock past an occurrence and assert no restart, with the negative asserted by `try_recv` after advancing rather than by a timeout.

- [ ] **Step 3: `boot.rs` — construct `Extras::real` and hand it to the builder**

One change at `crates/shep-daemon/src/boot.rs:521`, from `spawn_supervisor(runner, paths.clone(), events.clone())` to the builder with `.extras(Extras::real(breach_tx))`. `boot.rs` is the unix tier; `Extras::real` is pure, so the Windows leg still compiles and tests everything but this call.

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
| `watch_options` empty; a glob matching nothing; a watch root that does not exist | `watch/mod.rs` |
| `failure_threshold = 1`; `timeout` greater than `interval`; `interval` of zero | `probes/mod.rs` |
| `max_memory` smaller than any plausible reading (immediate breach on the first tick); `max_memory` exactly equal to the observed tree (no breach) | `limits/mod.rs` |
| a cron pattern that never fires again; a pattern firing every second against a `MAX_CRON_SLEEP` of 60s | `cron.rs` |
| a name-group with zero online instances (arm and immediately disarm) | `extras.rs` |

An `interval` of zero deserves a decision rather than a sweep result: a zero-interval probe is a spin loop. Either `normalize` rejects it (Task 2's territory, and the cheaper fix) or the loop floors it at a named minimum. Whichever the implementer picks, the sweep asserts the chosen behaviour and the choice goes in the report.

- [ ] **Step 1: Extend the supervisor proptest with the three new event kinds**
- [ ] **Step 2: The `WatchGroup` single-flight property**
- [ ] **Step 3: The five boundary sweeps**
- [ ] **Step 4: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 5: Commit** — `test(daemon): property and boundary coverage for the lifecycle extras`

---

### Task 14: End-to-end tier, map.md sync, module docs, changelogs

**Files:**
- Modify: `crates/shep-cli/tests/cli_e2e.rs` — **unix tier** (the file opens `#![cfg(unix)]`)
- Modify: `docs/systematic-refactor/refactor-workspace/map.md`
- Modify: `crates/shep-core/CHANGELOG.md`, `crates/shep-daemon/CHANGELOG.md`
- Modify: the module docs of every file this phase created

**E2E cases** — `assert_cmd`, a fresh `$SHEP_HOME` per test, no sleeps, and **every command chain carrying `.timeout(Duration::from_secs(30))` before `.output()`**, which is Phase 3's established template (`crates/shep-cli/tests/cli_e2e.rs`) and is what turns a regression into a failed assertion instead of a killed job:

1. **Watch restart.** Start a sheep with `watch = true` under a tempdir, write to a file in the tree, and observe the restart through `shep flock --format json`'s `restarts` count going from 0 to 1. Poll the command until it changes or a named deadline expires, exactly as `bleats_no_follow_until_written` does — do not sleep and assert once.
2. **Watch ignores what it should.** The same sheep, a write to a dot-file, and the restart count still 0 after the same deadline. This is the case that fails if the default ignores are dropped, and it is the reason case 1 alone is not enough.
3. **Readiness gates online.** A sheep with `wait_ready = true` whose script waits before writing `{"kind":"ready"}` to the shepherd channel: `shep flock` shows `starting`, then `online`. This is the only tier that exercises the real fd-3 channel end to end.
4. **A bad cron pattern is a config error with the right exit code.** `shep start` against a Flockfile with `cron_restart = "not a cron"` exits `4` (invalid config, spec §9) and its stderr JSON carries the pattern. Assert the exit code and the presence of the pattern in the message — the exact wording is croner's and is not ours to pin.
5. **An `https://` probe target is a config error.** Same shape, exit `4`, message names the target.

Cases 4 and 5 are the cheapest and the most valuable: they are the proof that spec §5's "typos fail loudly at parse time" survived the trip from `normalize` through the RPC boundary to an exit code.

**map.md sync — three drifts, recorded not silently fixed:**
- `probes/` is a module map.md never named; spec §7 requires it (`docs/specs/shep-v1.md:8-9` — where the two disagree the spec wins and map.md gets fixed).
- `watcher.rs` became `watch/` (two files: the OS seam and the filtering logic), under Rin's 500-line split rule and because the two halves have different test tiers.
- `worker.rs` was not built; its interval loops live in `cron.rs` and `limits/mod.rs` with the subsystems they serve.

Also record `limits/` and `probes/` being directories rather than files, and the three new seam traits, so map.md remains a module-level design a reader can navigate from.

**Module docs (IR-27) are decision guides, not API dumps.** Each new module's `//!` header answers the question a user of that module actually has:
- `cron.rs` — when a cron restart fires relative to wall-clock changes, and why a missed occurrence is not replayed.
- `limits/` — what "the process tree" means and why it is not the root pid, with the pm2 deviation callout in spec §4's voice.
- `probes/` — which readiness source wins when more than one is configured, and what a readiness timeout does versus a liveness failure.
- `watch/` — what is ignored by default and how `watch_options` and `ignore_watch` compose.

Each gets its links in a bottom reference block, and each names its honest caveats inline: no TLS, no redirects, polling granularity, real-time debounce.

**Changelogs (IR-45).** `shep-core` gets the cron dialect change under both Additions and Changes (patterns that used to pass now fail and vice versa — that is the entry a user needs). `shep-daemon` gets the four subsystems and the `Online` timing change for readiness-gated apps, which is a bus-visible behaviour change even though no wire type changed.

- [ ] **Step 1: Write the five e2e cases, run, confirm each fails against a stub before it passes**
- [ ] **Step 2: map.md sync**
- [ ] **Step 3: Module docs pass across every new file**
- [ ] **Step 4: Both changelogs**
- [ ] **Step 5: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 6: Commit** — `test(cli): end-to-end coverage for watch, readiness and config rejection`

---

## Exit criteria

1. All fourteen tasks complete and individually reviewed.
2. Every gate in Global Constraints green from its own exit code — including `cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu` and the two bench-crate gates.
3. `grep -rn "cfg(unix)\|cfg(windows)" crates/shep-daemon/src/{cron.rs,limits,probes,watch,extras.rs}` returns **only** the two shell-selection arms in `probes/os.rs` and the `#[cfg(unix)]` exec *tests*. Any other hit is a module that failed the pure-tier claim.
4. `grep -rn "ponytail" crates/shep-core/src crates/shep-daemon/src` returns nothing for cron — the stopgap note at `normalize.rs:55-56` is gone along with the stopgap.
5. `grep -rn "Phase 4\|Task [0-9]" crates/ Cargo.toml benches/` returns nothing. Rule 10, checked.
6. `grep -rn "assert_ne!" crates/shep-daemon/src crates/shep-core/src` returns nothing new.
7. A report to Rin listing: every third-party API where the real documentation differed from `docs/research/phase4-lifecycle.md`; every judgement call made on her behalf; the bench numbers and the machine they came from; and the `interval = 0` decision from Task 13.
8. Every test added by this phase has its "fails if" comment. A reviewer spot-checking three of them at random should be able to break the implementation in the named way and watch the named test go red.

## Open questions for Rin

**Four are settled (Rin, 2026-08-08); four remain below.**

- **Cron dialect: five-field standard cron only.** Widening a grammar later is backwards-compatible, narrowing one is not, so the wide default was the expensive direction to guess. See Task 1 for the `Seconds::Disallowed` setting, the extension-character rejection, and the day/month-name trap it has to avoid.
- **`https://` probe targets: rejected at config time**, with a typed error. A probe that silently fails every poll is indistinguishable from an app that is down, so failing loudly at config time is the honest option. A user with an HTTPS health endpoint cannot use a readiness probe in v1; that is the accepted cost of keeping a TLS stack out of the daemon (decision D1).
- **Memory-limit scope: the process tree**, sheep plus lambs. The tree is what gets killed, so the tree is what gets measured, and a root-pid limit is trivially dodged by any app that forks workers. This deviates from pm2 and wants a line in `docs/migration.md`: someone migrating with `max_memory` on a forking app will see restarts pm2 never gave them.
- **Breach and liveness restarts do not count against `max_restarts`.** Both keep routing through the path that resets the budget, matching spec §4's wording that only exits inside `min_uptime` count as unstable. A memory-leak loop therefore restarts indefinitely rather than reaching `errored` — accepted, on the grounds that a supervisor's job is keeping things up. Revisit if it proves wrong in practice.

### Still open — do not resolve these unilaterally

1. **What happens when readiness times out on start?** This plan goes `online` anyway with a `tracing::warn!`, on the grounds that treating a slow start as a spawn failure produces exactly the restart loop `max_restarts` exists to contain — and that pm2 users expect the lenient behaviour. The alternative is `errored`, which is stricter and arguably more honest: an app that did not signal readiness inside `listen_timeout` is, by its own configuration, not ready. A third option is a new `ProcessEventKind::ReadinessTimeout`, which is a wire-additive change and therefore out of this phase's scope but not out of the question for the next.
2. **Does the bench crate land in this phase?** Task 5 builds it because IR-26 requires a benchmark-backed comment on `MEMORY_POLL_INTERVAL` and IR-5 specifies exactly what a bench crate should look like — but it is the phase's only task that ships no user-visible behaviour, and deferring it means the const's comment cites a one-off measurement instead of a committed harness. If the answer is "defer", say so before Task 4 finishes, because Task 6's const comment is written either way.
3. **`MAX_CRON_SLEEP = 60s`, and at-most-one catch-up.** A cron worker re-derives its next occurrence at least once a minute, so a suspended laptop or an NTP step costs at most a minute of drift; and a daemon that missed six occurrences fires once on wake rather than six times. Both are restart-storm guards and both are guesses at what a user wants. Sixty seconds is one wakeup per minute per cron-configured sheep, which is cheap but not free on a large flock.
4. **The watch group restarts every instance of a name.** Spec §4 says one watcher per name-group, and this plan takes the restart to be group-wide to match. A rolling per-instance restart on file change would keep the app serving through a save, which is arguably what a watch is *for* in development — but it is also reload's job, and reload is not in this phase.
