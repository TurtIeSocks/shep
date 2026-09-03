//! Per-verb command implementations, `daemon` first. OS tier: gated
//! `#[cfg(unix)]` at this module's own declaration in `lib.rs`, so nothing
//! declared beneath it needs a `cfg` of its own -- and nothing under it is
//! compiled on Windows at all, which is why the tests here reach for `sh`
//! and `sleep` without a portability dance.

pub mod admin;
pub mod bleats;
pub(crate) mod bounded;
pub mod daemon;
pub mod dev;
pub(crate) mod dog_migration;
pub mod dogs;
pub(crate) mod empty;
pub(crate) mod foreground;
pub mod import;
pub(crate) mod init;
pub mod kv;
pub mod lifecycle;
pub mod logs;
pub mod muster;
pub mod query;
// Unix only, and structurally so rather than by omission. This module is
// `shep runtime`'s PID-1 zombie reaper: it exists because a unix init
// process inherits every orphan on the machine and must `waitpid` them or
// leak zombies forever. Windows has no zombie state and no reparent-to-init
// rule — a Windows process's exit status lives in its handle, and the kernel
// reclaims everything when the last handle closes — so there is nothing for
// a reaper to do there. See `commands::runtime` for how the Windows arm
// skips it.
#[cfg(unix)]
pub(crate) mod reap;
pub mod runtime;
pub mod schema;
pub(crate) mod selector;
pub mod serve;
pub(crate) mod shep_toml;
pub mod signal;
// Unix only, and this is the Windows tier's largest deliberate omission
// rather than an oversight. `shep startup` installs a boot-time unit —
// systemd, launchd, openrc, or a BSD `rc.d` script. Windows' equivalent is a
// real service registered with the Service Control Manager
// (`CreateService`, a `StartServiceCtrlDispatcher` entry point, and a
// control handler answering `SERVICE_CONTROL_STOP`), which is a different
// program shape rather than a sixth template: the SCM's service database is
// registry-backed, and a service's process does not have a `main` in the
// ordinary sense.
//
// `docs/specs/windows-estimate.md` calls that Tier B. Everything shipped so
// far is Tier A — a shepherd you launch yourself, in your own session, that
// does not survive a reboot — and the two verbs below refuse on Windows with
// a message saying exactly that, rather than pretending to install anything.
#[cfg(unix)]
pub(crate) mod startup;
pub mod trigger;
pub mod whisper;
