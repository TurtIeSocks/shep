# rand lens: Testing discipline

## Patterns

## Testing-discipline patterns in rand 0.10.2

### 1. Crate-root shared fixture module: `#[cfg(test)] pub mod test` with a documented deterministic RNG factory
`src/lib.rs:318-339`:
```rust
#[cfg(test)]
mod test {
    /// Construct a deterministic RNG with the given seed
    pub fn rng(seed: u64) -> impl Rng {
        // For tests, we want a statistically good, fast, reproducible RNG.
        // PCG32 will do fine, and will be easy to embed if we ever need to.
        const INC: u64 = 11634580027462260723;
        rand_pcg::Pcg32::new(seed, INC)
```
One factory, used by every co-located test in the crate (`crate::test::rng(N)` at src/distr/bernoulli.rs:200,223,241; src/rng.rs:461; src/distr/weighted/weighted_index.rs:421; src/seq/index.rs:590...). The fixture is NOT the production RNG — it's a dev-dependency chosen for the three properties named in the comment: statistically good, fast, reproducible. The "why this fake" rationale is written at the definition site. Returns `impl Rng`, hiding the concrete type so the fixture can be swapped.

### 2. Two-tier fakes: realistic fixture + trivial scripted fake
`src/lib.rs:331-342`:
```rust
    /// Construct a generator yielding a constant value
    pub fn const_rng(x: u64) -> StepRng { StepRng(x, 0) }
    /// Construct a generator yielding an arithmetic sequence
    pub fn step_rng(x: u64, increment: u64) -> StepRng { StepRng(x, increment) }
    #[derive(Clone)]
    pub struct StepRng(u64, u64);
```
`StepRng` is a hand-rolled trait impl (lib.rs:343-359) whose output is fully predictable, used when the test must know the exact input bits (src/rng.rs:416 `const_rng(0x11_22_33_44_55_66_77_88)`, src/rng.rs:437). The realistic fixture (`rng(seed)`) is for behavior; the trivial fake is for byte-exact plumbing assertions.

### 3. Unique seed per test — never shared
Every test passes a distinct literal seed: 1,2,3 (bernoulli.rs:200,223,241), 101-111 (rng.rs:468-569), 404/410/420-422 (seq/index.rs:590,697,621-640), 700 (weighted_index.rs:421), 805-807/820 (other.rs:321-377). No two tests correlate their streams; a failure is reproducible in isolation and adding a test never shifts another's data.

### 4. Value-stability tests: pinned output sequences as a public contract
Three flavors, all co-located:
- Hardcoded expected sequence per distribution — `src/distr/bernoulli.rs:239-253`:
```rust
    #[test]
    fn value_stability() {
        let mut rng = crate::test::rng(3);
        let distr = Bernoulli::new(0.4532).unwrap();
        let mut buf = [false; 10];
        for x in &mut buf { *x = rng.sample(distr); }
        assert_eq!(buf, [true, false, false, true, ...]);
```
- Local generic helper deduplicating the loop — `src/distr/other.rs:372-383` (`fn test_samples<T: Copy + Debug + PartialEq, D: Distribution<T>>(distr, zero, expected)` then ~12 one-line cases at :385-438). Same helper re-declared per test mod (float.rs:279, integer.rs:210, uniform.rs:575...) — small duplication preferred over a shared abstraction.
- Reference vectors from an external authority, with source URL in a comment — `src/rngs/xoshiro256plusplus.rs:114-115`: "These values were produced with the reference implementation: http://xoshiro.di.unimi.it/xoshiro256plusplus.c"; `src/rngs/std.rs:132-136` cites the ChaCha test-vector RFC draft.
The intent is stated in-test: `src/rngs/std.rs:113-114` "Test value-stability of StdRng. This is expected to break any time the algorithm is changed." And breakage is CHANGELOG-tracked as semver-relevant: CHANGELOG.md:203 "(breaks value stability; [#1287])". Failing case context goes in the assert message: `src/seq/index.rs:705-709` `assert_eq!(&buf[0..len], values, "failed sampling {}, {}", length, amount)`.

### 5. Debug-output assert as a leak test
`src/rngs/thread.rs:253-258`:
```rust
    #[test]
    fn test_debug_output() {
        // We don't care about the exact output here, but it must not include
        // private CSPRNG state or the cache stored by BlockRng!
        assert_eq!(std::format!("{:?}", crate::rng()), "ThreadRng { .. }");
    }
```
Paired with `#![deny(missing_debug_implementations)]` (src/lib.rs:34): every type MUST have Debug, and Debug of secret-bearing types is pinned to a redacted form by test.

### 6. Statistical tier: tolerance asserts, gated off miri
14 tests across 9 files carry `#[cfg_attr(miri, ignore)] // Miri is too slow` (bernoulli.rs:212, rng.rs:562, weighted_index.rs:419, seq/index.rs:610...). These do bulk runs with explicit numeric tolerances: bernoulli.rs:219-233 (`N = 100_000; assert!((avg1 - P).abs() < 5e-3)`), weighted_index.rs:426-434 (per-bucket relative error `err <= 0.25`). Exact-value tests stay miri-clean; only the slow bulk tier is skipped.

### 7. API-shape tests: dyn-compat and generic-usage smoke tests
`src/rng.rs:538-559`:
```rust
    fn test_rng_trait_object() {
        let mut r = &mut rng as &mut dyn Rng;
        r.next_u32();
```
plus `Box<dyn Rng>` (:549-559, feature-gated on alloc) and `use_rng(&mut rng)` through `impl RngExt` (:529-536). These pin the trait's object-safety and blanket-impl ergonomics so a refactor can't silently break downstream call shapes. `src/distr/other.rs:320-325` does the same with `&mut dyn Rng` through sampling.

### 8. tests/ reserved for crate-boundary concerns only
The whole `tests/` dir is ONE file. `tests/fill.rs:13-19`:
```rust
// Test that Fill may be implemented for externally-defined types
struct MyInt(i32);
impl Fill for MyInt {
    fn fill_slice<R: Rng + ?Sized>(this: &mut [Self], rng: &mut R) { todo!() }
```
Compile-only (`todo!()` body, `#![allow(unused)]` at :9): it proves an external crate can implement the public trait — something a co-located test cannot check because it has crate-private visibility. Everything behavioral is co-located `#[cfg(test)]`.

### 9. Boundary sweeps and exact error-variant asserts
- Exhaustive remainder sweep: `src/rng.rs:418-420` "check every remainder mod 8" — `lengths = [0,1,...,7, 80..87]`; empty-slice case has its own test (rng.rs:458-464).
- Zero/one boundaries: `src/seq/index.rs:588-597` (`sample_inplace(&mut r, 0, 0)`, `(1,0)`, `(1,1)`).
- Panics tested with `#[should_panic]` + targeted clippy allow: `src/rng.rs:502-508` `#[allow(clippy::reversed_empty_ranges)]`.
- Errors derive PartialEq so tests assert the exact variant: `src/distr/weighted/weighted_index.rs:396-399` `assert_eq!(WeightedIndex::new([f32::NAN, 0.5]).unwrap_err(), Error::InvalidWeight)`; empty/zero inputs each mapped to distinct variants (:472-479).
- Explicit-over-lint asserts with justification: `src/distr/bernoulli.rs:197-198` "// We prefer to be explicit here. #![allow(clippy::bool_assert_comparison)]".

### 10. Serde tests: round-trip via postcard + pinned cross-arch wire form
Round-trip with a compact no_std codec: `src/distr/bernoulli.rs:186-193` (`postcard::from_bytes(&postcard::to_allocvec(...))`, asserts on the inner field), weighted_index.rs:378-392. And the stronger contract — a serialized string produced on ANOTHER architecture, committed to the test, must still deserialize: `src/distr/uniform_int.rs:889-892` (`serialized_on_32bit` fixture deserialized on the host). Dev-deps carry both codecs (Cargo.toml:73-77: postcard "Only to test serde", serde_json).

### 11. Test-only scaffolding compiled into prod files but cfg'd out
`src/distr/utils.rs:235-241`: an entire helper trait exists only under test:
```rust
#[cfg(test)]
pub(crate) trait FloatSIMDScalarUtils: FloatSIMDUtils {
```
plus `#[cfg(test)] const LEN` on a production trait (:245-246) and `#[cfg(test)]` impls inside a macro (:313-314). Test hooks live next to the code, cost nothing in release.

### 12. Benches: separate unpublished workspace, criterion, deterministic-algorithm RNGs, CI-compiled
- `benches/Cargo.toml:1-6`: own `[workspace]`, `publish = false`; every `[[bench]]` has `harness = false` (:27-60).
- Group tuning kept short: `benches/benches/bool.rs:24-27` `sample_size(1000); warm_up_time(500ms); measurement_time(1000ms)`.
- Benches use a cheap fixed-algorithm generator (`let mut rng: Pcg32 = rand::make_rng()` bool.rs:30) so measurements profile the code under test, not the default RNG.
- `black_box` on inputs and outputs plus comments defending against optimizer shortcuts: `benches/benches/seq_choose.rs:26,38-42` and :45-47 "Collect full result to prevent unwanted shortcuts".
- CI compiles AND runs benches as tests so they never rot: `.github/workflows/benches.yml:42` `RUSTFLAGS=-Dwarnings cargo test --benches`, with a dedicated clippy+fmt job for the bench crate (:20-32).

### 13. CI matrix is the real test suite (`.github/workflows/test.yml`)
- Triggers: push/PR filtered by `paths-ignore: **.md, benches/**` (:3-13) + weekly cron `'0 0 * * SUN'` (:14-15) to catch toolchain/dependency drift with no code change.
- Least-privilege token: `permissions: contents: read` (:17-18).
- Separate fast lint jobs: clippy PINNED to toolchain 1.93 with `-D warnings` (:26-30) — pinning stops new nightly lints from breaking CI nondeterministically; fmt on stable (:32-40); docs built with `RUSTDOCFLAGS: "-Dwarnings --cfg docsrs ... --generate-link-to-definition"` on nightly (:42-52); `crate-ci/typos` (:54-58).
- Test matrix `fail-fast: false` (:63): linux/macos/windows stable; windows twice (gnu + msvc, one on beta, :73-79); explicit `variant: MSRV, toolchain: 1.85.0` (:80-83); i686 32-bit (:84-87); nightly + `variant: minimal_versions` with `cargo generate-lockfile -Z minimal-versions` (:88-91, :101-104); non-MSRV jobs run `cargo generate-lockfile --ignore-rust-version` (:105-107).
- Feature-combo test ladder per job (:114-122): `--lib --tests --no-default-features`, build with a mid-tier feature subset (`alloc,sys_rng,unbiased`), test with `alloc,sys_rng`, `--examples`, then `--features=serde,log` (all stable features).
- `test-cross` on powerpc (big-endian!) via `cross` with a cargo-plugin cache (:124-152) — endianness portability actually executed, `--no-fail-fast`.
- `test-miri` (:154-166) — UB detection for the unsafe code; note tests too slow for miri are the `#[cfg_attr(miri, ignore)]` set above, so the job stays fast.
- Build-only tiers where tests can't run: `test-no-std` thumbv6m (:168-177), `test-ios` (:179-188) — `cargo build` only.
- `release.yml:8-19`: crates.io Trusted Publishing via OIDC (`id-token: write` + `rust-lang/crates-io-auth-action`) — no long-lived registry secret.

### Anti-patterns rand visibly avoids
- No `thread_rng`/entropy in unit tests — nondeterminism is quarantined to two smoke tests that only assert types/ranges (src/lib.rs:379-398).
- No sleeps, no retries, no flaky tolerance hiding: statistical tests state their N and epsilon (bernoulli.rs:219,233).
- No mocking framework — hand-rolled 20-line fakes implementing the real trait (lib.rs:342-359).
- No blanket `#[allow]`: allows are per-test, narrow, and justified in a comment (bernoulli.rs:197-198); crate denies `missing_docs`, `missing_debug_implementations`, `clippy::undocumented_unsafe_blocks` (lib.rs:33-48); doctests deny warnings (`#![doc(test(attr(...deny(warnings))))]` lib.rs:35).
- No giant tests/ directory duplicating unit coverage — one compile-only boundary check.

## Apply to shep

## shep translation — test strategy rules

### Fixture module (rand lib.rs:318 analog) — `shep-daemon`
- Rule: each crate with behavioral tests gets ONE crate-root `#[cfg(test)] pub(crate) mod test` fixture module. In shep-daemon it exports:
  - `fn clock() -> ...`: every async test starts `tokio::time::pause()` — the fake-clock analog of `rng(seed)`. Wrap in `#[tokio::test(start_paused = true)]` as the default; a test using real time must justify it in a comment.
  - `fn proc_table(script: &[ProcScript]) -> FakeProcRunner`: hand-rolled impl of the daemon's `ProcessRunner` trait (the trait must exist for this reason — seam by design, like `TryRng`). Two tiers, mirroring `const_rng`/`step_rng`:
    - `const_proc(exit: ExitStatus)` — every spawn exits immediately with `exit`.
    - `script_proc(vec![Spawn(ok), ExitAfter(ms, code), Signal(SIGKILL), ...])` — fully predetermined lifecycle sequence.
  - Rule: write the WHY comment at the factory (rand lib.rs:325-326 pattern): "fast, deterministic, reproducible; real spawning tested only in CLI e2e".
- Rule: unique scenario per test — no shared fixture state, no shared tempdirs; each test builds its own script/config literal (rand's unique-seed discipline).

### Test taxonomy — style-guide section, applies workspace-wide
1. **Trivial/edge tier** (all crates): exact asserts, boundary sweeps. Supervisor: 0 processes, 1 process, restart-limit 0/1/max, empty config, config with every field defaulted. Copy rand's "every remainder mod 8" habit: for wire framing in shep-core, test every partial-read length around the frame-header size (rng.rs:418-420 analog).
2. **Stability tier** (shep-core): wire-protocol stability snapshots. rand's value_stability == shep's protocol stability:
   - insta snapshots of the serialized form of every request/response variant (`insta::assert_json_snapshot!` for the JSON wire, `assert_debug_snapshot!` for internal state) — the pinned-sequence analog.
   - PLUS the stronger uniform_int.rs:889 pattern insta can't give you: commit literal serialized bytes/strings from the PREVIOUS protocol version as fixtures and assert they still deserialize (`old_v1_status_response` const → `from_slice` ok). Breaking one is a protocol-version bump, recorded in CHANGELOG (rand CHANGELOG.md:203 "breaks value stability" discipline).
   - Deterministic-sequence tests in shep-daemon: with paused clock and scripted proc table, assert the EXACT sequence of restart instants/backoff delays as a pinned array — bernoulli.rs:239-253 shape.
3. **Property tier** (shep-daemon): proptest on the supervisor state machine — random event interleavings (exit/stop-request/start-request/signal) must uphold invariants (never two live PIDs per unit; state always reaches a terminal or steady state; restart count monotonic). This is rand's statistical tier; like rand, give it explicit bounds and gate it: `#[cfg_attr(miri, ignore)]` only if a miri job exists (see CI), and cap proptest cases in CI via env.
4. **API-shape tier** (shep-client, shep-daemon): if a trait is public (ProcessRunner, transport trait), pin dyn-compat: `let r: &mut dyn ProcessRunner = ...` and `Box<dyn ...>` smoke tests (rng.rs:538-559). `tests/` directory rule: ONE compile-only file per crate at most, proving an external crate can implement the public trait (tests/fill.rs pattern — `todo!()` bodies fine). Everything behavioral is co-located `#[cfg(test)]`.
5. **Leak tier** (shep-core): `test_debug_output` analog — any type carrying env vars/secrets (ProcessConfig with env) pins its Debug to a redacted form: `assert_eq!(format!("{cfg:?}"), "ProcessConfig { env: .., .. }")` (thread.rs:253-258). Pair with `#![deny(missing_debug_implementations)]` in every crate.
6. **E2E tier** (shep-cli): assert_cmd + tempfile: each test gets a fresh temp `SHEP_HOME`; daemon socket path inside it; asserts on exit code + stdout snapshot (insta `assert_snapshot!` of normalized output). This is rand's `cargo test --examples` slot — real binary, real spawn, few tests.
- Assert messages carry case context when a helper loops over cases: `assert_eq!(got, want, "failed case {name}")` (seq/index.rs:705-709).
- Errors in shep-core derive `PartialEq` so tests write `assert_eq!(err, Error::PortInUse)` — never `matches!` string matching (weighted_index.rs:396-416).
- Local per-mod `fn check(...)` helpers over shared test-util abstractions — rand re-declares `test_samples` in 6 mods rather than exporting one (other.rs:372 et al.). Small duplication wins.
- Test-only hooks: prefer `#[cfg(test)]` methods/traits next to prod code (distr/utils.rs:235) over `pub` inspection APIs.

### Lints (workspace `[workspace.lints]` + per-crate lib.rs)
- `#![deny(missing_docs)]`, `#![deny(missing_debug_implementations)]` (lib.rs:33-34); `#![doc(test(attr(deny(warnings))))]` so doctests can't rot (lib.rs:35).
- `#![deny(clippy::undocumented_unsafe_blocks)]`; shep goes further: `#![forbid(unsafe_code)]` in shep-core/shep-client/shep-cli; shep-daemon `deny` (it may need libc signal bits) — this decides the miri question below.
- Narrow allows only, at the smallest scope, with a one-line reason (bernoulli.rs:197-198).
- `clippy.toml` with `doc-valid-idents` for domain words (rand clippy.toml:2 — shep: "PID", "SIGKILL", "systemd").

### Benches — separate `benches/` workspace member
- `publish = false`, own `[workspace]`, criterion `harness = false` per bench (benches/Cargo.toml pattern).
- Bench targets: supervisor event-loop throughput, wire encode/decode, status-table render. Feed them the FakeProcRunner + fixed configs — deterministic input so criterion variance is the code's, not the fixture's (Pcg32-in-benches analog, bool.rs:30).
- `black_box` inputs/outputs; comment any construct that exists to defeat the optimizer (seq_choose.rs:26,45-47). Short measurement windows for cheap ops (bool.rs:24-27).
- CI: benches never rot — dedicated workflow running `RUSTFLAGS=-Dwarnings cargo test --benches` + clippy/fmt for the bench crate (benches.yml:20-42).

### CI matrix proposal (map of rand test.yml → shep)
Workflow `test.yml`, `permissions: contents: read`, `fail-fast: false`, `paths-ignore: ["**.md", "benches/**"]`, weekly cron for drift detection (rand test.yml:3-18,63):
- **clippy** — pinned toolchain version (not `stable`), `cargo clippy --workspace --all-targets -- -D warnings` (rand pins 1.93, test.yml:26-30). Bump the pin deliberately in a PR.
- **fmt** — stable, `cargo fmt --all -- --check`.
- **doc** — nightly, `RUSTDOCFLAGS="-Dwarnings --cfg docsrs --generate-link-to-definition" cargo doc --workspace --all-features --no-deps` (test.yml:45-52).
- **typos** — `crate-ci/typos@v1` (test.yml:54-58).
- **test matrix** (rand test.yml:60-122):
  - ubuntu stable, macos stable (shep is Unix — the windows rows map to nothing; macos IS shep's "second OS" and covers kqueue/BSD signal differences the way rand's windows-gnu/msvc covers ABI differences). One row on `beta` for early warning.
  - MSRV row: pinned `toolchain: 1.85.0`-style + `variant: MSRV`; all other rows `cargo generate-lockfile --ignore-rust-version` (test.yml:80-83,105-107).
  - nightly + `minimal_versions` row: `cargo generate-lockfile -Z minimal-versions` then test — catches under-specified dep bounds in shep-core's serde/tokio ranges (test.yml:88-91,101-104).
  - **Feature-combo ladder per crate** (rand's :114-122): `cargo test -p shep-core --no-default-features`, each optional feature singly (e.g. `json-logs`, `systemd-notify`), then `--workspace --all-features`. Script the ladder explicitly in the workflow like rand does — no cargo-hack needed at 2-3 features, adopt cargo-hack if the set grows.
- **test-cross → musl static** (rand's powerpc big-endian row, test.yml:124-152): `x86_64-unknown-linux-musl` — RUN tests, not just build (`cross test --no-fail-fast` or native musl target on ubuntu). This is shep's real deployment artifact; different libc is shep's "different endianness". Add `aarch64-unknown-linux-musl` as BUILD-ONLY tier (rand's thumbv6m/iOS build-only pattern, test.yml:168-188).
- **miri → only-if-unsafe** (rand test.yml:154-166): shep-core/client/cli are `forbid(unsafe_code)` → no miri job for them. Add a miri job ONLY if shep-daemon grows real unsafe (libc signal handling); scope it `cargo miri test -p shep-daemon` and rely on `#[cfg_attr(miri, ignore)]` for proptest/e2e tiers. Until then, the honest analog is nothing — don't cargo-cult a miri job that exercises zero unsafe.
- **release.yml**: crates.io Trusted Publishing via OIDC (`environment: release`, `id-token: write`, `rust-lang/crates-io-auth-action`) — no `CARGO_REGISTRY_TOKEN` secret stored (rand release.yml:8-19). Tag-pattern triggered.

### Anti-pattern rules for the style guide (from what rand avoids)
- Never use real time/real sleep in unit tests; nondeterminism is confined to the e2e tier, and even there assert on ranges, not exact timings (rand confines entropy to two type-check smoke tests, lib.rs:379-398).
- No mocking crates — hand-rolled fakes implementing the real trait (StepRng is 20 lines, lib.rs:342-359).
- No `tests/` sprawl duplicating unit coverage — co-locate; `tests/` is for the crate boundary only.
- No unexplained `#[allow]`; no workspace-level allow of anything clippy denies.
- Every stability snapshot break is a CHANGELOG entry, not a silent snapshot re-accept — `cargo insta review` diffs get the same scrutiny rand gives value-stability breaks (CHANGELOG.md:203).
