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
3. ~~**The Windows functional tier — last**~~ (Rin, 2026-08-12). **Superseded
   2026-08-15: Windows is out of v1 entirely** and moved to the v1.1+ section
   below. The estimate that was "mostly guesswork" has since been made, and it
   is what changed the decision — see
   [windows-estimate.md](windows-estimate.md).

**Dogs** (spec §8) was originally queued first and has since shipped, on
`feat/phase9-dogs`; see "Not deferred" below for what landed. **whistle**
(spec §8, §13) has since shipped too, on Phase 13; same section.

Ordering is not priority. Windows was last because its estimate was the
weakest; now that the estimate exists, it is out of v1 rather than at the end
of it.

## Committed to v1.1+ by design (spec §2)

Six deliberate scope cuts, not oversights — spec §2 carries the reasoning:

- HTTP/SSE MCP transport (whistle ships stdio-only first)
- cgroup v2 enforcement (`enforce = "kernel"`) — `LimitEnforcer`'s polling
  impl is the v1.0 tier
- `@shep/io` npm shim (built on demand)
- **The whole Windows tier** (spec §11) — 0%, not partial, and no longer a
  v1.0 target. The Windows arm of the CLI's entry point prints "shep does not
  yet support Windows" and exits `Failure` for every verb; `boot`, `sys`,
  `server` and `tokio_runner` are all `#[cfg(unix)]`. Named-pipe transport and
  Job Objects: absent.

  Rin ruled it out of v1 on 2026-08-15, once the estimate existed rather than
  being guessed. [windows-estimate.md](windows-estimate.md) is that estimate:
  roughly 36-49 tasks over 4-5 phases, and a redesign rather than a port. The
  145 `cfg(unix)` sites are the cheap part — four module trees are gated at the
  crate root despite containing no Unix calls at all. The cost is in the
  handful of places where behaviour must change, not merely the API:

  - **Graceful stop has no analogue.** `CTRL_BREAK_EVENT` reaches only console
    apps carrying their own handler, so `shep stop` degrades to `shep kill`
    for anything that did not opt into the shepherd channel.
  - **The shepherd channel cannot be fd 3.** `cmd.exe` has no fd-3
    redirection and `command-fds` is Unix-only, so the channel becomes a named
    pipe named by an environment variable. The wire format survives;
    [shepherd-channel.md](../shepherd-channel.md) does not.
  - **`user`/`group` would refuse permanently.** Dropping privilege needs a
    logon session or a primary-token privilege, which is a different feature
    rather than a different call.

  Also on the table and not yet decided: whether permanent non-support is the
  right answer. A tier is not built once, it is maintained forever on a
  platform no maintainer here runs — and the cheap Windows checks have already
  rotted twice (see the windows-gnu entry under known debt). WSL2 covers the
  common case today.
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

### `DaemonConfig` is not a proof token, unlike `ResolvedApp` — resolved, Phase 14

Phase 14's daemon-config flags layer was the thing that would force this
question, and it landed, so the question is answered rather than open.

`ResolvedApp` keeps its `config` private so that holding one proves it went
through `normalize`. `DaemonConfig` does not, and does not become one:
`validate` moved out of `load` into its own private method, called once at
the bottom of the new `load_layered` (`file < env < flags`, exactly one
validation pass, so a good `--max-cron-sleep` can rescue a broken
`shep.toml`) — but `daemon` and `dog` stay `pub`, the same as before.

The type is `#[non_exhaustive]` now, and that attribute is **for field
growth**, not for this. `DaemonConfig` has grown a section per phase and will
grow another; without the attribute each one is a breaking change for an
out-of-tree struct literal. It does **not** prove a value was validated:
`#[non_exhaustive]` blocks a struct literal and functional-update syntax from
outside the crate, but not field mutation —
`DaemonConfig::default().daemon.max_cron_sleep = Some(…)` compiles fine and
walks straight past it. The contract is stated in the type's own doc comment,
not enforced: `load` and `load_layered` are the validating constructors, and a
caller that mutates a loaded config afterwards is out of contract, silently.

Nothing in this codebase is in that position today — every call site loads a
`DaemonConfig` and consumes it within a few lines (`run_daemon`, the dogs
subsystem's `[dog.<name>]` read, whistle's `gate.rs`) — so nothing is
currently wrong. The escape hatch, if an out-of-tree caller ever needs to
mutate a loaded config and re-check it, is to make `validate` `pub`: a
one-line, non-breaking addition. Fields do not need to go private for that.

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

### A `.js` Flockfile has no evaluation timeout

A `.js` module handed to `require()` and never returning — one that starts a
server at require time rather than exporting config — hangs `shep start`
forever. There is no bound on the wait.

Not built because a bound means a reaper thread in a crate that forbids
unsafe code (`#![forbid(unsafe_code)]` on shep-cli), for a case where the
process is in the foreground, attached to the operator's own terminal, and
already interruptible with Ctrl-C. What would force it: any path that
evaluates a `.js` Flockfile unattended — a CI job or a provisioning script
running `shep start` non-interactively, where nobody is watching to press
Ctrl-C.

### The missing-node error message has no test

`shep start <path>.js --flockfile` on a machine with no `node` on `PATH`
produces a specific sentence (`crates/shep-cli/src/commands/lifecycle.rs`),
but nothing exercises that code path under test. Producing it for real needs
a `PATH` with no `node` on it, and mutating `PATH` for the duration of one
test means `std::env::set_var`, which is `unsafe` in edition 2024 — in a
crate that forbids unsafe code. The sentence is pinned instead as an exact
substring in `docs/migration.md`, which drifts from the code the moment
either one is edited without the other, `grep`-checked but not
`cargo test`-checked.

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

**`shep startup` / `shep unstartup`** (spec §9, §11) **shipped** for all four
of spec §11's init systems, as of Phase 14: `commands::startup` renders a
systemd `Type=notify` unit, a `launchd` `LaunchDaemon` plist, an openrc init
script, or a FreeBSD/OpenBSD `rc.d` script (`commands::startup::unit`),
installs or removes it privilege-gated by `geteuid()`, and `shep daemon
--foreground` (`crates/shep-daemon/src/notify.rs`) reports `READY=1` once the
muster restore has finished so the systemd unit does not go green over an
empty flock. On Linux, which init is active is now a runtime probe —
`/run/systemd/system` a directory means systemd, `/run/openrc/softlevel` or
`/run/openrc` a directory means openrc, neither means refuse naming both
paths — because systemd and openrc share one `target_os` and cannot be told
apart at compile time. FreeBSD and OpenBSD still resolve at compile time; the
probe only exists because Linux needed it. `--init
<systemd|openrc|launchd|freebsd-rc|openbsd-rc>` on `startup` and `unstartup`
overrides the probe on any target.

Three caveats, stated rather than buried:

- **Behaviour change on Linux.** Before Phase 14, every Linux build got
  `Init::Systemd` unconditionally, so a container with no
  `/run/systemd/system` was written a systemd unit that nothing would ever
  read. It is now refused — the correct answer, but a case that worked before
  and does not after. `--init systemd` restores the old behaviour for a
  container where that is actually wanted.
- **openrc has no readiness protocol.** There is no `sd_notify` analogue, so
  the openrc script's `start_post()` polls the shepherd's own control socket
  instead and blocks the "started" verdict until the first request is
  answered — which happens only after the muster restore and the dogs are up,
  the same milestone `READY=1` proves on systemd, one step later. FreeBSD gets
  the same poll through `start_postcmd`. OpenBSD's `rc.subr` has no
  documented post-start hook at all; its script reports started as soon as
  the process is spawned and says so in its own header comment, naming
  `shep flock` as the real check.
- **None of the three new scripts has been executed on its own operating
  system.** No FreeBSD, OpenBSD, or openrc host exists on this machine. They
  are pure `format!` output pinned by exact-string tests — the same tier the
  systemd unit has always had, since it too has only ever been *rendered*, on
  a Mac. That is a real and adequate tier for text; it is not a claim that the
  scripts work. Nothing in the docs claims the BSD or openrc scripts are
  supported until someone reports back from a host that actually runs one.

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

**`.js` Flockfile** (spec §5) **shipped**, Phase 14, with a ruling narrower
than the spec's own phrasing suggests: explicit only, never by directory
discovery and never by extension alone. `shep start <path> --flockfile` reads
`<path>` by shelling out to `node -e` with a small `try`/`catch` wrapper
(`JS_BRIDGE_SCRIPT` in `lifecycle.rs`) that requires the module and writes
its JSON to stdout, or `err.message` to stderr on failure — not `node -p`
bare, whose crash dump on an uncaught exception ends with a trailing
`Node.js vX.Y.Z` banner line rather than the actual error — and feeding the
result through the existing JSON parser; without the flag,
`shep start server.js` still means exactly what it always has — start
`server.js` as a script. The ten-name `DISCOVERY_ORDER` is unchanged and still
has no `.js` entry in it. The document it reads is Flockfile-shaped (an `app`
array, sheep-native field names), not a pm2 `ecosystem.config.js` — pointing
`--flockfile` at a real pm2 ecosystem file gets serde's own `unknown field
`apps`, expected `app`` refusal, and `shep import` remains the only pm2 path.
A `.js` module that never returns hangs `shep start` forever; there is no
timeout, recorded as known debt below.

**schemars JSON-schema export** (spec §5) **shipped**, Phase 14, behind a
non-default `schema` feature on shep-core that shep-cli turns on. The schema
describes the Flockfile **document** (generated from `RawFlockfile`, the
private type serde actually deserializes into, not from `AppConfig` alone —
an `AppConfig`-only schema would reject every real Flockfile, since a
Flockfile is `{"$schema": …, "app": […]}` and not an `AppConfig` object
itself), with `AppConfig` and its nested types referenced from `$defs`. It is
committed at `crates/shep-core/assets/flockfile.schema.json`, generated by the
hidden `shep schema` verb, and drift-guarded by an `include_str!` plus a
co-located test in shep-core — editing an `AppConfig` field or its doc comment
without regenerating the artefact fails `cargo test -p shep-core`. The schema
describes the deserializer, not the `normalize` step: `kill_signal` is an
unconstrained string in the schema even though `normalize` narrows it to five
signal names later.

**Daemon-config flags layer** (spec §5) **shipped**, Phase 14:
`DaemonConfig::load_layered` adds a third layer, `file < env < flags`, over
the `file < env` `load` already did. `shep daemon` gains `--log-json[=BOOL]`,
`--log-level <LEVEL>`, `--socket <PATH>` and `--max-cron-sleep <DUR>`, one
per `SHEP_*` variable `load` already reads and no others — `enabled_dogs` and
`adopted_dogs` stay `shep enable`/`shep adopt`-only, with no env or flag layer
of their own. Validation happens once, after all three layers are merged, so
a good `--max-cron-sleep` can rescue a `shep.toml` whose own value is below
the floor — the same reasoning that already governed `file < env`. The
boolean grammar (`1|0|true|false`) is shared between the env reader and the
flag's `value_parser` through one exported function, `parse_daemon_bool`,
rather than widened to clap's own broader `yes/no/y/n/on/off` grammar.

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

**`shep serve`, `shep dev` and `shep runtime`** (spec §9, §13) **shipped**,
Phase 15, closing the last three v1.0 verbs and the `[[bin]]` gap this file
used to name:

- **`serve` is hand-rolled, not axum/tower-http** (Rin's ruling, 2026-08-15).
  `crates/shep-cli/src/serve/` is six modules — `path`, `fs`, `mime`,
  `listing`, `auth`, `worker` — over `http.rs`, which moved up out of `dog/`
  to the crate root to serve both.
- **Directory listing is off by default**, where pm2's is on — `--listing`
  opts in. A listing publishes every filename under the directory.
- **Dotfiles are refused by default**, where pm2's `serve` publishes them —
  the reverse of the listing flip and the same argument. `--hidden` opts
  in; `.well-known/acme-challenge` is the use case it exists for.
- **No range requests, no conditional requests, no ETags, no compression,
  no keep-alive, no TLS, no HTTP/2, no `PM2_SERVE_*` compatibility.** None
  are named in spec §9's serve sentence; shep reads only `SHEP_`-prefixed
  variables. Range and conditional requests are v1.1 candidates — the
  visible cost today is no video seeking and a full re-read per request.
- **Exit code 11 (`flock_empty`) exists, and code 2 is clap's alone.**
  `runtime`'s fail-fast status collided with clap's usage-error code; an
  orchestrator cannot act on a status that means both "bad flag" and "dead
  app", so it now has its own code — 0 if the flock emptied clean, 11 if a
  sheep ended in `errored`.
- **`runtime` splits into a separate init process when it is PID 1**, rather
  than reaping in the supervisor's own process. An in-process subreaper
  loop would race tokio's own child reaping and corrupt the exit statuses
  spec §4 promises are exact; the init instead calls
  `set_child_subreaper`, forwards SIGTERM/SIGINT/SIGHUP/SIGQUIT to the
  supervisor it spawns, and reaps every orphan itself.
- **`serve`'s remaining symlink race, stated as what it is**: the leaf open
  (`fs::open_regular`) carries `O_NOFOLLOW`, but the component walk that
  precedes it is not atomic. What that leaves an attacker who can create
  files in the docroot between the walk and the open is a refusal or a
  directory they already controlled — not a read outside the docroot.
- **Any symlink under the docroot is refused by default, not only one that
  leaves it** — only the docroot itself may be a symlink unless the
  operator says otherwise. An in-docroot symlink pointing back inside the
  docroot (`dist/current -> ../releases/2026-08-15`, a symlinked `assets/`)
  404s by default, where pm2's serve and a canonicalize-then-check design
  both serve it — the deliberate cost of closing the TOCTOU above without a
  per-request `canonicalize`. It is off by default, one flag away:
  `--follow-symlinks` opts back into canonicalize-then-check, reopening the
  race, with a startup notice and, in the default mode on refusal, a
  per-request stderr line naming the path so the choice and its cost are
  never silent.
