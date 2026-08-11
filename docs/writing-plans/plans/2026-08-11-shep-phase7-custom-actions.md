# Phase 7 — custom actions (`shep trigger`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this plan task by task. Steps use `- [ ]` for tracking.

**Goal:** `shep trigger <target> <action> [params]` sends a named action to a
running app over the shepherd channel (fd 3) and reports what the app says
back.

**Architecture:** The daemon already owns both halves of the channel — the
runner's writer task drains `to_child`, and `run_sheep` relays every
`ChildMessage` including `ActionReply`. What is missing is a handle from the
actor to `to_child`, a waiter that can time out, and the verb. Aggregation
follows `begin_manual`'s shape; waiting follows `spawn_readiness_task`'s.

**Tech stack:** tokio actor, `serde` adjacently/internally tagged wire enums,
`insta` snapshots for wire fixtures, `ScriptedRunner` for the paused-clock
tier, real children for e2e.

---

## The prerequisite: fd 3 does not block

**Every child's fd 3 is non-blocking today, and that is a shipped bug.**
`tokio::net::UnixStream::pair()` creates both ends non-blocking, `into_std()`
documents that it *keeps* `O_NONBLOCK`, and `TokioRunner::spawn` never clears
it before mapping the fd to 3. A child doing a plain blocking `read <&3` gets
`EAGAIN` — measured, with `/bin/sh` reporting *"read error: 0: Resource
temporarily unavailable"*.

This is not only Phase 7's prerequisite. **`shutdown_with_message` ships today
and is broken the same way** for any app doing a blocking read. Event-loop
runtimes (Node, Go) set non-blocking themselves, which is why nobody noticed.

It gets its own commit, ahead of the feature, so it can be reverted or
backported alone.

---

## Global constraints

- **Never read or reference `/Users/rin/GitHub/pm2`.** Clean-room project;
  behaviour comes from this repo's specs.
- MSRV 1.88, edition 2024. Workspace lints deny `missing_docs` and
  `missing_debug_implementations`. `#![forbid(unsafe_code)]` outside
  `shep-daemon/src/sys.rs`.
- Style per `docs/idiomatic-rust.md`; cite `IR-<n>` where a rule drove a
  choice.
- **Rule 10:** no task-relative phrasing in shipped comments — name the thing,
  never "Task 4" or "this phase".
- CHANGELOGs reconciled, not appended (IR-45), each in the right crate.
- **The shepherd channel has no version field.** `PROTOCOL_VERSION` governs
  the client↔daemon socket only. Any change to the fd-3 strings is a silent
  break for every deployed app and for the future `@shep/io` shim.
- Gates, each from its own command with `$?` captured directly, never through
  a pipe (zsh: a pipeline's `$?` is the last command's, `${PIPESTATUS[0]}` is
  empty):
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace --all-features`,
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`.
  **Run one cargo command at a time** — cargo serialises on the target-dir
  build lock and two concurrent `--workspace` runs deadlock.
- Baseline at `863790a`: **748 passed, 1 pre-existing ignored, 15 targets
  against 15 result lines.**

---

## Settled decisions

Rin delegated these (2026-08-11) and approved the two she was asked about.
🔴 marks a decision that changes the fd-3 wire.

| # | Decision |
|---|---|
| 1 | **Registration reads off the channel, not the config.** `to_child.is_closed()` answers "can this sheep be triggered" with no second copy of the fact. |
| 2 | **Action names are free-form.** No declaration handshake, no `actions` list on `AppConfig`, the daemon never validates a name. Avoids a new fd-3 string. |
| 3 | **An unknown action is the app's problem.** Documented convention that an app should reply rather than stay silent; not enforced, because enforcing it needs a wire change. |
| 4 | **A channel-less sheep is a per-row refusal, not a whole-selector refusal.** Spec §9's grammar (`all`, `/regex/`, `fold:`) makes mixed flocks the normal case; `reopen`/`flush` are the precedent for per-item failure inside a success. |
| 5 | **New `Response::Triggered(Vec<ActionReply>)`** carrying a struct row. `ProcessInfo` cannot carry a reply body, and `selector_call` is typed to `Vec<ProcessInfo>` so trigger cannot reuse it. `EmptiedFile` is the precedent for a non-`ProcessInfo` row. |
| 6 | **`trigger` answers on completion, not acceptance.** An action has no structural time floor the way a reload's N × ~11s does. This is what makes the daemon-side timeout (Task 5) necessary. |
| 7 | 🔴 **Params ship now**, as `Option<String>` on `ShepherdMessage::Action`. Rin approved. Zero apps are deployed and no shim exists, so the field is free today and potentially breaking later. `skip_serializing_if` keeps the no-params string byte-identical to spec §7. |
| 8 | **`action_timeout` is a new `UpDuration` on `AppConfig`.** Every existing per-app budget lives there. The daemon-side timeout must be shorter than the RPC budget or the caller gets `DeadlineExceeded` before the honest answer arrives. |
| 9 | **No bus events in v1.** Every `ProcessEventKind` variant is a lifecycle state transition, and a trigger changes none — the sheep is `Online` before and after. `flush` and `reopen` are already bus-silent (verified: zero `self.emit` calls in either handler). An audit trail, if wanted, is a `daemon.command` topic covering every verb, not one event bolted onto trigger. `ServerFrame` is `#[non_exhaustive]`, so this stays additive. |
| 10 | **A drainee is skipped, reported per row.** Follows `handle_reload`'s `status != Online` filter. An operator asking `web` should not get an answer from a process that is about to die. |

---

## File structure

| File | Responsibility |
|---|---|
| `crates/shep-daemon/src/tokio_runner.rs` | clear `O_NONBLOCK` before the fd-3 handoff |
| `crates/shep-daemon/src/channel.rs` | `ShepherdMessage::Action` gains `params` |
| `crates/shep-daemon/src/supervisor.rs` | `to_child` on `SheepSlot`; aggregation, waiter, deadline |
| `crates/shep-core/src/config/app.rs` | `action_timeout` |
| `crates/shep-core/src/protocol/request.rs` | `Request::Trigger`, `Response::Triggered`, `ActionReply` row |
| `crates/shep-daemon/src/rpc.rs` | the `Trigger` arm |
| `crates/shep-cli/src/{cli.rs,commands/trigger.rs}` | the verb |
| `crates/shep-cli/src/commands/selector.rs` | **new** — the one `parse_selector` |

---

## Task 1: fd 3 blocks

**Files:** Modify `crates/shep-daemon/src/tokio_runner.rs`.

- [ ] **Step 1: Write the failing test.** A real child that reads fd 3 with a
      blocking read, does **at least two** round trips, and asserts both land.
      **Two is not optional:** a single round trip passes on the broken build
      because the first read wins a spawn-timing race. Say so in the comment.
- [ ] **Step 2: Watch it fail** with `EAGAIN`/"Resource temporarily
      unavailable", and record the output.
- [ ] **Step 3: Clear the flag.** `set_nonblocking(false)` on the std child end
      before `OwnedFd::from`. Safe std, no `unsafe`. State in a comment *why*
      the flag is there to begin with — it is inherited from tokio's pair, not
      set deliberately — so nobody re-adds it.
- [ ] **Step 4: Run the test and the `shutdown_with_message` cases.** This fix
      changes shipped behaviour for `shutdown_with_message`; confirm those
      still pass and say which ones you ran.
- [ ] **Step 5: Commit** — `fix(daemon): let a child block on the shepherd channel`.
      CHANGELOG entry (IR-45): this is a user-visible fix to a shipped feature,
      independent of the trigger verb.

## Task 2: the actor can reach the child

**Files:** Modify `crates/shep-daemon/src/supervisor.rs`, `crates/shep-daemon/src/runner.rs`.

Route (b) from the research: clone `ProcIo::to_child` onto `SheepSlot`. Route
(a) — a new `SheepCtl` variant — is rejected and the reason is sharper than it
looks: `SHEEP_CTL_CAPACITY` is 4, `try_send` never awaits, and `claim_manual`
documents that it ignores `Full` *because a queued `Kill` means the ladder is
already running*. Anything else occupying those four slots can make that
reasoning drop **a `Kill`**.

- [ ] **Step 1: Put `to_child` on the slot.** `mpsc::Sender` is `Clone`;
      `to_child_rx` is drained by the runner's own writer task, not by
      `run_sheep`, so the drain survives the kill ladder and the child's whole
      life. Capacity 32, so `Full` means something real.
- [ ] **Step 2: Clear it where `ctl` is cleared.** `handle_exited` sets
      `slot.ctl = None`; the `to_child` clone must go with it, or the writer
      task's `recv()` never returns `None` and the task outlives the child.
      Phase 6 changed `handle_exited`; read it as it stands rather than as the
      research describes it.
- [ ] **Step 3: Test the obligation** — a sheep that exits must not leave its
      writer task alive. Name the leak the test catches.
- [ ] **Step 4: Commit** — `feat(daemon): keep a handle on the shepherd channel`.

## Task 3: params on the fd-3 wire 🔴

**Files:** Modify `crates/shep-daemon/src/channel.rs`, `docs/specs/shep-v1.md`.

**This is the phase's one irreversible change.** Rin approved it: zero apps are
deployed and no `@shep/io` shim exists, so the field is free today and
potentially breaking later.

- [ ] **Step 1: Add `params: Option<String>`** to `ShepherdMessage::Action`,
      with `#[serde(skip_serializing_if = "Option::is_none", default)]`.
- [ ] **Step 2: Prove the existing fixture is untouched.** With
      `skip_serializing_if`, `{"kind":"action","name":"gc"}` must still
      round-trip byte-identically — that is what keeps this additive. The
      existing `action_wire_fixture_round_trips` must pass **unmodified**; if
      you had to change it, the serde attributes are wrong.
- [ ] **Step 3: Add a second fixture** for the with-params form, pinned the
      same way, round-tripped both directions.
- [ ] **Step 4: Amend spec §9.** It currently writes `trigger <target>
      <action>`. Record that params were added deliberately against the spec as
      written, and why the moment mattered.
- [ ] **Step 5: Commit** — `feat(core): let a triggered action carry params`.

## Task 4: the client wire

**Files:** Modify `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/rpc.rs`.

- [ ] **Step 1: `Request::Trigger { selector, action, params }`** — additive
      under `#[non_exhaustive]`, `PROTOCOL_VERSION` stays **1**.
- [ ] **Step 2: `Response::Triggered(Vec<ActionReply>)`** with a struct row
      carrying the sheep's id, its name, and the outcome. The outcome must be
      able to express: the app replied with a body, the sheep has no channel,
      the sheep is a drainee and was skipped, and the action timed out.
      Decision 4 is why these are per-row rather than a whole-request error.
- [ ] **Step 3: Add the fixture row and verify the regenerated snapshot delta
      is only your addition.** A regenerated `.snap` is the easiest place in a
      diff to hide a change nobody re-derives. Paste the delta in your report.
- [ ] **Step 4: Wire the `rpc.rs` arm.** Note `selector_call` is typed to
      `Vec<ProcessInfo>` and **cannot** be reused here; say what you did
      instead.
- [ ] **Step 5: Commit** — `feat(core): put trigger on the wire`.

## Task 5: the waiting model

**Files:** Modify `crates/shep-daemon/src/supervisor.rs`.

`PendingReply` fits the *aggregation* and not the *waiting*: it has no timeout,
because every command that registers one is backed by the kill ladder
guaranteeing an eventual `Msg::Exited`. **A custom action guarantees nothing**,
so a `PendingReply`-shaped trigger with an unresponsive app leaks an entry
forever and parks the caller.

Two shapes exist in the tree. `Msg::ReloadDeadline` generalises, but
`spawn_readiness_task` + `await_ready` is the better fit: **one** message back
carrying the outcome, rather than two racing arrivals. Reload could not do that
because it did not own both sides; trigger does — the `action-reply` lands in
`run_sheep`, which is shep's own code.

- [ ] **Step 1: Build the waiter** on `spawn_readiness_task`'s shape — waiter
      in the slot taken via `Option::take` so a second reply is dropped, the
      deadline owned by the spawned task, and a staleness stamp on the result.
- [ ] **Step 2: Pick the staleness stamp and justify it.** Phase 6 used the
      replacement's `new_id` rather than a generation counter because ids are
      never reused — one fact, no second copy to drift. Say whether the same
      reasoning holds here.
- [ ] **Step 3: Never await inside the actor loop.** Awaiting an
      acknowledgement from the actor closes a permanent cycle: the actor stops
      draining its mailbox, so a sheep task blocks sending to it, so nothing
      drains that sheep. `handle_reopen`'s doc states this at length — follow
      it.
- [ ] **Step 4: Test at the paused-clock tier** — a reply that arrives, a reply
      that never arrives, a reply that arrives after the deadline, and a second
      reply for an action already answered.
- [ ] **Step 5: Commit** — `feat(daemon): wait for an action's reply, or time out`.

## Task 6: aggregation, refusals, and the drainee

**Files:** Modify `crates/shep-daemon/src/supervisor.rs`.

- [ ] **Step 1: Aggregate like `begin_manual`** — one request, N matched sheep,
      resolve when all have answered or timed out, id-sorted.
- [ ] **Step 2: Per-row refusal for a channel-less sheep**, read off
      `to_child.is_closed()` per decision 1, not off the config.
- [ ] **Step 3: Skip a drainee**, per decision 10. Both halves of a swap match a
      name selector — `an_automatic_restart_never_lands_on_either_half_of_a_swap`
      pins that in its own assertion message. Report the skip as a row.
- [ ] **Step 4: `shep trigger all gc` against a flock where nothing has a
      channel** must not be a silent success. This is the same shape as standing
      backlog item 7 (a reload that does nothing and says nothing); close it
      here or say why it differs.
- [ ] **Step 5: Test** the mixed flock, the all-channel-less flock, and the
      mid-reload case.
- [ ] **Step 6: Commit** — `feat(daemon): answer a trigger for every sheep it matched`.

## Task 7: `action_timeout`

**Files:** Modify `crates/shep-core/src/config/app.rs`, `crates/shep-core/src/config/normalize.rs`.

- [ ] **Step 1: Add `action_timeout: UpDuration`** with a default, alongside
      `kill_timeout`/`listen_timeout`/`graceful_timeout`.
- [ ] **Step 2: Keep the daemon-side timeout shorter than the RPC budget.**
      `DEADLINE_GRACE` exists for exactly this shape. If the daemon's timeout
      exceeds the caller's budget the caller gets `DeadlineExceeded` and never
      sees the honest answer — test that ordering, do not just assert the
      constant.
- [ ] **Step 3: Commit** — `feat(core): bound a custom action with its own timeout`.

## Task 8: the verb, and one `parse_selector`

**Files:** Create `crates/shep-cli/src/commands/selector.rs` and
`crates/shep-cli/src/commands/trigger.rs`; modify `crates/shep-cli/src/cli.rs`
and the three existing copies.

- [ ] **Step 1: Extract `parse_selector` once.** It is currently duplicated
      **four** times and a `trigger` module would make five. Do this first, as
      its own commit, so the verb lands on top of one copy rather than adding
      to the pile.
- [ ] **Step 2: Add the verb** — `shep trigger <target> <action> [params]`,
      selector required, matching the five verbs that already require one (a
      test now pins all six).
- [ ] **Step 3: Say `channel = true` in the help.** Nothing user-facing
      currently mentions it, so an operator whose app has no channel gets a
      refusal with no idea why. Name the config field in the verb's help and in
      the refusal's own message.
- [ ] **Step 4: Render the rows** — the outcome per sheep, in the table and in
      `--format json`. Follow `EmptiedFile`'s precedent in `output/rows.rs`.
- [ ] **Step 5: Commit** the extraction and the verb separately.

## Task 9: the e2e proof

**Files:** Modify `crates/shep-daemon/tests/daemon_e2e.rs`; a fixture child.

The paused-clock tier cannot reach this: `ScriptedRunner` involves no real fd 3.
Note also that `ScriptedRunner` currently **ignores `spec.channel`** — fix or
document that, since it means the fake and the real runner disagree about when
a channel exists.

- [ ] **Step 1: A real child, a real fd 3, at least two round trips.**
- [ ] **Step 2: The refusal path** — an app with no channel, asserting the row
      says so and names `channel`.
- [ ] **Step 3: The timeout path** — an app that reads the action and never
      replies, asserting the caller gets the timeout row rather than
      `DeadlineExceeded`.
- [ ] **Step 4: `to_child.send()` returning `Ok` is not delivery** — measured
      in the research: the first send after the child died returns `Ok(())` and
      vanishes, and only the second errors. **The reply is the only proof a
      trigger landed**, so no test may assert delivery from a successful send.
- [ ] **Step 5: Commit** — `test: prove a trigger reaches a real child and comes back`.

## Task 10: docs, changelogs, and the report

- [ ] **Step 1: `map.md`** — verify every claim against the code and cite by
      **symbol, not line number**. That file has drifted twice by being synced
      to what a plan expected rather than what shipped.
- [ ] **Step 2: Changelogs** (IR-45), reconciled not appended, each in the right
      crate. The fd-3 fix is a separate user-visible entry from the verb.
- [ ] **Step 3: Document the channel contract for app authors** — `channel =
      true`, the fd-3 strings, that an action name is free-form, that the app
      should reply even to a name it does not know, and that the reply is what
      the operator sees.
- [ ] **Step 4: Report to Rin** — every judgement call, anything left unfixed,
      and which standing backlog items this phase closed.
- [ ] **Step 5: Commit** — `docs: record what a custom action is and is not`.

---

## Exit criteria

1. All ten tasks complete and individually reviewed.
2. Every gate green from its own exit code, including both bench-crate gates
   and **the serial run** — which caught a real regression in Phase 6 and is
   the least ceremonial gate here.
3. A child doing a plain blocking read on fd 3 works, proved by a test doing
   **at least two** round trips.
4. `{"kind":"action","name":"gc"}` round-trips byte-identically to spec §7,
   with the original fixture unmodified.
5. The regenerated request snapshot's delta is only the `Trigger` addition,
   shown verbatim in the task report.
6. `shep trigger` against a flock where nothing has a channel is not a silent
   success, and the message names `channel`.
7. A trigger against an app mid-reload skips the drainee and says so.
8. An app that never replies produces a timeout row, not a `DeadlineExceeded`
   from the client.
9. `parse_selector` exists once.
10. Every test added carries a "fails if" comment naming the mutation it
    catches, and the mutation was **actually performed and watched to fail**
    before the comment was written. Phase 6 shipped five tests naming a bug
    they could not catch; a reviewer picking three at random must be able to
    break the implementation in the named way and watch the named test redden.
11. Neither suite run leaves a process reparented to init.
