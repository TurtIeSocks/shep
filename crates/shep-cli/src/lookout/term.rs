//! Raw mode, the alternate screen, and getting out of both no matter how the
//! process ends.
//!
//! **This is the worst failure a TUI can have**, because it outlives the
//! process: a crash that leaves raw mode on and the alternate screen entered
//! leaves the operator with no echo, no line editing, no visible cursor and
//! often no scrollback, in a shell that looks broken and is.
//!
//! Two mechanisms, both, because neither covers the other's case:
//!
//! 1. **A panic hook** ([`install_panic_hook`]) that restores and *then* calls
//!    the previous hook. Order matters: restoring first puts the default hook's
//!    backtrace on a cooked terminal, on the main screen, where it can be read
//!    and scrolled. A hook does not run on an ordinary early return.
//! 2. **A [`RestoreGuard`]** whose `Drop` restores. `Drop` does not run under
//!    `panic = "abort"` — which this workspace does not set — and covers every
//!    `?` and early `return` the hook does not.
//!
//! [`restore`] is idempotent, because on a panic BOTH of them fire.
//!
//! Nothing that can panic is installed between the hook and raw mode: the hook
//! goes on first, then raw mode, then the alternate screen.
//!
//! **ratatui 0.30's own `init()` would install a restoring hook too.** It is
//! not used here for one reason: it picks the terminal, the backend and the
//! hook as a bundle, and this phase needs the backend swappable for
//! `TestBackend` so the UI loop itself is testable. The four lines below keep
//! that seam and cost nothing.
//!
//! Not called outside this module's own tests yet: Task 8 (`mod.rs`, the verb
//! and the event loop) is the real caller for [`install_panic_hook`],
//! [`enter`] and [`RestoreGuard`], and it has not landed. `#[allow(dead_code)]`
//! on each public item below says so explicitly, same convention
//! `theme::Palette`, `app::App` and `link::run_link` already carry for the
//! identical reason.

use std::io::{self, Stdout, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Puts the terminal back the way it was found.
///
/// Every step ignores its own failure: this runs from a panic hook, where
/// there is nothing sensible to do with an error and where returning one would
/// mean skipping the steps after it. Safe to call twice, and routinely is.
#[allow(dead_code)]
pub fn restore() {
    let mut out = io::stdout();
    let _ = crossterm::execute!(out, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Chains a restoring panic hook in front of whatever hook is installed.
///
/// Call before [`enter`], and only once per process.
#[allow(dead_code)]
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Enters raw mode and the alternate screen, and hides the cursor.
///
/// **Fails clean.** Raw mode goes on first and the alternate screen second, so
/// there is a window in which the first step has succeeded and the second has
/// not — and a bare `?` there would return `Err` with raw mode still ON, to a
/// caller that is about to return an exit code and never had a guard. The
/// operator would be left with no echo and no line editing, which
/// this module's own doc calls the worst failure a TUI can have. So
/// the second step restores before it reports. The caller arms its
/// [`RestoreGuard`] before calling this as well; both, not either, is the same
/// argument the panic hook and the guard are two of.
///
/// # Errors
/// Whatever `crossterm` could not do to the terminal.
#[allow(dead_code)]
pub fn enter() -> io::Result<Stdout> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    if let Err(err) = crossterm::execute!(out, EnterAlternateScreen, Hide) {
        restore();
        return Err(err);
    }
    Ok(out)
}

/// Restores on drop.
///
/// Holds a closure rather than calling [`restore`] directly so its own test can
/// observe that dropping it acts, without a terminal to act on — the behaviour
/// under test is "the guard runs its action exactly once when it goes out of
/// scope", and that is what regresses if someone converts this to a plain
/// struct with a manual teardown call.
#[allow(dead_code)]
pub struct RestoreGuard {
    action: Option<Box<dyn FnOnce()>>,
}

impl core::fmt::Debug for RestoreGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RestoreGuard")
            .field("armed", &self.action.is_some())
            .finish()
    }
}

impl RestoreGuard {
    /// A guard that calls [`restore`] when it is dropped.
    #[allow(dead_code)]
    #[must_use]
    pub fn new() -> Self {
        Self::with_action(restore)
    }

    /// A guard that calls `action` when it is dropped.
    #[allow(dead_code)]
    #[must_use]
    pub fn with_action(action: impl FnOnce() + 'static) -> Self {
        Self {
            action: Some(Box::new(action)),
        }
    }
}

impl Default for RestoreGuard {
    /// The same guard [`RestoreGuard::new`] builds.
    ///
    /// Present because `clippy::new_without_default` is a default-on style
    /// lint and `cargo clippy -- -D warnings` is in the task gate — an
    /// argument-less `new` with no `Default` fails it.
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if let Some(action) = self.action.take() {
            action();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `restore` stops being safe to call twice. Both the panic hook
    /// and the guard's `Drop` fire on a panic, so the second call is not an
    /// edge case — it is the ordinary path through a crash, and a `restore`
    /// that panicked on its second call would abort the process inside the
    /// panic handler and leave the terminal exactly as broken as doing nothing.
    #[test]
    fn restore_is_idempotent_outside_raw_mode() {
        restore();
        restore();
    }

    /// fails if the guard stops restoring on an ordinary drop. The panic hook
    /// does not run on a `?` or an early `return`; this is the half that does.
    #[test]
    fn the_guard_restores_when_it_is_dropped() {
        let restored = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let _guard = RestoreGuard::with_action({
                let restored = std::sync::Arc::clone(&restored);
                move || {
                    restored.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
        assert_eq!(restored.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
