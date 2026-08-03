//! Terminal event to GPUI input translation (SPEC §8).
//!
//! The key names produced here are the ones Zed's keymaps are written in, so
//! the table mirrors `gpui_linux`'s xkb translation
//! (`crates/gpui_linux/src/linux/platform.rs`) rather than inventing a
//! vocabulary: named keys are words, printable keys are the character the
//! terminal reported, and `key_char` is set only for characters that would
//! actually be typed.

use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use gpui::{
    Keystroke, Modifiers, MouseButton as GpuiMouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point,
    px,
};

use crate::cell::{CELL_HEIGHT, CELL_WIDTH};

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
    if modifiers.shift && key.chars().count() == 1 && key.to_lowercase() == key.to_uppercase() {
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

/// Rows scrolled per wheel notch, the same step GPUI's own platforms take.
const SCROLL_LINES: f32 = 3.0;

/// How long after a click a second one at the same cell is a double click.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(300);

/// Terminal mouse reports translated into the input a platform would deliver
/// (SPEC §17).
///
/// The element tree really was laid out at this geometry, so hit testing,
/// click-to-place-cursor, drag-select and double-click-to-select-word need no
/// editor-side code — only a position in the window's pixels and a click count,
/// which is the one thing a terminal does not report and this has to keep.
#[derive(Default)]
pub struct Mouse {
    last_click: Option<Click>,
    /// What is held down, so a drag can name the button the terminal reports
    /// without one and a move outside a drag names none.
    pressed: Option<GpuiMouseButton>,
}

struct Click {
    button: GpuiMouseButton,
    cell: (u16, u16),
    at: Instant,
    count: usize,
}

impl Mouse {
    /// The input for a terminal mouse report.
    ///
    /// The window's row 0 is the terminal's row 0: every row `ted` paints itself
    /// is at the *bottom* of the grid (SPEC §10.2), and the one that was not —
    /// the tab strip — became a row inside the pane in M4 (SPEC §25.2). So a
    /// report is scaled into pixels and nothing is subtracted from it; a click
    /// below the window lands on a row `ted` owns, where GPUI's own hit testing
    /// finds nothing.
    pub fn translate(&mut self, event: &MouseEvent) -> Option<PlatformInput> {
        let row = event.row;
        let column = event.column;
        // The cell's left edge rather than its middle: a click on cell N means
        // the cursor goes to column N, which is where that boundary is.
        let position = point(
            px(f32::from(column) * f32::from(CELL_WIDTH)),
            px(f32::from(row) * f32::from(CELL_HEIGHT)),
        );
        let modifiers = Modifiers {
            control: event.modifiers.contains(KeyModifiers::CONTROL),
            alt: event.modifiers.contains(KeyModifiers::ALT)
                || event.modifiers.contains(KeyModifiers::META),
            shift: event.modifiers.contains(KeyModifiers::SHIFT),
            platform: event.modifiers.contains(KeyModifiers::SUPER),
            function: false,
        };

        Some(match event.kind {
            MouseEventKind::Down(button) => {
                let button = gpui_button(button);
                let click_count = self.count_click(button, (column, row));
                self.pressed = Some(button);
                PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position,
                    modifiers,
                    click_count,
                    first_mouse: false,
                })
            }
            MouseEventKind::Up(button) => {
                let button = gpui_button(button);
                self.pressed = None;
                PlatformInput::MouseUp(MouseUpEvent {
                    button,
                    position,
                    modifiers,
                    click_count: self.last_click.as_ref().map_or(1, |click| click.count),
                })
            }
            MouseEventKind::Drag(button) => PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: Some(gpui_button(button)),
                modifiers,
            }),
            MouseEventKind::Moved => PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: self.pressed,
                modifiers,
            }),
            MouseEventKind::ScrollUp => scroll(position, modifiers, point(0.0, SCROLL_LINES)),
            MouseEventKind::ScrollDown => scroll(position, modifiers, point(0.0, -SCROLL_LINES)),
            MouseEventKind::ScrollLeft => scroll(position, modifiers, point(SCROLL_LINES, 0.0)),
            MouseEventKind::ScrollRight => scroll(position, modifiers, point(-SCROLL_LINES, 0.0)),
        })
    }

    /// Consecutive clicks of the same button on the same cell, which is what
    /// double-click-to-select-word is counting. A terminal reports the cell and
    /// not the pixel, so "the same place" can only mean the same cell.
    fn count_click(&mut self, button: GpuiMouseButton, cell: (u16, u16)) -> usize {
        let now = Instant::now();
        let count = match self.last_click.take() {
            Some(previous)
                if previous.button == button
                    && previous.cell == cell
                    && now.duration_since(previous.at) < DOUBLE_CLICK_INTERVAL =>
            {
                previous.count + 1
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            button,
            cell,
            at: now,
            count,
        });
        count
    }
}

fn scroll(position: Point<Pixels>, modifiers: Modifiers, lines: Point<f32>) -> PlatformInput {
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(lines),
        modifiers,
        touch_phase: TouchPhase::Moved,
    })
}

fn gpui_button(button: MouseButton) -> GpuiMouseButton {
    match button {
        MouseButton::Left => GpuiMouseButton::Left,
        MouseButton::Right => GpuiMouseButton::Right,
        MouseButton::Middle => GpuiMouseButton::Middle,
    }
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
        assert_eq!(
            actual.modifiers, expected.modifiers,
            "modifiers for {expected:?}"
        );
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
        let keystroke = key(KeyCode::Char('G'), KeyModifiers::SHIFT).expect("expected a keystroke");
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

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_cell_becomes_the_pixel_at_its_top_left_corner() {
        let mut mouse = Mouse::default();
        let input = mouse
            .translate(&mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 5))
            .expect("a click inside the window");
        let PlatformInput::MouseDown(event) = input else {
            panic!("a button press should arrive as a press");
        };
        assert_eq!(event.position.x, CELL_WIDTH * 3.0);
        assert_eq!(event.position.y, CELL_HEIGHT * 5.0);
        assert_eq!(event.click_count, 1);
    }

    /// The window's row 0 is the terminal's row 0 from M4 on: the tab strip is a
    /// row inside the pane rather than one withheld from the top of the window
    /// (SPEC §25.2), so a report is scaled with nothing subtracted from it.
    #[test]
    fn the_windows_first_row_is_the_terminals() {
        let mut mouse = Mouse::default();
        let input = mouse
            .translate(&mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0))
            .expect("a click on the first row");
        let PlatformInput::MouseDown(event) = input else {
            panic!("a button press should arrive as a press");
        };
        assert_eq!(event.position.y, Pixels::ZERO);
    }

    #[test]
    fn a_second_click_on_the_same_cell_counts_up_and_one_elsewhere_does_not() {
        let mut mouse = Mouse::default();
        let press = mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 2);
        let elsewhere = mouse_event(MouseEventKind::Down(MouseButton::Left), 9, 2);

        let counts =
            [&press, &press, &press, &elsewhere].map(|event| match mouse.translate(event) {
                Some(PlatformInput::MouseDown(event)) => event.click_count,
                _ => panic!("a button press should arrive as a press"),
            });
        assert_eq!(counts, [1, 2, 3, 1]);
    }

    #[test]
    fn the_wheel_scrolls_whole_lines() {
        let mut mouse = Mouse::default();
        let Some(PlatformInput::ScrollWheel(event)) =
            mouse.translate(&mouse_event(MouseEventKind::ScrollDown, 0, 0))
        else {
            panic!("the wheel should arrive as a scroll");
        };
        let ScrollDelta::Lines(lines) = event.delta else {
            panic!("the wheel should scroll in lines");
        };
        assert_eq!(lines, point(0.0, -SCROLL_LINES));
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
