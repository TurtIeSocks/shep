//! Whether this whistle may act, and where that answer comes from.
//!
//! One source: `[whistle] allow_control` in `$SHEP_HOME/shep.toml`. Not a
//! flag, not an environment variable — see [`resolve_control`].

use shep_core::config::DaemonConfig;

/// Whether whistle's control tools exist.
///
/// The same two-state concept lookout shipped in 12a
/// (`lookout::app::Control`), and deliberately a separate type rather than a
/// shared one: lookout reads the KV store because its gate is the operator's
/// own — a person is at the keyboard — while this one reads the shepherd's
/// config file because these tools act for a client nobody is watching. A
/// shared type would have to carry both sources and would serve neither. What
/// an operator learns once is the word `allow_control` and its two states.
///
/// **A fat-finger catch, not a security boundary.** whistle runs as the
/// operator's own uid; anyone who can launch it can run `shep stop`. What the
/// default buys is narrower and real: with the gate shut, text a sheep printed
/// — which `super::read`'s `tail_bleats` hands to a model verbatim — cannot
/// reach a tool that acts.
///
/// Not an error enum, so IR-20's `#[non_exhaustive]` rule does not apply; and
/// shep-cli is `[[bin]]`-only, so nothing here is in a library crate at all.
///
/// Not constructed outside this module's own tests yet: the `Whistle`
/// handler that builds a router from this is Task 8. `#[allow(dead_code)]`
/// says so explicitly rather than inventing a call site nothing needs yet —
/// same pattern as `output::table::render_table`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The four control tools are not registered. The default.
    ReadOnly,
    /// The four control tools are registered alongside the five read-only ones.
    Allowed,
}

impl Control {
    /// The one sentence that tells an operator how to open the gate.
    ///
    /// Named rather than inlined because three places say it — the tool
    /// catalogue, `get_info`'s instructions, and the stderr notice on a
    /// malformed config — and three copies would drift.
    ///
    /// Takes `self` (`Control` is `Copy`) rather than being receiverless: the
    /// call sites read `control.how_to_open()`, and a receiver leaves room for
    /// the `Allowed` arm to say something different later without moving any
    /// of them.
    ///
    /// Not called outside this module's own tests yet: `get_info`'s
    /// instructions and the malformed-config stderr notice are Task 8's.
    #[allow(dead_code)]
    #[must_use]
    pub const fn how_to_open(self) -> &'static str {
        "control tools are off; add `[whistle]` with `allow_control = true` to \
         $SHEP_HOME/shep.toml and restart whistle"
    }
}

/// Reads the gate out of `shep.toml`'s text.
///
/// `None` means the file does not exist, which is the ordinary case and reads
/// as "no". A file that will not parse also reads as "no": a broken config is
/// exactly when something is wrong with the machine, and a gate that failed
/// open then would vanish at the worst moment. The caller
/// (`super::whistle`) prints the parse failure to stderr, so a shut gate is
/// never silent about being shut for the wrong reason.
///
/// **`&|_| None` for the environment closure is about testability, not
/// security.** `DaemonConfig::load` layers `SHEP_LOG_JSON`, `SHEP_LOG_LEVEL`,
/// `SHEP_SOCKET` and `SHEP_MAX_CRON_SLEEP` over the parsed file; **none of the
/// four touches `allow_control` in either direction**, so no env closure could
/// open this gate and passing `None` defends nothing. What it does buy is that
/// this function is a pure function of the file's text: every case is testable
/// without a tempdir, without `std::env::set_var` (`unsafe` in edition 2024,
/// and it races the rest of the suite), and without depending on how the test
/// binary happened to be launched.
///
/// There is still no `SHEP_WHISTLE_ALLOW_CONTROL`, and there must not be one —
/// but the reason is spec §14.7's, which is about a config file being
/// auditable where a per-invocation setting is not. It is **not** that argv and
/// the environment cannot reach this gate. They can, by choosing which
/// `$SHEP_HOME` is read: `shep whistle --home <dir>` and `SHEP_HOME=<dir> shep
/// whistle` both select the `shep.toml` this function is handed. The launcher
/// is the boundary, in argv, environment and file alike.
///
/// Not called outside this module's own tests yet: the verb that reads
/// `shep.toml` and calls this is Task 8's `whistle::whistle`.
#[allow(dead_code)]
#[must_use]
pub fn resolve_control(shep_toml: Option<&str>) -> Control {
    match DaemonConfig::load(shep_toml, &|_| None) {
        Ok(config) if config.whistle.allow_control => Control::Allowed,
        _ => Control::ReadOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the file stops being read, or if the default stops being
    /// "no". Both halves matter: a gate that never opens is useless and a
    /// gate that opens by accident is worse than none.
    #[test]
    fn the_file_is_the_only_source_and_it_defaults_to_read_only() {
        assert_eq!(resolve_control(None), Control::ReadOnly);
        assert_eq!(resolve_control(Some("")), Control::ReadOnly);
        assert_eq!(
            resolve_control(Some("[daemon]\nlog_level = \"info\"\n")),
            Control::ReadOnly
        );
        assert_eq!(
            resolve_control(Some("[whistle]\nallow_control = true\n")),
            Control::Allowed
        );
        assert_eq!(
            resolve_control(Some("[whistle]\nallow_control = false\n")),
            Control::ReadOnly
        );
    }

    /// fails if a broken config file fails OPEN. A `shep.toml` that will not
    /// parse is exactly the moment something is wrong with the machine, and
    /// a gate that disappears then is a gate that was never there.
    #[test]
    fn a_file_that_will_not_parse_is_read_as_no() {
        assert_eq!(resolve_control(Some("[whistle")), Control::ReadOnly);
        assert_eq!(
            resolve_control(Some("[whistle]\nallow_control = \"yes\"\n")),
            Control::ReadOnly,
            "a string where a bool belongs is a broken file, not a true"
        );
    }

    // DELETED, deliberately, and the deletion is recorded here so it is not
    // reinstated by a well-meaning later reader:
    // `no_environment_variable_can_open_the_gate` was a dead check twice
    // over. It set no environment variable, so swapping `&|_| None` for
    // `&|k| std::env::var(k).ok()` — the exact regression its doc claimed to
    // catch — left it green. And the property was vacuous anyway:
    // `DaemonConfig::load` reads only SHEP_LOG_JSON, SHEP_LOG_LEVEL,
    // SHEP_SOCKET and SHEP_MAX_CRON_SLEEP (daemon.rs:178-205), none of which
    // touches `whistle.allow_control` in either direction, so no env closure
    // could open this gate whatever was passed. Its assertions duplicated the
    // first test's. The real environment-reaches-the-gate path is
    // `--home`/`$SHEP_HOME` selecting WHICH shep.toml is read — see the
    // plan's "Why there is no `--allow-control` flag" — and Task 10 pins that
    // one end to end, in a real process, where it can actually fail.

    /// fails if the refusal text stops naming the exact edit. An operator
    /// told "control is off" and not told the two lines to write will guess,
    /// and the most likely guess is a flag that does not exist.
    #[test]
    fn the_refusal_names_the_file_and_the_key() {
        let notice = Control::ReadOnly.how_to_open();
        // Method syntax on a value, which is why `how_to_open` takes `self`
        // rather than being a receiverless associated function.
        assert!(notice.contains("[whistle]"));
        assert!(notice.contains("allow_control = true"));
        assert!(notice.contains("shep.toml"));
        assert!(
            !notice.contains("--allow-control"),
            "whistle has no such flag; pointing at one would send an operator in a circle"
        );
    }
}
