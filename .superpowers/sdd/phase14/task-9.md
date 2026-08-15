# Task 9 — docs, ledger, changelogs

## Status: done, green

Doc-only task: no Rust source touched, so no RED/GREEN TDD steps and no
named mutations apply here — Task 9 is a reconciliation pass, verified by
the grep checks the plan itself specifies (steps 9.1 and 9.8) plus the full
phase gate (step 9.9).

## Step 9.1 — baseline

All eight baseline greps matched the plan's stated numbers exactly, on the
first read, no drift:

```
grep -c "the directory does not exist" docs/specs/deferred.md   -> 1
grep -c "ecosystem.config.js" docs/migration.md                 -> 2
grep -c "openrc and BSD rc.d remain open" docs/specs/deferred.md -> 1
grep -c "Deferred because making the fields private" docs/specs/deferred.md -> 1
grep -c "node was not found on PATH" docs/migration.md          -> 0
grep -c "to a .toml Flockfile" docs/migration.md                -> 0
grep -c "^## \[Unreleased\]" crates/shep-core/CHANGELOG.md      -> 1
grep -c "^## \[Unreleased\]" crates/shep-cli/CHANGELOG.md       -> 1
```

Also independently re-verified, before writing anything, that the code-level
claims Task 9's prose depends on actually hold in the tree Tasks 1-8 left
behind (not read from memory of the plan's own claims):

- `crates/shep-cli/src/commands/lifecycle.rs` has all four sentences from
  decision 3's failure taxonomy, including `node was not found on PATH` and
  `to a .toml Flockfile` verbatim.
- `crates/shep-core/src/config/daemon.rs` has `fn validate` (private,
  called once from `load_layered`), `pub fn load`, `pub fn load_layered`,
  `struct DaemonOverrides` (`#[non_exhaustive]`, consuming-self builder:
  `log_json`/`log_level`/`socket`/`max_cron_sleep`), and
  `pub fn parse_daemon_bool`.
- `crates/shep-core/Cargo.toml` has the `schema = ["dep:schemars"]` feature
  with the "gate is inert inside this workspace" rationale worded almost
  exactly as decision 5(c) describes it.
- `crates/shep-core/src/config/flockfile.rs` has the `#[cfg(feature =
  "schema")]` derive on `RawFlockfile` (renamed `Flockfile`), the
  `include_str!` `COMMITTED` constant, and `flockfile_schema_json`.
- `crates/shep-cli/src/cli.rs` defines `pub enum Init` and imports nothing
  but `std::path::PathBuf` — matches decision 10's placement requirement.
- `crates/shep-cli/src/commands/startup/unit.rs`: `grep -c
  "allow(dead_code)"` is `0` (both attributes deleted, as decision 10
  requires after the move).
- `crates/shep-cli/src/commands/startup/mod.rs` has `current_init` /
  `linux_init` with the systemd-then-openrc probe order and `fn unit_mode`,
  and `sh_double_quoted` / `shell_quote` both exist with the composition
  Task 8's own report describes.

## Step 9.2 — `docs/specs/deferred.md`

- Deleted the four "not yet built" entries (`.js` Flockfile, schemars,
  daemon-config flags layer, openrc/BSD rc.d units) from "Named as v1.0 in
  spec §2/§9, not yet built" — `dev/runtime` and `Windows functional tier`
  stayed, since those are still genuinely open.
- Rewrote `### DaemonConfig is not a proof token, unlike ResolvedApp` into
  the re-derived resolution from decision 7: not a proof token, does not
  become one, `#[non_exhaustive]` is for field growth only, contract is
  stated not enforced, escape hatch is a public `validate`. Did **not**
  write that `#[non_exhaustive]` proves validation — the opposite is stated
  explicitly, with the `Default::default()` + field-mutation counterexample
  from the plan.
- Rewrote the `shep startup`/`unstartup` "Not deferred" entry to cover all
  four init systems, with three named caveats: the Linux container
  behaviour change, openrc/FreeBSD's socket-poll readiness proxy and
  OpenBSD's honest lack of one, and the "none of the three new scripts has
  been executed on its own OS" line.
- Added three new "Not deferred, shipped" entries (`.js` Flockfile,
  schemars, daemon-config flags layer) in the same style as the existing
  entries, each naming what actually shipped and its real boundaries (the
  document-not-`AppConfig` schema shape, the four-flags-only layer, the
  explicit-only `.js` ruling).
- Added two new known-debt entries: no `.js` evaluation timeout, and the
  missing-node error message has no test (pinned in `docs/migration.md`
  instead, for the `std::env::set_var`-is-unsafe reason decision 3 gives).

## Step 9.3 — `docs/specs/shep-v1.md`

Amended §5's Flockfile paragraph: the schema-location sentence now says
`crates/shep-core/assets/` and "describes the whole document, not just
`AppConfig`"; added the `**Amended, Phase 14 (Rin's ruling).**` paragraph
using the plan's own wording almost verbatim (explicit-only `--flockfile`,
ten-name discovery order unchanged, `shep import` remains the pm2 path).

Checked §5's layering sentence (`file < SHEP_* env < CLI flags`) and §11's
init list (`systemd; launchd; openrc; freebsd/openbsd rc.d`) — both are
already accurate statements about shipped code, so left unchanged per the
plan's own instruction not to add a second claim beside an already-true one.

## Step 9.4 — `docs/migration.md`

Added the `### If your config is a .js file` section right after the
existing "does not read `ecosystem.config.js`" paragraph, with both pinned
clauses verbatim from decision 3's table.

## Step 9.5 — `docs/releasing.md`

Two new paragraphs under "Not blockers": the three-new-init-scripts caveat,
and the committed-artefact/`include_str!`/regeneration-command paragraph.
Also corrected the adjacent "Several v1.0 spec items are unbuilt" bullet,
which the splice would otherwise have left false — it used to name `.js`
Flockfiles, schemars, the CLI-flag layer, and openrc/BSD units as unbuilt;
all four shipped this phase, so the bullet now names only what is still
actually unbuilt (`serve`, `dev`, `runtime`, three lookout panes) and points
at the two new paragraphs for what shipped.

Also updated the stale `1219 passed / 0 failed / 4 ignored` figure in the
"Blockers" section to the number this task actually measured on the tree it
is describing: **1298 passed / 0 failed / 5 ignored** (see Verification run
below) — the old number predates this phase's new tests and would have been
a false claim sitting two paragraphs from a section this task exists to keep
honest.

## Step 9.6 — CHANGELOGs

`crates/shep-core/CHANGELOG.md` — four new `### Additions` entries
(`load_layered`/`DaemonOverrides`, `parse_daemon_bool`, the `schema`
feature, `$schema`) prepended to the existing `[Unreleased]` list, plus one
new entry under the existing `### Changes` heading (the file uses "Changes",
not "Changed" — matched the file's own convention rather than the plan's
literal wording) for `DaemonConfig`'s `#[non_exhaustive]`, explicit that it
is a breaking change for out-of-tree struct-literal construction and stating
plainly it does not mean validated.

`crates/shep-cli/CHANGELOG.md` — six new `### Additions` entries
(`--flockfile`, `shep schema`, the four daemon flags, `--init`, the three
new renderers) prepended to the existing list. The file had no `### Changes`
heading yet; added one (before `### Fixes`, matching shep-core's naming) for
the Linux-container regression, worded to match the plan's required content
(the old unconditional-systemd behaviour, what changed, why, the `--init
systemd` escape hatch).

Checked shep-cli's existing historical entries for anything now false about
the past (the plan's instruction) — found one line describing the original
Phase 10 `startup` addition ("a Linux host running openrc still gets a
systemd unit") which is a true description of that addition's own state at
the time it shipped, not a present-tense claim; left it as written, per
"a historical entry describing a past state stays."

## Step 9.7 — CLAUDE.md

Added one sentence to the Status paragraph naming Phase 14's four shipped
items and the `.js` refusal shape, after the existing Phase 13 sentence and
before "What's built vs. deferred."

## Step 9.8 — verify

All nine checks matched the plan's stated post-state exactly:

```
grep -c "the directory does not exist" docs/specs/deferred.md        -> 0
grep -c "openrc and BSD rc.d remain open" docs/specs/deferred.md     -> 0
grep -c "Deferred because making the fields private" docs/specs/deferred.md -> 0
grep -c "node was not found on PATH" docs/migration.md               -> 1
grep -c "node was not found on PATH" crates/shep-cli/src/commands/lifecycle.rs -> 1
grep -c "to a .toml Flockfile" docs/migration.md                     -> 1
grep -c "to a .toml Flockfile" crates/shep-cli/src/commands/lifecycle.rs       -> 1
grep -c "ecosystem.config.js" docs/migration.md                      -> 3
grep -rn "proves it was validated" crates docs/specs | wc -l          -> 0
```

Also confirmed the plan's own guard for step 9.2 item 5: `grep -c "is not a
proof token" docs/specs/deferred.md` is still `1` (the heading text, which
must survive the rewrite unchanged) rather than the wrong `is not a proof
token` phrasing the plan warned against using as the check.

## Step 9.9 — phase gate

One `CARGO_TARGET_DIR` for the whole task, workspace shape throughout, one
cargo command at a time as required. `benches/` gates run concurrently with
the workspace commands since it is its own separate-lock workspace.

- `cargo fmt --all --check`: exit 0, clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  exit 0, clean.
- `cargo test --workspace --all-features`: exit 0. **1298 passed, 0 failed,
  5 ignored** across 17 result lines (up from the plan's "~1200 across 17
  result lines" baseline shape, consistent with 8 feature-adding tasks
  landing since the baseline was measured).
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`:
  exit 0, clean. (Task 8's own report flagged a pre-existing broken
  intra-doc link in `crates/shep-cli/src/commands/dogs.rs` as unrelated
  debt at the time; it does not reproduce now, so it was fixed by an
  intervening task's diff — not investigated further since this task's own
  gate is green.)
- `cargo test --workspace --all-features -- --test-threads=1`: exit 0,
  same 1298/0/5 totals, serial. No regression the parallel run masked.
- `cargo check -p shep-daemon --all-targets --all-features --target
  x86_64-unknown-linux-gnu`: exit 0, clean.
- `cargo check --workspace --all-targets --all-features --target
  x86_64-pc-windows-gnu`: exit 0. 56 dead-code warnings on `shep-daemon`'s
  lib (`cfg(unix)`-gated modules, per this repo's own documented reasoning
  for using `check` rather than `clippy -D warnings` here) — not a failure,
  matches the documented shape.
- `cargo +stable bench --manifest-path benches/Cargo.toml -- --test`: exit
  0, both cases (`tree_rss/500_process_tree`, `sysinfo_sampler/sample_real_
  machine`) report `Success`.
- `cargo +1.88 bench --manifest-path benches/Cargo.toml -- --test`: exit 0,
  same two cases `Success`.
- `cargo publish --workspace --dry-run`: **first run refused**, correctly —
  `error: 1 files in the working directory contain changes that were not
  yet committed into git: crates/shep-core/CHANGELOG.md`. This is the
  dry-run doing its job, not a defect; committed this task's changes (see
  below) and re-ran clean, exit 0. This is also the check decision 5(b)
  argues for keeping in the gate permanently — it is the one thing that
  would have caught the schema artefact living outside its package
  directory, had that mistake shipped.

Did not run `cargo check -p shep-cli --target x86_64-unknown-linux-gnu` —
out of Task 9's own command list (only `-p shep-daemon` is), and the plan
names this machine's lack of a Linux cross C toolchain for `ring` as a known,
already-recorded gap, not something for this task to re-attempt.

## Two things this phase gets wrong if nobody says them (confirmed honored)

- No sentence anywhere in this diff claims `#[non_exhaustive]` on
  `DaemonConfig` proves a config was validated. The rewritten
  `deferred.md` section says the opposite outright, with the counterexample.
  `grep -rn "proves it was validated" crates docs/specs` is `0`.
- No sentence anywhere in this diff claims shep supports FreeBSD, OpenBSD,
  or openrc. Every new paragraph describing those three renderers says what
  was actually verified — rendering, pinned by exact-string tests — and
  states plainly that none has been executed on its own operating system,
  because no such host exists here.

## Files touched

- `docs/specs/deferred.md`
- `docs/specs/shep-v1.md`
- `docs/migration.md`
- `docs/releasing.md`
- `crates/shep-core/CHANGELOG.md`
- `crates/shep-cli/CHANGELOG.md`
- `CLAUDE.md`
- `.superpowers/sdd/phase14/task-9.md` (this report)

No Rust source files touched. (Paths are relative to the worktree root
`/Users/rin/GitHub/shep-phase14`.)
