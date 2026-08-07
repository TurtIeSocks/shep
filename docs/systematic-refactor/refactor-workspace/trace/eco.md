# Vendored ecosystem + deps

## Inventory

## Bucket: ecosystem-deps (vendored ecosystem + packaging)

### modules/pm2-io-agent/ — vendored pm2.io SaaS agent (~3,900 LOC src) [UNTESTED-partially: has own mocha units via `npm run test:io-agent`]
Separate daemon process (`InteractorDaemon.js` spawned detached) that bridges local pm2 daemon <-> pm2.io/Keymetrics cloud over websocket.
- `index.js` (2 LOC): re-export of `src/InteractorClient.js`.
- `constants.js` (96): own PM2_HOME paths (`interactor.sock`, `agent.log`, `agent.pid`, `agent.json5`), KEYMETRICS_ROOT_URL=`https://root.keymetrics.io`, env overrides, win32 named-pipe remap (`\\.\pipe\...`) [TODO at L90], `IS_BUN` detection.
- `config.js` (30): transporter registry — websocket only, endpoint key `ws`.
- `src/InteractorClient.js` (513): CLI-side control. `ping` (axon req to interactor.sock), `killInteractorDaemon`, `launchRPC` (dynamic RPC method generation from `client.methods()`), `daemonize` (spawns `node InteractorDaemon.js` detached, env-passes PM2_SECRET_KEY/PM2_PUBLIC_KEY/MACHINE_NAME; 7s timeout; Bun path skips unref/IPC message wait), `launchOrAttach`, `update`, `getOrSetConf` (merges env > CLI > `agent.json5` FS conf; json5→JSON regex fixup; writes conf back), `disconnectRPC`, `launchAndInteract` (entry point used by lib/), `getInteractInfo`.
- `src/InteractorDaemon.js` (455): daemon main. Boot: retrieveConf from env → TransporterInterface.bind('websocket') → startRPC (axon rep on interactor.sock exposing `kill`/`getInfos`) → `_pingRoot` POST `/api/node/verifyPM2` with public+secret keys → on success connect WS endpoints, re-verify every 60s, start WatchDog after 30s, start Push+Reverse interactors. `node:domain` catch-all restarts agent on fatal. SIGUSR1/2 → inspector cpu/heap profiling of the agent itself.
- `src/PM2Client.js` (112): axon sub-emitter on pub.sock + req/RPC on rpc.sock to pm2 daemon; dynamic RPC method generation; `remote()` dispatches to PM2Interface.
- `src/PM2Interface.js` (180): higher-level ops over daemon RPC: `getProcessByName`, `scale` (+N/-N/absolute), `dump` (writes dump.pm2), `restart/reload/reset/ping` via `_callWithProcessId`.
- `src/WatchDog.js` (70): sets `PM2_AGENT_ONLINE` at require time; on pm2 RPC `reconnecting` x6 → `child.exec('node <pm2> resurrect')` (agent resurrects pm2 = mutual supervision); autoDump every 5 min.
- `src/TransporterInterface.js` (173): EventEmitter2 wildcard fan-out over N transporters (only websocket exists); endpoint diffing/reconnect.
- `src/transporters/Transporter.js` (129): base class; reconnect loop with DNS-failure backoff.
- `src/transporters/WebsocketTransport.js` (204): `ws` client; auth via headers X-KM-PUBLIC/X-KM-SECRET; 5s ping heartbeat; offline queue (200 packets, status/monitoring dropped); opt-in RFC6902 status diffing via `fast-json-patch` (`WS_JSON_PATCH`); 10s queue flush.
- `src/push/PushInteractor.js` (206): pm2 bus `*` listener → normalize → `transport.send`. 1s `getMonitorData` status worker; log buffering (8 lines/proc); exception context attach via StackTraceParser; profile file upload (`heapdump`/`cpuprofile` read+unlink+send); `axm:trace` → aggregator.
- `src/push/DataRetriever.js` (77): status payload shape — per-proc (pid/name/cpu/mem/versioning/axm_*) + server metadata (username, hostname, loadavg, total_mem, node_version).
- `src/push/TransactionAggregator.js` (670) [COMPLEX]: APM trace aggregation — routes/variances via Levenshtein-ish path matching, histograms (Histogram/EDS/BinaryHeap utils), median latency, flush to KM every 30s/60s.
- `src/reverse/ReverseInteractor.js` (128): cloud→local remote control. `trigger:action`/`trigger:scoped_action` → msgProcess IPC to app; `trigger:pm2:action` → allowlist (`restart,reload,reset,scale,startLogging,stopLogging,ping,launchSysMonitoring,deepUpdate`) → PM2Interface. startLogging streams all logs for 120s.
- `src/Utility.js` (395): TTL cache, StackTraceParser (FS source context around callsite), EWMA, `Cipher` (aes256 via **deprecated `crypto.createCipher`** — dead within repo), HTTPClient (raw http/https + proxy-agent), network IP detect, fclone serialize.
- `src/utils/`: BinaryHeap(135)/EDS(111)/units(10)/probes/Histogram(204) — stats plumbing for aggregator.
- `test/`: mocha units + websocket integration mocks — decent coverage, wired to `npm run test:io-agent`.

### modules/pm2-io-bpm/ — vendored in-process APM (fork of @pm2/io, ~2,900 LOC) [tested via `npm run test:bpm`]
Injected into EVERY managed app: `lib/ProcessUtils.injectModules()` called from all 4 container entrypoints (ProcessContainer, ProcessContainerBun, ProcessContainerFork, ProcessContainerForkBun) unless `process.env.pmx === 'false'`. Runs inside the user's Node/Bun process, talks to daemon via `process.send` IPC (`axm:*` packets).
- `index.js` (11): global singleton via `Symbol.for('@pm2/io')`.
- `pmx.js` (380): PMX facade — `init/destroy`, metric constructors (counter/gauge/histogram/meter), `action()`, `notifyError`, `onExit`, `emit` (deprecated), `getTracer`, `initModule` (pm2 module bootstrap w/ keymetrics widget conf), express/koa error handlers. Standalone mode autodetect via PM2_SECRET_KEY+PM2_PUBLIC_KEY+PM2_APP_NAME env.
- `featureManager.js` (110): registry — notify, profiler, events, metrics, tracing, dependencies.
- `serviceManager.js` (21): global Map service locator.
- `configuration.js` (141): sends `axm:option:configuration` process metadata to daemon.
- `transports/IPCTransport.js` (115): `process.send` wrapper; autoExitHook polls `process._getActiveHandles()` (private API) every 3s to self-detach IPC listener so app can exit naturally.
- `services/transport.js` (11): `createTransport(name)` **ignores `name`, always returns IPCTransport** — standalone/websocket mode is dead code.
- `services/actions.js` (159): custom + scoped action registry; replies `axm:reply`/`axm:scoped_action:*`.
- `services/metrics.js` (186): metric registry, 990ms flush of `axm:monitor`.
- `services/inspector.js` (43): shared `node:inspector` session (disabled on Bun).
- `services/runtimeStats.js` (54): optional native `@pm2/node-runtime-stats` (GC stats) if user installed it.
- `features/notify.js` (238): uncaughtException/unhandledRejection hooks → `process:exception` + source context; express/koa middlewares; exits(1) if sole listener.
- `features/metrics.js` (102): orchestrates metrics/{v8,runtime,network,httpMetrics,eventLoopMetrics}.
- `features/tracing.js` (174): OpenTelemetry NodeSDK + auto-instrumentations (peer deps, lazy require) + vendored custom Zipkin exporter (`otel/custom-zipkin-exporter/`) shipping spans as `axm:trace`.
- `features/profiling.js` (70) + `profilers/inspectorProfiler.js` (302) / `addonProfiler.js` (193): heap/cpu profiles via inspector (or optional `v8-profiler-node8` addon), registered as `km:heapdump` etc. actions; Bun → addon path only (effectively unavailable).
- `features/events.js` (50), `features/dependencies.js` (48) (sends package deps list), `metrics/httpMetrics.js` (175) monkey-patches `http(s)` via shimmer for latency/throughput p50/p95/p99; `metrics/network.js` (132) patches `net.Socket` for byte counters; `metrics/v8.js` (104), `metrics/runtime.js` (161), `metrics/eventLoopMetrics.js` (136).
- `utils/`: shimmer, stackParser, EWMA, EDS, BinaryHeap, units, autocast, module-detect, miscellaneous, metrics/{counter,gauge,histogram,meter}; `utils/transactionAggregator.js` (429) — **unreferenced except via features/notify? actually only stackParser imported there; aggregator appears dead in bpm**.

### modules/vizion/ (~300 LOC) — git metadata for the "versioning" feature [recently rewritten in this fork: shells out to `git` via execFile, replaced legacy js-git]
- `index.js` (1) → `lib/vizion.js` (52): callback adapters — `analyze/isUpToDate/update/revertTo/prev/next`; revertTo validates revision as hex.
- `lib/git/git.js` (247): `runGit` (execFile, no shell, LC_ALL=C, GIT_TERMINAL_PROMPT=0, 5s timeout / 60s for `remote update`, 1MB maxBuffer). `parse` → {type,url(origin),revision,comment,unstaged,branch,remotes,remote,branch_exists_on_remote,ahead,next_rev,prev_rev (100-commit topo history),update_time,tags(≤10)}. `isUpdated` = `git remote update` + rev-parse compare. `revert` = `git reset --hard` (returns success flag, never hard-errors). `update/prev/next` compose the above.
- Consumers: God.js (analyze on process start w/ parent-dir walk, sets `pm2_env.versioning`), API/Version.js (`pm2 pull/backward/forward`), API/Containerizer.js; CLI `--no-vizion`. e2e: `test/e2e/misc/vizion.sh`.
- README still claims svn/hg support — git-only now.

### packager/ [STALE — last commit 2022-01-20] [UNTESTED]
- `build-dist.sh` (28): `npm pack` → dist/ + `npm install --production` → `pm2-v<ver>.tar.gz` + sha256.
- `build-deb-rpm.sh` (147): unpacks tarball to /usr/share/pm2, writes `/etc/default/pm2` (PM2_HOME=/etc/pm2) + systemd unit (`Type=forking`, `pm2 resurrect/reload/kill`), /usr/bin/pm2 wrapper script; fpm → noarch RPM (depends nodejs); dpkg-deb+fakeroot → deb; sed-patches `env node`→`env nodejs` for Debian.
- `setup.deb.sh` (232) / `setup.rpm.sh` (252): curl|bash-style repo installers (packagecloud apt/yum repo + GPG key import, OS detection).
- `publish_deb_rpm.sh` (14): package_cloud push to keymetrics/pm2 for EOL distros (ubuntu trusty…artful, debian wheezy…buster, el/5-7, poky).
- `debian/` control (Depends: nodejs >= 6.12.2 — predates engines>=18), postinst (creates `pm2` system user, enables systemd unit), prerm/postrm, lintian-overrides, copyright. `rhel/` same trio. `alpine/` APKBUILD (pinned sha512 of v2.7.2 zip — ancient) + `pm2_io.rsa.pub` signing key.

### pres/ [STALE] — 20 PNG marketing/README images + TMP.md (commands cheatsheet moved out of README). Referenced by README via raw.githubusercontent URLs.

### types/index.d.ts (730) [churn 4/2y] — hand-written public TS surface for programmatic API
Callback-style (errback) API: connect/start(5 overloads)/disconnect/stop/restart/delete/reload/killDaemon/describe/list/dump(declared twice)/flush/reloadLogs/launchBus/sendSignalToProcessName/startup/sendDataToProcessId/launchSysMonitoring/profile/env/getPID/trigger/inspect/serve/install/uninstall/sendLineToStdin/attach/get/set/multiset/unset. Interfaces: Proc, ProcessDescription, Pm2Env, Monit, StartOptions (~50 fields incl. cron, exp_backoff_restart_delay, stop_exit_codes, namespace, filter_env, docker fields), ServeOptions, InstallOptions; ProcessStatus / Platform unions. No Promise API declared (JS runtime has none either at this layer). types/tsconfig.json for self-check only, no dtslint tests [UNTESTED].

### package.json [HOT — 66 commits/2y] — v7.0.3, engines node>=18, AGPL-3.0, 4 bins (pm2, pm2-dev, pm2-docker, pm2-runtime), 22 runtime deps (audit in rust_map), overrides debug=4.4.3, devDeps express/mocha/should. Test scripts: unit.sh/e2e.sh (bash harnesses), per-module mocha suites.

### examples/ (skim) [low churn] — 28 sample-app dirs (cluster-http, ecosystem-file, esm, docker-pm2, run-php-python-ruby-bash, send-msg, module-test, …). Not referenced by test harness. `examples/send-msg/t2.js` is the ONLY consumer of the `tx2` runtime dependency.

## Flags

- **Dead code**: `Utility.Cipher` in pm2-io-agent uses `crypto.createCipher/createDecipher` — removed in Node 22; never called anywhere in repo (would crash if invoked on Node ≥22). `pm2-io-bpm/services/transport.js` ignores transport name → standalone/websocket APM mode unreachable; `pm2-io-bpm/utils/transactionAggregator.js` (429 LOC) unreferenced; pmx.js imports Meter/Histogram/Gauge/Counter/EventsFeature/TracingFeature it never uses; `tx2` runtime dependency consumed only by `examples/send-msg/t2.js`.
- **Security smells**: InteractorClient prints SECRET key to stdout on every `launchAndInteract` ("Public key: %s | Private key: %s") and again in error paths; secret key POSTed in JSON body to root.keymetrics.io (fine) but also persisted to `agent.json5` with default umask; ReverseInteractor grants the SaaS remote restart/reload/scale/stopLogging over pm2 (by design, but an always-on remote-control channel once linked); `startLogging` streams ALL process logs to cloud for 120s; WatchDog builds a shell string ``child.exec(`node ${pm2_binary_path} resurrect`)`` from env-provided PM2_BINARY_PATH (same-user env, low risk, still shell interpolation); setup.deb/rpm.sh are curl|bash repo installers importing GPG keys; @pm2/pm2-version-check phones home with version+docker state by default.
- **Portability traps**: win32 pipe names hardcoded, marked `@todo` (agent constants.js L90) — no per-user isolation on Windows pipes; `process._getActiveHandles()` (private Node API) in IPCTransport autoExitHook — Bun/deno/futures-Node hazard; Bun branches skip `child.unref()`/IPC-message wait in daemonize (different launch semantics per runtime); bpm profiling effectively unavailable on Bun (inspector disabled, addon needs unshipped native module); vizion depends on `git` binary in PATH (document as runtime requirement for Rust port).
- **Licensing**: repo is AGPL-3.0; vendored `modules/vizion` LICENSE is Apache-2.0 (Keymetrics 2016) and `modules/pm2-io-bpm` is a repack of @pm2/io (Apache-2.0 upstream, no LICENSE file in the vendored dir) — Apache-2.0 → AGPLv3 combination is one-way compatible, fine, but the Rust rewrite must carry AGPL obligations; `pm2-io-agent` vendored copy is AGPL-3.0; `@pm2/blessed` is an unmaintained fork of blessed (MIT); TransactionAggregator embeds code derived from Google Cloud Trace (Apache-2.0 headers upstream, stripped here); Utility EWMA copied from node-measured (MIT, attributed inline).
- **Staleness/rot**: packager/ untouched since 2022-01 — publishes to EOL distros (ubuntu trusty, debian wheezy, el/5), APKBUILD sha pinned to pm2 2.7.2, debian control wants nodejs>=6.12.2 vs engines>=18; vizion README still advertises svn/hg (git-only since fork rewrite); commander pinned at 2.15.1 (2018); types/index.d.ts declares `dump()` twice and header says "for pm2 7.0.0" while shape drifts from runtime (`Proc` fields).
- **Protocol quirk worth preserving or consciously breaking**: agent's `getOrSetConf` "json5" handling is a regex hack (`\s(\w+):` → quoted) over a file named agent.json5 — any Rust reimplementation of `pm2 link` state must read this legacy format or migrate it.
