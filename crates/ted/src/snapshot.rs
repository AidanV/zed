//! The backend-to-frontend projection (SPEC §10): one struct, rebuilt each
//! frame from GPUI-side reads, containing no GPUI handles — only plain data.
//! That constraint is what keeps an out-of-process frontend possible later, and
//! it makes the renderer testable without a terminal.

use unicode_segmentation::UnicodeSegmentation as _;

use crate::cell::cluster_cells;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewSnapshot {
    pub columns: u16,
    pub rows: u16,
    pub editor: Option<EditorView>,
    pub status: StatusView,
    /// Where to park the terminal's hardware cursor. SPEC §7 places the real
    /// cursor rather than drawing one, so the terminal blinks it and screen
    /// readers see it.
    pub cursor: Option<CellPoint>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorView {
    pub text_rect: CellRect,
    /// Horizontal scroll in cells. Vertical scroll is already applied: `rows`
    /// is the window into the display map, so `ted` never keeps its own
    /// vertical offset (SPEC §15).
    pub scroll_columns: u16,
    pub rows: Vec<RowView>,
    pub max_display_row: u32,
    pub soft_wrapped: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowView {
    pub display_row: u32,
    pub text: String,
    /// Byte column to cell column for this row, built once per row per frame.
    /// `DisplayPoint::column()` is a byte offset, so every consumer — cursor,
    /// selections, highlights — reads this one table instead of converting
    /// ad hoc (SPEC §5.4, and the §22 risk row on byte/grapheme confusion).
    pub byte_to_cell: Vec<u16>,
}

impl RowView {
    pub fn new(display_row: u32, text: String) -> Self {
        let byte_to_cell = byte_to_cell_table(&text);
        Self {
            display_row,
            text,
            byte_to_cell,
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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusView {
    pub path: Option<String>,
    pub dirty: bool,
    pub mode: Option<String>,
    pub message: Option<String>,
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
}
