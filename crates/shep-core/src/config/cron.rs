//! `cron_restart` schedule parsing: the croner-backed cron grammar (spec §4).
//!
//! Five-field standard cron only. croner still accepts `L`, `W`, `#` and
//! `?` natively; rejecting them is this module's job, done by a
//! token-aware pre-parse scan, since a character scan alone would reject
//! `JUL` and `WED` (both contain a reserved letter).
//!
//! The seven vixie `@nickname` shorthands are expanded to five fields
//! before croner ever sees them: its own nickname table has no
//! `@midnight` arm, so delegating would accept `@daily` and reject
//! `@midnight`.

use core::fmt;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::errors::CronError;
use croner::parser::{CronParser, Seconds};

/// The vixie nickname table, in the order spec §4 lists them. Matching is
/// ASCII-case-insensitive; `@yearly` and `@annually` are two spellings of
/// the same schedule, as are `@daily` and `@midnight`.
const NICKNAMES: [(&str, &str); 7] = [
    ("@yearly", "0 0 1 1 *"),
    ("@annually", "0 0 1 1 *"),
    ("@monthly", "0 0 1 * *"),
    ("@weekly", "0 0 * * 0"),
    ("@daily", "0 0 * * *"),
    ("@midnight", "0 0 * * *"),
    ("@hourly", "0 * * * *"),
];

/// Three-letter month and weekday names croner's alpha replacement accepts.
/// The extension-character scan below must treat these as opaque tokens:
/// `JUL` contains `L`, `WED` contains `W`.
const NAMES: [&str; 19] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC", "SUN",
    "MON", "TUE", "WED", "THU", "FRI", "SAT",
];

/// A validated `cron_restart` pattern together with the zone it is read in.
///
/// The croner and chrono-tz types are private: a cron dialect is a Flockfile
/// grammar promise, and pinning it to a dependency's public types would make
/// that dependency's next major version a breaking change to shep's own
/// config surface.
// wire format: the accepted pattern grammar is a config contract; widening or
// narrowing it is a breaking change
#[derive(Debug, Clone)]
pub struct CronSchedule {
    /// The pattern exactly as the caller wrote it, including a nickname
    /// spelling, never croner's normalized `Cron::as_str` rendering.
    pattern: String,
    zone: Tz,
    cron: Cron,
}

impl CronSchedule {
    /// Parses a `cron_restart` pattern and its optional `cron_timezone`.
    ///
    /// # Errors
    ///
    /// - [`CronParseError::Pattern`]: croner rejected the pattern.
    /// - [`CronParseError::Timezone`]: the name is not an IANA zone.
    pub fn parse(pattern: &str, timezone: Option<&str>) -> Result<Self, CronParseError> {
        let zone = match timezone {
            Some(name) => parse_timezone_name(name).ok_or_else(|| CronParseError::Timezone {
                name: name.to_string(),
            })?,
            None => Tz::UTC,
        };

        let trimmed = pattern.trim();
        let candidate = if is_single_at_token(trimmed) {
            expand_nickname(trimmed, pattern)?
        } else {
            trimmed.to_string()
        };
        reject_extension_characters(&candidate, pattern)?;

        let cron = cron_parser()
            .parse(&candidate)
            .map_err(|e| CronParseError::Pattern {
                pattern: pattern.to_string(),
                reason: e.to_string(),
            })?;

        Ok(Self {
            pattern: pattern.to_string(),
            zone,
            cron,
        })
    }

    /// The first occurrence strictly after `after`, in UTC.
    ///
    /// Returns `None` when the pattern can never match again, like `0 0 30 2 *`
    /// (30 February). A DST fall-back hour can return the same wall-clock
    /// occurrence twice across two calls, croner's own semantics.
    ///
    /// # Errors
    /// - [`CronScheduleError::Search`]: the search failed for a reason other than exhaustion.
    ///
    /// # Panics
    /// If converting `after` into `zone`'s calendar falls outside what
    /// `NaiveDateTime` can represent. Unreachable from `Utc::now()`.
    pub fn next_after(
        &self,
        after: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, CronScheduleError> {
        let start = after.with_timezone(&self.zone);
        match self.cron.find_next_occurrence(&start, false) {
            Ok(dt) => Ok(Some(dt.with_timezone(&Utc))),
            Err(CronError::TimeSearchLimitExceeded) => Ok(None),
            Err(e) => Err(CronScheduleError::Search {
                reason: e.to_string(),
            }),
        }
    }

    /// The pattern as written in the Flockfile.
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// Growth is expected: croner's dialect has more rejection modes than this
/// enum distinguishes today, and a future `cron_timezone` shorthand would
/// add one more.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronParseError {
    /// The pattern is not valid in shep's dialect. Carries the pattern as the
    /// user wrote it and the rendered reason: croner's own sentence where
    /// croner did the rejecting, ours where the pre-parse pass did.
    Pattern {
        /// The pattern as the user wrote it
        pattern: String,
        /// Why it was rejected
        reason: String,
    },
    /// The `cron_timezone` value is not a name in the IANA database.
    Timezone {
        /// The value as the user wrote it
        name: String,
    },
}

impl fmt::Display for CronParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pattern { pattern, reason } => {
                write!(f, "invalid cron_restart pattern `{pattern}`: {reason}")
            }
            Self::Timezone { name } => write!(f, "`{name}` is not a recognized IANA timezone"),
        }
    }
}

impl core::error::Error for CronParseError {}

/// Why a validated schedule could not produce its next occurrence.
///
/// One variant today and no `#[non_exhaustive]`: the only failure a search can
/// have that is not exhaustion is croner's own, and a second variant would be
/// a second reason, not a second rendering of this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronScheduleError {
    /// croner could not resolve the next occurrence; carries its rendered reason.
    Search {
        /// croner's own rendered reason
        reason: String,
    },
}

impl fmt::Display for CronScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search { reason } => write!(f, "cron schedule search failed: {reason}"),
        }
    }
}

impl core::error::Error for CronScheduleError {}

/// Builds the five-field-only parser. Not a `const`/`static`:
/// `CronParserBuilder::build` is not a `const fn`, and the builder itself is
/// cheap enough (no allocation) that building fresh per call costs nothing
/// measurable at Flockfile-parse rates.
///
/// `Seconds::Disallowed` is the one load-bearing call here: croner's own
/// default is `Seconds::Optional`, which accepts a six-field pattern and
/// would ship the wide dialect by accident. No `.dom_and_dow(true)` call:
/// croner's default is OR semantics between day-of-month and day-of-week,
/// which is the dialect map.md promises; `true` would switch to AND and
/// silently change what an existing pattern means.
fn cron_parser() -> CronParser {
    CronParser::builder().seconds(Seconds::Disallowed).build()
}

/// Parses an IANA timezone name. Shared by [`CronSchedule::parse`] and
/// `normalize`'s standalone `cron_timezone` check: a Flockfile may carry a
/// timezone with no `cron_restart` to pair it with, and spec §5 says that
/// typo fails loudly too.
pub(super) fn parse_timezone_name(name: &str) -> Option<Tz> {
    name.parse::<Tz>().ok()
}

/// True when `trimmed` is exactly one whitespace-free token starting with
/// `@`, the only shape nickname expansion applies to. A multi-token
/// pattern containing `@` is left alone; croner rejects it on its own
/// terms.
///
/// The `split_whitespace().count() == 1` clause has no mutation test: a
/// multi-token `@`-leading pattern ends in the same error either way, just
/// with a different message, so weakening the clause changes which
/// message fires, not whether the pattern is accepted.
fn is_single_at_token(trimmed: &str) -> bool {
    trimmed.starts_with('@') && trimmed.split_whitespace().count() == 1
}

/// Expands a single-token `@`-pattern against the closed vixie table.
/// `@reboot` and anything unrecognized are rejected here, with a message
/// naming the reason, never handed to croner, whose own rejection would
/// read as a field-count complaint that says nothing about nicknames.
fn expand_nickname(trimmed: &str, original: &str) -> Result<String, CronParseError> {
    if trimmed.eq_ignore_ascii_case("@reboot") {
        // Just the reason: `CronParseError::Pattern`'s Display already
        // renders `invalid cron_restart pattern `@reboot`:` ahead of this.
        return Err(CronParseError::Pattern {
            pattern: original.to_string(),
            reason: "shep's own restart policy already decides when a sheep starts".to_string(),
        });
    }
    for (name, expansion) in NICKNAMES {
        if trimmed.eq_ignore_ascii_case(name) {
            return Ok(expansion.to_string());
        }
    }
    Err(CronParseError::Pattern {
        pattern: original.to_string(),
        reason: format!(
            "`{trimmed}` is not a recognized cron_restart nickname (expected one of @yearly, \
             @annually, @monthly, @weekly, @daily, @midnight, @hourly)"
        ),
    })
}

/// Rejects croner's `L`, `W`, `#` and `?` extensions before the pattern
/// reaches croner, which accepts all four natively. Scans token-aware, per
/// whitespace-separated field, treating a recognized three-letter month or
/// weekday name as opaque first: a character-wise scan would reject `JUL`
/// and `WED`, which are valid standard cron.
fn reject_extension_characters(candidate: &str, original: &str) -> Result<(), CronParseError> {
    for field in candidate.split_whitespace() {
        if let Some(bad) = field_has_bad_char(field) {
            return Err(CronParseError::Pattern {
                pattern: original.to_string(),
                reason: format!(
                    "cron_restart pattern contains `{bad}`, a croner extension character \
                     shep's five-field dialect does not accept"
                ),
            });
        }
    }
    Ok(())
}

/// Scans one field for `L`/`W`/`#`/`?`, skipping over any three-character
/// window that case-insensitively matches a recognized month/weekday name.
fn field_has_bad_char(field: &str) -> Option<char> {
    let chars: Vec<char> = field.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 3 <= chars.len() {
            let window: String = chars[i..i + 3].iter().collect();
            if NAMES.iter().any(|name| name.eq_ignore_ascii_case(&window)) {
                i += 3;
                continue;
            }
        }
        if matches!(chars[i].to_ascii_uppercase(), 'L' | 'W' | '#' | '?') {
            return Some(chars[i]);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid RFC3339 timestamp")
    }

    /// Chains `n` successive calls to `next_after`, each starting strictly
    /// after the previous result.
    fn occurrence_sequence(
        schedule: &CronSchedule,
        start: DateTime<Utc>,
        n: usize,
    ) -> Vec<DateTime<Utc>> {
        let mut cursor = start;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let next = schedule
                .next_after(cursor)
                .expect("search succeeds")
                .expect("has a next occurrence");
            out.push(next);
            cursor = next;
        }
        out
    }

    fn assert_extension_char_rejected(pattern: &str, bad: char) {
        match CronSchedule::parse(pattern, None) {
            Err(CronParseError::Pattern {
                pattern: got_pattern,
                reason,
            }) => {
                assert_eq!(got_pattern, pattern);
                assert_eq!(
                    reason,
                    format!(
                        "cron_restart pattern contains `{bad}`, a croner extension character \
                         shep's five-field dialect does not accept"
                    )
                );
            }
            other => panic!("expected Pattern error, got {other:?}"),
        }
    }

    #[test]
    fn five_field_pattern_produces_pinned_occurrence_sequence() {
        // fails if the parser is configured with `Seconds::Required`, which
        // would reject this five-field pattern outright
        let schedule = CronSchedule::parse("0 3 * * *", None).unwrap();
        let seq = occurrence_sequence(&schedule, dt("2026-01-01T00:00:00Z"), 3);
        assert_eq!(
            seq,
            vec![
                dt("2026-01-01T03:00:00Z"),
                dt("2026-01-02T03:00:00Z"),
                dt("2026-01-03T03:00:00Z"),
            ]
        );
    }

    #[test]
    fn six_field_pattern_is_rejected() {
        // fails if the builder was left on croner's default
        // `Seconds::Optional`, which accepts the seconds field and ships the
        // wide dialect by accident
        match CronSchedule::parse("30 0 3 * * *", None) {
            Err(CronParseError::Pattern { pattern, .. }) => assert_eq!(pattern, "30 0 3 * * *"),
            other => panic!("expected Pattern error, got {other:?}"),
        }
    }

    #[test]
    fn year_field_pattern_is_rejected() {
        // fails if `.seconds(Seconds::Disallowed)` was "simplified away" on
        // the theory that a `.year(...)` call was also needed: one setting
        // closes both widenings
        match CronSchedule::parse("0 3 * * * 2027", None) {
            Err(CronParseError::Pattern { pattern, .. }) => assert_eq!(pattern, "0 3 * * * 2027"),
            other => panic!("expected Pattern error, got {other:?}"),
        }
    }

    #[test]
    fn nicknames_expand_to_the_same_occurrence_sequence_as_their_five_field_form() {
        // Transcribed by hand from spec §4, not read from `NICKNAMES`:
        // comparing a table against a copy of itself would pass even if
        // both were wrong.
        let anchor = dt("2026-01-01T00:00:00Z");
        let expected_expansions: [(&str, &str); 7] = [
            ("@yearly", "0 0 1 1 *"),
            ("@annually", "0 0 1 1 *"),
            ("@monthly", "0 0 1 * *"),
            ("@weekly", "0 0 * * 0"),
            ("@daily", "0 0 * * *"),
            ("@midnight", "0 0 * * *"),
            ("@hourly", "0 * * * *"),
        ];
        for (nickname, five_field) in expected_expansions {
            let via_nickname = CronSchedule::parse(nickname, None).unwrap();
            let via_five_field = CronSchedule::parse(five_field, None).unwrap();
            assert_eq!(
                occurrence_sequence(&via_nickname, anchor, 3),
                occurrence_sequence(&via_five_field, anchor, 3),
                "{nickname} vs {five_field}"
            );
        }
    }

    #[test]
    fn nickname_matching_is_ascii_case_insensitive() {
        // fails if the table is matched with `==` rather than an
        // ASCII-case-insensitive compare, which would turn `@DAILY` into an
        // unrecognized nickname
        let anchor = dt("2026-01-01T00:00:00Z");
        let upper = CronSchedule::parse("@DAILY", None).unwrap();
        let lower = CronSchedule::parse("@daily", None).unwrap();
        assert_eq!(
            occurrence_sequence(&upper, anchor, 3),
            occurrence_sequence(&lower, anchor, 3)
        );
    }

    #[test]
    fn nickname_pattern_keeps_its_own_spelling() {
        // fails if the expansion is stored in place of the user's own text,
        // the same defect `Cron::as_str` has for the five-field form
        let schedule = CronSchedule::parse("@daily", None).unwrap();
        assert_eq!(schedule.pattern(), "@daily");
    }

    #[test]
    fn reboot_nickname_is_rejected_with_its_own_message() {
        // fails if `@reboot` handling is a permissive "leading `@`, not
        // obviously malformed" check rather than a closed table
        match CronSchedule::parse("@reboot", None) {
            Err(CronParseError::Pattern { pattern, reason }) => {
                assert_eq!(pattern, "@reboot");
                assert_eq!(
                    reason,
                    "shep's own restart policy already decides when a sheep starts"
                );
            }
            other => panic!("expected Pattern error, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_nickname_is_rejected_without_reaching_croner() {
        // fails if an unrecognized `@`-token is handed to croner anyway,
        // which rejects it with a field-count sentence that says nothing
        // about nicknames
        match CronSchedule::parse("@fortnightly", None) {
            Err(CronParseError::Pattern { pattern, reason }) => {
                assert_eq!(pattern, "@fortnightly");
                assert_eq!(
                    reason,
                    "`@fortnightly` is not a recognized cron_restart nickname (expected one of \
                     @yearly, @annually, @monthly, @weekly, @daily, @midnight, @hourly)"
                );
            }
            other => panic!("expected Pattern error, got {other:?}"),
        }
    }

    #[test]
    fn zone_offset_is_applied_before_searching() {
        // fails if `next_after` ignores the zone and returns 03:00 UTC
        // directly instead of converting through Europe/Oslo's UTC+1 winter
        // offset
        let schedule = CronSchedule::parse("0 3 * * *", Some("Europe/Oslo")).unwrap();
        let seq = occurrence_sequence(&schedule, dt("2026-01-05T00:00:00Z"), 3);
        assert_eq!(
            seq,
            vec![
                dt("2026-01-05T02:00:00Z"),
                dt("2026-01-06T02:00:00Z"),
                dt("2026-01-07T02:00:00Z"),
            ]
        );
    }

    #[test]
    fn zone_offset_can_move_the_occurrence_to_a_different_utc_date() {
        // fails the same way as the Oslo case, but here the local and UTC
        // calendar dates disagree: a naive UTC-only implementation gets the
        // date wrong in the other direction
        let schedule = CronSchedule::parse("30 23 * * *", Some("Pacific/Auckland")).unwrap();
        let seq = occurrence_sequence(&schedule, dt("2026-07-05T00:00:00Z"), 3);
        assert_eq!(
            seq,
            vec![
                dt("2026-07-05T11:30:00Z"),
                dt("2026-07-06T11:30:00Z"),
                dt("2026-07-07T11:30:00Z"),
            ]
        );
    }

    #[test]
    fn spring_forward_gap_lands_on_the_first_valid_instant() {
        // fails if a fixed-time job silently skips the day it lands in the
        // 2am-3am gap instead of firing at the first valid instant after it
        let schedule = CronSchedule::parse("30 2 * * *", Some("America/New_York")).unwrap();
        let seq = occurrence_sequence(&schedule, dt("2026-03-06T12:00:00Z"), 4);
        assert_eq!(
            seq,
            vec![
                dt("2026-03-07T07:30:00Z"),
                dt("2026-03-08T07:00:00Z"), // gap day: 02:30 doesn't exist; fires at 03:00 EDT
                dt("2026-03-09T06:30:00Z"),
                dt("2026-03-10T06:30:00Z"),
            ]
        );
    }

    #[test]
    fn spring_forward_wildcard_skips_nonexistent_slots() {
        // fails if an interval job fires the gap's nominal 02:00-02:45
        // occurrences anyway instead of resuming on the new wall clock
        let schedule = CronSchedule::parse("*/15 * * * *", Some("America/New_York")).unwrap();
        let seq = occurrence_sequence(&schedule, dt("2026-03-08T06:40:00Z"), 10);
        assert_eq!(
            seq,
            vec![
                dt("2026-03-08T06:45:00Z"),
                dt("2026-03-08T07:00:00Z"), // 03:00 EDT, right after the gap
                dt("2026-03-08T07:15:00Z"),
                dt("2026-03-08T07:30:00Z"),
                dt("2026-03-08T07:45:00Z"),
                dt("2026-03-08T08:00:00Z"),
                dt("2026-03-08T08:15:00Z"),
                dt("2026-03-08T08:30:00Z"),
                dt("2026-03-08T08:45:00Z"),
                dt("2026-03-08T09:00:00Z"),
            ]
        );
    }

    #[test]
    fn fall_back_repeated_hour_fires_once() {
        // fails if `next_after` double-fires across the repeated 1am hour
        // instead of resolving it to the single EDT instant croner picks
        let schedule = CronSchedule::parse("30 1 * * *", Some("America/New_York")).unwrap();
        let seq = occurrence_sequence(&schedule, dt("2026-10-30T12:00:00Z"), 4);
        assert_eq!(
            seq,
            vec![
                dt("2026-10-31T05:30:00Z"),
                dt("2026-11-01T05:30:00Z"), // repeated hour: EDT instant only, not also EST
                dt("2026-11-02T06:30:00Z"),
                dt("2026-11-03T06:30:00Z"),
            ]
        );
    }

    #[test]
    fn pattern_that_never_matches_returns_none() {
        // fails if every `CronError` variant is mapped to `Err`, losing the
        // `Ok(None)` that `TimeSearchLimitExceeded` alone must produce
        let schedule = CronSchedule::parse("0 0 30 2 *", None).unwrap();
        assert_eq!(schedule.next_after(dt("2026-01-01T00:00:00Z")), Ok(None));
    }

    #[test]
    fn search_failure_other_than_exhaustion_surfaces_as_err() {
        // Guards `Err(_) => Ok(None)` from collapsing both `CronError` arms
        // into one. `MAX_UTC` reports `InvalidTime`, not
        // `TimeSearchLimitExceeded`, so this must take the `Err` arm.
        let schedule = CronSchedule::parse("0 3 * * *", None).unwrap();
        match schedule.next_after(DateTime::<Utc>::MAX_UTC) {
            Err(CronScheduleError::Search { reason }) => {
                assert_eq!(reason, "CronScheduler encountered an invalid time.");
            }
            other => panic!("expected Err(Search), got {other:?}"),
        }
    }

    #[test]
    fn malformed_pattern_is_rejected() {
        // fails if a genuine parse failure is swallowed into `Ok`, only to
        // surface later at scheduling time instead of at parse time
        match CronSchedule::parse("not a cron", None) {
            Err(CronParseError::Pattern { pattern, .. }) => assert_eq!(pattern, "not a cron"),
            other => panic!("expected Pattern error, got {other:?}"),
        }
    }

    #[test]
    fn five_tokens_of_garbage_are_rejected() {
        // fails if the validator only counts whitespace-separated tokens
        // instead of checking each field's range
        match CronSchedule::parse("99 99 99 99 99", None) {
            Err(CronParseError::Pattern { pattern, .. }) => assert_eq!(pattern, "99 99 99 99 99"),
            other => panic!("expected Pattern error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_timezone_is_rejected_at_parse_time() {
        // fails if `parse` accepts any string, leaving the bad zone to
        // surface later when the daemon's cron worker tries to schedule
        // against it
        match CronSchedule::parse("0 3 * * *", Some("Mars/Olympus")) {
            Err(CronParseError::Timezone { name }) => assert_eq!(name, "Mars/Olympus"),
            other => panic!("expected Timezone error, got {other:?}"),
        }
    }

    #[test]
    fn day_of_month_last_day_extension_is_rejected() {
        // fails if the character scan misses `L` sitting alone in a field
        assert_extension_char_rejected("0 0 L * *", 'L');
    }

    #[test]
    fn day_of_month_nearest_weekday_extension_is_rejected() {
        // fails if `W` is dropped from the scan: the character most likely
        // to be skipped, since `JUL`/`WED` make a naive scan treat it as
        // part of a name
        assert_extension_char_rejected("0 0 1W * *", 'W');
    }

    #[test]
    fn day_of_week_nth_occurrence_extension_is_rejected() {
        // fails if `#` is missed by the scan
        assert_extension_char_rejected("0 0 * * 5#3", '#');
    }

    #[test]
    fn day_of_week_any_extension_is_rejected() {
        // fails if `?` is missed by the scan
        assert_extension_char_rejected("0 0 ? * *", '?');
    }

    #[test]
    fn month_and_weekday_names_are_not_mistaken_for_extension_characters() {
        // fails if the scan is character-wise instead of name-aware: `JUL`
        // contains `L` and `WED` contains `W`, both legal here. A suite that
        // only covers rejections would pass an implementation that rejects
        // every name-bearing pattern.
        let schedule = CronSchedule::parse("0 0 * JUL WED", None).unwrap();
        assert_eq!(schedule.pattern(), "0 0 * JUL WED");
    }

    #[test]
    fn weekday_range_names_are_not_mistaken_for_extension_characters() {
        // fails the same way, for a range spelled with day names either side
        let schedule = CronSchedule::parse("0 0 * * MON-FRI", None).unwrap();
        assert_eq!(schedule.pattern(), "0 0 * * MON-FRI");
    }
}
