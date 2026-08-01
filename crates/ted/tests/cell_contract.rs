//! M0 acceptance (a) and (d) from `crates/ted/SPEC.md` §21: with the window
//! sized to an exact cell grid, does Zed's own layout agree with the terminal's
//! coordinate system, and how much of the grid does the editor not get?
//!
//! Everything here is asserted rather than eyeballed, and the quantities are
//! exact integers by construction — `CellTextSystem` defines the metrics, so
//! there is no font measurement to introduce fractional pixels. A test that
//! needed a tolerance would mean the cell contract had already failed.

use std::sync::Arc;

use editor::Editor;
use editor::display_map::DisplayRow;
use gpui::{
    AppContext as _, BorrowAppContext as _, Entity, HeadlessAppContext, Pixels, WindowHandle,
};
use language::Buffer;
use settings::SettingsStore;
use ted::cell::{CELL_HEIGHT, CELL_WIDTH, grid_size, text_cells};
use ted::text_system::CellTextSystem;

/// `EditorElement::prepaint` reserves `2 * em_width` of overscroll to the right
/// of the text (`extended_right`, `crates/editor/src/element.rs:8039`). Unlike
/// the gutter, the scrollbar and the minimap, nothing turns it off, so it
/// survives hiding all the chrome — which is exactly the inset M0 acceptance
/// (d) exists to measure and the SPEC §22 row "Chrome hidden by settings still
/// leaves the editor inset" predicts. If editor layout ever changes here, this
/// constant should fail loudly rather than every wrap boundary shifting
/// silently.
const OVERSCROLL_CELLS: u16 = 2;

/// Pins the two settings that make `cell.rs`'s constants true (SPEC §5.1) and
/// turns on width-based soft wrap so wrap boundaries are a function of the
/// window, which is what (a) is about.
const SETTINGS_OVERRIDE: &str = r#"{
    "buffer_font_size": 16,
    "buffer_line_height": { "custom": 1.0 },
    "soft_wrap": "editor_width"
}"#;

/// Field order is load-bearing: struct fields drop in declaration order, and
/// `HeadlessAppContext::drop` shuts the app down and runs GPUI's leak detector.
/// The entity handles must therefore be released before `cx`.
struct Harness {
    editor: Entity<Editor>,
    window: WindowHandle<Editor>,
    columns: u16,
    cx: HeadlessAppContext,
}

impl Harness {
    fn open(columns: u16, rows: u16, text: &str) -> Self {
        let mut cx = HeadlessAppContext::new(Arc::new(CellTextSystem::new()));

        cx.update(|cx| {
            let settings_store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);

            cx.update_global::<SettingsStore, _>(|store, cx| {
                let result = store.set_user_settings(SETTINGS_OVERRIDE, cx);
                assert!(
                    matches!(result.parse_status, settings::ParseStatus::Success),
                    "settings override did not parse: {:?}",
                    result.parse_status
                );
            });
        });

        let text = text.to_owned();
        let window = cx
            .open_window(grid_size(columns, rows), move |window, cx| {
                let buffer = cx.new(|cx| Buffer::local(text, cx));
                cx.new(|cx| {
                    let mut editor = Editor::for_buffer(buffer, None, window, cx);
                    // Together these zero every contribution to the editor's
                    // horizontal budget except the overscroll above, so the
                    // arithmetic below stays whole cells. `offset_content`
                    // matters even with the gutter hidden: it contributes a
                    // margin of `-descent` (`editor.rs:1263`), which is 0.4 of
                    // a cell under `CellTextSystem`'s metrics.
                    editor.set_show_gutter(false, cx);
                    editor.set_offset_content(false, cx);
                    editor.disable_scrollbars_and_minimap(window, cx);
                    editor
                })
            })
            .expect("failed to open headless window");

        let editor = window.root(&mut cx).expect("window has no root view");

        let mut harness = Self {
            editor,
            window,
            columns,
            cx,
        };
        // Twice: the first draw computes and installs the wrap width, the
        // second lays out against the rewrapped display map. Wrapping is
        // synchronous below `WRAP_YIELD_ROW_INTERVAL` (100) rows
        // (`display_map/wrap_map.rs:214`), which every buffer here is.
        harness.draw();
        harness.draw();
        harness
    }

    fn draw(&mut self) {
        self.cx
            .update_window(self.window.into(), |_, window, cx| {
                let arena_clear_needed = window.draw(cx);
                arena_clear_needed.clear(cx);
            })
            .expect("failed to draw window");
    }

    fn window_bounds(&mut self) -> gpui::Bounds<Pixels> {
        self.cx
            .update_window(self.window.into(), |_, window, _| window.bounds())
            .expect("failed to read window bounds")
    }

    fn last_bounds(&self) -> gpui::Bounds<Pixels> {
        self.editor.read_with(&self.cx, |editor, _| {
            *editor
                .last_bounds()
                .expect("editor reported no bounds; did the draw run?")
        })
    }

    fn visible_line_count(&self) -> f64 {
        self.editor
            .read_with(&self.cx, |editor, _| editor.visible_line_count())
            .expect("editor reported no visible line count")
    }

    fn visible_column_count(&self) -> f64 {
        self.editor
            .read_with(&self.cx, |editor, _| editor.visible_column_count())
            .expect("editor reported no visible column count")
    }

    /// The text of every display row, i.e. what soft wrap actually produced.
    fn display_rows(&mut self) -> Vec<String> {
        let window = self.window;
        self.cx
            .update_window(window.into(), |_, window, cx| {
                self.editor.update(cx, |editor, cx| {
                    let snapshot = editor.snapshot(window, cx);
                    let last_row = snapshot.display_snapshot.max_point().row().0;
                    (0..=last_row)
                        .map(|row| snapshot.display_snapshot.line(DisplayRow(row)))
                        .collect()
                })
            })
            .expect("failed to read display rows")
    }

    /// The number of cell columns the editor actually wraps at, derived the
    /// same way `EditorElement::prepaint` derives it.
    fn expected_wrap_columns(&self) -> u16 {
        self.columns - OVERSCROLL_CELLS
    }
}

#[test]
fn visible_line_count_equals_the_terminal_row_count() {
    for (columns, rows) in [(80u16, 24u16), (120, 40), (40, 10), (200, 60)] {
        let harness = Harness::open(columns, rows, "hello\n");

        // Exact: `bounds.size.height / line_height` is `rows * CELL_HEIGHT /
        // CELL_HEIGHT` (`element.rs:8050`), and `CellTextSystem` fixes both.
        assert_eq!(
            harness.visible_line_count(),
            f64::from(rows),
            "visible_line_count for a {columns}x{rows} grid"
        );
    }
}

#[test]
fn visible_column_count_is_the_grid_minus_the_overscroll_inset() {
    for (columns, rows) in [(80u16, 24u16), (120, 40), (40, 10)] {
        let harness = Harness::open(columns, rows, "hello\n");

        assert_eq!(
            harness.visible_column_count(),
            f64::from(columns - OVERSCROLL_CELLS),
            "visible_column_count for a {columns}x{rows} grid"
        );
    }
}

#[test]
fn the_editor_is_given_the_whole_window() {
    let mut harness = Harness::open(80, 24, "hello\n");

    let window_bounds = harness.window_bounds();
    let editor_bounds = harness.last_bounds();

    // The window's root view is the `Editor` entity itself — `impl Render for
    // Editor` returns `EditorElement` with no wrapper — so any difference here
    // would be an inset introduced by GPUI rather than by Zed's chrome.
    assert_eq!(editor_bounds.size, window_bounds.size);
    assert_eq!(editor_bounds.size.width, CELL_WIDTH * 80.0);
    assert_eq!(editor_bounds.size.height, CELL_HEIGHT * 24.0);
}

#[test]
fn ascii_lines_wrap_on_exact_cell_boundaries() {
    let columns = 40u16;
    let harness_columns = usize::from(columns);
    let mut harness = Harness::open(columns, 10, &"a".repeat(harness_columns * 3));
    let expected = harness.expected_wrap_columns();

    let rows = harness.display_rows();
    assert!(rows.len() > 1, "text did not wrap at all: {rows:?}");

    for (index, row) in rows.iter().enumerate() {
        let cells = text_cells(row);
        if index + 1 < rows.len() {
            // Every row but the last is filled exactly to the wrap width. The
            // break test is `width > wrap_width`, strictly greater
            // (`line_wrapper.rs:106`), so a line of exactly N cells against a
            // wrap width of N cells does not wrap — N columns fit.
            assert_eq!(
                cells,
                u32::from(expected),
                "row {index} of {} is not exactly the wrap width",
                rows.len()
            );
        } else {
            assert!(
                cells <= u32::from(expected),
                "final row {index} overflows the wrap width"
            );
        }
    }
}

#[test]
fn wide_characters_wrap_on_cell_boundaries_not_character_counts() {
    let columns = 40u16;
    // Each of these is two cells wide, so a row holds half as many of them.
    let mut harness = Harness::open(columns, 10, &"日".repeat(usize::from(columns) * 2));
    let expected = harness.expected_wrap_columns();

    let rows = harness.display_rows();
    assert!(rows.len() > 1, "wide text did not wrap at all");

    for (index, row) in rows.iter().enumerate() {
        let cells = text_cells(row);
        assert!(
            cells <= u32::from(expected),
            "row {index} occupies {cells} cells, over the {expected}-cell wrap width"
        );
        if index + 1 < rows.len() {
            // With an even wrap width and two-cell characters the rows fill
            // exactly; an odd wrap width would leave one cell unusable, which
            // is why this is stated as a range rather than equality.
            assert!(
                cells + 1 >= u32::from(expected),
                "row {index} wrapped early at {cells} cells, expected {expected}"
            );
            assert_eq!(cells % 2, 0, "a two-cell character was split across rows");
        }
    }
}

#[test]
fn the_measured_inset_is_the_overscroll_and_nothing_else() {
    // This is acceptance (d) stated as a number: with the gutter, its margin,
    // the scrollbar and the minimap all off, the difference between the grid
    // the terminal offers and the columns the editor wraps at is exactly the
    // `2 * em_width` overscroll — no more.
    for columns in [40u16, 80, 120, 200] {
        let mut harness = Harness::open(columns, 10, &"a".repeat(usize::from(columns) * 3));
        let rows = harness.display_rows();
        let widest = rows
            .iter()
            .take(rows.len().saturating_sub(1))
            .map(|row| text_cells(row))
            .max()
            .unwrap_or_default();

        let inset = u32::from(columns) - widest;
        assert_eq!(
            inset,
            u32::from(OVERSCROLL_CELLS),
            "measured inset for a {columns}-column grid: wrapped at {widest}"
        );
    }
}
