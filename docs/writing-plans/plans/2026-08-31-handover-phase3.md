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

Shep implements the reconnect. A dog gets it by being rebuilt, and no dog
author writes reconnect logic.

**The last clause of that was "and the dog contract does not change", and
task 1 made it false.** The shape chosen is a distinct
`ReconnectingClient` type rather than a mode on `Client`, because that is
what makes the CLI unaffected BY CONSTRUCTION rather than by every call
site being read carefully. The price is that a dog rebuilt against the new
`shep-client` still constructs a `Client` and still goes mute: its author
has to swap one type name. One line per dog, and both dogs in the registry
are the maintainer's, so it rides along with the two releases the carry
already needed. But it is a line, not a rebuild, and the plan claimed
otherwise. Confirmed: the built-ins already `use shep_client::{Client, ConnectError, EventStream, RequestError}`, and G9 establishes that a plain `cargo install <dog>` picks up a new `shep-client`.

---

## The problem the spec does not solve

**G8 requires the daemon to restart a refused dog once. The daemon cannot tell which dog was refused.**

`server.rs` refuses a mismatched handshake by reading `hello.protocol` and replying `ProtocolMismatch`. `Hello` carries `client_version` and `protocol` and no dog identity. A refused handshake never gets as far as a request, so `Request::DogConfig`'s name, the one place a dog does identify itself, is unreachable on exactly the path that needs it.

So G8's step 2 is not implementable as written. Three ways out:

1. **Carry the dog's name in `Hello`.** It already knows it: `dogs.rs` gives every dog `$SHEP_DOG_NAME` in its environment. An `Option<String>` on `Hello` is additive, and a client that omits it reads as `None`, which is what every non-dog client truthfully is.
2. **Infer from peer credentials.** `SO_PEERCRED` on Linux, `LOCAL_PEERCRED` on macOS, neither on Windows. Platform-specific work in a layer that `shep_core::transport` deliberately keeps platform-free.
3. **Infer from absence.** Track which dogs hold a connection; after a handover, treat a dog that has not re-established inside a deadline as refused. Conflates refused with slow and with crashed, and needs a deadline nobody has a principled value for.

**DECIDED 2026-08-31 by the maintainer: option 1, carry the dog's name in
`Hello`.** The reasoning below is what the decision was taken on and is kept
for whoever implements task 2.

**Recommendation: option 1.** It is the only one that makes the refusal itself informative rather than inferred, it costs one optional field, and it is the same additive-`Option` shape the handover blob has used five times without moving a version. Option 2 puts a platform gate above the transport, which CLAUDE.md calls a design decision rather than a shrug. Option 3 answers a different question than the one asked.

**ANSWERED 2026-08-31: no bump.** `Hello` carries no
`#[serde(deny_unknown_fields)]`, so serde ignores fields it does not know
and an older daemon parses a newer client's `Hello` cleanly. That was the
whole worry: `Hello` IS the version-negotiation frame, so a daemon that
rejected unknown fields would refuse a newer client BEFORE reading
`protocol`, and that would be a hard break. It does not.

The change is additive in both directions and meets none of this project's
own bar for a bump, which is a change an older peer cannot deserialize
(`PROTOCOL_VERSION` moved 1 to 2 because `SelectorSpec` gained a variant an
older daemon could not parse). Bumping would refuse every CLI invocation
and every dog until upgraded, which is worse here than anywhere: it forces
a mass dog refusal at exactly the moment G8's graceful handling does not
exist yet.

**Task 2 must pin this**, with a case proving an older-shaped `Hello`
still parses, so nobody adds `deny_unknown_fields` later without seeing
what it breaks.

The reasoning that led there, kept because the question is worth being able
to re-ask:

**Was open for the maintainer: does `Hello` gaining an optional field move `PROTOCOL_VERSION`?** My reading is no, on the blob's own precedent: an absent optional field is not a wire break, and a daemon reading `None` from an older client is correct rather than degraded. But `Hello` IS the version-negotiation frame, so this is the one place that argument deserves a second look before it is relied on.

---

## Order

1 is the spine and everything depends on it. 2 cannot be built before 1, because it is the answer to a question only a reconnecting client asks. 3 and 4 are independent of each other once 2 is in.

---

### Task 1: a reconnecting client

**Files:** `crates/shep-client/src/`, `crates/shep-cli/src/dog/mod.rs`.

A dog's connection has to re-establish itself when the daemon it was talking to is replaced.

**Not on `Client` itself.** The CLI is the other consumer and must NOT gain transparent reconnect: a `shep stop` whose connection dropped mid-request and silently retried could stop a sheep twice. H2 already rules that in-flight requests fail rather than retry, and that ruling is what keeps the CLI's one-shot semantics intact. So this is a wrapper, or a mode, that dogs opt into and the CLI does not.

- [x] **Step 1: Write the failing test.** A client whose connection is closed under it re-establishes and serves the next request.
- [x] **Step 2: Run it, watch it fail.**
- [x] **Step 3: Implement.** In-flight requests fail, never retry. Say in the commit why the CLI is not affected.
- [x] **Step 4: Prove it non-vacuous.**
- [x] **Step 5: Point `DogRuntime::start` at it** (`crates/shep-cli/src/dog/mod.rs:186` connects once today).
- [x] **Step 6: Commit.**

#### Outcome

**A distinct type, `shep_client::ReconnectingClient`, not a mode on `Client`.** Three shapes were available: a wrapper dogs name and the CLI does not; a flag on `Client::connect`; or inferring dog-ness from `$SHEP_DOG_NAME`, which `dogs.rs` already puts in every dog's environment. The CLI decides it. A flag makes "does this client retry?" a property of a CALL SITE rather than of a type, so every verb has to be read carefully to trust. Env-sniffing makes it a property of ambient state, so `SHEP_DOG_NAME=x shep stop web` would quietly acquire it and the question stops being provable at all. A distinct type makes the CLI unaffected BY CONSTRUCTION: no `shep` verb names it, so none can acquire the behaviour by accident or by a later edit to a shared constructor. `Client` gains one method — `closed()`, a query that resolves when the actor's command channel drops — and loses nothing.

**One claim in this plan is now false, and it is the wrapper's price.** Line 51 says a dog gets the reconnect by being rebuilt and the dog contract does not change. That is true of the two shapes that were rejected and not of the one that shipped: an adopted dog rebuilt against a newer `shep-client` still constructs a `Client` and still goes mute. Its author has to swap one type name. Both dogs in `web/public/dogs.json` are the maintainer's own, so the cost is one line in each on top of the two releases the decision section already accepted — but it is a line, not a rebuild.

**Supervised, not lazy, and that was forced by tasks 2 and 3 rather than chosen for elegance.** A background task waits on the current connection's death and dials again immediately. Reconnecting on next use would have been cheaper and is wrong twice over: a metrics dog scraped once a minute would spend that minute unreconnected, so G8's refusal would not surface until something happened to ask; and G13 wants `daemon reload` reporting staleness AFTER the dogs have reconnected, which is only immediate if the reconnect is driven by the disconnection. Measured below: the first scrape after `daemon reload` returned was already 200, six times out of six.

**A refused reconnect stops, and that is the seam task 2 hooks into.** `ConnectError::ProtocolMismatch` ends the supervisor and sets `LinkState::Refused`, carrying the daemon's own version and message. Everything else is treated as a successor that is not ready yet and retried with backoff (50ms doubling to a 5s ceiling), because across a handover the listening socket never stops being bound — `connect(2)` succeeds into the backlog and only the handshake waits.

**G13's client half falls out for free; its daemon half does not.** `ReconnectingClient::daemon()` reads the ack off the generation answering right now, returned owned rather than borrowed. That is the only correct answer for a type whose generation changes underneath it — a cached ack would describe the predecessor, which is exactly what the metrics dog must not publish. Task 3 still owns the reporting side.

**Bark is unchanged and this is the gap task 4 will meet.** Its `EventStream` belongs to one generation and ends when that connection dies, so `run_loop`'s `None` arm breaks, the dog exits 0, and autorestart replaces it — survival by accident, at one restart per reload, exactly as 2b recorded. Re-arming the subscription inside `ReconnectingClient` would silently swallow the gap between the connection dying and the successor accepting a new `Subscribe`, and an event stream that hides a gap is worse than one that ends, so `subscribe` documents the boundary instead. **Task 4's step 5 asks that neither dog's restart count move. That is a decision about the gap, not a line of plumbing, and it is not in this task's commit.**

##### Drill, measured

Route 2 of the two the brief offered: a temporary local bypass of `RefusedReason::Dog` (`if false && entry.dog.is_some()`), reverted before the gate and proven reverted — `git diff --stat crates/shep-daemon/` is empty and the branch's only daemon change is none at all. Release build, isolated `$SHEP_HOME` at `/tmp/p3/home`, one `awk` sheep plus the metrics dog, `curl 127.0.0.1:9615/metrics`.

The before-state was re-measured on this machine rather than quoted from 2b, by neutering the supervisor's wake-up (`core::future::pending()` in place of `closed().await`) and rebuilding:

| | before (no reconnect) | after (this task) |
|---|---|---|
| pre-reload scrape | HTTP 200 | HTTP 200 |
| scrapes after 1 reload | **503, 503, 503, 503, 503, 503** | 200 |
| six reloads, three scrapes each | — | **18 of 18 HTTP 200** |
| dog pid | 83876 unmoved | 85698 unmoved |
| dog restarts | 0 | 0 |
| dog status | online | online |
| dog stderr | 0 bytes | 0 bytes |
| `shep daemon reload` exit | 0 | 0 every time |

Every column except the scrape reads identically in both builds, which is the whole point of this defect: **a pid check cannot see it.** Restarts 0, status online, stderr 0 bytes and an unmoved pid describe a dog that has been answering 503 to everything for six reloads just as well as they describe a healthy one.

**The decisive check is content, not status.** A `freshsheep` started AFTER the six reloads appears in the exposition:

```
shep_sheep_status{sheep="freshsheep",id="2",fold="",status="online"} 1
```

A cached reading, a replayed exposition, or a connection to a predecessor could not produce that row. The dog is holding a live connection to the daemon that is running now.

**Bark, same drill, with a valid `[dog.bark.sinks]` entry:**

| | before reload | after 1 | after 2 |
|---|---|---|---|
| bark pid / restarts | 86973 / 25 | 87096 / **26** | 87191 / **27** |
| metrics pid / restarts | 85698 / 0 | 85698 / 0 | 85698 / 0 |

One restart per reload, pid moving each time. 2b's measurement, reproduced.

**Found in passing, unrelated and not fixed.** `shep enable bark` with no `[dog.bark.sinks]` at all produces a crash loop, not a refusal: `shep dog bark: rule 0 routes to no sink at all`, once per restart, restarts climbing about 6 per 2s. The default rule set is built from a sinks map that is empty, so the dog cannot start on the configuration `enable` leaves behind. Sibling of the `[[dog.bark.rules]]` parse defect 2b recorded, and like it, not the handover.

##### Mutations

Each test broken deliberately, run, restored. The blast radius is per-mutation and stated because two of them started out over-broad:

| mutation | fails |
|---|---|
| supervisor never spawned | all six `reconnect` tests + the metrics end-to-end |
| ack cached at construction | the two ack tests, and only those |
| in-flight request retried against the successor | `an_in_flight_request_fails_and_is_never_re_sent_to_the_successor` |
| retry after a refusal instead of returning | `a_refused_reconnect_stops_rather_than_spinning` |
| give up on a transient failure | `a_reconnect_retries_past_a_successor_that_is_not_accepting_yet` |
| `Drop` no longer aborts the supervisor | `dropping_the_handle_stops_the_supervisor` |
| supervisor never observes the death | the metrics end-to-end, alone among the dog tests |

Two of those needed the tests strengthening before they died, and both are worth recording because the first attempt looked green:

- **The refusal test passed the spin mutation.** It asserted `accepted() == 2` immediately after `link()` reached `Refused`, and a supervisor that recorded the refusal and then slept 50ms before going round again satisfies that. Fixed by adding a bounded negative — the count must NOT reach 3 within 8x `RECONNECT_MIN_DELAY`. A negative assertion is only as good as its window, so the window is stated against the delay it has to outrun.
- **The wait helper conflated the reconnect with the ack.** It polled `daemon().pid`, so caching the ack killed four tests instead of the two that are about the ack. Rewritten to poll the fake's accept count AND `link() == Connected`; neither alone is sound (the count rises before the handshake completes, and the link still reads `Connected` in the instant after a cut), and together they are, because the supervisor sets `Reconnecting` before it dials.

**One mutation survives and is reported rather than chased.** Retrying an in-flight `Closed` against the SAME dead generation passes every test. It is equivalent in effect — both attempts meet the same closed channel and return `Closed` — and killing it would need a test asserting how many times `request` consults the current generation, which is implementation shape rather than behaviour. The dangerous version, waiting for the fresh generation and then re-sending, is caught.

##### Gate

`cargo fmt --all --check` EXIT=0. `cargo clippy --workspace --all-targets --all-features -- -D warnings` EXIT=0. `cargo test --workspace --all-features` EXIT=0, **2167 passed**, 0 failed. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` EXIT=0. Windows cross-check with its own `CARGO_TARGET_DIR` EXIT=0.

`web/` needs nothing and this was checked rather than assumed: `cargo build --release` then `./web/scripts/generate-cli-reference.sh` leaves `git status --porcelain web/` empty, 2510 lines and 40 verbs, unchanged.

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
