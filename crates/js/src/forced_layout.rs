//! Synchronous, in-Session layout for geometry reads after a script DOM mutation.
//!
//! The engine lays out on its own thread and is BLOCKED while page JS runs, so an element a script
//! just created/mutated has no engine-pushed rect until the next render — `getBoundingClientRect`,
//! `getClientRects`, `offset*`, and `elementsFromPoint` would all read 0. That breaks anything that
//! measures freshly-mutated DOM, notably `test_driver.bless` (it appends a button and clicks it in
//! the same task — the click is "intercepted" because the button has no box).
//!
//! We already run the CSS cascade in-Session for `getComputedStyle` (same blocked-engine reason);
//! this does the same for LAYOUT. When the DOM version has advanced past the rects we last computed,
//! a geometry read recomputes the box tree here (cascade → `layout::layout_document`) using the same
//! system fonts the engine paints with, and refreshes `layout_rects`. When nothing changed since the
//! engine's last push, the engine's (authoritative) rects are served unchanged — no regression.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::HostState;

// Mirror the engine's font candidate lists so in-Session measurement matches the painter's metrics.
#[cfg(target_os = "macos")]
const FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/System/Library/Fonts/Geneva.ttf",
    "/System/Library/Fonts/SFNSMono.ttf",
];
#[cfg(target_os = "linux")]
const FONT_CANDIDATES: &[&str] = &[
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
    r"C:\Windows\Fonts\consola.ttf",
    r"C:\Windows\Fonts\lucon.ttf",
    r"C:\Windows\Fonts\segoeui.ttf",
    r"C:\Windows\Fonts\arial.ttf",
    r"C:\Windows\Fonts\arialuni.ttf",
    r"C:\Windows\Fonts\tahoma.ttf",
];
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const FONT_CANDIDATES: &[&str] = &[];

#[cfg(target_os = "macos")]
const FALLBACK_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/Apple Symbols.ttf",
];
#[cfg(target_os = "linux")]
const FALLBACK_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];
#[cfg(target_os = "windows")]
const FALLBACK_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\arialuni.ttf",
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\seguisym.ttf",
];
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const FALLBACK_CANDIDATES: &[&str] = &[];

/// A `TextMeasurer` over the system font + a broad-coverage fallback chain. Mirrors the engine's
/// `FontMeasurer` (advance-sum + ~1px/glyph faux-bold, 1.3× line height) so geometry agrees.
struct Measurer<'a> {
    primary: Option<Arc<fontdue::Font>>,
    fonts: &'a FontCache,
}

struct FontSlot {
    path: &'static str,
    font: OnceLock<Option<fontdue::Font>>,
}

/// Immutable font data is shared between JS workers. Resolve it only when a glyph needs it:
/// parsing macOS's large CJK fallback fonts for every layout used to consume the JS deadline
/// even for empty documents and ASCII text rendered with an already supplied host font.
struct FontCache {
    primary_candidates: &'static [&'static str],
    primary: OnceLock<Option<Arc<fontdue::Font>>>,
    fallback: Vec<FontSlot>,
}

impl FontCache {
    fn new(primary_candidates: &'static [&'static str], fallback: &'static [&'static str]) -> Self {
        Self {
            primary_candidates,
            primary: OnceLock::new(),
            fallback: fallback
                .iter()
                .map(|path| FontSlot {
                    path,
                    font: OnceLock::new(),
                })
                .collect(),
        }
    }

    fn primary(&self) -> Option<&fontdue::Font> {
        self.primary
            .get_or_init(|| {
                self.primary_candidates
                    .iter()
                    .find_map(|path| load_font(path))
                    .map(Arc::new)
            })
            .as_deref()
    }
}

fn load_font(path: &str) -> Option<fontdue::Font> {
    let bytes = std::fs::read(path).ok()?;
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
}

impl Measurer<'_> {
    fn advance(&self, ch: char, px: f32) -> f32 {
        let Some(primary) = self.primary.as_deref().or_else(|| self.fonts.primary()) else {
            // No usable system font: approximate so layout is still non-degenerate.
            return px * 0.5;
        };
        if primary.lookup_glyph_index(ch) != 0 {
            return primary.metrics(ch, px).advance_width;
        }
        for slot in &self.fonts.fallback {
            let Some(fb) = slot.font.get_or_init(|| load_font(slot.path)) else {
                continue;
            };
            if fb.lookup_glyph_index(ch) != 0 {
                return fb.metrics(ch, px).advance_width;
            }
        }
        primary.metrics(ch, px).advance_width
    }
}

impl layout::TextMeasurer for Measurer<'_> {
    fn text_width(&self, text: &str, px: f32, bold: bool, _family: Option<&str>) -> f32 {
        let mut w: f32 = text.chars().map(|c| self.advance(c, px)).sum();
        if bold {
            w += text.chars().count() as f32;
        }
        w
    }
    fn line_height(&self, px: f32, _family: Option<&str>) -> f32 {
        px * 1.3
    }
}

fn font_cache() -> &'static FontCache {
    static FONTS: OnceLock<FontCache> = OnceLock::new();
    FONTS.get_or_init(|| FontCache::new(FONT_CANDIDATES, FALLBACK_CANDIDATES))
}

/// If the DOM has changed since the rects in `layout_rects` were computed, recompute the box tree
/// from the live document and refresh `layout_rects` (+ document height). Cheap no-op when clean.
pub(crate) fn ensure_layout_fresh(state: &HostState) {
    crate::sync_style_context(state);
    if state.dom_version.get() == state.rects_dom_version.get() {
        return;
    }
    let (vw, vh, _dpr) = crate::device_metrics();
    if vw <= 0.0 || vh <= 0.0 {
        return;
    }
    let intrinsics: HashMap<dom::NodeId, (f32, f32)> = state
        .image_natural
        .borrow()
        .iter()
        .map(|(&k, &v)| (dom::NodeId(k), v))
        .collect();
    crate::style_query::with_cascade_map(state, |doc, styles| {
        // Read the active engine's font for each layout instead of retaining another engine's
        // override in thread-local state. A supplied font bypasses system-primary discovery.
        let measurer = Measurer {
            primary: crate::host_context::font(),
            fonts: font_cache(),
        };
        let root = layout::layout_document(
            doc,
            styles,
            vw as f32,
            vh as f32,
            &measurer,
            &intrinsics,
            None,
        );
        let mut rects = state.layout_rects.borrow_mut();
        rects.clear();
        collect_rects(&root, &mut rects);
        state.doc_height.set(root.dimensions.border_box().height);
    });
    state.rects_dom_version.set(state.dom_version.get());
}

fn collect_rects(b: &layout::LayoutBox, out: &mut HashMap<usize, (f32, f32, f32, f32)>) {
    if let Some(node) = b.node {
        out.entry(node.0).or_insert_with(|| {
            let r = b.dimensions.border_box();
            (r.x, r.y, r.width, r.height)
        });
    }
    for c in &b.children {
        collect_rects(c, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layout::TextMeasurer;

    fn supplied_font() -> Arc<fontdue::Font> {
        static FONT: OnceLock<Arc<fontdue::Font>> = OnceLock::new();
        FONT.get_or_init(|| {
            Arc::new(
                fontdue::Font::from_bytes(
                    include_bytes!("../../desktop/assets/Inter-Regular.ttf") as &[u8],
                    fontdue::FontSettings::default(),
                )
                .unwrap(),
            )
        })
        .clone()
    }

    fn fresh_cache() -> FontCache {
        // Local caches keep these assertions independent of other tests and installed fonts.
        FontCache::new(&["unused primary font"], &["unused fallback font"])
    }

    #[test]
    fn empty_text_and_line_height_do_not_load_fonts() {
        let fonts = fresh_cache();
        let measurer = Measurer {
            primary: None,
            fonts: &fonts,
        };
        assert_eq!(measurer.text_width("", 16.0, false, None), 0.0);
        assert_eq!(measurer.line_height(16.0, None), 16.0 * 1.3);
        assert!(fonts.primary.get().is_none());
        assert!(fonts.fallback[0].font.get().is_none());
    }

    #[test]
    fn supplied_ascii_font_bypasses_system_and_fallback_loading() {
        let fonts = fresh_cache();
        let font = supplied_font();
        assert_ne!(font.lookup_glyph_index('A'), 0);
        let expected = font.metrics('A', 16.0).advance_width;
        let measurer = Measurer {
            primary: Some(font),
            fonts: &fonts,
        };
        assert_eq!(measurer.text_width("A", 16.0, false, None), expected);
        assert!(fonts.primary.get().is_none());
        assert!(fonts.fallback[0].font.get().is_none());
    }

    #[test]
    fn cached_primary_is_shared_between_workers_without_loading_fallbacks() {
        let fonts = fresh_cache();
        let font = supplied_font();
        let expected = font.metrics('A', 16.0).advance_width;
        assert!(fonts.primary.set(Some(font)).is_ok());
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let fonts = &fonts;
                scope.spawn(move || {
                    let measurer = Measurer {
                        primary: None,
                        fonts,
                    };
                    assert_eq!(measurer.text_width("A", 16.0, false, None), expected);
                });
            }
        });
        assert!(fonts.fallback[0].font.get().is_none());
    }
}
