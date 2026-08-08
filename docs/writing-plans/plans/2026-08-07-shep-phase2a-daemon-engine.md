# shep Phase 2a — Daemon Engine Implementation Plan (rev 3, triple-verified)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build shep-daemon's supervision engine — a `ProcessRunner` abstraction with deterministic fakes, the restart state machine, real tokio spawning with log capture and the shepherd channel, the kill ladder, and the supervisor actor — plus the protocol/CI items parked by Phase 1's final review.

**Architecture:** Engine only (RPC/bus wire plane = Phase 2b). **Ownership model (locked by verification):** each sheep gets its own tokio task that OWNS the `RunningProcess` — it awaits exit, runs the kill ladder, and takes orders over a per-sheep control channel; the supervisor actor owns only entries + control senders and never touches a proc directly. This resolves the `&mut` exclusivity between waiting and killing. Pure decision logic (brain, backoff, assembly) is IO-free for instant deterministic tests.

**Tech Stack:** tokio (rt, process, io-util, time, sync, net, macros; test-util in dev), nix (signal, process), futures, tracing, proptest (dev), insta (dev).

**Phase roadmap context:** Plan 2a of the Phase-2 pair (2b = bus wiring + RPC server + snapshot + boot + e2e). Deferred out of 2a, explicitly: reload execution (Phase 4; `ReloadState` lands as data only), watcher/cron/memory enforcement (Phase 4), Windows Job Objects (typed `StopSignal` keeps the seam), **uid/gid spawn (`user`/`group` fields — Phase 2b, needs privilege-drop design)**, **supervisor-level proptest over command/exit interleavings (IR-37 — Phase 2b, once the event outlet makes invariants observable)**, **the `wait_ready` readiness gate itself (Phase 4 with probes — 2a WIRES the `from_child` channel and drains `ChildMessage::Ready`, emitting `Msg::Ready`, but the actor treats every spawned instance as Online immediately; gating Starting→Online on Ready lands with probe support)**.

## Global Constraints

- **Invoke the `shep-idiomatic-rust` skill before writing any Rust**; cite IR rules. Clean-room: never open `/Users/rin/GitHub/pm2`.
- No panicking constructors outside shep-cli (IR-21); `core::error::Error` never std; per-module error enums, variant docs = precise conditions (IR-18/19); `# Errors` on Result pub fns (IR-28).
- **Unsafe policy:** shep-daemon lib.rs `#![deny(unsafe_code)]`; any unsafe lives ONLY in `sys.rs` behind module-scoped `#[allow(unsafe_code)]` + rationale + per-block `// SAFETY:` (IR-22/23). Target: zero unsafe (command-fds + nix + std cover the needs). — Met for 2a itself (shipped zero unsafe); Phase 2b's Task 7 later needed real fd-adoption unsafe, confined to `sys.rs`'s own definition plus its one call site in `boot.rs` (two syntactic sites, one justified operation — see the 2b plan doc's own correction on this same line).
- Restart semantics byte-exact per spec §4 (defaults: min_uptime 1000ms, max_restarts 16, kill_timeout 1600ms; backoff ×1.5 cap 15000ms; **integer rule pinned in Task 4**).
- **Paused clock is the default test mode** (`#[tokio::test(start_paused = true)]`, requires tokio `test-util` in dev-deps — wired in Task 3). Real time only in `tests/real_runner.rs` with a comment. Hand-rolled fakes, no mock frameworks; unique fixtures per test (IR-33/34).
- Four gates green before every commit: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, `cargo test --workspace`. Focused runs never substitute.
- **Snippet docs/Debug elided for brevity: the workspace denies `missing_docs` AND `missing_debug_implementations` under `-D warnings`.** Every public item transcribed from this plan needs a `///` doc on the item, its fields, and its variants, and every public type needs `#[derive(Debug)]` (or a manual redacting Debug for secret-carrying types per IR-41). Add both while implementing — the plan shows shapes, not the doc/derive boilerplate.
- Terminology per docs/terminology.md; plain-English errors. Conventional commits + footer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- **Snippet elision rule:** this plan's code blocks elide `///` docs and some `#[derive(Debug)]` for brevity — the workspace DENIES `missing_docs` and `missing_debug_implementations`, so implementers add a doc to every public item/field/variant and `Debug` (redacting where secrets — none in this phase) to every public type as they transcribe.

## File Structure (locked)

```
crates/shep-core/src/{selector.rs, protocol/*, config/flockfile.rs}   Task 1 touches
.github/workflows/test.yml                                            Task 2
crates/shep-daemon/src/
  lib.rs        module tree, deny(unsafe_code), crate mini-guide (Task 10)
  runner.rs     ProcessRunner/RunningProcess traits, SpawnSpec, ProcIo, StopSignal, ExitOutcome, RunnerError
  fake.rs       scripted fake (cfg(any(test, feature = "test-fakes")))
  entry.rs      ProcessEntry, RestartBudget, ReloadState
  backoff.rs    restart_delay (pure)
  brain.rs      decide_on_exit (pure)
  assemble.rs   instance slots, env, log paths (pure)
  channel.rs    ChildMessage/ShepherdMessage + newline-JSON codec
  tokio_runner.rs  real runner (socketpair channel, log pumps)
  kill.rs       kill ladder (runs inside the sheep task)
  supervisor.rs actor + SupervisorHandle + sheep tasks
crates/shep-daemon/tests/real_runner.rs   real-time integration tier
```

---

### Task 1: Parked protocol batch (shep-core)

**Files:** Modify `crates/shep-core/src/selector.rs`, `protocol/{mod.rs,request.rs,events.rs,wire.rs}`, `config/flockfile.rs`.

**Interfaces:** `impl TryFrom<SelectorSpec> for ProcessSelector` (`Error = SelectorError`; regex via `RegexBuilder::new(..).size_limit(1 << 20)`) and `impl From<&ProcessSelector> for SelectorSpec`; `encode_frame -> Result<bytes::Bytes, WireError>`; `pub use wire::MAX_FRAME_BYTES;`; evolution-rule doc on `PROTOCOL_VERSION`; byte fixtures for Reply/HelloReply/BusEvent; fwd-compat comment on `RawFlockfile`.

- [ ] **Step 1: Failing tests.** `selector.rs` test mod:

```rust
    #[test]
    fn selector_spec_bridges() {
        use crate::protocol::SelectorSpec;
        let sel: ProcessSelector = SelectorSpec::Regex("^w".to_string()).try_into().unwrap();
        assert!(sel.matches("web", 1, None));
        assert_eq!(SelectorSpec::from(&sel), SelectorSpec::Regex("^w".to_string()));
        for spec in [
            SelectorSpec::All,
            SelectorSpec::Id(3),
            SelectorSpec::Name("web".to_string()),
            SelectorSpec::Fold("backend".to_string()),
        ] {
            let sel: ProcessSelector = spec.clone().try_into().unwrap();
            assert_eq!(SelectorSpec::from(&sel), spec);
        }
    }

    #[test]
    fn selector_spec_bad_regex_is_typed_error() {
        use crate::protocol::SelectorSpec;
        assert!(matches!(
            ProcessSelector::try_from(SelectorSpec::Regex("((".to_string())).unwrap_err(),
            SelectorError::BadRegex(_)
        ));
    }

    #[test]
    fn selector_spec_oversized_regex_is_rejected() {
        // Peer-supplied pattern: size_limit bounds compiled-program memory.
        use crate::protocol::SelectorSpec;
        let huge = format!("(a{}){{1000}}", "|b".repeat(20_000));
        assert!(ProcessSelector::try_from(SelectorSpec::Regex(huge)).is_err());
    }
```

`protocol/request.rs` test mod:

```rust
    #[test]
    fn v1_reply_fixture_still_deserializes() {
        // Committed byte fixture, protocol v1 (IR-35).
        let ok = r#"{"id":1,"result":{"Ok":{"kind":"pong"}}}"#;
        let reply: Reply = serde_json::from_str(ok).unwrap();
        assert!(matches!(reply.result, Ok(Response::Pong)));
        let err = r#"{"id":2,"result":{"Err":{"code":"not_found","message":"no sheep"}}}"#;
        let reply: Reply = serde_json::from_str(err).unwrap();
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::NotFound);
    }

    #[test]
    fn v1_hello_ack_fixture_still_deserializes() {
        let fixture = r#"{"Ok":{"daemon_version":"0.1.0","protocol":1,"pid":4242}}"#;
        let ack: HelloReply = serde_json::from_str(fixture).unwrap();
        assert_eq!(ack.unwrap().pid, 4242);
    }
```

`protocol/events.rs` test mod:

```rust
    #[test]
    fn v1_bus_event_fixture_still_deserializes() {
        // Adjacent-tagged shape pinned as a byte fixture (IR-35).
        let fixture = r#"{"event":"log_out","data":{"id":3,"line":"ready"}}"#;
        let ev: BusEvent = serde_json::from_str(fixture).unwrap();
        assert!(matches!(ev, BusEvent::LogOut { id: 3, .. }));
    }
```

`wire.rs`: update round-trip tests — `encode_frame` now yields `Bytes` directly; delete `.freeze()` at the framed-stream send.

- [ ] **Step 2:** `cargo test -p shep-core` → compile failures (TryFrom missing, Bytes mismatch).
- [ ] **Step 3: Implement.** selector bridges:

```rust
impl TryFrom<crate::protocol::SelectorSpec> for ProcessSelector {
    type Error = SelectorError;

    /// Compiles a wire selector into a matchable one
    ///
    /// # Errors
    ///
    /// - [`SelectorError::BadRegex`] — the peer-supplied pattern fails to
    ///   compile or exceeds the 1 MiB compiled-size bound.
    fn try_from(spec: crate::protocol::SelectorSpec) -> Result<Self, Self::Error> {
        use crate::protocol::SelectorSpec;
        Ok(match spec {
            SelectorSpec::All => Self::All,
            SelectorSpec::Id(id) => Self::Id(id),
            SelectorSpec::Name(name) => Self::Name(name),
            SelectorSpec::Fold(fold) => Self::Fold(fold),
            SelectorSpec::Regex(src) => Self::Regex(
                // Peer-supplied pattern: bound compiled-program memory.
                regex::RegexBuilder::new(&src)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| SelectorError::BadRegex(e.to_string()))?,
            ),
        })
    }
}

impl From<&ProcessSelector> for crate::protocol::SelectorSpec {
    fn from(sel: &ProcessSelector) -> Self {
        use crate::protocol::SelectorSpec;
        match sel {
            ProcessSelector::All => SelectorSpec::All,
            ProcessSelector::Id(id) => SelectorSpec::Id(*id),
            ProcessSelector::Name(name) => SelectorSpec::Name(name.clone()),
            ProcessSelector::Regex(re) => SelectorSpec::Regex(re.as_str().to_string()),
            ProcessSelector::Fold(fold) => SelectorSpec::Fold(fold.clone()),
        }
    }
}
```

`protocol/mod.rs` — version doc + re-export:

```rust
/// Wire protocol version.
///
/// Evolution rule: ADDITIVE optional fields (new serde-defaulted `Option<T>`
/// fields, new variants behind `#[non_exhaustive]`) keep the version.
/// Removing, renaming, or retyping anything serialized bumps it, recorded in
/// the CHANGELOG. Byte fixtures in each protocol module pin the deserialize
/// direction.
pub const PROTOCOL_VERSION: u32 = 1;
```

plus `MAX_FRAME_BYTES` added to the wire re-export list. `wire.rs`:

```rust
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Bytes, WireError> {
    let vec = serde_json::to_vec(value).map_err(|e| WireError::Json(e.to_string()))?;
    if vec.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge(vec.len()));
    }
    Ok(Bytes::from(vec)) // zero-copy: Bytes takes the Vec's buffer
}
```

(import `bytes::Bytes`, docs updated). `flockfile.rs` — above `RawFlockfile`:

```rust
// Forward-compat decision (Phase 1 final review): the top level is locked to
// the `app` key on purpose — a typo'd key must fail loudly. A future schema
// key (e.g. `version`) gets added HERE explicitly; older binaries then
// reject newer Flockfiles by design instead of silently ignoring config.
```

- [ ] **Step 4:** focused + full four-gate chain.
- [ ] **Step 5:** commit `feat(core): selector wire bridges, protocol evolution rule, byte fixtures, zero-copy encode` + footer.

---

### Task 2: CI hardening

**Files:** Modify `.github/workflows/test.yml`; possibly raise workspace dep version floors in `Cargo.toml`.

- [ ] **Step 1: Local minimal-versions rehearsal FIRST** (this job is red on bare floors like `serde = "1"` otherwise):

```bash
cargo +nightly generate-lockfile -Z minimal-versions && cargo +stable check --workspace; git checkout Cargo.lock
```

Raise any floor that fails (e.g. `serde = "1.0.190"`, `tokio = "1.36"`, `bytes = "1.5"` — whatever the rehearsal demands, minimal true versions per IR) and re-rehearse until green.

- [ ] **Step 2: Append jobs** to `test.yml`:

```yaml
  minimal-versions:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install nightly stable --profile minimal
      - run: cargo +nightly generate-lockfile -Z minimal-versions
      - run: cargo +stable test --workspace
  musl:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install stable --profile minimal --target x86_64-unknown-linux-musl
      - run: sudo apt-get update && sudo apt-get install -y musl-tools
      - run: cargo test --workspace --locked --target x86_64-unknown-linux-musl
  features:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install stable --profile minimal
      - run: cargo test -p shep-daemon --locked --no-default-features
      - run: cargo test --workspace --locked --all-features
  coverage:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install stable --profile minimal --component llvm-tools
      - uses: taiki-e/install-action@cargo-llvm-cov
      - run: cargo llvm-cov --workspace --locked --lcov --output-path lcov.info
      - uses: actions/upload-artifact@v4
        with: { name: coverage, path: lcov.info }
```

(The features job becomes meaningful when Task 3 adds shep-daemon's `test-fakes` feature.)

- [ ] **Step 3:** validate YAML (`python3 -c "import yaml; yaml.safe_load(open('.github/workflows/test.yml'))"`), run the musl + features commands locally if toolchains available (else note), four gates.
- [ ] **Step 4:** commit `ci: minimal-versions, musl, feature ladder, llvm-cov jobs (rehearsed locally)` + footer.

---

### Task 3: Runner traits + IO bundle + scripted fake

**Files:** Modify root `Cargo.toml` (workspace deps: `nix = { version = "0.29", default-features = false, features = ["signal", "process"] }`, `command-fds = { version = "0.3", default-features = false }`, `tracing = { version = "0.1", default-features = false, features = ["std", "attributes"] }`, `proptest = { version = "1", default-features = false, features = ["std"] }`), `crates/shep-daemon/Cargo.toml`; Create `runner.rs`, `channel.rs`, `fake.rs`; Modify `lib.rs`.

**shep-daemon Cargo.toml (exact):**

```toml
[features]
# Option: expose the scripted fake runner to other crates' tests (2b reuses it).
test-fakes = []

[dependencies]
shep-core.workspace = true
tokio = { workspace = true, features = ["rt", "process", "io-util", "time", "sync", "net", "macros"] }
nix.workspace = true
command-fds.workspace = true
tracing.workspace = true
serde.workspace = true
serde_json.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["test-util", "rt-multi-thread"] }
proptest.workspace = true
tempfile.workspace = true
```

(No `bytes` in [dependencies] — channel framing is newline-JSON over `BufReader::lines()`; add it only if a real need appears.)

**Interfaces (the load-bearing abstraction — verified compile-shape):**

```rust
/// One exit observation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitOutcome {
    /// Exit code on normal exit
    pub code: Option<i32>,
    /// Raw unix signal number when killed (SIGTERM=15, SIGKILL=9, ...)
    pub signal: Option<i32>,
}

/// Typed stop signal; `as_raw` gives the unix number so fake and real
/// runners record identical `ExitOutcome`s
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal { Term, Int, Quit, Usr2, Kill }
impl StopSignal {
    #[must_use]
    pub fn as_raw(self) -> i32 { match self { Self::Term => 15, Self::Int => 2, Self::Quit => 3, Self::Usr2 => 12, Self::Kill => 9 } }
}

/// One stdout/stderr line from a child
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// True = stderr
    pub err: bool,
    /// The line, no trailing newline
    pub line: String,
}

/// IO endpoints handed back by spawn — the runner pumps internally.
/// The sheep task owns this and MUST drain every receiver (an undrained
/// `from_child` back-pressures a metric-emitting child until it stalls on
/// its own fd-3 write — see the Task 9 `select!` model).
pub struct ProcIo {
    /// stdout+stderr lines
    pub logs: tokio::sync::mpsc::Receiver<LogLine>,
    /// Parsed child→daemon shepherd-channel messages
    pub from_child: tokio::sync::mpsc::Receiver<ChildMessage>,
    /// daemon→child shepherd-channel sender
    pub to_child: tokio::sync::mpsc::Sender<ShepherdMessage>,
}

/// A live child. `wait`'s future is explicitly Send (RPITIT) because the
/// sheep task that owns the proc is tokio::spawn'ed.
pub trait RunningProcess: Send + 'static {
    fn pid(&self) -> u32;
    /// Resolves exactly once with the exit outcome
    fn wait(&mut self) -> impl core::future::Future<Output = ExitOutcome> + Send;
    /// Sends a signal; Err on delivery failure
    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError>;
    /// SIGKILLs the whole process group/tree
    fn kill_tree(&mut self) -> Result<(), RunnerError>;
}

/// Spawn seam between engine and OS
pub trait ProcessRunner: Send + Sync + 'static {
    /// The live-child type this runner produces
    type Proc: RunningProcess;
    /// Spawns per the spec, returning the proc + its IO bundle
    ///
    /// # Errors
    /// - [`RunnerError::SpawnFailed`] — exec failure, permissions, missing binary.
    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError>;
}

/// Everything a spawn needs, pre-assembled by Task 6
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: std::collections::BTreeMap<String, String>,
    pub out_file: std::path::PathBuf,
    pub err_file: std::path::PathBuf,
    /// Open the shepherd channel (fd 3 socketpair)
    pub channel: bool,
}
// uid/gid (user/group): deferred to Phase 2b (privilege-drop design) — see roadmap note.

/// Error type returned from spawn and process control
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerError {
    /// The OS refused the spawn (exec failure, permissions, missing binary)
    SpawnFailed(String),
    /// Signal delivery failed (already reaped, EPERM)
    SignalFailed(String),
}
// manual Display (write! per arm) + impl core::error::Error — model: NormalizeError.
```

`channel.rs` (needed by ProcIo — lands in this task):

```rust
/// Child→daemon shepherd-channel message (spec §7 — kebab-case kinds)
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ChildMessage {
    /// `{"kind":"ready"}` — readiness signal (wait_ready gate)
    Ready,
    /// Custom metric sample
    Metric { name: String, value: f64 },
    /// Reply to a daemon-initiated action
    ActionReply { action: String, body: String },
}

/// Daemon→child message
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ShepherdMessage {
    /// Graceful-stop request (shutdown_with_message)
    Shutdown,
    /// Custom action dispatch
    Action { name: String },
}
```

with serde fixture tests pinned FROM SPEC STRINGS — all five kinds: `{"kind":"ready"}`, `{"kind":"metric","name":"rps","value":42.0}`, `{"kind":"action-reply","action":"x","body":"y"}`, `{"kind":"shutdown"}`, `{"kind":"action","name":"gc"}`.

**fake.rs** (`#[cfg(any(test, feature = "test-fakes"))]`): `ScriptedRunner::new(Vec<ProcScript>)`; `ProcScript { pub delay_ms: u64, pub outcome: ExitOutcome, pub obeys_signal: bool }`. `FakeProc` semantics (verified against Tasks 8/9 needs):
- **Cancel-safe wait (matches `tokio::process::Child::wait`'s documented cancel safety):** the exit deadline `tokio::time::Instant::now() + delay_ms` is computed AT SPAWN and stored; `wait()` uses `sleep_until(deadline)` — dropping and re-creating the wait future never restarts the clock.
- `wait()` races: (a) `sleep_until(exit_deadline)` → script outcome, (b) a `Notify`d signal/shutdown event → if `obeys_signal`, resolve NOW with `ExitOutcome { code: None, signal: Some(sig.as_raw()) }` (Shutdown message counts as Term) — works whether the event fired before OR during the wait, (c) a kill event → ALWAYS resolve now with `signal: Some(9)`.
- `signal(sig)`: records + notifies. `kill_tree()`: records + notifies kill. Assertions via `ScriptedRunner::kill_counts() -> Vec<u32>` — kill_tree call count per proc, INDEXED BY SPAWN ORDER (the only kill-assertion accessor).
- Spawn returns `(FakeProc, ProcIo)`; `ScriptedRunner::io_handles(spawn_index) -> FakeIo` where `FakeIo { pub logs_tx: mpsc::Sender<LogLine>, pub from_child_tx: mpsc::Sender<ChildMessage>, pub to_child_rx: mpsc::Receiver<ShepherdMessage> }` — tests inject Ready/logs and OBSERVE daemon→child messages. The fake internally watches its own to_child stream: receipt of `ShepherdMessage::Shutdown` triggers the signal event (rule b). Exhausted script → `Err(RunnerError::SpawnFailed("script exhausted".into()))`.
- Helpers: `const_exit(code)`, `stable_then_exit(ms, code)`, `never_exits()`, `ignores_signals()` (never exits + obeys_signal=false).
- WHY comment at the type: deterministic + instant under paused clock; real OS behavior covered only by tests/real_runner.rs.

- [ ] **Step 1: failing tests** in fake.rs — nine, full asserts: (1) const_exit resolves immediately with code; (2) stable_then_exit(5000,0) needs `advance(5s)`; (3) signal-before-wait resolves with as_raw number; (4) **signal-during-pending-wait resolves** (spawn, start wait in a task, advance a tick, signal, join); (5) **kill_tree resolves a pending wait with signal 9 and kill_counts()[0] == 1**; (6) exhausted script errors; (7) **cancel-safety: start wait, drop it after advance(1s), re-await — still resolves at the ORIGINAL spawn-relative deadline, not deadline+1s** (N1 regression guard); (8) **Shutdown over to_child resolves an obeys_signal wait with signal 15**, observable on `io_handles(0).to_child_rx`; (9) **io_handles: `from_child_tx` delivers `ChildMessage::Ready` through the spawned `ProcIo.from_child`; a message sent on `ProcIo.to_child` is observed on `to_child_rx`**.
- [ ] **Step 2:** RED (compile). **Step 3:** implement runner.rs + channel.rs + fake.rs + lib.rs tree. **Step 4:** focused + four gates. **Step 5:** commit `feat(daemon): runner abstraction, shepherd channel types, scripted fake` + footer.

---

### Task 4: Entry + budget + backoff (pure)

**Files:** Create `entry.rs`, `backoff.rs`; modify lib.rs.

**Interfaces:**
- `ProcessEntry { pub id: u32, pub spec: ResolvedApp, pub instance: u32, pub status: ProcStatus, pub pid: Option<u32>, pub restarts: u32, pub started_at: Option<tokio::time::Instant>, pub budget: RestartBudget, pub reload: ReloadState }` — **`tokio::time::Instant`** (paused-clock-aware; std Instant would make every scripted run look 0ms).
- `restarts` counts **respawns performed** (not exits): initial spawn is not a restart.
- `RestartBudget` (private `unstable_count`): `note_exit(&mut self, uptime: Duration, min_uptime: Duration) -> Stability` (`Stable` resets to 0), `unstable_count(&self) -> u32`, `exhausted(&self, max_restarts: u32) -> bool`, `reset(&mut self)`, `Default`.
- `ReloadState { None, SpawningReplacement { new_id: u32 }, Draining { old_pid: u32 } }` (data only).
- `backoff.rs`: `pub fn restart_delay(app: &AppConfig, consecutive_unstable: u32) -> Option<Duration>` — `restart_delay` fixed wins; else if `exp_backoff_restart_delay = Some(initial)`: **iterative integer rule `d = min(d * 3 / 2, 15000)` starting at `initial` for `consecutive_unstable = 1`**, applied `consecutive_unstable - 1` times; **`consecutive_unstable == 0` → `None` (stable exit ⇒ immediate restart)**; else None.

- [ ] **Step 1: failing tests.** Pinned sequence (initial 100ms, mechanically computed from the integer rule — DO NOT hand-adjust):

```rust
    #[test]
    fn backoff_sequence_is_pinned() {
        let mut app = shep_core::config::AppConfig::minimal("p", "./p");
        app.exp_backoff_restart_delay = Some("100".parse().unwrap());
        let expected: [u64; 15] = [
            100, 150, 225, 337, 505, 757, 1135, 1702, 2553, 3829, 5743, 8614,
            12921, 15000, 15000,
        ];
        for (i, want) in expected.iter().enumerate() {
            let got = restart_delay(&app, (i + 1) as u32).unwrap();
            assert_eq!(got.as_millis() as u64, *want, "consecutive_unstable={}", i + 1);
        }
    }
```

plus: `restart_delay(_, 0) == None` (stable exit); fixed `restart_delay` field overrides backoff; neither set → None at any count. Budget tests: unstable increments, stable resets, exhausted at 16, reset().

- [ ] **Step 2-5:** RED → implement → gates → commit `feat(daemon): entry, budget, pinned integer backoff` + footer.

---

### Task 5: Restart brain (pure + proptest)

**Files:** Create `brain.rs`; modify lib.rs.

**Interfaces:** `pub enum Decision { Restart { delay: Option<Duration> }, CleanStop, Errored }`; `pub fn decide_on_exit(app: &AppConfig, budget: &mut RestartBudget, uptime: Duration, exit: ExitOutcome, manual_stop: bool) -> Decision`. Rules in order: `manual_stop` → CleanStop; **`exit.code` matched against `stop_exit_codes` ONLY when `code.is_some()` — signal-terminated exits (code None) never match stop_exit_codes**; `!autorestart` → CleanStop; else note_exit → exhausted → Errored, else Restart with `restart_delay(app, budget.unstable_count())`.

- [ ] **Step 1: failing tests** — six units: manual stop wins; stop_exit_codes match → CleanStop; **signal exit (code None, signal 15) with `stop_exit_codes = [0]` → Restart, never CleanStop**; autorestart false → CleanStop; 16 consecutive unstable → Errored at the 16th decision; stable exit → Restart with delay None. Plus the proptest:

```rust
    proptest::proptest! {
        #[test]
        fn budget_errors_exactly_at_max_restarts(
            exits in proptest::collection::vec(0u64..500, 16..64)
        ) {
            // Every uptime < min_uptime(1000ms): the 16th decision must be
            // Errored, none before it.
            let app = shep_core::config::AppConfig::minimal("p", "./p");
            let mut budget = RestartBudget::default();
            for (i, ms) in exits.iter().enumerate() {
                let d = decide_on_exit(&app, &mut budget,
                    std::time::Duration::from_millis(*ms),
                    ExitOutcome { code: Some(1), signal: None }, false);
                if i < 15 {
                    proptest::prop_assert!(matches!(d, Decision::Restart { .. }));
                } else {
                    proptest::prop_assert!(matches!(d, Decision::Errored));
                    break;
                }
            }
        }
    }
```

- [ ] **Step 2-5:** RED → implement → gates → commit `feat(daemon): restart brain — signal exits never match stop_exit_codes` + footer.

---

### Task 6: Spawn assembly (pure)

**Files:** Create `assemble.rs`; modify lib.rs; modify `crates/shep-core/src/config/app.rs` doc comments only.

**Interfaces:** `pub fn instance_slots(existing: &[u32], count: u32) -> Vec<u32>` (lowest-free); `pub fn assemble(app: &ResolvedApp, instance: u32, paths: &ShepPaths) -> SpawnSpec` — env = app env + slot var (`increment_var` name or `SHEP_INSTANCE`); interpreter: `None` → program = script; `Some("none")` → program = script; `Some(interp)` → program = interp, args = [script] + app args; log defaults `logs/<name>-<instance>-out.log` / `-err.log`, `merge_logs` → `logs/<name>-out.log`; explicit `out_file`/`err_file` win; `channel = app.wait_ready || app.shutdown_with_message`. **Also update `app.rs` `out_file`/`err_file` field docs to state the instance-suffixed default + merge_logs collapse** (they currently claim un-suffixed).

- [ ] **Step 1: failing tests** — slots `([], 3) → [0,1,2]`, `([0,2], 2) → [1,3]`; SHEP_INSTANCE=1 present; custom increment_var renames; interpreter all three cases; merge_logs vs suffixed paths; explicit out_file wins; channel flag from wait_ready.
- [ ] **Step 2-5:** RED → implement → gates → commit `feat(daemon): spawn assembly — slots, env, interpreter, log paths` + footer.

---

### Task 7: Real tokio runner

**Files:** Create `tokio_runner.rs`, `tests/real_runner.rs`; modify lib.rs.

**Interfaces:** `TokioRunner::new()` implementing `ProcessRunner<Proc = TokioProc>`. Spawn: `tokio::process::Command` + `.process_group(0)` (std CommandExt, safe) + stdio piped; shepherd channel when `spec.channel`: `tokio::net::UnixStream::pair()`, child end → `into_std()?` → **`OwnedFd::from(std_end)`** (SAFE — `std::os::fd::OwnedFd: From<UnixStream>`; do NOT go through `into_raw_fd()` + `from_raw_fd`, which needs `unsafe` and trips `#![deny(unsafe_code)]` — N9), passed as fd 3 via `command_fds::FdMapping { parent_fd: owned, child_fd: 3 }` applied to `command.as_std_mut()` with `CommandFdExt::fd_mappings(..)` BEFORE spawn; env `SHEP_CHANNEL_FD=3`; **the std command owning the mapping must be dropped after `spawn()` so the parent's copy of the child fd closes and the daemon end sees EOF**. Daemon end split (`into_split()`): read half → `BufReader::lines()` → newline-JSON decode → `from_child` mpsc; `to_child` mpsc → write half encode. Log pumps: `BufReader::new(stdout).lines()` loop (io-util only, NO tokio-stream dep) → `LogLine { err: false, .. }` to the `logs` mpsc AND append to `spec.out_file` (create parent dirs); same for stderr. `wait()` = `child.wait()` → ExitOutcome via `ExitStatusExt` (`code()`, `signal()`). `signal()` = `nix::sys::signal::kill(Pid::from_raw(pid as i32), sig)`; `kill_tree()` = `kill(Pid::from_raw(-(pid as i32)), SIGKILL)` (negative = process group; nix safe API — **no sys.rs needed**; if any raw libc becomes unavoidable it goes to sys.rs per constraint). **Verify the `command-fds` 0.3 `FdMapping` field shape at implementation time** (the crate wasn't resolvable offline during planning) — adjust field names to the actual API if 0.3 differs.

- [ ] **Step 1: failing tests.** `tests/real_runner.rs` (REAL time — comment: `// real time: integration tier exercises the actual OS; IR-38 deviation deliberate: behavioral OS tests need a separate binary so unit tests stay paused-clock-pure`):
  1. spawn `/bin/sh -c 'echo out-line; echo err-line 1>&2; exit 7'` → wait → `code == Some(7)`; logs mpsc yielded both lines with correct `err` flags; out_file/err_file contain them.
  2. spawn `/bin/sh -c 'trap "" TERM; while true; do sleep 1; done'` → signal(Term) → wait for 300ms (real sleep) still running → kill_tree() → wait resolves with `signal == Some(9)` quickly.
  3. channel: `/bin/sh -c 'printf "{\"kind\":\"ready\"}\n" >&3; sleep 5'` with channel=true → `from_child.recv()` yields `ChildMessage::Ready`; then kill_tree reaps.
- [ ] **Step 2:** RED. **Step 3:** implement. **Step 4:** focused (`cargo test -p shep-daemon --test real_runner`) + four gates. **Step 5:** commit `feat(daemon): tokio runner — process groups, socketpair channel, log capture` + footer.

---

### Task 8: Kill ladder

**Files:** Create `kill.rs`; modify lib.rs.

**Interfaces:** `pub async fn kill_process<P: RunningProcess>(proc: &mut P, app: &AppConfig, to_child: Option<&tokio::sync::mpsc::Sender<ShepherdMessage>>) -> ExitOutcome` — runs INSIDE the sheep task (which owns the proc; see Task 9). Ladder: if `app.shutdown_with_message` and `to_child` present → send `ShepherdMessage::Shutdown`; else `proc.signal(stop_signal(app))` where `fn stop_signal(app) -> StopSignal` parses `kill_signal` name ("SIGTERM"/"TERM"/"SIGINT"/... case-insensitive; unknown → Term + `tracing::warn!`); then `tokio::time::timeout(app.kill_timeout.as_duration(), proc.wait())`; on timeout → `proc.kill_tree()` (delivery failure logged, not fatal) → `proc.wait().await`.

- [ ] **Step 1: failing tests** (paused clock, fakes; the fake's control-event resolution — Task 3 rules — makes all four transcription, not invention): (1) obedient: script `{ delay_ms: u64::MAX, outcome: <any>, obeys_signal: true }` → `kill_process` resolves with `signal == Some(15)`, `kill_counts()[0] == 0`, elapsed 0; (2) defiant: `ignores_signals()` → elapsed exactly 1600ms (default kill_timeout) via `tokio::time::Instant::now()` deltas under the paused clock, `kill_counts()[0] == 1`, outcome signal 9; (3) `shutdown_with_message = true` + `obeys_signal` fake + a `to_child` sender → `ShepherdMessage::Shutdown` observed on `io_handles(0).to_child_rx`, the fake resolves the wait with signal 15 (Task 3 rule), no signal() call recorded; (4) custom `kill_signal = "SIGINT"` → `proc.signal(StopSignal::Int)` → outcome signal 2.
- [ ] **Step 2-5:** RED → implement (stop_signal parser + ladder) → gates → commit `feat(daemon): kill ladder — message/signal, timeout, SIGKILL tree` + footer.

---

### Task 9: Supervisor actor + sheep tasks

**Files:** Create `supervisor.rs`; modify lib.rs.

**Interfaces (Phase 2b consumes exactly this):**

```rust
/// Public commands (the handle wraps these; the actor also receives
/// internal events through the same channel via `Msg`)
pub enum Command {
    Start { apps: Vec<ResolvedApp>, reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>> },
    Stop { selector: ProcessSelector, reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>> },
    Restart { selector: ProcessSelector, reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>> },
    Delete { selector: ProcessSelector, reply: oneshot::Sender<Result<Vec<u32>, SupervisorError>> },
    List { reply: oneshot::Sender<Vec<ProcessInfo>> },
    /// Graceful engine shutdown: kill ladder on every online sheep, then stop
    Shutdown { reply: oneshot::Sender<()> },
}

pub(crate) enum Msg {
    Command(Command),
    Exited { id: u32, outcome: ExitOutcome },
    RestartDue { id: u32 },
    /// ChildMessage::Ready drained by the sheep task; the actor logs and
    /// ignores it in 2a (wait_ready gating is Phase 4)
    Ready { id: u32 },
}

/// Error type returned from supervisor commands
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    /// Selector matched nothing
    NotFound,
    /// Spawn failed at start/restart (carries the runner message)
    SpawnFailed(String),
    /// The engine has shut down (actor channel closed)
    EngineStopped,
}
// manual Display + core::error::Error (IR-18/19).

#[derive(Clone)]
pub struct SupervisorHandle { tx: mpsc::Sender<Msg> }
// + async fn start/stop/restart/delete/shutdown wrapping oneshot, plus:
//   async fn list_checked(&self) -> Result<Vec<ProcessInfo>, SupervisorError>
//     (EngineStopped when the actor is gone — Phase 2b's RPC path uses this)
//   async fn list(&self) -> Vec<ProcessInfo>  (panicking convenience for tests,
//     #[track_caller]-free since it's #[cfg(test)]-adjacent usage; CLI never calls it)

/// Spawns the actor; `events` receives BusEvent::Process (+ LogOut/LogErr
/// forwarded from ProcIo.logs) — Phase 2b plugs its bus straight in.
pub fn spawn_supervisor<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    events: tokio::sync::broadcast::Sender<BusEvent>,
) -> SupervisorHandle
```

**Sheep-task ownership model (locked, revised post-verification):** for each spawned instance the actor creates a per-sheep task owning `(proc, ProcIo)` + a control receiver `mpsc::Receiver<SheepCtl>` (`pub(crate) enum SheepCtl { Kill }` — fire-and-forget, NO `done` oneshot; see N4). The sheep task runs one `select!` loop over FOUR branches:
1. `outcome = proc.wait()` → send `Msg::Exited { id, outcome }`, then **break the loop** (the proc is dead; the actor decides restart/errored/stopped).
2. `Some(SheepCtl::Kill) = ctl.recv()` → run `kill_process(&mut proc, app, to_child)` (the task owns `&mut proc`, so the kill ladder's `wait()` calls are fine) → send `Msg::Exited { id, outcome }`, break.
3. `Some(line) = io.logs.recv()` → forward `BusEvent::LogOut/LogErr` on `events`.
4. `Some(msg) = io.from_child.recv()` → `ChildMessage::Ready` → send `Msg::Ready { id }`; `Metric`/`ActionReply` → `tracing::debug!` and drain (full handling Phase 4). **This branch is mandatory — an undrained `from_child` back-pressures a metric-emitting child into a hang (N2).**

**One exit path only (N4):** every exit — natural or killed — reaches the actor as exactly one `Msg::Exited`; the actor never receives a duplicate and never blocks awaiting a kill. The actor holds `HashMap<u32, SheepSlot>` where `SheepSlot { entry: ProcessEntry, ctl: Option<mpsc::Sender<SheepCtl>>, manual: Option<ManualKind> }` (`ManualKind { Stop, Restart, Delete }`) — entries + control handles + a pending-manual-intent marker, never a proc. **Command replies are deferred, not blocking — with multi-match aggregation:** a `Stop`/`Restart`/`Delete` command resolves its selector to a set of ids, sets `manual` on each, sends each `SheepCtl::Kill`, and stashes ONE `PendingReply { remaining: HashSet<u32>, results: Vec<ProcessInfo>, reply: oneshot::Sender<..> }` in a `Vec<PendingReply>`; each arriving `Msg::Exited` that removes an id from a pending set appends the terminal `ProcessInfo`; when `remaining` empties, the actor fulfills the reply. `Shutdown` is the same aggregation over ALL online ids (empty set → reply immediately), then the actor breaks its loop. So `handle.stop(..).await` returns only after every matched sheep is terminal, and the actor loop never parks.

- **Ids** are a monotonic `u32` counter from 0, never reused. **`List` returns entries sorted by id** (the underlying map is unordered).
- **Start** expands each `ResolvedApp` through `instance_slots` + `assemble` (Task 6) into `instances` entries; the actor treats a freshly-spawned instance as `Online` immediately (the `wait_ready` gate is Phase 4 — see roadmap note). Spawn failure at Start → that entry is registered with status `errored`; the `Start` reply is `Err(SupervisorError::SpawnFailed(..))` on the FIRST failure but the already-registered entries persist (test 6 relies on this).
- **Exit handling** (on `Msg::Exited`): actor computes uptime (`tokio::time::Instant::now() - started_at`), reads+clears any `manual` marker, calls `decide_on_exit(app, &mut budget, uptime, outcome, manual_stop = manual.is_some())`. `Decision::Restart { delay }` → status `waiting-restart` + a timer task sending `Msg::RestartDue { id }` after `delay` (None → send immediately); on `RestartDue` respawn and `restarts += 1`. `Errored` → status `errored`. `CleanStop` → status `stopped` (or `deleted`+removed if `manual == Some(Delete)`). Manual `Restart` respawns with `budget.reset()` (spec §4: manual action resets budget).
- **Every transition emits `BusEvent::Process { event, info, manually, at_ms }`** on `events` (`at_ms`: wall-clock `SystemTime` — the ONE real-time read, isolated in a `now_ms()` helper). Tests assert terminal state by CONSUMING this event stream, not by counting scheduler ticks (N5).

- [ ] **Step 1: failing tests** — full code, paused clock, ScriptedRunner, each test its own script (IR-34). Terminal-state assertions consume the event stream (N5): under the paused clock, awaiting `rx.recv()` parks the test task, the runtime auto-advances virtual time through the pending backoff timers, and the terminal `BusEvent` arrives deterministically — no `advance`/`yield_now` tick-counting. Shared helper:

```rust
// Drives virtual time by parking on recv(); returns when the id reaches `kind`.
async fn await_event(rx: &mut tokio::sync::broadcast::Receiver<BusEvent>, id: u32, kind: ProcessEventKind) {
    loop {
        match rx.recv().await {
            Ok(BusEvent::Process { event, info, .. }) if info.id == id && event == kind => return,
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(e) => panic!("event stream closed before {kind:?} for id {id}: {e}"),
        }
    }
}
```

The nine tests with exact assertions:

```rust
#[tokio::test(start_paused = true)]
async fn start_lists_online_instances() {
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let runner = ScriptedRunner::new(vec![never_exits(), never_exits()]);
    let handle = spawn_supervisor(runner, test_paths(), events);
    let mut app = shep_core::config::AppConfig::minimal("web", "./srv");
    app.instances = 2;
    let infos = handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    assert_eq!(infos.len(), 2);
    let list = handle.list().await;
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|i| i.status == ProcStatus::Online));
    assert_eq!(list.iter().map(|i| i.id).collect::<Vec<_>>(), vec![0, 1]);
}

#[tokio::test(start_paused = true)]
async fn crash_loop_erroreds_after_budget_with_pinned_delays() {
    let (events, mut rx) = tokio::sync::broadcast::channel(1024);
    // 16 spawns: initial + 15 restarts; every exit instant (unstable).
    let runner = ScriptedRunner::new((0..16).map(|_| const_exit(1)).collect());
    let handle = spawn_supervisor(runner, test_paths(), events);
    let mut app = shep_core::config::AppConfig::minimal("crash", "./boom");
    app.exp_backoff_restart_delay = Some("100".parse().unwrap());
    handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    // Park on the event stream; auto-advance drives through all 15 backoff
    // delays (pinned Task 4 sequence, summing 68_571ms of virtual time).
    await_event(&mut rx, 0, ProcessEventKind::Errored).await;
    let list = handle.list().await;
    assert_eq!(list[0].status, ProcStatus::Errored);
    assert_eq!(list[0].restarts, 15); // respawns performed, not exits
}

#[tokio::test(start_paused = true)]
async fn stable_run_resets_budget() {
    let (events, mut rx) = tokio::sync::broadcast::channel(256);
    let mut script = vec![const_exit(1), const_exit(1), const_exit(1)];
    script.push(stable_then_exit(2000, 1)); // > min_uptime 1000ms => stable
    script.extend((0..16).map(|_| const_exit(1)));
    let runner = ScriptedRunner::new(script);
    let handle = spawn_supervisor(runner, test_paths(), events);
    let app = shep_core::config::AppConfig::minimal("flappy", "./f");
    handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    // 3 unstable (no backoff => immediate), 1 stable run resets the budget,
    // then 16 more unstable before errored.
    await_event(&mut rx, 0, ProcessEventKind::Errored).await;
    let list = handle.list().await;
    assert_eq!(list[0].status, ProcStatus::Errored);
    // 3 + 1 + 15 respawns after the initial spawn = 19
    assert_eq!(list[0].restarts, 19);
}

#[tokio::test(start_paused = true)]
async fn manual_stop_prevents_restart() {
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let runner = ScriptedRunner::new(vec![ProcScript { delay_ms: u64::MAX, outcome: ExitOutcome { code: None, signal: None }, obeys_signal: true }]);
    let handle = spawn_supervisor(runner, test_paths(), events);
    let app = shep_core::config::AppConfig::minimal("svc", "./svc");
    handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    let stopped = handle.stop(ProcessSelector::Name("svc".to_string())).await.unwrap();
    assert_eq!(stopped[0].status, ProcStatus::Stopped); // deferred reply: already terminal
    // No restart is ever scheduled: advancing a full minute yields no further
    // events and the status stays Stopped.
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
}

#[tokio::test(start_paused = true)]
async fn stop_exit_codes_mean_clean_stop() {
    let (events, mut rx) = tokio::sync::broadcast::channel(64);
    let runner = ScriptedRunner::new(vec![const_exit(0)]);
    let handle = spawn_supervisor(runner, test_paths(), events);
    let mut app = shep_core::config::AppConfig::minimal("oneshot", "./job");
    app.stop_exit_codes = vec![0];
    handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    await_event(&mut rx, 0, ProcessEventKind::Stop).await;
    assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
}

#[tokio::test(start_paused = true)]
async fn spawn_failure_surfaces_and_erroreds() {
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let runner = ScriptedRunner::new(vec![]); // exhausted immediately
    let handle = spawn_supervisor(runner, test_paths(), events);
    let app = shep_core::config::AppConfig::minimal("ghost", "./missing");
    let err = handle.start(vec![normalize(app).unwrap()]).await.unwrap_err();
    assert!(matches!(err, SupervisorError::SpawnFailed(_)));
    assert_eq!(handle.list().await[0].status, ProcStatus::Errored);
}

#[tokio::test(start_paused = true)]
async fn delete_and_selectors_route() {
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let runner = ScriptedRunner::new(vec![never_exits(), never_exits()]);
    let handle = spawn_supervisor(runner, test_paths(), events);
    let mut a = shep_core::config::AppConfig::minimal("api", "./a");
    a.fold = Some("backend".to_string());
    let b = shep_core::config::AppConfig::minimal("web", "./w");
    handle.start(vec![normalize(a).unwrap(), normalize(b).unwrap()]).await.unwrap();
    let hits = handle.stop(ProcessSelector::Fold("backend".to_string())).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "api");
    let deleted = handle.delete(ProcessSelector::Name("web".to_string())).await.unwrap();
    assert_eq!(deleted, vec![1]);
    assert_eq!(handle.list().await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn manual_restart_resets_budget_and_respawns() {
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    // Two unstable crashes bring the budget to 2; then a manual restart must
    // reset it (spec §4) and respawn. Script needs FOUR procs: initial +
    // 2 crash-respawns landing on the long-lived third, + the respawn the
    // manual restart itself performs.
    let runner = ScriptedRunner::new(vec![
        const_exit(1), const_exit(1), never_exits(), never_exits(),
    ]);
    let handle = spawn_supervisor(runner, test_paths(), events);
    let app = shep_core::config::AppConfig::minimal("svc", "./svc");
    handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    // Sync on state, not on the repeated Online event: immediate restarts
    // mean restarts==2 once the never_exits proc is up.
    loop {
        let info = handle.list().await.remove(0);
        if info.restarts == 2 && info.status == ProcStatus::Online { break; }
        tokio::task::yield_now().await;
    }
    let restarted = handle.restart(ProcessSelector::Name("svc".to_string())).await.unwrap();
    assert_eq!(restarted[0].status, ProcStatus::Online);
    // Budget reset by the manual action: online, not errored.
    assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    assert_eq!(handle.list().await[0].restarts, 3);
}

#[tokio::test(start_paused = true)]
async fn shutdown_kills_all_and_stops_the_engine() {
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let runner = ScriptedRunner::new(vec![never_exits(), never_exits()]);
    let handle = spawn_supervisor(runner, test_paths(), events);
    let mut app = shep_core::config::AppConfig::minimal("web", "./srv");
    app.instances = 2;
    handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    handle.shutdown().await; // kill ladder on every online sheep, then stop
    // After shutdown the handle's channel is closed; further commands error.
    assert!(handle.list_checked().await.is_err());
}
```

(`test_paths()` = ShepPaths::resolve over a tempdir; helper in the test mod. `list_checked()` = the fallible variant returning `Result<_, SupervisorError>` used to observe a closed actor; `list()` is its unwrap-on-live convenience.)

- [ ] **Step 2:** RED. **Step 3:** implement supervisor.rs per the locked model. **Step 4:** focused + four gates (workspace). **Step 5:** commit `feat(daemon): supervisor actor with sheep-task ownership + deterministic lifecycle tests` + footer.

---

### Task 10: Engine polish + crate docs

**Files:** Modify `crates/shep-daemon/src/lib.rs`.

- [ ] **Step 1:** Crate doc (IR-27 mini-guide): summary line, `## Engine taxonomy` (h5 groups: pure logic — brain/backoff/assemble/entry; abstractions — runner/fake; OS tier — tokio_runner/kill; orchestration — supervisor), `# Quick start` fenced ```no_run``` example: build a `ScriptedRunner`, `spawn_supervisor`, start one app, list (compiles under `test-fakes` feature note). `#[inline]` audit per IR-25 on trivial accessors (StopSignal::as_raw, budget accessors). Bottom reference-link block.
- [ ] **Step 2:** four gates (doctest compiles).
- [ ] **Step 3:** commit `docs(daemon): crate mini-guide + inline audit` + footer.

## Phase 2a exit criteria

- Ten tasks committed; four gates green.
- Deterministic suite: fake-driven paused-clock supervisor tests with pinned backoff sums; brain proptest; pure-fn units. Real tier: 3 OS integration tests.
- `SupervisorHandle` + `events` broadcast + `Shutdown` verb = the exact surface Phase 2b's RPC server consumes.
- No unsafe anywhere (or confined to sys.rs with SAFETY if forced).
- Parked Phase-1 items closed (Tasks 1-2); deferred items named in the roadmap note.
