# CLI surface

## Inventory

## Bucket: cli-bins (1832 LOC total)

### /Users/rin/GitHub/pm2/lib/binaries/CLI.js — 1072 LOC [HOT 9c/2y] [TODO L248 commander-patch]
Main `pm2` binary entry. Sets `PM2_USAGE='CLI'`, gates Bun < 1.1.25 (throws, cluster support), `Common.determineSilentCLI()` + `printVersion()`, instantiates `PM2` (lib/API.js), wires PM2ioHandler.

**Pre-parse behaviors (part of compat contract):**
- argv rewrite: bare `log` → `logs` (only left of `--`)
- `--no-daemon` anywhere in argv (left of `--`) → `new PM2({daemon_mode:false})`, daemon runs foreground in-process, then parse
- `startup`/`unstartup` in argv → parse after 100ms `setTimeout`, NO daemon connect (script generation needs no RPC)
- else `pm2.connect()` (auto-spawns daemon!) then parse; `completion` as argv[2] special-cased to tabtab handler
- Every command run: async `pm2.getVersion()` vs local pkg.version → "In-memory PM2 is out-of-date, do pm2 update" warning
- 0 args → usage + exit 1; unknown command (`*` catch-all) → "Command not found" + usage + exit 1
- Exit codes: SUCCESS_EXIT=0, ERROR_EXIT=1 (constants.js:44-45)
- `patchCommanderArg()`: manual workaround for commander 2.x variadic-plus-`--` bug — truncates cmd array at first arg after `--`

**Global options (~65, all commands inherit; commander obj itself passed into API — camelCase property names are the real contract):**
`-v --version` | `-s --silent` | `--ext <extensions>` | `-n --name <name>` | `-m --mini-list` | `--interpreter <i>` | `--interpreter-args <args>` | `--node-args <args>` | `-o --output <path>` | `-e --error <path>` | `-l --log [path]` | `--filter-env [envs]` (repeatable accumulator) | `--log-type <type>` (raw|json) | `--log-date-format <fmt>` | `--time` | `--disable-logs` | `--env <name>` | `-a --update-env` | `-f --force` | `-i --instances <n>` | `--parallel <n>` | `--shutdown-with-message` | `-p --pid <pid>` | `-k --kill-timeout <delay>` | `--listen-timeout <delay>` | `--max-memory-restart <mem>` | `--restart-delay <ms>` | `--exp-backoff-restart-delay <ms>` | `-x --execute-command` | `--max-restarts [count]` | `-u --user <name>` | `--uid <uid>` | `--gid <gid>` | `--namespace <ns>` | `--cwd <path>` | `--hp <home>` | `--wait-ip` | `--service-name <name>` | `-c --cron <pat>` | `-c --cron-restart <pat>` (DUPLICATE -c short) | `-w --write` | `--no-daemon` | `--source-map-support` | `--only <app>` | `--disable-source-map-support` | `--wait-ready` | `--merge-logs` | `--watch [paths]` (repeatable accumulator) | `--ignore-watch <paths>` | `--watch-delay <delay>` | `--no-color` | `--no-vizion` | `--no-autostart` | `--no-autorestart` | `--stop-exit-codes <codes...>` (variadic) | `--no-treekill` | `--no-pmx` | `--no-automation` | `--trace` | `--disable-trace` | `--sort <field:order>` | `--attach` | `--v8` | `--event-loop-inspector` | `--deep-monitoring`

**Full command list (name → API call; per-command options in parens):**
- `start [name|namespace|file|ecosystem|id...]` (`--watch --fresh --daemon --container --dist --image-name [n] --node-version [major] --dockerdaemon`) → container: `pm2.dockerMode(cmd,opts,'distribution'|'development')`; `start -` reads JSON from stdin → `_startJson(...,'restartProcessId','pipe')`; else sequential `forEachLimit(cmd,1)` over args → `pm2.start(script, commander)`; 0 args → default `cst.APP_CONF_DEFAULT_FILE` (ecosystem.config.js); "Script not found"/"NOT AVAILABLE IN PATH" errors → exit 1, else `speedList`
- `trigger <id|proc_name|namespace|all> <action_name> [params]` → pm2.trigger
- `deploy <file|environment>` → pm2.deploy
- `startOrRestart <json>` → _startJson 'restartProcessId'
- `startOrReload <json>` → _startJson 'reloadProcessId'
- `startOrGracefulReload <json>` → _startJson 'reloadProcessId' (identical to startOrReload)
- `pid [app_name]` → getPID
- `create` → pm2.boilerplate() (description is copy-paste bug: "return pid of...")
- `stop <id|name|namespace|all|json|stdin...>` (`--watch`) → sequential pm2.stop
- `restart <id|name|namespace|all|json|stdin...>` (`--watch`) → sequential pm2.restart(script, commander)
- `scale <app_name> <number>` → pm2.scale
- `profile:mem [time]` / `profile:cpu [time]` → pm2.profile('mem'|'cpu')
- `reload <id|name|namespace|all>` → pm2.reload
- `id <name>` → getProcessIdByName
- `inspect <name>` → pm2.inspect
- `delete|del <name|id|namespace|script|all|json|stdin...>` → `-` stdin pipe variant; sequential pm2.delete
- `sendSignal <signal> <pm2_id|name>` → numeric? sendSignalToProcessId : sendSignalToProcessName
- `ping` → pm2.ping (launches daemon if down)
- `updatePM2` / `update` → pm2.update
- `install|module:install <module|git:// url>` (`--tarball --install --docker --v1 --safe [time]`) → pm2.install
- `module:update <module|git://>` (`--tarball`) → pm2.install
- `module:generate [app_name]` → generateModuleSample
- `uninstall|module:uninstall <module>` → pm2.uninstall
- `package [target]` → pm2.package
- `publish|module:publish [folder]` (`--npm`) → pm2.publish
- `set [key] [value]` / `multiset <str>` / `get [key]` / `unset <key>` → config store
- `conf [key] [value]` → BUG: ignores args, calls `pm2.get()`
- `config <key> [value]` → pm2.conf
- `report` → pm2.report
- `link [secret] [public] [name]` (`--info-node [url]`) → linkManagement (PM2 Plus)
- `unlink` → pm2.unlink
- `monitor [name]` / `unmonitor [name]` → monitorState; monitor w/o name → plusHandler
- `open` → openDashboard
- `plus|register [command] [option]` (`--info-node [url] -d --discrete -a --install-all`) → PM2ioHandler.launch; `login` / `logout` → plusHandler('login'|'logout')
- `dump|save` (`--force`) → pm2.dump; `cleardump` → clearDump
- `send <pm_id> <line>` → sendLineToStdin
- `attach <pm_id> [command separator]` → pm2.attach (stdin/stdout attach)
- `resurrect` → pm2.resurrect
- `startup [platform]` / `unstartup [platform]` → pm2.startup/uninstallStartup
- `logrotate` → pm2.logrotate
- `ecosystem|init [mode]` → generateSample (mode = null|simple)
- `reset <name|id|all>` → pm2.reset (counters)
- `describe <name|id>`, `desc`, `info`, `show` → pm2.describe (4 separate command defs, not aliases)
- `env <id>` → pm2.env
- `list|ls`, `l`, `ps`, `status` → pm2.list
- `jlist` → pm2.jlist (raw JSON); `prettylist` → jlist(true); `slist|sysinfos` (`-t --tree`) → pm2.slist
- `monit` → pm2.dashboard; `imonit` → pm2.monit (legacy); `dashboard|dash` → pm2.dashboard
- `flush [api]` → pm2.flush; `reloadLogs` → reloadLogs
- `logs [id|name|namespace]` (`--json --format --raw --err --out --lines <n> --timestamp [fmt] --nostream --highlight [value]`) → default id='all', 15 lines; --nostream → printLogs; --json → Log.jsonStream; --format → Log.formatStream; else streamLogs. `--raw` detected via `cmd.parent.rawArgs.indexOf` (commander-2 hack); timestamp default fmt 'YYYY-MM-DD-HH:mm:ss'
- `kill` → killDaemon → exit 0
- `pull <name> [commit_id]` → pullAndRestart / _pullCommitId; `forward <name>`; `backward <name>` (vizion git ops)
- `deepUpdate` → pm2.deepUpdate
- `serve|expose [path] [port]` (`--port [p] --spa --ftp --basic-auth-username [u] --basic-auth-password [p] --monitor [app]`) → pm2.serve
- `autoinstall` → pm2.autoinstall (undocumented, no description)
- `examples` → static usage text
- `install-otel` / `uninstall-otel` → OtelManager.install/uninstall (fork addition, --trace prereq)
- `completion [install|uninstall]` — not a commander command; intercepted pre-parse → tabtab

`checkCompletion()` (L157-184): completes long/short flags from commander.options; process names (live `pm2.list()` RPC) after stop|restart|scale|reload|delete|reset|pull|forward|backward|logs|describe|desc|show; command names after bare `pm2`.

### /Users/rin/GitHub/pm2/lib/binaries/DevCLI.js — 183 LOC (3c/2y)
`pm2-dev` binary. Isolated daemon at `~/.pm2-dev` (own pm2_home via `PM2.custom`). Env: `PM2_NO_INTERACTION=true`, `PM2_DISCRETE_MODE=true`.
Options: `--raw --timestamp --node-args <a> --ignore [files] --post-exec [cmd] --silent-exec --test-mode --interpreter <i> --env [name] --auto-exit`. Commands: `*` and `start <file|json_file>` → `run()`.
`run()`: forces `watch=true, autostart=true, autorestart=true, restart_delay=1000`; `--ignore` → ignore_watch + always appends 'node_modules'; `--timestamp` → 'YYYY-MM-DD-HH:mm:ss'; starts app, prints banner (fmt.title/field), after 1s `launchBus` → on 'process:event' packet.event=='online' runs `--post-exec` via child_process.exec (output piped unless --silent-exec); `Log.devStream` streams all logs; SIGINT → `pm2.delete('all')` → destroy → exit 0. `autoExit()`: 3s poll of pm2.list, two consecutive polls with 0 online/launching apps → exit 1. 0 args → help + kill pm2 if connected.

### /Users/rin/GitHub/pm2/lib/binaries/Runtime.js — 101 LOC [STALE 2018] [UNTESTED] DEAD CODE
Old pm2-runtime. NOT referenced by any bin (bin/pm2-runtime → Runtime4Docker.js). pm2_home hardcoded `~/.pm3`. Options `--auto-manage --fast-boot --web [port] --secret --public --machine-name --env --watch -i`. References `commander.json`/`commander.format` that are never defined. Superseded entirely by Runtime4Docker.js.

### /Users/rin/GitHub/pm2/lib/binaries/Runtime4Docker.js — 192 LOC [STALE last commit 2024-03]
Real `pm2-runtime` AND `pm2-docker` (both bins require it). "Drop-in replacement Node.js binary for containers" — the no-daemon PID-1 mode.
Options: `-i --instances <n>` | `--secret/--public/--machine-name` (PM2 Plus) | `--no-autostart` | `--no-autorestart` | `--stop-exit-codes <codes...>` | `--node-args` | `-n --name` | `--max-memory-restart <mem>` | `-c --cron <pat>` | `--interpreter <i>` | `--trace` | `--v8` | `--format` (key=val logs) | `--raw` (default) | `--formatted` (|id|app|log) | `--json` (json log lines) | `--delay <seconds>` (default 0) | `--web [port]` (default 9615, cst.WEB_PORT) | `--only <app>` | `--no-auto-exit` (auto-exit ON by default) | `--env [name]` | `--watch` | `--error <path>` (default `/dev/null`) | `--output <path>` (default `/dev/null`) | `--deep-monitoring` | `.allowUnknownOption()`. Commands: `*` and `start <app.js|json_file>`.
**Flow (traced):** `Runtime.instanciate(cmd)` → `PM2.custom({pm2_home: $PM2_HOME||~/.pm2, daemon_mode: PM2_RUNTIME_DEBUG||false})` — daemon runs IN-PROCESS foreground → `connect` → install SIGINT/SIGTERM → `Runtime.exit()` = `pm2.kill()` then process.exit → `startLogStreaming()` (json|format|raw stream of 'all' to stdout — container logging convention; passes `commander.timestamp` which is never defined) → `startApp` after `--delay`*1000ms → `pm2.start(cmd, commander)`; err or 0 apps started → error + exit; `--web` → `pm2.web(port)`; auto-exit → after 4s start `autoExitWorker()`: every 2s (unref'd timer) `pm2.list`, count apps that are NOT pmx_module and status online|launching; 0 online → decrement fail_count (DEFAULT_FAIL_COUNT=3), at 0 → `exit(2)`; any app online resets nothing (fail_count only resets by re-entry with undefined). Exit code contract: 0 normal kill, 1 no-pm2/startup error, 2 auto-exit.

### /Users/rin/GitHub/pm2/bin/pm2, pm2-dev, pm2-docker, pm2-runtime — 3 LOC each
Node shebang shims: pm2→CLI.js, pm2-dev→DevCLI.js, pm2-docker→Runtime4Docker.js, pm2-runtime→Runtime4Docker.js. package.json bin map confirms all four.

### /Users/rin/GitHub/pm2/bin/pm2.ps1 — 3 LOC
PowerShell wrapper: `node $PSScriptRoot/../lib/binaries/CLI.js $args`. Not in package.json bin map — manual Windows convenience.

### /Users/rin/GitHub/pm2/lib/completion.js — 229 LOC [UNTESTED]
Vendored/hacked node-tabtab 0.0.4 (itself derived from npm completion by isaacs). tabtab NOT in package.json deps — fully vendored. Exports: `complete` (main: parses `completion` argv + COMP_CWORD/COMP_POINT/COMP_LINE env; no COMP_* → dump completion.sh to stdout with EPIPE swallow hack for macOS `source <(...)`; `install`/`uninstall` → splice completion.sh between `###-begin-pm2-completion-###`/`###-end-...###` markers into `~/.bashrc`|`~/.zshrc`), `log` (prefix-filtered candidate printer), plus dead exports `isComplete`, `parseOut`, `parseTasks`, and unused private fn `installed`. Shell rc filename derived from `process.env.SHELL.match(/\/bin\/(\w+)/)[1]` — crashes if SHELL unset (Windows), wrong file for fish/nonstandard shells.

### /Users/rin/GitHub/pm2/lib/completion.sh — 40 LOC [STALE 2015] [UNTESTED]
Bash/zsh completion template: bash `complete -o default -F _pm2_completion pm2`; zsh legacy `compctl -K`. Strips `=` and `@` from COMP_WORDBREAKS. Re-invokes `pm2 completion -- "${COMP_WORDS[@]}"` with COMP_* env per keystroke — every TAB spawns full Node CLI (slow), and process-name completion additionally connects to daemon.

## Flags

- **Dead code**: lib/binaries/Runtime.js (unreferenced since bins repointed to Runtime4Docker, last commit 2018, references options it never defines, pm2_home `~/.pm3`); completion.js exports `isComplete`/`parseOut`/`parseTasks` + private `installed()` never called anywhere; Runtime4Docker passes `commander.timestamp` to Log.stream but defines no `--timestamp` option (always undefined); CLI.js commented-out old `flush` block (L860-866).
- **Bugs shipped in surface**: `conf [key] [value]` action ignores args and calls `pm2.get()` — get/set semantics dead (use `config` instead); `create` command description is copy-paste of `pid`'s ("return pid of [app_name] or all"); duplicate `-c` short flag (--cron / --cron-restart) — commander 2.x tolerates, clap won't; `startOrReload` and `startOrGracefulReload` are byte-identical actions.
- **Vendored dependency**: lib/completion.js = node-tabtab 0.0.4 copied in ("hacked from") with PM2 copyright header slapped on; tabtab is MIT, header says "governed by a license that can be found in the LICENSE file" (AGPL-3.0) — relicensing-of-MIT-code smell; original attribution only via comment. Rust rewrite drops it entirely (clap_complete), issue evaporates.
- **Windows/portability traps**: completion.js `process.env.SHELL.match(/\/bin\/(\w+)/)[1]` → TypeError crash when SHELL unset (Windows/cron) or nonstandard path; assumes rc file `~/.<shell>rc`; Runtime4Docker defaults `--error/--output` to `/dev/null` (Windows: NUL — must map); Runtime.js/Runtime4Docker use `process.env.HOME` not os.homedir() → undefined on Windows (USERPROFILE); pm2.ps1 assumes `node` on PATH and duplicates npm's own generated shim; completion.sh EPIPE-swallow hack exists specifically for macOS `source <(pm2 completion)`.
- **Security smells**: `serve --basic-auth-username/--basic-auth-password` and `link [secret] [public]` as argv — visible in `ps`, shell history, and pm2 report output; `pm2 install <git:// url>` executes arbitrary module code as install path; completion install writes to ~/.bashrc (shell-injection surface if name/completer ever attacker-influenced — currently constant 'pm2', low risk).
- **Fragility ported-around, not ported**: `--no-daemon`/`startup` handled by scanning raw argv before parse (position-sensitive, `--` aware); `startup` waits arbitrary 100ms setTimeout before parsing; `pm2.connect()` before parse auto-spawns daemon even for commands that don't need it (e.g. `pm2 examples` boots a daemon); logs `--raw` detected by grepping `rawArgs` due to commander-2 option-collision; commander pinned at 2.15.1 (March 2018) with a manual variadic patch (`patchCommanderArg`) referencing commander issue #475.
- **Behavior quirks to preserve knowingly or break loudly**: `log` argv rewritten to `logs`; bare `pm2` prints usage and exits 1 (not 0); `pm2 start` with no args starts ecosystem.config.js; multi-target start/stop/delete run strictly sequentially (limit 1); auto-exit exit code 2 in runtime mode; version-mismatch warning on every command.
