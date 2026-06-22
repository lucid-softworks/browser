//! A [`paint::GlyphRasterizer`] backed by `fontdue` plus a system TTF.
//!
//! `fontdue` is a reused crate (pure-Rust TrueType rasterization). It sits behind the
//! `GlyphRasterizer` trait so the eventual hand-written rasterizer is a drop-in swap —
//! nothing outside this file knows fontdue exists.

use paint::{GlyphBitmap, GlyphRasterizer};

/// Candidate single-file TTFs, tried in order, per OS. Prefer modern fonts with a proper UNICODE
/// cmap: legacy fonts (e.g. macOS Monaco/Geneva) carry a Mac-Roman cmap where byte 0xB7 is `∑`, so
/// `·` (U+00B7) and most non-ASCII glyphs (é, α, →, €…) map to the WRONG glyph. Monospace first to
/// keep the look, then broad-coverage fallbacks. (A future upgrade is `font-kit` for true system
/// font enumeration; this fixed list is dependency-free and covers the common installs.)
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "linux")]
const FONT_CANDIDATES: &[&str] = &[
    // Debian/Ubuntu layout, then Fedora/Arch (/usr/share/fonts/{TTF,...}).
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
];

#[cfg(target_os = "windows")]
const FONT_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\consola.ttf", // Consolas (monospace)
    r"C:\Windows\Fonts\lucon.ttf",   // Lucida Console
    r"C:\Windows\Fonts\segoeui.ttf", // Segoe UI
    r"C:\Windows\Fonts\arial.ttf",
    r"C:\Windows\Fonts\arialuni.ttf", // Arial Unicode MS (broad coverage, if installed)
    r"C:\Windows\Fonts\tahoma.ttf",
];

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const FONT_CANDIDATES: &[&str] = &[];

pub struct SystemFont {
    font: fontdue::Font,
}

impl SystemFont {
    /// Load the first available system font. Returns `None` if none could be read/parsed.
    pub fn load() -> Option<Self> {
        for path in FONT_CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                return Some(Self { font });
            }
        }
        None
    }

    /// Load a font from in-memory TrueType/OpenType bytes (a fetched `@font-face` `src`). Returns
    /// `None` if the bytes aren't a font fontdue can parse (e.g. `woff`/`woff2`, which are
    /// compressed wrappers we don't decode).
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .ok()
            .map(|font| Self { font })
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
