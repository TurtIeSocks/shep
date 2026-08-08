# Phase 6 design notes — `shep serve` + `shep import` (spec §9)

Status: research for plan-writing · 2026-08-07 · network was up — versions below are
live crates.io `max_stable_version` as of today. Ground truth: spec §9 (serve, import),
§5 (Flockfile formats, `.js` shell-out), §10 (bind 127.0.0.1, redaction), §13.4
(flagship migration scenario), §14.8 (`SHEP_INSTANCE` / importer env mapping),
IR-2/3 (dep hygiene), IR-7 (`forbid(unsafe_code)` in shep-cli), IR-33..41 (testing,
redacted Debug). Clean-room rule: all pm2 format knowledge from public pm2 docs +
real-world dump samples only — never pm2 source; and it all stays inside
`shep-cli/src/import/` (spec §9 "All pm2 format knowledge confined here").

## 1. Crate choices (exact versions, crates.io 2026-08-07)

All `default-features = false` + enumerated features per IR-2. New deps land in
`[workspace.dependencies]` with purpose comments; only shep-cli consumes them.

| crate | version | features | purpose |
|---|---|---|---|
| axum | 0.8.9 | `http1`, `tokio` | router, `axum::serve` + graceful shutdown |
| tower-http | 0.7.0 | `fs` | `ServeDir`/`ServeFile` (range, ETag/conditional GET, MIME via mime_guess, traversal rejection — all free) |
| tower | 0.5.3 | `util` | `ServiceExt` (also dev-dep for `oneshot` router tests) |
| subtle | 2.6.1 | — | `ConstantTimeEq` for auth compare |
| sha2 | 0.11.0 | `std` | fixed-width digest before ct-compare (kills length leak); verify MSRV ≤ 1.88 at add time — 0.11 is the new RustCrypto line, fall back to 0.10.9 if it bites |
| base64 | 0.23.1 | `std` | `Authorization: Basic` decode |
| html-escape | 0.2.15 | — | dir-listing escaping |
| percent-encoding | 2.3.2 | `std` | dir-listing hrefs |
| httpdate | 1.0.3 | — | dir-listing mtime column |
| shlex | 2.0.1 | `std` | pm2 `args`-as-string split (import) |

Not added: hyper/http/http-body (transitive via axum — use `axum::http` re-exports);
askama (one hand-rolled `format!` template beats a template engine — KISS);
argon2/bcrypt (rejected below, D3); mime_guess (transitive via tower-http `fs`).
Import needs **zero** new parsing deps: serde_json / serde-saphyr / json5 / toml are
already workspace deps; `.js` ecosystem files reuse the §5 node shell-out.

**axum vs plain hyper (the (a) question):** axum. Spec §9 already names it; the
re-derivation agrees: plain hyper means re-implementing range requests, conditional
GET, MIME guessing, percent-decoding, and `..`-traversal defense — the exact
high-risk, zero-differentiation code `ServeDir` already carries tests for. Dep-weight
objection is answered by (a) feature-trimmed axum (no `json`/`query`/etc.) and (b)
the `serve` cargo feature (D5). What tower-http does NOT give us: dir listing and
basic auth — those are our two custom services, and they're small.

## 2. Module shape

```
crates/shep-cli/src/serve/
  mod.rs      ServeArgs (clap), run(): managed path (build AppConfig → client Start)
              vs --foreground worker path; sheep-name default "serve-<port>"
  worker.rs   bind TcpListener → build router → axum::serve.with_graceful_shutdown
              (SIGTERM via tokio::signal); bind-first so probe readiness is honest
  auth.rs     ServeCreds (TOML load + unix 0600 mode check + REDACTED Debug, IR-41),
              basic-auth middleware (axum::middleware::from_fn_with_state)
  listing.rs  fallback service: dir without index.html → HTML listing (opt-in)

crates/shep-cli/src/import/
  mod.rs      ImportArgs, source discovery (~/.pm2/dump.pm2, explicit path,
              ecosystem file), orchestration, --start hand-off to the start path
  source.rs   readers → Vec<serde_json::Value>: dump.pm2 (JSON array),
              ecosystem .json / .yaml|.yml (serde-saphyr) / .config.js (§5 node
              shell-out, same helper as Flockfile .js)
  map.rs      THE mapping table: &[MapRule { target, aliases, apply }] +
              apply_all(Value) -> (AppConfig, Vec<ReportEntry>)
  report.rs   ImportReport {per-app entries: Unmapped|Coerced|Heuristic|Dropped,
              JSON-pointer path, redacted value preview, note}; human table +
              --format json render

docs/migration.md   (outline §6 below)
```

Errors per IR-18: `ServeError` in serve/, `ImportError` in import/ (anyhow is allowed
in shep-cli, but these two have user-facing variants worth typing: bad creds-file
mode, unparseable dump, no source found).

## 3. Hardest design decisions

**D1 — serve process model: re-exec worker sheep (recommended).**
`shep serve DIR --port N` does NOT run a server in the CLI process or daemon. It
builds an `AppConfig { script: current_exe, args: ["serve", "--foreground", DIR,
"--port", N, ...], name: "serve-<port>", autorestart: true }` and starts it via the
normal client path — the server is an ordinary supervised sheep (spec §9 "as a
managed sheep"). One code path serves both modes: `--foreground` is the visible flag
that runs the worker (also handy standalone / in tests) — no second hidden
subcommand needed, and `shep describe` shows an honest, reproducible argv.
Restart/reload/logs all come free from the daemon.

**D2 — readiness: TCP probe, not shepherd channel.** Dogfooding `wait_ready` via the
channel is tempting, but the worker is shep-cli code and `#![forbid(unsafe_code)]`
(IR-7) blocks `File::from_raw_fd(SHEP_CHANNEL_FD)`; the `/dev/fd/N` dodge is
unix-folklore, not a contract. Instead register the serve sheep with
`readiness_probe = TCP connect host:port` (§7) — zero unsafe, exercises the probe
engine, and is honest because the worker binds before serving. Consequence: managed
mode rejects `--port 0` (ephemeral ports are for `--foreground` tests only).

**D3 — creds file: plaintext TOML `[users]`, 0600-enforced, SHA-256 + subtle
compare (recommended).** Format:

```toml
# shep serve auth — refuse to start unless mode is 0600 (unix)
[users]
alice = "hunter2"
```

- Rejected: argon2/PHC at rest — a per-request KDF (~50–100 ms) on a static file
  server is a self-inflicted DoS lever and a heavy dep. Rejected: htpasswd/bcrypt —
  interop nobody asked for, bcrypt dep, mixed-scheme parsing baggage. Plaintext +
  mode check matches the SECURITY.md premises model (§10/IR-42): same trust tier as
  a `.env` file. Documented as such.
- Compare: for EVERY user row, `sha256(name).ct_eq(sha256(given_name)) &
  sha256(pw).ct_eq(sha256(given_pw))`, OR-accumulated over all rows, no early exit —
  constant-time in both fields and in "which user matched". 401 +
  `WWW-Authenticate: Basic realm="shep"`; auth wraps ALL routes including listing.
- `ServeCreds` gets a manual redacting `Debug` + exact-string test (IR-41).
- Surface is PM2_SERVE_*-free: flags only — `shep serve [DIR] [--port 8080]
  [--host 127.0.0.1] [--spa] [--listing] [--auth FILE] [--name NAME]`. No env vars
  read, no creds on argv (only the file PATH), default bind 127.0.0.1 (§10). No
  special Flockfile surface in v1 — a Flockfile can declare the worker argv as a
  normal app (KISS).

**D4 — import parsing posture: `serde_json::Value` walk + declarative rule table
(recommended), never strict serde structs.** Clean-room means we only *believe in*
the documented keys; real dumps carry dozens of undocumented internals
(`pm2_env`-ish bookkeeping) that a `deny_unknown_fields` struct would choke on and a
tolerant struct would silently eat. The Value walk inverts that: consume keys the
rule table claims (each rule = target AppConfig field + alias list, e.g. `cwd` ←
`["cwd","pm_cwd"]`, `out_file` ← `["out_file","output","pm_out_log_path"]`), and
everything left over IS the unmapped report — the report falls out of the design
instead of being maintained in parallel. Coercions are tolerant with report entries:
number-as-string → number, env values stringified (pm2 allows numbers/bools),
`args` string → shlex split (flagged `Heuristic`). Per-app failure never aborts the
run. Env VALUES are redacted in the report (§10).

**D5 — semantic translations that aren't 1:1 (the judgement cluster):**
- `instances: "max" | 0 | -N` → resolve against `std::thread::available_parallelism()`
  AT IMPORT TIME, write the concrete number, report `Coerced` (AppConfig has plain
  `u32`; a Flockfile should be deterministic, not re-resolve per host).
- `exec_mode: "cluster_mode"` → no direct analog (shep "cluster" = N fork instances,
  §4). Keep `instances`; DON'T guess `reuse_port = true` (we can't know the app binds
  a port) — emit a `Heuristic` report entry recommending it + migration-guide link.
  If `instance_var` absent and mode was cluster, set
  `increment_var = "NODE_APP_INSTANCE"` so old Node apps still find their slot
  (spec §14.8's sanctioned mapping).
- `env_production` / `env_*` blocks → shep has no per-env profiles: import flag
  `--env <name>` merges exactly one chosen block over `env`; unchosen blocks →
  `Dropped` report entries.
- `interpreter_args`/`node_args` → inexpressible in AppConfig (no field between
  interpreter and script) → `Unmapped` + note with the manual rewrite recipe. Do NOT
  auto-rewrite (too clever, silently changes spawn semantics).
- `treekill: false` → `Unmapped` + WARNING note (shep always tree-kills, §4);
  `cron_restart` → validate against croner dialect at import, report on failure;
  `kill_signal` absent in pm2 → migration guide flags the SIGTERM-vs-SIGINT default
  deviation (§4); ecosystem `deploy` section → single `Dropped` line (§1 non-goal);
  combined `log_file` → map to same path in `out_file`+`err_file` + `merge_logs`,
  flagged `Heuristic`.
- **Hard emission gate:** every emitted Flockfile must round-trip through
  shep-core's strict parser (`deny_unknown_fields`) before it's written — the
  importer can never emit a file `shep start` would reject.

**D6 (minor) — SPA vs listing interaction:** independent flags, natural
composition: existing file → serve; directory with `index.html` → serve it;
directory without → listing iff `--listing`, else 404; any other 404 → `--spa`
fallback to root `index.html` (`ServeDir::fallback`/`not_found_service`). Listing is
a hand-rolled single-`format!` HTML page (html-escape + percent-encoding + httpdate;
dirs first, name/size/mtime, breadcrumb). Off by default.

## 4. What is safely knowable about pm2 formats (clean-room inventory)

From pm2's public docs (ecosystem reference, `pm2 save`/`startup` pages) + observed
real-world dump samples — NOT source: `~/.pm2/dump.pm2` is a JSON array of saved
process-config objects carrying the documented ecosystem keys (name, script, args,
cwd, interpreter, env + env_* blocks, instances, exec_mode, autorestart,
max_restarts, min_uptime, max_memory_restart, restart_delay,
exp_backoff_restart_delay, kill_timeout, listen_timeout, shutdown_with_message,
wait_ready, stop_exit_codes, cron_restart, watch/ignore_watch/watch_delay/
watch_options, out_file/error_file/log_file variants, merge_logs/combine_logs,
log_date_format, pid_file, namespace, uid/gid/user, instance_var/increment_var,
node_args/interpreter_args, time, treekill, windowsHide, vizion, pmx,
source_map_support) plus undocumented internals we deliberately do not enumerate —
they land in the report. Ecosystem files: `{ "apps": [...] }` (+ optional `deploy`)
as `.config.js` (module.exports), `.json`, or YAML process files. Everything beyond
this inventory is treated as unknown-by-design; the tolerant walk (D4) is the
mechanism that makes that safe rather than lossy.

Mapping table (documented key → AppConfig, one line each): name→name;
script/pm_exec_path→script (prefer absolute); args→args (string→shlex, Heuristic);
interpreter/exec_interpreter→interpreter; cwd/pm_cwd→cwd; env→env (stringify);
instances→instances (D5); autorestart→autorestart; max_restarts→max_restarts;
min_uptime→min_uptime (ms number or suffixed string); max_memory_restart→max_memory
(MemSize grammar already matches `\d+(G|M|K)?`); restart_delay→restart_delay;
exp_backoff_restart_delay→exp_backoff_restart_delay; kill_timeout→kill_timeout;
listen_timeout→listen_timeout; shutdown_with_message→shutdown_with_message;
wait_ready→wait_ready; stop_exit_codes→stop_exit_codes; cron_restart→cron_restart
(validated); watch(bool|array)→watch(+watch_options when array);
ignore_watch→ignore_watch; watch_delay→watch_delay; out_file/output/
pm_out_log_path→out_file; error_file/err_file/pm_err_log_path→err_file;
merge_logs/combine_logs→merge_logs; namespace→fold; uid/user→user; gid→group;
instance_var/increment_var→increment_var. Everything else → report (D5 notes).

## 5. Testing strategy (IR-33..40)

- **serve, router tier (fast, no sockets):** `tower::ServiceExt::oneshot` against
  the built router — status/headers/body asserts for: file hit, MIME, 404, SPA
  fallback, listing on/off, index.html precedence, 401 + WWW-Authenticate without
  creds, 200 with creds. Unique tempdir + own config literal per test (IR-34).
- **Boundary sweep (IR-40):** traversal battery — `..`, `%2e%2e%2f`, encoded NUL,
  absolute-path smuggling, symlink escaping the root (assert ServeDir's behavior and
  pin it), empty dir, 0-byte file, name needing both HTML- and percent-escaping.
- **auth unit tier:** creds parse errors, mode≠0600 refusal (unix), multi-user OR
  accumulation, wrong-user-right-password rejected; `ServeCreds` redacted-Debug
  exact-string test (IR-41). Timing is enforced by construction (subtle, no early
  exit) — reviewed, not benchmarked.
- **Listing snapshot:** insta snapshot of the rendered HTML for a fixed fixture dir
  (normalized mtimes) — template regressions become visible diffs.
- **serve e2e (IR-39):** assert_cmd, fresh `$SHEP_HOME`; `--foreground --port 0`
  smoke (parse bound port from stdout line, real GET); managed-mode e2e: `shep serve`
  → poll flock until probe-ready (event-driven wait, no sleeps) → GET → `shep stop`.
  Real time is justified here (network I/O) — comment per IR-33.
- **import unit tier:** table-driven test per MapRule (alias hit, coercion, report
  entry); committed fixture files: minimal dump, kitchen-sink dump (every documented
  key), cluster_mode dump, dump with one malformed entry (others survive), ecosystem
  .json/.yaml/.config.js (js behind `node-compat` feature per §12), env_* profile
  selection. Insta snapshots of emitted Flockfile TOML AND the human report (IR-35's
  spirit; these aren't wire frames so no byte-fixture tier).
- **Round-trip gate test:** every fixture's emitted Flockfile re-parsed by
  shep-core's strict Flockfile parser — must succeed (D5 gate).
- **Redaction test:** dump fixture with env secrets → assert report output contains
  key names but never values (exact-string, IR-41 style).
- **import e2e:** `shep import --dump <fixture> --out <dir>` snapshot of stdout;
  `--start` variant adopts into a fresh daemon and flock shows the sheep — the §13.4
  flagship path minus the reboot.

## 6. Migration guide outline (docs/migration.md)

1. TL;DR: the flagship three-liner (`shep import` → `shep muster save` → `shep
   startup`) + what to expect (§13.4).
2. Command map table: pm2 verb → shep verb (list→flock, logs→bleats, save/
   resurrect→muster, monit→lookout, module→dog, …straight aliases noted).
3. Concept map: ecosystem→Flockfile, namespace→fold, dump→muster roll,
   NODE_APP_INSTANCE→SHEP_INSTANCE (importer's automatic mapping).
4. What imports cleanly (the §4 table, user-voiced).
5. Behavioral deviations: SIGTERM default (pm2: SIGINT), restart-budget semantics
   (consecutive-unstable counter, no time window), cluster→N instances +
   `reuse_port` explainer, always-tree-kill, log format/timestamps.
6. What does NOT import: deploy section, pm2 plus/keymetrics, modules (→ dogs),
   watch_options internals, interpreter_args (manual recipe).
7. Reading the import report: entry kinds, `--strict` for CI, `--format json`.
8. Rollback note: import writes files only; nothing touches pm2's state.

## 7. Eventual plan task list (titles only)

- Workspace deps: serve/import crate additions behind shep-cli `serve` feature
  (default-on, additive per IR-3) + feature-ladder CI row update
- serve: worker router core (ServeDir, SPA fallback, index precedence)
- serve: dir-listing fallback service + HTML template + insta snapshot
- serve: creds file + constant-time basic-auth middleware (+IR-41 tests)
- serve: `--foreground` worker (bind-first, graceful shutdown, port-0 report line)
- serve: managed mode (AppConfig build, TCP readiness probe, name default)
- serve: traversal boundary sweep + e2e (foreground + managed)
- import: source readers (dump.pm2, ecosystem json/yaml/js shell-out)
- import: Value-walk engine + MapRule table (documented keys)
- import: semantic translations (instances/cluster, env_* `--env`, increment_var)
- import: report model + human/JSON render + redaction test
- import: Flockfile emission + strict round-trip gate
- import: `--start` adoption path + e2e fixtures
- docs/migration.md
- CHANGELOG entries + spec-drift check (§9 serve/import lines vs shipped flags)
