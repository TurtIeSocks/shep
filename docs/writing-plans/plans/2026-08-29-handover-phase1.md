# Daemon handover, phase 1: guard and recovery

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a version-skewed shep say so, name the fix, and always be recoverable without the socket, and ship `shep daemon reload` as the one command that fix names.

**Architecture:** Three seams. A public liveness helper in `shep-daemon::boot` that proves a pid belongs to shep by failing to take its pidfile lock, which both `shep kill` and `shep daemon reload` use to reach a daemon the socket cannot. A client-side version comparison against `HelloAck.daemon_version`, which already crosses the wire, with three verbs exempt so the remedy is never itself guarded. And `shep daemon reload` as a verb whose stop-and-start arm ships now and whose handover arm lands in phase 2.

**Tech Stack:** Rust 2024, MSRV 1.88. `nix` for signals and `flock` on unix, `std::fs` share-mode on Windows. `clap` derive for the CLI. No new dependencies.

**Spec:** [docs/brainstorming/specs/2026-08-29-daemon-handover-design.md](../../brainstorming/specs/2026-08-29-daemon-handover-design.md)

## What this phase deliberately does NOT build

The `execve` handover itself. Phase 1 ships the guard, the recovery path, and the verb; phase 2 replaces that verb's mechanism from underneath without changing its name or its output shape.

**One refinement to the spec's H7, made while planning and worth stating.** H7 lists "the signal handler" among the things worth landing early. Taken literally that means a SIGHUP handler that hands over nothing, which buys nothing on its own, because a phase 2 CLI picks its arm by version and would never signal a phase 1 daemon anyway. It is still worth building, for a different reason: SIGHUP's default disposition is to terminate the process. A phase 1 daemon that installs SIGHUP as an alias for its existing graceful stop can never be killed uncleanly by a stray or mistaken handover signal, and the flock goes down the kill ladder instead of being orphaned with broken pipes. That is defence in depth rather than the load-bearing reason, so Task 3 states it that way.

## Global Constraints

- MSRV 1.88, edition 2024. No new crate dependencies in any task.
- `#![forbid(unsafe_code)]` holds in shep-core, shep-client and shep-cli. Unsafe belongs only in `shep-daemon/src/sys.rs` and `sys_windows.rs`, with per-block `// SAFETY:`.
- Every new public item needs a doc comment, a `# Errors` section if it returns `Result`, and a deliberate `Debug` decision. Redact anything carrying env or secrets, with an exact-string test (IR-41).
- `core::error::Error`, never `std::error::Error`.
- Do not widen an input grammar beyond what this plan states.
- **Invoke the `shep-idiomatic-rust` skill before writing any Rust.** Cite rules as `IR-<n>` in review.
- Every task's inner loop is `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::` for daemon-side work and `cargo test -p shep --lib --bins --all-features -- --skip ::slow::` for CLI-side work. **One cargo shape per task.** Do not alternate `--workspace` with `-p`; the feature unification churn costs minutes each way.
- The package is `shep`, not `shep-cli`. `-p shep-cli` runs zero tests and exits 0.
- Task gate, once per task, each from its own command with `$?` captured directly and never through a pipe: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`.
- **`PROTOCOL_VERSION` does not move in this phase.** Task 4 adds an optional field to an existing error type, which is additive. Confirm the codec tolerates unknown fields before relying on that; if it does not, Task 4's fallback applies.
- Repo-relative paths only, in code, comments and commit messages. Never an absolute local path.
- Do not name any person in code, comments, commit messages or docs.

## File Structure

| File | Responsibility |
|---|---|
| `crates/shep-daemon/src/boot.rs` | gains `daemon_liveness`, the public proof-of-life helper, and makes `read_pidfile` reachable |
| `crates/shep-daemon/src/boot.rs` (signals) | gains SIGHUP as an alias for the existing graceful stop |
| `crates/shep-daemon/src/server.rs` | the protocol refusal gains the daemon's crate version |
| `crates/shep-core/src/protocol/*` | `RpcError` gains one optional field |
| `crates/shep-cli/src/commands/admin.rs` | `kill` gains its socket-free fallback |
| `crates/shep-cli/src/commands/daemon.rs` | hosts the new `daemon reload` subcommand |
| `crates/shep-cli/src/cli.rs` | `DaemonArgs` gains an optional subcommand |
| `crates/shep-cli/src/lib.rs` | the version guard, the exempt list, and the `flock` blanket-catch fix |
| `web/src/pages/docs/*` | the upgrade note |

---

### Task 1: prove a pid belongs to shep

**Files:**
- Modify: `crates/shep-daemon/src/boot.rs` (`read_pidfile` at :240, `PidfileLock` at :313)
- Test: same file, in its existing `mod tests`

**Interfaces:**
- Produces: `pub fn daemon_liveness(paths: &ShepPaths) -> Result<Shepherd, BootError>` and `pub enum Shepherd { Absent, Running(u32), Booting }`. Tasks 2 and 7 both consume this.

**Delivered with three states, not the two this plan first specified.** `Option<u32>` collapsed "nothing is running" and "a daemon is booting" into the same `None`: `boot` takes the lock at `PidfileLock::acquire` and records its pid several statements later, binding the socket in between, stale-socket recovery included. A caller reading that window as an absence would refuse with the wrong reason, or start a second daemon that then dies unable to take the lock. `Shepherd` is deliberately exhaustive rather than `#[non_exhaustive]`: the set is closed by the mechanism, and a missed arm here means signalling the wrong process.

The pidfile alone is not proof: a stale pidfile from a crash still exists and its pid may have been reused. A live daemon HOLDS the lock, and the kernel drops it on process death, so **failing to acquire the lock is the proof of life** and the pid to report is the one recorded in the file.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn liveness_reports_none_when_no_daemon_holds_the_lock() {
    let tmp = tempdir().unwrap();
    let paths = test_paths(&tmp);
    init_dirs(&paths).unwrap();
    assert_eq!(daemon_liveness(&paths).unwrap(), None);
}

#[test]
fn liveness_reports_none_for_a_stale_pidfile_nobody_holds() {
    let tmp = tempdir().unwrap();
    let paths = test_paths(&tmp);
    init_dirs(&paths).unwrap();
    std::fs::write(pidfile(&paths), "999999").unwrap();
    // The file exists and names a pid. Nothing holds the lock, so this is
    // NOT a live daemon and must not be reported as one.
    assert_eq!(daemon_liveness(&paths).unwrap(), None);
}

#[test]
fn liveness_reports_the_pid_a_lock_holder_recorded() {
    let tmp = tempdir().unwrap();
    let paths = test_paths(&tmp);
    init_dirs(&paths).unwrap();
    let mut held = PidfileLock::acquire(&paths).unwrap();
    held.record(&paths, 4242).unwrap();
    assert_eq!(daemon_liveness(&paths).unwrap(), Some(4242));
    drop(held);
    assert_eq!(daemon_liveness(&paths).unwrap(), None);
}
```

Reuse whatever `tempdir`/`test_paths` helper `boot.rs`'s existing `mod tests` already uses; do not add a new one. Read `pidfile_round_trips_and_reports_absence` at :1783 first and follow its setup exactly.

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: liveness`
Expected: FAIL, `cannot find function daemon_liveness`

- [x] **Step 3: Implement**

```rust
/// Whether a live shepherd owns this home, and what pid it recorded.
///
/// Proof of life is the pidfile LOCK, never the pidfile's contents. A live
/// daemon holds that lock for its whole run and the kernel drops it on
/// process death, `SIGKILL` included, so a failure to acquire it is the
/// only evidence that cannot be faked by a stale file whose pid has since
/// been reused.
///
/// Returns `Ok(None)` when the lock is free, whatever the file says.
///
/// # Errors
/// - [`BootError::Io`] — the lock directory could not be read or created.
///   A contended lock is NOT an error here; it is the `Some` case.
pub fn daemon_liveness(paths: &ShepPaths) -> Result<Option<u32>, BootError> {
    match PidfileLock::acquire(paths) {
        // We took it, so nobody else holds it. Release immediately: this
        // helper answers a question, it does not claim the home.
        Ok(lock) => {
            drop(lock);
            Ok(None)
        }
        Err(BootError::AlreadyRunning { pid, .. }) => Ok(pid),
        Err(other) => Err(other),
    }
}
```

Verify `BootError::AlreadyRunning`'s real field shape at its definition before writing this match; the plan's `{ pid, .. }` is the expected shape, not a confirmed one. If it carries `Option<u32>` the arm returns it directly; if it carries a bare `u32`, wrap it in `Some`.

Also widen `read_pidfile` from `pub(crate)` to `pub` if `daemon_liveness` ends up needing it, and give it a doc comment saying it reports what was RECORDED and is not evidence of life.

- [x] **Step 4: Run to verify they pass**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: liveness`
Expected: PASS, 3 tests

- [x] **Step 5: Task gate, then commit**

```bash
git add crates/shep-daemon/src/boot.rs
git commit -m "feat(daemon): expose whether a live shepherd owns this home

The pidfile lock is the only proof of life that a stale file cannot fake.
A live daemon holds it for its whole run and the kernel drops it on death,
so failing to acquire it is evidence and reading the file is not.

Exposed because shep kill and shep daemon reload both need to reach a
daemon whose socket is refusing them, and both must refuse to signal a pid
they cannot prove is shep's."
```

---

### Task 2: `shep kill` stops a daemon that will not talk

**Files:**
- Modify: `crates/shep-cli/src/commands/admin.rs`
- Test: same file, in its existing `mod tests`

**Interfaces:**
- Consumes: `shep_daemon::boot::daemon_liveness` from Task 1.

Today `kill` handshakes, so it cannot stop a daemon that refuses the handshake. That is what bricked a box: a live daemon, a live flock, and no command in shep able to stop it.

SIGTERM is already the right signal. The daemon's handler drives the graceful teardown that runs the kill ladder over every online sheep before stopping, so the flock stops cleanly rather than being orphaned.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn kill_falls_back_to_the_pidfile_when_the_handshake_refuses() {
    // A daemon that answers the socket but refuses at the handshake is
    // the incident case: kill must still stop it.
    let fixture = refusing_daemon().await;
    let code = kill_with_wait(fixture.client(), &mut streams, SHORT_WAIT).await;
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(fixture.received_sigterm());
}

#[tokio::test]
async fn kill_refuses_a_pid_the_lock_does_not_prove_is_sheps() {
    let tmp = tempdir().unwrap();
    let paths = test_paths(&tmp);
    init_dirs(&paths).unwrap();
    std::fs::write(boot::pidfile(&paths), "999999").unwrap();
    // Stale file, nothing holds the lock. Signalling 999999 could hit an
    // unrelated process, so this must refuse rather than guess.
    let code = kill_socket_free(&paths, &mut streams).await;
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(streams.err_contains("no shepherd"));
}
```

Add a third case: `Shepherd::Booting` must report that a shepherd is starting, not that none is running, and must signal nothing.

Follow the fixture style already in `admin.rs`'s `mod tests`. Note its comment at :200 explaining that `cfg(unix)` there is about the FAKE's mechanism, not the feature.

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep --lib --bins --all-features -- --skip ::slow:: kill_`
Expected: FAIL, unresolved function

- [x] **Step 3: Implement**

Add a socket-free path that runs when the socket cannot be used for any reason, refusal included:

```rust
/// Stops a shepherd without the socket, for when the socket is the problem.
///
/// Proves the pid via [`boot::daemon_liveness`] before signalling, because
/// a stale pidfile's pid may since have been reused and shep must never
/// signal a process it cannot show is its own.
///
/// # Errors
/// Reports, and exits non-zero, when no live shepherd owns this home.
async fn kill_socket_free(paths: &ShepPaths, streams: &mut Streams<'_>) -> ExitCode {
    let pid = match boot::daemon_liveness(paths)? {
        Shepherd::Running(pid) => pid,
        // Alive and owns the home, but there is no pid to signal yet. Must
        // not be reported as an absence, and must not be guessed at.
        Shepherd::Booting => {
            return streams.fail(EXIT_NO_DAEMON, "a shepherd is starting up; try again");
        }
        Shepherd::Absent => return streams.fail(EXIT_NO_DAEMON, "no shepherd running"),
    };
    // SIGTERM, not SIGKILL: the daemon's own handler runs the kill ladder
    // over every sheep before it exits, so the flock stops cleanly.
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM)?;
    wait_for_socket_to_disappear(&socket, KILL_TEARDOWN_WAIT).await;
    ...
}
```

Reuse the existing `wait_for_socket_to_disappear` so the socket-free path gets the same two-armed unix/Windows completion check the socket path already has. Do not write a second waiter.

Windows has no SIGTERM. Gate the signal send `#[cfg(unix)]` and on Windows report that the socket-free stop is not available, naming what the operator can do instead. Do not guess at a Windows equivalent.

- [x] **Step 4: Run to verify they pass**

Run: `cargo test -p shep --lib --bins --all-features -- --skip ::slow:: kill_`
Expected: PASS

- [x] **Step 5: Task gate, then commit**

```bash
git add crates/shep-cli/src/commands/admin.rs
git commit -m "fix(cli): shep kill can stop a daemon that refuses the handshake

Every verb that reaches the daemon went through the handshake, kill
included, so a protocol-skewed daemon could not be stopped by any command
in shep. That left a live daemon, a live flock and no path forward.

kill now falls back to the pidfile when the socket cannot be used, and
proves the pid via the lock before signalling: a stale pidfile's pid may
have been reused, and shep must never signal a process it cannot show is
its own. SIGTERM rather than SIGKILL, because the daemon's handler runs the
kill ladder over every sheep before exiting."
```

---

### Task 3: SIGHUP can never kill the shepherd uncleanly

**Files:**
- Modify: `crates/shep-daemon/src/boot.rs` (signal installer, around :1394)
- Test: same file

SIGHUP's default disposition terminates the process. Phase 2 uses SIGHUP as the handover trigger, and a phase 2 CLI picks its arm by version so it should never signal a daemon too old to hand over. This task is the floor under that: a stray or mistaken SIGHUP takes the graceful path instead of dropping the flock with broken pipes.

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn sighup_is_installed_alongside_the_shutdown_signals() {
    // Phase 2 sends SIGHUP to hand over. Until that exists, SIGHUP must be
    // a graceful stop rather than the kernel default, which is an
    // unhandled terminate that would orphan the flock.
    let installed = shutdown_signal_kinds();
    assert!(installed.contains(&SignalKind::hangup()));
}
```

- [x] **Step 2: Run to verify it fails**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: sighup`
Expected: FAIL

- [x] **Step 3: Implement**

Add `SignalKind::hangup()` to the existing list at :1394 alongside `terminate`, `interrupt` and `quit`. The comment at :1317 says the list is not iterated because each is parameterised by a `SignalKind`; keep that shape and add one entry.

Document at the call site why hangup is in a list of shutdown signals:

```rust
// SIGHUP is here as a floor, not as its final meaning. It becomes the
// handover trigger, and until it does its kernel default is an unhandled
// terminate that would drop the flock's pipes rather than walk the ladder.
// A daemon that treats it as a graceful stop can be signalled by a newer
// client without the flock paying for the version gap.
```

- [x] **Step 4: Run to verify it passes**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: sighup`
Expected: PASS

- [x] **Step 5: Task gate, then commit**

---

### Task 4: the protocol refusal names the daemon's version

**Files:**
- Modify: `crates/shep-core/src/protocol/` (the `RpcError` definition)
- Modify: `crates/shep-daemon/src/server.rs` (:475)
- Modify: `crates/shep-client/src/connection.rs` (the variant at :77, the flattening at :190)
- Test: all three

**Interfaces:**
- Produces: `RpcError.daemon_version: Option<String>` AND `ConnectError::ProtocolMismatch.daemon_version: Option<String>`, consumed by Task 7's arm selection.

**Both halves are required, and this plan originally specified only the first.** Found while reviewing Task 2. `crates/shep-client/src/connection.rs:190` flattens the whole `RpcError` into `ConnectError::ProtocolMismatch { client, message }`, keeping `err.message` and dropping everything else, so a field added to `RpcError` alone is discarded four lines after it arrives and Task 7 never sees it.

`ConnectError::ProtocolMismatch`'s own doc at `connection.rs:77` also has to change. It currently reads "the daemon's version exists only inside this prose, never as a separate field" and tells callers not to parse the message. That was a deliberate design statement, and Task 7 needs it reversed: the version becomes a field precisely so nobody has to parse prose for it. Update the doc in the same commit rather than leaving it contradicting the type beneath it.

Also add to `crates/shep-daemon/src/server.rs`'s side: the comment at `connection.rs:187` notes the flattening is "sound only because `server.rs` is the sole producer today and always sends `ProtocolMismatch`". That stays true; do not widen it.

To choose between the handover arm and the stop arm, the CLI must know the running daemon's version. `HelloAck.daemon_version` answers it on a clean handshake, but a protocol refusal reports the daemon's PROTOCOL and not its version, and a protocol bump is exactly when a reload matters most.

This cannot help the upgrade that introduces it, since daemons already shipped will never send it. That is the same one-time cost the handover carries, and it is why the field is worth adding a phase early.

- [x] **Step 1: Confirm the codec tolerates unknown fields**

Before writing anything, prove that an OLD client deserializing a NEW `RpcError` with an extra field does not error. Write a test that serializes the new shape and deserializes it into a struct without the field.

```rust
#[test]
fn an_old_client_ignores_a_field_it_has_never_seen() {
    #[derive(serde::Deserialize)]
    struct OldRpcError { code: RpcErrorCode, message: String }
    let new = serde_json::json!({
        "code": "protocol_mismatch",
        "message": "...",
        "daemon_version": "0.1.16"
    });
    let old: OldRpcError = serde_json::from_value(new).expect("must tolerate");
    assert_eq!(old.message, "...");
}
```

**If this test cannot pass** because the type carries `deny_unknown_fields`, do NOT remove that attribute. Fall back to embedding the version in the refusal's `message` and have Task 7 parse it, and record the reason in the commit message.

- [x] **Step 2: Write the failing test for the refusal**

```rust
#[tokio::test]
async fn a_protocol_refusal_carries_the_daemon_version() {
    let refusal = handshake_with_protocol(PROTOCOL_VERSION + 1).await.unwrap_err();
    assert_eq!(refusal.code, RpcErrorCode::ProtocolMismatch);
    assert_eq!(refusal.daemon_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
}
```

- [x] **Step 3: Run to verify it fails**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: protocol_refusal`
Expected: FAIL, no field `daemon_version`

- [x] **Step 4: Implement, both halves**

Daemon and wire side: add the field with `#[serde(default, skip_serializing_if = "Option::is_none")]` so a daemon that has nothing to say serializes exactly as before, then populate it at `server.rs:475`, which today reads `hello.protocol` and discards the rest.

Client side: carry it through the flattening at `connection.rs:190`, which currently keeps only `err.message`:

```rust
let ack = reply.map_err(|err| ConnectError::ProtocolMismatch {
    client: PROTOCOL_VERSION,
    daemon_version: err.daemon_version,
    message: err.message,
})?;
```

Then rewrite `ConnectError::ProtocolMismatch`'s doc at `connection.rs:77`. It says the daemon's version lives only in the prose and must not be parsed; that is exactly what this task stops being true.

- [x] **Step 4b: Test the client half separately**

The daemon-side test does not cover the flattening, which is where the field would be lost.

```rust
#[tokio::test]
async fn a_refusal_carries_the_daemon_version_past_the_flattening() {
    let err = connect_to_refusing_daemon().await.unwrap_err();
    let ConnectError::ProtocolMismatch { daemon_version, .. } = err else {
        panic!("expected a protocol refusal, got {err:?}");
    };
    assert_eq!(daemon_version.as_deref(), Some("0.1.16"));
}

#[tokio::test]
async fn an_old_daemons_refusal_still_connects_and_reports_no_version() {
    // A daemon predating this field sends no `daemon_version`. That must
    // deserialize cleanly and read as None, not fail the handshake, or
    // this field breaks the exact upgrade it exists to smooth.
    let err = connect_to_refusing_daemon_without_version().await.unwrap_err();
    let ConnectError::ProtocolMismatch { daemon_version, .. } = err else { panic!() };
    assert_eq!(daemon_version, None);
}
```

- [x] **Step 5: Run to verify it passes, then task gate and commit**

---

### Task 5: the CLI refuses a daemon whose version differs

**Files:**
- Modify: `crates/shep-cli/src/lib.rs`
- Test: same file

**Task 2 left a gap here worth closing while you are in this file.** It moved `Commands::Kill`'s dispatch from `connect_client` + `admin::kill(client, ..)` to `admin::kill(&paths, ..)`, because a handshake refusal is raised inside `Client::connect` and never yields a `Client` to hand onward. Nothing tests `run`'s dispatch arms, so that wiring is held only by the compiler. A fixture that drives `run` for one arm would cover it and would serve Task 6 too.

Not just a protocol mismatch. Any difference. The dangerous state was believing `cargo install shep` upgrades a running system. It does not, it never did, and a check that says so is worth more than one that lets a mixed pair limp along.

`HelloAck` already carries `daemon_version` and `Client::daemon()` already exposes it, so this is a client-side comparison against `CARGO_PKG_VERSION` with no wire change.

**Three verbs stay exempt: `kill`, `daemon reload`, `ping`.** The first two are how an operator gets out, `ping` is how they see what is running. A guard whose remedy is itself guarded is the trap this design exists to remove.

**`EXIT_VERSION_SKEW` is not a name the code had, and the exit code is a new one.** The enum is `ExitCode` in `crates/shep-cli/src/exit.rs`, and none of its twelve variants fitted: `ProtocolMismatch` (6) is the daemon REFUSING a handshake over differing wire versions, which is a question the wire asks itself, and this guard fires on a handshake that SUCCEEDED. Collapsing the two would print "protocol mismatch" about a protocol that matched. So `ExitCode::VersionSkew = 12` is a new row, `code_str()` is `"version_skew"`, and `output::emit_error`'s own `error[{code}]: {message}` shape produces the spec's first line for free.

**The refusal writes its own block rather than going through `Streams::fail`, under `--format table` only.** `fail` routes into `output::emit_error`, which sanitises through `terminal_safe::sanitise`, and that collapses every `\n` to a space — right for daemon-supplied text, wrong for a fixed block whose remedy has to sit on its own line to be copied. Under `--format json` it is one envelope with one `error.message` carrying the same facts. The two renderings share `VERSION_SKEW_CAUSE`, held as its two rendered lines and joined with `"\n"` or `" "`, so they cannot drift.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_version_difference_with_no_protocol_difference_is_refused() {
    // The case a protocol-only check misses entirely.
    let client = fake_daemon().version("0.1.8").protocol(PROTOCOL_VERSION).await;
    let code = run_verb(Commands::Flock(..), client).await;
    assert_eq!(code, EXIT_VERSION_SKEW);
}

#[tokio::test]
async fn the_error_names_the_command_that_fixes_it() {
    let out = refused_output().await;
    assert!(out.contains("this shep is"), "{out}");
    assert!(out.contains("the running shepherd is 0.1.8"), "{out}");
    assert!(out.contains("shep daemon reload"), "{out}");
}

#[tokio::test]
async fn the_three_recovery_verbs_are_not_refused() {
    for verb in [Verb::Kill, Verb::DaemonReload, Verb::Ping] {
        let client = fake_daemon().version("0.1.8").await;
        assert_ne!(run_verb(verb, client).await, EXIT_VERSION_SKEW, "{verb:?}");
    }
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep --lib --bins --all-features -- --skip ::slow:: version_`
Expected: FAIL

- [x] **Step 3: Implement**

The message names the fix rather than the condition. Exact text, and Step 1's tests pin it:

```
error[version_skew]: this shep is 0.1.15, the running shepherd is 0.1.8

`cargo install shep` replaced the binary. It did not restart the
shepherd, which is still running the old code.

  shep daemon reload
```

Put the exempt list in one named constant with a doc comment giving the reason, not an inline match arm. A future verb added to it should have to read why.

- [x] **Step 4: Run to verify they pass, then task gate and commit**

---

### Task 5b: the verbs that connect on their own are guarded too

**Files:**
- Modify: `crates/shep-cli/src/lookout/source.rs` (:235)
- Modify: `crates/shep-cli/src/whistle/shepherd.rs` (:74)
- Modify: `crates/shep-cli/src/commands/foreground.rs` (:118)
- Modify: `crates/shep-cli/src/lib.rs` (make the guard reachable)
- Test: alongside each

**Added after Task 5, which reported the gap rather than silently widening its own scope.** Task 5 guarded the three seams in `lib.rs` that hand back a `Client`. Three operator-facing verbs bypass those seams entirely by calling `Client::connect` inside their own module, so they connect to a version-skewed daemon and proceed as if nothing were wrong.

Spec G1 says the CLI refuses any daemon whose crate version differs. It does not say most verbs do. A `shep lookout` that renders a dashboard against a daemon it cannot agree with is exactly the silent mixed state this design exists to remove.

**Interfaces:**
- Consumes: `refuse_version_skew(streams, client, guard) -> Result<(), ExitCode>` and `VersionGuard`, both currently private in `lib.rs`. Widen to `pub(crate)` rather than duplicating the check. `serve.rs` already names `crate::VersionGuard::Enforce` at its call site, so that precedent exists.

**Out of scope, with reasons, so a later reader does not think they were missed:**

- `crates/shep-cli/src/dog/mod.rs:186` is a DOG's own connection to the daemon, not an operator verb. The dog version axis is Phase 3's whole subject and has its own rules there, including the one-restart-then-report behaviour. Guarding it here would pre-empt that design.
- `crates/shep-cli/src/commands/dogs.rs` calls `Client::connect(..).ok()` at four sites, deliberately tolerating an absent daemon. That `.ok()` swallows a refusal and reports it as an absence, which is the same defect as Task 6's, not this one. Task 6 covers it.
- `status.rs:70` is `ping` and `admin.rs:61` is `kill`. Both are exempt by design; leave them.

- [x] **Step 1: Write the failing tests**

One per verb, each asserting the verb refuses with `ExitCode::VersionSkew` against a daemon reporting a different version, and proceeds normally against a matching one. Use `shep_client::testing::fake_client_with_ack`, which is the fixture Task 5 found works; the plan's earlier `fake_daemon().version(..)` shape does not exist.

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep --lib --bins --all-features -- --skip ::slow:: version_skew`

- [x] **Step 3: Widen the guard and call it at each site**

`refuse_version_skew` and `VersionGuard` become `pub(crate)`. Each of the three sites calls it immediately after its `Client::connect` succeeds, before doing anything with the client.

None of the three is a recovery verb, so each passes `VersionGuard::Enforce` directly rather than being threaded one, matching what `serve.rs` already does. Put a one-line comment at each site saying why that verb can never be exempt: `lookout` and `whistle` both drive the daemon, and `foreground` registers a sheep.

- [x] **Step 4: Run to verify they pass**

- [x] **Step 5: Task gate, then commit**

---

### Task 6: a refusal is not an absence

**Files:**
- Modify: `crates/shep-cli/src/lib.rs` (:1285)
- Test: same file

`lib.rs:1285` is a blanket `Err(_) => query::flock_from_roll(&mut streams, &paths)`, so any connect failure prints "no shepherd running".

**`commands/dogs.rs` has the same defect in a different shape, at four sites.** Each does `Client::connect(&paths.socket).await.ok()`, and that `.ok()` turns a refusal into `None` exactly as the blanket `Err(_)` turns it into a roll fallback. Fix both here; they are one bug wearing two spellings. Task 5b deliberately left these alone as belonging to this task. During the incident the daemon was alive and answering; it answered the refusal. That sent the operator to the muster-roll path rather than the reload path.

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn flock_reports_a_refusal_as_a_refusal_not_as_no_daemon() {
    let client = fake_daemon().refusing_handshake().await;
    let out = run_flock(client).await;
    assert!(!out.contains("no shepherd running"), "a refusal is not an absence: {out}");
    assert!(out.contains("shep daemon reload"), "{out}");
}
```

- [x] **Step 2: Run to verify it fails**

Expected: FAIL, the output contains "no shepherd running"

- [x] **Step 3: Implement**

Narrow the blanket catch so it distinguishes "could not connect at all", which is genuinely no daemon and keeps the roll fallback, from "connected and was refused", which is a live daemon and must say so. Match on the error, not on `_`.

- [x] **Step 4: Run to verify it passes, then task gate and commit**

---

### Task 7: `shep daemon reload`

**Files:**
- Modify: `crates/shep-cli/src/cli.rs` (`DaemonArgs`)
- Create: `crates/shep-cli/src/commands/daemon.rs` if the verb needs its own module, otherwise extend the existing one
- Test: alongside

**Interfaces:**
- Consumes: `daemon_liveness` (Task 1), `RpcError.daemon_version` (Task 4).

`DaemonArgs` is a flat flags struct today, and `shep daemon` with no subcommand is how the binary re-execs itself to daemonize. So it gains `#[command(subcommand)] pub cmd: Option<DaemonCmd>` where `None` keeps today's boot behaviour exactly. **A bare `shep daemon` must still boot.** Test that explicitly; breaking it breaks daemonization itself.

Phase 1 ships the stop arm only. It reports what happened to each sheep rather than announcing that the flock stopped, because phase 2's handover does not stop it and the same output shape has to be true under both.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn a_bare_shep_daemon_still_boots_and_is_not_a_subcommand_error() {
    let parsed = Cli::try_parse_from(["shep", "daemon"]).expect("must still parse");
    let Commands::Daemon(args) = parsed.command else { panic!() };
    assert!(args.cmd.is_none(), "bare `shep daemon` must remain the boot path");
}

#[tokio::test]
async fn reload_picks_the_stop_arm_against_a_daemon_too_old_to_hand_over() {
    let client = fake_daemon().version("0.1.8").await;
    assert_eq!(reload_arm_for(&client).await, Arm::StopAndStart);
}

#[tokio::test]
async fn reload_picks_the_stop_arm_when_the_handshake_is_refused_without_a_version() {
    // An old daemon's refusal carries no daemon_version (Task 4 cannot
    // reach backwards). Unknown must mean the safe arm, never the fast one.
    let client = fake_daemon().refusing_handshake().without_version().await;
    assert_eq!(reload_arm_for(&client).await, Arm::StopAndStart);
}

#[tokio::test]
async fn reload_reports_each_sheep_rather_than_announcing_the_flock_stopped() {
    let out = run_reload(two_sheep()).await;
    // NOTE: sheep DO stop under phase 1's stop arm. This asserts the output
    // SHAPE, which must stay true when phase 2 stops stopping them.
    assert!(out.contains("web"), "{out}");
    assert!(!out.to_lowercase().contains("flock stopped"), "{out}");
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p shep --lib --bins --all-features -- --skip ::slow:: reload`

- [x] **Step 3: Implement**

Arm selection, with unknown always meaning the safe arm:

```rust
/// Which mechanism a reload will use.
///
/// `StopAndStart` is not a fallback to be removed later. It is the
/// permanent answer for Windows, which has no `exec`; for any daemon
/// predating the handover, which cannot be taught to hand over after the
/// fact; and for a handover that fails to rehydrate.
#[derive(Debug, PartialEq, Eq)]
enum Arm { Handover, StopAndStart }
```

The stop arm composes three things that already exist: SIGTERM the proven pid, wait for it using the existing waiter, spawn a new daemon, then muster. Do not write a fourth mechanism.

`Arm::Handover` is unreachable in phase 1. Return it from nothing yet, and leave the variant with a doc comment saying phase 2 fills it. Do NOT add a stub that pretends to hand over.

- [x] **Step 4: Run to verify they pass, then task gate and commit**

---

### Task 8: the docs say the thing nobody knew

**Files:**
- Modify: `web/src/pages/docs/getting-started.astro` (install step)
- Modify: whichever page carries the upgrade guidance, found by grep rather than assumed
- Regenerate: `web/src/content/cli/` via the script

`cargo install shep` upgrades the binary and nothing else. The running daemon keeps the old code until it is reloaded, and every dog keeps its own until it is reinstalled.

Per the repo's docs trigger, this task changed what an operator types and sees, so the CLI reference must be regenerated and the site must build.

- [ ] **Step 0: Make `shep daemon reload` discoverable, which spec G5 asks for**

Task 7 shipped the verb and reported the problem it leaves behind: **the version-skew refusal names a verb that `shep --help` does not list.** `daemon` is `#[command(hide = true)]` because it is the internal re-exec path, so its subcommand is hidden with it. An operator who reads the refusal is told the fix; one who goes looking for it cannot find it. That is the incident's shape again, in a smaller costume.

`shep --help`'s verb listing is hand-rolled, not clap's: `VERB_GROUPS` near the top of `crates/shep-cli/src/cli.rs`, rendered into the block a few lines below it. Both need to agree, and there is an exact-string test over that output.

**Do not simply append `daemon reload` to the "The shepherd" group.** Those entries are bare words, and `reload` ALREADY appears in "Run things", where it means reloading a sheep. Inline it would read as two more verbs, one of which collides with a different verb that does a different thing.

Give it its own labelled line instead, in the same style as the existing `Aliases` footer, and use it to carry the upgrade sentence G5 asks for. Something in this shape, wording yours:

```
Aliases          flock: list, ls   bleats: logs   lookout: dash   stock: scale   whisper: sendline
Upgrading        cargo install shep replaces the binary, not the running shepherd: shep daemon reload
```

That one line does three jobs: it makes the verb discoverable, it distinguishes it from the sheep-level `reload` by spelling out the whole path, and it is the `shep --help` half of G5.

Update the exact-string test rather than deleting it. Then run the generator, since this changes `--help` output.

- [ ] **Step 1: Add the upgrade note and the happy path**

```
cargo install shep
cargo install shep-log-rotate   # and every other dog
shep daemon reload
```

**No pm2 comparison anywhere.** Direct comparison lives only in `from-pm2.astro`; link there if a migrating reader would want it.

- [ ] **Step 2: Regenerate the CLI reference**

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```

`git diff` afterwards is the check. A stale copy fails no build, which is why it drifts.

- [ ] **Step 3: Build and typecheck the site**

```bash
cd web && npx astro check
```
```bash
cd web && npx astro build
```

Both must exit 0. `astro check` is the one that catches a wrong component prop; `astro build` is green on a prop that does not exist.

- [ ] **Step 4: Commit**

---

## Phase gate

After Task 8, and not per task:

```bash
cargo test --workspace --all-features -- --test-threads=1
```
```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```
```bash
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

Give the cross-checks their own `CARGO_TARGET_DIR` so they do not invalidate the host cache.

**Read the CI result before calling the branch green.** The local gate does not run Linux or Windows tests: `cargo test` on a Mac never compiles a `cfg(windows)` item, and the windows-gnu cross-check is `cargo check`, which executes nothing.

## Self-review checklist

- Every spec decision in Part 1 (H3, H4, H5, H6) and Part 2 (G1 through G5) maps to a task above. Part 3 (dogs) and the `execve` handover itself are deliberately out of scope and get their own plans.
- Task 4 is the only one that touches the wire, and it is additive. `PROTOCOL_VERSION` does not move.
- `SCHEMA_VERSION` does not move either. Nothing here renames, removes or retypes a JSON field.
- Task 7 depends on Tasks 1 and 4. Task 2 depends on Task 1. Tasks 3, 5, 6 are independent and can run in parallel with each other.
