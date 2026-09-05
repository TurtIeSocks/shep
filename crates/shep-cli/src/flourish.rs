//! Sheep for the moments with nothing else to look at.
//!
//! Three places only: an empty flock, a flock entirely stopped, and `shep
//! muster`. Never on an error and never after a destructive verb: the theme
//! never costs clarity. Uncoloured, since the words beside a face already
//! carry what colour would.

use shep_core::status::ProcStatus;

use crate::vocabulary::face;

/// The most sheep any flourish draws, so `shep muster` over forty processes
/// does not paint a field.
const MOST: usize = 5;

/// The gap between two faces on the same row.
///
/// Three spaces, matching `welcome.rs`'s own art. Shoulder to shoulder reads
/// as noise rather than as several sheep.
const GAP: &str = "   ";

/// Every status, in the order a flourish shows them: the reassuring one
/// first, then the transient ones, then rest, then the bad one.
/// [`mustered_faces`] and [`mustered_caption`] both walk this order, so a
/// mixed muster's row and its caption name statuses in the same order.
const STATUS_ORDER: [ProcStatus; 6] = [
    ProcStatus::Online,
    ProcStatus::Starting,
    ProcStatus::WaitingRestart,
    ProcStatus::Stopping,
    ProcStatus::Stopped,
    ProcStatus::Errored,
];

/// Joins already-resolved `faces` into one row, indented five columns to
/// match `welcome.rs`'s own face row, each face separated by [`GAP`].
fn faces_row(faces: &[&'static str]) -> String {
    let mut line = String::from("     ");
    for (i, face) in faces.iter().enumerate() {
        if i > 0 {
            line.push_str(GAP);
        }
        line.push_str(face);
    }
    line
}

/// A row of `count` faces, all one `status` (at least one, capped at
/// [`MOST`]).
fn row(status: ProcStatus, count: usize) -> String {
    let n = count.clamp(1, MOST);
    faces_row(&vec![face(status); n])
}

/// Nothing registered yet: `shep flock` with an empty roll.
///
/// Names the way out (`shep start`), since this state is exactly where an
/// operator asks "what now".
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn empty_flock() -> String {
    format!(
        "\n{}\n    no sheep in the flock yet\n    `shep start <script>` adds one\n\n",
        row(ProcStatus::Stopped, 1)
    )
}

/// Registered, every one of them at rest: `shep flock` where `count` sheep
/// are all `Stopped`.
///
/// `count` excludes dogs and `Stopping`; see
/// `commands::query::sheep_flourish` for what qualifies.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn all_asleep(count: usize) -> String {
    format!(
        "\n{}\n    {count} in the flock, all asleep\n    `shep start <name>` wakes one\n\n",
        row(ProcStatus::Stopped, count)
    )
}

/// How many of `statuses` carry each status, in [`STATUS_ORDER`], skipping
/// any status nothing carries, so [`mustered_caption`] tells uniform from
/// mixed by this list's length alone.
fn status_counts(statuses: &[ProcStatus]) -> Vec<(ProcStatus, usize)> {
    STATUS_ORDER
        .into_iter()
        .filter_map(|status| {
            let n = statuses.iter().filter(|&&s| s == status).count();
            (n > 0).then_some((status, n))
        })
        .collect()
}

/// The mustered row's faces: one per restored sheep, its own real status,
/// round-robin across the present statuses so every one stays visible under
/// [`MOST`]'s cap. A plain truncation could show five grazing faces for a
/// restore that was mostly stopped.
fn mustered_faces(counts: &[(ProcStatus, usize)]) -> Vec<&'static str> {
    let mut remaining = counts.to_vec();
    let mut faces = Vec::new();
    while faces.len() < MOST {
        let mut placed_one = false;
        for (status, left) in &mut remaining {
            if faces.len() >= MOST {
                break;
            }
            if *left > 0 {
                faces.push(face(*status));
                *left -= 1;
                placed_one = true;
            }
        }
        if !placed_one {
            break;
        }
    }
    faces
}

/// The mustered caption: one sentence for a uniform restore, a breakdown for
/// a mixed one. Only `Online` earns "back on their feet". `total` counts
/// every restored sheep, not just the faces [`mustered_faces`] had room for.
fn mustered_caption(counts: &[(ProcStatus, usize)], total: usize) -> String {
    if let [(status, _)] = counts {
        return match status {
            ProcStatus::Online => format!("{total} back on their feet"),
            ProcStatus::Stopped => format!("{total} restored, still at rest"),
            ProcStatus::Starting => format!("{total} restored, starting up"),
            ProcStatus::WaitingRestart => format!("{total} restored, waiting to restart"),
            ProcStatus::Stopping => format!("{total} restored, still shutting down"),
            ProcStatus::Errored => format!("{total} restored, errored"),
        };
    }
    let breakdown: Vec<String> = counts
        .iter()
        .map(|(status, n)| format!("{n} {status}"))
        .collect();
    format!("{total} restored: {}", breakdown.join(", "))
}

/// The flock is back, or however much of it actually came back.
///
/// Built from the real [`ProcStatus`] of every sheep `Response::Mustered`
/// named, never a bare count: a stopped sheep stays a flock member across a
/// restart, so `shep muster` restores it without starting it. Faces come
/// from [`face`], the same function the STATUS column reads.
///
/// # Panics
/// If `statuses` is empty. `commands::muster::muster` checks `count > 0`
/// first, and an empty `Mustered` gets `emit_notice` instead.
#[track_caller]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn mustered(statuses: &[ProcStatus]) -> String {
    assert!(
        !statuses.is_empty(),
        "mustered() needs at least one restored sheep; the empty case is emit_notice's job"
    );
    let counts = status_counts(statuses);
    format!(
        "\n{}\n    {}\n",
        faces_row(&mustered_faces(&counts)),
        mustered_caption(&counts, statuses.len())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flourish_never_shows_more_than_five_sheep() {
        for n in [1, 2, 5, 6, 40, 400] {
            let statuses = vec![ProcStatus::Online; n];
            let art = mustered(&statuses);
            let sheep = art.matches(face(ProcStatus::Online)).count();
            assert!(sheep <= 5, "{n} sheep rendered {sheep} faces:\n{art}");
            assert!(sheep >= 1, "at least one, even for {n}:\n{art}");
        }
    }

    /// The caption's numbers stay the real, uncapped counts.
    #[test]
    fn a_capped_mixed_muster_still_shows_every_status() {
        let statuses = [vec![ProcStatus::Online; 3], vec![ProcStatus::Stopped; 4]].concat();
        let art = mustered(&statuses);
        let online = art.matches(face(ProcStatus::Online)).count();
        let stopped = art.matches(face(ProcStatus::Stopped)).count();
        assert_eq!(online + stopped, 5, "the row must still cap at 5:\n{art}");
        assert!(
            online >= 1 && stopped >= 1,
            "both statuses must survive the cap: {online} online, {stopped} stopped:\n{art}"
        );
        assert!(
            art.contains("3 online") && art.contains("4 stopped"),
            "the caption states the real, uncapped counts:\n{art}"
        );
    }

    #[test]
    fn the_empty_flock_names_the_next_command() {
        let art = empty_flock();
        assert!(art.contains("shep start"), "{art}");
    }

    #[test]
    fn a_muster_that_restored_everything_stopped_says_so_not_that_they_woke_up() {
        let art = mustered(&[ProcStatus::Stopped, ProcStatus::Stopped]);
        assert!(
            art.contains(face(ProcStatus::Stopped)),
            "must draw the sleeping face:\n{art}"
        );
        assert!(
            !art.contains(face(ProcStatus::Online)),
            "must not draw the grazing face for a sheep that is not running:\n{art}"
        );
        assert!(
            !art.contains("back on their feet"),
            "must not claim they woke up:\n{art}"
        );
        assert!(
            art.contains("still at rest"),
            "must say what actually happened:\n{art}"
        );
    }

    #[test]
    fn a_muster_that_restored_everything_online_says_they_are_up() {
        let art = mustered(&[ProcStatus::Online, ProcStatus::Online, ProcStatus::Online]);
        assert!(
            art.contains(face(ProcStatus::Online)),
            "must draw the grazing face:\n{art}"
        );
        assert!(
            !art.contains(face(ProcStatus::Stopped)),
            "must not draw a sleeping face nothing here has:\n{art}"
        );
        assert!(art.contains("back on their feet"), "{art}");
    }

    #[test]
    fn a_mixed_muster_shows_both_faces_and_names_the_split() {
        let art = mustered(&[ProcStatus::Online, ProcStatus::Online, ProcStatus::Stopped]);
        assert!(art.contains(face(ProcStatus::Online)), "{art}");
        assert!(art.contains(face(ProcStatus::Stopped)), "{art}");
        assert!(art.contains("2 online"), "{art}");
        assert!(art.contains("1 stopped"), "{art}");
        assert!(
            !art.contains("back on their feet"),
            "a mix is not a uniform success, and must not read as one:\n{art}"
        );
    }

    #[test]
    fn the_flourishes_carry_no_em_dashes() {
        for art in [
            empty_flock(),
            all_asleep(3),
            mustered(&[ProcStatus::Online, ProcStatus::Online]),
            mustered(&[
                ProcStatus::Online,
                ProcStatus::Starting,
                ProcStatus::Stopped,
            ]),
        ] {
            assert!(!art.contains('\u{2014}'), "em dash in {art:?}");
            assert!(!art.contains('\u{2013}'), "en dash in {art:?}");
        }
    }

    #[test]
    fn the_flourishes_fit_an_eighty_column_terminal() {
        for art in [
            empty_flock(),
            all_asleep(5),
            mustered(&[ProcStatus::Online; 5]),
            mustered(&[
                ProcStatus::Online,
                ProcStatus::Online,
                ProcStatus::Starting,
                ProcStatus::Starting,
                ProcStatus::Stopped,
            ]),
        ] {
            for line in art.lines() {
                assert!(line.chars().count() <= 80, "line too wide: {line:?}");
            }
        }
    }
}
