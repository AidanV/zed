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
    CellPoint, CellRect, CommandLineView, CreaseState, CursorShape, EditorView, HintView,
    HoverView, MatchedText, MenuRow, MenuView, OverlayPlacement, OverlayView, PaneView, PromptView,
    RowView, SpanStyle, StatusView, StyledText, TabStripView, ViewSnapshot, tab_cells,
};

/// A prompt is always exactly this tall — the question on one row, the numbered
/// answers on the next — so the reserved-row count does not depend on how long
/// the question is. Both rows are clipped at the right edge like every other
/// line `ted` paints.
const PROMPT_ROWS: usize = 2;

/// How much of the grid a list may cover. A finder that hides the file it is
/// about to open is a worse finder, so past this the list scrolls (SPEC §24.1).
const OVERLAY_ROWS_PER_GRID: u16 = 2;

/// The switcher is sized to its content, within these. Narrower than the lower
/// bound it cannot show a name and a directory; wider than the grid allows it
/// stops being the small box SPEC §24.6 asked for.
const SWITCHER_MIN_WIDTH: u16 = 28;
const SWITCHER_MARGIN: u16 = 4;

/// The rows `ted` paints itself: the status line always, then the `:` / `/` line
/// when one is open, then a prompt when one is unanswered, then one row per
/// notification. Every one of them is at the *bottom* of the grid, and the GPUI
/// window is sized to the grid minus exactly this many rows, so the rects Zed
/// reports can never overlap them (SPEC §10.2).
///
/// The tab strip is deliberately absent from M4 on: it is a row inside each
/// pane, which Zed's own layout leaves for it, rather than a row withheld from
/// the window (SPEC §25.2). An open overlay is absent for a different reason —
/// it floats over the editor's cells and costs no rows at all, so a box whose
/// height follows a query never resizes the window (SPEC §24.1).
pub fn reserved_rows(command_line: bool, prompt: bool, notifications: usize) -> u16 {
    let reserved =
        1 + usize::from(command_line) + usize::from(prompt) * PROMPT_ROWS + notifications;
    u16::try_from(reserved).unwrap_or(u16::MAX)
}

pub fn render(snapshot: &ViewSnapshot, palette: &Palette, buffer: &mut Buffer) {
    if let Some(background) = snapshot
        .background
        .and_then(|color| palette.surface_background(color))
    {
        fill(
            clamp(snapshot.window, buffer.area),
            Style::default().bg(background),
            buffer,
        );
    }
    for pane in &snapshot.panes {
        render_pane(pane, palette, buffer);
    }
    // Over the panes, because the dock's rows are rows the center group no
    // longer has (SPEC §25.5).
    if let Some(terminal) = &snapshot.terminal {
        crate::terminal::render(terminal, palette, buffer);
    }
    // Over the editor, and under the overlay: a list the user opened is in front
    // of a panel they left open.
    if let Some(hover) = &snapshot.hover {
        render_hover(hover, palette, buffer);
    }
    if let Some(menu) = &snapshot.menu {
        render_menu(menu, palette, buffer);
    }
    if let Some(overlay) = &snapshot.overlay {
        render_overlay(overlay, snapshot, palette, buffer);
    }

    let Some(status_row) = snapshot.rows.checked_sub(1) else {
        return;
    };
    render_status(
        &snapshot.status,
        snapshot.columns,
        status_row,
        palette,
        buffer,
    );

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

/// One pane: its own ground, whatever it holds, its strip, and the rule down its
/// edge (SPEC §25.1).
///
/// The ground is painted first and across the whole pane rather than only under
/// the editor, because a pane is more than its editor — the row Zed left for the
/// strip and the rows a deployed search bar takes are the pane's too, and a pane
/// that painted only its text rect would leave the terminal's own background
/// showing through them.
fn render_pane(pane: &PaneView, palette: &Palette, buffer: &mut Buffer) {
    let area = clamp(pane.rect, buffer.area);
    if let Some(background) = pane
        .background
        .and_then(|color| palette.surface_background(color))
    {
        fill(area, Style::default().bg(background), buffer);
    }

    if let Some(editor) = &pane.editor {
        render_editor(editor, palette, buffer);
    }
    // Mutually exclusive with the editor: this is what is painted when the pane
    // has no item to paint (SPEC §24.7).
    if let Some(hint) = &pane.hint {
        render_hint(hint, palette, buffer);
    }
    if let Some(tabs) = &pane.tabs {
        render_tabs(tabs, palette, buffer);
    }

    if pane.divider && area.width > 0 {
        let column = area.x + area.width - 1;
        let style = match pane.divider_color {
            Some(color) => Style::default().fg(palette.color(color)),
            None => Style::default(),
        };
        for row in area.y..area.y + area.height {
            if let Some(cell) = buffer.cell_mut((column, row)) {
                cell.set_symbol("│");
                cell.set_style(style);
            }
        }
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

        render_gutter(
            row,
            gutter_area,
            offset,
            editor.fold_gutter_cells,
            palette,
            buffer,
        );
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
    fold_gutter_cells: u16,
    palette: &Palette,
    buffer: &mut Buffer,
) {
    if area.width == 0 {
        return;
    }
    let y = area.y + row_offset;
    let background = row.background.map(|color| palette.color(color));
    if let Some(background) = background {
        // The diff hunk's (or block's) tint across the whole gutter row (SPEC
        // §11 step 1), painted before anything else in this row so every glyph
        // below can carry the same background explicitly and survive `write`'s
        // full-cell reset.
        fill(
            Rect::new(area.x, y, area.width, 1),
            Style::default().bg(background),
            buffer,
        );
    }

    let mut style = terminal_style(&row.gutter.style, palette);
    if let Some(background) = background {
        style = style.bg(background);
    }

    if let Some(diff) = row.gutter.diff
        && let Some(cell) = buffer.cell_mut((area.x, y))
    {
        let mut marker_style = style;
        if let Some(foreground) = row.gutter.diff_foreground {
            marker_style = marker_style.fg(palette.color(foreground));
        }
        // The marker's own cell carries the staged/unstaged background, which
        // is why it is set here rather than with the row's tint above: it is
        // the one cell in the row that says more than "this row changed".
        if let Some(background) = row.gutter.diff_background {
            marker_style = marker_style.bg(palette.color(background));
        }
        cell.set_symbol(&diff.symbol().to_string());
        cell.set_style(marker_style);
    }

    // The rightmost gutter column, which line numbers always leave blank
    // (below) — reserved for the fold chevron whenever Zed's own reported
    // gutter width left room for one (SPEC §11 step 1).
    if fold_gutter_cells > 0
        && area.width > 1
        && let Some(crease) = row.gutter.crease
        && let Some(cell) = buffer.cell_mut((area.x + area.width - 1, y))
    {
        cell.set_symbol(match crease {
            CreaseState::Folded => "▸",
            CreaseState::Foldable => "▾",
        });
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
    if let Some(background) = row.background {
        // Painted first so every glyph cell below keeps it: `Cell::set_style`
        // merges rather than replaces (SPEC §11 step 1, and the same reasoning
        // as `write_cell`'s doc comment), and a span that names no background
        // of its own — ordinary syntax-highlighted text — leaves this in place.
        fill(
            Rect::new(area.x, y, area.width, 1),
            Style::default().bg(palette.color(background)),
            buffer,
        );
    }
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

/// The pane's items along its own top row, shaped like Zed's: the active tab on
/// the editor's own background, the inactive ones on a darker ground, separated
/// the way Zed separates them (SPEC §24.7).
///
/// The strip is painted in the row Zed's layout left at the top of the pane, and
/// only within that pane's columns (SPEC §25.2) — which is what makes two panes
/// side by side carry two strips rather than fighting over one.
fn render_tabs(tabs: &TabStripView, palette: &Palette, buffer: &mut Buffer) {
    let area = clamp(tabs.rect, buffer.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let ground = tabs
        .background
        .map(|color| Style::default().bg(palette.color(color)))
        .unwrap_or_default();
    fill(area, ground, buffer);

    // An unfocused pane's active tab is painted on the inactive ground, so the
    // strips agree with the cursor about which pane is live (SPEC §25.3).
    let active = if tabs.focused {
        Style::default()
            .fg(color_or_default(tabs.active_foreground, palette))
            .bg(color_or_default(tabs.active_background, palette))
    } else {
        ground.fg(color_or_default(tabs.active_foreground, palette))
    };
    let inactive = ground.fg(color_or_default(tabs.foreground, palette));
    let separator = ground.fg(color_or_default(tabs.separator, palette));

    let right = area.x + area.width;
    let mut x = area.x;
    for (index, tab) in tabs.tabs.iter().enumerate().skip(tabs.first) {
        if index > tabs.first {
            if x >= right {
                return;
            }
            write("│", Rect::new(x, area.y, 1, 1), separator, buffer);
            x += 1;
        }

        let width = tab_cells(tab).min(right.saturating_sub(x));
        if width == 0 {
            return;
        }
        let cells = Rect::new(x, area.y, width, 1);
        let style = if index == tabs.active {
            active
        } else {
            inactive
        };
        fill(cells, style, buffer);
        // `•` after the label is unsaved work, matching the switcher's rows.
        let label = if tab.modified {
            format!(" {} •", tab.label)
        } else {
            format!(" {}", tab.label)
        };
        write(&label, cells, style, buffer);
        x += width;
    }
}

/// The railed panel: a tinted block with a coloured bar down its left edge and
/// no border, so nothing needs an ASCII twin and four more cells go to the text
/// (SPEC §24.8).
fn render_hover(hover: &HoverView, palette: &Palette, buffer: &mut Buffer) {
    let area = clamp(hover.rect, buffer.area);
    let ground = Style::default()
        .fg(color_or_default(hover.foreground, palette))
        .bg(color_or_default(hover.background, palette));
    fill(area, ground, buffer);

    let mut y = area.y;
    for block in &hover.blocks {
        // Only the rail takes the block's colour; a block that named none keeps
        // the panel's own text colour rather than falling back to the
        // terminal's.
        let rail = match block.rail {
            Some(color) => ground.fg(palette.color(color)),
            None => ground,
        };
        for line in &block.lines {
            if y >= area.y + area.height {
                return;
            }
            write("▌", Rect::new(area.x, y, 1, 1), rail, buffer);
            if area.width > 2 {
                write_styled(
                    line,
                    Rect::new(area.x + 2, y, area.width - 2, 1),
                    ground,
                    palette,
                    buffer,
                );
            }
            y += 1;
        }
    }
}

/// The completions box, and the code-action menu in the same box (SPEC §24.9).
///
/// Three columns for a completion — label, kind word, signature — and one for an
/// action, whose leading glyph is already part of its label. No documentation
/// row: that is what `shift-k` is for, and leaving it out is what keeps the
/// box's height a function of the entry count alone.
fn render_menu(menu: &MenuView, palette: &Palette, buffer: &mut Buffer) {
    let area = clamp(menu.rect, buffer.area);
    if area.width < 3 || area.height < 3 {
        return;
    }

    let ground = Style::default()
        .fg(color_or_default(menu.foreground, palette))
        .bg(color_or_default(menu.background, palette));
    fill(area, ground, buffer);
    render_border(
        area,
        None,
        ground.fg(color_or_default(menu.border, palette)),
        buffer,
    );

    let content = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1);
    let selection = menu
        .selection_background
        .map(|color| ground.bg(palette.color(color)));

    for offset in 0..usize::from(area.height.saturating_sub(2)) {
        let Some(row) = menu.rows.get(menu.first + offset) else {
            break;
        };
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        let y = area.y + 1 + offset;

        let selected = menu.selected == Some(menu.first + usize::from(offset));
        let style = match (selected, selection) {
            (true, Some(selection)) => selection,
            (true, None) => ground.add_modifier(Modifier::REVERSED),
            (false, _) => ground,
        };
        fill(Rect::new(content.x, y, content.width, 1), style, buffer);

        match row {
            MenuRow::Divider => render_rule(area, y, ground, buffer),
            MenuRow::Header(label) => {
                write(
                    label,
                    Rect::new(content.x, y, content.width, 1),
                    style.add_modifier(Modifier::DIM),
                    buffer,
                );
            }
            MenuRow::Entry {
                label,
                matched,
                kind,
                signature,
            } => {
                let label_width = menu.kind_column.saturating_sub(1).min(content.width);
                write_styled(
                    label,
                    Rect::new(content.x, y, label_width, 1),
                    style,
                    palette,
                    buffer,
                );
                embolden(
                    matched,
                    &label.text,
                    Rect::new(content.x, y, label_width, 1),
                    buffer,
                );
                if let Some(kind) = kind
                    && menu.kind_column < content.width
                {
                    write(
                        kind,
                        Rect::new(
                            content.x + menu.kind_column,
                            y,
                            content.width - menu.kind_column,
                            1,
                        ),
                        style.add_modifier(Modifier::DIM),
                        buffer,
                    );
                }
                if !signature.is_empty() && menu.signature_column < content.width {
                    write_styled(
                        signature,
                        Rect::new(
                            content.x + menu.signature_column,
                            y,
                            content.width - menu.signature_column,
                            1,
                        ),
                        style,
                        palette,
                        buffer,
                    );
                }
            }
        }
    }
}

/// The screen an empty pane sits on (SPEC §24.7): the wordmark, then the ways
/// out of it, centred, keys in one column and what they do in the next.
fn render_hint(hint: &HintView, palette: &Palette, buffer: &mut Buffer) {
    let area = clamp(hint.rect, buffer.area);
    let columns = area.width;
    let rows = area.height;
    let ground = Style::default()
        .fg(color_or_default(hint.foreground, palette))
        .bg(color_or_default(hint.background, palette));
    // The pane's whole rect, because there is no editor behind this to have
    // filled it in already — and the pane's rather than the grid's, since one
    // pane of a split can be empty while the other holds a file (SPEC §25.1).
    fill(area, ground, buffer);

    let widest = hint
        .rows
        .iter()
        .map(|row| {
            hint.key_cells + 2 + text_cells(&row.description).min(u32::from(u16::MAX)) as u16
        })
        .max()
        .unwrap_or(0);
    let height = hint.rows.len().min(usize::from(u16::MAX)) as u16;
    if widest == 0 || widest > columns || height >= rows {
        return;
    }

    let logo_cells = hint
        .logo
        .iter()
        .map(|line| text_cells(line).min(u32::from(u16::MAX)) as u16)
        .max()
        .unwrap_or(0);
    let logo_height = hint.logo.len().min(usize::from(u16::MAX)) as u16;
    // The mark is the first thing to go when the grid cannot hold both: the
    // hints are what the screen is for, and a wordmark clipped to fit says less
    // than no wordmark at all. The extra row is the gap under it.
    let logo_rows = if logo_cells > 0 && logo_cells <= columns && height + logo_height + 1 < rows {
        logo_height + 1
    } else {
        0
    };

    let accent = ground.fg(color_or_default(hint.accent, palette));
    let x = area.x + (columns - widest) / 2;
    let top = area.y + (rows.saturating_sub(height + logo_rows)) / 2;
    if logo_rows > 0 {
        // Centred on the pane rather than over the block below it, because the
        // block's own width is an accident of the longest description.
        let logo_x = area.x + (columns - logo_cells) / 2;
        for (offset, line) in hint.logo.iter().enumerate() {
            let Ok(offset) = u16::try_from(offset) else {
                break;
            };
            write(
                line,
                Rect::new(logo_x, top + offset, area.x + columns - logo_x, 1),
                accent,
                buffer,
            );
        }
    }

    let top = top + logo_rows;
    for (offset, row) in hint.rows.iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        let y = top + offset;
        write_right(&row.key, x, hint.key_cells, y, accent, buffer);
        write(
            &row.description,
            Rect::new(
                x + hint.key_cells + 2,
                y,
                (area.x + columns).saturating_sub(x + hint.key_cells + 2),
                1,
            ),
            ground,
            buffer,
        );
    }
}

/// Writes styled text, then paints each span over the cells its byte range
/// covers.
///
/// Two passes rather than one run at a time, because `Cell::set_style` merges:
/// a span that names only a colour keeps the boldness `base` gave the row, and a
/// span that names only weight keeps its colour.
fn write_styled(
    text: &StyledText,
    area: Rect,
    base: Style,
    palette: &Palette,
    buffer: &mut Buffer,
) {
    write(&text.text, area, base, buffer);
    if text.spans.is_empty() {
        return;
    }

    let table = crate::snapshot::byte_to_cell_table(&text.text);
    for span in &text.spans {
        let Some(&start) = table.get(span.range.start) else {
            continue;
        };
        let end = table
            .get(span.range.end)
            .copied()
            .unwrap_or_else(|| table.last().copied().unwrap_or(start));
        let style = terminal_style(&span.style, palette);
        for column in start..end.min(area.width) {
            if let Some(cell) = buffer.cell_mut((area.x + column, area.y)) {
                cell.set_style(style);
            }
        }
    }
}

/// Bolds the cells a query matched, over whatever colour is already on them:
/// colour says what kind of thing an entry is, weight says why it matched
/// (SPEC §24.9).
fn embolden(matched: &[usize], text: &str, area: Rect, buffer: &mut Buffer) {
    if matched.is_empty() {
        return;
    }
    let table = crate::snapshot::byte_to_cell_table(text);
    for byte in matched {
        let Some(&column) = table.get(*byte) else {
            continue;
        };
        if column >= area.width {
            continue;
        }
        if let Some(cell) = buffer.cell_mut((area.x + column, area.y)) {
            cell.modifier.insert(Modifier::BOLD);
        }
    }
}

/// Where a list sits and how much of it is on screen.
///
/// Shared with [`cursor`], which has to put the terminal's cursor in the query
/// field: two answers to "where is the query row" would be one answer too many.
struct OverlayLayout {
    rect: Rect,
    query_row: Option<u16>,
    list_row: u16,
    /// The first row painted, scrolled far enough that the selection is on
    /// screen (SPEC §24.1).
    first: usize,
    visible: usize,
}

fn layout_overlay(overlay: &OverlayView, columns: u16, rows: u16) -> Option<OverlayLayout> {
    if columns < SWITCHER_MIN_WIDTH || rows < 4 {
        return None;
    }
    let has_query = overlay.query.is_some();
    let top = match overlay.placement {
        OverlayPlacement::Grid => 0,
        OverlayPlacement::TopCentre => 1,
    };
    // Borders, the query row, and the rule under it — which is only drawn when
    // there is a list under it to separate.
    let chrome = 2 + u16::from(has_query) + u16::from(has_query && !overlay.rows.is_empty());
    let room = rows
        .saturating_sub(top)
        .saturating_sub(chrome)
        .min(rows / OVERLAY_ROWS_PER_GRID);
    let visible = usize::from(room).min(overlay.rows.len());

    let width = match overlay.placement {
        OverlayPlacement::Grid => columns,
        OverlayPlacement::TopCentre => switcher_width(overlay, columns),
    };
    let x = (columns.saturating_sub(width)) / 2;
    let height = chrome.saturating_add(visible.min(usize::from(u16::MAX)) as u16);

    let selected = overlay.selected.unwrap_or(0);
    let first = selected
        .saturating_add(1)
        .saturating_sub(visible.max(1))
        .min(overlay.rows.len().saturating_sub(visible));

    Some(OverlayLayout {
        rect: Rect::new(x, top, width, height),
        query_row: has_query.then_some(top + 1),
        list_row: top + chrome - 1,
        first,
        visible,
    })
}

fn switcher_width(overlay: &OverlayView, columns: u16) -> u16 {
    let content = overlay
        .rows
        .iter()
        .map(|row| {
            let detail = row
                .detail
                .as_ref()
                .map(|detail| text_cells(&detail.text) + 2)
                .unwrap_or(0);
            text_cells(&row.label.text) + detail + if row.modified { 2 } else { 0 }
        })
        .max()
        .unwrap_or(0)
        .min(u32::from(u16::MAX)) as u16;
    content
        .saturating_add(4)
        .max(SWITCHER_MIN_WIDTH)
        .min(columns.saturating_sub(SWITCHER_MARGIN))
}

/// The one widget the finder and the switcher share (SPEC §24.1): a bordered box
/// over the editor's cells, growing downward from a fixed top edge to fit its
/// matches, with the selected row tinted and nothing else marking it.
fn render_overlay(
    overlay: &OverlayView,
    snapshot: &ViewSnapshot,
    palette: &Palette,
    buffer: &mut Buffer,
) {
    let Some(layout) = layout_overlay(overlay, snapshot.columns, snapshot.rows) else {
        return;
    };
    let area = clamp(
        CellRect::new(
            layout.rect.x,
            layout.rect.y,
            layout.rect.width,
            layout.rect.height,
        ),
        buffer.area,
    );
    if area.width < 4 || area.height < 2 {
        return;
    }

    // The theme's text colour always, and its background only when `ted` paints
    // one at all: a transparent session still lets the terminal's background
    // through the box, but the box's own text is the theme's rather than
    // whatever the code underneath happened to be coloured (SPEC §24.1).
    let editor = snapshot.editor();
    let ground = Style::default().fg(color_or_default(
        editor.and_then(|editor| editor.foreground),
        palette,
    ));
    let ground = match editor
        .and_then(|editor| editor.background)
        .and_then(|color| palette.surface_background(color))
    {
        Some(background) => ground.bg(background),
        None => ground,
    };
    fill(area, ground, buffer);
    render_border(area, overlay.title.as_deref(), ground, buffer);

    let content = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(4),
        area.height,
    );
    if let (Some(query_row), Some(query)) = (layout.query_row, overlay.query.as_ref()) {
        write(
            &format!("> {}", query.text),
            Rect::new(content.x, query_row, content.width, 1),
            ground,
            buffer,
        );
        // Only when there is a list under it to separate: a rule over nothing
        // would take the row the query is being typed on.
        if layout.visible > 0 {
            render_rule(area, layout.list_row.saturating_sub(1), ground, buffer);
        }
    }
    if let Some(footer) = &overlay.footer {
        let row = layout.query_row.unwrap_or(area.y + area.height - 1);
        write_right(footer, content.x, content.width, row, ground, buffer);
    }

    // The theme's own selection background — the same colour a selection in the
    // buffer uses — and nothing else: no bar, no caret, so every row starts at
    // the same column (SPEC §24.1).
    let selection = snapshot
        .editor()
        .and_then(|editor| editor.selection_background)
        .map(|color| ground.bg(palette.color(color)));

    let detail_column = detail_column(overlay, content.width);
    for offset in 0..layout.visible {
        let Some(row) = overlay.rows.get(layout.first + offset) else {
            break;
        };
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        let y = layout.list_row + offset;
        if y + 1 >= area.y + area.height {
            break;
        }

        let selected = overlay.selected == Some(layout.first + usize::from(offset));
        let style = match (selected, selection) {
            (true, Some(selection)) => selection,
            (true, None) => ground.add_modifier(Modifier::REVERSED),
            (false, _) => ground,
        };
        fill(
            Rect::new(area.x + 1, y, area.width.saturating_sub(2), 1),
            style,
            buffer,
        );

        let label = if row.modified {
            MatchedText {
                text: format!("{} •", row.label.text),
                matched: row.label.matched.clone(),
            }
        } else {
            row.label.clone()
        };
        write_matched(
            &label,
            Rect::new(content.x, y, detail_column.min(content.width), 1),
            style,
            buffer,
        );
        if let Some(detail) = &row.detail
            && detail_column < content.width
        {
            write_matched(
                detail,
                Rect::new(
                    content.x + detail_column,
                    y,
                    content.width - detail_column,
                    1,
                ),
                // Dimmed, because the column is there to tell two files of the
                // same name apart rather than to be read.
                style.add_modifier(Modifier::DIM),
                buffer,
            );
        }
    }
}

/// Where the directory column starts, from the widest name on screen, so it
/// starts in the same place on every row (SPEC §24.4).
fn detail_column(overlay: &OverlayView, width: u16) -> u16 {
    let widest = overlay
        .rows
        .iter()
        .map(|row| text_cells(&row.label.text) + if row.modified { 2 } else { 0 })
        .max()
        .unwrap_or(0)
        .min(u32::from(u16::MAX)) as u16;
    widest.saturating_add(2).min(width / 2).max(1)
}

fn render_border(area: Rect, title: Option<&str>, style: Style, buffer: &mut Buffer) {
    let bottom = area.y + area.height - 1;
    let inner = area.width.saturating_sub(2);
    write(
        &format!("╭{}╮", "─".repeat(usize::from(inner))),
        area,
        style,
        buffer,
    );
    write(
        &format!("╰{}╯", "─".repeat(usize::from(inner))),
        Rect::new(area.x, bottom, area.width, 1),
        style,
        buffer,
    );
    for y in area.y + 1..bottom {
        write("│", Rect::new(area.x, y, 1, 1), style, buffer);
        write(
            "│",
            Rect::new(area.x + area.width - 1, y, 1, 1),
            style,
            buffer,
        );
    }
    if let Some(title) = title
        && inner > 4
    {
        write(
            &format!("─ {title} "),
            Rect::new(area.x + 1, area.y, inner, 1),
            style,
            buffer,
        );
    }
}

fn render_rule(area: Rect, row: u16, style: Style, buffer: &mut Buffer) {
    let inner = area.width.saturating_sub(2);
    write(
        &format!("├{}┤", "─".repeat(usize::from(inner))),
        Rect::new(area.x, row, area.width, 1),
        style,
        buffer,
    );
}

/// Where the terminal's own cursor belongs, which is not always where the
/// editor's is: while one of `ted`'s own surfaces owns the keyboard, the cursor
/// belongs in the field being typed in, as a bar, whatever vim's mode says
/// (SPEC §24.1). A surface with nothing to type in hides it rather than leaving
/// it under a box.
pub fn cursor(snapshot: &ViewSnapshot) -> (Option<CellPoint>, CursorShape) {
    if let Some(overlay) = &snapshot.overlay {
        let Some(layout) = layout_overlay(overlay, snapshot.columns, snapshot.rows) else {
            return (None, snapshot.cursor_shape);
        };
        let Some((row, query)) = layout.query_row.zip(overlay.query.as_ref()) else {
            return (None, CursorShape::Bar);
        };
        let typed = query.text.get(..query.cursor).unwrap_or(&query.text);
        let column = layout.rect.x + 4 + text_cells(typed).min(u32::from(u16::MAX)) as u16;
        return (
            Some(CellPoint {
                column: column.min(snapshot.columns.saturating_sub(1)),
                row,
            }),
            CursorShape::Bar,
        );
    }

    if let Some(command_line) = &snapshot.command_line
        && let Some(row) = command_line_row(snapshot)
    {
        let typed = command_line
            .query
            .get(..command_line.cursor)
            .unwrap_or(&command_line.query);
        let column = 1 + text_cells(typed).min(u32::from(u16::MAX)) as u16;
        return (
            Some(CellPoint {
                column: column.min(snapshot.columns.saturating_sub(1)),
                row,
            }),
            CursorShape::Bar,
        );
    }

    (snapshot.cursor, snapshot.cursor_shape)
}

/// Directly above the status line, which is the last row of the grid.
fn command_line_row(snapshot: &ViewSnapshot) -> Option<u16> {
    snapshot.rows.checked_sub(2)
}

/// The sparse bar (SPEC §21/M3.5): the mode and the diagnostic counts at the
/// left, the cursor's position at the right, and whatever happens to be true in
/// the middle — so the row is mostly empty most of the time.
fn render_status(
    status: &StatusView,
    columns: u16,
    row: u16,
    palette: &Palette,
    buffer: &mut Buffer,
) {
    if columns == 0 {
        return;
    }

    let area = Rect::new(0, row, columns, 1);
    let ground = match (status.background, status.foreground) {
        (Some(background), Some(foreground)) => Style::default()
            .bg(palette.color(background))
            .fg(palette.color(foreground)),
        // A frame taken before there is a theme to read — "terminal too small"
        // is the only one — has no colours to be legible in, and inverting
        // whatever is behind the row is the one thing that always is.
        _ => Style::default().add_modifier(Modifier::REVERSED),
    };
    fill(area, ground, buffer);

    // The whole row, and the only thing on the bar that has to elide when the
    // grid is narrow (SPEC §24.5).
    if let Some(takeover) = &status.takeover {
        write(&elide_middle(takeover, columns), area, ground, buffer);
        return;
    }

    let mut left = 0u16;
    if let Some(mode) = &status.mode {
        left = write_run(
            mode,
            left,
            area,
            ground.add_modifier(Modifier::BOLD),
            buffer,
        );
        left = left.saturating_add(1);
    }
    // Severity by colour and a neutral glyph, the same decision the buffer's
    // underlines take (SPEC §24.8) — so the two never disagree about what an
    // error looks like, and a project with nothing wrong with it says nothing.
    if let Some(counts) = status.diagnostics.filter(|counts| !counts.is_empty()) {
        for (count, color) in [
            (counts.errors, counts.error_color),
            (counts.warnings, counts.warning_color),
        ] {
            if count == 0 {
                continue;
            }
            let style = ground.fg(color_or_default(color, palette));
            left = write_run(&format!("● {count}"), left, area, style, buffer);
            left = left.saturating_add(1);
        }
    }

    let position = status
        .position
        .map(|(line, column)| format!("{line}:{column}"));
    let right_cells = position
        .as_deref()
        .map(|text| text_cells(text).min(u32::from(columns)) as u16)
        .unwrap_or(0);
    if let Some(position) = &position {
        write_right(position, 0, columns, row, ground, buffer);
    }

    // Everything that is true only sometimes, and leaves the row when it stops
    // being true.
    let mut transient = Vec::new();
    if let Some(pending) = &status.pending_keys {
        transient.push(pending.clone());
    }
    if status.rewrapping {
        transient.push("wrapping…".to_owned());
    }
    if status.empty {
        transient.push("no buffer".to_owned());
    }
    if transient.is_empty() {
        return;
    }

    let middle = transient.join("  ");
    let cells = text_cells(&middle).min(u32::from(columns)) as u16;
    let gap = columns.saturating_sub(right_cells).saturating_sub(left);
    // Dropped rather than truncated when it does not fit: half a pending
    // keystroke is worse than none, and everything here is on the row only
    // because it is momentarily true.
    if cells == 0 || cells + 2 > gap {
        return;
    }
    let x = left + (gap - cells) / 2;
    write(
        &middle,
        Rect::new(x, row, columns.saturating_sub(x), 1),
        ground,
        buffer,
    );
}

/// Keeps the head and the tail of a string and drops its middle.
///
/// Written for vim's `ctrl-g` location string, which is a path followed by a
/// line count and a percentage: the numbers are short and sit at the end, so
/// what a narrow grid eats is the middle of the path — the part a reader can
/// most easily do without, since the file's name is at one end of it and its
/// worktree at the other.
fn elide_middle(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if text_cells(text) as usize <= width || width == 0 {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }

    const TAIL_CELLS: usize = 18;
    let tail_cells = (width / 2).min(TAIL_CELLS);
    let head_cells = width - 1 - tail_cells;

    let mut head = String::new();
    let mut cells = 0usize;
    for cluster in text.graphemes(true) {
        let next = cells + cluster_cells(cluster) as usize;
        if next > head_cells {
            break;
        }
        head.push_str(cluster);
        cells = next;
    }

    let mut tail = String::new();
    let mut cells = 0usize;
    for cluster in text.graphemes(true).rev() {
        let next = cells + cluster_cells(cluster) as usize;
        if next > tail_cells {
            break;
        }
        tail.insert_str(0, cluster);
        cells = next;
    }

    format!("{head}…{tail}")
}

/// Writes at `x` cells into a one-row area and reports where the next run may
/// start.
fn write_run(text: &str, x: u16, area: Rect, style: Style, buffer: &mut Buffer) -> u16 {
    let room = area.width.saturating_sub(x);
    let cells = text_cells(text).min(u32::from(room)) as u16;
    if cells == 0 {
        return x;
    }
    write(text, Rect::new(area.x + x, area.y, cells, 1), style, buffer);
    x + cells
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
    if let Some(message) = &command_line.message {
        line.push_str("  ");
        line.push_str(message);
    }
    write(&line, area, Style::default(), buffer);

    // The rest of the selected command, dimmed after the cursor. A list would
    // have covered the buffer for a half-typed command; one row never does
    // (SPEC §24.3).
    let typed = text_cells(&line).min(u32::from(columns)) as u16;
    if let Some(ghost) = &command_line.ghost
        && typed < columns
    {
        write(
            ghost,
            Rect::new(typed, row, columns - typed, 1),
            Style::default().add_modifier(Modifier::DIM),
            buffer,
        );
    }

    // The first thing that still fits beside what is already on the row: the
    // keybinding when the selected action has one, and the candidate count
    // otherwise (SPEC §24.3).
    let used = typed
        + command_line
            .ghost
            .as_deref()
            .map(|ghost| text_cells(ghost).min(u32::from(columns)) as u16)
            .unwrap_or(0);
    for candidate in &command_line.trailing {
        let width = text_cells(candidate).min(u32::from(columns)) as u16;
        if used + width + 1 > columns {
            continue;
        }
        write(
            candidate,
            Rect::new(columns - width, row, width, 1),
            Style::default().add_modifier(Modifier::DIM),
            buffer,
        );
        break;
    }
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
    if let Some(underline) = style.underline {
        result = result.add_modifier(Modifier::UNDERLINED);
        // `SGR 58`, which is the whole of what the buffer says about severity: a
        // straight coloured underline rather than a curl, which would mean
        // writing a Ratatui `Backend` for a difference the colour already
        // carries (SPEC §24.8).
        if let Some(color) = underline.color {
            result = result.underline_color(palette.color(color));
        }
    }
    if style.strikethrough {
        result = result.add_modifier(Modifier::CROSSED_OUT);
    }
    result
}

fn color_or_default(color: Option<gpui::Hsla>, palette: &Palette) -> ratatui::style::Color {
    color
        .map(|color| palette.color(color))
        .unwrap_or(ratatui::style::Color::Reset)
}

/// Writes text with the bytes a query matched emphasised, which is the only
/// thing a list row does that plain text does not.
fn write_matched(text: &MatchedText, area: Rect, style: Style, buffer: &mut Buffer) {
    write(&text.text, area, style, buffer);
    embolden(&text.matched, &text.text, area, buffer);
}

/// Right-aligns `text` inside a run of cells, clipping it away entirely rather
/// than truncating it when it does not fit.
fn write_right(text: &str, x: u16, width: u16, row: u16, style: Style, buffer: &mut Buffer) {
    let cells = text_cells(text).min(u32::from(width)) as u16;
    if cells == 0 || cells > width {
        return;
    }
    write(
        text,
        Rect::new(x + width - cells, row, cells, 1),
        style,
        buffer,
    );
}

/// Writes plain text into `area`, clipping at its right edge and honouring
/// cell widths the same way [`render_row`] does.
///
/// Every cell is [`reset`](write_cell) first, because `Style` carries only what
/// it names: writing over a cell would otherwise keep the colours and attributes
/// already on it (SPEC §24.1).
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
        write_cell(cluster, (x, area.y), style, buffer);
        for trailing in 1..width {
            let Ok(trailing) = u16::try_from(x as u32 + trailing) else {
                break;
            };
            write_cell("", (trailing, area.y), style, buffer);
        }
        column += width;
    }
}

fn fill(area: Rect, style: Style, buffer: &mut Buffer) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            write_cell(" ", (x, y), style, buffer);
        }
    }
}

/// Puts a grapheme in a cell, replacing everything that was there.
///
/// `Cell::set_style` *merges*: a colour the style leaves unset stays whatever
/// the cell already had, and modifiers are only added and removed by name. So
/// painting a surface over the editor with a partial style — which is every
/// surface, since a transparent one names no background at all — would leave the
/// syntax colour, the boldness and the diagnostic underline of the code
/// underneath on the cells it covered. Resetting first is what makes a style
/// mean the whole appearance of the cell rather than a patch on it.
fn write_cell(symbol: &str, (x, y): (u16, u16), style: Style, buffer: &mut Buffer) {
    if let Some(cell) = buffer.cell_mut((x, y)) {
        cell.reset();
        cell.set_symbol(symbol);
        cell.set_style(style);
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
    use crate::snapshot::{
        CreaseState, DiagnosticCounts, DiffMarker, GutterView, HintRow, HoverBlock, OverlayRow,
        QueryView, RowKind, SelectionSpan, StyledSpan, TabView, Underline,
    };
    use gpui::hsla;

    fn palette() -> Palette {
        Palette::new(ColorDepth::TrueColor, hsla(0.0, 0.0, 0.0, 1.0), false)
    }

    fn set_tabs(snapshot: &mut ViewSnapshot, tabs: TabStripView) {
        if let Some(pane) = snapshot.panes.first_mut() {
            pane.tabs = Some(tabs);
        }
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

    /// One pane filling the grid above the status line, which is what a session
    /// with no split looks like (SPEC §25.1).
    fn snapshot_of(columns: u16, rows: u16, lines: &[&str]) -> ViewSnapshot {
        ViewSnapshot {
            columns,
            rows,
            panes: vec![PaneView {
                rect: CellRect::new(0, 0, columns, rows.saturating_sub(1)),
                active: true,
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
            }],
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
        if let Some(editor) = snapshot.editor_mut() {
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
        assert_eq!(grid[2], "    ");
    }

    #[test]
    fn combining_marks_ride_along_with_their_base() {
        let snapshot = snapshot_of(4, 2, &["e\u{0301}x"]);
        let grid = grid(&snapshot);
        assert_eq!(grid[0], "e\u{0301}x  ");
    }

    /// SPEC §21/M3.5: the mode and the counts at the left, the position at the
    /// right, no path anywhere, and an otherwise empty row.
    #[test]
    fn the_status_line_carries_the_mode_the_counts_and_the_position() {
        let mut snapshot = snapshot_of(30, 2, &["x"]);
        snapshot.status = StatusView {
            mode: Some("NORMAL".to_owned()),
            position: Some((3, 7)),
            diagnostics: Some(DiagnosticCounts {
                errors: 2,
                warnings: 1,
                error_color: Some(hsla(0.0, 0.8, 0.5, 1.0)),
                warning_color: Some(hsla(0.1, 0.8, 0.5, 1.0)),
            }),
            ..Default::default()
        };
        assert_eq!(grid(&snapshot)[1], "NORMAL ● 2 ● 1             3:7");
    }

    #[test]
    fn a_project_with_nothing_wrong_with_it_says_nothing() {
        let mut snapshot = snapshot_of(20, 2, &["x"]);
        snapshot.status = StatusView {
            mode: Some("NORMAL".to_owned()),
            diagnostics: Some(DiagnosticCounts::default()),
            position: Some((1, 1)),
            ..Default::default()
        };
        assert_eq!(grid(&snapshot)[1], "NORMAL           1:1");
    }

    /// The counts are told apart by colour and nothing else, the same decision
    /// the buffer's underlines take (SPEC §24.8).
    #[test]
    fn the_two_counts_are_told_apart_by_colour() {
        let mut snapshot = snapshot_of(20, 2, &["x"]);
        let error = hsla(0.0, 0.8, 0.5, 1.0);
        let warning = hsla(0.1, 0.8, 0.5, 1.0);
        snapshot.status = StatusView {
            diagnostics: Some(DiagnosticCounts {
                errors: 1,
                warnings: 1,
                error_color: Some(error),
                warning_color: Some(warning),
            }),
            ..Default::default()
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        render(&snapshot, &palette(), &mut buffer);
        assert_eq!(
            buffer.cell((0, 1)).map(|cell| cell.fg),
            Some(palette().color(error))
        );
        assert_eq!(
            buffer.cell((4, 1)).map(|cell| cell.fg),
            Some(palette().color(warning))
        );
    }

    /// Everything transient sits in the middle of the row, between the standing
    /// left group and the standing right one (SPEC §21/M3.5).
    #[test]
    fn pending_keys_sit_in_the_middle_of_the_row() {
        let mut snapshot = snapshot_of(20, 2, &["x"]);
        snapshot.status = StatusView {
            pending_keys: Some("d2".to_owned()),
            position: Some((1, 1)),
            ..Default::default()
        };
        assert_eq!(grid(&snapshot)[1], "       d2        1:1");
    }

    /// `ctrl-g` takes the whole row rather than opening a surface or claiming
    /// the notification line (SPEC §24.5).
    #[test]
    fn the_location_string_takes_the_whole_row() {
        let mut snapshot = snapshot_of(30, 2, &["x"]);
        snapshot.status = StatusView {
            mode: Some("NORMAL".to_owned()),
            position: Some((3, 7)),
            takeover: Some("src/a.rs 12 lines --25%--".to_owned()),
            ..Default::default()
        };
        assert_eq!(grid(&snapshot)[1], "src/a.rs 12 lines --25%--     ");
    }

    /// The one thing on the bar that elides, and what it drops first is the
    /// middle of the path (SPEC §23's remaining list).
    #[test]
    fn a_narrow_grid_eats_the_middle_of_the_path_and_keeps_the_numbers() {
        let elided = elide_middle("crates/ted/src/snapshot.rs 2064 lines --50%--", 30);
        assert_eq!(text_cells(&elided), 30);
        assert!(elided.starts_with("crates/te"), "{elided:?}");
        assert!(elided.ends_with("--50%--"), "{elided:?}");
        assert_eq!(
            elide_middle("a.rs 1 lines --0%--", 40),
            "a.rs 1 lines --0%--"
        );
    }

    #[test]
    fn line_numbers_are_right_aligned_one_cell_clear_of_the_text() {
        let mut snapshot = snapshot_of(10, 2, &["fn main"]);
        if let Some(editor) = snapshot.editor_mut() {
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
        if let Some(editor) = snapshot.editor_mut() {
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
        if let Some(editor) = snapshot.editor_mut() {
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
        if let Some(editor) = snapshot.editor_mut() {
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
            query: "w".to_owned(),
            cursor: 1,
            ghost: Some("q".to_owned()),
            ..Default::default()
        });
        let grid = grid(&snapshot);
        assert_eq!(grid[2], ":wq                 ");
        assert_eq!(grid[3].trim_end(), "");
    }

    #[test]
    fn notifications_stack_above_the_command_line() {
        let mut snapshot = snapshot_of(20, 5, &["x"]);
        snapshot.command_line = Some(CommandLineView {
            prefix: '/',
            query: "needle".to_owned(),
            cursor: 6,
            message: Some("2/9".to_owned()),
            ..Default::default()
        });
        snapshot.notifications = vec!["unable to save".to_owned()];
        let grid = grid(&snapshot);
        assert_eq!(grid[2].trim_end(), "unable to save");
        assert_eq!(grid[3].trim_end(), "/needle  2/9");
        assert_eq!(grid[4].trim_end(), "");
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
        // The sparse bar with nothing true on it is an empty row (SPEC §21/M3.5).
        assert_eq!(grid[1], "            ");
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
        assert_eq!(grid[4].trim_end(), "");
    }

    #[test]
    fn a_prompt_sits_above_the_command_line_when_both_are_up() {
        let mut snapshot = snapshot_of(30, 6, &["x"]);
        snapshot.command_line = Some(CommandLineView {
            prefix: ':',
            query: "q".to_owned(),
            cursor: 1,
            ..Default::default()
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
        assert_eq!(grid[5].trim_end(), "");
    }

    fn overlay_row(label: &str, detail: &str) -> OverlayRow {
        OverlayRow {
            label: MatchedText::plain(label),
            detail: (!detail.is_empty()).then(|| MatchedText::plain(detail)),
            modified: false,
        }
    }

    fn finder(query: &str, rows: Vec<OverlayRow>, footer: &str) -> OverlayView {
        OverlayView {
            title: Some("files".to_owned()),
            query: Some(QueryView {
                cursor: query.len(),
                text: query.to_owned(),
            }),
            selected: (!rows.is_empty()).then_some(0),
            rows,
            footer: Some(footer.to_owned()),
            placement: OverlayPlacement::Grid,
        }
    }

    #[test]
    fn the_finder_is_a_bordered_box_over_the_grid_with_two_columns() {
        let mut snapshot = snapshot_of(50, 14, &["fn main() {}"]);
        snapshot.overlay = Some(finder(
            "sna",
            vec![
                overlay_row("snapshot.rs", "crates/ted/src"),
                overlay_row("render.rs", "crates/ted/src"),
            ],
            "2/412",
        ));
        let grid = grid(&snapshot);

        assert!(grid[0].starts_with("╭─ files "), "{:?}", grid[0]);
        assert!(grid[0].ends_with('╮'), "{:?}", grid[0]);
        assert!(grid[1].starts_with("│ > sna"), "{:?}", grid[1]);
        assert!(grid[1].ends_with("2/412 │"), "{:?}", grid[1]);
        assert!(
            grid[2].starts_with('├') && grid[2].ends_with('┤'),
            "{:?}",
            grid[2]
        );
        assert!(
            grid[3].starts_with("│ snapshot.rs") && grid[3].contains("crates/ted/src"),
            "{:?}",
            grid[3]
        );
        assert!(grid[4].contains("render.rs"), "{:?}", grid[4]);
        assert!(
            grid[5].starts_with('╰') && grid[5].ends_with('╯'),
            "{:?}",
            grid[5]
        );
        // The buffer behind it is untouched below the box: the overlay floats
        // over the editor's cells and reserves nothing (SPEC §24.1).
        assert_eq!(grid[6].trim_end(), "");
    }

    /// A cell's style is replaced by whatever is painted over it, never merged
    /// into: a box that named no colours of its own used to come out in the
    /// syntax colouring of the code it covered — the border in the blue of the
    /// `fn` under it, bold where the code was bold (SPEC §24.1).
    #[test]
    fn a_list_paints_its_own_colours_over_the_ones_it_covers() {
        let syntax = hsla(0.6, 0.7, 0.6, 1.0);
        let text = hsla(0.0, 0.0, 0.9, 1.0);
        let mut snapshot = snapshot_of(40, 12, &["fn main() {}"]);
        if let Some(editor) = snapshot.editor_mut() {
            editor.foreground = Some(text);
            editor.rows[0].spans = vec![StyledSpan {
                range: 0..12,
                style: SpanStyle {
                    foreground: Some(syntax),
                    bold: true,
                    underline: Some(Underline {
                        color: Some(syntax),
                    }),
                    ..Default::default()
                },
            }];
        }
        snapshot.overlay = Some(finder(
            "sna",
            vec![overlay_row("snapshot.rs", "crates/ted/src")],
            "1/1",
        ));

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 12));
        render(&snapshot, &palette(), &mut buffer);
        // The top border, over the `n` of `fn`.
        let border = buffer.cell((1, 0)).expect("no cell under the border");
        assert_eq!(border.fg, palette().color(text));
        assert!(!border.modifier.contains(Modifier::BOLD));
        assert!(!border.modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn the_box_grows_to_fit_its_matches_and_paints_no_blank_rows() {
        let mut snapshot = snapshot_of(40, 14, &["x"]);
        snapshot.overlay = Some(finder("s", vec![overlay_row("one.rs", "src")], "1/9"));
        let one = grid(&snapshot);
        assert!(one[3].contains("one.rs"), "{:?}", one[3]);
        assert!(one[4].starts_with('╰'), "{:?}", one[4]);

        snapshot.overlay = Some(finder(
            "s",
            vec![
                overlay_row("one.rs", "src"),
                overlay_row("two.rs", "src"),
                overlay_row("three.rs", "src"),
            ],
            "3/9",
        ));
        let grid = grid(&snapshot);
        assert!(grid[5].contains("three.rs"), "{:?}", grid[5]);
        assert!(grid[6].starts_with('╰'), "{:?}", grid[6]);
    }

    #[test]
    fn a_finder_with_nothing_to_show_says_so_and_opens_no_list() {
        let mut snapshot = snapshot_of(40, 14, &["x"]);
        snapshot.overlay = Some(finder("zzz", Vec::new(), "no matches"));
        let grid = grid(&snapshot);
        assert!(grid[1].ends_with("no matches │"), "{:?}", grid[1]);
        assert!(grid[2].starts_with('╰'), "{:?}", grid[2]);
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_selection_visible() {
        let mut snapshot = snapshot_of(40, 12, &["x"]);
        let rows = (0..20)
            .map(|index| overlay_row(&format!("file{index}.rs"), "src"))
            .collect::<Vec<_>>();
        let mut overlay = finder("f", rows, "20/20");
        overlay.selected = Some(19);
        snapshot.overlay = Some(overlay);

        let grid = grid(&snapshot);
        let visible = grid
            .iter()
            .filter(|row| row.contains("file"))
            .collect::<Vec<_>>();
        assert!(
            visible.len() < 20 && visible.iter().any(|row| row.contains("file19.rs")),
            "the selection scrolled out of view: {visible:?}"
        );
    }

    #[test]
    fn the_switcher_is_a_small_box_at_the_top_with_no_query_row() {
        let mut snapshot = snapshot_of(60, 14, &["fn main() {}"]);
        snapshot.overlay = Some(OverlayView {
            title: Some("buffers".to_owned()),
            query: None,
            rows: vec![
                OverlayRow {
                    label: MatchedText::plain("frame.rs"),
                    detail: Some(MatchedText::plain("crates/ted/src")),
                    modified: true,
                },
                overlay_row("snapshot.rs", "crates/ted/src"),
            ],
            // The second row, because the first is the buffer you are in.
            selected: Some(1),
            footer: None,
            placement: OverlayPlacement::TopCentre,
        });
        let grid = grid(&snapshot);

        // Row 0 is still the editor: the box hangs below it, centred.
        assert!(grid[0].starts_with("fn main"), "{:?}", grid[0]);
        assert!(grid[1].trim_start().starts_with('╭'), "{:?}", grid[1]);
        assert!(grid[2].contains("frame.rs •"), "{:?}", grid[2]);
        assert!(grid[3].contains("snapshot.rs"), "{:?}", grid[3]);
        assert!(grid[4].trim_start().starts_with('╰'), "{:?}", grid[4]);
        let left = grid[1].len() - grid[1].trim_start().len();
        assert!(left > 0, "the switcher is not centred: {:?}", grid[1]);
    }

    #[test]
    fn the_selected_row_is_a_background_tint_and_nothing_else() {
        let mut snapshot = snapshot_of(40, 12, &["x"]);
        let selection = hsla(0.6, 0.5, 0.3, 1.0);
        if let Some(editor) = snapshot.editor_mut() {
            editor.selection_background = Some(selection);
        }
        snapshot.overlay = Some(finder(
            "o",
            vec![overlay_row("one.rs", "src"), overlay_row("two.rs", "src")],
            "2/2",
        ));

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 12));
        render(&snapshot, &palette(), &mut buffer);
        let tint = palette().color(selection);
        assert_eq!(buffer.cell((2, 3)).map(|cell| cell.bg), Some(tint));
        assert_ne!(buffer.cell((2, 4)).map(|cell| cell.bg), Some(tint));

        // No bar and no caret: every row starts at the same column.
        let grid = grid(&snapshot);
        assert!(grid[3].starts_with("│ one.rs"), "{:?}", grid[3]);
        assert!(grid[4].starts_with("│ two.rs"), "{:?}", grid[4]);
    }

    /// SPEC §25.1 and §25.3: each pane is painted at its own rect, and the pane
    /// that has another one beside it carries a rule down its last column.
    #[test]
    fn two_panes_are_painted_at_their_own_rects_with_a_rule_between_them() {
        let pane = |x: u16, width: u16, active: bool, divider: bool, line: &str| PaneView {
            rect: CellRect::new(x, 0, width, 4),
            active,
            divider,
            tabs: Some(TabStripView {
                rect: CellRect::new(x, 0, width, 1),
                focused: active,
                tabs: vec![TabView {
                    label: line.to_owned(),
                    modified: false,
                }],
                ..Default::default()
            }),
            editor: Some(EditorView {
                text_rect: CellRect::new(x, 1, width.saturating_sub(u16::from(divider)), 3),
                rows: vec![RowView::new(0, line.to_owned())],
                ..Default::default()
            }),
            ..Default::default()
        };
        let snapshot = ViewSnapshot {
            columns: 20,
            rows: 5,
            window: CellRect::new(0, 0, 20, 4),
            panes: vec![
                pane(0, 10, false, true, "left"),
                pane(10, 10, true, false, "right"),
            ],
            ..Default::default()
        };

        let grid = grid(&snapshot);
        assert_eq!(grid[0], " left    │ right    ");
        assert_eq!(grid[1], "left     │right     ");
        // The rule runs the pane's whole height, and only the pane that has
        // something on the other side of it has one.
        assert!(
            grid.iter()
                .take(4)
                .all(|row| row.chars().nth(9) == Some('│'))
        );
        assert!(
            grid.iter()
                .take(4)
                .all(|row| row.chars().nth(19) != Some('│')),
            "the rightmost pane ruled itself off from the grid's edge: {grid:?}"
        );
    }

    #[test]
    fn the_tab_strip_takes_the_panes_top_row_and_marks_unsaved_work() {
        let mut snapshot = snapshot_of(40, 6, &["fn main() {}"]);
        // Zed's own layout inset the item by the strip's row, so the editor
        // reported a rect a row further down (SPEC §25.2).
        if let Some(editor) = snapshot.editor_mut() {
            editor.text_rect = CellRect::new(0, 1, 40, 4);
        }
        set_tabs(
            &mut snapshot,
            TabStripView {
                rect: CellRect::new(0, 0, 40, 1),
                focused: true,
                tabs: vec![
                    TabView {
                        label: "snapshot.rs".to_owned(),
                        modified: false,
                    },
                    TabView {
                        label: "frame.rs".to_owned(),
                        modified: true,
                    },
                ],
                active: 1,
                ..Default::default()
            },
        );
        let grid = grid(&snapshot);
        assert_eq!(grid[0].trim_end(), " snapshot.rs │ frame.rs •");
        assert_eq!(grid[1].trim_end(), "fn main() {}");
    }

    #[test]
    fn the_strip_scrolls_rather_than_eliding_its_middle() {
        let mut snapshot = snapshot_of(24, 4, &["x"]);
        set_tabs(
            &mut snapshot,
            TabStripView {
                rect: CellRect::new(0, 0, 24, 1),
                focused: true,
                tabs: (0..5)
                    .map(|index| TabView {
                        label: format!("file{index}.rs"),
                        modified: false,
                    })
                    .collect(),
                active: 4,
                first: 3,
                ..Default::default()
            },
        );
        let grid = grid(&snapshot);
        assert_eq!(grid[0].trim_end(), " file3.rs │ file4.rs");
    }

    #[test]
    fn the_hover_panel_rails_each_block_in_its_own_colour() {
        let mut snapshot = snapshot_of(40, 10, &["let x = f();"]);
        let error = hsla(0.0, 0.8, 0.5, 1.0);
        let text = hsla(0.0, 0.0, 0.9, 1.0);
        snapshot.hover = Some(HoverView {
            rect: CellRect::new(0, 1, 40, 4),
            background: None,
            foreground: Some(text),
            blocks: vec![
                HoverBlock {
                    rail: Some(error),
                    lines: vec![
                        StyledText::plain("error E0061"),
                        StyledText::plain("takes 5 arguments"),
                    ],
                },
                HoverBlock {
                    rail: Some(hsla(0.6, 0.5, 0.5, 1.0)),
                    lines: vec![
                        StyledText::default(),
                        StyledText::plain("fn f(a: u16) -> Row"),
                    ],
                },
            ],
        });
        let grid = grid(&snapshot);
        assert_eq!(grid[1].trim_end(), "▌ error E0061");
        assert_eq!(grid[2].trim_end(), "▌ takes 5 arguments");
        // A blank railed row keeps the diagnostic and the documentation from
        // reading as one message.
        assert_eq!(grid[3].trim_end(), "▌");
        assert_eq!(grid[4].trim_end(), "▌ fn f(a: u16) -> Row");

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        render(&snapshot, &palette(), &mut buffer);
        assert_eq!(
            buffer.cell((0, 1)).map(|cell| cell.fg),
            Some(palette().color(error))
        );
        // Only the rail is the block's colour; the message beside it is the
        // panel's own text, not the rail's and not the buffer's underneath.
        assert_eq!(
            buffer.cell((2, 1)).map(|cell| cell.fg),
            Some(palette().color(text))
        );
    }

    /// SPEC §21/M3.5: markdown reaches the panel as styling rather than as
    /// stripped text, so a rendered line's spans have to survive the projection
    /// and land on the cells they cover.
    #[test]
    fn a_rendered_markdown_line_keeps_its_styling_in_the_panel() {
        let mut snapshot = snapshot_of(40, 6, &["let x = f();"]);
        let code = hsla(0.2, 0.5, 0.5, 1.0);
        snapshot.hover = Some(HoverView {
            rect: CellRect::new(0, 1, 40, 2),
            background: None,
            foreground: Some(hsla(0.0, 0.0, 0.9, 1.0)),
            blocks: vec![HoverBlock {
                rail: None,
                lines: vec![StyledText {
                    text: "takes a Row".to_owned(),
                    spans: vec![StyledSpan {
                        range: 8..11,
                        style: SpanStyle {
                            foreground: Some(code),
                            bold: true,
                            ..Default::default()
                        },
                    }],
                }],
            }],
        });

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 6));
        render(&snapshot, &palette(), &mut buffer);
        // The rail takes a cell and a space, so byte 8 of the line is cell 10.
        assert_eq!(
            buffer.cell((10, 1)).map(|cell| cell.fg),
            Some(palette().color(code))
        );
        assert!(
            buffer
                .cell((10, 1))
                .is_some_and(|cell| cell.modifier.contains(Modifier::BOLD))
        );
        // And the prose either side of it is left alone.
        assert!(
            buffer
                .cell((2, 1))
                .is_some_and(|cell| !cell.modifier.contains(Modifier::BOLD))
        );
    }

    fn completion(label: &str, kind: &str, signature: &str, matched: Vec<usize>) -> MenuRow {
        MenuRow::Entry {
            label: StyledText::plain(label),
            matched,
            kind: Some(kind.to_owned()),
            signature: StyledText::plain(signature),
        }
    }

    /// SPEC §24.9: three columns, lined up down the box, and no documentation
    /// row under the list.
    #[test]
    fn the_completions_box_lays_its_three_columns_out_in_line() {
        let mut snapshot = snapshot_of(40, 10, &["let x = f"]);
        snapshot.menu = Some(MenuView {
            rect: CellRect::new(0, 1, 32, 4),
            rows: vec![
                completion("filter", "fn", "(self) -> Iter", vec![0]),
                completion("first", "fn", "(self) -> Option", vec![0]),
            ],
            selected: Some(0),
            first: 0,
            kind_column: 7,
            signature_column: 13,
            background: None,
            foreground: Some(hsla(0.0, 0.0, 0.9, 1.0)),
            selection_background: None,
            border: None,
        });

        let grid = grid(&snapshot);
        assert_eq!(grid[1].trim_end(), "╭──────────────────────────────╮");
        assert_eq!(grid[2].trim_end(), "│filter fn    (self) -> Iter   │");
        assert_eq!(grid[3].trim_end(), "│first  fn    (self) -> Option │");
        assert_eq!(grid[4].trim_end(), "╰──────────────────────────────╯");
    }

    /// Colour says what kind of thing an entry is, weight says why it matched,
    /// and the two are orthogonal (SPEC §24.9).
    #[test]
    fn the_typed_characters_are_bold_over_whatever_colour_the_label_has() {
        let mut snapshot = snapshot_of(40, 8, &["f"]);
        let syntax = hsla(0.5, 0.5, 0.5, 1.0);
        snapshot.menu = Some(MenuView {
            rect: CellRect::new(0, 1, 20, 3),
            rows: vec![MenuRow::Entry {
                label: StyledText {
                    text: "filter".to_owned(),
                    spans: vec![StyledSpan {
                        range: 0..6,
                        style: SpanStyle {
                            foreground: Some(syntax),
                            ..Default::default()
                        },
                    }],
                },
                matched: vec![0],
                kind: None,
                signature: StyledText::default(),
            }],
            selected: Some(0),
            first: 0,
            kind_column: 7,
            signature_column: 8,
            background: None,
            foreground: Some(hsla(0.0, 0.0, 0.9, 1.0)),
            selection_background: None,
            border: None,
        });

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 8));
        render(&snapshot, &palette(), &mut buffer);
        let matched = buffer.cell((1, 2)).expect("no cell for the matched byte");
        assert_eq!(matched.fg, palette().color(syntax));
        assert!(matched.modifier.contains(Modifier::BOLD));
        // The rest of the label is the same colour and not bold.
        let rest = buffer.cell((2, 2)).expect("no cell after the match");
        assert_eq!(rest.fg, palette().color(syntax));
        assert!(!rest.modifier.contains(Modifier::BOLD));
    }

    /// The screen an empty pane sits on, filling a pane that is the whole grid
    /// above the status line.
    fn hint_snapshot(columns: u16, rows: u16, logo: &'static [&'static str]) -> ViewSnapshot {
        ViewSnapshot {
            columns,
            rows,
            status: StatusView {
                empty: true,
                ..Default::default()
            },
            panes: vec![PaneView {
                rect: CellRect::new(0, 0, columns, rows.saturating_sub(1)),
                active: true,
                hint: Some(hint_view(
                    CellRect::new(0, 0, columns, rows.saturating_sub(1)),
                    logo,
                )),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn hint_view(rect: CellRect, logo: &'static [&'static str]) -> HintView {
        HintView {
            rect,
            logo,
            rows: vec![
                HintRow {
                    key: "ctrl-p".to_owned(),
                    description: "find a file".to_owned(),
                },
                HintRow {
                    key: ":q".to_owned(),
                    description: "quit ted".to_owned(),
                },
            ],
            key_cells: 6,
            background: None,
            foreground: None,
            accent: None,
        }
    }

    /// SPEC §24.7: an empty pane is a state `ted` sits in, and the screen names
    /// the ways out of it rather than leaving a blank grid.
    #[test]
    fn an_empty_pane_names_the_ways_out_of_it() {
        let snapshot = hint_snapshot(30, 8, &[]);

        let grid = grid(&snapshot);
        assert!(
            grid.iter().any(|row| row.contains("ctrl-p  find a file")),
            "{grid:?}"
        );
        // Right-aligned in the key column, so the descriptions line up.
        assert!(
            grid.iter().any(|row| row.contains("    :q  quit ted")),
            "{grid:?}"
        );
        // And the bar says there is nothing open rather than going silent.
        assert!(grid[7].contains("no buffer"), "{:?}", grid[7]);
    }

    /// SPEC §24.7: the wordmark stands over the hints, centred on the grid and
    /// separated from them by a row.
    #[test]
    fn the_wordmark_stands_over_the_hints() {
        let snapshot = hint_snapshot(30, 10, &["▀▀▀", " █ ", " █ "]);

        let grid = grid(&snapshot);
        assert_eq!(grid[1].trim_end(), "             ▀▀▀");
        assert_eq!(grid[2].trim_end(), "              █");
        assert_eq!(grid[3].trim_end(), "              █");
        // A row of its own between the mark and the hints.
        assert_eq!(grid[4].trim_end(), "");
        assert_eq!(grid[5].trim_end(), "     ctrl-p  find a file");
        assert_eq!(grid[6].trim_end(), "         :q  quit ted");
    }

    /// The hints are what the screen is for, so a grid with room for them but
    /// not for the mark keeps them and drops it (SPEC §24.7).
    #[test]
    fn a_short_grid_keeps_the_hints_and_drops_the_wordmark() {
        let snapshot = hint_snapshot(20, 5, &["▀▀▀", " █ ", " █ "]);

        let grid = grid(&snapshot);
        assert!(
            grid.iter().all(|row| !row.contains('▀')),
            "the mark was painted anyway: {grid:?}"
        );
        assert!(
            grid.iter().any(|row| row.contains("ctrl-p  find a file")),
            "{grid:?}"
        );
    }

    #[test]
    fn the_command_line_ghosts_the_rest_of_the_command() {
        let mut snapshot = snapshot_of(40, 4, &["x"]);
        snapshot.command_line = Some(CommandLineView {
            prefix: ':',
            query: "w".to_owned(),
            cursor: 1,
            ghost: Some("q".to_owned()),
            trailing: vec!["ctrl-n: 4 more".to_owned()],
            message: None,
        });
        let grid = grid(&snapshot);
        assert_eq!(
            grid[2].trim_end(),
            ":wq                       ctrl-n: 4 more"
        );

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 4));
        render(&snapshot, &palette(), &mut buffer);
        // The ghost is dimmed and the query is not, which is the whole of what
        // says one was typed and the other was offered.
        assert!(
            buffer
                .cell((2, 2))
                .is_some_and(|cell| cell.modifier.contains(Modifier::DIM))
        );
        assert!(
            buffer
                .cell((1, 2))
                .is_some_and(|cell| !cell.modifier.contains(Modifier::DIM))
        );
    }

    #[test]
    fn the_right_hand_end_takes_the_first_thing_that_fits() {
        let mut snapshot = snapshot_of(24, 4, &["x"]);
        snapshot.command_line = Some(CommandLineView {
            prefix: ':',
            query: "save".to_owned(),
            cursor: 4,
            ghost: None,
            trailing: vec!["ctrl-s".to_owned(), "ctrl-n: 9 more".to_owned()],
            message: None,
        });
        assert_eq!(grid(&snapshot)[2].trim_end(), ":save             ctrl-s");

        // With no room for the keybinding *and* what is already on the row, the
        // count is not a smaller answer — nothing is painted rather than
        // something misleading.
        let mut narrow = snapshot_of(12, 4, &["x"]);
        narrow.command_line = Some(CommandLineView {
            prefix: ':',
            query: "save all".to_owned(),
            cursor: 8,
            ghost: None,
            trailing: vec!["ctrl-shift-s".to_owned()],
            message: None,
        });
        assert_eq!(grid(&narrow)[2].trim_end(), ":save all");
    }

    #[test]
    fn a_diagnostic_underline_keeps_the_colour_its_severity_gave_it() {
        let mut snapshot = snapshot_of(12, 3, &["let x = 1;"]);
        let error = hsla(0.0, 0.8, 0.5, 1.0);
        if let Some(editor) = snapshot.editor_mut() {
            editor.rows[0].spans = vec![StyledSpan {
                range: 0..10,
                style: SpanStyle {
                    underline: Some(Underline { color: Some(error) }),
                    ..Default::default()
                },
            }];
        }

        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        render(&snapshot, &palette(), &mut buffer);
        let cell = buffer.cell((0, 0)).expect("no cell");
        assert!(cell.modifier.contains(Modifier::UNDERLINED));
        assert_eq!(cell.underline_color, palette().color(error));
    }

    #[test]
    fn the_hardware_cursor_follows_whichever_surface_owns_the_keyboard() {
        let mut snapshot = snapshot_of(40, 8, &["x"]);
        snapshot.cursor = Some(CellPoint { column: 4, row: 0 });
        assert_eq!(cursor(&snapshot).0, Some(CellPoint { column: 4, row: 0 }));

        snapshot.command_line = Some(CommandLineView {
            prefix: ':',
            query: "wq".to_owned(),
            cursor: 2,
            ..Default::default()
        });
        let (position, shape) = cursor(&snapshot);
        assert_eq!(position, Some(CellPoint { column: 3, row: 6 }));
        assert_eq!(shape, CursorShape::Bar);

        // An open list is in front of the `:` line as well as the editor.
        snapshot.overlay = Some(finder("sna", vec![overlay_row("a.rs", "src")], "1/1"));
        let (position, shape) = cursor(&snapshot);
        assert_eq!(position, Some(CellPoint { column: 7, row: 1 }));
        assert_eq!(shape, CursorShape::Bar);

        // A surface with nothing to type in hides it rather than leaving it
        // under a box.
        if let Some(overlay) = snapshot.overlay.as_mut() {
            overlay.query = None;
            overlay.placement = OverlayPlacement::TopCentre;
        }
        assert_eq!(cursor(&snapshot).0, None);
    }

    /// SPEC's M3 slice: a block row `highlighted_chunks` gave no text of its
    /// own must still read as *something*, not the blank line an empty-text
    /// row shares with a genuinely empty buffer line.
    #[test]
    fn a_block_row_shows_a_dimmed_placeholder_instead_of_blank_text() {
        let placeholder = hsla(0.0, 0.0, 0.6, 1.0);
        let placeholder_background = hsla(0.0, 0.0, 0.2, 1.0);
        let mut snapshot = snapshot_of(30, 3, &["‹block ted cannot render›"]);
        if let Some(editor) = snapshot.editor_mut() {
            editor.rows[0].kind = RowKind::Block;
            editor.rows[0].background = Some(placeholder_background);
            editor.rows[0].spans = vec![StyledSpan {
                range: 0..editor.rows[0].text.len(),
                style: SpanStyle {
                    foreground: Some(placeholder),
                    background: Some(placeholder_background),
                    ..Default::default()
                },
            }];
        }
        assert!(grid(&snapshot)[0].starts_with("‹block ted cannot render›"));

        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 3));
        render(&snapshot, &palette(), &mut buffer);
        let tint = palette().color(placeholder_background);
        // The tint reaches past the label too, so the row reads as one block
        // rather than text followed by the editor's ordinary background.
        assert_eq!(buffer.cell((0, 0)).map(|cell| cell.bg), Some(tint));
        assert_eq!(buffer.cell((29, 0)).map(|cell| cell.bg), Some(tint));
    }

    fn gutter_snapshot(columns: u16, rows: u16, gutter_width: u16, line: &str) -> ViewSnapshot {
        let mut snapshot = snapshot_of(columns, rows, &[line]);
        if let Some(editor) = snapshot.editor_mut() {
            editor.gutter_rect = CellRect::new(0, 0, gutter_width, rows.saturating_sub(1));
            editor.text_rect = CellRect::new(
                gutter_width,
                0,
                columns - gutter_width,
                rows.saturating_sub(1),
            );
        }
        snapshot
    }

    #[test]
    fn a_fold_chevron_points_right_when_folded_and_down_when_foldable() {
        let mut snapshot = gutter_snapshot(20, 2, 6, "x");
        if let Some(editor) = snapshot.editor_mut() {
            editor.fold_gutter_cells = 2;
            editor.rows[0].gutter.crease = Some(CreaseState::Folded);
        }
        assert_eq!(grid(&snapshot)[0].chars().nth(5), Some('▸'));

        if let Some(editor) = snapshot.editor_mut() {
            editor.rows[0].gutter.crease = Some(CreaseState::Foldable);
        }
        assert_eq!(grid(&snapshot)[0].chars().nth(5), Some('▾'));
    }

    /// SPEC's M3 slice: the chevron is conditional on the room Zed's own
    /// gutter layout reported, not painted just because a crease exists.
    #[test]
    fn no_chevron_is_painted_when_the_gutter_reserved_no_room_for_one() {
        let mut snapshot = gutter_snapshot(20, 2, 6, "x");
        if let Some(editor) = snapshot.editor_mut() {
            editor.fold_gutter_cells = 0;
            editor.rows[0].gutter.crease = Some(CreaseState::Folded);
        }
        assert_eq!(grid(&snapshot)[0].chars().nth(5), Some(' '));
    }

    /// SPEC §11 step 1: where a row *is* tinted — an expanded hunk, which is
    /// the only diff row the projection gives a background to — that tint
    /// reaches across the gutter and the text, not just the marker's own cell.
    #[test]
    fn a_diff_hunk_tints_the_gutter_and_the_text_row() {
        let added = hsla(0.3, 0.5, 0.4, 1.0);
        let added_background = hsla(0.3, 0.3, 0.15, 1.0);
        let mut snapshot = gutter_snapshot(20, 2, 6, "let x = 1;");
        if let Some(editor) = snapshot.editor_mut() {
            editor.rows[0].background = Some(added_background);
            editor.rows[0].gutter.diff = Some(DiffMarker::Added);
            editor.rows[0].gutter.diff_foreground = Some(added);
            editor.rows[0].gutter.line_number = Some(3);
        }

        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        render(&snapshot, &palette(), &mut buffer);
        let tint = palette().color(added_background);
        // The marker's own cell, a blank gutter cell beside it, and a text
        // cell all carry the same tint. The marker keeps it only because this
        // row named no staged/unstaged background of its own; when it does,
        // that background wins the one cell — see the test below.
        assert_eq!(buffer.cell((0, 0)).map(|cell| cell.bg), Some(tint));
        assert_eq!(buffer.cell((1, 0)).map(|cell| cell.bg), Some(tint));
        assert_eq!(buffer.cell((10, 0)).map(|cell| cell.bg), Some(tint));
        // The marker itself takes the diff's own colour rather than the line
        // number's active/muted one.
        assert_eq!(
            buffer.cell((0, 0)).map(|cell| cell.fg),
            Some(palette().color(added))
        );
        assert_eq!(
            buffer.cell((0, 0)).map(|cell| cell.symbol().to_owned()),
            Some("+".to_owned())
        );
    }

    /// SPEC §11 step 1: staging lifts the marker out of the hunk's tint. A
    /// staged marker is painted on the background the projection names, an
    /// unstaged one on the row's tint like every other cell, and neither
    /// changes anything outside that single cell.
    #[test]
    fn only_a_staged_marker_leaves_the_hunks_tint() {
        let tint = hsla(0.3, 0.3, 0.15, 1.0);
        let editor_background = hsla(0.6, 0.1, 0.08, 1.0);

        let cell_backgrounds = |marker_background| {
            let mut snapshot = gutter_snapshot(20, 2, 6, "let x = 1;");
            if let Some(editor) = snapshot.editor_mut() {
                editor.rows[0].background = Some(tint);
                editor.rows[0].gutter.diff = Some(DiffMarker::Added);
                editor.rows[0].gutter.diff_background = marker_background;
                editor.rows[0].gutter.line_number = Some(3);
            }
            let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
            render(&snapshot, &palette(), &mut buffer);
            (
                buffer.cell((0, 0)).map(|cell| cell.bg),
                buffer.cell((1, 0)).map(|cell| cell.bg),
            )
        };

        // Unstaged: the projection names no background, so the marker keeps the
        // row's tint and is indistinguishable from the cell beside it.
        let (unstaged_marker, unstaged_beside) = cell_backgrounds(None);
        assert_eq!(unstaged_marker, Some(palette().color(tint)));
        assert_eq!(unstaged_marker, unstaged_beside);

        // Staged: the editor's own background, which is what takes the marker
        // out of the tint the rest of the row keeps.
        let (staged_marker, staged_beside) = cell_backgrounds(Some(editor_background));
        assert_eq!(staged_marker, Some(palette().color(editor_background)));
        assert_ne!(
            staged_marker, unstaged_marker,
            "staged and unstaged must not paint the same cell"
        );
        // The cell beside it is the row's tint either way: staging says
        // nothing about the rest of the row.
        assert_eq!(staged_beside, Some(palette().color(tint)));
    }
}
