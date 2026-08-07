# Research: metrics dog (spec §8, Phase 5)

Status: design notes for Rin's review · 2026-08-07 · crate versions fetched live
from crates.io this date.

Scope: `shep dog metrics` — a first-party dog (shep-client consumer inside the
multi-call binary, map.md decision #3) serving Prometheus exposition on
`127.0.0.1:9615` (spec §8, assumption #4). Needs: per-sheep
cpu/mem/restarts/status/uptime, daemon self, host metrics; Grafana reference
dashboard in `assets/grafana/`; OTLP behind `otel` feature.

## 1. Crate survey (verdict first)

| Option | Version | Mandatory deps pulled | Verdict |
|---|---|---|---|
| hand-rolled exposition writer | — | none | **RECOMMENDED** |
| `prometheus-client` (official CNCF) | 0.25.0 (2026-06-15) | dtoa, itoa, parking_lot, derive crate | runner-up |
| `prometheus` (tikv) | 0.14.0 (2025-03-27) | cfg-if, fnv, lazy_static, memchr, parking_lot, thiserror (+protobuf in default features) | reject |
| `metrics` + `metrics-exporter-prometheus` | 0.24.6 + 0.18.3 | metrics-util, evmap, quanta, indexmap, base64, thiserror (+hyper stack for its listener) | reject |
| `tiny_http` | 0.12.0 (2022-10-06) | sync/threaded | reject |

**Why hand-rolled wins.** The dog's data model is a *snapshot* — one RPC reply
plus a couple of self-counters — re-rendered per scrape. Registry libraries
(`prometheus-client`'s `Family`/handle model, the `metrics` recorder facade)
are built for code instrumented at call sites with long-lived metric handles;
mapping a snapshot onto them means synthesizing handles each collect and
pruning stale series when sheep are deleted. That's more code than the
exposition format itself. Prometheus text format 0.0.4 is a tiny, frozen spec:
`# HELP` / `# TYPE` lines, `name{labels} value`, three escape rules
(label values: `\\`, `\"`, `\n`; help text: `\\`, `\n`). Our metric set is
closed (~18 names, all gauges + two counters), so `render(&MetricsSnapshot)
-> String` is a ~150-line pure function under total snapshot-test control —
the same discipline as the wire fixtures (IR-35), applied to a second compat
surface. Zero new deps satisfies IR-2 trivially.

**Runner-up.** If review prefers a maintained encoder, `prometheus-client`
0.25.0 is the only acceptable one: official, lean (dtoa/itoa/parking_lot),
`protobuf` support optional via `prost` (leave off). Use its `Collector` trait
for scrape-time encoding, not `Family` handles. The tikv `prometheus` crate is
the previous generation (lazy_static-era, global registry, protobuf in default
features); the `metrics` facade buys indirection we'd fight, plus quanta/evmap.

**The `metrics` facade does NOT buy OTLP either.** Spec §8 puts OTLP behind
the `otel` cargo feature. That will be `opentelemetry` 0.32.0 +
`opentelemetry-otlp` 0.32.0 push pipelines fed from the *same*
`MetricsSnapshot` — orthogonal to the exposition encoder choice. Additive
feature per IR-3 (`otel = [...]` composing upward); pin exact versions in that
phase, not now.

**Supporting crates (exact, current):**

- `sysinfo` 0.39.6 — shep-daemon only, `default-features = false, features =
  ["system"]` (processes + cpu + memory; skips disk/network/component/user
  backends). Already required by `LimitEnforcer` polling (spec §4) and
  describe/lookout — the dog adds no new dependency here (see D4).
- `axum` 0.8.9 — `default-features = false, features = ["http1", "tokio"]`.
  Already a v1 dependency via `shep serve` (spec §9), so the metrics dog's
  HTTP server is dependency-free at the margin (see D2).
- Dev-deps: `tower` 0.5.3 (`features = ["util"]`, `# Only to oneshot the
  axum router in tests`); optional `prometheus-parse` 0.2.5 (`# Only to
  round-trip-validate exposition output`) — nice-to-have, snapshots are the
  real gate.

## 2. Module shape

```
crates/shep-core/src/protocol/
  metrics.rs           NEW — MetricsSnapshot { flock: Vec<SheepMetrics>,
                       daemon: DaemonMetrics, host: HostMetrics }
                       // wire format: changing this is a breaking change (IR-11)
  request.rs           + Request::GetMetrics, + Response::Metrics(MetricsSnapshot)
                       (additive #[non_exhaustive] variants → PROTOCOL_VERSION
                       stays 1 per protocol/mod.rs evolution rule; new byte
                       fixtures + insta snapshots per IR-35)

crates/shep-daemon/src/
  host_metrics.rs      NEW — single sysinfo::System, continuous refresh cadence
                       (named const, IR-26); feeds LimitEnforcer, describe,
                       and the GetMetrics handler
  (rpc handler)        + GetMetrics arm assembling the snapshot

crates/shep-client/src/
  (client)             + typed get_metrics() wrapper (same shape as other verbs)

crates/shep-cli/src/dogs/
  mod.rs               dog dispatch for hidden `shep dog <name>` (dog-infra phase)
  metrics/
    mod.rs             entrypoint: [dog.metrics] config → client connect →
                       axum serve on `listen` (default 127.0.0.1:9615)
    collect.rs         scrape-time fetch behind a `MetricsSource` trait +
                       min-interval cache + last-good fallback + self-counters
    exposition.rs      pure render(&MetricsSnapshot, &SelfStats) -> String

assets/grafana/shep-overview.json
```

Config `[dog.metrics]` in shep.toml: `listen = "127.0.0.1:9615"` only (v1).
Cache TTL, RPC deadline, refresh cadence = named consts with benchmark/rationale
comments (IR-26), not config knobs (KISS).

## 3. Hardest design decisions

**D1 — Exposition encoder: hand-rolled.** See §1. Runner-up
`prometheus-client` 0.25.0 via its `Collector` trait if review wants a
maintained encoder. Content-type header: `text/plain; version=0.0.4;
charset=utf-8` (classic format; OpenMetrics negotiation is a non-goal —
Prometheus scrapes 0.0.4 indefinitely).

**D2 — HTTP server: axum 0.8.9.** `shep serve` already puts axum + tower-http
in shep-cli (spec §9), so the marginal cost is zero and the binary keeps one
HTTP stack. One `Router` with `GET /metrics` (+ `GET /healthz` returning 200,
cheap and Grafana-agent-friendly). Raw hyper 1.11 + hyper-util is ~80 lines of
connection-loop boilerplate for no dependency savings given serve; tiny_http
0.12.0 is synchronous, thread-per-conn, dormant since 2022, and would be the
only non-tokio server in the binary. Bind 127.0.0.1 default per §10; support
`127.0.0.1:0` + log the bound addr (e2e tests need it).

**D3 — Poll cadence vs bus-driven: neither — collect at scrape time, cached.**
Every exported series is a gauge (last-value semantics) except restart/scrape
counters, which the daemon/dog already track cumulatively — bus events buy
nothing but a second, lossy data path: the bounded bus drops under load
(`Dropped` notice, spec §6), so an event-driven dog needs poll reconciliation
anyway (that's bark's problem, §8; metrics shouldn't import it). Pull model
alignment: fetch on scrape means freshness tracks the scrape interval, an idle
dog does zero RPC, and there is exactly one code path. Guardrails:
`SCRAPE_CACHE_TTL` (2s, named const) coalesces scrape storms/multiple
Prometheis; RPC failure serves the last-good snapshot with `shep_up 0` and
bumps `shep_metrics_rpc_failures_total`; client's default 5s deadline sits
safely under Prometheus's 10s default scrape timeout. No background tick task
at all — KISS.

**D4 — Where sysinfo lives: daemon-side only; the dog is a pure wire
consumer.** Per-process cpu% requires successive refreshes of a *warm*
`sysinfo::System`; the daemon already runs one continuously for `max_memory`
enforcement (15s cadence, §4) and describe/lookout. A dog-side System would
duplicate that state and give garbage first-scrape cpu deltas. So
`GetMetrics` returns everything — flock, daemon self (its own pid via the same
System), host — and the whistle's `get_metrics` MCP tool reuses the identical
RPC (map.md). Bonus: the metrics dog becomes the reference implementation of a
third-party dog — nothing it does is privileged.

**D5 — Status encoding: state-set, not numeric enum.**
`shep_sheep_status{name="web",instance="0",state="online"} 1` with one 0/1
series per state (6 states, §4) — the OpenMetrics enum convention. PromQL
reads naturally (`shep_sheep_status{state="errored"} == 1`), Grafana state
timeline works without magic-number value mappings, and alert rules survive
adding a state. Cardinality 6 × flock size — negligible. Numeric-gauge
encoding (`status 2`) rejected: every consumer needs the decoder table and
it breaks silently when states are added.

**D6 — Units follow Prometheus base-unit convention.** cpu as `_ratio` (0.0–
1.0; divide sysinfo's percent by 100), memory `_bytes`, uptime `_seconds`
(f64, from `uptime_ms`). `restarts_total` is a counter; document that
delete + re-add resets it (Prometheus `rate()`/`increase()` handle resets
natively). Names are a compat surface once shipped — pinned by snapshot, and
renames follow IR-16 spirit (ship both for one release).

## 4. Metric inventory (the contract)

Per-sheep labels: `name`, `instance` (SHEP_INSTANCE slot), `fold` (empty when
none). No `id` label — ids churn across delete/re-add; name+instance is the
stable identity.

| Metric | Type | Notes |
|---|---|---|
| `shep_sheep_cpu_ratio` | gauge | 0.0–1.0 |
| `shep_sheep_memory_bytes` | gauge | RSS |
| `shep_sheep_restarts_total` | counter | from `ProcessInfo.restarts` |
| `shep_sheep_status` | gauge | state-set, extra `state` label, 0/1 |
| `shep_sheep_uptime_seconds` | gauge | 0 when not online |
| `shep_daemon_cpu_ratio` / `_memory_bytes` / `_uptime_seconds` | gauge | shepherd self |
| `shep_daemon_info` | gauge | value 1; labels `version`, `protocol` |
| `shep_flock_size` | gauge | `status` label, count per state |
| `shep_host_cpu_ratio` | gauge | whole-host |
| `shep_host_memory_used_bytes` / `_total_bytes` | gauge | |
| `shep_host_load1` / `load5` / `load15` | gauge | unix only; omit on Windows |
| `shep_host_cpu_count` | gauge | |
| `shep_up` | gauge | 1 = daemon reachable at last collect |
| `shep_metrics_scrapes_total` | counter | dog self |
| `shep_metrics_rpc_failures_total` | counter | dog self |
| `shep_metrics_collect_duration_seconds` | gauge | last collect wall time |

## 5. Testing strategy (IR-33..40)

- **IR-33/34** — `#[cfg(test)] mod test` fixture factory `sample_snapshot()`
  (WHY comment: one canonical snapshot exercising every metric family) in
  shep-cli; each test builds its own variant, no shared state. `collect.rs`
  codes against a `MetricsSource` trait; tests use a hand-rolled scripted fake
  (`const_source(snapshot)` / `failing_source(n)`) — no mock frameworks.
- **IR-35** — two compat surfaces: (a) `GetMetrics`/`Metrics(..)` wire shapes
  get byte fixtures + insta snapshots in shep-core beside the existing ones;
  (b) the exposition text itself gets an insta snapshot of
  `render(sample_snapshot())` — changing a metric name/label re-accepts a
  snapshot deliberately + CHANGELOG, never silently.
- **IR-36** — paused-clock (`start_paused = true`) tests on the cache: two
  scrapes inside `SCRAPE_CACHE_TTL` = exactly one source call; advance past
  TTL = second call; source failure = last-good body + `shep_up 0`.
- **IR-38** — one compile-only `tests/` file proving an external crate can
  implement `MetricsSource` (`todo!()` body).
- **IR-39** — assert_cmd e2e: fresh `$SHEP_HOME`, real daemon + one sleeper
  sheep, `shep dog metrics` with `listen = 127.0.0.1:0`, parse bound addr from
  its startup line, HTTP GET, serde-free line asserts
  (`shep_sheep_status{...,state="online"} 1` present, content-type exact).
  Event-driven waits, no sleeps.
- **IR-40 boundary sweeps** — empty flock (daemon+host sections only);
  escaping edges (label value with `"`, `\`, newline); f64 edges (NaN/±Inf
  cpu clamped before render — assert clamp); all six states in the state-set;
  Windows row: load-average series absent, everything else present.
- Router tested via `tower::ServiceExt::oneshot` — no port bind, asserts
  status + content-type + body; optional `prometheus-parse` 0.2.5 dev-dep
  round-trip check.

## 6. Grafana reference dashboard (`assets/grafana/shep-overview.json`)

Templating: `$datasource`, `$sheep` = `label_values(shep_sheep_status, name)`.

- **Row: Flock** — stat: flock size by status (`shep_flock_size`); table:
  name/instance/status/uptime/restarts (instant); timeseries: cpu ratio per
  sheep; timeseries: memory bytes per sheep; state-timeline:
  `shep_sheep_status == 1` by state; timeseries: restart rate
  (`rate(shep_sheep_restarts_total[5m])`).
- **Row: Shepherd** — stat: `shep_up` (thresholded red/green); timeseries:
  daemon memory; timeseries: daemon cpu; stat: daemon uptime + version
  (from `shep_daemon_info`).
- **Row: Host** — gauge: host cpu; gauge: memory used/total; timeseries:
  load 1/5/15.
- **Row: Dog self** — timeseries: scrape rate + rpc failure rate; stat:
  collect duration.

~13 panels. Ship pinned `schemaVersion`; validate by importing into a local
Grafana once during the phase (manual gate, noted in the task).

## 7. Eventual plan — task list (titles only)

- shep-core: `MetricsSnapshot` wire types + `GetMetrics`/`Metrics` variants + fixtures/snapshots
- shep-daemon: `host_metrics.rs` sysinfo sampler (single warm System, named cadence consts)
- shep-daemon: `GetMetrics` RPC handler assembling flock/daemon/host snapshot
- shep-client: typed `get_metrics()` wrapper
- shep-cli: `[dog.metrics]` config section parse + defaults
- shep-cli: `exposition.rs` renderer + escaping + insta pin + boundary sweeps
- shep-cli: `collect.rs` — `MetricsSource` trait, cache TTL, last-good fallback, self-counters (paused-clock tests)
- shep-cli: axum `/metrics` + `/healthz` server, `127.0.0.1:0` support, oneshot tests
- e2e: daemon + sleeper sheep + dog scrape round-trip (assert_cmd)
- assets: Grafana dashboard JSON + one-time import validation
- docs: metric reference table (names = compat surface) + CHANGELOG entries

## 8. Offline caveat

None — crates.io was reachable; all versions above fetched 2026-08-07.
