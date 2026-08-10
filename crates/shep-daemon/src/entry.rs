//! Process entry: metadata + lifecycle state for one managed sheep instance

use core::time::Duration;
use std::path::PathBuf;

use shep_core::{config::ResolvedApp, status::ProcStatus};

use crate::privilege::Credentials;

/// Lifecycle state of one managed process instance
#[derive(Debug, Clone)]
pub struct ProcessEntry {
    /// Globally unique ID (assigned at spawn registration)
    pub id: u32,
    /// Resolved application spec
    pub spec: ResolvedApp,
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
    /// Reload state machine (None, SpawningReplacement, or Draining)
    ///
    /// Written at registration and never read: reload execution is deferred,
    /// and this field is the data half landing ahead of it. The expectation
    /// below is what keeps that honest rather than silent — it fires the day
    /// a reload path reads this, and the attribute goes with it.
    #[expect(
        dead_code,
        reason = "data-only ahead of reload execution; the reader lands with it"
    )]
    pub reload: ReloadState,
    /// Resolved once at the initial `Start` and reused for every later
    /// respawn — never re-resolved, so a restart never re-touches the
    /// passwd database (see [`crate::privilege::resolve`]).
    pub credentials: Option<Credentials>,
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

/// Reload state machine for graceful reload scenarios
///
/// A reload runs two [`ProcessEntry`] records at once — the drainee (old,
/// going away) and the replacement (new) — and the two non-`None` variants
/// split across them rather than sharing one: [`Self::SpawningReplacement`]
/// lives on the drainee, [`Self::Draining`] lives on the replacement. Each
/// variant's field names the *other* entry, which is what makes the pair
/// navigable in both directions: from the drainee, `new_id` says who is
/// replacing it; from the replacement, `old_pid` says who it must outlive.
/// See the two variants for the full split.
///
/// Data only: nothing constructs the two non-`None` variants yet, because
/// reload execution is deferred. `allow` rather than `expect` because this
/// module's own tests do construct them, so the expectation would be
/// fulfilled in the lib build and unfulfilled in the test build.
#[allow(
    dead_code,
    reason = "data-only ahead of reload execution; the constructors land with it"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadState {
    /// Not in a reload sequence
    None,
    /// Spawning replacement instance for graceful reload
    ///
    /// Lives on the **drainee's** entry: the instance being replaced is what
    /// names its replacement, not the other way around. Sibling to
    /// [`ProcStatus::Stopping`] on that same entry's `status`, which is what
    /// every guard that only reads `status` sees; this variant is what says
    /// why, and who is coming to take its place.
    SpawningReplacement {
        /// [`ProcessEntry::id`] of the new replacement instance — an entry
        /// ID, not an OS `pid` (contrast [`Self::Draining`]'s `old_pid`,
        /// which is a `pid`): the replacement is looked up by entry, and
        /// only gains an OS pid once it is actually spawned.
        new_id: u32,
    },
    /// Draining connections before terminating old instance
    ///
    /// Lives on the **replacement's** entry, pointing back at the drainee it
    /// must outlive: the replacement cannot be considered the reload's
    /// success until the drainee it names has actually gone. That drainee's
    /// own entry is the one carrying `status = `[`ProcStatus::Stopping`]` —
    /// a different record from this one, set in the same logical transition
    /// but not the same struct.
    Draining {
        /// OS process ID of the instance being drained
        old_pid: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app_with, armed_entry, test_paths};

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

    #[test]
    fn reload_state_is_data_only() {
        let none = ReloadState::None;
        let spawning = ReloadState::SpawningReplacement { new_id: 42 };
        let draining = ReloadState::Draining { old_pid: 1234 };

        assert_eq!(none, ReloadState::None);
        assert_eq!(spawning, ReloadState::SpawningReplacement { new_id: 42 });
        assert_eq!(draining, ReloadState::Draining { old_pid: 1234 });
    }

    /// Computes the drainee/replacement pairing exactly the way settled
    /// decision 3 requires, and hands back the two `ReloadState` values
    /// rather than assigning them into `ProcessEntry::reload` itself.
    ///
    /// That field is `#[expect(dead_code)]`-guarded specifically because
    /// nothing reads it yet — the reader lands with the state machine in
    /// Task 5, not here. The guard turns out to be stricter than "nothing
    /// reads it": even a field *write* through `entry.reload = ...`
    /// assignment (as opposed to a struct-literal initializer, which is how
    /// every existing call site sets it) is enough to flip the lint
    /// expectation and break the guard Step 2 confirmed is still needed —
    /// confirmed empirically while writing this test. So this helper
    /// touches only `status`, which already has other readers, and leaves
    /// `.reload` alone entirely.
    ///
    /// Not the reload state machine — this task does not implement one —
    /// but a rehearsal, local to the test module, of the one invariant that
    /// machine must respect: which entry gets which variant, and that the
    /// two are not the same entry.
    fn pair_for_reload(
        drainee: &mut ProcessEntry,
        replacement: &ProcessEntry,
    ) -> (ReloadState, ReloadState) {
        drainee.status = ProcStatus::Stopping;
        let names_replacement = ReloadState::SpawningReplacement {
            new_id: replacement.id,
        };
        let names_drainee = ReloadState::Draining {
            old_pid: drainee
                .pid
                .expect("drainee must carry a pid to be worth draining"),
        };
        (names_replacement, names_drainee)
    }

    /// Fails if `pair_for_reload` is mutated to swap which entry gets
    /// `SpawningReplacement` vs. `Draining` (settled decision 3 inverted:
    /// the drainee names its replacement, the replacement points back at
    /// what it must outlive), or if it is mutated to also set the
    /// replacement's `status` to `Stopping` — the exact bug the earlier,
    /// wrong `Draining` doc encoded, claiming that variant pairs with
    /// `Stopping` on the same entry. It does not: `Stopping` lives on the
    /// drainee, `Draining` lives on the replacement, and those are two
    /// different [`ProcessEntry`] records.
    #[test]
    fn reload_state_pairs_the_drainee_and_its_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(&dir);
        let app = app_with("web", |_| {});

        let mut drainee = armed_entry(1, 0, 1111, app.clone(), &paths);
        let replacement = armed_entry(2, 0, 2222, app, &paths);

        let (names_replacement, names_drainee) = pair_for_reload(&mut drainee, &replacement);

        // The drainee carries the status every guard that only reads
        // `status` sees, and names the entry replacing it.
        assert_eq!(drainee.status, ProcStatus::Stopping);
        assert_eq!(
            names_replacement,
            ReloadState::SpawningReplacement {
                new_id: replacement.id
            }
        );

        // The replacement is not `Stopping` -- `Draining` lives on a
        // different entry than the one carrying `Stopping` -- and it points
        // back at the drainee's OS pid, the thing it must outlive.
        assert_ne!(replacement.status, ProcStatus::Stopping);
        assert_eq!(
            names_drainee,
            ReloadState::Draining {
                old_pid: drainee.pid.expect("drainee must carry a pid")
            }
        );
    }
}
