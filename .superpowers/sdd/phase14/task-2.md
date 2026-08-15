# Phase 14, Task 2 — Flockfile JSON Schema, `shep schema`

## Status: DONE, green

## Commit

`b695d15` on `feat/phase14-config-packaging` in `/Users/rin/GitHub/shep-phase14`
(worktree, not the primary checkout).

## Cargo command shape used

`--workspace` shape for the task gate (this task crosses shep-core and
shep-cli); `-p shep-core --lib --all-features` for the fast iteration loop
while writing the schema tests; `-p shep-cli --bins --all-features` once for
the CLI (never `--lib` — shep-cli is `[[bin]]`-only).

## Baseline (step 2.1) — matched the plan exactly, no drift

```
grep -c "schemars" crates/shep-core/Cargo.toml            -> 0
grep -c "^[features]" crates/shep-core/Cargo.toml          -> 0
grep -rn --include="*.rs" "JsonSchema" crates/shep-core     -> 0
git ls-files crates/shep-core/assets | wc -l                -> 0
grep -c '^name = "schemars"$' Cargo.lock                    -> 1
grep -c "schemars" Cargo.lock                                -> 5
```

## What landed

- `crates/shep-core/Cargo.toml`: non-default `schema` feature, optional
  `schemars = { workspace = true, optional = true }` (adds one dependency
  edge, zero packages).
- `#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]` on
  `AppConfig`, `ProbeConfig`, `ProbeKind` (`app.rs`) and on the private
  `RawFlockfile` (`flockfile.rs`, `#[schemars(rename = "Flockfile")]` so the
  title is right).
- Hand-written `JsonSchema` impls for `MemSize`/`UpDuration` in `values.rs`
  (string-shaped, patterns lifted verbatim from each type's own `FromStr`
  doc), each with a paired `the_schema_pattern_agrees_with_from_str` test
  that runs the schema's regex and `FromStr` over the same accept/reject
  lists.
- `shep_core::config::flockfile_schema_json()` in `flockfile.rs`: renders
  `schema_for!(RawFlockfile)`, pretty-printed with a trailing newline.
  `#[track_caller]` per the plan's `# Panics` note (IR-24).
- The committed artefact: `crates/shep-core/assets/flockfile.schema.json`,
  generated via `cargo run -p shep-cli -- schema` and read before any shape
  assertion was written (step 2.4's bootstrap order, followed as specified).
- The drift guard: `COMMITTED = include_str!(...)` + `REGENERATE` consts
  beside `RawFlockfile`, plus `the_committed_schema_is_current`,
  `the_schema_describes_a_document_not_one_app`,
  `kill_signal_stays_an_unconstrained_string`, and
  `duration_and_memory_fields_are_string_shaped` in `flockfile.rs`'s test
  module, all `#[cfg(feature = "schema")]`.
- `shep-cli`: `shep-core` dependency gains `features = ["schema"]`; hidden
  `Commands::Schema` verb in `cli.rs`; `commands/schema.rs` (thin — calls
  `flockfile_schema_json()` and writes it verbatim, `--format json`
  deliberately ignored per the plan); wired into `main.rs`'s early dispatch
  (same slot as `Completions` — needs no `$SHEP_HOME`) and into the
  exhaustive `unreachable!` catch-all.

## Deviation from the plan's literal code, and why (worth flagging)

The plan's own code for `COMMITTED`/`REGENERATE` (`#[cfg(feature =
"schema")]` consts at module scope, read only from the co-located test)
fails the task gate as written: `cargo clippy --workspace --all-targets
--all-features -- -D warnings` treats the plain `--lib` compilation unit as
a separate target from the `--tests` one, and cfg(test) is off for the
former — so both consts read as dead code there even though the test target
uses them. This reproduced with a plain `cargo check -p shep-core --lib
--all-features` before I'd written a single test, and persisted after.

Fix: added `#[allow(dead_code)]` with a rationale comment on both consts,
following this codebase's own precedent (`output/mod.rs`'s
`write_outcome`/`emit_flock`/etc. carry the identical pattern for the
identical reason — item genuinely used only by `#[cfg(test)]` code, kept
outside the test module on purpose). I did not move `COMMITTED` into the
test itself: that would trade away the property the plan explicitly wants
from `include_str!` — a deleted schema file failing *every* build, not only
`cargo test`.

## Tests: shep-core +6

`the_schema_pattern_agrees_with_from_str` (×2, one per newtype),
`the_committed_schema_is_current`, `the_schema_describes_a_document_not_one_app`,
`kill_signal_stays_an_unconstrained_string`,
`duration_and_memory_fields_are_string_shaped`. All six read against the
generated artefact per step 2.4's order, not against a guess.

shep-cli: +0 new tests (no parse test added for the hidden verb; existing
`Cli::command().debug_assert()` and the cli.rs test suite cover the new
`Schema` variant structurally).

## Generated-artefact shape, as actually read (step 2.4)

- Root: `properties.app` is `{"type":"array","items":{"$ref":"#/$defs/AppConfig"}}`.
  `additionalProperties: false` at root and on `AppConfig`, confirmed —
  `deny_unknown_fields` produced it as expected.
- **No `required` array anywhere** (not predicted explicitly in the plan,
  worth recording): both `RawFlockfile` and `AppConfig` carry
  `#[serde(default)]` at the container level, so every field including
  `name`/`script` has a schema `default` and none is schema-`required`.
  `the_schema_describes_a_document_not_one_app`'s actual assertions don't
  depend on `required` at all (it checks `properties.name.is_null()` at
  root instead), so this didn't need a test change — just noting the shape
  differs from a naive reading of "required names name and script".
- `kill_signal` (`Option<String>`): `{"type": ["string","null"], ...}`, no
  `enum`/`pattern` — confirmed unconstrained.
- `min_uptime` (`UpDuration`, non-`Option`): a bare `$ref` to
  `#/$defs/UpDuration`.
- `max_memory` (`Option<MemSize>`): `$ref` under `anyOf` beside
  `{"type":"null"}` — the second of the two plausible shapes the plan named.
- `description` strings from doc comments are present throughout, including
  the multi-paragraph ones (`action_timeout`, `channel`, etc.) verbatim with
  `\n` line breaks.

## Verify (step 2.7)

```
grep -c '^name = "schemars"$' Cargo.lock   1 -> 1   (confirmed)
grep -c "schemars" Cargo.lock              5 -> 6   (confirmed, observation only)
git ls-files crates/shep-core/assets | wc -l   0 -> 1
grep -rn --include="*.rs" "JsonSchema" crates/shep-core | wc -l   0 -> 7
cargo test -p shep-core --lib --all-features   238 passed; 0 failed; 1 ignored
cargo test -p shep-cli --bins --all-features   416 passed; 0 failed; 3 ignored
```

Windows cross-check wall-clock, recorded per the plan's ask since this task
changes it:

```
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
real 9.39s   (prior recorded baseline: 8.42s)
```

~1s slower, consistent with the plan's prediction that `--all-features`
compiles schemars for the Windows target too even though shep-cli's own
`schemars` sits in a `cfg(unix)` table — cost, not correctness, exactly as
decision 5(c) says.

## Mutations (step 2.8)

**First** (doc-comment edit on `AppConfig::script`): reddened exactly
`the_committed_schema_is_current`, message named the regeneration command,
**nothing else** — blast radius 1, matching the plan exactly. Reverted,
confirmed clean (238 passed).

**Second** (`MemSize`'s `JsonSchema` pattern widened to
`^\d+(G|M|K|T)?$`): reddened **three** tests, not the two the plan names.
`the_schema_pattern_agrees_with_from_str` failed on `512T` as predicted, and
`the_committed_schema_is_current` failed as predicted. A third,
`duration_and_memory_fields_are_string_shaped`, also failed — that test
(written by me in step 2.5, the plan gives no verbatim code for it) asserts
`max_memory`'s resolved `pattern` equals the literal string
`^\d+(G|M|K)?$`, so it catches the same mutation from a third angle. Not a
test that "reddened nothing" — the opposite, extra coverage — but worth
recording since the plan's own text says "two independent failures for one
edit" and I got three. Reverted, confirmed clean (238 passed).

## Task gate — all green, in the order run

```
cargo fmt --all --check                                          exit 0
cargo clippy --workspace --all-targets --all-features -- -D warnings   exit 0 (no warnings)
cargo test --workspace --all-features                             exit 0, all `test result: ok`, 0 failed across every crate (lib, e2e, real_runner, doctests)
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features   exit 0
```

Each run as its own command, `$?` read directly (no pipes), per the
project's zsh-pipestatus rule.

## Concerns

1. The dead-code-under-`--lib` issue above is a real gap in the plan's
   literal example code, not just a style nit — anyone pasting that exact
   snippet hits a red gate. Flagging in case Task 2's plan text gets reused
   or referenced elsewhere in this phase.
2. `duration_and_memory_fields_are_string_shaped`'s extra mutation coverage
   (see above) is fine as shipped, just noting the "two failures" claim in
   the plan doesn't hold for my exact test bodies (the plan didn't supply
   verbatim code for that test, so this isn't a plan defect — just a
   documented delta between the plan's prose and what I wrote).
3. No baseline mismatches found anywhere in step 2.1 or step 2.7 — every
   grep matched the plan's stated number exactly.
