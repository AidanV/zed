//! The cell-metric contract (SPEC §5): the single definition of how many GPUI
//! pixels a terminal cell is worth, shared by [`crate::text_system`] (which
//! measures in it) and [`crate::platform`] (which sizes the window in it).

use gpui::{Pixels, Size, px, size};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthChar as _;

/// A cell's advance as a fraction of the em square. Every quantity below is
/// derived from this, so an `m` is exactly one cell wide at any font size.
pub const CELL_WIDTH_PER_EM: f32 = 0.5;

/// `ted` pins these two settings so [`CELL_SIZE`] is a constant rather than a
/// function of the user's `settings.json` (SPEC §5.1, §5.5).
pub const BUFFER_FONT_SIZE_PX: f32 = 16.0;
pub const BUFFER_LINE_HEIGHT: f32 = 1.0;

pub const BUFFER_FONT_SIZE: Pixels = px(BUFFER_FONT_SIZE_PX);
pub const CELL_WIDTH: Pixels = px(BUFFER_FONT_SIZE_PX * CELL_WIDTH_PER_EM);
pub const CELL_HEIGHT: Pixels = px(BUFFER_FONT_SIZE_PX * BUFFER_LINE_HEIGHT);

pub const CELL_SIZE: Size<Pixels> = Size {
    width: CELL_WIDTH,
    height: CELL_HEIGHT,
};

/// The width of one cell at an arbitrary font size. `CellTextSystem` reports
/// metrics in em units and GPUI scales them by the font size, so this is what
/// `TextSystem::em_width` and `em_advance` both resolve to (SPEC §5.1).
pub fn cell_width_at(font_size: Pixels) -> Pixels {
    px(f32::from(font_size) * CELL_WIDTH_PER_EM)
}

/// Cells occupied by one grapheme cluster. Summing per character rather than
/// measuring the cluster is what makes combining marks contribute zero, which
/// is what `LineWrapper` needs (SPEC §5.3).
pub fn cluster_cells(cluster: &str) -> u32 {
    cluster
        .chars()
        .map(|ch| ch.width().unwrap_or(0) as u32)
        .sum()
}

/// Cells occupied by a string, counted the same way a terminal counts them.
pub fn text_cells(text: &str) -> u32 {
    text.graphemes(true).map(cluster_cells).sum()
}

/// The pixel size of a `columns` x `rows` grid of cells.
pub fn grid_size(columns: u16, rows: u16) -> Size<Pixels> {
    size(
        px(columns as f32 * f32::from(CELL_WIDTH)),
        px(rows as f32 * f32::from(CELL_HEIGHT)),
    )
}
