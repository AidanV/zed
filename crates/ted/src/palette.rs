//! Theme colours to terminal colours (SPEC §12).
//!
//! Zed themes are `Hsla` with real alpha; a terminal cell is one opaque
//! foreground and one opaque background out of 2^24, 256 or 16. Two things
//! follow, and both are handled here rather than at any call site: translucent
//! colours (selection tints, current-line highlights) are composited against the
//! editor background before conversion, and the conversion itself is cached,
//! because it runs once per span per frame.

use std::cell::RefCell;

use collections::HashMap;
use gpui::{Hsla, Rgba};
use ratatui::style::Color;

/// What the terminal can express, detected from the environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit direct colour.
    TrueColor,
    /// The xterm 256-colour palette: 16 ANSI, a 6×6×6 cube, a 24-step grey ramp.
    Indexed256,
    /// The 16 ANSI colours only.
    Ansi16,
}

impl ColorDepth {
    /// `COLORTERM` is the only reliable truecolor signal; `TERM` naming is the
    /// long-standing convention for the 256-colour tier.
    pub fn detect() -> Self {
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return Self::TrueColor;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256color") || term.contains("direct") {
            return Self::Indexed256;
        }
        if term.is_empty() || term == "dumb" {
            return Self::Ansi16;
        }
        Self::Ansi16
    }
}

pub struct Palette {
    depth: ColorDepth,
    /// The opaque colour translucent theme colours are composited over. Zed's
    /// selection and current-line highlights are semi-transparent by design, so
    /// without this they would resolve to a colour the theme never intended.
    backdrop: Rgba,
    /// When false the editor surface emits no background at all, so `ted`
    /// composes with a transparent terminal instead of fighting the user's
    /// colour scheme at the edges (SPEC §12).
    opaque_background: bool,
    cache: RefCell<HashMap<[u32; 4], Color>>,
}

impl Palette {
    pub fn new(depth: ColorDepth, backdrop: Hsla, opaque_background: bool) -> Self {
        Self {
            depth,
            backdrop: opaque(Rgba::from(backdrop)),
            opaque_background,
            cache: RefCell::new(HashMap::default()),
        }
    }

    pub fn depth(&self) -> ColorDepth {
        self.depth
    }

    pub fn opaque_background(&self) -> bool {
        self.opaque_background
    }

    /// Points the palette at the background the next frame will be projected
    /// over, and forgets every colour resolved against the old one.
    ///
    /// The backdrop cannot be captured once at startup. `ted` opens its window
    /// after the app exists, and `Workspace` re-reads the system appearance
    /// from that window and reloads the theme when it does
    /// (`workspace.rs`'s `observe_window_appearance`) — so the theme the first
    /// frame is projected from is routinely *not* the one that was global when
    /// the palette was built. `settings.json` is watched too, so the user can
    /// change it again at any point. Compositing over a stale backdrop is not a
    /// subtle error: a 10%-alpha tint resolved over a light background when the
    /// theme is dark comes out near-white, which is a colour the theme never
    /// specified.
    pub fn set_backdrop(&mut self, backdrop: Hsla) {
        let backdrop = opaque(Rgba::from(backdrop));
        if self.backdrop == backdrop {
            return;
        }
        self.backdrop = backdrop;
        self.cache.get_mut().clear();
    }

    /// The terminal colour for a theme colour, composited and quantised.
    pub fn color(&self, color: Hsla) -> Color {
        let key = [
            color.h.to_bits(),
            color.s.to_bits(),
            color.l.to_bits(),
            color.a.to_bits(),
        ];
        if let Some(&cached) = self.cache.borrow().get(&key) {
            return cached;
        }

        let resolved = self.quantize(composite(Rgba::from(color), self.backdrop));
        self.cache.borrow_mut().insert(key, resolved);
        resolved
    }

    /// The background for the editor surface: `None` leaves the terminal's own
    /// background showing through.
    pub fn surface_background(&self, color: Hsla) -> Option<Color> {
        self.opaque_background.then(|| self.color(color))
    }

    fn quantize(&self, color: Rgba) -> Color {
        let (red, green, blue) = to_bytes(color);
        match self.depth {
            ColorDepth::TrueColor => Color::Rgb(red, green, blue),
            ColorDepth::Indexed256 => Color::Indexed(nearest_index(color, 0..256)),
            ColorDepth::Ansi16 => Color::Indexed(nearest_index(color, 0..16)),
        }
    }
}

/// Composites `color` over `backdrop`, both straight-alpha. Backgrounds in Zed
/// themes routinely have alpha; a terminal cell does not.
fn composite(color: Rgba, backdrop: Rgba) -> Rgba {
    if color.a >= 1.0 {
        return opaque(color);
    }
    let alpha = color.a.clamp(0.0, 1.0);
    Rgba {
        r: color.r * alpha + backdrop.r * (1.0 - alpha),
        g: color.g * alpha + backdrop.g * (1.0 - alpha),
        b: color.b * alpha + backdrop.b * (1.0 - alpha),
        a: 1.0,
    }
}

fn opaque(color: Rgba) -> Rgba {
    Rgba { a: 1.0, ..color }
}

fn to_bytes(color: Rgba) -> (u8, u8, u8) {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    (channel(color.r), channel(color.g), channel(color.b))
}

/// The palette entry closest to `color`, measured in Oklab rather than sRGB so
/// that perceptually distinct syntax colours stay distinct after quantisation —
/// sRGB distance collapses mid-lightness hues into each other (SPEC §12).
fn nearest_index(color: Rgba, candidates: std::ops::Range<u16>) -> u8 {
    let target = oklab(color);
    let mut best = 0u8;
    let mut best_distance = f32::MAX;
    for index in candidates {
        let Ok(index) = u8::try_from(index) else {
            break;
        };
        let candidate = oklab(xterm_color(index));
        let distance = (candidate.0 - target.0).powi(2)
            + (candidate.1 - target.1).powi(2)
            + (candidate.2 - target.2).powi(2);
        if distance < best_distance {
            best_distance = distance;
            best = index;
        }
    }
    best
}

/// The standard xterm rendering of palette index `index`. The first 16 entries
/// are whatever the user's terminal defines them to be; these are the xterm
/// defaults, used only as a stand-in for measuring distance.
fn xterm_color(index: u8) -> Rgba {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

    let (red, green, blue) = match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let offset = index as usize - 16;
            (CUBE[offset / 36], CUBE[(offset / 6) % 6], CUBE[offset % 6])
        }
        232.. => {
            let level = 8 + 10 * (index as u16 - 232);
            let level = level.min(255) as u8;
            (level, level, level)
        }
    };

    Rgba {
        r: red as f32 / 255.0,
        g: green as f32 / 255.0,
        b: blue as f32 / 255.0,
        a: 1.0,
    }
}

/// sRGB to Oklab (Björn Ottosson's transform), for perceptual distance.
fn oklab(color: Rgba) -> (f32, f32, f32) {
    let linear = |channel: f32| {
        let channel = channel.clamp(0.0, 1.0);
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    let (red, green, blue) = (linear(color.r), linear(color.g), linear(color.b));

    let long = 0.4122214708 * red + 0.5363325363 * green + 0.0514459929 * blue;
    let medium = 0.2119034982 * red + 0.6806995451 * green + 0.1073969566 * blue;
    let short = 0.0883024619 * red + 0.2817188376 * green + 0.6299787005 * blue;

    let long = long.cbrt();
    let medium = medium.cbrt();
    let short = short.cbrt();

    (
        0.2104542553 * long + 0.7936177850 * medium - 0.0040720468 * short,
        1.9779984951 * long - 2.4285922050 * medium + 0.4505937099 * short,
        0.0259040371 * long + 0.7827717662 * medium - 0.8086757660 * short,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;

    fn truecolor() -> Palette {
        Palette::new(ColorDepth::TrueColor, hsla(0.0, 0.0, 0.0, 1.0), true)
    }

    #[test]
    fn truecolor_is_exact() {
        let palette = truecolor();
        // Pure red at full saturation and half lightness.
        assert_eq!(
            palette.color(hsla(0.0, 1.0, 0.5, 1.0)),
            Color::Rgb(255, 0, 0)
        );
        assert_eq!(
            palette.color(hsla(0.0, 0.0, 1.0, 1.0)),
            Color::Rgb(255, 255, 255)
        );
    }

    #[test]
    fn translucent_colors_composite_over_the_backdrop() {
        let palette = Palette::new(ColorDepth::TrueColor, hsla(0.0, 0.0, 0.0, 1.0), true);
        // Half-opaque white over black is mid grey, not white.
        let Color::Rgb(red, green, blue) = palette.color(hsla(0.0, 0.0, 1.0, 0.5)) else {
            panic!("expected a truecolor value");
        };
        assert_eq!((red, green, blue), (128, 128, 128));
    }

    /// A translucent colour resolved before the theme settled must not survive
    /// the theme changing, which it would if the cache outlived the backdrop.
    #[test]
    fn a_new_backdrop_forgets_the_colours_resolved_against_the_old_one() {
        let mut palette = Palette::new(ColorDepth::TrueColor, hsla(0.0, 0.0, 1.0, 1.0), true);
        let translucent = hsla(0.0, 0.0, 0.0, 0.5);
        // Half-opaque black over white, and cached at that value.
        assert_eq!(palette.color(translucent), Color::Rgb(128, 128, 128));

        palette.set_backdrop(hsla(0.0, 0.0, 0.0, 1.0));
        assert_eq!(
            palette.color(translucent),
            Color::Rgb(0, 0, 0),
            "the colour was still composited over the old backdrop"
        );

        // An opaque colour is unaffected by either backdrop, so re-resolving it
        // costs nothing and says nothing — the point is only that the cache
        // survives a backdrop that did not actually change.
        let before = palette.color(hsla(0.5, 0.5, 0.5, 1.0));
        palette.set_backdrop(hsla(0.0, 0.0, 0.0, 1.0));
        assert_eq!(palette.color(hsla(0.5, 0.5, 0.5, 1.0)), before);
    }

    #[test]
    fn quantisation_lands_on_the_exact_palette_entry_when_one_matches() {
        let palette = Palette::new(ColorDepth::Indexed256, hsla(0.0, 0.0, 0.0, 1.0), true);
        // 0xFF0000 is index 196 in the 6x6x6 cube, and index 9 in the ANSI
        // block; both are exactly the same colour, so the first one wins.
        assert_eq!(palette.color(hsla(0.0, 1.0, 0.5, 1.0)), Color::Indexed(9));
    }

    #[test]
    fn sixteen_colors_stay_within_the_ansi_block() {
        let palette = Palette::new(ColorDepth::Ansi16, hsla(0.0, 0.0, 0.0, 1.0), true);
        for hue in 0..12 {
            let color = palette.color(hsla(hue as f32 / 12.0, 0.8, 0.5, 1.0));
            let Color::Indexed(index) = color else {
                panic!("expected an indexed colour, got {color:?}");
            };
            assert!(index < 16, "index {index} is outside the ANSI block");
        }
    }

    #[test]
    fn a_transparent_surface_emits_no_background() {
        let palette = Palette::new(ColorDepth::TrueColor, hsla(0.0, 0.0, 0.0, 1.0), false);
        assert_eq!(palette.surface_background(hsla(0.0, 0.0, 0.1, 1.0)), None);
    }

    #[test]
    fn repeated_lookups_are_stable() {
        let palette = truecolor();
        let color = hsla(0.3, 0.5, 0.4, 0.75);
        assert_eq!(palette.color(color), palette.color(color));
    }
}
