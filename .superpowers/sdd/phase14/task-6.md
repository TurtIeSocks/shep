# Task 6 — runtime init detection and `--init`

**Status: DONE, green.**

**Commit:** `6c90adf` — `feat(cli): runtime init detection and --init`

## What landed

- `Init` moved from `commands::startup::unit` into `crates/shep-cli/src/cli.rs`
  (beside `Format`), gained `Openrc`, `FreebsdRc`, `OpenbsdRc` alongside the
  existing `Systemd`/`Launchd`, and both `#[cfg_attr(..., allow(dead_code))]`
  attributes on the old two-variant enum are gone.
- `StartupArgs` gained `pub init: Option<Init>` (`--init`, shared by `startup`
  and `unstartup`).
- `commands::startup::mod.rs`:
  - `linux_init(systemd: bool, openrc: bool) -> Option<Init>` — pure, `const
    fn`, tested; systemd wins a tie.
  - `current_init()` — lost `const`; Linux arm now does two real filesystem
    reads (`/run/systemd/system`, `/run/openrc/softlevel` or
    `/run/openrc`) and feeds `linux_init`; macOS/FreeBSD/OpenBSD stay
    compile-time facts; everything else is `None`.
  - `unit_mode(Init) -> u32` and `unit_path_for(Init, &str) -> PathBuf`
    replace the old `UNIT_MODE` constant and the inline two-armed
    `match … { Systemd => …, Launchd => … }` in `plan()`.
  - `unbuilt_renderer(Init) -> Option<&'static str>` is the single gate;
    `plan()` calls it immediately after resolving `init` and refuses
    (`ExitCode::Usage`, `"shep cannot write a {name} unit yet"`) before a
    `StartupPlan` naming an unbuilt init can exist.
  - `write_unit`, `install`, `remove` each grew one shared, marked-unreachable
    arm for `Openrc | FreebsdRc | OpenbsdRc`, via a new `unbuilt_step` helper.
    Each is commented `TASK-7-8 REPLACES THIS ARM`.
  - The refusal for "no init detected and no `--init`" names both probed
    paths, per decision 10's exact text.
- 5 new tests, all named in the plan: `the_mode_is_read_only_for_units_and_executable_for_scripts`,
  `systemd_wins_when_both_linux_probes_are_true`, `an_explicit_init_beats_detection`,
  `each_init_names_its_own_unit_path`, `only_the_unbuilt_renderers_are_refused`.

## Baseline vs. plan — one real mismatch

Step 6.1's baseline table:

```
grep -c "UNIT_MODE" crates/shep-cli/src/commands/startup/mod.rs   # 4
```

Actual count at task start was **5**, not 4 — the fifth hit is the test
comment at (then) line 890, `"A literal, deliberately, not \`UNIT_MODE\`: …"`.
This isn't a silent adjustment: the plan's own step 6.4 narrative, three
paragraphs after the baseline table, explicitly anticipates this fifth hit
("A fifth hit is a comment in a test explaining why it uses a literal instead
… reread it — it may now want to name the function") — so the baseline table
and the task's own prose disagree with each other, not just with the tree.
I followed the prose: reworded that comment to name `unit_mode(Init::Systemd)`
instead of the now-deleted `UNIT_MODE` constant. Flagging per the "STOP and
report a baseline mismatch" instruction rather than silently treating either
side as authoritative.

Every other step 6.1 baseline (`allow(dead_code)` = 2, `target_os` = 3,
`const UNIT_MODE` = 1, the two doc-string greps = 1 each, the clap `--init`
error/exit-2 behavior) matched exactly.

## A gap the plan didn't spell out: clippy dead_code on `linux_init`

Not in the plan text. `linux_init` is called from `current_init`'s Linux arm
only; on macOS (and every non-Linux target) that arm is `#[cfg]`-ed away, so
in the plain (non-test) `bin` target `linux_init` has zero non-test callers.
`cargo clippy --workspace --all-targets --all-features -- -D warnings` failed
with `function 'linux_init' is never used` before I added
`#[cfg_attr(not(target_os = "linux"), allow(dead_code))]` on it — the same
idiom the old `Init` variants used before this task deleted it from them.
This is a new function, not one of the two the plan named, so it isn't a
plan defect exactly, but it's a step the plan's own verify list (`cargo
clippy`) would have caught and the step-by-step instructions didn't mention.
Fixed; clippy is clean.

## Mutations (step 6.7) — all three reddened the named test, none dead

1. Swapped `linux_init`'s two arms (openrc wins the tie) →
   `systemd_wins_when_both_linux_probes_are_true` failed on the first two
   assertions, exactly as predicted. Reverted.
2. `unit_mode(Init::Openrc)` changed to `0o644` →
   `the_mode_is_read_only_for_units_and_executable_for_scripts` failed.
   Reverted.
3. Removed `Init::Openrc` from `unbuilt_renderer` (folded into the `None`
   arm, no renderer added) → `only_the_unbuilt_renderers_are_refused` failed
   on `unbuilt_renderer(Init::Openrc).is_some()`. Reverted.

## Verification — cargo command shape

Per the phase brief: `cargo test --workspace --all-features` for anything
crossing crates, `cargo test -p shep-core --lib --all-features` for the
shep-core-only quick loop (not used this task — Task 6 never touches
shep-core). I used exactly one shape per invocation, `$?` never through a
pipe.

- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` —
  clean (after the `linux_init` fix above).
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`
  — clean. (`UNIT_MODE`'s old intra-doc link was rewritten to `[unit_mode]`;
  no broken-link warning.)
- `cargo test -p shep-cli --bins --all-features` — **424 passed, 0 failed, 3
  ignored** (baseline was 419; +5 named tests above).
- `cargo test --workspace --all-features` — full green on the run that
  matters, but two *different* tests each failed once across two runs and
  passed clean in isolation, both in `shep-daemon`, both untouched by this
  diff (`crates/shep-daemon/tests/daemon_e2e.rs`'s
  `a_reload_costs_a_defiant_app_the_work_it_will_not_finish`, and
  `crates/shep-daemon/tests/real_runner.rs`'s
  `a_bare_interpreter_resolves_via_the_seeded_path`). Confirmed both are load
  artifacts, not a radius from this change: re-ran each targeted suite alone
  and both passed. The project's own CLAUDE.md and this phase's plan both
  name this exact failure shape ("bounded waits on real children produce
  false radii under load").
- `cargo check --workspace --all-targets --all-features --target
  x86_64-pc-windows-gnu` — **clean, 11.3s** (only pre-existing shep-daemon
  `cfg(unix)` dead-code warnings, as documented; nothing from this task's
  diff). This is the check the whole `Init`-move decision exists for.
- `cargo check -p shep-daemon --all-targets --all-features --target
  x86_64-unknown-linux-gnu` — clean, 26.7s. Doesn't touch any code this task
  changed.
- `cargo check -p shep-cli --all-targets --all-features --target
  x86_64-unknown-linux-gnu` (informational, not a gate command) — fails
  exactly as the plan predicted: `ring`'s build script can't find
  `x86_64-linux-gnu-gcc` on this machine. Known toolchain gap, not a defect;
  `current_init`'s `cfg(target_os = "linux")` arm (the two `Path` reads) is
  compiled by nothing on this machine, same as the plan says. `linux_init`
  itself — where the actual ordering logic lives — is compiled and tested
  everywhere including here.

All six step-6.6 grep checks confirmed at their target values after the
final edit:
`allow(dead_code)` in `unit.rs` → 0,
`const UNIT_MODE` in `mod.rs` → 0,
`unit_mode(` in `mod.rs` → 8 (non-zero, as required),
both `"openrc/rc.d are named as deferred"` / `"openrc and the BSD rc.d
scripts"` → 0,
`"TASK-7-8 REPLACES THIS ARM"` → **3** (see note below).

One self-inflicted near-miss on the last of those: my first draft of the
`unbuilt_step` doc comment quoted the literal marker string
`` `TASK-7-8 REPLACES THIS ARM` `` in prose, which pushed the grep count to 4
and would have left an extra hit for Task 8 to puzzle over. Reworded before
committing; final count is 3, matching the plan exactly.

## Concerns for whoever reads this next

- The `UNIT_MODE` baseline mismatch above (4 stated vs. 5 actual) is worth a
  one-line note if this plan is reused as a template — the step 6.1 table and
  the step 6.4 prose disagree with each other, not just with the tree.
- The `linux_init` clippy dead-code fix is a small addition the plan didn't
  ask for by name; it's the same narrowing idiom the plan explicitly deleted
  from the old `Init` enum for a different reason (constructibility, not
  dead-code), so it isn't a contradiction, just an omission.
- No renderer for `Openrc`/`FreebsdRc`/`OpenbsdRc` exists yet by design —
  `--init` for any of the three refuses with `ExitCode::Usage` and
  `"shep cannot write a {name} unit yet"`. That's Tasks 7 and 8, not this one.
