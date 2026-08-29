# shep v1 — Product & Behavior Specification

Status: draft for the maintainer's review · 2026-08-07
Inputs: [map.md](../systematic-refactor/refactor-workspace/map.md) (module map),
[goals.md](../systematic-refactor/refactor-workspace/goals.md),
[decision-briefs.md](../systematic-refactor/refactor-workspace/decision-briefs.md),
[terminology.md](../terminology.md), [idiomatic-rust.md](../idiomatic-rust.md).
This spec is the behavior contract; map.md stays the module-level design. Where
they disagree, this spec wins (and map.md gets fixed).

## 1. Product

shep is a general-purpose process manager: a single Rust binary whose daemon
(the shepherd) supervises long-running processes (the flock) with restart
policies, log capture, graceful reload, file watching, native
observability (Prometheus, webhooks, TUI, MCP), and first-party plugins
(dogs). Clean-room build inspired by pm2's feature list; MIT OR Apache-2.0.

**Non-goals (v1):** container orchestration; multi-host anything; a
third-party module registry (the dog contract replaces it); pm2 artifact
compatibility (formats live only in `shep import`); deployment tooling;
in-process Node instrumentation.

## 2. Versioned scope

**v1.0 ships:** daemon + supervision (fork model, N instances), full CLI,
Flockfile config, daemon config (`shep.toml`, file < env < flags layering),
JSON wire protocol over UDS/named-pipe, fd-pipe readiness + HTTP/TCP/exec
probes, custom actions (`trigger` over the shepherd channel), log
capture/tail/flush/reopen, watch-restart, cron-restart, memory-limit polling
behind `LimitEnforcer`, SO_REUSEPORT reload for all runtimes, dogs
infrastructure + metrics dog + bark dog, lookout TUI, whistle MCP (stdio),
`shep import` + migration guide, startup scripts (systemd Type=notify,
launchd, openrc, rc.d).

**v1.1 committed (design now, build later):** HTTP/SSE MCP transport; cgroup
v2 enforcement (`enforce = "kernel"`); `@shep/io` npm shim (on demand); the
Windows service integration (an SCM service, which `shep startup` does not
build) — the rest of that tier, named pipes and Job Objects and
start/stop/list/logs and its e2e, shipped instead, see §2's 2026-08-28
amendment; vcs metadata (git
revision shown in describe — `vcs` feature, off by default); `shep web`
JSON status endpoint (if demand — metrics dog covers observability).

**v1.2 candidate:** fd-passing/LISTEN_FDS true cluster parity (the maintainer: "v1.1 or
v1.2 even").

**Amendment, 2026-08-15:** the line above used to split Windows into a v1.0
"functional tier" (named pipes, Job Objects, start/stop/list/logs) and a
v1.1 "polish" tier (service integration, ctrl-event graceful stop, full
e2e). The maintainer ruled the whole tier out of v1.0 once an actual estimate
existed rather than a guess — [windows-estimate.md](windows-estimate.md)
puts it at roughly 36-49 tasks over 4-5 phases, and a redesign rather than
a port. Windows is 0%, not partial, and stays that way through v1.0; see
[deferred.md](deferred.md) for the up-to-date single list. §11 and §13
below are corrected to match; this note exists so nobody reaches for the
old split again.

**Amendment, 2026-08-28:** the amendment above is overtaken and is kept only
so the reasoning is legible. A Windows host became available, and
[windows-estimate.md](windows-estimate.md)'s own first recommendation was to
dispatch a CI leg before scoping anything: the tree was already compile-green
on native MSVC, and the estimate above turned out to be a guess about a
redesign that was not needed. The day-to-day tier is built, runs, and is
tested on real Windows in CI. Three refusals are permanent and argued at
their own call sites: no graceful signal outside the shepherd channel, no
`shep startup` (an SCM service is a different program shape), and no
`user`/`group`. §11 and §13 below are corrected to match.

The two lists above cover what is *deliberately* deferred. What is named
above as v1.0 but not yet built — the larger gap, tracked against the
implementation rather than designed away — is
[docs/specs/deferred.md](deferred.md).

## 3. Architecture

Four crates, one distributed binary (`shep`); see map.md for module detail.

- `shep-core` — types, config, paths, selectors, errors, wire protocol.
- `shep-daemon` — supervisor lib: registry actor, spawn/kill/reload state
  machines, watcher, workers, RPC server, bus, dog support. No binary.
- `shep-client` — async client: connect-or-spawn, typed RPC, event streams.
  Re-exports shep-core.
- `shep` — the `shep` binary: clap surface, output/UX, lookout, whistle,
  serve, dogs, import, runtime/dev modes. Hidden `daemon` subcommand is the
  daemonization target (re-exec self, detached, readiness handshake over a
  pipe: child reports `{pid, version}` once the socket is bound). `[[bin]]`
  aliases `shep-runtime` and `shep-dev` ship for container entrypoints
  (argv[0] dispatch to the same subcommands).

Runtime layout: `$SHEP_HOME` (default `~/.shep/`) holding `shep.toml` (daemon
config), `flock.json` (state snapshot), `logs/`, `pids/`, `run/` (sockets,
0700).

## 4. Process model & lifecycle semantics

One spawn path: `tokio::process::Command`, own process group, optional
uid/gid, piped stdio + one extra pipe fd (the shepherd channel). "Cluster"
= N instances of the same app, each with `SHEP_INSTANCE` slot id (lowest free
slot among same-name; `increment_var` supported). Processes a sheep spawns
are its **lambs** (the process-tree members): shown in `describe`'s tree
view, killed with the sheep by the process-group/Job-Object tree kill.

**States:** `starting`, `online`, `stopping`, `stopped`, plus `errored`
(restart budget exhausted or spawn failure) and `waiting-restart` (backoff
delay pending). Serialized exactly as these strings on the wire.

`stopping` is not a step every stop passes through — it is reachable from
exactly one place: a reload's `SpawnNew` step marks the instance being
replaced `stopping` before its replacement spawns, so the two never both
count as running (`ProcStatus::Stopping`'s own doc,
`crates/shep-core/src/status.rs`). A plain `stop` (or a restart, or the
stop half of a delete) stays `online` for its whole kill ladder below and
jumps straight to `stopped` once the process is reaped — there is no
observable `stopping` state on that path at all.

**Restart policy (per app):**
- `autorestart` (default true): restart on unexpected exit.
- `stop_exit_codes`: exit codes treated as clean stop, no restart.
- Restart budget: an exit within `min_uptime` (default 1000ms) counts as
  unstable; the unstable counter increments per consecutive unstable exit
  (no time window) and resets after any stable run (uptime ≥ min_uptime) or
  manual restart/reload. Counter reaching `max_restarts` (default 16) →
  `errored`.
- `exp_backoff_restart_delay` (opt-in, initial delay ms): delay grows ×1.5
  per consecutive unstable restart, capped at 15000ms; resets after a stable
  run (uptime ≥ min_uptime) or manual action.
- `restart_delay`: fixed alternative to backoff.

**Stop/kill ladder:** send stop signal (`kill_signal`, default SIGTERM;
`shutdown_with_message` sends `{"kind":"shutdown"}` on the shepherd channel
instead) → wait `kill_timeout` (default 1600ms) on `child.wait()` → SIGKILL
process group → reap. Tree kill = `kill(-pgid)`; Windows = Job Object
terminate. Exit code + signal recorded exactly (owning-parent `waitpid`).

> Deviation from pm2 (deliberate): default stop signal is SIGTERM, not
> SIGINT — SIGTERM is the unix convention; `kill_signal` covers the rest.

**Reload (graceful, any runtime):** shep does not provide zero downtime. It
provides an **overlap** in which the application can achieve it — the old
instance and its replacement are both live for a window, and closing the gap
inside that window is the app's to do (stop accepting, finish the work in
hand, exit). An app that ignores its stop signal until shep's `SIGKILL` loses
whatever its listener had queued and not yet accepted, on every reload, and
nothing shep does prevents that. State machine per instance:
`SpawnNew → AwaitReady → DrainOld → ReapOld`. AwaitReady = readiness signal
(§7) or `listen_timeout` (default 3000ms). DrainOld = stop ladder with
`graceful_timeout` (default 8000ms) cap.

Since 2026-08-28 that machine is one of two, chosen per app by
`ReloadMode::of`. The state above is the overlapping one, taken by an app
that asks for no probe, by one using `wait_ready`, and by one carrying
`reuse_port = true`. A probed app WITHOUT `reuse_port` instead reloads
serially: `DrainOld → ReapOld → SpawnNew → AwaitReady`. The instance being
replaced goes first and its replacement is spawned into the empty slot.

The reason is that a probe asks an address, and an address cannot say which
of two overlapping instances answered it. Serialising costs a gap you can
see, the drain plus the replacement's start, and buys a success you can
trust. It also fixes a failure the overlap never worked around: an app that
does not set `SO_REUSEPORT` cannot have two instances bound to one port, so
overlapping it spawns a replacement that takes `EADDRINUSE` and crash-loops.
That includes every Node app on macOS, where the option is `ENOTSUP`.

`reuse_port` is the app's own claim that it sets `SO_REUSEPORT` before its
own `bind()`. shep never binds a listen socket on an app's behalf and cannot
check the claim in advance. `SO_REUSEADDR`, which far more frameworks set by
default, is not sufficient, and a mixed pair — one process with the option,
one without — is refused by the kernel on both tier-1 platforms.

Reload proceeds instance-by-instance; failure of the new instance aborts the
rest. What that leaves running depends on the mode and on HOW it failed.

Under overlap the replaced instance has not been drained yet, so `abort_reload`
kills the replacement, puts the drainee back, and nothing is lost.

Under serial there is no drainee to put back, and the two failures diverge. A
replacement that cannot be SPAWNED leaves that instance slot empty until
something starts it; only the remaining old instances stay up. A replacement
that spawns but is not ready inside `listen_timeout` is left running and left
at `starting`: killing it too would empty the slot outright, and marking it
`online` would claim a readiness nothing observed. The reload ends there
either way.

> macOS caveat: `SO_REUSEPORT` there is last-binder-wins, not
> load-balancing — measured cross-process over 40 connections, macOS sent
> 40/40 to the newest binder while Linux split 20/20. That is favorable for
> reload (the new instance takes over 100% of new connections immediately;
> the old one only drains what it already had) and unfavorable for the
> "cluster" instance model (N instances of one app sharing a socket):
> steady-state load does not spread across sibling instances on macOS the
> way it does on Linux. See §11.

**Watch:** `notify` + debounce (`watch_delay`, default 500ms per trace
semantics), one watcher per app name-group, default ignores (dot-entries,
node_modules, log/pid dirs), `ignore_watch` + `watch_options` globs via
globset. Events during an in-flight restart are re-checked after it
completes.

**Cron restart:** `cron_restart` pattern (croner dialect), timezone option.

**Memory limit:** `max_memory` (MemSize, grammar `^\d+(G|M|K)?$`) — polling
enforcer, 15s cadence, restart on breach, via `LimitEnforcer` trait (cgroup
impl lands v1.1).

## 5. Configuration

**Flockfile** (per-app config): TOML preferred, YAML + JSON + JSON5 accepted;
`.js`
config via `node -e` with a try/catch bridge script that requires the module
and writes its JSON to stdout (requires node on PATH, documented) — not
`node -p` bare, whose crash-dump on failure hides the real error behind a
trailing `Node.js vX.Y.Z` banner. Discovery order in cwd:
`Flockfile.{toml,yaml,yml,json,json5}`
then lowercase `flockfile.*` in the same order. Schema = `AppConfig` in shep-core
(schemars-exported JSON schema ships in `crates/shep-core/assets/`, generated
from the parser's own document type and describing the whole document — not
just `AppConfig` — for editor completion). Field set per map.md app_spec;
sheep-native names, no pm2 aliases.

**Amended, Phase 14 (the maintainer's ruling).** `.js` is read only when named
explicitly with `shep start <path> --flockfile`. Directory discovery never
selects a `.js` file and the ten-name order above is unchanged, because
reading one runs `node` on it: entering a cloned repository and running
`shep start` must not execute code that repository contains. `shep start
server.js` with no flag still means what it always has — run `server.js` as a
script. The document `--flockfile` reads is Flockfile-shaped (the `app` key,
sheep-native field names), not a pm2 `ecosystem.config.js`; `shep import`
remains the pm2 path.

**Daemon config** `$SHEP_HOME/shep.toml`: `[daemon]` (log policy, socket
overrides), `[dog.<name>]` sections (each dog's typed config), alert rules
under `[dog.bark]`. Layering: file < `SHEP_*` env < CLI flags.

**Folds** (grouping): optional `fold = "<name>"` field in AppConfig assigns a
sheep to a named group. `shep fold <name>` lists that fold; `fold:<name>`
selects it in any verb; `flock`/`describe` display fold membership.

**KV store** (`shep set/get/unset`): retained for ad-hoc + dog runtime
tweaks; file-locked JSON; not the primary config path. Lives at
`$SHEP_HOME/kv.json`; keys are flat strings, not paths — a dot in a key is
part of its name, never a nesting separator. A dog reads the store through
`shep_core::kv`, not over the socket, unlike `[dog.<name>]`: that section
goes over the wire because the alternative was the child's environment,
which a process listing or crash dump can read back; a `0600` file inside a
`0700` `$SHEP_HOME`, opened by a process running as the same uid, has none
of that exposure, so the socket would buy a round trip for nothing.

## 6. Wire protocol (v1 = protocol version 1)

- Transport: UDS at `$SHEP_HOME/run/shep.sock` (unix, dir 0700); per-user
  named pipe (Windows).
- Framing: u32 length prefix + JSON (`LengthDelimitedCodec`).
- Handshake: client `Hello{client_version, protocol}` → daemon
  `HelloAck{daemon_version, protocol, pid}`. Version skew = typed error, not
  silence.
- RPC: `Envelope{id: u64, deadline_ms: Option<u64>, body: Request}` →
  `Reply{id, result: Result<Response, RpcError{code, message}>}`. Default
  client deadline 5s. Requests/responses are typed enum pairs (IR-13).
- Events: client sends `Subscribe{topics: Vec<Glob>}`; daemon filters
  server-side; bounded per-subscriber queue, drop-oldest, `Dropped{count}`
  notice event. Topics: `process.*` (lifecycle), `log.out`, `log.err`,
  `channel.*` (shepherd-channel messages), `daemon.*`. `channel.*` is
  `channel.ready`, `channel.metric`, `channel.action_reply` — every message
  kind a sheep writes on fd 3. The outbound half (`shutdown`, `action`) is
  deliberately absent: those are already reported to their caller, by
  `process.stop` and by `Response::Triggered`.
- Stability: every frame type has committed byte fixtures + insta snapshots
  (IR-35); breaking change = protocol version bump + CHANGELOG.
- Auth: socket permissions + SO_PEERCRED/getpeereid check (same-uid only by
  default).
- Stale socket recovery: bind EADDRINUSE → probe-connect → unlink if refused
  (load-bearing for the reboot-resurrect scenario in §13.4).
- Client reconnect: backoff 100ms ×1.5, cap 5s.

## 7. Readiness & health

- **Shepherd channel** (extra pipe fd, newline JSON, language-agnostic):
  child→daemon `{"kind":"ready"}`, `{"kind":"metric",...}`,
  `{"kind":"action-reply",...}`; daemon→child `{"kind":"shutdown"}`,
  `{"kind":"action",...}`. Fd number exported as `SHEP_CHANNEL_FD`, wire
  version as `SHEP_CHANNEL_VERSION` (`1`). An `action` carries `name` and
  `id`, and `params` when the operator supplied any — the `params` key is
  absent otherwise, which is what keeps it additive (§9). `id` is the
  dispatch's correlation token; an app that echoes it back on its
  `action-reply` as `id` gets its answer matched to that exact request, and
  an app that does not is matched by action name and by order, exactly as
  every app written before the field existed. Full contract for an app author
  writing to this wire, including the parts this bullet has no room for (why
  an action should reply even to a name it does not recognize, what the
  name-and-order fallback costs when two triggers of one action overlap, the
  `params` quoting gap): [`docs/shepherd-channel.md`](../shepherd-channel.md).
- **Probes** (per app, optional): `readiness_probe` / `liveness_probe` =
  HTTP GET / TCP connect / exec, with interval, timeout, failure threshold.
  Readiness gates reload; liveness failures trigger the restart policy.
- `wait_ready = true` selects channel-based readiness; probe config selects
  probe-based; neither selects the heuristic source (`ReadinessSource::
  Heuristic`, "ready" = spawn success + `listen_timeout`) — but the
  heuristic only ever *runs* during a reload's `AwaitReady` step. A plain
  `start`/`restart` with neither `wait_ready` nor `readiness_probe`
  configured goes `online` synchronously at spawn success, full stop: no
  wait, no `listen_timeout` involved, because only reload gates a spawn on
  readiness in the first place (`Supervisor::spawn_fresh`'s `gated` flag,
  `crates/shep-daemon/src/supervisor.rs` — `false` for the heuristic
  source outside a reload, so the sheep is `online` before the readiness
  machinery is ever consulted). `probes/ready.rs`'s own doc states this
  directly.

## 8. Dogs (plugins)

Contract: a dog is a process speaking the client wire protocol,
supervised by the daemon, tagged `dog` (badged in `shep flock`, shown in
its own table by default — a flock with no dogs prints exactly what it
printed before this section existed). First-party dogs live in the
multi-call binary as `shep dog <name>`. Lifecycle: `shep enable <name>` →
daemon config entry → autostart with the daemon; `shep disable <name>`
removes. Config: `[dog.<name>]` in shep.toml, read by the dog over the
Unix socket rather than through its environment. Third-party dog = any
binary speaking the protocol, registered with `shep adopt <name> <path>`
(`shep enable --exec <path> <name>` is a hidden alias, kept for pm2 muscle
memory); `shep rehome <name>` forgets the registration.

**metrics dog (v1):** serves Prometheus exposition on 127.0.0.1:9615
(configurable): per-sheep cpu/mem/restart_total/status/uptime, daemon self
metrics, host metrics. Reference Grafana dashboard JSON in `assets/grafana/`.
OTLP export = `otel` cargo feature.

**bark dog (v1):** subscribes `process.*` + polls state as reconciliation
(bus drops must not lose alerts). Named sinks in `[dog.bark.sinks]`:
Discord webhook, Slack webhook, generic JSON POST (templated body). Rules in
`[dog.bark.rules]`: event kinds, restart-loop detection, memory threshold;
per-rule debounce/cooldown; **each rule routes to one or more named sinks**
(per-event routing, must-have #7). Fired alerts append to
`$SHEP_HOME/barks.jsonl` (size-capped ring) — the data source for
`shep barks` and the whistle's `list_barks`.

Third-party extensions are treated as dogs once enabled: any binary
speaking the client wire protocol, registered via `shep adopt <name>
<path>`.

**This section carried three departures from what shipped, each decided
against the spec as written rather than found as an oversight in it — the
same posture §9's `trigger` amendment and §13's item 4 correction take,
recorded here for the same reason: what a later reader needs is the
reasoning, not just the corrected sentence.**

Dogs are not hidden behind `--all`. The original contract modeled a dog
listing on pm2's own "internal module" convention: a category ordinary
operators rarely care about, so bury it behind a flag the way `ps aux`
buries kernel threads. That model does not fit what a dog turned out to
be here — the metrics dog and the bark dog are the flock's own
observability layer, and an operator who just enabled one wants to see it
came up without reaching for a flag `shep enable`'s own output never
mentioned. `shep flock` prints dogs as a second table, present only when
at least one is registered, and `shep dogs` prints that table alone; a
flock with none still renders exactly as it did before this section
existed.

`enable --exec` is a hidden alias, and `adopt`/`rehome` are the verbs.
The spec as written had one lifecycle for a built-in dog and folded a
third-party one into `enable`'s own flag. Registering a third-party
binary needed to do more than a built-in dog's `enable` ever did — a
path to vet, refuse, and remember beyond the boolean question of whether
the dog is on — and `disable` needed a counterpart that answers "forget
this dog existed" rather than just "stop it for now." `enable --exec`
survives only because it is pm2's own spelling and muscle memory
transfers; `adopt`/`rehome` are what an operator reading `--help` is
pointed at.

A dog's configuration reaches it over the socket, not the environment.
The spec as written left this unstated, reading like an ordinary
`AppConfig`-shaped config passthrough. `[dog.bark.sinks]` routinely holds
a webhook URL, and a webhook URL is a bearer credential — the same
reasoning `SECURITY.md`'s redaction rule already applies to a sheep's own
`env` map, extended here rather than left as a second, quieter exposure:
an environment variable is readable from the process table, inherited by
every child a dog spawns, and captured into a crash dump, none of which
is true of a value the dog only ever asks the daemon for directly.

## 9. CLI surface (sheep-native)

Core verbs: `start` (script | Flockfile | `-` stdin JSON), `stop`, `restart`,
`reload`, `delete`, `scale`, `flock` (list; `list`/`ls` aliases), `describe`,
`bleats` (logs; `logs` alias), `flush`, `reopen` (reopen log files for
rotation; also SIGUSR2 to the daemon), `muster` (save + resurrect pair:
`shep save` / `shep muster` restores; `resurrect` hidden alias),
`signal`, `sendline`, `trigger <target> <action> [params]` (custom actions via
the shepherd channel `action`/`action-reply` messages), `enable`/`disable`
(dogs), `dogs` (list dogs), `barks` (recent alert history), `fold <name>`
(list a fold), `lookout` (TUI; `dash` alias), `whistle` (MCP stdio), `serve`,
`startup`/`unstartup`, `set`/`get`/`unset`, `import`, `dev`, `runtime`,
`daemon` (hidden), `dog <name>` (hidden), `kill` (daemon shutdown),
`thatlldo` (easter-egg alias for graceful `stop`). Selectors everywhere:
name, id, `all`, `/regex/`, `fold:<name>`. Global `--format json|table`
(versioned serde output schema), clap_complete completions incl. dynamic
sheep-name completion (short-timeout daemon query, silent degrade).

**`trigger` takes params, and this section did not always say so.** It was
specified as `trigger <target> <action>`, with nothing after the action name,
and §7's `action` message was specified to match. Both now carry an optional
argument string. That was decided against the spec as written rather than
found as an oversight in it, and it is recorded here because the reasoning is
what a later reader will need, not the outcome.

The moment decided it, not the merit. The shepherd channel has no version
field — `PROTOCOL_VERSION` governs the client↔daemon socket and nothing else
— so every string on fd 3 is a contract with every app that speaks it, and
there is no handshake in which to negotiate a replacement. While there are no
deployed apps and no `@shep/io` shim, a field added here costs nothing; from
the moment either exists, the same field is a rewrite for everything already
written. That asymmetry is the entire argument, and `trigger web
set-log-level debug` is an ordinary thing for an operator to want.

Additive is what makes it survivable at all: `params` is omitted from the
serialized message when there are none, so
`{"kind":"action","name":"gc","id":7}` is still exactly what an argument-free
action looks like on the wire, a message with no `params` key reads back as
none, and an app that ignores the field goes on working. It is **one opaque
string, not structured data** — shep does
not parse it, validate it, or hold a schema for it. An app that defines an
action already has a grammar for that action's arguments, and a second
grammar in the daemon would only be something for every app to either adopt
or work around.

**`scale` and `signal` each made a narrowing call worth recording here too.**
`scale <name> <count>` takes an absolute count and has no relative `+N`/`-N`
form: a count is idempotent under concurrent operators in a way a delta is
not, and the trace notes record a pm2 crash on the relative-remove path this
avoids reproducing. `signal <selector> <signal>` delivers to the sheep's own
process, not its group: the stop ladder already owns group-wide delivery for
the shutdown case, and broadcasting an operator's SIGHUP to every lamb would
reach processes the operator never named.

**Exit codes.** Distinct causes get distinct codes; no error ever exits 0.

| Code | Name | Meaning |
|---|---|---|
| 0 | success | The command did what it was asked. |
| 1 | failure | An error with no more specific code. |
| 2 | usage | Bad arguments. clap's own convention. |
| 3 | not found | A selector matched no registered sheep. |
| 4 | invalid config | A Flockfile or daemon config failed validation. |
| 5 | daemon unreachable | No daemon answered, and none could be started. |
| 6 | protocol mismatch | Client and daemon speak different wire versions. |
| 7 | spawn failed | The daemon could not spawn a sheep. |
| 8 | deadline exceeded | The request outlived its deadline. |
| 9 | internal | An unexpected daemon-side failure. |
| 10 | daemon already running | Another daemon already holds this `$SHEP_HOME`. |
| 11 | flock empty | The foreground flock emptied with a sheep in `errored`. `runtime`'s fail-fast status. |

Code 10 is a contract across a process boundary, not merely a CLI detail: a
CLI that loses the race to start a daemon learns it only from the exit
status of the child it spawned, so `shep daemon` must exit 10 on that path
and the client must read 10 as "another daemon won — keep probing", never
as a failure.

Code 2 is claimed by clap for usage errors, which collides with the
fail-fast code `runtime` is specified to use below. Resolved by giving
`runtime`'s fail-fast status its own code, 11, rather than sharing clap's:
an orchestrator watching the exit status cannot act on a code that means
both "bad flag" and "dead app".

**lookout (TUI):** ratatui; panes = flock table, bleats feed, sheep detail,
host usage; event-driven redraw; search/filter.

**whistle (MCP, stdio v1):** tools — `list_flock`, `describe_sheep`,
`get_metrics`, `tail_bleats`, `list_barks`; control tools (`start_sheep`,
`stop_sheep`, `restart_sheep`, `reload_sheep`) exist but require the daemon
flag `whistle.allow_control = true` (default false). rmcp SDK.

**import:** reads a box's pm2 state (`~/.pm2/dump.pm2`, ecosystem
json/yaml/js) → emits Flockfile + report of unmapped fields; `--start` to
adopt immediately. Companion `docs/migration.md`. All pm2 format knowledge
confined here.

**serve:** static file server as a managed sheep, hand-rolled rather than on
axum + tower-http (the maintainer's ruling — `docs/specs/deferred.md` has the reasoning),
SPA fallback, constant-time basic auth from a creds file. `--listing` and
`--hidden` opt into directory listing and dotfiles, both off by default;
`--bind` widens the loopback default; `--follow-symlinks` opts into following
symlinks under the docroot, off by default.

**dev:** isolated `$SHEP_HOME` (`~/.shep-dev`), forced watch, auto-exit.
**runtime:** foreground no-daemon mode for containers: PID-1 zombie reaping,
stdout log streaming, auto-exit when the flock is empty of online processes
(fail-fast exit code 2).

## 10. Security model

Premises (SECURITY.md, IR-42): runtime dir 0700, daemon unprivileged,
same-uid peer-cred RPC ⇒ no other local user can observe or control the
flock. Env values redacted by default in `describe`, RPC responses, and Debug
(`--with-env` opt-in); redacted Debug + exact-string tests for
secret-carrying types (IR-41). No telemetry, no phone-home; update check =
opt-in. Metrics and serve bind 127.0.0.1 by default. Root can always read
daemon memory (explicit non-goal).

## 11. Platform support

- **Tier 1 (v1 e2e green):** Linux (gnu + musl static artifact), macOS.
  macOS's `SO_REUSEPORT` is last-binder-wins, not load-balancing (§4): the
  "cluster" instance model does not spread steady-state connections across
  sibling instances on macOS the way it does on Linux, even though it is
  tier 1. The reload overlap is unaffected by this — the new instance
  taking over 100% of new connections is the desired reload behavior on
  either platform.
- **Windows:** built and running — see §2's 2026-08-28 amendment, which
  overtakes the 2026-08-15 one above it. The control transport is a named
  pipe rather than a unix socket, and a sheep is held in a job object rather
  than a process group; both live behind `shep_core::transport` and
  `shep_daemon::sys_windows` so nothing above them carries a platform gate.
  Typed `StopSignal` is what kept this an addition rather than a `core`
  rewrite, as intended. `stop` has no graceful signal to send outside the
  shepherd channel, `shep startup` is not built, and `user`/`group` are
  refused.
- Init integration: systemd unit generator uses `Type=notify` + sd_notify;
  launchd plist; openrc; freebsd/openbsd rc.d.

## 12. Testing & CI

Per idiomatic-rust.md IR-33..IR-45: paused-clock deterministic lifecycle
tests (backoff/kill timings asserted as pinned arrays), scripted fake
`ProcessRunner`, proptest on the supervisor state machine, wire byte-fixtures
+ insta snapshots, assert_cmd e2e with fresh `$SHEP_HOME` per test, runtime
compat matrix (Node/Bun/python fixtures) behind `node-compat` feature. CI:
fmt/clippy(pinned)/docs(-Dwarnings)/typos + {ubuntu, macos, windows} ×
{stable, MSRV} + minimal-versions + musl (tests run) + feature-combo ladder
+ llvm-cov coverage upload. Release: cargo-dist artifacts + crates.io
Trusted Publishing.

## 13. v1.0 definition of done

1. All §2 v1.0 features implemented and documented.
2. The four gates — `cargo fmt --check`, `cargo clippy -D warnings`,
   `cargo check`, `cargo test` — green on tier-1 platforms. Windows gets
   `cargo check` only (§11): the tier isn't built, so there is nothing for
   `clippy -D warnings` or `cargo test` to exercise there yet.
3. Wire fixtures committed; SECURITY.md, migration.md, Grafana asset shipped.
4. `shep import && shep save && reboot` → the flock survives on a Linux box
   (the flagship migration scenario; runbook in `docs/migration.md`).
5. Spec↔implementation drift review before tagging.

**Item 4 used to say the systemd unit runs `shep muster`, and that verb is
wrong for what `Type=notify` requires.** `ExecStart` under `Type=notify`
names the one process systemd supervises directly, and that process is the
one expected to report its own readiness — it has to be the long-running
daemon, not a command that runs once and exits. The unit's `ExecStart` is
`shep daemon --foreground`; the flock comes back because that daemon
restores the muster roll as part of its own boot, and `shep muster` stays
what an operator runs by hand afterward, not what the unit invokes. That
correction is recorded here, the way §9's `trigger` amendment is, because
the reasoning is what stops a later reader reaching for the same wrong verb
again: pm2 could write `ExecStart=pm2 resurrect` only because its unit ran
under `Type=forking`, tracking the actual daemon through a PID file once
the `ExecStart` command had already exited — two different processes, one
named in the unit and one being supervised. shep has no such split. Under
`Type=notify` they are the same process by construction, so the readiness
signal (`READY=1`, sent once the restore has finished —
`crates/shep-daemon/src/notify.rs`) can only ever come from the process
systemd itself started.

## 14. Assumptions (delegate-mode calls — flag any to change)

1. Default stop signal SIGTERM (pm2 used SIGINT) — unix convention wins.
2. Defaults carried from trace where unstated: min_uptime 1000ms,
   max_restarts 16, kill_timeout 1600ms, listen_timeout 3000ms,
   graceful_timeout 8000ms, backoff ×1.5 cap 15s. Memory poll tightened
   30s → 15s (cheap via sysinfo, halves worst-case breach latency).
3. Flockfile TOML-first (Rust ecosystem norm); YAML/JSON accepted; `.js`
   needs node present.
4. Metrics dog default port 9615 (pm2's old web port — familiar, unclaimed
   by IANA; trivially configurable).
5. `save` writes the roll, `muster` assembles the flock from it; `resurrect`
   kept as hidden alias.
6. Dogs print as their own table in default `shep flock` output — reversed
   from this list's original "hidden under `--all`"; see §8's own amendment
   for why.
7. Whistle control tools gated by daemon config, not CLI flag — config is
   auditable, flags are per-invocation.
8. `SHEP_INSTANCE` replaces `NODE_APP_INSTANCE` (sheep-native env; importer
   maps the old name into app env if the app needs it).
9. Probe engine scoped to readiness/liveness only in v1 — no per-probe
   custom actions.
10. serve/dev/runtime carried into v1 scope (map "keep" verdicts) — cut any
    of these if v1 should slim down.
11. Daemon idle-footprint goal (single-digit MB RSS) tracked via criterion
    benches + a CI-reported RSS number, not gated in the DoD.
12. vcs metadata deferred to v1.1; `shep web` deferred to v1.1 — the
    metrics dog turned out not to cover it (see `docs/specs/deferred.md`)
    — both were map modules without a ruled version.
