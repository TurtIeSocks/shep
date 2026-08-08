# rand lens: Rigor: unsafe, perf, features, errors

## Patterns

## rand 0.10.2 — rigor-and-features lens: extracted patterns

### 1. Feature ladder: commented, additive, `dep:`-explicit
`Cargo.toml:29-62`. Every feature carries a comment classifying it (`# Meta-features:`, `# Option:`, `# Option (enabled by default):`, `# Deprecated option:`) plus one line of consequence:
```toml
# Option (enabled by default): without "std" rand uses libcore; this option
# enables functionality expected to be available on a standard platform.
std = ["alloc", "getrandom?/std"]
```
- `dep:` syntax hides optional deps from the feature namespace: `serde = ["dep:serde"]` (`Cargo.toml:32`), `sys_rng = ["dep:getrandom", "getrandom/sys_rng"]` (`Cargo.toml:42`).
- Weak-dependency feature `getrandom?/std` (`Cargo.toml:36`) — enable a dep's feature only if the dep is already pulled in.
- Features compose upward, never conflict: `thread_rng = ["std", "std_rng", "sys_rng"]` (`Cargo.toml:51`).
- Behavior-toggling feature documents its reproducibility cost inline (`unbiased`, `Cargo.toml:56-59`).

### 2. Dependency discipline
`Cargo.toml:64-68`: every non-trivial dep is `default-features = false` with needed features enumerated (`rand_core`, `chacha20`). Dev-deps get the same treatment plus purpose comments: `# Only to test serde` above `postcard` with `default-features = false` (`Cargo.toml:72-73`).

### 3. Packaging metadata
- `include = ["src/", "LICENSE-*", "README.md", "CHANGELOG.md", "COPYRIGHT"]` (`Cargo.toml:18`) — published package is only what users need.
- docs.rs metadata with the exact local-repro command as a comment (`Cargo.toml:20-24`): `all-features = true`, `rustdoc-args = ["--generate-link-to-definition"]`.
- `rust-version = "1.85"` pinned MSRV (`Cargo.toml:17`).

### 4. Lint header block — deny the important, allow the narrow
`src/lib.rs:33-48`:
```rust
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]
#![doc(test(attr(allow(unused_variables), deny(warnings))))]
#![no_std]
...
#![allow(clippy::float_cmp, clippy::neg_cmp_op_on_partial_ord, clippy::nonminimal_bool)]
#![deny(clippy::undocumented_unsafe_blocks)]
```
Anti-pattern avoided: no blanket allows — the 3 allowed clippy lints are enumerated, domain-justified (float math), crate-scoped once. Doc tests deny warnings so examples rot loudly. `missing_debug_implementations` forces a conscious Debug decision on every public type (feeds pattern 10).

### 5. clippy.toml — config carries its own why
`clippy.toml:1-2`: `# Don't warn about these identifiers when using clippy::doc_markdown.` then `doc-valid-idents = ["ChaCha", "ChaCha12", "SplitMix64", "ZiB", ".."]` (`..` keeps clippy defaults).

### 6. SAFETY comment discipline, lint-enforced
`#![deny(clippy::undocumented_unsafe_blocks)]` (`src/lib.rs:48`) makes the comment mandatory; each comment states the exact invariant, not "this is safe":
- `src/distr/other.rs:121-122`: `// SAFETY: We ensure above that 'n' represents a 'char'.` → `unsafe { char::from_u32_unchecked(n) }` — the guarding logic sits directly above.
- `src/rngs/thread.rs:142-144` (and 217, 225, 233): `// SAFETY: We must make sure to stop using 'rng' before anyone else creates another mutable reference` — repeated verbatim at all four sites, no "see above".
- `src/distr/integer.rs:121-122`: `// SAFETY: All byte sequences of 'buf' represent values of the output type.`
- Single opt-out in the whole crate: `#[allow(clippy::undocumented_unsafe_blocks)]` scoped to one macro-generated fn wrapping x86 intrinsics (`src/distr/utils.rs:182`) — narrowest possible scope, never module- or crate-level.
- `src/distr/other.rs:170-177`: unsafe block paired with a runtime `debug_assert!(b.is_ascii_alphanumeric())` inside — belt-and-suspenders check of the stated invariant, plus an issue link at `src/distr/other.rs:187`.

### 7. Unsafe *macros* forced into caller-side `unsafe` blocks
`src/rng.rs:344-345`: `/// Call target for unsafe macros` / `const unsafe fn __unsafe() {}`. The `impl_fill!` macro documents a `# Safety` contract (`src/rng.rs:349-350`) and expands `__unsafe();` (`src/rng.rs:384`) so instantiation only compiles inside `unsafe`, and call sites carry SAFETY comments:
```rust
// SAFETY: All bit patterns of `[u8; size_of::<$t>()]` represent values of `u*`.
const _: () = unsafe { impl_fill!(u16, u32, u64, u128,) };
```
(`src/rng.rs:402-403`). Transferable trick: macro-with-obligation = make the obligation type-system-visible.

### 8. Rationale-essay comments
Two exemplars; what makes them good: they justify a *decision* (not restate code), cite a measurement, enumerate the failure scenarios considered, and live adjacent to the code.
- **UnsafeCell-vs-RefCell essay**, `src/rngs/thread.rs:21-33`: names the rejected safe alternative and its measured cost ("Previously we used a `RefCell`, with an overhead of ~15%"), states the aliasing invariant, then walks the only two scenarios that could break it and why each is nonsensical.
- **ALWAYS_TRUE essay**, `src/distr/bernoulli.rs:53-72`: documents the representation edge case, who cares and why, and the accepted tradeoff ("pay the performance price for all uses that *are* reasonable"), ending in a named const — the essay is the const's documentation.

### 9. Named-const-with-comment discipline
- `src/rngs/thread.rs:35-39`: comment gives benchmark evidence and unit conversion before the const: "reseeding has a noticeable impact with thresholds of 32 kB and less. We choose 64 kiB … equals 1024 blocks" → `const RESEED_BLOCK_THRESHOLD: u64 = 1024;`
- `src/distr/bernoulli.rs:74-76`: `// This is just '2.0.powi(64)', but written this way because it is not available in 'no_std' mode.` → `const SCALE: f64 = ...`
- Function-local consts for local magic numbers: `const GAP_SIZE: u32` with the surrogate-gap explanation (`src/distr/other.rs:107-111`).

### 10. inline/cold placement policy
Observed policy across src/ (138 inline attrs total):
- `#[inline(always)]` **only** on trivial forwarding in the per-call hot path: `StdRng`'s `TryRng` impl delegating to inner rng (`src/rngs/std.rs:78-91`), `ThreadRng`'s `try_next_u32/u64/fill_bytes` (`src/rngs/thread.rs:215-235`), macro-generated arithmetic helpers (`src/distr/utils.rs`).
- Plain `#[inline]` on small nontrivial hot fns and cheap constructors: `Bernoulli::new`/`from_ratio`/`sample` (`src/distr/bernoulli.rs:106,142,168`).
- `#[cold] #[inline(never)]` on the rare path, exactly once in the crate (`src/rngs/thread.rs:66-68` `try_to_reseed`): the hot `generate()` (`src/rngs/thread.rs:51-57`) does a threshold compare then calls the cold fn; the `panic!` lives in the cold fn, keeping the hot path's codegen small.

### 11. Error enum house style
Three exemplars: `BernoulliError` (`src/distr/bernoulli.rs:79-93`), `uniform::Error` (`src/distr/uniform.rs:122-139`), `weighted::Error` (`src/distr/weighted/mod.rs:85-117`). Consistent shape:
- Small **per-module** enums named for their construction site, doc header names the returning fns: `/// Error type returned from ['Uniform::new'] and 'new_inclusive'.` (`src/distr/uniform.rs:121`).
- Derives: `Clone, Copy, Debug, PartialEq, Eq` — payload-free fieldless variants keep `Copy`/`Eq` derivable.
- **Per-variant doc comment stating the precise condition**: `/// 'low > high', or equal in case of exclusive range.` (`src/distr/uniform.rs:124`).
- Manual `Display` via `f.write_str(match self { ... })` with static strings — no format machinery (`src/distr/uniform.rs:130-137`).
- `impl core::error::Error for Error {}` — `core`, not `std`, so no_std-clean (`src/distr/uniform.rs:139`, `src/distr/bernoulli.rs:93`).
- `#[non_exhaustive]` only where growth is anticipated, with the reason cited: `// Marked non_exhaustive to allow a new error code in the solution to #1476.` (`src/distr/weighted/mod.rs:86-87`).

### 12. no_std layering
`src/lib.rs:36,50-53`: `#![no_std]` root + gated std/alloc:
```rust
#[cfg(feature = "alloc")] extern crate alloc;
#[cfg(feature = "std")] extern crate std;
```
Feature ladder `std = ["alloc", ...]` (`Cargo.toml:36-39`) means std implies alloc. Alloc-needing impls gated per-item (`#[cfg(feature = "alloc")] impl SampleString ...`, `src/distr/other.rs:126,167`). Modules only compiled under a std-implying feature use `use std::` freely (`src/rngs/thread.rs:12-14`) — gate at module boundary, not per-line.

### 13. Feature-gated module tree + re-export normalization
`src/rngs/mod.rs:101-119`: `#[cfg(feature)]` on both `mod` and `pub use`; third-party types re-exported under the crate's namespace, renamed for clarity: `pub use getrandom::{Error as SysError, SysRng};` (`src/rngs/mod.rs:118-119`), `pub use chacha20::{ChaCha8Rng, ...}` (`:115-116`). Module-level doc (`src/rngs/mod.rs:9-95`) is a decision guide comparing the options, not an API dump.

### 14. SECURITY.md premises structure
`SECURITY.md:1-88`: Disclaimer (community project, no legal guarantee) → **Security premises** as an if/then contract — preconditions the user must satisfy (`SECURITY.md:12-30`: proper seeding, "state … and its seed value … are not exposed") then the properties guaranteed (`:31-41`: unpredictability, no state leakage via prior outputs) → per-component sections (`:43-63`) → scope caveat on distributions ("the usage of 'significant' here permits some bias", `:65-73`) → supported-versions policy (`:75-80`) → private reporting with a 90-day window (`:82-88`).

### 15. Non-leaking Debug + exact-string regression test
`src/rngs/thread.rs:150-155`:
```rust
/// Debug implementation does not leak internal state
impl fmt::Debug for ThreadRng {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "ThreadRng {{ .. }}")
    }
}
```
Locked by a test asserting the exact output with the *reason* in a comment (`src/rngs/thread.rs:253-258`): "it must not include private CSPRNG state or the cache stored by BlockRng!" — a future switch to `derive(Debug)` fails the test. The security doc lists non-leaking Debug as a design criterion (`src/rngs/thread.rs:94-95`). Enforced crate-wide by `#![deny(missing_debug_implementations)]` (`src/lib.rs:34`).

### 16. `#[track_caller]` + `# Panics` on panicking convenience fns
Every pub fn with a documented `# Panics` section carries `#[track_caller]` so the panic blames user code: `random_bool` (`src/lib.rs:254-260`), `random_ratio` (`:278-292`), `make_rng` (`:89-102`). Doc sections are structured: `# Example`, `# Panics`, `# Security` (`src/lib.rs:82-100`).

### 17. Value-stability / known-vector tests
`src/rngs/std.rs:111-129`: exact-output test with intent comment "expected to break any time the algorithm is changed"; external test vectors cited to their source RFC draft (`src/rngs/std.rs:132-136`). `src/distr/bernoulli.rs:239-253` `value_stability` locks 10 exact samples. Transferable: lock any wire format with exact-bytes tests.

### 18. Scoped test-side allows with reasons
`src/distr/bernoulli.rs:196-198`: `// We prefer to be explicit here.` / `#![allow(clippy::bool_assert_comparison)]` — fn-scoped, commented. `#[cfg_attr(miri, ignore)] // Miri is too slow` (`src/distr/bernoulli.rs:212`). Anti-pattern avoided: silent or file-wide allows.

## Apply to shep

## Translating to shep (shep-core / shep-daemon / shep-client / shep-cli)

### Feature flags — shep-daemon Cargo.toml sketch (pattern 1-2)
```toml
[features]
# Meta-features:
default = ["metrics"]

# Option (enabled by default): in-process metrics (per-process CPU/RSS
# sampling, restart counters). Disable for minimal-footprint daemon builds.
metrics = []

# Option: export metrics + traces via OpenTelemetry OTLP. Implies "metrics".
otel = ["metrics", "dep:opentelemetry", "dep:opentelemetry-otlp", "dep:opentelemetry_sdk"]

# Option: record VCS revision of managed apps at spawn; shown in `shep ls`.
vcs = ["dep:gix"]

# Option: accept pm2 ecosystem.config files and pm2 CLI aliases.
# Note: enabling this does not change shep's native config semantics.
node-compat = ["dep:serde_json"]

[dependencies]
shep-core = { path = "../shep-core", default-features = false }
tokio = { version = "1", default-features = false, features = ["rt-multi-thread", "process", "net", "signal"] }
gix = { version = "0.66", default-features = false, optional = true }
```
Rules for the style guide:
- Every feature line gets a `# Option:` / `# Option (enabled by default):` comment stating the consequence; deprecated features stay listed as `# Deprecated option:` until removal (rand `Cargo.toml:61-62`).
- Features are strictly additive and compose upward (`otel = ["metrics", ...]` mirrors `thread_rng = ["std", "std_rng", "sys_rng"]`, rand `Cargo.toml:51`). Never a feature that removes behavior.
- Optional deps always via `dep:`; a dep's feature enabled conditionally via weak syntax (`serde?/derive` style) when shep-core's `serde` feature shouldn't force the dep (rand `Cargo.toml:36`).
- Every dep `default-features = false` + enumerate what's needed — including tokio and dev-deps, with purpose comments on test-only deps (rand `Cargo.toml:72-73`).
- Each publishable crate: `include = [...]` list, `rust-version` pinned, `[package.metadata.docs.rs] all-features = true` with the local-repro command as a comment (rand `Cargo.toml:18-24`).

### Lint wall (pattern 4-6) — workspace root Cargo.toml
rand predates `[workspace.lints]`; shep should use it (same lints, one source of truth), each crate opting in with `lints.workspace = true`:
```toml
[workspace.lints.rust]
missing_docs = "deny"
missing_debug_implementations = "deny"
[workspace.lints.clippy]
undocumented_unsafe_blocks = "deny"
```
- shep-core, shep-client, shep-cli additionally: `#![forbid(unsafe_code)]` — they have no business with unsafe.
- Doc tests deny warnings via `#![doc(test(attr(deny(warnings))))]` in each lib.rs (rand `src/lib.rs:35`).
- Any `allow` must be (a) enumerated, never blanket, (b) narrowest scope (fn or block, like rand's single macro-scoped opt-out at `src/distr/utils.rs:182`), (c) commented with why (rand `src/distr/bernoulli.rs:196-198`).
- `clippy.toml` at workspace root: `doc-valid-idents = ["PM2", "OTel", "systemd", ".."]` with a comment, keeping `..` to preserve defaults (rand `clippy.toml:1-2`).

### Unsafe budget (patterns 6-8) — shep-daemon only
Expected unsafe surface: libc/nix calls around `fork`/`setsid`/`kill`/signal masks/`dup2` for log fds. Rules:
- All unsafe confined to one module (`shep-daemon/src/sys.rs`); the rest of the daemon calls its safe wrappers.
- Every block: `// SAFETY:` stating the exact invariant ("`fd` is owned by this struct and not yet closed", "no other thread exists between fork and exec"), repeated verbatim at each site — no "see above" (rand repeats its four ThreadRng SAFETY comments, `src/rngs/thread.rs:142,217,225,233`).
- Where an invariant is checkable, pair the unsafe block with a `debug_assert!` of the stated condition (rand `src/distr/other.rs:170-177`).
- If a macro or helper generates unsafe code with a caller-side obligation, use rand's `const unsafe fn __unsafe() {}` trick (`src/rng.rs:344-345,384,402-405`) to force callers into an `unsafe` block with their own SAFETY comment.
- Any choice of unsafe over a safe alternative (e.g. raw signalfd vs tokio::signal) requires a rationale essay: measured cost of the safe path, the invariant, the failure scenarios considered and why each can't happen (model: rand `src/rngs/thread.rs:21-33`).

### Error house style (pattern 11) — SpawnError / ConnectError / ProtocolError
- `ProtocolError` lives in shep-core next to the wire types; `SpawnError` in shep-daemon; `ConnectError` in shep-client. Per-module enums named for their construction site; doc header names the returning fns: `/// Error type returned from [`Client::connect`].`
- Derive `Debug, Clone, PartialEq, Eq` (+ `Copy` when payload-free). Per-variant doc comment stating the precise condition ("/// Daemon socket exists but nothing is listening (stale socket file).").
- Manual `Display` with `f.write_str(match self { ... })` for payload-free variants; `write!` only when a field belongs in the message. `impl core::error::Error` (core path keeps shep-core no_std-portable if that ever matters, costs nothing).
- `#[non_exhaustive]` on `ProtocolError` (wire-facing, will grow across protocol versions) with a comment citing why — mirror rand's issue-linked comment (`src/distr/weighted/mod.rs:86-87`). NOT on `SpawnError`/`ConnectError` unless growth is actually anticipated: non_exhaustive taxes downstream matching.
- Deviation from rand, stated openly: rand's errors are leaf conditions with no payload; shep's `SpawnError::Io` wraps `std::io::Error` → those variants implement `source()` and lose `Copy`/`Eq` on that enum. Keep leaf enums rand-shaped, wrapper enums thiserror-shaped or hand-rolled with `source()`.

### Redacted Debug for secrets (pattern 15)
- `AuthToken` newtype in shep-core: manual `Debug` printing `AuthToken { .. }`, doc comment `/// Debug implementation does not leak the token` (rand `src/rngs/thread.rs:150-155`).
- `ProcessSpec`'s env map: manual Debug printing keys with values elided (env commonly carries secrets: `DATABASE_URL`, `*_KEY`).
- Each redacted impl gets an exact-string test with the reason commented, so a lazy `derive(Debug)` refactor fails CI:
```rust
// Must not include the token value — Debug output lands in daemon logs.
assert_eq!(format!("{:?}", token), "AuthToken { .. }");
```
(rand `src/rngs/thread.rs:253-258`). `missing_debug_implementations = "deny"` makes every new type pick derive-or-redact explicitly.

### SECURITY.md for the daemon socket (pattern 14)
Same premises shape — an if/then contract, per component:
- Disclaimer (reasonable-effort project).
- **Security premises**: IF the runtime dir is mode 0700 and owned by the daemon user, AND the daemon runs unprivileged, AND clients connect only via that unix socket — THEN: no other local user can enumerate/control processes; auth tokens never appear in logs, Debug output, or RPC responses; managed processes' env is only readable by the socket owner.
- Per-component sections: socket + permissions; auth token lifecycle; spawned-process env handling; log files (what they may contain). State what is NOT protected (rand states "no further protections exist to in-memory state", `src/rngs/thread.rs:96-98`) — e.g. "a root user can always read daemon memory".
- Supported versions + private reporting with a response window.

### Perf-annotation policy (pattern 10) — hot: log framing; cold: error paths
- `#[inline(always)]`: only trivial delegation on the per-byte/per-frame hot path — shep-core frame header encode/decode, length-prefix parse, newtype accessors (rand `src/rngs/std.rs:78-91`).
- Plain `#[inline]`: small nontrivial hot fns and cheap constructors crossing the shep-core → daemon/client crate boundary (inter-crate inlining needs the hint absent LTO).
- `#[cold] #[inline(never)]`: rare paths reached from hot loops — protocol-error construction, client reconnect, log-rotation trigger. Copy rand's exact shape (`src/rngs/thread.rs:51-72`): hot fn does one threshold compare and calls the cold fn; panics/format machinery live in the cold fn.
- Every tuning threshold is a named const with a benchmark-backed comment including unit conversion: `// Flushing per-line costs ~X% at 10k lines/s; 64 KiB batches amortize it. 64 KiB / 4 KiB pipe reads = 16 reads.` `const LOG_FLUSH_BYTES: usize = 64 * 1024;` (rand `src/rngs/thread.rs:35-39`).

### Docs + tests discipline (patterns 13, 16, 17)
- Public fns that panic: `# Panics` section + `#[track_caller]` (client-side convenience wrappers); async fns returning Result get `# Errors` sections. Structured doc sections `# Example` / `# Panics` / `# Security` in that style (rand `src/lib.rs:82-100`).
- Re-export third-party types under shep-core's namespace with normalizing renames (`pub use nix::sys::signal::Signal;` or `Error as SysError` style, rand `src/rngs/mod.rs:118-119`); `#[cfg(feature)]` goes on both `mod` and `pub use`.
- Module-level docs in shep-daemon/rngs style: a decision guide ("which restart policy when") rather than an API listing (rand `src/rngs/mod.rs:9-95`).
- **Wire-format stability tests** in shep-core: exact-bytes round-trip vectors for every frame type, commented "expected to break on any wire change — bump PROTOCOL_VERSION when it does" (rand `src/rngs/std.rs:111-129`). This is the daemon's equivalent of value-stability: old clients talk to new daemons.
- Test-side lint allows: fn-scoped, commented (`// We prefer to be explicit here.`, rand `src/distr/bernoulli.rs:196-198`); slow tests gated with a reason (`#[cfg_attr(miri, ignore)] // Miri is too slow`, `:212`).
