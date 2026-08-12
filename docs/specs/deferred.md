# Deferred — what is not in v1.0, and why

The single list. Spec §2's "v1.1 committed" section names six items
deferred by design; everything else below is named as v1.0 scope in the
spec (§2, §3, §5, §6, §8, §9) but is not built as of the 2026-08-11
spec↔implementation audit (`feat/phase7-custom-actions` at `40ea59f`, 752
tests passing locally). A spec section is a plan, not a shipped-state
claim — drift between the two is what this file exists to stop hiding.
Linked from spec §2.

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

**migration guide** (spec §2, §9, §13.4) — `docs/migration.md`, the pm2
cutover companion to `shep import` (which now exists — Phase 8 Task 9).
Does not exist yet.

**openrc and BSD rc.d units** (spec §11) — `shep startup` writes a systemd
unit (`Type=notify`) on Linux and a `LaunchDaemon` plist on macOS; spec §11
names four init systems and there is no renderer for the other two. A machine
running openrc or an rc.d script is refused by name rather than handed a unit
for something it does not run.

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
