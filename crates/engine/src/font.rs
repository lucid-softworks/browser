//! A [`paint::GlyphRasterizer`] backed by `fontdue` plus a system TTF.
//!
//! `fontdue` is a reused crate (pure-Rust TrueType rasterization). It sits behind the
//! `GlyphRasterizer` trait so the eventual hand-written rasterizer is a drop-in swap —
//! nothing outside this file knows fontdue exists.

use paint::{GlyphBitmap, GlyphRasterizer};

/// Candidate single-file TTFs shipped with macOS, tried in order. Prefer modern fonts with a
/// proper UNICODE cmap: legacy Mac fonts like Monaco/Geneva carry a Mac-Roman cmap where byte
/// 0xB7 is `∑`, so `·` (U+00B7) and most non-ASCII glyphs (é, α, →, €…) map to the WRONG glyph.
/// SF Mono keeps a monospace look; San Francisco / Arial Unicode are broad-coverage fallbacks.
const FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    // Legacy single-file fonts (Mac-Roman cmap) — last resort only.
    "/System/Library/Fonts/Monaco.ttf",
    "/System/Library/Fonts/Geneva.ttf",
    "/System/Library/Fonts/NewYork.ttf",
];

pub struct SystemFont {
    font: fontdue::Font,
}

impl SystemFont {
    /// Load the first available system font. Returns `None` if none could be read/parsed.
    pub fn load() -> Option<Self> {
        for path in FONT_CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else { continue };
            if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return Some(Self { font });
            }
        }
        None
    }
}

impl GlyphRasterizer for SystemFont {
    fn rasterize(&self, ch: char, px: f32) -> Option<GlyphBitmap> {
        let (m, coverage) = self.font.rasterize(ch, px);
        if m.width == 0 || m.height == 0 {
            return None;
        }
        Some(GlyphBitmap {
            width: m.width,
            height: m.height,
            // fontdue gives offsets relative to the baseline / pen; convert to a top-left
            // origin: `top` is how far above the baseline the bitmap's first row sits.
            left: m.xmin,
            top: -(m.ymin + m.height as i32),
            advance: m.advance_width,
            coverage,
        })
    }

    fn advance(&self, ch: char, px: f32) -> f32 {
        self.font.metrics(ch, px).advance_width
    }
}
