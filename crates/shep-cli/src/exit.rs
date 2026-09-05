//! The process exit-code taxonomy: [`ExitCode`] and its wire-error
//! conversions. `docs/specs/shep-v1.md` §9 is the source of truth for the
//! numbers and their meanings.

use shep_core::protocol::RpcErrorCode;

/// A `shep` process exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// The command did what it was asked.
    #[cfg_attr(windows, allow(dead_code))]
    Success = 0,
    /// An error with no more specific code.
    Failure = 1,
    /// Bad arguments. clap's own convention.
    Usage = 2,
    /// A selector matched no registered sheep.
    NotFound = 3,
    /// A Flockfile or daemon config failed validation.
    InvalidConfig = 4,
    /// No daemon answered, and none could be started.
    #[cfg_attr(windows, allow(dead_code))]
    DaemonUnreachable = 5,
    /// Client and daemon speak different wire versions.
    ProtocolMismatch = 6,
    /// The daemon could not spawn a sheep.
    SpawnFailed = 7,
    /// The request outlived its deadline.
    DeadlineExceeded = 8,
    /// An unexpected daemon-side failure.
    Internal = 9,
    /// Another daemon already holds this `$SHEP_HOME`. Read across the
    /// process boundary by `shep_client::spawn::DAEMON_ALREADY_RUNNING`,
    /// which must stay equal to 10.
    ///
    /// How a losing child in a cold-start race tells the probing parent it
    /// lost.
    #[cfg_attr(windows, allow(dead_code))]
    DaemonAlreadyRunning = 10,
    /// The flock emptied and something in it had failed.
    ///
    /// `runtime`'s fail-fast status: nothing is online any more and at least
    /// one sheep ended `errored`. A flock that emptied cleanly, every sheep
    /// `stopped`, exits `Success` instead.
    FlockEmpty = 11,
    /// This binary and the running shepherd are different versions of shep.
    ///
    /// The handshake succeeded and only the crate versions differ, the state
    /// `cargo install shep` leaves behind. A wire disagreement is
    /// [`ExitCode::ProtocolMismatch`] instead. Never returned for `kill`,
    /// `daemon reload` or `ping`, which are how an operator gets out of it.
    VersionSkew = 12,
}

impl ExitCode {
    /// The stable machine-readable spelling of this code, as it appears in
    /// `--format json`'s `error.code` field (`"not_found"`, `"usage"`, …).
    ///
    /// The single place those strings are written: `emit_error` takes the
    /// code as a `&str`, so no verb invents its own spelling.
    #[must_use]
    pub const fn code_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Usage => "usage",
            Self::NotFound => "not_found",
            Self::InvalidConfig => "invalid_config",
            Self::DaemonUnreachable => "daemon_unreachable",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::SpawnFailed => "spawn_failed",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Internal => "internal",
            Self::DaemonAlreadyRunning => "daemon_already_running",
            Self::FlockEmpty => "flock_empty",
            Self::VersionSkew => "version_skew",
        }
    }
}

/// Maps a daemon-reported [`RpcErrorCode`] to the exit code that reports it.
///
/// `RpcErrorCode` is `#[non_exhaustive]`, so a variant this binary predates
/// becomes [`ExitCode::Internal`], not [`ExitCode::Failure`].
impl From<RpcErrorCode> for ExitCode {
    fn from(code: RpcErrorCode) -> Self {
        match code {
            RpcErrorCode::NotFound => Self::NotFound,
            RpcErrorCode::InvalidConfig => Self::InvalidConfig,
            RpcErrorCode::SpawnFailed => Self::SpawnFailed,
            RpcErrorCode::ProtocolMismatch => Self::ProtocolMismatch,
            RpcErrorCode::Internal => Self::Internal,
            RpcErrorCode::DeadlineExceeded => Self::DeadlineExceeded,
            _ => Self::Internal,
        }
    }
}

/// Maps a failure to reach the daemon at all to the exit code that reports
/// it.
///
/// [`shep_client::ConnectError::ProtocolMismatch`] is the one variant with
/// its own code; every other means nothing usable answered at the socket,
/// whatever stage failed. A future variant falls to [`ExitCode::Failure`].
impl From<&shep_client::ConnectError> for ExitCode {
    fn from(err: &shep_client::ConnectError) -> Self {
        use shep_client::ConnectError::{
            Connect, HandshakeClosed, HandshakeTimeout, Io, ProtocolMismatch, Wire,
        };
        match err {
            ProtocolMismatch { .. } => Self::ProtocolMismatch,
            Connect { .. } | Io(_) | Wire(_) | HandshakeClosed | HandshakeTimeout { .. } => {
                Self::DaemonUnreachable
            }
            _ => Self::Failure,
        }
    }
}

/// Maps a failed request against an already-connected daemon to the exit
/// code that reports it.
///
/// `Rpc` defers to the [`RpcErrorCode`] conversion, so the two taxonomies
/// cannot drift. `Closed` is the same "nothing is answering" condition as a
/// failed connect. `Wire` is this client failing to encode its own request, a
/// fault in this binary. A future variant falls to [`ExitCode::Failure`].
impl From<&shep_client::RequestError> for ExitCode {
    fn from(err: &shep_client::RequestError) -> Self {
        use shep_client::RequestError::{Closed, Rpc, Timeout, Wire};
        match err {
            Rpc(rpc) => Self::from(rpc.code),
            Timeout { .. } => Self::DeadlineExceeded,
            Closed => Self::DaemonUnreachable,
            Wire(_) => Self::Internal,
            _ => Self::Failure,
        }
    }
}

/// Maps a failed `connect_or_spawn` attempt to the exit code that reports
/// it.
///
/// `Connect` defers to the [`shep_client::ConnectError`] conversion, so a
/// version-skew refusal keeps [`ExitCode::ProtocolMismatch`] through the
/// autostart path. `Launch`, `DaemonExited` and `DeadlineExpired` are the
/// three ways autostart ends with no daemon answering. A future variant
/// falls to [`ExitCode::Failure`].
impl From<&shep_client::spawn::SpawnError> for ExitCode {
    fn from(err: &shep_client::spawn::SpawnError) -> Self {
        use shep_client::spawn::SpawnError::{Connect, DaemonExited, DeadlineExpired, Launch};
        match err {
            Connect(inner) => Self::from(inner),
            Launch(_) | DaemonExited { .. } | DeadlineExpired { .. } => Self::DaemonUnreachable,
            _ => Self::Failure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rpc_error_code_maps_to_a_distinct_nonzero_exit_code() {
        // `ALL` is exhaustive-checked inside shep-core, so a new variant lands
        // here, collides with `Internal` under the `From` impl's `_` arm, and
        // fails the distinctness assertion.
        let codes = shep_core::protocol::RpcErrorCode::ALL;
        let mapped: Vec<u8> = codes.iter().map(|c| ExitCode::from(*c) as u8).collect();
        assert!(
            mapped.iter().all(|&c| c != 0),
            "no error may map to Success"
        );
        let unique: std::collections::HashSet<_> = mapped.iter().collect();
        assert_eq!(
            unique.len(),
            mapped.len(),
            "distinct causes need distinct exit codes: {mapped:?}"
        );
    }

    /// Distinctness is the property; the exact words are pinned by a later
    /// snapshot.
    #[test]
    fn every_exit_code_has_its_own_machine_readable_spelling() {
        let all = [
            ExitCode::Success,
            ExitCode::Failure,
            ExitCode::Usage,
            ExitCode::NotFound,
            ExitCode::InvalidConfig,
            ExitCode::DaemonUnreachable,
            ExitCode::ProtocolMismatch,
            ExitCode::SpawnFailed,
            ExitCode::DeadlineExceeded,
            ExitCode::Internal,
            ExitCode::DaemonAlreadyRunning,
            ExitCode::FlockEmpty,
            ExitCode::VersionSkew,
        ];
        let strings: Vec<&str> = all.iter().map(|c| c.code_str()).collect();
        assert!(strings.iter().all(|s| !s.is_empty()));
        assert!(
            strings
                .iter()
                .all(|s| s.chars().all(|c| c.is_ascii_lowercase() || c == '_')),
            "these go on the JSON surface: {strings:?}"
        );
        let unique: std::collections::HashSet<_> = strings.iter().collect();
        assert_eq!(
            unique.len(),
            strings.len(),
            "duplicated spelling: {strings:?}"
        );
    }

    /// The one number both crates hard-code. If they diverge, the cold-start
    /// race in `connect_or_spawn` becomes a fatal error.
    #[cfg(unix)]
    #[test]
    fn the_already_running_exit_code_matches_the_clients_constant() {
        assert_eq!(
            ExitCode::DaemonAlreadyRunning as i32,
            shep_client::spawn::DAEMON_ALREADY_RUNNING
        );
    }
}
