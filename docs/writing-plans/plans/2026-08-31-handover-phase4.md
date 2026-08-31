# Daemon handover, phase 4: the dog version axis

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this task-by-task. Steps use `- [ ]` for tracking.

**Goal:** Let an operator find out that a dog's binary on disk cannot satisfy the running shepherd, before that dog is adopted or restarted into a state nobody can see.

**Spec:** `docs/brainstorming/specs/2026-08-29-daemon-handover-design.md`, sections **G9**, **G10**, **G11**, **G12** and **G13**.

**Base:** cut from `origin/main` after phase 3.

## Global constraints

- MSRV 1.88, edition 2024.
- `PROTOCOL_VERSION` is 2 and does not move in this phase. Nothing here changes a wire frame.
- The `shep-idiomatic-rust` skill fronts IR-1..IR-46. IR-11 (`# Errors`), IR-26 (named timing constants) and IR-41 (deliberate `Debug`) all apply.
- The CLI's package is named `shep`. `-p shep-cli` matches nothing and exits 0 having run no tests.
- `web/` is published. Every task in this phase changes what an operator types or sees, so every task regenerates the CLI reference and reads the prose pages.

---

## Three corrections the spec needs, found before any code

Phase 3 was planned off G8-G13 and shipped clean. Re-reading the same sections against the tree found three places where the spec describes a system that is not this one. Each changes a design decision in this phase, so each is stated here and corrected in the commit that acts on it.

### 1. G9 names the wrong field, and the right one was always there

G9 closes: *"So `Hello.client_version`, which the daemon already receives and discards, is not a convenience. It is the only thing that knows."*

That is wrong twice.

`Hello` carries `client_version: String` and `protocol: u32` as separate fields. `server.rs:475` refuses on `hello.protocol != PROTOCOL_VERSION`; `client_version` is only ever logged. So the crate version is not what decides compatibility, and it is not the only thing that knows — the field that knows sits beside it.

Phase 3's task 2 measured the first half directly: two builds differing only in `PROTOCOL_VERSION` both reported `0.1.22`.

**But do not over-correct into "protocol replaces crate version".** The spec's own verification list asks for *"a version difference with NO protocol difference, which is the case a protocol-only check misses"*. Two builds can share a protocol and still be different code. So:

| number | answers | on a mismatch |
|---|---|---|
| protocol | can this dog connect at all | hard: refuse |
| crate version | is this dog the same build as everything else | soft: report |

**A `--version` answer carries both.** Neither is sufficient, which is precisely why `Hello` has carried both since before this phase.

### 2. `shep-log-rotate` is real, and is not in this workspace

G11 argues from `shep-log-rotate` accepting `--print-config` and `--help` and refusing `--version`. `grep` finds the name nowhere in this tree outside the spec, and the built-in dogs are `metrics` and `bark` (`BUILT_IN_DOGS`, `crates/shep-cli/src/dog/mod.rs:46`).

It is nonetheless a real published crate, at 0.1.3 as of 2026-08-31, and it is the adopted dog running against a production flock. So it is not a hypothetical to reason around — it is the reference third-party dog this phase should be driven against, and the one that exposed phase 3b's reporting defects. Expect to install it rather than to find it.

### 3. The two dog populations have opposite properties, and the spec treats them as one

This is the finding that reshapes the phase. A built-in dog and an adopted dog get their program by different mechanisms (`dogs.rs:146-156`):

| | built-in (`metrics`, `bark`) | adopted (third-party) |
|---|---|---|
| program | `current_exe()` + `["dog", name]` | the recorded path, **no argv at all** |
| its binary on disk | *is* the shep binary | somebody else's crate |
| can it skew from the shepherd? | only by being an older *running* image | yes, on both axes |
| G12 rows reachable | 1 and 3 only | 1, 3, 4, 5 |

**A built-in dog can never be G12 row 4 or 5.** Its disk binary is the shepherd's disk binary, so "reinstall the dog" is not a distinct action and a restart always reaches the same code the shepherd exec'd. Row 3 it can reach — a dog carried across a handover is an older image than the shepherd that adopted it, which is exactly what phase 3's one-restart rule already fixes.

So **the `--version` contract is about adopted dogs.** Making `metrics` and `bark` answer a question whose answer is always "the same as the shepherd, by construction" adds a contract point carrying no information. `shep --version` already answers for both, because they are that binary.

Note the tension this creates with a comment already in `dog_app`: *"an argv shep invented for it is one more thing it has to agree with before it can start"*, which is why an adopted dog gets no argv at run time. A `--version` vet is the same repo answering that question the other way. That is defensible — vetting spawns a throwaway process, running does not — but say so at the call site, because the next reader will find both.

---

## Measured on 2026-08-31, and it strengthens the case

Both published dogs were found shipping lockfiles that pinned a protocol-1 `shep-core`: `shep-log-rotate` at 0.1.0 and `shep-deploy` at 0.1.10, against a current 0.1.24. Both repositories' CI had been red since 2026-08-30, failing on the exact sentence an operator hit in production, and nobody had connected a red dog repository to a stale dog in a flock.

The install semantics are what this phase turns on, and they are not intuitive:

| how the dog is built | which `shep-core` | so which protocol |
|---|---|---|
| `cargo install <dog>` | re-resolved, newest compatible | current |
| `cargo install --locked`, and CI | whatever the shipped lockfile pins | whatever was pinned that day |

Measured rather than reasoned: installing `shep-log-rotate` 0.1.3 to a throwaway root compiled `shep-core v0.1.24`, so the packaged lockfile was ignored. The same crate built `--locked` produced a protocol-1 dog, which is what CI had been doing for two days.

**So a dog's crate version tells you nothing about its protocol, and neither does knowing that somebody installed it.** You would also need to know which flags they used and what the lockfile said at publish time. None of that is visible from outside the binary, which is the strongest argument available for G11: the binary is the only thing that can answer, so ask it.

One more thing to carry into task 3. `RpcError` gained a public `daemon_version` field somewhere inside the 0.1.x range. Its fields are public and it has no constructor, so every literal built outside `shep-client` stopped compiling, and `shep-deploy` had eleven of them. Protocol equality is therefore necessary but not sufficient for "this dog still works", and a `--version` answer carrying only the protocol answers a narrower question than the operator is asking. Decide deliberately whether to close that gap here or defer it, and say which.

## The bug this found

`dog_app` calls `std::env::current_exe()` unguarded (`dogs.rs:147`) and hands the result straight to `AppConfig::minimal`.

`handover/mod.rs:1148` documents at length why the handover must **not** do that, and `check_target` refuses a path containing `" (deleted)"`. `check_target` is private to that module and used nowhere else.

So, on Linux only:

1. Shepherd runs from inode A at some path on `PATH`.
2. `cargo install shep` renames a new file over it (G10). Inode A is unlinked and still open.
3. A built-in dog restarts before any handover — a crash, autorestart, or `shep restart metrics`.
4. `current_exe()` reads `/proc/self/exe`, which is a symlink to the inode, and returns `"<path> (deleted)"`.
5. That string becomes the dog's `script`. It cannot be spawned.

macOS returns a clean path here, so no local run sees it — the handover module's own doc says the naive version "passes every local test", and CLAUDE.md says the same about the gate generally. CI's Linux legs are the only thing that can catch it.

This is the same disk-versus-running axis as the rest of the phase, on the population the rest of the phase does not otherwise touch, so it belongs here. It is also independently shippable — if the phase runs long, task 1 is its own PR.

---

## Order

Task 1 is independent and can land first or alone. Task 2 is the contract; 3 and 4 both depend on it and not on each other.

---

### Task 1: a built-in dog does not respawn from a deleted inode

**Files:** `crates/shep-daemon/src/dogs.rs`, `crates/shep-daemon/src/handover/mod.rs`.

The fix is to stop having two answers to "which file holds my binary" in one crate. `exec_target()` already resolves it correctly, preferring the recorded launch path and validating both arms. Decide whether `dog_app` should use it, use `check_target` alone, or grow its own guard, and argue the choice — `exec_target`'s fallback semantics were written for an exec that must reach the *new* binary, and a dog respawn may not want the same preference order.

Whatever you pick, `DogError::NoBinary` gains a second way to happen, so its docs and `# Errors` section move with it (IR-11).

- [ ] **Step 1: Write the failing test.** A `current_exe` answer carrying `" (deleted)"` does not become a dog's `script`. Pin the exact error text, per the spec's *"a message naming the fix is the feature"*.
- [ ] **Step 2: Run it, watch it fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove it non-vacuous.** Mutate the guard away and watch the test go red.
- [ ] **Step 5: Drive it on Linux.** macOS cannot see this bug. Reproduce the rename-over-a-running-binary sequence from G10 and restart a built-in dog. If a Linux box is not to hand, say so and let CI's `ubuntu-latest` leg be the proof, rather than reporting a macOS run as evidence.
- [ ] **Step 6: Commit.** No `web/` change — nothing an operator types moved.

### Task 2: the `--version` contract, and G9 corrected

**Files:** `docs/brainstorming/specs/2026-08-29-daemon-handover-design.md`, `docs/dogs.md`, `web/src/pages/docs/dogs.astro`.

Write the contract down, correct G9 in the same commit, and decide the output format with an argument attached.

The format is the whole of this task's design work. It carries two numbers (correction 1), third parties implement it from the published page, and shep parses it. Constraints worth weighing: a human runs `--version` far more often than shep does; a format that is easy to emit gets emitted correctly by strangers; and something has to be reserved for a future third number without breaking the parser.

State explicitly that built-in dogs satisfy this through `shep --version` and why (correction 3), so nobody later reads the contract as a gap and "fixes" it.

Measure what `shep --version` and `shep dog metrics --version` do today before writing about them.

- [ ] **Step 1: Measure the current behaviour** of both invocations against a real build.
- [ ] **Step 2: Write the contract** in `docs/dogs.md` and `web/`, with the format and its rationale.
- [ ] **Step 3: Correct G9** — the field, not the conclusion. G9's actual finding, that nothing an operator reads tells them which protocol a dog speaks, is sound and stays.
- [ ] **Step 4: Regenerate the CLI reference**, build and `astro check`.
- [ ] **Step 5: Commit.**

### Task 3: `adopt` vets the version

**Files:** `crates/shep-cli/src/commands/dogs.rs`.

`vet_binary` (reached at `dogs.rs:714`) already spawns the candidate to prove the kernel can exec it, so asking its version costs one more argument on a process that was going to start anyway.

G11: *"A dog that cannot satisfy the running daemon is refused at adopt time rather than becoming a silent online-and-idle entry, which is what happens today."*

**Backward compatibility is in the spec and is not optional.** *"Dogs predating the convention stay adoptable. One that does not answer is recorded as unknown rather than refused, and prediction degrades to G8's post-connection detection for that dog alone."* A dog that does not answer is adopted, recorded unknown. Refusing it would break every dog that exists.

A protocol mismatch refuses; a crate-version difference with a matching protocol reports (correction 1). Do not collapse those into one branch.

Keep the ordering `adopt` already documents and tests: a name collision is refused BEFORE the candidate is spawned, because a refusal running after the spawn has already run the thing it refuses.

- [ ] **Step 1: Write the failing tests.** Refused on protocol mismatch; adopted-with-a-report on crate-version-only difference; adopted-as-unknown when it does not answer; the collision still refused before any spawn.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove each non-vacuous**, the unknown-not-refused case above all — it is the one that passes for the wrong reason if the vet silently no-ops.
- [ ] **Step 5: Drive it.** Build a dog against a pinned old shep-core and adopt it; adopt one that does not answer at all.
- [ ] **Step 6: `web/` and commit.**

### Task 4: `shep restart <dog>` warns before creating row 5

**Files:** `crates/shep-cli/src/commands/`, wherever a dog restart is issued.

Row 5 is a working system that breaks at the next restart, days later, for an unrelated reason. G12: *"`shep restart <dog>` warns before creating that state."* The spec's verification list is specific about the ordering: *"the warning fires BEFORE the restart that breaks it."*

Warn, do not refuse. The operator asked for the restart, the disk binary may be what they just installed, and refusing an explicit command on a prediction is worse than letting them watch it happen with warning in hand. Row 5's fix is *"upgrade the daemon, or reinstall the dog back"* — two options, so the message names both and picks neither.

A built-in dog cannot reach row 5 (correction 3), so this applies to adopted dogs. Whether the check is silently skipped for built-ins or is structurally unreachable is a design choice — prefer the one a reader can see.

- [ ] **Step 1: Write the failing tests.** The warning fires before the restart; a healthy restart stays silent; the message names both fixes.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove each non-vacuous**, including the silent-when-healthy case.
- [ ] **Step 5: Drive row 5 end to end.** This is the phase's proof: a healthy flock, a dog upgraded on disk only, nothing yet wrong, and the warning arriving before the restart that would have broken it.
- [ ] **Step 6: `web/` and commit.**

---

## Out of scope

**Anything that carries more across the exec.** Phase 3 closed that. The gate's one remaining refusal, `PumpUnresponsive`, is permanent by design.

**Bark's restart per reload.** Recorded in `deferred.md` after phase 3. Closing it needs a ruling on what an orphaned dog does, which is a question about every dog rather than about bark.

**Sheep.** The spec's Part 4 exists to stop this guard being extended to them: a sheep is an arbitrary executable with no handshake and no version relationship to the daemon. The shepherd channel's `SHEP_CHANNEL_VERSION` is a third axis, set at spawn time, and a running sheep cannot be asked what it speaks.

**The IR-35 byte-fixture debt.** Eight compatibility tests, recorded in `deferred.md`, unchanged by this phase.

---

## Verification

Every phase of this work has found bugs by driving the real thing and none by reading code; phase 3 found four that way. **A green suite is not evidence here.**

The reproduction, from G9 and worth getting right the first time: force an old protocol with `cargo update -p shep-core --precise <old>`, **not** by pinning `shep-client`. `PROTOCOL_VERSION` lives in shep-core and the client's dependency on it floats within 0.1.x. The spec records this as an experiment that was gotten wrong first.

`$SHEP_HOME` stays under ~103 bytes or the control socket refuses the path.

Task 1's drill is Linux-only and a macOS run is not evidence for it.

## Gate

Per `CLAUDE.md`. One cargo shape per task — `-p shep-daemon` for task 1, `-p shep` for tasks 3 and 4 — and never two shapes in one brief without a separate `CARGO_TARGET_DIR`.

`web/` is in the gate for tasks 2, 3 and 4: `cargo build --release`, `./web/scripts/generate-cli-reference.sh`, then `astro build` **and** `astro check`. `check` is the one that catches a wrong prop; a build alone has shipped a broken page before.

Counts at the branch point are a shape, not a checksum — take them from a real run rather than from this line.

CI's process-spawning tiers flaked seven times across phases 2c and 3. A `slow`, `musl` or e2e failure gets read against `main`'s own history before it is treated as this branch's fault.
