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
- Produces: `pub struct Handover { version: u32, sheep: Vec<CarriedSheep>, listener_fd: RawFd, pidfile_fd: RawFd, next_id: u32, next_deadline: u64, next_action_stamp: u64 }` and `CarriedSheep { id, name, instance, pid, restarts, epoch, status, last_exit, credentials, out_pipe_fd, err_pipe_fd, out_log_fd, err_log_fd }`.

**`started_at` is deliberately NOT carried.** It is a `tokio::time::Instant` with no epoch, so it cannot be serialized. Re-derive it in the successor from the operating system, which is authoritative and more correct than carrying it: `sysinfo` is already a shep-daemon dependency and exposes a process start time. Read `crates/shep-daemon/src/limits/sample.rs` for how the crate already drives `sysinfo` before adding a second style.

The blob carries **no environment values**. A sheep's env may hold secrets and the successor re-reads them from config.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn a_blob_round_trips() {
    let h = sample_handover();
    let back: Handover = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
    assert_eq!(back, h);
}

#[test]
fn the_blob_carries_no_environment_values() {
    // A sheep's env can hold secrets. This is an exact-string assertion
    // rather than a field check, because the risk is a future field that
    // serializes env by accident (IR-41).
    let text = serde_json::to_string(&sample_handover_with_secret_env()).unwrap();
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
the file's mode that the plan did not ask for).

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

- [ ] **Step 1: Write the failing test**

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

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

Use `nix::unistd::execv`. Pass the successor's marker through the environment: the blob's path in `SHEP_HANDOVER`, following the `SHEP_CHANNEL_FD` precedent of naming a descriptor-carrying thing in the environment.

`execve` resets every signal handler to `SIG_DFL`, so nothing that was installed survives. That is expected and the successor re-installs; do not try to preserve handlers.

- [ ] **Step 4: Run to verify it passes**

- [ ] **Step 5: Task gate, then commit**

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

- [ ] **Step 1: Write the failing tests**

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

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify they pass**

- [ ] **Step 5: Task gate, then commit**

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

- [ ] **Step 1: Write the failing tests**

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

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify they pass**

- [ ] **Step 5: Task gate, then commit**

---

### Task 8: wire it together, and prove a sheep never noticed

**Files:**
- Modify: `crates/shep-daemon/src/boot.rs` (the SIGHUP arm), `crates/shep-cli/src/commands/daemon.rs` (`Arm::Handover`)
- Test: an end-to-end integration test

`Arm::Handover` is currently unreachable and carries `#[expect(dead_code)]`. Constructing it without removing that attribute is a compile error, which is deliberate; remove it here.

SIGHUP currently runs a graceful stop (phase 1, Task 3). It now runs the fitness check and either hands over or falls through to that same graceful stop.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_sheep_keeps_its_pid_and_its_log_across_a_handover() {
    // The two assertions that separate a hot restart from a fast one.
    // A version that passes neither is the stop arm phase 1 already shipped.
    let before = flock_pids().await;
    let log_len_before = read_log_len();
    reload().await;
    assert_eq!(flock_pids().await, before, "pids must not move");
    assert!(read_log_len() >= log_len_before, "the log must not restart");
    assert_no_gap_in_sequence(read_log());
}

#[tokio::test]
async fn a_flock_with_a_channel_sheep_takes_the_stop_arm() {
    // The gate doing its job. Not a handover, and not a failure either.
    ...
}

#[tokio::test]
async fn the_control_socket_accepts_throughout() { ... }
```

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify they pass**

- [ ] **Step 5: Full phase gate, then commit**

---

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
