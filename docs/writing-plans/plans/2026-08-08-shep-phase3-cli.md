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
- **shep-client** — OS tier. Gate at the `mod` declaration in `lib.rs`, not with an inner `#![cfg(unix)]`, so each module's `#[cfg(test)]` block is excluded along with it: `connection`, `actor`, `client`, `events`, `spawn`, `testing`. Pure tier: the crate docs and the `pub use shep_core` re-export, which is all that is left. **There is no portable error module** — fix J below puts each error enum in the module that produces it, and all of those are OS tier.
- **shep-cli** — pure tier, and its unit tests **must keep running on Windows**; that is what makes spec §11's "compiles + unit tests in CI from day one" mean anything. Pure: `cli.rs` (the entire clap tree, *including* the `Daemon` and `Completions` variants — the parse surface must not diverge by platform, so `Cli::command().debug_assert()` and the alias tests cover both), `exit.rs`'s `ExitCode` enum with its `code_str` method and its `From<RpcErrorCode>` impl, `completions.rs`, and **all of `output/` — including every command's rendered payload type**. OS tier, `#[cfg(unix)]`: `launch.rs`, every module under `commands/`, the `From<&ConnectError>` / `From<&RequestError>` / `From<&SpawnError>` impls in `exit.rs`, and the dispatch arms in `main.rs` that call them.
- **Payload types live in the pure tier, with the renderer that consumes them.** `output/rows.rs` owns `FlockRows`, `DeletedIds`, `PingRow`, `KillRow` and their `Render` impls; `commands/` defines no payload type and no `Render` impl. They are built entirely from `shep_core` types (`ProcessInfo`, `HelloAck`), which carry no `cfg` of any kind — verified: `crates/shep-core/src/` contains zero `cfg(unix)` / `cfg(windows)` / `std::os::unix` occurrences. A payload type under `commands/` would be `#[cfg(unix)]`, and `output/`'s own tests could not name it on the Windows leg.
- `run()` gets a `#[cfg(windows)]` arm that prints `shep does not yet support Windows` to stderr and returns `ExitCode::Failure`. `main` stays portable. **Both arms are `async fn`** — see Task 5.
- **Every `match` on a `#[non_exhaustive]` enum from shep-core or shep-client carries a `_` arm, and the arm's behaviour is stated, never a `todo!()`.** The four that Tasks 8-11 will match: `Response` (`shep-core/src/protocol/request.rs:117`) → unknown variant is `ExitCode::Internal` with "the daemon answered with a response this client does not understand"; `BusEvent` (`.../protocol/events.rs`) → unknown variant is ignored, silently, because a follow must not die on a bus event a newer daemon added; `RpcErrorCode` → `ExitCode::Internal` (already stated at Task 5); shep-client's three error enums in `exit.rs`'s conversions → `ExitCode::Failure`.
- `crates/shep-cli/tests/cli_e2e.rs` opens with `#![cfg(unix)]`. An integration test is its own compilation unit, so `--all-targets` and `cargo test --workspace` build it on Windows otherwise.

**The unsafe boundary — non-negotiable, this is the point of the phase's design**
- `crates/shep-client/src/lib.rs` and `crates/shep-cli/src/main.rs` both carry `#![forbid(unsafe_code)]`.
- The CLI **must not** set the `SHEP_READY_FD` environment variable and **must not** call `shep_daemon::sys::adopt_fd`. Readiness is established by a successful handshake, not by an inherited descriptor.
- Do not delete, edit, or "clean up" `shep-daemon/src/sys.rs`, `BootOptions::ready_fd`, `DaemonReady`, IR-22, or IR-7. They become dead code in this phase **by design**; retiring them is the maintainer's decision and is explicitly out of scope. If you notice they are unused, that is the expected outcome — say so in your report, change nothing.

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
- **Every `recv().await` in a test is wrapped in `tokio::time::timeout`, with a message naming what did not arrive.** A test that fails by hanging gives CI a killed job instead of an assertion — this project has produced nine of them already (Task 8's `lifecycle.rs:613,652`, and Task 9's `query.rs`, whose `ping` mutation test hung past 90 seconds with this rule unapplied). This has now been rediscovered by two different reviewers, one task apart; Tasks 10-12 are briefed from this section specifically so a third rediscovery does not happen.

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
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
cargo test --workspace --all-features -- --test-threads=1
```

The Windows gate is `--workspace --all-targets`, not `-p shep-cli --all-targets`. Scoping it to one package builds shep-client's *lib* but not its *test* targets, so a `UnixListener` inside a `#[cfg(test)]` module would sail through this gate and detonate on CI instead.

The Windows gate also carries `--all-features`, which the pre-round-2 plan did not. Keep it: it is cheap, it keeps the local gate a superset of every CI leg, and it is what puts the fakes in front of the Windows compiler at all — `--all-features` turns on `test-support`, so `shep-client/src/testing.rs` is offered to a `windows-*` target. The `unix` half of its `#[cfg(all(unix, any(test, feature = "test-support")))]` gate is the only thing keeping a `UnixListener` out of that build, which makes that gate load-bearing rather than decorative. Drop `--all-features` from this gate and the fakes stop being checked on Windows.

**Explicitly out of scope for Phase 3** — do not build these, do not stub them into the clap tree:
`reload`, `scale` (no `Request` variant exists), `muster` CLI verb (restore is a boot flag; `muster save` would need a new RPC), `--with-env` (`ProcessInfo` carries no `env` field — it needs additive wire work first), dynamic shell completion (clap_complete's engine is `unstable-dynamic` upstream), **named-pipe transport / functional Windows RPC** (`ShepPaths::pipe_name()` already exists for that future work; do not wire it up, and do not build a cfg-aliased transport to make Windows "work" — Windows compiles and refuses, that is the whole deliverable), dogs, `lookout`, `whistle`, `serve`, `import`, `dev`, `runtime`, `signal`, `sendline`, `trigger`, `startup`, `set`/`get`/`unset`.

---

## File Structure

```
crates/shep-client/
  Cargo.toml                 deps: shep-core, tokio, tokio-util, tokio-stream, futures-util, bytes
                             features: test-support (exposes `testing`)
                             [[test]] request_reply / event_stream / spawn: required-features
  src/lib.rs                 #![forbid(unsafe_code)], IR-6 doctest attr, # Quick start, re-export
                             shep-core, cfg(unix)-gated mod declarations, and the crate-root
                             re-export surface every shep-cli task imports from
  src/connection.rs   [unix]  raw framed connection + bounded handshake; owns ConnectError
  src/actor.rs        [unix]  the demux task: Reply -> oneshot by id, Event -> subscriber channel
  src/client.rs       [unix]  Client handle: request / subscribe / close; owns RequestError
  src/events.rs       [unix]  EventStream (named Stream type, IR-15)
  src/spawn.rs        [unix]  connect_or_spawn + the launcher contract; owns SpawnError
  src/testing.rs      [unix]  the ONE home for every hand-rolled fake, shared across tasks
                              and crates; cfg(any(test, feature = "test-support"))
  tests/*.rs          [unix]  every test that drives a fake (see the fakes rules below)

crates/shep-cli/
  Cargo.toml                 deps: shep-core, shep-client, shep-daemon, clap, clap_complete, tokio,
                             futures-util, serde, serde_json
  src/main.rs                #![forbid(unsafe_code)], #[tokio::main], dispatch, exit-code mapping
  src/cli.rs                 clap derive tree: Cli, Commands, GlobalArgs, and EVERY argument struct
  src/exit.rs                ExitCode enum + code_str + From<RpcErrorCode> (portable)
                             + From<&*Error> [unix]
  src/output/mod.rs          OutputEnvelope, Render trait, Streams, emit / emit_error
  src/output/rows.rs         EVERY rendered payload type + its Render impl (pure tier)
  src/output/table.rs        table renderer (`render_table`)
  src/completions.rs         static shell completions (pure tier — no client, no unix)
  src/commands/lifecycle.rs  [unix] start, stop, restart, delete
  src/commands/query.rs      [unix] flock, describe, fold, ping
  src/commands/bleats.rs     [unix] bleats/logs follow
  src/commands/admin.rs      [unix] kill
  src/commands/daemon.rs     [unix] the hidden `daemon` subcommand: boot in the foreground
  src/launch.rs              [unix] spawning `shep daemon` detached, for connect_or_spawn
  tests/cli_e2e.rs           #![cfg(unix)] assert_cmd end-to-end, fresh $SHEP_HOME per test
```

`serde_json` is **not** a shep-client dependency: `encode_frame`/`decode_frame` keep it inside shep-core (`crates/shep-core/src/protocol/wire.rs:31-46`). `bytes` is listed because `encode_frame` hands back a `Bytes`; if Task 1 finds it never has to *name* the type, drop it rather than carrying a dependency for nothing.

There is no `shep-client/src/error.rs`. Each error enum lives in the module that produces it (IR-18).

**shep-client's crate-root re-export surface.** Every shep-cli task below imports from the crate root, not from module paths, so each shep-client task ends by adding its own public items to this list in `lib.rs` (all of it inside the same `#[cfg(unix)]` region as the modules):

```rust
#[cfg(unix)] pub use client::{Client, RequestError, DEADLINE_GRACE, DEFAULT_DEADLINE, START_DEADLINE};
#[cfg(unix)] pub use connection::{ConnectError, HANDSHAKE_TIMEOUT};
#[cfg(unix)] pub use events::{EventStream, Lagged};
#[cfg(unix)] pub mod spawn;   // SpawnError, SpawnOptions, SpawnOutcome, connect_or_spawn, the consts
```

`spawn` stays a public *module* rather than a flattened re-export because the exit-code contract (`spawn::DAEMON_ALREADY_RUNNING`) reads better qualified — it is a cross-crate agreement, not a convenience import.

`EVENT_CHANNEL_CAPACITY` stays `pub(crate)`. It is the number behind `Lagged`, but a consumer of the published API has no business sizing anything against it, and the one caller that does need the figure — `FakeDaemon::overrun_by` — reaches it from inside the crate. A test that wants to overrun the buffer calls `overrun_by(n)` rather than doing the arithmetic itself.

**The fakes are one module with one owner, not a fake per task.** A `#[cfg(test)] mod tests` block is not compiled into a dependency at all, so a shep-cli test can never see a fake that lives in shep-client's private test module — and a fake that lives in `connection.rs`'s test module is invisible from `spawn.rs` too. The answer is `crates/shep-client/src/testing.rs`, holding every hand-rolled double, behind `#[cfg(all(unix, any(test, feature = "test-support")))]`. shep-cli reaches it with a dev-dependency that turns the feature on:

```toml
[dev-dependencies]
shep-client = { workspace = true, features = ["test-support"] }
```

Not its own crate (the maintainer, 2026-08-08): a `shep-client-testing` that depends on shep-client while shep-client dev-depends on it is a dependency cycle, and although Cargo permits one through dev-dependencies, it is an exotic shape to leave in a codebase. The feature mirrors shep-daemon's own `test-fakes`, so the workspace has one answer to this question rather than two.

Four consequences the implementer must not discover the hard way:

- **The three fake-driven shep-client test targets need the feature switched on**, because an integration test links the ordinary library rather than the `--cfg test` one, and `cfg(test)` therefore does not reach them. Each is declared in `Cargo.toml` with `required-features = ["test-support"]`, so a bare `cargo test -p shep-client` skips them instead of failing to compile. `cargo test --workspace` runs them — shep-cli's dev-dependency turns the feature on for the whole workspace build — and so does any `--all-features` leg. A CI leg that runs `-p shep-client` alone, without features, would be testing six of that crate's twenty-five tests.
- The fakes module compiles into an **ordinary build under `test-support`**, so **`missing_docs` applies** — every helper needs a doc comment, and every returned struct a `Debug`.
- It **may not use dev-dependencies**, because under `test-support` it is not a dev build. Everything in it is built from `tokio` (`net`/`rt`/`sync`/`macros`), `tokio-util`, `futures-util`, `shep_core::protocol`, this crate's own items and `std`. No `tempfile`: every helper takes the socket path as a `&Path` and the caller owns the `TempDir`.
- The `unix` half of the module's gate is load-bearing. The Windows gate carries `--all-features`, so `test-support` is on there, and an ungated module would put a `UnixListener` into a Windows build.

**shep-client's own fake-driven tests are integration tests by preference, not necessity.** They reach only for the published surface, and linking shep-client the way a real embedder does is the honest way to prove that. Nothing forces the split — the fakes are a module of the same crate — so a test that genuinely needs a crate internal belongs in a `#[cfg(test)] mod tests` block rather than pushing a `pub(crate)` item public to reach it from `tests/`. `connection.rs`'s tests are exactly that case and stay unit tests.

**Do not make `shep-client`'s `Frames` public** to serve the fakes. It is `Framed<UnixStream, LengthDelimitedCodec>`, so exporting it would pin tokio-util's `Framed` into shep-client's public API and tie the crate to that dependency's major version. The fakes need no such thing: living inside the crate, they name the `pub(crate)` alias directly.

---

### Task 1: shep-client foundation — the bounded handshake and `ConnectError`

**Files:**
- Modify: `crates/shep-client/Cargo.toml`
- Modify: `crates/shep-client/src/lib.rs`
- Create: `crates/shep-client/src/connection.rs`
- Create: `crates/shep-client/src/testing.rs`

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
    /// The daemon refused the handshake on version skew. `client` is our own
    /// `PROTOCOL_VERSION`; `message` is the daemon's own sentence, verbatim.
    ProtocolMismatch { client: u32, message: String },
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

**`ProtocolMismatch` carries no `daemon: u32`, deliberately, and this is not an oversight to "fix".** The refusal the daemon actually sends is built at `shep-daemon/src/server.rs:387-399`:

```rust
let refusal: HelloReply = Err(RpcError {
    code: RpcErrorCode::ProtocolMismatch,
    message: format!("daemon speaks protocol {PROTOCOL_VERSION}, client sent {}", hello.protocol),
});
```

`RpcError` is `{ code, message }` and the `HelloAck` — the only frame that would carry the daemon's `protocol` as a number — never arrives on a refusal. The daemon's version therefore exists **only** inside that prose sentence. A `daemon: u32` field could be filled by nothing but scraping the string, and it would then be a supported field of a public `#[non_exhaustive]` error type, pinned by the wording of a `format!` in another crate. Keep `client` (which we know for certain) and the message (which the CLI prints verbatim with its upgrade hint). **Do not parse the message.**

`HANDSHAKE_TIMEOUT` is **defined here** rather than in `spawn.rs`, because Task 2's `Client::connect` needs it two tasks before `spawn.rs` exists. Task 4 re-exports it at `shep_client::spawn::HANDSHAKE_TIMEOUT`, which is the path the rest of this plan names.

`open` wraps connect **and** handshake in one `tokio::time::timeout`. Exceeding it is `HandshakeTimeout { after }` — a distinct condition from a refused connect, and the two must never collapse into each other: a refusal means nothing is listening, a timeout means something is bound but not answering yet. Task 4 branches on exactly that difference.

The daemon's `ProtocolMismatch` arrives as `HelloReply::Err(RpcError { code: ProtocolMismatch, .. })` and the connection then closes. Surface it as the dedicated `ConnectError::ProtocolMismatch` variant, not as a generic error — the CLI renders it with an upgrade hint and it has its own exit code.

- [ ] **Step 1: Add dependencies**

In `crates/shep-client/Cargo.toml`:

```toml
[features]
# Option: expose `testing` (the hand-rolled daemon fakes) to other crates'
# tests. Off by default — it is test scaffolding, not public API. Mirrors
# shep-daemon's `test-fakes` (`crates/shep-daemon/Cargo.toml:12`), which does
# the same job for the scripted runner.
test-support = []

# Tasks 2-4's fake-driven tests are integration tests, so they link the
# ordinary (not `--cfg test`) library and need the feature switched on.
# Declared per target rather than left to fail: a bare
# `cargo test -p shep-client` skips these three instead of failing to compile.
[[test]]
name = "request_reply"       # Task 2
required-features = ["test-support"]

[[test]]
name = "event_stream"        # Task 3
required-features = ["test-support"]

[[test]]
name = "spawn"               # Task 4
required-features = ["test-support"]

[dependencies]
shep-core.workspace = true
tokio = { workspace = true, features = ["net", "rt", "time", "sync", "macros"] }
tokio-util.workspace = true
tokio-stream.workspace = true   # Task 3: wrappers::BroadcastStream
futures-util.workspace = true
bytes.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["rt-multi-thread", "test-util"] }
tempfile.workspace = true

# Task 4's zombie reaper needs `waitpid`; dev-only and unix-only, same shape
# shep-daemon uses. The workspace default features already cover it.
[target.'cfg(unix)'.dev-dependencies]
nix.workspace = true

[lints]
workspace = true
```

`tokio-stream` is a new workspace dependency; add it to the root `Cargo.toml` under the workspace rule at the same time:

```toml
# Task 3's EventStream. `broadcast::Receiver` has no poll API, so a hand-rolled
# `Stream::poll_next` over it either does not compile or busy-spins on
# `try_recv` (which registers no waker). `wrappers::BroadcastStream` is the
# upstream answer and is gated behind the `sync` feature, which does not exist
# before 0.1.3 — verified against the crates.io index, where 0.1.0-0.1.2 declare
# only `default`/`fs`/`io-util`/`net`/`time`. Start the minimal-versions
# rehearsal at 0.1.3 and raise the floor to whatever version actually provides
# `BroadcastStreamRecvError`, naming that type in the comment.
tokio-stream = { version = "0.1.3", default-features = false, features = ["sync"] }
```

- [ ] **Step 2: Set up `lib.rs` and the shared `testing` module**

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

Then create `crates/shep-client/src/testing.rs` and declare it in `lib.rs` behind `#[cfg(all(unix, any(test, feature = "test-support")))]`. **This module is the phase's single home for hand-rolled fakes.** Tasks 2, 3 and 4 each add their own helpers to it as their types come into existence — Task 1 cannot define a helper that returns a `Client`, because `Client` does not exist until Task 2 — but nobody creates a second fake anywhere else, in any crate. Every helper is `pub` at the module root, so call sites read `shep_client::testing::fake_client_on`. The full roster, with the task that writes each one:

| Helper | Written by | Signature |
|---|---|---|
| `fake_daemon` | Task 1 | `pub async fn fake_daemon(path: &Path, reply: HelloReply) -> JoinHandle<Hello>` |
| `sample_ack` | Task 1 | `pub fn sample_ack() -> HelloAck` |
| `sample_info` | Task 1 | `pub fn sample_info() -> ProcessInfo` |
| `FakeDaemon` (the scripted one) | Task 2 creates it and its reply script; Task 3 adds the event queue; Task 11 adds the teardown script | see Task 2 |
| `fake_client_on` | Task 2 | `pub async fn fake_client_on(path: &Path) -> (Client, FakeDaemon)` |
| `fake_client_with_ack` | Task 2 | `pub async fn fake_client_with_ack(path: &Path, ack: HelloAck) -> (Client, FakeDaemon)` |
| `fake_client_capturing_envelopes` | Task 2 | `pub async fn fake_client_capturing_envelopes(path: &Path) -> (Client, mpsc::Receiver<Envelope>)` |
| `fake_client_replying_err` | Task 2 | `pub async fn fake_client_replying_err(path: &Path, code: RpcErrorCode, message: &str) -> (Client, FakeDaemon)` |
| `fake_client_out_of_order` | Task 2 | `pub async fn fake_client_out_of_order(path: &Path) -> (Client, FakeDaemon)` |
| `fake_client_event_then_reply` | Task 2 | `pub async fn fake_client_event_then_reply(path: &Path) -> (Client, FakeDaemon)` |
| `fake_client_that_closes_after_handshake` | Task 2 | `pub async fn fake_client_that_closes_after_handshake(path: &Path) -> (Client, JoinHandle<()>)` |
| `fake_client_that_never_replies` | Task 2 | `pub async fn fake_client_that_never_replies(path: &Path) -> (Client, FakeDaemon)` |
| `fake_client_with_push` | Task 3 | `pub async fn fake_client_with_push(path: &Path) -> (Client, FakeDaemon)` |
| `fast_opts` | Task 4 | `pub fn fast_opts() -> SpawnOptions` |
| `start_fake_daemon_answering_on` | Task 4 | `pub fn start_fake_daemon_answering_on(path: &Path) -> JoinHandle<()>` |
| `child_exiting_with` | Task 4 | `pub fn child_exiting_with(code: i32) -> std::io::Result<std::process::Child>` |

Every one takes the socket path as `&Path` — the module may not use `tempfile`, so the caller owns the `TempDir`. Every returned struct needs a `Debug` and every item a doc comment; see the three consequences listed under the re-export surface.

Task 1's three:

```rust
/// Serves exactly one connection, replying to the `Hello` with `reply`, then
/// closing. The returned handle yields the `Hello` the client actually sent,
/// so a test can assert on the announcement as well as on the answer.
pub async fn fake_daemon(path: &Path, reply: HelloReply) -> tokio::task::JoinHandle<Hello> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let first = frames.next().await.unwrap().unwrap();
        let hello: Hello = decode_frame(&first).unwrap();
        frames.send(encode_frame(&reply).unwrap()).await.unwrap();
        hello
    })
}

/// A `HelloAck` with a distinctive version and pid, so a test that asserts on
/// either can tell a real read from a default.
pub fn sample_ack() -> HelloAck {
    HelloAck { daemon_version: "9.9.9".into(), protocol: PROTOCOL_VERSION, pid: 4242 }
}

/// One fully-populated `ProcessInfo` — every `Option` is `Some`, so a payload
/// type's anti-drift test sees every serialized field.
pub fn sample_info() -> ProcessInfo { /* id 1, name "web", Online, pid Some, fold Some, .. */ }
```

`fake_daemon` binds before it returns, so a caller that awaits it can `connect` immediately without a sleep — that is why it is `async fn` returning a `JoinHandle` rather than a plain spawn.

- [ ] **Step 3: Write the failing handshake tests**

In `connection.rs`, driving the fakes from `crate::testing`. The fake daemon is a bare `UnixListener` that speaks the wire by hand — do NOT pull in `shep-daemon` to test the client.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fake_daemon;
    use shep_core::protocol::{HelloAck, HelloReply, PROTOCOL_VERSION, RpcError, RpcErrorCode};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn open_sends_hello_and_returns_the_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let ack = HelloAck { daemon_version: "9.9.9".into(), protocol: PROTOCOL_VERSION, pid: 4242 };
        let served = fake_daemon(&path, Ok(ack.clone())).await;

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
        let _served = fake_daemon(&path, Err(refusal)).await;

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT).await.unwrap_err();

        let ConnectError::ProtocolMismatch { client, message } = err else {
            panic!("a protocol refusal must not be flattened into a generic error, got {err:?}");
        };
        assert_eq!(client, PROTOCOL_VERSION, "`client` is our own version, not the daemon's");
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
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
git add crates/shep-client && git commit -m "feat(client): bounded connection handshake with typed protocol-mismatch refusal"
```

---

### Task 2: The connection actor — request/reply with frame demultiplexing

**Files:**
- Create: `crates/shep-client/src/actor.rs`, `crates/shep-client/src/client.rs`, `crates/shep-client/tests/request_reply.rs`
- Modify: `crates/shep-client/src/lib.rs`, `crates/shep-client/src/testing.rs`

**Interfaces:**
- Consumes: `Connection`, `ConnectError`, `HANDSHAKE_TIMEOUT`, `shep_client::testing::{fake_daemon, sample_ack, sample_info}` (all Task 1), `shep_core::protocol::{Envelope, Reply, Request, Response, ServerFrame, BusEvent, HelloAck, RpcError, RpcErrorCode}`
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

pub struct Client { /* mpsc to the actor, the HelloAck, and the socket path */ }
impl Client {
    pub async fn connect(socket: &Path) -> Result<Self, ConnectError>;
    pub async fn connect_with_timeout(socket: &Path, timeout: Duration) -> Result<Self, ConnectError>;
    pub fn daemon(&self) -> &HelloAck;
    /// The path this client is connected through.
    ///
    /// `HelloAck` carries `daemon_version`, `protocol` and `pid` and nothing
    /// else (`shep-core/src/protocol/request.rs:20-28`), so the socket path
    /// cannot be recovered from the handshake. `shep kill` (Task 11) has to
    /// watch that file disappear to know teardown finished, so the `Client`
    /// keeps the `PathBuf` it connected with.
    pub fn socket(&self) -> &Path;
    pub async fn request(&self, body: Request) -> Result<Response, RequestError>;
    pub async fn request_with_deadline(&self, body: Request, deadline: Option<Duration>)
        -> Result<Response, RequestError>;
    pub async fn close(self) -> Result<(), RequestError>;
}
```

`connect` uses `HANDSHAKE_TIMEOUT`; `connect_with_timeout` exists for callers that need another value (Task 4's fast tests are the first).

**This task also fills in its share of `testing.rs`** (roster and rules in Task 1): `fake_client_on`, `fake_client_with_ack`, `fake_client_capturing_envelopes`, `fake_client_replying_err`, `fake_client_out_of_order`, `fake_client_event_then_reply`, `fake_client_that_closes_after_handshake`, `fake_client_that_never_replies`. Each binds a `UnixListener` at the caller's `&Path`, completes the handshake with `sample_ack()` unless the helper's name says otherwise, and hands back a connected `Client`. `fake_client_capturing_envelopes` additionally decodes every `Envelope` it receives onto an `mpsc::Receiver<Envelope>` the test drains. None of them may use `tempfile`.

**`FakeDaemon` is born here, because six of those helpers return one.** It is a *script*, not a live socket: nothing it is handed is written at the moment it is handed over — the script is a queue, and the queue drains against what the client asks for. The contract in full, because Tasks 3 and 8-11 depend on every clause:

```rust
/// A scripted daemon over one accepted connection.
///
/// 1. `reply_to_list(flock)` arms the answer to the next `Request::ListFlock`.
/// 2. `list_flock_count()` reports how many `ListFlock` requests arrived, so a
///    test can prove the client cached rather than re-asked.
/// 3. `push(event)` (Task 3) appends to the pending-event queue. Queued events
///    are written **only after** the fake has observed a `Request::Subscribe`
///    AND answered it with `Response::Subscribed` — the ordering
///    `shep-daemon/src/server.rs:357` really produces. Once the subscription
///    is live, a later `push` writes through immediately.
/// 4. `queue_reply_then_event(reply, event)` (Task 3) arms one reply followed
///    immediately by one event, for the reply-before-event ordering test.
/// 5. `overrun_by(n)` (Task 3) pushes `EVENT_CHANNEL_CAPACITY + n` events in
///    one go, to force a local lag.
/// 6. `reply_shutting_down_then_unlink_after(d)` (Task 11) answers
///    `Request::KillDaemon` with `Response::ShuttingDown`, waits `d`, then
///    unlinks the socket file — the real teardown sequence, compressed.
///    `reply_shutting_down_and_never_unlink()` answers and then does nothing,
///    which is the branch `kill`'s timeout exists for.
/// 7. `close()` is always the last scripted step: it drains anything still
///    queued, then drops the connection.
#[derive(Debug)]
pub struct FakeDaemon { /* .. */ }
```

Task 2 writes clauses 1, 2 and 7; Task 3 adds 3-5; Task 11 adds 6. Each is a method on the one type, in the one module — not a second fake.

**Two error types, on purpose.** `connect` returns `ConnectError` and `request` returns `RequestError` — a failure to reach the daemon and a failure of a request the daemon accepted are different things with different exit codes, and a single enum spanning both forces every call site to match on variants it can never see. This is IR-18, and it matches shep-daemon's merged shape (`BootError`, `SnapshotError`, `RunnerError`, `SysError`, `ConnError` — one per module, no umbrella).

**Deadlines, both of them.** `request` sends `Envelope { deadline_ms: Some(DEFAULT_DEADLINE.as_millis()), .. }` — it does **not** send `None` and inherit the daemon's default silently. The client-side `tokio::time::timeout` is a second, separate bound set to `deadline + DEADLINE_GRACE`.

**Why an actor rather than the daemon's own e2e requeue loop:** the e2e client in `daemon_e2e.rs` requeues unmatched frames because it is single-task and synchronous. A shared `Client` cannot do that — two concurrent `request` calls would each need to hold frames destined for the other. One owning task that routes `Reply` by id to a per-request oneshot, and `Event` to the subscriber channel, removes the problem structurally instead of managing it.

**The race this must survive:** the supervisor emits a sheep's bus event *before* it resolves the RPC reply that caused it (`daemon_e2e.rs:161-174` documents this, empirically, from the daemon side). An `Event` frame therefore legitimately arrives ahead of the `Reply` for the very request that produced it. The actor must route it and keep reading, never treat it as an out-of-order protocol violation.

- [ ] **Step 1: Write the failing tests**

These go in `crates/shep-client/tests/request_reply.rs`, an integration test with `#![cfg(unix)]`, **not** a `#[cfg(test)] mod tests` block in `client.rs` — they drive fakes that return a `Client`, and a unit-test build would put two copies of shep-client in one binary (File Structure explains it). Everything they touch is public: `Client`, `RequestError`, `DEADLINE_GRACE`, `DEFAULT_DEADLINE`, `START_DEADLINE`.

Every test here opens with its own socket, and the fixture never invents one:

```rust
let dir = tempfile::tempdir().unwrap();
let path = dir.path().join("s.sock");
```

`dir` must stay in scope for the whole test — dropping the `TempDir` unlinks the socket. The bodies below elide that preamble; write it in each.

```rust
#[tokio::test]
async fn two_concurrent_requests_each_get_their_own_reply() {
    // Fake daemon replies to id 1 with Pong and id 2 with Flock(vec![]) — DELIBERATELY
    // out of order (2 first) to prove routing is by id, not by arrival order.
    let (client, _served) = fake_client_out_of_order(&path).await;
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
    let (client, _served) = fake_client_event_then_reply(&path).await;
    assert!(matches!(client.request(Request::Ping).await.unwrap(), Response::Pong));
}

#[tokio::test]
async fn a_daemon_side_error_reply_becomes_a_typed_rpc_error() {
    let (client, _served) =
        fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
    let RequestError::Rpc(err) = client.request(Request::ListFlock).await.unwrap_err() else {
        panic!("an Err reply must surface as RequestError::Rpc");
    };
    assert_eq!(err.code, RpcErrorCode::NotFound);
}

#[tokio::test]
async fn a_dropped_connection_fails_pending_requests_instead_of_hanging() {
    let (client, served) = fake_client_that_closes_after_handshake(&path).await;
    served.await.unwrap();
    assert!(matches!(client.request(Request::Ping).await, Err(RequestError::Closed)));
}

#[tokio::test(start_paused = true)]
async fn a_deadline_expires_client_side_when_the_daemon_never_answers() {
    let (client, _served) = fake_client_that_never_replies(&path).await;
    let err = client
        .request_with_deadline(Request::Ping, Some(Duration::from_millis(250)))
        .await
        .unwrap_err();
    let RequestError::Timeout { after } = err else { panic!("expected a client-side timeout, got {err:?}") };
    assert_eq!(after, Duration::from_millis(250) + DEADLINE_GRACE);
}

/// A `kill` that polled the wrong path would wait out its whole teardown
/// budget and report "still tearing down" against a daemon that shut down
/// cleanly — and no other test here reads `socket()` at all.
#[tokio::test]
async fn a_client_remembers_the_path_it_connected_through() {
    let (client, _served) = fake_client_on(&path).await;
    assert_eq!(client.socket(), path);
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
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;

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
- Create: `crates/shep-client/src/events.rs`, `crates/shep-client/tests/event_stream.rs`
- Modify: `crates/shep-client/src/client.rs`, `crates/shep-client/src/actor.rs`, `crates/shep-client/src/lib.rs`, `crates/shep-client/src/testing.rs`

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

**`EventStream` wraps `tokio_stream::wrappers::BroadcastStream`; it does not hand-roll a poll over `broadcast::Receiver`.** That receiver has no poll API at all — `recv` (async), `try_recv`, `blocking_recv`, `resubscribe`, `len`, `sender_strong_count`, and no `poll_recv` (tokio 1.53.1, `src/sync/broadcast.rs`). A `try_recv` inside `poll_next` compiles and is worse than not compiling: it returns `TryRecvError::Empty` without registering a waker, so the stream busy-spins. `BroadcastStream` is upstream's answer to exactly this, built on `tokio_util::sync::ReusableBoxFuture`: it moves the receiver into a boxed `recv()` future and back out on each poll.

Its real item type, verified against `tokio-stream-0.1.19/src/wrappers/broadcast.rs`:

```rust
impl<T: 'static + Clone + Send> Stream for BroadcastStream<T> {
    type Item = Result<T, BroadcastStreamRecvError>;   // BroadcastStreamRecvError::Lagged(u64)
}
```

so it already distinguishes lag from close (`RecvError::Closed` → `Poll::Ready(None)`). **Keep our own `Lagged` anyway** and map `BroadcastStreamRecvError::Lagged(n)` to `Lagged { count: n }` in `EventStream::poll_next`. Two reasons, both about the public surface: `shep_client::Lagged` is already what Task 10 matches on, and re-exporting a tokio-stream type would make a third-party crate part of shep-client's stability surface for one variant. `BroadcastStream<T>` implements `Debug` for all `T` (`:92`), so `EventStream` can `#[derive(Debug)]`. `BroadcastStreamRecvError` is **not** `#[non_exhaustive]`, so match it exhaustively — a new variant upstream should break this build loudly.

**This task adds the event half of `FakeDaemon`'s script** — `push`, `overrun_by`, `queue_reply_then_event`, and clauses 3-5 of the contract Task 2 states. Why the queue exists rather than a straight socket write: a `broadcast::Receiver` never sees a value sent before it existed, and the receiver is created inside `subscribe()`. If `push` wrote through immediately, the actor would read the frame and broadcast it to zero receivers, and every queued line would be gone before the consumer subscribed. Tasks 8-11 depend on being able to queue before the code under test subscribes.

- [ ] **Step 1: Write the failing tests**

These go in `crates/shep-client/tests/event_stream.rs`, an integration test with `#![cfg(unix)]`, for the same reason Task 2's do. `Lagged` comes from shep-client's crate root; the overrun count comes from `FakeDaemon::overrun_by`, because `EVENT_CHANNEL_CAPACITY` is `pub(crate)` and stays that way.

As in Task 2, each test opens with its own `tempfile::tempdir()` and `path`; the bodies elide it.

```rust
#[tokio::test]
async fn subscribe_yields_events_the_daemon_pushes() {
    let (client, daemon) = fake_client_with_push(&path).await;
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
    let (client, daemon) = fake_client_with_push(&path).await;
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
    let (client, daemon) = fake_client_with_push(&path).await;
    let mut stream = client.subscribe(vec!["daemon.*".into()]).await.unwrap();

    daemon.push(BusEvent::DaemonShutdown).await;
    daemon.close().await;

    assert_eq!(stream.next().await.unwrap().unwrap(), BusEvent::DaemonShutdown);
    assert!(stream.next().await.is_none(), "the stream ends after the notice, not before it");
}

/// Overrun the local buffer and require a lag notice somewhere in what comes
/// back. Deliberately NOT "the first item is `Lagged`": nothing synchronises
/// the actor's reads against the consumer's first poll, so the actor may have
/// re-broadcast only a few frames by then and the first item is a normal
/// event. Asserting position would be a flake; asserting presence is the
/// behaviour under test. An implementation that maps `RecvError::Lagged` to a
/// silent skip — or to `Poll::Ready(None)` — never produces one and fails.
#[tokio::test]
async fn a_lagging_consumer_reports_the_lag_rather_than_silently_skipping() {
    let (client, daemon) = fake_client_with_push(&path).await;
    let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

    daemon.overrun_by(8).await;
    daemon.close().await;

    let mut lag = None;
    while let Some(item) = stream.next().await {
        if let Err(Lagged { count }) = item {
            lag = Some(count);
            break;
        }
    }
    let count = lag.expect("an overrun must be reported, never silently skipped");
    assert!(count > 0, "the lag notice must say how many were lost");
}
```

- [ ] **Step 2: Run, confirm failure.** Expected: `EventStream`, `Lagged`, and `Client::subscribe` do not exist.

- [ ] **Step 3: Implement**

`EventStream` holds a `BroadcastStream<BusEvent>` over the actor's `broadcast::Receiver<BusEvent>`. `poll_next` delegates and maps the one error variant: `Some(Err(BroadcastStreamRecvError::Lagged(n)))` becomes `Some(Err(Lagged { count: n }))`, `Some(Ok(e))` becomes `Some(Ok(e))`, `None` stays `None` (the wrapper already turns `RecvError::Closed` into end-of-stream). Name the channel capacity as a `pub(crate) const EVENT_CHANNEL_CAPACITY: usize` (IR-26) with a comment tying it to the daemon's own `CONN_QUEUE = 64` (`shep-daemon/src/server.rs:39`).

This task also adds `FakeDaemon` and `fake_client_with_push` to `testing.rs`, per the contract above and the roster in Task 1.

`subscribe` issues `Request::Subscribe { topics }`, awaits `Response::Subscribed`, and hands back a receiver the actor has already been feeding — the receiver is created *before* the request is sent, which is what makes the reply-then-event ordering test pass rather than deadlock.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(client): named EventStream with daemon-drop and local-lag distinguished`

---

### Task 4: connect_or_spawn — autostart without an inherited descriptor

**Files:**
- Create: `crates/shep-client/src/spawn.rs`, `crates/shep-client/tests/spawn.rs`
- Modify: `crates/shep-client/src/lib.rs`, `crates/shep-client/src/testing.rs`

**Interfaces:**
- Consumes: `Client`, `ConnectError`, `HANDSHAKE_TIMEOUT`, `shep_client::testing::{fake_daemon, sample_ack}` (Tasks 1-2), `shep_core::protocol::{RpcError, RpcErrorCode}`
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
    /// `after` is the **budget that was exceeded** — `opts.deadline`, the
    /// value the caller configured — not the wall-clock time actually spent.
    /// The two differ by however far past the deadline the last attempt ran,
    /// and a test pins it against the configured value, so recording
    /// `started.elapsed()` here is a red test for code that looks reasonable.
    DeadlineExpired { after: Duration, last: Option<ConnectError> },
}

pub async fn connect_or_spawn<L>(socket: &Path, launch: L) -> Result<SpawnOutcome, SpawnError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send + 'static;

pub async fn connect_or_spawn_with<L>(socket: &Path, launch: L, opts: SpawnOptions)
    -> Result<SpawnOutcome, SpawnError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send + 'static;
```

The `+ 'static` is what `tokio::task::spawn_blocking` requires — see Step 3.

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

These go in `crates/shep-client/tests/spawn.rs`, an integration test with `#![cfg(unix)]`, for the same reason Tasks 2 and 3's do. Everything under test is public (`connect_or_spawn`, `connect_or_spawn_with`, `SpawnOptions`, `SpawnOutcome`, `SpawnError`, the consts).

`fast_opts`, `start_fake_daemon_answering_on` and `child_exiting_with` go into `testing.rs` (Task 1's roster). `Reaper` and `spawn_long_lived` stay local to this test file rather than joining them: `Reaper` uses `nix`, a dev-dependency — available to an integration test, but not to `testing.rs`, which under `test-support` is compiled into an ordinary build and may use no dev-dependency at all.

```rust
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use shep_client::testing::{child_exiting_with, fake_daemon, fast_opts, sample_ack,
                           start_fake_daemon_answering_on};

/// Reaps the `cat` children the launcher closures spawn.
///
/// `connect_or_spawn_with` owns each `Child` and drops it on the way out —
/// which closes the pipe and lets `cat` exit — but it never `wait()`s, and it
/// must not: in production that child IS the daemon, and waiting on it would
/// hang the CLI forever. Nothing else reaps them either, so without this every
/// `cat` stays a zombie for the life of the test binary.
#[derive(Debug, Default)]
struct Reaper(Arc<Mutex<Vec<i32>>>);

impl Reaper {
    /// A launcher that spawns a child which outlives the call and then dies on
    /// its own, with no sleep and no orphan: `cat` blocks reading a piped
    /// stdin whose write end is owned by the `Child`. When
    /// `connect_or_spawn_with` drops that `Child`, the pipe closes, `cat` sees
    /// EOF and exits. Lifetime is tied exactly to the call under test — a
    /// `sleep 60` would leak past it, and Phase 2b already paid for that
    /// lesson (`daemon_e2e.rs:118-138`).
    fn spawn_long_lived(&self) -> impl FnOnce() -> std::io::Result<std::process::Child> + Send + 'static {
        let pids = Arc::clone(&self.0);
        move || {
            let child = std::process::Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            pids.lock().unwrap().push(i32::try_from(child.id()).unwrap());
            Ok(child)
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        for pid in self.0.lock().unwrap().drain(..) {
            let _ = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid), None);
        }
    }
}

#[tokio::test]
async fn an_existing_daemon_is_used_without_launching_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _served = fake_daemon(&path, Ok(sample_ack())).await; // Task 1's helper

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
    let reaper = Reaper::default();
    // The listener must outlive the closure; park it where the test owns it.
    // `std`'s listener, not tokio's: the launcher is a sync `FnOnce`, and
    // this one must never accept anyway.
    let held: Arc<Mutex<Option<std::os::unix::net::UnixListener>>> = Arc::default();

    let err = {
        let slot = Arc::clone(&held);
        let bind_at = path.clone();
        let long_lived = reaper.spawn_long_lived();
        connect_or_spawn_with(&path, move || {
            *slot.lock().unwrap() = Some(std::os::unix::net::UnixListener::bind(&bind_at)?);
            long_lived()
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
    let dir = tempfile::tempdir().unwrap();
    // Nothing ever binds here, so the first probe fails `Connect` and the
    // launcher runs. `child_exiting_with(3)` returns an already-doomed child.
    let absent_path = dir.path().join("absent.sock");

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
async fn a_child_exiting_with_the_already_running_code_keeps_probing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let answer_on = path.clone();

    let outcome = connect_or_spawn_with(&path, move || {
        // binds AND accepts; the handle is detached deliberately — the
        // fake outlives the launcher closure and dies with the runtime.
        start_fake_daemon_answering_on(&answer_on);
        std::process::Command::new("sh").args(["-c", "exit 10"]).spawn()
    }, fast_opts()).await.unwrap();
    assert!(matches!(outcome, SpawnOutcome::Spawned(_)),
        "another process winning the race is not this process's failure");
}

#[tokio::test]
async fn a_protocol_mismatch_propagates_instead_of_spawning_a_second_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _served = fake_daemon(&path, Err(RpcError {
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

**Call it on a blocking thread, not on the runtime worker.** `launch_daemon` (Task 7) does a `DirBuilder::create`, two `File::create`s and a `Command::spawn` — four blocking syscalls with no bound. Executing them on the executor stalls every other task in the process for as long as the filesystem takes:

```rust
let launched = tokio::task::spawn_blocking(launch).await;
let child = match launched {
    Ok(result) => result.map_err(SpawnError::Launch)?,
    // A panic inside the launcher must stay a panic. Three tests in this
    // task launch with `unreachable!("...")` as their whole assertion; if a
    // JoinError swallowed that, all three would silently stop testing
    // anything.
    Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
    Err(join) => return Err(SpawnError::Launch(std::io::Error::other(join))),
};
```

`std::panic::resume_unwind` is safe, so this costs the crate nothing against `#![forbid(unsafe_code)]`. `spawn_blocking` needs tokio's `rt` feature, which Task 1 already enables, and works on a current-thread runtime — the blocking pool is separate from the worker threads.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Prove the handshake-probe test gates — both transcripts required**

The revert is one exact edit, stated so nobody has to guess at it: **inside `Connection::open`, delete the `Hello`/`HelloAck` exchange.** Keep the `connect` and the `Framed::new`, drop the `frames.send(...)` and the `frames.next()`, and fill the struct's `ack` field with a placeholder — `HelloAck { daemon_version: "revert-probe".into(), protocol: PROTOCOL_VERSION, pid: 0 }`. That is exactly a bare `UnixStream::connect` as the readiness probe, it still compiles, and every caller still gets a `Client`.

Confirm `a_socket_that_accepts_but_never_handshakes_is_not_mistaken_for_ready` **FAILS**: the kernel completes the connect into the backlog on the very first attempt, so `connect_or_spawn_with` returns `Ok(SpawnOutcome::Spawned(_))` against a socket nobody is accepting on, and the test's `unwrap_err()` panics with `called Result::unwrap_err() on an Ok value`. Restore the exchange, confirm it **PASSES**.

Paste **both** transcripts into your report. This is the single most important gate in the phase, and the rule is unconditional: **a version of this test that passes under the bare-`connect()` implementation has not gated anything and must be rewritten, not accepted.** If your first attempt passes under both, the child is exiting too early or the socket is not actually bound — fix the fixture, do not weaken the assertion.

- [ ] **Step 6: Commit** — `feat(client): connect_or_spawn probing with a real handshake, not a bare connect`

---

### Task 5: CLI skeleton — clap tree, every argument struct, exit codes, main

**Files:**
- Modify: `crates/shep-cli/Cargo.toml`, `crates/shep-cli/src/main.rs`
- Create: `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/exit.rs`

**`cli.rs` owns every argument struct in the tree.** Tasks 7 through 11 consume them and define none. Two reasons: this task's own "run the tests, confirm they pass" step is impossible if the enum names types that do not exist yet, and the whole parse surface has to sit in one portable file for the Windows tier to hold.

**Interfaces:**
- Consumes: `shep_core::protocol::RpcErrorCode`, `shep_core::paths::ShepPaths`, `clap_complete::aot::Shell`, and — behind `#[cfg(unix)]`, for the three conversions and the dispatch — `shep_client::{ConnectError, RequestError, Client, spawn::{connect_or_spawn, SpawnError, DAEMON_ALREADY_RUNNING}}`
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
    /// Print the tail of each sheep's log file and exit, instead of following
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
    /// Boot without restoring the saved muster roll
    #[arg(long)]
    pub no_restore: bool,
}
```

**`--no-restore`, not `--restore`, and for the same reason as `--no-follow`.** The tempting `#[arg(long, default_value_t = true)] pub restore: bool` is unfalsifiable: a bare `bool` field infers `ArgAction::SetTrue`, so `--restore` sets `true` and *absence* now also defaults to `true`. There is no argv that produces `false`, which means there is no way to boot without restore and Task 7's `assert!(opts.restore)` can never see the other value. Declare the negative flag and compute `let restore = !args.no_restore;` at the call site, exactly as `bleats` does with `follow`.

**On `no_follow`, and why the obvious spelling is wrong.** The tempting declaration is a `follow: bool` field with `#[arg(long, default_value_t = true, action = clap::ArgAction::Set)]`. That does **not** produce `--no-follow`. `ArgAction::Set` stores a *value*, so it yields a flag that requires one — `--follow true` — and clap offers no derive attribute that synthesises a negated long from a positive one. Verified against clap 4.6.6's own `ArgAction::Set` doctest, which parses `["mycmd", "--flag", "value"]`. Declare the negative flag directly, as above (a bare `bool` field infers `ArgAction::SetTrue`), and compute `let follow = !args.no_follow;` at the call site.

The help text above is Task 10a's wording, not Task 10's: `--no-follow` prints the tail of each sheep's log file rather than draining a bus buffer that does not exist. Task 5 shipped the earlier string; Task 10a replaces it in the same edit that changes the behaviour, so the two never disagree in a released `--help`.

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
impl ExitCode {
    /// The stable machine-readable spelling of this code, as it appears in
    /// `--format json`'s `error.code` field (`"not_found"`, `"usage"`, …).
    ///
    /// `emit_error` (Task 6) takes the code as a `&str` so `output/` never has
    /// to know the CLI's taxonomy; this is the single place those strings are
    /// written, so call sites read `emit_error(err, fmt, code.code_str(), &msg)`
    /// and no verb invents its own spelling.
    pub fn code_str(self) -> &'static str;
}
impl From<RpcErrorCode> for ExitCode { /* infallible, total */ }

// OS tier: these three error types do not exist on a Windows build.
#[cfg(unix)] impl From<&shep_client::ConnectError> for ExitCode {}
#[cfg(unix)] impl From<&shep_client::RequestError> for ExitCode {}
#[cfg(unix)] impl From<&shep_client::spawn::SpawnError> for ExitCode {}
```

`SpawnError` is qualified through `spawn` while the other two are not, and that
asymmetry is the re-export surface's, not a slip: the File Structure section
flattens `ConnectError` and `RequestError` to the crate root and deliberately
leaves `spawn` a module. There is no `shep_client::SpawnError` to name.

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
# Task 10 is the reason: `EventStream` implements `futures_util::Stream`, and the
# trait that gives it a `.next()` lives in this crate. Nothing else can supply it
# — tokio dropped its own `StreamExt` in 1.0, and a transitive futures-util
# through shep-client is not nameable from here. The Ctrl-C wiring
# (`tokio::signal::ctrl_c().map(|_| ())`) needs `FutureExt` from the same crate.
futures-util.workspace = true
# `Render: Serialize` (Task 6) needs the trait and the derive, not just the
# serializer — `serde_json` alone does not bring them.
serde.workspace = true
serde_json.workspace = true

[dev-dependencies]
# The same shep-client the `[dependencies]` entry above pulls in, asking for
# the hand-rolled fakes Tasks 8-11 drive their verbs against — see the fakes
# rules under File Structure. A dev-dependency, so the feature is on for the
# test targets and off for the shipped binary.
shep-client = { workspace = true, features = ["test-support"] }
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
    use shep_core::protocol::RpcErrorCode::*;
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

/// `emit_error` takes the code as a `&str`, so a copy-pasted `code_str` arm
/// returning a neighbour's spelling would put the wrong `error.code` in every
/// JSON failure of one command and nothing would notice. Distinctness is the
/// property; the exact words are pinned by Task 6's snapshot.
#[test]
fn every_exit_code_has_its_own_machine_readable_spelling() {
    let all = [
        ExitCode::Success, ExitCode::Failure, ExitCode::Usage, ExitCode::NotFound,
        ExitCode::InvalidConfig, ExitCode::DaemonUnreachable, ExitCode::ProtocolMismatch,
        ExitCode::SpawnFailed, ExitCode::DeadlineExceeded, ExitCode::Internal,
        ExitCode::DaemonAlreadyRunning,
    ];
    let strings: Vec<&str> = all.iter().map(|c| c.code_str()).collect();
    assert!(strings.iter().all(|s| !s.is_empty()));
    assert!(
        strings.iter().all(|s| s.chars().all(|c| c.is_ascii_lowercase() || c == '_')),
        "these go on the JSON surface: {strings:?}"
    );
    let unique: std::collections::HashSet<_> = strings.iter().collect();
    assert_eq!(unique.len(), strings.len(), "duplicated spelling: {strings:?}");
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

`main` is `#[tokio::main]`, calls `run(cli).await`, and ends with `std::process::exit(code as i32)`. Keep `main` itself trivial — all logic in `run` so it is testable. `run` has two bodies, and **both are `async`**:

```rust
#[cfg(unix)]
async fn run(cli: Cli) -> ExitCode { /* the real dispatch */ }

#[cfg(windows)]
async fn run(_cli: Cli) -> ExitCode {
    eprintln!("shep does not yet support Windows");
    ExitCode::Failure
}
```

The unix arm has no choice: every verb it calls is `async` (`start`/`stop`/`restart`/`delete`, `flock`/`describe`/`fold`/`ping`, `bleats`, `kill`, `run_daemon`), and a `block_on` inside `#[tokio::main]` panics. The Windows arm is `async` purely so the call site does not need its own `cfg` — it awaits nothing, and the resulting `clippy::unused_async` is pedantic-only, so it does not trip the gate.

Parsing happens before either — a Windows build still parses, still prints help, still validates arguments, and still runs every test in this task.

**Turning `--home` into a `ShepPaths` is this task's job, and nothing else's.** `ShepPaths::resolve` takes an env-lookup closure, not a home path (`crates/shep-core/src/paths.rs:50-51`):

```rust
pub fn resolve(env: &dyn Fn(&str) -> Option<String>, home_dir: &Path) -> Self
```

It reads exactly one variable, `SHEP_HOME`, and otherwise returns `home_dir.join(".shep")` (`:52-55`). `GlobalArgs.home` already carries `env = "SHEP_HOME"`, so clap has folded the flag and the variable into one `Option<PathBuf>` with the flag winning. The bridge is therefore a closure that answers `SHEP_HOME` from that option and passes everything else through — this is the whole body of the `resolve_paths` this task factors out further down:

```rust
let env = |key: &str| match key {
    "SHEP_HOME" => global.home.as_ref().map(|p| p.to_string_lossy().into_owned()),
    other => std::env::var(other).ok(),
};
// `$HOME` is only the FALLBACK root: `resolve` returns `home_dir.join(".shep")`
// when `SHEP_HOME` answers nothing, and ignores the argument entirely when it
// answers something. So demand `$HOME` only in that first case — reading it
// unconditionally would fail a `--home` invocation in an environment that has
// no `$HOME`, which is one of the situations `--home` exists for.
let home_dir = match (std::env::var_os("HOME"), env("SHEP_HOME")) {
    (Some(dir), _) => PathBuf::from(dir),
    (None, Some(_)) => PathBuf::new(),
    // `run` renders "set --home or $SHEP_HOME: $HOME is not set" through
    // `emit_error` before exiting on this code.
    (None, None) => return Err(ExitCode::Usage),
};
let paths = ShepPaths::resolve(&env, &home_dir);
```

Two notes an implementer will otherwise re-derive: the closure needs its `key: &str` annotation for the `&dyn Fn` coercion to infer, and `to_string_lossy` is lossy on purpose — `resolve`'s parameter is `Option<String>`, so a non-UTF-8 `--home` cannot survive the API either way.

**Every command receives an already-connected `Client`; no verb connects, and no verb autostarts.** This is the decision the phase's autostart contract turns on, and it is stated once, here:

- `Commands::Start` → `connect_or_spawn(&paths.socket, launch)` where `launch` is a `move` closure over `paths.clone()` calling `crate::launch::launch_daemon(&paths)`. This is the **only** autostart in the binary.
- Every other client-taking verb → `Client::connect(&paths.socket)`. A failure here is `ExitCode::DaemonUnreachable`; `shep stop` against a dead daemon must not spawn a supervisor in order to tell it to stop nothing.
- `Commands::Daemon` and `Commands::Completions` take no client at all and never touch the socket.

The verbs are hermetic `&Client`-takers as a result, which is what makes Tasks 8-11's unit tests possible. The cost is that autostart itself has no unit-level test in shep-cli — its coverage is Task 4's `connect_or_spawn` suite plus Task 12 case 1. That is deliberate, and worth knowing before someone "adds a quick test" by making `start` connect for itself.

**The dispatch table, in full — and every arm has a named owner.** This task writes `run`'s `match` with all thirteen arms present. No command module exists yet, so each arm's body starts as one call to a single helper:

```rust
/// Placeholder for a dispatch arm whose command module has not landed yet.
/// Every one of these is deleted by the task named in Task 5's dispatch
/// table, and Task 12 Step 2 greps for the function name to prove none is
/// left. It returns `Internal` rather than `Usage` because reaching it is a
/// fault in this binary, not in what the user typed.
fn not_wired(verb: &str) -> ExitCode {
    eprintln!("shep: {verb} is not wired yet");
    ExitCode::Internal
}
```

Each later task **replaces its own arms in the same commit that creates its module**, and its Files list says `Modify: crates/shep-cli/src/main.rs`. Nothing here is left for Task 12 to discover.

| `Commands` variant | Client | Arm | Written by |
|---|---|---|---|
| `Daemon(a)` | none | `match run_daemon(paths, &a).await { Ok(()) => ExitCode::Success, Err(e) => daemon_exit_code(&e) }` | Task 7 |
| `Start(a)` | `connect_or_spawn` | `start(&client, &mut streams, fmt, &a).await` | Task 8 |
| `Stop(a)` / `Thatlldo(a)` | `connect` | `stop(&client, &mut streams, fmt, &a).await` | Task 8 |
| `Restart(a)` | `connect` | `restart(..)` | Task 8 |
| `Delete(a)` | `connect` | `delete(..)` | Task 8 |
| `Flock` | `connect` | `flock(&client, &mut streams, fmt).await` | Task 9 |
| `Describe(a)` | `connect` | `describe(..)` | Task 9 |
| `Fold(a)` | `connect` | `fold(..)` | Task 9 |
| `Ping` | `connect` | `ping(&client, &mut streams, fmt).await` | Task 9 |
| `Bleats(a)` | `connect` | `bleats(&client, &mut streams, fmt, &a).await` | Task 10 |
| `Kill` | `connect` | `kill(client, &mut streams, fmt).await` | Task 11 |
| `Completions(a)` | none | `completions(&mut streams.out, &a)` | Task 11 |

`streams` is `output::Streams { out: &mut io::stdout().lock(), err: &mut io::stderr().lock() }`, built once in `run` (Task 6 defines it — so at Task 5 time `run` writes to the two locks directly and Task 6 swaps in the struct). `fmt` is `cli.global.format`. The `Commands::Daemon` arm is the one that makes `ExitCode::DaemonAlreadyRunning = 10` reach a real process exit status — without it the whole cold-start contract, which Task 4 and Open Question 5 both rest on, is unreachable code.

The path resolution above is `fn resolve_paths(global: &GlobalArgs) -> Result<ShepPaths, ExitCode>`, a unit under test rather than a paragraph inside `run`:

```rust
/// A `--home` that never reached `ShepPaths` is the failure mode fix D exists
/// for, and it is invisible from the outside until a daemon binds the wrong
/// socket 30 seconds later.
#[test]
fn home_wins_over_the_ambient_environment() {
    let global = GlobalArgs { home: Some("/tmp/explicit".into()), format: Format::Table, quiet: false };
    let paths = resolve_paths(&global).unwrap();
    assert_eq!(paths.home, std::path::Path::new("/tmp/explicit"));
    assert_eq!(paths.socket, std::path::Path::new("/tmp/explicit/run/shep.sock"));
}
```

That test is unix-and-windows clean (`ShepPaths` is pure shep-core), so keep it out of the `#[cfg(unix)]` block.

- [ ] **Step 5: Run tests, confirm pass, plus the Windows gate**

```bash
cargo test -p shep-cli
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

- [ ] **Step 6: Commit** — `feat(cli): clap tree, every argument struct, and the exit-code taxonomy`

---

### Task 6: Output — one envelope, two renderings, one source of truth

**Files:**
- Create: `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/output/table.rs`
- Modify: `crates/shep-cli/src/main.rs` (build the `Streams` pair once in `run`)

Pure tier — this whole module compiles and tests on Windows. It names no shep-client type, and **it owns every rendered payload type in the CLI**; the *call sites* under `commands/` are what is OS tier.

**Interfaces:**
- Consumes: `Format` and `ExitCode` from `cli.rs` / `exit.rs` (Task 5), `shep_core::protocol::{ProcessInfo, HelloAck}`, `serde::Serialize`
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

/// The two streams a command writes to.
///
/// Production wires the process's own; tests wire a pair of `Vec<u8>`, which
/// is what makes every renderer assertion hermetic and safe under the
/// parallel `cargo test` gate. `&mut dyn Write` has no `Debug`, so this needs
/// a manual one — print `Streams { .. }` and nothing else.
pub struct Streams<'a> {
    pub out: &'a mut dyn std::io::Write,
    pub err: &'a mut dyn std::io::Write,
}

/// Implemented once per command payload. The two methods are the ONLY place a
/// field's presence is decided, so a field added to one and forgotten in the
/// other is a compile error rather than a silent divergence.
pub trait Render: Serialize {
    /// Column headers for table output.
    fn headers() -> &'static [&'static str];
    /// One row per record, cells in `headers()` order.
    fn rows(&self) -> Vec<Vec<String>>;
    /// Table header -> JSON key, the documented name mapping
    /// (`UPTIME` -> `uptime_ms`, and so on).
    fn json_key_for(header: &str) -> &'static str;
    /// Serialized fields that legitimately have no column, each with a
    /// comment giving the reason. Usually empty.
    const JSON_ONLY: &'static [&'static str];
}

/// Renders `data` to `out` in `fmt`.
///
/// # Errors
/// The underlying write failed.
pub fn emit<T: Render>(out: &mut dyn std::io::Write, fmt: Format, command: &str, data: T)
    -> std::io::Result<()>;

/// Renders a failure to `err` in `fmt`. `code` is `ExitCode::code_str()`.
///
/// # Errors
/// The underlying write failed.
pub fn emit_error(err: &mut dyn std::io::Write, fmt: Format, code: &str, message: &str)
    -> std::io::Result<()>;

/// Renders any payload as the padded table, returned rather than printed so a
/// test can read it. `emit` calls this for `Format::Table`.
pub fn render_table<T: Render>(data: &T) -> String;

/// `uptime_ms` as the two largest non-zero units (`1h 2m`, `3m 4s`, `5s`, `0s`).
pub fn human_duration(ms: u64) -> String;
```

and in `output/rows.rs`, every payload type in the binary:

```rust
/// `Vec<ProcessInfo>` for `flock`, `describe`, `fold`, `start`, `stop`,
/// `restart`. A newtype because `ProcessInfo` is shep-core's and the orphan
/// rule forbids implementing our `Render` on it directly. `transparent` so the
/// JSON is a plain array of `ProcessInfo`, not a wrapper object.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct FlockRows(pub Vec<ProcessInfo>);

/// `Response::Deleted(Vec<u32>)` — the ids that were removed.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DeletedIds(pub Vec<u32>);

/// `ping`: the daemon identity the handshake already told us.
#[derive(Debug, Serialize)]
pub struct PingRow { pub daemon_version: String, pub pid: u32 }

/// `kill`: what teardown actually achieved.
#[derive(Debug, Serialize)]
pub struct KillRow { pub pid: u32, pub socket_removed: bool }
```

Every one carries `Debug` because the workspace lints deny `missing_debug_implementations`; `OutputEnvelope`'s derived `Debug` is conditional on `T: Debug`, which these satisfy.

**Payload types live here, not under `commands/`, and this is load-bearing rather than tidy.** `output/` is pure tier and its own tests name these types; `commands/` is `#[cfg(unix)]`, so a `FlockRows` defined there could not be named by a pure-tier test at all — and the Windows CI leg compiles unit tests. It is also an ordering fix: `output/` lands three tasks before `commands/query.rs`, and a test cannot call a type a later task creates. This is the same remedy Task 5 already applies to argument structs, extended to payloads. Every one of these is built from `ProcessInfo` / `HelloAck` / `u32`, and shep-core carries no `cfg` of any kind, so the pure tier really is pure.

**`Render` is not object-safe, so dispatch is generic.** `headers()` has no receiver, and `Serialize` is not dyn-compatible as a supertrait — `Box<dyn Render>` will not compile. Every call site knows its payload type statically, so `emit<T: Render>` costs nothing and removes the temptation.

**The renderers take a writer; there is no `capture()` helper, and one cannot be written here.** Capturing the process's own stdout in-process needs `dup2`-style fd redirection, `main.rs` carries `#![forbid(unsafe_code)]`, no capture crate is in the manifest, and a process-wide redirect is unsound under the parallel `cargo test --workspace --all-features` gate regardless. Threading the writer costs one parameter and makes every renderer assertion a plain `Vec<u8>` comparison. Production builds the pair once in `run`:

```rust
let mut out = std::io::stdout().lock();
let mut err = std::io::stderr().lock();
let mut streams = Streams { out: &mut out, err: &mut err };
```

Pass `&mut *streams.out` to `emit` — a bare `streams.out` moves the borrow out of the struct and the next call site will not compile.

**A write failure is `ExitCode::Failure`, except `ErrorKind::BrokenPipe`, which is `ExitCode::Success`.** `shep flock | head` closes the pipe on purpose and is not a failed command. State it once, apply it at every `emit` call site.

**Keeping the two renderings honest** is the real design problem here. `Serialize` and `rows()` can drift silently. The plan's answer: a test per payload type that serializes a fully-populated value, collects its JSON object keys, and asserts they match `headers()` after `json_key_for`. A new field added to the struct fails that test until it is either added to `headers()` or explicitly listed in `JSON_ONLY` with a reason. **Four payload types, four anti-drift tests** — the rule is per payload type, not per module.

**Stream discipline:** rendered output goes to `out`; diagnostics, progress, and errors go to `err`. Under `--format json` an error is *also* a JSON object — `{"schema_version", "error": {"code", "message"}}` — so a script piping stdout gets clean data and a script capturing stderr gets a parseable failure. Never mix the two on one stream. That the two writers really are the process's stdout and stderr is not something a unit test can see; it is asserted end-to-end in Task 12 case 5, where `assert_cmd` hands back the two streams separately.

- [ ] **Step 1: Write the failing tests**

`sample_flock()` is a local `fn sample_flock() -> FlockRows` over three fully-populated `ProcessInfo`s — every `Option` is `Some`, or the anti-drift test cannot see the field.

```rust
#[test]
fn the_json_envelope_shape_is_pinned() {
    let out = OutputEnvelope { schema_version: SCHEMA_VERSION, command: "flock", data: sample_flock() };
    insta::assert_json_snapshot!(out);
}

/// The anti-drift gate, written once and instantiated four times — once per
/// payload type, per this task's own rule. Serializes a fully-populated value,
/// collects its JSON object keys, and asserts they match `headers()` after
/// `json_key_for`, so a field added to `Serialize` and forgotten in `rows()`
/// fails here rather than silently vanishing from the table.
fn assert_no_drift<T: Render>(value: &T, first_record: fn(&serde_json::Value) -> &serde_json::Value) {
    let json = serde_json::to_value(value).unwrap();
    let keys: std::collections::BTreeSet<&str> =
        first_record(&json).as_object().unwrap().keys().map(String::as_str).collect();

    let covered: std::collections::BTreeSet<&str> = T::headers()
        .iter()
        .map(|h| T::json_key_for(h))
        .chain(T::JSON_ONLY.iter().copied())
        .collect();

    assert_eq!(
        keys, covered,
        "a serialized field is a column, or it is in JSON_ONLY with a reason — never neither"
    );
}

#[test]
fn flock_rows_do_not_drift() { assert_no_drift(&sample_flock(), |j| &j[0]); }

#[test]
fn ping_row_does_not_drift() {
    assert_no_drift(&PingRow { daemon_version: "9.9.9".into(), pid: 4242 }, |j| j);
}

#[test]
fn kill_row_does_not_drift() {
    assert_no_drift(&KillRow { pid: 4242, socket_removed: true }, |j| j);
}

// `DeletedIds` serializes as an array of bare numbers, so it has no object
// keys to drift; its test is the record-count one below.

#[test]
fn table_and_json_report_the_same_record_count() {
    let rows = sample_flock(); // three sheep
    let json = serde_json::to_value(&rows).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 3);
    assert_eq!(rows.rows().len(), 3, "the two renderings must never disagree on how many records exist");

    let ids = DeletedIds(vec![1, 2, 3, 4]);
    assert_eq!(serde_json::to_value(&ids).unwrap().as_array().unwrap().len(), 4);
    assert_eq!(ids.rows().len(), 4);
}

#[test]
fn an_empty_payload_renders_headers_rather_than_a_bare_blank() {
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

#[test]
fn human_duration_takes_the_two_largest_nonzero_units() {
    assert_eq!(human_duration(3_723_000), "1h 2m");
    assert_eq!(human_duration(184_000), "3m 4s");
    assert_eq!(human_duration(5_000), "5s");
    assert_eq!(human_duration(0), "0s");
}

#[test]
fn an_error_under_format_json_is_a_parseable_object() {
    let mut err = Vec::new();
    emit_error(&mut err, Format::Json, ExitCode::NotFound.code_str(), "no sheep matched").unwrap();

    let json: serde_json::Value = serde_json::from_slice(&err)
        .expect("under --format json a failure must be parseable, not prose");
    assert_eq!(json["schema_version"], SCHEMA_VERSION);
    assert_eq!(json["error"]["code"], "not_found");
    assert_eq!(json["error"]["message"], "no sheep matched");
}

#[test]
fn an_error_under_format_table_is_plain_text() {
    let mut err = Vec::new();
    emit_error(&mut err, Format::Table, ExitCode::NotFound.code_str(), "no sheep matched").unwrap();
    let text = String::from_utf8(err).unwrap();
    assert!(text.contains("no sheep matched"));
    assert!(serde_json::from_str::<serde_json::Value>(&text).is_err(), "table mode is not JSON");
}

/// `emit` must not put the envelope wrapper on the table surface, and must not
/// put the table on the JSON surface. An implementation that ignored `fmt` and
/// always JSON-encoded would pass both format tests above individually.
#[test]
fn emit_honours_the_format_it_is_given() {
    let mut json_out = Vec::new();
    emit(&mut json_out, Format::Json, "flock", sample_flock()).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
    assert_eq!(parsed["command"], "flock");
    assert_eq!(parsed["data"].as_array().unwrap().len(), 3);

    let mut table_out = Vec::new();
    emit(&mut table_out, Format::Table, "flock", sample_flock()).unwrap();
    let text = String::from_utf8(table_out).unwrap();
    assert!(text.contains("NAME"));
    assert!(!text.contains("schema_version"), "the envelope is a JSON-only concept");
}
```

- [ ] **Step 2: Run, confirm failure.** Expected: `OutputEnvelope`, `Render`, `Streams`, `emit`, and the four payload types do not exist.

- [ ] **Step 3: Implement**

`emit(out, fmt, command, data)` matches on `fmt` and either `serde_json::to_writer`s an `OutputEnvelope` or writes `render_table(&data)`. `emit_error(err, fmt, code, message)` does the same for the failure shape and returns `io::Result<()>` — the exit code stays the caller's, which is why it takes the code as a string it only prints.

`render_table` computes column widths from the widest cell including the header, pads to that, and separates with two spaces. No box-drawing characters — a table a user can `awk` over beats one that looks nice. An empty payload still prints the header row; a bare blank line tells the user nothing about whether the command worked.

`FlockRows::rows()` renders `pid: None` and `fold: None` as `-`, not as an empty cell — an empty cell in a padded table is indistinguishable from a rendering bug. Its `headers()` is `["ID", "NAME", "STATUS", "PID", "RESTARTS", "UPTIME", "FOLD"]` and its `json_key_for` maps `UPTIME` to `uptime_ms`; `JSON_ONLY` is empty.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Prove the anti-drift test gates**

Add a throwaway `pub note: String` field to `PingRow` (the smallest payload, so the edit is one line). Confirm `ping_row_does_not_drift` FAILS with the new key present in `keys` and absent from `covered`. Remove it, confirm green. Transcript into the report.

- [ ] **Step 6: Commit** — `feat(cli): versioned output envelope with drift-gated table and JSON renderings`

---

### Task 7: The hidden `daemon` subcommand and the detached launcher

**Files:**
- Create: `crates/shep-cli/src/commands/daemon.rs`, `crates/shep-cli/src/launch.rs`
- Modify: `crates/shep-cli/src/main.rs` — replace the `not_wired` arm for `Commands::Daemon` with the arm Task 5's dispatch table names

Both new files are OS tier: `#[cfg(unix)]` at their `mod` declarations in `main.rs`.

**Interfaces:**
- Consumes: `DaemonArgs` from `cli.rs` (Task 5 — this task defines no argument struct), `ExitCode` from `exit.rs`, `shep_daemon::{boot::{boot, BootOptions, BootError, DIR_MODE}, tokio_runner::TokioRunner}`, `shep_core::{paths::ShepPaths, config::{DaemonConfig, DaemonConfigError}}`
- Produces:
```rust
/// Everything `run_daemon` can fail with. Per-module, per IR-18, and the same
/// shape shep-daemon itself uses (`BootError`, `SnapshotError`, `RunnerError`,
/// `SysError`, `ConnError` — one per module, no umbrella).
///
/// It exists because `BootError` cannot carry a config failure: its four
/// variants are `Io { path, source }`, `AlreadyRunning { pid }`,
/// `Snapshot(SnapshotError)` and `ReadyWrite(io::Error)`
/// (`shep-daemon/src/boot.rs:895-920`), none of which represents a bad
/// `shep.toml`. Returning `Result<(), BootError>` while also being required to
/// map a `DaemonConfigError` to `ExitCode::InvalidConfig` is not satisfiable,
/// and widening `BootError` would mean editing merged daemon code that this
/// plan's Global Constraints put off-limits.
#[derive(Debug)]
pub enum DaemonRunError {
    /// `shep.toml` was unreadable as config.
    Config(shep_core::config::DaemonConfigError),
    /// The supervisor failed to come up.
    Boot(shep_daemon::boot::BootError),
}

/// Runs the supervisor in this process until a signal or `KillDaemon`.
pub async fn run_daemon(paths: ShepPaths, args: &DaemonArgs) -> Result<(), DaemonRunError>;

/// Builds the boot options from config plus flags — the unit Step 1 tests.
pub fn boot_options(config: &DaemonConfig, args: &DaemonArgs) -> BootOptions;

/// Maps a boot failure to the process exit status the parent will read.
pub fn daemon_exit_code(err: &DaemonRunError) -> ExitCode;

/// Builds the fully configured `shep daemon` command, log directory created
/// and both log files opened, but NOT spawned.
pub fn launch_command(paths: &ShepPaths) -> std::io::Result<std::process::Command>;

/// Spawns `shep daemon` detached from this process's group and terminal.
/// Returns the child so the caller can `try_wait()` it while probing.
pub fn launch_daemon(paths: &ShepPaths) -> std::io::Result<std::process::Child>;
```

`DaemonConfigError` is verified at `crates/shep-core/src/config/daemon.rs:89-94`: two variants, `Toml(String)` and `BadEnvValue(&'static str, String)`, re-exported as `shep_core::config::DaemonConfigError`. It derives `Debug + Clone + PartialEq + Eq`; `BootError` derives only `Debug`, so `DaemonRunError` derives only `Debug` too.

`daemon_exit_code` maps `Config(_)` → `ExitCode::InvalidConfig`, `Boot(BootError::AlreadyRunning { .. })` → `ExitCode::DaemonAlreadyRunning`, and every other `Boot` → `ExitCode::Failure`. `BootError` is not `#[non_exhaustive]` and has exactly four variants, so match it exhaustively.

**`run_daemon` honours `[daemon].socket`.** `BootOptions { socket: None, .. }` would make that documented config key a silent no-op — `DaemonSection::socket` exists (`shep-core/src/config/daemon.rs:19`) and `boot` reads `options.socket` at `boot.rs:497`. So: load `DaemonConfig` from `paths.daemon_config`, then

```rust
BootOptions { socket: config.daemon.socket.clone(), ready_fd: None, restore: !args.no_restore }
```

`ready_fd` stays `None`, deliberately and permanently, per this plan's Global Constraints. Then

```rust
boot(TokioRunner::new(), paths, options)
    .await
    .map_err(DaemonRunError::Boot)?
    .run()
    .await
    .map_err(DaemonRunError::Boot)
```

Both `map_err`s are load-bearing and neither is a duplicate of the other: `boot` and `RunningDaemon::run` each return `Result<_, BootError>`, so the trailing one is what makes the expression's type `Result<(), DaemonRunError>` rather than `Result<(), BootError>`.

Reading the file is the one failure mode the two `DaemonRunError` variants do not name outright: `std::fs::read_to_string(&paths.daemon_config)` with `ErrorKind::NotFound` → `None` (a missing `shep.toml` is not an error, it is `DaemonConfig::load(None, &env)`), and any *other* io error → `DaemonRunError::Boot(BootError::Io { path: paths.daemon_config.clone(), source })`. It is genuinely an IO failure on a path, `BootError::Io` is exactly that, and it lands on `ExitCode::Failure`, which is right — an unreadable config file is not the same fault as an invalid one.

**`daemon_exit_code` maps `AlreadyRunning` to `ExitCode::DaemonAlreadyRunning` (10) and every other boot failure to `ExitCode::Failure`.** This is the only channel by which a losing child in a cold-start race can tell the parent it lost: the parent holds a `std::process::Child`, not a `Result<_, BootError>`, so an exit status is all it gets. Task 4's `connect_or_spawn` reads exactly this number. Do not renumber either side alone.

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

**No test in this task spawns anything, and that is the fix, not an omission.** `launch_command` starts from `std::env::current_exe()`, which inside `cargo test` is `target/debug/deps/shep_cli-<hash>` — the *test binary*. A test that called `launch_daemon` would fork a second copy of the harness with argv `["…", "daemon"]`, which libtest reads as a name filter; it would then run every test whose name contains "daemon", pipe its output into `shepd.out.log`, and get killed mid-run. Not a fork bomb, but a real subprocess doing unrelated work, interacting differently with `--nocapture`, with `--test-threads=1` (the plan's seventh gate), and with any future test name. Assert on the configured `Command` instead. `std::process::Command` exposes `get_program() -> &OsStr`, `get_args() -> impl Iterator<Item = &OsStr>` and `get_envs() -> impl Iterator<Item = (&OsStr, Option<&OsStr>)>`, and `&OsStr == &str` resolves through `impl PartialEq<str> for OsStr` plus core's blanket `impl PartialEq<&B> for &A` — all four verified against a real build.

```rust
#[test]
fn the_launcher_never_sets_the_readiness_fd_variable() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let cmd = launch_command(&paths).unwrap(); // configured, never spawned
    assert!(
        !cmd.get_envs().any(|(k, _)| k == "SHEP_READY_FD"),
        "the whole phase design rests on readiness being a handshake, not an fd"
    );
}

#[test]
fn the_launcher_pins_shep_home_to_the_resolved_path() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let cmd = launch_command(&paths).unwrap();
    let home = cmd.get_envs()
        .find(|(k, _)| *k == "SHEP_HOME")
        .and_then(|(_, v)| v)
        .expect("the child must not re-resolve $SHEP_HOME from ambient environment");
    assert_eq!(std::path::Path::new(home), paths.home);
}

/// A launcher that forgot `.arg("daemon")` would re-exec `shep` with no
/// subcommand, print help into `shepd.out.log`, exit 2, and the parent would
/// report `DaemonExited { status: 2 }` from thirty seconds of probing.
#[test]
fn the_launcher_runs_this_binarys_hidden_daemon_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let cmd = launch_command(&paths).unwrap();
    assert_eq!(cmd.get_program(), std::env::current_exe().unwrap().as_os_str());
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, ["daemon"]);
}

/// The ENOENT that would otherwise sink the phase's headline feature on first
/// use: on a cold `$SHEP_HOME` the log directory does not exist, the redirect
/// opens two files inside it, and the daemon's own `init_dirs` only runs after
/// exec. Because `launch_command` returns without spawning, "before spawning"
/// is what this test literally observes.
#[test]
fn the_launcher_creates_the_log_directory_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    assert!(!paths.logs.exists(), "precondition: a cold $SHEP_HOME");

    let _cmd = launch_command(&paths).unwrap();

    assert!(paths.logs.is_dir(), "the redirect targets must be openable");
    assert_eq!(mode_of(&paths.logs), shep_daemon::boot::DIR_MODE);
}

#[test]
fn boot_options_pass_ready_fd_none_and_the_configured_socket() {
    let config = DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/custom.sock\"\n"), &|_| None).unwrap();
    let opts = boot_options(&config, &DaemonArgs { no_restore: false });
    assert!(opts.ready_fd.is_none(), "readiness is a handshake in this phase");
    assert_eq!(opts.socket.as_deref(), Some(std::path::Path::new("/tmp/custom.sock")));
    assert!(opts.restore, "the default is to restore the muster roll");
}

/// The negated flag has to actually reach `BootOptions`. With the old
/// `#[arg(long, default_value_t = true)] restore: bool` there was no argv that
/// produced `false`, so this case could not be written at all.
#[test]
fn no_restore_boots_without_the_muster_roll() {
    let config = DaemonConfig::load(None, &|_| None).unwrap();
    let opts = boot_options(&config, &DaemonArgs { no_restore: true });
    assert!(!opts.restore);
}

#[test]
fn already_running_gets_its_own_exit_code_and_everything_else_is_failure() {
    use DaemonRunError::{Boot, Config};
    assert_eq!(
        daemon_exit_code(&Boot(BootError::AlreadyRunning { pid: Some(7) })),
        ExitCode::DaemonAlreadyRunning
    );
    assert_eq!(
        daemon_exit_code(&Boot(BootError::AlreadyRunning { pid: None })),
        ExitCode::DaemonAlreadyRunning
    );
    assert_eq!(
        daemon_exit_code(&Boot(BootError::Io { path: "/x".into(), source: std::io::Error::other("x") })),
        ExitCode::Failure
    );
    // The mapping that was unreachable through the old `Result<(), BootError>`.
    assert_eq!(
        daemon_exit_code(&Config(DaemonConfigError::Toml("expected `=`".into()))),
        ExitCode::InvalidConfig
    );
    assert_eq!(
        daemon_exit_code(&Config(DaemonConfigError::BadEnvValue("SHEP_LOG_JSON", "maybe".into()))),
        ExitCode::InvalidConfig
    );
}
```

`test_paths(&dir)` builds a `ShepPaths` rooted at the tempdir — `ShepPaths::resolve(&|k| (k == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned()), Path::new("/nonexistent"))`. `mode_of` reads `std::fs::metadata(..).permissions().mode() & 0o777`.

**The process-group assertion moves to Task 12.** `std::process::Command` has no getter for `process_group`, so the only honest test of it is a real spawn — and a real spawn belongs in the tier that already runs the real binary and already reads `pids/shepd.pid`. Task 12 case 1 carries it.

- [ ] **Step 2: Run, confirm failure.** Expected: `launch_daemon`, `run_daemon`, `daemon_exit_code` do not exist.

- [ ] **Step 3: Implement**

`launch_command` in order: create `paths.logs` at `DIR_MODE`, then `current_exe()`, `.arg("daemon")`, `.env("SHEP_HOME", &paths.home)`, `.process_group(0)`, `.stdout(File::create(paths.logs.join("shepd.out.log"))?)`, `.stderr(...)`, `.stdin(Stdio::null())`. `launch_daemon` is `launch_command(paths)?.spawn()` and nothing else — the directory creation lives inside `launch_command` so the "created before the spawn" property is observable without spawning.

Do **not** call `.env_clear()`. The child needs `PATH` to exec anything, and clearing the environment would also drop the `SHEP_*` overrides `DaemonConfig::load` reads. Pinning `SHEP_HOME` is the point; wiping everything else is not.

`run_daemon` loads `DaemonConfig` from `paths.daemon_config` — a missing file is not an error, it is `DaemonConfig::load(None, &env)` — and maps a `DaemonConfigError` to `DaemonRunError::Config`, which `daemon_exit_code` turns into `ExitCode::InvalidConfig`, not `Failure`. The `env` closure is `&|k: &str| std::env::var(k).ok()`: `DaemonConfig::load` reads `SHEP_LOG_JSON` and `SHEP_SOCKET` from it (`config/daemon.rs:74-82`), and the child was given a real environment on purpose.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): foreground daemon subcommand and its detached launcher`

---

### Task 8: Lifecycle verbs — start, stop, restart, delete

**Files:**
- Create: `crates/shep-cli/src/commands/lifecycle.rs`
- Modify: `crates/shep-cli/src/main.rs` — replace the `not_wired` arms for `Start`, `Stop`, `Thatlldo`, `Restart` and `Delete`

OS tier: `#[cfg(unix)]` at the `mod` declaration.

**Interfaces:**
- Consumes: `StartArgs`, `SelectorArgs`, `Format` and `ExitCode` from `cli.rs` / `exit.rs` (Task 5 — this task defines no argument struct), `crate::output::{Streams, emit, emit_error, FlockRows, DeletedIds}` (Task 6 — this task defines no payload type and no `Render` impl), `crate::launch::launch_daemon` (Task 7, for the `Start` dispatch arm only), `shep_core::config::{Flockfile, FlockFormat, FlockfileError, AppConfig}`, `shep_core::selector::ProcessSelector`, `shep_core::protocol::{SelectorSpec, Request, Response, RpcErrorCode}`, `shep_client::{Client, START_DEADLINE, spawn::connect_or_spawn}`, `shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err}` (dev)
- Produces:
```rust
pub async fn start(client: &Client, streams: &mut Streams<'_>, fmt: Format, args: &StartArgs) -> ExitCode;
pub async fn stop(client: &Client, streams: &mut Streams<'_>, fmt: Format, args: &SelectorArgs) -> ExitCode;
pub async fn restart(client: &Client, streams: &mut Streams<'_>, fmt: Format, args: &SelectorArgs) -> ExitCode;
pub async fn delete(client: &Client, streams: &mut Streams<'_>, fmt: Format, args: &SelectorArgs) -> ExitCode;

/// What `resolve_target` can fail with. Module-scoped per IR-18, and named for
/// the function rather than the verb on purpose: `start`'s own failures are
/// `RequestError` and `SpawnError`, which `exit.rs` already converts. There is
/// no `impl From<&TargetError> for ExitCode` — the mapping is a `match` inside
/// `start`, so `exit.rs` stays owned entirely by Task 5.
#[derive(Debug)]
pub enum TargetError {
    Stdin(std::io::Error),
    Read { path: PathBuf, source: std::io::Error },
    Flockfile(FlockfileError),
    Unresolvable { target: String },
}

pub fn resolve_target(target: &str, name: Option<&str>, stdin: &[u8])
    -> Result<Vec<AppConfig>, TargetError>;
```

`start` maps `Unresolvable` and `Read` to `ExitCode::Usage`, `Flockfile` to `ExitCode::InvalidConfig`, and `Stdin` to `ExitCode::Failure`.

**This task never connects.** `main` has already handed it a `Client` — `connect_or_spawn` for `Start`, `Client::connect` for the other three, per Task 5's dispatch table. That is what keeps these four verbs hermetic enough to unit-test.

Note what is **not** consumed: `shep_core::config::flockfile::discover`. Bare `shep start` with no target, resolving a Flockfile from the working directory, is not in this phase's verb list, so nothing here calls `discover` — and an unused import is a `-D warnings` failure, not a harmless extra.

`start` resolves its target in this order:

1. `-` → read stdin, parse as Flockfile JSON (`FlockFormat::Json`).
2. A path whose extension `FlockFormat::from_path` recognises (`toml`, `yaml`, `yml`, `json`, `json5`) → read and `Flockfile::parse`.
3. Any other existing path → one `AppConfig::minimal(name, script)`, where `name` is `--name` if given else the file stem, and `script` is the path **as a `&str`** — `minimal` takes `(&str, &str)`, not a `Path` (`shep-core/src/config/app.rs:205`).
4. Nothing matched → a usage error naming what was tried.

`Flockfile::parse(source: &str, format: FlockFormat)` takes a `&str` (`shep-core/src/config/flockfile.rs:72`), so the stdin branch is `String::from_utf8(stdin.to_vec())` first. A non-UTF-8 stdin is `TargetError::Stdin(io::Error::new(ErrorKind::InvalidData, "stdin is not UTF-8"))` — the natural home, and it keeps `Flockfile` out of a failure it never saw.

**The Flockfile document key is `app`, not `apps`.** `Flockfile`'s public *field* is `apps` (`flockfile.rs:18-21`), which is what makes the wrong fixture so easy to write, but the wire key is renamed on purpose and the struct is `#[serde(deny_unknown_fields)]`:

```rust
// crates/shep-core/src/config/flockfile.rs:27-32
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFlockfile {
    #[serde(default, rename = "app")]
    apps: Vec<AppConfig>,
}
```

`flockfile.rs:23-26` records the reasoning — a Phase 1 forward-compat decision that a typo'd key must fail loudly, so an older binary rejects a newer Flockfile instead of silently ignoring config. `[[app]]` in TOML, `{"app": [...]}` in JSON, `app:` in YAML. Do not "fix" a fixture back to `apps` from the field name.

Do not widen this grammar (spec fidelity — the skill's checklist calls out input-format widening as a top drift risk).

**The selector grammar is total apart from three inputs**, which matters because two tests here depend on picking one that genuinely fails. `ProcessSelector::parse` (`shep-core/src/selector.rs:31-56`) returns `Err` for exactly: `""` (`SelectorError::Empty`), `"fold:"` (`EmptyFold`), and a `/…/` whose body the regex crate rejects (`BadRegex`). Everything else falls through to `Ok(Self::Name(input))` — including `"/unclosed"`, which fails the `ends_with('/')` guard and becomes a sheep literally named `/unclosed`. Use **`"/[/"`**: it starts and ends with `/`, and the body `[` is an unterminated character class, so it is a real `BadRegex`.

`--fold` sets `AppConfig::fold` on every app the target resolved to.

`Request::Start` goes out with `request_with_deadline(.., Some(START_DEADLINE))`, not the 5s default: a cold spawn plus a readiness probe routinely outruns 5 seconds, and a client-side abandonment there would report failure for a sheep that came up fine.

Selectors go through `ProcessSelector::parse` and then `SelectorSpec::from(&parsed)` (`shep-core/src/selector.rs:121`). Parse client-side even though the daemon re-parses, so a malformed selector is a fast local usage error rather than a round trip.

`stop`/`restart`/`delete` are the same shape: parse selector, one request, render. `NotFound` from the daemon is a real outcome with its own exit code, not an error to swallow.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_dash_target_reads_a_flockfile_from_stdin_as_json() {
    // `app`, not `apps` — the wire key is renamed and unknown keys are a hard
    // error (flockfile.rs:23-32).
    let apps = resolve_target("-", None, br#"{"app":[{"name":"web","script":"./srv"}]}"#).unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "web");
}

#[test]
fn a_recognised_extension_parses_as_a_flockfile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flock.toml");
    std::fs::write(&path, "[[app]]\nname = \"web\"\nscript = \"./srv\"\n").unwrap();
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

/// Drives the VERB, not `resolve_target`, so the assertion covers the mapping
/// as well as the resolution — and proves nothing reached the wire. A `start`
/// that shipped the unresolved string to the daemon and let it fail would
/// return `NotFound` after a round trip and fail both assertions.
#[tokio::test]
async fn a_target_that_matches_nothing_is_a_usage_error_naming_what_was_tried() {
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams { out: &mut out, err: &mut err };
        start(&client, &mut streams, Format::Table, &start_args("./does-not-exist")).await
    };
    assert_eq!(code, ExitCode::Usage);
    assert!(envelopes.try_recv().is_err(), "an unresolvable target must not reach the daemon");
    assert!(String::from_utf8(err).unwrap().contains("./does-not-exist"));
}

/// The client-side parse is the point: `stop` must send a compiled
/// `SelectorSpec`, not the raw string and not `All`. Nothing else here reads
/// the envelope, so without this a `stop` that always sent
/// `SelectorSpec::All` would pass every other test in the task — and would
/// stop the entire flock.
#[tokio::test]
async fn a_selector_reaches_the_wire_in_its_compiled_form() {
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
    for (input, expected) in [
        ("all", SelectorSpec::All),
        ("7", SelectorSpec::Id(7)),
        ("web", SelectorSpec::Name("web".into())),
        ("/^web-/", SelectorSpec::Regex("^web-".into())),
        ("fold:api", SelectorSpec::Fold("api".into())),
    ] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams { out: &mut out, err: &mut err };
        let _ = stop(&client, &mut streams, Format::Table, &SelectorArgs { selector: input.into() }).await;
        let sent = envelopes.recv().await.unwrap();
        assert_eq!(sent.body, Request::Stop { selector: expected }, "{input}");
    }
}

/// `"/[/"` is one of the only three inputs the selector grammar rejects — see
/// the note above. A verb that skipped the client-side parse would send it and
/// exit `NotFound` instead.
#[tokio::test]
async fn a_malformed_selector_exits_usage_without_a_round_trip() {
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams { out: &mut out, err: &mut err };
        stop(&client, &mut streams, Format::Table, &SelectorArgs { selector: "/[/".into() }).await
    };
    assert_eq!(code, ExitCode::Usage);
    assert!(envelopes.try_recv().is_err(), "a malformed selector must fail locally");
}

#[tokio::test]
async fn a_not_found_reply_exits_not_found_rather_than_being_swallowed() {
    let (client, _served) =
        fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams { out: &mut out, err: &mut err };
    let code = stop(&client, &mut streams, Format::Table, &SelectorArgs { selector: "ghost".into() }).await;
    assert_eq!(code, ExitCode::NotFound);
}

#[tokio::test]
async fn start_asks_for_the_longer_deadline() {
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams { out: &mut out, err: &mut err };
    let _ = start(&client, &mut streams, Format::Table, &start_args("./srv")).await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(sent.deadline_ms, Some(u64::try_from(START_DEADLINE.as_millis()).unwrap()));
}
```

The async tests open with the same `tempfile::tempdir()` / `path` preamble as Tasks 2-4, and `start_args("./srv")` builds a `StartArgs` whose `target` exists on disk. `resolve_target` is the pure function the first four tests call; keeping the resolution separate from the RPC is what makes them fast and hermetic.

- [ ] **Step 2: Run, confirm failure.** Expected: `resolve_target` and the four verbs do not exist.

- [ ] **Step 3: Implement**

Write `resolve_target` as a single `match` over the four branches in the order given above, with the `Unresolvable` arm last. Resist adding a fifth: a bare directory, a URL, and a glob are all things a reader will be tempted to accept, and none is in the spec.

The four verbs share one shape — one request, `emit` the response's payload, map the error. Factor that into a small helper rather than writing it four times, but keep the helper *inside* this module; it is not a general abstraction and does not belong in `output/`. `Started`/`Stopped`/`Restarted` all carry `Vec<ProcessInfo>` and render as `FlockRows`; `Deleted` carries `Vec<u32>` and renders as `DeletedIds`. `Response` is `#[non_exhaustive]`, so each match carries a `_` arm returning `ExitCode::Internal`, per Global Constraints.

Only `start` autostarts, and it does so in `main`, not here — Task 5's dispatch table hands `start` a `Client` from `connect_or_spawn` and the other three a `Client` from `Client::connect`. A failed `connect` for `stop`/`restart`/`delete` never reaches this module: it exits `DaemonUnreachable` in the dispatch. Spawning a supervisor in order to tell it to stop nothing would be absurd.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): start, stop, restart, and delete`

---

### Task 9: Query verbs — flock, describe, fold, ping

**Files:**
- Create: `crates/shep-cli/src/commands/query.rs`
- Modify: `crates/shep-cli/src/main.rs` — replace the `not_wired` arms for `Flock`, `Describe`, `Fold` and `Ping`

OS tier: `#[cfg(unix)]` at the `mod` declaration.

**Interfaces:**
- Consumes: `SelectorArgs`, `FoldArgs`, `Format` and `ExitCode` from `cli.rs` / `exit.rs` (Task 5), `crate::output::{Streams, emit, emit_error, FlockRows, PingRow}` (Task 6 — this task defines no payload type and no `Render` impl), `shep_core::protocol::{Request, Response, SelectorSpec, ProcessInfo, HelloAck, PROTOCOL_VERSION}`, `shep_client::Client`, `shep_client::testing::{fake_client_capturing_envelopes, fake_client_with_ack}` (dev)
- Produces:
```rust
pub async fn flock(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode;
pub async fn describe(client: &Client, streams: &mut Streams<'_>, fmt: Format, args: &SelectorArgs) -> ExitCode;
pub async fn fold(client: &Client, streams: &mut Streams<'_>, fmt: Format, args: &FoldArgs) -> ExitCode;
pub async fn ping(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode;
```

No types. `FlockRows` and `PingRow` are Task 6's, in the pure tier, for the reason that task states.

`flock` renders the `Vec<ProcessInfo>` table: id, name, status, pid, restarts, uptime, fold. `describe` takes a selector.

**`fold <name>` ships in this phase.** Spec §5 (`shep-v1.md:138`) and §9 (`:216`) both require it, and it is fully buildable against today's daemon: `Request::Describe { selector: SelectorSpec::Fold(name) }`. It is a one-line variation on `describe`, and omitting it silently would be worse than a documented deferral.

`ping` reports the daemon's version and pid from the `HelloAck` the client already holds — it must NOT issue a `Request::Ping` round trip to learn something the handshake already told it. It still issues the `Ping` request as a liveness check; just source the version and pid from the ack.

**`ping` is a deliberate addition beyond spec §9.** §9's verb list does not name it. It is kept anyway: it is cheap, it is the natural liveness check, and it exercises the handshake path end to end from the command line, which nothing else in this phase does. Flagged here so a reviewer does not re-raise it as an over-build.

Uptime renders as a human duration in table mode and as raw `uptime_ms` in JSON — a formatted string is not a machine-readable field. Both live in Task 6's `FlockRows` and `human_duration`; nothing here re-implements them.

- [ ] **Step 1: Write the failing tests**

The rendering tests for `FlockRows` (empty table, uptime, drift) live in Task 6 with the type. What is left here is the four verbs' wire behaviour. Each opens with the usual `tempfile::tempdir()` / `path` preamble.

```rust
#[tokio::test]
async fn fold_asks_the_daemon_for_that_fold_and_nothing_wider() {
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams { out: &mut out, err: &mut err };
    let _ = fold(&client, &mut streams, Format::Table, &FoldArgs { name: "api".into() }).await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(
        sent.body,
        Request::Describe { selector: SelectorSpec::Fold("api".into()) }
    );
}

/// The fake daemon acks with a distinctive version and pid, then replies
/// `Pong` — which carries neither. A `ping` that sourced either from the reply
/// has nothing to source them FROM, so it would emit defaults or panic.
#[tokio::test]
async fn ping_reads_version_and_pid_from_the_handshake_not_from_a_reply() {
    let ack = HelloAck { daemon_version: "9.9.9".into(), protocol: PROTOCOL_VERSION, pid: 4242 };
    let (client, _daemon) = fake_client_with_ack(&path, ack).await;

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams { out: &mut out, err: &mut err };
        ping(&client, &mut streams, Format::Json).await
    };

    assert_eq!(code, ExitCode::Success);
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["data"]["daemon_version"], "9.9.9");
    assert_eq!(json["data"]["pid"], 4242);
}

#[tokio::test]
async fn ping_still_issues_the_liveness_request() {
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams { out: &mut out, err: &mut err };
    let _ = ping(&client, &mut streams, Format::Table).await;
    assert_eq!(envelopes.recv().await.unwrap().body, Request::Ping);
}
```

- [ ] **Step 2: Run, confirm failure.** Expected: the four verbs do not exist.

- [ ] **Step 3: Implement**

Each verb is one request, one `emit`, one error mapping. `flock` sends `Request::ListFlock` and renders `Response::Flock(v)` as `FlockRows(v)`; `describe` parses its selector client-side (as Task 8 does, for the same reason) and renders `Response::Described(v)` the same way; `fold` is `describe` with `SelectorSpec::Fold(args.name)` instead of a parsed selector — one line, delegating, not a copy. `ping` sends `Request::Ping` for liveness and renders `PingRow` built from `client.daemon()`. Every `match` on `Response` carries a `_` arm returning `ExitCode::Internal`.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): flock, describe, fold, and ping`

---

### Task 10: bleats — following the log stream

> **Shipped as written (`988aacb`), then amended by Task 10a.** `--no-follow` reads the log files rather than draining the bus. What changed: the two paragraphs marked *superseded* below, and the `drain_args` tests in Step 1, which Task 10a converts to follow mode because drain mode stops being a way to end a bus test. Everything else here is what the code does today, and Task 10a assumes it.

**Files:**
- Create: `crates/shep-cli/src/commands/bleats.rs`
- Modify: `crates/shep-cli/src/main.rs` — replace the `not_wired` arm for `Bleats`

OS tier: `#[cfg(unix)]` at the `mod` declaration.

**Interfaces:**
- Consumes: `BleatsArgs`, `Format` and `ExitCode` from `cli.rs` / `exit.rs` (Task 5 — this task defines no argument struct), `crate::output::Streams` (Task 6), `shep_client::{Client, EventStream, Lagged}`, `shep_client::testing::fake_client_with_push` (dev), `shep_core::protocol::{BusEvent, Request, Response, ProcessInfo}`
- Produces:
```rust
pub async fn bleats(client: &Client, streams: &mut Streams<'_>, fmt: Format, args: &BleatsArgs)
    -> ExitCode;

/// The same verb with the interrupt injected, so the Ctrl-C branch has a test
/// that does not need a real `SIGINT` — one would kill the test runner.
/// `bleats` delegates at `tokio::signal::ctrl_c()`; the same injectable shape
/// as Task 4's `SpawnOptions` and Task 11's `kill_with_wait`.
pub async fn bleats_with_signal(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &BleatsArgs,
    interrupt: impl std::future::Future<Output = ()> + Send,
) -> ExitCode;
```

`args.no_follow` is the declared flag; the code computes `let follow = !args.no_follow;` once, at the top, and reads `follow` from there. See Task 5 for why the flag is spelled negatively. **Superseded in part by Task 10a:** the flag no longer selects a mode of the same loop, it selects a different path that never subscribes. The negative spelling and the computation stand.

**`BusEvent::LogOut`/`LogErr` carry only `{ id, line }` — no name.** Resolve ids to names with one `ListFlock` before subscribing, and cache it. An id that appears later and is not in the cache renders as the bare id rather than blocking on a refresh; do not issue a `ListFlock` per unknown line.

Filtering by selector happens **client-side** on the resolved id set: the daemon's topic filter globs on the topic string (`log.out`), which carries no identity. Say so in a comment — the next reader will assume the daemon filtered.

**A sheep's stderr line goes to `streams.out`, not to `streams.err`.** Both are the data the user asked for; `streams.err` carries *shep's* diagnostics — the lag notice, the shutdown notice — and nothing else. Interleaving a followed sheep's stderr into shep's own diagnostic stream would make `shep logs > file` silently lose half the output, and `--err` would produce an empty file. `--err`/`--out` select which of the two kinds reaches `out`; they do not select a destination.

Ctrl-C: `tokio::select!` the stream against `tokio::signal::ctrl_c()`, flush stdout, exit `Success`. A user ending a follow deliberately has not failed.

If the daemon shuts down mid-follow, print the `DaemonShutdown` notice to stderr and exit `DaemonUnreachable` — the stream ending because the daemon went away is materially different from the user pressing Ctrl-C.

A `Lagged` item prints a one-line notice to stderr and keeps going. Dropped lines are not a reason to abandon a follow, but silently swallowing them is how a user concludes a sheep went quiet.

**`--format json` does not apply the envelope here, and that is a decision, not an omission.** A follow has no end, so there is nothing to wrap. Under `Format::Json` `bleats` emits one JSON object per line — `{"schema_version", "id", "name", "stream", "line"}`, `stream` being `"out"` or `"err"` — which is a stability surface like any other and gets its Task 12 fixture. Under `Format::Table` it emits `name | line` with the id substituted for an unresolvable name.

- [ ] **Step 1: Write the failing tests**

`FakeDaemon` is scripted, per the contract Task 3 states: queued events are written only after it observes a `Request::Subscribe` and answers it, and `close()` is the last scripted step. That is what lets these tests queue everything up front and still have `bleats` — which subscribes from the inside — observe it. Each test opens with the usual `tempfile::tempdir()` / `path` preamble, and the `out`/`err` buffers follow the pattern Tasks 8 and 9 use.

```rust
#[test]
fn no_follow_parses_and_plain_bleats_still_follows() {
    let Commands::Bleats(args) = Cli::try_parse_from(["shep", "bleats"]).unwrap().command
    else { panic!() };
    assert!(!args.no_follow, "the default is to follow");

    let Commands::Bleats(args) = Cli::try_parse_from(["shep", "bleats", "--no-follow"]).unwrap().command
    else { panic!() };
    assert!(args.no_follow);

    // The flag stores NO value: `--no-follow` is `ArgAction::SetTrue`, so a
    // following token is not consumed by it and lands on the positional
    // instead. Built against clap 4.6.6 and run, not recalled:
    //   ["shep","bleats","--no-follow","true"]
    //     => Ok(BleatsArgs { selector: "true", no_follow: true, .. })
    // A `follow: bool` field with `action = ArgAction::Set` — the spelling
    // this declaration exists to avoid — would instead bind "true" as the
    // flag's value and leave the selector at its default, which is exactly
    // what this asserts against.
    let Commands::Bleats(args) =
        Cli::try_parse_from(["shep", "bleats", "--no-follow", "true"]).unwrap().command
    else { panic!() };
    assert!(args.no_follow);
    assert_eq!(args.selector, "true", "--no-follow takes no value; the token is the selector");
}

#[tokio::test]
async fn ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare() {
    let (client, daemon) = fake_client_with_push(&path).await;
    daemon.reply_to_list(vec![info(1, "web")]);
    daemon.push(BusEvent::LogOut { id: 1, line: "hello".into() }).await;
    daemon.push(BusEvent::LogOut { id: 9, line: "orphan".into() }).await;
    daemon.close().await;

    let mut out = Vec::new();
    let mut err = Vec::new();
    {
        let mut streams = Streams { out: &mut out, err: &mut err };
        bleats(&client, &mut streams, Format::Table, &drain_args("all")).await;
    }
    let out = String::from_utf8(out).unwrap();

    assert!(out.contains("web") && out.contains("hello"));
    assert!(out.contains("9") && out.contains("orphan"), "an unknown id renders bare, not blocked on");
    assert_eq!(daemon.list_flock_count(), 1, "one listing, not one per unknown line");
}

#[tokio::test]
async fn err_and_out_filter_the_two_streams() {
    for (args, kept, gone) in [
        (drain_args_err("all"), "to-stderr", "to-stdout"),
        (drain_args_out("all"), "to-stdout", "to-stderr"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.push(BusEvent::LogOut { id: 1, line: "to-stdout".into() }).await;
        daemon.push(BusEvent::LogErr { id: 1, line: "to-stderr".into() }).await;
        daemon.close().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams { out: &mut out, err: &mut err };
            bleats(&client, &mut streams, Format::Table, &args).await;
        }
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains(kept), "{kept} should have survived: {rendered}");
        assert!(!rendered.contains(gone), "{gone} should have been filtered: {rendered}");
    }
}

/// The daemon's topic filter globs on `log.out` / `log.err`, which carry no
/// identity — so this filtering CANNOT have happened server-side, and a test
/// that let the fake daemon pre-filter would prove nothing.
#[tokio::test]
async fn a_selector_filters_client_side_on_the_resolved_id_set() {
    let (client, daemon) = fake_client_with_push(&path).await;
    daemon.reply_to_list(vec![info(1, "web"), info(2, "worker")]);
    // The fake queues BOTH; only the selector may narrow them.
    daemon.push(BusEvent::LogOut { id: 1, line: "from-web".into() }).await;
    daemon.push(BusEvent::LogOut { id: 2, line: "from-worker".into() }).await;
    daemon.close().await;

    let mut out = Vec::new();
    let mut err = Vec::new();
    {
        let mut streams = Streams { out: &mut out, err: &mut err };
        bleats(&client, &mut streams, Format::Table, &drain_args("web")).await;
    }
    let out = String::from_utf8(out).unwrap();

    assert!(out.contains("from-web"));
    assert!(!out.contains("from-worker"), "the selector must narrow the resolved id set: {out}");
}

/// The stream stays open for the whole test — the fake is never closed — so
/// the ONLY thing that can end this follow is the injected interrupt. A
/// `bleats` that ignored the interrupt arm hangs and the 2s timeout fails it.
#[tokio::test]
async fn ctrl_c_during_a_follow_exits_success() {
    let (client, daemon) = fake_client_with_push(&path).await;
    daemon.reply_to_list(vec![info(1, "web")]);
    daemon.push(BusEvent::LogOut { id: 1, line: "still running".into() }).await;

    let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel::<()>();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams { out: &mut out, err: &mut err };

    // Bound, not inlined: a `&follow_args("all")` temporary is freed at the end
    // of this `let`, and `follow` outlives it into the `join!` below.
    let args = follow_args("all");
    let follow = bleats_with_signal(
        &client,
        &mut streams,
        Format::Table,
        &args,
        async { let _ = interrupt_rx.await; },
    );
    let (_, code) = tokio::join!(
        async {
            tokio::task::yield_now().await;
            let _ = interrupt_tx.send(());   // a oneshot stays ready once sent
        },
        tokio::time::timeout(Duration::from_secs(2), follow),
    );
    assert_eq!(
        code.expect("the interrupt arm must end the follow"),
        ExitCode::Success,
        "a user ending a follow deliberately has not failed"
    );
}

/// The pair that makes the shutdown branch bite. Both end in
/// `DaemonUnreachable` — the daemon went away either way — so the exit code
/// alone discriminates nothing. The NOTICE is the behaviour under test: a
/// `bleats` that never matches `BusEvent::DaemonShutdown` and just maps any
/// end-of-stream to `DaemonUnreachable` passes the first assertion of each and
/// fails the stderr assertion of the first.
#[tokio::test]
async fn a_daemon_shutdown_mid_follow_is_announced_before_the_stream_ends() {
    let (client, daemon) = fake_client_with_push(&path).await;
    daemon.reply_to_list(vec![info(1, "web")]);
    daemon.push(BusEvent::DaemonShutdown).await;   // scripted: emitted after Subscribe
    daemon.close().await;                          // scripted: last step

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams { out: &mut out, err: &mut err };
        bleats(&client, &mut streams, Format::Table, &follow_args("all")).await
    };

    assert_eq!(code, ExitCode::DaemonUnreachable);
    assert!(
        String::from_utf8(err).unwrap().contains("shutting down"),
        "the shutdown notice is what distinguishes this from the connection simply ending"
    );
}

#[tokio::test]
async fn a_stream_that_just_ends_reports_no_shutdown_notice() {
    let (client, daemon) = fake_client_with_push(&path).await;
    daemon.reply_to_list(vec![info(1, "web")]);
    daemon.close().await;   // no DaemonShutdown event at all

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams { out: &mut out, err: &mut err };
        bleats(&client, &mut streams, Format::Table, &follow_args("all")).await
    };

    assert_eq!(code, ExitCode::DaemonUnreachable);
    assert!(
        !String::from_utf8(err).unwrap().contains("shutting down"),
        "a notice the daemon never sent must not be invented"
    );
}

#[tokio::test]
async fn a_lag_notice_reaches_stderr_and_the_follow_continues() {
    let (client, daemon) = fake_client_with_push(&path).await;
    daemon.reply_to_list(vec![info(1, "web")]);
    daemon.overrun_by(8).await;                                    // forces a Lagged item
    daemon.push(BusEvent::LogOut { id: 1, line: "after".into() }).await;
    daemon.close().await;

    let mut out = Vec::new();
    let mut err = Vec::new();
    {
        let mut streams = Streams { out: &mut out, err: &mut err };
        bleats(&client, &mut streams, Format::Table, &drain_args("all")).await;
    }

    let stderr = String::from_utf8(err).unwrap();
    assert!(stderr.contains("dropped") || stderr.contains("lagged"), "a lag must be told, not swallowed: {stderr}");
    assert!(String::from_utf8(out).unwrap().contains("after"), "a lag ends the gap, not the follow");
}
```

`follow_args` / `drain_args` / `drain_args_err` / `drain_args_out` build `BleatsArgs` with `no_follow` and the two stream flags set the four ways that matter. There is no `raise_ctrl_c()`: the interrupt is a parameter of `bleats_with_signal`, because a signal source that is not an argument cannot be injected, and a real `SIGINT` would kill the test runner.

- [ ] **Step 2: Run, confirm failure.** Expected: `bleats` does not exist.

- [ ] **Step 3: Implement**

Order matters: `ListFlock` for the id→name cache **first**, then `subscribe`. Doing it the other way loses every line the daemon pushes while the listing is in flight.

The main loop is one `tokio::select!` over three arms — the event stream, the `interrupt` future, and (under `--no-follow`) an immediate break once the buffered drain is exhausted. Each arm returns a distinct `ExitCode`, so the exit code *is* the record of how the follow ended. `bleats` is `bleats_with_signal(.., tokio::signal::ctrl_c().map(|_| ()))`; nothing else differs between the two.

**Superseded in part by Task 10a: the third arm is deleted.** There is nothing buffered for it to drain — the bus is live-only fan-out — so `--no-follow` takes a separate path that reads the log files instead. The other two arms, and everything else in this step, are unchanged.

`BusEvent` is `#[non_exhaustive]`: the match carries a `_` arm that ignores the event silently, per Global Constraints. A follow must not die on a bus event a newer daemon added.

Flush `streams.out` on every exit path. A follow that ends with lines still buffered loses them, and the user has no way to know.

- [ ] **Step 4: Run tests, confirm pass**

- [ ] **Step 5: Commit** — `feat(cli): bleats log following with client-side identity resolution`

---

### Task 10a: `bleats --no-follow` reads the log files — an amendment to Task 10

**This is a follow-up amendment, not a new verb, and Task 10 is already shipped and merged (`988aacb`).** `bleats` exists, follows, resolves ids client-side, and renders both formats. What does not work is the one thing `--no-follow` was specified to do: the drain arm Task 10 built breaks out of the `select!` once the bus has nothing ready *right now*, and the bus never has anything ready, because it is live-only fan-out — `Request` carries no log-history variant (`crates/shep-core/src/protocol/request.rs` — nine variants, none of them a history ask) and a fresh `Subscribe` carries no backlog. So the arm resolves immediately and prints nothing, reliably.

**the maintainer's ruling (2026-08-08): `--no-follow` reads the log files the daemon already writes; `--follow` keeps subscribing to the bus.** `tail` against `tail -f` — the split every user of a log already has in their hands. It is also strictly more than a replay buffer would have given, because a log file holds what the sheep wrote *before* this CLI ever connected.

**The wire change it rests on has already landed (`f62e504`).** `shep_core::protocol::ProcessInfo` carries `out_file: Option<String>` and `err_file: Option<String>` — the daemon's *resolved* paths, populated in `supervisor.rs`'s `to_info` from the assembled spec, which means the app's explicit `out_file` when it configured one and the derived default otherwise. The CLI cannot work these out for itself: an explicit `out_file` may point anywhere on the filesystem, so a convention-based guess shows nothing for exactly the sheep whose owner cared enough to configure a path. `PROTOCOL_VERSION` deliberately did **not** bump, since the fields are additive — so **`None` means "this peer predates the field", never "this sheep has no log file"**, and this task's rendering of `None` has to say that and nothing stronger.

**Files:**
- Modify: `crates/shep-cli/src/commands/bleats.rs` — the `--no-follow` path, its own tests, and the seven follow-path tests that today reach for `--no-follow` only as a way to terminate
- Modify: `crates/shep-cli/src/cli.rs` — one help string

OS tier already; nothing here moves the platform split. No CHANGELOG entry of its own — Task 12 Step 4 still writes shep-cli's first, and it should describe the shipped behaviour rather than the intermediate one.

**Interfaces:** `bleats` and `bleats_with_signal` keep the signatures Task 10 published, including the injected `interrupt`. The follow path — `parse_selector`, `resolve_names`, `subscribe`, `handle_event`, the two-arm `select!` — is untouched. New, all private to the module:

```rust
/// How many lines of each log file `--no-follow` prints.
const TAIL_LINES: usize = 50;

/// The most of one log file `--no-follow` will read to find those lines.
const TAIL_WINDOW_BYTES: u64 = 256 * 1024;

/// The last [`TAIL_LINES`] lines of one log file, bounded twice over.
fn read_tail(path: &Path) -> io::Result<Vec<String>>;

/// Renders the selected files of every sheep the selector admits, in id
/// order, and returns the exit code that reports how that went.
fn tail_log_files(
    streams: &mut Streams<'_>,
    fmt: Format,
    cache: &HashMap<u32, ProcessInfo>,
    selector: &ProcessSelector,
    args: &BleatsArgs,
) -> ExitCode;
```

**The `--no-follow` path never subscribes.** One `Request::ListFlock` — the one `resolve_names` already sends, which is also where the paths come from, so there is **no extra round trip** — then the files, then exit. Delete the third `select!` arm rather than teaching it to read files: with no subscription there is no stream, no `Lagged`, no `DaemonShutdown`, and nothing for `interrupt` to race, since a bounded file read terminates on its own. `resolve_names` returns `HashMap<u32, ProcessInfo>` and already carries the paths; it needs no change at all.

**The tail is bounded twice, and both bounds are load-bearing.** A log file is arbitrarily large and a line count alone does not bound memory — one 4 GiB line without a newline defeats it. So: seek to `len.saturating_sub(TAIL_WINDOW_BYTES)`, read to end, and if that seek landed anywhere but zero, discard everything up to and including the first `\n` — a window boundary lands mid-line, and half a line rendered as a whole one is a lie. Decode the window with `String::from_utf8_lossy`: a log file is whatever the child wrote and is under no obligation to be UTF-8, and refusing to show a log over one bad byte is the wrong failure — the same trade the wire already makes for these paths. Split on `\n`, drop the trailing empty piece a file ending in a newline produces, take the last `TAIL_LINES`. Read one file at a time, so peak memory is one window whatever the flock size.

`TAIL_LINES = 50` is a default, not a limit the user can lift — `--lines` is not in this phase's CLI surface (Global Constraints' out-of-scope list). Fifty keeps `shep bleats --no-follow` against a whole flock readable at a terminal while still carrying the tail of a crash, which is what people run this for. `TAIL_WINDOW_BYTES = 256 * 1024` binds only when lines average over 5 KiB, so in ordinary use the line bound is the one that decides. A future `--lines` flag replaces the first constant; it does not change the mechanism.

Use `std::fs`, not `tokio::fs`: shep-cli's tokio does not carry the `fs` feature (`crates/shep-cli/Cargo.toml`), adding one for this would owe Global Constraints a minimal-versions rehearsal, and the read is a bounded 256 KiB on a one-shot command with nothing else on the runtime.

**`--out` / `--err` select which file is read.** Their meaning does not change, only what they act on: `--out` reads `out_file` alone, `--err` reads `err_file` alone, neither reads both. They still do not choose a *destination* — every line either flag admits lands on `streams.out`, exactly as in follow mode, and `streams.err` still carries only shep's own diagnostics. The `stream` field of a JSON line is `"out"` for a line from `out_file` and `"err"` for one from `err_file`.

**Ordering, and the limitation it admits rather than hides.** Within a file, lines come out in file order, which is append order, which is chronological. Beyond that there is no merge: a sheep's `out_file` is printed in full, then its `err_file`, and sheep are printed in ascending id order. Nothing in a log file carries a timestamp — the daemon's pump appends the child's line and a newline, nothing else — so there is no key to interleave two files on, and inventing one from arrival guesswork would produce a plausible order that is wrong precisely when a sheep writes to both streams at once. Say so in the module docs: a reader seeing all of `out` before any of `err` must not read that as "everything on stdout happened first". `--out`/`--err` reduce each sheep to one file, which is the only way to get output from this path with no seam in it. The follow path is unaffected — the bus delivers in arrival order, and that *is* chronological across both streams.

Ascending id is also mechanical, not cosmetic: the cache is a `HashMap`, whose iteration order varies run to run, and Task 12 case 4 compares this command's stdout byte-for-byte against a committed fixture. Sort the matched ids before reading anything.

**Three ways a file yields no lines. None of them abandons the command.**

- **`None` — the daemon predates the field.** One notice per sheep to `streams.err` through the existing `write_notice`, code `"log_path_unknown"`, naming the sheep and saying the shepherd did not report a log path. The exit code is unaffected: this is version skew, not a fault in this run, and rendering it as a failure would make an ordinary old-daemon case read as broken. Warn only about a path the flags actually asked for — under `--out`, a daemon that reported neither path draws one notice, not two.
- **The file is not there (`ErrorKind::NotFound`) — silent.** The daemon creates both files when it spawns the child (`crates/shep-daemon/src/tokio_runner.rs`'s `open_append`, called before the pump reads a first line), so a missing file means this sheep has not run in this `$SHEP_HOME` at all: registered but never started, or a log deleted underneath. A notice for each such sheep would put a line of shep's own text on stderr for every quiet sheep in a fresh flock. The user sees nothing for that sheep, which is exactly true. The flip side is worth stating in the module docs, because it is the capability follow mode does not have: a **stopped** sheep still has its file, so `--no-follow` shows what it said before it stopped.
- **The file is there and will not read — one notice, and a non-zero exit at the end.** Code `"log_unreadable"`, naming the path and the OS error, then on to the next file. The command still prints everything it could, and *then* returns `ExitCode::Failure`. Spec §9's rule is that no error exits 0, and `tail`'s own convention is exactly this shape: print what is readable, warn per file, exit non-zero. What must not happen is bailing out mid-flock — one sheep's permission problem hiding every other sheep's lines is the opposite of what was asked for. `None` and `NotFound` do not set this code; only a read that failed for a reason the user can act on.

Both notices go through `write_notice`, so stderr keeps the single grammar Task 10's fix round established — structured under `--format json`, prose under `--format table` — rather than growing a third.

**Exit codes on this path**, in the order they can occur: a malformed selector is still `Usage`, decided before any request; a failed `ListFlock` is still whatever `ExitCode::from(&RequestError)` yields; a write failure still goes through `write_outcome`, so `shep bleats --no-follow | head` still exits `Success` on `BrokenPipe`; an unreadable file is `Failure`; everything else is `Success`. `DaemonUnreachable` cannot come out of this path — there is no stream to end. **A selector matching nothing is not `NotFound` here**: follow mode never reported it either, and an amendment is not the place to open a divergence between the two modes. `NotFound` stays the lifecycle and query verbs' code.

**The JSON line shape does not change, and that is the whole point.** File-sourced lines go through the same `write_line`, so they are byte-for-byte the shape the maintainer settled — `{"schema_version", "id", "name", "stream", "line"}`, one object per line, no envelope — and a consumer cannot tell a file-sourced line from a bus-sourced one. Under `Format::Table` it stays `name | line`. `name` comes from the same `resolved_name` against the same cache; on this path it is always resolvable, since the paths and the name came from the same listing entry.

**The help string in `cli.rs` becomes**, verbatim:

```rust
    /// Print the tail of each sheep's log file and exit, instead of following
```

- [ ] **Step 1: Convert the seven tests that use `--no-follow` only to terminate**

`ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`, `err_and_out_filter_the_two_streams`, `a_selector_filters_client_side_on_the_resolved_id_set`, `a_lag_notice_reaches_stderr_and_the_follow_continues`, `a_dropped_notice_reaches_stderr_worded_for_the_daemon_side_cause`, `json_format_renders_the_pinned_five_key_line_shape` and `a_broken_pipe_while_writing_a_line_exits_success` are all tests *about the bus*, which reached for drain mode only because it was the one way to make the call return. After this task it is the one way to make sure the bus is never consulted, so every one of them moves to follow mode plus `daemon.close_after_subscribe().await`.

That conversion also **retires the known limitation** documented at the top of `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare` — that these tests are green only under the current-thread scheduler because nothing synchronizes on "the pushed events reached the client". `close_after_subscribe` flushes everything already queued *before* it ends the script, and a follow that runs to end-of-stream therefore observes all of it in order, on any scheduler. Delete that doc paragraph rather than leaving it asserting something that has stopped being true.

Re-check each converted test's expected exit code rather than assuming it survives: a follow ending on end-of-stream returns `DaemonUnreachable`, while `a_broken_pipe_while_writing_a_line_exits_success` returns from the write path before the stream ends and stays `Success`. Rename the helpers with them — `drain_args`/`drain_args_err`/`drain_args_out` become `no_follow_args`/`no_follow_args_err`/`no_follow_args_out` and serve Step 2's tests, and follow mode gains `follow_args_err`/`follow_args_out`.

- [ ] **Step 2: Write the failing tests for the file path**

Each writes real files into its own `tempfile::tempdir()` and points a scripted `ProcessInfo`'s `out_file`/`err_file` at them. **This task needs no new fake surface** — contrast Task 11, which had to add two `FakeDaemon` methods: `reply_to_list` already carries whatever `ProcessInfo` a test builds, and `info()` is this module's own local helper. Every `bleats(...)` call stays wrapped in the module's `RUN_TIMEOUT`, per Global Constraints' bounded-receive rule: the file path terminates on its own, so a regression that goes back to subscribing would otherwise fail by hanging.

1. `no_follow_reads_the_files_and_never_the_bus` — the file holds `from-the-file`; the fake is also handed `daemon.push(BusEvent::LogOut { id, line: "from-the-bus" })`. Assert stdout has the first and **not** the second. A `--no-follow` still wired to the bus fails the second assertion; one wired to neither fails the first.
2. `the_tail_is_bounded_by_lines` — write `TAIL_LINES + 20` numbered lines; assert the last is present, the first is absent, and exactly `TAIL_LINES` lines reach stdout. A `read_to_string` implementation prints line 1 and fails.
3. `the_tail_is_bounded_by_bytes_and_never_shows_half_a_line` — one line of `TAIL_WINDOW_BYTES + 1024` bytes, then a short final line; assert the short line is printed and no fragment of the long one is. Guards the window and the discard-the-partial-first-line rule together; an implementation that keeps the partial head emits a quarter-megabyte fragment and fails.
4. `out_and_err_select_which_file_is_read` — three ways: `--out` (out lines only), `--err` (err lines only), neither (out lines, then err lines). The third assertion is also this module's pin on within-sheep ordering.
5. `files_are_printed_in_ascending_id_order` — script the listing in **descending** id order, so the `HashMap`'s iteration order cannot be what makes it pass.
6. `a_missing_file_is_silent_and_the_rest_still_print` — one sheep's path names a file that was never created, another's is real. Assert the real sheep's line, an empty stderr, and `Success`.
7. `an_unreadable_file_is_noticed_and_exits_failure_with_the_rest_still_printed` — point `out_file` at **a directory**, not at a `chmod 000` file: opening a directory succeeds on unix and the read fails `EISDIR` deterministically, including as root, and some CI runners are root, where a `000` file is still readable. Assert the notice names the path, the other sheep's line still reached stdout, and the code is `Failure`.
8. `a_daemon_that_reported_no_path_is_noticed_not_silently_empty` — `out_file: None`, `err_file: None`. Assert the `"log_path_unknown"` notice and `Success`. An implementation that skips a `None` in silence passes every other test here and fails this one.
9. `a_file_sourced_json_line_is_the_same_five_key_shape_as_a_bus_sourced_one` — under `Format::Json`, parse the emitted line and assert the exact five-key set and each value. It sits beside the existing bus-path shape test, and renaming a field of `BleatLine` must now fail both.

- [ ] **Step 3: Run, confirm failure.** Expected: `TAIL_LINES`, `read_tail` and `tail_log_files` do not exist, so Step 2's tests do not compile. Step 1's conversions are the opposite case — they drive the untouched follow path and must be **green before Step 2 is written at all**. A red one there is a fault in the conversion, not evidence for the amendment.

- [ ] **Step 4: Implement**

- [ ] **Step 5: Run tests, confirm pass**, then the full gate list from Global Constraints, each from its own exit code.

- [ ] **Step 6: Commit** — `feat(cli): bleats --no-follow tails the log files instead of the bus`

---

### Task 11: kill and static completions

**Files:**
- Create: `crates/shep-cli/src/commands/admin.rs` (OS tier), `crates/shep-cli/src/completions.rs` (**pure tier**)
- Modify: `crates/shep-cli/src/main.rs` — replace the `not_wired` arms for `Kill` and `Completions`, **and fix the one shipped test wiring `Completions` turns false** (Step 4 below spells it out)
- Modify: `crates/shep-client/src/testing.rs` — add clause 6 of `FakeDaemon`'s contract (the two teardown scripts), with the exact signatures in Produces below. Yes, a shep-cli task edits a shep-client module: the fakes have exactly one home, and a second one built here is the failure Task 1's roster exists to prevent.

`admin.rs` is OS tier: `#[cfg(unix)]` at the `mod` declaration. `completions.rs` is **not** — it names only `cli.rs`, `clap` and `clap_complete`, all of which compile everywhere, and its tests have to run on the Windows leg like the rest of the parse surface. Putting `completions` inside `admin.rs` would drag it behind a `cfg(unix)` for no reason and leave its tests with no portable home.

**Interfaces:**
- Consumes: `CompletionArgs`, `Cli`, `Format` and `ExitCode` from `cli.rs` / `exit.rs` (Task 5 — this task defines no argument struct), `crate::output::{Streams, emit, KillRow}` (Task 6), `shep_client::Client`, `shep_client::testing::fake_client_on` (dev), `shep_core::protocol::{Request, Response}`, `clap::CommandFactory`, `clap_complete::aot::{generate, Shell}`
- Produces:
```rust
// admin.rs — OS tier
/// How long `kill` waits for the socket file to disappear after the daemon
/// acknowledges shutdown. `RunningDaemon::run` unlinks it as its last step
/// (`boot.rs:727`), behind the full kill ladder over every online sheep
/// (`:722`) — this has to cover that ladder's whole budget, not just a
/// round trip (IR-26: named, not a prose "a few seconds").
const KILL_TEARDOWN_WAIT: Duration = Duration::from_secs(10);
/// Gap between socket-existence checks while waiting out teardown. Fixed, not
/// a backoff: the wait is already bounded and short, and a backoff would only
/// delay the common case where teardown finishes in milliseconds.
///
/// Slept with `tokio::time::sleep(..).await`, never `std::thread::sleep`.
/// `#[tokio::test]` is a current-thread runtime and the fake's delayed unlink
/// is a task on that same runtime, so a blocking sleep here parks the one
/// thread that would ever run it: the socket never disappears, the poll never
/// observes what it is waiting for, and the first test hangs to the deadline
/// with no assertion — a killed CI job instead of a failure. Same rule as
/// Global Constraints' bounded-receive line, one tier down.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub async fn kill(client: Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode;
pub async fn kill_with_wait(client: Client, streams: &mut Streams<'_>, fmt: Format, wait: Duration)
    -> ExitCode;

// completions.rs — pure tier
/// Writes the completion script for `args.shell` to `out`.
pub fn completions(out: &mut dyn std::io::Write, args: &CompletionArgs) -> ExitCode;

// crates/shep-client/src/testing.rs — clause 6 of FakeDaemon's contract.
// Both are new; `FakeDaemon`'s whole surface today is `reply_to_list`,
// `reply_to_describe`, `list_flock_count`, `queue_reply_then_event`, `push`,
// `overrun_by`, `close`, `close_after_subscribe` (`testing.rs:244-357`).
impl FakeDaemon {
    /// Arms the next request to be answered `Response::ShuttingDown`, after
    /// which this connection waits `after` and then unlinks its socket file
    /// — the real teardown sequence, compressed.
    pub fn reply_shutting_down_then_unlink_after(&self, after: Duration);
    /// Arms the next request to be answered `Response::ShuttingDown` and then
    /// nothing: the socket file stays. The branch `kill`'s timeout exists for.
    pub fn reply_shutting_down_and_never_unlink(&self);
}
```

**Three things about those two that an implementer will otherwise get wrong.** They are the reason this block declares them at all — Task 10 shipped a test body that did not compile because a check covered declared surface and not the bodies calling it.

1. **`pub fn (&self)`, not `async`.** Both call sites in Step 1 take a borrowed `daemon` and do not `.await`. The nearest precedent is the wrong one to copy: `close_after_subscribe` (`testing.rs:354`) is `pub async fn` because it arms behaviour on the background task and has to go through the script channel. Copying that shape here makes both call sites discarded futures — `unused_must_use` under `-D warnings`, and the arming silently never happens. `reply_to_list` (`:253`) documents this exact trap at `:248-252`; use its `Arc<Mutex<..>>`-flag shape, as `reply_to_describe` (`:270`) and `queue_reply_then_event` (`:284`) also do. Two more `Arc<Mutex<Option<..>>>` slots threaded through `fake_client_with_ack` and `serve_scripted`, same as the three already there.
2. **The fake has no socket path to unlink — thread one down.** `serve_scripted` (`testing.rs:378-386`) receives a `UnixListener`, a `HelloAck`, a script receiver and four `Arc` slots; nothing stores a path, and `fake_client_with_ack` binds `path` and drops it. `listener.local_addr()?.as_pathname()` would work but is fallible twice over for no gain. **Thread a `PathBuf` down instead**: `fake_client_with_ack` already has `path: &Path` in hand — pass `path.to_path_buf()` as a new `serve_scripted` parameter. One owned field, no `Option`, no unwrap chain.
3. **The delayed unlink runs inline in the request arm, not deferred to the select loop.** Step 3 has `kill` drop the `Client` immediately after the reply. That closes the connection, so `frames.next()` yields `None` and `serve_scripted` breaks out of its loop (its `let Some(Ok(frame)) = frame else { break }` guard) — anything scheduled for a later turn never runs, and the test fails on `assert!(!path.exists())` having never unlinked. So: in the `OutOfOrder::Idle` branch, write the `ShuttingDown` reply, then `tokio::time::sleep(after).await`, then `std::fs::remove_file` the threaded path, all before yielding back to the `select!`.

`kill` sends `Request::KillDaemon`, expects `Response::ShuttingDown`, and then — per the wire sequence — that connection closes while the daemon finishes teardown. Do not report success on the reply alone: poll for the socket file to disappear, bounded by `KILL_TEARDOWN_WAIT`, so `shep kill && shep start` cannot race the old daemon's unlink. If the poll times out, report that teardown is still in progress rather than claiming a clean stop.

**Where the path comes from.** `Client::socket()` (Task 2). `kill` takes the client by value and drops it after the reply, so copy the path out first — `let socket = client.socket().to_path_buf();` — before the drop. The `Client` had to keep it because `HelloAck` carries only `daemon_version`, `protocol` and `pid`.

Put a comment on the poll: a *new* daemon binding the same path mid-poll would make the file reappear and the poll could in principle observe it and hang on. It is essentially unreachable — nothing starts a daemon between our own two statements, and the loser of any such race exits 10 — so the poll deliberately carries no defence against it. Said out loud so a reader does not add one.

`completions <shell>` uses `clap_complete::aot::generate` with `Cli::command()`:

```rust
pub fn completions(out: &mut dyn std::io::Write, args: &CompletionArgs) -> ExitCode {
    clap_complete::aot::generate(args.shell, &mut Cli::command(), "shep", out);
    ExitCode::Success
}
```

Note the module path: it is `aot::generate`, not `clap_complete::generate`. The top-level re-export still resolves in 4.6 but is documented as deprecated in favour of `aot` (`clap_complete/src/lib.rs:102-103`), and `clap_complete::shells` is likewise the deprecated alias for `aot`'s shell types. `generate` takes `&mut Command`, a `bin_name`, and a `&mut dyn Write`, and returns `()` — there is no `Result` to propagate.

Static only. Add a one-line note in the generated help that sheep-name completion is not yet dynamic, and name it as a Phase 4+ follow-up rather than letting it drop off spec §9's list (`docs/research/phase3-cli.md:495-496`).

- [ ] **Step 1: Write the failing tests**

In `admin.rs`, OS tier:

```rust
#[tokio::test]
async fn kill_waits_for_the_socket_to_disappear_before_reporting_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    // A fake daemon that replies ShuttingDown, waits, THEN unlinks.
    let (client, daemon) = fake_client_on(&path).await;
    daemon.reply_shutting_down_then_unlink_after(Duration::from_millis(120));

    assert!(path.exists());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams { out: &mut out, err: &mut err };
        kill(client, &mut streams, Format::Table).await
    };
    assert_eq!(code, ExitCode::Success);
    assert!(!path.exists(), "success must mean the socket is actually gone");
}

#[tokio::test]
async fn a_teardown_that_never_finishes_reports_in_progress_not_success() {
    // Fake daemon replies ShuttingDown and never unlinks. Uses an injected
    // short wait, not KILL_TEARDOWN_WAIT — the test proves the branch, not
    // that ten seconds elapse.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, daemon) = fake_client_on(&path).await;
    daemon.reply_shutting_down_and_never_unlink();

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams { out: &mut out, err: &mut err };
        kill_with_wait(client, &mut streams, Format::Table, Duration::from_millis(80)).await
    };
    assert_ne!(code, ExitCode::Success);
    assert!(path.exists(), "precondition: the fake really did leave the socket behind");
}
```

In `completions.rs`, pure tier — these run on the Windows leg like the rest of the parse surface:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::aot::Shell;   // hoisted: both tests name it

    /// Routed through `completions`, not through `clap_complete::generate`
    /// directly. A test that called the upstream function would pass against a
    /// `completions` that printed nothing, wrote to the wrong stream, ignored
    /// `args.shell` and always emitted bash, or returned `Failure` — it would
    /// be testing clap_complete, which is upstream's job.
    #[test]
    fn completions_generate_a_named_script_for_every_supported_shell() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut buf = Vec::new();
            let code = completions(&mut buf, &CompletionArgs { shell });
            assert_eq!(code, ExitCode::Success, "{shell}");
            let script = String::from_utf8(buf).unwrap();
            assert!(!script.is_empty(), "{shell} produced nothing");
            assert!(script.contains("shep"), "{shell} script must name the binary");
        }
    }

    /// A `completions` that emitted a hard-coded stub rather than OUR command
    /// tree would satisfy the test above and fail this one.
    #[test]
    fn completions_cover_the_visible_aliases() {
        let mut buf = Vec::new();
        completions(&mut buf, &CompletionArgs { shell: Shell::Bash });
        let script = String::from_utf8(buf).unwrap();
        for verb in ["flock", "list", "ls", "bleats", "logs"] {
            assert!(script.contains(verb), "{verb} missing from the bash script");
        }
    }
}
```

Three shells, not five: bash, zsh and fish are the ones the aliases and the script body actually differ across, and `Shell` is `#[non_exhaustive]` upstream so the array is a maintained list rather than an exhaustive match either way.

- [ ] **Step 2: Run, confirm failure.** Expected: `kill` and `completions` do not exist.

- [ ] **Step 3: Implement**

`kill` takes `client` **by value** and drops it after the reply: the daemon closes that connection as it tears down, and holding it would just produce a `RequestError::Closed` on the way out that the caller would have to learn to ignore. Copy `client.socket()` into a `PathBuf` before that drop.

`kill` delegates to `kill_with_wait(client, streams, fmt, KILL_TEARDOWN_WAIT)`. That is the same injectable-timing shape as Task 4's `SpawnOptions`, and for the same reason — the timeout branch needs a test, and the test must not take ten seconds to prove it.

On success `kill` emits a `KillRow { pid, socket_removed }` (Task 6's payload) so `--format json` gets a real object rather than an empty envelope; `pid` comes from `client.daemon().pid`, read before the drop.

- [ ] **Step 4: Fix the shipped `main.rs` test this task turns false**

Wiring `Completions` breaks a currently-green test, and the break is silent until the suite runs. `completions_never_resolves_paths` (`crates/shep-cli/src/main.rs:402`) asserts `run(cli).await == ExitCode::Internal` — which is `not_wired`'s code (`main.rs:103-112`), not an outcome anyone chose. The moment `main.rs:145`'s `Commands::Completions(_) => return not_wired(..)` becomes a real call to `completions`, the same invocation returns `ExitCode::Success` and the assertion fails.

Two edits, both required:

- Flip the assertion to `ExitCode::Success`. The test's actual subject is unchanged and still worth keeping: `completions` returns *before* `resolve_paths` runs, so it works with no resolvable `$HOME`.
- Rewrite the doc paragraph at `main.rs:381-385`. It reasons about a code shape that will no longer exist — "the reinstated `resolve_paths` call would simply succeed and fall through to the same `not_wired("completions")` arm, so `run(cli)` still returns `Internal` either way". After this task there is no `not_wired("completions")` arm, and the two codes being confused are `Success` and `Usage`. The paragraph's *conclusion* survives (with `$HOME` set, the assertion cannot distinguish a `resolve_paths` that runs from one that does not, and closing that gap belongs to the e2e tier), so keep the conclusion and re-derive it from the new codes. Leave the surrounding paragraphs — the unsafe-env-mutation note and the `daemon` note — alone; both are still accurate.

This is the whole of the "Modify `main.rs`" line in Files beyond the two arms themselves.

- [ ] **Step 5: Run tests, confirm pass**

- [ ] **Step 6: Commit** — `feat(cli): daemon shutdown and static shell completions`

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
    /// Call it on the `Output` — that is, immediately after `.output()` and
    /// BEFORE the assertion on `output.status`, which panics on failure.
    /// Registering after the assertion leaks exactly the daemon the guard
    /// exists to reap, in exactly the case (a failed autostart) where a
    /// leaked daemon is most likely.
    fn adopt_home(&mut self, home: &Path) { self.0.push(home.to_path_buf()); }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        for home in &self.0 {
            let Ok(text) = std::fs::read_to_string(home.join("pids/shepd.pid")) else { continue };
            let Ok(pid) = text.trim().parse::<i32>() else { continue };
            let pid = nix::unistd::Pid::from_raw(pid);
            // Group, not leader: the daemon's own children are in its group.
            // But only while the daemon really IS its own group leader —
            // signalling `-pid` when it is not reaches somebody else's group,
            // and in a test runner that group contains the harness. Case 1
            // asserts the leader property holds; this checks it rather than
            // assuming it, because Drop also runs on the path where case 1
            // failed. ESRCH here means already reaped: fall back to the
            // leader-only signal, which is a no-op in that case.
            let target = match nix::unistd::getpgid(Some(pid)) {
                Ok(pgid) if pgid == pid => nix::unistd::Pid::from_raw(-pid.as_raw()),
                _ => pid,
            };
            // ESRCH on an already-reaped daemon is the expected happy path.
            let _ = nix::sys::signal::kill(target, nix::sys::signal::Signal::SIGKILL);
        }
    }
}
```

**Every case's command chain carries `.timeout(Duration::from_secs(30))`**, before `.output()`. `assert_cmd`'s `timeout` (`assert_cmd/src/cmd.rs:108`) kills the child when it expires (`cmd.rs:484-490`) so the `Output` still comes back and the assertion still runs. Without it every case ends in an unbounded block, and case 7 is the live hazard: `shep bleats --no-follow` is the one command under test whose regression mode is *following forever*. A `.output()` on a regressed `--no-follow` never returns, and CI reports a killed job instead of the failed assertion that would name the bug. This is Global Constraints' bounded-receive rule (line 60) one tier up — the same failure, with a process where the earlier tiers had a channel.

**`write_test_script` is this tier's first fixture helper, and it is load-bearing in a non-obvious way:**

```rust
/// Writes a trivial long-running script into `dir` and returns its path.
/// The executable bit is the point: without `set_mode(0o755)` every
/// `shep start` fails EACCES and every case that starts a sheep fails
/// together, for a reason that has nothing to do with the CLI.
fn write_test_script(dir: &TempDir) -> PathBuf;
```

Set the mode with `std::os::unix::fs::PermissionsExt::set_mode` — the same `PermissionsExt` import `launch.rs`'s own test module already uses. `#![cfg(unix)]` at the top of the file makes that unconditional here.

**Cases 4 and 7 need two more helpers, and both exist to keep those cases honest rather than merely green:**

```rust
/// Writes a script that emits one marker line on stdout, optionally one on
/// stderr, and then sleeps. Same `0o755` requirement as `write_test_script`.
///
/// `None` writes to stderr not at all — not an empty line. An empty line is
/// still a line: it reaches the err file, `--no-follow` renders it, and
/// case 4's byte-exact fixture gains a second object it did not predict.
///
/// The sleep is what makes the output countable: a script that exits is
/// restarted, and each restart appends another copy of every marker, so a
/// byte-exact fixture would stop being byte-exact after the first respawn.
fn write_logging_script(dir: &TempDir, out_marker: &str, err_marker: Option<&str>) -> PathBuf;

/// Runs `shep --home <home> bleats --no-follow` with `args` appended,
/// until its stdout is non-empty or `BLEATS_DEADLINE` expires, and returns
/// the last attempt's `Output` either way. The selector and any global flag
/// ride in `args` — `--format` is declared `global = true`
/// (`crates/shep-cli/src/cli.rs`), so clap takes it after the subcommand.
///
/// The retry covers a real gap: `shep start` returns once the sheep is
/// registered and spawned, while the daemon's log pump is a separate task
/// that has not necessarily written the child's first line yet. Polling the
/// log file at its conventional path instead would tie this tier to a
/// path-derivation rule the daemon is free to change (and which an app's
/// own `out_file` overrides anyway); polling the command does not.
///
/// It returns on expiry rather than panicking, so the failure that reaches
/// CI is the caller's own assertion naming its own marker. Each attempt
/// still carries the `.timeout(Duration::from_secs(30))` every other case
/// does, so nothing here can block unbounded.
fn bleats_no_follow_until_written(home: &Path, args: &[&str]) -> Output;

/// How long `bleats_no_follow_until_written` keeps retrying.
const BLEATS_DEADLINE: Duration = Duration::from_secs(10);
```

Keep `$SHEP_HOME` shallow — the tempdir root itself, not a nested path. macOS caps `sun_path` around 104 bytes and a nested fixture path silently overruns it. The reasoning and the fixture shape to copy are at `shep-daemon/src/lib.rs:256-259` (the `test_paths` helper's own comment) and `shep-daemon/tests/daemon_e2e.rs:57-58`.

Required cases:
1. `shep start <script>` with no daemon running autostarts one, and the sheep reaches Online. **Also assert the daemon is its own process-group leader** — read `pids/shepd.pid` and check `getpgid(pid) == pid`. That is the `Command::process_group(0)` contract, and `std::process::Command` exposes no getter for it, so a real spawn is the only honest test; this tier already spawns one and already reads that pidfile.
2. A second command reuses the daemon rather than spawning a second (assert one pid across both).
3. Two concurrent `shep start` invocations against a cold `$SHEP_HOME` produce exactly one daemon and no spurious error. This is the race Phase 2b's `flock(2)` makes safe; prove the client half is safe too — the loser exits 10, `connect_or_spawn` keeps probing, and both invocations exit 0.
4. `--format json` output validates against the committed fixture for `flock`, `describe`, `start`, `ping` and `bleats --no-follow`. The first four are envelopes. The `bleats` fixture is one JSON object per line and no envelope (Task 10), and it is compared **byte-for-byte, not by containment** — which is what stops an empty stdout from passing, the exact defect this case carried while it was blocked. Every field of that line is pinned, so byte-exact is achievable: `schema_version` is 1, `name` is the `--name fixture` this case passed, `stream` is `"out"`, `line` is the marker, and `id` is **0** because `$SHEP_HOME` is fresh per test and the supervisor allocates ids from zero (`crates/shep-daemon/src/supervisor.rs:299` and `:530`). Give this case a sheep of its own, from `write_logging_script` with `None` for the stderr marker — one stdout line, nothing at all on stderr, then sleep — so the whole of stdout is one line:

```
{"schema_version":1,"id":0,"name":"fixture","stream":"out","line":"<marker>"}
```

Key order is serde's field order, which is `BleatLine`'s declaration order and does not vary between runs. Do not share case 7's `$SHEP_HOME` or its sheep: two sheep in one home means one of them is id 1. Fetch the output through `bleats_no_follow_until_written`, or the comparison races the daemon's log pump and fails against an empty stdout for a reason that has nothing to do with the schema.
5. Exit codes **and stream discipline**: a selector matching nothing exits `NotFound`; the malformed selector **`/[/`** exits `Usage` (`/unclosed` would not — it parses as a sheep named `/unclosed` and would exit `NotFound`, making the case look green while testing nothing; see Task 8's note on the three inputs the grammar rejects); **`shep --home <nonexistent> flock`** exits `DaemonUnreachable`. Pin that verb — the case does not hold for `start`, and picking the wrong one makes it fail for a reason the case text does not predict. `Start` is the only verb routed through the autostart path (`main.rs:166-171`); everything else takes the plain `connect_client` and fails on a socket that is not there. `start` instead reaches `launch_daemon` → `launch_command`, which creates `$SHEP_HOME/logs` recursively (`crates/shep-cli/src/launch.rs:52-56`) — so "nonexistent directory" stops being true mid-command, the daemon boots, and the invocation exits `Success`. Any non-autostarting verb works here; `flock` or `ping` are the obvious two. For each of the three, under `--format json`, assert **stdout is empty and stderr parses as a JSON object with `error.code`**. That claim is structurally unreachable from a unit test — `emit_error` is handed one writer — and this is the only tier that sees the two real streams separately.
6. `shep kill` stops the daemon and removes the socket.
7. `shep bleats --no-follow` prints what a sheep actually wrote to its log files. Start one named `bleater` from `write_logging_script` with a marker on each stream, then take its output from `bleats_no_follow_until_written` and assert: exit `Success`, **stdout contains both markers**, and stderr contains neither — both of a sheep's streams are the data the user asked for, and `streams.err` carries only shep's own diagnostics (Task 10). Then invoke it once more with `--out`, and assert the stdout marker is present and the stderr marker is not. The second half is the assertion that can only pass if the flag really selects a file rather than being accepted and ignored; the first half fails on any implementation that prints nothing, which is precisely what this case could not do while it was written against the bus.
8. `shep --home <tmp> start <script>` autostarts a daemon whose socket is **under `<tmp>`** — assert on the location of the socket file, not on the command exiting 0. A child that re-resolved `$SHEP_HOME` from ambient environment binds elsewhere, and only this assertion catches it:

```rust
#[test]
fn home_reaches_the_spawned_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let output = Command::cargo_bin("shep").unwrap()
        .args(["--home", dir.path().to_str().unwrap(), "start", script.to_str().unwrap()])
        .env_remove("SHEP_HOME")   // the ambient value must not be what makes this pass
        .timeout(Duration::from_secs(30))   // never block unbounded; see above
        .output()
        .unwrap();

    // Registered on the Output, before anything that can panic — a failed
    // autostart is precisely when a daemon is most likely to be left behind.
    guard.adopt_home(dir.path());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let socket = dir.path().join("run/shep.sock");
    assert!(socket.exists(), "the daemon bound somewhere other than --home");
}
```

Copy that `timeout()` → `output()` → `adopt_home` → assert ordering into every case; it is the template. All four parts, in that order — the timeout is what turns a regression into a failed assertion instead of a killed job, and `adopt_home` before the assertion is what keeps a failed case from leaking a real daemon.

> **Cases 4 and 7 were blocked on a decision; it has been made.** Both used to rest on `shep bleats --no-follow` producing output from the daemon's bus, which it cannot: the bus is live-only fan-out and `Request` has no log-history variant to ask for a backlog with. The maintainer ruled on 2026-08-08 that `--no-follow` reads the log files instead, and Task 10a carries that change. **Both cases therefore depend on Task 10a being merged** — written against Task 10's shipped `--no-follow` they assert on output that command cannot produce. The two traps the old note named still apply to whoever writes them: do not weaken case 7 to "exits `Success`" (a `bleats` that emits nothing at all passes that), and do not commit a fixture that an empty stdout could match.

- [ ] **Step 1: Write all eight cases, run, confirm they fail** — cases 1-3, 5, 6 and 8 depend on nothing beyond Task 11; cases 4 and 7 need Task 10a merged first.
- [ ] **Step 2: Confirm the dispatch is complete**

`grep -rn "not_wired" crates/shep-cli/src` must return **nothing**. Task 5's dispatch table names the task that replaces each placeholder arm; this is the check that none was missed. Anything else these tests expose as missing is a defect in the task that owned it — fix it there, and say so in the report.
- [ ] **Step 3: Run the full gate list from Global Constraints, each from its own exit code**
- [ ] **Step 4: Write both CHANGELOGs** — including the `Usage = 2` collision note for the future `runtime` phase, the `DaemonAlreadyRunning = 10` cross-crate contract, and shep-client's public API as a new stability surface.
- [ ] **Step 5: Commit** — `test(cli): end-to-end tier with autostart, concurrency, and pinned JSON fixtures`

---

## Exit criteria

1. All twelve tasks complete and individually reviewed.
2. Every gate in Global Constraints green from its own exit code — including `cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu`.
3. `grep -rn "unsafe" crates/shep-client/src crates/shep-cli/src | grep -v "forbid(unsafe_code)"` returns nothing. (The unfiltered grep can never pass: `#![forbid(unsafe_code)]` contains the string it searches for.)
4. `grep -rn "SHEP_READY_FD\|adopt_fd" crates/shep-cli/src crates/shep-client/src` returns nothing.
5. `grep -rn "not_wired" crates/shep-cli/src` returns nothing — every arm in Task 5's dispatch table has been replaced by the task that owns it.
6. The three revert-proof transcripts (Task 2 routing, Task 4 handshake probe, Task 6 anti-drift) are in the reports, each with BOTH the broken-FAIL and restored-PASS runs.
7. `shep start`, `shep flock`, `shep kill` and `shep bleats --no-follow` work against a real daemon on a clean `$SHEP_HOME`, with `bleats --no-follow`'s stdout compared byte-for-byte against its committed fixture (Task 12 cases 4 and 7). **`bleats` in follow mode is not demonstrated end-to-end** — no case subscribes to a real daemon's bus — and is covered only by Task 10's unit tier against `FakeDaemon`. Report it in those terms; "works against a real daemon" must not be left standing for both modes.
8. A report to the maintainer listing: the now-dead readiness-pipe surface with evidence, and every judgment call made on her behalf.

## Open questions for the maintainer — do not resolve these unilaterally

1. **Retire the readiness pipe?** This phase makes `sys.rs`, `BootOptions::ready_fd`, `DaemonReady`, and `READY_FD_ENV` unreachable in production. Deleting them would let every crate in the workspace be `#![forbid(unsafe_code)]` and would retire IR-22 as satisfied-by-construction. Analysis: `docs/research/phase3-readiness-decision.md`.
2. **The bind→serve gap.** The daemon signals nothing between binding its socket (`boot.rs:498`) and accepting on it (`boot.rs:707`), so a client can connect into the backlog and wait through the whole muster restore. Phase 3 absorbs this with a 30s deadline plus a bounded per-attempt handshake. The real fix is ordering — either start `serve()` before the restore, or move readiness to the point where accepting begins. Both change merged daemon behaviour, so neither is in this phase.
3. **`Usage = 2` collides** with spec §9's fail-fast code for `runtime`. Taken as clap's convention for now.
4. **`--with-env` cannot be built yet** — `ProcessInfo` has no `env` field. It needs an additive wire change first.
5. **`DaemonAlreadyRunning = 10` is a new exit code**, not one spec §9 enumerates, and it is a cross-crate contract: shep-client hard-codes the same 10 so it can read a dead child's status. The alternative — treating every non-zero child status as fatal — makes the concurrent-cold-start case (Task 12 case 3) fail. Blessing the number, or picking a different one, is yours.
6. **`completions` or `completion`?** Spec §9 says "clap_complete completions" in prose without naming a verb; `docs/research/phase3-cli.md:451` writes `shep completion <shell>`, singular. The plan uses `Completions`. Whichever you pick becomes a stable CLI surface, so it is worth one word of your time. (An alias for the other spelling is trivial if you want both.)
7. ~~**`bleats --format json` emits JSON lines, not an envelope.**~~ **SETTLED (the maintainer, 2026-08-08): JSON lines, no envelope.** A follow has no end, so there is nothing to wrap: `bleats` emits one object per line, `{"schema_version", "id", "name", "stream", "line"}`, `stream` being `"out"` or `"err"`. This is the one place the output schema changes shape by command, and it is a stability surface with a committed fixture from day one. The rejected alternative was an envelope whose `data` is an array, which only terminates under `--no-follow` and so would have made the streaming case a different command in practice.
8. ~~**`shep-client` gains a `test-support` feature**~~ — **settled 2026-08-08 (the maintainer): yes, exactly as shep-daemon's `test-fakes` does.** A `publish = false` fakes crate was tried and reverted the same day: it keeps the scaffolding out of the published source, but only by making `shep-client-testing` depend on shep-client while shep-client dev-depends on it, and a dependency cycle — legal through dev-dependencies or not — is not a shape worth leaving in the tree for that. Consequence for later tasks: shep-client's own fake-driven test targets carry `required-features = ["test-support"]`, and shep-cli reaches the fakes through `shep-client = { workspace = true, features = ["test-support"] }` in its dev-dependencies (see File Structure).

9. ~~**`shep bleats --no-follow` cannot print anything.**~~ **SETTLED (the maintainer, 2026-08-08): `--no-follow` reads the log files; `--follow` keeps the bus.** The bus is live-only fan-out and `Request` has no history variant, so the drain arm Task 10 shipped exits on empty output by construction — which would have let Task 12's case 7 and its committed fixture both pass on nothing. The alternatives were bounded replay-on-subscribe, which redefines `Subscribe` for every future consumer including the lookout and the whistle, and dropping `--no-follow` from the phase. Reading the files needed one additive wire change to be honest about apps that configure an explicit `out_file` — `ProcessInfo`'s `out_file`/`err_file`, landed at `f62e504` — and gives more than replay would have, since a log file also holds what a sheep wrote before this CLI ever connected. Task 10a carries it.
