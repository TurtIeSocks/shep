# Why shep is built this way

The reasoning behind decisions that the code records the outcome of but not the
argument for. Most of it is a choice between real alternatives, and the entry
worth reading is usually the one saying what was tried and rejected, because
nothing in a shipped codebase records a path not taken.

Extracted on 2026-08-29 from the 25 implementation plans under
`docs/writing-plans/` and the 7 research notes under `docs/research/`, which
were deleted in the same commit. Those files were step-by-step task lists for
work that has since shipped, so what was worth keeping was the reasoning and
not the instructions. Everything here was checked against the code as it stood
that day; an entry marked **superseded** is one the code has since moved past,
kept because knowing a rule was deliberately reversed is worth as much as
knowing it holds.

`source` on each entry points into the deleted file, so `git log` is where to
go for the full argument. The commit that removed them names itself.

## Contents

- [Core types and the daemon's shape](#core-types-and-the-daemons-shape) (4)
- [The CLI surface](#the-cli-surface) (4)
- [Supervision and lifecycle](#supervision-and-lifecycle) (17)
- [The log plane](#the-log-plane) (6)
- [Reload](#reload) (9)
- [Custom actions and the shepherd channel](#custom-actions-and-the-shepherd-channel) (9)
- [The pm2 cutover](#the-pm2-cutover) (17)
- [Dogs](#dogs) (31)
- [Audit debt](#audit-debt) (11)
- [The Phase 11 verbs and the KV store](#the-phase-11-verbs-and-the-kv-store) (23)
- [lookout](#lookout) (35)
- [whistle](#whistle) (18)
- [Config and packaging](#config-and-packaging) (5)
- [serve, dev and runtime](#serve-dev-and-runtime) (8)
- [Output and first run](#output-and-first-run) (7)
- [Config overrides](#config-overrides) (8)

## Core types and the daemon's shape

### OwnedFd::from over into_raw_fd/from_raw_fd for channel adoption

Adopting the child's shepherd-channel socket end in tokio_runner.rs uses `OwnedFd::from(std_end)` rather than `into_raw_fd()` + `from_raw_fd()`.

**Why:** from_raw_fd is unsafe; OwnedFd::from(UnixStream) is a safe conversion. Choosing it kept tokio_runner.rs unsafe-free under the crate's #![deny(unsafe_code)], confining the crate's one unsafe operation to sys.rs.

`docs/writing-plans/plans/2026-08-07-shep-phase2a-daemon-engine.md:515; verified crates/shep-daemon/src/tokio_runner.rs (OwnedFd::from usage, grep found zero `unsafe` in the file)`

### Per-sheep task owns the process, not the actor

Each spawned instance gets its own tokio task that owns the live (proc, ProcIo) pair for its whole life; the supervisor actor holds only lifecycle state plus a fire-and-forget control sender, never a proc.

**Why:** If the actor held the proc directly, the code path that awaits its exit and the code path that runs the kill ladder against it would both need `&mut` access to the same value at the same time - unresolvable. A dedicated task per sheep gives each proc exactly one owner.

`docs/writing-plans/plans/2026-08-07-shep-phase2a-daemon-engine.md:7 (Architecture line); verified crates/shep-daemon/src/supervisor.rs:1-9 module doc (states the what, not the &mut-exclusivity reason)`

### Readiness signaling: three designs, two rejections - **superseded**

Readiness-on-boot went through three shapes. Rejected first: a second control socket (SHEP_READY_SOCK) the child connects to and writes on. Built instead: an inherited fd adopted via an unsafe `adopt_fd`. A later bug review found that shape unsound (a fully safe caller could build a BootOptions that drove adopt_fd's internal unsafe block into UB with no unsafe keyword at the call site). The maintainer explicitly rejected fixing this by widening the 'one unsafe site' rule to two documented sites; the fix that shipped instead retyped `BootOptions::ready_fd` as `Option<std::fs::File>` and had the CLI's main simply never populate it - readiness ships via a handshake, and the whole adopt_fd apparatus is now dead code by design.

**Why:** SHEP_READY_SOCK rejected: cost of a second thing needing 0700/unlink/stale-recovery to replace a five-line adoption, and a different readiness mechanism than spec/systemd/every comparable supervisor uses. The 'two unsafe sites' fix rejected: it treats a soundness bug as a documentation problem instead of a design problem.

`docs/writing-plans/plans/2026-08-07-shep-phase2a-daemon-engine.md:17 (correction chain); docs/writing-plans/plans/2026-08-07-shep-phase2b-daemon-plane.md:1952-1953, 2033, 2938; verified crates/shep-daemon/src/sys.rs:1-90 (rejected-alternative text ported into code verbatim, but still frames adopt_fd as awaiting a Phase 3 caller) and crates/shep-cli/src/commands/daemon.rs:323,357 (`ready_fd: None` unconditionally, comment says readiness is a handshake). Replaced by: a handshake protocol; sys.rs's own module doc has not been updated to say adopt_fd's intended caller was never written.`

### ResolvedApp's config field is private, not public

ResolvedApp (the normalize() proof-token) got a private `config` field plus `config()`/`into_config()` accessors instead of the plan's own template code, which had written `pub config: AppConfig`.

**Why:** A public field would let any crate construct a ResolvedApp directly with a struct literal, forging the 'this was validated' guarantee that normalize() exists to provide. Flagged explicitly in the plan's interface note as something 'caught in execution review' - the plan's own code sample got this wrong and the review caught it.

`docs/writing-plans/plans/2026-08-07-shep-phase1-foundation.md:1226 (interface note) vs 1305-1309 (contradicting sample); verified crates/shep-core/src/config/normalize.rs:66-81`

## The CLI surface

### bleats --format json emits bare lines, no envelope

bleats is the one CLI verb whose JSON output is not wrapped in the client's normal command envelope - one `{schema_version, id, name, stream, line}` object per line.

**Why:** A follow has no end, so there is nothing to close a wrapping envelope with. The rejected alternative was an envelope whose `data` field is an array, which only terminates once `--no-follow` stops the stream - meaning the streaming case would have needed a different output shape than the terminating case, on what is meant to be one command.

`docs/writing-plans/plans/2026-08-08-shep-phase3-cli.md:3132; verified crates/shep-cli/src/commands/bleats.rs:1-4 (module doc states the 'no envelope' fact; the rejected array-envelope alternative is only in the plan)`

### bleats --no-follow reads log files, not the bus

`shep bleats --no-follow` reads the sheep's on-disk log files directly rather than pulling from the daemon's event bus.

**Why:** The bus is live-only fan-out with no history/backlog request type. Two alternatives were rejected first: a bounded replay-on-subscribe primitive (rejected because it would redefine Request::Subscribe's semantics for every future consumer, including the lookout TUI and the whistle MCP server, not just bleats) and dropping --no-follow from the phase entirely. Reading files needed one additive wire change (ProcessInfo::out_file/err_file) so a configured explicit log path could be located, and gives strictly more than replay would have, since a log file holds what a sheep wrote before this CLI ever connected.

`docs/writing-plans/plans/2026-08-08-shep-phase3-cli.md:2662, 3101, 3135; verified crates/shep-cli/src/commands/bleats.rs:485-524 (comment states the what; the rejected-alternatives reasoning itself is in the plan, not the code)`

### shep-client test fakes: feature flag, not a separate crate

A standalone `shep-client-testing` crate (publish = false) was built and reverted the same day.

**Why:** shep-client-testing would depend on shep-client while shep-client dev-depends on it back - a dependency cycle. Cargo permits that through dev-dependencies, but it was judged not worth leaving in the tree for the sake of keeping fakes out of the published source. Settled on a `test-support` feature flag mirroring shep-daemon's existing `test-fakes` pattern, so the workspace has one answer to 'how do fakes stay out of the public API' instead of two.

`docs/writing-plans/plans/2026-08-08-shep-phase3-cli.md:3133; verified crates/shep-client/Cargo.toml:14-23 (documents the mirroring; does not mention the reverted crate attempt) and crates/shep-cli/Cargo.toml:258`

### The original readiness-pipe/adopt_fd unsafe mechanism was recommended for outright DELETION, but shipped instead as permanently-present, never-invoked dead code

A dedicated research pass ranked three options for the daemon-boot readiness handshake's one unsafe fd-adoption site: A (widen the forbid-unsafe rule to allow it outside sys.rs), B (a 'safe' wrapper - shown to be UNSOUND, since a safe caller could alias a live fd and close it from under its real owner with no unsafe keyword anywhere), and C (delete the pipe/sys.rs mechanism entirely, since HelloAck already carries a strict superset of what the pipe reports and the poll-connect-with-backoff loop Phase 3 needed to build anyway makes the pipe pure redundancy). Ranking was C > A > B, with B explicitly refused as unsafe-hiding.

**Why:** The actual outcome (recorded in the Phase 2b/3 plan corrections) took neither pure C nor pure A: sys.rs, adopt_fd, and BootOptions::ready_fd all still exist in the codebase today, but the CLI's main() never populates ready_fd (stays None unconditionally) and never calls adopt_fd - readiness is established entirely via the HelloAck handshake instead. This is functionally equivalent to C's argument (the mechanism carries zero production weight) while stopping short of deleting the code, likely because deleting a spec'd protocol and retiring numbered IR rules needed a maintainer sign-off the researcher explicitly flagged as a judgement call outside CLAUDE.md's decision-reservation list.

`docs/research/phase3-readiness-decision.md (research); shipped resolution recorded in docs/writing-plans/plans/2026-08-07-shep-phase2b-daemon-plane.md:2033; verified still true at crates/shep-daemon/src/sys.rs and boot.rs`

## Supervision and lifecycle

### A memory breach self-disarms the id it reports

PollingEnforcer stops reporting an id the tick after it breaches, until the sheep comes back online and re-arms.

**Why:** Without this, the next 15s tick would see the same over-limit tree while the restart from the first breach is still in flight, and the sheep would get restarted twice for one breach.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1162`

### A readiness-probe timeout is a warning, not a spawn failure

On deadline elapse a sheep goes online anyway, with a tracing::warn!, rather than being marked errored.

**Why:** Treating a readiness timeout as failure turns a slow-starting-but-fine app into a restart loop - exactly the failure mode max_restarts exists to contain - and it's the case pm2 users hit constantly. A distinct ProcessEventKind::ReadinessTimeout was considered and rejected only because a wire-additive event kind was out of scope for the phase, not because it's a bad idea.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1473`

### Cron dialect is five-field standard only; @nickname shorthands are expanded by shep before croner ever sees them

shep accepts the vixie @nickname set (@yearly/@monthly/@weekly/@daily/@hourly/@midnight/@annually) via its own pre-parse expansion pass, then hands croner a plain five-field pattern. Widening beyond that (croner's full seconds-field/L/W/# dialect) was deliberately not taken.

**Why:** croner's own handle_nicknames (verified in its source) has no arm for @midnight - an unmatched @token falls through unchanged and fails a field-count check. Delegating expansion to croner would ship an incoherent subset (@daily works, @midnight doesn't, with a confusing error). Expanding first also keeps the mapping shep's own rather than an implementation detail of croner's next version. Narrowing the accepted grammar was chosen over the wider default because widening later is backward-compatible and narrowing is not.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:394`

### Extras carries no daemon-wide shared prober; a shared one was a real, shipped bug

Liveness/readiness probers are built per-instance from the assembled SpawnSpec (spec_prober, renamed from readiness_prober) and handed to arm(), never held on the daemon-wide Extras struct.

**Why:** A shared prober built at boot (with no app in scope) would run every exec probe with no PATH/HOME/USER/LANG/TZ, and - the sharper defect - every instance of a clustered app would probe with the same unexpanded SHEP_INSTANCE, hitting the same port every time. This exact bug shipped once on the readiness path (caught because the first signature couldn't structurally reach `instance`) and was fixed before the liveness path could repeat it.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1941`

### https:// probe targets rejected at config time

ProbeTarget::parse rejects an https:// URL with a dedicated HttpsUnsupported error rather than accepting it and failing at poll time.

**Why:** The hand-rolled HTTP/1.1 prober has no TLS. A probe that silently fails every poll looks identical to an app that's down; failing loudly at shep start (exit 4) instead of at 03:00 is the honest tradeoff. Accepted cost: no HTTPS readiness probes in v1.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:611`

### Liveness/readiness probe interval measured from probe completion, not a fixed tick

The liveness loop does sleep(interval) after the probe resolves, rather than driving probes off a tokio::time::interval.

**Why:** tokio::time::interval's default MissedTickBehavior::Burst makes every tick overdue once a probe outlasts its own interval, so a slow-to-answer app gets probed back-to-back with no gap - the opposite of what a struggling app needs. Measured concretely in a test: an interval-driven loop reaches 7 calls in a span where the sleep-then-probe loop reaches 4.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1266`

### max_cron_sleep is a config knob (default 60s), not a hard const

The cron worker's capped-sleep re-derivation interval lives in [daemon] as an overridable Option<UpDuration> rather than a fixed constant.

**Why:** Sixty seconds bounds the drift a suspended laptop or NTP step can cause and is the right default for someone who hasn't thought about it - but a server with a thousand cron-configured apps that never suspends wants fewer wakeups, and a laptop that suspends hourly wants faster recovery. Making it configurable serves both without picking one number to regret.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:794`

### max_cron_sleep: 60s default, configurable, rejected not clamped below 1s floor

The cron worker's re-derive interval defaults to 60s (DEFAULT_MAX_CRON_SLEEP) but is a daemon-config knob (max_cron_sleep / SHEP_MAX_CRON_SLEEP) with a 1-second floor enforced by rejection, not clamping. No CLI flag and no upper bound.

**Why:** 60s bounds the drift a suspended laptop or NTP step can cost, chosen for someone who hasn't thought about it. Making it configurable serves the person who has. Rejecting below the floor (rather than clamping) matches the project's silent-failure-avoidance pattern; clamping would be quietest in exactly the place it can't be seen, since the daemon runs detached with stderr in shepd.err.log. No CLI flag because `shep daemon` is a hidden re-exec target no person ever types.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:694`

### Memory limit measures the whole process tree, not the root pid

max_memory is enforced against the sum of RSS over a sheep's whole process tree (tree_rss), not just the root pid's own reading.

**Why:** The kill unit spec §4 defines is the process-group tree, so measuring only the root is trivially dodged: a sheep that forks a worker and keeps its own RSS low never breaches while the group holds a gigabyte. This is a deliberate deviation from pm2's single-pid behavior, called out in module docs and migration notes.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:952`

### MEMORY_POLL_INTERVAL tightened from 30s to 15s

The polling memory-limit enforcer samples every 15s, not the 30s an earlier draft had.

**Why:** Spec §14.2 tightened it; sampling is cheap enough (per the workspace's first criterion bench harness) that halving worst-case breach latency costs nothing measurable.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1104`

### NormalizeError's cron variants carry owned Strings, not a wrapped croner::CronError

The new normalize() error variants for bad cron patterns store the offending pattern plus a String rendered from croner's Display, rather than wrapping CronError itself.

**Why:** NormalizeError derives Debug/Clone/PartialEq/Eq and existing tests compare whole values; CronError implements neither Clone nor PartialEq, so wrapping it would force dropping three derives and break six existing assertions in an otherwise-unrelated module. Rendering via Display also gives better user-facing text (the sentence without the type).

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:241`

### notify's macos_fsevent must be re-declared when defaults are off, and fsevent-sys needs a floor pin

notify is added with default-features=false but explicitly re-adds the `macos_fsevent` feature; a transitive `fsevent-sys = "4.1.0"` pin is also required.

**Why:** macos_fsevent is notify's only default feature; dropping defaults without naming it again silently falls back to the slow polling watcher on macOS, turning watch latency into seconds. Separately, notify 8.2.0 under-declares its fsevent-sys floor (asks for 4.0.0 but calls APIs added in 4.1.0), which only shows up under -Z minimal-versions.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1600`

### ProbeTarget is deliberately NOT #[non_exhaustive]

Unlike most public enums in this codebase, the readiness-probe transport enum has no wildcard arm and isn't marked for growth.

**Why:** Its error type grows (rejection modes accumulate) but the transport set itself is closed by design - a fourth probe transport is a spec change, and every match site should be forced to handle it via a compile error (E0004) rather than silently falling through a wildcard. This is the rare case where #[non_exhaustive]'s usual growth-anticipation reasoning is inverted on purpose.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1355`

### Readiness gates the start path only when the app configures it, and a timeout still goes online

An app with no wait_ready and no readiness_probe reaches online immediately on spawn, exactly as before this phase - the Heuristic readiness source exists and is tested but is unreachable from the start path. On a configured readiness deadline elapsing, the sheep goes online anyway with a warning, not errored.

**Why:** This is a deliberate departure from the research note, which had Heuristic gating every start (a 3s regression on every `shep start` in the default config, which nothing in the spec asks for - spec §7 puts the heuristic inside reload's AwaitReady, not the start path). Treating a readiness timeout as a spawn failure would turn a slow-starting-but-fine app into a restart loop, exactly the failure `max_restarts` exists to contain.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1467`

### Watch and cron both restart the WHOLE name-group, including already-stopped instances

A file-change or cron tick restarts every instance sharing a name, not only currently-running ones. An earlier draft scoped watch's restart narrower (running instances only) via new plumbing (a scope parameter, a new Msg variant); that was withdrawn.

**Why:** cron's own behavior was already this-way and the maintainer closed the asymmetry rather than preserve it: what actually keeps a stopped sheep down is disarming (no armed watcher/cron worker reaches it), not a status filter at restart time. An implementer reopening a status filter here is reversing a settled question.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:787`

### Watch and cron both restart the whole name-group, including stopped instances

A triggering file save or cron occurrence restarts every instance of a name via plain ProcessSelector::Name, stopped instances included. An earlier draft built machinery (a scope parameter, a new Msg variant, a restart_running handle method) to skip stopped instances; all of it was withdrawn.

**Why:** Disarming already covers the case anyone actually hits: a fully-stopped sheep has no armed watcher/cron worker at all, so neither trigger reaches it. The filter would only ever matter for a partially-stopped multi-instance group, and its cost was real engine surface added by a phase whose job was bolting subsystems on, not reaching into the actor. It also kept watch and cron symmetric. The user-visible rule that actually protects a stopped sheep is: stopping it disarms its watch.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:1744`

### watch=true with no cwd is rejected at config time

normalize() rejects watch=true when the app sets no cwd (NormalizeError::WatchWithoutCwd), rather than defaulting the watch root or silently arming nothing.

**Why:** Two alternatives were considered and rejected: defaulting to the daemon's own cwd is dangerous (nothing in the workspace chdirs, so under systemd with no WorkingDirectory= that default is `/`, arming a recursive watch over the whole filesystem); arming nothing with a warning is the same silent-failure shape the https rejection avoids. A watch root must come from the Flockfile.

`docs/writing-plans/plans/2026-08-08-shep-phase4-lifecycle.md:635`

## The log plane

### flush truncates AFTER flushing pending writes, not before

shep flush's log-clearing sequence flushes buffered writes to disk first, then truncates.

**Why:** tokio::fs::File genuinely holds unwritten bytes in its buffer; truncating first would let an already-dispatched write land at offset 0 immediately after the truncate, reappearing in the "empty" file.

`docs/writing-plans/plans/2026-08-09-shep-phase5-log-plane.md:290`

### flush truncates the recorded path, after flushing pending writes

shep flush truncates ProcessEntry::out_file/err_file (the paths the actor holds), never the pump's current inode, and only after routing a flush through the log-control channel first.

**Why:** tokio::fs::File genuinely buffers: a write already dispatched to the blocking pool can land at offset 0 immediately after a bare truncate if flush-then-truncate isn't ordered. And truncating by current inode rather than by path would truncate a rotator's freshly-renamed archive instead of the live file if run right after an external rename.

`docs/writing-plans/plans/2026-08-09-shep-phase5-log-plane.md:290`

### reopen uses a push channel with a synchronous ack, not a generation counter

LogCtl::Reopen carries a oneshot::Sender the pump signals after it has dropped its old handle and reopened the path; the supervisor's reopen command awaits every matched pump's oneshot before replying.

**Why:** The maintainer chose this over a generation-counter design specifically for the synchronous guarantee a logrotate postrotate stanza needs: after the RPC reply returns, every live pump provably holds the new inode. A counter could only promise 'before the next write' - and a quiet sheep with no next write would never actually reopen.

`docs/writing-plans/plans/2026-08-09-shep-phase5-log-plane.md:229`

### reopen uses a push channel with a synchronous reply, not a generation counter

shep reopen replies only after every live log pump provably holds the new file inode.

**Why:** A generation counter could only ever promise "before the next write" and a quiet sheep (no writes happening) would never reopen at all under that design - the push design gives logrotate's postrotate stanza the guarantee it actually needs.

`docs/writing-plans/plans/2026-08-09-shep-phase5-log-plane.md:229`

### reuse_port doc was wrong; macOS SO_REUSEPORT does not load-balance

AppConfig::reuse_port's doc previously claimed shep 'binds' listen sockets with SO_REUSEPORT (first person). Corrected: shep binds nothing - reuse_port is the operator asserting the app itself sets the socket option before it binds. The spec also gained a macOS caveat.

**Why:** Measured directly: macOS's SO_REUSEPORT is last-binder-wins (40/40 connections to the newest binder over 40 tries), while Linux actually load-balances (20/20 split). This means the same setting makes reload strictly better on macOS (the replacement gets 100% of new traffic immediately) while making intentional clustering not work as expected there. A mismatched pair (one process sets the option, one doesn't) gets EADDRINUSE on both platforms, undetectable by shep in advance.

`docs/writing-plans/plans/2026-08-09-shep-phase5-log-plane.md:325`

### tracing-subscriber installs only in run_daemon, never in boot()

The daemon's tracing subscriber is initialized in the CLI's run_daemon entry point, not inside shep-daemon's boot().

**Why:** tracing_subscriber::fmt::init() panics on a second install in the same process. boot() is called repeatedly across ~15 unit and e2e-fixture tests in one test binary, so installing there would fail every test after the first. run_daemon's only caller is main, and each real invocation is a fresh process.

`docs/writing-plans/plans/2026-08-09-shep-phase5-log-plane.md:180`

## Reload

### A planned 'muster-roll double-counting' fix was dropped after its premise was audited and found false

A task to make is_running exclude Stopping (to stop a mid-reload daemon-reboot from resurrecting an inflated instance count) was written, then dropped once investigated: is_running never counted Stopping in the first place, and instances_running is only ever read as a boolean gate on restore, never as a count.

**Why:** Recorded specifically so nobody re-litigates it: the actual, smaller issue that survives is a cosmetic one (a muster-roll snapshot taken in the few-hundred-ms window where both entries pass is_running mid-reload can show a transient inflated count) - free to avoid by ordering the Stopping marker no later than the replacement's spawn, but it was never a correctness bug worth its own task.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:110`

### Drainee status reuses ProcStatus::Stopping rather than adding a new variant

Reload marks the outgoing instance ProcStatus::Stopping (already on the wire, previously set by nothing) instead of adding a Draining variant.

**Why:** ProcStatus is not #[non_exhaustive], so a new variant would be a wire and API break. Reusing Stopping was free and turned out to have an existing guard already rejecting it as a restart target (handle_extra_restart), which became the mechanism that closes the liveness-restart race for free.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:30`

### Reload does not re-read config - **superseded**

A reload reuses the stored ResolvedApp and the credentials resolved at the sheep's original Start, rather than re-parsing the Flockfile.

**Why:** ProcessEntry::credentials is documented as resolved once-only; re-reading would collide with that contract and would also change reload's argument shape into something closer to a distinct 'apply new config' verb, which was deliberately treated as a different feature.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:74`. Replaced by: half of it, and the half that held is the half that mattered. A reload still parses no file. The credentials clause gained one deliberate exception on 2026-09-03: a reload that promotes a config parked by a Flockfile load resets `credentials` to `Unresolved` when `user` or `group` is among the promoted fields, and only then. That distinct 'apply new config' verb was in fact built, as `Request::ApplyConfig` rather than as a change to reload, so the entry's own reasoning about scope is intact; what moved is the once-only rule, narrowly, so an operator who changed identity gets the identity they asked for while every other config change still costs no passwd lookup. Verified crates/shep-daemon/src/supervisor.rs (the promotion path) and crates/shep-daemon/src/entry.rs (`ProcessEntry::credentials`).

### Reload does not re-read config from disk - **superseded**

A reload reuses the already-stored ResolvedApp and the credentials resolved at the sheep's original Start, rather than re-parsing the Flockfile.

**Why:** Re-reading would collide with ProcessEntry::credentials' documented once-only resolution rule, and changing that contract mid-reload is really a different feature (a config-reloading verb) that wasn't in scope.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:68`. Replaced by: the same narrow exception the entry above records, for the same reason. The two entries are near-duplicates extracted from two plans, and they were superseded together rather than one of them being quietly left standing.

### Reload provides an overlap, not zero downtime - and the reason is the accept backlog

Spec framing and every doc/changelog for reload must say 'overlap in which the app can achieve zero downtime,' never unqualified 'zero-downtime.'

**Why:** Measured on both platforms: when the old listener closes, its accept backlog is reset (Linux RESET, macOS EPIPE) - every connection queued but not yet accepted dies with it. A reload is downtime-free only insofar as the app itself stops accepting, drains, and exits inside graceful_timeout; an app that ignores SIGTERM until shep's SIGKILL drops its whole backlog every time, and nothing shep does prevents that.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:15`

### Reload replies early and reports real progress on the bus, not synchronously

handle_reload returns its RPC reply before the swap finishes, with new ProcessEventKind variants carrying progress over the bus.

**Why:** A reload of N instances costs roughly N*(listen_timeout+graceful_timeout) which for six instances alone exceeds MAX_DEADLINE_MS (60s) - a synchronous reply structurally cannot cover it.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:221`

### Reload's connection-drop assertion is platform-split: Linux only for dropped connections, macOS for a weaker property

The e2e test asserting reload can drop in-flight connections during the swap runs its hard count assertion only on Linux; macOS asserts only that the reload completes, the new instance serves, and the drainee is reaped.

**Why:** Linux keeps feeding the old listener's backlog until it's closed, so a reload CAN observably drop connections there; macOS's socket semantics mean the bug literally cannot manifest, so a single shared assertion would be either vacuous on macOS or flaky under Linux's real timing.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:236`

### Reload's drainee status reuses ProcStatus::Stopping rather than adding a Draining variant

The existing (already-wire-shipped) Stopping status is given its first writer for reload's outgoing instance, instead of introducing a new enum variant.

**Why:** ProcStatus is NOT #[non_exhaustive], so adding a variant would be both a wire break and an API break; reusing an existing status that already exists and had zero writers closes two other findings for free (Tasks 3 and 4 of that plan).

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:30`

### Reload's live-connection proof is platform-split by design, not by convenience

The e2e test asserting reload doesn't drop live connections asserts a connection-error count on Linux only; on macOS it asserts only the weaker property that the reload completes and the drainee is reaped.

**Why:** The two platforms don't share the mechanism the count is about: Linux's SO_REUSEPORT load-balances across the whole group, so the outgoing instance keeps taking a share of new connections right up until it closes (measured ~47/48 split) - that's where a reload can actually drop something. macOS is last-binder-wins, so the drainee stops getting new connections the instant its replacement is up, and the interesting failure mode simply cannot occur there. A single shared assertion would be vacuous on macOS or flaky on Linux.

`docs/writing-plans/plans/2026-08-10-shep-phase6-reload.md:236`

## Custom actions and the shepherd channel

### A successful to_child.send() is not proof a trigger was delivered

No test may treat Ok(()) from to_child.send() as evidence the action reached the child; only the ActionReply coming back is treated as proof of delivery.

**Why:** Measured directly: the first send to a channel whose child has already died still returns Ok(()) and the message vanishes - only the second send actually errors. This is why trigger's whole design routes through a reply-or-timeout waiter rather than trusting the send call.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:289`

### Action params were added to the fd-3 wire ahead of the written spec, as a one-time window

ShepherdMessage::Action gained an Option<String> params field even though spec §9 as written only had `trigger <target> <action>` with no params, with skip_serializing_if keeping the no-params wire form byte-identical.

**Why:** The shepherd channel has no version field - any later change to its strings is a silent break for every deployed app and the (not-yet-built) @shep/io shim. The maintainer approved adding it now specifically because zero apps were deployed and no shim existed yet, making the field free today and a breaking change if deferred.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:147`

### Actor-to-child reach for trigger: cloned to_child sender, not a new SheepCtl variant

The actor reaches a running child by holding a clone of ProcIo::to_child on SheepSlot, rather than adding a new SheepCtl message variant routed through the existing ladder channel.

**Why:** SHEEP_CTL_CAPACITY is only 4 and try_send never awaits; claim_manual's logic that a Full channel is safe to ignore rests on the assumption that a queued item can only be a Kill (meaning the ladder is already running). Anything else occupying those 4 slots could make that assumption false and silently drop a real Kill.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:127`

### Custom actions answer on completion, not on acceptance

shep trigger's RPC reply waits for the app's own reply, rather than confirming the daemon merely dispatched the message.

**Why:** An action has no structural time floor the way a reload's N-instance timing does, which is what makes the daemon-side timeout (a new AppConfig::action_timeout field) necessary in the first place rather than optional.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:80`

### Custom actions emit no bus events in v1

Neither dispatching a trigger nor receiving its reply produces a ProcessEventKind on the shared bus.

**Why:** Every existing ProcessEventKind is a lifecycle state transition and a trigger changes none - the sheep is Online before and after. reopen and flush are already bus-silent for the same reason (verified: zero self.emit calls in either handler). An audit trail, if wanted later, belongs on a separate daemon.command topic covering every verb, not bolted onto trigger alone.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:80`

### Custom-action params ship on the wire now, while zero apps are deployed

ShepherdMessage::Action gains an Option<String> params field immediately, rather than waiting for a real consumer, specifically because the field is cost-free today and would be a breaking wire change once any real app existed.

**Why:** The maintainer weighed "add it while it's free" against "YAGNI" and chose the former given the wire-compat cost of adding it later; skip_serializing_if keeps the params-less case byte-identical to the pre-existing spec.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:80`

### fd 3 (the shepherd channel) shipped non-blocking, a real bug affecting a shipped feature

Every child's fd 3 was inherited non-blocking from tokio::net::UnixStream::pair() and never cleared before being mapped to fd 3 in the child. Fixed by calling set_nonblocking(false) on the std child end before handoff.

**Why:** A child doing a plain blocking read on fd 3 got EAGAIN (measured with /bin/sh: 'read error: 0: Resource temporarily unavailable'). This wasn't just a Phase 7 prerequisite - shutdown_with_message had already shipped and was broken the same way for any app using a blocking read; nobody noticed because Node/Go set non-blocking themselves. Shipped as its own revertable/backportable commit ahead of the trigger feature.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:21`

### Trigger registration and action-name validation both deliberately avoid a second source of truth

Whether a sheep can be triggered is answered by reading to_child.is_closed() directly, never from a config flag; action names are completely free-form with no declaration list on AppConfig and no daemon-side validation.

**Why:** Both choices were made specifically to avoid adding a new fd-3 wire string or a second copy of a fact the channel itself already knows. An unknown action name is documented as the app's problem (it should reply anyway) rather than enforced, because enforcing it would need a wire change.

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:74`

### Trigger's waiting model is a dedicated waiter, not the existing PendingReply/aggregation shape

Trigger builds a new waiter on spawn_readiness_task/await_ready's shape (single message back carrying the outcome), rather than reusing PendingReply as-is.

**Why:** PendingReply has no timeout, because every existing command that uses it is backed by the kill ladder guaranteeing an eventual Msg::Exited. A custom action guarantees nothing back from the app, so a PendingReply-shaped trigger against an unresponsive app would leak an entry forever and park the caller. The staleness stamp reuses reload's reasoning (the new id, not a generation counter, since ids are never reused).

`docs/writing-plans/plans/2026-08-11-shep-phase7-custom-actions.md:192`

## The pm2 cutover

### A failed sd_notify does not kill the daemon

If the READY=1 datagram fails to send, the daemon logs tracing::warn! and continues booting rather than aborting.

**Why:** The daemon is fully functional regardless; only systemd's knowledge of readiness is affected, and systemd's own TimeoutStartSec is the honest way to surface that - killing a working daemon over a failed datagram would be strictly worse.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:1863`

### CPU baseline sampling happens only on the periodic tick, never on an on-demand read

The one-per-15s sampling loop is the sole writer of the CPU baseline used for percentage calculation; a `shep describe` invoked between ticks reads the existing baseline rather than triggering a fresh sample.

**Why:** Two flock() calls a moment apart would otherwise divide by a near-zero time window, producing a wildly wrong percentage; bounding the window to the fixed tick interval keeps it away from zero. A process with no baseline yet (spawned since the last tick) reports `-` rather than inventing a number from a sub-millisecond window.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:109`

### CPU/memory sampling splits from limit enforcement inside one tick, on-demand reads run under spawn_blocking

PollingEnforcer's existing 15s tick now also records a CPU/memory baseline for every sheep with a pid (not just armed ones), via the same TreeIndex it already builds. An on-demand sample for ListFlock/Describe runs in the RPC layer, not the actor, under tokio::task::spawn_blocking. A sheep with no baseline yet reports '-' for CPU rather than a computed value.

**Why:** SysinfoSampler::sample is a measured 5.77ms blocking syscall walk (benches/benches/memory_sample.rs, 883 processes) - a second polling loop would double that cost for nothing, the actor must never block, and a tokio worker thread shouldn't block either. The baseline is written only by the periodic tick (never by an on-demand read) because two flock(2) calls a moment apart would otherwise divide by a near-zero window; a fresh process with no baseline gets an honest '-' rather than an invented number from a sub-15s window.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:105-108, 2558, 2795`

### import writes a Flockfile and starts nothing; reads pm2's dump.pm2 only, never ecosystem.config.js overlays

shep import is read-only against pm2's saved process dump and produces a Flockfile the operator then runs `shep start` against; it does not merge or read the pm2 ecosystem file at all.

**Why:** Reading an ecosystem.js overlay faithfully would mean evaluating JavaScript, which conflicts with import's clean-room, no-code-execution posture.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:94`

### import's env mapping is declared-only, by construction; unmapped keys are named, not written

The imported env is the union of pm2's env_<name> maps; any key that's neither declared there, a known session-junk pattern, nor a pm2-injected variable is reported in the output and deliberately not written into the resulting Flockfile.

**Why:** The operator gets to decide with the evidence in front of them rather than shep guessing at intent for an unrecognized key.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:96`

### Import: declared env is the union of env_<name> maps, not env

A key is written only if it appears in some env_<name> map (by construction, only ecosystem-declared keys). A key present in env but not declared is checked against two closed lists (session-shell junk, pm2-injected) and dropped silently only if it matches; anything else is named in the output and never written.

**Why:** pm2 flattens a login shell into env but never into env_<name>; this makes the importer safe by construction rather than a guessed heuristic - an incomplete injected-key list costs one extra output line, never a silently wrong config. PATH is deliberately treated as session junk because the unit file, not the app's Flockfile, is where PATH must live for interpreter lookup to survive a reboot.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:1400`

### NODE_APP_INSTANCE becomes increment_var, never a literal env value

The importer maps pm2's NODE_APP_INSTANCE env key to AppConfig's increment_var mechanism instead of copying its literal value into env.

**Why:** The dump only records instance 0's value; copying it verbatim would pin every instance to instance 0.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:92`

### One restore path shared by boot and muster

boot::restore_flock and the Muster RPC handler call the same function in snapshot.rs rather than each having their own read-roll/validate/start logic.

**Why:** Two copies of 'read the roll, re-validate, record, start' drift, and the one that drifts is the one nobody reboots to test.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:87`

### pm2 dump import: flat row over serde flatten

shep import reads dump.pm2.json rows via plain serde_json::Value field lookup, not #[serde(flatten)], after measuring a real dump on 2026-08-12: no pm2_env wrapper, all config fields sit at the row's top level next to a splatted session environment, and env duplicates those same keys.

**Why:** flatten needs a catch-all map for env_<name> keys and interacts badly with a row that also carries the whole process environment as sibling string keys; readability wins for a handful of rows per dump.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:994`

### ProcessInfo loses Eq, keeps PartialEq

Adding cpu_percent: Option<f32> forced dropping the derived Eq impl (f32 has no total order); PartialEq remains.

**Why:** Nothing in the workspace required Eq (verified by grep), and PartialEq is enough for every assert_eq!; a fixed-point integer workaround was explicitly rejected because it pushes a formatting decision onto every reader.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:108, 2789`

### ProcessInfo::cpu_percent/memory_bytes only sampled on ListFlock/Describe, not on every mutating reply

Started/Stopped/Restarted/Reloading/Reopened/Flushed all carry ProcessInfo too but are never given a live stats sample.

**Why:** None of those replies is a place an operator reads resource usage; paying the ~5.77ms process-table walk on every stop would cost everyone for nobody's benefit.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:2827`

### Readiness fires at the end of boot(), after the muster restore

sd_notify's READY=1 is sent as the last step of boot(), after restore_flock completes and the RpcContext is assembled - not at process exec time (Type=simple semantics).

**Why:** The whole point of choosing Type=notify is that the unit only goes green once the flock actually exists; notifying earlier would report a healthy service that is still restoring, turning a hung restore into a silently-green unit rather than a failed start with a useful timeout.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:1861`

### sd_notify fires only after the muster restore completes, at the very end of boot()

The systemd READY=1 notification is deliberately the last thing boot() does, after the flock has been restored, not right after socket bind.

**Why:** That's the entire point of Type=notify: the unit only goes "green" once the flock genuinely exists, so a hung restore surfaces as a failed unit start rather than a green unit silently supervising nothing.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:103`

### shep startup's ExecStart runs the daemon under Type=notify, not `shep muster`

The generated systemd unit's ExecStart is the daemon binary itself; the flock-restore effect (equivalent to `shep muster`) happens because the daemon already restores its roll at boot, and the unit's prose describes that effect rather than a literal invoked command.

**Why:** Under Type=notify, systemd supervises whatever process it started - if that process were a short-lived `shep muster` invocation, systemd would have nothing left to supervise once it exits.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:102`

### startup installs system-level units only, not user units

shep startup writes a systemd system unit or launchd LaunchDaemon, never a user unit.

**Why:** A user unit would trade one root step for loginctl enable-linger plus a new failure mode where the flock silently fails to come back after reboot.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:114`

### startup's three-gate privilege/home resolution

shep startup resolves the target user (--user, else $SUDO_USER, else invoking user), then that target user's own passwd home + ".shep" (never this process's $HOME), then requires geteuid()==0 to actually install; otherwise it prints the exact sudo command and exits non-zero. A resolved $SHEP_HOME that does not exist is also refused.

**Why:** Under sudo, $HOME is root's, so naively using it would silently point the generated unit at /root/.shep and restore an empty flock after every reboot - a failure that surfaces months later with no explanation. shep also never self-escalates.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:107 (decisions 18-19), 2345`

### systemd readiness (sd_notify) needs no dependency and no unsafe

Readiness notification to $NOTIFY_SOCKET is implemented with plain std: UnixDatagram for a filesystem path, and std::os::linux::net::SocketAddrExt::from_abstract_name (stable since 1.70, under the 1.88 MSRV) for an abstract-namespace address.

**Why:** Both socket shapes are reachable from stable std alone, so no crate and no unsafe block were needed for systemd Type=notify support.

`docs/writing-plans/plans/2026-08-12-shep-phase8-cutover.md:1856`

## Dogs

### A dog config change requires disable+enable; no live push

A dog reads its [dog.<name>] section exactly once, at connect. There is no mechanism to push a config change to an already-running dog.

**Why:** Deliberately deferred as a v1.1 question rather than built now; documented in docs/dogs.md.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:106`

### A dog is a marker on the existing process entry, not a second registry

ProcessEntry gained dog: Option<DogSource>, carried onto the wire via ProcessInfo, rather than dogs living in a parallel supervision structure.

**Why:** Duplicating supervision would mean teaching reload, watch, cron, limits, the log plane and the muster roll about a second population of processes.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:94`

### A dog is a marker on the existing ProcessEntry, not a second process registry

ProcessEntry gains dog: Option<DogSource>; ProcessInfo carries it onto the wire, and every existing subsystem (reload, watch, cron, limits, logs, muster) is unmodified rather than duplicated for a second population.

**Why:** A separate dog registry would require teaching every one of those subsystems about a second kind of managed thing, doubling maintenance surface for no behavioral gain. A tripwire is written into the design itself: a `dog` branch answering "where did this come from / who should see this" is expected and fine; a branch answering "how is this supervised" (a different kill ladder, backoff, or Errored meaning) is a warning sign that the separate registry SHOULD have been built after all.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:95`

### A reload's own deadline is exposed per-instance on ProcessInfo, not as a sibling field on Response::Reloading - *unverified*

An external ledger recorded the maintainer's decision as "the reload deadline rides the reload response, as an additive field, no PROTOCOL_VERSION bump needed" - re-deriving from the actual wire shape found that premise WRONG and switched the design to a new ProcessInfo::reload_deadline_ms: Option<u64> field instead, still additive, still version 1.

**Why:** Response::Reloading(Vec<ProcessInfo>) is a tuple variant under #[serde(tag="kind", content="data")]; giving it a sibling field would turn `data` from a JSON array into an object, which shep's own documented wire-evolution rule classifies as a retype requiring a PROTOCOL_VERSION bump - and since the handshake compares versions for strict equality, that would stop every published client talking to every published daemon over one advisory number. Putting the deadline on ProcessInfo instead is strictly better, not merely a workaround: it's computed per-replacement-instance from that instance's own registered listen_timeout+graceful_timeout+slack (exactly what arm_reload_deadline already computes internally), so it hands a dog the real per-instance number rather than one it would otherwise have to infer from a possibly-stale Flockfile copy, and it closes the instance-counting gap too since one field appears per ProcessInfo already returned.

`docs/writing-plans/plans/2026-08-27-dog-prerequisites.md:1261 (NOT yet shipped - no reload_deadline_ms field exists on ProcessInfo in the current tree)`

### A running dog does not see a config change until disable+enable

A dog reads its [dog.<name>] section exactly once, at connect time; there is no live-push mechanism.

**Why:** Live config push was explicitly deferred as a v1.1 question rather than solved now - shep set/get already exists, and re-reading via disable+enable is a cheap, understood workaround in the meantime.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:106`

### A supervised dog's on-remove hook uses tokio::process with concurrent stdout/stderr draining, not std::process + a poll loop - *unverified*

The planned hook runner spawns a dog binary with `on-remove` as its sole argv and awaits it under a timeout via tokio::process::Command::output(), rather than following vet_binary's existing std::process + try_wait poll pattern.

**Why:** vet_binary can get away with std::process because it nulls all three stdio handles and never reads a byte, so it can't deadlock; the hook runner DOES read the dog's output, and a double that returns canned output can't reveal that a real child writing more than one pipe buffer with no concurrent reader would simply hang forever until killed at the budget, silently losing its own output. tokio::process::Command::output() under a timeout drains both pipes concurrently, which is the actual problem being solved; doing the equivalent with std::process would need two reader threads or a temp file. A dog refusing the hook's unknown argument (the ordinary case, since every dog that exists today predates this hook and shep-log-rotate is a real example) is deliberately modeled as HookOutcome::Refused, not a failure.

`docs/writing-plans/plans/2026-08-27-dog-prerequisites.md:334 (NOT yet shipped - crates/shep-cli/src/commands/hook.rs does not exist in the current tree)`

### adopt is a distinct verb from enable --exec, kept only as a hidden alias

shep adopt <name> <path> (both positional, required) registers a third-party binary under [daemon] adopted_dogs, separate from a built-in dog. enable --exec survives only as a hidden compatibility alias.

**Why:** Turning on a dog already shipped in the binary and vetting a binary shep has never seen are different acts with different failure modes (missing path, non-executable, wrong architecture); conflating them into one flag would blur that. The adopted binary's path lives in [daemon].adopted_dogs rather than inside [dog.<name>] because that per-dog section is the dog's own opaque config and a shep-owned key there could collide with a third-party dog's own schema.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:102-104`

### adopt is deliberately not `enable --exec`

Turning on a first-party dog that already ships inside the binary and vetting a third-party binary shep has never seen are kept as separate verbs with separate failure modes (a missing path, wrong architecture, non-executable file). `enable --exec` survives only as a hidden alias for pm2-muscle-memory arrivals.

**Why:** Conflating the two would blur genuinely different risk profiles under one command.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:103`

### All listings sort by (name, instance, id), applied once in snapshot_all

The actor's snapshot_all is the single sort point used by every reply and consumer (CLI, metrics dog, bark reconciliation).

**Why:** Sorting by id scatters a clustered app's instances across the listing; sorting by name groups them, with instance keeping a cluster's slots in order and id breaking ties from a reload's fresh id.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:110`

### Bark reads the daemon's own restarts count, never tallies bus events itself

Restart-loop detection uses ProcessInfo.restarts directly rather than counting restart events observed on the bus.

**Why:** A private tally kept by bark could drift from the number the supervisor actually acts on, which would tell the operator a different story than the one the daemon believes.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:115`

### Bark supports http:// sinks, not just https://

The hand-rolled client's URL parser accepts both http:// and https:// schemes rather than refusing plaintext outright.

**Why:** Sink::Json can point at any operator-configured endpoint, including an internal alerting sink with no TLS in front of it; rejecting http:// would force either a TLS-terminating test harness the tests don't otherwise need, or a second TLS-only code path for Discord/Slack that the test suite never exercises.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:2790`

### Bark's dependency ships unconditionally in every shep binary, by design

tokio-rustls/webpki-roots are compiled into the one distributed shep binary for every user, not behind a cargo feature flag.

**Why:** Per an earlier maintainer ruling (decision-briefs.md §3b): cargo features are for build-slimming source builds, not runtime pluggability - a feature-flagged dog is a weaker dog (no crash isolation, no independent restart) and the release binary is one binary anyway. Runtime opt-in is `shep enable bark`; the process model does the job a feature flag would do worse.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:2765`

### Bark's HTTP client is hand-rolled over tokio-rustls, not reqwest

Discord/Slack/generic-JSON webhook delivery is a from-scratch HTTP/1.1 client (request framing, status-line read) layered over tokio-rustls (ring provider, tls12 named explicitly) + webpki-roots, rather than pulling in reqwest.

**Why:** Measured against the workspace's existing 196 crates: reqwest's default rustls feature costs +93 crates plus a C build dependency (aws-lc-sys+cmake); reqwest's rustls-no-provider+ring path still costs +76; tokio-rustls+webpki-roots costs +10 with no C toolchain needed. Reasoning beyond the number: rustls covers the one part (TLS handshake/record layer) that must not be hand-rolled; the same plan already hand-rolls an HTTP *server* for the metrics dog rather than pulling axum/hyper, so this is the same trade made consistently; bark's actual needs are small (no redirects, no pooling, no HTTP/2, no cookies); and avoiding a C toolchain matters for something operators build on servers. The cost is explicitly recorded too: this diverges from the maintainer's own reqwest standardization elsewhere, and it's now code she owns and must find her own bugs in. default-features=false on tokio-rustls is load-bearing (its defaults otherwise pull aws_lc_rs back in).

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:100 (decision 19), 2718-2762; confirmed present in CLAUDE.md's own description of the ruling`

### Bark's loop subscribes to the bus for speed and polls for correctness

run_loop drives off a live tokio::sync::broadcast subscription for fast reaction, but also polls the flock on an interval; a reported dropped-frame count (broadcast lag) triggers an immediate out-of-cycle poll rather than waiting for the next interval.

**Why:** tokio::sync::broadcast drops events for a lagging subscriber rather than queuing them - cosmetic for shep bleats, but a missed page for alerting. The moment a drop is reported is exactly the moment correctness is in doubt, so that's when the reconciling poll needs to fire immediately.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:3162-3213`

### Bark's TLS client is hand-rolled over tokio-rustls, not reqwest

Discord/Slack webhook delivery uses a hand-rolled HTTP/1.1 client over tokio-rustls rather than the reqwest crate the maintainer standardizes on elsewhere.

**Why:** Measured against the existing 196-crate dependency graph: reqwest's default rustls feature costs +93 crates and a C build dependency (aws-lc-sys/cmake); even its rustls-no-provider variant costs +76. tokio-rustls+webpki-roots named directly costs +10 and was already present via existing TLS needs. rustls itself (the one part that must not be hand-rolled - the handshake and record layer) is kept; everything past it (redirects, pooling, HTTP/2, cookies, multipart) is genuinely unneeded for a webhook POST, and the codebase already makes the identical trade for serve's own HTTP server. The cost is recorded honestly too: this diverges from the maintainer's own cross-project reqwest standardization and it's now code she owns outright, bugs and all.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:113`

### Bark's TLS test coverage gap is accepted, not closed - *unverified*

The task's local test server is plaintext-only (tokio::net::TcpListener); every test exercises the http:// branch, and none drives tls_connector()/ClientConfig/the webpki-roots store.

**Why:** Closing the gap would need the test harness to terminate TLS itself (a self-signed cert, a way to make bark trust it) - a second dependency shape just for tests. The TLS handshake/record layer is rustls's own tested surface, not bark's; explicitly recorded as a known accepted gap rather than silently never-run coverage.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:2845`

### barks.jsonl's bounded ring lives in shep-core, shared by both writers

The size-capped barks.jsonl append/eviction logic lives once in shep-core because both the daemon (recording a dog that gave up) and the bark dog (recording a fired alert) append to it, and shep barks reads it.

**Why:** One implementation of the cap; two independent implementations would evict differently.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:117`

### Dog config travels over the socket, never the environment

A dog inherits only SHEP_HOME, connects, handshakes, and requests its own [dog.<name>] config via Request::DogConfig; the daemon replies with the raw TOML table text for that dog to parse itself.

**Why:** Secrets: bark's sinks are webhook URLs, and env vars are readable from the process table, inherited by every child, and captured into crash dumps. Serving the opaque rendered TOML (rather than a typed shep structure) also means a third-party dog only binds to the shape of its own section, not to shep's internal config model.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:97-98`

### Dog configuration travels over the socket, never the environment

A dog inherits exactly one env variable (SHEP_HOME); it connects, handshakes, and sends Request::DogConfig{name} to receive its [dog.<name>] table rendered back as opaque TOML text.

**Why:** The environment is readable from the process table, inherited by every child process, and captured whole into crash dumps - unacceptable for bark's webhook-URL-shaped secrets. The reply is deliberately opaque (raw TOML, not a typed shep structure) so a third-party dog binds only to the shape of its own config section, never to shep's internal config model.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:98`

### Exit codes 12/13 are reserved for third-party dogs, not defined as new ExitCode variants - *unverified*

The plan deliberately departs from an external ledger's wording ("shep's exit.rs owes rows for 12 and 13"): shep's own ExitCode enum gains no new variant, only a const DOG_RESERVED_FROM=12 and a test pinning every existing exit code below it.

**Why:** ExitCode is the taxonomy of exits shep itself produces, and shep has no code path meaning either 12 or 13 - adding unused variants would be dead surface a completeness test would then have to enumerate. The real risk is a COLLISION: `shep <dogname> [args]` passes an adopted dog's own exit code straight through verbatim, so `shep deploy web` genuinely can exit 12 on an operator's terminal as the dog speaking, not shep - reserving the range (rather than assigning it meaning) is what keeps that passthrough and any future shep-owned code from becoming indistinguishable at the one place anyone reads them.

`docs/writing-plans/plans/2026-08-27-dog-prerequisites.md:1071 (NOT yet shipped - crates/shep-cli/src/exit.rs tops out at FlockEmpty=11 in the current tree)`

### Flock listings sort by (name, instance, id), applied once centrally

Sorting happens exactly once, inside the actor's snapshot_all, so every consumer (CLI, metrics dog, bark's reconciliation) sees one consistent order.

**Why:** Sorting by id alone scatters a clustered app's instances across the listing; sorting by name groups them, with instance keeping a cluster's slots in order and id only breaking ties a reload's fresh id creates.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:116`

### lookout-tui.md's research put the reconnect state machine (Disconnected/Reconnected transitions) in shep-client as a cross-phase client-API requirement; it stayed local to lookout instead, and is still-open debt - *unverified*

D4 of the research argued shep-client's stream type must wrap connection-state transitions itself (an EventSub yielding ClientEvent::{Bus, Disconnected, Reconnected}), flagging that a bare Stream<Item=BusEvent> would block lookout's reconnect UX entirely.

**Why:** lookout instead built its own local Shepherd trait (lookout/source.rs) to own reconnect+resubscribe together, without any change to shep-client. This is not merely superseded but explicitly named as still-open technical debt as of 2026-08-27's dog-prerequisites plan (Task 8): shep-client was expected to get a narrower reconnect() API (Client::reconnect/reconnect_within returning Reconnected::{SameDaemon,NewDaemon}) built for lookout/whistle's benefit. That API never shipped. What shipped instead, in phase 3 on 2026-08-31, is `ReconnectingClient` (shep-client/src/reconnect.rs), a distinct wrapper type rather than a mode on Client, so a one-shot CLI verb cannot silently acquire retry semantics. It exists for dogs crossing a daemon handover rather than for lookout. Either way, lookout's own ladder (250ms x2 capped at 4s) deliberately still does NOT converge onto that shared schedule (100ms x1.5 capped at 5s) or onto the new API, because its 5-attempt bound exists specifically to reach a 'frozen' UI state a plain backoff has no concept of.

`docs/research/lookout-tui.md:117-129 (partially addressed by docs/writing-plans/plans/2026-08-27-dog-prerequisites.md:2124, itself not yet shipped as of the current tree - grep for Client::reconnect finds nothing)`

### No `--all` flag anywhere in flock/dogs listings

The design spec's own prose and its own rendered sample contradicted each other (prose said --all would widen listings to include stopped entries; the sample already showed a stopped sheep by default). The ruling fixed the prose, not the sample: stopped entries were always visible by default and a flag that could only ever widen an already-unfiltered listing would do nothing.

**Why:** A flag with no observable effect is worse than no flag; the two-table split (sheep table + dogs table, both default-visible) already achieves the decluttering --all was meant to provide.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:109`

### No dog watches another dog; a dead dog is recorded at the supervisor edge

When a dog dies, the daemon's own bus watcher (at the edge of the supervisor, not a branch inside handle_exited) records it, and the metrics dog exposes that fact; no dog cross-monitors another.

**Why:** Two dogs observing each other adds a failure mode without adding a genuinely independent observer, and fails hardest exactly when both go down together.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:116`

### rehome should keep [dog.<name>]'s operator-written settings, forgetting only the adoption - *unverified*

Currently rehome_dog deletes the dog's config table along with removing it from enabled_dogs/adopted_dogs; the maintainer approved (2026-08-26) changing it to forget only the adoption, leaving the [dog.<name>] table (including comments, since it's edited via toml_edit) untouched - so re-adopting the same dog finds its old configuration waiting rather than a blank table.

**Why:** disable_dog's own doc already makes this exact argument for `disable` ("an operator who disables a dog to restart it must not lose the configuration they wrote for it"); the only reason rehome hadn't followed it is that "forget the dog entirely" was read as covering the operator's own file too. The two verbs would still differ meaningfully: disable leaves the binary path in adopted_dogs (so the next enable brings it straight back); rehome forgets that, so recovery needs a fresh `shep adopt <path>`.

`docs/writing-plans/plans/2026-08-27-dog-prerequisites.md:91 (NOT yet shipped - crates/shep-cli/src/commands/shep_toml.rs:383-385 still deletes the [dog.<name>] table as of the current tree)`

### Restart-loop detection is two independent rule kinds

"The daemon gave up" (keyed to restart-budget exhaustion) is always on and untunable; a separate early-warning rule ("N restarts in M seconds") is opt-in.

**Why:** The budget-exhaustion rule can't disagree with the daemon's own restart policy; the early-warning rule is the one that pages at 3am for a mere blip, so it needs to be something the operator chooses to enable.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:114`

### shep dog <name> takes no process-lifetime stdout/stderr lock

shep dog <name> is dispatched from the CLI's early block (beside daemon and bleats) and deliberately does not hold a locked stdout/stderr guard for its whole lifetime.

**Why:** A dog runs indefinitely until signalled; a process-lifetime StderrLock held on the main thread had already wedged the daemon on its first warning once (2026-08-09), and would wedge a dog the same way.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:118`

### shep flock always shows a second Dogs table; no --all flag exists at all

shep flock prints the sheep table, then an always-visible Dogs table beneath it whenever any dog is registered (stopped entries were always visible by default too). --format json stays one flat array with each row carrying its own dog marker. The design spec's own --all flag proposal was dropped outright rather than deferred.

**Why:** The maintainer found the design spec's prose (an --all flag that would widen both tables to include stopped entries) directly contradicted its own rendered sample (which already showed a stopped sheep in default output). Since stopped entries were already always shown, a widening flag would widen nothing; hiding them by default just to give the flag something to do would be a visible regression against current and pm2 behavior. A dead bark dog is exactly what an operator needs to notice by default.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:108-109`

### shep-client gets an explicit, opt-in Client::reconnect (&mut self) API rather than a transparent retry inside request() - *unverified*

Ruled explicitly against making a dropped connection silently re-dial inside the ordinary request() call path; instead Client::reconnect()/reconnect_within() are new, separately-called methods returning Reconnected::{SameDaemon, NewDaemon} (distinguished via the pid already carried in HelloAck), and the documented remedy for a SUPERVISED dog specifically is to exit on RequestError::Closed rather than reconnect at all.

**Why:** Instance ids are minted per daemon lifetime and never persisted (ProcessEntry has no Serialize); a request like `Delete{selector: Id(7)}` transparently re-dialed after a daemon restart could silently land on a totally different process (or none) under a NEW daemon where id 7 means something else - converting a loud, safe failure into a silent, wrong action, which is exactly the shape of shep-deploy's own worst production defects. `&mut self` (rather than interior mutability that would slide back toward transparent silent reconnection) also forces a caller to hold exclusive access at exactly the moment a connection's identity changes, deliberately trading away the crate's own documented Arc<Client>-sharing convenience for that safety property. A supervised dog reconnecting instead of exiting risks becoming a SECOND copy racing the new daemon's own freshly-autostarted instance of itself.

`docs/writing-plans/plans/2026-08-27-dog-prerequisites.md:2124 (NOT yet shipped - no Client::reconnect method exists in the current tree)`

### shep.toml has exactly one writer (the CLI); the daemon only reads, and re-reads on every DogConfig request

enable/disable/adopt/rehome edit shep.toml; the daemon never writes it and re-reads the whole file fresh on every dog connect rather than caching a copy from boot.

**Why:** Two writers would need file locking - one of three sharp edges this project's own trace notes recorded from pm2's own answer (read-whole-file/write-whole-file, no locking, concurrent writers lose one). A single always-current reader makes 'disable then enable re-reads config' literally true, at the cost of one small file read per dog connect (once per dog per daemon lifetime).

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:99-100`

### Wildcard selectors never match a dog

ProcessSelector::is_exact plus one shared selection helper in the actor ensures a name/id-wildcard selector (e.g. reload all, stop all, delete all) can never target a dog.

**Why:** The design spec requires this for reload all; the same argument holds for stop/delete all, where accidentally killing an alerting dog takes monitoring down silently. Implemented once centrally rather than as five separate ad hoc `if` checks.

`docs/writing-plans/plans/2026-08-12-shep-phase9-dogs.md:111`

## Audit debt

### Action replies get a stamped correlation id (SHEP_CHANNEL_VERSION=1)

Every daemon-initiated action carries a unique numeric id; an app that echoes it back on action-reply gets matched to that exact request. An app that doesn't echo it falls back to the old name+order matching, with one documented sharp edge: an unstamped reply can be consumed as payment for an already-timed-out debt of the same action name while a live wait of the same name is left reporting timed_out.

**Why:** Closed audit findings wire #2/#3. Before this, a slow app's genuinely-on-time reply to a live trigger could be silently swallowed as payment for an earlier abandoned request of the same action name, and the operator was falsely told 'timed_out'. Backward compatibility with pre-existing apps that never echo an id was required, so the fallback behavior is preserved byte-for-byte.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:1225, 1896-1997`

### AF_UNIX handshake-close is observed differently on macOS vs linux/arm64

a_daemon_that_closes_without_answering_is_not_a_silent_success asserted ConnectError::HandshakeClosed, which is macOS's shape (the peer's close surfaces on the following read). On linux/arm64 the WRITE fails first (the send of the Hello payload itself errors), mapping to ConnectError::Io instead.

**Why:** Discovered as the one red test anywhere in the project when the Linux CI leg actually ran (it hadn't, for phases). Both CLI consumers already collapse the two error variants into the same DaemonUnreachable outcome, so nothing user-facing was actually wrong - but it's a genuine platform difference in how AF_UNIX delivers peer-close, not a bug, and any future socket-close test needs to assert on both shapes or the collapsed outcome rather than one specific variant.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:2300`

### check_log_ancestry's TOCTOU window: openat2 fix designed but deliberately not built - *unverified*

A documented but unimplemented design for closing a check-then-open race in check_log_ancestry: nix::fcntl::openat2 with RESOLVE_NO_SYMLINKS on Linux, wrapped in unsafe FromRawFd inside shep-daemon/src/sys.rs with a fallback ladder for ENOSYS/EPERM (old kernels, seccomp) back to today's check-then-O_NOFOLLOW-open path (which remains the only path on macOS).

**Why:** Deferred because it is new unsafe on a Linux-only path the project cannot locally test from a macOS dev machine - exactly the debt shape a platform audit finding had already complained about. Trigger to build it: a Linux box in the regular test loop, or a threat model including an attacker with write access to a log directory's parent.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:3645-3664`

### CI stayed workflow_dispatch-only "while the repo is private" - later reversed - **superseded**

Phase 10 deliberately made the CI workflow correct but did NOT flip its trigger from workflow_dispatch to push/pull_request, per the maintainer's standing instruction to avoid CI minutes until "the base phases ship".

**Why:** SUPERSEDED as of Phase 17 (2026-08-19): the repository went public and the workflow_dispatch restriction was removed since public repos get free standard GitHub Actions runners. CLAUDE.md now states this explicitly. Notable because CLAUDE.md itself carried the stale "private repo" claim for a time after the repo actually went public, costing real time on a later phase that was scoped to "turn CI on" when it already was.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:2830 (superseded per docs/writing-plans/plans/2026-08-19-phase17-deferred-sweep.md and CLAUDE.md's CI section)`

### DaemonConfig deliberately left as a non-proof-token, unlike ResolvedApp

ResolvedApp keeps its config field private so holding one proves it passed normalize(); DaemonConfig's daemon/dog fields stay pub, and its only validation (max_cron_sleep floor) happens inline inside DaemonConfig::load rather than a separable validate step.

**Why:** Nothing constructs a DaemonConfig by hand outside tests today, so nothing is currently wrong; making the fields private and splitting out validation is an architectural call reserved for the maintainer's own open-questions list, not a defect to fix speculatively. Trigger to revisit: any production path assembling a DaemonConfig from something other than a file (e.g. the daemon-config flags layer).

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:3616-3630; confirmed pub fields still present at crates/shep-core/src/config/daemon.rs:208-238`

### kill_signal validated at normalize time, not clamped at stop time

An unsupported kill_signal name is now a hard rejection during config normalization rather than being silently clamped to a default at the moment a stop actually happens.

**Why:** Closed audit finding 'config #2' - a late clamp meant an operator's config typo silently changed behavior only when a stop occurred, far from where the mistake was made.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:75`

### ProcessInfo made #[non_exhaustive] with a builder

ProcessInfo gained #[non_exhaustive] plus ProcessInfoBuilder, so future fields (later: lambs, cpu_percent) can be added without breaking every external construction site.

**Why:** Closed audit finding wire #1. Deliberately done ahead of any concrete new field so the lambs field (added a phase later) and other future additions would be cheap, rather than forcing a premature ProcessInfo split.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:76; confirmed at crates/shep-core/src/protocol/request.rs:605,777`

### reuse_port documented as inert (Phase 10) - SUPERSEDED - **superseded**

Phase 10 found AppConfig::reuse_port had zero production readers (reload overlap was unconditional) and rewrote its doc comment to say plainly 'this field is inert today', deferring the day it would matter to 'the day shep gains a reload mode that does NOT overlap by default'.

**Why:** Superseded later the same day (per CLAUDE.md): reuse_port is now read by ReloadMode::of to decide between Serial and Overlap reload - the exact trigger condition Phase 10 predicted. Confirmed at crates/shep-daemon/src/supervisor.rs:1665 (ReadinessSource::Probe(..) if !config.reuse_port => Self::Serial).

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:3335-3391`

### The cli_e2e 7-test correlation is a standing, unresolved false-positive risk - *unverified*

Four of nine cli_e2e tests in one grouping failed under --test-threads=1 while zero of six failed in another grouping - investigated and exonerated twice as a load artifact rather than a real regression, but never freshly re-measured since Phase 6.

**Why:** Recorded as an open risk in the serial phase-gate run CLAUDE.md mandates before a merge, rather than an edit - it needs one fresh bounded measurement pass with numbers written down, which nobody has done since.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:3693-3702`

### The macOS-vs-Linux handshake-close test accepts either error variant

The client's a_daemon_that_closes_without_answering_is_not_a_silent_success test now asserts ConnectError::HandshakeClosed | ConnectError::Io(_), not HandshakeClosed alone.

**Why:** On macOS the peer's close is observed by the read after Hello; on linux/arm64 AF_UNIX delivers the peer's close to the next write instead, producing Io first. Both CLI consumers already collapsed the two into the same user-facing outcome, so the test was widened to match reality on both platforms rather than being platform-gated.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:2302-2326; confirmed at crates/shep-client/src/connection.rs:288-302`

### The windows-gnu cross-check quietly dropped for three phases, then restored into CLAUDE.md

cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu was carried by every plan from Phase 3-6, silently absent from Phases 7-9's plans, and never written into CLAUDE.md itself - so nothing flagged its disappearance. Phase 10 re-measured it green (EXIT=0, 8.42s) and put it into CLAUDE.md directly rather than a plan.

**Why:** The likely cause of the silent drop was its prerequisite: ring's build script needs a cross C toolchain (mingw-w64) for that target, and a host without it simply can't run the check - an easy thing to stop doing and never mention. Recording it in CLAUDE.md rather than a per-phase plan was the fix, since plans expire and CLAUDE.md doesn't.

`docs/writing-plans/plans/2026-08-13-shep-phase10-audit-debt.md:2326-2352, 3703-3729`

## The Phase 11 verbs and the KV store

### A reload drainee is signalled, never skipped

Unlike shep trigger (which skips a process mid-reload-drain because an action expects a reply channel that a departing process can't honor), shep signal delivers to a drainee if the selector matches it.

**Why:** A signal expects no reply, and the drainee is still a live, matched process - skipping it would silently hold back delivery with no reply channel available to explain why.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:302`

### channel.* bus topics carry child-to-shepherd traffic only, never the shepherd's own outbound messages

channel.ready, channel.metric, channel.action_reply are the only three topics; a Shutdown or Action message the shepherd itself sends to a child is never mirrored onto the bus.

**Why:** Every ProcessEventKind reports something that already happened to a sheep; an in-flight dispatch the app hasn't answered yet doesn't fit that shape and would make the bus report a request rather than an outcome. It would also make the bus a loop for any dog that both subscribes to channel.* and calls Trigger, seeing its own dispatch echo back. Cost was also weighed explicitly: two more bus topics for traffic that already has a reporter (process.stop for Shutdown, Response::Triggered for Action) didn't clear the bar BusEvent's own doc sets for adding a variant.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:440`

### channel.* bus topics carry only child-to-shepherd traffic, never the shepherd's own outbound writes

Three topics (channel.ready, channel.metric, channel.action_reply) publish inbound shepherd-channel messages verbatim (not redacted) to the bus. The shepherd's own Shutdown and Action writes to a child are never published as bus events.

**Why:** Four reasons: deferred.md's actual gap was specifically about inbound traffic (Ready/Metric/stale-action-reply being invisible) - the outbound half already has a reporter (Response::Triggered, process.stop); a channel.action event would be the only bus event describing an unanswered request rather than a completed outcome, breaking the pattern that every ProcessEventKind is something that already happened; publishing outbound Action dispatches would let a dog that both subscribes and calls Trigger see its own dispatch echoed back, creating a loop; and BusEvent's own doc explicitly asks that a new variant's cost (real, for every subscriber that predates it) be weighed, which two more topics for already-reported traffic doesn't clear. The inbound messages are published unredacted (unlike a dog's [dog.<name>] section) because nothing on the shepherd channel is a credential - Ready is empty, Metric is a name+float, ActionReply is text the app itself chose to publish.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:440-478`

### KV keys are flat opaque strings, not a dotted/nested path grammar

A key like `bark.cooldown` is one opaque string containing a literal dot, matching [A-Za-z0-9._-]{1,128}, never parsed as a path into a nested structure.

**Why:** pm2's own store had a dotted/colon nesting grammar with its own quoting rules; the project's standing decision is that pm2 formats live only in the importer, and inventing a second config language for a store the spec itself calls secondary would be needless. The flat alphabet also means values never need quoting on the command line.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:348`

### Lambs render as pid + executable name only - no cmdline, no per-lamb memory

Lamb{pid, name} where name is the OS-reported executable name (sysinfo::Process::name()), never the process's argv.

**Why:** A process's argv routinely carries credentials (--password=, ?token=) and shep describe --format json is output people paste into GitHub issues - argv is the dangerous half of the information, the executable name is the useful half. Per-lamb memory was refused because the sheep's own row already reports the tree total and per-lamb numbers would only invite "why don't these add up" (they do, but explaining it isn't worth the field).

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:398`

### Lambs render as pid + executable name only - no cmdline, no per-lamb memory

Lamb { pid, name } where name is sysinfo::Process::name() (the executable name), never argv/cmdline. Per-lamb memory/CPU is not exposed; only the sheep's own row carries the tree total. ProcessInfo::lambs is Option<Vec<Lamb>> (None = 'this reply didn't walk the tree', Some(vec![]) = 'walked, found none'), and is populated only by Describe, never by ListFlock.

**Why:** A bare pid list is nearly useless without a name (the operator would just run ps anyway). cmdline/argv is refused because it routinely carries credentials (--password=, ?token=) and shep describe --format json output gets pasted into GitHub issues. Per-lamb memory is refused per deferred.md's own explicit warning against growing ProcessInfo speculatively, and because the sheep's row already reports the tree total. The Option (not bare Vec) preserves the same three-state honesty out_file/cpu_percent already use for backward-compat with a pre-field daemon peer. ListFlock is excluded because the walk costs a second full process-table refresh and a flock listing is the thing an operator runs in a polling loop.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:398-439; confirmed at crates/shep-core/src/protocol/request.rs:480, 660`

### lambs_of takes a pre-built LambIndex, not a self-contained (root_pid) call

The final signature is lambs_of(&self, index: &LambIndex, root_pid: u32) rather than a single-argument form that refreshes internally; no lambs_of_now(root_pid) convenience wrapper exists.

**Why:** The plan's own first draft specified two incompatible signatures across different steps and never resolved them, which would have shipped either a quadratic implementation or a mutation-test step pointing at a body that no longer existed. The chosen shape mirrors TreeIndex::build + TreeIndex::sum_from, split for the same reason: the expensive part (scanning the whole process table) is built once and shared across every root's walk - the caller that makes this matter is `shep describe all` on a large flock. A convenience wrapper was rejected because every caller in this phase already builds the index once via with_lambs, so a wrapper would only be sugar hiding the cost the split exists to expose.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:6693-6716; confirmed at crates/shep-daemon/src/limits/stats.rs:99,286`

### ProcessInfo split into identity/logs/stats/dog stays deferred even after lambs landed

deferred.md had named the lambs field as the trigger that would force splitting ProcessInfo into separate identity/log-path/stats/dog-provenance types. Phase 11 adds lambs and explicitly does not perform that split.

**Why:** Phase 10 made ProcessInfo #[non_exhaustive] with a builder specifically so adding lambs would be cheap without forcing the split prematurely; having added the field, the row is judged still coherent as one struct ('one sheep, everything known about it'). The stated trigger for revisiting is a second consumer needing a genuinely different projection of the data, not merely a field landing.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:7705-7710`

### ProcessInfo::lambs is Option<Vec<Lamb>>, tri-state, and populated only by Describe

None means "this reply didn't walk the process tree" (a peer daemon predating the field, or any non-Describe reply); Some(vec![]) means "walked, found none". ListFlock never populates it.

**Why:** Collapsing to a bare Vec would render a pre-field daemon's reply as "this sheep has zero lambs", a false claim rather than an unknown. The walk costs a second whole-process-table refresh, which is affordable for a one-shot Describe but not for a flock listing an operator leaves running in a loop.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:398`

### scale takes an absolute instance count; no relative +N/-N form

shep scale web 4 means "web has four instances when this returns", full stop - there is no delta grammar.

**Why:** A relative delta is ambiguous under concurrent scaling (two operators both running +2 against a flock of two land on either 4 or 6 depending on interleaving, and neither gets a checkable number); an absolute count is idempotent. This project's own trace notes also record a real pm2 crash on the relative-remove path specifically to avoid reproducing.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:246`

### scale-down removes the highest instance-slot numbers first

Scaling down takes the highest slot numbers, not the lowest, because instance_slots always allocates the lowest free slot on scale-up.

**Why:** This makes a scale-up-then-down round trip stable: 2->4->2 leaves the same slots {0,1} it started with. Taking the lowest first would leave a different pair, different log filenames, and a different SHEP_INSTANCE for surviving processes.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:246`

### sendline needs a per-app opt-in `stdin` field, default false; piping stdin is never unconditional

AppConfig::stdin defaults to false; without it, sendline answers a per-row `no_stdin` refusal.

**Why:** Piping stdin to every spawned sheep unconditionally would change behavior for processes nobody asked to change (today every sheep gets Stdio::null()), and many programs detect a closed/null stdin as their signal to run non-interactively (no prompt, no colour, no readline) - `less` and `git` are named examples. It also costs a descriptor and a task per sheep for the process's whole life, against the project's single-digit-MB idle-RSS budget.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:324`

### sendline requires an explicit per-app opt-in (AppConfig::stdin, default false)

A new stdin: bool field, default false, gates whether a sheep's stdin is piped at spawn time at all. Without it, sendline reports no_stdin naming the field, mirroring channel's no_channel treatment.

**Why:** Three reasons, weighted: piping unconditionally would change every spawn on the system (today every sheep gets Stdio::null()); many programs (less, git, a Node app checking process.stdin.isTTY) detect a piped stdin and silently switch into non-interactive mode nobody asked for; and it costs a descriptor plus a task per sheep for the process's whole life, against the project's single-digit-MB idle-RSS budget - the same budget that motivated channel's own opt-in default.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:324-347; confirmed at crates/shep-core/src/config/app.rs:353`

### shep scale 0 is refused, not treated as delete; dogs and mid-reload apps can't be scaled

shep scale web 0 is rejected the same way normalize already rejects instances==0, rather than being reinterpreted as deleting the app. Scaling a dog is refused (a dog is one process by contract). Scaling an app mid-reload is refused via the existing SupervisorError::ReloadInFlight.

**Why:** Accepting 0 would push a config through the daemon that the daemon's own validator would otherwise refuse. A reload holds two live processes in one instance slot; scaling down onto that slot mid-swap would remove one of the two processes and leave the reload with nothing to finish.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:269-280`

### shep scale takes an absolute count only, no relative +N/-N

shep scale web 4 sets the instance count to exactly 4; there is no relative delta form. Scale-down removes the highest instance slot numbers first.

**Why:** A relative delta is ambiguous under concurrent scaling (two operators running +2 against a flock of two get 4 or 6 depending on interleaving); an absolute count is idempotent. This project's own trace notes recorded a pm2 crash on the relative-remove path specifically to avoid reproducing it - not building the path is the strongest way to not reproduce the bug. Removing the highest slot first makes a scale-up-then-down a round trip back to the same slot numbers (since instance_slots always allocates the lowest free slot), preserving log filenames and SHEP_INSTANCE values for survivors.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:246-267`

### shep signal has no --group flag to reach a sheep's whole lamb tree

There is no way to send a signal to every process in a sheep's tree at once; shep signal always targets just the sheep's own pid.

**Why:** Deliberately deferred rather than built: a group-wide nudge and a single-process nudge are genuinely different asks, and one flag (--group) would flip the safe default (single-process) to be the non-default reading. Trigger to revisit: an app class where the sheep process is a supervisor that doesn't forward signals to its own workers - a real shape that just hasn't come up yet.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:7565-7577`

### shep signal targets only the sheep's own pid, never its process group

shep signal web SIGHUP delivers via kill to the sheep's own pid, not kill(-pgid, ...). The accepted signal set is nine names in a dedicated OperatorSignal grammar (SIGHUP/INT/QUIT/TERM/USR1/USR2/WINCH/CONT/KILL); SIGSTOP is refused, SIGKILL is accepted. No raw signal numbers cross into shep-core (they differ by platform - SIGUSR1 is 10 on Linux, 30 on macOS); the enum crosses the runner seam and tokio_runner.rs maps it to nix::sys::signal::Signal.

**Why:** Group-wide delivery is already the stop ladder's job (needed because a `thing & wait` wrapper keeps its child in a separate process group during a graceful stop). shep signal exists for a different job - an operator's direct conversation with the one process they named (SIGHUP-to-reopen-config, SIGUSR1-to-dump-state). Broadcasting to the whole process group would hit unrelated lambs the operator never addressed. SIGSTOP is refused because a stopped sheep would still read 'online' in every listing - the shepherd has no way to observe or report the difference, so it must not be able to create that state. The narrow reading is also the recoverable one: an operator who wanted the group can signal each instance, but can't un-send a broadcast.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:281-323; confirmed at crates/shep-core/src/signals.rs:21-43`

### shep signal targets the sheep's own pid, never its whole process group

Unlike the stop ladder (which is deliberately group-wide, to reach a `sh & wait` wrapper's children too), shep signal delivers only to the named sheep's own pid.

**Why:** signal exists for a different job than the stop ladder: an operator having a conversation with the application itself (SIGHUP-to-reopen-config, SIGUSR1-to-dump-state), and broadcasting that to every lamb in the group would hit an unrelated shell wrapper or node child with a signal meant for the parent. The reading is also recoverable in the safer direction: an operator who wanted group-wide reach can signal each instance separately, but one who didn't want it can't un-send a broadcast.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:281`

### shep signal's accepted grammar is a distinct nine-name enum, not KillSignal's four

OperatorSignal accepts SIGHUP/INT/QUIT/TERM/USR1/USR2/WINCH/CONT/KILL. SIGSTOP is explicitly refused.

**Why:** SIGSTOP would put the flock into a state (paused-but-still-reported-online) shep structurally can't observe or reconcile against; SIGKILL is accepted as the honest spelling of "die now" since the stop ladder already sends it under escalation. Raw platform signal numbers never cross into shep-core (SIGUSR1 is 10 on Linux, 30 on macOS) - the enum crosses the runner boundary and is mapped to nix::sys::signal::Signal explicitly at the platform seam.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:281`

### The KV store lives in shep-core as a plain file, never over the daemon socket

shep_core::kv stores $SHEP_HOME/kv.json; shep set/get/unset read and write it directly with no daemon involvement and no new Request variant. Locking reuses the pattern already proven by barks::append and ShepToml::edit: exclusive flock(2) on a sibling kv.json.lock (never the target, since rename swaps the inode the lock is held on), content staged through a uniquely-named temp file created at 0600, fsynced, then renamed. Keys are flat opaque strings matching [A-Za-z0-9._-]{1,128}, not a dotted/nested path grammar. unset --all (a flag) clears everything, deliberately not a magic key name like 'all'.

**Why:** The deciding question was who else reads the store: spec says a dog needs it too, ruling out a CLI-private module, but a dog is just shep dog <name> in the same binary, so a shep-core file is linked in for free. Going over the socket (like [dog.<name>] config) was rejected because that mechanism exists specifically to avoid leaking secrets through the environment - a 0600 file inside a 0700 $SHEP_HOME, read by the same uid, already has none of those exposure properties, so the socket buys nothing extra. A daemon-mediated store would also break the file's usefulness with no daemon running, unlike every other config verb in the tree (enable, barks, flush --daemon all work file-only). A nested-key grammar was rejected as a second config language next to the Flockfile with its own quoting rules, for a store the spec itself calls secondary. A flag for --all avoids the exact 'unset all' name-collision ambiguity FlushArgs' own doc already argues through.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:348-397`

### The KV store lives in shep-core, is CLI-written, and never crosses the wire (no daemon RPC)

shep set/get/unset read and write $SHEP_HOME/kv.json directly, with no connection to the shepherd - unlike a dog's [dog.<name>] section, which travels over the socket.

**Why:** The deciding question was who else reads it: a dog reads it too (spec names it for ad-hoc + dog runtime tweaks), and a dog is `shep dog <name>` - the same binary - so a shep-core-hosted file is linked into every dog for free with no daemon mediation needed. A daemon-mediated design would break the provisioning shape every other config verb in the tree supports (working with no shepherd running) for no security gain, since a 0600 file inside a 0700 $SHEP_HOME already has the same trust properties the socket would provide.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:348`

### The lambs view is explicitly documented as not the kill unit's own set

describe's caption and the Lamb type's own doc both state the ppid-descendant walk is not the same set the stop ladder kills: a double-forked descendant can leave the ppid tree while staying in the process group (killed but never listed), and a setsid() grandchild can stay in the ppid tree while leaving the group (listed but never killed).

**Why:** limits/mod.rs's own module doc had already recorded this divergence for the kill-ladder's use of ppid trees; the lambs feature explicitly repeats the caveat on-screen in describe's own caption, not buried only in a doc comment, so an operator doesn't infer a guarantee the walk never made.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:411-424`

### Verbs-before-surfaces ordering: Phase 11 before lookout/whistle

The maintainer ruled the six remaining daemon-surface verbs (scale, signal, sendline, KV, lambs, channel.*) must ship before shep lookout (TUI) or shep whistle (MCP).

**Why:** Both of those are UI surfaces over the daemon's operator API; building them before the API was complete would mean shipping a TUI pane that can't scale an app or an MCP tool list that can't send a signal, then widening the UI later instead of building it once over a finished surface.

`docs/writing-plans/plans/2026-08-13-shep-phase11-verbs.md:6-12; order confirmed by CLAUDE.md's phase history (Phase 12 lookout, Phase 13 whistle, both after Phase 11)`

## lookout

### 12a stops at one pane, deliberately

Phase 12a built lookout's whole shell (deps, terminal lifecycle, palette, event loop, link supervision) plus only the flock table, then produced rendered TestBackend frames on disk for the maintainer to look at before the other three panes (bleats feed, detail pane, host strip) were designed in 12b.

**Why:** The flock table went first so the panes could be looked at before the rest of the layout was settled. A selected row and cursor were considered for 12a and cut for the same reason: their only consumer, the detail pane, is 12b.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:8`

### A dropped/failed try_send needs its own Msg::Unsent, which the approved design omitted

If the reducer enters the in-flight ('sent, waiting') state and run_ui's try_send to the link task fails (channel full or closed), nothing in the original design would ever answer it - the bar would say 'waiting for the shepherd' forever and the one-action-at-a-time guard would block every future action permanently. Added Msg::Unsent{sent} to clear the action and report 'it was not sent' without claiming the shepherd is unreachable (Full is reachable even when the shepherd is healthy, since the channel is shared with lamb fetches and has capacity 2).

**Why:** Deliberately vague error text ('it was not sent', not 'shepherd unreachable') because the code genuinely cannot distinguish Full from Closed and shouldn't claim a cause it didn't observe - same discipline as the '-' CPU cell convention elsewhere in lookout for unknown values.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:194`

### Action gate lives in the KV store, not shep.toml/daemon config

lookout's allow_control gate (--allow-control flag or `shep set lookout.allow_control true`) is a fat-finger catch, not a security boundary, because lookout runs as the operator's own terminal process under the operator's own uid - the shepherd can't distinguish a lookout keypress from a `shep stop` and isn't entitled to refuse it. Contrast whistle's `[whistle] allow_control` in shep.toml, which is daemon-side because whistle acts on behalf of a client (an agent) the operator isn't watching.

**Why:** Same underlying property (both gates are fat-finger catches, not security controls) but different storage location because the threat model differs: lookout's threat is a fat finger by the person at the keyboard; whistle's is an agent acting on log content it just read.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:211`

### An armed (not-yet-sent) confirm in lookout is cleared the instant the link stops being Live

The design says arming is refused unless Link::Live, but doesn't handle a prompt that's already armed when the link then dies. This plan clears an ARMED action on the Retrying/Frozen transitions (a SENT action's in-flight line survives and resolves normally, since it's a real outstanding request).

**Why:** Because the clock (`now`) stops advancing once frozen, an armed prompt left alone would never expire on its own timer, and Enter would still try to send a request the operator was never going to get honoured. The design's own rule is that refusals happen at ARM time so an operator never answers a question that was never going to be honoured - leaving a stale prompt up inverts that.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:156`

### An armed confirm is cleared when the link leaves Live, deviating from the approved design

The approved design (A9) only refuses arming a confirm unless the link is Live at arm-time, but doesn't say what happens to an already-armed prompt if the link drops afterward. Implemented so Msg::Retrying/Msg::Frozen clear an armed (but not yet sent) action, making 'armed implies Link::Live' an actual invariant.

**Why:** Two compounding bugs in the literal reading: refusals in this design are meant to happen at arm-time so an operator never answers a question that was never going to be honored, and separately, app.now (which drives the confirm's expiry timer) stops advancing once the link is Lost - so an unclearer prompt would never expire and Enter would still try to send into a dead link.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:250`

### Armed+Sent modeled as one Option<Action> with a Stage enum, not two separate Option fields

The approved design's struct Armed{verb,id,name,at} plus a separately-described in-flight state would naturally become two Option fields on App. Implemented instead as a single action: Option<Action> where Action carries stage: Armed | Sent.

**Why:** Two Option fields would admit an impossible state (armed and in-flight simultaneously), directly contradicting the design's own stated 'one action at a time' rule - making the illegal state unrepresentable rather than merely untested.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:167`

### Bounded retry then freeze, never exit - but only after a link is established

On a shepherd dying mid-session, lookout retries 5x (250/500/1000/2000/4000ms, ~7.75s) then freezes the last-known values and never exits on its own. A shepherd that was never reachable is a different case: the FIRST connect happens before raw mode, and failure there produces the ordinary daemon_unreachable refusal/exit(5), never entering the TUI at all.

**Why:** An earlier draft had lookout open a full dashboard even with no shepherd running, cycle 'reconnecting' for 8s, then exit Success - inconsistent with every other client verb exiting 5 in that case. Deliberately not bleats' precedent (bleats exits cleanly on disconnect) because a standing dashboard vanishing is worse than a follow command ending.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:119`

### Flock table hand-built as Lines, not ratatui's Table widget

view/flock.rs builds Vec<Line> directly via Buffer::set_line rather than ratatui::widgets::Table, to match shep flock's own column algorithm (fixed widths, two-space gutters, no box-drawing) exactly rather than risk two independent column algorithms drifting on a multi-byte name. Ratatui is kept for the backend abstraction, diffing redraw, terminal lifecycle and TestBackend - not for its widgets.

**Why:** A dashboard pane that visually diverges from `shep flock`'s own table on the same data would be a subtle correctness bug in the eyes of an operator comparing them.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:344`

### Flock table subscribes AND polls; poll always wins conflicts

Ported from bark's run_loop: subscribe to process.*/daemon.* for latency, but also poll ListFlock every 2s and immediately on any Dropped/Lagged event, because broadcast channels drop rather than queue for a lagging subscriber and give no info about what was lost. interval_at (not interval) avoids double-counting the opening listing as a repair poll.

**Why:** tokio::sync::broadcast silently drops for a lagging subscriber; only an explicit re-list can recover truth. 2s chosen as ~15x cheaper than once-a-frame while staying under an operator's patience threshold.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:231`

### Host strip fitting reuses flock::fit; a bespoke truncation mechanism was built then cut

An earlier draft built a second fitting mechanism for the host strip (Vec<String> segments, a pop-based drop loop, a joined_width helper, tests walking every width 200 down to 10). It was cut in favor of joining segments in drop order and running the whole line through the same `fit` call every other line on screen uses.

**Why:** The maintainer's ruling for the phase was 'kept as plain as the flock table, no elaborate layout'; the bespoke mechanism bought nothing since joining in drop order already makes right-truncation equal the desired drop order for free.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:666`

### lookout gates destructive actions via the KV store, not shep.toml, because it's a fat-finger catch not a security boundary

--allow-control (or lookout.allow_control=true in the KV store) gates action keys in lookout, deliberately distinct from whistle's [whistle] allow_control in shep.toml.

**Why:** whistle's control tools act through the shepherd on behalf of a client the operator isn't watching, so the shepherd must be the authority and the flag has to be daemon-side. lookout runs as the operator's own process under the operator's own uid - the shepherd can't tell a lookout keypress from a raw `shep stop`, and wouldn't be entitled to refuse it if it could - so this gate exists only to catch a fat-fingered keypress, and its --help text says so explicitly rather than reading as a security control it isn't.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:412`

### lookout gets stop/restart/reload but deliberately not start

Explicit maintainer ruling recorded so it isn't re-derived: even though the design doc's wording called the lookout and whistle control surfaces 'identical', lookout does not get a start key. Adding it later is additive (one key, one arm-time refusal), not a redesign.

**Why:** Scope decision, hers, stated as settled rather than argued from first principles - worth preserving precisely because the design doc's own word choice ('identical') is misleading against the shipped surface.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:14`

### lookout subscribes before its first list; bleats lists before it subscribes - and both orders are correct for their own case

lookout's rows carry a whole ProcessInfo per event, so an event arriving before the first snapshot upserts fine into an empty map; bleats needs the id->name cache the initial listing builds before any line can be resolved, so it must list first.

**Why:** An earlier reviewer would assume the reversed order between the two features is a copy-paste bug; it isn't - each verb's order avoids the specific gap its own data shape creates. List-then-subscribe for lookout would lose every event between the reply and the subscription for no reason, since nothing about its data shape requires that ordering.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:503`

### lookout's daemon-death handling: bounded retry then freeze, forever - but the FIRST connection is a different rule entirely

After a link is once established, losing the shepherd triggers a 5-attempt reconnect ladder (250ms doubling to 4s, ~7.75s total) and then a permanent frozen state showing last-known values; lookout never exits on its own. But the very FIRST connect happens before raw mode is even entered, and if it fails, lookout never opens the alternate screen at all - it just prints the ordinary daemon_unreachable error and exits 5, exactly like every other client verb.

**Why:** The maintainer's ruling was about a shepherd dying UNDERNEATH a running dashboard, which presupposes it was alive; a shepherd that was never there is a different situation, and an earlier draft would have opened a full-screen dashboard, spent 8 seconds "reconnecting", announced a death that never happened, and finally exited Success on a machine with no shepherd at all - while `shep flock` one line earlier exited 5. The 8-second bound itself (not 30s, not 2s) is calibrated so an ordinary deliberate restart (shep kill; shep muster, or a systemd bounce) never trips the freeze, while a genuinely dead shepherd is declared dead before the operator walks away.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:348`

### lookout's flock table is drawn as raw Lines via Buffer::set_line, not ratatui's Table widget

view/flock.rs builds Vec<Line> by hand and writes it directly, reusing the exact column-sizing algorithm the CLI's own output/table.rs already implements, rather than handing the job to ratatui::widgets::Table.

**Why:** The visual contract is explicitly "the table shep flock prints, live" - handing that to a second, independent column-sizing algorithm would let the two drift the first time a multi-byte name appears. ratatui itself is still used for the parts that earn it: the double-buffered diffing renderer, the backend abstraction, and TestBackend.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:573`

### lookout's Phase 16 confirm-to-act state is one field (Action{stage}) not two independent Option flags

Rather than the design's literal two-field sketch (an armed struct plus a separately-described in-flight state), the reducer carries a single Option<Action> with an internal Stage enum (Armed | Sent).

**Why:** Two independent Options would admit a state the machine can never actually be in (armed AND in-flight simultaneously), directly contradicting the design's own "one action at a time" rule. Collapsing to one field with an internal stage makes that invalid state unrepresentable rather than merely disallowed by convention.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:156`

### lookout's selected row is stored as a sheep id, never as a table index, and it reseats to the same screen position when the sheep disappears

App::selected became Option<u32>. When the selected id vanishes from a wholesale-replaced flock map, selection falls to whatever now occupies the SAME table position (clamped to the last row), not to row 0.

**Why:** Since the flock map is replaced wholesale every 2 seconds, an index-based selection would silently point at a different sheep the moment an earlier row is deleted - the detail pane and feed would then describe the wrong sheep with no visible signal. Falling to row 0 on every unrelated deletion would also be jarring for an operator scrolled deep into a 200-sheep flock.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:547`

### lookout's status bar reserves a separate slot for "the filter box being actively typed into" above notices, contradicting the approved design's own stated premise

The design assumed a notice could never cover the filter query because "while editing, every keypress is text, so nothing can raise a notice" - that premise was found false (BusLagged, BusEvent::Dropped, and DaemonShutdown all raise notices while the box is open, and on_text_key never clears them), so a sixth status-bar slot was added specifically for the live-editing filter box, ranked above notices.

**Why:** Without the fix, a notice landing mid-word covers the operator's own in-progress query until they finish typing, with nothing they type clearing it back off - the alternative fix (clearing the notice at the top of on_text_key) was rejected because it destroys the notice's information entirely rather than merely deferring its display.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:156`

### lookout's terminal restore uses a hand-written panic hook + Drop guard, not ratatui's bundled init()/restore()

A custom std::panic::set_hook wraps the previous hook to call term::restore() before it, paired with a TerminalGuard's Drop calling the same restore(); ratatui 0.30's own init() (which bundles the backend, hook, and lifecycle as one unit) is deliberately not used.

**Why:** init() picks the terminal, backend, AND hook as one bundle, which would prevent swapping in TestBackend for headless testing - the whole point of this design is that the render path, the reducer, and the UI loop can each be driven with no real terminal. Writing the four extra lines by hand costs nothing and preserves that seam. (This directly contradicts an earlier research doc, docs/research/lookout-tui.md, which had recommended relying on ratatui's bundled init()/restore().)

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:644`

### Not using ratatui's own init(): panic hook + Drop guard built by hand

12a writes its own panic-hook-wraps-previous-hook + TerminalGuard::drop() restore mechanism instead of ratatui 0.30's built-in init(), even though init() does the same restoring-panic-hook thing.

**Why:** ratatui's init() bundles terminal+backend+hook as one unit, which would remove the seam needed to swap in TestBackend for headless testing of the UI loop itself. Writing the 4-line mechanism by hand costs nothing and keeps that seam.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:415`

### Palette maps design tokens to terminal colors; never paints background; color is always redundant with text

Design language's 4 semantic colors (meadow/bark/butter/ink-3) map to 256-color indices with 16-color fallbacks; --paper (page background) and --ink (default foreground) are deliberately NOT mapped - the terminal's own background/foreground stay. Every colored cell's text already says the same thing the color says, so NO_COLOR or a 16-color terminal only lose decoration, never information.

**Why:** Forcing a light-theme background into a terminal fights the operator's own theme and loses on half of them. "Errors get a colour, not a face" taken verbatim from the design language.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:290`

### q and Ctrl-C always quit lookout, even with a destructive-action confirm armed - a deliberate carve-out of "every other key cancels"

The design's routing rule says every non-Enter key cancels an armed confirm; this plan makes KeyPress::Quit return Effect::Quit from inside that same routing rule, above the cancel logic, rather than letting the confirm's cancel consume it.

**Why:** input.rs's own shipped doctrine already states that dropping the Ctrl-C mapping anywhere "would leave the most reflexive way out of a terminal program doing nothing", forcing the operator to a `kill -9` from another window past every restore path lookout has. The property the original cancel-everything rule exists for (a cancelling key doing its normal job on a target the operator has lost track of) is untouched, since quitting discards the confirm entirely rather than acting on it.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:156`

### q and Ctrl-C bypass the confirm-cancel routing rule and quit immediately, deviating from the approved design's literal wording

The approved design said 'every other key cancels' an armed confirm, which as written would consume KeyPress::Quit too - meaning q/Ctrl-C would stop working while a confirm prompt is up. Implemented instead so KeyPress::Quit returns Effect::Quit before the cancel logic runs.

**Why:** input.rs's shipped doctrine (predating this phase) already treats Ctrl-C as sacred: losing it 'would leave the most reflexive way out of a terminal program doing nothing,' forcing kill -9 past every restore path term.rs has. Text mode already carved out the same exception, so this generalizes an existing precedent rather than inventing a new one.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:243`

### ratatui+crossterm cfg(unix)-gated at first, later made unconditional - a superseded decision - **superseded**

12a deliberately put ratatui/crossterm under [target.'cfg(unix)'.dependencies] because lookout was unix-only (needs a unix socket) and Windows refused every verb before dispatch, so declaring them unconditionally would bloat the Windows build for no reachable code.

**Why:** SUPERSEDED: once Windows support became real (Tier A, post-phase-16-era), lookout and whistle both compile and run on Windows, so the same four deps became unconditional in shep-cli/Cargo.toml - the crossterm_winapi face is now a real feature cost, not dead weight. Verified: crates/shep-cli/Cargo.toml:224-236 carries the deliberate comment recording this reversal directly.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:90`

### Read is coalesced onto the redraw gate, not spawn_blocking

The blocking std::fs read runs on the UI task itself (shep-cli's tokio has no fs feature). A held-down j on a large flock delivers key-repeat as 20-30 Press events/sec, each of which could trigger a 128KiB file read - so the read is gated behind the same MIN_REDRAW throttle the draw already uses, turning worst case into one read per ~33ms.

**Why:** spawn_blocking would add a task, a channel, and a race between the reply and the next snapshot to hide about a millisecond - not worth it. The real risk was a busy loop with a syscall in it, not a documentation gap.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:491`

### Selection marker is a gutter character '>' , never a color or REVERSED row

Chosen over a REVERSED row style or colored row specifically so the selection survives NO_COLOR and 16-color terminals. '>' rather than '▸' because '▸' is East-Asian Ambiguous width and could shift a whole row's columns on some terminals. Implemented as a 2-column gutter outside the table's own width budget, not a Column, so it doesn't disturb columns_for's existing width thresholds.

**Why:** The whole palette module is built on 'colour is always redundant with text'; a decoration-only cursor (color/reverse) would violate that for the one signal that has no accompanying text.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:579`

### Selection stored as sheep id, not table index; reseats by position when the id vanishes

App::selected is Option<u32> (an id), not an index - because the flock map is replaced wholesale every 2s, so an index would silently point at a different sheep after a delete. Viewport offset is derived (centered on selection, clamped), never stored, to remove the class of bug where a stored offset disagrees with a stored selection.

**Why:** An id survives reordering/growth/shrinkage as a no-op; falling back to 'same position' rather than row 0 avoids throwing an operator back to the top of a 200-sheep flock on an unrelated delete.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:547`

### Status bar needs 6 slots, not 4 - because 'the filter' meant two different things in the approved design

The approved design conflated the filter text box being actively typed into with the persistent applied-filter line, using 'the filter' for both. Splitting them into distinct bar slots was necessary because Msg::BusLagged, BusEvent::Dropped and BusEvent::DaemonShutdown all raise notices that arrive while the box is open and keypresses don't clear them - so under the original 4-slot design, a notice landing mid-word would cover the query being typed until Enter/Esc, with no way to get it back.

**Why:** The design's own justification for accepting this cost ('while editing every keypress is text, so nothing can raise a notice') was checked against the code and found false - an example of empirically falsifying an accepted design assumption before shipping it. The rejected fix (clearing self.notice on every text keypress) was rejected too: it destroys the notice instead of deferring it.

`docs/writing-plans/plans/2026-08-16-shep-phase16-lookout-completion.md:176`

### The bleats-feed pane's tail reader counts missed lines and missed bytes as two SEPARATE numbers

Tail::missed_lines (exact) tracks lines the reader read and discarded (above the per-file line cap, or a partial line a window boundary cut); Tail::missed_bytes (exact, in bytes) tracks bytes appended before the reader's 64KiB window that were never read at all and whose line-count is genuinely unknowable.

**Why:** An earlier draft counted only the below-the-window case (bytes) as a rare 4MB-burst edge case, but the far more common failure is the in-window case: a sheep writing thirty lines between two 2-second polls loses twenty-five of them with missed_bytes sitting at zero, since Tail::lines holds up to 40 but the pane only renders 5. Reporting only one of the two would silently misrepresent the pane as complete during exactly the busy-flock moments someone is actually watching it.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:325`

### The feed reads log files directly; it does not subscribe to log.*

Four candidates for the bleats feed pane were compared (subscribe log.* with a UI ring; subscribe+filter in the link task; ask the shepherd to filter; read the selected sheep's log files from disk on a timer). Chose (d): bounded reads from disk, triggered by the same 2s flock-listing cadence.

**Why:** log.* topics carry no sheep identity so server-side filtering isn't possible without a wire change; any client-side subscribe makes lookout the highest-volume bus subscriber and manufactures the exact Dropped/Lagged condition the link exists to survive, worse still because a lag triggers an immediate repair ListFlock - turning log volume into shepherd request load at the worst moment. Reading files is bounded by the reader (one seek + 64KiB read per refresh) not the writer, costs zero bus traffic, and - the decisive extra win - still shows content for a sheep that has already stopped, which a subscription-based feed could never do.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:325`

### The flock table pane both subscribes to the bus AND polls every 2s; poll wins every conflict

tokio::sync::broadcast drops events for a lagging subscriber, so lookout's own live-refreshing table (like bark's dog before it) treats bus events as latency hints only and replaces its map wholesale from a ListFlock poll every 2s, with a Dropped/Lagged event triggering an immediate out-of-schedule poll.

**Why:** The bus is deliberately lossy; a dashboard that only subscribed would go silently wrong under exactly the load that makes watching it worthwhile. This pattern is ported verbatim from the bark dog's own reconciliation code rather than reinvented.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:460`

### The lookout bleats-feed pane reads log files directly; it does not subscribe to log.*

Four candidates were weighed (subscribe log.* with a ring buffer in the UI; subscribe+filter downstream in the link task; ask the shepherd to filter server-side; read the selected sheep's own two log files from disk on the existing 2s poll cadence). The fourth was built.

**Why:** Every subscribe-based option makes lookout the highest-volume subscriber on the bus for a pane that (in 12a) didn't even exist yet, and creates exactly the Dropped/Lagged feedback loop the poll-on-lag repair mechanism exists to survive: under log volume high enough to lag this subscriber, a subscribing design would answer every lag with an extra ListFlock RPC, converting log traffic into request load on the shepherd right when it's busiest. Reading files instead is bounded by the READER (one seek + one 64KiB window per refresh) rather than the writer, produces zero bus traffic, and (bonus) still shows a stopped sheep's history - a subscribing feed would show nothing at all for the exact sheep an operator is most likely to select.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:325`

### The selection cursor is a gutter character ('>'), never a color or reversed-row highlight

Column 0 carries '>' for the selected row; no REVERSED style, no background color change. '>' specifically, not a Unicode triangle glyph like '▸'.

**Why:** A colour-only or reverse-video-only marker would vanish entirely under NO_COLOR or a 16-colour terminal, violating the same rule the whole palette module is built around (decoration is always redundant with something already legible). '▸' was rejected specifically because it's East-Asian Ambiguous-width, so a terminal that renders it double-wide would shift every other column in that one row.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:579`

### Two distinct loss counters (missed_lines vs missed_bytes), because the common case wasn't the byte window

An earlier draft only counted bytes missed below the 64KiB window (the rare 4MB-burst case). The actually common loss is inside the window: Tail::lines holds up to 40 lines/file but the pane renders 5, so a sheep writing 30 lines between polls silently drops 25 with missed_bytes at zero.

**Why:** A pane that looked complete in the common busy-flock case would be lying exactly when someone is watching it. missed_bytes says 'never read' (not 'not shown') deliberately, because the reader genuinely cannot know how many lines are in unread bytes and inventing a line count would be dishonest.

`docs/writing-plans/plans/2026-08-14-shep-phase12b-lookout-panes.md:389`

### When the daemon link is lost and frozen, the reducer stops advancing the on-screen clock, not just the polling

App's Msg::Tick heartbeat arm ignores its own `now` while Link::Lost, so a row's rendered uptime (computed as stored_uptime_ms + elapsed-since-anchor) genuinely stops moving rather than continuing to count up for a sheep the shepherd can no longer see.

**Why:** A dashboard whose banner says "frozen as of 14:32:07" while its UPTIME column keeps ticking is telling a specific, visible lie about a specific process - one keystroke smaller than the equivalent bug in the host strip that phase 12b later fixes the same way.

`docs/writing-plans/plans/2026-08-14-shep-phase12a-lookout-shell.md:682`

## whistle

### Daemon-side refusals are in-band tool errors (CallToolResult with is_error), not JSON-RPC protocol errors

An early draft returned Err(ErrorData) for daemon-side refusals (e.g. ReloadInFlight), which rmcp turns into a protocol-level -32603 error a host may hide from the model entirely. Corrected: daemon refusals return Ok(CallToolResult::structured_error(...)) with the shepherd's own message passed through verbatim; Err(ErrorData) is reserved for genuinely protocol-level failures (unknown tool, bad params).

**Why:** MCP draws this line deliberately - protocol errors are for malformed requests, execution failures belong in-band so the model actually sees and can act on them. The load-bearing promise ('a model reading X can act on it') would silently stop holding if refusals became protocol errors a host is free to swallow.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:836`

### Gated tools are absent (deny by omission), not present-and-refusing

Considered building both routers always and disable_route()-ing the four control tools when the gate is closed (rmcp supports this, and the wire behavior is identical: -32602 tool not found either way). Instead: only the read-only router is built when the gate is closed; the control router is added with `+` (ToolRouter implements Add) only when the gate opens.

**Why:** A model can't be tempted by a tool it can't see at all; an additive design has one fewer thing to get wrong in a future refactor than a filter over an always-live route.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:934`

### Gated-off whistle control tools are absent from tools/list, never present-and-refusing

The read-only router is always built; the four control tools' router is added with `+` only when the config gate is open. rmcp's alternative (disable_route filtering a live, always-built route) was considered and rejected.

**Why:** A model can't be tempted by a tool it can't see; building the control router unconditionally and merely filtering it is one more thing to get wrong in a future refactor, whereas omission when the gate is closed is structurally simpler. The observable client-facing behavior is identical either way (a call to a hidden tool answers -32602 tool-not-found).

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:869`

### metrics-dog.md's research assumed a shared axum HTTP stack and daemon-side-only sysinfo sampling reused by whistle via a dedicated RPC; neither held - **superseded**

The research argued (D2) that shep serve would already put axum+tower-http in shep-cli so metrics' HTTP server would cost nothing extra, and (D4) that sysinfo must live daemon-side only (since per-process CPU% needs a warm, continuously-refreshed System the daemon already runs) with a new Request::Metrics/GetMetrics RPC as the single source both the metrics dog's /metrics scrape and whistle's get_metrics tool would consume.

**Why:** SUPERSEDED on both counts. axum was never added (serve was hand-rolled, per the entry above), so the metrics dog's HTTP server also reuses the hand-rolled http.rs. And no shared Metrics RPC was ever built: the metrics dog runs its own short-lived sysinfo::System in its own CLI process for host stats (sample_host(), pub(crate) precisely because whistle's get_metrics wants the identical sample) and whistle's get_metrics tool independently reuses the existing Request::ListFlock plus that same local host sample - each CLI-side consumer samples for itself rather than routing through a daemon RPC, since a scrape/tool-call is already the 'ask again' event and a fresh short-lived System per call needs no shared warm state across a wire boundary.

`docs/research/metrics-dog.md:103-142 (superseded; verified at crates/shep-cli/src/dog/metrics/mod.rs:94-124 and crates/shep-cli/src/whistle/read.rs:110-125)`

### No --allow-control flag for whistle, unlike lookout - but the stated first-draft reason for this was false and got corrected

First draft argued a CLI flag was refused because 'the launcher writes the argv, so a flag would be too easy to add' vs. an edit to shep.toml being a harder second edit - but SHEP_HOME is itself an env var and CLI flag (GlobalArgs::home), so argv/env already reach the gate exactly like a dedicated flag would. The corrected, surviving reason is legibility (spec §14.7): a boolean in a file has an mtime/diff/review trail; a flag has none and gets pasted from a README and forgotten.

**Why:** Recorded specifically because the plan calls out its own first draft's reasoning as false and replaces it - a good example of a rationale that would otherwise look load-bearing but wasn't.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:701`

### rmcp compiled cost measured at +14 crates (not the estimated ceiling of +344)

An earlier draft predicted the Cargo.lock delta could be as large as +344 packages, reasoning that rmcp's weak-feature-syntax references (reqwest?/rustls etc.) would all get locked. Measured with an actual cargo fetch/resolve: only +14 packages, because cargo only locks a weakly-referenced optional dependency when the feature *containing* the reference is itself enabled, and none of rmcp's reqwest-gated features were turned on.

**Why:** Concrete counterexample used to validate the mechanism: ratatui-termwiz's chain DOES get locked (because underline-color, the containing feature, is enabled) while insta's optional ron does NOT (never enabled) - same syntax, opposite outcomes depending on feature activation.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:306`

### rmcp SDK chosen over hand-rolled stdio JSON-RPC - opposite ruling from `shep serve`

rmcp, the official MCP Rust SDK, was weighed against hand-rolling the stdio JSON-RPC loop and won, on the argument that MCP is a still-evolving protocol where tracking an SDK beats owning a parser. This is the deliberate opposite of the `shep serve` ruling, where axum was rejected in favor of hand-rolling on the HTTP surface the metrics dog already has.

**Why:** Distinguishing principle: a settled, single-endpoint protocol (serve's static file HTTP) favors hand-rolling; an evolving multi-shape protocol (MCP) favors an SDK someone else tracks.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:209`

### Selector grammar deliberately excluded from every whistle tool argument

Every tool that names a sheep constructs SelectorSpec::Name(name) directly rather than running ProcessSelector::parse, so 'all', '/regex/', 'id:' and 'fold:' are not reachable through any tool - a string "all" only matches an app literally named 'all'.

**Why:** One line of code removes an entire class of failure: a model writing a selector that matches far more than it meant to (e.g. accidentally stopping the whole flock).

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:660`

### start_sheep narrowed to already-registered sheep only - full `shep start` power refused as a tool

A start_sheep tool with `shep start`'s full shape (script path/Flockfile/stdin JSON) would be arbitrary code execution as the operator, exposed to a model. Instead start_sheep takes only a name of an already-registered sheep and issues Request::Restart (a spawn for a non-running sheep) - bounded by what a human already registered, unable to introduce new processes or change config.

**Why:** The allow_control gate is explicitly not a security boundary, so it can't be relied on to make a wider start_sheep 'safe'; the blast radius of arbitrary process launch is the whole machine, not just the flock.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:567`

### start_sheep's pre-check is advisory (TOCTOU), documented rather than hidden

whistle's Describe-then-Restart pre-check for start_sheep is two separate requests over two separate connections (per the one-connection-per-call design), so a sheep can come online between them (cron/watch/autorestart/another operator) and Request::Restart doesn't re-check - it always kills+spawns. Closing this needs a wire-level atomic StartIfStopped, which is out of scope (PROTOCOL_VERSION stays 1). idempotent_hint is set to false (not true) precisely because of this race.

**Why:** Rejected treating the hint as describing the intended/common-case operation; the call made is that hints should describe the truth including worst-case interleaving, and MCP's own spec says clients shouldn't trust a server's self-description as gospel anyway.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:587`

### tail_bleats and list_barks tools cap their result at 200 lines; list_flock and get_metrics stay unbounded

The two log-reading tools clamp to a default 50 / hard cap 200; the two flock-shaped tools deliberately return everything with no size limit.

**Why:** An agent's context is finite but a log can be arbitrarily long, so truncating logs is safe; truncating a flock listing would make a model reason about a flock that isn't the one actually running, which is worse than a large reply.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:869`

### Three categories of tool deliberately never built, beyond the spec's own nine

delete_sheep/flush/kill excluded because they're irreversible in a way the four control tools aren't (a model mistaking one for a restart can't be recovered from by asking it to undo). signal_sheep/whisper/trigger excluded because their blast radius is whatever the app does with free-form input, not shep's to bound. scale_flock excluded because a count is easy for a model to be off by orders of magnitude on, and Response::Scaled only lists survivors so a mis-scale-to-1 reads as a success.

**Why:** Recorded so a later reader who's tempted to add one of these understands the reasoning was deliberate, not an oversight of the spec.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:638`

### Tool output reuses the CLI's payload field names/shapes exactly, but not its envelope

facts.rs defines structural twins of ProcessInfo/Bark with a deep-equality test against serde_json::to_value of the real types (not just a key-set check). Rejected wrapping in output::OutputEnvelope's {schema_version, command, data} shape because MCP already has its own envelope (CallToolResult/structuredContent) and nesting one inside the other would couple SCHEMA_VERSION bumps (a promise to jq scripts) to whistle's schema (a promise to MCP clients).

**Why:** Twins-plus-equality-test is the cheaper half of the alternative trade (deriving JsonSchema directly on ProcessInfo, which would put a schema-generation dependency into shep-core for a CLI-only concern).

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:891`

### whistle opens one fresh connection per tool call and closes it; it carries no reconnect ladder

Unlike lookout's long-lived connection with a five-rung reconnect ladder, whistle's shepherd.rs connects, sends, and drops for every single tool invocation.

**Why:** whistle has no screen and nothing to preserve across calls; a shepherd restarted between two tool calls is simply invisible under this design, with no stale handle, ladder, or state machine to test. The cost (one connect+handshake per call on a local unix socket, between calls a model makes seconds apart) is negligible against that simplicity.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:869`

### whistle's stdout carries nothing but the MCP wire; there is no Streams/emit path in that verb at all

main.rs dispatches whistle before any Streams construction, with only a stderr handle passed through; no tracing subscriber is installed at all (rmcp's internal tracing calls go nowhere with no subscriber, which is correct - stdout is a wire, stderr belongs to the launcher's own log).

**Why:** whistle's stdout IS the JSON-RPC transport; a single stray byte (an error envelope, a println!, a tracing record) corrupts the stream and the client's next parse fails on data it can never resynchronize from.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:869`

### whistle's stdout is the wire - nothing else may ever write to it

whistle is dispatched in main.rs taking only a stderr handle, with no Streams value in its path at all, so there's no code path that could call output::emit() to stdout. All diagnostics (malformed shep.toml, fatal transport errors) go to stderr exactly as dog::run_dog does. No tracing subscriber is installed (rmcp's internal tracing records would otherwise go... nowhere useful anyway, but installing one risks stdout contamination).

**Why:** A single stray byte on stdout corrupts the JSON-RPC stream and the client can't resynchronize its parser.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:954`

### whistle's tool output reuses shep-core's payload vocabulary exactly, via structural twin types, never the CLI's JSON envelope

facts.rs defines schemars-derived twins of ProcessInfo/Bark (same field names, same value shapes) with a test asserting deep JSON equality against the real ProcessInfo; the CLI's OutputEnvelope{schema_version, command, data} wrapper is deliberately never reused for MCP tool results.

**Why:** MCP already has its own envelope (CallToolResult/structuredContent with a per-tool schemars-generated output schema); nesting the CLI envelope inside it would mean the tool's declared schema has to describe schema_version and command, fields meaningless to an agent. It would also couple two independent version numbers: SCHEMA_VERSION is a promise to jq scripts, and bumping it for an MCP-only reason (or holding it back for one) would be wrong either way. The twin-type-plus-equality-test approach was chosen over deriving JsonSchema on ProcessInfo itself because that would pull a schema-generation dependency into shep-core for a purely CLI-side concern.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:869`

### whistle.allow_control is read only from the local file at startup, never over the wire - rejected mirroring DogConfig

Considered adding Request::WhistleConfig (mirroring Request::DogConfig) so whistle obeys whatever config the running shepherd loaded. Rejected: it would need a new wire variant (PROTOCOL_VERSION frozen at 1 this phase), a version-skew failure mode where the daemon's connection handler treats an undecodable frame as fatal and closes the connection outright rather than refusing politely, and DogConfig's own reasoning (webhook URLs are bearer credentials) doesn't transfer to a single boolean with no secret.

**Why:** Consequence stated plainly in the docs rather than hidden: the shepherd has no opinion about allow_control and never reads it; editing shep.toml requires restarting whistle, not the shepherd.

`docs/writing-plans/plans/2026-08-14-shep-phase13-whistle.md:772`

## Config and packaging

### .js Flockfiles: flag-gated, never by extension

shep start reads a .js Flockfile only behind an explicit --flockfile flag; directory discovery and bare extension-sniffing never route to it, even though the maintainer's ruling was just "never implicitly".

**Why:** The literal reading ("make .js an extension FlockFormat recognizes") would silently break `shep start server.js`, which since Phase 3 means "run this script" and has a passing test fixture named exactly server.js. A flag is explicit in the strongest sense: the operator typed a word that means evaluate-this. Also: the .js bridge produces a JS-authored Flockfile ({app:[...]}) , not a real pm2 ecosystem.config.js (key is `app` not `apps`, deny_unknown_fields rejects pm2 field names) - `shep import` remains the only pm2 path.

`docs/writing-plans/plans/2026-08-15-shep-phase14-config-packaging.md:305`

### DaemonConfig is not a proof token; fields stay pub

Unlike ResolvedApp (private config field proves normalize() ran), DaemonConfig's fields stay public and validation happens in the layered load_layered path rather than being architecturally enforced.

**Why:** An earlier plan draft claimed #[non_exhaustive] made DaemonConfig::load the only construction path from outside the crate - that's false: #[non_exhaustive] blocks struct literals, not field mutation on a Default-derived value, so a caller can build one and mutate a field past the type system's notice. Re-deriving from scratch: nothing in the codebase holds a DaemonConfig across a trust boundary the way ResolvedApp does, so the property has no consumer, and privatizing ~8 fields to guard one floor (max_cron_sleep) isn't worth the API cost. #[non_exhaustive] stays for field-growth reasons only.

`docs/writing-plans/plans/2026-08-15-shep-phase14-config-packaging.md:519`

### Init detection: runtime probe on Linux only, compile-time everywhere else, plus an always-available --init override

shep startup probes /run/systemd/system then /run/openrc/softlevel at runtime on Linux only (systemd and openrc share one target triple); FreeBSD/OpenBSD stay compile-time since target_os already disambiguates them uniquely.

**Why:** Without a runtime probe, openrc could never be selected on Linux since target_os can't distinguish it from systemd. This is a behavior change (a container with no /run/systemd/system used to get a systemd unit unconditionally; now it's refused) so it ships with an --init escape hatch honored by both startup and unstartup.

`docs/writing-plans/plans/2026-08-15-shep-phase14-config-packaging.md:983`

### schema is generated from RawFlockfile (the document), not AppConfig (one app)

crates/shep-core/assets/flockfile.schema.json is schema_for!(RawFlockfile) with #[cfg_attr(feature="schema", derive(JsonSchema))], committed inside the shep-core package and drift-checked via include_str! + a co-located test.

**Why:** An earlier draft generated the schema from AppConfig and would have shipped an artifact that rejects every real Flockfile (wrong required keys, deny_unknown_fields inverted). Generating from RawFlockfile means there is exactly one declaration of the document grammar, so a field added to one can't drift from the other. include_str! makes the schema a compile-time input so a stale committed file fails cargo test rather than silently rotting; putting the asset at the repo root (an earlier draft's choice) would have broken `cargo publish` since only files under the package dir are packed.

`docs/writing-plans/plans/2026-08-15-shep-phase14-config-packaging.md:264`

### Validate the daemon config exactly once, after file<env<flags are all layered

DaemonConfig::load_layered's floor check (max_cron_sleep) moves into a single validate() called once at the end, not per-layer.

**Why:** Validating per-layer would make a good SHEP_MAX_CRON_SLEEP unable to rescue a broken shep.toml, defeating the whole point of a file<env<flags override chain.

`docs/writing-plans/plans/2026-08-15-shep-phase14-config-packaging.md:553`

## serve, dev and runtime

### Path resolution order: split on '/' first, THEN percent-decode each segment

resolve() cuts the query/fragment, splits on raw '/', and only then percent-decodes each segment - decoding the whole target before splitting would let %2f manufacture separators after the traversal check already ran.

**Why:** Decode-then-split is the classic path-traversal bypass (`..%2f..%2fetc%2fpasswd`). A ':' is refused in every decoded segment unconditionally, including on Windows compile targets, because PathBuf::push("C:") replaces the whole base path - the resolver is compiled and tested on Windows even though the fs half is unix-only.

`docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:686`

### PID 1 forks a dedicated init process; the in-process waitpid loop can't work

shep runtime, when it detects std::process::id()==1, splits into a tiny init (forwards SIGTERM/INT/HUP/QUIT, reaps via waitpid(-1,WNOHANG)) that spawns the real supervisor as its child with --supervise.

**Why:** tokio::process already reaps its own child via waitpid(<pid>,WNOHANG) on SIGCHLD; a blind waitpid(-1,WNOHANG) loop in the SAME process races it and sometimes wins, making child.wait() return ECHILD instead of the real exit status - breaking the exact promise spec makes about exact exit-code/signal recording. The init's own exit funnels through std::process::exit directly (the crate's only such call site) because the status being relayed is the child's, not shep's own ExitCode taxonomy - forcing a foreign status through that closed enum is the category error, not bypassing it.

`docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:1111`

### runtime's empty-flock exit is 0 for a clean stop, 11 (new code) for any errored sheep

shep runtime exits 0 when every app stopped cleanly and none is `errored`; exits the new ExitCode::FlockEmpty (11) only when a sheep ended errored. pm2-runtime exits nonzero unconditionally on emptying; shep does not follow that.

**Why:** Code 2 was already claimed by clap for usage errors, colliding with the originally-planned fail-fast code; a new code was added rather than reused because the spec's own rule is that distinct causes get distinct codes. Exiting 0 on a clean batch-job emptying (autorestart=false or a matching stop_exit_codes) matters for container orchestrators deciding whether to restart.

`docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:1077`

### serve is hand-rolled on the existing HTTP surface, not axum

shep serve reuses crates/shep-cli/src/dog/http.rs (moved to crate root), extended with header-writing and streaming, rather than pulling in axum+tower-http as spec §9 literally named.

**Why:** The maintainer's binding ruling: serve is genuinely simple over code shep already owns, while an evolving protocol like MCP (rmcp/whistle) is worth an SDK - the same session that overruled axum for serve upheld rmcp for whistle. Directly reverses the earlier research doc (serve-import.md) which had recommended axum+tower-http.

`docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:486`

### serve refuses every symlink component by default; canonicalize-then-compare is rejected as the default

contain() walks path segments and refuses ANY symlink component (intermediate or leaf) rather than canonicalizing the resolved path once and checking starts_with(root). --follow-symlinks opts back into the canonicalize-and-check design, with a startup notice naming the race it reopens.

**Why:** A per-request canonicalize-then-open has an unavoidable TOCTOU window between the check and the open; per-component O_NOFOLLOW+symlink_metadata during the walk closes it with zero new dependencies and no unsafe. The tradeoff is deliberate: an ordinary deploy layout with a symlinked assets/ or a `current -> release-N` symlink now 404s by default - refusing more than "leaves the root" is accepted because a wrong refusal is loud and one flag away from fixed, while a wrong permission (file disclosure) is silent.

`docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:621`

### serve-import.md's research recommended axum+tower-http and a ConstantTimeEq-based creds check; both were overruled by the maintainer when serve actually shipped - **superseded**

The 2026-08-07 research doc recommended axum 0.8.9 + tower-http's ServeDir for path resolution/MIME/range support, and `subtle::ConstantTimeEq` + sha2 for the basic-auth compare.

**Why:** SUPERSEDED. When serve actually shipped (Phase 15), the maintainer explicitly overruled axum in favor of hand-rolling on the existing HTTP surface (her stated reasoning: serve is genuinely simple over code shep already owns, while an evolving protocol like MCP is worth an SDK), and the auth compare used ring::constant_time::verify_slices_are_equal + SHA-256 digest-first (already in the tree via tokio-rustls) rather than adding subtle+sha2 as new dependencies. The gap between this research and the shipped design is itself informative: hand-rolling can beat a well-regarded crate specifically when the crate is already fully present for an unrelated reason.

`docs/research/serve-import.md:14-33 (superseded by docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:486,887)`

### shep-cli becomes a library exposing exactly three functions

crates/shep-cli/src/lib.rs's whole public surface is main(), main_runtime(), main_dev() returning std::process::ExitCode; every module stays private.

**Why:** shep-cli is published to crates.io, so anything pub is a semver promise; internals move every phase (dog/ became a directory, http.rs moved twice). A full API would also invite "let me embed shep's clap tree", which the crate structurally can't support (cfg(unix)-gated dispatch, assumes it owns process exit) - the answer to wanting an embedding API is shep-client, which already exists for that purpose. Verified with `grep -c '^pub '` == 3.

`docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:348`

### Three real [[bin]] targets, not argv[0] busybox-style dispatch

shep-runtime and shep-dev are real separate binaries (not one binary hardlinked under different names reading argv[0]), each prepending its own verb to the argument vector before dispatch - except when the first argument is exactly `daemon` or `dog`, since those two re-exec vectors are constructed by the supervisor itself at exactly three call sites and must not be double-prefixed.

**Why:** argv[0] dispatch needs basename parsing and gives a wrong answer the moment someone copies the binary under a different name; three real [[bin]]s know what they are at compile time. The daemon/dog carve-out exists because shep_daemon::dogs and launch::launch_command construct those vectors directly against current_exe() - under shep-runtime that's the alias binary, so a naive always-prepend would turn `dog metrics` into `runtime dog metrics`, a clap error, killing every enabled dog in that container.

`docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md:406`

## Output and first run

### Boxed table column-drop floor is 3 columns (ID/NAME/STATUS); dropped columns are named in a footer

render_boxed drops columns by descending priority until the table fits the terminal, never below the three that identify a sheep, and always states what was hidden plus how to see it (`--format json`).

**Why:** A table that can't say which sheep a row is about has stopped being a table. A silently-vanishing column is worse than one openly named as hidden.

`docs/writing-plans/plans/2026-08-18-pretty-cli.md:800`

### Default $SHEP_HOME is auto-created; an explicitly-named missing home is refused, never created

ensure_home_at creates ~/.shep silently on first use, but a --home/$SHEP_HOME path that's missing is refused with a message pointing at `mkdir -p` and at dropping back to the default.

**Why:** ~/.shep is a name shep chose, so shep may conjure it; an operator-typed path is more likely a typo than intent, and silently creating it would produce a second, empty, invisible flock whose bug report reads as "shep lost all my processes" when the truth is "you're looking at a different flock". Uses DirBuilder::new().mode(DIR_MODE) at creation, not create_dir_all+chmod, to avoid a window where the directory exists world-readable.

`docs/writing-plans/plans/2026-08-17-first-run-experience.md:178`

### Sheep decoration never appears on error output or after a destructive verb

flourish.rs's empty/all-stopped/mustered sheep art is confined to exactly three success-path moments and explicitly excluded from any error or destructive-verb output.

**Why:** docs/terminology.md already rules that the theme never costs clarity; a sheep face beside `error[not_found]` makes a failure harder to read and reads as flippant to someone debugging at 2am.

`docs/writing-plans/plans/2026-08-18-pretty-cli.md:1102`

### shep --help is hand-templated into task-shaped groups; clap 4.6 can't group subcommands

A hand-written HELP_TEMPLATE const with nine named groups (Run things, See what's up, Survive reboots, ...) replaces clap's alphabetical listing, guarded by two tests: one enumerates the real command tree and fails if any visible verb is unfiled, the other pins the literal template text against the same HELP_GROUPS table so the two can't silently disagree.

**Why:** #[command(help_heading=...)] on a subcommand variant does not compile under clap 4.6.6 (verified), so grouping has to be hand-rendered rather than declared, which is exactly the kind of hand-maintained list that rots the first time someone adds a verb - hence the drift test.

`docs/writing-plans/plans/2026-08-17-first-run-experience.md:1080`

### StyleLevel is one dial (full/plain/bare), not three independent switches for color/boxes/sheep

A single enum controls colour+boxes+sheep together rather than three orthogonal flags; NO_COLOR stays a separate, orthogonal override since it's a cross-ecosystem convention about colour alone.

**Why:** In practice nobody wants sheep with a flat table, or colour with today's plain output - the tastes travel together, so one dial covers the real cases with less surface. An unparseable $SHEP_STYLE falls through to the next config layer rather than erroring, since a shell-profile typo shouldn't make every shep command unusable.

`docs/writing-plans/plans/2026-08-18-pretty-cli.md:150`

### Table cell width measured in visible characters (ANSI-stripped), not bytes or grapheme clusters

visible_width() strips CSI escape sequences and counts remaining chars; it deliberately does NOT account for east-asian double-width or grapheme clustering, and no unicode-width dependency is added.

**Why:** Padding by len() or chars().count() on a styled cell (colour escapes included) pushes every later table border to the right - three hand-drawn mockups made exactly this mistake during design. The floor at char-count rather than display-width is deliberate: shep names are operator-chosen ASCII-typical identifiers, a real display-width dependency is unjustified for a case nobody has hit, and a property test will catch it the day someone does.

`docs/writing-plans/plans/2026-08-18-pretty-cli.md:578`

### The first-run welcome prints to stderr (suppressed under --format json / non-terminal); `shep welcome` itself prints to stdout unconditionally

on_first_run fires as a side effect on whichever command created the home, writing to stderr and skipped entirely when Format::Json or stderr isn't a terminal - but the home is still created either way. `shep welcome` as an explicit verb prints the same text to stdout with no terminal check and answers --format json with a real envelope.

**Why:** A provisioning script running `shep start server.js | jq` on a cold machine must get clean stdout; suppression governs the diagnostic text, never the side effect (the home still gets created). An explicitly-invoked verb that printed nothing under --format json would read as broken, so the spec's suppress-under-json rule was deliberately narrowed to the side-effect path only.

`docs/writing-plans/plans/2026-08-17-first-run-experience.md:900`


## Config overrides

### A Flockfile load is additive by default: the file may add, and may never overwrite

`shep start <Flockfile>` merges the file into the sheep of the same name rather than replacing it. A key the file declares that nobody has established yet takes the file's value; every other key keeps exactly what it has, defaults included. `--reset` widens that to every setting but `env`, and `--reset-all` to everything.

**Why:** The alternative, letting a load overwrite whatever it found, is the shape a laptop wants and a host cannot have. A Flockfile arrives through the app's own repository, so overwrite-by-default would make a merged pull request a way to change a running flock's config out from under the operator who tuned it, and the operator would learn about it from the incident rather than from the diff. Append-only makes the worst case of an unreviewed merge "nothing happened". Rejected alongside it: making `--reset` the default and requiring a flag to be additive, which puts the safe behaviour behind a flag nobody types under pressure.

An unrecognised `ResetDepth` from a newer client falls back to additive for the same reason: append-only is the depth that cannot destroy something an operator set.

`verified crates/shep-daemon/src/supervisor.rs (merge_declared) and crates/shep-core/src/config/apply.rs (ResetDepth)`

### env is data and everything else is policy, which is where --reset stops

`--reset` puts every setting but `env` back to the template. `--reset-all` is a second flag rather than an argument to the first.

**Why:** Resetting policy is recoverable and resetting data is not. A `--reset` that also cleared `env` would take an app's database credentials away as a side effect of putting its restart budget back, and the operator asking for the second thing is almost never asking for the first. Two flags, because a single flag with a modifier reads as one act with a switch rather than as two different sizes of act.

`verified crates/shep-core/src/config/apply.rs (ResetDepth::Settings vs ResetDepth::All)`

### rearm_name is a force-replacing sibling to ExtrasRegistry::arm, not a flag on it

A config change to a lifecycle extra rebuilds the whole name-group's tasks through `rearm_name`, rather than calling `arm` again.

**Why:** `arm` deliberately PRESERVES a live cron or watch task, so a reload's replacement instance arming before the drainee disarms does not tear down a watcher the drainee still needs. That is right for the transition it was written for and exactly wrong for a config change: the group-scoped fields are read when the task is built, so a preserved task keeps the old values for as long as it lives. Adding a "replace" flag to `arm` was rejected because the two callers want opposite things from the same word, and a boolean at the call site is how that gets read backwards a year later. What it costs is recorded at the function: the OS watch is torn down and rebuilt with a real gap and no rescan, and the CPU baseline clears for one poll interval.

`verified crates/shep-daemon/src/extras.rs (rearm_name's own doc, "What this loses")`

### A liveness epoch on the extras registry, separate from the supervisor's respawn epoch

`ExtrasRegistry` counts an epoch per id, bumped every time an id is armed, and a `LivenessReport` carrying a stale one is dropped.

**Why:** A config-only re-arm replaces a liveness probe without the process underneath it changing at all, so the supervisor's existing pid guard cannot see it: the old probe's in-flight failure would restart a sheep whose probe had already been replaced. The respawn epoch could not be overloaded for this, in either direction. Bumping it on a config change would move a respawn-generation counter without a respawn; leaving it alone would leave a config-only re-arm unable to move it. So a second counter, and it lives on the registry because that is the one type that knows when a probe is actually replaced.

`verified crates/shep-daemon/src/extras.rs (ExtrasRegistry::liveness_epochs)`

### PROTOCOL_VERSION stayed 2 for Request::ApplyConfig, and the skew is louder than the six precedents

`Request` gained an `ApplyConfig` variant and `Response` an `Applied`, additively, with no version bump. Six prior additions in shep-core's changelog set that precedent.

**Why:** The rule the constant answers is whether an older peer can still be understood, and an added variant that an older client never sends does not break one. Bumping would refuse every older client for every verb to improve the error message for one. What is sharper here than in the six precedents: a NEW CLI against an OLDER daemon passes the handshake, sends `ApplyConfig`, and the daemon ends the connection on an envelope it cannot decode, so `shep start <Flockfile>` fails on a dead client rather than on a named version refusal. The remedy is `shep daemon reload` after upgrading, which is why it is now said in the docs rather than left to be discovered.

`verified crates/shep-core/src/protocol/mod.rs (PROTOCOL_VERSION = 2) and crates/shep-core/CHANGELOG.md`

### PROTOCOL_VERSION moved to 3 anyway, once the payload inside ApplyConfig stopped being purely additive

The entry above was right about `ApplyConfig` itself: the variant is additive, and that half of the reasoning stands. It stopped being the whole story when `ResetDepth::Settings` was renamed to `ResetDepth::Policy` (with `File` and `Env` added alongside it). A rename is not an addition: `"settings"` was the wire spelling of the `--reset` flag already shipping, so an older daemon that decoded it correctly today loses that ability, not merely a capability it never had.

**Why:** The six precedents, and `ApplyConfig` itself, all have the shape "a new variant an old daemon was never going to receive": old traffic keeps working, only a brand-new capability is unreachable until a restart. A rename breaks the OLD traffic too: a fresh CLI sending what used to be an ordinary `--reset` against a not-yet-restarted daemon now fails to decode, on functionality that worked yesterday. `PROTOCOL_VERSION` exists to turn exactly that into a named handshake refusal instead of a dead connection, and the six-precedent argument for leaving it alone does not reach a case where existing behavior regresses. `shep daemon reload` is still the whole fix; the difference is that skipping it now gets a named `version_skew` error instead of a decode failure with no diagnosis.

`verified crates/shep-core/src/protocol/mod.rs (PROTOCOL_VERSION = 3) and the request_wire snapshot renamed from v2 to v3`

### The two "reload does not re-read config" entries are about shep reload <sheep>, not shep daemon reload

`shep daemon reload` re-reads `shep.toml`, and always has. The superseded entries under Reload above are about `shep reload <sheep>` and a Flockfile, which are different files read by a different verb.

**Why:** Recorded explicitly because this is the confusion most likely to produce a wrong doc later: two entries with the word "reload" in the heading, asserting that a reload reads no config, sitting a search away from anybody asking whether the shepherd re-reads its own config file. It does. Nothing in this work changed that, and nothing about the Flockfile side is evidence about it. `getting-started.astro`'s upgrading section now says so where an operator reads rather than only here.

`verified crates/shep-cli/src/commands/daemon.rs (the pre-flight validates shep.toml before the reload) and web/src/pages/docs/getting-started.astro`

### --reset and --reset-all become one flag, --reset=<mode>, with four values rather than a two by two grid

`ResetDepth::{Settings, All}` and the two boolean flags behind them are gone. `--reset=<mode>` replaces both, required with an equals sign, and the four values are `file`, `policy`, `env`, `all`. A mode touches only what its name says: `file` puts back what the template declares and nothing it is silent on; `policy` widens that to every key, declared or not; `env` touches only `env`; `all` is both.

**Why:** The original two-flag design read env and everything-else as a single axis with two stops, which made `--reset` restore every non-env setting whether the template declared it or not. That default is the exact footgun this rename exists to fix: an app stocked to four instances against a Flockfile with no `instances` line dropped to one, because the compiled default won an argument the file never entered. `file` is the mode with nothing to put back in that case, so the count survives. The other gap the two-flag design left was an operator who wanted their env restored without losing policy tuning; `--reset` and `--reset-all` both meant "restore policy" was inseparable from "restore env" once env was in scope at all, and `env` is the mode that separates them. Six combinations exist across the two real axes (env: keep or reset; policy: keep, reset-declared, or reset-everything), and the four kept are the ones an operator would ask for. One discarded combination resets nothing at all, so it is the additive default with extra typing; the other is `file` plus `env`, coherent but left out because nobody has asked for it. `PROTOCOL_VERSION` moved from 2 to 3 for this, recorded above, because the rename changes the wire spelling of an operation that already shipped.

`verified crates/shep-core/src/config/apply.rs (ResetDepth), crates/shep-cli/src/cli.rs (ResetMode) and crates/shep-daemon/src/supervisor.rs (merge_declared)`

## Dog config store

### A dog's section moved out of `shep.toml` into a hand-editable `dogs.toml`, migrated once at boot, and a name in both files is a refusal rather than a merge

A dog's own settings now live under `[<name>]` in `$SHEP_HOME/dogs.toml` instead of under `[dog.<name>]` in `shep.toml`. The daemon migrates any old sections into the new file once, on the first boot that carries the change, and prints which dogs moved. `RawDaemonConfig::dog` stays on the old type on purpose, so an un-migrated `shep.toml` still parses instead of refusing to boot under `deny_unknown_fields`.

**Why:** Making `dogs.toml` hand-editable, the same way `shep.toml` is, means an operator can write a section into it before ever upgrading, so a migration that merged silently would have to pick a winner between two values for the same key with nothing to go on. Refusing is the only answer that does not guess, and it costs nothing but an edit: the fix is deleting one of the two sections and starting the daemon again. This was not in the original spec for the move; it came out of writing the migration itself, once the hand-editable file made the collision possible.

`verified crates/shep-core/src/config/dogs.rs (DogsConfig) and crates/shep-cli/src/commands/dog_migration.rs (migrate_dog_sections, DogMigrationError::WouldOverwrite)`
