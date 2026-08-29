# Refactor Goals — pm2 → Rust

Gathered 2026-08-07. The maintainer away during Phase 2; goals inferred from her brief + session context. Assumptions marked, open questions at bottom for her review.

## Goals (from brief)

1. **Language/runtime change: Node.js → Rust.** Full port, new workspace at `pm2-rs`. Rationale: long-standing todo, tech-debt escape, Rust reliability/perf.
2. **Tech-debt elimination.** Base project is old; assess keep/toss per module rather than 1:1 transliteration.
3. **Feature triage.** Explicitly wants: keep-list, toss-list, and missing-features ("nice to have") list.
4. **Code quality bar: `rand`-grade.** Pristine idiomatic Rust — lint config, docs, feature discipline modeled on `~/GitHub/rand`.

## Must-haves (the maintainer, 2026-08-07, explicit)

5. **Built-in system usage viewing, TUI.** pm2 `monit`/`dashboard` exist (blessed-based) but are weak. Rust: first-class ratatui TUI — per-process CPU/mem/restarts + host-level usage. Upgrade, not port.
6. **Native observability: Prometheus + Grafana.** `/metrics` endpoint on daemon (prometheus exposition format), per-process + daemon-self metrics. Ship a reference Grafana dashboard JSON. pm2 has nothing native here (pm2.io SaaS was the answer — dropped).
7. **Webhook alerts.** Process events (crash, restart-loop, high-mem, stopped) → configurable webhooks: Discord, Slack, generic JSON POST. Templated payloads, per-event routing.
8. **Configurable.** Daemon-level config file (not just per-app ecosystem config): ports, metrics on/off, webhook targets, alert thresholds, log policy. Layered: file + env + CLI flags.
9. **MCP server for agentic monitoring** (the maintainer, 2026-08-07). Built-in MCP server so AI agents can monitor (and optionally control) the daemon: process list, status, metrics, log tails as tools/resources. Read-only by default; lifecycle control behind explicit opt-in flag (daemon is a privilege boundary — agents get observation for free, mutation only when granted).

## Inferred goals (assumptions — flag if wrong)

- **Single-binary distribution.** Biggest practical win over npm-installed pm2. No node runtime needed on target host. *Assumed: yes.*
- **Daemon footprint.** pm2 daemon idles at ~50-80 MB RSS (V8 baseline). Rust daemon should idle in single-digit MB. *Assumed: goal, not hard target.*
- **Manage ANY process, not just Node.** pm2's cluster mode is Node-only (uses `cluster` module + code injection). Rust port supervises arbitrary executables; Node-specific injection features are redesigned or dropped. *Assumed: general-purpose supervisor is the point.*
- **CLI familiarity, not bug-for-bug compat.** `pm2 start/stop/list/logs/monit` muscle memory preserved; obscure flags negotiable. *Assumed: familiar-not-identical.*
- **No pm2.io/Keymetrics SaaS coupling.** Vendored `pm2-io-agent` is drop candidate. *Assumed: drop.*

## Constraints

- **Team:** the maintainer solo + Claude. Rust-experienced (LKH-3 port precedent, zendriver-rs).
- **Migration strategy: big-bang greenfield.** No interop with running JS pm2 required; old repo is reference only. (Strangler-fig meaningless across a runtime boundary for a daemon.)
- **Timeline:** none stated. Hobby/portfolio cadence assumed.
- **Breaking changes:** allowed everywhere except core CLI verbs.
- **LICENSE — RESOLVED (the maintainer, 2026-08-07): clean-room, own license.** pm2 is a *feature-list inspiration only*. Implementation never ports or references pm2 source code; the refactor-workspace artifacts serve as OUR behavior spec (observable behavior, CLI concepts, semantics — not code structure). License set to MIT OR Apache-2.0 (rand-style dual; swap if the maintainer prefers otherwise). Consequences: no obligation to pm2 file layouts, env-var names, dump-file formats, or CLI flag spellings — "compat" language in map.md/assessment.md now reads as *fidelity to our spec*, not compatibility with pm2 artifacts. Own branding + sheep terminology throughout (the maintainer's direction).

## Open questions for the maintainer (she reviews on return)

1. Is Node cluster-mode parity (in-process worker balancing) required, or is fork-mode + SO_REUSEPORT-style socket sharing acceptable?
2. Windows: pm2 supports it poorly. Port scope = unix-first, Windows later? *Assumed unix-first.*
3. ~~Compat level~~ → RESOLVED (the maintainer, 2026-08-07): **no pm2 baggage** — sheep-native surface (SHEP_* env, own formats/verbs; plain-English aliases stay). One exception: migration guide + `shep import` for existing pm2 boxes.
4. ~~Final name pick~~ → RESOLVED: **shep** (the maintainer, 2026-08-07). Lexicon in docs/terminology.md.
5. ~~MCP transport~~ → RESOLVED (the maintainer, 2026-08-07): stdio in v1 (dev/debug), HTTP/SSE committed for v1.1.
6. Map.md decisions 1/6/7 ruled; 2/3/4 pending — briefs in [decision-briefs.md](decision-briefs.md). Still open here: q1 cluster-mode parity, q2 Windows scope.
