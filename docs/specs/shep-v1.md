# shep v1 — Product & Behavior Specification

Status: draft for Rin's review · 2026-08-07
Inputs: [map.md](../systematic-refactor/refactor-workspace/map.md) (module map),
[goals.md](../systematic-refactor/refactor-workspace/goals.md),
[decision-briefs.md](../systematic-refactor/refactor-workspace/decision-briefs.md),
[terminology.md](../terminology.md), [idiomatic-rust.md](../idiomatic-rust.md).
This spec is the behavior contract; map.md stays the module-level design. Where
they disagree, this spec wins (and map.md gets fixed).

## 1. Product

shep is a general-purpose process manager: a single Rust binary whose daemon
(the shepherd) supervises long-running processes (the flock) with restart
policies, log capture, zero-downtime reload, file watching, native
observability (Prometheus, webhooks, TUI, MCP), and first-party plugins
(dogs). Clean-room build inspired by pm2's feature list; MIT OR Apache-2.0.

**Non-goals (v1):** container orchestration; multi-host anything; a
third-party module registry (the dog contract replaces it); pm2 artifact
compatibility (formats live only in `shep import`); deployment tooling;
in-process Node instrumentation.

## 2. Versioned scope

**v1.0 ships:** daemon + supervision (fork model, N instances), full CLI,
Flockfile config, daemon config (`shep.toml`, file < env < flags layering),
JSON wire protocol over UDS/named-pipe, fd-pipe readiness + HTTP/TCP/exec
probes, custom actions (`trigger` over the shepherd channel), log
capture/tail/flush/reopen, watch-restart, cron-restart, memory-limit polling
behind `LimitEnforcer`, SO_REUSEPORT reload for all runtimes, dogs
infrastructure + metrics dog + bark dog, lookout TUI, whistle MCP (stdio),
`shep import` + migration guide, startup scripts (systemd Type=notify,
launchd, openrc, rc.d), Windows functional tier (named pipes + Job Objects;
start/stop/list/logs work).

**v1.1 committed (design now, build later):** HTTP/SSE MCP transport; cgroup
v2 enforcement (`enforce = "kernel"`); `@shep/io` npm shim (on demand);
Windows polish (service integration, ctrl-event graceful stop, full e2e);
vcs metadata (git revision shown in describe — `vcs` feature, off by
default); `shep web` JSON status endpoint (if demand — metrics dog covers
observability).

**v1.2 candidate:** fd-passing/LISTEN_FDS true cluster parity (Rin: "v1.1 or
v1.2 even").

## 3. Architecture

Four crates, one distributed binary (`shep`); see map.md for module detail.

- `shep-core` — types, config, paths, selectors, errors, wire protocol.
- `shep-daemon` — supervisor lib: registry actor, spawn/kill/reload state
  machines, watcher, workers, RPC server, bus, dog support. No binary.
- `shep-client` — async client: connect-or-spawn, typed RPC, event streams.
  Re-exports shep-core.
- `shep-cli` — the `shep` binary: clap surface, output/UX, lookout, whistle,
  serve, dogs, import, runtime/dev modes. Hidden `daemon` subcommand is the
  daemonization target (re-exec self, detached, readiness handshake over a
  pipe: child reports `{pid, version}` once the socket is bound). `[[bin]]`
  aliases `shep-runtime` and `shep-dev` ship for container entrypoints
  (argv[0] dispatch to the same subcommands).

Runtime layout: `$SHEP_HOME` (default `~/.shep/`) holding `shep.toml` (daemon
config), `flock.json` (state snapshot), `logs/`, `pids/`, `run/` (sockets,
0700).

## 4. Process model & lifecycle semantics

One spawn path: `tokio::process::Command`, own process group, optional
uid/gid, piped stdio + one extra pipe fd (the shepherd channel). "Cluster"
= N instances of the same app, each with `SHEP_INSTANCE` slot id (lowest free
slot among same-name; `increment_var` supported). Processes a sheep spawns
are its **lambs** (the process-tree members): shown in `describe`'s tree
view, killed with the sheep by the process-group/Job-Object tree kill.

**States:** `starting → online → stopping → stopped`, plus `errored`
(restart budget exhausted or spawn failure) and `waiting-restart` (backoff
delay pending). Serialized exactly as these strings on the wire.

**Restart policy (per app):**
- `autorestart` (default true): restart on unexpected exit.
- `stop_exit_codes`: exit codes treated as clean stop, no restart.
- Restart budget: an exit within `min_uptime` (default 1000ms) counts as
  unstable; the unstable counter increments per consecutive unstable exit
  (no time window) and resets after any stable run (uptime ≥ min_uptime) or
  manual restart/reload. Counter reaching `max_restarts` (default 16) →
  `errored`.
- `exp_backoff_restart_delay` (opt-in, initial delay ms): delay grows ×1.5
  per consecutive unstable restart, capped at 15000ms; resets after a stable
  run (uptime ≥ min_uptime) or manual action.
- `restart_delay`: fixed alternative to backoff.

**Stop/kill ladder:** send stop signal (`kill_signal`, default SIGTERM;
`shutdown_with_message` sends `{"kind":"shutdown"}` on the shepherd channel
instead) → wait `kill_timeout` (default 1600ms) on `child.wait()` → SIGKILL
process group → reap. Tree kill = `kill(-pgid)`; Windows = Job Object
terminate. Exit code + signal recorded exactly (owning-parent `waitpid`).

> Deviation from pm2 (deliberate): default stop signal is SIGTERM, not
> SIGINT — SIGTERM is the unix convention; `kill_signal` covers the rest.

**Reload (zero-downtime, any runtime):** state machine per instance:
`SpawnNew → AwaitReady → DrainOld → ReapOld`. AwaitReady = readiness signal
(§7) or `listen_timeout` (default 3000ms). DrainOld = stop ladder with
`graceful_timeout` (default 8000ms) cap. Socket sharing via SO_REUSEPORT
(`reuse_port = true` app option); without it, reload degrades to
rolling-restart (documented, one instance at a time). Reload proceeds
instance-by-instance; failure of the new instance aborts the rest and keeps
old instances running.

**Watch:** `notify` + debounce (`watch_delay`, default 500ms per trace
semantics), one watcher per app name-group, default ignores (dot-entries,
node_modules, log/pid dirs), `ignore_watch` + `watch_options` globs via
globset. Events during an in-flight restart are re-checked after it
completes.

**Cron restart:** `cron_restart` pattern (croner dialect), timezone option.

**Memory limit:** `max_memory` (MemSize, grammar `^\d+(G|M|K)?$`) — polling
enforcer, 15s cadence, restart on breach, via `LimitEnforcer` trait (cgroup
impl lands v1.1).

## 5. Configuration

**Flockfile** (per-app config): TOML preferred, YAML + JSON + JSON5 accepted;
`.js`
config via `node -p 'JSON.stringify(require(p))'` (requires node on PATH,
documented). Discovery order in cwd: `Flockfile.{toml,yaml,yml,json,json5}`
then lowercase `flockfile.*` in the same order. Schema = `AppConfig` in shep-core
(schemars-exported JSON schema ships in assets for editor completion). Field
set per map.md app_spec; sheep-native names, no pm2 aliases.

**Daemon config** `$SHEP_HOME/shep.toml`: `[daemon]` (log policy, socket
overrides), `[dog.<name>]` sections (each dog's typed config), alert rules
under `[dog.bark]`. Layering: file < `SHEP_*` env < CLI flags.

**Folds** (grouping): optional `fold = "<name>"` field in AppConfig assigns a
sheep to a named group. `shep fold <name>` lists that fold; `fold:<name>`
selects it in any verb; `flock`/`describe` display fold membership.

**KV store** (`shep set/get/unset`): retained for ad-hoc + dog runtime
tweaks; file-locked JSON; not the primary config path.

## 6. Wire protocol (v1 = protocol version 1)

- Transport: UDS at `$SHEP_HOME/run/shep.sock` (unix, dir 0700); per-user
  named pipe (Windows).
- Framing: u32 length prefix + JSON (`LengthDelimitedCodec`).
- Handshake: client `Hello{client_version, protocol}` → daemon
  `HelloAck{daemon_version, protocol, pid}`. Version skew = typed error, not
  silence.
- RPC: `Envelope{id: u64, deadline_ms: Option<u64>, body: Request}` →
  `Reply{id, result: Result<Response, RpcError{code, message}>}`. Default
  client deadline 5s. Requests/responses are typed enum pairs (IR-13).
- Events: client sends `Subscribe{topics: Vec<Glob>}`; daemon filters
  server-side; bounded per-subscriber queue, drop-oldest, `Dropped{count}`
  notice event. Topics: `process.*` (lifecycle), `log.out`, `log.err`,
  `channel.*` (shepherd-channel messages), `daemon.*`.
- Stability: every frame type has committed byte fixtures + insta snapshots
  (IR-35); breaking change = protocol version bump + CHANGELOG.
- Auth: socket permissions + SO_PEERCRED/getpeereid check (same-uid only by
  default).
- Stale socket recovery: bind EADDRINUSE → probe-connect → unlink if refused
  (load-bearing for the reboot-resurrect scenario in §13.4).
- Client reconnect: backoff 100ms ×1.5, cap 5s.

## 7. Readiness & health

- **Shepherd channel** (extra pipe fd, newline JSON, language-agnostic):
  child→daemon `{"kind":"ready"}`, `{"kind":"metric",...}`,
  `{"kind":"action-reply",...}`; daemon→child `{"kind":"shutdown"}`,
  `{"kind":"action",...}`. Fd number exported as `SHEP_CHANNEL_FD`.
- **Probes** (per app, optional): `readiness_probe` / `liveness_probe` =
  HTTP GET / TCP connect / exec, with interval, timeout, failure threshold.
  Readiness gates reload; liveness failures trigger the restart policy.
- `wait_ready = true` selects channel-based readiness; probe config selects
  probe-based; neither → "ready" = spawn success + listen_timeout heuristic.

## 8. Dogs (plugins)

Contract: a dog is a process speaking the client wire protocol,
supervised by the daemon, tagged `dog` (badged in `shep flock`, hidden by
default in user listings unless `--all`). First-party dogs live in the
multi-call binary as `shep dog <name>`. Lifecycle: `shep enable <name>` →
daemon config entry → autostart with the daemon; `shep disable <name>`
removes. Config: `[dog.<name>]` in shep.toml. Third-party dog = any binary
speaking the protocol, registered with `shep enable --exec <path> <name>`.

**metrics dog (v1):** serves Prometheus exposition on 127.0.0.1:9615
(configurable): per-sheep cpu/mem/restart_total/status/uptime, daemon self
metrics, host metrics. Reference Grafana dashboard JSON in `assets/grafana/`.
OTLP export = `otel` cargo feature.

**bark dog (v1):** subscribes `process.*` + polls state as reconciliation
(bus drops must not lose alerts). Named sinks in `[dog.bark.sinks]`:
Discord webhook, Slack webhook, generic JSON POST (templated body). Rules in
`[dog.bark.rules]`: event kinds, restart-loop detection, memory threshold;
per-rule debounce/cooldown; **each rule routes to one or more named sinks**
(per-event routing, must-have #7). Fired alerts append to
`$SHEP_HOME/barks.jsonl` (size-capped ring) — the data source for
`shep barks` and the whistle's `list_barks`.

Third-party extensions are treated as dogs once enabled: any binary
speaking the client wire protocol, registered via
`shep enable --exec <path> <name>`.

## 9. CLI surface (sheep-native)

Core verbs: `start` (script | Flockfile | `-` stdin JSON), `stop`, `restart`,
`reload`, `delete`, `scale`, `flock` (list; `list`/`ls` aliases), `describe`,
`bleats` (logs; `logs` alias), `flush`, `reopen` (reopen log files for
rotation; also SIGUSR2 to the daemon), `muster` (save + resurrect pair:
`shep muster save` / `shep muster` restores; `resurrect` hidden alias),
`signal`, `sendline`, `trigger <target> <action>` (custom actions via the
shepherd channel `action`/`action-reply` messages), `enable`/`disable`
(dogs), `dogs` (list dogs), `barks` (recent alert history), `fold <name>`
(list a fold), `lookout` (TUI; `dash` alias), `whistle` (MCP stdio), `serve`,
`startup`/`unstartup`, `set`/`get`/`unset`, `import`, `dev`, `runtime`,
`daemon` (hidden), `dog <name>` (hidden), `kill` (daemon shutdown),
`thatlldo` (easter-egg alias for graceful `stop`). Selectors everywhere:
name, id, `all`, `/regex/`, `fold:<name>`. Global `--format json|table`
(versioned serde output schema), clap_complete completions incl. dynamic
sheep-name completion (short-timeout daemon query, silent degrade).

**Exit codes.** Distinct causes get distinct codes; no error ever exits 0.

| Code | Name | Meaning |
|---|---|---|
| 0 | success | The command did what it was asked. |
| 1 | failure | An error with no more specific code. |
| 2 | usage | Bad arguments. clap's own convention. |
| 3 | not found | A selector matched no registered sheep. |
| 4 | invalid config | A Flockfile or daemon config failed validation. |
| 5 | daemon unreachable | No daemon answered, and none could be started. |
| 6 | protocol mismatch | Client and daemon speak different wire versions. |
| 7 | spawn failed | The daemon could not spawn a sheep. |
| 8 | deadline exceeded | The request outlived its deadline. |
| 9 | internal | An unexpected daemon-side failure. |
| 10 | daemon already running | Another daemon already holds this `$SHEP_HOME`. |

Code 10 is a contract across a process boundary, not merely a CLI detail: a
CLI that loses the race to start a daemon learns it only from the exit
status of the child it spawned, so `shep daemon` must exit 10 on that path
and the client must read 10 as "another daemon won — keep probing", never
as a failure.

Code 2 is claimed by clap for usage errors, which collides with the
fail-fast code `runtime` is specified to use below. `runtime` resolves that
when it is built.

**lookout (TUI):** ratatui; panes = flock table, bleats feed, sheep detail,
host usage; event-driven redraw; search/filter.

**whistle (MCP, stdio v1):** tools — `list_flock`, `describe_sheep`,
`get_metrics`, `tail_bleats`, `list_barks`; control tools (`start_sheep`,
`stop_sheep`, `restart_sheep`, `reload_sheep`) exist but require the daemon
flag `whistle.allow_control = true` (default false). rmcp SDK.

**import:** reads a box's pm2 state (`~/.pm2/dump.pm2`, ecosystem
json/yaml/js) → emits Flockfile + report of unmapped fields; `--start` to
adopt immediately. Companion `docs/migration.md`. All pm2 format knowledge
confined here.

**serve:** static file server as a managed sheep (axum + tower-http, SPA
fallback, dir listing, constant-time basic auth from creds file).

**dev:** isolated `$SHEP_HOME` (`~/.shep-dev`), forced watch, auto-exit.
**runtime:** foreground no-daemon mode for containers: PID-1 zombie reaping,
stdout log streaming, auto-exit when the flock is empty of online processes
(fail-fast exit code 2).

## 10. Security model

Premises (SECURITY.md, IR-42): runtime dir 0700, daemon unprivileged,
same-uid peer-cred RPC ⇒ no other local user can observe or control the
flock. Env values redacted by default in `describe`, RPC responses, and Debug
(`--with-env` opt-in); redacted Debug + exact-string tests for
secret-carrying types (IR-41). No telemetry, no phone-home; update check =
opt-in. Metrics and serve bind 127.0.0.1 by default. Root can always read
daemon memory (explicit non-goal).

## 11. Platform support

- **Tier 1 (v1 e2e green):** Linux (gnu + musl static artifact), macOS.
- **Windows v1 functional tier:** compiles + unit tests in CI from day one;
  named-pipe RPC + Job Objects kill; start/stop/flock/bleats work. Typed
  `StopSignal` keeps unix-isms out of core. Service + graceful ctrl-event +
  e2e = v1.1.
- Init integration: systemd unit generator uses `Type=notify` + sd_notify;
  launchd plist; openrc; freebsd/openbsd rc.d.

## 12. Testing & CI

Per idiomatic-rust.md IR-33..IR-45: paused-clock deterministic lifecycle
tests (backoff/kill timings asserted as pinned arrays), scripted fake
`ProcessRunner`, proptest on the supervisor state machine, wire byte-fixtures
+ insta snapshots, assert_cmd e2e with fresh `$SHEP_HOME` per test, runtime
compat matrix (Node/Bun/python fixtures) behind `node-compat` feature. CI:
fmt/clippy(pinned)/docs(-Dwarnings)/typos + {ubuntu, macos, windows} ×
{stable, MSRV} + minimal-versions + musl (tests run) + feature-combo ladder
+ llvm-cov coverage upload. Release: cargo-dist artifacts + crates.io
Trusted Publishing.

## 13. v1.0 definition of done

1. All §2 v1.0 features implemented and documented.
2. The four gates — `cargo fmt --check`, `cargo clippy -D warnings`,
   `cargo check`, `cargo test` — green on tier-1 platforms + Windows
   functional tier.
3. Wire fixtures committed; SECURITY.md, migration.md, Grafana asset shipped.
4. `shep import && shep muster save && reboot` → systemd unit runs
   `shep muster` and the flock survives on a Linux box (the flagship
   migration scenario).
5. Spec↔implementation drift review before tagging.

## 14. Assumptions (delegate-mode calls — flag any to change)

1. Default stop signal SIGTERM (pm2 used SIGINT) — unix convention wins.
2. Defaults carried from trace where unstated: min_uptime 1000ms,
   max_restarts 16, kill_timeout 1600ms, listen_timeout 3000ms,
   graceful_timeout 8000ms, backoff ×1.5 cap 15s. Memory poll tightened
   30s → 15s (cheap via sysinfo, halves worst-case breach latency).
3. Flockfile TOML-first (Rust ecosystem norm); YAML/JSON accepted; `.js`
   needs node present.
4. Metrics dog default port 9615 (pm2's old web port — familiar, unclaimed
   by IANA; trivially configurable).
5. `muster` = both save and restore (`muster save` / `muster`); `resurrect`
   kept as hidden alias.
6. Dogs hidden in default `shep flock` output (badged under `--all`).
7. Whistle control tools gated by daemon config, not CLI flag — config is
   auditable, flags are per-invocation.
8. `SHEP_INSTANCE` replaces `NODE_APP_INSTANCE` (sheep-native env; importer
   maps the old name into app env if the app needs it).
9. Probe engine scoped to readiness/liveness only in v1 — no per-probe
   custom actions.
10. serve/dev/runtime carried into v1 scope (map "keep" verdicts) — cut any
    of these if v1 should slim down.
11. Daemon idle-footprint goal (single-digit MB RSS) tracked via criterion
    benches + a CI-reported RSS number, not gated in the DoD.
12. vcs metadata deferred to v1.1; `shep web` deferred to v1.1-if-demand
    (metrics dog covers observability) — both were map modules without a
    ruled version.
