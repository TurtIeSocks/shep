# Daemon handover, phase 3: dogs

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this task-by-task. Steps use `- [ ]` for tracking.

**Goal:** Carry every dog across the handover, giving it a connection that
survives the shepherd it was talking to being replaced.

**Amended after task 4**, which measured what shipped rather than what was
planned. This said "with no restart", and that holds for `metrics` and not
for `bark`: bark's event stream belongs to one connection generation, so it
still exits and is replaced once per reload. Closing that needs a ruling on
what an orphaned dog does, which is a question about every dog rather than
about bark, and is recorded rather than left as a quietly unmet step.

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
for whoever implements task 2. **Built in task 2 as `Hello.dog_name`, with
`PROTOCOL_VERSION` unmoved and that reading pinned by a test** — see that
task's outcome section.

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

- [x] **Step 1: Write the failing tests**, including that a twice-refused dog is NOT restarted a third time.
- [x] **Step 2: Run them, watch them fail.**
- [x] **Step 3: Implement.** `server.rs` currently drops everything but `hello.protocol` and logs the refusal at no level; that is half the defect.
- [x] **Step 4: Prove each non-vacuous**, especially the never-loop case.
- [x] **Step 5: Real reload** with a deliberately stale dog, proving one restart and no loop.
- [x] **Step 6: Commit.**

#### Outcome

**`Hello.dog_name`, an `Option<String>`, and `PROTOCOL_VERSION` did not move.** The decision above was taken on the reading that `Hello` carries no `#[serde(deny_unknown_fields)]`; that reading is now pinned by `a_hello_without_a_dog_name_still_parses`, which asserts both directions — a committed byte fixture from before the field, and a frame carrying a key an older daemon would not know. Nobody can add `deny_unknown_fields` later without seeing what it breaks. `#[serde(skip_serializing_if)]` follows `RpcError::daemon_version`, which was added under the same argument, so a non-dog client's `Hello` is byte-identical to the one protocol 2 shipped with and `hello_handshake_shape` needed no change.

**The CLI cannot name a dog, by construction, and that is the same shape task 1 chose for the same reason.** A `Hello` naming a dog is what lets the daemon restart that dog, so a client able to claim a name it was not spawned as could have a dog restarted on its say-so. `Client`'s public constructors pass `None`; its dog-naming constructor is `pub(crate)` and its only caller is `ReconnectingClient::connect_as_dog`, the type dogs name and no `shep` verb does. Nothing in `shep-client` reads `$SHEP_DOG_NAME` — `DogRuntime::start` takes the name from its own caller's argv — so `SHEP_DOG_NAME=x shep stop web` cannot reach the branch.

**The name rides every handshake, not just the first.** The refusal that matters is a successor's, and a dog that named itself at boot and then reconnected anonymously would leave that successor exactly as unable to act as it was before.

**One restart falls out of the state, not a budget.** `DogRefusals` is a count per dog, cleared by a SUCCESSFUL handshake and by nothing else. A dog that cannot get in never clears, so it can never earn a second restart however many times it is refused; a dog that does get in is back to a clean slate, so a later daemon that refuses it gets its own one restart. One restart per episode, and an episode ends when the dog is talking again. The count does not survive a handover, deliberately: a successor has refused nobody, and a dog it can talk to is not stale by any definition it could apply.

**In `dogs.rs`, not the supervisor**, for the reason `spawn_dog_watch` already gives: this answers *who should see this*, from outside, and the supervisor stays a machine that knows only how to supervise. The connection layer supplies the one fact only it has — a handshake was refused, by this name — and asks `dogs.rs` what that costs. The restart itself runs on its own task, because a restart is a full kill ladder and the caller is a connection handler holding a socket this daemon has already refused.

**What it does NOT do is stop a stale dog's own `autorestart`, and that is worth stating because the drill makes it visible.** A dog whose process EXITS on a refused handshake — a FIRST connection refused, rather than one lost to a handover — is respawned by the supervisor as any sheep is, and goes on being refused until its restart budget (16) runs out. That loop is bounded and it is the supervisor's existing mechanism. G8 is about not adding daemon restarts on top of it. The third refusal onward is `AlreadyStale`: no restart, and no second error line.

**A refusal carrying no dog name is `debug!`, and a real reload decided that.** It was `warn!` until it was measured. The daemon's default level IS `warn`, and `shep daemon reload` across a protocol bump leaves the CLI polling for a successor it cannot speak to: one reload wrote **442 of those lines in 9.8 seconds**. The operator is already reading the skew in plain English from their own CLI and `handle_conn` has always logged the closed connection at `debug!`, so the only fact the line adds is the client's crate version. A dog's refusal is the opposite case in every respect and keeps its level.

##### Drill, measured

Route 2 of the two the brief offered, plus a genuinely stale dog. Under an isolated `$SHEP_HOME` at `/tmp/p3b/home`, release builds, and three binaries: `shep-old` (this tree, protocol 2), `shep-new` (this tree with `PROTOCOL_VERSION` bumped to 3), and a predecessor built with `if false && entry.dog.is_some()` so the reload really hands a dog over. Both source edits reverted and proven reverted — `git diff` empty — before the gate. The dog is a shim that `exec`s `shep-old dog metrics`, adopted under the name `metrics`, so the running image and the disk binary are both protocol 2 while the daemon speaks 3.

**A. A dog that stays alive when refused, which is what a carried dog is.** The shim holds the process open after the refusal rather than exiting, so every restart in this run is one the daemon chose.

| | value |
|---|---|
| refusals the daemon logged | 3 lines, total, ever |
| G8 restarts issued | **1** (pid 22658 → 22703, restarts 0 → 1) |
| second refusal | 11 ms later, `ERROR ... stale and will not be restarted again` |
| pid at T+74s | **22703, unmoved** |
| restarts at T+74s | **1, frozen** |
| further daemon log lines | **0** |

**B. The ordinary case G8 exists for: stale running image, correct binary on disk.** The shim runs `shep-old` once and `shep-new` on every exec after it, which is exactly what a package manager leaves behind.

| | value |
|---|---|
| G8 restarts issued | 1 |
| error lines | **0** — no second refusal happened |
| `curl 127.0.0.1:9615/metrics` | **HTTP 200** |
| decisive check | a sheep started AFTER the restart appears in the exposition |

That last row is the one that matters, for the reason task 1 gave: a pid check cannot tell a live connection from a dead one, and only content can.

**C. Through a real `shep daemon reload`.** Predecessor at protocol 2 with the dog carry un-refused, the binary on disk replaced by an atomic rename as a package manager would, then `daemon reload`.

| | value |
|---|---|
| daemon pid across the reload | 25677, unchanged — the exec happened |
| successor's boot | `a sheep is already registered under this name` — the carry happened |
| G8 restarts issued | **1** |
| further refusals, from the dog's own autorestart | 15 |
| further G8 action | **none** |
| whole daemon log, default level | **3 lines** (445 before the `debug!` fix) |
| operator's end state | `metrics errored`, restarts 16 (the budget), exit 6; `web` untouched |

G8's restarts and `autorestart`'s are separable only in the log, which is why the log is the assertion: one `restarting it once` line against 15 later refusals.

**Found in passing, and it is task 4's, not a defect here.** A carried dog loses its `dog` marker, which the plan already records for task 4 — but the consequence runs further than `shep dogs` and `shep flock`. `spawn_dog_watch` records an exhausted restart budget only when `info.dog.is_some()`, so the carried dog that erupted through its whole budget above wrote **nothing** to `barks.jsonl`. The bark an operator would read after the outage depends on the marker the carry drops.

**Also found in passing.** `dog_version` in every G8 line reads `0.1.22` for both the protocol-2 and the protocol-3 build, because they differ only in `PROTOCOL_VERSION`. That is G9's point made concrete: `Hello.client_version` is the only thing that knows, and it does not know either.

##### Mutations

Ten, each applied, run against the full four-crate lib suite with `--no-fail-fast`, and reverted:

| mutation | fails |
|---|---|
| `Hello` gains `deny_unknown_fields` | the no-dog-name fixture, alone |
| `skip_serializing_if` dropped from `dog_name` | `hello_handshake_shape`, alone |
| the reconnect supervisor dials without the name | `a_dogs_name_rides_every_handshake_including_the_refused_one` |
| `Client::connect_as` drops the name before the frame | that test, plus the dog-runtime one |
| `DogRuntime::start` calls `connect` instead of `connect_as_dog` | `a_dog_announces_its_own_name_at_the_handshake`, alone |
| a stale dog is restarted too | `a_twice_refused_dog_is_reported_stale_and_never_restarted_again`, alone |
| the second refusal never reads `Stale` | that test, plus the two ladder tests |
| `handshook` becomes a no-op | the two clearing tests, and only those |
| the refusal path ignores `hello.dog_name` | all three wiring tests |
| `AlreadyStale` collapses into `Stale` | the ladder test, alone |

Two are worth recording:

- **`DogRuntime::start` calling `connect` was the one only an end-to-end run would have caught**, before a test was written at that seam. It is one line, it makes every real dog announce itself, and the whole of the rest of the suite is blind to it. `a_dog_announces_its_own_name_at_the_handshake` is what closes that, and it needed `fake_daemon`'s returned `Hello` rather than `serve_one_request`'s envelope.
- **The never-loop test was checked against its own window.** Task 1 found its refusal test initially passed a spin, so this one asserts three independent things: the dog is REPORTED stale, its pid does not move inside a window sized at ten times the restart that really happened earlier in the same test, and the harness is scripted with exactly the two spawns G8 permits so a third would fail to spawn and take the dog out of `Online`. The window was then verified alone — with spare scripts added so the exhaustion tripwire could not fire, the spin mutation still failed it, at pid 1002 inside 200 ms. The two mechanisms agree rather than one propping up the other.

##### Gate

`cargo fmt --all --check` EXIT=0. `cargo clippy --workspace --all-targets --all-features -- -D warnings` EXIT=0. `cargo test --workspace --all-features` EXIT=0, **2180 passed**, 0 failed — task 1's 2167 plus the thirteen this task adds. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` EXIT=0. Windows cross-check with its own `CARGO_TARGET_DIR` EXIT=0.

**`web/` needed something this time, and the generator is not what found it.** `cargo build --release` then `./web/scripts/generate-cli-reference.sh` leaves the generated reference byte-identical — 2510 lines, 40 verbs — because no verb, flag or exit code moved. The hand-written pages are the ones that went stale: both `docs/dogs.md` and `web/src/pages/docs/dogs.astro` state the third-party dog wire contract, and both told an author to read `$SHEP_DOG_NAME`, put it in `Request::DogConfig`, and stopped there. A dog following either page exactly sends an anonymous `Hello` and cannot be restarted. Both now name `dog_name`, and both name `ReconnectingClient::connect_as_dog` — which was already missing from task 1, since the reconnect lives on that type and not on `Client`. `astro build` EXIT=0, `astro check` EXIT=0, 0 errors and 0 warnings.

### Task 3: a reconnected dog's version is fresh (G13)

**Files:** `crates/shep-client/src/`, `crates/shep-daemon/src/`.

`Client::ack` is taken once at connect and handed out by `daemon()`. After a reconnect it would still describe the predecessor. `metrics` prints `daemon_version` from it, so it would publish a version that is no longer running.

G13's rule: `daemon reload` reports dog staleness AFTER the dogs have reconnected, when the answer is a fact rather than a claim about a process being replaced.

- [x] **Step 1: Write the failing test.** After a reconnect, `daemon()` reports the successor.
- [x] **Step 2: Run it, watch it fail.**
- [x] **Step 3: Implement.**
- [x] **Step 4: Prove it non-vacuous.**
- [x] **Step 5: Real reload**, scraping `metrics` afterwards and reading `daemon_version` back.
- [x] **Step 6: Commit.**

#### Outcome

**Steps 1 to 4 were task 1's, and were verified rather than trusted.**
`ReconnectingClient::daemon` reads the ack off the generation answering
right now and returns it owned (`reconnect.rs`), the reading is pinned by
`the_ack_follows_the_successor_rather_than_the_predecessor`, and the
metrics dog takes it per scrape from the generation that just answered
`ListFlock` (`dog/metrics/mod.rs`, with a comment saying why two reads
either side of a handover would publish one daemon's version beside
another's pid). Nothing was rebuilt. The client half of G13 is done.

**The daemon half is a reading the operator asks for, not a line the
daemon logs.** `Request::DogStaleness` answers with two lists: `stale`,
which is `DogRefusals::stale()` and the only thing reportable, and
`pending`, which is why the caller must ask again. `PROTOCOL_VERSION` did
not move: both are `#[non_exhaustive]` enums gaining a variant, which is
what the evolution rule on that constant already calls additive, and the
one caller is `daemon reload` asking a successor it has just proven runs
this binary's own version.

**`pending` has two sources because a dog can be unheard-from in two
ways.** A dog refused ONCE is mid-restart, so G8 has asked for its verdict
and the verdict has not arrived. A dog the daemon supervises that has never
handshaken has not been asked yet, which is exactly what a carried dog is
for the whole gap between the exec and its reconnect. The second needed
`DogRefusals` to record accepted handshakes as well as refused ones:
"nobody has heard from that dog" and "that dog is talking happily" both
have no refusal recorded, and telling them apart is the whole of what the
waiting is for.

**The supervisor's own rows are a sound roster, and that is a property of
`boot` rather than an assumption.** A dog is registered before this daemon
accepts anything: restore the flock, `spawn_enabled_dogs`, and only then
hand the listener to `serve`. So a caller that can ask the question is
talking to a daemon that already knows which dogs it has, and there is no
window where an empty roster reads as "every dog answered".

**One gap, measured, and task 4 closes it for free.** A CARRIED dog loses
its `dog` marker today, so on the handover arm the roster is empty and the
wait rests on the refused-once half alone. Confirmed under an isolated
`SHEP_HOME` in drill C below: after a carried reload `shep dogs` prints
nothing at all and `metrics` sits in `shep flock` beside the operator's own
app. Nothing here can say anything FALSE as a result, because silence is
what a clean reload prints anyway, but it can be silent about a dog that
had not dialled back yet. Restoring the marker in `CarriedSheep` populates
the roster and the wait covers a carried dog too, with no change to this
task's code.

**Only a dog with a PROCESS is waited on, and a real crash loop is what
decided that.** `Starting` and `Online`, not `WaitingRestart` or `Errored`:
a dog with no process cannot handshake, so waiting on one would make an
operator pay a whole timeout for a dog every other listing already reports
as broken. `shep enable bark` with no `[dog.bark.sinks]` produces exactly
that state today (task 1 found the crash loop), and it is `waiting-restart`
most of the time. Nothing is lost by leaving those out: a dog whose restart
this daemon ASKED for is in the refused-once half whatever its row says.

**Silent unless something is wrong, and the wait is not paid unless there
is something to wait for.** A flock whose dogs all came back prints nothing
about them and finds nothing pending on its first ask. Three ordinary
reloads measured at 0.154s, 0.156s and 0.155s, against 0.935s for the one
with a dog to wait for.

**The report says what happened and never what shep did not check.** Two
handshakes were refused with a restart from the binary on disk in between;
what version that file holds is unread, and unreadable until a dog answers
`--version` (G11, a later phase). So the sentence is "restarting it from
the binary on disk did not help, so rebuild or reinstall it", never a claim
about the file. `dog_version` is deliberately not on the wire either: task
2 measured two builds differing only in `PROTOCOL_VERSION` both reporting
`0.1.22`, so carrying a version here would invite the one inference it
cannot support.

**Nothing new reaches the daemon log.** Task 2's 442-line incident is the
cautionary one, and this was checked rather than assumed: three ordinary
reloads with a healthy dog wrote **0 lines**, and the one with a stale dog
wrote the same **2** G8 already wrote.

##### What the operator sees

Ordinary reload, healthy dogs, verbatim and complete:

```
notice[reload]: sheep 'metrics' has being a dog, which this daemon cannot yet hand over; reload falls back to a stop-and-start instead
notice[reload]: the shepherd is now 0.1.22 (pid 97375)
ID  NAME  STATUS  PID    RESTARTS  EXIT  CPU  MEM  UPTIME  FOLD  SMIT
0   fast  online  97396  0         -     -    -    0s      -     -
```

Both notices predate this task. A stale dog adds exactly one line:

```
notice[reload]: the `metrics` dog cannot talk to this shepherd; restarting it from the binary on disk did not help, so rebuild or reinstall it against shep 0.1.22
```

And a dog that never answers inside the budget adds a different one, so
that silence never has to mean two things:

```
notice[reload]: the `metrics` dog had not answered this shepherd after 3s, so this reload cannot say whether it came back
```

##### Drill, measured

Under an isolated `SHEP_HOME` at `/tmp/p3c/home`, release builds, one `awk`
sheep plus a dog. Three binaries from this tree: `shep-old` (protocol 2),
`shep-new` (protocol 3), and `shep-carry` (protocol 2 with `if false &&
entry.dog.is_some()` in `handover::refusal`, route 2 of the two the brief
offered). Both source edits reverted and proven reverted before the gate:
`git diff --stat crates/shep-core/src/protocol/mod.rs
crates/shep-daemon/src/handover/mod.rs` is empty.

The stale dog is an adopted shim that `exec`s `shep-old dog metrics`, so
its running image AND its binary on disk are both protocol 2 while the
daemon speaks 3 — G12's row 4, the one a restart cannot fix.

**A. The decisive one: the two answers DIFFER.** The shim sleeps 400ms
before exec'ing, which is what a dog with work to do at startup looks like.
One `shep daemon reload`, CLI stderr timestamped, against the daemon's own
log:

| time (UTC) | what happened | what a report taken then would say |
|---|---|---|
| 16:42:44.600 | CLI prints the shepherd line; the reading loop starts | **nothing — the successor had refused nobody** |
| 16:42:44.930 | first refusal, G8 restart issued (+330ms) | nothing |
| 16:42:45.351 | second refusal; `stale` becomes `["metrics"]` (+751ms) | `metrics` |
| 16:42:45.380 | CLI prints the stale report (+780ms) | `metrics` |

**A report taken when the loop started would have said nothing at all.**
That is the whole of G13, measured: the answer did not exist yet at the
moment a naive report would have been taken, and it took a restart round
trip to come into existence. Whole verb: 0.935s, exit 0, daemon log 2
lines.

**A'. The same drill with a shim that does not sleep**, kept because it
shows how narrow the window is without one: first refusal 16:35:21.989,
second 16:35:22.001 (**12ms** for a whole kill ladder, spawn, connect and
refusal), and the CLI's shepherd line at 16:35:22.067 — the answer was
already a fact before the loop started. Correct report either way, and 12ms
is not a margin to design a report around.

**B. The ordinary case, three times, with a healthy built-in dog:**

| | reload 1 | reload 2 | reload 3 |
|---|---|---|---|
| wall clock | 0.154s | 0.156s | 0.155s |
| lines about dogs | **0** | **0** | **0** |
| daemon log lines | **0** | **0** | **0** |
| `curl /metrics` after | 200 | 200 | 200 |

**C. The handover arm, dogs carried** (`shep-carry` both sides, no protocol
skew):

| | value |
|---|---|
| daemon pid / dog pid | 98455 / 98501 **unmoved** |
| `curl /metrics` after | **200** — the reconnect works |
| lines about dogs | **0**, correctly: nothing was stale |
| wall clock | 0.055s |
| `shep dogs` after | **empty** |
| `shep flock` after | `metrics` listed beside `fast` |
| daemon log | 1 line, the carry's own "already registered under this name" |

The last two rows are the marker loss, reproduced. They are what bounds
this task's guarantee on the handover arm, and what task 4 restores.

**D. A handover ACROSS a protocol bump cannot be reported, and that is the
fixture rather than the code.** The handover arm needs a CLI that can talk
to the PREDECESSOR, and after the exec that same CLI cannot talk to the
successor: `await_successor` ran its whole 10s budget, fell back to
"starting one instead", and the reading never ran. The daemon did its half
regardless (refusal 16:46:21.458, stale 16:46:21.879). Worth recording so
nobody reads the silence as a defect: an operator upgrading shep replaces
CLI and daemon together, which is drill A's arm.

##### Mutations

Fourteen, each applied, run against the three-crate lib suite with
`--no-fail-fast`, and restored byte-for-byte from a saved copy. **No
survivors.**

| mutation | fails |
|---|---|
| the supervisor half never contributes | the not-handshaken test and the not-running one |
| a handshake no longer settles a dog | `a_dog_that_has_not_handshaken_is_pending` |
| a stale dog is also reported pending | `a_dog_being_restarted_is_pending_and_then_stale` |
| a dog with no process is waited on | `a_dog_that_has_stopped_running_is_not_waited_on` |
| every sheep is waited on, not only dogs | `a_flock_of_ordinary_sheep_has_nothing_stale_and_nothing_pending` |
| a stale dog still reads as mid-restart | the `restarting` test, and the rpc ladder |
| a refusal no longer forgets the handshake | `only_an_accepted_handshake_says_a_dog_has_answered` |
| a handshake is never recorded | that test, plus the rpc pending one |
| the report is taken on the first ask | both waiting tests |
| an unanswered dog is never mentioned | `a_reload_stops_waiting_for_a_dog_that_never_answers` |
| the report fires with nothing stale | `a_reload_whose_dogs_all_answered_says_nothing_about_them` |
| the reload never asks about dogs | both waiting tests |
| the report claims it read the disk | `the_stale_report_says_what_happened_and_never_reads_the_disk` |
| the wait has no deadline | `a_reload_stops_waiting_for_a_dog_that_never_answers` |

Three worth recording:

- **The sheep in the no-dogs test is the point, not scenery.** The first
  version of that case had an empty flock, and the mutation that walked
  every row instead of every DOG row passed it. A sheep does not speak this
  protocol at all (spec, part 4), so a reader that waited for `web` to
  handshake would hold an operator's reload open forever. The test starts a
  real sheep now and the mutation dies.
- **The never-answers test hung rather than failed** when the deadline
  mutation was applied, which is a signal nobody can read. It wraps the
  call in a `tokio::time::timeout` now, so the same mutation fails in 5.01s
  naming the line (IR-46: the timeout is both the forcing mechanism and the
  assertion).
- **The disk-claim mutation is the one only a wording test catches.**
  Rewriting the sentence to say the binary on disk is the wrong version
  compiles, reads plausibly, passes every behavioural test, and asserts
  something shep cannot know until G11. Pinned as an exact string in both
  the singular and the plural shape, because the two are written out
  separately and a copy-paste between them is invisible.

##### Gate

`cargo fmt --all --check` EXIT=0. `cargo clippy --workspace --all-targets
--all-features -- -D warnings` EXIT=0. `cargo test --workspace
--all-features` EXIT=0, **2190 passed**, 0 failed across 32 binaries — task
2's 2180 plus the ten this task adds. `RUSTDOCFLAGS="-D warnings" cargo doc
--workspace --no-deps --all-features` EXIT=0. Windows cross-check with its
own `CARGO_TARGET_DIR` EXIT=0 (the four dead-code warnings are the
`cfg(unix)` ones `CLAUDE.md` documents).

**`web/` needed the hand-written half only.** `cargo build --release` then
`./web/scripts/generate-cli-reference.sh` leaves the generated reference
byte-identical — 2510 lines, 40 verbs — because no verb, flag or exit code
moved, and a stale dog does not change the exit code either. Three prose
pages did go stale: `getting-started.astro` documents the reload and said
nothing about what it reports, and `docs/dogs.md` and
`web/src/pages/docs/dogs.astro` both told a dog author that a `dog_name`
gets them "reported stale" without saying where that report goes. `astro
build` EXIT=0, `astro check` EXIT=0, 0 errors and 0 warnings.

### Task 4: strike the refusal

**Files:** `crates/shep-daemon/src/handover/mod.rs`, `crates/shep-daemon/src/supervisor.rs`, `web/src/pages/docs/getting-started.astro`.

Last one, and it is four lines plus the fixtures. 2b built and measured this carry already, so the mechanism is known: strike `entry.dog.is_some()` in `refusal`, give `CarriedSheep` a `dog: Option<DogSource>`, restore it in `install_adopted`.

Two things 2b recorded for whoever does this:

- **Losing the `dog` field is not cosmetic.** `matching_ids` includes a dog only for an exact selector, so a carried dog without its marker leaves `shep dogs` and appears in `shep flock` beside the operator's own apps.
- **Four tests use `RefusedReason::Dog` as their refusal fixture.** The two in `handover/mod.rs` are one line each (`e.reload = ReloadState::Replacement`). The two in `boot.rs` boot a real daemon, and a reload parked in `AwaitReady` is the one to build: a pending stop needs a script that ignores signals, which then blocks the test's own 5s teardown on `kill_timeout`.

- [x] **Step 1: Write the failing tests.**
- [x] **Step 2: Run them, watch them fail.**
- [x] **Step 3: Implement, and move the four fixtures.**
- [x] **Step 4: Prove each non-vacuous.**
- [x] **Step 5: Real reload** with both built-in dogs, proving `curl /metrics` still answers 200 afterwards and neither dog's restart count moved. Half met and half ruled on: metrics holds its pid and `restarts 0` across six reloads while serving 200; bark still restarts once per reload, and the ruling is below.
- [x] **Step 6: `web/` and commit.**

#### Outcome

**Four lines, as 2b measured, and four more that 2b could not have seen.**
The carry itself was exactly what that section promised: strike
`entry.dog.is_some()` in `refusal`, give `CarriedSheep` a `dog:
Option<DogSource>`, read it off the entry in `from_entry` the way `reload`
already is, restore it in `install_adopted`. `VERSION` unmoved, no
`#[serde(default)]`, a legacy-blob test beside its seven siblings. What was
not in the estimate: the four lines put a dog into the muster roll for the
first time, and the fix for that is its own line with its own argument. See
"Found in passing" below.

**`PumpUnresponsive` is the only refusal left, and `Display` changed shape
because of it.** The `(sheep, feature)` template existed for the variants
that read "which this daemon cannot yet hand over"; `Dog` was the last of
them, and a wedged pump has always had its own sentence because it is a
fault rather than a gap a later phase closes. The template went with the
variant. The tail — "reload falls back to a stop-and-start instead" — is
unchanged and load-bearing: two `cli_e2e` cases probe for exactly that
string to tell a carried flock from a stopped-and-started one, and a third
does now.

**The enum stays `#[non_exhaustive]` on a different argument than before.**
It used to be justified by how much of the handover had shipped — every
phase turned one variant into something the daemon carries, so a matcher had
to tolerate variants going away. There is nothing left to remove. It stays
because shep-daemon is a published library and a second way for a live
sheep's descriptors to be unknowable would otherwise be a breaking change
for an out-of-tree matcher (IR-20).

**The whole `DogSource` rides the blob, not a boolean.** `shep dogs` prints
a SOURCE column, so a successor holding only "this is a dog" would answer it
with a guess. The adopted variant's path is the operator's own `shep adopt`
argument, which the blob's `AppConfig` already carries as the program to
run, so this adds nothing a reader of that file could not already see
(IR-41).

##### What the marker turns back on, measured rather than assumed

Four readers, and this task strengthens three of them with no change to
their code:

| reader | what it does with the marker | measured |
|---|---|---|
| `matching_ids` | passes a dog over for every selector but an exact one | `shep flock`'s sheep table lists `web` and `freshsheep` only; both dogs are in their own section |
| `dogs::spawn_dog_watch` | records an exhausted restart budget, but only for a marked entry | a CARRIED dog exhausted after a real reload wrote its line to `barks.jsonl` — drill B |
| `rpc::dog_staleness` | builds its roster from it | both dogs are waited on now; task 3's measured gap on the handover arm is closed |
| `dogs::spawn_enabled_dogs` | matches by name and inspects the reply's marker | the successor's "a sheep is already registered under this name" line is gone: **0 daemon log lines across six reloads**, against 1 in task 3's drill C |

##### Found in passing, and it is not cosmetic: a carried dog would have entered the muster roll

A successor rebuilds its registry from the blob — `record_config` per carried
app — and the snapshot writer turns that registry into `flock.json` within
seconds. **A dog has never been in the roll**: `spawn_enabled_dogs`
registers dogs straight through the supervisor and nothing on that path ever
touches `FlockRegistry`. Carrying one would have put it there for the first
time, and permanently, because the roll outlives the daemon: a later cold
boot would `restore_flock` `metrics` as an ordinary UNMARKED sheep before
`spawn_enabled_dogs` ran, and `shep disable metrics` could not take it back
out.

`boot::apps_for_the_roll` is the filter. It is a named function rather than
an inline `.filter()` because the registry takes bare `AppConfig`s, which
carry no marker: this is the last place that still holds the blob's own
rows, and that is the argument the function's doc makes.

**Only the end-to-end tier reaches the call site**, which is the mutation
worth recording — see below.

##### The ruling on bark: deferred, with the design written down

**Bark still restarts once per reload.** Measured here across three spaced
reloads: pid moving every time, `restarts` 2 → 3 → 4, `online` after each,
while metrics held pid 13096 at `restarts 0` throughout. Reproduces task 1
exactly.

**Three reloads inside 50ms produce ONE bark restart, not three**, and that
is worth knowing before reading any restart count: bark exits when the
connection it holds dies, so a reload it was not connected for costs it
nothing. The count is per reload bark is actually up for.

**The fix is not "re-arm the stream inside the client".** Task 1's argument
still holds: `ReconnectingClient::subscribe` re-arming its own stream would
silently swallow the gap between a connection dying and the successor
accepting a fresh `Subscribe`, and an event stream that hides a gap is worse
than one that ends.

**The fix belongs in bark, where the gap already has an answer.**
`run_loop`'s `Some(Err(dropped))` arm reconciles against `ListFlock` the
moment the bus reports a lag, on the reasoning that a drop carries no
information about what was lost and the only way to know is to ask the
shepherd what things look like now. A handover gap is the same class of loss
and deserves the same answer: re-subscribe, then reconcile. The state-based
rules and the per-subject debounce are already built for a subject seen
twice by two routes, so nothing new is invented.

**Four things stop it being small, and the fourth is why it is its own task
rather than a line here:**

1. `EventSource` needs a `resubscribe`, which means a production adapter
   holding the `ReconnectingClient` beside the stream and its topics.
   `run_bark` moves that client into `ClientFlockSource` today.
2. The adapter has to WAIT for the link before it can subscribe — a
   `Subscribe` against a dead generation fails immediately with `Closed` —
   and `ReconnectingClient` exposes `link()` as a reading, not a future.
3. `LinkState::Refused` has to exit rather than retry, so G8's one restart
   from disk still applies to a bark dog that cannot speak this protocol.
4. **It needs a ruling on the ORPHANED dog, which is about every dog rather
   than about bark.** Today bark exits when its shepherd goes away for any
   reason; one that re-subscribed instead would linger, and would attach
   itself to whatever daemon next binds that socket, beside that daemon's
   own bark dog. The metrics dog already has that hazard through
   `ReconnectingClient`'s supervisor, which retries forever, and nobody has
   ruled on it. **Observed live during this task's mutation pass**: an
   orphaned `shep dog metrics` from a killed test daemon was still resident
   nine minutes later.

Answering it for bark alone would leave the two dogs answering the same
question differently for the third time. Recorded in
`docs/specs/deferred.md` under "The bark dog still restarts once per
reload", with `run_loop`'s `None` arm pointing at it so the next reader
finds the ruling instead of rediscovering the question.

**Why the phase is still complete without it.** The defect phase 3 was built
for was a live process holding a dead socket: silent on both sides, 503 to
every scrape, and indistinguishable from health on every column a listing
has. Bark's is the opposite in every respect — it is a pid change and a
counter an operator can read, it self-heals every time, and
`install_adopted` gives each successor a fresh `RestartBudget` so reloads
cannot exhaust one. What it costs is a false reading on one column and a
lost debounce window, not an outage.

##### The four fixtures moved, and there is nowhere left for them to move to

All four needed A refusal rather than that one, and all four now use a
wedged log pump. Their fixture has moved every time a phase carried the
feature it was reaching for — a shepherd channel in 2b task 5, two instances
in task 6, a dog here — and this is the last move available.

| test | where | how it moved |
|---|---|---|
| `one_unsupported_sheep_refuses_the_whole_flock` | `handover/mod.rs` | a new `wedged(&entry)` helper beside `plain` |
| `the_refusal_names_which_sheep_and_why` | `handover/mod.rs` | same, plus an assertion on the "falls back to" tail that the old version did not make |
| `a_sighup_over_a_flock_it_cannot_carry_refuses_before_it_execs` | `boot.rs` | a `with_a_pump_that_never_reports` sheep started through the booted daemon |
| `an_abandoned_handover_starts_every_pump_reading_again` | `boot.rs` | a third sheep, wedged, beside the two that answer |

**2b's advice on the boot pair is stale and was not followed.** It said to
build a reload parked in `AwaitReady`, because a pending stop needs a script
that ignores signals and would then block the test's own 5s teardown on
`kill_timeout`. Both of those were refusals in 2b and neither is one now:
`refusal` reads `pump_unresponsive` and nothing else. A wedged pump is the
only fixture available, and it needs no choreography at all.

**Both boot cases moved to a paused clock, and that is not incidental.** The
refusal is a missed `REPORT_DEADLINE`, which is two real seconds each.
Measured: the daemon lib suite went **1.9s → 5.46s awake, and back to 2.35s
paused**; the two cases run in 0.03s, stable over ten consecutive runs. The
`SIGNAL_TEST_LOCK` guard is unrelated and stays — it is about concurrent
`raise()` reaching this daemon's real listeners, not about the clock — and
both comments now say which is which.

**`an_abandoned_handover_starts_every_pump_reading_again` kept its own job
rather than collapsing into its sibling.** With one wedged sheep it would
have been `a_handover_abandoned_on_a_wedged_pump_resumes_the_pumps_that_
parked` with a `boot()` in front. It has three sheep instead, two answering
and one wedged, so it still proves the resume reaches EVERY pump reported to
where the sibling has only one to reach — and it asserts `answered.len() ==
2` so a future edit cannot quietly reduce it to the sibling's shape.

##### Drill, measured

**No bypass, for the first time in this phase.** Tasks 1, 2 and 3 each
needed a temporary `if false && entry.dog.is_some()` to see anything. This
is a plain release build of the committed tree, driving `shep daemon reload`
against a real flock with both built-in dogs. Isolated `$SHEP_HOME` at
`/tmp/p3d/home` (13 bytes) and `/tmp/p3e/home`.

**A. Six reloads, both built-in dogs, one `sh` sheep.** Reloads 1-3 were
back to back; 4-6 were spaced four seconds apart so bark was connected for
each.

| | before | after 6 |
|---|---|---|
| shepherd pid | 13052 | **13052** |
| `web` pid / restarts | 13073 / 0 | **13073 / 0**, uptime 1m 49s unbroken |
| `metrics` pid / restarts | 13096 / 0 | **13096 / 0**, uptime 1m 49s |
| `bark` pid / restarts | 13871 / 1 | 14268 / **4** |
| `curl /metrics` | HTTP 200 | **HTTP 200, six of six** |
| reload wall clock | — | **0.043s, 0.049s, 0.050s** (spaced runs) |
| daemon log lines | 0 | **0** |
| `metrics-0-err.log` | 0 bytes | **0 bytes** |
| lines about dogs in the reload | — | **0** |

Reload wall clock is worth reading against task 3's, which measured 0.055s
for a handover arm whose dog roster was EMPTY because the marker was lost.
The roster is populated now, both dogs are waited on, and the verb is no
slower: the reconnect lands before the first ask.

**The decisive check is content, not status**, for the reason task 1 gave: a
pid check cannot tell a live connection from a dead one. A sheep started
after all six reloads appears in the exposition:

```
shep_sheep_status{sheep="freshsheep",id="4",fold="",status="online"} 1
```

**The two populations, after six reloads:**

```
$ shep dogs
ID  NAME     STATUS  PID    RESTARTS  EXIT  CPU   MEM    UPTIME  SOURCE
3   bark     online  14268  4         -     0.0%  11.0M  16s     built-in
1   metrics  online  13096  0         -     0.5%  12.3M  1m 49s  built-in

$ shep flock
ID  NAME        STATUS  PID    RESTARTS  EXIT  CPU   MEM   UPTIME  FOLD  SMIT
4   freshsheep  online  14378  0         -     0.2%  3.0M  2s      -     -
0   web         online  13073  0         -     0.1%  3.1M  1m 49s  -     -

Dogs
...
```

Task 3's drill C had `shep dogs` **empty** and `metrics` listed beside the
operator's own app. That is the marker loss, and it is closed.

**The muster roll, read back through `shep save`:** `{'file':
'/tmp/p3d/home/flock.json', 'apps': 2}`, holding `['freshsheep', 'web']`.
No dog, after six real handovers.

**B. `barks.jsonl`, which is task 2's finding and needs a dog that dies.**
An adopted shim dog (`shep adopt`, exits 1 once a sentinel file appears),
carried across a real reload — pid 14637 unmoved — then made to burn its
whole budget:

```
1   shimdog  errored  -  16  1  -  -  0s  adopted

$ shep barks
WHEN                 RULE    SUBJECT  MESSAGE
2026-08-31 13:56:26  daemon  shimdog  dog shimdog exhausted its restart budget: 16 restarts against a budget of 16
```

Task 2 measured a carried dog erupting through its whole budget and writing
**nothing**. One line now, and it is the alert an operator reads after an
outage.

**C. A dog that never handshakes costs every reload the full wait, and says
so.** The same shim, which does not speak the protocol at all:

```
wall=3.066s exit=0
notice[reload]: the shepherd is now 0.1.22 (pid 14591)
notice[reload]: the `shimdog` dog had not answered this shepherd after 3s, so this reload cannot say whether it came back
```

Reported rather than buried, because it is a real cost this task introduces
and it will reach a real operator: **a third-party dog built against a
`shep-client` older than task 2 sends an anonymous `Hello`, so nothing ever
records a handshake for it and every reload pays `DOG_SETTLE_WAIT` and
prints that line.** It is the phase decision's already-accepted cost
("mute after a reload until rebuilt") arriving as a sentence instead of as
silence, and the trade is three seconds against the stop arm restarting the
entire flock. Exit stays 0 and the flock is carried either way.

##### Mutations

Eight, each applied to the committed tree, run, and restored from a saved
copy rather than through git.

| mutation | fails |
|---|---|
| the gate refuses a dog again | `a_dog_is_carried_rather_than_refused`, alone |
| `from_entry` drops the marker | the two blob cases, the adopt case, the roll case — 4 |
| `from_entry` invents one on every sheep | those, plus three carried-reload cases — 7 |
| the blob field stops being optional on the wire | `a_blob_written_before_a_dog_was_carried_still_loads`, alone |
| `install_adopted` drops the marker | `an_adopted_dog_keeps_its_marker_and_stays_out_of_a_wildcard`, alone |
| `apps_for_the_roll` stops filtering | `a_carried_dog_does_not_reach_the_muster_roll`, alone |
| **`boot` stops CALLING `apps_for_the_roll`** | **the end-to-end case, alone in the workspace** |
| the reconnect supervisor never observes the death | 8 `reconnect` cases, the metrics dog case, and the end-to-end one |

Three worth recording:

- **The call-site mutation is the one only an end-to-end test catches, and it
  was not covered until this drill went looking for it.** Restoring the old
  `carried_apps.extend(...)` line leaves the helper, its unit case and all
  **703** daemon lib tests green, because nothing but a booted daemon drives
  `boot`'s use of it. The end-to-end case now reads the roll back through
  `shep save` and fails at 3 apps against 2. Every task in this phase found
  one of these; this is task 4's, and it is a `flock.json` that silently
  acquires a dog forever.
- **Inventing a marker has a blast radius of 7, and the three extra failures
  are the right alarm.** `a_carried_ready_failed_instance_is_still_
  replaceable`, `a_carried_reload_naming_no_registered_instance_is_dropped`
  and `a_carried_swap_that_cannot_finish_is_still_abandoned_on_time` all
  fail, because every carried sheep becomes a dog and wildcards stop
  reaching any of them. A change that turned the whole flock into a kennel
  would take `shep restart all` down with it, and three cases say so
  without being about dogs at all.
- **`a_dog_is_carried_rather_than_refused` is the only unit test guarding the
  strike itself**, and its radius is 1. Nothing else in the lib suite goes
  through `fitness` with a dog in it. The end-to-end case is the second
  guard, and it catches the same mutation through the real verb: the reload
  prints "falls back to a stop-and-start" and the assertion names it.

##### Gate

`cargo fmt --all --check` EXIT=0. `cargo clippy --workspace --all-targets
--all-features -- -D warnings` EXIT=0. `cargo test --workspace
--all-features` EXIT=0, **2196 passed**, 0 failed across 32 binaries — task
3's 2190 plus seven added and one removed. `RUSTDOCFLAGS="-D warnings" cargo
doc --workspace --no-deps --all-features` EXIT=0. Windows cross-check with
its own `CARGO_TARGET_DIR` EXIT=0 (the same `cfg(unix)` dead-code warnings
`CLAUDE.md` documents; `apps_for_the_roll` is `#[cfg(unix)]` and adds none).

Daemon lib: **703**, up from 698, and back to **2.35s** with the two boot
cases on a paused clock.

**`web/` needed the hand-written half only.** `cargo build --release` then
`./web/scripts/generate-cli-reference.sh` leaves the generated reference
byte-identical — 2510 lines, 40 verbs — because no verb, flag, exit code or
config key moved. Three pages carried claims this task makes false:
`getting-started.astro` said a dog sends the reload down the older path,
which is now a wedged log pump and nothing else; and `docs/dogs.md` and
`web/src/pages/docs/dogs.astro` documented the reconnect without ever saying
what a reload does to a dog, which is the question a dog author reading that
section actually has. Both now say it, including bark's exception, because a
page that claimed every dog keeps its pid would be wrong about one of the
two shipped dogs. `astro build` EXIT=0, `astro check` EXIT=0, 0 errors and 0
warnings.

##### Is the phase complete?

**Yes, with one residual named above and recorded in `deferred.md`.** Every
task's steps are ticked, `RefusedReason` has no feature variant left, and a
real `shep daemon reload` carries a dog for the first time. The residual is
bark's restart, which is G7's "no process restart at all" met for one of the
two built-in dogs and not the other. It is written down rather than left
implicit, in three places: this section, `docs/specs/deferred.md`, and the
`None` arm of `run_loop` itself.

What the phase does NOT close, deliberately and as the plan said at the
outset: G11 and G12's row 5 — a dog answering `--version` so `adopt` can
check the binary on disk.

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
