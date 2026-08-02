//! Painting a [`ViewSnapshot`] into a cell grid (SPEC §11).
//!
//! A custom widget rather than `Paragraph`: the coordinate model is Zed's, not
//! Ratatui's line-wrapping model, and the display map has already decided where
//! every row breaks. Everything here is pure — a snapshot and a palette in, a
//! Ratatui buffer out — so the whole renderer is testable without a terminal.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::cell::{cluster_cells, text_cells};
use crate::palette::Palette;
use crate::snapshot::{
    CellRect, CommandLineView, EditorView, PromptView, RowView, SpanStyle, StatusView, ViewSnapshot,
};

/// A prompt is always exactly this tall — the question on one row, the numbered
/// answers on the next — so the reserved-row count does not depend on how long
/// the question is. Both rows are clipped at the right edge like every other
/// line `ted` paints.
const PROMPT_ROWS: usize = 2;

/// The rows `ted` paints itself, bottom-up: the status line always, then the
/// `:` / `/` line when one is open, then a prompt when one is unanswered, then
/// one row per notification. The GPUI window is sized to the grid minus exactly
/// this many rows, so the editor's reported rect can never overlap them
/// (SPEC §10.2).
pub fn reserved_rows(command_line: bool, prompt: bool, notifications: usize) -> u16 {
    let reserved =
        1 + usize::from(command_line) + usize::from(prompt) * PROMPT_ROWS + notifications;
    u16::try_from(reserved).unwrap_or(u16::MAX)
}

pub fn render(snapshot: &ViewSnapshot, palette: &Palette, buffer: &mut Buffer) {
    if let Some(editor) = &snapshot.editor {
        render_editor(editor, palette, buffer);
    }

    let Some(status_row) = snapshot.rows.checked_sub(1) else {
        return;
    };
    render_status(&snapshot.status, snapshot.columns, status_row, buffer);

    let mut next_row = status_row;
    if let Some(command_line) = &snapshot.command_line {
        let Some(row) = next_row.checked_sub(1) else {
            return;
        };
        next_row = row;
        render_command_line(command_line, snapshot.columns, row, buffer);
    }

    // Directly above the status line, and above the `:` line when both are up:
    // it is the only thing on screen the user has to answer before anything
    // else happens.
    if let Some(prompt) = &snapshot.prompt {
        let Some(answers_row) = next_row.checked_sub(1) else {
            return;
        };
        let Some(message_row) = answers_row.checked_sub(1) else {
            return;
        };
        next_row = message_row;
        render_prompt(prompt, snapshot.columns, message_row, answers_row, buffer);
    }

    for notification in &snapshot.notifications {
        let Some(row) = next_row.checked_sub(1) else {
            return;
        };
        next_row = row;
        let area = Rect::new(0, row, snapshot.columns, 1);
        fill(
            area,
            Style::default().add_modifier(Modifier::REVERSED),
            buffer,
        );
        write(
            notification,
            area,
            Style::default().add_modifier(Modifier::REVERSED),
            buffer,
        );
    }
}

fn render_editor(editor: &EditorView, palette: &Palette, buffer: &mut Buffer) {
    let text_area = clamp(editor.text_rect, buffer.area);
    let gutter_area = clamp(editor.gutter_rect, buffer.area);

    let surface = editor
        .background
        .and_then(|background| palette.surface_background(background));
    if let Some(surface) = surface {
        fill(gutter_area, Style::default().bg(surface), buffer);
        fill(text_area, Style::default().bg(surface), buffer);
    }

    for (offset, row) in editor.rows.iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        if offset >= text_area.height {
            break;
        }

        render_gutter(row, gutter_area, offset, palette, buffer);
        render_row(
            row,
            editor.scroll_columns,
            text_area,
            offset,
            palette,
            buffer,
        );
    }

    for selection in &editor.selections {
        paint_selection(editor, selection, text_area, palette, buffer);
    }

    for cursor in &editor.secondary_cursors {
        // The terminal has one hardware cursor, which the primary selection
        // already owns, so every other cursor is drawn as an inverted cell.
        if let Some(cell) = buffer.cell_mut((cursor.column, cursor.row)) {
            cell.modifier.insert(Modifier::REVERSED);
        }
    }
}

fn render_gutter(
    row: &RowView,
    area: Rect,
    row_offset: u16,
    palette: &Palette,
    buffer: &mut Buffer,
) {
    if area.width == 0 {
        return;
    }
    let y = area.y + row_offset;
    let style = terminal_style(&row.gutter.style, palette);

    if let Some(diff) = row.gutter.diff
        && let Some(cell) = buffer.cell_mut((area.x, y))
    {
        cell.set_symbol(&diff.symbol().to_string());
        cell.set_style(style);
    }

    let Some(line_number) = row.gutter.line_number else {
        return;
    };
    let text = line_number.to_string();
    let Ok(width) = u16::try_from(text.len()) else {
        return;
    };
    // Right-aligned one cell short of the gutter's right edge, so the number
    // never abuts the first column of code.
    let Some(start) = (area.x + area.width).checked_sub(width + 1) else {
        return;
    };
    if start < area.x {
        return;
    }
    write(&text, Rect::new(start, y, width, 1), style, buffer);
}

/// Places each grapheme at the cell the measurement side assigned it. A
/// double-width grapheme occupies its own cell and leaves the next one empty,
/// which is exactly what Ratatui's diffing expects to find there.
fn render_row(
    row: &RowView,
    scroll_columns: u16,
    area: Rect,
    row_offset: u16,
    palette: &Palette,
    buffer: &mut Buffer,
) {
    let y = area.y + row_offset;
    let mut spans = row.spans.iter().peekable();
    let mut column: u32 = 0;

    for (byte, cluster) in row.text.grapheme_indices(true) {
        while spans.peek().is_some_and(|span| span.range.end <= byte) {
            spans.next();
        }
        let style = spans
            .peek()
            .filter(|span| span.range.contains(&byte))
            .map(|span| terminal_style(&span.style, palette))
            .unwrap_or_default();

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
            cell.set_style(style);
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
                cell.set_style(style);
            }
        }
    }
}

/// Applies the selection background over cells that already carry their syntax
/// colours, so selecting text never loses the highlighting underneath it.
fn paint_selection(
    editor: &EditorView,
    selection: &crate::snapshot::SelectionSpan,
    area: Rect,
    palette: &Palette,
    buffer: &mut Buffer,
) {
    let Some(row_offset) = editor
        .rows
        .iter()
        .position(|row| row.display_row == selection.display_row)
    else {
        return;
    };
    let Ok(row_offset) = u16::try_from(row_offset) else {
        return;
    };
    if row_offset >= area.height {
        return;
    }

    let background = editor
        .selection_background
        .map(|color| palette.color(color));
    let y = area.y + row_offset;

    for cell_column in selection.start_cell..selection.end_cell {
        let Some(visible) = cell_column.checked_sub(editor.scroll_columns) else {
            continue;
        };
        if visible >= area.width {
            break;
        }
        let Some(cell) = buffer.cell_mut((area.x + visible, y)) else {
            continue;
        };
        match background {
            Some(color) => {
                cell.set_bg(color);
            }
            // Without a theme colour to use, inverting is the one thing a
            // terminal can always do and never makes text unreadable.
            None => cell.modifier.insert(Modifier::REVERSED),
        }
    }
}

fn render_status(status: &StatusView, columns: u16, row: u16, buffer: &mut Buffer) {
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
    if let Some((index, count)) = status.panes {
        line.push_str(&format!(" [pane {index}/{count}]"));
    }
    if status.rewrapping {
        line.push_str(" [wrapping…]");
    }

    let mut right = String::new();
    if let Some(pending) = &status.pending_keys {
        right.push_str(pending);
        right.push(' ');
    }
    if let Some((line_number, column)) = status.position {
        right.push_str(&format!("{line_number}:{column}"));
    }

    let style = Style::default().add_modifier(Modifier::REVERSED);
    let area = Rect::new(0, row, columns, 1);
    fill(area, style, buffer);
    write(&line, area, style, buffer);

    let right_cells = text_cells(&right).min(u32::from(columns)) as u16;
    if right_cells > 0 {
        let start = columns - right_cells;
        write(&right, Rect::new(start, row, right_cells, 1), style, buffer);
    }
}

fn render_command_line(
    command_line: &CommandLineView,
    columns: u16,
    row: u16,
    buffer: &mut Buffer,
) {
    if columns == 0 {
        return;
    }
    let area = Rect::new(0, row, columns, 1);
    fill(area, Style::default(), buffer);

    let mut line = String::new();
    line.push(command_line.prefix);
    line.push_str(&command_line.query);

    // Completions share the line with the query rather than opening a popup:
    // the selected one is what `enter` will dispatch, so it has to be visible.
    if let Some(selected) = command_line
        .selected_completion
        .and_then(|index| command_line.completions.get(index))
    {
        line.push_str("  → ");
        line.push_str(selected);
    }
    if let Some(message) = &command_line.message {
        line.push_str("  ");
        line.push_str(message);
    }

    write(&line, area, Style::default(), buffer);
}

/// Paints the question on `message_row` and its numbered answers on
/// `answers_row` (SPEC §13.3).
///
/// The question is reversed like a notification, because it is one until it is
/// answered; the answers are painted plainly, like the `:` line, because that
/// row is what the user is about to type into.
fn render_prompt(
    prompt: &PromptView,
    columns: u16,
    message_row: u16,
    answers_row: u16,
    buffer: &mut Buffer,
) {
    if columns == 0 {
        return;
    }

    let mut message = prompt.message.clone();
    if let Some(detail) = &prompt.detail {
        message.push_str(" — ");
        message.push_str(detail);
    }

    let reversed = Style::default().add_modifier(Modifier::REVERSED);
    let message_area = Rect::new(0, message_row, columns, 1);
    fill(message_area, reversed, buffer);
    write(&message, message_area, reversed, buffer);

    let mut answers = String::new();
    for (index, answer) in prompt.answers.iter().enumerate() {
        if !answers.is_empty() {
            answers.push_str("  ");
        }
        // Numbered from 1, because that is the key the user presses.
        answers.push_str(&format!("[{}] {answer}", index + 1));
    }

    let answers_area = Rect::new(0, answers_row, columns, 1);
    fill(answers_area, Style::default(), buffer);
    write(&answers, answers_area, Style::default(), buffer);
}

fn terminal_style(style: &SpanStyle, palette: &Palette) -> Style {
    let mut result = Style::default();
    if let Some(foreground) = style.foreground {
        result = result.fg(palette.color(foreground));
    }
    if let Some(background) = style.background {
        result = result.bg(palette.color(background));
    }
    if style.bold {
        result = result.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        result = result.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        result = result.add_modifier(Modifier::UNDERLINED);
    }
    if style.strikethrough {
        result = result.add_modifier(Modifier::CROSSED_OUT);
    }
    result
}

/// Writes plain text into `area`, clipping at its right edge and honouring
/// cell widths the same way [`render_row`] does.
fn write(text: &str, area: Rect, style: Style, buffer: &mut Buffer) {
    let mut column: u32 = 0;
    for cluster in text.graphemes(true) {
        let width = cluster_cells(cluster);
        if width == 0 {
            continue;
        }
        if column + width > area.width as u32 {
            break;
        }
        let Ok(x) = u16::try_from(area.x as u32 + column) else {
            break;
        };
        if let Some(cell) = buffer.cell_mut((x, area.y)) {
            cell.set_symbol(cluster);
            cell.set_style(style);
        }
        for trailing in 1..width {
            let Ok(trailing) = u16::try_from(x as u32 + trailing) else {
                break;
            };
            if let Some(cell) = buffer.cell_mut((trailing, area.y)) {
                cell.set_symbol("");
                cell.set_style(style);
            }
        }
        column += width;
    }
}

fn fill(area: Rect, style: Style, buffer: &mut Buffer) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(" ");
                cell.set_style(style);
            }
        }
    }
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
    use crate::palette::ColorDepth;
    use crate::snapshot::{CellPoint, DiffMarker, GutterView, SelectionSpan, StyledSpan};
    use gpui::hsla;

    fn palette() -> Palette {
        Palette::new(ColorDepth::TrueColor, hsla(0.0, 0.0, 0.0, 1.0), false)
    }

    fn grid(snapshot: &ViewSnapshot) -> Vec<String> {
        let mut buffer = Buffer::empty(Rect::new(0, 0, snapshot.columns, snapshot.rows));
        render(snapshot, &palette(), &mut buffer);
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
                rows: lines
                    .iter()
                    .enumerate()
                    .map(|(index, line)| RowView::new(index as u32, (*line).to_owned()))
                    .collect(),
                max_display_row: lines.len().saturating_sub(1) as u32,
                soft_wrapped: true,
                ..Default::default()
            }),
            ..Default::default()
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
    fn the_status_line_reports_mode_path_and_dirty_state() {
        let mut snapshot = snapshot_of(30, 2, &["x"]);
        snapshot.status = StatusView {
            mode: Some("NORMAL".to_owned()),
            path: Some("a.rs".to_owned()),
            dirty: true,
            position: Some((3, 7)),
            ..Default::default()
        };
        assert_eq!(grid(&snapshot)[1], "NORMAL a.rs [+]            3:7");
    }

    #[test]
    fn pending_keys_sit_beside_the_cursor_position() {
        let mut snapshot = snapshot_of(20, 2, &["x"]);
        snapshot.status = StatusView {
            pending_keys: Some("d2".to_owned()),
            position: Some((1, 1)),
            ..Default::default()
        };
        assert_eq!(grid(&snapshot)[1], "[No Name]     d2 1:1");
    }

    #[test]
    fn line_numbers_are_right_aligned_one_cell_clear_of_the_text() {
        let mut snapshot = snapshot_of(10, 2, &["fn main"]);
        if let Some(editor) = snapshot.editor.as_mut() {
            editor.gutter_rect = CellRect::new(0, 0, 4, 1);
            editor.text_rect = CellRect::new(4, 0, 6, 1);
            editor.rows[0].gutter = GutterView {
                line_number: Some(12),
                ..Default::default()
            };
        }
        assert_eq!(grid(&snapshot)[0], " 12 fn mai");
    }

    #[test]
    fn a_diff_marker_takes_the_leftmost_gutter_cell() {
        let mut snapshot = snapshot_of(10, 2, &["x"]);
        if let Some(editor) = snapshot.editor.as_mut() {
            editor.gutter_rect = CellRect::new(0, 0, 4, 1);
            editor.text_rect = CellRect::new(4, 0, 6, 1);
            editor.rows[0].gutter = GutterView {
                line_number: Some(7),
                diff: Some(DiffMarker::Added),
                ..Default::default()
            };
        }
        assert_eq!(grid(&snapshot)[0], "+ 7 x     ");
    }

    #[test]
    fn selections_recolour_cells_without_replacing_their_text() {
        let mut snapshot = snapshot_of(8, 2, &["abcdef"]);
        if let Some(editor) = snapshot.editor.as_mut() {
            editor.rows[0].spans = vec![StyledSpan {
                range: 0..6,
                style: SpanStyle {
                    foreground: Some(hsla(0.1, 0.5, 0.6, 1.0)),
                    ..Default::default()
                },
            }];
            editor.selection_background = Some(hsla(0.6, 0.5, 0.3, 1.0));
            editor.selections = vec![SelectionSpan {
                display_row: 0,
                start_cell: 1,
                end_cell: 4,
            }];
        }
        assert_eq!(grid(&snapshot)[0], "abcdef  ");

        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        render(&snapshot, &palette(), &mut buffer);
        let selected = palette().color(hsla(0.6, 0.5, 0.3, 1.0));
        for x in 1..4u16 {
            assert_eq!(buffer.cell((x, 0)).map(|cell| cell.bg), Some(selected));
        }
    }

    #[test]
    fn secondary_cursors_are_drawn_as_inverted_cells() {
        let mut snapshot = snapshot_of(8, 2, &["abcdef"]);
        if let Some(editor) = snapshot.editor.as_mut() {
            editor.secondary_cursors = vec![CellPoint { column: 2, row: 0 }];
        }
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        render(&snapshot, &palette(), &mut buffer);
        assert!(
            buffer
                .cell((2, 0))
                .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn the_command_line_sits_directly_above_the_status_line() {
        let mut snapshot = snapshot_of(20, 4, &["x"]);
        snapshot.command_line = Some(CommandLineView {
            prefix: ':',
            query: "wq".to_owned(),
            cursor: 2,
            completions: vec!["save and quit".to_owned()],
            selected_completion: Some(0),
            message: None,
        });
        let grid = grid(&snapshot);
        assert_eq!(grid[2], ":wq  → save and quit");
        assert!(grid[3].starts_with("[No Name]"));
    }

    #[test]
    fn notifications_stack_above_the_command_line() {
        let mut snapshot = snapshot_of(20, 5, &["x"]);
        snapshot.command_line = Some(CommandLineView {
            prefix: '/',
            query: "needle".to_owned(),
            cursor: 6,
            completions: Vec::new(),
            selected_completion: None,
            message: Some("2/9".to_owned()),
        });
        snapshot.notifications = vec!["unable to save".to_owned()];
        let grid = grid(&snapshot);
        assert_eq!(grid[2].trim_end(), "unable to save");
        assert_eq!(grid[3].trim_end(), "/needle  2/9");
        assert!(grid[4].starts_with("[No Name]"));
    }

    #[test]
    fn an_empty_snapshot_paints_nothing_but_the_status_line() {
        let snapshot = ViewSnapshot {
            columns: 12,
            rows: 2,
            ..Default::default()
        };
        let grid = grid(&snapshot);
        assert_eq!(grid[0], "            ");
        assert_eq!(grid[1], "[No Name]   ");
    }

    #[test]
    fn reserved_rows_grow_with_the_lines_ted_owns() {
        assert_eq!(reserved_rows(false, false, 0), 1);
        assert_eq!(reserved_rows(true, false, 0), 2);
        assert_eq!(reserved_rows(true, false, 3), 5);
        // A prompt is two rows: the question and its answers.
        assert_eq!(reserved_rows(false, true, 0), 3);
        assert_eq!(reserved_rows(true, true, 3), 7);
    }

    #[test]
    fn a_prompt_puts_its_answers_directly_above_the_status_line() {
        let mut snapshot = snapshot_of(40, 5, &["x"]);
        snapshot.prompt = Some(PromptView {
            message: "a.rs has changes. Save them?".to_owned(),
            detail: None,
            answers: vec![
                "Save".to_owned(),
                "Don't Save".to_owned(),
                "Cancel".to_owned(),
            ],
        });
        let grid = grid(&snapshot);
        assert_eq!(grid[2].trim_end(), "a.rs has changes. Save them?");
        assert_eq!(grid[3].trim_end(), "[1] Save  [2] Don't Save  [3] Cancel");
        assert!(grid[4].starts_with("[No Name]"));
    }

    #[test]
    fn a_prompt_sits_above_the_command_line_when_both_are_up() {
        let mut snapshot = snapshot_of(30, 6, &["x"]);
        snapshot.command_line = Some(CommandLineView {
            prefix: ':',
            query: "q".to_owned(),
            cursor: 1,
            completions: Vec::new(),
            selected_completion: None,
            message: None,
        });
        snapshot.prompt = Some(PromptView {
            message: "Overwrite?".to_owned(),
            detail: Some("changed on disk".to_owned()),
            answers: vec!["Overwrite".to_owned(), "Cancel".to_owned()],
        });
        let grid = grid(&snapshot);
        assert_eq!(grid[2].trim_end(), "Overwrite? — changed on disk");
        assert_eq!(grid[3].trim_end(), "[1] Overwrite  [2] Cancel");
        assert_eq!(grid[4].trim_end(), ":q");
        assert!(grid[5].starts_with("[No Name]"));
    }
}
