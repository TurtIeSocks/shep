//! `crossterm::event::Event` -> [`KeyPress`]. The whole crossterm-typed edge
//! of the keyboard, kept in one small file so `super::app` never imports a
//! terminal crate and its reducer tests never construct one.
//!
//! [`map_key`]'s real caller is `super::run_ui`'s keyboard arm.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use super::app::KeyPress;

/// The [`KeyPress`] this event means, or `None` for a key lookout does not
/// bind.
///
/// Only `KeyEventKind::Press` counts. Terminals that report repeats and
/// releases (Windows consoles, and anything with the kitty keyboard protocol
/// enabled) would otherwise fire an action once per repeat of a held key —
/// which is the fat-finger case the control gate exists for, arriving through
/// the keymap instead of through the operator.
///
/// **`Ctrl-C` is a binding, not a signal.** In raw mode crossterm delivers it
/// as an ordinary key event; there is no `SIGINT` to catch. Dropping this
/// mapping would leave the most reflexive way out of a terminal program doing
/// nothing, and the operator's next move — `kill -9` from another window —
/// skips every restore path `super::term` has.
#[must_use]
pub fn map_key(event: &Event) -> Option<KeyPress> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(KeyPress::Quit),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(KeyPress::Quit),
        KeyCode::Char('j') | KeyCode::Down => Some(KeyPress::SelectDown),
        KeyCode::Char('k') | KeyCode::Up => Some(KeyPress::SelectUp),
        KeyCode::Char('g') | KeyCode::Home => Some(KeyPress::SelectFirst),
        KeyCode::Char('G') | KeyCode::End => Some(KeyPress::SelectLast),
        KeyCode::Char('r') => Some(KeyPress::Refresh),
        KeyCode::Char('x') => Some(KeyPress::Stop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// fails if a key stops resolving, or starts resolving to the wrong thing.
    /// `x` in particular: it is the one key wired to an action, so a keymap
    /// that silently rebound it would be a keymap that acts on the wrong
    /// intent once a future phase makes the action real — it still refuses
    /// today, in both control states.
    #[test]
    fn every_bound_key_resolves_to_its_press() {
        assert_eq!(map_key(&key(KeyCode::Char('q'))), Some(KeyPress::Quit));
        assert_eq!(map_key(&key(KeyCode::Esc)), Some(KeyPress::Quit));
        assert_eq!(
            map_key(&key(KeyCode::Char('j'))),
            Some(KeyPress::SelectDown)
        );
        assert_eq!(map_key(&key(KeyCode::Down)), Some(KeyPress::SelectDown));
        assert_eq!(map_key(&key(KeyCode::Char('k'))), Some(KeyPress::SelectUp));
        assert_eq!(map_key(&key(KeyCode::Up)), Some(KeyPress::SelectUp));
        assert_eq!(
            map_key(&key(KeyCode::Char('g'))),
            Some(KeyPress::SelectFirst)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('G'))),
            Some(KeyPress::SelectLast)
        );
        assert_eq!(map_key(&key(KeyCode::Char('r'))), Some(KeyPress::Refresh));
        assert_eq!(map_key(&key(KeyCode::Char('x'))), Some(KeyPress::Stop));
        assert_eq!(map_key(&key(KeyCode::Char('z'))), None);
    }

    /// fails if the four movement keys stop meaning SELECTION. They were
    /// named for scrolling in 12a and the pane genuinely scrolled; it now
    /// carries a cursor, and a name that says otherwise is the kind this
    /// project's reviews keep catching. The KEYS are unchanged — an operator's
    /// muscle memory is not what this rename touches.
    #[test]
    fn the_movement_keys_are_unchanged_and_now_mean_selection() {
        assert_eq!(
            map_key(&key(KeyCode::Char('j'))),
            Some(KeyPress::SelectDown)
        );
        assert_eq!(map_key(&key(KeyCode::Down)), Some(KeyPress::SelectDown));
        assert_eq!(map_key(&key(KeyCode::Char('k'))), Some(KeyPress::SelectUp));
        assert_eq!(map_key(&key(KeyCode::Up)), Some(KeyPress::SelectUp));
        assert_eq!(
            map_key(&key(KeyCode::Char('g'))),
            Some(KeyPress::SelectFirst)
        );
        assert_eq!(map_key(&key(KeyCode::Home)), Some(KeyPress::SelectFirst));
        assert_eq!(
            map_key(&key(KeyCode::Char('G'))),
            Some(KeyPress::SelectLast)
        );
        assert_eq!(map_key(&key(KeyCode::End)), Some(KeyPress::SelectLast));
    }

    /// fails if Ctrl-C stops quitting. In raw mode crossterm does NOT deliver
    /// Ctrl-C as a signal — it arrives here as an ordinary key event — so if
    /// this mapping goes away, the most-reflexive way out of a terminal
    /// program stops working and the operator's next move is `kill -9` from
    /// another window, which skips every restore path this module has.
    #[test]
    fn ctrl_c_quits_because_raw_mode_swallows_the_signal() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(map_key(&event), Some(KeyPress::Quit));
        // Without the modifier it is not a binding at all.
        assert_eq!(map_key(&key(KeyCode::Char('c'))), None);
    }

    /// fails if key REPEATS and RELEASES start being handled as presses. On a
    /// terminal that reports them (Windows consoles, and any terminal with the
    /// kitty keyboard protocol on), a held `x` would fire the action once per
    /// repeat — which is exactly the fat-finger case the control gate exists
    /// for, arriving through the keymap instead.
    #[test]
    fn only_a_press_counts() {
        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(map_key(&Event::Key(release)), None);

        let mut repeat = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(map_key(&Event::Key(repeat)), None);
    }
}
