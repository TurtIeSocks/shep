# Phase 3 readiness decision — where the daemon-boot `unsafe` goes, or whether it goes away

Status: research for the maintainer's ruling · 2026-08-08 · read-only investigation, no code changed.
Ground truth: `crates/shep-daemon/src/sys.rs` (rationale essay + `adopt_fd`'s `# Safety`),
`crates/shep-daemon/src/boot.rs` (`BootOptions`, `boot`, `write_ready`, `DaemonReady`,
`PidfileLock`), IR-7/IR-22/IR-23/IR-24, spec §3/§6/§7/§10/§11/§13.4, SECURITY.md,
`docs/systematic-refactor/refactor-workspace/{map.md,decision-briefs.md}`, and the
Phase 2b plan's own Decision-1 corrections.

**Ranking: C > A > B.** Option B is unsound in the form proposed. Option A costs two IR
rules and re-litigates a call the maintainer already made. Option C deletes the mechanism.

---

## 0. Three facts the options have to be scored against

Established by reading, not assumed. All three cut the same way.

**Fact 1 — `HelloAck` is a strict superset of `DaemonReady`.**
`crates/shep-core/src/protocol/request.rs:21` — `HelloAck { daemon_version, protocol, pid }`
against `boot.rs:388` — `DaemonReady { pid, version }`. The pipe's payload is not merely
redundant with the handshake the client performs as its very next action; it is a *weaker*
duplicate, because it omits `protocol`, and protocol skew is the thing spec §6 requires be
"a typed error, not silence". Nothing can be learned from the readiness line that the
CLI does not immediately re-learn, better, from `HelloAck`.

**Fact 2 — the poll-connect-with-backoff loop is already required to exist.**
Spec §6: "Client reconnect: backoff 100ms ×1.5, cap 5s." The Phase 2b plan parks it
explicitly: "*that is client-side state; it lands in shep-client with connect-or-spawn
(Phase 3)*". Option C therefore adds no machinery — it reuses a loop Phase 3 must build
regardless, and adds one `try_wait()` call to it. The pipe is the *second* mechanism, not
the first.

**Fact 3 — the essay's systemd claim is wrong, and it is load-bearing for IR-24.**
`sys.rs:33-40` rejects the safe socket alternative partly because a socket "puts the
readiness handshake on a different mechanism from the one the spec, systemd `Type=notify`
integration, and every comparable supervisor use." sd_notify is *not* an inherited fd —
it is `NOTIFY_SOCKET`, a unix **datagram socket path** the daemon connects to and sends
`READY=1` on. Spec §11 commits shep to generating `Type=notify` units, so shep must ship
the socket-shaped readiness path anyway. The pipe is not the shared mechanism; it is the
odd one out. IR-24 requires the rejected alternative's cost be stated honestly, and this
one is stated backwards.

Two corollaries worth pinning because they get conflated:

- **`SHEP_READY_FD` ≠ `SHEP_CHANNEL_FD`.** map.md Design decision #2 ("fd-pipe protocol
  + probe-based readiness in v1") and decision-briefs.md #2 are about the **sheep→shepherd**
  channel (spec §7, `SHEP_CHANNEL_FD`), which the daemon *creates* with `command_fds`
  (`tokio_runner.rs:26,156`) and therefore owns both ends of — no adoption, no unsafe, ever.
  Option C does not touch it. The daemon-boot pipe (`SHEP_READY_FD`, spec §3) is a separate
  mechanism and was never the subject of a ruled decision brief.
- **SECURITY.md does not document `SHEP_READY_FD`.** The task brief says it does; it does
  not. SECURITY.md names `SHEP_CHANNEL_FD` (line 45) and nothing else fd-shaped. The only
  `SHEP_READY_FD` trust-boundary prose in the repo is `server.rs:84-88` and a Phase 2b plan
  checklist item. Deleting the pipe costs SECURITY.md zero edits.

---

## 1. Option A — enumerated carve-out in shep-cli's `main`

**What it costs, precisely: two IR rules, not one.** `#[allow(unsafe_code)]` cannot
override `#![forbid(unsafe_code)]` — `forbid` is not overridable by an inner `allow`; it is
a hard compile error to try. So Option A does not just amend IR-22, it forces shep-cli from
`forbid` down to `deny`, amending **IR-7** ("`#![forbid(unsafe_code)]` in shep-core,
shep-client, shep-cli") as well. That trades a *mechanically enforced* property for a
*conventionally enforced* one, in the crate with the largest surface area, the most future
growth (clap surface, TUI, whistle, serve, dogs, import, runtime/dev modes), and the most
agent-written code. `forbid` is the thing that makes IR-22 self-policing; `deny` + a
convention that the allow stays narrow is the thing that erodes.

**It is also the ruling the maintainer already made, relocated one crate over.** Yesterday a subagent
proposed rewording IR-22 to permit an `unsafe fn`'s call-site block outside `sys.rs`. She
rejected the rewording and chose "fix the design instead". Option A is that rewording with
`boot.rs` swapped for `main.rs`. The fix it replaced (b729ed9) is barely a day old.

**What it gets right:** it is honest. The precondition genuinely is a caller obligation, and
`unsafe fn` genuinely is the right shape for it. If the pipe survives, A is the only correct
way to call `adopt_fd` — B is unsound and C removes the caller. Rank it second, not third.

## 2. Option B — safe wrapper in sys.rs

### It is unsound. Say it once, plainly, so it is never re-proposed:

> **A safe `fn` that turns an environment-supplied fd number into an owning `File` is
> unsound, because a safe caller can call it after the process has opened descriptors of
> its own, `from_raw_fd` an integer that now names one of them, and close that resource out
> from under its real owner on drop — an I/O-safety violation (RFC 3128) reached with no
> `unsafe` keyword anywhere at the call site. `Once`, env-var removal, the fd-3 floor, and
> the `F_GETFD` probe each close a different hole and none of them closes this one.**

Walked through, because the proposal sounds plausible:

| Guard proposed | What it actually closes | Why the hole survives |
|---|---|---|
| `std::sync::Once` | double adoption (scenario (d)) | one adoption of a recycled number is already fatal |
| remove the env var after reading | a *later* reader seeing a stale number | the number was already consumed by this call; also `remove_var` is itself `unsafe` in edition 2024 (thread-safety), so it imports a second caller obligation to discharge the first |
| `fd >= 3` floor | scenario (a), hostile `SHEP_READY_FD=1` | fd 4 can be recycled exactly as easily as fd 1 |
| `fcntl(F_GETFD)` probe | scenario (b), closed/never-opened numbers | proves the number is open *right now*, never who opened it — `sys.rs:138-143` already says this in its own words |

Concrete failure with every guard in place: the CLI opens the Flockfile → fd 3. `SHEP_READY_FD=3`
survives in the environment from a grandparent, or an operator sets it, or a wrapper script
leaks it. `take_ready_pipe()` passes the floor, passes the probe, hands back a `File`
aliasing the live Flockfile handle, writes a JSON readiness line into it, and closes it on
drop. The original `File` now names a closed descriptor; the next read through it hits
whatever the kernel recycles into that slot. No caller wrote `unsafe`.

**This is not a new bug — it is the exact bug b729ed9 fixed, moved one layer up.** The
pre-Decision-1 shape was a safe `BootOptions { ready_fd: Option<RawFd> }` driving an
internal `unsafe { sys::adopt_fd(fd) }`; the whole-branch review found it unsound for this
reason, and the Phase 2b plan's own correction records it: "*a safe caller could build a
`BootOptions` that drove `boot`'s internal `unsafe { sys::adopt_fd(fd) }` block into UB with
no `unsafe` keyword of its own*". Option B rebuilds that hole with a shorter parameter list.

### The one variant that *is* sound, and why it still loses

`dup(2)` instead of `from_raw_fd`. Duplicating a number you do not own never takes ownership
and never double-closes, so the I/O-safety violation disappears entirely; the residual risk
downgrades from unsound-in-the-Rust-sense to "we might write one JSON line into the wrong
open file", which an `fstat`/`S_ISFIFO` check narrows further. A safe `fn take_ready_pipe()
-> Option<File>` built on `dup` is genuinely sound.

It still loses, on cost:

- The inherited original is never closed, so the pipe never sees EOF from the daemon side.
  `write_ready`'s contract ("dropping `pipe` at the end of this call is the parent's own EOF
  signal", `boot.rs:394-397`) breaks; the parent must switch to `read_line`.
- Worse, the un-closed original is not `CLOEXEC` (it was inherited across `exec`), so every
  managed sheep the daemon spawns inherits the readiness write end. A sheep outliving a dead
  daemon then holds the pipe open and the parent's EOF-means-died signal is gone for good.
  Fixable with an `fcntl(F_SETFD, FD_CLOEXEC)` on a descriptor we deliberately do not own,
  which is defensible but is exactly the kind of "clever, opaque" the core principles rank
  below "readable, simple, slightly repetitive".
- And it buys all that to preserve a mechanism Facts 1–3 show is redundant.

So: B-as-proposed is unsafe-hiding and must be refused; B-as-`dup` is sound and dominated.

## 3. Option C — delete the readiness pipe

### What is actually lost

Costed honestly, one line per claim.

| Claimed loss | Real? | Detail |
|---|---|---|
| Edge trigger → level poll adds latency | **Yes, the one real cost** | Cold start pays up to one poll interval. Spec §6's 100 ms first interval would be a visible regression on every daemon-spawning invocation. Mitigation: start the ladder tight (2–5 ms, ×1.5 into the spec's 100 ms→5 s shape) — daemon boot to socket-bind is a few ms, so it converges in one or two ticks. |
| "Daemon died" detection | **No** | `try_wait()` gives the *exit status*; the pipe gives an anonymous EOF. Strictly more information. |
| Failure diagnostics | **No, improves** | The pipe carries no error variant; a failed boot just closes it. With the child's stderr redirected to `$SHEP_HOME/logs/shepd.log`, the CLI can report status *and* where to look. (Both designs need that redirect, so it is not a cost attributable to C.) |
| Identity (`pid`, `version`) | **No** | Fact 1 — `HelloAck` carries both plus `protocol`. |
| Distinguishing "our daemon" from "someone else's" | **No, improves** | If our child loses the `PidfileLock` race and exits `AlreadyRunning`, poll-connect still succeeds against the winner — which is the correct outcome for connect-or-spawn. The pipe design has to special-case that: it sees EOF-without-line and reports failure while a perfectly good daemon is serving. |
| systemd `Type=notify` | **No — confirmed separate** | Fact 3. `NOTIFY_SOCKET` datagram, no fd inheritance, unimplemented today, unaffected either way. |
| Spec §7 readiness | **No — different subsystem** | §7 is per-sheep: shepherd channel (`SHEP_CHANNEL_FD`) + probes. Untouched. |
| Spec §13.4 flagship (reboot → `shep muster`) | **No** | Under a `Type=notify` unit the daemon *is* `ExecStart`; there is no parent CLI and no pipe in that path at all. |
| Windows (spec §11 functional tier) | **Improves** | Named pipes, no fd inheritance — the `SHEP_READY_FD` design needs a second, Windows-shaped readiness mechanism. Poll-connect is one mechanism on both platforms. |

**Net:** the pipe's entire remaining value is "≤ one poll interval sooner", paid for with the
only `unsafe` in the workspace, a unix-only code path, a trust boundary, and an env var.

### What gets deleted

| Location | What goes |
|---|---|
| `crates/shep-daemon/src/sys.rs` | the whole file — 298 lines, `adopt_fd`, `SysError`, the IR-24 essay, 5 tests, the crate's only `#[allow(unsafe_code)]` |
| `crates/shep-daemon/src/lib.rs` | `mod sys;` + taxonomy entry; `testing::FD_REUSE_LOCK` and its ~20-line doc (only users are `sys.rs`'s tests); `#![deny(unsafe_code)]` → `#![forbid(unsafe_code)]` |
| `crates/shep-daemon/src/boot.rs` | `READY_FD_ENV`, `DaemonReady`, `write_ready`, `BootOptions::ready_fd`, `BootError::ReadyWrite`, `boot` step 3, the "No unsafe in this module" doc block, and 2 tests (`readiness_reports_pid_and_version_then_closes_the_pipe`, `boot_writes_readiness_to_the_callers_pipe_after_the_socket_is_bound`) |
| `crates/shep-daemon/src/server.rs:84-88` | the `SHEP_READY_FD` paragraph in the canonical security writeup |
| docs | IR-22 (retire or restate), IR-7 (widen to all four crates), spec §3's readiness-handshake parenthetical, map.md's `rpc_server.rs` note ("readiness handshake via pipe"), the Phase 2b plan's Task 7 / success-criteria correction chain, a new CHANGELOG entry |

`nix` stays (`Flock`, signals). `command_fds` stays (shepherd channel). Nothing in shep-core
or the wire protocol changes.

### What Phase 3 then builds

1. **`shep-client::connect_or_spawn`** — try connect; on `NotFound`/`ConnectionRefused`,
   spawn, then poll-connect to a deadline.
   - Spawn: `Command::new(current_exe()).arg("daemon")`, `.process_group(0)` (safe, std
     since 1.64 — already the pattern at `tokio_runner.rs:156`), `stdin(null)`,
     stdout/stderr appended to `$SHEP_HOME/logs/shepd.log` at 0600. **No `pre_exec`, no
     `setsid`, no unsafe.**
   - Poll ladder: tight first tick (2–5 ms) ramping ×1.5 into spec §6's shape; overall
     deadline 5 s (matches §6's cap and the default client deadline).
   - Liveness: `child.try_wait()` each tick → `Some(status)` yields a typed
     `ConnectError::DaemonExited { status, log_path }` instead of a timeout.
   - Take the daemon command as a builder field (IR-builder preference) so tests can point
     it at a fake and cover both the died-early and never-ready arms without a real daemon.
2. **`shep-cli daemon` subcommand** — `BootOptions { socket, restore }`, no `ready_fd`;
   `boot(...).await?.run().await`.
3. **Lints** — all four crates `#![forbid(unsafe_code)]`; verify with
   `cargo clippy --workspace -- -D warnings` and a grep for `unsafe` returning nothing.

### The failure mode a reviewer will find later — naming it now

**The `bind` → `serve` gap becomes observable.** `boot` binds the socket at step 2 and
returns; `RpcServer::serve` only starts inside `RunningDaemon::run`, *after* `boot`'s step 4
muster restore. A connect therefore succeeds the instant the listener exists (kernel backlog),
but the first `Hello` sits unanswered for the whole restore window. The pipe fires at the
same instant, so the semantics are **identical** — but Option C makes the CLI *notice*,
because its success criterion is a completed handshake rather than a line on a pipe. On a
large muster roll the CLI's default 5 s Hello deadline can expire against a daemon that is
booting perfectly well.

Two honest responses, both cheap: give the *first* Hello after a spawn a longer deadline than
the steady-state 5 s, and/or start `serve()` before the restore so the handshake is answered
while the flock comes back up. Prefer the second — it makes "connected" mean "serving",
which is what every later caller assumes anyway. Either way it is a Phase 3 line item, not a
blocker, and it is worth writing down before a reviewer finds it as a bug.

Secondary, lower severity: a CLI can connect to a daemon that is mid-teardown and get a
`HelloAck` moments before the socket is unlinked. Spec §6's reconnect backoff already covers
it; the pipe does not fix it either.

---

## 4. Recommendation and the sign-off question

**Recommend C.** The single deciding fact: **the readiness pipe is the only reason any
`unsafe` exists anywhere in this workspace, and it buys nothing the client's
already-mandated connect-with-backoff (spec §6) plus `HelloAck` does not already deliver —
`HelloAck { daemon_version, protocol, pid }` is a strict superset of
`DaemonReady { pid, version }`.**

**Does it need the maintainer's sign-off? Split the answer — and this is the part to act on.**

*Does not need sign-off, because Phase 3 needs it either way:* the poll-connect +
`try_wait` + backoff machinery in `shep-client` is required by spec §6 and by the
systemd-started-daemon case (a daemon shep did not spawn has no pipe to report on). Build it
now. Doing so makes the pipe provably dead code — which is also the strongest possible
evidence to hand the maintainer.

*Needs sign-off, because it is a ruling, not a design fix:* deleting `sys.rs`, retiring
IR-22, widening IR-7, editing spec §3, and removing the `BootOptions::ready_fd` field she
personally reshaped in b729ed9 **one day ago**. "Fix the design instead" licensed changing a
struct field. It does not obviously license deleting a spec'd protocol and retiring two
numbered rules — and doing that to her own day-old commit without asking reads as an agent
overriding a human ruling, even though the *direction* (remove the unsafe rather than widen
the rule) is precisely the one she chose. Note also that the pipe is **not** in map.md's
ruled-decision list or goals.md's open questions, so CLAUDE.md's "if a decision is listed
there, it is the maintainer's" does not formally reserve it — which is why this is a judgement call and
not a clear-cut hold.

**Sequencing that needs no permission to start:** build C's machinery (forced), leave
`sys.rs` and `ready_fd` in place and unreferenced by the CLI, and put the deletion +
IR amendment behind one yes/no from the maintainer. If she says no, Option A is the fallback and costs
one `#[allow]` plus the IR-7/IR-22 amendments — but at that point the pipe is *demonstrably*
carrying no weight, which is the argument she should get to weigh.

**Do not re-propose Option B.** See §2's boxed sentence.
