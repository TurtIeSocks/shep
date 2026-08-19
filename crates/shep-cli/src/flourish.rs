//! Sheep for the moments with nothing else to look at.
//!
//! Three places only: an empty flock, a flock entirely stopped, and `shep
//! muster`. Never on an error and never after a destructive verb --
//! `docs/terminology.md`'s rule is that the theme never costs clarity, and a
//! sheep beside `error[not_found]` makes a failure harder to read and reads
//! as flippant to someone debugging at 2am.
//!
//! Uncoloured, deliberately. `output::paint::style_for`'s colour would
//! repeat information the words beside it already give -- unlike the STATUS
//! column, where one table mixes several statuses and colour is the only
//! thing separating them at a glance. `welcome.rs`, the one other place shep
//! draws ASCII, makes the same call for the same reason.
//!
//! [`mustered`] is built from the real status of every restored sheep,
//! never from a bare count. A first version rendered `ProcStatus::Online`
//! unconditionally regardless of what actually came back, so a `shep
//! muster` against an already-stopped roll -- a stopped sheep stays a
//! member of the flock across a restart, so restoring it does not start it
//! -- printed grazing faces and "back on their feet" directly beneath a
//! table saying `stopped`. Caught in review against a real
//! mustered-while-stopped flock, not by any test here, because every test
//! up to that point only ever fed the function a bare `n`. [`empty_flock`]
//! and [`all_asleep`] still take a bare count: both describe a listing that
//! is, by construction, uniformly one status before either is ever called
//! (`commands::query::sheep_flourish` only reaches them once it has
//! already confirmed that), so there is no mix for either to get wrong.

use shep_core::status::ProcStatus;

use crate::vocabulary::face;

/// The most sheep any flourish draws, so `shep muster` over forty processes
/// does not paint a field.
const MOST: usize = 5;

/// The gap between two faces on the same row.
///
/// Shoulder to shoulder (no gap at all) reads as noise rather than as
/// several sheep -- correction 1 caught this by rendering `(-.-)(-.-)(-.-)`
/// and finding it illegible. `welcome.rs`'s own art separates its three
/// sheep by three spaces; this matches it rather than inventing a second
/// convention for the same picture.
const GAP: &str = "   ";

/// Every status, in the order a flourish shows them: the reassuring one
/// first, then the transient ones, then rest, then the bad one.
/// [`mustered_faces`] and [`mustered_caption`] both walk statuses in this
/// order, so a mixed muster's row and its caption always name statuses in
/// the same order as each other, not only agree with the table.
const STATUS_ORDER: [ProcStatus; 6] = [
    ProcStatus::Online,
    ProcStatus::Starting,
    ProcStatus::WaitingRestart,
    ProcStatus::Stopping,
    ProcStatus::Stopped,
    ProcStatus::Errored,
];

/// Joins already-resolved `faces` into one row, indented five columns to
/// match `welcome.rs`'s own left margin, each face separated by [`GAP`].
///
/// Five, not four: `welcome.rs`'s own `ART` draws its leftmost sheep's face
/// row (`( o.o )`) five columns in -- verified by rendering the two
/// together rather than assumed, since `ART`'s own lines are not a uniform
/// block (they range from three to seven columns of leading whitespace
/// across the picture) and only the face row's own margin is the one this
/// function means to match.
///
/// The shared low-level builder: [`row`] calls it for the uniform case,
/// [`mustered`] for the mixed one, so the margin and the gap are each
/// spelled once.
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
/// Names the way out (`shep start`) because this state exists at the exact
/// moment an operator is asking "what now" -- a face with nothing beside it
/// would answer a question nobody asked.
///
/// Its only real caller, `commands::query::sheep_flourish`, lives in
/// `commands/`, which is `#[cfg(unix)]`-gated in `main.rs` -- same reason
/// `output::Streams::out` carries the same attribute, so
/// `#[cfg_attr(windows, allow(dead_code))]` says so explicitly rather than
/// leaving a Windows `cargo check` to report it unprompted.
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
/// `count` is the caller's own count of sheep (dogs excluded, and
/// `Stopping` -- a shutdown in progress, not a flock at rest -- excluded
/// too); see `commands::query::sheep_flourish` for exactly what qualifies.
///
/// Its only real caller lives in `commands/`, unix-only -- see
/// [`empty_flock`]'s own doc for why this carries the same
/// `#[cfg_attr(windows, allow(dead_code))]`.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn all_asleep(count: usize) -> String {
    format!(
        "\n{}\n    {count} in the flock, all asleep\n    `shep start <name>` wakes one\n\n",
        row(ProcStatus::Stopped, count)
    )
}

/// How many of `statuses` carry each status, in [`STATUS_ORDER`] --
/// skipping any status nothing in `statuses` carries, so a uniform restore
/// produces exactly one entry and [`mustered_caption`] can tell "uniform"
/// from "mixed" by the length of this list alone.
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
/// round-robin across the present statuses rather than a plain truncation.
/// A plain truncation of more than [`MOST`] sheep could show five grazing
/// faces for a restore that was mostly stopped, merely because the online
/// ones happened to sort first -- round-robin keeps every present status
/// visible under the cap instead. A uniform restore round-robins over one
/// group, which is the same picture [`row`] draws directly.
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

/// The mustered caption: one honest sentence for a uniform restore --
/// `Online` is the only status that earns "back on their feet"; every
/// other single status says plainly what it is instead of implying
/// success. A mixed restore gets a breakdown, `total` counted from every
/// restored sheep rather than merely the faces [`mustered_faces`] had room
/// to draw, so the numbers stay honest under [`MOST`]'s cap too.
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

/// The flock is back -- or however much of it actually came back.
///
/// Built from `statuses`, the real [`ProcStatus`] of every sheep
/// `Response::Mustered` named, never from a bare count: a stopped sheep
/// stays a member of the flock across a restart, so `shep muster` against
/// an already-stopped roll restores it without starting it, and the
/// flourish has to say that rather than claim the opposite. Every face
/// drawn here comes from [`face`], the same function the STATUS column
/// itself reads, so a face here can never say something that column would
/// disagree with.
///
/// # Panics
/// If `statuses` is empty. Its only caller, `commands::muster::muster`,
/// already checked `count > 0` before reaching this -- an empty
/// `Mustered` gets `emit_notice`'s "the muster roll restored nothing"
/// instead, never this function. The same class of loud, invariant-guard
/// panic `output::table::render_table`'s own `#[track_caller]` doc
/// describes for a row/header arity mismatch: better this than a
/// nonsensical "0 restored, still at rest" caption two lines down.
///
/// Its only real caller, `commands::muster::muster`, lives in `commands/`,
/// unix-only -- see [`empty_flock`]'s own doc for why this carries the same
/// `#[cfg_attr(windows, allow(dead_code))]`.
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

    /// Capped, so `shep muster` over forty processes does not paint a
    /// field -- checked against a uniform restore, the case the original
    /// bare-`usize` API covered.
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

    /// A capped mix (more sheep than [`MOST`] can draw) must still show
    /// every status actually present, not just whichever one filled the
    /// first five slots -- and the caption's own numbers must stay the
    /// real, uncapped counts.
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

    /// The empty state exists to answer "what now", so it must say.
    #[test]
    fn the_empty_flock_names_the_next_command() {
        let art = empty_flock();
        assert!(art.contains("shep start"), "{art}");
    }

    /// The bug this fix round exists for: a muster that restored an
    /// already-stopped flock must never claim they are up.
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

    /// The mirror case: a muster that restored a genuinely running flock
    /// keeps the cheerful line, since here it is true.
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

    /// A genuinely mixed restore must show both faces and name the real
    /// split, never collapse to one status's story.
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

    /// No em dashes in copy a user reads.
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

    /// Every line inside 80 columns, like the welcome -- including a
    /// realistic mixed caption, not only the uniform ones.
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
