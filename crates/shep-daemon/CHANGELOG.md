# Changelog

All notable changes to `shep-daemon` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against.

## [Unreleased]

### Additions

- Add the cron-restart worker: one worker per name-group, restarting every
  instance of the name — stopped instances included — on its `cron_restart`
  schedule, the same reach the watch below has. The dialect is five-field
  standard cron in the app's `cron_timezone`, and the next
  occurrence is re-derived from the wall clock on every iteration rather than
  tracked across one long sleep, so a suspend or an NTP step costs at most one
  `max_cron_sleep` of drift. **A missed occurrence is not replayed**: a
  machine that slept through six hourly occurrences restarts once, at the
  next one, instead of firing six times in a burst on wake.
- Add the memory-limit enforcer: an app's `max_memory` ceiling is polled every
  15 seconds against the sheep's whole **process tree** — its own pid plus
  every lamb — and a breach restarts it. This deviates from pm2, which
  measures the root pid alone; an app that forks workers may therefore see
  restarts pm2 never gave it. A breach restart **resets the restart budget**,
  exactly as `shep restart` does — it does not merely skip its own increment,
  it also forgives every unstable exit counted before it — so a leaking app
  restarts indefinitely rather than reaching `errored`.
- Add liveness probes: `liveness_probe` polls a sheep over HTTP, TCP or a
  command on its `interval`, and restarts it once `failure_threshold`
  *consecutive* probes have failed. The HTTP client is hand-rolled and carries
  no TLS stack and no redirect following — a `3xx` is a failed probe, and an
  `https://` target is refused at config time by `shep-core` rather than
  failing every poll, since a probe that always fails is indistinguishable
  from an app that is down.
- Add the filesystem watch: an app with `watch = true` gets one watcher over
  its `cwd`, debounced by `watch_delay` (default 500ms), and a change under
  that tree restarts the app. A delivered path triggers when it matches the
  app's `watch_options` (or `**` when it names none) and does not match the
  ignores, so ignore always wins; dot-entries, `node_modules`, and shep's own
  `logs/` and `pids/` are in the ignores by default, and an app's
  `ignore_watch` extends those defaults rather than replacing them.
  `watch = true` without a `cwd` is refused at config time rather than arming
  nothing quietly — see `shep-core`'s entry for why defaulting to the daemon's
  own cwd was the worse of the two remaining options.

  **One path escapes the globs entirely.** A change reported *at the watch
  root itself* triggers a restart before either set is consulted, so
  `ignore_watch` cannot suppress it. That is the rescan signal an inotify
  queue overflow produces — it means "unknown paths under here changed", not
  "this path changed", and no user pattern can be matched against it
  meaningfully. Restarting on it is the conservative reading; the alternative
  is a watch that goes quiet exactly when it knows least.

  Two halves of the reach are worth stating together, because either alone
  misleads. **A triggering change restarts every instance of the name**,
  stopped instances included. **Stopping a sheep disarms its watch.** For a
  single-instance app that means total protection: `shep stop web` and no
  later save brings `web` back. For one instance of a multi-instance app it
  does not: `shep stop web-1` with `web-2` still running leaves the group's
  one watcher armed, so the next save restarts the whole name and `web-1`
  comes back up. Stop the group, or delete the instance.
- Add the extras registry that arms all four of the above when a sheep goes
  live and disarms them across every terminal transition, including the
  `Drop` that aborts every armed task when the supervisor itself goes away —
  covering both a graceful shutdown that never kills a `WaitingRestart` sheep
  and a panicking actor.
- Add the process-lifecycle engine: a `ProcessRunner` spawn seam with a real
  `tokio::process`-backed implementation (own process group, fd-3 shepherd
  channel, log capture) and a deterministic scripted fake for tests.
- Add spawn assembly (env, interpreter resolution, log paths), the restart
  brain (exit-outcome decision tree) and pinned-integer exponential backoff,
  and the kill ladder (message, signal, timeout, then `SIGKILL` on the whole
  process group).
- Add the supervisor actor: registers and spawns per-sheep tasks, routes
  `Start`/`Stop`/`Restart`/`Delete`/`Shutdown`, and resolves each app's
  `user`/`group` config to numeric uid/gid once per spawn (unix; refused
  outright elsewhere). Verified under a paused clock with a proptest over
  random command/exit interleavings (never two live pids per unit, restart
  count monotonic, always reaches steady state).
- Add the unix-socket control plane: `RpcServer` (same-uid peer-credential
  auth, a versioned handshake that refuses protocol skew with a typed error,
  per-call deadlines clamped server-side) and the portable `rpc::dispatch`
  it calls into, which never touches a socket or a byte.
- Add the daemon-wide event bus: `Subscribe` with server-side topic-glob
  filtering, a bounded per-subscriber queue that drops the oldest event and
  reports `Dropped { count }` rather than blocking the bus.
- Add the muster roll: debounced atomic `flock.json` writes (owner-only,
  `0600` — the one place this daemon persists an app's `env` to disk) and
  restart-survival restore that validates each entry independently instead
  of aborting the whole muster on one bad one.
- Add the daemon boot sequence: `0700` runtime layout (created at that mode
  directly, never chmod'ed after), an atomically-written pidfile, control-
  socket bind with stale-socket recovery, a readiness-pipe handshake for the
  CLI's `daemon` subcommand, SIGTERM/SIGINT/SIGQUIT graceful shutdown and a
  SIGUSR2 log-reopen stub, and a load-bearing ordered teardown (roll saved
  before the flock is killed, or `shep muster` after a reboot restores
  nothing).
- The pure decision tiers (brain, backoff, assemble, entry, the `runner`
  trait and its fake) compile and test on every platform; the OS tier
  (real spawning, signals, the kill ladder, the socket itself) is unix-only.
- Report each sheep's resolved log paths on `ProcessInfo`. `ProcessEntry`
  now carries the `out_file`/`err_file` that `assemble` resolved for it,
  copied off the assembled `SpawnSpec` at registration rather than derived a
  second time, so the reported paths are by construction the ones the child
  is writing to — including when the app configured an explicit `out_file`
  pointing outside the log directory entirely.
- This crate's fifty-one `tracing` records now reach a reader. Nothing here
  changed and nothing here installs a subscriber — that belongs to the
  binary, once per process, and a library that installed one would fail
  every test after the first — but the `shep` binary now does, at `warn` by
  default, so every warn-and-continue arm in this crate is output rather
  than a comment claiming output. The arms worth knowing about: `extras`
  reports a watch, a cron worker or a liveness probe it could not arm and
  lets the sheep come up `online` regardless, and `supervisor`'s
  `Actor::handle_ready_result` reports a readiness deadline that elapsed,
  which is otherwise indistinguishable from a sheep that answered.
- Add `runner::LogCtl`, the request type a sheep's log pump takes mid-flight,
  and the first way anything has been able to reach the file handle that pump
  writes to. `Reopen` makes the pump flush, close and
  re-open both of a sheep's log files, then answer on a `oneshot`. That
  acknowledgement is the point of the shape rather than a nicety: a flag the
  pump would notice before its next write promises nothing about a sheep that
  has gone quiet, and an external rotator needs to know the swap has happened
  before it compresses or deletes what it renamed. The acknowledgement
  carries a `Result`: `Ok` means both old handles were flushed and closed AND
  both paths were opened again, while `runner::ReopenError` names the paths
  that could not be opened. Either answer clears the rotator to act on its
  rename, since the old handles are closed regardless — what the error adds
  is that the sheep has no file left to log that stream to. The child is not
  involved and never notices: it holds a pipe, and the daemon does the file
  I/O on the far side of it. Reaching a pump means holding the `ProcIo` field
  below.

  `Flush` is the second variant: it waits for every write already handed to
  the blocking pool to reach the file and keeps the handle, which is the
  half of `shep flush` that runs before anything is truncated. It answers
  with a `Result` too, and for a sharper reason than `Reopen` does —
  `LogFile::reopen` can log a flush failure and move on because the handle
  it belongs to is being replaced by a working one, while nothing here is
  replaced and the bytes still owed are exactly what the truncate is racing.
  `runner::FlushError` names the paths that are not empty, from either half
  of the verb.
- Answer `Request::Reopen`: the supervisor keeps a clone of every running
  sheep's log-control sender and pushes a `LogCtl::Reopen` at each sheep the
  selector matches, which is what makes `create`-mode rotation — rename the
  file, then ask — work at all. Until now the pump kept filling the renamed
  inode and the live path was never recreated, so `shep bleats --no-follow`
  printed nothing and exited 0 with no diagnostic; a restart was the only
  working reopen. The reply lands only once every matched pump has swapped
  both handles, so a `postrotate` stanza that waits for it knows nothing is
  still holding what it renamed. A matched sheep with no live pump is
  reported as a success rather than an error: there was nothing to reopen,
  which is not a failure worth failing `reopen all` over. A pump that
  answered and could not open a path again is the opposite case and fails
  the request (`SupervisorError::ReopenFailed`, `RpcErrorCode::Internal` on
  the wire), naming every such sheep and path — every matched sheep is
  visited first, so one sheep whose log directory is gone neither stops the
  rest being reopened nor goes unreported. The
  acknowledgements are awaited on a task of their own and never inside the
  actor loop — an actor parked on one stops draining its mailbox, which
  stops the sheep task draining its logs, which stops the pump answering.
  Holding that clone costs the pump no life of its own: a pump ends when its
  `logs` receiver goes away as readily as when its last control sender does,
  and the sheep task lets go of both together. That is what retires the pump
  of a sheep whose child forked a lamb and left it holding the pipe — with
  neither stream ever reaching EOF, nothing else would.
- Answer `Request::Flush`: every matched sheep's pump is sent a
  `LogCtl::Flush` and answers, and only then is each distinct recorded log
  path truncated. Both halves of that sentence are load-bearing. The flush
  comes first because `write_all` on a `tokio::fs::File` returns as soon as
  the real `write(2)` is queued, so a line already in flight would otherwise
  land at offset 0 of a file that had just been emptied — the one line that
  survives a flush, in the log its operator was told is empty. And it is the
  RECORDED PATH that is truncated, never the inode the pump currently holds:
  after an external rotator's rename those name different files, and a flush
  that chased the handle would empty the archive and leave the live log
  untouched. Being path-based is also what lets a stopped sheep, which has no
  pump at all, be flushed — its logs are still readable with
  `shep bleats --no-follow`, so they are still worth emptying. Paths are
  deduplicated, so instances sharing one file under `merge_logs` truncate it
  once: one truncate empties the file for every `O_APPEND` handle open on it,
  and a second would only widen the window in which a sibling's freshly
  flushed line can be wiped. A pump that could not land what it owed, or a
  path that could not be truncated, fails the request
  (`SupervisorError::FlushFailed`, `RpcErrorCode::Internal` on the wire)
  naming every such path — keyed by path rather than by sheep, since a shared
  path belongs to no single one. Every matched sheep and path is visited
  first, so one unwritable file neither stops the rest being emptied nor goes
  unreported. A missing path is not a failure: a log file that is not there is
  already empty, and it is deliberately not created, which would otherwise
  leave a stray empty log wherever a rotator had just renamed one away. Like
  the reopen above, every await lives on a task of its own and never inside
  the actor loop.

### Fixes

- Report an automatic restart as automatic. Every restart the daemon raised
  on its own — cron, watch, a memory breach or a liveness failure — emitted
  `BusEvent::Process { manually: true }`, whose documented meaning is "a
  user action caused it". A client using that flag to tell an operator's
  `shep restart` from the daemon acting alone was wrong on all four. This
  is a change on the wire, not only in the docs.
- Stop a watched sheep restarting forever on its own log writes. An app
  naming an explicit `out_file` or `err_file` under its own `cwd` put those
  files inside the tree its watch covered, so each startup line triggered
  the next restart. The default `**/logs/**` ignore never covered it: those
  globs are matched after the watch root is stripped, and the daemon's own
  log directory lies outside the app's `cwd` entirely. The assembled log
  paths are now derived into the watch's ignore set. The loop was
  self-sustaining and `max_restarts` could not stop it, because an
  automatic restart resets the restart budget.
- Send the kill ladder's graceful stop to the sheep's whole process group
  instead of its leader alone, so a wrapper script that forks a child without
  `exec`ing it (`thing & wait`) no longer leaves that child running, orphaned
  and untracked, once the wrapper exits on the signal. The escalated `SIGKILL`
  was already group-wide but only ran on timeout, which such a wrapper never
  reaches — it exits promptly. Lambs now also get a chance to shut down
  cleanly rather than only ever meeting `SIGKILL`.
- Create every runtime directory at `0700` directly via `DirBuilder::mode`
  instead of creating then `chmod`-ing, closing a TOCTOU window where a
  freshly created directory briefly sat at its umask-derived (potentially
  world-writable) mode.
- Adopt the CLI's inherited readiness descriptor as the first fd-touching
  statement in `boot`, before anything else opens or closes one of its own —
  closes an IO-safety hazard where a stale `SHEP_READY_FD` could land on a
  descriptor the daemon had since opened for itself (e.g. its own listener),
  closing it out from under `tokio` on drop.
- Stop a `Delete` racing a `Restart` from bypassing `pending_delete`: the
  caller was told a sheep was deleted while it kept respawning `Online` with
  a live control channel.
- Restart budget now errors at exactly `max_restarts`, not `max_restarts + 1`
  (spec §4).
- Let a `stop` or `delete` override an automatic restart that is already
  mid-kill-ladder. A memory breach, a liveness failure, a cron occurrence or a
  change under a watched tree claimed the sheep's next exit, so a `stop`
  arriving behind one was silently converted into a restart: the sheep came
  back up with `restarts: 1` and the `stop` caller was handed an `Online`
  snapshot of it. Two commands that each have an operator waiting on an answer
  still resolve first-command-wins, and an automatic restart still never
  displaces either. What a restart *does* is unchanged either way — an
  automatic one resets the restart budget exactly as `shep restart` does,
  whichever of the four raised it.

### Changes

- An app that configures `wait_ready` or a `readiness_probe` no longer reaches
  `online` at spawn. It holds at `starting` until the shepherd channel
  delivers `{"kind":"ready"}` or the first probe passes, whichever its config
  selects — `wait_ready` wins when both are set, since the channel is the app
  telling us directly and a probe is an outside guess at the same fact. Apps
  configuring neither are unaffected and still go `online` at spawn.

  No wire type changed, but the timing is visible to anything watching: a
  `shep flock` or `shep describe` issued right after `shep start` now reports
  `starting` for such an app, and the `online` transition arrives on the bus
  later than it used to. Scripts that started an app and immediately asserted
  `online` need to poll instead.

  On `listen_timeout` elapsing without a signal, the sheep goes `online`
  anyway: the daemon logs a warning, and that warning is the only thing
  telling a `starting` that ran long from one that answered — the status and
  the bus event are the same either way. Treating a slow start as a spawn
  failure would produce exactly the restart loop `max_restarts` exists to
  contain, out of an app that is slow rather than broken.
- `BootOptions` gains a `max_cron_sleep: Option<Duration>` field, carrying
  `[daemon] max_cron_sleep` from `shep.toml` to the cron workers; `None` means
  the crate-private default, applied by `boot` and nowhere else. Filed as a
  change rather than an addition because the struct carries no
  `#[non_exhaustive]`: any downstream struct literal that names every field
  stops compiling until it names this one too (`..Default::default()` is
  unaffected).
- `supervisor::Command` becomes `pub(crate)`, removing a public type. Same
  reasoning: it is `pub` in a `pub mod` and not `#[non_exhaustive]`, so every
  new subsystem's command was a semver break on a surface nobody consumes.
  `SupervisorHandle` is the only door into the actor, and nothing outside this
  crate names the enum.
- Most of this crate becomes `pub(crate)`, generalizing that one narrowing to
  the whole surface. The modules `backoff`, `brain`, `bus`, `cron`, `entry`,
  `extras`, `kill`, `server` and `watch` are no longer public at all, taking
  with them `Clock`/`SystemClock`/`spawn_cron_worker`, the entire `extras`
  surface, `WatchFilter`/`WatchSource`/`watch_tree` and their errors,
  `RpcServer`/`check_peer`/`daemon_uid`, `TopicFilter`/`spawn_forwarder`,
  `restart_delay`, `decide_on_exit`, `kill_process`, and `ProcessEntry` with
  its budget and reload types. Inside the modules that stay public, so do
  `MEMORY_POLL_INTERVAL`, `PollingEnforcer`, `LimitBreach`,
  `LivenessFailure`, `spawn_liveness_task`, `probes::os`, `probes::ready`
  (`ReadinessSource`, `Readiness`, `await_ready`), `privilege::resolve` and
  `PrivilegeError`, `SupervisorBuilder`, six of `SupervisorHandle`'s nine
  public methods, `dispatch`/`Outcome`/`budget` and both deadline constants,
  `RpcContext`'s fields, `FlockRegistry`, `write_atomic`, `restorable`,
  `SnapshotWriter` with both snapshot constants, and `boot`'s `init_dirs`,
  `read_pidfile`, `socket_path`, `bind_socket` and `DaemonReady`.

  The rule behind it: a dog is a separate process speaking the protocol, so
  what a dog author builds against is `shep-core`. Nothing needs to link this
  crate, and a `pub` item nobody links is not API — it is a semver
  obligation taken on by accident.

  What is left public is small, and each item now says in its own doc which
  consumer holds it open. `boot`, `tokio_runner` and `boot::DIR_MODE` are
  `shep-cli`'s; `runner`'s whole surface follows from `ProcessRunner` being
  the bound on `boot`. `limits::sample`, `LimitEnforcer` and `Prober` are held
  by the bench crate and by the external-implementor test that keeps those
  seams honest. `assemble`, `channel::ChildMessage`, `privilege::Credentials`,
  `snapshot::read`, `boot::pidfile` and `RunningDaemon::context` are held by
  integration tests, and `supervisor`'s remaining surface by the crate-root
  doc example, which rustdoc compiles as its own crate. `sys` and
  `READY_FD_ENV` stay public with no caller at all: both halves of the
  readiness handshake belong to a `shep-cli` `main` that is not written yet,
  and `adopt_fd`'s ordering precondition cannot be discharged from inside
  this crate.

  Doc links to the newly-private names became plain code spans rather than
  being deleted; in the crate-root taxonomy, a linked module name now means
  public and a backticked one means internal.
- `ProcIo` gains a `log_ctl: mpsc::Sender<LogCtl>` field: the control channel
  into a sheep's log pump, carrying the requests described under Additions
  above. Filed here rather than there because the struct carries no
  `#[non_exhaustive]`: any downstream `ProcIo` literal, or destructuring that
  names every field, stops compiling until it names this one too.

  Dropping the sender ends the pump, so a holder must keep it for as long as
  the child is alive. Ending the pump drops the read ends of the child's
  stdout and stderr along with it, and the child's next write to either then
  gets `EPIPE`/`SIGPIPE` — a dropped sender kills children, it does not
  merely stop collecting from them. A send that fails means the pump is
  already gone, which makes a reopen or a flush a no-op rather than an error.

  The real runner also spawns one pump task per sheep now instead of one per
  stream, so a single request covers both files and answers once.
