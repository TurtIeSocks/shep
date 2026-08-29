# Phase 10 — audit debt

Closes the surviving debt from the five v1.1 audits (`config`, `wire`,
`platform`, `tests`, `docs`) against merged `main` at `c611853`.

The maintainer's ruling, 2026-08-12: everything ships in v1 rather than being held for a
point release — *"We're not in a rush to release this to the public. We want a
hot looking app right off the bat if we have to compete with well established
apps like pm2 and other rust attempts."* So this phase runs **ahead of** the
remaining unbuilt v1.0 surface (lookout, whistle, serve, dev/runtime,
scale/signal/sendline, the KV store, `.js` Flockfiles, schemars, the
daemon-config flags layer, the `channel.*` bus topic, lambs in `describe`,
openrc and BSD rc.d) and ahead of Windows, which goes last.

The statuses this plan works from were re-derived from the code, not copied
from the audit reports. Findings marked FIXED or OBSOLETE in that re-derivation
are not tasks here and are not silently reopened.

---

## Global constraints

- MSRV 1.88, edition 2024, `MIT OR Apache-2.0`
- `#[forbid(unsafe_code)]` in shep-core, shep-client and shep-cli; unsafe only
  in shep-daemon/src/sys.rs with per-block `// SAFETY:`
- `PROTOCOL_VERSION` stays 1; wire changes are additive under
  `#[non_exhaustive]` and must keep the pinned insta fixtures passing
- the fast loop is `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`;
  shep-cli is `[[bin]]`-only so it needs `--bins`, never `--lib`, which silently
  runs nothing and reports success
- the task gate is fmt, clippy `-D warnings`, `cargo test --workspace --all-features`,
  and `RUSTDOCFLAGS="-D warnings" cargo doc`; one cargo command at a time,
  `$?` captured directly, never through a pipe
- baseline is 1030 passed / 0 failed / 3 ignored across 15 result lines
- terminology: the daemon is "the shepherd" and only that; one managed process
  is "a sheep" and the plural is always "the flock"; destructive operations and
  error text stay plain

### Reading the counts

Every task states an expected test count. Treat it as a **shape, not a
checksum** — two earlier phases shipped briefs carrying a stale figure and cost
a review loop each. What matters is the delta this task adds and that
`failed` stays `0` across all 15 result lines.

### The exact commands

One cargo command per invocation, `$?` read directly:

```bash
cargo test -p shep-core   --lib  --all-features
cargo test -p shep-daemon --lib  --all-features -- --skip ::slow::
cargo test -p shep-daemon --lib  --all-features            # when touching extras.rs / watch/ / the sampler
cargo test -p shep-client --lib  --all-features
cargo test -p shep-cli    --bins --all-features            # NOT --lib: shep-cli has no lib target
cargo test -p shep-cli    --test cli_e2e --all-features
cargo test -p shep-daemon --test daemon_e2e --all-features
```

Task gate, each from its own command:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

---

## What this phase closes

| Task | Findings closed |
|---|---|
| 1 | config #2 — `kill_signal` validated late and clamped |
| 2 | wire #1 — `ProcessInfo` has no `#[non_exhaustive]` |
| 3 | wire #2 — `ActionReply` name-only correlation; wire #3 — `SHEP_CHANNEL_VERSION` |
| 4 | wire #4 — fixture gaps (PARTIAL); wire #5 — the ninefold `Vec<ProcessInfo>` |
| 5 | platform #1 — the red linux/arm64 test; platform #3 — the never-compiled Linux branch; platform (new) — the windows-gnu gate Phase 9 dropped |
| 6 | tests #1 — the workflow is made correct; the trigger stays manual by the maintainer's standing decision, recorded in deferred.md with the cost. **Not closed.** tests #5 — the root-only privilege-drop test (the half that is a CI job) |
| 7 | config #5 — IR-20 rationale gap across six error enums |
| 8 | platform (new) — the false `ring` claim; docs (new) — README's test count, which decays every phase; config #3 — `reuse_port` dead and undocumented; platform #6 — macOS-anchored `sun_path` comment; platform #4 — the `openat2` comment's framing |
| 9 | tests #6 — fails-only-by-hanging has no checklist line; and the ledger entries for every finding this phase deliberately does not build |

### Findings deliberately NOT built, and where they are recorded instead

Each of these gets a **deferred.md entry in Task 9** so it is tracked rather
than rediscovered as new drift. None of them becomes code in Phase 10.

- **config #4 — `DaemonConfig` is not a proof token.** Marked `needs-design`
  with "user impact: nothing today". Making `DaemonConfig`'s fields private
  and splitting a `validate` step out of `::load` is an architectural call on a
  type the maintainer's own open-questions list owns (CLAUDE.md: *if a decision is listed
  there, it is the maintainer's, not yours*). Recorded, not decided.
- **wire #6 — `ProcessInfo` fuses four concerns.** The audit itself says do not
  act on it speculatively, and says the `lambs` field is what will force the
  question. Task 2 makes that field cheap to add; the split waits for it.
- **platform #4 — `check_log_ancestry`'s TOCTOU window / unused `openat2`.**
  `nix 0.29`'s `fcntl::openat2` *is* available under the `fs` feature this
  crate already enables, so the syscall is reachable without new deps — but it
  returns a `RawFd`, and turning that into a `File` needs
  `FromRawFd`, which is `unsafe` and would have to land in
  `shep-daemon/src/sys.rs` behind a Linux-only `cfg`, with an `ENOSYS`/`EPERM`
  fallback ladder for pre-5.6 kernels and seccomp sandboxes. That is
  new unsafe on a Linux-only path this project **cannot execute a test for
  locally** — the exact debt shape platform #3 exists to complain about. Task 8
  narrows the doc comment that currently reads as a blanket dismissal; the
  implementation is a scoped deferred.md entry with the design written down.
- **platform #2 — reload's Linux-only assertions.** The input's own verdict:
  "a deliberate, documented tradeoff, not an oversight … not something to
  spend a Phase 10 task on by itself." Task 6 adds the ubuntu-arm runner leg
  that would execute them, which is as far as this phase goes.
- **platform #5 — `shep startup` init detection is `target_os`-only.** Doc and
  code already match; the input names the action item as a five-second manual
  check, not a code change. This one gets **no new ledger entry**: it is
  already recorded accurately at `docs/specs/deferred.md:91-101`, inside the
  **openrc and BSD rc.d units** paragraph, which already says
  `current_init` picks the renderer by compile target with no runtime check.
  Task 9 writes nothing for it — re-recording it would be the second copy that
  drifts.
- **tests #3 — the `cli_e2e` 7-test correlation.** `needs-design`; twice
  investigated, twice exonerated. A fresh bounded measurement pass is worth
  doing, but it is a measurement, not an edit, and it belongs where its
  numbers can be recorded.
- **tests #5, the other half — the root-only privilege-drop test.** The Docker
  CI job *is* built (Task 6). What is not built is any change to
  `real_runner.rs:642` itself, which is correct as it stands — it asserts
  `geteuid().is_root()` before anything else, so it fails loudly on a
  non-root runner rather than passing vacuously. **No ledger entry**, because
  there is nothing deferred: the test is right and the job that runs it ships
  in Task 6.

- **`bind_socket` reports an over-length `$SHEP_HOME` as a raw `ENAMETOOLONG`.**
  Noticed while correcting the `sun_path` comments in Task 8 — `boot.rs`'s
  `bind_socket` performs no length check of its own, so an operator with an
  unusually deep `$SHEP_HOME` gets the OS error with no sentence naming the
  limit or the variable. Low impact and a small fix, but not this phase's
  subject; Task 8 corrects the comments and Task 9 records the gap.

---

## Task order and why

1. **Task 1 (`kill_signal`)** first: it is the only finding with ongoing user
   harm, and it touches two files nothing else in this phase touches.
2. **Task 2 (`ProcessInfo`)** second, because it is the mechanical sweep —
   26 struct literals across three crates — and every later task that adds a
   test constructing a `ProcessInfo` wants the builder to already exist.
3. **Task 3 (stamped actions)** and **Task 4 (fixtures)** are the wire pair;
   3 adds wire fields that 4's fixtures then pin, so 3 goes first.
4. **Task 5 (platform)** and **Task 6 (CI)** are the verification pair; 5
   establishes the two local cross-check commands, one of which (the Linux
   leg) 6 also encodes as a job. Task 5 must land before Task 8, because Task
   8's `ring` comment states what Task 5's windows-gnu run actually found.
5. **Tasks 7–9** are comment, doc and ledger work with no code dependencies;
   they go last so they can describe what 1–6 actually shipped.

Tasks 1–6 each end with the full task gate. Tasks 7–9 are docs/comments only
and end with `cargo fmt --all --check`, `cargo clippy … -D warnings` and
`RUSTDOCFLAGS="-D warnings" cargo doc …`; the full workspace test run is
carried by the phase gate at the end.

---

## Task 1 — `kill_signal` is rejected at normalize, not clamped at stop time

**Closes:** config.md #2 (Important, one-file).

**The harm today.** `crates/shep-daemon/src/kill.rs:90` is the *only* parser of
`app.kill_signal`, it runs at stop time, and an unrecognized name falls through
to `tracing::warn!` + `StopSignal::Term`. `normalize()` never looks at the
field. So an operator who writes `kill_signal = "SIGUSR1"` (a real signal shep
does not support) gets a clean `shep start`, and every stop and every reload
for the life of that process silently sends SIGTERM instead — discoverable only
by reading daemon logs at the moment of a stop. For an app that relies on a
specific signal for graceful shutdown, that is indefinite silent misbehaviour.

**The shape of the fix.** The grammar moves into shep-core as a small typed
enum, `normalize` rejects an unparseable name the way it already rejects an
unparseable cron pattern or watch glob, and the daemon's `stop_signal` becomes
a total map with no clamp. `AppConfig::kill_signal` stays `Option<String>` —
the same treatment `cron_restart`, `watch_options` and `ignore_watch` get, so
the Flockfile schema and the `Request::Start` wire representation are both
unchanged and no fixture moves.

### Files

- **create** `crates/shep-core/src/config/kill_signal.rs`
- **modify** `crates/shep-core/src/config/mod.rs` — `pub mod` + re-export
- **modify** `crates/shep-core/src/config/normalize.rs` — new variant, new
  check, `# Errors` line
- **modify** `crates/shep-core/src/config/app.rs` — `kill_signal`'s doc comment
- **modify** `crates/shep-daemon/src/kill.rs` — `stop_signal` rewritten
- **modify** `crates/shep-core/CHANGELOG.md`, `crates/shep-daemon/CHANGELOG.md`

### Interfaces this task produces

```rust
// crates/shep-core/src/config/kill_signal.rs
pub enum KillSignal { Term, Int, Quit, Usr2 }
impl KillSignal {
    pub const ACCEPTED: [&'static str; 4];
    pub fn parse(name: &str) -> Option<Self>;
    pub fn as_str(self) -> &'static str;
}

// crates/shep-core/src/config/normalize.rs
NormalizeError::InvalidKillSignal { name: String, value: String }
```

Consumed by: `shep-daemon/src/kill.rs::stop_signal`. Nothing else in the phase.

### Step 1.1 — RED: normalize refuses an unsupported signal name

Add to `crates/shep-core/src/config/normalize.rs`'s `#[cfg(test)] mod tests`:

```rust
/// fails if a `kill_signal` shep cannot send is accepted here. Accepting it
/// is what put SIGTERM on the wire for the life of the process with nothing
/// but one daemon log line to say so — the clamp this rejection replaces.
#[test]
fn a_kill_signal_shep_cannot_send_is_refused_by_name() {
    let mut app = AppConfig::minimal("web", "./srv");
    app.kill_signal = Some("SIGUSR1".to_string());

    let err = normalize(app).unwrap_err();

    assert_eq!(
        err,
        NormalizeError::InvalidKillSignal {
            name: "web".to_string(),
            value: "SIGUSR1".to_string(),
        }
    );
    // The message has to name the accepted set, because the operator's next
    // move is picking a different word and there is nowhere else to look.
    let rendered = err.to_string();
    assert!(rendered.contains("SIGUSR1"), "{rendered}");
    assert!(rendered.contains("SIGTERM"), "{rendered}");
    assert!(rendered.contains("SIGUSR2"), "{rendered}");
}

/// fails if the four supported names, their bare forms, or a lowercase
/// spelling stop being accepted. This is the compatibility half: every
/// spelling `stop_signal` accepted before this task must still normalize.
#[test]
fn every_spelling_the_daemon_already_accepted_still_normalizes() {
    for name in [
        "SIGTERM", "TERM", "sigterm", "term", "SIGINT", "INT", "SIGQUIT",
        "QUIT", "SIGUSR2", "USR2", "sigusr2",
    ] {
        let mut app = AppConfig::minimal("web", "./srv");
        app.kill_signal = Some(name.to_string());
        assert!(
            normalize(app).is_ok(),
            "`{name}` was accepted before this task and must still be"
        );
    }
}

/// fails if an unset `kill_signal` is refused — the overwhelmingly common
/// case, and the one a validation pass is most likely to break by treating
/// `None` as an empty string.
#[test]
fn an_unset_kill_signal_is_not_a_config_error() {
    let app = AppConfig::minimal("web", "./srv");
    assert!(app.kill_signal.is_none());
    assert!(normalize(app).is_ok());
}
```

Run:

```bash
cargo test -p shep-core --lib --all-features
```

**Expected failure — for the stated reason:** compile error,
`no variant or associated item named `InvalidKillSignal` found for enum
`NormalizeError``. Not a runtime assertion failure. If it compiles and the
first test fails on `unwrap_err` panicking with `Ok`, that is also the right
red — it means the variant exists but the check does not.

### Step 1.2 — GREEN: the grammar, in shep-core

Create `crates/shep-core/src/config/kill_signal.rs`:

```rust
//! The stop-signal grammar: the four signals `kill_signal` may name.
//!
//! Lives in shep-core rather than beside the kill ladder that sends them
//! because two layers need the same answer and only one of them can reach the
//! OS: `normalize` has to REFUSE a name the daemon could not send, and the
//! daemon has to MAP an accepted name onto its own portable `StopSignal`.
//! Splitting that grammar across the two crates is how the clamp got in — the
//! daemon knew four names, the validator knew none, and the gap between them
//! was a `tracing::warn!` nobody reads in a detached process.

/// A signal `kill_signal` may name.
///
/// Four, not every signal on the platform, and deliberately: each one here is
/// a signal the daemon's stop ladder can actually deliver and then escalate
/// past. Growth is possible but is not anticipated — the ladder's shape, not
/// the grammar's, is what would have to change first — so this is left
/// exhaustive rather than `#[non_exhaustive]` (IR-20: don't cargo-cult it).
/// A caller matching on all four today gets a compile error the day a fifth
/// arrives, which is the outcome we want at both call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillSignal {
    /// `SIGTERM` — the default, and a graceful stop request.
    Term,
    /// `SIGINT` — interrupt, what Ctrl-C sends.
    Int,
    /// `SIGQUIT` — quit, core-dumping by default.
    Quit,
    /// `SIGUSR2` — user-defined signal 2, the one several runtimes reserve
    /// for a graceful restart.
    Usr2,
}

impl KillSignal {
    /// Every spelling this grammar accepts, canonical form, in the order an
    /// error message lists them.
    ///
    /// Public because [`NormalizeError::InvalidKillSignal`](crate::config::NormalizeError)
    /// renders it into the refusal, and a caller building its own diagnostic
    /// (a `--help` line, an editor completion) wants the same list rather
    /// than a second copy that can drift.
    pub const ACCEPTED: [&'static str; 4] = ["SIGTERM", "SIGINT", "SIGQUIT", "SIGUSR2"];

    /// Parses one `kill_signal` name, case-insensitively, with or without the
    /// `SIG` prefix. `None` for anything else.
    ///
    /// Both spellings are accepted because both were accepted before this
    /// grammar existed, and a validation pass that starts refusing a
    /// Flockfile that worked yesterday is a worse bug than the one it fixes.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "SIGTERM" | "TERM" => Some(Self::Term),
            "SIGINT" | "INT" => Some(Self::Int),
            "SIGQUIT" | "QUIT" => Some(Self::Quit),
            "SIGUSR2" | "USR2" => Some(Self::Usr2),
            _ => None,
        }
    }

    /// The canonical name, always `SIG`-prefixed and uppercase.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Usr2 => "SIGUSR2",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `ACCEPTED` and `as_str` disagree — the list an operator is
    /// shown in a refusal has to be the list `parse` actually takes, and the
    /// two are written out separately.
    #[test]
    fn every_accepted_name_round_trips_through_parse() {
        for name in KillSignal::ACCEPTED {
            let parsed = KillSignal::parse(name)
                .unwrap_or_else(|| panic!("`{name}` is advertised but not parsed"));
            assert_eq!(parsed.as_str(), name);
        }
    }

    /// fails if the bare form or a lowercase spelling stops parsing.
    #[test]
    fn the_prefix_and_the_case_are_both_optional() {
        assert_eq!(KillSignal::parse("usr2"), Some(KillSignal::Usr2));
        assert_eq!(KillSignal::parse("SigQuit"), Some(KillSignal::Quit));
    }

    /// fails if a real signal shep cannot deliver is waved through. `SIGUSR1`
    /// is the exact name that motivated this module: a plausible typo for
    /// `SIGUSR2`, and one that used to become SIGTERM in silence.
    #[test]
    fn a_signal_the_ladder_cannot_send_does_not_parse() {
        assert_eq!(KillSignal::parse("SIGUSR1"), None);
        assert_eq!(KillSignal::parse("SIGKILL"), None);
        assert_eq!(KillSignal::parse(""), None);
    }
}
```

`SIGKILL` is refused on purpose: it is the ladder's own escalation rung, and
naming it as the *graceful* signal would ask the daemon to skip the grace
period it was configured to honour. Say that in the review, not in a code
comment — the module doc already carries the "signals the ladder can escalate
past" framing.

Wire it into `crates/shep-core/src/config/mod.rs`:

```rust
pub mod kill_signal;
...
pub use kill_signal::KillSignal;
```

### Step 1.3 — GREEN: normalize rejects

In `crates/shep-core/src/config/normalize.rs`, add to the `use` list:

```rust
use crate::config::{AppConfig, CronParseError, CronSchedule, KillSignal, ProbeConfig, ProbeTarget};
```

Add the check. Place it immediately after the `max_memory` zero check and
before the `action_timeout` ceiling, so the ordering reads
identity → schedule → probes → resources → stop behaviour → watch:

```rust
    if let Some(name) = &app.kill_signal
        && KillSignal::parse(name).is_none()
    {
        // Rejected rather than clamped, and this one is the sharpest case of
        // that trade in the file. The daemon's stop ladder used to fall back
        // to SIGTERM and log a warning, which meant a typo cost the operator
        // every stop and every reload for the life of the process, with the
        // only evidence in a detached daemon's log at the moment of a stop.
        // `max_cron_sleep` and `MIN_LIVENESS_INTERVAL` reject for the same
        // reason at lower stakes: the user's file is the only place a
        // silently-substituted value could ever be noticed.
        return Err(NormalizeError::InvalidKillSignal {
            name: app.name,
            value: name.clone(),
        });
    }
```

The variant, placed after `ActionTimeoutTooLong`:

```rust
    /// `kill_signal` names a signal the daemon's stop ladder cannot send.
    /// Carries the app name and the value as written.
    InvalidKillSignal {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// The value as the user wrote it.
        value: String,
    },
```

Its `Display` arm, in the same match:

```rust
            Self::InvalidKillSignal { name, value } => {
                write!(
                    f,
                    "`{name}`: kill_signal `{value}` is not one shep can send (accepted: {})",
                    KillSignal::ACCEPTED.join(", ")
                )
            }
```

And the `# Errors` line on `normalize`, after the `ActionTimeoutTooLong` entry:

```rust
/// - [`NormalizeError::InvalidKillSignal`] — `kill_signal` names a signal the
///   daemon's stop ladder cannot send (carries the app name and the value).
```

### Step 1.4 — GREEN: the daemon maps, and no longer clamps

Replace `stop_signal` in `crates/shep-daemon/src/kill.rs` (currently lines
84–107) with:

```rust
/// Maps `app.kill_signal` onto a [`StopSignal`]; unset defaults to `SIGTERM`.
///
/// Total over [`KillSignal`], with no fallback branch, because the grammar
/// and the rejection both live in shep-core now: a config that reached the
/// daemon at all came through `normalize`, which refuses any name this cannot
/// map. The `_ => Term` clamp this replaced is the whole of config.md #2 — it
/// turned a typo into SIGTERM for the life of the process and said so only in
/// a log line no detached daemon has a reader for.
///
/// The one branch that is still defensive is the unparseable name below. It
/// is unreachable through `normalize`, and it is an `error!` rather than a
/// `warn!` for that reason: reaching it means a config bypassed validation,
/// which is a bug in the daemon's own wiring and not something an operator
/// can fix by editing a file.
fn stop_signal(app: &AppConfig) -> StopSignal {
    let Some(name) = app.kill_signal.as_deref() else {
        return StopSignal::Term;
    };
    let Some(signal) = KillSignal::parse(name) else {
        tracing::error!(
            kill_signal = name,
            "kill_signal reached the stop ladder unvalidated; normalize should have refused it. \
             Falling back to SIGTERM"
        );
        return StopSignal::Term;
    };
    match signal {
        KillSignal::Term => StopSignal::Term,
        KillSignal::Int => StopSignal::Int,
        KillSignal::Quit => StopSignal::Quit,
        KillSignal::Usr2 => StopSignal::Usr2,
    }
}
```

Import: `use shep_core::config::{AppConfig, KillSignal};`.

Run:

```bash
cargo test -p shep-core --lib --all-features
```

Expect green, `+6` tests over baseline for this crate (3 in `normalize.rs`, 3
in `kill_signal.rs`).

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

Expect green. `kill.rs`'s existing tests already cover each signal; check
whether any of them asserts on the *clamp* (a test feeding a bad name and
asserting `Term`). If one does, it is now asserting a branch reachable only
through a wiring bug — rewrite it to assert the `error!` line is emitted using
`testing::capture_logs`, or delete it and say which in the commit message.
Do not leave it asserting the old behaviour as if it were the contract.

### Step 1.5 — GREEN: the field's own doc

`crates/shep-core/src/config/app.rs:100` currently reads:

```rust
    /// Stop signal (default SIGTERM; parsed daemon-side into StopSignal)
```

Replace with:

```rust
    /// Stop signal, one of `SIGTERM`/`SIGINT`/`SIGQUIT`/`SIGUSR2` (the `SIG`
    /// prefix and the case are both optional). Unset means `SIGTERM`.
    ///
    /// A `String` rather than a [`KillSignal`](crate::config::KillSignal) so
    /// the Flockfile schema and this struct's wire form stay plain text;
    /// `normalize` is what refuses a name outside that set, the same split
    /// `cron_restart` and the watch globs already use.
```

### Step 1.6 — MUTATION

Break exactly this line in `crates/shep-core/src/config/normalize.rs`:

```rust
        && KillSignal::parse(name).is_none()
```

change to

```rust
        && false
```

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `a_kill_signal_shep_cannot_send_is_refused_by_name` fails at
`normalize(app).unwrap_err()` with "called `Result::unwrap_err()` on an `Ok`
value". `every_spelling_the_daemon_already_accepted_still_normalizes` and
`an_unset_kill_signal_is_not_a_config_error` must stay GREEN — a mutation that
reddens all three means the tests are only measuring "normalize returns Err",
not the grammar.

Second mutation, in `crates/shep-daemon/src/kill.rs`: change
`KillSignal::Usr2 => StopSignal::Usr2` to `KillSignal::Usr2 => StopSignal::Term`.
Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`. The
existing `kill.rs` signal test must go red. If it does not, that test never
distinguished the signals and this task owes one that does.

### Step 1.7 — CHANGELOGs and gate

`crates/shep-core/CHANGELOG.md`, under `[Unreleased]`:

- `Additions`: "`config::KillSignal`, the four-signal grammar `kill_signal`
  accepts."
- `Fixes`: "`normalize` refuses a `kill_signal` shep cannot send instead of
  leaving the daemon to substitute SIGTERM at every stop."

`crates/shep-daemon/CHANGELOG.md`, under `Fixes`: "The stop ladder no longer
clamps an unrecognized `kill_signal` to SIGTERM — `normalize` refuses it first."

Then the full task gate.

---

## Task 2 — `ProcessInfo` becomes `#[non_exhaustive]`, with a builder

**Closes:** wire.md #1 (Important, one-file + a wide mechanical sweep).

**Why now.** `ProcessInfo` is the one wire type that has actually grown a field
in three separate phases, and it is the only wire type without the attribute —
Phase 9's brand-new `DogSource` got `#[non_exhaustive]` on sight, at
`request.rs:172-175`, which is what makes this an oversight rather than an
evolving norm. There are 32 struct literals of it in the tree, 26 of them
outside shep-core, and Phase 9 added several. `lambs` is named in
`deferred.md` as the next field, so the next sweep is scheduled, not
hypothetical.

**Counting them is itself a trap, and the first draft of this plan fell in
it.** A bare `grep "ProcessInfo {"` matches `fn f(..) -> ProcessInfo {` as
readily as a literal, and matched `struct V1ProcessInfo {` too — 59 hits, of
which 32 are literals. The filtered form is below and the table under it is
labelled an estimate on purpose: the compiler's own E0639 list is the
authority, and Step 2.1 generates it before a single site is touched.

**What `#[non_exhaustive]` costs, precisely.** Outside shep-core it blocks two
things and nothing else:

1. the struct literal `ProcessInfo { .. }`, including the functional-update
   form `ProcessInfo { out_file: .., ..base }`;
2. an exhaustive struct *pattern* — `let ProcessInfo { id, name, status, … }`
   with no `..`.

It does **not** block field access or field assignment. Fields stay `pub`, so
`let mut info = base.clone(); info.out_file = Some(p);` keeps working
unchanged, and that is the mechanical replacement for every `..base` site.

### Files

- **modify** `crates/shep-core/src/protocol/request.rs` — attribute, builder,
  builder tests
- **modify** `crates/shep-core/CHANGELOG.md`
- **sweep** every out-of-crate construction site:

**An estimate, measured 2026-08-13 at `b7c466b`, not the worklist.** The
worklist is Step 2.1's E0639 output. This table exists so an implementer can
tell "the sweep is nearly done" from "the sweep has barely started", and so a
file that produces no error is recognised as expected rather than as a missed
one.

| File | Literals |
|---|---|
| `crates/shep-cli/src/commands/bleats.rs` | 13 |
| `crates/shep-cli/src/output/rows.rs` | 2 |
| `crates/shep-cli/src/output/table.rs` | 1 |
| `crates/shep-cli/src/dog/bark/mod.rs` | 1 |
| `crates/shep-cli/src/dog/bark/rules.rs` | 1 |
| `crates/shep-cli/src/dog/metrics/mod.rs` | 1 |
| `crates/shep-cli/src/dog/metrics/exposition.rs` | 1 |
| `crates/shep-daemon/src/supervisor.rs` | 1 (`to_info`, `:4105`) |
| `crates/shep-daemon/src/snapshot.rs` | 1 |
| `crates/shep-daemon/src/server.rs` | 1 |
| `crates/shep-daemon/src/dogs.rs` | 1 |
| `crates/shep-daemon/src/bus.rs` | 1 |
| `crates/shep-client/src/testing.rs` | 1 |

26 out-of-crate literals. Files an earlier count wrongly listed, and which
must produce **no** E0639 at all: `crates/shep-daemon/tests/daemon_e2e.rs`
(both hits are `-> ProcessInfo {` signatures), `crates/shep-daemon/src/watch/mod.rs`,
`rpc.rs`, `extras.rs`, `cron.rs`, and `crates/shep-cli/src/output/mod.rs`
(zero literals each). If the compiler names one of those, read the site before
editing it — it means something changed since this table was measured.

In-crate sites (`request.rs` 3, `events.rs` 2, `frame.rs` 1) need no change —
the literal is still legal inside shep-core, and `sample_info()` in
`request.rs:563` should stay a literal so the builder cannot mask a field the
struct grew.

The authoritative list is regenerated, not trusted. Both `grep -v` filters are
load-bearing — the first drops the struct's own declaration, the second drops
every `-> ProcessInfo {` return-type signature, which is what inflated the
first draft's count from 32 to 59:

```bash
grep -rn "ProcessInfo {" --include="*.rs" . \
  | grep -v "pub struct ProcessInfo {" \
  | grep -v -- "-> ProcessInfo {"
```

Even that leaves two false positives to eyeball rather than edit:
`request.rs:923`'s `struct V1ProcessInfo {` (a different type, in the v1
deserialization test) and `table.rs:315`'s
`fn info_with_name(..) -> shep_core::protocol::ProcessInfo {`, whose
fully-qualified return type slips past the second filter.

### Interfaces this task produces

```rust
// crates/shep-core/src/protocol/request.rs
impl ProcessInfo {
    pub fn builder(id: u32, name: impl Into<String>, status: ProcStatus) -> ProcessInfoBuilder;
}

pub struct ProcessInfoBuilder { /* private */ }

impl ProcessInfoBuilder {
    pub fn pid(self, pid: Option<u32>) -> Self;
    pub fn restarts(self, restarts: u32) -> Self;
    pub fn uptime_ms(self, uptime_ms: u64) -> Self;
    pub fn fold(self, fold: Option<String>) -> Self;
    pub fn out_file(self, out_file: Option<String>) -> Self;
    pub fn err_file(self, err_file: Option<String>) -> Self;
    pub fn cpu_percent(self, cpu_percent: Option<f32>) -> Self;
    pub fn memory_bytes(self, memory_bytes: Option<u64>) -> Self;
    pub fn dog(self, dog: Option<DogSource>) -> Self;
    pub fn build(self) -> ProcessInfo;
}
```

Re-exported from `crates/shep-core/src/protocol/mod.rs` alongside
`ProcessInfo`. Consumed by Tasks 4, 5 and 6 wherever they build a row, and by
every crate in the workspace.

### Step 2.1 — RED: the attribute, before the builder

Add the attribute first and let the compiler produce the failing list. In
`crates/shep-core/src/protocol/request.rs`, above `pub struct ProcessInfo`,
after the existing block comment:

```rust
/// `#[non_exhaustive]`: this struct has grown a field in three separate
/// phases (`out_file`/`err_file`, then `cpu_percent`/`memory_bytes`, then
/// `dog`), and `deferred.md` already names the next one — `lambs`, for
/// `describe`'s tree view. Each of those additions was a hand-edit sweep
/// across every construction site in the workspace because any crate could
/// write the literal. Under the attribute the compiler names only the sites
/// that must decide something, and an out-of-tree consumer cannot be broken
/// by an addition at all (IR-20 — growth here is not anticipated, it is
/// scheduled). Use [`ProcessInfo::builder`] to construct one; the fields stay
/// `pub`, so reading them and assigning to them are both unchanged.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
```

Run:

```bash
cargo check --workspace --all-targets --all-features
```

**Expected failure — for the stated reason:** `E0639`, *"cannot create
non-exhaustive struct using struct expression"*, once per out-of-crate site.
That error list **is** the sweep's worklist. Save it:

```bash
cargo check --workspace --all-targets --all-features 2>&1 \
  | grep -E "^error\[E063[19]\]|^  --> " > /tmp/e0639.txt
wc -l /tmp/e0639.txt
```

Expect roughly 52 lines — 26 error headers and 26 `-->` locations, matching
the estimate table above. A count far below that means the check stopped at
the first crate rather than reporting the workspace; a count far above it
means the table is stale and the table is what is wrong, not the compiler.

If any site instead reports `E0638` (*"non-exhaustive structs … cannot be
matched against without a wildcard"*), that is an exhaustive pattern; fix it by
adding `..` to the pattern rather than by reaching for the builder.

### Step 2.2 — GREEN: the builder

Immediately after the `ProcessInfo` struct in `request.rs`:

```rust
impl ProcessInfo {
    /// Starts a builder for one sheep's row.
    ///
    /// The three required arguments are the three fields no row can omit and
    /// no reader can default: which sheep this is, what it is called, and
    /// what state it is in. Everything else is optional, derived, or
    /// meaningfully absent, which is exactly the shape a builder is for —
    /// a nine-argument `new` would put `Option<String>, Option<String>,
    /// Option<f32>, Option<u64>` next to each other at every call site and
    /// invite a silent transposition the type system could not catch.
    #[must_use]
    pub fn builder(id: u32, name: impl Into<String>, status: ProcStatus) -> ProcessInfoBuilder {
        ProcessInfoBuilder {
            info: Self {
                id,
                name: name.into(),
                status,
                pid: None,
                restarts: 0,
                uptime_ms: 0,
                fold: None,
                out_file: None,
                err_file: None,
                cpu_percent: None,
                memory_bytes: None,
                dog: None,
            },
        }
    }
}

/// Builds a [`ProcessInfo`], which is `#[non_exhaustive]` and so cannot be
/// written as a struct literal outside this crate.
///
/// Every setter takes the field's own type, `Option` included, rather than
/// the unwrapped value. That is deliberate and it is the difference between a
/// straight port and a rewrite: the daemon already holds `Option<u32>` for a
/// pid and `Option<f32>` for a CPU reading, so `.pid(entry.pid())` carries
/// across unchanged where `.pid(u32)` would put an `if let` ladder at every
/// call site. A setter is skipped, not passed `None`, when a row genuinely
/// has nothing to say about that field.
///
/// Defaults for the skipped fields are the ones a not-yet-running sheep has:
/// no pid, no uptime, no restarts, no resource reading, not a dog.
#[derive(Debug, Clone)]
#[must_use = "a builder that is never `build`-ed produces no ProcessInfo"]
pub struct ProcessInfoBuilder {
    info: ProcessInfo,
}

impl ProcessInfoBuilder {
    /// Sets the OS pid; `None` while the sheep is not running.
    pub fn pid(mut self, pid: Option<u32>) -> Self {
        self.info.pid = pid;
        self
    }

    /// Sets the restart count since registration.
    pub fn restarts(mut self, restarts: u32) -> Self {
        self.info.restarts = restarts;
        self
    }

    /// Sets milliseconds since the last successful start.
    pub fn uptime_ms(mut self, uptime_ms: u64) -> Self {
        self.info.uptime_ms = uptime_ms;
        self
    }

    /// Sets fold membership.
    pub fn fold(mut self, fold: Option<String>) -> Self {
        self.info.fold = fold;
        self
    }

    /// Sets the resolved stdout log path.
    pub fn out_file(mut self, out_file: Option<String>) -> Self {
        self.info.out_file = out_file;
        self
    }

    /// Sets the resolved stderr log path.
    pub fn err_file(mut self, err_file: Option<String>) -> Self {
        self.info.err_file = err_file;
        self
    }

    /// Sets tree CPU as a percentage of one core.
    pub fn cpu_percent(mut self, cpu_percent: Option<f32>) -> Self {
        self.info.cpu_percent = cpu_percent;
        self
    }

    /// Sets tree resident set size in bytes.
    pub fn memory_bytes(mut self, memory_bytes: Option<u64>) -> Self {
        self.info.memory_bytes = memory_bytes;
        self
    }

    /// Marks this row a dog and names where the dog came from.
    pub fn dog(mut self, dog: Option<DogSource>) -> Self {
        self.info.dog = dog;
        self
    }

    /// Finishes the row.
    #[must_use]
    pub fn build(self) -> ProcessInfo {
        self.info
    }
}
```

Every setter needs `#[must_use]`? No — they return `Self` and the struct
carries `#[must_use]`, which propagates to any expression of that type. `build`
gets its own because it returns a `ProcessInfo`.

Export it. `crates/shep-core/src/protocol/mod.rs:15-18` currently reads:

```rust
pub use request::{
    ActionOutcome, ActionReply, DogSectionToml, DogSource, Envelope, Hello, HelloAck, HelloReply,
    ProcessInfo, Reply, Request, Response, RpcError, RpcErrorCode, SelectorSpec,
};
```

Add `ProcessInfoBuilder` immediately after `ProcessInfo` — the list is
alphabetical and `rustfmt` will rewrap it:

```rust
pub use request::{
    ActionOutcome, ActionReply, DogSectionToml, DogSource, Envelope, Hello, HelloAck, HelloReply,
    ProcessInfo, ProcessInfoBuilder, Reply, Request, Response, RpcError, RpcErrorCode, SelectorSpec,
};
```

Do **not** add it to `lib.rs`'s `prelude` (`lib.rs:31-42`): the prelude is the
one-import surface for downstream crates and carries five config/value types,
none of them protocol. A crate that builds a `ProcessInfo` already imports
`protocol::ProcessInfo` and gets the builder from the same path.

### Step 2.3 — GREEN: builder tests

In `request.rs`'s test module:

```rust
/// fails if the builder's defaults drift from what a registered-but-not-yet
/// running sheep actually looks like. A builder that quietly defaulted
/// `uptime_ms` to something non-zero, or `restarts` to 1, would put a wrong
/// number in front of an operator with nothing to compare it against.
#[test]
fn a_builder_with_nothing_set_is_a_sheep_that_has_not_run() {
    let info = ProcessInfo::builder(3, "web", ProcStatus::Stopped).build();

    assert_eq!(info.id, 3);
    assert_eq!(info.name, "web");
    assert_eq!(info.status, ProcStatus::Stopped);
    assert_eq!(info.pid, None);
    assert_eq!(info.restarts, 0);
    assert_eq!(info.uptime_ms, 0);
    assert_eq!(info.fold, None);
    assert_eq!(info.out_file, None);
    assert_eq!(info.err_file, None);
    assert_eq!(info.cpu_percent, None);
    assert_eq!(info.memory_bytes, None);
    assert_eq!(info.dog, None);
}

/// fails if any setter writes a field other than its own — the failure a
/// twelve-field builder is most likely to ship, and one no individual
/// round-trip test would catch. Every field is given a value distinct from
/// every other field's default, so a copy-pasted setter body shows up as a
/// mismatch rather than as a coincidence.
#[test]
fn every_setter_writes_its_own_field_and_no_other() {
    let built = ProcessInfo::builder(3, "web", ProcStatus::Online)
        .pid(Some(4242))
        .restarts(1)
        .uptime_ms(60_000)
        .fold(Some("backend".to_string()))
        .out_file(Some("/home/ada/.shep/logs/web-0-out.log".to_string()))
        .err_file(Some("/home/ada/.shep/logs/web-0-err.log".to_string()))
        .cpu_percent(Some(12.5))
        .memory_bytes(Some(48 * 1024 * 1024))
        .dog(None)
        .build();

    // `sample_info()` is still a struct literal, on purpose: it is the one
    // place in the workspace that names every field by hand, so this
    // comparison fails the day the struct grows a field the builder cannot
    // set. That is the point of comparing against it rather than against
    // another builder call.
    assert_eq!(built, sample_info());

    // `dog` is the one field the comparison above cannot speak for, and it
    // is the field the whole dogs subsystem reads. `sample_info()`'s `dog`
    // is `None`, which is also the builder's default, so a `dog` setter with
    // an EMPTY BODY passes the assert_eq! above and passes it for the wrong
    // reason. `sample_info()` cannot be changed to `Some(..)` to fix that —
    // it feeds `reply_wire_snapshots` and `bus_event_wire_snapshots`, so
    // altering it moves pinned bytes. So the field gets its own line, with a
    // value nothing defaults to.
    assert_eq!(
        ProcessInfo::builder(1, "metrics", ProcStatus::Online)
            .dog(Some(DogSource::BuiltIn))
            .build()
            .dog,
        Some(DogSource::BuiltIn),
        "an empty `dog` setter body is invisible to the comparison above"
    );
}
```

`DogSource` is already in scope in this module (`use super::*;`), so no new
import is needed. If it is not, import it rather than reaching for a different
value — `BuiltIn` is the variant with no fields and it is what a real dog row
carries.

Run:

```bash
cargo test -p shep-core --lib --all-features
```

Expect green, `+2`.

### Step 2.4 — GREEN: the sweep

Two mechanical shapes, applied per site from `/tmp/e0639.txt`.

**Shape A — a full literal.** `crates/shep-client/src/testing.rs:112`:

```rust
pub fn sample_info() -> ProcessInfo {
    ProcessInfo::builder(1, "web", ProcStatus::Online)
        .pid(Some(4242))
        .restarts(3)
        .uptime_ms(60_000)
        .fold(Some("backend".to_string()))
        .out_file(Some("/home/ada/.shep/logs/web-0-out.log".to_string()))
        .err_file(Some("/home/ada/.shep/logs/web-0-err.log".to_string()))
        .cpu_percent(Some(12.5))
        .memory_bytes(Some(48 * 1024 * 1024))
        .dog(Some(DogSource::BuiltIn))
        .build()
}
```

**Shape B — a functional update.** `crates/shep-cli/src/commands/bleats.rs:1333`
and its eight siblings:

```rust
// before
let sheep = ProcessInfo {
    out_file: Some(out_path),
    ..info(1, "web")
};

// after
let mut sheep = info(1, "web");
sheep.out_file = Some(out_path);
```

The fields are `pub`, so this compiles from any crate. Do **not** add a
`with_out_file`-style setter to `ProcessInfo` itself to make Shape B prettier —
that is a second construction API for a struct that now has one.

Sweep in this order, running the owning crate's tests after each crate rather
than after each file:

```bash
cargo test -p shep-client --lib --all-features
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
cargo test -p shep-cli --bins --all-features
cargo test -p shep-daemon --test daemon_e2e --all-features
```

`crates/shep-daemon/src/supervisor.rs` has exactly **one** site, `to_info` at
`:4105`, and it is the one that matters for review: it is the daemon's real
snapshot path, every field there comes from a `SheepSlot`, and every row an
operator ever sees in `shep flock` or `shep describe` comes out of it. Port it
field-for-field; a setter left off is a column silently blanked for every
sheep at once. Everything else in the sweep is a test fixture.

### Step 2.5 — Verify the wire did not move

```bash
cargo test -p shep-core --lib --all-features
```

`request_wire_snapshots`, `reply_wire_snapshots`, `bus_event_wire_snapshots`
and `v1_fixture_still_deserializes` must all pass **with no snapshot
re-acceptance**. `#[non_exhaustive]` is a Rust-visibility attribute; it changes
no serialized byte. If insta reports a pending snapshot here, something else in
the sweep changed a value — find it, do not accept it.

```bash
find crates/shep-core/src/protocol/snapshots -name '*.snap.new' | wc -l
```

must print `0`. Written as `find`, not `ls <glob>`: under zsh's default
`nomatch`, the success case of `ls .../*.snap.new` is a shell error
("no matches found") on stderr with a non-zero status, so the check reads as
broken exactly when it passes. `find` prints the count either way and `0` is
unambiguous.

### Step 2.6 — MUTATION

Break this line in `ProcessInfoBuilder`:

```rust
    pub fn memory_bytes(mut self, memory_bytes: Option<u64>) -> Self {
        self.info.memory_bytes = memory_bytes;
```

change the assignment to `self.info.uptime_ms = memory_bytes.unwrap_or(0);`.

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `every_setter_writes_its_own_field_and_no_other` fails on the
`assert_eq!(built, sample_info())`. If it stays green, the test is comparing
two things that were both built the wrong way, and the `sample_info()` literal
anchor has been lost.

Second mutation, on the field the comparison cannot speak for: blank the
`dog` setter's body —

```rust
    pub fn dog(mut self, dog: Option<DogSource>) -> Self {
        // self.info.dog = dog;
        self
    }
```

— and run `cargo test -p shep-core --lib --all-features`.
`every_setter_writes_its_own_field_and_no_other` **must go red on its second
assertion**, the `dog(Some(DogSource::BuiltIn))` line, and its
`assert_eq!(built, sample_info())` must still pass. That split is the whole
point: an empty `dog` body is invisible to the fixture comparison, because
`sample_info().dog` and the builder's default are both `None`. If the test
goes red on the first assertion instead, the mutation was applied to the
wrong setter. If it stays green entirely, Step 2.3's second assertion was not
written.

Third mutation: delete `#[non_exhaustive]` from `ProcessInfo` and run
`cargo check --workspace --all-targets --all-features`. It must stay GREEN.
That is the correct result and it is not a defect in the test suite — it is a
property of the attribute. **No test in this repository guards
`#[non_exhaustive]` on `ProcessInfo`, and this task does not add one.** The
attribute has no observable effect inside the defining crate, so no
`#[cfg(test)]` module can see it, and the only thing that would catch its
removal is a compile-fail harness — `trybuild` as a dev-dependency, with a
`.rs`/`.stderr` pair asserting E0639. That is a new dependency and a new test
tier for a single attribute, and it is not worth it here. Say so in the
commit message rather than implying a guard exists.

What this task DOES add is the positive half — proof, from outside the crate,
that the builder is a complete replacement for the literal it just outlawed.
That is a real property and it is worth a file, but the file must be named and
documented for what it proves:

**create** `crates/shep-core/tests/process_info_builder_from_outside_the_crate.rs`:

```rust
//! Proves [`ProcessInfo::builder`] reaches every field from outside
//! shep-core, and that field assignment still works across the boundary.
//!
//! It does **not** prove `ProcessInfo` is `#[non_exhaustive]`, and the
//! filename deliberately does not claim otherwise. That attribute is
//! invisible inside the defining crate and this file's `assert`s run in a
//! separate crate but still only observe what compiles here — nothing in the
//! repository guards the attribute itself. The only thing that would is a
//! `trybuild` compile-fail pair asserting E0639, which Phase 10 declined as
//! a whole new test tier for one attribute.
//!
//! This file is a deliberate **exception** to IR-38, not an application of
//! it. IR-38 reads: "`tests/` dir = at most one compile-only file per crate
//! proving an external crate can implement the public trait (`todo!()`
//! bodies fine). Everything behavioral is co-located `#[cfg(test)]`." This
//! file has assertions and is therefore behavioral, so IR-38 does not permit
//! it. It earns the exception on the same grounds IR-38's own carve-out
//! rests on — the property needs a real crate boundary to observe, and
//! shep-core's `#[cfg(test)]` modules are inside the boundary. It is
//! shep-core's one `tests/` file and must stay the only one.

// `ProcStatus` lives at `shep_core::status` and is re-exported through the
// prelude, NOT through `protocol` — `protocol/mod.rs`'s `pub use` list does
// not name it. Two imports, deliberately, rather than one wrong one.
use shep_core::prelude::ProcStatus;
use shep_core::protocol::{DogSource, ProcessInfo};

#[test]
fn the_builder_reaches_every_field_from_outside_the_crate() {
    let info = ProcessInfo::builder(1, "web", ProcStatus::Online)
        .pid(Some(1))
        .restarts(1)
        .uptime_ms(1)
        .fold(Some("f".to_string()))
        .out_file(Some("o".to_string()))
        .err_file(Some("e".to_string()))
        .cpu_percent(Some(1.0))
        .memory_bytes(Some(1))
        .dog(Some(DogSource::BuiltIn))
        .build();

    // Every field, read back across the boundary. `dog` is set to a real
    // variant rather than `None` for the same reason Step 2.3's second
    // assertion exists: `None` is the default, so it proves nothing.
    assert_eq!(info.id, 1);
    assert_eq!(info.name, "web");
    assert_eq!(info.status, ProcStatus::Online);
    assert_eq!(info.pid, Some(1));
    assert_eq!(info.restarts, 1);
    assert_eq!(info.uptime_ms, 1);
    assert_eq!(info.fold.as_deref(), Some("f"));
    assert_eq!(info.out_file.as_deref(), Some("o"));
    assert_eq!(info.err_file.as_deref(), Some("e"));
    assert_eq!(info.cpu_percent, Some(1.0));
    assert_eq!(info.memory_bytes, Some(1));
    assert_eq!(info.dog, Some(DogSource::BuiltIn));

    // Field ASSIGNMENT is still legal across the boundary — the attribute
    // blocks construction, not mutation, and several call sites in shep-cli
    // and shep-daemon depend on that.
    let mut adjusted = info.clone();
    adjusted.pid = None;
    assert_eq!(adjusted.name, info.name);
    assert_eq!(adjusted.pid, None);
}
```

Run `cargo test -p shep-core --test process_info_builder_from_outside_the_crate`.
This adds a 16th result line; note it in the phase's count.

**Fourth mutation, so that file is not itself a check that cannot fail.**
Blank the `pid` setter's body the way the `dog` mutation blanked its own, and
re-run that one test binary. `the_builder_reaches_every_field_from_outside_the_crate`
must go red on `assert_eq!(info.pid, Some(1))`. Before the field-by-field
asserts above were added, this file built a `ProcessInfo` and then checked
only that assignment worked — every setter in it could have been empty.

### Step 2.7 — CHANGELOG and gate

`crates/shep-core/CHANGELOG.md`:

- `Changes`: "`protocol::ProcessInfo` is `#[non_exhaustive]` — construct it
  with `ProcessInfo::builder`. Fields remain `pub`; reading and assigning are
  unchanged. A future field is no longer a breaking change for downstream
  crates."
- `Additions`: "`protocol::ProcessInfoBuilder`."

Full task gate.

---

## Task 3 — a stamped action correlation, and `SHEP_CHANNEL_VERSION`

**Closes:** wire.md #2 (Important, one-file) and wire.md #3 (Minor, one-line).

**The bug, exactly.** `ActionWaits::answer` (`supervisor.rs:1100-1105`) checks
the `abandoned` debt queue before it ever looks at a live wait, and pops a debt
entry unconditionally. Sequence: `gc` is triggered (T1), the app is slow, T1
times out, `resolve()` pushes `"gc"` onto `abandoned`. The operator re-triggers
`gc` (T2). The app — which was simply slow, not dead — sends one `action-reply`
for `gc`. `answer` finds the debt first, consumes it, returns `None`. T2's live
wait is never woken and reports `timed_out`, to an operator whose app answered
correctly and promptly. That is a wrong answer, not an error.

**Why it can be fixed now.** The daemon already stamps every wait —
`PendingAction::stamp`, a per-daemon counter set in `Actor::arm_action`
(`supervisor.rs:3855-3856`) and carried through `Msg::ActionResult`. Nothing of
it reaches the wire. Putting it on the wire and letting the app echo it back is
additive in both directions, and degrades to exactly today's behaviour for an
app that ignores it. `docs/shepherd-channel.md`'s current claim — *"adding one
now would be a silent wire break for every app already speaking it"* — is true
of a *required* correlation id and false of an optional echo, and that sentence
gets corrected here.

**And the version var.** `SHEP_CHANNEL_VERSION` is one line next to
`SHEP_CHANNEL_FD` in `tokio_runner.rs:243`. It ships in the same task because
it is what lets an app tell "this daemon stamps actions" from "this daemon
predates stamping" without probing.

### Files

- **modify** `crates/shep-daemon/src/channel.rs` — both message shapes, both
  fixtures
- **modify** `crates/shep-daemon/src/supervisor.rs` — `ActionWaits`,
  `Msg::ActionReply`, `handle_action_reply`, `arm_action`, `run_sheep`'s
  forward
- **modify** `crates/shep-daemon/src/tokio_runner.rs` — the env var
- **modify** `docs/shepherd-channel.md` — the correlation section and both
  wire tables
- **modify** `docs/specs/shep-v1.md` — §7's message enumeration (line 209-215)
  and §9's `params` example (line 339). This task changes the fd-3 wire an app
  receives, and the spec is the project's behaviour contract — it is what the
  next audit re-derives against, so leaving it asserting the old shape is how
  a correct change gets re-reported as drift.
- **modify** `crates/shep-daemon/CHANGELOG.md`

### Interfaces this task produces

```rust
// crates/shep-daemon/src/channel.rs
pub enum ShepherdMessage {
    Shutdown,
    Action { name: String, params: Option<String>, id: u64 },
}
pub enum ChildMessage {
    Ready,
    Metric { name: String, value: f64 },
    ActionReply { action: String, body: String, id: Option<u64> },
}

/// The value of `SHEP_CHANNEL_VERSION`.
pub const CHANNEL_VERSION: &str = "1";

// crates/shep-daemon/src/supervisor.rs — private
struct AbandonedReply { stamp: u64, action: String }
impl ActionWaits {
    fn answer(&mut self, action: &str, stamp: Option<u64>) -> Option<oneshot::Sender<String>>;
}
enum Msg { ActionReply { id: u32, action: String, body: String, stamp: Option<u64> }, .. }
```

Consumed by Task 4 (no — Task 4 pins client↔daemon wire, not fd 3; the fd-3
fixtures are pinned in this task's own `channel.rs` tests) and by Task 9's
deferral ledger only as prose.

### Step 3.1 — RED: the swallowed live reply

Add to `crates/shep-daemon/src/supervisor.rs`'s test module. The
`ActionWaits` type is private, so this is a unit test against it directly,
which is where the mechanism lives:

```rust
/// fails if a stamped reply is consumed as a debt payment instead of waking
/// the live wait it names. This is wire.md #2, in one function: T1 times out
/// and leaves a `gc` debt, T2 is triggered and is live, and the app's next
/// `gc` reply — carrying T2's stamp — must reach T2.
///
/// The alert that must not be missed: before this task, `answer` returned
/// `None` here and the operator was told `timed_out` about a request the app
/// had answered promptly and correctly.
#[test]
fn a_stamped_reply_wakes_its_own_wait_even_with_a_debt_outstanding() {
    let mut waits = ActionWaits::default();

    // T1: armed, then resolved without its reply — the timeout path.
    let (t1_reply, _t1_out) = oneshot::channel();
    let (t1_waiter, _t1_body) = oneshot::channel();
    waits.arm(PendingAction {
        stamp: 1,
        action: "gc".to_string(),
        waiter: Some(t1_waiter),
        reply: t1_reply,
    });
    assert!(waits.resolve(1).is_some(), "T1 must have been live");

    // T2: armed and still live.
    let (t2_reply, _t2_out) = oneshot::channel();
    let (t2_waiter, t2_body) = oneshot::channel();
    waits.arm(PendingAction {
        stamp: 2,
        action: "gc".to_string(),
        waiter: Some(t2_waiter),
        reply: t2_reply,
    });

    let woken = waits
        .answer("gc", Some(2))
        .expect("a reply stamped with the live wait's own stamp must reach it");
    woken.send("collected".to_string()).unwrap();
    assert_eq!(t2_body.blocking_recv().unwrap(), "collected");
}

/// fails if an UNSTAMPED reply stops behaving the way it does today. An app
/// that does not echo the stamp — every app written before this task — must
/// see byte-identical behaviour: the debt is paid first, the live wait is
/// left alone. Changing this is the regression a stamped path is most likely
/// to cause.
#[test]
fn an_unstamped_reply_still_settles_the_oldest_debt_first() {
    let mut waits = ActionWaits::default();

    let (t1_reply, _t1_out) = oneshot::channel();
    let (t1_waiter, _t1_body) = oneshot::channel();
    waits.arm(PendingAction {
        stamp: 1,
        action: "gc".to_string(),
        waiter: Some(t1_waiter),
        reply: t1_reply,
    });
    waits.resolve(1);

    let (t2_reply, _t2_out) = oneshot::channel();
    let (t2_waiter, _t2_body) = oneshot::channel();
    waits.arm(PendingAction {
        stamp: 2,
        action: "gc".to_string(),
        waiter: Some(t2_waiter),
        reply: t2_reply,
    });

    assert!(
        waits.answer("gc", None).is_none(),
        "an unstamped reply pays the debt, exactly as it did before stamping"
    );
    assert!(
        waits.answer("gc", None).is_some(),
        "and the next one reaches the live wait, exactly as it did before"
    );
}

/// fails if a reply stamped for a wait that has ALREADY given up leaks into a
/// live wait of the same name. The stamped path has to settle its own debt,
/// not just skip the queue.
#[test]
fn a_stamped_reply_for_a_dead_wait_does_not_reach_a_live_one() {
    let mut waits = ActionWaits::default();

    let (t1_reply, _t1_out) = oneshot::channel();
    let (t1_waiter, _t1_body) = oneshot::channel();
    waits.arm(PendingAction {
        stamp: 1,
        action: "gc".to_string(),
        waiter: Some(t1_waiter),
        reply: t1_reply,
    });
    waits.resolve(1);

    let (t2_reply, _t2_out) = oneshot::channel();
    let (t2_waiter, _t2_body) = oneshot::channel();
    waits.arm(PendingAction {
        stamp: 2,
        action: "gc".to_string(),
        waiter: Some(t2_waiter),
        reply: t2_reply,
    });

    assert!(
        waits.answer("gc", Some(1)).is_none(),
        "T1's own late reply belongs to T1's debt, not to T2"
    );
    assert!(
        waits.answer("gc", Some(2)).is_some(),
        "and T2 is still waiting for its own"
    );
}
```

Run:

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

**Expected failure — for the stated reason:** compile error, `this method takes
1 argument but 2 arguments were supplied` on `waits.answer("gc", Some(2))`. Not
an assertion failure. Once the signature lands but the body still checks
`abandoned` first, the first test fails on `.expect("a reply stamped with the
live wait's own stamp must reach it")`, which is the assertion red this task is
actually chasing — see both.

### Step 3.2 — GREEN: the wire fields

In `crates/shep-daemon/src/channel.rs`, `ChildMessage::ActionReply`:

```rust
    /// Reply to a daemon-initiated action
    ActionReply {
        /// The action name this replies to
        action: String,
        /// Free-form reply body
        body: String,
        /// The `id` of the [`ShepherdMessage::Action`] this answers, echoed
        /// back verbatim. `None` when the app did not echo it.
        ///
        /// Optional, and that is the whole design. An app that echoes gets
        /// exact correlation: its reply reaches the wait that asked, even
        /// when an earlier trigger of the same action name timed out and is
        /// still owed a reply. An app that does not echo — every app written
        /// before this field existed — sends no `id` key at all, and the
        /// daemon falls back to matching by name and order exactly as it did
        /// before. Nothing already speaking this channel breaks, which is
        /// what makes the field additive on a wire with no handshake.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        id: Option<u64>,
    },
```

`ShepherdMessage::Action`:

```rust
    /// Custom action dispatch
    Action {
        /// The action name
        name: String,
        /// Argument text ... (existing doc unchanged)
        #[serde(skip_serializing_if = "Option::is_none", default)]
        params: Option<String>,
        /// This dispatch's correlation id, unique for the life of the
        /// daemon. Echo it back on your [`ChildMessage::ActionReply`] as
        /// `id` and the daemon matches your answer to this exact request
        /// rather than to its name.
        ///
        /// Always present, unlike [`Self::params`]: an app that ignores the
        /// key is unaffected, and an app that wants to echo must never have
        /// to handle its absence. `u64` and monotonically increasing, but
        /// neither of those is a promise an app should lean on — treat it as
        /// an opaque token to hand back.
        id: u64,
    },
```

And the version constant, at the top of `channel.rs`:

```rust
/// The value the shepherd exports as `SHEP_CHANNEL_VERSION` to every child it
/// opens a channel for.
///
/// One version, and it stays `"1"` through this field addition, because the
/// addition is additive in both directions: a daemon that stamps and an app
/// that ignores the stamp interoperate exactly as before. What the variable
/// buys is not negotiation — the shepherd still cannot ask an app what it
/// speaks — but the ability for a defensive app to notice that fd 3 is
/// carrying a protocol it has never seen, instead of failing to parse a line
/// with nothing anywhere connecting that failure to a protocol change.
///
/// `docs/shepherd-channel.md` is the definition of what `"1"` means.
pub const CHANNEL_VERSION: &str = "1";
```

Update `channel.rs`'s two hand-written fixtures. The action-reply fixture
gains a stamped case and keeps its unstamped one:

```rust
    /// fails if a reply that carries no `id` stops deserializing — the
    /// spelling every app written before Phase 10 sends, and the one the
    /// name-and-order fallback exists for.
    #[test]
    fn an_action_reply_without_an_id_round_trips() {
        let fixture = r#"{"kind":"action-reply","action":"gc","body":"ok"}"#;
        let msg = ChildMessage::ActionReply {
            action: "gc".to_string(),
            body: "ok".to_string(),
            id: None,
        };
        assert_eq!(serde_json::from_str::<ChildMessage>(fixture).unwrap(), msg);
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }

    /// fails if an echoed `id` is dropped on the way in, or emitted when
    /// absent on the way out. Both directions, because the daemon writes
    /// this type in tests and reads it in production.
    #[test]
    fn an_action_reply_with_an_echoed_id_round_trips() {
        let fixture = r#"{"kind":"action-reply","action":"gc","body":"ok","id":7}"#;
        let msg = ChildMessage::ActionReply {
            action: "gc".to_string(),
            body: "ok".to_string(),
            id: Some(7),
        };
        assert_eq!(serde_json::from_str::<ChildMessage>(fixture).unwrap(), msg);
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }

    /// fails if the daemon stops writing `id` on an action, or starts
    /// writing `params` when there is none. `id` is unconditional and
    /// `params` is not — the two halves of the same line.
    #[test]
    fn an_action_carries_its_id_with_or_without_params() {
        let bare = r#"{"kind":"action","name":"gc","id":7}"#;
        assert_eq!(
            serde_json::to_string(&ShepherdMessage::Action {
                name: "gc".to_string(),
                params: None,
                id: 7,
            })
            .unwrap(),
            bare
        );

        let with_params = r#"{"kind":"action","name":"set-log-level","params":"debug","id":8}"#;
        assert_eq!(
            serde_json::to_string(&ShepherdMessage::Action {
                name: "set-log-level".to_string(),
                params: Some("debug".to_string()),
                id: 8,
            })
            .unwrap(),
            with_params
        );
    }
```

Field order in the serialized string follows declaration order — `name`,
`params`, `id`. Keep the declaration in that order so the fixture strings above
are literally what serde emits; if a reviewer reorders the fields, these
fixtures fail, which is the intent.

### Step 3.3 — GREEN: the debt queue learns its stamp

In `supervisor.rs`, replace `abandoned: VecDeque<String>` and `answer`:

```rust
/// One reply a sheep's app still owes a wait that has already ended.
///
/// The `stamp` is what separates a late reply from a prompt one when both
/// name the same action: an app that echoes lets the daemon settle exactly
/// the debt it belongs to, and an app that does not is matched by `action`
/// and by order, the only signal the channel gives on its own.
#[derive(Debug)]
struct AbandonedReply {
    /// The wait that ended without this reply.
    stamp: u64,
    /// Its action name — the fallback key for an app that does not echo.
    action: String,
}
```

```rust
    /// Routes one reply to `action` — stamped with `stamp` if the app echoed
    /// the dispatch's `id` — to the waiter it belongs to, or `None` if it
    /// belongs to nothing.
    ///
    /// Two paths, and which one runs is the app's choice, not a mode:
    ///
    /// - **Stamped.** The reply names its own dispatch, so it goes to the
    ///   live wait carrying that stamp; failing that, it settles that stamp's
    ///   own debt; failing that, it belongs to nothing. A live wait for the
    ///   same action name is never touched by another wait's reply, which is
    ///   the correctness gap this path closes (wire.md #2).
    /// - **Unstamped.** Byte-identical to the behaviour before stamping
    ///   existed: the oldest debt of that name is settled first, and only
    ///   once the debt is clear does a reply of that name reach a live wait.
    ///   Order is the only signal an unstamped channel gives, and this is
    ///   what makes of it what can be made.
    ///
    /// `None` still covers three ordinary shapes on both paths and none of
    /// them is an error: a debt settled, a second reply to an action already
    /// answered, or a reply the app volunteered without being asked.
    fn answer(&mut self, action: &str, stamp: Option<u64>) -> Option<oneshot::Sender<String>> {
        if let Some(stamp) = stamp {
            if let Some(pending) = self
                .live
                .iter_mut()
                .find(|pending| pending.stamp == stamp && pending.waiter.is_some())
            {
                return pending.waiter.take();
            }
            if let Some(owed) = self
                .abandoned
                .iter()
                .position(|debt| debt.stamp == stamp)
            {
                self.abandoned.remove(owed);
            }
            return None;
        }
        if let Some(owed) = self
            .abandoned
            .iter()
            .position(|debt| debt.action == action)
        {
            self.abandoned.remove(owed);
            return None;
        }
        self.live
            .iter_mut()
            .find(|pending| pending.action == action && pending.waiter.is_some())
            .and_then(|pending| pending.waiter.take())
    }
```

`resolve` pushes the richer entry:

```rust
        if pending.waiter.is_some() {
            self.abandoned.push_back(AbandonedReply {
                stamp: pending.stamp,
                action: pending.action,
            });
            if self.abandoned.len() > MAX_ABANDONED_ACTION_REPLIES {
                self.abandoned.pop_front();
            }
        }
```

Update `ActionWaits`'s own doc comment. The paragraph beginning *"An app is
free to answer an action after that action's wait has given up, and a reply
carries the action NAME and nothing else — no request id, no stamp the daemon
chose, and adding one would be a silent break for every deployed app"* is now
false and must be replaced:

```rust
/// # Why the second half exists
///
/// An app is free to answer an action after that action's wait has given up.
/// Since Phase 10 the daemon stamps every dispatch and an app may echo that
/// stamp back, in which case a late reply is unambiguous and settles its own
/// debt with nothing else at risk. An app that does not echo leaves the
/// daemon with the action NAME and nothing else, and a late reply to a `gc`
/// that timed out is then byte-identical to a prompt reply to a `gc`
/// triggered afterwards. Handing that to the second wait answers an
/// operator's question with another operator's answer — a wrong answer, not
/// an error, and the sharpest failure this type exists to prevent.
///
/// What separates them for an unstamped app is order, which is the one thing
/// the channel preserves on its own: a child reads its actions in the order
/// they were written and its replies arrive in the order it wrote them. So a
/// wait that ends without its reply leaves a debt behind, and the next
/// unstamped reply naming that action pays the debt instead of the live wait.
/// Only once the debt is settled does an unstamped reply of that name reach a
/// wait again. Echoing the stamp is how an app opts out of that whole
/// mechanism.
```

### Step 3.4 — GREEN: plumb the stamp

`Msg::ActionReply` (`supervisor.rs:303`) gains a field, and its doc's claim
*"The correlation an app gives us is the action NAME and nothing else"* is
replaced:

```rust
    /// The sheep's shepherd channel carried a reply to an action.
    ///
    /// Routed to the waiting action task, if one is waiting — dropped
    /// silently otherwise, exactly as `Msg::Ready` is. `stamp` is the
    /// dispatch id the app echoed, when it echoed one; without it the only
    /// correlation the app gave us is the action NAME, and which wait the
    /// reply belongs to is a question [`ActionWaits::answer`] answers rather
    /// than one the message carries.
    ActionReply {
        /// The sheep's id.
        id: u32,
        /// The action the app is answering.
        action: String,
        /// The reply body, exactly as the app sent it.
        body: String,
        /// The dispatch stamp the app echoed, if it echoed one.
        stamp: Option<u64>,
    },
```

The actor loop arm (`supervisor.rs:1361`):

```rust
                Msg::ActionReply {
                    id,
                    action,
                    body,
                    stamp,
                } => {
                    self.handle_action_reply(id, &action, body, stamp);
                    false
                }
```

`handle_action_reply` (`supervisor.rs:3891`):

```rust
    fn handle_action_reply(&mut self, id: u32, action: &str, body: String, stamp: Option<u64>) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        if let Some(waiter) = slot.actions.answer(action, stamp) {
            let _ = waiter.send(body);
        }
    }
```

`arm_action` (`supervisor.rs:3860`) puts the stamp on the wire:

```rust
        let waiter = spawn_action_task(
            id,
            stamp,
            ShepherdMessage::Action {
                name: action.clone(),
                params,
                id: stamp,
            },
            to_child,
            timeout,
            self.tx.clone(),
        );
```

The `run_sheep` forward (`supervisor.rs:4626`) — note the rename, since `id`
already means the sheep here:

```rust
                    Some(ChildMessage::ActionReply {
                        action,
                        body,
                        // The child's `id` is the DISPATCH's, not the sheep's;
                        // `id` below is the sheep's. Renamed at the boundary
                        // so no line downstream has to hold both meanings.
                        id: stamp,
                    }) => {
                        let _ = actor_tx
                            .send(Msg::ActionReply {
                                id,
                                action,
                                body,
                                stamp,
                            })
                            .await;
                    }
```

**The sweep is wider than `ChildMessage::ActionReply`.** `id: u64` is
unconditional on `ShepherdMessage::Action`, so every construction of that
variant stops compiling too. Six sites, all of them named here rather than
left to the compiler, because two of them require choosing a number and a
number chosen by trying values until the test goes green is exactly the
failure this phase exists to stop:

| Site | What it is | What to write |
|---|---|---|
| `crates/shep-daemon/src/supervisor.rs:3860` | `arm_action`'s real dispatch | `id: stamp` (above) |
| `crates/shep-daemon/src/supervisor.rs:8753` | `assert_eq!(sent_action(..), ShepherdMessage::Action { .. })` in the round-trip test | `id: 0` |
| `crates/shep-daemon/src/supervisor.rs:8814` | the same assert in the timeout test | `id: 0` |
| `crates/shep-daemon/src/fake.rs:844` | the fake's own channel round-trip | any value; `id: 0` |
| `crates/shep-daemon/tests/real_runner.rs:477` | `channel_round_trip`'s send | `id: u64::from(round)` |
| `crates/shep-daemon/src/channel.rs:141`, `:169` | the two serde fixtures | per Step 3.2 |

**Why `0`, and why it is pinned rather than discovered.** `Actor.next_action_stamp`
is initialised to `0` (`supervisor.rs:845`) and `arm_action` reads it and then
increments (`:3855-3856`), so the first dispatch of any freshly-built actor
carries stamp `0`. Both supervisor tests build their own actor and trigger
exactly once, so `0` is the first and only stamp either of them ever sees.

Write the literal deliberately and say so in a comment at both sites: it is
what proves the daemon stamps at all, and a `..` or a wildcard there would
turn the assert back into "an action of this name was sent", which is what it
asserted before this task. If a later change makes a test trigger twice, the
second is `id: 1` — the counter is per-actor and never resets.

`real_runner.rs:477` is the one site that should NOT hardcode a constant:
`channel_round_trip` is called once per round and the value is only carried
through the fake's pipe, so deriving it from `round` keeps each round's
message distinct, which is what that helper is for.

Then the `ChildMessage::ActionReply` sites the compiler names —
`supervisor.rs:8761` and `real_runner.rs:533`, `:544`. Give at least one of
them `id: Some(..)` so the stamped path is exercised end to end through the
actor, not only in the `ActionWaits` unit tests. `supervisor.rs:8761` is the
right one: it is the round-trip test whose action is asserted at `:8753` as
`id: 0`, so `id: Some(0)` there makes the echo a real round trip rather than
two unrelated literals.

### Step 3.5 — GREEN: the env var

`crates/shep-daemon/src/tokio_runner.rs:243`:

```rust
        if spec.channel {
            command.env("SHEP_CHANNEL_FD", "3");
            // Not negotiation — the shepherd still cannot ask an app what it
            // speaks — but an app that wants to be defensive can now tell a
            // channel it understands from one it does not, instead of
            // failing to parse a line with nothing connecting that failure to
            // a protocol change. One line, taken while it is still free.
            command.env("SHEP_CHANNEL_VERSION", crate::channel::CHANNEL_VERSION);
```

A test in `crates/shep-daemon/tests/real_runner.rs`, alongside the existing
fd-3 cases. Asserted against a real spawn rather than against the `Command`
builder, because what an app can actually read is the only thing the variable
is for:

```rust
/// fails if a real child with a channel does not see `SHEP_CHANNEL_VERSION`
/// in its environment. The child prints the variable and the pump carries it
/// to the log file, so this proves the whole path an app walks — env set on
/// the `Command`, inherited across the exec, readable by a plain shell.
///
/// Bounded by `await_file_contents`'s own `LOG_WRITE_DEADLINE` rather than by
/// an unbounded read: a version that never arrives has to fail here in
/// seconds, not hang until the harness gives up on the whole binary.
#[tokio::test]
async fn a_child_with_a_channel_is_told_which_channel_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_for(&dir, "/bin/sh", &["-c", "echo \"$SHEP_CHANNEL_VERSION\""]);
    // The variable rides with the channel, not with every spawn: an app with
    // no fd 3 has no channel to be told the version of.
    spec.channel = true;

    let runner = TokioRunner::new();
    let (_proc, _io) = runner.spawn(&spec).unwrap();

    await_file_contents(&dir.path().join("out.log"), "1\n").await;
}

/// fails if `SHEP_CHANNEL_VERSION` leaks into a child that was given no
/// channel. The pair matters: a variable set unconditionally would tell an
/// app with no fd 3 that it has a channel to speak.
#[tokio::test]
async fn a_child_without_a_channel_is_told_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let spec = spec_for(&dir, "/bin/sh", &["-c", "echo \"[$SHEP_CHANNEL_VERSION]\""]);
    assert!(!spec.channel, "`spec_for` opens no channel by default");

    let runner = TokioRunner::new();
    let (_proc, _io) = runner.spawn(&spec).unwrap();

    await_file_contents(&dir.path().join("out.log"), "[]\n").await;
}
```

Check `await_file_contents`'s exact signature and trailing-newline convention
at `real_runner.rs:70` before writing the expected strings — it is the
existing helper and it already carries the bound; do not hand-roll a second
one.

The bound is the point, and it is the rule Task 9 writes down as IR-46. The
citation is deliberately **absent** from the comment above: `docs/idiomatic-rust.md`
ends at IR-45 today and IR-46 does not exist until Task 9, which runs last.
A shipped comment citing a rule number that resolves to nothing for six tasks
is a broken reference, and Task 7 is editing IR-20 in the same phase, so a
renumber would leave it silently wrong rather than merely early. Task 9's own
Step 9.3 walks back over these two tests and confirms they satisfy the rule
once it exists.

### Step 3.6 — GREEN: the app-author contract

`docs/shepherd-channel.md`. Three edits, and the third is the one that matters.

**"What you send"** table row:

```
| `{"kind":"action-reply","action":"<name>","body":"<text>","id":<number>}` | Your answer to a triggered action. `action` names which one; `body` is free-form text and becomes what the operator sees. `id` is optional — echo the `id` from the `action` message you are answering and shep matches your reply to that exact request. |
```

**"What you receive"** table row:

```
| `{"kind":"action","name":"<name>","id":<number>}`, optionally with `"params":"<text>"` | An operator ran `shep trigger <selector> <name> [params]` against you. `params` is present only when the operator supplied one; `id` is always present — echo it on your reply. |
```

**"Getting a channel"**, after the `SHEP_CHANNEL_FD` paragraph:

```markdown
The daemon also exports `SHEP_CHANNEL_VERSION`, currently `1`. It describes
the wire on fd 3 as this document defines it. shep cannot ask what your app
speaks, so this is not a negotiation — it is there so a defensive app can
notice a version it has never seen and say so, rather than failing to parse a
line with nothing to connect that failure to a protocol change.
```

**The correlation section.** Replace the whole paragraph beginning *"Replies
are matched by action name and by order, not by a request id"*:

```markdown
**Echo the `id`, and your reply is matched exactly.** Every `action` message
carries an `id` — an opaque number, unique for the life of the daemon. Put it
back on your `action-reply` as `id` and shep hands your answer to that exact
request. Do that and everything below stops applying to you.

**If you don't echo it, replies are matched by action name and by order.**
That is the fallback, and it is what every app written before `id` existed
gets. shep matches your `action-reply` to a waiting trigger by name, and if
you have two of the same action outstanding, by the order you wrote them. If
you reply to an action after its trigger has already timed out, that late
reply settles the debt for that one timeout rather than being handed to
whatever triggered the same action name next — but that protection covers
exactly one stray reply per timeout, and it has a sharp edge: while a debt is
outstanding, an unstamped reply to a *live* trigger of the same name is
consumed as the debt payment, and the live trigger reports `timed_out` even
though you answered it promptly. Echoing `id` is how you make that
impossible. Failing that, don't sit on a reply, and don't send more than one
per action you were asked.
```

And in "Summary for the impatient", after "Reply to every `action` message":

```markdown
- Echo the `id` from the action on your reply — one field, and it is what
  makes a slow action's answer land on the right trigger.
```

**Then the spec, which is the contract the next audit re-derives against.**
`docs/specs/shep-v1.md` asserts the old shape in two places and both become
false the moment this task lands.

§7's message enumeration (lines 209-215) currently reads *"daemon→child
`{"kind":"shutdown"}`, `{"kind":"action",...}`. Fd number exported as
`SHEP_CHANNEL_FD`. An `action` carries `name`, and `params` when the operator
supplied any"*, and its pointer to `docs/shepherd-channel.md` advertises *"how
a reply is matched to its trigger with no correlation id"*. Replace the
`SHEP_CHANNEL_FD` sentence onward with:

```markdown
  `{"kind":"action",...}`. Fd number exported as `SHEP_CHANNEL_FD`, wire
  version as `SHEP_CHANNEL_VERSION` (`1`). An `action` carries `name` and
  `id`, and `params` when the operator supplied any — the `params` key is
  absent otherwise, which is what keeps it additive (§9). `id` is the
  dispatch's correlation token; an app that echoes it back on its
  `action-reply` as `id` gets its answer matched to that exact request, and
  an app that does not is matched by action name and by order, exactly as
  every app written before the field existed. Full contract for an app author
  writing to this wire, including the parts this bullet has no room for (why
  an action should reply even to a name it does not recognize, what the
  name-and-order fallback costs when two triggers of one action overlap, the
  `params` quoting gap): [`docs/shepherd-channel.md`](../shepherd-channel.md).
```

The "no correlation id" clause is the half that must not survive — it is the
sentence Task 3 exists to make false.

§9's `params` example (line 339) reads *"so `{"kind":"action","name":"gc"}` is
still exactly what an argument-free action looks like on the wire"*. It is now
`{"kind":"action","name":"gc","id":7}`. Correct the example, and leave the
surrounding argument about `params` being additive untouched — that argument
is still exactly right, and `id` is a second instance of it rather than a
counter-example:

```markdown
Additive is what makes it survivable at all: `params` is omitted from the
serialized message when there are none, so `{"kind":"action","name":"gc","id":7}`
is still exactly what an argument-free action looks like on the wire, a
message with no `params` key reads back as none, and an app that ignores the
field goes on working.
```

### Step 3.7 — MUTATION

Break this line in `ActionWaits::answer`:

```rust
                .find(|pending| pending.stamp == stamp && pending.waiter.is_some())
```

change `pending.stamp == stamp` to `pending.action == action`.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Must go red:** `a_stamped_reply_for_a_dead_wait_does_not_reach_a_live_one`
fails on `waits.answer("gc", Some(1)).is_none()` — T1's own late reply now
reaches T2, which is the exact bug this task exists to close, re-created.
`a_stamped_reply_wakes_its_own_wait_even_with_a_debt_outstanding` will pass
under this mutation, and that is why the third test is not redundant with the
first: one proves a stamped reply *arrives*, the other proves it arrives at the
*right* wait, and only the second can fail this way.

Second mutation, on the fallback path: change

```rust
            .position(|debt| debt.action == action)
```

to `.position(|_debt| false)`. Run the same command.
`an_unstamped_reply_still_settles_the_oldest_debt_first` must go red on its
first assertion. If it stays green, the fallback is untested and every app
written before this task is unguarded.

Third mutation: delete the `command.env("SHEP_CHANNEL_VERSION", …)` line and run
`cargo test -p shep-daemon --test real_runner --all-features`.
`a_child_with_a_channel_is_told_which_channel_it_is` must go red on
`await_file_contents` timing out against `"1\n"`, and
`a_child_without_a_channel_is_told_nothing` must stay green.

Fourth mutation, on the pair's other half: move the `command.env` call outside
the `if spec.channel` block. Now
`a_child_without_a_channel_is_told_nothing` must go red and the first must stay
green. A mutation pair that reddens the same test both ways means the two cases
are testing one fact twice.

### Step 3.8 — CHANGELOG and gate

`crates/shep-daemon/CHANGELOG.md`:

- `Fixes`: "A reply to a live trigger is no longer swallowed as a previous
  trigger's timeout debt when the app echoes the dispatch `id`."
- `Additions`: "`ShepherdMessage::Action` carries `id`; `ChildMessage::ActionReply`
  accepts an optional `id` echo. Additive — an app that ignores both is
  matched by name and order exactly as before."
- `Additions`: "`SHEP_CHANNEL_VERSION` is exported to every child with a
  channel; `channel::CHANNEL_VERSION` is its value."

Full task gate.

---

## Task 4 — the wire fixture sweep, and one doc line on `Response`

**Closes:** wire.md #4 (PARTIAL, one-file) and wire.md #5 (Minor, one-line).

Mechanical, no design question. Three existing insta lists grow; the pre-Phase-9
gaps close. Phase 9's own dog wire surface is already properly pinned and needs
nothing — say so in the commit so the next auditor does not re-check it.

### Files

- **modify** `crates/shep-core/src/protocol/request.rs` — `request_wire_snapshots`,
  `reply_wire_snapshots`, one doc line on `Response`
- **modify** `crates/shep-core/src/protocol/events.rs` — `bus_event_wire_snapshots`
- **regenerate** `crates/shep-core/src/protocol/snapshots/*.snap`

### Step 4.1 — RED: the three unpinned `SelectorSpec` variants

`request_wire_snapshots` currently exercises `All` and `Name` only. Append,
after the `Request::DisableDog` row (id 13):

```rust
            // The three selector shapes no fixture reached before Phase 10.
            // Grouped and adjacent on purpose: `Id`, `Regex` and `Fold` are
            // three newtypes over three different inner types, and the wire
            // tells them apart only by their own `kind` tag — a `Fold` that
            // serialized under `regex`'s tag is a `shep restart fold:api`
            // that silently becomes a regex match, which is a wrong set of
            // sheep restarted and not an error anyone sees.
            Envelope {
                id: 14,
                deadline_ms: None,
                body: Request::Describe {
                    selector: SelectorSpec::Id(7),
                },
            },
            Envelope {
                id: 15,
                deadline_ms: None,
                body: Request::Describe {
                    selector: SelectorSpec::Regex("^web-".to_string()),
                },
            },
            Envelope {
                id: 16,
                deadline_ms: None,
                body: Request::Describe {
                    selector: SelectorSpec::Fold("api".to_string()),
                },
            },
```

All three ride on `Describe` so the only thing that differs row to row is the
selector, which is what the pinning is for.

### Step 4.2 — RED: the six unpinned `ProcessEventKind` variants

In `events.rs`'s `bus_event_wire_snapshots`, append one row per unpinned kind
**to the end of the `events` vec, after the `Dropped` row** — not after the
`Exit` row it shares a variant with. The list is a flat
`vec![Exit, LogOut, Dropped]` and insta serializes it positionally, so an
insertion in the middle shifts `LogOut` and `Dropped` down and the diff stops
being a pure addition, which is Step 4.3's own acceptance guard. Grouping by
variant is not worth losing a readable diff over.

The binding at `events.rs:143` is `let events = vec![...]`. It becomes
`let mut events = vec![...]` so `events.extend(lifecycle)` compiles.

To keep the snapshot readable and the diff meaningful, hoist the shared `info`
into a local built with Task 2's builder:

```rust
        // Every lifecycle kind a `process.*` subscriber can receive, over one
        // identical `info`, so the snapshot rows differ by their `event` tag
        // and by nothing else. Only `Exit` and the three reload kinds were
        // pinned before Phase 10; the six here are the ordinary events a real
        // integration — a dashboard, a bark rule — depends on first, and a
        // Rust-identifier rename on any of them would change the wire string
        // mechanically, compile clean, and break that integration silently.
        let sample = ProcessInfo::builder(3, "web", ProcStatus::WaitingRestart)
            .restarts(2)
            .uptime_ms(500)
            .out_file(Some("/home/ada/.shep/logs/web-0-out.log".to_string()))
            .err_file(Some("/home/ada/.shep/logs/web-0-err.log".to_string()))
            .build();

        let lifecycle = [
            ProcessEventKind::Start,
            ProcessEventKind::Online,
            ProcessEventKind::Restart,
            ProcessEventKind::Stop,
            ProcessEventKind::Delete,
            ProcessEventKind::Errored,
        ]
        .map(|event| BusEvent::Process {
            event,
            info: sample.clone(),
            manually: false,
            at_ms: 1_700_000_000_000,
        });
```

then, after the `vec!` literal and before the `assert_json_snapshot!` line:

```rust
        events.extend(lifecycle);
```

Keep the existing `Exit`, `LogOut` and `Dropped` rows exactly where they are,
in that order, so the existing snapshot's first three entries do not move — a
reordered snapshot is a diff nobody reads, and Step 4.3 is written to fail if
one of them changes.

### Step 4.3 — RED: the eleven unpinned `Response` variants

In `reply_wire_snapshots`, after the `Response::DogStarted` row (id 8):

```rust
            // The eleven variants no fixture reached before Phase 10. The
            // existing comment on the `Triggered` row is right that pinning
            // `Flock` once already proves the `Vec<ProcessInfo>` SHAPE — but
            // it does not prove any of these variants' own `kind` tags, and
            // three of them are not `Vec<ProcessInfo>`-shaped at all
            // (`Deleted` is a `Vec<u32>`, `Subscribed` and `ShuttingDown`
            // carry nothing). Each row below therefore carries the emptiest
            // legal body: what is being pinned here is the tag, and a body
            // repeated eight times would bury it.
            Reply { id: 9, result: Ok(Response::Described(vec![])) },
            Reply { id: 10, result: Ok(Response::Started(vec![])) },
            Reply { id: 11, result: Ok(Response::Stopped(vec![])) },
            Reply { id: 12, result: Ok(Response::Restarted(vec![])) },
            Reply { id: 13, result: Ok(Response::Reloading(vec![])) },
            Reply { id: 14, result: Ok(Response::Deleted(vec![7, 8])) },
            Reply { id: 15, result: Ok(Response::Reopened(vec![])) },
            Reply { id: 16, result: Ok(Response::Flushed(vec![])) },
            Reply { id: 17, result: Ok(Response::Mustered(vec![])) },
            Reply { id: 18, result: Ok(Response::Subscribed) },
            Reply { id: 19, result: Ok(Response::ShuttingDown) },
```

`Deleted` gets two real ids rather than an empty vec: it is the one variant
here whose body type is not `ProcessInfo`, and an empty `Vec<u32>` and an empty
`Vec<ProcessInfo>` serialize identically, which would leave its element type
unpinned.

Cross-check the eleven names against the enum before writing them —
`request.rs:369-428` and the dog variants after it. The input's re-derivation
lists eleven (`Described`, `Started`, `Stopped`, `Restarted`, `Reloading`,
`Deleted`, `Reopened`, `Flushed`, `Mustered`, `Subscribed`, `ShuttingDown`);
the original audit listed ten and omitted `Mustered`. Trust the enum, not
either list, and note in the commit which count was right.

Run:

```bash
cargo test -p shep-core --lib --all-features
```

**Expected failure — for the stated reason:** three insta snapshot assertion
failures, each printing a diff whose *only* content is added rows. Not a
compile error. If a row's diff shows a *changed* existing entry, an insertion
went in the wrong place — fix the position, do not accept the snapshot.

### Step 4.4 — GREEN: accept, then read

```bash
cargo insta accept --workspace
```

then read the three `.snap` files by hand before committing:

```bash
git diff --stat crates/shep-core/src/protocol/snapshots/
git diff crates/shep-core/src/protocol/snapshots/
```

Confirm every hunk is a pure addition, and that each added row's `kind` string
is the snake_case of its Rust identifier (`shutting_down`, `reload_abandoned`,
`fold`, `regex`). A `kind` that is not the mechanical derivation means someone
added a `#[serde(rename)]` this sweep has just cemented.

```bash
cargo test -p shep-core --lib --all-features
```

Expect green, no `.snap.new` left behind.

### Step 4.5 — GREEN: the `Response` doc line

wire.md #5's entire recommendation. On `pub enum Response`, above the
`#[non_exhaustive]`:

```rust
/// Nine variants carry a bare `Vec<ProcessInfo>` (`Flock`, `Described`,
/// `Started`, `Stopped`, `Restarted`, `Reloading`, `Reopened`, `Flushed`,
/// `Mustered`), and that repetition is intentional — do not collapse them
/// into one. Each names which request it answers, which is what lets a
/// variant diverge later without a protocol bump: `Reloading` already means
/// an acceptance rather than a result, and `Mustered` already means "every
/// sheep of every restored app" rather than "what this call started". A
/// single `Listing(Vec<ProcessInfo>)` would have to relitigate both of those
/// as a breaking change.
```

### Step 4.6 — MUTATION

Break exactly one line in `crates/shep-core/src/protocol/request.rs`, in the
`Response` enum:

```rust
    ShuttingDown,
```

change to

```rust
    #[serde(rename = "shutdown")]
    ShuttingDown,
```

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `reply_wire_snapshots` fails with a diff showing
`"kind": "shutting_down"` → `"kind": "shutdown"`. Before this task that
mutation was invisible to every test in the workspace.

Second mutation, in `events.rs`: rename the variant `Errored` to `Failed`
(and its match arms). `bus_event_wire_snapshots` must go red on the
`"event": "errored"` → `"event": "failed"` line. This is the exact class of
change wire.md #4 warns about — a rename someone believes is "just internal".

Third mutation, in `request.rs`: swap `SelectorSpec::Fold(String)` and
`SelectorSpec::Regex(String)` in the enum's declaration order. This one must
**not** change the snapshot (serde tags by name, not position) — confirm that,
and if it does change, the enum is position-sensitive somewhere and that is a
finding of its own.

### Step 4.7 — CHANGELOG and gate

`crates/shep-core/CHANGELOG.md`, under `Changes`: "Wire fixtures now pin every
`SelectorSpec` variant, every `ProcessEventKind`, and every `Response` tag. No
wire string changed — this closes coverage, not behaviour."

Full task gate.

---

## Task 5 — the red linux/arm64 test, and two cross-compile gates

**Closes:** platform.md #1 (Important, one-line), platform.md #3 (Important,
one-line), and the new platform finding — the windows-gnu gate every plan
through Phase 6 carried and Phase 9 silently dropped.

**#1, precisely.** `a_daemon_that_closes_without_answering_is_not_a_silent_success`
(`crates/shep-client/src/connection.rs:258`) asserts
`Err(ConnectError::HandshakeClosed)`. That is macOS's shape: the peer's close
is observed by the *read* after `Hello` is sent. On linux/arm64 the *write*
fails first — `frames.send(payload)` at `connection.rs:169` maps to
`ConnectError::Io` — because AF_UNIX delivers the peer's close to the next
write. Both CLI consumers already collapse the two variants into the same
outcome (`exit.rs:127` → `DaemonUnreachable`; `spawn.rs:246-252` treats `Io`
and `HandshakeClosed` alike), so nothing user-facing is wrong. But this is the
only test in the project known to be red anywhere, which means
`cargo test --workspace` does not go green on the platform the maintainer is migrating to.

**#3, precisely.** `notify.rs:122`'s `#[cfg(target_os = "linux")] fn
send_to_abstract` has never been through a compiler on this codebase. It is
what a systemd `Type=notify` unit depends on for readiness reporting — the unit
`shep startup` itself installs on Linux. The fix is not a code change; it is
running a compiler at it, and writing the command down so it stays run.

### The third finding, and why it is settled rather than open

Phases 3, 4, 5 and 6 all carried
`cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu`
in their gate lists (`docs/writing-plans/plans/2026-08-08-shep-phase3-cli.md:75`
and three siblings). Phases 7, 8 and 9 carry it nowhere — `grep -rn
"x86_64-pc-windows-gnu" docs/writing-plans/plans/` returns nothing for any of
the three. It was dropped without a sentence saying so, and it never reached
`CLAUDE.md`'s own gate list, which is why nothing noticed.

**It was measured on 2026-08-13, at `b7c466b`, and it passes.** Run from a
separate `CARGO_TARGET_DIR` so the host cache was untouched:

```
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
EXIT=0    Finished `dev` profile ... in 8.42s
```

So this is not an open question for the maintainer and this plan does not put one to her.
It is a check that was dropped, still works, and goes back. What it needs is a
stated prerequisite: `ring`'s build script runs `cc` for the target, so the
cross-compile needs a C toolchain for `x86_64-pc-windows-gnu`. On this machine
that is `x86_64-w64-mingw32-gcc`, present at `/opt/homebrew/bin` since Phase
9's final review installed `mingw-w64`. A host without it cannot run the gate,
and that is the most likely reason it quietly stopped being carried.

### Files

- **modify** `crates/shep-client/src/connection.rs` — the assertion and its
  comment
- **modify** `CLAUDE.md` — two new cross-check commands after "The task gate"
- **modify** `crates/shep-client/CHANGELOG.md`

### Step 5.1 — RED: state the outcome, not the variant

Replace the test body at `connection.rs:257-270`:

```rust
    /// fails if a daemon that accepts and immediately closes is reported as
    /// anything other than "unreachable". Deliberately asserts the OUTCOME
    /// BUCKET rather than one `ConnectError` variant, because which variant
    /// this produces is a kernel-semantics question and the two kernels shep
    /// runs on answer it differently:
    ///
    /// - macOS lets the `Hello` write succeed and delivers the close to the
    ///   following read, which is `HandshakeClosed`;
    /// - Linux delivers the peer's close to the pending write, so
    ///   `frames.send` fails first and the error is `Io`.
    ///
    /// Both are correct. Nothing downstream distinguishes them either —
    /// `shep-cli`'s `exit.rs` folds `Io`, `HandshakeClosed`, `Connect`,
    /// `Wire` and `HandshakeTimeout` alike into `DaemonUnreachable`, and
    /// `spawn.rs`'s `connect_or_spawn_with` special-cases only `Connect` and
    /// `HandshakeTimeout`. Pinning the variant here asserted a platform, not
    /// a contract, and was red on linux/arm64 for exactly that reason
    /// (platform.md #1).
    ///
    /// What must NOT happen is a silent success, and that is what this still
    /// guards: an `Ok(Connection)` from a peer that answered nothing.
    #[tokio::test]
    async fn a_daemon_that_closes_without_answering_is_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT)
            .await
            .expect_err("a peer that closed without a HelloReply is not a connection");

        assert!(
            matches!(err, ConnectError::HandshakeClosed | ConnectError::Io(_)),
            "a peer that closed mid-handshake must report as unreachable, got {err:?}"
        );
    }
```

**Two things this test must not become**, both of which a reviewer will
reasonably propose:

- *"Assert the `DaemonUnreachable` bucket directly."* No — that bucket is
  `shep-cli`'s `ExitCode`, and `shep-client` neither depends on `shep-cli` nor
  may start to. The comment naming `exit.rs:127` is how the downstream fact is
  carried; a dependency edge is not.
- *"Widen the set to include `HandshakeTimeout` while we're here."* No — a
  timeout here would mean the peer never closed at all, which is a different
  bug, and admitting it would leave this test unable to fail. Two variants is
  the whole of the divergence; anything more is giving up on the assertion.

### Step 5.2 — Watch it fail for the stated reason

There is no local Linux box in this project's loop, so the failure this fix
addresses cannot be reproduced on the development machine. Do not fake it.
Instead, prove the *new* assertion is not vacuous by forcing the other branch:

```bash
cargo test -p shep-client --lib --all-features
```

green on macOS (it was green before — the variant it happens to produce here is
`HandshakeClosed`). Then temporarily change the accepted set to
`matches!(err, ConnectError::Connect { .. })` and re-run: it must go red with
`got HandshakeClosed`. Revert. That is the honest local red for this one.

The real cross-platform confirmation is Task 6's `ubuntu-24.04-arm` matrix leg,
which is written but not enabled; say so in the commit message rather than
claiming the platform was verified.

### Step 5.3 — GREEN: compile the Linux branch

`shep-daemon` does not depend on `ring` (its dependency list is
`crates/shep-daemon/Cargo.toml:15-77`; TLS lives only in shep-cli), so a
`--target` check of that crate needs no cross C toolchain and no linker —
`cargo check` does not link.

```bash
rustup target add x86_64-unknown-linux-gnu
```

```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```

`--all-targets` is load-bearing: without it the `#[cfg(test)]` module is not
compiled and `an_abstract_address_reaches_the_abstract_namespace`
(`notify.rs:228`) — which is itself `#[cfg(target_os = "linux")]`-gated — stays
uncompiled, which is the finding.

Three possible outcomes, and each has a defined response:

1. **Clean.** Record the result in the commit message with the date;
   `send_to_abstract` has now been compiled. The `CLAUDE.md` entry itself is
   written once, in Step 5.4, so both cross-checks land in one block rather
   than two edits to the same section.
2. **A compile error inside `notify.rs`.** That is the finding paying off; fix
   it, and say in the commit that the branch had never been compiled and what
   was wrong.
3. **A dependency's build script demands a cross C toolchain.** Record the
   exact crate and error, do not chase it, and fall back to running the check
   through the ubuntu leg of Task 6's workflow — noting in `CLAUDE.md` that
   this gate is CI-only and why.

### Step 5.4 — GREEN: restore the windows-gnu gate

The check itself, run exactly as Phases 3 through 6 ran it:

```bash
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

Prerequisites, both already satisfied on this machine — verify rather than
assume, because a missing one is the difference between "passes" and "cannot
be run":

```bash
rustup target list --installed | grep x86_64-pc-windows-gnu
command -v x86_64-w64-mingw32-gcc
```

The first must print the target; the second must print a path. If the second
prints nothing, install it (`brew install mingw-w64`) rather than skipping the
gate — skipping it silently is what put this task here.

**Expected: `EXIT=0`, `Finished \`dev\` profile`.** That is what the
2026-08-13 measurement returned in 8.42s from a cold, separate target dir. A
warm host cache is not reused for a different target triple, so budget a
minute or two the first time and seconds afterwards.

**`cargo check`, not `clippy -D warnings`, and this is a decision rather than
an omission.** The same run emits **51 warnings in `shep-daemon`'s lib** on
this target, all of them dead-code: `crates/shep-daemon/src/lib.rs` gates
`boot`, `sys`, `server` and `tokio_runner` off on Windows (their `nix` and
`command-fds` dependencies are `[target.'cfg(unix)'.dependencies]`), so
everything those modules were the only consumer or producer of becomes
unreachable — `BUS_CAPACITY`, `spawn_forwarder`, `PollingEnforcer`,
`SheepStats`, and `RpcContext`'s `daemon_version` and `pid` fields among them.
None of those is a defect. They are the mechanical consequence of Windows
being 0% implemented on purpose (`deferred.md`), and they would all disappear
the day it is not.

So the gate's question is **"does the tree still compile for a target nobody
has implemented yet"**, and `cargo check` asks exactly that. Spelling it
`clippy … -D warnings` would ask a different question, get 51 answers, and the
only way to make it green would be to `#[allow(dead_code)]` code that is not
dead on any platform we ship — degrading the host clippy gate to buy nothing.
Say this in the `CLAUDE.md` entry, not just here, so the next person to notice
the asymmetry finds the reason instead of "fixing" it.

Add both cross-checks to `CLAUDE.md`, in their own section immediately after
"The task gate":

```markdown
### The two cross-checks — run once per phase, not per task

```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

One cargo command at a time, as everywhere else, and give them their own
`CARGO_TARGET_DIR` if you want the host cache left alone.

**Linux.** `notify.rs`'s abstract-namespace branch and its test are both
`#[cfg(target_os = "linux")]`, so a macOS `cargo test` compiles neither. That
branch is what a systemd `Type=notify` unit — the unit `shep startup` installs
— depends on for readiness reporting, and it went five phases without a
compiler ever reading it (platform audit #3). `--all-targets` is what reaches
the test. shep-daemon has no `ring` in its tree, so this needs no cross C
toolchain; `-p shep-cli` would, and is not in this gate.

**Windows.** Every plan through Phase 6 carried this one; Phases 7-9 dropped
it without saying so, and it never reached this file, which is why nothing
noticed for three phases. Restored in Phase 10 after being measured green
(`EXIT=0`, 8.42s, 2026-08-13). It needs a C toolchain for the target —
`brew install mingw-w64` — because `ring`'s build script runs `cc`; a host
without `x86_64-w64-mingw32-gcc` cannot run it, and that is presumably how it
came to be dropped.

`cargo check`, deliberately, not `clippy -- -D warnings`: shep-daemon's
`boot`/`sys`/`server`/`tokio_runner` are `cfg(unix)`-gated, so on Windows 51
dead-code warnings fall out of code that is not dead anywhere we ship. The
question this gate asks is whether the tree still compiles for a target nobody
has implemented yet. Silencing those warnings would mean `#[allow(dead_code)]`
on live code.
```

### Step 5.5 — MUTATION

In `crates/shep-daemon/src/notify.rs`, inside `send_to_abstract`, change

```rust
    let addr = SocketAddr::from_abstract_name(name).map_err(NotifyError::Io)?;
```

to

```rust
    let addr = SocketAddr::from_abstract_name(name_typo).map_err(NotifyError::Io)?;
```

Run the cross-check command. It **must** fail with `cannot find value
`name_typo` in this scope`. If it passes, the branch is still not being
compiled and the gate does not do what it claims — most likely `--all-targets`
was dropped or the wrong `-p` was used. Revert.

For the client test, mutate `connection.rs:174`: change

```rust
            .ok_or(ConnectError::HandshakeClosed)?
```

to

```rust
            .ok_or(ConnectError::HandshakeTimeout { after: Duration::ZERO })?
```

Run `cargo test -p shep-client --lib --all-features`. The test must go red with
`got HandshakeTimeout { .. }`. This is the check that the widened `matches!`
did not widen into unfailability.

**For the windows-gnu gate**, whose whole risk is that a restored check might
be reaching cached artifacts or a subset of the workspace rather than the
whole tree. Add one line at the top of `crates/shep-core/src/status.rs`:

```rust
use std::os::unix::ffi::OsStrExt as _;
```

Run the host gate first — `cargo check --workspace --all-targets
--all-features` — and confirm it stays **green**: `std::os::unix` exists on
macOS, so a mutation that reddened both would prove nothing about the target.
Then run

```bash
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

It **must** go red with `error[E0433]`/`E0432` on `std::os::unix` — the module
does not exist on Windows. That is the pair that proves the gate compiles the
whole workspace for the target rather than replaying a host cache. Revert.

If it stays green, the most likely cause is a stale `CARGO_TARGET_DIR` or a
`--target` that was silently ignored; do not proceed on a gate that cannot
fail.

### Step 5.6 — CHANGELOG and gate

`crates/shep-client/CHANGELOG.md`, under `Fixes`: "The handshake-close test
asserts the unreachable outcome rather than one `ConnectError` variant, so it
passes on Linux, where the peer's close surfaces on the write rather than the
read."

Full task gate, plus both cross-checks. The commit message says three things
plainly: that the linux/arm64 fix is written but not executed on linux/arm64
(Task 6's matrix leg is written and not enabled), that the windows-gnu gate
passed at `EXIT=0` on a host with mingw-w64 installed, and that it is back in
`CLAUDE.md` after three phases out.

---

## Task 6 — make CI correct and ready. Do not enable it.

**Closes:** tests.md #1 (Important, one-line) and the CI half of tests.md #5.

**The standing instruction.** the maintainer has said to keep ignoring CI minutes until
the base phases ship. `.github/workflows/test.yml:12-13` stays
`on: workflow_dispatch:`. **This task does not add a `push`, `pull_request` or
`schedule` trigger.** What it does is make the file correct, so that flipping
one line later is a decision about money and not about whether the jobs are
right, and write down what flipping it would cost.

**Why it still matters that it has never run.** Every "all gates green" claim
in this project's history — 897 tests through 1030 — is self-reported by the
same agent that wrote the code. That is not an accusation; it is a description
of the evidence available, and it is why the workflow being *correct* is worth
a task even while it is switched off.

### Files

- **modify** `.github/workflows/test.yml`
- **modify** `docs/specs/deferred.md` — the cost note

### Step 6.1 — Fix what the jobs actually run

Four discrepancies between the workflow and the project's own task gate, each
of which would make a green CI run mean less than it looks:

1. `lint` runs `cargo clippy --workspace --all-targets` with **no
   `--all-features`**; the task gate runs it with. Feature-gated code
   (`shep-daemon`'s `test-fakes`) is therefore unlinted.
2. `test` runs `cargo test --workspace --locked` with **no `--all-features`**;
   the task gate runs it with. Same gap, on the tests themselves.
3. The matrix has no aarch64 leg, which is exactly where Task 5's finding
   lives. GitHub now offers `ubuntu-24.04-arm`.
4. No job compiles `x86_64-unknown-linux-gnu` as a *check* — the `test` job
   does compile it natively on `ubuntu-latest`, which is sufficient for
   platform #3, but only if that job is in the matrix and actually runs the
   crate's tests, which it does.

Apply 1–3:

```yaml
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install 1.93 --profile minimal --component clippy,rustfmt
      - run: cargo +1.93 fmt --all -- --check
      # `--all-features` mirrors the project's own task gate (CLAUDE.md).
      # Without it, everything behind shep-daemon's `test-fakes` feature is
      # unlinted, and clippy's whole value here is that it sees what the
      # local gate sees.
      - run: cargo +1.93 clippy --workspace --all-targets --all-features -- -D warnings
```

```yaml
  test:
    strategy:
      fail-fast: false
      matrix:
        # `ubuntu-24.04-arm` is the leg that matters most and the one this
        # matrix went nine phases without: shep-client's handshake-close test
        # was red on linux/arm64 for five phases (platform audit #1) and
        # nothing here would have said so. Billed as a standard Linux runner,
        # not at the macOS/Windows multiplier.
        os: [ubuntu-latest, ubuntu-24.04-arm, macos-latest, windows-latest]
        toolchain: [stable, "1.88"]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install ${{ matrix.toolchain }} --profile minimal
      # `--all-features` mirrors the task gate, as in `lint` above.
      - run: cargo +${{ matrix.toolchain }} test --workspace --locked --all-features
```

Leave `minimal-versions`, `musl`, `features`, `coverage`, `docs`, `typos` and
`bench` exactly as they are — the audit found nothing wrong with them, and the
`features` job deliberately runs `--no-default-features` and `--all-features`
as its own ladder.

**Then the file's own cost claim, which this task would otherwise leave
stale.** The header comment (lines 2-5) says *"one run of this file is 15 jobs
— six of them on those two platforms"*. That is wrong twice. It is wrong
**today**: counting the matrices out, the file is 16 jobs (`lint` 1, `docs` 1,
`typos` 1, `test` 3×2=6, `minimal-versions` 1, `musl` 1, `features` 2,
`coverage` 1, `bench` 2), of which 5 are macOS or Windows (`test` gives 2 and
2, `features` gives 1). Do not "fix" 15 to 19 by adding four — recount from
the file. And it is wrong **after this task**: the arm leg adds 2 to `test`
and `privileged` adds 1, so the file becomes 19 jobs, 5 of them still on the
billed platforms.

19 is the same number Step 6.3 writes into `deferred.md`. Those two numbers
are one fact recorded in two files and they must be edited together — a task
whose acceptance check is "the file is correct" cannot ship a workflow whose
first sentence disagrees with the ledger entry the same task writes.

```yaml
name: test
# Manual-only while the repository is private. Actions minutes on a private
# repo bill macOS at 10x and Windows at 2x, and one run of this file is 19
# jobs — five of them on those two platforms (two macOS and two Windows in
# `test`, one Windows in `features`). The full arithmetic, and what turning
# the triggers on would cost, is in docs/specs/deferred.md. Run it from the
# Actions tab when you want it.
#
# Restore the automatic triggers when the repository goes public, where
# Actions is free:
#   push: { branches: [main] }
#   pull_request:
#   schedule: [{ cron: "0 0 * * SUN" }]
```

### Step 6.2 — The root-in-Docker job

`crates/shep-daemon/tests/real_runner.rs:642` is `#[ignore]`d because the
privilege-drop path it proves needs a real uid change, which needs root. Three
`#[ignore]`s exist workspace-wide; the other two (`barks.rs:491`,
`shep_toml.rs:726`) are cross-process-race child halves that are re-exec'd by
their own parent tests and must stay ignored. Only this one wants a runner.

```yaml
  privileged:
    # The one `#[ignore]`d test that is ignored for want of privilege rather
    # than by design: `real_runner.rs`'s privilege-drop case needs a real uid
    # change and a real supplementary-group clear, which needs root. The other
    # two ignores in the workspace are child halves of cross-process race
    # tests, re-exec'd by their own parents, and must stay ignored.
    #
    # A container, not `sudo` on the runner: the test asserts a root daemon
    # can drop to `nobody` and that std cleared root's supplementary groups
    # on the way, so it has to actually START as root. A throwaway container
    # filesystem is the cheap way to have that.
    runs-on: ubuntu-latest
    container:
      image: rust:1.88
    steps:
      - uses: actions/checkout@v4
      # `--exact` with the one path, not a bare `--ignored`: the other two
      # `#[ignore]`s in the workspace (`barks.rs`'s `bark_race_child`,
      # `shep_toml.rs`'s `config_race_child`) are child halves that assert
      # nothing and are re-exec'd by their own parents. Running them here
      # would add two green lines that mean nothing.
      - run: >
          cargo test -p shep-daemon --test real_runner --locked --all-features
          -- --ignored --exact a_dropped_child_runs_as_the_requested_user
```

The test is `a_dropped_child_runs_as_the_requested_user`
(`crates/shep-daemon/tests/real_runner.rs:642`); it asserts `geteuid().is_root()`
first, so a non-root runner fails loudly rather than passing vacuously, and it
drops to `nobody`, which the `rust:1.88` image already has — no `useradd` step
is needed.

Two things to confirm rather than assume: that the container image's toolchain
is the MSRV this project pins, and that `--locked` does not fail because the
image ships a cargo that wants to rewrite `Cargo.lock`. If either bites, pin
the image to a digest and say why in the job's comment.

### Step 6.3 — Write down what enabling it would cost

The workflow's header comment says *why* it is manual and, after Step 6.1,
carries the job count. The arithmetic behind that count goes in
`docs/specs/deferred.md`.

**Where, exactly.** `deferred.md` has four `##` sections today: *"Scope
decision, 2026-08-12: everything below §2's six cuts ships in v1"*,
*"Committed to v1.1+ by design (spec §2)"*, *"Named as v1.0 in spec §2/§9, not
yet built"* and *"Not deferred"*. None of them fits a workflow that exists and
works but is switched off, and three tasks in this phase each need somewhere
to write a paragraph. So **Task 6 creates one new `##` section**, placed
between *"Named as v1.0 in spec §2/§9, not yet built"* and *"Not deferred"*,
and Tasks 8 and 9 append to it:

```markdown
## Known debt, recorded rather than built (Phase 10)

Not scope cuts and not unbuilt spec surface — these are things that exist and
work, or that are known to be missing, and that Phase 10 decided to write down
rather than change. Each says what it is, why it was not done, and what would
force it.
```

Unlike the sections above it, whose entries are bold-lead paragraphs, this one
uses `###` subheads: its entries run several paragraphs each and a bold lead
stops being findable at that length.

The first entry, from this task:

```markdown
### Automatic CI, and what it would cost to turn on

`.github/workflows/test.yml` is `on: workflow_dispatch:` — manual only. It is
correct and ready; the trigger is the only thing missing, and it is missing on
purpose while the repository is private.

The arithmetic, so the decision is about money rather than about whether the
jobs work. GitHub bills private-repository Actions minutes with a multiplier
per platform: Linux ×1, Windows ×2, macOS ×10. One run of this file is:

- `test`: 4 runners × 2 toolchains = 8 jobs — 2 of them macOS (×10), 2 Windows
  (×2), 4 Linux (×1)
- `features`: 2 jobs — 1 Windows (×2), 1 Linux
- `lint`, `docs`, `typos`, `minimal-versions`, `musl`, `coverage`,
  `privileged`: 7 Linux jobs
- `bench`: 2 Linux jobs

so 19 jobs, of which the two macOS legs dominate the bill at ten times their
wall-clock. A `push`+`pull_request` trigger runs the whole file on every commit
to a branch with a PR open; a `schedule` row adds one run a week regardless.

**The decision is the maintainer's and has been made for now: leave it manual until the
base phases ship.** Recorded here so the next person to read the workflow does
not "fix" the missing trigger, and so that every "all gates green" claim in
this project's history is understood for what it is — self-reported by the
agent that wrote the code, never independently re-run.

The job count here and the one in `.github/workflows/test.yml`'s header
comment are one fact written in two places. Change a matrix and both move.
```

Before committing, confirm the two agree:

```bash
grep -n "19 jobs" .github/workflows/test.yml docs/specs/deferred.md
```

must print one line from each file. A single line means one of them was
edited and the other was not, which is the exact state this task exists to
leave the repository out of.

### Step 6.4 — Verify the workflow without running it

There is no cargo command for a YAML file. Use two checks. Both tools are
installed on this machine — PyYAML imports and `actionlint` is at
`/opt/homebrew/bin/actionlint`, both confirmed 2026-08-13 — so neither is
optional and neither has an "if available" escape. An unlinted workflow is
exactly the "correct and ready" claim this task exists to make true.

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/test.yml')); print('parsed')"
```

must print `parsed`.

```bash
actionlint .github/workflows/test.yml
```

must print nothing and exit `0`. If either tool turns out to be missing,
install it (`pip install pyyaml`, `brew install actionlint`) rather than
skipping the check and saying so.

`actionlint` is the one that can actually catch this task's likeliest mistake:
`ubuntu-24.04-arm` is a real GitHub-hosted label and a typo for it — 
`ubuntu-24.04-arm64`, say — is a job that never schedules rather than a job
that fails. Read its output rather than only its exit code.

Then confirm the trigger did not move. **This is the task's own acceptance
check**, so it has to be a check that can actually fail. Two commands:

```bash
awk '/^on:/{f=1;next} /^[a-z]/{f=0} f' .github/workflows/test.yml
```

must print exactly one line, `  workflow_dispatch:` — everything between the
`on:` key and the next top-level key.

```bash
grep -cE '^[[:space:]]+(push|pull_request|schedule):' .github/workflows/test.yml
```

must print `0`. The commented-out trigger block in the header starts each line
with `#`, so `^[[:space:]]+` does not reach it — the count is `0` today and
becomes `1` the moment a real trigger is uncommented.

The first draft of this plan wrote `grep -A 2 "^on:"` here and said it "must
print `workflow_dispatch:` and nothing else". It prints three lines — `on:`,
`  workflow_dispatch:`, `permissions:` — so its stated expectation was already
false at HEAD, which leaves an implementer no way to tell a pass from a fail.

A diff that adds a `push:` line fails the task regardless of everything else
in it.

### Step 6.5 — MUTATION

Uncomment the `push: { branches: [main] }` line from the header block into the
`on:` map — the realistic version of this mistake, since that is where the
text an editor would reach for already sits — and re-run **both** Step 6.4
checks. The `awk` must now print two lines, and the `grep -c` must print `1`.
Revert immediately.

Both have to move. If only the `awk` changes, the `grep -c` regex is wrong for
the indentation the file actually uses and would miss the real thing.

That is the only mutation this task can have — the jobs themselves cannot be
mutation-tested without spending the minutes this task exists to not spend, and
saying so plainly is better than inventing a check that proves nothing.

### Step 6.6 — Gate

Docs and YAML only:

```bash
cargo fmt --all --check
```

and confirm `git diff --stat` touches only `.github/workflows/test.yml` and
`docs/specs/deferred.md`.

---

## Task 7 — IR-20, applied consistently across six error enums

**Closes:** config.md #5 (Minor, multi-file).

Six `pub` error enums carry neither `#[non_exhaustive]` nor a comment
justifying its absence. Two are the audit's originals; four are Phase 9's, which
repeated the gap rather than closing it. `NormalizeError` (`normalize.rs:290`)
has the attribute plus a rationale, and `CronScheduleError` (`cron.rs:183-187`)
explicitly omits it with a comment explaining why — those two are the pattern to
match.

IR-20 says the attribute goes *only where growth is anticipated, with a comment
citing why*, and warns against cargo-culting. So this task does not paste the
attribute six times. It applies one rule, writes the rule down, and lets the
rule decide each case.

**The rule.** A `pub` error enum in a **library** crate (shep-core, shep-daemon,
shep-client) gets `#[non_exhaustive]` and a one-line rationale: those crates are
published, an out-of-tree consumer can match on them exhaustively, and a new
variant would be a breaking change with no version bump to signal it. A `pub`
error enum in **shep-cli** does not: shep-cli is `[[bin]]`-only, has no `lib.rs`
and no published surface, so nothing outside the binary can match on it at all
and the attribute would only tax the crate's own `match`es. Those get the
explicit omission comment `CronScheduleError` already models.

### Files

- **modify** `crates/shep-core/src/config/flockfile.rs:201` — `FlockfileError`
- **modify** `crates/shep-core/src/config/daemon.rs:223` — `DaemonConfigError`
- **modify** `crates/shep-core/src/barks.rs:116` — `BarkError`
- **modify** `crates/shep-daemon/src/dogs.rs:71` — `DogError`
- **modify** `crates/shep-cli/src/dog/bark/sinks.rs:130` — `SinkConfigError`
- **modify** `crates/shep-cli/src/commands/shep_toml.rs:412` — `ShepTomlError`
- **modify** `docs/idiomatic-rust.md` — IR-20's text, so the rule outlives this
  task
- **modify** `crates/shep-core/CHANGELOG.md`, `crates/shep-daemon/CHANGELOG.md`

Line numbers are from the input's re-derivation and will have moved once Tasks
1–6 land; find each enum by name.

### Step 7.1 — The four library enums

`FlockfileError`:

```rust
/// Error type returned from [`Flockfile::parse`]
///
/// `#[non_exhaustive]`: a fifth backend is the named next step for this type
/// — `deferred.md` lists `.js` Flockfiles — and it brings its own rejection
/// reason with it, which must not be a breaking change for a consumer
/// matching on this enum (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlockfileError {
```

`DaemonConfigError`:

```rust
/// Error type returned from [`DaemonConfig::load`]
///
/// `#[non_exhaustive]`: every `[daemon]` key this crate learns to validate
/// brings its own rejection reason, and `deferred.md`'s daemon-config flags
/// layer is a whole set of them at once (IR-20 — the same reasoning
/// [`NormalizeError`](crate::config::NormalizeError) states for the per-app
/// side).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonConfigError {
```

`BarkError`:

```rust
/// … (existing doc unchanged) …
///
/// `#[non_exhaustive]`: shep-core is a published library and this enum is
/// reachable from it, so a third failure shape — a ring whose on-disk format
/// this build does not recognise, say — must not break an out-of-tree
/// consumer's `match` (IR-20).
#[non_exhaustive]
#[derive(Debug)]
pub enum BarkError {
```

`DogError`: its doc comment at `dogs.rs:77` already discusses `DogSource`'s
`#[non_exhaustive]`-ness without ever mentioning its own, which is the tell.

```rust
/// … (existing doc unchanged) …
///
/// `#[non_exhaustive]` on this enum too, and not only on the [`DogSource`] it
/// discusses above: `shep-daemon`'s `dogs` module is `pub`, a dog gains a
/// failure shape every time it gains a source or a config surface, and an
/// out-of-tree consumer matching exhaustively today would face a breaking
/// change the day it does (IR-20).
#[non_exhaustive]
#[derive(Debug)]
pub enum DogError {
```

### Step 7.2 — The two shep-cli enums

`SinkConfigError`:

```rust
/// … (existing doc unchanged) …
///
/// Deliberately NOT `#[non_exhaustive]`, and this is the comment IR-20 asks
/// for in the negative case. shep-cli is `[[bin]]`-only — no `lib.rs`, no
/// published surface — so nothing outside this binary can match on this enum
/// and there is no downstream `match` for the attribute to protect. Adding it
/// would tax only this crate's own exhaustive matches, which are the ones we
/// WANT the compiler to break when a new sink kind arrives. Same reasoning as
/// [`CronScheduleError`](shep_core::config::CronScheduleError)'s own omission,
/// for a different reason: that one is closed, this one is unexported.
#[derive(Debug)]
pub enum SinkConfigError {
```

`ShepTomlError`: same comment, adjusted to name `ShepToml::edit`'s callers.
Note this enum's declaration currently has **no `#[derive(Debug)]` at all**
(`shep_toml.rs:412` reads `pub enum ShepTomlError {` with the doc line above
it) — check whether `Debug` is hand-implemented nearby before adding a derive;
if it is neither derived nor implemented, that is a separate small finding, and
it is fixed in this task with a line in the commit message saying so.

### Step 7.3 — Write the rule down

`docs/idiomatic-rust.md`, IR-20, currently:

```
- **IR-20** `#[non_exhaustive]` only where growth is anticipated, with a
  comment citing why (`ProtocolError`: wire will grow). It taxes downstream
  matching — don't cargo-cult it.
```

Replace with:

```
- **IR-20** `#[non_exhaustive]` only where growth is anticipated, with a
  comment citing why (`ProtocolError`: wire will grow). It taxes downstream
  matching — don't cargo-cult it. The default that settles the usual case: a
  `pub` error enum in a LIBRARY crate (shep-core, shep-daemon, shep-client)
  gets it, because an out-of-tree consumer can match exhaustively and a new
  variant would break them with no version bump to say so; a `pub` error enum
  in shep-cli does not, because the crate is `[[bin]]`-only and its own
  exhaustive matches are the ones we want broken. Either way the comment is
  mandatory — `CronScheduleError` is the model for the negative case. Every
  wire enum gets it unconditionally, and so does `ProcessInfo`, the one wire
  STRUCT (Phase 10, wire audit #1).
```

Add a PR-checklist line, in the fenced block at the bottom:

```
[ ] new pub error enum: non_exhaustive per crate tier + why comment (IR-20)
```

### Step 7.4 — Verify, then MUTATE

**Expect a no-op for every gate, and say so up front rather than discovering
it.** This task's diff is six attributes and six comments, and there is no
check in this repository that can go red on it. That is not a gap in the
task — it is what `#[non_exhaustive]` is.

The reasoning, which is worth stating because the first draft of this plan
claimed the opposite. `#[non_exhaustive]` has **no same-crate effect at all**,
so nothing inside shep-core or shep-daemon can notice. Across a crate boundary
it would break an exhaustive `match`, and there is no such `match`: the only
cross-crate use of any of the four library enums is *construction* of
`DaemonConfigError::Toml` and `DaemonConfigError::BadEnvValue` in shep-cli
(`commands/daemon.rs:583` and `:587`), and constructing a variant of an
enum-level `#[non_exhaustive]` enum from another crate stays legal — the
attribute blocks exhaustive matching, not construction. Confirm it rather than
trusting this paragraph:

```bash
for e in FlockfileError DaemonConfigError BarkError DogError; do
  echo "== $e"
  grep -rn "$e" --include="*.rs" crates/shep-cli/ crates/shep-client/
done
```

Every hit must be a construction, a doc reference or an import — no `match`.
If one is a `match`, that is the one site this task can break, and the fix is
a `_ =>` arm **with a comment saying what it is for**, not a silently
swallowed variant. A cross-crate exhaustive match would be a **compile error**,
not a test failure, so it surfaces in `cargo check` before any test runs.

Run the gates anyway, because "expected to be a no-op" is a claim about the
diff and the gates are how it is checked:

```bash
cargo test --workspace --all-features
```

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Two things the first draft said clippy would catch, and will not.
`clippy::wildcard_enum_match_arm` is a **restriction** lint, off by default and
not enabled in this workspace's lint table, so it fires on nothing here.
`unreachable_patterns` fires on a redundant arm, which adding an attribute
cannot create. Neither is a reason to skip the run — a stray edit in six files
is a real risk — but neither is "the real check for this task" either, because
this task has no real check.

**Mutation.** Delete `#[non_exhaustive]` from `DogError` and run
`cargo clippy --workspace --all-targets --all-features -- -D warnings`. It will
stay green, exactly as Task 2 found for `ProcessInfo`. Record that it stayed
green; a mutation whose expected result is "no change" is still worth running
once, because it is the difference between knowing there is no guard and
assuming there is one.

So this task's mutation is on the *rule*, not the code: change one word in
IR-20's new text from `LIBRARY` to `BINARY` and confirm that the six enums as
shipped now contradict the document. That is a review-time check, not a
compiler one, and stating it as such is the honest version. The compile-fail
guard that WOULD test this belongs with `ProcessInfo`'s, and Task 2 already
built that file; extending it to an error enum is not worth a second crate
boundary for a Minor finding.

### Step 7.5 — CHANGELOGs and gate

`crates/shep-core/CHANGELOG.md` and `crates/shep-daemon/CHANGELOG.md`, under
`Changes`: "`FlockfileError`, `DaemonConfigError`, `BarkError` and `DogError`
are `#[non_exhaustive]`. Match on them with a wildcard arm."

fmt, clippy, doc.

---

## Task 8 — three claims that are not true, and one field that does nothing

**Closes:** the new platform finding (the `ring` claim), config.md #3
(`reuse_port`), platform.md #6's citation fix, and platform.md #4's doc half
(the `openat2` framing). Also refreshes README's test-count sentence, which is
decay rather than a wrong claim.

**Runs after Task 5**, which is where the windows-gnu cross-compile is
actually executed. This task writes what that run found into a permanent code
comment, and a comment asserting a build outcome nobody ran is how the `ring`
claim being fixed here got written in the first place.

**One README item that is already done, and must not be redone.** The audit
input flagged two README cells: the `that'll do` row saying `no`, and a stale
test count. Both were corrected on `main` in `c611853` — *"docs: correct two
README claims that were wrong when written"* — which is this plan's own base
commit, one newer than the commit the audit input was re-derived at.
`README.md:92` already reads `| that'll do | ... | \`shep thatlldo\` | yes |`
and `README.md:183` already reads `1030 tests`. **Do not edit the `that'll do`
row and do not add a grep for it**; an earlier draft of this task did both,
and the grep passed without the implementer touching anything, which is the
precise shape of check this phase exists to stop shipping. What remains is
Step 8.2, and it is about the count decaying, not about it being wrong.

### Files

- **modify** `Cargo.toml` (root, the `tokio-rustls` comment)
- **modify** `crates/shep-cli/Cargo.toml` (the `tokio-rustls` comment)
- **modify** `README.md` (the test-count sentence at `:183` — the lexicon
  table is already correct, see above)
- **modify** `crates/shep-core/src/config/app.rs` (`reuse_port`'s doc)
- **modify** `docs/specs/deferred.md` (`reuse_port`)
- **modify** `crates/shep-daemon/src/runner.rs` (the `openat2` framing, in
  **two** doc comments — `:284` and `:366`)
- **modify** `crates/shep-daemon/src/testing.rs` and
  `crates/shep-daemon/tests/daemon_e2e.rs` (the `sun_path` comment)

### Step 8.1 — The `ring` claim

**Only the root `Cargo.toml` carries the false claim.** Its comment, at lines
183-186, reads *"tokio-rustls + webpki-roots, named directly, costs +10 crates
and none of it needs a C toolchain"* — and the phrase wraps across a line
break between "C" and "toolchain", which matters for Step 8.5's grep.
`crates/shep-cli/Cargo.toml:45-51` says only that `aws_lc_rs` is *"a C build
dependency"*, which is true and needs no correction beyond a pointer. An
earlier draft of this task treated both files as wrong and asserted "three
claims that are not true" partly on that basis; the shep-cli mirror is one of
them only in the sense that it points at a root comment that was.

The claim is false because `Cargo.lock` shows `ring 0.17.14` depending on
`cc 1.4.2`, and ring's `build.rs` runs `cc` to compile C and assembly crypto
primitives for the target. The comparison to `aws_lc_rs` is still real and
still the reason for the choice — aws-lc-sys needs `cmake` on top of a C
compiler — but "none of it needs a C toolchain" is not the difference.

**What the corrected comment may claim, and no more.** Task 5 ran the
cross-compile, so this is measurement rather than inference:

```
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
EXIT=0    Finished `dev` profile ... in 8.42s
```

on a macOS host with `mingw-w64` installed (`x86_64-w64-mingw32-gcc` on
`PATH`). So the honest statement is *"a cross-compile needs a C toolchain for
the target, and with one present the whole workspace cross-compiles clean"* —
**not** "the cross-compile dies for want of mingw", which is what an earlier
draft of this comment said and which nobody had run.

Root `Cargo.toml`, replacing the sentence:

```toml
# Bark's sinks are Discord and Slack webhooks, which are HTTPS, so this is
# the one thing in the workspace that needs TLS. The maintainer's ruling (2026-08-12)
# is a hand-rolled HTTP/1.1 client over tokio-rustls rather than reqwest:
# reqwest's default rustls feature costs +93 crates over this workspace's
# existing 196 and pulls aws-lc-sys, which needs BOTH a C compiler and cmake;
# tokio-rustls + webpki-roots, named directly, costs +10 crates and needs a C
# compiler but no cmake. `ring`, not the default `aws_lc_rs`, is the crypto
# provider, and tokio-rustls's own default features pull aws_lc_rs in unless
# named away.
#
# What this DOES cost, stated plainly because an earlier version of this
# comment claimed the opposite: ring's build.rs uses `cc` to compile C and
# assembly for the target, so building shep-cli for a target needs a C
# compiler for THAT target. Native builds are unaffected — macOS ships clang,
# the CI musl job installs musl-tools, and GitHub's windows-latest runners
# ship the MSVC build tools ring wants for windows-msvc. A CROSS-compile is
# where it is felt: `--target x86_64-pc-windows-gnu` from macOS needs
# mingw-w64 installed. Measured 2026-08-13 with it installed, the whole
# workspace cross-compiles clean —
#   cargo check --workspace --all-targets --all-features \
#     --target x86_64-pc-windows-gnu     # EXIT=0, 8.42s
# — and that check is back in CLAUDE.md's gate section after three phases
# out. shep-daemon has no ring in its tree, so the Linux cross-check beside
# it needs no cross C toolchain and is scoped to `-p shep-daemon` for exactly
# that reason.
tokio-rustls = { version = "0.26", default-features = false, features = ["ring", "tls12"] }
```

The backslash continuation inside a TOML comment is only there to keep the
line under the file's width; it is a comment, so nothing parses it.

`crates/shep-cli/Cargo.toml`, the shorter mirror. Its existing text is not
wrong — it says `aws_lc_rs` is "a C build dependency", which it is — so this
is a pointer added, not a claim corrected:

```toml
# `dog::bark::sinks`'s TLS transport for Discord/Slack webhooks. The maintainer's ruling
# (2026-08-12): a hand-rolled HTTP/1.1 client over `tokio-rustls`, not
# `reqwest` — see the root `Cargo.toml` entry for the full accounting,
# including what ring's own `cc` build dependency costs a cross-compile.
# `ring`/`tls12` name the crypto provider directly so `tokio-rustls`'s own
# default features (which pull in `aws_lc_rs`, which needs cmake on top of a
# C compiler) never apply.
tokio-rustls.workspace = true
```

**Verification, not assertion.** Before writing either comment, confirm the
dependency edge still holds at this HEAD:

```bash
grep -n -A 8 '^name = "ring"' Cargo.lock
```

must show `cc` among its dependencies. If it does not, the claim has changed
and this whole step needs rewriting rather than editing.

### Step 8.2 — README's test count, which decays every phase

**One edit, not two.** The `that'll do` row at `README.md:92` already reads
`yes` and the count at `README.md:183` already reads `1030` — both corrected in
`c611853`, this plan's base commit. Neither was missed; they were fixed one
commit before the plan was written. Leave the lexicon table alone.

What is left is not a wrong claim but a decaying one. `README.md:183` reads:

```markdown
**Tested by trying to break it.** 1030 tests, and every task ends with a
mutation pass: break a line on purpose, confirm a test goes red, put the line
back. It keeps turning up tests that could not fail, which is the reason to
do it.
```

`1030` is correct today and stops being correct the moment Task 1 adds its
first test. It has already been re-corrected once. Rather than re-stating a
number that goes stale every phase, make the sentence say what it is actually
claiming:

```markdown
Over a thousand tests, and the ones worth having are the ones that were made
to fail on purpose: every phase ends with a mutation pass that breaks a named
line and checks the right test goes red. Five phases have turned up tests that
could not fail — including one that was mathematically incapable of it.
```

That is honest, does not decay, and says the more interesting thing. Run the
`humanizer` skill over it before committing — README copy is public-facing prose
and the existing README's voice is the sample to match.

### Step 8.3 — `reuse_port`

`AppConfig::reuse_port` (`app.rs:171`) has zero production readers: grep across
`crates/shep-daemon/src` returns nothing, and reload overlap happens
unconditionally. Its own doc comment currently says *"shep's contribution is
permission for the old and new instance to overlap during reload"* — which
describes behaviour shep performs whether the field is set or not, so the field
reads as gating something it does not gate.

Confirm:

```bash
grep -rn "reuse_port" crates/shep-daemon/src/
```

must return nothing. If Phase 10's own tasks introduced a reader, this step is
void.

Keep the field — `shep import` writes it (`import/convert.rs:187`), `shep flock`
displays it (`output/rows.rs:720-758`), and deleting it would silently drop a
value out of an imported config. Correct its doc's last paragraph:

```rust
    /// **This field is inert today.** shep never reads it: reload overlap
    /// already happens unconditionally, so setting it changes nothing and
    /// leaving it unset costs nothing. It is kept because `shep import`
    /// writes it for a cluster-mode pm2 app and `shep flock` displays it, so
    /// dropping it would silently discard a value out of an imported config.
    /// It becomes load-bearing the day shep gains a reload mode that does NOT
    /// overlap by default, which is when the permission it describes stops
    /// being free — see `docs/specs/deferred.md`.
    pub reuse_port: bool,
```

And in `docs/specs/deferred.md`, as a `###` entry under the
**"Known debt, recorded rather than built (Phase 10)"** section Task 6
creates. If Task 6 has not run yet, create the section with the two-sentence
preamble Step 6.3 specifies rather than inventing a second home for it —
`deferred.md` has exactly four `##` headings today and none of them is
"things that exist but do nothing yet":

```markdown
### `reuse_port` is accepted, stored, displayed — and never read

`AppConfig::reuse_port` has no production reader anywhere in the workspace.
Reload's overlap between the old and new instance is unconditional, so the
permission this field grants is one shep already takes.

Kept rather than removed: `shep import` sets it for a cluster-mode pm2 app and
`shep flock` renders it, so deleting the field would silently drop a value out
of a config an operator handed us. It costs one `bool` per app.

It stops being inert the day shep grows a reload mode that does not overlap by
default — a `graceful = false` or a serial reload — at which point this is the
field that says which apps may be overlapped. Until then the doc comment on the
field says plainly that it does nothing, which is the part that was missing.
```

### Step 8.4 — Two comments that overstate their case

**`openat2`, in two comments and not one.** The framing appears twice in
`runner.rs` and the plan's first draft found only the second. `:284`, on
`open_log_path`, says `openat2(RESOLVE_NO_SYMLINKS)` *"is Linux-only and so
out of scope for a project with macOS as a tier-1 platform (spec §11)"*.
`:364-369`, on `check_log_ancestry`, says *"the syscall that would provide one
(`openat2(RESOLVE_NO_SYMLINKS)`) is Linux-only while macOS is tier-1"*. Both
read as a reason the syscall is unavailable to this project. It is not — being
Linux-only is a reason it cannot be the *only* path, not a reason it cannot be
a fast path beside the portable one. Confirm both sites before editing:

```bash
grep -n "openat2" crates/shep-daemon/src/runner.rs
```

must print two lines. Narrow both claims to what is true, and name the real
cost. `:284`'s is short, since the long version belongs on
`check_log_ancestry` where the design note goes:

```rust
/// that in the open itself needs `openat2(RESOLVE_NO_SYMLINKS)`, which is
/// Linux-only — so it cannot be the only path here, though it could be a
/// Linux fast path beside this one. What that would cost, and why Phase 10
/// did not spend it, is on [`check_log_ancestry`] and in
/// `docs/specs/deferred.md`.
```

and `:364-369`'s, the full version:

```rust
/// # What remains
///
/// A TOCTOU window. This checks the ancestry and then opens the path with no
/// atomic tie between the two, so an attacker who can rearrange a directory
/// between the check and the open still wins that race. The bar is raised
/// substantially; the operation is not atomic.
///
/// The syscall that would close it on Linux is
/// `openat2(RESOLVE_NO_SYMLINKS)`, and it IS reachable — `nix 0.29` exposes
/// `fcntl::openat2` under the `fs` feature this crate already enables. What
/// stops it being a Linux fast path here is not availability but cost:
/// `openat2` hands back a `RawFd`, so adopting it into a `File` needs
/// `FromRawFd`, which is `unsafe` and belongs in `sys.rs` (IR-22), behind a
/// `cfg(target_os = "linux")` with an `ENOSYS`/`EPERM` fallback ladder for
/// pre-5.6 kernels and seccomp sandboxes — new unsafe on a Linux-only path
/// this project cannot execute a test for from a macOS development machine.
/// The design is written down in `docs/specs/deferred.md` rather than
/// half-built here.
```

**`sun_path`.** `crates/shep-daemon/src/testing.rs:220-223` says *"sun_path caps
a socket path near 104 bytes"*, which is macOS's number; Linux allows 108. The
same comment appears at `crates/shep-daemon/tests/daemon_e2e.rs:60-61`. (The
audit's second citation, `boot.rs:1699`, no longer resolves to that content —
it is now inside an unrelated muster-restoration test. Use the `daemon_e2e.rs`
line.) Correct both to:

```rust
// `sun_path` caps the socket path at 104 bytes on macOS and 108 on Linux, and
// macOS temp paths are already long — so the tighter number is the one to
// build against, and it is the platform this runs on most.
```

No behaviour changes; `boot.rs:357`'s `bind_socket` still surfaces an
over-length `$SHEP_HOME` as a raw `ENAMETOOLONG`, which is recorded in Task 9
rather than fixed here.

### Step 8.5 — MUTATION

This task is comments and doc prose, so the mutations are grep-shaped and that
is stated rather than dressed up. What matters is that each grep can print a
different answer before and after the edit — the first draft's version could
not, and that is the defect being fixed here as much as any comment is.

**The `ring` claim.** The false phrase in the tree is *"none of it needs a C"*
/ *"toolchain."* — it wraps across `Cargo.toml:183-184`. `grep -c "no C
toolchain"`, which the first draft used, matches nothing before the edit and
nothing after it: the words are "none of it needs", not "no", and the phrase
spans a newline so even `grep "C toolchain"` misses it. It printed `0` for
both files at HEAD and would have printed `0` had the task never run. Two
discriminating checks instead, and only against the root file, which is the
only one that ever carried the claim:

```bash
grep -c "none of it needs" Cargo.toml
```

must print `0` — it prints `1` at HEAD, so this one goes from `1` to `0` and
is the check that the false sentence is gone.

```bash
grep -c 'uses `cc`' Cargo.toml
```

must print `1` — the corrected comment's own sentinel, `0` at HEAD. Two
directions, so a task that deleted the old sentence without writing the new
one fails the second, and a task that added the new one while leaving the old
fails the first.

Then the mutation proper: re-introduce `none of it needs a C` in
`Cargo.toml`'s comment and confirm the first grep prints `1`. Revert.

**`reuse_port`.**

```bash
grep -rn "reuse_port" crates/shep-daemon/src/
echo "exit=$?"
```

must print `exit=1` (no matches) — `grep` exits `1` when nothing matches, so
this one does discriminate. If a later phase adds a reader, that grep starts
finding one and the doc comment written here becomes false — which is what the
deferred.md entry in Step 8.3 says out loud.

**`openat2`.**

```bash
grep -c "out of scope for a project with macOS" crates/shep-daemon/src/runner.rs
```

must print `0`; it prints `1` at HEAD. That is the `:284` half, which the
first draft of this task never touched.

**No README grep.** The first draft asserted
`grep -n "that'll do" README.md` "must show the row ending in `| yes |`" — it
does, and it did before the plan existed, because `c611853` had already fixed
it. A check that passes identically whether or not the task ran is worse than
no check: it reports success for work that was never done. Step 8.2's edit is
a prose rewrite and its verification is reading the sentence.

### Step 8.6 — Gate

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

The `doc` run is the one that matters here: three of these edits are rustdoc
comments with intra-doc links, and a broken link is a `-D warnings` failure.

---

## Task 9 — the testing rule, and the deferral ledger

**Closes:** tests.md #6 (Minor, one-line), and records every finding this phase
deliberately does not build.

### Files

- **modify** `docs/idiomatic-rust.md` — a new IR-46 and a checklist line
- **modify** `docs/specs/deferred.md` — seven ledger entries, appended to the
  **"Known debt, recorded rather than built (Phase 10)"** section Task 6
  creates and Task 8 has already added `reuse_port` to

### Step 9.1 — IR-46, the forcing mechanism

tests.md #6: a test that fails only by hanging has now been found in five
phases. Phase 9 hit it twice — Task 15's metrics dispatch test hung forever once
a real signal-blocking server started, and Phase 9 Task 5's `await_status(Online)`
resolved on pre-crash state, a vacuous pass rather than a hang but the same root
cause. No mechanical guard exists: grepping `docs/idiomatic-rust.md` and
`CLAUDE.md` for "forcing mechanism" or anything like it returns nothing. The fix
the audit asks for is one line in the guidance that already exists.

After IR-40, in section H:

```
- **IR-46** Every `await` in a test needs a FORCING MECHANISM — something that
  makes it resolve, or makes it fail, within a bound the test itself sets. Two
  failure shapes this catches, both found in five separate phases: an await
  nothing will ever resolve (the test hangs, and a hang reports as a timeout
  minutes later with no diagnostic), and an await that resolves on a state the
  test was not waiting for (`await_status(Online)` satisfied by the state
  BEFORE the crash under test — a vacuous pass, which is worse than a hang
  because it is green). Concretely: wrap the wait in `tokio::time::timeout`
  and assert on the result, or wait for a transition rather than a state, or
  drive a paused clock past the point the thing must have happened by. "The
  test passes locally" is not a forcing mechanism; neither is the harness's
  own process timeout, which fails the whole binary and names nothing.
```

Checklist line, in the fenced block:

```
[ ] every await in a test has a forcing mechanism, not just a hope  (IR-46)
```

### Step 9.2 — The ledger

`docs/specs/deferred.md`, appended as `###` entries to the
**"Known debt, recorded rather than built (Phase 10)"** section — the one
Task 6 creates and Task 8 has already put `reuse_port` under. Do not invent a
section name; `deferred.md` has four `##` headings and none of them is "the
section that records deliberate deferrals", which is what an earlier draft of
three separate tasks said and which three separate implementers would have
guessed three different ways.

Each entry says what, why not now, and what would force it.

**Seven entries: the phase-opening list's six, plus one record.** The
phase-opening "Findings deliberately NOT built" list and this ledger must name
the same set, so cross-check them before writing — an earlier draft promised
entries for two items that never got one and wrote an entry for an item that
was not on the list.

Six come straight off that list: config #4 (`DaemonConfig`), wire #6
(`ProcessInfo`'s four concerns), platform #4 (`check_log_ancestry`'s TOCTOU),
`bind_socket`'s raw `ENAMETOOLONG`, platform #2 (reload's Linux-only
assertions), and tests #3 (the `cli_e2e` correlation).

Two items on that list get **no** entry, and the list now says so in place:
platform #5 (already recorded accurately at `deferred.md:91-101`, inside the
openrc/BSD paragraph — a second copy is a second thing to drift) and tests
#5's non-CI half (nothing is deferred; the test is correct and Task 6 ships
the job that runs it).

The seventh is not a deferral at all. It is a dated record of the windows-gnu
gate's three-phase absence, written down because Task 5 closed it and the fact
that it lapsed unnoticed is the part worth keeping.

```markdown
### `DaemonConfig` is not a proof token, unlike `ResolvedApp`

`ResolvedApp` keeps its `config` private so that holding one proves it went
through `normalize` (`normalize.rs:63`). `DaemonConfig` does not: its `daemon`
and `dog` fields are `pub`, and the one validation it performs — the
`max_cron_sleep` floor — happens inline inside `DaemonConfig::load`
(`daemon.rs:203-210`) rather than in a `validate` step a hand-built value would
also have to pass.

Nothing constructs one by hand today outside tests, so nothing is currently
wrong. Deferred because making the fields private and splitting `validate` out
of `load` is an architectural call on a type whose shape is the maintainer's to decide,
not a defect with a known fix. What would force it: any production path that
assembles a `DaemonConfig` from something other than a file — the daemon-config
flags layer, for instance.

### `ProcessInfo` fuses four concerns behind one discriminator

Identity and lifecycle (`id`, `name`, `status`, `pid`, `restarts`,
`uptime_ms`, `fold`), log paths (`out_file`, `err_file`), resource stats
(`cpu_percent`, `memory_bytes`) and dog provenance (`dog`) all ride in one
struct, and a dog's row leaves several of them meaningless.

Deferred on the wire audit's own recommendation: do not split speculatively.
What would force it is the `lambs` field — the moment a row carries a process
tree, the question of what a `FlockMember` is stops being cosmetic. Phase 10
made that field cheap to add (`ProcessInfo` is `#[non_exhaustive]` with a
builder), which is deliberately the opposite of forcing the split early.

### `check_log_ancestry`'s TOCTOU window, and the Linux syscall that would close it

`check_log_ancestry` verifies a log path's ancestry and `open_log_path` then
opens it, with no atomic tie between the two. The realistic local-multiuser
attack is caught — a loose or wrong-owned ancestor is refused, and
`O_NOFOLLOW` refuses a symlink standing at the final component — but an
attacker who can rearrange a directory between the check and the open still
wins that race.

The design, written down so it does not have to be rediscovered:

- Linux fast path: `nix::fcntl::openat2` (available under the `fs` feature this
  crate already enables) with `ResolveFlag::RESOLVE_NO_SYMLINKS`, opening
  relative to a directory fd for the log directory.
- The `RawFd` it returns is adopted into a `File` with `FromRawFd`, which is
  `unsafe`, so the wrapper lives in `shep-daemon/src/sys.rs` with a per-block
  `// SAFETY:` (IR-22/23) and nothing else in the crate touches the raw fd.
- Fallback ladder: `ENOSYS` (kernel < 5.6) and `EPERM` (seccomp filters that
  do not allow the syscall) both fall through to today's
  check-then-`O_NOFOLLOW`-open path, which stays as the portable
  implementation and remains the only path on macOS.

Not built in Phase 10 because it is new `unsafe` on a Linux-only path that this
project cannot execute a test for from a macOS development machine — the exact
shape of debt the platform audit's "never been compiled" finding exists to
complain about. What would force it: a Linux box in the regular test loop, or a
threat model that includes an attacker with write access to a log directory's
parent.

### `bind_socket` reports an over-length `$SHEP_HOME` as a raw `ENAMETOOLONG`

`sun_path` caps a unix socket path at 104 bytes on macOS and 108 on Linux.
`boot.rs`'s `bind_socket` performs no length check of its own, so an operator
who sets `$SHEP_HOME` unusually deep gets the OS error with no sentence saying
which limit was hit or which variable to shorten. Low impact — it takes a
deliberately deep path — and the fix is a length check plus a friendly message,
which is worth doing the next time that file is open.

### Reload's Linux-only assertions have no automatic execution

`daemon_e2e.rs:1892` and `:1948` carry `#[cfg(target_os = "linux")]` on the
reload connection-count assertions, which is correct: they depend on Linux's
accept balancing. Their only real execution to date was one manual Docker run.
Phase 10 added the `ubuntu-24.04-arm` and `ubuntu-latest` legs that would run
them, but the workflow stays `workflow_dispatch`-only, so they still execute
only when someone presses the button. Recorded so the gap is known, not because
the tests are wrong.

### The `cli_e2e` 7-test correlation

Four of nine `cli_e2e` tests in one grouping failed under `--test-threads=1`
where zero of six in another did — investigated twice, exonerated twice as a
load artefact rather than a regression, and never measured again since Phase 6.
It is a standing false-positive risk in the serial phase-gate run that CLAUDE.md
mandates before a merge. What it needs is one fresh bounded measurement pass
with the numbers written down, which is a measurement rather than an edit, and
is why it is here rather than in a task.

### The windows-gnu cross-check went three phases unrun

`cargo check --workspace --all-targets --all-features --target
x86_64-pc-windows-gnu` was in the gate list of every plan from Phase 3 through
Phase 6. Phase 7's plan does not carry it, nor Phase 8's, nor Phase 9's, and
no plan says why — it was dropped silently. It had also never been written into
`CLAUDE.md`'s own gate section, so there was nothing outside the plans to
notice its absence.

This one is **closed, not deferred**, and is recorded here only so the gap is
dated. Phase 10 ran it (`EXIT=0`, 8.42s, 2026-08-13, at `b7c466b`) and put it
back, in `CLAUDE.md` this time rather than in a plan that expires. The likely
reason it lapsed is its prerequisite: `ring`'s build script runs `cc` for the
target, so the check needs a C toolchain for `x86_64-pc-windows-gnu`
(`mingw-w64`), and a host without one cannot run it at all — an easy thing to
stop doing and never mention. Windows was 0% implemented for all three of
those phases, so nothing broke; what was lost was the guarantee that nothing
had.

It is spelled `cargo check`, not `clippy -- -D warnings`, and that is a
decision rather than an oversight: shep-daemon's `boot`, `sys`, `server` and
`tokio_runner` are `cfg(unix)`-gated, so the Windows target reports 51
dead-code warnings for code that is not dead on any platform shep ships.
Silencing them would mean `#[allow(dead_code)]` on live code.
```

### Step 9.3 — Verify and MUTATE

Docs only:

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

(nothing in this task is a rustdoc comment, so this is a no-op guard against a
stray edit), and:

```bash
grep -c "IR-46" docs/idiomatic-rust.md
```

must print `2` — the rule and the checklist line.

**Mutation:** this task's product is a checklist, and the honest mutation is to
use it. Walk every test written in Tasks 1–6 that awaits anything and check it
against IR-46 as written:

- Task 3's `a_child_with_a_channel_is_told_which_channel_it_is` and its
  no-channel sibling — bounded by `await_file_contents`'s own
  `LOG_WRITE_DEADLINE`. Passes.
- Task 3's three `ActionWaits` tests — synchronous, one `blocking_recv` on a
  channel whose sender is held by the test itself. Passes, but confirm the
  `blocking_recv` really is reachable: a `blocking_recv` on a tokio runtime
  thread panics, and the first test is a plain `#[test]` for that reason.
- Task 5's rewritten handshake test — bounded by `HANDSHAKE_TIMEOUT`, which is
  the thing under test. Passes.
- Task 6 and Tasks 7-9 add no test that awaits anything.

And one thing the rule does not reach, worth saying while the checklist is
open: IR-46 bounds an `await`, and the failure Task 2 and Task 8 each nearly
shipped is a **grep that returns the same answer before and after the edit**.
Different shape, same defect — a check that cannot distinguish done from not
done. There is no rule for it here, and adding a second one in the same edit
would dilute IR-46; but note the pattern in the commit message so the next
audit has the phrase to look for.

Any test that fails this walk is IR-46's first catch in the very phase that
wrote it, and the fix is to bound it before the phase closes. Record the
outcome either way — a checklist rule that never fires on the code written
beside it is a rule nobody applied.

---

## Exit criteria

All four gates green, each run from its own command with `$?` captured
directly, one cargo command at a time:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

plus the phase gate:

```bash
cargo test --workspace --all-features -- --test-threads=1
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

and both `benches/` gates (own `CARGO_TARGET_DIR`, own workspace).

The windows-gnu line is back after three phases out (Task 5). It needs
`mingw-w64` on the host and it is `cargo check`, not clippy — both for reasons
`CLAUDE.md` now carries.

Expected shape, not a checksum: baseline 1030 passed / 0 failed / 3 ignored
across 15 result lines, plus roughly

- Task 1: +6 (3 normalize, 3 `kill_signal`)
- Task 2: +2 in shep-core, +1 new integration test file
  (`process_info_builder_from_outside_the_crate`) → **16 result lines**
- Task 3: +3 `ActionWaits` unit tests, +3 channel fixtures, +1 `real_runner`
- Task 5: no new tests, one rewritten
- Tasks 4, 6–9: no new test functions (fixtures grow inside existing tests)

so about **1046 passed / 0 failed / 3 ignored across 16 result lines**. Recount
at the merge; do not carry this number into a brief.

Additionally, and each of these is written so that it prints a *different*
answer if the thing it guards is wrong — the phase's own recurring defect is a
check that passes either way:

```bash
find crates/shep-core/src/protocol/snapshots -name '*.snap.new' | wc -l   # 0
```

```bash
awk '/^on:/{f=1;next} /^[a-z]/{f=0} f' .github/workflows/test.yml          # "  workflow_dispatch:"
grep -cE '^[[:space:]]+(push|pull_request|schedule):' .github/workflows/test.yml   # 0
```

**A Phase 10 diff that enables CI fails the phase.**

```bash
grep -c "none of it needs" Cargo.toml    # 0   (1 at HEAD — the false claim)
grep -c 'uses `cc`' Cargo.toml           # 1   (0 at HEAD — the corrected one)
```

```bash
grep -n "PROTOCOL_VERSION" crates/shep-core/src/protocol/mod.rs
```

must still print `pub const PROTOCOL_VERSION: u32 = 1;` — it is a `u32`, not
the `u16` an earlier draft assumed, and the point is the `1`.

```bash
grep -n "19 jobs" .github/workflows/test.yml docs/specs/deferred.md
```

must print one line from each file (Task 6's paired count).

And, last: every task's mutation step was run and its named test went red for
its named reason. A mutation that did not redden is a finding, not a formality
— five phases running, this project has turned one up every time. Three of
this phase's mutations are *expected* not to redden and say so in place
(Task 2's `#[non_exhaustive]` deletion, Task 7's `DogError` deletion, Task 4's
enum-reorder); those three are recorded as "stayed green, as designed", and
nothing else may be.
