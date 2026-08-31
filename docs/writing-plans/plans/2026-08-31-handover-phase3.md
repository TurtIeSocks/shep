# Daemon handover, phase 3: dogs

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this task-by-task. Steps use `- [ ]` for tracking.

**Goal:** Carry every dog across the handover with no restart, which means giving a dog a connection that survives its daemon being replaced.

**Spec:** `docs/brainstorming/specs/2026-08-29-daemon-handover-design.md`, sections G6 through G13.

**Base:** cut from `origin/main` at v0.1.21, which contains 2a, 2b and 2c.

---

## The decision this phase is built on

Taken 2026-08-31: **carry every dog, and put the reconnect in `shep-client`.** The alternative, carrying only the two built-in dogs and continuing to refuse adopted ones, was rejected because dogs should be treated alike.

What that buys and costs is recorded in 2b's task 7 section. The cost worth restating: an adopted dog compiled against an older `shep-client` has no reconnect, so it is mute after a reload until rebuilt. Both dogs in `web/public/dogs.json` are the maintainer's own (`shep-log-rotate`, `shep-deploy`), so "until rebuilt" means until she cuts two releases she controls.

## What is actually broken

Verified by driving six real reloads in 2b, not by reading:

```
metrics dog after 6 handovers:  pid unmoved, restarts 0, status online, stderr 0 bytes
curl /metrics:                  HTTP 503, every scrape, for 6m 22s
```

A dog's PROCESS crosses the exec for free, because it is a child of a daemon whose pid does not change. Its CONNECTION does not: only the listener is in the blob, and accepted sockets die with the image. So a carried dog is a live process holding a dead socket.

Neither built-in dog handles this, and their different outcomes are an accident rather than two designs:

| dog | what it does when the connection dies | outcome |
|---|---|---|
| `bark` | its `EventStream` ends, `run_loop`'s `None` arm breaks, it exits 0, autorestart replaces it | survives by accident, at one restart per reload |
| `metrics` | holds an `Arc<Client>` across `accept_forever`, never re-examines it | mute, 503 forever, silent on both sides |

Nor could a dog have needed a reconnect before now. Every path that removed a shepherd also removed its dogs. A dog outliving the daemon it is connected to is a situation the handover invented.

## Where the code lives, and where it runs

This distinction caused real confusion and is worth stating once:

```
shep-client            the code, written once, in shep
   |
   | linked by
   v
each dog's process     where the behaviour executes
```

Shep implements the reconnect. A dog gets it by being rebuilt. No dog author writes reconnect logic, and the dog contract does not change. Confirmed: the built-ins already `use shep_client::{Client, ConnectError, EventStream, RequestError}`, and G9 establishes that a plain `cargo install <dog>` picks up a new `shep-client`.

---

## The problem the spec does not solve

**G8 requires the daemon to restart a refused dog once. The daemon cannot tell which dog was refused.**

`server.rs` refuses a mismatched handshake by reading `hello.protocol` and replying `ProtocolMismatch`. `Hello` carries `client_version` and `protocol` and no dog identity. A refused handshake never gets as far as a request, so `Request::DogConfig`'s name, the one place a dog does identify itself, is unreachable on exactly the path that needs it.

So G8's step 2 is not implementable as written. Three ways out:

1. **Carry the dog's name in `Hello`.** It already knows it: `dogs.rs` gives every dog `$SHEP_DOG_NAME` in its environment. An `Option<String>` on `Hello` is additive, and a client that omits it reads as `None`, which is what every non-dog client truthfully is.
2. **Infer from peer credentials.** `SO_PEERCRED` on Linux, `LOCAL_PEERCRED` on macOS, neither on Windows. Platform-specific work in a layer that `shep_core::transport` deliberately keeps platform-free.
3. **Infer from absence.** Track which dogs hold a connection; after a handover, treat a dog that has not re-established inside a deadline as refused. Conflates refused with slow and with crashed, and needs a deadline nobody has a principled value for.

**Recommendation: option 1.** It is the only one that makes the refusal itself informative rather than inferred, it costs one optional field, and it is the same additive-`Option` shape the handover blob has used five times without moving a version. Option 2 puts a platform gate above the transport, which CLAUDE.md calls a design decision rather than a shrug. Option 3 answers a different question than the one asked.

**Open for the maintainer: does `Hello` gaining an optional field move `PROTOCOL_VERSION`?** My reading is no, on the blob's own precedent: an absent optional field is not a wire break, and a daemon reading `None` from an older client is correct rather than degraded. But `Hello` IS the version-negotiation frame, so this is the one place that argument deserves a second look before it is relied on.

---

## Order

1 is the spine and everything depends on it. 2 cannot be built before 1, because it is the answer to a question only a reconnecting client asks. 3 and 4 are independent of each other once 2 is in.

---

### Task 1: a reconnecting client

**Files:** `crates/shep-client/src/`, `crates/shep-cli/src/dog/mod.rs`.

A dog's connection has to re-establish itself when the daemon it was talking to is replaced.

**Not on `Client` itself.** The CLI is the other consumer and must NOT gain transparent reconnect: a `shep stop` whose connection dropped mid-request and silently retried could stop a sheep twice. H2 already rules that in-flight requests fail rather than retry, and that ruling is what keeps the CLI's one-shot semantics intact. So this is a wrapper, or a mode, that dogs opt into and the CLI does not.

- [ ] **Step 1: Write the failing test.** A client whose connection is closed under it re-establishes and serves the next request.
- [ ] **Step 2: Run it, watch it fail.**
- [ ] **Step 3: Implement.** In-flight requests fail, never retry. Say in the commit why the CLI is not affected.
- [ ] **Step 4: Prove it non-vacuous.**
- [ ] **Step 5: Point `DogRuntime::start` at it** (`crates/shep-cli/src/dog/mod.rs:186` connects once today).
- [ ] **Step 6: Commit.**

### Task 2: what a refused reconnect does (G8)

**Files:** `crates/shep-core/src/protocol/` (`Hello`), `crates/shep-daemon/src/server.rs`, `crates/shep-daemon/src/dogs.rs`.

Per the section above, and per G8: record the refusal, restart that dog ONCE from disk, and if the restart is refused too, stop and report it stale. A second refusal proves the disk binary cannot satisfy this daemon, so retrying is a spin rather than optimism.

- [ ] **Step 1: Write the failing tests**, including that a twice-refused dog is NOT restarted a third time.
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement.** `server.rs` currently drops everything but `hello.protocol` and logs the refusal at no level; that is half the defect.
- [ ] **Step 4: Prove each non-vacuous**, especially the never-loop case.
- [ ] **Step 5: Real reload** with a deliberately stale dog, proving one restart and no loop.
- [ ] **Step 6: Commit.**

### Task 3: a reconnected dog's version is fresh (G13)

**Files:** `crates/shep-client/src/`, `crates/shep-daemon/src/`.

`Client::ack` is taken once at connect and handed out by `daemon()`. After a reconnect it would still describe the predecessor. `metrics` prints `daemon_version` from it, so it would publish a version that is no longer running.

G13's rule: `daemon reload` reports dog staleness AFTER the dogs have reconnected, when the answer is a fact rather than a claim about a process being replaced.

- [ ] **Step 1: Write the failing test.** After a reconnect, `daemon()` reports the successor.
- [ ] **Step 2: Run it, watch it fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Prove it non-vacuous.**
- [ ] **Step 5: Real reload**, scraping `metrics` afterwards and reading `daemon_version` back.
- [ ] **Step 6: Commit.**

### Task 4: strike the refusal

**Files:** `crates/shep-daemon/src/handover/mod.rs`, `crates/shep-daemon/src/supervisor.rs`, `web/src/pages/docs/getting-started.astro`.

Last one, and it is four lines plus the fixtures. 2b built and measured this carry already, so the mechanism is known: strike `entry.dog.is_some()` in `refusal`, give `CarriedSheep` a `dog: Option<DogSource>`, restore it in `install_adopted`.

Two things 2b recorded for whoever does this:

- **Losing the `dog` field is not cosmetic.** `matching_ids` includes a dog only for an exact selector, so a carried dog without its marker leaves `shep dogs` and appears in `shep flock` beside the operator's own apps.
- **Four tests use `RefusedReason::Dog` as their refusal fixture.** The two in `handover/mod.rs` are one line each (`e.reload = ReloadState::Replacement`). The two in `boot.rs` boot a real daemon, and a reload parked in `AwaitReady` is the one to build: a pending stop needs a script that ignores signals, which then blocks the test's own 5s teardown on `kill_timeout`.

- [ ] **Step 1: Write the failing tests.**
- [ ] **Step 2: Run them, watch them fail.**
- [ ] **Step 3: Implement, and move the four fixtures.**
- [ ] **Step 4: Prove each non-vacuous.**
- [ ] **Step 5: Real reload** with both built-in dogs, proving `curl /metrics` still answers 200 afterwards and neither dog's restart count moved. That measurement is the whole phase.
- [ ] **Step 6: `web/` and commit.** `getting-started.astro` says "A dog or anything mid-reload sends the reload down the older path instead"; the first half stops being true here.

---

## Out of scope, deliberately

**G11 and G12's row 5.** A dog answering `--version` so `adopt` can check the binary ON DISK, and the trap where upgrading only a dog leaves a system that breaks at its next restart, days later, for an unrelated reason. Real, and not needed to carry a dog. Worth its own phase once this one has shipped, and G12's matrix is already written.

## The reload drill

Unchanged, in `docs/writing-plans/plans/2026-08-30-handover-phase2b.md` under "The reload drill, exactly". Every constraint still binds: the `awk` rate, `$SHEP_HOME` under 103 bytes, stopping the sheep before counting, seam-aware counting.

**A green suite is not evidence in this phase.** Six bugs in the timer-strand class were each found by driving a real reload and none by reading code, and this phase's own defect was found the same way. Every task drives a real reload, and for a dog that means proving it still WORKS afterwards, not that its process is still there. A live dog holding a dead socket looks perfect to a pid check.

## Gate

Per `CLAUDE.md`. Inner loop `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`, one cargo shape per task, Windows cross-check with its own `CARGO_TARGET_DIR`. Note that this phase touches `shep-client` and `shep-cli` as well, so a shep-scoped run needs `cargo test -p shep --lib --bins --all-features`.

Counts at the branch point: about **675** daemon lib, **2149** workspace. A shape, not a checksum.

**CI's process-spawning tiers flaked five times during 2b and 2c.** A `slow`, `musl` or e2e failure gets read against `main`'s own history before it is treated as the branch's fault.
