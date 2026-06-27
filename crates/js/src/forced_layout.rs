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
struct Measurer {
    primary: Option<fontdue::Font>,
    fallback: Vec<fontdue::Font>,
}

impl Measurer {
    fn load() -> Self {
        let opts = fontdue::FontSettings::default();
        let primary = FONT_CANDIDATES
            .iter()
            .filter_map(|p| std::fs::read(p).ok())
            .find_map(|b| fontdue::Font::from_bytes(b, opts).ok());
        let fallback = FALLBACK_CANDIDATES
            .iter()
            .filter_map(|p| std::fs::read(p).ok())
            .filter_map(|b| fontdue::Font::from_bytes(b, opts).ok())
            .collect();
        Self { primary, fallback }
    }

    fn advance(&self, ch: char, px: f32) -> f32 {
        let Some(primary) = &self.primary else {
            // No usable system font: approximate so layout is still non-degenerate.
            return px * 0.5;
        };
        if primary.lookup_glyph_index(ch) != 0 {
            return primary.metrics(ch, px).advance_width;
        }
        for fb in &self.fallback {
            if fb.lookup_glyph_index(ch) != 0 {
                return fb.metrics(ch, px).advance_width;
            }
        }
        primary.metrics(ch, px).advance_width
    }
}

impl layout::TextMeasurer for Measurer {
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

thread_local! {
    // Loaded once per Session worker thread (fonts are immutable).
    static MEASURER: Measurer = Measurer::load();
}

/// If the DOM has changed since the rects in `layout_rects` were computed, recompute the box tree
/// from the live document and refresh `layout_rects` (+ document height). Cheap no-op when clean.
pub(crate) fn ensure_layout_fresh(state: &HostState) {
    if state.dom_version.get() == state.rects_dom_version.get() {
        return;
    }
    let (vw, vh, _dpr) = crate::eval_loop::device_metrics();
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
        MEASURER.with(|m| {
            let root =
                layout::layout_document(doc, styles, vw as f32, vh as f32, m, &intrinsics, None);
            let mut rects = state.layout_rects.borrow_mut();
            rects.clear();
            collect_rects(&root, &mut rects);
            state.doc_height.set(root.dimensions.border_box().height);
        });
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
