//! Process lifecycle status

use core::fmt;

use serde::{Deserialize, Serialize};

/// Lifecycle state of a sheep (one managed process)
///
/// The serialized strings are the wire contract; `waiting-restart` means a
/// backoff or restart delay is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcStatus {
    /// Spawned, not yet ready
    Starting,
    /// Running and (if configured) ready
    Online,
    /// This instance is going away and is not a restart target
    ///
    /// Reachable from exactly one path: a reload's `SpawnNew` step, which
    /// marks the instance being replaced before its replacement is spawned,
    /// so the two never both count as running. Nothing else sets it — an
    /// operator's `stop` leaves a sheep `Online` for its whole kill ladder
    /// instead, so this status names reload's transient specifically, never
    /// "any kill ladder in progress". A scheduled restart or an out-of-band
    /// liveness/memory-limit restart must both reject a sheep in this status
    /// rather than race the fresh replacement coming to take over its slot.
    Stopping,
    /// Cleanly stopped; not scheduled to run
    Stopped,
    /// Restart budget exhausted or spawn failed
    Errored,
    /// Restart pending after a backoff or configured delay
    WaitingRestart,
}

impl fmt::Display for ProcStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Online => "online",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Errored => "errored",
            Self::WaitingRestart => "waiting-restart",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_strings_are_stable() {
        let cases = [
            (ProcStatus::Starting, "\"starting\""),
            (ProcStatus::Online, "\"online\""),
            (ProcStatus::Stopping, "\"stopping\""),
            (ProcStatus::Stopped, "\"stopped\""),
            (ProcStatus::Errored, "\"errored\""),
            (ProcStatus::WaitingRestart, "\"waiting-restart\""),
        ];
        for (status, json) in cases {
            assert_eq!(serde_json::to_string(&status).unwrap(), json);
            assert_eq!(serde_json::from_str::<ProcStatus>(json).unwrap(), status);
            assert_eq!(format!("\"{status}\""), json);
        }
    }
}
