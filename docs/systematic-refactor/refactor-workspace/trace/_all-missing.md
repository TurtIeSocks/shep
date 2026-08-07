

---
# bucket:core
- **Kernel-enforced resource limits**: max_memory_restart is a polling check (pidusage loop elsewhere); modern PM uses cgroup v2 memory.max/cpu.max (Linux) — enforce, don't poll
- **Race-free process identity**: `kill(pid,0)` liveness + `_tree_pids` polling has pid-reuse ABA bug; pidfd (Linux), Job Objects (Windows), and owning-parent `waitpid` eliminate it
- **Real health checks**: only `wait_ready` IPC message + listen event exist; no HTTP/TCP/exec probes, no liveness vs readiness distinction, no configurable failure thresholds
- **Socket activation / fd passing for all runtimes**: zero-downtime reload is Node-cluster-only; fork-mode "reload" is just restart (downtime). systemd LISTEN_FDS-style protocol would generalize it
- **Restart policy gaps**: backoff is fixed ×1.5 cap 15s, no jitter, no per-exit-code restart policies (only stop_exit_codes blacklist), no "restart budget" windows (unstable_restarts heuristic `created_at < min_uptime*max_restarts` is opaque and resets oddly via created_at=null)
- **Built-in log rotation**: requires pm2-logrotate module; `_reloadLogs` reopen exists but no size/age policy in core
- **Durable state**: dump file written only on demand/kill, non-atomic (backup-file dance); crash between dumps loses topology; no journal/WAL of state transitions
- **Structured events**: bus events are ad-hoc JS objects; no stable schema, no persistent event log for post-mortem
- **Observability pull model**: metrics are push-based via pm2-io agent; no Prometheus/OpenMetrics endpoint
- **Privilege separation**: setuid/setgid exists but no sandboxing (namespaces, seccomp, rlimits per-app beyond uid)
- **Watcher hygiene**: no coalescing across instances of same app (N watchers × restartProcessName), no glob include lists, hand-rolled debounce
- **Windows graceful stop**: SIGINT default kill signal is a Unix concept; Node fakes it, taskkill /F is instant-kill — a rewrite needs GenerateConsoleCtrlEvent/named-pipe shutdown handshake for graceful Windows stops
- **Per-app clock**: kill_timeout/listen_timeout/GRACEFUL_TIMEOUT constants are a mix of env-var globals and per-app fields — normalize into one per-app timeout policy struct

---
# bucket:rpc
- No RPC deadline/timeout/cancellation anywhere — hung daemon hangs every client forever; killDaemon's 3s timer is the only deadline in the plane.
- No authn/authz beyond socket file perms; no `SO_PEERCRED`/`getpeereid` peer check; group-writable (775) socket grants full 30-method RPC = run-anything-as-daemon-user. Modern PM: peer-cred check + per-method policy.
- No protocol version handshake — client/daemon version skew handled by out-of-band `getVersion` + "pm2 update" ritual; a version field in a hello frame would prevent silent mismatch.
- Pub bus: broadcast-everything to every subscriber, client-side filtering only; no server-side subscription, no event replay/ring buffer (subscriber misses everything pre-connect), unbounded send queues (hwm=Infinity), silent drops when peer not writable.
- No structured error codes — errors travel as `message` string + JS stack; clients string-match.
- No readiness/liveness integration: no systemd socket activation, no `sd_notify`, pidfile+ping only.
- Windows: single global pipe namespace (no multi-user, no multi-PM2_HOME); no signal support path (kill-ack via SIGQUIT is unix-only, Windows falls back to ping-polling).
- Daemon self-observability: no metrics endpoint on the RPC plane (prometheus), no request logging/tracing ids.
- Crash recovery is `node $_ update` resurrection via shell env var — a real PM should have supervised-restart semantics (systemd unit generation exists elsewhere in pm2 but daemon itself is unsupervised by default).
- No concurrent-CLI safety story: multiple clients can race (reload.lock exists for reload only); no RPC-level locking/serialization guarantees exposed.
- First-run VersionCheck + KM agent auto-launch = surprise network calls; modern default is opt-in telemetry.

---
# bucket:apiCore
- **Structured machine output as first-class**: `jlist` dumps raw internal state (util.inspect when debug!), no stable JSON schema/versioning; modern PM would offer `--output json|yaml` with a documented schema on every command.
- **No pagination/filter server-side**: every listing/describe/log-file resolution pulls full `getMonitorData` (entire process table incl. full env) over RPC, then filters client-side — O(procs×env) per `pm2 ls`.
- **Log handling**: no built-in rotation (external logrotate module/root-only template), no size caps, no compression, no since/until time filters on `pm2 logs`, tail heuristic can miss long lines; modern: structured log store, `--since 5m`, follow with backpressure.
- **Health/readiness**: only `wait_ready` on start; no liveness probes, no HTTP/TCP checks, no per-process health status in `ls`.
- **Exit codes & error taxonomy**: everything funnels to ERROR_EXIT=1 with printed strings; no typed error codes for scripting.
- **TUI**: two overlapping TUIs (monit legacy + dash), neither testable, no headless snapshot mode, fixed 200-line log buffer, no search/filter in dashboard, metadata pane crashes on out-of-range selection.
- **Watch/attach UX**: `pm2 ls --watch` is a clear-screen 900ms redraw loop; ratatui live view should subsume it.
- **Update-check**: `update` is dump/kill/restore of daemon with a 250ms sleep race; no version negotiation between CLI and daemon (mismatch only handled by full restart).
- **Concurrency limits hardcoded**: CONCURRENT_ACTIONS constants, magic override to 10 for delete, 1 for ≤2 ids — should be config.
- **No dry-run** for start/delete/scale from ecosystem files.
- **Secrets hygiene**: `describe`/env tables print full env (incl. secrets) unredacted; PM2_SERVE_BASIC_AUTH_PASSWORD flows through plain env.

---
# bucket:apiExtra
- Native Windows service support (pm2 startup covers 8 unix init systems, zero Windows; `windows-service` crate makes this cheap in Rust).
- Enforced resource limits: cgroup v2 memory/cpu caps, not just observe-and-restart (`max_memory_restart` polls; a cgroup limit is exact and race-free).
- Health checks: HTTP/TCP/exec probes with configurable liveness/readiness (pm2 only has `wait_ready` process.send, Node-only).
- Built-in log rotation (pm2 requires the pm2-logrotate module — the plus flow auto-installs it, admitting the gap).
- Native metrics endpoint (Prometheus/OTLP) instead of SaaS-only monitoring via pm2.io agent.
- Signed/verified module & config supply chain — current install paths are unauthenticated wget-over-http + npm with zero checksum/signature verification.
- Authenticated daemon RPC — pm2's UDS/axon has no authorization; anything with socket access controls all procs.
- Graceful reload for arbitrary binaries (SO_REUSEPORT / socket-activation handoff) — pm2's zero-downtime reload only truly works for Node cluster mode.
- Inter-app dependency ordering (depends_on / after) for startup and resurrect.
- Declarative reconcile: apply ecosystem diff (add/remove/change detection) instead of imperative start/restart per app.
- Atomic, versioned state snapshots (dump file is manual backup-file juggling; no history).
- First-class secrets handling (Serve basic-auth creds and env config land in process env, visible via `pm2 env`).
- Structured audit/event log of manager actions.

---
# bucket:cli
- No stable machine-readable output contract: `jlist` dumps raw internal state, no versioned JSON schema, no `--json` global flag on normal commands (systemctl/docker both have it). Rust: serde-versioned output structs, `--format json|table`.
- No exit-code documentation/contract; codes inconsistent (1 generic, 2 only for runtime auto-exit). Rust: enumerated exit codes.
- Completion: bash/zsh only, spawns full Node process per TAB keystroke, mutates user dotfiles to install. clap_complete gives all shells statically for free.
- Global-options-on-everything: `pm2 ls --max-memory-restart 100M` parses fine and silently does nothing — no per-command validation. Clap subcommand scoping fixes.
- No `--dry-run` / config validation command for ecosystem files before applying.
- No native systemd integration in runtime mode (Type=notify/sd_notify readiness, socket activation, watchdog) — pm2's --wait-ready is app→pm2 only, never propagated to init system.
- PID-1 correctness: pm2-runtime is advertised for containers but does no explicit zombie reaping of re-parented orphans; Rust rewrite should do proper subreaper/reap loop.
- Version drift handled by warning text + manual `pm2 update`; daemon/CLI protocol has no version negotiation.
- Dead/vestigial surface still shipped: `imonit`, `deepUpdate`, `--v1` module install, `conf` (broken), `create` (mislabeled), Runtime.js, 4 duplicate describe commands, 4 duplicate list commands — Rust should collapse to aliases and hide deprecated ones from help.
- Secrets as CLI flags (`serve --basic-auth-password`, `link [secret]`, `--secret/--public`) — no env-var/file alternative; leaks via ps and shell history.
- No structured/leveled logging of pm2's own CLI output; silent mode is env-var + flag soup (PM2_DISCRETE_MODE, PM2_SILENT, -s).
- No first-class healthcheck probes (HTTP/TCP/exec) driving restart decisions — only memory ceiling + exit codes.
- `pm2-dev` watch loop lacks debounce config exposure and ignores only comma-list + node_modules; modern watchers do glob + gitignore semantics.

---
# bucket:aux
- **No authenticated/secure HTTP API**: pm2 web is read-only, unauthenticated, CORS *, 0.0.0.0. Modern PM: local unix-socket or token-auth REST/gRPC with full control ops (start/stop/scale) + TLS option.
- **No Prometheus/OpenMetrics endpoint**: SysMetrics + per-proc monit data locked in bespoke axm_monitor format; `/metrics` exporter is table stakes now.
- **Config formats**: no TOML; no env-var interpolation in ecosystem files; no `pm2 config validate`/`check` dry-run; validation errors lack file/line context; JSON "parsing" is actually JS eval (accepts non-JSON, executes code).
- **KV store has no locking, no watch/subscribe** — concurrent `pm2 set` races silently clobber.
- **Update check is silent phone-home telemetry** (os, uptime, node version, docker flag, daily) with env-only opt-out; modern default is opt-in, explicit payload disclosure.
- **Memory-limit enforcement is a 30s poll** — misses fast OOM spikes; Linux modern: cgroup v2 memory.high/memory.events or PSI-driven, event-based.
- **Watcher drops changes during restart** (restarting flag swallows events, no trailing re-check); no glob pattern support in ignore, only regex/paths.
- **No structured (JSON) daemon logging**; console monkeypatching for timestamps instead of a logging framework.
- **Host metrics Windows-blind** (SysMetrics Linux/macOS only) — sysinfo crate fixes free.
- **Cron restart has no timezone option surfaced** and no "next run" introspection in `pm2 ls`.
- **No config-file hot-diff** — `pm2 startOrReload ecosystem` re-verifies everything but the normalization layer can't report *what* changed.
- **Non-crypto UUIDs** (Math.random) for anything identity-ish.

---
# bucket:eco
- Metrics are SaaS-captive: no Prometheus `/metrics` or OTLP export from the daemon; a Rust rewrite should expose process cpu/mem/restarts/event-loop-agnostic stats natively (metrics-rs + exporter) instead of the axm→pm2.io pipeline.
- No real health checks: only `wait_ready` (process.send('ready')) and listen_timeout; modern PM needs HTTP/TCP/exec liveness+readiness probes driving restarts.
- systemd integration is bolt-on (Type=forking + `pm2 resurrect`); should be `Type=notify` with sd_notify, socket activation, and journald-native logging.
- Resource limits are reactive polling (pidusage sample → restart on max_memory_restart); no cgroup v2 enforcement (memory.max, cpu.max) on Linux.
- Log management: rotation is an external module (pm2-logrotate), no size/age caps in core, no structured JSON log output mode for the daemon's own logs.
- No authn on control plane: unix sockets rely on filesystem perms only; win32 named pipes (`\\.\pipe\rpc.sock`) are world-connectable by default — a rewrite needs peer-cred checks / token auth.
- Secrets hygiene: `dump.pm2` and `agent.json5` persist full env (incl. secrets) and SaaS secret key in cleartext without tightened file modes.
- Version-check/telemetry phones home by default (@pm2/pm2-version-check with docker detection); should be opt-in.
- Cron is restart-only (`cron_restart`); no one-shot scheduled jobs, no jitter, no backoff cap tuning beyond exp_backoff_restart_delay.
- No declarative reconcile: `pm2 start ecosystem` is imperative; drift between dump file and config file is a classic footgun (agent's WatchDog auto-dump every 5 min silently overwrites saved state).
- No canary/percentage rollout; reload is all-instances rolling only, cluster mode is Node-specific (SO_REUSEPORT in Rust generalizes it to any language).
- Windows support is second-class (named-pipe TODO in agent constants, no service integration, signal semantics); Rust can do a proper Windows service + Job Objects.
- Trace/profiling features depend on in-process JS agent; language-agnostic alternative (eBPF/perf integration or just delegating to OTel) is absent for non-Node apps despite pm2 advertising php/python/ruby support.

---
# bucket:tests
- No macOS CI at all — primary dev platform here is darwin; launchd startup path never exercised anywhere.
- Windows CI = unit subset + smoke only; zero e2e; signals/kill-timeout/backoff excluded → Windows process-kill semantics entirely unverified.
- `signals.js` + `exp_backoff_restart_delay` excluded from Linux Docker CI too ("timing-dependent") → the kill_timeout and backoff contracts have ZERO CI coverage on any platform; Rust should make them deterministic (paused clocks), not excluded.
- Four stacked retry layers (mocharc retries:2, unit.sh file-retry, include.sh runTest retry, docker-parallel container retry) — flakiness masked up to ~6x, no flake tracking/quarantine; a test can fail >80% of runs and still pass CI.
- No coverage measurement or reporting anywhere in the pipeline.
- No lint/typecheck/static-analysis CI gate.
- Assertions grep human-readable `prettylist` output — output format is silently load-bearing; any UX change breaks tests, and count-matching (`grep -o | wc -l`) can't distinguish which process matched. Rust: assert on jlist JSON.
- Sleep-based synchronization (0.1–1.5 s) throughout e2e — slow and racy; no event-driven wait primitive.
- ~10 unit suites + ~15 e2e scripts permanently dead-in-CI (EXCLUDED list) with no tracking issue per exclusion — features like flush, module tarball, uid/gid, pm2-dev, pm2-runtime, versioning, cron, startup ship untested.
- No upgrade-path test (`pm2 update` daemon hot-swap from previous version) — the riskiest real-world operation.
- No fuzzing of config/process-file parsers despite two real vulns locked by regression tests (CVE-2025-5891 ReDoS, prototype pollution) — Rust: cargo-fuzz targets on ecosystem/json5/yaml parse.
- Hermeticity: module.sh installs live npm packages; nvm-node-version.sh needs nvm; extra-lang needs php/python — network+toolchain flakes baked in; no offline mode.
- No performance/regression benchmarks in CI (benchmarks/ dir is dead); a Rust rewrite should land criterion benches for start/reload/treekill latency.
- No structured test-result artifacts (JUnit XML) — failures only readable as raw logs.
- No property-based tests for state machine (God transitions) — natural proptest target in Rust.