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

1, 2 and 5 are independent. 3 and 4 are coupled and 4 depends on 3. Nothing after 2 may start before 1 and 2 are green, because all five touch `refusal()` in `handover/mod.rs` and would conflict.

---

### Task 1: `pending_delete`

**Files:** `crates/shep-daemon/src/handover/mod.rs` (the refusal and `CarriedSheep`), `crates/shep-daemon/src/supervisor.rs` (`install_adopted`).

A `bool` on the blob, restored onto the entry. No timer, no linked id, no waiter beyond D1's reply.

- [ ] **Step 1: Write the failing test.** A carried sheep with `pending_delete` set is adopted with it still set, and its next exit deregisters it rather than respawning it.
- [ ] **Step 2: Run it, watch it fail** on `Refused(PendingDelete)`.
- [ ] **Step 3: Implement.** Field on `CarriedSheep`, `Option<bool>` so an older blob loads as `None` — do NOT move `VERSION`, per tasks 4, 5 and 6's precedent.
- [ ] **Step 4: Prove it non-vacuous.** Drop the field from the blob; that test and only that test fails.
- [ ] **Step 5: Real reload.** `shep delete` a sheep whose exit is slow, reload mid-delete, confirm the delete still completes and the row is gone.
- [ ] **Step 6: Commit** with the plan section updated.

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
