//! `crossterm::event::Event` -> [`KeyPress`]. The whole crossterm-typed edge
//! of the keyboard, kept in one small file so `super::app` never imports a
//! terminal crate and its reducer tests never construct one.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use super::app::{ActionVerb, InputMode, KeyPress};

/// The [`KeyPress`] this event means under `mode`, or `None` for a key
/// lookout does not bind there.
///
/// Only `KeyEventKind::Press` counts: a terminal that reports repeats and
/// releases would otherwise fire an action once per repeat of a held key.
/// `Ctrl-C` is a binding in either mode, since raw mode delivers it as an
/// ordinary key event and there is no `SIGINT` to catch.
#[must_use]
pub fn map_key(event: &Event, mode: InputMode) -> Option<KeyPress> {
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
    if mode == InputMode::Text {
        return match key.code {
            // SHIFT stays: crossterm delivers a capital as `Char('W')` with
            // SHIFT set. ALT is filtered, since `Alt-w` is never a letter.
            KeyCode::Char(typed) if !key.modifiers.contains(KeyModifiers::ALT) => {
                Some(KeyPress::TextChar(typed))
            }
            KeyCode::Backspace => Some(KeyPress::TextBackspace),
            KeyCode::Enter => Some(KeyPress::TextApply),
            KeyCode::Esc => Some(KeyPress::TextAbandon),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Some(KeyPress::Quit),
        KeyCode::Esc => Some(KeyPress::Escape),
        KeyCode::Char('/') => Some(KeyPress::FilterStart),
        KeyCode::Char('j') | KeyCode::Down => Some(KeyPress::SelectDown),
        KeyCode::Char('k') | KeyCode::Up => Some(KeyPress::SelectUp),
        KeyCode::Char('g') | KeyCode::Home => Some(KeyPress::SelectFirst),
        KeyCode::Char('G') | KeyCode::End => Some(KeyPress::SelectLast),
        KeyCode::Char('r') => Some(KeyPress::Refresh),
        KeyCode::Char('x') => Some(KeyPress::Action(ActionVerb::Stop)),
        KeyCode::Char('R') => Some(KeyPress::Action(ActionVerb::Restart)),
        KeyCode::Char('L') => Some(KeyPress::Action(ActionVerb::Reload)),
        KeyCode::Char('s') => Some(KeyPress::Settings),
        KeyCode::Char(' ') => Some(KeyPress::Cycle),
        KeyCode::Enter => Some(KeyPress::Confirm),
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

    #[test]
    fn every_bound_key_resolves_to_its_press() {
        assert_eq!(
            map_key(&key(KeyCode::Char('q')), InputMode::Normal),
            Some(KeyPress::Quit)
        );
        assert_eq!(
            map_key(&key(KeyCode::Esc), InputMode::Normal),
            Some(KeyPress::Escape)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('j')), InputMode::Normal),
            Some(KeyPress::SelectDown)
        );
        assert_eq!(
            map_key(&key(KeyCode::Down), InputMode::Normal),
            Some(KeyPress::SelectDown)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('k')), InputMode::Normal),
            Some(KeyPress::SelectUp)
        );
        assert_eq!(
            map_key(&key(KeyCode::Up), InputMode::Normal),
            Some(KeyPress::SelectUp)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('g')), InputMode::Normal),
            Some(KeyPress::SelectFirst)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('G')), InputMode::Normal),
            Some(KeyPress::SelectLast)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('r')), InputMode::Normal),
            Some(KeyPress::Refresh)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('x')), InputMode::Normal),
            Some(KeyPress::Action(ActionVerb::Stop))
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('R')), InputMode::Normal),
            Some(KeyPress::Action(ActionVerb::Restart))
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('L')), InputMode::Normal),
            Some(KeyPress::Action(ActionVerb::Reload))
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('s')), InputMode::Normal),
            Some(KeyPress::Settings)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char(' ')), InputMode::Normal),
            Some(KeyPress::Cycle)
        );
        assert_eq!(map_key(&key(KeyCode::Char('z')), InputMode::Normal), None);
    }

    #[test]
    fn the_movement_keys_are_unchanged_and_now_mean_selection() {
        assert_eq!(
            map_key(&key(KeyCode::Char('j')), InputMode::Normal),
            Some(KeyPress::SelectDown)
        );
        assert_eq!(
            map_key(&key(KeyCode::Down), InputMode::Normal),
            Some(KeyPress::SelectDown)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('k')), InputMode::Normal),
            Some(KeyPress::SelectUp)
        );
        assert_eq!(
            map_key(&key(KeyCode::Up), InputMode::Normal),
            Some(KeyPress::SelectUp)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('g')), InputMode::Normal),
            Some(KeyPress::SelectFirst)
        );
        assert_eq!(
            map_key(&key(KeyCode::Home), InputMode::Normal),
            Some(KeyPress::SelectFirst)
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('G')), InputMode::Normal),
            Some(KeyPress::SelectLast)
        );
        assert_eq!(
            map_key(&key(KeyCode::End), InputMode::Normal),
            Some(KeyPress::SelectLast)
        );
    }

    #[test]
    fn ctrl_c_quits_because_raw_mode_swallows_the_signal() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(map_key(&event, InputMode::Normal), Some(KeyPress::Quit));
        assert_eq!(map_key(&key(KeyCode::Char('c')), InputMode::Normal), None);
    }

    #[test]
    fn only_a_press_counts() {
        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(map_key(&Event::Key(release), InputMode::Normal), None);

        let mut repeat = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(map_key(&Event::Key(repeat), InputMode::Normal), None);
    }

    #[test]
    fn typing_q_while_editing_types_a_letter() {
        assert_eq!(
            map_key(&key(KeyCode::Char('q')), InputMode::Text),
            Some(KeyPress::TextChar('q'))
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('q')), InputMode::Normal),
            Some(KeyPress::Quit)
        );
    }

    #[test]
    fn the_text_mode_binds_exactly_the_box_s_keys() {
        assert_eq!(
            map_key(&key(KeyCode::Backspace), InputMode::Text),
            Some(KeyPress::TextBackspace)
        );
        assert_eq!(
            map_key(&key(KeyCode::Enter), InputMode::Text),
            Some(KeyPress::TextApply)
        );
        assert_eq!(
            map_key(&key(KeyCode::Esc), InputMode::Text),
            Some(KeyPress::TextAbandon)
        );
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(map_key(&ctrl_c, InputMode::Text), Some(KeyPress::Quit));
        assert_eq!(map_key(&key(KeyCode::F(5)), InputMode::Text), None);
    }

    #[test]
    fn a_shifted_letter_is_still_a_letter_in_the_box() {
        let shifted = Event::Key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT));
        assert_eq!(
            map_key(&shifted, InputMode::Text),
            Some(KeyPress::TextChar('W'))
        );
    }

    #[test]
    fn slash_opens_the_filter_in_normal_mode() {
        assert_eq!(
            map_key(&key(KeyCode::Char('/')), InputMode::Normal),
            Some(KeyPress::FilterStart)
        );
    }
}
