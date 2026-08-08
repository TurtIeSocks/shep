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

**Platform tiering (spec §11 — CI-enforced)**
- The split follows `shep-daemon`'s merged precedent — the ruling comment at `crates/shep-daemon/Cargo.toml:34-40` and the four gated module declarations at `crates/shep-daemon/src/lib.rs:311` (`boot`), `:321` (`sys`), `:330` (`tokio_runner`), `:342` (`server`). The **pure tier** compiles on every target; the **OS tier** is `#[cfg(unix)]`. **Anything consuming `shep_daemon::{boot, sys, tokio_runner, server}` is OS tier by definition** — those four modules do not exist on a Windows build.
- This is not a plan-local rule. `.github/workflows/test.yml` puts `windows-latest` in the test matrix (line 32) and runs `cargo test --workspace --locked` on it (line 38). That leg is green on `main` today. A task that writes unconditional unix-only code does not merely fail this plan's gate — it fails CI, and deleting the gate would only move the failure there.
- **shep-client** — OS tier. Gate at the `mod` declaration in `lib.rs`, not with an inner `#![cfg(unix)]`, so each module's `#[cfg(test)]` block is excluded along with it: `connection`, `actor`, `client`, `events`, `spawn`. Pure tier: the crate docs and the `pub use shep_core` re-export, which is all that is left. **There is no portable error module** — fix J below puts each error enum in the module that produces it, and all of those are OS tier.
- **shep-cli** — pure tier, and its unit tests **must keep running on Windows**; that is what makes spec §11's "compiles + unit tests in CI from day one" mean anything. Pure: `cli.rs` (the entire clap tree, *including* the `Daemon` and `Completions` variants — the parse surface must not diverge by platform, so `Cli::command().debug_assert()` and the alias tests cover both), `exit.rs`'s `ExitCode` enum and its `From<RpcErrorCode>` impl, and `output/`. OS tier, `#[cfg(unix)]`: `launch.rs`, every module under `commands/`, the `From<&ConnectError>` / `From<&RequestError>` / `From<&SpawnError>` impls in `exit.rs`, and the dispatch arms in `main.rs` that call them.
- `run()` gets a `#[cfg(windows)]` arm that prints `shep does not yet support Windows` to stderr and returns `ExitCode::Failure`. `main` stays portable.
- `crates/shep-cli/tests/cli_e2e.rs` opens with `#![cfg(unix)]`. An integration test is its own compilation unit, so `--all-targets` and `cargo test --workspace` build it on Windows otherwise.

**The unsafe boundary — non-negotiable, this is the point of the phase's design**
- `crates/shep-client/src/lib.rs` and `crates/shep-cli/src/main.rs` both carry `#![forbid(unsafe_code)]`.
- The CLI **must not** set the `SHEP_READY_FD` environment variable and **must not** call `shep_daemon::sys::adopt_fd`. Readiness is established by a successful handshake, not by an inherited descriptor.
- Do not delete, edit, or "clean up" `shep-daemon/src/sys.rs`, `BootOptions::ready_fd`, `DaemonReady`, IR-22, or IR-7. They become dead code in this phase **by design**; retiring them is Rin's decision and is explicitly out of scope. If you notice they are unused, that is the expected outcome — say so in your report, change nothing.

**Readiness — the trap this phase exists to avoid**
- A bound-but-not-accepting unix socket still completes `connect()` into the kernel backlog. **A bare `connect()` is therefore not a readiness probe.** The probe is: connect, send `Hello`, receive a `HelloAck`. Only that counts as ready.
- **Every probe is bounded.** `Connection::open` takes a timeout and wraps the whole connect-plus-handshake in `tokio::time::timeout`. Without it the handshake read blocks forever against a backlogged socket, the retry loop never returns from its first attempt, and the 30s deadline — checked *between* attempts — never fires.
- Retry schedule, from spec §6: **backoff 100ms, ×1.5, capped at 5s**, against a total deadline of **30s** for the spawn-and-wait path, with a **5s** handshake timeout per attempt. Against a backlogged socket that admits roughly six attempts before the deadline. That is the intended behaviour, not an accident — do not "optimise" the handshake timeout down and reintroduce the hang.
- The daemon binds its socket before it restores the muster roll and before `RunningDaemon::run` starts accepting. A large roll can therefore delay the first accept by seconds. The 30s total deadline exists for exactly this; do not shorten it.

**Wire and schema stability**
- `--format json` output is a stability surface. Every command's JSON gets a committed fixture and an insta snapshot, same discipline as the wire protocol (IR-35), and a CHANGELOG entry on change.
- Additive evolution only: new fields are additive, removing or retyping a field is a `schema_version` bump.

**Style (from docs/idiomatic-rust.md — cite by number in reviews)**
- `impl core::error::Error`, never `std::error::Error` (IR-19). Per-module error enums whose variant docs state the precise condition (IR-18). No crate-wide umbrella error — `shep-daemon`'s merged precedent is `BootError`, `SnapshotError`, `RunnerError`, `SysError`, `ConnError`, one per module.
- Every `Result`-returning public fn carries a `# Errors` section (IR-28).
- `# Panics` and `#[track_caller]` travel together, or neither appears (IR-21).
- No panicking constructors outside `shep-cli` (IR-21). Inside `shep-cli`, a panic on genuinely impossible internal state is acceptable; a panic on user input is not.
- Public `Stream` types are named, not `impl Stream` (IR-15).
- `#[non_exhaustive]` goes on library-crate public enums where growth is expected (IR-20) — that means shep-client's three error enums. It does **not** go on `shep-cli`'s `ExitCode`: a binary crate has no downstream matcher for it to protect, so there it is noise.
- No magic numbers in prose or in code — a duration a reader has to guess at gets a named `const` with a comment (IR-26).
- Async trait methods returning futures use RPITIT with an explicit `+ Send` bound — AFIT is not `Send`-provable. (This rule is stated in the Phase 2a/2b plans' Global Constraints; it is not a numbered IR rule. Do not cite it as IR-9 — IR-9 is the unrelated clippy `doc-valid-idents` rule.)
- Types carrying env or secrets get a manual redacted `Debug` plus an exact-string test (IR-41).
- Tests: paused tokio clock where time matters, no sleeps as synchronization, hand-rolled fakes, unique fixtures per test (IR-33, IR-34).

**Terminology (docs/terminology.md)**
- `sheep` = one managed process, singular only. The plural is **flock**, never "sheeps".
- **"the shepherd" names the daemon and nothing else.** Do not use it for the binary, the project, or the CLI — including in the clap `about` string.
- `bleats` = logs (`logs` is a first-class alias). `fold` = group. `muster` = the saved roll.
- Straight verbs (`start`/`stop`/`list`) stay first-class aliases. Destructive operations and all error text stay plain English — the theme never costs clarity. `shep delete` says "delete".

**Gates — every one from its OWN exit code, no pipelines that swallow status**
```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace
cargo test --workspace --all-features
RUSTDOCFLAGS="-Dwarnings --cfg docsrs" cargo doc --workspace --all-features --no-deps
cargo check --workspace --all-targets --target x86_64-pc-windows-gnu
cargo test --workspace --all-features -- --test-threads=1
```

The Windows gate is `--workspace --all-targets`, not `-p shep-cli --all-targets`. Scoping it to one package builds shep-client's *lib* but not its *test* targets, so a `UnixListener` inside a `#[cfg(test)]` module would sail through this gate and detonate on CI instead.

**Explicitly out of scope for Phase 3** — do not build these, do not stub them into the clap tree:
`reload`, `scale` (no `Request` variant exists), `muster` CLI verb (restore is a boot flag; `muster save` would need a new RPC), `--with-env` (`ProcessInfo` carries no `env` field — it needs additive wire work first), dynamic shell completion (clap_complete's engine is `unstable-dynamic` upstream), **named-pipe transport / functional Windows RPC** (`ShepPaths::pipe_name()` already exists for that future work; do not wire it up, and do not build a cfg-aliased transport to make Windows "work" — Windows compiles and refuses, that is the whole deliverable), dogs, `lookout`, `whistle`, `serve`, `import`, `dev`, `runtime`, `signal`, `sendline`, `trigger`, `startup`, `set`/`get`/`unset`.

---

## File Structure

```
crates/shep-client/
  Cargo.toml                 deps: shep-core, tokio, tokio-util, futures-util, bytes, serde_json
  src/lib.rs                 #![forbid(unsafe_code)], IR-6 doctest attr, # Quick start, re-export
                             shep-core, cfg(unix)-gated mod declarations, and the crate-root
                             re-export surface every shep-cli task imports from
  src/connection.rs   [unix]  raw framed connection + bounded handshake; owns ConnectError
  src/actor.rs        [unix]  the demux task: Reply -> oneshot by id, Event -> subscriber channel
  src/client.rs       [unix]  Client handle: request / subscribe / close; owns RequestError
  src/events.rs       [unix]  EventStream (named Stream type, IR-15)
  src/spawn.rs        [unix]  connect_or_spawn + the launcher contract; owns SpawnError

crates/shep-cli/
  Cargo.toml                 deps: shep-core, shep-client, shep-daemon, clap, clap_complete, tokio, serde_json
  src/main.rs                #![forbid(unsafe_code)], #[tokio::main], dispatch, exit-code mapping
  src/cli.rs                 clap derive tree: Cli, Commands, GlobalArgs, and EVERY argument struct
  src/exit.rs                ExitCode enum + From<RpcErrorCode> (portable) + From<&*Error> [unix]
  src/output/mod.rs          OutputEnvelope, Render trait, format dispatch
  src/output/table.rs        table renderer
  src/commands/lifecycle.rs  [unix] start, stop, restart, delete
  src/commands/query.rs      [unix] flock, describe, fold, ping
  src/commands/bleats.rs     [unix] bleats/logs follow
  src/commands/admin.rs      [unix] kill, completions
  src/commands/daemon.rs     [unix] the hidden `daemon` subcommand: boot in the foreground
  src/launch.rs              [unix] spawning `shep daemon` detached, for connect_or_spawn
  tests/cli_e2e.rs           #![cfg(unix)] assert_cmd end-to-end, fresh $SHEP_HOME per test
```

There is no `shep-client/src/error.rs`. Each error enum lives in the module that produces it (IR-18).

**shep-client's crate-root re-export surface.** Every shep-cli task below imports from the crate root, not from module paths, so each shep-client task ends by adding its own public items to this list in `lib.rs` (all of it inside the same `#[cfg(unix)]` region as the modules):

```rust
#[cfg(unix)] pub use client::{Client, RequestError, DEADLINE_GRACE, DEFAULT_DEADLINE, START_DEADLINE};
#[cfg(unix)] pub use connection::{ConnectError, HANDSHAKE_TIMEOUT};
#[cfg(unix)] pub use events::{EventStream, Lagged};
#[cfg(unix)] pub mod spawn;   // SpawnError, SpawnOptions, SpawnOutcome, connect_or_spawn, the consts
```

`spawn` stays a public *module* rather than a flattened re-export because the exit-code contract (`spawn::DAEMON_ALREADY_RUNNING`) reads better qualified — it is a cross-crate agreement, not a convenience import.

---

### Task 1: shep-client foundation — the bounded handshake and `ConnectError`

**Files:**
- Modify: `crates/shep-client/Cargo.toml`
- Modify: `crates/shep-client/src/lib.rs`
- Create: `crates/shep-client/src/connection.rs`

**Interfaces:**
- Consumes: `shep_core::protocol::{codec, encode_frame, decode_frame, Hello, HelloAck, HelloReply, PROTOCOL_VERSION, RpcError, RpcErrorCode, WireError}`
- Produces:
```rust
/// Budget for one connect-plus-handshake attempt.
///
/// Deliberately mirrors the daemon's own `HANDSHAKE_TIMEOUT_MS = 5_000`
/// (`shep-daemon/src/server.rs:41`) so neither side out-waits the other.
/// Re-exported from `spawn` (Task 4), which is where the whole spawn budget
/// reads from one place.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Growth is expected — this is a library crate's public error type (IR-20).
#[non_exhaustive]
pub enum ConnectError {
    Connect { path: PathBuf, source: std::io::Error },
    Io(std::io::Error),
    Wire(shep_core::protocol::WireError),
    HandshakeClosed,
    HandshakeTimeout { after: Duration },
    ProtocolMismatch { daemon: u32, client: u32, message: String },
}

pub(crate) struct Connection {
    frames: Framed<UnixStream, LengthDelimitedCodec>,
    ack: HelloAck,
}
impl Connection {
    pub(crate) async fn open(socket: &Path, timeout: Duration) -> Result<Self, ConnectError>;
    pub(crate) fn ack(&self) -> &HelloAck;
}
```

`HANDSHAKE_TIMEOUT` is **defined here** rather than in `spawn.rs`, because Task 2's `Client::connect` needs it two tasks before `spawn.rs` exists. Task 4 re-exports it at `shep_client::spawn::HANDSHAKE_TIMEOUT`, which is the path the rest of this plan names.

`open` wraps connect **and** handshake in one `tokio::time::timeout`. Exceeding it is `HandshakeTimeout { after }` — a distinct condition from a refused connect, and the two must never collapse into each other: a refusal means nothing is listening, a timeout means something is bound but not answering yet. Task 4 branches on exactly that difference.

The daemon's `ProtocolMismatch` arrives as `HelloReply::Err(RpcError { code: ProtocolMismatch, .. })` and the connection then closes. Surface it as the dedicated `ConnectError::ProtocolMismatch` variant, not as a generic error — the CLI renders it with an upgrade hint and it has its own exit code.

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

- [ ] **Step 2: Set up `lib.rs`**

shep-client becomes a real public API this phase, so its crate root matches shep-core's (`crates/shep-core/src/lib.rs:7-21`): the IR-6 doctest attribute `#![doc(test(attr(deny(warnings))))]`, `#![forbid(unsafe_code)]`, and an IR-27 `# Quick start` doctest. Gate the module declarations, not the module bodies:

```rust
#![doc(test(attr(deny(warnings))))]
#![forbid(unsafe_code)]

// Unix-only: built on `tokio::net::UnixStream`, and — via `spawn` — on the
// exit-code contract of a `shep daemon` child. Gated at the `mod` line so
// each module's own `#[cfg(test)]` block goes with it; an inner
// `#![cfg(unix)]` would leave the declaration visible and the tests behind.
// Platform tiering follows shep-daemon's ruling — see this plan's Global
// Constraints and `shep-daemon/Cargo.toml:34-40`.
#[cfg(unix)]
mod connection;
```

The `# Quick start` doctest must compile on Windows, where none of the gated modules exist. Write it against the portable surface only (`shep_core` re-exports), or mark it ```` ```no_run ```` and `#[cfg(unix)]`-guard nothing — a doctest that names `Client` will fail the Windows gate.

- [ ] **Step 3: Write the failing handshake tests**

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

        let conn = Connection::open(&path, HANDSHAKE_TIMEOUT).await.unwrap();

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

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT).await.unwrap_err();

        let ConnectError::ProtocolMismatch { message, .. } = err else {
            panic!("a protocol refusal must not be flattened into a generic error, got {err:?}");
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
            Connection::open(&path, HANDSHAKE_TIMEOUT).await,
            Err(ConnectError::HandshakeClosed)
        ));
    }

    #[tokio::test]
    async fn connecting_to_a_missing_socket_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let ConnectError::Connect { path: reported, .. } =
            Connection::open(&path, HANDSHAKE_TIMEOUT).await.unwrap_err()
        else {
            panic!("a missing socket must report which path failed");
        };
        assert_eq!(reported, path);
    }

    /// The bound-but-never-accepted case, at the `Connection` layer. The
    /// kernel completes `connect()` into the backlog, so only the timeout
    /// ends this. Real timings would make it a 5s test; 150ms proves the
    /// same thing.
    #[tokio::test]
    async fn a_socket_bound_but_never_accepted_from_times_out_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let _listener = UnixListener::bind(&path).unwrap(); // bound; never accepted from

        let err = Connection::open(&path, Duration::from_millis(150)).await.unwrap_err();

        let ConnectError::HandshakeTimeout { after } = err else {
            panic!("a backlogged connect must time out, not hang or read as success; got {err:?}");
        };
        assert_eq!(after, Duration::from_millis(150));
    }
}
```

- [ ] **Step 4: Run them, confirm they fail**

Run: `cargo test -p shep-client`
Expected: FAIL — `Connection` and `ConnectError` do not exist.

- [ ] **Step 5: Implement `ConnectError`**

`core::error::Error` (IR-19), `#[non_exhaustive]` (IR-20), one variant per precise condition (IR-18), `Display` in plain English with no sheep theme. `source()` returns the inner `io::Error` / `WireError` where one exists.

- [ ] **Step 6: Implement `Connection::open`**

```rust
tokio::time::timeout(timeout, async { /* connect, Framed, Hello, read one frame */ })
    .await
    .map_err(|_| ConnectError::HandshakeTimeout { after: timeout })?
```

Inside: connect, wrap in `Framed::new(stream, codec())`, send `encode_frame(&Hello { client_version: env!("CARGO_PKG_VERSION").to_string(), protocol: PROTOCOL_VERSION })`, read one frame, decode as `HelloReply`. `None` from the stream means the peer closed — `HandshakeClosed`, never a silent success. Document `# Errors` for every variant reachable here (IR-28).

- [ ] **Step 7: Run tests, confirm they pass, then commit**

```bash
cargo test -p shep-client
cargo check --workspace --all-targets --target x86_64-pc-windows-gnu
git add crates/shep-client && git commit -m "feat(client): bounded connection handshake with typed protocol-mismatch refusal"
```

---

### Task 2: The connection actor — request/reply with frame demultiplexing

**Files:**
- Create: `crates/shep-client/src/actor.rs`
- Create: `crates/shep-client/src/client.rs`
- Modify: `crates/shep-client/src/lib.rs`

**Interfaces:**
- Consumes: `Connection`, `ConnectError`, `HANDSHAKE_TIMEOUT` (all Task 1), `shep_core::protocol::{Envelope, Reply, Request, Response, ServerFrame, BusEvent, RpcError}`
- Produces:
```rust
/// Daemon-side budget applied when a caller names none. Mirrors the daemon's
/// own `DEFAULT_DEADLINE_MS = 5_000` (`shep-daemon/src/rpc.rs:36`), which is
/// what an `Envelope` with `deadline_ms: None` would get anyway — stated here
/// so the value is a decision, not an inheritance.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);

/// Budget for `Request::Start`. A cold spawn plus a readiness probe routinely
/// outruns the 5s default, and the daemon clamps anything over
/// `MAX_DEADLINE_MS = 60_000` (`shep-daemon/src/rpc.rs:38`), so this is well
/// inside what the daemon will honour.
pub const START_DEADLINE: Duration = Duration::from_secs(30);

/// How much longer the client waits than the deadline it asked the daemon to
/// honour. Without a gap the client abandons a request the daemon is still
/// legitimately working on, and the user sees a timeout for work that
/// succeeded (IR-26: named, not a magic `+ 2`).
pub const DEADLINE_GRACE: Duration = Duration::from_secs(2);

/// Growth is expected — library-crate public error type (IR-20).
#[non_exhaustive]
pub enum RequestError {
    Rpc(shep_core::protocol::RpcError),
    Timeout { after: Duration },
    Closed,
    Wire(shep_core::protocol::WireError),
}

pub struct Client { /* mpsc to the actor, plus the HelloAck */ }
impl Client {
    pub async fn connect(socket: &Path) -> Result<Self, ConnectError>;
    pub async fn connect_with_timeout(socket: &Path, timeout: Duration) -> Result<Self, ConnectError>;
    pub fn daemon(&self) -> &HelloAck;
    pub async fn request(&self, body: Request) -> Result<Response, RequestError>;
    pub async fn request_with_deadline(&self, body: Request, deadline: Option<Duration>)
        -> Result<Response, RequestError>;
    pub async fn close(self) -> Result<(), RequestError>;
}
```

`connect` uses `HANDSHAKE_TIMEOUT`; `connect_with_timeout` exists for callers that need another value (Task 4's fast tests are the first).

**Two error types, on purpose.** `connect` returns `ConnectError` and `request` returns `RequestError` — a failure to reach the daemon and a failure of a request the daemon accepted are different things with different exit codes, and a single enum spanning both forces every call site to match on variants it can never see. This is IR-18, and it matches shep-daemon's merged shape (`BootError`, `SnapshotError`, `RunnerError`, `SysError`, `ConnError` — one per module, no umbrella).

**Deadlines, both of them.** `request` sends `Envelope { deadline_ms: Some(DEFAULT_DEADLINE.as_millis()), .. }` — it does **not** send `None` and inherit the daemon's default silently. The client-side `tokio::time::timeout` is a second, separate bound set to `deadline + DEADLINE_GRACE`.

**Why an actor rather than the daemon's own e2e requeue loop:** the e2e client in `daemon_e2e.rs` requeues unmatched frames because it is single-task and synchronous. A shared `Client` cannot do that — two concurrent `request` calls would each need to hold frames destined for the other. One owning task that routes `Reply` by id to a per-request oneshot, and `Event` to the subscriber channel, removes the problem structurally instead of managing it.

**The race this must survive:** the supervisor emits a sheep's bus event *before* it resolves the RPC reply that caused it (`daemon_e2e.rs:161-174` documents this, empirically, from the daemon side). An `Event` frame therefore legitimately arrives ahead of the `Reply` for the very request that produced it. The actor must route it and keep reading, never treat it as an out-of-order protocol violation.

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
async fn a_daemon_side_error_reply_becomes_RequestError_Rpc() {
    let (client, _served) = fake_client_replying_err(RpcErrorCode::NotFound, "no sheep matched").await;
    let RequestError::Rpc(err) = client.request(Request::ListFlock).await.unwrap_err() else {
        panic!("an Err reply must surface as RequestError::Rpc");
    };
    assert_eq!(err.code, RpcErrorCode::NotFound);
}

#[tokio::test]
async fn a_dropped_connection_fails_pending_requests_instead_of_hanging() {
    let (client, served) = fake_client_that_closes_after_handshake().await;
    served.await.unwrap();
    assert!(matches!(client.request(Request::Ping).await, Err(RequestError::Closed)));
}

#[tokio::test(start_paused = true)]
async fn a_deadline_expires_client_side_when_the_daemon_never_answers() {
    let (client, _served) = fake_client_that_never_replies().await;
    let err = client
        .request_with_deadline(Request::Ping, Some(Duration::from_millis(250)))
        .await
        .unwrap_err();
    let RequestError::Timeout { after } = err else { panic!("expected a client-side timeout, got {err:?}") };
    assert_eq!(after, Duration::from_millis(250) + DEADLINE_GRACE);
}

/// Nothing else here reads the envelope the client actually sent, so an
/// implementation that always sends `deadline_ms: None` would pass every
/// test above while silently inheriting the daemon's default for every verb.
/// This is the same gap the Phase 2b whole-branch review caught daemon-side;
/// it does not ship again client-side.
#[tokio::test]
async fn every_request_carries_an_explicit_deadline_on_the_wire() {
    // fake_client_capturing_envelopes returns a channel of the decoded
    // `Envelope`s the fake daemon received, in arrival order.
    let (client, envelopes) = fake_client_capturing_envelopes().await;

    let _ = client.request(Request::Ping).await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(
        sent.deadline_ms,
        Some(u64::try_from(DEFAULT_DEADLINE.as_millis()).unwrap()),
        "request() must state its deadline, not inherit the daemon's default silently"
    );

    let _ = client
        .request_with_deadline(Request::Ping, Some(START_DEADLINE))
        .await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(sent.deadline_ms, Some(u64::try_from(START_DEADLINE.as_millis()).unwrap()));
}
```

- [ ] **Step 2: Run, confirm failure.** Expected: `Client` does not exist.

- [ ] **Step 3: Implement the actor**

One `tokio::spawn`ed task owning the `Framed`. Commands arrive on an `mpsc`; it holds `HashMap<u64, oneshot::Sender<Result<Response, RequestError>>>` and a `broadcast::Sender<BusEvent>`. `tokio::select!` between the command channel and the frame stream. On stream end or error: drain the map, failing every pending request with `RequestError::Closed` — never leave a caller hanging.

Request ids come from a monotonic counter owned by the actor, not the caller.

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
- Modify: `crates/shep-client/src/client.rs`, `crates/shep-client/src/actor.rs`, `crates/shep-client/src/lib.rs`

**Interfaces:**
- Consumes: `Client`, `RequestError` (Task 2), `shep_core::protocol::{BusEvent, ProcessEventKind, Request, Response}`
- Produces:
```rust
/// Live bus events for one subscription. Named rather than `impl Stream` (IR-15).
pub struct EventStream { /* broadcast::Receiver<BusEvent> */ }
impl futures_util::Stream for EventStream { type Item = Result<BusEvent, Lagged>; }

/// The local receiver fell behind and the client-side buffer dropped events.
/// Distinct from `BusEvent::Dropped`, which is the DAEMON's own drop notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lagged { pub count: u64 }

impl Client {
    pub async fn subscribe(&self, topics: Vec<String>) -> Result<EventStream, RequestError>;
}
```

Two different drop mechanisms exist and must not be conflated: `BusEvent::Dropped { count }` is the daemon telling us its per-subscriber queue overflowed, and arrives as a normal event on the stream. `Lagged` is our own `broadcast::Receiver` falling behind, and arrives as an `Err` item. Surface both; document the difference on `Lagged`.

A second `Subscribe` on one connection **replaces** the daemon-side filter rather than adding to it (`shep-daemon/src/server.rs:358-364`). Document that on `subscribe` — a caller wanting two topic sets needs two `Client`s.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn subscribe_yields_events_the_daemon_pushes() {
    let (client, daemon) = fake_client_with_push().await;
    let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

    for i in 0..3u32 {
        daemon.push(BusEvent::LogOut { id: 1, line: format!("line {i}") }).await;
    }

    for i in 0..3u32 {
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event, BusEvent::LogOut { id: 1, line: format!("line {i}") },
            "events must arrive in push order");
    }
}

/// `server.rs:357` sends the `Subscribed` reply ahead of any event, by queue
/// order. The client must have routed that reply before the first event
/// reaches the stream — an implementation that waits for the reply *after*
/// installing the subscriber deadlocks against a daemon that pushes fast.
#[tokio::test]
async fn the_subscribed_reply_arrives_before_any_event() {
    let (client, daemon) = fake_client_with_push().await;
    daemon.queue_reply_then_event(
        Response::Subscribed,
        BusEvent::Process {
            event: ProcessEventKind::Online,
            info: sample_info(),
            manually: true,
            at_ms: 0,
        },
    );

    let mut stream = tokio::time::timeout(
        Duration::from_secs(1),
        client.subscribe(vec!["process.*".into()]),
    )
    .await
    .expect("subscribe must not deadlock behind its own reply")
    .unwrap();

    assert!(matches!(
        stream.next().await.unwrap().unwrap(),
        BusEvent::Process { event: ProcessEventKind::Online, .. }
    ));
}

/// `RunningDaemon::run` sends `DaemonShutdown` on the bus (boot.rs:719) and
/// only then closes the sockets. The consumer must see the event before the
/// stream ends — losing it to the close is how `bleats` (Task 10) would
/// report a clean end-of-stream for a daemon that actually went away.
#[tokio::test]
async fn a_daemon_shutdown_event_ends_the_stream_cleanly() {
    let (client, daemon) = fake_client_with_push().await;
    let mut stream = client.subscribe(vec!["daemon.*".into()]).await.unwrap();

    daemon.push(BusEvent::DaemonShutdown).await;
    daemon.close().await;

    assert_eq!(stream.next().await.unwrap().unwrap(), BusEvent::DaemonShutdown);
    assert!(stream.next().await.is_none(), "the stream ends after the notice, not before it");
}

#[tokio::test]
async fn a_lagging_consumer_reports_Lagged_rather_than_silently_skipping() {
    // Capacity is small and known; push capacity+N without polling, then poll.
    let (client, daemon) = fake_client_with_push().await;
    let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

    for i in 0..(EVENT_CHANNEL_CAPACITY + 8) {
        daemon.push(BusEvent::LogOut { id: 1, line: i.to_string() }).await;
    }

    let Some(Err(Lagged { count })) = stream.next().await else {
        panic!("an overrun must be reported, never silently skipped");
    };
    assert!(count > 0, "the lag notice must say how many were lost");
}
```

- [ ] **Step 2: Run, confirm failure.** Expected: `EventStream`, `Lagged`, and `Client::subscribe` do not exist.

- [ ] **Step 3: Implement**

`EventStream` wraps the actor's `broadcast::Receiver<BusEvent>` and polls it in `Stream::poll_next`: map `RecvError::Lagged(n)` to `Poll::Ready(Some(Err(Lagged { count: n })))` and `RecvError::Closed` to `Poll::Ready(None)`. Name the channel capacity as a `const EVENT_CHANNEL_CAPACITY: usize` (IR-26) with a comment tying it to the daemon's own `CONN_QUEUE = 64` (`shep-daemon/src/server.rs:39`).

`subscribe` issues `Request::Subscribe { topics }`, awaits `Response::Subscribed`, and hands back a receiver the actor has already been feeding — the receiver is created *before* the request is sent, which is what makes the reply-then-event ordering test pass rather than deadlock.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(client): named EventStream with daemon-drop and local-lag distinguished`

---

### Task 4: connect_or_spawn — autostart without an inherited descriptor

**Files:**
- Create: `crates/shep-client/src/spawn.rs`
- Modify: `crates/shep-client/src/lib.rs`

**Interfaces:**
- Consumes: `Client`, `ConnectError`, `HANDSHAKE_TIMEOUT` (Tasks 1-2)
- Produces:
```rust
/// How long the spawn-and-wait path will keep probing before giving up.
pub const SPAWN_DEADLINE: Duration = Duration::from_secs(30);
/// First retry gap; grows ×1.5 up to `BACKOFF_CAP` (spec §6).
pub const BACKOFF_START: Duration = Duration::from_millis(100);
/// Ceiling for the retry gap (spec §6).
pub const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// The per-attempt handshake budget, defined in `connection` and surfaced
/// here so the whole spawn budget reads from one place.
pub use crate::connection::HANDSHAKE_TIMEOUT;

/// Exit status a `shep daemon` child uses when another daemon already holds
/// this `$SHEP_HOME` (`shep-cli`'s `ExitCode::DaemonAlreadyRunning`).
///
/// This couples the client to the CLI's exit-code taxonomy, deliberately:
/// the client cannot inspect a `BootError` across a process boundary, and an
/// exit status is the only channel a dead child leaves behind. Changing
/// either side without the other reintroduces the race in `SpawnError`'s doc.
pub const DAEMON_ALREADY_RUNNING: i32 = 10;

/// Every timing `connect_or_spawn` obeys, injectable so tests do not spend
/// 30 wall-clock seconds proving a probe is bounded.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub deadline: Duration,
    pub backoff_start: Duration,
    pub backoff_cap: Duration,
    pub handshake_timeout: Duration,
}

/// The four consts above, in field order.
impl Default for SpawnOptions { /* SPAWN_DEADLINE, BACKOFF_START, BACKOFF_CAP, HANDSHAKE_TIMEOUT */ }

#[derive(Debug)]
pub enum SpawnOutcome { Connected(Client), Spawned(Client) }

/// Growth is expected — library-crate public error type (IR-20).
#[non_exhaustive]
pub enum SpawnError {
    Connect(ConnectError),
    Launch(std::io::Error),
    DaemonExited { status: std::process::ExitStatus },
    DeadlineExpired { after: Duration, last: Option<ConnectError> },
}

pub async fn connect_or_spawn<L>(socket: &Path, launch: L) -> Result<SpawnOutcome, SpawnError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send;

pub async fn connect_or_spawn_with<L>(socket: &Path, launch: L, opts: SpawnOptions)
    -> Result<SpawnOutcome, SpawnError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send;
```

`connect_or_spawn` delegates to `connect_or_spawn_with(socket, launch, SpawnOptions::default())`. Production code calls the short form and never names a timing.

`DeadlineExpired` carries the **last** probe failure. A bare "timed out" tells a user nothing about why — whether nothing was ever listening, or something was listening and never answered, is the whole diagnosis.

**The contract, in order:**
1. Try `Client::connect_with_timeout(socket, opts.handshake_timeout)`. Success → `Connected`. This is the overwhelmingly common path and must not pay any spawn cost.
2. On `ConnectError::Connect` — and only that — call `launch()`. Nothing is listening, so nothing is there to disturb.
3. On `ConnectError::HandshakeTimeout` from the *initial* probe, do **not** launch: something is bound and not yet answering, which is a daemon in the bind→serve gap (see Open Question 2). Enter the probe loop against the same deadline without spawning a second daemon.
4. On any other `ConnectError` — `ProtocolMismatch` above all — propagate immediately as `SpawnError::Connect`. A daemon that refuses on version skew is still a daemon; spawning another would be wrong and would fail identically.
5. Loop until `opts.deadline`: sleep the current backoff, then attempt a **full handshake** with `opts.handshake_timeout`. Success → `Spawned`. Keep the most recent `ConnectError` for `DeadlineExpired`.
6. Between attempts, `try_wait()` the child. **Exit status `DAEMON_ALREADY_RUNNING` (10) is not a failure** — another process won the cold-start race and a daemon is live or coming live, so keep probing until the deadline. Any other non-zero status is fatal: stop immediately and return `DaemonExited { status }` rather than burning the remaining 30 seconds probing a corpse. The daemon's `flock(2)` already makes concurrent boots safe (Phase 2b), so the client needs no lock of its own — it only needs to not misread the loser as fatal.

**Why the probe is a handshake and not a `connect`:** a bound-but-not-accepting socket accepts connections into the backlog. The daemon binds at `boot.rs:498` but does not accept until `RunningDaemon::run` reaches `.serve()` at `boot.rs:707`, after the muster restore. A `connect`-only probe therefore returns success against a daemon that cannot yet answer, and the very next request hangs.

**Production defaults versus test values — two different tables, never conflated.**

| `SpawnOptions` field | production default (`Default`) | value `fast_opts()` uses in tests |
|---|---|---|
| `deadline` | `SPAWN_DEADLINE` = 30 s | 600 ms |
| `backoff_start` | `BACKOFF_START` = 100 ms | 10 ms |
| `backoff_cap` | `BACKOFF_CAP` = 5 s | 50 ms |
| `handshake_timeout` | `HANDSHAKE_TIMEOUT` = 5 s | 100 ms |

Only `a_child_that_dies_fails_fast_instead_of_waiting_out_the_deadline` runs on the production defaults — it is an assertion *about* the 30 s deadline, so shrinking it would delete the thing under test. Every other test in this task uses `fast_opts()` and finishes in well under a second.

- [ ] **Step 1: Write the failing tests**

```rust
/// Sub-second mirror of `SpawnOptions::default()`. Ratios preserved so the
/// backoff still grows and still caps; only the magnitudes shrink.
fn fast_opts() -> SpawnOptions {
    SpawnOptions {
        deadline: Duration::from_millis(600),
        backoff_start: Duration::from_millis(10),
        backoff_cap: Duration::from_millis(50),
        handshake_timeout: Duration::from_millis(100),
    }
}

/// A child that outlives the call and then dies on its own, with no sleep and
/// no orphan: `cat` blocks reading a piped stdin whose write end is owned by
/// the `Child`. When `connect_or_spawn_with` drops that `Child` on return,
/// the pipe closes, `cat` sees EOF and exits. Lifetime is tied exactly to the
/// call under test — a `sleep 60` would leak past it, and Phase 2b already
/// paid for that lesson (`daemon_e2e.rs:118-138`).
fn spawn_long_lived() -> std::io::Result<std::process::Child> {
    std::process::Command::new("cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
}

#[tokio::test]
async fn an_existing_daemon_is_used_without_launching_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _served = fake_daemon(path.clone(), Ok(sample_ack())).await; // Task 1's helper

    let outcome = connect_or_spawn_with(&path, || {
        unreachable!("a live daemon must never be re-spawned")
    }, fast_opts()).await.unwrap();
    assert!(matches!(outcome, SpawnOutcome::Connected(_)));
}

/// THE load-bearing test of this task.
///
/// The launcher does what a real cold start does: it makes a socket appear
/// that is BOUND but never accepted from — a daemon that has reached
/// `boot.rs:498` and not `boot.rs:707` — and returns a child that STAYS
/// ALIVE for the whole call. Both halves matter. If the child exited, the
/// dead-child fast path would short-circuit before any probe ran, and the
/// bare-`connect()` implementation this test exists to catch would pass.
#[tokio::test]
async fn a_socket_that_accepts_but_never_handshakes_is_not_mistaken_for_ready() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    // The listener must outlive the closure; park it where the test owns it.
    // `std`'s listener, not tokio's: the launcher is a sync `FnOnce`, and
    // this one must never accept anyway.
    let held: Arc<Mutex<Option<std::os::unix::net::UnixListener>>> = Arc::default();

    let err = {
        let slot = Arc::clone(&held);
        let bind_at = path.clone();
        connect_or_spawn_with(&path, move || {
            *slot.lock().unwrap() = Some(std::os::unix::net::UnixListener::bind(&bind_at)?);
            spawn_long_lived()
        }, fast_opts())
        .await
        .unwrap_err()
    };

    let SpawnError::DeadlineExpired { last: Some(ConnectError::HandshakeTimeout { .. }), after } = err
    else {
        panic!("a backlogged connect must read as an unfinished handshake, got {err:?}");
    };
    assert_eq!(after, fast_opts().deadline);
    assert!(held.lock().unwrap().is_some(), "the fixture must actually have bound the socket");
}

#[tokio::test]
async fn a_child_that_dies_fails_fast_instead_of_waiting_out_the_deadline() {
    let started = Instant::now();
    let err = connect_or_spawn(&absent_path, || child_exiting_with(3)).await.unwrap_err();
    let SpawnError::DaemonExited { status } = err else {
        panic!("a dead child's status must reach the caller, got {err:?}");
    };
    assert_eq!(status.code(), Some(3));
    assert!(started.elapsed() < Duration::from_secs(5), "must not wait out SPAWN_DEADLINE on a dead child");
}

/// The losing side of a cold-start race (fix G). The launcher starts a child
/// that immediately exits 10 AND brings up a daemon that answers — exactly
/// what happens when another `shep` process won the `flock(2)`. Treating any
/// non-zero status as fatal fails this test.
#[tokio::test]
async fn a_child_exiting_with_DAEMON_ALREADY_RUNNING_keeps_probing() {
    let outcome = connect_or_spawn_with(&path, || {
        start_fake_daemon_answering_on(&path); // binds AND accepts
        std::process::Command::new("sh").args(["-c", "exit 10"]).spawn()
    }, fast_opts()).await.unwrap();
    assert!(matches!(outcome, SpawnOutcome::Spawned(_)),
        "another process winning the race is not this process's failure");
}

#[tokio::test]
async fn a_protocol_mismatch_propagates_instead_of_spawning_a_second_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _served = fake_daemon(path.clone(), Err(RpcError {
        code: RpcErrorCode::ProtocolMismatch,
        message: "daemon speaks protocol 2, client speaks 1".into(),
    })).await;

    let err = connect_or_spawn_with(&path, || {
        unreachable!("a refusing daemon is still a daemon")
    }, fast_opts()).await.unwrap_err();
    assert!(matches!(err, SpawnError::Connect(ConnectError::ProtocolMismatch { .. })));
}

/// A daemon in the bind→serve gap must not provoke a second daemon.
#[tokio::test]
async fn a_bound_but_silent_socket_is_probed_not_respawned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

    let err = connect_or_spawn_with(&path, || {
        unreachable!("something is already bound here")
    }, fast_opts()).await.unwrap_err();

    assert!(matches!(err, SpawnError::DeadlineExpired { .. }));
}
```

Note on `a_child_that_dies_fails_fast_instead_of_waiting_out_the_deadline`: it asserts wall-clock behaviour against the production 30s deadline, so it must run on a real clock, not `start_paused`, and it must use `connect_or_spawn` (not `_with`). Keep the bound generous (5s against 30s) so it cannot flake on a loaded CI box. Every other test here runs on `fast_opts()` and finishes inside a second, so none of them may use `start_paused` either — they drive real sockets and a real child, and a paused clock would stall the timeout that is the whole point.

- [ ] **Step 2: Run, confirm failure.** Expected: `connect_or_spawn` and `SpawnError` do not exist.

- [ ] **Step 3: Implement**

Order the loop so the contract's steps 1-6 read off the code in the same sequence. Two details are easy to get subtly wrong:

- **`try_wait()` before each probe, not after.** Checking after means one full backoff sleep is always paid on a child that is already dead.
- **The `DAEMON_ALREADY_RUNNING` branch stops calling `try_wait()` once it fires.** The child is reaped; a second `try_wait()` on a reaped child errors. Record "the child is gone and that was fine" in a local and keep probing on the deadline alone.

The launcher is a sync `FnOnce() -> io::Result<Child>` called once, from inside the async fn. `connect_or_spawn_with` owns the returned `Child` for the rest of the call and drops it on every exit path.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Prove the handshake-probe test gates — both transcripts required**

Replace the handshake probe with a bare `tokio::net::UnixStream::connect`. Confirm `a_socket_that_accepts_but_never_handshakes_is_not_mistaken_for_ready` **FAILS** — under the bare probe the call returns `Ok(SpawnOutcome::Spawned(_))` against a socket nobody is accepting on, and `unwrap_err()` panics. Restore the handshake probe, confirm it **PASSES**.

Paste **both** transcripts into your report. This is the single most important gate in the phase, and the rule is unconditional: **a version of this test that passes under the bare-`connect()` implementation has not gated anything and must be rewritten, not accepted.** If your first attempt passes under both, the child is exiting too early or the socket is not actually bound — fix the fixture, do not weaken the assertion.

- [ ] **Step 6: Commit** — `feat(client): connect_or_spawn probing with a real handshake, not a bare connect`

---

### Task 5: CLI skeleton — clap tree, every argument struct, exit codes, main

**Files:**
- Modify: `crates/shep-cli/Cargo.toml`, `crates/shep-cli/src/main.rs`
- Create: `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/exit.rs`

**`cli.rs` owns every argument struct in the tree.** Tasks 7 through 11 consume them and define none. Two reasons: this task's own "run the tests, confirm they pass" step is impossible if the enum names types that do not exist yet, and the whole parse surface has to sit in one portable file for the Windows tier to hold.

**Interfaces:**
- Consumes: `shep_core::protocol::RpcErrorCode`, `clap_complete::aot::Shell`
- Produces:
```rust
#[derive(Debug, clap::Parser)]
#[command(name = "shep", version, about = "A process manager for your flock")]
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
    /// List one fold (spec §5 / §9)
    Fold(FoldArgs),
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

#[derive(Debug, clap::Args)]
pub struct FoldArgs {
    /// The fold to list
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct BleatsArgs {
    /// Which sheep (default: all)
    #[arg(default_value = "all")]
    pub selector: String,
    /// Drain what is buffered and exit instead of streaming
    #[arg(long)]
    pub no_follow: bool,
    /// Only stderr
    #[arg(long, conflicts_with = "out")]
    pub err: bool,
    /// Only stdout
    #[arg(long, conflicts_with = "err")]
    pub out: bool,
}

#[derive(Debug, clap::Args)]
pub struct CompletionArgs {
    /// Shell to generate a completion script for
    #[arg(value_enum)]
    pub shell: clap_complete::aot::Shell,
}

#[derive(Debug, clap::Args)]
pub struct DaemonArgs {
    /// Restore the saved muster roll on boot
    #[arg(long, default_value_t = true)]
    pub restore: bool,
}
```

**On `no_follow`, and why the obvious spelling is wrong.** The tempting declaration is a `follow: bool` field with `#[arg(long, default_value_t = true, action = clap::ArgAction::Set)]`. That does **not** produce `--no-follow`. `ArgAction::Set` stores a *value*, so it yields a flag that requires one — `--follow true` — and clap offers no derive attribute that synthesises a negated long from a positive one. Verified against clap 4.6.6's own `ArgAction::Set` doctest, which parses `["mycmd", "--flag", "value"]`. Declare the negative flag directly, as above (a bare `bool` field infers `ArgAction::SetTrue`), and compute `let follow = !args.no_follow;` at the call site.

`clap_complete::aot::Shell` already implements `clap::ValueEnum`, so `#[arg(value_enum)]` is all the derive needs. It is `#[non_exhaustive]` upstream; do not match on it exhaustively anywhere.

```rust
/// No `#[non_exhaustive]`: this is a binary crate, so there is no downstream
/// matcher for it to protect, and IR-20's growth argument does not apply
/// (contrast shep-client's three error enums, which do carry it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Another daemon already holds this `$SHEP_HOME`. Read across the
    /// process boundary by `shep_client::spawn::DAEMON_ALREADY_RUNNING`,
    /// which must stay equal to 10.
    DaemonAlreadyRunning = 10,
}
impl From<RpcErrorCode> for ExitCode { /* infallible, total */ }

// OS tier: these three error types do not exist on a Windows build.
#[cfg(unix)] impl From<&shep_client::ConnectError> for ExitCode {}
#[cfg(unix)] impl From<&shep_client::RequestError> for ExitCode {}
#[cfg(unix)] impl From<&shep_client::SpawnError> for ExitCode {}
```

`ExitCode` and `From<RpcErrorCode>` are pure tier — `RpcErrorCode` lives in shep-core and compiles everywhere. Only the three shep-client conversions are gated.

`Usage = 2` is clap's own convention and collides with the exit code spec §9 reserves for `runtime`'s fail-fast. `runtime` is not in this phase. Take clap's 2 now and record the collision in the CHANGELOG so the `runtime` phase resolves it deliberately rather than discovering it.

`Thatlldo` is hidden — spec §9 calls it an easter egg, and only `resurrect` is explicitly named hidden there, so this is a judgment call. It behaves exactly as `Stop`; implement it by delegating, not by duplicating.

- [ ] **Step 1: Add dependencies**

First, `assert_cmd` and `predicates` are in this phase's Tech Stack but are in neither `[workspace.dependencies]` nor the lockfile. Add them to the root `Cargo.toml` under the workspace rule (`default-features = false`, features named), then wire the crate:

```toml
# crates/shep-cli/Cargo.toml
[dependencies]
shep-core.workspace = true
shep-daemon.workspace = true
shep-client.workspace = true
clap = { version = "4", default-features = false, features = ["std", "derive", "help", "usage", "error-context", "suggestions", "env", "wrap_help"] }
clap_complete = { version = "4", default-features = false }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "signal", "net", "time", "sync"] }
# `Render: Serialize` (Task 6) needs the trait and the derive, not just the
# serializer — `serde_json` alone does not bring them.
serde.workspace = true
serde_json.workspace = true

[dev-dependencies]
tempfile.workspace = true
insta.workspace = true       # Task 6's envelope snapshots
assert_cmd.workspace = true  # Task 12's real-binary tier
predicates.workspace = true

# Task 7's process-group assertion and Task 12's Drop guard both signal a real
# pid; same cfg(unix) dev-only shape shep-daemon uses. The workspace default
# features (signal, process, user) already cover `getpgid` and `kill`.
[target.'cfg(unix)'.dev-dependencies]
nix.workspace = true
```

`shep-daemon` stays an unconditional dependency: its pure tier compiles on Windows, and only the four modules this crate reaches for are `#[cfg(unix)]` — so the gate belongs on our `use` sites, not on the dependency edge.

`default-features = false` on clap drops `color`; add it back only if a reviewer asks. On clap_complete it is a no-op — that crate's `default` feature set is empty — but keep it for consistency with the workspace rule. Do **not** enable `unstable-dynamic`; dynamic completion is out of scope. Run the minimal-versions rehearsal for every new crate here and pin floors with a reason comment if it fails.

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
fn logs_reaches_bleats() {
    assert!(matches!(
        Cli::try_parse_from(["shep", "logs"]).unwrap().command,
        Commands::Bleats(_)
    ));
}

#[test]
fn format_defaults_to_table_and_accepts_json() {
    let cli = Cli::try_parse_from(["shep", "flock"]).unwrap();
    assert_eq!(cli.global.format, Format::Table);
    let cli = Cli::try_parse_from(["shep", "--format", "json", "flock"]).unwrap();
    assert_eq!(cli.global.format, Format::Json);
}

#[test]
fn every_rpc_error_code_maps_to_a_distinct_nonzero_exit_code() {
    // `RpcErrorCode` is `#[non_exhaustive]`, so `From` needs a `_` arm and the
    // compiler cannot force this list to stay complete. Keeping it here means a
    // shep-core addition shows up as a review question, not silently as Failure.
    // The final assertion pins the fallback so a new variant is at least
    // classified as an internal fault rather than a generic failure.
    let codes = [NotFound, InvalidConfig, SpawnFailed, ProtocolMismatch, Internal, DeadlineExceeded];
    let mapped: Vec<u8> = codes.iter().map(|c| ExitCode::from(*c) as u8).collect();
    assert!(mapped.iter().all(|&c| c != 0), "no error may map to Success");
    let unique: std::collections::HashSet<_> = mapped.iter().collect();
    assert_eq!(unique.len(), mapped.len(), "distinct causes need distinct exit codes: {mapped:?}");
}

#[test]
fn the_already_running_exit_code_matches_the_clients_constant() {
    // The one number both crates hard-code. If these ever diverge, the
    // cold-start race in Task 4 silently becomes a fatal error again.
    assert_eq!(ExitCode::DaemonAlreadyRunning as i32, shep_client::spawn::DAEMON_ALREADY_RUNNING);
}
```

`the_already_running_exit_code_matches_the_clients_constant` names a shep-client item, so it is OS tier: put it behind `#[cfg(unix)]`. Every other test here runs on Windows and must keep doing so.

- [ ] **Step 3: Run, confirm failure.** Expected: `Cli`, `ExitCode` do not exist.

- [ ] **Step 4: Implement**

`main` is `#[tokio::main]`, calls `run(cli) -> ExitCode`, and ends with `std::process::exit(code as i32)`. Keep `main` itself trivial — all logic in `run` so it is testable. `run` has two bodies:

```rust
#[cfg(unix)]
fn run(cli: Cli) -> ExitCode { /* the real dispatch */ }

#[cfg(windows)]
fn run(_cli: Cli) -> ExitCode {
    eprintln!("shep does not yet support Windows");
    ExitCode::Failure
}
```

Parsing happens before either — a Windows build still parses, still prints help, still validates arguments, and still runs every test in this task.

- [ ] **Step 5: Run tests, confirm pass, plus the Windows gate**

```bash
cargo test -p shep-cli
cargo check --workspace --all-targets --target x86_64-pc-windows-gnu
```

- [ ] **Step 6: Commit** — `feat(cli): clap tree, every argument struct, and the exit-code taxonomy`

---

### Task 6: Output — one envelope, two renderings, one source of truth

**Files:**
- Create: `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/src/output/table.rs`

Pure tier — this whole module compiles and tests on Windows. It names no shep-client type; the renderers take payloads, and it is the *call sites* under `commands/` that are OS tier.

**Interfaces:**
- Consumes: `Format` and `ExitCode` from `cli.rs` / `exit.rs` (Task 5), `serde::Serialize`
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

/// Renders `data` to stdout in `fmt`. Generic, not `Box<dyn Render>`.
pub fn emit<T: Render>(fmt: Format, command: &str, data: T);

/// Renders a failure to stderr in `fmt`, and returns the code the caller exits with.
pub fn emit_error(fmt: Format, code: ExitCode, message: &str) -> ExitCode;
```

**`Render` is not object-safe, so dispatch is generic.** `headers()` has no receiver, and `Serialize` is not dyn-compatible as a supertrait — `Box<dyn Render>` will not compile. Every call site knows its payload type statically, so `emit<T: Render>` costs nothing and removes the temptation.

**Keeping the two renderings honest** is the real design problem here. `Serialize` and `rows()` can drift silently. The plan's answer: a test per payload type that serializes a fully-populated value, collects its JSON object keys, and asserts they match `headers()` after a documented name mapping. A new field added to the struct fails that test until it is either added to `headers()` or explicitly listed in the payload's `JSON_ONLY` allowlist with a reason.

**Stream discipline:** rendered output goes to stdout; diagnostics, progress, and errors go to stderr. Under `--format json` an error is *also* a JSON object on stderr — `{"schema_version", "error": {"code", "message"}}` — so a script piping stdout gets clean data and a script capturing stderr gets a parseable failure. Never mix the two on one stream.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_json_envelope_shape_is_pinned() {
    let out = OutputEnvelope { schema_version: SCHEMA_VERSION, command: "flock", data: sample_flock() };
    insta::assert_json_snapshot!(out);
}

/// The anti-drift gate. Serializes a fully-populated value, collects its JSON
/// object keys, and asserts they match `headers()` after the documented name
/// mapping — so a field added to `Serialize` and forgotten in `rows()` fails
/// here rather than silently vanishing from the table.
#[test]
fn every_serialized_field_is_either_a_column_or_explicitly_json_only() {
    let value = serde_json::to_value(sample_flock()).unwrap();
    let keys: std::collections::BTreeSet<&str> =
        value[0].as_object().unwrap().keys().map(String::as_str).collect();

    let covered: std::collections::BTreeSet<&str> = FlockRows::headers()
        .iter()
        .map(|h| FlockRows::json_key_for(h))
        .chain(FlockRows::JSON_ONLY.iter().copied())
        .collect();

    assert_eq!(
        keys, covered,
        "a serialized field is a column, or it is in JSON_ONLY with a reason — never neither"
    );
}

#[test]
fn table_and_json_report_the_same_record_count() {
    let rows = sample_flock(); // three sheep
    let json = serde_json::to_value(&rows).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 3);
    assert_eq!(rows.rows().len(), 3, "the two renderings must never disagree on how many records exist");
}

#[test]
fn an_error_under_format_json_is_a_json_object_on_stderr() {
    let (stdout, stderr) = capture(|| emit_error(Format::Json, ExitCode::NotFound, "no sheep matched"));
    assert!(stdout.is_empty(), "a script piping stdout must get clean data or nothing");

    let json: serde_json::Value = serde_json::from_str(&stderr)
        .expect("under --format json a failure must be parseable, not prose");
    assert_eq!(json["schema_version"], SCHEMA_VERSION);
    assert_eq!(json["error"]["code"], "not_found");
    assert_eq!(json["error"]["message"], "no sheep matched");
}

#[test]
fn an_error_under_format_table_is_plain_text_on_stderr() {
    let (stdout, stderr) = capture(|| emit_error(Format::Table, ExitCode::NotFound, "no sheep matched"));
    assert!(stdout.is_empty());
    assert!(stderr.contains("no sheep matched"));
    assert!(serde_json::from_str::<serde_json::Value>(&stderr).is_err(), "table mode is not JSON");
}
```

`FlockRows::json_key_for` is the documented name mapping (table `UPTIME` → JSON `uptime_ms`, and so on); `JSON_ONLY` is the allowlist of fields that legitimately have no column, each with a comment giving the reason.

- [ ] **Step 2: Run, confirm failure.** Expected: `OutputEnvelope`, `Render`, `emit` do not exist.

- [ ] **Step 3: Implement**

`emit<T: Render>(fmt, command, data)` matches on `fmt` and either writes `serde_json::to_writer` of an `OutputEnvelope` or hands `headers()`/`rows()` to `table::render`. `emit_error` writes to stderr and returns the code it was given, so a call site reads `return emit_error(fmt, ExitCode::NotFound, &msg);`.

The table renderer computes column widths from the widest cell including the header, pads to that, and separates with two spaces. No box-drawing characters — a table a user can `awk` over beats one that looks nice.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Prove the anti-drift test gates**

Add a throwaway field to a payload struct. Confirm `every_serialized_field_is_either_a_column_or_explicitly_json_only` FAILS. Remove it, confirm green. Transcript into the report.

- [ ] **Step 6: Commit** — `feat(cli): versioned output envelope with drift-gated table and JSON renderings`

---

### Task 7: The hidden `daemon` subcommand and the detached launcher

**Files:**
- Create: `crates/shep-cli/src/commands/daemon.rs`, `crates/shep-cli/src/launch.rs`

Both files are OS tier: `#[cfg(unix)]` at their `mod` declarations in `main.rs`.

**Interfaces:**
- Consumes: `DaemonArgs` from `cli.rs` (Task 5 — this task defines no argument struct), `ExitCode` from `exit.rs`, `shep_daemon::{boot::{boot, BootOptions, BootError, DIR_MODE}, tokio_runner::TokioRunner}`, `shep_core::{paths::ShepPaths, config::DaemonConfig}`
- Produces:
```rust
/// Runs the supervisor in this process until a signal or `KillDaemon`.
pub async fn run_daemon(paths: ShepPaths, args: DaemonArgs) -> Result<(), BootError>;

/// Maps a boot failure to the process exit status the parent will read.
pub fn daemon_exit_code(err: &BootError) -> ExitCode;

/// Spawns `shep daemon` detached from this process's group and terminal.
/// Returns the child so the caller can `try_wait()` it while probing.
pub fn launch_daemon(paths: &ShepPaths) -> std::io::Result<std::process::Child>;
```

**`run_daemon` honours `[daemon].socket`.** `BootOptions { socket: None, .. }` would make that documented config key a silent no-op — `DaemonSection::socket` exists (`shep-core/src/config/daemon.rs:19`) and `boot` reads `options.socket` at `boot.rs:497`. So: load `DaemonConfig` from `paths.daemon_config`, then

```rust
BootOptions { socket: config.daemon.socket, ready_fd: None, restore: args.restore }
```

`ready_fd` stays `None`, deliberately and permanently, per this plan's Global Constraints. Then `boot(TokioRunner::new(), paths, options).await?.run().await`.

**`daemon_exit_code` maps `BootError::AlreadyRunning` to `ExitCode::DaemonAlreadyRunning` (10) and every other `BootError` to `ExitCode::Failure`.** This is the only channel by which a losing child in a cold-start race can tell the parent it lost: the parent holds a `std::process::Child`, not a `Result<_, BootError>`, so an exit status is all it gets. Task 4's `connect_or_spawn` reads exactly this number. Do not renumber either side alone.

**`launch_daemon` creates `paths.logs` before it spawns.** The redirect below opens two files inside that directory, and on a cold `$SHEP_HOME` nothing has created it yet — the daemon's own `init_dirs` runs *after* exec, inside the child, so the redirect fails with ENOENT and the child never starts. That is the exact path Task 12 case 1 exercises, so without this the phase's headline feature fails on first use.

```rust
use std::os::unix::fs::DirBuilderExt as _;

std::fs::DirBuilder::new()
    .recursive(true)
    .mode(shep_daemon::boot::DIR_MODE) // 0o700, the daemon's own constant
    .create(&paths.logs)?;
```

Mode set **at creation**, never create-then-chmod, matching the daemon's own TOCTOU discipline (`boot.rs:85-90`). The daemon's `init_dirs` remains authoritative and idempotent and still runs; this duplicates one directory of it on purpose, so the two log files can be opened. Say so in a comment — a later reader will otherwise "clean it up" and reintroduce the ENOENT.

**`launch_daemon` sets `SHEP_HOME` on the child explicitly**, from the already-resolved `paths.home`, rather than letting it inherit. The parent resolved `$SHEP_HOME` from `--home`; a child re-resolving from ambient environment binds a *different* socket, and the parent then probes the right path for 30 seconds and fails — on a flag the CLI advertises. Setting it explicitly also makes the child's resolution deterministic instead of dependent on whatever the parent's environment happened to hold.

The rest: `std::env::current_exe()`, the `daemon` subcommand, `Command::process_group(0)` (stable since 1.64 via `CommandExt`, no unsafe) so the daemon survives the parent exiting and its terminal closing, and stdout/stderr redirected to `paths.logs.join("shepd.out.log")` and `paths.logs.join("shepd.err.log")` — inheriting the parent's terminal would spray daemon output over the user's shell after the CLI returns.

**Do not** implement a classic double-fork. `process_group(0)` plus redirected stdio achieves what this needs without any `unsafe`, and `#![forbid(unsafe_code)]` in this crate is not negotiable.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_launcher_never_sets_the_readiness_fd_variable() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let cmd = launch_command(&paths); // the Command builder, before spawn
    assert!(
        !cmd.get_envs().any(|(k, _)| k == "SHEP_READY_FD"),
        "the whole phase design rests on readiness being a handshake, not an fd"
    );
}

#[test]
fn the_launcher_pins_shep_home_to_the_resolved_path() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let cmd = launch_command(&paths);
    let home = cmd.get_envs()
        .find(|(k, _)| *k == "SHEP_HOME")
        .and_then(|(_, v)| v)
        .expect("the child must not re-resolve $SHEP_HOME from ambient environment");
    assert_eq!(std::path::Path::new(home), paths.home);
}

#[test]
fn the_launcher_creates_the_log_directory_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    assert!(!paths.logs.exists(), "precondition: a cold $SHEP_HOME");

    let mut child = launch_daemon(&paths).unwrap();

    assert!(paths.logs.is_dir(), "the redirect targets must be openable");
    assert_eq!(mode_of(&paths.logs), shep_daemon::boot::DIR_MODE);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn the_child_is_placed_in_its_own_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let mut child = launch_daemon(&paths).unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    // getpgid(child) == child, not the test runner's group.
    assert_eq!(nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid))).unwrap().as_raw(), pid);
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn run_daemon_passes_ready_fd_none_and_the_configured_socket() {
    // Assert on the constructed BootOptions, not on boot's behaviour: factor
    // the construction into a `fn boot_options(config, args) -> BootOptions`
    // so this test has something to call.
    let config = DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/custom.sock\"\n"), &|_| None).unwrap();
    let opts = boot_options(&config, &DaemonArgs { restore: true });
    assert!(opts.ready_fd.is_none(), "readiness is a handshake in this phase");
    assert_eq!(opts.socket.as_deref(), Some(std::path::Path::new("/tmp/custom.sock")));
    assert!(opts.restore);
}

#[test]
fn already_running_gets_its_own_exit_code_and_everything_else_is_failure() {
    assert_eq!(daemon_exit_code(&BootError::AlreadyRunning { pid: Some(7) }), ExitCode::DaemonAlreadyRunning);
    assert_eq!(daemon_exit_code(&BootError::AlreadyRunning { pid: None }), ExitCode::DaemonAlreadyRunning);
    assert_eq!(
        daemon_exit_code(&BootError::Io { path: "/x".into(), source: std::io::Error::other("x") }),
        ExitCode::Failure
    );
}
```

`launch_command(&paths) -> std::process::Command` is the builder `launch_daemon` spawns; splitting it out is what makes the two env assertions possible without running a daemon. `boot_options(&config, &args) -> BootOptions` is the same trick for `run_daemon`: the construction is the thing under test, and `boot`'s behaviour is not.

- [ ] **Step 2: Run, confirm failure.** Expected: `launch_daemon`, `run_daemon`, `daemon_exit_code` do not exist.

- [ ] **Step 3: Implement**

`launch_command` in order: `current_exe()`, `.arg("daemon")`, `.env("SHEP_HOME", &paths.home)`, `.process_group(0)`, `.stdout(File::create(paths.logs.join("shepd.out.log"))?)`, `.stderr(...)`, `.stdin(Stdio::null())`. `launch_daemon` creates `paths.logs` first, then `launch_command(paths).spawn()`.

Do **not** call `.env_clear()`. The child needs `PATH` to exec anything, and clearing the environment would also drop the `SHEP_*` overrides `DaemonConfig::load` reads. Pinning `SHEP_HOME` is the point; wiping everything else is not.

`run_daemon` loads `DaemonConfig` from `paths.daemon_config` — a missing file is not an error, it is `DaemonConfig::load(None, &env)`. A `DaemonConfigError` maps to `ExitCode::InvalidConfig`, not `Failure`.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): foreground daemon subcommand and its detached launcher`

---

### Task 8: Lifecycle verbs — start, stop, restart, delete

**Files:**
- Create: `crates/shep-cli/src/commands/lifecycle.rs`

OS tier: `#[cfg(unix)]` at the `mod` declaration.

**Interfaces:**
- Consumes: `StartArgs` and `SelectorArgs` from `cli.rs` (Task 5 — this task defines no argument struct), `shep_core::config::{Flockfile, FlockFormat, FlockfileError, AppConfig}`, `shep_core::selector::ProcessSelector`, `shep_core::protocol::{SelectorSpec, Request, Response}`, `shep_client::{Client, spawn::connect_or_spawn, START_DEADLINE}`, `crate::launch::launch_daemon`
- Produces: `pub async fn start(...)`, `stop`, `restart`, `delete` — no types.

Note what is **not** consumed: `shep_core::config::flockfile::discover`. Bare `shep start` with no target, resolving a Flockfile from the working directory, is not in this phase's verb list, so nothing here calls `discover` — and an unused import is a `-D warnings` failure, not a harmless extra.

`start` resolves its target in this order:

1. `-` → read stdin, parse as Flockfile JSON (`FlockFormat::Json`).
2. A path whose extension `FlockFormat::from_path` recognises (`toml`, `yaml`, `yml`, `json`, `json5`) → read and `Flockfile::parse`.
3. Any other existing path → one `AppConfig::minimal(name, script)`, where `name` is `--name` if given else the file stem, and `script` is the path **as a `&str`** — `minimal` takes `(&str, &str)`, not a `Path` (`shep-core/src/config/app.rs:205`).
4. Nothing matched → a usage error naming what was tried.

Do not widen this grammar (spec fidelity — the skill's checklist calls out input-format widening as a top drift risk).

`--fold` sets `AppConfig::fold` on every app the target resolved to.

`Request::Start` goes out with `request_with_deadline(.., Some(START_DEADLINE))`, not the 5s default: a cold spawn plus a readiness probe routinely outruns 5 seconds, and a client-side abandonment there would report failure for a sheep that came up fine.

Selectors go through `ProcessSelector::parse` and then `SelectorSpec::from(&parsed)` (`shep-core/src/selector.rs:121`). Parse client-side even though the daemon re-parses, so a malformed selector is a fast local usage error rather than a round trip.

`stop`/`restart`/`delete` are the same shape: parse selector, one request, render. `NotFound` from the daemon is a real outcome with its own exit code, not an error to swallow.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_dash_target_reads_a_flockfile_from_stdin_as_json() {
    let apps = resolve_target("-", None, br#"{"apps":[{"name":"web","script":"./srv"}]}"#).unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "web");
}

#[test]
fn a_recognised_extension_parses_as_a_flockfile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flock.toml");
    std::fs::write(&path, "[[apps]]\nname = \"web\"\nscript = \"./srv\"\n").unwrap();
    let apps = resolve_target(path.to_str().unwrap(), None, b"").unwrap();
    assert_eq!(apps[0].name, "web");
}

#[test]
fn any_other_existing_path_becomes_one_minimal_app_named_for_its_stem() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.js");
    std::fs::write(&path, "").unwrap();
    let apps = resolve_target(path.to_str().unwrap(), None, b"").unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "server");
    assert_eq!(apps[0].script, path.to_str().unwrap());
}

#[test]
fn an_explicit_name_overrides_the_file_stem() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.js");
    std::fs::write(&path, "").unwrap();
    let apps = resolve_target(path.to_str().unwrap(), Some("api"), b"").unwrap();
    assert_eq!(apps[0].name, "api");
}

#[test]
fn a_target_that_matches_nothing_is_a_usage_error_naming_what_was_tried() {
    let err = resolve_target("./does-not-exist", None, b"").unwrap_err();
    assert_eq!(ExitCode::from(&err), ExitCode::Usage);
    assert!(err.to_string().contains("./does-not-exist"));
}

#[test]
fn selectors_round_trip_through_the_wire_form() {
    for (input, expected) in [
        ("all", SelectorSpec::All),
        ("7", SelectorSpec::Id(7)),
        ("web", SelectorSpec::Name("web".into())),
        ("/^web-/", SelectorSpec::Regex("^web-".into())),
        ("fold:api", SelectorSpec::Fold("api".into())),
    ] {
        let parsed = ProcessSelector::parse(input).unwrap();
        assert_eq!(SelectorSpec::from(&parsed), expected, "{input}");
    }
}

#[test]
fn a_malformed_selector_fails_locally_without_a_round_trip() {
    assert!(ProcessSelector::parse("/unclosed").is_err());
}

#[tokio::test]
async fn a_not_found_reply_exits_NotFound_rather_than_being_swallowed() {
    let (client, _served) = fake_client_replying_err(RpcErrorCode::NotFound, "no sheep matched").await;
    let code = stop(&client, Format::Table, &SelectorArgs { selector: "ghost".into() }).await;
    assert_eq!(code, ExitCode::NotFound);
}

#[tokio::test]
async fn start_asks_for_the_longer_deadline() {
    let (client, envelopes) = fake_client_capturing_envelopes().await;
    let _ = start(&client, Format::Table, &start_args("./srv")).await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(sent.deadline_ms, Some(u64::try_from(START_DEADLINE.as_millis()).unwrap()));
}
```

`resolve_target(target, name, stdin) -> Result<Vec<AppConfig>, StartError>` is the pure function the first five tests call; keeping the resolution separate from the RPC is what makes them fast and hermetic. `StartError` is this module's own error enum (IR-18) with one variant per resolution failure — `Stdin(io::Error)`, `Read { path, source }`, `Flockfile(FlockfileError)`, `Unresolvable { target: String }`.

- [ ] **Step 2: Run, confirm failure.** Expected: `resolve_target` and the four verbs do not exist.

- [ ] **Step 3: Implement**

Write `resolve_target` as a single `match` over the four branches in the order given above, with the `Unresolvable` arm last. Resist adding a fifth: a bare directory, a URL, and a glob are all things a reader will be tempted to accept, and none is in the spec.

The four verbs share one shape — `connect_or_spawn` (start) or `Client::connect` (the rest), one request, `emit`, map the error. Factor that into a small helper rather than writing it four times, but keep the helper *inside* this module; it is not a general abstraction and does not belong in `output/`.

Only `start` autostarts. `stop`/`restart`/`delete` against a dead daemon exit `DaemonUnreachable` — spawning a supervisor in order to tell it to stop nothing would be absurd.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): start, stop, restart, and delete`

---

### Task 9: Query verbs — flock, describe, fold, ping

**Files:**
- Create: `crates/shep-cli/src/commands/query.rs`

OS tier: `#[cfg(unix)]` at the `mod` declaration.

**Interfaces:**
- Consumes: `SelectorArgs` and `FoldArgs` from `cli.rs` (Task 5), `shep_core::protocol::{Request, Response, SelectorSpec, ProcessInfo}`, `shep_client::Client`, `crate::output::{emit, Render}`
- Produces: `pub async fn flock(..)`, `describe(..)`, `fold(..)`, `ping(..)`, and `impl Render for FlockRows`.

`flock` renders the `Vec<ProcessInfo>` table: id, name, status, pid, restarts, uptime, fold. `describe` takes a selector.

**`fold <name>` ships in this phase.** Spec §5 (`shep-v1.md:138`) and §9 (`:216`) both require it, and it is fully buildable against today's daemon: `Request::Describe { selector: SelectorSpec::Fold(name) }`. It is a one-line variation on `describe`, and omitting it silently would be worse than a documented deferral.

`ping` reports the daemon's version and pid from the `HelloAck` the client already holds — it must NOT issue a `Request::Ping` round trip to learn something the handshake already told it. It still issues the `Ping` request as a liveness check; just source the version and pid from the ack.

**`ping` is a deliberate addition beyond spec §9.** §9's verb list does not name it. It is kept anyway: it is cheap, it is the natural liveness check, and it exercises the handshake path end to end from the command line, which nothing else in this phase does. Flagged here so a reviewer does not re-raise it as an over-build.

Uptime renders as a human duration in table mode and as raw `uptime_ms` in JSON — a formatted string is not a machine-readable field.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_empty_flock_renders_headers_rather_than_a_bare_blank() {
    let out = render_table(&FlockRows(vec![]));
    assert!(out.contains("NAME"), "an empty flock still tells the user what it would show");
    assert_eq!(out.lines().filter(|l| !l.trim().is_empty()).count(), 1);
}

#[test]
fn uptime_is_a_duration_in_the_table_and_a_number_in_json() {
    let rows = FlockRows(vec![info_with_uptime_ms(3_723_000)]); // 1h 2m 3s
    let table = render_table(&rows);
    assert!(table.contains("1h"), "table uptime is for a human: {table}");

    let json = serde_json::to_value(&rows).unwrap();
    assert_eq!(json[0]["uptime_ms"], serde_json::json!(3_723_000u64));
    assert!(json[0].get("uptime").is_none(), "no formatted duplicate on the machine surface");
}

#[tokio::test]
async fn fold_asks_the_daemon_for_that_fold_and_nothing_wider() {
    let (client, envelopes) = fake_client_capturing_envelopes().await;
    let _ = fold(&client, Format::Table, &FoldArgs { name: "api".into() }).await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(
        sent.body,
        Request::Describe { selector: SelectorSpec::Fold("api".into()) }
    );
}

#[tokio::test]
async fn ping_reads_version_and_pid_from_the_handshake_not_from_a_reply() {
    // The fake daemon acks with a distinctive version/pid, then replies Pong.
    // A `ping` that sourced either from the reply cannot produce these.
    let (client, _served) = fake_client_with_ack(HelloAck {
        daemon_version: "9.9.9".into(),
        protocol: PROTOCOL_VERSION,
        pid: 4242,
    }).await;
    let out = capture_stdout(|| ping(&client, Format::Json)).await;
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["data"]["daemon_version"], "9.9.9");
    assert_eq!(json["data"]["pid"], 4242);
}

#[tokio::test]
async fn ping_still_issues_the_liveness_request() {
    let (client, envelopes) = fake_client_capturing_envelopes().await;
    let _ = ping(&client, Format::Table).await;
    assert_eq!(envelopes.recv().await.unwrap().body, Request::Ping);
}
```

- [ ] **Step 2: Run, confirm failure.** Expected: the four verbs and `FlockRows` do not exist.

- [ ] **Step 3: Implement**

`FlockRows(Vec<ProcessInfo>)` is a newtype so `Render` can be implemented on it — `ProcessInfo` is shep-core's and the orphan rule forbids implementing a shep-cli trait on it directly. `Serialize` forwards transparently (`#[serde(transparent)]`) so the JSON is a plain array of `ProcessInfo`, not a wrapper object.

`headers()` is `["ID", "NAME", "STATUS", "PID", "RESTARTS", "UPTIME", "FOLD"]`. `rows()` renders `pid: None` and `fold: None` as `-`, not as an empty cell — an empty cell in a padded table is indistinguishable from a rendering bug.

Uptime formatting takes `uptime_ms` and emits the two largest non-zero units (`1h 2m`, `3m 4s`, `5s`, `0s`). Put it in a small `fn human_duration(ms: u64) -> String` with its own unit tests; every future table verb will want it.

`fold` is `describe` with `SelectorSpec::Fold(args.name)` instead of a parsed selector — one line, delegating, not a copy.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): flock, describe, fold, and ping`

---

### Task 10: bleats — following the log stream

**Files:**
- Create: `crates/shep-cli/src/commands/bleats.rs`

OS tier: `#[cfg(unix)]` at the `mod` declaration.

**Interfaces:**
- Consumes: `BleatsArgs` from `cli.rs` (Task 5 — this task defines no argument struct), `shep_client::{Client, EventStream, Lagged}`, `shep_core::protocol::{BusEvent, Request, Response, ProcessInfo}`
- Produces: `pub async fn bleats(client: &Client, fmt: Format, args: &BleatsArgs) -> ExitCode;`

`args.no_follow` is the declared flag; the code computes `let follow = !args.no_follow;` once, at the top, and reads `follow` from there. See Task 5 for why the flag is spelled negatively.

**`BusEvent::LogOut`/`LogErr` carry only `{ id, line }` — no name.** Resolve ids to names with one `ListFlock` before subscribing, and cache it. An id that appears later and is not in the cache renders as the bare id rather than blocking on a refresh; do not issue a `ListFlock` per unknown line.

Filtering by selector happens **client-side** on the resolved id set: the daemon's topic filter globs on the topic string (`log.out`), which carries no identity. Say so in a comment — the next reader will assume the daemon filtered.

Ctrl-C: `tokio::select!` the stream against `tokio::signal::ctrl_c()`, flush stdout, exit `Success`. A user ending a follow deliberately has not failed.

If the daemon shuts down mid-follow, print the `DaemonShutdown` notice to stderr and exit `DaemonUnreachable` — the stream ending because the daemon went away is materially different from the user pressing Ctrl-C.

A `Lagged` item prints a one-line notice to stderr and keeps going. Dropped lines are not a reason to abandon a follow, but silently swallowing them is how a user concludes a sheep went quiet.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn no_follow_parses_and_plain_bleats_still_follows() {
    let Commands::Bleats(args) = Cli::try_parse_from(["shep", "bleats"]).unwrap().command
    else { panic!() };
    assert!(!args.no_follow, "the default is to follow");

    let Commands::Bleats(args) = Cli::try_parse_from(["shep", "bleats", "--no-follow"]).unwrap().command
    else { panic!() };
    assert!(args.no_follow);

    // The flag takes no value. `--no-follow true` must be a parse error, not
    // a silent success — this is the thing `ArgAction::Set` got wrong.
    assert!(Cli::try_parse_from(["shep", "bleats", "--no-follow", "true"]).is_err());
}

#[tokio::test]
async fn ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare() {
    let (client, daemon) = fake_client_with_push().await;
    daemon.reply_to_list(vec![info(1, "web")]);

    let out = capture_stdout(|| async {
        daemon.push(BusEvent::LogOut { id: 1, line: "hello".into() }).await;
        daemon.push(BusEvent::LogOut { id: 9, line: "orphan".into() }).await;
        daemon.close().await;
        bleats(&client, Format::Table, &drain_args("all")).await
    }).await;

    assert!(out.contains("web") && out.contains("hello"));
    assert!(out.contains("9") && out.contains("orphan"), "an unknown id renders bare, not blocked on");
    assert_eq!(daemon.list_flock_count(), 1, "one listing, not one per unknown line");
}

#[tokio::test]
async fn err_and_out_filter_the_two_streams() {
    for (args, kept, dropped) in [
        (drain_args_err("all"), "to-stderr", "to-stdout"),
        (drain_args_out("all"), "to-stdout", "to-stderr"),
    ] {
        let (client, daemon) = fake_client_with_push().await;
        daemon.reply_to_list(vec![info(1, "web")]);
        let out = capture_stdout(|| async {
            daemon.push(BusEvent::LogOut { id: 1, line: "to-stdout".into() }).await;
            daemon.push(BusEvent::LogErr { id: 1, line: "to-stderr".into() }).await;
            daemon.close().await;
            bleats(&client, Format::Table, &args).await
        }).await;
        assert!(out.contains(kept), "{kept} should have survived: {out}");
        assert!(!out.contains(dropped), "{dropped} should have been filtered: {out}");
    }
}

/// The daemon's topic filter globs on `log.out` / `log.err`, which carry no
/// identity — so this filtering CANNOT have happened server-side, and a test
/// that let the fake daemon pre-filter would prove nothing.
#[tokio::test]
async fn a_selector_filters_client_side_on_the_resolved_id_set() {
    let (client, daemon) = fake_client_with_push().await;
    daemon.reply_to_list(vec![info(1, "web"), info(2, "worker")]);

    let out = capture_stdout(|| async {
        // The fake pushes BOTH; only the selector may narrow them.
        daemon.push(BusEvent::LogOut { id: 1, line: "from-web".into() }).await;
        daemon.push(BusEvent::LogOut { id: 2, line: "from-worker".into() }).await;
        daemon.close().await;
        bleats(&client, Format::Table, &drain_args("web")).await
    }).await;

    assert!(out.contains("from-web"));
    assert!(!out.contains("from-worker"), "the selector must narrow the resolved id set: {out}");
}

#[tokio::test]
async fn ctrl_c_during_a_follow_exits_Success() {
    let (client, daemon) = fake_client_with_push().await;
    daemon.reply_to_list(vec![info(1, "web")]);
    // The stream stays open; only the signal ends this.
    let follow = tokio::spawn(async move { bleats(&client, Format::Table, &follow_args("all")).await });

    daemon.push(BusEvent::LogOut { id: 1, line: "still running".into() }).await;
    raise_ctrl_c();

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), follow).await.unwrap().unwrap(),
        ExitCode::Success,
        "a user ending a follow deliberately has not failed"
    );
}

#[tokio::test]
async fn a_daemon_shutdown_mid_follow_exits_DaemonUnreachable() {
    let (client, daemon) = fake_client_with_push().await;
    daemon.reply_to_list(vec![info(1, "web")]);
    daemon.push(BusEvent::DaemonShutdown).await;
    daemon.close().await;
    assert_eq!(bleats(&client, Format::Table, &follow_args("all")).await, ExitCode::DaemonUnreachable);
}

#[tokio::test]
async fn a_lag_notice_reaches_stderr_and_the_follow_continues() {
    let (client, daemon) = fake_client_with_push().await;
    daemon.reply_to_list(vec![info(1, "web")]);

    let (stdout, stderr) = capture(|| async {
        daemon.overrun_by(8).await;                                    // forces a Lagged item
        daemon.push(BusEvent::LogOut { id: 1, line: "after".into() }).await;
        daemon.close().await;
        bleats(&client, Format::Table, &drain_args("all")).await
    }).await;

    assert!(stderr.contains("dropped") || stderr.contains("lagged"), "a lag must be told, not swallowed: {stderr}");
    assert!(stdout.contains("after"), "a lag ends the gap, not the follow");
}
```

`follow_args` / `drain_args` / `drain_args_err` / `drain_args_out` build `BleatsArgs` with `no_follow` and the two stream flags set the four ways that matter; `raise_ctrl_c()` is the test-only hook that resolves the `ctrl_c()` future (a channel the production path selects on, not a real signal — a real `SIGINT` would kill the test runner).

- [ ] **Step 2: Run, confirm failure.** Expected: `bleats` does not exist.

- [ ] **Step 3: Implement**

Order matters: `ListFlock` for the id→name cache **first**, then `subscribe`. Doing it the other way loses every line the daemon pushes while the listing is in flight.

The main loop is one `tokio::select!` over three arms — the event stream, `ctrl_c()`, and (under `--no-follow`) an immediate break once the buffered drain is exhausted. Each arm returns a distinct `ExitCode`, so the exit code *is* the record of how the follow ended.

Flush stdout on every exit path. A follow that ends with lines still in the `LineWriter` buffer loses them, and the user has no way to know.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): bleats log following with client-side identity resolution`

---

### Task 11: kill and static completions

**Files:**
- Create: `crates/shep-cli/src/commands/admin.rs`

OS tier: `#[cfg(unix)]` at the `mod` declaration.

**Interfaces:**
- Consumes: `CompletionArgs` from `cli.rs` (Task 5 — this task defines no argument struct), `shep_client::Client`, `clap::CommandFactory`, `clap_complete::aot::{generate, Shell}`
- Produces:
```rust
/// How long `kill` waits for the socket file to disappear after the daemon
/// acknowledges shutdown. `RunningDaemon::run` unlinks it as its last step
/// (`boot.rs:727`), behind the full kill ladder over every online sheep
/// (`:722`) — this has to cover that ladder's whole budget, not just a
/// round trip (IR-26: named, not a prose "a few seconds").
const KILL_TEARDOWN_WAIT: Duration = Duration::from_secs(10);

pub async fn kill(client: Client, fmt: Format) -> ExitCode;
pub fn completions(args: &CompletionArgs) -> ExitCode;
```

`kill` sends `Request::KillDaemon`, expects `Response::ShuttingDown`, and then — per the wire sequence — that connection closes while the daemon finishes teardown. Do not report success on the reply alone: poll for the socket file to disappear, bounded by `KILL_TEARDOWN_WAIT`, so `shep kill && shep start` cannot race the old daemon's unlink. If the poll times out, report that teardown is still in progress rather than claiming a clean stop.

Put a comment on the poll: a *new* daemon binding the same path mid-poll would make the file reappear and the poll could in principle observe it and hang on. It is essentially unreachable — nothing starts a daemon between our own two statements, and the loser of any such race exits 10 — so the poll deliberately carries no defence against it. Said out loud so a reader does not add one.

`completions <shell>` uses `clap_complete::aot::generate` with `Cli::command()`:

```rust
clap_complete::aot::generate(args.shell, &mut Cli::command(), "shep", &mut std::io::stdout());
```

Note the module path: it is `aot::generate`, not `clap_complete::generate`. The top-level re-export still resolves in 4.6 but is documented as deprecated in favour of `aot` (`clap_complete/src/lib.rs:102-103`), and `clap_complete::shells` is likewise the deprecated alias for `aot`'s shell types. `generate` takes `&mut Command`, a `bin_name`, and a `&mut dyn Write`, and returns `()` — there is no `Result` to propagate.

Static only. Add a one-line note in the generated help that sheep-name completion is not yet dynamic, and name it as a Phase 4+ follow-up rather than letting it drop off spec §9's list (`docs/research/phase3-cli.md:495-496`).

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn kill_waits_for_the_socket_to_disappear_before_reporting_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    // A fake daemon that replies ShuttingDown, waits, THEN unlinks.
    let (client, daemon) = fake_client_on(&path).await;
    daemon.reply_shutting_down_then_unlink_after(Duration::from_millis(120));

    assert!(path.exists());
    let code = kill(client, Format::Table).await;
    assert_eq!(code, ExitCode::Success);
    assert!(!path.exists(), "success must mean the socket is actually gone");
}

#[tokio::test]
async fn a_teardown_that_never_finishes_reports_in_progress_not_success() {
    // Fake daemon replies ShuttingDown and never unlinks. Uses an injected
    // short wait, not KILL_TEARDOWN_WAIT — the test proves the branch, not
    // that ten seconds elapse.
    let code = kill_with_wait(client, Format::Table, Duration::from_millis(80)).await;
    assert_ne!(code, ExitCode::Success);
}

#[test]
fn completions_generate_non_empty_scripts_for_every_supported_shell() {
    use clap_complete::aot::Shell;
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Elvish, Shell::PowerShell] {
        let mut buf = Vec::new();
        clap_complete::aot::generate(shell, &mut Cli::command(), "shep", &mut buf);
        let script = String::from_utf8(buf).unwrap();
        assert!(!script.is_empty(), "{shell} produced nothing");
        assert!(script.contains("shep"), "{shell} script must name the binary");
    }
}

#[test]
fn completions_cover_the_visible_aliases() {
    let mut buf = Vec::new();
    clap_complete::aot::generate(Shell::Bash, &mut Cli::command(), "shep", &mut buf);
    let script = String::from_utf8(buf).unwrap();
    for verb in ["flock", "list", "ls", "bleats", "logs"] {
        assert!(script.contains(verb), "{verb} missing from the bash script");
    }
}
```

`completions_generate_non_empty_scripts_for_every_supported_shell` and `completions_cover_the_visible_aliases` name only `cli.rs` and `clap_complete`, both pure tier — put them where they run on Windows too. Only the two `kill` tests are OS tier.

- [ ] **Step 2: Run, confirm failure.** Expected: `kill` and `completions` do not exist.

- [ ] **Step 3: Implement**

`kill` takes `client` **by value** and drops it after the reply: the daemon closes that connection as it tears down, and holding it would just produce a `RequestError::Closed` on the way out that the caller would have to learn to ignore.

Split the wait as `kill_with_wait(client, fmt, wait: Duration)`, with `kill` delegating at `KILL_TEARDOWN_WAIT`. That is the same injectable-timing shape as Task 4's `SpawnOptions`, and for the same reason — the timeout branch needs a test, and the test must not take ten seconds to prove it.

Poll the socket path with a short fixed interval (name it, IR-26) rather than a growing backoff: the wait is already bounded and short, and a backoff here only delays the common case where teardown finishes in milliseconds.

`Shell` is `#[non_exhaustive]` upstream, so the completions test's array is a maintained list, not an exhaustive match — a new clap_complete shell will not break the build, and that is fine.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): daemon shutdown and static shell completions`

---

### Task 12: End-to-end tier, JSON fixtures, CHANGELOG

**Files:**
- Create: `crates/shep-cli/tests/cli_e2e.rs`
- Create: `crates/shep-cli/tests/fixtures/*.json`
- Modify: `crates/shep-cli/CHANGELOG.md`, `crates/shep-client/CHANGELOG.md`

The file opens with `#![cfg(unix)]`. An integration test is its own compilation unit, and `--all-targets` plus `cargo test --workspace` build it on the Windows CI leg otherwise.

Real binary via `assert_cmd`, fresh `$SHEP_HOME` per test in a `tempfile::TempDir`. Copy the teardown discipline from `daemon_e2e.rs:43-152` — a `Drop` guard that SIGKILLs the spawned daemon's process *group* (`-pid`, not `pid`). That guard was empirically proven load-bearing in Phase 2b (a panicking test leaked a real orphan without it; see the "Drop-prevents-leak experiment" note at `daemon_e2e.rs:118-138`); a CLI suite that spawns real daemons needs it at least as much.

The CLI's version differs from the daemon suite's in one way, and it matters: this tier never holds a `Child`, because the daemon it must reap was spawned by the *binary under test*, not by the test process. So `DaemonGuard` records `$SHEP_HOME`s rather than pids, and on drop reads each one's `pids/shepd.pid` (the path `shep_daemon::boot::pidfile` builds) and SIGKILLs that process group:

```rust
#[derive(Debug, Default)]
struct DaemonGuard(Vec<PathBuf>);

impl DaemonGuard {
    /// Register a `$SHEP_HOME` whose daemon this test is responsible for.
    /// Call it as soon as a command that may autostart has returned — before
    /// any assertion that could panic past the cleanup.
    fn adopt_home(&mut self, home: &Path) { self.0.push(home.to_path_buf()); }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        for home in &self.0 {
            let Ok(text) = std::fs::read_to_string(home.join("pids/shepd.pid")) else { continue };
            let Ok(pid) = text.trim().parse::<i32>() else { continue };
            // Group, not leader: the daemon's own children are in its group.
            // ESRCH on an already-reaped daemon is the expected happy path.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}
```

Keep `$SHEP_HOME` shallow — the tempdir root itself, not a nested path. macOS caps `sun_path` around 104 bytes and a nested fixture path silently overruns it. The reasoning and the fixture shape to copy are at `shep-daemon/src/lib.rs:256-259` (the `test_paths` helper's own comment) and `shep-daemon/tests/daemon_e2e.rs:57-58`.

Required cases:
1. `shep start <script>` with no daemon running autostarts one, and the sheep reaches Online.
2. A second command reuses the daemon rather than spawning a second (assert one pid across both).
3. Two concurrent `shep start` invocations against a cold `$SHEP_HOME` produce exactly one daemon and no spurious error. This is the race Phase 2b's `flock(2)` makes safe; prove the client half is safe too — the loser exits 10, `connect_or_spawn` keeps probing, and both invocations exit 0.
4. `--format json` output validates against the committed fixture for `flock`, `describe`, and `start`.
5. Exit codes: a selector matching nothing exits `NotFound`; a malformed selector exits `Usage`; a command against a socket path in a nonexistent directory exits `DaemonUnreachable`.
6. `shep kill` stops the daemon and removes the socket.
7. `shep bleats --no-follow` drains buffered lines and exits `Success`.
8. `shep --home <tmp> start <script>` autostarts a daemon whose socket is **under `<tmp>`** — assert on the location of the socket file, not on the command exiting 0. A child that re-resolved `$SHEP_HOME` from ambient environment binds elsewhere, and only this assertion catches it:

```rust
#[test]
fn home_reaches_the_spawned_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    Command::cargo_bin("shep").unwrap()
        .args(["--home", dir.path().to_str().unwrap(), "start", script.to_str().unwrap()])
        .env_remove("SHEP_HOME")   // the ambient value must not be what makes this pass
        .assert()
        .success();

    guard.adopt_home(dir.path()); // before the assertion, so a failure still reaps

    let socket = dir.path().join("run/shep.sock");
    assert!(socket.exists(), "the daemon bound somewhere other than --home");
}
```

- [ ] **Step 1: Write all eight, run, confirm they fail**
- [ ] **Step 2: Implement whatever wiring they expose as missing**
- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: Write both CHANGELOGs** — including the `Usage = 2` collision note for the future `runtime` phase, the `DaemonAlreadyRunning = 10` cross-crate contract, and shep-client's public API as a new stability surface.
- [ ] **Step 5: Commit** — `test(cli): end-to-end tier with autostart, concurrency, and pinned JSON fixtures`

---

## Exit criteria

1. All twelve tasks complete and individually reviewed.
2. Every gate in Global Constraints green from its own exit code — including `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu`.
3. `grep -rn "unsafe" crates/shep-client/src crates/shep-cli/src | grep -v "forbid(unsafe_code)"` returns nothing. (The unfiltered grep can never pass: `#![forbid(unsafe_code)]` contains the string it searches for.)
4. `grep -rn "SHEP_READY_FD\|adopt_fd" crates/shep-cli/src crates/shep-client/src` returns nothing.
5. The three revert-proof transcripts (Task 2 routing, Task 4 handshake probe, Task 6 anti-drift) are in the reports, each with BOTH the broken-FAIL and restored-PASS runs.
6. `shep start`, `shep flock`, `shep bleats`, `shep kill` work against a real daemon on a clean `$SHEP_HOME`.
7. A report to Rin listing: the now-dead readiness-pipe surface with evidence, and every judgment call made on her behalf.

## Open questions for Rin — do not resolve these unilaterally

1. **Retire the readiness pipe?** This phase makes `sys.rs`, `BootOptions::ready_fd`, `DaemonReady`, and `READY_FD_ENV` unreachable in production. Deleting them would let every crate in the workspace be `#![forbid(unsafe_code)]` and would retire IR-22 as satisfied-by-construction. Analysis: `docs/research/phase3-readiness-decision.md`.
2. **The bind→serve gap.** The daemon signals nothing between binding its socket (`boot.rs:498`) and accepting on it (`boot.rs:707`), so a client can connect into the backlog and wait through the whole muster restore. Phase 3 absorbs this with a 30s deadline plus a bounded per-attempt handshake. The real fix is ordering — either start `serve()` before the restore, or move readiness to the point where accepting begins. Both change merged daemon behaviour, so neither is in this phase.
3. **`Usage = 2` collides** with spec §9's fail-fast code for `runtime`. Taken as clap's convention for now.
4. **`--with-env` cannot be built yet** — `ProcessInfo` has no `env` field. It needs an additive wire change first.
5. **`DaemonAlreadyRunning = 10` is a new exit code**, not one spec §9 enumerates, and it is a cross-crate contract: shep-client hard-codes the same 10 so it can read a dead child's status. The alternative — treating every non-zero child status as fatal — makes the concurrent-cold-start case (Task 12 case 3) fail. Blessing the number, or picking a different one, is yours.
6. **`completions` or `completion`?** Spec §9 says "clap_complete completions" in prose without naming a verb; `docs/research/phase3-cli.md:451` writes `shep completion <shell>`, singular. The plan uses `Completions`. Whichever you pick becomes a stable CLI surface, so it is worth one word of your time. (An alias for the other spelling is trivial if you want both.)
