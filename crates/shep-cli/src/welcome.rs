//! The first-run welcome: the art, the quick start, and the text around them.
//!
//! One module owns the copy so it has a single home and a single pinning
//! test, the way `docs/lookout/frames.txt` pins a rendered frame.

use std::path::Path;

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{OutputEnvelope, SCHEMA_VERSION, Streams};

/// The art, with `{version}` and `{home}` substituted at render time.
///
/// Original work, not taken from the ASCII-art corpus. Deliberately about a
/// third the height of pm2's banner: the point is to be seen once and not
/// resented.
///
/// No `\` line-continuation before the first line, deliberately: `\` followed
/// by a newline strips the newline *and the next line's leading whitespace*,
/// which silently unindented the leftmost sheep by six columns. An
/// exact-string test cannot catch that on its own, because the expected value
/// is written the same way and loses the same six columns.
const ART: &str = "      ,-~-.     ,-~-.     ,-~-.
     ( o.o )   ( o.o )   ( o.o )       shep {version}
      `-^-'     `-^-'     `-^-'        flock at {home}
       \" \"       \" \"       \" \"
    /\\  /\\
   ( o  o )--,   the shepherd keeps them running
    `--..--'  |
      |  |    '
";

/// The five commands that get someone from nothing to a process that
/// survives a reboot.
///
/// Deliberately absent: `--home`, `fold`, a link, and anything about dogs or
/// the whistle. Those are `shep --help`'s job. A welcome that lists
/// everything teaches nothing.
const QUICK_START: &str = "\
Getting started
  shep start server.js    start it and keep it alive
  shep flock              see what's running
  shep bleats server      follow its output
  shep save               remember this flock across reboots
  shep startup            bring it back after a reboot

  shep welcome            show this again
";

/// Renders the welcome for one flock.
///
/// `home` appears twice, once as the art's caption and once in the prose line
/// under it, so a `--home` render names the flock it is actually about rather
/// than advertising the default.
pub(crate) fn render(home: &Path) -> String {
    let home = home.display().to_string();
    let art = ART
        .replace("{version}", env!("CARGO_PKG_VERSION"))
        .replace("{home}", &home);
    format!(
        "{art}\nSet up {home}. Logs, pids and the shepherd's socket live here.\n\n{QUICK_START}"
    )
}

/// The welcome as one JSON field, so `--format json` gets an envelope like
/// every other verb rather than silence.
///
/// Not a [`crate::output::Render`] impl: that trait is for tabular data and
/// requires `headers()` and `rows()`, which free-form text has neither of.
/// The envelope is built from the same [`OutputEnvelope`] and
/// [`SCHEMA_VERSION`] every other verb goes through, so the schema stays
/// consistent without pretending this is a table.
#[derive(Debug, serde::Serialize)]
struct WelcomeData {
    /// The rendered welcome, newlines and all.
    text: String,
}

/// Prints the welcome as a side effect of whichever command created the home.
///
/// Suppressed under `--format json` and when stderr is not a terminal: a cold
/// machine is exactly where a provisioning script runs first, and a banner in
/// the middle of `shep start server.js | jq` is a bug. The home is still
/// created when the text is suppressed. Suppression governs the output, never
/// the side effect.
///
/// `stderr_is_terminal` is a parameter rather than an `IsTerminal` call in
/// here for the same reason [`crate::commands::daemon`]'s `ansi_enabled`
/// takes one: a test writing into a `Vec` could not otherwise reach this
/// branch.
pub(crate) fn on_first_run(streams: &mut Streams<'_>, home: &Path, stderr_is_terminal: bool) {
    if streams.fmt == Format::Json || !stderr_is_terminal {
        return;
    }
    let _ = write!(streams.err, "{}", render(home));
}

/// `shep welcome`: the same text, asked for by name.
///
/// stdout rather than stderr, and no terminal check, because here the welcome
/// *is* the command's output rather than a diagnostic. An explicit invocation
/// outranks the side-effect path, so a `shep welcome` that also happens to
/// create the home prints once, here.
pub(crate) fn welcome(streams: &mut Streams<'_>, home: &Path) -> ExitCode {
    let text = render(home);
    let wrote = match streams.fmt {
        Format::Table => write!(streams.out, "{text}"),
        Format::Json => {
            let envelope = OutputEnvelope {
                schema_version: SCHEMA_VERSION,
                command: "welcome",
                data: WelcomeData { text },
            };
            serde_json::to_writer(&mut *streams.out, &envelope)
                .map_err(std::io::Error::other)
                .and_then(|()| writeln!(streams.out))
        }
    };
    match wrote {
        Ok(()) => ExitCode::Success,
        Err(_) => ExitCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned whole, the way `docs/lookout/frames.txt` pins a rendered frame.
    /// Art drifts silently otherwise, and the one thing a welcome cannot
    /// afford is to look unmaintained.
    #[test]
    fn the_welcome_renders_exactly_this() {
        let rendered = render(Path::new("/home/ada/.shep"));
        let expected = format!(
            "      ,-~-.     ,-~-.     ,-~-.
     ( o.o )   ( o.o )   ( o.o )       shep {version}
      `-^-'     `-^-'     `-^-'        flock at /home/ada/.shep
       \" \"       \" \"       \" \"
    /\\  /\\
   ( o  o )--,   the shepherd keeps them running
    `--..--'  |
      |  |    '

Set up /home/ada/.shep. Logs, pids and the shepherd's socket live here.

Getting started
  shep start server.js    start it and keep it alive
  shep flock              see what's running
  shep bleats server      follow its output
  shep save               remember this flock across reboots
  shep startup            bring it back after a reboot

  shep welcome            show this again
",
            version = env!("CARGO_PKG_VERSION"),
        );
        assert_eq!(rendered, expected);
    }

    /// The path appears twice and both must follow `--home`, or a second
    /// flock's welcome would advertise the first flock's directory.
    #[test]
    fn the_home_path_is_substituted_everywhere_it_appears() {
        let rendered = render(Path::new("/srv/api"));
        assert_eq!(
            rendered.matches("/srv/api").count(),
            2,
            "both the art's caption and the prose line name the home:\n{rendered}"
        );
        assert!(
            !rendered.contains("~/.shep"),
            "no hardcoded default leaks through:\n{rendered}"
        );
    }

    /// No em dashes in copy a user reads. Doc comments may carry them; this
    /// may not.
    #[test]
    fn the_welcome_copy_has_no_em_dashes() {
        let rendered = render(Path::new("/home/ada/.shep"));
        assert!(
            !rendered.contains('\u{2014}'),
            "em dash in user-facing copy"
        );
        assert!(
            !rendered.contains('\u{2013}'),
            "en dash in user-facing copy"
        );
    }

    /// Runs `f` against buffered streams rendering as `fmt`, and hands back
    /// what each received.
    fn drain(fmt: Format, f: impl FnOnce(&mut Streams<'_>)) -> (String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt,
            };
            f(&mut streams);
        }
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// A fresh machine is exactly where a provisioning script runs first, so
    /// the side-effect welcome goes to stderr and leaves stdout for the
    /// command the operator actually ran.
    #[test]
    fn the_first_run_welcome_goes_to_stderr() {
        let (out, err) = drain(Format::Table, |s| {
            on_first_run(s, Path::new("/home/ada/.shep"), true);
        });
        assert!(out.is_empty(), "stdout must stay clean: {out}");
        assert!(
            err.contains("Getting started"),
            "stderr must carry it: {err}"
        );
    }

    /// `shep start server.js | jq` on a cold box must not find a sheep in its
    /// input, and neither must a `--format json` consumer.
    #[test]
    fn the_first_run_welcome_is_suppressed_for_json_and_for_pipes() {
        let (_, json) = drain(Format::Json, |s| {
            on_first_run(s, Path::new("/x"), true);
        });
        assert!(json.is_empty(), "--format json must suppress it: {json}");

        let (_, piped) = drain(Format::Table, |s| {
            on_first_run(s, Path::new("/x"), false);
        });
        assert!(
            piped.is_empty(),
            "a non-terminal stderr suppresses it: {piped}"
        );
    }

    /// Asked for by name it is the command's output, not a diagnostic, so it
    /// goes to stdout and no terminal check applies.
    #[test]
    fn the_welcome_verb_prints_to_stdout_even_when_piped() {
        let (out, err) = drain(Format::Table, |s| {
            welcome(s, Path::new("/home/ada/.shep"));
        });
        assert!(
            out.contains("Getting started"),
            "stdout must carry it: {out}"
        );
        assert!(err.is_empty(), "nothing belongs on stderr here: {err}");
    }

    /// Every other verb answers `--format json` with an envelope. So does
    /// this one, rather than printing nothing and looking broken.
    #[test]
    fn the_welcome_verb_answers_json_with_an_envelope() {
        let (out, _) = drain(Format::Json, |s| {
            welcome(s, Path::new("/home/ada/.shep"));
        });
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["command"], "welcome");
        assert_eq!(parsed["schema_version"], SCHEMA_VERSION);
        assert!(
            parsed["data"]["text"]
                .as_str()
                .unwrap()
                .contains("Getting started"),
            "the envelope carries the text: {out}"
        );
    }

    /// A terminal is 80 columns until proven otherwise, and a welcome that
    /// wraps looks broken rather than charming.
    #[test]
    fn the_welcome_fits_an_eighty_column_terminal() {
        let rendered = render(Path::new("/home/ada/.shep"));
        for line in rendered.lines() {
            assert!(
                line.chars().count() <= 80,
                "line is {} columns: {line:?}",
                line.chars().count()
            );
        }
    }
}
