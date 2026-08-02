//! The backend-to-frontend projection (SPEC §10): one struct, rebuilt each
//! frame from GPUI-side reads, containing no GPUI handles — only plain data.
//! That constraint is what keeps an out-of-process frontend possible later, and
//! it makes the renderer testable without a terminal.

use std::collections::HashMap;
use std::ops::Range;

use buffer_diff::DiffHunkStatusKind;
use editor::display_map::{
    Block, ChunkRendererId, ChunkReplacement, DisplayRow, DisplaySnapshot, ToDisplayPoint as _,
};
use editor::{DisplayPoint, Editor};
use gpui::{App, Entity, FontStyle, FontWeight, Hsla, Pixels, Window};
use language::LanguageAwareStyling;
use multi_buffer::{MultiBufferSnapshot, RowInfo};
use theme::{ActiveTheme as _, ThemeColors};
use unicode_segmentation::UnicodeSegmentation as _;
use workspace::{Pane, Workspace};

use crate::cell::{CELL_HEIGHT, CELL_WIDTH, cluster_cells, text_cells};

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
    /// The pane's other items, along the top (SPEC §24.7). The only thing `ted`
    /// paints above the editor, and the reason every rect the editor reports is
    /// shifted down by a row while it is there.
    pub tabs: Option<TabStripView>,
    pub status: StatusView,
    pub command_line: Option<CommandLineView>,
    /// A list painted *over* the editor — the finder or the switcher
    /// (SPEC §24.1). It reserves no rows, so the buffer behind it never
    /// relayouts while it is open.
    pub overlay: Option<OverlayView>,
    /// The railed panel `shift-k` opens (SPEC §24.8), painted over the editor
    /// like an overlay and dismissed by the next key.
    pub hover: Option<HoverView>,
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
    /// Cells at the gutter's right edge Zed reserves for a fold indicator
    /// (`GutterDimensions::fold_area_width`, SPEC §11 step 1). The renderer
    /// paints a fold chevron only when this is nonzero, so a gutter Zed itself
    /// left no room in never gets one either.
    pub fold_gutter_cells: u16,
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
    /// The editor's default text colour. Rows carry their own colours, so this
    /// is here for what `ted` paints *over* the editor: a surface that named no
    /// colour of its own would inherit the syntax colouring of whatever cells it
    /// covered (SPEC §24.1).
    pub foreground: Option<Hsla>,
    pub selection_background: Option<Hsla>,
    pub max_display_row: u32,
    pub soft_wrapped: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowKind {
    #[default]
    Text,
    /// The header above a buffer that starts a new file in the multibuffer,
    /// folded or not (`Block::BufferHeader`, `Block::FoldedBuffer`). Labelled
    /// with the buffer's path, since `highlighted_chunks` gives block rows no
    /// text of their own (SPEC §11 point 6).
    BufferHeader,
    /// A boundary between two excerpts of the same buffer (`Block::ExcerptBoundary`).
    ExcerptHeader,
    /// A block `ted` has no projection for: diagnostics, git blame, code lens,
    /// and anything else built from `Block::Custom`'s opaque `render` closure.
    /// Degrades to a dimmed placeholder naming it, rather than the blank line
    /// `highlighted_chunks` gives every block row regardless of what it is.
    Block,
}

impl RowKind {
    /// Whether a cursor or selection may visibly land on this row. Every kind
    /// but `Text` is a decoration the display map only produces between real
    /// buffer positions, so a selection whose display range happens to span
    /// one must not paint over it (SPEC's M3 slice: blocks must not look
    /// selectable).
    pub fn is_selectable(self) -> bool {
        matches!(self, RowKind::Text)
    }
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
    /// The background this row paints across both the gutter and the text,
    /// before anything else: a diff hunk's tint, or a block/header's own
    /// surface (SPEC §11 step 1). `None` keeps the editor's own background
    /// from the initial fill.
    pub background: Option<Hsla>,
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
            background: None,
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
    pub underline: Option<Underline>,
    pub strikethrough: bool,
}

/// An underline and, when the highlight named one, its colour. Diagnostics are
/// the reason the colour is carried: severity reaches the renderer as the
/// underline's colour and nothing else, so dropping it would make an error and a
/// warning look identical (SPEC §24.8).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Underline {
    /// `None` underlines in the text's own colour.
    pub color: Option<Hsla>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GutterView {
    /// The buffer row to display, already 1-based. `None` on soft-wrapped
    /// continuation rows and block rows, which carry no line number.
    pub line_number: Option<u32>,
    pub diff: Option<DiffMarker>,
    /// The diff marker's own colour — the theme's status colour for
    /// added/modified/deleted — kept apart from `style`'s line-number colour,
    /// which tracks the cursor row instead (SPEC §11 step 1).
    pub diff_foreground: Option<Hsla>,
    /// Whether this row's buffer row has a crease, and which way the chevron
    /// should point (SPEC §11 step 1, and SPEC §5.4 on fold placeholders).
    pub crease: Option<CreaseState>,
    /// Resolved here rather than in the renderer, so the active line number's
    /// brighter colour is a theme decision like every other colour.
    pub style: SpanStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreaseState {
    /// Collapsed: the buffer content behind it is a fold placeholder.
    Folded,
    /// Not collapsed, but `DisplaySnapshot::crease_for_buffer_row` reports the
    /// row could be.
    Foldable,
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

/// A run of text with the byte offsets a query matched in it, emphasised by the
/// renderer. Both `fuzzy_nucleo`'s matchers and `CommandInterceptItem` already
/// report their matches in exactly this form, and the offsets map through
/// `byte_to_cell_table` like every other byte column (SPEC §5.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchedText {
    pub text: String,
    pub matched: Vec<usize>,
}

impl MatchedText {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            matched: Vec::new(),
        }
    }
}

/// A list painted over the editor: the file finder and the buffer switcher are
/// the same widget with different content (SPEC §24.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayView {
    /// Painted into the top border, Helix-style: "files", "buffers".
    pub title: Option<String>,
    /// Absent when the list is not filtered, which is what keeps the switcher
    /// three rows shorter than the finder (SPEC §24.6).
    pub query: Option<QueryView>,
    pub rows: Vec<OverlayRow>,
    pub selected: Option<usize>,
    /// "3/412", "no matches". Right-aligned on the query row when there is one,
    /// and into the bottom border otherwise.
    pub footer: Option<String>,
    pub placement: OverlayPlacement,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryView {
    pub text: String,
    /// Byte offset of the insertion point within `text`.
    pub cursor: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayRow {
    pub label: MatchedText,
    /// The second column: a file's directory, dimmed. Left-aligned at a column
    /// the renderer computes from the widest label, rather than right-aligned,
    /// because two files called `snapshot.rs` are told apart by a column that
    /// starts in the same place on every row (SPEC §24.4).
    pub detail: Option<MatchedText>,
    /// Unsaved work, painted as `•` after the label (SPEC §24.6).
    pub modified: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayPlacement {
    /// The width of the grid, top edge fixed at the first row (SPEC §24.4).
    #[default]
    Grid,
    /// Small and centred near the top, where Zed puts its own switcher
    /// (SPEC §24.6).
    TopCentre,
}

/// The pane's items along the top (SPEC §24.7).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStripView {
    pub tabs: Vec<TabView>,
    pub active: usize,
    /// The first tab painted. Overflow scrolls rather than eliding the middle,
    /// so the active tab is always on screen.
    pub first: usize,
    pub active_background: Option<Hsla>,
    pub background: Option<Hsla>,
    pub active_foreground: Option<Hsla>,
    pub foreground: Option<Hsla>,
    pub separator: Option<Hsla>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabView {
    /// From `Pane::tab_details`, so two files called `mod.rs` grow a directory
    /// in their labels and nothing else does.
    pub label: String,
    pub modified: bool,
}

/// The panel `shift-k` opens over the editor: a tinted block with a coloured bar
/// down its left edge, carrying the diagnostics under the cursor and the
/// language server's documentation for it (SPEC §24.8).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoverView {
    /// Where the panel goes, already resolved against the editor's text rect —
    /// below the cursor when the rows are there and above it when they are not.
    pub rect: CellRect,
    pub background: Option<Hsla>,
    /// Paired with `background`: the panel is a surface of the theme's rather
    /// than a window onto the buffer, so its text is the theme's UI colour and
    /// not the colour of the code underneath it.
    pub foreground: Option<Hsla>,
    pub blocks: Vec<HoverBlock>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoverBlock {
    /// What kind of thing is talking: the severity's colour for a diagnostic, a
    /// neutral accent for documentation. A position with both stacks them in one
    /// panel, so without this the two would read as one message.
    pub rail: Option<Hsla>,
    /// Already wrapped to the panel's width, because wrapping is a decision
    /// about cells and belongs on this side of the projection.
    pub lines: Vec<String>,
}

/// One thing the language server had to say about the cursor's position, before
/// it is wrapped into a panel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoverContent {
    pub rail: Option<Hsla>,
    pub text: String,
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
///
/// One row, and never more: completions are ghost text on the line rather than a
/// list, so nothing ever covers the buffer for a half-typed command (SPEC §24.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandLineView {
    pub prefix: char,
    pub query: String,
    /// Byte offset of the insertion point within `query`.
    pub cursor: usize,
    /// The rest of the selected candidate, dimmed after the cursor. Either the
    /// tail of a command the query is a prefix of — which `right` accepts — or a
    /// description of one, which it does not.
    pub ghost: Option<String>,
    /// What to right-align on the row, best first: the selected action's
    /// keybinding when it has one, then how many other candidates the matcher
    /// found. The renderer paints the first that fits beside the query, which is
    /// what "when there is room for it" means (SPEC §24.3).
    pub trailing: Vec<String>,
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
    /// How many of those rows sit *above* the editor — the tab strip, and
    /// nothing else in M2. Withholding them is not enough on its own: every rect
    /// the editor reports starts at the window's row 0 and has to be shifted
    /// down by exactly this much (SPEC §24.7).
    pub top_rows: u16,
    pub command_line: Option<CommandLineView>,
    /// `ted`'s own list, already built by [`crate::overlay`]. It floats over the
    /// editor and costs no reserved rows (SPEC §24.1).
    pub overlay: Option<OverlayView>,
    /// What [`crate::hover`] read for the cursor's position, unwrapped: the
    /// panel's rect and line breaks are decisions about cells and are taken
    /// here, where the editor's rect is known.
    pub hover: Vec<HoverContent>,
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
    let overlay = frame.overlay;
    let hover = frame.hover;
    let workspace = frame.workspace.read(cx);
    let active_pane = workspace.active_pane().clone();
    let panes = workspace.panes().len();
    let pane_index = workspace
        .panes()
        .iter()
        .position(|pane| pane == &active_pane)
        .map(|index| index + 1)
        .unwrap_or(1);
    // Gated on the row already withheld for it rather than on the item count
    // again: one answer to "is there a strip", so the window's size and the
    // rects painted inside it cannot disagree (SPEC §24.7).
    let tabs = (frame.top_rows > 0)
        .then(|| tab_strip(&active_pane, frame.columns, window, cx))
        .flatten();

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
            tabs,
            status,
            command_line: frame.command_line,
            overlay,
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
            tabs,
            status,
            command_line: frame.command_line,
            overlay,
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

    // The one addition SPEC §24.7 calls for, applied to the text rect, the
    // gutter rect and the cursor together: applying it to two of the three puts
    // the cursor a row off its own text.
    shift_down(&mut snapshot, frame.top_rows);

    snapshot.tabs = tabs;
    snapshot.status = status;
    snapshot.command_line = frame.command_line;
    snapshot.hover = place_hover(hover, &snapshot, cx);
    snapshot.overlay = overlay;
    snapshot.prompt = frame.prompt;
    snapshot.notifications = notifications;
    snapshot
}

fn shift_down(snapshot: &mut ViewSnapshot, rows: u16) {
    if rows == 0 {
        return;
    }
    if let Some(editor) = snapshot.editor.as_mut() {
        editor.text_rect.y += rows;
        editor.gutter_rect.y += rows;
        // Selection spans name a display row and are resolved against the text
        // rect when they are painted, so they move with it. The cursors are
        // already grid points and do not.
        for cursor in &mut editor.secondary_cursors {
            cursor.row += rows;
        }
    }
    if let Some(cursor) = snapshot.cursor.as_mut() {
        cursor.row += rows;
    }
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
        cursor,
        cursor_shape,
        ..Default::default()
    }
}

/// The pane's items along the top, and where the strip has to start so the
/// active tab is on screen (SPEC §24.7).
fn tab_strip(pane: &Entity<Pane>, columns: u16, window: &Window, cx: &App) -> Option<TabStripView> {
    let items = pane.read(cx).items().cloned().collect::<Vec<_>>();
    if items.is_empty() {
        return None;
    }

    // `tab_details` computes exactly the detail level each tab needs, so two
    // files called `mod.rs` grow a directory in their labels and nothing else
    // does.
    let details = workspace::pane::tab_details(&items, window, cx);
    let tabs = items
        .iter()
        .enumerate()
        .map(|(index, item)| TabView {
            label: item
                .tab_content_text(details.get(index).copied().unwrap_or(0), cx)
                .to_string(),
            modified: item.is_dirty(cx),
        })
        .collect::<Vec<_>>();

    let active = pane
        .read(cx)
        .active_item_index()
        .min(tabs.len().saturating_sub(1));
    let colors = cx.theme().colors();
    Some(TabStripView {
        first: first_visible_tab(&tabs, active, columns),
        tabs,
        active,
        active_background: Some(colors.tab_active_background),
        background: Some(colors.tab_inactive_background),
        active_foreground: Some(colors.text),
        foreground: Some(colors.text_muted),
        separator: Some(colors.border),
    })
}

/// The cells one tab occupies: a space either side of the label, and two more
/// for the modified marker. The renderer lays tabs out to exactly this, so the
/// scroll position computed here and the strip painted there cannot disagree.
pub fn tab_cells(tab: &TabView) -> u16 {
    let label = text_cells(&tab.label).min(u32::from(u16::MAX)) as u16;
    label
        .saturating_add(2)
        .saturating_add(if tab.modified { 2 } else { 0 })
}

/// The leftmost tab that leaves the active one on screen. Eliding the middle
/// would keep two tabs the user is not looking at and hide the one they are, so
/// the strip scrolls instead (SPEC §24.7).
fn first_visible_tab(tabs: &[TabView], active: usize, columns: u16) -> usize {
    let mut first = 0;
    while first < active {
        let width: u16 = tabs
            .get(first..=active)
            .unwrap_or_default()
            .iter()
            .map(tab_cells)
            // One separator cell between each pair.
            .fold(0u16, |total, cells| total.saturating_add(cells))
            .saturating_add((active - first) as u16);
        if width <= columns {
            break;
        }
        first += 1;
    }
    first
}

/// Wraps what the language server said into a panel and decides where it goes:
/// below the cursor when the rows are there, above it when they are not
/// (SPEC §24.8).
fn place_hover(
    contents: Vec<HoverContent>,
    snapshot: &ViewSnapshot,
    cx: &App,
) -> Option<HoverView> {
    if contents.is_empty() {
        return None;
    }
    let text_rect = snapshot.editor.as_ref()?.text_rect;
    let cursor = snapshot.cursor?;
    // The rail and the space after it.
    let width = text_rect.width.max(3);
    let inner = usize::from(width - 2);

    let mut blocks = Vec::new();
    let mut total = 0u16;
    for (index, content) in contents.into_iter().enumerate() {
        let mut lines = Vec::new();
        // A blank railed row between blocks, so a diagnostic and the
        // documentation under it are never read as one message.
        if index > 0 {
            lines.push(String::new());
        }
        lines.extend(wrap_to_cells(&content.text, inner));
        total = total.saturating_add(lines.len().min(usize::from(u16::MAX)) as u16);
        blocks.push(HoverBlock {
            rail: content.rail,
            lines,
        });
    }
    if total == 0 {
        return None;
    }

    let bottom = text_rect.y.saturating_add(text_rect.height);
    let below = bottom.saturating_sub(cursor.row.saturating_add(1));
    let above = cursor.row.saturating_sub(text_rect.y);
    let (y, height) = if total <= below {
        (cursor.row + 1, total)
    } else if total <= above {
        (cursor.row - total, total)
    } else if below >= above {
        (cursor.row.saturating_add(1), below)
    } else {
        (text_rect.y, above)
    };
    if height == 0 {
        return None;
    }

    Some(HoverView {
        rect: CellRect::new(text_rect.x, y, width, height),
        background: Some(cx.theme().colors().elevated_surface_background),
        foreground: Some(cx.theme().colors().text),
        blocks,
    })
}

/// Breaks text at `width` cells, at a space where there is one and mid-word
/// where there is not. Explicit newlines are honoured first, so a message that
/// arrived with its own line structure keeps it.
fn wrap_to_cells(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut cells = 0usize;
        for word in paragraph.split(' ') {
            let word_cells = text_cells(word) as usize;
            if !line.is_empty() && cells + 1 + word_cells > width {
                lines.push(std::mem::take(&mut line));
                cells = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                cells += 1;
            }
            if word_cells > width {
                for cluster in word.graphemes(true) {
                    let cluster_cells = cluster_cells(cluster) as usize;
                    if cells + cluster_cells > width {
                        lines.push(std::mem::take(&mut line));
                        cells = 0;
                    }
                    line.push_str(cluster);
                    cells += cluster_cells;
                }
                continue;
            }
            line.push_str(word);
            cells += word_cells;
        }
        lines.push(line);
    }
    lines
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
    // Gates the fold chevron (SPEC §11 step 1): Zed's own reported width for
    // the fold column, rather than a `ted`-side guess, so a gutter Zed left no
    // room in (e.g. `gutter.folds = false`, SPEC's M3 slice) never gets one.
    let fold_gutter_cells = cells_ceil(gutter.fold_area_width(), CELL_WIDTH);
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

    // Two things vim's visual modes need are kept outside the selections
    // themselves, and reading the selections alone gets both wrong.
    let line_mode = editor.selections.line_mode();
    let offset_cursor = vim::mode(editor, cx).is_some_and(|mode| mode.has_selection());
    let newest = editor.selections.newest_display(display);
    let (_, newest_head) = rendered_selection(
        newest.start..newest.end,
        newest.reversed,
        display,
        line_mode,
        offset_cursor,
    );
    let cursor_display_row = newest_head.row().0;

    let end_row = first_row
        .saturating_add(visible)
        .min(last_display_row.saturating_add(1));

    let colors = cx.theme().colors().clone();
    // The default fold widget's own colours (`FoldPlaceholder::fold_element`,
    // `display_map/fold_map.rs`), reused for every placeholder `ted` draws in
    // text rather than a widget, so a fold and an unrenderable block read as
    // the same kind of "there is something here `ted` collapsed" marker.
    let placeholder_style = SpanStyle {
        foreground: Some(colors.text_placeholder),
        background: Some(colors.ghost_element_background),
        ..Default::default()
    };
    let built_rows = rows_from_chunks(
        &editor_snapshot,
        first_row..end_row,
        &style,
        placeholder_style,
    );

    // `blocks_in_range` only reports a block's *first* row, so the query has to
    // start well before the visible window to still find the block a
    // continuation row belongs to. `BLOCK_LOOKBACK_ROWS` comfortably covers
    // known header heights (`FILE_HEADER_HEIGHT`, `MULTI_BUFFER_EXCERPT_HEADER_HEIGHT`)
    // and ordinary custom blocks without scanning the whole file.
    const BLOCK_LOOKBACK_ROWS: u32 = 32;
    let blocks: HashMap<u32, &Block> = display
        .blocks_in_range(
            DisplayRow(first_row.saturating_sub(BLOCK_LOOKBACK_ROWS))..DisplayRow(end_row),
        )
        .map(|(row, block)| (row.0, block))
        .collect();
    let buffer_snapshot = display.buffer_snapshot();
    // A plain reborrow, so the loop below never needs to reborrow `cx` itself
    // on every iteration just to read the theme.
    let app_cx: &App = cx;

    let mut row_infos = display.row_infos(DisplayRow(first_row));
    let mut rows_view = Vec::new();
    for (offset, (text, spans)) in built_rows.into_iter().enumerate() {
        let display_row = first_row.saturating_add(offset as u32);
        let info = row_infos.next().unwrap_or_default();
        let mut row = RowView::new(display_row, text);
        row.spans = spans;

        let is_block_line = display.is_block_line(DisplayRow(display_row));
        let mut background = None;
        if is_block_line {
            match blocks.get(&display_row) {
                Some(block) => match block_label(block, buffer_snapshot, app_cx, &colors) {
                    Some((kind, label, block_style)) => {
                        row.kind = kind;
                        row.byte_to_cell = byte_to_cell_table(&label);
                        row.spans = vec![StyledSpan {
                            range: 0..label.len(),
                            style: block_style,
                        }];
                        row.text = label;
                        background = block_style.background;
                    }
                    // A `Block::Spacer`: genuinely blank vertical space, not a
                    // degraded projection, so it gets no label and no tint.
                    None => row.kind = RowKind::Block,
                },
                // A continuation row of a block whose first row is further
                // back than `BLOCK_LOOKBACK_ROWS`, or taller than it. Still
                // tinted, so it never reads as an ordinary blank buffer line
                // even without a label to put on it.
                None => {
                    row.kind = RowKind::Block;
                    background = Some(colors.ghost_element_background);
                }
            }
        } else {
            row.kind = RowKind::Text;
        }

        row.soft_wrap_indent = display
            .soft_wrap_indent(DisplayRow(display_row))
            .unwrap_or(0)
            .min(u32::from(u16::MAX)) as u16;

        let diff = info.diff_status.map(|status| match status.kind {
            DiffHunkStatusKind::Added => DiffMarker::Added,
            DiffHunkStatusKind::Modified => DiffMarker::Modified,
            DiffHunkStatusKind::Deleted => DiffMarker::Deleted,
        });
        // Read from the status colours already threaded through `EditorStyle`
        // (SPEC §11 step 1) rather than the dedicated `editor_diff_hunk_*`
        // theme fields Zed's own gutter uses, so this stays a plain function
        // of what `build_editor_view` already has in scope.
        let diff_colors = diff.map(|marker| match marker {
            DiffMarker::Added => (style.status.created, style.status.created_background),
            DiffMarker::Modified => (style.status.modified, style.status.modified_background),
            DiffMarker::Deleted => (style.status.deleted, style.status.deleted_background),
        });
        if background.is_none() {
            background = diff_colors.map(|(_, background)| background);
        }
        row.background = background;

        // Only a real buffer row can have a crease, and a block row's
        // placeholder already says everything `ted` can about it.
        //
        // `is_line_folded` has to be checked before falling back to
        // `crease_for_buffer_row`, not the other way round: for the common
        // case of an indent-derived crease (no explicit entry in
        // `crease_snapshot`), `crease_for_buffer_row`'s own fallback logic
        // skips itself once the row `is_line_folded` — it has nothing cheap to
        // re-derive from a buffer row whose content is hidden — so it goes
        // back to reporting `None` the moment the fold it describes succeeds.
        // Asking fold state directly is the only way the chevron survives the
        // fold it is showing.
        let crease = (!is_block_line)
            .then(|| info.multibuffer_row)
            .flatten()
            .and_then(|buffer_row| {
                if display.is_line_folded(buffer_row) {
                    Some(CreaseState::Folded)
                } else {
                    display
                        .crease_for_buffer_row(buffer_row)
                        .map(|_| CreaseState::Foldable)
                }
            });

        row.gutter = GutterView {
            line_number: show_line_numbers
                .then(|| line_number_for(&info, display_row, cursor_display_row, relative_numbers))
                .flatten(),
            diff,
            diff_foreground: diff_colors.map(|(foreground, _)| foreground),
            crease,
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
    let mut primary_cursor = None;

    for selection in editor.selections.all_display(display) {
        let (range, head) = rendered_selection(
            selection.start..selection.end,
            selection.reversed,
            display,
            line_mode,
            offset_cursor,
        );
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
    let buffer_point = newest_head.to_point(display);
    let primary_position = Some((buffer_point.row + 1, buffer_point.column + 1));

    let built = BuiltEditor {
        editor: EditorView {
            text_rect,
            gutter_rect,
            fold_gutter_cells,
            scroll_columns: scroll.x.max(0.0) as u16,
            rows: rows_view,
            selections,
            secondary_cursors,
            background: Some(style.background),
            foreground: Some(style.text.color),
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
    // The fold placeholder's own colours: `HighlightedChunk::style` is `None`
    // for a fold's "⋯" text (`fold_map.rs` pushes the placeholder chunk with
    // `..Default::default()`), because GUI Zed colours it by rendering
    // `FoldPlaceholder::render` as a widget instead. `ted` renders the
    // placeholder as text (SPEC §5.4), so it has to supply that colour itself.
    placeholder_style: SpanStyle,
) -> Vec<(String, Vec<StyledSpan>)> {
    let mut built = vec![(String::new(), Vec::new()); rows.end.saturating_sub(rows.start) as usize];
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
        let is_fold_placeholder = matches!(
            &chunk.replacement,
            Some(ChunkReplacement::Renderer(renderer))
                if matches!(renderer.id, ChunkRendererId::Fold(_))
        );
        let chunk_style = if is_fold_placeholder {
            placeholder_style
        } else {
            span_style(chunk.style, style)
        };
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

    let mut foreground = highlight.color.unwrap_or(editor_style.text.color);
    // What Zed does with `Diagnostic::is_unnecessary`: dead code fades rather
    // than acquiring a marker of its own (SPEC §24.8). The palette composites
    // alpha against the editor background, so fading the alpha is all a terminal
    // needs to arrive at the same colour.
    if let Some(fade) = highlight.fade_out {
        foreground.fade_out(fade);
    }

    SpanStyle {
        foreground: Some(foreground),
        background: highlight.background_color,
        bold: highlight
            .font_weight
            .is_some_and(|weight| weight >= FontWeight::BOLD),
        italic: highlight.font_style == Some(FontStyle::Italic),
        // The colour is the whole point: severity reaches the buffer as the
        // underline's colour and nothing else, so an error and a warning are
        // told apart here or nowhere (SPEC §24.8).
        underline: highlight.underline.map(|underline| Underline {
            color: underline.color,
        }),
        strikethrough: highlight.strikethrough.is_some(),
    }
}

/// The label and style `ted` draws for a block row, since `highlighted_chunks`
/// gives every block row's text as nothing but the newlines that hold its
/// height (`BlockChunks::next`, `block_map.rs`) — the real content is an
/// `AnyElement` GUI Zed builds from the block's `render` closure, which `ted`
/// cannot execute.
///
/// `None` only for `Block::Spacer`: genuine blank vertical space rather than
/// something `ted` failed to project, so it gets no placeholder at all.
fn block_label(
    block: &Block,
    buffer_snapshot: &MultiBufferSnapshot,
    cx: &App,
    colors: &ThemeColors,
) -> Option<(RowKind, String, SpanStyle)> {
    let header_style = |foreground| SpanStyle {
        foreground: Some(foreground),
        background: Some(colors.editor_subheader_background),
        ..Default::default()
    };
    match block {
        Block::BufferHeader { excerpt, .. }
        | Block::FoldedBuffer {
            first_excerpt: excerpt,
            ..
        } => {
            let path = excerpt
                .buffer(buffer_snapshot)
                .resolve_file_path(true, cx)
                .unwrap_or_else(|| "untitled".to_owned());
            Some((RowKind::BufferHeader, path, header_style(colors.text)))
        }
        Block::ExcerptBoundary { .. } => Some((
            RowKind::ExcerptHeader,
            "⋯".to_owned(),
            header_style(colors.text_muted),
        )),
        Block::Custom(_) => Some((
            RowKind::Block,
            "‹block ted cannot render›".to_owned(),
            SpanStyle {
                foreground: Some(colors.text_placeholder),
                background: Some(colors.ghost_element_background),
                ..Default::default()
            },
        )),
        Block::Spacer { .. } => None,
    }
}

/// What a selection actually looks like: the range it covers, and the display
/// point its cursor sits on.
///
/// Neither is the selection as vim stores it, and `editor`'s own element derives
/// both the same way in `SelectionLayout::new`. A whole-line selection is a flag
/// on the collection rather than an expanded range, so `shift-v` reaching here
/// unexpanded would paint as an ordinary character selection; and a forward
/// selection's head is its *exclusive* end, one position past the block cursor,
/// so `v` would move the cursor a cell to the right of where vim has it.
fn rendered_selection(
    mut range: Range<DisplayPoint>,
    reversed: bool,
    display: &DisplaySnapshot,
    line_mode: bool,
    offset_cursor: bool,
) -> (Range<DisplayPoint>, DisplayPoint) {
    let mut head = if reversed { range.start } else { range.end };
    if line_mode {
        let lines =
            display.expand_to_line(range.start.to_point(display)..range.end.to_point(display));
        range = lines.start.to_display_point(display)..lines.end.to_display_point(display);
    }

    if offset_cursor && !range.is_empty() && !reversed {
        if head.column() > 0 {
            // `clip_point` rather than a bare subtraction: a column is a byte
            // offset, and one byte back from a multi-byte grapheme is not a
            // position.
            head = display.clip_point(
                DisplayPoint::new(head.row(), head.column() - 1),
                editor::Bias::Left,
            );
        } else if head.row().0 > 0 {
            let previous = DisplayRow(head.row().0 - 1);
            head = display.clip_point(
                DisplayPoint::new(previous, display.line_len(previous)),
                editor::Bias::Left,
            );
            // The clip may have moved the head up further than one row, across
            // a block, so the range follows it rather than the row it started
            // from.
            range.end = DisplayPoint::new(DisplayRow(head.row().0 + 1), 0);
        }
    }

    (range, head)
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
        // A multi-row selection can straddle a block or header row in display
        // coordinates without the buffer selection ever covering it — the
        // display map only puts those rows between real buffer positions, not
        // on one — so painting a selection span there would make an
        // unrenderable block or a fold's own text look selectable.
        if !row.kind.is_selectable() {
            continue;
        }
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
    let row = rows.get(offset)?;
    // Belt and braces alongside `selection_cells`' guard: a cursor's display
    // point is always clipped to a real buffer position by the display map, so
    // this should never actually be a block row, but landing a block cursor
    // one cell into placeholder text it does not describe would be a worse
    // failure than simply not drawing one.
    if !row.kind.is_selectable() {
        return None;
    }
    let column = row.cell_for_byte(byte_column);
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
    use std::sync::Arc;

    use gpui::{AppContext as _, BorrowAppContext as _, HeadlessAppContext, WindowHandle};
    use language::Buffer;
    use multi_buffer::{MultiBufferOffset, MultiBufferRow};
    use settings::SettingsStore;

    use crate::cell::grid_size;
    use crate::text_system::CellTextSystem;

    /// Enables folds regardless of what `bootstrap.rs` pins for the real
    /// binary, so this covers the chevron-and-placeholder path SPEC's M3
    /// slice adds, independent of that unrelated default.
    const REAL_EDITOR_SETTINGS: &str = r#"{
        "buffer_font_size": 16,
        "buffer_line_height": { "custom": 1.0 },
        "soft_wrap": "editor_width",
        "gutter": { "folds": true }
    }"#;

    /// A minimal real `Editor`, for the parts of this slice — fold placeholders,
    /// inlay hints, cursor placement through both — that only a real display map
    /// produces. Deliberately smaller than `tests/editing_session.rs`'s `Session`:
    /// no vim, no keymaps, since nothing here dispatches a keystroke.
    struct RealEditor {
        editor: Entity<Editor>,
        window: WindowHandle<Editor>,
        cx: HeadlessAppContext,
    }

    impl RealEditor {
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
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    let result = store.set_user_settings(REAL_EDITOR_SETTINGS, cx);
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

            let mut session = Self { editor, window, cx };
            // Twice: the first draw computes and installs the wrap width, the
            // second lays out against the rewrapped display map (SPEC §10.2's
            // "Ordering" — `Editor::style` and `last_bounds` need a real paint).
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

        fn update<R>(
            &mut self,
            f: impl FnOnce(&mut Editor, &mut Window, &mut gpui::Context<Editor>) -> R,
        ) -> R {
            let editor = self.editor.clone();
            let result = self
                .cx
                .update_window(self.window.into(), |_, window, cx| {
                    editor.update(cx, |editor, cx| f(editor, window, cx))
                })
                .expect("failed to update the editor");
            self.draw();
            result
        }

        fn snapshot(&mut self, columns: u16, rows: u16) -> ViewSnapshot {
            let editor = self.editor.clone();
            self.cx
                .update_window(self.window.into(), |_, window, cx| {
                    for_editor(&editor, columns, rows, 0, window, cx)
                })
                .expect("failed to build a snapshot")
        }
    }

    /// SPEC's M3 slice: whether the display map already gives `ted` the fold's
    /// placeholder text through `highlighted_chunks`, and whether the gutter's
    /// crease reporting flips from foldable to folded across the fold.
    #[test]
    fn folding_a_row_turns_its_chevron_and_swaps_in_the_placeholder() {
        let mut session = RealEditor::open(40, 8, "fn main() {\n    let x = 1;\n}\n");

        let before = session.snapshot(40, 8);
        let row0 = &before.editor.as_ref().expect("no editor view").rows[0];
        assert_eq!(
            row0.gutter.crease,
            Some(CreaseState::Foldable),
            "an indented block under `fn main() {{` should be foldable by default"
        );
        assert!(
            !row0.text.contains('⋯'),
            "nothing is folded yet: {:?}",
            row0.text
        );

        session.update(|editor, window, cx| editor.fold_at(MultiBufferRow(0), window, cx));

        let after = session.snapshot(40, 8);
        let editor_view = after.editor.as_ref().expect("no editor view");
        let row0 = &editor_view.rows[0];
        assert_eq!(row0.gutter.crease, Some(CreaseState::Folded));
        assert!(row0.text.contains('⋯'), "no placeholder in {:?}", row0.text);

        // The placeholder's own span carries the fold widget's colours
        // (`FoldPlaceholder::fold_element`'s `text_placeholder` /
        // `ghost_element_background`), not the plain text colour — this is the
        // part `highlighted_chunks` leaves to `ted` (`HighlightedChunk::style`
        // is `None` for a fold's placeholder chunk).
        let ellipsis_byte = row0.text.find('⋯').expect("no placeholder byte offset");
        let placeholder_span = row0
            .spans
            .iter()
            .find(|span| span.range.contains(&ellipsis_byte))
            .expect("no span covers the placeholder");
        assert!(placeholder_span.style.background.is_some());
        assert_ne!(
            placeholder_span.style.foreground,
            row0.spans
                .first()
                .map(|span| span.style.foreground)
                .unwrap_or_default(),
            "the placeholder should not read as plain buffer text"
        );
    }

    /// SPEC's M3 slice: an inlay's text arrives inline through the same chunk
    /// stream as everything else, so `byte_to_cell` needs no inlay-specific
    /// code — and the cursor, placed from `DisplayPoint`s Zed already adjusted
    /// for the inlay, has to land past it rather than inside or before it.
    #[test]
    fn an_inlay_shifts_the_cells_after_it_and_the_cursor_lands_past_it() {
        let mut session = RealEditor::open(40, 6, "let x = 1;\n");

        // Right after "x" (byte offset 5), same as an LSP type-hint inlay.
        let anchor = session.update(|editor, _window, cx| {
            editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_before(MultiBufferOffset(5))
        });
        session.update(|editor, _window, cx| {
            editor.splice_inlays(&[], vec![editor::Inlay::mock_hint(0, anchor, ": i32")], cx);
        });

        let snapshot = session.snapshot(40, 6);
        let row0 = &snapshot.editor.as_ref().expect("no editor view").rows[0];
        assert_eq!(row0.text, "let x: i32 = 1;");

        // The inlay's own span is coloured apart from the surrounding buffer
        // text (SPEC: Zed styles inlays with the theme's `hint` colour).
        let inlay_byte = row0.text.find(": i32").expect("inlay text missing");
        let inlay_span = row0
            .spans
            .iter()
            .find(|span| span.range.contains(&inlay_byte))
            .expect("no span covers the inlay");
        let plain_span = row0
            .spans
            .iter()
            .find(|span| span.range.contains(&0))
            .expect("no span covers the row's start");
        assert_ne!(inlay_span.style.foreground, plain_span.style.foreground);

        // Move the cursor to buffer offset 5 — right after "x", the same
        // position the inlay is anchored to — and check the display column,
        // which Zed derives from `DisplayPoint` rather than anything `ted`
        // computes, lands past the inlay's own cells rather than inside them.
        session.update(|editor, window, cx| {
            editor.change_selections(
                editor::SelectionEffects::default(),
                window,
                cx,
                |selections| {
                    selections.select_ranges(vec![MultiBufferOffset(5)..MultiBufferOffset(5)]);
                },
            );
        });

        let snapshot = session.snapshot(40, 6);
        let text_rect = snapshot.editor.as_ref().expect("no editor view").text_rect;
        let cursor = snapshot.cursor.expect("no cursor in the snapshot");
        let expected_column =
            text_rect.x + text_cells("let x: i32").min(u32::from(u16::MAX)) as u16;
        assert_eq!(cursor.column, expected_column);
    }

    /// SPEC's M3 slice: a `Block::Custom` — the kind diagnostics, git blame and
    /// code lens all build — carries only an opaque `render` closure `ted`
    /// cannot execute, so it has to degrade to a named placeholder rather than
    /// the blank line `highlighted_chunks` gives every block row.
    #[test]
    fn a_custom_block_degrades_to_a_named_placeholder_with_no_line_number() {
        let mut session = RealEditor::open(40, 8, "one\ntwo\nthree\n");
        session.update(|editor, _window, cx| {
            let anchor = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_before(MultiBufferOffset(0));
            let block = editor::display_map::BlockProperties {
                placement: editor::display_map::BlockPlacement::Below(anchor),
                height: Some(1),
                style: editor::display_map::BlockStyle::Fixed,
                render: Arc::new(|_| {
                    use gpui::IntoElement as _;
                    gpui::Empty.into_any_element()
                }),
                priority: 0,
            };
            editor.insert_blocks(vec![block], None, cx);
        });

        let snapshot = session.snapshot(40, 8);
        let rows = &snapshot.editor.as_ref().expect("no editor view").rows;
        let block_row = rows
            .iter()
            .find(|row| row.kind == RowKind::Block)
            .expect("no block row in the projection");
        assert!(!block_row.text.is_empty(), "the block row was left blank");
        assert_eq!(block_row.gutter.line_number, None);
    }

    /// SPEC §24.7: a tab strip moves the editor down the grid, and everything
    /// the projection has already placed in grid coordinates has to move with
    /// it. Selection spans name a display row and are resolved when they are
    /// painted, so they are carried already; both kinds of cursor are not.
    #[test]
    fn a_tab_strip_moves_every_cursor_down_with_the_text() {
        let mut snapshot = ViewSnapshot {
            editor: Some(EditorView {
                text_rect: CellRect::new(4, 0, 20, 6),
                gutter_rect: CellRect::new(0, 0, 4, 6),
                secondary_cursors: vec![CellPoint { column: 6, row: 2 }],
                ..Default::default()
            }),
            cursor: Some(CellPoint { column: 5, row: 1 }),
            ..Default::default()
        };

        shift_down(&mut snapshot, 1);

        let editor = snapshot.editor.expect("no editor view");
        assert_eq!(editor.text_rect.y, 1);
        assert_eq!(editor.gutter_rect.y, 1);
        assert_eq!(
            editor.secondary_cursors,
            vec![CellPoint { column: 6, row: 3 }],
            "a secondary cursor stayed on the row the tab strip took"
        );
        assert_eq!(snapshot.cursor, Some(CellPoint { column: 5, row: 2 }));
    }

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

    fn tab(label: &str, modified: bool) -> TabView {
        TabView {
            label: label.to_owned(),
            modified,
        }
    }

    #[test]
    fn a_tab_is_its_label_plus_the_space_around_it_and_its_marker() {
        assert_eq!(tab_cells(&tab("main.rs", false)), 9);
        assert_eq!(tab_cells(&tab("main.rs", true)), 11);
    }

    #[test]
    fn the_strip_scrolls_only_as_far_as_the_active_tab_needs() {
        let tabs: Vec<TabView> = (0..5)
            .map(|index| tab(&format!("f{index}.rs"), false))
            .collect();
        // Each tab is 8 cells, and a separator sits between each pair.
        assert_eq!(first_visible_tab(&tabs, 0, 80), 0);
        assert_eq!(first_visible_tab(&tabs, 4, 80), 0);
        // Room for two tabs and the separator between them, and no more.
        assert_eq!(first_visible_tab(&tabs, 4, 17), 3);
        assert_eq!(first_visible_tab(&tabs, 2, 17), 1);
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_keeps_the_lines_it_was_given() {
        assert_eq!(
            wrap_to_cells("this function takes 5 arguments", 20),
            vec!["this function takes", "5 arguments"]
        );
        assert_eq!(
            wrap_to_cells("error E0061\nrust-analyzer", 40),
            vec!["error E0061", "rust-analyzer"]
        );
    }

    #[test]
    fn a_word_wider_than_the_panel_is_broken_rather_than_clipped() {
        assert_eq!(
            wrap_to_cells("std::collections::HashMap", 10),
            vec!["std::colle", "ctions::Ha", "shMap"]
        );
        // Cells, not characters: a wide grapheme takes two of them.
        assert_eq!(wrap_to_cells("日本語", 4), vec!["日本", "語"]);
    }

    fn text_row(display_row: u32, text: &str) -> RowView {
        RowView::new(display_row, text.to_owned())
    }

    /// SPEC's M3 slice: a multi-row selection can straddle a block or header
    /// row in display coordinates without the buffer selection ever covering
    /// it, so a block row's placeholder must not look selectable even when it
    /// falls inside a selection's row range.
    #[test]
    fn a_selection_spanning_a_block_row_does_not_paint_over_its_placeholder() {
        let mut header = text_row(1, "src/main.rs");
        header.kind = RowKind::BufferHeader;
        let rows = vec![text_row(0, "one"), header, text_row(2, "two")];

        let range = DisplayPoint::new(DisplayRow(0), 0)..DisplayPoint::new(DisplayRow(2), 3);
        let spans = selection_cells(&rows, &range, 0..3);

        let block_row_spans = spans.iter().filter(|(row, ..)| *row == 1).count();
        assert_eq!(
            block_row_spans, 0,
            "the header row got a selection span: {spans:?}"
        );
        // The ordinary text rows either side are untouched by the guard.
        assert!(spans.iter().any(|(row, ..)| *row == 0));
        assert!(spans.iter().any(|(row, ..)| *row == 2));
    }

    /// A fold's placeholder ("⋯") is `RowKind::Text`, not a block — folded
    /// lines stay selectable and land-able in real Zed, so the guard must not
    /// catch them too.
    #[test]
    fn a_folded_lines_placeholder_stays_selectable() {
        let rows = vec![text_row(0, "⋯")];
        let range = DisplayPoint::new(DisplayRow(0), 0)..DisplayPoint::new(DisplayRow(0), 3);
        let spans = selection_cells(&rows, &range, 0..1);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn a_cursor_cannot_land_on_a_block_row() {
        let mut block = text_row(0, "‹block ted cannot render›");
        block.kind = RowKind::Block;
        let rows = vec![block];
        let text_rect = CellRect::new(0, 0, 40, 5);
        assert_eq!(cell_for(&rows, 0, 0, &text_rect), None);
    }

    #[test]
    fn a_cursor_lands_normally_on_an_ordinary_text_row() {
        let rows = vec![text_row(0, "abc")];
        let text_rect = CellRect::new(4, 0, 40, 5);
        assert_eq!(
            cell_for(&rows, 0, 1, &text_rect),
            Some(CellPoint { column: 5, row: 0 })
        );
    }
}
