# rand lens: Type-driven API design

## Patterns

## Type-driven API patterns in rand 0.10.2

### 1. Two-layer trait design: minimal dyn-safe core + blanket ext trait
`src/rng.rs:56` + `src/rng.rs:321`:
```rust
pub trait RngExt: Rng {
    // ~8 generic convenience methods, all with default bodies
}
impl<R: Rng + ?Sized> RngExt for R {}
```
Core trait `Rng` (from rand_core) is tiny (`next_u32/next_u64/fill_bytes`) and dyn-safe; ALL generic ergonomics (`random<T>`, `random_range`, `sample_iter`) live in the ext trait as **default methods only** — no required methods, blanket-impl'd for every `R: Rng + ?Sized`. The blanket impl effectively seals it (any manual impl would conflict). Docs at `src/rng.rs:27-43` explicitly teach the calling convention: `fn foo<R: Rng + ?Sized>(rng: &mut R)`, noting only the core trait is dyn-safe. Tests prove dyn usage works: `&mut dyn Rng` at `src/rng.rs:539-547`, `Box<dyn Rng>` at `src/rng.rs:549-559`.

### 2. `&mut R` + `R: Rng + ?Sized` for borrowed capability; `self` by value only for adapter construction
Every sampling function takes the RNG as `&mut R where R: Rng + ?Sized` — `src/distr/distribution.rs:40`:
```rust
fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> T;
```
Same shape at `src/seq/slice.rs:52-54`, `src/seq/iterator.rs:66-68`. By-value `self` appears exactly where an adapter must own its input — `src/distr/distribution.rs:75` (`fn sample_iter<R>(self, rng: R)`) — and the docs say how to opt out: "this method consumes its argument. Use `(&mut rng).random_iter()`" (`src/rng.rs:105-106`). This works because of the reference blanket impl (pattern 3).

### 3. Blanket impls for references make ownership optional
`src/distr/distribution.rs:113-117`:
```rust
impl<T, D: Distribution<T> + ?Sized> Distribution<T> for &D {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> T {
        (*self).sample(rng)
    }
}
```
So `rng.sample(&distr)` and `rng.sample(distr)` both compile; by-value adapter APIs accept `&mut rng` (rand_core's `impl Rng for &mut R`). One impl removes an entire class of clone-or-move friction.

### 4. Fallibility as trait layer: `TryRng<Error>` + `Error = Infallible` for the infallible tier
Infallible generators implement only the Try tier with `Infallible` — `src/rngs/small.rs:100-104`:
```rust
impl TryRng for SmallRng {
    type Error = Infallible;
    #[inline(always)]
    fn try_next_u32(&mut self) -> Result<u32, Infallible> { ... }
}
```
`Rng` is then just `TryRng<Error = Infallible>` (documented `src/rngs/mod.rs:69-70`), so panic-free generic code writes itself. Capability marker traits are empty impls: `impl TryCryptoRng for ThreadRng {}` (`src/rngs/thread.rs:241`, also `src/rngs/std.rs:104`) — a compile-time "is secure" claim, zero methods.

### 5. Newtype pins public contract while internals stay swappable
`src/rngs/small.rs:14-17,79`:
```rust
#[cfg(target_pointer_width = "64")]
type Rng = super::xoshiro256plusplus::Xoshiro256PlusPlus;
pub struct SmallRng(Rng);
```
`StdRng(Rng)` same shape (`src/rngs/std.rs:14,73`). The newtype fixes what IS the contract: `type Seed = [u8; 32]; // Fix to 256 bits. Changing this is a breaking change!` (`src/rngs/std.rs:95-96`, `small.rs:82-83`) even though the inner algorithm's native seed differs (`small.rs:87-91` truncates). Forwarding methods are `#[inline(always)]` one-liners (`std.rs:78-91`).

### 6. Interior-mutability handle with rationale essay + per-block SAFETY comments
`src/rngs/thread.rs:21-33` opens with a 13-line comment justifying `UnsafeCell` over `RefCell` (measured 15% overhead) and enumerating why aliasing can't occur. Struct at `thread.rs:131-135`:
```rust
#[derive(Clone)]
pub struct ThreadRng {
    // Rc is explicitly !Send and !Sync
    rng: Rc<UnsafeCell<BlockRng<ReseedingCore>>>,
}
```
Every `unsafe` block has a `// SAFETY:` line (`thread.rs:143-144,217-219`), enforced crate-wide by `#![deny(clippy::undocumented_unsafe_blocks)]` (`src/lib.rs:48`). `!Send/!Sync` achieved structurally via `Rc`, not via phantom markers. `Default` delegates to the canonical constructor (`thread.rs:206-210`).

### 7. Debug must not leak secrets — and it's TESTED
`src/rngs/thread.rs:150-155`:
```rust
/// Debug implementation does not leak internal state
impl fmt::Debug for ThreadRng {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "ThreadRng {{ .. }}")
    }
}
```
Paired with `#![deny(missing_debug_implementations)]` (`lib.rs:34`) forcing every public type to decide its Debug story, and a unit test asserting the exact redacted output (`thread.rs:253-258`).

### 8. Constructors return `Result` with tiny per-module error enums; panics only at the ergonomic layer with `#[track_caller]`
`src/distr/uniform.rs:122-139`: 2-variant `enum Error { EmptyRange, NonFinite }` + `Display` + `impl core::error::Error`. Constructor `Uniform::new -> Result<Uniform<X>, Error>` (`uniform.rs:235-241`). The convenience layer converts to panics with caller-attributed location — `src/rng.rs:162-170`:
```rust
#[track_caller]
fn random_range<T, R>(&mut self, range: R) -> T ... {
    assert!(!range.is_empty(), "cannot sample empty range");
    range.sample_single(self).unwrap()
}
```
`#[track_caller]` on every panicking pub fn (`lib.rs:102,259,291,313`, `rng.rs:162,192,226,315`, `seq/index.rs:239`), each with a `# Panics` doc section (`lib.rs:89-92`, `rng.rs:137-139`). Panic messages include the offending value (`rng.rs:196`).

### 9. Bidirectional associated-type registration ("type family" pattern)
`src/distr/uniform.rs:270-287`:
```rust
pub trait SampleUniform: Sized {
    type Sampler: UniformSampler<X = Self>;
}
pub trait UniformSampler: Sized {
    type X;
    ...
}
```
Type → its sampler via assoc type; sampler → its type via mirror assoc type, with the `X = Self` bound making the pair provably consistent. Public type is a newtype over the assoc type: `pub struct Uniform<X: SampleUniform>(X::Sampler)` (`uniform.rs:222`). Users extend by "registering" a backend (worked example in module docs `uniform.rs:51-88`). **Choice rule visible here:** `Distribution<T>` uses a *generic parameter* because one distribution samples many types (StandardUniform → u8, f64, tuples...); `SampleUniform` uses an *assoc type* because each type has exactly one sampler.

### 10. Range/selector polymorphism via small input trait
`src/distr/uniform.rs:439-445`:
```rust
pub trait SampleRange<T> {
    fn sample_single<R: Rng + ?Sized>(self, rng: &mut R) -> Result<T, Error>;
    fn is_empty(&self) -> bool;
}
```
Implemented for `Range`, `RangeInclusive` (`uniform.rs:447-469`) and, for unsigned ints only, `RangeTo`/`RangeToInclusive` via macro (`uniform.rs:471-504`) — so `rng.random_range(..len)` works but only where a zero lower bound is meaningful. API accepts the *user's natural syntax* (`0..10`, `'a'..='z'`) and the trait normalizes it.

### 11. `TryFrom` for fallible conversions, `From` only when infallible
`src/distr/uniform.rs:374-380` (`TryFrom<Range<X>> for Uniform<X>` with `type Error = Error`) vs `src/seq/index.rs:131-136` (`From<Vec<u32>> for IndexVec`, trivially infallible). Never a panicking `From`.

### 12. Iterator adapters are NAMED public structs built by trait default methods
`src/distr/distribution.rs:128-152`:
```rust
pub struct Iter<D, R, T> {
    distr: D,
    rng: R,
    phantom: core::marker::PhantomData<T>,
}
```
- Infinite iterator declares it honestly: `size_hint = (usize::MAX, None)` (`distribution.rs:149-151`) + `FusedIterator` (`154-159`).
- `Map` uses `PhantomData<fn(T) -> S>` (`distribution.rs:169`) — fn-pointer phantom to avoid dragging `T: Send/Sync` requirements into auto-traits.
- Finite adapter implements `ExactSizeIterator` (`seq/slice.rs:540-547`).
- `impl Trait` in return position ONLY for one-off combinator chains not worth naming: `fn choose_iter(...) -> Option<impl Iterator<Item = &Self::Output>>` (`seq/slice.rs:78-84`).
Rule visible: if users may store/return the iterator, name the struct; otherwise `impl Trait`.

### 13. Extension traits on std/foreign types with minimal required surface
`src/seq/slice.rs:25-33`: `trait IndexedRandom: Index<usize>` — one required method (`fn len`), everything else defaulted. Then `impl<T> IndexedRandom for [T]` is 3 lines (`slice.rs:462-466`). Hierarchy grows by capability: `IndexedRandom` → `IndexedMutRandom` (blanket: `impl<IR: IndexedRandom + IndexMut<usize> + ?Sized> IndexedMutRandom for IR {}`, `slice.rs:468`) → `SliceRandom` (`slice.rs:402`). Iterator flavor: `trait IteratorRandom: Iterator + Sized` (`seq/iterator.rs:34`) with docs telling the user "You must `use` this trait" + example (`iterator.rs:22-33`).

### 14. Prelude = traits + a few common types, all `#[doc(no_inline)]`; root re-exports the entry points
`src/prelude.rs:21-34` — nothing exotic, every line `#[doc(no_inline)]` so docs point at the real home. Root `lib.rs` re-exports the foundation crate whole (`pub use rand_core;` `lib.rs:56`) plus its key traits by name (`lib.rs:59`), and provides free-function shims documented as shorthand: "This function is shorthand for `rng().random()`" (`lib.rs:150-152,190-195`). Convenience never has its own semantics.

### 15. Lint wall + doc discipline (crate root, `src/lib.rs:33-48`)
```rust
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]
#![doc(test(attr(allow(unused_variables), deny(warnings))))]
#![no_std]
#![deny(clippy::undocumented_unsafe_blocks)]
```
Targeted `#![allow]`s are named and few (`float_cmp`, `neg_cmp_op_on_partial_ord`, `nonminimal_bool`, `lib.rs:43-47`). Local suppressions carry a justification comment — `seq/iterator.rs:137-144` explains WHY clippy is wrong before `#[allow(clippy::double_ended_iterator_last)]`. Doc-only imports use `#[cfg(doc)] use crate::RngExt;` (`uniform.rs:118-119`, `distribution.rs:17-18`) so intra-doc links resolve without unused-import noise.

### 16. `#[must_use]` sparingly, where discarding is a plausible bug — and explicit discard when intended
`#[must_use]` appears exactly once: `partial_shuffle` (`seq/slice.rs:451`) whose return is the point. Internal reuse discards it explicitly: `let _ = self.partial_shuffle(rng, self.len());` (`slice.rs:479`). No cargo-cult `#[must_use]` on every getter.

### 17. Representation-hiding enum with `#[doc(hidden)]` variants
`src/seq/index.rs:32-40`:
```rust
pub enum IndexVec {
    #[doc(hidden)]
    U32(Vec<u32>),
    #[cfg(target_pointer_width = "64")]
    #[doc(hidden)]
    U64(Vec<u64>),
}
```
Public type, hidden variants: matchable within the crate, opaque in docs, freedom to change representation. Rationale documented at the API that returns it (`index.rs:231-233`: "we hide the underlying type behind an abstraction"). Cross-arch portability rule stated once at module level: "all `usize` indices are sampled as a `u32` where possible" (`seq/mod.rs:25-27`), enforced by `compile_error!` for unsupported targets (`index.rs:26-27`).

### 18. Deprecation shims for renames, delegating to the new name
`src/seq/slice.rs:280-293`:
```rust
#[deprecated(since = "0.10.0", note = "Renamed to `sample`")]
fn choose_multiple<R>(&self, rng: &mut R, amount: usize) -> ... {
    self.sample(rng, amount)
}
```
Also deprecated type alias (`slice.rs:549-552`). Old names cost one delegating line, keep downstream compiling with warnings.

### 19. Additive cargo features; features imply their deps; convenience gated not core
`Cargo.toml`: `thread_rng = ["std", "std_rng", "sys_rng"]`; `sys_rng = ["dep:getrandom", ...]`. All the `rand::random*` free fns are `#[cfg(feature = "thread_rng")]` (`lib.rs:188,208,233,257,289,311`); trait definitions never are. Fallback logic in `make_rng` handles both cfg arms in one body (`lib.rs:101-113`).

### 20. Hot/cold path split with attributes
`src/rngs/thread.rs:66-72`:
```rust
#[cold]
#[inline(never)]
fn try_to_reseed(&mut self) {
    if let Err(e) = self.reseed() {
        panic!("could not reseed ThreadRng: {e}");
    }
}
```
Rare failure path extracted out-of-line; the hot `generate` stays branch-cheap (`thread.rs:51-57`). Constants that tune behavior get a WHY comment with measurements (`thread.rs:35-39`).

### 21. Test infrastructure as typed fixtures in `crate::test`
`src/lib.rs:323-359`: `pub fn rng(seed: u64) -> impl Rng` (deterministic PCG32, unique seed per test: `rng(101)`, `rng(107)`...), plus `StepRng` — a 12-line fake implementing the core trait for exact-value assertions (`lib.rs:341-359`). Value-stability tests pin outputs across releases (`uniform.rs:574-625`, `slice.rs:591-624`).

## Apply to shep

## Translation to shep

### shep-core (types, config, wire protocol)
- **Newtypes for units, rand-style**: `MemSize(u64)`, `Restarts(u32)`, wrap `std::time::Duration` only if serialization format differs. Derive `Clone, Copy, Debug, PartialEq, Eq` like `Uniform` (`uniform.rs:215`). `impl From<u64> for MemSize` (infallible, like `From<Vec<u32>> for IndexVec` index.rs:131) and `impl TryFrom<&str> for MemSize { type Error = ParseMemSizeError }` for `"512M"` parsing (like `TryFrom<Range> for Uniform` uniform.rs:374). Never a panicking `From`.
- **Per-module micro error enums**: each fallible constructor gets a 2-4 variant enum + `Display` + `impl core::error::Error` (`uniform.rs:122-139`, `bernoulli.rs:79-93`). No crate-wide mega-enum in shep-core; compose at daemon/client level.
- **ProcessSelector as input trait** (SampleRange shape, `uniform.rs:439-445`): `trait IntoSelector { fn into_selector(self) -> ProcessSelector; }` implemented for `&str` (name), `u32`/`ProcessId` (id), `All` marker — CLI and client methods take `impl IntoSelector` so `client.restart("web")`, `client.restart(ProcessId(3))`, `client.restart(All)` all read naturally. Implement only where meaningful (rand gives `..n` ranges to unsigned ints only, `uniform.rs:471-504`).
- **Typed RPC via bidirectional assoc types** (SampleUniform/UniformSampler shape, `uniform.rs:270-287`):
  ```rust
  pub trait Request: Serialize + DeserializeOwned {
      const METHOD: &'static str;          // wire tag
      type Response: Serialize + DeserializeOwned;
  }
  ```
  Each RPC (StartReq, StopReq, ListReq) registers its response type once in shep-core; daemon and client both dispatch off the same pair, so a mismatched response type is a compile error, not a runtime decode error. Mirror-bound the reverse direction only if responses need to know their request.
- **Wire enums: pin the contract in a comment + value-stability test.** Like `type Seed = [u8; 32]; // Changing this is a breaking change!` (`std.rs:95-96`): every `#[repr]`/serde wire type gets a `// wire format: changing this is a breaking change` comment and a golden-bytes round-trip test (rand's value-stability tests, `uniform.rs:574-625`).
- **Representation-hiding**: if a wire type needs an internal enum users shouldn't match (e.g. compact vs extended process-list encoding), use `#[doc(hidden)]` variants (`index.rs:32-40`) or a private-field struct. Never `usize` on the wire (portability rule, `seq/mod.rs:25-27`).
- **Config structs**: `AppConfig::builder()` fits the user's builder-pattern rule; validation returns `Result<AppConfig, ConfigError>` at build, mirroring `Uniform::new -> Result` (`uniform.rs:235`). No panicking constructors in shep-core at all — panicking conveniences belong to shep-cli only.

### shep-daemon (tokio supervisor lib)
- **Two-layer actor handle** (ThreadRng shape): the handle is `#[derive(Clone)] pub struct SupervisorHandle { tx: mpsc::Sender<Command> }` — a cheap clone of shared state exactly like `ThreadRng { rng: Rc<...> }` (`thread.rs:131-135`). Async replaces `UnsafeCell` with channels, but keep the discipline: a rationale comment block on the handle explaining the concurrency model (`thread.rs:21-33` essay), and `Default`/`new` delegating to one canonical constructor (`thread.rs:206-210`).
- **Redacting Debug, tested**: any type holding env vars, tokens, or socket paths gets a manual `Debug` printing `"AppEnv { .. }"` + a unit test asserting the exact output (`thread.rs:150-155,253-258`). Enable `#![deny(missing_debug_implementations)]` so every public type makes this decision explicitly.
- **Hot/cold split**: supervisor event loop keeps rare paths (respawn-after-crash, reseed-style recovery) in `#[cold] #[inline(never)]` fns (`thread.rs:66-72`). Tuning constants (restart backoff, buffer sizes) get a WHY comment with the measurement or reasoning (`thread.rs:35-39`).
- **Marker traits for capabilities**: empty marker impls declare properties the type system should track — e.g. `trait Reloadable {}` on process kinds that support graceful reload, mirroring `impl TryCryptoRng for StdRng {}` (`std.rs:104`). Gate `reload()` on the marker bound instead of a runtime "unsupported" error where statically knowable.
- **`unsafe` policy**: workspace `#![deny(clippy::undocumented_unsafe_blocks)]` (`lib.rs:48`); shep should need near-zero unsafe — when libc process calls require it, every block gets `// SAFETY:` naming the invariant (`thread.rs:143-144`).

### shep-client (async RPC lib)
- **Ext-trait split**: minimal dyn-safe transport trait + blanket ext (RngExt shape, `rng.rs:56,321`):
  ```rust
  pub trait Transport {  // dyn-safe: async fn via boxed future or async-trait
      async fn call_raw(&mut self, method: &str, body: Bytes) -> Result<Bytes, RpcError>;
  }
  pub trait TransportExt: Transport {
      async fn call<Q: Request>(&mut self, req: Q) -> Result<Q::Response, RpcError> { /* default body */ }
  }
  impl<T: Transport + ?Sized> TransportExt for T {}
  ```
  All typed convenience (`start`, `stop`, `list`) = default methods on the ext trait; generic code writes `fn f<T: Transport + ?Sized>(t: &mut T)`; `Box<dyn Transport>` works for tests. Document the calling convention on the trait itself like `rng.rs:27-43`.
- **Fallibility tiers**: transport error is an assoc type; an in-memory/loopback test transport sets `type Error = Infallible` (`small.rs:100-101`) so unit tests of client logic never handle IO errors. Real Unix-socket transport uses a small `RpcError` enum.
- **Reference blanket impls**: `impl<T: Transport + ?Sized> Transport for &mut T` (mirror of `Distribution for &D`, `distribution.rs:113-117`) so helpers can take transports by value or borrow without ceremony.
- **Event streams**: named public adapter structs, not `impl Stream`, when users will store them: `pub struct EventStream { ... }` implementing `futures::Stream<Item = Event>` — the async analog of `Iter<D, R, T>` (`distribution.rs:128-152`). Honest `size_hint`; implement `FusedStream` where true (`distribution.rs:154-159`). Use `PhantomData<fn() -> T>` if a typed event stream needs a phantom (variance/auto-trait trick, `distribution.rs:169`). Reserve `-> impl Stream` for one-off filtered/combined views (choose_iter precedent, `slice.rs:78-84`).
- **Extension trait on results**: `trait ProcessListExt` on `[ProcessInfo]` with one required method and defaulted helpers (`find_by_name`, `running`), impl'd for `[ProcessInfo]` in 3 lines — the IndexedRandom shape (`slice.rs:25-33,462-466`).

### shep-cli (clap multi-call binary)
- **Panic/exit boundary lives here only**: CLI converts `Result` to user-facing errors; any helper that panics on bad input carries `#[track_caller]` + `# Panics` docs (`rng.rs:162-170`), and panic messages include the offending value (`rng.rs:196` prints `p={:?}`).
- **Free-function shims**: top-level conveniences (`shep_client::connect_default()`) documented literally as "shorthand for X" with a link, adding zero semantics (`lib.rs:150-152`).
- **Deprecation over breakage**: renamed subcommands/flags keep a delegating deprecated alias for one release (`slice.rs:280-293`).

### Workspace-wide style rules (docs/idiomatic-rust.md candidates)
1. Lint wall in workspace `[lints]`: `missing_docs`, `missing_debug_implementations`, `clippy::undocumented_unsafe_blocks` = deny; every local `#[allow]` gets a why-comment (`iterator.rs:137-144`).
2. Doc tests deny warnings: `#![doc(test(attr(deny(warnings))))]` (`lib.rs:35`).
3. `#[cfg(doc)] use` for doc-link-only imports (`uniform.rs:118-119`).
4. `#[must_use]` only where discarding is a plausible bug; intentional discards written `let _ =` (`slice.rs:451,479`).
5. Assoc type when exactly one impl per type (Request→Response); generic param when many (handler traits over message types) — the `Distribution<T>` vs `SampleUniform::Sampler` split.
6. Features are additive, imply their deps (`thread_rng = ["std", "std_rng", "sys_rng"]`), and gate convenience layers, never core trait definitions.
7. Prelude: traits + top 3 types, all `#[doc(no_inline)]` (`prelude.rs:21-34`); shep-core gets one, re-exported by client/daemon.
8. Test fixtures: `crate::test` module with deterministic constructors (`fn fixture(seed: u64) -> impl Trait`) and a hand-rolled fake implementing the core trait (`StepRng`, `lib.rs:341-359`); golden-value tests for every wire format.
9. Forwarding newtype methods are `#[inline]`/`#[inline(always)]` one-liners (`std.rs:78-91`).

### Anti-patterns rand visibly avoids
- Undocumented `unsafe` (denied at compile time, `lib.rs:48`).
- Public types without `Debug` (denied) — and naive derived `Debug` on state-carrying types (ThreadRng hand-writes it).
- Panicking constructors in the core layer (all `new` return `Result`; panics only in `#[track_caller]` ergonomic wrappers).
- Anonymous `impl Trait` for storable iterators (named `Iter`/`Map`/`IndexedSamples` structs instead).
- Silent renames (deprecated shims with `since`/`note`).
- `usize` in portable/wire-affecting positions (`seq/mod.rs:25-27`).
- Blanket `#[must_use]` noise.
- Ext-trait methods requiring implementation (all defaulted → blanket impl can never be wrong).
