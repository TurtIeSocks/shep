# Decision briefs — map.md open items 2, 3, 4 (+ sheep architecture, + Windows)

Written 2026-08-07 at Rin's request. Work sizes: S (<1 day), M (days), L (week+).

**Status:** #2 DECIDED (A+D v1, npm shim v1.1, no Node-IPC emulation). #4 DECIDED
(polling behind `LimitEnforcer` trait v1; cgroup v2 feature v1.1). #3 evolved into
the pluggable-sheep architecture question — analysis below, Rin's call.

## #2 — Node-app readiness/metrics shim

**The problem.** pm2 injected itself into Node apps, so `process.send('ready')`
(zero-downtime gating), custom metrics, and custom actions came "free". shep
spawns plain processes — apps need SOME channel to talk to the shepherd. The
question is what shape it takes.

| Option | How | Pros | Cons | Work |
|---|---|---|---|---|
| A. fd-pipe protocol only | shep opens extra pipe fd to every child; newline-JSON messages (`{"kind":"ready"}`, `{"kind":"metric",...}`) | Language-agnostic (bash can `echo >&3`); zero packages to maintain; one protocol for everything | Each app hand-writes 3 lines of JSON; Node devs used to `process.send('ready')` must adapt | S (protocol already in map: spawn.rs IPC pipe) |
| B. A + tiny npm shim (`@shep/io`) | Wraps option A: `shep.ready()`, `shep.metric()`, `shep.action()` | Nice Node DX; sheep-branded, not pm2-flavored; trivially thin | A package to publish/maintain/version; only helps Node | S on top of A |
| C. Emulate Node's IPC channel (`NODE_CHANNEL_FD`) | shep speaks Node's own IPC framing so unmodified `process.send('ready')` works | Existing pm2-style apps run untouched | pm2-baggage-shaped (contradicts decision 7); Node-internal protocol, no stability guarantee; helps only Node; most work | M |
| D. Probe-based readiness only | No app cooperation: HTTP/TCP/exec health probes decide "ready" | Zero app changes for any language; probes are wanted anyway for liveness | Metrics/actions still need a channel; polling less crisp than an explicit signal | M (probe engine — but it's on the missing-features list regardless) |

**Recommendation: A + D in v1** (fd protocol + probes; probes double as the
health-check must-have). **B in v1.1** if Node users ask — it's an afternoon.
Skip C entirely per decision 7.

## #3 — Modules system

**The problem.** pm2 modules = npm packages pm2 installs and supervises
(pm2-logrotate, pm2-server-monit...). Trace verdict: the top modules exist to
patch gaps shep fills natively — log rotation (core policy), metrics
(Prometheus native), alerts (bark native). What remains is generic third-party
extensibility.

| Option | Pros | Cons | Work |
|---|---|---|---|
| A. Cut entirely | Zero work/surface; "extension" = just another managed process in the Flockfile — shep is literally a tool for running such things; supply-chain risk = zero (pm2's installer was unauthenticated wget + npm, a flagged vuln class) | No curated ecosystem story; no `shep install foo` one-liner | none |
| B. Archive-based modules (fetch → verify → unpack → register → autostart) | Real ecosystem potential; portable (not npm-bound) | Registry/signing/config-namespace design; supply-chain surface we must secure properly; maintenance forever | L |
| C. Defer: design nothing, reserve the concept | Decides nothing badly | — | none |

**Recommendation: A for v1** (with C's spirit: nothing in the architecture
blocks adding B later — a module would just be a managed app + config sugar).
The whistle (MCP) + bark (webhooks) already cover the integration use cases
agents and alerting tools actually want. Revisit only on concrete demand.

## #4 — cgroup v2 enforcement

**The problem.** `max_memory_restart` today = poll RSS every 30s, restart on
breach. Misses fast OOM spikes; the kill races the kernel OOM killer. Linux
cgroup v2 gives kernel-enforced `memory.max`/`memory.high`, `cpu.max`, plus
`cgroup.kill` (atomic whole-tree kill — strictly better than pgid kill).

**Positives:** exact + race-free limits; catches spikes polling can't;
`cgroup.kill` upgrades tree-kill; PSI pressure signals for smarter restarts;
per-app CPU caps become possible (pm2 never had them).

**Negatives:** Linux-only — polling path must exist anyway for macOS/BSD, so
it's a second code path, forever; needs cgroup delegation (trivial as root or
under systemd `Delegate=yes`, fiddly for unprivileged non-systemd setups —
graceful degrade to polling required); container-in-container quirks; more
sys.rs surface (though cgroupfs is plain file writes — no unsafe).

**Work:** M. Create scope + move PID + write limits ≈ file I/O; monitoring via
`memory.events` inotify. The real cost is the capability-detection/degrade
matrix and testing it.

**Recommendation: v1.1, prepped in v1.** v1 ships the portable polling
enforcer behind a `LimitEnforcer` trait (S — just a seam, IR-33 wants the
trait anyway for the fake in tests); cgroup v2 becomes a second impl behind a
`cgroups` feature in v1.1, opt-in per app (`enforce = "kernel"`). Nobody waits
on Linux plumbing to ship v1; the seam makes v1.1 additive.

## #3b — First-party plugin architecture (must-haves as pluggable helpers)

**DECIDED (Rin, 2026-08-07): ship this model, renamed to DOG** — `dog/dogs`
for first-party plugin processes; `sheep` stays reserved for managed user
processes. Section below predates the rename ("sheep model" = dog model).

Rin's question: what would the must-haves look like as pluggable sheep instead
of native daemon code? Her pm2-module pain: archaic install, archaic config.

**Sorting the must-haves first.** Two are already client-side and unaffected:
the TUI (lookout) and MCP (whistle) live in the `shep` binary as subcommands —
pluggable by nature. Daemon config is core, can't be a module. The genuinely
sortable pair: **metrics exporter** and **bark (webhooks)** — do they run IN
the daemon, or as processes the daemon manages?

**The sheep model.** A sheep = a shep-client consumer: it connects to the
daemon socket, subscribes to the event bus and/or polls monitoring RPCs, does
its one job. First-party sheep ship inside the same multi-call binary as
hidden subcommands (`shep sheep metrics`, `shep sheep bark`). Enabling one =
`shep enable metrics` → writes daemon config → daemon starts/supervises it
like any app (tagged `internal` so the flock listing can badge it).

Both pm2 pain points die by construction: **install** = nothing (it's already
in the binary you have); **config** = a typed, documented section in the one
daemon TOML (`[sheep.metrics] port = 9615`), not a KV grab-bag.

| | Native (in-daemon) | First-party sheep |
|---|---|---|
| Daemon reliability | webhook/HTTP bugs run inside the supervisor | crash isolation — a panicking exporter can't drop the shepherd; supervisor's only job stays supervising |
| Footprint | one process | +1 small process per enabled sheep (~few MB each) |
| Restart/upgrade | daemon restart touches everything | sheep restart independently; daemon untouched |
| Wire protocol | can cheat with internal access | becomes a hard API early (we planned stability tests anyway — forcing function, arguably good) |
| Metrics fidelity | in-process counters | sampled over RPC — fine at monitoring cadences (1-30s) |
| Alert delivery | direct bus access | bus subscriber; MUST handle bounded-queue drop notices + reconcile by polling (design note) |
| Third-party story | needs a module system (L, supply chain) | free: any binary speaking the client protocol is a sheep; same enable flow, zero new infrastructure |
| Opinionatedness | opinions compiled into the daemon | core unopinionated; opinions opt-in |

**Cargo features still exist** — but for build-slimming source builds, not
runtime pluggability. The release binary compiles everything; runtime opt-in
comes from the process model. (Feature-flags-as-the-plugin-mechanism is the
weaker version: no crash isolation, no independent restarts, and one release
binary means the flags are inert for most users anyway.)

**Recommendation: sheep model.** Core native: supervisor, RPC, bus, config,
KV, log capture (inherently in-daemon). Sheep: metrics, bark, future
log-shipper/etc. TUI + MCP stay client subcommands. This deletes #3's module
system permanently — the sheep contract IS the extension story. Marginal cost
over native ≈ the enable/disable UX + treating the wire protocol as stable
from day one (already the plan).

## Windows scope (goals q2)

Rin leans take-it-up-front; fallback = architecture-ready if cost is huge.

**Cost of full first-class Windows in v1:** ≈ +30-40% on the daemon's
process-control layer + CI/testing burden. Pieces: named pipes for RPC
(tokio named-pipe / interprocess — M), Job Objects for kill-tree (windows
crate, well-documented — M), no Unix signals → graceful stop needs
GenerateConsoleCtrlEvent or a pipe-message handshake (annoying — M),
`windows-service` integration (M), daemonization (our re-exec model is
already portable — free), host metrics via sysinfo (free).

**Recommendation — middle path, weighted toward Rin's instinct:**
- **v1 structural:** platform abstraction from day one (`ProcessControl`
  trait, unix + windows impls; no raw unix-isms in core types — typed
  `StopSignal`, no i32 signals on the wire; paths via `dirs`). Windows CI
  compiles + runs unit tests from day one — catches unix-isms the moment
  they're written, which is where retrofit cost actually comes from.
- **v1 functional:** named pipes + Job Objects implemented (the two
  load-bearing primitives) — `shep start/stop/list/logs` works on Windows.
- **v1.1 polish:** service integration, ctrl-event graceful-stop handshake,
  full e2e suite green on windows-latest.

This captures the retrofit-avoidance (the expensive part is unix assumptions
leaking into core, not the Windows code itself) without gating v1 on Windows
e2e polish.

## Cluster parity (goals q1) — DECIDED

v1: N fork instances + SO_REUSEPORT load balancing. True cluster parity
(fd-passing / LISTEN_FDS protocol) = v1.1/v1.2 per Rin.

**Caveat added 2026-08-09 (measured):** "load balancing" holds on Linux
(kernel spreads new connections across siblings) but not on macOS, where
`SO_REUSEPORT` is last-binder-wins — cross-process over 40 connections,
macOS sent 40/40 to the newest binder, Linux split 20/20. macOS is tier 1
(spec §11), so this is a real v1 gap for the cluster model specifically,
not a v1.1 deferral; it does not touch reload, which wants last-binder-wins
behavior. Now documented in spec §4 and §11 rather than only in the trace
notes.

## Research decisions (from Phase 4-6 design research, 2026-08-07 — PENDING Rin)

1. **MSRV 1.85 → 1.88 — DONE (forced now, not future).** Originally framed as a
   Phase-4 concern (ratatui/sysinfo/rmcp). Phase-2a Task 7 review found it is
   already forced: **serde-saphyr 1.0.1 (a current shep-core dep, Rin-approved)
   uses let-chains (stable 1.88) + `is_multiple_of` (1.87)** — no `rust-version`
   declared, edition 2024, and neither 1.0.0 nor 1.0.1 avoids it. The `1.85` pin
   was already a lie and the CI 1.85 legs were red. Bumped workspace
   `rust-version` → 1.88 and CI matrix `1.85` → `1.88` (2026-08-07). Cost: shep-core/
   client advertise 1.88 as their published-lib MSRV. **Reversible** — Rin can lower
   it only by reverting serde-saphyr, which she already chose over the alternatives.
2. **Readiness-probe failure on normal start** (not reload): pm2-compatible
   "online-with-warning at listen_timeout" vs strict "errored". Recommend the
   pm2-compatible behavior (less surprising for migrators). Phase 4 decision.
3. **`shep-client` subscription stream shape** → `ClientEvent { Bus(BusEvent),
   Disconnected, Reconnected }` as a named stream struct (IR-15), so the lookout
   TUI (and any consumer) gets reconnect UX. Low-risk, clear win — **baking into
   the Phase 2b/client plan unless Rin objects.**
4. **Additive `GetMetrics` RPC** (metrics dog + whistle both consume it; keeps
   protocol v1 per the evolution rule). Lands with the metrics phase; noted so 2b's
   RPC dispatch leaves room. Non-decision, just tracked.

## Parking lot (v2 ideas — logged, not scoped)

- **HMR/bacon-style dev loops** (Rin, 2026-08-07): don't bind to any Rust HMR
  lib (space immature: hot-lib-reloader, dioxus subsecond, dexterous). Two
  generic hooks instead: `watch_action = restart | signal | command` (signal
  lets HMR-equipped apps hot-patch without dying) and `shep dev` build
  pipeline (`dev.build_command` — watch → build → restart-on-success).
