//! The signals `shep signal` may name.
//!
//! A grammar of its own, next to [`KillSignal`](crate::config::KillSignal)'s
//! four: that one is what a Flockfile's `kill_signal` may say and the stop
//! ladder delivers; this one is a nudge an operator hands a running app,
//! with no ladder and no escalation.
//!
//! No `as_raw`: signal numbers are not portable (`SIGUSR1` is 10 on Linux,
//! 30 on macOS), and shep-core has no libc to ask.
//!
//! `SIGSTOP` parses to nothing: a `SIGSTOP`ed sheep still reads `online`
//! everywhere shep reports state. `SIGCONT` is accepted, for the way back.

/// A signal `shep signal` may name.
///
/// Nine, not every signal on the platform: each is something an operator
/// plausibly means to say to an application. Nothing here is a signal shep
/// would deliver on the kernel's behalf (`SIGSEGV`, `SIGBUS`, `SIGPIPE` and
/// the rest are the kernel's to send).
///
/// Exhaustive, not `#[non_exhaustive]`: a caller matching on all nine
/// should get a compile error the day a tenth arrives, not a silent
/// wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorSignal {
    /// `SIGHUP`: hang up, the near-universal "re-read your configuration".
    Hup,
    /// `SIGINT`: interrupt, what Ctrl-C sends.
    Int,
    /// `SIGQUIT`: quit, core-dumping by default. Several runtimes dump every
    /// thread's stack on it instead.
    Quit,
    /// `SIGTERM`: the polite stop. Sending it here bypasses the stop ladder
    /// entirely: shep does not start a `kill_timeout`, does not escalate, and
    /// does not mark the sheep stopped. Use `shep stop` for a stop.
    Term,
    /// `SIGUSR1`: user-defined signal 1.
    Usr1,
    /// `SIGUSR2`: user-defined signal 2, the one several runtimes reserve for
    /// a graceful restart.
    Usr2,
    /// `SIGWINCH`: terminal resized. Harmless to nearly everything, which is
    /// what makes it the signal to test a wiring with.
    Winch,
    /// `SIGCONT`: continue a stopped process.
    Cont,
    /// `SIGKILL`: unblockable, immediate. The restart policy will see the
    /// exit as any other unexpected one and act on it: an app with
    /// `autorestart` on comes back.
    Kill,
}

impl OperatorSignal {
    /// Every spelling this grammar accepts, canonical form, in the order a
    /// refusal lists them.
    ///
    /// Public because it is rendered into the refusal an operator reads and
    /// into `shep signal --help`; a second hand-written list in either place
    /// is one free to drift.
    pub const ACCEPTED: [&'static str; 9] = [
        "SIGHUP", "SIGINT", "SIGQUIT", "SIGTERM", "SIGUSR1", "SIGUSR2", "SIGWINCH", "SIGCONT",
        "SIGKILL",
    ];

    /// Parses one signal name, case-insensitively, with or without the `SIG`
    /// prefix. `None` for anything else, including a raw number: a number
    /// means different signals on different platforms, and shep will not
    /// guess which one an operator meant.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "SIGHUP" | "HUP" => Some(Self::Hup),
            "SIGINT" | "INT" => Some(Self::Int),
            "SIGQUIT" | "QUIT" => Some(Self::Quit),
            "SIGTERM" | "TERM" => Some(Self::Term),
            "SIGUSR1" | "USR1" => Some(Self::Usr1),
            "SIGUSR2" | "USR2" => Some(Self::Usr2),
            "SIGWINCH" | "WINCH" => Some(Self::Winch),
            "SIGCONT" | "CONT" => Some(Self::Cont),
            "SIGKILL" | "KILL" => Some(Self::Kill),
            _ => None,
        }
    }

    /// The canonical name, always `SIG`-prefixed and uppercase.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hup => "SIGHUP",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Term => "SIGTERM",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
            Self::Winch => "SIGWINCH",
            Self::Cont => "SIGCONT",
            Self::Kill => "SIGKILL",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is what a refusal prints, so a name advertised but not
    /// parsed sends an operator in a circle.
    #[test]
    fn every_accepted_name_round_trips_through_parse() {
        for name in OperatorSignal::ACCEPTED {
            let parsed = OperatorSignal::parse(name)
                .unwrap_or_else(|| panic!("`{name}` is advertised but not parsed"));
            assert_eq!(parsed.as_str(), name);
        }
    }

    /// Both accepted for the reason `KillSignal` accepts both: an operator
    /// types what `kill -l` prints, the bare form.
    #[test]
    fn the_prefix_and_the_case_are_both_optional() {
        assert_eq!(OperatorSignal::parse("hup"), Some(OperatorSignal::Hup));
        assert_eq!(OperatorSignal::parse("SigUsr1"), Some(OperatorSignal::Usr1));
        assert_eq!(OperatorSignal::parse("WINCH"), Some(OperatorSignal::Winch));
    }

    /// The one real, spellable, deliverable signal this grammar refuses: a
    /// stopped sheep still reads `online` everywhere shep reports state.
    #[test]
    fn sigstop_is_refused_because_the_shepherd_could_not_report_it() {
        assert_eq!(OperatorSignal::parse("SIGSTOP"), None);
        assert_eq!(OperatorSignal::parse("stop"), None);
    }

    /// `SIGSEGV` is the shape that matters: a real signal, plausibly typed,
    /// that shep has no business delivering on an operator's behalf.
    #[test]
    fn a_name_outside_the_table_does_not_parse() {
        assert_eq!(OperatorSignal::parse("SIGSEGV"), None);
        assert_eq!(OperatorSignal::parse(""), None);
        assert_eq!(OperatorSignal::parse("9"), None);
    }

    /// The two exist for different jobs and may differ, but the
    /// operator-facing set being narrower than the config-facing one would
    /// mean an operator cannot name a signal shep itself sends.
    #[test]
    fn every_kill_signal_name_is_also_an_operator_signal() {
        for name in crate::config::KillSignal::ACCEPTED {
            assert!(
                OperatorSignal::parse(name).is_some(),
                "`{name}` is a kill_signal but not an operator signal"
            );
        }
    }
}
