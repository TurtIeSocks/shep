//! The process exit-code taxonomy: [`ExitCode`] and its wire-error
//! conversions. `docs/specs/shep-v1.md` §9 is the source of truth for the
//! numbers and their meanings.

use shep_core::protocol::RpcErrorCode;

/// A `shep` process exit status.
///
/// No `#[non_exhaustive]`: this is a binary crate, so there is no downstream
/// matcher for it to protect, and IR-20's growth argument does not apply
/// (contrast shep-client's three error enums, which do carry it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// The command did what it was asked.
    ///
    /// Its first real call site is unix-only: `run_daemon` returning
    /// `Ok(())` on a clean shutdown (signal or `KillDaemon`) maps here.
    /// Every other verb still routes through `not_wired`'s `Internal`
    /// until its own dispatch arm replaces that placeholder — which is why
    /// this stays dead on the Windows target, where the whole `daemon`
    /// dispatch arm does not exist.
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
    ///
    /// Never constructed on Windows: the three conversions that produce it
    /// (`From<&ConnectError>`, `From<&RequestError>`, `From<&SpawnError>`)
    /// are all `#[cfg(unix)]` — there is no transport for a daemon to be
    /// unreachable *over* until spec §11's Windows functional tier lands.
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
    /// Constructed by the hidden `daemon` subcommand's own exit-code
    /// mapping (unix-only) when a boot fails with
    /// `BootError::AlreadyRunning` — the only channel by which a losing
    /// child in a cold-start race can tell the probing parent it lost.
    /// Stays dead on the Windows target, where that dispatch arm does not
    /// exist.
    #[cfg_attr(windows, allow(dead_code))]
    DaemonAlreadyRunning = 10,
}

impl ExitCode {
    /// The stable machine-readable spelling of this code, as it appears in
    /// `--format json`'s `error.code` field (`"not_found"`, `"usage"`, …).
    ///
    /// `emit_error` takes the code as a `&str` so `output/` never has to
    /// know the CLI's taxonomy; this is the single place those strings are
    /// written, so call sites read `emit_error(err, fmt, code.code_str(), &msg)`
    /// and no verb invents its own spelling.
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
        }
    }
}

/// Maps a daemon-reported [`RpcErrorCode`] to the exit code that reports it.
///
/// `RpcErrorCode` is `#[non_exhaustive]`, so this carries a `_` arm; an
/// unrecognised future variant becomes [`ExitCode::Internal`] rather than
/// silently defaulting to [`ExitCode::Failure`] — a daemon that started
/// speaking a code this binary predates is exactly the "unexpected
/// daemon-side failure" [`ExitCode::Internal`] describes.
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
/// its own dedicated code ([`ExitCode::ProtocolMismatch`]); every other
/// variant means nothing usable answered at the socket, which is
/// [`ExitCode::DaemonUnreachable`] regardless of which stage of the connect
/// or handshake failed to complete. `ConnectError` is `#[non_exhaustive]`
/// (IR-20), so a future variant falls to [`ExitCode::Failure`] rather than
/// being guessed at.
#[cfg(unix)]
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
/// A daemon-side [`shep_client::RequestError::Rpc`] answer defers to the
/// [`RpcErrorCode`] conversion above, so the two taxonomies never drift
/// apart. [`shep_client::RequestError::Closed`] means the daemon went away
/// mid-request, which is the same "nothing is answering" condition as a
/// failed connect ([`ExitCode::DaemonUnreachable`]).
/// [`shep_client::RequestError::Wire`] is this client failing to encode its
/// own request — a fault in this binary, not the daemon
/// ([`ExitCode::Internal`]). `RequestError` is `#[non_exhaustive]` (IR-20),
/// so a future variant falls to [`ExitCode::Failure`] rather than being
/// guessed at.
#[cfg(unix)]
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
/// [`shep_client::spawn::SpawnError::Connect`] carries the [`shep_client::ConnectError`]
/// that caused the first probe (or a later protocol-mismatch probe) to fail,
/// so it defers to that conversion — a daemon that answers but refuses on
/// version skew still gets [`ExitCode::ProtocolMismatch`], even reached
/// through the autostart path. `Launch`, `DaemonExited` and
/// `DeadlineExpired` are the three ways autostart can end without a daemon
/// ever answering, matching spec §9's own wording for code 5 ("no daemon
/// answered, and none could be started"), so all three map to
/// [`ExitCode::DaemonUnreachable`]. `SpawnError` is `#[non_exhaustive]`
/// (IR-20), so a future variant falls to [`ExitCode::Failure`] rather than
/// being guessed at.
#[cfg(unix)]
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
        // `RpcErrorCode` is `#[non_exhaustive]`, so the `From` impl above needs a
        // `_` arm and the compiler cannot force *this* list to stay complete on
        // its own. Iterating `RpcErrorCode::ALL` instead of a hand-written array
        // closes that gap: `ALL` is exhaustive-checked inside shep-core (its
        // defining crate, where `#[non_exhaustive]` does not apply), so a new
        // variant there is compiled into `ALL` — and then lands here, where an
        // unmapped variant collides with `Internal` under the `From` impl's `_`
        // arm and this test's distinctness assertion below catches it.
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

    /// `emit_error` takes the code as a `&str`, so a copy-pasted `code_str` arm
    /// returning a neighbour's spelling would put the wrong `error.code` in every
    /// JSON failure of one command and nothing would notice. Distinctness is the
    /// property; the exact words are pinned by a later snapshot.
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

    /// The one number both crates hard-code. If these ever diverge, the
    /// cold-start race in `connect_or_spawn` silently becomes a fatal error
    /// again.
    #[cfg(unix)]
    #[test]
    fn the_already_running_exit_code_matches_the_clients_constant() {
        assert_eq!(
            ExitCode::DaemonAlreadyRunning as i32,
            shep_client::spawn::DAEMON_ALREADY_RUNNING
        );
    }
}
