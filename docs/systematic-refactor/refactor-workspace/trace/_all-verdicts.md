

---
# bucket:core
### lib/God.js
- Verdict: Keep semantics (prepare/executeApp/handleExit/injectVariables); Drop writeExitSeparator; Rewrite structure entirely
- Evidence: [COMPLEX] executeApp ~220 lines of nested callbacks+once-listeners; restart/backoff semantics (min_uptime×max_restarts window, unstable_restarts, exp backoff ×1.5 cap 15s) are the product's core contract; tested via god.mocha.js + e2e
- Confidence: High

### lib/God/ClusterMode.js
- Verdict: Drop (Node-cluster-specific); Keep only the message-relay semantics
- Evidence: [UNTESTED by name] entire file exists because Node `cluster.fork` exists; a Rust daemon cannot use it. JSON-stringified env workaround and execArgv plumbing are Node artifacts. The msg→bus relay contract (typed vs raw packets, node_version) must survive in whatever IPC replaces it
- Confidence: High

### lib/God/ForkMode.js
- Verdict: Keep semantics — this becomes THE only spawn path in Rust
- Evidence: interpreter resolution, scalar-env flattening, daemon-side pipe log capture (JSON/date-format variants), pid file, _reloadLogs reopen — all portable and well-shaped for tokio::process. Duplicated IPC relay w/ ClusterMode (comment admits it) → unify
- Confidence: High

### lib/God/Methods.js
- Verdict: Keep semantics (killProcess/processIsDead/resetState/registry); Rewrite death-detection mechanism
- Evidence: kill escalation (SIGINT → poll kill_retry_time → SIGKILL after kill_timeout → timeout error) and shutdown_with_message are documented behavior. Poll-by-kill(pid,0) has pid-reuse ABA race; Rust owns its children so waitpid/pidfd is strictly better
- Confidence: High

### lib/God/Reload.js
- Verdict: Keep semantics (user-visible reload contract); Rewrite mechanism — impossible as-is without Node cluster
- Evidence: `_old_<id>` key juggling is hacky but the observable contract (new up before old dies; soft = old gets 'shutdown' + drain window; timeouts 3000/8000) is what `pm2 reload` means. Zero-downtime depends on cluster master's shared listen socket → Rust needs SO_REUSEPORT or FD passing. 5 test files reference reload
- Confidence: High

### lib/God/ActionMethods.js
- Verdict: Keep semantics for process lifecycle verbs; Rewrite dispatch; Drop/fix ProcessName watch paths
- Evidence: [COMPLEX] 909 LOC; stopWatch/toggleWatch/startWatch ProcessName branches are live bugs (object indexed by array → silent no-op); method-name-string dispatch is an RPC-layer smell; dumpProcessList backup dance → atomic write; getReport leaks full env
- Confidence: High

### lib/ProcessContainer.js
- Verdict: Drop — cannot exist in a Rust rewrite
- Evidence: [COMPLEX] whole file is in-process Node injection (stdout monkeypatch, require.main forgery, module._load). Salvage the *contract*: log framing, process:exception reporting, 'shutdown'/'log:reload' message handling → move daemon-side or into optional npm shim
- Confidence: High

### lib/ProcessContainerFork.js
- Verdict: Drop; optionally reincarnate as tiny npm shim package
- Evidence: only jobs are pmx injection + title + node_version + main-module fake; a Rust daemon spawning `node app.js` directly loses nothing users depend on except pmx metrics/actions
- Confidence: Med (pmx-dependent users would notice)

### lib/ProcessContainerBun.js
- Verdict: Drop
- Evidence: ~200 LOC copy-paste of ProcessContainer.js + Bun console patch; both reasons to exist (Node cluster, in-process wrap) vanish in Rust; Bun apps become plain fork spawns
- Confidence: High

### lib/ProcessContainerForkBun.js
- Verdict: Drop
- Evidence: 31-line shim, same story as ProcessContainerFork
- Confidence: High

### lib/TreeKill.js
- Verdict: Keep semantics (kill whole tree bottom-up, return killed pids); Rewrite with proper OS primitives
- Evidence: snapshot-based `ps` walk has spawn race; win32 path ignores the signal entirely (/F always). Rust: process groups/setsid on Unix (detached spawn already implies it), Job Objects on Windows, cgroup kill on Linux. Has dedicated test (treekill.mocha.js)
- Confidence: High

### lib/ProcessUtils.js
- Verdict: Drop injectModules (Node-side pmx bootstrap); Keep isESModule only if a Node shim ships
- Evidence: [UNTESTED]; pure child-side Node concerns; ESM detection irrelevant when daemon just execs the interpreter
- Confidence: High

### lib/Watcher.js
- Verdict: Keep semantics (watch → debounce → restart); Rewrite with notify + real debouncer; fix restart-by-name fan-out
- Evidence: dead+buggy disableAll; per-instance watchers each restarting the whole name-group is O(N²) restart pressure; `restarting` flag + watch_delay is a hand-rolled debounce
- Confidence: High

---
# bucket:rpc
### lib/Daemon.js
- Verdict: Keep semantics
- Evidence: [TODO]-hack (env `$_` resurrection), tested only indirectly; boot ritual, pidfile, signal semantics, 30-method RPC surface, readiness handshake, kill/graceful-exit sequences are the compat contract. Drop: domain resurrection, inspector self-profiling.
- Confidence: High

### lib/Client.js
- Verdict: Keep semantics
- Evidence: [TODO] L47, has mocha test; ping→auto-spawn→connect state machine and query helpers are core UX ("first pm2 command boots daemon"). Rewrite as typed async client; drop method-name string-sniffing in executeRemote.
- Confidence: High

### lib/Event.js
- Verdict: Keep semantics
- Evidence: [UNTESTED]; 37 LOC; `process:event` envelope `{event, manually, process, at}` is consumed by `pm2 logs`/agents — wire-compat matters.
- Confidence: High

### constants.js
- Verdict: Keep semantics
- Evidence: 5 commits/2y (borderline churn); env-var surface = documented user contract; keymetrics-specific consts can defer.
- Confidence: High

### paths.js
- Verdict: Keep semantics
- Evidence: [TODO] (Windows pipe naming); on-disk layout is compat-critical (dump.pm2, logs/, pids/, socket names, PM2_* env overrides). Fix the Windows pipe todo in the rewrite.
- Confidence: High

### index.js
- Verdict: Keep semantics
- Evidence: 12 LOC; programmatic-API entry contract (`require('pm2')` singleton).
- Confidence: High

### modules/pm2-axon
- Verdict: Rewrite (drop wire format)
- Evidence: vendored, no LICENSE file, [TODO]×3, deprecated idioms (`__proto__`, comma-operator arg juggling), 15-arg AMP limit, hwm=Infinity unbounded queues, infinite silent reconnect; only ~4 socket types actually used (req/rep/pub-emitter/sub-emitter). Keep only its *behavioral* niceties: stale-UDS detection, reconnect-with-backoff.
- Confidence: High

### modules/pm2-axon-rpc
- Verdict: Rewrite (replace protocol)
- Evidence: string-dispatch RPC, `fn.toString()` param introspection, no timeouts/auth/error codes; semantics trivially re-expressible as typed request/response enums.
- Confidence: High

### modules/fclone.js
- Verdict: Drop
- Evidence: exists only to JSON-sanitize arbitrary JS object graphs (circular→`'[Circular]'`); Rust typed structs + serde make it structurally unnecessary. Preserve one behavior: Error→`{name,message,stack}` shaping in event payloads.
- Confidence: High

---
# bucket:apiCore
### lib/API.js
- Verdict: Keep semantics / Rewrite
- Evidence: churn 5/2y, [COMPLEX] (`_startJson` ~280 LOC, `_operate` ~277 LOC, deep callback nesting throughout); this IS pm2's contract — name/id/'all'/regex/namespace resolution order, JSON-vs-script dispatch, env-merge rules, --update-env immutability, module-restart-only rule all must port exactly (heavily covered by test/programmatic/*). Structure (mixin monkey-patching, `cb(true)` series-abort, cb-or-exitCli dual mode) must NOT port.
- Confidence: High

### lib/API/Log.js
- Verdict: Keep semantics
- Evidence: e2e-tested output formats (prefix coloring, json/logfmt streams) are user-visible contract; tail heuristic (lines*200 bytes) worth replacing with proper reverse line reader.
- Confidence: High

### lib/API/LogManagement.js
- Verdict: Keep semantics (flush/reloadLogs/streamLogs), Refactor printLogs into streamLogs (90-LOC copy-paste), Defer logrotate
- Evidence: flush.mocha.js + e2e logs cover behavior; logrotate is Linux-root-only /etc writer — questionable in Rust core, better as docs/subcommand generating config text.
- Confidence: High

### lib/API/Monit.js
- Verdict: Drop (merge into new ratatui dashboard)
- Evidence: [UNTESTED]; legacy pm2-htop view on vendored multimeter/charm; Dashboard.js supersedes it functionally; `Object.size` built-in pollution; two TUIs for one job.
- Confidence: High

### lib/API/Dashboard.js
- Verdict: Keep semantics / Rewrite (ratatui)
- Evidence: [UNTESTED] TUI but the 4-pane layout + live log feed + metrics is the flagship `pm2 dash` UX; blessed-specific markup (`{green-fg}`) and 300ms full-rerender loop discarded; fragility: `processes[this.list.selected]` unchecked, per-process loop redundantly re-renders selected pane N times.
- Confidence: High

### lib/API/Version.js
- Verdict: Defer (or Drop from v1)
- Evidence: git pull/backward/forward via vendored vizion; shell-injection-prone `cd path;cmd` exec; niche feature (pm2 pull); modern deploys don't mutate a live checkout. If kept: `git2`/`gix` + std::process with arg vectors.
- Confidence: Med

### lib/API/UX/pm2-ls.js
- Verdict: Keep semantics / Rewrite
- Evidence: [HOT] 7/2y (fork actively invests here: width-adaptive full/condensed/mini layouts, fitColumn ANSI workaround); table content contract worth keeping; `containersListing`, `listHighResourcesProcesses`, `checkIfProcessAreDumped` = dead code, drop.
- Confidence: High

### lib/API/UX/pm2-describe.js
- Verdict: Keep semantics
- Evidence: `pm2 show` field set is user contract; straightforward port; note `JSON.parse(args.replace(/'/g,'"'))` fragile args rendering (crashes on args containing apostrophes).
- Confidence: High

### lib/API/UX/pm2-ls-minimal.js
- Verdict: Keep semantics
- Evidence: [STALE] 2019 but load-bearing: it's the non-TTY default (speedList pipes here); latent bug `pm_exec_path.script` (string.script=undefined) when name missing.
- Confidence: High

### lib/API/UX/helpers.js
- Verdict: Keep semantics
- Evidence: trivial pure formatters; fix `timeSince` `>1` off-by-one and bytesToSize trailing-space quirk deliberately or keep bug-compatible for diffability.
- Confidence: High

---
# bucket:apiExtra
### lib/API/Startup.js
- Verdict: Keep semantics
- Evidence: [COMPLEX]; `pm2 startup/save/resurrect` is headline core value; template+enable-commands model is sound; e2e-tested. Recommend porting systemd/launchd/openrc/rc.d tiers; upstart/systemv/smf/oldsystem = Defer/Drop (dead platforms).
- Confidence: High

### lib/API/Configuration.js
- Verdict: Keep semantics (reduced)
- Evidence: [STALE]; thin kv-config CLI; needed only if module system ported; known `multiset` undefined-var bug shows low usage of that path.
- Confidence: Med

### lib/API/Extra.js
- Verdict: Keep semantics (split)
- Evidence: mixed bag. Keep: getPID/env/ping/getVersion, sendSignal*, sendLineToStdin/attach, trigger/custom-actions, report, serve wrapper, monit+dashboard (→ratatui). Drop: boilerplate (`pm2 create`), autoinstall, remote/remoteV2 (Keymetrics), inspect + profile (V8/Node-runtime-specific; meaningless for arbitrary binaries). Two live bugs (`conf.ERROR_EXIT`, undefined `cmd`) show low-traffic paths.
- Confidence: High

### lib/API/Containerizer.js
- Verdict: Drop
- Evidence: broken require path (`../../tools/prompt` → nonexistent) crashes interactive flow = unused in practice; stale `keymetrics/pm2` base image; docker build/run wrapping out of process-manager scope.
- Confidence: High

### lib/API/Deploy.js
- Verdict: Drop (or Defer to separate crate)
- Evidence: [UNTESTED]; all logic in external bash-based pm2-deploy@1.0.2; CI/CD superseded this workflow; only ~30 lines of real glue.
- Confidence: Med

### lib/API/Serve.js
- Verdict: Keep semantics
- Evidence: [HOT] (7 commits, fork actively added dir-listing); popular convenience (`pm2 serve --spa`); small well-defined contract (env-var config, SPA fallback, traversal guard, basic auth, 404.html). Drop the pm2.io APM HTML injection + agent.json5 read.
- Confidence: High

### lib/API/schema.json
- Verdict: Keep semantics (verbatim contract)
- Evidence: [STALE]=stable; the app-config compatibility surface; validated by json_validation.mocha.js; Rust port must honor every key+alias+regex+default. Drop-candidates within: trace/v8/event_loop_inspector/deep_monitoring/pmx/automation/io/post_update (pm2.io APM knobs).
- Confidence: High

### lib/API/interpreter.json
- Verdict: Keep semantics
- Evidence: [STALE]=stable; 10-entry map incl. fork's .ts/.tsx→bun; trivial.
- Confidence: High

### lib/API/Modules/ (index, Modularizer, NPM, TAR, LOCAL, flagExt)
- Verdict: Defer + Rewrite-reduced
- Evidence: [HOT] dir but ecosystem-coupled: NPM path assumes npm/bun at runtime and Node module code (pm2-logrotate etc.); LOCAL.js internal modules (v8-profiler-node8, gc-stats) are dead-upstream Node-runtime add-ons → Drop. TAR path is the portable concept worth re-designing (archive→unpack→register→autostart). flagExt.js → Drop, replace with watcher globs.
- Confidence: Med (need decision: support existing Node pm2 modules or not)

### lib/API/ExtraMgmt/Docker.js
- Verdict: Drop
- Evidence: [STALE] 2019; 30-line docker passthrough tied to sysinfo container pseudo-ids; niche.
- Confidence: Med

### lib/API/pm2-plus/ (PM2IO, link, helpers, process-selector, auth-strategies, pres)
- Verdict: Drop
- Evidence: SaaS coupling to pm2.io/keymetrics (hardcoded OAuth client ids, id.keymetrics.io endpoints, vendored pm2-io-agent); plaintext password prompt flow; null-deref bug in both auth strategies; ASCII ad motd. Replace with generic metrics-export extension point. `monitorState` (process-selector) is the only piece with reusable semantics.
- Confidence: High

---
# bucket:cli
### lib/binaries/CLI.js
- Verdict: Keep semantics (command surface = compat contract; internals rewritten)
- Evidence: [HOT] 9c/2y, [TODO] commander-patch hack; heavily e2e-tested (test/e2e/cli/* ~30 scripts); every command/flag enumerated above is user-facing API. Pre-parse quirks (log→logs, --no-daemon scan, startup 100ms delay) are behavior, not accident — keep observable effect, drop the hacks.
- Confidence: High

### lib/binaries/DevCLI.js
- Verdict: Keep semantics
- Evidence: tested (test/e2e/binaries/pm2-dev.sh); small, clear feature (isolated ~/.pm2-dev daemon, forced watch, post-exec hook, auto-exit). Worth porting as `pm2 dev` subcommand rather than separate bin.
- Confidence: High

### lib/binaries/Runtime.js
- Verdict: Drop
- Evidence: [STALE 2018] [UNTESTED]; zero references — bin/pm2-runtime points at Runtime4Docker.js; references undefined options; pm2_home `~/.pm3` junk.
- Confidence: High

### lib/binaries/Runtime4Docker.js
- Verdict: Keep semantics
- Evidence: [STALE by commit date but load-bearing]; tested (test/e2e/binaries/pm2-runtime.sh, test/e2e/docker.sh, docker-parallel.sh); this IS container mode — PID-1 foreground daemon, stdout log streaming, /dev/null default log files, auto-exit code 2. Exit-code + logging contract must survive exactly.
- Confidence: High

### bin/pm2, pm2-dev, pm2-docker, pm2-runtime
- Verdict: Drop (replaced by Cargo [[bin]] targets / multi-call binary)
- Evidence: 3-line node shims; churn is version-bump noise.
- Confidence: High

### bin/pm2.ps1
- Verdict: Drop
- Evidence: exists only because runtime is node; native Windows .exe makes it moot; not even in package.json bin.
- Confidence: High

### lib/completion.js + lib/completion.sh
- Verdict: Replace-with-crate (clap_complete)
- Evidence: [UNTESTED], vendored tabtab 0.0.4 from 2015, dead exports, SHELL-parse crash on Windows, mutates user rc files. Keep only two semantics: (1) `pm2 completion` family exists, (2) dynamic process-name completion after stop/restart/delete/logs/etc.
- Confidence: High

---
# bucket:aux
### lib/Common.js
- Verdict: Keep semantics (core of it) / Rewrite structure
- Evidence: [HOT][COMPLEX] — ecosystem parsing + app normalization is THE load-bearing config path; every alias/default/env-merge rule here is observable behavior users depend on (cmd→script, fork→fork_mode, space-in-script→bash -c, log path defaults, filter_env). But parseConfig's JS-eval of .json, NVM auto-install, lsc/coffee support = drop. Silent-mode console monkeypatching = redesign.
- Confidence: High

### lib/Utility.js
- Verdict: Keep semantics (subset)
- Evidence: extendMix `'null'`-string-deletes protocol, getCanonicModuleName, startLogging append-streams, overrideConsole bus re-emit are real daemon behavior. extend/clone/getDate/UUID die in Rust (ownership/std/uuid crate).
- Confidence: High

### lib/Configuration.js
- Verdict: Keep semantics
- Evidence: pm2 set/get/unset + module config feed off it; dotted/colon key splitting w/ quotes + `all` wipe + file format (module_conf.json, 4-space JSON) must stay compatible. Sync/async duplication collapses to one impl. Proto-pollution guard moot in Rust but key-parse tests port.
- Confidence: High

### lib/HttpInterface.js
- Verdict: Rewrite (keep endpoint shape, fix security)
- Evidence: single GET / JSON snapshot worth keeping for compat; 0.0.0.0 default bind + CORS * + env-var dump + res.send crash bug = must not port as-is.
- Confidence: High

### lib/OtelManager.js
- Verdict: Rewrite/Redesign
- Evidence: fork-new; runtime `npm install` into PM2 root is a supply-chain + root-perms smell and blocks CLI. Rust daemon gets otel natively (opentelemetry crate); child-process Node instrumentation needs NODE_OPTIONS bootstrap design instead.
- Confidence: Med

### lib/Worker.js
- Verdict: Keep semantics
- Evidence: max_memory_restart poll, exp-backoff reset, cron-restart registry, sysmetrics cadence, version-check schedule — all observable daemon behavior. No direct unit test (grep hits are fixtures) → [UNTESTED]. Deprecated `domain` → tokio task isolation.
- Confidence: High

### lib/VersionCheck.js
- Verdict: Drop (or opt-in rewrite)
- Evidence: [STALE][UNTESTED]; silent daily telemetry (os/uptime/nodev/docker) to pm2 server via @pm2/pm2-version-check. Docker-detection helpers trivially reimplementable if update-check is kept as opt-in.
- Confidence: High

### lib/Watcher.js
- Verdict: Keep semantics
- Evidence: watch→pm_cwd fallback, default ignore regex (dotfiles+node_modules), watch_delay, restarting-guard debounce, disable-on-kill. `disableAll` splice-on-object bug — don't port. Tested.
- Confidence: High

### lib/tools/Config.js
- Verdict: Keep semantics / Rewrite as typed deserialization
- Evidence: schema.json validation incl. camelCase alias generation, sbyte/stime units ("100M", "30s"), string→array shlex split — user-visible CLI/ecosystem behavior. Stateful `_errors` non-reentrancy dies with serde-based rewrite.
- Confidence: High

### lib/tools/SysMetrics.js
- Verdict: Replace-with-crate (sysinfo), keep output shape
- Evidence: fork-authored, well-designed (execFile-only, no shell), tested — but sysinfo crate covers cpu/ram/net/disk/fs natively cross-platform. Keep the axm_monitor `{name:{value,unit}}` snapshot shape + metric names for `pm2 ls` renderer compat.
- Confidence: High

### lib/tools/which.js → Replace-with-crate (`which`) — shelljs-derived, [TODO]; PATHEXT handled by crate. High.
### lib/tools/sexec.js → Replace: tokio::process::Command w/ `sh -c` where startup scripts need shell. High.
### lib/tools/open.js + xdg-open → Replace-with-crate (`open`/`opener`); drop 21KB vendored shell script; SUDO_USER re-exec logic keep if `pm2 monitor` signup flow survives. High.
### lib/tools/prompt.js → Replace-with-crate (dialoguer). High.
### lib/tools/passwd.js → Replace-with-crate (uzers / nix getpwnam) — file parsing breaks on macOS Directory Services + LDAP/NSS. High.
### lib/tools/isbinaryfile.js → Replace-with-crate (content_inspector) — semantics: interpreter='none' for binaries must survive. High.
### lib/tools/json5.js → Replace-with-crate (json5). Only used for agent.json5. High.
### lib/tools/fmt.js → Drop (println!/comfy-table). High.
### lib/tools/treeify.js → Drop or termtree/ptree crate (module display tree). Med.
### lib/tools/multimeter/ + charm/ → Drop — legacy `pm2 monit` bars; ratatui dashboard replaces (other bucket). High.
### lib/tools/promise.min.js → Drop — Promise polyfill, dead weight since Node 0.12. High.
### lib/tools/copydirSync.js → Replace-with-crate (fs_extra::dir::copy). High.
### lib/tools/deleteFolderRecursive.js → Drop (std::fs::remove_dir_all). High.
### lib/tools/find-package-json.js → Rewrite 15-line walk-up loop w/ serde_json. High.
### lib/tools/IsAbsolute.js → Drop — [STALE] dead code, zero lib/ usages; std Path::is_absolute. High.

### lib/templates/
- Verdict: Keep semantics (assets)
- Evidence: systemd/launchd/openrc/rcd/smf/upstart init scripts + logrotate + ecosystem scaffolds are the `pm2 startup`/`pm2 init` product surface; placeholder substitution (%USER%, %HOME_PATH%, %PM2_PATH%) ports verbatim. Ecosystem .tpl emit stays JS (users run under node) — add TOML/YAML variants.
- Confidence: High

### lib/motd
- Verdict: Keep (asset), edit content
- Evidence: first-run banner; pm2.io upsell lines optional in fork.
- Confidence: High

---
# bucket:eco
### modules/pm2-io-agent/
- Verdict: Drop (feature-gate at most)
- Evidence: entire module exists to feed pm2.io SaaS (root.keymetrics.io); [COMPLEX] TransactionAggregator 670 LOC; churn 6/2y (Bun tweaks only); blast radius into core is small and clean — exactly 3 files: `lib/Client.js` (2 call sites: launchAndInteract on daemon boot, no-daemon boot), `lib/API.js` (3: ping on construct → `gl_is_km_linked`, disconnectRPC on exitCli, launchAndInteract after `pm2 update`), `lib/API/pm2-plus/link.js` (3: link/unlink/info) — all already guarded by PM2_NO_INTERACTION/conf-absent no-ops. Dropping also removes deps ws, proxy-agent, fast-json-patch and the pm2-plus CLI's @pm2/js-api. Cloud protocol is proprietary; reimplementing it in Rust has no value without the SaaS.
- Confidence: High

### modules/pm2-io-bpm/
- Verdict: Keep semantics of the IPC protocol only; Drop the module as Rust code
- Evidence: runs INSIDE user's Node/Bun process (injected by all 4 ProcessContainer entrypoints) — cannot be Rust. The daemon-side contract worth keeping: `axm:monitor`/`axm:action`/`axm:reply`/`axm:option:configuration`/`process:exception`/`human:event` packets over child IPC, consumed by God/Daemon and shown in `pm2 describe`/`pm2 trigger`. Rust daemon must parse these; the JS shim (or user-installed @pm2/io / tx2) keeps emitting them. Vendored copy itself: standalone/websocket transport is dead code, `utils/transactionAggregator.js` unreferenced, OTel tracing lazy-requires peer deps not in package.json.
- Confidence: High

### modules/vizion/
- Verdict: Keep semantics
- Evidence: recently hardened in this fork (execFile, hex-validated revisions, no shell); small (~300 LOC); backs user-visible features `pm2 pull/backward/forward`, `versioning` field in status, `--no-vizion`; e2e-covered. Output shape (revision/comment/unstaged/branch/ahead/prev_rev/next_rev) is asserted by e2e.
- Confidence: High

### packager/
- Verdict: Rewrite (from scratch, keep only the systemd/packaging intent)
- Evidence: [STALE] 0 commits/2y, last touch 2022; targets EOL distros (trusty, wheezy, el/5), APKBUILD pinned to v2.7.2, debian control demands nodejs>=6.12.2 contradicting engines>=18; entire flow is "tarball of JS + nodejs dep", meaningless for a static Rust binary. Keep: systemd unit semantics, `pm2` system user, /etc/default env file idea.
- Confidence: High

### pres/
- Verdict: Drop (from Rust workspace; images stay with whatever README needs them)
- Evidence: [STALE] marketing PNGs + a cheatsheet markdown; zero code.
- Confidence: High

### types/index.d.ts
- Verdict: Keep semantics (as API contract spec), Drop as artifact
- Evidence: authoritative enumeration of programmatic API + StartOptions fields (~50) the Rust CLI/config parser must accept; hand-written, already drifts (duplicate `dump`, `Proc` vs actual pm2_env). In Rust workspace it becomes the ecosystem-config schema + CLI surface checklist; optionally regenerate a .d.ts for an npm compat shim.
- Confidence: High

### package.json
- Verdict: Replace-with-crate(s) — Cargo workspace manifest
- Evidence: [HOT] 66 commits/2y (version bumps + dep bumps); full dep→crate mapping in rust_map; `tx2` dep is dead (examples-only).
- Confidence: High

### examples/
- Verdict: Defer
- Evidence: skim-only per brief; useful later as e2e fixtures for Rust port (cluster-http, ecosystem-file, esm, run-php-python-ruby-bash cover the interpreter matrix); not wired into tests today.
- Confidence: Med

---
# bucket:tests
### test/programmatic/ (core mocha suite)
- Verdict: Keep semantics
- Evidence: [HOT] (issue_* regression files added by fork, otel suites 2x churn); defines the behavioral contract for lifecycle, cluster, reload, dump/resurrect, env, config KV, security regressions. Port as Rust integration tests against pm2-core API.
- Confidence: High

### test/interface/ (bus specs)
- Verdict: Keep semantics
- Evidence: only executable spec of the IPC event wire contract (process:event/log/exception/human:event) that pm2-io agents depend on; must survive any Rust IPC redesign as golden snapshot tests.
- Confidence: High

### test/e2e/ (shell suite)
- Verdict: Keep semantics / Rewrite
- Evidence: behaviors are the CLI compat contract (exit codes, pid files, regex ops, serve, logs, resurrect fallback); mechanism (sleep+grep prettylist, retry-once) is flaky-by-design — rewrite as Rust integration tests asserting on `pm2 jlist` JSON with event-driven waits.
- Confidence: High

### test/e2e/include.sh + unit.sh + e2e.sh
- Verdict: Rewrite
- Evidence: [HOT]; bash orchestration + retry layers replaced wholesale by cargo-nextest; unit.sh currently broken (nonexistent `test/interface/sysmetrics.mocha.js` path) proving it is unmaintained vs CI runner.
- Confidence: High

### test/docker-parallel.sh + test/Dockerfile
- Verdict: Keep semantics
- Evidence: [HOT] fork-built; per-test container isolation + runtime matrix (Node 18/20/24, Bun) is the right shape for a Rust pm2 that still manages Node/Bun apps; parallelism/bail/retry logic itself → nextest.
- Confidence: Med

### test/windows.sh
- Verdict: Rewrite
- Evidence: [HOT]; exists only because bash e2e can't run on Windows; Rust integration tests are natively cross-platform — replace with full suite on windows-latest, keep its exclusion knowledge (signal delivery, timing tests) as #[cfg]-gated tests.
- Confidence: High

### .github/workflows/node.js.yml
- Verdict: Keep semantics / Rewrite
- Evidence: [HOT] 17 commits; matrix idea (3 Node versions + Bun + Windows) worth keeping as app-runtime compat jobs; add cargo build/test/clippy/fmt jobs + macOS.
- Confidence: High

### .mocharc.js
- Verdict: Drop
- Evidence: [STALE]; mocha-specific (bail, retries:2); replaced by nextest profile.
- Confidence: High

### test/parallel.js
- Verdict: Drop
- Evidence: superseded by docker-parallel.sh, unreferenced by package.json/CI.
- Confidence: High

### test/helpers/
- Verdict: Drop
- Evidence: apps.js [STALE] 2016, forks deleted `lib/Satan.js`, zero references; plan.js trivial (assertion counting is free in Rust).
- Confidence: High

### test/pm2_check_dependencies.sh + test/benchmarks/
- Verdict: Drop
- Evidence: [STALE] 2018; unreferenced by any runner or CI; benchmarks unwired with committed result file.
- Confidence: High

### test/fixtures/
- Verdict: Keep semantics
- Evidence: fixture apps (echo/http/signal-trap/throw/leak, ecosystems, interpreters) are the test corpus a Rust pm2 still needs since it manages Node/Bun/Python apps; port near-verbatim; prune dead ones (ecosystem.json5 unreferenced, fixtures/git embedded repo only used by disabled versioning tests).
- Confidence: High

### test/programmatic/sys_infos.mocha.js
- Verdict: Drop
- Evidence: [STALE] 2019; requires deleted `lib/Sysinfo/SystemInfo.js`; superseded by sysmetrics.mocha.js.
- Confidence: High

### Dead-in-CI mocha set (client, conf_update, flagExt, flush, internal_config, module_configuration, module_tar, user_management, version)
- Verdict: Keep semantics (revive in Rust)
- Evidence: all in docker-parallel EXCLUDED list → zero CI executions; behaviors (flush, module tarball install, uid/gid spawn, module config) are real features that must get CI-run tests in the rewrite.
- Confidence: Med

### signals.js + exp_backoff_restart_delay.mocha.js
- Verdict: Keep semantics (priority revive)
- Evidence: excluded from Docker CI as "timing-dependent" → the kill_timeout/backoff contract is currently CI-unverified on every platform; deterministic clocks (tokio time pause) fix this in Rust.
- Confidence: High

### Vendored module tests (pm2-axon, pm2-axon-rpc, pm2-io-agent, pm2-io-bpm)
- Verdict: Defer
- Evidence: axon/axon-rpc tests document the wire protocol the Rust IPC replaces — consult during protocol design, don't port; bpm/io-agent tests matter only if the in-process metrics agent is ported.
- Confidence: Med