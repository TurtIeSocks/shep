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
| task 1's drill | **a kill ladder's SIGTERM→SIGKILL escalation** | task 2, done |

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

**Both are in as of task 2, and the unit held: task 1's carry is demonstrated
end to end there, on a plain build with nothing patched out.**

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

##### Demonstrated for real, by task 2

Task 2 removed `RefusedReason::PendingStop`, so the carry above no longer
needs a bypass to be observed. Its second drill is task 1's, run against a
plain release build with nothing patched out: `shep delete sticky`
backgrounded, `shep daemon reload` two seconds later, `reload exit=0`,
shepherd 63605 unmoved, sheep 66479 unmoved and still `online` the moment the
reload returned, the delete client answered
`error[daemon_unreachable]: the connection closed before a reply arrived`
(exit 5) exactly as D1 predicts, and thirty seconds later the row was GONE:
`shep flock` listed nothing and the saved roll's `apps` array was `[]`. No
hand-sent `SIGKILL` this time either, because the ladder task 2 re-arms
finished the delete on its own.

### Task 2: `manual`

**Files:** same two, plus `PendingManual` needs to cross the wire.

`PendingManual { kind, origin }` is not `Serialize` today. Carry the `kind`; `origin` says who asked, and after the exec the answer is "a client that is gone", which D1 settles.

**That last clause turned out to be the wrong reading, and the Outcome below
says why.** D1 is about the reply, which lives on a `PendingReply` and is not
carried at all; `origin` is about cause, and it crosses unchanged.

- [x] **Step 1: Write the failing test.** A carried sheep with a manual stop pending still stops, and does not respawn, after the exec.
- [x] **Step 2: Run it, watch it fail.**
- [x] **Step 3: Implement.** Decide what `origin` becomes on the far side and say why in the commit; do not invent a new variant without arguing for it.
- [x] **Step 4: Prove it non-vacuous**, including that a manual *restart* still restarts rather than being read as a stop.
- [x] **Step 5: Real reload.** `shep stop` a sheep with a long `kill_timeout`, reload during the ladder, confirm it still stops.
- [x] **Step 6: Commit.**

##### Outcome

**`origin` crosses unchanged, and no new variant.** The temptation is to
read the far side as "nobody is waiting any more" and carry `Automatic`,
since the connection behind the command dies at the exec. That is wrong
twice. `origin` answers who CAUSED this exit rather than who is still
connected, and both of its readers are about cause: the `manually` flag on
the bus events the exit produces, and `claim_manual`'s carve-out. Carrying
`Automatic` for an operator's stop would broadcast that exit as the daemon's
own doing — the exact lie the flag exists to prevent — and would let a later
`shep restart` take the marker off a ladder already running and give it a
different ending. And the reply `origin` looks like it is tracking does not
live on the marker at all: it lives on a `PendingReply`, which is not
carried, so there is nobody to answer either way. D1 is about that reply, not
about the origin.

**The kill ladder needed a re-arm, and the elapsed portion is not
recoverable.** `install_adopted` now ends the running-sheep branch with
`claim_manual(id, manual, LadderCap::Stop)`, which is the one site that pairs
a marker with the single `Kill` that produces its exit, `try_send` rule
included. What cannot be re-armed is how much of the ladder already ran:
nothing anywhere records when it started, and the remaining grace is a
`tokio::time::timeout` deadline inside a task of the predecessor's —
monotonic, and meaningless outside the runtime that read it. Unlike 2b task
8's restart delay, it is not even a value the actor holds; making it
carryable would need a new `SheepSlot` field. So the successor runs the WHOLE
ladder again, polite rung first, which errs long for the same reason
`restart_delay(config, 1)` does: a re-arm that jumped straight to
`kill_tree` would `SIGKILL` a child that may never have been asked politely
at all, since the predecessor's `Kill` could still have been unread in the
ctl mailbox at the exec. The cost is one extra `kill_timeout` and one
repeated `SIGTERM`, both measured below.

`LadderCap::Stop` rather than `graceful_timeout`, because every marker the
gate can carry today was claimed under that cap: the two sites passing
`LadderCap::Drain` are a reload's drain, and both leave `ReloadState::Drainee`
on the entry, which `ReloadInFlight` still refuses. **Task 4 has to revisit
that line**, and the code says so at the call site.

`Candidate` and `OwnedCandidate` lost `pending_stop` outright, the same way
they lost `pending_delete` in task 1 and for the same forced reason:
`refusal()` was its only reader, so a `pub` field nothing reads inside a
`pub(crate)` module is `dead_code` under `-D warnings`. `handover::fitness`
is down to two refusals, a dog and an in-flight reload, and
`web/src/pages/docs/getting-started.astro` already named exactly those two
("A dog or anything mid-reload sends the reload down the older path
instead"). That sentence was incomplete before this task and is exact now, so
`web/` needed no edit — the generated CLI reference is untouched too, since
nothing here adds a verb, flag, key, exit code or payload field.

`ManualKind`, `CommandOrigin` and `PendingManual` became `pub(crate)` and
gained `Serialize`/`Deserialize`, `snake_case` on the wire to match
`ProcStatus`'s spelling in the same file. `VERSION` does NOT move: `manual`
is `Option<PendingManual>`, one `Option` rather than the two
`pending_delete` needs, because a missing key and "no command owns this
exit" are the same statement here and there is no third state. Nothing on
the marker is sensitive — two closed enums — so a derived `Debug` is the
deliberate answer for IR-41.

**A and B are one commit, deliberately.** Removing `RefusedReason::PendingStop`
without the re-arm ships a strictly worse outcome than the refusal it
replaces: an operator's `shep stop` would leave a sheep that never dies. The
marker restore and the ladder arm are also literally the same `claim_manual`
call. A bisect landing between them would find a daemon that loses processes.

##### Drill, measured

Release build, isolated `SHEP_HOME` at `/tmp/mn/home`, one app: a `sh` script
that does `trap '' TERM` and then sleeps, `kill_timeout = "30s"`,
`autorestart = false`. Shepherd 63605 throughout — it never moved across any
of the three reloads, which is what says a handover happened rather than a
stop-and-start.

**1. `shep stop`, reload mid-ladder.** Sheep 64440.

| | |
|---|---|
| `shep stop sticky` (backgrounded) | t+0 |
| `shep daemon reload` | t+2s, **exit 0**, `notice[reload]: the shepherd is now 0.1.20 (pid 63605)` |
| immediately after the reload | shepherd 63605 unmoved, sheep 64440 unmoved, still `online` |
| sheep terminal | **t+30s from the RELOAD** (t+32s from the stop), `stopped`, `EXIT = SIGKILL` |
| processes left | none |
| the stop's own client | `error[daemon_unreachable]: the connection closed before a reply arrived`, exit 5 |

The child traps `SIGTERM`, so `SIGKILL` in the EXIT column is the whole
assertion: only the escalation could have ended it, and **nothing was killed
by hand**. Against the committed binary, task 1's drill got `exit=8` and a
refusal naming `PendingStop`, and its bypassed build needed a hand-sent
`SIGKILL` because this rung was missing.

**The control, same drill with no reload at all:** terminal at **t+31s from
the stop**, `EXIT = SIGKILL`. So the reloaded run's 30s is measured from the
exec rather than from the stop — the whole ladder run again, exactly as the
re-arm's comment claims, at a cost of the 2s already spent.

**2. `shep delete`, reload mid-delete.** Sheep 66479. This is task 1's carry,
finally observable with nothing patched out.

| | |
|---|---|
| `shep delete sticky` (backgrounded) | t+0 |
| `shep daemon reload` | t+2s, **exit 0** |
| immediately after | shepherd 63605 unmoved, sheep 66479 unmoved, still `online` |
| row gone | **t+30s from the reload**; `shep flock` lists zero rows |
| saved roll | `apps: []` — deleted, not `stopped` |
| the delete's own client | `daemon_unreachable`, exit 5 |

**3. `shep restart`, reload mid-ladder.** Sheep 67565. The kind has to
survive, not just the marker.

| | |
|---|---|
| `shep daemon reload` | t+2s, exit 0, shepherd 63605 unmoved, sheep 67565 unmoved and `online` |
| respawned | **t+30s from the reload**: `online`, pid **68487**, `restarts=1`, previous `EXIT` recorded as `SIGKILL` |

Read as a stop, this row would be `stopped` with `restarts=0`. It is not.

##### Mutations

Five, each applied alone against the daemon lib suite (651 with
`--skip ::slow::`) and reverted afterwards, with the file byte-compared to a
pre-mutation copy each time.

| # | what was broken | what failed |
|---|---|---|
| 1 | `install_adopted` drops the re-arm entirely | the three new adoption cases, **and nothing else in 651** |
| 2 | the marker is written onto the slot but no ladder is armed | the same three, all by timeout — this is the half the plan called "worse than the refusal", and no unit case outside these three notices |
| 3 | the ladder is armed under a hardcoded `ManualKind::Stop` | `a_carried_manual_restart_respawns_and_keeps_its_origin` ONLY (650 pass) |
| 4 | `manual` made required on the wire (`deserialize_with = "Option::deserialize"`) | `a_blob_written_before_a_manual_marker_was_carried_still_loads` ONLY, with serde's `missing field` error naming it |
| 5 | the ladder is armed under a hardcoded `CommandOrigin::Operator` | the restart case ONLY, at its `manually` assertion |

**What only the end-to-end tier catches: nothing here, and that is the
finding.** Unlike 2b's mutation 2 (`arm_extras`, invisible to all 646 lib
tests), every mutation above is caught by a unit case, because a real child
under `TokioRunner` is reachable from the lib tier: `adoptable_child` gives
the ladder a real process, its own process group, and a real trap. What the
lib tier could NOT have told anyone is that the defect existed at all — it
was found by driving a reload by hand, and the tests were written afterwards
to pin it.

**Two test-infrastructure findings worth keeping**, both of which cost a run
each. A child that inherits the test binary's process group is not a group
leader, so `killpg` answers `ESRCH`, the ladder logs a warning and delivers
nothing, and a case about a stop fails for a reason that has nothing to do
with the stop; `adoptable_child` sets `process_group(0)` and says so. And a
child that inherits the test binary's stdout holds that pipe open, so under
`cargo test | <anything>` a mutation run turned a failing assertion into a
hang; the same helper now uses `Stdio::null()` and bounds its loop.

**Tasks 3 and 4 are ONE unit too, corrected 2026-08-31 — the same mistake
as 1 and 2, made twice.** `ReloadInFlight` refuses every sheep mid-swap, so
nothing with a reload watchdog is ever carried, so a re-armed watchdog has
nothing to fire against. Task 3 alone would be unobservable exactly the way
task 1 alone was, and would need the same temporary bypass to show anything.

They ship together. The ordering inside the task still stands and still
matters: the watchdog re-arm is built first, because a carried swap with no
watchdog is worse than a refused one.

The error both times was splitting on structure rather than on observable
behaviour. Two struct members, two refusal variants: neither is a seam a
drill can stand on.

### Task 3: the reload deadline watchdog

**Files:** `supervisor.rs` (`install_adopted`, `arm_reload_deadline`).

Third instance of the timer-strand class. The stamp counter is already carried (2a's `Counters::next_deadline`), so this is a re-arm, not a carry.

- [x] **Step 1: Write the failing test.** A carried swap past its deadline is abandoned, rather than sitting in its phase forever.
- [x] **Step 2: Run it, watch it fail.**
- [x] **Step 3: Implement** the re-arm, from the same `listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK` the original used. **Note the Overlap+probe case arms twice** (`:5380`); a re-arm that handles only the first is a half fix.
- [x] **Step 4: Prove it non-vacuous.**
- [x] **Step 5: Real reload,** measured the way task 8 measured the restart strand: a control daemon that was not reloaded, and a real wall clock.
- [x] **Step 6: Commit.**

### Task 4: a swap in flight

**Files:** `handover/mod.rs`, `supervisor.rs`, `entry.rs`.

The hard one, and it depends on task 3 because a carried swap with no watchdog is worse than a refused one.

Carry, per D3: each entry's `ReloadState`, and the `ReloadJob` for each app mid-swap: `queue`, `mode`, `swap { old_id, new_id, phase }`. The ids are entry ids and 2a already carries `next_id`, so they stay valid across the exec.

- [x] **Step 1: Write the failing tests**, one per `ReloadPhase`. `DrainFirst`, `AwaitReady`, `DrainOld` and `Verify` fail differently and a single case will not cover them.
- [x] **Step 2: Run them, watch them fail.**
- [x] **Step 3: Implement.**
- [x] **Step 4: Prove each non-vacuous.** Report which mutations only the end-to-end tier catches; 2b found several that all 646 lib tests missed.
- [x] **Step 5: Real reload in each phase**, both `Serial` and `Overlap`, with pids checked on both halves of every swap.
- [x] **Step 6: Commit.**

##### Outcome

**Three timers, not one, and the brief named one of them.** The mapped facts
held (`ReloadState` on the entry, `ReloadJob` per app, the ids valid across
the exec), but the count of what the exec strands was short. A swap is driven
by whichever of three tasks its phase is waiting on:

| phase | what ends it | where the re-arm is |
|---|---|---|
| `DrainFirst` | the drainee's own exit, produced by its kill ladder | task 2's `claim_manual`, now under the drain's cap |
| `AwaitReady` | `Msg::ReadyResult` from a readiness task | `install_adopted`, already there since 2b task 5 for every `Starting` sheep |
| `DrainOld` | the drainee's exit again | as `DrainFirst` |
| `Verify` | `Msg::ReloadVerified` from `spawn_verify_task` | **new**, `install_carried_reloads` |
| any of the four | `Msg::ReloadDeadline`, if none of the above ever comes | **new**, `install_carried_reloads` |

So `AwaitReady` needed no new code at all: a carried replacement is a
`Starting` sheep, and the wait 2b re-armed for those is the same wait. The
post-drain probe is the one the brief did not name, and it is a fourth
instance of the class rather than a detail of the third. A successor that
re-armed only the watchdog would abandon a deploy whose replacement is fine,
16s later, with a `ReloadAbandoned` for a swap that had already worked.
Drill 5 below measures exactly that gap.

**`ReloadJob::deadline` is deliberately not carried**, which is the one field
of the four that does not cross. It stamps a timer that dies with the image,
and the successor takes a fresh stamp off the carried `next_deadline` when it
arms its own. Carrying it would name a watchdog that does not exist.

**D3's "continue" held in all four phases, and two of them were not obvious.**
`DrainFirst` and `DrainOld` both route on the `Drainee` marker, which is what
sends the exit to `reap_drainee` rather than to `decide_on_exit`, and for an
`autorestart` app the latter respawns the old code into a slot the
replacement owns. `AwaitReady` routes on `Replacement`, which is what
`handle_ready_result` reads to send the result to `reload_ready_result`
instead of straight to `Online`. `Verify` needed the probe above. Nothing in
any of the four turned out to be unsound, so the escape hatch was not used.

**One residual is worth naming, and it is 2b task 5's own race widened.** The
blob is a snapshot taken on the actor loop, so a `{"kind":"ready"}` that
arrives between it and the exec flips the predecessor's slot without reaching
the blob. For an ordinary `Starting` sheep that costs one `listen_timeout` of
`Starting` and then `Online` anyway. For a `wait_ready` REPLACEMENT it costs
the reload: the signal is not sent twice, the successor's re-armed wait times
out, and `reload_ready_result` reads a `TimedOut` replacement as a failure and
abandons: the drainee goes back to serving and the replacement is killed. The
window is narrow (the mailbox is FIFO, so it is only the gap between the child
writing and the readiness task reporting, plus the snapshot's own descriptor
sweep), the ending is one the reload already has when a ready signal is
genuinely lost, and the operator gets a `ReloadAbandoned` rather than silence.
Recorded rather than fixed: closing it would mean carrying "readiness already
resolved" as a fact separate from the status, which is a wider change than
this residual is worth.

**Task 2's `LadderCap::Stop` line, revisited as its call site asked.** The cap
now comes off the role: `ReloadState::Drainee` takes `LadderCap::Drain`,
everything else keeps `Stop`. That is the same rule the two live sites follow
(both pass `Drain`, and both leave `Drainee` on the entry), read backwards
from the marker, which is the only record of which ask is in hand. The
residual is the same shape as the elapsed grace task 2 could not recover:
which cap the ladder ACTUALLY started under is recorded nowhere, and an
operator's `stop` reaching a drainee during the overlap's `AwaitReady` window
claims the marker first, under `kill_timeout`, with the drain that follows
riding that ladder. A successor re-arming that sheep uses `graceful_timeout`
instead. The cost is bounded by whichever of the two is longer and the ending
is the same either way. Drill 4b measures the cap live, against an app whose
two timeouts are 8s and 120s.

**`manually` is knowable for a replacement, and was `false`.**
`install_adopted` arms a carried `Starting` sheep's readiness with
`manually: false`, on the argument that the flag belongs to the spawn that
armed the original wait and is not this image's to know. That is right for an
ordinary sheep and wrong for a replacement: `spawn_replacement` passes `true`
unconditionally, because a reload is an operator's doing, so a carried
`Replacement` marker is the same claim arriving by a different route. Left
alone it would have broadcast an operator's deploy as the daemon's own.

**A carried job naming no registered instance is dropped rather than
refused.** A snapshot cannot produce one, because the job and the entries it
names are read in one synchronous step on the actor loop. But a blob is a file,
and this is the residual `refuse_repeated_fds` already guards on the
descriptor side. Note that "registered" is weaker than it looks: a SERIAL
reload deregisters its drainee at `ReapOld` and keeps the job, so `swap.old_id`
is a dangling id by design from `AwaitReady` onwards, and the anchor the
watchdog reads its timings off is `new_id` first, `old_id` only as a fallback.
That asymmetry is why this is a drop-with-a-warning rather than a validation:
the invariants are mode- and phase-dependent, and a second statement of them
in `adopt` would be exactly the drift task 5 was told not to ship.

**Which is also why `handover::adopt::dry_run` needed no change, and the
reason is not "nothing was added".** The rehearsal runs the successor's own
checks, and the successor gains no new refusal here. What it does gain is two
new fields in a blob it parses, and the rehearsal already covers those for
free: `dry_run` reads the whole blob back through `Handover::load_value`
before rehearsing a single descriptor, so a `reloads` array or a `reload`
marker that could not survive the round trip is refused before the exec like
anything else. `a_blob_carrying_a_swap_in_flight_passes_the_rehearsal` pins
that claim rather than leaving it asserted.

**`VERSION` unmoved**, on tasks 1 and 2's precedent exactly. Two `Option`
fields, `CarriedSheep::reload` and `Handover::reloads`, where an absent key
loads as `None`, and `None` is what a predecessor that refused to carry a swap
at all truthfully meant. `ReloadState`, `ReloadMode`, `ReloadSwap` and
`ReloadPhase` gained `Serialize`/`Deserialize` and `snake_case`, matching
`ProcStatus`'s spelling in the same blob. Nothing on any of them is sensitive
(an app name, two entry ids and three closed enums), so a derived `Debug` is
the deliberate answer for IR-41.

**One new Windows dead-code warning, scoped rather than blanket-allowed.**
Both sites that build a `CarriedReload` are `cfg(unix)` while the type travels
on `Handover`, which is not, so a Windows target reports it as never
constructed. `#[cfg_attr(not(unix), expect(dead_code, ...))]`: an `expect`
rather than an `allow` so a Windows handover would have to delete the line
rather than inherit it, and scoped to the target where it is genuinely dead
rather than hiding anything on unix.

**Tasks 3 and 4 are one commit, as the plan's own correction said.** Removing
`RefusedReason::ReloadInFlight` without the two re-arms ships a swap that sits
in its phase for the rest of the daemon's life, taking `shep reload all` down
with it; building the re-arms without removing the refusal leaves them with
nothing to fire against, which is task 1's mistake for the third time. A
bisect landing between them would find a daemon that loses reloads.

##### Drills, measured

Release build, isolated `SHEP_HOME` at `/tmp/rl/home`. Shepherd **34375**
throughout every drill below: it never moved across any of the seven
handovers, which is what says a handover happened rather than a
stop-and-start. Every `shep daemon reload` exit 0, every one answering
`notice[reload]: the shepherd is now 0.1.20 (pid 34375)`.

**Two of the eight phase-and-mode cells are unreachable by construction, and
the drills say which.** `DrainFirst` is `Serial`-only (an overlap spawns
before it drains) and `Verify` is `Overlap`-only (`post_drain_probe` returns
`None` for a serial reload, which already asked with the slot empty).
Serial's own `AwaitReady` is a single synchronous call inside `reap_drainee`,
which moves it to `DrainOld` before returning, so it cannot be snapshotted at
all. That leaves five reachable cells and five drills.

**1. `Serial`, `DrainFirst`.** A probed app with no `reuse_port`, whose
script traps `SIGTERM`, `graceful_timeout = 25s`.

| | |
|---|---|
| `shep reload sticky` | serial drain begins; the sheep goes `stopping` |
| `shep daemon reload` | 1s in, exit 0 |
| immediately after | shepherd 34375 unmoved, sheep 34396 unmoved, still `stopping` |
| the drain ends | within 21s of the poll starting, which began seconds after the exec, by `SIGKILL`. The child traps `SIGTERM`, so only the escalation could have ended it |
| the replacement | id 1, pid **34917**, `online`, `restarts=0` |

The replacement is the assertion: `DrainFirst` is the one phase with no
replacement yet, so a successor that dropped the `Drainee { new_id: None }`
marker would have deregistered a `stopping` sheep and left the instance slot
empty.

**2. `Serial`, `DrainOld`.** The drain is over, the replacement is `starting`,
and its probe is gated on a file. `listen_timeout = 40s`.

| | |
|---|---|
| before | shepherd 34375, replacement id 3 pid **35158** `starting` |
| `shep daemon reload` | exit 0 |
| immediately after | shepherd 34375 unmoved, 35158 unmoved, still `starting` |
| the probe allowed to pass, **after the exec** | `online` within 2s, same pid |
| the job afterwards | gone: a second `shep reload quick` was accepted rather than refused |

**3. `Overlap`, `AwaitReady`.** A `wait_ready` app whose child sleeps 20s
before writing `{"kind":"ready"}` to fd 3.

| | |
|---|---|
| before | drainee id 5 pid **35651** `stopping`, replacement id 6 pid **36002** `starting` |
| `shep daemon reload` | exit 0 |
| immediately after | shepherd 34375 unmoved, **both** pids unmoved, both statuses unchanged |
| the replacement reports ready | **after the exec**, up the carried socketpair into the successor's re-armed wait |
| the swap completes | once the child's own 20s delay elapsed: 36002 `online`, 35651 drained and reaped, one row left |

The strongest of the five. The readiness signal was written by a child the
successor never spawned, over a descriptor the successor inherited, into a
wait the successor armed, and it committed a swap the successor did not start.

**4. `Overlap`, `DrainOld`.** Both instances up, the drainee on its ladder
ignoring `SIGTERM`, `graceful_timeout = 30s`.

| | |
|---|---|
| before | drainee id 7 pid **36580** `stopping`, replacement id 8 pid **36637** `online` |
| `shep daemon reload` | exit 0 |
| immediately after | shepherd 34375 unmoved, both pids unmoved |
| the drain ends | **t+31s from the exec**, which is `graceful_timeout`, by `SIGKILL` |
| afterwards | one row, 36637, unmoved and `online` |

**4b. The ladder cap.** The same drill with the app's two timeouts three
orders of magnitude apart: `graceful_timeout = 8s`, `kill_timeout = 120s`.

| | |
|---|---|
| before | drainee 39285 `stopping`, replacement 39311 `online` |
| after the exec | both unmoved |
| the drainee dies | **t+8s from the exec** |

8s is `graceful_timeout`. Under task 2's unconditional `LadderCap::Stop` it
would have been 120s.

**5. `Overlap`, `Verify`.** A probed `reuse_port` app, held in `Verify` by
removing the file its probe tests for during the drain. `listen_timeout = 90s`.

| | |
|---|---|
| in `Verify` | replacement id 14 pid **39740** `online`; a second `shep reload verified` refused: `verified is already being reloaded` |
| `shep daemon reload` | exit 0 |
| immediately after | shepherd 34375 unmoved, 39740 unmoved, **still refused**, so the job crossed the exec |
| the probe allowed to pass, after the exec | the job ends **1s** later, with `reload abandoned` nowhere in the log |

Without the second re-arm the job would have ended at the watchdog instead:
`listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK`, 105s, with a
`ReloadAbandoned` for a swap whose replacement was serving the whole time.

**The bound, with a control.** Same app, same route into `Verify`, with the
probe now unable to pass at all: `listen_timeout = 15s`,
`graceful_timeout = 5s`, so the three candidate endings are 15s (the probe's
own deadline), 25s (the watchdog) and never (nothing re-armed).

| | control, no daemon reload | carried, reloaded mid-`Verify` |
|---|---|---|
| shepherd | 34375 | 34375 → 34375 |
| the replacement | 40857 | 42067 → 42067 |
| the job ended | **14.8s** after `Verify` began | **15.4s** after `Verify` began |
| the log line | `reload abandoned: the replacement did not answer its readiness probe once the instance it replaced was gone` | the same line, `new_id=22` |

The offset is what discriminates, and it lands on the probe. Across all seven
handovers: zero panics, zero "the flock is still running and nothing is
supervising it", no `run/handover.json` left behind, and no stray sheep
process.

**What could NOT be drilled, and why it is the unit tier's job.** The
watchdog's own trigger is a message that never comes at all, and the only
child that produces one is wedged in uninterruptible sleep past its own
`SIGKILL`, which is not producible on demand on a Mac. Every other ending is
bounded
by `listen_timeout` or `graceful_timeout`, and the watchdog is
`listen_timeout + graceful_timeout + 5s` by construction, so it can never
fire first against a killable child. `ProcScript::never_reports_its_exit`
exists for exactly this and its doc says so; mutation 1 below is what proves
the re-arm.

##### Mutations

Ten, each applied alone against
`cargo test -p shep-daemon --lib --all-features -- --skip ::slow::` and
reverted afterwards, with the file compared against a pre-mutation copy each
time. Baseline 671 passing.

| # | what was broken | what failed |
|---|---|---|
| 1 | `install_carried_reloads` arms no watchdog | `a_carried_swap_that_cannot_finish_is_still_abandoned_on_time` ONLY (670 pass) |
| 2 | the `Verify` probe re-arm dropped | `a_carried_swap_in_verify_is_asked_again_rather_than_abandoned` ONLY |
| 3 | `install_adopted` writes `ReloadState::None` over every carried marker | four: both drain cases, the serial spawn, and the `AwaitReady` commit |
| 4 | the ladder cap hardcoded to `LadderCap::Stop` | `a_carried_drainee_is_capped_by_graceful_timeout_not_kill_timeout` ONLY |
| 5 | `manually` hardcoded `false` for an adopted replacement | `a_carried_replacement_awaiting_readiness_commits_its_swap` ONLY, at its flag assertion |
| 6 | `install_carried_reloads` never called | five, every carried-swap case including the watchdog |
| 7 | `reloads` made required on the wire | `a_blob_written_before_a_swap_was_carried_still_loads`, **plus `boot::tests::a_blob_on_disk_makes_this_process_a_successor`** |
| 8 | `sheep[].reload` made required on the wire | `a_blob_written_before_a_swap_was_carried_still_loads` ONLY |
| 9 | the dropped-job guard removed | `a_carried_reload_naming_no_registered_instance_is_dropped`, by PANIC inside `arm_reload_deadline` rather than by assertion |
| 10 | the snapshot reports no reloads at all | `a_snapshot_taken_mid_swap_carries_the_job_and_the_markers` ONLY |

**Nothing here is caught only by the end-to-end tier, and 10 is why.** 2b's
lesson was that the successor's arming had no unit-level cover in either
scope, so `install_adopted` could drop `arm_extras` with all 646 lib tests
green. The mirror of that here would have been a snapshot that carried no job:
every one of the nine cases below it builds a `CarriedReload` by hand and
hands it to `spawn_adopted`, so all nine stay green with the snapshot half
deleted. `a_snapshot_taken_mid_swap_carries_the_job_and_the_markers` is the
case that closes it, and it is the only one that drives a real reload on a
live actor and then takes a real snapshot of it.

**Two test-infrastructure findings, each of which cost a run.** An unbounded
`await_event` under a paused clock does not fail when the timer it is waiting
for was never armed. It HANGS, because auto-advance has nothing to advance
to, and mutation 1 took the whole suite down with it before the wait was
bounded. And the paused clock auto-advances inside `handover_snapshot`'s own
awaits, so the first draft of the snapshot case reached `DrainOld` with a
replacement already spawned before the snapshot was taken; it runs on a real
clock with a 200ms `listen_timeout` instead.

##### Docs

`web/src/pages/docs/getting-started.astro` said "A dog or anything mid-reload
sends the reload down the older path instead". This task makes the second half
false, so the clause is gone and a dog is named alone. The generated CLI
reference was regenerated as the docs rule requires: the only drift is the
version banner, `0.1.18` to `0.1.20`, which is two releases of pre-existing
staleness rather than anything this task added: no verb, flag, key, exit code
or payload field changed. `astro build` and `astro check` both clean.

**The phase is complete.** All five tasks are in, `handover::fitness` is down
to two refusals (a wedged log pump, which is a fault rather than a gap, and a
dog, which is phase 3's), and every refusal 2c set out to remove is gone.

### Task 5: validate before the exec

**Files:** `handover/mod.rs`, `boot.rs`.

D2. The predecessor already builds the blob and holds every descriptor. Before `execve`, check the thing the successor will check: each numbered descriptor is open and of the expected kind, no number is repeated, and the blob parses as the type the successor will parse it into.

A failure here is not an error the operator sees. It is the stop-and-start arm, which is the outcome they would have had anyway.

- [x] **Step 1: Write the failing test.** A blob naming a closed descriptor sends the reload down the stop arm instead of exec'ing.
- [x] **Step 2: Run it, watch it fail.**
- [x] **Step 3: Implement.**
- [x] **Step 4: Prove it non-vacuous.**
- [x] **Step 5: Real reload** where a descriptor is closed under the daemon, confirming the flock survives by the stop arm rather than being left unsupervised.
- [x] **Step 6: Commit.**

#### The four claims above hold, checked 2026-08-31

`nix::unistd::execve` at `handover/mod.rs:1120` is the only exec in the crate; every other `Command::new` is in a test. Nothing re-execs a predecessor: `hand_over` has exactly one production caller, `boot::hand_over_carrying`. `boot::rehydrate` at `:630` maps an adopt failure to `BootError::Adopt` and refuses to boot, and `adopt`'s own order leaves the pidfile descriptor open and unowned on a refusal, so the lock stays held. `BootError`'s `Display` arm at `:2141` says the flock is unsupervised and points at `shep muster`.

#### Step 1's case is weaker than the plan thought, and the shipped tests are stronger

**A closed descriptor was already refused before the exec.** `hand_over` clears `FD_CLOEXEC` on every number the blob names, and a closed one meets `EBADF` there. So a test built on "a closed descriptor" would have passed with no fix at all.

Four cases were genuinely uncovered, and they are what the tests use:

- a descriptor that is OPEN but not the kind its slot is adopted as. The sweep clears it happily, the exec happens, and the successor refuses.
- a number below the stdio floor. Open, clears fine, refused after.
- a blob naming one number twice. Clearing `FD_CLOEXEC` is idempotent by design, so a repeat crosses untouched and builds two owners on the far side.
- a blob a successor could not read back off disk. For a handover this is the reachable case rather than the paranoid one, since a successor is by definition a different build: one that has moved `VERSION` refuses at `load_value`.

#### What was built

`handover::adopt::dry_run`, called from `boot::hand_over_carrying` between the fitness gate and `hand_over`. The two gates ask different questions: the first whether this flock is a shape a handover carries, the second whether the blob describing it is one a successor could adopt.

**It runs the successor's own adoption rather than describing one.** A second implementation of "is this descriptor adoptable" is worse than none: if the successor's checks tighten and a hand-written copy does not, the rehearsal passes, the exec happens, and the boot still fails with the predecessor gone. So nothing re-states a check.

| what could drift | what stops it |
|---|---|
| the per-kind checks | `dry_run` calls the same `adopt_listener`/`adopt_pipe`/`adopt_stdin`/`adopt_log`/`adopt_channel`/`adopt_fd` the successor calls, over an `F_DUPFD_CLOEXEC` duplicate so an adoption that takes ownership can run against a descriptor the predecessor must keep |
| which number is which slot | one array, `CarriedFds::all_kinded`, pinned against `CarriedFds::all` (what the `FD_CLOEXEC` sweep walks) by a test; a seventh descriptor changes `all`'s return type and stops `all_kinded` compiling |
| the repeated-number rule | `refuse_repeated_fds` itself, called rather than re-derived |
| the floor-and-open probe | `sys::adopt_handover_fd`'s own, extracted as `sys::adoptable_fd` and shared. Checked against the BLOB's number, never the duplicate's: a duplicate is always open and always above the floor, so a rehearsal that only inspected duplicates would wave through exactly the two blobs the successor is certain to refuse |
| the parse | `Handover::load_value`, the successor's own entry point, and the descriptors are rehearsed against the REPARSED blob |
| a slot dispatched to the wrong adoption | `the_rehearsal_and_the_adoption_agree_on_every_slot` walks all six and compares verdicts |

Two seams are named in the code rather than hidden. The successor reads bytes with `from_str` while this goes through `to_value`; and the four socket and pipe adoptions set `O_NONBLOCK`, which is a property of the open file description and so reaches the original through the duplicate — a write of the value already there, because tokio does not accept a blocking one.

`VERSION` unmoved: nothing about the blob format changed.

##### The measurement

Fault injected identically in both builds (uncommitted, `SHEP_HANDOVER_FAULT`): one sheep's `out_pipe` replaced with an open `/dev/null`, which is open enough for the `FD_CLOEXEC` sweep and not a pipe. One quiet sheep, so the "unsupervised orphan" outcome is visible rather than being masked by `SIGPIPE` — a chatty sheep dies when the successor exits and closes the last reader, which is a different bad ending and hides this one.

| | before (fix stashed) | after |
|---|---|---|
| `shep daemon reload` exit | 0 | 0 |
| shepherd, before → after | 22988 → **29421** | 30605 → **66107** |
| the sheep, before → after | 23052 → **29442** | 30680 → **66158** |
| old sheep afterwards | **alive, `ppid` 1, orphaned** | gone |
| `quiet.sh` processes afterwards | **2** | **1** |
| what the shepherd supervises | 1 of the 2 | the 1 |
| `run/handover.json` afterwards | **left behind** | gone |

The blob left behind names `out_pipe: 16` — the injected number — for `quiet` at pid 23052, which is the orphan. It is on disk because the successor's `adopt` refused, and `adopt` unlinks only on success.

The same fault by raw `SIGHUP`, so the two log lines can be read side by side:

```
BEFORE  error[failure]: the daemon failed to boot: this shepherd was handed a flock it could not
        take over: sheep 'quiet' stdout pipe is not a readable pipe: not a pipe. The flock is still
        running and nothing is supervising it. ...
        -> sheep 84773 alive at ppid 1, no shep process anywhere, blob on disk

AFTER   WARN shep_daemon::boot: SIGHUP: this flock could not be handed to a successor; stopping
        gracefully instead ... refusal=a successor could not have adopted this flock, so none was
        started: sheep 'quiet' stdout pipe is not a readable pipe: not a pipe. This is a shep bug
        worth reporting: the descriptors are ones this shepherd opened itself, and the check that
        refused them is the successor's own. ...
        -> flock stopped by the ladder, no orphan, no blob
```

The refusal reaches the operator's log carrying the successor's own wording, so it names the sheep and the stream.

**Residual, pre-existing and not touched here.** Both reloads took 10s, because `daemon::hand_over`'s `await_successor` waits out `admin::KILL_TEARDOWN_WAIT` for a successor that in the refusal case was deliberately never started. That is the CLI's behaviour for any post-signal handover failure, and shortening it would mean the CLI learning that the predecessor refused, which a signal cannot tell it.

##### Mutations

Ten, each applied alone against `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::` and reverted afterwards. Baseline 656 passing.

| # | what was broken | what failed |
|---|---|---|
| 1 | the `dry_run` call removed from `hand_over_carrying` | `a_sighup_over_a_blob_no_successor_could_adopt_refuses_before_it_execs` only |
| 2 | `adoptable_fd` checks the duplicate's number, not the blob's | `a_reserved_or_closed_number_is_refused_before_the_exec`, plus the boot case at its `-1` assertion |
| 3 | `refuse_repeated_fds` dropped from `dry_run` | `a_blob_naming_one_descriptor_twice_is_refused_before_the_exec` only |
| 4 | the reparse skipped, rehearsing the blob as handed | `a_blob_a_successor_could_not_read_back_is_refused_before_the_exec` only |
| 5 | `all_kinded` labels `out_pipe` as `Stdin` | seven, including `every_carried_number_is_kinded_in_the_same_order` |
| 5b | `all_kinded` swaps the `out_pipe`/`err_pipe` fields, slots unchanged | `every_carried_number_is_kinded_in_the_same_order` at its `all()` equality |
| 6 | the `Channel` slot dispatched to `adopt_log` | `the_rehearsal_and_the_adoption_agree_on_every_slot` only |
| 7 | the sheep walk skipped | three, including the open-but-wrong-kind case |
| 8 | one duplicate leaked per rehearsal | `a_rehearsal_leaks_no_descriptors` only |
| 9 | the adoption handed the original instead of a duplicate | the test binary ABORTS: `IO Safety violation: owned file descriptor already closed`, attributed to `a_rehearsal_leaves_every_descriptor_it_checked_working` |
| 10 | `rehearse` refuses everything | six, including `a_blob_a_successor_could_adopt_passes_the_rehearsal` |

6 is the one worth keeping. It is exactly the drift this task was told not to ship — a rehearsal that grows lax while the adoption stays strict — and one test catches it, alone, with the other 655 green.

9 is the second. Without the duplicate the rehearsal closes the predecessor's own descriptors, and Rust's IO-safety net turns that into an abort rather than a subtle failure.

---

### Review finding, closed after the tasks: `ready_failed` was not carried

Found in review of the finished phase, not by a task. The blob carries
`pending_delete`, `manual` and `reload`; `SheepSlot::ready_failed` is the
fourth slot fact and it was still going out with the predecessor's image.

**It is the same class as the four above, arriving by the one route none of
them took.** Nothing about it is a timer. It is a VERDICT — the flag both
abandonment arms set when a reload's readiness verification fails — and
`reload_eligible` is `status == Online || ready_failed`, so it is the whole of
what keeps an abandoned reload's leftover replaceable. That instance is left
`Starting` on purpose, because `Online` over a process that is up and not
serving is the false success the serial mode exists to remove. Drop the flag
and the status is all that is left, and the status says the instance cannot be
replaced. `handle_reload` still answers `Ok`, because it replies to the caller
before its selector pass runs — so the rollback reload a deploy tool issues
after a bad release reports success and replaces nothing. Exactly the
looks-fine-is-not shape.

#### What was built

`CarriedSheep::ready_failed: Option<bool>`, threaded through `from_entry` and
restored in `install_adopted`, `VERSION` unmoved and no `#[serde(default)]`,
following the three siblings. One thing is NOT copied from them: what an
absent key means. Each of those was a gate refusal before it was a field, so a
missing key proves the fact was false. This was never a refusal — a
predecessor carried such an instance happily and silently dropped the flag —
so `None` is that predecessor saying nothing rather than saying "no". `false`
is still the only reading available, and it is exactly what a successor
assumed before the field existed, so an older blob adopts the way it always
did. The field's own doc makes that argument rather than borrowing the
sibling's.

**The readiness re-arm is now gated on the flag, and that is the half the
finding did not name.** `install_adopted` re-arms a wait for every `Starting`
sheep, which is right for one still mid-start and wrong for one whose wait
already ran and failed. `handle_ready_result`'s `TimedOut` arm goes `Online`
ANYWAY, and `went_online` clears `ready_failed` on the way past, so an ungated
re-arm writes the false success the abandonment arms refuse to write AND
spends the carried flag one `listen_timeout` after the exec, before any
rollback can use it. Both `Starting` sheep look identical; the carried flag is
the only thing that tells them apart, which is why it is read before the
re-arm decides rather than at either `SheepSlot` literal.

#### Drill, measured

An app with an exec `readiness_probe` gated on a sentinel file, `autorestart =
false`, `listen_timeout = "20s"`, under an isolated `$SHEP_HOME`. Serial
reload, since a probe without `reuse_port` takes it. Start with the sentinel
present, remove it, reload: the drain finishes, the replacement never answers,
and the reload is abandoned at 20s leaving `id 1` `starting` with
`ready_failed` set. Then `shep daemon reload`, then the rollback.

| after the handover | before | after |
|---|---|---|
| shepherd pid across the exec | unmoved | unmoved |
| sentinel restored, `shep reload probed` | exit 0, `id 1` untouched | exit 0, `id 1` drained |
| the flock 6s later | `id 1`, pid unchanged, `online` | `id 2`, new pid, `online` |

The `online` in the before column is the second failure in one row: the
rollback replaced nothing, and the release that never served is being reported
as serving. A control run with no handover at all replaces the instance
correctly, which is what pins the loss on the exec rather than on the
abandonment.

The re-arm gate has its own pair, same setup with the sentinel left absent, so
the probe keeps failing:

| 28s after the handover, probe still failing | before | after |
|---|---|---|
| `shep flock` | `online` | `starting` |

#### Mutations

| mutation | reddens |
|---|---|
| the restore in `install_adopted` dropped (both slot literals) | `a_carried_ready_failed_instance_is_still_replaceable` only |
| the `&& !ready_failed` gate on the readiness re-arm removed | `a_carried_ready_failed_instance_gets_no_fresh_readiness_wait` only |
| `HandoverDraft` reads `false` instead of the slot | `the_snapshot_carries_the_actors_counters_and_slot_state` only |
| `#[serde(skip_serializing)]` on the field | `a_blob_written_before_ready_failed_was_carried_still_loads`, plus the two existing round-trip cases |
| the field made non-optional | `a_blob_written_before_ready_failed_was_carried_still_loads` at its legacy half |

The third is the one worth keeping. The write side was a hole: every other
case builds its own `CarriedSheep`, so a daemon that never put the flag IN the
blob passed all of them. It was found by mutating rather than by reading, and
it is the half of a carry that is easiest to leave out.

`a_blob_carrying_a_failed_readiness_verdict_passes_the_rehearsal` has no
mutation of its own and is not claimed to: `dry_run` reparses whatever struct
it is handed, so nothing short of a hand-written refusal reddens it. It is a
regression guard on the reparse, the same shape and the same worth as
`a_blob_carrying_a_swap_in_flight_passes_the_rehearsal`.

## Inherited from 2b — DECIDED AND CLOSED

**The re-armed restart delay was a judgement call, it shipped in v0.1.20, and
the maintainer's ruling has replaced it.** The delay is now anchored to the
sheep's own exit: a sheep that exited at T comes back at T plus its delay,
however many handovers happen in between. Built on
`feat/handover-restart-deadline`, not in 2c itself.

What this section recorded, and what came of it. 2b's task 8 re-armed a
carried `WaitingRestart` sheep with `backoff::restart_delay(config, 1)`, the
delay a FIRST unstable exit would get, so an app with `restart_delay = "1h"`
reloaded at minute 59 waited another full hour. The third option this section
named is what was built: a `SystemTime` deadline, which survives the exec
where the monotonic `Instant` cannot. The predecessor records the moment the
respawn falls due, carries it on `CarriedSheep::restart_due`, and the
successor arms for what is left.

Two things this section did not have, settled by building it:

- **An absolute moment, not a carried remainder.** Subtracting at snapshot
  time and carrying a `Duration` is the other way to make a monotonic
  deadline cross the exec, and it is immune to the clock jump this section
  worried about. Rejected anyway, because each hop would add its own handover
  duration back on: four reloads inside one delay would drift by four
  handovers. An absolute moment absorbs each handover rather than
  accumulating it, which is what "however many handovers" actually asks for.
- **The clock jump is clamped, not simply accepted.** `adopted_restart_delay`
  bounds the remainder by `restart_delay(config, 1)` — the value the old code
  used — so a backward jump can at worst give back exactly v0.1.20's
  behaviour. That turns this section's "acceptable trade" into a bounded one.

The three cases (elapsed, clock-jumped, absent) and their rulings live in that
function's own doc and in the commit message.

Measured under an isolated `$SHEP_HOME`, an app with `restart_delay = "45s"`
that exits immediately, reloaded 15s into the wait, with a never-reloaded
control alongside. The shepherd's pid was unchanged across every reload, so
each one was a carry rather than a fallback stop-and-start.

| binary | reloads | exit-to-respawn |
|---|---|---|
| v0.1.21 | none (control) | 45.0s |
| v0.1.21 | one, at t+15s | **60.1s** |
| v0.1.21 | one, at t+15s (repeat) | **60.1s** |
| this branch | none (control) | 45.0s |
| this branch | one, at t+15s | **45.0s** |
| this branch | two, at t+15s and t+30s | **45.0s** |

The 15.1s excess before is exactly the reload offset: the clock restarted. The
two-reload row is the chain staying flat.

**One residual, and it is a tradeoff rather than an oversight.** All six
mutations are caught by the daemon lib tier, so nothing here is e2e-only --
but that also means no permanent test drives the deadline through a REAL
`execve`. `a_sheep_owed_a_restart_still_gets_one_after_a_daemon_reload` in
`crates/shep-cli/tests/cli_e2e.rs` reloads a real `WaitingRestart` sheep and
would be the place for it; it asserts that the sheep comes back, not that it
comes back on time. Adding the timing half needs a delay long enough to
separate "what was left" from "the whole thing" on a loaded runner, plus a
deliberate wait before the reload, which is roughly +20s on the slowest suite
and makes the assertion a claim about the machine's speed as much as about
shep -- the exact shape that has already cost this project four red CI runs
and put three test groups in `mod slow`. The drill above covers it instead,
twice per side with a control and a shepherd-pid check. Buying the permanent
guard is the maintainer's call.

## The reload drill

Unchanged from 2b's, in `docs/writing-plans/plans/2026-08-30-handover-phase2b.md` under "The reload drill, exactly". Every constraint there still binds: the `awk` rate, `$SHEP_HOME` under 103 bytes, stopping the sheep before counting, seam-aware counting.

## Gate

Per `CLAUDE.md`. Inner loop `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`. One cargo shape per task. The Windows cross-check gets its own `CARGO_TARGET_DIR`.

Counts at the branch point: **647** daemon lib, roughly **2110** workspace. A shape, not a checksum. After task 2: **651** daemon lib with `--skip ::slow::`, **2125** workspace.

**CI's `slow` tier flaked three times on 2b's night** — a macOS `EIO` on a pidfile, and the Windows node-pipe budget test twice. Both are documented as machine-speed-sensitive. Read a `slow` failure against `main`'s own history before treating it as yours.
