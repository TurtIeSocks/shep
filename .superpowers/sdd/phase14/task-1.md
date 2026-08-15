# Phase 14 — Task 1: `.js` Flockfiles behind `--flockfile`

**Status:** DONE, task gate green, committed.

**Commit:** `4ec54ec` — "feat(cli): read .js Flockfiles through node behind --flockfile"

## Test numbers

- `cargo test -p shep-core --lib --all-features`: 232 passed / 0 failed / 1 ignored (baseline 231, +1: `discovery_never_names_a_js_file_and_stays_ten_names`)
- `SHEP_REQUIRE_NODE=1 cargo test -p shep-cli --bins --all-features`: 416 passed / 0 failed / 3 ignored (baseline 409, +7: 1 in `cli.rs`, 6 in `lifecycle.rs`)
- `cargo test --workspace --all-features`: 1264 passed / 0 failed / 5 ignored across 17 result lines (baseline stated in dispatch: 1256/0/5 — delta is exactly the 8 tests this task adds; ignored count unchanged)
- `cargo fmt --all --check`: EXIT=0
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: EXIT=0
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`: EXIT=0

All baseline greps from Step 1.1 matched the plan exactly before starting (`JSON.stringify(require` 0→1, `a fifth backend` 1→0, `flockfile` in cli.rs 0, `DISCOVERY_ORDER` at line 177/10 names). Ancestry check (`ancestor=0`) confirmed the plan's greps were derived against a tree this branch descends from, so none needed re-deriving.

## What shipped

- `StartArgs.flockfile: bool` (`crates/shep-cli/src/cli.rs`) — `--flockfile` flag, defaults off.
- `resolve_target` gained an `as_flockfile: bool` parameter and one new match arm, inserted between the `-` arm and the recognised-extension arm exactly as decision 1 specifies. With the flag false, every branch is byte-for-byte what it was before.
- `evaluate_js_flockfile` in `crates/shep-cli/src/commands/lifecycle.rs`: spawns `node`, canonicalizes the path first, passes it only as `process.argv[1]` (never interpolated into JS source), stdin `/dev/null`, stdout+stderr captured.
- Two new `TargetError` variants: `UnknownFlockfileFormat` (Usage) and `Js { detail, node_missing }` (Failure if node missing, InvalidConfig otherwise) — no new `ExitCode` variant, confirmed against `exit.rs`.
- `FlockfileError`'s `#[non_exhaustive]` rationale rewritten (`a fifth backend` prediction removed) since `.js` never reaches shep-core.
- Discovery-order pin test added in `flockfile.rs`: 10 names, none end in `.js`, all parseable — mechanizes Rin's ruling.
- `SECURITY.md` gained a `### Flockfiles` section stating shep-core executes nothing and `.js` is the one opt-in exception reached only through `--flockfile`. Verified the "used by nothing today" claim about `discover()` still holds (`grep -rn "discover" crates` — only the re-export and its own tests call it).
- 7 new tests total: `start_takes_a_flockfile_flag_and_defaults_it_off` (cli.rs) + 6 in lifecycle.rs (script-without-flag regression guard, flag-no-op-on-toml, unreadable-extension refusal, plus 3 node-gated cases: js-evaluated, throws→InvalidConfig quoting node, pm2-shape refused naming both keys).

## Two deviations from the plan's literal code — both load-bearing, both self-flagged

1. **Dropped the `path` field from `TargetError::Js`.** The plan's struct carried `path: PathBuf` alongside `detail: String`, but nothing reads `path` outside the derived `Debug` impl — every `detail` string is already built with the path baked in. `cargo clippy -D warnings` flags this as dead code (derived-trait reads don't count for dead-code analysis). Fixed by dropping the redundant field rather than adding `#[allow(dead_code)]`.

2. **Switched the node invocation from bare `node -p` to `node -e` with an in-script `try`/`catch`.** This is the substantive one. Decision 3's literal algorithm — let `require()` throw uncaught, then take "the last non-blank line of stderr" as the message — does not work on a current node. Verified empirically against node v26.5.0 across four failure shapes (thrown `Error`, `SyntaxError`, `MODULE_NOT_FOUND`, thrown plain string): node's uncaught-exception crash dump always ends with a trailing `Node.js vX.Y.Z` banner line, so "last non-blank line" reliably grabs the banner (or, depending on stack depth, a stack frame) — never the actual message. My first implementation attempt reproduced this exactly: `a_js_flockfile_that_throws_is_an_invalid_config_quoting_node` failed with `got: node could not evaluate <path>: Node.js v26.5.0`.

   Fix: `node -e` running `try { process.stdout.write(JSON.stringify(require(process.argv[1]))); } catch (err) { process.stderr.write(err && err.message ? String(err.message) : String(err)); process.exitCode = 1; }`. This writes exactly the failure message to stderr ourselves, sidestepping V8's crash-dump formatting entirely, and stays inside the *documented* mechanic — the plan's own doc comment on `evaluate_js_flockfile` already says "Under `-p` / `-e`, node puts the first user argument at `process.argv[1]`" — so this is a narrower implementation choice, not a different design. The path is still never interpolated into JS source, still passed only via `process.argv[1]`. Extraction was changed from `.rev().find(...)` (last non-blank line) to `.find(...)` (first non-blank line), since our own message is deliberately single-purpose stderr content now.

   Both fixes are documented in-line at the point of use (`crates/shep-cli/src/commands/lifecycle.rs`), with the empirical evidence stated in the doc comment rather than just asserted.

## Mutations (Step 1.8)

- **Mutation 1** (plan's stated "headline" mutation, revised): reorder the `as_flockfile` arm below `(_, Some(format))`. Confirmed no-op per the plan's own analysis — did not re-run empirically since the plan already identifies this as testing nothing (identical read-and-parse for recognised extensions; `.js`/unrecognized extensions still fall through since `FlockFormat::from_path` returns `None` for them either way). Went straight to the mutation the plan says is the real one.
- **Mutation 2** (the actual test, per plan's correction): reorder the `as_flockfile` arm below `_ if path.exists()`. Ran with `--no-fail-fast`: exactly the 4 named tests failed (`the_flag_refuses_an_extension_it_cannot_read`, `a_js_flockfile_under_the_flag_is_evaluated`, `a_js_flockfile_that_throws_is_an_invalid_config_quoting_node`, `a_pm2_ecosystem_shape_is_refused_naming_the_right_key`). Blast radius 1 (shep-cli bins), as predicted. Reverted, confirmed diff-clean against the pre-mutation file.
- **Mutation 3**: delete the `as_flockfile` guard entirely (arm always matches). Ran with `--no-fail-fast`: 5 failed — `a_js_file_without_the_flag_is_still_a_script`, `an_explicit_name_overrides_the_file_stem`, `any_other_existing_path_becomes_one_minimal_app_named_for_its_stem` (the pre-existing test that predates this feature), `a_fold_flag_lands_on_the_resolved_app`, `start_asks_for_the_longer_deadline` (the latter two timed out — their fixtures route through the now-always-flockfile arm and either fail Flockfile parsing or hang trying to reach the daemon). Wider than the plan's own 3-named-test enumeration but consistent with its prediction ("every test that passes a plain script path... a pre-existing test nobody wrote for this feature going red is the confirmation"). Reverted, confirmed diff-clean.

No mutation reddened nothing — both real mutations produced real, correctly-scoped failures.

## RED step note

Step 1.2's literal text describes the un-implemented test as failing "with clap's `unexpected argument '--flockfile' found`". In practice, since the field-access assertions (`a.flockfile`, `b.flockfile`) are baked into the same test as written, the actual RED failure mode observed was a compile error (`E0609: no field 'flockfile' on type 'cli::StartArgs'`) rather than a clap runtime panic — a stronger form of RED (nothing compiles without the work), confirmed by temporarily reverting `cli.rs` to HEAD and adding only the test. Not a defect, just a note that the plan's stated failure mode doesn't exactly match what a single-file diff produces.

## Concerns for later tasks / review

- The `evaluate_js_flockfile` node-invocation change (deviation 2 above) is confined to shep-cli and touches nothing any other task depends on, but Task 9 (docs/changelogs) should be aware the actual command is now `node -e <script> <path>`, not `node -p "JSON.stringify(require(process.argv[1]))" <path>` — if Task 9's migration.md or changelog entries quote the literal invocation, they should quote the shipped one.
- `docs/specs/deferred.md` still needs the "no timeout on `.js` evaluation" entry per decision 3 — not added here, this task's scope was code + SECURITY.md only per the plan's stated file list; Task 9 owns deferred.md.
- Confirmed no interaction with `DaemonConfig`/proof-token questions (Task 4) or with anything in `commands/startup/` (Tasks 6-8) — this task touched only `cli.rs`, `commands/lifecycle.rs`, `config/flockfile.rs`, and `SECURITY.md`, exactly the plan's stated file list.
