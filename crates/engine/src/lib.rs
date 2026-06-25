//! The browser engine: owns the pipeline state and produces a painted framebuffer.
//!
//! Phase 0/1 scope: fetch a URL (via `net`), remember the result, and paint a status
//! screen — a computed gradient plus real text rendered by our compositor. The full
//! parse → style → layout → paint pipeline lands in later phases; the function boundaries
//! (`html::parse`, `style`, `layout`) already exist as stubs so wiring them in is additive.

mod canvas;
mod font;
mod svg;
mod woff2;

pub(crate) use std::collections::HashMap;
use std::time::Instant;

use font::{Fonts, SystemFont};
use paint::{Color, Framebuffer, GlyphRasterizer, Rect};

/// A borrowed, C-ABI view of the engine's RGBA8 (straight-alpha) framebuffer, handed to the
/// progressive-load frame callback. `pixels` points at the engine's own buffer and is valid ONLY
/// for the duration of the callback call (the engine reuses/reallocates it on the next paint), so a
/// callback must copy synchronously. A null `pixels` means "nothing painted".
///
/// Layout matches the FFI crate's `Framebuffer` struct exactly (same field order/types) so the FFI
/// layer can forward callbacks without conversion.
#[repr(C)]
pub struct FrameView {
    pub pixels: *const u8,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// A progressive-load frame callback: invoked synchronously (on the load thread) with a borrowed
/// [`FrameView`] each time a new partial/final frame is painted during `load_url`. The opaque
/// `ctx` pointer is passed through unchanged.
pub type FrameCallback = extern "C" fn(*mut std::ffi::c_void, FrameView);

/// Minimum wall-clock gap between progressive partial paints during a streaming load. Bounds the
/// re-cascade/layout/paint cost so a fast multi-chunk download doesn't spend all its time painting
/// intermediate frames.
const PARTIAL_PAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(30);

/// A decoded raster image ready to blit: straight-alpha RGBA8 pixels plus dimensions.
#[derive(Clone)]
struct DecodedImage {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

/// Maximum number of images fetched + decoded per page; the rest are skipped.
const MAX_IMAGES: usize = 24;
/// Maximum images fetched concurrently. Kept low so a page's image burst doesn't trip a CDN's
/// per-client rate limit (e.g. Wikimedia 429s aggressive bursts).
const MAX_CONCURRENT_IMAGE_FETCHES: usize = 5;
/// Skip decoding images whose decoded pixel area would exceed this (guards memory / time).
const MAX_IMAGE_PIXELS: u64 = 32 * 1024 * 1024; // ~32 megapixels

/// Result of the most recent navigation.
enum LoadState {
    Empty,
    Loaded {
        url: String,
        /// Parsed DOM, present when the response was HTML.
        doc: Option<dom::Document>,
        /// Author stylesheets parsed from the page's `<style>` elements, in document order.
        styles: Vec<css::Stylesheet>,
        /// Console output (and error lines) produced by running the page's inline scripts.
        console: Vec<String>,
        /// Decoded `<img>` images keyed by their DOM node, fetched on the last navigation.
        images: HashMap<dom::NodeId, DecodedImage>,
    },
    Failed {
        url: String,
        error: String,
    },
}

/// Cached cascade+layout result, reused across renders when only the scroll offset changes.
/// Invalidated on navigation (`load_url`) and when the device viewport size changes.
struct LayoutCache {
    dw: u32,
    dh: u32,
    root: layout::LayoutBox,
    content_h: f32,
    /// The cascade's computed styles (kept so SVG rasterization can read inline `<svg>` shapes'
    /// CSS `fill`/`stroke` and forced-colors state, which aren't carried on the layout tree).
    styles: std::collections::HashMap<dom::NodeId, style::ComputedStyle>,
    /// The page's resolved *used* color scheme (true = dark), captured during the cascade that
    /// produced this layout (see `style::cascade_with_root_scheme`). Drives the default canvas
    /// background when no html/body `background-color` is set. Stored here (rather than re-read from
    /// the process-global at paint time) so a concurrent cascade can't flip it under us.
    root_scheme_dark: bool,
}

/// The result of a click landing on (or inside) a `<select>` control: enough for the platform
/// shell to pop up a native dropdown menu and report back the chosen index. The rect (`x`/`y`/
/// `width`/`height`) is in DEVICE pixels, viewport-relative (scroll already subtracted) — the
/// caller divides by the backing scale to get points. `selected` is the 0-based index of the
/// currently-selected option in `options`.
/// A point in DOCUMENT space: device pixels, top-origin, with the page scroll offset already
/// folded in (i.e. the same space the layout tree's absolute rects live in, and what
/// `dispatch_click` hit-tests against as `x`, `y + scroll_y`). Storing the selection anchor/focus
/// as document points (rather than run/char indices) keeps a selection valid across re-layout and
/// scrolling — it is re-resolved to text positions at paint / copy time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectHit {
    pub node_id: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub options: Vec<String>,
    pub selected: usize,
}

pub struct Engine {
    /// Logical viewport size in points and the backing scale factor (e.g. 2.0 on Retina).
    vp_w: u32,
    vp_h: u32,
    scale: f32,
    state: LoadState,
    font: Option<SystemFont>,
    /// Loaded `@font-face` web fonts, keyed by lowercased family name. Populated from the page's
    /// stylesheets after load; consulted (alongside `font`) when measuring/painting text whose
    /// computed `font-family` names a declared face.
    font_faces: HashMap<String, SystemFont>,
    /// Vertical scroll offset of the page content, in device pixels (0 = top). Clamped to
    /// the laid-out document height during `render`.
    scroll_y: f32,
    /// Whether the effective OS appearance is Dark. Pushed by the host (Swift) on launch and on
    /// every Light/Dark toggle; surfaced to page JS (`matchMedia('(prefers-color-scheme: dark)')`)
    /// and the CSS cascade (`@media (prefers-color-scheme)`).
    is_dark: bool,
    /// Cached layout tree so scrolling only re-paints (no re-cascade / re-layout).
    layout_cache: Option<LayoutCache>,
    /// Retained so the FFI layer can hand out a pointer that stays valid until the next
    /// render or until the engine is dropped.
    framebuffer: Option<Framebuffer>,
    /// Live per-page JS runtime (persistent V8 isolate) kept alive after load so page event
    /// handlers fire and timers keep running — i.e. the page is interactive. Replaced (old one
    /// dropped → its thread stops) on each navigation; `None` for pages without scripts.
    session: Option<js::Session>,
    /// The currently focused editable text field (`<input>` text-like / `<textarea>`), if any.
    /// Set when a click lands on such a control; key events are routed here. Cleared on navigation
    /// and when a click lands elsewhere.
    focused_node: Option<dom::NodeId>,
    /// The `value` of `focused_node` captured when it was focused, used to detect a change on blur
    /// (HTML `change` fires when an editable field loses focus and its value differs).
    focus_value: Option<String>,
    /// The node currently under the pointer (most recent `dispatch_move` hit). Used so hover events
    /// (mouseover/out, mouseenter/leave) fire only on transitions. Cleared on navigation.
    hovered_node: Option<dom::NodeId>,
    /// IntersectionObserver change-tracking: last `isIntersecting` flag per (observerId, nodeId).
    /// An IO callback fires only on the initial observation and whenever this flips. Cleared on
    /// navigation.
    prev_intersecting: HashMap<(u64, usize), bool>,
    /// ResizeObserver change-tracking: last reported (width, height) per (observerId, nodeId). A RO
    /// callback fires on the initial observation and whenever the size changes. Cleared on navigation.
    prev_size: HashMap<(u64, usize), (f32, f32)>,
    /// Progressive-load frame callback `(fn, ctx)`: when set, invoked with a [`FrameView`] each time
    /// a partial/final frame is painted during a streaming `load_url`. `None` = no progressive
    /// frames (the caller only pulls the final frame via `render`).
    frame_cb: Option<(FrameCallback, *mut std::ffi::c_void)>,
    /// Active text selection as `(anchor, focus)` DOCUMENT-space points (see [`Point`]). The anchor
    /// is where the drag began; the focus follows the pointer. `None` = nothing selected. Resolved
    /// to text positions (run + char) at paint/copy time so it survives scroll/re-layout.
    selection: Option<(Point, Point)>,
    /// DevTools "Elements" inspector highlight: the DOM node whose border box is painted with a
    /// translucent overlay AFTER the page. `None` = no highlight. Cleared on navigation.
    inspect_node: Option<dom::NodeId>,
    /// Rasterized `<canvas>` bitmaps keyed by canvas node id. Rebuilt each `render` from the JS
    /// 2D-context display lists (pulled via `Session::canvas_lists`); composited like decoded images.
    canvas_bitmaps: HashMap<dom::NodeId, DecodedImage>,
    /// Rasterized inline `<svg>` bitmaps keyed by the `<svg>` node id. Rebuilt each `render` by
    /// walking the SVG DOM subtree directly (no JS); composited like decoded images / canvas.
    svg_bitmaps: HashMap<dom::NodeId, DecodedImage>,
    /// `mask-image` sources keyed by their resolved url string, fetched/decoded at navigation
    /// (`collect_masks`): either an SVG source string (rasterized to alpha at paint size) or a
    /// decoded raster whose alpha channel is the coverage. Drives [`Engine::mask_bitmaps`].
    mask_sources: HashMap<String, MaskSource>,
    /// Per-box mask coverage bitmaps keyed by the masked element's node id. Rebuilt each `render`
    /// (after layout) by rasterizing the box's [`MaskSource`] to its border-box device size; the
    /// alpha channel is the coverage the painter multiplies the background by.
    mask_bitmaps: HashMap<dom::NodeId, DecodedImage>,
    /// The current page's decoded favicon (RGBA8), resolved from `<link rel=icon>` (or the
    /// origin's `/favicon.ico`) during `load_url`. `None` until one loads. Read by the shell to show
    /// the site icon in the tab and address bar. Cleared at the start of each navigation.
    favicon: Option<DecodedImage>,
    /// Decoded `background-image` sources at natural size, keyed by resolved url (fetched once,
    /// shared across boxes that use the same image). Rebuilt per navigation.
    bg_sources: HashMap<String, DecodedImage>,
    /// Per-box composed `background-image` bitmaps (border-box device size, image placed per
    /// size/repeat/position), keyed by node id. Rebuilt each layout (`update_bg_image_bitmaps`).
    bg_bitmaps: HashMap<dom::NodeId, DecodedImage>,
}

/// A fetched/decoded `mask-image` source, ready to rasterize to a per-box coverage bitmap.
#[derive(Clone)]
enum MaskSource {
    /// An SVG mask: the raw SVG markup, parsed + rasterized to the box size at paint time. Its
    /// rasterized alpha is the mask coverage.
    Svg(String),
    /// A raster mask (PNG/etc): the decoded RGBA. Its alpha channel is the mask coverage, scaled
    /// (nearest-neighbour) to the box.
    Raster(DecodedImage),
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

mod debug;
mod input;
mod lifecycle;
mod painter;
mod resources;
mod scripting;
mod text_helpers;
mod traversal;

pub(crate) use painter::*;
pub use resources::*;
pub use scripting::*;
pub(crate) use text_helpers::*;
pub(crate) use traversal::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that drive the process-global OS appearance (`set_color_scheme`) and then
    /// read color-scheme-dependent output, which `cargo test` would otherwise race on. Poisoning is
    /// irrelevant (we only need exclusion).
    static COLOR_SCHEME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn color_scheme_guard() -> std::sync::MutexGuard<'static, ()> {
        COLOR_SCHEME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Column of RGB pixels down the middle of a framebuffer (for comparing renders).
    fn center_column(fb: &Framebuffer) -> Vec<u8> {
        let x = (fb.width / 2) as usize;
        (0..fb.height)
            .flat_map(|y| {
                let i = (y * fb.stride) as usize + x * 4;
                fb.pixels[i..i + 3].to_vec()
            })
            .collect()
    }

    #[test]
    fn hr_paints_a_visible_horizontal_rule() {
        // An <hr> gets a UA height + gray background, so it must paint a horizontal band of
        // non-background (gray ~#888) pixels. Backgrounds paint without a font (deterministic in CI).
        let html = "<html><body><hr></body></html>";
        let path = std::env::temp_dir().join("browser_hr_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 200, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let fb = e.render();

        // Scan rows for a row that is mostly gray (~136 per channel, the #888 rule).
        let mut found_band = false;
        for y in 0..fb.height {
            let mut gray = 0u32;
            for x in 0..fb.width {
                let i = (y * fb.stride) as usize + (x as usize) * 4;
                let (r, g, b) = (fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]);
                if (r as i32 - 136).abs() < 30
                    && (g as i32 - 136).abs() < 30
                    && (b as i32 - 136).abs() < 30
                {
                    gray += 1;
                }
            }
            // A real rule spans most of the width.
            if gray > fb.width / 2 {
                found_band = true;
                break;
            }
        }
        assert!(
            found_band,
            "expected a horizontal band of gray rule pixels for <hr>"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scrolling_shifts_page_content() {
        // A page much taller than the viewport: 30 colored blocks of height 80 (~2400px).
        // Backgrounds paint without a font, so this is deterministic in CI.
        let mut body = String::from("<html><body>");
        for i in 0..30 {
            let shade = 40 + (i * 6) % 200;
            body.push_str(&format!(
                "<div style=\"height:80px; background-color:#{shade:02x}1414\"></div>"
            ));
        }
        body.push_str("</body></html>");
        let path = std::env::temp_dir().join("browser_scroll_test.html");
        std::fs::write(&path, body).unwrap();

        let mut e = Engine::new();
        e.set_viewport(120, 200, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);

        let top = center_column(e.render()).clone();
        // Scroll down well past one viewport and re-render.
        e.scroll_by(600.0);
        let scrolled = center_column(e.render()).clone();
        assert_ne!(
            top, scrolled,
            "scrolling a tall page must change the visible content"
        );

        // Scrolling back to the top restores the original view (clamped at 0).
        e.scroll_by(-100000.0);
        let back = center_column(e.render()).clone();
        assert_eq!(
            top, back,
            "scrolling back to the top restores the original render"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mask_image_clips_background_to_shape() {
        // A 40x40 red box with a circle mask: the centre must be red, the corners must show the page
        // background (white) — proving the background is composited only through the mask's opaque
        // pixels (the icon technique) instead of filling the whole box. The mask is a data: SVG with a
        // viewBox so it scales to the box; the circle leaves the corners transparent.
        let svg = "<svg viewBox='0 0 20 20'><circle cx='10' cy='10' r='9' fill='black'/></svg>";
        let url = format!("data:image/svg+xml,{svg}");
        let html = format!(
            "<html><body style='margin:0'><div style=\"width:40px;height:40px;background:#ff0000;\
             mask:url(&quot;{url}&quot;) no-repeat center / contain\"></div></body></html>"
        );
        let path = std::env::temp_dir().join("browser_mask_test.html");
        std::fs::write(&path, &html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(100, 100, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let fb = e.render();

        let px = |x: u32, y: u32| -> (u8, u8, u8) {
            let i = (y * fb.stride) as usize + (x as usize) * 4;
            (fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2])
        };
        // Centre of the box (20,20) is inside the circle → red.
        let (cr, cg, cb) = px(20, 20);
        assert!(
            cr > 200 && cg < 60 && cb < 60,
            "centre should be red, got {:?}",
            (cr, cg, cb)
        );
        // Corner (1,1) is outside the circle → page background (white), NOT red.
        let (kr, kg, kb) = px(1, 1);
        assert!(
            kr > 200 && kg > 200 && kb > 200,
            "corner should be page background (white), got {:?}",
            (kr, kg, kb)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn external_stylesheet_mask_url_resolves_against_sheet_dir() {
        // The browserscore bug, end-to-end over file://: the page lives at <dir>/index.html and
        // links <dir>/css/app.css; that sheet masks `.b` with url('../icons/dot.svg'). Per CSS the
        // relative url resolves against the SHEET's dir (<dir>/icons/dot.svg), NOT the page's dir
        // (<dir>/icons is right; <dir-of-page>/../icons would be wrong). If we (wrongly) resolved
        // against the document, the page is at <dir>/index.html so `../icons` escapes <dir> and the
        // SVG 404s → no mask → the whole box paints solid red (corners red). We assert corners show
        // the page background, proving the mask loaded from the sheet-relative path.
        let dir = std::env::temp_dir().join("browser_ext_mask_test");
        let _ = std::fs::create_dir_all(dir.join("css"));
        let _ = std::fs::create_dir_all(dir.join("icons"));
        // index.html sits in `dir`; the sheet sits one level deeper in `dir/css`. `../icons/dot.svg`
        // from the SHEET → `dir/icons/dot.svg`; from the PAGE → `dir/../icons/dot.svg` (escapes dir).
        let svg = "<svg viewBox='0 0 20 20'><circle cx='10' cy='10' r='9' fill='black'/></svg>";
        std::fs::write(dir.join("icons/dot.svg"), svg).unwrap();
        std::fs::write(
            dir.join("css/app.css"),
            ".b{width:40px;height:40px;background:red;\
             -webkit-mask:url('../icons/dot.svg') no-repeat center/contain;\
             mask:url('../icons/dot.svg') no-repeat center/contain}",
        )
        .unwrap();
        std::fs::write(
            dir.join("index.html"),
            "<html><head><link rel=stylesheet href='css/app.css'></head>\
             <body style='margin:0'><div class='b'></div></body></html>",
        )
        .unwrap();

        let mut e = Engine::new();
        e.set_viewport(100, 100, 1.0);
        assert_eq!(
            e.load_url(&format!("file://{}", dir.join("index.html").display())),
            0
        );
        let fb = e.render();
        let px = |x: u32, y: u32| -> (u8, u8, u8) {
            let i = (y * fb.stride) as usize + (x as usize) * 4;
            (fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2])
        };
        // Centre of the 40x40 box is inside the circle → red.
        let (cr, cg, cb) = px(20, 20);
        assert!(
            cr > 200 && cg < 60 && cb < 60,
            "centre should be red, got {:?}",
            (cr, cg, cb)
        );
        // Corner is outside the circle → page background, NOT red — proving the mask loaded from the
        // sheet-relative `../icons/dot.svg`. If url() resolved against the document the SVG would
        // 404, leaving the whole box red (corner red).
        let (kr, kg, kb) = px(1, 1);
        assert!(
            !(kr > 200 && kg < 60 && kb < 60),
            "corner is red → mask did not load (url() resolved against document, not sheet), got {:?}",
            (kr, kg, kb)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn box_without_mask_fills_fully() {
        // Regression: an unmasked red box fills its whole border box, corners included.
        let html = "<html><body style='margin:0'>\
            <div style='width:40px;height:40px;background:#ff0000'></div></body></html>";
        let path = std::env::temp_dir().join("browser_nomask_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(100, 100, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let fb = e.render();
        let i = fb.stride as usize + 4;
        let (r, g, b) = (fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]);
        assert!(
            r > 200 && g < 60 && b < 60,
            "unmasked box corner should be red, got {:?}",
            (r, g, b)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drag_selects_text_and_clear_empties_it() {
        // Two words on (likely) one line: a drag from the left of "Hello" to the middle of "world"
        // should select "Hello wor" (or similar). Using a wide viewport keeps it on one line.
        let html = "<html><body><p>Hello world</p></body></html>";
        let path = std::env::temp_dir().join("browser_select_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(400, 200, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render(); // build the layout cache so text runs exist

        // Find the text run for "Hello world" via the same DFS the resolver uses.
        let cache = e.layout_cache.as_ref().expect("layout");
        let runs = collect_text_runs(&cache.root);
        let run = runs
            .iter()
            .find(|r| r.text.contains("Hello"))
            .expect("text run");
        let font = e.font.as_ref().expect("font");

        // Left edge of the run (start of "Hello"), at the vertical middle of the run.
        let y_mid = run.rect.y + run.rect.height * 0.5;
        let x_start = run.rect.x + 0.5;
        // x at the middle of the word "world": accumulate advances through "Hello wor".
        let target = "Hello wor";
        let mut x_mid = run.rect.x;
        for ch in target.chars() {
            x_mid += font.advance(ch, run.font_size) + run.letter_spacing;
        }

        // Selection points are passed pre-scroll (scroll_y == 0 here, so document == viewport).
        e.selection_start(x_start, y_mid);
        e.selection_extend(x_mid, y_mid);
        assert!(
            e.has_selection(),
            "a drag across words must produce a selection"
        );
        let sel = e.selected_text();
        assert!(
            sel.starts_with("Hello") && sel.contains("wor"),
            "expected selection to span 'Hello'..'wor', got {sel:?}"
        );
        assert!(
            !sel.contains("world"),
            "selection should stop mid-word, got {sel:?}"
        );

        // Clearing empties the selection.
        e.selection_clear();
        assert!(!e.has_selection());
        assert_eq!(e.selected_text(), "");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bounding_client_rect_and_metrics_report_real_geometry() {
        // A div with an explicit 200x80 box. After load+layout the engine pushes the laid-out rects
        // to the JS session, so getBoundingClientRect / offsetWidth / offsetHeight read real values
        // (≈ the CSS width/height) and document.body.scrollHeight reports the full page height (>0).
        let html = "<html><body style=\"margin:0\">\
            <div style=\"width:200px;height:80px\"></div>\
            <script>window.__ready = true;</script></body></html>";
        let path = std::env::temp_dir().join("browser_bounding_rect_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(800, 600, 1.0); // scale 1 → CSS px == device px
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render(); // ensure layout is built + rects pushed

        let wh = e.console_eval(
            "var r = document.querySelector('div').getBoundingClientRect(); r.width + 'x' + r.height",
        );
        assert_eq!(wh, "200x80", "getBoundingClientRect width/height");

        let ow = e.console_eval(
            "var d = document.querySelector('div'); d.offsetWidth + 'x' + d.offsetHeight",
        );
        assert_eq!(ow, "200x80", "offsetWidth/offsetHeight");

        // The body's scrollHeight reports the full document content height (> 0).
        let sh = e.console_eval("document.body.scrollHeight > 0");
        assert_eq!(sh, "true", "document.body.scrollHeight should be positive");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn retina_scale_renders_device_px_but_keeps_css_px_geometry() {
        // The SAME 200x80 div on a 2x (Retina) backing. Layout runs in CSS px and is baked to device
        // px, so: the framebuffer is 2x the logical viewport; CSS-facing geometry (getBoundingClientRect,
        // innerWidth) stays in CSS px and is therefore IDENTICAL to the 1x case; devicePixelRatio reads 2.
        // Regression: before HiDPI layout scaling, page content was laid out 1 device-px per CSS-px, so
        // this div's rect came back halved ("100x40") and the page rendered at half physical size.
        let html = "<html><body style=\"margin:0\">\
            <div style=\"width:200px;height:80px\"></div>\
            <script>window.__ready = true;</script></body></html>";
        let path = std::env::temp_dir().join("browser_retina_scale_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(800, 600, 2.0); // 2x backing: device framebuffer is 1600x1200
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let fb = e.render();
        assert_eq!(
            (fb.width, fb.height),
            (1600, 1200),
            "framebuffer is the logical viewport x backing scale"
        );

        // CSS px is invariant to the backing scale — same values as the scale-1 test above. This is
        // driven by the engine's per-instance `scale`, so it's deterministic. (We deliberately don't
        // assert `window.innerWidth`/`devicePixelRatio` here: those read process-global display
        // metrics that other tests sharing the process clobber under parallel execution.)
        let wh = e.console_eval(
            "var r = document.querySelector('div').getBoundingClientRect(); r.width + 'x' + r.height",
        );
        assert_eq!(
            wh, "200x80",
            "getBoundingClientRect stays in CSS px on Retina"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hit_testing_apis_element_and_caret_from_point() {
        // A div with an explicit 200x80 box flush at the top-left. The engine lays it out and seeds
        // the JS session's rect table, so the hit-testing APIs resolve against real geometry.
        let html = "<html><body style=\"margin:0\">\
            <div id=\"box\" style=\"width:200px;height:80px\">hello</div>\
            <script>window.__ready = true;</script></body></html>";
        let path = std::env::temp_dir().join("browser_hit_test_apis.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(800, 600, 1.0); // scale 1 → CSS px == device px
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render(); // ensure layout is built + rects pushed

        // elementFromPoint inside the box (CSS px, viewport-relative) returns the div.
        let id = e.console_eval("var el = document.elementFromPoint(20, 20); el ? el.id : 'null'");
        assert_eq!(
            id, "box",
            "elementFromPoint(20,20) should return the div#box"
        );

        // A point outside the viewport yields null.
        let outside = e.console_eval("document.elementFromPoint(-5, 5) === null");
        assert_eq!(
            outside, "true",
            "elementFromPoint outside the viewport is null"
        );

        // caretPositionFromPoint throws TypeError with too few arguments (arity check).
        let arity = e.console_eval(
            "var t = false; try { document.caretPositionFromPoint(); } catch (e) { t = (e instanceof TypeError); } t",
        );
        assert_eq!(
            arity, "true",
            "caretPositionFromPoint() with no args throws TypeError"
        );

        // caretRangeFromPoint(0, 0) returns a collapsed Range with offsets 0/0.
        let range = e.console_eval(
            "var r = document.caretRangeFromPoint(0, 0); \
             (r instanceof Range) + ',' + r.startOffset + ',' + r.endOffset + ',' + r.collapsed",
        );
        assert_eq!(
            range, "true,0,0,true",
            "caretRangeFromPoint(0,0) is a collapsed Range at 0/0"
        );

        // caretPositionFromPoint over the box returns a CaretPosition whose offsetNode is contained
        // by the div (the text node "hello" or the div itself).
        let caret = e.console_eval(
            "var p = document.caretPositionFromPoint(20, 20); \
             (p instanceof CaretPosition) + ',' + (p && document.getElementById('box').contains(p.offsetNode))",
        );
        assert_eq!(
            caret, "true,true",
            "caretPositionFromPoint over the box returns a CaretPosition inside it"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn matchmedia_prefers_color_scheme_tracks_os_appearance() {
        let _g = color_scheme_guard();
        // A page with a script so the engine keeps a live JS Session we can console_eval against.
        let html = "<html><body><script>window.__ready = true;</script></body></html>";
        let path = std::env::temp_dir().join("browser_color_scheme_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(800, 600, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);

        // Dark: the dark query matches, light does not.
        e.set_color_scheme(true);
        assert_eq!(
            e.console_eval("matchMedia('(prefers-color-scheme: dark)').matches"),
            "true",
            "dark query should match in Dark mode",
        );
        assert_eq!(
            e.console_eval("matchMedia('(prefers-color-scheme: light)').matches"),
            "false",
            "light query should not match in Dark mode",
        );

        // Light: reversed.
        e.set_color_scheme(false);
        assert_eq!(
            e.console_eval("matchMedia('(prefers-color-scheme: dark)').matches"),
            "false",
            "dark query should not match in Light mode",
        );
        assert_eq!(
            e.console_eval("matchMedia('(prefers-color-scheme: light)').matches"),
            "true",
            "light query should match in Light mode",
        );

        // A bare `(prefers-color-scheme)` query matches regardless of appearance.
        assert_eq!(
            e.console_eval("matchMedia('(prefers-color-scheme)').matches"),
            "true",
            "bare prefers-color-scheme should always match",
        );

        // `change` fires on an existing MediaQueryList when the appearance flips.
        let _ = e.console_eval(
            "window.__pcsChanges = 0; \
             var __mql = matchMedia('(prefers-color-scheme: dark)'); \
             __mql.addEventListener('change', function (ev) { window.__pcsChanges++; window.__pcsLast = ev.matches; });",
        );
        e.set_color_scheme(true); // light -> dark: should fire once with matches=true
        assert_eq!(
            e.console_eval("window.__pcsChanges + ':' + window.__pcsLast"),
            "1:true",
            "change should fire with matches=true on flip to Dark",
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Render `html` at `os_dark` appearance and return the top-left canvas pixel (r, g, b).
    /// Holds the color-scheme test lock for the whole set+render so the OS flag can't be flipped
    /// by a parallel test mid-render.
    fn canvas_top_left(html: &str, os_dark: bool) -> (u8, u8, u8) {
        let _g = color_scheme_guard();
        let path = std::env::temp_dir().join(format!(
            "browser_color_scheme_canvas_{}.html",
            rand_suffix()
        ));
        std::fs::write(&path, html).unwrap();
        let mut e = Engine::new();
        e.set_viewport(200, 200, 1.0);
        e.set_color_scheme(os_dark);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let fb = e.render();
        let px = (fb.pixels[0], fb.pixels[1], fb.pixels[2]);
        let _ = std::fs::remove_file(&path);
        px
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    #[test]
    fn color_scheme_dark_gives_dark_canvas() {
        // `color-scheme: dark` (only dark) → dark canvas regardless of OS flag.
        let html = "<html style=\"color-scheme: dark\"><body>hi</body></html>";
        assert_eq!(
            canvas_top_left(html, true),
            (0x1e, 0x1e, 0x1e),
            "dark canvas in Dark OS"
        );
        assert_eq!(
            canvas_top_left(html, false),
            (0x1e, 0x1e, 0x1e),
            "dark canvas in Light OS"
        );
        // Via :root { color-scheme: dark } too.
        let html2 =
            "<html><head><style>:root{color-scheme:dark}</style></head><body>hi</body></html>";
        assert_eq!(canvas_top_left(html2, true), (0x1e, 0x1e, 0x1e));
    }

    #[test]
    fn color_scheme_light_or_unset_stays_white() {
        // Only-light → white canvas regardless of OS flag.
        let light = "<html style=\"color-scheme: light\"><body>hi</body></html>";
        assert_eq!(
            canvas_top_left(light, true),
            (0xff, 0xff, 0xff),
            "light stays white in Dark OS"
        );
        assert_eq!(canvas_top_left(light, false), (0xff, 0xff, 0xff));
        // Unset → white canvas even when the OS is dark (no opt-in).
        let unset = "<html><body>hi</body></html>";
        assert_eq!(
            canvas_top_left(unset, true),
            (0xff, 0xff, 0xff),
            "no opt-in stays white"
        );
    }

    #[test]
    fn color_scheme_light_dark_follows_os() {
        // `light dark` (both supported) → follow the OS appearance.
        let html = "<html style=\"color-scheme: light dark\"><body>hi</body></html>";
        assert_eq!(
            canvas_top_left(html, true),
            (0x1e, 0x1e, 0x1e),
            "dark canvas when OS Dark"
        );
        assert_eq!(
            canvas_top_left(html, false),
            (0xff, 0xff, 0xff),
            "white canvas when OS Light"
        );
    }

    #[test]
    fn color_scheme_browserscore_style_dark_canvas() {
        // Mirrors browserscore.dev: color-scheme:dark gated behind the dark media query, and a body
        // background that references an undefined custom property (leaving it transparent, so the
        // canvas shows through). Dark OS → dark canvas.
        let html = "<html><head><style>\
            @media (prefers-color-scheme: dark){:root{color-scheme:dark}} \
            body{background:var(--undefined-var)}\
            </style></head><body>hi</body></html>";
        assert_eq!(
            canvas_top_left(html, true),
            (0x1e, 0x1e, 0x1e),
            "dark canvas when OS Dark"
        );
        assert_eq!(
            canvas_top_left(html, false),
            (0xff, 0xff, 0xff),
            "white canvas when OS Light"
        );
    }

    #[test]
    fn color_scheme_dynamic_toggle_updates_canvas() {
        let _g = color_scheme_guard();
        // `light dark` page: toggling the OS appearance re-resolves the used scheme on next render.
        let html = "<html style=\"color-scheme: light dark\"><body>hi</body></html>";
        let path = std::env::temp_dir().join(format!(
            "browser_color_scheme_toggle_{}.html",
            rand_suffix()
        ));
        std::fs::write(&path, html).unwrap();
        let mut e = Engine::new();
        e.set_viewport(200, 200, 1.0);
        e.set_color_scheme(false);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let fb = e.render();
        assert_eq!(
            (fb.pixels[0], fb.pixels[1], fb.pixels[2]),
            (0xff, 0xff, 0xff)
        );
        // Flip to Dark — the cascade re-runs and the canvas goes dark.
        e.set_color_scheme(true);
        let fb = e.render();
        assert_eq!(
            (fb.pixels[0], fb.pixels[1], fb.pixels[2]),
            (0x1e, 0x1e, 0x1e)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn typing_into_focused_input_updates_value() {
        // A page with a text input and a trivial inline script (so a JS session is started).
        let html = "<html><body><input id=f>\
            <script>window.__ready = true;</script></body></html>";
        let path = std::env::temp_dir().join("browser_input_focus_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 120, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        // Build the layout cache (also proves the input box lays out without panicking).
        let _ = e.render();

        // Focus the input (click-to-focus is fiddly for an empty zero-width control in a test).
        assert!(
            e.focus_first_text_field(),
            "page must have an editable text field"
        );
        let f = e.focused_node_for_test().expect("focused node");
        assert!(e.has_text_focus(), "an input must report text focus");

        // Type "h" then "i": the input's value attribute reflects the typed text.
        assert!(e.dispatch_key("h", "KeyH"));
        assert!(e.dispatch_key("i", "KeyI"));
        assert_eq!(e.node_attr(f, "value").as_deref(), Some("hi"));

        // Backspace removes the last character.
        assert!(e.dispatch_key("Backspace", "Backspace"));
        assert_eq!(e.node_attr(f, "value").as_deref(), Some("h"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clicking_checkbox_toggles_checked_and_fires_change() {
        let html = "<html><body>\
            <input id=c type=checkbox>\
            <script>\
              document.getElementById('c').addEventListener('change', function (e) {\
                document.body.setAttribute('data-changed', e.target.checked ? 'on' : 'off');\
              });\
            </script></body></html>";
        let path = std::env::temp_dir().join("browser_checkbox_click_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 120, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render(); // build the layout cache so the checkbox has a hit box

        let c = e.node_by_attr_id("c").expect("checkbox node");
        assert!(e.node_attr(c, "checked").is_none(), "starts unchecked");
        let (cx, cy) = e.node_center_device(c).expect("checkbox laid out");

        assert!(
            e.dispatch_click(cx, cy),
            "clicking the checkbox warrants a re-render"
        );
        let c2 = e.node_by_attr_id("c").expect("checkbox node");
        assert!(
            e.node_attr(c2, "checked").is_some(),
            "checkbox should be checked after click"
        );
        // The page's change handler set a body attribute.
        let body = e.node_by_attr_id("__nope__"); // sanity: missing id returns None
        assert!(body.is_none());
        assert_eq!(
            e.visible_attr_body("data-changed").as_deref(),
            Some("on"),
            "change handler should have run"
        );

        let _ = std::fs::remove_file(&path);
    }

    // --- Form-widget rendering -----------------------------------------------------------------

    /// Load `html`, size the viewport, render, and return the engine (with a built layout cache).
    #[cfg(test)]
    fn load_rendered(html: &str, name: &str, w: u32, h: u32) -> Engine {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, html).unwrap();
        let mut e = Engine::new();
        e.set_viewport(w, h, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();
        let _ = std::fs::remove_file(&path);
        e
    }

    /// The (r,g,b) of the framebuffer pixel at device (x, y).
    #[cfg(test)]
    fn px_at(e: &Engine, x: i32, y: i32) -> (u8, u8, u8) {
        let fb = e.framebuffer().unwrap();
        let i = (y as u32 * fb.stride) as usize + (x as usize) * 4;
        (fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2])
    }

    /// Count non-background pixels in a node's device rect (bg taken as the top-left page color).
    #[cfg(test)]
    fn painted_pixels_in(e: &Engine, r: layout::Rect) -> usize {
        let fb = e.framebuffer().unwrap();
        let bg = px_at(e, 1, 1);
        let mut n = 0;
        let x0 = (r.x.max(0.0)) as i32;
        let y0 = (r.y.max(0.0)) as i32;
        let x1 = ((r.x + r.width) as i32).min(fb.width as i32);
        let y1 = ((r.y + r.height) as i32).min(fb.height as i32);
        for y in y0..y1 {
            for x in x0..x1 {
                if px_at(e, x, y) != bg {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn range_renders_track_and_thumb_offset_by_value() {
        // Two ranges: low value (thumb near left) vs high value (thumb near right). The thumb's
        // horizontal centroid of painted pixels must move right as the value increases.
        let html = "<html><body>\
            <input id=lo type=range min=0 max=100 value=10>\
            <input id=hi type=range min=0 max=100 value=90>\
            </body></html>";
        let e = load_rendered(html, "browser_range_test.html", 400, 200);
        let centroid_x = |id: &str| -> f32 {
            let n = e.node_by_attr_id(id).unwrap();
            let r = e.node_device_rect(n).unwrap();
            let fb = e.framebuffer().unwrap();
            let bg = px_at(&e, 1, 1);
            let (mut sum, mut cnt) = (0.0f32, 0.0f32);
            for y in r.y as i32..(r.y + r.height) as i32 {
                for x in r.x as i32..(r.x + r.width) as i32 {
                    if x >= 0
                        && y >= 0
                        && x < fb.width as i32
                        && y < fb.height as i32
                        && px_at(&e, x, y) != bg
                    {
                        sum += x as f32;
                        cnt += 1.0;
                    }
                }
            }
            assert!(cnt > 0.0, "range {id} painted nothing");
            sum / cnt
        };
        let lo = centroid_x("lo");
        let hi = centroid_x("hi");
        assert!(
            hi > lo + 10.0,
            "thumb should move right with value: lo={lo}, hi={hi}"
        );
    }

    #[test]
    fn color_swatch_shows_the_value_color() {
        let html = "<html><body><input id=c type=color value=\"#3366ff\"></body></html>";
        let e = load_rendered(html, "browser_color_test.html", 200, 120);
        let n = e.node_by_attr_id("c").unwrap();
        let r = e.node_device_rect(n).unwrap();
        // The swatch center should be ≈ #3366ff.
        let (cr, cg, cb) = px_at(
            &e,
            (r.x + r.width / 2.0) as i32,
            (r.y + r.height / 2.0) as i32,
        );
        assert!(
            (cr as i32 - 0x33).abs() < 24
                && (cg as i32 - 0x66).abs() < 24
                && (cb as i32 - 0xff).abs() < 24,
            "swatch center {:?} should be ~#3366ff",
            (cr, cg, cb)
        );
    }

    #[test]
    fn progress_and_meter_fill_proportionally() {
        let html = "<html><body>\
            <progress id=p value=0.6 max=1></progress><br>\
            <meter id=m value=0.3 max=1></meter>\
            </body></html>";
        let e = load_rendered(html, "browser_bar_test.html", 400, 200);
        // The filled portion (a darker/colored band starting at the left) must span ≈ value/max of
        // the bar width. We measure the colored run on the bar's mid row.
        let filled_frac = |id: &str, accent: (u8, u8, u8)| -> f32 {
            let n = e.node_by_attr_id(id).unwrap();
            let r = e.node_device_rect(n).unwrap();
            let y = (r.y + r.height / 2.0) as i32;
            let mut filled = 0;
            for x in r.x as i32..(r.x + r.width) as i32 {
                let (pr, pg, pb) = px_at(&e, x, y);
                if (pr as i32 - accent.0 as i32).abs() < 40
                    && (pg as i32 - accent.1 as i32).abs() < 40
                    && (pb as i32 - accent.2 as i32).abs() < 40
                {
                    filled += 1;
                }
            }
            filled as f32 / r.width
        };
        let pf = filled_frac("p", (36, 110, 230)); // progress: blue
        let mf = filled_frac("m", (76, 174, 80)); // meter: green
        assert!((pf - 0.6).abs() < 0.15, "progress fill {pf} should be ~0.6");
        assert!((mf - 0.3).abs() < 0.15, "meter fill {mf} should be ~0.3");
    }

    #[test]
    fn checkbox_paints_a_visible_box_that_differs_checked_vs_unchecked() {
        let html = "<html><body>\
            <input id=u type=checkbox>\
            <input id=c type=checkbox checked>\
            </body></html>";
        let e = load_rendered(html, "browser_checkbox_paint_test.html", 200, 120);
        let u = e.node_by_attr_id("u").unwrap();
        let c = e.node_by_attr_id("c").unwrap();
        let ur = e.node_device_rect(u).unwrap();
        let cr = e.node_device_rect(c).unwrap();
        // Both paint a visible box.
        assert!(
            painted_pixels_in(&e, ur) > 0,
            "unchecked checkbox painted nothing"
        );
        assert!(
            painted_pixels_in(&e, cr) > 0,
            "checked checkbox painted nothing"
        );
        // The checked one (filled accent + check) has more painted/colored pixels than the empty one.
        assert!(
            painted_pixels_in(&e, cr) > painted_pixels_in(&e, ur),
            "checked checkbox should differ (more fill) than unchecked"
        );
    }

    #[test]
    fn label_for_has_geometry_and_click_toggles_target() {
        // A no-op <script> so the JS runtime/session exists (dispatch_click routes through it).
        let html = "<html><body>\
            <input id=cb type=checkbox>\
            <label id=lbl for=cb>Toggle me</label>\
            <script>/* keep a session alive */</script>\
            </body></html>";
        let mut e = load_rendered(html, "browser_label_for_test.html", 300, 120);
        let lbl = e.node_by_attr_id("lbl").expect("label node");
        // The label now lays out with a real, non-zero box (was 0x0 before).
        let r = e.node_device_rect(lbl).expect("label laid out");
        assert!(
            r.width > 0.0 && r.height > 0.0,
            "label should have a non-zero rect: {r:?}"
        );

        let cb = e.node_by_attr_id("cb").unwrap();
        assert!(e.node_attr(cb, "checked").is_none(), "starts unchecked");
        // Click the center of the label → the for= target checkbox toggles on.
        let (lx, ly) = (r.x + r.width / 2.0, r.y + r.height / 2.0);
        assert!(
            e.dispatch_click(lx, ly),
            "clicking the label warrants a re-render"
        );
        let cb2 = e.node_by_attr_id("cb").unwrap();
        assert!(
            e.node_attr(cb2, "checked").is_some(),
            "clicking <label for> should toggle the target checkbox"
        );
    }

    #[test]
    fn hover_move_fires_page_mouseover_handler() {
        let html = "<html><body>\
            <div id=m style=\"width:80px;height:40px;background-color:#445566\">menu</div>\
            <script>\
              document.getElementById('m').addEventListener('mouseover', function () {\
                document.body.setAttribute('data-hover', 'yes');\
              });\
            </script></body></html>";
        let path = std::env::temp_dir().join("browser_hover_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 120, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();

        let m = e.node_by_attr_id("m").expect("menu node");
        let (cx, cy) = e.node_center_device(m).expect("menu laid out");
        assert!(
            e.dispatch_move(cx, cy),
            "moving over a new node should change hover"
        );
        assert_eq!(e.visible_attr_body("data-hover").as_deref(), Some("yes"));

        // Moving again to the same node is a cheap no-op (returns false).
        let _ = e.render();
        let m2 = e.node_by_attr_id("m").expect("menu node");
        let (cx2, cy2) = e.node_center_device(m2).expect("menu laid out");
        assert!(
            !e.dispatch_move(cx2, cy2),
            "hovering the same node again is a no-op"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clicking_field_then_outside_fires_blur() {
        let html = "<html><body>\
            <input id=f>\
            <div id=elsewhere style=\"width:60px;height:40px;background-color:#334455\">x</div>\
            <script>\
              document.getElementById('f').addEventListener('blur', function () {\
                document.body.setAttribute('data-blurred', 'yes');\
              });\
            </script></body></html>";
        let path = std::env::temp_dir().join("browser_blur_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 120, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();

        // Focus the input via the test helper, then click outside it.
        assert!(e.focus_first_text_field());
        let _ = e.render();
        let other = e.node_by_attr_id("elsewhere").expect("other node");
        let (ox, oy) = e.node_center_device(other).expect("other laid out");
        assert!(e.dispatch_click(ox, oy));
        assert_eq!(
            e.visible_attr_body("data-blurred").as_deref(),
            Some("yes"),
            "clicking outside the field should fire blur"
        );
        assert!(
            e.focused_node_for_test().is_none(),
            "focus cleared after clicking outside"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn select_at_reports_options_and_set_select_index_fires_change() {
        let html = "<html><body>\
            <select id=s>\
              <option value=a>Apple</option>\
              <option value=b selected>Banana</option>\
              <option value=c>Cherry</option>\
            </select>\
            <script>\
              document.getElementById('s').addEventListener('change', function (e) {\
                document.body.setAttribute('data-changed', e.target.value);\
              });\
            </script></body></html>";
        let path = std::env::temp_dir().join("browser_select_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 120, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();

        let s = e.node_by_attr_id("s").expect("select node");
        let (cx, cy) = e.node_center_device(s).expect("select laid out");

        // select_at over the laid-out <select> returns its three options + selected index (Banana).
        let hit = e
            .select_at(cx, cy)
            .expect("click on <select> returns a SelectHit");
        assert_eq!(hit.node_id, s.0);
        assert_eq!(
            hit.options,
            vec![
                "Apple".to_string(),
                "Banana".to_string(),
                "Cherry".to_string()
            ]
        );
        assert_eq!(hit.selected, 1, "Banana is the pre-selected option");
        assert!(hit.width > 0.0 && hit.height > 0.0, "rect has a size");

        // Picking Cherry changes the selection, fires change (handler stamps body), and is reflected
        // by a fresh select_at (now selected index 2).
        assert!(
            e.set_select_index(s.0, 2),
            "selecting a different option changes it"
        );
        assert_eq!(
            e.visible_attr_body("data-changed").as_deref(),
            Some("c"),
            "the page's change handler ran with the new value"
        );
        let _ = e.render();
        let s2 = e.node_by_attr_id("s").expect("select node");
        let (cx2, cy2) = e.node_center_device(s2).expect("select laid out");
        let hit2 = e.select_at(cx2, cy2).expect("still a select");
        assert_eq!(hit2.selected, 2, "Cherry is now selected");

        // Re-selecting the same option reports no change.
        assert!(
            !e.set_select_index(s2.0, 2),
            "re-picking the current option is a no-op"
        );

        let _ = std::fs::remove_file(&path);
    }

    fn base64_encode(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(A[(n >> 18 & 63) as usize] as char);
            out.push(A[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                A[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                A[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn base64_decode_known_strings() {
        assert_eq!(base64_decode("SGVsbG8h").unwrap(), b"Hello!");
        assert_eq!(base64_decode("SGVsbG8=").unwrap(), b"Hello");
    }

    #[test]
    fn data_url_png_image_decodes() {
        // Generate a 3x2 PNG, base64-encode it into a data URL, and decode it back.
        let mut img = image::RgbaImage::new(3, 2);
        for p in img.pixels_mut() {
            *p = image::Rgba([200, 40, 40, 255]);
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let url = format!("data:image/png;base64,{}", base64_encode(&png));
        let bytes = decode_data_url(&url).expect("data url decodes");
        let decoded = decode_image(&bytes).expect("png decodes");
        assert_eq!((decoded.w, decoded.h), (3, 2));
    }

    #[test]
    fn renders_a_nonblank_framebuffer() {
        let mut e = Engine::new();
        e.set_viewport(200, 100, 1.0);
        let fb = e.render();
        assert_eq!(fb.width, 200);
        assert_eq!(fb.height, 100);
        // The gradient guarantees some non-zero blue somewhere.
        assert!(fb.pixels.iter().skip(2).step_by(4).any(|&b| b > 0));
    }

    #[test]
    fn link_at_returns_resolved_url_for_anchor_text() {
        // A page with a single anchor. The engine needs a font to lay text out; if none is
        // available in this environment, skip (layout produces no text boxes).
        if SystemFont::load().is_none() {
            eprintln!("no system font; skipping link_at test");
            return;
        }
        let html = "<html><body><a href=\"https://example.com/x\">link</a></body></html>";
        let path = std::env::temp_dir().join("browser_link_at_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(400, 300, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        // Render to build the layout cache.
        let _ = e.render();

        // Find the anchor's Text box in the cached layout (the only one, content "link").
        fn find_text<'a>(b: &'a layout::LayoutBox, want: &str) -> Option<&'a layout::LayoutBox> {
            if let layout::BoxContent::Text(t) = &b.content {
                if t.contains(want) {
                    return Some(b);
                }
            }
            for c in &b.children {
                if let Some(f) = find_text(c, want) {
                    return Some(f);
                }
            }
            None
        }
        let cache = e.layout_cache.as_ref().expect("layout cache built");
        let tb = find_text(&cache.root, "link").expect("anchor text box present");
        let r = tb.dimensions.border_box();
        // Layout-space center of the text box.
        let lx = r.x + r.width / 2.0;
        let ly = r.y + r.height / 2.0;
        // Convert to device pixels (inverse of the layout->device mapping in render/link_at): the
        // page paints flush at (0,0) (no engine inset), so device = layout - scroll.
        let dx = lx;
        let dy = ly - e.scroll_y;

        assert_eq!(
            e.link_at(dx, dy).as_deref(),
            Some("https://example.com/x"),
            "click inside the anchor returns its resolved URL"
        );
        // A click far away in empty space returns no link.
        assert_eq!(e.link_at(5.0, 290.0), None, "empty space has no link");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_url_is_recorded_as_failure() {
        let mut e = Engine::new();
        assert_eq!(e.load_url("http://"), -1);
    }

    #[test]
    fn extracts_visible_text_skipping_non_rendered() {
        let doc = html::parse(
            "<html><head><title>T</title><style>x{}</style></head>\
             <body><h1>Hello   World</h1><p>Some <b>bold</b> text.</p>\
             <script>var a = 1;</script></body></html>",
        );
        let text = extract_visible_text(&doc);
        // Title/style/script are skipped; whitespace collapsed; block breaks present.
        assert_eq!(text, "Hello World\nSome bold text.");
    }

    #[test]
    fn runs_inline_scripts_and_captures_console() {
        let doc =
            html::parse(r#"<html><body><script>console.log("hi", 6*7)</script></body></html>"#);
        let (_doc, console) = run_scripts(doc, "https://example.com/");
        assert!(
            console.iter().any(|l| l == "hi 42"),
            "expected 'hi 42' in console, got {console:?}"
        );
    }

    #[test]
    fn body_onload_attribute_fires_on_window_load() {
        // `<body onload="...">` is a Window-reflecting body event handler: it sets window.onload and
        // must run when the `load` event fires — even if no script ever touches `document.body`. This
        // is what unblocks the `check-layout-th.js` WPT tests that start from `<body onload=...>`.
        let doc = html::parse(
            r#"<html><body onload="console.log('loaded:' + (typeof window.onload))">
               <script>/* presence of a script starts the JS session + lifecycle */</script>
               </body></html>"#,
        );
        let (_doc, console) = run_scripts(doc, "https://example.com/");
        assert!(
            console.iter().any(|l| l == "loaded:function"),
            "body onload should fire on window load; got {console:?}"
        );
    }

    #[test]
    fn inline_scripts_share_state_in_document_order() {
        let doc = html::parse(
            r#"<html><body><script>var x = 5;</script><script>console.log(x * 2)</script></body></html>"#,
        );
        let (_doc, console) = run_scripts(doc, "https://example.com/");
        assert!(console.iter().any(|l| l == "10"), "got {console:?}");
    }

    #[test]
    fn external_script_fetch_failure_is_noted_in_order() {
        // The src resolves but points at a nonexistent local file, so the fetch fails and we
        // emit a `[failed to load script: …]` note rather than aborting the load.
        let doc = html::parse(
            r#"<html><body><script src="file:///nonexistent/xyz-abc.js"></script></body></html>"#,
        );
        let (_doc, console) = run_scripts(doc, "https://example.com/");
        assert_eq!(console.len(), 1, "got {console:?}");
        assert!(
            console[0].starts_with("[failed to load script: file:///nonexistent/xyz-abc.js"),
            "got {console:?}"
        );
    }

    #[test]
    fn resolve_url_resolves_relative_and_rejects_non_fetchable() {
        assert_eq!(
            resolve_url("https://a.com/x/y.html", "../style.css"),
            Some("https://a.com/style.css".to_string())
        );
        // Absolute href passes through unchanged.
        assert_eq!(
            resolve_url("https://a.com/x/y.html", "https://cdn.com/app.js"),
            Some("https://cdn.com/app.js".to_string())
        );
        // Fragment-only and javascript: are not fetchable.
        assert_eq!(resolve_url("https://a.com/x/y.html", "#frag"), None);
        assert_eq!(resolve_url("https://a.com/x/y.html", "javascript:0"), None);
        assert_eq!(
            resolve_url("https://a.com/x/y.html", "data:text/css,a{}"),
            None
        );
        assert_eq!(resolve_url("https://a.com/x/y.html", ""), None);
    }

    #[test]
    fn collects_style_sources_in_document_order_classified() {
        // inline <style> A, then <link rel=stylesheet>, then inline <style> B: order and
        // link-vs-inline classification must be preserved (pure, no fetching).
        let doc = html::parse(
            r#"<html><head>
                 <style>a{color:red}</style>
                 <link rel="stylesheet" href="../theme.css">
                 <style>b{color:blue}</style>
               </head><body></body></html>"#,
        );
        let sources = collect_style_sources(&doc, "https://a.com/x/page.html");
        assert_eq!(sources.len(), 3, "got {sources:?}");
        assert!(matches!(&sources[0], StyleSource::Inline(s) if s.contains("color:red")));
        assert_eq!(
            sources[1],
            StyleSource::External("https://a.com/theme.css".to_string())
        );
        assert!(matches!(&sources[2], StyleSource::Inline(s) if s.contains("color:blue")));
    }

    #[test]
    fn collects_script_sources_in_order_skipping_non_js() {
        let doc = html::parse(
            r#"<html><body>
                 <script>var a=1;</script>
                 <script src="app.js"></script>
                 <script type="application/json">{"x":1}</script>
                 <script type="module" src="mod.js"></script>
               </body></html>"#,
        );
        let sources = collect_script_sources(&doc, "https://a.com/x/page.html");
        // JSON and module scripts are skipped; inline + classic external remain, in order.
        assert_eq!(sources.len(), 2, "got {sources:?}");
        assert!(matches!(&sources[0], ScriptSource::Inline(s) if s.contains("var a=1")));
        assert_eq!(
            sources[1],
            ScriptSource::External("https://a.com/x/app.js".to_string())
        );
    }

    #[test]
    fn base_url_honors_base_href_element() {
        let doc = html::parse(
            r#"<html><head><base href="https://cdn.example/assets/"></head><body></body></html>"#,
        );
        assert_eq!(
            base_url(&doc, "https://orig.com/page.html"),
            "https://cdn.example/assets/"
        );
        // A relative <base href> resolves against the response URL.
        let doc2 = html::parse(r#"<html><head><base href="/sub/"></head></html>"#);
        assert_eq!(
            base_url(&doc2, "https://orig.com/a/b.html"),
            "https://orig.com/sub/"
        );
        // No <base>: falls back to the response URL.
        let doc3 = html::parse("<html><head></head></html>");
        assert_eq!(
            base_url(&doc3, "https://orig.com/a/b.html"),
            "https://orig.com/a/b.html"
        );
    }

    #[test]
    fn external_stylesheet_via_local_file_is_applied() {
        // Write a CSS file to a temp dir and reference it via a file:// <link>; the fetched
        // sheet must be parsed and interleaved with the inline <style>, in document order.
        let dir = std::env::temp_dir().join(format!("engine_css_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let css_path = dir.join("ext.css");
        std::fs::write(&css_path, "p { color: #00ff00 }").unwrap();
        let css_url = format!("file://{}", css_path.display());
        let base = format!("file://{}/page.html", dir.display());

        let html = format!(
            r#"<html><head>
                 <style>h1 {{ color: #ff0000 }}</style>
                 <link rel="stylesheet" href="{css_url}">
               </head><body></body></html>"#
        );
        let doc = html::parse(&html);
        let (sheets, console) = collect_stylesheets(&doc, &base);
        // One inline sheet + one fetched external sheet, in document order.
        assert_eq!(sheets.len(), 2, "console: {console:?}");
        assert!(console.is_empty(), "unexpected notes: {console:?}");
        // The external sheet (second) carries the `p` rule from the file.
        assert!(
            sheets[1]
                .rules
                .iter()
                .any(|r| r.selectors.iter().any(|s| s.contains('p'))),
            "external sheet not parsed: {:?}",
            sheets[1]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_stylesheet_at_import_is_followed_in_order() {
        // A `<link>` CSS file that `@import`s another CSS file (both via file://). The imported
        // sheet's rules must be collected BEFORE the importer's own rules (CSS precedence), with
        // no network. Tests recursion (the importer also has its own rule) and ordering.
        let dir = std::env::temp_dir().join(format!("engine_import_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // tokens.css is imported by main.css and defines `.token`.
        let tokens_path = dir.join("tokens.css");
        std::fs::write(&tokens_path, ".token { color: #111111 }").unwrap();
        // main.css imports tokens.css (relative) then defines `.main`.
        let main_path = dir.join("main.css");
        std::fs::write(
            &main_path,
            "@import \"tokens.css\";\n.main { color: #222222 }",
        )
        .unwrap();

        let css_url = format!("file://{}", main_path.display());
        let base = format!("file://{}/page.html", dir.display());
        let html = format!(
            r#"<html><head><link rel="stylesheet" href="{css_url}"></head><body></body></html>"#
        );
        let doc = html::parse(&html);
        let (sheets, console) = collect_stylesheets(&doc, &base);

        // Two sheets: imported tokens.css FIRST, then main.css.
        assert_eq!(sheets.len(), 2, "console: {console:?}");
        assert!(console.is_empty(), "unexpected notes: {console:?}");
        assert!(
            sheets[0]
                .rules
                .iter()
                .any(|r| r.selectors.iter().any(|s| s == ".token")),
            "imported tokens.css should come first: {sheets:?}"
        );
        assert!(
            sheets[1]
                .rules
                .iter()
                .any(|r| r.selectors.iter().any(|s| s == ".main")),
            "importer main.css should come second: {sheets:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cdata_wrapped_inline_style_import_is_followed() {
        // XHTML reftests (WPT `.xht`) wrap inline CSS in `<![CDATA[ … ]]>` so the XML parser leaves
        // `@import`/`url(...)` alone. Our lenient HTML parser captures `<style>` as raw text, so the
        // markers land literally in the CSS. The wrapper must be stripped before `@import`
        // extraction — otherwise the leading `<![CDATA[` makes the scanner read `@import …;` as a
        // normal rule and skip it, so the imported sheet (which may define the test's @font-face)
        // is never fetched. Regression for the WOFF2 reftests rendering the fallback letter raw.
        let dir = std::env::temp_dir().join(format!("engine_cdata_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tokens_path = dir.join("tokens.css");
        std::fs::write(&tokens_path, ".token { color: #111111 }").unwrap();
        let base = format!("file://{}/page.xht", dir.display());
        // Inline <style> whose body is CDATA-wrapped and starts with an @import.
        let html = "<html><head><style type=\"text/css\"><![CDATA[\n@import url(\"tokens.css\");\n.main { color: #222222 }\n]]></style></head><body></body></html>";
        let doc = html::parse(html);

        // The extracted inline source must have no CDATA markers left.
        let sources = collect_style_sources(&doc, &base);
        let inline = sources
            .iter()
            .find_map(|s| match s {
                StyleSource::Inline(t) => Some(t.clone()),
                StyleSource::External(_) => None,
            })
            .expect("an inline <style> source");
        assert!(
            !inline.contains("CDATA") && !inline.contains("]]>"),
            "CDATA wrapper not stripped: {inline:?}"
        );
        assert_eq!(
            css::extract_imports(&inline),
            vec!["tokens.css".to_string()],
            "the @import must be visible to the extractor"
        );

        // End to end: the imported sheet is actually fetched + collected.
        let (sheets, console) = collect_stylesheets(&doc, &base);
        assert!(
            sheets
                .iter()
                .flat_map(|s| s.rules.iter())
                .any(|r| r.selectors.iter().any(|s| s == ".token")),
            "imported tokens.css should be followed from the CDATA-wrapped inline @import; \
             sheets={sheets:?} console={console:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_errors_are_captured_as_warnings() {
        let doc =
            html::parse(r#"<html><body><script>throw new Error("boom")</script></body></html>"#);
        let (_doc, console) = run_scripts(doc, "https://example.com/");
        assert!(
            console
                .iter()
                .any(|l| l.starts_with('⚠') && l.contains("boom")),
            "got {console:?}"
        );
    }

    #[test]
    fn no_scripts_yields_empty_console() {
        let doc = html::parse("<html><body><p>hi</p></body></html>");
        let (_doc, console) = run_scripts(doc, "https://example.com/");
        assert!(console.is_empty());
    }

    #[test]
    fn script_dom_mutation_is_reflected_in_visible_text() {
        // A script that rewrites an element's text via the real DOM must change what the page
        // renders: the new text appears and the old text is gone.
        let doc = html::parse(
            r#"<html><body><p id="m">old</p><script>document.getElementById("m").textContent="new"</script></body></html>"#,
        );
        let (doc, _console) = run_scripts(doc, "https://example.com/");
        let text = extract_visible_text(&doc);
        assert!(text.contains("new"), "expected 'new' in {text:?}");
        assert!(!text.contains("old"), "expected 'old' gone from {text:?}");
    }

    #[test]
    fn cascade_integration_styles_sheet_and_inline() {
        // A <style> rule (#x red, 24px) plus an inline-styled element. We exercise the same
        // path render() uses: parse DOM, collect <style> sheets, run style::cascade.
        let doc = html::parse(
            r#"<html><head><style>#x { color: #ff0000; font-size: 24px } h1 { color: blue }</style></head>
               <body><h1 id="x">Title</h1><p style="color: green; font-weight: bold">para</p></body></html>"#,
        );
        let (sheets, _console) = collect_stylesheets(&doc, "https://example.com/");
        let computed = style::cascade(&doc, &sheets);

        // Find the <h1> and <p> nodes.
        fn find(doc: &dom::Document, tag: &str) -> dom::NodeId {
            fn walk(doc: &dom::Document, id: dom::NodeId, tag: &str) -> Option<dom::NodeId> {
                if let dom::NodeData::Element(e) = &doc.get(id).data {
                    if e.tag == tag {
                        return Some(id);
                    }
                }
                for &c in &doc.get(id).children {
                    if let Some(f) = walk(doc, c, tag) {
                        return Some(f);
                    }
                }
                None
            }
            walk(doc, doc.root(), tag).expect("tag not found")
        }

        let h1 = find(&doc, "h1");
        let p = find(&doc, "p");

        // #x (id) beats h1 (type): red, 24px overrides the UA h1 size.
        assert_eq!(computed[&h1].color, (255, 0, 0));
        assert_eq!(computed[&h1].font_size, 24.0);
        assert!(computed[&h1].bold); // UA h1 is bold

        // Inline style on <p>: green + bold.
        assert_eq!(computed[&p].color, (0, 128, 0));
        assert!(computed[&p].bold);
        assert!(computed[&p].display_block);
    }

    #[test]
    fn layout_and_paint_runs_end_to_end_without_panic() {
        // A tiny local TextMeasurer so this test doesn't depend on a system font being present
        // (CI may have none). Geometry is deliberately not asserted — the layout algorithm is
        // implemented in parallel; we only assert the layout+paint path runs without panicking.
        struct TestMeasurer;
        impl layout::TextMeasurer for TestMeasurer {
            fn text_width(&self, text: &str, px: f32, bold: bool, _family: Option<&str>) -> f32 {
                let mut w = text.chars().count() as f32 * px * 0.5;
                if bold {
                    w += text.chars().count() as f32;
                }
                w
            }
            fn line_height(&self, px: f32, _family: Option<&str>) -> f32 {
                px * 1.3
            }
        }

        let doc = html::parse(
            r#"<html><body><div style="background-color:#ff0000; padding:10px">hi</div></body></html>"#,
        );
        let (sheets, _console) = collect_stylesheets(&doc, "https://example.com/");
        let computed = style::cascade(&doc, &sheets);
        let measurer = TestMeasurer;
        let no_images = HashMap::new();
        let root =
            layout::layout_document(&doc, &computed, 400.0, 600.0, &measurer, &no_images, None);

        // The painter clips to a band; paint into a small framebuffer without a font (text is
        // skipped when no font, but background/border painting still exercises the walk). Using
        // a no-op rasterizer keeps the text path live too.
        struct NoFont;
        impl GlyphRasterizer for NoFont {
            fn rasterize(&self, _ch: char, _px: f32) -> Option<paint::GlyphBitmap> {
                None
            }
            fn advance(&self, _ch: char, px: f32) -> f32 {
                px * 0.5
            }
        }
        let mut fb = Framebuffer::new(400, 300);
        let images: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        let no_canvas: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        let no_faces: HashMap<String, SystemFont> = HashMap::new();
        paint_box(
            &mut fb,
            Fonts {
                system: &NoFont,
                faces: &no_faces,
            },
            &root,
            16.0,
            28.0,
            28.0,
            300.0,
            &images,
            &no_canvas,
            &no_canvas,
            &no_canvas,
            &no_canvas,
            &[],
            &mut 0,
        );

        // The root box should exist; with the parallel layout stub it may have no children yet,
        // so only assert the path completed and the root carries the viewport width.
        assert!(root.dimensions.content.width >= 0.0);
    }

    #[test]
    fn opacity_blends_background_over_gradient() {
        // A full-viewport div with opacity:0.5 and a solid white background, painted over the
        // dark gradient, must yield a lighter result than the same opaque div would *not* —
        // simplest check: the opacity:0.5 render differs from the opacity:1 render, and the
        // half-opacity pixel is between the gradient and full white.
        struct M;
        impl layout::TextMeasurer for M {
            fn text_width(&self, t: &str, px: f32, _b: bool, _family: Option<&str>) -> f32 {
                t.chars().count() as f32 * px * 0.5
            }
            fn line_height(&self, px: f32, _family: Option<&str>) -> f32 {
                px * 1.3
            }
        }
        struct NoFont;
        impl GlyphRasterizer for NoFont {
            fn rasterize(&self, _c: char, _p: f32) -> Option<paint::GlyphBitmap> {
                None
            }
            fn advance(&self, _c: char, p: f32) -> f32 {
                p * 0.5
            }
        }

        let render_div = |opacity: &str| -> [u8; 3] {
            let html = format!(
                r#"<html><body><div style="height:100px; background-color:#ffffff; opacity:{opacity}"></div></body></html>"#
            );
            let doc = html::parse(&html);
            let (sheets, _c) = collect_stylesheets(&doc, "https://example.com/");
            let computed = style::cascade(&doc, &sheets);
            let root =
                layout::layout_document(&doc, &computed, 100.0, 200.0, &M, &HashMap::new(), None);
            let mut fb = Framebuffer::new(100, 100);
            paint_gradient(&mut fb);
            let imgs: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
            let no_canvas: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
            let no_faces: HashMap<String, SystemFont> = HashMap::new();
            paint_box(
                &mut fb,
                Fonts {
                    system: &NoFont,
                    faces: &no_faces,
                },
                &root,
                0.0,
                0.0,
                0.0,
                200.0,
                &imgs,
                &no_canvas,
                &no_canvas,
                &no_canvas,
                &no_canvas,
                &[],
                &mut 0,
            );
            // Sample a pixel inside the div.
            let i = (50 * fb.stride + 50 * 4) as usize;
            [fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]]
        };

        let opaque = render_div("1");
        let half = render_div("0.5");
        // Opaque white background → near 255 everywhere.
        assert!(opaque[0] > 240, "opaque white r={}", opaque[0]);
        // Half opacity → blended with the dark gradient → noticeably darker than opaque white,
        // but lighter than the bare gradient (which is < ~50).
        assert!(
            half[0] < opaque[0],
            "half {:?} should be darker than opaque {:?}",
            half,
            opaque
        );
        assert!(
            half[0] > 80,
            "half white over dark should still be fairly light, r={}",
            half[0]
        );
    }

    // A no-op text measurer/font used by the paint render tests below.
    struct TM;
    impl layout::TextMeasurer for TM {
        fn text_width(&self, t: &str, px: f32, _b: bool, _family: Option<&str>) -> f32 {
            t.chars().count() as f32 * px * 0.5
        }
        fn line_height(&self, px: f32, _family: Option<&str>) -> f32 {
            px * 1.3
        }
    }
    struct NF;
    impl GlyphRasterizer for NF {
        fn rasterize(&self, _c: char, _p: f32) -> Option<paint::GlyphBitmap> {
            None
        }
        fn advance(&self, _c: char, p: f32) -> f32 {
            p * 0.5
        }
    }

    /// Render an HTML body into a `w`x`h` framebuffer (black background) via the real paint path.
    fn render_html(html: &str, w: u32, h: u32) -> Framebuffer {
        let doc = html::parse(html);
        let (sheets, _c) = collect_stylesheets(&doc, "https://example.com/");
        let computed = style::cascade(&doc, &sheets);
        let root = layout::layout_document(
            &doc,
            &computed,
            w as f32,
            h as f32,
            &TM,
            &HashMap::new(),
            None,
        );
        let mut fb = Framebuffer::new(w, h);
        fb.clear(Color::BLACK);
        let imgs: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        let no_canvas: HashMap<dom::NodeId, DecodedImage> = HashMap::new();
        let no_faces: HashMap<String, SystemFont> = HashMap::new();
        paint_box(
            &mut fb,
            Fonts {
                system: &NF,
                faces: &no_faces,
            },
            &root,
            0.0,
            0.0,
            0.0,
            h as f32,
            &imgs,
            &no_canvas,
            &no_canvas,
            &no_canvas,
            &no_canvas,
            &[],
            &mut 0,
        );
        fb
    }

    fn px_rgb(fb: &Framebuffer, x: i32, y: i32) -> [u8; 3] {
        let i = (y as u32 * fb.stride) as usize + (x as usize) * 4;
        [fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]]
    }

    #[test]
    fn linear_gradient_left_dark_right_light() {
        // A full-width div with a left-to-right black→white gradient: left edge ≈ black, right ≈
        // white.
        let fb = render_html(
            r#"<html><body><div style="height:60px; background: linear-gradient(to right, rgb(0,0,0), rgb(255,255,255))"></div></body></html>"#,
            200,
            80,
        );
        let left = px_rgb(&fb, 2, 30);
        let right = px_rgb(&fb, 197, 30);
        assert!(left[0] < 40, "left edge should be near black, got {left:?}");
        assert!(
            right[0] > 215,
            "right edge should be near white, got {right:?}"
        );
        assert!(right[0] > left[0] + 150, "gradient should ramp dark→light");
    }

    #[test]
    fn box_shadow_paints_outside_the_box() {
        // A small box offset from the top-left with a box-shadow toward the bottom-right. Pixels
        // just outside the box's lower-right should be non-background (shadow), while the far
        // top-left corner stays background-black.
        let fb = render_html(
            r#"<html><body><div style="width:40px; height:40px; margin:20px; background:rgb(0,0,255); box-shadow: 12px 12px 0px rgb(255,0,0)"></div></body></html>"#,
            120,
            120,
        );
        // The box occupies roughly x∈[20,60], y∈[20,60] (margin 20). Shadow offset +12,+12.
        // Sample a point inside the shadow but outside the box (e.g. x=66, y=66).
        let shadow = px_rgb(&fb, 66, 66);
        assert!(
            shadow[0] > 100,
            "expected red-ish shadow outside the box, got {shadow:?}"
        );
        // Far above-left of everything: untouched background.
        let bg = px_rgb(&fb, 5, 5);
        assert_eq!(bg, [0, 0, 0], "top-left should be background black");
    }

    #[test]
    fn transform_translate_shifts_painted_pixels_right() {
        // The same box with and without translate(40px,0): the translated render has its colored
        // pixels shifted right by ~40px.
        let base = render_html(
            r#"<html><body><div style="width:30px; height:30px; background:rgb(0,200,0)"></div></body></html>"#,
            200,
            60,
        );
        let moved = render_html(
            r#"<html><body><div style="width:30px; height:30px; background:rgb(0,200,0); transform: translate(40px,0)"></div></body></html>"#,
            200,
            60,
        );
        // Find the rightmost green pixel on row y=15 in each render.
        let rightmost_green = |fb: &Framebuffer| -> i32 {
            let mut last = -1;
            for x in 0..fb.width as i32 {
                let p = px_rgb(fb, x, 15);
                if p[1] > 120 && p[0] < 80 {
                    last = x;
                }
            }
            last
        };
        let b = rightmost_green(&base);
        let m = rightmost_green(&moved);
        assert!(b >= 0 && m >= 0, "green not found base={b} moved={m}");
        assert!(
            (m - b - 40).abs() <= 3,
            "translate should shift ~40px: base={b} moved={m}"
        );
    }

    // Count colored (non-black) pixels on a given row within an x range.
    fn colored_on_row(fb: &Framebuffer, y: i32, x0: i32, x1: i32) -> i32 {
        let mut n = 0;
        for x in x0..x1.min(fb.width as i32) {
            let p = px_rgb(fb, x, y);
            if p != [0, 0, 0] {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn underline_paints_a_line_below_the_baseline() {
        // The NF stub rasterizes no glyphs, so any colored pixels on a row come from the
        // decoration line (or background), not the text itself. An underlined run must produce a
        // horizontal colored line; a plain run must not.
        let underlined = render_html(
            r#"<html><body style="margin:0"><span style="text-decoration:underline; color:rgb(255,255,255); font-size:20px">hello</span></body></html>"#,
            200,
            60,
        );
        let plain = render_html(
            r#"<html><body style="margin:0"><span style="color:rgb(255,255,255); font-size:20px">hello</span></body></html>"#,
            200,
            60,
        );
        // The run sits on the first line; baseline ≈ 20*0.8 = 16, underline just below it.
        let mut found_underline = false;
        for y in 16..26 {
            if colored_on_row(&underlined, y, 0, 60) >= 10 {
                found_underline = true;
            }
        }
        assert!(found_underline, "expected an underline row of white pixels");
        // The plain run draws no glyphs (NF stub) and no decoration → no colored rows.
        let mut plain_colored = 0;
        for y in 0..60 {
            plain_colored += colored_on_row(&plain, y, 0, 60);
        }
        assert_eq!(
            plain_colored, 0,
            "undecorated text should paint no line, got {plain_colored} px"
        );
    }

    #[test]
    fn line_through_and_overline_paint_at_distinct_heights() {
        let strike = render_html(
            r#"<html><body style="margin:0"><span style="text-decoration:line-through; color:rgb(255,255,255); font-size:20px">hello</span></body></html>"#,
            200,
            60,
        );
        let over = render_html(
            r#"<html><body style="margin:0"><span style="text-decoration:overline; color:rgb(255,255,255); font-size:20px">hello</span></body></html>"#,
            200,
            60,
        );
        let row_of = |fb: &Framebuffer| -> i32 {
            for y in 0..40 {
                if colored_on_row(fb, y, 0, 60) >= 10 {
                    return y;
                }
            }
            -1
        };
        let strike_y = row_of(&strike);
        let over_y = row_of(&over);
        assert!(strike_y >= 0, "line-through not painted");
        assert!(over_y >= 0, "overline not painted");
        // Overline sits clearly above the strike-through (which crosses the x-height middle).
        assert!(
            over_y < strike_y,
            "overline ({over_y}) should be above line-through ({strike_y})"
        );
    }

    #[test]
    fn mark_paints_yellow_behind_the_text() {
        let fb = render_html(
            r#"<html><body style="margin:0"><mark>hi</mark></body></html>"#,
            200,
            60,
        );
        // Scan the top line for yellow (#ffff00) pixels behind the run.
        let mut yellow = 0;
        for y in 0..30 {
            for x in 0..60 {
                let p = px_rgb(&fb, x, y);
                if p[0] > 200 && p[1] > 200 && p[2] < 60 {
                    yellow += 1;
                }
            }
        }
        assert!(
            yellow > 20,
            "expected a yellow mark highlight band, got {yellow} px"
        );
    }

    #[test]
    fn sup_run_is_painted_above_a_normal_run() {
        // Compare the top y of the colored band for a baseline run vs a superscript run. The
        // superscript run (vertical-align:super) is shifted up, so its highest colored pixel is
        // higher (smaller y). We give the sup an underline so the NF stub still paints a visible
        // line we can locate.
        let normal = render_html(
            r#"<html><body style="margin:0"><span style="text-decoration:underline; color:rgb(255,255,255); font-size:20px">x</span></body></html>"#,
            120,
            80,
        );
        let supscript = render_html(
            r#"<html><body style="margin:0"><sup style="text-decoration:underline; color:rgb(255,255,255); font-size:20px">x</sup></body></html>"#,
            120,
            80,
        );
        let top_y = |fb: &Framebuffer| -> i32 {
            for y in 0..80 {
                if colored_on_row(fb, y, 0, 120) >= 1 {
                    return y;
                }
            }
            -1
        };
        let n = top_y(&normal);
        let s = top_y(&supscript);
        assert!(n >= 0 && s >= 0, "lines not found normal={n} sup={s}");
        assert!(
            s < n,
            "superscript run ({s}) should sit above the normal run ({n})"
        );
    }

    #[test]
    fn font_measurer_bold_is_wider() {
        // FontMeasurer needs a real font; skip gracefully when none is present.
        use layout::TextMeasurer;
        if let Some(font) = SystemFont::load() {
            let faces = HashMap::new();
            let m = FontMeasurer {
                font: &font,
                faces: &faces,
            };
            let plain = m.text_width("abc", 16.0, false, None);
            let bold = m.text_width("abc", 16.0, true, None);
            assert!(bold > plain, "bold {bold} should exceed plain {plain}");
            assert_eq!(m.line_height(10.0, None), 13.0);
        }
    }

    #[test]
    fn script_created_element_appears_in_visible_text() {
        let doc = html::parse(
            r#"<html><body><script>var el=document.createElement("p");el.textContent="injected";document.body.appendChild(el);</script></body></html>"#,
        );
        let (doc, _console) = run_scripts(doc, "https://example.com/");
        let text = extract_visible_text(&doc);
        assert!(text.contains("injected"), "expected 'injected' in {text:?}");
    }

    #[test]
    fn local_png_image_is_decoded_and_produces_an_image_box() {
        // Generate a tiny PNG with the `image` crate, reference it from an HTML page via file://,
        // load it through the engine, and assert (a) the image was decoded and (b) layout produces
        // an Image box of the intrinsic size. No network is used.
        let dir = std::env::temp_dir().join(format!("engine_img_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png_path = dir.join("tiny.png");

        // A 4x3 opaque-red image.
        let (iw, ih) = (4u32, 3u32);
        let buf = image::RgbaImage::from_pixel(iw, ih, image::Rgba([200, 30, 40, 255]));
        buf.save(&png_path).unwrap();

        let img_url = format!("file://{}", png_path.display());
        let html = format!(r#"<html><body><img src="{img_url}"></body></html>"#);
        let html_path = dir.join("page.html");
        std::fs::write(&html_path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(400, 300, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", html_path.display())), 0);

        // (a) The image was fetched + decoded.
        assert_eq!(e.decoded_image_count(), 1, "expected one decoded image");
        assert_eq!(e.first_decoded_image_size(), Some((iw, ih)));

        // (b) Render runs the layout path; verify an Image box of the right size exists by
        // re-running layout with the same intrinsic map the engine builds.
        let body = std::fs::read_to_string(&html_path).unwrap();
        let doc = html::parse(&body);
        let (sheets, _console) =
            collect_stylesheets(&doc, &format!("file://{}", html_path.display()));
        let computed = style::cascade(&doc, &sheets);
        let base = base_url(&doc, &format!("file://{}", html_path.display()));
        let mut console = Vec::new();
        let images = collect_images(&doc, &base, &mut console);
        assert_eq!(images.len(), 1);
        let intrinsic: HashMap<dom::NodeId, (f32, f32)> = images
            .iter()
            .map(|(&id, img)| (id, (img.w as f32, img.h as f32)))
            .collect();

        struct M;
        impl layout::TextMeasurer for M {
            fn text_width(&self, t: &str, px: f32, _b: bool, _family: Option<&str>) -> f32 {
                t.chars().count() as f32 * px * 0.5
            }
            fn line_height(&self, px: f32, _family: Option<&str>) -> f32 {
                px * 1.3
            }
        }
        let root = layout::layout_document(&doc, &computed, 400.0, 300.0, &M, &intrinsic, None);

        fn find_image(b: &layout::LayoutBox) -> Option<&layout::LayoutBox> {
            if matches!(b.content, layout::BoxContent::Image(_)) {
                return Some(b);
            }
            for c in &b.children {
                if let Some(f) = find_image(c) {
                    return Some(f);
                }
            }
            None
        }
        let ibox = find_image(&root).expect("expected an Image box in the layout tree");
        assert_eq!(ibox.dimensions.content.width, iw as f32);
        assert_eq!(ibox.dimensions.content.height, ih as f32);

        // Render must not panic and produces a framebuffer.
        let fb = e.render();
        assert_eq!(fb.width, 400);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 2x2 solid opaque-red JPEG XL (raw codestream, lossless), produced by `cjxl`. The `image`
    /// crate cannot decode this; it exercises the `jxl-oxide` path in `decode_image`.
    const RED_2X2_JXL: &[u8] = &[
        0xff, 0x0a, 0x08, 0x10, 0xb0, 0x12, 0x08, 0x10, 0x10, 0x00, 0x38, 0x00, 0x4b, 0x18, 0x8b,
        0x15, 0xc2, 0x49, 0x41, 0x1e, 0x40, 0x04, 0xe8, 0x8f, 0xfe, 0x00,
    ];

    #[test]
    fn jpeg_xl_bytes_decode_to_rgba8() {
        assert!(is_jxl(RED_2X2_JXL), "raw codestream should sniff as JXL");
        let img = decode_image(RED_2X2_JXL).expect("jxl-oxide should decode the codestream");
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.rgba.len(), 2 * 2 * 4);
        for px in img.rgba.chunks_exact(4) {
            // Lossless red: ~(255, 0, 0, 255). Allow a tiny slack for color conversion rounding.
            assert!(px[0] > 247, "R≈255, got {}", px[0]);
            assert!(px[1] < 8, "G≈0, got {}", px[1]);
            assert!(px[2] < 8, "B≈0, got {}", px[2]);
            assert_eq!(px[3], 255, "opaque");
        }
    }

    #[test]
    fn local_jpeg_xl_image_is_decoded_via_engine() {
        // The full <img> pipeline (fetch + decode) must handle a `.jxl` source over file://.
        let dir = std::env::temp_dir().join(format!("engine_jxl_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jxl_path = dir.join("red.jxl");
        std::fs::write(&jxl_path, RED_2X2_JXL).unwrap();

        let img_url = format!("file://{}", jxl_path.display());
        let html = format!(r#"<html><body><img src="{img_url}"></body></html>"#);
        let html_path = dir.join("page.html");
        std::fs::write(&html_path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(400, 300, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", html_path.display())), 0);
        assert_eq!(e.decoded_image_count(), 1, "expected the JXL to decode");
        assert_eq!(e.first_decoded_image_size(), Some((2, 2)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- ES module collection / import extraction / specifier rewrite -------------------

    #[test]
    fn collect_module_entries_picks_module_scripts() {
        let doc = html::parse(
            r#"<html><body>
                 <script src="/classic.js"></script>
                 <script type="module" src="/app.js"></script>
                 <script type="module">import "./side.js";</script>
               </body></html>"#,
        );
        let entries = collect_module_entries(&doc, "https://x.com/page/");
        assert_eq!(
            entries.len(),
            2,
            "classic script must be excluded: {entries:?}"
        );
        assert_eq!(
            entries[0],
            ModuleEntry::External("https://x.com/app.js".to_string())
        );
        assert!(matches!(&entries[1], ModuleEntry::Inline(s) if s.contains("./side.js")));
        // Classic scripts are NOT collected as modules.
        let classic = collect_script_sources(&doc, "https://x.com/page/");
        assert!(classic
            .iter()
            .any(|s| matches!(s, ScriptSource::External(u) if u.ends_with("classic.js"))));
        // ...and the module scripts are skipped by the classic collector.
        assert!(!classic
            .iter()
            .any(|s| matches!(s, ScriptSource::External(u) if u.ends_with("app.js"))));
    }

    #[test]
    fn extract_specifiers_handles_all_forms() {
        let src = r#"
            import { a, b } from "./util.js";
            import def from '../lib/x.js';
            import "./side-effect.js";
            export { c } from "./reexp.js";
            export * from "./all.js";
            const lazy = import("./dyn.js");
            // import "./comment.js";
            const s = "import 'not-real.js'";
        "#;
        let specs = extract_specifiers(src);
        let found: Vec<&str> = specs.iter().map(|s| s.spec.as_str()).collect();
        assert_eq!(
            found,
            vec![
                "./util.js",
                "../lib/x.js",
                "./side-effect.js",
                "./reexp.js",
                "./all.js",
                "./dyn.js",
            ],
            "got {found:?}"
        );
        // Comment and string-literal occurrences must NOT be extracted.
        assert!(!found.contains(&"./comment.js"));
        assert!(!found.contains(&"not-real.js"));
    }

    #[test]
    fn bare_specifier_classification() {
        assert!(is_bare_specifier("vue"));
        assert!(is_bare_specifier("@vue/runtime-core"));
        assert!(is_bare_specifier("lodash/merge"));
        assert!(!is_bare_specifier("./local.js"));
        assert!(!is_bare_specifier("../up.js"));
        assert!(!is_bare_specifier("/abs.js"));
        assert!(!is_bare_specifier("https://x/y.js"));
        assert!(!is_bare_specifier("file:///a.js"));
    }

    #[test]
    fn collect_module_graph_rewrites_inline_entry_specifiers_to_canonical_urls() {
        // An inline module importing a relative specifier should be rewritten to an absolute URL,
        // and a bare specifier should be skipped with a note. No network: the relative import
        // points at a file:// path so the fetch would fail, but we only assert the rewrite/notes
        // on the inline entry source which is in the map already.
        let doc = html::parse(
            r#"<html><body><script type="module">
                 import { x } from "./dep.js";
                 import vue from "vue";
                 console.log(x);
               </script></body></html>"#,
        );
        let (entries, sources, notes) = collect_module_graph(&doc, "https://site.test/app/");
        assert_eq!(entries.len(), 1);
        let entry_src = sources.get(&entries[0]).expect("entry source present");
        // Relative specifier rewritten to its canonical absolute URL.
        assert!(
            entry_src.contains("https://site.test/app/dep.js"),
            "expected rewritten dep url in {entry_src:?}"
        );
        // The bare `vue` import is recorded as skipped (and left intact / unresolved).
        assert!(
            notes
                .iter()
                .any(|n| n.contains("[skipped bare import: vue]")),
            "expected bare-import note, got {notes:?}"
        );
    }

    #[test]
    fn intersection_observer_fires_on_scroll_into_view() {
        // A tall page with a spacer pushing the target far below the fold, then a target the JS
        // observes. The observer callback writes the isIntersecting flag onto <body data-seen>.
        let html = r#"<html><body>
            <div style="height:2000px; background-color:#102030"></div>
            <div id="target" style="height:100px; background-color:#a01010"></div>
            <script>
              var io = new IntersectionObserver(function (entries) {
                for (var i = 0; i < entries.length; i++) {
                  if (entries[i].isIntersecting) { document.body.setAttribute('data-seen', '1'); }
                }
              });
              io.observe(document.getElementById('target'));
            </script>
            </body></html>"#;
        let path = std::env::temp_dir().join("browser_io_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 300, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();

        // Target is ~2000px down; the 300px viewport at the top can't see it → not intersecting.
        assert_eq!(
            e.body_attr("data-seen"),
            None,
            "target must not be seen at the top"
        );

        // Scroll down past the spacer so the target enters the viewport, then tick to re-evaluate.
        e.scroll_by(2000.0);
        let _ = e.render();
        let mut fired = false;
        for _ in 0..5 {
            e.tick();
            if e.body_attr("data-seen").as_deref() == Some("1") {
                fired = true;
                break;
            }
        }
        assert!(
            fired,
            "IntersectionObserver callback must fire once the target scrolls into view"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn websocket_unreachable_fires_close_offline() {
        // Deterministic + offline: opening a WebSocket to an unreachable host must fire onerror then
        // onclose (readyState → 3 CLOSED). The host socket thread fails to connect and delivers
        // error+close events over the WS channel, which the session drains (during the load drain or
        // a subsequent tick) and dispatches via __wsDeliver. We poll the LIVE session state (the
        // close handler records the flags on `window`) so the assertion doesn't depend on which
        // drain delivered the event.
        let html = r#"<html><body>
            <script>
              window.__wsErr = 0; window.__wsClosed = -1;
              var ws = new WebSocket('ws://127.0.0.1:1/');
              ws.onerror = function () { window.__wsErr = 1; };
              ws.onclose = function (e) { window.__wsClosed = ws.readyState; };
            </script>
            </body></html>"#;
        let path = std::env::temp_dir().join("browser_ws_unreachable_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 300, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();

        // Poll generously (up to ~5s): the connect-failure → close-event → drain → dispatch chain
        // can take longer than a second on a heavily loaded CI runner. Breaks out as soon as it fires.
        let mut closed = false;
        for _ in 0..200 {
            if e.console_eval("String(window.__wsClosed)") == "3" {
                closed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            e.tick();
        }
        assert!(
            closed,
            "WebSocket to an unreachable host must fire onclose with readyState 3"
        );
        assert_eq!(
            e.console_eval("String(window.__wsErr)"),
            "1",
            "onerror must fire too"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resize_observer_fires_on_viewport_change() {
        // Observe a block element; changing the viewport width reflows it (full-width block), so the
        // observer fires with the new size. The callback records the observed width on <body>.
        let html = r#"<html><body>
            <div id="box" style="height:50px; background-color:#20a020"></div>
            <script>
              var ro = new ResizeObserver(function (entries) {
                for (var i = 0; i < entries.length; i++) {
                  document.body.setAttribute('data-w', String(Math.round(entries[i].contentRect.width)));
                }
              });
              ro.observe(document.getElementById('box'));
            </script>
            </body></html>"#;
        let path = std::env::temp_dir().join("browser_ro_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 300, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();

        // Initial observation must have fired with the initial (200-wide) box.
        let initial = e.body_attr("data-w");
        assert!(
            initial.is_some(),
            "ResizeObserver must deliver an initial size, got {initial:?}"
        );

        // Widen the viewport so the full-width block reflows to ~400px wide.
        e.set_viewport(400, 300, 1.0);
        let _ = e.render();
        let mut changed = false;
        for _ in 0..5 {
            e.tick();
            let w = e.body_attr("data-w");
            if w.is_some() && w != initial {
                changed = true;
                break;
            }
        }
        assert!(changed, "ResizeObserver callback must fire with the new size after a viewport change (was {initial:?}, now {:?})", e.body_attr("data-w"));

        let _ = std::fs::remove_file(&path);
    }

    /// Context the progressive-frame test callback writes into (count + last dims).
    struct FrameProbe {
        count: u32,
        last_w: u32,
        last_h: u32,
    }

    /// A C-ABI frame callback that records invocation count + the last framebuffer dimensions into
    /// the `FrameProbe` pointed to by `ctx`.
    extern "C" fn probe_cb(ctx: *mut std::ffi::c_void, fb: FrameView) {
        // Safe: the test keeps the FrameProbe alive on its stack across the synchronous load_url
        // call that invokes this callback, and passes a pointer to it as `ctx`.
        let probe = unsafe { &mut *(ctx as *mut FrameProbe) };
        probe.count += 1;
        if !fb.pixels.is_null() {
            probe.last_w = fb.width;
            probe.last_h = fb.height;
        }
    }

    #[test]
    fn streaming_load_emits_frames_and_matches_final_render() {
        let html = "<html><body>\
            <style>div{height:30px;background-color:#3050a0}</style>\
            <h1>Streaming</h1><p>hello progressive world</p>\
            <div></div></body></html>";
        let path = std::env::temp_dir().join("browser_streaming_test.html");
        std::fs::write(&path, html).unwrap();
        let url = format!("file://{}", path.display());

        // Reference (non-streaming): no callback installed.
        let mut reference = Engine::new();
        reference.set_viewport(200, 150, 2.0);
        assert_eq!(reference.load_url(&url), 0);
        let ref_text = reference.visible_text();
        let ref_fb_center = center_column(reference.render()).clone();

        // Streaming: install a frame callback that counts invocations + records last dims.
        let mut probe = FrameProbe {
            count: 0,
            last_w: 0,
            last_h: 0,
        };
        let mut e = Engine::new();
        e.set_viewport(200, 150, 2.0);
        e.set_frame_callback(Some((
            probe_cb,
            &mut probe as *mut FrameProbe as *mut std::ffi::c_void,
        )));
        assert_eq!(e.load_url(&url), 0);

        // file:// delivers one chunk → at least the first partial frame + the final frame.
        assert!(
            probe.count >= 1,
            "frame callback must fire at least once, got {}",
            probe.count
        );
        // The last frame's dims are the device viewport (200*2 x 150*2).
        assert_eq!(
            (probe.last_w, probe.last_h),
            (400, 300),
            "final frame dims = device viewport"
        );

        // The FINAL state/render is byte-for-byte the non-streaming result (streaming only adds
        // earlier partial frames).
        assert_eq!(
            e.visible_text(),
            ref_text,
            "streamed final text matches non-streaming"
        );
        let stream_fb_center = center_column(e.render()).clone();
        assert_eq!(
            stream_fb_center, ref_fb_center,
            "streamed final render matches non-streaming"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_then_render_visible_text_unchanged() {
        // Regression: loading a known local page then rendering yields the expected visible text,
        // unaffected by the streaming rewrite (no frame callback installed = no partial frames).
        let html = "<html><body><h1>Title</h1><p>Body text here.</p></body></html>";
        let path = std::env::temp_dir().join("browser_regression_text_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(300, 200, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();
        let text = e.visible_text();
        assert!(
            text.contains("Title"),
            "visible text must contain the heading: {text:?}"
        );
        assert!(
            text.contains("Body text here."),
            "visible text must contain the paragraph: {text:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dom_tree_json_has_tags_attrs_and_nesting() {
        let html = "<html><body>\
            <div class=\"box\" id=\"main\"><p>Hello world</p>  \n  </div></body></html>";
        let path = std::env::temp_dir().join("browser_domtree_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(200, 150, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);

        let json = e.dom_tree_json();
        // Structure: root is <html> with type "element".
        assert!(
            json.starts_with("{\"id\":"),
            "tree must start with a node object: {json}"
        );
        assert!(
            json.contains("\"type\":\"element\""),
            "elements tagged: {json}"
        );
        assert!(json.contains("\"tag\":\"html\""), "root tag html: {json}");
        assert!(json.contains("\"tag\":\"body\""), "body nested: {json}");
        assert!(json.contains("\"tag\":\"div\""), "div nested: {json}");
        assert!(json.contains("\"tag\":\"p\""), "p nested: {json}");
        // Attributes preserved.
        assert!(json.contains("\"class\":\"box\""), "class attr: {json}");
        assert!(json.contains("\"id\":\"main\""), "id attr: {json}");
        // Text node: whitespace-collapsed, tagged as text.
        assert!(
            json.contains("\"type\":\"text\""),
            "text node present: {json}"
        );
        assert!(
            json.contains("\"text\":\"Hello world\""),
            "collapsed text: {json}"
        );
        // The all-whitespace trailing text node was skipped (no empty text node).
        assert!(
            !json.contains("\"text\":\"\""),
            "empty text nodes skipped: {json}"
        );
        // It parses as a single JSON value (balanced braces/brackets).
        assert!(
            json.matches('{').count() == json.matches('}').count(),
            "balanced braces: {json}"
        );

        // No document loaded → "{}".
        let empty = Engine::new();
        assert_eq!(empty.dom_tree_json(), "{}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn node_at_point_returns_element_and_inspect_overlay_changes_pixels() {
        // A tall colored block so a hit-test over the body content lands on a laid-out element, and
        // the inspect overlay visibly changes pixels.
        let html = "<html><body>\
            <div id=\"target\" style=\"height:120px;background-color:#202020\"></div>\
            </body></html>";
        let path = std::env::temp_dir().join("browser_inspect_test.html");
        std::fs::write(&path, html).unwrap();

        let mut e = Engine::new();
        e.set_viewport(120, 200, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let before = center_column(e.render()).clone();

        // A point well inside the block: returns some element id.
        let node = e.node_at_point(60.0, 40.0);
        assert!(
            node.is_some(),
            "node_at_point over laid-out content returns an element id"
        );

        // Highlight that node; the render must differ where the overlay draws.
        e.set_inspect_node(node);
        let after = center_column(e.render()).clone();
        assert_ne!(before, after, "inspect overlay must change pixels");

        // Clearing the highlight restores the original render.
        e.set_inspect_node(None);
        let cleared = center_column(e.render()).clone();
        assert_eq!(
            before, cleared,
            "clearing the inspect node restores the render"
        );

        // Setting an out-of-range node id is ignored (no panic, no overlay).
        e.set_inspect_node(Some(usize::MAX));
        let _ = e.render();

        let _ = std::fs::remove_file(&path);
    }

    /// Render a canvas page at 1x and return the framebuffer pixel (r,g,b,a) at device (x,y).
    fn canvas_render_px(
        html: &str,
        name: &str,
    ) -> (Engine, Box<dyn Fn(&Framebuffer, i32, i32) -> [u8; 4]>) {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, html).unwrap();
        let mut e = Engine::new();
        e.set_viewport(320, 240, 1.0);
        assert_eq!(e.load_url(&format!("file://{}", path.display())), 0);
        let _ = e.render();
        let _ = std::fs::remove_file(&path);
        let at = |fb: &Framebuffer, x: i32, y: i32| -> [u8; 4] {
            if x < 0 || y < 0 || x >= fb.width as i32 || y >= fb.height as i32 {
                return [0, 0, 0, 0];
            }
            let i = (y as u32 * fb.stride) as usize + x as usize * 4;
            [
                fb.pixels[i],
                fb.pixels[i + 1],
                fb.pixels[i + 2],
                fb.pixels[i + 3],
            ]
        };
        (e, Box::new(at))
    }

    #[test]
    fn canvas_fill_rect_paints_red_inside_only() {
        // A 200x120 canvas at the top-left; fill a red rect at (10,10,50,40).
        let html = "<html><body style='margin:0'><canvas id='c' width='200' height='120'></canvas>\
            <script>var x=document.getElementById('c').getContext('2d');\
            x.fillStyle='#ff0000';x.fillRect(10,10,50,40);</script></body></html>";
        let (mut e, at) = canvas_render_px(html, "browser_canvas_rect.html");
        let fb = e.render();
        // Inside the rect (30,30): red.
        let inside = at(fb, 30, 30);
        assert!(
            inside[0] > 200 && inside[1] < 60 && inside[2] < 60,
            "inside should be red, got {inside:?}"
        );
        // Outside the rect (120,90): NOT red.
        let outside = at(fb, 120, 90);
        assert!(
            !(outside[0] > 200 && outside[1] < 60 && outside[2] < 60),
            "outside must not be red, got {outside:?}"
        );
    }

    #[test]
    fn canvas_fill_text_paints_some_pixels() {
        let html = "<html><body style='margin:0'><canvas id='c' width='200' height='80'></canvas>\
            <script>var x=document.getElementById('c').getContext('2d');\
            x.fillStyle='#00ff00';x.font='40px sans-serif';x.fillText('Hi',10,50);</script></body></html>";
        let (mut e, at) = canvas_render_px(html, "browser_canvas_text.html");
        let fb = e.render();
        // Scan the text region for any greenish (non-background) pixel.
        let mut found = false;
        for y in 10..70 {
            for x in 5..120 {
                let p = at(fb, x, y);
                if p[1] > 120 && p[0] < 120 && p[2] < 120 {
                    found = true;
                }
            }
        }
        assert!(found, "fillText should rasterize some green glyph pixels");
    }

    #[test]
    fn canvas_fill_path_triangle_has_interior() {
        // A filled triangle with vertices (10,10),(90,10),(50,80). Its centroid (~50,33) is interior.
        let html = "<html><body style='margin:0'><canvas id='c' width='120' height='100'></canvas>\
            <script>var x=document.getElementById('c').getContext('2d');\
            x.fillStyle='#0000ff';x.beginPath();x.moveTo(10,10);x.lineTo(90,10);x.lineTo(50,80);\
            x.closePath();x.fill();</script></body></html>";
        let (mut e, at) = canvas_render_px(html, "browser_canvas_tri.html");
        let fb = e.render();
        // Centroid is inside → blue.
        let inside = at(fb, 50, 33);
        assert!(
            inside[2] > 200 && inside[0] < 60 && inside[1] < 60,
            "triangle interior should be blue, got {inside:?}"
        );
        // A corner well outside the triangle (5,90) is NOT blue.
        let outside = at(fb, 5, 90);
        assert!(
            !(outside[2] > 200 && outside[0] < 60),
            "outside the triangle must not be blue, got {outside:?}"
        );
    }

    // --- Table presentational attributes / border-collapse -------------------------------------

    #[test]
    fn td_bgcolor_red_fills_cell() {
        // <td bgcolor="red"> paints red pixels in the cell.
        let html = "<html><body style='margin:0'>\
            <table cellspacing='0'><tr><td id='c' bgcolor='red'>cell</td></tr></table></body></html>";
        let e = load_rendered(html, "browser_td_bgcolor.html", 200, 100);
        let td = e
            .node_by_attr_id("c")
            .and_then(|id| e.node_device_rect(id))
            .expect("td rect");
        // Scan the cell interior for a clearly-red pixel.
        let mut red = false;
        let x0 = td.x as i32 + 1;
        let y0 = td.y as i32 + 1;
        let x1 = (td.x + td.width) as i32 - 1;
        let y1 = (td.y + td.height) as i32 - 1;
        for y in y0..y1 {
            for x in x0..x1 {
                let (r, g, b) = px_at(&e, x, y);
                if r > 200 && g < 80 && b < 80 {
                    red = true;
                }
            }
        }
        assert!(red, "td bgcolor=red should fill the cell with red");
    }

    #[test]
    fn border_collapse_draws_single_line_between_cells() {
        // Two collapsed cells with 1px borders: the shared vertical edge must be a SINGLE 1px line,
        // not a doubled/gapped pair. We scan a horizontal strip across the shared edge and count
        // contiguous dark-pixel runs; collapse should give exactly one run there.
        let html = "<html><body style='margin:0'>\
            <style>table{border-collapse:collapse} td{border:1px solid black; padding:6px}</style>\
            <table><tr><td id='a'>AA</td><td id='b'>BB</td></tr></table></body></html>";
        let e = load_rendered(html, "browser_collapse_line.html", 200, 100);
        let r0 = e
            .node_by_attr_id("a")
            .and_then(|id| e.node_device_rect(id))
            .expect("td0");
        let r1 = e
            .node_by_attr_id("b")
            .and_then(|id| e.node_device_rect(id))
            .expect("td1");
        // The two cells are flush: cell1's left == cell0's right (within 1px).
        assert!(
            (r1.x - (r0.x + r0.width)).abs() <= 1.5,
            "collapsed cells not flush: {} vs {}",
            r1.x,
            r0.x + r0.width
        );
        // Scan a row through the middle of the cells; count dark vertical runs across the boundary.
        let y = (r0.y + r0.height / 2.0) as i32;
        let scan_x0 = (r0.x + r0.width) as i32 - 4;
        let scan_x1 = (r1.x) as i32 + 4;
        let mut runs = 0;
        let mut prev_dark = false;
        for x in scan_x0..=scan_x1 {
            let (r, g, b) = px_at(&e, x, y);
            let dark = r < 100 && g < 100 && b < 100;
            if dark && !prev_dark {
                runs += 1;
            }
            prev_dark = dark;
        }
        assert_eq!(
            runs, 1,
            "collapsed shared edge should be a single line, found {runs} dark runs"
        );
    }

    #[test]
    fn table_border_attr_paints_visible_borders() {
        // <table border="2"> gives the table a visible border (dark pixels near the table frame).
        let html = "<html><body style='margin:0'>\
            <table id='t' border='2' cellspacing='0'><tr><td>X</td></tr></table></body></html>";
        let e = load_rendered(html, "browser_table_border_attr.html", 200, 100);
        let r = e
            .node_by_attr_id("t")
            .and_then(|id| e.node_device_rect(id))
            .expect("table rect");
        // Scan the top border band for dark pixels.
        let mut dark = 0;
        let yb = r.y as i32;
        for x in (r.x as i32 + 1)..((r.x + r.width) as i32 - 1) {
            for y in yb..(yb + 2) {
                let (rr, g, b) = px_at(&e, x, y);
                if rr < 120 && g < 120 && b < 120 {
                    dark += 1;
                }
            }
        }
        assert!(
            dark > 5,
            "table border=2 should paint a visible top border, dark px = {dark}"
        );
    }
}
