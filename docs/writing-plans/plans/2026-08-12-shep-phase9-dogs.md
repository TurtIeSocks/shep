# shep Phase 9 — dogs: the contract, the metrics dog, and the bark dog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this plan task by task. Steps use `- [ ]` for tracking.
>
> **REQUIRED SUB-SKILL:** invoke `shep-idiomatic-rust` before writing or reviewing any Rust here. Cite rules as `IR-<n>`.

**Goal:** ship the plugin surface spec §8 promises, together with both dogs that use it. A dog is a process speaking the client wire protocol, supervised by the daemon, marked as a dog — no second protocol, no second registry.

**Why both dogs in one phase** (design spec, first section): a contract exercised by one consumer is not validated. Metrics polls and serves; bark subscribes and writes state. They are different enough that the second is what finds the wrong abstraction, and building them in separate phases would harden the contract around the first.

**Success criterion:** `shep enable metrics && shep enable bark` on a running daemon brings both up, `shep flock` prints them in their own table beneath the sheep, `curl 127.0.0.1:9615/metrics` answers a Prometheus exposition naming every sheep and both dogs, and a sheep that exhausts its restart budget produces a POST to a local sink plus a line in `$SHEP_HOME/barks.jsonl`.

**Architecture.** Three layers, and only the first is new machinery:

- **The contract** — a `dog` marker on `ProcessEntry`, carried onto `ProcessInfo`; `Request::DogConfig` answering a dog's own `[dog.<name>]` section as an opaque TOML blob; `EnableDog`/`DisableDog` starting and stopping one through the *ordinary* supervisor path. No new kill ladder, no new backoff, no new budget.
- **Each dog's own logic** — a Prometheus exposition renderer plus a hand-rolled HTTP/1.1 server (metrics), and a rules engine plus webhook sinks plus a size-capped `barks.jsonl` ring (bark). Both are argv branches of the same binary, the multi-call pattern the hidden `daemon` subcommand already uses.
- **The surface** — `enable`/`disable`, `adopt`/`rehome`, `dogs`, `barks`, and a second table under `shep flock`.

**Tech stack:** one new *workspace* dependency, `reqwest` 0.13 over rustls (TLS for webhook POSTs — Task 19 states the reasoning and the exact feature list), plus `toml_edit`, which is already in `Cargo.lock` as `toml` 0.8's own dependency and so costs zero new crates. The metrics dog's HTTP server and every test's HTTP sink are still hand-rolled over `tokio::net::TcpListener`, and neither reaches for `hyper`, `axum`, or a `prometheus` crate — `reqwest` brings `hyper` in transitively as its own outbound transport for bark's webhook POSTs, but nothing shep writes itself sits on top of it.

---

## Global constraints

Every task implicitly includes these.

- **Never open, read, or reference `/Users/rin/GitHub/pm2`.** Clean-room. Everything about pm2's module system comes from `docs/brainstorming/specs/2026-08-12-shep-phase9-dogs-design.md`, which a dedicated design phase produced. `/Users/rin/GitHub/rand` is the style reference and may be read freely.
- **Never open `dump.pm2`, `ecosystem.config.js`, or `ecosystem.config.d.ts` in the repo root.** They are Rin's real production data, git-excluded, and nothing derived from them may enter a committed file.
- MSRV **1.88**, edition **2024**. Workspace lints deny `missing_docs` and `missing_debug_implementations`; `clippy::undocumented_unsafe_blocks` and `clippy::missing_errors_doc` are `deny`.
- `#![forbid(unsafe_code)]` in shep-core/shep-client/shep-cli; shep-daemon is `#![deny(unsafe_code)]` with the one `#![allow]` in `sys.rs`. **Nothing in this phase needs unsafe** — a task reaching for it has misread its brief.
- **Rule 10:** no task-relative phrasing in shipped comments or docs. Name the thing, never "Task 5", "this phase", "the new field".
- CHANGELOG entries (IR-45) **reconciled, not appended**, in the crate whose surface changed. Folded into the task whose deliverable needs them, never batched at the end.
- **`PROTOCOL_VERSION` stays 1.** Every wire addition is additive under `#[non_exhaustive]`. Any regenerated insta snapshot's delta must be **read and verified to be only the addition**, and pasted verbatim into the task report — a regenerated `.snap` is the easiest place in a diff to hide a change nobody re-derives. **Three snapshots pin this wire**, not one: `shep_core__protocol__request__tests__request_wire_v1.snap`, `…__reply_wire_v1.snap`, and `shep_core__protocol__events__tests__bus_event_wire_v1.snap`. A field added to `ProcessInfo` moves the last two; a `Request` variant moves the first.
- Terminology per `docs/terminology.md`: **dog** (a plugin process), **the shepherd** (the daemon, never "dog"), **adopt**/**rehome** (register/forget a third-party dog), **bark** (a webhook alert), **barks** (the history), **bleats** (log output), **lambs** (a sheep's child processes). Destructive ops and error text stay plain English.
- Commit style: conventional commits, footer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`. A commit message containing backticks uses `git commit -F -` with a **quoted** heredoc.

### The inner loop

```
cargo test -p shep-daemon --lib --all-features -- --skip watch:: --skip extras::
```

**1.25s, 333 of 412.** The 79 it skips are FSEvents-bound and cost 281s of CPU between them.

**Tasks that change `crates/shep-daemon/src/extras.rs` or `crates/shep-daemon/src/limits/` must run the lib suite unfiltered** — `--skip extras::` hides exactly the tests such a change breaks:

```
cargo test -p shep-daemon --lib --all-features -- --skip watch::
```

**~23s.** No task in this phase is expected to touch either file; a task that finds it must, says so in its report and switches to this form.

For shep-cli work: **`shep-cli` is `[[bin]]`-only.** `-p shep-cli --lib` errors with "no library targets". Use `cargo test -p shep-cli --bins`. For shep-core work: `cargo test -p shep-core --lib`.

### The task gate — once, when a task is otherwise done

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Each from **its own command** with `$?` captured directly, **never through a pipe**: this is zsh, a pipeline's `$?` is the last command's, and `${PIPESTATUS[0]}` is an empty string. Five tasks last phase hit this.

**One cargo command shape per task.** The workspace shares one target-dir build lock, so concurrent runs block rather than parallelise, and alternating `-p` with `--workspace` invalidates every crate whose feature set changed. Each task below states its shape; do not "helpfully" add the other. `benches/` is its own workspace with its own lock — it names only `shep_daemon::limits::sample`, which nothing here touches, so its two gates belong to the phase gate (Task 23) and to no individual task.

### Baseline

Measure at `main`'s head before Task 1 and record it in that task's report: `cargo test --workspace --all-features`, counting `Running`/`Doc-tests` lines against `test result:` lines rather than reading a green tail — `cargo test` stops at the first failing binary, so a red suite can read as a short green one.

### Test discipline

- **Every test carries a "fails if" comment naming the mutation it catches, and the mutation must actually be performed and watched to fail before the comment is written.** Seven tests last phase were caught unable to fail, all by mutation. Two shapes recur: an assertion comparing a value against the same constant the implementation uses (which passes for any value), and a fixture that cannot tell right from wrong. **Twice the right fix was to change the implementation, not the assertion** — the redundancy was the defect.
- **Scope every mutation to implementation lines.** An unscoped `sed` once rewrote a test's expected string along with the code it was measuring, producing a false clean measurement. Edit the one line by hand, or `sed` with a line-range confined to the function body.
- **Never `git checkout <file>` to revert a mutation.** One task discarded its own unstaged implementation that way. Take a `cp` snapshot first and restore from it:
  ```
  cp crates/shep-daemon/src/dogs.rs /tmp/dogs.rs.bak
  # ... mutate, run, watch it redden ...
  cp /tmp/dogs.rs.bak crates/shep-daemon/src/dogs.rs
  ```
- **Every test that awaits a daemon answer bounds its read.** Two mutations in Phase 7 *hung* the suite instead of reddening it, and a hung suite reads as a broken machine rather than as a failed assertion. `tokio::time::timeout(..) + recv`, never a bare `recv().await`, and never a bare `try_recv` for a negative assertion.
- **A dog test must reap what it spawns.** Dogs are real child processes wherever a test uses the real runner. A test that spawns one owns a `Drop` guard that kills it, and the suite must leave nothing reparented to init.
- **`to_child.send()` returning `Ok` is not delivery.** The first send after a child dies returns `Ok(())` and vanishes.
- Paused tokio clock by default, no sleeps, hand-rolled fakes, unique fixtures per test (IR-33/34).

---

## Settled decisions

Recorded so no task re-litigates them. Items marked (Rin) come from the approved design spec; the rest follow from it or from existing precedent and are mine — flag any you believe is wrong rather than working around it.

| # | Decision |
|---|---|
| 1 | (Rin) **A dog is a marker on the existing entry, not a second registry.** `ProcessEntry` gains `dog: Option<DogSource>`; `ProcessInfo` carries it onto the wire. Duplicating supervision would mean teaching reload, watch, cron, limits, the log plane and the muster roll about a second population. |
| 2 | (Rin) **The tripwire on that marker:** a `dog` branch answering *where did this come from* or *who should see this* is expected; a `dog` branch answering *how is this supervised* — a different kill ladder, backoff curve, restart budget, or meaning for `Errored` — is the warning that the separate registry should have been built. Checkable as Task 23's exit criterion. |
| 3 | **A dog never enters `FlockRegistry`, by construction rather than by a branch.** `Request::Start` is the only caller of `registry.record`, and `EnableDog` is a different verb on a different path. That is the whole of "absent from the muster roll" — no `if dog` anywhere in `snapshot.rs`. |
| 4 | (Rin) **Configuration travels over the socket, never the environment.** A dog inherits exactly one variable it did not already need to exec, `SHEP_HOME`; it connects, handshakes, and sends `Request::DogConfig { name }`. The reason is secrets: bark's sinks are webhook URLs, and the environment is readable from the process table, inherited by every child, and captured into crash dumps. |
| 5 | (Rin) **The reply is an opaque blob the dog parses** — the `[dog.<name>]` table rendered back to TOML text, not a typed shep structure. A third-party dog binds to the shape of its own section, not to our config model, file discovery, or layering rules. |
| 6 | **The daemon re-reads `shep.toml` on every `DogConfig` request** rather than serving a copy cached at boot. One reader, never stale, and it is what makes the documented "`shep disable X && shep enable X` re-reads the config" literally true. The cost is one small file read per dog connect, which happens once per dog per daemon lifetime. |
| 7 | **`shep.toml` has exactly one writer, and it is the CLI.** `enable`/`disable`/`adopt`/`rehome` edit the file; the daemon only ever reads it. Two writers would need locking, which is one of the three sharp edges the design spec records from pm2's own answer (read-whole-file/write-whole-file with no locking, so concurrent sets lose one). |
| 8 | **An adopted dog's binary path lives in `[daemon] adopted_dogs`, not in `[dog.<name>]`.** The per-dog section is the dog's own opaque config (decision 5), and putting a shep-owned key inside it would collide with a third-party dog's schema. A name in `enabled_dogs` with no entry in `adopted_dogs` is a built-in. |
| 9 | (Rin) **`adopt` is deliberately not `enable --exec`.** Turning on a dog that already ships inside the binary and vetting a binary shep has never seen are different acts with different failure modes — a missing path, a file that is not executable, the wrong architecture. `enable --exec` survives as a **hidden** alias, because it is what someone arriving from pm2 would try first. |
| 10 | (Rin) **Both of `adopt`'s arguments are positional** (`shep adopt <name> <path>`). Both are required and always present, so a flag for either is ceremony. The name stays separate from the binary's filename because the name is the config key: two adopted dogs running one binary under different `[dog.<name>]` sections need distinct names. |
| 11 | (Rin) **`enable` starts the dog immediately when a daemon is running**, rather than only arming it for the next boot. With no daemon running it writes the config, says so, and exits 0 — the dog comes up with the next boot. |
| 12 | (Rin) **A config change does not reach a running dog.** The dog read its section once, at connect. `shep disable <name> && shep enable <name>` re-reads it, and `docs/dogs.md` says so. Live push is a v1.1 question. |
| 13 | (Rin) **Two tables, one registry.** `shep flock` prints the sheep table, then a `Dogs` table beneath it whenever any dog is registered. `--format json` stays **one flat array** of every entry, each carrying its own `dog` marker: the JSON *is* the single registry, and the two tables are a rendering of it. `SCHEMA_VERSION` stays 1. |
| 14 | (Rin) **The dogs table shows by default.** §8 hid dogs behind `--all`; a separate table already achieves the uncluttered listing that was for, and a bark dog that has died is precisely the thing an operator needs to notice. `shep dogs` prints that second table alone. |
| 15 | (Rin) **No `--all` flag, at all.** The design spec's prose said `--all` would widen both tables to include stopped entries, while its own rendered sample showed a stopped sheep (`bpm_client  stopped`) in what read as default output — the two contradicted each other. Rin's ruling resolves it by fixing the prose, not the sample: stopped entries have always been visible by default and stay that way, so a flag that could only ever widen an already-unfiltered listing would widen nothing, and hiding stopped entries to give the flag something to do would be a user-visible regression against today's behaviour, against pm2's, and against that sample. The two-table split and the default-visible dogs table land; `--all` is dropped outright, not deferred. The design spec's `--all` sentence is corrected to match. |
| 16 | (Rin) **Listings sort by name.** Sorting by id scatters a clustered app's instances; sorting by name groups them. Applied **once**, in the actor's `snapshot_all`, so every listing reply and every consumer — the CLI, the metrics dog, bark's reconciliation — sees one order. Sorted by `(name, instance, id)`: instance keeps a clustered app's slots in their own order, and id breaks the tie a reload's fresh id creates. |
| 17 | **A dog is named by an exact `name` or `id` selector, and never by a wildcard one.** The design requires `reload all` to skip dogs; the same argument holds verbatim for `stop all` and `delete all`, where getting it wrong takes alerting down silently. Implemented once as `ProcessSelector::is_exact` plus a single selection helper in the actor, never as five copies of an `if`. |
| 18 | **The dog marker rides `ProcessInfo` as `Option<DogSource>`, and `None` conflates "a sheep" with "a peer that predates the field" on purpose.** Unlike `cpu_percent`, that conflation is *correct*: a daemon that predates dogs has none, so "not a dog" is the true answer in both cases. Say so in the field's doc rather than letting a reader assume it was overlooked. |
| 19 | (Rin) **Bark's sinks need TLS, so they need a dependency: `reqwest` 0.13.** Discord and Slack webhooks are HTTPS and there is no way around that. Rin has standardised on `reqwest` across her recent Rust projects, and consistency across the codebases she maintains outweighs a smaller crate count; an async client also fits a program that is tokio all the way down, where a blocking client would mean `spawn_blocking` around every webhook POST. `default-features = false` with only `rustls` named explicitly, matching the shape every dependency in this workspace already takes (Task 19 has the exact feature list and how it was confirmed). It ships in the binary unconditionally, for every user, by the same decided model that makes bark a dog rather than in-daemon code (`decision-briefs.md` §3b) — not a size tradeoff being accepted. The **test** server is hand-rolled over `tokio::net::TcpListener` and needs nothing. |
| 20 | (Rin) **Restart-loop detection is two rule kinds, not one threshold.** "The daemon gave up" is keyed to budget exhaustion, is on by default, has nothing to tune, and cannot disagree with the daemon. The early warning ("N restarts in M seconds") is opt-in, because it is the one that pages at 3am for a blip. |
| 21 | (Rin) **Bark reads `ProcessInfo.restarts` — the daemon's own count — rather than tallying bus events.** A private tally would drift from the number the daemon acts on, and the operator would be told a different story from the one the supervisor believes. |
| 22 | (Rin) **When a dog dies, the daemon records it and metrics exposes it — and nothing watches across dogs.** Two dogs observing each other adds a failure mode without adding an independent observer, and fails hardest when both go down together. The daemon's record is written by a bus watcher at the *edge* of the supervisor, never by a branch inside `handle_exited` (decision 2). |
| 23 | **`barks.jsonl`'s ring lives in shep-core**, because both the daemon (a dog that gave up) and the bark dog (a fired alert) append to it, and `shep barks` reads it. One implementation of the cap, or the two writers evict differently. |
| 24 | **`shep dog <name>` is dispatched from `run`'s early block, beside `daemon` and `bleats`, and takes no locked stdout/stderr guard.** A dog runs until it is signalled; a process-lifetime `StderrLock` held on the main thread wedged the daemon on its first warning in 2026-08-09 and would wedge a dog the same way. |

---

## File structure

| File | Create / Modify | Responsibility |
|---|---|---|
| `crates/shep-core/src/protocol/request.rs` | modify | `DogSource`, `ProcessInfo::dog`, `Request::DogConfig`/`EnableDog`/`DisableDog`, `Response::DogSection`/`DogStarted` |
| `crates/shep-core/src/config/daemon.rs` | modify | `DaemonSection::adopted_dogs` |
| `crates/shep-core/src/selector.rs` | modify | `ProcessSelector::is_exact` |
| `crates/shep-core/src/barks.rs` | **create** | the `Bark` record and the size-capped `barks.jsonl` ring |
| `crates/shep-daemon/src/entry.rs` | modify | `ProcessEntry::dog` |
| `crates/shep-daemon/src/supervisor.rs` | modify | `Command::StartDog`, `SupervisorHandle::start_dog`, `Actor::matching_ids`, `to_info`, `snapshot_all`'s order |
| `crates/shep-daemon/src/dogs.rs` | **create** | `DogSpec`, the dog's `AppConfig`, boot's dog spawn, the bus watcher that records a dog that gave up |
| `crates/shep-daemon/src/rpc.rs` | modify | the `DogConfig`/`EnableDog`/`DisableDog` arms |
| `crates/shep-daemon/src/boot.rs` | modify | `BootOptions::dogs`, the spawn step, the watcher |
| `crates/shep-cli/src/cli.rs` | modify | `enable`, `disable`, `adopt`, `rehome`, `dogs`, `barks`, hidden `dog` |
| `crates/shep-cli/src/main.rs` | modify | dispatch arms + their unit coverage |
| `crates/shep-cli/src/commands/daemon.rs` | modify | `boot_options` fills `dogs`; the inert-dog warning goes |
| `crates/shep-cli/src/commands/dogs.rs` | **create** | `enable`, `disable`, `adopt`, `rehome`, `dogs`, `barks` |
| `crates/shep-cli/src/commands/shep_toml.rs` | **create** | the one `shep.toml` writer, over `toml_edit` |
| `crates/shep-cli/src/dog/mod.rs` | **create** | `shep dog <name>` dispatch and the shared dog runtime |
| `crates/shep-cli/src/dog/http.rs` | **create** | the hand-rolled HTTP/1.1 request reader and response writer |
| `crates/shep-cli/src/dog/metrics/mod.rs` | **create** | the metrics dog |
| `crates/shep-cli/src/dog/metrics/exposition.rs` | **create** | the Prometheus text renderer |
| `crates/shep-cli/src/dog/bark/mod.rs` | **create** | the bark dog's loop |
| `crates/shep-cli/src/dog/bark/rules.rs` | **create** | the rules engine |
| `crates/shep-cli/src/dog/bark/sinks.rs` | **create** | Discord / Slack / generic JSON POST |
| `crates/shep-cli/src/output/rows.rs` | modify | `DogRows`, `BarkRows`, `dog` in `JSON_ONLY` |
| `crates/shep-cli/src/output/mod.rs` | modify | `emit_flock` — two tables, one registry |
| `assets/grafana/shep.json` | **create** | the reference dashboard |
| `docs/dogs.md` | **create** | the dog guide: the contract, the two dogs, third-party dogs |


---

## Task 1: the dog marker on the wire

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs`
- Modify: `crates/shep-core/src/protocol/snapshots/shep_core__protocol__request__tests__reply_wire_v1.snap`, `…__events__tests__bus_event_wire_v1.snap`
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces — produced, and depended on by Tasks 2, 5, 6, 9, 14, 20:**

```rust
/// Where a dog came from: this binary, or one an operator adopted.
///
/// The one thing an operator wants when a dog misbehaves, which is why it
/// is a column rather than a detail. Carried on [`ProcessInfo::dog`], so a
/// listing distinguishes the two populations without a second request.
///
/// `#[non_exhaustive]`: a future source — a dog fetched from a registry,
/// say — must not need a protocol version bump (IR-20).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DogSource {
    /// An argv branch of the shep binary itself (`shep dog <name>`).
    BuiltIn,
    /// A binary an operator adopted, run at the daemon's own trust level.
    Adopted {
        /// The binary's path, exactly as the operator gave it to `adopt`.
        path: String,
    },
}

pub struct ProcessInfo {
    // ... existing fields unchanged ...
    /// Set when this entry is a dog, naming where the dog came from;
    /// `None` for a sheep.
    pub dog: Option<DogSource>,
}
```

Cargo shape for this task: `-p shep-core`.

`path` is a `String`, not a `PathBuf`, for the reason `ProcessInfo::out_file`'s own comment gives at length: serde's `PathBuf` impl **refuses** a non-UTF-8 path, and that refusal aborts the whole `Reply` rather than one field.

**`None` deliberately means both "a sheep" and "a peer that predates this field", and that is correct rather than sloppy** (decision 18) — a daemon built before dogs existed has none, so "not a dog" is the true answer either way. `cpu_percent`'s doc has to enumerate three cases precisely because a zero would be a *claim*; there is no claim to get wrong here. Say this in the field's doc, so the next reader does not "fix" it.

- [ ] **Step 0: Record the baseline.** `cargo test --workspace --all-features` at `main`'s head, counting `Running`/`Doc-tests` lines against `test result:` lines. Paste both counts and the pass/ignored totals into the report; every later task's "N still pass" claim is measured against it.

- [ ] **Step 1: Write the failing tests.** In `request.rs`'s `mod tests`, give `sample_info()` a `dog: None` (it stands for a sheep, and `an_old_client_still_decodes_a_new_process_info` reads it), then add:

```rust
    /// fails if `DogSource` loses its `tag = "kind"` or its snake_case
    /// rename, and fails if `Adopted`'s `path` is renamed — any of the three
    /// changes one of these two strings while every type-level test in this
    /// module keeps passing. The marker is what the CLI splits two tables on
    /// and what the metrics dog reports a health gauge from, so a silent
    /// rename here is a silently empty dogs table.
    #[test]
    fn a_dog_source_serializes_snake_case_under_its_kind() {
        assert_eq!(
            serde_json::to_string(&DogSource::BuiltIn).unwrap(),
            r#"{"kind":"built_in"}"#
        );
        let adopted = DogSource::Adopted {
            path: "/usr/local/bin/shep-otel".to_string(),
        };
        let wire = r#"{"kind":"adopted","path":"/usr/local/bin/shep-otel"}"#;
        assert_eq!(serde_json::to_string(&adopted).unwrap(), wire);
        assert_eq!(serde_json::from_str::<DogSource>(wire).unwrap(), adopted);
    }

    /// fails if `dog` stops being optional. A daemon built before dogs
    /// sends a reply with no such key and still announces protocol 1, so a
    /// required field would make a current client unable to list against it
    /// at all — the same skew rule `out_file` and `cpu_percent` are pinned
    /// under, and the same committed-byte-fixture proof.
    #[test]
    fn v1_process_info_without_a_dog_marker_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log","cpu_percent":12.5,"memory_bytes":50331648}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.dog, None);
    }
```

and extend `reply_wire_snapshots` with one more reply, so the marker is pinned in its *set* state and not only as a `null` on `sample_info`:

```rust
            // `sample_info()` above pins the absent marker (a sheep's
            // `"dog": null`); this row is the only place the present one is
            // pinned, and `Adopted` rather than `BuiltIn` because it is the
            // variant carrying a payload — the unit variant's shape is
            // already proven by every fieldless variant on this wire.
            Reply {
                id: 6,
                result: Ok(Response::Flock(vec![ProcessInfo {
                    id: 7,
                    name: "otel".to_string(),
                    dog: Some(DogSource::Adopted {
                        path: "/usr/local/bin/shep-otel".to_string(),
                    }),
                    ..sample_info()
                }])),
            },
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-core --lib` — expected: `DogSource` does not exist and `ProcessInfo` has no `dog` field.

- [ ] **Step 3: Implement.** Add `DogSource` above `ProcessInfo`, and `dog` as `ProcessInfo`'s **last** field so the serialized key order stays append-only. `ProcessInfo` keeps `PartialEq` and still derives no `Eq`. `PROTOCOL_VERSION` stays **1**.

Then fix every construction site the compiler names. There are more than the obvious ones — `crates/shep-daemon/src/supervisor.rs`'s `to_info`, its test fixtures, `crates/shep-cli/src/output/table.rs`'s `info_with_name`, `crates/shep-client/src/testing.rs`'s `sample_info`, and `crates/shep-core/src/protocol/events.rs`'s two inline `ProcessInfo` literals. `cargo check --workspace --all-targets --all-features` finds them all at once; do that once rather than chasing them per crate.

- [ ] **Step 4: Regenerate the snapshots and read the delta.**

```
cargo insta test --accept -p shep-core     # or: INSTA_UPDATE=always cargo test -p shep-core --lib
git diff crates/shep-core/src/protocol/snapshots/
```

Expected: `"dog": null` added to every `ProcessInfo` object in **both** `reply_wire_v1.snap` and `bus_event_wire_v1.snap`, plus the one new `Flock` reply from step 1. `request_wire_v1.snap` must not move at all — it carries no `ProcessInfo`. **Paste the diff verbatim into the task report.** Any other line changing means something else moved.

- [ ] **Step 5: `FlockRows::JSON_ONLY` and `FlushedRows::JSON_ONLY`** (`crates/shep-cli/src/output/rows.rs`) each gain `"dog"`, with its own inline reason — the sheep table has no `SOURCE` column, because in that table every row is a sheep. `assert_no_drift` fails until they do, and that failure is the anti-drift gate working, not an obstacle.

- [ ] **Step 6: CHANGELOG** — shep-core: `ProcessInfo::dog` and `DogSource` added, additive, `PROTOCOL_VERSION` unchanged; what `None` means and why it is not three cases.

- [ ] **Step 7: Task gate, then commit** — `feat(core): mark which entries are dogs`

---

## Task 2: the dog verbs on the wire

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs`
- Modify: `crates/shep-core/src/protocol/snapshots/…request_wire_v1.snap`, `…reply_wire_v1.snap`
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces — produced, and depended on by Tasks 6, 10, 11, 12:**

```rust
pub enum Request {
    // ... existing variants unchanged ...
    /// Ask for one dog's `[dog.<name>]` section, as the dog itself parses it
    DogConfig {
        /// The dog's name — the config key, not a selector
        name: String,
    },
    /// Start one dog now, marking it as coming from `source`
    EnableDog {
        /// The dog's name
        name: String,
        /// Where its binary comes from
        source: DogSource,
    },
    /// Stop and deregister one dog
    DisableDog {
        /// The dog's name
        name: String,
    },
}

pub enum Response {
    // ... existing variants unchanged ...
    /// Answer to `DogConfig` — the dog's own section, rendered back to TOML
    DogSection {
        /// The `[dog.<name>]` table as TOML text, empty when the file has
        /// no such section
        toml: String,
    },
    /// Answer to `EnableDog` — the dog as it stands now
    DogStarted(ProcessInfo),
}
```

Cargo shape for this task: `-p shep-core`.

**`DisableDog` answers `Response::Deleted(Vec<u32>)`**, which already exists and already means "ids removed". Disabling deregisters exactly as `Delete` does, so this is the same fact and not a coincidence of shape; a variant of its own would carry nothing `Deleted` does not. Say so in `DisableDog`'s own doc, so a reader is not left hunting for `DogDisabled`.

**`DogSection`, not `DogConfig`, on the reply side.** The request is a verb and the reply is the thing itself, the way `ListFlock` answers `Flock`. Two identically-tagged variants in different enums would also make a wire dump ambiguous to a human reading it.

**`name` is a `String`, not a `SelectorSpec`.** A dog's name is a config key: it selects a `[dog.<name>]` table and an `enabled_dogs` entry, not a set of processes. A selector here would invite `shep enable all`, which has no meaning.

- [ ] **Step 1: Write the failing tests.** Extend `request_wire_snapshots` with the three envelopes and `reply_wire_snapshots` with the two replies:

```rust
            // The three dog verbs together, in the order an operator meets
            // them: ask for a section, start a dog, stop one. Adjacent on
            // purpose — `enable_dog` and `disable_dog` differ by their
            // `kind` and by `source`, so a `DisableDog` accidentally given
            // `EnableDog`'s tag shows up here as two near-identical objects
            // rather than as a diff a reader has to compare field by field.
            Envelope {
                id: 11,
                deadline_ms: None,
                body: Request::DogConfig {
                    name: "bark".to_string(),
                },
            },
            Envelope {
                id: 12,
                deadline_ms: None,
                body: Request::EnableDog {
                    name: "metrics".to_string(),
                    source: DogSource::BuiltIn,
                },
            },
            Envelope {
                id: 13,
                deadline_ms: None,
                body: Request::DisableDog {
                    name: "metrics".to_string(),
                },
            },
```

```rust
            // The opaque blob, pinned as a blob: the daemon renders a TOML
            // table into a string and never a typed structure, so what this
            // row proves is that the section crosses the wire as text.
            Reply {
                id: 7,
                result: Ok(Response::DogSection {
                    toml: "port = 9615\n".to_string(),
                }),
            },
            // The only `Response` variant carrying a BARE `ProcessInfo`
            // rather than a `Vec` of them: `enable` starts exactly one dog,
            // and a one-element list would invite a reader to wonder when it
            // holds two.
            Reply {
                id: 8,
                result: Ok(Response::DogStarted(ProcessInfo {
                    id: 4,
                    name: "metrics".to_string(),
                    dog: Some(DogSource::BuiltIn),
                    ..sample_info()
                })),
            },
```

plus the tag test:

```rust
    /// fails if any of the three verbs or either reply is given a `rename`,
    /// or if `Response`'s `content = "data"` is dropped. `disable_dog`'s
    /// answer is `Deleted`, which no other test in this module pairs with
    /// this verb — a handler wired to answer `Deleted` for `EnableDog` would
    /// still round-trip, and this is where the pairing is written down.
    #[test]
    fn the_dog_verbs_serialize_snake_case_with_their_payloads_under_data() {
        assert_eq!(
            serde_json::to_string(&Request::DogConfig {
                name: "bark".to_string()
            })
            .unwrap(),
            r#"{"kind":"dog_config","name":"bark"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::DisableDog {
                name: "bark".to_string()
            })
            .unwrap(),
            r#"{"kind":"disable_dog","name":"bark"}"#
        );
        let section = Response::DogSection {
            toml: "port = 9615\n".to_string(),
        };
        let wire = r#"{"kind":"dog_section","data":{"toml":"port = 9615\n"}}"#;
        assert_eq!(serde_json::to_string(&section).unwrap(), wire);
        assert_eq!(serde_json::from_str::<Response>(wire).unwrap(), section);
    }
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-core --lib`.

- [ ] **Step 3: Implement.** The three `Request` variants go after `Muster` and before `KillDaemon`; the two `Response` variants after `Mustered` and before `Subscribed`. Every struct-variant field needs a doc comment (`missing_docs` is `deny`, and that includes them). Both enums are already `#[non_exhaustive]`; `PROTOCOL_VERSION` stays **1**.

- [ ] **Step 4: Regenerate the snapshots and read the delta.** Expected: exactly three new objects in `request_wire_v1.snap` and two in `reply_wire_v1.snap`, and `bus_event_wire_v1.snap` untouched. **Paste the diff verbatim.**

- [ ] **Step 5: CHANGELOG** — shep-core: three request verbs and two response variants, additive; `DisableDog` answers the existing `Deleted`.

- [ ] **Step 6: Task gate, then commit** — `feat(core): put the dog verbs on the wire`

---

## Task 3: where an adopted dog's binary is recorded

**Files:**
- Modify: `crates/shep-core/src/config/daemon.rs`
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 7, 10, 11:**

```rust
pub struct DaemonSection {
    // ... existing fields unchanged ...
    /// Dogs to autostart with the daemon (`shep enable` writes this)
    pub enabled_dogs: Vec<String>,
    /// Where an adopted dog's binary lives, keyed by dog name
    /// (`shep adopt` writes this; `shep rehome` removes it).
    ///
    /// A name in [`Self::enabled_dogs`] with no entry here is a built-in
    /// dog — an argv branch of the shep binary itself. That is the whole of
    /// the distinction, and it is deliberately NOT recorded inside
    /// `[dog.<name>]`: that table is the dog's own opaque configuration, and
    /// a shep-owned key inside it would collide with a third-party dog's
    /// schema.
    pub adopted_dogs: BTreeMap<String, PathBuf>,
}
```

Cargo shape for this task: `-p shep-core`.

`PathBuf`, not `String`: this one never crosses the wire as part of a config load — `DogSource::Adopted` is what travels, and it carries the string. Here the value is read straight off a TOML file and handed to a spawn, which is a path.

- [ ] **Step 1: Write the failing tests.** In `daemon.rs`'s `mod tests`:

```rust
    /// fails if `adopted_dogs` is not `default`ed, or is declared outside
    /// `deny_unknown_fields`'s reach: a `shep.toml` written before it
    /// existed must still load, and a typo'd key must still be refused.
    /// Both halves matter — dropping `default` breaks every existing file,
    /// and the table is the one place an operator names a binary shep is
    /// about to run at the daemon's own trust level.
    #[test]
    fn adopted_dogs_default_empty_and_round_trip_by_name() {
        let bare = DaemonConfig::load(Some("[daemon]\nlog_json = true\n"), &no_env).unwrap();
        assert!(bare.daemon.adopted_dogs.is_empty());

        let src = r#"
[daemon]
enabled_dogs = ["metrics", "otel"]

[daemon.adopted_dogs]
otel = "/usr/local/bin/shep-otel"
"#;
        let cfg = DaemonConfig::load(Some(src), &no_env).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["metrics", "otel"]);
        assert_eq!(
            cfg.daemon.adopted_dogs.get("otel"),
            Some(&std::path::PathBuf::from("/usr/local/bin/shep-otel"))
        );
        assert!(
            !cfg.daemon.adopted_dogs.contains_key("metrics"),
            "a name with no entry here is a built-in, and that is the whole distinction"
        );
    }
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-core --lib`.

- [ ] **Step 3: Implement.** Add the field after `enabled_dogs`, with `use std::path::PathBuf` at the top of the module.

- [ ] **Step 4: Fix the exact-string `Debug` test.** `daemon.rs`'s existing assertion (search for `DaemonConfig { daemon: DaemonSection {`) pins `DaemonSection`'s whole rendering and **must** change. It is an IR-41 exact-string test, so changing it is the deliberate act it is designed to force — update the expected literal to include `adopted_dogs: {}` in field order, and do **not** relax the assertion to a `contains`. The redaction rule is unchanged: paths are not secrets, and the `dog` tables are still summarised as `<N tables>`.

- [ ] **Step 5: CHANGELOG** — shep-core: `[daemon] adopted_dogs`, what it distinguishes, and why it is not a key inside `[dog.<name>]`.

- [ ] **Step 6: Task gate, then commit** — `feat(core): record where an adopted dog's binary lives`

---

## Task 4: a wildcard selector never names a dog

**Files:**
- Modify: `crates/shep-core/src/selector.rs`
- Modify: `crates/shep-daemon/src/supervisor.rs`
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 5, 6:**

```rust
// shep-core/src/selector.rs
impl ProcessSelector {
    /// Whether this selector names ONE entry the caller already knew of, by
    /// its name or its id, rather than sweeping whatever matches.
    ///
    /// The distinction a dog turns on: a dog is a process an operator
    /// installed, not a member of the flock `all` means, so a wildcard must
    /// pass it by while `shep restart metrics` still reaches it. `Regex` and
    /// `Fold` are wildcards here even when they happen to match one entry —
    /// what matters is that the operator did not name it.
    #[must_use]
    pub const fn is_exact(&self) -> bool;
}

// shep-daemon/src/supervisor.rs, on Actor
    /// Every registered id `selector` names, in id order.
    ///
    /// The one place selection happens. A dog is included only for a
    /// selector that named it (`ProcessSelector::is_exact`), so `stop all`,
    /// `reload all`, `delete all` and a `/regex/` sweep pass every dog by
    /// while `shep restart bark` still reaches one.
    fn matching_ids(&self, selector: &ProcessSelector) -> Vec<u32>;
```

Cargo shape for this task: `--workspace` (it edits two crates and the daemon's tests are what prove the behaviour). State it and do not switch to `-p` partway.

**This task lands before the marker exists**, so `matching_ids` is written against `slot.entry.dog` only once Task 5 has added it. Sequence the two the other way if the implementer prefers — but then Task 5 owns both halves and this task shrinks to `is_exact` alone. Either order is fine; **say in the report which one was taken**, because Task 6's brief assumes `matching_ids` exists.

The five sites this replaces are `begin_manual`, `handle_reload`, `handle_reopen`, `handle_flush`, and `begin_action` — each currently spells the same `self.sheep.iter().filter_map(...)` by hand. `handle_reopen` and `handle_flush` need the slots as well as the ids; they call `matching_ids` and then look each id up, which is what they already effectively do.

- [ ] **Step 1: Write the failing tests.** In `selector.rs`:

```rust
    /// fails if `Fold` or `Regex` is counted as exact. Either mistake makes
    /// `shep reload /^web/` sweep up a dog, which is the failure the split
    /// exists to prevent — and it is invisible until a flock happens to run
    /// a dog whose name the pattern matches.
    #[test]
    fn only_a_name_or_an_id_names_one_entry_the_caller_knew_of() {
        assert!(ProcessSelector::Name("bark".into()).is_exact());
        assert!(ProcessSelector::Id(4).is_exact());
        assert!(!ProcessSelector::All.is_exact());
        assert!(!ProcessSelector::Fold("api".into()).is_exact());
        // Built through the real constructor: a `Regex` is a wildcard even
        // when its pattern is a literal that can only ever match one name.
        assert!(!ProcessSelector::regex("^bark$").unwrap().is_exact());
    }
```

(Use whatever constructor `selector.rs` actually exposes for the regex variant — read the file; it compiles the pattern and returns a `Result`.)

In `supervisor.rs`'s `mod tests`, using the existing `actor_with_one_online_sheep` fixture extended with one dog entry:

```rust
    /// fails if a wildcard reaches a dog. Both halves are load-bearing and
    /// neither implies the other: without the first assertion a helper that
    /// excluded dogs from EVERYTHING passes, and `shep disable bark` — which
    /// stops the dog by naming it — would silently match nothing.
    #[test]
    fn a_wildcard_passes_a_dog_by_and_its_own_name_still_reaches_it() {
        let actor = actor_with_a_sheep_and_a_dog();
        assert_eq!(
            actor.matching_ids(&ProcessSelector::All),
            vec![SHEEP_ID],
            "`all` is the flock, not the kennel"
        );
        assert_eq!(
            actor.matching_ids(&ProcessSelector::Name("bark".into())),
            vec![DOG_ID]
        );
        assert_eq!(actor.matching_ids(&ProcessSelector::Id(DOG_ID)), vec![DOG_ID]);
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** `is_exact` as a `const fn` match. `matching_ids`:

```rust
    fn matching_ids(&self, selector: &ProcessSelector) -> Vec<u32> {
        let exact = selector.is_exact();
        let mut ids: Vec<u32> = self
            .sheep
            .iter()
            .filter(|(_, slot)| exact || slot.entry.dog.is_none())
            .filter_map(|(id, slot)| {
                let config = slot.entry.spec.config();
                selector
                    .matches(&config.name, *id, config.fold.as_deref())
                    .then_some(*id)
            })
            .collect();
        ids.sort_unstable();
        ids
    }
```

Then rewrite the five sites to call it. Three of them already sorted afterwards and can drop their own sort; `begin_manual` did not sort at all, so this makes its order deterministic — a strict improvement, and worth one line in the report because it changes the order stop/restart events are emitted in for a multi-match selector.

- [ ] **Step 4: Prove the collapse is complete.** `grep -n "selector.matches\|\.matches(&config.name" crates/shep-daemon/src/supervisor.rs` must find exactly one line, inside `matching_ids`. Paste the output into the report. The `crates/shep-daemon/src/rpc.rs` call inside the `Describe` arm is a *different* filter, over `ProcessInfo` rather than slots, and Task 6 handles it.

- [ ] **Step 5: Run the inner loop**, confirm the 333 baseline plus the new cases.

- [ ] **Step 6: CHANGELOG** — shep-core: `ProcessSelector::is_exact`. shep-daemon: a wildcard selector no longer reaches a dog.

- [ ] **Step 7: Task gate, then commit** — `feat: keep dogs out of what a wildcard selector sweeps`

---

## Task 5: the marker on the entry, and starting a dog

**Files:**
- Modify: `crates/shep-daemon/src/entry.rs`, `crates/shep-daemon/src/supervisor.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 4, 6, 7:**

```rust
// entry.rs
pub struct ProcessEntry {
    // ... existing fields unchanged ...
    /// Set when this entry is a dog, naming where the dog came from.
    ///
    /// A MARKER, and deliberately not a second registry: reload, watch,
    /// cron, the memory ceiling, the log plane and the muster roll all
    /// supervise a dog exactly as they supervise a sheep, and a field is
    /// what keeps that true. It is read where the question is *where did
    /// this come from* (a listing's `SOURCE`) or *who should see this* (a
    /// wildcard selector, a flock table). It is never read to decide how a
    /// process is supervised — a different kill ladder, backoff curve or
    /// restart budget keyed on this field is the signal that the separate
    /// registry should have been built instead.
    pub dog: Option<DogSource>,
}

// supervisor.rs
impl SupervisorHandle {
    /// Registers and starts one dog, marked as coming from `source`.
    ///
    /// Idempotent by name: a dog already registered under `app`'s name is
    /// reported as it stands rather than started twice, which is what makes
    /// `shep enable` safe to run against a daemon that already has the dog.
    ///
    /// # Errors
    /// - [`SupervisorError::EngineStopped`] — shutdown has begun.
    /// - [`SupervisorError::SpawnFailed`] — the binary could not be spawned.
    pub async fn start_dog(
        &self,
        app: ResolvedApp,
        source: DogSource,
    ) -> Result<ProcessInfo, SupervisorError>;
}
```

Cargo shape for this task: `-p shep-daemon`.

**`spawn_fresh` takes the marker as a parameter and writes it onto the entry; `respawn` reads it off the slot it is respawning and never re-derives it.** That is the entire mechanism, and it is why a restart, a memory-limit respawn, a cron occurrence and a watch-triggered restart all keep the marker without any of them knowing dogs exist.

`do_start` grows the same parameter and passes it through. **Do not add a `dog` field to `AppConfig` or `ResolvedApp`** — that is user config read from a Flockfile, and a Flockfile must not be able to declare a dog.

- [ ] **Step 1: Write the failing tests.** In `supervisor.rs`'s `mod tests`:

```rust
    /// fails if `start_dog` marks the entry and the marker is then lost on
    /// respawn — which is the shape a marker written by the START path
    /// rather than carried by the ENTRY takes, and it is invisible until a
    /// dog crashes once: the dog vanishes from the dogs table and reappears
    /// among the sheep, with no error anywhere.
    #[tokio::test]
    async fn a_dog_that_restarts_is_still_a_dog() {
        let h = harness(vec![
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
        ]);
        let dog = h
            .ctx
            .supervisor
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();
        assert_eq!(dog.dog, Some(DogSource::BuiltIn));

        // The scripted exit, then the automatic respawn it earns.
        let after = await_status(&h, dog.id, ProcStatus::Online).await;
        assert_eq!(after.restarts, 1, "the ordinary restart path, not a dog one");
        assert_eq!(after.dog, Some(DogSource::BuiltIn));
    }

    /// fails if `start_dog` is not idempotent by name. `shep enable` runs
    /// against a daemon that may already have the dog — from `enabled_dogs`
    /// at boot — and a second live process under one name would give the
    /// dog two connections, two metrics listeners on one port, and two
    /// copies of every bark.
    #[tokio::test]
    async fn enabling_a_dog_twice_starts_one_process() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let first = h
            .ctx
            .supervisor
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();
        let second = h
            .ctx
            .supervisor
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.pid, first.pid, "the same process, not a fresh one");
        let listed = h.ctx.supervisor.list().await;
        assert_eq!(listed.iter().filter(|i| i.name == "bark").count(), 1);
    }
```

`dog_app(name)` is a test helper in this module: `normalize(AppConfig::minimal(name, "/nonexistent/shep")).unwrap()`. The `ScriptedRunner` never execs, so the path is a label. `await_status` is a bounded poll (`tokio::time::timeout` around a `list()` loop) — **not** a bare wait; write it if the module has no equivalent, and give it a 5s ceiling.

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-daemon --lib --all-features -- --skip watch:: --skip extras::`

- [ ] **Step 3: Implement.**
  - `ProcessEntry::dog` as the last field; every construction site in `supervisor.rs` and `testing.rs` gains it.
  - `spawn_fresh(&mut self, app, instance, credentials, dog: Option<DogSource>)`, writing `dog` into **both** the success and the failure entry — a dog whose binary does not exist must still show up in the dogs table as `errored`, which is exactly the case `adopt` with a bad path produces.
  - `respawn` clones `slot.entry.dog` alongside `slot.entry.credentials`. There is no branch: the two fields are carried the same way for the same reason.
  - `do_start(&mut self, apps, dog: Option<DogSource>)`; the existing `Command::Start` arm passes `None`.
  - `Command::StartDog { app, source, reply }`, handled by rejecting when `self.shutting_down` (the same `EngineStopped` rule `Start` follows, for the same reason: a child spawned after the shutdown aggregation is computed is a child nothing will kill), then looking for an existing entry with that name and returning `to_info` of it if found, else `do_start(vec![app], Some(source))` and taking the single result.

- [ ] **Step 4: Prove the marker never reaches supervision.** Run and paste:

```
grep -n "dog" crates/shep-daemon/src/supervisor.rs
grep -rn "dog" crates/shep-daemon/src/kill.rs crates/shep-daemon/src/backoff.rs crates/shep-daemon/src/runner.rs crates/shep-daemon/src/tokio_runner.rs
```

The second command must find **nothing**. The first must find only: the `Command::StartDog` variant and its arm, `start_dog`, `spawn_fresh`/`do_start`/`respawn`'s parameter and field, `to_info`'s read, `matching_ids`'s filter, and test code. Anything else is decision 2's warning firing and stops the task.

- [ ] **Step 5: Mutation check.** Delete the `dog` clone from `respawn` and watch `a_dog_that_restarts_is_still_a_dog` redden; restore from a `cp` snapshot. Paste the failing assertion into the report.

- [ ] **Step 6: CHANGELOG** — shep-daemon: the dog marker, and `start_dog`'s idempotence.

- [ ] **Step 7: Task gate, then commit** — `feat(daemon): start a dog through the ordinary spawn path`

---

## Task 6: the dog's own spec, and the three RPC arms

**Files:**
- Create: `crates/shep-daemon/src/dogs.rs`
- Modify: `crates/shep-daemon/src/lib.rs`, `crates/shep-daemon/src/rpc.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 7, 10, 11, 12:**

```rust
// dogs.rs
/// One dog the daemon knows about: its name, and where its binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogSpec {
    /// The dog's name — the `[dog.<name>]` key and the entry's name.
    pub name: String,
    /// Where its binary comes from.
    pub source: DogSource,
}

/// The app config the daemon spawns `spec` from.
///
/// A built-in dog is `<this binary> dog <name>`; an adopted one is the
/// operator's binary with no arguments. Either way the child's environment
/// carries exactly one thing it did not already need in order to exec:
/// `SHEP_HOME`, which is how every client locates the socket. No
/// `[dog.<name>]` value is ever placed here — a dog asks for its section
/// over the socket (`Request::DogConfig`), because the environment is
/// readable from the process table, inherited by every child, and captured
/// into crash dumps.
///
/// `autorestart` and the restart budget are left at their defaults: a dog
/// is supervised exactly as a sheep is.
///
/// # Errors
/// - [`DogError::NoBinary`] — [`std::env::current_exe`] failed, so a
///   built-in dog has no program to run.
/// - [`DogError::Config`] — the assembled config failed `normalize`.
pub fn dog_app(spec: &DogSpec, paths: &ShepPaths) -> Result<ResolvedApp, DogError>;

/// The `[dog.<name>]` section of `path`, rendered back to TOML text.
///
/// Reads the file on every call rather than serving a copy cached at boot:
/// one reader can never be stale, and it is what makes
/// `shep disable X && shep enable X` re-read an edited section.
///
/// A missing file, or a file with no such section, is `Ok(String::new())` —
/// a dog with no configuration is the ordinary case, not a fault.
///
/// # Errors
/// - [`DogError::Config`] — the file exists and is not valid `shep.toml`.
/// - [`DogError::Io`] — the file exists and could not be read.
pub fn dog_section(path: &Path, name: &str) -> Result<String, DogError>;
```

Cargo shape for this task: `-p shep-daemon`.

`RpcContext` gains one field, `daemon_config: PathBuf`, filled from `paths.daemon_config` in `boot` and in `testing::harness_with_extras`. It sits beside `snapshot_path`, which is the same kind of thing for the same reason.

The three arms:

```rust
        Request::DogConfig { name } => match crate::dogs::dog_section(&ctx.daemon_config, &name) {
            Ok(toml) => reply(Ok(Response::DogSection { toml })),
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
            })),
        },
        Request::EnableDog { name, source } => {
            let spec = DogSpec { name, source };
            match crate::dogs::dog_app(&spec, &ctx.paths) {
                Err(err) => reply(Err(RpcError {
                    code: RpcErrorCode::InvalidConfig,
                    message: err.to_string(),
                })),
                Ok(app) => match ctx.supervisor.start_dog(app, spec.source).await {
                    Ok(info) => reply(Ok(Response::DogStarted(info))),
                    Err(err) => reply(Err(rpc_error(&err))),
                },
            }
        }
        Request::DisableDog { name } => {
            match ctx.supervisor.delete(ProcessSelector::Name(name)).await {
                Ok(ids) => reply(Ok(Response::Deleted(ids))),
                Err(err) => reply(Err(rpc_error(&err))),
            }
        }
```

**`DisableDog` goes through `delete` with an exact `Name` selector**, which is exactly why Task 4's rule kept an exact selector able to reach a dog. It reuses the whole stop-then-deregister path — kill ladder, graceful timeout, deregistration — rather than opening a second way to end a supervised process.

`RpcContext` needs `paths: ShepPaths` for `dog_app` (log paths and `SHEP_HOME`). It currently carries `snapshot_path` alone; **replace neither** — add `paths` and leave `snapshot_path` as it stands, because five call sites read it and widening this task to a field rename is scope the phase does not need. Say in the report that the redundancy is deliberate and name it as a follow-up.

- [ ] **Step 1: Write the failing tests.** In `dogs.rs`:

```rust
    /// fails if a `[dog.<name>]` value is folded into the child's
    /// environment. That is the design's whole reason for putting config on
    /// the socket: a webhook URL in the environment is readable from the
    /// process table on some systems, inherited by every child the dog
    /// spawns, and captured into crash dumps. The assertion is over the
    /// ASSEMBLED spec, not the config, because `assemble` is where an env
    /// map would actually be merged.
    #[test]
    fn a_dogs_child_environment_carries_shep_home_and_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::write(
            &paths.daemon_config,
            "[dog.bark]\nwebhook = \"https://example.invalid/hook\"\n",
        )
        .unwrap();
        let spec = DogSpec {
            name: "bark".to_string(),
            source: DogSource::BuiltIn,
        };
        let app = dog_app(&spec, &paths).unwrap();
        let assembled = crate::assemble::assemble(&app, 0, &paths, None);
        assert_eq!(
            assembled.env.get("SHEP_HOME"),
            Some(&paths.home.display().to_string())
        );
        assert!(
            !assembled.env.values().any(|v| v.contains("example.invalid")),
            "a dog's configuration never travels in its environment: {:?}",
            assembled.env
        );
    }

    /// fails if a built-in dog is spawned as anything but this binary's own
    /// hidden `dog <name>` branch, and fails if an adopted one is given
    /// arguments it never asked for — which would make every third-party
    /// dog see an argv shep invented for it.
    #[test]
    fn a_built_in_dog_runs_this_binary_and_an_adopted_one_runs_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        let built_in = dog_app(
            &DogSpec {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
            },
            &paths,
        )
        .unwrap();
        assert_eq!(
            built_in.config().script,
            std::env::current_exe().unwrap().display().to_string()
        );
        assert_eq!(built_in.config().args, vec!["dog", "metrics"]);

        let adopted = dog_app(
            &DogSpec {
                name: "otel".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            },
            &paths,
        )
        .unwrap();
        assert_eq!(adopted.config().script, "/usr/local/bin/shep-otel");
        assert!(adopted.config().args.is_empty());
        assert_eq!(adopted.config().name, "otel", "the NAME is the config key, never the filename");
    }

    /// fails if `dog_section` returns the whole file, or a typed structure,
    /// or fails on a file with no such section. The blob is what a
    /// third-party dog parses; handing it a table it did not ask for is the
    /// same bug as handing it nothing.
    #[test]
    fn a_dogs_section_comes_back_as_its_own_table_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nlog_json = true\n\n[dog.bark]\ndebounce = \"30s\"\n\n[dog.metrics]\nport = 9615\n",
        )
        .unwrap();

        let bark = dog_section(&path, "bark").unwrap();
        assert!(bark.contains("debounce"));
        assert!(!bark.contains("9615"), "one dog never sees another's config");
        assert!(!bark.contains("log_json"), "nor the daemon's own");
        // Round-trips as TOML, since that is the contract the dog parses under.
        let parsed: toml::Table = toml::from_str(&bark).unwrap();
        assert_eq!(parsed["debounce"].as_str(), Some("30s"));

        assert_eq!(dog_section(&path, "absent").unwrap(), "");
        assert_eq!(dog_section(&dir.path().join("gone.toml"), "bark").unwrap(), "");
    }
```

In `rpc.rs`'s `mod tests`:

```rust
    /// fails if `DisableDog` is wired to anything but a real deregistration
    /// — a handler that answered `Deleted(vec![])` without stopping
    /// anything passes every type-level test and leaves the dog running
    /// after `shep disable` reported success.
    #[tokio::test]
    async fn disabling_a_dog_stops_it_and_takes_it_off_the_listing() {
        let h = harness(vec![ProcScript::never_exits()]);
        let started = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::EnableDog {
                        name: "bark".to_string(),
                        source: DogSource::BuiltIn,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::DogStarted(info)) = started.result else {
            panic!("expected DogStarted, got {:?}", started.result)
        };
        assert_eq!(info.dog, Some(DogSource::BuiltIn));

        let disabled = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::DisableDog {
                        name: "bark".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(disabled.result.unwrap(), Response::Deleted(vec![info.id]));
        assert!(h.ctx.supervisor.list().await.is_empty());
    }

    /// fails if the daemon serves a section it cached at boot. The file is
    /// written AFTER the harness built its context, so a cached reader
    /// answers the empty string here — which is exactly the bug that would
    /// make `shep disable X && shep enable X` fail to pick up an edit.
    #[tokio::test]
    async fn a_dog_config_request_reads_the_file_as_it_stands_now() {
        let h = harness(vec![]);
        std::fs::write(&h.ctx.daemon_config, "[dog.bark]\ndebounce = \"30s\"\n").unwrap();
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::DogConfig {
                        name: "bark".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::DogSection { toml }) = reply.result else {
            panic!("expected DogSection, got {:?}", reply.result)
        };
        assert!(toml.contains("30s"));
    }
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-daemon --lib --all-features -- --skip watch:: --skip extras::`

- [ ] **Step 3: Implement `dogs.rs`.** `dog_section` reads the file (`NotFound` → `Ok(String::new())`), loads it through `DaemonConfig::load(Some(&src), &|_| None)` — the existing loader, so a broken `shep.toml` becomes one named error rather than a second parser — then `cfg.dog.get(name)` and `toml::to_string(table)`. Pass **no** environment closure: `SHEP_*` overrides govern the daemon's own knobs and have nothing to say about a dog's section.

`DogError` is a per-module error enum (IR-18) with `Display`, `core::error::Error`, and a `source()`. Its `Debug` needs no redaction: it carries a path and a parser message, never a config value — state that in a comment, because the type sits next to code that handles webhook URLs.

- [ ] **Step 4: Wire the three arms** in `rpc.rs`, add `RpcContext::daemon_config` and `RpcContext::paths`, and fill both in `boot` and in `testing.rs`.

- [ ] **Step 5: Fix the `Describe` filter.** `rpc.rs`'s `Describe` arm filters `ProcessInfo`s by hand and must follow Task 4's rule, or `shep describe all` lists dogs the flock table then has nowhere to put:

```rust
                    let exact = selector.is_exact();
                    let hits: Vec<_> = infos
                        .into_iter()
                        .filter(|i| exact || i.dog.is_none())
                        .filter(|i| selector.matches(&i.name, i.id, i.fold.as_deref()))
                        .collect();
```

`ListFlock` is deliberately **not** filtered: it is the single registry both tables are rendered from (decision 13).

- [ ] **Step 6: Mutation check.** Change `dog_section` to return the whole file's text and watch `a_dogs_section_comes_back_as_its_own_table_and_nothing_else` redden on the `9615` assertion — the one that proves one dog cannot read another's config. Restore from a `cp` snapshot. Paste the failure.

- [ ] **Step 7: CHANGELOG** — shep-daemon: the three dog verbs answered; a dog's config travels over the socket and is re-read per request.

- [ ] **Step 8: Task gate, then commit** — `feat(daemon): answer the dog contract`

---

## Task 7: enabled dogs come up with the daemon

**Files:**
- Modify: `crates/shep-daemon/src/boot.rs`, `crates/shep-daemon/src/dogs.rs`
- Modify: `crates/shep-cli/src/commands/daemon.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`, `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Task 22:**

```rust
// boot.rs
pub struct BootOptions {
    // ... existing fields unchanged ...
    /// Dogs to start once the flock is back, in the order given.
    ///
    /// Assembled by the caller from `[daemon] enabled_dogs` and
    /// `[daemon] adopted_dogs`, so shep-daemon never reads `shep.toml`
    /// itself — the same division `socket` and `max_cron_sleep` already
    /// follow.
    pub dogs: Vec<DogSpec>,
}

// dogs.rs
/// Starts every dog in `specs`, warning and carrying on for each one that
/// will not start.
///
/// Never fails the boot. A dog that cannot be spawned is a monitoring gap,
/// and refusing to bring the flock up over it would turn that gap into an
/// outage — the one trade this whole subsystem is built to avoid.
pub async fn spawn_enabled_dogs(
    specs: &[DogSpec],
    paths: &ShepPaths,
    supervisor: &SupervisorHandle,
);
```

Cargo shape for this task: `--workspace` (it edits shep-daemon and shep-cli together). State it; do not switch to `-p` partway.

**Two seams to get right, both silent if got wrong.** `DogSpec` lives in `shep_daemon::dogs`, which must be a `pub mod` in `crates/shep-daemon/src/lib.rs` for shep-cli's `boot_options` to name it — shep-cli already depends on shep-daemon unconditionally. And `[daemon] adopted_dogs` holds a `PathBuf` while `DogSource::Adopted` holds a `String` (the wire refuses a non-UTF-8 `PathBuf` outright — Task 1); convert with `display().to_string()` at the one point of assembly, which is lossy exactly where the wire already is, and nowhere else.

**Where the step goes: after the muster restore, before the readiness notification.** Both halves matter. After the restore, because a metrics dog that came up first would answer for an empty flock during the restore window and a bark dog would raise `process.start` alerts for every restored sheep. Before the notification, because `Type=notify` going green is meant to mean the whole daemon is up — decision 16 of Phase 8 put the restore inside that promise, and monitoring belongs inside it for the same reason.

- [ ] **Step 1: Write the failing tests.** In `boot.rs`'s `mod tests`:

```rust
    /// fails if the dogs come up before the muster restore, or not at all.
    /// The order half is the point: a metrics dog that starts first answers
    /// for an empty flock for the whole restore window, and a bark dog
    /// raises a start alert for every sheep the roll brought back. The
    /// assertion reads the ORDER the scripted runner was asked to spawn in,
    /// which is the only place the sequence is observable.
    #[tokio::test]
    async fn boot_restores_the_flock_before_it_lets_the_dogs_out() {
        // ... write a roll with one app, boot with
        // `dogs: vec![DogSpec { name: "metrics".into(), source: BuiltIn }]`,
        // then assert the scripted runner's spawn log is [the sheep, the dog]
        // and that a listing carries both with the right markers.
    }

    /// fails if a dog that will not start takes the boot down with it. The
    /// dog is given a source whose binary cannot be spawned; the flock must
    /// still come up, and the daemon must still serve.
    #[tokio::test]
    async fn a_dog_that_will_not_start_does_not_fail_the_boot() {
        // ... boot with one unspawnable dog, assert `boot(..)` is `Ok`,
        // assert the entry is present and `Errored`, and assert
        // `capture_logs` holds a warning naming the dog.
    }
```

Write both bodies against the fixtures `boot.rs` already uses — `boot_restores_a_saved_flock_and_tears_down_in_order` is the nearest existing case and shows how a roll is planted and how the scripted runner's spawns are read. `capture_logs` (`testing.rs`) is what turns the warn-and-continue arm into something assertable.

In `commands/daemon.rs`'s `mod tests`:

```rust
    /// fails if `enabled_dogs` or `adopted_dogs` is dropped between
    /// `shep.toml` and `BootOptions` — the entire failure mode of a knob
    /// nobody plumbed, and the one this file has been warning about since
    /// the field was added with no reader. Both halves: a bare name is a
    /// built-in, and a name with a path is adopted.
    #[test]
    fn boot_options_carry_every_enabled_dog_with_the_source_the_file_names() {
        let src = r#"
[daemon]
enabled_dogs = ["metrics", "otel"]

[daemon.adopted_dogs]
otel = "/usr/local/bin/shep-otel"
"#;
        let config = DaemonConfig::load(Some(src), &|_| None).unwrap();
        let opts = boot_options(
            &config,
            &DaemonArgs { no_restore: false, foreground: false },
            None,
        );
        assert_eq!(
            opts.dogs,
            vec![
                DogSpec { name: "metrics".into(), source: DogSource::BuiltIn },
                DogSpec {
                    name: "otel".into(),
                    source: DogSource::Adopted {
                        path: "/usr/local/bin/shep-otel".into()
                    }
                },
            ]
        );
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** `BootOptions::dogs` (defaulting to empty at every existing construction site — `grep -rn "BootOptions {" crates/` finds them, including `daemon_e2e.rs` and `real_runner.rs`), the boot step, `spawn_enabled_dogs`, and `boot_options`'s assembly.

- [ ] **Step 4: Retire the inert-dog warning.** `dog_config_is_inert` and `warn_on_inert_dog_config` in `commands/daemon.rs`, their call in `run_daemon`, their test, and the CHANGELOG entry that announced them: all four go. The knob has a reader now, and a daemon that warns "this build has no dogs infrastructure yet" while starting two dogs is worse than silent. **Delete rather than adapt** — there is no residual "inert" state to warn about, since a `[dog.<name>]` section with no matching `enabled_dogs` entry is exactly how an operator stages config before enabling, and warning about it would fire on correct usage.

- [ ] **Step 5: Run the full lib suite** (`cargo test --workspace --all-features`) — this task changes a struct five test files construct.

- [ ] **Step 6: CHANGELOGs** — shep-daemon: enabled dogs start at boot, after the restore. shep-cli: `[daemon] enabled_dogs` and `[daemon] adopted_dogs` are read; the inert-dog warning is gone, reconciled into the entry that introduced it rather than appended beneath it (IR-45).

- [ ] **Step 7: Task gate, then commit** — `feat(daemon): let the enabled dogs out at boot`

---

## Task 8: listings sort by name

**Files:**
- Modify: `crates/shep-daemon/src/supervisor.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced:** none new. `Actor::snapshot_all` changes its output order.

Cargo shape for this task: `-p shep-daemon`.

Sorting by id scatters a clustered app's four instances across a table; sorting by name groups them, which is what makes a four-instance app readable at a glance. **Applied once, in `snapshot_all`**, because that is the single function every listing reply is built from — `ListFlock`, `Describe`, `Mustered`, and the roll's own `list_checked`. Sorting in the CLI instead would leave the metrics dog and bark reading a different order from the operator, and sorting in each verb would be four copies of one rule.

Sort key: `(name, instance, id)`. Name groups the app; instance keeps its slots in their own order; id breaks the tie a reload creates, where a replacement takes the drainee's slot number with a fresh id.

- [ ] **Step 1: Write the failing test.**

```rust
    /// fails if the listing comes back in id order. Built so id order and
    /// name order genuinely disagree — `web` is registered second and must
    /// still come first — because a fixture whose two orders coincide
    /// cannot tell the two implementations apart, and that is the shape of
    /// fixture this project has shipped before.
    #[tokio::test]
    async fn a_listing_groups_an_apps_instances_under_its_name() {
        let h = harness(vec![ProcScript::never_exits(); 4]);
        start_app(&h, AppConfig { instances: 2, ..AppConfig::minimal("zebra", "./z") }).await;
        start_app(&h, AppConfig { instances: 2, ..AppConfig::minimal("alpha", "./a") }).await;

        let listed = h.ctx.supervisor.list().await;
        let names: Vec<&str> = listed.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["alpha", "alpha", "zebra", "zebra"]);
        let ids: Vec<u32> = listed.iter().map(|i| i.id).collect();
        assert_ne!(
            ids,
            {
                let mut sorted = ids.clone();
                sorted.sort_unstable();
                sorted
            },
            "the fixture must make id order and name order disagree, or it proves nothing"
        );
    }
```

That second assertion is the guard against the "fixture that cannot distinguish right from wrong" shape. It fails loudly if a future edit makes the two orders coincide.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** One `sort_by` in `snapshot_all`, keyed on `(&entry.spec.config().name, entry.instance, entry.id)`.

- [ ] **Step 4: Find what the order change breaks, and fix it honestly.** Run the inner loop **and** `cargo test -p shep-daemon --lib --all-features -- --skip watch::` (the unfiltered form) — a test that indexed `listed[0]` on the old order fails here, and the fix is to key the assertion on the *name* rather than to re-sort the listing back. Every such change goes in the report by test name, so a reviewer can check none of them was an assertion weakened to pass.

- [ ] **Step 5: Check the e2e fixtures.** `crates/shep-cli/tests/fixtures/flock.json` and `describe.json` are compared structurally over the whole normalized value, so a reordered array is a failure. Re-run `cargo test -p shep-cli --test cli_e2e` and update the fixtures if their apps happen to be out of name order; say in the report whether any moved.

- [ ] **Step 6: CHANGELOG** — shep-daemon: every listing comes back grouped by name.

- [ ] **Step 7: Task gate, then commit** — `feat(daemon): group a listing by name so a clustered app reads as one`


---

## Task 9: two tables, one registry

**Files:**
- Modify: `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/src/commands/query.rs`
- Modify: `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 10, 11, 22:**

```rust
// output/rows.rs
/// The dogs half of a flock listing: the `ProcessInfo`s whose `dog` marker
/// is set, rendered by where they came from rather than by their place in
/// the flock.
///
/// No `ID` column, and that is the point of the split rather than an
/// omission: ids reflect spawn order across one registry, so a dog booted
/// alongside the flock lands among the sheep's numbers. Nobody sees that,
/// because the two populations are never rendered together — which is what
/// makes the shared id space cost nothing at the surface.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DogRows(pub Vec<ProcessInfo>);

// output/mod.rs
/// Renders one flock listing: the sheep table, then the dogs table beneath
/// it whenever any dog is registered.
///
/// `Format::Json` renders exactly what [`emit`] would for the whole
/// listing — one array, every entry, each carrying its own `dog` marker.
/// The machine surface keeps the single registry the two tables are a
/// rendering OF, so a consumer never has to reassemble one from two.
///
/// # Errors
/// The underlying write failed.
pub fn emit_flock(
    out: &mut dyn io::Write,
    fmt: Format,
    command: &str,
    listing: Vec<ProcessInfo>,
) -> io::Result<()>;

// cli.rs
pub enum Commands {
    /// List the dogs, and nothing else.
    Dogs,
}
```

Cargo shape for this task: `-p shep-cli`. `cargo test -p shep-cli --bins`, then `cargo test -p shep-cli --test cli_e2e`.

`DogRows`' headers are `NAME`, `SOURCE`, `STATUS`, `PID`, `RESTARTS`, `CPU`, `MEM`, `UPTIME` — the design's own sample, in its order. `json_key_for` maps `SOURCE` to `dog`; `JSON_ONLY` carries `id`, `fold`, `out_file` and `err_file`, each with its own inline reason (the anti-drift gate demands one per entry).

`SOURCE` renders as `built-in` or `adopted`; the adopted binary's path stays in the JSON. A path is routinely longer than every other column put together, which is the same reason `FlockRows` keeps `out_file` out of its table — and `shep dogs --format json` is one keystroke away for an operator who needs it.

**`render_table` is reused, not duplicated.** `emit_flock`'s table arm partitions the listing, calls `render_table::<FlockRows>` and then `render_table::<DogRows>`, separated by one blank line and the caption `Dogs`. Nothing about widths, padding, char-counting or the empty-payload header row is written twice; the two tables are independently sized, which is correct — they share no columns.

`shep dogs` calls the same function with the sheep half already dropped, so the dogs table has exactly one renderer.

**Decision 15 applies: no `--all` flag.** Neither `flock` nor `dogs` gains one in this task.

- [ ] **Step 1: Write the failing tests.** In `rows.rs`:

```rust
    /// fails if `SOURCE` renders the adopted binary's path into the table.
    /// A path is wider than every other column combined and would push
    /// UPTIME off a terminal — the same reason `FlockRows` keeps the log
    /// paths out of its own table, and the path is still one `--format
    /// json` away.
    #[test]
    fn the_source_column_names_a_kind_and_leaves_the_path_to_json() {
        let rows = DogRows(vec![
            dog_info("metrics", DogSource::BuiltIn),
            dog_info(
                "otel",
                DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            ),
        ]);
        let headers = DogRows::headers();
        let at = |cells: &[String], h: &str| {
            cells[headers.iter().position(|x| *x == h).unwrap()].clone()
        };
        assert_eq!(at(&rows.rows()[0], "SOURCE"), "built-in");
        assert_eq!(at(&rows.rows()[1], "SOURCE"), "adopted");

        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json[1]["dog"]["path"], "/usr/local/bin/shep-otel");
    }

    /// The anti-drift gate for this type. Fails if a `ProcessInfo` field is
    /// serialized with neither a column nor a `JSON_ONLY` entry.
    #[test]
    fn dog_rows_do_not_drift() {
        assert_no_drift::<DogRows>(&DogRows(vec![dog_info("metrics", DogSource::BuiltIn)]));
    }
```

(`dog_info` is a local helper over the module's existing `sample_info` with `dog` set. `assert_no_drift` already exists in this module — read its signature before calling it.)

In `output/mod.rs`:

```rust
    /// fails if the two populations are rendered into one table, or if the
    /// dogs table is hidden behind a flag. Both halves: the sheep table must
    /// not carry the dog's row, and the dogs table must appear with no flag
    /// at all — a bark dog that has died is precisely what an operator needs
    /// to notice, and hiding it means finding out by NOT being paged.
    #[test]
    fn a_flock_listing_prints_the_dogs_in_their_own_table() {
        let mut out = Vec::new();
        emit_flock(&mut out, Format::Table, "flock", mixed_listing()).unwrap();
        let text = String::from_utf8(out).unwrap();

        let (sheep_table, dogs_table) = text.split_once("\nDogs\n").expect("a Dogs caption");
        assert!(sheep_table.contains("web"));
        assert!(!sheep_table.contains("bark"), "a dog is not a sheep");
        assert!(dogs_table.contains("bark"));
        assert!(!dogs_table.contains("web"));
        assert!(
            !dogs_table.starts_with("ID"),
            "the dogs table has no ID column"
        );
    }

    /// fails if the JSON surface is split to match the tables. The machine
    /// surface IS the single registry — one array, every entry, each
    /// carrying its own marker — and a consumer that had to reassemble one
    /// from two would be paying for a rendering decision.
    #[test]
    fn the_json_surface_stays_one_array_of_every_entry() {
        let mut out = Vec::new();
        emit_flock(&mut out, Format::Json, "flock", mixed_listing()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["data"].as_array().unwrap().len(), 2);
        assert_eq!(json["data"][0]["dog"], serde_json::Value::Null);
        assert_eq!(json["data"][1]["dog"]["kind"], "built_in");
    }

    /// fails if a flock with no dogs prints an empty second table. An empty
    /// table still prints its header row (`render_table`'s own rule), so a
    /// caption and a bare header line would appear under every listing on
    /// every machine running no dogs at all.
    #[test]
    fn a_flock_with_no_dogs_prints_one_table_and_no_caption() {
        let mut out = Vec::new();
        emit_flock(&mut out, Format::Table, "flock", vec![sheep_info("web")]).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Dogs"));
    }
```

In `main.rs`:

```rust
    /// fails if `Commands::Dogs` is wired to another verb's function. The
    /// dispatch arms carried no unit coverage at all until recently, and a
    /// verb pointed at the wrong handler was invisible workspace-wide.
    #[test]
    fn dogs_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "dogs"]).unwrap().command,
            Commands::Dogs
        ));
    }
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-cli --bins`.

- [ ] **Step 3: Implement.** `DogRows` (added to `output/mod.rs`'s `pub use rows::{...}` list, which carries the `#[cfg_attr(windows, allow(unused_imports))]` this whole re-export already needs), `emit_flock`, the `Dogs` variant, the dispatch arm, and `query.rs`'s `flock`/`dogs` both routing through `emit_flock`. `flock`'s existing `request_and_render` helper renders through `emit`; give `flock` and `dogs` their own small path rather than widening that helper with a second renderer — it serves six verbs and none of the other five wants two tables.

- [ ] **Step 4: e2e.** `cli_e2e.rs`'s existing `flock` case must still pass unchanged (a flock with no dogs prints exactly what it printed before — that is what step 1's third test pins at the unit tier and what this confirms at the real-binary one). Say in the report whether any fixture moved.

- [ ] **Step 5: Mutation check.** Change `emit_flock`'s partition to put dogs in the sheep table and watch `a_flock_listing_prints_the_dogs_in_their_own_table` redden on the `!sheep_table.contains("bark")` assertion. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 6: CHANGELOG** — shep-cli: `shep flock` prints a dogs table beneath the flock; `shep dogs` prints it alone; `--format json` is unchanged in shape.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): print the dogs in their own table`

---

## Task 10: `shep enable` and `shep disable`

**Files:**
- Create: `crates/shep-cli/src/commands/dogs.rs`, `crates/shep-cli/src/commands/shep_toml.rs`
- Modify: `crates/shep-cli/src/commands/mod.rs`, `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`, `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/Cargo.toml`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Task 11:**

```rust
// commands/shep_toml.rs
/// The one writer of `$SHEP_HOME/shep.toml` in this binary.
///
/// Edits through `toml_edit`, so an operator's comments, key order and
/// formatting survive: this file is hand-written far more often than it is
/// generated, and a `shep enable` that reformatted it would be a reason not
/// to run `shep enable`. A missing file is created; a file that will not
/// parse is refused rather than overwritten.
#[derive(Debug)]
pub struct ShepToml {
    path: PathBuf,
    doc: toml_edit::DocumentMut,
}

impl ShepToml {
    /// Reads `path`, treating a missing file as an empty document.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] — the file exists and could not be read.
    /// - [`ShepTomlError::Parse`] — the file exists and is not valid TOML.
    pub fn open(path: &Path) -> Result<Self, ShepTomlError>;

    /// Adds `name` to `[daemon] enabled_dogs` (idempotently) and ensures a
    /// `[dog.<name>]` table exists for the dog to be configured through.
    pub fn enable_dog(&mut self, name: &str);

    /// Removes `name` from `[daemon] enabled_dogs`, leaving `[dog.<name>]`
    /// in place: an operator who disables a dog to restart it must not lose
    /// the configuration they wrote for it.
    pub fn disable_dog(&mut self, name: &str);

    /// Records `name`'s binary in `[daemon] adopted_dogs` and enables it.
    pub fn adopt_dog(&mut self, name: &str, exec: &Path);

    /// Forgets `name` entirely: out of `enabled_dogs`, out of
    /// `adopted_dogs`, and `[dog.<name>]` removed. The difference between
    /// `rehome` and `disable`, and the reason they are two verbs.
    pub fn rehome_dog(&mut self, name: &str);

    /// Writes the document back, creating the parent directory if needed.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] — the write failed.
    pub fn save(&self) -> Result<(), ShepTomlError>;
}

// commands/dogs.rs
/// `shep enable <name>`: writes the config, and starts the dog if a
/// shepherd is running.
pub async fn enable(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode;

/// `shep disable <name>`: removes it from the config, and stops it if a
/// shepherd is running.
pub async fn disable(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode;
```

Cargo shape for this task: `-p shep-cli`.

**The dependency:** `toml_edit = { version = "0.22", default-features = false, features = ["display", "parse"] }` in `crates/shep-cli/Cargo.toml`. It is already in `Cargo.lock` at 0.22.27 as `toml` 0.8's own dependency, so this adds **zero** crates to the tree. Confirm that rather than trusting it: run `cargo tree -p shep-cli --depth 100 2>/dev/null | wc -l` before and after and paste both numbers, and verify the feature names against `toml_edit`'s own manifest before committing — the plan names them from the version in the lockfile today.

**These verbs do not autostart the daemon.** `enable` against no running daemon writes the config, prints that the dog will come up with the next shepherd, and exits **0** — decision 11. Autostarting a whole supervisor as a side effect of a config edit would be a surprise out of proportion to the ask; `shep muster` is the one verb that autostarts, and it says so in its own help text.

**The order is config first, then the daemon.** If the RPC fails, the config still says the dog is enabled, and the next boot brings it up — which is the state an operator asked for. The reverse order would leave a dog running that no boot restores.

- [ ] **Step 1: Write the failing tests.** In `shep_toml.rs`:

```rust
    /// fails if the writer round-trips through a plain `toml::Table`. An
    /// operator's `shep.toml` is hand-written, and a `shep enable` that
    /// silently dropped their comments and reordered their keys is a reason
    /// not to run `shep enable`.
    #[test]
    fn enabling_a_dog_leaves_the_rest_of_the_file_exactly_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        let original = "# the shepherd's own knobs\n[daemon]\nlog_level = \"info\"  # chatty\nlog_json = false\n";
        std::fs::write(&path, original).unwrap();

        let mut doc = ShepToml::open(&path).unwrap();
        doc.enable_dog("metrics");
        doc.save().unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# the shepherd's own knobs"));
        assert!(written.contains("# chatty"));
        assert!(
            written.find("log_level").unwrap() < written.find("log_json").unwrap(),
            "key order survives"
        );

        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["metrics"]);
        assert!(cfg.dog.contains_key("metrics"), "a table to configure it through");
    }

    /// fails if `enable` appends a duplicate on the second call, which
    /// would make the daemon try to start one dog twice at boot, or if
    /// `disable` takes the dog's configuration with it — the operator who
    /// disables a dog to restart it must get their rules back.
    #[test]
    fn enable_is_idempotent_and_disable_keeps_the_config_it_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[dog.bark]\ndebounce = \"30s\"\n").unwrap();

        let mut doc = ShepToml::open(&path).unwrap();
        doc.enable_dog("bark");
        doc.enable_dog("bark");
        doc.save().unwrap();
        let cfg = DaemonConfig::load(Some(&std::fs::read_to_string(&path).unwrap()), &|_| None).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["bark"]);

        let mut doc = ShepToml::open(&path).unwrap();
        doc.disable_dog("bark");
        doc.save().unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(cfg.daemon.enabled_dogs.is_empty());
        assert!(
            written.contains("30s"),
            "disable stops a dog; rehome is what forgets it"
        );
    }

    /// fails if a `shep.toml` that will not parse is overwritten instead of
    /// refused. That file may hold every knob a daemon boots with; losing
    /// it to a typo'd `shep enable` is not recoverable.
    #[test]
    fn a_file_that_will_not_parse_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon\nlog_json = true\n").unwrap();
        assert!(matches!(
            ShepToml::open(&path),
            Err(ShepTomlError::Parse { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[daemon\nlog_json = true\n"
        );
    }
```

In `commands/dogs.rs`, driving the existing `shep_client::testing` fakes:

```rust
    /// fails if `enable` sends anything but `EnableDog` with the name it was
    /// given and a `BuiltIn` source — the class of bug that left `restart`
    /// and `delete` sending `Request::Stop` with every test green.
    #[tokio::test]
    async fn enable_asks_the_shepherd_to_start_that_dog_as_a_built_in() { /* ... */ }

    /// fails if a `shep enable` with no shepherd running is reported as a
    /// failure. The config edit is the part the operator asked for, and it
    /// landed; the dog comes up with the next boot. A non-zero exit here
    /// would make `shep enable` unusable in a provisioning script that
    /// configures a host before starting anything.
    #[tokio::test]
    async fn enable_with_no_shepherd_writes_the_config_and_exits_zero() { /* ... */ }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** `ShepToml`, the two verbs, `DogEnabledRow`/`DogDisabledRow` in `output/rows.rs` (name, source, whether a shepherd acted, the resulting status), the clap variants, and the dispatch arms with their own `*_parses_to_its_own_command` tests beside `dogs`'.

`ShepTomlError` is a per-module enum (IR-18). Its `Debug` must **not** print the document: a `shep.toml` holds `[dog.bark]` webhook URLs, and an error rendered into a log would carry them. Redact to the path and the parser message, with an exact-string test (IR-41).

- [ ] **Step 4: Mutation check.** Make `disable_dog` also remove the `[dog.<name>]` table, and watch `enable_is_idempotent_and_disable_keeps_the_config_it_did_not_write` redden on the `30s` assertion. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 5: CHANGELOG** — shep-cli: `enable`/`disable`, what each does with and without a running shepherd, and that a config change reaches a running dog only through `disable` then `enable`.

- [ ] **Step 6: Task gate, then commit** — `feat(cli): enable and disable a dog`

---

## Task 11: `shep adopt` and `shep rehome`

**Files:**
- Modify: `crates/shep-cli/src/commands/dogs.rs`, `crates/shep-cli/src/commands/shep_toml.rs`, `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`, `crates/shep-cli/src/output/rows.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced:**

```rust
/// `shep adopt <name> <path>`: vets a binary shep has never seen, records
/// it, and starts it if a shepherd is running.
pub async fn adopt(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &AdoptArgs,
) -> ExitCode;

/// `shep rehome <name>`: stops an adopted dog and forgets it entirely.
pub async fn rehome(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode;

/// Why a binary cannot be adopted.
///
/// The three modes `enable` structurally cannot have, and the reason the
/// two verbs are split rather than one verb carrying an `--exec` flag: a
/// dog that already ships inside this binary has no path to be missing, no
/// permission bit to be unset, and no architecture to be wrong.
#[derive(Debug, PartialEq, Eq)]
pub enum AdoptRefusal {
    /// Nothing exists at that path.
    Missing,
    /// It exists and is not a file (a directory, most often a `bin/` the
    /// operator meant to point inside of).
    NotAFile,
    /// It exists and no execute bit is set for anyone.
    NotExecutable,
    /// It exists, is executable, and this kernel refused to exec it —
    /// the wrong architecture, or an interpreter line naming something
    /// absent.
    WillNotExec {
        /// What `exec` reported.
        reason: String,
    },
}

/// Vets `path` as a dog binary, before anything is written to `shep.toml`.
///
/// # Errors
/// The refusal, which the caller renders. Nothing here is a shep fault, so
/// none of these is an [`ExitCode::Internal`].
pub fn vet_binary(path: &Path) -> Result<PathBuf, AdoptRefusal>;
```

Cargo shape for this task: `-p shep-cli`.

**`vet_binary` returns the ABSOLUTE path**, canonicalized. The daemon spawns from `shep.toml` after a reboot, from whatever working directory the init system gave it; a relative path recorded here would resolve against the operator's shell and then fail to exec months later, with nothing to connect the failure to the `adopt` that caused it.

**The three checks run before the config is touched**, so a refused adopt leaves `shep.toml` exactly as it was. That ordering is the opposite of `enable`'s (config first, then the daemon) and deliberately so: `enable` cannot fail its vetting, because there is nothing to vet.

**`WillNotExec` is checked by actually trying it**, not by reading an ELF or Mach-O header:

```rust
    // Spawned with the same arguments the daemon will use, and killed the
    // instant it is confirmed to exist: the question is whether this kernel
    // can exec this file, and the only authority on that is this kernel.
    // Reading a header would mean writing a second, partial loader that
    // disagrees with the real one — on a fat Mach-O, on a shebang naming an
    // absent interpreter, on a binary needing a missing dynamic library.
```

A spawned probe must be reaped. Use `std::process::Command::spawn`, then `kill` and `wait` in the same function, with the `wait` unconditional (including on the error path) so no zombie survives a refusal. Say in the report how the probe is torn down.

**`--exec` survives as a hidden alias.** `shep enable --exec <path> <name>` parses and routes to `adopt`, with the arguments in pm2's order, and is `#[arg(hide = true)]` so `--help` teaches the real verb. Note the argument order inversion loudly in the code: `enable --exec` takes path-then-name, `adopt` takes name-then-path, and a reader who assumes they agree has introduced a silent swap.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// The three modes `enable` cannot have, and the reason the two verbs
    /// are split. fails if any of them is reported as one of the others —
    /// "not executable" for a path that does not exist sends an operator to
    /// `chmod` a file that is not there.
    #[test]
    fn a_binary_shep_has_never_seen_is_vetted_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            vet_binary(&dir.path().join("nope")),
            Err(AdoptRefusal::Missing)
        );
        assert_eq!(vet_binary(dir.path()), Err(AdoptRefusal::NotAFile));

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(vet_binary(&plain), Err(AdoptRefusal::NotExecutable));

        // The same file, now executable: the ONLY thing that changed is the
        // mode bit, so a `vet_binary` that refused for some other reason
        // fails here rather than passing for the wrong one.
        let mut mode = std::fs::metadata(&plain).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&plain, mode).unwrap();
        assert_eq!(vet_binary(&plain).unwrap(), plain.canonicalize().unwrap());

        // Executable, and not something this kernel can run.
        let bogus = dir.path().join("bogus");
        std::fs::write(&bogus, b"\x7fELF\x00\x00\x00 not really").unwrap();
        let mut mode = std::fs::metadata(&bogus).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&bogus, mode).unwrap();
        assert!(matches!(
            vet_binary(&bogus),
            Err(AdoptRefusal::WillNotExec { .. })
        ));
    }

    /// fails if a refused adopt still edits `shep.toml`. The vetting is
    /// worth nothing if the config records the binary anyway and the next
    /// boot tries to run it.
    #[tokio::test]
    async fn a_refused_adopt_leaves_the_config_untouched() { /* ... */ }

    /// fails if `rehome` behaves as `disable` does — the whole difference
    /// between the two verbs is that this one forgets the registration,
    /// including the `[dog.<name>]` table and the `adopted_dogs` entry.
    #[test]
    fn rehome_forgets_everything_disable_deliberately_keeps() { /* ... */ }

    /// fails if `enable --exec` routes to `enable` (which would try to run a
    /// built-in dog named after a path), and fails if the argument order is
    /// read as `adopt`'s. The two orders are inverted, and a swap here is
    /// silent: both arguments are strings.
    #[test]
    fn the_hidden_pm2_spelling_reaches_adopt_with_the_arguments_the_right_way_round() {
        use clap::Parser;
        let parsed = Cli::try_parse_from(["shep", "enable", "--exec", "/opt/bin/d", "otel"]).unwrap();
        // ... assert it resolves to name "otel", path "/opt/bin/d"
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** `AdoptArgs { pub name: String, pub path: PathBuf }` — both positional, in that order (decision 10). `vet_binary`, the two verbs, the hidden `--exec` route, the rendered rows, the dispatch arms and their parse tests.

- [ ] **Step 4: Mutation check.** Move the `vet_binary` call in `adopt` to *after* the `ShepToml::save`, and watch `a_refused_adopt_leaves_the_config_untouched` redden. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 5: Confirm no zombies.** Run `cargo test -p shep-cli --bins` and, immediately after, `ps -o pid,ppid,stat,command -A | grep -c ' Z '` — or the platform equivalent — and say in the report that the vetting probe leaves none.

- [ ] **Step 6: CHANGELOG** — shep-cli: `adopt`/`rehome`, the three refusals, the trust level an adopted dog runs at (the daemon's own, with no sandboxing beyond it — stated, not implied), and the hidden `enable --exec` alias.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): adopt and rehome a third-party dog`

---

## Task 12: `shep dog <name>` and the dog runtime

**Files:**
- Create: `crates/shep-cli/src/dog/mod.rs`
- Modify: `crates/shep-cli/src/main.rs`, `crates/shep-cli/src/cli.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 15, 21:**

```rust
/// A dog's connection to the shepherd, and its own configuration.
///
/// The whole of the dog contract from the dog's side: locate the socket
/// from `$SHEP_HOME` (the one variable a dog inherits), connect, handshake,
/// ask for `[dog.<name>]`, parse it. A dog has no useful work before this
/// exists — metrics polls the shepherd, bark subscribes to it — so nothing
/// here is deferred or made optional.
#[derive(Debug)]
pub struct DogRuntime {
    /// The connected client. A dog IS a client; there is no second protocol.
    pub client: Client,
    /// This dog's `[dog.<name>]` section, exactly as the shepherd rendered
    /// it, for the dog to parse into its own shape. Empty when the file has
    /// no such section.
    pub section: String,
    /// `$SHEP_HOME` as this dog resolved it.
    pub paths: ShepPaths,
}

impl DogRuntime {
    /// Connects and fetches `name`'s section.
    ///
    /// # Errors
    /// - [`DogRunError::Connect`] — no shepherd answered at the socket.
    /// - [`DogRunError::Request`] — the shepherd refused the config request.
    pub async fn start(name: &str, paths: ShepPaths) -> Result<Self, DogRunError>;

    /// This dog's section parsed into `T`, or `T::default()` when the
    /// shepherd had no section for it.
    ///
    /// # Errors
    /// - [`DogRunError::Section`] — the section does not fit `T`, naming
    ///   the dog and the parser's own message. A dog refuses to run on
    ///   configuration it cannot read rather than silently falling back to
    ///   defaults an operator did not ask for.
    pub fn config<T: serde::de::DeserializeOwned + Default>(&self) -> Result<T, DogRunError>;
}

/// Runs the named dog until it is signalled. `main`'s `dog` arm.
pub async fn run_dog(name: &str, paths: ShepPaths) -> ExitCode;
```

Cargo shape for this task: `-p shep-cli`.

**`dog/` is `#[cfg(unix)]`-gated in `main.rs`, exactly as `commands/` is.** It names `shep_client::Client` and binds sockets, neither of which has a Windows tier yet (spec §11's Windows functional tier is 0%). The `output/` modules stay pure — `DogRows` is built from `ProcessInfo` alone and its tests run on the Windows leg, which is the same division `FlockRows` already follows and the reason payload types live under `output/` rather than beside their verbs.

**Dispatched from `run`'s early block**, beside `daemon` and `bleats`, taking **no** locked stdout/stderr guard (decision 24). A `StderrLock` held on the main thread for a process lifetime wedged the daemon on its first warning in 2026-08-09, silently, with an empty log; a dog runs until it is signalled and would wedge the same way.

**A dog's own diagnostics go to stderr**, which the daemon's log pump captures into `$SHEP_HOME/logs/<name>-0-err.log` like any sheep's. That is the whole of a dog's logging story: it is a supervised process, so `shep bleats metrics` already works.

**An unknown dog name is `ExitCode::Usage`, not `Internal`.** The name comes from `enabled_dogs`, which an operator typed; naming the two built-ins in the message is what turns a typo into a fix.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// fails if a dog is handed defaults for a section it could not parse.
    /// A bark dog silently running with no rules because a `debounce` was
    /// misspelled is precisely the outcome that makes an operator trust the
    /// alerting they no longer have.
    #[test]
    fn a_section_that_does_not_fit_is_refused_rather_than_defaulted() {
        #[derive(Debug, Default, serde::Deserialize, PartialEq)]
        #[serde(deny_unknown_fields, default)]
        struct Cfg {
            port: u16,
        }
        let runtime = runtime_with_section("port = \"nine thousand\"\n");
        let err = runtime.config::<Cfg>().unwrap_err();
        assert!(matches!(err, DogRunError::Section { .. }));
        assert!(err.to_string().contains("port"));

        let empty = runtime_with_section("");
        assert_eq!(empty.config::<Cfg>().unwrap(), Cfg::default());
    }

    /// fails if the dog asks for someone else's section, or for none at
    /// all. `Request::DogConfig` carries the name, and a dog that sent a
    /// hardcoded one would read another dog's webhook URLs.
    #[tokio::test]
    async fn a_dog_asks_for_its_own_section_by_name() { /* fake daemon, capture the envelope */ }

    /// fails if `Commands::Dog` is wired to another verb, or if it is not
    /// hidden. It is a re-exec target, not something an operator runs.
    #[test]
    fn the_dog_subcommand_parses_and_stays_hidden() { /* ... */ }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** `DogArgs { pub name: String }`, `#[command(hide = true)] Dog(DogArgs)`, the early dispatch arm, `DogRuntime`, `DogRunError`, and `run_dog` dispatching on the name to the two built-ins (Tasks 15 and 21 fill those in; until then each is a stub that returns `ExitCode::Failure` with a message naming the task's own module — **not** a `todo!()`, which would abort a supervised process with a panic and a confusing log line).

`DogRunError`'s `Debug` is redacted (IR-41, exact-string test): the `Section` variant carries a parser message that can quote the offending line, and that line can be a webhook URL. Redact to the dog's name and a fixed description.

- [ ] **Step 4: CHANGELOG** — shep-cli: the hidden `dog` subcommand and what a dog inherits (one variable, `SHEP_HOME`).

- [ ] **Step 5: Task gate, then commit** — `feat(cli): give a dog its connection and its own config`


---

## Task 13: the HTTP surface, hand-rolled

**Files:**
- Create: `crates/shep-cli/src/dog/http.rs`
- Modify: `crates/shep-cli/src/dog/mod.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 15, 19:**

```rust
/// One HTTP/1.1 request, as much of it as a dog needs.
#[derive(Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method, uppercased as it arrived.
    pub method: String,
    /// The request target, path and query together.
    pub target: String,
    /// Header names lowercased; values trimmed.
    pub headers: BTreeMap<String, String>,
    /// The body, read to `content-length` and no further.
    pub body: Vec<u8>,
}

/// Reads one request off `stream`, bounded in both size and time.
///
/// Hand-rolled rather than pulled from a crate, and the reason is the whole
/// dependency tree: this workspace carries no HTTP server and does not want
/// one for a loopback endpoint serving one path. What it needs is a request
/// line, a header map and a body — under a hundred lines against
/// `tokio::io`, with no TLS to get wrong because the metrics endpoint is
/// loopback by default and binding it wider is the operator's explicit act.
///
/// Both bounds are load-bearing. `MAX_HEADER_BYTES` is what stops a peer
/// sending headers forever; `read_timeout` is what stops one that opens a
/// connection and says nothing from holding a task open. A metrics endpoint
/// is reachable by anything that can reach the port, which on a shared host
/// is more than the operator.
///
/// # Errors
/// - [`HttpError::Io`] — the read failed or the peer closed mid-request.
/// - [`HttpError::Malformed`] — no request line, or a header with no colon.
/// - [`HttpError::TooLarge`] — the head exceeded [`MAX_HEADER_BYTES`], or
///   the declared `content-length` exceeded [`MAX_BODY_BYTES`].
/// - [`HttpError::Timeout`] — the request did not arrive within
///   `read_timeout`.
pub async fn read_request<R: AsyncRead + Unpin>(
    stream: &mut R,
    read_timeout: Duration,
) -> Result<HttpRequest, HttpError>;

/// Writes one response and nothing else: `Connection: close` on every
/// reply, so neither side has to reason about keep-alive, pipelining, or a
/// half-read body left in the buffer.
///
/// # Errors
/// - The underlying write failed.
pub async fn write_response<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), HttpError>;

/// Ceiling on a request's head. Generous for a real client, small enough
/// that a hostile one cannot grow a dog's memory with it.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Ceiling on a declared `content-length`. The metrics dog reads no bodies
/// at all; this exists so a test sink can, and so the ceiling is one number
/// rather than a per-caller decision.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
```

Cargo shape for this task: `-p shep-cli`.

**This module is the answer to "sinks are HTTP, so a local test server — choose the smallest way given what is already in the dependency tree".** The tree has `tokio` with `net` and `io-util` and nothing else HTTP-shaped. So: the metrics dog serves with `read_request` + `write_response` over a `TcpListener`, and every test that needs a sink to POST at binds a `TcpListener` and reads with the same `read_request`. Reusing the reader in tests cannot mask a bad send — the reader is not what is under test, and what the assertions read is the captured method, target, headers and body.

Generic over `AsyncRead`/`AsyncWrite` rather than taking a `TcpStream`, so the unit tests below drive it over a `tokio::io::duplex` pair with no socket at all, and only the two integration-shaped tests bind a port.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// fails if the body is read past `content-length`, or if a request with
    /// no body blocks waiting for one. The second half is what a bare
    /// `read_to_end` gets wrong, and it hangs rather than failing — which is
    /// why this test is bounded and why the timeout is a parameter at all.
    #[tokio::test]
    async fn a_request_is_read_to_its_declared_length_and_no_further() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(b"POST /hook HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello-and-then-some")
            .await
            .unwrap();
        let req = tokio::time::timeout(
            Duration::from_secs(5),
            read_request(&mut server, Duration::from_secs(1)),
        )
        .await
        .expect("read_request must not hang on a body it already has")
        .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/hook");
        assert_eq!(req.body, b"hello");
        assert_eq!(req.headers.get("content-length").map(String::as_str), Some("5"));
        assert_eq!(req.headers.get("host").map(String::as_str), Some("x"), "names lowercase");
    }

    /// fails if a peer can grow the dog's memory by sending headers
    /// forever. The metrics endpoint is reachable by anything that can
    /// reach the port; on a shared host that is more than the operator.
    #[tokio::test]
    async fn a_head_past_the_ceiling_is_refused_rather_than_buffered() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let mut flood = b"GET / HTTP/1.1\r\n".to_vec();
        flood.extend(std::iter::repeat(b'x').take(MAX_HEADER_BYTES + 1));
        client.write_all(&flood).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(5),
                read_request(&mut server, Duration::from_secs(1))
            )
            .await
            .expect("the ceiling must fail, never hang")
            .unwrap_err(),
            HttpError::TooLarge { .. }
        ));
    }

    /// fails if a peer that connects and says nothing holds a task open.
    /// The tokio clock is paused, so this measures the timeout rather than
    /// waiting for it.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_says_nothing_is_dropped_at_the_timeout() { /* ... */ }

    /// fails if `Connection: close` is dropped from the response. Without
    /// it a client is entitled to keep the connection open and wait for a
    /// second reply that never comes, and `curl 127.0.0.1:9615/metrics`
    /// hangs after printing the exposition.
    #[tokio::test]
    async fn every_response_closes_its_connection() { /* ... */ }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** Read the head byte-by-byte into a bounded `Vec` until `\r\n\r\n` (a `BufReader::read_until` over `\n` is fine too — say which was used), parse, then read exactly `content-length` bytes. `write_response` emits `HTTP/1.1 <status>`, `Content-Type`, `Content-Length`, `Connection: close`, a blank line, and the body.

`HttpError` is a per-module enum with `Display` and `core::error::Error`. Its `Debug` needs no redaction — it carries sizes and a fixed reason, never a header value, and a header value is where an `Authorization` would be. State that in a comment.

- [ ] **Step 4: Mutation check.** Drop the `Connection: close` header and watch `every_response_closes_its_connection` redden. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 5: CHANGELOG** — shep-cli: none. This module has no user-visible surface until Task 15 serves through it; say so in the report rather than writing an entry for it.

- [ ] **Step 6: Task gate, then commit** — `feat(cli): read and write the little HTTP a dog needs`

---

## Task 14: the Prometheus exposition

**Files:**
- Create: `crates/shep-cli/src/dog/metrics/exposition.rs`, `crates/shep-cli/src/dog/metrics/mod.rs`
- Modify: `crates/shep-cli/src/dog/mod.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 15, 16:**

```rust
/// What the shepherd and the host looked like when the exposition was
/// rendered.
#[derive(Debug, Default)]
pub struct Reading {
    /// Every registered entry, sheep and dogs alike, as `ListFlock`
    /// answered.
    pub flock: Vec<ProcessInfo>,
    /// The shepherd's crate version, from the handshake rather than from a
    /// request: `HelloAck` already answered it, so asking again would be a
    /// round trip for something in hand (the same reasoning `shep ping`'s
    /// own module records).
    pub daemon_version: String,
    /// The shepherd's pid, from the same handshake.
    pub daemon_pid: u32,
    /// Host totals, `None` where the sampler could not read them.
    pub host: Option<HostReading>,
}

/// The machine the flock is running on.
///
/// Read through `sysinfo`, which is already a workspace dependency —
/// shep-daemon samples every sheep's tree with it — so naming it in
/// shep-cli's manifest adds **zero** crates to the tree. Confirm that with
/// `cargo tree -p shep-cli | wc -l` before and after rather than trusting
/// it, and hold the same 0.38 line the workspace entry pins for MSRV
/// reasons its own comment gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReading {
    /// Total physical memory in bytes.
    pub memory_total_bytes: u64,
    /// Memory in use, as the platform reports it.
    pub memory_used_bytes: u64,
    /// How many processes the host is running, the flock included. The
    /// number that explains a sampling walk getting slower.
    pub processes: usize,
    /// Seconds since the host booted.
    pub uptime_seconds: u64,
}

/// Renders `reading` as Prometheus text exposition, format version 0.0.4.
///
/// One `# HELP`/`# TYPE` pair per metric name, every series of a name
/// grouped beneath it, and a trailing newline — the three things a scraper
/// is entitled to and the three a hand-rolled renderer gets wrong.
///
/// Label values are escaped per the exposition format (`\\`, `"`, `\n`).
/// A sheep's name is operator-supplied and reaches this function verbatim,
/// so an unescaped quote in one name would corrupt every series after it in
/// the same response.
#[must_use]
pub fn render(reading: &Reading) -> String;
```

Cargo shape for this task: `-p shep-cli`.

**No `prometheus` crate.** The exposition format is a line per series; the crate would bring a registry, a collector trait and a gatherer to produce text this function produces directly, over data that arrives as one `Vec<ProcessInfo>` per scrape and is never accumulated.

The metric set, from spec §8 and the design's own list:

| Name | Type | Labels | Source |
|---|---|---|---|
| `shep_sheep_cpu_percent` | gauge | `sheep`, `id`, `fold` | `ProcessInfo::cpu_percent`, series omitted when `None` |
| `shep_sheep_memory_bytes` | gauge | `sheep`, `id`, `fold` | `ProcessInfo::memory_bytes`, omitted when `None` |
| `shep_sheep_restart_total` | counter | `sheep`, `id`, `fold` | `ProcessInfo::restarts` |
| `shep_sheep_uptime_seconds` | gauge | `sheep`, `id`, `fold` | `ProcessInfo::uptime_ms` |
| `shep_sheep_status` | gauge | `sheep`, `id`, `fold`, `status` | one series per status, `1` for the one it is in |
| `shep_dog_up` | gauge | `dog`, `source` | `1` when the dog is `Online`, else `0`, for every dog-marked entry |
| `shep_daemon_up` | gauge | `version` | always `1` — the scrape reached the shepherd |
| `shep_daemon_pid` | gauge | — | the shepherd's pid, so a restart is visible as a step change |
| `shep_host_memory_total_bytes` | gauge | — | `HostReading`, the whole group omitted when `None` |
| `shep_host_memory_used_bytes` | gauge | — | `HostReading` |
| `shep_host_processes` | gauge | — | `HostReading` |
| `shep_host_uptime_seconds` | gauge | — | `HostReading` |

**`shep_sheep_status` is one series per status with a `status` label, not one gauge holding an enum ordinal.** A number that means `Errored` only if you have the enum's declaration order in front of you is not a metric; a `status="errored"` series alerting rules can name is.

**A sheep with no reading contributes no series at all**, rather than a zero. Same rule the table follows for the same reason: a zero is a claim the daemon declined to make, and a Grafana panel averaging invented zeros reports a flock idler than it is.

**`shep_dog_up` is rendered from the flock's own dog-marked entries, and that covers every enabled dog because enabling one REGISTERS it whether or not it spawns.** `spawn_fresh` inserts an entry on the failure path too (Task 5), so a dog whose binary does not exist is registered and `Errored` rather than absent — and a dog that exhausted its budget is deregistered by nothing, so it stays registered and `Errored` as well. Both report `0`.

That property is load-bearing and worth naming, because the alternative reading is tempting: a series rendered only for dogs that *are running* makes "the monitoring is down" expressible as "no series", and no series is not something an alerting rule fires on. "Is the monitoring up" is the one question monitoring cannot answer about itself, which is why this answer belongs outside shep — and why the series has to exist to be alerted on. If a future change lets an enabled dog be absent from `ListFlock` entirely, this metric silently stops covering it, and the fix is to source the roster over the contract rather than to accept the gap.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// fails if a sheep with no reading is rendered as a zero. A Grafana
    /// panel averaging invented zeros reports a flock idler than it is, and
    /// the daemon says `None` precisely when it will not make that claim.
    #[test]
    fn a_sheep_with_no_reading_contributes_no_series() {
        let mut info = sample_info("web");
        info.cpu_percent = None;
        info.memory_bytes = None;
        let text = render(&Reading { flock: vec![info], ..reading() });
        assert!(!text.contains("shep_sheep_cpu_percent{"));
        assert!(!text.contains("shep_sheep_memory_bytes{"));
        // The counters do not depend on a sample and must still be there.
        assert!(text.contains("shep_sheep_restart_total{"));
    }

    /// fails if a status becomes an ordinal. `shep_sheep_status 4` is not a
    /// metric anyone can write an alert against without the enum's
    /// declaration order in front of them.
    #[test]
    fn status_is_a_label_with_one_series_per_state() {
        let mut info = sample_info("web");
        info.status = ProcStatus::Errored;
        let text = render(&Reading { flock: vec![info], ..reading() });
        assert!(text.contains(r#"shep_sheep_status{sheep="web",id="3",fold="backend",status="errored"} 1"#));
        assert!(text.contains(r#"status="online"} 0"#));
    }

    /// fails if a label value goes out unescaped. A sheep's name is
    /// operator-supplied and reaches the renderer verbatim; one quote in one
    /// name corrupts every series after it in the same response, and the
    /// scraper reports a parse error rather than a bad name.
    #[test]
    fn a_label_value_is_escaped_so_one_odd_name_cannot_corrupt_the_response() {
        let text = render(&Reading {
            flock: vec![sample_info(r#"we"b\x"#)],
            ..reading()
        });
        assert!(text.contains(r#"sheep="we\"b\\x""#));
        // Every line after the escape must still parse as `name{labels} value`.
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            assert_eq!(line.matches('"').count() % 2, 0, "unbalanced quotes: {line}");
        }
    }

    /// fails if a dog that is registered and dead reports nothing. "Is the
    /// monitoring up" is the one question monitoring cannot answer about
    /// itself, and a missing series is not an answer an alert can fire on.
    /// The fixture is a dog whose binary would not spawn — registered and
    /// `Errored`, which is exactly what a bad `adopt` produces.
    #[test]
    fn a_dog_that_is_down_reports_zero_rather_than_nothing() {
        let mut dead = sample_info("bark");
        dead.status = ProcStatus::Errored;
        dead.dog = Some(DogSource::BuiltIn);
        let text = render(&Reading { flock: vec![dead], ..reading() });
        assert!(text.contains(r#"shep_dog_up{dog="bark",source="built-in"} 0"#));
        assert!(
            !text.contains(r#"shep_sheep_status{sheep="bark""#),
            "a dog is not reported as a sheep"
        );
    }

    /// fails if a metric name is emitted without exactly one HELP and one
    /// TYPE, or if a name's series are not contiguous. Both are format
    /// requirements a scraper rejects the whole response over, and both are
    /// what a renderer built one series at a time gets wrong.
    #[test]
    fn every_metric_name_carries_one_help_one_type_and_contiguous_series() { /* ... */ }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** Group by metric name, emit `# HELP`/`# TYPE` once per name, then its series. Escape label values with a small private helper, and give that helper its own test.

- [ ] **Step 4: Verify against a real parser if one is at hand** — `promtool check metrics < /tmp/exposition.txt` if `promtool` exists on the machine. If it does not, say so in the report rather than claiming a check that did not run.

- [ ] **Step 5: Mutation check.** Drop the escape from the label writer and watch `a_label_value_is_escaped_so_one_odd_name_cannot_corrupt_the_response` redden on the unbalanced-quotes loop, not only on the first assertion — if only the first fails, the loop is not doing its job and needs a sharper case. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 6: CHANGELOG** — shep-cli: the metric set, in a table, since it is a public interface an operator writes dashboards against.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): render the flock as prometheus exposition`

---

## Task 15: the metrics dog

**Files:**
- Modify: `crates/shep-cli/src/dog/metrics/mod.rs`, `crates/shep-cli/src/dog/mod.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Task 22:**

```rust
/// `[dog.metrics]`.
///
/// `deny_unknown_fields`: a misspelled key must be a startup error naming
/// it, not a dog silently serving on a port the operator did not choose.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// Where to listen. Loopback by default; binding wider is explicit.
    ///
    /// A metrics endpoint carries every sheep's name, and on many hosts a
    /// sheep's name is the name of an internal service. `0.0.0.0:9615` is
    /// available to an operator who wants it and is never the default —
    /// this dog will not widen its own exposure as a side effect of being
    /// enabled.
    pub bind: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { bind: SocketAddr::from(([127, 0, 0, 1], 9615)) }
    }
}

/// Runs the metrics dog until it is signalled.
pub async fn run(runtime: DogRuntime) -> ExitCode;
```

Cargo shape for this task: `-p shep-cli`.

**Every scrape is a fresh `Request::ListFlock`**, not a cached reading refreshed on a timer. The daemon already takes a live sample on that verb (Phase 8's `with_live_stats`) and a scrape is the only reader — a timer would either serve a stale reading between scrapes or pay the sample when nobody is asking. A scrape interval faster than the daemon's own 15s CPU window gets the same CPU number twice, which is honest: that is the resolution the baseline has.

**`/metrics` answers the exposition; everything else answers 404 with a one-line body naming `/metrics`.** An operator who curls `/` should be told where to look, not given the exposition from the wrong path — that is how a scrape config ends up depending on a path the next version does not serve.

**A refused bind is a fatal, named exit, not a warning.** The dog's whole purpose is to serve that port; a metrics dog running and serving nothing is worse than one that is `Errored` in `shep dogs`, because the first looks fine. `EADDRINUSE` on 9615 is the ordinary case — a second daemon, or the operator's own Prometheus pushgateway — and the message says the address.

**A failed `ListFlock` answers 503, not a stale exposition or a 200 with nothing in it.** A scraper reading a 200 records "the flock is empty", which is indistinguishable from a real empty flock; a 503 is `up == 0` for that target, which is what actually happened.

- [ ] **Step 1: Write the failing tests.** These bind a real loopback port, so each takes port `0` and reads back the assigned one — never a fixed number, which is how a test suite starts failing on a developer's machine for reasons unrelated to the change.

```rust
    /// fails if a scrape is served from a cached reading. The fake shepherd
    /// answers a DIFFERENT flock to the second `ListFlock`, so a dog that
    /// polled once at startup serves the first one twice and reddens here.
    /// A cached reading is not a hypothetical shortcut — it is what a dog
    /// written around a refresh timer does, and it is invisible while the
    /// flock happens not to change.
    #[tokio::test]
    async fn every_scrape_asks_the_shepherd_again() {
        let shepherd = ScriptedShepherd::answering(vec![
            vec![sample_info("web")],
            vec![sample_info("web"), sample_info("api")],
        ]);
        let dog = serve_on_free_port(shepherd.client(), MetricsConfig::default_on_port(0)).await;

        let first = scrape(dog.addr(), "/metrics").await;
        assert!(first.contains(r#"sheep="web""#));
        assert!(!first.contains(r#"sheep="api""#));

        let second = scrape(dog.addr(), "/metrics").await;
        assert!(
            second.contains(r#"sheep="api""#),
            "the second scrape must see the second listing: {second}"
        );
        assert_eq!(shepherd.list_calls(), 2, "one ListFlock per scrape");
    }

    /// fails if the default bind is anything but loopback. A metrics
    /// endpoint carries every sheep's name; widening it must be the
    /// operator's explicit act and never a consequence of `shep enable`.
    #[test]
    fn the_default_bind_is_loopback() {
        assert_eq!(
            MetricsConfig::default().bind,
            "127.0.0.1:9615".parse::<SocketAddr>().unwrap()
        );
        // And an empty section — the ordinary case for a dog nobody
        // configured — resolves to that same default rather than to
        // `0.0.0.0`, which a `Default` derived on `SocketAddr` would give.
        let parsed: MetricsConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, MetricsConfig::default());
    }

    /// fails if a shepherd that will not answer produces a 200. A scraper
    /// reading a 200 with an empty body records an empty flock, which is
    /// indistinguishable from a real one; a 503 is `up == 0`, which is what
    /// happened.
    #[tokio::test]
    async fn a_shepherd_that_will_not_answer_produces_a_503() { /* ... */ }

    /// fails if any path serves the exposition. A scrape config that
    /// happens to work against `/` is a scrape config that breaks the day
    /// the path is honoured.
    #[tokio::test]
    async fn only_the_metrics_path_serves_metrics() { /* ... */ }
```

Every one of these bounds its reads with `tokio::time::timeout`, and each owns a guard that aborts the serving task on drop — a dog left listening past its test holds a port for the rest of the binary's run.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** `sysinfo.workspace = true` in `crates/shep-cli/Cargo.toml` for the host reading (zero new crates — confirm and paste the `cargo tree` counts), `MetricsConfig` parsed through `DogRuntime::config`, a `TcpListener::bind`, an accept loop spawning one task per connection, `read_request` → route → `ListFlock` → `render` → `write_response`. Shut down on `tokio::signal::ctrl_c` **and** on SIGTERM, which is what the daemon's kill ladder actually sends: use `tokio::signal::unix::signal(SignalKind::terminate())`. A dog that ignores SIGTERM rides the whole ladder to SIGKILL on every `shep disable`, which is slow and looks like a hang.

- [ ] **Step 4: Exercise it by hand and paste the output.**

```
shep enable metrics
curl -s 127.0.0.1:9615/metrics | head -30
curl -s -o /dev/null -w '%{http_code}\n' 127.0.0.1:9615/
shep dogs
```

- [ ] **Step 5: Mutation check.** Change the default bind to `0.0.0.0` and watch `the_default_bind_is_loopback` redden on **both** assertions — the `Default` impl one and the empty-section one. If only the first fails, `serde(default)` is not routing through the impl and the second assertion is the one that caught it. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 6: CHANGELOG** — shep-cli: the metrics dog, its default bind, its config key, and that a scrape is a live read.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): serve the flock's metrics`

---

## Task 16: the reference dashboard

**Files:**
- Create: `assets/grafana/shep.json`, `assets/grafana/README.md`
- Modify: `docs/specs/deferred.md`

**Interfaces — produced:** none. This task ships data and prose.

No cargo shape: this task compiles nothing. Run the gate anyway, because `cargo fmt --check` covers the workspace and this task must not have touched Rust.

`assets/` does not exist yet — spec §12 promises it and this is the first thing to live there.

The dashboard names exactly the metrics Task 14 renders, and nothing else. **Every panel's query must be checkable against that task's table**; a panel querying a metric shep does not emit is worse than a missing panel, because it renders as an empty graph an operator reads as "no load".

Panels:
1. **Flock status** — a table of `shep_sheep_status == 1`, so each sheep shows its one live state.
2. **CPU per sheep** — `shep_sheep_cpu_percent`, stacked.
3. **Memory per sheep** — `shep_sheep_memory_bytes`, stacked, bytes unit.
4. **Restarts** — `increase(shep_sheep_restart_total[1h])`, which is the shape an operator alerts on rather than the raw counter.
5. **Dog health** — `shep_dog_up`, a stat panel per dog, red at 0. **First on the page**, above the flock: if this one is red, every panel below it is stale and the reader needs to know that before reading them.
6. **Shepherd** — `shep_daemon_up` with the `version` label shown, so a dashboard tells you which shep is running.

- [ ] **Step 1: Write the dashboard.** Schema version and `__inputs` for a `DATASOURCE_PROMETHEUS` input, so it imports without hand-editing. `"uid": null` on the dashboard itself — a pinned uid collides on import into a Grafana that already has one.

- [ ] **Step 2: Check every query against Task 14's table.** Paste the list of metric names the JSON references (`grep -o 'shep_[a-z_]*' assets/grafana/shep.json | sort -u`) beside the table, and confirm every one is rendered. This is the step, not a formality — it is the only thing standing between a shipped dashboard and six empty panels.

- [ ] **Step 3: Validate the JSON.** `python3 -m json.tool assets/grafana/shep.json > /dev/null` — a dashboard that will not parse fails at import, in front of the operator.

- [ ] **Step 4: `assets/grafana/README.md`** — how to import it, which datasource it expects, and the one thing it cannot show: a metric the dog does not render. Public-facing prose, so **invoke the `humanizer` skill** on it and match `docs/shepherd-channel.md`'s register.

- [ ] **Step 5: `docs/specs/deferred.md`** — the Grafana dashboard JSON is named in the dogs entry; strike it there rather than leaving the entry to be reconciled wholesale in Task 23.

- [ ] **Step 6: Task gate, then commit** — `feat: ship a reference grafana dashboard`


---

## Task 17: `barks.jsonl`, and the ring that keeps it bounded

**Files:**
- Create: `crates/shep-core/src/barks.rs`
- Modify: `crates/shep-core/src/lib.rs`
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 18, 21, 22:**

```rust
/// One fired alert, as it lands in `$SHEP_HOME/barks.jsonl`.
///
/// One JSON object per line, because the file is appended to by two
/// writers (the bark dog when a rule fires, and the shepherd when an
/// enabled dog exhausts its budget) and read by a third (`shep barks`).
/// A line-delimited format is the one shape where an interrupted write
/// costs the reader one record instead of the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bark {
    /// Unix millis when the alert fired.
    pub at_ms: u64,
    /// The rule that fired, or `daemon` when the shepherd wrote this
    /// itself.
    pub rule: String,
    /// What it is about: a sheep's name, or a dog's.
    pub subject: String,
    /// The human-readable line. Plain English, no theme — this is read
    /// during an incident.
    pub message: String,
    /// Which sinks the alert was delivered to, and whether each took it.
    /// Empty when the shepherd wrote the record itself: it has no sinks
    /// and no webhook code, and says so by carrying none.
    pub sinks: Vec<SinkOutcome>,
}

/// What one sink made of one alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkOutcome {
    /// The sink's name from `[dog.bark.sinks]`.
    pub sink: String,
    /// `None` when it was delivered; the failure otherwise.
    pub error: Option<String>,
}

/// Appends `bark` to `path`, evicting oldest-first to keep the file under
/// `max_bytes`.
///
/// Eviction is oldest-out by whole lines: the file is rewritten with a
/// prefix of its lines dropped, atomically, so a reader never sees a
/// truncated one. A single record larger than `max_bytes` is written
/// anyway and leaves the file over the cap — the alternative is silently
/// dropping the alert that was too interesting to fit.
///
/// # Errors
/// - [`BarkError::Io`] — the file could not be read, written, or replaced.
/// - [`BarkError::Encode`] — the record could not be serialized.
pub fn append(path: &Path, bark: &Bark, max_bytes: u64) -> Result<(), BarkError>;

/// Reads every bark in `path`, oldest first, skipping any line that will
/// not parse.
///
/// A line that will not parse is a partially-written record from a writer
/// that died mid-append, or a record from a future shep. Neither is a
/// reason to refuse the whole history during an incident, which is the one
/// time this file is read.
///
/// # Errors
/// - [`BarkError::Io`] — the file exists and could not be read. A missing
///   file is `Ok(Vec::new())`: no barks yet is not a fault.
pub fn read(path: &Path) -> Result<Vec<Bark>, BarkError>;

/// Cap the ring keeps itself under when nobody configured one.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;
```

Cargo shape for this task: `-p shep-core`.

**In shep-core, because it has two writers and neither is the other's crate** (decision 23): the shepherd appends when an enabled dog gives up, the bark dog appends when a rule fires, and `shep barks` reads. Two implementations of the cap would evict differently, and the one that drifts is the one nobody watches.

**Not the log plane's rotation code**, because there is none to share: a sheep's logs are rotated by an external rotator that renames the file and signals `reopen` (spec §4). This ring is shep's own file with shep's own cap, and the design's "matching the log plane's model" is about the *model* — a size cap, oldest out — not about a shared implementation.

**Rewrite-and-replace, not truncate-in-place.** A truncating writer that is interrupted leaves a file whose first line is a fragment, and every reader afterwards skips a real record. Write the surviving lines plus the new one to a sibling temporary file in the same directory and `rename` over the original — the same atomic-replace shape `snapshot::write_atomic` already uses, and worth reading before writing this one.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// The eviction, which is the whole reason this is a ring and not an
    /// append. A cap the test never reaches leaves an append-only file with
    /// extra code, so the cap here is deliberately small enough that the
    /// third write MUST evict — and the assertion names the surviving
    /// subject rather than counting lines, so a ring that evicted the
    /// NEWEST record would fail here rather than pass on the count.
    #[test]
    fn the_ring_drops_the_oldest_bark_to_stay_under_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");
        let cap = 2 * one_bark_len();

        for (i, subject) in ["first", "second", "third"].iter().enumerate() {
            append(&path, &bark_for(subject, i as u64), cap).unwrap();
        }

        let barks = read(&path).unwrap();
        let subjects: Vec<&str> = barks.iter().map(|b| b.subject.as_str()).collect();
        assert_eq!(subjects, ["second", "third"], "oldest out, newest kept");
        assert!(
            std::fs::metadata(&path).unwrap().len() <= cap,
            "the cap is a cap"
        );
    }

    /// fails if a record larger than the whole cap is silently dropped. An
    /// alert too interesting to fit is exactly the one an operator needs;
    /// leaving the file over its cap for one record is the cheaper wrong.
    #[test]
    fn a_bark_bigger_than_the_cap_is_written_anyway() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");
        let huge = Bark { message: "x".repeat(4096), ..bark_for("web", 0) };
        append(&path, &huge, 64).unwrap();
        assert_eq!(read(&path).unwrap().len(), 1);
    }

    /// fails if one unparseable line refuses the whole history. That line
    /// is a writer that died mid-append or a record from a future shep, and
    /// this file is read during an incident — the surviving records are
    /// what the reader came for.
    #[test]
    fn a_line_that_will_not_parse_costs_one_record_and_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");
        append(&path, &bark_for("web", 1), DEFAULT_MAX_BYTES).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"at_ms\": 2, \"rul\n")
            .unwrap();
        append(&path, &bark_for("api", 3), DEFAULT_MAX_BYTES).unwrap();

        let barks = read(&path).unwrap();
        assert_eq!(
            barks.iter().map(|b| b.subject.as_str()).collect::<Vec<_>>(),
            ["web", "api"]
        );
    }

    /// fails if a missing file is an error. No barks yet is the state every
    /// machine starts in.
    #[test]
    fn no_file_yet_is_no_barks_rather_than_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(&dir.path().join("nothing.jsonl")).unwrap(), vec![]);
    }
```

`one_bark_len()` measures a serialized `bark_for(..)` line rather than hard-coding a byte count — a constant that happened to equal the implementation's own would pass for any cap, which is the assertion-against-the-same-constant shape this project has shipped before.

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-core --lib`.

- [ ] **Step 3: Implement.** Read the existing lines, drop from the front while the total (including the new record) exceeds `max_bytes`, write the survivors plus the new line to `<path>.tmp` in the same directory, `rename`. Read `crates/shep-daemon/src/snapshot.rs`'s `write_atomic` first and match its handling of the temporary file's mode and its cleanup on failure.

`Bark`'s `Debug` is **derived, not redacted**, and that is a decision worth its own comment: a bark carries a rule name, a subject and a message, all of which are shep's own prose. It carries **no** sink URL — `SinkOutcome` names the sink by its config key, never by its target. That is why the type is safe to print and why it must stay that way; a future field carrying a URL would change the answer, and the comment says so.

- [ ] **Step 4: Mutation check.** Change the eviction to drop from the *end* and watch `the_ring_drops_the_oldest_bark_to_stay_under_its_cap` redden on the subjects assertion. Then change the cap comparison to `<` and confirm the size assertion still holds (it should — this is a check that the test is not over-fitted). Restore from a `cp` snapshot; paste both outcomes.

- [ ] **Step 5: CHANGELOG** — shep-core: `Bark`, the ring, its cap, and what a reader does with a line it cannot parse.

- [ ] **Step 6: Task gate, then commit** — `feat(core): keep the bark history bounded`

---

## Task 18: the shepherd's own record of a dog that gave up

**Files:**
- Modify: `crates/shep-daemon/src/dogs.rs`, `crates/shep-daemon/src/boot.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced:**

```rust
/// Watches the bus and records, locally, every enabled dog that exhausts
/// its restart budget.
///
/// The shepherd cannot DELIVER an alert about a dead bark dog: it has no
/// sinks and no webhook code, by design. What it can guarantee is a local
/// trail, so an operator reading `shep barks` after an outage finds the
/// moment alerting stopped rather than a gap they have to infer.
///
/// A bus WATCHER rather than a branch inside the supervisor, and the
/// distinction is the phase's own tripwire: this answers *who should see
/// this*, from outside, and the supervisor stays a machine that knows only
/// how to supervise. A `dog` arm inside `handle_exited` would be the same
/// behaviour reaching into the wrong place.
///
/// Its `JoinHandle` is held by the caller and aborted at teardown: the task
/// parks on a broadcast receiver, which ends on its own when the sender
/// drops, and holding the handle is what makes the end deterministic rather
/// than dependent on sender count.
pub fn spawn_dog_watch(
    events: broadcast::Receiver<BusEvent>,
    barks: PathBuf,
) -> tokio::task::JoinHandle<()>;
```

Cargo shape for this task: `-p shep-daemon`.

**It records `Errored` on a dog and nothing else.** Not `Exit`, which fires on every restart a dog survives, and not a sheep's `Errored`, which is bark's job and which the shepherd has no business duplicating — two records of one event, written by two writers into one file, is how a history stops being trustworthy.

**A lagged watcher re-reads nothing.** `BusEvent::Dropped` on this receiver means the shepherd missed a dog's death notice, which the watcher logs loudly at `warn` and cannot recover — it has no poll, deliberately, because building one here would be building a second bark dog inside the shepherd. Metrics' `shep_dog_up` is the answer to that gap and is why decision 22 pairs the two.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// fails if the shepherd records a sheep's death as well as a dog's.
    /// Bark writes the sheep records; two writers for one event, into one
    /// file, is how a history stops being trustworthy. Both halves are
    /// needed: without the negative assertion, a watcher that recorded
    /// EVERY `Errored` passes.
    #[tokio::test]
    async fn the_shepherd_records_a_dog_that_gave_up_and_leaves_the_sheep_to_bark() {
        let dir = tempfile::tempdir().unwrap();
        let barks = dir.path().join("barks.jsonl");
        let (events, rx) = broadcast::channel(16);
        let watch = spawn_dog_watch(rx, barks.clone());

        events.send(errored_event("web", None)).unwrap();
        events.send(errored_event("bark", Some(DogSource::BuiltIn))).unwrap();

        let recorded = await_barks(&barks, 1).await;
        assert_eq!(recorded.len(), 1, "one record, and it is the dog's");
        assert_eq!(recorded[0].subject, "bark");
        assert_eq!(recorded[0].rule, "daemon");
        assert!(
            recorded[0].sinks.is_empty(),
            "the shepherd has no sinks and says so by carrying none"
        );

        watch.abort();
    }

    /// fails if a restart a dog survives is recorded as a death. A dog that
    /// crashes and comes back is not an outage, and a `barks.jsonl` full of
    /// them is one an operator stops reading.
    #[tokio::test]
    async fn a_dog_that_merely_exited_is_not_recorded() { /* Exit, not Errored */ }
```

`await_barks(&path, n)` polls the file under a `tokio::time::timeout` ceiling — the watcher is a separate task, so a bare read races it, and a bare `recv().await` on nothing is the hang this project has already paid for twice.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** The watcher task; `boot` spawns it right after the bus exists and holds the handle on `RunningDaemon`, aborting it in the same teardown step that stops the snapshot writer. Read that step before writing it — the ordering comment there explains why each subsystem ends where it does.

The record's `message` is plain English naming the dog, its restart count and the budget it exhausted (`docs/terminology.md`: error text stays plain). The `tracing::error!` beside it carries the same facts, because the two audiences are different — one is `shep barks` during an incident, the other is `journalctl`.

- [ ] **Step 4: Mutation check.** Widen the filter to every `Errored` and watch `the_shepherd_records_a_dog_that_gave_up_and_leaves_the_sheep_to_bark` redden on the count. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 5: CHANGELOG** — shep-daemon: the shepherd records an enabled dog that exhausts its budget, and why it cannot deliver that alert itself.

- [ ] **Step 6: Task gate, then commit** — `feat(daemon): leave a trail when a dog gives up`

---

## Task 19: bark's sinks

**Files:**
- Create: `crates/shep-cli/src/dog/bark/sinks.rs`, `crates/shep-cli/src/dog/bark/mod.rs`
- Modify: `crates/shep-cli/src/dog/mod.rs`, `crates/shep-cli/Cargo.toml`, `Cargo.toml`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 20, 21:**

```rust
/// One named entry under `[dog.bark.sinks]`.
///
/// `Debug` is REDACTED (IR-41): every variant carries a webhook URL, and a
/// Discord or Slack webhook URL is a bearer credential — anyone holding it
/// can post to that channel. A sink printed into a log, a panic message or
/// an error chain leaks it to whoever reads the log.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Sink {
    /// A Discord webhook: `{"content": "..."}`.
    Discord {
        /// The webhook URL.
        url: String,
    },
    /// A Slack incoming webhook: `{"text": "..."}`.
    Slack {
        /// The webhook URL.
        url: String,
    },
    /// A JSON POST with a body the operator templates.
    Json {
        /// Where to POST.
        url: String,
        /// The body, with `{subject}`, `{rule}`, `{message}` and `{at_ms}`
        /// substituted. Defaults to an object carrying all four.
        body: Option<String>,
    },
}

/// The body `sink` sends for `bark` — pure, and the half worth testing
/// exhaustively.
///
/// # Errors
/// - [`SinkError::Template`] — the rendered body is not valid JSON, which
///   a templated `body` can produce and which every one of these endpoints
///   refuses with a 400 an operator would otherwise have to guess at.
pub fn render_body(sink: &Sink, bark: &Bark) -> Result<String, SinkError>;

/// POSTs `bark` to `sink`, bounded by `timeout`.
///
/// # Errors
/// - [`SinkError::Template`] — as [`render_body`].
/// - [`SinkError::Transport`] — the request failed or timed out.
/// - [`SinkError::Status`] — the endpoint answered outside 2xx, carrying
///   the status and the first line of the body. Discord's own rate-limit
///   429 arrives this way and reads as one.
pub async fn deliver(sink: &Sink, bark: &Bark, timeout: Duration) -> Result<(), SinkError>;
```

Cargo shape for this task: `-p shep-cli`.

### The dependency

**Discord and Slack webhooks are HTTPS.** There is no way to POST to one without TLS, and TLS is not something to hand-roll. So bark needs an HTTP client, and this is the phase's one new workspace dependency.

**Chosen: `reqwest` 0.13, over rustls, async.** Rin has standardised on `reqwest` across her recent Rust projects; consistency across the codebases she maintains outweighs a smaller crate count, and an async client fits a program that is tokio all the way down — a blocking client would mean `spawn_blocking` around every webhook POST, for no benefit bark needs.

```toml
# Bark's sinks are Discord and Slack webhooks, which are HTTPS, so this is
# the one thing in the workspace that needs TLS. reqwest 0.13, over rustls,
# is what Rin standardises on across her Rust projects. Every dependency in
# this workspace is default-features = false, so rustls is named explicitly
# rather than inherited — reqwest 0.13 already defaults to it (native-tls,
# an OpenSSL system dependency on some platforms, moved to opt-in), but the
# workspace never leans on a crate's own defaults to get there. `json` is
# not named: `render_body` already renders the templated body to a `String`,
# and `deliver` sends it with an explicit `content-type` header rather than
# through `.json()`, so nothing here needs reqwest's own (de)serialization.
reqwest = { version = "0.13", default-features = false, features = ["rustls"] }
```

**Confirmed against `reqwest`'s own `Cargo.toml` and its `src/lib.rs` feature-flag doc (0.13.4, the current 0.13 release, on the project's GitHub repository):** 0.12 spelled this feature `rustls-tls`; 0.13 renamed it to `rustls` and made it the crate's own default TLS backend, with `native-tls` moved to opt-in. `json` gates `RequestBuilder::json()`/`Response::json()` alone — `.header()`, `.body()` and `.timeout()` are core `RequestBuilder` methods gated behind no feature, so a POST carrying a hand-rendered JSON string, an explicit content-type header, and a timeout needs nothing beyond `rustls`. Paste `cargo tree -p shep-cli | wc -l` before and after into the report, so the real cost is a number rather than an estimate.

**The dependency ships unconditionally, in every `shep` binary, for every user including one who never enables bark — and that is the decided model, not a size tradeoff being accepted.** `docs/systematic-refactor/refactor-workspace/decision-briefs.md` §3b (Rin, 2026-08-07) settled the shape a first-party dog takes: cargo features are for build-slimming source builds, not runtime pluggability, because a feature-flagged dog is the weaker version of a dog — no crash isolation, no independent restart, and inert for most users anyway since the release binary is one binary. Runtime opt-in is `shep enable bark`, the process model doing the job a feature flag would do worse. `reqwest` sits in that one binary the same way the `bark` argv branch itself does, and for the same reason.

### The test server

**Hand-rolled over `tokio::net::TcpListener`, reading with Task 13's `read_request`.** It binds port 0, reports the port back through a `oneshot`, accepts one connection, captures the request, and answers whatever status the test asked for. Never a real webhook, and — because the sink is URL-driven — the Discord and Slack bodies are tested by pointing those variants' `url` at the local `http://` server. That is the whole reason the body renderer is a separate pure function: what is being asserted is the *shape Discord expects*, and reaching Discord to learn it would be a test of the network.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// fails if Discord's body is sent under Slack's key or vice versa.
    /// Both are one-key JSON objects over the same transport, so a swap
    /// compiles, delivers, and is answered with a 400 nobody sees until an
    /// incident — the alert is simply never posted.
    #[test]
    fn each_webhook_gets_the_body_its_own_endpoint_expects() {
        let bark = bark_for("web", "the shepherd gave up on web");
        let discord: serde_json::Value =
            serde_json::from_str(&render_body(&discord_sink(), &bark).unwrap()).unwrap();
        assert_eq!(discord["content"], "the shepherd gave up on web");
        assert!(discord.get("text").is_none());

        let slack: serde_json::Value =
            serde_json::from_str(&render_body(&slack_sink(), &bark).unwrap()).unwrap();
        assert_eq!(slack["text"], "the shepherd gave up on web");
        assert!(slack.get("content").is_none());
    }

    /// fails if a templated body is sent without being checked. Every one
    /// of these endpoints answers a malformed body with a 400, and an
    /// operator reading "400" has no way to know their template lost a
    /// brace — this is the one failure bark can name precisely.
    #[test]
    fn a_template_that_does_not_render_json_is_refused_before_it_is_sent() {
        let sink = Sink::Json {
            url: "http://127.0.0.1:1/".to_string(),
            body: Some(r#"{"text": "{message}"#.to_string()),
        };
        assert!(matches!(
            render_body(&sink, &bark_for("web", "x")),
            Err(SinkError::Template { .. })
        ));
    }

    /// fails if a substituted value is interpolated raw. A sheep's name and
    /// a bark's message are shep's own prose, but the message quotes an
    /// app's name, and an app named `we"b` would break the template's JSON
    /// the same way it would break a Prometheus label.
    #[test]
    fn a_substituted_value_is_json_escaped_into_the_template() { /* ... */ }

    /// The delivery half, against a local server and never a real webhook.
    /// fails if the POST goes out with the wrong method, path or
    /// content-type — three things a receiving endpoint rejects and a unit
    /// test over `render_body` alone can say nothing about.
    #[tokio::test]
    async fn a_delivery_posts_json_to_the_url_it_was_given() {
        let (addr, captured) = one_shot_sink(200, "").await;
        let sink = Sink::Json {
            url: format!("http://{addr}/hook"),
            body: None,
        };
        deliver(&sink, &bark_for("web", "x"), Duration::from_secs(5))
            .await
            .unwrap();
        let req = tokio::time::timeout(Duration::from_secs(5), captured)
            .await
            .expect("the sink server must receive a request")
            .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/hook");
        assert_eq!(
            req.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&req.body).unwrap()["subject"], "web");
    }

    /// fails if a non-2xx is treated as delivered. Discord's rate-limit 429
    /// arrives exactly this way, and a bark counted as delivered when it
    /// was refused is the failure mode alerting exists to not have.
    #[tokio::test]
    async fn a_refused_delivery_is_a_failure_carrying_the_status() {
        let (addr, _captured) = one_shot_sink(429, "rate limited").await;
        let err = deliver(&Sink::Json { url: format!("http://{addr}/"), body: None }, &bark_for("web", "x"), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, SinkError::Status { code: 429, .. }));
    }

    /// fails if `Sink`'s Debug prints a URL. A webhook URL is a bearer
    /// credential: whoever reads the log can post to that channel.
    #[test]
    fn a_sinks_debug_never_prints_its_webhook() {
        let rendered = format!("{:?}", discord_sink());
        assert_eq!(rendered, "Sink::Discord { url: <redacted> }");
        assert!(!rendered.contains("discord.com"));
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** `render_body` (pure), `deliver` (an async `reqwest::Client`, POSTing the rendered body with an explicit `content-type: application/json` header and `RequestBuilder::timeout(timeout)` — a per-request timeout that cancels the request future itself, with no separate thread for it to abandon), the hand-written `Debug`, and `one_shot_sink` in the test module.

- [ ] **Step 4: Mutation check.** Swap `content` and `text` between the Discord and Slack renderers and watch `each_webhook_gets_the_body_its_own_endpoint_expects` redden on **both** the positive and the negative assertion — if only the positive fails, the negative one is not sharp enough. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 5: CHANGELOG** — shep-cli: the three sink kinds, the templated body's substitutions, the new dependency and why it exists.

- [ ] **Step 6: Task gate, then commit** — `feat(cli): deliver a bark to a webhook`

---

## Task 20: bark's rules

**Files:**
- Create: `crates/shep-cli/src/dog/bark/rules.rs`
- Modify: `crates/shep-cli/src/dog/bark/mod.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Task 21:**

```rust
/// One entry under `[dog.bark.rules]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// What fires it.
    #[serde(flatten)]
    pub when: Trigger,
    /// Sinks by name, from `[dog.bark.sinks]`. At least one; a rule
    /// routing nowhere is a rule that fires into a file and is refused at
    /// startup rather than discovered during an incident.
    pub sinks: Vec<String>,
    /// How long after one firing this rule stays quiet FOR THE SAME
    /// SUBJECT. Per-subject, never global: a flock where one sheep flaps
    /// must not mute the alert for a different sheep going down.
    #[serde(default = "default_debounce")]
    pub debounce: UpDuration,
}

/// What makes a rule fire.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Trigger {
    /// Any of these bus event kinds, by their wire spelling
    /// (`exit`, `errored`, `online`, ...).
    Event {
        /// The kinds this rule fires on.
        kinds: Vec<String>,
    },
    /// The shepherd gave up: a sheep reached `Errored`. On by DEFAULT with
    /// no configuration at all, because it is the alert that must not be
    /// missed — the app is down and staying down — and because it cannot
    /// disagree with the shepherd: it is keyed to the shepherd's own
    /// decision rather than to a threshold bark chose.
    GaveUp,
    /// The early warning: `restarts` restarts within `within`. Opt-in,
    /// because it is the one that pages at 3am for a blip, and the
    /// threshold should be one the operator chose.
    RestartRate {
        /// How many restarts.
        restarts: u32,
        /// Within how long.
        within: UpDuration,
    },
    /// A sheep's memory crossed a ceiling, read from the reconciliation
    /// poll rather than from the bus — memory is a level, and the bus
    /// carries events.
    MemoryAbove {
        /// The ceiling.
        bytes: MemSize,
    },
}

/// Bark's whole state: the rules, what each subject last looked like, and
/// when each rule last fired for each subject.
#[derive(Debug)]
pub struct Rules { /* ... */ }

impl Rules {
    /// Builds the engine, refusing a configuration that cannot work.
    ///
    /// # Errors
    /// - [`RulesError::UnknownSink`] — a rule routes to a sink name
    ///   `[dog.bark.sinks]` does not define. Refused at startup rather than
    ///   at 3am: the rule would fire correctly and deliver nowhere.
    /// - [`RulesError::NoSinks`] — a rule routes to none at all.
    /// - [`RulesError::UnknownKind`] — an `Event` rule names an event kind
    ///   that is not on the wire, which is a typo and not a future event:
    ///   bark and the shepherd ship in one binary.
    pub fn new(rules: Vec<Rule>, sinks: &BTreeMap<String, Sink>) -> Result<Self, RulesError>;

    /// The default rule set, for a `[dog.bark]` that configured none: one
    /// `GaveUp` rule routed to every configured sink.
    #[must_use]
    pub fn default_rules(sinks: &BTreeMap<String, Sink>) -> Vec<Rule>;

    /// What one bus event fires, after debounce.
    #[must_use]
    pub fn on_event(&mut self, event: &BusEvent, now_ms: u64) -> Vec<Firing>;

    /// What the reconciliation poll fires: everything the bus should have
    /// carried and did not, plus the level-triggered rules that have no bus
    /// event at all.
    ///
    /// Reads `ProcessInfo::restarts` — the shepherd's own count — rather
    /// than a tally bark kept. A private tally drifts from the number the
    /// shepherd acts on, and the operator would be told a different story
    /// from the one the supervisor believes.
    #[must_use]
    pub fn on_poll(&mut self, flock: &[ProcessInfo], now_ms: u64) -> Vec<Firing>;
}

/// One rule firing for one subject: the bark to write and where to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    /// The record, with [`Bark::sinks`] still EMPTY: what each sink made of
    /// it is not known until it has been tried, and the loop fills that in
    /// before the record is written. A `Firing` carrying delivery outcomes
    /// would be claiming a delivery that has not happened.
    pub bark: Bark,
    /// The sink names it routes to.
    pub sinks: Vec<String>,
}
```

Cargo shape for this task: `-p shep-cli`.

**`on_poll` is not a second implementation of `on_event`.** It is the same rule set evaluated against a *level* rather than an *edge*, and the state that makes them agree is one map keyed by subject: last seen status, last seen restart count, and last firing time per rule. An `Errored` seen by either route fires once, because the firing is recorded against the subject and the debounce covers the other route. **This is the property Task 21's reconciliation test exercises**, and it is why the state lives here rather than in the loop.

**Debounce is per rule per subject.** A global debounce means the second sheep to go down during an incident is silent, which is the incident's most interesting fact.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// fails if the same `Errored` fires twice when both routes see it —
    /// once off the bus, once off the poll a second later. An operator
    /// paged twice for one outage stops trusting the page, and this is the
    /// shape reconciliation introduces the moment it exists.
    #[test]
    fn an_errored_seen_by_both_routes_fires_once() {
        let mut rules = gave_up_rules();
        let first = rules.on_event(&errored_event("web"), 1_000);
        assert_eq!(first.len(), 1);
        let second = rules.on_poll(&[errored_info("web")], 2_000);
        assert!(second.is_empty(), "the debounce covers the other route");
    }

    /// fails if the poll cannot fire what the bus never delivered — which
    /// is the entire reason bark polls at all.
    #[test]
    fn the_poll_fires_what_the_bus_never_carried() {
        let mut rules = gave_up_rules();
        let fired = rules.on_poll(&[errored_info("web")], 1_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].bark.subject, "web");
    }

    /// fails if the debounce is global rather than per subject. The second
    /// sheep to go down during an incident is the incident's most
    /// interesting fact, and a global debounce silences it.
    #[test]
    fn one_flapping_sheep_does_not_mute_another_going_down() {
        let mut rules = gave_up_rules();
        assert_eq!(rules.on_event(&errored_event("web"), 1_000).len(), 1);
        assert_eq!(rules.on_event(&errored_event("api"), 1_100).len(), 1);
        assert!(rules.on_event(&errored_event("web"), 1_200).is_empty());
    }

    /// fails if bark keeps its own restart tally. The shepherd's count is
    /// the number it acts on; a private one drifts, and the operator is
    /// told a different story from the one the supervisor believes. The
    /// fixture makes them DISAGREE — the info says 9, and bark has seen
    /// three events — so an implementation reading either one passes only
    /// if it reads the right one.
    #[test]
    fn the_early_warning_counts_the_shepherds_restarts_and_not_its_own() {
        let mut rules = restart_rate_rules(5, UpDuration::from_millis(60_000));
        for at in [1_000, 2_000, 3_000] {
            rules.on_event(&restart_event("web"), at);
        }
        let mut info = online_info("web");
        info.restarts = 9;
        let fired = rules.on_poll(&[info], 4_000);
        assert_eq!(fired.len(), 1, "9 restarts crosses a threshold of 5; 3 does not");
    }

    /// fails if a rule routing to a sink nobody defined is accepted. It
    /// would fire correctly for months and deliver nowhere, and the first
    /// time anyone finds out is the incident it was written for.
    #[test]
    fn a_rule_routed_at_a_sink_that_does_not_exist_is_refused_at_startup() {
        let err = Rules::new(vec![rule_to("pager")], &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, RulesError::UnknownSink { .. }));
        assert!(err.to_string().contains("pager"));
    }

    /// fails if `[dog.bark]` with sinks and no rules alerts on nothing.
    /// "The shepherd gave up" is on by default with nothing to tune — that
    /// is what makes it the alert that cannot be missed.
    #[test]
    fn a_bark_with_sinks_and_no_rules_still_alerts_when_the_shepherd_gives_up() {
        let sinks = one_sink("ops");
        let rules = Rules::default_rules(&sinks);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].when, Trigger::GaveUp);
        assert_eq!(rules[0].sinks, vec!["ops"]);
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** One `BTreeMap<String, SubjectState>` keyed by subject name; `Firing` built by a shared helper both routes call, so the record's shape cannot differ by route.

- [ ] **Step 4: Mutation check.** Make the debounce global (one timestamp per rule rather than per rule per subject) and watch `one_flapping_sheep_does_not_mute_another_going_down` redden. Then make `on_poll` tally its own restarts and watch `the_early_warning_counts_the_shepherds_restarts_and_not_its_own` redden — this second one is the check that the fixture's disagreement is real. Restore from a `cp` snapshot; paste both failures.

- [ ] **Step 5: CHANGELOG** — shep-cli: the rule kinds, the two restart-loop rules and why they are two, the default rule, and the per-subject debounce.

- [ ] **Step 6: Task gate, then commit** — `feat(cli): decide what is worth barking about`

---

## Task 21: bark's loop, and the drop it exists for

**Files:**
- Modify: `crates/shep-cli/src/dog/bark/mod.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Task 22:**

```rust
/// `[dog.bark]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BarkConfig {
    /// Named sinks.
    pub sinks: BTreeMap<String, Sink>,
    /// Named rules. Empty means [`Rules::default_rules`].
    pub rules: Vec<Rule>,
    /// How often the reconciliation poll runs when nothing has gone wrong.
    pub poll: UpDuration,
    /// Cap on `barks.jsonl`.
    pub history_bytes: u64,
    /// Per-delivery timeout.
    pub sink_timeout: UpDuration,
}

/// One source of bus events: a frame, or a notice that frames were lost.
///
/// A trait rather than a concrete `EventStream`, so the reconciliation test
/// can drive this loop from a REAL `tokio::sync::broadcast::Receiver` with
/// a small capacity and make the bus genuinely drop events. That is the
/// property bark exists for, and a test that subscribed and saw everything
/// would prove the fast path, which was never the risk.
///
/// `broadcast::Receiver` is not a stand-in for the production source; it is
/// what the shepherd's own bus IS (`shep_daemon::bus`), one process
/// boundary away.
pub trait EventSource: Send {
    /// The next event; `Err(count)` when the source dropped `count` frames
    /// before this one; `None` when it ends.
    fn next(&mut self) -> impl Future<Output = Option<Result<BusEvent, u64>>> + Send;
}

/// What bark reads the flock through, so the loop's poll is drivable
/// without a socket.
pub trait FlockSource: Send {
    /// The flock as it stands.
    fn flock(&self) -> impl Future<Output = Result<Vec<ProcessInfo>, RequestError>> + Send;
}

/// Bark's loop: subscribe for speed, poll for correctness.
///
/// **A dropped frame polls immediately** rather than waiting for the next
/// interval. The bus is a `tokio::sync::broadcast`, so a lagging subscriber
/// has events DROPPED rather than queued; for `shep bleats` that is a
/// cosmetic notice, and for alerting it is a missed page. The subscription
/// is what makes bark fast; the poll is what makes it correct; and the
/// moment a drop is reported is exactly when correctness is in question.
pub async fn run_loop<E: EventSource, F: FlockSource>(
    events: E,
    flock: F,
    rules: Rules,
    config: &BarkConfig,
    barks_path: &Path,
) -> ExitCode;
```

Cargo shape for this task: `-p shep-cli`.

**`BarkConfig` needs a hand-written `Default`**, not a derived one: `#[serde(default)]` on the struct requires one, and a derived impl gives `poll = 0`, `history_bytes = 0` and `sink_timeout = 0` — a bark dog that polls in a hot loop, keeps no history and times every delivery out instantly. Write the four defaults with the reasoning for each beside it (`poll` at 30s, `history_bytes` at [`shep_core::barks::DEFAULT_MAX_BYTES`], `sink_timeout` at 10s), and pin them with a test that parses an empty section — the same shape Task 15's `the_default_bind_is_loopback` uses, and for the same reason: an empty `[dog.bark]` is the ordinary case.

**The reconciliation test is the one this whole subsystem is built around.** It must make the bus *actually* drop, not simulate a drop:

1. `let (tx, rx) = tokio::sync::broadcast::channel(4);` — a real channel with a real, small capacity.
2. Send far more than four events before the loop reads any, so tokio's own overflow drops the early ones. The `errored` event for `web` is among the dropped.
3. Drive the loop with `rx` as its `EventSource` and a `FlockSource` that answers a listing in which `web` is `Errored` with 16 restarts.
4. Assert: the loop's first `flock()` call happens *because of the lag*, and the `GaveUp` bark for `web` is written to `barks.jsonl` and delivered to the sink.

Step 2 needs care: `broadcast::Receiver` only lags when the sender outruns it, so the sends must complete before the receiver is polled. Send them all synchronously before `run_loop` is spawned. **Assert that the drop really happened** — a first `next()` returning `Err(count)` with `count > 0` — rather than assuming it; a channel capacity the test does not actually overflow makes the whole case vacuous, and that is exactly the "fixture that cannot distinguish right from wrong" shape this project has shipped before.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// THE test this dog exists for. fails if the poll is only ever driven
    /// by its interval: `web`'s `errored` frame is genuinely dropped by a
    /// real broadcast channel, so a loop that reconciles on a timer alone
    /// stays silent for the whole poll interval — and under a paused clock,
    /// forever.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_frame_makes_bark_poll_and_catch_up() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        for i in 0..64 {
            tx.send(log_event(i)).unwrap();
        }
        tx.send(errored_event("web")).unwrap();

        // The drop is real, or this test proves nothing.
        assert!(
            matches!(rx.recv().await, Err(broadcast::error::RecvError::Lagged(n)) if n > 0),
            "the fixture must actually overflow the channel"
        );

        let (tx2, rx2) = tokio::sync::broadcast::channel(4);
        for i in 0..64 {
            tx2.send(log_event(i)).unwrap();
        }
        tx2.send(errored_event("web")).unwrap();

        let (addr, captured) = one_shot_sink(200, "").await;
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");
        let flock = ScriptedFlock::answering(vec![errored_info("web", 16)]);

        let loop_handle = tokio::spawn(run_loop(
            rx2,
            flock.clone(),
            gave_up_rules(),
            &config_with_sink(addr, &barks_path),
            &barks_path,
        ));

        let req = tokio::time::timeout(Duration::from_secs(5), captured)
            .await
            .expect("a dropped frame must produce a delivered bark")
            .unwrap();
        assert!(String::from_utf8_lossy(&req.body).contains("web"));

        let recorded = shep_core::barks::read(&barks_path).unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].subject, "web");
        assert_eq!(recorded[0].sinks[0].error, None);

        assert_eq!(
            flock.calls(),
            1,
            "the poll ran because of the lag, not because an interval elapsed \
             — the clock is paused, so no interval has"
        );

        loop_handle.abort();
    }

    /// fails if a sink that refuses the delivery costs the record. The
    /// local trail is what an operator reads when the page never arrived,
    /// and it is most valuable exactly when the sink is the thing that
    /// broke.
    #[tokio::test]
    async fn a_bark_is_recorded_even_when_every_sink_refuses_it() { /* one_shot_sink(500, ..) */ }

    /// fails if a slow sink stalls the loop. Discord's rate limit is
    /// measured in seconds, and a bark dog that stops reading the bus while
    /// it waits starts DROPPING the frames it exists to catch — the loop
    /// would cause the exact fault it is built to survive.
    #[tokio::test(start_paused = true)]
    async fn a_slow_sink_never_stalls_the_loop() { /* ... */ }
```

The paused clock in the first test is load-bearing rather than a speed trick: it makes "the poll ran because of the lag" *checkable*, because no interval can have elapsed.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** A `tokio::select!` over the event source, a poll interval, and a shutdown signal (`ctrl_c` and SIGTERM, as Task 15). A lag arm that polls immediately. **Deliveries are spawned, never awaited inline**, and the resulting `Bark` is written after delivery so `SinkOutcome` is honest — which means the record's write is inside the spawned task, and `barks.jsonl` is appended to from several tasks at once. `barks::append` is a read-modify-rename, so serialize those writes behind a `tokio::sync::Mutex` held only for the append; say in the report that the lock exists and what it covers.

- [ ] **Step 4: Mutation check.** Delete the lag arm's immediate poll — leaving the lag logged and nothing else — and watch `a_dropped_frame_makes_bark_poll_and_catch_up` fail **by timing out on the sink**, which under a paused clock it will do at the `tokio::time::timeout` ceiling rather than hanging. Confirm it is the named `expect` that fires and not a hang. Restore from a `cp` snapshot; paste the failure.

- [ ] **Step 5: Wire `run_dog`.** `dog/mod.rs`'s bark branch parses `BarkConfig` through `DogRuntime::config`, builds `Rules`, subscribes with `client.subscribe(vec!["process.*".into()])`, and drives `run_loop`. `EventSource` is implemented for `shep_client::EventStream` here (its `next` already yields `Option<Result<BusEvent, Lagged>>`, so the impl is a `map_err` over `Lagged::count`) and for `broadcast::Receiver<BusEvent>` in the test module.

- [ ] **Step 6: CHANGELOG** — shep-cli: the bark dog, the subscribe-plus-poll shape, and that a dropped frame polls immediately.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): bark when the shepherd gives up`

---

## Task 22: `shep barks`, and both dogs against a real daemon

**Files:**
- Modify: `crates/shep-cli/src/commands/dogs.rs`, `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`, `crates/shep-cli/src/output/rows.rs`
- Modify: `crates/shep-cli/tests/cli_e2e.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced:**

```rust
/// `shep barks`: the alert history, newest last.
pub fn barks(streams: &mut Streams<'_>, fmt: Format, paths: &ShepPaths, args: &BarksArgs) -> ExitCode;

/// `Vec<Bark>` — the alert history.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct BarkRows(pub Vec<Bark>);
```

Cargo shape for this task: `-p shep-cli`. `cargo test -p shep-cli --bins`, then `cargo test -p shep-cli --test cli_e2e`.

`shep barks` reads the file directly and **never connects to the shepherd**. The history is on disk precisely so it survives the shepherd — an operator reading it after a crash is the case it exists for, and a verb that needed a running daemon to show why the daemon died would be the wrong shape entirely. Same precedent as `shep flush --daemon`, which also works on files rather than through the socket.

`BarksArgs { pub tail: Option<usize> }` — `--tail N` shows the last N. Newest **last**, not first: the file is a log and a reader scrolling to the bottom of a terminal expects the most recent line there, which is also what `tail` itself does.

Columns: `WHEN`, `RULE`, `SUBJECT`, `MESSAGE`, `SINKS`. `WHEN` renders the millis as a local timestamp; `SINKS` renders `ops` for a delivered one and `ops(failed)` for a refused one, so the failure is visible in the table an operator is already reading rather than only in the JSON.

### The e2e case

This is where the phase's success criterion is actually checked, against the real binary, a real daemon and a real dog process.

- [ ] **Step 1: Write the failing tests.** In `cli_e2e.rs`, following the file's existing harness (read `saving_the_roll_then_mustering_reports_the_same_flock` first — it shows how a real daemon is started, torn down, and asserted against):

```rust
    /// The phase's own success criterion, at the only tier that can check
    /// it: a real binary, a real shepherd, and a real dog PROCESS spawned
    /// by that shepherd. Every tier below this one scripts the runner, so
    /// none of them has ever exec'd `shep dog metrics` — and an argv branch
    /// that does not exist fails at exec, which no unit test can see.
    ///
    /// fails if the dog is not spawned, if it cannot reach the socket from
    /// the one variable it inherits, if it cannot fetch its own section, or
    /// if it cannot bind and serve. Those four are the whole contract.
    #[test]
    fn a_real_shepherd_runs_a_real_metrics_dog_that_answers_a_scrape() {
        // ... write a shep.toml with `[dog.metrics] bind = "127.0.0.1:<free>"`,
        // start a daemon, `shep start` one sheep, `shep enable metrics`,
        // poll the endpoint under a bounded deadline, assert the exposition
        // names the sheep and carries `shep_dog_up{dog="metrics"...} 1`.
        // Tear down with the harness's own Drop guard: this test spawns a
        // GRANDCHILD process, and a leaked metrics dog holds a port for
        // every later run on the machine.
    }

    /// fails if `shep dogs` renders the sheep, or `shep flock` renders the
    /// dogs into the sheep table. The two-table split has unit coverage;
    /// what this adds is that the real verbs are wired to the real
    /// renderers, which is the gap that let a verb point at the wrong
    /// handler unnoticed workspace-wide.
    #[test]
    fn dogs_and_flock_render_the_two_populations_the_right_way_round() { /* ... */ }

    /// fails if `shep barks` needs a shepherd. The history is on disk so it
    /// outlives the daemon, and the case it exists for is an operator
    /// reading it after a crash.
    #[test]
    fn barks_reads_the_history_with_no_shepherd_running() { /* ... */ }
```

Every one of these owns a teardown guard. The metrics case spawns a grandchild — the daemon spawns the dog — so the guard must kill the *process group*, not the daemon's pid. `cli_e2e.rs` already has this shape for the daemon; extend it rather than adding a second.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement** `shep barks`, `BarkRows`, the clap variant, the dispatch arm and its parse test.

- [ ] **Step 4: Confirm nothing is left behind.** After `cargo test -p shep-cli --test cli_e2e`, check for a surviving dog:

```
pgrep -af 'shep dog' ; echo "exit=$?"
```

Expect no match. Paste the output. Then force one deliberate panic inside the metrics e2e case and confirm the guard still reaps — a green suite never exercises the teardown its guards govern, which is Phase 8's exit criterion 18 applied to a process this phase newly spawns.

- [ ] **Step 5: CHANGELOG** — shep-cli: `shep barks` and its `--tail`.

- [ ] **Step 6: Task gate, then commit** — `feat(cli): show the bark history`

---

## Task 23: the docs, the spec, and the phase gate

**Files:**
- Create: `docs/dogs.md`
- Modify: `docs/specs/shep-v1.md` (§8), `docs/specs/deferred.md`, `docs/terminology.md`, `docs/systematic-refactor/refactor-workspace/map.md`, `README.md` if it lists verbs
- Modify: every crate CHANGELOG this phase left unreconciled

No cargo shape: this task writes documentation. The gate still runs, because `RUSTDOCFLAGS="-D warnings" cargo doc` covers intra-doc links.

- [ ] **Step 1: `docs/dogs.md`.** Public-facing prose, so **invoke the `humanizer` skill** before it is final, and match the register of `docs/shepherd-channel.md` — this repo's existing document for someone writing code against shep. Sections:

  1. **What a dog is** — a process speaking the client wire protocol, supervised by the shepherd, marked as a dog. `PROTOCOL_VERSION` covers a dog exactly as it covers `shep flock`.
  2. **Turning one on** — `enable`/`disable` for the two that ship inside the binary; that `enable` starts it now when a shepherd is running and arms it for the next boot when none is.
  3. **Configuration** — `[dog.<name>]`, that it travels over the socket rather than the environment, and **the rule stated plainly**: a config change does not reach a running dog, and `shep disable X && shep enable X` is what re-reads it.
  4. **The metrics dog** — the metric table from Task 14, the default loopback bind, and what widening it exposes.
  5. **The bark dog** — sinks, rules, the two restart-loop rules and why they are two, the per-subject debounce, and `barks.jsonl`.
  6. **Writing your own** — `adopt`/`rehome`, the wire a third-party dog speaks, the one variable it inherits, and the trust level it runs at: **the daemon's own, with no sandboxing beyond it.** State it rather than implying it. It is the same trust a sheep already has, which is the honest comparison and the reason it is acceptable.
  7. **When a dog dies** — the shepherd's local record, the metrics health gauge, and why nothing watches across dogs.

- [ ] **Step 2: Amend spec §8.** Three departures from what it says today, each recorded the way §9's `trigger` amendment is recorded — **what was decided and why the reasoning matters to a later reader**, not just the corrected sentence:
  - dogs are **not** hidden behind `--all`; they get their own table, shown by default;
  - `enable --exec` is a hidden alias and `adopt`/`rehome` are the verbs;
  - a dog's configuration reaches it over the socket, and the reason is secrets.

- [ ] **Step 3: `docs/specs/deferred.md`** — reconcile the whole file against the tree, not only the dogs entry. The dogs entry goes; the `enabled_dogs`/`[dog.<name>]`-have-no-reader sentence goes with it. Check the `shep web` line, which is conditional on "only if the metrics dog turns out not to cover it" — say whether it does. `lookout`, `whistle`, `serve`, `dev`/`runtime` and the rest stay and must still read true.

- [ ] **Step 4: `docs/terminology.md`** — the plugin row's example column still says `shep enable --exec <path> <name>` as the third-party spelling; it is now the hidden alias, and `adopt` is the verb. The `adopt`/`rehome` rows are already right.

- [ ] **Step 5: `map.md`** — verify every claim against the code before writing it, and **cite by symbol, not line number**. That file has twice been synced to what a plan expected rather than what shipped.

- [ ] **Step 6: Reconcile every CHANGELOG** (IR-45). Each entry describes what an operator or an API consumer sees, not which task produced it (Rule 10). The user-visible headlines: dogs are supervised processes with their own table; `enable`/`disable`/`adopt`/`rehome`/`dogs`/`barks`; the metrics dog; the bark dog.

- [ ] **Step 7: Full phase gate** — the four task gates, **plus** the serial run and both bench-crate gates:

```
cargo test --workspace --all-features -- --test-threads=1
cargo bench --manifest-path benches/Cargo.toml -- --test
cargo clippy --manifest-path benches/Cargo.toml --all-targets -- -D warnings
```

The serial run is not ceremony: it was red on `main` before Phase 5 and caught a real regression in Phase 6. The bench gates are here rather than in an individual task because `benches/` names only `shep_daemon::limits::sample`, which nothing in this phase touches — confirm that with `grep -rn "use shep" benches/benches/` and say so, rather than running them on faith.

- [ ] **Step 8: The marker tripwire, run and pasted.**

```
grep -rn "dog" crates/shep-daemon/src/kill.rs crates/shep-daemon/src/backoff.rs \
                crates/shep-daemon/src/runner.rs crates/shep-daemon/src/tokio_runner.rs \
                crates/shep-daemon/src/snapshot.rs
grep -n "dog" crates/shep-daemon/src/supervisor.rs
```

The first must find nothing. Every hit in the second must answer *where did this come from* or *who should see this*: the `StartDog` command and its arm, `start_dog`, the marker's parameter and field, `to_info`'s read, `matching_ids`' filter, tests. **A hit answering *how is this supervised* — a different kill ladder, backoff curve, restart budget, or meaning for `Errored` — is the design's own warning that the separate registry should have been built**, and it is a finding for Rin rather than something to quietly keep.

- [ ] **Step 9: Report to Rin** — every judgement call made on her behalf, anything left unfixed, and specifically:
  - whether the tripwire grep in step 8 came back clean, quoted in full;
  - which snapshot deltas moved, and confirmation that each was only its own task's addition;
  - the measured cost of the phase's slowest new test, and whether the e2e tier left any process behind.

- [ ] **Step 10: Commit** — `docs: record what a dog is and what it costs`

---

## Exit criteria

1. All twenty-three tasks complete and individually reviewed.
2. Every gate green **from its own exit code**, including both bench-crate gates and the serial run. `Running`/`Doc-tests` lines counted against `test result:` lines, starting from the baseline Task 1 recorded.
3. **The marker never reaches supervision.** Task 23's step-8 grep is clean: no `dog` in `kill.rs`, `backoff.rs`, `runner.rs`, `tokio_runner.rs` or `snapshot.rs`, and every hit in `supervisor.rs` answers *where did this come from* or *who should see this*. A hit answering *how is this supervised* is the warning, and it is reported rather than kept.
4. A dog is supervised by the ordinary machinery: a crash restarts it, a crash loop exhausts the same budget a sheep gets, and the restarted process is still a dog. Pinned by a test that fails if the marker is written by the start path rather than carried by the entry.
5. `shep flock` prints two tables and `--format json` prints one array. A flock with no dogs prints exactly what it printed before this phase — no caption, no empty second table.
6. A wildcard selector never reaches a dog, and an exact one always does. Both halves pinned; the second is what makes `shep disable` work at all.
7. **A dog's configuration never travels in its environment.** Pinned against the ASSEMBLED spawn spec, not the config, because `assemble` is where an env map would actually be merged.
8. `Request::DogConfig` answers the file as it stands, not a copy cached at boot — pinned by a test that writes the section *after* the context exists.
9. **Third-party `adopt` covers all three failure modes** — path missing, not a file, not executable — plus a binary this kernel refuses to exec, each reported as itself and none as another. A refused adopt leaves `shep.toml` byte-identical.
10. `shep.toml` has one writer, and it preserves comments and key order. A file that will not parse is refused, never replaced.
11. **`barks.jsonl`'s eviction is tested, not just its append.** The cap is reached, the oldest record is the one that goes, and the surviving subjects are named rather than counted.
12. **The reconciliation test makes the bus actually drop events.** A real `broadcast::channel` overflows, the test asserts the drop happened before asserting anything about it, and the catch-up bark is delivered to a local sink and recorded. Under a paused clock, so "the poll ran because of the lag" is checkable rather than assumed.
13. Bark reads the shepherd's own `restarts` count, pinned by a fixture where the shepherd's number and a private tally would disagree.
14. **Sinks are tested against a local HTTP server and never a real webhook.** No test in this phase resolves `discord.com` or `slack.com`.
15. `Sink`'s `Debug` never prints a webhook URL, pinned by an exact-string test. The same holds for `ShepTomlError` and `DogRunError`.
16. The metrics dog binds loopback by default, pinned on both the `Default` impl and an empty parsed section; a shepherd that will not answer produces a 503 rather than a 200.
17. Every metric the Grafana dashboard queries is one the exposition renders, checked name by name.
18. `PROTOCOL_VERSION` is still **1**, and **each** regenerated snapshot's delta is pasted verbatim in its task's report and is only that task's addition.
19. **Both halves of the marker grep**: files this phase creates, *and* lines it adds to files it only modifies, are free of task-relative phrasing. Phase 4 skipped the second half and a marker shipped.
20. **Every test added carries a "fails if" comment naming the mutation it catches, and the mutation was actually performed and watched to fail before the comment was written.** Seven tests last phase were caught unable to fail. A reviewer picking three at random must be able to break the implementation in the named way and watch the named test redden.
21. No test added in this phase can hang: every await of a daemon answer is bounded, every negative assertion polls a bounded window, and every HTTP read has both a size and a time ceiling.
22. **Nothing is left running.** `pgrep -af 'shep dog'` finds nothing after the suite, and the e2e teardown was calibrated by forcing one deliberate panic — a green suite never exercises the guards it depends on.
23. `docs/specs/deferred.md` matches the tree: the dogs entry is gone, the sentence about `enabled_dogs` having no reader is gone, and nothing left in the file claims something the tree no longer matches.
