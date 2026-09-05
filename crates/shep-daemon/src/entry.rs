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
    /// `None` outside the window between a load and the respawn that
    /// promotes it. `spec` still describes what the running child was
    /// spawned from. A whole config, not the field-name list a load's
    /// per-app report also calls `pending`.
    pub pending: Option<ResolvedApp>,
    /// Whether the config in [`Self::pending`] changes who this sheep runs
    /// as, so promoting it must re-resolve [`Self::credentials`].
    ///
    /// Recorded when the load parks the config, against the spec held at
    /// that moment; not recomputed later, since `spec` can be rewritten by
    /// another instance's respawn before this one's own load lands. Stays
    /// set until [`Self::pending`] is promoted. `false` whenever
    /// [`Self::pending`] is `None`.
    pub pending_reidentifies: bool,
    /// The [`AppConfig`](shep_core::config::AppConfig) field names an
    /// operator has set on this sheep that its current Flockfile does not
    /// declare, in field-name order. Empty for a sheep no load has ever
    /// touched.
    ///
    /// Cached rather than read from the override store at listing time,
    /// since `to_info` runs far more often than the store changes. Kept in
    /// sync by every path that registers or carries a sheep: a config load
    /// writes it from the same ledger it persists to the store; a reload
    /// carries it off the drainee; a fresh registration reads
    /// `Actor::overridden_for`. Names only, never values.
    pub overridden: Vec<String>,
    /// Instance number within the app (for clustered apps, 0..instances-1)
    pub instance: u32,
    /// Current lifecycle status
    pub status: ProcStatus,
    /// OS process ID (None if not running)
    pub pid: Option<u32>,
    /// Count of respawns performed (initial spawn is not a restart)
    pub restarts: u32,
    /// Time process started (None if not running; paused-clock-aware tokio::time::Instant)
    pub started_at: Option<tokio::time::Instant>,
    /// Restart budget and stability tracking
    pub budget: RestartBudget,
    /// Which half of a reload this entry is, if any.
    ///
    /// `None` outside the few seconds its app's reload is in flight. Read by
    /// the supervisor when a readiness wait resolves and when an exit is
    /// decided; the variant docs below are what those reads key on.
    pub reload: ReloadState,
    /// The identity this entry's next spawn runs under.
    ///
    /// Resolved once, at the entry's first `Start` or spawn, and reused for
    /// every later respawn, so a restart never re-touches the passwd
    /// database or changes a running app's identity underneath it (see
    /// [`crate::privilege::resolve`]). [`SpawnIdentity::Unresolved`] means
    /// never looked up, not "no user configured": starting it as the
    /// shepherd would be a silent privilege downgrade.
    ///
    /// The one exception: a parked `user`/`group` change
    /// ([`Self::pending_reidentifies`]) resets this to `Unresolved` so the
    /// promotion's spawn resolves the new identity.
    pub credentials: SpawnIdentity,
    /// Where this instance's stdout is appended, copied from the
    /// [`SpawnSpec`](crate::runner::SpawnSpec) that
    /// [`assemble`](crate::assemble::assemble) built.
    ///
    /// Carried here so `ProcessInfo` can report it without re-deriving the
    /// `merge_logs`-dependent default. Fixed at registration.
    pub out_file: PathBuf,
    /// Where this instance's stderr is appended, resolved exactly as
    /// [`Self::out_file`].
    pub err_file: PathBuf,
    /// Set when this entry is a dog, naming where the dog came from.
    ///
    /// A marker only: reload, watch, cron, the memory ceiling, the log
    /// plane and the muster roll supervise a dog exactly as a sheep. Read
    /// to answer where it came from or who should see it, never to decide
    /// how it is supervised.
    pub dog: Option<DogSource>,
    /// How this instance's process most recently stopped existing.
    ///
    /// Set by `Actor::handle_exited` for every exit, including an
    /// operator's own `stop`/`delete`. `None` for an entry that has never
    /// exited under this daemon.
    ///
    /// Survives a respawn: `Actor::respawn` never touches this field, so it
    /// keeps answering "why did this last stop" into the next run. A
    /// reload's replacement entry copies it from the drainee it replaces.
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

    /// Whether the restart budget is exhausted: `unstable_count` reaching
    /// `max_restarts` errors the process on the Nth unstable exit (N-1
    /// restarts performed).
    pub fn exhausted(&self, max_restarts: u32) -> bool {
        self.unstable_count >= max_restarts
    }

    /// Reset the unstable counter (e.g., after a reload)
    pub fn reset(&mut self) {
        self.unstable_count = 0;
    }
}

/// Which half of a reload's swap a [`ProcessEntry`] is, if either.
///
/// [`Self::Drainee`] and [`Self::Replacement`] are named for the role they
/// mark, not the phase the job was in when set, since each outlives that
/// phase. Only the drainee holds a back-reference to its replacement's id;
/// the reverse lives on the reload job itself.
///
/// Serialized: a handover successor that installed an entry without this
/// would route its exit to `decide_on_exit` instead of the reload
/// machinery. `snake_case` to match `ProcStatus`'s wire spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadState {
    /// Not half of any swap
    None,
    /// This entry is the instance being replaced.
    ///
    /// Pairs with `status ==` [`ProcStatus::Stopping`] on the same entry;
    /// this variant says why, and names the replacement.
    Drainee {
        /// [`ProcessEntry::id`] of the new replacement instance, not an OS
        /// `pid`.
        ///
        /// `None` for the whole of a serial reload's drain, since that mode
        /// empties the slot before spawning into it, so there is nothing to
        /// name until this instance's exit is handled.
        new_id: Option<u32>,
    },
    /// This entry is the replacement.
    ///
    /// The drainee it must outlive is a separate record, reachable from the
    /// reload job rather than from here.
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
