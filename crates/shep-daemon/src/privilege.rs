//! Privilege drop: resolving an app's requested `user`/`group` to numeric ids.
//!
//! `resolve` is the only entry point: given an [`AppConfig`], it returns
//! the [`Credentials`] a spawn should apply, or `None` when the app asked
//! for neither. Resolution touches the OS passwd/group database (a real, if
//! hermetic and read-only, syscall), which is why it stays a discrete step
//! the supervisor calls once per `Start` rather than folding into the pure
//! `assemble` (see `assemble.rs`'s module doc) — [`crate::supervisor`]
//! resolves once and stores the result on the `ProcessEntry`, reusing it for
//! every later restart instead of re-touching the passwd database each time.
//!
//! [`Credentials`] is the only name here that leaves the crate: it is a field
//! type on [`SpawnSpec`](crate::runner::SpawnSpec), and `tests/real_runner.rs`
//! builds one to prove a real child comes up under the requested uid/gid.
//! `resolve` and `PrivilegeError` have no reader outside this crate.
//!
//! [`Credentials`]/`PrivilegeError` stay plain data (no OS-specific
//! fields), so they can live inside the portable `SpawnSpec`/`ProcessEntry`
//! without pulling this crate's "engine tier compiles everywhere" invariant
//! (see `lib.rs`'s module taxonomy) into the unix-only OS tier. Only the
//! actual passwd/group lookup — `getpwnam`/`getgrnam` via `nix` — is
//! unix-specific; on any other target an explicit `user`/`group` request is
//! simply refused, since the concept doesn't exist there.

use core::fmt;

use serde::{Deserialize, Serialize};
use shep_core::config::AppConfig;

/// Resolved unix credentials for a spawned sheep
// IR-25 audit (Task 11): `uid`/`gid` are plain `pub` fields, not accessor
// methods — reading them is already a zero-cost, un-inlinable field access,
// so there is nothing here for `#[inline]` to annotate. Noted rather than
// silently skipped, since the brief named this type alongside two real
// accessors (`TopicFilter::patterns`, `SnapshotWriter::writes`) that DID get
// `#[inline]`.
//
// `Serialize`/`Deserialize` are here for the handover blob (`handover::Handover`),
// which carries an entry's resolved identity across an `execve` so a restart
// cannot re-touch the passwd database. Two numeric ids, so the derived
// representation is the obvious one; see [`SpawnIdentity`]'s note for what
// changing it would cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// Numeric user id to run the child as
    pub uid: u32,
    /// Numeric group id to run the child as, when a `group` was requested
    pub gid: Option<u32>,
}

/// What identity an entry's next spawn will run under, and whether that
/// question has been asked yet.
///
/// "This app asked for nobody in particular, so the child runs as the
/// shepherd" and "nobody has looked this app's `user` up yet" are different
/// facts, and a spawn that confuses them starts the child as the shepherd
/// when the app asked for someone else. A plain `Option<Credentials>` on
/// `ProcessEntry` spelled both `None`, so the confusion was not something a
/// caller could be careful about. (`ProcessEntry` is crate-internal, hence
/// code font rather than a link.)
///
/// This type is what [`assemble`](crate::assemble::assemble) does NOT take:
/// the assembler still wants a settled `Option<Credentials>`, so reaching one
/// from a stored identity means saying out loud which of the two cases is in
/// hand.
///
/// `Debug` is derived, like [`Credentials`]' own: a uid and a gid are
/// ordinary numbers an operator can read out of `id`, not secrets, and the
/// daemon's own logs are more useful for naming which one a refused spawn
/// wanted.
///
/// `Serialize`/`Deserialize` exist for one reader, the handover blob
/// (`handover::Handover`, crate-internal, hence code font). That blob is
/// written by one shep image and read by another, possibly newer, one, so
/// this type's serialized shape is part of the blob's version-1 format:
/// renaming a variant or retyping a field is a format change and must bump
/// `handover::VERSION` alongside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnIdentity {
    /// `resolve` has never run for this entry (code font, not a link: the
    /// function is crate-internal while this type is not).
    ///
    /// Two entries reach this: one registered at rest from the muster roll,
    /// which is a membership record rather than a `Start`, and one
    /// registered `Errored` BECAUSE its resolution failed. Both must resolve
    /// before they can spawn, and neither may fall back to the shepherd's
    /// own identity.
    Unresolved,
    /// `resolve` has run, and this is what it said: `Some` when the app
    /// named a `user` or a `group`, `None` when it named neither and the
    /// child correctly runs as the shepherd.
    ///
    /// Settled for the life of the entry. A restart reuses this value rather
    /// than looking the name up a second time, so a running app's identity
    /// can never change underneath it and no restart re-touches the passwd
    /// database.
    Resolved(Option<Credentials>),
}

/// Error resolving an app's `user`/`group` config to numeric ids
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrivilegeError {
    /// No passwd entry for this user name
    UnknownUser(String),
    /// No group entry for this group name
    UnknownGroup(String),
    /// The passwd/group database could not be read
    Lookup(String),
    /// Credentials were requested but this daemon does not run as root, so
    /// it cannot change a child's identity
    NotPermitted {
        /// The requested user, if any
        user: Option<String>,
        /// The requested group, if any
        group: Option<String>,
    },
}

impl fmt::Display for PrivilegeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownUser(name) => write!(f, "unknown user {name:?}"),
            Self::UnknownGroup(name) => write!(f, "unknown group {name:?}"),
            Self::Lookup(msg) => write!(f, "passwd/group lookup failed: {msg}"),
            Self::NotPermitted { user, group } => write!(
                f,
                "cannot run as user={user:?} group={group:?}: not running as root"
            ),
        }
    }
}

impl core::error::Error for PrivilegeError {}

/// Resolves an app's `user`/`group` names to numeric ids
///
/// Returns `None` when the app asks for neither.
///
/// # Errors
/// - [`PrivilegeError::UnknownUser`] — no passwd entry for that name.
/// - [`PrivilegeError::UnknownGroup`] — no group entry for that name.
/// - [`PrivilegeError::Lookup`] — the passwd/group database could not be read.
/// - [`PrivilegeError::NotPermitted`] — credentials were requested but this
///   daemon does not run as root, so it cannot change a child's identity.
#[cfg(unix)]
pub(crate) fn resolve(app: &AppConfig) -> Result<Option<Credentials>, PrivilegeError> {
    if app.user.is_none() && app.group.is_none() {
        return Ok(None);
    }

    let own_uid = nix::unistd::geteuid().as_raw();
    let own_gid = nix::unistd::getegid().as_raw();

    let uid = match &app.user {
        Some(name) => resolve_user(name)?,
        None => own_uid,
    };
    let gid = match &app.group {
        Some(name) => Some(resolve_group(name)?),
        None => None,
    };

    // Changing identity to the one this process already has needs no
    // privilege — only asking for a genuinely DIFFERENT uid/gid does. Root
    // can become anyone; a non-root daemon can only ever "become" itself.
    let changing_identity = uid != own_uid || gid.is_some_and(|g| g != own_gid);
    if changing_identity && !nix::unistd::geteuid().is_root() {
        return Err(PrivilegeError::NotPermitted {
            user: app.user.clone(),
            group: app.group.clone(),
        });
    }

    Ok(Some(Credentials { uid, gid }))
}

/// Unix has no uid/gid concept off-platform: an explicit request is refused
/// outright rather than silently ignored (a config that asks for a user and
/// gets the daemon's own identity back with no error is a privilege-drop
/// footgun, not a graceful degradation).
#[cfg(not(unix))]
pub(crate) fn resolve(app: &AppConfig) -> Result<Option<Credentials>, PrivilegeError> {
    match (&app.user, &app.group) {
        (None, None) => Ok(None),
        _ => Err(PrivilegeError::Lookup(
            "uid/gid privilege drop is unix-only".to_string(),
        )),
    }
}

#[cfg(unix)]
fn resolve_user(name: &str) -> Result<u32, PrivilegeError> {
    match nix::unistd::User::from_name(name) {
        Ok(Some(user)) => Ok(user.uid.as_raw()),
        Ok(None) => Err(PrivilegeError::UnknownUser(name.to_string())),
        Err(errno) => Err(PrivilegeError::Lookup(errno.to_string())),
    }
}

#[cfg(unix)]
fn resolve_group(name: &str) -> Result<u32, PrivilegeError> {
    match nix::unistd::Group::from_name(name) {
        Ok(Some(group)) => Ok(group.gid.as_raw()),
        Ok(None) => Err(PrivilegeError::UnknownGroup(name.to_string())),
        Err(errno) => Err(PrivilegeError::Lookup(errno.to_string())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn own_user_name() -> String {
        nix::unistd::User::from_uid(nix::unistd::geteuid())
            .unwrap()
            .expect("this process has a passwd entry")
            .name
    }

    #[test]
    fn no_user_or_group_means_no_credentials() {
        assert_eq!(resolve(&AppConfig::minimal("web", "./srv")).unwrap(), None);
    }

    #[test]
    fn a_real_user_name_resolves_to_its_uid() {
        // Resolving our OWN name is always permitted: changing identity to the
        // one we already have needs no privilege.
        let mut app = AppConfig::minimal("web", "./srv");
        app.user = Some(own_user_name());
        let creds = resolve(&app).unwrap().expect("a user was requested");
        assert_eq!(creds.uid, nix::unistd::geteuid().as_raw());
        assert_eq!(creds.gid, None);
    }

    #[test]
    fn an_unknown_user_is_a_typed_error_naming_the_user() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.user = Some("definitely-not-a-real-shep-user".to_string());
        assert_eq!(
            resolve(&app).unwrap_err(),
            PrivilegeError::UnknownUser("definitely-not-a-real-shep-user".to_string())
        );
    }

    #[test]
    fn asking_for_another_user_without_root_is_refused_in_plain_english() {
        if nix::unistd::geteuid().is_root() {
            // Running as root, the refusal cannot trigger; the guard itself is
            // what this test covers, so there is nothing to assert here.
            return;
        }
        let mut app = AppConfig::minimal("web", "./srv");
        app.user = Some("root".to_string());
        let err = resolve(&app).unwrap_err();
        assert!(matches!(err, PrivilegeError::NotPermitted { .. }));
        assert!(
            err.to_string().contains("not running as root"),
            "the message must say what to do: {err}"
        );
    }
}
