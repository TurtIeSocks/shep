# Deferred, closed

The closed half of [deferred.md](deferred.md), split out on 2026-08-29.

Everything here is FIXED, STALE, resolved, rejected, or simply what shipped.
None of it is outstanding work. It is kept rather than deleted because several
of these entries record a WRONG diagnosis and how it was found, which is the
part no commit message carries and the part most likely to be re-derived by
somebody hitting the same symptom.

Nothing on this page is parsed by anything. `web/src/data/chalkboard.ts`
imports deferred.md, not this file, and only the two sections that stayed
there.

### `cmd /C` cannot carry a quoted script from `std::process::Command` -- and it is not shep's bug

Found 2026-08-26 while porting `daemon_e2e.rs`, first misdiagnosed, then
measured. Recorded because the wrong diagnosis is the instructive part.

**What it looked like.** A sheep configured `wait_ready = true` that
announces itself on the shepherd channel went `online` on unix and hung to
its `listen_timeout` on Windows. The natural reading was that shep's Windows
channel -- the `SHEP_CHANNEL_PIPE` replacement for fd 3 -- did not work, and
that is what this entry said for about an hour.

**It was wrong. The channel works.**
`real_runner_windows.rs::a_child_reaches_the_shepherd_channel_by_pipe_name`
now proves it directly, below the daemon: a child opens
`%SHEP_CHANNEL_PIPE%` by name, writes one line, and the runner's pumps
deliver it on `ProcIo::from_child`.

**The actual cause is a Windows argument-passing rule worth knowing.**
`std::process::Command` escapes an argument's inner quotes as `\"`, which is
the MSVC C runtime's convention. **`cmd.exe` does not use that parser** -- it
takes the backslash literally. So a fixture built as
`cmd /C (echo {"kind":"ready"}) > "%SHEP_CHANNEL_PIPE%"` arrives at cmd with
mangled quoting, and the redirect fails with "The filename, directory name,
or volume label syntax is incorrect". The line never reaches the pipe, and
the sheep never announces itself.

The tell was there from the first run and I read past it: the fixtures that
PASSED (`forever_app`, `announce_app`) contain no quotes, and every one that
failed does.

**The fix, in both e2e files: write a script FILE.** A `.cmd` file's
contents go through no argument escaping at all. `cli_e2e.rs` already worked
this way, which is why it never hit this.

**What this means for operators, and it is a real constraint rather than a
test detail:** a Flockfile app whose `script` is `cmd` and whose `args`
contain double quotes will hit exactly the same wall. The workaround is the
same -- put the script in a `.cmd` file and point `script` at it. There is
nothing shep can do about it short of `CommandExt::raw_arg`, which would
mean shep guessing at quoting on the app author's behalf.

### Every Windows daemon shutdown returned `Err` from `run()` -- FIXED

Found 2026-08-26 by porting `daemon_e2e.rs` to Windows, and it is the
clearest argument for doing that port at all: nothing else had noticed.

`RunningDaemon::run`'s teardown unlinks what boot created — the socket and
the pidfile. On Windows the control address is a named pipe, so
`std::fs::remove_file("\\.\pipe\shep-...")` is not a harmless no-op: it
fails with `ERROR_INVALID_PARAMETER` (87), and that error became `run()`'s
return value on **every** clean shutdown.

**It was invisible from the outside**, which is why it survived a full
manual verification earlier the same day. `shep kill` does not read that
`Result` — it reports success from the RPC reply and the address going
quiet, both of which were correct — so a live flock looked entirely healthy.
`daemon_e2e`'s fixture unwraps the value, and reddened on the first run.

Fixed by skipping the socket unlink on Windows, where a pipe stops existing
when its last handle closes and teardown has nothing to do that the kernel
is not already doing. The pidfile unlink is unchanged on both platforms.

**The general lesson is worth more than the fix.** A no-op-looking cleanup
call on a path that is not a filesystem path is exactly the shape of bug an
end-to-end tier catches and a manual run does not, because the manual run
reads the operator-facing report and the report was fine.


### `shep start` hung when its stdout was a PIPE, on Windows -- FIXED

Found 2026-08-26 by a smoke test, minutes after the Windows tier was
otherwise working, and fixed the same day. Recorded because the WRONG
reasoning that allowed it is easy to write again.

```
shep start                 # fine
shep start | Out-Null      # hung forever, before the fix
shep flock | Out-Null      # always fine -- spawns nothing
```

**The mechanism.** The detached shepherd inherited the parent's stdout pipe
handle. It never wrote to it — its own stdout goes to
`$SHEP_HOME/logs/shepd.out.log` — but a pipe cannot reach EOF while any
handle to its write end is open, so the reader waited forever. The decisive
evidence was that killing `shep` did not release the pipeline: `timeout 45
shep start | cat` outlived its own timeout, because `cat` was waiting on the
daemon rather than on `shep`.

**The wrong claim that allowed it**, which is the part worth keeping.
`launch.rs`'s Windows arm carried a comment saying no `seal_inherited_fds`
counterpart was needed, because Windows inherits only handles explicitly
marked inheritable and `Command` marks exactly the three stdio handles it
sets. Half of that is true and it is the wrong half: `CreateProcess` with
`bInheritHandles = TRUE` — which `std` passes whenever any stdio is
redirected — hands over **every** inheritable handle the parent holds, not
only the ones `std` prepared. "Windows does not have `fork`" is not the same
statement as "Windows does not over-inherit".

**The fix** is `shep_daemon::sys_windows::seal_std_handles`, called
immediately before the spawn: it clears `HANDLE_FLAG_INHERIT` on this
process's three standard handles, which is the exact analogue of what
`seal_inherited_fds` does with `FD_CLOEXEC` on unix. It changes only the
inherit flag, so this process goes on using its own stdio unchanged.

**A false negative is also recorded, because it cost real time.** The first
verification after the fix reported the hang as still present. It had not
reproduced — a pre-fix daemon from an earlier run was still alive, so
`shep start` connected to that one instead of spawning a fixed one, and the
old daemon was still holding an older shell's pipe. The tell was in the
output and was noticed without being acted on: a just-started daemon
reporting `uptime 2m 59s`. **Kill every stray `shep` before testing a change
to the launch path**, or the thing under test is not the thing running.

Verified after a clean kill, twice: with an empty flock, and with a live
sheep. Both completed with exit 0 and the payload arrived through the pipe.

### Automatic CI, and what it would cost to turn on

**This entry was stale and is corrected here, 2026-08-19.** The workflow has
run on `push` and `pull_request` since 2026-08-16, and the repository is
public, so standard runners are free and the arithmetic below is history
rather than a live decision. It is kept because the per-platform multipliers
would matter again if the repository ever went private.

The arithmetic, so the decision is about money rather than about whether the
jobs work. GitHub bills private-repository Actions minutes with a multiplier
per platform: Linux ×1, Windows ×2, macOS ×10. One run of this file is:

- `test`: 4 runners × 2 toolchains = 8 jobs — 2 of them macOS (×10), 2 Windows
  (×2), 4 Linux (×1)
- `features`: 2 jobs — 1 Windows (×2), 1 Linux
- `lint`, `docs`, `typos`, `minimal-versions`, `musl`, `windows-gnu`,
  `coverage`, `privileged`: 8 Linux jobs
- `bench`: 2 Linux jobs

so 20 jobs, of which the two macOS legs dominate the bill at ten times their
wall-clock. A `push`+`pull_request` trigger runs the whole file on every commit
to a branch with a PR open; a `schedule` row adds one run a week regardless.
(`windows-gnu`, added 2026-08-25 to close the cross-check gap below, is a
Linux job cross-compiling to the GNU target -- it costs nothing extra on the
multiplier table even though it targets Windows.)

**Superseded 2026-08-16: the maintainer turned it on.** `push` to `main` and
`pull_request` now trigger the file. The weekly `schedule` row is still off,
because a full 19-job run against an unchanged tree spends the expensive part
of this file to learn nothing; it is worth adding once the repository is public
and the runs are free.

The reason this entry still matters: every "all gates green" claim in this
project's history predates the first automatic run, and each was self-reported
by the agent that wrote the code rather than independently re-run. The first
real CI run is the first outside check this project has ever had.

The job count used to be written in two places -- here and in
`.github/workflows/test.yml`'s own header comment -- and had to move in step.
As of 2026-08-25 the workflow's header no longer restates the count (the
private-repo premise it existed to cost out is gone), so this file is now the
only place it lives. Change a matrix and update it here.

### `reuse_port` is accepted, stored, displayed — and never read -- FIXED, 2026-08-28

`AppConfig::reuse_port` had no production reader anywhere in the workspace.
Reload's overlap between the old and new instance was unconditional, so the
permission this field granted was one shep already took, and `normalize`
refused it outright rather than accept a knob nothing implemented.

The entry predicted its own ending correctly: "It stops being inert the day
shep grows a reload mode that does not overlap by default, a `graceful = false`
or a serial reload, at which point this is the field that says which apps may
be overlapped." That day was 2026-08-28, and what forced it was not tidiness.

**A reload's readiness probe was being answered by the instance the reload was
replacing.** `await_ready`'s `Probe` arm probes at t=0, deliberately, so a fast
app is not held at `starting` for a whole interval; at t=0 the outgoing
instance is still bound to the address the probe names, because the drain runs
only after readiness resolves. An address probe cannot say which process
answered it, and an overlapping reload exists to have two. Found by deploying
real repositories against a real shepherd — a release whose listener bound the
wrong port was verified, recorded as deployed, and reported `exit 0`, with the
app down behind it.

So a probed app with no `reuse_port` now reloads SERIALLY (`ReloadMode` in
`supervisor.rs`): drain, then spawn into the empty slot, where the only process
that can answer the probe is the replacement. `reuse_port` is the opt-in back
to the overlap, for an app that really does set `SO_REUSEPORT` and can
therefore hold two instances on one port. Nothing needed migrating: `normalize`
refused the field, so no config that loads could contain it.

`shep import` still does not write it for a cluster-mode pm2 app, which is
deliberate and unchanged. Only the operator knows whether the app actually
calls `reusePort: true`, and asserting it on their behalf would buy back the
overlap that produces `EADDRINUSE` — see `from-pm2.astro`.

### `bind_socket` surfaces an over-length `$SHEP_HOME` as a raw `ENAMETOOLONG` -- FIXED, Phase 17

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

### Reload's Linux-only assertions have no automatic execution -- STALE, closed 2026-08-25

`daemon_e2e.rs`'s `a_reload_costs_a_draining_app_no_connections` and
`a_reload_costs_a_defiant_app_the_work_it_will_not_finish` each carry
`#[cfg(target_os = "linux")]` on their reload connection-count assertion
(`grep -n 'cfg(target_os = "linux")' crates/shep-daemon/tests/daemon_e2e.rs`
finds both), which is correct: they depend on Linux's accept balancing.

**This entry described a real gap once and does not anymore.** It was
written when the workflow was still `workflow_dispatch`-only; that changed
2026-08-16 (see "Automatic CI" above), and the entry was never revisited
after. As of 2026-08-25: the `test` job's `ubuntu-latest` and
`ubuntu-24.04-arm` legs run `cargo test --workspace --locked --all-features`
on every `push` to `main` and every `pull_request`. Neither leg's `--skip`
filter (`::slow::`, `two_concurrent_boots`) matches either test name, the
whole `daemon_e2e.rs` file is `#![cfg(unix)]` so it compiles on both Linux
legs, and each leg's own `target_os` is `linux`, so the `#[cfg(target_os =
"linux")]` numeric assertions themselves are compiled in and run, not just
the platform-neutral half of each test. No workflow change was needed; the
gap the entry named had already closed and nobody had said so.

### The `cli_e2e` 7-test correlation -- STALE, and what was really there

**This entry no longer reproduces.** Measured 2026-08-25: four serial runs of
`cargo test -p shep --test cli_e2e --all-features -- --test-threads=1`, all
**71 passed, 0 failed**, including one under 28 CPU burners on 14 cores with
the load average climbing from 15 to 34. All 71 also pass individually.

The harness gained `DaemonGuard`, `sweep_flock`, `graceful_kill` and a
correctly sized `CMD_TIMEOUT` after Phase 6, which is exactly the leak class
this entry described. It was fixed by other work and nobody came back to say
so.

**In-process shared state is absent by construction rather than by luck**, and
that is worth recording so the next person does not re-audit it: `cli_e2e.rs`
contains no `std::env::set_var` and no `set_current_dir` anywhere. Every
`.env(..)` and `.current_dir(..)` is the child-scoped `Command` method, every
invocation carries `--home <tempdir>` through `fn shep()` or `SHEP_DEV_HOME`,
and `free_port` and `serve_raw_response` bind `127.0.0.1:0` per case.

**Real shared state did turn up, just not this one.** The machine was carrying
70 orphaned `shep daemon` processes, the oldest six days old, each holding a
deleted temporary home. Two tests leaked one per run because they call
`shep start --flockfile` on a file that will be refused, and `start`
autostarts a shepherd before it ever opens the file. Fixed, and the fix is
measured: one leak per run each before, none after, and the whole suite adds
no new pids where it previously added two.

**One product question falls out of that**, recorded rather than answered:
`shep start --flockfile broken.js` autostarts a shepherd, then refuses the
file. So a command that fails still leaves a daemon running. That may be the
right trade, since the shepherd is what the next command wants anyway, but
nothing says it out loud today.

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

**Now closed the other way too, 2026-08-25.** Being in `CLAUDE.md` only ever
meant a human had to remember to run it, which is exactly the failure mode
that let it lapse for three phases the first time.

**One correction to the above, made 2026-08-29 while archiving this entry.**
It said the job runs "on every push and pull request". It does not: it gates
on `needs.changes.outputs.rust == 'true'`, so a docs-only change skips it.
That is the right behaviour and it is still a workflow rather than a human
remembering, which is the point the entry was making. The claim was just
wider than the truth.

The `windows-gnu` job in
`.github/workflows/test.yml` runs the identical command, cross-compiling
from `ubuntu-latest` with `apt-get`'s
`mingw-w64` rather than a native Windows runner: `ring`'s build script needs
a real GNU `cc` regardless of host OS, and a cross-compiled Windows binary
cannot be executed on the Linux host either way, so `check` on Linux costs
one Linux-priced job instead of a Windows-priced one for a target the `test`
job's native `windows-latest` legs do not otherwise cover (those exercise the
MSVC target, not GNU). Verified locally first: `EXIT=0` with a scratch
`CARGO_TARGET_DIR`, 2026-08-25.

### `lookout`'s flock table and bleats feed measure `char`s, not display columns -- FIXED, 2026-08-26

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
the names in the flock table, and 12b is the first phase to feed the same function arbitrary
log bytes rather than operator-chosen names, which is what makes it worth
recording rather than what makes it new. Fixing it means measuring display
width (`unicode-width` or equivalent) instead of `char` count — a new
dependency this phase's review declined to add for a cosmetic gap. What would
force it: an operator running `shep lookout` against a flock with CJK names or
logs, where a missing `…` is confusing rather than theoretical.

**Fixed 2026-08-26, and the dependency argument had expired.** `unicode-width`
was already in this tree twice over — `ratatui-core` pulls it for its own
grapheme measurement, and `shep-core` reaches it through `serde-saphyr`'s
`annotate-snippets` — so naming it directly in `crates/shep-cli/Cargo.toml`
added **zero crates**. `Cargo.lock` resolves one `unicode-width`, before and
after.

**Three call sites, not one.** Reading the code to fix `fit` turned up the
same fault in the other two places shep pads a cell, neither of them recorded
here:

- `output/width.rs`'s `visible_width`, which every cell of the box-drawn
  table (`shep style full`, the default) is padded by. Its own doc called the
  `char` count "a deliberate floor" whose alternative was "a `unicode-width`
  dependency for a case nobody has hit" — true when written, and the same
  sentence that stops being true the moment `lookout` needs the crate anyway.
- `output/table.rs`'s `render_table`, the `shep style plain` renderer, which
  measured with `chars().count()` and padded with `{cell:<width$}` — a format
  spec that pads by character count, so measuring alone would not have fixed
  it.

Fixing one and not the others would have left the same CJK name aligned under
`full` and crooked under `plain`, which is worse than uniformly wrong. All
three now share one rule, `output::width::char_columns`, and the two `str`-level
questions stay separate on purpose: `visible_width` discounts ANSI escapes
because its callers write raw bytes to a terminal that interprets them, and
`fit` does not, because ratatui never interprets an escape inside a `Span` —
a log line carrying `\x1b[32m` draws a literal `32m` and occupies three
columns. Measuring that as zero would under-count the exact cell the fix was
for.

`fit` now returns exactly `width` columns in **both** branches. A double-width
character that will not fit the last column before the `…` is dropped and the
column padded, because there is no half of it to draw and a short cell shifts
every column after it on that row alone.

Mutation-checked: restoring the `char` count reddens 6 tests across all three
surfaces. Grapheme clustering is still not done and is still a deliberate
floor — a combining mark measures zero and rides along with its base
character, which is the case that matters, and both truncating callers stop
on a whole `char` boundary.

### A `.js` Flockfile has no evaluation timeout -- FIXED, 2026-08-28

`evaluate_js_flockfile` takes a budget now, `JS_EVAL_BUDGET` is 30s, and node
is killed once it passes. The refusal exits `InvalidConfig` and names the
cause: *node was still running <path> after 30s, so shep killed it; a
Flockfile module has to export its config and let node exit, and one that
leaves a server listening or a timer armed does not.*

What the budget waits for is node EXITING, not `require` returning. A module
can assign `module.exports` and return while an armed timer holds the event
loop open, which is the shape the unit test uses.

**The reason recorded here for not building it was wrong**, which is the part
worth keeping. A bound needs no reaper thread and no unsafe. `Child::try_wait`
in a poll loop is safe std, and `commands/dogs.rs` already ran that exact loop
for `adopt`'s exec probe. What the threads in `commands/bounded.rs` are for is
a different problem: a pipe holds 64 KiB, so a child filling stdout blocks and
never exits, and a deadline that read the pipes after the wait could not fire
on the loudest children it exists for. `Command::output` spawns them for the
same reason. The budget covers the reads as well as the wait, so a process
node left behind on an inherited pipe cannot hold `run_bounded` past the
deadline. It can outlive the budget perfectly well, and shep has no handle on
it: that process is not shep's child. So the case gets its own answer and its
own sentence, because node exited on its own there and nothing was killed,
and a refusal claiming a kill would be describing a different failure.

**No knob.** 30s is a const, not a flag and not a `shep.toml` key. Nothing
honest reaches it, and a config that does has a bug the operator wants to hear
about rather than a setting they want to raise. Adding one later is additive.

### The missing-node error message has no test -- FIXED, Phase 17

`shep start <path>.js --flockfile` on a machine with no `node` on `PATH`
produces a specific sentence (`crates/shep-cli/src/commands/lifecycle.rs`),
but nothing exercises that code path under test. Producing it for real needs
a `PATH` with no `node` on it, and mutating `PATH` for the duration of one
test means `std::env::set_var`, which is `unsafe` in edition 2024 — in a
crate that forbids unsafe code. **Fixed in Phase 17**, and the reasoning above was wrong in one place worth
naming. `set_var` is only needed by a UNIT test, which would have to mutate
its own process. `cli_e2e` already runs shep as a subprocess, and
`Command::env` sets the CHILD's environment: no unsafe, nothing racy, and the
parent's `PATH` is untouched. The test runs a `.js` Flockfile with an empty
`PATH` and asserts the sentence names both the cause and the fix.
Mutation-checked -- restore a real `PATH` and it fails with a different error,
which is what proves it exercises the missing-node path rather than passing
by accident.

### Two `# Panics` sections without `#[track_caller]` -- FIXED, Phase 17

`crates/shep-daemon/src/fake.rs` has seven `spawn_index` accessors that
document a `# Panics` section and carry no `#[track_caller]`, and
`CronSchedule::next_after` in `crates/shep-core/src/config/cron.rs` is the
same shape. IR-21 wants the two to travel together or not at all, and this
crate's own `limits/mod.rs` says so in a comment.

Not urgent, and deliberately not done before the first publish. Adding the
attribute is purely additive, so it can ship in any later version without a
breaking change, and `fake.rs` sits behind the non-default `test-fakes`
feature so it never reaches docs.rs. What would force it: a panic from one of
those accessors pointing at the accessor rather than at the caller that passed
the bad index, which is exactly the debugging cost IR-21 exists to avoid.

### A Flockfile's relative `script` resolves against the daemon's cwd -- FIXED, Phase 17

Found 2026-08-19 by the maintainer, asking what `cwd` does while writing `shep init`'s
skeleton. Measured with three distinct directories so nothing could be
confused for anything else.

A Flockfile app whose `script` is relative and which sets no `cwd` does NOT
resolve that script against the Flockfile's own directory. It resolves against
**the daemon's** working directory, which is whatever directory the shepherd
happened to be autostarted from and is invisible from the command line.

```
daemon cwd = /home    Flockfile = /proj    invoked from = /caller
[[app]] name = "x", script = "./sub/prog"
-> error[spawn_failed]: No such file or directory (os error 2)
```

Invoking from the Flockfile's own directory changes nothing; the caller's cwd
is not consulted either. Adding `cwd = "/proj"` to the entry fixes it at once.
The ad-hoc path is unaffected and behaves sensibly: `shep start ./x`
canonicalises the script and sets `cwd` to the caller's directory
(`lifecycle.rs:358`), which is exactly what `6cf7124` established.

**Why this is worse than it first looks.** A Flockfile is a file you commit
and share. Its behaviour here depends on invisible daemon state, so the same
committed file works on the machine where the shepherd was started in the
right place and fails on the one where it was not, with an error naming
neither cause. `normalize()` already refuses `watch` without a `cwd` for a
closely related reason (`WatchWithoutCwd`: defaulting to the daemon's cwd
"risks watching the whole filesystem under a systemd unit"), so the hazard is
recognised in one place and not the other.

**Options, none chosen yet.** Resolve a relative `script` against the
Flockfile's directory, which is what a reader expects and what makes a
committed file portable, at the cost of changing behaviour for anyone relying
on today's. Or default `cwd` to the Flockfile's directory when unset, which is
the same fix expressed where an operator can see it. Or refuse a relative
`script` with no `cwd`, mirroring `WatchWithoutCwd` exactly, which is the
smallest change and turns a silent misfire into a message. **The middle option
also fixes the documentation problem it caused:** `shep init`'s skeleton wants
to say something true about `cwd`, and today the honest sentence is awkward
because the answer differs between the ad-hoc and Flockfile paths.

**Fixed in Phase 17.** the maintainer chose the second option: an app that names no
`cwd` gets the Flockfile's own directory, absolutised, so the rule fits in
one sentence an operator can read. Verified against a real daemon with three
distinct directories -- shepherd in one, Flockfile in another, invocation in a
third -- and the child now runs where its Flockfile lives.

### `~/` is not expanded in any path a Flockfile carries -- FIXED, Phase 17

Found 2026-08-19, immediately after the cwd finding above and by the same
route: the maintainer wrote `cwd = "~/web-server"` as an example in `shep init`'s
scaffold, and it does not work.

```
[[app]] cwd = "~/web-server"
-> error[spawn_failed]: No such file or directory (os error 2)
```

There is no tilde handling anywhere in the workspace. `assemble.rs:150` is
`config.cwd.as_ref().map(PathBuf::from)`, so shep looks for a directory
literally named `~`. This is correct at the layer it sits in -- `~` is a shell
feature, expanded before a program ever sees an argument -- and a value read
from a file has no shell between it and the parser.

**the maintainer's decision: shep should expand it.** In her words: "it does kind of seem
like a daemon should somewhat emulate the jobs of a shell? Since we're
replacing the functionality for someone to use `bun run index.js --cwd
'/srv/server'`." A process manager stands in for the interactive shell that
would otherwise have started the process, so inheriting a narrow piece of the
shell's job is coherent rather than sloppy.

**Scope, and she named it precisely as `~/`.** That is the right line and it
is worth keeping:

- `~/...` -- the invoking user's home. Cheap, unambiguous, no lookup.
- `~user/...` -- another user's home. Needs a passwd lookup, and under a
  systemd unit the answer is not obviously the one anyone meant. Recommend
  refusing rather than half-supporting.
- `$VAR` and `${VAR}` -- NOT in scope. Once a config file starts doing
  variable expansion it is a shell, and the question of which environment
  (the operator's, the daemon's, the app's own `env` table) becomes real and
  has no good answer.

**The trap to avoid is expanding in one field and not the others.** Four
fields carry paths: `script` (app.rs:77), `cwd` (81), `out_file` (153) and
`err_file` (155). Expanding `~/` in `cwd` alone would be worse than expanding
nowhere, because it teaches that tildes work and then fails somewhere else.
Whatever lands must cover all four, and a test should assert that -- ideally
one that enumerates the path-bearing fields so a fifth added later fails until
it is handled.

**Where it belongs.** `normalize()` is the natural home: it already turns an
`AppConfig` into a `ResolvedApp` and already refuses several shapes, so
expansion is the same kind of work at the same seam, and it happens once
rather than at every use. Note the daemon may run as a different user than the
CLI, which is exactly why this must resolve where the config is normalised
rather than where it is executed.

**Also fix the error.** Whatever is decided, `No such file or directory (os
error 2)` names neither the cause nor the fix. A path that starts with `~`
and was not expanded should say so.

### `shep adopt`'s vetting runs the candidate against the WRONG `$SHEP_HOME` -- FIXED, 2026-08-25

**Fixed in `8a8056b`, by (1) and (3) below.** `vet_binary` now takes the
home this invocation resolved and passes it to the probe, so `shep adopt
--home /tmp/scratch ./my-dog` vets the candidate against `/tmp/scratch` and
not against whatever the shell happened to have. The probe's stdin, stdout
and stderr all go to `Stdio::null()`, so a candidate can no longer write on
the operator's terminal during the command that is deciding whether to trust
it.

**(2) was considered and deliberately not taken.** A real adopted dog runs
with the daemon's own filtered environment, so `env_clear()` would vet under
stricter conditions than the dog will ever meet, and a binary needing
`DYLD_LIBRARY_PATH` or its like would be refused despite working perfectly
once adopted. Vetting has to model the real thing rather than an idealised
one. `vet_binary`'s own comment carries that reasoning.

The probe also carries `SHEP_DOG_NAME` now, for the same reason it carries
`SHEP_HOME`: see the dog-name entry below.

The original entry follows.

Found 2026-08-20, the hard way, while building `shep-log-rotate`. It came
within a `max_size` default of rotating the live `~/.shep` that supervises
`zeus-auth`.

`vet` proves a kernel can exec the candidate by actually execing it
(`crates/shep-cli/src/commands/dogs.rs:384`):

```rust
match Command::new(&canonical).spawn() {
```

No `.env()`, no `.env_clear()`, no `.stdout()`/`.stderr()`. **The child
inherits the operator's entire environment and their terminal.** On macOS
`macos_deferred_exec_failure` then waits a short window before the kill, so
the candidate gets real running time, not an instant teardown.

**Executing is not the bug.** The doc's argument for it is sound and
`docs/dogs.md` is honest that an adopted dog runs at the shepherd's trust
level. The bug is narrower and worse: **`--home` does not reach this child.**
`shep adopt --home /tmp/scratch ./my-dog` vets `my-dog` with `SHEP_HOME`
inherited from the ambient environment, which is usually unset, so the
candidate resolves the default `~/.shep`. A dog reads `SHEP_HOME` to find its
socket, which is the one thing `docs/dogs.md` promises it. So the operator
names one home on the command line and shep runs the candidate against a
different one.

For a rotator that means: connect to the live daemon, `ListFlock` the real
flock, and rotate real logs, during a command whose entire purpose was to
decide whether to trust this binary at all. Measured: nothing was lost only
because `max_size` defaults to 10M and `zeus-auth-0-out.log` was 200 KB. That
is a coincidence, not a guard.

Three fixes, cheap, and they compose:

1. **Pass the resolved `--home` to the child.** One `.env("SHEP_HOME", ...)`.
   Smallest fix and it closes the reported case.
2. **`env_clear()`, then set only what a dog is promised.** `docs/dogs.md`
   already says `SHEP_HOME` is the one variable a dog inherits, so vetting
   with the operator's full environment contradicts the documented contract
   and hands an unvetted binary whatever tokens are in that shell.
3. **Give the child null stdio.** A candidate can currently scribble on the
   operator's terminal mid-vet, and a hostile one can imitate shep's own
   output at the exact moment the operator is deciding whether to trust it.

Deferred only because it is the maintainer's call how far to take it. (1) alone is a
two-line change and fixes the case that was actually hit.

### `emit_error`'s table arm prints whatever it is handed, unsanitised -- FIXED, 2026-08-25

**Fixed in `f34d88b`, by (1) below**, and wider than (1) as written.
`emit_error` runs its message through `terminal_safe::sanitise` before
either arm sees it, so the class is closed at the one place every caller
passes through rather than in each error type.

Two things the entry did not anticipate, both in the fix:

- **`emit_notice` gets the same treatment.** It is a sibling emitter with
  its own envelope, and leaving it out would have left the hole open on
  every `bleats` notice.
- **The JSON arm is sanitised too, not only the table arm.** `serde_json`
  escapes a control byte to `\u001b`, so a terminal never renders it
  directly, but `shep ... --format json | jq -r .error.message` unescapes
  it straight back onto a terminal.

`code` is deliberately left alone: every one is a `&'static str` from
`ExitCode::code_str` or a literal at the call site, so none is ever
attacker-supplied.

**(2), the `TerminalSafe` newtype, was not built.** It pushes the
obligation to where the string is built, which is where it belongs, and it
is still the better answer if shep grows much more error text off the wire.
`shep install` remains the case that would force it.

The original entry follows.

Found 2026-08-23 by the adversarial review of `shep dogs --available`, and
recorded because the instance was fixed while the class was not.

`emit_error`'s `Format::Table` arm is a bare `writeln!`. Nothing between an
error's `Display` and the operator's terminal removes control characters. That
is correct for every caller shep has today, because every one of them builds
its message from shep's own strings.

It stopped being obviously correct the moment shep grew a caller whose error
text comes off the wire. `FetchError::Redirect` carried a hostile `Location`
header straight through it and cleared the screen; that is fixed at the
capture seam, and a test now pins that no `FetchError` variant's `Display` can
carry a control character. **But the next such caller gets no warning.** The
guarantee lives in each error type rather than at the point of printing, so
adding an error that interpolates untrusted text reintroduces the hole
silently.

Two ways to close it, and the choice is the maintainer's:

1. **Sanitise inside `emit_error`'s table arm.** One place, closes the class
   for good. Costs a pass over every error message shep prints, and would
   strip any deliberate escape a future error wanted, which today is none.
2. **Make it unrepresentable**: a `TerminalSafe` newtype that `emit_error`
   requires, so an error carrying raw wire text will not compile. More work,
   and it pushes the obligation to where the string is built, which is where
   it belongs.

Deferred rather than picked, because the live hole is closed and the right
answer depends on whether shep expects more error text to come off the wire.
`shep install`, if it is ever built, would be exactly that.

### A dog cannot learn the name it was adopted under, and getting it wrong is silent -- FIXED, 2026-08-27

**Fixed by option 1 below**, the environment variable: every way shep runs a
dog now sets `SHEP_DOG_NAME` to the name it was registered under, beside the
`SHEP_HOME` it already set. Three seams, so a dog is never run under a
contract it will not meet again: `shep_daemon::dogs::dog_app` (supervised),
`run_adopted_dog` (`shep <name>`), and `vet_binary`'s exec probe during
`adopt` itself. The last of the three carries no test of its own, and its
comment says why rather than leaving the gap to be found: the probe child is
killed on sight, immediately on every kernel but macOS, so anything it wrote
to prove what it received would race its own teardown.

Options 2 and 3 were not taken and did not need to be. Option 2 (letting
`DogConfig` tell an absent section from an unadopted name) is a wire change,
and handing the dog the key removes the guess that made the ambiguity
reachable. Option 3 (documenting the pid trick) is in `docs/dogs.md`
anyway, now as the fallback for an older shep rather than as the answer.

The original entry follows.

An adopted dog is spawned with **no argv at all** and **one** environment
entry. `dogs.rs`'s `dog_app` maps `DogSource::Adopted { path }` to
`(path.clone(), Vec::new())`, then inserts `SHEP_HOME` and nothing else. The
comment there explains the argv decision and the reasoning is sound: "an argv
shep invented for it is one more thing it has to agree with before it can
start."

**The name is the one thing a dog needs and cannot be given.** It is the
`[dog.<name>]` key the dog's own configuration lives under, and the dog is
what sends `Request::DogConfig { name }`. So the dog has to guess, and the
guess has to match whatever the operator typed at `shep adopt`.

**The failure mode is silence.** `dog_section` returns `Ok(String::new())` for
a name with no section, which is deliberate and right on its own terms: a dog
with no configuration is the ordinary case, not a fault. But it is
indistinguishable from a name nobody adopted. Adopt a binary as `logrotate`
when its author hardcoded `log-rotate`, and every setting in the operator's
`shep.toml` is discarded, every default is used instead, and neither side
prints anything. It looks exactly like working.

**There is a workaround, and it should not have to be one.** A dog knows its
own pid, and `ListFlock` reports a pid per entry, so the entry that is a dog
and carries that pid is the dog itself, and its `name` is the key.
`shep-log-rotate` does this. It works, and every dog author would have to
reinvent it, having first discovered the problem the hard way.

Three ways out, none of them large, and the choice is the maintainer's:

1. **Pass the name after all**, as one argument or as `SHEP_DOG_NAME`. It
   contradicts `dog_app`'s comment, but an environment variable is not an argv
   and a dog that ignores an unknown variable still starts.
2. **Let `DogConfig` distinguish the two cases** — a section that is absent
   from a dog that is registered, versus a name that was never adopted. The
   second is a genuine operator error and could be refused rather than
   answered with the empty string.
3. **Document the pid trick in `docs/dogs.md`** and leave the contract alone.
   Cheapest, and it leaves every dog author to write the same twelve lines.

Deferred rather than picked here because the fix is a wire or contract
decision, and this project's job was to find it rather than to make that call.

### The license files are not inside the published tarballs -- FIXED, Phase 17

`cargo package` does not include `LICENSE-MIT` or `LICENSE-APACHE`, so the
crates ship with a `license = "MIT OR Apache-2.0"` field and no license text.
Tooling reads the field, so nothing is broken, and `docs/releasing.md` already
records it as cosmetic.

**Fixed in Phase 17**, before the first publish rather than after, so no
version ships without the text. Each crate directory carries a symlink to the
workspace-root `LICENSE-MIT` and `LICENSE-APACHE`: cargo packages only files
under a crate's own directory, so an `include` cannot reach the workspace
root, and cargo dereferences the symlinks when packaging. Verified by
extracting a built `.crate` and comparing bytes against the source -- 1060 for
`LICENSE-MIT`, identical.

The note about permanence still stands and is why this was worth doing now:
tarball contents cannot be corrected for a version after it is published.

## Not deferred

**The Windows tier** (spec §11) **shipped**, 2026-08-26, and this entry
replaces the scope cut that stood in the section above until that day. This
  entry described a 0% tier and a decision to keep it that way; both are
  superseded. See [windows-estimate.md](windows-estimate.md), whose own
  "run the CI leg first" recommendation is what unblocked it — the tree was
  already compile-green on native MSVC, and a Windows host was available.

  **Tier A is implemented and verified against a live flock**: `start`,
  `stop`, `restart`, `reload`, `flock`, `describe`, `bleats`, `delete`,
  `save`/`muster`, `lookout`, `whistle` and the dogs, over a named pipe,
  with each sheep in a job object. The three predictions the estimate made
  about *shape* all held; the ones about cost were pessimistic, because the
  145 `cfg(unix)` sites really were the cheap part — only ten files in
  `shep-cli` contained a Unix API call at all.

  What the estimate got RIGHT and is now settled:

  - **Graceful stop has no analogue.** `TokioProc::signal`'s Windows arm
    refuses rather than pretending, so `shep stop` waits the full
    `kill_timeout` and terminates unless the app opted into the shepherd
    channel. Measured live: 1625ms against a 1600ms `kill_timeout`.
  - **The shepherd channel cannot be fd 3.** It is a named pipe named by
    `$SHEP_CHANNEL_PIPE`; the wire format is untouched, so `shep trigger`'s
    correlation id survives. `SHEP_CHANNEL_FD` is deliberately NOT set on
    Windows so an app can branch on which variable is present.
  - **`user`/`group` refuse permanently.** `privilege::resolve`'s non-unix
    arm already did this; the runner `debug_assert`s it never sees
    credentials.

  What the estimate got WRONG, corrected here:

  - **The `forbid(unsafe_code)` blocker was already known not to be real**
    (the estimate says so itself) and `barks.rs`'s no-op lock is now a real
    `share_mode(0)` lock with the two-process race test running on Windows.
  - **`first_pipe_instance` deletes the stale-socket problem rather than
    solving it.** A pipe has no directory entry, so there is nothing to
    recover from and the whole probe-and-recover arm is unix-only. It also
    gives the daemon a second, OS-enforced mutual exclusion that unix
    cannot have.
  - **The peer-uid check needed no FFI replacement.** The pipe's own ACL
    denies a foreign local user the open-for-write that speaking the
    protocol requires, and does it at `CreateFile` time — earlier than any
    post-accept check. `shep_core::transport`'s module doc is the writeup.

  **The published `shep-client` blocked every Windows dog author, and that
  is the sharpest argument for the transport seam.** Measured 2026-08-26 by
  `cargo install shep-log-rotate` on Windows, which does not fail gracefully
  — it does not COMPILE:

  ```
  error[E0432]: unresolved import `shep_client::Client`
  note: found an item that was configured out
    --> shep-client-0.1.0/src/lib.rs:45:5
  43 | #[cfg(unix)]
  ```

  `shep-client` is the published embedding API, and gating `Client` behind
  `cfg(unix)` meant no third-party dog — and no downstream embedder at all —
  could build against it on that platform. Un-gating it is therefore not
  only about shep's own binary.

  Verified end to end against the real crates.io dog: `shep-log-rotate
  0.1.1`, source unmodified, builds against the ported `shep-client`, and
  then `shep adopt` vets it, registers it, starts it, and supervises it —
  `online`, 0 restarts, empty stderr, talking to the shepherd over the named
  pipe for its `[dog.log-rotate]` config. The dog ecosystem works on
  Windows.

  It follows that **`cargo install shep-log-rotate` stays broken on Windows
  until a shep-client carrying this port is published.** Nothing in this
  branch can fix that for an operator; it needs a release.

  **Still refused on Windows**, and stated here rather than implied:
  `shep startup`/`unstartup` (Tier B — an SCM service, not a fifth unit
  template), `user`/`group`, seven of nine `shep signal` names, and the
  `$SHEP_HOME` ACL, which inherits its parent rather than being `0700`.

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
A `.js` module that keeps node alive hung `shep start` forever until the 30s
`JS_EVAL_BUDGET` landed on 2026-08-28; see the entry above.

**schemars JSON-schema export** (spec §5) **shipped**, Phase 14, behind a
non-default `schema` feature on shep-core that shep turns on. The schema
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
should be trusted with, not a technical limit, and it is the maintainer's to overrule.

**`shep serve`, `shep dev` and `shep runtime`** (spec §9, §13) **shipped**,
Phase 15, closing the last three v1.0 verbs and the `[[bin]]` gap this file
used to name:

- **`serve` is hand-rolled, not axum/tower-http** (the maintainer's ruling, 2026-08-15).
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
  spec §4 promises are exact; the init instead relies on the kernel already
  treating real PID 1 as the implicit subreaper — it never calls
  `set_child_subreaper` — forwards SIGTERM/SIGINT/SIGHUP/SIGQUIT to the
  supervisor it spawns, and reaps every orphan itself. That reaping only
  happens when the init genuinely is PID 1: `$SHEP_FORCE_INIT`, a test-only
  override that drives the same split off PID 1, gets identical signal
  forwarding but not the reaping, since the kernel then reparents orphans
  to whatever real subreaper it finds up the ancestor chain, not to this
  process.
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

### Rendering each dog's README as its own docs-site page — rejected

The maintainer proposed this, 2026-08-26, then agreed to drop it once the reasoning
below was laid out. Recorded so the argument does not have to be re-derived
the next time it sounds like a nice idea.

**The blocker was never XSS.** A hostile README running script on shep's own
domain is a real risk, but it is a *solved* one in the general sense: render
only a restricted Markdown subset, or run the HTML through an established
sanitiser that strips `<script>`, `javascript:` hrefs, and `on*` attributes.

That is not the same problem `dog_index.rs` already solves, and citing it as
precedent here would be wrong. `terminal_safe::sanitise` strips control
characters and invisible formatting characters for a *terminal* — it does
nothing about a `<script>` tag, a `javascript:` href, or an `onerror`
attribute, none of which are control characters. Terminal sanitisation and
HTML sanitisation are different problems, and the first buys nothing for the
second. The HTML work is still known and would not be the hard part — it is
just work this repo has not written, not code already sitting in
`dog_index.rs` waiting to be reused.

**The actual blocker is curation, and no sanitiser touches that.** An index
entry is reviewed exactly once, in a pull request, by a human who read the
bytes being merged. A README is reviewed never: it lives in the dog's own
repository, and it can change the moment after that pull request lands, to
anything its author wants, with nobody at shep looking again. Shipping this
would turn one reviewed `dogs.json` entry into a standing licence for
whoever controls that repository to publish arbitrary content on shep's own
domain, under shep's own styling, forever and unreviewed.

**A disclaimer does not fix it, because the failure mode is trust, not
confusion.** Nobody who lands on `shep-pm.com/dogs/whatever` reads
it as a stranger's unmoderated page; they read it as shep's docs, reviewed
the way shep's docs are reviewed. That trust is the entire reason the feature
would be worth building, and it is exactly what turns a compromised or
malicious README dangerous later: the page would be believed *because* it is
on shep's site, not despite it. A footnote saying "we did not review this"
does not change what a reader actually does once they are on the page.

**The version that would be defensible, if this ever comes back:** fetch
each README at build time rather than at request time, and pin what was
fetched — a Flockfile-schema-style lockfile, not a live proxy — so any
change upstream surfaces as a diff against this repo's own pull request
history, reviewed exactly like a `dogs.json` entry already is, instead of
taking effect on shep's site the moment somebody else's repository changes.
And strip HTML outright rather than trying to sanitise it: a stripped-down
renderer has no tag left to smuggle an attack through, where a sanitiser
only ever has to lose once. Fetching at request time gives up the curation
property completely no matter how good the sanitiser is, because the content
a reviewer approved and the content actually being served are free to
diverge starting with the very next upstream commit.

### A config edit reaches nothing, and the warning about it is wrong -- FIXED, 2026-09-03

Found 2026-08-30, from the maintainer's question: an app running four
instances, `instances = 5` edited into the Flockfile, and no way to get the
fifth without restarting the other four.

For `instances` alone there is a way. `shep stock web 5` fills the lowest free
slot and leaves 0 through 3 running, writing the new count onto the stored
spec and into the muster roll. For every other field there is nothing.
`handle_reload` and the restart path both say so in as many words --
*"Nothing here re-reads configuration."* The only route is `shep delete`
followed by `shep start`, which restarts every instance.

`Request::ConfigDrift` closed half of this: an edit that will not apply is
reported rather than vanishing without a word. Applying it was left open
deliberately, in the code -- *"Whether `start` should reconcile by default, or
grow an `--update` flag, is the maintainer's call and neither is taken here."*

**The fields split three ways, and the first group is larger than "a config
change needs a restart" suggests.**

- **Read at decision time, so nothing need be restarted.** `autorestart`,
  `max_restarts`, `min_uptime`, `restart_delay`, `exp_backoff_restart_delay`
  and `stop_exit_codes` are read by `brain::decide` when a sheep exits;
  `kill_signal`, `kill_timeout` and `graceful_timeout` when a kill ladder
  runs; `max_memory`, `cron_restart`, `cron_timezone`, `watch` and the
  liveness probe when `extras` arms a worker, which it already does through
  `arm_instance`/`disarm_instance`. A write-back takes effect at the next such
  decision with no disruption at all.
- **Consumed at spawn, so they reach the next process rather than the running
  one.** `listen_timeout` and `readiness_probe`.
- **Baked into the child, so one instance swap each.** `script`, `args`,
  `cwd`, `interpreter`, `env`, `user`, `group`, `out_file`, `err_file`,
  `merge_logs`, `channel`, `stdin`, `wait_ready`.

**`shep stock` already proves every mechanism a wider verb needs**: normalize
before write, write-back onto the stored spec, partial-failure handling, and
muster-roll persistence. `AppConfig::drifted_fields` already computes which
fields moved. What is missing is the routing between the three groups.

**One part of this is a bug rather than a gap.** The drift warning tells the
operator that "`shep start` adds instances to a sheep the flock already has".
True of the daemon's `Request::Start`, false of the `shep start` an operator
types: the CLI sorts apps into resumed and fresh, and only fresh ones reach
that request. The sentence describes something no terminal can produce.

**What shipped.** `Request::ApplyConfig`, answered by `Response::Applied`, sent
by `shep start <Flockfile>` and by a Flockfile discovered in the working
directory. It merges each declared app into the sheep of the same name:
the MERGE registers nothing, prunes nothing and kills nothing running. That
is a statement about the apply phase and not about the verb around it: the
`shep start` carrying the load still registers and starts an app the flock
does not have, on its own fresh path, which is the whole of what `shep start`
has always done. A
field the daemon reads fresh takes effect immediately, and a field the running
child holds parks for the sheep's next spawn, where `shep reload` and `shep
restart` promote it. A `CFG` column in `shep flock` and a pending section in
`shep describe` are where an operator sees the parked half. `web/src/pages/docs/overrides.astro`
is the operator-facing account.

**Be exact about the complaint that opened this entry.** An `instances = 5`
edit reaching nothing is fixed by `shep start <Flockfile> --reset`, NOT by the
default. A plain load is additive and holds `instances` out of scope however
unestablished it is, because appending a count is not a value change: a
scale-down through a load would DELETE instances, and the store cannot yet
tell `shep stock`'s deliberate count apart from a count nobody has touched. So
the answer to the original question is a flag, and `shep stock web 5` is still
the shortest route to that one field.

**Three corrections to the field split above**, all measured against the read
sites rather than inferred from the field names, and all now carried in
`crates/shep-core/src/config/apply.rs`:

- `kill_signal` is NOT read at decision time, despite sitting beside
  `kill_timeout` and `graceful_timeout`, which are. It is read inside
  `kill_process` from the `app` parameter of the long-lived per-sheep task,
  whose `ResolvedApp` is moved in once at `spawn_sheep_task` and never
  refreshed. So an edit reaches the next spawn, not the next kill.
- `shutdown_with_message` belongs with the baked-in group, not with the kill
  ladder: `assemble` ORs it into whether fd 3 is opened, and that is the
  child's own fd table.
- The eight fields a lifecycle extra reads when it is ARMED (`max_memory`,
  `watch`, `ignore_watch`, `watch_delay`, `watch_options`, `cron_restart`,
  `cron_timezone`, `liveness_probe`) are read at decision time in the sense
  above, but a write to the stored spec is not enough on its own: the worker
  already armed against the old value keeps it for as long as it lives, and
  `ExtrasRegistry::arm` PRESERVES a live name-group task by design. They need
  a force-replacing re-arm, which is why `rearm_name` exists.

**The wrong warning is gone rather than reworded.** The drift warning was
deleted outright once the load path could apply the edit it was warning about.

