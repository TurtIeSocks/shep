# Trace — pm2 v7.0.3 (Node.js) base project

Traced 2026-08-07 via 9 parallel agents (function-level on <500-line files, flow-level on giants). Full per-bucket detail in [trace/](trace/) — this file is the index + load-bearing findings.

**Repo:** `/Users/rin/GitHub/pm2` — 53k LOC JS total; core `lib/` = 14.6k; rest is vendored `modules/` (pm2-axon, pm2-axon-rpc, pm2-io-agent, pm2-io-bpm, vizion, fclone), vendored `lib/tools/` one-offs, tests, packaging. AGPL-3.0. Active fork: Bun support, OTel, dir-listing serve, hardened vizion.

## Bucket index

| Bucket | File | Covers |
|---|---|---|
| Supervisor core | [trace/core.md](trace/core.md) | God.js, God/*, ProcessContainer×4, TreeKill, ProcessUtils, Watcher |
| RPC plane | [trace/rpc.md](trace/rpc.md) | Daemon.js, Client.js, Event.js, constants, paths, pm2-axon(-rpc), fclone |
| API core | [trace/apiCore.md](trace/apiCore.md) | API.js (1933 LOC), Log*, Monit, Dashboard, Version, UX/* |
| API extra | [trace/apiExtra.md](trace/apiExtra.md) | Startup, Configuration, Extra, Containerizer, Deploy, Serve, Modules/*, pm2-plus/*, schema.json |
| CLI | [trace/cli.md](trace/cli.md) | binaries/* (CLI.js full command enum), bin/*, completion |
| Aux plumbing | [trace/aux-plumbing.md](trace/aux-plumbing.md) | Common.js, Utility, Configuration store, HttpInterface, OtelManager, Worker, VersionCheck, tools/*, templates |
| Ecosystem | [trace/eco.md](trace/eco.md) | pm2-io-agent, pm2-io-bpm, vizion, packager, types/, full 22-dep audit |
| Tests/CI | [trace/tests.md](trace/tests.md) | Full suite inventory, CI matrix, coverage holes |
| rand style | [trace/randStyle.md](trace/randStyle.md) | Conventions to adopt (workspace, lints, docs, errors, tests, release) |

Synthesis extracts: [trace/_all-verdicts.md](trace/_all-verdicts.md), [trace/_all-rust_map.md](trace/_all-rust_map.md), [trace/_all-missing.md](trace/_all-missing.md).

## The five load-bearing flows (the product contract)

1. **Spawn**: `prepare` → instance expansion (0/-N→numCPUs) → `executeApp` → fork mode (`spawn` + daemon-side log pipes + IPC channel) or cluster mode (Node `cluster.fork` + ProcessContainer injection). Env flattening rules, `NODE_APP_INSTANCE` slot algorithm, pid/log file suffixing, `wait_ready` gate (IPC `'ready'` packet, `listen_timeout` fallback 3000ms).
2. **Restart brain**: `handleExit` — `min_uptime`×`max_restarts` unstable-restart window, exponential backoff ×1.5 cap 15s (`exp_backoff_restart_delay`), `stop_exit_codes`, `autorestart` flags. THE core semantic to port byte-exact.
3. **Kill ladder**: `killProcess` → SIGINT (configurable; or IPC `'shutdown'` msg if `shutdown_with_message`) → poll `kill_retry_time` 100ms → SIGKILL survivors after `kill_timeout` 1600ms → timeout error. Tree-kill via `ps` snapshot walk (racy).
4. **Reload (0-downtime)**: cluster-mode-only — `_old_<id>` registry key juggling, new worker up (`'listening'`/`'ready'`) → old drained (`'shutdown'` msg, GRACEFUL_TIMEOUT 8000) → reaped. Works ONLY because Node cluster master shares the listen socket. Fork-mode "reload" is restart-with-downtime.
5. **Client boot**: any `pm2` command → ping daemon socket → auto-spawn daemon if missing (re-exec self, detached, readiness handshake via Node IPC) → RPC over vendored axon (req/rep) + event bus (pub/sub, broadcast-everything, client filters).

## Tag summary

- **[HOT]**: Common.js, CLI.js, pm2-ls.js, God.js, API.js, Serve.js, ForkMode.js, Modules/, package.json, CI workflow — churn concentrated in config normalization + CLI surface + list rendering.
- **[COMPLEX]**: God.js `executeApp` (~220-line callback pyramid), ActionMethods.js (909 LOC), API.js `_startJson`/`_operate` (~280 LOC each), ProcessContainer.js (monkeypatch injection), Startup.js, TransactionAggregator (670 LOC, dead with SaaS).
- **[STALE]**: Runtime.js (2018, dead), packager/ (EOL distros), pm2-ls-minimal (2019 but load-bearing), Docker.js, schema.json (stable ≠ dead).
- **[UNTESTED]** (authoritative from tests bucket): Watcher-by-name paths, Worker.js, Monit/Dashboard TUIs, Deploy, Event.js, HttpInterface — plus **kill_timeout/backoff timing suites excluded from ALL CI** (zero platform coverage of the core kill contract), Windows e2e = zero, macOS CI = zero.
- **[TODO]**: near-zero TODO comments (1 grep hit in lib/) — debt lives in structure, not markers.

## Live bugs found during trace (do not port)

- `stopWatch`/`toggleWatch`/`startWatch` ProcessName paths: `clusters_db[findByName(...)]` indexes object with an *array* → silent no-op.
- `Watcher.disableAll`: splice-on-object, dead+buggy; per-instance watchers each restart the whole name-group (O(N²) restart pressure).
- ProcessContainer setuid/setgid: checks `process.env.gid` but applies `pm2_env.gid`.
- `pm2-describe` args rendering: `JSON.parse(args.replace(/'/g,'"'))` crashes on apostrophes.
- pm2-ls-minimal: `pm_exec_path.script` on a string → undefined when name missing.
- Extra.js: undefined `conf.ERROR_EXIT`, undefined `cmd` on low-traffic paths; Containerizer broken require path (crashes → feature unused in practice).
- API.js scale `'-N'` parse bug; Dashboard `processes[this.list.selected]` unchecked OOB.
- getReport dumps daemon's full `process.env` (secrets) — also HttpInterface: 0.0.0.0 bind, CORS *, env dump, unauthenticated.
- axon: unbounded queues (hwm=Infinity), silent drops, infinite silent reconnect, 15-arg AMP limit, no deadlines anywhere in RPC plane.
- Non-crypto `Math.random` UUIDs; `kill(pid,0)` + `_tree_pids` polling has pid-reuse ABA race.
- writeExitSeparator: dead code (call sites commented out; would truncate logs if revived).
