//! Stateless key-to-action mapping.
//!
//! Every key press is translated into an [`Action`] (or `None`) in a single
//! place, keeping the event loop in `tui.rs` thin and making it easy to
//! remap keys without touching application logic.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::actions::Action;

/// Map a key event to an [`Action`].
///
/// `search_active` indicates whether the search bar is currently open — in
/// that case, most keys are captured for search input rather than routed to
/// normal navigation.
pub fn map_key_event(key: KeyEvent, search_active: bool) -> Option<Action> {
    if search_active {
        map_search_key(key)
    } else {
        map_normal_key(key)
    }
}

// ── Search-bar mode ──────────────────────────────────────────────────────

fn map_search_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc => Some(Action::ToggleSearch),
        KeyCode::Enter => Some(Action::SearchConfirm),
        KeyCode::Up => Some(Action::SearchPrevResult),
        KeyCode::Down => Some(Action::SearchNextResult),
        KeyCode::Tab => Some(Action::SearchCycleMode),
        KeyCode::Backspace => Some(Action::SearchBackspace),
        KeyCode::Delete => Some(Action::SearchDeleteForward),
        KeyCode::Left => Some(Action::SearchCursorLeft),
        KeyCode::Right => Some(Action::SearchCursorRight),
        KeyCode::Home => Some(Action::SearchCursorHome),
        KeyCode::End => Some(Action::SearchCursorEnd),
        KeyCode::Char(c) => Some(Action::SearchInput(c)),
        _ => None,
    }
}

// ── Normal mode ──────────────────────────────────────────────────────────

fn map_normal_key(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        // Quit / help
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::Help),

        // Workspace creation
        KeyCode::Char('n') if ctrl => Some(Action::NewWorkspace),
        KeyCode::Char('t') if ctrl => Some(Action::NewWorkspace),

        // Workspace close / cycle
        KeyCode::Char('W') => Some(Action::CloseWorkspace),
        KeyCode::Char(']') => Some(Action::NextWorkspace),
        KeyCode::Char('[') => Some(Action::PreviousWorkspace),
        KeyCode::Tab if ctrl => Some(Action::NextWorkspace),
        KeyCode::BackTab if ctrl => Some(Action::PreviousWorkspace),

        // Workspace direct switch (1-9)
        KeyCode::Char(c @ '1'..='9') => {
            let index = (c as usize) - ('1' as usize);
            Some(Action::SwitchWorkspace(index))
        }

        // Search bar
        KeyCode::Char('f') => Some(Action::ToggleSearch),

        // Viewport panning (arrow keys)
        KeyCode::Up => Some(Action::MoveUp),
        KeyCode::Down => Some(Action::MoveDown),
        KeyCode::Right => Some(Action::MoveRight),
        KeyCode::Left => Some(Action::MoveLeft),

        // Graph navigation (hjkl)
        KeyCode::Char('h') => Some(Action::NavigateParent),
        KeyCode::Char('H') => Some(Action::CycleParent),
        KeyCode::Char('l') => Some(Action::NavigateChild),
        KeyCode::Char('k') => Some(Action::NavigatePrevSibling),
        KeyCode::Char('j') => Some(Action::NavigateNextSibling),

        // Node interaction
        KeyCode::Enter => Some(Action::ExpandOrCollapse),
        KeyCode::Char('r') => Some(Action::ResetView),
        KeyCode::Char('F') => Some(Action::FindReferences),
        KeyCode::Char('t') => Some(Action::ToggleCallDirection),
        KeyCode::Char('R') => Some(Action::Refresh),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn press_ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn normal_mode_quit() {
        assert_eq!(
            map_key_event(press(KeyCode::Char('q')), false),
            Some(Action::Quit)
        );
        assert_eq!(
            map_key_event(press(KeyCode::Esc), false),
            Some(Action::Quit)
        );
    }

    #[test]
    fn normal_mode_workspace_digits() {
        for digit in '1'..='9' {
            let expected = (digit as usize) - ('1' as usize);
            assert_eq!(
                map_key_event(press(KeyCode::Char(digit)), false),
                Some(Action::SwitchWorkspace(expected))
            );
        }
    }

    #[test]
    fn normal_mode_search_toggle() {
        assert_eq!(
            map_key_event(press(KeyCode::Char('f')), false),
            Some(Action::ToggleSearch)
        );
    }

    #[test]
    fn search_mode_esc_closes() {
        assert_eq!(
            map_key_event(press(KeyCode::Esc), true),
            Some(Action::ToggleSearch)
        );
    }

    #[test]
    fn search_mode_char_input() {
        assert_eq!(
            map_key_event(press(KeyCode::Char('a')), true),
            Some(Action::SearchInput('a'))
        );
    }

    #[test]
    fn search_mode_navigation() {
        assert_eq!(
            map_key_event(press(KeyCode::Up), true),
            Some(Action::SearchPrevResult)
        );
        assert_eq!(
            map_key_event(press(KeyCode::Down), true),
            Some(Action::SearchNextResult)
        );
    }

    #[test]
    fn ctrl_n_creates_workspace() {
        assert_eq!(
            map_key_event(press_ctrl(KeyCode::Char('n')), false),
            Some(Action::NewWorkspace)
        );
    }
}
