# API peripheral features

## Inventory

## Bucket: api-extra (peripheral API features)

### lib/API/Startup.js — 629 LOC [COMPLEX]
Startup-script generation + process-list persistence. Key fns:
- `detectInitSystem()`: probes PATH binaries → init system: `systemctl`→systemd, `update-rc.d`→upstart, `chkconfig`→systemv, `rc-update`→openrc, `launchctl`→launchd, `sysrc`→rcd (FreeBSD), `rcctl`→rcd-openbsd, `svcadm`→smf (Solaris). **No Windows.**
- `startup(platform, opts, cb)` (~210-line switch): reads template from `lib/templates/init-scripts/*.tpl` (systemd, systemd-online for `--wait-ip`, upstart, launchd, rcd, rcd-openbsd, openrc, smf), substitutes `%PM2_PATH% %NODE_PATH% %USER% %HOME_PATH% %SERVICE_NAME%`, writes to platform destination (`/etc/systemd/system/pm2-USER.service`, `~/Library/LaunchAgents/pm2.USER.plist`, `/etc/init.d/...`, `/usr/local/etc/rc.d/...`, `/etc/rc.d/...`, smf XML in tmpdir), then runs enable commands serially via `sexec`. Platform aliases: ubuntu/centos/arch/oracle→systemd; ubuntu14/12→upstart; amazon/centos6→systemv; macos/darwin→launchd; freebsd→rcd; openbsd→rcd-openbsd; sunos/solaris→smf.
- `isNotRoot()`: non-root → prints copy/paste `sudo env PATH=... pm2 startup <platform> -u <user> --hp <home>` and errors.
- `uninstallStartup()`: per-platform stop/disable/rm command lists; detects legacy `/etc/init.d/pm2-init.sh` ("oldsystem").
- `dump(force)`: `getMonitorData` → strips `instances`/`pm_id`/`prev_restart_delay`, skips `pmx_module` procs, backs up dump file, writes `DUMP_FILE_PATH` (refuses empty list unless `--force`); `clearDump`; `autodump` = no-op back-compat stub.
- `resurrect()`: reads dump (backup fallback, deletes broken), diffs current vs saved by app name, `prepare`s only missing apps via `eachLimit(CONCURRENT_ACTIONS)`.
Tested: test/e2e/misc/startup.sh. 3 commits/2yr.

### lib/API/Configuration.js — 212 LOC [STALE]
CLI verbs over lib/Configuration.js kv store (`module_conf.json`): `get/set/multiset/unset/conf`. Key format `module:key` or `module.key`; setting a scoped key restarts that module with `updateEnv` under `PM2_PROGRAMMATIC`. No-arg `set`/`conf` opens `$EDITOR` on `PM2_MODULE_CONF_FILE`. Bug: `multiset` non-scoped branch calls `displayConf(app_name)` with hoisted-undefined `app_name`. 1 commit/2yr (2025-05). Tested: test/e2e/modules/get-set.sh.

### lib/API/Extra.js — 734 LOC
Grab-bag of ~20 CLI methods:
- `getVersion`, `ping`, `getPID(name)`, `env(app_id)` (dumps pm2_env).
- `report()`: GitHub-issue diagnostic — daemon `getReport` + CLI/system info (os.*) + process list + last 20 daemon log lines.
- `profile(cpu|mem, time)`: remote `profileCPU/profileMEM`, writes `.cpuprofile`/`.heapprofile` (V8/Node-specific). Crashes if type not cpu/mem (`cmd` undefined).
- `boilerplate()`: `pm2 create` — copies `lib/templates/sample-apps` project + prompt + starts it.
- `sendLineToStdin`/`attach(pm_id)`: readline REPL wired to proc stdin + `log:*` bus events.
- `sendDataToProcessId`, `msgProcess`, `trigger(pm_id, action)`: custom in-app actions (pmx); waits on `axm:reply` bus events with `process_wait_count` barrier.
- `sendSignalToProcessName/Id`.
- `serve(path, port, opts)`: starts `lib/API/Serve.js` as managed proc; config via env vars incl. `PM2_SERVE_BASIC_AUTH_USERNAME/PASSWORD` (creds in process env).
- `autoinstall()`: starts Sysinfo/ServiceDetection.
- `remote/remoteV2`: Keymetrics remote-command dispatch (calls own methods by name).
- `generateSample(mode)`: writes `ecosystem.config.js` from template.
- `dashboard()` (blessed UI, 800ms poll loop), `monit()` (400ms poll). Bug: `monit` error path references undefined `conf.ERROR_EXIT` → ReferenceError.
- `inspect(app)`: triggers `internal:inspect` (chrome://inspect, Node-only).
5 commits/2yr. Partial e2e coverage (monit.sh, attach.sh).

### lib/API/Containerizer.js — 335 LOC
`pm2 docker:dev|docker:distribution` (wired lib/binaries/CLI.js:277-279). Flow: `checkDockerSetup` (docker version, sudo-group help) → generate or mode-switch Dockerfile (`parseAndSwitch` rewrites `FROM keymetrics/pm2:<ver>`, `ENV NODE_ENV`, `COPY . /var/app`, `CMD ["pm2-docker"...]`/`["pm2-dev"...]`) → `docker build` (opt `--no-cache`) → `docker run --net host` (dev: bind-mount cwd) → SIGINT handler prints suggested `docker commit/push` using vendored vizion git metadata. **Broken:** `require('../../tools/prompt')` in `dockerMode` resolves to nonexistent `<repo>/tools/prompt` (correct: `../tools/prompt`) → interactive path crashes; proves feature unused. Uses vendored `lib/tools/promise.min.js` polyfill. Tested: containerizer.mocha.js (parseAndSwitch only). 3 commits/2yr (lint-only).

### lib/API/Deploy.js — 117 LOC [UNTESTED]
`pm2 deploy <file> <env> <cmd>`. Thin shim: ecosystem-file discovery (config candidates + ecosystem.json5 + package.json), parse, default `post-deploy` = `pm2 startOrRestart <file> --env <env>`, delegates entirely to external `pm2-deploy@1.0.2` npm pkg (bash-script git-over-ssh deploy). `deployHelper()` = usage text. 1 commit/2yr.

### lib/API/Serve.js — 471 LOC [HOT]
Standalone static HTTP server **script** (spawned as the managed process). Config from env: `PM2_SERVE_PORT/HOST/PATH/SPA/FTP/HOMEPAGE/BASIC_AUTH*/MONITOR`. ~170-entry ext→MIME table. Path-traversal guard (path.relative startsWith '..' check — correct). Modes: plain static; SPA fallback-to-homepage; "ftp" mode = HTML directory listing (escaped, dirs-first). Custom 404.html. Basic auth: plaintext string compare. CORS `*` + GET on all responses. When `PM2_SERVE_MONITOR` set: reads `$PM2_HOME/agent.json5` (regex-hack JSON5→JSON), injects pm2.io APM browser `<script>` (apm.pm2.io) into served HTML. 7 commits/2yr (dir-listing added by fork). Tested: test/e2e/cli/serve.sh.

### lib/API/schema.json — 375 LOC [STALE]
**The ecosystem app-config contract** (consumed by lib/tools/Config.js validateJSON: type coercion, alias resolution, regex, min). Full key list (name : type [aliases] {default}):
- `script`: string, required [exec]
- `name`: string; `name_prefix`: string; `namespace`: string {default}
- `filter_env`: bool|array|string {false}
- `install_url`: string; `cwd`: string
- `args`: array|string; `node_args`: array|string [interpreterArgs, interpreter_args]
- `exec_interpreter`: string [interpreter] {node}
- `out_file`: string [out, output, out_log]; `error_file`: string [error, err, err_file, err_log]; `log_file`: bool|string [log]; `disable_logs`: bool {false}; `log_type`: string (json); `log_date_format`: string (dayjs fmt); `time`: bool
- `env`: object|string; pattern `^env_\S*$`: object|string (per-env blocks)
- `max_memory_restart`: string|number, regex `^\d+(G|M|K)?$`, ext_type sbyte
- `pid_file`: string [pid]
- `restart_delay`: number {0}; `exp_backoff_restart_delay`: number {0}
- `source_map_support`: bool {true}; `disable_source_map_support`: bool {false}
- `wait_ready`: bool {false}; `listen_timeout`: number; `kill_timeout`: number {1600}; `shutdown_with_message`: bool {false}
- `instances`: number {1}; `exec_mode`: string regex `^(cluster|fork)(_mode)?$` {fork}
- `cron_restart`: string|number [cron]
- `merge_logs`: bool [combine_logs] {false}
- `vizion`: bool {true}; `autostart`: bool {true}; `autorestart`: bool {true}
- `stop_exit_codes`: array|number
- `watch`: bool|array|string {false}; `watch_delay`: number; `ignore_watch`: array|string; `watch_options`: object (chokidar passthrough)
- `min_uptime`: number|string regex `^\d+(h|m|s)?$` min 100 ext_type stime {1000}; `max_restarts`: number min 0 {16}
- `execute_command`: bool; `force`: bool {false}; `append_env_to_name`: bool {false}
- `post_update`: array (Keymetrics pull hook)
- `trace`/`disable_trace`/`v8`/`event_loop_inspector`/`deep_monitoring`: bool (pm2.io APM knobs)
- `increment_var`: string; `instance_var`: string {NODE_APP_INSTANCE}
- `pmx`: bool|string {true}; `automation`: bool {true}; `treekill`: bool {true}
- `port`: number (injects PORT env); `username`: string; `uid`: number|string [user]; `gid`: number|string
- `windowsHide`: bool {true}; `kill_retry_time`: number {100}; `write`: bool; `io`: object (apm config)
Tested: json_validation.mocha.js.

### lib/API/interpreter.json — 12 LOC [STALE]
Ext→interpreter map (consumed lib/Common.js): .sh→bash, .py→python, .rb→ruby, .php→php, .pl→perl, .js→node, .coffee→coffee, .ls→lsc, **.ts/.tsx→bun** (fork's Bun support).

### lib/API/Modules/ — 1247 LOC total [HOT dir: 8 commits/2yr]
- **index.js** (120): CLI verbs install/uninstall/launchAll/package/publish/generateModuleSample/deleteModule (matches proc by name + `pmx_module` flag; deletes first match only).
- **Modularizer.js** (148): dispatcher. Strips `[;`|]` from module name (weak shell sanitization). Routes: INTERNAL_MODULES→LOCAL; `.`→NPM.localStart (dev mode + watch); tarball/`*.tar.gz`→TAR; else NPM. `launchModules` on daemon boot: serial npm-then-tar. Module registry = keys under Configuration `MODULE_CONF_PREFIX`(_TAR).
- **NPM.js** (443): install = backup(copydirSync→tmpdir) → uninstall → spawn **bun preferred if on PATH** (`bun install --cwd`) else `npm --prefix` else builtin node+npm, into `~/.pm2/modules/<name>` → merge package.json `config` into Configuration defaults → `StartModule` (script autodetect: `apps` field → `bin` → `main`; `force_name`, `started_as_module`, watch in dev). `--safe N`: watches restart_time>2 for N ms then `Rollback.revert`. `publish`: semver minor bump, `npm publish`, then shell string `git add . ; git commit -m "<ver>"; git push origin master`.
- **TAR.js** (368): tarball modules. Remote fetch via spawned **wget** (not fetch). Extract: spawn `tar zxf --strip-components 1` into `~/.pm2/modules/<name>`; module name read by extracting only `module/package.json` from archive. Optional hardcoded `yarn install`. App-name prefixing rules (`needPrefix`). `packager`: `tar zcf --transform 's,X,module,'` — **GNU-tar-only**. `publish`: POST FormData (global fetch/Blob, Node 18+) to `pm2_configuration.registry`/api/v1/modules.
- **LOCAL.js** (122): INTERNAL_MODULES table — deep-monitoring, gc-stats, event-loop-inspector, v8-profiler-node8, typescript+ts-node, livescript, coffee-script v1/v2 — npm-installed into **pm2's own node_modules**. Parallel multi-install + optional post_install script.
- **flagExt.js** (46) [UNTESTED]: recursive **sync** walk of cwd; collects files NOT matching `--ext` extensions → becomes `ignore_watch` (API.js:716). Inverted-logic whitelist watcher. Unix-only `/` path concat.
Tested: modules.mocha.js, e2e/modules/{module,module-safeguard,get-set}.sh.

### lib/API/ExtraMgmt/Docker.js — 30 LOC [STALE 2019]
Maps stop/restart/delete actions on container pseudo-ids shown in `pm2 ls` (sysinfo containers section) to spawned `docker stop|rm|restart` (shell:true). Consumed at lib/API.js:1540.

### lib/API/pm2-plus/ — ~1160 LOC (5 commits/2yr)
- **PM2IO.js** (381): `pm2 plus` hub. Auth strategy pick: darwin/win32/linux-with-desktop → WebAuth (browser OAuth), else CliAuth. **Hardcoded OAuth client ids** ('138558311', '0943857435'). Verbs: connect/login/register, validate (email token), welcome (ASCII), logout (kill agent + revoke), create/web (bucket CRUD via `@pm2/js-api`, opens app.pm2.io). `open()` browser launcher with SUDO_USER re-exec (regex-validated).
- **link.js** (126): `pm2 link stop|kill|info|delete|<secret> <public>` → drives **vendored** modules/pm2-io-agent InteractorClient (launchAndInteract/killInteractorDaemon). Sets `WS_JSON_PATCH` env.
- **helpers.js** (97): openDashboard (app.pm2.io/#/r/<pubkey>), clearSetup, minimumSetup (auto-installs pm2-logrotate + pm2-server-monit [+deep-metrics enterprise], reload all). Dead commented-out prompt block.
- **process-selector.js** (52): monitorState monitor|unmonitor by all|name|id → remote call.
- **auth-strategies/CliAuth.js** (306): terminal login/register against id.keymetrics.io: password prompt → POST JSON creds; register w/ T&C confirm; token file `PM2_IO_ACCESS_TOKEN`; `PM2_IO_TOKEN` env override; refresh-token renewal. Bug: tryEach final cb dereferences `result.refresh_token` without null-check when all strategies fail → TypeError.
- **auth-strategies/WebAuth.js** (195): browser OAuth; one-shot http server on **fixed port 43532** catches redirect token; same null-deref risk; duplicate `open()` impl.
- **pres/**: motd, motd.update, welcome — ASCII-art pm2.io ads (motd.update printed post-`pm2 update` when unlinked, lib/API.js:400).
Tested: e2e/cli/plus.sh only; auth strategies untested.

## Flags

- **Broken/dead code**: Containerizer.js `require('../../tools/prompt')` resolves outside lib/ → nonexistent; crashes `pm2 docker:*` prompt path (feature de facto dead). Extra.js `monit()` error path uses undefined `conf` → ReferenceError; `profile()` with unknown type derefs undefined `cmd`. Configuration.js `multiset` non-scoped branch passes hoisted-undefined `app_name`. Both auth strategies (CliAuth/WebAuth) deref `result.refresh_token` when result is undefined after total failure. `autodump()` = documented no-op stub. helpers.js commented-out prompt block; Serve.js commented-out @pm2/io probe. Vendored `promise.min.js` polyfill (Node ≥0.12 has Promise).
- **Security smells**: TAR module install fetches arbitrary URL via wget, no checksum/signature, extracts with spawned tar (path-sanitization delegated to tar binary). Modularizer name "sanitization" strips only ``[;`|]`` yet names later interpolated into shell strings (`cd X ; yarn install`, git commit -m). NPM.publish builds `git commit -m "<version>"` shell string. Serve.js: plaintext non-constant-time basic-auth compare; creds passed via child env (readable via `pm2 env`/`ps e`); CORS `*` unconditionally; injects third-party apm.pm2.io script into served HTML when monitor enabled. CliAuth POSTs password JSON to id.keymetrics.io; WebAuth one-shot token catcher on fixed localhost:43532 (port squat/token interception); hardcoded OAuth client IDs; SUDO_USER re-exec in `open()` (regex-guarded — pattern of a past injection fix). Startup runs root shell command chains built by string concat from opts.serviceName/user (`sexec(commands.join('&& '))`) — injection if serviceName attacker-controlled.
- **Portability traps**: startup — no Windows; `process.getuid()` calls unguarded (throws on Windows). TAR packager uses GNU-tar-only `--transform` (fails with macOS/BSD tar); wget not present on stock macOS. flagExt.js unix `/` path concat + full sync fs walk of cwd. Deploy delegates to bash scripts (pm2-deploy) — no Windows. Docker.js spawn with `shell:true`.
- **SaaS coupling**: hardcoded pm2.io/keymetrics endpoints (id.keymetrics.io, app.pm2.io, apm.pm2.io, registry API) across pm2-plus/, Serve.js, TAR.publish — dead weight/liability if service sunsets; schema carries APM-only keys (trace, v8, deep_monitoring, io, pmx, automation, post_update).
- **Vendored deps licensing**: `modules/` contains vendored pm2-axon, pm2-axon-rpc, pm2-io-agent, vizion, fclone (Containerizer uses vizion; link.js uses pm2-io-agent) — verify each embedded LICENSE vs repo's AGPL-3.0 umbrella before any code-level reference in the rewrite; `lib/tools/promise.min.js` minified vendored file with no visible license header.
- **Oddities**: profile filename uses dayjs `'dd-HH:mm:ss'` (dd = weekday abbrev, e.g. `Su-14:03:22.cpuprofile`); interpreter.json maps .ts/.tsx to bun only (no tsx/ts-node fallback if bun absent — fork-specific behavior to preserve or gate); Modules NPM path silently prefers bun over npm when both installed.
