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

1. **The audit debt** — what the five 2026-08-12 audits turned up. Real bugs
   first (`kill_signal` accepts a typo and then sends the wrong signal
   forever; an on-time `ActionReply` can be matched to the wrong request),
   then the wire and config asymmetries, then the tooling and doc staleness.
2. **The rest of the v1.0 surface** — lookout, serve, dev/runtime,
   `.js` Flockfile, schemars, the daemon-config flags layer, and openrc +
   BSD rc.d.
3. **The Windows functional tier — last** (Rin, 2026-08-12). It is the one
   item whose cost estimate is mostly guesswork: the decision brief put it at
   +30-40% on the daemon's process-control layer, and that number gets much
   better once nothing else is in flight to confound it.

**Dogs** (spec §8) was originally queued first and has since shipped, on
`feat/phase9-dogs`; see "Not deferred" below for what landed. **whistle**
(spec §8, §13) has since shipped too, on Phase 13; same section.

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
- `shep web` JSON status endpoint. Resolved, 2026-08-13: the metrics dog
  does not cover this — it serves Prometheus exposition text for a
  scraper, and `shep web` was a hand-fetched JSON payload for a
  dashboard, an incompatible shape for an incompatible consumer. This
  stays its own deferred item rather than being folded into the dog.

## Named as v1.0 in spec §2/§9, not yet built

Schedule rather than design is what leaves these open. Where a phase has
landed part of a spec section, the entry names the part still missing rather
than the whole section. See `docs/systematic-refactor/refactor-workspace/`
for what phase is next.

**OTLP export (metrics dog)** (spec §8) — the metrics dog serves
Prometheus exposition only; no `otel` cargo feature exists in
`crates/shep-cli/Cargo.toml`.

**lookout's search/filter** (spec §9) — `shep lookout` ships all four panes
spec §9 names (Phase 12a's shell and flock table, Phase 12b's host-usage
strip, sheep detail pane and bleats feed), but no filter or search line.
Rin's v1 ruling for 12b excluded it explicitly ("no filtering UI, no
elaborate layout"), and it also carries an unresolved design question 12a
already wrote down: whether the filter takes the CLI's own selector grammar
or plain substring matching. That choice is Rin's, not this file's, and is
why the item stays open rather than half-built either way.

**lookout actions** (spec §9) — the control gate (`--allow-control`,
`lookout.allow_control`) exists and every action key checks it, but there is
still exactly one action key, `x` (stop), and it refuses honestly in both
gate states: `read-only: actions need --allow-control` when the gate is
closed, `stop is not built yet` when it is open. No action has landed behind
the gate.

**lambs in the detail pane** (spec §9) — the sheep detail pane
([`crates/shep-cli/src/lookout/view/detail.rs`](../../crates/shep-cli/src/lookout/view/detail.rs))
reads only the `ProcessInfo` the flock table's own rows already carry, so it
never shows `lambs`. `ListFlock` deliberately does not populate that field;
only `Describe` does, with a process-table walk, and calling `Describe` on a
two-second poll timer for whichever sheep is selected is the cost this phase
declined to add. Rendered frames of what both 12a and 12b built are in
[docs/lookout/frames.txt](../lookout/frames.txt).

**serve** (spec §9, §13) — static file server as a managed sheep. `axum`
and `tower-http` are not dependencies of any crate.

**dev / runtime** (spec §9, §13) — `shep dev` (isolated `$SHEP_HOME`,
forced watch) and `shep runtime` (foreground no-daemon container mode,
PID-1 zombie reaping). Neither verb exists, nor the `shep-runtime`/
`shep-dev` `[[bin]]` aliases spec §3 describes — `shep-cli/Cargo.toml` has
one `[[bin]]`.

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
derive; no schema ships in `assets/` (the directory does not exist). The
dependency question is settled, not open: whistle's own payload types derive
`JsonSchema`, so `schemars 1.2.2` is now a declared, direct, versioned
dependency of `shep-cli` (Phase 13) — at zero extra compiled crates, since
`rmcp`'s `server` feature already pulls it, but still an edge this project
owns and holds a `-Z minimal-versions` floor for. What is left for
`AppConfig` is a derive and a writer, not a dependency decision.

**Daemon-config flags layer** (spec §5) — layering is `file < SHEP_* env`
today; the third, CLI-flag layer over the top does not exist.

## Known debt, recorded rather than built

Not scope cuts and not unbuilt spec surface — these are things that exist and
work, or that are known to be missing, and that Phase 10 decided to write down
rather than change. Each says what it is, why it was not done, and what would
force it.

### Automatic CI, and what it would cost to turn on

`.github/workflows/test.yml` is `on: workflow_dispatch:` — manual only. It is
correct and ready; the trigger is the only thing missing, and it is missing on
purpose while the repository is private.

The arithmetic, so the decision is about money rather than about whether the
jobs work. GitHub bills private-repository Actions minutes with a multiplier
per platform: Linux ×1, Windows ×2, macOS ×10. One run of this file is:

- `test`: 4 runners × 2 toolchains = 8 jobs — 2 of them macOS (×10), 2 Windows
  (×2), 4 Linux (×1)
- `features`: 2 jobs — 1 Windows (×2), 1 Linux
- `lint`, `docs`, `typos`, `minimal-versions`, `musl`, `coverage`,
  `privileged`: 7 Linux jobs
- `bench`: 2 Linux jobs

so 19 jobs, of which the two macOS legs dominate the bill at ten times their
wall-clock. A `push`+`pull_request` trigger runs the whole file on every commit
to a branch with a PR open; a `schedule` row adds one run a week regardless.

**The decision is Rin's and has been made for now: leave it manual until the
base phases ship.** Recorded here so the next person to read the workflow does
not "fix" the missing trigger, and so that every "all gates green" claim in
this project's history is understood for what it is — self-reported by the
agent that wrote the code, never independently re-run.

The job count here and the one in `.github/workflows/test.yml`'s header
comment are one fact written in two places. Change a matrix and both move.

### `reuse_port` is accepted, stored, displayed — and never read

`AppConfig::reuse_port` has no production reader anywhere in the workspace.
Reload's overlap between the old and new instance is unconditional, so the
permission this field grants is one shep already takes.

Kept rather than removed: `shep import` sets it for a cluster-mode pm2 app and
`shep flock` renders it, so deleting the field would silently drop a value out
of a config an operator handed us. It costs one `bool` per app.

It stops being inert the day shep grows a reload mode that does not overlap by
default, a `graceful = false` or a serial reload, at which point this is the
field that says which apps may be overlapped. Until then the doc comment on
the field says plainly that it does nothing, which is the part that was
missing.

### `bind_socket` surfaces an over-length `$SHEP_HOME` as a raw `ENAMETOOLONG`

Noticed while correcting the `sun_path` comments in the same task. `boot.rs`'s
`bind_socket` performs no length check of its own before handing the path to
the kernel, so an operator with an unusually deep `$SHEP_HOME` gets the OS
error with no sentence naming the limit (104 bytes on macOS, 108 on Linux) or
the variable responsible. Low impact and a small fix — a length check ahead of
the bind that names both — but not this task's subject.

### `DaemonConfig` is not a proof token, unlike `ResolvedApp`

`ResolvedApp` keeps its `config` private so that holding one proves it went
through `normalize` (`normalize.rs:63`). `DaemonConfig` does not: its `daemon`
and `dog` fields are `pub`, and the one validation it performs — the
`max_cron_sleep` floor — happens inline inside `DaemonConfig::load`
(`daemon.rs:203-210`) rather than in a `validate` step a hand-built value would
also have to pass.

Nothing constructs one by hand today outside tests, so nothing is currently
wrong. Deferred because making the fields private and splitting `validate` out
of `load` is an architectural call on a type whose shape is Rin's to decide,
not a defect with a known fix. What would force it: any production path that
assembles a `DaemonConfig` from something other than a file — the daemon-config
flags layer, for instance.

### `ProcessInfo` fuses four concerns behind one discriminator

Identity and lifecycle (`id`, `name`, `status`, `pid`, `restarts`,
`uptime_ms`, `fold`), log paths (`out_file`, `err_file`), resource stats
(`cpu_percent`, `memory_bytes`) and dog provenance (`dog`) all ride in one
struct, and a dog's row leaves several of them meaningless.

Deferred on the wire audit's own recommendation: do not split speculatively.
What would force it is the `lambs` field — the moment a row carries a process
tree, the question of what a `FlockMember` is stops being cosmetic. Phase 10
made that field cheap to add (`ProcessInfo` is `#[non_exhaustive]` with a
builder), which is deliberately the opposite of forcing the split early.

### `check_log_ancestry`'s TOCTOU window, and the Linux syscall that would close it

`check_log_ancestry` verifies a log path's ancestry and `open_log_path` then
opens it, with no atomic tie between the two. The realistic local-multiuser
attack is caught — a loose or wrong-owned ancestor is refused, and
`O_NOFOLLOW` refuses a symlink standing at the final component — but an
attacker who can rearrange a directory between the check and the open still
wins that race.

The design, written down so it does not have to be rediscovered:

- Linux fast path: `nix::fcntl::openat2` (available under the `fs` feature this
  crate already enables) with `ResolveFlag::RESOLVE_NO_SYMLINKS`, opening
  relative to a directory fd for the log directory.
- The `RawFd` it returns is adopted into a `File` with `FromRawFd`, which is
  `unsafe`, so the wrapper lives in `shep-daemon/src/sys.rs` with a per-block
  `// SAFETY:` (IR-22/23) and nothing else in the crate touches the raw fd.
- Fallback ladder: `ENOSYS` (kernel < 5.6) and `EPERM` (seccomp filters that
  do not allow the syscall) both fall through to today's
  check-then-`O_NOFOLLOW`-open path, which stays as the portable
  implementation and remains the only path on macOS.

Not built in Phase 10 because it is new `unsafe` on a Linux-only path that this
project cannot execute a test for from a macOS development machine — the exact
shape of debt the platform audit's "never been compiled" finding exists to
complain about. What would force it: a Linux box in the regular test loop, or a
threat model that includes an attacker with write access to a log directory's
parent.

### Reload's Linux-only assertions have no automatic execution

`daemon_e2e.rs`'s `a_reload_costs_a_draining_app_no_connections` and
`a_reload_costs_a_defiant_app_the_work_it_will_not_finish` each carry
`#[cfg(target_os = "linux")]` on their reload connection-count assertion
(`grep -n 'cfg(target_os = "linux")' crates/shep-daemon/tests/daemon_e2e.rs`
finds both), which is correct: they depend on Linux's
accept balancing. Their only real execution to date was one manual Docker run.
Phase 10 added the `ubuntu-24.04-arm` and `ubuntu-latest` legs that would run
them, but the workflow stays `workflow_dispatch`-only, so they still execute
only when someone presses the button. Recorded so the gap is known, not because
the tests are wrong.

### The `cli_e2e` 7-test correlation

Four of nine `cli_e2e` tests in one grouping failed under `--test-threads=1`
where zero of six in another did — investigated twice, exonerated twice as a
load artefact rather than a regression, and never measured again since Phase 6.
It is a standing false-positive risk in the serial phase-gate run that CLAUDE.md
mandates before a merge. What it needs is one fresh bounded measurement pass
with the numbers written down, which is a measurement rather than an edit, and
is why it is here rather than in a task.

### The windows-gnu cross-check went three phases unrun

`cargo check --workspace --all-targets --all-features --target
x86_64-pc-windows-gnu` was in the gate list of every plan from Phase 3 through
Phase 6. Phase 7's plan does not carry it, nor Phase 8's, nor Phase 9's, and
no plan says why — it was dropped silently. It had also never been written into
`CLAUDE.md`'s own gate section, so there was nothing outside the plans to
notice its absence.

This one is **closed, not deferred**, and is recorded here only so the gap is
dated. Phase 10 ran it (`EXIT=0`, 8.42s, 2026-08-13, at `b7c466b`) and put it
back, in `CLAUDE.md` this time rather than in a plan that expires. The likely
reason it lapsed is its prerequisite: `ring`'s build script runs `cc` for the
target, so the check needs a C toolchain for `x86_64-pc-windows-gnu`
(`mingw-w64`), and a host without one cannot run it at all — an easy thing to
stop doing and never mention. Windows was 0% implemented for all three of
those phases, so nothing broke; what was lost was the guarantee that nothing
had.

It is spelled `cargo check`, not `clippy -- -D warnings`, and that is a
decision rather than an oversight: shep-daemon's `boot`, `sys`, `server` and
`tokio_runner` are `cfg(unix)`-gated, so the Windows target reports 51
dead-code warnings for code that is not dead on any platform shep ships.
Silencing them would mean `#[allow(dead_code)]` on live code.

### `shep signal` cannot reach a sheep's lambs, on purpose

`signal` delivers to the sheep's own pid. An operator who wants a whole
process tree to get a `SIGHUP` — the nginx-worker shape — has no verb for it:
`stop` signals the group but also runs a kill ladder behind it, and there is
no group-wide nudge.

Deferred rather than built because the two are genuinely different asks and
one flag on `signal` (`--group`) would make the safe reading the non-default
one. What would force it: an app class where the sheep is a supervisor that
does not forward signals to its own workers, which is a real shape and simply
has not come up here yet.

### `lookout`'s flock table and bleats feed measure `char`s, not display columns

[`crates/shep-cli/src/lookout/view/flock.rs`](../../crates/shep-cli/src/lookout/view/flock.rs)'s
`fit` — the function every truncated line in `shep lookout` goes through —
counts `text.chars().count()` to decide where to cut and place its `…`. A
double-width character (CJK, many emoji) counts as one `char` but draws in
two terminal columns, so a NAME or a log line built from them can overrun its
column and lose the ellipsis that marks the cut.

Confirmed cosmetic, not a security issue: ratatui's `Buffer::set_line` clips
at the render area rather than bleeding into a neighbouring pane, and no ESC
or CR byte reaches a buffer cell, so there is no escape-injection path from a
hostile log line through this function — only a truncation marker that can go
missing.

Not fixed in Phase 12b, deliberately: 12a already carried this limitation for
sheep names, and 12b is the first phase to feed the same function arbitrary
log bytes rather than operator-chosen names, which is what makes it worth
recording rather than what makes it new. Fixing it means measuring display
width (`unicode-width` or equivalent) instead of `char` count — a new
dependency this phase's review declined to add for a cosmetic gap. What would
force it: an operator running `shep lookout` against sheep with CJK names or
logs, where a missing `…` is confusing rather than theoretical.

## Not deferred

**Dogs** (spec §8) **shipped**: the dog contract (`shep_daemon::dogs`,
`DogSpec`/`DogSource`) — a dog is an ordinary supervised process marked
with where it came from, not a second kind of supervision; the
`enable`/`disable`/`adopt`/`rehome`/`dogs`/`barks` verbs and the hidden
`dog <name>` re-exec dispatch; `[dog.<name>]` served over the socket via
`Request::DogConfig`, re-read per request rather than cached at boot; the
metrics dog (Prometheus exposition on `127.0.0.1:9615` by default,
reference Grafana dashboard in `assets/grafana/`); the bark dog
(`[dog.bark.sinks]` Discord/Slack/JSON webhooks, `[dog.bark.rules]`
event/`gave_up`/`restart_rate`/`memory_above` triggers with per-subject
debounce, bus-plus-poll reconciliation so a dropped event still fires);
`barks.jsonl`, the size-capped ring both the bark dog and the shepherd's
own dog-restart-budget record write to. Operator-facing contract:
`docs/dogs.md`. `[daemon] enabled_dogs` and `[dog.<name>]`
(`DaemonSection`, `crates/shep-core/src/config/daemon.rs`) have a reader
now: boot starts every enabled dog from the first, and a dog asks for the
second over the socket. What §8 still promises beyond this — OTLP export
— is separate work and remains open, above.

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

**The six daemon-surface verbs** (spec §4, §5, §6, §9) **shipped** on
`feat/phase11-verbs`: `shep stock <name> <count>` (`scale` stays as a
visible alias; absolute counts only —
scale-up fills the lowest free instance slots, scale-down releases the
highest, and the new count is written back to the muster roll so a reboot
keeps it); `shep signal <selector> <signal>`, delivered to each sheep's own
process and not its group, over `signals::OperatorSignal`'s nine names;
`shep whisper <selector> <line>` (`sendline` stays as a visible alias), for
apps whose Flockfile opts in with
`stdin = true`; the KV store (`shep set`/`get`/`unset` over
`shep_core::kv`, a `0600` `$SHEP_HOME/kv.json` under the same sibling-lockfile
and atomic-rename shape `barks.jsonl` and `shep.toml` already use, reachable
by a dog without going over the socket — operator contract: `docs/kv.md`);
`ProcessInfo::lambs` and `describe`'s tree view, populated by `Describe`
alone and captioned with what the parent-pid walk is not; and the
`channel.*` bus topic, carrying every message a sheep writes on fd 3,
including an `action-reply` no trigger is waiting for.

What each of those does NOT do, recorded so it is not rediscovered as drift:

- `scale` has no relative `+N`/`-N` form and will not grow one — an absolute
  count is idempotent and pm2's relative-remove path is one of the crashes
  the trace notes exist to keep us from reproducing.
- `scale` is refused while the same app still has instances shutting down
  from an earlier scale or delete, the way it is already refused mid-reload.
  A scale-down's reply is the survivors and does not wait for the departures,
  so those slots are still registered; a second scale counting them answered
  `Ok` for a flock that then shrank underneath the muster roll. Two `shep
  scale` calls back to back in a provisioning script need a wait between
  them, bounded by the app's own `kill_timeout`.
- `signal` refuses `SIGSTOP`: a stopped sheep still reads `online` in every
  listing the shepherd can produce, so accepting it would put the flock in a
  state shep cannot report.
- `sendline`'s `Sent` means the bytes were written and flushed to the pipe,
  not that the app read them. A pipe holds 64 KiB before it blocks, and there
  is nothing on that path that could tell the difference.
- `sendline`'s `not_written` on the TIMEOUT path does not promise the line was
  never written. The shepherd stops waiting after 2s; it cannot stop a write
  already part-way into a pipe the app is not draining, because abandoning one
  halfway would leave a partial line behind — so those bytes land in full
  whenever the app drains. A line still QUEUED behind that one is dropped once
  its caller gives up, so a retry cannot pile duplicates up and deliver them
  together, but the first line of a retry sequence can still arrive late.
  Treat a retry as a second command.
- The KV store is flat. A dot in a key is part of the name, not a path.
- `lambs` is a parent-pid walk and is not the kill unit, in both directions
  (`shep-daemon`'s `limits` module doc has the account). Only `Describe`
  populates it; `ListFlock` deliberately does not walk.
- `channel.*` carries child→shepherd traffic only. The shepherd's own
  `shutdown` and `action` writes are already reported by `process.stop` and by
  `Response::Triggered`; adding them stays additive if that changes.

**whistle** (spec §8, §13) **shipped**: `shep whistle`, an MCP server over
stdio (`rmcp`), nine tools — five read-only, always present, and four that
act, present only when `[whistle] allow_control = true` in
`$SHEP_HOME/shep.toml`. Gated-off tools are absent from the tool list, not
present and refusing. `start_sheep` is narrowed to an already-registered
sheep rather than the wider Flockfile/script form `shep start` takes, and
every other daemon refusal a control tool can meet — a reload already in
flight, an unknown sheep, a stopped shepherd — reaches the model as an
in-band tool result rather than a protocol error. Operator-facing contract:
`docs/whistle/README.md` and the generated `docs/whistle/tools.md`.

Spec §14.7 says control tools "require the daemon flag
`whistle.allow_control = true`" (`docs/specs/shep-v1.md:405-406`). That
sentence stays as written — the spec is not rewritten to match an
implementation — but Phase 13 reads "daemon flag" as the `[whistle]` section
of `$SHEP_HOME/shep.toml`, per §14.7's own "daemon config, not CLI flag",
and there is no `--allow-control` CLI flag on `shep whistle` at all.

What §8/§13 name beyond this and remain open: HTTP/SSE transport (above,
under "Committed to v1.1+ by design"), and MCP resources, prompts, sampling,
completions, subscriptions and tasks — `get_info` advertises tools only.
Six verbs an operator can run today have deliberately no tool at all:
`delete_sheep` and `flush` are irreversible in a way the four control tools
are not; `kill` takes the shepherd itself down, and whistle's own connection
with it; `signal_sheep` and `whisper` take free-form input whose blast
radius is not shep's to bound; `scale_flock` takes a count a model can be
off by an order of magnitude on. That is a judgement about what an agent
should be trusted with, not a technical limit, and it is Rin's to overrule.
