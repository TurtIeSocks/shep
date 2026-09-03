//! The timestamp every line in a sheep's or a dog's log file carries, and
//! how a reader takes it back off.
//!
//! # Why this is shared rather than the daemon's own
//!
//! It is a contract between one writer and several readers. The daemon
//! writes the stamp; `shep bleats --no-follow` reads it back off a file, and
//! so does `shep lookout`'s tail pane, and so does the `whistle` tool a
//! model calls. A format defined in the daemon and re-spelled in each reader
//! is a format that drifts, and the failure would be silent: a reader whose
//! idea of the width is one character out returns lines with a stray digit
//! at the front and nothing raises an error.
//!
//! # Why the stamp is on the file and not on the line
//!
//! A log file that carries no time is a file an operator cannot date. The
//! incident this exists for was a 341 KB log of byte-identical lines whose
//! newest entry was two days old, read by someone who had no way to see that
//! and who spent two days acting on it as if it were live. `mtime` answers
//! only for the whole file, and only until something touches it — shep's own
//! rotation touches it — so the answer has to live on the line.
//!
//! What it must NOT do is change what a sheep is reported to have said.
//! `Bus::publish_log` carries a sheep's line verbatim, so `shep bleats
//! --follow` and every dog subscribed to `log.*` see the sheep's own bytes;
//! [`strip`] is what keeps the file readers agreeing with them. `line` in
//! `bleats --format json` therefore means the same thing on both paths, and
//! means what it always did.

use core::fmt::Write as _;

/// The `strftime` spelling of the stamp: `2026-09-02T14:22:31.412+02:00`.
///
/// **Local time**, because the reader is a person looking at their own
/// clock — the same call `shep list`'s table already makes for the
/// timestamps it prints. **With the offset**, because that is what keeps
/// local time from being a lie: a bare one is unreadable across a DST
/// boundary and unusable to anyone correlating this file against a UTC one,
/// and printing it costs six characters. **RFC 3339**, because it sorts
/// lexicographically within one offset and every log tool already parses it.
/// **Milliseconds**, because a dog's handshake round trip is measured in
/// them, so a second-resolution stamp would put a spawn, its handshake and
/// its refusal all at the same instant.
pub const LOG_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";

/// How many bytes [`stamp_into`] writes — the stamp and the single space
/// that separates it from the line.
///
/// Fixed rather than approximate, and that is a property of
/// [`LOG_TIMESTAMP_FORMAT`] rather than a hope: `%:z` renders `+HH:MM` for
/// every offset chrono can produce (it truncates the sub-minute offsets a
/// handful of pre-1900 zones carry), and every other field in that format is
/// zero-padded to a fixed width. 10 date + 1 `T` + 8 time + 4 `.mmm` + 6
/// offset + 1 space.
pub const LOG_STAMP_BYTES: usize = 30;

/// Appends the current local time in [`LOG_TIMESTAMP_FORMAT`], plus the
/// separating space, to `buf`.
///
/// Takes a buffer rather than returning a `String` because the caller on the
/// hot path runs once per logged line and can reuse one allocation for the
/// life of a log file — a sheep emitting 1.6M lines a second is a workload
/// shep's log pump has actually been measured against.
///
/// # Panics
///
/// Debug builds only, and only if [`LOG_TIMESTAMP_FORMAT`] is edited into a
/// width [`LOG_STAMP_BYTES`] no longer describes. Every reader strips the
/// prefix by that count, so an edit that changed it would otherwise be found
/// by a reader's mangled output rather than by a test run.
#[track_caller]
pub fn stamp_into(buf: &mut String) {
    let start = buf.len();
    // Infallible: the only way `write!` to a `String` fails is a `Display`
    // impl that itself errors, and chrono's `DelayedFormat` only does that
    // for a format string it could not parse — which this one is a `const`
    // to keep from ever being.
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
/// The stamp is RECOGNISED rather than assumed, by parsing the first
/// [`LOG_STAMP_BYTES`] as RFC 3339. Blindly cutting a fixed prefix would be
/// cheaper and is wrong twice over: a log file predating this format, or one
/// an operator's own tooling appended to, would lose the first 30 characters
/// of every line — and a rotated archive holding both is a file readers have
/// to handle, since nothing rewrites what is already on disk.
///
/// Cheap on the common path: the shape test rejects a line that is too short
/// or has no space where the separator belongs before any parsing happens.
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

    /// Fails if a stamped line does not survive a round trip through the
    /// writer and the reader.
    ///
    /// The two halves are the contract, and they live in one file precisely
    /// so this test can exist: a width that drifts from the format breaks
    /// every reader at once and nothing else would say so.
    #[test]
    fn a_stamped_line_comes_back_the_way_it_went_in() {
        let mut written = String::new();
        stamp_into(&mut written);
        written.push_str("the sheep said this");

        assert_eq!(written.len(), LOG_STAMP_BYTES + "the sheep said this".len());
        assert_eq!(strip(&written), "the sheep said this");
    }

    /// Fails if a reader eats the front of a line that carries no stamp.
    ///
    /// Rotated archives written before this format existed are files a
    /// reader still has to handle, and so is anything an operator's own
    /// tooling appended. A fixed-width cut would take 30 characters off each
    /// of these and report the remainder as the whole line.
    #[test]
    fn an_unstamped_line_is_left_exactly_as_it_is() {
        for line in [
            "",
            "short",
            "an old line from before shep stamped anything at all",
            // Long enough to reach the width, and a space in the right
            // place — everything but a real timestamp.
            "2026-99-99T99:99:99.999+99:99 nonsense in the shape of a stamp",
            "############################# looks like a prefix, parses as nothing",
        ] {
            assert_eq!(strip(line), line, "{line:?} carries no stamp to strip");
        }
    }

    /// Fails if a multi-byte character at the cut point panics the reader.
    ///
    /// A log file is whatever the child wrote. `split_at_checked` answers
    /// `None` rather than panicking on a non-boundary index, which is the
    /// whole reason it is used here instead of `split_at`.
    #[test]
    fn a_line_split_mid_character_is_returned_whole() {
        let line = format!("{}x", "é".repeat(LOG_STAMP_BYTES));
        assert_eq!(strip(&line), line);
    }
}
