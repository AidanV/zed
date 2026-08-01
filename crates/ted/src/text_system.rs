//! `CellTextSystem` (SPEC §5.6): a `gpui::PlatformTextSystem` whose metrics are
//! defined rather than measured, so that one GPUI "em" is exactly one terminal
//! cell at any font size.
//!
//! `em_width` and `em_advance` reach `CellTextSystem` through two different
//! trait methods (`typographic_bounds` and `advance`); SPEC §5.1 requires them
//! to agree exactly, since editor layout consumes both independently
//! (wrap width from one, visible column count from the other). Both route
//! through `glyph_width_units`, the single place cell width is computed, so
//! they cannot drift apart.

use std::borrow::Cow;

use anyhow::{Result, anyhow};
use collections::HashMap;
use gpui::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId, Hsla, LineLayout, Pixels,
    PlatformTextSystem, RenderGlyphParams, ShapedGlyph, ShapedRun, Size, TextRenderingMode, point,
    px, size,
};
use parking_lot::RwLock;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::cell;

/// Chosen so that `cell::CELL_WIDTH_PER_EM` (`0.5`) and every font size in the
/// clamp range `[6, 100]` (`crates/theme_settings/src/settings.rs:18`) scale
/// without rounding.
const UNITS_PER_EM: u32 = 1000;
const ASCENT: f32 = 800.0;
const DESCENT: f32 = -200.0;
const LINE_GAP: f32 = 0.0;
const UNDERLINE_POSITION: f32 = -100.0;
const UNDERLINE_THICKNESS: f32 = 50.0;
const CAP_HEIGHT: f32 = 700.0;
const X_HEIGHT: f32 = 500.0;

/// The one synthetic family every `Font` descriptor is interned under.
/// `CellTextSystem::font_id` treats all font ids as behaviourally equivalent
/// (SPEC §5.6), so a single name is all `all_font_names` needs to report.
const FONT_FAMILY_NAME: &str = "ted-cell";

fn fixed_font_metrics() -> FontMetrics {
    FontMetrics {
        units_per_em: UNITS_PER_EM,
        ascent: ASCENT,
        descent: DESCENT,
        line_gap: LINE_GAP,
        underline_position: UNDERLINE_POSITION,
        underline_thickness: UNDERLINE_THICKNESS,
        cap_height: CAP_HEIGHT,
        x_height: X_HEIGHT,
        bounding_box: Bounds {
            origin: point(0.0, DESCENT),
            size: size(UNITS_PER_EM as f32 * cell::CELL_WIDTH_PER_EM, ASCENT - DESCENT),
        },
    }
}

/// A coarse emoji heuristic covering the common presentation blocks. It only
/// selects the paint path (`Window::paint_emoji` picks the polychrome atlas
/// texture; SPEC does not require exact emoji detection) and never affects a
/// measured width, which always comes from `unicode-width` via `cell::cluster_cells`.
fn cluster_is_emoji(cluster: &str) -> bool {
    cluster.chars().any(|ch| {
        matches!(ch as u32, 0x1F000..=0x1FFFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF)
    })
}

#[derive(Default)]
struct FontTable {
    ids_by_descriptor: HashMap<Font, FontId>,
}

#[derive(Default)]
struct GlyphTable {
    chars_by_id: Vec<char>,
    ids_by_char: HashMap<char, GlyphId>,
}

pub struct CellTextSystem {
    fonts: RwLock<FontTable>,
    glyphs: RwLock<GlyphTable>,
}

impl CellTextSystem {
    pub fn new() -> Self {
        Self {
            fonts: RwLock::new(FontTable::default()),
            glyphs: RwLock::new(GlyphTable::default()),
        }
    }

    /// Interns `descriptor`, returning the same `FontId` for the same
    /// descriptor on every call. Every id is behaviourally equivalent (SPEC
    /// §5.6), so the only job here is stability, not font resolution.
    fn intern_font(&self, descriptor: &Font) -> FontId {
        if let Some(&id) = self.fonts.read().ids_by_descriptor.get(descriptor) {
            return id;
        }
        let mut fonts = self.fonts.write();
        if let Some(&id) = fonts.ids_by_descriptor.get(descriptor) {
            return id;
        }
        let id = FontId(fonts.ids_by_descriptor.len());
        fonts.ids_by_descriptor.insert(descriptor.clone(), id);
        id
    }

    /// Interns `ch`, returning the same `GlyphId` for the same character on
    /// every call. Backs `glyph_for_char`, and `char_for_glyph` is its inverse.
    fn intern_glyph(&self, ch: char) -> GlyphId {
        if let Some(&id) = self.glyphs.read().ids_by_char.get(&ch) {
            return id;
        }
        let mut glyphs = self.glyphs.write();
        if let Some(&id) = glyphs.ids_by_char.get(&ch) {
            return id;
        }
        let id = GlyphId(glyphs.chars_by_id.len() as u32);
        glyphs.chars_by_id.push(ch);
        glyphs.ids_by_char.insert(ch, id);
        id
    }

    fn char_for_glyph(&self, glyph_id: GlyphId) -> Result<char> {
        self.glyphs
            .read()
            .chars_by_id
            .get(glyph_id.0 as usize)
            .copied()
            .ok_or_else(|| anyhow!("glyph id {} was never issued by CellTextSystem", glyph_id.0))
    }

    /// The single source of truth for a glyph's width, in font units. Shared
    /// by `advance` and `typographic_bounds` so the two cannot disagree
    /// (SPEC §5.1): both are `cells(ch) * UNITS_PER_EM * CELL_WIDTH_PER_EM`.
    fn glyph_width_units(&self, glyph_id: GlyphId) -> Result<f32> {
        let ch = self.char_for_glyph(glyph_id)?;
        let mut buffer = [0u8; 4];
        let cluster = ch.encode_utf8(&mut buffer);
        Ok(cell::cluster_cells(cluster) as f32 * UNITS_PER_EM as f32 * cell::CELL_WIDTH_PER_EM)
    }
}

impl Default for CellTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for CellTextSystem {
    fn add_fonts(&self, _fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        vec![FONT_FAMILY_NAME.to_string()]
    }

    fn font_id(&self, descriptor: &Font) -> Result<FontId> {
        Ok(self.intern_font(descriptor))
    }

    fn font_metrics(&self, _font_id: FontId) -> FontMetrics {
        fixed_font_metrics()
    }

    fn typographic_bounds(&self, _font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let width = self.glyph_width_units(glyph_id)?;
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(width, 0.0),
        })
    }

    fn advance(&self, _font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let width = self.glyph_width_units(glyph_id)?;
        Ok(size(width, 0.0))
    }

    fn glyph_for_char(&self, _font_id: FontId, ch: char) -> Option<GlyphId> {
        Some(self.intern_glyph(ch))
    }

    fn glyph_raster_bounds(&self, _params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        Ok(Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: size(DevicePixels(1), DevicePixels(1)),
        })
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        // Mirrors `AtlasKey::texture_kind` (`crates/gpui/src/platform.rs:1269`):
        // emoji and subpixel-rendered glyphs go through a 4-channel texture,
        // everything else through a 1-channel (alpha-only) one. The tile
        // itself is always transparent, since nothing is ever really painted.
        let channels_per_pixel: usize = if params.is_emoji || params.subpixel_rendering {
            4
        } else {
            1
        };
        let width = raster_bounds.size.width.0.max(0) as usize;
        let height = raster_bounds.size.height.0.max(0) as usize;
        Ok((raster_bounds.size, vec![0u8; width * height * channels_per_pixel]))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let metrics = fixed_font_metrics();

        if text.is_empty() {
            return LineLayout {
                font_size,
                width: px(0.0),
                ascent: metrics.ascent(font_size),
                descent: metrics.descent(font_size),
                runs: Vec::new(),
                len: 0,
            };
        }

        let cell_width = cell::cell_width_at(font_size);

        // `TextSystem::layout_line` (`crates/gpui/src/text_system.rs:645`) can
        // hand us an empty run list for non-empty text (e.g. an empty
        // `TextRun` slice); fall back to one run covering the whole line
        // under a default descriptor, since every id is equivalent anyway.
        let fallback_runs;
        let runs: &[FontRun] = if runs.is_empty() {
            let default_font_id = self.font_id(&Font::default()).unwrap_or(FontId(0));
            fallback_runs = [FontRun {
                len: text.len(),
                font_id: default_font_id,
            }];
            &fallback_runs
        } else {
            runs
        };

        let mut run_end_bytes = Vec::with_capacity(runs.len());
        let mut cumulative_bytes = 0usize;
        for run in runs {
            cumulative_bytes += run.len;
            run_end_bytes.push(cumulative_bytes);
        }

        let mut run_glyphs: Vec<Vec<ShapedGlyph>> = vec![Vec::new(); runs.len()];
        let mut run_cursor = 0usize;
        let mut accumulated_cells: u32 = 0;

        for (start_byte, cluster) in text.grapheme_indices(true) {
            while run_cursor + 1 < runs.len() && start_byte >= run_end_bytes[run_cursor] {
                run_cursor += 1;
            }

            // Every grapheme cluster gets exactly one glyph, positioned at
            // its start; this is what makes `x_for_index` inside a cluster
            // resolve to the cluster start (SPEC §5.6).
            let leading_char = cluster.chars().next().unwrap_or('\u{0}');
            let glyph = ShapedGlyph {
                id: self.intern_glyph(leading_char),
                position: point(cell_width * accumulated_cells as f32, px(0.0)),
                index: start_byte,
                is_emoji: cluster_is_emoji(cluster),
            };
            run_glyphs[run_cursor].push(glyph);

            accumulated_cells += cell::cluster_cells(cluster);
        }

        let width = cell_width * accumulated_cells as f32;

        let shaped_runs = runs
            .iter()
            .zip(run_glyphs)
            .filter(|(_, glyphs)| !glyphs.is_empty())
            .map(|(run, glyphs)| ShapedRun {
                font_id: run.font_id,
                glyphs,
            })
            .collect();

        LineLayout {
            font_size,
            width,
            ascent: metrics.ascent(font_size),
            descent: metrics.descent(font_size),
            runs: shaped_runs,
            len: text.len(),
        }
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        // No antialiasing is ever performed (every rasterized tile is a blank
        // transparent pixel), so there is no subpixel fringing to correct
        // for; Grayscale is the least specific "not subpixel" choice, and
        // matches `NoopTextSystem` (`crates/gpui/src/platform.rs:1207`).
        TextRenderingMode::Grayscale
    }

    fn glyph_dilation_for_color(&self, _color: Hsla) -> u8 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TextSystem, font};
    use std::sync::Arc;
    use unicode_width::UnicodeWidthChar as _;

    fn platform_text_system() -> Arc<TextSystem> {
        Arc::new(TextSystem::new(Arc::new(CellTextSystem::new())))
    }

    /// SPEC §5.1: `em_width` (`typographic_bounds`) and `em_advance`
    /// (`advance`) are consumed by different editor code paths and must
    /// never disagree, at any legal buffer font size
    /// (`crates/theme_settings/src/settings.rs:18` clamps to `[6, 100]`).
    #[test]
    fn em_and_ch_widths_match_cell_width_at_every_clamped_font_size() {
        let text_system = platform_text_system();
        let font_id = text_system.resolve_font(&Font::default());

        for size_px in 6..=100 {
            let font_size = px(size_px as f32);
            let expected = cell::cell_width_at(font_size);

            assert_eq!(
                text_system.em_width(font_id, font_size).expect("em_width"),
                expected,
                "em_width at {size_px}px"
            );
            assert_eq!(
                text_system.em_advance(font_id, font_size).expect("em_advance"),
                expected,
                "em_advance at {size_px}px"
            );
            assert_eq!(
                text_system.ch_width(font_id, font_size).expect("ch_width"),
                expected,
                "ch_width at {size_px}px"
            );
            assert_eq!(
                text_system.ch_advance(font_id, font_size).expect("ch_advance"),
                expected,
                "ch_advance at {size_px}px"
            );
        }

        for size_px in [6.25f32, 13.5, 42.75, 99.9] {
            let font_size = px(size_px);
            let expected = cell::cell_width_at(font_size);
            assert_eq!(
                text_system.em_width(font_id, font_size).expect("em_width"),
                expected
            );
            assert_eq!(
                text_system.em_advance(font_id, font_size).expect("em_advance"),
                expected
            );
        }
    }

    /// SPEC §20.3: the same `unicode-width` measure the renderer uses for
    /// placement must agree with what `layout_line` reports as width, over a
    /// corpus that exercises multi-byte, double-width, zero-width and
    /// multi-codepoint-cluster text.
    #[test]
    fn layout_line_width_matches_text_cells_across_a_unicode_corpus() {
        let system = CellTextSystem::new();
        let corpus = [
            "",
            "hello world",
            "日本語",
            "👨‍👩‍👧‍👦",
            "e\u{0301}",
            "🇯🇵🇺🇸",
            "a日b👍c\u{0301}d",
        ];

        for size_px in [6.0f32, 16.0, 42.5, 100.0] {
            let font_size = px(size_px);
            let cell_width = cell::cell_width_at(font_size);
            for text in corpus {
                let layout = system.layout_line(text, font_size, &[]);
                let expected_width = cell::text_cells(text) as f32 * cell_width;
                assert_eq!(
                    layout.width, expected_width,
                    "width mismatch for {text:?} at {size_px}px"
                );
            }
        }
    }

    /// SPEC §5.6: one `ShapedGlyph` per grapheme cluster, positioned at a
    /// whole-cell multiple, with `index` at the cluster's start byte so
    /// `x_for_index` inside a cluster resolves to the cluster start.
    #[test]
    fn glyph_positions_are_cell_multiples_and_indices_are_char_boundaries() {
        let system = CellTextSystem::new();
        let text = "a\u{0301}日本👨‍👩‍👧‍👦b";

        for size_px in [8.0f32, 16.0, 33.0] {
            let font_size = px(size_px);
            let cell_width = cell::cell_width_at(font_size);
            let layout = system.layout_line(text, font_size, &[]);

            let glyphs: Vec<_> = layout
                .runs
                .iter()
                .flat_map(|run| run.glyphs.iter())
                .collect();
            let expected_clusters: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
            assert_eq!(glyphs.len(), expected_clusters.len());

            let mut accumulated_cells = 0u32;
            let mut previous_x = px(0.0);
            for (glyph, (start_byte, cluster)) in glyphs.iter().zip(expected_clusters.iter()) {
                assert!(
                    text.is_char_boundary(glyph.index),
                    "glyph index {} is not a char boundary",
                    glyph.index
                );
                assert_eq!(glyph.index, *start_byte);

                let expected_x = cell_width * accumulated_cells as f32;
                assert_eq!(glyph.position.x, expected_x);
                assert!(glyph.position.x >= previous_x);
                previous_x = glyph.position.x;

                accumulated_cells += cell::cluster_cells(cluster);
            }

            assert_eq!(layout.width, cell_width * accumulated_cells as f32);
        }
    }

    /// SPEC §5.3: `LineWrapper::width_for_char` consumes exactly this
    /// quantity (`crates/gpui/src/text_system/line_wrapper.rs:487`), so it
    /// must be `cells(ch) * cell_width_at(size)` for every character,
    /// including zero-width and control ones.
    #[test]
    fn layout_width_matches_cell_count_for_individual_chars() {
        let text_system = platform_text_system();
        let font_id = text_system.resolve_font(&Font::default());

        let chars = ['a', '0', ' ', '日', '👍', '\u{0301}', '\t', '\u{0}'];
        for size_px in [6.0f32, 16.0, 100.0] {
            let font_size = px(size_px);
            let cell_width = cell::cell_width_at(font_size);
            for &ch in &chars {
                let expected = ch.width().unwrap_or(0) as f32 * cell_width;
                let actual = text_system.layout_width(font_id, font_size, ch);
                assert_eq!(actual, expected, "layout_width mismatch for {ch:?} at {size_px}px");
            }
        }
    }

    #[test]
    fn empty_string_has_zero_width_and_no_glyphs() {
        let system = CellTextSystem::new();
        for size_px in [6.0f32, 16.0, 100.0] {
            let layout = system.layout_line("", px(size_px), &[]);
            assert_eq!(layout.width, px(0.0));
            assert!(layout.runs.is_empty());
            assert_eq!(layout.len, 0);
        }
    }

    /// The run-partition contract: one `ShapedRun` per byte range, in the
    /// order the caller gave them, tagged with that range's `font_id`.
    #[test]
    fn layout_line_honours_font_run_byte_partition() {
        let system = CellTextSystem::new();
        let text = "abXYZ";
        let font_size = px(16.0);

        let font_id_a = system.font_id(&Font::default()).expect("font_id");
        let font_id_b = system.font_id(&font("other-family")).expect("font_id");

        let runs = [
            FontRun {
                len: 2,
                font_id: font_id_a,
            },
            FontRun {
                len: 3,
                font_id: font_id_b,
            },
        ];

        let layout = system.layout_line(text, font_size, &runs);
        assert_eq!(layout.runs.len(), 2);
        assert_eq!(layout.runs[0].font_id, font_id_a);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        assert_eq!(layout.runs[1].font_id, font_id_b);
        assert_eq!(layout.runs[1].glyphs.len(), 3);
    }

    #[test]
    fn glyph_for_char_never_fails_including_control_and_zero_width_chars() {
        let system = CellTextSystem::new();
        let font_id = system.font_id(&Font::default()).expect("font_id");
        for ch in ['a', '\u{0}', '\u{0301}', '\t', '\u{200D}', '👍'] {
            assert!(system.glyph_for_char(font_id, ch).is_some());
        }
    }
}
