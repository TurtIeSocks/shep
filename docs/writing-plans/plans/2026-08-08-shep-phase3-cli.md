# shep Phase 3 — CLI wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
> **REQUIRED SKILL:** invoke `shep-idiomatic-rust` before writing ANY Rust here. Cite rules as `IR-<n>`.

**Goal:** Build `shep-client` (async RPC client + programmatic API) and `shep-cli` (the `shep` binary) on top of the merged daemon, so a user can start, inspect, and stop a flock from the command line.

**Architecture:** `shep-client` owns one connection actor per `Client`: it performs the handshake, demultiplexes `ServerFrame::Reply` to per-request oneshots and `ServerFrame::Event` to subscriber channels, and exposes `connect_or_spawn` for the autostart path. `shep-cli` is a clap-derive tree whose leaves are thin — parse args, call one client method, hand the result to a renderer. The `shep daemon` subcommand boots the merged `shep_daemon::boot` in the foreground; the parent detaches it into its own process group and then waits for a real handshake to succeed.

**Tech Stack:** tokio, tokio-util (codec), clap 4 (derive), clap_complete (aot), futures-util, serde/serde_json, assert_cmd + predicates (dev).

---

## Global Constraints

Every task's requirements implicitly include this section. Values here are exact and are not to be "improved".

**Rust / workspace**
- MSRV **1.88**, edition **2024**, resolver 3.
- Workspace lints deny `missing_docs` and `missing_debug_implementations`. Every new public item needs a doc comment and a deliberate `Debug` decision.
- `clippy::undocumented_unsafe_blocks` is denied workspace-wide.
- Any new dependency: `default-features = false`, features named explicitly, and a `-Z minimal-versions` rehearsal. If the rehearsal fails, add a floor pin to `[workspace.dependencies]` **with a comment explaining the exact API that forced it** — match the existing comment style in the root `Cargo.toml`.

**The unsafe boundary — non-negotiable, this is the point of the phase's design**
- `crates/shep-client/src/lib.rs` and `crates/shep-cli/src/main.rs` both carry `#![forbid(unsafe_code)]`.
- The CLI **must not** set the `SHEP_READY_FD` environment variable and **must not** call `shep_daemon::sys::adopt_fd`. Readiness is established by a successful handshake, not by an inherited descriptor.
- Do not delete, edit, or "clean up" `shep-daemon/src/sys.rs`, `BootOptions::ready_fd`, `DaemonReady`, IR-22, or IR-7. They become dead code in this phase **by design**; retiring them is Rin's decision and is explicitly out of scope. If you notice they are unused, that is the expected outcome — say so in your report, change nothing.

**Readiness — the trap this phase exists to avoid**
- A bound-but-not-accepting unix socket still completes `connect()` into the kernel backlog. **A bare `connect()` is therefore not a readiness probe.** The probe is: connect, send `Hello`, receive a `HelloAck`. Only that counts as ready.
- Retry schedule, from spec §6: **backoff 100ms, ×1.5, capped at 5s**, against a total deadline of **30s** for the spawn-and-wait path.
- The daemon binds its socket before it restores the muster roll and before `RunningDaemon::run` starts accepting. A large roll can therefore delay the first accept by seconds. The 30s total deadline exists for exactly this; do not shorten it.

**Wire and schema stability**
- `--format json` output is a stability surface. Every command's JSON gets a committed fixture and an insta snapshot, same discipline as the wire protocol (IR-35), and a CHANGELOG entry on change.
- Additive evolution only: new fields are additive, removing or retyping a field is a `schema_version` bump.

**Style (from docs/idiomatic-rust.md — cite by number in reviews)**
- `impl core::error::Error`, never `std::error::Error` (IR-19). Per-module error enums whose variant docs state the precise condition (IR-18).
- Every `Result`-returning public fn carries a `# Errors` section (IR-28).
- `# Panics` and `#[track_caller]` travel together, or neither appears (IR-21).
- No panicking constructors outside `shep-cli` (IR-21). Inside `shep-cli`, a panic on genuinely impossible internal state is acceptable; a panic on user input is not.
- Public `Stream` types are named, not `impl Stream` (IR-15).
- Async trait methods returning futures use RPITIT with an explicit `+ Send` bound — AFIT is not `Send`-provable. (This rule is stated in the Phase 2a/2b plans' Global Constraints; it is not a numbered IR rule. Do not cite it as IR-9 — IR-9 is the unrelated clippy `doc-valid-idents` rule.)
- Types carrying env or secrets get a manual redacted `Debug` plus an exact-string test (IR-41).
- Tests: paused tokio clock where time matters, no sleeps as synchronization, hand-rolled fakes, unique fixtures per test (IR-33, IR-34).

**Terminology (docs/terminology.md)**
- `sheep` = one managed process, singular only. The plural is **flock**, never "sheeps".
- `bleats` = logs (`logs` is a first-class alias). `fold` = group. `muster` = the saved roll.
- Straight verbs (`start`/`stop`/`list`) stay first-class aliases. Destructive operations and all error text stay plain English — the theme never costs clarity. `shep delete` says "delete".

**Gates — every one from its OWN exit code, no pipelines that swallow status**
```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace
cargo test --workspace --all-features
RUSTDOCFLAGS="-Dwarnings --cfg docsrs" cargo doc --workspace --all-features --no-deps
cargo check -p shep-cli --all-targets --target x86_64-pc-windows-gnu
cargo test --workspace --all-features -- --test-threads=1
```

**Explicitly out of scope for Phase 3** — do not build these, do not stub them into the clap tree:
`reload`, `scale` (no `Request` variant exists), `muster` CLI verb (restore is a boot flag; `muster save` would need a new RPC), `--with-env` (`ProcessInfo` carries no `env` field — it needs additive wire work first), dynamic shell completion (clap_complete's engine is `unstable-dynamic` upstream), dogs, `lookout`, `whistle`, `serve`, `import`, `dev`, `runtime`, `signal`, `sendline`, `trigger`, `startup`, `set`/`get`/`unset`.

---

## File Structure

```
crates/shep-client/
  Cargo.toml                 deps: shep-core, tokio, tokio-util, futures-util, bytes, serde_json
  src/lib.rs                 #![forbid(unsafe_code)], crate docs, re-export shep-core, pub mods
  src/error.rs               ClientError
  src/connection.rs          raw framed connection + handshake (steps 1-4 of the wire sequence)
  src/actor.rs               the demux task: Reply -> oneshot by id, Event -> subscriber channel
  src/client.rs              Client handle: request / subscribe / close
  src/events.rs              EventStream (named Stream type, IR-15)
  src/spawn.rs               connect_or_spawn + the launcher callback contract

crates/shep-cli/
  Cargo.toml                 deps: shep-core, shep-client, shep-daemon, clap, clap_complete, tokio, serde_json
  src/main.rs                #![forbid(unsafe_code)], #[tokio::main], dispatch, exit-code mapping
  src/cli.rs                 clap derive tree: Cli, Commands, GlobalArgs
  src/exit.rs                ExitCode enum + From<RpcErrorCode>
  src/output/mod.rs          OutputEnvelope, Render trait, format dispatch
  src/output/table.rs        table renderer
  src/commands/lifecycle.rs  start, stop, restart, delete
  src/commands/query.rs      flock, describe, ping
  src/commands/bleats.rs     bleats/logs follow
  src/commands/admin.rs      kill, completions
  src/commands/daemon.rs     the hidden `daemon` subcommand: boot in the foreground
  src/launch.rs              spawning `shep daemon` detached, for connect_or_spawn
  tests/cli_e2e.rs           assert_cmd end-to-end, fresh $SHEP_HOME per test
```

---

### Task 1: shep-client foundation — errors and the handshake

**Files:**
- Modify: `crates/shep-client/Cargo.toml`
- Modify: `crates/shep-client/src/lib.rs`
- Create: `crates/shep-client/src/error.rs`
- Create: `crates/shep-client/src/connection.rs`

**Interfaces:**
- Consumes: `shep_core::protocol::{codec, encode_frame, decode_frame, Hello, HelloAck, HelloReply, PROTOCOL_VERSION, RpcError, RpcErrorCode, WireError}`
- Produces:
```rust
pub enum ClientError {
    Connect { path: PathBuf, source: std::io::Error },
    Io(std::io::Error),
    Wire(shep_core::protocol::WireError),
    HandshakeClosed,
    ProtocolMismatch { daemon: u32, client: u32, message: String },
    Rpc(shep_core::protocol::RpcError),
    Closed,
    Timeout { after: Duration },
}

pub(crate) struct Connection {
    frames: Framed<UnixStream, LengthDelimitedCodec>,
    ack: HelloAck,
}
impl Connection {
    pub(crate) async fn open(socket: &Path) -> Result<Self, ClientError>;
    pub(crate) fn ack(&self) -> &HelloAck;
}
```

The daemon's `ProtocolMismatch` arrives as `HelloReply::Err(RpcError { code: ProtocolMismatch, .. })` and the connection then closes. Surface it as the dedicated `ClientError::ProtocolMismatch` variant, not as a generic `Rpc` — the CLI renders it with an upgrade hint and it has its own exit code.

- [ ] **Step 1: Add dependencies**

In `crates/shep-client/Cargo.toml`:

```toml
[dependencies]
shep-core.workspace = true
tokio = { workspace = true, features = ["net", "rt", "time", "sync", "macros"] }
tokio-util.workspace = true
futures-util.workspace = true
bytes.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["rt-multi-thread", "test-util"] }
tempfile.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing handshake tests**

In `connection.rs`. The fake daemon is a bare `UnixListener` that speaks the wire by hand — do NOT pull in `shep-daemon` to test the client.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::{codec, decode_frame, encode_frame, Hello, HelloAck, HelloReply};
    use tokio::net::UnixListener;

    /// Serves exactly one connection, replying with `reply`, then closes.
    async fn fake_daemon(path: PathBuf, reply: HelloReply) -> tokio::task::JoinHandle<Hello> {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut frames = Framed::new(stream, codec());
            let first = frames.next().await.unwrap().unwrap();
            let hello: Hello = decode_frame(&first).unwrap();
            frames.send(encode_frame(&reply).unwrap()).await.unwrap();
            hello
        })
    }

    #[tokio::test]
    async fn open_sends_hello_and_returns_the_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let ack = HelloAck { daemon_version: "9.9.9".into(), protocol: PROTOCOL_VERSION, pid: 4242 };
        let served = fake_daemon(path.clone(), Ok(ack.clone())).await;

        let conn = Connection::open(&path).await.unwrap();

        assert_eq!(conn.ack(), &ack);
        let hello = served.await.unwrap();
        assert_eq!(hello.protocol, PROTOCOL_VERSION, "the client must announce the version it speaks");
        assert!(!hello.client_version.is_empty(), "the client must identify its own version");
    }

    #[tokio::test]
    async fn a_protocol_refusal_becomes_its_own_error_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let refusal = RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 2, client speaks 1".into(),
        };
        let _served = fake_daemon(path.clone(), Err(refusal)).await;

        let err = Connection::open(&path).await.unwrap_err();

        let ClientError::ProtocolMismatch { message, .. } = err else {
            panic!("a protocol refusal must not be flattened into a generic Rpc error, got {err:?}");
        };
        assert!(message.contains("protocol 2"), "the daemon's own message must survive: {message}");
    }

    #[tokio::test]
    async fn a_daemon_that_closes_without_answering_is_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        assert!(matches!(
            Connection::open(&path).await,
            Err(ClientError::HandshakeClosed)
        ));
    }

    #[tokio::test]
    async fn connecting_to_a_missing_socket_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let ClientError::Connect { path: reported, .. } = Connection::open(&path).await.unwrap_err()
        else {
            panic!("a missing socket must report which path failed");
        };
        assert_eq!(reported, path);
    }
}
```

- [ ] **Step 3: Run them, confirm they fail**

Run: `cargo test -p shep-client`
Expected: FAIL — `Connection` and `ClientError` do not exist.

- [ ] **Step 4: Implement `ClientError`**

`core::error::Error` (IR-19), one variant per precise condition (IR-18), `Display` in plain English with no sheep theme.

- [ ] **Step 5: Implement `Connection::open`**

Connect, wrap in `Framed::new(stream, codec())`, send `encode_frame(&Hello { client_version: env!("CARGO_PKG_VERSION").to_string(), protocol: PROTOCOL_VERSION })`, read one frame, decode as `HelloReply`. `None` from the stream means the peer closed — `HandshakeClosed`, never a silent success. Document `# Errors` for every variant reachable here (IR-28).

- [ ] **Step 6: Run tests, confirm they pass, then commit**

```bash
cargo test -p shep-client
git add crates/shep-client && git commit -m "feat(client): connection handshake with typed protocol-mismatch refusal"
```

---

### Task 2: The connection actor — request/reply with frame demultiplexing

**Files:**
- Create: `crates/shep-client/src/actor.rs`
- Create: `crates/shep-client/src/client.rs`
- Modify: `crates/shep-client/src/lib.rs`

**Interfaces:**
- Consumes: `Connection` (Task 1), `shep_core::protocol::{Envelope, Reply, Request, Response, ServerFrame, BusEvent}`
- Produces:
```rust
pub struct Client { /* mpsc to the actor, plus the HelloAck */ }
impl Client {
    pub async fn connect(socket: &Path) -> Result<Self, ClientError>;
    pub fn daemon(&self) -> &HelloAck;
    pub async fn request(&self, body: Request) -> Result<Response, ClientError>;
    pub async fn request_with_deadline(&self, body: Request, deadline: Option<Duration>)
        -> Result<Response, ClientError>;
    pub async fn close(self) -> Result<(), ClientError>;
}
```

**Why an actor rather than the daemon's own e2e requeue loop:** the e2e client in `daemon_e2e.rs` requeues unmatched frames because it is single-task and synchronous. A shared `Client` cannot do that — two concurrent `request` calls would each need to hold frames destined for the other. One owning task that routes `Reply` by id to a per-request oneshot, and `Event` to the subscriber channel, removes the problem structurally instead of managing it.

**The race this must survive:** the supervisor emits a sheep's bus event *before* it resolves the RPC reply that caused it (`daemon_e2e.rs:161-174`). An `Event` frame therefore legitimately arrives ahead of the `Reply` for the very request that produced it. The actor must route it and keep reading, never treat it as an out-of-order protocol violation.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn two_concurrent_requests_each_get_their_own_reply() {
    // Fake daemon replies to id 1 with Pong and id 2 with Flock(vec![]) — DELIBERATELY
    // out of order (2 first) to prove routing is by id, not by arrival order.
    let (client, _served) = fake_client_out_of_order().await;
    let (a, b) = tokio::join!(
        client.request(Request::Ping),
        client.request(Request::ListFlock),
    );
    assert!(matches!(a.unwrap(), Response::Pong));
    assert!(matches!(b.unwrap(), Response::Flock(f) if f.is_empty()));
}

#[tokio::test]
async fn an_event_arriving_before_its_own_reply_does_not_break_the_request() {
    // Fake daemon sends BusEvent::Process{..} FIRST, then Reply{id:1}. This is the real
    // supervisor's ordering, not a contrived one — see daemon_e2e.rs:161-174.
    let (client, _served) = fake_client_event_then_reply().await;
    assert!(matches!(client.request(Request::Ping).await.unwrap(), Response::Pong));
}

#[tokio::test]
async fn a_daemon_side_error_reply_becomes_ClientError_Rpc() {
    let (client, _served) = fake_client_replying_err(RpcErrorCode::NotFound, "no sheep matched").await;
    let ClientError::Rpc(err) = client.request(Request::ListFlock).await.unwrap_err() else {
        panic!("an Err reply must surface as ClientError::Rpc");
    };
    assert_eq!(err.code, RpcErrorCode::NotFound);
}

#[tokio::test]
async fn a_dropped_connection_fails_pending_requests_instead_of_hanging() {
    let (client, served) = fake_client_that_closes_after_handshake().await;
    served.await.unwrap();
    assert!(matches!(client.request(Request::Ping).await, Err(ClientError::Closed)));
}

#[tokio::test(start_paused = true)]
async fn a_deadline_expires_client_side_when_the_daemon_never_answers() {
    let (client, _served) = fake_client_that_never_replies().await;
    let err = client
        .request_with_deadline(Request::Ping, Some(Duration::from_millis(250)))
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::Timeout { .. }));
}
```

- [ ] **Step 2: Run, confirm failure.** Expected: `Client` does not exist.

- [ ] **Step 3: Implement the actor**

One `tokio::spawn`ed task owning the `Framed`. Commands arrive on an `mpsc`; it holds `HashMap<u64, oneshot::Sender<Result<Response, ClientError>>>` and a `broadcast::Sender<BusEvent>`. `tokio::select!` between the command channel and the frame stream. On stream end or error: drain the map, failing every pending request with `ClientError::Closed` — never leave a caller hanging.

Request ids come from a monotonic counter owned by the actor, not the caller. `deadline_ms` on the `Envelope` is the daemon-side budget; the client-side `tokio::time::timeout` is separate and must be at least as long, or the client gives up on a request the daemon is still honouring.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Prove the routing test actually gates**

Temporarily change the actor to resolve replies in arrival order rather than by id. Confirm `two_concurrent_requests_each_get_their_own_reply` FAILS. Restore, confirm it passes. Paste both transcripts into your report — a routing test that passes under order-based resolution is testing nothing.

- [ ] **Step 6: Commit**

```bash
git commit -m "feat(client): connection actor routing replies by id and events to subscribers"
```

---

### Task 3: EventStream — the subscription surface

**Files:**
- Create: `crates/shep-client/src/events.rs`
- Modify: `crates/shep-client/src/client.rs`, `crates/shep-client/src/actor.rs`

**Interfaces:**
- Produces:
```rust
/// Live bus events for one subscription. Named rather than `impl Stream` (IR-15).
pub struct EventStream { /* broadcast::Receiver<BusEvent> */ }
impl futures_util::Stream for EventStream { type Item = Result<BusEvent, Lagged>; }

/// The local receiver fell behind and the client-side buffer dropped events.
/// Distinct from `BusEvent::Dropped`, which is the DAEMON's own drop notice.
pub struct Lagged { pub count: u64 }

impl Client {
    pub async fn subscribe(&self, topics: Vec<String>) -> Result<EventStream, ClientError>;
}
```

Two different drop mechanisms exist and must not be conflated: `BusEvent::Dropped { count }` is the daemon telling us its per-subscriber queue overflowed, and arrives as a normal event. `Lagged` is our own `broadcast::Receiver` falling behind. Surface both; document the difference on `Lagged`.

A second `Subscribe` on one connection **replaces** the daemon-side filter rather than adding to it (`server.rs:360-364`). Document that on `subscribe` — a caller wanting two topic sets needs two `Client`s.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn subscribe_yields_events_the_daemon_pushes() { /* fake pushes 3 LogOut, assert 3 arrive in order */ }

#[tokio::test]
async fn the_subscribed_reply_arrives_before_any_event() {
    // server.rs:357 guarantees this ordering; the client must not deadlock waiting
    // for a Subscribed reply that it has already routed past.
}

#[tokio::test]
async fn a_daemon_shutdown_event_ends_the_stream_cleanly() {
    // DaemonShutdown is delivered as an event, THEN the socket closes.
    // The consumer must see the event before the stream ends — not lose it to the close.
}

#[tokio::test]
async fn a_lagging_consumer_reports_Lagged_rather_than_silently_skipping() { }
```

- [ ] **Step 2-4:** Run/fail, implement, run/pass. `EventStream` wraps `BroadcastStream`-style polling; map `RecvError::Lagged(n)` to `Lagged { count: n }` and `RecvError::Closed` to end-of-stream.

- [ ] **Step 5: Commit** — `feat(client): named EventStream with daemon-drop and local-lag distinguished`

---

### Task 4: connect_or_spawn — autostart without an inherited descriptor

**Files:**
- Create: `crates/shep-client/src/spawn.rs`
- Modify: `crates/shep-client/src/lib.rs`

**Interfaces:**
- Produces:
```rust
/// How long the spawn-and-wait path will keep probing before giving up.
pub const SPAWN_DEADLINE: Duration = Duration::from_secs(30);
/// First retry gap; grows ×1.5 up to `BACKOFF_CAP` (spec §6).
pub const BACKOFF_START: Duration = Duration::from_millis(100);
/// Ceiling for the retry gap (spec §6).
pub const BACKOFF_CAP: Duration = Duration::from_secs(5);

pub enum SpawnOutcome { Connected(Client), Spawned(Client) }

pub async fn connect_or_spawn<L>(socket: &Path, launch: L) -> Result<SpawnOutcome, ClientError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send;
```

**The contract, in order:**
1. Try `Client::connect`. Success → `Connected`. This is the overwhelmingly common path and must not pay any spawn cost.
2. On `ClientError::Connect` (only — a `ProtocolMismatch` is a real answer from a real daemon and must propagate immediately, never trigger a spawn), call `launch()`.
3. Loop until `SPAWN_DEADLINE`: sleep the current backoff, then attempt a **full handshake**. Success → `Spawned`.
4. Between attempts, `try_wait()` the child. If it has exited, stop immediately and report its status — do not burn the remaining 30 seconds probing a corpse.
5. If the child exited with the "another daemon already holds this `$SHEP_HOME`" outcome, that is **not** a failure: another process won the race and a daemon is live. Keep probing until the deadline. The daemon's `flock(2)` already makes concurrent boots safe (Phase 2b), so the client needs no lock of its own — it only needs to not misread `AlreadyRunning` as fatal.

**Why the probe is a handshake and not a `connect`:** a bound-but-not-accepting socket accepts connections into the backlog. The daemon binds at `boot.rs:498` but does not accept until `RunningDaemon::run` reaches `.serve()` at `boot.rs:707`, after the muster restore. A `connect`-only probe therefore returns success against a daemon that cannot yet answer, and the very next request hangs.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_existing_daemon_is_used_without_launching_anything() {
    let launched = Arc::new(AtomicBool::new(false));
    // ... fake daemon already listening ...
    let outcome = connect_or_spawn(&path, || { launched.store(true, SeqCst); unreachable!() }).await.unwrap();
    assert!(matches!(outcome, SpawnOutcome::Connected(_)));
    assert!(!launched.load(SeqCst), "a live daemon must never be re-spawned");
}

#[tokio::test]
async fn a_socket_that_accepts_but_never_handshakes_is_not_mistaken_for_ready() {
    // THE load-bearing test of this task. Bind a listener and never accept.
    // connect() will succeed into the backlog; the handshake will not complete.
    let listener = UnixListener::bind(&path).unwrap(); // bound, never accepted from
    let child = spawn_a_child_that_exits_immediately();
    let err = connect_or_spawn(&path, || Ok(child)).await.unwrap_err();
    assert!(!matches!(err, ClientError::Closed), "a backlogged connect must not read as ready");
    drop(listener);
}

#[tokio::test]
async fn a_child_that_dies_fails_fast_instead_of_waiting_out_the_deadline() {
    let started = Instant::now();
    let err = connect_or_spawn(&absent_path, || Ok(child_exiting_with(3))).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "must not wait out SPAWN_DEADLINE on a dead child");
    // and the error must name the child's exit status
}

#[tokio::test]
async fn a_protocol_mismatch_propagates_instead_of_spawning_a_second_daemon() {
    // A daemon that refuses on version skew is still a daemon. Spawning another
    // would be wrong AND would fail identically.
}
```

Note on the third test: it asserts wall-clock behaviour, so it must run on a real clock, not `start_paused`. Keep it that way and keep the bound generous (5s against a 30s deadline) so it cannot flake on a loaded CI box.

- [ ] **Step 2-4:** Run/fail, implement, run/pass.

- [ ] **Step 5: Prove the handshake-probe test gates**

Replace the handshake probe with a bare `UnixStream::connect`. Confirm `a_socket_that_accepts_but_never_handshakes_is_not_mistaken_for_ready` FAILS. Restore, confirm it passes. Both transcripts into the report. This is the single most important gate in the phase.

- [ ] **Step 6: Commit** — `feat(client): connect_or_spawn probing with a real handshake, not a bare connect`

---

### Task 5: CLI skeleton — clap tree, exit codes, main

**Files:**
- Modify: `crates/shep-cli/Cargo.toml`, `crates/shep-cli/src/main.rs`
- Create: `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/exit.rs`

**Interfaces:**
- Produces:
```rust
#[derive(Debug, clap::Parser)]
#[command(name = "shep", version, about = "A shepherd for your processes")]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, clap::Args)]
pub struct GlobalArgs {
    /// Override $SHEP_HOME for this invocation
    #[arg(long, global = true, env = "SHEP_HOME")]
    pub home: Option<PathBuf>,
    /// Output format
    #[arg(long, global = true, value_enum, default_value_t = Format::Table)]
    pub format: Format,
    /// Suppress non-essential output
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format { Table, Json }

#[derive(Debug, clap::Subcommand)]
pub enum Commands {
    Start(StartArgs),
    Stop(SelectorArgs),
    Restart(SelectorArgs),
    Delete(SelectorArgs),
    #[command(visible_aliases = ["list", "ls"])]
    Flock,
    Describe(SelectorArgs),
    #[command(visible_alias = "logs")]
    Bleats(BleatsArgs),
    Ping,
    Kill,
    Completions(CompletionArgs),
    /// Graceful stop. Easter-egg alias for `stop`.
    #[command(hide = true)]
    Thatlldo(SelectorArgs),
    /// Run the supervisor in the foreground. Spawned by the CLI; not for direct use.
    #[command(hide = true)]
    Daemon(DaemonArgs),
}
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u8)]
pub enum ExitCode {
    Success = 0,
    Failure = 1,
    Usage = 2,
    NotFound = 3,
    InvalidConfig = 4,
    DaemonUnreachable = 5,
    ProtocolMismatch = 6,
    SpawnFailed = 7,
    DeadlineExceeded = 8,
    Internal = 9,
}
impl From<RpcErrorCode> for ExitCode { /* infallible, total */ }
impl From<&ClientError> for ExitCode { }
```

`Usage = 2` is clap's own convention and collides with the exit code spec §9 reserves for `runtime`'s fail-fast. `runtime` is not in this phase. Take clap's 2 now and record the collision in the CHANGELOG so the `runtime` phase resolves it deliberately rather than discovering it.

`Thatlldo` is hidden — spec §9 calls it an easter egg, and only `resurrect` is explicitly named hidden there, so this is a judgment call. It behaves exactly as `Stop`; implement it by delegating, not by duplicating.

- [ ] **Step 1: Add dependencies**

```toml
clap = { version = "4", default-features = false, features = ["std", "derive", "help", "usage", "error-context", "suggestions", "env", "wrap_help"] }
clap_complete = { version = "4", default-features = false }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "signal", "net", "time", "sync"] }
serde_json.workspace = true
```

`default-features = false` on clap drops `color`; add it back only if a reviewer asks. Run the minimal-versions rehearsal for both new crates and pin floors with a reason comment if it fails.

- [ ] **Step 2: Write the failing tests**

```rust
#[test]
fn the_command_tree_parses_and_is_internally_consistent() {
    use clap::CommandFactory;
    Cli::command().debug_assert(); // clap's own structural self-check
}

#[test]
fn list_and_ls_both_reach_flock() {
    for argv in [["shep", "flock"], ["shep", "list"], ["shep", "ls"]] {
        assert!(matches!(Cli::try_parse_from(argv).unwrap().command, Commands::Flock));
    }
}

#[test]
fn format_defaults_to_table_and_accepts_json() { }

#[test]
fn every_rpc_error_code_maps_to_a_distinct_nonzero_exit_code() {
    // Guards against a future RpcErrorCode variant silently defaulting to Failure.
    let codes = [NotFound, InvalidConfig, SpawnFailed, ProtocolMismatch, Internal, DeadlineExceeded];
    let mapped: Vec<u8> = codes.iter().map(|c| ExitCode::from(*c) as u8).collect();
    assert!(mapped.iter().all(|&c| c != 0), "no error may map to Success");
    let unique: std::collections::HashSet<_> = mapped.iter().collect();
    assert_eq!(unique.len(), mapped.len(), "distinct causes need distinct exit codes: {mapped:?}");
}
```

- [ ] **Step 3-5:** Run/fail, implement, run/pass. `main` is `#[tokio::main]`, calls `run(cli) -> ExitCode`, and ends with `std::process::exit(code as i32)`. Keep `main` itself trivial — all logic in `run` so it is testable.

- [ ] **Step 6: Commit** — `feat(cli): clap tree, global args, and the exit-code taxonomy`

---

### Task 6: Output — one envelope, two renderings, one source of truth

**Files:**
- Create: `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/src/output/table.rs`

**Interfaces:**
- Produces:
```rust
/// Bumped only for a breaking change to any command's `data` shape.
/// Additive fields do not bump it.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct OutputEnvelope<'a, T> {
    pub schema_version: u32,
    pub command: &'a str,
    pub data: T,
}

/// Implemented once per command payload. The two methods are the ONLY place a
/// field's presence is decided, so a field added to one and forgotten in the
/// other is a compile error rather than a silent divergence.
pub trait Render: Serialize {
    /// Column headers for table output.
    fn headers() -> &'static [&'static str];
    /// One row per record, cells in `headers()` order.
    fn rows(&self) -> Vec<Vec<String>>;
}
```

**Keeping the two renderings honest** is the real design problem here. `Serialize` and `rows()` can drift silently. The plan's answer: a test per payload type that serializes a fully-populated value, collects its JSON object keys, and asserts they match `headers()` after a documented name mapping. A new field added to the struct fails that test until it is either added to `headers()` or explicitly listed in the payload's `JSON_ONLY` allowlist with a reason.

**Stream discipline:** rendered output goes to stdout; diagnostics, progress, and errors go to stderr. Under `--format json` an error is *also* a JSON object on stderr — `{"schema_version", "error": {"code", "message"}}` — so a script piping stdout gets clean data and a script capturing stderr gets a parseable failure. Never mix the two on one stream.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_json_envelope_shape_is_pinned() {
    let out = OutputEnvelope { schema_version: SCHEMA_VERSION, command: "flock", data: sample_flock() };
    insta::assert_json_snapshot!(out);
}

#[test]
fn every_serialized_field_is_either_a_column_or_explicitly_json_only() {
    // The anti-drift gate. Fails when a new field appears in Serialize but not in headers().
}

#[test]
fn table_and_json_report_the_same_record_count() { }

#[test]
fn an_error_under_format_json_is_a_json_object_on_stderr() { }
```

- [ ] **Step 2-4:** Run/fail, implement, run/pass.

- [ ] **Step 5: Prove the anti-drift test gates**

Add a throwaway field to a payload struct. Confirm `every_serialized_field_is_either_a_column_or_explicitly_json_only` FAILS. Remove it, confirm green. Transcript into the report.

- [ ] **Step 6: Commit** — `feat(cli): versioned output envelope with drift-gated table and JSON renderings`

---

### Task 7: The hidden `daemon` subcommand and the detached launcher

**Files:**
- Create: `crates/shep-cli/src/commands/daemon.rs`, `crates/shep-cli/src/launch.rs`

**Interfaces:**
- Consumes: `shep_daemon::{boot::{boot, BootOptions, BootError}, tokio_runner::TokioRunner}`, `shep_core::paths::ShepPaths`
- Produces:
```rust
#[derive(Debug, clap::Args)]
pub struct DaemonArgs {
    /// Restore the saved muster roll on boot
    #[arg(long, default_value_t = true)]
    pub restore: bool,
}

/// Runs the supervisor in this process until a signal or `KillDaemon`.
pub async fn run_daemon(paths: ShepPaths, args: DaemonArgs) -> Result<(), BootError>;

/// Spawns `shep daemon` detached from this process's group and terminal.
/// Returns the child so the caller can `try_wait()` it while probing.
pub fn launch_daemon(paths: &ShepPaths) -> std::io::Result<std::process::Child>;
```

`run_daemon` constructs `BootOptions { socket: None, ready_fd: None, restore: args.restore }` — `ready_fd` stays `None`, deliberately and permanently, per this plan's Global Constraints. Then `boot(TokioRunner::new(), paths, options).await?.run().await`.

`launch_daemon` uses `std::env::current_exe()` and `Command::process_group(0)` (stable since 1.64 via `CommandExt`, no unsafe) so the daemon survives the parent exiting and its terminal closing. Redirect the child's stdout and stderr to files under `paths.logs` — inheriting the parent's terminal would spray daemon output over the user's shell after the CLI returns.

**Do not** implement a classic double-fork. `process_group(0)` plus redirected stdio achieves what this needs without any `unsafe`, and `#![forbid(unsafe_code)]` in this crate is not negotiable.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_launcher_never_sets_the_readiness_fd_variable() {
    // Reads the Command's configured env. SHEP_READY_FD must be absent —
    // this is the invariant the whole phase design rests on.
}

#[tokio::test]
async fn run_daemon_passes_ready_fd_none() {
    // Assert on the constructed BootOptions, not on boot's behaviour.
}

#[test]
fn the_child_is_placed_in_its_own_process_group() { }
```

- [ ] **Step 2-4:** Run/fail, implement, run/pass.

- [ ] **Step 5: Commit** — `feat(cli): foreground daemon subcommand and its detached launcher`

---

### Task 8: Lifecycle verbs — start, stop, restart, delete

**Files:**
- Create: `crates/shep-cli/src/commands/lifecycle.rs`

**Interfaces:**
- Consumes: `shep_core::config::{flockfile::{Flockfile, FlockFormat, discover}, app::AppConfig}`, `shep_core::selector::ProcessSelector`, `Client`, `connect_or_spawn`
- Produces:
```rust
#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// A script path, a Flockfile, or `-` to read Flockfile JSON from stdin
    pub target: String,
    /// Name for this sheep (script form only)
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub fold: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct SelectorArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>`
    pub selector: String,
}
```

`start` resolves its target in this order: `-` → read stdin as Flockfile JSON; a path whose extension `FlockFormat::from_path` recognises → parse as a Flockfile; any other existing path → a single `AppConfig::minimal(name_or_file_stem, path)`; nothing matched → a usage error naming what was tried. Do not widen this grammar (spec fidelity — the skill's checklist calls out input-format widening as a top drift risk).

Selectors go through `ProcessSelector::parse` and then `SelectorSpec::from(&parsed)`. Parse client-side even though the daemon re-parses, so a malformed selector is a fast local usage error rather than a round trip.

`stop`/`restart`/`delete` are the same shape: parse selector, one request, render. `NotFound` from the daemon is a real outcome with its own exit code, not an error to swallow.

- [ ] **Step 1: Write the failing tests** — target resolution (all four branches incl. the failure), selector round-trip, `NotFound` exit code, and stdin JSON.
- [ ] **Step 2-4:** Run/fail, implement, run/pass.
- [ ] **Step 5: Commit** — `feat(cli): start, stop, restart, and delete`

---

### Task 9: Query verbs — flock, describe, ping

**Files:**
- Create: `crates/shep-cli/src/commands/query.rs`

`flock` renders the `Vec<ProcessInfo>` table: id, name, status, pid, restarts, uptime, fold. `describe` takes a selector. `ping` reports the daemon's version and pid from the `HelloAck` the client already holds — it must NOT issue a `Request::Ping` round trip to learn something the handshake already told it. (Still issue the `Ping` request itself as a liveness check; just source the version and pid from the ack.)

Uptime renders as a human duration in table mode and as raw `uptime_ms` in JSON — a formatted string is not a machine-readable field.

- [ ] **Step 1: Write the failing tests** — empty-flock renders headers not a bare blank; uptime formats; JSON keeps `uptime_ms` numeric.
- [ ] **Step 2-4:** Run/fail, implement, run/pass.
- [ ] **Step 5: Commit** — `feat(cli): flock, describe, and ping`

---

### Task 10: bleats — following the log stream

**Files:**
- Create: `crates/shep-cli/src/commands/bleats.rs`

**Interfaces:**
```rust
#[derive(Debug, clap::Args)]
pub struct BleatsArgs {
    /// Which sheep (default: all)
    #[arg(default_value = "all")]
    pub selector: String,
    /// Keep streaming (default: true; --no-follow for a one-shot drain)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub follow: bool,
    /// Only stderr
    #[arg(long, conflicts_with = "out")]
    pub err: bool,
    /// Only stdout
    #[arg(long, conflicts_with = "err")]
    pub out: bool,
}
```

**`BusEvent::LogOut`/`LogErr` carry only `{ id, line }` — no name.** Resolve ids to names with one `ListFlock` before subscribing, and cache it. An id that appears later and is not in the cache renders as the bare id rather than blocking on a refresh; do not issue a `ListFlock` per unknown line.

Filtering by selector happens **client-side** on the resolved id set: the daemon's topic filter globs on the topic string (`log.out`), which carries no identity. Say so in a comment — the next reader will assume the daemon filtered.

Ctrl-C: `tokio::select!` the stream against `tokio::signal::ctrl_c()`, flush stdout, exit `Success`. A user ending a follow deliberately has not failed.

If the daemon shuts down mid-follow, print the `DaemonShutdown` notice to stderr and exit `DaemonUnreachable` — the stream ending because the daemon went away is materially different from the user pressing Ctrl-C.

- [ ] **Step 1: Write the failing tests** — id→name resolution incl. the unknown-id fallback, `--err`/`--out` filtering, client-side selector filtering, Ctrl-C exits Success, mid-stream shutdown exits DaemonUnreachable.
- [ ] **Step 2-4:** Run/fail, implement, run/pass.
- [ ] **Step 5: Commit** — `feat(cli): bleats log following with client-side identity resolution`

---

### Task 11: kill and static completions

**Files:**
- Create: `crates/shep-cli/src/commands/admin.rs`

`kill` sends `Request::KillDaemon`, expects `Response::ShuttingDown`, and then — per the wire sequence — that connection closes while the daemon finishes teardown. Do not report success on the reply alone: poll for the socket file to disappear (bounded, a few seconds), so `shep kill && shep start` cannot race the old daemon's unlink. If the poll times out, report that teardown is still in progress rather than claiming a clean stop.

`completions <shell>` uses `clap_complete::generate` with `Cli::command()`. Static only. Add a one-line note in the generated help that sheep-name completion is not yet dynamic.

- [ ] **Step 1: Write the failing tests** — `kill` waits for the socket to go; completions generate non-empty output for bash/zsh/fish.
- [ ] **Step 2-4:** Run/fail, implement, run/pass.
- [ ] **Step 5: Commit** — `feat(cli): daemon shutdown and static shell completions`

---

### Task 12: End-to-end tier, JSON fixtures, CHANGELOG

**Files:**
- Create: `crates/shep-cli/tests/cli_e2e.rs`
- Create: `crates/shep-cli/tests/fixtures/*.json`
- Modify: `crates/shep-cli/CHANGELOG.md`, `crates/shep-client/CHANGELOG.md`

Real binary via `assert_cmd`, fresh `$SHEP_HOME` per test in a `tempfile::TempDir`. Copy the teardown discipline from `daemon_e2e.rs:43-152` — a `Drop` guard that kills the spawned daemon's process group. That guard was empirically proven load-bearing in Phase 2b (a panicking test leaked a real orphan without it); a CLI suite that spawns real daemons needs it at least as much.

Keep `$SHEP_HOME` shallow — the tempdir root itself, not a nested path. macOS caps `sun_path` around 104 bytes and a nested fixture path silently overruns it (`lib.rs:260-266`).

Required cases:
1. `shep start <script>` with no daemon running autostarts one, and the sheep reaches Online.
2. A second command reuses the daemon rather than spawning a second (assert one pid across both).
3. Two concurrent `shep start` invocations against a cold `$SHEP_HOME` produce exactly one daemon and no spurious error. This is the race Phase 2b's `flock(2)` makes safe; prove the client half is safe too.
4. `--format json` output validates against the committed fixture for `flock`, `describe`, and `start`.
5. Exit codes: a selector matching nothing exits `NotFound`; a malformed selector exits `Usage`; a command against a socket path in a nonexistent directory exits `DaemonUnreachable`.
6. `shep kill` stops the daemon and removes the socket.
7. `shep bleats --no-follow` drains buffered lines and exits `Success`.

- [ ] **Step 1: Write all seven, run, confirm they fail**
- [ ] **Step 2: Implement whatever wiring they expose as missing**
- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: Write both CHANGELOGs** — including the `Usage = 2` collision note for the future `runtime` phase.
- [ ] **Step 5: Commit** — `test(cli): end-to-end tier with autostart, concurrency, and pinned JSON fixtures`

---

## Exit criteria

1. All twelve tasks complete and individually reviewed.
2. Every gate in Global Constraints green from its own exit code.
3. `grep -rn "unsafe" crates/shep-client/src crates/shep-cli/src` returns nothing.
4. `grep -rn "SHEP_READY_FD\|adopt_fd" crates/shep-cli/src crates/shep-client/src` returns nothing.
5. The three revert-proof transcripts (Task 2 routing, Task 4 handshake probe, Task 6 anti-drift) are in the reports.
6. `shep start`, `shep flock`, `shep bleats`, `shep kill` work against a real daemon on a clean `$SHEP_HOME`.
7. A report to Rin listing: the now-dead readiness-pipe surface with evidence, and every judgment call made on her behalf.

## Open questions for Rin — do not resolve these unilaterally

1. **Retire the readiness pipe?** This phase makes `sys.rs`, `BootOptions::ready_fd`, `DaemonReady`, and `READY_FD_ENV` unreachable in production. Deleting them would let every crate in the workspace be `#![forbid(unsafe_code)]` and would retire IR-22 as satisfied-by-construction. Analysis: `docs/research/phase3-readiness-decision.md`.
2. **The bind→serve gap.** The daemon signals nothing between binding its socket and accepting on it, so a client can connect into the backlog and wait through the whole muster restore. Phase 3 absorbs this with a 30s deadline. The real fix is ordering — either start `serve()` before the restore, or move readiness to the point where accepting begins. Both change merged daemon behaviour, so neither is in this phase.
3. **`Usage = 2` collides** with spec §9's fail-fast code for `runtime`. Taken as clap's convention for now.
4. **`--with-env` cannot be built yet** — `ProcessInfo` has no `env` field. It needs an additive wire change first.
