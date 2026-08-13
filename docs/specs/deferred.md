# Deferred — what is not in v1.0, and why

The single list. Spec §2's "v1.1 committed" section names six items
deferred by design; everything else below is named as v1.0 scope in the
spec (§2, §3, §5, §6, §8, §9) but is not built as of the 2026-08-12
spec↔implementation audit (`feat/phase8-cutover` at `fc3679e`, 883 tests
passing locally, 1 ignored). A spec section is a plan, not a shipped-state
claim — drift between the two is what this file exists to stop hiding.
Linked from spec §2.

## Scope decision, 2026-08-12: everything below §2's six cuts ships in v1

Rin's call, after the five v1.1 audits came back: *"we should probably fix
everything in v1. We're not in a rush to release this to the public. We want
a hot looking app right off the bat if we have to compete with well
established apps like pm2 and other rust attempts."*

So this file now holds two different kinds of thing, and the section headings
say which is which. The six items under "Committed to v1.1+ by design" are
still deferred — they are scope cuts the spec argues for. Everything under
"Named as v1.0 in spec §2/§9, not yet built" is a **build queue**, in this
order:

1. **Dogs** (spec §8) — in flight on `feat/phase9-dogs`.
2. **The audit debt** — what the five 2026-08-12 audits turned up. Real bugs
   first (`kill_signal` accepts a typo and then sends the wrong signal
   forever; an on-time `ActionReply` can be matched to the wrong request),
   then the wire and config asymmetries, then the tooling and doc staleness.
3. **The rest of the v1.0 surface** — lookout, whistle, serve, dev/runtime,
   scale/signal/sendline, the KV store, `.js` Flockfile, schemars, the
   daemon-config flags layer, the `channel.*` topic, lambs in describe, and
   openrc + BSD rc.d.
4. **The Windows functional tier — last** (Rin, 2026-08-12). It is the one
   item whose cost estimate is mostly guesswork: the decision brief put it at
   +30-40% on the daemon's process-control layer, and that number gets much
   better once nothing else is in flight to confound it.

Ordering is not priority. Windows is last because its estimate is the
weakest, not because it matters least.

## Committed to v1.1+ by design (spec §2)

Six deliberate scope cuts, not oversights — spec §2 carries the reasoning:

- HTTP/SSE MCP transport (whistle ships stdio-only first)
- cgroup v2 enforcement (`enforce = "kernel"`) — `LimitEnforcer`'s polling
  impl is the v1.0 tier
- `@shep/io` npm shim (built on demand)
- Windows polish: service integration, ctrl-event graceful stop, full e2e
  (the functional tier below is the v1.0 target)
- vcs metadata (`vcs` feature, off by default)
- `shep web` JSON status endpoint (only if the metrics dog turns out not to
  cover it)

## Named as v1.0 in spec §2/§9, not yet built

Schedule rather than design is what leaves these open. Where a phase has
landed part of a spec section, the entry names the part still missing rather
than the whole section. See `docs/systematic-refactor/refactor-workspace/`
for what phase is next.

**Dogs subsystem** (spec §8) — the whole thing: the dog contract, the
`enable`/`disable`/`dogs`/`barks` verbs and hidden `dog <name>` dispatch,
the metrics dog (Prometheus on `127.0.0.1:9615`) and its Grafana dashboard
JSON, the bark dog (Discord/Slack/JSON webhook sinks, alert rules).
`[daemon] enabled_dogs` and `[dog.<name>]` (`DaemonSection`,
`crates/shep-core/src/config/daemon.rs`) parse and validate today but have
no reader — daemon boot now warns if either is set
(`crates/shep-cli/src/commands/daemon.rs`).

**lookout** (spec §9, §13) — the ratatui TUI (`lookout`/`dash` verb).
`ratatui` is not a dependency of any crate.

**whistle** (spec §8, §13) — the MCP stdio server (`whistle` verb). `rmcp`
is not a dependency of any crate.

**serve** (spec §9, §13) — static file server as a managed sheep. `axum`
and `tower-http` are not dependencies of any crate.

**dev / runtime** (spec §9, §13) — `shep dev` (isolated `$SHEP_HOME`,
forced watch) and `shep runtime` (foreground no-daemon container mode,
PID-1 zombie reaping). Neither verb exists, nor the `shep-runtime`/
`shep-dev` `[[bin]]` aliases spec §3 describes — `shep-cli/Cargo.toml` has
one `[[bin]]`.

**`scale`, `signal`, `sendline`** (spec §9) — no clap variant.

**`set`/`get`/`unset`** (spec §5, the KV store) — no clap variant, no
file-locked JSON store.

**openrc and BSD rc.d units** (spec §11) — `shep startup` writes a systemd
unit (`Type=notify`) on Linux and a `LaunchDaemon` plist on macOS; spec §11
names four init systems and there is no renderer for the other two.
`commands::startup::current_init` picks the renderer by compile target only
(`target_os = "linux"` → systemd, `target_os = "macos"` → launchd), with no
runtime check for which init system is actually active. A target that is
neither — a BSD host, principally — is refused before any file is written,
with a platform-level message naming neither renderer by name. A Linux host
that runs openrc instead of systemd is not detected at all: `shep startup`
still writes a systemd unit and tries to enable it, and the failure surfaces
only when `systemctl` turns out not to exist.

**Windows functional tier** (spec §11) — 0%, not partial. The Windows arm
of `main.rs::run` prints "shep does not yet support Windows" and exits
`Failure` for every verb; `boot`, `sys`, `server`, `tokio_runner` are all
`#[cfg(unix)]`. Named-pipe transport and Job Objects: absent.

**`.js` Flockfile** (spec §5) — TOML/YAML/JSON/JSON5 discovery and parsing
all work; the `node -p 'JSON.stringify(require(p))'` fallback does not.

**schemars JSON-schema export** (spec §5) — `AppConfig` has no `schemars`
derive; no schema ships in `assets/` (the directory does not exist).

**Daemon-config flags layer** (spec §5) — layering is `file < SHEP_* env`
today; the third, CLI-flag layer over the top does not exist.

**`channel.*` bus topic** (spec §6) — spec'd as subscribable alongside
`process.*`/`log.out`/`log.err`/`daemon.*`. No such topic variant exists
(`BusEvent::topic` in `crates/shep-core/src/protocol/events.rs`) and
nothing forwards shepherd-channel (fd 3) traffic to the bus. `shep trigger`
shipping (below) does not close this: its reply is scoped to the caller
that sent one trigger, so `Ready`/`Metric` traffic and a stale or
unprompted `action-reply` stay just as invisible as before.

**Lambs in `describe`'s tree view** (spec §4) — a sheep's child processes
(lambs) are killed with it via the process-group tree kill, but no wire
field carries them and lamb pids are not persisted, so `describe` cannot
render the tree spec §4 promises.

## Not deferred

`shep trigger` (custom actions over the shepherd channel, spec §7/§9)
**shipped**: the fd-3 wire (`ShepherdMessage::Action`/
`ChildMessage::ActionReply`, `params` included), the RPC
(`Request::Trigger`/`Response::Triggered`), the daemon's waiting model (one
wait per matched sheep, run concurrently, bounded by each app's own
`AppConfig::action_timeout`), and the verb itself
(`shep trigger <selector> <action> [params]`) are all built and tested,
including a real-child, two-round-trip end-to-end case
(`crates/shep-daemon/tests/daemon_e2e.rs`). App-author-facing contract:
`docs/shepherd-channel.md`. What §6 promises beyond it — the `channel.*`
bus topic, above — is separate work and remains open.

**`shep save` / `shep muster`** (the muster pair, spec §9) **shipped**:
the wire (`Request::SaveRoll`/`Response::RollSaved`,
`Request::Muster`/`Response::Mustered`), the daemon's one restore
implementation (`snapshot::muster`, called from both `boot::restore_flock`
at boot and the `Muster` RPC arm for an operator), and the verbs
themselves (`shep save`, `shep muster` with hidden alias `resurrect`, per
spec §14.5). A muster against a flock that already has an app leaves it
running rather than restarting or duplicating it — `snapshot::restorable`'s
rule, not stated in the spec itself.

**`shep import`, and the migration guide** (spec §2, §9, §13.4)
**shipped**: `commands::import` (`dump`, `convert`, `env`, `render`) reads
`~/.pm2/dump.pm2` — JSON only, not `ecosystem.config.js`/`.yaml` — and
writes a Flockfile whose every app passes `shep_core::config::normalize`.
The migration-guide half is `docs/migration.md`.

**`shep startup` / `shep unstartup`** (spec §9, §11) **shipped** for two of
spec §11's four init systems: `commands::startup` renders a systemd
`Type=notify` unit or a `launchd` `LaunchDaemon` plist
(`commands::startup::unit`), installs or removes it privilege-gated by
`geteuid()`, and `shep daemon --foreground` (`crates/shep-daemon/src/notify.rs`)
reports `READY=1` once the muster restore has finished so the unit does not
go green over an empty flock. openrc and BSD rc.d remain open, above.

**CPU and memory in `shep flock`/`shep describe`** (spec §9's observability
surface) **shipped**: `limits::stats` (`SheepStats`, `StatsState`) samples
every sheep's process tree on the existing memory-poll tick;
`ProcessInfo::cpu_percent`/`memory_bytes` carry the reading on the wire,
populated only by `ListFlock`/`Describe` (`rpc::with_live_stats`); the CLI
renders them as the `CPU`/`MEM` columns (`FlockRows`, `output::human_bytes`).
