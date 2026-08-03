//! The terminal panel (SPEC §25.5): a shell kept beside the editor, projected
//! from `alacritty_terminal`'s own grid rather than from `TerminalElement`'s
//! shaped runs.
//!
//! `Terminal::last_content()` hands back `Vec<IndexedCell>` — a point, a
//! character, a foreground, a background, and flags — which maps onto a
//! terminal cell with no measurement involved, because it already is one
//! (SPEC §25.5, "read the terminal's grid, not its element tree"). Nothing
//! here goes through [`crate::snapshot::ViewSnapshot`]'s editor projection.

use gpui::{App, Entity, Hsla, Pixels};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use terminal::{Color, Content, CursorShape as TerminalCursorShape, is_default_background_color};
use terminal_view::TerminalView;
use terminal_view::terminal_element::{convert_color, is_blank};
use terminal_view::terminal_panel::TerminalPanel;
use theme::ActiveTheme as _;

use crate::cell::{CELL_HEIGHT, CELL_WIDTH, text_cells};
use crate::palette::Palette;
use crate::snapshot::{CellPoint, CellRect, CursorShape};

/// A read of the terminal panel's grid (SPEC §25.5): what `ted` paints where
/// the panel's own pane is, once the pane's rect, strip and divider have
/// already been handled like any other pane's (SPEC §25.1–§25.3).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalPanelView {
    /// Where the panel's terminal grid sits, in cells, floored from the bounds
    /// the element reported (SPEC §25.5).
    pub rect: CellRect,
    /// One entry per occupied cell, in the panel's own coordinates (0,0 is the
    /// top-left of `rect`).
    pub cells: Vec<TerminalCell>,
    /// The terminal's own cursor, in absolute grid cells, when the panel has
    /// focus — `ted` has one hardware cursor and it goes where the keyboard is.
    pub cursor: Option<CellPoint>,
    pub cursor_shape: CursorShape,
    /// The panel's own background, painted before any cell. `None` leaves the
    /// terminal's real background showing through, the same call
    /// [`Palette::surface_background`] makes for the editor surface.
    pub background: Option<Hsla>,
    pub focused: bool,
}

/// One occupied cell of the grid alacritty reported, translated but not
/// reinterpreted: SPEC §25.5 calls the terminal panel "the one surface in
/// `ted` that loses nothing at all in translation," because `INVERSE`,
/// `BOLD`, `ITALIC`, `UNDERLINE`, `STRIKEOUT` and `DIM` are terminal
/// attributes on both sides of the projection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalCell {
    pub column: u16,
    pub row: u16,
    /// One grapheme cluster: the cell's own character, plus any zero-width
    /// characters alacritty stacked onto it (combining marks, emoji variation
    /// selectors).
    pub text: String,
    pub foreground: Option<Hsla>,
    /// `None` when the cell's background is the terminal's own default and
    /// the panel's background fill (or the real terminal's, under it) already
    /// covers it — the same convention a `RowView`'s background carries for
    /// the editor surface.
    pub background: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
}

/// Reads the panel's terminal grid, or `None` when there is nothing to
/// project: no terminal item open, or a rect Zed has not laid out yet.
///
/// Must run after Zed has painted the panel at least once, the same ordering
/// requirement `snapshot::build_editor_view` has on `Editor::last_bounds`
/// (SPEC §10.2): `Content::terminal_bounds` is populated during
/// `TerminalElement`'s own layout.
pub fn read(panel: &Entity<TerminalPanel>, focused: bool, cx: &App) -> Option<TerminalPanelView> {
    // `TerminalPanel::active_pane` is `pub(crate)` to `terminal_view`, so
    // `panes()` — which returns every pane in the panel's own group — is the
    // only public way in. `ted` gives the user no way to split the terminal
    // panel itself (SPEC §25.6's left-out table has no entry for it), so the
    // first pane is the only one there ever is.
    let pane = panel.read(cx).panes().into_iter().next()?.clone();
    let item = pane.read(cx).active_item()?;
    let terminal_view = item.act_as::<TerminalView>(cx)?;
    let terminal = terminal_view.read(cx).terminal().clone();
    let terminal = terminal.read(cx);
    let content = terminal.last_content();

    // Mirrors how `snapshot::build_editor_view` floors `Editor::last_bounds`:
    // the rect is a reported bound like every other one in SPEC §25, and the
    // pty's own row and column count follows from it rather than the other
    // way round (SPEC §25.5, "Where it goes").
    let bounds = content.terminal_bounds.bounds;
    let rect = CellRect::new(
        cells(bounds.origin.x, CELL_WIDTH),
        cells(bounds.origin.y, CELL_HEIGHT),
        cells(bounds.size.width, CELL_WIDTH),
        cells(bounds.size.height, CELL_HEIGHT),
    );
    if rect.width == 0 || rect.height == 0 {
        return None;
    }

    let theme = cx.theme();
    let mut cells = Vec::new();
    for indexed in &content.cells {
        // The trailing half of a double-width grapheme: the leading cell's
        // own text is already more than one cell wide, and the renderer
        // blanks the cell after it when it paints that one (SPEC §25.5).
        //
        // `Flags::LEADING_WIDE_CHAR_SPACER` — the placeholder alacritty
        // writes before a wide glyph that would otherwise split across a
        // line wrap — has no equivalent accessor on `terminal::Cell`: it
        // exposes `is_wide_char_spacer()` for the trailing spacer but names
        // no method for the leading one, and the `Flags` bitset itself is
        // private to the `terminal` crate. Alacritty always writes that
        // placeholder as a literal space (`Term::input_normal`), so it falls
        // through to the blank check below and is skipped that way instead —
        // unless the pen had a non-default colour at the point of the wrap,
        // in which case one cell keeps a stray background. `ted` cannot tell
        // the two cases apart through the API it has been given.
        if indexed.is_wide_char_spacer() {
            continue;
        }
        // A blank cell the background fill already covers, and the reason
        // this is a `Vec` of occupied cells rather than one entry per grid
        // cell — cheap enough to compare frame to frame (SPEC §25.5).
        if is_blank(indexed) {
            continue;
        }
        let Ok(row) = u16::try_from(indexed.point.line) else {
            continue;
        };
        let Ok(column) = u16::try_from(indexed.point.column) else {
            continue;
        };
        if row >= rect.height || column >= rect.width {
            continue;
        }

        let mut text = String::from(indexed.character());
        if let Some(extra) = indexed.zerowidth() {
            text.extend(extra.iter());
        }

        let (foreground_color, background_color) = apply_inverse(
            indexed.foreground(),
            indexed.background(),
            indexed.is_inverse(),
        );
        let background = (!is_default_background_color(background_color))
            .then(|| convert_color(&background_color, theme));

        // `Flags::HIDDEN` has the same gap as the leading wide-char spacer:
        // no accessor reaches it through `terminal::Cell`, so a concealed
        // cell paints its text like any other rather than just its
        // background (SPEC §25.5).
        cells.push(TerminalCell {
            column,
            row,
            text,
            foreground: Some(convert_color(&foreground_color, theme)),
            background,
            bold: indexed.is_bold(),
            italic: indexed.is_italic(),
            underline: indexed.has_underline(),
            strikethrough: indexed.has_strikeout(),
            dim: indexed.is_dim(),
        });
    }

    Some(TerminalPanelView {
        rect,
        cells,
        cursor: focused.then(|| cursor_position(content, rect)).flatten(),
        cursor_shape: cursor_shape(content.cursor.shape),
        background: Some(theme.colors().terminal_background),
        focused,
    })
}

/// `Flags::INVERSE` swaps a cell's foreground and background, which is the
/// whole of what it means on either side of the projection — one of the
/// flags SPEC §25.5 says are "terminal attributes on both sides," so nothing
/// here is reinterpreted, only relocated.
fn apply_inverse(foreground: Color, background: Color, inverse: bool) -> (Color, Color) {
    if inverse {
        (background, foreground)
    } else {
        (foreground, background)
    }
}

/// The terminal's own cursor, in absolute grid cells, or `None` when
/// alacritty has hidden it or placed it outside the reported bounds.
fn cursor_position(content: &Content, rect: CellRect) -> Option<CellPoint> {
    if content.cursor.shape == TerminalCursorShape::Hidden {
        return None;
    }
    let row = u16::try_from(content.cursor.point.line).ok()?;
    let column = u16::try_from(content.cursor.point.column).ok()?;
    if row >= rect.height || column >= rect.width {
        return None;
    }
    Some(CellPoint {
        column: rect.x.saturating_add(column),
        row: rect.y.saturating_add(row),
    })
}

/// SPEC §7 gives `ted` three cursor shapes to ask the real terminal for. A
/// hollow block has no dedicated one; a filled block is the closer of the two
/// remaining choices, since both mark "not inserting" the way the bar does
/// not. `Hidden` never reaches here in practice — [`cursor_position`] returns
/// `None` for it first — so its mapping is arbitrary.
fn cursor_shape(shape: TerminalCursorShape) -> CursorShape {
    match shape {
        TerminalCursorShape::Block | TerminalCursorShape::HollowBlock => CursorShape::Block,
        TerminalCursorShape::Bar => CursorShape::Bar,
        TerminalCursorShape::Underline => CursorShape::Underline,
        TerminalCursorShape::Hidden => CursorShape::Block,
    }
}

/// Floors a pixel extent into whole cells, the same arithmetic
/// `snapshot`'s private `cells` helper uses for the editor's rect (SPEC §5.3).
fn cells(pixels: Pixels, cell: Pixels) -> u16 {
    (f32::from(pixels) / f32::from(cell)).max(0.0) as u16
}

/// Paints a [`TerminalPanelView`] into a cell grid: a view and a palette in,
/// cells written to a Ratatui buffer out, the same contract
/// [`crate::render::render`] has for the rest of the grid (SPEC §11, applied
/// to the panel by SPEC §25.5).
pub fn render(view: &TerminalPanelView, palette: &Palette, buffer: &mut Buffer) {
    let area = clamp(view.rect, buffer.area);
    let surface = view
        .background
        .and_then(|background| palette.surface_background(background));
    if let Some(surface) = surface {
        fill(area, Style::default().bg(surface), buffer);
    }

    // The panel's own ground, under every cell that named no background of its
    // own: `write_cell` resets the cell it paints, so a cell that inherited the
    // fill instead of carrying it would come out with the real terminal's
    // background showing through the middle of the panel.
    let ground = match surface {
        Some(surface) => Style::default().bg(surface),
        None => Style::default(),
    };

    for cell in &view.cells {
        let Some(x) = view.rect.x.checked_add(cell.column) else {
            continue;
        };
        let Some(y) = view.rect.y.checked_add(cell.row) else {
            continue;
        };
        let style = terminal_style(cell, ground, palette);
        write_cell(&cell.text, (x, y), style, buffer);

        // A double-width grapheme's trailing cell, blanked rather than left
        // holding whatever the buffer had before (SPEC §11 step 2).
        let width = text_cells(&cell.text).min(u32::from(u16::MAX)) as u16;
        for trailing in 1..width {
            let Some(trailing_x) = x.checked_add(trailing) else {
                break;
            };
            write_cell("", (trailing_x, y), style, buffer);
        }
    }
}

fn terminal_style(cell: &TerminalCell, ground: Style, palette: &Palette) -> Style {
    let mut style = ground;
    if let Some(foreground) = cell.foreground {
        style = style.fg(palette.color(foreground));
    }
    if let Some(background) = cell.background {
        style = style.bg(palette.color(background));
    }
    if cell.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if cell.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn fill(area: Rect, style: Style, buffer: &mut Buffer) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            write_cell(" ", (x, y), style, buffer);
        }
    }
}

/// Puts a grapheme in a cell, replacing everything that was there — the same
/// full-reset convention `render`'s own `write_cell` uses, and for the same
/// reason: a style that names no colour of its own must not inherit one from
/// whatever the buffer held before.
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
    use gpui::hsla;
    use terminal::NamedColor;

    fn palette() -> Palette {
        Palette::new(ColorDepth::TrueColor, hsla(0.0, 0.0, 0.0, 1.0), true)
    }

    fn cell(column: u16, row: u16, text: &str) -> TerminalCell {
        TerminalCell {
            column,
            row,
            text: text.to_owned(),
            foreground: Some(hsla(0.0, 0.0, 1.0, 1.0)),
            ..Default::default()
        }
    }

    #[test]
    fn cells_land_at_the_rect_s_absolute_position() {
        let view = TerminalPanelView {
            rect: CellRect::new(2, 3, 10, 5),
            cells: vec![cell(0, 0, "a"), cell(1, 0, "b")],
            ..Default::default()
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));
        render(&view, &palette(), &mut buffer);
        assert_eq!(buffer.cell((2, 3)).map(|cell| cell.symbol()), Some("a"));
        assert_eq!(buffer.cell((3, 3)).map(|cell| cell.symbol()), Some("b"));
    }

    #[test]
    fn a_wide_grapheme_blanks_the_cell_after_it() {
        let view = TerminalPanelView {
            rect: CellRect::new(0, 0, 10, 2),
            cells: vec![cell(0, 0, "日")],
            ..Default::default()
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        render(&view, &palette(), &mut buffer);
        assert_eq!(buffer.cell((0, 0)).map(|cell| cell.symbol()), Some("日"));
        assert_eq!(buffer.cell((1, 0)).map(|cell| cell.symbol()), Some(""));
    }

    #[test]
    fn a_cell_outside_the_buffer_is_dropped_not_painted() {
        let view = TerminalPanelView {
            rect: CellRect::new(0, 0, 10, 2),
            cells: vec![cell(50, 50, "x")],
            ..Default::default()
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        // The point of the test is that this does not panic.
        render(&view, &palette(), &mut buffer);
    }

    #[test]
    fn background_and_bold_reach_the_terminal_style() {
        let mut painted = cell(0, 0, "x");
        painted.background = Some(hsla(0.0, 0.0, 0.2, 1.0));
        painted.bold = true;
        let view = TerminalPanelView {
            rect: CellRect::new(0, 0, 10, 2),
            cells: vec![painted],
            ..Default::default()
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        render(&view, &palette(), &mut buffer);
        let cell = buffer.cell((0, 0)).expect("no cell");
        assert_eq!(cell.bg, ratatui::style::Color::Rgb(51, 51, 51));
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let foreground = Color::Named(NamedColor::Red);
        let background = Color::Named(NamedColor::Blue);
        assert_eq!(
            apply_inverse(foreground, background, true),
            (background, foreground)
        );
        assert_eq!(
            apply_inverse(foreground, background, false),
            (foreground, background)
        );
    }
}
