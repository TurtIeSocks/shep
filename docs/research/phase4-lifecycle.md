# Phase 4 research — lifecycle extras: watch, cron, memory limit, probes

Scope: spec §4 (watch, cron_restart, max_memory), §7 (probes + readiness), reload
AwaitReady gate (§4 reload). Network was up 2026-08-07; all versions verified
against crates.io that day.

## 1. Crate choices (exact versions, crates.io 2026-08-07)

| Crate | Version | Cargo line notes (IR-2) |
|---|---|---|
| `notify` | **8.2.0** | `default-features = false, features = ["macos_fsevent"]` (its only default; target-gated, harmless on Linux). Alternative `macos_kqueue` not needed. |
| `notify-debouncer-full` | **0.7.0** | no features. Floors: notify ^8.2.0, notify-types ^2.0.0, file-id ^0.2.3. Optional crossbeam/flume backends — skip both. |
| `globset` | **0.4.20** | already in workspace deps as `0.4`. |
| `croner` | **3.0.1** | `default-features = false` (serde feature exists; not needed). Normal deps: chrono ^0.4.42, derive_builder, strum. |
| `chrono` | **0.4.45** | `default-features = false, features = ["clock", "std"]`. Floor ^0.4.42 forced by croner — add a floor-pin comment for the minimal-versions row (IR-44). |
| `chrono-tz` | **0.10.4** | `default-features = false, features = ["std"]`. croner takes any `chrono::TimeZone`; chrono-tz is our IANA-name resolver (`"Europe/Oslo".parse::<Tz>()`). It is only a *dev*-dep of croner — we carry it ourselves. |
| `sysinfo` | **0.39.6** | `default-features = false, features = ["system"]`. Do NOT take defaults (disk/network/component/user = dead weight) and do NOT take `multithread` (pulls rayon into the daemon). |
| HTTP probe client | **none** | hand-rolled HTTP/1.1 GET over `tokio::net::TcpStream` — see decision D1. Fallback if overruled: hyper 1.11.0 + hyper-util 0.1.20 + http-body-util 0.1.4. Rejected: reqwest 0.13.4 (tower/rustls tree vs single-digit-MB RSS goal, spec §14.11), minreq 3.0.0 / ureq 3.3.0 (blocking; timeout-cancel semantics fight tokio). |

All are shep-daemon deps except croner + chrono + chrono-tz, which shep-core also
needs for config validation (see D2). MSRV: everything above builds on 1.85.

### croner 3.0.1 API survey (verified via docs.rs + README)

- Parse: `Cron::from_str(pat)` for defaults; configured parse via
  `CronParser::builder().seconds(Seconds::Optional).dom_and_dow(true).build().parse(pat)`.
  Use `Seconds::Optional` — that is the JS-croner dialect (map.md compat promise:
  optional 6th seconds field prepended). `dom_and_dow(true)` only if we want AND
  semantics; default OR matches JS croner — **keep default OR**.
- Next occurrence: `cron.find_next_occurrence(&DateTime<Tz>, inclusive: bool)` —
  generic over any `chrono::TimeZone`, so `chrono_tz::Tz` plugs straight in.
  `find_previous_occurrence` and `CronIterator` also exist.
- Dialect extras: `L`, `W`, `#`, `?`, `5#L`, `+` — richer than pm2's node-cron; fine.
- **DST (documented behavior, croner `JobType` rules):** fixed-time jobs (concrete
  hour/minute) that fall in a spring-forward gap fire at the first valid moment
  after the gap; wildcard/interval jobs (`*/N`) skip gap occurrences and resume on
  the new wall clock; on fall-back, fixed jobs fire once (no double-fire). This is
  exactly the contract we want — pin it with tests (US 2026 transitions:
  2026-03-08 spring forward, 2026-11-01 fall back, `America/New_York`).

### notify-debouncer-full 0.7 API survey

`new_debouncer(watch_delay, None, handler)` → `Debouncer` guard (stops on drop);
`debouncer.watch(&path, RecursiveMode::Recursive)`. Handler gets
`DebounceEventResult = Result<Vec<DebouncedEvent>, Vec<Error>>` **on the
debouncer's own OS thread** — bridge to tokio with
`tokio::sync::mpsc::UnboundedSender::send` (non-blocking, thread-safe).
`RecommendedCache` file-id cache by default (rename tracking) — keep it.

### sysinfo 0.39 cost model (memory poll)

`System::refresh_processes_specifics(ProcessesToUpdate::All, true,
ProcessRefreshKind::nothing().with_memory())` then `Process::memory()` (bytes),
`Process::parent()` for tree building. Must refresh **All** — refreshing only
known pids can't discover newborn lambs. Memory-only refresh of the full table is
one `/proc/<pid>/stat(m)` read per process on Linux, `proc_pidinfo` on macOS:
low-single-digit ms for a few hundred processes, negligible at 15s cadence.
Criterion bench (IR-5) backs the const comment; the map.md "procfs Linux hot
path" stays deferred until that bench proves a need (KISS).

## 2. Module shape (shep-daemon)

```
crates/shep-daemon/src/
  watch/
    mod.rs      WatchGroup task: globset filter, restart trigger, queued-event
                re-check. Pure-async over an mpsc<Vec<PathBuf>> — paused-clock testable.
    source.rs   OS seam: notify + debouncer → mpsc bridge. Owns the Debouncer guard.
                Real-FS smoke tests only (real time, justifying comment per IR-33).
  cron.rs       CronSchedule newtype over croner (pattern + Tz), pure
                next_after(now) -> Option<DateTime<Tz>>; Clock seam.
  limits.rs     LimitEnforcer trait (spec §4 seam), PollingEnforcer (v1),
                MemorySampler trait + SysinfoSampler; breach mpsc to registry.
  probes.rs     Prober trait + ScriptedProber (test) + OsProber (http/tcp/exec);
                ProbeTask (liveness loop), await_ready (readiness gate).
  worker.rs     interval-task host per map.md: owns cron worker loops + the
                memory poll tick; catch_unwind per tick.
```

map.md drift to fix when planning: probes.rs is a new module map.md never named
(spec §7 requires it; spec wins), and watcher.rs becomes `watch/` (two files —
OS seam vs logic — under Rin's 500-line split rule).

Sheep-task integration points (owned by the supervisor from earlier phases):
registry arms/disarms watch groups, cron schedules, limit enforcement, and
liveness tasks on lifecycle transitions; `await_ready` is called by both the
start path (`starting → online`) and reload's AwaitReady state.

## 3. Hardest design decisions

**D1 — HTTP probe client: hand-roll it.** Spec's whole contract is "HTTP GET
must return 2xx" (§7, `ProbeKind::Http` doc). That is ~40 lines over
`TcpStream`: connect, write `GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection:
close\r\n\r\n`, read + parse the status line (cap 8 KiB), 200..=299 = pass,
`tokio::time::timeout` around the lot. Zero new deps, fully async, trivially
fake-able. Consequence to document honestly: no TLS, no redirects — `https://`
targets are rejected at config validation with a typed error ("https probe
targets unsupported in v1"), not silently failed at runtime. Escape hatch: the
`Prober` trait seam means swapping in hyper-util's legacy client later touches
one impl. reqwest rejected on the daemon idle-RSS goal (§14.11); minreq/ureq
rejected for blocking-IO timeout semantics.

**D2 — where croner lives: shep-core.** normalize.rs currently has a 5-field
stopgap with a ponytail note saying real validation lands this phase — and the
stopgap is actually *wrong* for the promised dialect (JS croner accepts an
optional seconds field, i.e. 6 fields, plus `L/W/#`). Replace it with a
croner-backed parse in shep-core (`cron_restart` pattern + `cron_timezone` IANA
name via chrono-tz) so a bad Flockfile fails loudly at parse time (§5 "typos
fail loudly", IR-21 validation-first). Cost: chrono + chrono-tz (embedded tzdb)
in core's tree — acceptable because everything ships in the one `shep` binary
anyway; the binary-size cost is identical wherever it lives. Daemon re-uses the
same parse (DRY: one function in core, `NormalizeError::InvalidCron` /
`InvalidTimezone` carry the offending value).

**D3 — cron scheduling under tokio: pure next_after + capped-sleep loop +
Clock seam.** Two clocks are in play: chrono wall time (what cron means) and
tokio's clock (what tests pause). Bridge: (a) `CronSchedule::next_after(now)` is
a pure function — pinned-array tests (IR-36) incl. the DST cases need no tokio
at all; (b) the worker loop re-derives every tick:
`sleep(min(next - clock.now(), MAX_CRON_SLEEP)).await` with
`MAX_CRON_SLEEP = 60s` (named const, IR-26) — a laptop suspend, NTP step, or DST
wall shift is corrected within a minute instead of firing from a stale sleep;
(c) `now` comes from a 2-method `Clock` seam (dyn-safe; test impl derives wall
time from `tokio::time::Instant` elapsed against a fixed epoch, so
`start_paused = true` drives cron tests deterministically). Missed occurrences
while suspended: fire **at most one** catch-up restart on wake, never replay
the backlog (restart-storm guard; document it).

**D4 — memory limit scope: process tree (sheep + her lambs), one shared
sample pass.** Spec §4 defines the tree as the kill unit (lambs die with the
sheep); measuring only the root pid would let a forking sheep dodge its limit,
and the pm2 single-pid behavior is a known gap. So: one sysinfo
`refresh_processes` (memory-only kind) per 15s tick for the *whole* table,
build a parent→children map once, DFS-sum RSS from each armed sheep's root pid.
One pass serves every armed sheep (never per-sheep refreshes), and the same
sampler later feeds describe/lookout/metrics-dog readings — one `MemorySampler`,
many consumers. Deviation from pm2 (tree vs single pid) gets a spec-§4-style
deviation callout in docs. `MEMORY_POLL_INTERVAL = 15s` named const citing §14.2.

**D5 — LimitEnforcer seam shape: arm/disarm + breach channel.** The v1.1 cgroup
enforcer (`enforce = "kernel"`) must slot in without touching the engine, so the
trait contract is mechanism-free: `arm(sheep_id, root_pid, MemSize)` /
`disarm(sheep_id)`, breaches reported as `LimitBreach { sheep_id, observed }`
on an mpsc given at construction. `PollingEnforcer` implements it by sampling;
the cgroup impl will implement it by writing `memory.max` and watching
`memory.events`. Registry reacts to a breach with the normal restart path (stop
ladder → spawn); a breach restart does **not** touch the unstable counter —
only exits within `min_uptime` do (§4). Dyn-safe single-method-pair trait, no
ext layer needed (IR-10's two-layer split buys nothing here); dyn-compat smoke
test per IR-10 anyway since the registry holds `Box<dyn LimitEnforcer>`.

**D6 — readiness unification: one `await_ready`, three sources.** Derive
`ReadinessSource { Channel, Probe(ProbeConfig), Heuristic }` from AppConfig
(`wait_ready` → Channel; `readiness_probe` → Probe; neither → Heuristic, §7).
One `await_ready(source, deadline = listen_timeout)` future serves both normal
start (`starting → online`) and reload's AwaitReady gate: Channel = first
`{"kind":"ready"}`; Probe = first probe success (poll every `interval`,
starting immediately; `failure_threshold` does NOT apply to readiness — it is a
liveness concept); Heuristic = deadline elapse itself. On deadline without
success: normal start goes online anyway with a warning event (pm2-compatible,
avoids restart storms — flag for Rin, it's a judgement call); reload treats it
as new-instance failure → abort remaining instances, old flock keeps running
(§4 reload). Liveness: per-sheep task while online, `sleep(interval)` measured
from probe completion (no overlap), consecutive-failure counter, threshold →
`LivenessFailed` event → registry runs the restart policy (§7). `Prober` is a
generic param on the engine (mirrors `ProcessRunner`, IR-13 rationale), with
`ScriptedProber` as the IR-33 hand-rolled fake; exec probes run via
`tokio::process::Command` (`sh -c` / `cmd /C`), `kill_on_drop`, in the sheep's
cwd + env (probes usually need `PORT`).

Watch re-check (§4) resolved without a decision entry because the channel *is*
the mechanism: events arriving during an in-flight restart queue in the
WatchGroup's mpsc; after `restart_group().await` completes, the loop drains and
re-filters the queue — anything surviving the globs triggers the next round.
No dirty flag, no extra state machine. Filtering happens post-debounce against
two GlobSets: ignore = built-in defaults (dot-entries, `**/node_modules/**`,
log/pid dirs) + `ignore_watch`; include = `watch_options` (empty ⇒ match-all).
One WatchGroup per name-group (all N instances share one debouncer — the
map.md O(N²) fix), restart targets the whole group.

## 4. Testing strategy (IR-33..IR-39, plus 40)

- **Fixtures (IR-33/34):** crate-root test module grows `ScriptedProber`
  (scripted outcome sequences), `ScriptedSampler` (scripted per-pid RSS arrays),
  `TestClock` (tokio-instant-derived wall time), each with a WHY comment; every
  test its own config literal + tempdir.
- **Pure pinned tests (IR-30/36):** `CronSchedule::next_after` — pinned
  occurrence arrays incl. spring-forward gap (fixed 02:30 job → 03:00), fall-back
  single-fire, `*/15` gap-skip, `Seconds::Optional` 6-field patterns, tz vs UTC
  disagreement across midnight. Asserted doctests on the pure parts.
- **Paused-clock sequences (IR-36):** cron worker fires at exact instants
  (capped-sleep re-derive verified by jumping the TestClock); memory poll
  breaches on the exact 15s tick the scripted RSS crosses the limit;
  probe threshold trips after exactly N failures at interval spacing;
  `await_ready` races (ready-before-deadline, deadline-elapse, probe-success)
  in both start and reload state machines.
- **Property tier (IR-37):** extend the supervisor proptest interleavings with
  `LivenessFailed` + breach events (invariants hold: never two live pids per
  instance, counter monotonic, steady state reached); WatchGroup never has two
  restarts in flight.
- **Real-OS integration (justified real time per IR-33):** watch/source.rs
  smoke — tempdir, real notify, short watch_delay, event-driven wait; OsProber
  against a loopback `TcpListener` (scripted 200/500/hang responses) and
  `sh -c` exec probes.
- **Compile-only (IR-38):** add `LimitEnforcer` + `Prober` external-impl proofs
  to shep-daemon's single tests/ file alongside `ProcessRunner`.
- **E2E (IR-39, shep-cli phase):** touch-file → watch-restart observed via
  event stream; readiness-probe reload against a fixture HTTP server (assert
  old instance outlives new-instance failure); config-error paths (bad cron,
  bad tz, https probe target) assert exact typed errors. No sleeps.
- **Boundary sweeps (IR-40):** empty watch_options, glob that matches nothing,
  failure_threshold = 1, timeout > interval, `max_memory` smaller than any
  observed RSS (immediate breach), cron pattern that never fires again
  (`find_next_occurrence` → None ⇒ log + task ends), 0-instance group.
- **CI (IR-44):** minimal-versions row will need the chrono ^0.4.42 floor from
  croner honored; sweep for under-declared transitive floors like the existing
  Cargo.toml pin block.

## 5. Eventual plan — task list (titles only)

- Workspace deps: notify / notify-debouncer-full / croner / chrono / chrono-tz / sysinfo (IR-2 comments + minimal-versions floor sweep)
- shep-core: croner-backed cron_restart + cron_timezone validation (replace 5-field stopgap)
- shep-core: probe target validation (http-url grammar incl. https rejection, host:port, non-empty exec)
- shep-daemon cron.rs: CronSchedule + Clock seam + pinned DST tests
- shep-daemon worker.rs: cron worker loop (capped-sleep re-derive, one-shot catch-up) wired to registry restart
- shep-daemon limits.rs: MemorySampler seam + SysinfoSampler + criterion bench for the 15s const
- shep-daemon limits.rs: LimitEnforcer trait + PollingEnforcer + registry breach wiring
- shep-daemon probes.rs: Prober seam + ScriptedProber + liveness ProbeTask (threshold engine)
- shep-daemon probes.rs: OsProber (hand-rolled HTTP GET, TCP connect, exec)
- shep-daemon probes.rs: await_ready unification (start transition + reload AwaitReady gate)
- shep-daemon watch/source.rs: notify+debouncer → mpsc adapter + real-FS smoke
- shep-daemon watch/mod.rs: WatchGroup task (globset filter, queued-event re-check) + paused-clock tests
- Registry integration: arm/disarm watch, cron, limits, liveness on lifecycle transitions
- Proptest extension: liveness/breach interleavings + watch single-flight invariant
- E2E: watch-restart, readiness reload, typed config-error paths
- map.md sync: probes module added, watcher split recorded
- Docs pass: module decision guides (IR-27) + CHANGELOG entries
