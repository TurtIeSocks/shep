//! Raw mode, the alternate screen, and getting out of both no matter how the
//! process ends.
//!
//! A crash that leaves raw mode on and the alternate screen entered leaves
//! the operator with no echo, no line editing and no visible cursor.
//!
//! Two mechanisms cover it, since neither reaches the other's case:
//! [`install_panic_hook`] restores before calling the previous hook, and
//! [`RestoreGuard`]'s `Drop` covers every `?` and early return, though not
//! under `panic = "abort"`, which this workspace does not set. [`restore`]
//! is idempotent, since a panic fires both. Nothing that can panic runs
//! between the hook and raw mode: hook, then raw mode, then the screen.

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
pub fn restore() {
    let mut out = io::stdout();
    let _ = crossterm::execute!(out, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Chains a restoring panic hook in front of whatever hook is installed.
///
/// Call before [`enter`], and only once per process.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Installs the panic hook, then panics on purpose.
///
/// Gated behind `SHEP_TERM_PANIC_PROBE` so it never fires by accident and
/// never appears on the command surface. `tests/term_panic_order.rs` drives
/// it through a subprocess to check that [`install_panic_hook`] restores the
/// terminal before the previous hook prints its backtrace.
///
/// # Panics
/// Always. The message names what is being tested.
#[track_caller]
pub fn probe_panic_for_test() -> ! {
    install_panic_hook();
    panic!("shep_term_panic_probe: deliberate panic exercising restore-before-backtrace ordering");
}

/// Enters raw mode and the alternate screen, and hides the cursor.
///
/// Raw mode goes on before the alternate screen, so a failure entering the
/// alternate screen restores the terminal before returning the error rather
/// than leaving raw mode on. The caller still arms its own [`RestoreGuard`]
/// before calling this.
///
/// # Errors
/// Whatever `crossterm` could not do to the terminal.
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
/// Holds a closure rather than calling [`restore`] directly, so its own test
/// can observe the drop firing without a real terminal.
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
    #[must_use]
    pub fn new() -> Self {
        Self::with_action(restore)
    }

    /// A guard that calls `action` when it is dropped.
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
    /// Satisfies `clippy::new_without_default`.
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

    /// Both the panic hook and the guard's `Drop` fire on a panic, so the
    /// second call is the ordinary path through a crash, not an edge case.
    #[test]
    fn restore_is_idempotent_outside_raw_mode() {
        restore();
        restore();
    }

    /// The panic hook does not run on a `?` or an early return; this guard
    /// does.
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
