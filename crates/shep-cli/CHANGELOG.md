# Changelog

All notable changes to `shep-cli` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against.

## [Unreleased]

### Additions

- Add the clap command tree (`Cli`, `Commands`, and every argument struct
  the CLI will ever parse — `Start`, `Stop`/`Restart`/`Reload`/`Delete`/
  `Describe`, `Trigger`, `Flock` (aliases `list`/`ls`), `Fold`, `Bleats`
  (alias `logs`), `Reopen`, `Flush`, `Ping`, `Kill`, `Completions`, the
  hidden `Thatlldo` and `Daemon`), pure tier so it compiles and its tests
  run on Windows.
- Add `shep reload <selector>`: replace each instance of the matched sheep
  with a fresh one, one instance at a time, so the app gets a window in which
  it can hand over. **Not zero downtime** — the old listener's queue of
  connections it has not accepted yet is dropped when it closes, so an app
  that does not stop accepting and finish what it has in hand before
  `graceful_timeout` runs out loses whatever was waiting there. The verb's
  own `--help` says so.

  **A port-binding app has to set `SO_REUSEPORT` itself before it binds**, or
  every reload of it fails. shep binds nothing and so cannot set the option
  on the app's behalf; the `reuse_port` app option is the operator asserting
  that the app does, and a mismatch is `EADDRINUSE` at the replacement spawn,
  undetectable in advance. What the operator sees without it is nothing at
  all: `shep reload` has already exited 0 by the time the replacement fails,
  so the abandonment shows up as `process.reload_abandoned` on the bus and in
  the shepherd's log, and the old instance goes on serving. `--help` names
  the precondition for the same reason.

  The selector is **required**, exactly as it is for `stop`/`restart`/
  `delete` and for the same reason: the verb replaces running processes, so
  the operator names the target. That requirement is now pinned by a test
  covering every verb sharing `SelectorArgs` — a `default_value` on that one
  field would have turned a bare `shep stop` into `shep stop all` for six
  verbs at once, and nothing caught it before.

  **The command exits as soon as the shepherd accepts the reload**, printing
  the flock as it stood at that moment rather than after the swaps. A
  clustered app takes longer to swap than any reply can wait for, so the
  alternative was not a slower `shep reload` but one that reported a timeout
  for a reload still running. Progress is on the bus, under `process.reload`,
  `process.reloaded` and `process.reload_abandoned`.
- Add the process exit-code taxonomy (`ExitCode`, matching spec §9's table
  exactly, values included) with its stable `code_str` spelling and a
  `From<RpcErrorCode>` conversion; the three `From<&shep_client::*Error>`
  conversions are unix-only, since the error types they read from are.
- Add `main`'s dispatch skeleton: argument parsing, `$SHEP_HOME` resolution
  from `--home`/`$SHEP_HOME`/`$HOME`, and a placeholder arm for every verb —
  each replaced by its own command module as that verb is implemented.
- Exit code 2 (`Usage`) is clap's own convention for bad arguments and
  collides with the fail-fast code spec §9 reserves for the `runtime`
  subcommand's own use. `runtime` does not exist yet; whichever change
  builds it resolves the collision deliberately, rather than discovering it.
- Carry `ProcessInfo`'s new `out_file`/`err_file` in every `--json` payload
  built from `FlockRows` (`flock`, `describe`, `fold`, `start`, `stop`,
  `restart`, `reload`, `reopen`). They are `JSON_ONLY` on those verbs, not
  columns:
  absolute log paths are routinely longer than the rest of the row put
  together and would wreck the table they exist to print. `flush` is the one
  exception and renders them — see its own entry below.
- Add the end-to-end test tier (`tests/cli_e2e.rs`): the real `shep` binary
  against a real daemon, a real socket, and real spawned sheep, each on a
  fresh `$SHEP_HOME`. Five groups of cases. **Daemon lifecycle**:
  autostart from cold, daemon reuse across commands, the concurrent
  cold-start race, `kill`'s socket teardown, and that an autostarted daemon
  binds under the `--home` it was given rather than an ambient `$SHEP_HOME`.
  **Output contract**: exit codes and stdout/stderr stream discipline under
  `--format json`, and the committed fixtures below. **The log plane**:
  `bleats --no-follow` against real log files (both default and `--out`),
  `reopen` after an external rename, an external `copytruncate` with no shep
  verb involved at all, `flush` and its refusal to run without a selector, and
  the two `[daemon]` log knobs deciding the renderer and the level of the
  daemon's own records. **Restart triggers on their real clocks**: a write
  under a watched tree and a dot-file write that must trigger nothing, a cron
  occurrence, and a memory ceiling a process tree really crosses — the last
  two on wall time, not a paused one, which is what makes them the slowest
  cases in the workspace and the reason they are not `#[ignore]`d (an ignored
  test closes no gap). **Config-time refusals and the readiness gate**: a bad
  cron pattern and an `https://` probe target, each failing at parse rather
  than three seconds into a sheep's life, and a `wait_ready` sheep that holds
  at `starting` until it signals. Unix-only (`#![cfg(unix)]`): an
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
  (`cli_e2e`'s `concurrent_cold_starts_produce_exactly_one_daemon` proves
  this against two real, genuinely concurrent invocations). Changing either
  side without the other
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
  Every `tracing` record in `shep-daemon` reached nobody before this: a watch
  that could not be armed, a cron pattern that would not parse, and the
  observed RSS and ceiling behind a memory restart — the last of which no
  bus event carries at all. `shep-daemon`'s own changelog carries the count;
  repeating it here is what let it go stale.
- Add `shep reopen [selector]`, which tells the daemon to reopen the log
  files of the sheep the selector matches — the half of `create`-mode
  rotation that runs after the rotator's rename. A zero exit means every
  matched sheep's log pump holds a handle on the recreated path, so a
  logrotate `postrotate` stanza can wait for it. A rotator that moved the log
  DIRECTORY aside rather than the files is covered too: the pump puts it back
  at `0700`, the mode every directory shep creates gets. The selector is
  optional and defaults to `all`, matching `bleats` rather than
  `stop`/`restart`/`delete`: those destroy something and this destroys
  nothing, and rotating the whole flock at once is the ordinary case. A
  matched sheep that is not running has nothing to reopen and is listed in
  the output rather than failing the command. A pump that could not open a
  path again does fail it, naming the sheep and the path: the rename is
  still safe to act on, but that sheep is writing a stream nowhere, and
  exiting 0 there would be the silent failure this verb exists to end. **That
  failure can name a sheep the selector did not** — the daemon asks every
  writer to a path it is rotating, which during a reload is both halves of a
  swap, while the table stays keyed by the selector. The
  request carries `LOG_PLANE_DEADLINE` rather than the client's 5s default,
  since the daemon visits matched sheep serially with no per-sheep bound —
  the default would report failure to a `postrotate` stanza whose reopen was
  still running. Output is the same table of matched sheep `stop` and
  `restart` print. A rotator that would rather signal a pid than run a client
  can send the daemon `SIGUSR2` instead, which does the same work at the
  `all` selector — see `shep-daemon`'s entry for what that form gives up: no
  reply to wait on, and no narrower selector.
- Add `shep flush <selector>`, which empties the log files of the sheep the
  selector matches: the daemon flushes what every pump writing to one of
  those files still owes it, then truncates the paths those sheep were
  registered with. **The selector is required**, where `bleats` and `reopen`
  both default to `all` — this is the one command in the CLI whose slip of
  the finger cannot be undone, so it follows `stop`/`restart`/`delete` and
  makes the operator name a target. `shep flush all` is still short to type
  when it is meant. What it empties is exactly the paths the Flockfile
  named: `out_file`/`err_file` are taken verbatim and never checked against
  the log directory, so an app pointing one of them at a file that is not a
  log has that file emptied too, with the shepherd's privileges. A matched
  sheep that is not running is emptied like any other, since the operation
  addresses paths rather than open handles and a stopped sheep's logs are
  still readable with `shep bleats --no-follow`. The sheep goes on
  logging into the same file afterwards, at offset 0 — its handle is
  `O_APPEND` and the daemon never touches it. A file that could not be
  emptied fails the command and is named on stderr; exiting 0 there would
  leave an operator believing a log is empty when it holds everything it did
  before. No selector reaches the shepherd's own
  `shepd.out.log`/`shepd.err.log`: the CLI's launcher creates those before the
  daemon exists and the daemon inherits them as plain fds 1 and 2, so it holds
  no handle to flush and no path to truncate — they are `--daemon`'s, below.
  Output is one row per matched SHEEP, not per file emptied, carrying that
  sheep's two log paths: `ID`, `NAME`, `OUT_FILE`, `ERR_FILE`. `flush` is the
  only flock-shaped verb that renders the paths rather than keeping them to
  `--format json`, and it is the only one whose subject is the files — a verb
  that empties something an operator may have mistyped and then reports
  `STATUS`/`PID`/`UPTIME` has said nothing about what it destroyed. The
  lifecycle fields stay in the JSON, which is byte-identical to what the other
  verbs answer with, so nothing consuming `--format json` has to special-case
  this command. A sheep sharing a log path with a matched one has that file
  emptied under it as well, its pump flushed first like any other writer to
  that path, and no row of its own: the selector names sheep, and so does the
  table.
- Add `shep flush --daemon`, the only way to empty the shepherd's own
  `shepd.out.log`/`shepd.err.log`. It **replaces** the selector rather than
  composing with it — `shep flush all --daemon` is a usage error — because the
  two halves answer with different shapes, because one invocation renders one
  payload, and because the shepherd's logs are meant to be reached only by
  being named rather than by riding along with a flock-wide flush. A flag and
  not a reserved `shep` selector: nothing stops an app being named `shep`, and
  a selector that meant something different depending on the Flockfile would
  be a trap. The CLI empties these two itself and asks the daemon nothing —
  they are the CLI's files, and it needs no socket, so this is the one flush
  that works while the shepherd is down, which is when an operator most often
  wants it. No flush barrier is needed or possible: the daemon's records go
  through its subscriber straight to fd 2, synchronously, with nothing queued
  to outrun a truncate. Output is a table of the files themselves — stream,
  path, and whether each was `emptied` or `absent` — because for this half the
  paths ARE the answer. A file that is not there is already empty and is
  reported rather than created, so `shep flush --daemon` on a cold
  `$SHEP_HOME` exits 0.
- Add `shep trigger <selector> <action> [params]`, which sends a named,
  free-form action to the sheep the selector matches over its shepherd
  channel and reports what each one answered. Delivery needs `channel = true`
  in the app's own Flockfile — or `wait_ready`/`shutdown_with_message`,
  either of which opens the same channel on its own — and nothing user-facing
  said so before this: an operator without it got a `no_channel` row and no
  way to know why. Both `--help` and the row itself now name the field. The
  selector is **required**, matching `stop`/`restart`/`reload`/`delete`/
  `describe`: this reaches a running app, so the operator names the target.

  A row's own outcome is never a request failure — `replied`, `no_channel`,
  `skipped` (a reload drainee, mid-swap) and `timed_out` (no reply inside the
  app's own `action_timeout`) all render as rows of one successful reply, the
  same precedent `reopen`/`flush` set for a per-sheep refusal inside a
  request that otherwise succeeded. Only a selector matching nothing, or the
  daemon itself being unreachable, fails the command as a whole.

  The table renders `ID`/`NAME`/`OUTCOME`/`DETAIL`; a `Replied` body is
  arbitrary, app-chosen text of unknown length, so the table cannot show it
  verbatim the way `--format json` does — a long body would stretch every row
  in the column to match it, and an embedded newline would split one row
  across output lines and desync every column beneath it. `DETAIL` therefore
  escapes embedded newlines to `\n`/`\r` and caps the preview at 80
  characters with a trailing `...`; `--format json` always carries the real
  reply, full length, real newlines included. Sent with a 60s deadline
  (`TRIGGER_DEADLINE`, `shep-client`) rather than the client's 5s default,
  since an app's own `action_timeout` can be configured up to 58s and the
  default would abandon a reply the daemon was still honestly building.

  `parse_selector` — duplicated once per verb module (`lifecycle`, `logs`,
  `query`, `bleats`) and about to become a fifth copy for this verb — is now
  one function in `commands::selector`, landed as its own commit ahead of
  this one so the new verb builds on a single copy instead of adding to the
  pile.

- Add `shep save`, which asks the daemon to write the muster roll now,
  bypassing the snapshot writer's debounce (`Request::SaveRoll` /
  `Response::RollSaved`). `save` is pm2's own word, so the muscle memory
  transfers directly. It takes **no selector**: the roll always records the
  whole flock, so it is not one of the six verbs `SelectorArgs` gates.

  The reply names the path the daemon wrote and how many apps that roll
  records, and both ride the table — `FILE`/`APPS`, every field a column,
  matching `EmptiedFiles`' own reason: a verb that wrote a file and would not
  say which one has reported nothing. A failed save exits non-zero and names
  why, rather than the silent no-op the verb exists to rule out.

  Dispatched through `connect_client`, never `connect_or_spawn_client`:
  saving the roll of a daemon that is not running is not a thing, and
  autostarting one just to save an empty flock would overwrite a good roll
  with an empty one.

- Add `shep muster` (hidden alias `resurrect`, pm2's own word), which asks
  the daemon to assemble the flock from the roll `save` wrote
  (`Request::Muster` / `Response::Mustered`), rendered the same way `flock`
  is. Sent with `START_DEADLINE` rather than the client's 5s default, same
  reasoning as `start`: a muster spawns every app in the roll, and a cold
  restore of a real flock routinely outruns five seconds. An empty
  `Mustered` — the roll restored nothing — gets an explicit notice on
  stderr, so that answer is never a silent exit 0.

  This is the binary's **second** autostart path, after `start`: dispatched
  through `connect_or_spawn_client` rather than `connect_client`, because
  bringing a fresh daemon up is the whole point of the verb on a machine
  that just rebooted. When that autostart itself just spawned the daemon,
  boot has already restored the roll before this request goes out, so the
  `Muster` that follows spawns nothing new and simply reports the flock
  restore produced — `Response::Mustered` always names every sheep of every
  app the roll restored, not only what this particular call spawned, which
  is what makes the verb idempotent for an init system that runs it more
  than once.

- Add `shep import`, which reads a pm2 dump (`--from`, default
  `~/.pm2/dump.pm2`) and writes it out as a Flockfile (`--out`, default
  `./Flockfile.toml`) — the last piece of the pm2 cutover path.
  **Starts nothing**: no client, no daemon round trip, just a file
  read and a file write. `--dry-run` prints the rendered Flockfile to
  stdout instead of writing it, with no envelope, so
  `shep import --dry-run > Flockfile.toml` produces a byte-exact file;
  without it, an existing output path is left alone unless `--force`.

  A pm2 dump is per-instance — one row per running process — so the
  conversion collapses same-named rows back into one app each, taking the
  first row's scalars (script, cwd, interpreter, ...) and the row count as
  `instances`. **Every clustered app is named on stderr**: shep binds
  nothing, so N instances on one port is `EADDRINUSE` at start unless the
  app itself sets `SO_REUSEPORT` (Node's `reusePort: true`, needing Node
  >= 22.12) — the warning exists so that is discovered at import time, not
  at the first restart. **Every ambiguous env key is named on stderr and
  left out of the Flockfile**: a key that is neither declared in an
  ecosystem file's `env_<name>` block nor recognizable login-shell or pm2
  session junk is the operator's to decide, never guessed at — an
  inherited `BUN_INSTALL` or `DATABASE_URL` is exactly the kind of thing a
  heuristic would eventually get wrong, silently. `NODE_APP_INSTANCE`
  becomes `increment_var` rather than a copied value, since copying it
  would pin instance 0's number into every instance.

  The renderer serializes a purpose-built projection of `AppConfig`, not
  the type itself — `AppConfig` is `#[serde(default)]` across roughly forty
  fields and would bury the handful that matter under the rest, each
  written out at its own spec default. Every field this importer can
  produce is skipped when it already matches that default, and
  `max_memory`/`restart_delay` render in their string forms (`"512M"`,
  `"5s"`), never as raw integers a Flockfile parser would reject.

- Add `shep daemon --foreground`, for an init system that runs the shepherd
  itself rather than letting the CLI autostart one. It reports readiness on
  `$NOTIFY_SOCKET` once the muster restore has finished, which is what lets a
  `Type=notify` unit go green when the flock is actually back instead of when
  the process execs.

  It is a second arrangement, not a second code path. `shep daemon` already
  runs the supervisor in this process; the flag adds the readiness report and
  nothing else — no fork, no re-exec, not one step of the boot changed.
  Everything that makes an autostarted daemon survivable on its own — the new
  process group, the detached terminal, stderr redirected into
  `shepd.err.log` — lives in `launch.rs`, on the *parent's* side of a re-exec
  this arrangement never performs, and systemd does those jobs itself.

  The flag is also the only thing that turns the report on, so a `shep` the
  CLI autostarts from inside some other notify-type service inherits that
  service's `$NOTIFY_SOCKET` and stays silent on it. `launch_daemon` passes
  exactly one argument, `daemon`, and its own test pins that argument vector.

- Add `shep startup` and `shep unstartup`, which install and remove the init
  unit that brings the shepherd — and the flock it last saved — back after a
  reboot. On Linux that is a systemd unit at
  `/etc/systemd/system/shep-<user>.service`, `Type=notify` so the unit goes
  green once the restore has finished rather than when the process execs; on
  macOS a `LaunchDaemon` plist at
  `/Library/LaunchDaemons/io.github.turtiesocks.shep.<user>.plist`. Both
  carry this binary's resolved path, the target user's `$SHEP_HOME`, and the
  `PATH` of the invocation that wrote them — the last of those is what makes
  an interpreter installed under `~/.bun` or `~/.cargo` findable on a machine
  that has only just booted.

  **shep never escalates.** No `sudo`, no setuid, no privilege helper
  anywhere on this path. Running as root it writes the unit and enables it;
  running as anyone else it prints the exact command to run — fully resolved,
  and quoted, so a `$SHEP_HOME` with a space in it survives the paste — and
  exits non-zero, so a script notices instead of believing a unit was
  installed. `unstartup` disables and removes under the same rule, and prints
  its own command without a `--home`, since a removal is addressed by the
  unit's path and label alone.

  **Under `sudo` the unit is built for `$SUDO_USER`, never for root.** The
  invoking user IS root there, so a unit resolved from it would supervise
  root's flock while the operator's stayed down, and would look correct doing
  it. The `$SHEP_HOME` follows the same rule and comes from the target user's
  passwd entry rather than `$HOME`, which `sudo` has already reset to root's:
  a unit carrying `/root/.shep` boots cleanly and restores nothing, and says
  so months later or not at all. A `$SHEP_HOME` that does not exist is
  refused rather than written into a unit, because that is what the same trap
  produces when nobody catches it.

  One caveat the verb cannot detect: `sudo` on most distributions replaces
  `PATH` with its own `secure_path` before the command runs, so a unit
  written by `sudo shep startup` carries that rather than the operator's
  login `PATH`. `systemctl cat shep-<user>` shows what was actually written.

  **An existing unit is never overwritten.** `shep startup` refuses and names
  `shep unstartup`. Rewriting the file changes nothing about the service
  already loaded on either init system, so an overwrite would leave the file
  and the running unit disagreeing — and an operator who edited theirs in
  place should be told, not have the edits replaced. `unstartup` then
  `startup` closes both halves; a `--force` flag would close neither.

  Output is one row per step, in the order the steps were taken: the file
  written or removed, and each `systemctl`/`launchctl` invocation, with what
  it answered. A step that fails does not stop the ones after it — a
  half-installed unit is worse than a fully-attempted one, and the operator
  needs every row to know which half they are holding — and the command exits
  non-zero once they have all run. `shep unstartup` on a machine that never
  ran `startup` reports the unit `absent` and exits 0, matching
  `shep flush --daemon` on a log file that is not there.

  openrc and the BSD rc.d scripts get no renderer: spec §11 names four init
  systems and this pair covers two, chosen by compile target rather than by
  probing which init system is actually running. A target that is neither
  Linux nor macOS is refused before any file is written, with a
  platform-level message; a Linux host running openrc still gets a systemd
  unit, and the mismatch surfaces later, at the `systemctl` step.

- `flock` and `describe` show each sheep's live CPU and memory. `CPU` and
  `MEM` land between `RESTARTS` and `UPTIME`, where `pm2 ls` puts them and
  where an operator scanning the table looks; `-` for a sheep with no
  reading, the same rule `PID` and `FOLD` already follow and for the same
  reason — an empty cell in a padded table reads as a bug, and `0.0%` would
  claim something the daemon never measured.

  `MEM` goes through a new `human_bytes`, not `MemSize`'s own `Display`:
  that impl only names a unit that divides the value exactly, so a live
  resident set of 50 462 720 bytes would print as the unreadable "50462720"
  rather than "48.1M". `CPU` gets the same one decimal place — six would be
  noise on a number this volatile.

  Both fields already rode along in the JSON; this only gives them a
  column. `shep flush`'s own table is untouched — its CHANGELOG entry
  already covers why lifecycle and resource fields stay JSON-only there.

### Fixes

- Open the shepherd's own `shepd.out.log`/`shepd.err.log` `O_APPEND` in the
  launcher. The daemon inherits both as fds 1 and 2 and never opens them
  itself, so `File::create`'s plain `O_WRONLY|O_CREAT|O_TRUNC` left both
  descriptors tracking their own offset for the daemon's whole life — and a
  descriptor tracking its own offset writes PAST an external truncation rather
  than at offset 0 of the emptied file. Measured: ten bytes, an external
  truncate, three more bytes, and the file is thirteen bytes of which the
  first ten are `NUL`; under `O_APPEND` the same sequence leaves three. This
  is the sparse hole `shep-daemon`'s `open_append` argues about for a sheep's
  logs, in the one place shep opens a log file that is not a sheep's, and
  `shep flush --daemon` is the truncation that would have walked into it. The
  launch-time emptying is kept — `std` refuses `append` together with
  `truncate`, so it is a `set_len(0)` on the appending handle — and reusing one
  `$SHEP_HOME` across relaunches still starts both files empty. A daemon
  launched by an older `shep`, or run in the foreground behind the operator's
  own shell redirection, keeps whatever descriptor it was given.
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
- Warn, once at daemon boot, when `shep.toml` sets `[daemon] enabled_dogs`
  or any `[dog.<name>]` section: both parse, validate and round-trip, but
  there is no dogs infrastructure in this build to read either one, so an
  operator who wrote `enabled_dogs = ["metrics"]` got a daemon that boots
  and silently does nothing with it. The daemon still boots — a hard error
  would be disproportionate to a field that does nothing, and would break a
  config that works today the moment dogs land and start reading the same
  field for real.
