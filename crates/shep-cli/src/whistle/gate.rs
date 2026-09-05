//! Whether this whistle may act, and where that answer comes from.
//!
//! One source: `[whistle] allow_control` in `$SHEP_HOME/shep.toml`. Not a
//! flag, not an environment variable.

use shep_core::config::DaemonConfig;

/// Whether whistle's control tools exist.
///
/// Not a security boundary, just a fat-finger catch: whistle runs as the
/// operator's own uid, so anyone who can launch it can run `shep stop`
/// directly. With the gate shut, though, text a sheep printed (which
/// `tail_bleats` hands a model verbatim) cannot reach a tool that acts.
///
/// Separate from lookout's own `Control`: lookout reads the KV store since
/// a person is at the keyboard, this reads `shep.toml` since these tools
/// act for a client nobody is watching.
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
    /// Named rather than inlined: the tool catalogue, `get_info` and the
    /// malformed-config stderr notice all say it, and three copies would
    /// drift.
    ///
    /// Takes `self` rather than being receiverless, leaving room for the
    /// `Allowed` arm to say something different later.
    #[must_use]
    pub const fn how_to_open(self) -> &'static str {
        "control tools are off; add `[whistle]` with `allow_control = true` to \
         $SHEP_HOME/shep.toml and restart whistle"
    }
}

/// Reads the gate out of `shep.toml`'s text.
///
/// Missing or unparsable both read as `ReadOnly`: a broken config fails
/// closed rather than open at the worst moment. The caller prints the
/// parse failure to stderr, so a shut gate is never silent about why.
///
/// The environment closure is always `&|_| None`: none of
/// `DaemonConfig::load`'s four env vars touch `allow_control`, so this
/// stays a pure function of the file's text, testable without a tempdir.
///
/// No `SHEP_WHISTLE_ALLOW_CONTROL` exists; only `--home`/`$SHEP_HOME` can
/// vary which `shep.toml` is read.
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

    /// fails if a broken config file fails open. A `shep.toml` that will not
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

    /// fails if the refusal text stops naming the exact edit. An operator
    /// told "control is off" and not told the two lines to write will guess,
    /// and the most likely guess is a flag that does not exist.
    #[test]
    fn the_refusal_names_the_file_and_the_key() {
        let notice = Control::ReadOnly.how_to_open();
        assert!(notice.contains("[whistle]"));
        assert!(notice.contains("allow_control = true"));
        assert!(notice.contains("shep.toml"));
        assert!(
            !notice.contains("--allow-control"),
            "whistle has no such flag; pointing at one would send an operator in a circle"
        );
    }
}
