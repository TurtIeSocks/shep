# shep Phase 6 — Reload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **REQUIRED SUB-SKILL:** Use the `shep-idiomatic-rust` skill before writing or reviewing any Rust here.

**Goal:** `shep reload` — replace each instance of an app with a fresh one, one at a time, without taking the port down.

**Architecture:** Spec §4's per-instance state machine, `SpawnNew → AwaitReady → DrainOld → ReapOld`. The replacement registers under a **new id** in the **same instance slot**; both entries coexist until the drainee is reaped. Reload replies as soon as it is accepted and reports progress on the bus.

**Tech Stack:** the existing actor, kill ladder, readiness gate and bus. No new dependency.

---

## The honest claim

**shep does not provide zero downtime. It provides an overlap in which the application can achieve it.**

Measured on both platforms: when the old listener closes, **its accept backlog is reset** — Linux `RESET`, macOS `EPIPE`. Every connection queued and not yet accepted dies with it, and a busy server always has a non-empty backlog at the instant it exits. So a reload is downtime-free exactly insofar as the app stops accepting, drains, and exits inside `graceful_timeout`. An app that ignores `SIGTERM` until shep's `SIGKILL` drops its whole backlog on every reload, and nothing shep does prevents that.

Every doc, changelog line and test name in this phase must be true under that sentence. **Do not write "zero-downtime" unqualified anywhere.**

`reuse_port = true` is the operator asserting the *app* sets `SO_REUSEPORT` itself — shep binds nothing (settled 2026-08-09, and `AppConfig::reuse_port`'s doc was corrected to say so). No child in this repo has ever bound a socket, so `reuse_port` and `graceful_timeout` currently have **zero readers**; this phase gives them their first.

---

## Global Constraints

1. **MSRV 1.88, edition 2024.** No new dependency.
2. **`ProcStatus::Stopping` is the drainee's status** (Rin, 2026-08-10). It already exists, is already on the wire, and is set by nothing — this phase gives it its first writer. Chosen over a new `Draining` variant because `ProcStatus` is **not** `#[non_exhaustive]`, so a variant is a wire *and* API break. This single choice also closes two other findings for free — see Tasks 3 and 4.
3. **Rule 9** one owner per constant; **Rule 10** no task-relative phrasing in shipped comments; **Rule 11** advance a paused clock in steps no larger than the shortest period of the loop under test, and make negative assertions poll a bounded window (`timeout` + `recv`), never a bare `try_recv`.
4. **The actor never awaits on its own loop.** `CRITICAL-2` at `supervisor.rs`. Reload's waits belong in spawned tasks reporting back as `Msg`, the shape `spawn_readiness_task` established.
5. **`CommandOrigin` keeps two variants.** Its exhaustive match is a deliberate pin; do not add a third.
6. Workspace lints deny `missing_docs`, `missing_debug_implementations`, `undocumented_unsafe_blocks`, `clippy::missing_errors_doc`. `#![forbid(unsafe_code)]` outside `shep-daemon/src/sys.rs`. Style per `docs/idiomatic-rust.md`.
7. **Gates**, each from its own command with `$?` captured directly, **never piped** — a pipeline's `$?` is the last command's, and that has produced a false green six times on this project:
   ```
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-features
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
   cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
   cargo +1.88 check --workspace --all-targets --all-features
   cargo test --workspace --all-features -- --test-threads=1
   ```
   Plus both bench-crate gates from inside `benches/`. Count `Running`/`Doc-tests` lines against `test result:` lines — **baseline 15 against 15** — and confirm every crate ran rather than reading a green tail.
8. **Baseline: 705 passing + 1 pre-existing ignored** (`a_dropped_child_runs_as_the_requested_user`, needs root).
9. **zsh**, not bash. `${PIPESTATUS[0]}` yields an empty string. Quote glob-bearing arguments. **Commit messages containing backticks must use `git commit -F -` with a quoted heredoc.**
10. **Reap what you spawn**, including on failure paths. Short `$SHEP_HOME` — macOS caps the socket path near 97 chars.
11. **Restore mutations from a snapshot you own, never `git checkout <file>`** — that reverts to `HEAD` and silently discards uncommitted work.
12. **Measure blast radius with `--no-fail-fast`.** Without it the integration binaries never run once the lib binary fails, and a radius of 3 reads as 1.

## Fixture sizing — the trap that has cost this project twenty-two tests

`ScriptedRunner` answers `SpawnFailed("script exhausted")` once its scripts run out; the supervisor then emits `Errored`, and that state is frequently **indistinguishable from the failure under test**. Reload makes this worse: every reload is *two* spawns per instance, so a pool sized for a correct run is short by exactly the number a broken implementation needs. **Count the scripts, state the worst case, and name the spawn a broken implementation performs that a correct one does not.**

A second variant, discovered in Phase 5: a pool one short lands a sheep *pumpless*, which reads exactly like a pump that was never asked.

---

## Settled decisions

Recorded so no task re-litigates them. Rin ruled 2, 8 and 9; the rest follow from those or from existing precedent, and are mine — flag any you believe is wrong rather than working around it.

| # | Decision |
|---|---|
| 1 | Drainee status is `Stopping` (Rin). |
| 2 | ~~`is_running` excludes `Stopping`, which fixes the muster-roll inflation.~~ **Amended after Task 1's audit:** `is_running` already excluded it, and the "inflation" was never a correctness bug — see the dropped Task 3. Reusing `Stopping` is still right; its payoff is Task 4's guards. |
| 3 | `ReloadState::SpawningReplacement { new_id }` lives on the **old** entry (it names the new); `Draining { old_pid }` on the **new** entry (it points back at what it must outlive). |
| 4 | Reload reuses `CommandOrigin::Operator`. No third variant. |
| 5 | An operator's `stop`/`delete` mid-reload **aborts the reload** and applies to both ids. |
| 6 | `DrainOld`'s ladder is capped by `graceful_timeout` **replacing** `kill_timeout`, and keys the `{"kind":"shutdown"}` message on `shutdown_with_message` exactly as `kill_process` does today. |
| 7 | Reload is **rejected while `shutting_down`**, like `Start` and `Restart` — it spawns, so CRITICAL-1 applies. |
| 8 | Reload of a **not-running** sheep is a **no-op success**, listed in the reply. Reload's contract is "replace a running instance"; there is nothing to replace, and starting it would surprise an operator who deliberately stopped it. `shep start` exists. |
| 9 | Reload **does not re-read config**. It reuses the stored `ResolvedApp` and the credentials resolved at the first `Start` — re-reading collides with `ProcessEntry::credentials`' documented once-only rule and would change the verb's argument shape. A config-rereading verb is a different feature. |
| 10 | A reload arriving while one is in flight **for the same name** is refused with a clear error. |
| 11 | New `ProcessEventKind` variants for progress. Older subscribers cannot decode them and drop the frame silently — acceptable, and stated in the changelog. |
| 12 | `restarts` **carries over** to the replacement. A reload is not a crash, but the count is the operator's view of that instance's history. |
| 13 | `reopen` adopts `flush`'s "every writer to the path" rule — Task 6. |

---

## Task 1: `Stopping` becomes a real status

**Files:** Modify `crates/shep-daemon/src/supervisor.rs`, `crates/shep-daemon/src/entry.rs`.

**Interfaces:** Produces a settable `ProcStatus::Stopping`; consumed by every later task.

`ProcStatus::Stopping` is on the wire and set by nothing. Before reload can use it, establish what it means and what already reacts to it.

- [ ] **Step 1: Audit every match on `ProcStatus`** and report what each does with `Stopping` today. `handle_extra_restart`'s guard already rejects it — that is the free win in Task 4, so confirm it rather than assuming it.
- [ ] **Step 2: Give it a doc** stating it means "this instance is going away and is not a restart target", and that it is reachable only from reload.
- [ ] **Step 3: Test** that a sheep in `Stopping` is rejected by `handle_extra_restart` and by `handle_restart_due`. Fails if either guard stops covering it.
- [ ] **Step 4: Commit** — `feat(daemon): make Stopping a status the engine can actually set`

---

## Task 2: `ReloadState` gets an owner and a reader

**Files:** Modify `crates/shep-daemon/src/entry.rs`.

`ProcessEntry::reload` carries an `#[expect(dead_code)]` that **becomes a compile error the moment a reader lands** — budget for it here rather than being surprised in Task 5.

- [ ] **Step 1: Apply settled decision 3** — `SpawningReplacement { new_id }` on the old entry, `Draining { old_pid }` on the new. Document why each sits where it does; the enum as landed does not say, and the next reader will otherwise guess.
- [ ] **Step 2: Remove the `#[expect]`** and confirm the build is clean without it.
- [ ] **Step 3: Test** the transitions the state machine will drive.
- [ ] **Step 4: Commit** — `feat(daemon): say which entry holds which half of a reload`

---

## Task 3: ~~the muster roll stops double-counting~~ — DROPPED

**Dropped after Task 1's audit refuted its premise.** Kept here rather than
deleted, because the reasoning is what justifies not doing the work.

The task was written on this claim: *mid-reload both entries pass
`is_running`, so a daemon reboot in that window resurrects `instances = 1` as
two.* Both halves are false.

- **`is_running` never counted `Stopping`.** It is
  `matches!(status, Online | Starting | WaitingRestart)`. Step 1 would have
  been a no-op.
- **`instances_running` is not a count on the way back in.** Its only
  production reader is `restorable`'s `if saved.instances_running == 0 ||
  !app.autostart { continue }` — a **boolean gate**. The number of instances
  a restored app actually starts comes from `app.config().instances`
  (`supervisor.rs:920`, `instance_slots(&existing, app.config().instances)`).
  A roll recording `2` and a roll recording `1` restore identically.

What survives is smaller and belongs to Task 5, not here: between the
replacement reaching `Starting` and the drainee being marked `Stopping`, both
entries satisfy `is_running`, so a roll written in that window records a
count the flock does not have. It is cosmetic — a human reading the roll file
mid-reload — but it is free to avoid, so **Task 5 marks the drainee
`Stopping` no later than the replacement's spawn**, and pins that ordering
with a test.

Settled decision 2 in the table above is amended accordingly: reusing
`Stopping` is still correct, but its payoff is the guards in Task 4, not a
muster-roll fix that was never needed.

---

## Task 4: close the liveness window

> **Runs after Task 5, not before it.** Two reasons, both found during
> execution. Step 1's "decide the marker timing" is a question only the state
> machine can answer — when the drainee is claimed is part of how the machine
> sequences, so deciding it here would mean deciding it twice. Step 2's test
> drives a real reload of an app with a `liveness_probe`, which cannot exist
> until Task 5 lands. What remains here after Task 5 is the integration proof
> that the window is actually shut.
>
> The unit-level half is already done: Task 1's
> `a_stopping_sheep_rejects_an_extra_restart` pins the guard that drops an
> automatic report against a drainee. This task proves the same thing end to
> end, through a reload the daemon actually performs.

**Files:** Modify `crates/shep-daemon/src/supervisor.rs`.

The exposed window is **`AwaitReady`, not `DrainOld`** — `claim_manual` drops an automatic report once the drainee carries a `manual` marker, so the ~3s while the replacement starts and the drainee is `Online` *and unclaimed* is the gap. An app whose liveness probe fails during that window gets **restarted rather than reaped**.

- [ ] **Step 1: Decide the marker timing** — the fix is when the drainee is claimed, not a change to liveness. State the reasoning where the code does it.
- [ ] **Step 2: Test** a reload of an app with a `liveness_probe` whose drainee stops answering during `AwaitReady`, asserting the drainee is reaped and not respawned. **Size the fixture for the respawn a broken implementation performs** — that is the spawn the correct one does not.
- [ ] **Step 3: Commit** — `fix(daemon): stop a draining instance restarting itself`

---

## Task 5: the state machine

**Files:** Modify `crates/shep-daemon/src/supervisor.rs`, `crates/shep-daemon/src/kill.rs`.

The core. Everything before this was clearing the ground.

**`spawn_fresh` must diverge three ways for a replacement**, and each has a reason worth keeping in the code:
- **Always-gated readiness.** Note carefully: this is not an option but the only way the feature works. `spawn_fresh`/`respawn` gate on `!Heuristic`, so **a Heuristic app has no readiness task today** — `ready.rs`'s Heuristic arm is fully implemented, tested, and has never once run. Reload is what runs it.
- **The same instance slot.** An app deriving its port from `SHEP_INSTANCE` would otherwise bind a different port, defeating the entire mechanism.
- **A new id.** The proptest invariant is "never two live pids for one *id*"; a same-id replacement violates it, a new-id one does not.

- [ ] **Step 1: `SpawnNew`** — replacement registered, same slot, new id, gated.
- [ ] **Step 2: `AwaitReady`** — `handle_ready_result` must know whether a wait belongs to a reload. A timeout is **new-instance failure → abort** for `Channel` and `Probe`; for `Heuristic`, elapse means **success**, because `await_ready` returns `Ready` on elapse. Getting this backwards makes every Heuristic reload fail.
- [ ] **Step 3: `DrainOld`** — the kill ladder capped by `graceful_timeout` per settled decision 6. `kill_process` hardcodes `kill_timeout` and `SheepCtl` has one payload-free variant, so this is **two changes, not one**.
- [ ] **Step 4: `ReapOld`** — deregister the drainee's slot. **Nothing reaps it today**: its pump dies via `logs_tx.closed()`, but the `SheepSlot` lives forever without this.
- [ ] **Step 5: Abort semantics** — a failed replacement aborts the rest and keeps old instances running (spec §4). The failed replacement must itself be killed through the ladder, since it may have forked lambs. Say what its entry becomes.
- [ ] **Step 6: Test the machine at the paused-clock tier** — ordering, timeouts, abort, budget, and the marker interactions. All of this is reachable with the fake; only the downtime claim is not.
- [ ] **Step 7: Commit** — `feat(daemon): reload an app one instance at a time`

---

## Task 6: the log plane mid-reload

**Files:** Modify `crates/shep-daemon/src/supervisor.rs`.

**Reload makes every app temporarily a shared-log-path app**, because `assemble` derives log paths from name plus instance and both entries share a slot. This was pre-existing for `merge_logs` and is now reachable by default.

- [ ] **Step 1:** `flush`'s widened barrier **already covers the drainee** — verified via path equality in the `pumps` filter. Add a test so it stays true.
- [ ] **Step 2:** `reopen` is **selector-keyed only** and does not. Adopt `flush`'s "every writer to the path" rule per settled decision 13.
- [ ] **Step 3: Test** both verbs against an app mid-reload. Fails if either stops covering the drainee's pump.
- [ ] **Step 4: Commit** — `fix(daemon): reach every writer to a path a reload is sharing`

---

## Task 7: pin the extras overlap

**Files:** Modify `crates/shep-daemon/src/extras.rs`.

The new id arms and joins the name group **before** the old id disarms, so `ExtrasRegistry::disarm` sees a non-empty member set and does not tear down the group's watch or cron worker. Reload is the only operation with that overlap.

Member-set semantics are design. **The ordering reload depends on is not — nothing in `ExtrasRegistry` enforces it.**

- [ ] **Step 1: Pin the ordering** with a test that fails if it inverts.
- [ ] **Step 2: Mind the test-design trap the research named** — a torn-down-and-rebuilt group looks **identical** to an untouched one in a test environment. Assert something that distinguishes them, and say in the comment what that is.
- [ ] **Step 3: Commit** — `test(daemon): pin the arming order a reload depends on`

---

## Task 8: the CLI verb and the wire

**Files:** Modify `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/rpc.rs`, `crates/shep-cli/src/{cli.rs,main.rs}`, `crates/shep-cli/src/commands/lifecycle.rs`.

- [ ] **Step 1:** `Request::Reload { selector }` and a response. Additive under `#[non_exhaustive]`; `PROTOCOL_VERSION` stays 1. Add the fixture row and **verify the regenerated snapshot delta is only your addition** — a regenerated fixture is the easiest place in a diff to hide a change.
- [ ] **Step 2: Reply early, report on the bus** (Rin, 2026-08-09). A reload of N instances costs roughly N × (`listen_timeout` + `graceful_timeout`) ≈ N × 11s, and `MAX_DEADLINE_MS` is 60s, so a synchronous reply cannot cover six instances. New `ProcessEventKind` variants per settled decision 11.
- [ ] **Step 3: Required selector**, matching `stop`/`restart`/`delete` — reload restarts processes.
- [ ] **Step 4: Settled decisions 7, 8, 10** — rejected while shutting down; no-op success for a not-running sheep; refused while one is in flight for the same name.
- [ ] **Step 5: Commit** — `feat: shep reload`

---

## Task 9: the empirical proof

**Files:** Create a fixture server; modify `crates/shep-daemon/tests/daemon_e2e.rs`.

**This is the task the phase exists to justify.** Everything above is testable at the paused-clock tier. The downtime claim is not: `ScriptedRunner` involves no sockets at all.

- [ ] **Step 1: One fixture server, two behaviours** — well-behaved (closes its listener on `SIGTERM`, drains, exits) and `SIGTERM`-ignoring. **The gap between those two runs is the finding.** A test asserting unconditional zero-downtime would be green on the good fixture and lying about production.
- [ ] **Step 2: Assert a count, not a boolean.** "Zero connection errors with a well-behaved app, non-zero with a signal-ignoring one."
- [ ] **Step 3: Platform split** (Rin, 2026-08-10). **Linux keeps feeding the old listener until it closes; macOS does not** — so Linux is where a reload can actually drop connections, and macOS is where the bug cannot manifest. Assert the error count **on Linux only**; on macOS assert the weaker property that still holds — the reload completes, the new instance serves, the drainee is reaped. State in the test doc what each platform is asserting and why they differ. A single shared assertion would be vacuous on macOS or flaky on Linux.
- [ ] **Step 4: Mind the port.** A fixed port under a runner with no serialization will collide; `serial_test` is not a dependency and `SIGNAL_TEST_LOCK` is this repo's precedent for why that bites.
- [ ] **Step 5: Commit** — `test: prove what a reload does to a live connection`

---

## Task 10: docs, changelogs, and the report

- [ ] **Step 1: Renumber.** "Phase 6" is currently used by three research docs for the UX-surface phase. Reload takes 6; that phase becomes 7. Fix all three in this commit so the collision does not survive.
- [ ] **Step 2: `map.md`** — verify every claim against the code before writing it, and **cite by symbol, not line number**. That file has twice been synced to what a plan expected rather than what shipped.
- [ ] **Step 3: Changelogs** (IR-45), reconciled not appended. **Nothing may say "zero-downtime" unqualified** — see The honest claim.
- [ ] **Step 4: Report to Rin** — every judgement call made on her behalf, anything left unfixed, and the measured cost of the e2e tier.
- [ ] **Step 5: Commit** — `docs: record what reload does and does not promise`

---

## Exit criteria

1. All ten tasks complete and individually reviewed.
2. Every gate green from its own exit code, including both bench-crate gates and **the serial run** — which was red on `main` before Phase 5 and is now load-bearing.
3. `grep -rn "zero.downtime" crates/ docs/` returns nothing unqualified.
4. `ProcStatus::Stopping` has a writer, and `grep` finds no `#[expect(dead_code)]` on `ProcessEntry::reload`.
5. A reload of a `liveness_probe` app does not respawn its drainee — pinned by a test that fails if the marker timing regresses.
6. The muster roll records the configured instance count mid-reload, not the transient one.
7. Both `flush` and `reopen` reach every writer to a shared path during a reload.
8. **Both halves of the marker grep**: files this phase creates, *and* lines it adds to files it only modifies. Phase 4 skipped the second half and a marker shipped.
9. Every test added carries a "fails if" comment naming the mutation it catches, and a reviewer picking three at random can break the implementation in the named way and watch the named test go red.
10. Neither suite run leaves a process reparented to init, calibrated by forcing one deliberate panic — a green suite never exercises the teardown its guards govern.
