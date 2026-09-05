//! What the two flock renderings share: role names, the status mapping, and
//! the faces.
//!
//! `shep flock` renders a table through `output/`, and `shep lookout`
//! renders one through ratatui. They must agree about what `online` looks
//! like, and they cannot share code: their colour types come from different
//! crates, and `mod lookout` is `#[cfg(unix)]` while `mod output` is not.
//!
//! So this module is the only place that defines a face or a mapping.
//! Each renderer only binds [`Role`] to its own colour type: `theme.rs` to
//! ratatui's `Color`, `output/` to `anstyle::Style`.

use shep_core::status::ProcStatus;

/// A colour role, named for the meadow rather than for the colour, so the
/// 256-colour and 16-colour tiers can differ without renaming anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Healthy: a sheep that is up.
    Meadow,
    /// In between: coming up, or waiting to.
    Butter,
    /// Wrong: errored.
    Bark,
    /// Quiet: stopped, stopping, and every muted chrome element.
    Ink3,
}

/// Which role a status wears.
///
/// Lifted verbatim from `lookout/theme.rs`'s own `status()`, which shipped
/// first. Both renderers now read this, so they agree by construction rather
/// than by two people remembering the same thing.
pub(crate) const fn role_of(status: ProcStatus) -> Role {
    match status {
        ProcStatus::Online => Role::Meadow,
        ProcStatus::Starting | ProcStatus::WaitingRestart => Role::Butter,
        ProcStatus::Errored => Role::Bark,
        ProcStatus::Stopping | ProcStatus::Stopped => Role::Ink3,
    }
}

/// The sheep wearing that status.
///
/// Five columns, ASCII, and mutually distinct: five for the table's
/// column budget, ASCII since an emoji is double-width and cannot take a
/// foreground colour, distinct since a face that only differs by colour
/// tells a `NO_COLOR` reader nothing.
///
/// Read by `output::rows::FlockRows`'s STATUS cell. `lookout`'s flock
/// pane colours the status word instead of growing its own face; this is
/// the only place a face or a status-to-role mapping is defined.
pub(crate) const fn face(status: ProcStatus) -> &'static str {
    match status {
        ProcStatus::Online => "(o.o)",
        ProcStatus::Starting => "(o~o)",
        // A sheep waiting to be picked back up must read differently from
        // one coming up fresh at a glance.
        ProcStatus::WaitingRestart => "(>_<)",
        ProcStatus::Stopping | ProcStatus::Stopped => "(-.-)",
        ProcStatus::Errored => "(x.x)",
    }
}

/// What a STATUS cell reports: the lifecycle status the shepherd holds, or
/// the one fact that overrides it.
///
/// `ProcStatus` answers whether the process is alive. For a dog, alive is
/// not enough: a dog is a peer as well as a process, and one that has
/// never completed a handshake is not doing its job however alive it is.
/// [`Self::Silent`] is that fact.
///
/// Not a seventh `ProcStatus` variant: `ProcStatus` is the wire contract
/// for what the supervisor knows about a process, and silence is a fact
/// about a connection that only a reader joining two fields can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reported {
    /// What the shepherd's own lifecycle state says.
    Live(ProcStatus),
    /// A dog whose process is up and which has never answered this
    /// shepherd.
    Silent,
}

impl Reported {
    /// What one row reports, from the two fields that decide it.
    ///
    /// Only `Online` is overridden: every other status is already
    /// honest about a dog not answering, since `starting` covers the
    /// moment every dog is silent right after spawn, and the rest
    /// describe a process not there to answer.
    ///
    /// `handshook` is `None` for a sheep, and for a listing from a
    /// shepherd that predates the field: both render the same as
    /// `Some(true)`.
    pub(crate) const fn of(status: ProcStatus, handshook: Option<bool>) -> Self {
        match (status, handshook) {
            (ProcStatus::Online, Some(false)) => Self::Silent,
            _ => Self::Live(status),
        }
    }

    /// The word this cell shows.
    ///
    /// A `String` because [`ProcStatus`]'s own spelling lives in its
    /// `Display` impl, in shep-core, and restating those six words here to
    /// hand back a `&'static str` would be a second source for the wire
    /// contract's own vocabulary.
    pub(crate) fn word(self) -> String {
        match self {
            Self::Live(status) => status.to_string(),
            // Matches `dogs.rs`'s own `silent_dogs` population name, so
            // a reload's report and a flock listing agree.
            Self::Silent => "silent".to_string(),
        }
    }

    /// The sheep wearing it. Same five-column ASCII rule as [`face`].
    pub(crate) const fn face(self) -> &'static str {
        match self {
            Self::Live(status) => face(status),
            // Not `(o.o)`: that would undo the fix this type exists for.
            // Not `(x.x)` either: this process has not failed, only gone
            // unheard.
            Self::Silent => "(?_?)",
        }
    }

    /// The colour role it wears.
    pub(crate) const fn role(self) -> Role {
        match self {
            Self::Live(status) => role_of(status),
            // Not `Bark`: the process has not failed, and painting it
            // the same red as a crash would send an operator looking for
            // one.
            Self::Silent => Role::Butter,
        }
    }
}

/// The paragraph a `silent` row owes the reader, or `None` when the row is
/// not silent.
///
/// `silent` is the only STATUS a table can show that an operator cannot
/// act on from the word alone, so this supplies the consequence in the
/// per-entity view.
///
/// Never says why the dog is silent: this shepherd cannot see the
/// evidence, which lives only in the dog's own log. Every arm points at
/// `shep bleats` instead of guessing.
pub(crate) fn silence_note(
    name: &str,
    reported: Reported,
    dog_stale: Option<bool>,
) -> Option<String> {
    if reported != Reported::Silent {
        return None;
    }
    let budget = shep_daemon::dogs::DOG_SILENCE_BUDGET.as_secs();
    Some(match dog_stale {
        // Transient: the dog was spawned recently and has not dialled
        // back yet. Future tense, so it does not read as a fault.
        Some(false) => format!(
            "silent  `{name}`'s process is up and it has never answered this shepherd. \
             After {budget}s of that, shep restarts a dog once from the binary on disk; if \
             the restarted dog stays silent shep gives up and says so here. \
             `shep bleats {name}` shows what the dog itself says about connecting."
        ),
        // Loud about the give-up: nothing else will happen on its own.
        Some(true) => format!(
            "silent  `{name}`'s process is up and this shepherd has GIVEN UP on it: the one \
             restart it earned did not help, so it will not be restarted again and nothing \
             more will happen on its own. shep wrote what it saw at that moment into this \
             dog's own log -- run `shep bleats {name}` and read it, because that line names \
             what shep observed and this listing cannot. Three different faults arrive here \
             looking identical: a dog that never reached the socket, one that reaches it and \
             never names itself, and one this shepherd turned away on protocol skew. What to \
             do about them differs, so the log is the surface that says."
        ),
        // Predates the field: says so rather than guessing which arm
        // above applies.
        None => format!(
            "silent  `{name}`'s process is up and it has never answered this shepherd. The \
             shepherd answering this listing is too old to say whether it has given up on the \
             dog or is still waiting for it -- run `shep bleats {name}` for the dog's own \
             account, and `shep daemon reload` to bring the running shepherd up to the shep \
             that is installed, which can answer."
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status has a face and a role. A `match` would catch a missing
    /// arm at compile time; this catches a face that is empty or the wrong
    /// width, which compiles fine and looks broken.
    #[test]
    fn every_status_has_a_five_column_face() {
        for status in [
            ProcStatus::Online,
            ProcStatus::Starting,
            ProcStatus::WaitingRestart,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
        ] {
            let face = Reported::Live(status).face();
            assert_eq!(
                face.chars().count(),
                5,
                "{status} face {face:?} must be 5 columns; the table budget assumes it"
            );
            assert!(
                face.is_ascii(),
                "{status} face {face:?} must be ASCII: an emoji is \
                 double-width, inconsistently so, and cannot take a colour"
            );
        }

        // The sixth face, which no `ProcStatus` reaches: it is decided by
        // `handshook` rather than by a lifecycle state, so the loop above
        // cannot enumerate it and it would otherwise be the one face
        // nothing measured.
        let silent = Reported::Silent.face();
        assert_eq!(silent.chars().count(), 5, "silent face {silent:?}");
        assert!(silent.is_ascii(), "silent face {silent:?}");
    }

    /// fails if a silence stops overriding `online`, or starts overriding a
    /// status that was already honest about it.
    #[test]
    fn a_silence_overrides_online_and_nothing_else() {
        assert_eq!(
            Reported::of(ProcStatus::Online, Some(false)),
            Reported::Silent
        );
        for status in [
            ProcStatus::Starting,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
        ] {
            assert_eq!(Reported::of(status, Some(false)), Reported::Live(status));
        }
        // A dog that is talking, and a sheep or an older shepherd's row.
        assert_eq!(
            Reported::of(ProcStatus::Online, Some(true)),
            Reported::Live(ProcStatus::Online)
        );
        assert_eq!(
            Reported::of(ProcStatus::Online, None),
            Reported::Live(ProcStatus::Online)
        );
    }

    /// fails if `silent` stops being one word, or stops being the word the
    /// shepherd's own `silent_dogs` population is named for.
    #[test]
    fn a_silent_row_says_silent_in_butter() {
        assert_eq!(Reported::Silent.word(), "silent");
        assert_eq!(Reported::Silent.role(), Role::Butter);
        assert_eq!(
            Reported::Live(ProcStatus::Online).word(),
            ProcStatus::Online.to_string(),
            "a live row still spells its status exactly as the wire does"
        );
    }

    /// fails if a row that is not silent picks up a paragraph explaining a
    /// word it does not show. Every `ProcStatus` reads honestly on its own
    /// (`Reported::of`'s own doc says why), so a note under one of those
    /// would be an explanation of nothing.
    #[test]
    fn only_a_silent_row_gets_a_note() {
        for status in [
            ProcStatus::Online,
            ProcStatus::Starting,
            ProcStatus::WaitingRestart,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
        ] {
            // Every latch value, because a live row must stay quiet
            // regardless of what the shepherd reports about dogs.
            for stale in [None, Some(false), Some(true)] {
                assert_eq!(
                    silence_note("web", Reported::Live(status), stale),
                    None,
                    "status={status:?} dog_stale={stale:?}"
                );
            }
        }
        assert!(silence_note("log-rotate", Reported::Silent, Some(false)).is_some());
    }

    /// fails if the three latch states collapse into one sentence: they
    /// are three different situations with three different next steps.
    #[test]
    fn the_three_silences_read_differently_and_all_lead_somewhere() {
        let notes: Vec<String> = [None, Some(false), Some(true)]
            .into_iter()
            .map(|stale| silence_note("log-rotate", Reported::Silent, stale).expect("a silent row"))
            .collect();

        for note in &notes {
            assert!(note.contains("log-rotate"), "names the dog: {note}");
            assert!(
                note.contains("shep bleats log-rotate"),
                "and sends the reader to the one file that holds the evidence: {note}"
            );
            assert!(
                note.starts_with("silent"),
                "labelled with the word it explains: {note}"
            );
        }

        let mut distinct = notes.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            notes.len(),
            "three states, three sentences: {notes:?}"
        );
    }

    /// fails if the give-up stops being the loud part: it has to say the
    /// shepherd has stopped and that nothing further will happen.
    ///
    /// Must not name a cause: three different faults reach this arm and
    /// look identical from a listing, including one that did answer this
    /// shepherd, so naming one would be wrong for the others.
    #[test]
    fn the_given_up_note_says_the_shepherd_has_stopped_trying() {
        let note = silence_note("log-rotate", Reported::Silent, Some(true)).expect("a silent row");
        assert!(note.contains("GIVEN UP"), "{note}");
        assert!(note.contains("will not be restarted again"), "{note}");
        assert!(note.contains("shep bleats log-rotate"), "{note}");
        // The listing cannot tell the three faults apart, so it must not
        // imply it can. A refused dog reaches this arm too.
        assert!(
            !note.contains("never answered"),
            "a refused dog did answer: {note}"
        );
        assert!(
            !note.contains("reinstalling"),
            "this surface cannot say whether reinstalling helps: {note}"
        );
    }

    /// The mapping is the one `lookout` already shipped. Changing it here
    /// changes both renderings, which is the point of this module existing.
    #[test]
    fn the_roles_match_what_lookout_already_showed() {
        assert_eq!(role_of(ProcStatus::Online), Role::Meadow);
        assert_eq!(role_of(ProcStatus::Starting), Role::Butter);
        assert_eq!(role_of(ProcStatus::WaitingRestart), Role::Butter);
        assert_eq!(role_of(ProcStatus::Errored), Role::Bark);
        assert_eq!(role_of(ProcStatus::Stopping), Role::Ink3);
        assert_eq!(role_of(ProcStatus::Stopped), Role::Ink3);
    }

    /// Distinct across these five. `Stopping` is left out: it shares
    /// `Stopped`'s `(-.-)` on purpose, so testing it here would fail the
    /// thing this test exists to catch.
    #[test]
    fn the_faces_are_distinct_from_one_another() {
        let faces = [
            face(ProcStatus::Online),
            face(ProcStatus::Starting),
            face(ProcStatus::WaitingRestart),
            face(ProcStatus::Stopped),
            face(ProcStatus::Errored),
            // `Silent` is not a `ProcStatus` and so cannot be reached by the
            // loop above, but it is a sixth face in the same column of the
            // same table and has to be distinct from all five.
            Reported::Silent.face(),
        ];
        // `dedup` is a `Vec` method, not a slice one, so the faces are
        // collected into a `Vec` first.
        let mut seen = faces.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            faces.len(),
            "each state needs its own face: {faces:?}"
        );
    }
}
