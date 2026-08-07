# rand lens: Documentation conventions

## Patterns

# rand docs-conventions — extracted patterns

## 1. Crate-level doc gates (lib.rs)
`src/lib.rs:33-48`:
```rust
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]
#![doc(test(attr(allow(unused_variables), deny(warnings))))]
...
#![cfg_attr(docsrs, feature(doc_cfg))]
...
#![deny(clippy::undocumented_unsafe_blocks)]
```
- Docs are lint-enforced, not aspirational: missing docs = build error; every doctest denies warnings globally so examples rot loudly.
- Targeted `#![allow(clippy::float_cmp, ...)]` (lib.rs:43-47) — allows are named individually at crate root, never blanket.
- docs.rs build pinned in Cargo.toml with a *local repro command in a comment* (`Cargo.toml:20-24`): `all-features = true`, `rustdoc-args = ["--generate-link-to-definition"]`, comment shows the exact `RUSTDOCFLAGS=... cargo +nightly doc` line.

## 2. Crate doc = Quick Start only, then link out
`src/lib.rs:10-27`: crate doc is a one-line summary + `# Quick Start` with one runnable example + `See also [The Book: Quick Start](...)`. All taxonomy lives in module docs, not the crate root.

## 3. Module-doc mini-guide anatomy (rngs/mod.rs — the template)
`src/rngs/mod.rs:9-95`. Structure, in order:
1. Title line: `//! Random number generators and adapters` (noun phrase, no period).
2. `## Generators` section, subdivided with **h5 headers** (`##### Non-deterministic generators`, `##### Standard generators`, `##### Named portable generators`, rngs/mod.rs:16,25,41) — h5 keeps the rustdoc sidebar uncluttered while still grouping.
3. Each item is a definition-list bullet, `-   [`Name`] is/provides ...` with 4-space continuation indent and honest caveats inline ("insecure", "should be secure, but see documentation on [`ThreadRng`]", rngs/mod.rs:18-23,36-39).
4. `### Additional generators` — ecosystem crates outside this crate, each with a one-line verdict incl. negatives ("very slow and has [security issues](...)", rngs/mod.rs:58-59), then discovery pointer ("search [crates with the `rng` tag]", :65).
5. `## Traits and functionality` — how the trait hierarchy composes (:67-75), incl. "Use the [`rand_core`] crate when implementing your own RNGs" (where extension writers go).
6. Footnote for citation: `[^1]: D. J. Bernstein, [*ChaCha...*](...)` referenced from body text `[^1]` (:51,77).
7. **Bottom link block** — every reference-style link defined once at file bottom (:79-95), mixing intra-doc (``[`TryRng`]: crate::TryRng``) and external URLs. Prose stays clean; links maintained in one place.
8. Code below the doc mirrors it: private impl modules, curated `pub use` surface with feature gates (:97-119).

## 4. distr/mod.rs variant: Quick start first, then concept taxonomy
`src/distr/mod.rs:12-22`: `# Quick start` with runnable example is the FIRST section; concepts (`# Distribution trait`, `## The Standard Uniform distribution`, `## Non-uniform sampling`) follow. Explicit scope rejection with alternatives: "This crate no longer includes other non-uniform distributions; instead it is recommended that you use either [`rand_distr`] or [`statrs`]" (:84-85).

## 5. Item-doc section order (fixed house order)
Observed across `make_rng` (lib.rs:77-100), `Bernoulli` (bernoulli.rs:18-45), `ThreadRng` (thread.rs:75-130), `rng()` (thread.rs:169-200), `random_bool` (rng.rs:172-190):

**summary line → prose (incl. "See also"/"This function is shorthand for") → `# Example(s)` → `# Panics` → domain sections (`# Security`, `# Precision`, `# Forks and interrupts`) → bottom link block.**

- Summary line is a fragment, no trailing period, states what you GET: "Construct and seed an RNG" (lib.rs:77), "Access a fast, pre-initialized generator" (thread.rs:169).
- Delegating wrappers state the delegation precisely with nested links in HTML code: `/// This function is shorthand for <code>[rng()].[random()](RngExt::random)</code>` (lib.rs:152; `<var>range</var>` for parameters, lib.rs:220).
- Cross-item section reference instead of duplication: `/// # Security` / `/// Refer to [`ThreadRng#Security`].` (lib.rs:94-96) with anchor link `[`ThreadRng#Security`]: crate::rngs::ThreadRng#security` (lib.rs:100). One canonical Security writeup, N pointers.
- `# Panics` states the condition, terse: "If `p < 0` or `p > 1`." (lib.rs:254-256); qualifies likelihood: "This is unlikely outside of early boot or unusual system conditions" (lib.rs:91-92).
- **`# Panics` doc + `#[track_caller]` travel together** (lib.rs:89-102, rng.rs:137-162). CHANGELOG 0.10.1 treats them as one change: "Document panic behavior of `make_rng` and add `#[track_caller]` ([#1761])" (CHANGELOG.md:35).
- Perf guidance lives in docs as "See also X, which may be faster if..." (rng.rs:174-175, lib.rs:272-273).
- `ThreadRng`'s `# Security` is a *design-criteria list with explicit limitations* — "The Rand project can provide no guarantee of fitness for purpose. The design criteria ... are as follows:" then bullets incl. what it does NOT do (thread.rs:84-104).

## 6. Error-type doc discipline
`src/distr/bernoulli.rs:78-93`:
```rust
/// Error type returned from [`Bernoulli::new`].
pub enum BernoulliError {
    /// `p < 0` or `p > 1`.
    InvalidProbability,
}
```
- Type doc names the fns that return it (intra-doc link). Each variant doc is the *condition* that produces it (also uniform.rs:121-127: "`low > high`, or equal in case of exclusive range."). `Display` gives a human sentence, `impl core::error::Error` always present (bernoulli.rs:85-93).

## 7. Doctest idioms
- **Edge cases as asserted doctests with verdict comments** — docs double as spec (bernoulli.rs:136-140):
```rust
/// // Edge cases:
/// assert_eq!(Bernoulli::from_ratio(3, 3).unwrap().p(), 1.0); // always true
/// assert!(Bernoulli::from_ratio(4, 3).is_err());             // numerator > denominator
```
- **Hidden lines (`# `)**: exercise the result to defeat unused-warnings without polluting the rendered example — `/// # let _ = rand::Rng::next_u32(&mut rng);` (lib.rs:86); `# #![allow(dead_code)]` for skeleton types (distr/mod.rs:172); `# fn main() { ... # }` wrapper when needed (thread.rs:180-191).
- **`no_run`** for examples with side effects (writes `/tmp/random.bytes`) — still compile-checked (lib.rs:119-128).
- **`ignore`** only when the example cannot compile in-crate (fork example uses `libc`, not a dependency) — and the surrounding prose explains what it demonstrates (thread.rs:110-118).
- Examples show the *better pattern* proactively: "If you're calling `random()` repeatedly, consider using a local `rng` handle" + second example (lib.rs:171-184; thread.rs:186 same).

## 8. Intra-doc-link rules
- Reference-style definitions collected in a bottom block per doc comment/module, never inline paths in prose (lib.rs:98-100,186-187; rngs/mod.rs:79-95).
- **`#[cfg(doc)] use crate::RngExt;`** — import that exists only so intra-doc links resolve, without an unused-import warning in normal builds (distr/distribution.rs:17-18, distr/uniform.rs:118-119).
- `#[doc(inline)]` on the one re-export that should render as a local item (distr/mod.rs:118-119); `#[doc(hidden)] pub mod hidden_export` for cross-crate-internal API with a comment naming the consumer (`// used by rand_distr`, distr/mod.rs:103-106).
- Doc comment on trait impls too: `/// Debug implementation does not leak internal state` (thread.rs:150-151) + unit test pinning exact Debug output `"ThreadRng { .. }"` (thread.rs:253-258).
- Non-obvious internals get *block comments above the item*, not doc comments: UnsafeCell rationale essay (thread.rs:21-33), ALWAYS_TRUE special-case rationale (bernoulli.rs:53-71) — implementation reasoning stays out of rendered docs.

## 9. README anatomy (README.md)
Badges row (:3-6) → what-it-is bullet groups (:8-31) → **"Rand *is not*:" anti-scope section** (:33-45) naming competitor alternatives (fastrand, oorandom) and punting security fitness to the user → Documentation links (:47-50) → Versions w/ maturity statement (:53-60) → Crate Features, one line per feature stating what it enables + implication chains ("`thread_rng` (implies `std`, `std_rng`, `sys_rng`)", :64-86) → Portability/platform notes → License. Link-refs at bottom (:110-111).

## 10. CHANGELOG house style (CHANGELOG.md)
- Header: "The format is based on [Keep a Changelog] ... adheres to [Semantic Versioning]" (:4-5); pointer to sibling-crate changelog and Upgrade Guide (:7-9).
- `## [Unreleased]` section always present at top (:11).
- Release heading: `## [0.10.2] — 2026-07-02` (:18). Optional one-line severity callout under the heading: "This release includes a fix for a soundness bug; see [#1763]." (:32).
- Category subsections in rough order: `### Security and unsafe` / `### Fixes` / `### Changes` / `### Additions` / `### Removals` / `### Deprecated` (:13,20,23,57,63,88).
- Entry grammar: imperative verb + backticked API paths + old->new renames spelled out (`Rename `os_rng` -> `sys_rng``, :54) + `([#NNNN])` PR ref. Link definitions collected per-release at section bottom (:68-84).

## Anti-patterns rand's setup forbids
- Undocumented public items (`deny(missing_docs)`), undocumented unsafe (`deny(clippy::undocumented_unsafe_blocks)` — every `unsafe` block in src carries `// SAFETY:`, e.g. rng.rs:364-368, thread.rs:143-144).
- Warning-producing doctests (`doc(test(attr(deny(warnings))))`).
- Types without Debug (`deny(missing_debug_implementations)`) — and Debug that leaks state (tested, thread.rs:253-258).
- Inline bare URLs in prose; duplicated Security/Precision text across items (anchor-links instead); implementation rationale in rendered docs (block comments instead).

## Apply to shep

# shep translation — docs house rules

## Crate roots (all four crates)
Every `lib.rs`/`main.rs` gets the gate block, adapted:
```rust
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]
#![doc(test(attr(deny(warnings))))]
#![deny(clippy::undocumented_unsafe_blocks)]
```
(drop `allow(unused_variables)` — shep doctests are Result-driven, unused vars should fail). Any `#![allow(...)]` must name the specific lint at crate root with a reason. `[package.metadata.docs.rs]` in each Cargo.toml with `all-features = true`, `rustdoc-args = ["--generate-link-to-definition"]`, and a comment holding the local repro command (rand Cargo.toml:20-24 pattern).

Crate doc = summary line + `# Quick start` with ONE runnable (or `no_run`) example + link to deeper docs. Taxonomy goes in module docs. shep-client's crate doc quick start: connect + list processes, `no_run`.

## Module-doc skeleton — shep-daemon::supervisor (rngs/mod.rs template)
```rust
//! Process supervision and lifecycle management
//!
//! ## Supervisors
//!
//! ##### Restart policies
//! -   [`Always`] restarts on any exit. Default for services.
//! -   [`OnFailure`] restarts only on non-zero exit; see caveat on
//!     signal-terminated processes below.
//! ...
//! ### Related functionality
//! -   [`shep_core::config`] defines the on-disk policy schema
//! ...
//! ## Traits and functionality
//! (how Supervisor/Handle/Spawner compose; where to implement extensions)
//!
//! [`Always`]: crate::supervisor::RestartPolicy::Always
//! [Restart chapter]: https://…
```
Rules: title = noun phrase, no period. h2 for major sections, h5 for taxonomy sub-groups (sidebar hygiene). Each type = definition-bullet with honest caveat inline ("not fork-safe", "loses output on SIGKILL"). One "Related/ecosystem" subsection pointing across crates (shep-core types, shep-client). ALL links in a bottom reference block. Module file body mirrors doc: private impl mods, curated feature-gated `pub use` list.

## Item-doc skeleton — a spawn fn (shep-daemon or shep-client)
Fixed section order: **summary fragment (no period) → prose/See-also → `# Example` → `# Errors` → `# Panics` (only if it can) → domain section (`# Cancellation safety`, `# Security`) → link block.**
```rust
/// Spawn a managed process under this supervisor
///
/// The child inherits the daemon's environment filtered by
/// [`EnvPolicy`]. See also [`Supervisor::spawn_batch`], which may be
/// faster when starting many processes.
///
/// # Example
///
/// ```no_run
/// # #[tokio::main]
/// # async fn main() -> Result<(), shep_client::Error> {
/// let client = shep_client::Client::connect_default().await?;
/// let id = client.spawn(ProcessSpec::new("worker", "/usr/bin/worker")).await?;
/// # Ok(()) }
/// ```
///
/// # Errors
///
/// - [`Error::Io`] if the executable cannot be started.
/// - [`Error::LimitExceeded`] if the supervisor is at `max_children`.
///
/// # Cancellation safety
///
/// Dropping the returned future before completion may leave the child
/// running; it will be adopted by the supervisor's reaper.
///
/// [`EnvPolicy`]: shep_core::config::EnvPolicy
```
Additional rules from rand:
- **`# Errors` replaces rand's `# Panics` as the mandatory section** for shep's Result-heavy API; keep `# Panics` too whenever a panic path exists, and pair every documented panic with `#[track_caller]` — treat the doc + attribute as one commit unit (rand CHANGELOG.md:35 precedent).
- Thin wrappers (shep-cli commands wrapping shep-client calls, shep convenience fns) document as "This function is shorthand for <code>[Client::x].[y](...)</code>" — precise delegation, no duplicated behavior text.
- Write ONE canonical `# Security` writeup (shep-daemon socket-permissions / env-leak section on the `Daemon` type, styled as design-criteria bullets + explicit non-goals, thread.rs:84-104 model) and anchor-link it from everything else: `/// Refer to [`Daemon#Security`].` + `[`Daemon#Security`]: crate::Daemon#security`.

## Error types (shep-core)
Every error enum: type doc names returning fns via intra-doc links; each variant doc states the *condition* ("socket path exceeds `SUN_LEN`"), not a restatement of the name; `Display` = human sentence; `impl core::error::Error`. Wire-protocol error codes documented on the variant that carries them.

## Doctest idioms
- **`no_run` is the default for anything daemon-touching** (connect, spawn, RPC): compile-checked, never executed in CI. Wrap async with hidden lines:
  `/// # #[tokio::main]` / `/// # async fn main() -> Result<(), Error> {` ... `/// # Ok(()) }`. (tokio must be a dev-dependency of the doc'd crate.)
- **`ignore` only when it cannot compile in-crate** (example depends on something not in the dep tree — rand's libc fork example, thread.rs:110). Always precede with prose saying what it shows; prefer restructuring over `ignore`.
- Edge-case asserts as doctests on pure shep-core logic (config parsing, backoff math, wire-frame encode/decode): `assert!(RestartPolicy::parse("never").is_ok());` with `// verdict` comments — the bernoulli.rs:136-140 pattern. These run in CI; daemon examples don't.
- Hidden `# let _ = ...;` lines to consume results and keep `deny(warnings)` doctests green.
- When a naive usage exists, show the better pattern in the same doc ("reuse one `Client` across calls instead of reconnecting") — rand lib.rs:171-184 model.

## Intra-doc links
- Reference-style bottom blocks everywhere; no inline `crate::` paths or bare URLs in prose.
- Cross-crate links (shep-cli docs → shep_core types): `#[cfg(doc)] use shep_core::ProcessSpec;` when needed purely for link resolution.
- `#[doc(inline)]` on the curated re-exports (shep-daemon re-exporting `shep_core::ProcessSpec`); `#[doc(hidden)] pub mod` + `// used by shep-daemon` comment for wire-protocol internals shep-core must expose across the workspace but not to users.
- **Debug redaction is a documented, tested contract**: `ProcessSpec`/config types holding env vars or secrets get `/// Debug implementation does not leak environment values` on the impl + a unit test asserting the exact redacted output (thread.rs:150-155,253-258 pattern).
- Implementation rationale (why UnsafeCell, why this reap strategy, PID-reuse race notes) = `//` block comments above the item, never `///`.

## README (workspace root)
Order: badges → what-shep-is bullet groups (one per crate) → **"shep is not:" section** naming systemd/pm2/supervisord and when to use them instead → docs links → versions/maturity statement → Crate Features table (one line each, implication chains spelled out: "`daemon` (implies `protocol`, `config`)") → platform notes (macOS launchd interaction, Linux cgroups) → license. Link-refs at bottom.

## CHANGELOG house style
- One `CHANGELOG.md` per published crate (rand keeps rand_core's separate and links it, CHANGELOG.md:7) — shep-core/shep-daemon/shep-client/shep-cli each get one; workspace root links them.
- Keep a Changelog + SemVer declaration at top; permanent `## [Unreleased]` section.
- Release heading `## [0.3.1] — 2026-08-07`; optional severity line beneath ("This release fixes a daemon deadlock; see [#42].").
- Sections in order: `### Security and unsafe`, `### Fixes`, `### Changes`, `### Additions`, `### Removals`, `### Deprecated` — omit empty ones.
- Entry grammar: imperative verb, backticked API paths, renames as `old` -> `new`, wire-protocol version bumps called out explicitly, every entry ends `([#NN])` with the link definitions collected at the end of that release's block.
