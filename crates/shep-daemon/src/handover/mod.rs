//! Whole-flock handover: whether this daemon's flock can be replaced in
//! place, and (in later tasks) the blob and the exec that carry it.
//!
//! Phase 2a carries only the plainest sheep: no shepherd channel, no stdin,
//! no dog, one instance, no in-flight reload, and nothing an operator has
//! already asked to stop or delete. [`fitness`] is the gate: get it wrong in
//! the permissive direction and a half-built handover corrupts a live
//! flock; get it wrong in the strict direction and the caller merely falls
//! back to the stop-and-start arm that already works. That asymmetry is why
//! an unclear case refuses rather than guesses.

// Nothing in this crate calls `fitness` yet. Task 8 wires it into
// `boot.rs`'s SIGHUP arm; until then, an honestly-unreachable gate is
// better than a stub that pretends to decide something and always answers
// the same way.
#![expect(
    dead_code,
    reason = "task 8 wires fitness into boot.rs's SIGHUP arm; nothing calls it yet"
)]

mod fds;

use crate::entry::ProcessEntry;

/// Whether a flock can be handed over in place, or must fall back to a
/// stop-and-start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fitness {
    /// Every sheep in the flock is carryable by phase 2a.
    Carryable,
    /// At least one sheep is not carryable, and why.
    Refused(RefusedReason),
}

/// Why a flock cannot be handed over in place, and what happens instead.
///
/// Every variant is a feature phase 2a does not yet carry, not an error. The
/// caller falls back to the stop arm, which is correct behaviour rather
/// than a degraded one.
///
/// `#[non_exhaustive]`, unlike [`crate::boot::Shepherd`]: that enum is
/// closed by its mechanism (a pidfile lock is either free, held-with-pid or
/// held-without, and there is no fourth state). This one is closed by
/// nothing but how much of the handover has shipped. 2b and 2c each widen
/// what phase 2a refuses today into something a later phase carries, so a
/// match here must keep tolerating a variant this module has not named yet.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefusedReason {
    /// The sheep holds a shepherd channel: `channel`, `wait_ready` or
    /// `shutdown_with_message`, whose socketpair 2b carries.
    Channel {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep has `stdin = true`, whose pipe 2b carries.
    Stdin {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep is a dog, which 2b's descriptor inventory does not cover
    /// yet.
    Dog {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep's app runs more than one instance, which 2b carries.
    MultiInstance {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep is mid-reload, drainee or replacement, which 2c carries.
    ReloadInFlight {
        /// The sheep's name.
        sheep: String,
    },
    /// An operator's `stop` is waiting on this sheep's next exit, which 2c
    /// carries.
    PendingStop {
        /// The sheep's name.
        sheep: String,
    },
    /// An operator's `delete` targets this sheep, which 2c carries.
    PendingDelete {
        /// The sheep's name.
        sheep: String,
    },
}

impl core::fmt::Display for RefusedReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (sheep, feature) = match self {
            Self::Channel { sheep } => (sheep, "a shepherd channel"),
            Self::Stdin { sheep } => (sheep, "stdin"),
            Self::Dog { sheep } => (sheep, "being a dog"),
            Self::MultiInstance { sheep } => (sheep, "more than one instance"),
            Self::ReloadInFlight { sheep } => (sheep, "an in-flight reload"),
            Self::PendingStop { sheep } => (sheep, "a pending manual stop"),
            Self::PendingDelete { sheep } => (sheep, "a pending delete"),
        };
        write!(
            f,
            "sheep '{sheep}' has {feature}, which this daemon cannot yet hand \
             over; reload falls back to a stop-and-start instead"
        )
    }
}

/// One sheep's carryability-relevant facts.
///
/// Bundles a [`ProcessEntry`] with the two facts that do not live on it: a
/// pending manual stop and a pending delete both live on the supervisor's
/// private slot type, not on the entry it wraps, so `fitness` cannot reach
/// them through `entry` alone. The caller, the supervisor, which owns both
/// of them, builds this view; `fitness` stays a pure function over data it is
/// handed rather than reaching into the registry itself.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The sheep's lifecycle entry.
    pub entry: &'a ProcessEntry,
    /// Whether an operator's `stop` is waiting on this sheep's next exit.
    pub pending_stop: bool,
    /// Whether an operator's `delete` targets this sheep.
    pub pending_delete: bool,
}

/// Decide whether a flock can be handed over in place.
///
/// Whole-flock, not per-sheep: the handover blob describes one process
/// image, so a flock is carried whole or refused whole. An empty flock is
/// carryable.
#[must_use]
pub fn fitness(sheep: &[Candidate<'_>]) -> Fitness {
    for candidate in sheep {
        if let Some(reason) = refusal(candidate) {
            return Fitness::Refused(reason);
        }
    }
    Fitness::Carryable
}

/// Why `candidate` alone refuses the flock, if it does.
fn refusal(candidate: &Candidate<'_>) -> Option<RefusedReason> {
    let entry = candidate.entry;
    let config = entry.spec.config();
    let name = || config.name.clone();

    if config.channel || config.wait_ready || config.shutdown_with_message {
        return Some(RefusedReason::Channel { sheep: name() });
    }
    if config.stdin {
        return Some(RefusedReason::Stdin { sheep: name() });
    }
    if entry.dog.is_some() {
        return Some(RefusedReason::Dog { sheep: name() });
    }
    if config.instances > 1 {
        return Some(RefusedReason::MultiInstance { sheep: name() });
    }
    if !matches!(entry.reload, crate::entry::ReloadState::None) {
        return Some(RefusedReason::ReloadInFlight { sheep: name() });
    }
    if candidate.pending_delete {
        return Some(RefusedReason::PendingDelete { sheep: name() });
    }
    if candidate.pending_stop {
        return Some(RefusedReason::PendingStop { sheep: name() });
    }
    None
}

#[cfg(test)]
mod tests {
    use shep_core::config::AppConfig;
    use shep_core::status::ProcStatus;
    use std::path::PathBuf;

    use super::*;
    use crate::entry::{ReloadState, RestartBudget};
    use crate::privilege::SpawnIdentity;
    use crate::testing::app_with;

    /// A plain, `Online` entry: no channel, no stdin, not a dog, one
    /// instance, no in-flight reload. Every field a real spawn would set is
    /// present so a future field this gate should read cannot be silently
    /// left at a `Default` that hides a bug.
    fn entry_fixture(mutate: impl FnOnce(&mut AppConfig)) -> ProcessEntry {
        let spec = app_with("web", mutate);
        ProcessEntry {
            id: 1,
            spec,
            instance: 0,
            status: ProcStatus::Online,
            pid: Some(100),
            restarts: 0,
            started_at: None,
            budget: RestartBudget::default(),
            reload: ReloadState::None,
            credentials: SpawnIdentity::Resolved(None),
            out_file: PathBuf::from("/tmp/shep-handover-test-out.log"),
            err_file: PathBuf::from("/tmp/shep-handover-test-err.log"),
            dog: None,
            last_exit: None,
        }
    }

    fn plain(entry: &ProcessEntry) -> Candidate<'_> {
        Candidate {
            entry,
            pending_stop: false,
            pending_delete: false,
        }
    }

    #[test]
    fn a_plain_sheep_is_carryable() {
        let e = entry_fixture(|_| {});
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    #[test]
    fn one_unsupported_sheep_refuses_the_whole_flock() {
        // Not per-sheep. The blob describes one process image, so a flock is
        // carried whole or not at all.
        let plain_entry = entry_fixture(|_| {});
        let channelled = entry_fixture(|app| app.channel = true);
        assert!(matches!(
            fitness(&[plain(&plain_entry), plain(&channelled)]),
            Fitness::Refused(_)
        ));
    }

    #[test]
    fn the_refusal_names_which_sheep_and_why() {
        // The operator sees this in `shep daemon reload`'s output, so it has
        // to say what to do about it, not just that it declined.
        let channelled = entry_fixture(|app| app.channel = true);
        let Fitness::Refused(r) = fitness(&[plain(&channelled)]) else {
            panic!("expected a refusal")
        };
        let text = r.to_string();
        assert!(text.contains("shepherd channel"), "{text}");
    }

    #[test]
    fn an_empty_flock_is_carryable() {
        assert_eq!(fitness(&[]), Fitness::Carryable);
    }

    #[test]
    fn wait_ready_alone_refuses_as_a_channel() {
        let e = entry_fixture(|app| app.wait_ready = true);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Channel { .. })
        ));
    }

    #[test]
    fn shutdown_with_message_alone_refuses_as_a_channel() {
        let e = entry_fixture(|app| app.shutdown_with_message = true);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Channel { .. })
        ));
    }

    #[test]
    fn stdin_refuses() {
        let e = entry_fixture(|app| app.stdin = true);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Stdin { .. })
        ));
    }

    #[test]
    fn a_dog_refuses() {
        let mut e = entry_fixture(|_| {});
        e.dog = Some(shep_core::protocol::DogSource::BuiltIn);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Dog { .. })
        ));
    }

    #[test]
    fn more_than_one_instance_refuses() {
        let e = entry_fixture(|app| app.instances = 2);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::MultiInstance { .. })
        ));
    }

    #[test]
    fn an_in_flight_reload_refuses() {
        let mut e = entry_fixture(|_| {});
        e.reload = ReloadState::Replacement;
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::ReloadInFlight { .. })
        ));
    }

    #[test]
    fn a_pending_manual_stop_refuses() {
        let e = entry_fixture(|_| {});
        let candidate = Candidate {
            entry: &e,
            pending_stop: true,
            pending_delete: false,
        };
        assert!(matches!(
            fitness(&[candidate]),
            Fitness::Refused(RefusedReason::PendingStop { .. })
        ));
    }

    #[test]
    fn a_pending_delete_refuses() {
        let e = entry_fixture(|_| {});
        let candidate = Candidate {
            entry: &e,
            pending_stop: false,
            pending_delete: true,
        };
        assert!(matches!(
            fitness(&[candidate]),
            Fitness::Refused(RefusedReason::PendingDelete { .. })
        ));
    }
}
