//! What the two flock renderings share: role names, the status mapping, and
//! the faces.
//!
//! `shep flock` renders a table through `output/`, and `shep lookout`
//! renders one through ratatui. They must agree about what `online` looks
//! like, and they cannot share code: their colour types come from different
//! crates, and `mod lookout` is `#[cfg(unix)]` while `mod output` is not.
//!
//! So this module owns the vocabulary and neither renderer decides any of
//! it. Each binds [`Role`] to its own colour type -- `theme.rs` to ratatui's
//! `Color`, `output/` to `anstyle::Style`. A face or a mapping decided
//! anywhere but here is a review defect.

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
/// Five columns each, ASCII, and mutually distinct -- all three pinned by
/// this module's tests. Five because the table's column budget assumes it;
/// ASCII because an emoji is double-width (inconsistently across terminals)
/// and cannot take a foreground colour, which would make the width maths
/// guesswork; distinct because a face that only differs by colour tells a
/// `NO_COLOR` reader nothing.
///
/// Read by `output::rows::FlockRows`'s own STATUS cell, the box-drawn
/// table's one face-bearing column. `lookout`'s own flock pane never grows
/// a face of its own -- it colours the status word instead, and
/// `lookout/theme.rs`'s own module doc explains why (colour is always
/// redundant with the text beside it there, so a face would be a second
/// decoration saying the same thing a second way). A face or a
/// status-to-role mapping defined anywhere but here, in either renderer, is
/// a review defect (this module's own top doc says so first).
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
/// `ProcStatus` answers "is the process alive". For a sheep that is the
/// whole of what STATUS means, and this type collapses to [`Self::Live`].
/// For a dog it is not: a dog is a PEER as well as a process, and one that
/// has never completed a handshake is not doing its job however alive it
/// is. `shep flock` reported such a dog as `(o.o) online` with zero
/// restarts while its own log filled with protocol refusals, which is the
/// case [`Self::Silent`] exists to stop.
///
/// Deliberately not a seventh `ProcStatus` variant: `ProcStatus` is the wire
/// contract for what the supervisor knows about a PROCESS, and silence is a
/// fact about a CONNECTION that only a reader joining two fields can see.
/// A variant would also be a protocol break, where
/// [`ProcessInfo::handshook`](shep_core::protocol::ProcessInfo::handshook)
/// is additive.
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
    /// # Why only `Online` is overridden
    ///
    /// `online` is the one word that lies here. Every other status is
    /// already honest about a dog that is not answering: `starting` says
    /// the relationship is not established yet — and a dog is silent for a
    /// moment every single time it is spawned, so overriding that would
    /// report a fault on every healthy start — while `stopped`, `errored`
    /// and `waiting-restart` describe a process that is not there to
    /// answer. Narrowing to `Online` is what keeps this a correction rather
    /// than a second opinion.
    ///
    /// `handshook` is `None` for a sheep AND for a listing from a shepherd
    /// that predates the field, and both must render exactly as they did
    /// before it existed — see the field's own doc for why collapsing those
    /// two costs nothing.
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
            // One word, matching `dogs.rs`'s own `silent_dogs` /
            // `DOG_SILENCE_BUDGET` / `record_silent_dog`: the shepherd
            // already calls this population silent, and a surface that
            // called it something else would make an operator reading a
            // reload's report and a flock listing think they were two
            // things.
            Self::Silent => "silent".to_string(),
        }
    }

    /// The sheep wearing it. Same five-column ASCII rule as [`face`].
    pub(crate) const fn face(self) -> &'static str {
        match self {
            Self::Live(status) => face(status),
            // Not a happy face, which is the whole point: a status that
            // looks fine while being a problem is the defect being fixed,
            // so `(o.o)` here would undo it. Not `(x.x)` either — that is
            // `errored`, a process that failed, and this process has not.
            // The dog is confused rather than dead, and it is exactly what
            // the operator watching it was.
            Self::Silent => "(?_?)",
        }
    }

    /// The colour role it wears.
    pub(crate) const fn role(self) -> Role {
        match self {
            Self::Live(status) => role_of(status),
            // `Butter`, the "a gap the operator can close" tier
            // `outcome_role` and `source_role` already use, and NOT
            // `Bark`. Bark is reserved for a failure, and painting a
            // running process the same red as a crashed one would send an
            // operator looking for a crash that never happened. The gap
            // here is real and closable — reinstall the dog and restart it
            // — which is what Butter says everywhere else in this crate.
            Self::Silent => Role::Butter,
        }
    }
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

    /// Distinct across these five -- `Stopping` is left out on purpose: it
    /// shares `Stopped`'s `(-.-)` deliberately (quiet is quiet, whichever
    /// direction it's headed), so testing it here would fail the very thing
    /// this test exists to catch. Every other pair must still differ, or a
    /// face carries nothing the colour did not.
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
        // `dedup` is a `Vec` method, not a slice one -- it shrinks the
        // length, which a fixed-size array cannot do -- so the faces are
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
