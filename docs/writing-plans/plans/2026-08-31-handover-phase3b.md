# Daemon handover, phase 3b: say what the shepherd already knows

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this task-by-task. Steps use `- [ ]` for tracking.

**Goal:** Stop reporting a dog as healthy when the shepherd has never once heard from it.

**Spec:** `docs/brainstorming/specs/2026-08-29-daemon-handover-design.md`, sections **G8**, **G12** and **G13**.

**Base:** `origin/main` at v0.1.23.

## Global constraints

- MSRV 1.88, edition 2024.
- `PROTOCOL_VERSION` is 2. Task 4 adds a field to `ProcessInfo`; follow the `dog: Option<DogSource>` precedent in the same struct, which is additive, absent-means-false, and moved neither constant.
- `SCHEMA_VERSION` stays 1. Its rule is that only a rename, a removal or a retype moves it.
- The CLI's package is named `shep`. `-p shep-cli` runs nothing and exits 0.
- IR-11 (`# Errors`), IR-26 (named timing constants), IR-41 (deliberate `Debug`).

---

## Why this exists

Found in production on 2026-08-31, not by a test. A shepherd upgraded to 0.1.23 while `log-rotate`, an adopted dog, still spoke protocol 1. The operator saw:

- `shep daemon reload` — *"the `log-rotate` dog had not answered this shepherd after 3s, so this reload cannot say whether it came back"*, and no suggestion of what to do next
- `shep flock` — `log-rotate`, `(o.o) online`, restarts 0, uptime 16s
- `shep bleats log-rotate` — the same refusal repeating without end

Three surfaces, and the only one telling the truth was the one nobody is directed to.

**Every fact needed to say this correctly was already computed.** `dog_staleness` (`rpc.rs:589`) derives exactly the right set — a registered dog whose process is running and which has never handshook — and puts `log-rotate` in `pending`. The shepherd knew. Three call sites declined to say so.

## The trace

```
dog connects           Hello { protocol: 1, dog_name: ABSENT }
server.rs:475          protocol != PROTOCOL_VERSION, refuse
server.rs:500          match &hello.dog_name -> None
server.rs:520          debug!("refused a client on protocol skew")
                       ...which the daemon's default `warn` level discards
```

`dog_name` was added to `Hello` in phase 3. A protocol-1 client predates it and cannot send one, so `record_refused_dog` never runs and the ladder in `DogRefusals::refused` is never entered.

**G8's one-restart rule is keyed on a name, so it can only fire for dogs new enough to name themselves.** The dogs most likely to need it are the ones structurally unable to reach it. That is the defect, and everything else in this phase is a consequence of it.

## What is NOT wrong

Two claims were made against this incident and then measured false. Recorded so nobody re-derives them.

**`cargo install` did not need `--force`.** Cargo skips only when the installed version equals the registry version; `log-rotate` went 0.1.2 to 0.1.3 and the install ran. The reinstall changed nothing because the *process* was never restarted — G10, a rename leaves the running inode mapped — and `shep restart log-rotate` was the missing step.

**G9 is not falsified.** The spec already prescribes the whole fix at its line 504: *"`cargo install shep-log-rotate` then `shep restart log-rotate`. Verified."* The documentation was right. Nothing shep printed at any point said the second half.

---

## Order, and the dependency between tasks 2 and 3

Task 1 is unrelated to the other three and can ship alone.

**Task 2 must precede task 3.** The reason written here first was wrong, and task 2's drill measured it: this said the ladder would move the motivating case out of `unsettled_dog_report` and into `stale_dog_report`, which already carries advice. It does not. Task 2's `DOG_SILENCE_BUDGET` is 5s, so the first rung lands at +5s and the second at +10s, while the reload's own `DOG_SETTLE_WAIT` is 3s. At the moment the reload reports, neither rung has fired, and a live reload printed the unsettled sentence exactly as before.

Tuning cannot reconcile them either. Landing the second rung inside a 3s wait needs a sub-1.5s budget, which would restart a dog for being briefly slow.

**So the dependency holds and the consequence inverts.** Task 3 must be written for a population that still contains the skewed dog at reload time — not for the transient remnant this plan originally predicted. What task 2 actually gives task 3 is the timing to be honest about: at reload time the shepherd does not yet know the verdict, but it does know a restart is coming and when. A report saying that is worth more than one saying nothing.

Two earlier phases in this series split tasks on a seam no drill could stand on. This one had a sound seam and a wrong rationale, which a drill caught and reading never would have.

---

### Task 1: the table-form skew message says what to do

**Files:** `crates/shep-cli/src/lib.rs`.

The two output formats diverged on framing. JSON emits `... Run \`shep daemon reload\`.`; the table form emits the same remedy as a bare indented line with no imperative anywhere near it:

```
error[version-skew]: this shep is 0.1.23, the running shepherd is 0.1.22

`cargo install shep` replaced the binary. It did not restart the
shepherd, which is still running the old code.

  shep daemon reload
```

An operator read that and was not certain the last line was a command to run. The indentation is deliberate and stays — the code comment at `lib.rs:1724` is explicit that the remedy "has to sit on a line of its own to be seen and copied". What is missing is the sentence pointing at it.

`VERSION_SKEW_CAUSE`'s doc claims the two renderings "cannot drift into saying different things". The text did not drift; the framing around it did. Consider whether the fix belongs in a shared constant so that stays true.

- [ ] **Step 1: Write the failing test.** The table form names the remedy as an instruction, not only as a line. Pin the exact string — an existing test at `lib.rs:2984` already pins this block's wording and moves with it.
- [ ] **Step 2: Run it, watch it fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove it non-vacuous.**
- [ ] **Step 5: Drive it.** Force a real skew and read the output as an operator, not as a diff.
- [ ] **Step 6: `web/` and commit.** Grep the prose pages for this block before assuming they are fine.

### Task 2: G8's ladder reaches a dog that cannot name itself

**Files:** `crates/shep-daemon/src/dogs.rs`, `crates/shep-daemon/src/rpc.rs`.

`dog_staleness` already computes the set. Drive `DogRefusals::refused(name)` from it, so a registered dog that is running and has never handshook enters the same ladder a named refusal does: restarted once from disk, then marked stale, never spun.

**Prefer this to peer credentials.** Attributing the refused connection by peer pid is more precise, but `SO_PEERCRED` has no named-pipe equivalent and Windows would need `GetNamedPipeClientProcessId`, forking a code path that phase 15 deliberately unified into `shep_core::transport`. The set difference needs no client cooperation and no platform arm.

**Name the tradeoff at the call site.** A dog that is merely slow to connect gets restarted once. That is bounded by the existing ladder and cheap for a dog that is not yet doing anything, but it is a real behaviour change and a reader deserves to find the reasoning where the decision is.

Decide what "has had long enough" means here and give it a named constant (IR-26). The reload path already waits a settle budget before reporting; whether this shares that budget or has its own is a design call, and the two answer different questions.

- [ ] **Step 1: Write the failing tests.** A dog alive and never handshook is restarted once and then marked stale; one that handshakes inside the budget is untouched; a dog already stale is not re-laddered.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove each non-vacuous.** The untouched-healthy-dog case above all — it passes for the wrong reason if the inference never fires at all.
- [ ] **Step 5: Drive it.** A real protocol-1 dog against a current shepherd, watching for exactly one restart and then silence.
- [ ] **Step 6: Commit.** No operator-facing string changed yet; that is task 3.

### Task 3: the unsettled report says what to check

**Files:** `crates/shep-cli/src/commands/daemon.rs`.

After task 2, `unsettled_dog_report` describes a UNION of two populations, and neither of them is what this plan first predicted. `dog_staleness` seeds `pending` from `DogRefusals::restarting()`, dogs whose restart the shepherd has already asked for, then adds every running dog that has not handshook and is not yet stale. So one entry may be mid-restart and the next may be merely silent inside the budget, and the skewed dog from the incident is still in there at reload time.

That rules out any wording promising a restart is coming, since for half the population it has already been requested. State the rule instead of this dog's future: what the shepherd does with a dog that stays silent is true of every entry regardless of which half it is in.

It still needs a call to action, because "cannot say whether it came back" leaves an operator with no next move, and the next move that worked in production was reading the dog's own log. `shep bleats <dog>` is where the answer was.

Two sentences already exist in this file and `stale_dog_report` is the better model: it names a remedy. Match its register rather than inventing a third voice. Check whether it should also name the restart step, since the production incident turned on `cargo install` alone being insufficient and the spec's own fix is two commands.

- [ ] **Step 1: Write the failing tests**, pinning both sentences exactly. The spec's verification list is explicit that "the exact strings" are the feature.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove each non-vacuous.**
- [ ] **Step 5: Drive a real reload** with a skewed dog and read both sentences as an operator.
- [ ] **Step 6: `web/` and commit.**

### Task 4: `online` stops meaning "the process is alive"

**Files:** `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/rpc.rs`, `crates/shep-cli/src/output/`.

`shep flock` showed `(o.o) online` for a dog that had never completed a handshake. The status column reports process liveness, which for a sheep is the whole truth and for a dog is not: a dog that cannot talk to the shepherd is not doing its job, however alive it is.

`ProcessInfo` carries no handshake fact, so this is a wire change. Follow `dog: Option<DogSource>` in the same struct — additive, `Option`, absent means "this peer predates the field", neither version constant moves. Route it through `ProcessInfoBuilder` rather than adding a positional field.

The rendering is the design work. A dog that has never handshook is not `online` and is not `errored` either; the process is fine and the relationship is not. Decide what the STATUS column says, whether the sheep face changes, and what `--format json` carries. Note that the dogs table has its own header set including `SOURCE`, so it can differ from the sheep table without disturbing it.

- [ ] **Step 1: Write the failing tests.** A never-handshook dog does not render as `online`; a healthy dog is unchanged; the JSON form carries the fact; an absent field renders as it does today.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove each non-vacuous**, the unchanged-healthy-dog case included.
- [ ] **Step 5: Drive it.** `shep flock` against a real skewed dog, in all three styles, and `--format json`.
- [ ] **Step 6: `web/` and commit.** A STATUS value is documented surface.

### Task 5: `shep lookout` stops disagreeing with `shep flock`

**Files:** `crates/shep-cli/src/lookout/app.rs`, `crates/shep-cli/src/lookout/view/flock.rs`, `crates/shep-cli/src/lookout/view/detail.rs`, `crates/shep-cli/src/lookout/theme.rs`.

Task 4 routed `shep flock` through `vocabulary::Reported`, so a dog that has
never handshook prints `silent` there. It never touched the dashboard.
`shep lookout`'s flock pane and detail pane still read `ProcessInfo::status`
directly and still say `online`. That is worse than before task 4: an
operator now has two shep surfaces open on the same dog, and they disagree.
Nothing tells the operator which one to believe.

`vocabulary.rs`'s own module doc already says the rule: a face or a status
mapping decided anywhere but there is a review defect. Lookout deciding its
own answer for the STATUS cell — which it does today, by reading
`row.info.status` straight into `.to_string()` and into
`Palette::status` — is exactly the defect the module doc warns about, just
not caught at the time because task 4 had no reason to look past `output/`.

`output::rows::reported` is the pattern to follow: it guards `Reported::of`
behind `p.dog.is_none()` so a sheep, which has no handshake at all, can never
be painted silent by a future daemon-side bug that leaves a field unset. Give
`Row` in `lookout/app.rs` the same guarded lookup, and route both panes'
STATUS cells through it and through `Reported`'s `word()`, `face()` is not
used here — `theme.rs`'s own doc says lookout colours the word rather than
growing a face — and `role()` via a new `Palette` method that takes a
`Reported` instead of a bare `ProcStatus`.

**The group/rollup row needs a decision, not a silent pass-through.**
`output::rows::group_paint` already argued this for the table: a dog is
never stocked to several instances, so no group row that function can see
has a handshake to report, and it deliberately skips `Reported::of`.
`lookout::app::App::is_grouped` has the identical shape (`instance.is_some()`
on every member), so the identical argument applies — the group and detail
rollups can keep reading `ProcStatus` unchanged. Write that argument down at
each call site rather than leaving a reader to re-derive it, and confirm it
with a test: a dog can never appear in a group row.

- [ ] **Step 1: Write the failing tests.** A silent dog's row in the flock
      pane reads `silent`, not `online`; a healthy dog is unchanged; a sheep
      is unchanged; the detail pane's status word and colour agree with the
      flock pane for the same dog.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove each non-vacuous**, the two unchanged cases most of
      all — they pass for the wrong reason if the new lookup path is never
      reached.
- [ ] **Step 5: Drive it.** A real protocol-1 dog against a current
      shepherd, `shep lookout` open beside `shep flock`, and confirm both say
      the same word.
- [ ] **Step 6: `web/` and commit.** Grep the docs site for a lookout STATUS
      description before assuming nothing needs it. Regenerate the rendered
      frames in `crates/shep-cli/src/lookout/snapshots/` and
      `docs/lookout/frames.txt` if the STATUS cell's own rendering changed
      for any fixture they cover, and read the diff line by line.

---

## Out of scope

**The `--version` contract and the disk check.** That is phase 4, which predicts staleness from the binary on disk. This phase reports what already happened, and the two are independent: prediction is worth nothing while the report says a dog is fine.

**Bark's restart per reload.** Recorded in `deferred.md`, still needs a ruling on orphaned dogs.

---

## Verification

Every phase of this work found its bugs by driving the real thing, and this one was found by an operator in production rather than by any of them. **A green suite is not evidence here.**

Reproduce a protocol-1 dog with `cargo update -p shep-core --precise <old>`, never by pinning `shep-client` — the protocol lives in shep-core and the client's dependency on it floats within 0.1.x (G9).

`$SHEP_HOME` stays under ~103 bytes or the control socket refuses the path.

## Gate

Per `CLAUDE.md`. One cargo shape per task: `-p shep` for tasks 1, 3 and 5,
`-p shep-daemon` for task 2, `--workspace` for task 4 since it crosses three
crates.

Inner loop is `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`. Task 4's wire change wants the full workspace run before commit.

`web/` for tasks 1, 3 and 4: `cargo build --release`, `./web/scripts/generate-cli-reference.sh`, then `astro build` **and** `astro check`.

CI's process-spawning tiers flake. A `slow`, `musl` or e2e failure is read against `main`'s own history before being treated as this branch's fault.
