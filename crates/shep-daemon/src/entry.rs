//! Process entry: metadata + lifecycle state for one managed sheep instance

use core::time::Duration;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use shep_core::{
    config::ResolvedApp,
    protocol::{DogSource, ExitInfo},
    status::ProcStatus,
};

use crate::privilege::SpawnIdentity;

/// Lifecycle state of one managed process instance
#[derive(Debug, Clone)]
pub struct ProcessEntry {
    /// Globally unique ID (assigned at spawn registration)
    pub id: u32,
    /// Resolved application spec
    pub spec: ResolvedApp,
    /// The config a file load left for this sheep's next spawn.
    ///
    /// `None` for every sheep outside the window between a load that changed
    /// a `NeedsRespawn` field and the restart that picks it up. `spec` keeps
    /// describing what the running child was spawned from, which is the only
    /// account of that anywhere; overwriting it would erase it.
    ///
    /// A whole config, and not the same thing as the `pending` list on a
    /// load's per-app report, which shares its name: that one is field NAMES
    /// for an operator to read and includes fields already on `spec`. This is
    /// the config a respawn promotes.
    pub pending: Option<ResolvedApp>,
    /// Whether the config in [`Self::pending`] changes who this sheep runs
    /// as, so that promoting it must re-resolve [`Self::credentials`].
    ///
    /// Recorded by the load that PARKED the config, against the spec this
    /// entry held at that moment, and not recomputed later. A promotion can
    /// only diff `pending` against `spec`, and `spec` is not a fixed point:
    /// a load writes one spec, derived from the app's first instance, onto
    /// every instance of the name. Restart instance 0 alone -- a crash, a
    /// memory breach and a liveness failure all do that with nobody asking
    /// -- and the next load reads its promoted spec as the base for its
    /// siblings, so the `user` change instance 1 has still not applied is no
    /// longer visible as a difference to instance 1. Answering the question
    /// while the answer is still there is what makes it survive that.
    ///
    /// STICKY, for the same reason: it stays set through every later load
    /// until [`Self::pending`] is actually promoted, because a second load
    /// that changes nothing about identity has not undone the first one's
    /// change. Cleared in exactly one place, the promotion itself.
    ///
    /// `false` whenever [`Self::pending`] is `None`, which is every sheep
    /// outside a parking window.
    pub pending_reidentifies: bool,
    /// Instance number within the app (for clustered apps, 0..instances-1)
    pub instance: u32,
    /// Current lifecycle status
    pub status: ProcStatus,
    /// OS process ID (None if not running)
    pub pid: Option<u32>,
    /// Count of respawns performed (initial spawn is NOT a restart)
    pub restarts: u32,
    /// Time process started (None if not running; paused-clock-aware tokio::time::Instant)
    pub started_at: Option<tokio::time::Instant>,
    /// Restart budget and stability tracking
    pub budget: RestartBudget,
    /// Which half of a reload this entry is, if any.
    ///
    /// `None` for an ordinary instance, which is every entry outside the few
    /// seconds a reload of its app is in flight. The supervisor reads this on
    /// two paths that would otherwise get a reload's entries badly wrong — a
    /// readiness wait resolving, and an exit being decided — so the variant
    /// docs below are what those paths are keying on.
    pub reload: ReloadState,
    /// The identity this entry's next spawn runs under.
    ///
    /// Resolved once — at the initial `Start` for an entry that got one, at
    /// the first spawn otherwise — and reused for every later respawn, so a
    /// restart neither re-touches the passwd database nor changes a running
    /// app's identity underneath it (see [`crate::privilege::resolve`]).
    /// There is exactly one exception, two paragraphs down, and it is an
    /// operator asking for the change rather than an accident.
    ///
    /// [`SpawnIdentity::Unresolved`] until that happens, and that is a
    /// different fact from an app that asked for no user at all: an entry
    /// registered at rest from the muster roll, or registered `Errored`
    /// because its `user` could not be resolved, has never been looked up,
    /// and starting it as the shepherd would be a silent privilege
    /// downgrade rather than the configured behaviour.
    ///
    /// One exception, and only one: a config load that parks a `user` or
    /// `group` change sets [`Self::pending_reidentifies`], and the promotion
    /// of that config puts this back to [`SpawnIdentity::Unresolved`] so the
    /// spawn it precedes resolves the new name. That is an operator asking
    /// for the change on purpose, which is the case the once-only rule was
    /// never protecting against; every other promoted field keeps the
    /// resolved value and costs no passwd lookup.
    pub credentials: SpawnIdentity,
    /// Where this instance's stdout is appended, copied from the
    /// [`SpawnSpec`](crate::runner::SpawnSpec) that
    /// [`assemble`](crate::assemble::assemble) built.
    ///
    /// Carried here so the wire-facing `ProcessInfo` can report it without
    /// re-deriving it: only the assembler knows whether the app set an
    /// explicit `out_file` or takes the `merge_logs`-dependent default, and
    /// a second copy of that rule would be free to drift out of agreement
    /// with the path the child is actually writing to. `spec` and
    /// `instance` never change after registration, so neither does this.
    pub out_file: PathBuf,
    /// Where this instance's stderr is appended, resolved exactly as
    /// [`Self::out_file`].
    pub err_file: PathBuf,
    /// Set when this entry is a dog, naming where the dog came from.
    ///
    /// A MARKER, and deliberately not a second registry: reload, watch,
    /// cron, the memory ceiling, the log plane and the muster roll all
    /// supervise a dog exactly as they supervise a sheep, and a field is
    /// what keeps that true. It is read where the question is *where did
    /// this come from* (a listing's source column) or *who should see this*
    /// (a wildcard selector, a flock table). It is never read to decide how
    /// a process is supervised — a different kill ladder, backoff curve or
    /// restart budget keyed on this field is the signal that the separate
    /// registry should have been built instead.
    pub dog: Option<DogSource>,
    /// How this instance's process most recently stopped existing.
    ///
    /// Set unconditionally by `Actor::handle_exited` — the one place a
    /// process under a registered id stops existing — for every exit,
    /// including an operator's own `stop`/`delete`: the process still
    /// genuinely stopped, and that stays true information regardless of who
    /// asked for it. `None` for an entry that has never exited under this
    /// daemon (a fresh [`Self::id`] from `Actor::spawn_fresh` or
    /// `Actor::register_at_rest` — both private to this crate, so named in
    /// code font rather than linked).
    ///
    /// Survives a respawn on purpose: `Actor::respawn` mutates this same
    /// entry in place and never touches this field, so it keeps answering
    /// "why did this instance last stop" through the
    /// instance's next run, not just while it is down. A reload's
    /// replacement entry (`spawn_replacement`) copies it from the drainee it
    /// replaces for the same reason `restarts` and `dog` do — the
    /// replacement is the same instance continuing, not a new one.
    pub last_exit: Option<ExitInfo>,
}

/// Restart budget and consecutive-unstable-exit tracking
#[derive(Debug, Clone, Default)]
pub struct RestartBudget {
    /// Number of consecutive unstable exits (private: use note_exit to update)
    unstable_count: u32,
}

/// Stability classification for a process exit
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stability {
    /// Uptime >= min_uptime (exit was healthy)
    Stable,
    /// Uptime < min_uptime (exit was unhealthy)
    Unstable,
}

impl RestartBudget {
    /// Record an exit and classify it as stable or unstable.
    ///
    /// Stable exits reset the counter to 0; unstable exits increment it.
    pub fn note_exit(&mut self, uptime: Duration, min_uptime: Duration) -> Stability {
        if uptime >= min_uptime {
            self.unstable_count = 0;
            Stability::Stable
        } else {
            self.unstable_count += 1;
            Stability::Unstable
        }
    }

    /// Get the current consecutive-unstable-exit count
    pub fn unstable_count(&self) -> u32 {
        self.unstable_count
    }

    /// Check if the restart budget is exhausted.
    ///
    /// Spec §4: the counter *reaching* `max_restarts` consecutive unstable
    /// exits is what errors the process — i.e. exhausted on the Nth
    /// unstable exit where N = `max_restarts` (N-1 restarts performed).
    pub fn exhausted(&self, max_restarts: u32) -> bool {
        self.unstable_count >= max_restarts
    }

    /// Reset the unstable counter (e.g., after a reload)
    pub fn reset(&mut self) {
        self.unstable_count = 0;
    }
}

/// Which half of a reload's swap a [`ProcessEntry`] is, if either
///
/// A reload runs two entries at once — the drainee (old, going away) and the
/// replacement (new) — and the two non-`None` variants split across them
/// rather than sharing one: [`Self::Drainee`] lives on the drainee,
/// [`Self::Replacement`] lives on the replacement. The variants are named for
/// the ROLE they mark rather than for the phase the job was in when they were
/// written, because they outlive that phase in both directions: a drainee
/// carries its marker through the whole drain, and a replacement carries its
/// own from the moment it is spawned. What the job is doing is `ReloadPhase`'s
/// to say, in `supervisor`.
///
/// Getting from one half to the other belongs to the reload job, and only the
/// drainee's direction is answered here at all: the replacement's
/// back-reference lives on that job, in the entry ids the machinery around it
/// navigates by.
///
/// Serialized because a handover carries it: a successor that installed a
/// drainee or a replacement without this would route that instance's exit to
/// `decide_on_exit` rather than to the reload machinery, which for an
/// `autorestart` app respawns the old code into a slot the replacement owns.
/// `snake_case` on the wire to match `ProcStatus`'s own spelling, the blob's
/// nearest neighbour, since the blob is a JSON file an operator may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadState {
    /// Not half of any swap
    None,
    /// This entry is the instance being replaced
    ///
    /// The instance being replaced is what names its replacement, not the
    /// other way around. Sibling to [`ProcStatus::Stopping`] on this same
    /// entry's `status`, which is what every guard that only reads `status`
    /// sees; this variant is what says why, and who is coming to take its
    /// place.
    Drainee {
        /// [`ProcessEntry::id`] of the new replacement instance — an entry
        /// ID, not an OS `pid`: the replacement is looked up by entry, and
        /// only gains an OS pid once it is actually spawned.
        ///
        /// `None` for the whole of a SERIAL reload's drain, which is the one
        /// arrangement in which this instance is being replaced by something
        /// that does not exist yet: a serial reload empties the instance slot
        /// before it spawns into it, so there is no replacement to name until
        /// this instance's own exit is handled. It is `Some` from the moment
        /// there is an id to put in it, in either mode.
        new_id: Option<u32>,
    },
    /// This entry is the replacement
    ///
    /// Says only that: this record is the half that arrived. The drainee it
    /// must outlive is a different record, carrying `status =
    /// `[`ProcStatus::Stopping`]` and set in the same logical transition —
    /// reachable from the reload job, which is what every caller that needs
    /// it already holds.
    Replacement,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_stable_exit_resets_counter() {
        let mut budget = RestartBudget::default();
        budget.note_exit(Duration::from_secs(1), Duration::from_millis(500));
        assert_eq!(budget.unstable_count(), 0);

        budget.note_exit(Duration::from_millis(100), Duration::from_millis(500));
        assert_eq!(budget.unstable_count(), 1);

        budget.note_exit(Duration::from_secs(5), Duration::from_millis(500));
        assert_eq!(budget.unstable_count(), 0);
    }

    #[test]
    fn budget_unstable_increments_counter() {
        let mut budget = RestartBudget::default();
        for i in 1..=5 {
            budget.note_exit(Duration::from_millis(100), Duration::from_secs(10));
            assert_eq!(budget.unstable_count(), i);
        }
    }

    #[test]
    fn budget_exhausted_at_max_restarts() {
        let mut budget = RestartBudget::default();
        let max = 5;

        // max-1 unstable exits: not yet exhausted.
        for _ in 0..max - 1 {
            budget.note_exit(Duration::from_millis(100), Duration::from_secs(10));
        }
        assert!(!budget.exhausted(max));

        // The max-th unstable exit reaches the budget: exhausted.
        budget.note_exit(Duration::from_millis(100), Duration::from_secs(10));
        assert!(budget.exhausted(max));
    }

    #[test]
    fn budget_reset_clears_counter() {
        let mut budget = RestartBudget::default();
        for _ in 0..10 {
            budget.note_exit(Duration::from_millis(100), Duration::from_secs(10));
        }
        assert_eq!(budget.unstable_count(), 10);

        budget.reset();
        assert_eq!(budget.unstable_count(), 0);
    }
}
