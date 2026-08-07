# Tests + CI

## Inventory

## Bucket: tests-ci — /Users/rin/GitHub/pm2

### Runners & infra
| File | LOC | Purpose | Tags |
|---|---|---|---|
| `.mocharc.js` | 12 | mocha config: bail, exit, timeout 10s, **retries: 2** (global flake mask), bdd | [STALE] 0 commits 2y |
| `test/unit.sh` | 115 | sequential local unit runner; ~40 mocha files; per-file retry-once + `pm2 kill` reset between files | [HOT] 8 commits. **BROKEN**: line 111 runs `test/interface/sysmetrics.mocha.js` — file lives in `test/programmatic/` → `npm run test:unit` fails at that line; unnoticed because CI never runs unit.sh |
| `test/e2e.sh` | 108 | sequential local e2e runner; `set -e`; ~55 scripts; IS_BUN guard skips 5 node-only scripts; several commented out | [HOT] 4 |
| `test/e2e/include.sh` | 118 | shared bash helpers: `spec`/`fail`/`success`; `should`/`shouldnot`/`exists` = sleep 0.3 → `pm2 prettylist` → grep -o pattern → count match; `runTest` retry-once wrapper. Grep-on-human-output is the core e2e assertion mechanism |
| `test/docker-parallel.sh` | 389 | **the actual CI runner** (`npm run test:parallel`). Builds image once, tars codebase, runs each test in isolated container (tmpfs `~/.pm2`, `--init`), MAX_JOBS parallel, bail on first failure. EXCLUDED_TESTS array = authoritative dead-in-CI list (30+ entries). Discovers tests via `find` (so new files auto-run) | [HOT] 7 |
| `test/Dockerfile` | 70 | ubuntu:22.04; installs curl/git/python3/php/bc/procps/gcc; node via nodesource (ARG NODE_VERSION) or bun via `curl\|bash` with **bun→node symlink** for shebangs; PM2_DISCRETE_MODE/PM2_SILENT/NODE_ENV=test | [HOT] 4 |
| `test/windows.sh` | 145 | Windows CI (Git Bash): unit subset (~35 files, excludes lazy_api, exp_backoff, signals.js — timing/signal-dependent) + smoke test (version/ls/start/online/kill). **Zero e2e on Windows** by design (bash semantics) | [HOT] 6 |
| `test/parallel.js` | 112 | legacy Node docker orchestrator, superseded by docker-parallel.sh, unreferenced by package.json/CI | dead |
| `test/helpers/apps.js` | 72 | forks **`lib/Satan.js`** — deleted years ago; zero test references | [STALE] 2016, dead |
| `test/helpers/plan.js` | 36 | assertion-count helper (`Plan(n, done)`); used by 7 mocha files | live |
| `test/pm2_check_dependencies.sh` | ~40 | checks php/nvm/node/python present; unreferenced | [STALE] 2018, dead |
| `test/benchmarks/` | 3 files | monit CPU sampling scripts + committed `result.monit`; unwired | dead |
| `.github/workflows/node.js.yml` | 44 | CI (see below) | [HOT] 17 commits |

### CI matrix (`.github/workflows/node.js.yml`, on: push + PR)
- **test-node**: ubuntu-latest × Node {18, 20, 24}, `NODE_VERSION=$v MAX_JOBS=6 npm run test:parallel` (Docker), 30 min timeout.
- **test-bun**: ubuntu-latest, Bun latest, `RUNTIME=bun MAX_JOBS=6 npm run test:parallel`; bun jobs additionally exclude source_map/wrapped-fork/log-json/inside-pm2/homogen-json-action/profiling/otel-install.
- **test-windows**: windows-latest × Node {18, 20, latest}, `bash test/windows.sh` (unit subset + smoke only).
- **No macOS job. No coverage. No lint gate. CI = docker-parallel.sh only** — unit.sh/e2e.sh are local-dev conveniences (and unit.sh is currently broken).

### Programmatic suite (mocha, `test/programmatic/`, ~60 files) — what each locks down
- `api.mocha.js` 340L: pm2 API object lifecycle — connect/start/stop by id, force-start duplicate, cluster -i 4, treekill flag persistence across restart, same-name multi-app, no-daemon mode, module startup, custom PM2_HOME instance.
- `api.backward.compatibility.mocha.js` 36L: legacy `require('pm2')` API surface stays callable.
- `lazy_api.mocha.js` 95L: auto-connect on first command (no explicit connect). [flaky: excluded on Windows]
- `programmatic.js` 422L: broadest API contract — start w/ cwd, wrong-file error, version/list/delete/dump/resurrect/ping/reload-one/describe/restart-by-name-and-id/stop, JSON start in cluster+fork mode.
- `client.mocha.js` 60L: Client daemon RPC methods. **[UNTESTED-in-CI: excluded]**
- `god.mocha.js` 307L: God internals — fork one process, state machine (stopped kept in DB, restart→online), cluster multi-launch by CPU count, dump, getMonitorData.
- `cluster.mocha.js` 255L: cluster restart/reload/gracefulReload of 4, scale up 8 / down 2 / no-op, listen_timeout honored + updateable, kill_timeout not-killed-before.
- `graceful.mocha.js` 217L: wait_ready + listen_timeout in fork and cluster; SIGINT sent right after ready (not waiting full listen timeout).
- `reload-locker.mocha.js` 73L: concurrent reload lock (reload during reload).
- `signals.js` 324L: **kill_timeout contract** — PM2_KILL_TIMEOUT env (1s/3s), per-app kill_timeout json overrides env (4s), SIGINT delivery, PM2_KILL_USE_MESSAGE (shutdown message instead of signal), delayed handlers, fork + cluster. **[UNTESTED-in-CI: Docker-excluded (timing) + Windows-excluded]**
- `treekill.mocha.js` 189L: kill full process tree incl. 3-level deep, SIGTERM-trapping children get SIGKILL, SIGINT-ignoring, dead/invalid pid, <500 ms budget.
- `auto_restart.mocha.js` 49L: restart on uncaughtException.
- `exp_backoff_restart_delay.mocha.js` 66L: exponential backoff restart delay growth + reset. **[UNTESTED-in-CI: Docker+Windows excluded — local unit.sh only]**
- `max_memory_limit.js` 90L: max_memory_restart (bytes + human units).
- `instances.mocha.js` 106L: `-i max`/bounds, NODE_APP_INSTANCE numbering.
- `id.mocha.js` 82L: pm_id uniqueness across start/delete cycles.
- `namespace.mocha.js` 151L: namespace start/restart/stop/delete grouping.
- `dump.mocha.js` + `misc_commands.js` + `resurect_state.mocha.js`: dump/backup rotation, resurrect fallback: broken dump→backup, missing dump→backup, both broken→none + cleanup; autosave on stop/start/delete.
- `env_switching.js` 143L: `--env production` picks `env_production` block.
- `filter_env.mocha.js` 81L: filter_env strips matching vars from child env.
- `logs.js` 279L: merge_logs, log timestamps (log_date_format), disable logs, /dev/null target.
- `flush.mocha.js` 80L: flush truncates logs. **[UNTESTED-in-CI: excluded]**
- `configuration.mocha.js` 250L: `pm2 set/get/unset` KV — nested keys (`a.b`, `a:b`), quoted values, multiset, sync variants.
- `internal_config.mocha.js`, `conf_update.mocha.js`, `module_configuration.mocha.js`: module config defaults/update. **[UNTESTED-in-CI: excluded]**
- `json_validation.mocha.js` 75L: schema.json coercion of process-file fields (string→num, bad values).
- `common.mocha.js` 43L: Common.getConfigFileCandidates / config detection order.
- `fclone.mocha.js` 137L: deep-clone util incl. circular refs, buffers, fn preservation.
- `containerizer.mocha.js` 146L: Dockerfile generation logic.
- `custom_action.mocha.js` 102L: `pm2 trigger` custom actions with/without params.
- `send_data_process.mocha.js` 102L: sendDataToProcessId round-trip.
- `inside.mocha.js` 86L: pm2 API calls from a process managed by pm2 (nested).
- `path_resolution.mocha.js` 47L: script paths resolved relative to config file cwd.
- `watcher.js` 200L: file-watch restart, ignore_watch, watch stop on delete.
- `modules.mocha.js` 58L / `module_tar.mocha.js` 225L (**excluded**) / `version.mocha.js` 69L (**excluded**): module install/uninstall npm + tarball, mono/multi-app modules, respawn survival.
- `http_interface.mocha.js` 128L: `pm2 web` JSON endpoint, CORS wildcard, WEB_STRIP_ENV_VARS.
- `sysmetrics.mocha.js` 189L: SysMetrics real syscalls — cpuUsage/ram/net/diskIO/fsUsage/collect.
- `sys_infos.mocha.js` 30L: requires **deleted `lib/Sysinfo/SystemInfo.js`** — cannot run, dead. [STALE] 2019
- `user_management.mocha.js` 60L: --uid/--gid spawn. **[UNTESTED-in-CI: excluded]**
- `otel_tracing.mocha.js` 218L + `otel_tracing_ws.mocha.js` 194L: fork-specific OTel — `--trace` injects tracer, phases across restart/daemon-kill/reconnect; WS server variant. CI-run via find (not in unit.sh).
- Regression locks (fork-era, all CI-run): `issue_5990` Bun interpreter substring match / determineExecMode; `issue_6073` `[object Object]` env leak; `issue_6075` **CVE-2025-5891 Config.js ReDoS**; `issue_6089` Configuration prototype pollution (set/setSync/unset/unsetSync); `issue_6106` Windows home path resolution; `issues/json_env_passing_4080` env via JSON.
- `flagExt.mocha.js` 48L: `--ext` watch extensions. **[UNTESTED-in-CI: excluded]**

### Interface suite (`test/interface/`, mocha.opts timeout 25s)
- `bus.spec.mocha.js` 183L (cluster) + `bus.fork.spec.mocha.js` 177L (fork): **IPC bus event contract** — `process:event` (online/exit w/ properties), `log:out`/`log:err`, `process:exception` (incl. promise rejection), `human:event` (process:msg). This is the wire contract pm2-io/keymetrics agents consume.
- `utility.mocha.js` 34L: Utility.getCanonicModuleName.

### E2E shell suite (`test/e2e/`, ~70 scripts; assertion = grep prettylist)
- **cli/** (30): `reload.sh` 164L (reload+SIGINT propagation), `start-app.sh` (env var printing, compiled output), `operate-regex.sh` (start/stop/restart by regex), `app-configuration.sh` (set/get/unset via CLI), `binary.sh` (shebang binaries, right interpreter), `startOrX.sh` (startOrRestart/startOrReload), `reset.sh` (restart_time reset, 1 and 5 instances), `env-refresh.sh` 94L (--update-env vs --skip-env, deploy env blocks), `extra-lang.sh`+`python-support.sh` (PHP/Python interpreter map, log output), `multiparam.sh`, `smart-start.sh` (start on existing = restart, state transitions), `args.sh` (script args passthrough), `attach.sh`, `serve.sh` 133L (static serve /, /index.html, SPA, port change), `monit.sh` (monitor/unmonitor flag by id/name/all), `cli-actions-1.sh` 257L (describe states, exit codes, pid files created/deleted, update, malformed json), `cli-actions-2.sh` 151L (start/stop/restart by script name, string-throw stack, log file naming), `dump.sh`+`resurrect.sh` (dump+backup, resurrect fallbacks), `watch.sh` 99L (watch flag lifecycle across stop/restart), `right-exit-code.sh` (error exit on unknown/no process), `fork.sh` (fork_mode, restart counters), `piped-config.sh` (`cat conf | pm2 start -`), `bun.sh` (TS fork+cluster under Bun; RUNTIME=bun only), `mjs.sh`, `sort.sh` (dead-in-CI), `ecosystem.e2e.sh` 8L (stub, dead-in-CI), `plus.sh` [STALE] 2018 dead-in-CI, `otel-install.sh` (auto-install OTel deps on --trace).
- **internals/** (11): `wait-ready-event.sh`, `daemon-paths-override.sh` (PM2_HOME env), `increment-var.sh` 123L (NODE_APP_INSTANCE hole-filling on delete/scale), `infinite-loop.sh` (killtoofast → errored + restart cap), `options-via-env.sh` (PM2_* env options), `start-consistency.sh` (JSON start == CLI start), `source_map.sh` (auto/forced source-map support), `wrapped-fork.sh` (fork wrapper output identical to raw node), dead-in-CI: `listen-timeout.sh`, `signal.sh`, `promise.sh`.
- **logs/** (9): custom date format, reload logs, entire-log 224L, /dev/null log, missing dir creation, namespace logs, json logs w/ timestamp (node-only), dead-in-CI: `log-timestamp.sh`.
- **misc/** (9): `misc.sh` 109L (no-autorestart, env define), `inside-pm2.sh` (env after inner restart), `instance-number.sh`, `nvm-node-version.sh` (per-app node version via nvm), `port-release.sh` (port freed after kill), dead-in-CI: `startup.sh` (upstart file gen/removal — needs init), `cron-system.sh` (cron restart, invalid pattern error), `versioning-cmd.sh`+`vizion.sh` (git backward/forward/pullAndReload).
- **process-file/** (7): json-file 116L, yaml (incl. malformed), json-reload (env change on re-start), app-config-update 72L (node_args add/remove, null deletes param), js-configuration, homogen-json-action (node-only), dead-in-CI: append-env-to-name.
- **binaries/**: `pm2-dev.sh`, `pm2-runtime.sh` (watch args, exit-when-no-app) — both dead-in-CI.
- **root**: `esmodule.sh` (.mjs + package-type module detection, both modes), dead-in-CI: `docker.sh` (Dockerfile gen, pm2-docker), `file-descriptor.sh` (fd leak check), `pull.sh` (git pull/backward/forward).
- **fixtures/** (~150 files): echo/http/cluster/signal-trap/throw/leak apps, ecosystems (json/json5/yaml/js), interpreter fixtures (.ts/.tsx/.coffee/.ls/.py/.php/.c), an embedded real git repo (`fixtures/git/`), source-map pairs, module tarball fixtures.

### Vendored module suites (run in CI via docker-parallel)
- `modules/pm2-axon/test/` (~35 files): req/rep, pub/sub, push/pull, emitter wildcards, HWM, queue — wire semantics of the axon socket layer.
- `modules/pm2-axon-rpc/test/` (2): RPC over TCP + unix socket.
- `modules/pm2-io-agent/test/units/` (6): InteractorClient/Daemon, PM2Client, PM2Interface, TransporterInterface, WatchDog.
- `modules/pm2-io-bpm/test/` (15 spec files): in-process metrics agent — api, autoExit, events, profiling (node-only), otel/tracing, eventloop/http/network/runtime/v8 metrics, actions/metrics services, standalone mode.

## Flags

- **unit.sh line 111 broken**: runs `test/interface/sysmetrics.mocha.js`; file is at `test/programmatic/sysmetrics.mocha.js` → `npm run test:unit` fails; masked because CI only runs docker-parallel. Symptom of local runners drifting from CI runner.
- **Dead test code**: `test/parallel.js` (superseded, unreferenced), `test/helpers/apps.js` (forks deleted `lib/Satan.js`, 2016), `test/pm2_check_dependencies.sh` (2018), `test/benchmarks/` (unwired, committed result file), `test/programmatic/sys_infos.mocha.js` (requires deleted `lib/Sysinfo/SystemInfo.js`), `test/e2e/cli/ecosystem.e2e.sh` (8-line stub, no assertions), `test/fixtures/ecosystem.json5` (nothing references it → lib/tools/json5.js parser has zero test coverage), `test/e2e/cli/plus.sh` (2018, keymetrics-era).
- **Embedded git repo as fixture**: `test/fixtures/git/` is a full .git directory (objects, hooks, refs) committed into the repo; only consumed by disabled versioning tests; will confuse tooling and licensing scans.
- **Security smells**: Dockerfile installs Bun via `curl | bash`; nodesource setup via piped gpg; e2e `include.sh` writes to fixed `/tmp/tmp_out.txt` (symlink/race on shared systems); `module.sh`/`module-safeguard.sh` install arbitrary live npm packages during CI.
- **Bun CI relies on `bun→node` symlink hack** (Dockerfile) so `#!/usr/bin/env node` shebangs resolve to Bun — a global environment lie; Rust rewrite should route interpreters explicitly instead.
- **Portability traps encoded in tests**: Windows exclusions (SIGINT not deliverable to JS handlers, timing tests flaky); e2e suite is bash-only by admission (windows.sh comment); `bc` dependency for arithmetic in runners; `sysctl -n hw.ncpu` vs `nproc` fork in docker-parallel; `declare -A` requires bash 4 (macOS ships bash 3.2 — docker-parallel.sh won't run on stock macOS shell).
- **Retry stacking** (mocharc retries:2 + three runner-level retries) systematically hides races — treat every currently-passing timing test as suspect when porting semantics.
- **CI-vs-local divergence**: CI truth is docker-parallel's find + EXCLUDED array; unit.sh/e2e.sh have their own drifting include lists (e.g. otel suites CI-run but absent from unit.sh; sort.sh has assertions but is dead-in-CI). Single source of truth needed in the rewrite.
- **AGPL-3.0**: test fixtures and vendored module tests (axon/io-agent/bpm) fall under the same license — porting their *semantics* to pm2-rs keeps the rewrite AGPL-encumbered unless a clean-room contract spec is written from this inventory instead of translating test code line-by-line.
