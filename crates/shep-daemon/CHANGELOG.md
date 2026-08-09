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

### Fixes

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
  anyway, and silently: the daemon logs a warning, but no `tracing-subscriber`
  is wired yet, so nothing renders it and a `starting` that ran long is
  indistinguishable from one that answered. Treating a slow start as a spawn
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
