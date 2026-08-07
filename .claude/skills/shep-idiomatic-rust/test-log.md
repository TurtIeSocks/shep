# Skill test log (RED/GREEN per writing-skills TDD)

2026-08-07. Application scenario: "write `shep-core/src/mem_size.rs`, production quality" to a fresh general-purpose agent.

## RED (no skill) — violations observed

Otherwise-good code with these deviations from docs/idiomatic-rust.md:
1. Panicking `const` constructors (`from_kib/from_mib/from_gib` panic on overflow) in shep-core — IR-21.
2. `impl std::error::Error`, not `core::error::Error` — IR-19.
3. No `# Errors` section on the `FromStr` impl — IR-28.
4. `# Panics` docs without `#[track_caller]` — IR-21.
5. Widened input format beyond spec (KB/KiB/MiB spellings, interior whitespace) with no documented basis.

## GREEN (skill checklist + spec pointer in context) — all five corrected

No panicking constructors; `core::error::Error`; `# Errors` on every Result fn; no unpaired `# Panics`; strict `^\d+(G|M|K)?$` grammar with explicit rejection tests. Bonus compliance unprompted: `// wire format` breaking-change comment (IR-11), verdict-comment doctests (IR-30), `f.write_str` Display shape (IR-19).

## Caveats / future hardening

- One rep per arm (writing-skills recommends 5+ for wording micro-tests) — signal was unambiguous on style rules, but re-run reps before tightening wording.
- GREEN prompt included the spec grammar inline (RED task didn't state it), so item 5 is partially confounded; items 1-4 had identical task info in both arms — clean signal.
- Re-test when the skill gains rules or the spec renumbers.
