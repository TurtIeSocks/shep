---
name: shep-idiomatic-rust
description: Use when writing, reviewing, or refactoring ANY Rust code in the shep workspace (shep-core, shep-daemon, shep-client, shep-cli) — new modules, types, traits, error enums, tests, docs, Cargo.toml edits, or CI config.
---

# shep Idiomatic Rust

**REQUIRED READING before writing code: [docs/idiomatic-rust.md](../../../docs/idiomatic-rust.md)** — 45 numbered rules (IR-1..IR-45) distilled from rand 0.10.2, the project's quality bar. Cite rules by number in reviews. Full evidence: `docs/idiomatic-rust/lenses/`.

## Baseline-failure checklist

These are the rules agents violate when writing "good" Rust from instinct. Check every one before returning code:

| Check | Rule |
|---|---|
| NO panicking constructors outside shep-cli — return `Result`, even in `const fn` (drop constness or take pre-validated input) | IR-21 |
| `impl core::error::Error`, never `std::error::Error` | IR-19 |
| Every `Result`-returning pub fn has an `# Errors` doc section | IR-28 |
| `# Panics` doc and `#[track_caller]` travel together — never one without the other | IR-21 |
| Error enums: per-module, variant docs state the precise *condition* | IR-18, IR-19 |
| unsafe only in `shep-daemon/src/sys.rs`, `// SAFETY:` per block | IR-22, IR-23 |
| Secret/env-carrying types: manual redacted `Debug` + exact-string test | IR-41 |
| Tests: paused tokio clock default, no sleeps, hand-rolled fakes, unique fixtures per test | IR-33, IR-34 |
| Wire-facing type changed → stability fixtures + CHANGELOG | IR-35, IR-45 |
| New dep: `default-features = false`; new feature: additive + `# Option:` comment | IR-2, IR-3 |
| `#[must_use]` only where discarding is a plausible bug | IR-17 |
| Don't widen accepted input formats beyond the spec (no bonus unit spellings, no lenient whitespace) without a map.md/goals.md basis | spec fidelity |

## Sheep terminology

User-facing naming follows [docs/terminology.md](../../../docs/terminology.md) — flock/fold/Flockfile/bleats/bark/whistle. Destructive ops and error text stay plain (kill is kill).
