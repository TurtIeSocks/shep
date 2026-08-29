# shep Phase 1 — Foundation (shep-core + CI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build shep-core — typed config (AppConfig + Flockfile + daemon config), value newtypes, selectors, paths, and the versioned wire protocol with stability fixtures — plus the CI gate, so Phases 2+ build on tested foundations.

**Architecture:** Everything in `crates/shep-core`, pure library code, no daemon behavior yet. Per-module error enums (rand idiom), serde throughout, wire protocol as typed enums + length-delimited JSON framing. CI lands first so every later commit is gated.

**Tech Stack:** Rust edition 2024 (MSRV 1.85), serde/serde_json/toml/serde_yml/json5, tokio + tokio-util (wire codec only), regex, globset, insta (snapshots), proptest (targeted).

**Phase roadmap context:** This was planned as 1 of 6 (foundation → daemon core → client+CLI → lifecycle extras → dogs+observability → UX surface); reload has since taken the sixth slot and moved the UX surface behind it. Later phases get their own plans once this lands. Deliberately deferred out of Phase 1: `config/kv.rs` (lands with Phase 4's `set`/`get` verbs), stale-socket recovery + reconnect backoff (transport runtime, Phase 2/3), croner-dialect cron validation (Phase 2, where crons execute), `channel.*` bus events (Phase 4, trigger/actions), topic glob matching + the `globset` dep (Phase 2, server-side subscription filtering), CI hardening jobs — minimal-versions, musl-run-tests, feature-combo ladder, llvm-cov (Phase 2/3 CI task), schemars JSON-schema export for AppConfig (the UX-surface phase, docs/assets).

## Global Constraints

Every task implicitly includes these; verbatim from the governing docs:

- **Invoke the `shep-idiomatic-rust` skill before writing any Rust** (CLAUDE.md hard trigger). Cite IR rules in review notes.
- **Clean-room:** never open `~/GitHub/pm2` — build from `docs/specs/shep-v1.md` + `docs/systematic-refactor/refactor-workspace/map.md` only. Spec wins over map on conflict.
- **No panicking constructors in shep-core** — all fallible constructors return `Result` (IR-21).
- `impl core::error::Error`, never `std::error::Error` (IR-19). Error enums: per-module, variant docs state the precise condition, manual `Display` via `f.write_str` for fieldless variants (IR-18/19).
- Every `Result`-returning pub fn documents `# Errors`; `# Panics` ⇔ `#[track_caller]` (IR-28/21).
- Deps: `default-features = false` + enumerate features; workspace-level versions in `[workspace.dependencies]` (IR-2).
- Wire-facing types carry `// wire format: changing this is a breaking change` + stability fixtures (IR-11/35).
- Doctests: edge cases as asserted examples on pure logic; `no_run` for anything IO-touching (IR-30).
- The four gates green before every commit: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, `cargo test --workspace`.
- Terminology per `docs/terminology.md`: sheep (singular), flock (plural), dogs (plugins), lambs (tree children), fold (group). Error text stays plain English.
- Commit style: conventional commits, footer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File Structure (locked by this plan)

```
crates/shep-core/src/
  lib.rs               module tree + crate docs + doctest-deny gate
  paths.rs             ShepPaths: SHEP_HOME resolution + on-disk layout
  values.rs            MemSize, StopTimeout-style duration newtype (UpDuration)
  status.rs            ProcStatus enum (wire strings)
  config/
    mod.rs             re-exports
    app.rs             AppConfig (Flockfile per-app schema) + defaults
    normalize.rs       AppConfig -> ResolvedApp validation/normalization
    flockfile.rs       Flockfile discovery + multi-format parse
    daemon.rs          DaemonConfig (shep.toml) + file<env<flags layering
  selector.rs          ProcessSelector parse + match
  protocol/
    mod.rs             PROTOCOL_VERSION const + re-exports
    request.rs         Request/Response/RpcError enums + Envelope/Reply
    events.rs          BusEvent + subscription topic globs
    wire.rs            length-delimited JSON codec helpers
crates/shep-core/tests/  (none this phase — co-located tests only, IR-38)
.github/workflows/test.yml
.config/nextest.toml    (only if a test needs a profile; otherwise skip — YAGNI)
```

---

### Task 1: CI gate + workspace dependencies

**Files:**
- Create: `.github/workflows/test.yml`
- Modify: `Cargo.toml` (workspace root — add `[workspace.dependencies]` versions)
- Modify: `crates/shep-core/Cargo.toml` (add deps)

**Interfaces:**
- Produces: CI running the four gates on {ubuntu, macos, windows} × {stable, 1.85}; workspace dep versions every later task consumes: `serde = { version = "1", default-features = false, features = ["derive"] }`, `serde_json = { version = "1", default-features = false, features = ["std"] }`, `toml = { version = "0.8", default-features = false, features = ["parse", "display"] }`, `serde_yml = { version = "0.0.12", default-features = false }`, `json5 = { version = "0.4", default-features = false }`, `regex = { version = "1", default-features = false, features = ["std", "unicode-perl"] }`, `globset = { version = "0.4", default-features = false }`, `tokio = { version = "1", default-features = false }`, `tokio-util = { version = "0.7", default-features = false, features = ["codec"] }`, `insta = { version = "1", default-features = false, features = ["json"] }`

- [ ] **Step 1: Add `[workspace.dependencies]` to root Cargo.toml**

Append to the existing `[workspace.dependencies]` block (which already holds the path deps):

```toml
serde = { version = "1", default-features = false, features = ["derive"] }
serde_json = { version = "1", default-features = false, features = ["std"] }
toml = { version = "0.8", default-features = false, features = ["parse", "display"] }
serde_yml = { version = "0.0.12", default-features = false }
json5 = { version = "0.4", default-features = false }
regex = { version = "1", default-features = false, features = ["std", "unicode-perl"] }
tokio = { version = "1", default-features = false }
tokio-util = { version = "0.7", default-features = false, features = ["codec"] }
insta = { version = "1", default-features = false, features = ["json"] }
```

- [ ] **Step 2: Add shep-core deps**

In `crates/shep-core/Cargo.toml` replace the empty `[dependencies]`:

```toml
[dependencies]
serde.workspace = true
serde_json.workspace = true
toml.workspace = true
serde_yml.workspace = true
json5.workspace = true
regex.workspace = true
tokio-util.workspace = true

[dev-dependencies]
insta.workspace = true
tokio = { workspace = true, features = ["macros", "rt"] }
```

- [ ] **Step 3: Create CI workflow**

`.github/workflows/test.yml`:

```yaml
name: test
on:
  push: { branches: [main] }
  pull_request:
  schedule: [{ cron: "0 0 * * SUN" }]
permissions:
  contents: read
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install 1.93 --profile minimal --component clippy,rustfmt
      - run: cargo +1.93 fmt --all -- --check
      - run: cargo +1.93 clippy --workspace --all-targets -- -D warnings
  docs:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install nightly --profile minimal
      - run: cargo +nightly doc --workspace --all-features --no-deps
        env: { RUSTDOCFLAGS: "-Dwarnings --cfg docsrs" }
  typos:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: crate-ci/typos@v1
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
        toolchain: [stable, "1.85"]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install ${{ matrix.toolchain }} --profile minimal
      - run: cargo +${{ matrix.toolchain }} test --workspace
```

(Remaining spec §12 ladder — minimal-versions, musl running tests, feature
combos, llvm-cov — assigned to the Phase 2/3 CI-hardening task per the
roadmap note.)

- [ ] **Step 4: Run the four gates locally**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo check --workspace && cargo test --workspace`
Expected: all pass (workspace still stubs; deps resolve).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/shep-core/Cargo.toml .github/
git commit -m "ci: add four-gate workflow + workspace dependency versions

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: MemSize newtype

**Files:**
- Create: `crates/shep-core/src/values.rs`
- Modify: `crates/shep-core/src/lib.rs` (add `pub mod values;`)

**Interfaces:**
- Produces: `pub struct MemSize(u64)` with `MemSize::from_bytes(u64) -> Self` (const), `.bytes(self) -> u64`, `FromStr<Err = ParseMemSizeError>` (grammar `^\d+(G|M|K)?$`, binary units), `Display` (largest exact unit), `Serialize`/`Deserialize` (as the string form), `pub enum ParseMemSizeError { Empty, MissingDigits, InvalidCharacter, Overflow }`.

- [ ] **Step 1: Write failing tests** — add `values.rs` containing ONLY the test mod first:

```rust
//! Config value newtypes: memory sizes and durations

#[cfg(test)]
mod mem_size_tests {
    use super::*;

    #[test]
    fn plain_digits_parse_as_bytes() {
        assert_eq!("123".parse::<MemSize>().unwrap().bytes(), 123);
    }

    #[test]
    fn units_are_binary() {
        assert_eq!("7K".parse::<MemSize>().unwrap().bytes(), 7 * 1024);
        assert_eq!("512M".parse::<MemSize>().unwrap().bytes(), 512 << 20);
        assert_eq!("3G".parse::<MemSize>().unwrap().bytes(), 3 << 30);
    }

    #[test]
    fn rejects_spec_violations() {
        use ParseMemSizeError::*;
        assert_eq!("".parse::<MemSize>(), Err(Empty));
        assert_eq!("G".parse::<MemSize>(), Err(MissingDigits));
        assert_eq!("512m".parse::<MemSize>(), Err(InvalidCharacter)); // lowercase
        assert_eq!(" 512M".parse::<MemSize>(), Err(InvalidCharacter)); // whitespace
        assert_eq!("1.5G".parse::<MemSize>(), Err(InvalidCharacter)); // fraction
        assert_eq!("512MB".parse::<MemSize>(), Err(InvalidCharacter)); // multi-letter
        assert_eq!("18446744073709551616".parse::<MemSize>(), Err(Overflow));
        assert_eq!("17179869184G".parse::<MemSize>(), Err(Overflow));
    }

    #[test]
    fn display_uses_largest_exact_unit_and_round_trips() {
        for bytes in [0u64, 1, 1023, 1024, 1536, 1 << 20, (1 << 30) + 1024, u64::MAX] {
            let size = MemSize::from_bytes(bytes);
            let reparsed: MemSize = size.to_string().parse().unwrap();
            assert_eq!(reparsed, size, "display of {bytes} bytes must reparse");
        }
        assert_eq!(MemSize::from_bytes(3 << 30).to_string(), "3G");
        assert_eq!(MemSize::from_bytes(1536).to_string(), "1536");
    }

    #[test]
    fn serde_uses_string_form() {
        let size: MemSize = serde_json::from_str("\"512M\"").unwrap();
        assert_eq!(size.bytes(), 512 << 20);
        assert_eq!(serde_json::to_string(&size).unwrap(), "\"512M\"");
        assert!(serde_json::from_str::<MemSize>("\"512MB\"").is_err());
    }
}
```

Add `pub mod values;` to `lib.rs`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core mem_size -- --nocapture`
Expected: COMPILE ERROR — `MemSize` not defined.

- [ ] **Step 3: Implement** — above the test mod in `values.rs`:

```rust
use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// Binary units per the Flockfile grammar `^\d+(G|M|K)?$`: K/M/G are
// KiB/MiB/GiB, not decimal. Unit definitions, not tuning thresholds.
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// A memory quantity in bytes, used for memory-limit thresholds
///
/// Parses the Flockfile grammar `^\d+(G|M|K)?$` (binary units; plain digits
/// are bytes). Ordering compares byte counts, so a configured limit compares
/// directly against a sampled RSS wrapped with [`MemSize::from_bytes`].
///
/// # Example
/// ```
/// use shep_core::values::MemSize;
///
/// let limit: MemSize = "512M".parse()?;
/// assert_eq!(limit.bytes(), 512 << 20);
/// assert!("512MB".parse::<MemSize>().is_err()); // strict grammar
/// # Ok::<(), shep_core::values::ParseMemSizeError>(())
/// ```
// wire format: changing this is a breaking change (serialized as its string
// form inside AppConfig, which travels over the client<->daemon socket)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemSize(u64);

impl MemSize {
    /// Wraps a raw byte count, e.g. an RSS sample
    #[inline]
    #[must_use]
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    /// Returns the quantity in bytes
    #[inline]
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl FromStr for MemSize {
    type Err = ParseMemSizeError;

    /// Parses `^\d+(G|M|K)?$` — binary units, plain digits = bytes
    ///
    /// # Errors
    ///
    /// - [`ParseMemSizeError::Empty`] — empty input.
    /// - [`ParseMemSizeError::MissingDigits`] — unit suffix with no digits.
    /// - [`ParseMemSizeError::InvalidCharacter`] — anything outside ASCII
    ///   digits plus one trailing `G`/`M`/`K` (lowercase, whitespace,
    ///   fractions, multi-letter suffixes all land here).
    /// - [`ParseMemSizeError::Overflow`] — byte count exceeds `u64::MAX`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParseMemSizeError::Empty);
        }
        let (digits, multiplier) = match s.as_bytes()[s.len() - 1] {
            b'G' => (&s[..s.len() - 1], GIB),
            b'M' => (&s[..s.len() - 1], MIB),
            b'K' => (&s[..s.len() - 1], KIB),
            _ => (s, 1),
        };
        if digits.is_empty() {
            return Err(ParseMemSizeError::MissingDigits);
        }
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseMemSizeError::InvalidCharacter);
        }
        let value: u64 = digits
            .parse()
            .map_err(|_| ParseMemSizeError::Overflow)?;
        value
            .checked_mul(multiplier)
            .map(Self)
            .ok_or(ParseMemSizeError::Overflow)
    }
}

/// Formats with the largest binary unit dividing the value exactly;
/// output always re-parses to the same value
impl fmt::Display for MemSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 => f.write_str("0"),
            b if b % GIB == 0 => write!(f, "{}G", b / GIB),
            b if b % MIB == 0 => write!(f, "{}M", b / MIB),
            b if b % KIB == 0 => write!(f, "{}K", b / KIB),
            b => write!(f, "{b}"),
        }
    }
}

impl Serialize for MemSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MemSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // String, not &str: the toml deserializer cannot always borrow
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Failure to parse a [`MemSize`] from the grammar `^\d+(G|M|K)?$`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMemSizeError {
    /// The input string was empty
    Empty,
    /// A unit suffix with no digits before it (`"M"`)
    MissingDigits,
    /// A character outside ASCII digits plus one optional trailing
    /// `G`/`M`/`K` — covers lowercase units, whitespace, signs, fractions,
    /// and multi-letter suffixes such as `"MB"`
    InvalidCharacter,
    /// The quantity in bytes does not fit in `u64`
    Overflow,
}

impl fmt::Display for ParseMemSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "memory size is empty",
            Self::MissingDigits => "memory size has a unit suffix but no digits",
            Self::InvalidCharacter => {
                "memory size must be ASCII digits with an optional trailing G, M, or K"
            }
            Self::Overflow => "memory size in bytes overflows u64",
        })
    }
}

impl core::error::Error for ParseMemSizeError {}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core mem_size && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: all tests PASS, clippy/fmt clean.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/values.rs crates/shep-core/src/lib.rs
git commit -m "feat(core): MemSize newtype with strict ^\\d+(G|M|K)?\$ grammar

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: UpDuration newtype (stime grammar)

**Files:**
- Modify: `crates/shep-core/src/values.rs` (append)

**Interfaces:**
- Produces: `pub struct UpDuration(std::time::Duration)` — grammar `^\d+(h|m|s)?$` (plain digits = milliseconds), `UpDuration::from_millis(u64) -> Self` (const), `.as_duration(self) -> std::time::Duration`, `.as_millis(self) -> u64`, `FromStr<Err = ParseUpDurationError>`, `Display` (largest exact unit, ms as plain digits), serde as string form, `pub enum ParseUpDurationError { Empty, MissingDigits, InvalidCharacter, Overflow }`.

- [ ] **Step 1: Write failing tests** — append to `values.rs`:

```rust
#[cfg(test)]
mod up_duration_tests {
    use super::*;

    #[test]
    fn plain_digits_are_milliseconds() {
        assert_eq!("1600".parse::<UpDuration>().unwrap().as_millis(), 1600);
    }

    #[test]
    fn units_seconds_minutes_hours() {
        assert_eq!("30s".parse::<UpDuration>().unwrap().as_millis(), 30_000);
        assert_eq!("5m".parse::<UpDuration>().unwrap().as_millis(), 300_000);
        assert_eq!("2h".parse::<UpDuration>().unwrap().as_millis(), 7_200_000);
    }

    #[test]
    fn rejects_spec_violations() {
        use ParseUpDurationError::*;
        assert_eq!("".parse::<UpDuration>(), Err(Empty));
        assert_eq!("s".parse::<UpDuration>(), Err(MissingDigits));
        assert_eq!("30S".parse::<UpDuration>(), Err(InvalidCharacter)); // uppercase
        assert_eq!("1.5s".parse::<UpDuration>(), Err(InvalidCharacter));
        assert_eq!("30 s".parse::<UpDuration>(), Err(InvalidCharacter));
        assert_eq!("99999999999999999999h".parse::<UpDuration>(), Err(Overflow));
    }

    #[test]
    fn display_round_trips() {
        for ms in [0u64, 1, 999, 1000, 1600, 30_000, 300_000, 7_200_000, 3_601_000] {
            let d = UpDuration::from_millis(ms);
            assert_eq!(d.to_string().parse::<UpDuration>().unwrap(), d, "{ms}ms");
        }
        assert_eq!(UpDuration::from_millis(30_000).to_string(), "30s");
        assert_eq!(UpDuration::from_millis(1600).to_string(), "1600");
        assert_eq!(UpDuration::from_millis(7_200_000).to_string(), "2h");
    }

    #[test]
    fn serde_uses_string_form() {
        let d: UpDuration = serde_json::from_str("\"30s\"").unwrap();
        assert_eq!(d.as_millis(), 30_000);
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"30s\"");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core up_duration`
Expected: COMPILE ERROR — `UpDuration` not defined.

- [ ] **Step 3: Implement** — append to `values.rs` (above tests):

```rust
/// A duration from the Flockfile grammar `^\d+(h|m|s)?$`
///
/// Plain digits are milliseconds; `s`/`m`/`h` are seconds/minutes/hours.
/// Used for `min_uptime`, `kill_timeout`, and the other lifecycle timers.
///
/// # Example
/// ```
/// use shep_core::values::UpDuration;
///
/// assert_eq!("30s".parse::<UpDuration>()?.as_millis(), 30_000);
/// assert!("30S".parse::<UpDuration>().is_err()); // lowercase units only
/// # Ok::<(), shep_core::values::ParseUpDurationError>(())
/// ```
// wire format: changing this is a breaking change (string form in AppConfig)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UpDuration(core::time::Duration);

impl UpDuration {
    /// Wraps a raw millisecond count
    #[inline]
    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        Self(core::time::Duration::from_millis(ms))
    }

    /// Returns the wrapped [`core::time::Duration`]
    #[inline]
    #[must_use]
    pub const fn as_duration(self) -> core::time::Duration {
        self.0
    }

    /// Returns the duration in whole milliseconds
    #[inline]
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0.as_millis() as u64
    }
}

impl FromStr for UpDuration {
    type Err = ParseUpDurationError;

    /// Parses `^\d+(h|m|s)?$` — plain digits are milliseconds
    ///
    /// # Errors
    ///
    /// - [`ParseUpDurationError::Empty`] — empty input.
    /// - [`ParseUpDurationError::MissingDigits`] — unit with no digits.
    /// - [`ParseUpDurationError::InvalidCharacter`] — anything outside ASCII
    ///   digits plus one trailing lowercase `h`/`m`/`s`.
    /// - [`ParseUpDurationError::Overflow`] — milliseconds overflow `u64`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParseUpDurationError::Empty);
        }
        let (digits, ms_per_unit) = match s.as_bytes()[s.len() - 1] {
            b'h' => (&s[..s.len() - 1], 3_600_000),
            b'm' => (&s[..s.len() - 1], 60_000),
            b's' => (&s[..s.len() - 1], 1_000),
            _ => (s, 1),
        };
        if digits.is_empty() {
            return Err(ParseUpDurationError::MissingDigits);
        }
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseUpDurationError::InvalidCharacter);
        }
        let value: u64 = digits
            .parse()
            .map_err(|_| ParseUpDurationError::Overflow)?;
        value
            .checked_mul(ms_per_unit)
            .map(Self::from_millis)
            .ok_or(ParseUpDurationError::Overflow)
    }
}

/// Formats with the largest unit dividing the value exactly (ms as digits)
impl fmt::Display for UpDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.as_millis();
        match ms {
            0 => f.write_str("0"),
            v if v % 3_600_000 == 0 => write!(f, "{}h", v / 3_600_000),
            v if v % 60_000 == 0 => write!(f, "{}m", v / 60_000),
            v if v % 1_000 == 0 => write!(f, "{}s", v / 1_000),
            v => write!(f, "{v}"),
        }
    }
}

impl Serialize for UpDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for UpDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // String, not &str: the toml deserializer cannot always borrow
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Failure to parse an [`UpDuration`] from the grammar `^\d+(h|m|s)?$`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseUpDurationError {
    /// The input string was empty
    Empty,
    /// A unit suffix with no digits before it (`"s"`)
    MissingDigits,
    /// A character outside ASCII digits plus one optional trailing
    /// lowercase `h`/`m`/`s`
    InvalidCharacter,
    /// The duration in milliseconds does not fit in `u64`
    Overflow,
}

impl fmt::Display for ParseUpDurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "duration is empty",
            Self::MissingDigits => "duration has a unit suffix but no digits",
            Self::InvalidCharacter => {
                "duration must be ASCII digits with an optional trailing h, m, or s"
            }
            Self::Overflow => "duration in milliseconds overflows u64",
        })
    }
}

impl core::error::Error for ParseUpDurationError {}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core up_duration && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS + clean.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/values.rs
git commit -m "feat(core): UpDuration newtype with ^\\d+(h|m|s)?\$ grammar

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: ProcStatus enum

**Files:**
- Create: `crates/shep-core/src/status.rs`
- Modify: `crates/shep-core/src/lib.rs` (add `pub mod status;`)

**Interfaces:**
- Produces: `pub enum ProcStatus { Starting, Online, Stopping, Stopped, Errored, WaitingRestart }` — serde strings exactly `"starting"`, `"online"`, `"stopping"`, `"stopped"`, `"errored"`, `"waiting-restart"`; `Display` = same strings; derives `Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize`.

- [ ] **Step 1: Write failing test** — `status.rs` with test mod only:

```rust
//! Process lifecycle status

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_strings_are_stable() {
        // wire format: these six strings are the protocol contract (spec §4)
        let cases = [
            (ProcStatus::Starting, "\"starting\""),
            (ProcStatus::Online, "\"online\""),
            (ProcStatus::Stopping, "\"stopping\""),
            (ProcStatus::Stopped, "\"stopped\""),
            (ProcStatus::Errored, "\"errored\""),
            (ProcStatus::WaitingRestart, "\"waiting-restart\""),
        ];
        for (status, json) in cases {
            assert_eq!(serde_json::to_string(&status).unwrap(), json);
            assert_eq!(serde_json::from_str::<ProcStatus>(json).unwrap(), status);
            assert_eq!(format!("\"{status}\""), json);
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core status`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement** — above tests:

```rust
use core::fmt;

use serde::{Deserialize, Serialize};

/// Lifecycle state of a sheep (one managed process)
///
/// The serialized strings are the wire contract; `waiting-restart` means a
/// backoff or restart delay is pending.
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcStatus {
    /// Spawned, not yet ready
    Starting,
    /// Running and (if configured) ready
    Online,
    /// Stop ladder in progress
    Stopping,
    /// Cleanly stopped; not scheduled to run
    Stopped,
    /// Restart budget exhausted or spawn failed
    Errored,
    /// Restart pending after a backoff or configured delay
    WaitingRestart,
}

impl fmt::Display for ProcStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Online => "online",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Errored => "errored",
            Self::WaitingRestart => "waiting-restart",
        })
    }
}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core status && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/status.rs crates/shep-core/src/lib.rs
git commit -m "feat(core): ProcStatus enum with stable wire strings

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: ShepPaths

**Files:**
- Create: `crates/shep-core/src/paths.rs`
- Modify: `crates/shep-core/src/lib.rs` (add `pub mod paths;`)

**Interfaces:**
- Produces: `pub struct ShepPaths { pub home: PathBuf, pub daemon_config: PathBuf, pub snapshot: PathBuf, pub logs: PathBuf, pub pids: PathBuf, pub run: PathBuf, pub socket: PathBuf, pub barks: PathBuf }` and `ShepPaths::resolve(env: &dyn Fn(&str) -> Option<String>, home_dir: &Path) -> ShepPaths`. Layout per spec §3: `$SHEP_HOME` default `<home_dir>/.shep`; children `shep.toml`, `flock.json`, `logs/`, `pids/`, `run/`, `run/shep.sock`, `barks.jsonl`. Env override: `SHEP_HOME`.

- [ ] **Step 1: Write failing tests** — `paths.rs` test mod:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn default_layout_under_home_dir() {
        let p = ShepPaths::resolve(&no_env, Path::new("/home/ada"));
        assert_eq!(p.home, Path::new("/home/ada/.shep"));
        assert_eq!(p.daemon_config, Path::new("/home/ada/.shep/shep.toml"));
        assert_eq!(p.snapshot, Path::new("/home/ada/.shep/flock.json"));
        assert_eq!(p.logs, Path::new("/home/ada/.shep/logs"));
        assert_eq!(p.pids, Path::new("/home/ada/.shep/pids"));
        assert_eq!(p.run, Path::new("/home/ada/.shep/run"));
        assert_eq!(p.socket, Path::new("/home/ada/.shep/run/shep.sock"));
        assert_eq!(p.barks, Path::new("/home/ada/.shep/barks.jsonl"));
    }

    #[test]
    fn shep_home_env_overrides_root() {
        let env = |key: &str| (key == "SHEP_HOME").then(|| "/srv/shep".to_string());
        let p = ShepPaths::resolve(&env, Path::new("/home/ada"));
        assert_eq!(p.home, Path::new("/srv/shep"));
        assert_eq!(p.socket, Path::new("/srv/shep/run/shep.sock"));
    }

    #[test]
    fn pipe_name_is_per_home_and_sanitized() {
        // Windows transport identity (spec §6): derived from SHEP_HOME so
        // two homes never share a pipe; non-alphanumerics collapse to '-'.
        let p = ShepPaths::resolve(&no_env, Path::new("/home/ada"));
        assert_eq!(p.pipe_name(), r"\\.\pipe\shep-home-ada--shep");
        let env = |key: &str| (key == "SHEP_HOME").then(|| "/srv/shep".to_string());
        let q = ShepPaths::resolve(&env, Path::new("/home/ada"));
        assert_eq!(q.pipe_name(), r"\\.\pipe\shep-srv-shep");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core paths`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement**:

```rust
//! On-disk layout of `$SHEP_HOME`
//!
//! One resolver, no hidden `std::env` reads — the environment comes in as a
//! closure so tests and the daemon share one code path.

use std::path::{Path, PathBuf};

/// Resolved filesystem layout for one shep home
///
/// All paths are derived from `$SHEP_HOME` (default `<home>/.shep`); nothing
/// here touches the filesystem — creation happens daemon-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShepPaths {
    /// Root: `$SHEP_HOME`
    pub home: PathBuf,
    /// Daemon config: `shep.toml`
    pub daemon_config: PathBuf,
    /// Flock snapshot (muster roll): `flock.json`
    pub snapshot: PathBuf,
    /// Log directory
    pub logs: PathBuf,
    /// Pid-file directory
    pub pids: PathBuf,
    /// Runtime dir (sockets; created 0700)
    pub run: PathBuf,
    /// Control socket: `run/shep.sock`
    pub socket: PathBuf,
    /// Bark history ring: `barks.jsonl`
    pub barks: PathBuf,
}

impl ShepPaths {
    /// Windows named-pipe identity for this home: `\\.\pipe\shep-<sanitized>`
    ///
    /// Derived from the home path (non-alphanumerics become `-`) so distinct
    /// `$SHEP_HOME`s never collide on the global pipe namespace.
    #[must_use]
    pub fn pipe_name(&self) -> String {
        let sanitized: String = self
            .home
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let trimmed = sanitized.trim_matches('-');
        format!(r"\\.\pipe\shep-{trimmed}")
    }

    /// Resolves the layout from an environment lookup and the user's home dir
    #[must_use]
    pub fn resolve(env: &dyn Fn(&str) -> Option<String>, home_dir: &Path) -> Self {
        let home = env("SHEP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".shep"));
        let run = home.join("run");
        Self {
            daemon_config: home.join("shep.toml"),
            snapshot: home.join("flock.json"),
            logs: home.join("logs"),
            pids: home.join("pids"),
            socket: run.join("shep.sock"),
            barks: home.join("barks.jsonl"),
            run,
            home,
        }
    }
}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core paths && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/paths.rs crates/shep-core/src/lib.rs
git commit -m "feat(core): ShepPaths resolver for the SHEP_HOME layout

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: AppConfig struct (Flockfile per-app schema)

**Files:**
- Create: `crates/shep-core/src/config/mod.rs`, `crates/shep-core/src/config/app.rs`
- Modify: `crates/shep-core/src/lib.rs` (add `pub mod config;`)

**Interfaces:**
- Consumes: `MemSize`, `UpDuration` from `crate::values` (Tasks 2-3).
- Produces: `pub struct AppConfig` — the v1 field set (exact names below; serde `deny_unknown_fields`; **manual redacting Debug per IR-41**); `pub struct ProbeConfig { kind: ProbeKind, target, interval, timeout, failure_threshold }` + `pub enum ProbeKind { Http, Tcp, Exec }`; `AppConfig::minimal(name: &str, script: &str) -> Self` (infallible constructor with defaults). Field defaults per spec §4/§7: `autorestart: true`, `min_uptime: 1000ms`, `max_restarts: 16`, `kill_timeout: 1600ms`, `listen_timeout: 3000ms`, `graceful_timeout: 8000ms`, `autostart: true`, probe `interval: 10s` / `timeout: 5s` / `failure_threshold: 3`.

- [ ] **Step 1: Write failing tests** — `config/app.rs` test mod:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::{MemSize, UpDuration};

    #[test]
    fn minimal_config_gets_spec_defaults() {
        let app = AppConfig::minimal("web", "./server");
        assert_eq!(app.name, "web");
        assert_eq!(app.script, "./server");
        assert!(app.autorestart);
        assert!(app.autostart);
        assert_eq!(app.instances, 1);
        assert_eq!(app.min_uptime, UpDuration::from_millis(1000));
        assert_eq!(app.max_restarts, 16);
        assert_eq!(app.kill_timeout, UpDuration::from_millis(1600));
        assert_eq!(app.listen_timeout, UpDuration::from_millis(3000));
        assert_eq!(app.graceful_timeout, UpDuration::from_millis(8000));
        assert!(app.max_memory.is_none());
        assert!(app.fold.is_none());
    }

    #[test]
    fn toml_round_trip_with_newtypes() {
        let toml_src = r#"
name = "worker"
script = "python3"
args = ["job.py", "--fast"]
max_memory = "512M"
min_uptime = "5s"
fold = "backend"
env = { RUST_LOG = "info" }
"#;
        let app: AppConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(app.max_memory, Some("512M".parse::<MemSize>().unwrap()));
        assert_eq!(app.min_uptime, UpDuration::from_millis(5000));
        assert_eq!(app.fold.as_deref(), Some("backend"));
        assert_eq!(app.env.get("RUST_LOG").map(String::as_str), Some("info"));
        assert_eq!(app.args, vec!["job.py", "--fast"]);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = toml::from_str::<AppConfig>("name = \"x\"\nscript = \"y\"\nmax_memory_restart = \"1G\"")
            .unwrap_err();
        assert!(err.to_string().contains("max_memory_restart"), "{err}");
    }

    #[test]
    fn probe_config_parses_with_defaults() {
        let src = r#"
name = "api"
script = "./api"

[readiness_probe]
kind = "http"
target = "http://127.0.0.1:8080/healthz"
"#;
        let app: AppConfig = toml::from_str(src).unwrap();
        let probe = app.readiness_probe.unwrap();
        assert_eq!(probe.kind, ProbeKind::Http);
        assert_eq!(probe.target, "http://127.0.0.1:8080/healthz");
        assert_eq!(probe.interval, UpDuration::from_millis(10_000));
        assert_eq!(probe.timeout, UpDuration::from_millis(5_000));
        assert_eq!(probe.failure_threshold, 3);
        assert!(app.liveness_probe.is_none());
    }

    #[test]
    fn debug_redacts_env_values() {
        // IR-41: env may carry secrets; Debug output lands in daemon logs.
        // Exact string pinned so a lazy derive(Debug) refactor fails here.
        let mut app = AppConfig::minimal("web", "./srv");
        app.env.insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        app.env.insert("RUST_LOG".to_string(), "info".to_string());
        assert_eq!(
            format!("{app:?}"),
            "AppConfig { name: \"web\", script: \"./srv\", env: <2 vars>, .. }"
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core config::app`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement.** `config/mod.rs`:

```rust
//! Configuration: per-app schema (Flockfile), normalization, discovery,
//! and the daemon's own `shep.toml`

pub mod app;

pub use app::AppConfig;
```

`config/app.rs` (above tests):

```rust
//! Per-app configuration schema — one sheep's Flockfile entry

use core::fmt;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::values::{MemSize, UpDuration};

/// How a health probe checks a sheep
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    /// HTTP GET must return 2xx
    Http,
    /// TCP connect must succeed
    Tcp,
    /// Command must exit 0
    Exec,
}

/// Readiness/liveness probe configuration (spec §7)
// wire format: changing field names/defaults is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    /// Probe mechanism
    pub kind: ProbeKind,
    /// URL (http), `host:port` (tcp), or command line (exec)
    pub target: String,
    /// Time between probes (default 10s)
    #[serde(default = "default_probe_interval")]
    pub interval: UpDuration,
    /// Per-probe timeout (default 5s)
    #[serde(default = "default_probe_timeout")]
    pub timeout: UpDuration,
    /// Consecutive failures before the probe reports unhealthy (default 3)
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
}

fn default_probe_interval() -> UpDuration {
    UpDuration::from_millis(10_000)
}
fn default_probe_timeout() -> UpDuration {
    UpDuration::from_millis(5_000)
}
fn default_failure_threshold() -> u32 {
    3
}

/// Per-app configuration — one sheep's entry in a Flockfile
///
/// Field names are the Flockfile contract (sheep-native; pm2 spellings are
/// rejected — the importer translates them). Unknown fields are errors so
/// typos fail loudly at parse time.
///
/// # Example
/// ```
/// use shep_core::config::AppConfig;
///
/// let app: AppConfig = toml::from_str("name = \"web\"\nscript = \"./srv\"").unwrap();
/// assert!(app.autorestart); // spec default
/// ```
// wire format: changing field names/defaults is a breaking change
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AppConfig {
    /// Unique sheep name (required)
    pub name: String,
    /// Executable or script path (required)
    pub script: String,
    /// Arguments passed to the script
    pub args: Vec<String>,
    /// Working directory (default: daemon's cwd at spawn registration)
    pub cwd: Option<String>,
    /// Interpreter override (`"none"` = run script directly)
    pub interpreter: Option<String>,
    /// Environment for the sheep (merged over the daemon's filtered env)
    pub env: BTreeMap<String, String>,
    /// Instance count ("cluster" = N fork instances; spec §4)
    pub instances: u32,
    /// Restart on unexpected exit
    pub autorestart: bool,
    /// Start when the daemon starts / on `shep muster`
    pub autostart: bool,
    /// Exit codes treated as clean stop (no restart)
    pub stop_exit_codes: Vec<i32>,
    /// Uptime below this marks an exit as unstable
    pub min_uptime: UpDuration,
    /// Consecutive unstable exits before `errored`
    pub max_restarts: u32,
    /// Fixed delay before every restart (alternative to backoff)
    pub restart_delay: Option<UpDuration>,
    /// Initial backoff delay; grows ×1.5 capped at 15s (spec §4)
    pub exp_backoff_restart_delay: Option<UpDuration>,
    /// Stop signal (default SIGTERM; parsed daemon-side into StopSignal)
    pub kill_signal: Option<String>,
    /// Grace period between stop signal and SIGKILL
    pub kill_timeout: UpDuration,
    /// Send `{"kind":"shutdown"}` on the shepherd channel instead of a signal
    pub shutdown_with_message: bool,
    /// Readiness fallback window when no ready signal/probe configured
    pub listen_timeout: UpDuration,
    /// Drain window for the old instance during reload
    pub graceful_timeout: UpDuration,
    /// Memory ceiling — polling enforcer restarts above this
    pub max_memory: Option<MemSize>,
    /// Watch files and restart on change
    pub watch: bool,
    /// Watch ignore globs (defaults added daemon-side: dot-entries, node_modules)
    pub ignore_watch: Vec<String>,
    /// Watch debounce window (default 500ms, applied daemon-side)
    pub watch_delay: Option<UpDuration>,
    /// Cron pattern for scheduled restarts (croner dialect)
    pub cron_restart: Option<String>,
    /// Fold (group) this sheep belongs to
    pub fold: Option<String>,
    /// Run as this user (unix)
    pub user: Option<String>,
    /// Run as this group (unix)
    pub group: Option<String>,
    /// Stdout log file (default: `$SHEP_HOME/logs/<name>-out.log`)
    pub out_file: Option<String>,
    /// Stderr log file (default: `$SHEP_HOME/logs/<name>-err.log`)
    pub err_file: Option<String>,
    /// Merge instance logs into one file pair
    pub merge_logs: bool,
    /// Expect `{"kind":"ready"}` on the shepherd channel
    pub wait_ready: bool,
    /// Bind listen sockets with SO_REUSEPORT (enables zero-downtime reload)
    // SUPERSEDED, left as written: shep binds nothing and cannot set the
    // option, and what reload provides is an overlap, not zero downtime.
    // The shipped `AppConfig::reuse_port` doc says both. This file is the
    // record of what was planned, not of what landed.
    pub reuse_port: bool,
    /// Readiness probe — gates reload's AwaitReady (spec §7)
    pub readiness_probe: Option<ProbeConfig>,
    /// Liveness probe — failures feed the restart policy (spec §7)
    pub liveness_probe: Option<ProbeConfig>,
    /// Watch include globs (empty = watch cwd)
    pub watch_options: Vec<String>,
    /// Timezone for `cron_restart` (IANA name)
    pub cron_timezone: Option<String>,
    /// Env var receiving the instance slot (default `SHEP_INSTANCE`)
    pub increment_var: Option<String>,
}

/// Debug implementation does not leak env values (IR-41)
impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("name", &self.name)
            .field("script", &self.script)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .finish_non_exhaustive()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            script: String::new(),
            args: Vec::new(),
            cwd: None,
            interpreter: None,
            env: BTreeMap::new(),
            instances: 1,
            autorestart: true,
            autostart: true,
            stop_exit_codes: Vec::new(),
            min_uptime: UpDuration::from_millis(1000),
            max_restarts: 16,
            restart_delay: None,
            exp_backoff_restart_delay: None,
            kill_signal: None,
            kill_timeout: UpDuration::from_millis(1600),
            shutdown_with_message: false,
            listen_timeout: UpDuration::from_millis(3000),
            graceful_timeout: UpDuration::from_millis(8000),
            max_memory: None,
            watch: false,
            ignore_watch: Vec::new(),
            watch_delay: None,
            cron_restart: None,
            fold: None,
            user: None,
            group: None,
            out_file: None,
            err_file: None,
            merge_logs: false,
            wait_ready: false,
            reuse_port: false,
            readiness_probe: None,
            liveness_probe: None,
            watch_options: Vec::new(),
            cron_timezone: None,
            increment_var: None,
        }
    }
}

impl AppConfig {
    /// A minimal config with spec defaults — the programmatic entry point
    #[must_use]
    pub fn minimal(name: &str, script: &str) -> Self {
        Self {
            name: name.to_string(),
            script: script.to_string(),
            ..Self::default()
        }
    }
}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core config && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS. (Note: `name`/`script` required-ness is enforced by Task 7's normalize, not serde — `default` + empty-string check keeps one validation path.)

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/config/ crates/shep-core/src/lib.rs
git commit -m "feat(core): AppConfig with v1 field set and spec defaults

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: Normalization (AppConfig → ResolvedApp)

**Files:**
- Create: `crates/shep-core/src/config/normalize.rs`
- Modify: `crates/shep-core/src/config/mod.rs` (add `pub mod normalize;` + `pub use normalize::{ResolvedApp, ConfigError};`)

**Interfaces:**
- Consumes: `AppConfig` (Task 6).
- Produces: `pub fn normalize(app: AppConfig) -> Result<ResolvedApp, ConfigError>`; `pub struct ResolvedApp` with PRIVATE `config` field + accessors `config(&self) -> &AppConfig` / `into_config(self) -> AppConfig` (a public field would let any crate forge the proof token — caught in execution review); `pub enum ConfigError { MissingName, MissingScript, ZeroInstances, InvalidCron(String), DuplicateName(String) }` + `pub fn normalize_all(apps: Vec<AppConfig>) -> Result<Vec<ResolvedApp>, ConfigError>` (adds duplicate-name detection).

- [ ] **Step 1: Write failing tests** — `config/normalize.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn valid_minimal_config_normalizes() {
        let resolved = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        assert_eq!(resolved.config.name, "web");
    }

    #[test]
    fn missing_name_and_script_are_distinct_errors() {
        assert_eq!(
            normalize(AppConfig::minimal("", "./srv")).unwrap_err(),
            ConfigError::MissingName
        );
        assert_eq!(
            normalize(AppConfig::minimal("web", "")).unwrap_err(),
            ConfigError::MissingScript
        );
    }

    #[test]
    fn zero_instances_rejected() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 0;
        assert_eq!(normalize(app).unwrap_err(), ConfigError::ZeroInstances);
    }

    #[test]
    fn bad_cron_pattern_rejected_with_pattern_in_error() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("not a cron".to_string());
        match normalize(app).unwrap_err() {
            ConfigError::InvalidCron(p) => assert_eq!(p, "not a cron"),
            other => panic!("expected InvalidCron, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_names_rejected_across_a_flock() {
        let apps = vec![
            AppConfig::minimal("web", "./a"),
            AppConfig::minimal("web", "./b"),
        ];
        assert_eq!(
            normalize_all(apps).unwrap_err(),
            ConfigError::DuplicateName("web".to_string())
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core normalize`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement** (cron validation: five-field whitespace check now, croner crate arrives with the daemon phase that executes crons — validating dialect then; one seam, noted):

```rust
//! Validation and normalization: `AppConfig` -> `ResolvedApp`
//!
//! `ResolvedApp` is a proof token: constructing one is only possible through
//! [`normalize`], so daemon code can require it and skip re-validation.

use core::fmt;

use std::collections::BTreeSet;

use crate::config::AppConfig;

/// A validated app config — only obtainable via [`normalize`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedApp {
    /// The validated configuration
    pub config: AppConfig,
}

/// Validates one app config
///
/// # Errors
///
/// - [`ConfigError::MissingName`] — `name` is empty.
/// - [`ConfigError::MissingScript`] — `script` is empty.
/// - [`ConfigError::ZeroInstances`] — `instances == 0`.
/// - [`ConfigError::InvalidCron`] — `cron_restart` is not a 5-field pattern.
pub fn normalize(app: AppConfig) -> Result<ResolvedApp, ConfigError> {
    if app.name.is_empty() {
        return Err(ConfigError::MissingName);
    }
    if app.script.is_empty() {
        return Err(ConfigError::MissingScript);
    }
    if app.instances == 0 {
        return Err(ConfigError::ZeroInstances);
    }
    if let Some(pattern) = &app.cron_restart {
        // ponytail: field-count check only; croner dialect validation lands
        // with the daemon phase that actually schedules crons
        if pattern.split_whitespace().count() != 5 {
            return Err(ConfigError::InvalidCron(pattern.clone()));
        }
    }
    Ok(ResolvedApp { config: app })
}

/// Validates a whole flock, rejecting duplicate sheep names
///
/// # Errors
///
/// Everything [`normalize`] returns, plus
/// [`ConfigError::DuplicateName`] — two apps share a `name`.
pub fn normalize_all(apps: Vec<AppConfig>) -> Result<Vec<ResolvedApp>, ConfigError> {
    let mut seen = BTreeSet::new();
    apps.into_iter()
        .map(|app| {
            if !seen.insert(app.name.clone()) {
                return Err(ConfigError::DuplicateName(app.name));
            }
            normalize(app)
        })
        .collect()
}

/// Error type returned from [`normalize`] and [`normalize_all`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `name` is empty
    MissingName,
    /// `script` is empty
    MissingScript,
    /// `instances` is zero
    ZeroInstances,
    /// `cron_restart` is not a 5-field cron pattern (carries the pattern)
    InvalidCron(String),
    /// Two apps in one flock share this name
    DuplicateName(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => f.write_str("app config is missing a name"),
            Self::MissingScript => f.write_str("app config is missing a script"),
            Self::ZeroInstances => f.write_str("instances must be at least 1"),
            Self::InvalidCron(p) => write!(f, "invalid cron pattern `{p}`"),
            Self::DuplicateName(n) => write!(f, "duplicate sheep name `{n}`"),
        }
    }
}

impl core::error::Error for ConfigError {}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core normalize && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/config/
git commit -m "feat(core): AppConfig normalization with proof-token ResolvedApp

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: Flockfile discovery + multi-format parse

**Files:**
- Create: `crates/shep-core/src/config/flockfile.rs`
- Modify: `crates/shep-core/src/config/mod.rs` (add `pub mod flockfile;` + re-exports `Flockfile, FlockfileError`)

**Interfaces:**
- Consumes: `AppConfig` (Task 6).
- Produces: `pub struct Flockfile { pub apps: Vec<AppConfig> }`; `Flockfile::parse(source: &str, format: FlockFormat) -> Result<Flockfile, FlockfileError>`; `pub enum FlockFormat { Toml, Yaml, Json, Json5 }` + `FlockFormat::from_path(path: &Path) -> Option<FlockFormat>` (`.toml`/`.yaml`,`.yml`/`.json`/`.json5`); `pub fn discover(dir: &Path) -> Option<PathBuf>` (order: `Flockfile.toml`, `Flockfile.yaml`, `Flockfile.yml`, `Flockfile.json`, `Flockfile.json5`, then lowercase `flockfile.*` same order); `pub enum FlockfileError { Toml(String), Yaml(String), Json(String), Json5(String), NoApps }`. Document shape: TOML uses `[[app]]` array-of-tables; YAML/JSON use `{"app": [...]}`.

- [ ] **Step 1: Write failing tests**:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_array_of_tables() {
        let src = r#"
[[app]]
name = "web"
script = "./srv"

[[app]]
name = "worker"
script = "python3"
args = ["job.py"]
"#;
        let flock = Flockfile::parse(src, FlockFormat::Toml).unwrap();
        assert_eq!(flock.apps.len(), 2);
        assert_eq!(flock.apps[1].name, "worker");
    }

    #[test]
    fn json_and_json5_and_yaml() {
        let json = r#"{ "app": [{ "name": "web", "script": "./srv" }] }"#;
        assert_eq!(Flockfile::parse(json, FlockFormat::Json).unwrap().apps.len(), 1);

        let json5 = r#"{ app: [{ name: "web", script: "./srv" }], /* comment */ }"#;
        assert_eq!(Flockfile::parse(json5, FlockFormat::Json5).unwrap().apps.len(), 1);

        let yaml = "app:\n  - name: web\n    script: ./srv\n";
        assert_eq!(Flockfile::parse(yaml, FlockFormat::Yaml).unwrap().apps.len(), 1);
    }

    #[test]
    fn empty_app_list_is_an_error() {
        assert_eq!(
            Flockfile::parse("app: []\n", FlockFormat::Yaml).unwrap_err(),
            FlockfileError::NoApps
        );
    }

    #[test]
    fn parse_errors_carry_the_backend_message() {
        match Flockfile::parse("not toml [[", FlockFormat::Toml).unwrap_err() {
            FlockfileError::Toml(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Toml error, got {other:?}"),
        }
    }

    #[test]
    fn format_from_path() {
        use std::path::Path;
        assert_eq!(FlockFormat::from_path(Path::new("Flockfile.toml")), Some(FlockFormat::Toml));
        assert_eq!(FlockFormat::from_path(Path::new("f.yml")), Some(FlockFormat::Yaml));
        assert_eq!(FlockFormat::from_path(Path::new("f.json5")), Some(FlockFormat::Json5));
        assert_eq!(FlockFormat::from_path(Path::new("f.js")), None);
    }

    #[test]
    fn discover_prefers_toml_then_capitalized() {
        let dir = std::env::temp_dir().join(format!("shep-flock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("flockfile.json"), "{}").unwrap();
        std::fs::write(dir.join("Flockfile.yaml"), "").unwrap();
        assert_eq!(discover(&dir), Some(dir.join("Flockfile.yaml")));
        std::fs::write(dir.join("Flockfile.toml"), "").unwrap();
        assert_eq!(discover(&dir), Some(dir.join("Flockfile.toml")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core flockfile`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement**:

```rust
//! Flockfile: discovery and multi-format parsing
//!
//! One document shape across formats: a list of app tables under the `app`
//! key (`[[app]]` in TOML). Parsing is strict serde — no code execution;
//! `.js` configs are the CLI's job (it shells out to node and feeds the
//! resulting JSON through [`FlockFormat::Json`]).

use core::fmt;

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::AppConfig;

/// A parsed Flockfile: the declared flock
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flockfile {
    /// App entries in declaration order
    pub apps: Vec<AppConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFlockfile {
    #[serde(default, rename = "app")]
    apps: Vec<AppConfig>,
}

/// Input format of a Flockfile
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlockFormat {
    /// `Flockfile.toml` — `[[app]]` tables
    Toml,
    /// `.yaml`/`.yml`
    Yaml,
    /// Strict JSON
    Json,
    /// JSON5 (comments, trailing commas)
    Json5,
}

impl FlockFormat {
    /// Maps a file extension to its format (`None` = unsupported, e.g. `.js`)
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            "json" => Some(Self::Json),
            "json5" => Some(Self::Json5),
            _ => None,
        }
    }
}

impl Flockfile {
    /// Parses Flockfile source text in the given format
    ///
    /// # Errors
    ///
    /// - Format variants ([`FlockfileError::Toml`] etc.) — backend parse
    ///   failure, carrying the backend's message.
    /// - [`FlockfileError::NoApps`] — parsed fine but declared no apps.
    pub fn parse(source: &str, format: FlockFormat) -> Result<Self, FlockfileError> {
        let raw: RawFlockfile = match format {
            FlockFormat::Toml => {
                toml::from_str(source).map_err(|e| FlockfileError::Toml(e.to_string()))?
            }
            FlockFormat::Yaml => {
                serde_yml::from_str(source).map_err(|e| FlockfileError::Yaml(e.to_string()))?
            }
            FlockFormat::Json => {
                serde_json::from_str(source).map_err(|e| FlockfileError::Json(e.to_string()))?
            }
            FlockFormat::Json5 => {
                json5::from_str(source).map_err(|e| FlockfileError::Json5(e.to_string()))?
            }
        };
        if raw.apps.is_empty() {
            return Err(FlockfileError::NoApps);
        }
        Ok(Self { apps: raw.apps })
    }
}

const DISCOVERY_ORDER: [&str; 10] = [
    "Flockfile.toml",
    "Flockfile.yaml",
    "Flockfile.yml",
    "Flockfile.json",
    "Flockfile.json5",
    "flockfile.toml",
    "flockfile.yaml",
    "flockfile.yml",
    "flockfile.json",
    "flockfile.json5",
];

/// Finds the Flockfile in a directory (spec §5 order, extended with the
/// `.yml`/`.json5` spellings — spec updated to this ten-name list)
#[must_use]
pub fn discover(dir: &Path) -> Option<PathBuf> {
    DISCOVERY_ORDER
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// Error type returned from [`Flockfile::parse`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlockfileError {
    /// TOML backend rejected the source (carries its message)
    Toml(String),
    /// YAML backend rejected the source
    Yaml(String),
    /// JSON backend rejected the source
    Json(String),
    /// JSON5 backend rejected the source
    Json5(String),
    /// The document parsed but declared no apps
    NoApps,
}

impl fmt::Display for FlockfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(m) => write!(f, "invalid TOML Flockfile: {m}"),
            Self::Yaml(m) => write!(f, "invalid YAML Flockfile: {m}"),
            Self::Json(m) => write!(f, "invalid JSON Flockfile: {m}"),
            Self::Json5(m) => write!(f, "invalid JSON5 Flockfile: {m}"),
            Self::NoApps => f.write_str("Flockfile declares no apps"),
        }
    }
}

impl core::error::Error for FlockfileError {}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core flockfile && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/config/
git commit -m "feat(core): Flockfile discovery and toml/yaml/json/json5 parsing

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: DaemonConfig (shep.toml) + layering

**Files:**
- Create: `crates/shep-core/src/config/daemon.rs`
- Modify: `crates/shep-core/src/config/mod.rs` (add module + re-export `DaemonConfig`)

**Interfaces:**
- Produces: `pub struct DaemonConfig { pub daemon: DaemonSection, pub dog: BTreeMap<String, toml::Table> }`; `DaemonSection { pub log_json: bool, pub enabled_dogs: Vec<String> }`; `DaemonConfig::load(file_source: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Result<DaemonConfig, DaemonConfigError>` — layering file < env (`SHEP_LOG_JSON=1|true`); CLI-flag layer applied by the CLI later (it mutates the struct). Dog sections stay raw `toml::Table` — each dog deserializes its own section (typed per-dog config lives with the dog, Phase 5).

- [ ] **Step 1: Write failing tests**:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = DaemonConfig::load(None, &no_env).unwrap();
        assert!(!cfg.daemon.log_json);
        assert!(cfg.daemon.enabled_dogs.is_empty());
        assert!(cfg.dog.is_empty());
    }

    #[test]
    fn file_sets_values_and_keeps_dog_sections_raw() {
        let src = r#"
[daemon]
log_json = true
enabled_dogs = ["metrics"]

[dog.metrics]
port = 9615
"#;
        let cfg = DaemonConfig::load(Some(src), &no_env).unwrap();
        assert!(cfg.daemon.log_json);
        assert_eq!(cfg.daemon.enabled_dogs, vec!["metrics"]);
        assert_eq!(
            cfg.dog["metrics"]["port"].as_integer(),
            Some(9615)
        );
    }

    #[test]
    fn env_overrides_file() {
        let env = |k: &str| (k == "SHEP_LOG_JSON").then(|| "true".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nlog_json = false"), &env).unwrap();
        assert!(cfg.daemon.log_json);
    }

    #[test]
    fn socket_override_via_file_and_env() {
        let cfg = DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/a.sock\""), &no_env).unwrap();
        assert_eq!(cfg.daemon.socket.as_deref(), Some(std::path::Path::new("/tmp/a.sock")));
        let env = |k: &str| (k == "SHEP_SOCKET").then(|| "/tmp/b.sock".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/a.sock\""), &env).unwrap();
        assert_eq!(cfg.daemon.socket.as_deref(), Some(std::path::Path::new("/tmp/b.sock")));
    }

    #[test]
    fn bad_toml_is_a_typed_error() {
        assert!(matches!(
            DaemonConfig::load(Some("[daemon"), &no_env),
            Err(DaemonConfigError::Toml(_))
        ));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core daemon`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement**:

```rust
//! Daemon-level configuration: `$SHEP_HOME/shep.toml`
//!
//! Layering (spec §5): file < `SHEP_*` env < CLI flags. This module applies
//! the first two; the CLI applies its flags onto the returned struct.

use core::fmt;

use std::collections::BTreeMap;

use serde::Deserialize;

/// The `[daemon]` section
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonSection {
    /// Emit the daemon's own logs as JSON lines
    pub log_json: bool,
    /// Control-socket path override (default: `$SHEP_HOME/run/shep.sock`)
    pub socket: Option<std::path::PathBuf>,
    /// Dogs to autostart with the daemon (`shep enable` writes this)
    pub enabled_dogs: Vec<String>,
}

/// Parsed daemon configuration with raw per-dog sections
///
/// Dog sections stay untyped here: each dog deserializes its own
/// `[dog.<name>]` table so dog config schemas live with the dog code.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DaemonConfig {
    /// The `[daemon]` section
    pub daemon: DaemonSection,
    /// Raw `[dog.<name>]` sections keyed by dog name
    pub dog: BTreeMap<String, toml::Table>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawDaemonConfig {
    daemon: DaemonSection,
    dog: BTreeMap<String, toml::Table>,
}

impl DaemonConfig {
    /// Builds config from optional file source + environment overrides
    ///
    /// # Errors
    ///
    /// - [`DaemonConfigError::Toml`] — the file source is invalid TOML.
    /// - [`DaemonConfigError::BadEnvValue`] — a `SHEP_*` value is not
    ///   parseable (`SHEP_LOG_JSON` accepts `1|0|true|false`).
    pub fn load(
        file_source: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, DaemonConfigError> {
        let raw: RawDaemonConfig = match file_source {
            Some(src) => toml::from_str(src).map_err(|e| DaemonConfigError::Toml(e.to_string()))?,
            None => RawDaemonConfig::default(),
        };
        let mut cfg = Self { daemon: raw.daemon, dog: raw.dog };
        if let Some(v) = env("SHEP_LOG_JSON") {
            cfg.daemon.log_json = match v.as_str() {
                "1" | "true" => true,
                "0" | "false" => false,
                _ => return Err(DaemonConfigError::BadEnvValue("SHEP_LOG_JSON", v)),
            };
        }
        if let Some(v) = env("SHEP_SOCKET") {
            cfg.daemon.socket = Some(std::path::PathBuf::from(v));
        }
        Ok(cfg)
    }
}

/// Error type returned from [`DaemonConfig::load`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonConfigError {
    /// `shep.toml` is invalid TOML (carries the parser message)
    Toml(String),
    /// A `SHEP_*` env var held an unparseable value (var name, value)
    BadEnvValue(&'static str, String),
}

impl fmt::Display for DaemonConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(m) => write!(f, "invalid shep.toml: {m}"),
            Self::BadEnvValue(var, v) => write!(f, "invalid value `{v}` for {var}"),
        }
    }
}

impl core::error::Error for DaemonConfigError {}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core daemon && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/config/
git commit -m "feat(core): DaemonConfig with shep.toml parsing and env layering

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: ProcessSelector

**Files:**
- Create: `crates/shep-core/src/selector.rs`
- Modify: `crates/shep-core/src/lib.rs` (add `pub mod selector;`)

**Interfaces:**
- Consumes: nothing internal.
- Produces: `pub enum ProcessSelector { All, Id(u32), Name(String), Regex(regex::Regex), Fold(String) }`; `ProcessSelector::parse(input: &str) -> Result<Self, SelectorError>` — rules: `"all"` → All; all-digits → Id; `/re/` (slash-delimited) → Regex; `fold:<name>` → Fold; anything else → Name. `ProcessSelector::matches(&self, name: &str, id: u32, fold: Option<&str>) -> bool`. `pub enum SelectorError { Empty, BadRegex(String), EmptyFold }`.

- [ ] **Step 1: Write failing tests**:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rules() {
        assert!(matches!(ProcessSelector::parse("all").unwrap(), ProcessSelector::All));
        assert!(matches!(ProcessSelector::parse("3").unwrap(), ProcessSelector::Id(3)));
        assert!(matches!(
            ProcessSelector::parse("web").unwrap(),
            ProcessSelector::Name(n) if n == "web"
        ));
        assert!(matches!(
            ProcessSelector::parse("/^w/").unwrap(),
            ProcessSelector::Regex(_)
        ));
        assert!(matches!(
            ProcessSelector::parse("fold:backend").unwrap(),
            ProcessSelector::Fold(fname) if fname == "backend"
        ));
    }

    #[test]
    fn parse_errors() {
        assert_eq!(ProcessSelector::parse("").unwrap_err(), SelectorError::Empty);
        assert_eq!(
            ProcessSelector::parse("fold:").unwrap_err(),
            SelectorError::EmptyFold
        );
        assert!(matches!(
            ProcessSelector::parse("/((/").unwrap_err(),
            SelectorError::BadRegex(_)
        ));
    }

    #[test]
    fn matching() {
        let by_name = ProcessSelector::parse("web").unwrap();
        assert!(by_name.matches("web", 0, None));
        assert!(!by_name.matches("worker", 0, None));

        let by_regex = ProcessSelector::parse("/^w/").unwrap();
        assert!(by_regex.matches("worker", 9, None));
        assert!(!by_regex.matches("api", 9, None));

        let by_fold = ProcessSelector::parse("fold:backend").unwrap();
        assert!(by_fold.matches("anything", 0, Some("backend")));
        assert!(!by_fold.matches("anything", 0, None));

        assert!(ProcessSelector::parse("all").unwrap().matches("x", 42, None));
        assert!(ProcessSelector::parse("42").unwrap().matches("x", 42, None));
    }

    #[test]
    fn a_name_that_looks_numeric_is_an_id() {
        // Documented precedence (spec §9): digits select by id. A sheep
        // literally named "42" must be selected by /^42$/ or renamed.
        assert!(matches!(ProcessSelector::parse("42").unwrap(), ProcessSelector::Id(42)));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core selector`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement**:

```rust
//! Target selection: one parse for every CLI verb and RPC filter
//!
//! Precedence: `all` > `fold:<name>` > `/regex/` > all-digits id > name.

use core::fmt;

/// A parsed process selector (spec §9: name, id, `all`, `/regex/`, `fold:`)
#[derive(Debug, Clone)]
pub enum ProcessSelector {
    /// Every sheep in the flock
    All,
    /// By numeric id
    Id(u32),
    /// By exact name
    Name(String),
    /// By regex over names (slash-delimited on the CLI)
    Regex(regex::Regex),
    /// Every sheep in a fold
    Fold(String),
}

impl ProcessSelector {
    /// Parses CLI selector syntax
    ///
    /// # Errors
    ///
    /// - [`SelectorError::Empty`] — empty input.
    /// - [`SelectorError::EmptyFold`] — `fold:` with no name.
    /// - [`SelectorError::BadRegex`] — `/re/` body rejected by the regex
    ///   crate (carries its message).
    pub fn parse(input: &str) -> Result<Self, SelectorError> {
        if input.is_empty() {
            return Err(SelectorError::Empty);
        }
        if input == "all" {
            return Ok(Self::All);
        }
        if let Some(fold) = input.strip_prefix("fold:") {
            if fold.is_empty() {
                return Err(SelectorError::EmptyFold);
            }
            return Ok(Self::Fold(fold.to_string()));
        }
        if input.len() >= 2 && input.starts_with('/') && input.ends_with('/') {
            let body = &input[1..input.len() - 1];
            return regex::Regex::new(body)
                .map(Self::Regex)
                .map_err(|e| SelectorError::BadRegex(e.to_string()));
        }
        if input.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(id) = input.parse() {
                return Ok(Self::Id(id));
            }
        }
        Ok(Self::Name(input.to_string()))
    }

    /// Tests one sheep against this selector
    #[must_use]
    pub fn matches(&self, name: &str, id: u32, fold: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Id(want) => *want == id,
            Self::Name(want) => want == name,
            Self::Regex(re) => re.is_match(name),
            Self::Fold(want) => fold == Some(want.as_str()),
        }
    }
}

/// Error type returned from [`ProcessSelector::parse`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorError {
    /// The selector string was empty
    Empty,
    /// `fold:` with no fold name after the colon
    EmptyFold,
    /// The `/regex/` body failed to compile (carries the regex message)
    BadRegex(String),
}

impl fmt::Display for SelectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("selector is empty"),
            Self::EmptyFold => f.write_str("fold selector is missing a name"),
            Self::BadRegex(m) => write!(f, "invalid selector regex: {m}"),
        }
    }
}

impl core::error::Error for SelectorError {}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core selector && cargo clippy -p shep-core --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-core/src/selector.rs crates/shep-core/src/lib.rs
git commit -m "feat(core): ProcessSelector parse + match (all/id/name/regex/fold)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 11: Protocol types (Request/Response/Envelope/BusEvent)

**Files:**
- Create: `crates/shep-core/src/protocol/mod.rs`, `crates/shep-core/src/protocol/request.rs`, `crates/shep-core/src/protocol/events.rs`
- Modify: `crates/shep-core/src/lib.rs` (add `pub mod protocol;`)

**Interfaces:**
- Consumes: `AppConfig` (Task 6), `ProcStatus` (Task 4), `ProcessSelector` — NOT on the wire; wire carries `SelectorSpec` (serializable mirror: `{All, Id(u32), Name(String), Regex(String), Fold(String)}`).
- Produces (all `Serialize + Deserialize`, snake_case tags):
  - `pub const PROTOCOL_VERSION: u32 = 1;`
  - `pub struct Hello { pub client_version: String, pub protocol: u32 }`, `pub struct HelloAck { pub daemon_version: String, pub protocol: u32, pub pid: u32 }`
  - `pub enum Request { Ping, ListFlock, Describe { selector: SelectorSpec }, Start { apps: Vec<AppConfig> }, Stop { selector: SelectorSpec }, Restart { selector: SelectorSpec }, Delete { selector: SelectorSpec }, KillDaemon, Subscribe { topics: Vec<String> } }` (Phase 2+ extends; `#[non_exhaustive]`)
  - `pub struct ProcessInfo { pub id: u32, pub name: String, pub status: ProcStatus, pub pid: Option<u32>, pub restarts: u32, pub uptime_ms: u64, pub fold: Option<String> }`
  - `pub enum Response { Pong, Flock(Vec<ProcessInfo>), Described(Vec<ProcessInfo>), Started(Vec<ProcessInfo>), Stopped(Vec<ProcessInfo>), Restarted(Vec<ProcessInfo>), Deleted(Vec<u32>), Subscribed, ShuttingDown }` (`#[non_exhaustive]`)
  - `pub struct Envelope { pub id: u64, pub deadline_ms: Option<u64>, pub body: Request }`, `pub struct Reply { pub id: u64, pub result: Result<Response, RpcError> }`, `pub struct RpcError { pub code: RpcErrorCode, pub message: String }`, `pub enum RpcErrorCode { NotFound, InvalidConfig, SpawnFailed, ProtocolMismatch, Internal }` (`#[non_exhaustive]`)
  - events.rs: `pub enum BusEvent { Process { event: ProcessEventKind, info: ProcessInfo, manually: bool, at_ms: u64 }, LogOut { id: u32, line: String }, LogErr { id: u32, line: String }, Dropped { count: u64 }, DaemonShutdown }` with `pub enum ProcessEventKind { Start, Online, Exit, Restart, Stop, Delete, Errored }` (`#[non_exhaustive]` both)

- [ ] **Step 1: Write failing wire-stability tests** — `protocol/request.rs` test mod (insta snapshots + hand-pinned fixtures):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::status::ProcStatus;

    fn sample_info() -> ProcessInfo {
        ProcessInfo {
            id: 3,
            name: "web".to_string(),
            status: ProcStatus::Online,
            pid: Some(4242),
            restarts: 1,
            uptime_ms: 60_000,
            fold: Some("backend".to_string()),
        }
    }

    #[test]
    fn request_wire_snapshots() {
        let requests = vec![
            Envelope { id: 1, deadline_ms: Some(5000), body: Request::Ping },
            Envelope { id: 2, deadline_ms: None, body: Request::ListFlock },
            Envelope {
                id: 3,
                deadline_ms: None,
                body: Request::Stop { selector: SelectorSpec::Name("web".to_string()) },
            },
            Envelope {
                id: 4,
                deadline_ms: None,
                body: Request::Start { apps: vec![AppConfig::minimal("web", "./srv")] },
            },
        ];
        insta::assert_json_snapshot!("request_wire_v1", requests);
    }

    #[test]
    fn reply_wire_snapshots() {
        let replies = vec![
            Reply { id: 1, result: Ok(Response::Pong) },
            Reply { id: 2, result: Ok(Response::Flock(vec![sample_info()])) },
            Reply {
                id: 3,
                result: Err(RpcError {
                    code: RpcErrorCode::NotFound,
                    message: "no sheep matches `web`".to_string(),
                }),
            },
        ];
        insta::assert_json_snapshot!("reply_wire_v1", replies);
    }

    #[test]
    fn v1_fixture_still_deserializes() {
        // Committed byte fixture from protocol v1 — if this breaks, bump
        // PROTOCOL_VERSION and record it in the CHANGELOG (IR-35).
        let fixture = r#"{"id":7,"deadline_ms":null,"body":{"kind":"stop","selector":{"kind":"name","value":"web"}}}"#;
        let env: Envelope = serde_json::from_str(fixture).unwrap();
        assert_eq!(env.id, 7);
        assert!(matches!(
            env.body,
            Request::Stop { selector: SelectorSpec::Name(ref n) } if n == "web"
        ));
    }

    #[test]
    fn hello_handshake_shape() {
        let hello = Hello { client_version: "0.1.0".to_string(), protocol: PROTOCOL_VERSION };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"client_version":"0.1.0","protocol":1}"#);
    }

    #[test]
    fn hello_reply_carries_typed_skew_error() {
        let refusal: HelloReply = Err(RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 1, client sent 2".to_string(),
        });
        let json = serde_json::to_string(&refusal).unwrap();
        assert_eq!(
            json,
            r#"{"Err":{"code":"protocol_mismatch","message":"daemon speaks protocol 1, client sent 2"}}"#
        );
        let back: HelloReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, refusal);
    }
}
```

And in `protocol/events.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::request::ProcessInfo;
    use crate::status::ProcStatus;

    #[test]
    fn bus_event_wire_snapshots() {
        let events = vec![
            BusEvent::Process {
                event: ProcessEventKind::Exit,
                info: ProcessInfo {
                    id: 3,
                    name: "web".to_string(),
                    status: ProcStatus::WaitingRestart,
                    pid: None,
                    restarts: 2,
                    uptime_ms: 500,
                    fold: None,
                },
                manually: false,
                at_ms: 1_700_000_000_000,
            },
            BusEvent::LogOut { id: 3, line: "listening on :8080".to_string() },
            BusEvent::Dropped { count: 17 },
        ];
        insta::assert_json_snapshot!("bus_event_wire_v1", events);
    }

    #[test]
    fn topics_follow_the_dotted_grammar() {
        // spec §6: process.* / log.out / log.err / daemon.*
        let e = BusEvent::LogOut { id: 1, line: String::new() };
        assert_eq!(e.topic(), "log.out");
        assert_eq!(BusEvent::DaemonShutdown.topic(), "daemon.shutdown");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core protocol`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement.** `protocol/mod.rs`:

```rust
//! The client<->daemon wire protocol (version 1)
//!
//! Typed request/response enums + bus events. Framing lives in [`wire`];
//! every type here is snapshot-pinned — changing any serialized shape is a
//! protocol version bump recorded in the CHANGELOG.

pub mod events;
pub mod request;
// `pub mod wire;` is added by the framing task — declaring it here would
// break this task's gates (E0583: file not found).

pub use events::{BusEvent, ProcessEventKind};
pub use request::{
    Envelope, Hello, HelloAck, HelloReply, ProcessInfo, Reply, Request, Response, RpcError,
    RpcErrorCode, SelectorSpec,
};

/// Wire protocol version; bump on any breaking change to serialized shapes
pub const PROTOCOL_VERSION: u32 = 1;
```

`protocol/request.rs`:

```rust
//! RPC frames: requests, responses, envelopes, and structured errors

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::status::ProcStatus;

/// Client's opening frame
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Client crate version (semver string)
    pub client_version: String,
    /// [`crate::protocol::PROTOCOL_VERSION`] the client speaks
    pub protocol: u32,
}

/// Daemon's handshake answer
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    /// Daemon crate version
    pub daemon_version: String,
    /// Protocol version the daemon speaks
    pub protocol: u32,
    /// Daemon pid
    pub pid: u32,
}

/// Serializable selector (mirror of [`crate::selector::ProcessSelector`];
/// regex travels as its source string)
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SelectorSpec {
    /// Every sheep
    All,
    /// By id
    Id(u32),
    /// By exact name
    Name(String),
    /// By regex source
    Regex(String),
    /// By fold name
    Fold(String),
}

/// One RPC request (Phase 1 verb set; later phases extend)
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Request {
    /// Liveness check
    Ping,
    /// Full flock listing
    ListFlock,
    /// Detailed info for matching sheep
    Describe {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Register + start apps
    Start {
        /// Validated app configs (client normalizes before sending)
        apps: Vec<AppConfig>,
    },
    /// Stop matching sheep (stay registered)
    Stop {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Restart matching sheep
    Restart {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Stop + deregister matching sheep
    Delete {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Graceful daemon shutdown
    KillDaemon,
    /// Subscribe this connection to bus topics (glob patterns)
    Subscribe {
        /// Topic globs, e.g. `process.*`
        topics: Vec<String>,
    },
}

/// Snapshot of one sheep for listings and events
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Stable numeric id
    pub id: u32,
    /// Sheep name
    pub name: String,
    /// Lifecycle status
    pub status: ProcStatus,
    /// OS pid while running
    pub pid: Option<u32>,
    /// Restart count since registration
    pub restarts: u32,
    /// Milliseconds since last successful start
    pub uptime_ms: u64,
    /// Fold membership
    pub fold: Option<String>,
}

/// One RPC response (pairs with [`Request`] variants)
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Response {
    /// Answer to `Ping`
    Pong,
    /// Answer to `ListFlock`
    Flock(Vec<ProcessInfo>),
    /// Answer to `Describe`
    Described(Vec<ProcessInfo>),
    /// Answer to `Start`
    Started(Vec<ProcessInfo>),
    /// Answer to `Stop`
    Stopped(Vec<ProcessInfo>),
    /// Answer to `Restart`
    Restarted(Vec<ProcessInfo>),
    /// Answer to `Delete` — ids removed
    Deleted(Vec<u32>),
    /// Answer to `Subscribe`
    Subscribed,
    /// Answer to `KillDaemon`
    ShuttingDown,
}

/// A request frame
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Per-connection request id
    pub id: u64,
    /// Client-imposed deadline (daemon aborts work past it)
    pub deadline_ms: Option<u64>,
    /// The request
    pub body: Request,
}

/// A reply frame
///
/// `result` uses serde's stock `Result` representation — the wire carries
/// `{"Ok": ...}` / `{"Err": ...}` (capitalized keys). Deliberate, pinned by
/// snapshot: stock serde beats a custom enum the client would convert anyway.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// Echoes [`Envelope::id`]
    pub id: u64,
    /// The outcome
    pub result: Result<Response, RpcError>,
}

/// Handshake outcome: `HelloAck` or a typed refusal (spec §6 —
/// version skew is an error, not silence). Same `Ok`/`Err` wire shape
/// as [`Reply::result`]; refusals use [`RpcErrorCode::ProtocolMismatch`].
pub type HelloReply = Result<HelloAck, RpcError>;

/// Structured RPC failure
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// Machine-readable code
    pub code: RpcErrorCode,
    /// Human-readable message (plain English, no theme)
    pub message: String,
}

/// Machine-readable RPC error codes
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RpcErrorCode {
    /// Selector matched nothing
    NotFound,
    /// Config failed validation daemon-side
    InvalidConfig,
    /// Spawn failed (exec error, permissions)
    SpawnFailed,
    /// Handshake protocol version mismatch
    ProtocolMismatch,
    /// Unexpected daemon-side failure
    Internal,
}
```

`protocol/events.rs`:

```rust
//! Bus events broadcast to subscribed clients

use serde::{Deserialize, Serialize};

use crate::protocol::request::ProcessInfo;

/// What happened to a sheep
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProcessEventKind {
    /// Spawn initiated
    Start,
    /// Became ready/online
    Online,
    /// Process exited
    Exit,
    /// Restart initiated
    Restart,
    /// Stopped by request
    Stop,
    /// Deregistered
    Delete,
    /// Restart budget exhausted
    Errored,
}

/// One event on the daemon bus
///
/// The serde tag (`event`) is structural; subscription TOPICS are the dotted
/// strings from [`BusEvent::topic`] (`process.exit`, `log.out`, `daemon.*` —
/// spec §6 grammar). Phase 2's server-side filter globs against `topic()`.
// wire format: changing existing variants is a breaking change
// NOTE (execution correction): internally-tagged `tag = "event"` cannot
// compile — Process's own `event` field collides with the internal tag
// (serde_derive error). Shipped shape is ADJACENTLY tagged, matching
// Response's convention:
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BusEvent {
    /// Lifecycle event for one sheep
    Process {
        /// What happened
        event: ProcessEventKind,
        /// Sheep snapshot at event time
        info: ProcessInfo,
        /// True when a user action caused it
        manually: bool,
        /// Unix millis
        at_ms: u64,
    },
    /// One stdout line from a sheep
    LogOut {
        /// Sheep id
        id: u32,
        /// The line (no trailing newline)
        line: String,
    },
    /// One stderr line from a sheep
    LogErr {
        /// Sheep id
        id: u32,
        /// The line
        line: String,
    },
    /// The bounded queue dropped this many events for this subscriber
    Dropped {
        /// Dropped-event count since last notice
        count: u64,
    },
    /// Daemon is shutting down
    DaemonShutdown,
}

impl BusEvent {
    /// The dotted subscription topic for this event (spec §6 grammar)
    #[must_use]
    pub fn topic(&self) -> &'static str {
        match self {
            Self::Process { event, .. } => match event {
                ProcessEventKind::Start => "process.start",
                ProcessEventKind::Online => "process.online",
                ProcessEventKind::Exit => "process.exit",
                ProcessEventKind::Restart => "process.restart",
                ProcessEventKind::Stop => "process.stop",
                ProcessEventKind::Delete => "process.delete",
                ProcessEventKind::Errored => "process.errored",
            },
            Self::LogOut { .. } => "log.out",
            Self::LogErr { .. } => "log.err",
            Self::Dropped { .. } => "daemon.dropped",
            Self::DaemonShutdown => "daemon.shutdown",
        }
    }
}
```

- [ ] **Step 4: Run tests, review + accept snapshots, run gates**

Run: `cargo test -p shep-core protocol`
Expected: snapshot tests fail on first run (no accepted snapshots). Review with `cargo insta review` (or inspect `crates/shep-core/src/protocol/snapshots/*.snap.new`), accept, re-run: PASS. Then all four gates.

- [ ] **Step 5: Commit (snapshots included)**

```bash
git add crates/shep-core/src/protocol/ crates/shep-core/src/lib.rs
git commit -m "feat(core): wire protocol v1 types with pinned snapshots

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 12: Wire framing codec

**Files:**
- Create: `crates/shep-core/src/protocol/wire.rs` (declared in Task 11's mod.rs)

**Interfaces:**
- Consumes: protocol types (Task 11).
- Produces: `pub fn encode_frame<T: Serialize>(value: &T) -> Result<bytes::BytesMut, WireError>` and `pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, WireError>`; `pub fn codec() -> tokio_util::codec::LengthDelimitedCodec` (u32 big-endian length prefix, max frame 16 MiB); `pub enum WireError { Json(String), FrameTooLarge(usize) }`. `MAX_FRAME_BYTES: usize = 16 * 1024 * 1024`. (Requires adding `bytes = { version = "1", default-features = false }` to workspace deps + shep-core — fold into Step 3.)

- [ ] **Step 1: Write failing tests**:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Envelope, Request};

    #[test]
    fn encode_decode_round_trip() {
        let env = Envelope { id: 9, deadline_ms: Some(5000), body: Request::Ping };
        let bytes = encode_frame(&env).unwrap();
        let back: Envelope = decode_frame(&bytes).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn decode_rejects_garbage_with_json_error() {
        assert!(matches!(
            decode_frame::<Envelope>(b"not json"),
            Err(WireError::Json(_))
        ));
    }

    #[test]
    fn codec_uses_u32_prefix_and_max_frame() {
        let c = codec();
        // 16 MiB cap per spec-adjacent sanity: a frame larger than this is a
        // protocol violation, not a legitimate message.
        assert_eq!(c.max_frame_length(), MAX_FRAME_BYTES);
    }

}
```

The async framed test needs `tokio` dev-features `["macros", "rt", "io-util"]` and `futures-util` for `SinkExt`/`StreamExt` — added in Step 3. Include it in the same test mod:

```rust
    #[tokio::test]
    async fn framed_stream_round_trip() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_util::codec::{FramedRead, FramedWrite};

        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut writer = FramedWrite::new(client, codec());
        let mut reader = FramedRead::new(server, codec());

        let env = Envelope { id: 1, deadline_ms: None, body: Request::ListFlock };
        writer.send(encode_frame(&env).unwrap().freeze()).await.unwrap();

        let frame = reader.next().await.unwrap().unwrap();
        let back: Envelope = decode_frame(&frame).unwrap();
        assert_eq!(back, env);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p shep-core wire`
Expected: COMPILE ERROR.

- [ ] **Step 3: Implement.** Add to workspace deps: `bytes = { version = "1", default-features = false }`, `futures-util = { version = "0.3", default-features = false, features = ["sink", "std"] }` (`SinkExt` is gated behind the non-default `sink` feature). shep-core: `bytes.workspace = true` in `[dependencies]`; `futures-util.workspace = true` + tokio features `["macros", "rt", "io-util"]` in `[dev-dependencies]`. Add `pub mod wire;` (with a `///` doc: `/// Frame encoding shared by daemon and client`) to `protocol/mod.rs` and `pub use wire::{codec, decode_frame, encode_frame, WireError};` to its re-exports. Then:

```rust
//! Frame encoding: u32 length prefix + JSON payload
//!
//! One codec constructor + encode/decode helpers shared by daemon and
//! client so framing parameters can never drift between the two.

use core::fmt;

use bytes::BytesMut;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_util::codec::LengthDelimitedCodec;

/// Hard ceiling per frame; larger is a protocol violation
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Builds the shared length-delimited codec (u32 BE prefix, 16 MiB cap)
#[must_use]
pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_type::<u32>()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

/// Serializes one value to a frame payload
///
/// # Errors
///
/// - [`WireError::Json`] — serialization failed (carries serde's message).
/// - [`WireError::FrameTooLarge`] — payload exceeds [`MAX_FRAME_BYTES`].
pub fn encode_frame<T: Serialize>(value: &T) -> Result<BytesMut, WireError> {
    let vec = serde_json::to_vec(value).map_err(|e| WireError::Json(e.to_string()))?;
    if vec.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge(vec.len()));
    }
    Ok(BytesMut::from(vec.as_slice()))
}

/// Deserializes one frame payload
///
/// # Errors
///
/// - [`WireError::Json`] — the payload is not valid JSON for `T`.
pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, WireError> {
    serde_json::from_slice(frame).map_err(|e| WireError::Json(e.to_string()))
}

/// Error type returned from [`encode_frame`] and [`decode_frame`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// JSON (de)serialization failed (carries the serde message)
    Json(String),
    /// Encoded payload exceeds [`MAX_FRAME_BYTES`] (carries actual size)
    FrameTooLarge(usize),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(m) => write!(f, "wire frame JSON error: {m}"),
            Self::FrameTooLarge(n) => {
                write!(f, "frame of {n} bytes exceeds the {MAX_FRAME_BYTES}-byte limit")
            }
        }
    }
}

impl core::error::Error for WireError {}
```

- [ ] **Step 4: Run tests + gates**

Run: `cargo test -p shep-core wire && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/shep-core/
git commit -m "feat(core): length-delimited JSON wire codec with 16MiB frame cap

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 13: lib.rs polish — crate docs, lint header, prelude

**Files:**
- Modify: `crates/shep-core/src/lib.rs` (full replacement)

**Interfaces:**
- Produces: crate-level docs + doctest gate + `pub mod prelude` re-exporting `AppConfig, Flockfile, MemSize, UpDuration, ProcStatus, ProcessSelector, ShepPaths` with `#[doc(no_inline)]`.

- [ ] **Step 1: Replace lib.rs**:

```rust
//! Shared foundation of the shep workspace
//!
//! Typed configuration (Flockfile + daemon config), value newtypes,
//! process selectors, `$SHEP_HOME` paths, and wire protocol version 1.
//! Every other crate depends on this one; it depends on no sibling.
//!
//! # Quick start
//! ```
//! use shep_core::prelude::*;
//!
//! let app: AppConfig = toml::from_str("name = \"web\"\nscript = \"./srv\"").unwrap();
//! assert!(app.autorestart);
//! let limit: MemSize = "512M".parse().unwrap();
//! assert_eq!(limit.bytes(), 512 << 20);
//! ```
//!
//! Module-by-module design: `docs/systematic-refactor/refactor-workspace/map.md`;
//! behavior contract: `docs/specs/shep-v1.md`.

#![doc(test(attr(deny(warnings))))]
#![forbid(unsafe_code)]

pub mod config;
pub mod paths;
pub mod protocol;
pub mod selector;
pub mod status;
pub mod values;

/// One-import surface for downstream crates
pub mod prelude {
    #[doc(no_inline)]
    pub use crate::config::{AppConfig, Flockfile};
    #[doc(no_inline)]
    pub use crate::paths::ShepPaths;
    #[doc(no_inline)]
    pub use crate::selector::ProcessSelector;
    #[doc(no_inline)]
    pub use crate::status::ProcStatus;
    #[doc(no_inline)]
    pub use crate::values::{MemSize, UpDuration};
}
```

(`toml` must be a dev-dependency-visible doctest dep — it already is a real dependency, so the doctest compiles.)

- [ ] **Step 2: Run the four gates**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo check --workspace && cargo test --workspace`
Expected: all green — doctests included.

- [ ] **Step 3: Commit**

```bash
git add crates/shep-core/src/lib.rs
git commit -m "feat(core): crate docs, unsafe-forbid, prelude

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Phase 1 exit criteria

- All 13 tasks committed; four gates green on the workspace.
- `cargo test -p shep-core` covers: value grammars (strict), config defaults + round-trips, Flockfile formats + discovery, daemon config layering, selector rules, wire snapshots + v1 fixture, framed round-trip.
- Insta snapshots committed under `crates/shep-core/src/protocol/snapshots/`.
- CI workflow live (push to see it run — first push to a remote happens whenever the maintainer wires one).
- Handoff: Phase 2 plan (daemon core) gets written against these exact interfaces.
