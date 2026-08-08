# shep Phase 2b — Daemon Plane Implementation Plan (rev 1, source-verified)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build shep-daemon's wire plane on top of Phase 2a's engine — the event bus (broadcast ring → per-subscriber forwarder tasks with server-side topic globs), the UDS RPC server (handshake, peer-cred auth, framed dispatch, per-call deadlines), the muster roll (`flock.json` atomic writes + restore), daemon boot (dirs, pidfile, stale-socket recovery, signals, readiness pipe) — plus the two items 2a parked for this phase: uid/gid spawn and the supervisor proptest.

**Architecture:** 2a's `SupervisorHandle` + `broadcast::Sender<BusEvent>` are the only seams the plane touches; nothing here reaches into the actor. Pure decision logic (topic filtering, roll building, deadline budgets, credential resolution) is IO-free and tested instantly; the tokio-heavy pieces (connection loop, forwarder, writer, accept loop) are thin shells over those pure cores. **Portability split (locked):** `bus.rs`, `snapshot.rs`, `rpc.rs` compile everywhere; `server.rs`, `boot.rs`, `sys.rs`, `privilege.rs` are `#[cfg(unix)]` — matching the existing `[target.'cfg(unix)'.dependencies]` gate on nix/command-fds. Windows named pipes reuse `rpc.rs` unchanged when that tier lands. **One exception inside the portable tier:** `snapshot.rs`'s atomic-write and restore *logic* is fully portable, but the 0600-owner-only guarantee it produces is a unix permission-bit property with no Windows ACL equivalent to assert — the one test that checks the mode bit is `#[cfg(unix)]` (Task 3), not the whole file.

**Tech Stack:** tokio (rt, process, io-util, time, sync, net, macros, signal; test-util + rt-multi-thread in dev), tokio-util (codec), futures-util, bytes, globset, tempfile, nix (signal, process, user), command-fds, tracing, serde/serde_json, proptest (dev).

**Phase roadmap context:** Plan 2b of the Phase-2 pair (2a = runner abstraction, restart brain, kill ladder, supervisor actor). Closes 2a's two assigned deferrals: **uid/gid spawn (`user`/`group`, privilege-drop design — Task 8)** and **supervisor-level proptest over command/exit interleavings (IR-37 — Task 9)**. Deferred out of 2b, explicitly:

- **Windows named-pipe transport** — Phase 5 functional tier; `ShepPaths::pipe_name()` and the portable `rpc.rs` dispatch are the seam, `server.rs` is the only piece that gets a sibling.
- **Client reconnect backoff (spec §6: 100ms ×1.5, cap 5s)** — that is client-side state; it lands in shep-client with connect-or-spawn (Phase 3).
- **`Request::Muster` / `Request::MusterSave` verbs and the `shep muster` CLI** — Phase 3. 2b ships the primitives those verbs call: `RpcContext::snapshot_now()` and boot-time restore.
- **`channel.*` bus topics (spec §6 topic grammar)** — no `BusEvent` variant carries shepherd-channel traffic until custom actions land (Phase 4). `TopicFilter` globs `BusEvent::topic()`, so it matches new topics the day they exist; no filter change needed.
- **Real per-sheep log-file reopen on SIGUSR2** — 2b INSTALLS the handler (load-bearing on its own: SIGUSR2's default disposition is *terminate*, so an unhandled rotation signal kills the daemon) and re-creates a missing log dir. Reopening the runner's live file handles needs a log-writer seam that lands with `flush`/`reopen` in Phase 4.
- **Supplementary groups on uid/gid spawn** — `std::os::unix::process::CommandExt::groups` is unstable; 2b sets uid+gid only and documents the gap.
- **`wait_ready` gating, probes, watcher, cron, memory enforcement, reload execution** — Phase 4 (carried forward from 2a unchanged).
- **SECURITY.md (IR-42) and per-crate CHANGELOGs (IR-45)** — release phase. 2b writes the canonical rustdoc `# Security` block (IR-29) that SECURITY.md will link to, and records the protocol addition in the `protocol` module doc.

## Global Constraints

- **Invoke the `shep-idiomatic-rust` skill before writing any Rust**; cite IR rules. Clean-room: never open `/Users/rin/GitHub/pm2`.
- **Workspace MSRV floor is Rust 1.88** (root `Cargo.toml:15`, `rust-version = "1.88"`, forced by serde-saphyr's let-chains). Every dependency this phase touches (Task 2's `nix` "user" feature, Task 2's new `shep-daemon` deps) must build at that floor — the minimal-versions rehearsals in Tasks 2 and 11 are how that's checked — and the CI matrix's non-stable leg pins to 1.88, not a moving target.
- No panicking constructors outside shep-cli (IR-21); `core::error::Error` never std; per-module error enums, variant docs = precise conditions (IR-18/19); `# Errors` on Result pub fns (IR-28). Error enums that wrap `io::Error` implement `source()` and drop `Clone`/`PartialEq` for that enum only (IR-19) — their tests assert with `matches!`, not `assert_eq!`.
- **Unsafe policy:** shep-daemon lib.rs keeps `#![deny(unsafe_code)]`; **Task 7 spends the phase's only unsafe** — one block in `sys.rs` behind a module-scoped `#[allow(unsafe_code)]`, a per-block `// SAFETY:`, and the IR-24 rationale essay (rejected alternative + why each failure mode can't happen). No other module may contain unsafe.
- **Paused clock is the default test mode** (`#[tokio::test(start_paused = true)]`) — with one hard exception introduced by this phase: **paused clock and real IO must never mix.** Auto-advance fires when the runtime goes idle; a socket wakeup arrives from outside the runtime, so a `timeout()` around a real read can expire *before* the peer's bytes land. Every test that drives a real `UnixStream`/`UnixListener`/child process runs on the real clock under plain `#[tokio::test]` **with a one-line comment saying why**. Fake-driven tests (bus, snapshot writer, dispatch, supervisor) stay paused.
- **No hand-computed number sequences anywhere.** Derive counts mechanically or compute them in-test from the value under test (e.g. read the actual `Lagged(n)` and assert the notice carries the same `n`). 2a's pinned backoff array is the *only* hand-pinned sequence in the workspace and this phase does not add a second.
- **Every serde attribute is cross-checked against the shipped enum representations** before writing a fixture: `Request`/`Response`/`SelectorSpec` are `#[serde(tag = "kind", ...)]` (Response adds `content = "data"`), `BusEvent` is adjacently tagged `tag = "event", content = "data"`, `RpcErrorCode`/`ProcessEventKind` are bare `rename_all = "snake_case"`, `ProcStatus` is `kebab-case`, `Reply::result`/`HelloReply` use stock serde `Result` (`{"Ok":…}` / `{"Err":…}`). Read the real file before pinning bytes.
- **Any tokio trait method a spawned task calls needs an explicitly `Send` future.** Generic helpers awaited from inside `tokio::spawn` carry `+ Send` on the future bound (`F: Future<Output = Outcome> + Send`) rather than relying on inference through a trait object.
- **Fakes and loop futures must be cancel-safe.** Anything re-created each `select!` iteration (`sleep_until(stored_deadline)`, `recv()`, `accept()`) recomputes from stored state, never from "now" — the same discipline `FakeProc`'s spawn-time `exit_deadline` already encodes.
- **Terminal state is asserted by consuming the event stream, never by counting scheduler ticks** (`yield_now` loops, `advance` guesses). Where an event cannot express the assertion (a file write), expose a real counter the daemon would want anyway.
- **Unix-only dependencies go in `[target.'cfg(unix)'.dependencies]`, and the modules that use them are `#[cfg(unix)]`.** A Windows CI leg must still compile the portable tier.
- **Rustdoc intra-doc links must never point at private items** — docs CI runs `cargo +nightly doc --workspace --all-features --no-deps` with `-Dwarnings --cfg docsrs`. Link only `pub` items; refer to private helpers in plain code spans.
- **Snippet elision rule:** this plan's code blocks elide `///` docs and some `#[derive(Debug)]` for brevity — the workspace DENIES `missing_docs` and `missing_debug_implementations`, so implementers add a doc to every public item/field/variant and `Debug` (redacting where secrets travel, IR-41) to every public type as they transcribe.
- Four gates green before every commit: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, `cargo test --workspace`. Focused runs never substitute.
- Terminology per docs/terminology.md (the flock, a sheep, the muster roll; plain names on destructive ops); plain-English errors. Conventional commits + footer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File Structure (locked)

```
crates/shep-core/src/protocol/
  frame.rs      ServerFrame (untagged decode helper)                    Task 1
  request.rs    RpcErrorCode::DeadlineExceeded                          Task 1
  mod.rs        re-exports                                              Task 1
Cargo.toml (root)   nix "user" feature; no new workspace deps           Task 2
crates/shep-daemon/Cargo.toml   globset, tempfile, tokio-util, bytes,
                                futures-util, tokio "signal"            Task 2
crates/shep-daemon/src/
  lib.rs        module tree + crate mini-guide update       Tasks 2..11
  bus.rs        BUS_CAPACITY, new_bus, TopicFilter, forwarder      Task 2
  snapshot.rs   FlockRegistry, FlockSnapshot, atomic write,
                restore, debounced writer task                     Task 3
  rpc.rs        RpcContext, budget, dispatch, Outcome (portable)   Task 4
  server.rs     RpcServer: peer-cred auth, handshake, conn loop
                (#[cfg(unix)])                                     Task 5
  boot.rs       dirs/perms, pidfile, socket bind + stale recovery  Task 6
                readiness pipe, signals, boot()/RunningDaemon      Task 7
  sys.rs        the phase's only unsafe: adopt an inherited fd     Task 7
  privilege.rs  user/group -> Credentials resolution               Task 8
  supervisor.rs proptest over command/exit interleavings (IR-37)   Task 9
crates/shep-daemon/tests/daemon_e2e.rs   real-daemon integration tier    Task 10
```

---

### Task 1: Server-frame decode helper + deadline error code (shep-core)

**Files:** Create `crates/shep-core/src/protocol/frame.rs`; Modify `protocol/mod.rs`, `protocol/request.rs`.

**Why:** a connection carries two kinds of server-to-client frame — `Reply` (answers) and `BusEvent` (subscriptions) — on one socket. Every reader (Phase 3's client, this phase's e2e tests) needs one type that decodes either. Their JSON key sets are disjoint (`id`+`result` vs `event`+`data`), so an untagged enum costs **zero wire bytes** and needs no protocol bump: existing fixtures stay valid. The `DeadlineExceeded` code lands in the same commit because Task 4 needs a way to say "your deadline expired" that is not indistinguishable from `Internal`.

**Interfaces:**

```rust
// protocol/frame.rs
/// Anything the daemon writes to a connected client
///
/// Untagged on purpose: `Reply` and `BusEvent` have disjoint key sets, so
/// this decodes either without adding a byte to the wire. The daemon
/// serializes `Reply`/`BusEvent` directly; a `ServerFrame` round-trips to
/// byte-identical output (pinned by `server_frame_is_byte_identical`).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
// Growth is anticipated: a future frame kind (progress, flow control) is
// additive here and stays additive on the wire (IR-20).
#[non_exhaustive]
pub enum ServerFrame {
    /// An answer to one [`Envelope`]
    Reply(Reply),
    /// One subscribed bus event
    Event(BusEvent),
}
```

plus in `request.rs`, one variant appended to `RpcErrorCode` (already `#[non_exhaustive]`, already `rename_all = "snake_case"` ⇒ serializes as `"deadline_exceeded"`):

```rust
    /// The request's deadline expired before the daemon finished it
    DeadlineExceeded,
```

and in `protocol/mod.rs`: `pub mod frame;` + `pub use frame::ServerFrame;`, and one sentence appended to the `PROTOCOL_VERSION` doc recording that `ServerFrame` + `DeadlineExceeded` are additive-by-the-rule (no bump).

- [ ] **Step 1: Failing tests.** `protocol/frame.rs` test mod:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        BusEvent, ProcessEventKind, ProcessInfo, Reply, Response, RpcError, RpcErrorCode,
        encode_frame,
    };
    use crate::status::ProcStatus;

    fn sample_reply() -> Reply {
        Reply { id: 7, result: Ok(Response::Pong) }
    }

    fn sample_event() -> BusEvent {
        BusEvent::Process {
            event: ProcessEventKind::Online,
            info: ProcessInfo {
                id: 3,
                name: "web".to_string(),
                status: ProcStatus::Online,
                pid: Some(4242),
                restarts: 0,
                uptime_ms: 0,
                fold: None,
            },
            manually: false,
            at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn server_frame_decodes_both_directions_of_the_stream() {
        // The two shapes are disjoint: a Reply has no `event` key and an
        // event has no `id`/`result` pair, so untagged never guesses wrong.
        let reply = r#"{"id":1,"result":{"Ok":{"kind":"pong"}}}"#;
        assert!(matches!(
            serde_json::from_str::<ServerFrame>(reply).unwrap(),
            ServerFrame::Reply(Reply { id: 1, .. })
        ));
        let event = r#"{"event":"log_out","data":{"id":3,"line":"ready"}}"#;
        assert!(matches!(
            serde_json::from_str::<ServerFrame>(event).unwrap(),
            ServerFrame::Event(BusEvent::LogOut { id: 3, .. })
        ));
    }

    #[test]
    fn server_frame_is_byte_identical_to_its_payload() {
        // The daemon encodes Reply/BusEvent directly; if wrapping ever
        // started adding bytes, every client would break at once.
        let reply = sample_reply();
        assert_eq!(
            encode_frame(&ServerFrame::Reply(reply.clone())).unwrap(),
            encode_frame(&reply).unwrap()
        );
        let event = sample_event();
        assert_eq!(
            encode_frame(&ServerFrame::Event(event.clone())).unwrap(),
            encode_frame(&event).unwrap()
        );
    }

    #[test]
    fn an_error_reply_still_decodes_as_a_reply_frame() {
        let err = Reply {
            id: 2,
            result: Err(RpcError {
                code: RpcErrorCode::DeadlineExceeded,
                message: "request deadline of 5000 ms expired".to_string(),
            }),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerFrame>(&json).unwrap(),
            ServerFrame::Reply(err)
        );
    }
}
```

`protocol/request.rs` test mod — one added test pinning the new code's wire string and the v1 fixtures still passing:

```rust
    #[test]
    fn deadline_exceeded_code_serializes_snake_case() {
        // Additive variant (evolution rule): the existing codes keep their
        // strings, so v1 byte fixtures above still deserialize unchanged.
        assert_eq!(
            serde_json::to_string(&RpcErrorCode::DeadlineExceeded).unwrap(),
            "\"deadline_exceeded\""
        );
        assert_eq!(
            serde_json::from_str::<RpcErrorCode>("\"deadline_exceeded\"").unwrap(),
            RpcErrorCode::DeadlineExceeded
        );
    }
```

- [ ] **Step 2:** `cargo test -p shep-core` → RED (module missing, variant missing).
- [ ] **Step 3: Implement.** `frame.rs` as above; append the variant; wire the re-exports; extend the `PROTOCOL_VERSION` doc. Note in the `frame.rs` module doc that untagged deserialization needs `serde/std`, which arrives via `serde_json`'s `std` feature (already enabled workspace-wide) — no new feature.
- [ ] **Step 4:** focused (`cargo test -p shep-core`) + full four-gate chain.
- [ ] **Step 5:** commit `feat(core): ServerFrame decode helper + deadline_exceeded error code` + footer.

---

### Task 2: Daemon deps + the event bus

**Files:** Modify root `Cargo.toml`, `crates/shep-daemon/Cargo.toml`, `crates/shep-daemon/src/lib.rs`; Create `crates/shep-daemon/src/bus.rs`.

**Root Cargo.toml** — one edit only: nix gains the `user` feature (verified against nix 0.29: `geteuid`, `User::from_name`, `Group::from_name` all live behind `feature = "user"`; `nix::fcntl::fcntl` needs no feature at all in 0.29):

```toml
# Only shep-daemon: process-group signal delivery and process-related syscalls for the
# real ProcessRunner (kill ladder, SIGKILL tree); `user` adds geteuid for the same-uid
# peer-cred check and passwd/group lookups for `user`/`group` spawn.
nix = { version = "0.29", default-features = false, features = ["signal", "process", "user"] }
```

**crates/shep-daemon/Cargo.toml (exact):**

```toml
[dependencies]
shep-core.workspace = true
tokio = { workspace = true, features = ["rt", "process", "io-util", "time", "sync", "net", "macros", "signal"] }
# The client<->daemon framing: the SAME codec shep-core exposes, so the daemon can
# never drift from the shared frame parameters.
tokio-util.workspace = true
# SinkExt/StreamExt on the framed halves of a connection.
futures-util.workspace = true
# encode_frame hands back Bytes; the per-connection write queue carries them as-is.
bytes.workspace = true
# Server-side topic filtering for Subscribe (spec §6 glob topics).
globset.workspace = true
# Atomic muster-roll writes: NamedTempFile in the same dir + persist = rename(2).
tempfile.workspace = true
tracing.workspace = true
serde.workspace = true
serde_json.workspace = true

[target.'cfg(unix)'.dependencies]
nix.workspace = true
command-fds.workspace = true
```

(`tempfile` moves out of `[dev-dependencies]` — keeping it in both is a duplicate-key error. `globset` is already a workspace dep and gains its first consumer here.)

**Interfaces:**

```rust
/// Ring capacity of the daemon event bus.
///
/// Every subscriber reads this one ring at its own cursor, so the number is
/// the per-subscriber backlog: a subscriber more than `BUS_CAPACITY` events
/// behind loses the OLDEST ones and is told how many (spec §6's drop-oldest
/// + `Dropped` notice). 1024 events is ~1 MiB at a 1 KiB log line — enough
/// that a client stalled for a second on a chatty sheep catches up, small
/// enough to leave the single-digit-MB idle footprint goal alone (spec §14.11).
pub const BUS_CAPACITY: usize = 1024;

/// Ceiling on topic patterns per `Subscribe`.
///
/// Patterns are peer-supplied and each compiles into the connection's
/// matcher; bounding the count bounds that work the same way the selector's
/// regex size limit bounds a compiled pattern.
pub const MAX_TOPIC_PATTERNS: usize = 32;

#[must_use]
pub fn new_bus() -> tokio::sync::broadcast::Sender<BusEvent>;

/// Compiled server-side topic filter for one subscription
#[derive(Debug)]
pub struct TopicFilter { /* GlobSet + the source patterns */ }

impl TopicFilter {
    /// # Errors
    /// - [`BusError::TooManyPatterns`] — more than [`MAX_TOPIC_PATTERNS`].
    /// - [`BusError::BadPattern`] — a pattern the glob compiler rejects.
    pub fn new(patterns: &[String]) -> Result<Self, BusError>;
    #[must_use] pub fn matches(&self, event: &BusEvent) -> bool;
    #[must_use] pub fn patterns(&self) -> &[String];
}

/// Error type returned from [`TopicFilter::new`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// A subscribe pattern failed to compile (carries pattern + compiler message)
    BadPattern { pattern: String, message: String },
    /// More than [`MAX_TOPIC_PATTERNS`] patterns in one subscribe (carries the count)
    TooManyPatterns(usize),
}

/// Spawns the forward task for one subscriber
pub fn spawn_forwarder(
    rx: tokio::sync::broadcast::Receiver<BusEvent>,
    filter: TopicFilter,
    out: tokio::sync::mpsc::Sender<bytes::Bytes>,
) -> tokio::task::JoinHandle<()>;
```

**Implementation (complete — the loop is four lines because the decision is pure):**

```rust
#[must_use]
pub fn new_bus() -> broadcast::Sender<BusEvent> {
    broadcast::channel(BUS_CAPACITY).0
}

impl TopicFilter {
    pub fn new(patterns: &[String]) -> Result<Self, BusError> {
        if patterns.len() > MAX_TOPIC_PATTERNS {
            return Err(BusError::TooManyPatterns(patterns.len()));
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|e| BusError::BadPattern {
                pattern: pattern.clone(),
                message: e.to_string(),
            })?;
            builder.add(glob);
        }
        let set = builder.build().map_err(|e| BusError::BadPattern {
            pattern: patterns.join(", "),
            message: e.to_string(),
        })?;
        Ok(Self { set, patterns: patterns.to_vec() })
    }

    #[must_use]
    pub fn matches(&self, event: &BusEvent) -> bool {
        self.set.is_match(event.topic())
    }
}

/// What the forwarder should do with one receive result
enum Forwarded {
    Frame(Bytes),
    Skip,
    Stop,
}

// Pure: every forwarding decision lives here so the task itself has nothing
// left to test but plumbing.
fn step(received: Result<BusEvent, RecvError>, filter: &TopicFilter) -> Forwarded {
    let event = match received {
        Ok(event) if filter.matches(&event) => event,
        Ok(_) => return Forwarded::Skip,
        // Drop notices BYPASS the filter on purpose: a subscriber to
        // `process.*` still has to learn it lost events, and `daemon.dropped`
        // would otherwise be filtered out exactly when it matters most.
        Err(RecvError::Lagged(count)) => BusEvent::Dropped { count },
        Err(RecvError::Closed) => return Forwarded::Stop,
    };
    match encode_frame(&event) {
        Ok(bytes) => Forwarded::Frame(bytes),
        Err(err) => {
            tracing::warn!(%err, topic = event.topic(), "dropping an unencodable bus event");
            Forwarded::Skip
        }
    }
}

pub fn spawn_forwarder(
    mut rx: broadcast::Receiver<BusEvent>,
    filter: TopicFilter,
    out: mpsc::Sender<Bytes>,
) -> JoinHandle<()> {
    // Cancel-safety: `recv` and `send` are both cancel-safe and are awaited
    // sequentially (no select!), so an aborted forwarder can lose at most the
    // frame in flight — which the subscriber is no longer there to read.
    tokio::spawn(async move {
        loop {
            match step(rx.recv().await, &filter) {
                Forwarded::Frame(bytes) => {
                    if out.send(bytes).await.is_err() {
                        break; // subscriber hung up
                    }
                }
                Forwarded::Skip => {}
                Forwarded::Stop => break,
            }
        }
    })
}
```

**Back-pressure model (write this as a `//` block comment above `spawn_forwarder`, IR-31):** a client that stops reading stalls the connection's writer, which fills the write queue, which parks this task on `send`, which stops draining the ring — so the *broadcast channel itself* becomes the bounded per-subscriber queue, drops the oldest events, and reports the exact count as `Lagged(n)`. That is spec §6's requirement implemented by the runtime rather than by a hand-rolled `VecDeque`, and it isolates one slow client from every other connection.

- [ ] **Step 1: Failing tests** in `bus.rs` (full code; channels only, so the paused clock is safe here):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo, decode_frame};
    use shep_core::status::ProcStatus;

    fn filter(patterns: &[&str]) -> TopicFilter {
        let owned: Vec<String> = patterns.iter().map(|p| (*p).to_string()).collect();
        TopicFilter::new(&owned).unwrap()
    }

    fn process_event(id: u32, event: ProcessEventKind) -> BusEvent {
        BusEvent::Process {
            event,
            info: ProcessInfo {
                id,
                name: format!("sheep-{id}"),
                status: ProcStatus::Online,
                pid: Some(1000 + id),
                restarts: 0,
                uptime_ms: 0,
                fold: None,
            },
            manually: false,
            at_ms: 0,
        }
    }

    #[test]
    fn globs_match_the_dotted_topic_grammar() {
        let processes = filter(&["process.*"]);
        assert!(processes.matches(&process_event(0, ProcessEventKind::Exit)));
        assert!(!processes.matches(&BusEvent::LogOut { id: 0, line: String::new() }));
        let logs = filter(&["log.out", "log.err"]);
        assert!(logs.matches(&BusEvent::LogOut { id: 0, line: String::new() }));
        assert!(logs.matches(&BusEvent::LogErr { id: 0, line: String::new() }));
        assert!(!logs.matches(&BusEvent::DaemonShutdown));
        let everything = filter(&["*"]);
        assert!(everything.matches(&BusEvent::DaemonShutdown));
        assert!(everything.matches(&process_event(1, ProcessEventKind::Start)));
    }

    #[test]
    fn an_empty_topic_list_matches_nothing() {
        // Documented contract: subscribe to `*` for everything; an empty list
        // is a subscription to nothing, not a wildcard.
        let none = TopicFilter::new(&[]).unwrap();
        assert!(!none.matches(&BusEvent::DaemonShutdown));
        assert!(none.patterns().is_empty());
    }

    #[test]
    fn a_bad_pattern_is_a_typed_error_carrying_the_pattern() {
        let err = TopicFilter::new(&["process.[".to_string()]).unwrap_err();
        assert!(matches!(err, BusError::BadPattern { ref pattern, .. } if pattern == "process.["));
    }

    #[test]
    fn too_many_patterns_are_refused_with_the_count() {
        let many: Vec<String> = (0..=MAX_TOPIC_PATTERNS).map(|i| format!("t{i}")).collect();
        assert_eq!(
            TopicFilter::new(&many).unwrap_err(),
            BusError::TooManyPatterns(MAX_TOPIC_PATTERNS + 1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn lag_becomes_a_dropped_notice_that_bypasses_the_filter() {
        // The count is READ from the runtime, never hand-computed: whatever
        // tokio says was missed is exactly what the subscriber is told.
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        for id in 0..10 {
            tx.send(process_event(id, ProcessEventKind::Start)).unwrap();
        }
        let missed = match rx.recv().await {
            Err(RecvError::Lagged(n)) => n,
            other => panic!("expected a lag after overflowing the ring, got {other:?}"),
        };
        assert!(missed > 0);
        // `process.*` would filter a daemon.dropped topic out; it must not.
        let Forwarded::Frame(bytes) = step(Err(RecvError::Lagged(missed)), &filter(&["process.*"]))
        else {
            panic!("a lag must always produce a Dropped frame")
        };
        assert_eq!(
            decode_frame::<BusEvent>(&bytes).unwrap(),
            BusEvent::Dropped { count: missed }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn forwarder_delivers_only_matching_frames_then_closes_with_the_bus() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_forwarder(rx, filter(&["process.*"]), out_tx);

        tx.send(process_event(0, ProcessEventKind::Start)).unwrap();
        tx.send(BusEvent::LogOut { id: 0, line: "noise".to_string() }).unwrap();
        tx.send(process_event(0, ProcessEventKind::Online)).unwrap();
        drop(tx);
        handle.await.unwrap();

        // Ordering IS the filtering assertion: the two process frames arrive
        // back to back, so nothing was emitted for the log line between them.
        let first: BusEvent = decode_frame(&out_rx.recv().await.unwrap()).unwrap();
        let second: BusEvent = decode_frame(&out_rx.recv().await.unwrap()).unwrap();
        assert!(matches!(first, BusEvent::Process { event: ProcessEventKind::Start, .. }));
        assert!(matches!(second, BusEvent::Process { event: ProcessEventKind::Online, .. }));
        assert!(out_rx.recv().await.is_none(), "forwarder must drop its sender at bus close");
    }

    #[tokio::test(start_paused = true)]
    async fn forwarder_stops_when_the_subscriber_hangs_up() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(1);
        let handle = spawn_forwarder(rx, filter(&["*"]), out_tx);
        drop(out_rx);
        tx.send(BusEvent::DaemonShutdown).unwrap();
        handle.await.unwrap(); // resolves rather than leaking a task
    }
}
```

- [ ] **Step 2:** RED (module + deps missing).
- [ ] **Step 3:** Implement the Cargo edits + `bus.rs`; add `pub mod bus;` to lib.rs.
- [ ] **Step 4: minimal-versions rehearsal for the new deps** (the CI job is red on bare floors otherwise), then the four gates:

```bash
cargo +nightly generate-lockfile -Z minimal-versions && cargo +stable check --workspace --all-targets; git checkout Cargo.lock
```

Raise any floor the rehearsal demands (candidates: `globset`, `tempfile` — `NamedTempFile::new_in`/`persist` must exist at the floor; `futures-util`, `bytes`) in the ROOT Cargo.toml with a comment naming the API that forced it, and re-rehearse until green.

- [ ] **Step 5:** commit `feat(daemon): event bus — topic globs, forwarder task, drop-oldest notices` + footer.

---

### Task 3: The muster roll — registry, snapshot, atomic write, restore

**Files:** Create `crates/shep-daemon/src/snapshot.rs`; Modify `lib.rs` (module + move 2a's `now_ms()` helper up).

**Refactor note (small, deliberate — two moves into `lib.rs`):**

1. 2a isolated its single wall-clock read in a `now_ms()` helper inside `supervisor.rs`. Move it verbatim to `lib.rs` as `pub(crate) fn now_ms() -> u64` and re-point `supervisor.rs` at it — the roll needs the same read and there must be exactly one (DRY, and one place to fake later).
2. IR-33 wants ONE crate-root fixture module. Create `#[cfg(test)] mod testing` in `lib.rs` and move 2a's `test_paths()` into it (with its WHY comment), then re-point `supervisor.rs`'s tests at `crate::testing::test_paths`. **Every test mod from Task 3 onward uses that one helper** — Tasks 3–8 and the harness in Tasks 4–5 all assume it:

```rust
#[cfg(test)]
pub(crate) mod testing {
    use shep_core::paths::ShepPaths;

    // WHY a shallow home: later tasks bind a UDS under `run/`, and sun_path
    // caps a socket path near 104 bytes. Using the tempdir root as
    // $SHEP_HOME (no extra nesting) keeps every test in this crate under the
    // limit on macOS, whose temp paths are already long.
    pub(crate) fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
        let home = dir.path().to_path_buf();
        ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
            std::path::Path::new("/nonexistent"),
        )
    }
}
```

**Interfaces:**

```rust
/// Schema version of `flock.json`
pub const SNAPSHOT_VERSION: u32 = 1;

/// How long the writer lets a burst of lifecycle events settle before it
/// rewrites the roll.
///
/// One restart emits Exit + Restart + Start + Online within microseconds;
/// 250 ms folds a whole restart storm into a single atomic write while still
/// landing the roll orders of magnitude faster than the reboot it protects
/// against (spec §13.4).
pub const SNAPSHOT_DEBOUNCE_MS: u64 = 250;

/// The muster roll: which apps were registered, and how many were up
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlockSnapshot {
    pub version: u32,
    pub saved_at_ms: u64,
    pub apps: Vec<SavedApp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedApp {
    pub app: AppConfig,
    pub instances_running: u32,
}

/// The daemon's record of the config each registered sheep was started from
///
/// The supervisor owns runtime state; nothing in a [`ProcessInfo`] can
/// reproduce the `AppConfig` a sheep came from, which is exactly what a roll
/// needs. Cheap to clone (one `Arc`).
#[derive(Debug, Clone, Default)]
pub struct FlockRegistry { /* Arc<Mutex<BTreeMap<String, AppConfig>>> */ }

impl FlockRegistry {
    #[must_use] pub fn new() -> Self;
    pub fn record(&self, apps: &[ResolvedApp]);
    /// Builds the roll from the live listing, pruning names the flock no
    /// longer has (a deleted sheep must not resurrect).
    #[must_use] pub fn roll(&self, infos: &[ProcessInfo], now_ms: u64) -> FlockSnapshot;
}

/// # Errors
/// - [`SnapshotError::NoParent`] — the roll path has no directory to write into.
/// - [`SnapshotError::Encode`] — the roll failed to serialize.
/// - [`SnapshotError::Io`] — the temp file, fsync, or rename failed.
pub fn write_atomic(path: &Path, snapshot: &FlockSnapshot) -> Result<(), SnapshotError>;

/// # Errors
/// - [`SnapshotError::Io`] — the roll could not be read.
/// - [`SnapshotError::Parse`] — invalid JSON, or a schema version this
///   daemon does not know.
pub fn read(path: &Path) -> Result<FlockSnapshot, SnapshotError>;

/// Apps a `muster` should start, plus the ones the roll can no longer justify
#[derive(Debug)]
pub struct Restorable {
    pub apps: Vec<ResolvedApp>,
    pub rejected: Vec<(String, NormalizeError)>,
}

#[must_use] pub fn restorable(snapshot: FlockSnapshot) -> Restorable;

/// Handle to the debounced writer task
#[derive(Debug)]
pub struct SnapshotWriter { /* JoinHandle + Arc<AtomicU64> */ }

impl SnapshotWriter {
    /// Completed roll writes since boot — the number the metrics dog reports
    #[must_use] pub fn writes(&self) -> u64;
    /// Stops the writer and waits for it (the caller then owns roll timing)
    pub async fn stop(self);
}

pub fn spawn_snapshot_writer(
    path: PathBuf,
    supervisor: SupervisorHandle,
    registry: FlockRegistry,
    events: broadcast::Receiver<BusEvent>,
) -> SnapshotWriter;
```

**Implementation notes (the load-bearing three):**

1. **Atomic + owner-only.** `NamedTempFile::new_in(parent)` (same filesystem — `rename(2)` is only atomic within one) creates mode 0600 and `persist` keeps it. That mode is not cosmetic: the roll stores app `env` verbatim so a restore can reproduce it, which is the one place shep writes secrets to disk (spec §10 redacts them everywhere else). Write → `sync_all()` → `persist`.
2. **Restore re-validates.** The roll is a file a human can edit, so `restorable` runs every entry back through `normalize` exactly like peer input (spec §6's "the daemon MUST re-normalize" rule), collecting failures instead of aborting the whole muster.
3. **Restore rule (assumption, documented in the fn doc):** restore an app iff `instances_running > 0 && app.autostart`. "Was up when we saved" is the muster contract; `autostart = false` is the user's explicit opt-out of being brought back automatically.

```rust
fn is_running(status: ProcStatus) -> bool {
    matches!(status, ProcStatus::Online | ProcStatus::Starting | ProcStatus::WaitingRestart)
}

impl FlockRegistry {
    pub fn roll(&self, infos: &[ProcessInfo], now_ms: u64) -> FlockSnapshot {
        // A poisoned lock recovers instead of panicking: the map is a plain
        // BTreeMap, so a panic elsewhere cannot leave it inconsistent, and
        // taking the daemon down over it would be the worse failure.
        let mut apps = self.apps.lock().unwrap_or_else(PoisonError::into_inner);
        apps.retain(|name, _| infos.iter().any(|info| &info.name == name));
        let saved = apps
            .iter()
            .map(|(name, app)| SavedApp {
                app: app.clone(),
                instances_running: u32::try_from(
                    infos.iter().filter(|i| &i.name == name && is_running(i.status)).count(),
                )
                .unwrap_or(u32::MAX),
            })
            .collect();
        FlockSnapshot { version: SNAPSHOT_VERSION, saved_at_ms: now_ms, apps: saved }
    }
}
```

Writer loop (shape — cancel-safe by construction):

```rust
let mut deadline: Option<Instant> = None;
loop {
    tokio::select! {
        received = events.recv() => match received {
            // Only lifecycle events change the roll; log lines must not
            // rewrite a file once per output line.
            Ok(event) => if is_state_change(&event) && deadline.is_none() {
                deadline = Some(Instant::now() + Duration::from_millis(SNAPSHOT_DEBOUNCE_MS));
            },
            // A lag may have swallowed a lifecycle event: assume dirty.
            Err(RecvError::Lagged(_)) => if deadline.is_none() {
                deadline = Some(Instant::now() + Duration::from_millis(SNAPSHOT_DEBOUNCE_MS));
            },
            Err(RecvError::Closed) => break,
        },
        // Recomputed from the STORED deadline every iteration, so losing the
        // select! race never extends the debounce window (same cancel-safety
        // discipline as FakeProc's spawn-time exit deadline).
        () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
            deadline = None;
            write_now(&path, &supervisor, &registry, &writes).await;
        }
    }
}

async fn write_now(path: &Path, supervisor: &SupervisorHandle, registry: &FlockRegistry, writes: &AtomicU64) {
    // Engine gone: there is nothing left to record and the shutdown path has
    // already written the final roll.
    let Ok(infos) = supervisor.list_checked().await else { return };
    let roll = registry.roll(&infos, crate::now_ms()); // lock released before any IO
    match write_atomic(path, &roll) {
        Ok(()) => { writes.fetch_add(1, Ordering::SeqCst); }
        Err(err) => tracing::warn!(%err, "muster roll write failed"),
    }
}
```

(The write is a few KiB to a local file once per debounce window; `spawn_blocking` would buy a task hop and nothing else — say so in a `//` comment. No lock is held across an `await`; `clippy::await_holding_lock` enforces it.)

- [ ] **Step 1: Failing tests** in `snapshot.rs` — pure tiers first, then the writer under a paused clock:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use shep_core::config::{AppConfig, normalize};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
    use shep_core::status::ProcStatus;
    use std::time::Duration;

    use crate::testing::test_paths; // the one crate-root fixture (IR-33)

    fn info(id: u32, name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo {
            id,
            name: name.to_string(),
            status,
            pid: Some(1000 + id),
            restarts: 0,
            uptime_ms: 0,
            fold: None,
        }
    }

    #[test]
    fn roll_counts_running_instances_and_prunes_deleted_names() {
        let registry = FlockRegistry::new();
        let web = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        let job = normalize(AppConfig::minimal("job", "./job")).unwrap();
        registry.record(&[web, job]);

        let infos = [
            info(0, "web", ProcStatus::Online),
            info(1, "web", ProcStatus::WaitingRestart),
            info(2, "web", ProcStatus::Stopped),
        ]; // `job` was deleted: no entries left
        let roll = registry.roll(&infos, 1_700_000_000_000);

        assert_eq!(roll.version, SNAPSHOT_VERSION);
        assert_eq!(roll.saved_at_ms, 1_700_000_000_000);
        assert_eq!(roll.apps.len(), 1, "a name with no live entry is pruned");
        assert_eq!(roll.apps[0].app.name, "web");
        assert_eq!(roll.apps[0].instances_running, 2); // online + waiting-restart
        // The prune is sticky: a second roll must not resurrect `job`.
        assert_eq!(registry.roll(&infos, 0).apps.len(), 1);
    }

    #[test]
    fn write_atomic_round_trips_with_no_leftovers() {
        // Portable: snapshot.rs is in the 'compiles everywhere' tier (Global
        // Constraints' portability split). The 0600 owner-only guarantee is
        // unix-only and lives in write_atomic_is_owner_only_on_unix below —
        // Windows has no equivalent permission bit to assert here.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.json");
        let registry = FlockRegistry::new();
        registry.record(&[normalize(AppConfig::minimal("web", "./srv")).unwrap()]);
        let roll = registry.roll(&[info(0, "web", ProcStatus::Online)], 42);

        write_atomic(&path, &roll).unwrap();
        write_atomic(&path, &roll).unwrap(); // overwriting keeps the guarantees

        assert_eq!(read(&path).unwrap(), roll);
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "no temp file may survive a completed write");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_is_owner_only_on_unix() {
        // The roll stores app env verbatim (spec §10): owner-only, always.
        // This is the ONE unix-gated test in an otherwise-portable file
        // (Global Constraints' portability split exception) — 0600 is a
        // unix permission-bit guarantee with no Windows ACL equivalent.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.json");
        let registry = FlockRegistry::new();
        registry.record(&[normalize(AppConfig::minimal("web", "./srv")).unwrap()]);
        let roll = registry.roll(&[info(0, "web", ProcStatus::Online)], 42);

        write_atomic(&path, &roll).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the muster roll holds app env in cleartext");
    }

    #[test]
    fn read_rejects_corrupt_json_and_unknown_schema_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(matches!(read(&path), Err(SnapshotError::Parse { .. })));

        let future = format!(
            "{{\"version\":{},\"saved_at_ms\":0,\"apps\":[]}}",
            SNAPSHOT_VERSION + 1
        );
        std::fs::write(&path, future.as_bytes()).unwrap();
        assert!(matches!(read(&path), Err(SnapshotError::Parse { .. })));
    }

    #[test]
    fn restorable_takes_running_autostart_apps_only() {
        let mut stopped = AppConfig::minimal("stopped", "./s");
        stopped.instances = 1;
        let mut opted_out = AppConfig::minimal("manual", "./m");
        opted_out.autostart = false;

        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![
                SavedApp { app: AppConfig::minimal("web", "./srv"), instances_running: 2 },
                SavedApp { app: stopped, instances_running: 0 },
                SavedApp { app: opted_out, instances_running: 1 },
            ],
        };
        let restorable = restorable(roll);
        assert_eq!(restorable.apps.len(), 1);
        assert_eq!(restorable.apps[0].config().name, "web");
        assert!(restorable.rejected.is_empty());
    }

    #[test]
    fn restorable_reports_a_hand_edited_invalid_app_instead_of_aborting() {
        let mut broken = AppConfig::minimal("broken", "./b");
        broken.instances = 0; // someone edited the roll
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![
                SavedApp { app: broken, instances_running: 1 },
                SavedApp { app: AppConfig::minimal("web", "./srv"), instances_running: 1 },
            ],
        };
        let restorable = restorable(roll);
        assert_eq!(restorable.apps.len(), 1, "one bad entry must not sink the muster");
        assert_eq!(
            restorable.rejected,
            vec![("broken".to_string(), shep_core::config::NormalizeError::ZeroInstances)]
        );
    }

    #[test]
    fn debug_does_not_leak_env_values() {
        // IR-41: the roll carries env; its Debug lands in daemon logs.
        let mut app = AppConfig::minimal("web", "./srv");
        app.env.insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp { app, instances_running: 1 }],
        };
        let rendered = format!("{roll:?}");
        assert!(!rendered.contains("postgres://secret"), "{rendered}");
        assert!(rendered.contains("<1 vars>"), "{rendered}");
    }

    #[tokio::test(start_paused = true)]
    async fn writer_coalesces_a_burst_into_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.home).unwrap();
        let (events, _keep) = tokio::sync::broadcast::channel(64);
        let supervisor = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events.clone(),
        );
        let registry = FlockRegistry::new();
        let app = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        registry.record(std::slice::from_ref(&app));
        supervisor.start(vec![app]).await.unwrap();

        // Subscribing here means the start's own events are already behind us.
        let writer = spawn_snapshot_writer(
            paths.snapshot.clone(),
            supervisor.clone(),
            registry,
            events.subscribe(),
        );
        for event in [ProcessEventKind::Exit, ProcessEventKind::Restart, ProcessEventKind::Online] {
            events.send(BusEvent::Process {
                event,
                info: info(0, "web", ProcStatus::Online),
                manually: false,
                at_ms: 0,
            })
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(SNAPSHOT_DEBOUNCE_MS + 1)).await;

        assert_eq!(writer.writes(), 1, "one debounce window is one write");
        let roll = read(&paths.snapshot).unwrap();
        assert_eq!(roll.apps.len(), 1);
        assert_eq!(roll.apps[0].instances_running, 1);
        writer.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn writer_ignores_log_traffic() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.home).unwrap();
        let (events, _keep) = tokio::sync::broadcast::channel(64);
        let supervisor = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events.clone(),
        );
        let writer = spawn_snapshot_writer(
            paths.snapshot.clone(),
            supervisor,
            FlockRegistry::new(),
            events.subscribe(),
        );
        for id in 0..50 {
            events.send(BusEvent::LogOut { id, line: "chatty".to_string() }).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(SNAPSHOT_DEBOUNCE_MS * 4)).await;
        assert_eq!(writer.writes(), 0, "log lines must never rewrite the roll");
        assert!(!paths.snapshot.exists());
        writer.stop().await;
    }
}
```

- [ ] **Step 2:** RED. **Step 3:** implement `snapshot.rs` + the `now_ms()` move. **Step 4:** focused + four gates. **Step 5:** commit `feat(daemon): muster roll — registry, atomic writes, debounced writer, restore` + footer.

---

### Task 4: RPC dispatch (portable)

**Files:** Create `crates/shep-daemon/src/rpc.rs`; Modify `lib.rs`.

**Interfaces:**

```rust
/// Deadline applied when a client sends none (spec §6: 5s default)
pub const DEFAULT_DEADLINE_MS: u64 = 5_000;
/// Ceiling on a client-supplied deadline — a peer cannot pin a daemon task open
pub const MAX_DEADLINE_MS: u64 = 60_000;

/// Everything a request handler may touch — one clone per connection
#[derive(Clone, Debug)]
pub struct RpcContext {
    pub supervisor: SupervisorHandle,
    pub events: broadcast::Sender<BusEvent>,
    pub registry: FlockRegistry,
    pub snapshot_path: PathBuf,
    pub daemon_version: String,
    pub pid: u32,
    pub shutdown: Arc<watch::Sender<bool>>,
}

impl RpcContext {
    /// Asks the daemon to begin graceful shutdown
    pub fn shutdown(&self);
    /// Writes the muster roll now — the primitive Phase 3's `muster save` calls
    ///
    /// # Errors
    /// - [`SnapshotError`] — as [`write_atomic`](crate::snapshot::write_atomic).
    pub async fn snapshot_now(&self) -> Result<(), SnapshotError>;
}

/// What the connection layer must do with a dispatched request
#[derive(Debug)]
pub enum Outcome {
    /// Send this reply and keep reading
    Reply(Reply),
    /// Send this reply, then start forwarding events through `filter`
    Subscribe { reply: Reply, filter: TopicFilter },
    /// Send this reply, then trigger daemon shutdown and close
    Shutdown(Reply),
}

pub async fn dispatch(envelope: Envelope, ctx: &RpcContext) -> Outcome;

/// The deadline this envelope gets: its own, clamped, or the default
#[must_use] pub fn budget(deadline_ms: Option<u64>) -> Duration;
```

**Implementation (complete for the decision core):**

```rust
#[must_use]
pub fn budget(deadline_ms: Option<u64>) -> Duration {
    // clamp's lower bound is 1ms so a literal `0` means "expire immediately"
    // rather than silently becoming "no deadline at all".
    Duration::from_millis(deadline_ms.unwrap_or(DEFAULT_DEADLINE_MS).clamp(1, MAX_DEADLINE_MS))
}

pub async fn dispatch(envelope: Envelope, ctx: &RpcContext) -> Outcome {
    let id = envelope.id;
    with_deadline(id, budget(envelope.deadline_ms), run(id, envelope.body, ctx)).await
}

// `+ Send`: this future is awaited inside the per-connection tokio::spawn, so
// the bound is stated rather than inferred (Global Constraints).
async fn with_deadline<F: Future<Output = Outcome> + Send>(
    id: u64,
    budget: Duration,
    work: F,
) -> Outcome {
    match tokio::time::timeout(budget, work).await {
        Ok(outcome) => outcome,
        Err(_) => Outcome::Reply(Reply {
            id,
            result: Err(RpcError {
                code: RpcErrorCode::DeadlineExceeded,
                message: format!(
                    "the request deadline of {} ms expired before the daemon finished",
                    budget.as_millis()
                ),
            }),
        }),
    }
}

async fn run(id: u64, request: Request, ctx: &RpcContext) -> Outcome {
    let reply = |result| Outcome::Reply(Reply { id, result });
    match request {
        Request::Ping => reply(Ok(Response::Pong)),
        Request::ListFlock => match ctx.supervisor.list_checked().await {
            Ok(infos) => reply(Ok(Response::Flock(infos))),
            Err(err) => reply(Err(rpc_error(&err))),
        },
        Request::Describe { selector } => match selector_of(selector) {
            Err(err) => reply(Err(err)),
            Ok(selector) => match ctx.supervisor.list_checked().await {
                Err(err) => reply(Err(rpc_error(&err))),
                Ok(infos) => {
                    let hits: Vec<_> = infos
                        .into_iter()
                        .filter(|i| selector.matches(&i.name, i.id, i.fold.as_deref()))
                        .collect();
                    if hits.is_empty() {
                        reply(Err(not_found()))
                    } else {
                        reply(Ok(Response::Described(hits)))
                    }
                }
            },
        },
        // Peer input is untrusted: re-normalize before anything is registered.
        Request::Start { apps } => match normalize_all(apps) {
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
            })),
            Ok(resolved) => {
                ctx.registry.record(&resolved);
                match ctx.supervisor.start(resolved).await {
                    Ok(infos) => reply(Ok(Response::Started(infos))),
                    Err(err) => reply(Err(rpc_error(&err))),
                }
            }
        },
        Request::Stop { selector } => selector_call(id, selector, |s| ctx.supervisor.stop(s), Response::Stopped).await,
        Request::Restart { selector } => selector_call(id, selector, |s| ctx.supervisor.restart(s), Response::Restarted).await,
        Request::Delete { selector } => match selector_of(selector) {
            Err(err) => reply(Err(err)),
            Ok(selector) => match ctx.supervisor.delete(selector).await {
                Ok(ids) => reply(Ok(Response::Deleted(ids))),
                Err(err) => reply(Err(rpc_error(&err))),
            },
        },
        Request::Subscribe { topics } => match TopicFilter::new(&topics) {
            Ok(filter) => Outcome::Subscribe {
                reply: Reply { id, result: Ok(Response::Subscribed) },
                filter,
            },
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
            })),
        },
        Request::KillDaemon => Outcome::Shutdown(Reply { id, result: Ok(Response::ShuttingDown) }),
        // `Request` is #[non_exhaustive]: a verb from a newer client that this
        // daemon has never heard of is an error, not a panic.
        _ => reply(Err(RpcError {
            code: RpcErrorCode::Internal,
            message: "this daemon does not implement that request".to_string(),
        })),
    }
}

fn rpc_error(err: &SupervisorError) -> RpcError {
    match err {
        SupervisorError::NotFound => not_found(),
        SupervisorError::SpawnFailed(msg) => RpcError {
            code: RpcErrorCode::SpawnFailed,
            message: msg.clone(),
        },
        SupervisorError::EngineStopped => RpcError {
            code: RpcErrorCode::Internal,
            message: "the supervisor engine has stopped".to_string(),
        },
    }
}

fn selector_of(spec: SelectorSpec) -> Result<ProcessSelector, RpcError> {
    ProcessSelector::try_from(spec).map_err(|err| RpcError {
        code: RpcErrorCode::InvalidConfig,
        message: err.to_string(),
    })
}
```

`selector_call` is the helper Stop and Restart share — convert the selector, call the supervisor, map the hits through the passed `Response` constructor. Its future bound is stated, not inferred, because the whole chain is awaited inside the per-connection `tokio::spawn`:

```rust
async fn selector_call<F, Fut>(
    id: u64,
    spec: SelectorSpec,
    call: F,
    ok: fn(Vec<ProcessInfo>) -> Response,
) -> Outcome
where
    F: FnOnce(ProcessSelector) -> Fut + Send,
    Fut: Future<Output = Result<Vec<ProcessInfo>, SupervisorError>> + Send,
{
    let result = match selector_of(spec) {
        Ok(selector) => call(selector).await.map(ok).map_err(|err| rpc_error(&err)),
        Err(err) => Err(err),
    };
    Outcome::Reply(Reply { id, result })
}
```

(If 2a marked `SupervisorError` `#[non_exhaustive]`, `rpc_error` needs a wildcard arm mapping to `Internal` — check before writing.)

**Deadline semantics, stated honestly in `dispatch`'s doc:** the deadline bounds *the reply*, not the actor. Dropping the work future stops the daemon waiting on the supervisor; the command that was already handed to the actor still runs to completion. So a `DeadlineExceeded` on `Start` means "no answer within your budget", not "nothing happened" — a client that retries must reconcile with `ListFlock`. Anything stronger would need per-command cancellation inside the actor, which 2a's locked `Command` surface deliberately does not have.

- [ ] **Step 1: Failing tests** in `rpc.rs` (paused clock, ScriptedRunner, no sockets):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use shep_core::config::AppConfig;
    use shep_core::protocol::{Envelope, Request, Response, RpcErrorCode, SelectorSpec};
    use shep_core::status::ProcStatus;

    struct Harness {
        ctx: RpcContext,
        _dir: tempfile::TempDir,
        _events_rx: tokio::sync::broadcast::Receiver<shep_core::protocol::BusEvent>,
        shutdown_rx: tokio::sync::watch::Receiver<bool>,
    }

    fn harness(scripts: Vec<ProcScript>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::testing::test_paths(&dir);
        let (events, events_rx) = tokio::sync::broadcast::channel(256);
        let supervisor = spawn_supervisor(ScriptedRunner::new(scripts), paths.clone(), events.clone());
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        Harness {
            ctx: RpcContext {
                supervisor,
                events,
                registry: crate::snapshot::FlockRegistry::new(),
                snapshot_path: paths.snapshot.clone(),
                daemon_version: "0.1.0".to_string(),
                pid: 4242,
                shutdown: std::sync::Arc::new(shutdown),
            },
            _dir: dir,
            _events_rx: events_rx,
            shutdown_rx,
        }
    }

    fn envelope(id: u64, body: Request) -> Envelope {
        Envelope { id, deadline_ms: None, body }
    }

    fn reply_of(outcome: Outcome) -> shep_core::protocol::Reply {
        match outcome {
            Outcome::Reply(reply) | Outcome::Subscribe { reply, .. } | Outcome::Shutdown(reply) => reply,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ping_answers_pong_on_the_same_envelope_id() {
        let h = harness(vec![]);
        let reply = reply_of(dispatch(envelope(9, Request::Ping), &h.ctx).await);
        assert_eq!(reply.id, 9);
        assert_eq!(reply.result.unwrap(), Response::Pong);
    }

    #[tokio::test(start_paused = true)]
    async fn start_registers_the_config_and_lists_it() {
        let h = harness(vec![ProcScript::never_exits()]);
        let started = reply_of(
            dispatch(
                envelope(1, Request::Start { apps: vec![AppConfig::minimal("web", "./srv")] }),
                &h.ctx,
            )
            .await,
        );
        let Response::Started(infos) = started.result.unwrap() else { panic!("expected started") };
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].status, ProcStatus::Online);

        // The roll can only be built if Start recorded the config.
        let roll = h.ctx.registry.roll(&infos, 0);
        assert_eq!(roll.apps.len(), 1);
        assert_eq!(roll.apps[0].app.script, "./srv");

        let listed = reply_of(dispatch(envelope(2, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else { panic!("expected flock") };
        assert_eq!(flock.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn start_re_normalizes_untrusted_peer_config() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(1, Request::Start { apps: vec![AppConfig::minimal("", "./srv")] }),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    #[tokio::test(start_paused = true)]
    async fn a_selector_matching_nothing_is_not_found() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(1, Request::Stop { selector: SelectorSpec::Name("ghost".to_string()) }),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::NotFound);
    }

    #[tokio::test(start_paused = true)]
    async fn a_bad_peer_regex_is_invalid_config_not_a_panic() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(1, Request::Describe { selector: SelectorSpec::Regex("((".to_string()) }),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    #[tokio::test(start_paused = true)]
    async fn describe_filters_by_fold() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let mut api = AppConfig::minimal("api", "./a");
        api.fold = Some("backend".to_string());
        dispatch(
            envelope(1, Request::Start { apps: vec![api, AppConfig::minimal("web", "./w")] }),
            &h.ctx,
        )
        .await;
        let reply = reply_of(
            dispatch(
                envelope(2, Request::Describe { selector: SelectorSpec::Fold("backend".to_string()) }),
                &h.ctx,
            )
            .await,
        );
        let Response::Described(hits) = reply.result.unwrap() else { panic!("expected described") };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "api");
    }

    #[tokio::test(start_paused = true)]
    async fn subscribe_hands_back_a_compiled_filter() {
        let h = harness(vec![]);
        let outcome = dispatch(
            envelope(1, Request::Subscribe { topics: vec!["process.*".to_string()] }),
            &h.ctx,
        )
        .await;
        let Outcome::Subscribe { reply, filter } = outcome else { panic!("expected subscribe") };
        assert_eq!(reply.result.unwrap(), Response::Subscribed);
        assert_eq!(filter.patterns(), ["process.*"]);

        let bad = reply_of(
            dispatch(envelope(2, Request::Subscribe { topics: vec!["[".to_string()] }), &h.ctx).await,
        );
        assert_eq!(bad.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    #[tokio::test(start_paused = true)]
    async fn kill_daemon_asks_for_shutdown_without_taking_the_engine_down_itself() {
        let mut h = harness(vec![]);
        let Outcome::Shutdown(reply) = dispatch(envelope(1, Request::KillDaemon), &h.ctx).await
        else {
            panic!("expected a shutdown outcome")
        };
        assert_eq!(reply.result.unwrap(), Response::ShuttingDown);
        // Dispatch only reports the intent; the connection layer triggers it.
        assert!(!*h.shutdown_rx.borrow_and_update());
        h.ctx.shutdown();
        assert!(h.shutdown_rx.changed().await.is_ok());
        assert!(*h.shutdown_rx.borrow());
    }

    #[test]
    fn budgets_default_and_clamp() {
        assert_eq!(budget(None), Duration::from_millis(DEFAULT_DEADLINE_MS));
        assert_eq!(budget(Some(250)), Duration::from_millis(250));
        assert_eq!(budget(Some(0)), Duration::from_millis(1));
        assert_eq!(budget(Some(u64::MAX)), Duration::from_millis(MAX_DEADLINE_MS));
    }

    #[tokio::test(start_paused = true)]
    async fn work_past_its_deadline_answers_deadline_exceeded() {
        // Driven at the deadline seam with a future that never finishes: the
        // paused clock auto-advances the moment the test parks, so this is
        // instant and exact.
        let outcome = with_deadline(
            5,
            Duration::from_millis(250),
            std::future::pending::<Outcome>(),
        )
        .await;
        let reply = reply_of(outcome);
        assert_eq!(reply.id, 5);
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::DeadlineExceeded);
        assert!(err.message.contains("250 ms"), "{}", err.message);
    }
}
```

- [ ] **Step 2:** RED. **Step 3:** implement `rpc.rs`, and put `Harness`/`harness` in the crate-root `testing` module from Task 3 rather than in `rpc.rs`'s test mod — Task 5's server tests need exactly the same fixture, and IR-33 wants one factory, not two. **Step 4:** focused + four gates. **Step 5:** commit `feat(daemon): RPC dispatch — verb routing, typed errors, per-call deadlines` + footer.

---

### Task 5: The RPC server — auth, handshake, connection loop

**Files:** Create `crates/shep-daemon/src/server.rs`; Modify `lib.rs` (`#[cfg(unix)] pub mod server;`).

**Interfaces:**

```rust
/// Frames queued toward one client before the connection back-pressures
pub const CONN_QUEUE: usize = 64;
/// How long a connected peer has to send its `Hello`
pub const HANDSHAKE_TIMEOUT_MS: u64 = 5_000;

/// The control socket — shep's privilege boundary
///
/// # Security
///
/// (IR-29 canonical writeup; everything else links here.)
/// ...design criteria: `$SHEP_HOME/run` is 0700 so no other user can reach
/// the socket path at all; every accepted connection is checked with
/// `SO_PEERCRED`/`getpeereid` and refused unless the peer's uid equals the
/// daemon's; the handshake refuses protocol skew with a typed error rather
/// than silence; frames are capped at `MAX_FRAME_BYTES`; peer-supplied
/// selectors and topic globs are size-bounded before compiling; every call
/// carries a clamped deadline. Explicit non-goals: root can always read
/// daemon memory, and a peer with the same uid is fully trusted (it could
/// simply run the binary itself).
#[derive(Debug)]
pub struct RpcServer { /* listener + ctx */ }

impl RpcServer {
    #[must_use] pub fn new(listener: UnixListener, ctx: RpcContext) -> Self;
    /// Serves until `shutdown` flips or its sender drops
    pub async fn serve(self, shutdown: watch::Receiver<bool>);
}

/// Checks that a connected peer runs as the daemon's own user
///
/// # Errors
/// - [`AuthError::NoCredentials`] — the OS refused to report peer credentials.
/// - [`AuthError::ForeignUid`] — the peer's uid is not the daemon's.
pub fn check_peer(stream: &UnixStream, daemon_uid: u32) -> Result<u32, AuthError>;

/// The daemon's effective uid
#[must_use] pub fn daemon_uid() -> u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The OS would not report peer credentials on this socket (carries the message)
    NoCredentials(String),
    /// The peer runs as another user (carries both uids)
    ForeignUid { peer: u32, daemon: u32 },
}

/// Error type ending one connection
#[derive(Debug)]
pub enum ConnError {
    Auth(AuthError),
    Frame(std::io::Error),
    Decode(WireError),
    Encode(WireError),
    ProtocolMismatch { client: u32 },
    HandshakeTimeout,
    NoHandshake,
    PeerGone,
}
```

**Peer-credential decision (deviation, deliberate — record it as a `//` comment at `check_peer`):** the check uses `tokio::net::UnixStream::peer_cred()`, not `nix::sys::socket::getsockopt(PeerCredentials)`. nix 0.29 gates `PeerCredentials` behind `#[cfg(linux_android)]` — on macOS, a tier-1 platform (spec §11), that sockopt does not exist and the daemon would not compile. tokio's `UCred` already dispatches to `SO_PEERCRED` on Linux, `getpeereid` on macOS/BSD, and `LOCAL_PEERCRED`/`getpeerucred` elsewhere, needs no new dependency, and adds no unsafe. nix is still used for `geteuid()`, which has no such split.

**Connection shape (the ordering is load-bearing):**

```rust
async fn handle_conn(stream: UnixStream, ctx: RpcContext) -> Result<(), ConnError> {
    check_peer(&stream, daemon_uid()).map_err(ConnError::Auth)?;
    let (read_half, write_half) = stream.into_split();
    let mut frames = FramedRead::new(read_half, codec());
    let (out_tx, out_rx) = mpsc::channel::<Bytes>(CONN_QUEUE);
    let writer = tokio::spawn(write_loop(FramedWrite::new(write_half, codec()), out_rx));

    let outcome = converse(&mut frames, &out_tx, &ctx).await;

    // Drop the queue's sender and JOIN the writer before returning, on EVERY
    // path: a protocol-skew refusal is written by that task, so returning the
    // error early would close the socket before the client ever saw why.
    drop(out_tx);
    let _ = writer.await;
    outcome
}

async fn converse(frames: &mut Frames, out: &mpsc::Sender<Bytes>, ctx: &RpcContext) -> Result<(), ConnError> {
    handshake(frames, out, ctx).await?;
    let mut forwarder: Option<JoinHandle<()>> = None;
    while let Some(frame) = frames.next().await {
        let frame = frame.map_err(ConnError::Frame)?;   // oversize/short frame ends the connection
        let envelope: Envelope = decode_frame(&frame).map_err(ConnError::Decode)?;
        match dispatch(envelope, ctx).await {
            Outcome::Reply(reply) => send(out, &reply).await?,
            Outcome::Subscribe { reply, filter } => {
                send(out, &reply).await?; // ordered ahead of any event by the queue
                // A second Subscribe REPLACES the first: spec §6 gives a
                // connection one topic list, not a growing union.
                if let Some(old) = forwarder.replace(spawn_forwarder(ctx.events.subscribe(), filter, out.clone())) {
                    old.abort();
                }
            }
            Outcome::Shutdown(reply) => {
                send(out, &reply).await?;
                ctx.shutdown();
                break;
            }
        }
    }
    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }
    Ok(())
}

async fn handshake(frames: &mut Frames, out: &mpsc::Sender<Bytes>, ctx: &RpcContext) -> Result<(), ConnError> {
    let frame = tokio::time::timeout(Duration::from_millis(HANDSHAKE_TIMEOUT_MS), frames.next())
        .await
        .map_err(|_| ConnError::HandshakeTimeout)?
        .ok_or(ConnError::NoHandshake)?
        .map_err(ConnError::Frame)?;
    let hello: Hello = decode_frame(&frame).map_err(ConnError::Decode)?;
    if hello.protocol != PROTOCOL_VERSION {
        // Version skew is a typed error, not silence (spec §6).
        let refusal: HelloReply = Err(RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: format!("daemon speaks protocol {PROTOCOL_VERSION}, client sent {}", hello.protocol),
        });
        send(out, &refusal).await?;
        return Err(ConnError::ProtocolMismatch { client: hello.protocol });
    }
    let ack: HelloReply = Ok(HelloAck {
        daemon_version: ctx.daemon_version.clone(),
        protocol: PROTOCOL_VERSION,
        pid: ctx.pid,
    });
    send(out, &ack).await
}
```

`RpcServer::serve` is a `select!` over `listener.accept()` (cancel-safe) and `shutdown.changed()` (cancel-safe); an accept error is logged and the loop continues (a transient `EMFILE` must not take the daemon down).

- [ ] **Step 1: Failing tests** in `server.rs`. **Real clock throughout, with the comment** — these drive real sockets, and paused-clock auto-advance would race the handshake timeout against the peer's bytes.

```rust
#[cfg(test)]
mod tests {
    // Real time: every test here drives a real UnixStream. Under a paused
    // clock the runtime auto-advances whenever it goes idle, which can expire
    // HANDSHAKE_TIMEOUT_MS before the peer's bytes are delivered.
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use shep_core::protocol::{
        BusEvent, Envelope, Hello, HelloReply, PROTOCOL_VERSION, ProcessEventKind, ProcessInfo,
        Request, Response, RpcErrorCode, ServerFrame, codec, decode_frame, encode_frame,
    };
    use tokio_util::codec::Framed;

    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

    struct Client {
        frames: Framed<UnixStream, tokio_util::codec::LengthDelimitedCodec>,
    }

    impl Client {
        async fn send<T: Serialize>(&mut self, value: &T) {
            self.frames.send(encode_frame(value).unwrap()).await.unwrap();
        }

        async fn recv<T: DeserializeOwned>(&mut self) -> T {
            let frame = tokio::time::timeout(RECV_TIMEOUT, self.frames.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("connection closed early")
                .unwrap();
            decode_frame(&frame).unwrap()
        }

        async fn closed(&mut self) -> bool {
            tokio::time::timeout(RECV_TIMEOUT, self.frames.next())
                .await
                .expect("timed out waiting for close")
                .is_none()
        }
    }

    /// Spawns `handle_conn` over a socketpair and hands back the client end.
    fn connected(ctx: RpcContext) -> Client {
        let (server, client) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let _ = handle_conn(server, ctx).await;
        });
        Client { frames: Framed::new(client, codec()) }
    }

    #[tokio::test]
    async fn handshake_acks_a_matching_protocol() {
        let h = harness(vec![]); // same helper shape as rpc.rs's tests
        let mut client = connected(h.ctx.clone());
        client.send(&Hello { client_version: "0.1.0".to_string(), protocol: PROTOCOL_VERSION }).await;
        let ack: HelloReply = client.recv().await;
        let ack = ack.expect("a matching protocol must be acked");
        assert_eq!(ack.protocol, PROTOCOL_VERSION);
        assert_eq!(ack.pid, h.ctx.pid);
        assert_eq!(ack.daemon_version, h.ctx.daemon_version);
    }

    #[tokio::test]
    async fn handshake_refuses_protocol_skew_before_closing() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone());
        client.send(&Hello { client_version: "9.9.9".to_string(), protocol: PROTOCOL_VERSION + 1 }).await;
        let refusal: HelloReply = client.recv().await;
        let err = refusal.expect_err("skew must be refused");
        assert_eq!(err.code, RpcErrorCode::ProtocolMismatch);
        assert!(client.closed().await, "the daemon must close after refusing");
    }

    #[tokio::test]
    async fn a_request_before_the_handshake_ends_the_connection() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone());
        client.send(&Envelope { id: 1, deadline_ms: None, body: Request::Ping }).await;
        assert!(client.closed().await);
    }

    #[tokio::test]
    async fn ping_round_trips_over_the_socket() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone());
        client.send(&Hello { client_version: "0.1.0".to_string(), protocol: PROTOCOL_VERSION }).await;
        let _: HelloReply = client.recv().await;
        client.send(&Envelope { id: 11, deadline_ms: Some(1000), body: Request::Ping }).await;
        let frame: ServerFrame = client.recv().await;
        let ServerFrame::Reply(reply) = frame else { panic!("expected a reply frame") };
        assert_eq!(reply.id, 11);
        assert_eq!(reply.result.unwrap(), Response::Pong);
    }

    #[tokio::test]
    async fn subscribe_streams_only_matching_events() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone());
        client.send(&Hello { client_version: "0.1.0".to_string(), protocol: PROTOCOL_VERSION }).await;
        let _: HelloReply = client.recv().await;
        client
            .send(&Envelope {
                id: 1,
                deadline_ms: None,
                body: Request::Subscribe { topics: vec!["process.*".to_string()] },
            })
            .await;
        let frame: ServerFrame = client.recv().await;
        assert!(matches!(frame, ServerFrame::Reply(ref r) if r.result == Ok(Response::Subscribed)));

        let event = |kind| BusEvent::Process {
            event: kind,
            info: ProcessInfo {
                id: 0,
                name: "web".to_string(),
                status: shep_core::status::ProcStatus::Online,
                pid: Some(1000),
                restarts: 0,
                uptime_ms: 0,
                fold: None,
            },
            manually: false,
            at_ms: 0,
        };
        h.ctx.events.send(event(ProcessEventKind::Start)).unwrap();
        h.ctx.events.send(BusEvent::LogOut { id: 0, line: "filtered".to_string() }).unwrap();
        h.ctx.events.send(event(ProcessEventKind::Online)).unwrap();

        // Back-to-back arrival is the filtering assertion — no negative wait.
        let first: ServerFrame = client.recv().await;
        let second: ServerFrame = client.recv().await;
        assert!(matches!(
            first,
            ServerFrame::Event(BusEvent::Process { event: ProcessEventKind::Start, .. })
        ));
        assert!(matches!(
            second,
            ServerFrame::Event(BusEvent::Process { event: ProcessEventKind::Online, .. })
        ));
    }

    #[tokio::test]
    async fn a_garbage_frame_ends_the_connection_without_panicking() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone());
        client.send(&Hello { client_version: "0.1.0".to_string(), protocol: PROTOCOL_VERSION }).await;
        let _: HelloReply = client.recv().await;
        client.frames.send(bytes::Bytes::from_static(b"not json")).await.unwrap();
        assert!(client.closed().await);
    }

    #[tokio::test]
    async fn peer_credentials_gate_on_uid() {
        let (a, _b) = UnixStream::pair().unwrap();
        let me = daemon_uid();
        assert_eq!(check_peer(&a, me).unwrap(), me);
        assert_eq!(
            check_peer(&a, me + 1).unwrap_err(),
            AuthError::ForeignUid { peer: me, daemon: me + 1 }
        );
    }

    #[tokio::test]
    async fn a_slow_subscriber_gets_a_dropped_notice_instead_of_hanging_the_bus() {
        // Adversarial finding #3: bus.rs's `step()` unit test proves the
        // Lagged->Dropped translation in isolation, but nothing before this
        // exercised it through the REAL connection stack — CONN_QUEUE
        // filling, the forwarder parking on `out.send`, and the broadcast
        // ring (bus.rs's BUS_CAPACITY) actually overflowing for a subscriber
        // that truly never reads. Real time: real socket, matching this
        // whole test mod's rule.
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone());
        client.send(&Hello { client_version: "0.1.0".to_string(), protocol: PROTOCOL_VERSION }).await;
        let _: HelloReply = client.recv().await;
        client
            .send(&Envelope {
                id: 1,
                deadline_ms: None,
                body: Request::Subscribe { topics: vec!["log.*".to_string()] },
            })
            .await;
        let _: ServerFrame = client.recv().await; // the Subscribed reply

        // Never call client.recv() again until AFTER the flood: CONN_QUEUE
        // fills, the forwarder blocks on `out.send`, and the broadcast ring
        // (BUS_CAPACITY) takes the overflow from there — the exact
        // back-pressure chain bus.rs's module comment documents.
        let flood = crate::bus::BUS_CAPACITY + CONN_QUEUE + 16;
        for i in 0..flood {
            h.ctx
                .events
                .send(BusEvent::LogOut { id: 0, line: format!("line-{i}") })
                .unwrap();
        }

        // Resume reading. The count comes from tokio's own Lagged(n) inside
        // the forwarder, never hand-computed here (no-hand-computed-sequences
        // rule) — this only asserts a Dropped notice arrives and is nonzero.
        let dropped = loop {
            match client.recv::<ServerFrame>().await {
                ServerFrame::Event(BusEvent::Dropped { count }) => break count,
                ServerFrame::Event(_) => continue,
                other => panic!("expected eventually a Dropped notice, got {other:?}"),
            }
        };
        assert!(dropped > 0, "a flood past CONN_QUEUE + BUS_CAPACITY must report a real lag");
    }
}
```

- [ ] **Step 2:** RED. **Step 3:** implement `server.rs`; its tests import `harness` from the crate-root `testing` module (Task 4 put it there) rather than growing a second copy. **Step 4:** focused + four gates. **Step 5:** commit `feat(daemon): UDS RPC server — peer-cred auth, handshake, subscriptions` + footer.

---

### Task 6: Boot — layout, pidfile, stale-socket recovery

**Files:** Create `crates/shep-daemon/src/boot.rs`; Modify `lib.rs` (`#[cfg(unix)] pub mod boot;`).

**Interfaces:**

```rust
/// Mode for every directory shep creates (spec §10: no other user, at all)
pub const DIR_MODE: u32 = 0o700;

/// Creates `$SHEP_HOME` and its subdirectories, tightening loose modes
///
/// # Errors
/// - [`BootError::Io`] — a directory could not be created or chmod'ed.
pub fn init_dirs(paths: &ShepPaths) -> Result<(), BootError>;

/// The daemon's own pidfile: `$SHEP_HOME/pids/shepd.pid`
#[must_use] pub fn pidfile(paths: &ShepPaths) -> PathBuf;

/// # Errors
/// - [`BootError::Io`] — the pidfile could not be written.
pub fn write_pidfile(paths: &ShepPaths, pid: u32) -> Result<(), BootError>;

/// Reads the recorded daemon pid, if any
///
/// # Errors
/// - [`BootError::Io`] — the pidfile exists but could not be read.
pub fn read_pidfile(paths: &ShepPaths) -> Result<Option<u32>, BootError>;

/// The socket this daemon binds: the layout default, or a config override
#[must_use] pub fn socket_path(paths: &ShepPaths, override_path: Option<&Path>) -> PathBuf;

/// Binds the control socket, recovering from a crashed daemon's leftovers
///
/// # Errors
/// - [`BootError::AlreadyRunning`] — a live daemon answered on the socket.
/// - [`BootError::Io`] — bind, probe, or unlink failed.
pub fn bind_socket(paths: &ShepPaths, socket: &Path) -> Result<UnixListener, BootError>;

#[derive(Debug)]
pub enum BootError {
    /// A filesystem step failed (carries the path and the OS error)
    Io { path: PathBuf, source: std::io::Error },
    /// Another daemon already answers on this socket (carries its pid if recorded)
    AlreadyRunning { pid: Option<u32> },
    /// The muster roll could not be read or written
    Snapshot(SnapshotError),
    /// The readiness pipe could not be adopted or written
    Ready(SysError),
}
```

**Stale-socket algorithm (spec §6, verbatim):**

```rust
pub fn bind_socket(paths: &ShepPaths, socket: &Path) -> Result<UnixListener, BootError> {
    match UnixListener::bind(socket) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            // EADDRINUSE only says the path exists. Probe it: a live daemon's
            // listener accepts at the kernel level even mid-accept, while a
            // file left behind by a crash (or a reboot) refuses. This is the
            // load-bearing step for the reboot-resurrect scenario (§13.4).
            match std::os::unix::net::UnixStream::connect(socket) {
                Ok(_) => Err(BootError::AlreadyRunning { pid: read_pidfile(paths)? }),
                Err(probe)
                    if matches!(
                        probe.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(socket).map_err(|source| BootError::Io {
                        path: socket.to_path_buf(),
                        source,
                    })?;
                    UnixListener::bind(socket).map_err(|source| BootError::Io {
                        path: socket.to_path_buf(),
                        source,
                    })
                }
                Err(source) => Err(BootError::Io { path: socket.to_path_buf(), source }),
            }
        }
        Err(source) => Err(BootError::Io { path: socket.to_path_buf(), source }),
    }
}
```

`socket_path` honors `DaemonSection::socket` when set; when the override's parent directory is group- or world-writable, `bind_socket` emits a `tracing::warn!` naming the path (an override outside `run/` forfeits the 0700 guarantee the security model rests on — warn, do not refuse; the operator asked for it).

- [ ] **Step 1: Failing tests** in `boot.rs` (real filesystem, tempdirs; no clock involved):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_paths; // the one crate-root fixture (IR-33)
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn init_dirs_creates_the_whole_layout_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        for path in [&paths.home, &paths.logs, &paths.pids, &paths.run] {
            assert!(path.is_dir(), "{} was not created", path.display());
            assert_eq!(mode_of(path), DIR_MODE, "{}", path.display());
        }
        init_dirs(&paths).unwrap(); // idempotent: a restart must not fail here
    }

    #[test]
    fn init_dirs_tightens_a_world_readable_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.run).unwrap();
        std::fs::set_permissions(&paths.run, std::fs::Permissions::from_mode(0o755)).unwrap();
        init_dirs(&paths).unwrap();
        assert_eq!(mode_of(&paths.run), DIR_MODE, "a loose run dir must be tightened, not accepted");
    }

    #[test]
    fn pidfile_round_trips_and_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        assert_eq!(read_pidfile(&paths).unwrap(), None);
        write_pidfile(&paths, 4242).unwrap();
        assert_eq!(read_pidfile(&paths).unwrap(), Some(4242));
        assert_eq!(pidfile(&paths), paths.pids.join("shepd.pid"));
    }

    #[test]
    fn socket_path_honors_a_config_override() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        assert_eq!(socket_path(&paths, None), paths.socket);
        let custom = dir.path().join("custom.sock");
        assert_eq!(socket_path(&paths, Some(&custom)), custom);
    }

    #[tokio::test]
    async fn bind_socket_binds_a_fresh_path() {
        // Real time: real socket IO (see the paused-clock rule).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let listener = bind_socket(&paths, &paths.socket).unwrap();
        assert!(paths.socket.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn a_socket_left_by_a_crash_is_unlinked_and_rebound() {
        // Neither std nor tokio unlinks a UnixListener's path on drop, so this
        // is exactly the file a killed daemon leaves behind.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        drop(UnixListener::bind(&paths.socket).unwrap());
        assert!(paths.socket.exists(), "the stale file must still be there");
        let listener = bind_socket(&paths, &paths.socket).unwrap();
        assert!(paths.socket.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn a_live_socket_is_reported_as_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let live = UnixListener::bind(&paths.socket).unwrap();
        write_pidfile(&paths, 4242).unwrap();
        assert!(matches!(
            bind_socket(&paths, &paths.socket),
            Err(BootError::AlreadyRunning { pid: Some(4242) })
        ));
        drop(live);
    }
}
```

- [ ] **Step 2:** RED. **Step 3:** implement. **Step 4:** focused + four gates. **Step 5:** commit `feat(daemon): boot layout — 0700 dirs, pidfile, stale-socket recovery` + footer.

---

### Task 7: Readiness pipe, signal handlers, and the assembled daemon

**Files:** Create `crates/shep-daemon/src/sys.rs`; Modify `boot.rs`, `lib.rs`.

**This task spends the phase's only `unsafe`.** `sys.rs` gets a module-scoped `#[allow(unsafe_code)]` with the IR-24 rationale essay above it:

> **Why unsafe here, and nowhere else.** The daemonization contract (spec §3) is that the CLI re-execs itself detached and the child reports `{pid, version}` on an inherited pipe once the socket is bound. Adopting an inherited descriptor is the one operation std offers no safe path for: `OwnedFd`/`File` can only be built from a raw number through `from_raw_fd`, which is unsafe because nothing in the type system proves the number names a descriptor this process owns.
> **Rejected alternative:** have the parent pass a socket path (`SHEP_READY_SOCK`) and let the child connect and write. It is entirely safe and was the first design. Its cost is a second socket in the boot path — one more thing to place inside 0700, unlink, and recover when stale — to replace a five-line adoption, and it puts the readiness handshake on a different mechanism from the one the spec, systemd `Type=notify` integration, and every comparable supervisor use. Not worth it.
> **Invariant:** the descriptor was inherited across `exec` from our own parent and is not otherwise owned in this process. **Checked, not assumed:** `adopt_ready_pipe` refuses anything below fd 3 (stdio is owned elsewhere) and calls `fcntl(fd, F_GETFD)` first, so a closed or never-opened number returns `SysError::BadFd` instead of being adopted. **Failure scenarios considered:** (a) a hostile `SHEP_READY_FD=1` — refused by the fd-3 floor, so stdout is never closed underneath the logger; (b) a stale number for a descriptor closed since exec — refused by the `fcntl` probe; (c) a number that has been *recycled* into another live descriptor since exec — impossible in practice because the adoption happens during boot, before the daemon opens anything, and the env var comes from our own parent process, not from a user; (d) double adoption — `adopt_ready_pipe` is called once, from `boot`, and consumes the number into an owning `File`.

**Interfaces:**

```rust
// sys.rs
/// Takes ownership of a descriptor inherited across `exec`
///
/// # Errors
/// - [`SysError::ReservedFd`] — `fd` is below 3 (stdio is owned elsewhere).
/// - [`SysError::BadFd`] — `fd` names no open descriptor in this process.
///
/// # Safety-relevant behavior
/// See the module rationale: the descriptor is validated before adoption and
/// the returned [`File`] owns it from then on.
pub fn adopt_fd(fd: RawFd) -> Result<std::fs::File, SysError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SysError {
    /// The descriptor number is below 3 and cannot be adopted
    ReservedFd(RawFd),
    /// The descriptor is not open in this process (carries the errno name)
    BadFd { fd: RawFd, errno: String },
    /// Writing the readiness line failed (carries the OS message)
    ReadyWrite(String),
}

// boot.rs
/// Environment variable naming the inherited readiness descriptor
pub const READY_FD_ENV: &str = "SHEP_READY_FD";

/// What the daemonizing parent reads off the readiness pipe
// wire format: shep-cli parses this line; changing it is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonReady {
    pub pid: u32,
    pub version: String,
}

/// Reports readiness to the parent and closes the pipe
///
/// # Errors
/// - [`BootError::Ready`] — the descriptor could not be adopted or written.
pub fn signal_ready(fd: RawFd, ready: &DaemonReady) -> Result<(), BootError>;

/// Options the CLI hands the daemon at boot
#[derive(Debug, Default)]
pub struct BootOptions {
    pub socket: Option<PathBuf>,
    pub ready_fd: Option<RawFd>,
    /// Restore the muster roll if one exists (spec §9's `shep muster`)
    pub restore: bool,
}

/// Brings the daemon up: layout, roll restore, bus, supervisor, socket
///
/// # Errors
/// - [`BootError::AlreadyRunning`] — another daemon owns this `$SHEP_HOME`.
/// - [`BootError::Io`] — a boot filesystem step failed.
pub async fn boot<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    options: BootOptions,
) -> Result<RunningDaemon, BootError>;

#[derive(Debug)]
pub struct RunningDaemon { /* ctx, listener, writer, options */ }

impl RunningDaemon {
    /// Handles for driving this daemon from outside its run loop
    #[must_use] pub fn context(&self) -> RpcContext;
    #[must_use] pub fn socket(&self) -> &Path;
    /// Serves until a signal or `KillDaemon`, then tears down in order
    ///
    /// # Errors
    /// - [`BootError::Io`] — a teardown filesystem step failed.
    pub async fn run(self) -> Result<(), BootError>;
}
```

**Teardown order (locked — a test pins it):**

```
1. stop the snapshot writer            (nothing may rewrite the roll from here on)
2. write the final muster roll         (records the flock AS IT WAS, still running)
3. broadcast BusEvent::DaemonShutdown  (subscribers learn before their sockets close)
4. supervisor.shutdown().await         (kill ladder on every online sheep)
5. unlink the socket, remove the pidfile
```

Steps 1–2 before 4 is the whole point: run them the other way round and the roll records a flock of stopped sheep, and `shep muster` after a reboot restores nothing — silently breaking spec §13.4, the flagship migration scenario.

**Signal handling:**

```rust
fn install_signals(shutdown: Arc<watch::Sender<bool>>, paths: ShepPaths) -> Result<Arc<AtomicU64>, BootError> {
    let reopens = Arc::new(AtomicU64::new(0));
    for kind in [SignalKind::terminate(), SignalKind::interrupt(), SignalKind::quit()] { /* spawn: on first signal, shutdown.send(true) */ }
    // SIGUSR2 is `shep reopen`'s out-of-band form (spec §9). Installing the
    // handler is load-bearing on its own: SIGUSR2's DEFAULT disposition is to
    // terminate, so an unhandled log-rotation signal kills the daemon. Full
    // per-sheep handle reopening lands with `flush`/`reopen` (Phase 4); today
    // this re-creates a missing log dir, counts the request, and logs it.
    Ok(reopens)
}
```

- [ ] **Step 1: Failing tests.** `sys.rs` (no runtime needed):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::IntoRawFd;

    #[test]
    fn a_real_inherited_descriptor_is_adopted_and_owned() {
        // into_raw_fd gives up std's ownership, which is exactly the state an
        // exec-inherited descriptor is in: live, and owned by nobody yet.
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = child.into_raw_fd();
        {
            let mut adopted = adopt_fd(fd).unwrap();
            std::io::Write::write_all(&mut adopted, b"hello\n").unwrap();
        } // dropping the File closes the descriptor
        let mut read = String::new();
        let mut parent = parent;
        parent.read_to_string(&mut read).unwrap();
        assert_eq!(read, "hello\n", "EOF proves the adopted descriptor was closed on drop");
    }

    #[test]
    fn stdio_numbers_are_refused() {
        for fd in 0..3 {
            assert_eq!(adopt_fd(fd).unwrap_err(), SysError::ReservedFd(fd));
        }
    }

    #[test]
    fn a_closed_descriptor_is_refused_instead_of_adopted() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = a.into_raw_fd();
        drop(adopt_fd(fd).unwrap()); // adopt once, closing it
        assert!(matches!(adopt_fd(fd), Err(SysError::BadFd { .. })));
    }
}
```

`boot.rs` additions:

```rust
    #[test]
    fn readiness_reports_pid_and_version_then_closes_the_pipe() {
        use std::io::Read;
        use std::os::unix::io::IntoRawFd;
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        let ready = DaemonReady { pid: 4242, version: "0.1.0".to_string() };
        signal_ready(child.into_raw_fd(), &ready).unwrap();
        let mut line = String::new();
        let mut parent = parent;
        parent.read_to_string(&mut line).unwrap();
        assert_eq!(line.trim_end(), serde_json::to_string(&ready).unwrap());
        assert!(line.ends_with('\n'), "the parent reads a line: {line:?}");
    }

    #[tokio::test]
    async fn boot_restores_a_saved_flock_and_tears_down_in_order() {
        // Real time: binds a real socket.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp {
                app: shep_core::config::AppConfig::minimal("web", "./srv"),
                instances_running: 1,
            }],
        };
        crate::snapshot::write_atomic(&paths.snapshot, &roll).unwrap();

        let daemon = boot(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            BootOptions { restore: true, ..BootOptions::default() },
        )
        .await
        .unwrap();
        let ctx = daemon.context();
        let flock = ctx.supervisor.list_checked().await.unwrap();
        assert_eq!(flock.len(), 1, "the muster roll must be back on its feet");
        assert_eq!(flock[0].name, "web");

        let run = tokio::spawn(daemon.run());
        ctx.shutdown();
        tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap().unwrap();

        // The roll written during teardown records the flock as it WAS.
        let final_roll = crate::snapshot::read(&paths.snapshot).unwrap();
        assert_eq!(final_roll.apps[0].instances_running, 1,
            "the roll must be written before the flock is killed, or muster restores nothing");
        assert!(!paths.socket.exists(), "the socket is unlinked on a clean exit");
        assert_eq!(read_pidfile(&paths).unwrap(), None);
    }

    #[tokio::test]
    async fn sigterm_triggers_the_same_graceful_shutdown() {
        // Real time + a real signal. Safe to raise here only because the
        // handler is installed first: SIGTERM's default action would kill the
        // test binary. tokio never uninstalls it, which is harmless.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let daemon = boot(ScriptedRunner::new(vec![]), paths.clone(), BootOptions::default())
            .await
            .unwrap();
        let run = tokio::spawn(daemon.run());
        tokio::time::sleep(Duration::from_millis(50)).await; // let install_signals land
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap().unwrap();
        assert!(!paths.socket.exists());
    }
```

(The 50 ms sleep is the one real sleep in the phase and it guards a signal race, not a state transition — comment it as such. If `run()` grows a "handlers installed" readiness signal later, replace it.)

- [ ] **Step 2:** RED. **Step 3:** implement `sys.rs` (essay + one `// SAFETY:` block) and `boot.rs`'s remainder. **Step 4:** focused + four gates; confirm `cargo clippy` accepts the `#[allow(unsafe_code)]` scope and `undocumented_unsafe_blocks` is satisfied. **Step 5:** commit `feat(daemon): daemon boot — readiness pipe, signal handlers, ordered teardown` + footer.

---

### Task 8: uid/gid spawn (2a deferral — privilege drop)

**Files:** Create `crates/shep-daemon/src/privilege.rs`; Modify `runner.rs` (`SpawnSpec`), `assemble.rs`, `entry.rs`, `supervisor.rs`, `tokio_runner.rs`, `fake.rs` if its `SpawnSpec` literals need the new field, `tests/real_runner.rs`, `lib.rs`.

**Interfaces:**

```rust
// privilege.rs (#[cfg(unix)])
/// Resolved unix credentials for a spawned sheep
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credentials {
    pub uid: u32,
    pub gid: Option<u32>,
}

/// Resolves an app's `user`/`group` names to numeric ids
///
/// Returns `None` when the app asks for neither.
///
/// # Errors
/// - [`PrivilegeError::UnknownUser`] — no passwd entry for that name.
/// - [`PrivilegeError::UnknownGroup`] — no group entry for that name.
/// - [`PrivilegeError::Lookup`] — the passwd/group database could not be read.
/// - [`PrivilegeError::NotPermitted`] — credentials were requested but this
///   daemon does not run as root, so it cannot change a child's identity.
pub fn resolve(app: &AppConfig) -> Result<Option<Credentials>, PrivilegeError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeError {
    UnknownUser(String),
    UnknownGroup(String),
    Lookup(String),
    NotPermitted { user: Option<String>, group: Option<String> },
}
```

`SpawnSpec` gains `pub credentials: Option<Credentials>` (replacing 2a's `// uid/gid … deferred to Phase 2b` comment). `assemble(app, instance, paths, credentials)` takes it as a parameter — resolution touches the passwd database, so it stays out of the pure assembler. The supervisor resolves **once per `Start`**, stores it on the `ProcessEntry`, and reuses it for every restart; a resolution failure fails that `Start` with `SupervisorError::SpawnFailed(err.to_string())` (2a's error enum is unchanged).

`tokio_runner.rs` applies it on the std command before spawn:

```rust
if let Some(creds) = spec.credentials {
    // std sets the gid before the uid in the child (setgid must happen while
    // still privileged), which is the order privilege drop requires.
    // KNOWN LIMITATION: supplementary groups are inherited from the daemon —
    // CommandExt::groups is still unstable. Documented, deferred.
    if let Some(gid) = creds.gid {
        command.gid(gid);
    }
    command.uid(creds.uid);
}
```

**PATH/base-env fix (adversarial finding #1 — BLOCKER, folded into this task because it touches `assemble`'s signature too):** `tokio_runner.rs:148-149` does `command.env_clear()` then `command.envs(&spec.env)` — by design (`SpawnSpec::env` is documented "fully resolved, no daemon-env leakage beyond this map"). But `assemble.rs` today builds `spec.env` from `config.env` plus only the `SHEP_INSTANCE`/`increment_var` slot var — no `PATH`. `runner.rs`'s own doc on `SpawnSpec::program` says "resolved via `PATH` if bare", yet nothing puts a `PATH` in the map that survives `env_clear()`. A bare interpreter or program (`node`, `python3`, a `PATH`-relative script with no `interpreter` at all) spawns via `Command::new("node")` with an **empty** env — the OS has nowhere to look it up, so every such spawn fails with `RunnerError::SpawnFailed` (ENOENT). This never surfaces in-tree because 2a's and this plan's own tests only ever spawn absolute paths (`/bin/sh`, `test_paths()`'s `./srv` fixtures never actually exec).

Fix: `assemble` seeds a small base env *before* folding in the app's own `config.env`, so app-set values still win on conflict (`BTreeMap::extend`'s last-writer-wins):

```rust
// assemble.rs
use std::collections::BTreeMap;

/// The env every spawned child starts from, before the app's own `env` map
/// is folded on top (app config always wins on conflict).
///
/// `tokio_runner.rs` calls `env_clear()` then `envs(&spec.env)` — the child
/// sees exactly this map and nothing else. Without a `PATH` in it, a bare
/// program/interpreter name (anything with no `/`: `node`, `python3`, `sh`,
/// a PATH-relative script) can never be found by exec; this is reading the
/// DAEMON'S OWN env once (not a file, not the child's), so it stays a pure
/// function of process state, not a filesystem/network IO the module doc's
/// "no I/O" note is warning about.
fn base_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
    env.insert("PATH".to_string(), path);
    for key in ["HOME", "USER", "LANG", "TZ"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    env
}
```

and in `assemble`, replace the current `let mut env = config.env.clone();` with:

```rust
    // Base env FIRST (PATH + a small inherited allowlist), then the app's
    // own env on top: env_clear() + envs(&spec.env) in tokio_runner.rs means
    // anything not seeded here is invisible to the child (adversarial
    // finding #1 — a bare interpreter/program spawned with no PATH is
    // ENOENT, not a slow failure).
    let mut env = base_env();
    env.extend(config.env.clone());
```

(The rest of `assemble` — the `slot_var` insert immediately after — is unchanged.)

- [ ] **Step 1: Failing tests** in `privilege.rs` — hermetic on any unix box because they resolve *this* process's own identity:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::config::AppConfig;

    fn own_user_name() -> String {
        nix::unistd::User::from_uid(nix::unistd::geteuid())
            .unwrap()
            .expect("this process has a passwd entry")
            .name
    }

    #[test]
    fn no_user_or_group_means_no_credentials() {
        assert_eq!(resolve(&AppConfig::minimal("web", "./srv")).unwrap(), None);
    }

    #[test]
    fn a_real_user_name_resolves_to_its_uid() {
        // Resolving our OWN name is always permitted: changing identity to the
        // one we already have needs no privilege.
        let mut app = AppConfig::minimal("web", "./srv");
        app.user = Some(own_user_name());
        let creds = resolve(&app).unwrap().expect("a user was requested");
        assert_eq!(creds.uid, nix::unistd::geteuid().as_raw());
        assert_eq!(creds.gid, None);
    }

    #[test]
    fn an_unknown_user_is_a_typed_error_naming_the_user() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.user = Some("definitely-not-a-real-shep-user".to_string());
        assert_eq!(
            resolve(&app).unwrap_err(),
            PrivilegeError::UnknownUser("definitely-not-a-real-shep-user".to_string())
        );
    }

    #[test]
    fn asking_for_another_user_without_root_is_refused_in_plain_english() {
        if nix::unistd::geteuid().is_root() {
            // Running as root, the refusal cannot trigger; the guard itself is
            // what this test covers, so there is nothing to assert here.
            return;
        }
        let mut app = AppConfig::minimal("web", "./srv");
        app.user = Some("root".to_string());
        let err = resolve(&app).unwrap_err();
        assert!(matches!(err, PrivilegeError::NotPermitted { .. }));
        assert!(
            err.to_string().contains("not running as root"),
            "the message must say what to do: {err}"
        );
    }
}
```

plus, in `assemble.rs`'s test mod, two tests pinning the PATH-seeding contract (adversarial finding #1):

```rust
    #[test]
    fn assembled_env_always_carries_a_path() {
        // tokio_runner.rs's env_clear() + envs(&spec.env) means this map IS
        // the child's whole env: no PATH here, and a bare interpreter name
        // (node, python3, sh, ...) can never be found by exec.
        let app_config = AppConfig {
            name: "web".to_string(),
            script: "app.js".to_string(),
            args: vec![],
            interpreter: Some("node".to_string()),
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let spec = assemble(&app, 0, &test_paths());
        let path = spec.env.get("PATH").expect("PATH must survive env_clear()+envs(&spec.env)");
        assert!(!path.is_empty(), "an empty PATH is exactly the ENOENT failure mode");
    }

    #[test]
    fn an_explicit_app_path_overrides_the_seeded_default() {
        let mut app_config = AppConfig {
            name: "web".to_string(),
            script: "app.js".to_string(),
            args: vec![],
            interpreter: Some("node".to_string()),
            ..Default::default()
        };
        app_config.env.insert("PATH".to_string(), "/opt/custom/bin".to_string());
        let app = normalize(app_config).unwrap();
        let spec = assemble(&app, 0, &test_paths());
        assert_eq!(spec.env.get("PATH").map(String::as_str), Some("/opt/custom/bin"));
    }
```

plus, in `supervisor.rs`'s test mod, one paused-clock test that a `user` the box does not have fails the `Start` rather than silently spawning as the daemon:

```rust
    #[tokio::test(start_paused = true)]
    async fn an_unresolvable_user_fails_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let handle = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            crate::testing::test_paths(&dir),
            events,
        );
        let mut app = shep_core::config::AppConfig::minimal("svc", "./svc");
        app.user = Some("definitely-not-a-real-shep-user".to_string());
        let err = handle.start(vec![normalize(app).unwrap()]).await.unwrap_err();
        assert!(matches!(err, SupervisorError::SpawnFailed(_)));
    }
```

and in `tests/real_runner.rs`, the root-only proof, ignored by default:

```rust
#[tokio::test]
#[ignore = "needs root: run with `sudo -E cargo test -p shep-daemon --test real_runner -- --ignored`"]
async fn a_dropped_child_runs_as_the_requested_user() {
    // Real time: this file's whole tier is real OS behavior.
    assert!(nix::unistd::geteuid().is_root(), "this test only means anything as root");
    let target = nix::unistd::User::from_name("nobody")
        .unwrap()
        .expect("every unix box has `nobody`");

    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_for(&dir, "/bin/sh", &["-c", "id -u"]); // this file's existing helper
    spec.credentials = Some(shep_daemon::privilege::Credentials {
        uid: target.uid.as_raw(),
        gid: Some(target.gid.as_raw()),
    });

    let runner = TokioRunner::new();
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let printed = tokio::time::timeout(std::time::Duration::from_secs(5), io.logs.recv())
        .await
        .expect("the child must print its uid")
        .expect("the log pump must deliver the line");
    assert_eq!(printed.line.trim(), target.uid.as_raw().to_string());
    assert!(!printed.err);
    assert_eq!(proc.wait().await.code, Some(0));
}
```

- [ ] **Step 2:** RED. **Step 3:** implement; update every `SpawnSpec` literal in the workspace (the fake's test fixture included); implement `assemble.rs`'s `base_env()` + the PATH-first env merge (adversarial finding #1) in the SAME step, since both touch `assemble`'s signature/body. **Step 4:** focused + four gates. **Step 5:** commit `fix(daemon): uid/gid spawn + seed PATH/base env so bare interpreters exec` + footer.

---

### Task 9: Supervisor proptest (2a deferral — IR-37)

**Files:** Modify `crates/shep-daemon/src/supervisor.rs` — production code AND test mod. (Not test-mod-only, despite 2a's framing of this task as "just the proptest": adversarial finding #2 is a real production bug in `supervisor.rs` that the proptest as scoped below would not even exercise — see Step 1.)

**Step 1 (adversarial finding #2 — MAJOR, fix first): Delete racing Shutdown must still deregister the sheep.** `Command::Delete` (`handle_command`, `Command::Delete { selector, reply } => self.begin_manual(selector, ManualKind::Delete, ReplyKind::Ids(reply))`) has no `self.shutting_down` guard, unlike `Start`/`Restart`. `begin_shutdown` sets `manual = Some(ManualKind::Stop)` on every online sheep with first-command-wins dedup (IMPORTANT-4). A `Delete` that lands on an id AFTER `begin_shutdown` already claimed it hits `begin_manual`'s `already_in_flight` branch: it only does `remaining.insert(id)` for its own `PendingReply`, it never overwrites `slot.manual`. When that sheep goes terminal, `decide_on_exit` sees `manual.is_some()` (it's `Some(Stop)`, not `Some(Delete)`) and always returns `Decision::CleanStop`; `handle_exited`'s `Decision::CleanStop if manual == Some(ManualKind::Delete)` arm (the only one that calls `self.sheep.remove(&id)`) does NOT match, so the sheep lands in `Decision::CleanStop => { self.set_status(id, ProcStatus::Stopped); ... }` instead — still registered. `resolve_pending` then fulfills BOTH the shutdown's and the Delete's `PendingReply` for that id (it walks every pending entry), so the `Delete` caller is handed back `Ok(vec![id])` — told the sheep is gone — while `self.sheep` still holds it as `Stopped`.

Fix: give `SheepSlot` a `pending_delete: bool` that records delete-intent independently of which command's `manual` marker won the race, and let `handle_exited` deregister on that flag too, not only on `manual == Some(ManualKind::Delete)`.

```rust
// SheepSlot (near `manual`):
    /// Set whenever a `Delete` targets this id, even if an earlier command
    /// already owns `manual` (adversarial finding #2 — the fix for
    /// Delete-racing-Shutdown). `manual` records who owns the next Kill;
    /// `pending_delete` records intent that must survive regardless of who
    /// won that race, so a Delete can never be silently downgraded to a
    /// Stop just because a Shutdown's Stop got there first.
    pending_delete: bool,
```

`begin_manual`'s `is_running` branch (inside the `for id in matched` loop, right after the existing `if !already_in_flight { ... }` block, before `remaining.insert(id);`):

```rust
                if kind == ManualKind::Delete {
                    // Regardless of which command's `manual` marker won,
                    // this id must still be deregistered once it goes
                    // terminal — see the SheepSlot::pending_delete doc.
                    if let Some(slot) = self.sheep.get_mut(&id) {
                        slot.pending_delete = true;
                    }
                }
                remaining.insert(id);
```

`handle_exited` (capture the flag alongside `manual`, and add it to the CleanStop guard):

```rust
        let manual = slot.manual.take();
        let pending_delete = std::mem::take(&mut slot.pending_delete);
        // ... (uptime/started_at unchanged) ...
            Decision::CleanStop if manual == Some(ManualKind::Delete) || pending_delete => {
                let mut removed = self.sheep.remove(&id).expect("checked above");
                removed.entry.status = ProcStatus::Stopped;
                let info = to_info(&removed.entry);
                self.emit(ProcessEventKind::Delete, info.clone(), true);
                info
            }
```

Both `SheepSlot { ... }` literals in `spawn_fresh` gain `pending_delete: false`.

Regression test (same `tokio::spawn` + `yield_now` race idiom `overlapping_stop_and_restart_agree_on_one_outcome` already uses in this file to sequence two commands onto one sheep — not a scheduler-tick assertion, just ordering the mailbox):

```rust
    #[tokio::test(start_paused = true)]
    async fn delete_racing_shutdown_still_deregisters_the_sheep() {
        let (events, _rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![ProcScript::ignores_signals()]); // wide kill-ladder window
        let handle = spawn_supervisor(runner, test_paths(), events);
        let app = AppConfig::minimal("svc", "./svc");
        let started = handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        let id = started[0].id;

        let h2 = handle.clone();
        let shutter = tokio::spawn(async move { h2.shutdown().await });
        for _ in 0..10 {
            tokio::task::yield_now().await; // let Shutdown claim the manual marker first
        }
        let deleted = handle.delete(ProcessSelector::Id(id)).await.unwrap();
        shutter.await.unwrap();

        assert_eq!(deleted, vec![id], "the caller was told this id was deleted");
        assert!(
            handle.list().await.iter().all(|info| info.id != id),
            "a Delete that raced a Shutdown must still deregister the sheep, \
             not just tell its caller it did"
        );
    }
```

- [ ] **Step 1a:** write the regression test above — RED against today's `supervisor.rs` (the id survives shutdown as `Stopped`). Implement `pending_delete` as specified. **Step 1b:** confirm GREEN, then the four gates.

**Step 2 (the proptest this task was originally scoped for) — Design:** proptest generates two independent things — a **command script** (what the operator does) and a **process script** (how the children behave) — and the interleaving between them emerges from the runtime, so nothing is hand-derived. Invariants are read off the event stream and successive `list()` snapshots, never off tick counts.

```rust
#[derive(Debug, Clone, Copy)]
enum Step { List, StopAll, RestartAll, DeleteFirst, StartOne }

fn step_strategy() -> impl proptest::strategy::Strategy<Value = Step> { /* prop_oneof! over the five */ }

fn script_strategy() -> impl proptest::strategy::Strategy<Value = ProcScript> {
    // Weighted toward long-lived children so a run explores command handling
    // rather than only exhausting the restart budget.
    prop_oneof![
        6 => Just(ProcScript::never_exits()),
        2 => Just(ProcScript::const_exit(1)),
        1 => Just(ProcScript::stable_then_exit(2_000, 0)),
        1 => Just(ProcScript::ignores_signals()),
    ]
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config { cases: 24, ..proptest::test_runner::Config::default() })]

    #[test]
    fn supervisor_upholds_its_invariants_under_any_interleaving(
        steps in proptest::collection::vec(step_strategy(), 1..10),
        scripts in proptest::collection::vec(script_strategy(), 128..129),
    ) {
        // A current-thread runtime with a paused clock inside the proptest
        // body: every backoff delay is virtual, so a 24-case run is instant.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        runtime.block_on(async move {
            let (events, mut rx) = tokio::sync::broadcast::channel(4096);
            let handle = spawn_supervisor(
                ScriptedRunner::new(scripts),
                crate::testing::test_paths(&dir),
                events,
            );
            let mut started = 0u32;
            let mut highest_restarts = std::collections::HashMap::<u32, u32>::new();

            for step in steps {
                match step {
                    Step::StartOne => {
                        started += 1;
                        let app = AppConfig::minimal(&format!("sheep-{started}"), "./s");
                        let _ = handle.start(vec![normalize(app).unwrap()]).await;
                    }
                    Step::StopAll => {
                        if let Ok(stopped) = handle.stop(ProcessSelector::All).await {
                            // A deferred reply means every match is terminal.
                            for info in stopped {
                                prop_assert_eq!(info.status, ProcStatus::Stopped);
                            }
                        }
                    }
                    Step::RestartAll => { let _ = handle.restart(ProcessSelector::All).await; }
                    Step::DeleteFirst => {
                        if let Some(first) = handle.list().await.first() {
                            let id = first.id;
                            if let Ok(deleted) = handle.delete(ProcessSelector::Id(id)).await {
                                prop_assert_eq!(deleted, vec![id]);
                            }
                            prop_assert!(handle.list().await.iter().all(|i| i.id != id));
                        }
                    }
                    Step::List => {}
                }

                let listed = handle.list().await;
                // (1) ids are unique and the listing is sorted by id.
                let ids: Vec<u32> = listed.iter().map(|i| i.id).collect();
                let mut sorted = ids.clone();
                sorted.sort_unstable();
                sorted.dedup();
                prop_assert_eq!(&ids, &sorted);
                for info in &listed {
                    // (2) restart counts never decrease for a given id.
                    let seen = highest_restarts.entry(info.id).or_default();
                    prop_assert!(info.restarts >= *seen);
                    *seen = info.restarts;
                    // (3) no status outside the spec's set ever surfaces.
                    prop_assert!(matches!(
                        info.status,
                        ProcStatus::Starting | ProcStatus::Online | ProcStatus::Stopping
                            | ProcStatus::Stopped | ProcStatus::Errored | ProcStatus::WaitingRestart
                    ));
                }
            }

            // (4) never two live processes for one id: the event stream must
            // never show Start -> Start for an id without a terminal event
            // between them.
            let mut live = std::collections::HashSet::<u32>::new();
            while let Ok(event) = rx.try_recv() {
                if let BusEvent::Process { event, info, .. } = event {
                    match event {
                        ProcessEventKind::Start => {
                            prop_assert!(live.insert(info.id), "two live spawns for id {}", info.id);
                        }
                        ProcessEventKind::Exit
                        | ProcessEventKind::Stop
                        | ProcessEventKind::Errored
                        | ProcessEventKind::Delete => { live.remove(&info.id); }
                        _ => {}
                    }
                }
            }
            // The async block's error type is proptest's, so `?` above and this
            // tail agree; block_on hands the Result back to the proptest body.
            Ok::<(), proptest::test_runner::TestCaseError>(())
        })?;
    }
}
```

- [ ] **Step 2a:** write the proptest; run it — it is RED only if an invariant is genuinely violated (note: the `Step` enum above has no `Shutdown` variant, so this proptest does NOT itself cover the finding #2 race — that is exactly why Step 1's regression test exists as a separate, targeted case). Treat any other failure as a 2a bug and fix it in this task's commit, quoting the minimized case in the commit body.
- [ ] **Step 2b:** cap CI cases via `PROPTEST_CASES` in the workflow env if the run exceeds a few seconds (IR-37's explicit-bounds rule).
- [ ] **Step 3:** four gates (covering both Step 1's fix and Step 2's proptest). **Step 4:** commit `fix(daemon): delete-racing-shutdown deregistration; test: supervisor proptest over command/exit interleavings (IR-37)` + footer.

---

### Task 10: End-to-end tier — a real daemon over a real socket

**Files:** Create `crates/shep-daemon/tests/daemon_e2e.rs`.

Header comment (IR-38 deviation, stated once at the top):

```rust
//! Real-daemon integration tier: boots shep-daemon on a temp `$SHEP_HOME`,
//! talks to it over the control socket with shep-core's own codec, and
//! drives real child processes.
//!
//! Real time throughout, by necessity: these tests own real sockets and real
//! children, and a paused clock's auto-advance would expire timeouts before
//! IO wakeups arrive. IR-38 deviation deliberate — behavioral OS tests need
//! their own binary so the unit tier stays paused-clock pure.
```

**Fixture + client (write these first; every test below builds on them):**

```rust
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// A booted daemon on its own `$SHEP_HOME`, with its run loop spawned.
struct Fixture {
    dir: tempfile::TempDir,
    paths: ShepPaths,
    ctx: RpcContext,
    run: tokio::task::JoinHandle<Result<(), BootError>>,
}

impl Fixture {
    async fn boot(dir: tempfile::TempDir, restore: bool) -> Self {
        // $SHEP_HOME is the tempdir root itself: sun_path caps the socket
        // path near 104 bytes and macOS temp paths are already long.
        let home = dir.path().to_path_buf();
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
            std::path::Path::new("/nonexistent"),
        );
        let daemon = boot(
            TokioRunner::new(),
            paths.clone(),
            BootOptions { restore, ..BootOptions::default() },
        )
        .await
        .expect("the daemon must boot on a fresh home");
        let ctx = daemon.context();
        let run = tokio::spawn(daemon.run());
        Self { dir, paths, ctx, run }
    }

    async fn connect(&self) -> Client {
        let stream = UnixStream::connect(&self.paths.socket).await.unwrap();
        let mut client = Client { frames: Framed::new(stream, codec()), next_id: 1 };
        let ack: HelloReply = client.recv_as().await;
        client.hello_ack = Some(ack.expect("the daemon must ack our protocol"));
        client
    }

    /// Shuts the daemon down and waits for its ordered teardown.
    async fn shutdown(self) -> tempfile::TempDir {
        self.ctx.shutdown();
        tokio::time::timeout(RECV_TIMEOUT, self.run)
            .await
            .expect("teardown must not hang")
            .unwrap()
            .unwrap();
        self.dir
    }
}

struct Client { /* Framed<UnixStream, LengthDelimitedCodec>, next_id, hello_ack */ }

impl Client {
    /// Sends `Hello`, then one request, and returns its `Reply`, skipping any
    /// bus events that arrive in between.
    async fn request(&mut self, body: Request) -> Reply;
    /// The next frame of any kind.
    async fn next_frame(&mut self) -> ServerFrame;
    /// Reads frames until a `Process` event of `kind` for `id` arrives.
    async fn await_process_event(&mut self, id: u32, kind: ProcessEventKind) -> ProcessInfo;
}
```

(`Client::connect` sends the `Hello` before returning, so every test starts from a handshaken connection; the skew test builds its socket by hand instead. Every read inside these helpers is wrapped in `tokio::time::timeout(RECV_TIMEOUT, …)` — IR-39's no-sleeps, event-driven rule.)

Tests:

1. **`handshake_then_start_list_and_stop_a_real_sheep`** — full code, since it is the pattern the rest follow:

```rust
#[tokio::test]
async fn handshake_then_start_list_and_stop_a_real_sheep() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;
    assert_eq!(client.hello_ack().pid, std::process::id());
    assert_eq!(client.hello_ack().protocol, PROTOCOL_VERSION);

    // Subscribe BEFORE starting: the bus delivers from the moment you join.
    let subscribed = client
        .request(Request::Subscribe { topics: vec!["process.*".to_string()] })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal("sleeper", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else { panic!("expected started") };
    assert_eq!(infos.len(), 1);
    let id = infos[0].id;
    let spawned_pid = infos[0].pid.expect("a real spawn reports a real pid");

    let online = client.await_process_event(id, ProcessEventKind::Online).await;
    assert_eq!(online.pid, Some(spawned_pid));

    let listed = client.request(Request::ListFlock).await;
    let Response::Flock(flock) = listed.result.unwrap() else { panic!("expected flock") };
    assert_eq!(flock.len(), 1);
    assert_eq!(flock[0].status, ProcStatus::Online);
    assert_eq!(flock[0].pid, Some(spawned_pid));

    let stopped = client.request(Request::Stop { selector: SelectorSpec::All }).await;
    let Response::Stopped(gone) = stopped.result.unwrap() else { panic!("expected stopped") };
    // The reply is deferred until the kill ladder finished, so this is terminal.
    assert_eq!(gone[0].status, ProcStatus::Stopped);
    client.await_process_event(id, ProcessEventKind::Stop).await;

    fixture.shutdown().await;
}
```

2. **`log_lines_reach_a_log_subscriber`** — subscribe `["log.*"]`, start `/bin/sh -c 'echo hello-flock; sleep 5'`, assert a `BusEvent::LogOut` carrying `hello-flock` for that id.
3. **`protocol_skew_is_refused_over_the_real_socket`** — connect, send `Hello{protocol: PROTOCOL_VERSION + 1}`, expect `HelloReply::Err(ProtocolMismatch)` and then EOF.

4. **`kill_daemon_shuts_the_flock_down_and_unlinks_the_socket`** (adversarial finding #5 — full code; the flagship ordered-teardown proof):

```rust
#[tokio::test]
async fn kill_daemon_shuts_the_flock_down_and_unlinks_the_socket() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let sleeper = |name: &str| {
        let mut app = AppConfig::minimal(name, "/bin/sh");
        app.interpreter = Some("none".to_string());
        app.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
        app
    };
    let started = client
        .request(Request::Start { apps: vec![sleeper("one"), sleeper("two")] })
        .await;
    let Response::Started(infos) = started.result.unwrap() else { panic!("expected started") };
    let pids: Vec<i32> = infos
        .iter()
        .map(|i| i32::try_from(i.pid.expect("a real spawn reports a real pid")).unwrap())
        .collect();
    assert_eq!(pids.len(), 2);

    let killed = client.request(Request::KillDaemon).await;
    assert_eq!(killed.result.unwrap(), Response::ShuttingDown);

    let socket = fixture.paths.socket.clone();
    let pidfile_path = shep_daemon::boot::pidfile(&fixture.paths);
    tokio::time::timeout(RECV_TIMEOUT, fixture.run)
        .await
        .expect("teardown must not hang")
        .unwrap()
        .unwrap();

    assert!(!socket.exists(), "the control socket must be unlinked on teardown");
    assert!(!pidfile_path.exists(), "the pidfile must be removed on teardown");

    // Neither child may survive teardown: poll kill(pid, None) for ESRCH
    // (no such process) instead of sleeping a fixed guess.
    for pid in pids {
        let reaped = tokio::time::timeout(RECV_TIMEOUT, async {
            loop {
                match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                    Err(nix::errno::Errno::ESRCH) => break,
                    _ => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await;
        assert!(reaped.is_ok(), "pid {pid} must be reaped by teardown's kill ladder");
    }

    // Engine unreachable: a fresh connect on the now-unlinked socket path
    // must fail, not hang or succeed against a daemon that never really left.
    assert!(
        UnixStream::connect(&socket).await.is_err(),
        "the daemon must not still be answering after KillDaemon"
    );
}
```

5. **`a_socket_left_behind_by_a_crash_does_not_block_the_next_boot`** — boot, abort `run()` without teardown so the socket file survives, boot again on the same `$SHEP_HOME`, and complete a `Ping` — the reboot-resurrect leg of spec §13.4.

6. **`muster_restores_the_flock_across_a_daemon_lifetime`** (adversarial finding #5 — full code; the §13.4 flagship migration scenario end to end, minus the reboot):

```rust
#[tokio::test]
async fn muster_restores_the_flock_across_a_daemon_lifetime() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let mut alpha = AppConfig::minimal("alpha", "/bin/sh");
    alpha.interpreter = Some("none".to_string());
    alpha.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
    let mut beta = AppConfig::minimal("beta", "/bin/sh");
    beta.interpreter = Some("none".to_string());
    beta.instances = 2;
    beta.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
    let started = client.request(Request::Start { apps: vec![alpha, beta] }).await;
    let Response::Started(before) = started.result.unwrap() else { panic!("expected started") };
    assert_eq!(before.len(), 3, "alpha (1 instance) + beta (2 instances)");
    let old_pids: std::collections::HashSet<u32> =
        before.iter().map(|i| i.pid.unwrap()).collect();

    // Explicit write, no polling: the roll write is a call, not a race.
    fixture.ctx.snapshot_now().await.unwrap();
    let roll = shep_daemon::snapshot::read(&fixture.paths.snapshot).unwrap();
    let running_by_name: std::collections::HashMap<_, _> =
        roll.apps.iter().map(|a| (a.app.name.clone(), a.instances_running)).collect();
    assert_eq!(running_by_name.get("alpha"), Some(&1));
    assert_eq!(running_by_name.get("beta"), Some(&2));

    let dir = fixture.shutdown().await; // same $SHEP_HOME survives the reboot

    let rebooted = Fixture::boot(dir, true).await;
    let listed = rebooted.connect().await.request(Request::ListFlock).await;
    let Response::Flock(after) = listed.result.unwrap() else { panic!("expected flock") };
    assert_eq!(after.len(), 3, "both apps' full instance counts must come back");
    for info in &after {
        assert_eq!(info.status, ProcStatus::Online);
        let pid = info.pid.expect("a restored sheep is a real live process");
        assert!(!old_pids.contains(&pid), "a restored sheep gets a fresh pid, id {}", info.id);
    }
    rebooted.shutdown().await;
}
```

7. **`a_bare_interpreter_resolves_via_the_inherited_path`** (adversarial finding #1 — regression: without Task 8's `base_env()` fix, this spawn fails with `SpawnFailed`/ENOENT because `assemble`'s env carries no `PATH` and `tokio_runner.rs` clears the daemon's own env before applying it):

```rust
#[tokio::test]
async fn a_bare_interpreter_resolves_via_the_inherited_path() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // "sh" with no leading `/` — Command::new("sh") only finds this via
    // PATH, which is exactly what assemble()'s base_env() must seed for
    // env_clear()+envs(&spec.env) to still resolve it (adversarial finding
    // #1). An absolute-path test (as every other e2e test here uses) cannot
    // catch a PATH regression, on purpose that gap is what this test closes.
    let mut app = AppConfig::minimal("bare", "-c");
    app.interpreter = Some("sh".to_string());
    app.args = vec!["echo shep-bare-interpreter-ok".to_string()];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else { panic!("expected started") };
    let id = infos[0].id;

    // A failed exec (ENOENT from an empty PATH) lands the sheep in Errored,
    // never Online — reaching Online is the load-bearing assertion.
    let online = client.await_process_event(id, ProcessEventKind::Online).await;
    assert_eq!(online.status, ProcStatus::Online);

    fixture.shutdown().await;
}
```

- [ ] **Step 1:** write all seven tests (RED — `daemon_e2e.rs` does not compile until Task 7's surface exists; it should exist by now. Test 7 additionally stays RED against a not-yet-fixed Task 8 if the tasks are done out of order).
- [ ] **Step 2:** implement whatever gaps the tests expose in `boot.rs`/`server.rs`; do not weaken the tests to fit.
- [ ] **Step 3:** focused (`cargo test -p shep-daemon --test daemon_e2e`) + four gates. Run the file twice back to back to catch leaked sockets or orphaned children.
- [ ] **Step 4:** commit `test(daemon): end-to-end tier — real socket, real children, muster round trip` + footer.

---

### Task 11: Crate docs, security writeup, and polish

**Files:** Modify `crates/shep-daemon/src/lib.rs`, `server.rs` (doc only), `docs/specs/shep-v1.md` if a spec↔implementation drift was found.

- [ ] **Step 1:** Extend the crate mini-guide (IR-27) with the plane's taxonomy — h5 groups: *engine* (runner/brain/backoff/assemble/kill/supervisor, from 2a), *plane* (bus/rpc/server/snapshot/boot), *platform* (sys/privilege, unix-only). Add a second `# Quick start` block, ```no_run```, booting a daemon on a temp home and pinging it over the socket. Keep every intra-doc link pointing at `pub` items only (docs CI is `-Dwarnings --all-features`).
- [ ] **Step 2:** Finish the canonical `# Security` block on `RpcServer` (IR-29) and replace the security prose everywhere else with a link to it. Cross-check each claim against the code that enforces it: 0700 dirs (Task 6), same-uid peer check (Task 5), 0600 roll (Task 3), frame cap + bounded selectors/globs, clamped deadlines, `SHEP_READY_FD` trust boundary (Task 7).
- [ ] **Step 3:** `#[inline]` audit per IR-25 on the new trivial accessors (`TopicFilter::patterns`, `SnapshotWriter::writes`, `Credentials` field access); no `#[inline(always)]` anywhere in this phase — nothing here is per-frame hot except `TopicFilter::matches`, which is a `GlobSet` call, not a forwarding one.
- [ ] **Step 4:** Re-run the minimal-versions rehearsal (deps changed since Task 2), then the four gates plus `cargo +nightly doc --workspace --all-features --no-deps` with `RUSTDOCFLAGS="-Dwarnings --cfg docsrs"`, and `cargo test -p shep-daemon --no-default-features` + `--all-features` (the CI feature ladder).
- [ ] **Step 5:** commit `docs(daemon): plane taxonomy, canonical security writeup, inline audit` + footer.

## Phase 2b exit criteria

- Eleven tasks committed; four gates green on every one; docs job green with `-Dwarnings --all-features`.
- Spec §6 fully covered: UDS transport, shared framing, typed handshake with skew refusal, `Envelope`/`Reply` dispatch with clamped per-call deadlines, `Subscribe` with server-side glob filtering, bounded per-subscriber queue with drop-oldest + `Dropped{count}`, same-uid peer-cred auth, stale-socket recovery. Client reconnect backoff is the one §6 line deferred, to shep-client (Phase 3).
- Spec §3 layout honored: 0700 dirs, pidfile, `flock.json` written atomically at 0600, readiness pipe handshake ready for the CLI's `daemon` subcommand.
- 2a's two assigned deferrals closed: uid/gid spawn with a documented privilege-drop design, and the supervisor proptest (IR-37).
- Deterministic tiers stay paused-clock and fake-driven; every socket- or child-driven test is real-time and says why; no test counts scheduler ticks.
- Exactly one `unsafe` block in the workspace, in `sys.rs`, with a `// SAFETY:` comment and the IR-24 rationale essay.
- The DoD §13.4 muster scenario is proven end to end (boot → start → roll → shutdown → boot → restored), and the stale-socket leg of it has its own test.
