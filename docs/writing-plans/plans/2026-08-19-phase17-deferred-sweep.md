# Phase 17: the deferred sweep

> **For agentic workers:** nine bounded items from `docs/specs/deferred.md`, plus the `shep init` follow-ups from the 2026-08-19 teaching session. Steps use checkbox (`- [ ]`) syntax.

**Goal:** clear the small, decided debt in `deferred.md` and turn CI on, so the next phase starts from a green baseline that a machine verifies rather than one laptop.

**Architecture:** no new subsystems. Three config-semantics changes land in `shep-core`'s `normalize()`; the rest are tests, messages, packaging and CI.

## Global Constraints

- **`docs/idiomatic-rust.md`'s 45 rules** (`IR-<n>`). Every item documented with *why*. `core::error::Error`, not `std::error::Error`. `# Errors` sections. `# Panics` with `#[track_caller]`.
- **No em dashes or en dashes in anything a user reads**, including `///` comments clap renders into `--help`. A test pins the top-level help.
- **Clean-room rule, non-negotiable:** never open, read or reference `/Users/rin/GitHub/pm2`.
- **One cargo shape per task.** The workspace shares one target-dir lock, so concurrent runs block rather than parallelise. Gates each as their own command with `$?` read directly, never through a pipe.
- **`cli_e2e` keeps passing.** Where a behaviour change legitimately alters an expectation, say which and why.

## Decisions taken by Rin, 2026-08-19

1. **cwd:** a Flockfile app with no `cwd` defaults to **the Flockfile's own directory**.
2. **CI:** turn automatic CI **on**. The `workflow_dispatch`-only restriction dated from when the repo was private; it is public now, and public repos get free standard runners.
3. **`reuse_port`:** **refuse at parse time** rather than wiring it up or documenting it as inert.

## Explicitly out of scope

Held back as genuinely not small, and recorded so nobody folds them in:

- **`check_log_ancestry`'s TOCTOU** — new `unsafe` on a Linux-only path that cannot be executed from this machine, and the subject Rin has said she wants to learn rather than receive.
- **`lookout`'s char-vs-display-column widths** — needs a `unicode-width` dependency the project deliberately refused.
- **Splitting `ProcessInfo`** — architectural, and `last_exit` moved it closer only yesterday.

---

### Task 1: `~/` expands in every path a Flockfile carries

**Files:** `crates/shep-core/src/config/normalize.rs`, tests alongside.

`deferred.md`'s entry has the full decision. In short: `~/` only; refuse `~user/`; no `$VAR`. Four fields carry paths and **all four must be covered or none** — `script`, `cwd`, `out_file`, `err_file` — because expanding in one teaches that tildes work and then fails elsewhere.

`normalize()` is the seam: it already turns an `AppConfig` into a `ResolvedApp` and already refuses several shapes, and it matters that the daemon may run as a different user than the CLI, so this must resolve where config is normalised rather than where it is executed.

- [ ] Test first: each of the four fields expands `~/x`; `~user/x` is refused with a message naming why; `$HOME/x` is left alone.
- [ ] A test that enumerates the path-bearing fields, so a fifth added later fails until handled.
- [ ] Implement.
- [ ] The refusal message must name the path and say what is supported.

### Task 2: a Flockfile app's `cwd` defaults to the Flockfile's directory

**Files:** `crates/shep-cli/src/commands/lifecycle.rs`, `crates/shep-core/src/config/normalize.rs`.

Today a relative `script` with no `cwd` resolves against the **daemon's** working directory, so a committed Flockfile works on the machine where the shepherd happened to start in the right place and fails on the next one. Measured 2026-08-19 with three distinct directories; `deferred.md` carries the evidence.

- [ ] Test first, with three distinct directories so nothing can be confused for anything else: daemon cwd, Flockfile directory, and invocation directory all different. Assert the child runs in the Flockfile's directory.
- [ ] Implement. Note the ad-hoc path (`shep start ./x`) already sets cwd to the caller's directory and must keep doing so; this is the Flockfile path only.
- [ ] `shep init`'s scaffold comment for `cwd` becomes true again and should be updated in the same commit.

### Task 3: `reuse_port` is refused at parse time

**Files:** `crates/shep-core/src/config/normalize.rs`, `crates/shep-core/src/config/app.rs`.

It is accepted, stored and displayed, and no production code reads it. Refusing is honest and stops someone shipping a Flockfile that silently does nothing.

- [ ] Test first: a config setting `reuse_port` is refused with a message saying it is accepted-but-unimplemented and will return.
- [ ] Implement.
- [ ] Update `deferred.md`'s entry to record that it is now refused rather than inert.

### Task 4: turn CI on

**Files:** `.github/workflows/test.yml`, `docs/specs/deferred.md`.

- [ ] Change the trigger from `workflow_dispatch`-only to push and pull_request, keeping dispatch.
- [ ] Verify the workflow's own skip lists still name the right groups (`::slow::`, `two_concurrent_boots`).
- [ ] Update `deferred.md`'s "Automatic CI" entry and `CLAUDE.md`'s claim that the repository is private, which is now false.
- [ ] **This is the task that matters most in the phase.** The Linux-only tests in `shep-cli` have never run anywhere, and every green reported to date came from one laptop.

### Task 5: `bind_socket`'s over-length `$SHEP_HOME`

**Files:** `crates/shep-daemon/src/boot.rs`.

A `$SHEP_HOME` longer than `sun_path` surfaces as a raw `ENAMETOOLONG`. Name the cause and the limit.

- [ ] Test first, then implement.

### Task 6: `#[track_caller]` on two `# Panics` sections

**Files:** `crates/shep-daemon/src/fake.rs`.

Seven `spawn_index` accessors documented as panicking, without the attribute that makes the panic report the caller (IR-24).

- [ ] Mechanical. Add and verify.

### Task 7: the missing-node error message gets a test

**Files:** `crates/shep-cli/tests/` or the relevant unit site.

`shep start <path>.js --flockfile` with no `node` on `PATH` produces a message nothing pins.

- [ ] Add the test.

### Task 8: license files reach the published tarballs

**Files:** `crates/*/Cargo.toml`, possibly the workspace root.

`cargo package` excludes `LICENSE-MIT` and `LICENSE-APACHE`. `deferred.md` carries 263 lines of prior analysis. **Pre-publish, so this one actually blocks shipping.**

- [ ] Follow the recorded analysis; verify with `cargo package --list` for every published crate.

### Task 9: `shep init`'s `--all` follow-ups

**Files:** `crates/shep-core/src/config/app.rs`, `crates/shep-cli/src/commands/init.rs`.

From the teaching session. Two halves:

- [ ] **Make `group` and `blurb` required**, the way `example` already is, with a test that fails the build when a field lacks them. Twenty of forty fields carry no `group` today and sort last; twenty carry no `blurb` and fall back to `///` prose written for developers (`ActionOutcome::TimedOut`, `DEFAULT_DEADLINE`, spec section numbers).
- [ ] **Restore Rin's own prose** for `autorestart` and `cwd`, which unification replaced with the `///` text. Her wording was "Restarts the process automatically when it exits unexpectedly" and "Falls back to the cwd of the shep daemon if omitted" — the latter needs rewording anyway once Task 2 lands.
- [ ] Once every field has a `blurb`, the em-dash sweep Rin made across `app.rs`'s doc comments can be reverted: operator prose lives in `blurb`, so `///` goes back to being for developers. **Ask before reverting it** — it is her change.

---

## Final verification

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Plus both cross-checks in their own `CARGO_TARGET_DIR`, and — once Task 4 lands — a real CI run, which is the first time any of this will have been verified off one laptop.
