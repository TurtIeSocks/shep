# Shared plumbing

## Inventory

## Inventory — aux bucket (shared plumbing)

### lib/Common.js — 919 LOC [HOT: 9 commits/2y] [COMPLEX: verifyConfs ~190 LOC]
CLI-side shared utils. Key flows:
1. **Ecosystem parse** `parseConfig(confObj, filename)`: extension detect via `knonwConfigFileExtensions` (sic, typo'd export) — `.json`→**`vm.runInThisContext('('+content+')')`** (JS eval, not JSON.parse!), `.yml/.yaml`→js-yaml `load`, `.config.js/.cjs/.mjs`→`require()` with cache-delete. `isConfigFile` uses `indexOf` substring match, not endsWith. `getConfigFileCandidates(name)` generates candidates for bare `pm2 start name`.
2. **App normalization** `prepareAppConf(opts, app)`: resolve cwd (+sets `process.env.PWD`), resolve `pm_exec_path` (fallback `which(script)`), env merge (process.env ⊕ app.env; `filter_env` bool/string/array substring filter), `sink.resolveInterpreter` (ext→interpreter via API/interpreter.json; Bun detection via `cst.IS_BUN`→`process.execPath`; binary detect via isbinaryfile→'none'; `node@X` NVM install via execSync; python→python3 fallback + PYTHONUNBUFFERED; lsc/coffee paths; missing node→`cst.BUILTIN_NODE_PATH`), `sink.determineExecMode` (node/bun + instances→cluster_mode default, else fork_mode), log-path resolution loop for log/out/error/pid files (default paths, `NULL`/`/dev/null`→`\\.\NUL` on win32, mkdir -p missing dirs).
3. **verifyConfs(appConfs)** — called on EVERY operation: exec_mode alias (`fork`→`fork_mode`), `cmd`/`command`→`script` alias, name render from script basename, space-in-script→wrap in `bash -c`/`sh -c` (unix only), `time`→log_date_format, uid/gid resolution via tools/passwd (root-only, unix-only), trace/deep_monitoring→OtelManager.ensureInstalled, instances `'max'`→0, cron validation via croner, final schema validation via tools/Config.validateJSON.
4. **mergeEnvironmentVariables(app, env_name, deploy_conf)**: priority deploy_conf[env].env < app.env < app.env_<name>; returns `{...env, current_conf}` shape consumed by Utility.extendExtraConfig.
5. Misc: `lockReload/unlockReload` (timestamp lockfile, 30s stale timeout), `determineSilentCLI` (monkeypatches all console.* to no-op), `printOut/printError/log/warn/err` silent-gated printers, ANSI-aware `cropToWidth`, `extend/safeExtend` (safeExtend has ~50-key ignore list of pm2-internal keys), `deepCopy`=fclone, `getCurrentUsername` (os.userInfo→env fallbacks).

### lib/Utility.js — 277 LOC (daemon-side twin of Common)
`findPackageVersion` (walk-up package.json via tools/find-package-json), `getDate`=Date.now, `extendExtraConfig` (merge current_conf into proc.pm2_env via extendMix), `extendMix` (extend but `'null'` string deletes key — CLI "unset" protocol), `formatCLU` (clone pm2_env minus env), `whichFileExists(arr)` first-stat-success, `clone`=fclone, `overrideConsole(bus)` (timestamps daemon console + re-emits as `log:PM2` bus events), `startLogging(stds, cb)` (creates append WriteStreams for std/out/err via async/waterfall, skips NULL//dev/null), `getCanonicModuleName` (strips .tgz/git+/http/scope/@version/#branch/.git from module specs — 6 regex stages), `checkPathIsNull`, `generateUUID` (Math.random, non-crypto).

### lib/Configuration.js — 309 LOC (pm2 set/get KV store)
File: `cst.PM2_MODULE_CONF_FILE` (~/.pm2/module_conf.json), JSON, 4-space indent. `splitKey` splits on `.` or `:` with quote support; **blocks `__proto__`/`constructor`/`prototype`** (prototype-pollution fix, tested in issue_6089 mocha). Ops: `set/unset` (async cb), `setSync/unsetSync/setSyncIfNotExist`, `multiset` (space-separated quoted pairs via eachSeries), `get/getSync/getAll/getAllSync`. Nested keys create intermediate objects; `key === 'all'` wipes store. All ops = read-whole-file → mutate → write-whole-file, **no locking**.

### lib/HttpInterface.js — 75 LOC (pm2 web)
Standalone script (spawned as pm2-managed process). `pm2.connect` → plain `http.createServer`. **Endpoints: `GET /` only** → JSON `{system_info:{hostname,uptime}, monit:{loadavg,total_mem,free_mem,cpu,interfaces}, processes:[pm2.list]}`; everything else 404. CORS `*`, GET-only header. Env vars of every process included unless `PM2_WEB_STRIP_ENV_VARS`. Binds `WEB_IPADDR` default **0.0.0.0**, port 9615. Error path calls `res.send` — doesn't exist on http.ServerResponse (crash).

### lib/OtelManager.js — 69 LOC (new, fork-added)
Manages 6 `@opentelemetry/*` packages. `isInstalled` (require.resolve sdk-node), `install`/`uninstall` = **runtime `execSync('npm|bun install --no-save ...')` inside PM2_ROOT**, `ensureInstalled` auto-installs on `app.trace`/`deep_monitoring` (called from Common.verifyConfs). Does not itself instrument — gates availability; actual tracing wired elsewhere (ProcessContainer side).

### lib/Worker.js — 200 LOC (daemon background loop)
Injected into God. On `God.Worker.start()`:
1. `wrappedTasks` every `WORKER_INTERVAL` (30s): via deprecated `domain` for crash isolation; per-proc (eachLimit concurrency 1): reset `prev_restart_delay` if uptime > `EXP_BACKOFF_RESET_TIMER` (30s); `maxMemoryRestart` → `God.reloadProcessId` if monit.memory > max_memory_restart (skipped if axm_options.pid set); re-entrancy guard `is_running`.
2. `sysMetricsTask` same interval: `SysMetrics.collect` → `God.system_infos` (exposed via `God.getSystemData` RPC); disable via `pm2 set pm2:sysmonit false`.
3. Daily `VersionCheck` unless `PM2_DISABLE_VERSION_CHECK`.
Also owns cron-restart registry: `God.registerCron/deleteCron` — croner job per pm_id → `God.restartProcessId`.

### lib/VersionCheck.js — 46 LOC [STALE: last 2022] [UNTESTED]
Phones home via `@pm2/pm2-version-check` with `{state, version, os.type, uptime, node version, docker(detected via /.dockerenv + /proc/self/cgroup)}`; prints "not UP TO DATE" if semver.lt.

### lib/Watcher.js — 117 LOC
God mixin. `God.watch.enable(pm2_env)`: chokidar watch (watch bool/[]→pm_cwd), ignored default `/[\/\\]\.|node_modules/`, `watch_options` passthrough, ignoreInitial. On any event: `restarting` flag guard → setTimeout(watch_delay||0) → `God.restartProcessName`. `disable` by pm_id, `disableAll` on kill (bug: calls `.splice` on plain object). Tested: test/programmatic/watcher.js.

### lib/tools/ — mostly vendored one-offs
- **Config.js** 252 LOC — schema-driven validator for API/schema.json: camelCase alias generation, `filterOptions(cmd)` commander→conf, `validateJSON` (type coercion, regex keys `\\` patterns, defaults), `_valid` (auto-parse Number, regex, min/max, string→Array shlex-ish split, custom `sbyte` (K/M/G) + `stime` (s/m/h) units). Stateful `this._errors` (not reentrant).
- **SysMetrics.js** 349 LOC — fork-authored (not vendored), dependency-free host metrics: CPU% (os.cpus deltas), RAM (/proc/meminfo | sysctl+vm_stat), net per-iface (/sys/class/net | netstat -bdnI, iface whitelist regex, argv-only execFile, 2s timeout), disk IO (/sys/block | ioreg), fs usage (df -kP). Returns axm_monitor-shaped `{name:{value,unit}}`. `create()` for test isolation; tested (sysmetrics.mocha.js). Linux+macOS only.
- **which.js** 121 LOC — PATH search, PATHEXT on win32. Derived from shelljs. [TODO: node<v6 comment]
- **sexec.js** 56 LOC — child.exec wrapper, callback(code,stdout,stderr), pipes to stdout unless silent. shelljs-exec-derived. Used by API.js, Startup, TAR/NPM modules.
- **open.js** 57 LOC — xdg-open/open/cmd-start launcher; SUDO_USER re-exec via sudo -u (validated w/ regex).
- **prompt.js** 78 LOC — readline prompt/password(raw mode)/confirm/choose.
- **passwd.js** 59 LOC — parses `/etc/passwd` + `/etc/group` into maps keyed by name AND id.
- **isbinaryfile.js** 95 LOC — first-512-bytes NULL/UTF-8 heuristic (vendored gjtorikian/isBinaryFile).
- **json5.js** 690 LOC — vendored JSON5 recursive-descent parser (Crockford-derived). Used only for `~/.pm2/agent.json5` interaction conf in API.js.
- **fmt.js** 73 LOC — console separators/fields (Andrew Chilton, MIT header). Used by Daemon, DevCLI, Extra, Containerizer.
- **treeify.js** 114 LOC — ASCII tree printer (notatestuser). Used by API.js (module tree display).
- **multimeter/** (~4 files) + **charm/** — substack progress-bar + terminal control vendored; used by API/Monit.js legacy monitor.
- **promise.min.js** — minified promise-polyfill; required by Containerizer only. Node has native Promise — dead weight.
- **copydirSync.js** 102 LOC — recursive sync copy w/ mode/utimes (deprecated `'binary'` encoding, whole-file buffering). Used by Extra, Modules/NPM.
- **deleteFolderRecursive.js** 20 LOC — manual rimraf. `fs.rmSync(p,{recursive:true})` supersedes.
- **find-package-json.js** 75 LOC — walk-up package.json iterator (3rd-Eden).
- **IsAbsolute.js** 21 LOC — path-is-absolute clone. **Zero usages in lib/ — dead code** (only examples/ copy references it).
- **xdg-open** 21KB — vendored freedesktop shell script (MIT-style header intact).

### lib/templates/
`ecosystem*.tpl` (4 variants: CJS/ES, full w/ deploy section, simple) — `pm2 init` scaffolds. `init-scripts/`: systemd.tpl (Type=forking, `%PM2_PATH% resurrect/reload/kill`, PIDFile, %USER%/%HOME_PATH%/%NODE_PATH% placeholders), systemd-online.tpl, upstart, launchd, openrc, rcd (FreeBSD), rcd-openbsd, smf (Solaris), amazon sh. `logrotate.d/pm2` (weekly, 12 rotations, copytruncate). `Dockerfiles/` (node/java/ruby tpl for pm2 in docker). `sample-apps/` (http-server, metrics-actions, python).

### lib/motd — ASCII banner shown on first install (references pm2.io upsell).

## Flags

- **SECURITY — parseConfig JS-evals .json files**: `vm.runInThisContext('('+content+')')` — a "JSON" ecosystem file is arbitrary code execution (runInThisContext is NOT a sandbox; 1000ms timeout only). Rust: strict json5 parse.
- **SECURITY — HttpInterface**: default bind 0.0.0.0:9615, CORS *, no auth, full process env (secrets) exposed unless PM2_WEB_STRIP_ENV_VARS (default false). Known pm2 exposure class. Also crash bug: error path calls nonexistent `res.send` on http.ServerResponse.
- **SECURITY/supply-chain — OtelManager**: `execSync('npm install --no-save ...')` at runtime into PM2 install dir, triggered by `pm2 start --trace`; runs with CLI's privileges (often root via `pm2 startup`).
- **PRIVACY — VersionCheck**: daily telemetry (os type, uptime, node version, docker detection) to pm2's server, opt-out env only.
- **Bugs to not port**: Common.determineSilentCLI boolean chain `(s2opt != -1 != s2opt < pos)` — nonsense precedence; `Common.isConfigFile` substring match (`app.json.bak` → json); Watcher.disableAll `.splice()` on a plain object (throws); verifyConfs error text `--git` (means --gid) and mixed Error/array returns; Configuration read-modify-write race (no lock).
- **Dead code**: tools/IsAbsolute.js (zero lib/ usages), tools/promise.min.js (Promise polyfill, Node≥0.12 native), tools/fmt.js trivial, multimeter/charm legacy monit only.
- **Deprecated APIs**: Worker.js uses Node `domain` (deprecated); copydirSync uses `'binary'` encoding.
- **Portability traps**: passwd.js parses /etc/passwd — wrong on macOS (Directory Services) and LDAP/NSS systems → uid/gid resolution (`pm2 start --user`) silently fails there; Windows null device `\\.\NUL` handling in log paths must be preserved; NVM paths differ Windows (NVM_HOME, node32/64.exe rename) vs unix (NVM_DIR); which.js PATHEXT uppercasing; SysMetrics Linux/macOS only; space-in-script→`bash -c` wrap explicitly skipped on Windows.
- **Vendored licensing (AGPL-3.0 repo, notices must survive or code must be replaced)**: which.js + sexec.js derived from **shelljs (BSD-3-Clause — attribution REQUIRED, headers absent)**; isbinaryfile.js is gjtorikian/isBinaryFile (MIT) but carries a PM2 copyright header (mis-attribution); promise.min.js (taylorhakes promise-polyfill, MIT) no license text; multimeter/charm (substack, MIT/X11) no license text; treeify (MIT, attribution-only header), json5 (Crockford-derived, MIT project, no license text), copydirSync (npm copy-dir, MIT, no header), find-package-json (3rd-Eden, MIT, no header). xdg-open and fmt.js DO carry proper permissive headers. Replacing all with crates (as mapped) dissolves the issue.
- **Oddity**: `knonwConfigFileExtensions` typo is a public-ish export name; Config.js validator mutates shared `this._errors` (not reentrant); Utility.generateUUID is Math.random-based (predictable).
