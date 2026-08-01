//! Painting a [`ViewSnapshot`] into a cell grid (SPEC §11).
//!
//! A custom widget rather than `Paragraph`: the coordinate model is Zed's, not
//! Ratatui's line-wrapping model, and the display map has already decided where
//! every row breaks.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::cell::cluster_cells;
use crate::snapshot::{CellRect, EditorView, StatusView, ViewSnapshot};

pub fn render(snapshot: &ViewSnapshot, buffer: &mut Buffer) {
    if let Some(editor) = &snapshot.editor {
        render_editor(editor, buffer);
    }
    render_status(&snapshot.status, snapshot.columns, snapshot.rows, buffer);
}

fn render_editor(editor: &EditorView, buffer: &mut Buffer) {
    let area = clamp(editor.text_rect, buffer.area);
    for (offset, row) in editor.rows.iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        render_row(&row.text, editor.scroll_columns, area, offset, buffer);
    }
}

/// Places each grapheme at the cell the measurement side assigned it. A
/// double-width grapheme occupies its own cell and leaves the next one empty,
/// which is exactly what Ratatui's diffing expects to find there.
fn render_row(text: &str, scroll_columns: u16, area: Rect, row_offset: u16, buffer: &mut Buffer) {
    let y = area.y + row_offset;
    let mut column: u32 = 0;

    for cluster in text.graphemes(true) {
        let width = cluster_cells(cluster);
        if width == 0 {
            continue;
        }

        let start = column;
        column += width;

        let Some(visible_start) = start.checked_sub(scroll_columns as u32) else {
            continue;
        };
        if visible_start >= area.width as u32 {
            break;
        }

        let x = area.x as u32 + visible_start;
        let Ok(x) = u16::try_from(x) else {
            break;
        };
        if let Some(cell) = buffer.cell_mut((x, y)) {
            cell.set_symbol(cluster);
        }

        for trailing in 1..width {
            let trailing = x as u32 + trailing;
            if trailing >= (area.x + area.width) as u32 {
                break;
            }
            let Ok(trailing) = u16::try_from(trailing) else {
                break;
            };
            if let Some(cell) = buffer.cell_mut((trailing, y)) {
                cell.set_symbol("");
            }
        }
    }
}

fn render_status(status: &StatusView, columns: u16, rows: u16, buffer: &mut Buffer) {
    let Some(y) = rows.checked_sub(1) else {
        return;
    };
    if columns == 0 {
        return;
    }

    let mut line = String::new();
    if let Some(mode) = &status.mode {
        line.push_str(mode);
        line.push(' ');
    }
    line.push_str(status.path.as_deref().unwrap_or("[No Name]"));
    if status.dirty {
        line.push_str(" [+]");
    }
    if let Some(message) = &status.message {
        line.push_str(" — ");
        line.push_str(message);
    }

    render_row(&line, 0, Rect::new(0, y, columns, 1), 0, buffer);
}

fn clamp(rect: CellRect, area: Rect) -> Rect {
    let x = rect.x.min(area.width);
    let y = rect.y.min(area.height);
    Rect::new(
        x,
        y,
        rect.width.min(area.width.saturating_sub(x)),
        rect.height.min(area.height.saturating_sub(y)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::RowView;

    fn grid(snapshot: &ViewSnapshot) -> Vec<String> {
        let mut buffer = Buffer::empty(Rect::new(0, 0, snapshot.columns, snapshot.rows));
        render(snapshot, &mut buffer);
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
            })
            .collect()
    }

    fn snapshot_of(columns: u16, rows: u16, lines: &[&str]) -> ViewSnapshot {
        ViewSnapshot {
            columns,
            rows,
            editor: Some(EditorView {
                text_rect: CellRect::new(0, 0, columns, rows.saturating_sub(1)),
                scroll_columns: 0,
                rows: lines
                    .iter()
                    .enumerate()
                    .map(|(index, line)| RowView::new(index as u32, (*line).to_owned()))
                    .collect(),
                max_display_row: lines.len().saturating_sub(1) as u32,
                soft_wrapped: true,
            }),
            status: StatusView::default(),
            cursor: None,
        }
    }

    #[test]
    fn rows_land_on_their_own_lines() {
        let snapshot = snapshot_of(8, 4, &["one", "two", "three"]);
        let grid = grid(&snapshot);
        assert_eq!(grid[0], "one     ");
        assert_eq!(grid[1], "two     ");
        assert_eq!(grid[2], "three   ");
    }

    #[test]
    fn wide_characters_take_two_cells_and_blank_the_next() {
        let snapshot = snapshot_of(8, 2, &["日本a"]);
        let grid = grid(&snapshot);
        // Each wide grapheme occupies its cell; the trailing cell is emptied,
        // so the row reads as the graphemes followed by padding.
        assert_eq!(grid[0], "日本a   ");
    }

    #[test]
    fn text_is_clipped_at_the_right_edge() {
        let snapshot = snapshot_of(4, 2, &["abcdefgh"]);
        assert_eq!(grid(&snapshot)[0], "abcd");
    }

    #[test]
    fn horizontal_scroll_skips_leading_cells() {
        let mut snapshot = snapshot_of(4, 2, &["abcdefgh"]);
        if let Some(editor) = snapshot.editor.as_mut() {
            editor.scroll_columns = 3;
        }
        assert_eq!(grid(&snapshot)[0], "defg");
    }

    #[test]
    fn rows_beyond_the_text_rect_are_dropped() {
        let snapshot = snapshot_of(4, 3, &["a", "b", "c", "d"]);
        let grid = grid(&snapshot);
        assert_eq!(grid[0], "a   ");
        assert_eq!(grid[1], "b   ");
        // Row index 2 is the status line, so "c" and "d" have nowhere to go.
        assert_eq!(grid[2], "[No Name]".chars().take(4).collect::<String>());
    }

    #[test]
    fn combining_marks_ride_along_with_their_base() {
        let snapshot = snapshot_of(4, 2, &["e\u{0301}x"]);
        let grid = grid(&snapshot);
        assert_eq!(grid[0], "e\u{0301}x  ");
    }

    #[test]
    fn the_status_line_reports_the_path_and_dirty_state() {
        let mut snapshot = snapshot_of(20, 2, &["x"]);
        snapshot.status = StatusView {
            path: Some("a.rs".to_owned()),
            dirty: true,
            mode: Some("NORMAL".to_owned()),
            message: None,
        };
        assert_eq!(grid(&snapshot)[1], "NORMAL a.rs [+]     ");
    }

    #[test]
    fn an_empty_snapshot_paints_nothing_but_the_status_line() {
        let snapshot = ViewSnapshot {
            columns: 12,
            rows: 2,
            editor: None,
            status: StatusView::default(),
            cursor: None,
        };
        let grid = grid(&snapshot);
        assert_eq!(grid[0], "            ");
        assert_eq!(grid[1], "[No Name]   ");
    }
}
