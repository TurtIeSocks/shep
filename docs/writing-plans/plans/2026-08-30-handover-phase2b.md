# Daemon handover, phase 2b: the surface

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Carry the sheep 2a refuses, and close the three defects 2a measured but did not fix.

**Architecture:** 2a built the spine and gated everything it could not carry. This phase does two different kinds of work: it removes refusals from `handover::fitness`, and it repairs three things no gate can refuse because none of them is visible in an app's config.

**Spec:** [docs/brainstorming/specs/2026-08-29-daemon-handover-design.md](../../brainstorming/specs/2026-08-29-daemon-handover-design.md), section H2a.

## Order, and why it is not negotiable

**The three structural tasks come first.** The tear is not a property of the sheep 2b adds; it affects every sheep 2a already carries. Widening the gate first would ship a known defect to more apps and make it harder to attribute when it bites. Fix the foundation, then widen.

1. carry the pump's reader buffer
2. give `report_fds` a deadline, with an answer that distinguishes a wedged pump from a stopped one
3. pin a reported descriptor until the exec
4. stdin
5. the shepherd channel
6. multi-instance
7. dogs
8. re-arm audit, and the end-to-end case

## Global Constraints

Same as 2a, and they are load-bearing rather than ceremony:

- MSRV 1.88, edition 2024. No new dependencies.
- The whole phase is `#[cfg(unix)]`. Windows has no `execve` and `Arm::for_daemon` must keep returning the stop arm there.
- Unsafe only in `crates/shep-daemon/src/sys.rs`, per-block `// SAFETY:` (IR-22/23).
- **Invoke the `shep-idiomatic-rust` skill before writing any Rust.** Cite `IR-<n>`.
- **No em dashes** in doc comments or prose.
- **One cargo shape per task.** Daemon-only work uses `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`; anything crossing crates uses `cargo test --workspace --all-features`.
- **The Windows cross-check is in the PER-TASK gate**, with its own `CARGO_TARGET_DIR`. It caught two breaks in 2a that the host-only gate called green.
- Repo-relative paths only. Do not name any person.

## What 2a taught about verifying this particular feature

Three fixes to `await_successor` shipped in one session, two found by review rather than by the suite. **The suite could not have caught any of them**, because every failure was in which daemon answered rather than in whether the flock survived, and the flock survived every time.

So: **drive a real reload by hand before pushing anything that touches the handover path.** Build release, start a flock, reload it several times, and check the exit code as well as the pids. A green suite is not evidence for this feature.

---

### Task 1: carry the pump's reader buffer

**Files:**
- Modify: `crates/shep-daemon/src/handover/mod.rs` (`CarriedSheep`)
- Modify: `crates/shep-daemon/src/tokio_runner.rs` (the pump, `LogCtl::ReportFds`)
- Modify: `crates/shep-daemon/src/handover/adopt.rs` (seed the successor's reader)
- Test: alongside

**The defect, measured rather than reasoned.** The pump reads through `Lines<BufReader<ChildStdout>>`. At the exec, bytes the `BufReader` has consumed from the pipe and not yet emitted die with the image. 8a's flush empties `LogFile`'s WRITE buffer; nothing empties the reader's.

2a measured it with a sheep emitting as fast as the pipe allows, three runs of three:

| after | next line | expected |
|---|---|---|
| `7385` | `2` | `7386` |
| `4872` | `00` | `4873` |
| `10917` | `1916` | `10918` |

The third is the shape of the whole thing. `1916` is not a suffix of `10918`, so what died was not one line: it was everything the reader held, and the resume landed about a thousand lines further on.

**The design.** The buffered bytes are just bytes. Carry them per stream in the blob, and have the successor prepend them to its fresh reader before it reads the pipe. Order is preserved because those bytes came off the pipe before anything still in the kernel buffer.

Do NOT try to solve this by draining and emitting. A drain still has to do something with a trailing partial line, and writing it out as a line is a smaller tear rather than no tear.

**A size bound is required.** `BufReader`'s default capacity is 8 KiB per stream, so a bounded flock's blob grows by a bounded amount. State the bound in the code and assert it, so a future capacity change cannot silently make the blob unbounded.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_carried_reader_buffer_survives_the_handover() {
    // The regression is only visible when the reader is NOT empty at the
    // exec, which is why the sheep has to outrun the pump rather than tick
    // politely. A test with a sleeping writer passes without the fix.
    let pump = PumpHarness::start_over_pipes();
    write_fast(&pump, 1..=5_000).await;
    let fds = report_fds(&pump).await;
    assert!(
        !fds.out_buffered.is_empty(),
        "this case proves nothing unless the reader is holding bytes"
    );

    let adopted = adopt_with(fds);
    let seen = read_all_lines(&adopted).await;
    assert_unbroken_sequence(&seen);
}
```

The first assertion is the important one. Without it the test silently degrades to the empty-buffer case and passes against no implementation at all, which is the failure mode three separate 2a tasks hit.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify it passes, and prove it non-vacuous**

Drop the carried bytes on the floor in the successor and confirm the test fails with a tear. Report the mutation and its output.

- [ ] **Step 5: Drive a real reload**

Per the note above. A chatty sheep, several reloads, `shep bleats` unbroken across all of them.

- [ ] **Step 6: Task gate, then commit**

---

### Task 2: a deadline on `report_fds`, and an answer that is not ambiguous

A stalled pump blocks the handover AND its graceful-stop fallback, so the daemon has no way out.

The fix is not a timeout wrapper. `CarriedFds::none()` is what a STOPPED sheep reports, so collapsing a timed-out live pump into it would let the fitness gate carry a wedged sheep with its descriptors silently dropped, which is worse than the hang. The snapshot needs a third answer, and it has to reach the gate.

Sketch only; expand when Task 1 lands.

---

### Task 3: pin a reported descriptor until the exec

A reported number is not owned by anything between `ReportFds` and `Handover::write`. An EOF or a `LogFile::reopen` can release it, and a later open can reuse it. `adopt`'s kind check makes a pipe landing on a log fail loudly; a log handle landing on a log handle stays quiet.

Two shapes, and the choice is the task: duplicate every reported descriptor into handover-owned storage, or serialise retirement and reopening against the exec.

Sketch only.

---

### Tasks 4 to 7: widen the gate

One refusal each, from `handover::fitness`: `Stdin`, `Channel`, `MultiInstance`, `Dog`.

`Stdin` is one more descriptor through machinery that already carries four. The channel is a socketpair with two pump tasks, structurally the log pipes again. Multi-instance is the one to distrust: `merge_logs` gives several sheep handles on ONE inode, and 8a measured that the fd count does not fall when logs are merged. Dogs survive the exec as children for free; what needs designing is their reconnect, and Phase 3 owns their version axis.

Sketch only. Each removes exactly one variant and its test.

---

### Task 8: re-arm audit, and the end-to-end case

8c added `arm_extras` for an adopted `Online` sheep. Audit what that actually covers against the spec's H2 table: watch debouncers, cron, memory limits, and the CPU baseline that H2 accepts losing.

The end-to-end case is 2a's, widened: a flock containing every kind 2b now carries, reloaded, with pids unchanged and no gap in any sheep's log.

---

## Phase gate

The four per-task commands, plus:

```bash
cargo test --workspace --all-features -- --test-threads=1
```
```bash
CARGO_TARGET_DIR=/tmp/xcheck-linux cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```

**And read the CI result.** 2a's headline bug reached four Linux jobs and never once failed on macOS.
