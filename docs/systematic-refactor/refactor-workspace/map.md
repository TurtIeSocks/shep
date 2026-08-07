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
    ecosystem.rs     ← was lib/Common.js (parseConfig + file detection)
      Action: port + redesign
      Notes: serde_json strict (NOT JS-eval — kills code-exec-on-parse), json5, serde_yml.
             .config.js/.cjs/.mjs → spawn `node -p JSON.stringify(require(p))` (documented:
             JS configs need node on PATH). Extension match by endsWith (fixes substring bug).
    normalize.rs     ← was lib/Common.js (prepareAppConf/verifyConfs/mergeEnvironmentVariables)
      Action: port + redesign
      Notes: pure AppConfig → Result<ResolvedApp> functions (mutation+Error-or-value → typed).
             Alias/default/env-merge rules byte-compatible: cmd→script, fork_mode, bash -c
             on spaced scripts, log path defaults, filter_env. Cron validation via croner crate
             (same pattern dialect as JS croner — compat safe).
    daemon_config.rs ← new module, no old equivalent            [MUST-HAVE #8]
      Action: write fresh
      Notes: daemon-level config file (TOML): metrics on/off+port, webhook targets, alert
             thresholds, log policy. Layered: file < env < CLI flags (figment or hand-rolled).
             pm2 had nothing here — env-var soup only.
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
  spawn.rs           ← was lib/God/ForkMode.js + ClusterMode.js (unified — ONE spawn path)
      Action: port + merge
      Notes: tokio::process::Command, process_group(0), uid/gid via CommandExt, piped stdio +
             extra IPC pipe fd (newline-JSON — carries 'shutdown', log:reload, process:*, axm:*;
             keeps pm2-io shim compat). Cluster mode = N fork instances (Node cluster injection
             dies — see reload.rs for the load-balancing story). Log pipeline: BufReader lines →
             framing (raw/date-prefix/json) → broadcast + append files; /dev/null skip;
             reopen-on-reload kept for logrotate.
  kill.rs            ← was lib/God/Methods.js (kill ladder) + lib/TreeKill.js
      Action: port semantics + rewrite mechanism
      Notes: ladder exact: SIGINT(configurable)/shutdown-msg → timeout(kill_timeout 1600ms) on
             child.wait() → SIGKILL survivors → timeout error. Mechanism: owning-parent waitpid
             (exact exit code+signal, kills pid-reuse ABA race), kill(-pgid) for trees (replaces
             racy ps-snapshot walk), Job Objects on Windows (fixes taskkill /F signal-ignoring).
  reload.rs          ← was lib/God/Reload.js
      Action: port contract + rewrite mechanism                 [UPGRADE over pm2]
      Notes: explicit state machine SpawnNew → AwaitReady(ready-msg | listening | timeout 3000)
             → DrainOld(shutdown msg | SIGTERM, GRACEFUL_TIMEOUT 8000) → ReapOld. Zero-downtime
             for ALL runtimes: SO_REUSEPORT (socket2) default, LISTEN_FDS fd-passing protocol
             as the principled option — pm2 only had it for Node cluster.
  watcher.rs         ← was lib/Watcher.js
      Action: port + redesign
      Notes: notify + notify-debouncer-full; ONE watcher per name-group (fixes O(N²) fan-out);
             ignore defaults (dotfiles, node_modules) via globset; watch_delay = debounce dur;
             re-check after restart completes (fixes dropped-event gap). disableAll bug not ported.
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
      Notes: boot ritual kept (pidfile, both-sockets-bound readiness handshake via pipe,
             SIGTERM/INT/QUIT graceful dump+exit, SIGUSR2 reload logs). Per-conn task: read
             frame → dispatch → reply. Peer-cred check (SO_PEERCRED/getpeereid) — pm2 had
             NONE. Per-call deadlines default 5s. Drop: domain resurrection, $_ env hack,
             inspector self-profiling.
  bus.rs             ← was God.bus (EventEmitter2) + axon pub/sub
      Action: rewrite
      Notes: tokio::sync::broadcast<BusEvent>; wire side: subscribe-with-topic-globs on connect,
             server-side filtering (pm2: broadcast-everything). Bounded queue + drop-oldest +
             drop-count event (pm2: unbounded, silent).
  worker.rs          ← was lib/Worker.js
      Action: port
      Notes: tokio interval tasks: max_memory_restart poll, backoff reset, cron_restart registry
             (croner crate), host metrics cadence. domain → catch_unwind per tick.
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
      Notes: subscribe(topic globs) → stream of BusEvent.
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
  commands/*.rs      ← was lib/binaries/CLI.js command definitions + lib/API/Extra.js keepers
      Action: port surface
      Notes: every command+flag from the trace enum; global opts via #[arg(global)];
             `--` passthrough native (patchCommanderArg dies); -c dup resolved (cron-restart
             wins, --cron long alias); StartOptions struct = the camelCase→API contract,
             explicit + tested. Duplicate verbs collapse to clap aliases; dead surface
             (imonit, deepUpdate, --v1, conf, create) hidden or gone. stdin `-` JSON kept.
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
      Notes: LogFormat enum {Pretty,Raw,Json,Logfmt}; reverse block reader for tail (lines×200-
             bytes guess dies); flush truncate; printLogs/streamLogs 90-line copy-paste merged.
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
| croner (JS) | croner (Rust, same lineage) | pattern-compat |
| dayjs | jiff/chrono + moment-token translator | log_date_format compat needs shim |
| debug | tracing + EnvFilter (DEBUG=pm2:* mapped) | |
| js-yaml | serde_yml | serde_yaml archived |
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
