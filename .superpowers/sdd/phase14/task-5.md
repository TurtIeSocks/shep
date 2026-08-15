# Task 5 — the daemon flags layer, wired

## Status: DONE, green

## Commit

`25f03cd` on `feat/phase14-config-packaging` — "feat(cli): the daemon flags
layer, wired into run_daemon"

## What landed

- `crates/shep-cli/src/cli.rs`: `DaemonArgs` gains `log_json: Option<bool>`
  (`--log-json`, three states via `num_args = 0..=1` +
  `default_missing_value = "true"`), `log_level: Option<LogLevel>`
  (`--log-level`), `socket: Option<PathBuf>` (`--socket`),
  `max_cron_sleep: Option<UpDuration>` (`--max-cron-sleep`); plus the three
  clap value parsers (`bool_flag` over `shep_core::config::parse_daemon_bool`,
  `log_level_flag` over `LogLevel::from_name`, `duration_flag` over
  `UpDuration::FromStr`) and two parse tests (`log_json_has_three_states`,
  `the_flag_bool_grammar_matches_the_env_grammar`).
- `crates/shep-cli/src/commands/daemon.rs`: new `daemon_overrides(&DaemonArgs)
  -> DaemonOverrides`, `run_daemon` now calls `DaemonConfig::load_layered`
  with it instead of `load`, doc updated to say the environment is the middle
  layer now. New test `every_daemon_flag_reaches_the_config`. All 10 existing
  `DaemonArgs { .. }` struct literals in this file's tests gained the four
  new fields (all `None`) — the compile error from the struct growing is what
  proved every site was found.

## Test numbers

`cargo test -p shep-cli --bins --all-features`: 416 -> 419 passed, 0 failed
(the 3 named above). Full task gate, one command at a time, `$?` read
directly, `CARGO_TARGET_DIR` pointed at this worktree's own scratch dir:

- `cargo fmt --all --check`: EXIT=0
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  EXIT=0
- `cargo test --workspace --all-features`: EXIT=0, 17 result lines, `0 failed`
  on every one (419/47/1/6/4/1/8/7/246/1/500/18/1/13/2/4/3 passed)
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`:
  EXIT=0

Cargo command shape used throughout: workspace shape
(`cargo test --workspace --all-features` / `cargo clippy --workspace
--all-targets --all-features`), per the worktree brief — this task crosses
shep-core (via `DaemonConfig::load_layered`, `DaemonOverrides`,
`parse_daemon_bool`) and shep-cli.

## Baseline

Step 5.1's baseline matched exactly: `grep -c "pub no_restore\|pub foreground"
crates/shep-cli/src/cli.rs` = 2, `grep -c "load_layered"
crates/shep-cli/src/commands/daemon.rs` = 0 (before), `cargo test -p shep-cli
--bins --all-features` = 416 passed / 0 failed, matching the plan's stated
RED state.

## Mutation

Step 5.5: swapped `.log_level(args.log_level)` for `.log_level(None)` in
`daemon_overrides`. Result: exactly one test failed —
`commands::daemon::tests::every_daemon_flag_reaches_the_config`, on its
`log_level` assertion (`left: Error, right: Trace` — the config fell back to
the file's `error`), 418 passed / 1 failed, nothing else touched. Reverted;
diffed the reverted file against a pre-mutation copy to confirm byte-for-byte
identity before re-running the gate.

## Concern (not blocking, worth flagging)

Step 5.4's verify grep says `grep -c "load_layered"
crates/shep-cli/src/commands/daemon.rs   # 0 -> 1`, but step 5.3's own test
snippet (`every_daemon_flag_reaches_the_config`) itself calls
`DaemonConfig::load_layered(...)` a second time. The actual count after the
task, correctly, is 2 (the `run_daemon` call site plus the test), not 1. The
plan's own two steps are mutually inconsistent on this one grep; I trusted the
step 5.3 test text (which matches what a real regression-catching test needs)
over the step 5.4 count and did not shave a call out of the test to force the
grep to 1. Said here per the "if a baseline doesn't match, report it" rule —
this is a plan-internal inconsistency, not a code defect, and nothing else in
the task was affected by it.

## Two house rules re-confirmed while writing this

- `DaemonConfig` was not touched to look like a proof token anywhere in this
  task — `daemon_overrides` builds a `DaemonOverrides` (itself not a proof
  token either, same non-exhaustive-for-field-growth reasoning as decision 8)
  and hands it straight to `load_layered`, which validates once at the end.
- Nothing in this task touches openrc/BSD rendering; not applicable here.

## A real bug I introduced and caught before committing

My first `Edit` inserted `daemon_overrides` and its doc comment directly
between `boot_options`'s trailing doc-comment line and its `#[must_use]`
attribute, with no blank line separating the two doc blocks. Rust attaches
contiguous `///` lines to whichever item follows the last one, so this
silently reattached the entirety of `boot_options`'s eleven-paragraph doc
comment onto `daemon_overrides` and left `boot_options` completely
undocumented. `cargo doc` did not catch it (both functions are `pub`, both
ended up documented, just under the wrong name) and clippy did not either.
Caught by re-reading the diff before committing, not by any automated gate —
worth naming since neither fmt, clippy, test, nor rustdoc's `-D warnings`
would have caught a doc comment quietly migrating to the wrong item. Fixed by
moving `daemon_overrides` (with its own doc block) above `boot_options`'s doc
block entirely, confirmed by re-reading the file and re-running the full gate
(fmt/clippy/test/doc all green a second time, same 419-passing shep-cli
count).
