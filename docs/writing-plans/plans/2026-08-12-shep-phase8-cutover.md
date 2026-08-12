# shep Phase 8 — the pm2 cutover set Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this plan task by task. Steps use `- [ ]` for tracking.
>
> **REQUIRED SUB-SKILL:** invoke `shep-idiomatic-rust` before writing or reviewing any Rust here. Cite rules as `IR-<n>`.

**Goal:** make it possible to retire pm2 on a real machine. Four components, each usable on its own the moment its last task lands: `shep save`/`shep muster`, `shep import`, `shep startup`/`shep unstartup`, and inline CPU/memory in `shep flock`.

**Success criterion** is spec §13.4: `shep import && shep save && reboot` leaves the flock running, started by the init system rather than by a login shell. It needs a reboot, so it is a documented manual runbook (Task 16), not a CI job.

**Architecture:** nothing here is a new subsystem. `save`/`muster` put an operator in front of `snapshot.rs`, which already writes and restores the roll. `import` is a pure JSON→`AppConfig` transformation plus a TOML renderer, entirely inside shep-cli. `startup` is a pure unit/plist renderer plus a privilege gate and two `std::process::Command` calls. Stats splits the sampling half of `PollingEnforcer`'s existing 15s loop away from the enforcement half and hands the reading to the RPC layer.

**Tech stack:** no new *workspace* dependency. shep-cli gains two dependencies the workspace already pins: `toml` (rendering the Flockfile) and `nix` (`geteuid`, `User::from_name` — already a `cfg(unix)` dev-dependency there). `sd_notify` is `std::os::unix::net::UnixDatagram` and nothing else.

---

## Global constraints

Every task implicitly includes these.

- **Never open, read, or reference `/Users/rin/GitHub/pm2`.** Clean-room. Everything about pm2's dump comes from `docs/brainstorming/specs/2026-08-12-shep-phase8-cutover-design.md`, which a dedicated design phase produced. `/Users/rin/GitHub/rand` is the style reference and may be read freely.
- MSRV **1.88**, edition **2024**. Workspace lints deny `missing_docs` and `missing_debug_implementations`; `clippy::undocumented_unsafe_blocks` and `clippy::missing_errors_doc` are `deny`.
- `#![forbid(unsafe_code)]` in shep-core/shep-client/shep-cli; shep-daemon is `#![deny(unsafe_code)]` with the one `#![allow]` in `sys.rs`, where each block needs its own `// SAFETY:` (IR-22/23). **Nothing in this phase needs unsafe** — if a task reaches for it, that is the signal to stop and re-read the task.
- **Rule 10:** no task-relative phrasing in shipped comments or docs. Name the thing, never "Task 5", "this phase", "the new field".
- CHANGELOG entries (IR-45) **reconciled, not appended**, in the crate whose surface changed. Folded into the task whose deliverable needs them, never batched at the end.
- **`PROTOCOL_VERSION` stays 1.** Every wire addition is additive under `#[non_exhaustive]`. Any regenerated insta snapshot's delta must be **read and verified to be only the addition**, and pasted verbatim into the task report — a regenerated `.snap` is the easiest place in a diff to hide a change nobody re-derives.
- Terminology per `docs/terminology.md`: `save` writes the muster roll, `muster` assembles the flock from it; sheep (singular), flock (plural), fold, bleats. Destructive ops and error text stay plain English.
- Commit style: conventional commits, footer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`. A commit message containing backticks uses `git commit -F -` with a **quoted** heredoc.

### The inner loop

```
cargo test -p shep-daemon --lib --all-features -- --skip watch:: --skip extras::
```

**Measured 2026-08-12: 1.33s, 333 passed, 79 filtered out.** The 79 it skips are FSEvents-bound and cost 281s of CPU between them. (`CLAUDE.md` says 316/395; that is stale — the true figures are 333/412.)

**Tasks 13 and 14 change `extras.rs` and must not use that loop as written**, because `--skip extras::` hides exactly the tests they break. Those two tasks use:

```
cargo test -p shep-daemon --lib --all-features extras:: -- --skip watch::
```

**Measured: 17.63s, 44 passed** for the unfiltered `extras::` set. Slower than the inner loop and still 20× cheaper than the gate.

For shep-cli work: **`shep-cli` is `[[bin]]`-only.** `-p shep-cli --lib` errors with "no library targets". Use:

```
cargo test -p shep-cli --bins
```

For shep-core work: `cargo test -p shep-core --lib`.

### The task gate — once, when a task is otherwise done

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Each from **its own command** with `$?` captured directly, **never through a pipe**: this is zsh, a pipeline's `$?` is the last command's, and `${PIPESTATUS[0]}` is an empty string. That has produced a false green six times on this project.

**One cargo command shape per task.** The workspace shares one target-dir build lock, so concurrent runs block rather than parallelise, and alternating `-p` with `--workspace` invalidates every crate whose feature set changed. Each task below states its shape; do not "helpfully" add the other. `benches/` is its own workspace with its own lock and may run alongside.

### Baseline

Measured at `7b5fd76` (`main`), 2026-08-12, `cargo test --workspace --all-features`:

**793 passed, 1 pre-existing ignored (`a_dropped_child_runs_as_the_requested_user`, needs root), 15 targets against 15 `test result:` lines.** Count `Running`/`Doc-tests` lines against `test result:` lines rather than reading a green tail — `cargo test` stops at the first failing binary, so a red suite can read as a short green one.

### Test discipline

- **Every test carries a "fails if" comment naming the mutation it catches, and the mutation must actually be performed and watched to fail before the comment is written.** This project has shipped five tests naming a bug they could not catch. A reviewer picking three at random must be able to break the implementation in the named way and watch the named test redden.
- **Every test that awaits a daemon answer bounds its read.** Two mutations in Phase 7 *hung* the suite instead of reddening it, because a wait nothing drives home never resolves. `tokio::time::timeout(..) + recv`, never a bare `recv().await`, and never a bare `try_recv` for a negative assertion (Phase 6's Rule 11).
- **`to_child.send()` returning `Ok` is not delivery.** The first send after a child dies returns `Ok(())` and vanishes. No test may assert delivery from a successful send.
- **Fixtures are synthesised.** A real pm2 dump carries absolute paths, a live SSH session's environment, and a production host's layout. Never derive a fixture from one — the fixture in Task 6 is written by hand from the documented shape.
- Paused tokio clock by default, no sleeps, hand-rolled fakes, unique fixtures per test (IR-33/34).

---

## Settled decisions

Recorded so no task re-litigates them. Items marked (Rin) come from the approved design spec; the rest follow from it or from existing precedent and are mine — flag any you believe is wrong rather than working around it.

| # | Decision |
|---|---|
| 1 | (Rin) **`shep save` replies with the path written and the number of apps recorded.** A save that silently does nothing is the failure mode the verb exists to rule out. |
| 2 | **`save` against a stopped engine is an RPC error, never a silent success.** `RpcContext::snapshot_now` returns `Ok(())` when `list_checked` fails, which is right for teardown and a lie for an operator. Task 2 splits the two. |
| 3 | **`Response::Mustered(Vec<ProcessInfo>)` lists every sheep of every app the roll restored**, not only the ones this call spawned. `do_start` is idempotent through `instance_slots`, so a `muster` right after an autostart legitimately spawns nothing — and reporting an empty list there would read as "the roll was empty". |
| 4 | **An empty `Mustered` gets an explicit stderr notice from the CLI.** Same shape as decision 1, one layer up: a muster that restores nothing must say so rather than print an empty table. |
| 5 | **One restore path, shared.** `boot::restore_flock` and the `Muster` handler call the same function in `snapshot.rs`. Two copies of "read the roll, re-validate, record, start" drift, and the one that drifts is the one nobody reboots to test. |
| 6 | (Rin) **`import` reads the dump only**, writes a Flockfile, and **starts nothing**. No `ecosystem.config.js` overlay: reading it faithfully means evaluating JavaScript. |
| 7 | **A dump row is flat; fields are read from the row itself.** Measured against a real dump on 2026-08-12, after this plan was drafted: no `pm2_env` key exists, `name`/`pm_exec_path`/`cwd` sit at the row's top level (71 keys), and the `env` dict duplicates the top-level string keys exactly — all 31 of them. The `pm2_env` fallback the plan originally carried is therefore dead code and is removed. A row with no `pm_exec_path` is still a **loud, named failure** carrying the keys it did find, not a skipped row: that guard is what would catch a dump shape this measurement did not cover. |
| 8 | (Rin) **Declared env only, by construction.** The declared env is the union of the row's `env_<name>` maps — by construction those hold only what the ecosystem file declared. A key in `env` that is neither declared, nor a named session-junk pattern, nor pm2-injected is **named in the output and not written**. The operator decides; the evidence is in front of them. |
| 9 | **The pm2-injected list being incomplete is safe by construction.** An injected key we do not recognise lands in the "named, not written" bucket — noise, never a silent wrong config. This is why the list may stay short rather than being guessed at length. |
| 10 | **`NODE_APP_INSTANCE` becomes `increment_var`, never a literal env value.** Copying it verbatim would pin instance 0's value into every instance. Spec §14.8 assigns this mapping to the importer. *Beyond the design spec's field table — flagged for Rin.* |
| 11 | (Rin) **`exec_mode: cluster_mode` → `instances = N` + `reuse_port = true`, and `import` says so at import time**, naming each affected app and the `reusePort` requirement it carries. shep binds nothing; four processes on one port is `EADDRINUSE`, and it must not surface as a bind failure at first start. |
| 12 | **`import --dry-run` writes the rendered Flockfile to stdout with no envelope.** Precedent: `shep completions`, which prints a raw shell script the same way (`commands/completions.rs`), and `bleats`, whose module doc already records a verb opting out of the envelope with a stated reason. `shep import --dry-run > Flockfile.toml` must produce a byte-exact file. |
| 13 | **Import notes go to stderr, one line each, in both modes and both formats.** One rule, no conditionals, and the report the design spec requires is present regardless of `--format`. |
| 14 | (Rin) **`ExecStart` is the daemon, not `shep muster`.** Under `Type=notify` systemd supervises the process it starts. The restore still happens — the daemon already restores the roll at boot — so spec §13.4 describes the effect rather than the literal argv, and Task 16 rewords it. |
| 15 | (Rin) **`--foreground` is a deliberate second path**, not a flag that quietly changes the existing one. It names the init-supervised path so a generated unit never depends on the private contract of the bare hidden `daemon` verb, and it is what enables the readiness notification. |
| 16 | (Rin) **`sd_notify` fires after the muster restore completes**, at the end of `boot()`. That is the entire point of `Type=notify`: the unit goes green when the flock exists, and a hung restore becomes a failed start instead of a green unit supervising nothing. |
| 17 | **sd_notify needs no dependency and no unsafe.** `UnixDatagram::unbound()` + `send_to(path)` covers a filesystem `$NOTIFY_SOCKET`; `std::os::linux::net::SocketAddrExt::from_abstract_name` + `UnixDatagram::send_to_addr` covers an `@`-prefixed abstract one. Both stable since 1.70, under the 1.88 floor. |
| 18 | (Rin) **`startup` installs and enables when privileged; otherwise it prints exactly the command to run and exits non-zero** so a script notices. shep never escalates on its own. |
| 19 | **`startup` refuses when the resolved `$SHEP_HOME` does not exist.** Under `sudo shep startup`, `$HOME` is root's, so the unit would carry `/root/.shep` and restore nothing after a reboot — silently. The target user comes from `--user`, else `$SUDO_USER`, else the invoking user; the home comes from an explicit `--home`/`$SHEP_HOME`, else that user's passwd home. |
| 20 | **System-level units only** (Rin, design assumptions): a systemd system unit and a launchd `LaunchDaemon`. User units trade one root step for `loginctl enable-linger` plus a failure mode where the flock silently does not return. |
| 21 | (Rin) **One 15s tick, two consumers.** Sampling splits from enforcement inside the loop that already exists; a second loop would double a measured 5.77ms syscall walk for nothing. |
| 22 | (Rin) **The CPU baseline is written only by the periodic tick, never by an on-demand read.** Two `flock` calls a moment apart would otherwise divide by a near-zero window. This bounds the window to ≤15s and keeps it away from zero. |
| 23 | (Rin) **A sheep with no baseline reports `-`.** A process spawned since the last tick has no honest CPU number, and inventing one from a 50ms window is worse than an empty cell. |
| 24 | **The on-demand sample runs in the RPC layer under `spawn_blocking`, never in the actor.** `SysinfoSampler::sample` is a measured 5.77ms blocking syscall walk (`benches/benches/memory_sample.rs`, 883 host processes); the actor must never block, and a tokio worker thread should not either. |
| 25 | **`ProcessInfo` loses `Eq`.** `cpu_percent: Option<f32>` cannot derive it. Nothing in the workspace requires `Eq` on `ProcessInfo` (verified by grep); `PartialEq` stays, so every `assert_eq!` still compiles. It is a public API change and gets a CHANGELOG entry. |

---

## File structure

| File | Create / Modify | Responsibility |
|---|---|---|
| `crates/shep-core/src/protocol/request.rs` | modify | `Request::SaveRoll`/`Muster`, `Response::RollSaved`/`Mustered`, `ProcessInfo::cpu_percent`/`memory_bytes` |
| `crates/shep-daemon/src/snapshot.rs` | modify | `SavedRoll`, `muster` (the one restore path) |
| `crates/shep-daemon/src/rpc.rs` | modify | `save_roll_now`, the `SaveRoll`/`Muster` arms, fresh stats on `ListFlock`/`Describe` |
| `crates/shep-daemon/src/boot.rs` | modify | `restore_flock` delegates; `BootOptions::notify_ready`; the notify call site |
| `crates/shep-daemon/src/notify.rs` | **create** | `READY=1` to `$NOTIFY_SOCKET`, safe std only |
| `crates/shep-daemon/src/testing.rs` | modify | `AnnouncingRunner`, the scripted sampler's new field, the harness's optional sampler |
| `crates/shep-daemon/src/limits/stats.rs` | **create** | `StatsState`: watched roots, CPU baselines, on-demand sampling |
| `crates/shep-daemon/src/limits/sample.rs` | modify | `ProcessRss::cpu_ms`, `TreeIndex::cpu_from`, `.with_cpu()` |
| `crates/shep-daemon/src/limits/mod.rs` | modify | the tick updates baselines as well as enforcing |
| `crates/shep-daemon/src/extras.rs` | modify | every sheep with a pid is watched for stats |
| `crates/shep-cli/src/cli.rs` | modify | `save`, `muster`, `import`, `startup`, `unstartup`, `daemon --foreground` |
| `crates/shep-cli/src/main.rs` | modify | dispatch arms + their unit coverage |
| `crates/shep-cli/src/commands/muster.rs` | **create** | `save` and `muster` |
| `crates/shep-cli/src/commands/import/mod.rs` | **create** | the verb |
| `crates/shep-cli/src/commands/import/dump.rs` | **create** | reading `dump.pm2` |
| `crates/shep-cli/src/commands/import/convert.rs` | **create** | collapsing and field mapping |
| `crates/shep-cli/src/commands/import/env.rs` | **create** | declared / junk / ambiguous |
| `crates/shep-cli/src/commands/import/render.rs` | **create** | `AppConfig` → Flockfile TOML |
| `crates/shep-cli/src/commands/import/testdata/dump.pm2.json` | **create** | the synthesised fixture |
| `crates/shep-cli/src/commands/startup/mod.rs` | **create** | the verb, the privilege gate |
| `crates/shep-cli/src/commands/startup/unit.rs` | **create** | systemd unit + launchd plist rendering |
| `crates/shep-cli/src/output/rows.rs` | modify | `SavedRollRow`, `ImportRows`, `StartupRows`, CPU/MEM columns |
| `crates/shep-cli/src/output/table.rs` | modify | `human_bytes` |
| `docs/migration.md` | **create** | the pm2 → shep guide and the §13.4 runbook |

---

## Task 1: `save` on the wire

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs`
- Modify: `crates/shep-core/CHANGELOG.md`

**Interfaces — produced, and depended on by Tasks 2, 3:**

```rust
pub enum Request {
    // ... existing variants unchanged ...
    /// Write the muster roll now, bypassing the snapshot writer's debounce
    SaveRoll,
}

pub enum Response {
    // ... existing variants unchanged ...
    /// Answer to `SaveRoll`
    RollSaved {
        /// Absolute path of the roll the daemon wrote
        path: String,
        /// How many apps that roll records
        apps: u32,
    },
}
```

Cargo shape for this task: `-p shep-core`.

`path` is a `String`, not a `PathBuf`, for the reason `ProcessInfo::out_file`'s own comment gives at length: serde's `PathBuf` impl **refuses** a non-UTF-8 path, and that refusal aborts the whole `Reply` rather than one field. Every path already on this wire travels as a string.

`apps` is a `u32`, matching `SavedApp::instances_running`'s width in `snapshot.rs`.

- [ ] **Step 1: Write the failing tests.** In `request.rs`'s existing `mod tests`, extend `request_wire_snapshots` with one more envelope and `reply_wire_snapshots` with one more reply, then add the round-trip case:

```rust
            // The first fieldless verb added since `Ping`/`ListFlock`, and
            // pinned for that reason: a fieldless variant serializes as a
            // bare `{"kind":"..."}` with no `selector` key at all, so a
            // reader comparing this row against `stop`'s sees the whole
            // difference between the two shapes in one place.
            Envelope {
                id: 9,
                deadline_ms: None,
                body: Request::SaveRoll,
            },
```

```rust
            // The only struct-shaped `Response` variant, so the one worth
            // pinning here: every other variant on this wire is a newtype
            // over a Vec or a unit, both shapes already proven above.
            Reply {
                id: 5,
                result: Ok(Response::RollSaved {
                    path: "/home/rin/.shep/flock.json".to_string(),
                    apps: 2,
                }),
            },
```

```rust
    /// fails if `SaveRoll` or `RollSaved` is given a `rename`, or if
    /// `Response`'s `content = "data"` is dropped — either changes these two
    /// strings while every type-level test in this module keeps passing.
    #[test]
    fn save_roll_serializes_snake_case_with_its_payload_under_data() {
        assert_eq!(
            serde_json::to_string(&Request::SaveRoll).unwrap(),
            r#"{"kind":"save_roll"}"#
        );
        let reply = Response::RollSaved {
            path: "/tmp/flock.json".to_string(),
            apps: 3,
        };
        let wire = r#"{"kind":"roll_saved","data":{"path":"/tmp/flock.json","apps":3}}"#;
        assert_eq!(serde_json::to_string(&reply).unwrap(), wire);
        assert_eq!(serde_json::from_str::<Response>(wire).unwrap(), reply);
    }
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-core --lib` — expected: `Request::SaveRoll` and `Response::RollSaved` do not exist.

- [ ] **Step 3: Add the two variants**, each with the doc comments above (`missing_docs` is `deny`, and that includes struct-variant fields). `Request::SaveRoll` goes after `Trigger` and before `KillDaemon`; `Response::RollSaved` goes after `Triggered` and before `Subscribed`. Both enums are already `#[non_exhaustive]`; `PROTOCOL_VERSION` stays **1**.

- [ ] **Step 4: Regenerate the snapshots and read the delta.**

```
cargo insta test --accept -p shep-core     # or: INSTA_UPDATE=always cargo test -p shep-core --lib
git diff crates/shep-core/src/protocol/snapshots/
```

Expected: exactly one new object in `request_wire_v1.snap` (`{"id": 9, "deadline_ms": null, "body": {"kind": "save_roll"}}`) and one in `reply_wire_v1.snap`. **Paste the diff verbatim into the task report.** Any other line changing means something else moved and the accept was wrong.

- [ ] **Step 5: CHANGELOG** (IR-45) — reconcile shep-core's entry: a new request verb and a new response variant, additive, `PROTOCOL_VERSION` unchanged.

- [ ] **Step 6: Task gate, then commit** — `feat(core): put a muster-roll save on the wire`

---

## Task 2: the daemon writes the roll on demand

**Files:**
- Modify: `crates/shep-daemon/src/rpc.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced, depended on by Task 3 (through the wire) and Task 16:**

```rust
/// Where a muster roll landed and what it recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedRoll {
    /// The path written.
    pub path: PathBuf,
    /// How many apps the roll records.
    pub apps: u32,
}

impl RpcContext {
    /// Writes the muster roll now, reporting what it recorded.
    ///
    /// `None` means the supervisor engine has already stopped: there is
    /// nothing left to record and the shutdown path has already written the
    /// final roll.
    ///
    /// # Errors
    /// - [`SnapshotError`] — as `write_atomic`.
    pub async fn save_roll_now(&self) -> Result<Option<SavedRoll>, SnapshotError>;

    /// Writes the muster roll now, discarding what it recorded.
    ///
    /// # Errors
    /// - [`SnapshotError`] — as `write_atomic`.
    pub async fn snapshot_now(&self) -> Result<(), SnapshotError>;
}
```

Cargo shape for this task: `-p shep-daemon`.

`snapshot_now` keeps its exact current signature and its exact current behaviour — a no-op `Ok(())` once the engine has stopped — because `boot::run`'s teardown depends on it and `boot.rs`'s `boot_restores_a_saved_flock_and_tears_down_in_order` pins it with a deliberately-wrong `instances_running: 99` sentinel. It becomes a one-line wrapper over `save_roll_now`, so there is one writer and nothing to drift.

**`Ok(None)` is what the `SaveRoll` arm must refuse.** An operator typing `shep save` against a daemon whose engine has stopped is exactly the case decision 1 exists to rule out, and `Ok(())` would report success for a roll nobody wrote.

- [ ] **Step 1: Write the failing tests.** In `rpc.rs`'s `mod tests`:

```rust
    /// fails if `SaveRoll` stops writing, or writes without reporting: the
    /// assertion reads the file the reply named and compares its app count
    /// against the number the reply claimed, so a handler that answered
    /// `apps: 0` for a two-app flock — or named a path it never wrote —
    /// reddens here rather than in an operator's terminal after a reboot.
    #[tokio::test]
    async fn save_roll_writes_the_file_it_names_and_counts_what_it_recorded() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![
                            AppConfig::minimal("web", "./srv"),
                            AppConfig::minimal("worker", "./work"),
                        ],
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        let reply = reply_of(dispatch(envelope(2, Request::SaveRoll), &h.ctx).await);
        let Ok(Response::RollSaved { path, apps }) = reply.result else {
            panic!("expected RollSaved, got {:?}", reply.result)
        };
        assert_eq!(apps, 2);

        let roll = crate::snapshot::read(std::path::Path::new(&path)).unwrap();
        assert_eq!(roll.apps.len(), 2, "the reply's count must match the file");
        assert_eq!(path, h.ctx.snapshot_path.display().to_string());
    }

    /// fails if the handler forwards `snapshot_now`'s engine-stopped
    /// `Ok(())` as a success. That is the whole reason this verb exists:
    /// a save that wrote nothing and said "saved" is the failure mode an
    /// operator reboots into.
    #[tokio::test]
    async fn save_roll_against_a_stopped_engine_is_an_error_not_a_silent_success() {
        let h = harness(vec![]);
        h.ctx.supervisor.shutdown().await;

        let reply = reply_of(dispatch(envelope(1, Request::SaveRoll), &h.ctx).await);
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Internal);
        assert!(
            err.message.contains("engine"),
            "the operator must be told why nothing was written: {}",
            err.message
        );
    }
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-daemon --lib --all-features -- --skip watch:: --skip extras::`

- [ ] **Step 3: Implement.** Add `SavedRoll` to `rpc.rs` next to `RpcContext`, rewrite `snapshot_now` as the wrapper, and add the dispatch arm:

```rust
        Request::SaveRoll => match ctx.save_roll_now().await {
            Ok(Some(saved)) => reply(Ok(Response::RollSaved {
                // Lossy on purpose, matching `to_info`'s treatment of log
                // paths: a non-UTF-8 roll path must degrade one field, not
                // abort the whole reply.
                path: saved.path.to_string_lossy().into_owned(),
                apps: saved.apps,
            })),
            Ok(None) => reply(Err(RpcError {
                code: RpcErrorCode::Internal,
                message: "the supervisor engine has stopped; no roll was written".to_string(),
            })),
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::Internal,
                message: err.to_string(),
            })),
        },
```

`save_roll_now` itself is the existing `snapshot_now` body with the count taken from the roll it just built:

```rust
    pub async fn save_roll_now(&self) -> Result<Option<SavedRoll>, SnapshotError> {
        let Ok(infos) = self.supervisor.list_checked().await else {
            return Ok(None);
        };
        let roll = self.registry.roll(&infos, crate::now_ms());
        write_atomic(&self.snapshot_path, &roll)?;
        Ok(Some(SavedRoll {
            path: self.snapshot_path.clone(),
            // `u32` matches `SavedApp::instances_running`; a flock large
            // enough to overflow it has other problems.
            apps: u32::try_from(roll.apps.len()).unwrap_or(u32::MAX),
        }))
    }
```

There is **no new `RpcErrorCode`**. `Internal` carries both failures for the reason `rpc_error`'s existing comment on `ReloadInFlight` gives: the enum is versioned, a client predating a new code cannot decode the reply at all, and the message is the part that says what to do.

- [ ] **Step 4: Run the inner loop, confirm both new tests pass and 333+2 still do.**

- [ ] **Step 5: CHANGELOG** — shep-daemon: `SaveRoll` answered; `RpcContext::save_roll_now` added, `snapshot_now` unchanged in behaviour.

- [ ] **Step 6: Task gate, then commit** — `feat(daemon): write the muster roll when an operator asks`

---

## Task 3: `shep save`

**Files:**
- Create: `crates/shep-cli/src/commands/muster.rs`
- Modify: `crates/shep-cli/src/commands/mod.rs`, `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`, `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/output/mod.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced:**

```rust
// commands/muster.rs
pub async fn save(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode;

// output/rows.rs
/// `Response::RollSaved` — where the muster roll landed, and what it recorded.
#[derive(Debug, Serialize)]
pub struct SavedRollRow {
    /// The roll's path, exactly as the daemon reported it.
    pub file: String,
    /// How many apps that roll records.
    pub apps: u32,
}

// cli.rs
pub enum Commands {
    /// Write the muster roll now, so a reboot can bring this flock back.
    Save,
}
```

Cargo shape for this task: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`.

**`main.rs`'s dispatch arms had zero unit coverage until recently** — a verb wired to the wrong function was invisible workspace-wide. Every verb this plan adds gets a dispatch assertion in `main.rs`'s own test module (Step 4 below), not only a test of the function it should reach.

- [ ] **Step 1: Write the failing tests.**

In `output/rows.rs`'s `mod tests`, next to the existing anti-drift cases:

```rust
    /// fails if `SavedRollRow` grows a field that never reaches the table —
    /// the same gate `flock_rows_do_not_drift` applies, instantiated for a
    /// payload whose every field is a column.
    #[test]
    fn saved_roll_row_does_not_drift() {
        let row = SavedRollRow {
            file: "/home/rin/.shep/flock.json".to_string(),
            apps: 9,
        };
        assert_no_drift(&row, |json| json, &[]);
    }
```

In `commands/muster.rs`:

```rust
    /// fails if `save` sends anything but `SaveRoll` — a verb wired to
    /// `ListFlock` still gets a reply from the fake daemon and would pass
    /// every other assertion here.
    #[tokio::test]
    async fn save_sends_save_roll_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = save(&client, &mut streams, Format::Table).await;

        let envelope = tokio::time::timeout(FAKE_REPLY_WAIT, envelopes.recv())
            .await
            .expect("the fake daemon must answer inside the bound")
            .unwrap();
        assert_eq!(envelope.body, Request::SaveRoll);
    }

    /// fails if `save` treats an RPC failure as a success. `shep save`
    /// exists so a failed save is loud; an exit 0 here is the bug the verb
    /// was added to make impossible.
    #[tokio::test]
    async fn a_failed_save_exits_non_zero_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, _served) = fake_client_replying_err(
            &path,
            RpcErrorCode::Internal,
            "the supervisor engine has stopped; no roll was written",
        )
        .await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            save(&client, &mut streams, Format::Table).await
        };
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty(), "a failed save prints no success table");
        assert!(String::from_utf8(err).unwrap().contains("engine"));
    }
```

`FAKE_REPLY_WAIT` is a module-level `const FAKE_REPLY_WAIT: Duration = Duration::from_secs(5);` — every await of a daemon answer in this phase is bounded, per Test discipline.

In `cli.rs`'s `mod tests`, extend the existing `alias_visibility_and_hiding_are_pinned` visible list with `"save"`.

In `main.rs`'s `mod tests`:

```rust
    /// fails if `Commands::Save` is wired to another verb's function. The
    /// dispatch arms carried no unit coverage at all until recently, and a
    /// verb pointed at the wrong handler was invisible workspace-wide.
    #[test]
    fn save_parses_to_its_own_command() {
        use clap::Parser;
        assert!(matches!(
            Cli::try_parse_from(["shep", "save"]).unwrap().command,
            Commands::Save
        ));
    }
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p shep-cli --bins`

- [ ] **Step 3: Implement.** `commands/muster.rs` follows `commands/query.rs`'s shape — one request, `emit` the payload, map the error — with the client's default deadline (a roll write is a few KiB to a local file):

```rust
pub async fn save(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    match client.request(Request::SaveRoll).await {
        Ok(Response::RollSaved { path, apps }) => write_outcome(emit(
            &mut *streams.out,
            fmt,
            "save",
            SavedRollRow { file: path, apps },
        )),
        Ok(_unrecognised) => { /* ExitCode::Internal, exactly as `trigger` does */ }
        Err(err) => { /* ExitCode::from(&err), exactly as `trigger` does */ }
    }
}
```

`SavedRollRow` renders `FILE` / `APPS`, with `json_key_for` mapping them to `file` / `apps` and `JSON_ONLY: &[]` — every field is a column, for `EmptiedFiles`' stated reason: a verb that wrote a file and would not say which one has reported nothing.

Dispatch in `main.rs`, in the locked-handles block alongside `Ping`:

```rust
        Commands::Save => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => muster::save(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
```

`connect_client`, not `connect_or_spawn_client`: saving the roll of a daemon that is not running is not a thing, and autostarting one to save an empty flock would overwrite a good roll with an empty one.

- [ ] **Step 4: Run tests, confirm pass.**

- [ ] **Step 5: CHANGELOG** — shep-cli: `shep save` added.

- [ ] **Step 6: Task gate, then commit** — `feat(cli): shep save`

---

## Task 4: `muster` on the wire, and one restore path

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/snapshot.rs`, `crates/shep-daemon/src/boot.rs`, `crates/shep-daemon/src/rpc.rs`
- Modify: `crates/shep-core/CHANGELOG.md`, `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced, depended on by Task 5:**

```rust
// shep-core
pub enum Request {
    /// Assemble the flock from the muster roll on disk
    Muster,
}
pub enum Response {
    /// Answer to `Muster` — every sheep of every app the roll restored
    Mustered(Vec<ProcessInfo>),
}

// shep-daemon/src/snapshot.rs
/// Reads the muster roll and starts every app it restores, returning the
/// names it restored.
///
/// A missing roll restores nothing and is not an error — a fresh
/// `$SHEP_HOME` has none, and that is a first boot rather than a fault. A
/// roll that exists and will not parse IS reported: something corrupted or
/// hand-edited it, and starting an empty flock instead would hide that.
///
/// # Errors
/// - [`SnapshotError`] — the roll exists but could not be read or parsed.
pub(crate) async fn muster(
    path: &Path,
    registry: &FlockRegistry,
    supervisor: &SupervisorHandle,
) -> Result<Vec<String>, SnapshotError>;
```

Cargo shape: this task crosses a crate boundary, so it uses `--workspace` throughout — `cargo test --workspace --all-features --lib` while iterating, and the full gate at the end. Do not switch to `-p` partway.

`muster` returns the **names** it restored rather than a listing: the caller decides what to do with them (boot ignores them, the RPC arm turns them into a listing), and returning `Vec<ProcessInfo>` would tempt the boot path into reporting a listing nobody reads.

- [ ] **Step 1: Write the failing tests.**

In `request.rs`, add the envelope row and the round-trip pin, exactly as Task 1 did:

```rust
            // Paired with the `save_roll` row above so the two halves of the
            // roll — the direction that writes it and the direction that
            // assembles from it — sit next to each other, differing by their
            // `kind` and by nothing else.
            Envelope {
                id: 10,
                deadline_ms: None,
                body: Request::Muster,
            },
```

In `snapshot.rs`'s `mod tests`:

```rust
    /// fails if `muster` starts an app the roll says was down, or skips one
    /// it says was up. Both halves in one case, because a restore rule that
    /// inverted would pass either half alone.
    #[tokio::test(start_paused = true)]
    async fn muster_starts_what_the_roll_recorded_running_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(paths.snapshot.parent().unwrap()).unwrap();
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![
                SavedApp {
                    app: AppConfig::minimal("up", "./srv"),
                    instances_running: 1,
                },
                SavedApp {
                    app: AppConfig::minimal("down", "./srv"),
                    instances_running: 0,
                },
            ],
        };
        write_atomic(&paths.snapshot, &roll).unwrap();

        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let handle = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events,
        );
        let registry = FlockRegistry::new();

        let restored = muster(&paths.snapshot, &registry, &handle).await.unwrap();
        assert_eq!(restored, vec!["up".to_string()]);
        let listed = handle.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "up");
        handle.shutdown().await;
    }

    /// fails if a missing roll becomes an error. A fresh `$SHEP_HOME` has
    /// none, and a daemon that refused to boot over it would be unbootable
    /// on a clean machine.
    #[tokio::test(start_paused = true)]
    async fn a_missing_roll_restores_nothing_without_failing() {
        // ... same harness, no `write_atomic` call ...
        assert_eq!(muster(&paths.snapshot, &registry, &handle).await.unwrap(), Vec::<String>::new());
    }
```

In `rpc.rs`'s `mod tests`:

```rust
    /// fails if `Muster` reports only what THIS call spawned. `do_start` is
    /// idempotent through `instance_slots`, so a muster against an already
    /// restored flock legitimately spawns nothing — and an empty reply there
    /// is indistinguishable from "the roll was empty", which is the one
    /// thing this reply exists to tell apart.
    #[tokio::test]
    async fn a_second_muster_still_reports_the_flock_the_roll_restored() {
        let h = harness(vec![ProcScript::never_exits()]);
        reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("web", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        reply_of(dispatch(envelope(2, Request::SaveRoll), &h.ctx).await);

        let reply = reply_of(dispatch(envelope(3, Request::Muster), &h.ctx).await);
        let Ok(Response::Mustered(infos)) = reply.result else {
            panic!("expected Mustered, got {:?}", reply.result)
        };
        assert_eq!(infos.len(), 1, "the sheep the roll restores, not the ones this call spawned");
        assert_eq!(infos[0].name, "web");
    }
```

Size the `ScriptedRunner` pool for **the spawn a broken implementation performs that a correct one does not**: a `Muster` that ignored `instance_slots` and spawned a second `web` needs two scripts. Give the harness one, and say in the comment that exhausting the pool is how the wrong implementation shows up here — `ScriptedRunner` answers `SpawnFailed("script exhausted")`, which becomes `Errored`, and that state is otherwise indistinguishable from the failure under test.

- [ ] **Step 2: Run, confirm failure.** `cargo test --workspace --all-features --lib`

- [ ] **Step 3: Implement.**

`snapshot::muster` is `boot::restore_flock`'s body, moved, with the names collected and the two `tracing::warn!` calls kept exactly as they are — a rejected entry and a failed spawn must still be reported and must still not sink the caller:

```rust
pub(crate) async fn muster(
    path: &Path,
    registry: &FlockRegistry,
    supervisor: &SupervisorHandle,
) -> Result<Vec<String>, SnapshotError> {
    let saved = match read(path) {
        Ok(saved) => saved,
        Err(SnapshotError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => return Err(err),
    };
    let restorable = restorable(saved);
    for (name, err) in &restorable.rejected {
        tracing::warn!(name, %err, "muster roll entry rejected on restore");
    }
    if restorable.apps.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = restorable
        .apps
        .iter()
        .map(|app| app.config().name.clone())
        .collect();
    // Recorded regardless of whether `start` below fully succeeds, matching
    // `rpc::run`'s own Start handler: already-registered entries must
    // persist even when a later spawn in the same batch fails.
    registry.record(&restorable.apps);
    if let Err(err) = supervisor.start(restorable.apps).await {
        // One bad entry does not sink the muster — the same policy
        // `restorable` already applies at validation time. The sheep that
        // failed to spawn is already recorded `Errored` by the supervisor.
        tracing::warn!(%err, "muster roll restore failed to spawn one or more apps");
    }
    Ok(names)
}
```

`boot::restore_flock` becomes:

```rust
async fn restore_flock(
    paths: &ShepPaths,
    registry: &FlockRegistry,
    supervisor: &SupervisorHandle,
) -> Result<(), BootError> {
    snapshot::muster(&paths.snapshot, registry, supervisor)
        .await
        .map(|_names| ())
        .map_err(BootError::Snapshot)
}
```

The `rpc.rs` arm:

```rust
        Request::Muster => match crate::snapshot::muster(
            &ctx.snapshot_path,
            &ctx.registry,
            &ctx.supervisor,
        )
        .await
        {
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::Internal,
                message: err.to_string(),
            })),
            Ok(names) => match ctx.supervisor.list_checked().await {
                Err(err) => reply(Err(rpc_error(&err))),
                // Every sheep of every app the roll restored, not only the
                // ones this call spawned — see `Response::Mustered`.
                Ok(infos) => reply(Ok(Response::Mustered(
                    infos
                        .into_iter()
                        .filter(|info| names.contains(&info.name))
                        .collect(),
                ))),
            },
        },
```

- [ ] **Step 4: Regenerate the request snapshot, read the delta, paste it into the report.** Expected: one new object, `{"kind": "muster"}`.

- [ ] **Step 5: Run the full lib suite.** `cargo test --workspace --all-features --lib`

- [ ] **Step 6: CHANGELOGs** — shep-core: `Muster`/`Mustered` added, additive. shep-daemon: the boot restore and the `Muster` verb share one implementation.

- [ ] **Step 7: Task gate, then commit** — `feat: assemble the flock from the roll on demand`

---

## Task 5: `shep muster`

**Files:**
- Modify: `crates/shep-cli/src/commands/muster.rs`, `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`, `docs/specs/deferred.md`

**Interfaces — produced:**

```rust
pub async fn muster(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode;
```

Cargo shape: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`.

Three things distinguish this verb from `save`:

1. **It autostarts.** `shep muster` against a dead daemon must bring one up — that is what the verb is for. It is the **second** autostart path in the binary, so `run`'s own doc ("`Start` is the one exception at this layer … the *only* autostart path in the binary") stops being true and must be corrected in the same commit.
2. **It asks for `START_DEADLINE`, not the 5s default.** A muster spawns every app in the roll; `lifecycle::start` already established the precedent and the reason.
3. **An empty reply gets an explicit stderr notice** (decision 4).

The `resurrect` alias is a **hidden** alias (`#[command(alias = "resurrect")]`, not `visible_aliases`), per spec §9 and §14.5: it exists so a pm2 muscle-memory invocation works, not so `--help` teaches it.

- [ ] **Step 1: Write the failing tests.**

```rust
    /// fails if `muster` sends anything but `Muster`, or asks for the
    /// client's plain 5s default. A cold restore of a real flock routinely
    /// outruns five seconds, and a client-side abandonment there reports
    /// failure for a flock that came up fine.
    #[tokio::test]
    async fn muster_sends_muster_with_the_start_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams { out: &mut out, err: &mut err };
        let _ = muster(&client, &mut streams, Format::Table).await;

        let envelope = tokio::time::timeout(FAKE_REPLY_WAIT, envelopes.recv())
            .await
            .expect("the fake daemon must answer inside the bound")
            .unwrap();
        assert_eq!(envelope.body, Request::Muster);
        assert_eq!(
            envelope.deadline_ms,
            Some(u64::try_from(START_DEADLINE.as_millis()).unwrap())
        );
    }

    /// fails if an empty muster prints an empty table and exits 0 in
    /// silence. "The roll restored nothing" is the answer an operator needs
    /// most and the one an empty table hides.
    #[tokio::test]
    async fn a_muster_that_restored_nothing_says_so_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_on(&path).await;
        daemon.queue_reply_then_event(Response::Mustered(Vec::new()), /* … */);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams { out: &mut out, err: &mut err };
            muster(&client, &mut streams, Format::Table).await
        };
        assert_eq!(code, ExitCode::Success, "an empty roll is not a failure");
        assert!(
            !err.is_empty(),
            "an empty muster must not be a silent success"
        );
    }
```

The second case needs a fake that answers a chosen `Response`. `shep_client::testing` exposes `reply_to_list`/`reply_to_describe`/`queue_reply_then_event` — **read `crates/shep-client/src/testing.rs` and use whichever already answers an arbitrary `Response`; add a `reply_to` helper there only if none does**, and say in the report which you found.

In `main.rs`'s tests, the dispatch assertion, plus:

```rust
    /// fails if `resurrect` stops reaching `muster`, or starts showing up in
    /// `--help`. It exists for a pm2 muscle-memory invocation, not to be
    /// taught.
    #[test]
    fn resurrect_is_a_hidden_alias_for_muster() {
        use clap::{CommandFactory, Parser};
        assert!(matches!(
            Cli::try_parse_from(["shep", "resurrect"]).unwrap().command,
            Commands::Muster
        ));
        let cmd = Cli::command();
        let muster = cmd.find_subcommand("muster").unwrap();
        assert!(
            muster.get_visible_aliases().next().is_none(),
            "resurrect must stay out of --help"
        );
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement**, including the correction to `run`'s doc comment about autostart being unique to `Start`. Dispatch:

```rust
        Commands::Muster => match connect_or_spawn_client(&mut streams, fmt, &paths).await {
            Ok(client) => muster::muster(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
```

The notice goes through `output::emit_error`'s sibling for non-error notices if one exists, else a plain write to `streams.err` — **read `output/mod.rs` and `commands/bleats.rs` first**; `bleats` already prints notices to stderr under `--quiet` control, and this notice follows whatever that does rather than inventing a second convention.

Note the autostart interaction and state it in the verb's doc: when `connect_or_spawn` **spawned** the daemon, boot has already restored the roll, so the `Muster` that follows spawns nothing and reports the flock that restore produced. That is decision 3 doing its job, not a wasted round trip.

- [ ] **Step 4: Run tests, confirm pass.**

- [ ] **Step 5: `docs/specs/deferred.md`** — remove the `save` / `muster` entry; it is no longer deferred. Do not touch the `import` or `startup` entries yet.

- [ ] **Step 6: CHANGELOG** — shep-cli: `shep muster` (hidden alias `resurrect`) added; second autostart path.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): shep muster`

---

## Task 6: reading `dump.pm2`

**Files:**
- Create: `crates/shep-cli/src/commands/import/mod.rs` (module tree + the fixture's `include_str!` only, this task), `crates/shep-cli/src/commands/import/dump.rs`, `crates/shep-cli/src/commands/import/testdata/dump.pm2.json`
- Modify: `crates/shep-cli/src/commands/mod.rs`, `crates/shep-cli/Cargo.toml`

**Interfaces — produced, depended on by Tasks 7, 8, 9:**

```rust
/// One instance row out of a pm2 dump.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DumpRow {
    pub name: String,
    pub pm_exec_path: String,
    pub args: Vec<String>,
    pub pm_cwd: Option<String>,
    pub exec_interpreter: Option<String>,
    pub exec_mode: Option<String>,
    pub autorestart: Option<bool>,
    pub restart_delay: Option<u64>,
    pub merge_logs: Option<bool>,
    pub max_memory_restart: Option<u64>,
    /// The row's `env` map: what the process is actually running with.
    pub env: BTreeMap<String, String>,
    /// Every `env_<name>` map, keyed by the suffix — by construction these
    /// hold only what the ecosystem file declared.
    pub declared: BTreeMap<String, BTreeMap<String, String>>,
    /// Keys dropped because their value was neither a string, a number, nor
    /// a boolean — nothing a Flockfile env can hold.
    pub unrepresentable: Vec<String>,
}

/// Parses a whole dump document.
///
/// # Errors
/// - [`DumpError::Json`] — the document is not valid JSON.
/// - [`DumpError::NotAnArray`] — valid JSON, but not the array of instance rows a dump is.
/// - [`DumpError::RowMissingName`] — a row carries no `name`.
/// - [`DumpError::RowMissingScript`] — a row carries no `pm_exec_path` (carries the keys it did find).
pub(crate) fn parse(source: &str) -> Result<Vec<DumpRow>, DumpError>;

#[derive(Debug)]
pub(crate) enum DumpError {
    Json(String),
    NotAnArray,
    RowMissingName { index: usize },
    RowMissingScript { index: usize, name: String, keys: Vec<String> },
}
```

Cargo shape: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`.

Add to `crates/shep-cli/Cargo.toml`:

```toml
# Rendering the Flockfile `shep import` writes. `parse` is unused here — the
# CLI reads Flockfiles through shep-core — but the workspace entry names both
# and a per-crate feature narrowing would be the only difference.
toml.workspace = true
```

### The nesting question, settled by measurement

Measured against a real dump on 2026-08-12: a row is **flat**. There is no
`pm2_env` key; `name`, `pm_exec_path`, `args`, `exec_interpreter`,
`exec_mode`, `max_memory_restart` and `env_production` all sit at the row's
top level, alongside the process environment splatted in as individual string
keys. The `env` dict holds exactly that same environment — all 31 of its keys
also appear at the top level — which is what makes it usable as the bound on
"which top-level strings are environment rather than config".

So config is read from the row, declared env from `env_production`, and `env`
is consulted only to name what was dropped (decision 7).

Reading through `serde_json::Value` rather than `#[serde(flatten)]` is deliberate: flatten would need a catch-all map to collect the `env_<name>` keys, and it interacts badly with a row that carries the whole process environment as sibling string keys. Readability wins here (core principles), and a dump is a handful of rows.

**A row with no `pm_exec_path` is a named failure, not a skipped row.** It is what would catch a dump shape the 2026-08-12 measurement did not cover, rather than emitting a Flockfile full of apps with no script. `DumpError::RowMissingScript` carries the row's index, its name, and the keys it *did* find (sorted, truncated to the first 20) so the operator can see the shape and report it.

**Env values are not always strings.** A declared `PORT: 3000` in an ecosystem file arrives as a JSON number. Strings pass through, numbers and booleans are stringified, and anything else (an object, an array, a null) is dropped into `unrepresentable` and named in the output — never silently.

- [ ] **Step 1: Write the fixture, by hand.** `crates/shep-cli/src/commands/import/testdata/dump.pm2.json`. **This is synthesised from the shape the design spec documents and is never derived from a real dump** — a real dump carries absolute paths from a production host, a live SSH session's environment, and that host's layout. Four instance rows, three apps:

```json
[
  {
    "name": "api",
    "pid": 41201,
    "pm_id": 0,
    "pm_exec_path": "/srv/api/dist/server.js",
    "args": ["--port", "8080"],
    "pm_cwd": "/srv/api",
    "exec_interpreter": "node",
    "exec_mode": "cluster_mode",
    "autorestart": true,
    "merge_logs": false,
    "max_memory_restart": 536870912,
    "SSH_TTY": "/dev/pts/3",
    "XDG_SESSION_ID": "914",
    "BUN_INSTALL": "/home/deploy/.bun",
    "env": {
      "NODE_ENV": "production",
      "NODE_APP_INSTANCE": "0",
      "PM2_HOME": "/home/deploy/.pm2",
      "BUN_INSTALL": "/home/deploy/.bun",
      "SSH_TTY": "/dev/pts/3",
      "XDG_SESSION_ID": "914",
      "MOTD_SHOWN": "pam",
      "LS_COLORS": "rs=0:di=01;34:ln=01;36:",
      "LANG": "en_US.UTF-8",
      "SHLVL": "1",
      "PATH": "/home/deploy/.bun/bin:/usr/local/bin:/usr/bin:/bin"
    },
    "env_production": { "NODE_ENV": "production" }
  },
  {
    "name": "api",
    "pid": 41202,
    "pm_id": 1,
    "pm_exec_path": "/srv/api/dist/server.js",
    "args": ["--port", "8080"],
    "pm_cwd": "/srv/api",
    "exec_interpreter": "node",
    "exec_mode": "cluster_mode",
    "autorestart": true,
    "merge_logs": false,
    "max_memory_restart": 536870912,
    "env": {
      "NODE_ENV": "production",
      "NODE_APP_INSTANCE": "1",
      "PM2_HOME": "/home/deploy/.pm2",
      "BUN_INSTALL": "/home/deploy/.bun",
      "SSH_TTY": "/dev/pts/3",
      "XDG_SESSION_ID": "914",
      "MOTD_SHOWN": "pam",
      "LS_COLORS": "rs=0:di=01;34:ln=01;36:",
      "LANG": "en_US.UTF-8",
      "SHLVL": "1",
      "PATH": "/home/deploy/.bun/bin:/usr/local/bin:/usr/bin:/bin"
    },
    "env_production": { "NODE_ENV": "production" }
  },
  {
    "name": "worker",
    "pid": 41310,
    "pm_id": 2,
    "pm_exec_path": "/srv/worker/index.ts",
    "args": [],
    "pm_cwd": "/srv/worker",
    "exec_interpreter": "bun",
    "exec_mode": "fork_mode",
    "autorestart": false,
    "restart_delay": 5000,
    "merge_logs": true,
    "env": {
      "QUEUE_URL": "redis://127.0.0.1:6379/2",
      "JAVA_HOME": "/usr/lib/jvm/default",
      "SSH_TTY": "/dev/pts/3",
      "LANG": "en_US.UTF-8"
    },
    "env_staging": {
      "QUEUE_URL": "redis://127.0.0.1:6379/2",
      "QUEUE_CONCURRENCY": 4
    }
  },
  {
    "name": "migrate",
    "pid": 41455,
    "pm_id": 3,
    "pm_exec_path": "/srv/migrate/bin/migrate",
    "args": ["--once"],
    "pm_cwd": "/srv/migrate",
    "exec_interpreter": "none",
    "exec_mode": "fork_mode",
    "env": {
      "DATABASE_URL": "postgres://localhost/app",
      "TERM": "xterm-256color"
    }
  }
]
```

Every one of those values is invented. What each row is *for*: `api` covers cluster collapsing, `node`, a memory ceiling, a declared key, a pm2-injected instance number and an inherited toolchain path; `worker` covers `bun`, `autorestart: false`, `restart_delay`, `merge_logs`, a declared key with a **numeric** value that is absent from `env`, and an ambiguous `JAVA_HOME`; `migrate` covers `exec_interpreter: "none"`, an app started by hand with **no declared env at all**, and one session-junk key. `api`'s first row additionally carries `SSH_TTY`, `XDG_SESSION_ID` and `BUN_INSTALL` splatted onto its own top level, sitting beside its config fields exactly as the 2026-08-12 measurement found them, and duplicated in `env` — the row a real dump produces, and proof the reader is not confused by config and session noise sharing one object.

- [ ] **Step 2: Write the failing tests** in `dump.rs`:

```rust
    const FIXTURE: &str = include_str!("testdata/dump.pm2.json");

    /// fails if the reader stops reading a row's fields from its own top
    /// level. Every row in the fixture is flat, and `api`'s first row also
    /// carries splatted session keys beside its config fields — a reader
    /// that expected a wrapper object would find no `pm_exec_path` at all
    /// and error instead of parsing.
    #[test]
    fn the_fixture_parses_into_four_rows_with_their_fields() {
        let rows = parse(FIXTURE).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].name, "api");
        assert_eq!(rows[0].pm_exec_path, "/srv/api/dist/server.js");
        assert_eq!(rows[0].args, ["--port", "8080"]);
        assert_eq!(rows[0].pm_cwd.as_deref(), Some("/srv/api"));
        assert_eq!(rows[0].exec_interpreter.as_deref(), Some("node"));
        assert_eq!(rows[0].exec_mode.as_deref(), Some("cluster_mode"));
        assert_eq!(rows[0].max_memory_restart, Some(536_870_912));
        assert_eq!(rows[2].restart_delay, Some(5000));
        assert_eq!(rows[2].autorestart, Some(false));
        assert_eq!(rows[2].merge_logs, Some(true));
    }

    /// fails if a row with no `pm_exec_path` is skipped instead of reported.
    /// A skipped row means a Flockfile missing an app the operator believes
    /// they migrated, discovered after the reboot; the error names the
    /// index, the app, and the keys the row did carry, which is what would
    /// catch a dump shape the 2026-08-12 measurement did not cover.
    #[test]
    fn a_row_with_no_script_is_a_named_failure() {
        let odd = r#"[{"name":"web","script":"/srv/web"}]"#;
        let err = parse(odd).unwrap_err();
        let DumpError::RowMissingScript { index, name, keys } = err else {
            panic!("expected RowMissingScript, got {err:?}")
        };
        assert_eq!(index, 0);
        assert_eq!(name, "web");
        assert!(keys.iter().any(|k| k == "script"), "{keys:?}");
    }

    /// fails if a declared env value that is not a string aborts the parse
    /// or is dropped in silence. `QUEUE_CONCURRENCY: 4` is a number in the
    /// fixture because an ecosystem file's `PORT: 3000` is one in life.
    #[test]
    fn declared_env_scalars_are_stringified_and_the_rest_is_named() {
        let rows = parse(FIXTURE).unwrap();
        let worker = &rows[2];
        assert_eq!(
            worker.declared["staging"]["QUEUE_CONCURRENCY"],
            "4"
        );
        let nested = r#"[{"name":"w","pm_exec_path":"/w","env":{"OPTS":{"a":1}}}]"#;
        let rows = parse(nested).unwrap();
        assert!(rows[0].env.is_empty());
        assert_eq!(rows[0].unrepresentable, ["OPTS"]);
    }

    /// fails if a document that is not the array of rows a dump is gets
    /// read as an empty dump — "imported 0 apps" for a file that was never
    /// a dump is the least useful answer there is.
    #[test]
    fn a_document_that_is_not_an_array_is_refused() {
        assert!(matches!(parse("{}"), Err(DumpError::NotAnArray)));
        assert!(matches!(parse("not json"), Err(DumpError::Json(_))));
    }
```

- [ ] **Step 3: Run, confirm failure.** `cargo test -p shep-cli --bins`

- [ ] **Step 4: Implement.** `parse` is `serde_json::from_str::<serde_json::Value>` → `as_array` → per row: pull each field by name directly off the row object, then a single pass over the object's keys collecting `env_<suffix>` maps. `DumpError` gets the per-module `Display` + `core::error::Error` impls house style requires (IR-18/19), each variant's doc naming the precise condition (IR-19).

- [ ] **Step 5: Run tests, confirm pass.**

- [ ] **Step 6: Task gate, then commit** — `feat(cli): read a pm2 dump`

---

## Task 7: instance collapsing and the field mapping

**Files:**
- Create: `crates/shep-cli/src/commands/import/convert.rs`
- Modify: `crates/shep-cli/src/commands/import/mod.rs`

**Interfaces — produced, depended on by Tasks 8, 9:**

```rust
/// What one dump became: the apps to write, and everything the operator has
/// to be told about them.
#[derive(Debug)]
pub(crate) struct Imported {
    /// One entry per app name, in the order the dump first mentions it.
    pub apps: Vec<AppConfig>,
    /// One per thing the operator decides, in app order.
    pub notes: Vec<ImportNote>,
}

/// Something the import cannot decide on the operator's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportNote {
    /// The app ran in pm2 cluster mode; shep binds nothing, so the app
    /// itself must set `SO_REUSEPORT`.
    ClusterMode { app: String, instances: u32 },
    /// An env key the app inherited from the shell that started it, which
    /// was neither declared nor a known session-shell or pm2 key.
    InheritedEnv { app: String, key: String },
    /// An env value a Flockfile cannot hold.
    UnrepresentableEnv { app: String, key: String },
    /// The app read its instance number from a pm2 variable, recorded as
    /// `increment_var` rather than copied as a value.
    InstanceVar { app: String, var: String },
}

/// Collapses instance rows into apps and maps every field this importer
/// knows how to map. Every returned [`AppConfig`] has already been through
/// `shep_core::config::normalize`.
///
/// # Errors
/// - [`ConvertError::Rejected`] — a mapped app does not normalize (carries the app name and the reason).
pub(crate) fn convert(rows: Vec<DumpRow>) -> Result<Imported, ConvertError>;
```

Cargo shape: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`.

The mapping, one row per the design spec's table:

| pm2 | shep |
|---|---|
| `name` | `name` (the grouping key) |
| `pm_cwd` | `cwd` |
| `pm_exec_path` | `script` |
| `args` | `args` |
| `exec_interpreter` (`"none"` → run directly) | `interpreter` (`None` for `"none"`) |
| `autorestart` | `autorestart` |
| `restart_delay` (ms) | `restart_delay` (`UpDuration::from_millis`) |
| `merge_logs` | `merge_logs` |
| `max_memory_restart` (bytes) | `max_memory` (`MemSize::from_bytes`) |
| rows sharing a `name` | `instances` = the row count |
| `exec_mode == "cluster_mode"` | `reuse_port = true` + a `ClusterMode` note |
| `NODE_APP_INSTANCE` present in `env` | `increment_var` + an `InstanceVar` note |

**Grouping is by `name` and the first row wins every scalar.** Two instances of one app are the same app; if they disagree about `pm_exec_path`, one of them is a leftover from a deploy and the first is as good a choice as any. Do not merge, do not average, do not error.

**`normalize` runs on every mapped app before it is returned.** It is the same validation the daemon applies to peer input, and running it here means `shep import` fails at import time — with the app named — rather than writing a Flockfile that `shep start` refuses tomorrow. `ConvertError::Rejected` carries `NormalizeError`'s own message.

**Instance count is the row count, never a configured `instances` field.** The dump records what is *running*; an app configured for 4 and running 2 should come across as the 2 that are up, matching the muster roll's own "was up when we saved" rule.

- [ ] **Step 1: Write the failing tests** in `convert.rs`:

```rust
    fn imported() -> Imported {
        convert(dump::parse(include_str!("testdata/dump.pm2.json")).unwrap()).unwrap()
    }

    /// fails if instance rows stop collapsing — three apps out of four rows
    /// is the whole of what "the dump is per-instance" means, and an
    /// importer that skipped it would register `api` twice under one name,
    /// which `shep start` then refuses.
    #[test]
    fn four_instance_rows_collapse_into_three_apps() {
        let imported = imported();
        let names: Vec<&str> = imported.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["api", "worker", "migrate"]);
        assert_eq!(imported.apps[0].instances, 2, "api ran two instances");
        assert_eq!(imported.apps[1].instances, 1);
    }

    /// fails if any single mapping is dropped. One case per field rather
    /// than one per test: the mapping is a table, and a table is worth
    /// asserting as a table — a reader comparing this against the design
    /// spec's own table sees the same rows in the same order.
    #[test]
    fn every_mapped_field_lands_where_the_table_says() {
        let imported = imported();
        let api = &imported.apps[0];
        assert_eq!(api.script, "/srv/api/dist/server.js");
        assert_eq!(api.args, ["--port", "8080"]);
        assert_eq!(api.cwd.as_deref(), Some("/srv/api"));
        assert_eq!(api.interpreter.as_deref(), Some("node"));
        assert_eq!(api.max_memory, Some(MemSize::from_bytes(536_870_912)));
        assert!(api.autorestart);

        let worker = &imported.apps[1];
        assert_eq!(worker.interpreter.as_deref(), Some("bun"));
        assert!(!worker.autorestart);
        assert_eq!(worker.restart_delay, Some(UpDuration::from_millis(5000)));
        assert!(worker.merge_logs);

        // `exec_interpreter: "none"` means run the script directly, which in
        // a Flockfile is the ABSENCE of `interpreter` — not the literal
        // string "none", which shep would try to exec.
        let migrate = &imported.apps[2];
        assert_eq!(migrate.interpreter, None);
        assert_eq!(migrate.script, "/srv/migrate/bin/migrate");
    }

    /// fails if a cluster-mode app comes across without `reuse_port`, or
    /// without the note. Both halves are the same cutover blocker: shep
    /// binds nothing, so four instances on one port is EADDRINUSE, and the
    /// operator has to hear it at import time rather than at first start.
    #[test]
    fn cluster_mode_sets_reuse_port_and_says_so() {
        let imported = imported();
        assert!(imported.apps[0].reuse_port, "api ran in cluster mode");
        assert!(
            !imported.apps[1].reuse_port,
            "fork mode must not assert an option the app never set"
        );
        assert!(imported.notes.contains(&ImportNote::ClusterMode {
            app: "api".to_string(),
            instances: 2,
        }));
    }

    /// fails if `NODE_APP_INSTANCE` is copied into the app env as a value.
    /// Copying it pins instance 0's number into every instance, which is
    /// worse than dropping it: every worker would believe it is worker 0.
    #[test]
    fn the_pm2_instance_variable_becomes_increment_var_and_never_a_value() {
        let imported = imported();
        let api = &imported.apps[0];
        assert_eq!(api.increment_var.as_deref(), Some("NODE_APP_INSTANCE"));
        assert!(!api.env.contains_key("NODE_APP_INSTANCE"));
        assert!(imported.notes.contains(&ImportNote::InstanceVar {
            app: "api".to_string(),
            var: "NODE_APP_INSTANCE".to_string(),
        }));
    }

    /// fails if a mapped app is returned without going through `normalize`.
    /// Every app this fixture produces must be one the daemon would accept;
    /// a Flockfile that `shep start` refuses is not an import, it is a
    /// deferred failure.
    #[test]
    fn every_mapped_app_normalizes() {
        for app in imported().apps {
            let name = app.name.clone();
            shep_core::config::normalize(app).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** Group with an insertion-ordered pass (a `Vec<String>` of names in first-seen order plus a `HashMap<String, Vec<DumpRow>>`, or `BTreeMap` plus a separate order vector — **not** a bare `HashMap` iteration, whose order would make the output and every test non-deterministic). Build each `AppConfig` from `AppConfig::minimal(name, script)` and set only the mapped fields, so every unmapped field keeps its spec default rather than being spelled out.

Env is **not** this task's job — leave `env` empty and `notes` free of env entries; Task 8 fills both. Say so in a doc comment on `convert` rather than leaving a reader to wonder why `env` is always empty here.

- [ ] **Step 4: Run tests, confirm pass.**

- [ ] **Step 5: Task gate, then commit** — `feat(cli): collapse pm2 instances into apps`

---

## Task 8: declared env, and naming what is dropped

**Files:**
- Create: `crates/shep-cli/src/commands/import/env.rs`
- Modify: `crates/shep-cli/src/commands/import/convert.rs`, `crates/shep-cli/src/commands/import/mod.rs`

**Interfaces — produced, depended on by Task 9:**

```rust
/// What an app's env became, and what the operator has to decide.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AppEnv {
    /// The env to write into the Flockfile.
    pub env: BTreeMap<String, String>,
    /// Keys present on the running process that were neither declared nor
    /// recognised — named in the output, never written.
    pub inherited: Vec<String>,
    /// The pm2 variable the app read its instance number from, if any.
    pub instance_var: Option<String>,
}

/// Splits one row's environment into what a Flockfile should carry and what
/// the operator has to decide about.
pub(crate) fn split(row: &DumpRow) -> AppEnv;
```

Cargo shape: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`.

### The rule, and why it is a construction rather than a heuristic

The **declared** env is the union of the row's `env_<name>` maps. By construction those hold only what the ecosystem file declared — pm2 never flattens a login shell into them, only into `env`. So:

- A key in the declared union is **written**. Its value comes from `env` when the running process has one (the dump records what is actually running), else from the declared map.
- A key in `env` that is not declared is checked against two closed lists: **session junk** (dropped silently — it is a login shell's, not an app's) and **pm2-injected** (dropped silently, except `NODE_APP_INSTANCE`, which becomes `increment_var`).
- **Everything else is named in the output and not written.** The operator decides whether it belongs in the Flockfile, the unit, or nowhere.

Decision 9 is what lets the pm2-injected list stay short and honest: a key it fails to recognise lands in the "named, not written" bucket. The failure mode of an incomplete list is one extra line of output, never a silently wrong config. **Do not grow either list by guessing** — a session-junk list that is too long is the one direction that *does* lose data silently.

```rust
/// Variables a login shell puts in every process it starts. Dropped without
/// comment: they describe the session that ran `pm2 start`, not the app, and
/// an init-started daemon has none of them.
const SESSION_SHELL: &[&str] = &[
    "COLORTERM", "DBUS_SESSION_BUS_ADDRESS", "DISPLAY", "EDITOR", "HISTFILE",
    "HISTSIZE", "HOME", "HOSTNAME", "LANG", "LOGNAME", "LS_COLORS", "MAIL",
    "MOTD_SHOWN", "OLDPWD", "PAGER", "PATH", "PWD", "SHELL", "SHLVL", "TERM",
    "TMPDIR", "USER", "VISUAL", "_",
];

/// Prefixes with the same standing as [`SESSION_SHELL`].
const SESSION_SHELL_PREFIXES: &[&str] = &["LC_", "SSH_", "SUDO_", "XDG_"];

/// Variables pm2 puts into a process it supervises. Short on purpose: a key
/// this list misses is NAMED rather than written, so the cost of an
/// incomplete list is a line of output, not a wrong Flockfile.
const PM2_INJECTED: &[&str] = &["NODE_APP_INSTANCE", "pm_id", "unique_id"];

/// Prefix with the same standing as [`PM2_INJECTED`].
const PM2_INJECTED_PREFIXES: &[&str] = &["PM2_"];

/// The pm2 variable an app reads its instance number from. Recorded as
/// `increment_var` rather than copied as a value: the dump holds instance
/// 0's number, and writing it would tell every instance it is instance 0.
const PM2_INSTANCE_VAR: &str = "NODE_APP_INSTANCE";
```

**`PATH` is session junk here, deliberately.** The design spec is explicit that `PATH` is captured into the *unit*, not into an app's env — that is the mechanism making an interpreter under `~/.bun` findable after a reboot. An app's Flockfile carrying a login shell's `PATH` would defeat it.

- [ ] **Step 1: Write the failing tests** in `env.rs`:

```rust
    fn rows() -> Vec<DumpRow> {
        dump::parse(include_str!("testdata/dump.pm2.json")).unwrap()
    }

    /// fails if the declared env stops being the union of the `env_<name>`
    /// maps — an importer reading `env` instead would write all eleven of
    /// api's keys, twenty-four of which across the design's real dump were a
    /// dead login session.
    #[test]
    fn only_declared_keys_are_written() {
        let api = split(&rows()[0]);
        assert_eq!(
            api.env.keys().collect::<Vec<_>>(),
            ["NODE_ENV"],
            "one declared key, and the ten inherited ones are not it"
        );
        assert_eq!(api.env["NODE_ENV"], "production");
    }

    /// fails if a declared key absent from the running process's env is
    /// dropped. `env` holds what is running; the declared map is what the
    /// ecosystem file asked for, and a key the process never received is
    /// still one the operator declared.
    #[test]
    fn a_declared_key_missing_from_the_running_env_still_comes_across() {
        let worker = split(&rows()[2]);
        assert_eq!(worker.env["QUEUE_CONCURRENCY"], "4");
        assert_eq!(worker.env["QUEUE_URL"], "redis://127.0.0.1:6379/2");
    }

    /// fails if an ambiguous key is dropped silently. This is the whole
    /// decision the design refuses to make on the operator's behalf: a
    /// heuristic that guessed which inherited vars matter will eventually be
    /// wrong, and being wrong silently is what makes it expensive.
    #[test]
    fn an_unrecognised_inherited_key_is_named_and_not_written() {
        let api = split(&rows()[0]);
        assert_eq!(api.inherited, ["BUN_INSTALL"]);
        assert!(!api.env.contains_key("BUN_INSTALL"));

        let worker = split(&rows()[2]);
        assert_eq!(worker.inherited, ["JAVA_HOME"]);

        // An app started by hand has no declared env at all, so every key it
        // is running with is the operator's to decide on.
        let migrate = split(&rows()[3]);
        assert_eq!(migrate.inherited, ["DATABASE_URL"]);
        assert!(migrate.env.is_empty());
    }

    /// fails if a session or pm2 key starts being named. Naming them is not
    /// harmless: twenty-four lines of `LS_COLORS` and `XDG_SESSION_ID` per
    /// app is how an operator learns to skim past the two lines that matter.
    #[test]
    fn session_and_pm2_keys_are_dropped_without_comment() {
        let api = split(&rows()[0]);
        for quiet in ["SSH_TTY", "XDG_SESSION_ID", "MOTD_SHOWN", "LS_COLORS",
                      "LANG", "SHLVL", "PATH", "PM2_HOME", "NODE_APP_INSTANCE"] {
            assert!(!api.inherited.iter().any(|k| k == quiet), "{quiet} was named");
            assert!(!api.env.contains_key(quiet), "{quiet} was written");
        }
        assert_eq!(api.instance_var.as_deref(), Some("NODE_APP_INSTANCE"));
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement `split`, then wire it into `convert`.** `convert` calls `split` on the app's **first** row (its scalars already win, and an app's declared env does not differ per instance) and turns `AppEnv` into `AppConfig::env`, `AppConfig::increment_var`, and the `InheritedEnv`/`InstanceVar` notes. `DumpRow::unrepresentable` becomes `UnrepresentableEnv` notes in the same pass.

- [ ] **Step 4: Run tests, confirm pass** — including Task 7's, which now see a non-empty `env` and must still pass unchanged.

- [ ] **Step 5: Task gate, then commit** — `feat(cli): import only the env an app actually declared`

---

## Task 9: rendering the Flockfile, and `shep import`

**Files:**
- Create: `crates/shep-cli/src/commands/import/render.rs`
- Modify: `crates/shep-cli/src/commands/import/mod.rs`, `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`, `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/tests/cli_e2e.rs`
- Modify: `crates/shep-cli/CHANGELOG.md`, `docs/specs/deferred.md`

**Interfaces — produced:**

```rust
// render.rs
/// Renders apps as Flockfile TOML: one `[[app]]` table each, carrying only
/// the fields that differ from a spec default.
pub(crate) fn flockfile(apps: &[AppConfig]) -> Result<String, toml::ser::Error>;

// commands/import/mod.rs
pub fn import(streams: &mut Streams<'_>, fmt: Format, args: &ImportArgs) -> ExitCode;

// cli.rs
pub struct ImportArgs {
    /// Read this pm2 dump instead of `~/.pm2/dump.pm2`
    #[arg(long)]
    pub from: Option<PathBuf>,
    /// Write the Flockfile here instead of `./Flockfile.toml`
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Print the Flockfile that would be written, and write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Overwrite an existing Flockfile
    #[arg(long)]
    pub force: bool,
}

// output/rows.rs
/// One app `shep import` read out of a pm2 dump.
#[derive(Debug, Serialize)]
pub struct ImportRow {
    /// The app's name, which is also the key its instance rows were grouped by.
    pub name: String,
    /// The script the app runs.
    pub script: String,
    /// How many instances of it the dump recorded running.
    pub instances: u32,
    /// Whether the app has to set `SO_REUSEPORT` itself (pm2 cluster mode).
    pub reuse_port: bool,
}
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct ImportRows(pub Vec<ImportRow>);
```

Cargo shape: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`; the e2e case runs under `cargo test -p shep-cli --test cli_e2e`.

`import` takes **no `Client`** and is `fn`, not `async fn`: it reads a file, writes a file, and starts nothing (decision 6). `logs::flush_daemon` is the precedent for a verb whose dispatch arm never connects.

### Rendering

The renderer serializes a purpose-built projection, not `AppConfig` itself: `AppConfig` is `#[serde(default)]` across ~40 fields and would emit every one of them, burying the six that matter.

```rust
/// The subset of an `AppConfig` a pm2 import can produce, rendered as one
/// `[[app]]` table. Every field is skipped when it matches the spec default,
/// so an imported Flockfile reads as what the operator has to know about
/// rather than as a dump of every knob shep has.
#[derive(Debug, Serialize)]
struct Rendered {
    name: String,
    script: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interpreter: Option<String>,
    #[serde(skip_serializing_if = "is_one")]
    instances: u32,
    #[serde(skip_serializing_if = "is_true")]
    autorestart: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    restart_delay: Option<UpDuration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_memory: Option<MemSize>,
    #[serde(skip_serializing_if = "is_false")]
    merge_logs: bool,
    #[serde(skip_serializing_if = "is_false")]
    reuse_port: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    increment_var: Option<String>,
    // LAST, and not by style: TOML refuses a value after a table, so a map
    // declared above any scalar makes every scalar after it a serialization
    // error. `flockfile_round_trips_through_the_real_parser` is what catches
    // a reordering.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct Doc {
    #[serde(rename = "app")]
    apps: Vec<Rendered>,
}
```

**`app`, not `apps`.** `Flockfile`'s public *field* is `apps`, which is what makes the wrong key easy to write, but the document key is `app` and `RawFlockfile` is `#[serde(deny_unknown_fields)]` (`shep-core/src/config/flockfile.rs`), so `apps` produces a Flockfile shep refuses to parse.

### `--dry-run` and the envelope

Decision 12: `--dry-run` writes the rendered TOML to stdout and nothing else, with no envelope, so `shep import --dry-run > Flockfile.toml` produces a byte-exact file. `shep completions` already prints a raw artifact to stdout the same way. Without `--dry-run`, stdout carries the normal envelope over `ImportRows`. Notes go to stderr in both modes and both formats (decision 13).

- [ ] **Step 1: Write the failing tests.**

In `render.rs`:

```rust
    /// fails if the renderer emits a Flockfile shep cannot read back — a
    /// wrong document key (`apps` for `app`), a field name that drifted from
    /// `AppConfig`, or a map declared before a scalar. It parses with the
    /// REAL parser and compares against the apps that went in, so the
    /// projection cannot drift from `AppConfig` without this reddening.
    #[test]
    fn flockfile_round_trips_through_the_real_parser() {
        let apps = convert(dump::parse(include_str!("testdata/dump.pm2.json")).unwrap())
            .unwrap()
            .apps;
        let rendered = flockfile(&apps).unwrap();
        let parsed = Flockfile::parse(&rendered, FlockFormat::Toml).unwrap();
        assert_eq!(parsed.apps, apps);
    }

    /// fails if a spec default starts being written. An imported Flockfile
    /// listing all forty of shep's knobs is one nobody reads, and the two
    /// lines that matter — `reuse_port` and `max_memory` — are what gets
    /// lost in it.
    #[test]
    fn defaults_are_left_out() {
        let rendered = flockfile(&[AppConfig::minimal("web", "./srv")]).unwrap();
        assert_eq!(rendered.trim(), "[[app]]\nname = \"web\"\nscript = \"./srv\"");
    }

    /// fails if a value shep writes stops being one shep can parse back:
    /// 536870912 bytes must render as `512M` and 5000 ms as `5s`, because
    /// `MemSize` and `UpDuration` serialize as their string forms and a
    /// renderer emitting raw integers would produce a Flockfile whose
    /// `max_memory = 536870912` is a TOML integer where a string is expected.
    #[test]
    fn newtype_values_render_in_their_string_form() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.max_memory = Some(MemSize::from_bytes(536_870_912));
        app.restart_delay = Some(UpDuration::from_millis(5000));
        let rendered = flockfile(&[app]).unwrap();
        assert!(rendered.contains("max_memory = \"512M\""), "{rendered}");
        assert!(rendered.contains("restart_delay = \"5s\""), "{rendered}");
    }
```

In `commands/import/mod.rs`:

```rust
    /// fails if `import` writes over a Flockfile without being asked. The
    /// default output path is one an operator is likely to already have, and
    /// clobbering a hand-written one has nothing to undo it.
    #[test]
    fn an_existing_flockfile_is_not_overwritten_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("Flockfile.toml");
        std::fs::write(&out, "[[app]]\nname = \"mine\"\nscript = \"./mine\"\n").unwrap();
        let dump = dir.path().join("dump.pm2");
        std::fs::write(&dump, include_str!("testdata/dump.pm2.json")).unwrap();

        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let code = {
            let mut streams = Streams { out: &mut out_buf, err: &mut err_buf };
            import(&mut streams, Format::Table, &args(&dump, &out, false, false))
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(std::fs::read_to_string(&out).unwrap().contains("mine"));
    }

    /// fails if `--dry-run` writes a file, or wraps the Flockfile in an
    /// envelope. `shep import --dry-run > Flockfile.toml` must produce a
    /// file shep can parse; an envelope makes it one shep cannot.
    #[test]
    fn dry_run_prints_a_parseable_flockfile_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("dump.pm2");
        std::fs::write(&dump, include_str!("testdata/dump.pm2.json")).unwrap();
        let out = dir.path().join("Flockfile.toml");

        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let code = {
            let mut streams = Streams { out: &mut out_buf, err: &mut err_buf };
            import(&mut streams, Format::Table, &args(&dump, &out, true, false))
        };
        assert_eq!(code, ExitCode::Success);
        assert!(!out.exists(), "--dry-run writes nothing");
        let printed = String::from_utf8(out_buf).unwrap();
        assert_eq!(
            Flockfile::parse(&printed, FlockFormat::Toml).unwrap().apps.len(),
            3,
            "`shep import --dry-run > Flockfile.toml` must produce a file \
             shep can read back: {printed}"
        );
    }

    /// fails if the cluster warning or an ambiguous env key stops reaching
    /// the operator. Both are cutover blockers the design names: one surfaces
    /// as EADDRINUSE at first start otherwise, the other as an app missing
    /// half its configuration after a reboot.
    #[test]
    fn the_report_names_every_cluster_app_and_every_ambiguous_key() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("dump.pm2");
        std::fs::write(&dump, include_str!("testdata/dump.pm2.json")).unwrap();
        let out = dir.path().join("Flockfile.toml");

        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        {
            let mut streams = Streams { out: &mut out_buf, err: &mut err_buf };
            let _ = import(&mut streams, Format::Table, &args(&dump, &out, true, false));
        }
        let report = String::from_utf8(err_buf).unwrap();
        assert!(report.contains("api"), "{report}");
        assert!(report.contains("SO_REUSEPORT"), "{report}");
        for key in ["BUN_INSTALL", "JAVA_HOME", "DATABASE_URL"] {
            assert!(report.contains(key), "{key} was never named: {report}");
        }
    }
```

`args(dump, out, dry_run, force)` is the module's own one-line `ImportArgs` builder, mirroring `commands/trigger.rs`'s `args(..)` helper.

In `output/rows.rs`: `import_rows_do_not_drift`, instantiating `assert_no_drift` over a populated `ImportRows` with `|json| &json[0]`, matching `flock_rows_do_not_drift`.

In `main.rs`: the dispatch assertion for `Commands::Import`.

In `crates/shep-cli/tests/cli_e2e.rs`, one case: run the real binary with `--from` pointing at the fixture (via `concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands/import/testdata/dump.pm2.json")`) and `--out` inside a `TempDir`, assert exit 0, assert the written file parses, and assert **no daemon was started** — `paths.socket` must not exist afterwards, which is the e2e-only half of "import starts nothing". Carry `.timeout(CMD_TIMEOUT)` like every other case in that file.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** The verb: resolve the source (`--from`, else `$HOME/.pm2/dump.pm2`; no `$HOME` and no `--from` is `ExitCode::Usage` naming both), read it, `dump::parse`, `convert`, `render::flockfile`, then either print (dry run) or write. Notes render as one stderr line each; write the two counts as the first line:

```
read 4 instance rows for 3 apps from /home/deploy/.pm2/dump.pm2
```

Every error maps to an exit code: a missing dump → `Usage` (it names the path and says `pm2 save` writes one); a malformed dump → `InvalidConfig`; an app that will not normalize → `InvalidConfig`; a write failure → `Failure`.

Dispatch in `main.rs`, in the locked-handles block, with no client:

```rust
        // Reads a file and writes a file; starts nothing, so there is
        // nothing to ask the socket. `logs::flush_daemon` is the other arm
        // that finishes without a client.
        Commands::Import(ref args) => import::import(&mut streams, fmt, args),
```

- [ ] **Step 4: Run tests, confirm pass** — `cargo test -p shep-cli --bins`, then `cargo test -p shep-cli --test cli_e2e`.

- [ ] **Step 5: `docs/specs/deferred.md`** — remove the `import` half of the `import` + migration-guide entry; leave the `docs/migration.md` half until Task 16 writes it.

- [ ] **Step 6: CHANGELOG** — shep-cli: `shep import` added, with the cluster-mode and env-filtering behaviour named.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): shep import`

---

## Task 10: the daemon says it is ready, after the restore

**Files:**
- Create: `crates/shep-daemon/src/notify.rs`
- Modify: `crates/shep-daemon/src/lib.rs`, `crates/shep-daemon/src/boot.rs`, `crates/shep-daemon/src/testing.rs` (`AnnouncingRunner`), `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/commands/daemon.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`, `crates/shep-cli/CHANGELOG.md`

**Interfaces — produced, depended on by Tasks 11, 12, 16:**

```rust
// shep-daemon/src/notify.rs  (`#[cfg(unix)] pub(crate) mod notify;`)
/// The variable an init system sets on a service that reports its own readiness.
pub(crate) const NOTIFY_SOCKET_ENV: &str = "NOTIFY_SOCKET";

/// Sends `READY=1` to `$NOTIFY_SOCKET`, reporting whether there was one.
///
/// `Ok(false)` means the variable is unset — the ordinary case for a daemon
/// the CLI autostarted, and for launchd, which has no readiness protocol.
///
/// # Errors
/// - [`NotifyError::Unsupported`] — the address names the abstract namespace on a platform without one.
/// - [`NotifyError::Io`] — the socket could not be opened, or the datagram not sent.
pub(crate) fn notify_ready() -> Result<bool, NotifyError>;

/// Sends `READY=1` to one already-resolved address.
///
/// # Errors
/// As [`notify_ready`].
pub(crate) fn notify(target: &OsStr) -> Result<(), NotifyError>;

// shep-daemon/src/boot.rs
pub struct BootOptions {
    // ... existing fields ...
    /// Where to report readiness once the muster restore has finished, for
    /// an init system supervising this process directly. `None` — the
    /// ordinary case — reports nothing.
    ///
    /// The resolved address rather than a bool, and not read from the
    /// environment inside this crate: `std::env::set_var` is `unsafe` in
    /// edition 2024 and this crate is `#![deny(unsafe_code)]`, so a boot
    /// test could not establish an ambient `$NOTIFY_SOCKET` to observe the
    /// ordering against. The CLI reads the variable once, where it already
    /// reads every other `SHEP_*` override.
    pub notify_socket: Option<OsString>,
}

// shep-cli/src/cli.rs
pub struct DaemonArgs {
    pub no_restore: bool,
    /// Run supervised by an init system: do not expect to have been
    /// daemonized, and report readiness once the flock is back.
    #[arg(long)]
    pub foreground: bool,
}

// shep-cli/src/commands/daemon.rs — one more parameter, so the environment
// read stays at the one call site in `run_daemon` and this stays testable.
pub fn boot_options(
    config: &DaemonConfig,
    args: &DaemonArgs,
    notify_socket: Option<&OsStr>,
) -> BootOptions;
```

Cargo shape: this task crosses shep-daemon and shep-cli, so it uses `--workspace`: `cargo test --workspace --all-features --lib --bins` while iterating.

### No dependency, no unsafe

`sd_notify` is one datagram, and `std` can address both socket shapes (decision 17):

- A filesystem path (`/run/systemd/notify`, what a modern systemd system service gets): `UnixDatagram::unbound()?.send_to(b"READY=1\n", path)`.
- An `@`-prefixed abstract name: `std::os::unix::net::SocketAddr::from_abstract_name` + `UnixDatagram::send_to_addr`, both stable since **1.70** and both `#[cfg(target_os = "linux")]`. On any other unix an `@` address is `NotifyError::Unsupported` — there is no abstract namespace to reach, and saying so beats sending into a file named `@…`.

### Where it fires, and why exactly there

At the **end** of `boot()`, after `restore_flock` and after the `RpcContext` is assembled — the last thing before the `Ok(RunningDaemon { .. })`. Decision 16: the unit goes green when the flock exists, not when the process execs, which is what makes "did it survive the reboot?" answerable and turns a hung restore into a failed start rather than a green unit supervising nothing.

A failed notify is a `tracing::warn!` and the boot continues. The daemon is fully functional; what fails is systemd's knowledge of it, and systemd's own `TimeoutStartSec` is the honest reporter of that. Killing a working daemon over a failed datagram would be the worse outcome.

- [ ] **Step 1: Write the failing tests** in `notify.rs`:

```rust
    /// fails if the datagram never leaves, or leaves with the wrong bytes.
    /// systemd matches `READY=1` literally; `ready=1`, `READY=true`, or a
    /// missing newline all leave the unit hanging until TimeoutStartSec,
    /// and none of them is visible from inside this process.
    #[test]
    fn ready_reaches_a_listening_socket_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        // Bounded: an unread datagram would otherwise park this test
        // forever rather than failing it.
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        notify(path.as_os_str()).unwrap();

        let mut buf = [0u8; 64];
        let read = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..read], b"READY=1\n");
    }

    /// fails if a bad address is swallowed. A silent success here is a unit
    /// that hangs for ninety seconds and then reports a timeout with nothing
    /// in the journal to say why.
    #[test]
    fn an_address_nothing_is_listening_on_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nobody-here.sock");
        assert!(notify(path.as_os_str()).is_err());
    }
```

In `boot.rs`'s `mod tests`, one case proving the ordering — the whole point of the flag:

```rust
    /// fails if the notification is sent before the muster restore finishes.
    /// That ordering is the entire reason `Type=notify` was chosen over
    /// `Type=simple`: a unit that goes green at exec time reports a flock
    /// that is not up yet, and a restore that hangs reads as a healthy
    /// service supervising nothing.
    ///
    /// The socket is read AFTER `boot` returns rather than raced against it:
    /// the datagram is queued by the kernel, so its presence at that point
    /// proves it was sent, and the restored flock is asserted alongside so a
    /// notify that fired first could not pass by arriving anyway.
    #[tokio::test]
    async fn readiness_is_reported_only_once_the_roll_is_restored() {
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        write_atomic(
            &paths.snapshot,
            &FlockSnapshot {
                version: SNAPSHOT_VERSION,
                saved_at_ms: 0,
                apps: vec![SavedApp {
                    app: AppConfig::minimal("web", "./srv"),
                    instances_running: 1,
                }],
            },
        )
        .unwrap();

        // Inside the TempDir and short: macOS caps a unix socket path near
        // 97 characters, which `test_paths` already keeps this under.
        let notify_path = dir.path().join("n.sock");
        let listener = std::os::unix::net::UnixDatagram::bind(&notify_path).unwrap();
        // Bounded: a datagram that never arrives must fail this case, not
        // park it. Two mutations in an earlier phase hung the suite instead
        // of reddening it, for exactly this reason.
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // Both events land on the SAME socket, so the assertion is on their
        // ORDER rather than on their presence. This is the whole design of
        // the case: reading only READY=1 after `boot` returns would pass on
        // a notify moved to the TOP of `boot`, because the datagram is
        // queued by the kernel and is still there whenever the test looks.
        // A marker sent from inside the restore's own spawn is the only
        // thing that distinguishes the two orders. AF_UNIX SOCK_DGRAM
        // enqueues synchronously, and these two sends are strictly
        // sequential in program order, so the queue order is the program
        // order.
        let runner = AnnouncingRunner::new(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            &notify_path,
        );

        let daemon = boot(
            runner,
            paths.clone(),
            BootOptions {
                restore: true,
                notify_socket: Some(notify_path.clone().into_os_string()),
                ..BootOptions::default()
            },
        )
        .await
        .unwrap();

        let mut buf = [0u8; 64];
        let read = listener.recv(&mut buf).unwrap();
        assert_eq!(
            &buf[..read],
            b"SPAWNED\n",
            "READY=1 arrived before the roll was restored: a unit that goes \
             green at exec time reports a flock that is not up yet, and a \
             restore that hangs reads as a healthy service supervising nothing"
        );
        let read = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..read], b"READY=1\n");

        daemon.context().shutdown();
        daemon.run().await.unwrap();
    }
```

`AnnouncingRunner` is a `#[cfg(test)]` wrapper in `testing.rs`: a `ProcessRunner` that sends one `SPAWNED\n` datagram to a path before delegating to the runner it wraps. Fifteen lines, and it is what turns this case from a delivery check into an ordering one.

That ordering case is the whole reason `BootOptions` carries the resolved address rather than a bool: a test cannot establish an ambient `$NOTIFY_SOCKET` to observe the ordering against, because `std::env::set_var` is `unsafe` in edition 2024 and both crates refuse unsafe. `commands::daemon::boot_options` reads `std::env::var_os(NOTIFY_SOCKET_ENV)` when `--foreground` is set and hands the value down — the same place every `SHEP_*` override is already read, and one fewer environment read inside the daemon crate. `notify_ready()` stays as the convenience over `notify()` that `boot_options` calls through.

In `commands/daemon.rs`:

```rust
    /// fails if `--foreground` stops reaching the boot option. The flag is
    /// the only thing that turns readiness reporting on, and a unit whose
    /// ExecStart lost it hangs until TimeoutStartSec with nothing to say why.
    #[test]
    fn the_foreground_flag_reaches_the_boot_options() {
        let config = DaemonConfig::load(None, &|_| None).unwrap();
        let bare = boot_options(
            &config,
            &DaemonArgs { no_restore: false, foreground: false },
            None,
        );
        assert!(bare.notify_socket.is_none(), "an autostarted daemon reports to nobody");

        let supervised = boot_options(
            &config,
            &DaemonArgs { no_restore: false, foreground: true },
            Some(OsStr::new("/run/systemd/notify")),
        );
        assert_eq!(
            supervised.notify_socket.as_deref(),
            Some(OsStr::new("/run/systemd/notify"))
        );

        // Without the flag the address is ignored, so a shep the CLI
        // autostarted from inside some other notify-type service cannot
        // report ITS readiness by accident.
        let unflagged = boot_options(
            &config,
            &DaemonArgs { no_restore: false, foreground: false },
            Some(OsStr::new("/run/systemd/notify")),
        );
        assert!(unflagged.notify_socket.is_none());
    }
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement**, including `#[cfg(unix)] pub(crate) mod notify;` in `lib.rs`'s **Platform** taxonomy block, with a one-line entry in the module-taxonomy doc list next to `sys`/`privilege`/`tokio_runner` (that list is prose the crate doc renders; leaving a module out of it is the drift this crate has already corrected twice).

- [ ] **Step 4: Run tests, confirm pass.**

- [ ] **Step 5: CHANGELOGs** — shep-daemon: readiness reported to `$NOTIFY_SOCKET` after the muster restore. shep-cli: `shep daemon --foreground`.

- [ ] **Step 6: Task gate, then commit** — `feat(daemon): report readiness once the flock is back`

---

## Task 11: rendering a unit and a plist

**Files:**
- Create: `crates/shep-cli/src/commands/startup/unit.rs`, `crates/shep-cli/src/commands/startup/mod.rs` (module tree only, this task)
- Modify: `crates/shep-cli/src/commands/mod.rs`

**Interfaces — produced, depended on by Task 12:**

```rust
/// Everything a generated init unit carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnitSpec {
    /// The user the daemon runs as.
    pub user: String,
    /// This binary's own resolved path.
    pub exec: PathBuf,
    /// `$SHEP_HOME` the daemon is given.
    pub home: PathBuf,
    /// `PATH` captured from the invoking environment — the mechanism that
    /// makes an interpreter installed under `~/.bun` or `~/.cargo` findable
    /// after a reboot.
    pub path: OsString,
    /// The daemon's working directory.
    pub working_dir: PathBuf,
}

pub(crate) fn systemd_unit(spec: &UnitSpec) -> String;
pub(crate) fn launchd_plist(spec: &UnitSpec) -> String;

/// `/etc/systemd/system/shep-<user>.service`
pub(crate) fn systemd_unit_path(user: &str) -> PathBuf;
/// `io.github.turtiesocks.shep.<user>`
pub(crate) fn launchd_label(user: &str) -> String;
/// `/Library/LaunchDaemons/<label>.plist`
pub(crate) fn launchd_plist_path(user: &str) -> PathBuf;

/// Which init system this build targets. Linux is systemd, macOS is launchd;
/// there is no runtime detection because there is nothing else either target
/// could be, and openrc/rc.d are named as deferred in `docs/specs/deferred.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Init { Systemd, Launchd }
```

Cargo shape: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`.

The systemd unit, exactly:

```ini
[Unit]
Description=shep process manager for <user>
After=network.target

[Service]
Type=notify
NotifyAccess=main
User=<user>
WorkingDirectory=<working_dir>
Environment="SHEP_HOME=<home>"
Environment="PATH=<path>"
ExecStart=<exec> daemon --foreground
ExecReload=<exec> reload all
ExecStop=<exec> kill
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Four things about it are decisions, not formatting:

- **`ExecStart` is the daemon** (decision 14). `shep muster` is a client verb that talks to a daemon; under `Type=notify` systemd supervises the process it starts, so `ExecStart=shep muster` would have systemd supervising a process that exits immediately. The restore still happens, because the daemon restores the roll at boot.
- **`Environment=` values are quoted, and `%` is escaped as `%%`.** A `PATH` containing a space breaks an unquoted value; a `%` is a systemd specifier and silently expands to something else. Both are real in a captured `PATH`.
- **No `KillMode=`.** The default (`control-group`) is the safe one: `ExecStop=shep kill` runs the graceful teardown, and whatever survives it is still cleaned up. `KillMode=process` would leave an orphaned flock behind any daemon that died without running its teardown.
- **No `TimeoutStartSec=`.** The default 90s is exactly what turns a hung restore into a failed start, which is the property decision 16 is buying.

The launchd plist, exactly:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string><label></string>
	<key>ProgramArguments</key>
	<array>
		<string><exec></string>
		<string>daemon</string>
		<string>--foreground</string>
	</array>
	<key>UserName</key><string><user></string>
	<key>WorkingDirectory</key><string><working_dir></string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>SHEP_HOME</key><string><home></string>
		<key>PATH</key><string><path></string>
	</dict>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key>
	<dict><key>SuccessfulExit</key><false/></dict>
	<key>StandardOutPath</key><string><home>/logs/shepd.out.log</string>
	<key>StandardErrorPath</key><string><home>/logs/shepd.err.log</string>
</dict>
</plist>
```

`--foreground` is on both, and on launchd it carries only half its meaning: launchd has no readiness protocol, so `$NOTIFY_SOCKET` is unset and `notify_ready` reports `Ok(false)`. It stays on the argv anyway so both platforms invoke the daemon through the same documented entry point rather than one of them depending on the bare hidden verb's private contract (decision 15).

`KeepAlive`/`SuccessfulExit=false` is launchd's `Restart=on-failure`.

- [ ] **Step 1: Write the failing tests** in `unit.rs`:

```rust
    fn spec() -> UnitSpec {
        UnitSpec {
            user: "deploy".to_string(),
            exec: PathBuf::from("/usr/local/bin/shep"),
            home: PathBuf::from("/home/deploy/.shep"),
            path: OsString::from("/home/deploy/.bun/bin:/usr/local/bin:/usr/bin:/bin"),
            working_dir: PathBuf::from("/home/deploy"),
        }
    }

    /// fails if any of the four ExecStart/Reload/Stop/Type lines drifts.
    /// Each is load-bearing: Type=notify is what makes the unit go green on
    /// a restored flock, and an ExecStart naming `muster` would have systemd
    /// supervising a client that exits immediately.
    #[test]
    fn the_systemd_unit_carries_the_four_lines_that_matter() {
        let unit = systemd_unit(&spec());
        assert!(unit.contains("Type=notify"), "{unit}");
        assert!(unit.contains("ExecStart=/usr/local/bin/shep daemon --foreground"), "{unit}");
        assert!(unit.contains("ExecReload=/usr/local/bin/shep reload all"), "{unit}");
        assert!(unit.contains("ExecStop=/usr/local/bin/shep kill"), "{unit}");
        assert!(unit.contains("WantedBy=multi-user.target"), "{unit}");
    }

    /// fails if an Environment value stops being quoted, or a `%` stops
    /// being escaped. A PATH with a space silently truncates at the space;
    /// a `%` is a systemd specifier and expands to something else entirely.
    /// Both are reachable from a real captured PATH, and neither is visible
    /// until an interpreter is not found after a reboot.
    #[test]
    fn environment_values_are_quoted_and_specifier_escaped() {
        let mut spec = spec();
        spec.path = OsString::from("/opt/my tools/bin:/usr/bin:/pct%dir/bin");
        let unit = systemd_unit(&spec);
        assert!(
            unit.contains(r#"Environment="PATH=/opt/my tools/bin:/usr/bin:/pct%%dir/bin""#),
            "{unit}"
        );
    }

    /// fails if plist values stop being XML-escaped. A `&` in a path makes
    /// the whole plist unparseable, and launchd's refusal names the file
    /// rather than the character.
    #[test]
    fn plist_values_are_xml_escaped() {
        let mut spec = spec();
        spec.home = PathBuf::from("/home/r&d/.shep");
        let plist = launchd_plist(&spec);
        assert!(plist.contains("<string>/home/r&amp;d/.shep</string>"), "{plist}");
        assert!(!plist.contains("r&d"), "a raw ampersand makes the plist unparseable");
    }

    /// fails if `systemd-analyze verify` rejects the generated unit —
    /// systemd's own parser is the only thing that can say the unit is
    /// well-formed, and every assertion above is our opinion of it.
    ///
    /// Skips, loudly, where the tool does not exist: this is a macOS
    /// development machine's ordinary state, and a test that failed there
    /// would be disabled rather than fixed. On the Linux CI leg it runs.
    #[test]
    fn systemd_analyze_accepts_the_generated_unit() {
        let Ok(analyze) = which_systemd_analyze() else {
            eprintln!("skipping: systemd-analyze is not on this machine");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep-deploy.service");
        std::fs::write(&path, systemd_unit(&spec())).unwrap();
        let out = std::process::Command::new(analyze)
            .arg("verify")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "systemd-analyze verify rejected the unit:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
```

`which_systemd_analyze` probes `/usr/bin/systemd-analyze` and `/bin/systemd-analyze` with `Path::exists` rather than shelling out to `which` — one fewer process and no PATH dependency in a test.

> **Note on `systemd-analyze verify`:** it emits warnings on stderr for a unit whose `ExecStart` binary does not exist on the machine, and still exits 0. Assert on the **exit status**, and print stderr in the failure message rather than asserting it is empty — an empty-stderr assertion would fail on every machine where `/usr/local/bin/shep` is not installed, which is all of them.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** Both renderers are `format!` over the spec with two escape helpers (`systemd_environment_value`, `xml_text`), each with its own doc naming what it escapes and why.

- [ ] **Step 4: Run tests, confirm pass**, and say in the report whether `systemd-analyze` ran or skipped on your machine.

- [ ] **Step 5: Task gate, then commit** — `feat(cli): render a systemd unit and a launchd plist`

---

## Task 12: `shep startup` / `shep unstartup`

**Files:**
- Modify: `crates/shep-cli/src/commands/startup/mod.rs`, `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/main.rs`, `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/Cargo.toml`
- Modify: `crates/shep-cli/CHANGELOG.md`, `docs/specs/deferred.md`

**Interfaces — produced:**

```rust
// commands/startup/mod.rs
pub fn startup(streams: &mut Streams<'_>, fmt: Format, explicit_home: Option<&Path>, args: &StartupArgs) -> ExitCode;
pub fn unstartup(streams: &mut Streams<'_>, fmt: Format, args: &StartupArgs) -> ExitCode;

/// The user a generated unit runs the daemon as: `--user` when given, else
/// `$SUDO_USER`, else the invoking user.
pub(crate) fn target_user(explicit: Option<&str>, sudo_user: Option<&str>, invoking: &str) -> String;

/// The `$SHEP_HOME` a generated unit carries: an explicit `--home`/`$SHEP_HOME`
/// when given, else the target user's own `<passwd home>/.shep`.
pub(crate) fn target_home(explicit: Option<&Path>, user_home: &Path) -> PathBuf;

/// Whether this process can install a system unit.
///
/// A value rather than a `geteuid()` call inside [`install`], because a test
/// cannot become root and one that skipped when unprivileged would never run
/// anywhere. [`startup`] reads `geteuid()` once and passes the answer down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Privilege { Root, Unprivileged }

/// Everything resolved before any privilege is needed: the unit to render,
/// where it goes, and the command to print if this process cannot install it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupPlan {
    pub init: Init,
    pub spec: UnitSpec,
    pub unit_path: PathBuf,
    /// The launchd label, unused on systemd.
    pub label: String,
}

/// Writes and enables the unit, or prints the command that would.
pub(crate) fn install(
    streams: &mut Streams<'_>,
    fmt: Format,
    plan: &StartupPlan,
    privilege: Privilege,
) -> ExitCode;

/// Disables and removes the unit, or prints the command that would.
pub(crate) fn remove(
    streams: &mut Streams<'_>,
    fmt: Format,
    plan: &StartupPlan,
    privilege: Privilege,
) -> ExitCode;

// cli.rs
pub struct StartupArgs {
    /// Install the unit for this user instead of the invoking one
    #[arg(long)]
    pub user: Option<String>,
}

// output/rows.rs
/// One step `shep startup` or `shep unstartup` took.
#[derive(Debug, Serialize)]
pub struct StartupStep {
    /// What was done: `wrote`, `removed`, `ran`.
    pub action: &'static str,
    /// The file or command it was done to.
    pub target: String,
    /// `ok`, or the failure in one line.
    pub result: String,
}
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct StartupSteps(pub Vec<StartupStep>);
```

Add to `crates/shep-cli/Cargo.toml`:

```toml
# `geteuid` (the privilege gate `shep startup` refuses on) and
# `User::from_name` (the target user's own home, which is NOT $HOME under
# sudo). Both are behind nix's `user` feature, which the workspace entry
# already enables. A cfg(unix) table because nix does not build on Windows
# and this crate's Windows leg compiles everything outside `commands/`.
[target.'cfg(unix)'.dependencies]
nix.workspace = true
```

Cargo shape: `-p shep-cli`, tested with `cargo test -p shep-cli --bins`.

### Three gates, in order

1. **The target user.** `--user`, else `$SUDO_USER`, else the invoking user's name. Under `sudo shep startup`, `geteuid()` is 0 and the invoking user is `root` — a unit installed for `root` would supervise root's flock, not the operator's.
2. **The `$SHEP_HOME`.** An explicit `--home`/`$SHEP_HOME` wins; otherwise the **target user's** passwd home plus `.shep`, matching `ShepPaths::resolve`'s own `home_dir.join(".shep")`. Never this process's `$HOME`: `sudo` resets it to root's, so the unit would carry `/root/.shep` and restore nothing after a reboot — silently, months later (decision 19).
3. **The privilege gate.** `geteuid() == 0` installs and enables. Otherwise print the exact command to run — `sudo <exec> startup --user <user> --home <home>`, fully resolved so the reprint needs no thought — and exit `ExitCode::Failure`, non-zero so a script notices. shep never escalates on its own (decision 18).

**And a fourth refusal:** if the resolved `$SHEP_HOME` does not exist, refuse with `ExitCode::Usage`, naming the path and `--home`. The overwhelmingly likely cause is gate 2's sudo trap, and a unit pointing at a non-existent home is a reboot that restores nothing.

`startup` and `unstartup` are dispatched from `main.rs`'s **early block**, alongside `Completions` and `Daemon`, before `resolve_paths`: they resolve their own home from the target user (gate 2), so routing them through the shared `$HOME` gate would impose a requirement they do not have and would hand them the wrong answer under sudo.

- [ ] **Step 1: Write the failing tests** in `startup/mod.rs`:

```rust
    /// fails if `$SUDO_USER` stops winning over the invoking user. Under
    /// `sudo shep startup` the invoking user IS root, so a resolution that
    /// ignored SUDO_USER would install a unit supervising root's flock
    /// while the operator's stayed down — and the unit would look correct.
    #[test]
    fn the_target_user_prefers_an_explicit_name_then_sudo_user() {
        assert_eq!(target_user(Some("deploy"), Some("rin"), "root"), "deploy");
        assert_eq!(target_user(None, Some("rin"), "root"), "rin");
        assert_eq!(target_user(None, None, "rin"), "rin");
    }

    /// fails if the home falls back to this process's `$HOME`. `sudo` resets
    /// HOME to root's, so a unit built from it carries /root/.shep and
    /// restores nothing after a reboot — the failure the whole gate exists
    /// to prevent, and one that surfaces months later.
    #[test]
    fn the_target_home_comes_from_the_target_user_not_the_invoker() {
        assert_eq!(
            target_home(None, Path::new("/home/rin")),
            Path::new("/home/rin/.shep")
        );
        assert_eq!(
            target_home(Some(Path::new("/srv/shep")), Path::new("/home/rin")),
            Path::new("/srv/shep")
        );
    }

    /// fails if an unprivileged startup exits 0, or prints a command the
    /// operator cannot paste. Exit 0 makes a script believe a unit was
    /// installed; a command missing --home re-runs the sudo trap the gate
    /// above exists to close.
    #[test]
    fn an_unprivileged_startup_prints_the_command_and_exits_non_zero() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams { out: &mut out, err: &mut err };
            install(&mut streams, Format::Table, &plan_for_test(&home), Privilege::Unprivileged)
        };
        assert_ne!(code, ExitCode::Success);
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains("sudo"), "{printed}");
        assert!(printed.contains("--home"), "{printed}");
        assert!(printed.contains(home.to_str().unwrap()), "{printed}");
    }

    /// fails if a `$SHEP_HOME` that does not exist is accepted. That is what
    /// the sudo trap produces when nobody notices it, and the unit it yields
    /// is one that boots cleanly and restores an empty flock.
    #[test]
    fn a_shep_home_that_does_not_exist_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams { out: &mut out, err: &mut err };
            // Root, deliberately: an unprivileged run would refuse for the
            // other reason and this case would pass without ever exercising
            // the home check.
            install(&mut streams, Format::Table, &plan_for_test(&missing), Privilege::Root)
        };
        assert_eq!(code, ExitCode::Usage);
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains(missing.to_str().unwrap()), "{printed}");
        assert!(
            !plan_for_test(&missing).unit_path.exists(),
            "a refused startup writes no unit"
        );
    }

    /// The unit path a `StartupPlan` built for a test points at, so `install`
    /// can be driven without writing into `/etc` or `/Library`. Every field
    /// but `home` is fixed; `home` is what each case varies.
    fn plan_for_test(home: &Path) -> StartupPlan { /* .. */ }
```

The privilege gate must be a **parameter**, not a `geteuid()` call inside the function under test — a test cannot become root, and one that skipped when unprivileged would never run anywhere. `startup()` reads `geteuid()` once and hands `install()` a `Privilege` value; `install()` is what the tests drive. Say this in `install`'s own doc.

**`install` validates before it writes or runs anything**, and that ordering is load-bearing for the tests as well as for the operator: the only case that passes `Privilege::Root` is the one that must be refused, and it would otherwise write into `/etc/systemd/system` and shell out to `systemctl` on the machine running the suite. `StartupPlan::unit_path` is a field rather than a call to `systemd_unit_path` inside `install` for the same reason — a test points it into a `TempDir`. **No test in this phase may reach a `systemctl` or `launchctl` invocation.** Task 16's runbook is where those are exercised, by hand, on a machine somebody meant to change.

In `main.rs`: dispatch assertions for both verbs, and a case pinning that neither reaches `resolve_paths` — the same shape as `completions_never_resolves_paths`, with that test's own honest note about what it can and cannot catch.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement.** Privileged install, in order, each step a `StartupStep` row:

| Init | Steps |
|---|---|
| systemd | write `/etc/systemd/system/shep-<user>.service` (0644) → `systemctl daemon-reload` → `systemctl enable --now shep-<user>.service` |
| launchd | write `/Library/LaunchDaemons/<label>.plist` (0644) → `launchctl bootstrap system <plist>` |

`unstartup` reverses it:

| Init | Steps |
|---|---|
| systemd | `systemctl disable --now shep-<user>.service` → remove the unit → `systemctl daemon-reload` |
| launchd | `launchctl bootout system/<label>` → remove the plist |

A command that fails is a row with its stderr's first line as `result`, and the verb exits non-zero **after** running the remaining steps — a half-installed unit is worse than a fully-attempted one, and the operator needs every row to know which half.

`unstartup` on an already-absent unit is a success with an `absent` row, matching `flush --daemon`'s treatment of a log file that is not there.

- [ ] **Step 4: Run tests, confirm pass.**

- [ ] **Step 5: `docs/specs/deferred.md`** — remove the `startup`/`unstartup` entry. Update the entry's neighbours if removing it leaves a sentence claiming something the tree no longer matches.

- [ ] **Step 6: CHANGELOG** — shep-cli: `shep startup`/`shep unstartup`, the privilege rule, and the two paths written.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): shep startup and unstartup`

---

## Task 13: sampling splits from enforcement

**Files:**
- Create: `crates/shep-daemon/src/limits/stats.rs`
- Modify: `crates/shep-daemon/src/limits/sample.rs`, `crates/shep-daemon/src/limits/mod.rs`, `crates/shep-daemon/src/extras.rs`, `crates/shep-daemon/src/testing.rs`, `benches/benches/memory_sample.rs`
- Modify: `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced, depended on by Task 14:**

```rust
// limits/sample.rs
pub struct ProcessRss {
    pub pid: u32,
    pub parent: Option<u32>,
    pub bytes: u64,
    /// Accumulated CPU time in CPU-milliseconds, as the OS reports it.
    ///
    /// Cumulative since the process started, not a rate: a percentage is a
    /// delta between two readings divided by the wall time between them.
    /// Bigger than the process's wall-clock lifetime on a multi-core
    /// machine, which is why a percentage over 100 is honest rather than a
    /// bug.
    pub cpu_ms: u64,
}

impl TreeIndex {
    /// Sums accumulated CPU time over `root` and every descendant, exactly
    /// as [`Self::sum_from`] sums resident memory.
    pub(crate) fn cpu_from(&self, root: u32) -> u64;
}

// limits/stats.rs
/// One sheep's live resource reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SheepStats {
    /// Tree CPU over the window since the last periodic baseline, as a
    /// percentage of one core. `None` when this pid has no baseline yet.
    pub cpu_percent: Option<f32>,
    /// Tree resident set size, current as of the reading that produced this.
    pub memory_bytes: u64,
}

/// Which sheep are worth sampling, and what their CPU counters read at the
/// last periodic tick.
pub(crate) struct StatsState { /* .. */ }

impl StatsState {
    pub(crate) fn new(sampler: Arc<dyn MemorySampler>) -> Self;
    /// Starts sampling `root_pid` for `id`. Re-watching an id replaces the
    /// previous pid — a respawn gives the same id a new one.
    pub(crate) fn watch(&self, id: u32, root_pid: u32);
    /// Stops sampling `id`. A no-op for an id never watched.
    pub(crate) fn unwatch(&self, id: u32);
    /// Records every watched root's CPU counter from one periodic reading.
    /// The ONLY writer of the baseline — see this type's own doc.
    pub(crate) fn record_baseline(&self, index: &TreeIndex, now: Instant);
    /// [`Self::record_baseline`] over a reading this call takes itself, for
    /// a caller that has not already built an index. The polling tick uses
    /// the other one: it has an index in hand and must not walk the process
    /// table twice per tick.
    pub(crate) fn record_baseline_now(&self, now: Instant);
    /// One live reading per watched sheep, keyed by root pid. Blocking: it
    /// performs the syscall walk itself.
    pub(crate) fn sample_now(&self) -> HashMap<u32, SheepStats>;
}

// extras.rs
pub struct Extras {
    // ... existing fields ...
    /// Live resource readings, shared with the RPC layer so `flock` and
    /// `describe` can take one on demand.
    pub stats: Arc<StatsState>,
}
```

**Cargo shape for this task: `-p shep-daemon`, and the inner loop is the extras-inclusive one** — this task changes `extras.rs`, and `--skip extras::` would hide exactly the 44 tests it can break:

```
cargo test -p shep-daemon --lib --all-features extras:: -- --skip watch::
cargo test -p shep-daemon --lib --all-features -- --skip watch:: --skip extras::
```

`benches/` is its own workspace with its own build lock and is run separately: `cargo bench --manifest-path benches/Cargo.toml -- --test`.

### What actually changes, and what must not

- **One tick, two consumers** (decision 21). `PollingEnforcer`'s loop already builds a `TreeIndex` once per tick; it now calls `stats.record_baseline(&index, Instant::now())` with the same index before the enforcement pass. A second loop would double a measured 5.77ms syscall walk to buy nothing.
- **Enforcement is unchanged.** It still runs only for armed ids, still self-disarms on breach, still backpressures on a full `breaches` channel. Nothing in `LimitEnforcer`'s contract moves.
- **Watching is unconditional.** `arm_instance` currently returns `None` when an app configures neither `max_memory` nor a `liveness_probe`; every sheep with a pid now gets an `InstanceExtras` because every sheep is sampled. The `wants_anything` warning branch for a pid-less entry stays exactly as it is.
- **`sample_now` never writes the baseline** (decision 22). Two `flock` calls a moment apart would otherwise divide by a near-zero window and report nonsense.

### `.with_cpu()` is required, and it is easy to miss

`Process::accumulated_cpu_time()` is populated **only** when the refresh asked for CPU: sysinfo 0.38.4 gates it on `refresh_kind.cpu()` on every platform (`src/unix/linux/process.rs`, `src/unix/apple/macos/process.rs`). The existing sampler asks for `ProcessRefreshKind::nothing().with_memory()`, so without `.with_cpu()` every reading is `0` and every percentage is `0.0` — a plausible, wrong number rather than an error. `MINIMUM_CPU_UPDATE_INTERVAL` and the two-refresh rule do **not** apply: they govern `cpu_usage()`, which shep does not use. `accumulated_cpu_time` is a counter and is correct on the first read.

### Three call sites break, and one of them is in another workspace

Adding a field to `ProcessRss` breaks every struct literal:

- `crates/shep-daemon/src/limits/sample.rs` and `mod.rs` test helpers (`fn rss(pid, parent, bytes)`) — keep `rss` as it is, defaulting `cpu_ms` to `0`, and add `fn rss_cpu(pid, parent, bytes, cpu_ms)` beside it. Every existing memory case then stays byte-for-byte unchanged, which is what makes a regression in one of them mean something.
- `crates/shep-daemon/src/testing.rs`'s `ScriptedSampler` readings.
- **`benches/benches/memory_sample.rs`**, two literals in `synthetic_process_tree`. That crate is its own workspace, so the workspace gate will not catch it — `cargo bench --manifest-path benches/Cargo.toml -- --test` is what does.

`crates/shep-daemon/tests/external_impls.rs` only *implements* `MemorySampler` with a `todo!()` body and constructs no literal, so it needs no change — confirm rather than assume.

- [ ] **Step 1: Write the failing tests.**

In `sample.rs`:

```rust
    // fails if `cpu_from` sums only descendants, or double-counts a pid
    // reachable by two paths — the same two mutations `tree_rss`'s own
    // cases pin for memory, which cannot see a CPU-side regression.
    #[test]
    fn cpu_sums_the_whole_tree_including_the_root() {
        let table = [
            rss_cpu(100, None, 1024, 500),
            rss_cpu(101, Some(100), 2048, 250),
            rss_cpu(102, Some(101), 4096, 125),
        ];
        let index = TreeIndex::build(&table);
        assert_eq!(index.cpu_from(100), 875);
        assert_eq!(index.cpu_from(101), 375);
        assert_eq!(index.sum_from(100), 7168, "memory must be unaffected");
    }
```

In `stats.rs`:

```rust
    /// fails if a sheep with no baseline reports a number. A process
    /// spawned since the last tick has no honest CPU figure, and one
    /// invented from a 50 ms window is worse than an empty cell.
    #[tokio::test(start_paused = true)]
    async fn a_sheep_with_no_baseline_reports_no_cpu_but_still_reports_memory() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![
            rss_cpu(100, None, 4096, 1000),
        ]]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);

        let now = stats.sample_now();
        assert_eq!(now[&100].cpu_percent, None);
        assert_eq!(now[&100].memory_bytes, 4096, "memory is always current");
    }

    /// fails if the delta is computed against the wrong pair of readings.
    /// 1500 CPU-ms over a 15 s window is 10% of one core; a percentage
    /// computed against the process's whole accumulated time instead would
    /// read 16.7%, and against the wrong elapsed window, anything at all.
    #[tokio::test(start_paused = true)]
    async fn cpu_is_the_delta_since_the_periodic_baseline() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 2_500)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);

        stats.record_baseline_now(Instant::now());
        tokio::time::advance(MEMORY_POLL_INTERVAL).await;

        let now = stats.sample_now();
        let cpu = now[&100].cpu_percent.expect("a baseline exists");
        assert!((cpu - 10.0).abs() < 0.01, "expected ~10%, got {cpu}");
    }

    /// fails if an on-demand read writes the baseline. Two `flock` calls a
    /// moment apart would then divide a near-zero CPU delta by a near-zero
    /// window — the second call reporting anything from 0% to thousands,
    /// depending on rounding.
    #[tokio::test(start_paused = true)]
    async fn a_second_read_a_moment_later_still_measures_from_the_periodic_baseline() {
        // Three readings: the baseline, then two on-demand ones a
        // millisecond apart. The CPU counter barely moves between the last
        // two, which is exactly the shape that makes a baseline-writing
        // implementation divide ~1 CPU-ms by ~1 ms and report ~100%.
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 2_500)],
            vec![rss_cpu(100, None, 4096, 2_501)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);
        stats.record_baseline_now(Instant::now());
        tokio::time::advance(MEMORY_POLL_INTERVAL).await;

        let first = stats.sample_now()[&100].cpu_percent.unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        let second = stats.sample_now()[&100].cpu_percent.unwrap();
        assert!(
            (first - 10.0).abs() < 0.01,
            "1500 CPU-ms over 15 s is 10%, got {first}"
        );
        assert!(
            (second - 10.0).abs() < 0.02,
            "the second read divided by the gap between the two READS rather \
             than by the window since the tick: {first} then {second}"
        );
    }

    /// fails if `unwatch` stops pruning. A pid whose sheep is gone would
    /// otherwise be sampled forever, and — worse — its baseline would be
    /// inherited by whatever process the OS next gives that pid to.
    #[tokio::test(start_paused = true)]
    async fn an_unwatched_sheep_leaves_no_baseline_behind() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 2_500)],
            vec![rss_cpu(100, None, 4096, 9_000)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);
        stats.record_baseline_now(Instant::now());

        stats.unwatch(1);
        tokio::time::advance(MEMORY_POLL_INTERVAL).await;
        stats.record_baseline_now(Instant::now());
        assert!(
            stats.sample_now().is_empty(),
            "an unwatched sheep must not still be sampled"
        );

        // The pid comes back — which the OS really does do — and must NOT
        // inherit the counter of whatever held it before.
        stats.watch(2, 100);
        assert_eq!(
            stats.sample_now()[&100].cpu_percent,
            None,
            "a recycled pid must start from no baseline, not from a stale one"
        );
    }
```

In `extras.rs`:

```rust
    /// fails if only limit-carrying sheep get watched. An app with no
    /// `max_memory` is the ordinary case, and `shep flock` reporting `-`
    /// for every one of them is the bug this split exists to fix.
    #[test]
    fn every_sheep_with_a_pid_is_watched_even_with_no_limit_and_no_probe() {
        let fixture = registry_fixture(); // this module's existing helper
        let entry = online_entry(7, 4242, AppConfig::minimal("web", "./srv"));
        fixture.registry.arm(&entry, fixture.prober(), &fixture.extras, &fixture.supervisor);

        assert_eq!(
            fixture.extras.stats.watched_for_test(),
            vec![(7, 4242)],
            "an app with neither max_memory nor a liveness_probe is the \
             ORDINARY case, and `shep flock` reporting `-` for every one of \
             them is what this split exists to fix"
        );

        fixture.registry.disarm(7, "web");
        assert!(fixture.extras.stats.watched_for_test().is_empty());
    }
```

- [ ] **Step 2: Run, confirm failure**, with **both** loop commands (the `extras::` one is the one that shows the last case).

- [ ] **Step 3: Implement.** `StatsState`'s two maps are `std::sync::Mutex`, not tokio's, for the reason `SysinfoSampler` gives for its own: the critical sections are map operations never held across an `.await`, and a poisoned lock recovers with `unwrap_or_else(PoisonError::into_inner)` rather than taking the daemon down.

**`record_baseline_now(&self, now: Instant)` is the shape the tests above use**: it samples and records in one call, so the enforcer's tick has one line to write and no test has to reach through to the sampler. `record_baseline(&self, index: &TreeIndex, now: Instant)` stays alongside it and is what the tick actually calls, because the tick already built the index and must not walk the process table twice — `record_baseline_now` is the one-line convenience over it. `watched_for_test` is `#[cfg(test)]`-gated and returns the watch map sorted by id, so `extras.rs`'s case can assert on it without a second fake.

- [ ] **Step 4: Run both loop commands, then the bench crate.**

```
cargo test -p shep-daemon --lib --all-features extras:: -- --skip watch::
cargo test -p shep-daemon --lib --all-features -- --skip watch:: --skip extras::
cargo bench --manifest-path benches/Cargo.toml -- --test
```

- [ ] **Step 5: Update `benches/benches/memory_sample.rs`'s measured-numbers comment** if the `.with_cpu()` refresh moved `sysinfo_sampler/sample_real_machine` materially. Re-measure rather than guess; that comment is a recorded observation and is the basis for `MEMORY_POLL_INTERVAL`.

- [ ] **Step 6: CHANGELOG** — shep-daemon: `ProcessRss::cpu_ms` added (a breaking change for an external `MemorySampler`), sampling split from enforcement.

- [ ] **Step 7: Task gate, then commit** — `feat(daemon): sample every sheep, enforce only where a limit exists`

---

## Task 14: CPU and memory on the wire

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/rpc.rs`, `crates/shep-daemon/src/boot.rs`
- Modify: `crates/shep-core/CHANGELOG.md`, `crates/shep-daemon/CHANGELOG.md`

**Interfaces — produced, depended on by Task 15:**

```rust
// shep-core: ProcessInfo loses `Eq`, keeps `PartialEq`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    // ... existing fields ...
    /// Tree CPU as a percentage of one core, over the window since the
    /// daemon's last periodic sample. `None` when the sheep is not running,
    /// when it has been up for less than one sampling window, or when the
    /// peer daemon predates this field — all three of which a reader
    /// renders as unknown, never as zero.
    pub cpu_percent: Option<f32>,
    /// Tree resident set size in bytes, current as of the reply. `None`
    /// under the same three conditions as [`Self::cpu_percent`], minus the
    /// window one — memory needs no baseline.
    pub memory_bytes: Option<u64>,
}

// shep-daemon: RpcContext gains the shared state
pub struct RpcContext {
    // ... existing fields ...
    pub(crate) stats: Arc<StatsState>,
}
```

Cargo shape: crosses crates → `--workspace`. `cargo test --workspace --all-features --lib`.

### Why `ProcessInfo` loses `Eq`

`f32` is not `Eq`. Nothing in the workspace requires `Eq` on `ProcessInfo` — verified by grep across `crates/` and `benches/` — and `PartialEq` stays, so every `assert_eq!` still compiles. It is a public API change and gets a CHANGELOG line (decision 25). **Do not** work around it with a fixed-point integer: `cpu_percent` is a percentage with a fraction, and a `u32` of hundredths would push the formatting decision onto every reader.

Both fields are `Option` for the reason `out_file`'s own comment argues at length: the handshake compares only `PROTOCOL_VERSION`, this addition deliberately does not bump it, so a daemon built before the field connects happily to a client built after it and sends replies with no such key. `None` means "this peer predates the field, or has nothing honest to say" — a reader renders it as unknown, never as `0`.

### Where the sample is taken

In the **RPC layer**, on `ListFlock` and `Describe`, after `list_checked` returns and before the reply is built. Not in the actor and not in `to_info`: `SysinfoSampler::sample` is a measured **5.77ms** blocking syscall walk (`benches/benches/memory_sample.rs`, 883 host processes), the actor must never block, and a tokio worker thread should not either — so it runs under `spawn_blocking` (decision 24):

```rust
/// Fills in each running sheep's live CPU and memory.
///
/// The sample is taken here rather than inside the supervisor for two
/// reasons that point the same way: the actor must never block, and the
/// reading is a syscall walk over the host's whole process table — measured
/// at 5.77 ms across 883 processes — so it runs on a blocking-pool thread
/// and not on a runtime worker.
///
/// Joined by pid, not by id: `StatsState` keys on the root pid it was armed
/// against, which is the same number `ProcessInfo::pid` carries, and a sheep
/// with no pid is not running and has nothing to report.
async fn with_live_stats(stats: &Arc<StatsState>, mut infos: Vec<ProcessInfo>) -> Vec<ProcessInfo> {
    let stats = Arc::clone(stats);
    let Ok(sample) = tokio::task::spawn_blocking(move || stats.sample_now()).await else {
        // The blocking pool is gone or the task panicked: report the flock
        // without stats rather than fail a listing over a decoration.
        return infos;
    };
    for info in &mut infos {
        if let Some(reading) = info.pid.and_then(|pid| sample.get(&pid)) {
            info.cpu_percent = reading.cpu_percent;
            info.memory_bytes = Some(reading.memory_bytes);
        }
    }
    infos
}
```

**Only `ListFlock` and `Describe` get it.** `Started`/`Stopped`/`Restarted`/`Reloading`/`Reopened`/`Flushed` all carry `ProcessInfo` too, and none of them is a place an operator reads resource usage — paying a 5.77ms walk on every `stop` would be a cost for nobody. Say so where the two call sites are, so the asymmetry reads as a decision.

- [ ] **Step 1: Write the failing tests.**

In `request.rs`, extend `sample_info()` with both fields populated (`Some(12.5)`, `Some(48 * 1024 * 1024)`) — the snapshot and the anti-drift tests both depend on every `Option` being `Some` — and add:

```rust
    /// fails if the two fields stop being optional. A daemon built before
    /// them sends a reply with no such keys, and both peers still announce
    /// protocol 1 — a required field would make a current client unable to
    /// list against that daemon at all.
    #[test]
    fn v1_process_info_without_stats_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log"}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.cpu_percent, None);
        assert_eq!(info.memory_bytes, None);
    }
```

In `rpc.rs`:

```rust
    /// fails if `ListFlock` stops taking a live sample — the fields would
    /// come back `None` for a running sheep, which a reader renders as `-`
    /// and an operator reads as "shep cannot see it".
    #[tokio::test]
    async fn list_flock_carries_a_live_memory_reading_for_a_running_sheep() {
        // The harness's sampler is scripted, so the number below is the
        // fixture's and not the machine's — this asserts the plumbing, not
        // sysinfo. `ScriptedRunner` hands out a known pid, and the scripted
        // reading is built around that same pid; see the harness helper.
        let h = harness_with_stats(vec![ProcScript::never_exits()]);
        reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("web", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        let listed = reply_of(dispatch(envelope(2, Request::ListFlock), &h.ctx).await);
        let Ok(Response::Flock(infos)) = listed.result else {
            panic!("expected Flock, got {:?}", listed.result)
        };
        assert_eq!(infos[0].memory_bytes, Some(4096));
        assert_eq!(
            infos[0].cpu_percent, None,
            "no periodic baseline has been recorded, and a number invented \
             from the read's own window is worse than an empty cell"
        );
    }

    /// fails if a stopped sheep is given someone else's numbers. A sheep
    /// with no pid cannot be joined against a pid-keyed reading, and a join
    /// that fell back to id would hand it whatever sheep happens to share
    /// that number — a reading for one sheep printed against another.
    #[tokio::test]
    async fn a_sheep_with_no_pid_reports_no_stats() {
        let h = harness_with_stats(vec![ProcScript::never_exits()]);
        reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("web", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Stop {
                        selector: SelectorSpec::Name("web".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        let listed = reply_of(dispatch(envelope(3, Request::ListFlock), &h.ctx).await);
        let Ok(Response::Flock(infos)) = listed.result else {
            panic!("expected Flock, got {:?}", listed.result)
        };
        assert_eq!(infos[0].pid, None);
        assert_eq!(infos[0].memory_bytes, None);
        assert_eq!(infos[0].cpu_percent, None);
    }

    /// fails if a lifecycle verb starts paying for a sample. A 5.77 ms
    /// syscall walk over the host's whole process table, on every `stop`,
    /// buys a reading nobody reads there.
    #[tokio::test]
    async fn a_lifecycle_reply_carries_no_stats() {
        let h = harness_with_stats(vec![ProcScript::never_exits()]);
        reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("web", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let stopped = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Stop {
                        selector: SelectorSpec::Name("web".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::Stopped(infos)) = stopped.result else {
            panic!("expected Stopped, got {:?}", stopped.result)
        };
        assert_eq!(
            infos[0].memory_bytes, None,
            "only `flock` and `describe` take a live sample"
        );
    }
```

`harness_with_stats` is the existing `crate::testing::harness` given a `ScriptedSampler` whose reading names the pid `ScriptedRunner` hands out. **Read `testing.rs` and `extras.rs`'s test module first** — `ScriptedSampler` exists and `Extras` is already constructible with fakes there. Extend the existing harness (an optional sampler, defaulted) rather than building a second one, and say in the report what you extended and what the scripted pid is.

- [ ] **Step 2: Run, confirm failure.** `cargo test --workspace --all-features --lib`

- [ ] **Step 3: Implement.** `boot()` builds the `StatsState` once, hands one clone to `Extras` and one to `RpcContext` — the same shared-`Arc` shape `Extras::enforcer` already uses and for the same reason: two owners, one mechanism.

- [ ] **Step 4: Regenerate the reply snapshot, read the delta, paste it into the report.** Expected: exactly two new keys on each `ProcessInfo` object in `reply_wire_v1.snap`, and nothing else. **A float in an insta JSON snapshot is only stable if the value is exactly representable** — `12.5` is, `12.3` is not. Use `12.5`.

- [ ] **Step 5: Run the full lib suite.**

- [ ] **Step 6: CHANGELOGs** — shep-core: `ProcessInfo::cpu_percent`/`memory_bytes` added, additive on the wire; `ProcessInfo` no longer derives `Eq`. shep-daemon: `flock` and `describe` answer with a live reading.

- [ ] **Step 7: Task gate, then commit** — `feat: report each sheep's cpu and memory`

---

## Task 15: the columns

**Files:**
- Modify: `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/output/table.rs`, `crates/shep-cli/tests/cli_e2e.rs`, `crates/shep-cli/tests/fixtures/flock.json`, `crates/shep-cli/tests/fixtures/describe.json`, `crates/shep-cli/tests/fixtures/start.json`, `crates/shep-cli/tests/fixtures/bleats_no_follow.json`
- Modify: `crates/shep-cli/CHANGELOG.md`, `docs/specs/deferred.md`

**Interfaces — produced:**

```rust
// output/table.rs
/// Formats a byte count for a table cell: the largest binary unit that
/// leaves at least one significant digit, one decimal place under 10.
///
/// Not `MemSize`'s `Display`, which renders the largest unit dividing the
/// value EXACTLY and so prints a live RSS of 50 462 720 bytes as
/// "50462720". A resident-set reading is never a round number of MiB.
#[must_use]
pub fn human_bytes(bytes: u64) -> String;
```

Cargo shape: `-p shep-cli`. `cargo test -p shep-cli --bins`, then `cargo test -p shep-cli --test cli_e2e`.

`FlockRows` gains two columns, `CPU` and `MEM`, between `RESTARTS` and `UPTIME` — where `pm2 ls` puts them, and where an operator scanning a table looks. `-` for `None`, the same rule `PID` and `FOLD` already follow and for the same stated reason: an empty cell in a padded table is indistinguishable from a rendering bug.

**`FlushedRows::JSON_ONLY` must grow both field names.** It lists the serialized fields that legitimately have no column, and `assert_no_drift` fails the moment a serialized field appears in neither. That failure is the anti-drift test doing its job; adding the names with a one-line reason (a flush neither reads nor changes a sheep's resource usage) is the fix, not an `#[allow]`.

- [ ] **Step 1: Write the failing tests.**

In `table.rs`:

```rust
    /// fails if `human_bytes` renders a live RSS as raw digits. `MemSize`'s
    /// own Display only names a unit that divides the value exactly, and a
    /// resident set is never an exact number of MiB — so a column built on
    /// it would show "50462720" where an operator expects "48.1M".
    #[test]
    fn bytes_render_with_a_unit_a_reader_can_scan() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(50_462_720), "48.1M");
        assert_eq!(human_bytes(3 << 30), "3.0G");
        assert_eq!(human_bytes(u64::MAX), "16.0E");
    }
```

In `rows.rs`, extend the existing `sample_info` helper with both fields `Some` (`flock_rows_do_not_drift` diffs JSON keys against `headers()`, and a `None` field vanishes from the JSON entirely, so it would not see either one), then:

```rust
    /// fails if a sheep with no reading renders an empty cell or a zero.
    /// A zero is a claim — "this sheep is using no CPU" — and the daemon
    /// says `None` precisely when it cannot make that claim.
    #[test]
    fn a_sheep_with_no_reading_renders_a_dash_not_a_zero() {
        let mut info = sample_info(1, "web", 60_000);
        info.cpu_percent = None;
        info.memory_bytes = None;
        let rows = FlockRows(vec![info]);
        let cells = &rows.rows()[0];
        let headers = FlockRows::headers();
        let cpu = cells[headers.iter().position(|h| *h == "CPU").unwrap()].clone();
        let mem = cells[headers.iter().position(|h| *h == "MEM").unwrap()].clone();
        assert_eq!(cpu, "-");
        assert_eq!(mem, "-");
    }

    /// fails if the two new fields are left out of `FlushedRows::JSON_ONLY`.
    /// That list is what keeps a serialized-but-uncolumned field honest, and
    /// this is the gate that says so.
    #[test]
    fn flushed_rows_do_not_drift() { /* existing case, now covering both fields */ }
```

In `cli_e2e.rs`, the assertion belongs in **`normalize_process_info`** (`cli_e2e.rs:864`), which already assert-then-nulls every volatile field (`pid`, `uptime_ms`, `out_file`, `err_file`) so the committed fixtures stay stable. Two fields join it:

- `memory_bytes` must be a **positive integer** for `flock` and `describe` — **the e2e tier is the only one where a real sysinfo reading exists**; every tier below it is scripted — and **`null` for `start`**, whose reply deliberately pays for no sample. That split means `normalize_process_info` needs the verb it is normalizing; `assert_envelope_matches_fixture` already has it (it takes `"start"`/`"flock"`/`"describe"` as an argument), so thread it through.
- `cpu_percent` must be **`null`**, and asserting that is the point rather than a concession: a sheep started seconds ago has had no periodic tick, so there is no honest number, and a build that invented one from the read's own window would fail here.

Both are then set to `Null` before the fixture comparison, exactly as the four existing fields are. The committed fixtures gain `"cpu_percent": null` and `"memory_bytes": null` on every `ProcessInfo` object; the comparison is **structural equality over the whole normalized `Value`** (`load_fixture`'s own doc), so a fixture missing either key fails rather than silently stopping short.

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement**, including the committed JSON fixtures under `crates/shep-cli/tests/fixtures/`. `flock.json`, `describe.json` and `start.json` each carry `ProcessInfo` objects and gain both keys as `null`; `ping.json` and `bleats_no_follow.json` carry none and must not be touched (`bleats_no_follow.json` is compared **byte-for-byte**, not structurally — an edit there breaks a different case for a different reason).

- [ ] **Step 4: Run tests, confirm pass** — `--bins`, then `--test cli_e2e`.

- [ ] **Step 5: `docs/specs/deferred.md`** — nothing in the stats work was on that list; confirm rather than assume, and say so in the report.

- [ ] **Step 6: CHANGELOG** — shep-cli: `flock` and `describe` show CPU and memory.

- [ ] **Step 7: Task gate, then commit** — `feat(cli): show each sheep's cpu and memory`

---

## Task 16: `migration.md`, the runbook, and the report

**Files:**
- Create: `docs/migration.md`
- Modify: `docs/specs/shep-v1.md` (§13.4), `docs/specs/deferred.md`, `docs/systematic-refactor/refactor-workspace/map.md`, `README.md` if it lists verbs
- Modify: every crate CHANGELOG that this phase left unreconciled

No cargo shape: this task writes documentation. The gate still runs, because `RUSTDOCFLAGS="-D warnings" cargo doc` covers intra-doc links and this task touches doc comments only if it finds a stale one.

- [ ] **Step 1: `docs/migration.md`.** The pm2 → shep guide. It is public-facing prose, so **invoke the `humanizer` skill on it before it is final** and match the register of `docs/shepherd-channel.md`, which is this repo's existing app-author-facing document. Sections:

  1. **What comes across, and what does not** — the field-mapping table, verbatim from the design spec, plus the two things that do not: cluster-mode fd passing (deferred to v1.2; the app sets `SO_REUSEPORT` itself, Node ≥ 22.12's `reusePort: true`), and an inherited shell environment.
  2. **The env rule, stated as a rule** — declared env comes across, a login shell's does not, and anything ambiguous is named at import time for the operator to place. Say why: an app started by hand over SSH inherits `BUN_INSTALL` and `JAVA_HOME`, and an init-started daemon has neither.
  3. **`PATH` is the unit's** — the mechanism that makes an interpreter under `~/.bun` or `~/.cargo` findable after a reboot.
  4. **The three commands**, in order: `shep import`, `shep save`, `shep startup`.
  5. **The §13.4 runbook** — Step 2 below.
  6. **Rolling back** — `shep unstartup`, and pm2's own state is untouched by all of this because `import` only ever reads `dump.pm2`.

- [ ] **Step 2: The §13.4 manual runbook**, inside `migration.md`. It needs a reboot, so it cannot run in CI without a VM; the runbook is what makes the criterion falsifiable by hand rather than assumed. Every step names what to check and what a failure looks like:

```
1.  shep import --dry-run           # read the Flockfile before it is written
2.  shep import                     # writes ./Flockfile.toml; starts nothing
3.  pm2 delete all && pm2 kill      # the one destructive step, and it is pm2's
4.  shep start ./Flockfile.toml     # the flock comes up under shep
5.  shep flock                      # every app online, CPU and MEM populated
6.  shep save                       # names the roll it wrote and the app count
7.  sudo shep startup --user <you>  # writes and enables the unit
8.  systemctl status shep-<you>     # active (running), and green
9.  reboot
10. systemctl status shep-<you>     # active (running) WITHOUT anyone logging in
11. shep flock                      # the same apps, new pids, uptime near zero
```

State the two failure signatures explicitly, because they are the ones this design was built to make visible:

- **`activating (start)` then a timeout at step 8 or 10** — the daemon booted but never sent `READY=1`, or the muster restore hung. `journalctl -u shep-<you>` carries the daemon's own records.
- **`active (running)` at step 10 but an empty `shep flock` at step 11** — the unit came up against the wrong `$SHEP_HOME`. `systemctl cat shep-<you>` shows which one; this is the sudo trap `shep startup` refuses when it can see it.

- [ ] **Step 3: Reword spec §13.4.** It currently reads "systemd unit runs `shep muster`". Under `Type=notify` systemd supervises the process it starts, so `ExecStart` is the daemon and the restore happens because the daemon restores the roll at boot. Record it the way §9's `trigger` amendment is recorded — **what was decided, and why the reasoning matters to a later reader**, not just the corrected sentence. pm2 could write `ExecStart=pm2 resurrect` only because it used `Type=forking` and tracked the forked child through a PID file.

- [ ] **Step 4: `docs/specs/deferred.md`** — reconcile the whole file against the tree, not only this phase's four entries. Confirm the `save`/`muster`, `import` + migration guide, and `startup`/`unstartup` entries are gone, and that the "Not deferred" section gained this phase's set. openrc and freebsd/openbsd rc.d remain deferred and should be **named** as such, since spec §11 lists four init systems and this phase ships two.

- [ ] **Step 5: `map.md`** — verify every claim against the code before writing it, and **cite by symbol, not line number**. That file has twice been synced to what a plan expected rather than what shipped.

- [ ] **Step 6: Reconcile every CHANGELOG** (IR-45). Each entry should describe what an operator or an API consumer sees, not which task produced it (Rule 10). The four user-visible headlines: `save`/`muster`, `import`, `startup`/`unstartup`, and CPU/memory in `flock`.

- [ ] **Step 7: Report to Rin** — every judgement call made on her behalf, anything left unfixed, and specifically:
  - whether `RowMissingScript` fires on a row with no `pm_exec_path`, and whether the fixture stayed synthesised rather than derived from a real dump;
  - decision 10 (`NODE_APP_INSTANCE` → `increment_var`), which is beyond the design spec's field table and rests on spec §14.8;
  - whether `systemd-analyze verify` ran or skipped, and on what;
  - the measured cost of the phase's slowest new test.

- [ ] **Step 8: Full phase gate** — the four task gates, **plus** the serial run and both bench-crate gates:

```
cargo test --workspace --all-features -- --test-threads=1
cargo bench --manifest-path benches/Cargo.toml -- --test
cargo clippy --manifest-path benches/Cargo.toml --all-targets -- -D warnings
```

The serial run is not ceremony: it was red on `main` before Phase 5 and caught a real regression in Phase 6.

- [ ] **Step 9: Commit** — `docs: record what a pm2 cutover actually takes`

---

## Exit criteria

1. All sixteen tasks complete and individually reviewed.
2. Every gate green **from its own exit code**, including both bench-crate gates and the serial run. `Running`/`Doc-tests` lines counted against `test result:` lines, baseline **15 against 15**, starting from **793 passed / 1 ignored**.
3. `shep save` against a daemon whose engine has stopped exits non-zero and says why. A save that silently does nothing is the failure this verb exists to rule out.
4. `shep muster` that restores nothing says so, rather than printing an empty table and exiting 0.
5. The muster roll has exactly one restore implementation: `grep -rn "restorable(" crates/shep-daemon/src/` finds one caller.
6. **The import fixture is synthesised.** It contains no absolute path from any real machine, no real hostname or username, and no environment variable value taken from a live session. A reviewer reading `testdata/dump.pm2.json` can see it was written rather than captured.
7. Every app the fixture produces passes `shep_core::config::normalize`, and the rendered Flockfile round-trips through the real `Flockfile::parse` back to the same `AppConfig` values.
8. `shep import` names every cluster-mode app and the `SO_REUSEPORT` requirement it carries, and every ambiguous env key it dropped. `shep import --dry-run > Flockfile.toml` produces a file `shep start` accepts.
9. `shep import` starts nothing: the e2e case asserts no socket exists afterwards.
10. The generated systemd unit is accepted by `systemd-analyze verify` where the tool exists, and its `ExecStart` is the daemon rather than `shep muster`.
11. `shep startup` unprivileged prints a paste-able command including `--home` and exits non-zero; `shep startup` with a `$SHEP_HOME` that does not exist refuses rather than installing a unit that would restore nothing.
12. `READY=1` is sent after the muster restore completes, pinned by a test that fails if the two are reordered.
13. `shep flock` shows a live memory reading for a running sheep and `-` for a sheep with no CPU baseline yet, and two `shep flock` calls a moment apart do not report a CPU number computed from the gap between them.
14. `PROTOCOL_VERSION` is still **1**, and **each** regenerated snapshot's delta is pasted verbatim in its task's report and is only that task's addition.
15. **Both halves of the marker grep**: files this phase creates, *and* lines it adds to files it only modifies, are free of task-relative phrasing. Phase 4 skipped the second half and a marker shipped.
16. **Every test added carries a "fails if" comment naming the mutation it catches, and the mutation was actually performed and watched to fail before the comment was written.** This project has shipped five tests naming a bug they could not catch; a reviewer picking three at random must be able to break the implementation in the named way and watch the named test redden.
17. No test added in this phase can hang: every await of a daemon answer is bounded, and every negative assertion polls a bounded window rather than calling `try_recv` once.
18. Neither suite run leaves a process reparented to init, calibrated by forcing one deliberate panic — a green suite never exercises the teardown its guards govern.
19. `docs/specs/deferred.md` matches the tree: the four entries this phase closes are gone, openrc and rc.d are still named, and nothing in it claims something the tree no longer matches.
