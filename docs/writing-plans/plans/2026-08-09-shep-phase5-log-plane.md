# shep Phase 5 — Log plane and diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **REQUIRED SUB-SKILL:** Use the `shep-idiomatic-rust` skill before writing or reviewing any Rust here.

**Goal:** Make the daemon's diagnostics visible, give operators `flush` and `reopen` so external log rotation works, and unblock `cargo publish`.

**Architecture:** Three independent strands that share one theme — nothing about shep's logging currently reaches a human. A `tracing-subscriber` turns 51 dead log sites into output. A control channel on `ProcIo` lets the actor reach the per-sheep log pumps, which nothing can do today, so `reopen` can promise a synchronous contract. And three manifest lines plus a re-export make the library crates publishable and usable.

**Tech Stack:** `tracing-subscriber` 0.3.23 (`default-features = false`, `std`/`fmt`/`ansi`/`env-filter`/`json`), existing `tracing`, `tokio::fs`, the existing UDS JSON protocol.

## Scope note — what moved out

Research reshaped this phase. **Reload is now Phase 6** and **custom actions is Phase 7** (Rin, 2026-08-09). Reload needs bus-tracked progress, two fixture servers, a load harness and per-platform socket tests; it is the riskiest work this project has attempted and gets undivided review attention. Custom actions carries ten undetermined design questions.

Two things from those phases land here anyway, because they are corrections rather than features:

- `AppConfig::reuse_port`'s doc makes a **false claim** (Task 9). A wrong doc should not wait a phase.
- The `channel` config field (Task 5) was decided alongside custom actions, and `reopen` benefits from the same deliberate-fd-3 reasoning.

## Global Constraints

1. **MSRV 1.88, edition 2024.** `tracing-subscriber` 0.3.23 declares `rust-version = 1.65.0` — verified compatible with `cargo +1.88 check`, no pin needed.
2. **New dependency:** `default-features = false`, feature list stated with a `# Option:` comment (IR-2, IR-3). Dropping the default `tracing-log` is free — nothing in shep uses the `log` crate.
3. **The `-Z minimal-versions` leg has not been rehearsed** for `tracing-subscriber` or its five new transitive deps. This workspace already carries six floor pins because deps under-declare their own minimums, including `tracing-core = "0.1.28"` pinned precisely because `tracing` under-declares. **Expect at least one more pin. Task 3 owns rehearsing it.**
4. **Wire-facing changes need stability fixtures and a CHANGELOG entry** (IR-35, IR-45). `Request`/`Response` are `#[non_exhaustive]`, so new variants are additive and `PROTOCOL_VERSION` stays 1.
5. **Rule 9:** one owner per constant. **Rule 10:** no task-relative phrasing in shipped comments — name the thing, never "Task 6" or "this phase".
6. **Rule 11:** advance a paused clock in steps no larger than the shortest period of the loop under test; negative assertions poll a bounded window (`timeout` + `recv`), never a bare `try_recv`.
7. **Fixture sizing.** `ScriptedRunner` answers `SpawnFailed("script exhausted")` once scripts run out, the supervisor emits `Errored`, and that state is frequently indistinguishable from the failure under test. Size against what a **broken** implementation demands, count the scripts, and name the spawn a broken implementation performs that a correct one does not. This phase's predecessor found twenty-two tests that passed for the wrong reason, every one from this single cause.
8. **Workspace lints** deny `missing_docs`, `missing_debug_implementations`, `undocumented_unsafe_blocks`, `clippy::missing_errors_doc`. `#![forbid(unsafe_code)]` outside `shep-daemon/src/sys.rs`.
9. **Gates**, each from its own command with `$?` captured directly — **never piped**, since a pipeline's `$?` is the last command's and that has already produced a false green in this project:
   ```
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-features
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
   cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
   cargo +1.88 check --workspace --all-targets --all-features
   cargo test --workspace --all-features -- --test-threads=1
   ```
   Plus the bench crate's own two, run from inside `benches/`. `cargo test --workspace` halts at the first failing crate — count `Running`/`Doc-tests` lines against `test result:` lines (baseline 11 + 3 = 14) and confirm every crate ran.
10. **Baseline: 613 passing + 1 pre-existing ignored** (`a_dropped_child_runs_as_the_requested_user`, needs root). The suite takes ~50s longer than it used to; two e2e cases wait on a real cron minute boundary and a real 15s memory poll. That is expected, not a hang.
11. **zsh**, not bash. `${PIPESTATUS[0]}` yields an empty string. Quote glob-bearing arguments. **Commit messages containing backticks must use `git commit -F -` with a quoted heredoc** — backticks inside `-m "..."` are command substitution and have already mangled one commit here.
12. **Reap what you spawn.** A killed `cargo test` does not reap daemons its tests started; two were found reparented to init earlier in this project. Use a short `$SHEP_HOME` — macOS caps the socket path near 97 chars and a long one dies at boot with `SUN_LEN`, which has cost three agents a debugging round.

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` | version literals on three workspace path deps; `tracing-subscriber` entry |
| `crates/shep-client/src/lib.rs` | `Stream` re-export |
| `crates/shep-client/src/events.rs` | inherent `EventStream::next` |
| `crates/shep-cli/src/commands/daemon.rs` | subscriber installation (`run_daemon`, **not** `boot`) |
| `crates/shep-core/src/config/daemon.rs` | `log_level` beside the existing `log_json` |
| `crates/shep-core/src/config/app.rs` | `channel` field; `reuse_port` doc correction |
| `crates/shep-core/src/protocol/request.rs` | `Flush`/`Reopen` request + response variants |
| `crates/shep-daemon/src/runner.rs` | `ProcIo` gains the log control channel; `LogCtl` |
| `crates/shep-daemon/src/tokio_runner.rs` | pump `select!`s reader vs control; ordering-claim doc fix |
| `crates/shep-daemon/src/fake.rs` | `ScriptedRunner` grows the control channel |
| `crates/shep-daemon/src/supervisor.rs` | `Command::{Flush,Reopen}`, handle methods, per-sheep effect |
| `crates/shep-daemon/src/rpc.rs` | dispatch arms |
| `crates/shep-daemon/src/boot.rs` | SIGUSR2 → reopen-all; `reopens` cite fix |
| `crates/shep-cli/src/cli.rs`, `main.rs`, `commands/logs.rs` | the two verbs |
| `docs/specs/shep-v1.md` | macOS SO_REUSEPORT caveat |

---

## Task 1: Make the workspace publishable

**Files:** Modify `Cargo.toml`.

**Interfaces:** Produces nothing other tasks consume.

Three of four crates fail `cargo publish` today. Verified per-crate: `shep-core` passes (it is the only crate with no sibling dependency); `shep-daemon`, `shep-client` and `shep-cli` all fail with `dependency ... does not specify a version`. Cargo strips `path` at publish time and needs a version to put in its place.

- [ ] **Step 1: Reproduce the failure**

```bash
cargo publish -p shep-client --dry-run 2>&1 | tail -20
```
Expected: `all dependencies must have a version requirement specified when publishing. dependency 'shep-core' does not specify a version`.

- [ ] **Step 2: Add version literals**

In `[workspace.dependencies]`:
```toml
shep-core = { path = "crates/shep-core", version = "0.1.0" }
shep-daemon = { path = "crates/shep-daemon", version = "0.1.0" }
shep-client = { path = "crates/shep-client", version = "0.1.0" }
```

There is **no `version.workspace = true` form inside a dependency entry** — this is a literal, so a release bump touches two places. Add a comment saying so, naming `cargo-release` as the mechanical answer if that ever becomes annoying.

- [ ] **Step 3: Verify all four**

```bash
for c in shep-core shep-daemon shep-client shep-cli; do cargo publish -p "$c" --dry-run >/dev/null 2>&1; echo "$c=$?"; done
```
Expected: the manifest-verification error is gone from all four. A remaining `no matching package named 'shep-core' found` is the ordinary first-publish ordering error and resolves once `shep-core` is actually on crates.io. Publish order is `shep-core` → `shep-daemon` + `shep-client` → `shep-cli`.

- [ ] **Step 4: Record the two consequences**

`shep-core`'s `[target.'cfg(any())'.dependencies]` floor-pin block publishes as **real** manifest entries, so crates.io and docs.rs will list `annotate-snippets`, `pest`, `quote` and `syn` against it. That is cosmetic but will look wrong to a reader — note it in the CHANGELOG entry rather than leaving it to be discovered. Also note that the package is `shep-cli` while the binary is `shep`, so the install command is `cargo install shep-cli`.

- [ ] **Step 5: Commit** — `fix(workspace): give path dependencies the versions cargo publish requires`

---

## Task 2: Let consumers call `.next()` on an `EventStream`

**Files:** Modify `crates/shep-client/src/events.rs`, `crates/shep-client/src/lib.rs`, `crates/shep-cli/Cargo.toml`.

**Interfaces:** Produces `EventStream::next` (inherent) and a re-exported `Stream` trait.

`EventStream` implements `futures_util::Stream`, and there is **no stable `core::stream::Stream`** — so the trait is not nameable without a third-party crate. The proof this is real comes from inside the workspace: `shep-cli`'s own `Cargo.toml` carries `futures-util` with a comment stating exactly this blocker. shep-client's first consumer hit it.

Land **both** shapes:

- [ ] **Step 1: Inherent `next`**

An inherent method wins name resolution over a trait method, so the common case needs no import and no extra dependency at all. Precedent: `tokio::sync::broadcast::Receiver::recv`, `reqwest::Response::chunk`.

```rust
impl EventStream {
    /// Returns the next event, or `None` once the subscription ends.
    ///
    /// Inherent so a caller needs no `StreamExt` import. For combinators,
    /// the [`Stream`] implementation is also re-exported from the crate root.
    pub async fn next(&mut self) -> Option<Result<BusEvent, Lagged>> { /* … */ }
}
```

- [ ] **Step 2: Re-export the trait**

`pub use futures_util::Stream;` from `shep-client`'s root, with `#[doc(inline)]` per IR-32. The trait alone, so the type is nameable in bounds; `StreamExt` follows only if a consumer needs combinators, and the inherent `next` covers the common case.

This ties shep-client's public API to futures-util 0.3's semver — already true de facto, since `impl Stream` is *in* the public API. Precedent: `tokio-stream` re-exports `Stream` from `futures-core`; `axum` re-exports much of `http`.

- [ ] **Step 3: Test from a consumer's position**

The existing `crates/shep-client/tests/event_stream.rs` does `use futures_util::StreamExt;`. Add a case that calls `.next()` with **no** `futures_util` import at all — that is the assertion that the papercut is closed. Fails if the inherent method is removed.

- [ ] **Step 4: Shrink `shep-cli`'s import**

`shep-cli/src/commands/bleats.rs`'s only `StreamExt` use is `stream.next()`. With the inherent method it can drop to `FutureExt` alone (still needed for the Ctrl-C wiring), and that `Cargo.toml` comment — which currently documents the blocker as permanent — must be rewritten.

- [ ] **Step 5: Commit** — `feat(client): make EventStream usable without a futures-util dependency`

---

## Task 3: Wire a `tracing-subscriber`

**Files:** Modify `Cargo.toml`, `crates/shep-daemon/Cargo.toml`, `crates/shep-cli/Cargo.toml`, `crates/shep-cli/src/commands/daemon.rs`, `crates/shep-core/src/config/daemon.rs`.

**Interfaces:** Produces a `[daemon] log_level` config field and `SHEP_LOG_LEVEL`; consumes the **already-existing** `log_json`.

51 log sites in `shep-daemon` — 7 `error!`, 33 `warn!`, 2 `info!`, 9 `debug!` — reach nobody today. No other crate depends on `tracing`.

**A knob already exists and is dead.** `DaemonSection::log_json` is parsed from `[daemon] log_json`, overridden by `SHEP_LOG_JSON`, covered by five tests, and named in `run_daemon`'s doc — and nothing reads it. This task makes it real.

- [ ] **Step 1: Add the dependency**

```toml
# Option: the daemon's own diagnostics. `env-filter` costs exactly one crate
# (matchers) and `json` exactly one (tracing-serde); the rest of the tree is
# already present via regex/globset/serde. The default `tracing-log` bridge is
# dropped because nothing here uses the `log` crate.
tracing-subscriber = { version = "0.3.23", default-features = false, features = ["std", "fmt", "ansi", "env-filter", "json"] }
```

Measured: base tree 103 crates → 109 with this set.

- [ ] **Step 2: Add `log_level` beside `log_json`**

Follow `log_json`'s existing shape exactly — file `<` env `<` flag layering, `SHEP_LOG_LEVEL` per the workspace naming rule (`SHEP_` plus the screaming-snake TOML key, "with no exception to work around"). **Do not read `RUST_LOG`** — it would be the first knob to break that rule. Default `warn`, which surfaces 40 of the 51 sites; `debug` would be genuinely noisy once the dogs work lands, since `supervisor.rs`'s debug arms fire per dropped-restart and per child metric.

- [ ] **Step 3: Install it in `run_daemon`, never in `boot`**

`crates/shep-cli/src/commands/daemon.rs:run_daemon` is safe: its only caller is `main.rs`, and the e2e tier drives the real binary as a subprocess, so each invocation gets a fresh process.

`boot()` is **not** safe. Six unit tests in `boot.rs` call it, plus `daemon_e2e.rs`'s `Fixture::boot` used by roughly nine more, and `tracing_subscriber::fmt::init()` is `try_init().expect(…)` — it **panics on the second install in a process**, failing every test after the first. `boot.rs` already carries a `SIGNAL_TEST_LOCK` for exactly this class of process-wide-state bug; read its doc before choosing anything else.

Sink is **stderr**. The launcher already redirects it into `shepd.err.log`, and a hand-run daemon then correctly logs to its parent's terminal. Having the daemon open its own file by name would duplicate the launcher's job and diverge the moment the daemon is not launched by the launcher.

- [ ] **Step 4: Make the warn arms assertable**

This is the part worth more than the logging itself. `fmt::layer().with_test_writer()` plus `tracing::subscriber::with_default` gives a scoped, per-test subscriber — turning "warn-and-continue is silent" from a logging gap into a **testable contract**. Add the harness and use it to pin the highest-value arm: `extras.rs:arm_watch`'s failure paths, where an app comes up `online` with no watch and, today, no signal at all.

Fails if the arm stops logging, or logs without the app name.

- [ ] **Step 5: Rehearse `-Z minimal-versions`**

Global Constraint 3. Expect at least one new floor pin; add it with the same comment style as the existing six, naming the API that forced it.

- [ ] **Step 6: Correct the docs this falsifies**

Four in-code sites plus `map.md` currently assert "no `tracing-subscriber` is wired": `extras.rs` (`arm_instance`, `arm_cron`, `arm_watch`, `spawn_extras_reporter`), `daemon_e2e.rs`'s `a_wait_ready_sheep_goes_online_on_its_own_channel_message`, and map.md's `probes/ready.rs` entry. They become factually wrong the moment this lands and must change in the same commit.

- [ ] **Step 7: Commit** — `feat(daemon): wire a tracing subscriber so the daemon's diagnostics reach someone`

---

## Task 4: An explicit `channel` config field

**Files:** Modify `crates/shep-core/src/config/app.rs`, `crates/shep-daemon/src/assemble.rs`.

**Interfaces:** Produces `AppConfig::channel`; consumed by `assemble`.

Today `assemble` sets `channel = config.wait_ready || config.shutdown_with_message`, so fd 3 opens as a **side effect of an unrelated readiness flag**. Rin settled that an explicit field is the answer (2026-08-09), decided alongside custom actions but landing here.

- [ ] **Step 1: Add the field, defaulting false**, documented as "open the shepherd channel on fd 3 for this app". Its doc must say what it costs — a socketpair plus two tasks per sheep, against spec §14.11's single-digit-MB idle-RSS goal — so the default is understood as deliberate.
- [ ] **Step 2: Widen the gate** to `config.channel || config.wait_ready || config.shutdown_with_message`. The two implicit openers stay: an app that sets `wait_ready` still gets its channel without also setting `channel`.
- [ ] **Step 3: Test** that each of the three flags independently opens it, and that none of them opens it when all are false. Fails if the gate drops any term.
- [ ] **Step 4: Commit** — `feat(core): let an app open the shepherd channel without a readiness flag`

---

## Task 5: The log-writer seam

**Files:** Modify `crates/shep-daemon/src/runner.rs`, `crates/shep-daemon/src/tokio_runner.rs`, `crates/shep-daemon/src/fake.rs`.

**Interfaces:** Produces `LogCtl` and `ProcIo::log_ctl`; consumed by Tasks 6 and 7.

**This is the trunk. Nothing can reach a log pump's file handle today** — it is a local `let mut file` inside the pump's async block, the pump's `JoinHandle` is discarded, and `ProcIo` carries only `logs`, `from_child` and `to_child`. That absence is the seam the Phase 2b plan deferred.

Rin chose the **push channel** over a generation counter, for the synchronous contract: after the reply returns, every live pump provably holds the new inode — which is what a logrotate `postrotate` stanza actually needs. A counter could only ever promise "before the next write", and **a quiet sheep would never reopen at all**.

- [ ] **Step 1: Define the control message**

```rust
/// What the supervisor can ask a log pump to do mid-flight.
#[derive(Debug)]
pub enum LogCtl {
    /// Drop the current handle and `open_append` the path again, then
    /// acknowledge. Sent when an external rotator has renamed the file.
    Reopen { done: oneshot::Sender<()> },
}
```

The `oneshot` is what makes the contract synchronous — it is the whole reason this shape was chosen over the counter, so it is not optional.

- [ ] **Step 2: Add it to `ProcIo`** as `pub log_ctl: mpsc::Sender<LogCtl>`, with a doc explaining that the pumps are the only readers and that dropping it ends them.

- [ ] **Step 3: Make the pump `select!`**

`tokio_runner.rs:spawn_log_pump` currently loops on `lines()`. It becomes a `select!` over the reader and the control receiver. On `Reopen`: flush the current handle, drop it, call `open_append` again, send on `done`.

**Preserve `O_APPEND`.** `open_append` sets `.append(true)`, and that is what makes `copytruncate` rotation work today — verified: after an external truncate the next write lands at offset 0 with no sparse hole. An implementation that swapped it for a cached offset plus positional writes would silently produce enormous sparse files after every rotation.

Two pumps per sheep (out and err) each need their own control receiver, or one receiver shared with a stream discriminant. Say which you chose and why.

- [ ] **Step 4: Grow `ScriptedRunner`**

`fake.rs`'s runner must carry the channel. Note that it **ignores `spec.out_file`/`err_file` entirely and writes no files**, so reopen's pump behaviour is not exercisable through the fake — that belongs in `tests/real_runner.rs`, which already asserts real file content. The fake needs the channel to compile and to let the actor-tier tests run, not to prove the reopen.

- [ ] **Step 5: Fix the over-strong ordering claim**

`spawn_log_pump`'s doc claims a receiver observing a line on the channel "can rely on that line's file write having already landed", and `real_runner.rs:exit_code_and_logs_are_captured` says the same. **Tokio does not guarantee that** — `tokio::fs::File` copies into an internal buffer and dispatches the real `write(2)` to the blocking pool, so `write_all().await` returning means *queued*. Green in practice because at most one line is in flight and the next `poll_write` awaits the previous op, but not by contract. Correct the prose in both places; do not add a per-line `flush()`, which is a real per-line cost.

- [ ] **Step 6: Commit** — `feat(daemon): give the supervisor a way to reach its log pumps`

---

## Task 6: `shep reopen`

**Files:** Modify `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/{supervisor.rs,rpc.rs}`, `crates/shep-cli/src/{cli.rs,main.rs}`, create `crates/shep-cli/src/commands/logs.rs`.

**Interfaces:** Consumes `LogCtl` from Task 5.

`create`-mode rotation (rename then signal) is **broken today, and silently**: after a `mv`, the pump keeps filling the renamed inode, the live path is never recreated, and `shep bleats --no-follow` then prints nothing and **exits 0 with no diagnostic**. A restart is currently the only working reopen. This task is that fix.

Reopen is cheap for a reason worth stating in the module doc so nobody reaches for a child-side mechanism: **the child never sees the log file.** It gets `Stdio::piped()`, so swapping the daemon's `File` is invisible across the process boundary — no signal to the child, no fd surgery, no restart, no gap in the pipe.

- [ ] **Step 1: Protocol variants.** `Request::Reopen { selector }` and a response. Additive under `#[non_exhaustive]`, so `PROTOCOL_VERSION` stays 1. Add the wire fixture row to the `request_wire_v1` insta snapshot (IR-35).
- [ ] **Step 2: Supervisor command.** `Command::Reopen`, a `SupervisorHandle` method, and a handler that resolves the selector and sends `LogCtl::Reopen` to each matched running sheep's pumps, awaiting the `oneshot`s.
- [ ] **Step 3: CLI verb.** `reopen` **defaults to `all`**, matching `bleats` (`default_value = "all"`) — reopen destroys nothing, and this makes SIGUSR2 exactly `reopen all`.
- [ ] **Step 4: Stopped sheep are a no-op success.** There is no live pump to reopen. Include them in the response rather than erroring.
- [ ] **Step 5: Real-filesystem test** in `tests/real_runner.rs`: rename the log file, `reopen`, assert the original path exists again and receives subsequent lines while the renamed file stops growing. Fails if the pump keeps the old inode.
- [ ] **Step 6: Commit** — `feat: reopen log files so external rotation works`

---

## Task 7: `shep flush`

**Files:** Same set as Task 6, plus `crates/shep-daemon/src/entry.rs` (read-only use).

`flush` **truncates, after flushing pending writes** (Rin, 2026-08-09). Flushing first matters because `tokio::fs::File` genuinely holds unwritten bytes: without it, a write already dispatched to the blocking pool can land at offset 0 immediately after the truncate.

- [ ] **Step 1: Pin it to the recorded path, never the inode.** Truncate `ProcessEntry::out_file`/`err_file` — the paths the actor already holds for every registered sheep, running or stopped. **If flush chased the pump's current inode, running it after a rotator's rename would truncate the archive instead of the live file.**
- [ ] **Step 2: Required selector.** `flush` destroys data, so it follows `stop`/`restart`/`delete` (`SelectorArgs`, no `default_value`), not `bleats`. State that reasoning in the verb's doc.
- [ ] **Step 3: Flush before truncating** by reusing Task 5's control channel, then truncate by path.
- [ ] **Step 4: Stopped sheep still truncate** — the operation is path-based, and `bleats --no-follow` exists precisely to read a stopped sheep's logs.
- [ ] **Step 5: The daemon's own `shepd.*.log` is out of scope.** The CLI's launcher opens those with `File::create` (which truncates on every launch — that is the entire rotation story today), and the daemon only inherits fds 1 and 2. Say so in the verb's doc rather than leaving the omission to be discovered.
- [ ] **Step 6: Test** that N instances sharing one path under `merge_logs` truncate correctly and that the response shape is decided and asserted. Fails if flush resolves to an inode.
- [ ] **Step 7: Commit** — `feat: truncate log files with shep flush`

---

## Task 8: SIGUSR2 means `reopen all`

**Files:** Modify `crates/shep-daemon/src/boot.rs`.

Spec §9 says "also SIGUSR2 to the daemon". A signal carries no selector, so SIGUSR2 is exactly `reopen all`.

Installing the handler is load-bearing on its own and already is: **SIGUSR2's default disposition is to terminate**, so without it an operator's `kill -USR2` — or a logrotate `postrotate` stanza — kills the daemon instead of rotating.

- [ ] **Step 1: Mind the boot ordering.** Signals install at **step 1**, deliberately before the socket exists; the supervisor is built at **step 4**. A SIGUSR2 listener therefore cannot be handed a `SupervisorHandle` at install time. Whatever carries the request must be created at step 1 and connected at step 4 or later.
- [ ] **Step 2: Decide `SignalTasks::reopens`' fate.** With the push channel chosen it is no longer the mechanism. Either keep it as an observability counter with its `#[allow(dead_code)]` finally justified by a reader, or delete it. Say which and why.
- [ ] **Step 3: Fix the wrong section cite.** `SignalTasks::reopens`' comment says "(spec §5)" — §5 is *Configuration*. Flush and reopen are §2 and §9. `install_signals`' own doc already gets it right.
- [ ] **Step 4: Drop the now-false log line.** It currently says per-sheep reopening "is not built yet".
- [ ] **Step 5: Test** that SIGUSR2 reopens every running sheep. `boot.rs`'s `SIGNAL_TEST_LOCK` exists for this class of test — read its doc first.
- [ ] **Step 6: Commit** — `feat(daemon): make SIGUSR2 reopen every sheep's logs`

---

## Task 9: Correct two false claims about `reuse_port`

**Files:** Modify `crates/shep-core/src/config/app.rs`, `docs/specs/shep-v1.md`, `docs/systematic-refactor/refactor-workspace/decision-briefs.md`.

Doc-only, and it corrects claims that are **measurably false**. Reload itself is Phase 6; these corrections should not wait behind it.

- [ ] **Step 1: Rewrite `AppConfig::reuse_port`'s doc.** It currently reads "**Bind** listen sockets with SO_REUSEPORT" — first person, as though shep binds. **shep binds nothing**; the child binds after `exec`, and a socket option must be set before `bind()` by the process that binds. `reuse_port = true` means the operator **asserts the app sets the option itself** (Node ≥22's `reusePort`, Go's `Control` hook, nginx's `reuseport`). shep's contribution is permission to overlap, not the mechanism.
- [ ] **Step 2: Document the failure mode.** An operator who sets `reuse_port = true` on an app that does not set it gets `EADDRINUSE` at the replacement spawn, every reload, and **shep cannot detect the misconfiguration in advance**. Measured: a mixed pair is refused on both platforms, and `SO_REUSEADDR` — which far more apps set by default — is **not** sufficient.
- [ ] **Step 3: Add the macOS caveat to the spec** (§4 and §11). **macOS SO_REUSEPORT does not load-balance; it is last-binder-wins.** Measured cross-process over 40 connections: macOS sent 40/40 to the newest binder, Linux split 20/20. `decision-briefs.md` records "N fork instances + SO_REUSEPORT load balancing" as **decided**, and macOS is a tier-1 platform — so the caveat belongs in the spec, not only in map.md's one line. Note the sign: this makes *reload* better on macOS (the new instance takes 100% immediately) while breaking *clustering*.
- [ ] **Step 4: Commit** — `docs: say what reuse_port actually does, and what macOS does with it`

---

## Task 10: Changelogs, map.md sync, and the e2e tier

**Files:** Modify both CHANGELOGs, `map.md`, `crates/shep-cli/tests/cli_e2e.rs`.

- [ ] **Step 1: E2E case — rotation.** Start a sheep, write lines, rename its log, `shep reopen`, write more, and assert the original path holds only the post-reopen lines while the renamed file holds only the earlier ones. Poll to a named deadline; never sleep once and assert.
- [ ] **Step 2: E2E case — flush.** Assert the file is empty afterwards and that the sheep keeps logging into it.
- [ ] **Step 3: Changelogs** (IR-45). `shep-core` gets `channel` and `log_level`; `shep-daemon` gets the subscriber, the two verbs and the SIGUSR2 behaviour; `shep-client` gets the inherent `next` and the `Stream` re-export. **Reconcile rather than append** — both files already carry substantial `[Unreleased]` sections, and several entries were extended in place rather than duplicated.
- [ ] **Step 4: map.md.** Add the log-plane entries and the `tracing-subscriber` decision. map.md has twice been synced to what a plan *expected* rather than what shipped — verify each claim against the code before writing it, and cite by symbol, not line number.
- [ ] **Step 5: Report to Rin** — every third-party API where reality differed from this plan, every judgement call made on her behalf, and the final dependency-count delta.
- [ ] **Step 6: Commit** — `docs: record the log plane`

---

## Exit criteria

1. All ten tasks complete and individually reviewed.
2. Every gate in Global Constraints green from its own exit code, including both bench-crate gates and the `-Z minimal-versions` rehearsal.
3. `cargo publish --dry-run` reaches the ordering error (not the manifest error) for all four crates.
4. A consumer can call `EventStream::next()` with no `futures-util` dependency — pinned by a test that imports nothing.
5. `grep -rn "no tracing-subscriber\|tracing-subscriber is wired" crates/ docs/` returns nothing.
6. `grep -rn "Task [0-9]\|Phase 5" crates/` returns nothing on lines this phase adds — **both halves**: files this phase creates, *and* lines added to files it only modifies. The second half was missed in Phase 4 and a marker shipped.
7. External rotation works both ways, proven end to end: `copytruncate` (already working — assert it still does) and rename-then-`reopen`.
8. Every test added carries a "fails if" comment naming the mutation it catches, and a reviewer picking three at random can break the implementation in the named way and watch the named test go red.
9. Neither suite run leaves a process reparented to init, calibrated by forcing one deliberate panic — a green suite never exercises the teardown path the guards govern.
