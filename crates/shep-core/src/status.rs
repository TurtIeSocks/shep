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
    /// Set by whichever step marks the drainee before its replacement takes
    /// the slot: `SpawnNew` for an overlap reload, `DrainOld` for a serial
    /// one. Either way, the old and new instance never both count as
    /// running. An operator's `stop` leaves a sheep `Online` through its
    /// whole kill ladder instead; a scheduled or out-of-band restart must
    /// reject a sheep in this status rather than race the replacement
    /// taking over its slot.
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
