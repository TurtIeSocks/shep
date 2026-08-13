//! The signals `shep signal` may name.
//!
//! A grammar of its own, next to [`KillSignal`](crate::config::KillSignal)'s
//! four rather than replacing them, because the two answer different
//! questions. `KillSignal` is what a Flockfile's `kill_signal` may say: a
//! signal the stop ladder can deliver as its polite rung and then escalate
//! PAST. This one is what an operator may hand a running app: a nudge, with no
//! ladder behind it and no escalation to follow. `SIGHUP` belongs here and not
//! there; `SIGKILL` belongs here and not there for the opposite reason.
//!
//! # No raw numbers
//!
//! Deliberately no `as_raw`. Signal numbers are not portable — `SIGUSR1` is 10
//! on Linux and 30 on macOS, `SIGCONT` is 18 and 19 — and shep-core is the
//! portable crate with no libc to ask. The enum crosses shep-daemon's runner
//! seam as an enum, exactly as `StopSignal` does, and `tokio_runner.rs` is the
//! one place that turns it into something the kernel understands.
//!
//! # What is not here, and why
//!
//! `SIGSTOP` parses to nothing. It is deliverable and an operator might mean
//! it, but a `SIGSTOP`ed sheep still reads `online` in `shep flock`, in
//! `describe`, on the bus and to every dog — the shepherd owns no mechanism
//! that could see the difference. Refusing it keeps shep from producing a
//! flock state it cannot describe. `SIGCONT` IS accepted, because an operator
//! who stopped a sheep by some other route needs a way back.

/// A signal `shep signal` may name.
///
/// Nine, not every signal on the platform. Each one here is something an
/// operator plausibly means to say to an application, and nothing here is a
/// signal shep would be delivering on the kernel's behalf (`SIGSEGV`,
/// `SIGBUS`, `SIGPIPE` and the rest are the kernel's to send, not an
/// operator's).
///
/// Exhaustive, not `#[non_exhaustive]`, matching
/// [`KillSignal`](crate::config::KillSignal) and for the same reason (IR-20:
/// don't cargo-cult it). Growth is possible but is not anticipated, and a
/// caller matching on all nine — shep-daemon's own mapping to `nix` is the one
/// that matters — should get a compile error the day a tenth arrives rather
/// than a silent wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorSignal {
    /// `SIGHUP` — hang up. The near-universal "re-read your configuration".
    Hup,
    /// `SIGINT` — interrupt, what Ctrl-C sends.
    Int,
    /// `SIGQUIT` — quit, core-dumping by default. Several runtimes dump every
    /// thread's stack on it instead.
    Quit,
    /// `SIGTERM` — the polite stop. Sending it here bypasses the stop ladder
    /// entirely: shep does not start a `kill_timeout`, does not escalate, and
    /// does not mark the sheep stopped. Use `shep stop` for a stop.
    Term,
    /// `SIGUSR1` — user-defined signal 1.
    Usr1,
    /// `SIGUSR2` — user-defined signal 2, the one several runtimes reserve for
    /// a graceful restart.
    Usr2,
    /// `SIGWINCH` — terminal resized. Harmless to nearly everything, which is
    /// what makes it the signal to test a wiring with.
    Winch,
    /// `SIGCONT` — continue a stopped process.
    Cont,
    /// `SIGKILL` — unblockable, immediate. The restart policy will see the
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
    /// prefix. `None` for anything else, including a raw number — a number
    /// means different signals on different platforms, and shep will not guess
    /// which one an operator meant.
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

    /// fails if `ACCEPTED` and `as_str` disagree. The list is what a refusal
    /// prints, so an operator picking a replacement word is reading it — a
    /// name advertised but not parsed sends them in a circle.
    #[test]
    fn every_accepted_name_round_trips_through_parse() {
        for name in OperatorSignal::ACCEPTED {
            let parsed = OperatorSignal::parse(name)
                .unwrap_or_else(|| panic!("`{name}` is advertised but not parsed"));
            assert_eq!(parsed.as_str(), name);
        }
    }

    /// fails if the bare form or a lowercase spelling stops parsing. Both are
    /// accepted for the reason `KillSignal` accepts both: an operator types
    /// what `kill -l` prints, and that is the bare form.
    #[test]
    fn the_prefix_and_the_case_are_both_optional() {
        assert_eq!(OperatorSignal::parse("hup"), Some(OperatorSignal::Hup));
        assert_eq!(OperatorSignal::parse("SigUsr1"), Some(OperatorSignal::Usr1));
        assert_eq!(OperatorSignal::parse("WINCH"), Some(OperatorSignal::Winch));
    }

    /// fails if SIGSTOP is ever waved through. It is the one real, spellable,
    /// deliverable signal this grammar refuses, and the refusal is the design:
    /// a stopped sheep still reads `online` in every listing shep can produce,
    /// so accepting it would put the flock in a state the shepherd cannot
    /// report on.
    #[test]
    fn sigstop_is_refused_because_the_shepherd_could_not_report_it() {
        assert_eq!(OperatorSignal::parse("SIGSTOP"), None);
        assert_eq!(OperatorSignal::parse("stop"), None);
    }

    /// fails if a name outside the table parses. `SIGSEGV` is the shape that
    /// matters: a real signal, plausibly typed, that shep has no business
    /// delivering on an operator's behalf.
    #[test]
    fn a_name_outside_the_table_does_not_parse() {
        assert_eq!(OperatorSignal::parse("SIGSEGV"), None);
        assert_eq!(OperatorSignal::parse(""), None);
        assert_eq!(OperatorSignal::parse("9"), None);
    }

    /// fails if this grammar stops covering the one `kill_signal` already
    /// accepts. The two exist for different jobs and are allowed to differ —
    /// but the operator-facing set being NARROWER than the config-facing one
    /// would mean a signal shep sends on every stop is one an operator may not
    /// ask for by name, which is indefensible in either direction.
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
