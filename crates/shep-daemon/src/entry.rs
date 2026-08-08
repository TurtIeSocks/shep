//! Process entry: metadata + lifecycle state for one managed sheep instance

use core::time::Duration;

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
    pub reload: ReloadState,
    /// Resolved once at the initial `Start` and reused for every later
    /// respawn — never re-resolved, so a restart never re-touches the
    /// passwd database (see [`crate::privilege::resolve`]).
    pub credentials: Option<Credentials>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadState {
    /// Not in a reload sequence
    None,
    /// Spawning replacement instance for graceful reload
    SpawningReplacement {
        /// Process ID of the new replacement instance
        new_id: u32,
    },
    /// Draining connections before terminating old instance
    Draining {
        /// OS process ID of the instance being drained
        old_pid: u32,
    },
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

    #[test]
    fn reload_state_is_data_only() {
        let none = ReloadState::None;
        let spawning = ReloadState::SpawningReplacement { new_id: 42 };
        let draining = ReloadState::Draining { old_pid: 1234 };

        assert_eq!(none, ReloadState::None);
        assert_eq!(spawning, ReloadState::SpawningReplacement { new_id: 42 });
        assert_eq!(draining, ReloadState::Draining { old_pid: 1234 });
    }
}
