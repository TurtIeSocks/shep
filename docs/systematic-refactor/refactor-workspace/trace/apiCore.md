# API core + display

## Inventory

## api-core inventory

### lib/API.js — 1933 LOC — churn 5/2y (borderline HOT), [COMPLEX] (`_startJson` ~280 LOC, `_operate` ~277 LOC), tested (test/programmatic/api.mocha.js, api.backward.compatibility.mocha.js)
Class `API` = the whole programmatic surface + CLI backend. Constructor: resolves cwd/pm2_home (`independent:true` → random /tmp dir, non-Windows only), builds `Client` (RPC), reads `Configuration.getSync('pm2')`, detects pm2-plus link via INTERACTOR_PID_PATH kill(pid,0) probe, async-loads INTERACTION_CONF (JSON→json5 fallback). Windows stdout `setBlocking(true)` hack.

**Public methods (1-line each):**
- `connect(noDaemon, cb)` — Client.start; if fresh daemon spawned → `launchAll` (modules) then cb.
- `destroy(cb)` — killDaemon then `rm -rf pm2_home` via sexec (refuses paths containing `.pm2`).
- `disconnect(cb)` / `close(cb)` — Client.close.
- `launchModules(cb)` — alias launchAll.
- `launchBus(cb)` — Client.launchBus (event bus: logs/events).
- `exitCli(code)` — KMDaemon.disconnectRPC → Client.close → fd-bitmask stdout/stderr drain dance → process.exit.
- `start(cmd, opts, cb)` — config-file/object → `_startJson`; else `_startScript`; no cb → `speedList()`.
- `reset(name, cb)` — RPC `resetMetaProcessId` per id ('all'/name/id resolution).
- `update(cb)` — dump → killDaemon → launchDaemon → launchRPC → resurrect → launchAll → KM relink; prints motd file on version change.
- `reload(name, opts, cb)` — `Common.lockReload()` mutex (file-based, `--force` override); config file → _startJson('reloadProcessId') else _operate.
- `restart(cmd, opts, cb)` — cmd `-` = read JSON from stdin (pipe); config file → _startJson; else _operate('restartProcessId'); prints IMMUTABLE_MSG unless --update-env.
- `delete(name, jsonVia, cb)` — pipe/file → actionFromJson('deleteProcessId'); else _operate.
- `stop(name, cb)` — same trio as delete with 'stopProcessId'.
- `list(opts, cb)` — RPC `getMonitorData`; `--watch` = clear-screen + re-render UX.list every 900ms forever.
- `killDaemon(cb)` / `kill(cb)` — notifyKillPM2 → delete all → killAgent → Client.killDaemon.
- `getProcessIdByName(name, cb)` — RPC lookup + console.log.
- `jlist(debug)` — print raw JSON (or util.inspect) of getMonitorData.
- `slist(tree)` — RPC `getSystemData`, print (treeify optional).
- `speedList(code, apps_acted)` — post-command table render: getSystemData (host-metrics line) + getMonitorData → UX.list/list_min; TTY detection; 1 retry after 1400ms on error; `--no-daemon` mode → stream logs + auto-exit; `--attach` → streamLogs of acted apps.
- `scale(app, number, cb)` — '+N' add via RPC `duplicateProcessId` loop, '-N' remove, absolute N diff.
- `describe(id, cb)` — filter getMonitorData by pm_id/name → UX.describe per match.
- `deepUpdate(cb)` — shells `npm i -g pm2@latest; pm2 update`.

**Private:** `_startScript` — async series of 4 resolvers: restartExistingProcessName → restartExistingNameSpace → restartExistingProcessId → restartExistingProcessPathOrStartNew (final: Common.resolveAppAttributes → RPC `prepare`); aborts series by passing `true` as err. `_startJson` — parse file/pipe/object, merge `static` server entries as Serve.js apps, apply CLI overrides (--only/--watch/--namespace/--instances/--uid/--gid/name_prefix), diff against running list: existing apps → _operate(action) with merged env (updateEnv=true), missing → `startApps` → RPC `prepare`. `actionFromJson` — per-app name→ids → RPC action, notifyGod. `_operate` — the central verb dispatcher: name='all' → all ids (modules excluded unless stopping); `/regex/` → match names via getMonitorData; name → ids by name, fallback namespace; numeric → Docker higher-id detection (pm2_configuration.docker) → DockerMgmt.processCommand, else name-as-number/namespace/id fallback chain; then `processIds`: eachLimit RPC calls (concurrency CONCURRENT_ACTIONS, forced 1 if ≤2 ids, 10 for delete), env rebuild if update_env, notifyGod per action, result trimmed to {name,namespace,pm_id,status,restart_time,env}. `_handleAttributeUpdate` — camelCase→snake_case CLI arg filter, deletes commander default-true booleans (treekill/pmx/vizion/automation/autostart/autorestart) to avoid clobbering.

Bottom of file: mixin loading — Extra/Deploy/Modules/pm2-plus(link,process-selector,helpers)/Configuration/Version/Startup/LogManagement/Containerizer all monkey-patch `API.prototype`.

### lib/API/Log.js — 315 LOC — churn 2/2y, tested via e2e (test/e2e/logs/*)
- `Log.tail(apps_list, lines, raw, cb)` — sort files by mtime (sync stat in comparator), read last `lines*200` bytes per file, split lines, print with colored `name | ` prefix padding (green=out, red=err, blue=PM2).
- `Log.stream(Client, id, raw, timestamp, exclusive, highlight)` — launchBus → `bus.on('log:*')`; filter by id/name/pm_id/namespace or 'all'; exclusive out/err filter; dynamic padding growth (min_padding); timestamp via dayjs format; highlight substring via bgBlackBright; `socket.on('reconnect attempt')` + global._auto_exit → exit 0 when target PM2 dies (--no-daemon mode).
- `Log.devStream` — same + 'process:event' online → "[rundev] App restarted"; used by `pm2-dev`.
- `Log.jsonStream` — bus events as NDJSON ({message,timestamp,type,process_id,app_name}).
- `Log.formatStream` — logfmt-ish output (`timestamp= app= id= type= message=`).
- `pad(pad, str, padLeft)` helper.

### lib/API/LogManagement.js — 371 LOC — churn 1/2y, tested (flush.mocha.js, e2e/logs/*)
Mixin on CLI.prototype:
- `flush(api, cb)` — truncate PM2_LOG_FILE_PATH + per-process out/err/pm_log paths via `fs.closeSync(fs.openSync(path,'w'))`; `api` = optional id/name filter.
- `logrotate(opts, cb)` — root-only (`process.getuid()!=0` → error+sudo hint); writes template to `/etc/logrotate.d/pm2-<user>` with %HOME_PATH%/%USER% substitution. Linux-only.
- `reloadLogs(cb)` — RPC `reloadLogs` (daemon reopens fds).
- `streamLogs(id, lines, raw, timestamp, exclusive, highlight)` — getMonitorData → build files_list (out/err paths, dedupe, skip /dev/null) matching id/name/namespace/`/regex/`; Log.tail(PM2 log first if id all/PM2) then Log.tail(files) then Log.stream (per regex/namespace match or once). lines=0 → stream only.
- `printLogs(id, lines, ...)` — same file-list build, tail only, then exitCli (no stream). ~90 LOC copy-paste of streamLogs' list building.

### lib/API/Monit.js — 247 LOC — [UNTESTED] TUI, churn 2/2y
`pm2 monit` legacy htop-style view (driven from Extra.js `monit()`: Monit.init() then getMonitorData poll loop). Uses **vendored** `lib/tools/multimeter` (+ vendored charm) progress bars. `init` → multimeter(process), ^C handler; `reset` — charm reset + banner; `refresh(processes)` — bar-count mismatch or status change → full `addProcesses` redraw, else `updateBars`. `addProcess` — writes name/status lines + 2 bars (cpu %, memory ratio); `drawRatio` — memory scaled to totalmem/500|50|5|1 tiers; `updateBars` — offline → red status text, no monit → 'No data'. Mutates global `Object.size = fn` (built-in pollution).

### lib/API/Dashboard.js — 457 LOC — [UNTESTED] TUI, churn 2/2y
`pm2 dash` blessed (@pm2/blessed fork) 4-pane TUI, driven from Extra.js `dashboard()`: init() + launchBus log feed → Dashboard.log + getMonitorData poll → Dashboard.refresh. Panes: process list (30% left, 70% h), logBox (right), metricsBox (axm custom metrics, bottom-left), metadataBox (bottom-right), help footer. left/right keys cycle focus; q/esc/Ctrl-C exit; 300ms render interval. `refresh` — sort by name, per-proc line `[id] name Mem: X MB CPU: Y % status` with red↔green `gradient()` hex color by % of total pm2 memory / cpu; renders selected proc's logs + 18-line metadata (name/ns/version/restarts/uptime/paths/args/interpreter/exec-mode/node-version/watch/unstable-restarts/versioning) + axm_monitor metrics. `log(type, data)` — per-pm_id ring buffers, global 200-line cap evicting from largest buffer. Helpers: `timeSince` (dup of UX helper), `gradient`.

### lib/API/Version.js — 383 LOC — churn 2/2y, tested via e2e (pull.sh, versioning-cmd.sh)
**Misnamed: it's git-versioning ops (vizion), not version-check.** Mixin on CLI.prototype:
- `_pull(opts, cb)` — getProcessByNameOrId → vizion.update(repo_path) → getPostUpdateCmds → execCommands → reload/restart proc.
- `pullAndRestart` / `pullAndReload` — both call `_pull` with action:'reload' (Restart variant is a lie).
- `pullCommitId(name, commit, cb)` — vizion.isUpToDate → vizion.revertTo(commit) → post-update cmds → reload.
- `backward(name, cb)` — vizion.prev; on post-update cmd failure rolls forward (vizion.next) to undo.
- `forward(name, cb)` — vizion.next; on failure rolls back (vizion.prev).
- `execCommands` — eachSeries `exec('cd '+repo_path+';'+command)` accumulating stdout; `exec` wrapper with 3MB buffer, EXEC_TIMEOUT (module-level var, mutated by ecosystem `exec_timeout`).
- `getPostUpdateCmds` — scans ecosystem.json/process.json/package.json in repo for `apps[].post_update` matching proc name; abuses eachSeries error channel to return the array.

### lib/API/UX/index.js — 9 LOC — barrel: {helpers, describe, list, list_min}.

### lib/API/UX/pm2-ls.js — 565 LOC — [HOT] 7 commits/2y (active fork work: width-adaptive layout)
Default export `(list, commander, systemdata)` = `pm2 ls` renderer.
- `listModulesAndAppsManaged` — cli-tableau fixed-width tables; dynamic name col (≤40) + id col; 3 layouts picked by terminal width (full 13-col / condensed 7-col / mini 5-col; non-TTY fallback 300 cols = full); `--sort field:order` via getNestedProperty; apps vs modules split (pmx_module flag; pm2-sysmonit hidden); `[T]` tracing / `[M]` monitored name prefixes; uid→username via passwd parse; `fitColumn` pre-truncation (works around cli-tableau ANSI miscount).
- `containersListing(sys_infos)` — Docker containers table (stacked variant <140 cols). **Dead: never called in file, not exported.**
- `listHighResourcesProcesses` — system procs >60% cpu / >30% mem table (excludes node/God). **Dead: never called.**
- `miniMonitBar(sys_infos)` — one-line host metrics summary (cpu/temp/ram/gpu/net per-iface with err+drop/disk+fs-use) from daemon SysMetrics axm_monitor map.
- `checkIfProcessAreDumped` — dump-file vs running diff warning; **call commented out = dead.**
- Module-global mutable `proc_id`, `CONDENSED_MODE`.

### lib/API/UX/pm2-describe.js — 197 LOC — churn 2/2y
`pm2 describe/show` renderer: main attr table (status/name/ns/version/restarts/uptime/paths/args/interpreter/exec-mode/node-version/node-env/watch/unstable-restarts/created-at; conditional splices for pm_log_path/cron_restart/max_memory_restart), module human_info table, module_conf table, versioning table, axm_actions table (+ trigger hint), axm_monitor metrics table, divergent-env table (process.env keys differing from proc env, value width-clamped), footer hints.

### lib/API/UX/pm2-ls-minimal.js — 31 LOC — [STALE] last commit 2019-11
`pm2 ls -m` / non-TTY: plaintext key:value block per process.

### lib/API/UX/helpers.js — 213 LOC — churn 1/2y
`bytesToSize` (1024-based, trailing space quirk), `colorStatus` (online/running→green, restarting/created→yellow, launching→blue, else red), `safe_push` (null→'N/A' guard before table.push), `timeSince` (s/m/h/D/M/Y; `>1` thresholds so "1h" renders as "60m"-ish off-by-one), `colorizedMetric` (green/yellow/red thresholds, inverted mode), `getNestedProperty` (dot-path), `openEditor` (spawn $VISUAL/$EDITOR, vim/notepad fallback), `dispKeys` (module config display).

## Flags

- **Command injection surfaces**: `API.destroy` → `sexec('rm -rf ' + pm2_home)` (path unquoted; only guards substring '.pm2'); Version.js `exec('cd '+repo_path+';'+command)` runs ecosystem-supplied `post_update` strings through shell with repo path concatenated; `deepUpdate` shells `npm i -g pm2@latest; pm2 update`. All must become arg-vector spawns in Rust.
- **Bug — `pm2 scale app -N` crashes**: API.js `rmProcs(procs[0], number, end)` passes a single proc object where rmProcs indexes `procs[i++].pm2_env` → TypeError on the '-N' relative form (absolute-number downscale path passes the array and works).
- **Bug (latent) — pm2-ls-minimal.js:15**: `p.basename(l.pm2_env.pm_exec_path.script)` — `.script` on a string = undefined → throws when a process has no name.
- **Fragile args rendering**: pm2-describe.js & Dashboard.js `JSON.parse(args.replace(/'/g,'"'))` — crashes on args containing apostrophes/quotes.
- **Dead code**: pm2-ls.js `containersListing`, `listHighResourcesProcesses` (never called), `checkIfProcessAreDumped` (call commented out); Log.js/devStream `var that = this` unused throughout; API.js `_operate` unused `var fn`; speedList commented-out hint lines.
- **Globals/pollution**: Monit.js sets `Object.size` on the Object built-in; pm2-ls.js module-globals `proc_id` (monotonically grows across renders, used as fake container ids) and `CONDENSED_MODE` mutated at render time; Version.js module-level `EXEC_TIMEOUT` mutated per ecosystem file (leaks across calls); `global._auto_exit` cross-module flag (speedList → Log.stream).
- **Windows traps**: LogManagement.logrotate calls `process.getuid()` — undefined on win32 → TypeError (plus /etc/logrotate.d is Linux-only; macOS also lacks it); API constructor `independent` mode explicitly non-Windows (/tmp path); stdout `_handle.setBlocking` private-API hack flagged `@todo windows connoisseur double check`; exitCli assumes stdout.fd==1/stderr.fd==2.
- **User-input regex**: `_operate` and streamLogs/printLogs build `new RegExp(id.replace(/\//g,''))` from CLI input — strips ALL slashes (regex containing `/` silently altered), JS regex = ReDoS-able; Rust `regex` crate fixes both.
- **Sync I/O in hot paths**: Log.tail `fs.statSync` inside sort comparator + statSync per file; pm2-ls reads DUMP_FILE_PATH synchronously (dead path); flush truncates via sync open/close per file.
- **Vendored licensing**: `lib/tools/multimeter/` (+ embedded `charm/`) = substack's multimeter/charm vendored with **no license headers or LICENSE file** (upstream MIT/X11 — attribution obligation unmet); `fclone`, `vizion`, `pm2-io-agent` also vendored under `modules/` (out of bucket but touched by API.js). `@pm2/blessed` is a maintained fork of unmaintained blessed (MIT). Repo itself AGPL-3.0 — Rust rewrite must keep AGPL or get relicensing consent; vendored MIT deps fine to replace with crates.
- **Race/timing smells**: `update()` fixed 250ms setTimeout before cb; constructor's async KMDaemon.ping populates `gl_interact_infos` — commands may run before it lands (speedList PM2+ banner nondeterministic); Dashboard 300ms blind rerender.
- **`pullAndRestart` is mislabeled** — performs reload, not restart (action:'reload' in both pull variants).
- **`pm2 ls --watch`** setInterval never cleared, callback never returns — process hangs by design, exit only via signal.
