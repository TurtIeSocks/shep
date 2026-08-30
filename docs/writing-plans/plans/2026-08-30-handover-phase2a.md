# Daemon handover, phase 2a: the spine

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shep daemon reload` replaces the shepherd in place, keeping simple sheep running on their original pids with no gap in their logs, and falls back to the stop arm for any flock carrying something this phase cannot yet move.

**Architecture:** The daemon `execve`s its own binary path. Descriptors that must live through that have `FD_CLOEXEC` cleared deliberately and their numbers written into a handover blob; the successor reads the blob from an environment variable, adopts the descriptors by number, and rebuilds every Rust-side object around them. Adopted sheep have no `tokio::process::Child`, so they are reaped by a second, strictly targeted `waitpid` path that runs alongside tokio's own.

**Tech Stack:** Rust 2024, MSRV 1.88. `nix` for `fcntl`, `execv` and `waitpid`. `sysinfo`, already in shep-daemon, for process start times. No new dependencies.

**Spec:** [docs/brainstorming/specs/2026-08-29-daemon-handover-design.md](../../brainstorming/specs/2026-08-29-daemon-handover-design.md), sections H1, H2 and H2a.

## What 2a deliberately refuses

A half-built handover that silently mishandles an app is worse than no handover. Phase 1 shipped `Arm::for_daemon`, which already chooses between a handover and a stop-and-start, so this phase adds a fitness check and **falls back to the stop arm** for anything it cannot carry:

- a sheep with a shepherd channel (`channel`, `wait_ready`, or `shutdown_with_message`)
- a sheep with `stdin = true`
- any dog
- an app with `instances > 1`
- any in-flight reload
- any pending manual stop or delete

Those are 2b's and 2c's. The fallback is correct behaviour, not a stub.

## Global Constraints

- MSRV 1.88, edition 2024. **No new crate dependencies in any task.**
- The whole phase is `#[cfg(unix)]`. Windows has no `execve`; `Arm::for_daemon` already returns the stop arm there and must keep doing so.
- Unsafe is permitted ONLY in `crates/shep-daemon/src/sys.rs`, with a per-block `// SAFETY:` comment (IR-22/23). `fcntl`, `execv` and `waitpid` go through `nix`, which is safe-wrapped; if any raw call is unavoidable it lives in `sys.rs` and nowhere else.
- Every new public item needs a doc comment, a `# Errors` section if it returns `Result`, and a deliberate `Debug` decision. **The handover blob carries no environment values and no secrets**; if a type could ever hold one, redact it with an exact-string test (IR-41).
- `core::error::Error`, never `std::error::Error`.
- **Invoke the `shep-idiomatic-rust` skill before writing any Rust.** Cite `IR-<n>` in review.
- **One cargo shape per task.** Daemon-side work uses `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`. Anything crossing into shep-cli uses `cargo test --workspace --all-features`. Never alternate within a task.
- Task gate, once per task, each from its own command with `$?` captured directly and never through a pipe: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`.
- **The Windows cross-check is part of the PER-TASK gate in this phase, not just the phase gate.** Run it with its own target dir so it does not invalidate the host cache:

  ```bash
  CARGO_TARGET_DIR=/tmp/xcheck-win cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
  ```

  This is not the usual convention and it is here for a measured reason. Task 2 added `crates/shep-daemon/src/handover/fds.rs`, which uses `nix` and `std::os::fd`, and wired the module into `lib.rs` with no `#[cfg(unix)]`. That broke the Windows build, and the host-only task gate reported green for a whole task before Task 3 caught it. Every file in this phase is unix-only, so the gate that would notice has to run every time, not at the end.
- Repo-relative paths only, in code, comments and commit messages. Never an absolute local path. Do not name any person.

## The failure this phase exists to avoid

Losing a sheep's stdout read end does **not** lose its output. The child blocks on `write()` once the 64KiB pipe buffer fills, and hangs. It reads as an application bug, not a shep bug. Every descriptor decision in this plan is subordinate to that.

## File Structure

| File | Responsibility |
|---|---|
| `crates/shep-daemon/src/handover/mod.rs` | new module: the blob type, fitness check, and the exec |
| `crates/shep-daemon/src/handover/fds.rs` | clearing `FD_CLOEXEC`, and adopting a descriptor by number |
| `crates/shep-daemon/src/handover/adopt.rs` | rebuilding readers, the listener, and log handles from raw fds |
| `crates/shep-daemon/src/handover/reap.rs` | targeted reaping for adopted pids |
| `crates/shep-daemon/src/boot.rs` | successor detection at boot, and the SIGHUP arm |
| `crates/shep-cli/src/commands/daemon.rs` | `Arm::Handover` becomes constructible |

---

### Task 1: decide whether this flock can be carried

**Files:**
- Create: `crates/shep-daemon/src/handover/mod.rs`
- Test: same file

**Interfaces:**
- Produces: `pub enum Fitness { Carryable, Refused(RefusedReason) }` and `pub fn fitness(sheep: &[&ProcessEntry]) -> Fitness`. Task 8 consumes it.

This gate is what makes every later stage safe to ship. Get it wrong in the permissive direction and a half-built handover corrupts a flock; wrong in the strict direction and it merely falls back to a working stop-and-start.

**Refuse when ANY sheep has**: a shepherd channel, `stdin = true`, a `dog` marker, `instance` implying a multi-instance app, a non-`None` `reload` marker, or a pending manual/delete. Read the real field names off `crates/shep-daemon/src/entry.rs` before writing the match; do not trust these names.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn a_plain_sheep_is_carryable() {
    let e = entry_fixture();          // no channel, no stdin, not a dog, instance 0
    assert_eq!(fitness(&[&e]), Fitness::Carryable);
}

#[test]
fn one_unsupported_sheep_refuses_the_whole_flock() {
    // Not per-sheep. The blob describes one process image, so a flock is
    // carried whole or not at all.
    let plain = entry_fixture();
    let channelled = entry_with_channel();
    assert!(matches!(fitness(&[&plain, &channelled]), Fitness::Refused(_)));
}

#[test]
fn the_refusal_names_which_sheep_and_why() {
    // The operator sees this in `shep daemon reload`'s output, so it has to
    // say what to do about it, not just that it declined.
    let Fitness::Refused(r) = fitness(&[&entry_with_channel()]) else { panic!() };
    let text = r.to_string();
    assert!(text.contains("shepherd channel"), "{text}");
}

#[test]
fn an_empty_flock_is_carryable() {
    assert_eq!(fitness(&[]), Fitness::Carryable);
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: fitness`
Expected: FAIL, unresolved `fitness`

- [x] **Step 3: Implement**

`RefusedReason` carries the sheep's name and the feature that blocked it, and its `Display` names the fallback:

```rust
/// Why a flock cannot be handed over in place, and what happens instead.
///
/// Every variant is a feature phase 2a does not yet carry, not an error. The
/// caller falls back to the stop arm, which is correct behaviour rather than
/// a degraded one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefusedReason {
    /// The sheep holds a shepherd channel, whose socketpair 2b carries.
    Channel { sheep: String },
    ...
}
```

`#[non_exhaustive]` here, unlike `Shepherd` in `boot.rs`: this set genuinely grows as 2b and 2c widen what is carryable, which is the opposite of that enum's closed-by-mechanism argument.

- [x] **Step 4: Run to verify they pass**

- [x] **Step 5: Task gate, then commit**

---

### Task 2: clear `FD_CLOEXEC`, and prove it

**Files:**
- Create: `crates/shep-daemon/src/handover/fds.rs`
- Test: same file

**Interfaces:**
- Produces: `pub fn keep_across_exec(fd: BorrowedFd<'_>) -> io::Result<()>` and `pub fn is_kept(fd: BorrowedFd<'_>) -> io::Result<bool>`.

Everything the daemon holds is close-on-exec by default, verified against pinned `mio`, `tokio`, `std` and `command-fds` sources. `command-fds` is the one place in the tree that already clears the flag deliberately, for the shepherd channel's fd 3; read `command-fds-0.3.3/src/lib.rs` for the shape before writing this.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn a_fresh_pipe_is_close_on_exec_and_can_be_kept() {
    let (r, _w) = std::io::pipe().unwrap();
    // Establishes the default this whole phase depends on. If this ever
    // fails, std changed and the fd inventory needs re-auditing.
    assert!(!is_kept(r.as_fd()).unwrap(), "std pipes are CLOEXEC by default");
    keep_across_exec(r.as_fd()).unwrap();
    assert!(is_kept(r.as_fd()).unwrap());
}

#[test]
fn keeping_is_idempotent() {
    let (r, _w) = std::io::pipe().unwrap();
    keep_across_exec(r.as_fd()).unwrap();
    keep_across_exec(r.as_fd()).unwrap();
    assert!(is_kept(r.as_fd()).unwrap());
}
```

`nix::unistd::pipe()`, as originally written above, wraps the raw `pipe(2)`
syscall and is not close-on-exec by default — only `std::io::pipe()` (stable
since Rust 1.87, this workspace runs 1.88) matches the "std pipes are
CLOEXEC by default" assumption the comment states and the phase's descriptor
inventory depends on. Swapped in both tests before running them.

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: fds::tests`
Actual: FAIL, unresolved `is_kept`/`keep_across_exec` (E0425 ×6)

- [x] **Step 3: Implement**

Read the existing flags with `F_GETFD`, clear only the `FD_CLOEXEC` bit, write back with `F_SETFD`. **Do not write a bare `0`**: that would clobber any other flag the descriptor carries, and it makes the call non-idempotent in a way the second test above is written to catch.

- [x] **Step 4: Run to verify they pass**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: fds::tests`
Actual: 2 passed

- [x] **Step 5: Task gate, then commit**

---

### Task 3: the handover blob

**Files:**
- Modify: `crates/shep-daemon/src/handover/mod.rs`
- Test: same file

**Interfaces:**
- Produces: `pub struct Handover { version: u32, sheep: Vec<CarriedSheep>, listener_fd: RawFd, pidfile_fd: RawFd, next_id: u32, next_deadline: u64, next_action_stamp: u64 }` and `CarriedSheep { id, name, instance, pid, restarts, epoch, status, last_exit, credentials, fds, app }`.

**`started_at` is deliberately NOT carried.** It is a `tokio::time::Instant` with no epoch, so it cannot be serialized. Re-derive it in the successor from the operating system, which is authoritative and more correct than carrying it: `sysinfo` is already a shep-daemon dependency and exposes a process start time. Read `crates/shep-daemon/src/limits/sample.rs` for how the crate already drives `sysinfo` before adding a second style.

The blob carries **each sheep's whole resolved spec, environment included**. This reverses what this task originally specified, and the reversal is the maintainer's, recorded in the design spec's H2. The muster roll already persists every sheep's env in cleartext and permanently -- `SavedApp.app` is a whole `AppConfig`, `AppConfig.env` is a plain `BTreeMap<String, String>` with no skip attribute, and `flock.json` is written at `0600` -- so a blob carrying the same values at the same mode, on a file the successor unlinks the moment it has read it, is strictly less exposure than the file already sitting there for the life of the flock. Refusing to carry it forced the successor to rebuild each spec from the roll and bind carried sheep to roll apps by name and instance, except the roll records a running COUNT per app rather than which slots were up, and `muster` starts what it restores: a second source of truth that can disagree with the blob, to protect a value already on disk.

`CarriedSheep.app` is the `AppConfig` beneath `ProcessEntry.spec`'s `ResolvedApp`, not the `ResolvedApp` itself. That type is a proof token obtainable only through `normalize`, so a `Deserialize` for it would mint the token from arbitrary JSON for every consumer of shep-core. The carried value has already been normalized, and `normalize` is pure over one of its own outputs, so the successor rebuilds the token by normalizing it again -- which is what the roll's own restore path already does at `snapshot.rs:333`.

`VERSION` stays at `1`. No released image has ever written or read a handover blob, so there is no compatibility to preserve, and bumping it inside the branch that introduces the format would only mean version 1 never existed.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn a_blob_round_trips() {
    let h = sample_handover();
    let back: Handover = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
    assert_eq!(back, h);
}

#[test]
fn a_blob_round_trips_a_sheeps_environment_intact() {
    // A successor that silently lost an env value would respawn the app
    // under an environment it was never started with.
    let text = serde_json::to_string(&sample_handover_with_secret_env()).unwrap();
    let back: Handover = serde_json::from_str(&text).unwrap();
    assert_eq!(
        back.sheep[0].app.env.get("TOKEN").map(String::as_str),
        Some("hunter2"),
        "{text}"
    );
}

#[test]
fn debug_redacts_a_carried_sheeps_environment() {
    // The blob carries env; a log line naming the daemon's own state must
    // not. Exact-string rather than a field check, because the risk is a
    // future field printing env by accident (IR-41).
    let text = format!("{:?}", sample_handover_with_secret_env());
    assert!(!text.contains("hunter2"), "{text}");
}

#[test]
fn a_blob_from_a_future_version_is_refused_not_guessed_at() {
    let mut v = serde_json::to_value(sample_handover()).unwrap();
    v["version"] = serde_json::json!(u32::MAX);
    assert!(Handover::load_value(v).is_err());
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`
Actual: FAIL, unresolved `Handover`/`CarriedSheep`/`CarriedFds`/`VERSION` (E0425/E0422/E0433 x10)

- [x] **Step 3: Implement**

`version` is checked on load and refused if unrecognised. A successor that cannot understand the blob must fail loudly and let the caller fall back, never adopt a partial picture.

Write with mode `0600` to `$SHEP_HOME/run/handover.json`, and have the successor unlink it after reading. Set the permissions **at creation** (`OpenOptionsExt::mode`), not with a `chmod` afterwards, so there is no window where it is world-readable.

- [x] **Step 4: Run to verify they pass**

Actual: 569 passed, 18 filtered (up from 564: the plan's three, plus two over
the file's mode that the plan did not ask for). Amended after tasks 4 to 7,
when the spec was carried: 595 passed, 18 filtered, the no-env test replaced
by the two above.

- [x] **Step 5: Task gate, then commit**

---

### Task 4: resolve the binary to exec, and do not trust `current_exe`

**Files:**
- Modify: `crates/shep-daemon/src/handover/mod.rs`
- Test: same file, plus a `#[cfg(target_os = "linux")]` test

**Interfaces:**
- Produces: `pub fn exec_target() -> io::Result<PathBuf>`.

**This task exists because the obvious implementation silently execs the OLD binary, which defeats the entire feature.** Everything in this tree spawns via `std::env::current_exe()` (`crates/shep-cli/src/launch.rs:85` and others), and that is wrong here.

Measured 2026-08-30 on macOS: a running process whose binary is replaced by `rename` still gets a clean path from `current_exe()`, and that path holds the new image, so the naive version happens to work.

On Linux it does not. `current_exe()` reads `/proc/self/exe`, which resolves to the **inode** the process was executed from. After `cargo install` renames a new file over the path, that symlink still points at the old, now-unlinked inode, and `readlink` returns a path with `" (deleted)"` appended. Exec'ing that string fails, and stripping the suffix is a guess about a string the kernel is not promising.

So: prefer a path recorded at boot, and treat `current_exe()` as a fallback whose Linux result must be validated before use.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn the_exec_target_exists_and_is_a_file() {
    let p = exec_target().unwrap();
    assert!(p.is_file(), "{}", p.display());
}

#[cfg(target_os = "linux")]
#[test]
fn a_deleted_inode_path_is_never_returned() {
    // /proc/self/exe resolves to the inode, so a binary replaced by a
    // `cargo install` rename gives `"<path> (deleted)"`. Exec'ing that
    // fails, which would make a handover silently unable to upgrade,
    // which is the whole point of the feature.
    let p = exec_target().unwrap();
    assert!(
        !p.to_string_lossy().contains("(deleted)"),
        "exec target resolved to a deleted inode: {}",
        p.display()
    );
}
```

The Linux test cannot fail on a developer's Mac. **CI is the only thing that runs it**, per this repo's own rule that a local gate does not cover Linux. Say so in the commit message.

- [x] **Step 2: Run to verify they fail**

Actual: FAIL, unresolved `exec_target`/`check_target`/`TargetProblem`/
`launch_path_from_argv` (E0425/E0433 x9).

- [x] **Step 3: Implement**

`RunningDaemon` turned out to be the wrong home. `exec_target()` takes no
arguments and is called from the exec path, which holds no daemon context by
then, and the struct's fields are private with nothing threading them to
this module. So the recorded path is a process-global `OnceLock` in this
module, set by a new `record_launch_path()` that `boot` calls first thing.

Record the daemon's own launch path at boot, taken from `argv[0]` resolved against the cwd at startup, and fall back to `current_exe()` only when that is unusable. Reject any candidate whose string contains `" (deleted)"` or which does not exist on disk, and return a clear error rather than exec'ing something wrong.

- [x] **Step 4: Run to verify they pass**

Actual: 575 passed, 18 filtered (up from 569: the plan's one portable test,
plus five over the validation itself and the argv[0] resolution, so the rule
the Linux-only test protects is exercised on a platform that compiles it).

- [x] **Step 5: Task gate, then commit**

---

### Task 5: the exec

**Files:**
- Modify: `crates/shep-daemon/src/handover/mod.rs`
- Test: an integration test that really execs

**Interfaces:**
- Produces: `pub fn hand_over(blob: &Handover, paths: &ShepPaths) -> io::Result<Infallible>`, which does not return on success.

Order matters and getting it wrong loses a flock: write the blob to disk FIRST, then clear `FD_CLOEXEC` on every descriptor the blob names, then `execv`. If the exec fails, the blob is stale and must be removed before returning the error, or the next boot adopts a picture that never happened.

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn an_exec_replaces_the_image_and_keeps_a_descriptor() {
    // The whole mechanism in one assertion: a pipe written before the exec
    // is readable by the process after it, on the same fd number, proving
    // both that the image changed and that the descriptor crossed.
    let out = std::process::Command::new(helper_bin())
        .arg("handover-selftest")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("adopted: hello"));
}
```

shep-daemon is a library, so there is no helper binary to reach for and the
plan's `helper_bin()` has nothing to return. The test binary is its own
helper instead: three stages of the same test, selected by environment. The
ordinary run re-runs this one test in a child with `--exact` and a marker
naming a temp `$SHEP_HOME`; that child writes `hello` into a pipe, names the
read end in a blob, and calls `hand_over`; the image `execve` lands in sees
`SHEP_HANDOVER`, reads the blob, and reads the same fd number back. A second
test covers the failed-exec cleanup, which needs a target that cannot run
and so goes through a private `exec_into` rather than `hand_over`.

- [x] **Step 2: Run to verify it fails**

Actual: FAIL, unresolved `HANDOVER_ENV`/`hand_over`/`exec_into` (E0425 x4).

- [x] **Step 3: Implement**

Use `nix::unistd::execv`. Pass the successor's marker through the environment: the blob's path in `SHEP_HANDOVER`, following the `SHEP_CHANNEL_FD` precedent of naming a descriptor-carrying thing in the environment.

`execve` resets every signal handler to `SIG_DFL`, so nothing that was installed survives. That is expected and the successor re-installs; do not try to preserve handlers.

`nix::unistd::execve`, not `execv`. `execv` inherits this process's
`environ`, so setting `SHEP_HANDOVER` in it would mean `std::env::set_var`,
which is unsafe in edition 2024 and unsound in a process with as many
threads as the daemon; handing the environment over explicitly needs
neither, and keeps the module free of unsafe (IR-22/23). Descriptors are
cleared through a new `fds::keep_raw_across_exec`, for the same reason:
`BorrowedFd::borrow_raw` is unsafe and a number needs no borrow.

`exec_target()` resolves before the blob is written, so the one failure that
leaves nothing behind gets no cleanup; blob, then descriptors, then exec is
exactly as ordered above.

- [x] **Step 4: Run to verify it passes**

Actual: 577 passed, 18 filtered (up from 575: the plan's exec test, plus the
one over the failed-exec cleanup the ordering argument demands). The exec
test was also confirmed non-vacuous by removing the `FD_CLOEXEC` loop, at
which point the successor cannot read the descriptor and it fails.

- [x] **Step 5: Task gate, then commit**

---

### Task 6: the successor adopts what it was handed

**Files:**
- Create: `crates/shep-daemon/src/handover/adopt.rs`
- Modify: `crates/shep-daemon/src/boot.rs`
- Test: both

**Interfaces:**
- Consumes: `Handover` (Task 3), `exec_target` (Task 4).
- Produces: `pub fn adopt(blob: &Handover) -> io::Result<Adopted>`, carrying a rebuilt `UnixListener`, per-sheep async readers, and per-sheep log writers.

At boot, `SHEP_HANDOVER` in the environment means this process is a successor. It reads the blob, unlinks it, and rebuilds every Rust object around descriptors it did not open.

Rebuild each one from its raw number:

- the control listener with `UnixListener::from_std`, after `std::os::unix::net::UnixListener::from_raw_fd`
- each pipe read end as an async reader
- each log file as an appending handle. **`O_APPEND` must be preserved.** `tokio_runner.rs:1454` records that dropping it even briefly reintroduces a sparse-hole hazard a `copytruncate` rotator depends on not existing.

**The pidfile lock needs no rebuilding and must not be re-acquired.** `flock` is a property of the open file description, so it survived the exec with its descriptor. Re-acquiring would mean releasing first, opening a window for a second daemon to win it.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn an_adopted_listener_accepts() { ... }

#[test]
fn an_adopted_log_handle_still_appends() {
    // Not merely writable: appending. A handle reopened without O_APPEND
    // passes a naive write test and corrupts a rotation.
    ...
}

#[test]
fn a_blob_naming_a_descriptor_that_is_not_open_fails_loudly() {
    // Better to refuse the whole rehydrate than to supervise a flock with
    // one sheep's output silently going nowhere.
    ...
}
```

- [x] **Step 2: Run to verify they fail**

Actual: FAIL, unresolved `adopt` and `successor_handover_at` (E0432/E0425 x5).

- [x] **Step 3: Implement**

Nothing here opens anything: every object is wrapped around an inherited
number, which is what preserves `O_APPEND` on the log handles and the `flock`
on the pidfile without a single reopen.

The one unsafe stays in `sys.rs`, as a second safe entry point
`adopt_handover_fd` next to the existing `adopt_fd`. `adopt_fd` is an
`unsafe fn` whose precondition is an ordering claim about the call site, and
calling it from `handover::adopt` would put an `unsafe` block outside
`sys.rs` (IR-22/23). The handover situation discharges that precondition
structurally rather than by ordering: an inherited descriptor is open before
the successor's first instruction, so the kernel can never hand its number to
anything this process opens later. That argument is written where the unsafe
lives.

The pidfile is adopted LAST, so any refusal before it leaves that descriptor
open and unowned and its lock still held. A successor that cannot rehydrate
must not release this home to a second daemon on its way out.

`successor_handover` in `boot.rs` reads `SHEP_HANDOVER`, and a blob that is
missing or of an unknown version is logged at `error` and treated as a fresh
boot rather than as a panic or a silent fall-through. The predecessor is gone
by then, so there is no stop arm to take; the case this actually happens in
is a stale inherited variable with no live flock behind it, and a genuinely
lost blob is self-limiting because the inherited pidfile lock stops the fresh
boot at `AlreadyRunning` before it restores anything.

- [x] **Step 4: Run to verify they pass**

Actual: 587 passed, 18 filtered (up from 577: the plan's three, plus three
over the pidfile ordering, an adopted pipe really reading, and a file offered
where a pipe was named). The `O_APPEND` test was confirmed non-vacuous by
handing it a non-appending handle, at which point the write at offset 0
overwrites the file and it fails.

- [x] **Step 5: Task gate, then commit**

---

### Task 7: reap adopted pids, targeted and never wildcard

**Files:**
- Create: `crates/shep-daemon/src/handover/reap.rs`
- Test: same file

**Interfaces:**
- Produces: `pub struct AdoptedReaper` with `pub async fn wait(&self, pid: u32) -> io::Result<ExitOutcome>`.

An adopted sheep has no `tokio::process::Child`, and there is no way to make one: `Child` is produced only by `Command::spawn`. So this is a second reaping mechanism running permanently alongside tokio's.

**A wildcard `waitpid(-1, ..)` is forbidden, and this repo already learned why.** `crates/shep-cli/src/commands/reap.rs:6` and `docs/decisions.md:1588` both record that it races tokio's own reaper and steals statuses it needed, turning a clean exit into an `io::Error`; CI was bitten by it in `crates/shep-cli/tests/init.rs:325`. `tokio_runner.rs:172` already degrades to `{code: None, signal: None}` when it happens, losing the real exit.

Targeted waits are safe precisely because tokio holds no `Child` for an adopted pid, so nothing else in the process will ever wait on it. Arm a `SIGCHLD` stream, and on each wakeup call `waitpid(Pid::from_raw(pid), WNOHANG)` **for each adopted pid individually**.

Read `crates/shep-cli/src/commands/reap.rs` first. It is prior art for the vocabulary and the `nix` usage, but NOT for the architecture: it works by living in a separate process from tokio's reaper, which a successor cannot do.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn an_adopted_pid_yields_its_real_exit_code() { ... }

#[test]
fn an_adopted_pid_killed_by_a_signal_reports_the_signal() { ... }

#[test]
fn reaping_an_adopted_pid_does_not_disturb_a_tokio_spawned_child() {
    // The regression this file exists to prevent. A tokio-spawned child
    // and an adopted pid exit at the same time; both must report their own
    // real status, neither an io::Error.
    ...
}
```

The third test is the one that matters. Give it its own name in CI's serial job if it proves contention-sensitive, following `two_concurrent_boots`.

Written as four real-child tests plus three that need no child. The plan's
three, plus: a pid that had already exited before the reaper's first look
(the kernel holds the zombie, so it must yield the real status rather than
an error), a second wait replaying the first one's status instead of
meeting `ECHILD`, pid 0 refused because `waitpid(0, ..)` is a group-wide
wait wearing a different number, and a pure `outcome_of` case pinning that
a code and a signal are never collapsed into each other.

`waitpid(-1, ..)` also turned out NOT to be catchable by two children
exiting at the same instant, which is how the regression test was written
first: in isolation the wildcard passed, because tokio's own driver reaps
its child before the reaper's wakeup lands, leaving only the adopted pid
for the wildcard to find. The version that stands rests on a stronger
property instead. tokio calls `waitpid` for a live `Child` only when its
`wait()` is polled, so a supervised child that exits with nothing awaiting
it leaves its status pending in the kernel while the adopted sheep is still
running, and a wildcard takes that pending status every time.

- [x] **Step 2: Run to verify they fail**

Actual: FAIL, unresolved `AdoptedReaper`/`outcome_of` (E0432).

- [x] **Step 3: Implement**

`AdoptedReaper::wait` arms its own `SIGCHLD` stream through
`tokio::signal::unix::signal`, which multiplexes, so it takes nothing away
from the stream tokio's process driver arms for itself. Armed BEFORE the
first look, so an exit landing in between wakes the loop instead of being
lost; a pid that already exited needs no wakeup, since the first look finds
the zombie.

The reaper remembers each status it takes. A status can be collected once,
so a second or concurrent wait on a reaped pid would meet `ECHILD` and lose
it; the map makes it replay, which is the contract `RunningProcess::wait`
already states for tokio-supervised sheep. The lock spans the `waitpid`
call so two concurrent waits cannot interleave a take with a lookup.

No unsafe: `nix::sys::wait` is safe-wrapped and the pid needs no borrow.

- [x] **Step 4: Run to verify they pass**

Actual: 594 passed, 18 filtered (up from 587: the plan's three plus the four
above). Confirmed non-vacuous by substituting `Pid::from_raw(-1)` for the
targeted pid, at which point the regression test fails on its own with the
adopted pid reporting the supervised child's exit code, and three of the
other real-child tests fail too when the binary runs whole.

- [x] **Step 5: Task gate, then commit**

Actual: fmt 0, clippy 0, `cargo test --workspace --all-features` 0, doc 0,
windows-gnu cross-check 0. Clippy's `zombie_processes` fires on every test
child here and is expected with a reason rather than silenced: adding the
`Child::wait()` it asks for would take the status the assertions are about.

---

### Task 8: SPLIT. This was mis-scoped and is now 8a through 8d

**Task 8 as originally written was wrong, and the implementer refused it rather than shipping half a handover.** It read as a wiring task. Five load-bearing mechanisms it depends on do not exist, and building them is roughly 800 to 1500 lines across `supervisor.rs` (16,578 lines), `tokio_runner.rs`, `boot.rs`, `runner.rs`, `transport.rs` and `daemon.rs`, one of which changes a published trait.

Verified against the tree: `ProcessRunner` has only `spawn` and `preflight` (`crates/shep-daemon/src/runner.rs:742`), and `LogCtl` can `Reopen` and `Flush` but cannot report a descriptor (`runner.rs:102`).

What is genuinely missing:

1. **No way to learn a sheep's descriptor numbers.** The four fds `CarriedFds` names are owned by the log pump task, inside `Lines<BufReader<ChildStdout>>` and `LogFile`. Needs a new `LogCtl` variant that flushes and reports raw fds, plus a supervisor command that fans it out and waits.
2. **No way to build the blob.** `next_id`, `next_deadline` and `next_action_stamp` are private `Actor` fields with no accessor, and `epoch`, `manual` and `pending_delete` are on the private `SheepSlot`. Both the `Candidate` list and the blob have to be assembled inside the actor.
3. **An adopted sheep has no `RunningProcess`.** `TokioProc` holds a `tokio::process::Child`, which only `Command::spawn` produces. Task 7's `AdoptedReaper` is built and tested with no route into the supervisor. Needs a defaulted trait method (IR-20), a `TokioProc` that is either spawned or adopted, and an adopted `ProcIo`.
4. **No way to install an adopted flock.** `spawn_fresh` is the only path that inserts a `SheepSlot` with a live `ctl`. A successor needs one that inserts slots carrying the blob's ids, epochs, statuses and `last_exit`.
5. **The blob carries no spec, deliberately**, since env is a secret surface and an exact-string test pins it. So the successor recovers each spec from the muster roll and binds carried sheep to roll apps by name and instance. The roll stores `AppConfig` per app with a running COUNT, not per-instance ids, and `muster` starts what it restores. A restore-without-spawning-then-bind path is a design decision still to make.

The three seams Task 6 predicted are all confirmed small. They are also not the expensive part.

**The fallback design changed too. See the spec's new H3a.** A daemon that refuses internally and stops gracefully leaves the CLI polling for a successor nobody started, so fitness is now asked over the socket before the CLI signals.

#### Task 8a: report descriptors, and build the blob from a live flock

**Files:**
- Modify: `crates/shep-daemon/src/runner.rs` (`LogCtl`)
- Modify: `crates/shep-daemon/src/tokio_runner.rs` (the log pump's handler)
- Modify: `crates/shep-daemon/src/supervisor.rs` (a new actor command)
- Test: alongside each

**Interfaces:**
- Produces: a supervisor command returning `Result<(Vec<Candidate>, Handover), _>`, consumed by 8d.

The four descriptors `CarriedFds` names are owned by the log pump task, inside `Lines<BufReader<ChildStdout>>` and `LogFile`. Nothing outside that task can see them, and `LogCtl` today has only `Reopen` and `Flush` (`runner.rs:102`).

**Flush and report in ONE round trip, not two.** A pump that reports its numbers and is then asked separately to flush leaves a window where the buffered bytes are not on disk but the blob already claims the descriptor is ready to carry. The new variant does both and acknowledges once, which is the same shape `Reopen` already uses.

**The counters and the per-slot fields are only reachable inside the actor.** `next_id`, `next_deadline` and `next_action_stamp` are private `Actor` fields; `epoch`, `manual` and `pending_delete` are on the private `SheepSlot`. So the command assembles both the `Candidate` list and the `Handover` from in there, rather than exposing accessors.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_pump_reports_the_descriptors_it_holds() {
    // The numbers must be the pump's own, not a guess: an fd number is only
    // meaningful in the process that owns it, and the whole handover is
    // built on carrying exactly these.
    let (pump, ctl) = spawn_pump_fixture().await;
    let fds = ask_for_fds(&ctl).await.unwrap();
    assert!(fds.out_pipe.is_some() && fds.err_pipe.is_some());
    assert!(fds.out_log.is_some() && fds.err_log.is_some());
    assert!(all_distinct(&fds));
}

#[tokio::test]
async fn reporting_flushes_first() {
    // Written before the report, readable on disk after it. A blob whose
    // descriptors are ready but whose bytes are not is a log gap the
    // successor cannot repair, because the bytes died with the image.
    let (pump, ctl) = spawn_pump_fixture().await;
    write_line(&pump, "before-the-blob").await;
    let _ = ask_for_fds(&ctl).await.unwrap();
    assert!(read_log_file().contains("before-the-blob"));
}

#[tokio::test]
async fn a_blob_from_a_live_flock_names_four_open_descriptors_per_sheep() {
    let sup = supervisor_with_two_plain_sheep().await;
    let (candidates, blob) = sup.handover_snapshot().await.unwrap();
    assert_eq!(candidates.len(), 2);
    for s in &blob.sheep {
        for fd in s.fds.all() {
            assert!(is_open(fd.unwrap()), "blob names a closed descriptor");
        }
    }
}

#[tokio::test]
async fn the_snapshot_carries_the_actors_counters_and_slot_state() {
    // These are the fields nothing outside the actor can see, and the
    // reason this is a command rather than a getter.
    let sup = supervisor_with_a_pending_stop().await;
    let (candidates, blob) = sup.handover_snapshot().await.unwrap();
    assert!(candidates[0].pending_stop, "a pending stop must reach the gate");
    assert!(blob.next_id > 0, "a successor that reissues a live id collides");
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: handover_snapshot`

- [x] **Step 3: Implement**

Add the `LogCtl` variant. Follow `Reopen`'s existing shape for the acknowledgement channel rather than inventing a second one, and give its doc comment the flush-and-report reasoning above.

A sheep with no live pump (registered but stopped) reports `None` for all four. That is the `Option<RawFd>` case `CarriedFds` already models, and Task 1's gate does not refuse it: a stopped sheep has no descriptors to carry and nothing to lose.

- [x] **Step 4: Run to verify they pass**

- [x] **Step 5: Task gate, then commit**

#### Task 8b: the adopt seam on the runner

**Files:**
- Modify: `crates/shep-daemon/src/runner.rs` (`ProcessRunner`)
- Modify: `crates/shep-daemon/src/tokio_runner.rs` (`TokioProc`, `LogFile`, the pump)
- Modify: `crates/shep-core/src/transport.rs` (a constructor)
- Modify: `crates/shep-daemon/src/boot.rs` (`PidfileLock`)
- Test: alongside each

**Cross-crate, so the cargo shape is `cargo test --workspace --all-features`.**

An adopted sheep has no `tokio::process::Child` and there is no way to make one. Task 7 built and tested `AdoptedReaper` for exactly this, and it currently has no route into the supervisor. This task is that route.

**`ProcessRunner::adopt` is DEFAULTED (IR-20).** shep-daemon is published, so a required method would break every out-of-tree implementor. The default returns an error saying adoption is unsupported by this runner, which is the truthful answer for a runner that never took part in a handover.

**`TokioProc` becomes spawned-or-adopted.** It already stores `pid: u32` separately from its `Child`, precisely because `Child::id()` returns `None` after a wait, so the pid half needs nothing. What changes is the wait: the spawned arm keeps `child.wait()`, the adopted arm goes through `AdoptedReaper`. `signal` and `kill_tree` address the pid and are untouched.

The three seams Task 6 measured, all confirmed small:

- `LogFile::from_file(path, file)`. `LogFile` is already generic over its sink with only `path` and `handle`, and `reopen` goes back through `open_append` by path, so rotation keeps working on an adopted handle unchanged.
- a `#[cfg(unix)]` constructor on `shep_core::transport::Listener`, whose inner `tokio::net::UnixListener` is private and which offers only `bind(&Path)`.
- a `PidfileLock` arm holding an already-locked `std::fs::File`. **It must not re-lock.** The descriptor crossed the exec with its `flock` intact, and re-acquiring means releasing first, which opens a window for a second daemon to win the home.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_adopted_proc_reports_its_real_exit() {
    // The whole point of the seam. Task 7 proved the reaper; this proves it
    // is reachable through the type the supervisor actually holds.
    let mut proc = adopted_proc_for(spawn_exiting_with(3).await);
    assert_eq!(proc.wait().await.unwrap().code, Some(3));
}

#[tokio::test]
async fn a_spawned_proc_still_reports_its_real_exit() {
    // Regression. The adopted arm must not disturb the path every sheep
    // takes today.
    let mut proc = runner.spawn(&spec_exiting_with(4)).unwrap().0;
    assert_eq!(proc.wait().await.unwrap().code, Some(4));
}

#[tokio::test]
async fn an_adopted_proc_reports_a_signal_as_a_signal() {
    // rows.rs renders code and signal differently, so collapsing them makes
    // the EXIT column lie.
    let mut proc = adopted_proc_for(spawn_then_kill().await);
    let out = proc.wait().await.unwrap();
    assert_eq!((out.code, out.signal), (None, Some(9)));
}

#[test]
fn a_log_file_from_an_open_handle_still_appends() {
    // Not merely writable. Task 6 caught this exact difference by swapping
    // .append(true) for .write(true) and watching the file lose its first
    // line.
    let f = open_appending(&path);
    write_line(&path, "first");
    let mut lf = LogFile::from_file(path.clone(), f.into());
    lf.write_line("second");
    assert_eq!(read(&path), "first
second
");
}

#[test]
fn the_adopted_pidfile_arm_does_not_release_the_lock() {
    // The failure it prevents: releasing to re-acquire opens a window where
    // a second daemon wins the home. `flock` conflicts between separate
    // descriptions even inside one process, which is what makes this
    // testable at all.
    let held = PidfileLock::acquire(&paths).unwrap();
    let adopted = PidfileLock::from_locked(dup_of(&held));
    assert!(PidfileLock::acquire(&paths).is_err(), "the lock must never be free");
    drop(adopted);
}
```

- [x] **Step 2: Run to verify they fail**

- [x] **Step 3: Implement**

- [x] **Step 4: Run to verify they pass**

- [x] **Step 5: Task gate, then commit**

#### Task 8c: install the adopted flock

**Files:**
- Modify: `crates/shep-daemon/src/supervisor.rs` (a sibling to `spawn_fresh`)
- Test: same file

**Interfaces:**
- Consumes: `Handover` and `CarriedSheep` (Task 3), `ProcessRunner::adopt` and `AdoptSpec` (8b).

`spawn_fresh` is the only path that inserts a `SheepSlot` with a live `ctl`, and it spawns. A successor needs one that installs a slot around a process that is already running.

**Much smaller than it was, because the blob carries each sheep's `AppConfig`.** The earlier design rebuilt every spec from the muster roll and bound carried sheep to roll apps by name and instance, against a roll that records a running count rather than which slots were up, with `muster` starting whatever it restored. All gone. Re-normalize each carried `AppConfig` to recover its `ResolvedApp`, exactly as `snapshot.rs:333` already does on a muster.

**Restore the counters before installing any slot.** `next_id`, `next_deadline` and `next_action_stamp` reset to zero in every constructor, so a successor that installs sheep first and restores counters second can hand a fresh sheep an id a caller is still holding.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_adopted_flock_keeps_its_pids_and_ids() {
    // The blob's whole purpose. A sheep whose pid moved was respawned, and
    // this phase exists to not do that.
    let sup = supervisor_from_blob(blob_with_two_sheep()).await;
    let info = sup.list().await.unwrap();
    assert_eq!(pids_of(&info), vec![4242, 4243]);
    assert_eq!(ids_of(&info), vec![7, 8]);
}

#[tokio::test]
async fn an_adopted_sheep_keeps_its_counters_and_last_exit() {
    // Losing these is silent. RESTARTS resetting to zero hands a
    // crash-looping app amnesty it did not earn, and a lost last_exit
    // answers "why did it stop" with nothing.
    let sup = supervisor_from_blob(blob_with_history()).await;
    let info = sup.list().await.unwrap();
    assert_eq!(info[0].restarts, 4);
    assert_eq!(info[0].last_exit, Some(ExitInfo { code: Some(2), signal: None }));
}

#[tokio::test]
async fn an_adopted_sheeps_exit_flows_through_the_ordinary_path() {
    // The one that proves the reaper is actually wired. An adopted sheep
    // that exits must reach handle_exited and be judged by decide_on_exit
    // like any other, not sit there online forever.
    let sup = supervisor_from_blob(blob_with_one_real_child()).await;
    kill_the_child();
    let info = await_status(&sup, ProcStatus::Stopped).await;
    assert!(info[0].last_exit.is_some(), "the exit must be recorded, not lost");
}

#[tokio::test]
async fn the_successor_does_not_reissue_a_live_id() {
    // Counters restored BEFORE any slot is installed. A successor that
    // starts next_id at zero hands a new sheep an id a caller still holds.
    let sup = supervisor_from_blob(blob_whose_next_id_is(9)).await;
    let fresh = sup.start(&plain_app()).await.unwrap();
    assert!(fresh.id >= 9, "reissued a live id: {}", fresh.id);
}
```

- [x] **Step 2: Run to verify they fail**

- [x] **Step 3: Implement**

A carried sheep with no descriptors (registered but stopped, `CarriedFds::none()`) installs as a slot with no pump and no `ctl`, which is the state it was already in. Do not try to adopt a process that is not running.

- [x] **Step 4: Run to verify they pass**

- [x] **Step 5: Task gate, then commit**

#### Task 8d: the arms, and proving a sheep never noticed

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs` (a fitness query)
- Modify: `crates/shep-daemon/src/rpc.rs`, `boot.rs` (answer it; the SIGHUP arm)
- Modify: `crates/shep-cli/src/commands/daemon.rs` (`Arm::Handover`)
- Test: an end-to-end integration test

**Cross-crate, so the cargo shape is `cargo test --workspace --all-features`.**

**The two assertions that define success, and the whole phase:**

- a sheep's pid is UNCHANGED across a `shep daemon reload`
- its log gains no gap

A version passing neither is the stop arm phase 1 already shipped.

##### The fitness query, and why `PROTOCOL_VERSION` does not move

Spec H3a: a signal carries no reply, so a daemon that takes SIGHUP, refuses, and falls back to its own graceful stop leaves the CLI polling for a successor nobody started. The CLI therefore asks first, over the socket, and only then chooses an arm.

Adding a `Request` variant would normally bump `PROTOCOL_VERSION`, the way `SelectorSpec::Instance` did, because an older daemon cannot deserialize a variant it has never seen. **It does not need to here, and the reason is an invariant worth a test rather than a comment.**

`daemon reload` is an exempt verb, so it connects to a mismatched daemon deliberately. It learns the daemon's version from the handshake, and a daemon predating the handover takes the stop arm **without the query ever being sent**. So no daemon that cannot parse the variant is ever asked. Pin that with a test: a reload against an older daemon must reach the stop arm and must not send a fitness query.

If that invariant cannot be held, bump the version rather than shipping a query an old daemon meets as a parse error.

##### What SIGHUP does

Phase 1 made it a graceful stop so a stray signal could never drop a flock uncleanly. It now hands over, because the CLI has already asked and only signals a flock it was told is carryable. Keep the graceful stop as the arm taken when a handover cannot proceed.

##### The torn-line hazard

The pump reads through a `BufReader`, so bytes consumed but not yet a complete line die with the image, and the successor's fresh reader starts mid-line and emits the remainder as its own line. 8a's flush covers `LogFile`'s buffer, not that one.

**Write the no-gap assertion to catch a torn line, not only a missing one.** A sheep emitting a numbered sequence across the reload is the cheap version: assert every number appears exactly once, in order, each on its own line.

##### Cleanup that comes due here

Four `#[expect(dead_code)]` attributes fire as unfulfilled once their items are called. `handover/mod.rs`'s module-level one gets deleted outright rather than narrowed, and its reason text is already stale.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_sheep_keeps_its_pid_and_its_log_across_a_handover() {
    let before = flock_pids().await;
    let seen_before = read_sequence();
    reload().await;
    assert_eq!(flock_pids().await, before, "a moved pid means it was respawned");
    let seen = read_sequence();
    assert!(seen.len() > seen_before.len(), "the sheep stopped logging");
    assert_no_duplicates_or_tears(&seen);
}

#[tokio::test]
async fn a_flock_with_a_channel_sheep_takes_the_stop_arm() {
    // The gate doing its job. Not a handover, and not a failure either.
    let out = reload_with_channel_sheep().await;
    assert!(out.contains("shepherd channel"), "{out}");
    assert_ne!(flock_pids().await, before, "the stop arm does restart");
}

#[tokio::test]
async fn a_reload_against_an_older_daemon_never_sends_the_query() {
    // What keeps PROTOCOL_VERSION where it is.
    let sent = record_requests(|| reload_against_version("0.1.8")).await;
    assert!(!sent.iter().any(is_fitness_query), "{sent:?}");
}

#[tokio::test]
async fn the_control_socket_accepts_throughout() { ... }
```

- [x] **Step 2: Run to verify they fail**

- [x] **Step 3: Implement**

- [x] **Step 4: Run to verify they pass**

- [x] **Step 5: Full phase gate, then commit**

## Phase gate

After Task 8, and not per task:

```bash
cargo test --workspace --all-features -- --test-threads=1
```
```bash
CARGO_TARGET_DIR=/tmp/xcheck-linux cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```
```bash
CARGO_TARGET_DIR=/tmp/xcheck-win cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

**Then read the CI result before calling the branch green.** Task 4's `(deleted)` test is Linux-only and a macOS run never compiles it. This is the phase where that matters most: the local gate cannot prove the handover execs the new binary on the platform where it is hardest.

## Empirical acceptance, beyond the suite

Run by hand before the phase is called done, with two real binaries at different versions, as phase 1's acceptance was:

1. start a flock of plain sheep under the old binary, note every pid
2. replace the binary on disk
3. `shep daemon reload`
4. assert every sheep pid is UNCHANGED, the daemon's own version moved, and `shep bleats` shows no gap
5. repeat with one sheep given `channel = true`, and assert the stop arm was taken instead

## Self-review checklist

- Every descriptor in the spec's H2 inventory is either carried by Task 3's blob or explicitly refused by Task 1's gate. No third category.
- No task adds a dependency, and unsafe appears only in `sys.rs` if at all.
- `PROTOCOL_VERSION` and `SCHEMA_VERSION` do not move. This phase changes no wire type and no JSON envelope.
- Tasks 1, 2, 3 and 4 are independent of one another. Task 5 needs 2, 3 and 4. Task 6 needs 3. Task 7 is independent. Task 8 needs all of them.
