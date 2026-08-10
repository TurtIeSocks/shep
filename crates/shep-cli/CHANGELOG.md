# Changelog

All notable changes to `shep-cli` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against.

## [Unreleased]

### Additions

- Add the clap command tree (`Cli`, `Commands`, and every argument struct
  the CLI will ever parse — `Start`, `Stop`/`Restart`/`Delete`/`Describe`,
  `Flock` (aliases `list`/`ls`), `Fold`, `Bleats` (alias `logs`), `Reopen`,
  `Ping`, `Kill`, `Completions`, the hidden `Thatlldo` and `Daemon`), pure
  tier so it compiles and its tests run on Windows.
- Add the process exit-code taxonomy (`ExitCode`, matching spec §9's table
  exactly, values included) with its stable `code_str` spelling and a
  `From<RpcErrorCode>` conversion; the three `From<&shep_client::*Error>`
  conversions are unix-only, since the error types they read from are.
- Add `main`'s dispatch skeleton: argument parsing, `$SHEP_HOME` resolution
  from `--home`/`$SHEP_HOME`/`$HOME`, and a placeholder arm for every verb —
  each replaced by its own command module as that verb is implemented.
- Exit code 2 (`Usage`) is clap's own convention for bad arguments and
  collides with the fail-fast code spec §9 reserves for the `runtime`
  subcommand's own use. `runtime` is out of scope for this phase; whichever
  task builds it resolves the collision deliberately, rather than
  discovering it.
- Carry `ProcessInfo`'s new `out_file`/`err_file` in every `--json` payload
  built from `FlockRows` (`flock`, `describe`, `fold`, `start`, `stop`,
  `restart`). They are `JSON_ONLY`, not columns: absolute log paths are
  routinely longer than the rest of the row put together and would wreck the
  table those verbs exist to print.
- Add the end-to-end test tier (`tests/cli_e2e.rs`): the real `shep` binary
  against a real daemon, a real socket, and real spawned sheep, each on a
  fresh `$SHEP_HOME`. Covers autostart from cold, daemon reuse across
  commands, the concurrent cold-start race, exit codes and stdout/stderr
  stream discipline under `--format json`, `kill`'s socket teardown,
  `bleats --no-follow` against real log files (both default and `--out`),
  and that an autostarted daemon binds under the `--home` it was given
  rather than an ambient `$SHEP_HOME`. Unix-only (`#![cfg(unix)]`): an
  integration test file is its own compilation unit, so without the gate
  `--all-targets` would build it — with its unix-only `nix` dev-dependency —
  on the Windows CI leg too.
- Commit `--format json` fixtures for `flock`, `describe`, `start`, `ping`
  and `bleats --no-follow` under `tests/fixtures/*.json` (IR-35's byte-fixture
  discipline, same as the wire protocol). The four envelopes are compared
  structurally, with the fields a real spawned process cannot pin across
  runs (`pid`, `uptime_ms`, `out_file`, `err_file`) asserted against their
  own real shape and then normalized before the comparison; `bleats
  --no-follow`'s one JSON-line-per-record output carries no envelope (see
  its own entry below) and is compared byte-for-byte.
- `DaemonAlreadyRunning = 10` is a cross-crate contract, not an internal
  implementation detail: `shep-client`'s `spawn::DAEMON_ALREADY_RUNNING`
  hard-codes the same number so `connect_or_spawn` can tell "a losing
  cold-start racer's daemon exited on purpose" apart from every other exit,
  which is what lets both sides of a concurrent `shep start` race exit 0
  (Task 12's end-to-end tier proves this against two real, genuinely
  concurrent invocations). Changing either side without the other
  reintroduces the race — `exit.rs`'s own test pins the two constants equal.
- Render the daemon's own diagnostics. The hidden `daemon` subcommand now
  installs a `tracing-subscriber` on **stderr**, which `launch.rs` already
  redirects into `$SHEP_HOME/logs/shepd.err.log` — so a hand-run daemon logs
  to the terminal it was run from, and a launched one logs to that file,
  without either path naming a file here. `[daemon] log_level`
  (`SHEP_LOG_LEVEL`) picks the level, default `warn`; the long-parsed
  `[daemon] log_json` (`SHEP_LOG_JSON`) finally does something and switches
  the renderer to JSON lines. Colour is on only when stderr is a terminal and
  `NO_COLOR` is unset or empty — that one is a cross-ecosystem convention
  about the terminal rather than a shep knob, which is why it is honoured
  where `RUST_LOG` is deliberately ignored.
  Fifty-one log sites in `shep-daemon` reached nobody before this: a watch
  that could not be armed, a cron pattern that would not parse, and the
  observed RSS and ceiling behind a memory restart — the last of which no
  bus event carries at all.
- Add `shep reopen [selector]`, which tells the daemon to reopen the log
  files of the sheep the selector matches — the half of `create`-mode
  rotation that runs after the rotator's rename. A zero exit means every
  matched sheep's log pump holds a handle on the recreated path, so a
  logrotate `postrotate` stanza can wait for it. The selector is
  optional and defaults to `all`, matching `bleats` rather than
  `stop`/`restart`/`delete`: those destroy something and this destroys
  nothing, and rotating the whole flock at once is the ordinary case. A
  matched sheep that is not running has nothing to reopen and is listed in
  the output rather than failing the command. A pump that could not open a
  path again does fail it, naming the sheep and the path: the rename is
  still safe to act on, but that sheep is writing a stream nowhere, and
  exiting 0 there would be the silent failure this verb exists to end.
  Output is the same table of matched sheep `stop` and `restart` print.

### Fixes

- Stop holding `std::io::stderr().lock()` for the daemon's entire lifetime.
  `run` took the process-wide stdout and stderr guards before dispatching,
  which is right for verbs that last milliseconds and wrong for the one that
  runs until a signal: `Stderr`'s lock is re-entrant only for the thread
  holding it, so the first record any tokio worker wrote blocked forever and
  took the supervisor down with it — silently, leaving an empty
  `shepd.err.log` and a daemon that still accepted connections but answered
  no handshake. The `daemon` arm now holds no handle at all — its two error
  envelopes take the lock for the length of one write each, which is also what
  stops a record from a live worker tearing a `--format json` envelope in half
  — and `bleats`, which follows until Ctrl-C and had the identical shape, now
  uses unlocked handles that take the lock per write. The guard had been held
  harmlessly since this crate's first day, because nothing wrote to stderr off
  the main thread until the daemon grew a subscriber for its own records.
- Give the workspace's path dependencies a version alongside their `path`,
  which `cargo publish` requires. The package here is `shep-cli`, but the
  `[[bin]]` it produces is named `shep`, so once published the install
  command is `cargo install shep-cli` — `cargo install shep` looks up an
  unrelated crate.
