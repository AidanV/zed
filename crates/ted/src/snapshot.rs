//! The backend-to-frontend projection (SPEC §10): one struct, rebuilt each
//! frame from GPUI-side reads, containing no GPUI handles — only plain data.
//! That constraint is what keeps an out-of-process frontend possible later, and
//! it makes the renderer testable without a terminal.

use std::collections::HashMap;
use std::ops::Range;

use buffer_diff::{DiffHunkStatus, DiffHunkStatusKind};
use editor::display_map::{
    Block, ChunkRendererId, ChunkReplacement, DisplayRow, DisplaySnapshot, ToDisplayPoint as _,
};
use editor::{Bias, DisplayPoint, Editor};
use gpui::{App, Entity, FontStyle, FontWeight, Hsla, Pixels, Window};
use language::LanguageAwareStyling;
use multi_buffer::{MultiBufferPoint, MultiBufferRow, MultiBufferSnapshot, RowInfo};
use theme::{ActiveTheme as _, ThemeColors};
use unicode_segmentation::UnicodeSegmentation as _;
use workspace::{Pane, Workspace};

use crate::cell::{CELL_HEIGHT, CELL_WIDTH, cluster_cells, text_cells};

/// How tall the completions box may grow, and how wide each of its columns may
/// get (SPEC §24.9). The height is a function of the entry count alone, so
/// these are the only things that bound it.
const MENU_MAX_ROWS: u16 = 12;
const MENU_MIN_WIDTH: u16 = 20;
const MENU_MAX_LABEL_CELLS: u16 = 40;
const MENU_MAX_SIGNATURE_CELLS: u16 = 40;

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
    /// The cells the GPUI window occupies — the grid minus the rows `ted` keeps
    /// for itself (SPEC §10.2) — painted with the editor's ground before
    /// anything else.
    ///
    /// The pane tree does not always claim all of them: a workspace lays a
    /// divider or a collapsed dock out around the center group, and those cells
    /// are still inside the window. Filling the window first is what keeps them
    /// from showing the real terminal's background through the middle of an
    /// opaque session (SPEC §25.1).
    pub window: CellRect,
    pub background: Option<Hsla>,
    /// The pane tree, one entry per leaf, each at the rect its own bounds
    /// reported (SPEC §25.1). One pane fills the grid; a split fills its share
    /// of it.
    pub panes: Vec<PaneView>,
    /// The terminal panel's grid, when the dock holding it is open
    /// (SPEC §25.5). Painted over the panes, because the dock's rows are rows
    /// the center group no longer has.
    pub terminal: Option<crate::terminal::TerminalPanelView>,
    pub status: StatusView,
    pub command_line: Option<CommandLineView>,
    /// A list painted *over* the editor — the finder or the switcher
    /// (SPEC §24.1). It reserves no rows, so the buffer behind it never
    /// relayouts while it is open.
    pub overlay: Option<OverlayView>,
    /// The railed panel `shift-k` opens (SPEC §24.8), painted over the editor
    /// like an overlay and dismissed by the next key.
    pub hover: Option<HoverView>,
    /// The language server's completions, or the code actions deployed from the
    /// gutter — one box, because they are one `editor` field (SPEC §24.9).
    pub menu: Option<MenuView>,
    pub prompt: Option<PromptView>,
    pub notifications: Vec<String>,
    /// Where to park the terminal's hardware cursor. SPEC §7 places the real
    /// cursor rather than drawing one, so the terminal blinks it and screen
    /// readers see it. It belongs to the active pane, or to the terminal panel
    /// while that has focus — a terminal has one, and it goes where the keyboard
    /// goes (SPEC §25.3, §25.5).
    pub cursor: Option<CellPoint>,
    pub cursor_shape: CursorShape,
}

impl ViewSnapshot {
    /// The pane the keyboard is in, which is what every editor-anchored surface
    /// means by "the editor" (SPEC §25.4).
    pub fn active_pane(&self) -> Option<&PaneView> {
        self.panes
            .iter()
            .find(|pane| pane.active)
            .or_else(|| self.panes.first())
    }

    pub fn editor(&self) -> Option<&EditorView> {
        self.active_pane()?.editor.as_ref()
    }

    pub fn editor_mut(&mut self) -> Option<&mut EditorView> {
        let index = self
            .panes
            .iter()
            .position(|pane| pane.active)
            .or(if self.panes.is_empty() { None } else { Some(0) })?;
        self.panes.get_mut(index)?.editor.as_mut()
    }
}

/// One leaf of the pane tree, at the rect the tree laid it out in
/// (SPEC §25.1).
///
/// A pane is what `ted` paints *between*: everything inside it — the strip, the
/// editor, the hint screen — is placed against this rect rather than against the
/// grid, which is the whole of what M4 changed about the projection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneView {
    pub rect: CellRect,
    /// The pane's items, in its own top row — the row Zed's layout leaves for it
    /// because `ted` replaced the tab bar with an element exactly a cell tall
    /// (SPEC §25.2). `None` for a pane with no items, which has no tab bar and
    /// so no row.
    pub tabs: Option<TabStripView>,
    pub editor: Option<EditorView>,
    /// The screen an empty pane sits on (SPEC §24.7), in this pane's rect.
    /// Mutually exclusive with `editor`: it is what is painted when this pane
    /// has no item to paint.
    pub hint: Option<HintView>,
    /// Whether the keyboard is here. The cursor says so already; the strip
    /// agrees with it, and nothing else is dimmed (SPEC §25.3).
    pub active: bool,
    /// Whether the pane's last column carries a rule, which it does whenever
    /// something is on the other side of it (SPEC §25.3).
    pub divider: bool,
    pub background: Option<Hsla>,
    pub divider_color: Option<Hsla>,
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

/// Text together with the styling of its own slices.
///
/// What a `RowView` is for the buffer, this is for everything `ted` paints over
/// it that is more than one colour: a completion's syntax-coloured label
/// (SPEC §24.9) and a rendered markdown line in the hover panel (SPEC §24.8).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyledText {
    pub text: String,
    /// Byte ranges within `text`, in order. Gaps take the surface's own style,
    /// so plain prose costs no spans at all.
    pub spans: Vec<StyledSpan>,
}

impl StyledText {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            spans: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cell_width(&self) -> u16 {
        text_cells(&self.text).min(u32::from(u16::MAX)) as u16
    }
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
    /// The background behind the marker glyph alone, which is where `ted` says
    /// whether the hunk is staged: the hunk's tint while it is unstaged, and
    /// `None` — nothing painted at all — once it is staged. The exception is a
    /// row inside an expanded hunk, which is tinted across its width, so a
    /// staged marker there needs the editor's own background to escape it
    /// rather than nothing (SPEC §11 step 1).
    pub diff_background: Option<Hsla>,
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

/// A row's diff marker, whether the hunk behind it is staged, and whether it
/// reached this row as content or as a summary.
///
/// Staged means `!has_secondary_hunk()` — the same predicate `DiffHunkDelegate`
/// paints a hunk as staged by (`editor/src/git.rs`), so a hunk half-staged or
/// mid-toggle counts as unstaged in `ted` exactly as it does in the GUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HunkMark {
    marker: DiffMarker,
    staged: bool,
    /// Whether this row is part of an *expanded* hunk, and so is the changed
    /// text itself rather than a line the change merely touched. Only these
    /// rows are tinted across their whole width.
    expanded: bool,
}

impl From<DiffHunkStatus> for HunkMark {
    /// From `RowInfo::diff_status`, which is set on the rows of an expanded
    /// hunk and nowhere else (SPEC §10.3) — so a mark built this way is
    /// expanded by construction.
    fn from(status: DiffHunkStatus) -> Self {
        Self {
            marker: match status.kind {
                DiffHunkStatusKind::Added => DiffMarker::Added,
                DiffHunkStatusKind::Modified => DiffMarker::Modified,
                DiffHunkStatusKind::Deleted => DiffMarker::Deleted,
            },
            staged: !status.has_secondary_hunk(),
            expanded: true,
        }
    }
}

impl HunkMark {
    /// The background for the marker cell alone: the hunk's tint while it is
    /// unstaged, and the editor's own background — or nothing at all, where
    /// nothing else is painted — once it is staged.
    ///
    /// So staging *removes* colour from under the marker rather than adding it,
    /// and a gutter with no tinted markers left in it is a file whose changes
    /// are all staged. A staged marker only needs a colour of its own when the
    /// row around it is tinted, which is the one case where emitting nothing
    /// would leave it wearing that tint.
    fn background(self, tint: Hsla, colors: &ThemeColors) -> Option<Hsla> {
        if !self.staged {
            return Some(tint);
        }
        self.expanded.then_some(colors.editor_background)
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

/// The pane's items along its own top row (SPEC §24.7, §25.2).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStripView {
    /// The row Zed's layout left for it at the top of the pane, which is where
    /// the strip is painted and how wide it may be (SPEC §25.2).
    pub rect: CellRect,
    pub tabs: Vec<TabView>,
    pub active: usize,
    /// The first tab painted. Overflow scrolls rather than eliding the middle,
    /// so the active tab is always on screen.
    pub first: usize,
    /// Whether this strip's pane holds the keyboard. An unfocused pane paints
    /// its active tab on the inactive ground, so the strips agree with the
    /// cursor about which pane is live (SPEC §25.3).
    pub focused: bool,
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
    pub lines: Vec<StyledText>,
}

/// The box the language server's completions and the code-action menu are both
/// painted into (SPEC §24.9).
///
/// One widget, because upstream they are one `editor` field: a menu that is open
/// is open in exactly one of the two shapes, and the difference between them is
/// which columns its rows carry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuView {
    /// Already resolved against the editor's text rect — below the anchor when
    /// the rows are there, above it when they are not.
    pub rect: CellRect,
    pub rows: Vec<MenuRow>,
    pub selected: Option<usize>,
    /// The first row painted, scrolled far enough that the selection is on
    /// screen.
    pub first: usize,
    /// Where the kind and signature columns start inside the box's text area.
    /// Computed once from the widest label rather than per row, so the columns
    /// line up down the box.
    pub kind_column: u16,
    pub signature_column: u16,
    pub background: Option<Hsla>,
    pub foreground: Option<Hsla>,
    pub selection_background: Option<Hsla>,
    pub border: Option<Hsla>,
}

/// One row of the box. Only `Entry` can be selected: a header and a rule are
/// things the menu says about its entries rather than entries themselves.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuRow {
    Entry {
        /// The label column, syntax-coloured by `styled_runs_for_code_label`.
        label: StyledText,
        /// Byte offsets within `label.text` the query matched, bolded on top of
        /// the colour: colour says what kind of thing an entry is, weight says
        /// why it matched.
        matched: Vec<usize>,
        /// A word — `fn`, `const`, `struct` — because a word needs no legend
        /// and the column is narrow either way. `None` for a code action, which
        /// has no kind to put in a column.
        kind: Option<String>,
        /// Everything past the label, already faded by the same call that
        /// coloured it. Empty for a code action.
        signature: StyledText,
    },
    /// A group's name, above the completions in it.
    Header(String),
    /// A rule between two groups.
    Divider,
}

impl MenuRow {
    pub fn is_selectable(&self) -> bool {
        matches!(self, MenuRow::Entry { .. })
    }
}

/// What `ted` paints where the editor would be when the pane has nothing open
/// (SPEC §24.7).
///
/// An empty pane is a state `ted` sits in rather than an exit condition, so the
/// screen names the ways out of it instead of leaving the user in front of a
/// blank grid wondering whether the session is still alive.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HintView {
    /// The empty pane's own rect (SPEC §25.1): one pane of a split can be empty
    /// while the other holds a file, so the screen is centred in the pane rather
    /// than in the grid.
    pub rect: CellRect,
    /// The wordmark above the rows, one line per cell row. Empty when the pane
    /// is not the shape for it, which the renderer decides.
    pub logo: &'static [&'static str],
    pub rows: Vec<HintRow>,
    /// The widest key, so the renderer can right-align the column without
    /// measuring the rows twice.
    pub key_cells: u16,
    pub background: Option<Hsla>,
    pub foreground: Option<Hsla>,
    /// The colour of the keys and of the wordmark — everything on the screen
    /// that is not prose.
    pub accent: Option<Hsla>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HintRow {
    pub key: String,
    pub description: String,
}

/// One thing the language server had to say about the cursor's position, before
/// it is wrapped into a panel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoverContent {
    pub rail: Option<Hsla>,
    pub lines: Vec<HoverLine>,
}

/// A line of the panel as the markdown renderer produced it, before it has been
/// broken to the panel's width (SPEC §24.8).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoverLine {
    pub text: StyledText,
    /// Prose wraps. A fenced code line and a box-drawn table row are laid out
    /// already and are clipped instead: breaking either would say something the
    /// document does not.
    pub wrap: bool,
    /// Cells to indent the continuation of a wrapped line by, so a list item's
    /// second line lines up under its text rather than under its bullet.
    pub indent: u16,
}

impl HoverLine {
    /// A line that wraps and continues at the left edge, which is what ordinary
    /// prose does and most of a doc comment is.
    pub fn prose(text: StyledText) -> Self {
        Self {
            text,
            wrap: true,
            indent: 0,
        }
    }
}

/// The sparse bar (SPEC §21/M3.5).
///
/// Three things stand on it — the mode, the project's diagnostic counts, and the
/// cursor's line and column — and everything else appears in the middle of the
/// row only while it is true, so the row is mostly empty most of the time.
///
/// No path: the tab strip (§24.7) names the file already, and a bar that
/// repeated it would spend its width saying twice what is said once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusView {
    pub mode: Option<String>,
    /// One-based line and column of the primary cursor, as a user expects to
    /// read them.
    pub position: Option<(u32, u32)>,
    /// The project's errors and warnings, coloured by severity. Project-wide
    /// rather than per-file, because that is what `Project::diagnostic_summary`
    /// returns and what Zed's own status bar shows (SPEC §24.8).
    pub diagnostics: Option<DiagnosticCounts>,
    /// Multi-keystroke bindings in flight, e.g. `d2` while `d2w` is being typed
    /// (SPEC §8.3).
    pub pending_keys: Option<String>,
    /// A wrap pass still running after a resize on a large file (SPEC §10.2).
    pub rewrapping: bool,
    /// Whether the pane has nothing open at all. The bar says so rather than
    /// going silent, because the hint screen behind it is a state `ted` sits in
    /// and not a failure (SPEC §24.7).
    pub empty: bool,
    /// Text that replaces the whole row: vim's `ctrl-g` location string until
    /// the next keystroke (SPEC §24.5), or a message `ted` has no other row to
    /// put anywhere. The only thing on the bar that elides when the grid is
    /// narrow.
    pub takeover: Option<String>,
    /// The bar's own surface. `None` falls back to inverting whatever is behind
    /// it, which is what the pre-theme frames — "terminal too small" — get.
    pub background: Option<Hsla>,
    pub foreground: Option<Hsla>,
}

/// What the project has to say about itself, in two numbers.
///
/// The colours travel with the counts because severity is carried by colour and
/// nothing else here, the same decision the buffer's underlines take
/// (SPEC §24.8) — so a bar that dropped them would show two numbers that mean
/// the same thing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DiagnosticCounts {
    pub errors: usize,
    pub warnings: usize,
    pub error_color: Option<Hsla>,
    pub warning_color: Option<Hsla>,
}

impl DiagnosticCounts {
    pub fn is_empty(self) -> bool {
        self.errors == 0 && self.warnings == 0
    }
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
    /// Whether this session has vim attached. The hint screen an empty pane
    /// sits on names the ways out of it (SPEC §24.7), and without vim there is
    /// no `:` line for three of them to be typed into (SPEC §24.5).
    pub vim: bool,
    /// The terminal panel's grid, already read from the emulator by
    /// [`crate::terminal`] (SPEC §25.5). It arrives built rather than being read
    /// here because none of it goes through the editor projection.
    pub terminal: Option<crate::terminal::TerminalPanelView>,
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
    let active_pane = frame.workspace.read(cx).active_pane().clone();
    let pane_entities = frame.workspace.read(cx).panes().to_vec();
    // The window is the grid minus the rows `ted` keeps for itself, and every
    // rect Zed reports is inside it (SPEC §10.2). With one pane that window is
    // also the pane, which is exactly the case `bounding_box_for_pane` answers
    // `None` for: no axis ever laid a lone root pane out.
    let window_rect = CellRect::new(
        0,
        0,
        frame.columns,
        frame.rows.saturating_sub(frame.reserved_rows),
    );

    let colors = cx.theme().colors().clone();
    let mut status = StatusView {
        pending_keys: pending_keys(window),
        diagnostics: Some(diagnostic_counts(frame.workspace, cx)),
        background: Some(colors.status_bar_background),
        foreground: Some(colors.text),
        ..Default::default()
    };

    let mut panes = Vec::with_capacity(pane_entities.len());
    let mut active_built = None;
    for pane in &pane_entities {
        let rect = frame
            .workspace
            .read(cx)
            .bounding_box_for_pane(pane)
            .map(|bounds| cell_rect(bounds, window_rect))
            .unwrap_or(window_rect);
        let active = pane == &active_pane;
        let built = pane_view(pane, rect, window_rect, active, frame.vim, window, cx);

        if active {
            status.empty = built.view.hint.is_some();
            status.mode = built.mode;
            status.position = built.position;
            status.rewrapping = built.rewrapping;
            status.takeover = built.takeover;
            active_built = Some((built.cursor, built.cursor_shape, built.menu));
        }
        panes.push(built.view);
    }

    let (cursor, cursor_shape, menu) = active_built.unwrap_or_default();
    let mut snapshot = ViewSnapshot {
        columns: frame.columns,
        rows: frame.rows,
        window: window_rect,
        background: Some(colors.editor_background),
        panes,
        terminal: frame.terminal,
        status,
        command_line: frame.command_line,
        overlay,
        menu,
        prompt: frame.prompt,
        notifications,
        cursor,
        cursor_shape,
        ..Default::default()
    };
    // The panel's own cursor wins while the panel has the keyboard: there is one
    // hardware cursor and it goes where the typing goes (SPEC §25.5).
    if let Some(terminal) = snapshot.terminal.as_ref()
        && terminal.focused
    {
        snapshot.cursor = terminal.cursor;
        snapshot.cursor_shape = terminal.cursor_shape;
    }
    snapshot.hover = place_hover(hover, &snapshot, cx);
    snapshot
}

fn pending_keys(window: &Window) -> Option<String> {
    window
        .pending_input_keystrokes()
        .filter(|keystrokes| !keystrokes.is_empty())
        .map(|keystrokes| {
            keystrokes
                .iter()
                .map(|keystroke| keystroke.unparse())
                .collect::<Vec<_>>()
                .join(" ")
        })
}

/// The project's errors and warnings, project-wide, which is both what the call
/// returns and what Zed's own status bar shows (SPEC §24.8).
fn diagnostic_counts(workspace: &Entity<Workspace>, cx: &App) -> DiagnosticCounts {
    let summary = workspace
        .read(cx)
        .project()
        .read(cx)
        .diagnostic_summary(false, cx);
    let status = cx.theme().status();
    DiagnosticCounts {
        errors: summary.error_count,
        warnings: summary.warning_count,
        error_color: Some(status.error),
        warning_color: Some(status.warning),
    }
}

/// The name of the thing the empty pane belongs to (SPEC §24.7).
///
/// Block glyphs rather than an outline of Zed's mark: the hints below are drawn
/// in box drawing already, and a mark at the same stroke weight would read as
/// another row of chrome rather than as the one thing on the screen that is not
/// instructions. The glyphs are there — `ted` targets terminals with a complete
/// font and has no ASCII tier (SPEC §24.1) — and it says `TED` rather than `ZED`
/// because that is the program the reader started.
const WORDMARK: [&str; 5] = [
    "██████ ██████ ██████",
    "  ██   ██     ██   ██",
    "  ██   █████  ██   ██",
    "  ██   ██     ██   ██",
    "  ██   ██████ ██████",
];

/// The ways out of an empty pane, named (SPEC §24.7).
///
/// The finder's keystroke is read back from the keymap rather than written down
/// here, because a hint that names a binding the user has rebound is worse than
/// no hint. Everything else is typed into the `:` line, which a `--no-vim`
/// session does not have at all (SPEC §24.5) — so without vim the screen offers
/// the finder and the one key that always works.
fn hint_screen(rect: CellRect, vim: bool, window: &Window, cx: &App) -> HintView {
    let mut rows = Vec::new();
    if let Some(keystrokes) = binding_for(&workspace::ToggleFileFinder::default(), window) {
        rows.push(HintRow {
            key: keystrokes,
            description: "find a file".to_owned(),
        });
    }
    if vim {
        rows.extend([
            HintRow {
                key: ":e <path>".to_owned(),
                description: "open a file by name".to_owned(),
            },
            HintRow {
                key: ":Explore".to_owned(),
                description: "browse the project".to_owned(),
            },
            HintRow {
                key: ":q".to_owned(),
                description: "quit ted".to_owned(),
            },
        ]);
    } else {
        rows.push(HintRow {
            key: "ctrl-c".to_owned(),
            description: "quit ted".to_owned(),
        });
    }

    let key_cells = rows
        .iter()
        .map(|row| text_cells(&row.key).min(u32::from(u16::MAX)) as u16)
        .max()
        .unwrap_or(0);
    let colors = cx.theme().colors();
    HintView {
        rect,
        logo: &WORDMARK,
        rows,
        key_cells,
        background: Some(colors.editor_background),
        foreground: Some(colors.text_muted),
        accent: Some(colors.text_accent),
    }
}

fn binding_for(action: &dyn gpui::Action, window: &Window) -> Option<String> {
    let binding = window.highest_precedence_binding_for_action(action)?;
    Some(
        binding
            .keystrokes()
            .iter()
            .map(|keystroke| keystroke.inner().unparse())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// A rect Zed reported, in cells (SPEC §5.3, §25.1).
///
/// Floored on both axes and then clipped to the window, so a pane whose share of
/// an odd column count ends half a cell into the next one is painted from the
/// cell it starts in rather than the one after it — the same half-cell the
/// single-pane case has always absorbed, and it lands the same way because the
/// pane on the other side had its own wrap width floored by the same arithmetic.
fn cell_rect(bounds: gpui::Bounds<gpui::Pixels>, window: CellRect) -> CellRect {
    let left = cells(bounds.origin.x, CELL_WIDTH).min(window.width);
    let top = cells(bounds.origin.y, CELL_HEIGHT).min(window.height);
    let width = cells(bounds.size.width, CELL_WIDTH).min(window.width - left);
    let height = cells(bounds.size.height, CELL_HEIGHT).min(window.height - top);
    CellRect::new(left, top, width, height)
}

/// One pane, and what the frame needs from it when it is the active one.
///
/// Everything after `view` is the active pane's alone — the one cursor, the one
/// menu, the one thing the status line is about — so an inactive pane fills them
/// in with nothing rather than with its own answers (SPEC §25.4).
#[derive(Default)]
struct BuiltPane {
    view: PaneView,
    cursor: Option<CellPoint>,
    cursor_shape: CursorShape,
    menu: Option<MenuView>,
    mode: Option<String>,
    position: Option<(u32, u32)>,
    rewrapping: bool,
    /// Vim's `ctrl-g` location string while it stands (SPEC §24.5).
    takeover: Option<String>,
}

/// The projection of one pane: its strip, its item, and the rule down its edge
/// (SPEC §25.1).
fn pane_view(
    pane: &Entity<Pane>,
    rect: CellRect,
    window_rect: CellRect,
    active: bool,
    vim: bool,
    window: &mut Window,
    cx: &mut App,
) -> BuiltPane {
    let colors = cx.theme().colors();
    // Something is on the other side of this edge — another pane, and never the
    // grid's own edge, because a rule there would separate the pane from
    // nothing (SPEC §25.3).
    let divider = rect.x.saturating_add(rect.width) < window_rect.width;
    let mut view = PaneView {
        rect,
        active,
        divider,
        background: Some(colors.editor_background),
        divider_color: Some(colors.border),
        ..Default::default()
    };

    let item = pane.read(cx).active_item();
    let Some(item) = item else {
        // No item means no tab bar, so Zed left no row for a strip and the hint
        // screen has the pane's whole rect (SPEC §25.2).
        view.hint = Some(hint_screen(rect, vim, window, cx));
        return BuiltPane {
            view,
            ..Default::default()
        };
    };

    view.tabs = tab_strip(pane, rect, active, window, cx);
    let Some(editor) = item.act_as::<Editor>(cx) else {
        // An item `ted` has no projection for. The pane keeps its strip, which
        // is what names the thing that is open.
        return BuiltPane {
            view,
            ..Default::default()
        };
    };

    // The rows and columns of the pane the item may be painted in: the strip's
    // row is Zed's, taken out of the pane above the item, and the rule's column
    // is `ted`'s, taken out below (SPEC §25.2, §25.3).
    let mut area = rect;
    if view.tabs.is_some() {
        area.y = area.y.saturating_add(1);
        area.height = area.height.saturating_sub(1);
    }
    if divider {
        area.width = area.width.saturating_sub(1);
    }

    let (built, cursor) =
        editor.update(cx, |editor, cx| build_editor_view(editor, area, window, cx));
    let mode = vim::mode(editor.read(cx), cx).map(|mode| mode.to_string());
    let cursor_shape = match mode.as_deref() {
        Some("INSERT") => CursorShape::Bar,
        Some("REPLACE") => CursorShape::Underline,
        _ => CursorShape::Block,
    };
    // Only the active pane's: the box is one surface over the grid, and an
    // inactive pane has nothing typing into it to have opened one.
    let menu = active
        .then(|| crate::menu::read(&editor, cx))
        .flatten()
        .and_then(|contents| place_menu(contents, &built.editor, cursor, cx));
    // Read here rather than in `build` because the label is the editor's, and
    // read *only* while the pane is active because the bar is one row about one
    // pane. `vim::Vim::action` already clears it on the next action outside a dot
    // replay, so the bar comes back on its own with no dismissal logic
    // (SPEC §24.5).
    let takeover = active
        .then(|| vim::status_label(editor.read(cx), cx))
        .flatten()
        .map(|label| label.to_string());

    view.editor = Some(built.editor);
    BuiltPane {
        view,
        // Only the pane with the keyboard in it gets the one hardware cursor
        // there is; an unfocused pane keeps its selections and loses its caret,
        // which is what vim does with an unfocused window (SPEC §25.3).
        cursor: active.then_some(cursor).flatten(),
        cursor_shape,
        menu,
        mode,
        position: built.primary_position,
        rewrapping: built.rewrapping,
        takeover,
    }
}

/// The projection of a single editor filling the grid, without a workspace
/// around it.
///
/// Tests go through here: everything the cell contract governs — rects, wrap,
/// cursor placement, gutter, selections, syntax spans — is decided under this
/// function, so exercising it needs no `Project`, `Client` or database.
pub fn for_editor(
    editor: &Entity<Editor>,
    columns: u16,
    rows: u16,
    reserved_rows: u16,
    window: &mut Window,
    cx: &mut App,
) -> ViewSnapshot {
    let area = CellRect::new(0, 0, columns, rows.saturating_sub(reserved_rows));
    let (view, cursor) =
        editor.update(cx, |editor, cx| build_editor_view(editor, area, window, cx));

    let mode = vim::mode(editor.read(cx), cx).map(|mode| mode.to_string());
    let cursor_shape = match mode.as_deref() {
        Some("INSERT") => CursorShape::Bar,
        Some("REPLACE") => CursorShape::Underline,
        _ => CursorShape::Block,
    };

    let menu = crate::menu::read(editor, cx)
        .and_then(|contents| place_menu(contents, &view.editor, cursor, cx));

    ViewSnapshot {
        columns,
        rows,
        window: area,
        panes: vec![PaneView {
            rect: area,
            editor: Some(view.editor),
            active: true,
            ..Default::default()
        }],
        status: StatusView {
            mode,
            position: view.primary_position,
            rewrapping: view.rewrapping,
            pending_keys: pending_keys(window),
            ..Default::default()
        },
        menu,
        cursor,
        cursor_shape,
        ..Default::default()
    }
}

/// Where the completions box goes: below the anchor when the rows are there,
/// above it when they are not, clamped to the editor's text rect (SPEC §24.9).
///
/// The anchor is the cursor for completions and the gutter's own row for code
/// actions, which is how actions deploy from the indicator rather than from
/// wherever the cursor happens to be sitting.
fn place_menu(
    contents: crate::menu::Contents,
    editor: &EditorView,
    cursor: Option<CellPoint>,
    cx: &App,
) -> Option<MenuView> {
    if contents.rows.is_empty() {
        return None;
    }
    let text_rect = editor.text_rect;
    let anchor = match contents.anchor {
        crate::menu::Anchor::Cursor => cursor?,
        crate::menu::Anchor::GutterRow(display_row) => {
            let offset = editor
                .rows
                .iter()
                .position(|row| row.display_row == display_row)?;
            CellPoint {
                column: text_rect.x,
                row: text_rect.y.saturating_add(u16::try_from(offset).ok()?),
            }
        }
    };

    let (label_cells, kind_cells, signature_cells) = menu_columns(&contents.rows);
    // One space between each pair of columns, and a border either side.
    let gaps = u16::from(kind_cells > 0) + u16::from(signature_cells > 0);
    let inner = label_cells
        .saturating_add(kind_cells)
        .saturating_add(signature_cells)
        .saturating_add(gaps)
        .max(1);
    let width = inner
        .saturating_add(2)
        .min(text_rect.width)
        .max(MENU_MIN_WIDTH.min(text_rect.width));
    if width < 3 {
        return None;
    }

    // The box's height follows the entry count alone, which is what keeping
    // documentation out of it buys: the box does not resize under the eye as
    // the selection moves (SPEC §24.9).
    let wanted = contents
        .rows
        .len()
        .min(usize::from(MENU_MAX_ROWS))
        .min(usize::from(u16::MAX)) as u16;
    let bottom = text_rect.y.saturating_add(text_rect.height);
    let below = bottom.saturating_sub(anchor.row.saturating_add(1));
    let above = anchor.row.saturating_sub(text_rect.y);
    let chrome = 2;
    let (y, height) = if wanted + chrome <= below {
        (anchor.row + 1, wanted + chrome)
    } else if wanted + chrome <= above {
        (anchor.row - (wanted + chrome), wanted + chrome)
    } else if below >= above {
        (anchor.row.saturating_add(1), below)
    } else {
        (text_rect.y, above)
    };
    if height <= chrome {
        return None;
    }
    let visible = usize::from(height - chrome);

    let x = anchor
        .column
        .min(text_rect.x + text_rect.width.saturating_sub(width));
    let colors = cx.theme().colors();
    Some(MenuView {
        rect: CellRect::new(x, y, width, height),
        first: first_visible_menu_row(contents.selected, contents.rows.len(), visible),
        rows: contents.rows,
        selected: contents.selected,
        kind_column: label_cells.saturating_add(1),
        signature_column: label_cells
            .saturating_add(kind_cells)
            .saturating_add(gaps.min(2)),
        background: Some(colors.elevated_surface_background),
        foreground: Some(colors.text),
        selection_background: Some(colors.element_selected),
        border: Some(colors.border),
    })
}

/// The width each of the three columns needs, capped so one absurd signature
/// cannot push the box past what the text rect can hold.
fn menu_columns(rows: &[MenuRow]) -> (u16, u16, u16) {
    let mut label = 0u16;
    let mut kind = 0u16;
    let mut signature = 0u16;
    for row in rows {
        match row {
            MenuRow::Entry {
                label: text,
                kind: word,
                signature: rest,
                ..
            } => {
                label = label.max(text.cell_width());
                kind = kind.max(
                    word.as_deref()
                        .map(|word| text_cells(word).min(u32::from(u16::MAX)) as u16)
                        .unwrap_or(0),
                );
                signature = signature.max(rest.cell_width());
            }
            MenuRow::Header(text) => {
                label = label.max(text_cells(text).min(u32::from(u16::MAX)) as u16);
            }
            MenuRow::Divider => {}
        }
    }
    (
        label.min(MENU_MAX_LABEL_CELLS),
        kind,
        signature.min(MENU_MAX_SIGNATURE_CELLS),
    )
}

/// The first row painted, scrolled far enough that the selection is on screen.
fn first_visible_menu_row(selected: Option<usize>, total: usize, visible: usize) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    if visible == 0 || total <= visible {
        return 0;
    }
    selected
        .saturating_sub(visible.saturating_sub(1))
        .min(total - visible)
}

/// The pane's items along the top, and where the strip has to start so the
/// active tab is on screen (SPEC §24.7).
fn tab_strip(
    pane: &Entity<Pane>,
    rect: CellRect,
    focused: bool,
    window: &Window,
    cx: &App,
) -> Option<TabStripView> {
    let items = pane.read(cx).items().cloned().collect::<Vec<_>>();
    if items.is_empty() || rect.height == 0 {
        return None;
    }
    // The row Zed's layout leaves at the top of the pane for the element `ted`
    // put where the tab bar was (SPEC §25.2).
    let rect = CellRect::new(rect.x, rect.y, rect.width, 1);

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
        first: first_visible_tab(&tabs, active, rect.width),
        rect,
        tabs,
        active,
        focused,
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
    let text_rect = snapshot.editor()?.text_rect;
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
            lines.push(StyledText::default());
        }
        for line in &content.lines {
            lines.extend(wrap_hover_line(line, inner));
        }
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

/// One rendered line broken to the panel's width.
///
/// A line the markdown renderer marked unwrappable — a fenced code line, a row
/// of a box-drawn table — is clipped instead: breaking either would say
/// something the document does not (SPEC §24.8).
fn wrap_hover_line(line: &HoverLine, width: usize) -> Vec<StyledText> {
    if width == 0 {
        return Vec::new();
    }
    if !line.wrap {
        return vec![slice_styled(&line.text, clip_range(&line.text.text, width))];
    }

    // The continuation is indented, so it has that many fewer cells to fill —
    // wrapping both to the full width and then indenting would push the tail of
    // every continued line off the panel.
    let indent = usize::from(line.indent).min(width.saturating_sub(1));
    let rest_width = width.saturating_sub(indent).max(1);
    wrap_ranges(&line.text.text, width, rest_width)
        .into_iter()
        .enumerate()
        .map(|(index, range)| {
            let sliced = slice_styled(&line.text, range);
            if index == 0 || indent == 0 {
                sliced
            } else {
                indent_styled(sliced, indent)
            }
        })
        .collect()
}

/// The bytes covering the first `width` cells, which is what a line that may
/// not be broken keeps.
fn clip_range(text: &str, width: usize) -> Range<usize> {
    let mut cells = 0usize;
    for (offset, cluster) in text.grapheme_indices(true) {
        let next = cells + cluster_cells(cluster) as usize;
        if next > width {
            return 0..offset;
        }
        cells = next;
    }
    0..text.len()
}

/// Breaks one line at a space where there is one and mid-word where there is
/// not, as byte ranges into `text`.
///
/// Ranges rather than owned strings because a wrapped line's *styling* has to be
/// cut at exactly the boundaries its text was, and a `String` has forgotten
/// where it came from by the time the spans are sliced.
fn wrap_ranges(text: &str, first_width: usize, rest_width: usize) -> Vec<Range<usize>> {
    if first_width == 0 {
        return vec![0..text.len()];
    }
    let mut lines = Vec::new();
    let mut width = first_width;
    let mut start = 0usize;
    let mut cells = 0usize;
    // The last space this line could be broken at, and the offset just past it,
    // so the space itself belongs to neither line.
    let mut space: Option<(usize, usize)> = None;

    for (offset, cluster) in text.grapheme_indices(true) {
        let cluster_cells = cluster_cells(cluster) as usize;
        if cells + cluster_cells > width && offset > start {
            let (end, next) = match space.filter(|(at, _)| *at > start) {
                Some((at, after)) => (at, after),
                None => (offset, offset),
            };
            lines.push(start..end);
            start = next;
            width = rest_width.max(1);
            cells = text_cells(text.get(start..offset).unwrap_or_default()) as usize;
            space = None;
        }
        if cluster == " " {
            space = Some((offset, offset + cluster.len()));
        }
        cells += cluster_cells;
    }
    lines.push(start..text.len());
    lines
}

/// A byte range of a styled line, with the spans cut to the same range and
/// rebased on it.
fn slice_styled(text: &StyledText, range: Range<usize>) -> StyledText {
    let sliced = text.text.get(range.clone()).unwrap_or_default().to_owned();
    let spans = text
        .spans
        .iter()
        .filter_map(|span| {
            let start = span.range.start.max(range.start);
            let end = span.range.end.min(range.end);
            (start < end).then(|| StyledSpan {
                range: start - range.start..end - range.start,
                style: span.style,
            })
        })
        .collect();
    StyledText {
        text: sliced,
        spans,
    }
}

fn indent_styled(text: StyledText, cells: usize) -> StyledText {
    let indent = " ".repeat(cells);
    let spans = text
        .spans
        .into_iter()
        .map(|span| StyledSpan {
            range: span.range.start + indent.len()..span.range.end + indent.len(),
            style: span.style,
        })
        .collect();
    StyledText {
        text: indent + &text.text,
        spans,
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

/// `area` is the cells of the pane the item may be painted in — the pane's rect
/// less the strip's row above it and the rule's column beside it (SPEC §25.2,
/// §25.3). Everything here is still read back from the editor; the area only
/// says what to clip against, which with one pane is the window and with a split
/// is that pane's share of it.
fn build_editor_view(
    editor: &mut Editor,
    area: CellRect,
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
    // Before the first frame there are no bounds to read, and the pane's own
    // area is the closest true statement about where the editor will be.
    let top = bounds
        .map(|bounds| cells(bounds.origin.y, CELL_HEIGHT))
        .unwrap_or(area.y)
        .max(area.y);
    let left = bounds
        .map(|bounds| cells(bounds.origin.x, CELL_WIDTH))
        .unwrap_or(area.x)
        .max(area.x);
    let bottom = area.y.saturating_add(area.height);
    let right = area.x.saturating_add(area.width);
    let height = bounds
        .map(|bounds| cells(bounds.size.height, CELL_HEIGHT))
        .unwrap_or(area.height)
        .min(bottom.saturating_sub(top));

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
        .unwrap_or_else(|| right.saturating_sub(left.saturating_add(gutter_cells)));

    let text_left = left.saturating_add(gutter_cells);
    let gutter_rect = CellRect::new(
        left,
        top,
        gutter_cells.min(right.saturating_sub(left)),
        height,
    );
    let text_rect = CellRect::new(
        text_left,
        top,
        text_width.min(right.saturating_sub(text_left.min(right))),
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

    let diff_markers = diff_markers_for_rows(display, first_row..end_row);

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

        // An *expanded* hunk is the one case where the row knows more than the
        // hunk does: its deleted text is present in the multibuffer as rows of
        // its own, and `RowInfo::diff_status` says which side of the change
        // each row is. So a modified hunk that has been expanded is drawn as
        // the `-` and `+` it really is, rather than as `~` over both halves.
        let diff = info
            .diff_status
            .map(HunkMark::from)
            .or_else(|| diff_markers.get(&display_row).copied());
        // Read from the status colours already threaded through `EditorStyle`
        // (SPEC §11 step 1) rather than the dedicated `editor_diff_hunk_*`
        // theme fields Zed's own gutter uses, so this stays a plain function
        // of what `build_editor_view` already has in scope.
        let diff_colors = diff.map(|mark| match mark.marker {
            DiffMarker::Added => (style.status.created, style.status.created_background),
            DiffMarker::Modified => (style.status.modified, style.status.modified_background),
            DiffMarker::Deleted => (style.status.deleted, style.status.deleted_background),
        });
        // Only an expanded hunk tints the whole row, because only then is the
        // row itself the changed text. A collapsed hunk stands for a change
        // that is not on screen as rows of its own, so it says so in the
        // marker's cell and leaves the line it summarises alone.
        if background.is_none() && diff.is_some_and(|mark| mark.expanded) {
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
            diff: diff.map(|mark| mark.marker),
            diff_foreground: diff_colors.map(|(foreground, _)| foreground),
            diff_background: diff
                .zip(diff_colors)
                .and_then(|(mark, (_, tint))| mark.background(tint, &colors)),
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

/// The diff marker each visible display row carries, keyed by display row.
///
/// Read from the diff itself rather than from `RowInfo::diff_status`, which is
/// only `Some` where a hunk has been *expanded* into the multibuffer as deleted
/// text — the project diff view and an expanded hunk, neither of which is the
/// ordinary case of an edited file whose gutter should still show what changed.
/// `EditorElement` goes to the same place through `display_diff_hunks_for_rows`
/// (`editor/src/git.rs`), which is `pub(super)` and so has to be re-derived
/// here from the two public halves it is built from.
fn diff_markers_for_rows(display: &DisplaySnapshot, rows: Range<u32>) -> HashMap<u32, HunkMark> {
    let start = DisplayPoint::new(DisplayRow(rows.start), 0).to_point(display);
    let end = DisplayPoint::new(DisplayRow(rows.end), 0).to_point(display);

    let mut markers = HashMap::new();
    for hunk in display.buffer_snapshot().diff_hunks_in_range(start..end) {
        // Collapsed by construction: a hunk reaching a row through this map is
        // one whose changed text is not on screen as rows of its own. Where it
        // is, `RowInfo::diff_status` answers for those rows first.
        let marker = HunkMark {
            expanded: false,
            ..HunkMark::from(hunk.status())
        };
        let first = display
            .point_to_display_point(MultiBufferPoint::new(hunk.row_range.start.0, 0), Bias::Left)
            .row()
            .0;
        // An empty row range is a pure deletion: nothing of it survives in the
        // buffer, so it marks the one row that closed over it rather than a
        // span, and it never displaces a marker that row earned itself.
        if hunk.row_range.is_empty() {
            markers.entry(first).or_insert(marker);
            continue;
        }
        // The *end* of the hunk's last buffer row, not its start, so a
        // soft-wrapped row is marked across every display row it occupies —
        // the same two points `display_diff_hunks_for_rows` resolves.
        let last_row = MultiBufferRow(hunk.row_range.end.0.saturating_sub(1));
        let last = display
            .point_to_display_point(
                MultiBufferPoint::new(last_row.0, display.buffer_snapshot().line_len(last_row)),
                Bias::Right,
            )
            .row()
            .0;
        for row in first..=last {
            markers.insert(row, marker);
        }
    }
    markers
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
    style_from_highlight(highlight, editor_style.text.color)
}

/// A GPUI highlight as a terminal style, against the colour whatever surface it
/// lands on would otherwise have painted.
///
/// The default colour is a parameter rather than the editor's, because the
/// completions box is a surface of the theme's and a run that names no colour
/// belongs to *it* — and because `fade_out` has nothing to fade without one, so
/// a faded run with no base would arrive at full strength (SPEC §24.9).
pub fn style_from_highlight(highlight: Option<gpui::HighlightStyle>, default: Hsla) -> SpanStyle {
    let Some(highlight) = highlight else {
        return SpanStyle {
            foreground: Some(default),
            ..Default::default()
        };
    };

    let mut foreground = highlight.color.unwrap_or(default);
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
        let row0 = &before.editor().expect("no editor view").rows[0];
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
        let editor_view = after.editor().expect("no editor view");
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
        let row0 = &snapshot.editor().expect("no editor view").rows[0];
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
        let text_rect = snapshot.editor().expect("no editor view").text_rect;
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
        let rows = &snapshot.editor().expect("no editor view").rows;
        let block_row = rows
            .iter()
            .find(|row| row.kind == RowKind::Block)
            .expect("no block row in the projection");
        assert!(!block_row.text.is_empty(), "the block row was left blank");
        assert_eq!(block_row.gutter.line_number, None);
    }

    /// SPEC §25.1: a pane's bounds are floored into cells and clipped to the
    /// window, so an axis that split an odd number of columns lands the right
    /// pane on the cell it starts in rather than the one after it.
    #[test]
    fn a_panes_bounds_are_floored_into_the_cell_it_starts_in() {
        let window = CellRect::new(0, 0, 101, 20);
        let half = gpui::Bounds {
            origin: gpui::point(CELL_WIDTH * 50.5, gpui::px(0.0)),
            size: gpui::size(CELL_WIDTH * 50.5, CELL_HEIGHT * 20.0),
        };
        let rect = cell_rect(half, window);
        assert_eq!((rect.x, rect.width), (50, 50));

        // And nothing reaches past the window, whatever the layout reported.
        let overrun = gpui::Bounds {
            origin: gpui::point(gpui::px(0.0), gpui::px(0.0)),
            size: gpui::size(CELL_WIDTH * 200.0, CELL_HEIGHT * 40.0),
        };
        let rect = cell_rect(overrun, window);
        assert_eq!((rect.width, rect.height), (101, 20));
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

    fn wrapped(text: &str, width: usize) -> Vec<String> {
        wrap_hover_line(&HoverLine::prose(StyledText::plain(text)), width)
            .into_iter()
            .map(|line| line.text)
            .collect()
    }

    #[test]
    fn wrapping_breaks_at_spaces() {
        assert_eq!(
            wrapped("this function takes 5 arguments", 20),
            vec!["this function takes", "5 arguments"]
        );
    }

    #[test]
    fn a_word_wider_than_the_panel_is_broken_rather_than_clipped() {
        assert_eq!(
            wrapped("std::collections::HashMap", 10),
            vec!["std::colle", "ctions::Ha", "shMap"]
        );
        // Cells, not characters: a wide grapheme takes two of them.
        assert_eq!(wrapped("日本語", 4), vec!["日本", "語"]);
    }

    /// SPEC §24.8: a fenced code line and a box-drawn table row are laid out
    /// already, so they are clipped rather than broken.
    #[test]
    fn a_line_that_may_not_wrap_is_clipped_instead() {
        let line = HoverLine {
            text: StyledText::plain("fn build(frame: Frame) -> ViewSnapshot"),
            wrap: false,
            indent: 0,
        };
        let wrapped = wrap_hover_line(&line, 10);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].text, "fn build(f");
    }

    /// A list item's continuation lines up under its text rather than under its
    /// bullet, which is what the renderer's `indent` is for.
    #[test]
    fn a_continuation_is_indented_and_wraps_to_the_narrower_width() {
        let line = HoverLine {
            text: StyledText::plain("• one two three four"),
            wrap: true,
            indent: 2,
        };
        let wrapped = wrap_hover_line(&line, 10)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>();
        assert_eq!(wrapped, vec!["• one two", "  three", "  four"]);
    }

    /// Wrapping cuts a line's styling at exactly the boundaries it cut the
    /// text, which is the whole reason `wrap_ranges` reports ranges.
    #[test]
    fn a_wrapped_lines_spans_are_cut_and_rebased_with_its_text() {
        let text = StyledText {
            text: "alpha bravo charlie".to_owned(),
            spans: vec![StyledSpan {
                // "bravo charlie", spanning the break.
                range: 6..19,
                style: SpanStyle {
                    bold: true,
                    ..Default::default()
                },
            }],
        };
        let wrapped = wrap_hover_line(&HoverLine::prose(text), 12);
        assert_eq!(wrapped[0].text, "alpha bravo");
        assert_eq!(wrapped[0].spans[0].range, 6..11);
        assert_eq!(wrapped[1].text, "charlie");
        assert_eq!(wrapped[1].spans[0].range, 0..7);
    }

    #[test]
    fn the_menu_scrolls_only_as_far_as_the_selection_needs() {
        assert_eq!(first_visible_menu_row(Some(0), 20, 5), 0);
        assert_eq!(first_visible_menu_row(Some(3), 20, 5), 0);
        // The selection has just left the bottom of the box.
        assert_eq!(first_visible_menu_row(Some(5), 20, 5), 1);
        // And it never scrolls past the last row.
        assert_eq!(first_visible_menu_row(Some(19), 20, 5), 15);
        assert_eq!(first_visible_menu_row(None, 20, 5), 0);
        // A box tall enough for everything never scrolls at all.
        assert_eq!(first_visible_menu_row(Some(4), 5, 5), 0);
    }

    /// SPEC §24.9: the columns are computed once from the widest label rather
    /// than per row, and capped so one absurd signature cannot push the box
    /// past what the text rect can hold.
    #[test]
    fn the_menus_columns_are_the_widest_of_each_and_no_wider() {
        let entry = |label: &str, kind: &str, signature: &str| MenuRow::Entry {
            label: StyledText::plain(label),
            matched: Vec::new(),
            kind: Some(kind.to_owned()),
            signature: StyledText::plain(signature),
        };
        let rows = vec![
            entry("filter", "fn", "(self) -> Iter"),
            entry("f", "const", "u8"),
            MenuRow::Divider,
        ];
        assert_eq!(menu_columns(&rows), (6, 5, 14));

        let long = "x".repeat(200);
        let rows = vec![entry(&long, "fn", &long)];
        assert_eq!(
            menu_columns(&rows),
            (MENU_MAX_LABEL_CELLS, 2, MENU_MAX_SIGNATURE_CELLS)
        );
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
