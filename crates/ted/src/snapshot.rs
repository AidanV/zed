//! The backend-to-frontend projection (SPEC §10): one struct, rebuilt each
//! frame from GPUI-side reads, containing no GPUI handles — only plain data.
//! That constraint is what keeps an out-of-process frontend possible later, and
//! it makes the renderer testable without a terminal.

use std::ops::Range;

use buffer_diff::DiffHunkStatusKind;
use editor::display_map::DisplayRow;
use editor::{DisplayPoint, Editor};
use gpui::{App, Entity, FontStyle, FontWeight, Hsla, Pixels, Window};
use language::LanguageAwareStyling;
use multi_buffer::RowInfo;
use unicode_segmentation::UnicodeSegmentation as _;
use workspace::Workspace;

use crate::cell::{CELL_HEIGHT, CELL_WIDTH, cluster_cells};

/// A rectangle in terminal cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl CellRect {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// A position in terminal cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellPoint {
    pub column: u16,
    pub row: u16,
}

/// The shape the terminal's own cursor is asked to take, which is how `ted`
/// signals vim's mode without drawing a cell (SPEC §7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewSnapshot {
    pub columns: u16,
    pub rows: u16,
    pub editor: Option<EditorView>,
    pub status: StatusView,
    pub command_line: Option<CommandLineView>,
    pub prompt: Option<PromptView>,
    pub notifications: Vec<String>,
    /// Where to park the terminal's hardware cursor. SPEC §7 places the real
    /// cursor rather than drawing one, so the terminal blinks it and screen
    /// readers see it.
    pub cursor: Option<CellPoint>,
    pub cursor_shape: CursorShape,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorView {
    /// Where the editor paints text, as the editor itself reported it
    /// (SPEC §10.2). Never assumed; always read back from layout.
    pub text_rect: CellRect,
    /// Zero-width when the gutter is hidden, in which case nothing is painted
    /// there and `text_rect` starts at the editor's left edge.
    pub gutter_rect: CellRect,
    /// Horizontal scroll in cells. Vertical scroll is already applied: `rows`
    /// is the window into the display map, so `ted` never keeps its own
    /// vertical offset (SPEC §15).
    pub scroll_columns: u16,
    pub rows: Vec<RowView>,
    pub selections: Vec<SelectionSpan>,
    /// Cursors other than the primary one, in absolute grid cells. The terminal
    /// has a single hardware cursor, which the primary one owns, so these are
    /// drawn as inverted cells instead (SPEC §11).
    pub secondary_cursors: Vec<CellPoint>,
    pub background: Option<Hsla>,
    pub selection_background: Option<Hsla>,
    pub max_display_row: u32,
    pub soft_wrapped: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowKind {
    #[default]
    Text,
    /// A block decoration (diagnostics, git blame, excerpt headers). M1 paints
    /// the row's plain text; M3 gives blocks their own rendering.
    Block,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowView {
    pub display_row: u32,
    pub kind: RowKind,
    pub gutter: GutterView,
    /// The row exactly as the display map produced it. Tabs are already
    /// expanded to spaces by `TabMap`, so this contains no `\t` (SPEC §11).
    pub text: String,
    /// Styled slices of `text`, in order and covering it exactly.
    pub spans: Vec<StyledSpan>,
    /// Byte column to cell column for this row, built once per row per frame.
    /// `DisplayPoint::column()` is a byte offset, so every consumer — cursor,
    /// selections, highlights — reads this one table instead of converting
    /// ad hoc (SPEC §5.4, and the §22 risk row on byte/grapheme confusion).
    pub byte_to_cell: Vec<u16>,
    pub soft_wrap_indent: u16,
}

impl RowView {
    pub fn new(display_row: u32, text: String) -> Self {
        let byte_to_cell = byte_to_cell_table(&text);
        let spans = vec![StyledSpan {
            range: 0..text.len(),
            style: SpanStyle::default(),
        }];
        Self {
            display_row,
            kind: RowKind::Text,
            gutter: GutterView::default(),
            text,
            spans,
            byte_to_cell,
            soft_wrap_indent: 0,
        }
    }

    /// The cell column for a byte column, saturating at the end of the row so
    /// a cursor past the last byte still lands somewhere sensible.
    pub fn cell_for_byte(&self, byte_column: usize) -> u16 {
        match self.byte_to_cell.get(byte_column) {
            Some(&cell) => cell,
            None => self.byte_to_cell.last().copied().unwrap_or(0),
        }
    }

    /// The cells this row occupies in total.
    pub fn cell_width(&self) -> u16 {
        self.byte_to_cell.last().copied().unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyledSpan {
    /// A byte range within [`RowView::text`], rather than an owned copy, so a
    /// row's text exists once and `byte_to_cell` indexes it directly.
    pub range: Range<usize>,
    pub style: SpanStyle,
}

/// A resolved text style: theme colours and the attributes a terminal can
/// express. Not a GPUI `HighlightStyle`, so [`crate::palette`] is the only place
/// theme types are interpreted (SPEC §10.1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpanStyle {
    pub foreground: Option<Hsla>,
    pub background: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GutterView {
    /// The buffer row to display, already 1-based. `None` on soft-wrapped
    /// continuation rows and block rows, which carry no line number.
    pub line_number: Option<u32>,
    pub diff: Option<DiffMarker>,
    /// Resolved here rather than in the renderer, so the active line number's
    /// brighter colour is a theme decision like every other colour.
    pub style: SpanStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffMarker {
    Added,
    Modified,
    Deleted,
}

impl DiffMarker {
    pub fn symbol(self) -> char {
        match self {
            Self::Added => '+',
            Self::Modified => '~',
            Self::Deleted => '-',
        }
    }
}

/// A run of selected cells on one display row. Visual-block mode yields one of
/// these per row with the same column range, which is what makes it rectangular
/// with no special case in the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionSpan {
    pub display_row: u32,
    pub start_cell: u16,
    /// Exclusive. Equal to `start_cell` for an empty selection, which paints
    /// nothing.
    pub end_cell: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusView {
    pub mode: Option<String>,
    pub path: Option<String>,
    pub dirty: bool,
    /// One-based line and column of the primary cursor, as a user expects to
    /// read them.
    pub position: Option<(u32, u32)>,
    /// Multi-keystroke bindings in flight, e.g. `d2` while `d2w` is being typed
    /// (SPEC §8.3).
    pub pending_keys: Option<String>,
    /// Surfaced whenever there is more than one pane, because M1 renders only
    /// the active one and invisible state the user can navigate into is the
    /// failure mode to avoid (SPEC §14.3).
    pub panes: Option<(usize, usize)>,
    /// A wrap pass still running after a resize on a large file (SPEC §10.2).
    pub rewrapping: bool,
}

/// `ted`'s own `:` or `/` line (SPEC §13.2). The query is `ted`'s to render;
/// the semantics belong to vim's interceptor and to `BufferSearchBar`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandLineView {
    pub prefix: char,
    pub query: String,
    /// Byte offset of the insertion point within `query`.
    pub cursor: usize,
    pub completions: Vec<String>,
    pub selected_completion: Option<usize>,
    /// Match counts for `/`, or an error for `:`.
    pub message: Option<String>,
}

/// A question GPUI asked the window, waiting on an answer (SPEC §13.3).
///
/// The answers are numbered rather than laid out as buttons because a terminal
/// has no pointer: the number is the whole interaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptView {
    pub message: String,
    pub detail: Option<String>,
    pub answers: Vec<String>,
}

/// Everything the projection needs that `ted` — not Zed — is the source of.
pub struct Frame<'a> {
    pub columns: u16,
    pub rows: u16,
    /// The rows `ted` paints itself, withheld from the GPUI window
    /// (SPEC §10.2). The editor's rect can never reach into them.
    pub reserved_rows: u16,
    pub command_line: Option<CommandLineView>,
    pub prompt: Option<PromptView>,
    pub notifications: Vec<String>,
    pub workspace: &'a Entity<Workspace>,
}

/// Reads the projection out of the entities.
///
/// Must run *after* the draw: `Editor::style` and `last_bounds` are populated
/// during element layout, so a projection taken before the first frame has
/// neither (SPEC §10.2, "Ordering"). A missing editor at any step yields an
/// empty buffer view rather than failing (SPEC §9).
pub fn build(frame: Frame<'_>, window: &mut Window, cx: &mut App) -> ViewSnapshot {
    let notifications = frame.notifications;
    let workspace = frame.workspace.read(cx);
    let active_pane = workspace.active_pane().clone();
    let panes = workspace.panes().len();
    let pane_index = workspace
        .panes()
        .iter()
        .position(|pane| pane == &active_pane)
        .map(|index| index + 1)
        .unwrap_or(1);

    let mut status = StatusView {
        // Surfaced only when there is more than one, because M1 renders just
        // the active pane (SPEC §14.3).
        panes: (panes > 1).then_some((pane_index, panes)),
        pending_keys: window
            .pending_input_keystrokes()
            .filter(|keystrokes| !keystrokes.is_empty())
            .map(|keystrokes| {
                keystrokes
                    .iter()
                    .map(|keystroke| keystroke.unparse())
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        ..Default::default()
    };

    let Some(item) = workspace.active_item(cx) else {
        return ViewSnapshot {
            columns: frame.columns,
            rows: frame.rows,
            status,
            command_line: frame.command_line,
            prompt: frame.prompt,
            notifications,
            ..Default::default()
        };
    };
    // `tab_content_text` rather than the project path: opening a single file
    // makes it its own worktree root, and the path relative to that root is the
    // empty string.
    status.path = Some(item.tab_content_text(0, cx).to_string());
    status.dirty = item.is_dirty(cx);

    let Some(editor) = item.act_as::<Editor>(cx) else {
        return ViewSnapshot {
            columns: frame.columns,
            rows: frame.rows,
            status,
            command_line: frame.command_line,
            prompt: frame.prompt,
            notifications,
            ..Default::default()
        };
    };

    let mut snapshot = for_editor(
        &editor,
        frame.columns,
        frame.rows,
        frame.reserved_rows,
        window,
        cx,
    );
    status.mode = snapshot.status.mode.take();
    status.position = snapshot.status.position;
    status.rewrapping = snapshot.status.rewrapping;

    snapshot.status = status;
    snapshot.command_line = frame.command_line;
    snapshot.prompt = frame.prompt;
    snapshot.notifications = notifications;
    snapshot
}

/// The projection of a single editor, without a workspace around it.
///
/// `build` goes through here, and so do tests: everything the cell contract
/// governs — rects, wrap, cursor placement, gutter, selections, syntax spans —
/// is decided in this function, so exercising it needs no `Project`, `Client`
/// or database.
pub fn for_editor(
    editor: &Entity<Editor>,
    columns: u16,
    rows: u16,
    reserved_rows: u16,
    window: &mut Window,
    cx: &mut App,
) -> ViewSnapshot {
    let editor_height = rows.saturating_sub(reserved_rows);
    let (view, cursor) = editor.update(cx, |editor, cx| {
        build_editor_view(editor, columns, editor_height, window, cx)
    });

    let mode = vim::mode(editor.read(cx), cx).map(|mode| mode.to_string());
    let cursor_shape = match mode.as_deref() {
        Some("INSERT") => CursorShape::Bar,
        Some("REPLACE") => CursorShape::Underline,
        _ => CursorShape::Block,
    };

    ViewSnapshot {
        columns,
        rows,
        editor: Some(view.editor),
        status: StatusView {
            mode,
            position: view.primary_position,
            rewrapping: view.rewrapping,
            pending_keys: window
                .pending_input_keystrokes()
                .filter(|keystrokes| !keystrokes.is_empty())
                .map(|keystrokes| {
                    keystrokes
                        .iter()
                        .map(|keystroke| keystroke.unparse())
                        .collect::<Vec<_>>()
                        .join(" ")
                }),
            ..Default::default()
        },
        command_line: None,
        prompt: None,
        notifications: Vec::new(),
        cursor,
        cursor_shape,
    }
}

/// What the backend wants to tell the user that `ted` has no other place for.
///
/// Read before the window is sized rather than during [`build`], because every
/// line here costs a reserved row and the window must be sized to the grid minus
/// exactly those rows (SPEC §10.2).
pub fn backend_notifications(
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Vec<String> {
    let mut notifications = Vec::new();

    // A modal `ted` has no projection for still has focus and still consumes
    // keys, so it must degrade visibly rather than invisibly (SPEC §13.1).
    if workspace.update(cx, |workspace, cx| workspace.has_active_modal(window, cx)) {
        notifications.push("a modal is open that ted cannot render — press escape".to_owned());
    }

    // A workspace notification's content is a view, so until the Mirror strategy
    // lands (SPEC §13.1) `ted` can only report that one exists. That still beats
    // silence: a dropped "unable to save" is worse than useless (SPEC §13.3).
    notifications.extend(
        workspace
            .read(cx)
            .notification_ids()
            .iter()
            .map(describe_notification),
    );
    notifications
}

fn describe_notification(id: &workspace::notifications::NotificationId) -> String {
    use workspace::notifications::NotificationId;
    match id {
        NotificationId::Named(name) => format!("Zed: {name}"),
        // The other variants are keyed by `TypeId`, which has no name worth
        // showing, so all `ted` can honestly say is that one is there.
        NotificationId::Unique(_) | NotificationId::Composite(_, _) => {
            "Zed reported something ted cannot render yet".to_owned()
        }
    }
}

struct BuiltEditor {
    editor: EditorView,
    /// One-based line and column of the primary cursor in the buffer, which is
    /// what the status line shows — display coordinates would count soft-wrap
    /// rows and confuse anyone comparing against `:42`.
    primary_position: Option<(u32, u32)>,
    rewrapping: bool,
}

fn build_editor_view(
    editor: &mut Editor,
    columns: u16,
    rows: u16,
    window: &mut Window,
    cx: &mut gpui::Context<Editor>,
) -> (BuiltEditor, Option<CellPoint>) {
    let style = editor.style(cx).clone();
    let editor_snapshot = editor.snapshot(window, cx);
    let display = &editor_snapshot.display_snapshot;

    let font_size = style.text.font_size.to_pixels(window.rem_size());
    let font_id = cx.text_system().resolve_font(&style.text.font());
    let gutter = editor_snapshot.gutter_dimensions(font_id, font_size, &style, window, cx);

    let bounds = editor.last_bounds().copied();
    let origin = bounds.map(|bounds| bounds.origin).unwrap_or_default();
    let top = cells(origin.y, CELL_HEIGHT);
    let left = cells(origin.x, CELL_WIDTH);
    let height = bounds
        .map(|bounds| cells(bounds.size.height, CELL_HEIGHT))
        .unwrap_or(rows)
        .min(rows.saturating_sub(top));

    // Rounded up rather than floored: the gutter's own margin is a fraction of
    // a cell (SPEC §5.4), and rounding it down would put the first column of
    // text on top of the last column of the gutter.
    let gutter_cells = cells_ceil(gutter.full_width(), CELL_WIDTH);
    // The floored column count is authoritative wherever it and a rect could
    // disagree (SPEC §5.3): it is exactly the count `calculate_wrap_width`
    // wrapped against.
    let text_width = editor
        .visible_column_count()
        .map(|count| count.max(0.0).floor() as u16)
        .unwrap_or_else(|| columns.saturating_sub(gutter_cells));

    let gutter_rect = CellRect::new(left, top, gutter_cells, height);
    let text_rect = CellRect::new(
        left + gutter_cells,
        top,
        text_width.min(columns.saturating_sub(left + gutter_cells)),
        height,
    );

    let scroll = editor.scroll_position(cx);
    let first_row = scroll.y.max(0.0) as u32;
    // Fractional visible counts mean a partial bottom row; render the whole
    // ones and drop the partial (SPEC §5.3).
    let visible = editor
        .visible_line_count()
        .map(|count| count.max(0.0).floor() as u32)
        .unwrap_or(u32::from(height));
    let last_display_row = display.max_point().row().0;

    let relative_numbers = editor.relative_line_numbers(cx).enabled();
    let show_line_numbers = editor.line_numbers_enabled(cx);
    let cursor_display_row = editor.selections.newest_display(display).head().row().0;

    let end_row = first_row
        .saturating_add(visible)
        .min(last_display_row.saturating_add(1));
    let built_rows = rows_from_chunks(&editor_snapshot, first_row..end_row, &style);

    let mut row_infos = display.row_infos(DisplayRow(first_row));
    let mut rows_view = Vec::new();
    for (offset, (text, spans)) in built_rows.into_iter().enumerate() {
        let display_row = first_row.saturating_add(offset as u32);
        let info = row_infos.next().unwrap_or_default();
        let mut row = RowView::new(display_row, text);
        row.spans = spans;
        row.kind = if display.is_block_line(DisplayRow(display_row)) {
            RowKind::Block
        } else {
            RowKind::Text
        };
        row.soft_wrap_indent = display
            .soft_wrap_indent(DisplayRow(display_row))
            .unwrap_or(0)
            .min(u32::from(u16::MAX)) as u16;
        row.gutter = GutterView {
            line_number: show_line_numbers
                .then(|| line_number_for(&info, display_row, cursor_display_row, relative_numbers))
                .flatten(),
            diff: info.diff_status.map(|status| match status.kind {
                DiffHunkStatusKind::Added => DiffMarker::Added,
                DiffHunkStatusKind::Modified => DiffMarker::Modified,
                DiffHunkStatusKind::Deleted => DiffMarker::Deleted,
            }),
            style: SpanStyle {
                foreground: Some(if display_row == cursor_display_row {
                    style.status.info
                } else {
                    style.status.ignored
                }),
                ..Default::default()
            },
        };
        rows_view.push(row);
    }

    let visible_rows = first_row..first_row.saturating_add(visible);
    let mut selections = Vec::new();
    let mut secondary_cursors = Vec::new();
    let newest = editor.selections.newest_display(display);
    let mut primary_cursor = None;

    for selection in editor.selections.all_display(display) {
        let range = selection.start..selection.end;
        for (display_row, start_cell, end_cell) in
            selection_cells(&rows_view, &range, visible_rows.clone())
        {
            if start_cell != end_cell {
                selections.push(SelectionSpan {
                    display_row,
                    start_cell,
                    end_cell,
                });
            }
        }

        let head = selection.head();
        let Some(point) = cell_for(&rows_view, head.row().0, head.column() as usize, &text_rect)
        else {
            continue;
        };
        if selection.id == newest.id {
            primary_cursor = Some(point);
        } else {
            secondary_cursors.push(point);
        }
    }

    // Buffer coordinates, not display ones: a status line that counted
    // soft-wrap rows would disagree with `:42` and with every other editor.
    let buffer_point = newest.head().to_point(display);
    let primary_position = Some((buffer_point.row + 1, buffer_point.column + 1));

    let built = BuiltEditor {
        editor: EditorView {
            text_rect,
            gutter_rect,
            scroll_columns: scroll.x.max(0.0) as u16,
            rows: rows_view,
            selections,
            secondary_cursors,
            background: Some(style.background),
            selection_background: Some(style.local_player.selection),
            max_display_row: last_display_row,
            soft_wrapped: matches!(editor.soft_wrap_mode(cx), editor::SoftWrap::EditorWidth),
        },
        primary_position,
        rewrapping: editor.display_map.read(cx).is_rewrapping(cx),
    };
    (built, primary_cursor)
}

/// The buffer row a gutter shows, honouring relative line numbers. `None` on
/// soft-wrapped continuation rows, which repeat their parent's number in Zed
/// only when `relative_line_numbers` is `wrapped`.
fn line_number_for(
    info: &RowInfo,
    display_row: u32,
    cursor_display_row: u32,
    relative: bool,
) -> Option<u32> {
    let buffer_row = info.buffer_row?;
    if relative && display_row != cursor_display_row {
        return Some(display_row.abs_diff(cursor_display_row));
    }
    Some(buffer_row + 1)
}

/// The text and styling of every visible row, from one pass over the display
/// map. `highlighted_chunks` is the same call `EditorElement` makes, so the
/// colours are exactly the ones GUI Zed would paint.
///
/// One call for the whole range, not one per row: constructing the iterator
/// seeks the multibuffer and the syntax tree, and asking it for a single row
/// pays that seek again for every row on screen. `EditorElement` makes exactly
/// one call for its visible range too (`element.rs:3119`) and splits the chunks
/// on newlines, which is also where each row's text comes from — so the text
/// and the styling cannot disagree about where a row ends.
fn rows_from_chunks(
    snapshot: &editor::EditorSnapshot,
    rows: Range<u32>,
    style: &editor::EditorStyle,
) -> Vec<(String, Vec<StyledSpan>)> {
    let mut built =
        vec![(String::new(), Vec::new()); rows.end.saturating_sub(rows.start) as usize];
    let language_aware = LanguageAwareStyling {
        tree_sitter: true,
        diagnostics: true,
    };
    let chunks = snapshot.display_snapshot.highlighted_chunks(
        DisplayRow(rows.start)..DisplayRow(rows.end),
        language_aware,
        style,
    );

    let mut row = 0usize;
    for chunk in chunks {
        if row >= built.len() {
            break;
        }
        let chunk_style = span_style(chunk.style, style);
        // A chunk is not bounded by a row, so its newlines are what advance the
        // row rather than the chunk boundary.
        for (index, segment) in chunk.text.split('\n').enumerate() {
            if index > 0 {
                row += 1;
            }
            let Some((text, spans)) = built.get_mut(row) else {
                break;
            };
            if segment.is_empty() {
                continue;
            }
            let start = text.len();
            text.push_str(segment);
            spans.push(StyledSpan {
                range: start..text.len(),
                style: chunk_style,
            });
        }
    }
    built
}

fn span_style(
    highlight: Option<gpui::HighlightStyle>,
    editor_style: &editor::EditorStyle,
) -> SpanStyle {
    let Some(highlight) = highlight else {
        return SpanStyle {
            foreground: Some(editor_style.text.color),
            ..Default::default()
        };
    };
    SpanStyle {
        foreground: Some(highlight.color.unwrap_or(editor_style.text.color)),
        background: highlight.background_color,
        bold: highlight
            .font_weight
            .is_some_and(|weight| weight >= FontWeight::BOLD),
        italic: highlight.font_style == Some(FontStyle::Italic),
        underline: highlight.underline.is_some(),
        strikethrough: highlight.strikethrough.is_some(),
    }
}

/// The cells a selection covers on each visible row it touches. A visual-block
/// selection produces the same column range on every row, so the rectangle
/// falls out with no special case.
fn selection_cells(
    rows: &[RowView],
    range: &Range<DisplayPoint>,
    visible: Range<u32>,
) -> Vec<(u32, u16, u16)> {
    let mut spans = Vec::new();
    for display_row in range.start.row().0..=range.end.row().0 {
        if !visible.contains(&display_row) {
            continue;
        }
        let Some(row) = rows.iter().find(|row| row.display_row == display_row) else {
            continue;
        };
        let start = if display_row == range.start.row().0 {
            row.cell_for_byte(range.start.column() as usize)
        } else {
            0
        };
        let end = if display_row == range.end.row().0 {
            row.cell_for_byte(range.end.column() as usize)
        } else {
            row.cell_width()
        };
        spans.push((display_row, start, end.max(start)));
    }
    spans
}

/// A display position as an absolute grid cell, through the row's
/// `byte_to_cell` table — the only permitted byte-to-cell conversion
/// (SPEC §5.4).
fn cell_for(
    rows: &[RowView],
    display_row: u32,
    byte_column: usize,
    text_rect: &CellRect,
) -> Option<CellPoint> {
    let offset = rows.iter().position(|row| row.display_row == display_row)?;
    let column = rows.get(offset)?.cell_for_byte(byte_column);
    let offset = u16::try_from(offset).ok()?;
    if offset >= text_rect.height {
        return None;
    }
    Some(CellPoint {
        column: text_rect.x + column.min(text_rect.width.saturating_sub(1)),
        row: text_rect.y + offset,
    })
}

fn cells(pixels: Pixels, cell: Pixels) -> u16 {
    (f32::from(pixels) / f32::from(cell)).max(0.0) as u16
}

fn cells_ceil(pixels: Pixels, cell: Pixels) -> u16 {
    (f32::from(pixels) / f32::from(cell)).max(0.0).ceil() as u16
}

/// Maps every byte offset in `text` to its cell column, with one extra entry
/// for the position just past the end. Bytes inside a grapheme cluster all map
/// to the cluster's starting cell, which is where a cursor belongs.
pub fn byte_to_cell_table(text: &str) -> Vec<u16> {
    let mut table = Vec::with_capacity(text.len() + 1);
    let mut cells: u32 = 0;
    for cluster in text.graphemes(true) {
        for _ in 0..cluster.len() {
            table.push(cells.min(u16::MAX as u32) as u16);
        }
        cells += cluster_cells(cluster);
    }
    table.push(cells.min(u16::MAX as u32) as u16);
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_bytes_map_to_their_own_column() {
        assert_eq!(byte_to_cell_table("abc"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn wide_characters_advance_two_cells() {
        // Each of these is 3 bytes and 2 cells wide.
        assert_eq!(byte_to_cell_table("日本"), vec![0, 0, 0, 2, 2, 2, 4]);
    }

    #[test]
    fn combining_marks_do_not_advance() {
        let table = byte_to_cell_table("e\u{0301}x");
        // "e" + combining acute is one cluster of one cell; "x" follows at 1.
        assert_eq!(table.last(), Some(&2));
        assert_eq!(table[0], 0);
        assert_eq!(table[3], 1);
    }

    #[test]
    fn bytes_inside_a_cluster_share_the_cluster_start() {
        let table = byte_to_cell_table("日");
        assert_eq!(table, vec![0, 0, 0, 2]);
    }

    #[test]
    fn empty_text_still_has_an_end_position() {
        assert_eq!(byte_to_cell_table(""), vec![0]);
    }

    #[test]
    fn cell_for_byte_saturates_past_the_end() {
        let row = RowView::new(0, "ab".to_owned());
        assert_eq!(row.cell_for_byte(0), 0);
        assert_eq!(row.cell_for_byte(2), 2);
        assert_eq!(row.cell_for_byte(99), 2);
    }

    #[test]
    fn a_bare_row_carries_one_span_covering_its_whole_text() {
        let row = RowView::new(3, "hello".to_owned());
        assert_eq!(row.spans.len(), 1);
        assert_eq!(row.spans[0].range, 0..5);
        assert_eq!(row.cell_width(), 5);
    }
}
