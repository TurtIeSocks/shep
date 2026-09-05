//! The timestamp every line in a sheep's or a dog's log file carries, and
//! how a reader takes it back off.
//!
//! Shared between the daemon, which writes the stamp, and every reader:
//! `shep bleats --no-follow`, `shep lookout`'s tail pane, and the `whistle`
//! tool. The stamp lives on the line rather than the file's `mtime`, which
//! answers only for the whole file and stops answering once anything
//! touches it, log rotation included.
//!
//! [`strip`] never changes what a sheep is reported to have said:
//! `Bus::publish_log` carries its line verbatim, so a stripped line matches
//! what `--follow` and `log.*` subscribers already saw.

use core::fmt::Write as _;

/// The `strftime` spelling of the stamp: `2026-09-02T14:22:31.412+02:00`.
///
/// Local time with the UTC offset, RFC 3339, to the millisecond: the offset
/// keeps local time unambiguous across a DST boundary, RFC 3339 sorts
/// lexicographically within one offset, and milliseconds resolve a dog's
/// handshake round trip.
pub const LOG_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";

/// How many bytes [`stamp_into`] writes: the stamp plus the separating space.
///
/// Fixed because every field in [`LOG_TIMESTAMP_FORMAT`] is zero-padded, and
/// `%:z` always renders `+HH:MM` (chrono truncates rarer sub-minute
/// offsets). 10 date + 1 `T` + 8 time + 4 `.mmm` + 6 offset + 1 space.
pub const LOG_STAMP_BYTES: usize = 30;

/// Appends the current local time in [`LOG_TIMESTAMP_FORMAT`], plus the
/// separating space, to `buf`.
///
/// Takes a buffer rather than returning a `String`: the caller runs this
/// once per logged line and reuses one allocation for the life of a log
/// file.
///
/// # Panics
///
/// Debug builds only, when [`LOG_TIMESTAMP_FORMAT`]'s width diverges from
/// [`LOG_STAMP_BYTES`].
#[track_caller]
pub fn stamp_into(buf: &mut String) {
    let start = buf.len();
    // Infallible: `write!` to a `String` only errors if `Display` does, and
    // `DelayedFormat` only errors on an unparsable format string, which this
    // being a `const` rules out.
    let _ = write!(
        buf,
        "{} ",
        chrono::Local::now().format(LOG_TIMESTAMP_FORMAT)
    );
    debug_assert_eq!(
        buf.len() - start,
        LOG_STAMP_BYTES,
        "the stamp's width is fixed and readers strip it by count"
    );
}

/// `line` with its stamp removed, or `line` unchanged if it does not carry
/// one.
///
/// Recognises the stamp by parsing the first [`LOG_STAMP_BYTES`] as RFC
/// 3339, rather than blindly cutting a fixed prefix: a line predating this
/// format, or one appended by other tooling, keeps its first 30 characters
/// instead of losing them.
///
/// Cheap on the common path: a line too short or missing the separator
/// space is rejected before parsing runs.
#[must_use]
pub fn strip(line: &str) -> &str {
    let Some((stamp, rest)) = line.split_at_checked(LOG_STAMP_BYTES) else {
        return line;
    };
    let Some(stamp) = stamp.strip_suffix(' ') else {
        return line;
    };
    if chrono::DateTime::parse_from_rfc3339(stamp).is_ok() {
        rest
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamped_line_comes_back_the_way_it_went_in() {
        let mut written = String::new();
        stamp_into(&mut written);
        written.push_str("the sheep said this");

        assert_eq!(written.len(), LOG_STAMP_BYTES + "the sheep said this".len());
        assert_eq!(strip(&written), "the sheep said this");
    }

    /// Covers a line from before this format existed and one appended by
    /// other tooling.
    #[test]
    fn an_unstamped_line_is_left_exactly_as_it_is() {
        for line in [
            "",
            "short",
            "an old line from before shep stamped anything at all",
            // Long enough to reach the width, with a space in the right
            // place, but not a real timestamp.
            "2026-99-99T99:99:99.999+99:99 nonsense in the shape of a stamp",
            "############################# looks like a prefix, parses as nothing",
        ] {
            assert_eq!(strip(line), line, "{line:?} carries no stamp to strip");
        }
    }

    /// `split_at_checked` returns `None` rather than panicking on a
    /// non-boundary index; `split_at` would panic here.
    #[test]
    fn a_line_split_mid_character_is_returned_whole() {
        let line = format!("{}x", "é".repeat(LOG_STAMP_BYTES));
        assert_eq!(strip(&line), line);
    }
}
