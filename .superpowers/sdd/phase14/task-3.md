# Task 3 — `$schema` accepted and ignored (SEVERABLE)

Status: DONE, green, committed.

Commit: `b1ae5407dd4e21fbc4d88809b9eef2274826628f`
feat(shep-core): accept and ignore $schema on RawFlockfile

## What changed

- `crates/shep-core/src/config/flockfile.rs`: added `schema: Option<String>`
  (`#[serde(default, rename = "$schema")]`) to `RawFlockfile`, destructured
  and discarded in `Flockfile::parse` (`let RawFlockfile { schema: _schema,
  apps } = raw;`). `deny_unknown_fields` untouched. Three new tests:
  `a_schema_key_is_accepted_and_ignored`, `one_more_key_is_legal_and_no_others_are`,
  `a_toml_flockfile_takes_the_key_too`.
- `crates/shep-core/assets/flockfile.schema.json`: regenerated via
  `cargo run -p shep-cli -- schema`. `$schema` now under `properties`;
  `additionalProperties: false` still present.

Field lives on `RawFlockfile` only, never on `AppConfig` — not on the wire,
`PROTOCOL_VERSION` untouched, no fixture edited.

## TDD trace

- Step 3.1 baseline: `grep -c 'rename = "\$schema"' flockfile.rs` → 0 (grep
  exit 1), matched plan. `cargo test -p shep-core --lib --all-features`:
  238 passed, 0 failed (baseline before this task).
- Step 3.2 RED: added the three tests before touching `RawFlockfile`. Ran
  them targeted — `a_schema_key_is_accepted_and_ignored` and
  `a_toml_flockfile_takes_the_key_too` failed with exactly the plan's
  predicted message: `Json("unknown field \`$schema\`, expected \`app\`...")`
  / TOML's equivalent. GREEN: added the field + destructure, all three pass.
- Step 3.3: `the_committed_schema_is_current` went red on its own right after
  the GREEN step (schema now stale) — confirmed the failure message names
  the regen command, ran `cargo run -p shep-cli -- schema > .../flockfile.schema.json`,
  reran: green. Confirmed by hand that `$schema` is under `properties` and
  `additionalProperties: false` survived.
- Step 3.4 MUTATION: deleted `#[serde(deny_unknown_fields)]` from
  `RawFlockfile`. Both named tests reddened as the plan predicts —
  `one_more_key_is_legal_and_no_others_are` (bare `schema` no longer
  rejected) and `the_committed_schema_is_current` (schema drift, since
  `additionalProperties: false` disappears from the emitted schema).
  Mutation did NOT go silent — it is a real guard. Reverted; confirmed
  green again (241 passed, 0 failed, 1 ignored).

No dead-mutation finding this task — both named tests reddened as predicted.

## Gate (cargo command shape: `--workspace` throughout, per this phase's rule)

- `cargo fmt --all --check` — EXIT=0
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — EXIT=0, clean
- `cargo test --workspace --all-features` (first pass, no `--no-fail-fast`):
  2 failures in `cli_e2e` (`a_write_to_a_dot_file_under_a_watched_tree_restarts_nothing`,
  `a_write_under_a_watched_tree_restarts_the_sheep`) — both `deadline_exceeded`
  on the daemon RPC, i.e. real-child/FSEvents timing under load, not touched
  by this task's diff (pure shep-core parser change). Isolated rerun of just
  those two tests: 2/2 passed in 39.53s. Re-ran the full workspace suite with
  `--no-fail-fast` to see past the fail-fast stop: **every result line
  reported 0 failed**, including `cli_e2e` at 47/47 this time. Confirmed
  load-artifact flakiness, exactly the shape the plan warns about ("Bounded
  waits on real children produce false radii under load").
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` — EXIT=0

## Test numbers

`shep-core --lib --all-features`: 241 passed, 0 failed, 1 ignored (238
baseline + 3 new). Full workspace (`--no-fail-fast`, all crates + integration
suites + doctests): every result line 0 failed — shep bin 416, cli_e2e 47,
term_panic_order 1, shep-client lib 6, event_stream 4, event_stream_next 1,
request_reply 8, spawn 7, shep-core lib 241, process_info_builder 1,
shep-daemon lib 500, daemon_e2e 18, external_impls 1, real_runner 13,
plus 3 more small suites and doctests (3 doctests, all ok).

## Concerns

- None in this task's own diff. The one thing worth flagging up: `cli_e2e`'s
  two watch-restart tests are flaky under concurrent machine load (this
  worktree is one of two phases building simultaneously per the dispatch
  brief) — not a regression from Task 3, but future task reports on this
  branch should expect the same false-radius shape and re-run in isolation
  before trusting a `cargo test --workspace` failure that touches
  `watch`/FSEvents timing.
