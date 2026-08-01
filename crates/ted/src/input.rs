//! Terminal event to GPUI input translation (SPEC §8).
//!
//! The key names produced here are the ones Zed's keymaps are written in, so
//! the table mirrors `gpui_linux`'s xkb translation
//! (`crates/gpui_linux/src/linux/platform.rs`) rather than inventing a
//! vocabulary: named keys are words, printable keys are the character the
//! terminal reported, and `key_char` is set only for characters that would
//! actually be typed.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gpui::{Keystroke, Modifiers};

/// Translates a crossterm key event into the keystroke a platform would deliver.
///
/// Returns `None` for events that carry no keystroke: key releases and bare
/// modifier presses, which the Kitty keyboard protocol reports and legacy mode
/// does not.
pub fn keystroke_for(event: &KeyEvent) -> Option<Keystroke> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }

    let mut modifiers = Modifiers {
        control: event.modifiers.contains(KeyModifiers::CONTROL),
        alt: event.modifiers.contains(KeyModifiers::ALT)
            || event.modifiers.contains(KeyModifiers::META),
        shift: event.modifiers.contains(KeyModifiers::SHIFT),
        platform: event.modifiers.contains(KeyModifiers::SUPER),
        function: false,
    };

    let (key, typed) = match event.code {
        KeyCode::Char(' ') => ("space".to_owned(), Some(" ".to_owned())),
        KeyCode::Char(character) => {
            let key = character.to_lowercase().collect::<String>();
            (key, Some(character.to_string()))
        }
        KeyCode::Backspace => ("backspace".to_owned(), None),
        KeyCode::Enter => ("enter".to_owned(), None),
        KeyCode::Left => ("left".to_owned(), None),
        KeyCode::Right => ("right".to_owned(), None),
        KeyCode::Up => ("up".to_owned(), None),
        KeyCode::Down => ("down".to_owned(), None),
        KeyCode::Home => ("home".to_owned(), None),
        KeyCode::End => ("end".to_owned(), None),
        KeyCode::PageUp => ("pageup".to_owned(), None),
        KeyCode::PageDown => ("pagedown".to_owned(), None),
        KeyCode::Tab => ("tab".to_owned(), None),
        KeyCode::BackTab => {
            modifiers.shift = true;
            ("tab".to_owned(), None)
        }
        KeyCode::Delete => ("delete".to_owned(), None),
        KeyCode::Insert => ("insert".to_owned(), None),
        KeyCode::Esc => ("escape".to_owned(), None),
        KeyCode::Menu => ("menu".to_owned(), None),
        KeyCode::F(number) => (format!("f{number}"), None),
        KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::KeypadBegin
        | KeyCode::Null
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => return None,
    };

    // Zed's keymaps spell shifted symbols as the symbol itself (`<`, not
    // `shift-,`), so shift is only a modifier for keys that have a case.
    if modifiers.shift
        && key.chars().count() == 1
        && key.to_lowercase() == key.to_uppercase()
    {
        modifiers.shift = false;
    }

    // A chord is a command, not text. Dropping `key_char` here is what stops
    // `ctrl-w` from also inserting a `w` when no binding claims it.
    let key_char = typed.filter(|_| !modifiers.control && !modifiers.platform);

    Some(Keystroke {
        modifiers,
        key,
        key_char,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Keystroke> {
        keystroke_for(&KeyEvent::new(code, modifiers))
    }

    fn parsed(source: &str) -> Keystroke {
        match Keystroke::parse(source) {
            Ok(keystroke) => keystroke,
            Err(error) => panic!("{error}"),
        }
    }

    #[track_caller]
    fn assert_binds_as(actual: Option<Keystroke>, expected: &str) {
        let actual = actual.expect("expected a keystroke");
        let expected = parsed(expected);
        assert_eq!(actual.modifiers, expected.modifiers, "modifiers for {expected:?}");
        assert_eq!(actual.key, expected.key, "key for {expected:?}");
    }

    #[test]
    fn motions_match_the_vim_keymap() {
        for motion in ["h", "j", "k", "l"] {
            let code = KeyCode::Char(motion.chars().next().unwrap_or('h'));
            let keystroke = key(code, KeyModifiers::NONE).expect("expected a keystroke");
            assert_eq!(keystroke.key, motion);
            assert_eq!(keystroke.modifiers, Modifiers::none());
            assert_eq!(keystroke.key_char.as_deref(), Some(motion));
        }
    }

    #[test]
    fn uppercase_letters_carry_shift_and_the_typed_character() {
        let keystroke =
            key(KeyCode::Char('G'), KeyModifiers::SHIFT).expect("expected a keystroke");
        assert_eq!(keystroke.key, "g");
        assert!(keystroke.modifiers.shift);
        assert_eq!(keystroke.key_char.as_deref(), Some("G"));
        assert_binds_as(key(KeyCode::Char('G'), KeyModifiers::SHIFT), "shift-g");
    }

    #[test]
    fn shifted_symbols_drop_the_shift_modifier() {
        for symbol in ['<', '>', '?', ':', '"', '{', '}', '|', '~', '!', '$', '%'] {
            let keystroke =
                key(KeyCode::Char(symbol), KeyModifiers::SHIFT).expect("expected a keystroke");
            assert_eq!(keystroke.key, symbol.to_string());
            assert!(!keystroke.modifiers.shift, "shift retained for {symbol}");
            assert_eq!(keystroke.key_char.as_deref(), Some(&*symbol.to_string()));
        }
    }

    #[test]
    fn control_chords_do_not_insert_text() {
        let keystroke =
            key(KeyCode::Char('d'), KeyModifiers::CONTROL).expect("expected a keystroke");
        assert_eq!(keystroke.key, "d");
        assert!(keystroke.modifiers.control);
        assert_eq!(keystroke.key_char, None);
        assert_binds_as(key(KeyCode::Char('d'), KeyModifiers::CONTROL), "ctrl-d");
    }

    #[test]
    fn named_keys_use_the_keymap_vocabulary() {
        assert_binds_as(key(KeyCode::Esc, KeyModifiers::NONE), "escape");
        assert_binds_as(key(KeyCode::Enter, KeyModifiers::NONE), "enter");
        assert_binds_as(key(KeyCode::Tab, KeyModifiers::NONE), "tab");
        assert_binds_as(key(KeyCode::BackTab, KeyModifiers::NONE), "shift-tab");
        assert_binds_as(key(KeyCode::Backspace, KeyModifiers::NONE), "backspace");
        assert_binds_as(key(KeyCode::Delete, KeyModifiers::NONE), "delete");
        assert_binds_as(key(KeyCode::Char(' '), KeyModifiers::NONE), "space");
        assert_binds_as(key(KeyCode::PageUp, KeyModifiers::NONE), "pageup");
        assert_binds_as(key(KeyCode::PageDown, KeyModifiers::NONE), "pagedown");
        assert_binds_as(key(KeyCode::Left, KeyModifiers::NONE), "left");
        assert_binds_as(key(KeyCode::F(7), KeyModifiers::NONE), "f7");
    }

    #[test]
    fn named_keys_never_insert_text() {
        for code in [
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Left,
            KeyCode::F(1),
        ] {
            let keystroke = key(code, KeyModifiers::NONE).expect("expected a keystroke");
            assert_eq!(keystroke.key_char, None, "{code:?} produced text");
        }
    }

    #[test]
    fn space_inserts_a_space() {
        let keystroke = key(KeyCode::Char(' '), KeyModifiers::NONE).expect("expected a keystroke");
        assert_eq!(keystroke.key, "space");
        assert_eq!(keystroke.key_char.as_deref(), Some(" "));
    }

    #[test]
    fn releases_and_bare_modifiers_produce_nothing() {
        let mut release = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(keystroke_for(&release), None);

        assert_eq!(
            key(
                KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftControl),
                KeyModifiers::CONTROL
            ),
            None
        );
    }
}
