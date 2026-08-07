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
}
