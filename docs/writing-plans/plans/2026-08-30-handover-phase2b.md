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

- [x] **Step 1: Write the failing tests**

At minimum: a pump that never answers makes the snapshot refuse rather than hang; the refusal names that sheep; a timed-out pump is not in the parked set; and every pump that DID park is resumed when the snapshot refuses.

The last one is the easy one to omit and the expensive one to omit.

- [x] **Step 2: Run to verify they fail**

- [x] **Step 3: Implement**

Pick the deadline deliberately and say why in the code. It bounds a flush plus a drain of at most one buffer per stream, so it is a small multiple of a disk write rather than a guess.

- [x] **Step 4: Prove non-vacuous, then task gate and commit**

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

`Stdin` is one more descriptor through machinery that already carries four. The channel was sketched as "a socketpair with two pump tasks, structurally the log pipes again"; it is one descriptor rather than two, it needs no parking, and the work turned out to be in two places the sketch did not name — see task 5's outcome. Multi-instance was called the one to distrust, on the grounds that `merge_logs` gives several sheep handles on ONE inode and that 8a measured the fd count not falling when logs are merged. Both halves are true and neither is a problem: several handles on one inode is several NUMBERS, which is what `refuse_repeated_fds` compares, and the fd count not falling is that fact stated the other way round. It was the cheapest of the four; see task 6's outcome. Dogs survive the exec as children for free; what needs designing is their reconnect, and Phase 3 owns their version axis.

Sketch only. Each removes exactly one variant and its test.

#### Task 4: stdin

- [x] **Step 1: Write the failing tests**
- [x] **Step 2: Run to verify they fail**
- [x] **Step 3: Implement**
- [x] **Step 4: Prove each one non-vacuous**
- [x] **Step 5: Drive a real reload, and whisper to the sheep afterwards**
- [x] **Step 6: Task gate, then commit**

##### Outcome

It was one more descriptor, and `CarriedFds` grew a fifth field for it. The direction changed three things, none of them structural:

- The successor rebuilds it with `pipe::Sender::from_file` rather than `Receiver`, which is the check that refuses a blob naming the end the child reads from.
- `CarriedFds`'s "all four or none" rule no longer holds. Four descriptors say whether a sheep is running; the fifth says whether its app asked for a pipe on fd 0, which is a different question.
- The blob format grew a field without moving `VERSION`. Serde already lets an absent `Option` field load as `None`, and `None` is what an older predecessor's blob truthfully means: it refused to carry a stdin sheep at all. A hard parse failure here would leave a successor refusing to boot after its predecessor had exec'd itself away.

**No parking, and the asymmetry is the reason.** A log pump is parked because it READS: bytes it takes off the pipe after the report are bytes the successor cannot find there, and lines it writes after the flush die in the write buffer at the exec. A stdin pump does neither. It writes what an operator hands it, and a line written before the exec is delivered.

What parking also bought was pinning, and stdin gets that from ownership instead: the pump ends only when the last `to_stdin` sender drops, and the supervisor's own slot holds one for as long as the sheep is registered, so the number cannot be closed and reissued between the report and the exec.

One residual, not closed and not new. A pump inside `write_all` at the exec leaves a partial line in the pipe, and the successor's next whisper lands behind it. Any daemon death has always done this, and `LineOutcome::NotWritten` is what the operator gets either way.

##### Drill, measured

Six `shep daemon reload`s over a flock of two, one of them `/bin/cat` with `stdin = true`. Every reload exit 0, the shepherd's pid unmoved at 97553, both sheep unmoved at 97575 and 97576, uptime continuous. A `shep whisper` after each reload reached the same never-restarted `cat`, whose echo landed in its own log file in order, no gaps and no duplicates.

#### Task 5: the shepherd channel

- [x] **Step 1: Write the failing tests**
- [x] **Step 2: Run to verify they fail**
- [x] **Step 3: Implement**
- [x] **Step 4: Prove each one non-vacuous**
- [x] **Step 5: Drive a real reload, and trigger the sheep afterwards**
- [x] **Step 6: Task gate, then commit**

##### Outcome

**One descriptor, not two, and no parking.** The sketch called it the log pipes again; it is neither shape. A socketpair is ONE open file description that is read and written at once, so `CarriedFds` grew a sixth field rather than a pair, and `tokio::io::split`'s two halves share it. And the read side needs no quiescence, for three reasons worth keeping:

- the framing is newline-delimited and the reader's error arm warns and continues rather than breaking, so a frame torn at the exec costs one message and resynchronises at the next newline. There is no permanent desync to prevent.
- every message the window can lose is already bounded. A lost `ready` is absorbed by the successor's re-armed wait, which puts the sheep `Online` at its deadline anyway; a lost `action-reply` had no client left to reach, because the exec dropped that connection; a `metric` is a debug log line.
- parking a channel reader has a failure mode parking a log pump does not. A missed un-park stops draining fd 3, the socket fills, and the APP blocks on write. The log-pump version of that mistake stops logging.

**The work was in two places the sketch did not name, and neither is about descriptors.**

**The first is `wait_ready`, and it was a real bug rather than a new feature.** A sheep gated on readiness is `Starting`, and the wait that would resolve it is a task of the predecessor's that the `execve` takes away. Nothing else moves a sheep off `Starting` except its own exit, so a successor that adopted one left it there for the rest of that daemon's life: outside `listen_timeout`, outside every status an operator acts on, and without `arm_extras`'s watch, cron and memory limits, which fire at the `Online` transition. `install_adopted` now re-arms the wait from the app's own `listen_timeout`.

That bug was NOT new to this task. `readiness_probe` gates a sheep the same way and the gate has never refused one, so a probe-gated sheep caught mid-start has been carried and stranded since 2a. The `Starting` comment in `install_adopted` recorded the behaviour as a residual; it was reachable on `main`.

One race is not closeable from the successor's side and is documented at the re-arm. The blob is a snapshot taken on the actor loop, so a `{"kind":"ready"}` arriving between it and the exec flips the predecessor's slot without reaching the blob, and the successor then waits for a signal already sent. That wait ends at `listen_timeout` and `handle_ready_result` puts the sheep `Online` anyway, with a warning, so the cost is bounded at one `listen_timeout` of `Starting` on a sheep that was already serving. The alternative was refusing to carry a `Starting` sheep at all, which restarts a whole flock over one app in its first three seconds.

**The second is that the log pump can report a channel number and be wrong about it.** The pump is told the number at the spawn, exactly as it is told stdin's, and stdin's pinning argument does not transfer. The stdin pump ends only when the last sender drops; the channel's WRITER also ends on a write that fails, which is what a child that has closed its fd 3 produces, and the socketpair's last reference goes with it. The number then names whatever the kernel has since handed to the next `open`, and `UnixStream::from(OwnedFd)` checks nothing, so the successor would write a shepherd message into a log file.

`SheepSlot::open_channel` is the fact that actually decides delivery and it is only reachable from the actor loop, so `handle_handover_snapshot` reads it alongside the slot and the snapshot masks the field with it. `adopt_channel` adds the kind check the two pipe fields get free from `from_file`: `getpeername` refuses anything that is not a socket (`ENOTSOCK`) and anything listening rather than connected (`ENOTCONN`, which is what this daemon's own control listener answers).

The drill below measures what that mask is worth, because a build without it was measured too.

**Three refusals left**, and the module header says so now instead of naming 2a: a dog, more than one instance, an in-flight reload, and an operator's pending stop or delete. Two existing tests moved off `channel = true` onto `instances = 2`, since what they are about is the gate firing at all.

##### Drill, measured

Five `shep daemon reload`s over a flock of three: a `wait_ready` + `channel` sheep that answers a `ping` action with its own pid, an `awk` counter with no sleep, and a `shutdown_with_message` sheep.

| | before | after 5 reloads |
|---|---|---|
| shepherd pid | 57775 | 57775 |
| chatty / fast / bye pids | 57796 / 57797 / 57798 | 57796 / 57797 / 57798 |
| `shep trigger chatty ping` | `replied pong-57796` | `replied pong-57796` after every one |

Every reload exit 0. The reply body carries the CHILD's own pid, which is the assertion a pid check cannot make: a socketpair that survived as a number but was attached to the wrong end, or re-paired by the successor, answers nothing at all. `shep stop bye` afterwards put `told: {"kind":"shutdown"}` in that sheep's log, so the writer direction still reached a child five execs later.

The counter, 52,320,601 lines across those five reloads: **0 whole lines lost, 0 duplicates, 4 seams.** Each seam is task 1's known residual and nothing else, for example `28182606 / 8182607 / 28182608`, where `8182607` is the tail of `28182607`. One of the five reloads produced no seam at all. So carrying the channel did not disturb the log path.

**`wait_ready` across the exec, its own run.** An app that sleeps 8s before writing `{"kind":"ready"}`, with `listen_timeout = "60s"`, reloaded at t+2 while `starting`: shepherd unmoved at 58570, sheep unmoved at 58591, still `starting` at t+8, `online` at t+10, and a `trigger` afterwards answered `pong-58591`. The readiness signal was written AFTER the exec and came up the carried socketpair into a wait the successor armed.

**The mask, measured both ways.** An app that does `exec 3>&-` and keeps running, then two `shep trigger`s (`timed_out`, then `no_channel` once the writer task had broken), then one reload:

| build | reload | shepherd pid | sheep pid |
|---|---|---|---|
| shipped | exit 0, handover | 58954 unmoved | 58975 unmoved |
| mask removed | exit 0, and `the shepherd did not come back on this version after the handover signal; starting one instead` | 59335 → 59461 | 59356 → 59483 |

Without the mask the successor refused to boot on the stale descriptor, the predecessor had already exec'd away, and the CLI's fallback started a fresh shepherd that restarted the flock from the roll. `shep daemon reload` still exited 0. That is the whole cost of the handover, paid silently, after one `shep trigger` at a child that closed fd 3.

#### Task 6: multi-instance

- [x] **Step 1: Write the failing tests**
- [x] **Step 2: Run to verify they fail**
- [x] **Step 3: Implement**
- [x] **Step 4: Prove each one non-vacuous**
- [x] **Step 5: Drive a real reload over a merged and an unmerged clustered app**
- [x] **Step 6: Task gate, then commit**

##### Outcome

**Nothing was needed but striking the refusal, and the reason is that a slot has been a sheep all along.** The supervisor keys its slots on entry id, not on name; `handle_handover_snapshot` walks `self.sheep.values()`; `CarriedSheep` has carried an `instance` field since 2a; and `install_adopted` reassembles from `carried.instance()` rather than from a count. Every descriptor the blob names is per SHEEP, and an app running three instances is three sheep with three pumps and three sets of numbers. The gate was the only thing that had ever treated the app as the unit.

**The `merge_logs` hazard is not real, and here is the measurement rather than the argument.** Each instance's pump runs its own `open_append` on its own `LogFile::open`, so one inode is reached through several open file descriptions with several numbers. On a live two-instance merged app the shepherd held `merged-out.log` on fd **32 and 36**, and `merged-err.log` on **33 and 37**, all four on inode `189235633`. After five reloads it held the same file on **33 and 36**. `refuse_repeated_fds` compares numbers, sees no repeat, and needs no rethinking. Had the premise gone the other way, the failure would have been ugly out of proportion to its cause: that function refuses the WHOLE blob, so every merged clustered app would have sent every reload of its flock down the stop arm forever, with a message naming a descriptor rather than the config that produced it. That is why it now has a case of its own at both ends, `two_pumps_on_one_log_path_report_different_numbers` on the spawn side and `two_instances_sharing_one_log_file_are_both_adopted` on the adopt side.

**Slot identity was already carried, and the mutation run is what says so.** Rehydrating every adopted sheep into slot 0 leaves all **644** daemon lib tests green. So does assembling every adopted sheep's log paths at slot 0. Only the new end-to-end case notices either, and it notices because it reads each `shep flock` row's own `out_file` back and requires the lines in it to name that row's slot and that row's pid. A pid check cannot make that assertion: a slot swap leaves two live processes, both adopted, both `Online`, each answering to the other's name and writing under the other's `SHEP_INSTANCE`.

**Three refusals left**, all 2c's, and the module header says so: a dog, an in-flight reload, and an operator's pending stop or delete. Four existing tests moved onto `Dog` for their refusal fixture, two in `handover` and two in `boot`, which task 7 will have to move again. `instances = 2` was the fixture task 5 had moved them onto for the same reason, so the churn is the phase working rather than a mistake.

**One thing found and deliberately not fixed: `REPORT_DEADLINE`'s arithmetic is now one config line away.** The sweep visits pumps serially at 2s each, and `shep daemon reload` gives the successor `admin::KILL_TEARDOWN_WAIT`, which is 10s. Six wedged pumps is therefore where the sweep outlasts the client that asked; past that the client reports a failed handover and musters against the PREDECESSOR, which is still serving and which then refuses and stops gracefully seconds later, leaving an operator with exit 0 and no flock. Reaching six used to take six app stanzas and now takes `instances = 6`. Nothing about the cost changed and the trigger is still a filesystem that has stopped completing writes, so it is recorded at the constant rather than repaired: the fix is a different shape (visit the pumps concurrently, which still knows which one went quiet) and it belongs with whoever decides what the client should do when the sweep outlasts it.

##### Drill, measured

Five `shep daemon reload`s over six sheep: `split` at three instances with separate logs, `merged` at two instances with `merge_logs = true`, and a single-instance `solo`. Every sheep an `awk` counter with no sleep, tagged `slot|pid|n` so a line names which instance wrote it. Every reload exit 0, shepherd unmoved at 11270, all six pids unmoved at 11291, 11292, 11293, 11294, 11295 and 11298, restarts 0.

| log | lines | groups it holds | lost | duplicates | seams |
|---|---|---|---|---|---|
| `split-0-out.log` | 3,952,105 | `0\|11291` only | 0 | 0 | 0 |
| `split-1-out.log` | 3,961,309 | `1\|11292` only | 0 | 0 | 0 |
| `split-2-out.log` | 3,965,357 | `2\|11293` only | 0 | 0 | 0 |
| `solo-0-out.log` | 3,962,322 | `0\|11298` only | 0 | 0 | 0 |
| `merged-out.log` | 7,946,256 | `0\|11294` and `1\|11295` | 0 | 0 | 1 |

**23,787,349 lines, 0 whole lines lost, 0 duplicates, 1 seam**, and the seam is task 1's known residual wearing a shape only a merged app can wear. Instance 1's line `100030` lost the newline at the end of its write, so instance 0's next line landed on the same row as `1|11295|1000300|11294|100604`, and the orphaned newline turned up 546 lines later as a blank row. Both numbers are physically in the file. Nothing is missing; two lines are fused and one is blank.

**Each split log holds exactly one slot.** That is the assertion the whole task is about, and it is stronger than the pid check beside it: a successor that swapped two slots would leave every pid alive, every log growing and every status `online`, with the only evidence being that slot 0's file says slot 1.

**The `name:slot` selector still reaches one instance after the exec.** On a slower second run, `shep describe split:1` answered `split 1 11899` and `shep stop split:1` stopped exactly that pid, leaving `split:0` and `split:2` online. `merged-out.log` held 40 lines from each of `0|11901` and `1|11902`, interleaved line by line, so both carried handles were still independently writable.

##### Mutations

Eight, each applied alone and reverted afterwards.

| # | what was broken | what failed |
|---|---|---|
| 1 | `refusal` refuses `instances > 1` again | `an_app_with_more_than_one_instance_is_carried`, and the e2e case at its refusal assertion |
| 2 | `CarriedSheep::from_entry` writes `instance: 0` | `each_instance_carries_its_own_slot_and_descriptors` |
| 3 | `refuse_repeated_fds` also refuses two rows of one name | `two_instances_sharing_one_log_file_are_both_adopted` |
| 4 | `LogFile::raw_fd` reports one number per PATH rather than per handle | `two_pumps_on_one_log_path_report_different_numbers` |
| 5 | `refusal` drops the `Dog` check | the four tests moved onto that fixture, plus `a_dog_refuses` |
| 6 | `install_adopted` writes `instance: 0` on every adopted entry | the e2e case ONLY; 644 daemon lib tests stay green |
| 7 | `install_adopted` assembles every adopted sheep at slot 0 | the e2e case ONLY; 644 daemon lib tests stay green |
| 8 | `assemble` ignores `merge_logs` | the e2e case at its fixture check |

6 and 7 are the finding worth keeping. The successor's slot binding has no unit-level cover at all, in either half: not the row's `instance`, and not the log paths assembled from it. Both are reachable only by reloading a real clustered flock and reading each row's own file back, which is what the new e2e case does and what nothing did before.

One flake was created and fixed in passing. The first version of the adopt-side case wrote four lines through two `tokio::fs::File` handles with one flush at the end, and failed once in twelve runs on `zero-1/one-1/one-2/zero-2`: a `tokio::fs::File` buffers and hands the real `write(2)` to the blocking pool, so an ordered assertion over two handles is an assertion about that pool. Flushing after every line makes each write land before the next begins. Twelve clean runs afterwards.

##### Follow-up defect: the serial sweep, fixed

Task 6 measured the arithmetic and did not fix it: `spawn_handover_task` visited pumps one after another, so a flock of N wedged pumps cost N times `REPORT_DEADLINE` (2s) before the caller's own fallback could even start. Past six wedged pumps that sweep already outlasted `shep-cli`'s `admin::KILL_TEARDOWN_WAIT` (10s), and reaching six had just become `instances = 6` in one stanza rather than six separate app stanzas. Worse than a slow reload: the client gave up first, fell back to `connect_or_spawn_client`, connected to the PREDECESSOR (still serving, since only the snapshot task was blocked), mustered against it, and exited 0. The sweep then finished seconds later on the actual successor, `fitness` refused on `PumpUnresponsive`, and the daemon took the graceful-stop fallback: an operator who saw exit 0 was left with no flock seconds afterward.

Fixed by visiting the pumps CONCURRENTLY (`futures_util::future::join_all`, the same primitive `spawn_send_line_task` already uses for `STDIN_WRITE_TIMEOUT`) instead of a `for` loop. The worst case is now one `REPORT_DEADLINE` regardless of N. `join_all` returns its results in input order rather than completion order, so the id-sorted order `handle_handover_snapshot` establishes before spawning the task survives into both the candidates and the blob with no re-sort needed, and each pump's report still carries its own sheep's identity, so attribution (`RefusedReason::PumpUnresponsive { sheep }`) is unaffected by the race. `spawn_reopen_task` was deliberately left serial: its caller carries `rpc`'s own per-request budget rather than a fixed client-side wait, so the same trade does not apply there.

Measured: six wedged pumps, serial 12s, concurrent 2s (both virtual time, under a paused tokio clock). A normal single-sheep reload (three reloads of a chatty `awk` counter, no wedged pumps) was re-run by hand to confirm the drill is unaffected: every reload exit 0, shepherd pid and sheep pid both unmoved, 25,579,208 lines with zero gaps and zero duplicates after `shep stop`.

---

### Task 8: re-arm audit, and the end-to-end case

8c added `arm_extras` for an adopted `Online` sheep. Audit what that actually covers against the spec's H2 table: watch debouncers, cron, memory limits, and the CPU baseline that H2 accepts losing.

The end-to-end case is 2a's, widened: a flock containing every kind 2b now carries, reloaded, with pids unchanged and no gap in any sheep's log.

---

## The reload drill, exactly

Every task that touches the carried path needs this, and getting it slightly wrong hides the defect. Recorded verbatim because three attempts were lost to variations of it.

```sh
printf '#!/bin/sh\nawk '"'"'BEGIN{i=1; while(1){print i; i++}}'"'"'\n' > /tmp/qc/fast.sh
chmod +x /tmp/qc/fast.sh
printf '[[app]]\nname = "fast"\nscript = "/tmp/qc/fast.sh"\ninterpreter = "sh"\n' > /tmp/qc/Flockfile.toml
export SHEP_HOME=/tmp/qc/home
```

Then start, reload three times with a `sleep 1` between, `shep stop fast`, and only then count.

Four things that each cost real time to learn:

- **The rate is the whole point.** A sheep with a `sleep` between lines does not tear. `awk` with no sleep gives roughly 1.6M lines a second; a shell `echo` loop gives about 10k and shows nothing. A defect measured as fixed at 10k reappeared at 100x.
- **`$SHEP_HOME` must be short.** The control socket refuses a path over 103 bytes, and the session scratchpad path is 119. Use `/tmp/<short>/home`.
- **Stop the sheep before counting.** The teardown produces its own seam, which otherwise reads as a duplicate and sends you chasing the wrong thing.
- **Count seam-aware.** A tear shows as a gap PLUS one part-eaten line, for example `14097652 / 7828 / 14097829`, where `7828` is the tail of `14097828`. A counter that treats a non-increasing step as a rewind reports nonsense like "32M lines lost". Count forward gaps and duplicates separately.

Baselines on that drill, three reloads: `origin/main` loses about 2900 lines, 2b task 1's first attempt about 1950, the shipped version 0 to 3.

## Working state, for whoever picks this up

- Branch `feat/handover-2b`, unpushed, no PR. Tasks 1, 2, 4, 5 and 6 committed; task 3 folded into 1.
- Counts after task 6: **644** daemon lib with `--skip ::slow::`, **2099** workspace.
- The rejected buffer-carry patch is at `stash@{0}` and in this session's scratchpad as `task1-carry.patch`. Read it for plumbing, not design.
- `handover/mod.rs`'s module header names the three refusals left, all of them 2c's. Task 7 has one more to strike, `Dog`, and four tests currently using it as their refusal fixture will need moving with it.
- PR #73 has two threads left open for 2b. Both are now addressed: the `report_fds` deadline is task 2, and descriptor pinning fell out of task 1's parking. They can be closed with a pointer to those commits.
- `spawn_handover_task` used to visit sheep serially, so a flock of wedged pumps waited N x 2s before the fallback, and past six that outlasted the client's 10s wait and reached exit 0 with no flock. Fixed by a follow-up defect task: see "Follow-up defect: the serial sweep, fixed" under Task 6.

## Phase gate

The four per-task commands, plus:

```bash
cargo test --workspace --all-features -- --test-threads=1
```
```bash
CARGO_TARGET_DIR=/tmp/xcheck-linux cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```

**And read the CI result.** 2a's headline bug reached four Linux jobs and never once failed on macOS.
