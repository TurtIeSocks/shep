//! Process lifecycle status

use core::fmt;

use serde::{Deserialize, Serialize};

/// Lifecycle state of a sheep (one managed process)
///
/// The serialized strings are the wire contract; `waiting-restart` means a
/// backoff or restart delay is pending.
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcStatus {
    /// Spawned, not yet ready
    Starting,
    /// Running and (if configured) ready
    Online,
    /// Stop ladder in progress
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
        // wire format: these six strings are the protocol contract (spec §4)
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
