//! `shep startup`/`unstartup`: generating and installing an init system
//! unit for the daemon. [`mod@unit`] renders a systemd unit or a launchd
//! plist from a [`unit::UnitSpec`] — pure `format!`, no filesystem or
//! process access. The verb that resolves a real `UnitSpec`, writes the
//! rendered text to disk, and (on systemd) runs `systemctl enable` is
//! Task 12; this module is the renderer it will call into.

pub(crate) mod unit;
