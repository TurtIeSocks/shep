# rand style reference

# rand crate — Rust style conventions (extracted 2026-08-07, rand 0.10.2, edition 2024, MSRV 1.85)

## 1. Crate / feature-flag architecture
- Single lib crate; **benches split into a separate non-published sub-crate** (`benches/Cargo.toml` with `publish = false` + empty `[workspace]` to detach it) so heavy dev-deps (criterion) never touch the main crate's dep tree.
- `[features]` block is **documented inline, grouped by kind**: `# Meta-features:` first (`default`, `serde = ["dep:serde"]`), then `# Option: ...` comment above every flag explaining what it enables and caveats ("requires nightly", "affects reproducibility"). Deprecated flags kept with `# Deprecated option:` comment.
- **`dep:` syntax for all optional deps** — features never implicitly expose dep names. Weak dep features: `std = ["alloc", "getrandom?/std"]`.
- Layered feature ladder: `alloc` ⊂ `std` ⊂ `thread_rng = ["std", "std_rng", "sys_rng"]`; each capability is its own flag; `#![no_std]` core + `extern crate alloc/std` gated per feature.
- `default-features = false` on every dependency declaration; minimal versions pinned meaningfully (`serde = "1.0.103"` — the actual minimum needed, and CI verifies with `-Z minimal-versions`).
- `include = ["src/", "LICENSE-*", "README.md", "CHANGELOG.md", "COPYRIGHT"]` to slim the published package.
- Public API strategy: re-export upstream (`pub use rand_core;` + selective `pub use rand_core::{...}`), private `mod rng;` with `pub use rng::{Fill, RngExt};`, a `prelude` module with `#[doc(no_inline)]` re-exports, `#[doc(hidden)] pub mod hidden_export` for cross-crate internals, `#[doc(inline)]` for the flagship type.

## 2. Lint config + MSRV policy
- Crate-root lints (lib.rs), not Cargo `[lints]`: `#![deny(missing_docs)]`, `#![deny(missing_debug_implementations)]`, `#![deny(clippy::undocumented_unsafe_blocks)]`, narrowly-scoped `#![allow(clippy::float_cmp, ...)]` with domain justification.
- `#![doc(test(attr(allow(unused_variables), deny(warnings))))]` — doctests deny warnings.
- `clippy.toml` only for `doc-valid-idents` (project jargon exempt from `doc_markdown`).
- `rust-version = "1.85"` in Cargo.toml; **CI pins clippy to a specific recent toolchain (1.93)** with `-D warnings` while MSRV job tests only compile/test on 1.85.0 — lint churn decoupled from MSRV.
- rustfmt: default at root (CI `cargo fmt --all -- --check`), benches override `max_width = 120`.
- Test-local allows scoped tightly: `#![allow(clippy::bool_assert_comparison)]` *inside one test fn* with a "We prefer to be explicit here" comment.

## 3. Doc conventions
- Every file: license header comment block, then `//!` module doc. Module docs are **mini-guides**: title line (no trailing period), `# Quick start` with runnable example, taxonomy sections (`## Generators`, `##### Non-deterministic ...`), footnotes (`[^1]`), and a reference-link block at the bottom (all `[`X`]: path` links collected at the end, not inline).
- Item docs follow a strict section order: summary line, prose, `# Plot` / `# Example` / `# Precision` / `# Panics` / `# Security` as applicable. **Every panic path gets `# Panics`**; security-relevant items get `# Security` referencing a canonical doc anchor (`[`ThreadRng#Security`]`).
- Doctests everywhere, including edge cases as asserted examples (`assert!(Bernoulli::from_ratio(4, 3).is_err()); // numerator > denominator`). `<code>[rng()].[random()](RngExt::random)</code>` HTML for composed-call cross-links.
- `#![cfg_attr(docsrs, feature(doc_cfg))]` + docs.rs metadata `all-features = true`, `rustdoc-args = ["--generate-link-to-definition"]`, with a comment showing the local build command. `#[cfg(doc)] use ...` imports purely for intra-doc links.
- CI builds docs on nightly with `RUSTDOCFLAGS="-Dwarnings --cfg docsrs"` — broken links fail CI. `typos` CI job.

## 4. Error handling + API design idioms
- **Small, per-constructor error enums**, not one crate-wide error: `BernoulliError`, `uniform::Error`, `weighted::Error`. Pattern: fieldless `enum` + `#[derive(Clone, Copy, Debug, PartialEq, Eq)]` + manual `fmt::Display` via `f.write_str(match self {...})` + empty `impl core::error::Error for X {}` (core, not std — no_std-safe). Each variant doc-commented with the exact condition (`/// \`p < 0\` or \`p > 1\`.`).
- Constructors return `Result<Self, Error>`; validation up-front with early returns. Infallible-by-construction ops use `type Error = Infallible`.
- Panicking convenience fns get `#[track_caller]` + documented `# Panics`. Hot paths `#[inline]`/`#[inline(always)]`; cold error paths `#[cold] #[inline(never)]`.
- Unsafe: every block has `// SAFETY:` (13 in src, lint-enforced); large *rationale comments* above tricky design choices (the UnsafeCell-vs-RefCell essay in thread.rs, the `ALWAYS_TRUE` special-case essay in bernoulli.rs) — the "why", including perf numbers and issue refs (`See #968`).
- Security-sensitive types get a **non-leaking manual `Debug`** (`ThreadRng { .. }`) — and a test asserting the exact Debug output.
- Named consts over magic numbers, each with a comment (`RESEED_BLOCK_THRESHOLD` explains the benchmark basis).

## 5. Test / bench organization
- **Unit tests co-located**: `#[cfg(test)] mod test` at file bottom, testing internals (private fields like `p_int`).
- Shared test infra in `crate::test` (in lib.rs's test mod): deterministic seeded `rng(seed)` helper + `StepRng`/`const_rng` fakes — tests never use entropy.
- **Value-stability tests**: hardcoded expected output sequences for reproducibility guarantees (11 files have `value_stability` tests).
- Statistical tests marked `#[cfg_attr(miri, ignore)] // Miri is too slow`.
- `tests/` integration files are minimal — only what needs the external-consumer view (e.g. "Fill may be implemented for externally-defined types").
- Benches: criterion with `harness = false` per `[[bench]]`; each bench file has module doc + one `pub fn bench(c: &mut Criterion)` using `benchmark_group` with explicit `sample_size`/`warm_up_time`/`measurement_time`; deterministic RNG (Pcg32) inside benches too.
- CI matrix is the real test suite: stable/beta/nightly/MSRV/minimal-versions/miri/cross (ppc big-endian)/no_std-target build/iOS build; feature-combination runs (`--no-default-features`, `--features alloc,sys_rng`, `--all-features`); `paths-ignore` for md/benches; weekly cron.

## 6. Release / changelog hygiene
- Keep-a-Changelog + SemVer, stated in the header. `## [Unreleased]` section always at top; sections `### Fixes` / `### Changes` / `### Additions` / `### Removals`; **every line ends with `([#NNNN])`** and PR link refs collected below each release block. Release headings dated `## [0.10.2] — 2026-07-02`. Soundness fixes called out in prose under the release.
- Release automation: tag push (`[0-9].[0-9].*`) triggers **crates.io Trusted Publishing** (OIDC `id-token: write`, no long-lived token secret) via `rust-lang/crates-io-auth-action`.
- SECURITY.md: disclaimer, explicit **security premises** (exact preconditions + what's guaranteed per component), supported-versions policy (patch latest + 12-month window), private disclosure via GitHub security advisories, 90-day window.
- Workflows: `permissions: contents: read` at top (least privilege).

## 7. Conventions to adopt — new multi-crate workspace (process manager: daemon + CLI + client lib)

**Cargo/workspace**
- Workspace with `[workspace.package]` (edition 2024, `rust-version`, license, repository) inherited by member crates; `[workspace.dependencies]` for shared versions. (rand predates being a workspace; this is the modern equivalent of its discipline.)
- Layering mirrors rand↔rand_core: `pm-core` (protocol types + traits, `no_std`-agnostic where possible, zero heavy deps) ← `pm-client` (lib) ← `pm-cli`, `pm-daemon` (bins). Client lib re-exports core (`pub use pm_core;`) so consumers need one dep.
- Every internal+external dep `default-features = false`; optional deps behind `dep:`-syntax features; comment every feature flag inline in Cargo.toml, grouped `# Meta-features:` / `# Option:`.
- Bench crate(s) `publish = false`, own `[workspace]` detach or workspace-member with `autobenches`+criterion isolated; `include = [...]` on published crates.
- Pin true minimum dep versions; add a `minimal-versions` CI job to keep them honest.

**Lints/MSRV**
- Workspace `[workspace.lints]`: `missing_docs = "deny"` (client lib + core at minimum), `missing_debug_implementations = "deny"`, `clippy::undocumented_unsafe_blocks = "deny"`; narrow `allow`s only with justification comments.
- MSRV in `rust-version`; CI: clippy pinned to one recent toolchain with `-D warnings`, separate MSRV build/test job, `cargo fmt --all --check`, doc build with `-Dwarnings --cfg docsrs`, `typos`.

**API/errors**
- Per-operation small error enums in core (`SpawnError`, `ConnectError`, `ProtocolError`), fieldless where possible, `Clone+Copy+Debug+PartialEq+Eq`, manual `Display` (match→`f.write_str`), `impl core::error::Error`. Doc-comment every variant with its exact trigger condition. No catch-all `anyhow` in the client lib (fine in CLI bin).
- `Result` constructors, validation-first early returns; `#[track_caller]` + `# Panics` doc on anything that can panic; `#[must_use]` on builders/handles.
- Manual `Debug` that redacts secrets/tokens/sockets state (daemon auth tokens = rand's CSPRNG-state precedent), plus a test asserting the redacted Debug output.
- `// SAFETY:` on every unsafe block (lint-enforced); long rationale comments for non-obvious design (why this IPC framing, why this reseed/restart threshold) with measured numbers and issue links.

**Docs**
- License header + `//!` module doc in every file; module docs as mini-guides with `# Quick start` runnable example; reference links collected at file bottom; `# Panics`/`# Errors`/`# Security` sections; edge cases as asserted doctests; `docs.rs` metadata `all-features = true` + local-build command comment; `prelude` module in client lib with `#[doc(no_inline)]`.

**Tests**
- Unit tests co-located in `#[cfg(test)] mod test`; shared deterministic fixtures in a crate-level `test` helper mod (fake clock/fake process table = rand's `StepRng` pattern); integration `tests/` only for external-consumer-view checks (trait implementable downstream, CLI e2e); wire-protocol **stability tests** with hardcoded byte sequences (rand's `value_stability` pattern — critical for daemon↔client version skew).
- CI matrix: OS spread (linux/macos/windows), MSRV, minimal-versions, feature-combo runs per crate, miri for any unsafe.

**Release**
- Keep-a-Changelog per publishable crate (or one with per-crate sections), `[Unreleased]` at top, `([#PR])` on every line, Fixes/Changes/Additions/Removals sections.
- Tag-triggered release workflow using crates.io Trusted Publishing (OIDC, no token secret); `permissions: contents: read` default on all workflows.
- SECURITY.md stating premises (what the daemon socket/auth guarantees and under which preconditions), supported-versions window, private advisory reporting. For a process manager this matters more than for most crates — the daemon is a privilege boundary.

Files read: `/Users/rin/GitHub/rand/Cargo.toml`, `clippy.toml`, `src/lib.rs`, `src/distr/bernoulli.rs`, `src/distr/mod.rs`, `src/distr/uniform.rs` (error section), `src/rngs/mod.rs`, `src/rngs/thread.rs`, `src/prelude.rs`, `benches/Cargo.toml`, `benches/benches/bool.rs`, `benches/rustfmt.toml`, `tests/fill.rs`, `CHANGELOG.md`, `SECURITY.md`, `.github/workflows/{test,release}.yml`.
