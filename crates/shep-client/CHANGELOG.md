# Changelog

All notable changes to `shep-client` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against.

## [Unreleased]

### Additions

Phase 3 built this crate's whole public surface without a CHANGELOG entry;
this is that entry, written retrospectively once the surface was proven
end-to-end (shep-cli's `tests/cli_e2e.rs`) rather than piecemeal per task.
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
  outruns the default), `REOPEN_DEADLINE` (longer for its own reason — the
  daemon visits matched sheep one at a time and each reopen is two `open(2)`s
  behind a `flush`, with no bound of its own on a wedged or NFS-backed log
  directory), `DEADLINE_GRACE`, `HANDSHAKE_TIMEOUT`, `SPAWN_DEADLINE`,
  `BACKOFF_START`, `BACKOFF_CAP`.
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
