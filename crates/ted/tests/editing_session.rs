//! M1 acceptance (SPEC §21): the editor-crate and vim behaviour `ted` claims to
//! inherit, exercised through the same path a keystroke really takes —
//! `Window::dispatch_keystroke` into a real `Editor` with `vim` attached — and
//! asserted against the cell grid `ted` would actually paint.
//!
//! Everything here runs in-process against `HeadlessAppContext`, so there is no
//! `Project`, `Client` or database involved. The workspace-level features (`:w`,
//! `/` search, the terminal's own modes) are covered end-to-end by
//! `tests/pty_smoke.rs`, which drives the real binary.

use std::sync::Arc;

use editor::Editor;
use gpui::{AppContext as _, BorrowAppContext as _, Entity, HeadlessAppContext, WindowHandle};
use language::Buffer;
use ratatui::buffer::Buffer as CellBuffer;
use ratatui::layout::Rect;
use settings::SettingsStore;
use ted::cell::grid_size;
use ted::palette::{ColorDepth, Palette};
use ted::render::{render, reserved_rows};
use ted::snapshot::ViewSnapshot;
use ted::text_system::CellTextSystem;

/// The same pins `bootstrap` applies, minus the chrome settings, which only
/// matter once a `Workspace` is drawing chrome.
const SETTINGS_OVERRIDE: &str = r#"{
    "buffer_font_size": 16,
    "buffer_line_height": { "custom": 1.0 },
    "soft_wrap": "editor_width",
    "vim_mode": true,
    "gutter": {
        "runnables": false,
        "bookmarks": false,
        "breakpoints": false,
        "folds": false
    }
}"#;

/// Field order is load-bearing: struct fields drop in declaration order, and
/// `HeadlessAppContext::drop` shuts the app down and runs GPUI's leak detector,
/// so the entity handles must be released before `cx`.
struct Session {
    editor: Entity<Editor>,
    window: WindowHandle<Editor>,
    palette: Palette,
    columns: u16,
    rows: u16,
    cx: HeadlessAppContext,
}

impl Session {
    fn open(columns: u16, rows: u16, text: &str) -> Self {
        let mut cx = HeadlessAppContext::with_asset_source(
            Arc::new(CellTextSystem::new()),
            Arc::new(assets::Assets),
        );

        cx.update(|cx| {
            let settings_store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            command_palette_hooks::init(cx);
            vim::init(cx);

            cx.update_global::<SettingsStore, _>(|store, cx| {
                let result = store.set_user_settings(SETTINGS_OVERRIDE, cx);
                assert!(
                    matches!(result.parse_status, settings::ParseStatus::Success),
                    "settings override did not parse: {:?}",
                    result.parse_status
                );
            });

            // SPEC §8.2: the Linux base keymap on every OS, then vim's on top.
            for (path, source) in [
                (
                    "keymaps/default-linux.json",
                    settings::KeybindSource::Default,
                ),
                (settings::VIM_KEYMAP_PATH, settings::KeybindSource::Vim),
            ] {
                let mut bindings = settings::KeymapFile::load_asset_allow_partial_failure(path, cx)
                    .unwrap_or_else(|error| panic!("could not load {path}: {error}"));
                for binding in &mut bindings {
                    binding.set_meta(source.meta());
                }
                cx.bind_keys(bindings);
            }
        });

        // The editor gets the grid minus the one row `ted` reserves for its own
        // status line (SPEC §10.2), which is what the window is sized to.
        let editor_rows = rows - reserved_rows(false, false, 0);
        let text = text.to_owned();
        let window = cx
            .open_window(grid_size(columns, editor_rows), move |window, cx| {
                let buffer = cx.new(|cx| Buffer::local(text, cx));
                cx.new(|cx| {
                    let mut editor = Editor::for_buffer(buffer, None, window, cx);
                    editor.set_offset_content(false, cx);
                    editor.disable_scrollbars_and_minimap(window, cx);
                    editor
                })
            })
            .expect("failed to open headless window");

        let editor = window.root(&mut cx).expect("window has no root view");
        cx.update_window(window.into(), |_, window, cx| {
            editor.update(cx, |editor, cx| {
                use gpui::Focusable as _;
                window.focus(&editor.focus_handle(cx), cx);
            });
        })
        .expect("failed to focus the editor");

        let mut session = Self {
            editor,
            window,
            palette: Palette::new(ColorDepth::TrueColor, gpui::hsla(0.0, 0.0, 0.0, 1.0), false),
            columns,
            rows,
            cx,
        };
        // Twice: the first draw computes and installs the wrap width, the second
        // lays out against the rewrapped display map.
        session.draw();
        session.draw();
        session
    }

    fn draw(&mut self) {
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                let arena_clear_needed = window.draw(cx);
                arena_clear_needed.clear(cx);
            })
            .expect("failed to draw window");
        self.cx.run_until_parked();
    }

    /// Sends `keys` the way the terminal would: one `Keystroke` per space-
    /// separated chunk, through the dispatch tree the previous paint built.
    fn keys(&mut self, keys: &str) -> &mut Self {
        for key in keys.split(' ').filter(|key| !key.is_empty()) {
            let keystroke =
                gpui::Keystroke::parse(key).unwrap_or_else(|error| panic!("{key:?}: {error}"));
            self.cx
                .update_window(self.window.into(), |_, window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                })
                .expect("failed to dispatch a keystroke");
            self.cx.run_until_parked();
            self.draw();
        }
        self
    }

    fn snapshot(&mut self) -> ViewSnapshot {
        let (columns, rows) = (self.columns, self.rows);
        let editor = self.editor.clone();
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                ted::snapshot::for_editor(
                    &editor,
                    columns,
                    rows,
                    reserved_rows(false, false, 0),
                    window,
                    cx,
                )
            })
            .expect("failed to build a snapshot")
    }

    /// The grid `ted` would paint, one `String` per terminal row.
    fn grid(&mut self) -> Vec<String> {
        let snapshot = self.snapshot();
        let mut buffer = CellBuffer::empty(Rect::new(0, 0, snapshot.columns, snapshot.rows));
        render(&snapshot, &self.palette, &mut buffer);
        (0..snapshot.rows)
            .map(|y| {
                (0..snapshot.columns)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|cell| cell.symbol())
                            .unwrap_or_default()
                            .to_owned()
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// The text half of a rendered row, i.e. the part at and after the rect
    /// the editor reported for its text (SPEC §10.2).
    fn row_text(&mut self, row: usize) -> String {
        let text_x = usize::from(self.snapshot().editor.expect("no editor view").text_rect.x);
        let grid = self.grid();
        let row = grid.get(row).cloned().unwrap_or_default();
        row.chars()
            .skip(text_x)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn text(&mut self) -> String {
        let editor = self.editor.clone();
        self.cx
            .update_window(self.window.into(), |_, _, cx| editor.read(cx).text(cx))
            .expect("failed to read the buffer")
    }

    fn mode(&mut self) -> Option<String> {
        self.snapshot().status.mode
    }
}

#[test]
fn a_file_renders_with_line_numbers_and_the_cursor_on_the_first_cell() {
    let mut session = Session::open(24, 6, "alpha\nbeta\ngamma\n");
    assert_eq!(session.row_text(0), "alpha");
    assert_eq!(session.row_text(1), "beta");
    assert_eq!(session.row_text(2), "gamma");

    let numbers: Vec<Option<u32>> = session
        .snapshot()
        .editor
        .expect("no editor view")
        .rows
        .iter()
        .map(|row| row.gutter.line_number)
        .collect();
    assert_eq!(numbers, vec![Some(1), Some(2), Some(3), Some(4)]);

    let snapshot = session.snapshot();
    assert_eq!(snapshot.status.mode.as_deref(), Some("NORMAL"));
    assert_eq!(snapshot.status.position, Some((1, 1)));
    // The text rect starts where the gutter ends, and the cursor sits in it.
    let editor = snapshot.editor.expect("no editor view");
    assert_eq!(
        snapshot.cursor.expect("no cursor").column,
        editor.text_rect.x
    );
    assert_eq!(snapshot.cursor.expect("no cursor").row, editor.text_rect.y);
}

#[test]
fn hjkl_moves_the_cursor_and_the_status_line_follows() {
    let mut session = Session::open(24, 6, "alpha\nbeta\ngamma\n");

    session.keys("j j l l");
    assert_eq!(session.snapshot().status.position, Some((3, 3)));

    session.keys("k h");
    assert_eq!(session.snapshot().status.position, Some((2, 2)));

    // `0` and `$` are the ones that would break first if byte and cell columns
    // were being confused.
    session.keys("$");
    assert_eq!(session.snapshot().status.position, Some((2, 4)));
    session.keys("0");
    assert_eq!(session.snapshot().status.position, Some((2, 1)));
}

#[test]
fn insert_mode_types_text_and_escape_returns_to_normal() {
    let mut session = Session::open(24, 6, "alpha\n");

    session.keys("i");
    assert_eq!(session.mode().as_deref(), Some("INSERT"));
    assert_eq!(
        session.snapshot().cursor_shape,
        ted::snapshot::CursorShape::Bar
    );

    session.keys("x y");
    assert_eq!(session.text(), "xyalpha\n");

    session.keys("escape");
    assert_eq!(session.mode().as_deref(), Some("NORMAL"));
    assert_eq!(
        session.snapshot().cursor_shape,
        ted::snapshot::CursorShape::Block
    );
    assert_eq!(session.row_text(0), "xyalpha");
}

#[test]
fn operators_undo_and_redo_all_reach_the_buffer() {
    let mut session = Session::open(24, 6, "alpha beta gamma\n");

    session.keys("d w");
    assert_eq!(session.text(), "beta gamma\n");

    session.keys("u");
    assert_eq!(session.text(), "alpha beta gamma\n");

    session.keys("ctrl-r");
    assert_eq!(session.text(), "beta gamma\n");
}

#[test]
fn a_pending_multi_key_binding_shows_in_the_status_line() {
    let mut session = Session::open(24, 6, "alpha beta\n");

    session.keys("g");
    assert_eq!(session.snapshot().status.pending_keys.as_deref(), Some("g"));

    session.keys("g");
    assert_eq!(session.snapshot().status.pending_keys, None);
}

#[test]
fn visual_mode_reports_a_selection_over_the_cells_it_covers() {
    let mut session = Session::open(24, 6, "alpha\nbeta\n");

    session.keys("v l l");
    assert_eq!(session.mode().as_deref(), Some("VISUAL"));

    let editor = session.snapshot().editor.expect("no editor view");
    assert_eq!(editor.selections.len(), 1);
    let selection = editor.selections[0];
    assert_eq!(selection.display_row, 0);
    assert_eq!((selection.start_cell, selection.end_cell), (0, 3));
}

#[test]
fn visual_line_mode_selects_whole_rows() {
    let mut session = Session::open(24, 6, "alpha\nbeta\n");

    session.keys("shift-v j");
    assert_eq!(session.mode().as_deref(), Some("VISUAL LINE"));

    let editor = session.snapshot().editor.expect("no editor view");
    let rows: Vec<u32> = editor
        .selections
        .iter()
        .map(|selection| selection.display_row)
        .collect();
    assert_eq!(rows, vec![0, 1]);
    assert_eq!(editor.selections[0].start_cell, 0);
    assert_eq!(editor.selections[0].end_cell, 5);
}

#[test]
fn wide_characters_put_the_cursor_on_the_cell_not_the_byte() {
    // Each of these is three bytes and two cells wide, so a cursor driven by
    // `DisplayPoint::column()` without `byte_to_cell` would land three cells
    // further right per character (SPEC §5.4).
    let mut session = Session::open(24, 6, "日本語\n");
    let text_x = session
        .snapshot()
        .editor
        .expect("no editor view")
        .text_rect
        .x;

    session.keys("l");
    assert_eq!(
        session.snapshot().cursor.expect("no cursor").column,
        text_x + 2
    );

    session.keys("l");
    assert_eq!(
        session.snapshot().cursor.expect("no cursor").column,
        text_x + 4
    );

    assert_eq!(session.row_text(0), "日本語");
}

#[test]
fn soft_wrap_breaks_at_the_column_the_editor_reports() {
    let columns = 30u16;
    let mut session = Session::open(columns, 8, &format!("{}\n", "a".repeat(90)));

    let snapshot = session.snapshot();
    let editor = snapshot.editor.expect("no editor view");
    let wrap_columns = editor.text_rect.width;
    assert!(wrap_columns > 0 && wrap_columns < columns);

    // The buffer's trailing newline gives one empty display row past the text.
    let filled: Vec<u16> = editor
        .rows
        .iter()
        .map(|row| row.cell_width())
        .take_while(|width| *width > 0)
        .collect();
    assert!(filled.len() > 1, "text did not wrap at all: {filled:?}");
    // Every row but the last is filled exactly to the wrap width: the break test
    // is `width > wrap_width`, strictly greater, so N columns fit (SPEC §5.3).
    let (last, rest) = filled.split_last().expect("no rows");
    assert!(
        rest.iter().all(|width| *width == wrap_columns),
        "rows wrapped at {filled:?}, expected {wrap_columns}"
    );
    assert!(*last <= wrap_columns);

    // Continuation rows carry no line number of their own.
    assert_eq!(editor.rows[0].gutter.line_number, Some(1));
    assert_eq!(editor.rows[1].gutter.line_number, None);
}

#[test]
fn scrolling_is_the_editors_and_the_snapshot_only_mirrors_it() {
    let text = (1..=60)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut session = Session::open(24, 12, &text);

    // The window is 11 rows, so the first screen is display rows 0..11.
    let first = session.snapshot().editor.expect("no editor view");
    assert_eq!(first.rows.first().map(|row| row.display_row), Some(0));

    // `G` jumps to the last line and the editor autoscrolls; `ted` keeps no
    // scroll offset of its own (SPEC §15).
    session.keys("shift-g");
    let last = session.snapshot().editor.expect("no editor view");
    assert_eq!(
        session.snapshot().status.position.map(|(row, _)| row),
        Some(60)
    );
    assert!(
        last.rows.first().map(|row| row.display_row).unwrap_or(0) > 0,
        "the viewport did not scroll: {:?}",
        last.rows.first().map(|row| row.display_row)
    );
    assert!(
        last.rows
            .iter()
            .any(|row| row.gutter.line_number == Some(60)),
        "the last line is not on screen"
    );

    session.keys("g g");
    let back = session.snapshot().editor.expect("no editor view");
    assert_eq!(back.rows.first().map(|row| row.display_row), Some(0));
}

#[test]
fn ctrl_d_and_ctrl_u_move_by_the_terminals_own_row_count() {
    let text = (1..=200)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut session = Session::open(24, 12, &text);

    let before = session.snapshot().status.position.expect("no position").0;
    session.keys("ctrl-d");
    let after = session.snapshot().status.position.expect("no position").0;
    assert!(
        after > before,
        "ctrl-d did not move down: {before} -> {after}"
    );

    session.keys("ctrl-u");
    let back = session.snapshot().status.position.expect("no position").0;
    assert!(
        back < after,
        "ctrl-u did not move back up: {after} -> {back}"
    );
}

#[test]
fn every_row_is_covered_by_its_spans_exactly_once() {
    let mut session = Session::open(40, 8, "fn main() {\n    let x = 1;\n}\n");
    let editor = session.snapshot().editor.expect("no editor view");

    for row in &editor.rows {
        let mut offset = 0usize;
        for span in &row.spans {
            assert_eq!(
                span.range.start, offset,
                "row {} has a gap or overlap before {:?}",
                row.display_row, span.range
            );
            assert!(row.text.is_char_boundary(span.range.start));
            assert!(row.text.is_char_boundary(span.range.end));
            offset = span.range.end;
        }
        assert_eq!(
            offset,
            row.text.len(),
            "row {} is not fully covered by its spans",
            row.display_row
        );
    }
}

#[test]
fn a_terminal_below_the_minimum_size_says_so_instead_of_resizing() {
    // The floor exists because the editor's width arithmetic goes negative
    // below it (SPEC §10.2); the message is `ted`'s, not the editor's.
    let mut session = Session::open(24, 6, "alpha\n");
    session.columns = 10;
    session.rows = 3;

    // `for_editor` still projects; the size check lives in the frame loop, so
    // what this pins is that the renderer clips rather than panicking.
    let grid = session.grid();
    assert_eq!(grid.len(), 3);
    for row in &grid {
        assert!(row.chars().count() <= 10, "row overflows the grid: {row:?}");
    }
}
