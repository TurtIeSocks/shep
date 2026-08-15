# Task 8 — FreeBSD and OpenBSD `rc.d` renderers

## Status: done, green

## Baseline mismatch (STOP-and-report, per instructions)

Step 8.1's two greps did not match reality:

- `grep -rn "rc.subr" crates | wc -l` — plan said baseline `0`, actual was
  `2`. Cause: Task 6 (already merged, independent of Task 8) added the `Init`
  enum with doc comments on `FreebsdRc`/`OpenbsdRc` that mention
  `` `/etc/rc.subr` `` and `` `/etc/rc.d/rc.subr` `` in `cli.rs`. This is
  genuine baseline drift from an earlier, independent task landing text that
  happens to contain the phrase — not a dead check, just a stale number in
  the plan. After Task 8's renderers, the count is `17`.
- `grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src | wc -l` —
  plan said baseline `2`, actual was `0`, because the real marker text Task 7
  left in the codebase is `TASK-8 REPLACES THIS ARM` (no `-7`), three
  occurrences, not two. Verified with `grep -n "TASK-8 REPLACES THIS ARM"
  crates/shep-cli/src` before doing anything else. Used the real anchor for
  the rest of the task; ended at `0` as intended.

Neither mismatch blocked the task — both were explainable from reading the
actual tree — but flagging per the "stop and report" instruction.

## What shipped

**Step 8.2 — the username refusal (TDD'd for real).** Added
`is_rc_safe_user` in `mod.rs` and its test
`a_user_name_that_cannot_be_a_shell_variable_is_refused`. Confirmed RED first:
wrote the test alone, ran `cargo test -p shep-cli --bins --all-features`, got
`E0425: cannot find function is_rc_safe_user` — failed for the stated reason.
Then added the function and the refusal in `plan()` (after `target_user`,
before `passwd_home`), confirmed GREEN. Promoted `shell_quote` (previously
private in `mod.rs`) to `pub(crate)` so the two BSD renderers can reuse it as
their single-quote former, per decision 11's composition rule.

**Steps 8.3/8.4 — the two renderers**, `unit::freebsd_rc_script` and
`unit::openbsd_rc_script`, added to `unit.rs`. Both follow decision 11's
quoting rule exactly: identifiers (`name=`, `rcvar=`, `shep_<user>_*`
fragments, `daemon_user=`'s value) take the raw, already-validated `spec.user`
since they are shell *identifiers*, not values; every other interpolated
value goes through `sh_double_quoted`; the two values that land inside a
string a nested shell re-evaluates (FreeBSD's `${name}_env`, OpenBSD's
`rc_exec` argument) go through `sh_double_quoted(shell_quote(value))`.

**Verified the framework vocabulary against primary sources before writing
either renderer, per the plan's explicit instruction — not from memory:**

- FreeBSD: fetched `man.freebsd.org` for `rc.subr(8)`. `start_postcmd` is
  real (`argument_postcmd` — "shell commands to run if... the default method
  for argument returned a zero exit code"). `${name}_env` is real, documented
  as "a list of environment variables... passed as arguments to env(1)" —
  word-split, not `eval`, but the practical effect the plan worried about
  (an unescaped space becomes two entries) is confirmed real either way.
  Both match the plan's draft; no correction needed there.
- OpenBSD: fetched `man.openbsd.org/rc.subr.8` and the actual OpenBSD source
  (`github.com/openbsd/src/blob/master/etc/rc.d/rc.subr`), not just the man
  page. **`rc_post` runs after stop only; there is no post-start hook** —
  confirmed exactly as the plan claimed, so the "no post-start hook" comment
  ships unchanged, per the honesty rule.
  **One real correction found and made:** the plan's draft used `${rcexec}`
  and hedged with "VERIFY against rc.subr(8): if su there preserves the
  environment, two export lines are simpler." A first web-search summary
  (of a third-party HashiCorp example) suggested `${rcexec}` was right, but
  the actual OpenBSD source shows `rc_exec` is a function (`rc_exec()`
  defined with an internal, differently-named local `_rcexec`), called
  directly as `rc_exec "..."` — so the shipped script calls `rc_exec`, not
  `${rcexec}`. The source also settles the hedge outright: `rc_exec` invokes
  `su -fl -c <class> -s /bin/sh <user> -c "..."`, and `su(1)`'s `-l` flag
  **does** discard the caller's environment (confirmed against `su.1`
  separately). So embedding `SHEP_HOME=... PATH=...` as literal prefixes
  inside the string handed to `rc_exec` is not a hedge against uncertainty —
  it is the only mechanism that reaches the daemon at all, since anything
  `export`ed above `rc_start` is discarded by `su -l` before the daemon ever
  runs. The shipped doc comment says this plainly and confidently, with no
  "VERIFY" hedge left in it, and `rc_cmd $1` (unquoted) is kept as drafted —
  confirmed against a real production example (OpenBSD's own `vault`/`httpd`
  rc.d scripts) as the idiomatic form.

**Step 8.4 continued — removed the unbuilt-renderer scaffolding.**
`unbuilt_renderer` now returns `None` for all five `Init` variants (dead by
construction), so per the plan it and its test were deleted outright, along
with the now-orphaned `unbuilt_step` helper and the `if let Some(name) =
unbuilt_renderer(init)` refusal gate in `plan()` (the plan didn't spell this
last deletion out explicitly, but it's the direct, necessary consequence of
deleting the function it called). The three `TASK-8 REPLACES THIS ARM` arms
in `install`, `remove`, and `write_unit` were replaced with real behavior:
`write_unit` now calls the two new renderers; `install`/`remove` run
`sysrc`/`service` for FreeBSD and `rcctl` for OpenBSD, matching the
Install/Remove lines the plan's own renderer docs specified, built from
`unit_file_name(plan)` (the actual generated `shep_<user>` basename) rather
than reformatting the user separately.

**Step 8.5 — tests.** All seven named tests, plus the username-refusal test
from 8.2 — eight new tests, one deleted (`only_the_unbuilt_renderers_are_refused`),
net **+7**, matching the plan's reconciled delta exactly.
`the_openbsd_script_matches_the_spec_exactly` needed one iteration: my first
hand-written expected string over-quoted `SHEP_HOME`/`PATH` — `shell_quote`
correctly leaves an already-shell-safe path unquoted (it only wraps a value
that needs it), so the default `spec()`'s plain paths render unquoted; fixed
the expected string to match, rather than the code.

## Mutations (Step 8.7) — all three behaved exactly as predicted

1. `name="shep_<user>"` → `name="shep"` (leaving `rcvar` alone): reddened
   exactly `the_freebsd_rcvar_matches_the_script_name`, nothing else.
2. Deleted the "no post-start hook" paragraph from the OpenBSD script:
   reddened exactly `the_openbsd_script_admits_it_has_no_readiness_gate` and
   `the_openbsd_script_matches_the_spec_exactly`, nothing else.
3. Dropped the inner `shell_quote` from FreeBSD's `${name}_env` line
   (kept `sh_double_quoted` alone): reddened exactly
   `a_path_with_a_space_stays_one_freebsd_env_entry`; confirmed
   `the_freebsd_script_quotes_shell_metacharacters` still passed — the two
   escapers guard different things, as decision 11 says.

All three reverted; working tree confirmed byte-identical to before each
mutation (`diff` empty) before moving to the next.

## Verification run

Single cargo command shape for the whole task: `-p shep-cli`, since Task 8's
diff touches only `crates/shep-cli/src/commands/startup/{mod,unit}.rs`. Noted
per the worktree's per-task-shape instruction.

- `cargo test -p shep-cli --bins --all-features`: **436 passed, 0 failed, 3
  ignored**. Net delta is +7 by test-name enumeration (8 new tests listed
  above, 1 deleted), matching the plan's reconciled step-8.6 expectation; did
  not separately capture a raw pre-task total to diff against.
- `cargo test -p shep-cli --all-features` (full, including the `tests/`
  integration suites and `cli_e2e`/`daemon_e2e`-style binaries in this
  crate): **436 + 47 + 1 = 484 passed, 0 failed** across three result lines.
- `cargo fmt --all --check`: one diff (rustfmt reflowing the new
  `is_rc_safe_user` body), applied with `cargo fmt --all`; re-checked clean.
- `cargo clippy -p shep-cli --all-targets --all-features -- -D warnings`:
  clean.
- `RUSTDOCFLAGS="-D warnings" cargo doc -p shep-cli --no-deps --all-features`:
  **fails, but pre-existing and unrelated to Task 8** — one broken intra-doc
  link, `[`shep_client::testing`]` in `crates/shep-cli/src/commands/dogs.rs:124`,
  a file Task 8 never touches. Confirmed by `git stash`-ing this task's whole
  diff and re-running: the identical single error reproduces on a clean
  checkout of the branch tip. Not fixed — out of Task 8's scope (a different
  file, someone else's task), and this task's own new doc links (several
  intra-doc references between `mod.rs` and `unit.rs`) introduce zero new
  doc-link errors: the error set is byte-identical before and after this
  task's diff. Flagging loudly per the "quarantine, don't silently move on"
  rule rather than fixing a file outside this task's brief.
- `cargo test -p shep-cli --doc --all-features`: `error: no library targets
  found in package shep-cli` — expected, shep-cli is bin-only, no doctests
  exist to run.
- Did not run the Windows cross-check (`cargo check ... --target
  x86_64-pc-windows-gnu`): the repo's own CLAUDE.md scopes that to "once per
  phase, not per task," and Task 8 adds no `cfg`-gated code — the two new
  renderers are plain `format!` functions compiled on every target already.

## Two things this phase gets wrong if nobody says them (confirmed honored)

- No `DaemonConfig`-is-a-proof-token claim was written anywhere in this
  task's diff or docs — not applicable to Task 8's files anyway, but
  confirmed clean.
- No doc anywhere in this diff claims shep supports FreeBSD, OpenBSD, or
  openrc. Every doc comment on the two new renderers describes what was
  verified (rendering, and the manual/source facts checked against primary
  sources) and does not claim the scripts were run on their target OS —
  because they were not, and no such host exists in this project.

## Files touched

- `crates/shep-cli/src/commands/startup/mod.rs`
- `crates/shep-cli/src/commands/startup/unit.rs`

(paths are relative to the worktree root `/Users/rin/GitHub/shep-phase14`)
