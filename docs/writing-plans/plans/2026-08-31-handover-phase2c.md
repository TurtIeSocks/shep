# Daemon handover, phase 2c: the in-flight cases

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this task-by-task. Steps use `- [ ]` for tracking.

**Goal:** Remove the last three `handover::fitness` refusals — `PendingStop`, `PendingDelete`, `ReloadInFlight` — and close the one failure mode that has no recovery today.

**Spec:** `docs/brainstorming/specs/2026-08-29-daemon-handover-design.md`, H2a's "2c, the hard cases" bullet.

**Base:** cut from `origin/main` at v0.1.20, which contains 2a and 2b.

---

## What 2b established that this phase inherits

2b removed four refusals and found the same bug four times. It is one class, and naming it is most of 2c's design:

**Any state whose progress depends on a `tokio::spawn`ed timer is stranded by the exec.** The task belongs to the predecessor's runtime and dies with the image, while the successor installs the *status* that timer was supposed to resolve.

| found in | the stranded state | closed by |
|---|---|---|
| task 5 | `Starting`, waiting on readiness | re-arm from `listen_timeout` |
| task 8 | `WaitingRestart`, owed a respawn | re-arm from `backoff::restart_delay` |
| this phase | a reload waiting on its deadline | task 3 below |
| task 1's drill | **a kill ladder's SIGTERM→SIGKILL escalation** | task 2 below |

The fifth was found by task 1 driving a real reload: a sheep carried
mid-ladder never escalated, and had to be killed by hand. That is the same
shape again, and it is why task 2 owns it.

Each was invisible to the whole suite and to a pid check, and each left the flock looking healthy. **The lesson 2c must not relearn: a green suite is not evidence here. Every task drives a real reload.**

## The three refusals, and what each actually is

Mapped 2026-08-31 against the code, with citations, so no task starts from a guess.

**`PendingStop`** is `SheepSlot::manual: Option<PendingManual>` (`supervisor.rs:2284`), a `{ kind, origin }` marker set by `claim_manual` (`:3937`) and cleared in `handle_exited` (`:6277`). It means "a stop, restart or delete owns this sheep's next exit." A client is waiting, but not on the marker: the reply goes through a `PendingReply` that aggregates several ids (`:4119`).

**`PendingDelete`** is a plain `bool` (`:2296`), set on a `Delete` (`:4106`) and taken in `handle_exited` (`:6279`). It deliberately survives a respawn (`:5463`). The simplest of the three: no timer, no linked entry.

**`ReloadInFlight`** is the hard one. A swap is two entries linked by id — `ReloadState::Drainee { new_id }` and `ReloadState::Replacement` (`entry.rs:170`) — plus a `ReloadJob { queue, mode, swap, deadline }` in `reloads: HashMap<String, ReloadJob>` (`supervisor.rs:2502`), a `ReloadPhase` of `DrainFirst`/`AwaitReady`/`DrainOld`/`Verify`, and **one or two** watchdog timers: `Overlap` with a probe arms a second one in `post_drain_probe` (`:5380`).

## Rollback cannot exist as the spec words it

The spec lists "rollback when a rehydrate fails." Checked, and there is nothing to roll back to:

- the `execve` at `handover/mod.rs:1120` is the only exec and is one-way
- no code anywhere re-execs the predecessor, and its image is gone
- on a failed adopt, `boot.rs:630` returns `BootError::Adopt`; the daemon refuses to boot holding the pidfile lock, and the flock keeps running unsupervised
- `boot.rs:2141` already tells the operator exactly that, and points at `shep muster`

So the honest reframing, and this is the one scope change 2c makes to the spec: **the predecessor validates the blob while it still exists, and refuses the handover into the safe stop-and-start arm rather than exec'ing into a successor that cannot boot.** Rollback-after becomes do-not-exec. Task 5.

---

## Decisions for the maintainer

Implementation proceeds on the stated assumption in each case. Say the word on any of them and the affected task changes.

**D1. A carried `manual` loses its client's reply.** The connection dies at the exec, which H2 already accepts for every in-flight RPC, and task 5 ruled the same way for an in-flight `shep trigger`: the reply is dropped by the path that already drops a reply arriving after its wait ended. *Assumption: same ruling. The operator sees a dropped connection from `shep stop`, never a wrong answer, and the sheep still stops.*

**D2. "Rollback" becomes pre-exec validation**, per the section above. *Assumption: accepted, because the alternative is not implementable.*

**D3. A successor inherits a swap it did not start. Continue it, or abort it?** Aborting is simpler and `abort_reload` already exists. But a reload the operator asked for, silently abandoned by an unrelated daemon upgrade, is a surprise with no notice. *Assumption: CONTINUE. Carry the job and re-arm the watchdog, and let the existing deadline path be the safety net — if the swap cannot finish, `handle_reload_deadline` already abandons it and emits `ReloadAbandoned` (`:5606`, `:5630`). That adds no new failure mode, because it is the same ending the swap would have had without a handover.*

---

## Order

**Corrected 2026-08-31, after task 1 shipped: tasks 1 and 2 are ONE unit,
not two.** `pending_delete` and `manual` are inseparable in the code. Every
site that sets `pending_delete = true` also calls `claim_manual` in the same
breath, and `PendingStop` is derived from `manual.is_some()` rather than from
a stop-specific flag. So removing `RefusedReason::PendingDelete` on its own
changes nothing an operator can observe: the sheep is still refused, now on
`PendingStop`. Task 1's carry could only be demonstrated at all through a
temporary bypass of the `PendingStop` check, which was reverted before its
gate. Task 2 is what makes task 1 real, and neither is finished until both
are in.

That was a defect in this plan, not in the code. It came from reading the two
fields as two facts because they are two struct members, without checking
whether anything ever sets one without the other.

5 is independent. 3 and 4 are coupled, and 4 depends on 3. Everything touches
`refusal()` in `handover/mod.rs`, so tasks run one at a time unless they are
in separate worktrees.

---

### Task 1: `pending_delete`

**Files:** `crates/shep-daemon/src/handover/mod.rs` (the refusal and `CarriedSheep`), `crates/shep-daemon/src/supervisor.rs` (`install_adopted`).

A `bool` on the blob, restored onto the entry. No timer, no linked id, no waiter beyond D1's reply.

- [x] **Step 1: Write the failing test.** A carried sheep with `pending_delete` set is adopted with it still set, and its next exit deregisters it rather than respawning it.
- [x] **Step 2: Run it, watch it fail** on `Refused(PendingDelete)`.
- [x] **Step 3: Implement.** Field on `CarriedSheep`, `Option<bool>` so an older blob loads as `None` — do NOT move `VERSION`, per tasks 4, 5 and 6's precedent.
- [x] **Step 4: Prove it non-vacuous.** Drop the field from the blob; that test and only that test fails.
- [x] **Step 5: Real reload.** `shep delete` a sheep whose exit is slow, reload mid-delete, confirm the delete still completes and the row is gone.
- [x] **Step 6: Commit** with the plan section updated.

##### Outcome

The mapped facts held, with one imprecision: the brief said "restored onto the entry," but `pending_delete` lives on `SheepSlot`, not on `ProcessEntry` — `install_adopted` restores it onto the slot it builds, same as `epoch` and `manual`.

`Candidate` and `OwnedCandidate` lost their own `pending_delete` field entirely rather than keeping it unread. `refusal()` was its only reader; once that check comes out, a `pub` field nothing reads inside a `pub(crate)` module is `dead_code` under `-D warnings`, so dropping it was forced, not a style call. `CarriedSheep` gained a fifth non-descriptor field, `pending_delete: Option<bool>`, following stdin's and the channel's own precedent exactly: `None` is what a predecessor from before this field existed truthfully meant (it refused a pending delete outright), so `VERSION` does not move.

**The finding beyond the mapped facts: `pending_delete` and `manual` are inseparable today, so task 1 alone cannot be observed end to end.** Both sites that set `SheepSlot::pending_delete = true` (`Delete`, and the failed-respawn-during-a-reload path) also call `claim_manual` in the same breath, and both `handle_handover_snapshot` and `handle_handover_fitness` derive `pending_stop` from `slot.manual.is_some()` — not from a stop-specific flag. So a flock mid-delete still refuses today, on `PendingStop` (task 2's, not yet removed) exactly where it used to refuse on `PendingDelete`. Removing `PendingDelete` alone changes nothing about whether a live `shep daemon reload` actually carries a pending-delete sheep — tasks 1 and 2 are independently steppable in the code (per the plan's own "Order" section) but not independently observable end to end. The first drill below, against the committed binary, shows this directly. To still prove task 1's own mechanism live, the second drill was run against a build with the (still-active) `PendingStop` check bypassed by one line, verified, then reverted before the gate ran; nothing from that bypass is in this diff.

##### Drill, measured

**Against the committed binary** (task 1 alone; `PendingStop` still refuses). `shep delete sticky` backgrounded, `shep daemon reload` fired 1s later, against a `kill_timeout = "20s"` sheep that traps `SIGTERM`. The client's own upfront fitness check refused, correctly naming the surviving reason rather than the removed one: `sheep 'sticky' has a pending manual stop, which this daemon cannot yet hand over; reload falls back to a stop-and-start instead`. `reload exit=8` (`deadline_exceeded`: teardown still in progress when the client's own wait ran out). Shepherd pid 69458, sheep pid 69480; both processes gone once the predecessor's own teardown finished on its own, past the client's wait. Out of scope for this task, but recorded rather than silently noticed: the saved roll afterward held `sticky` as `stopped`, not deleted — a delete claimed against a daemon that then takes an interrupted stop-and-start is not the carry path at all (`install_adopted` is never called on it), so this is unrelated to `pending_delete`'s carrying and pre-dates this task; flagged separately rather than fixed here.

**Against a one-line, uncommitted, reverted-before-the-gate bypass of the `PendingStop` check** (to isolate task 1's own mechanism). Same script, `kill_timeout = "30s"`, delete backgrounded then reload fired 1s later. `reload exit=0`, `notice[reload]: the shepherd is now 0.1.20 (pid 67398)` — shepherd 67398 unmoved, sheep 67419 unmoved and still `online` immediately after. `shep delete`'s own client got `the connection closed before a reply arrived`, exactly D1's prediction. Sticky's own kill ladder never escalated to `SIGKILL` on its own — a second, separate stranded-timer defect in `manual`'s escalation, task 2's territory and not this one — so the `SIGKILL` was sent by hand; the successor (still pid 67398) then deregistered the sheep entirely: `shep flock` afterward listed zero rows, and the saved roll's `apps` array was empty, not `stopped`. Shepherd pid unmoved, sheep pid unmoved until deliberately killed, row gone: the carry worked.

`diff` against the pre-bypass file was empty after reverting it, and the full lib suite (647/647) was re-run before any gate command.

### Task 2: `manual`

**Files:** same two, plus `PendingManual` needs to cross the wire.

`PendingManual { kind, origin }` is not `Serialize` today. Carry the `kind`; `origin` says who asked, and after the exec the answer is "a client that is gone", which D1 settles.

- [ ] **Step 1: Write the failing test.** A carried sheep with a manual stop pending still stops, and does not respawn, after the exec.
- [ ] **Step 2: Run it, watch it fail.**
- [ ] **Step 3: Implement.** Decide what `origin` becomes on the far side and say why in the commit; do not invent a new variant without arguing for it.
- [ ] **Step 4: Prove it non-vacuous**, including that a manual *restart* still restarts rather than being read as a stop.
- [ ] **Step 5: Real reload.** `shep stop` a sheep with a long `kill_timeout`, reload during the ladder, confirm it still stops.
- [ ] **Step 6: Commit.**

### Task 3: the reload deadline watchdog

**Files:** `supervisor.rs` (`install_adopted`, `arm_reload_deadline`).

Third instance of the timer-strand class. The stamp counter is already carried (2a's `Counters::next_deadline`), so this is a re-arm, not a carry.

- [ ] **Step 1: Write the failing test.** A carried swap past its deadline is abandoned, rather than sitting in its phase forever.
- [ ] **Step 2: Run it, watch it fail.**
- [ ] **Step 3: Implement** the re-arm, from the same `listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK` the original used. **Note the Overlap+probe case arms twice** (`:5380`); a re-arm that handles only the first is a half fix.
- [ ] **Step 4: Prove it non-vacuous.**
- [ ] **Step 5: Real reload,** measured the way task 8 measured the restart strand: a control daemon that was not reloaded, and a real wall clock.
- [ ] **Step 6: Commit.**

### Task 4: a swap in flight

**Files:** `handover/mod.rs`, `supervisor.rs`, `entry.rs`.

The hard one, and it depends on task 3 because a carried swap with no watchdog is worse than a refused one.

Carry, per D3: each entry's `ReloadState`, and the `ReloadJob` for each app mid-swap — `queue`, `mode`, `swap { old_id, new_id, phase }`. The ids are entry ids and 2a already carries `next_id`, so they stay valid across the exec.

- [ ] **Step 1: Write the failing tests**, one per `ReloadPhase`. `DrainFirst`, `AwaitReady`, `DrainOld` and `Verify` fail differently and a single case will not cover them.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove each non-vacuous.** Report which mutations only the end-to-end tier catches; 2b found several that all 646 lib tests missed.
- [ ] **Step 5: Real reload in each phase**, both `Serial` and `Overlap`, with pids checked on both halves of every swap.
- [ ] **Step 6: Commit.**

### Task 5: validate before the exec

**Files:** `handover/mod.rs`, `boot.rs`.

D2. The predecessor already builds the blob and holds every descriptor. Before `execve`, check the thing the successor will check: each numbered descriptor is open and of the expected kind, no number is repeated, and the blob parses as the type the successor will parse it into.

A failure here is not an error the operator sees. It is the stop-and-start arm, which is the outcome they would have had anyway.

- [ ] **Step 1: Write the failing test.** A blob naming a closed descriptor sends the reload down the stop arm instead of exec'ing.
- [ ] **Step 2: Run it, watch it fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove it non-vacuous.**
- [ ] **Step 5: Real reload** where a descriptor is closed under the daemon, confirming the flock survives by the stop arm rather than being left unsupervised.
- [ ] **Step 6: Commit.**

---

## Inherited from 2b, for a second look

**The re-armed restart delay is a judgement call, and it shipped in v0.1.20.**
2b's task 8 re-arms a carried `WaitingRestart` sheep with
`backoff::restart_delay(config, 1)`, which is the delay a FIRST unstable exit
would get. So an app with `restart_delay = "1h"` reloaded at minute 59 waits
another full hour rather than the minute it had left.

The reasoning is sound as far as it goes: what elapsed is a
`tokio::time::Instant` from a runtime that no longer exists, and erring long
respects an operator's pacing where erring short could hammer whatever the
delay exists to protect. Restarting immediately is the other obvious option
and is worse for the same reason.

**But there is a third option neither considered, and it is better than
both.** The elapsed time is unrecoverable only because it was recorded as a
monotonic `Instant`. A `SystemTime` deadline survives the exec, so the
predecessor could carry `now + remaining` and the successor re-arm for
exactly what was left. That honours the operator's schedule instead of
approximating it in either direction.

The cost is that a wall clock can jump under it — NTP, a suspend — where the
monotonic one cannot. For a restart delay that seems an acceptable trade,
since a jump changes when a down sheep comes back rather than corrupting
anything, but it is the maintainer's call and it is not urgent: today's
behaviour is safe, merely blunt.

Not scoped into 2c. Recorded here because it was found while auditing 2c's
own timer-strand class, and because the class is exactly what makes it
fixable.

## The reload drill

Unchanged from 2b's, in `docs/writing-plans/plans/2026-08-30-handover-phase2b.md` under "The reload drill, exactly". Every constraint there still binds: the `awk` rate, `$SHEP_HOME` under 103 bytes, stopping the sheep before counting, seam-aware counting.

## Gate

Per `CLAUDE.md`. Inner loop `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`. One cargo shape per task. The Windows cross-check gets its own `CARGO_TARGET_DIR`.

Counts at the branch point: **647** daemon lib, roughly **2110** workspace. A shape, not a checksum.

**CI's `slow` tier flaked three times on 2b's night** — a macOS `EIO` on a pidfile, and the Windows node-pipe budget test twice. Both are documented as machine-speed-sensitive. Read a `slow` failure against `main`'s own history before treating it as yours.
