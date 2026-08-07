# Daemon/Client RPC plane

## Inventory

## Bucket: daemon-rpc — inventory

### lib/Daemon.js (456 LOC) — daemon process entrypoint + RPC/pub server
- `Daemon(opts)`: fields `pub_socket_file` (default `cst.DAEMON_PUB_PORT`), `rpc_socket_file` (`cst.DAEMON_RPC_PORT`), `pid_path` (`cst.PM2_PID_FILE_PATH`), `ignore_signals`.
- `start()`: wraps `innerStart()` in deprecated Node `domain`; on uncaught error prints report then **resurrects by spawning `node $_ update`** (unix: `process.env['_']` = shell-provided invoked-binary path; Windows: `__dirname/../bin/pm2`), exits when child closes.
- `innerStart(cb)`: writes pidfile (`process.pid`), installs signal handlers, binds **pub** (`axon 'pub-emitter'`) and **rep** (`axon 'rep'` + `rpc.Server`) on the two socket files. Each bind → `fs.chmod(sock,'775')` + optional `fs.chown` from `PM2_SOCKET_USER`/`PM2_SOCKET_GROUP` (numeric). `sendReady` fires cb once both flags set, then `process.send({online:true,success:true,pid,pm2_version})` over the spawn IPC channel (this is the client's readiness signal). Default cb prints boot banner (version, home, sockets, worker interval, kill timeout, runtime binary).
- Inner `profile(type,msg,cb)`: uses Node `inspector` to CPU/heap-profile the **daemon itself** for `msg.timeout||5000` ms, writes JSON profile to client-supplied path `msg.pwd`.
- `server.expose({...})` — **30 RPC methods**: `killMe`, `profileCPU`, `profileMEM`, `prepare`, `getMonitorData`, `getSystemData`, `startProcessId`, `stopProcessId`, `restartProcessId`, `deleteProcessId`, `sendLineToStdin`, `softReloadProcessId`, `reloadProcessId`, `duplicateProcessId`, `resetMetaProcessId`, `stopWatch`, `startWatch`, `toggleWatch`, `notifyByProcessId`, `notifyKillPM2`, `monitor`, `unmonitor`, `msgProcess`, `sendDataToProcessId`, `sendSignalToProcessId`, `sendSignalToProcessName`, `ping`, `getVersion`, `getReport`, `reloadLogs`. All but killMe/profile* delegate to `God.*`; uniform signature `(msg, cb)`.
- `close(opts,cb)` (= `killMe`): emit `pm2:kill` on God.bus, kill `system_infos_proc`, close rpc then pub sockets, **SIGQUIT to client pid** (`opts.pid`, unix only), unlink pidfile, `process.exit(0)` after 2ms.
- `handleSignals`: SIGTERM/SIGINT/SIGQUIT→`gracefullExit`; SIGHUP ignored; SIGUSR2→`God.reloadLogs`.
- `gracefullExit`: reentrancy guard `isExiting`; emit `pm2:kill`; `God.dumpProcessList()` (dump.pm2); delete every process serially (`eachLimit(...,1)`); unlink pidfile; exit 0.
- `startLogic()` — event plumbing: God.bus handlers for `axm:action` (append to `pm2_env.axm_actions`, dedup by action_name), `axm:option:configuration` (merge into `axm_options`, name override), `axm:monitor` (Object.assign into `axm_monitor`); then **`God.bus.onAny` bridge → `pub.emit(event, Utility.clone(data))`** forwarding EVERYTHING except `axm:action|axm:monitor|axm:option:setPID|axm:option:configuration` to the pub socket. `Utility.clone` = fclone.
- `require.main === module` guard: sets `process.title` (`PM2_DAEMON_TITLE` override) and starts — this file IS the daemon executable, spawned by Client.
- Tags: [TODO]-adjacent (resurrection hack), tested only indirectly via `test/programmatic/client.mocha.js` + bash e2e.

### lib/Client.js (776 LOC) — CLI/API side: connect, auto-spawn, RPC + bus wrappers
- ctor: conf = `constants.js` (or injected), `daemon_mode` default true, calls `initFileStructure` (mkdir -p `logs/`, `pids/`, `modules/`, create `module_conf.json` `{}`; first-run: VersionCheck phone-home + print motd banner unless `PM2_PROGRAMMATIC`; `touch` timestamp file; `PM2_DISCRETE_MODE` writes touch silently).
- `start(cb)` flow: `pingDaemon` → alive ⇒ `launchRPC`; dead + `daemon_mode:false` ⇒ **in-process** `new Daemon({ignore_signals:true}).innerStart()` + KM interactor; dead + daemon mode ⇒ `launchDaemon` then `launchRPC`. cb gets `{daemon_mode, new_pm2_instance, rpc_socket_file, pub_socket_file, pm2_home}`.
- `launchDaemon(opts,cb)`: spawn `process.execPath [node_args] lib/Daemon.js`, `detached:true`, `windowsHide`, cwd from conf, `stdio:[null, pm2.log fd, pm2.log fd, 'ipc']`, env += `{SILENT, PM2_HOME}`; `PM2_NODE_OPTIONS` splits into node args; `LOW_MEMORY_ENVIRONMENT` adds `--gc-global --max-old-space-size=<totalmem>`; `child.unref()` **skipped when `IS_BUN`**; waits for one IPC `message` (daemon ready), `child.disconnect()`; then `KMDaemon.launchAndInteract` (keymetrics agent) unless `opts.interactor==false` or `PM2_NO_INTERACTION`.
- `pingDaemon(cb)`: axon req connect to rpc.sock; `'reconnect attempt'` event (fires after first failed connect, ~100ms) ⇒ cb(false); `'connect'` ⇒ close, cb(true); `EACCES` ⇒ print `sudo chown` hint (if socket uid==0) and `process.exit(1)`.
- `launchRPC(cb)`: `rpc.Client` over axon req; cb on connect (4ms setTimeout) or error.
- `launchBus(cb)`: axon `'sub-emitter'` connect to pub.sock; cb(err, sub_emitter, sock).
- `disconnectRPC`/`disconnectBus`: close with 200ms destroy-timeout fallback; **bug: `disconnectBus` timeout path reads `Client.sub_sock` (constructor) instead of `that.sub_sock` → destroy never runs** (line 475).
- `executeRemote(method, app_conf, fn)`: string-sniffs method name — contains `stop`/`delete` ⇒ pre-call `stopWatch` RPC; `kill` ⇒ `stopWatch('deleteAll')`; `restartProcessId`+`--watch` argv ⇒ `toggleWatch`. Lazy-connects (`this.start`) if no client. Then `client.call(method, app_conf, fn)`.
- `killDaemon(fn)`: sends `killMe {pid:process.pid}`; unix waits SIGQUIT from daemon; Windows polls `pingDaemon` after 250ms; 3s timeout fallback; then `close()`.
- Query helpers (all client-side filters over `getMonitorData`): `getAllProcess`, `getAllProcessId`, `getAllProcessIdWithoutModules` (filters `pmx_module`), `getProcessIdByName` (name or resolved exec path), `getProcessIdsByNamespace`, `getProcessByName`, `getProcessByNameOrId` (name|exec path|OS pid|pm_id). `notifyGod` → `notifyByProcessId`.
- Tags: [TODO] (`@todo ret err` L47), tested by `test/programmatic/client.mocha.js` (instantiate/start/bus/methods/ping/kill).

### lib/Event.js (37 LOC) — God event emitters
- `God.notify(action_name, data, manually)` → bus `process:event` envelope `{event, manually:bool, process: Utility.formatCLU(data), at: timestamp}`.
- `God.notifyByProcessId(opts, cb)` — RPC-exposed; same envelope from `clusters_db[opts.id]`; errors if id missing/unknown.
- Tags: [UNTESTED] directly.

### constants.js (114 LOC) — global config constants
- Merges `paths.js(process.env.OVER_HOME)` into consts (so `OVER_HOME` env re-roots everything — undocumented).
- Notable: `IS_BUN` (`typeof Bun !== 'undefined'`), `IS_WINDOWS` (`win32||win64||OSTYPE msys/cygwin`), status strings (`online/stopped/stopping/waiting restart/launching/errored/one-launch-status`), `CLUSTER_MODE_ID`/`FORK_MODE_ID`.
- Env-driven knobs: `PM2_ENABLE_GIT_PARSING`, `PM2_OPTIMIZE_MEMORY`, `INSTANCE_NAME|MACHINE_NAME|PM2_MACHINE_NAME`, `KEYMETRICS_SECRET|PM2_SECRET_KEY|SECRET_KEY`, same for PUBLIC, `KEYMETRICS_NODE|PM2_APM_ADDRESS|ROOT_URL|INFO_NODE` (default `root.keymetrics.io`), `EXP_BACKOFF_RESET_TIMER` (30s), `KEYMETRICS_PUSH_PORT` (80), `PM2_RELOAD_LOCK_TIMEOUT` (30s), `PM2_GRACEFUL_TIMEOUT` (8s), `PM2_GRACEFUL_LISTEN_TIMEOUT` (3s), `PM2_CONCURRENT_ACTIONS` (2), `PM2_DEBUG`, `PM2_API_IPADDR` (0.0.0.0), `PM2_API_PORT` (9615), `PM2_WEB_STRIP_ENV_VARS`, `PM2_MODIFY_REQUIRE`, `PM2_WORKER_INTERVAL` (30s), `PM2_KILL_TIMEOUT` (1600ms), `PM2_KILL_SIGNAL` (SIGINT), `PM2_KILL_USE_MESSAGE`, `PM2_PROGRAMMATIC` (auto-true if `pm_id` env set), `PM2_LOG_DATE_FORMAT` (`YYYY-MM-DDTHH:mm:ss`). Misc: `LOGS_BUFFER_SIZE` 8, `WORKER_INTERVAL`, keymetrics `REMOTE_PORT` 41624 / `REMOTE_HOST` s1.keymetrics.io / `SEND_INTERVAL` 1000.

### paths.js (88 LOC) — filesystem layout
- `PM2_HOME` resolution: `$PM2_HOME` → `~/.pm2` → fallback `/etc/.pm2` (no homedir).
- Layout under PM2_HOME: `conf.js`, `module_conf.json`, `pm2.log`, `pm2.pid`, `reload.lock`, `pids/`, `logs/`, `modules/`, `pm2-io-token`, `dump.pm2`, `dump.pm2.bak`, **`rpc.sock`** (DAEMON_RPC_PORT), **`pub.sock`** (DAEMON_PUB_PORT), `interactor.sock`, `agent.log`, `agent.pid`, `agent.json5`.
- Embedded-node detection: `./node` dir next to paths.js ⇒ `BUILTIN_NODE_PATH`/`BUILTIN_NPM_PATH`.
- **Every key env-overridable**: env name = key if it contains `PM2_`, else `PM2_`+key (e.g. `PM2_DAEMON_RPC_PORT`, `PM2_DUMP_FILE_PATH`); `PM2_HOME`/`PM2_ROOT_PATH` excluded.
- Windows: sockets forced to named pipes `\\.\pipe\rpc.sock`, `\\.\pipe\pub.sock`, `\\.\pipe\interactor.sock` — static, ignores PM2_HOME. [TODO] comment acknowledges.

### index.js (12 LOC) — programmatic entry: sets `PM2_PROGRAMMATIC='true'`, exports `new API` singleton + `.custom` class.

### modules/pm2-axon (vendored, ~1100 LOC total) — message socket lib
- `lib/index.js`: type registry `pub-emitter|sub-emitter|push|pull|pub|sub|req|rep` → `axon.socket(type)`.
- `sockets/sock.js` (430 LOC) [TODO]: base Socket (EventEmitter + Configurable settings: `hwm:Infinity`, `identity:String(pid)`, `retry timeout:100`, `retry max timeout:5000`). `pack` = amp-message encode. `connect(addr)`: string addr parsed via URL (tcp://host:port) else treated as path (UDS/pipe); TCP `setNoDelay`; **infinite reconnect** with backoff `retry*1.5` capped at max, emits `'reconnect attempt'` each retry; ignores errno set `ECONNREFUSED/ECONNRESET/ETIMEDOUT/EHOSTUNREACH/ENETUNREACH/ENETDOWN/EPIPE/ENOENT` (emits `'ignored error'`, others `'error'`). `bind(addr)`: net.createServer; **stale-UDS handling**: on `EADDRINUSE` probe-connect to socket; probe `ECONNREFUSED|ENOENT` ⇒ unlink file + re-listen; probe succeeds ⇒ error `'Process already listening on socket'`; other bind errors ⇒ unlink + retry listen. Framing via `amp.Stream` parser per connection.
- `sockets/req.js` (101 LOC): request ids `identity:seq` (`<pid>:<n>`); callback registry; **appends id as final message part**; response pops id, dispatches callback; round-robins across connected socks; unconnected ⇒ `enqueue` (queue plugin, flushed on connect, dropped past hwm — hwm=Infinity so unbounded).
- `sockets/rep.js` (73 LOC): pops trailing id from request parts, emits `'message'(...parts, reply)`; `reply(...)` coerces `args[0] = args[0] || null`, re-appends id, writes if `sock.writable` else drops (`peer went away`).
- `sockets/pub.js`: `send` broadcasts packed frame to all connected socks (skips non-writable); `sendv2(data,cb)` = ack-when-all-flushed variant (unused by Daemon).
- `sockets/sub.js`: client-side regex subscription list; `'*'` → `(.+)`, anchored `^...$`; **NB `onmessage` caches `hasSubscriptions()` at connection time**. `send` throws.
- `sockets/pub-emitter.js` (26 LOC): thin façade, `emit(event, ...args)` = pub.send — wire frame = `[topic-string, ...args]`.
- `sockets/sub-emitter.js` (92 LOC): overrides sub.onmessage; shifts topic part, matches listener regexes, `fn(...wildcardCaptures, ...restArgs)`; `on(event,fn)`/`off(event)`.
- `plugins/queue.js`: buffered offline sends, flush on connect, `drop` event past hwm.
- Wire framing (deps `amp@0.3.1` + `amp-message@0.1.2`, external npm, node_modules absent — format from pinned-version knowledge, Med confidence): AMP frame = 1 meta byte (hi nibble = version 1, lo nibble = argc, **max 15 args**) then per-arg `[u32 BE length][payload]`. amp-message arg codec: `Buffer`→raw bytes; `string`→`"s:"+utf8`; anything else (incl. null; undefined coerced to null)→`"j:"+JSON.stringify`; decode sniffs 2-byte prefix.
- Has own `test/` dir.

### modules/pm2-axon-rpc (vendored, ~200 LOC) — RPC layer on req/rep
- `lib/server.js` (139 LOC): `expose(name,fn)` map; on message `{type:'methods'}` ⇒ reply `{methods:{name:{name,params}}}` (params via `fn.toString()` regex — breaks on arrow fns); `{method, args}` ⇒ validate, push tail callback `cb(err, ...rest)` → reply `{args:[...rest]}` | `{error: err.message|err, stack}`; `fn.apply(null, args)`. Missing method ⇒ `{error:'method "x" does not exist'}`.
- `lib/client.js` (63 LOC): `call(name, ...args, fn)` → send `{type:'call', method, args}`; response `'error' in msg` ⇒ `fn(Error(msg.error) with msg.stack)` else `fn(null, ...msg.args)`. `methods(fn)` introspection.
- Full request wire frame: `[ "j:"+JSON({type:'call',method,args:[app_conf]}), "s:"+"<pid>:<seq>" ]`; response: `[ "j:"+JSON({args:[...]}|{error,stack}), "s:"+same-id ]`.
- No timeouts, no cancellation, no auth, dispatch by string method name. Has own `test/` dir.

### modules/fclone.js (49 LOC) — safe-clone for bus payloads
- Deep clone: Date/Buffer/TypedArray copied; Error→`{name,message,stack}` plus enumerable keys; array-like duck-typing (`length` + `indexOf`); **circular refs → literal string `'[Circular]'`** (ancestor-stack check). Used via `Utility.clone` on every pub-bridge payload + axm merges. Test: `test/programmatic/fclone.mocha.js`.

## Traced flows
1. **Daemon boot**: CLI → `Client.start` → ping fails → spawn detached `node lib/Daemon.js` (stdio→pm2.log, IPC pipe) → daemon writes pidfile, binds pub.sock + rpc.sock (chmod 775, stale-socket recovery in axon bind), exposes 30 methods, bridges God.bus→pub → `process.send({online:true,...})` → client disconnects IPC, connects RPC, optionally boots KM agent.
2. **Client connect**: `pingDaemon` (connect-or-first-retry ⇒ alive/dead) → `launchRPC` (axon req + rpc.Client) → `executeRemote(method, payload, cb)` with lazy auto-start if not connected.
3. **RPC round-trip**: JSON-in-AMP req/rep, per-request id `<pid>:<seq>` as trailing frame part, node-style callback tunneled as `{args}`/`{error,stack}`.
4. **Event bus**: God.bus (EventEmitter2, wildcard, `:` delimiter, maxListeners 1000) → Daemon `onAny` → fclone → pub-emitter frame `[topic, json]` → broadcast to ALL subscribers (no wire-level subscription; sub-emitter filters client-side by regex, `*` wildcard). Topics incl. `process:event` (from Event.js), `process:msg`, `log:out`, `log:err`, `pm2:kill`, `axm:reply`; axm:action/monitor/option:* consumed daemon-side, not forwarded.
5. **Kill**: client `killMe{pid}` → daemon closes sockets, SIGQUIT→client, unlink pidfile, exit; client waits SIGQUIT (unix) / ping-poll (win) / 3s timeout.

## Flags

- **Security: `profileCPU`/`profileMEM` write daemon-generated files to arbitrary client-supplied path (`msg.pwd`, unchecked)** — any socket peer makes the daemon write files as daemon user (lib/Daemon.js L163-213).
- **Security: socket chmod 775 + `PM2_SOCKET_USER`/`PM2_SOCKET_GROUP` chown** — group members get full unauthenticated RPC (start arbitrary processes as daemon user). Document as privilege boundary in rewrite.
- **Security/portability: Windows named pipes are static global names (`\\.\pipe\rpc.sock`)** — cross-user collision + squatting; acknowledged `@todo` in paths.js L81.
- **Fragile: daemon resurrection spawns `node process.env['_'] update`** (Daemon.js L50) — `$_` is shell-dependent; wrong/absent under cron/systemd/exec; can execute unintended binary.
- **Bug (latent): Client.disconnectBus timeout path references `Client.sub_sock` (constructor) not `that.sub_sock`** (Client.js L475) — destroy-on-timeout never executes.
- Dead code: `found_proc` unused in `getAllProcess`/`getAllProcessId`/`getAllProcessIdWithoutModules`; `IS_WINDOWS`/`killDaemon` check `'win64'` (not a real `process.platform` value); commented Travis block in launchDaemon; `PubSocket.sendv2` unused by daemon plane; axon push/pull sockets unused by pm2 core.
- Deprecated Node APIs: `domain` (crash trap), `__proto__` prototype assignment throughout axon.
- Wire quirks to NOT replicate: AMP 15-arg frame limit; rep `args[0] = args[0] || null` coerces falsy first reply part to null; rpc-server `params()` regex throws on arrow-function methods; `SubSocket.onmessage` caches `hasSubscriptions()` at connection time (pm2 unaffected — sub-emitter overrides).
- Race: `Daemon.close` exits 2ms after socket close — RPC reply for `killMe` never reliably delivered; protocol relies on SIGQUIT side-channel instead (unix) — redesign with a proper shutdown ack.
- Licensing: vendored `modules/pm2-axon`, `modules/pm2-axon-rpc`, `modules/fclone.js` carry **no LICENSE files** (upstreams are MIT — attribution missing in this AGPL-3.0 repo); `modules/pm2-io-agent` and `modules/vizion` do have LICENSE. Rust rewrite drops all three anyway.
- Privacy: first-run `VersionCheck` phone-home + motd banner (Client.initFileStructure) and auto `KMDaemon.launchAndInteract` toward `root.keymetrics.io`/`s1.keymetrics.io:41624` if agent configured.
- Bun quirk: `child.unref()` deliberately skipped when `IS_BUN` (Client.js L271) — Bun-specific workaround to preserve; verify against current Bun before porting.
- `fs.chmod(sock, '775')` string form — parsed as octal 0o775; works but write as `0o775` in Rust port.
- Unbounded memory: axon req-socket offline queue + pub hwm default `Infinity`.
