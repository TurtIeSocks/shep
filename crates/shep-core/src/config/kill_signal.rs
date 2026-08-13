//! The stop-signal grammar: the four signals `kill_signal` may name.
//!
//! Lives in shep-core rather than beside the kill ladder that sends them
//! because two layers need the same answer and only one of them can reach the
//! OS: `normalize` has to REFUSE a name the daemon could not send, and the
//! daemon has to MAP an accepted name onto its own portable `StopSignal`.
//! Splitting that grammar across the two crates is how the clamp got in — the
//! daemon knew four names, the validator knew none, and the gap between them
//! was a `tracing::warn!` nobody reads in a detached process.

/// A signal `kill_signal` may name.
///
/// Four, not every signal on the platform, and deliberately: each one here is
/// a signal the daemon's stop ladder can actually deliver and then escalate
/// past. Growth is possible but is not anticipated — the ladder's shape, not
/// the grammar's, is what would have to change first — so this is left
/// exhaustive rather than `#[non_exhaustive]` (IR-20: don't cargo-cult it).
/// A caller matching on all four today gets a compile error the day a fifth
/// arrives, which is the outcome we want at both call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillSignal {
    /// `SIGTERM` — the default, and a graceful stop request.
    Term,
    /// `SIGINT` — interrupt, what Ctrl-C sends.
    Int,
    /// `SIGQUIT` — quit, core-dumping by default.
    Quit,
    /// `SIGUSR2` — user-defined signal 2, the one several runtimes reserve
    /// for a graceful restart.
    Usr2,
}

impl KillSignal {
    /// Every spelling this grammar accepts, canonical form, in the order an
    /// error message lists them.
    ///
    /// Public because [`NormalizeError::InvalidKillSignal`](crate::config::NormalizeError)
    /// renders it into the refusal, and a caller building its own diagnostic
    /// (a `--help` line, an editor completion) wants the same list rather
    /// than a second copy that can drift.
    pub const ACCEPTED: [&'static str; 4] = ["SIGTERM", "SIGINT", "SIGQUIT", "SIGUSR2"];

    /// Parses one `kill_signal` name, case-insensitively, with or without the
    /// `SIG` prefix. `None` for anything else.
    ///
    /// Both spellings are accepted because both were accepted before this
    /// grammar existed, and a validation pass that starts refusing a
    /// Flockfile that worked yesterday is a worse bug than the one it fixes.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "SIGTERM" | "TERM" => Some(Self::Term),
            "SIGINT" | "INT" => Some(Self::Int),
            "SIGQUIT" | "QUIT" => Some(Self::Quit),
            "SIGUSR2" | "USR2" => Some(Self::Usr2),
            _ => None,
        }
    }

    /// The canonical name, always `SIG`-prefixed and uppercase.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Usr2 => "SIGUSR2",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `ACCEPTED` and `as_str` disagree — the list an operator is
    /// shown in a refusal has to be the list `parse` actually takes, and the
    /// two are written out separately.
    #[test]
    fn every_accepted_name_round_trips_through_parse() {
        for name in KillSignal::ACCEPTED {
            let parsed = KillSignal::parse(name)
                .unwrap_or_else(|| panic!("`{name}` is advertised but not parsed"));
            assert_eq!(parsed.as_str(), name);
        }
    }

    /// fails if the bare form or a lowercase spelling stops parsing.
    #[test]
    fn the_prefix_and_the_case_are_both_optional() {
        assert_eq!(KillSignal::parse("usr2"), Some(KillSignal::Usr2));
        assert_eq!(KillSignal::parse("SigQuit"), Some(KillSignal::Quit));
    }

    /// fails if a real signal shep cannot deliver is waved through. `SIGUSR1`
    /// is the exact name that motivated this module: a plausible typo for
    /// `SIGUSR2`, and one that used to become SIGTERM in silence.
    #[test]
    fn a_signal_the_ladder_cannot_send_does_not_parse() {
        assert_eq!(KillSignal::parse("SIGUSR1"), None);
        assert_eq!(KillSignal::parse("SIGKILL"), None);
        assert_eq!(KillSignal::parse(""), None);
    }
}
