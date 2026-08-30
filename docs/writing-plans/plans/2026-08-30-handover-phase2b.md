# Daemon handover, phase 2b: the surface

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Carry the sheep 2a refuses, and close the three defects 2a measured but did not fix.

**Architecture:** 2a built the spine and gated everything it could not carry. This phase does two different kinds of work: it removes refusals from `handover::fitness`, and it repairs three things no gate can refuse because none of them is visible in an app's config.

**Spec:** [docs/brainstorming/specs/2026-08-29-daemon-handover-design.md](../../brainstorming/specs/2026-08-29-daemon-handover-design.md), section H2a.

## Order, and why it is not negotiable

**The three structural tasks come first.** The tear is not a property of the sheep 2b adds; it affects every sheep 2a already carries. Widening the gate first would ship a known defect to more apps and make it harder to attribute when it bites. Fix the foundation, then widen.

1. quiesce the pump at the report (was "carry the reader buffer"; that design was measured and rejected, see Task 1)
2. give `report_fds` a deadline, with an answer that distinguishes a wedged pump from a stopped one
3. pin a reported descriptor until the exec (folded into 1: a parked pump cannot release a number)
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

### Task 1: quiesce the pump at the report

**Files:**
- Modify: `crates/shep-daemon/src/tokio_runner.rs` (the pump, `LogCtl::ReportFds`)
- Modify: `crates/shep-daemon/src/handover/mod.rs` (an un-park path for the abort case)
- Test: alongside

**This task was "carry the pump's reader buffer" and that design was wrong.** It was built, measured on a real flock, and rejected. What follows replaces it, and Task 3 folds into it, because both are the same defect.

#### Why carrying the buffer is not enough, measured

The blob is a snapshot. The pump is live. Between `ReportFds` and the `execve` the pump keeps reading and emitting, so the bytes the successor prepends are bytes the predecessor has ALREADY written to the log file.

From a real reload, with the carry in place:

```
after 5082301 came 5060798, then 459 lines of old output, then 5083785
459 lines x 8 bytes = 3672, exactly the out=3672 the report answered
grep -c "^5060798$"  ->  2
```

Once where the predecessor wrote it, once where the successor replayed it. So the carry adds duplication and reordering on top of the tear it was meant to remove.

#### The second loss, which was not on this plan at all

A slower sheep leaves the reader empty at every report, so the carry does nothing, and **roughly 400 lines still vanish per reload**. Those are lines appended after the report's flush and killed in `LogFile`'s WRITE buffer at the exec.

Read side and write side, same window.

#### The design

**After answering `ReportFds`, the pump stops reading its streams until the exec.** Then the snapshot, the flush and the reported descriptor numbers are all still true when the exec happens, because nothing has moved since they were taken.

That is one change closing three residuals: the read-side tear, the write-side loss, and Task 3's unpinned descriptors, which cannot be pinned while their owner is still consuming.

**An un-park path is required and is the sharp edge.** A handover that reports and then aborts must leave the pump reading again, or a failed reload silently stops a sheep's logging for the rest of the daemon's life. `exec_into` already has an error path that restores `FD_CLOEXEC`; the un-park belongs with it.

#### What quiescing does NOT fix

`tokio::io::Lines` exposes `get_ref` and `get_mut` but not its own partial-line accumulator, so a fragment in flight at the exec is still lost. That window is a pump parked mid-line with an empty `BufReader`, costing a fragment of one line rather than a block. Closing it means the pump reading through a buffer it owns rather than through `Lines`, which is a larger change and is not this task. Document the residual where the parking happens.

#### What to keep from the rejected attempt

A 950-line patch is preserved in this session's scratchpad as `task1-carry.patch`, and the work in it is not wasted:

- `ReportFds` is served in TWO places, `LogFiles::serve` and `reserve_slot`, and neither could see the readers, which were locals in the spawned task. Threading them through as a `Streams<O, E>` struct is needed by the parking design too.
- One borrow detail worth not rediscovering: bind `deliver_line(..).await`'s result before matching on it. A scrutinee's temporaries outlive the arms, and an arm clears the reader that future was reading.
- `CarriedFds::MAX_BUFFERED = 8 * 1024`, asserted by a test that writes 11.6 KiB through a parked pump.

Whether the blob still carries buffered bytes at all is now an open question rather than a given. A parked pump's reader may simply be empty by construction, in which case the field goes.

**Answered: no field.** A parked pump's reader is NOT empty by construction, because `select!` can serve the report while a line sits ready in the `BufReader`, and a pump parked on a full `logs` channel holds a bufferful. So the report DRAINS the reader into the log file before it answers, bounded at one bufferful (`MAX_DRAIN`), instead of copying it into the blob. What the drain declines to take is not lost: it is still in the kernel's pipe, which the successor inherits by number. That leaves `CarriedFds` unchanged, so no wire or blob change, and no adopt-side prepend.

- [x] **Step 1: Write the failing test**

The measurement that matters is a chatty sheep across a reload, with every line appearing exactly once, in order. The rejected attempt's harness proved a test can pass while the log is duplicating, so assert absence of duplicates as well as absence of gaps.

- [x] **Step 2: Run to verify it fails**

- [x] **Step 3: Implement, including the un-park**

- [x] **Step 4: Prove it non-vacuous**

- [x] **Step 5: Drive a real reload, on a sheep fast enough to tear**

An `awk` or shell loop emitting with no sleep. The rejected design passed every suite it had and failed here, three runs of three.

- [x] **Step 6: Task gate, then commit**

#### Outcome, measured after the fact

The drain was manufacturing the loss it existed to prevent. It counted bytes EMITTED against its budget, but every `next_line` on an empty reader issued a `read(2)` pulling up to 8 KiB out of the pipe. On a stream that is never empty the reader refilled as fast as it drained, so when the loop stopped it held bytes it had just taken from the pipe: gone from the pipe the successor inherits, never written, destroyed at the exec. A larger budget would have read more as well as written more, so raising the bound could never have helped.

It now writes only whole lines already in the reader's buffer and stops when no newline remains, so the buffer strictly shrinks and nothing new arrives to replace what is written.

Same drill each time, `awk` with no sleep at roughly 1.6M lines a second, three reloads, `shep stop` before counting:

| build | lines | gaps | lines lost |
|---|---|---|---|
| `origin/main` | 19,892,398 | 3 | 2872 |
| first attempt | 23,038,701 | 3 | 1954 |
| shipped | 19.3M to 23.8M | 1 to 3 | **1 to 3** |
| shipped, verified independently | 9,877,880 | 0 | **0** |

Zero duplicates throughout, pid unmoved, every reload exit 0.

**Known limitation, deliberately not closed here.** At most one line per reload, often none: the line the sheep was mid-way through when the report landed, split between the reader's buffer tail and `tokio::io::Lines`' private accumulator, whose head is unreachable. The tail lands in the log as a short line of its own, so the seam reads as a gap plus one part-eaten line rather than a rewind. A naive counter mistakes that for a rewind and reports nonsense.

The cheap fix was designed and rejected. Writing the reader's tail out verbatim would make the seam byte-perfect, but every write this pump makes today is a whole line, and several sheep can share one file under `merge_logs`, where a partial write and its continuation are two `write(2)` calls that another instance's line can land between. That trades one torn line for two. Closing it properly means the pump owning its read buffer rather than using `Lines`, which this plan already defers.

### Task 2: a deadline on `report_fds`, and an answer that is not ambiguous

**Files:**
- Modify: `crates/shep-daemon/src/supervisor.rs` (`handover_snapshot`)
- Modify: `crates/shep-daemon/src/handover/mod.rs` (`fitness`, and whatever carries the answer)
- Test: alongside

A stalled pump blocks the handover AND its graceful-stop fallback, so the daemon has no way out at all.

**The fix is not a timeout wrapper.** `CarriedFds::none()` is what a STOPPED sheep reports. Collapsing a timed-out live pump into it would let the fitness gate carry a wedged sheep with its descriptors silently dropped, which is worse than the hang it replaces. The snapshot needs a third answer and it has to reach the gate, which is why this is a signature change rather than a local fix.

**Task 1 sharpened this.** A pump that answers `ReportFds` now parks; one that times out never parked, so it is still reading while every other pump is frozen. Two things follow and neither is optional:

- a timed-out pump must NOT appear in `ParkedPumps`, or the resume path will wake something that was never asleep
- the already-parked pumps must be resumed when the snapshot refuses, exactly as the fitness refusal resumes them. A handover abandoned on a timeout leaves the rest of the flock parked otherwise, which is a silent logging stop for the life of the daemon and is worse than the wedge

**The gate refuses on the third answer.** A sheep whose pump did not answer is not carryable, and the stop arm is correct for it.

- [ ] **Step 1: Write the failing tests**

At minimum: a pump that never answers makes the snapshot refuse rather than hang; the refusal names that sheep; a timed-out pump is not in the parked set; and every pump that DID park is resumed when the snapshot refuses.

The last one is the easy one to omit and the expensive one to omit.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

Pick the deadline deliberately and say why in the code. It bounds a flush plus a drain of at most one buffer per stream, so it is a small multiple of a disk write rather than a guess.

- [ ] **Step 4: Prove non-vacuous, then task gate and commit**

A real reload is not required here, since nothing about the healthy path changes. Say so rather than skipping it silently.

---

### Task 3: pin a reported descriptor until the exec (FOLDED INTO TASK 1)

Quiescing the pump pins the descriptors by construction: a parked pump does not hit EOF and does not reopen, so a reported number cannot be released and reused before the exec. Keep this heading as the record of why the task disappeared, and verify the property holds once Task 1 lands rather than assuming it.

The original statement follows.

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
