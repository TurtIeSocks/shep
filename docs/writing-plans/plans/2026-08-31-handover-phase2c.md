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

Counts at the branch point: **647** daemon lib, roughly **2110** workspace. A shape, not a checksum. After task 2: **651** daemon lib with `--skip ::slow::`, **2125** workspace.

**CI's `slow` tier flaked three times on 2b's night** — a macOS `EIO` on a pidfile, and the Windows node-pipe budget test twice. Both are documented as machine-speed-sensitive. Read a `slow` failure against `main`'s own history before treating it as yours.
