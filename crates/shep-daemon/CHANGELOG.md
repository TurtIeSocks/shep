# Changelog

All notable changes to `shep-daemon` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against.

## [Unreleased]

### Additions

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
