# What a Windows tier costs

> **Superseded 2026-08-26: the tier is built.** Everything below is kept as
> written, because it turned out to be a good estimate and the record of why
> is worth more than a tidy file. Read it as a forecast, not as current
> state — `docs/specs/deferred.md`'s Windows entry has what actually shipped.
>
> **§5's recommendation was followed exactly, and it is what unblocked
> this.** "Run the CI leg first" was the right first move: the answer was
> that the ~10% of the tree outside `cfg(unix)` was already green on native
> MSVC, in 17 seconds. §4's "nobody on this project has a Windows host" then
> stopped being true, which removed the argument §5 actually rested on —
> not that the work was hard, but that it would be done blind.
>
> **The shape predictions held; the cost predictions were pessimistic.** The
> PORT/REDESIGN/DROP split was right in every case, which is what §4 said to
> expect ("the *shape* is solid, because it falls out of API existence").
> The task count was high: §1's claim that "the 145 `cfg(unix)`-family sites
> are the least interesting number in the brief" was truer than it knew —
> only ten files in `shep-cli` contained a Unix API call at all, and
> un-gating really was an afternoon.
>
> **Three things this document did not anticipate**, each found only by
> running the code on a real host, and each a silent failure rather than a
> compile error:
>
> - `base_env()` is Unix-shaped. `env_clear()` without `SystemRoot` makes a
>   Windows child fail before `main`; `powershell` produces no output and no
>   error at all.
> - `OpenOptions::append(true)` strips `FILE_WRITE_DATA`, so the
>   `set_len(0)` in `launch.rs`'s `emptied_appending` returns
>   `ERROR_ACCESS_DENIED` — which broke `shep start`'s autostart entirely.
> - `git`'s `core.autocrlf` reddens three byte-exact fixture tests on any
>   Windows checkout, for reasons having nothing to do with the port.
>
> §4's list of six unanswerable questions was the right list. Two now have
> answers: **(1)** `CTRL_BREAK_EVENT` is not usable — a detached shepherd
> shares no console with its sheep — so the honest answer was "no graceful
> stop outside the channel", and that is what shipped. **(2)** breakaway is
> a non-issue, because `Job::create` does not grant it, so a job member
> cannot escape; `kill_tree` on Windows reaches strictly more of the tree
> than its unix twin, verified by mutation.

Written 2026-08-15, from five parallel surveys of the porting surface plus a
spot-check of every claim marked REDESIGN or high-risk. Phase 15 was landing
under the surveys as they ran; anything read from that in-flight work is
marked as such.

This document exists because `docs/specs/deferred.md` puts the Windows
functional tier last and says, honestly, that its estimate "is mostly
guesswork" — a decision brief's +30-40% on the daemon's process-control
layer. This is the attempt to replace that number with something Rin can
act on.

## 1. The headline

**Roughly 36-49 tasks across 4-5 phases, and it is a redesign wearing a
port's clothes.**

The 145 `cfg(unix)`-family sites are the least interesting number in the
brief. Un-gating them is close to free: `crates/shep-cli/src/lib.rs:29-41`
gates four whole module trees (`commands`, `dog`, `lookout`, `whistle`) at
the crate root, and grepping inside them finds that `import`, `whistle`'s
MCP loop, the metrics dog's `TcpListener` HTTP server, and the entire
ratatui/crossterm lookout render path contain no Unix API calls at all. The
blanket gate is doing double duty — hiding real platform gaps and hiding
files that have none. Splitting it is an afternoon.

The cost is concentrated in five or six places where the Unix design has no
Windows analogue, and where the *behaviour must change*, not just the call.
Counted by weight rather than by site:

- **PORT** (same design, different API): job objects for process groups,
  `TerminateJobObject` for `kill_tree`, `sysinfo` (already cross-platform),
  detach flags in `launch.rs`, the elevation probe, log-file share modes,
  the file locks. Tedious, low-risk, mostly mechanical once the seam exists.
- **REDESIGN** (the behaviour changes): graceful stop, the shepherd channel,
  the whole POSIX permission model, boot exclusivity and stale-socket
  recovery, service installation. These are where the phases go.
- **DROP** (no analogue exists, refuse honestly): seven of the nine named
  signals, `user`/`group` privilege drop, `is_rc_safe_user`.

The three architecture seams that make this tractable are already right, and
that is the good news. `crates/shep-daemon/src/kill.rs` says in its own
module doc that it "never touches OS signal APIs directly"; the
`RunningProcess`/`ProcessRunner` trait boundary at
`crates/shep-daemon/src/runner.rs:634-724` carries no OS types, so a
`WindowsRunner` slots into exactly the seam `TokioProc` occupies and none of
`supervisor.rs`'s Actor tests change. `brain.rs`, `backoff.rs` and `entry.rs`
— the restart budget, the backoff math, the `min_uptime` stability check —
have zero Unix sites between them, confirmed by grep rather than assumed.
And `crates/shep-core/src/paths.rs:40` already derives and tests a sanitized
`\\.\pipe\shep-<home>` name that nothing consumes yet. Somebody built for
this.

### Corrections to the surveys

Four claims did not survive the spot-check, and three of them make the
estimate *better*.

**The `forbid(unsafe_code)` blocker on the kv/bark locks is not real.**
`crates/shep-core/src/barks.rs:339-354` documents its own Windows no-op by
saying "shep-core is `#![forbid(unsafe_code)]` so `LockFileEx` is not ours
to call directly." True, but `LockFileEx` is not the only answer.
`std::os::windows::fs::OpenOptionsExt::share_mode(0)` has been stable and
safe since Rust 1.13, and an exclusive open of the sibling `.lock` file
gives the same guarantee for shep's use, which is a lock token and nothing
else. It is not a *blocking* lock, so `RingLock::acquire`'s "contention
blocks rather than failing" contract needs a bounded retry loop instead of
`FlockArg::LockExclusive` — a real difference, and one to write down — but
no unsafe, no new crate, and no exception carved into a crate-level
`forbid`. This downgrades a high-risk item to a medium one and should be
corrected in `barks.rs`'s comment whenever the tier is built.

**The `O_NOFOLLOW` refusal is cheaper than surveyed.** Survey 4 put the
reparse-point check in a new unsafe-permitted Windows sys module.
`OpenOptionsExt::custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)` plus
`std::fs::symlink_metadata` are both safe std, and together they give an
honest analogue: open the reparse point rather than following it, inspect,
refuse. The threat model still shifts (directory junctions need no
privilege on Windows where file symlinks do, and NTFS hardlinks have no
Unix-side precedent in `runner.rs` at all), so this stays a REDESIGN of the
*check*, but not of the *plumbing*.

**`PidfileLock` ports cleanly, and survey 2 was wrong to fold it into the
socket redesign.** The load-bearing property in
`crates/shep-daemon/src/boot.rs:212-245` is that flock's locks are owned by
the open file description and the kernel releases them on process death,
`SIGKILL` included, with no unlock call — which is exactly what makes a
crashed daemon's stale pidfile harmless. Windows handles close on process
termination too, so an exclusive `share_mode(0)` open of the pidfile
preserves the property that matters. The daemon's single-instance guarantee
survives the port.

**SO_REUSEPORT is not absent from the tree.** Survey 2 reported two hits,
both doc comments in `cli.rs`. There are eleven, and one of them is a real
`setsockopt(&sock, sockopt::ReusePort, &true)` in
`crates/shep-daemon/examples/reuse_port_sheep.rs:182`, which
`daemon_e2e.rs:1951` depends on for the reload test. The survey's
*conclusion* holds — shep binds nothing and sets this on nothing, it is
advice printed to app authors — but the correction matters for coverage
accounting: the zero-downtime-reload end-to-end test is built on a Unix-only
example sheep and would need a Windows twin, or that behaviour goes
unverified on the new tier.

One more thing the surveys under-counted. `crates/shep-cli/Cargo.toml:150-172`
gates `ratatui`, `crossterm`, `rmcp` and `schemars` behind
`[target.'cfg(unix)'.dependencies]`, with a comment explaining that
declaring them unconditionally would pull `crossterm_winapi` and a second
`windows-sys` face into a binary that cannot use any of it. So "lookout is
already portable" is true of the code and false of the build: un-gating it
means accepting those dependencies on the Windows leg and a slower
cross-check. Small, but it is not zero, and the Cargo.toml comment will read
as a contradiction to whoever un-gates it without reading this.

## 2. The three things that actually cost

### Graceful stop, because Windows has no SIGTERM

This is the deepest item, and it is worth being clear that it is not
plumbing. The polite-then-brutal ladder in `kill.rs:57-82` and
`tokio_runner.rs:114-116` is most of what shep *is* over `start /b` — send
the configured `StopSignal` to the whole process group, wait `grace`,
escalate to `kill_tree`. Windows has no mechanism to deliver anything
SIGTERM-shaped to an arbitrary foreign process.

The nearest thing, `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pgid)`,
reaches only console-subsystem processes that were spawned with
`CREATE_NEW_PROCESS_GROUP` and that installed their own
`SetConsoleCtrlHandler`. A GUI app, a process with no console, or a plain
Rust binary that never registered a handler gets nothing gentler than
`TerminateProcess`.

**What shep would do instead:** make the shepherd channel's
`shutdown_with_message` the only reliable graceful path on Windows, and let
the bare ladder degrade to "wait `grace`, then `TerminateJobObject`."

**What shep would have to admit:** that `shep stop` on Windows is `shep
kill` for any app that did not opt into the channel. Not a footnote — an
operator-facing behaviour difference that belongs in `--help`, in the docs,
and in the release notes. And it is coupled to the next item, which is what
makes the pair expensive: the honest answer to "no SIGTERM" leans entirely
on the mechanism that also has to be rebuilt.

### The shepherd channel, because fd 3 does not exist there

`crates/shep-daemon/src/tokio_runner.rs:299-333` builds a
`UnixStream::pair()`, clears `O_NONBLOCK` on the child's end deliberately so
a plain blocking read works, and hands it over as literal fd 3 via
`command_fds::FdMapping`. `docs/shepherd-channel.md:29-44` publishes that as
a contract to app authors, and the decisive sentence is this one: "a shell
script doing `read -r line <&3` works." `cmd.exe` has no fd-3 redirection.
The contract as written cannot hold, regardless of how clever the handle
inheritance gets, and `command-fds` is itself a `pre_exec`-based Unix-only
crate (`crates/shep-daemon/Cargo.toml:62-64`).

**What shep would do instead:** a named pipe whose path the daemon exports
as a new `SHEP_CHANNEL_PIPE` environment variable, which the app opens
itself. The wire format is untouched — newline-delimited JSON, the same
`ready`/`metric`/`action-reply` outbound and `shutdown`/`action` inbound
shapes — so `shep trigger`'s request/reply flow and the correlation id
survive unchanged. Only the "how do I get the descriptor" step moves.

**What shep would have to admit:** that every existing integration written
against the published contract needs a Windows branch, and that
`docs/shepherd-channel.md` needs a genuine rewrite rather than a relabel —
its whole mental model is an inherited descriptor and a blocking `read()`.
`SHEP_CHANNEL_VERSION` exists precisely so a defensive app can notice, which
helps; the daemon exporting both variables and letting apps branch on which
is present is probably the cheapest migration.

This item and the previous one land in the same function —
`tokio_runner.rs`'s single `spawn()` call site wires the channel, the
process group and the credentials together. They must be planned as one
piece of work, not two independently schedulable ones.

### The permission model, because POSIX mode bits are a scalar and Windows ACLs are a relationship

Four separate mechanisms are the same problem wearing four hats. `0700` on
`$SHEP_HOME` (`boot.rs:70,97-129`) is the primary access-control layer, and
`server.rs:56-59` says so outright — the same-uid `peer_cred()` check at
`server.rs:188-207` is explicitly *secondary* to the directory being
unreachable. `runner.rs:480-502` walks log-path ancestry looking for a
foreign owner uid or a world-writable bit. And
`tokio_runner.rs:263-282` drops privilege with `Command::uid()/gid()`,
relying on std's documented `setgroups(0, NULL)`-before-`setuid` ordering.

Windows answers all four with ACLs, SIDs and tokens. That is a different
model, not a different call: "owner-only" is an ACE granting one SID, built
and applied and protected from inheritance, not a number.

**What shep would do instead:** construct an explicit DACL for `$SHEP_HOME`
and for the control pipe granting only the daemon's own SID, with
`PROTECTED_DACL_SECURITY_INFORMATION` so it does not inherit the parent's;
replace the peer-uid check with `ImpersonateNamedPipeClient` plus a token
SID comparison. This is the one place a real unsafe FFI surface is
unavoidable, and it belongs in a `sys_windows.rs` beside the existing
`sys.rs`, the crate's only home for unsafe per IR-22/23.

**What shep would have to admit:** that `user` and `group` in a Flockfile
refuse on Windows. The honest equivalent is `CreateProcessWithLogonW` or
`CreateProcessAsUser` against a real token, which needs either a password at
spawn time or `SeAssignPrimaryTokenPrivilege` and a full LSA logon session.
That is a materially different, security-sensitive feature, and a partial
version of it would be worse than a refusal. And until the DACL work lands,
a freshly created `$SHEP_HOME` on Windows inherits whatever ACL its parent
has — normally readable by other local accounts, which is the exact posture
spec §10 says shep refuses on Unix. That regression must not ship silently.

## 3. The split

Yes, there is a smaller tier worth shipping first, and the line is clean.

**Tier A — the developer laptop.** `shep start`, `stop`, `restart`, `list`,
`logs`, `describe`, `delete`, `import`, `lookout`, `whistle`, the metrics
dog. A shepherd you launch yourself, in your own session, that does not
survive a reboot. Roughly 28-39 tasks.

It needs, in order: the transport seam done once (`shep-client`'s
`connection.rs` hardcodes `UnixStream` in its `Frames` alias, so this is an
actual `Transport` abstraction, not a type swap — and it is the single
highest-leverage item in the whole tier, because everything else waits on
it); job objects replacing process groups and `TerminateJobObject` replacing
negative-pid `SIGKILL`; the channel and graceful-stop redesign together; the
filesystem tier; the module-gate split; and a Windows arm for the four
`tokio::signal::unix` sites at `lookout/mod.rs:293`, `dog/bark/mod.rs:209`,
`dog/metrics/mod.rs:23` and `boot.rs:51`.

Once the transport and the spawn path land, a genuinely large amount comes
along for free. `sysinfo` already supports Windows in both call sites; the
metrics dog's HTTP endpoint is a plain `TcpListener`; lookout is pure
ratatui and crossterm; `shep import` is JSON and TOML text with no OS calls
in any of its five files. Those were never the hard part.

**Tier B — the production supervisor.** Everything above, plus surviving a
reboot: a Windows Service hosted through the SCM
(`StartServiceCtrlDispatcher`, a control handler answering
`SERVICE_CONTROL_STOP`, `SetServiceStatus` transitions, realistically via
the `windows-service` crate), and a real `CreateService`/`DeleteService`
install path for `shep startup`. That last one is not a sixth template
beside the five that `commands/startup/` renders to disk — the SCM's service
database is registry-backed, so the refuse-if-exists and refuse-if-
unprivileged control flow ports while the write mechanics underneath do not.
Roughly 8-10 tasks.

**The verbs that would still refuse inside a shipped Tier A**, and this has
to be said in the release notes rather than implied:

- `shep signal` — seven of its nine names (HUP, QUIT, USR1, USR2, WINCH,
  CONT, TERM) have no Windows delivery mechanism to a foreign process at
  all. KILL maps to `TerminateProcess`; INT maps loosely to
  `CTRL_C_EVENT` with every console caveat above. The right shape is a
  per-signal refusal replacing today's whole-verb one. This verb stays
  partly refused even after Tier B.
- `shep startup` and `shep unstartup` — Tier B only, by definition.
- `user` and `group` in a Flockfile — refused, permanently, per above.
- `shep kv` and bark-sink locking — refused *unless* the exclusive-open lock
  is built. A no-op lock that silently succeeds, which is what
  `barks.rs`/`kv.rs` compile to on non-Unix today, is worse than a refusal.
  This is the item most likely to look like it "just works" if nobody names
  it.

## 4. What cannot be estimated from here

Nobody on this project has a Windows host. `.github/workflows/test.yml` is
`on: workflow_dispatch:` and has never been run — `deferred.md:118-135` has
the reasoning and the billing arithmetic. So every runtime claim in this
document is read from source plus documented Win32 semantics, and the task
counts are guesses calibrated against this codebase's own phase sizes, not
against measured Windows work.

The task numbers are the softest thing here. The *shape* — which items are
PORT, which are REDESIGN, which have no analogue — is solid, because it
falls out of API existence rather than API behaviour. The counts could be
off by a third in either direction.

Specifically unanswerable without a host:

1. Whether `CTRL_BREAK_EVENT` reaches a child spawned with
   `CREATE_NEW_PROCESS_GROUP` without also hitting the daemon, and whether
   an unmodified Node, Python or Rust child treats it as anything gentler
   than a hard terminate. This decides whether the honest answer is "a
   degraded graceful stop" or "no graceful stop at all outside the channel."
2. Whether `TerminateJobObject` reaches a grandchild that requested
   `CREATE_BREAKAWAY_FROM_JOB` — the direct parity question to the escaped-
   `setsid` gap `kill.rs:52` already documents on Unix.
3. Whether `tempfile`'s `persist()` — which is `MoveFileExW` with
   `MOVEFILE_REPLACE_EXISTING`, verified by reading tempfile 3.27.0's own
   Windows backend — actually throws `ERROR_SHARING_VIOLATION` at a rate
   that matters in shep's real access pattern. No reader in this codebase
   opens with `FILE_SHARE_DELETE`, so POSIX's "rename always succeeds"
   becomes probabilistic. `ReplaceFileW` is the Windows-native primitive
   built for this case and tempfile does not use it.
4. Whether an external log rotator can rename shep's open log file even
   after `open_append` gains `FILE_SHARE_DELETE`, and what a reader sees
   during Windows' delete-pending window.
5. Whether `MAX_PATH` bites. This depends entirely on host configuration
   shep cannot control, and a long sheep name under a OneDrive-synced
   profile makes it realistic rather than theoretical.
6. Whether Defender or enterprise EDR interferes with job object
   assignment, named pipe creation, or a self-daemonizing binary that
   re-execs itself with a hidden subcommand. A process manager's real
   deployment shape is a locked-down corporate host, and this risk category
   cannot even be named concretely without trying it.

**The first experiment is not any of those.** It is dispatching the existing
`test.yml` workflow once, manually. It costs Actions minutes and five
minutes of attention, and it answers the only question worth asking before
any of this is scoped: is the ~10% of the tree that is not behind
`cfg(unix)` even honestly green on `windows-latest` today? Nothing in this
repository currently knows. Do that before anything else.

The second is a Windows VM running `shep start`, `shep lookout` and a
`shep whistle` round-trip once a transport exists. There is no substitute
and no way to skip it.

## 5. The recommendation

**Ship the alpha without Windows, and do not commit to Tier A yet. Run the
CI leg first.**

The argument against building it now is not that it is hard — it is 4-5
phases, which this project has demonstrably absorbed before. It is that
every one of those phases would be built blind, by a maintainer who cannot
run the result, against six open questions whose answers change the design
rather than the implementation. Question 1 alone decides whether `shep stop`
on Windows is a degraded feature or an absent one, and that is a product
decision that should be made with evidence.

The option nobody has said out loud is that **"shep does not support
Windows" is a legitimate permanent answer for a process manager.** It is
worth saying plainly, because the alternative has a cost that does not
appear in any task count: a Windows tier is not built once. It is
maintained forever, on a platform no maintainer runs, where every future
feature needs a second design, every bug report arrives unreproducible, and
CI is the only thing standing between the tier and silent rot — CI that
today has never run. Look at what that already cost: the Linux abstract-
namespace branch went five phases without a compiler reading it, and the
windows-gnu cross-check went three phases unrun after silently falling out
of the plan template. Those are the *cheap* checks. A functional tier is a
standing obligation an order of magnitude larger.

What it costs in users is real but bounded, and honesty is most of the
mitigation. The competitor comparison is the thing to be clear-eyed about:
pm2 runs on Windows, imperfectly, and some fraction of the audience shep is
addressed to will bounce on that alone. But the deployment target for a
process supervisor is overwhelmingly Linux, WSL2 covers the Windows
*developer* case at zero cost to this project, and the honest failure mode
of the current arrangement — one clear line of stderr and exit 1 — is far
better than a half-tier that silently no-ops its file locks and calls
`TerminateProcess` a graceful stop.

So: state the position in the README rather than leaving it as a deferred
item that implies it is coming. Say "Linux and macOS; Windows via WSL2" as a
statement of scope, not an apology. Then dispatch `test.yml` once, and if
Rin still wants the tier after seeing what the compile-only leg reports, the
first phase to build is the transport seam — it is the highest-leverage item
in the estimate, it unblocks everything else, and it is worth doing on its
own merits even on Unix, because a `Transport` abstraction is a better shape
than `Frames = Framed<UnixStream, LengthDelimitedCodec>` regardless of
whether a named pipe ever appears behind it.

That is the one piece of Windows work that pays for itself before Windows
exists. Everything after it should wait for a host.
