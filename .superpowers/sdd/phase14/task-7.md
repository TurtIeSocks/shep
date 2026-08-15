# Task 7 — the openrc renderer

## Status: DONE, all gates green

## Baseline (step 7.1)

`ancestor=0` — this worktree's history descends from the commit that wrote
the plan, so its greps apply unmodified.

```
grep -rni "openrc" crates                                              | 33
grep -rn "openrc/rc.d are named as deferred" crates                     | 0  (matches plan)
grep -rn "openrc and the BSD rc.d scripts" crates/shep-cli/src          | 0  (matches plan)
grep -c "fn openrc_script" crates/shep-cli/src/commands/startup/unit.rs | 0  (matches plan)
grep -c "rc-update" crates/shep-cli/src/commands/startup/mod.rs         | 0  (matches plan)
grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src               | 3  (matches plan)
cargo test -p shep-cli --bins --all-features                            | 424 passed; 0 failed; 3 ignored
```

Every baseline matched the plan's stated expectation except the openrc count,
which the plan only says to "record" (no target given). 424/0/3 is the
number this task's own deltas are measured against.

## What shipped

- `crates/shep-cli/src/commands/startup/unit.rs`: `pub(crate) fn
  openrc_script(&UnitSpec) -> String` (the `#!/sbin/openrc-run` script, byte
  for byte per the plan's template) and `fn sh_double_quoted(&str) -> String`
  (escapes `"`, `$`, `` ` ``, `\` for content inside a double-quoted shell
  assignment, single-pass so it can't double-escape).
- `crates/shep-cli/src/commands/startup/mod.rs`:
  - `install`'s `Init::Openrc` arm now runs `rc-update add <unit> default`
    then `rc-service <unit> start`.
  - `remove`'s `Init::Openrc` arm now runs `rc-service <unit> stop`, `rc-update
    del <unit> default`, then removes the file.
  - `unbuilt_renderer` no longer refuses `Init::Openrc` (folded into the
    `None` arm with `Systemd`/`Launchd`).
  - `only_the_unbuilt_renderers_are_refused` updated to assert
    `unbuilt_renderer(Init::Openrc) == None`.
  - **Also fixed `write_unit`'s render-dispatch match** to call
    `unit::openrc_script(&plan.spec)` for `Init::Openrc` — see finding below,
    this was not in the plan's Task 7 text but is required for the feature to
    work at all.
- `crates/shep-cli/src/commands/startup/unit.rs` tests: the five specified
  in step 7.4, with one assertion string corrected — see finding below.

## Tests (step 7.4/7.5)

```
cargo test -p shep-cli --bins --all-features
```
429 passed; 0 failed; 3 ignored (+5 over baseline, exactly the five new
tests, `0` -> `0` ignored delta as expected).

Verify greps:
```
grep -c "fn openrc_script" .../unit.rs        0 -> 1   (matches plan)
grep -c "rc-update" .../mod.rs                0 -> 2   (matches plan)
grep -rn "TASK-7-8 REPLACES THIS ARM" ...      3 -> 0   (plan predicted 3 -> 2; see finding)
grep -rni "openrc" crates                     33 -> 53  (well up, matches plan's qualitative expectation)
```

## Findings (both reported per the plan's instruction to say so, not paper over)

**1. Step 7.3 never mentions `write_unit`, but leaving it unchanged would
ship a functionally broken `--init openrc`.** The plan's step 7.3 shows only
the `install`/`remove` match-arm edits. `write_unit`'s own render-dispatch
match (`Init::Systemd => ..., Init::Launchd => ..., Init::Openrc |
Init::FreebsdRc | Init::OpenbsdRc => String::new()`) is not discussed
anywhere in Task 7. Left as-is, `shep startup --init openrc` would write an
**empty**, mode-0755 `/etc/init.d/shep-<user>` (via `write_unit`, step 1 of
`install`) and then run `rc-update add` / `rc-service start` against that
empty file — the renderer built in step 7.2 would never actually reach disk.
I split `write_unit`'s match to call `unit::openrc_script` for `Init::Openrc`,
same shape as the `install`/`remove` split. This is why the `TASK-7-8
REPLACES THIS ARM` marker count landed at `3 -> 0`, not the plan's predicted
`3 -> 2`: all three sites (`install`, `remove`, `write_unit`) needed their
`Openrc` arm pulled out, not just the two the plan showed. I renamed the two
remaining `FreebsdRc | OpenbsdRc`-only arms' comments to `TASK-8 REPLACES
THIS ARM` (accurate now — only two inits, not three — and Task 8 is what
removes them), which is why the old string's count went to 0 rather than 1.
This is a plan gap, not an implementation deviation from spec intent: the
feature description (openrc `install`/`unstartup` fully working) requires it.

**2. The step 7.4 test `the_openrc_script_quotes_shell_metacharacters` has a
transposed assertion string that can never match, regardless of whether
escaping is implemented correctly.** The plan's fixture is `PathBuf::from(r#"/tmp/we"ird/$HOME/`x`/back\slash"#)`
(literal text `we"ird`, i.e. `w`,`e`,`"`,`i`,`r`,`d`). Correctly escaped, that
substring becomes `we\"ird` — backslash inserted immediately before the
quote, so `e` still precedes the escaped quote. The plan's assertion is
`rendered.contains(r#"\"eird"#)` — literal `\`,`"`,`e`,`i`,`r`,`d` — which
puts `e` *after* the escaped quote instead of before it. That six-character
sequence does not occur in the correctly-escaped string in either order (nor
in the unescaped source), by direct check:
```python
home = r'/tmp/we"ird/$HOME/`x`/back\slash'
escaped = ''.join(('\\'+c if c in '"$`\\' else c) for c in home)
# escaped == '/tmp/we\\"ird/\\$HOME/\\`x\\`/back\\\\slash'
'\\"eird' in escaped   # False
'we\\"ird' in escaped  # True
```
This is exactly the "pattern cannot match the real text" trap the plan's own
methodology section warns about (character transposition in the fixture
text). I wrote the test with the corrected assertion — `rendered.contains(r#"we\"ird"#)`
— which preserves the test's actual intent (prove the quote character gets
escaped where it occurs) and is what step 7.6's second mutation (drop
`sh_double_quoted` from `home`) actually reddens; I confirmed the mutation
does correctly fail this corrected test. The other three assertions
(`$HOME`, backtick, backslash) in that same test were exact as written in the
plan and needed no change.

## Mutations (step 7.6)

1. **Delete the `start_post` block.** Plan says "both readiness tests fail".
   Only `the_openrc_script_polls_for_readiness_and_bounds_the_wait` reddened;
   `the_openrc_script_says_why_it_polls` stayed green, because both strings
   it checks (`"openrc has no sd_notify analogue"`, `"binds its control
   socket before the restore"`) live in the *top-of-script* header comment
   (rendered before `name=...`), not inside the `start_post` function body or
   its own preceding comment — deleting `start_post` doesn't touch them. This
   is a genuine partial-miss versus the plan's stated expectation, reported
   per the "if a mutation reddens nothing [or less than expected], that is a
   finding" instruction. Reverted; file confirmed byte-identical to the
   pre-mutation state via `diff`.
2. **Drop `sh_double_quoted` from the `home` interpolation.** Reddened
   `the_openrc_script_quotes_shell_metacharacters` exactly as predicted (with
   the corrected assertion string from finding 2). Reverted.
3. **Change `name=` back to the constant `"shep"`.** Reddened
   `the_openrc_name_is_per_user_and_matches_the_file` exactly as predicted.
   Reverted.

After all three reverts, `unit.rs` is byte-identical (via `diff`) to its
state immediately after `cargo fmt`.

## Task gate (step 7.7)

Each run separately, `$?` captured directly, one cargo command at a time,
workspace shape throughout:

```
cargo fmt --all --check                                                   EXIT=0
cargo clippy --workspace --all-targets --all-features -- -D warnings      EXIT=0
cargo test --workspace --all-features                                     EXIT=0, 429/47/1/6/4/1/8/7/246/1/500/18/1/13/2/4/3 all "0 failed" across all 17 result lines
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features  EXIT=0
```

## Commit

Committed on `feat/phase14-config-packaging` before writing this report, per
the overnight-run rule.
