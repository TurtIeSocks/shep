# Phase 14 — Task 4 report

**Status:** DONE, all gates green.
**Commit:** `3714859` on `feat/phase14-config-packaging` (worktree
`/Users/rin/GitHub/shep-phase14`).

## What landed

`crates/shep-core/src/config/daemon.rs`, `crates/shep-core/src/config/mod.rs`:

- `#[non_exhaustive]` on `DaemonConfig`, with the decision-7 doc comment
  stating plainly that the attribute is for field growth, not a validation
  guarantee — it blocks struct literals and functional update from outside
  the crate but not field mutation, and does not make the type a proof
  token.
- The `max_cron_sleep` floor check extracted out of `load` into a private
  `fn validate(&self, key: &'static str) -> Result<(), DaemonConfigError>`,
  called once at the bottom of a new `load_layered`. `load` now delegates:
  `Self::load_layered(file_source, env, &DaemonOverrides::new())`.
- `DaemonOverrides`: `#[non_exhaustive]`, consuming-self builder
  (`ProcessInfo::builder` shape) over four `Option` fields (`log_json`,
  `log_level`, `socket`, `max_cron_sleep`), derived `Debug` (IR-41 —
  no secrets in any of the four).
- `parse_daemon_bool`: exported the shared `1|0|true|false` grammar;
  `load_layered`'s `SHEP_LOG_JSON` arm now calls it instead of inlining the
  match.
- Both exported from `config/mod.rs`.
- 5 new tests in `daemon.rs`'s `mod tests`, all named in the plan's step 4.5.

## Baselines (step 4.1)

All matched the plan's stated values before any edit:

```
grep -c "fn validate" ...daemon.rs                → 0 (exit 1)
grep -c "^#\[non_exhaustive\]" ...daemon.rs        → 1
grep -c "DaemonOverrides" ...daemon.rs             → 0 (exit 1)
grep DaemonConfig{ outside daemon.rs               → 0 (exit 1)
cargo test -p shep-core --lib --all-features       → 241 passed, 0 failed
```

## Verify (step 4.6)

```
grep -c "fn validate" ...daemon.rs                 → 1  (0→1, matches)
grep -c "^#\[non_exhaustive\]" ...daemon.rs         → 3  (1→3, matches)
cargo test -p shep-core --lib --all-features        → 246 passed, 0 failed  (+5, matches)
cargo test -p shep-cli --bins --all-features         → 416 passed, 0 failed, 3 ignored (unchanged, matches)
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
                                                      → 482 passed, 0 failed (unchanged, matches)
```

`load` kept its exact contract — shep-cli and shep-daemon counts are
identical to pre-change, confirming `load_layered` is a pure extension.

## Mutations (step 4.7) — two findings

**Mutation 1** (move `self.validate(key)?` from the bottom of
`load_layered` to immediately after the file parse, before env/flags are
folded in). The plan states this should redden exactly two tests:
`a_flag_rescues_a_below_floor_file_value` and the pre-existing
`env_max_cron_sleep_floor_check_runs_on_the_winner`.

**Actual: three tests failed.** A third test not named in the plan's
mutation step also reddened:
`a_below_floor_flag_is_refused_naming_the_flag` — it goes from
`unwrap_err()` to an `Ok`, because with validation moved before the flags
pass, a flag that sets a below-floor value is applied to `cfg` but never
re-checked. This is a real gap in the plan's own enumeration, not a defect
in the mutation or the code: the test does correctly catch the mutation,
the plan just didn't say so. Reverted cleanly; re-ran `config::daemon` —
back to 26 passed, 0 failed.

**Mutation 2** (change `DaemonOverrides::log_json` from `Option<bool>` to
`bool`, with the field-growth setter and the `load_layered` call site
updated to compile — the natural shape of that mutation, since an
`Option`-typed field can't compile against a plain `bool`). The plan states
this should redden `an_absent_flag_leaves_every_lower_layer_alone`.

**Actual: that named test does NOT fail — it stays green.** Both branches
of the equality it checks (`load(src, no_env)` and
`load_layered(src, no_env, DaemonOverrides::new())`) route through the same
mutated code, which now unconditionally writes `overrides.log_json`
(`false`, the type's default) over whatever the file/env layers set. Both
sides land on the same wrong value and the `assert_eq!` between them still
holds — the test compares layered-vs-plain, not either one against the
correct answer, so it's blind to a mutation that breaks both identically.
The mutation *is* caught, but by two different, pre-existing tests instead:
`file_sets_values_and_keeps_dog_sections_raw` and `env_overrides_file`,
both plain `load()` callers that assert `log_json` survives a file/env
value — they fail because `load()` itself now routes through the same
always-overwrite path. Reverted cleanly; re-ran `config::daemon` — back to
26 passed, 0 failed, confirmed identical to pre-mutation state via `diff`
against the pre-Task-4 backup.

**Summary of the two findings:** mutation 1 under-names its blast radius
(3 real failures vs. 2 stated); mutation 2's named test doesn't fire at all
— the coverage exists, but in the wrong two tests, not the one the plan
points at. Neither is a defect in the shipped code; both are inaccuracies
in the plan's mutation-testing narrative that a future editor of this
section should fix.

## Task gate

```
cargo fmt --all --check                                              → EXIT=0
cargo clippy --workspace --all-targets --all-features -- -D warnings → EXIT=0
cargo test --workspace --all-features                                → EXIT=0, all "test result: ok",
                                                                          0 failed across every binary
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features → EXIT=0 (after one fix, below)
```

One real finding from the doc gate, fixed before commit: the first draft of
`load_layered`'s doc comment linked `[`Self::validate`]`, an intra-doc link
to a private item, which `rustdoc::private_intra_doc_links` (implied by
`-D warnings`) rejects. Reworded to plain prose (`the private \`validate\`
method below`) rather than suppressing the lint.

shep-core lib total: 246 passed (was 241; +5 exactly, all named above).
Full-workspace total across every binary/doctest: all green, 0 failed
(largest components: shep-daemon lib 500 passed, shep-cli bins 416 passed,
cli_e2e 47 passed, daemon_e2e 18 passed).

Cargo command shape used throughout: `cargo test --workspace --all-features`
per this dispatch's instruction (the phase crosses shep-core and shep-cli);
`cargo test -p shep-core --lib --all-features` for the fast loop while
iterating inside shep-core alone. No mixing of `-p` and `--workspace` forms
within a given check.

## Notes for the reader

- No CLI changes in this task, as specified — Task 5 wires `DaemonOverrides`
  into `DaemonArgs`.
- `DaemonConfig` is not, and does not become, a proof token; its doc comment
  says exactly that (decision 7c). Did not write anything implying
  validation is enforced by construction.
- Did not touch anything under `/Users/rin/GitHub/pm2` or reference it.
- Did not invoke shep-idiomatic-rust guidance selectively — loaded it before
  writing any Rust, per the project's hard trigger.

## Concern

The two mutation-testing findings above (section "Mutations") are worth a
look before Task 5 reuses this file's test patterns — specifically, a test
that compares two mutated-identically branches against each other (like
`an_absent_flag_leaves_every_lower_layer_alone`) can silently stop being a
mutation-catcher for its own named property while still passing, and only
an unrelated test happens to save it. Nothing to fix in this task's shipped
code — both real coverage gaps turned out to be already covered by other
tests, by luck rather than by the plan's design — but the plan's own
mutation narrative undercounts blast radius in one direction and
misattributes it in the other, and Task 5 or a later reviewer should not
copy the same "compare two branches that were mutated together" pattern for
its own flag-plumbing tests, since it does not actually catch the argued
mutation.
