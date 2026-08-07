

---
# bucket:core
## Workspace placement (crate `pm2d` = daemon, `pm2` = CLI elsewhere)

| pm2 file | Rust home | Notes |
|---|---|---|
| lib/God.js | `pm2d::supervisor` (state machine) + `pm2d::registry` | `clusters_db` → `HashMap<ProcId, ProcessEntry>` inside an actor task or `RwLock`; kill the `_old_<id>` string-key hack — reload state becomes an explicit enum field on the entry. `bus` (EventEmitter2 wildcard) → `tokio::sync::broadcast<Event>` with a typed `Event` enum; wildcard subscriptions → match arms |
| God/ForkMode.js | `pm2d::spawn` | `tokio::process::Command`, `.kill_on_drop(false)`, piped stdio, `process_group(0)`/`setsid` (replaces `detached:true` and makes tree-kill exact), `uid()/gid()` via `CommandExt`. Log pipeline: `tokio::io::BufReader` lines → framing (raw/date-prefix via `time` or `chrono`; JSON via `serde_json`) → broadcast + append file (`tokio::fs::File` in append mode; keep `_reloadLogs` = reopen-on-signal for logrotate) |
| God/ClusterMode.js + ProcessContainer*.js | **deleted** | See redesign below |
| God/Methods.js | `pm2d::kill` + `pm2d::registry` | `killProcess` escalation ladder as an async fn: send SIGINT (configurable) → `tokio::time::timeout(kill_timeout)` on child.wait() → SIGKILL survivors. Direct children: `child.wait()` (no polling, exact exit code+signal). Tree survivors: Linux `pidfd_open` or `/proc` scan; macOS `sysinfo`; Windows Job Object handle wait |
| God/Reload.js | `pm2d::reload` | Explicit state machine: SpawnNew → AwaitReady(listen/ready/timeout) → DrainOld(shutdown msg or SIGTERM, GRACEFUL_TIMEOUT) → ReapOld. Keep timeouts/env-merge semantics byte-compatible |
| God/ActionMethods.js | `pm2d::actions` | Each RPC verb = async handler on a typed request enum (replaces method-name-string dispatch); `eachLimit(2)` → `futures::stream::iter(..).for_each_concurrent(2, ..)`; dump file → `serde_json` + tempfile-rename atomic write (delete the backup-restore dance), keep on-disk format readable for migration from JS pm2 dump |
| lib/TreeKill.js | `pm2d::kill::tree` | Unix: `kill(-pgid)` (children spawned with own pgid — exact, race-free, replaces `ps` snapshot); Linux bonus: cgroup-v2 per-app scope + `cgroup.kill`. Windows: Job Objects (`CreateJobObject`/`TerminateJobObject`) instead of `taskkill` — fixes signal-ignoring. `sysinfo` only as fallback for un-grouped externals |
| lib/Watcher.js | `pm2d::watch` | `notify` + `notify-debouncer-full` (replaces chokidar + restarting-flag + watch_delay); one watcher per name-group, not per instance (fixes fan-out); keep `ignore_watch` regex/glob semantics via `globset` |
| lib/ProcessUtils.js | dropped / npm shim | see below |

## Dependency → crate
- chokidar → `notify` (+ `notify-debouncer-full`)
- pidusage → `sysinfo` (cross-platform), `procfs` on Linux for precise CPU%
- eventemitter2 → `tokio::sync::broadcast` + typed enum
- async (timesLimit/eachLimit) → `futures` stream combinators
- dayjs log dates → `time` or `chrono` format strings (must accept pm2's dayjs format tokens or translate)
- debug → `tracing` + `tracing-subscriber` env filter
- vizion (vendored git parser) → `gix` behind a feature flag, or drop (ENABLE_GIT_PARSING already defaults off)
- treekill → own module per above (no crate needed; `nix` for signals/pgid)
- Node IPC channel (`stdio[3]:'ipc'`) → newline-delimited JSON over an extra pipe fd (`command.fd_mappings`/`pre_exec` dup2) or per-child Unix socket; protocol messages preserved: `'shutdown'`, `{type:'log:reload'}`, `process:exception`, `process:msg`, axm action dispatch — keeps `pm2-io`-instrumented Node apps working

## Redesign: what dies with Node cluster (the big one)
Everything ClusterMode/ProcessContainer does exists only because a Node master can (a) `require()` user code inside a child it controls, and (b) share its listening socket among workers. A Rust daemon can do neither. Consequences:
1. **One spawn path.** "cluster mode" becomes N fork-mode instances. Keep injecting `NODE_APP_INSTANCE` (injectVariables slot algorithm) for app compat.
2. **Load-balancing替代:** opt-in `SO_REUSEPORT` via `socket2` (Linux: kernel LB, works; macOS: reuseport ≠ load-balance semantics — document) or true FD passing: daemon binds, sends listener fd via `sendmsg`/SCM_RIGHTS (`sendfd`/`passfd` crate) + small Node shim that calls `server.listen({fd})`. Without a shim, arbitrary apps can't accept a passed fd → SO_REUSEPORT is the pragmatic default; systemd-style `LISTEN_FDS` protocol is the principled one.
3. **Log capture** always daemon-side pipes (fork path today) — stdout monkeypatching gone; Bun console patch obsolete.
4. **`process.title` renaming** gone (child owns its argv); acceptable loss, `pm2 ls` shows names anyway.
5. **In-process features** (process:exception forwarding, pmx metrics/actions, wait_ready via `process.send('ready')`) survive only through the IPC-over-pipe protocol + optional `pm2-io`-compatible npm shim; `ready` should also accept a TCP/HTTP health-check alternative for non-Node apps.
6. **Reload for fork apps** stops being "restart in disguise": with SO_REUSEPORT/FD-passing, hardReload/softReload semantics extend to ALL runtimes, not just Node cluster — a genuine improvement over pm2.

---
# bucket:rpc
## Rust workspace mapping (pm2-rs)

- **`pm2-core` crate**
  - `pm2_core::paths` ← paths.js: `Paths::resolve(env)` struct, `directories`/`std::env::home_dir` replacement via `dirs` crate; keep `PM2_HOME` + per-key `PM2_<KEY>` env overrides as explicit match table (readable > reflective loop). Windows: per-user pipe name `\\.\pipe\pm2-<sha(PM2_HOME or SID)>-rpc` — fixes the acknowledged todo.
  - `pm2_core::config` ← constants.js: one `Config` struct built from env with serde defaults or hand-rolled `from_env()`; `LazyLock<Config>`. Status strings → `enum ProcStatus` with `serde(rename)` to keep JSON compat (`"waiting restart"` etc.).
- **`pm2-ipc` crate** ← pm2-axon + pm2-axon-rpc + amp/amp-message. Replace wholesale:
  - Transport: `tokio::net::UnixListener/UnixStream` on unix; `interprocess` crate (`local_socket`) or `tokio::net::windows::named_pipe` for Windows — one `LocalSocket` abstraction.
  - Framing: `tokio_util::codec::LengthDelimitedCodec` (u32 prefix) + `serde_json` (or `rmp-serde` behind a feature; JSON first for debuggability/compat tooling). Kills the 15-arg AMP limit and the `s:`/`j:` prefix codec.
  - RPC: do NOT pull jsonrpsee/tarpc (HTTP/TLS-oriented, heavy). Hand-roll: `#[derive(Serialize,Deserialize)] enum Request { GetMonitorData{..}, StartProcessId{..}, ... }` + `struct Envelope { id: u64, body: Request }` / `enum Response { Ok(Value...), Err{ message, stack: Option<String>, code } }` — typed dispatch replaces string method names; keep a `LegacyCall{method:String,args:Vec<Value>}` variant only if wire compat with JS clients is a goal (probably not — rewrite owns both ends).
  - Request ids: plain `u64` per connection (the `<pid>:<seq>` scheme exists only because axon multiplexes callbacks; per-connection ids suffice). Add per-call deadline (`tokio::time::timeout`, default ~5s configurable) — pm2 has none.
  - Reconnect/backoff: `backoff` crate or 10-line loop (100ms → ×1.5 → cap 5s, preserve pm2 feel); stale-UDS recovery: on EADDRINUSE, probe-connect, unlink if refused — port this exact algorithm, it is battle-tested.
- **`pm2-daemon` crate (bin)** ← Daemon.js:
  - Daemonization: keep pm2's re-spawn model (spawn own binary with `--daemon`, detached, stdout/err → pm2.log) — portable to Windows, unlike fork/setsid; `std::process::Command` + `CREATE_NEW_PROCESS_GROUP`/`setsid` per-platform. Readiness handshake: replace Node IPC `process.send` with a pipe (pass write-fd/handle to child, child writes `{pid,version}` when both sockets bound) — same both-sockets-bound gate.
  - Signals: `tokio::signal::unix` (SIGTERM/INT/QUIT → graceful dump+exit; SIGUSR2 → reload logs; SIGHUP ignored). Kill-ack SIGQUIT-to-client: replace with an RPC reply/shutdown event — signaling the client pid is a hack; keep 3s client timeout.
  - Drop: `domain` resurrection (Rust: panic=abort in daemon + client-side "daemon died → offer restart"; or supervise via systemd/launchd unit), inspector self-profiling (`profileCPU/MEM` → feature-gated `pprof` crate or drop).
- **`pm2-client` crate (lib)** ← Client.js: async `Pm2Client { connect_or_spawn() }`, typed wrappers for the 30 methods, query helpers as filters over `Vec<ProcessInfo>`. Kill `executeRemote`'s method-name sniffing — make watch-stop explicit in each command's implementation.
- **Event bus** ← Event.js + Daemon.startLogic + pub/sub-emitter: in-daemon `tokio::sync::broadcast::Sender<BusEvent>` (typed enum: `ProcessEvent{event,manually,process,at}`, `LogOut`, `LogErr`, `ProcessMsg`, `Pm2Kill`, ...); replaces EventEmitter2 wildcard bus. Wire side: per-subscriber forward task draining a broadcast receiver → length-delimited JSON `{topic, payload}` frames; add wire-level subscribe message (client sends topic patterns on connect, server filters) — strictly better than pm2's broadcast-everything + client regex, keep `*` glob semantics (`globset` crate). Slow-subscriber policy: bounded channel + drop-oldest + drop-count event (pm2: unbounded, silent).
- **fclone** → gone; serde on typed structs. Error payloads: explicit `SerializableError{name,message,stack}` type.
- **Dep swaps in this bucket**: axon/amp → tokio + tokio-util + serde_json; async/forEach+eachLimit → `futures::stream::iter(..).for_each_concurrent(1..n)`; debug → `tracing`; ansis prefixes → `owo-colors`/`anstyle`; EventEmitter2 → `tokio::sync::broadcast`; domain → panic hook; Node inspector → `pprof` (optional).
- **Callback→async**: every `(msg, cb)` God method becomes `async fn(&self, req) -> Result<T, RpcError>`; rpc server task per connection: read frame → dispatch → write reply (serialize on daemon state via `Arc<Mutex<State>>` or actor/mpsc — actor recommended, God is effectively single-threaded today which hides races).

---
# bucket:apiCore
## Rust workspace mapping (pm2-rs)

- **`pm2-api` crate** (lib): `API` class → `Pm2 { client: RpcClient, config, paths }` struct with async methods. Kill the mixin pattern — Extra/Deploy/Version/LogManagement prototype-patching becomes plain modules/impl blocks (`pm2-api/src/{lifecycle,logs,versioning,scale,describe}.rs`). Kill dual cb-or-exitCli mode: API methods return `Result<T, Pm2Error>`; only the CLI binary maps errors to exit codes + printed output. `speedList` split into pure "fetch snapshot" (api) + "render" (CLI).
- **Target resolution** (`'all'`/name/id/`/regex/`/namespace, module rules) → one `enum ProcessSelector { All, Name(String), Id(u32), Regex(regex::Regex), Namespace(String) }` parsed once, resolved by one function — replaces the copy-pasted fallback chains in `_operate`, `streamLogs`, `printLogs`, `describe`. regex via `regex` crate (bonus: no ReDoS, unlike JS `new RegExp` on user input).
- **`_startScript` 4-step series with `cb(true)` abort** → sequential async fn with early `return Ok(...)`; `_startJson` env-merge → typed `EcosystemConfig` (serde) + explicit merge fn; eachLimit concurrency → `futures::stream::iter(...).buffer_unordered(n)` or `tokio::task::JoinSet` + `Semaphore`.
- **`pm2-cli` crate** (bin): commander → `clap` (derive). `exitCli` fd-drain hack unnecessary — Rust stdout flush + `std::process::exit`.
- **`pm2-tui` crate**: Dashboard.js + Monit.js → single `ratatui` + `crossterm` app (list/logs/metadata/metrics panes = ratatui `List`/`Paragraph`/`Table` widgets; gradient fn → `ratatui::style::Color::Rgb`; 300ms interval → event-driven redraw on bus message / tick). Drop Monit entirely; drop vendored multimeter+charm+blessed wholesale.
- **`pm2-ux` module** (in cli crate): cli-tableau → `comfy-table` (handles ANSI width correctly — deletes the `fitColumn` workaround class of bugs) or `tabled`; ansis/chalk → `owo-colors`/`anstyle`; width-adaptive full/condensed/mini stays as explicit layout enum keyed on `terminal_size`; `bytesToSize` → `humansize` or hand-rolled for byte-compat; `timeSince` → `humantime` (or keep bug-compat); dayjs → `jiff`/`chrono`; passwd uid→username → `uzers` crate (replaces tools/passwd.js parse).
- **Log streaming**: bus EventEmitter `log:*` → `tokio::sync::broadcast::Sender<LogEvent>` from the daemon-connection task; tail → `tokio::fs` reverse block reader (drop the lines*200-bytes guess); stream/jsonStream/formatStream → one consumer with an `enum LogFormat { Pretty, Raw, Json, Logfmt }`; NDJSON via serde_json.
- **flush** → `OpenOptions::new().write(true).truncate(true)`; logrotate template → keep as text template gen, gate `cfg(target_os = "linux")` + euid check via `nix`.
- **Versioning (Version.js)** if kept: vizion → `gix` or `git2`; `exec('cd p;cmd')` → `tokio::process::Command::new(...).current_dir(repo_path)` with arg vector (kills injection); module-global mutable EXEC_TIMEOUT → per-call timeout param with `tokio::time::timeout`.
- **Other dep swaps seen here**: async(eachLimit/series) → tokio; debug → `tracing`; fclone → `Clone` derive; json5 → `json5` crate (needed: INTERACTION_CONF fallback parse); dump/DUMP_FILE_PATH JSON → serde.
- **Redesigns**: `--watch` list mode's leaked 900ms setInterval → tokio interval task with ctrl-c cancellation; reload lock (`Common.lockReload`) → file lock via `fd-lock` with RAII guard (fixes unlock-on-panic); scale's recursive add/rm closures → simple loop, fixing the '-N' bug; Docker higher-pm_id heuristic → explicit config flag.

---
# bucket:apiExtra
## Rust workspace placement (pm2-rs)

- **Startup.js** → `pm2-init` crate (or `pm2_cli::startup`). Templates: `minijinja`/`askama` or plain `format!` with typed context struct {pm2_path, node_path?, user, home, service_name}. Init detection: `which` crate + `/run/systemd/system` existence check (more reliable than binary probe). Command execution: `std::process::Command` argv arrays — kill `sexec` shell-string concat entirely. Root check: `nix::unistd::geteuid`. Platform matrix: systemd (+`network-online.target` variant), launchd (plist via `plist` crate), openrc, freebsd rc.d, openbsd rc.d; feature-gate; add native Windows service (`windows-service` crate) — pm2 never had it.
- **dump/resurrect** → daemon-side `pm2_core::snapshot`: serde_json Vec<AppSpec>, atomic write (`tempfile` + rename) replaces the backup-file juggling; resurrect = diff-by-name against running set, spawn missing.
- **Configuration.js + lib/Configuration store** → `pm2_config::kv`: serde_json file + `fs4` file lock; scoped keys `module:key` as typed (namespace, key) tuple; module-restart side effect via daemon RPC.
- **schema.json** → `pm2_config::app_spec::AppConfig` struct: `#[serde(alias = "...")]` for every alias listed in inventory; newtypes `MemSize` (sbyte: `\d+(G|M|K)?` FromStr), `UpDuration` (stime: `\d+(h|m|s)?`), `ExecMode` enum {Fork, Cluster} accepting `_mode` suffix; `env_*` pattern keys via `#[serde(flatten)]` HashMap + prefix filter; keep JSON-schema export (schemars) for docs/editor completion. This struct IS the compat contract — port every key.
- **interpreter.json** → `phf::phf_map!` static in `pm2_core::interpreter`; keep .ts/.tsx→bun behavior behind runtime probe (bun on PATH?).
- **Extra.js keepers** → `pm2_cli` subcommands over daemon RPC: JSON-RPC-over-axon → length-prefixed JSON (or MessagePack) over UDS with `tokio`; EventEmitter bus (`log:*`, `axm:reply`) → `tokio::sync::broadcast` channels with typed event enum; `trigger` barrier (process_wait_count) → await N replies with `tokio::time::timeout`. `report` → `sysinfo` crate one-shot collector. `dashboard`/`monit` → `ratatui` + event-driven refresh (subscribe, not 400/800ms poll loops). `attach` → `rustyline`/raw stdin forward.
- **Serve.js** → separate `pm2-serve` bin crate: `axum` + `tower-http::services::ServeDir` (gets traversal safety, ranges, precompression free) or minimal `hyper`; `mime_guess` replaces the 170-entry table; SPA fallback via fallback service; dir listing via askama template; basic auth: `subtle::ConstantTimeEq` compare, creds via file/stdin not env; drop APM injection. Config still env-var-compatible (PM2_SERVE_*) for CLI parity.
- **Deploy.js** → drop; if demanded later: `pm2-deploy` crate with `openssh`/`russh`, porting pm2-deploy's shell protocol (setup/update/revert/curr/prev/exec/list).
- **Containerizer.js** → drop (no Rust counterpart needed).
- **Modules/** → `pm2-modules` crate, redesigned around TAR concept: fetch via `reqwest` (replaces spawned wget), unpack via `tar` + `flate2` crates with path-sanitization (replaces GNU tar spawn, fixes macOS BSD-tar breakage), registry entry in config store, autostart on daemon boot. NPM/bun module support only if targeting Node apps: shell out to detected npm/bun with argv arrays. LOCAL.js internal modules: no port. flagExt.js: replace with `globset` include/exclude on the watcher (notify crate) — kills the sync fs walk.
- **pm2-plus/** → no port. Extension point instead: optional `pm2-metrics` feature exposing Prometheus endpoint (`prometheus` crate) and/or OTLP (`opentelemetry`); `monitorState` semantics (per-proc monitoring toggle) fold into core proc flags.

## JS dep → crate swaps seen in this bucket
- ansis/chalk → `owo-colors`/`anstyle`
- async/forEachLimit,eachLimit,tryEach,parallel → `futures::stream::iter(...).buffer_unordered(n)` / plain for-await / sequential loop
- dayjs → `jiff` or `chrono` (log_date_format uses dayjs tokens — need token translation for compat)
- cli-tableau → `comfy-table`
- blessed (Dashboard/Monit) → `ratatui`
- promise.min.js vendored polyfill → n/a (delete)
- @pm2/js-api + pm2-io-agent → dropped with pm2-plus
- pm2-deploy → dropped or russh-based rewrite
- which.js → `which` crate; sexec → `std::process::Command`/`tokio::process`
- global fetch/FormData (TAR publish, auth) → `reqwest::multipart` (only if kept)
- chokidar (watch_options passthrough in schema) → `notify` + `globset`; schema's `watch_options` object must be re-specced (chokidar-specific keys can't passthrough)

---
# bucket:cli
## Rust workspace mapping (pm2-rs)

- **Crate `pm2-cli`** — single binary crate, bins via `[[bin]]`: `pm2` (main), `pm2-runtime` (+ hardlink/alias `pm2-docker`), optionally `pm2-dev` — or better: multi-call single binary dispatching on argv[0], plus `pm2 dev` / `pm2 runtime` subcommands so separate bins become thin aliases. Modules: `cli/args.rs` (global opts struct), `cli/commands/*.rs` (one per command group: start, process_ops, logs, modules, plus, startup, config, serve), `cli/runtime.rs` (container mode), `cli/dev.rs`.
- **commander 2.15.1 → clap v4 derive.** Global options → `#[arg(global = true)]` on shared struct flattened into subcommands (matches pm2's "every flag valid everywhere" model). `--no-*` flags → `ArgAction::SetFalse` paired ids or `Option<bool>` with `overrides_with`. `--watch [paths]` / `--filter-env [envs]` accumulators → `ArgAction::Append` + optional value. Variadic `--stop-exit-codes <codes...>` → `num_args(1..)`. `--` passthrough (patchCommanderArg hack) → `last = true` / `trailing_var_arg` — clap handles natively, delete the hack.
- **Duplicate `-c` (--cron vs --cron-restart): clap rejects duplicate shorts at build time.** Decide: `-c` binds `--cron-restart`, `--cron` becomes long-only `alias`. Behavior identical (both feed same field today).
- **The real contract is the camelCase option→API property mapping** (commander object passed wholesale into `pm2.start` etc. — `maxMemoryRestart`, `killTimeout`, ...). In Rust: one `StartOptions` struct with serde field renames, consumed by pm2-core config builder — makes the implicit contract explicit and testable.
- **Completion → `clap_complete`**: static generation for bash/zsh/fish/powershell/elvish replaces completion.sh + tabtab entirely. Dynamic process-name completion (today: live `pm2.list()` RPC on TAB) → `clap_complete::CompleteEnv` dynamic completer with a value hint that queries daemon socket with short timeout, degrade silently if daemon down. Keep `pm2 completion [shell]` printing to stdout; drop rc-file mutation (`completion install`) or keep as convenience append with the same begin/end markers.
- **pm2-runtime redesign**: no-daemon mode = run the daemon event loop in-process on the tokio runtime instead of the fork/detach path — same `pm2-core` code, different entry. Signals via `tokio::signal::unix` (SIGINT/SIGTERM → graceful kill → exit 0). Add PID-1 zombie reaping (`nix::sys::wait::waitpid(WNOHANG)` on SIGCHLD) — Node/libuv did this implicitly for its children only; a Rust PID 1 must reap all. Auto-exit worker → tokio interval task counting non-module online apps, preserve fail_count=3 / 2s / exit-code 2. `--delay` → `tokio::time::sleep`. Log streaming to stdout (raw/json/key=val/formatted) → `tracing` or hand-rolled formatter over broadcast channel from daemon log bus.
- **pm2-dev redesign**: separate pm2_home namespace (`~/.pm2-dev`), watch via `notify` crate (replaces chokidar), post-exec via `tokio::process::Command`, 'online' event hook via tokio broadcast channel subscription (replaces `launchBus` EventEmitter + 1s setTimeout race), auto-exit as interval task.
- **Sequential `forEachLimit(_, 1)` loops** (start/stop/restart/delete multi-arg) → plain `for` + `.await`; `--parallel <n>` → `futures::stream::iter(...).buffer_unordered(n)`.
- **stdin `-` JSON pipe** (start/delete) → read stdin to EOF → serde_json. Keep.
- **Version-drift check** (local vs in-memory daemon version) → include in daemon handshake response instead of extra RPC per command.
- **`startup` 100ms setTimeout + `--no-daemon` argv-scan hacks** → obsolete: make daemon connection lazy/per-command (only commands needing RPC connect; startup/unstartup/completion/examples never do). Improves on pm2, preserves observable behavior.
- **Dep replacements**: commander→clap4+clap_complete; ansis/chalk→owo-colors+anstream (NO_COLOR-aware); debug→tracing+EnvFilter; async(forEachLimit)→tokio/futures; semver Bun-gate→delete (irrelevant); tabtab(vendored)→clap_complete; child_process.exec→tokio::process.
- **Cross-refs other buckets**: `pm2.dockerMode` (start --container/--dist) lives in API bucket — CLI just routes; Log.{stream,jsonStream,formatStream,devStream} → pm2-cli log formatter module over daemon bus; OtelManager (install-otel) is Node-ecosystem-specific — Rust equivalent questionable, defer/drop pending API-bucket verdict.

---
# bucket:aux
## Rust workspace mapping

**Crate layout suggestion**: `pm2-core` (config/model), `pm2-daemon` (God-side), `pm2-cli` (CLI-side), `pm2-web` (http api, feature-gated or subcommand).

- **Common.js** → split three ways:
  - `pm2-core::config::ecosystem` — file detection + parse. `serde_json` (strict, NOT JS-eval), `json5` crate for lenient JSON, `serde_yaml` for .yml/.yaml. **`.config.js/.cjs/.mjs` cannot be require()'d from Rust** — redesign: spawn `node -p 'JSON.stringify(require(path))'` (bun fallback) subprocess and parse stdout; document that JS configs need node in PATH. Extension match must become endsWith (fixes substring bug).
  - `pm2-core::config::normalize` — prepareAppConf/verifyConfs/mergeEnvironmentVariables as pure `AppConfig → Result<ResolvedApp>` functions (currently mutation + mixed Error-or-value returns; make it typed). Cron validation: `croner` crate (Rust port exists, same author lineage) — keeps pattern-compat with JS croner used at runtime.
  - `pm2-cli::output` — printOut/printError/silent gating → `tracing` + a quiet flag; kill console monkeypatching. cropToWidth → `console::truncate_str` or unicode-width.
- **tools/Config.js + API/schema.json** → `pm2-core::config::schema`: typed `AppConfig` struct, `#[serde(alias)]` for camelCase aliases (generate from snake_case via macro or build.rs), custom `Deserialize` for sbyte (`byte-unit` crate) and stime (`humantime`), shlex crate for string→args split. Validation errors as `thiserror` enum, no shared mutable `_errors`.
- **Utility.js** → `pm2-daemon::util`: extendMix `'null'`-delete → explicit `Option<Option<T>>` patch type; startLogging → `tokio::fs::OpenOptions::append`; overrideConsole → `tracing` subscriber emitting to broadcast channel (EventEmitter bus → `tokio::sync::broadcast`); getCanonicModuleName → `pm2-core::modules::canonic_name` w/ unit tests ported from utility.mocha.js; generateUUID → `uuid::Uuid::new_v4` (fixes non-crypto RNG).
- **Configuration.js** → `pm2-core::config_store`: one impl (async), `serde_json::Value` tree, dotted/colon key path parser (port quote handling + `all` wipe), 4-space pretty print for file compat, **add advisory file lock** (`fs4`/`fd-lock`) fixing the read-modify-write race. Prototype-pollution guard obsolete (no prototypes) — keep key-parser tests.
- **HttpInterface.js** → `pm2-web` (axum or plain hyper): keep `GET /` payload shape for dashboard compat; default bind **127.0.0.1**, `--host/--port` flags, strip env by default (`--with-env` opt-in), optional bearer token. Run as `pm2 web` subcommand spawning a managed process (parity) or daemon feature.
- **OtelManager.js** → drop runtime-npm-install. Daemon self-telemetry: `opentelemetry` + `opentelemetry-otlp` crates compiled in. Child Node app tracing: ship a JS bootstrap file, inject via `NODE_OPTIONS=--require`, require user to install otel packages in their app (or `pm2 install-otel` runs npm in a dedicated dir, never as side effect of `pm2 start --trace`).
- **Worker.js** → `pm2-daemon::worker`: `tokio::time::interval(30s)` tasks; domain → task-per-tick with `catch_unwind`/JoinHandle error logging; cron registry → `HashMap<PmId, croner job handle>` or `tokio-cron-scheduler`; maxMemoryRestart + backoff-reset logic ports 1:1; re-entrancy guard → `AtomicBool` or skip via interval semantics.
- **VersionCheck.js** → drop, or `pm2-cli::update_check` opt-in with `reqwest` + `semver` crate, no telemetry payload beyond version.
- **Watcher.js** → `pm2-daemon::watcher`: `notify` + `notify-debouncer-full`; port default ignore (dot-entries + node_modules) as glob/regex (`globset`), watch_delay as debounce duration, per-pm_id watcher map, restarting-guard becomes debouncer + in-flight restart flag that RE-CHECKS after restart completes (fixes dropped-event gap).
- **tools crate replacements** (one line each): which→`which`; open+xdg-open→`open`; prompt→`dialoguer`; passwd→`uzers`/`nix::unistd::{User,Group}`; isbinaryfile→`content_inspector`; json5→`json5`; copydirSync→`fs_extra`; deleteFolderRecursive→`std::fs::remove_dir_all`; find-package-json→10-line loop; treeify→`termtree`; fmt/multimeter/charm/promise.min/IsAbsolute→delete (ratatui covers monit UI elsewhere).
- **SysMetrics.js** → `sysinfo` crate behind a `HostMetrics` trait producing the same axm_monitor map (metric names `CPU Usage`, `RAM Usage`, `net:rx_5:<if>`, `fs:use:<mount>`, `Disk Reads/Writes`, `CPU Temperature=-1` sentinel) so `pm2 ls` rendering + `getSystemData` RPC stay wire-compatible. sysinfo also gives Windows metrics for free (JS version is Linux/macOS only).
- **templates/ + motd** → `rust-embed` or `include_str!` in `pm2-cli::startup` / `pm2-cli::init`; placeholder substitution stays string-replace. Ecosystem tpl emit unchanged (JS files run under node).
- **Cross-cutting redesigns**: callback-hell (async/waterfall, eachLimit, eachSeries) → async/await + `futures::stream::iter().buffer_unordered(1)`; EventEmitter bus → `tokio::sync::broadcast`; mixed return-Error-or-value (prepareAppConf, verifyConfs) → `Result`; fclone deep-copy culture → ownership/Clone derive.

---
# bucket:eco
## Rust workspace placement

- `modules/pm2-io-agent/` → **no crate**. If SaaS compat ever wanted: `crates/pm2-agent` behind `--features cloud`, tokio-tungstenite + reqwest(proxy) + serde_json; axon RPC to daemon replaced by the workspace's native IPC. Recommended: ship `pm2 link` as a stub that prints "not supported".
- `modules/pm2-io-bpm/` → **no Rust port**. Daemon side: `crates/pm2-daemon/src/axm.rs` — serde enums for `axm:monitor|axm:action|axm:reply|axm:option:configuration|process:exception` IPC packets so JS apps using @pm2/io / tx2 keep working (`pm2 describe` custom metrics, `pm2 trigger` actions). Child side stays a tiny JS shim file shipped as an asset (the ProcessContainer equivalent), or is cut entirely in v1.
- `modules/vizion/` → `crates/pm2-vcs` (or `pm2-core::versioning`). Implement exactly like the fork does: `std::process::Command::new("git")` with arg vectors (no shell), 5s/60s timeouts via tokio, `LC_ALL=C`, `GIT_TERMINAL_PROMPT=0`. Avoid git2/libgit2 — `remote update` needs credentials/ssh agent behavior identical to user's git; shelling out preserves it and matches current semantics 1:1. Keep the callback shapes as a serde struct `VersioningMeta`.
- `packager/` → delete; replace with `cargo-dist` or nfpm/`cargo-deb`+`cargo-generate-rpm` in CI. Ship: static musl binary, deb/rpm/apk, Homebrew tap. systemd unit: prefer `Type=notify` + `sd_notify` (daemon is Rust now) over forking+resurrect; keep `pm2 startup` generator semantics.
- `pres/` → keep in repo root as `assets/`, no crate.
- `types/index.d.ts` → `crates/pm2-config`: `StartOptions`→`#[derive(Deserialize)] AppConfig` (every field incl. cron, exp_backoff_restart_delay, stop_exit_codes, namespace, increment_var, instance_var, filter_env, max_memory_restart human suffixes); ProcessStatus enum; Platform enum for startup scripts. Publish npm shim `pm2-compat` re-exporting the .d.ts over a thin CLI/socket client if programmatic-API compat is wanted.

## Dependency audit — all 22 runtime deps → Rust

| dep | used by | Rust replacement |
|---|---|---|
| @pm2/blessed 0.1.81 | lib/API/Dashboard.js (`pm2 monit` TUI) | **ratatui** + crossterm |
| @pm2/js-api 0.8.1 | lib/API/pm2-plus/* (link/register/monitor CLI) | drop with SaaS; else reqwest |
| @pm2/pm2-version-check | lib/VersionCheck.js (phone-home update check + docker detect) | drop, or opt-in check against GitHub releases via reqwest |
| amp 0.3.1 / amp-message | modules/pm2-axon framing (vendored) | tokio_util::codec custom codec ONLY if JS-client wire compat needed; otherwise dies with axon (daemon RPC redesign: serde+length-prefixed JSON or gRPC/unix) |
| ansis 4.0.0-node10 | colors everywhere in lib/ | owo-colors or anstyle |
| async 3.2.6 | lib/API*, Modules, God flows | native async/await — disappears |
| chokidar 3.6.0 | lib/Watcher.js (`watch:true`) | **notify** + notify-debouncer-full (match ignore globs via globset) |
| cli-tableau 2.0.1 | pm2 ls / describe / plus tables | comfy-table or tabled |
| commander 2.15.1 (2018!) | 5 CLI binaries | **clap v4** derive; replicate `pm2 start <script> -- args` passthrough |
| croner 4.1.97 | lib/Worker.js cron_restart, Common.js validation | **croner** crate (same-author Rust port, same pattern dialect) — safest for compat; else `cron` |
| dayjs 1.11.15 | log timestamps, `log_date_format` (moment tokens), uptime fmt | chrono/jiff + a moment-format-token translator (moment `YYYY-MM-DD HH:mm` ≠ strftime — must shim for config compat) |
| debug 4.4.3 (+override) | everywhere | **tracing** + EnvFilter; map `DEBUG=pm2:*` |
| eventemitter2 6.4.9 | God bus, agent transporter (wildcard events) | tokio::sync::broadcast with typed event enum; wildcard subscriptions become match arms |
| fast-json-patch 3.1.1 | agent WS_JSON_PATCH status diffs | drop with agent; else `json-patch` crate |
| js-yaml 4.3.0 | lib/Common.js (YAML ecosystem files) | serde_yml (serde_yaml is archived) |
| pidusage 4.0.1 | lib/God/ActionMethods.js (cpu/mem per pid) | **sysinfo** (cross-platform incl. macOS/Windows); procfs direct on Linux for hot path |
| pm2-deploy 1.0.2 | lib/API/Deploy.js (`pm2 deploy`) | Defer; shell out to ssh like today, or russh later |
| proxy-agent 6.5.0 | agent HTTP/WS proxy | drop with agent; else reqwest proxy support |
| semver 7.7.2 | Modules/NPM, VersionCheck, CLI | **node-semver** crate (node range syntax `^ ~ ||` differs from cargo semver) |
| tx2 1.0.5 | **nothing in lib/ — examples/send-msg/t2.js only** | drop from manifest now, even pre-rewrite |
| ws 8.21.0 | agent WebsocketTransport | drop with agent; else tokio-tungstenite |
| eventemitter2/axon/axon-rpc/fclone (vendored in modules/, other bucket) | daemon RPC | full redesign: JSON-RPC over UDS with serde, or tarpc; fclone (cycle-safe clone) unnecessary with serde |

## Redesign notes
- Callback pyramids (InteractorClient.launchOrAttach → ping → launchRPC → kill → daemonize) → plain async fn chains.
- Dynamic RPC method generation (`client.methods()` reflection in PM2Client/InteractorClient) → static typed trait; kills a whole failure class.
- Agent-as-separate-process + mutual watchdog (agent resurrects pm2, pm2 launches agent) → unnecessary in Rust; if any cloud/export component survives, make it a tokio task inside the daemon.
- bpm's require-hook monkey-patching (shimmer over http/net) has no Rust analog and shouldn't: expose daemon-side metrics (see missing) instead.
- vizion parent-dir walk on ENOTGIT lives in God.js (caller), not vizion — keep that split: `pm2-vcs::analyze` returns Err(NotARepo), supervisor walks up.

---
# bucket:tests
### Test workspace layout (pm2-rs)
- `crates/pm2-core/src/**` — unit tests inline `#[cfg(test)]`: port fclone (obsolete — serde handles it; keep circular-ref guard cases), Utility, Common config-candidate detection, json_validation (serde + validator), Configuration KV incl. pollution/ReDoS regression inputs, path_resolution, id uniqueness.
- `crates/pm2-cli/tests/e2e/` — integration tests replacing `test/e2e/*.sh`, one module per old script group (`cli.rs`, `logs.rs`, `process_file.rs`, `internals.rs`, `misc.rs`). Crates: `assert_cmd` + `predicates` (CLI invocation, exit codes — replaces `spec`/`right-exit-code.sh`), `tempfile` (per-test PM2_HOME — replaces docker tmpfs trick AND enables parallelism without containers), `serde_json` asserts over `pm2 jlist` (replaces grep-prettylist `should` helper — count online/stopped/restart_time from typed structs).
- `crates/pm2-daemon/tests/` — port god/cluster/reload/graceful/signals/treekill against daemon API directly.
- `tests/compat/` (workspace-level) — the golden compat contract (below), gated behind `--features node-compat`, requires node in PATH; keep `test/fixtures/` JS apps verbatim as corpus + add pure-shell/binary fixtures so core lifecycle tests run without Node installed.
- IPC event contract (`bus.*.spec`) → golden-file snapshot tests: serialize daemon events to JSON, compare with committed snapshots (`insta` crate). EventEmitter bus → `tokio::sync::broadcast`; the bus specs become subscriber-side assertions.

### Runner/infra replacements
- mocha + .mocharc (bail/retries/timeout) → **cargo-nextest**: per-test process isolation (kills the "pm2 kill between files" reset dance), `retries` policy in `.config/nextest.toml` only for the explicitly-flaky compat tests, per-test timeouts.
- unit.sh/e2e.sh/include.sh/windows.sh/docker-parallel.sh bash orchestration → `cargo nextest run` (parallelism, bail, timing report all built in).
- sleep-based sync (`sleep 0.3` before every assert) → event-driven waits: subscribe to daemon broadcast, `tokio::time::timeout`; signals/exp_backoff timing tests → `tokio::time::pause` deterministic clocks (this is what makes the CI-excluded timing suites CI-runnable).
- treekill tests → `nix` crate (process groups, SIGTERM-trap fixtures as tiny sh scripts); Windows equivalent via Job Objects — finally testable since integration tests are native.
- serve.sh → `reqwest` asserts; http_interface → axum-based `pm2 web` + reqwest.
- Docker matrix: keep for **runtime compat only** (Node 18/20/24, Bun w/ bun→node symlink trick from test/Dockerfile) — Rust binary is hermetic, so per-test containers become unnecessary; one container per matrix cell running the full compat suite.

### CI (GitHub Actions)
- Jobs: `cargo fmt --check` + `clippy -D warnings` + `cargo nextest run` on {ubuntu, macos, windows} × {stable, MSRV}; `cargo-llvm-cov` coverage upload; compat-matrix jobs (docker, Node 18/20/24 + Bun) running `tests/compat`; release job cross-compiling.
- Keep 30-min timeout + push/PR triggers. Port docker-parallel's EXCLUDED knowledge as `#[ignore]`/cfg attributes with reasons in comments, not a parallel shell list.

### Crate substitutions relevant to this bucket
- mocha/should → built-in `#[test]` + nextest; `should` bash helper → jlist serde asserts; `insta` for event/output snapshots; `assert_cmd`/`predicates` for CLI; `tempfile` for isolated homes; `nix` for signals/proc trees; `reqwest` for HTTP asserts; `wait-timeout`/tokio timeouts replacing bash `sleep`.