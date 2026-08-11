# Refactor Map — pm2 (Node) → shep (Rust workspace)

Primary deliverable. Tree = new structure; every leaf carries `← was` + Action + Notes. Crate names final: `shep-*` (Rin picked `shep` 2026-08-07; naming + lexicon in [../../terminology.md](../../terminology.md)).

**CLEAN-ROOM NOTE (Rin's decision, 2026-08-07):** pm2 is feature-list inspiration only; implementation is clean-room under our own license (MIT OR Apache-2.0). This document is a *behavior spec*, not a code-derivation map — `← was` lines identify which pm2 feature inspired each module, and "compat"/"byte-exact"/"contract" phrasing means *fidelity to the behavior recorded here*, not compatibility with pm2's artifacts. During implementation: build from this spec, do not open pm2 source. We owe nothing to pm2's file layouts, env-var names, dump formats, or flag spellings — keep them only where they're genuinely good defaults.

## Workspace shape (4 crates — lean by design)

```
shep/  (repo dir currently pm2-rs)
  Cargo.toml            [workspace] + [workspace.package] + [workspace.dependencies] + [workspace.lints]
  crates/
    shep-core/           shared types, config, protocol — depended on by everything
    shep-daemon/         supervisor lib (no bin — embedded in the CLI binary)
    shep-client/         async RPC client + programmatic API lib (re-exports core)
    shep-cli/            THE binary: shep (+ shep-runtime, shep-dev thin [[bin]] aliases)
```

**One distributed binary.** `shep` embeds daemon, client, TUI, serve. Daemonization = re-exec self with hidden `daemon` subcommand (ports Client.js auto-spawn ritual; portable to Windows). No separate TUI/serve/init crates at v1 — modules inside `shep-cli`, split only if they grow (ponytail: fewest crates that hold).

rand conventions adopted workspace-wide (see [trace/randStyle.md](trace/randStyle.md) §7): edition 2024 + MSRV in `[workspace.package]`, `default-features = false` everywhere, `dep:`-syntax features with inline comments, `[workspace.lints]` deny missing_docs/undocumented_unsafe_blocks, per-operation small error enums, co-located unit tests + deterministic fixtures, wire-stability snapshot tests, Keep-a-Changelog, Trusted Publishing, SECURITY.md with explicit premises (daemon socket = privilege boundary).

## crates/shep-core

```
src/
  config/
    app_spec.rs      ← was lib/API/schema.json + tools/Config.js + types/index.d.ts
      Action: rewrite as typed serde
      Notes: AppConfig struct, #[serde(alias)] per camelCase alias, MemSize ("100M") +
             Duration ("30s") newtypes via FromStr, ExecMode enum, env_* flatten map,
             shlex string→args. schemars JSON-schema export for docs. THE compat contract —
             every key ported; APM knobs (trace/v8/pmx/io/...) dropped.
             `channel` (Phase 5) is the one key with no pm2 ancestor: it opens the fd-3 shepherd
             channel for an app that wants one without also wanting `wait_ready` or
             `shutdown_with_message`, which were previously the only ways to get one. Defaults
             to false — see spawn.rs for the per-sheep cost that default is protecting.
    ecosystem.rs     ← was lib/Common.js (parseConfig + file detection)
      Action: port + redesign
      Notes: serde_json strict (NOT JS-eval — kills code-exec-on-parse), json5, serde-saphyr.
             .config.js/.cjs/.mjs → spawn `node -p JSON.stringify(require(p))` (documented:
             JS configs need node on PATH). Extension match by endsWith (fixes substring bug).
    normalize.rs     ← was lib/Common.js (prepareAppConf/verifyConfs/mergeEnvironmentVariables)
      Action: port + redesign
      Notes: pure AppConfig → Result<ResolvedApp> functions (mutation+Error-or-value → typed).
             Alias/default/env-merge rules byte-compatible: cmd→script, fork_mode, bash -c
             on spaced scripts, log path defaults, filter_env. Cron validation delegates to
             config/cron.rs, whose `cron_parser()` builds croner with `Seconds::Disallowed`:
             FIVE-FIELD standard cron only, and croner's own `L`/`W`/`#`/`?` extensions are
             rejected before the pattern reaches it. The seven vixie `@nicknames` are expanded
             to five-field patterns first; `@reboot` is rejected. Same dialect stated from the
             worker's side: the daemon's cron.rs entry below.
      Rejects (spec §5 — a typo fails at parse time, not three seconds into a worker's life).
             The list grows as subsystems land; as of Phase 4, `normalize` refuses:
             - InvalidCron / InvalidTimezone — a pattern outside the dialect above; a
               `cron_timezone` that is not an IANA name, checked even with no `cron_restart`
             - InvalidProbe — a `readiness_probe`/`liveness_probe` target ProbeTarget::parse
               rejects, `https://` included (no TLS in the prober — decision D1)
             - ZeroFailureThreshold — a probe's `failure_threshold` explicitly `0`
             - IntervalBelowMinimum — a `liveness_probe.interval` under one second, which
               would hot-spin an unbounded loop. A `readiness_probe.interval` is exempt:
               that poll is bounded by `listen_timeout`
             - ZeroMaxMemory / ZeroWatchDelay — `max_memory` of `0` (a ceiling every
               process exceeds) or `watch_delay` of `0` (the debouncer's tick is
               `delay / 4`, so zero pegs a core per watched app)
             - WatchWithoutCwd — `watch = true` with no `cwd` to arm a watcher over
             - InvalidWatchGlob — a `watch_options` or `ignore_watch` pattern globset will
               not compile; both lists are checked whether or not `watch` is on
             Each carries the field it came from; the parsed values are discarded, since the
             daemon re-parses when it arms the subsystem. `normalize`'s own `# Errors` section
             is the authoritative list — keep the two in step.
    daemon_config.rs ← new module, no old equivalent            [MUST-HAVE #8]
      Action: write fresh
      Notes: daemon-level config file (TOML): metrics on/off+port, webhook targets, alert
             thresholds, log policy. Layered: file < env < CLI flags (figment or hand-rolled).
             pm2 had nothing here — env-var soup only.
      Log policy so far (Phase 5): `[daemon] log_level` (`SHEP_LOG_LEVEL`), a `LogLevel` of
             exactly off/error/warn/info/debug/trace, lowercase and nothing else, defaulting to
             `warn` — where the daemon's warn-and-continue arms live. A name outside the six is
             a startup ERROR, never a silent fallback. `[daemon] log_json` (`SHEP_LOG_JSON`) had
             been parsed and ignored since it was added; it now picks the renderer. Both are
             read by shep-cli's `install_log_subscriber` (see main.rs, below) — this crate only
             parses them.
    kv.rs            ← was lib/Configuration.js
      Action: port
      Notes: pm2 set/get/unset store; dotted/colon key parse w/ quotes, `all` wipe,
             module_conf.json 4-space format kept; + fd-lock advisory lock (fixes RMW race).
             Sync/async duplication collapses to one async impl.
  paths.rs           ← was paths.js
      Action: port
      Notes: Paths::resolve(env) struct; SHEP_HOME + SHEP_* overrides as explicit match table
             (decision 7: sheep-native names; pm2 layouts live only in the importer).
             Windows: per-user pipe name (fixes the trace-noted gap). Layout: ~/.shep/
             {flock.json, logs/, pids/}.
  constants.rs       ← was constants.js
      Action: port (reduced)
      Notes: Config struct + LazyLock; ProcStatus enum with serde(rename) keeping JSON strings
             ("waiting restart" etc). Keymetrics consts dropped.
  selector.rs        ← was scattered in API.js/_operate + Log/Monit fallback chains
      Action: merge (dedup)
      Notes: enum ProcessSelector { All, Name, Id, Regex, Namespace } parsed once, resolved by
             one fn — replaces 5 copy-pasted resolution chains. regex crate (no ReDoS).
  protocol/
    request.rs       ← was modules/pm2-axon-rpc (method-name-string dispatch)
      Action: rewrite
      Notes: #[derive(Serialize,Deserialize)] enum Request — all 30 daemon methods typed.
             Envelope{id: u64, deadline}; Response::Err{code, message, stack} structured
             (pm2 had string-match errors, no deadlines anywhere).
    events.rs        ← was lib/Event.js + God bus ad-hoc objects + pm2-io-bpm packet shapes
      Action: port shapes + typing
      Notes: BusEvent enum: ProcessEvent{event,manually,process,at}, LogOut/LogErr, ProcessMsg,
             AxmMonitor/AxmAction/AxmReply, ProcessException, Pm2Kill. Wire shapes golden-
             snapshot-tested (insta) — @pm2/io-instrumented Node apps keep working.
    wire.rs          ← was modules/pm2-axon + amp framing
      Action: rewrite
      Notes: LengthDelimitedCodec(u32) + serde_json frames over UDS (unix) / named pipe
             (windows, via interprocess). Kills 15-arg AMP limit, hwm=Infinity unbounded
             queues. KEEP two battle-tested algorithms: stale-UDS detection (EADDRINUSE →
             probe-connect → unlink-if-refused) and reconnect backoff (100ms ×1.5 cap 5s).
  error.rs           ← new (rand idiom)
      Action: write fresh
      Notes: per-operation small enums (SpawnError, ConnectError, ProtocolError, NormalizeError),
             Clone+Copy+Debug+PartialEq+Eq where fieldless, manual Display, core::error::Error.
```

## crates/shep-daemon

```
src/
  supervisor.rs      ← was lib/God.js (prepare/executeApp/handleExit/injectVariables)
      Action: port + redesign
      Notes: actor task owning HashMap<ProcId, ProcessEntry> (God is single-threaded today —
             actor keeps that honest). Restart brain byte-exact: min_uptime×max_restarts
             window, backoff ×1.5 cap 15s, stop_exit_codes. Instance expansion 0/-N→numCPUs.
             NODE_APP_INSTANCE slot algorithm + increment_var kept. _old_<id> string-key hack →
             explicit ReloadState enum on entry. executeApp 220-line pyramid → sequential async fn.
      Log plane (Phase 5): the actor keeps a clone of every running sheep's `ProcIo::log_ctl`,
             which is what lets `handle_reopen`/`handle_flush` reach a pump without a restart.
             Neither awaits inside the actor loop — `spawn_reopen_task`/`spawn_flush_task` own
             the awaits, because an actor parked on an acknowledgement stops draining its
             mailbox, which stops the sheep task draining its logs, which stops the pump
             answering. Every matched sheep is visited before anything is reported, so one
             broken log directory neither stops the rest nor goes unreported
             (`SupervisorError::ReopenFailed` / `FlushFailed`, `RpcErrorCode::Internal` on the
             wire). A matched sheep with no live pump is a success for reopen (nothing to
             reopen) and still flushable (a flush truncates PATHS).
             Flush's barrier is drawn around the FILE, not the selection: `paths` is a
             `BTreeSet<PathBuf>` of every matched sheep's recorded out/err file, and every pump
             writing to one of those paths is flushed first — including a sheep the selector
             skipped that shares a path under `merge_logs`, whose in-flight line would otherwise
             be the one line surviving the emptying. Then each DISTINCT path is truncated once.
             It is the RECORDED path that is truncated, never the inode the pump holds: after an
             external rename those name different files, and chasing the handle would empty the
             archive and leave the live log alone. A missing path is not a failure and is
             deliberately not created.
             `handle_reopen` draws its barrier around the same set (Phase 6): every slot writing
             to a path a matched sheep writes to, matched or not. It was selector-keyed until
             reload made EVERY app a shared-log-path app for the length of a swap — both halves
             derive byte-identical paths from the instance slot they share — so `shep reopen
             <one id>` mid-reload left the drainee appending to the renamed inode while the
             `postrotate` stanza waiting on the exit code was told otherwise. A path is what an
             external rotator renames, and an operator naming one writer of a file is not a
             statement about the others. Both REPLIES stay selector-keyed: a row means "a sheep
             you named". A failure may name one you did not, which is the honest report of a
             shared file that could not be reopened.
  spawn.rs           ← was lib/God/ForkMode.js + ClusterMode.js (unified — ONE spawn path)
      Action: port + merge
      Drift (Phase 3, recorded in Phase 5): shipped as a PAIR — `runner.rs` (portable: the
             `ProcessRunner` seam, `ProcIo`, the log-control types, and the scripted fake that
             makes the supervisor's tests deterministic) and `tokio_runner.rs` (unix: the real
             `tokio::process` spawn, the log pump, `open_append`). A seam whose trait and whose
             OS implementation share a file cannot compile without the OS half.
      Notes: tokio::process::Command, process_group(0), uid/gid via CommandExt, piped stdio +
             the shepherd channel on fd 3 (a socketpair, newline-JSON both ways: `ChildMessage`
             = ready/metric/action-reply up, `ShepherdMessage` = shutdown/action down; keeps a
             future shim's compat). Opened when `assemble` sees
             `channel || wait_ready || shutdown_with_message` and not otherwise: a socketpair
             plus two pump tasks per sheep is real cost against spec §14.11's idle-RSS goal.
             NOTE the channel does NOT carry a log:reload of pm2's kind — see the log plane
             below, where the child is not a participant at all.
             Cluster mode = N fork instances (Node cluster injection dies — see reload.rs for
             the load-balancing story). Log pipeline: BufReader lines → broadcast + append
             files. Framing (raw/date-prefix/json) and the /dev/null skip are still ahead.
      Log plane (Phase 5): ONE `spawn_log_pump` task per SHEEP, not one per stream, so a single
             request covers both files and answers once. `open_append` opens each `LogFile` with
             `.append(true)` and that is load-bearing rather than a convenience: `O_APPEND` seeks
             to end-of-file per write, which is the whole reason an external `copytruncate`
             rotator works with no cooperation from shep — a handle carrying its own offset would
             put the next line past a sparse hole the size of what was emptied. `open_append`
             also creates any parent directory it needs at `boot::DIR_MODE` (0700), which is what
             puts a log DIRECTORY back at that mode after a rotator moved it aside, and which is
             also what an app whose `out_file` points outside the layout gets. The pump is the
             only owner of that guarantee.
             Reaching a live pump is `ProcIo::log_ctl`, an mpsc sender of `LogCtl`: `Reopen`
             flushes, closes and re-opens both files (`create`-mode rotation); `Flush` lands what
             is already in flight and KEEPS the handle (the half of `shep flush` that runs before
             anything is truncated). Each answers on a oneshot carrying a Result —
             `ReopenError`/`FlushError` name the paths — because an external rotator has to know
             the swap happened before it compresses what it renamed, and a flag the pump would
             notice "before its next write" promises nothing about a sheep that has gone quiet.
             The CHILD is never involved and never notices: it holds a pipe, the daemon does the
             file I/O on the far side, so no signal, no fd surgery and no restart is needed to
             rotate or empty a sheep's logs. A pump ends when its `logs` receiver goes away as
             readily as when its last control sender does — which is what retires the pump of a
             sheep whose child forked a lamb and left it holding the pipe, where neither stream
             ever reaches EOF.
  kill.rs            ← was lib/God/Methods.js (kill ladder) + lib/TreeKill.js
      Action: port semantics + rewrite mechanism
      Notes: ladder exact: SIGTERM(configurable via `kill_signal`)/shutdown-msg → timeout on
             child.wait() → SIGKILL survivors → timeout error. SIGTERM rather than pm2's SIGINT
             is a deliberate deviation (spec §"deviations": unix convention wins). Mechanism:
             owning-parent waitpid (exact exit code+signal, kills pid-reuse ABA race),
             kill(-pgid) for trees (replaces racy ps-snapshot walk), Job Objects on Windows
             (fixes taskkill /F signal-ignoring).
      Two caps, not one (Phase 6): `LadderCap` picks which of the app's timeouts bounds the
             wait. `Stop` is `kill_timeout` (1600ms) and covers an operator's stop/restart/
             delete, the daemon's own automatic restarts and the engine-wide shutdown; `Drain`
             is `graceful_timeout` (8000ms) and belongs to a reload's drain — the one stop that
             asks the instance to finish the work already in hand, and so the one given longer
             to do it. `graceful_timeout` had no reader in the daemon before reload;
             `kill_timeout` already bounded the wait on every other stop.
  reload.rs          ← was lib/God/Reload.js
      Action: port contract + rewrite mechanism                 [UPGRADE over pm2]
      Drift (Phase 6, recorded): THERE IS NO SUCH FILE. A reload is a sequence of actor
             decisions over the actor's own state, so it shipped inside supervisor.rs —
             `ReloadJob`, `ReloadSwap`, `ReloadPhase`, and the `handle_reload` →
             `advance_reload` → `spawn_replacement` → `reload_ready_result` → `begin_drain` →
             `reap_drainee` → `finish_swap` chain — with the per-entry marker `ReloadState` on
             entry.rs. A module of its own would have had to reach into `Actor::sheep`,
             `next_id`, the kill ladder, the extras registry and the bus, which is every field
             the actor exists to own.
      Notes: explicit state machine SpawnNew → AwaitReady → DrainOld → ReapOld, one instance of
             an app at a time. `ReloadPhase` carries only two of the four names because the
             other two are instants rather than intervals: SpawnNew is one synchronous step of
             the actor loop, and ReapOld is the drainee's `Msg::Exited` arriving. The question
             a handler actually asks is the one those two variants answer — is the old
             instance still there to go back to?
             ONE `ReloadJob` PER APP NAME in `Actor::reloads`, holding the instances not yet
             taken (`queue`, in instance-slot order) and the one pair mid-swap (`swap`). The
             key is what makes a second reload of the same app REFUSABLE rather than queued or
             interleaved (`SupervisorError::ReloadInFlight`, refused whole before anything is
             spawned), and what every other handler asks "is this id half of a live reload".
             SpawnNew (`spawn_replacement`) registers the replacement under a NEW ID IN THE
             DRAINEE'S INSTANCE SLOT — same slot because `assemble` writes it into the child's
             environment and an app deriving its port from it must bind the same port, new id
             because two live processes under one id is what the supervisor's property test
             forbids. Reload is the first operation for which those two diverge. The drainee
             takes `ProcStatus::Stopping` — which this is the ONLY production writer of, in
             the whole daemon — and `ReloadState::SpawningReplacement{new_id}`, both BEFORE
             `runner.spawn` is called; the
             replacement carries `ReloadState::Draining`, which says only which half of the
             swap it is — the drainee it must outlive is named by the reload job, in the
             entry ids the rest of the machinery navigates by, rather than by an OS pid
             nothing reads. That ordering is load-bearing twice over: `snapshot.rs`'s `is_running`
             does not count `Stopping`, so the muster roll cannot record a count the flock
             does not have; and `handle_extra_restart`'s `Online` guard then rejects the two
             automatic triggers that reach it — a memory breach and a liveness failure — for
             the whole overlap, which is what stops a drainee being RESTARTED because it is
             draining. `restarts` carries over from the drainee, the restart budget does not.
             AwaitReady is gated for EVERY replacement, including an app configuring neither
             `wait_ready` nor `readiness_probe` — `await_ready`'s `Heuristic` arm exists for
             this caller and had no production caller before it. `reload_ready_result` keys on
             the `Readiness` VERDICT and never on the deadline, which is what makes one mapping
             correct for all three sources: `Heuristic` reports `Ready` when its deadline
             elapses, because for a heuristic the elapse IS the signal. `TimedOut` is the one
             place a readiness deadline is a FAILURE rather than a slow start, and it abandons.
             DrainOld (`begin_drain`) is the ordinary kill ladder under `LadderCap::Drain`
             (`graceful_timeout`, not `kill_timeout` — see kill.rs), claimed through
             `claim_manual` like an operator's stop. ReapOld is `reap_drainee` on the drainee's
             exit, which deregisters it — nothing else ever would, and a drainee left
             registered is a dead row per instance per reload — then `finish_swap` and on to
             the next instance.
             ABANDONMENT (`abort_reload`, spec §4): the drainee goes back to serving where that
             is still true, the instances not yet reached are left alone, and the replacement
             is killed through the ladder (`LadderCap::Stop` — nothing is being drained) and
             DEREGISTERED rather than left as an `Errored` row that would double every
             name-keyed verb for the life of the flock. Only reachable while the swap is
             `AwaitReady`; past the commit there is no old instance to return to.
             An automatic restart is held off BOTH halves of an uncommitted swap
             (`in_an_uncommitted_swap`, checked in `begin_manual`) — a cron occurrence or a
             watched-file change landing inside the readiness window destroys the overlap from
             either side. The trigger is DROPPED, not deferred. `begin_shutdown` clears every
             job (CRITICAL-1: a reload's next step is always a spawn).
             THE WIRE: `Request::Reload` is answered `Response::Reloading` the moment the
             reload is ACCEPTED, before the first replacement is spawned, because one instance
             costs `listen_timeout` + `graceful_timeout` ≈ 11s and `rpc.rs` caps a request
             budget at `MAX_DEADLINE_MS` 60s — a synchronous reply would time out while the
             reload went on running. Progress is therefore the bus's job alone:
             `process.reload` on the instance being replaced (before its replacement's
             `process.start`), `process.reloaded` on the replacement once the drainee is gone,
             `process.reload_abandoned` on the instance still serving.
      NOT BUILT, and the previous entry here promised it: SO_REUSEPORT is NOT set by shep and
             cannot be — a socket option must be set before `bind()` by the process that binds,
             and no shep process ever binds an app's port. `reuse_port` is the OPERATOR
             ASSERTING the app sets it itself; a mismatch is `EADDRINUSE` at every replacement
             spawn, undetectable in advance. No socket2, no LISTEN_FDS fd passing (spec defers
             it to v1.2). So THE OVERLAP IS THE WHOLE MECHANISM, and it is not zero downtime:
             the old listener's accept backlog is reset when it closes, on both tier-1
             platforms, so a reload is downtime-free exactly insofar as the APP stops
             accepting, drains and exits inside `graceful_timeout`. Measured, not reasoned:
             a defiant app loses 5-8 connections in ~260 on Linux (which load-balances new
             connections across every listener on the port, so the drainee keeps taking about
             half until it closes) and none on macOS (last binder wins, so the replacement
             takes everything the moment it is up). A draining app loses none on either.
  watch/             ← was lib/Watcher.js
      Action: port + redesign
      Drift (Phase 4, recorded): built as a DIRECTORY, not the `watcher.rs` named above — the
             OS seam and the filtering logic have different test tiers (source.rs needs a real
             filesystem, mod.rs is pure), and one file would have crossed Rin's 500-line split.
      Notes: notify + notify-debouncer-full; ONE watcher per name-group (fixes O(N²) fan-out);
             ignore defaults (dotfiles, node_modules) via globset; watch_delay = debounce dur;
             re-check after restart completes (fixes dropped-event gap). disableAll bug not ported.
             A trigger restarts the WHOLE name-group, stopped instances included; what keeps a
             stopped sheep down is disarming its group's watcher, never a filter on the restart.
    source.rs        the OS seam: notify's debounced batches → tokio mpsc, `WatchSource` drop
                     guard. `watch_tree` is the seam fn (no trait — one implementation, and the
                     fake tier drives the channel directly).
    mod.rs           `WatchFilter` (pure globset include/ignore) + `spawn_watch_group`'s restart
                     loop. A rescan — notify's own flag for "events were dropped, re-read the
                     tree" — triggers before either glob set is consulted. It rides beside the
                     paths rather than being spelled as one, so an ordinary event on the watch
                     root is filtered like any other path.
  probes/            ← new module (spec §7 — pm2 had no probes at all)
      Action: write fresh
      Drift (Phase 4, recorded): map.md never named this module; spec §7 requires it, and where
             the two disagree the spec wins.
      Notes: `Prober` seam (SEAM TRAIT 3/3) + the liveness loop; failure_threshold consecutive
             misses report once, then the loop ends (the replacement pid gets a new loop).
             Hand-rolled HTTP with no TLS and no redirect following — `https://` targets are a
             config error (decision D1), rejected in shep-core so a typo fails at parse time.
    os.rs            the real HTTP/TCP/exec prober, and these modules' OS tier. Three things in
                     `probe_exec` and beside it are `cfg(unix)`/`cfg(windows)`, not one: shell
                     selection (`sh -c` vs `cmd /C`), `process_group(0)` on the probe child, and
                     the `kill_probe_group` unix/windows pair that SIGKILLs an abandoned probe's
                     whole group (a no-op on windows, which has no group to signal). Everywhere
                     else in the Phase 4 modules a `cfg` gates a TEST, not behavior — one in
                     extras.rs, one in watch/source.rs, plus os.rs's own unix-only cases.
    ready.rs         `ReadinessSource`/`await_ready` — the starting→online gate. `wait_ready`
                     (channel) beats `readiness_probe`; with neither, a plain start is online at
                     spawn (the Heuristic source is reload's, not start's). A readiness TIMEOUT
                     takes the sheep online SILENTLY — never `errored`, which would be the
                     restart loop max_restarts exists to contain, out of an app that is merely
                     slow. `Actor::handle_ready_result` emits a `tracing::warn!` on that path,
                     which shep-cli's `install_log_subscriber` renders into shepd.err.log at the
                     default `warn` level; the bus still shows only the `online` transition.
  actions.rs         ← was lib/God/ActionMethods.js
      Action: port + redesign
      Notes: each RPC verb = async handler on Request enum arm (string dispatch dies).
             eachLimit(2) → for_each_concurrent(2). Watch-by-name silent no-op bugs fixed.
             getReport redacts env by default.
  snapshot.rs        ← was ActionMethods.dumpProcessList + API/Startup resurrect path
      Action: port + fix
      Notes: serde Vec<AppSpec>, tempfile+rename atomic write (backup-dance deleted). Own
             format (decision 7); pm2 dump.pm2 parsing lives in shep-cli import.rs only.
             Resurrect (= muster) diff-by-name, spawn missing.
  rpc_server.rs      ← was lib/Daemon.js (RPC surface + boot)
      Action: port + redesign
      Drift (Phase 3/5, recorded in Phase 5): split three ways — `boot.rs` (the ritual and the
             signal handlers), `server.rs` (the unix socket and its per-connection tasks) and
             `rpc.rs` (the portable dispatcher, which never touches a socket or a byte).
      Notes: boot ritual kept (pidfile, both-sockets-bound readiness handshake via pipe,
             SIGTERM/INT/QUIT graceful dump+exit). Per-conn task: read frame → dispatch →
             reply. Peer-cred check (SO_PEERCRED/getpeereid) — pm2 had NONE. Per-call
             deadlines default 5s, capped at `MAX_DEADLINE_MS` 60s. Drop: domain resurrection,
             $_ env hack, inspector self-profiling.
      One verb outruns the ceiling (Phase 6): a reload of a clustered app costs ~11s PER
             INSTANCE, so no budget a client is allowed to ask for can cover it, and expiring a
             budget bounds the REPLY and not the actor's work. `Reload` is therefore answered
             `Response::Reloading` — an ACCEPTANCE, the one reply in the enum that does not
             describe finished work — and its progress goes on the bus instead. Raising the
             ceiling was the alternative and was refused: it would cost every other verb its
             meaning. Both of reload's refusals (an app already reloading; a reload arriving
             after shutdown has begun) map to `RpcErrorCode::Internal` for want of a code of
             their own, which is a wire change rather than a mapping change.
      SIGUSR2 = REOPEN, not reload (Phase 5): `boot::install_signals` installs it, and it means
             exactly `shep reopen all` — a signal carries no selector, so `all` is the only
             thing it can mean. Installing it is load-bearing on its own, since SIGUSR2's
             default disposition is to terminate: an unhandled `kill -USR2` would kill the
             shepherd rather than rotate it, so the handler goes in at boot step 1, before the
             socket is bound. The supervisor it reopens through does not exist until step 4, so
             `install_signals` returns a `oneshot::Sender<SupervisorHandle>` that `boot` sends
             on once it has one; the listener parks on the receiver. The disposition is already
             replaced when `install_signals` returns, so a SIGUSR2 raced into the step-1/step-4
             gap is served LATE, never dropped. What the signal form gives up against the
             socket form: no reply, so the result is logged and nothing can wait for the swap,
             and no selector narrower than the whole flock.
  bus.rs             ← was God.bus (EventEmitter2) + axon pub/sub
      Action: rewrite
      Notes: tokio::sync::broadcast<BusEvent>; wire side: subscribe-with-topic-globs on connect,
             server-side filtering (pm2: broadcast-everything). Bounded queue + drop-oldest +
             drop-count event (pm2: unbounded, silent).
  worker.rs          ← was lib/Worker.js
      Action: NOT BUILT
      Drift (Phase 4, recorded): pm2's Worker.js is one timer loop doing four unrelated jobs, and
             porting it as one module would have rebuilt that coupling. Its loops were split to
             live beside the subsystems they serve: cron_restart → cron.rs, max_memory_restart
             poll → limits/. Backoff reset already lives in the restart brain; host metrics
             cadence belongs to the metrics dog, not the daemon. Nothing is missing — the file
             is.
  cron.rs            ← was lib/Worker.js (cron_restart half)
      Action: write fresh
      Notes: `Clock` seam (SEAM TRAIT 1/3) — cron means WALL time, every other deadline here is
             a tokio Instant a paused test can move, so the two cannot be one clock. Five-field
             standard cron via croner, seconds disallowed, croner's L/W/#/? extensions rejected;
             the seven vixie @nicknames expanded to five-field patterns by shep before croner
             ever sees them. Re-derives the next occurrence at least every `max_cron_sleep`
             ([daemon] key, 60s default, floor 1s) so a laptop suspend or NTP step costs at most
             that much drift. A missed occurrence is NOT replayed. Restarts the whole name-group,
             stopped instances included — same reach as watch/.
  limits/            ← was lib/Worker.js (max_memory_restart half)
      Action: write fresh
      Drift (Phase 4, recorded): a DIRECTORY, for the same seam/logic split as watch/.
      Notes: `MemorySampler` seam (SEAM TRAIT 2/3) over sysinfo + the pure `tree_rss` sum;
             `LimitEnforcer`/`PollingEnforcer` watch for a breach at MEMORY_POLL_INTERVAL (15s,
             benchmark-backed by benches/). DEVIATION from pm2: the ceiling is enforced against
             the process TREE (sheep + lambs via the ppid walk), not the root pid, because a
             root-pid limit is trivially dodged by any app that forks workers. Wants a line in
             docs/migration.md.
  extras.rs          ← new module (no pm2 counterpart)
      Action: write fresh
      Notes: the registry that arms all four subsystems above when a sheep goes live and disarms
             them across eight terminal transitions (seven disarming) plus its own Drop, which
             aborts every armed task — covering both a graceful shutdown that never kills a
             WaitingRestart sheep and a panicking actor. Cron and watch restarts route through
             `SupervisorHandle::restart_automatic`; breach and liveness route through
             `SupervisorHandle::extra_restart`, a separate `Command` variant whose handler drops
             a stale report first — slot still present, pid still this sheep's, status still
             Online — three guards `restart_automatic` does not carry (`handle_extra_restart`).
             Both doors declare CommandOrigin::Automatic, so an operator's stop or delete
             displaces either one mid-ladder.
  dog_support.rs    ← new module (decision #3: dog architecture)
      Action: write fresh
      Notes: daemon-side dog plumbing ONLY: enabled-dogs list in daemon_config → autostart
             as supervised internal-tagged processes; typed [dog.<name>] config sections
             passed through. Metrics + bark logic themselves are dogs in shep-cli (below) —
             the daemon just exposes bus + monitoring RPCs they consume.
  host_metrics.rs    ← was lib/tools/SysMetrics.js
      Action: replace-with-crate (sysinfo)
      Notes: keep axm_monitor snapshot shape + metric names (pm2 ls renderer compat);
             Windows metrics free (JS was Linux/macOS only).
  vcs.rs             ← was modules/vizion (feature "vcs", off by default)
      Action: port
      Notes: fork-hardened shape kept: git via argv vectors, LC_ALL=C, GIT_TERMINAL_PROMPT=0,
             timeouts. NotARepo → supervisor walks up (split preserved). gix/git2 rejected —
             shell-out matches user's git auth behavior.
```

## crates/shep-client

```
src/
  client.rs          ← was lib/Client.js
      Action: port + redesign
      Notes: ping → auto-spawn daemon → connect state machine kept ("first command boots
             daemon"). Typed async wrappers for all Request variants. executeRemote
             method-name sniffing dies. Version handshake in hello frame (pm2: out-of-band).
  api.rs             ← was lib/API.js (lifecycle plumbing)
      Action: port + redesign
      Notes: Pm2 struct, async methods returning Result (cb-or-exitCli dual mode dies; only
             CLI maps to exit codes). _startJson/_startScript flows sequential-async.
             Module-restart-only rule, --update-env immutability kept.
  events.rs          ← was API launchBus path
      Action: rewrite
      Notes: subscribe(topic globs) → stream of BusEvent. `EventStream::next` is an INHERENT
             method, so pulling one event needs no `futures-util` in the consumer's own manifest
             (pinned by a test that imports none); the `Stream` trait is re-exported from the
             crate root for callers who need it nameable in a bound. Same one-dependency rule
             lib.rs's shep_core re-export follows.
  lib.rs             re-exports shep_core (rand: one-dep consumers) + prelude module.
```

## crates/shep-cli

```
src/
  main.rs            ← was bin/pm2 + lib/binaries/CLI.js bootstrap
      Action: rewrite (clap v4 derive)
      Notes: multi-call binary (argv[0] dispatch) + [[bin]] aliases pm2-runtime/pm2-dev.
             Hidden `daemon` subcommand = daemonization target. Lazy daemon connection
             (kills --no-daemon argv-scan + startup 100ms hacks).
      The daemon's own diagnostics live HERE, not in shep-daemon (Phase 5 decision):
             `commands/daemon.rs`'s `install_log_subscriber` is called by `run_daemon` and
             deliberately NOT by `shep_daemon::boot`. A global subscriber installs once per
             process; `boot` is called many times over by a single test binary, so a subscriber
             inside it would fail every test after the first. `run_daemon` is called once, by
             `main`, and the e2e tier reaches it as a subprocess — which is the only way to
             exercise it. It writes to STDERR and names no file: a hand-run daemon logs to its
             terminal, and a launched one logs to $SHEP_HOME/logs/shepd.err.log because
             `launch.rs` already redirects that stream there. `[daemon] log_level`
             (`SHEP_LOG_LEVEL`, default `warn`) picks the level and `[daemon] log_json`
             (`SHEP_LOG_JSON`) picks the renderer. `RUST_LOG` is deliberately IGNORED — it would
             be a second way to configure shep, competing with our own knob over one decision,
             which is what the SHEP_ prefix rule exists to prevent. `NO_COLOR` is honoured, on
             the opposite reasoning: it is a cross-ecosystem convention about the terminal
             rather than a shep knob, so it is not ours to opt out of.
  commands/*.rs      ← was lib/binaries/CLI.js command definitions + lib/API/Extra.js keepers
      Action: port surface
      Notes: every command+flag from the trace enum; global opts via #[arg(global)];
             `--` passthrough native (patchCommanderArg dies); -c dup resolved (cron-restart
             wins, --cron long alias); StartOptions struct = the camelCase→API contract,
             explicit + tested. Duplicate verbs collapse to clap aliases; dead surface
             (imonit, deepUpdate, --v1, conf, create) hidden or gone. stdin `-` JSON kept.
      `shep reload <selector>` (Phase 6): `commands/lifecycle.rs::reload`, alongside
             `stop`/`restart`/`delete` and sharing their `SelectorArgs` — so the selector is
             REQUIRED for the same reason theirs are, now pinned by a test over every verb that
             shares the struct (a `default_value` on that one field would have turned a bare
             `shep stop` into `shep stop all` for six verbs at once). It takes the client's 5s
             default deadline, not the log plane's 30s, because the daemon answers on
             ACCEPTANCE — the command prints the flock as it stood at that moment and exits,
             and does NOT subscribe to the bus to follow the swaps. Its `--help` says in as
             many words that the window is not zero downtime.
  runtime.rs         ← was lib/binaries/Runtime4Docker.js (+ Runtime.js dropped)
      Action: port + fix
      Notes: no-daemon mode = daemon event loop in-process. Exit-code contract exact
             (auto-exit fail_count 3 / 2s / code 2). PID-1 zombie reaping added (subreaper +
             WNOHANG loop — pm2 never reaped re-parented orphans).
  dev.rs             ← was lib/binaries/DevCLI.js
      Action: port
      Notes: ~/.pm2-dev namespace, forced watch, post-exec hook, auto-exit; bus subscription
             replaces 1s-setTimeout race.
  output/            ← was lib/API/UX/* + cli-tableau + ansis
      Action: port content, swap machinery
      Notes: comfy-table (ANSI width correct — fitColumn workaround dies), owo-colors +
             anstream (NO_COLOR free), width-adaptive full/condensed/mini as layout enum.
             jlist gains versioned serde schema + global --format json|table (pm2 gap).
  tui.rs             ← was lib/API/Dashboard.js + Monit.js (merged)   [MUST-HAVE #5]
      Action: rewrite (ratatui + crossterm)
      Notes: 4-pane dash UX kept, event-driven redraw (300ms full-rerender dies), + host
             usage pane (sysinfo), search/filter, OOB-selection crash fixed. One TUI, not two.
  logs.rs            ← was lib/API/Log.js + LogManagement.js
      Action: port + redesign
      Drift (Phase 4/5, recorded in Phase 5): split by what the verb ACTS ON, not by its old
             file. `commands/bleats.rs` READS a sheep's output (it is what `shep logs` aliases);
             `commands/logs.rs` acts on the log FILES — `reopen` and `flush`, and nothing else.
      Notes: LogFormat enum {Pretty,Raw,Json,Logfmt}; reverse block reader for tail (lines×200-
             bytes guess dies); printLogs/streamLogs 90-line copy-paste merged.
      Log-plane verbs (Phase 5): `shep reopen [selector]` — selector OPTIONAL, defaulting to
             `all` like `bleats`, since it destroys nothing and rotating the whole flock at once
             is the ordinary case; it is the half of `create`-mode rotation that runs after the
             rotator's rename, and a zero exit is what a logrotate `postrotate` stanza waits on.
             `shep flush <selector>` — selector REQUIRED, following `stop`/`restart`/`delete`:
             this is the one verb whose slip of the finger cannot be undone, and `shep flush
             all` is short to type when it is meant. Both send `LOG_PLANE_DEADLINE` (30s, from
             shep-client) rather than the 5s default, because the daemon walks the matched flock
             file by file with no per-sheep bound. Both render the matched sheep as the same
             table `stop` and `restart` answer with — ONE ROW PER SHEEP, never per file emptied.
             What `flush` empties is exactly the paths the Flockfile names, taken verbatim and
             never checked against the log directory, for every registered sheep the selector
             matches whether or not it ever ran. Out of reach by construction: the shepherd's
             own shepd.out.log/shepd.err.log, which the CLI's launcher creates before the daemon
             exists and the daemon inherits as plain fds 1 and 2 — it holds no handle to flush
             and no recorded path to truncate, so restarting the shepherd is what empties those.
      KNOWN DUPLICATION: `request_and_render` + `parse_selector` are a third per-module copy,
             after `commands/lifecycle.rs`'s and `commands/query.rs`'s. Extraction is deferred,
             not decided against.
  startup.rs         ← was lib/API/Startup.js + lib/templates/
      Action: port (reduced platforms)
      Notes: systemd (Type=notify + sd_notify — upgrade from Type=forking), launchd, openrc,
             freebsd/openbsd rc.d. upstart/systemv/smf dropped (dead platforms). Templates via
             include_str! + typed context. Root check nix::geteuid. windows-service crate =
             native Windows service (pm2 never had it).
  serve.rs           ← was lib/API/Serve.js
      Action: port + harden
      Notes: axum + tower-http ServeDir (traversal/ranges free), SPA fallback, dir listing,
             basic-auth via ConstantTimeEq + creds file (not env), PM2_SERVE_* env compat.
             APM injection dropped. Runs as managed instance of own binary (hidden subcommand).
  web.rs             ← was lib/HttpInterface.js
      Action: rewrite
      Notes: GET / payload shape kept; 127.0.0.1 default, --with-env opt-in, bearer token opt.
  completion.rs      ← was lib/completion.js/.sh (vendored tabtab 2015)
      Action: replace-with-crate (clap_complete)
      Notes: all shells static; dynamic proc-name completion via short-timeout daemon query,
             silent degrade. rc-file mutation dropped.
  mcp.rs             ← new module, no old equivalent              [MUST-HAVE #9]
      Action: write fresh
      Notes: MCP server over stdio (rmcp — official Rust MCP SDK), spawned as hidden/documented
             subcommand; agents connect via `command: "<bin>", args: ["mcp"]`. Tools: list
             processes, describe, host+proc metrics, tail logs, alert history. Read-only by
             default; start/stop/restart/reload tools only with --allow-control. Thin layer
             over shep-client — zero daemon changes needed. Decision 6: stdio ships v1
             (dev/debug), HTTP/SSE transport is a committed v1.1 feature.
  dogs/             ← new modules (decision #3: dog architecture; hidden `shep dog <name>`)
    metrics.rs       [MUST-HAVE #6]
      Action: write fresh
      Notes: shep-client consumer: polls monitoring RPC + host metrics, serves prometheus
             /metrics on 127.0.0.1 (port from [dog.metrics]). Reference Grafana dashboard
             JSON in assets/. OTLP export behind "otel" feature. Enabled: `shep enable metrics`.
    bark.rs          [MUST-HAVE #7]
      Action: write fresh
      Notes: bus subscriber → rule engine ([dog.bark] thresholds: crash, restart-loop,
             high-mem) → reqwest webhooks: Discord/Slack templates + generic JSON POST.
             Debounce/cooldown per rule. MUST handle bounded-bus drop notices + reconcile by
             polling — alerts never silently vanish.
  import.rs          ← new module (decision 7's one exception)
      Action: write fresh
      Notes: `shep import` — reads a box's existing pm2 state (dump.pm2, ecosystem
             .json/.yaml; .js configs via `node -p JSON.stringify(require(p))`) and emits a
             Flockfile + optional immediate start. Companion docs/migration.md guide.
             ALL pm2 format knowledge is confined to this module.
```

## Tests (workspace-level)

```
crates/*/src (co-located #[cfg(test)])   ← was test/programmatic subset (utility, config, kv, schema)
crates/shep-daemon/tests/                 ← was god/cluster/reload/signals/treekill mocha
    Notes: tokio::time::pause makes kill_timeout/backoff DETERMINISTIC — the suites pm2
           excluded from all CI become always-run. proptest on supervisor state machine.
crates/shep-daemon/examples/reuse_port_sheep.rs   ← new (Phase 6)
    Notes: the reload measurement's fixture, and the ONLY real child in the suite that is not
           `/bin/sh` running an inline script — `/bin/sh` cannot set a socket option. Binds
           `SHEEP_PORT_BASE + SHEP_INSTANCE` with SO_REUSEPORT and answers `<pid>\n`, so every
           answered connection is attributable to a process; `SHEEP_DEFIANT=1` makes it ignore
           SIGTERM, and THE GAP BETWEEN THE TWO RUNS IS THE FINDING. An `examples/` target
           because that is the only kind cargo builds for a plain `cargo test`, allows a
           dev-dependency (`nix`, which must not join the shipped daemon's graph) and never
           installs. Both platforms assert the weak property (the swap completes, the
           replacement owns the port, the drainee is reaped); the CONNECTION COUNT is asserted
           `#[cfg(target_os = "linux")]` only, because macOS hands every new connection to the
           last socket to bind and the loss cannot manifest there. The Linux half was run in a
           container, not inferred.
crates/shep-cli/tests/e2e/                ← was test/e2e/*.sh
    Notes: assert_cmd + tempfile PM2_HOME (parallel without docker) + serde asserts on jlist
           (grep-prettylist dies). Exit-code contract tests from right-exit-code.sh.
tests/compat/ (feature node-compat)      ← was test/fixtures + interpreter matrix
    Notes: fixtures verbatim + pure-binary fixtures so core suite runs Node-free.
           Bus wire shapes → insta golden snapshots (was test/interface).
CI: fmt+clippy+nextest × {ubuntu,macos,windows} × {stable,MSRV}; llvm-cov; docker runtime
    matrix (Node 18/20/24, Bun) for compat suite; cargo-dist release. Retry-stack (4 layers) dies.
```

## Bulk 1:1 crate swaps

| Old | New | Notes |
|---|---|---|
| commander 2.15 | clap v4 + clap_complete | derive; passthrough native |
| chokidar | notify + notify-debouncer-full + globset | |
| pidusage | sysinfo (+ procfs Linux hot path) | |
| @pm2/blessed | ratatui + crossterm | |
| cli-tableau | comfy-table | fitColumn bug class dies |
| ansis | owo-colors + anstream | NO_COLOR aware |
| async.js | async/await + futures combinators | disappears |
| eventemitter2 | tokio::sync::broadcast + typed enum | |
| croner (JS) | croner (Rust, same lineage) | five-field subset only; croner's own `L`/`W`/`#`/`?` rejected |
| dayjs | jiff/chrono + moment-token translator | log_date_format compat needs shim |
| debug | tracing + tracing-subscriber + EnvFilter | no `DEBUG=pm2:*` mapping and no `RUST_LOG` — `[daemon] log_level`/`SHEP_LOG_LEVEL` is the one knob (decision 7) |
| js-yaml | serde-saphyr | serde_yaml archived; serde_yml (its fork) swapped out for a pure-Rust parser that keeps `forbid(unsafe_code)` |
| semver (node ranges) | node-semver crate | `^ ~ \|\|` ≠ cargo semver |
| ws / proxy-agent / fast-json-patch / @pm2/js-api | — | die with SaaS agent |
| amp / amp-message | tokio_util LengthDelimitedCodec | |
| tools/which | which | |
| tools/open + xdg-open | open | |
| tools/prompt | dialoguer | |
| tools/passwd | uzers / nix | fixes macOS DS + LDAP |
| tools/isbinaryfile | content_inspector | interpreter='none' semantics kept |
| tools/json5 | json5 | |
| tools/copydirSync | fs_extra | |
| tools/treeify | termtree | |
| Math.random UUID | uuid v4 | crypto-strength |
| fclone | serde | Error→{name,message,stack} kept |

## Dropped (not in new codebase)

- ClusterMode.js, ProcessContainer{,Fork,Bun,ForkBun}.js, ProcessUtils injection — Node-injection architecture (contract preserved via IPC pipe + optional npm shim)
- modules/pm2-io-agent, API/pm2-plus/*, VersionCheck.js — SaaS/telemetry (replaced by native metrics.rs + alerts.rs)
- API/{Containerizer,Deploy}.js, ExtraMgmt/Docker.js, Extra.js barnacles (boilerplate/autoinstall/remote/inspect/profile)
- binaries/Runtime.js, bin/* shims, pm2.ps1, completion.sh machinery
- tools/{fmt,multimeter,charm,promise.min,IsAbsolute,deleteFolderRecursive,sexec}, packager/, pres/
- Monit.js (merged into tui.rs), .mocharc, bash test orchestration, dead test helpers
- Deferred (design ready, not v1): Version.js pull/backward/forward, Modules/* TAR redesign, deploy crate

## Design decisions (Rin ruled 2026-08-07)

1. **DECIDED: JSON frames v1.** rmp-serde stays a possible later feature; not planned.
2. **DECIDED**: fd-pipe protocol + probe-based readiness in v1; optional `@shep/io` npm shim v1.1; no Node-IPC emulation.
3. **DECIDED: dog architecture.** Module system permanently deleted. Metrics + bark ship as first-party **dogs**: shep-client consumers inside the multi-call binary (`shep dog metrics`), enabled via `shep enable <dog>` → daemon-config entry → supervised like any process (dog-tagged). Third-party extension = any binary speaking the client protocol. TUI/MCP stay client subcommands. See [decision-briefs.md](decision-briefs.md) #3b.
4. **DECIDED**: v1 polling behind `LimitEnforcer` trait; cgroup v2 (`enforce = "kernel"`) feature in v1.1.
5. **DECIDED**: name `shep`, license MIT OR Apache-2.0 (clean-room).
6. **DECIDED: MCP stdio in v1** (dev/debug use while building), **HTTP/SSE lands v1.1** as a real feature.
7. **DECIDED: no pm2 baggage.** Sheep-native surface: `SHEP_HOME`, `SHEP_*` env vars, Flockfile formats, own dump format, own CLI verbs (plain-English aliases per terminology.md stay — that's usability, not pm2 compat). The single exception: **a migration guide + `shep import`** — reads an existing box's pm2 state (dump.pm2, ecosystem files) and emits a Flockfile. pm2 formats live ONLY inside the importer.
