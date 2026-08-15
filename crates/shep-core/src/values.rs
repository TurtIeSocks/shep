//! Config value newtypes: memory sizes and durations

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
        let value: u64 = digits.parse().map_err(|_| ParseMemSizeError::Overflow)?;
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
///
/// `#[non_exhaustive]`: today's four variants exhaust one frozen grammar, but
/// a future revision of it — fractional sizes (`1.5G`), or a binary-vs-decimal
/// unit distinction — would want its own variant rather than folding into
/// [`Self::InvalidCharacter`]'s catch-all, and shep-core is a published
/// library an out-of-tree matcher should not break for the day that lands
/// (IR-20).
#[non_exhaustive]
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

/// String-shaped, matching this type's `Serialize`/`Deserialize`, which go
/// through `Display`/`FromStr` rather than the wrapped `u64`. A derive here
/// would emit `{"type":"integer"}` and describe a wire form that does not
/// exist.
///
/// The pattern is `FromStr`'s own grammar, lifted from its doc comment
/// above. If you change one, change the other — the paired test below is
/// what catches it.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for MemSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MemSize".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": r"^\d+(G|M|K)?$",
            "description": "A byte quantity: digits, optionally suffixed G, M or K (binary units).",
        })
    }
}

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
        // Sound: every constructor bounds millis to u64 (`from_millis`
        // stores its u64 argument directly; `FromStr` reaches this type
        // only through a `checked_mul` that already fits in u64). Revisit
        // if a constructor from a raw `Duration` is ever added — that could
        // carry more than u64::MAX milliseconds.
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
        let value: u64 = digits.parse().map_err(|_| ParseUpDurationError::Overflow)?;
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
///
/// `#[non_exhaustive]`, for the same reason as
/// [`ParseMemSizeError`]: a future grammar
/// revision — fractional durations, or a `d`/`w` unit — would want its own
/// variant rather than folding into [`Self::InvalidCharacter`]'s catch-all,
/// and shep-core is a published library an out-of-tree matcher should not
/// break for the day that lands (IR-20).
#[non_exhaustive]
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

/// String-shaped, matching this type's `Serialize`/`Deserialize` — see
/// [`MemSize`]'s own `JsonSchema` impl for the full reasoning, which applies
/// here unchanged.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for UpDuration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UpDuration".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": r"^\d+(h|m|s)?$",
            "description": "A duration: digits, optionally suffixed h, m or s. Plain digits are milliseconds.",
        })
    }
}

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
        for bytes in [
            0u64,
            1,
            1023,
            1024,
            1536,
            1 << 20,
            (1 << 30) + 1024,
            u64::MAX,
        ] {
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

    /// fails if the schema pattern and `FromStr` disagree. A pattern that is
    /// merely self-consistent is worthless; it has to agree with the parser
    /// the schema claims to describe. The reject list carries `512T` and
    /// `1P` for a specific reason: a widened suffix set is the way this
    /// pattern most plausibly goes wrong, and a reject list without a
    /// would-be-accepted suffix cannot catch it.
    #[cfg(feature = "schema")]
    #[test]
    fn the_schema_pattern_agrees_with_from_str() {
        let schema = serde_json::to_value(schemars::schema_for!(MemSize)).unwrap();
        let pattern = schema["pattern"].as_str().unwrap();
        let re = regex::Regex::new(pattern).unwrap();
        for accepted in ["512M", "1G", "4096", "7K"] {
            assert!(re.is_match(accepted), "pattern rejects {accepted}");
            assert!(
                accepted.parse::<MemSize>().is_ok(),
                "FromStr rejects {accepted}"
            );
        }
        for rejected in ["512MB", "512m", "1.5G", "", "M", "512T", "1P", "512g"] {
            assert!(!re.is_match(rejected), "pattern accepts {rejected}");
            assert!(
                rejected.parse::<MemSize>().is_err(),
                "FromStr accepts {rejected}"
            );
        }
    }
}

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
        // Digit string itself overflows u64 before any unit multiplication.
        assert_eq!("99999999999999999999h".parse::<UpDuration>(), Err(Overflow));
        // Digit string fits u64 on its own, but overflows on the ×3_600_000
        // (hours-to-ms) multiplication.
        assert_eq!("9999999999999999h".parse::<UpDuration>(), Err(Overflow));
    }

    #[test]
    fn display_round_trips() {
        for ms in [
            0u64, 1, 999, 1000, 1600, 30_000, 300_000, 7_200_000, 3_601_000,
        ] {
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

    /// fails if the schema pattern and `FromStr` disagree — see the
    /// `MemSize` version of this same test, above, for the full reasoning
    /// behind the reject list's shape.
    #[cfg(feature = "schema")]
    #[test]
    fn the_schema_pattern_agrees_with_from_str() {
        let schema = serde_json::to_value(schemars::schema_for!(UpDuration)).unwrap();
        let pattern = schema["pattern"].as_str().unwrap();
        let re = regex::Regex::new(pattern).unwrap();
        for accepted in ["1600", "30s", "5m", "2h"] {
            assert!(re.is_match(accepted), "pattern rejects {accepted}");
            assert!(
                accepted.parse::<UpDuration>().is_ok(),
                "FromStr rejects {accepted}"
            );
        }
        for rejected in ["30S", "1.5s", "30 s", "", "s", "30d", "30w"] {
            assert!(!re.is_match(rejected), "pattern accepts {rejected}");
            assert!(
                rejected.parse::<UpDuration>().is_err(),
                "FromStr accepts {rejected}"
            );
        }
    }
}
