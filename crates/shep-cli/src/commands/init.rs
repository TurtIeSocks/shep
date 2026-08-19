//! `shep init`: scaffold a Flockfile, or add an app to one that exists.
//!
//! Design: `docs/brainstorming/specs/2026-08-18-flockfile-templates-design.md`.
//!
//! This module is being built lesson by lesson. Right now it owns exactly one
//! thing: the commented skeleton a bare `shep init` writes.

/// The commented Flockfile a bare `shep init` writes.
///
/// Rin's call on the design's first open question: not an empty `app = []`,
/// but a file carrying a commented-out `[[app]]` showing its options, so the
/// scaffolded file is the reference documentation. A `[dog.<name>]` block
/// joins it in a later lesson.
///
/// # The comment convention this file uses
///
/// Two kinds of `#` line, and the difference is load-bearing because
/// [`tests::the_skeleton_uncomments_into_a_working_flockfile`] relies on it:
///
/// - **Prose**, explaining things to a reader: `#` followed by a SPACE.
///   `# Every app the flock runs gets one of these.`
/// - **Commented-out config**, meant to be uncommented and used: `#`
///   followed immediately by the config, no space.
///   `#name = "api"`
///
/// So uncommenting the file means stripping a leading `#` from exactly the
/// lines where the next character is not a space. That rule is what lets a
/// test prove the reference is real rather than plausible-looking prose.
#[allow(dead_code)]
pub(crate) fn skeleton() -> String {
    todo!("lesson 1: write the commented skeleton")
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::config::{FlockFormat, Flockfile};

    /// Uncomments the skeleton per the convention on [`skeleton`]: a leading
    /// `#` is stripped only where the character after it is not a space, so
    /// prose stays commented and config becomes live.
    fn uncomment(source: &str) -> String {
        source
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                match trimmed.strip_prefix('#') {
                    Some(rest) if !rest.starts_with(' ') => rest.to_string(),
                    _ => line.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point of a commented reference: uncomment it and it must
    /// actually work.
    ///
    /// A scaffolded file is the most-read documentation this project has,
    /// because it is read at the moment someone is deciding whether to keep
    /// using shep. A reference that looks right and does not parse is worse
    /// than no reference at all, because it is trusted.
    ///
    /// This parses through the REAL parser, not a hand-rolled check, so the
    /// skeleton cannot drift into a shape the daemon would reject.
    #[test]
    fn the_skeleton_uncomments_into_a_working_flockfile() {
        let live = uncomment(&skeleton());

        let parsed = Flockfile::parse(&live, FlockFormat::Toml).unwrap_or_else(|err| {
            panic!("the uncommented skeleton must parse as a Flockfile: {err}\n\n--- what was parsed ---\n{live}")
        });

        assert_eq!(
            parsed.apps.len(),
            1,
            "the skeleton declares exactly one example app:\n{live}"
        );

        let app = &parsed.apps[0];
        assert!(
            !app.name.is_empty(),
            "the example app needs a name, which is one of the two fields \
             normalize() actually requires"
        );
        assert!(
            !app.script.is_empty(),
            "the example app needs a script, the other required field"
        );
    }

    /// The skeleton is a file a human reads first. It must not arrive as a
    /// wall of live config: with the comments left alone, it declares
    /// nothing at all.
    ///
    /// This is the other half of the convention. If prose and config were
    /// not distinguishable, one of these two tests would always fail.
    #[test]
    fn the_skeleton_as_written_declares_no_apps() {
        let parsed = Flockfile::parse(&skeleton(), FlockFormat::Toml)
            .expect("the skeleton as written is still valid TOML, just empty");

        assert!(
            parsed.apps.is_empty(),
            "shep init must not drop a live app into a fresh Flockfile; \
             everything starts commented out"
        );
    }

    /// No em dashes or en dashes in anything a user reads. The skeleton is
    /// about as user-facing as a file gets: it is written into their repo.
    #[test]
    fn the_skeleton_carries_no_em_dashes() {
        let text = skeleton();
        assert!(!text.contains('\u{2014}'), "em dash in the skeleton");
        assert!(!text.contains('\u{2013}'), "en dash in the skeleton");
    }
}
