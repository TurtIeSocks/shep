# Changelog

All notable changes to `shep-client` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against.

## [Unreleased]

## [0.1.15] - 2026-08-29


## [0.1.14] - 2026-08-29


## [0.1.13] - 2026-08-29


## [0.1.12] - 2026-08-29


## [0.1.11] - 2026-08-29


## [0.1.10] - 2026-08-28

### Added

- `testing::fake_client_answering`, a fake daemon whose reply to each request
  comes from a caller-supplied closure while every envelope is still forwarded
  to the caller. The existing fakes each did one half: one shows the wire but
  answers everything `Pong`, so a caller that reads its own reply and decides
  what to send next stalls at the first request; the other answers a
  `ListFlock` properly but keeps its envelopes to itself. A verb that lists,
  decides from the listing, and then sends needs both at once. Available under
  the `test-support` feature, like the rest of the module.

## [0.1.9] - 2026-08-28


## [0.1.8] - 2026-08-28


## [0.1.0] - 2026-08-26

### Additions

This crate's whole public surface shipped without a CHANGELOG entry at the
time; this is that entry, written retrospectively once the surface was
proven end-to-end (shep-cli's `tests/cli_e2e.rs`) rather than piecemeal per
change.
Everything below is a stability surface as of this release — a breaking
change to any of it is a `[Unreleased]` entry of its own, not a silent diff.

- Add `Client`: `connect`/`connect_with_timeout` (a full handshake, never a
  bare `connect(2)` — a bound-but-not-accepting socket still completes that,
  so only a completed handshake counts as "a daemon answered"),
  `request`/`request_with_deadline` (typed `Request` → `Response`,
  `RequestError` on failure), `subscribe` (topic globs → `EventStream`),
  `daemon` (the `HelloAck` the handshake already produced — reading it again
  is a wasted round trip), `socket` (the path this client is connected to),
  and `close`.
- Add `ConnectError`, `RequestError` and (in the `spawn` module) `SpawnError`
  — one error enum per module per IR-18, each `#[non_exhaustive]` (IR-20):
  every one of them is a library-crate public error type downstream code
  matches on, and each is expected to grow as this crate's own coverage
  does.
- Add `EventStream` (a named `Stream` type, IR-15) and `Lagged` — a
  subscription's own item type, distinguishing "this client's receiver fell
  behind reading its socket" from `BusEvent::Dropped` (the daemon's own
  outbound queue overflowing), which is a different fault on the other side
  of the connection. `EventStream::next` is an inherent method, so pulling
  one event needs no `futures-util` dependency of the caller's own; the
  `Stream` trait itself is also re-exported from the crate root
  (`#[doc(inline)]`, IR-32) for callers that need it nameable in a bound.
- Add the `spawn` module: `connect_or_spawn`/`connect_or_spawn_with` (the
  autostart state machine — probe, launch only on "nothing listening", retry
  with backoff against a total deadline), `SpawnOutcome`, `SpawnOptions`, and
  `SpawnError`. Kept as a qualified module rather than flattened into the
  crate root on purpose: `spawn::DAEMON_ALREADY_RUNNING` reads as a
  deliberate cross-crate agreement at every call site, not an ordinary
  import.
- `DAEMON_ALREADY_RUNNING = 10` is a cross-crate contract with `shep-cli`:
  the daemon subprocess a losing `connect_or_spawn` racer launches exits
  with exactly this status when another process's `flock(2)` won the
  cold-start race, which is how the racer tells its own parent "keep
  probing, this was not a real failure" across a process boundary that
  carries no other channel. `shep-cli`'s `ExitCode::DaemonAlreadyRunning`
  hard-codes the same number (`exit.rs`'s own test pins them equal); this is
  what lets two genuinely concurrent `shep start` invocations against a cold
  `$SHEP_HOME` both exit 0 (proven against two real, concurrent processes by
  `shep-cli`'s `tests/cli_e2e.rs`).
- Add the timing constants every retry/deadline in this crate reads from,
  each named rather than an inline magic number (IR-26): `DEFAULT_DEADLINE`,
  `START_DEADLINE` (longer — a cold spawn plus a readiness probe routinely
  outruns the default), `LOG_PLANE_DEADLINE` (longer for its own reason — the
  daemon walks the matched flock file by file for `Reopen` and `Flush` alike,
  one sheep at a time with no bound of its own on a wedged or NFS-backed log
  directory), `TRIGGER_DEADLINE` (60s — an app's own `action_timeout` can be
  configured up to 58s, so anything shorter would abandon a `Request::Trigger`
  reply the daemon was still honestly building; the daemon clamps any
  deadline a caller asks for to the same 60s regardless, so asking for more
  buys nothing), `DEADLINE_GRACE`, `HANDSHAKE_TIMEOUT`, `SPAWN_DEADLINE`,
  `BACKOFF_START`, `BACKOFF_CAP`. `LOG_PLANE_DEADLINE` was briefly named
  `REOPEN_DEADLINE`, before `Flush` gave the same 30 seconds a second reader
  and the reopen-specific name stopped being true; both verbs read the one
  constant, so the budget cannot drift between them.
- Add the `test-support` feature: `pub mod testing`, the one home for every
  hand-rolled fake this crate and `shep-cli` share (`FakeDaemon` and its
  scripting methods, `fake_client_*` constructors), the same
  `publish = false`-avoiding shape `shep-daemon`'s own `test-fakes` uses. A
  separate fakes crate was tried and reverted the same day it was proposed:
  it would have needed a dev-dependency cycle (a fakes crate depending on
  `shep-client` while `shep-client` dev-depends on it back) to keep the
  scaffolding out of the published source, which is not a shape worth
  leaving in the tree to avoid one Cargo feature.
- Re-export `shep_core` at the crate root, so downstream users need a single
  dependency rather than naming both crates themselves.
- Add `testing::fake_daemon_accepting_repeatedly` — a fake that binds and
  answers every connection with one reply each, until its handle is
  aborted, for `shep whistle`: the first caller in this workspace that opens
  a fresh connection per request rather than reusing one. Every other fake
  in this module accepts exactly one connection, which is right for a
  caller handed an already-connected `Client` but not for this one. Returns
  an `Arc<AtomicU32>` request counter alongside the `JoinHandle`, since the
  accept loop never ends on its own and a test needs to read the count
  while the fake is still running.

### Fixes

- The handshake-close test asserts the unreachable outcome rather than one
  `ConnectError` variant, so it passes on Linux, where the peer's close
  surfaces on the write rather than the read.
