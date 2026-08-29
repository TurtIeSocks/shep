# Assessment — keep / toss verdicts

Consolidated from 9 trace agents. Full evidence per module: [trace/_all-verdicts.md](trace/_all-verdicts.md). "Keep semantics" = behavior ports to Rust; all *code* is rewritten regardless.

**Totals: 34 keep-semantics · 12 rewrite/replace-mechanism · 24 drop · 6 defer.**

## Keep semantics (the product)

| Module | What survives | Conf |
|---|---|---|
| God.js | prepare/executeApp/handleExit/injectVariables — restart brain, instance expansion, wait_ready | High |
| God/ForkMode.js | THE spawn path: interpreter resolution, env flattening, daemon-side log pipes, pid files, log reopen | High |
| God/Methods.js | kill ladder semantics, shutdown_with_message, registry ops, resetState | High |
| God/Reload.js | reload contract (new-up-before-old-dies, drain window, timeouts 3000/8000) — mechanism replaced | High |
| God/ActionMethods.js | all lifecycle RPC verbs; dispatch redesigned; watch-by-name bugs NOT ported | High |
| TreeKill.js | kill-whole-tree semantics — mechanism → process groups / Job Objects | High |
| Watcher.js | watch→debounce→restart, ignore defaults, watch_delay — chokidar→notify | High |
| Daemon.js | boot ritual, pidfile, signals, 30-method RPC surface, readiness handshake, graceful kill | High |
| Client.js | ping→auto-spawn→connect state machine ("first command boots daemon") | High |
| Event.js | `process:event` envelope wire shape | High |
| constants.js + paths.js | env-var surface + on-disk layout (dump.pm2, logs/, pids/, sockets, PM2_* overrides) | High |
| API.js | full command semantics: name/id/all/regex/namespace resolution, JSON-vs-script dispatch, env-merge, --update-env | High |
| API/Log.js + LogManagement.js | output formats, flush, reloadLogs, streaming | High |
| API/Dashboard.js | 4-pane `pm2 dash` UX → ratatui rewrite (absorbs Monit) | High |
| UX/pm2-ls + describe + minimal + helpers | table content contract, width-adaptive layouts, show fields | High |
| API/Startup.js | pm2 startup/save/resurrect — systemd/launchd/openrc/rc.d tiers only | High |
| API/Serve.js | static serve + SPA + auth + dir listing ([HOT], fork investment) | High |
| API/schema.json + interpreter.json | THE app-config compat contract — every key/alias/default honored | High |
| binaries/CLI.js | full command+flag surface = compat contract (internals rewritten via clap) | High |
| binaries/DevCLI.js | pm2 dev: isolated home, forced watch, post-exec hook → subcommand | High |
| binaries/Runtime4Docker.js | container mode: PID-1 foreground, stdout logs, auto-exit code 2 — exit-code contract exact | High |
| Common.js | ecosystem parsing + app normalization (aliases, defaults, env-merge) — minus JS-eval | High |
| Utility.js (subset) | extendMix null-delete protocol, canonic module name, append log streams | High |
| lib/Configuration.js | pm2 set/get/unset, dotted-key parse, module_conf.json format + advisory lock added | High |
| Worker.js | max_memory_restart poll, backoff reset, cron registry, metrics cadence | High |
| tools/Config.js | schema validation, camelCase aliases, sbyte/stime units → serde | High |
| templates/ + motd | init-script assets, placeholder substitution; + TOML/YAML ecosystem variants | High |
| modules/vizion | git metadata semantics (fork-hardened, shell-out with argv) — feature-gated | High |
| modules/pm2-io-bpm | IPC protocol ONLY (axm:monitor/action/reply, process:exception) — daemon parses, JS shim emits | High |
| types/index.d.ts | as spec checklist for AppConfig fields; artifact dropped | High |
| test/programmatic + interface + e2e | behavioral contract → Rust integration tests; bus specs → golden snapshots | High |
| test/fixtures/ | test corpus ports near-verbatim | High |
| index.js | programmatic-API entry contract (via client lib) | High |

## Rewrite / replace mechanism

| Module | Replacement | Conf |
|---|---|---|
| pm2-axon + pm2-axon-rpc | typed request/response enums, length-delimited JSON over UDS/named-pipe; keep stale-socket recovery + reconnect-backoff algorithms | High |
| HttpInterface.js | keep GET / shape; bind 127.0.0.1, strip env, auth — never port as-is | High |
| OtelManager.js | native opentelemetry crates; kill runtime-npm-install | Med |
| completion.js/.sh | clap_complete (all shells) + dynamic proc-name completer | High |
| tools/SysMetrics.js | sysinfo crate, keep axm_monitor output shape + metric names | High |
| tools/{which,open,prompt,passwd,isbinaryfile,json5,copydirSync,find-package-json,treeify,sexec} | 1:1 crate swaps (which/open/dialoguer/uzers/content_inspector/json5/fs_extra/loop/termtree/std Command) | High |
| packager/ | cargo-dist / nfpm; keep systemd-unit + system-user intent; Type=notify upgrade | High |
| test runners (mocha, bash orchestration, windows.sh) | cargo-nextest, deterministic clocks (tokio pause) — makes CI-excluded timing suites runnable | High |
| CI workflow | cargo fmt/clippy/nextest × {linux,macos,windows} × {stable,MSRV} + runtime-compat docker matrix | High |
| dump/resurrect write path | atomic tempfile+rename (kills backup-dance) | High |
| API/Version.js (if kept) | shell-out git with argv vectors (kills `cd p;cmd` injection) | Med |
| Reload mechanism | SO_REUSEPORT default / LISTEN_FDS protocol — extends 0-downtime to ALL runtimes (upgrade over pm2) | High |

## Drop

| Module | Why | Conf |
|---|---|---|
| God/ClusterMode.js | Node-cluster-specific; msg-relay contract survives in IPC design | High |
| ProcessContainer.js ×4 (+ProcessUtils injection) | in-process Node injection impossible from Rust; contract moves daemon-side + optional npm shim | High |
| modules/pm2-io-agent | pm2.io SaaS feed; blast radius 3 files/8 call sites; removes ws/proxy-agent/fast-json-patch deps | High |
| API/pm2-plus/* | SaaS coupling, hardcoded OAuth ids, plaintext password prompt, null-deref bugs | High |
| API/Containerizer.js | broken require = unused in practice; out of scope | High |
| API/Deploy.js | bash pm2-deploy wrapper; CI/CD superseded; ~30 lines glue | Med |
| API/ExtraMgmt/Docker.js | stale 2019 docker passthrough | Med |
| API/Monit.js | superseded by Dashboard → single ratatui TUI | High |
| Extra.js: boilerplate/autoinstall/remote/inspect/profile | Keymetrics + V8-runtime-specific | High |
| binaries/Runtime.js | dead since 2018, zero references | High |
| bin/* shims + pm2.ps1 | replaced by native [[bin]] targets | High |
| VersionCheck.js | silent daily telemetry — opt-in rewrite at most | High |
| modules/fclone | serde makes it structurally unnecessary; keep Error→{name,message,stack} shaping | High |
| tools/{fmt,multimeter,charm,promise.min,IsAbsolute,deleteFolderRecursive,xdg-open} | dead weight / stdlib | High |
| pres/ | marketing PNGs | High |
| LOCAL internal modules (v8-profiler etc.), flagExt.js | dead-upstream Node addons; globs replace flagExt | High |
| test/{helpers,parallel.js,benchmarks,pm2_check_dependencies.sh,sys_infos.mocha.js} + .mocharc | dead/unreferenced/superseded | High |
| schema.json APM knobs (trace/v8/deep_monitoring/pmx/io/...) | pm2.io-only | High |
| tx2 dep | used by examples only — dead in manifest | High |

## Defer (decide later, not v1)

| Module | Note |
|---|---|
| API/Version.js (pm2 pull/backward/forward) | niche; shell-out design ready if demanded |
| API/Modules/* | redesign around TAR concept; "support Node pm2 modules?" needs the maintainer's call |
| API/Configuration.js CLI | needed only if module system lands |
| examples/ | future e2e fixtures (interpreter matrix) |
| Vendored module tests | consult during protocol design, don't port |
| pm2-deploy semantics | separate crate if ever demanded |

## Cross-cutting structural verdicts

- Callback pyramids / async.js / EventEmitter2 wildcard bus / mixin monkey-patching / cb-or-exitCli dual-mode / method-name-string RPC dispatch: **none of it ports** — async/await, typed enums, `tokio::sync::broadcast`, `Result`.
- Node cluster mode → N fork instances + SO_REUSEPORT/FD-passing. One spawn path. Biggest architectural divergence, and an upgrade (reload works for every runtime, not just Node).
- Security posture must flip: authenticated-by-peer-cred RPC, 127.0.0.1 binds, redacted env output, atomic writes, real UUIDs, opt-in telemetry.
