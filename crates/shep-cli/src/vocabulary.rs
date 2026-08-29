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
            let face = face(status);
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

    /// Every face must be distinct, or a face carries nothing the colour
    /// did not.
    #[test]
    fn the_faces_are_distinct_from_one_another() {
        let faces = [
            face(ProcStatus::Online),
            face(ProcStatus::Starting),
            face(ProcStatus::WaitingRestart),
            face(ProcStatus::Stopped),
            face(ProcStatus::Errored),
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
