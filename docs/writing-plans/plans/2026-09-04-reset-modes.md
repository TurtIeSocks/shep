# Reset modes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `--reset` and `--reset-all` with one flag taking a required mode, `--reset={file|policy|env|all}`, adding the two modes the design was missing and fixing the reset that resets what the template is silent about.

**Architecture:** `ResetDepth` gains two variants and renames one, then the merge in `supervisor.rs` derives four arms from two axes rather than three arms from a ladder. The CLI trades two booleans for one required-value enum. The wire carries the new variants additively, on the same terms as the six precedents in shep-core's changelog.

**Tech Stack:** Rust 2024, MSRV 1.88. `clap` derive with `ValueEnum`, `serde` for the wire.

**Spec:** `docs/brainstorming/specs/2026-09-02-config-overrides-design.md`, section 3, rewritten in `181452c` on this branch. Read it before Task 2; it is the authority and this plan argues from it.

## Global Constraints

- **Clean-room rule, non-negotiable:** never open, read, or port source from any pm2 checkout on this machine.
- **Invoke the `shep-idiomatic-rust` skill before writing or reviewing any Rust.** Cite rules as `IR-<n>`.
- **No em dashes or en dashes anywhere**, including code comments and commit messages. Use a comma, colon, period or parentheses.
- **Never write a real person's name, a personal email, or any absolute home-directory path** into a committed file or a commit message. Repo-relative paths only. `/home/ada` fixtures are the crate's existing fictional convention.
- **Every new public item needs a doc comment** with `# Errors` on anything fallible, and a deliberate `Debug` decision (IR-41).
- **`#[non_exhaustive]` needs a reason comment** (IR-20).
- **`#![forbid(unsafe_code)]` is live** in shep-core and shep-cli.
- **Prove every new test non-vacuous** by mutating what it protects and watching that test go red. **Verify the mutation actually applied**: on a recent branch a patch silently failed to match because `cargo fmt` had rewrapped the line, and three green runs briefly looked like evidence a test could not fail.
- **A comment that stops being true is a defect.** This change invalidates several that name `Settings` or argue about two flags.
- **ONE cargo shape:** `cargo test --workspace --lib --bins --all-features -- --skip ::slow::` while iterating. This crosses three crates, so `-p` would need three invocations and each switch invalidates the others' feature resolution.
- **Task gate, once, at the end:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`, then `cd web && npx astro build` and `npx astro check`. Each from its own command with `$?` captured directly, never through a pipe: in zsh a pipeline's `$?` belongs to the last command.
- **Worktree:** `.claude/worktrees/reset-modes`, branch `feat/reset-modes`, cut from `0361a9f`.

---

## What is verified and what is not

Everything in this section I read at `181452c`. Line numbers drift; grep rather than trusting them.

- `ResetDepth` is at `crates/shep-core/src/config/apply.rs:137`, with variants `None`, `Settings`, `All`.
- It crosses the wire inside `Request::ApplyConfig` (`crates/shep-core/src/protocol/request.rs`), so new variants are a wire change.
- 96 mentions across six files: `supervisor.rs` 62 (mostly tests), `lifecycle.rs` 13, `rpc.rs` 4, `request.rs` 3, `config/mod.rs` 1, `apply.rs` 1.
- `crates/shep-cli/src/cli.rs:688-699` holds `pub reset: bool` (with `conflicts_with = "reset_all"`) and `pub reset_all: bool`.
- `crates/shep-cli/src/commands/lifecycle.rs:1377` maps those two booleans to a depth.
- `StartArgs.targets` is `Vec<String>` with `#[arg(num_args = 0..)]`, a greedy variadic positional.

**Three decision points in the merge, all in `crates/shep-daemon/src/supervisor.rs`.** Grep for `ResetDepth::` to find them; the line numbers below were true at `181452c`.

1. Around 2859, `in_scope` for a non-`env` key. Today `Settings | All => true`, everything else append-only.
2. Around 2905, the `env` arm. Today `Settings => {}`, `All` resets `env` and calls `establish_env`.
3. Around 6081 and 6098, whether the override record survives. Today both key on `All`.

**I have NOT worked out which of the four modes belongs on each arm, deliberately.** Point 3 is the subtle one: today's `Settings` resets a key's VALUE while keeping its entry in the override record, and those are two different things. Deriving the arms is Task 2's job, from the spec table, with a test per cell. Do not treat any mapping in this plan as given.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/shep-core/src/config/apply.rs` | `ResetDepth`, its four variants and their docs |
| `crates/shep-core/src/protocol/request.rs` | the wire spelling of those variants |
| `crates/shep-daemon/src/supervisor.rs` | the three decision points above |
| `crates/shep-cli/src/cli.rs` | one `--reset=<mode>` arg replacing two booleans |
| `crates/shep-cli/src/commands/lifecycle.rs` | argv to depth, and the refusals |
| `docs/`, `web/`, `CLAUDE.md` | the operator-facing account |

---

### Task 1: the four variants

**Files:**
- Modify: `crates/shep-core/src/config/apply.rs` (`ResetDepth`, around line 137)
- Modify: `crates/shep-core/src/protocol/request.rs` (wire spelling)
- Modify: `crates/shep-daemon/src/supervisor.rs`, `crates/shep-cli/src/commands/lifecycle.rs`, `crates/shep-daemon/src/rpc.rs` (rename call sites only)

**Interfaces:**
- Consumes: nothing.
- Produces: `ResetDepth::{None, File, Policy, Env, All}`. `Settings` is renamed to `Policy` with its behaviour untouched; `File` and `Env` are added and, in this task only, behave exactly as `Policy` and `All` respectively so the tree compiles and every existing test still passes.

Behaviour changes in Task 2. This task is a rename plus two variants that are not yet wired to anything distinct, so a reviewer can check the wire and the naming without also checking semantics.

- [ ] **Step 1: Rename and add**

`Settings` becomes `Policy` everywhere. Add `File` and `Env`. Give each variant a doc comment naming both axes it sits on, since that is what tells them apart:

```rust
    /// Put back what the template declares, and nothing else. `env` is
    /// kept, and so is any key the template never mentions: an app stocked
    /// to four instances against a file with no `instances` line keeps its
    /// count, because the file never entered that argument.
    File,
```

Write the other three in the same shape, each saying what happens to `env` and what happens to a key the template does not declare. The spec's table is the source; do not invent a fifth axis.

- [ ] **Step 2: Check the wire spelling**

`ResetDepth` is serialized inside `Request::ApplyConfig`. Find how the enum is tagged (grep `request.rs` for `ResetDepth` and read the surrounding derive) and confirm the rename changes the wire string. It does if the tag is derived from the variant name.

Record in your report whether `PROTOCOL_VERSION` should move. The precedent is six additive variants in shep-core's changelog that did not move it, with the consequence that a newer CLI against an older daemon fails on a closed connection rather than a named refusal. A RENAME is not additive in the same way, since an old daemon that knew `Settings` now receives `Policy`. Say what you concluded and why; this is the one call in the plan I want argued rather than assumed.

- [ ] **Step 3: Make the tree compile with no behaviour change**

`File` behaves as `Policy` and `Env` as `All` for now, in every match arm. Say so in a comment at each site, referencing Task 2, so nobody reads it as intended.

- [ ] **Step 4: Run the suite**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS, with no test changed. If a test needed editing beyond the rename, stop and say which: that means the rename was not behaviour-neutral.

- [ ] **Step 5: Commit**

```bash
git commit -m "refactor(core): ResetDepth gains File and Env, Settings becomes Policy"
```

---

### Task 2: the four arms

**Files:**
- Modify: `crates/shep-daemon/src/supervisor.rs`, the three decision points

**Interfaces:**
- Consumes: `ResetDepth::{None, File, Policy, Env, All}` from Task 1.
- Produces: no new signatures. This task is semantics.

This is the task where being wrong deletes an operator's tuning. Read the spec's section 3 table first.

- [ ] **Step 1: Write the failing tests, one per cell**

Eight tests, two axes by four modes. The fixture is one app whose template declares `max_restarts` and `env`, where an operator has additionally overridden `max_memory` (a key the template never declares) and set an `env` value.

For each mode, assert both axes explicitly rather than asserting a summary:

```rust
#[test]
fn file_puts_back_what_the_template_declares_and_leaves_the_rest() {
    // Both axes in one test, because the mode is defined by the pair and a
    // test that checked one would pass for two different modes.
    // `max_restarts` is declared by the template, so it goes back.
    // `max_memory` is not, so the operator's value stands.
    // `env` is kept under this mode.
}
```

Name the other seven for what they pin, in the same shape. The `instances` case gets its own, since it is the footgun the mode exists for:

```rust
#[test]
fn a_file_reset_does_not_scale_an_app_the_template_says_nothing_about() {
    // An app stocked to four against a template carrying no `instances`
    // line keeps four. Under the old single reset it dropped to one,
    // because the compiled default won an argument the file never entered.
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: FAIL. `File` and `Env` still behave as their Task 1 stand-ins.

- [ ] **Step 3: Derive the three arms**

Work out, from the spec table, what each mode does at each of the three decision points. Point 3 is the one to think hardest about: resetting a key's value and dropping its entry from the override record are different operations, and today both key on `All`. Decide which modes should do which, and argue it in a comment at the site rather than only in the commit.

If the spec's table does not determine an answer at some point, say so in your report rather than picking quietly. That is a spec gap and I will rule on it.

- [ ] **Step 4: Run the tests**

Expected: PASS, all eight plus the instances test.

- [ ] **Step 5: Prove they are not vacuous**

Collapse `File` back onto `Policy` at each decision point in turn and confirm the specific tests that distinguish them go red. Grep the file after each patch to confirm the mutation actually applied before believing a green run.

- [ ] **Step 6: Commit**

---

### Task 3: one flag, one required mode

**Files:**
- Modify: `crates/shep-cli/src/cli.rs` (around 688-699)
- Modify: `crates/shep-cli/src/commands/lifecycle.rs` (`reset_depth`, around 1377, and the refusal sites)

**Interfaces:**
- Consumes: `ResetDepth` from Task 1.
- Produces: `--reset=<mode>` on both `start` and `add`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn reset_with_no_value_is_a_usage_error_naming_every_mode() {
    // A destructive verb makes the operator name the destruction. The
    // error is the teaching surface, so it is pinned as an exact string:
    // an error that lists three of four modes is worse than none.
}

#[test]
fn a_reset_mode_does_not_swallow_the_target() {
    // `StartArgs.targets` is a greedy variadic positional. The equals form
    // is required precisely so `shep start Flockfile.toml --reset=file`
    // cannot parse the path as the mode or the mode as a target.
}
```

Add one test per mode asserting argv maps to the right `ResetDepth`, and keep whatever tests already cover the two refusals (a sheep name reads no file, a bare script path is a command line rather than a file). Those refusals apply to every mode.

- [ ] **Step 2: Run them to verify they fail**

- [ ] **Step 3: Replace the two booleans**

One field carrying an `Option<ResetMode>` derived through clap's `ValueEnum`, with `require_equals = true`. Whether `ResetMode` is a CLI-local enum mapped to `ResetDepth` or `ResetDepth` itself deriving `ValueEnum` is your call: shep-core taking a `clap` dependency for it would be the wrong trade, so check whether it already has one before deciding.

Delete `--reset-all` outright rather than aliasing it. It shipped in `0.1.31` a few hours before this branch, so the population that has typed it is approximately nobody, and an alias that silently maps to one of four modes would be worse than an error telling them to pick.

- [ ] **Step 4: Run the tests, prove them non-vacuous, commit**

Mutation for the parsing test: drop `require_equals` and confirm the greedy-positional test goes red.

---

### Task 4: the operator-facing account

**Files:**
- Modify: `web/src/pages/docs/overrides.astro` and any page the grep below finds
- Modify: `docs/` prose mentioning either old flag
- Modify: `CLAUDE.md`, the config-overrides paragraph
- Regenerate: `web/src/data/cli-reference.generated.txt`

- [ ] **Step 1: Find every mention**

```bash
grep -rn 'reset-all\|--reset' docs/ web/src/ CLAUDE.md README.md
```

Grep the word, not the phrase: a wrapped line will not match a phrase search, and a claim about these flags is never in only one place.

**Do not rewrite history.** `docs/specs/deferred-history.md`, `docs/specs/shep-v1.md`, `docs/systematic-refactor/refactor-workspace/map.md` and either crate's `CHANGELOG.md` (release-plz generates those) record what was true and stay as they are.

- [ ] **Step 2: Rewrite the prose**

`overrides.astro` carries a three-row table of the old behaviours; it becomes four. Match each page's existing voice rather than pasting one paragraph across markdown and Astro. Short beats thorough: the failure mode here is writing too much.

State the breaking change plainly where an operator reads it, including that `--reset-all` is gone and which mode replaces it.

- [ ] **Step 3: Regenerate and build**

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```
```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

The generated reference MUST change: the flags moved. `astro check` is the one CI does not run, and a page passing a component a prop it does not have builds clean and renders wrong.

- [ ] **Step 4: Commit**

---

## Self-Review

**Spec coverage.** Section 3's four modes are Tasks 1 and 2. The required-value and equals-form decisions are Task 3. The `instances` footgun the `file` mode exists for has its own test in Task 2 step 1. The breaking change is Task 4 step 2.

**Deliberately left open, for the implementer to answer and me to rule on.** Whether `PROTOCOL_VERSION` moves for a rename rather than an addition (Task 1 step 2). Which modes drop the override record versus merely reset a value (Task 2 step 3). Whether `ResetDepth` itself derives `ValueEnum` (Task 3 step 3). Each is flagged at its step rather than guessed here, because on the previous branch three defects came from this plan's author asserting things about existing code he had not read.

**Placeholder scan.** No TBDs. Test bodies in Tasks 2 and 3 are named and commented but not written out, deliberately: the fixture shape depends on `merge_declared`'s actual signature, and a plan that invented one would send an implementer to reconcile it. Every such case says what the test must pin.

**Type consistency.** `ResetDepth::{None, File, Policy, Env, All}` is the spelling in every task. `Settings` appears nowhere after Task 1.
