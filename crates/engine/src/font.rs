//! A [`paint::GlyphRasterizer`] backed by `fontdue` plus a system TTF.
//!
//! `fontdue` is a reused crate (pure-Rust TrueType rasterization). It sits behind the
//! `GlyphRasterizer` trait so the eventual hand-written rasterizer is a drop-in swap —
//! nothing outside this file knows fontdue exists.

use paint::{GlyphBitmap, GlyphRasterizer};

/// Candidate single-file TTFs for the DEFAULT (proportional sans-serif) UA font, tried in order, per
/// OS. The web overwhelmingly uses proportional sans/serif text, so the default must be proportional
/// — a monospace default makes every page (e.g. wikipedia.org) render fixed-width. Prefer modern
/// fonts with a proper UNICODE cmap: legacy fonts (e.g. macOS Monaco/Geneva) carry a Mac-Roman cmap
/// where byte 0xB7 is `∑`, so `·` (U+00B7) and most non-ASCII glyphs map to the WRONG glyph.
/// (A future upgrade is `font-kit` for true system font enumeration + per-`font-family` serif/
/// monospace selection; this fixed list is dependency-free.)
#[cfg(target_os = "macos")]
const FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial.ttf", // proportional sans (reliable single-file TTF)
    "/System/Library/Fonts/SFNS.ttf",               // San Francisco (proportional), if parseable
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/System/Library/Fonts/Geneva.ttf",
    "/System/Library/Fonts/SFNSMono.ttf", // monospace — only if no proportional face is available
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

/// Broad-coverage fonts consulted, in order, for glyphs the primary font lacks (CJK, Cyrillic,
/// Greek, Arabic, Hebrew, symbols, …). Without this, non-Latin text on e.g. wikipedia.org renders
/// as blanks/`.notdef`, since the primary monospace/Latin face has no glyphs for those scripts.
/// `.ttc` collections load face 0, which is the regular weight for these families.
#[cfg(target_os = "macos")]
const FALLBACK_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf", // huge coverage (Latin/Greek/Cyrillic/CJK/Arabic/Hebrew…)
    "/System/Library/Fonts/PingFang.ttc",                   // CJK (Simplified/Traditional Chinese)
    "/System/Library/Fonts/Hiragino Sans GB.ttc",           // CJK fallback
    "/System/Library/Fonts/Apple Symbols.ttf",              // math/misc symbols
    "/System/Library/Fonts/Supplemental/Arial.ttf",
];

#[cfg(target_os = "linux")]
const FALLBACK_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

#[cfg(target_os = "windows")]
const FALLBACK_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\arialuni.ttf", // Arial Unicode MS (broad), if installed
    r"C:\Windows\Fonts\msyh.ttc",     // Microsoft YaHei (CJK)
    r"C:\Windows\Fonts\msgothic.ttc", // MS Gothic (CJK)
    r"C:\Windows\Fonts\seguisym.ttf", // Segoe UI Symbol
    r"C:\Windows\Fonts\arial.ttf",
];

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const FALLBACK_CANDIDATES: &[&str] = &[];

/// The shared, lazily-loaded fallback font chain (parsed once for the whole process). Web-font
/// faces and the system font both consult it for glyphs they lack.
fn fallback_fonts() -> &'static [fontdue::Font] {
    use std::sync::OnceLock;
    static FONTS: OnceLock<Vec<fontdue::Font>> = OnceLock::new();
    FONTS.get_or_init(|| {
        FALLBACK_CANDIDATES
            .iter()
            .filter_map(|path| std::fs::read(path).ok())
            .filter_map(|bytes| {
                fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
            })
            .collect()
    })
}

pub struct SystemFont {
    font: fontdue::Font,
}

/// The set of fonts available while measuring/painting a page: the system fallback face plus any
/// loaded `@font-face` web fonts (keyed by lowercased family name). [`pick`](Fonts::pick) resolves a
/// computed `font-family` list to the face to draw a run with — the same selection layout uses to
/// MEASURE text, so the painter draws with the very font the run was sized for (without this, a run
/// in a web font is measured in that font but painted in the system font: wrong glyphs).
#[derive(Clone, Copy)]
pub(crate) struct Fonts<'a> {
    pub(crate) system: &'a dyn GlyphRasterizer,
    pub(crate) faces: &'a std::collections::HashMap<String, SystemFont>,
}

impl<'a> Fonts<'a> {
    /// Pick the loaded font for a computed `font-family` list: the first comma-separated family that
    /// names a loaded `@font-face` web font, else the system font.
    pub(crate) fn pick(&self, family: Option<&str>) -> &'a dyn GlyphRasterizer {
        if let Some(list) = family {
            for fam in list.split(',') {
                let name = fam.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
                if let Some(face) = self.faces.get(&name) {
                    return face;
                }
            }
        }
        self.system
    }
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

    /// Load a font from in-memory `@font-face` `src` bytes. Bare TrueType/OpenType is parsed
    /// directly; a WOFF2 (`wOF2`) wrapper is first decoded to SFNT (see [`crate::woff2`]) — a
    /// MALFORMED WOFF2 fails to decode and yields `None`, so the caller falls back to the next
    /// source (matching the WOFF2 conformance reftests). Legacy `woff` (1.0) is not decoded.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        let bytes = if bytes.starts_with(b"wOF2") {
            crate::woff2::decode(&bytes)?
        } else {
            bytes
        };
        fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .ok()
            .map(|font| Self { font })
    }

    /// Pick the font to draw `ch` with: this face if it has the glyph, otherwise the first fallback
    /// font that does (so non-Latin text falls back to a broad-coverage face). Falls back to this
    /// face when nothing has the glyph (it then renders `.notdef`).
    fn font_for(&self, ch: char) -> &fontdue::Font {
        if self.font.lookup_glyph_index(ch) != 0 {
            return &self.font;
        }
        for f in fallback_fonts() {
            if f.lookup_glyph_index(ch) != 0 {
                return f;
            }
        }
        &self.font
    }
}

impl GlyphRasterizer for SystemFont {
    fn rasterize(&self, ch: char, px: f32) -> Option<GlyphBitmap> {
        let (m, coverage) = self.font_for(ch).rasterize(ch, px);
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
        self.font_for(ch).metrics(ch, px).advance_width
    }
}
